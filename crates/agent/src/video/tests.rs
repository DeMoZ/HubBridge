//! The agent over real TCP on localhost, driven by a minimal host.

use std::net::SocketAddr;
use std::path::PathBuf;

use hubbridge_link::{ClientAuth, Secret, client_handshake};
use hubbridge_proto::{DisplayMode, fixtures};
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::time::timeout;

use super::*;
use crate::AgentConfig;
use crate::fake::FakeVideoSink;

const SOON: Duration = Duration::from_secs(1);

const SPS_PPS: &[u8] = &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2];
const IDR: &[u8] = &[0, 0, 0, 1, 0x65, 3, 4];
const DELTA: &[u8] = &[0, 0, 0, 1, 0x41, 5];

fn display(id: u32) -> DisplayInfo {
    DisplayInfo {
        id,
        name: "HDMI Screen".into(),
        width: 1280,
        height: 720,
        refresh_mhz: 60_000,
        modes: vec![DisplayMode {
            width: 1280,
            height: 720,
            refresh_mhz: 60_000,
        }],
        presentation: true,
        secure: true,
    }
}

fn list(ids: &[u32]) -> DisplayList {
    DisplayList {
        displays: ids.iter().map(|id| display(*id)).collect(),
        desktop_mode: false,
    }
}

const CONFIG: VideoConfig = VideoConfig {
    codec: VideoCodec::H264,
    width: 1108,
    height: 720,
    fps: 30,
    bitrate_kbps: 6000,
};

struct Phone {
    agent: Agent,
    addr: SocketAddr,
    sink: Arc<FakeVideoSink>,
    dir: PathBuf,
}

impl Drop for Phone {
    fn drop(&mut self) {
        self.agent.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn bare_phone() -> Phone {
    let dir = std::env::temp_dir().join(format!("hubbridge-agent-{}", hubbridge_link::random_id()));
    let agent = Agent::new(AgentConfig {
        state_dir: dir.clone(),
        name: "Test Phone".into(),
        model: "test".into(),
        os: "test".into(),
        app_version: "0.2.0".into(),
        usb_host: true,
    })
    .unwrap();
    agent.set_displays(list(&[8]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = agent.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    Phone {
        agent,
        addr,
        sink: Arc::new(FakeVideoSink::default()),
        dir,
    }
}

async fn phone() -> Phone {
    let p = bare_phone().await;
    p.agent.set_video_sink(p.sink.clone());
    p
}

/// One session of a host.
struct Host {
    tx: SecureSender<OwnedWriteHalf>,
    rx: mpsc::UnboundedReceiver<Frame>,
    next: u32,
    /// Events read while waiting for a reply.
    events: Vec<Message>,
}

/// A paired host: id and key.
#[derive(Clone)]
struct Identity {
    id: String,
    key: Secret,
}

async fn connect(addr: SocketAddr, id: &str, auth: ClientAuth) -> (Host, Option<Secret>) {
    let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
    let session = client_handshake(BufReader::new(r), w, id, id, auth).await.unwrap();
    let (frames, rx) = mpsc::unbounded_channel();
    let mut receiver = session.receiver;
    tokio::spawn(async move {
        while let Ok(frame) = receiver.recv().await {
            if frames.send(frame).is_err() {
                break;
            }
        }
    });
    (
        Host {
            tx: session.sender,
            rx,
            next: 1,
            events: Vec::new(),
        },
        session.new_key,
    )
}

async fn paired(phone: &Phone, id: &str) -> Identity {
    let pin = phone.agent.start_pairing();
    let (mut host, key) = connect(phone.addr, id, ClientAuth::Pair { pin }).await;
    host.send(0, Message::Bye).await;
    Identity {
        id: id.into(),
        key: key.unwrap(),
    }
}

async fn session(phone: &Phone, who: &Identity) -> Host {
    let key = who.key.clone();
    let auth = ClientAuth::Resume {
        key_for: Box::new(move |_| Some(key.clone())),
    };
    connect(phone.addr, &who.id, auth).await.0
}

impl Host {
    async fn send(&mut self, req: u32, msg: Message) {
        self.tx.send(&Frame { req, msg }).await.unwrap();
    }

    /// The next frame; `None` once the session closed.
    async fn recv(&mut self, limit: Duration) -> Option<Frame> {
        let frame = timeout(limit, self.rx.recv()).await.expect("no frame in time")?;
        match frame.msg {
            Message::Bye => None,
            _ => Some(frame),
        }
    }

    async fn request(&mut self, msg: Message) -> Message {
        let req = self.next;
        self.next += 1;
        self.send(req, msg).await;
        loop {
            let frame = self.recv(Duration::from_secs(8)).await.expect("session open");
            if frame.req == req {
                return frame.msg;
            }
            self.events.push(frame.msg);
        }
    }

    /// The next event that `want` accepts; others are skipped.
    async fn event(&mut self, limit: Duration, want: impl Fn(&Message) -> bool) -> Message {
        if let Some(at) = self.events.iter().position(&want) {
            return self.events.remove(at);
        }
        let deadline = Instant::now() + limit;
        loop {
            let frame = self
                .recv(deadline.saturating_duration_since(Instant::now()))
                .await
                .expect("session open");
            if want(&frame.msg) {
                return frame.msg;
            }
        }
    }

    /// Reads until the session closes.
    async fn closed(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .is_some()
        {}
    }

    /// No event matching `unwanted` arrives within `wait`.
    async fn never(&mut self, wait: Duration, unwanted: impl Fn(&Message) -> bool) {
        assert!(!self.events.iter().any(&unwanted), "{:?}", self.events);
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            assert!(!unwanted(&frame.msg), "unexpected {:?}", frame.msg);
        }
    }

    async fn start_mirror(&mut self, display_id: u32) -> u64 {
        match self
            .request(Message::MirrorStart {
                display_id,
                config: CONFIG,
            })
            .await
        {
            Message::MirrorStarted { stream_id } => stream_id,
            other => panic!("start refused: {other:?}"),
        }
    }

    async fn picture(&mut self, seq: u64, data: &[u8], keyframe: bool, config: bool) {
        let frame = VideoFrame {
            seq,
            pts_us: seq,
            keyframe,
            config,
            data: data.to_vec(),
        };
        self.send(0, Message::VideoFrame(frame)).await;
    }

    async fn ack(&mut self) -> u64 {
        match self.event(SOON, |m| matches!(m, Message::VideoAck { .. })).await {
            Message::VideoAck { seq } => seq,
            _ => unreachable!(),
        }
    }
}

fn is_stopped(m: &Message) -> bool {
    matches!(m, Message::MirrorStopped { .. })
}

fn code(m: &Message) -> ErrorCode {
    match m {
        Message::Error { code, .. } => *code,
        other => panic!("not an error: {other:?}"),
    }
}

/// A control and an attached media session with a running stream.
async fn mirroring(phone: &Phone) -> (Identity, Host, Host, u64) {
    let who = paired(phone, "mac").await;
    let mut control = session(phone, &who).await;
    let stream = control.start_mirror(8).await;
    let mut media = session(phone, &who).await;
    assert_eq!(
        media.request(Message::MirrorAttach { stream_id: stream }).await,
        Message::Done
    );
    (who, control, media, stream)
}

/// The sink is stopped exactly once (it happens on a thread of its own).
async fn stopped_once(phone: &Phone, stream: u64) {
    timeout(Duration::from_secs(5), async {
        while phone.sink.log(stream).unwrap().stops == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sink was not stopped");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(phone.sink.log(stream).unwrap().stops, 1);
}

async fn told(control: &mut Host, stream: u64, limit: Duration) -> MirrorEnd {
    match control.event(limit, is_stopped).await {
        Message::MirrorStopped {
            stream_id,
            reason,
            message,
        } => {
            assert_eq!(stream_id, stream);
            assert!(!message.is_empty());
            reason
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn display_events_reach_only_sessions_that_asked() {
    let phone = phone().await;
    let who = paired(&phone, "mac").await;
    let mut asked = session(&phone, &who).await;
    let mut old = session(&phone, &who).await;
    assert_eq!(asked.request(Message::GetDisplays).await, Message::Displays(list(&[8])));
    assert!(matches!(
        old.request(Message::GetAgentInfo).await,
        Message::AgentInfo(_)
    ));

    for ids in [&[8u32, 9][..], &[9], &[]] {
        phone.agent.set_displays(list(ids));
        let got = asked.event(SOON, |m| matches!(m, Message::DisplaysChanged(_))).await;
        assert_eq!(got, Message::DisplaysChanged(list(ids)));
    }
    // An unchanged list sends nothing.
    phone.agent.set_displays(list(&[]));
    asked
        .never(Duration::from_millis(400), |m| matches!(m, Message::DisplaysChanged(_)))
        .await;
    // The old host saw none of it and its session works.
    old.never(Duration::from_millis(100), |m| matches!(m, Message::DisplaysChanged(_)))
        .await;
    assert!(matches!(
        old.request(Message::Ping { seq: 1, sent_us: 2 }).await,
        Message::Pong { seq: 1, sent_us: 2 }
    ));
    phone.agent.set_devices(vec![fixtures::cp2102_uart(1)]);
    old.event(SOON, |m| matches!(m, Message::DeviceAdded(_))).await;
    assert!(old.events.is_empty());
}

#[tokio::test]
async fn without_a_sink_mirroring_is_unsupported() {
    let phone = bare_phone().await;
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    let refused = control
        .request(Message::MirrorStart {
            display_id: 8,
            config: CONFIG,
        })
        .await;
    assert_eq!(code(&refused), ErrorCode::Unsupported);
}

#[tokio::test]
async fn refused_starts() {
    let phone = phone().await;
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    let start = |display_id, config| Message::MirrorStart { display_id, config };

    let unknown = control.request(start(3, CONFIG)).await;
    assert_eq!(code(&unknown), ErrorCode::UnknownDisplay);
    assert!(matches!(&unknown, Message::Error { message, .. } if message == "the phone has no display 3"));
    for bad in [
        VideoConfig { width: 0, ..CONFIG },
        VideoConfig { height: 0, ..CONFIG },
        VideoConfig { fps: 0, ..CONFIG },
        VideoConfig { fps: 61, ..CONFIG },
    ] {
        assert_eq!(
            code(&control.request(start(8, bad)).await),
            ErrorCode::Unsupported,
            "{bad:?}"
        );
    }
    phone.sink.fail_start(Some("turn Samsung DeX off"));
    let failed = control.request(start(8, CONFIG)).await;
    assert_eq!(
        failed,
        Message::Error {
            code: ErrorCode::DisplayFailed,
            message: "turn Samsung DeX off".into()
        }
    );
    // The refused start did not keep the display.
    phone.sink.fail_start(None);
    let stream = control.start_mirror(8).await;
    assert_eq!(phone.sink.log(stream).unwrap().display_id, 8);
    let busy = control.request(start(8, CONFIG)).await;
    assert_eq!(code(&busy), ErrorCode::Busy);
    assert!(matches!(&busy, Message::Error { message, .. } if message == "this display is already showing a mirror"));
    assert_eq!(phone.sink.log(stream).unwrap().stops, 0);
}

#[tokio::test]
async fn attach_rules() {
    let phone = phone().await;
    let (who, mut control, mut media, stream) = mirroring(&phone).await;
    // The media session is marked, for the phone's computer count.
    let sessions = phone.agent.status().sessions;
    assert_eq!(sessions.iter().filter(|s| s.media).count(), 1);
    assert!(
        serde_json::to_string(&phone.agent.status())
            .unwrap()
            .contains("\"media\":true")
    );

    let other = paired(&phone, "intruder").await;
    let mut theirs = session(&phone, &other).await;
    let refused = theirs.request(Message::MirrorAttach { stream_id: stream }).await;
    assert_eq!(code(&refused), ErrorCode::Unsupported);
    let mut unknown = session(&phone, &who).await;
    let refused = unknown.request(Message::MirrorAttach { stream_id: stream ^ 1 }).await;
    assert_eq!(code(&refused), ErrorCode::Unsupported);
    let mut second = session(&phone, &who).await;
    let refused = second.request(Message::MirrorAttach { stream_id: stream }).await;
    assert_eq!(code(&refused), ErrorCode::Busy);
    // Not as a later request of a session.
    let refused = control.request(Message::MirrorAttach { stream_id: stream }).await;
    assert_eq!(code(&refused), ErrorCode::Unsupported);

    // The owner's stream keeps running.
    media.picture(1, SPS_PPS, false, true).await;
    assert_eq!(media.ack().await, 1);
    control.never(Duration::from_millis(100), is_stopped).await;
    assert_eq!(phone.sink.log(stream).unwrap().stops, 0);
}

#[tokio::test]
async fn frames_are_acknowledged_in_order_and_status_flows_without_frames() {
    let phone = phone().await;
    let (_who, _control, mut media, stream) = mirroring(&phone).await;
    phone.sink.set_delay(Duration::from_millis(20));
    media.picture(1, SPS_PPS, false, true).await;
    media.picture(2, &[SPS_PPS, IDR].concat(), true, false).await;
    for seq in 3..=12 {
        media.picture(seq, DELTA, false, false).await;
    }
    for seq in 1..=12 {
        assert_eq!(media.ack().await, seq);
    }
    assert_eq!(phone.sink.log(stream).unwrap().seqs, (1..=12).collect::<Vec<_>>());
    // A status every second with nothing flowing, with the lock state.
    media.events.clear();
    phone.agent.set_phone_locked(true);
    let locked = |m: &Message| matches!(m, Message::VideoStatus(s) if s.phone_locked && s.decoded == 11);
    media.event(Duration::from_millis(1500), locked).await;
    let began = Instant::now();
    media.event(Duration::from_millis(1500), locked).await;
    assert!(began.elapsed() > Duration::from_millis(700));
    // Pings are answered, and the platform can ask for a keyframe.
    assert!(matches!(
        media.request(Message::Ping { seq: 4, sent_us: 5 }).await,
        Message::Pong { seq: 4, .. }
    ));
    phone.agent.request_keyframe(stream);
    media.event(SOON, |m| *m == Message::VideoNeedKeyframe).await;
}

#[tokio::test]
async fn a_decoder_error_asks_for_a_keyframe_and_gives_up_on_the_second() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    // A delta before any keyframe is bad.
    media.picture(1, DELTA, false, false).await;
    media.event(SOON, |m| *m == Message::VideoNeedKeyframe).await;
    assert_eq!(media.ack().await, 1, "the window does not stay full");
    media.picture(2, &[SPS_PPS, IDR].concat(), true, false).await;
    assert_eq!(media.ack().await, 2);
    // An error, and the keyframe that should repair it fails too.
    phone.sink.fail_next(FrameResult::NeedKeyframe);
    media.picture(3, DELTA, false, false).await;
    assert_eq!(media.ack().await, 3);
    phone.sink.fail_next(FrameResult::NeedKeyframe);
    media.picture(4, &[SPS_PPS, IDR].concat(), true, false).await;
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::DecoderFailed);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_mirror_stop() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    assert_eq!(
        control.request(Message::MirrorStop { stream_id: stream }).await,
        Message::Done
    );
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
    control.never(Duration::from_millis(200), is_stopped).await;
    // Stopping again is fine, and the display is free.
    assert_eq!(
        control.request(Message::MirrorStop { stream_id: stream }).await,
        Message::Done
    );
    control.start_mirror(8).await;
}

#[tokio::test]
async fn end_by_display_removed() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    phone.agent.set_displays(list(&[9]));
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::DisplayRemoved);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_losing_the_video_session() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    media.tx.shutdown().await;
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::VideoSessionLost);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_another_message_on_the_video_session() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    media.send(9, Message::ListDevices).await;
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::VideoSessionLost);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_not_attaching() {
    let phone = phone().await;
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    let stream = control.start_mirror(8).await;
    let began = Instant::now();
    assert_eq!(
        told(&mut control, stream, ATTACH_TIMEOUT + SOON).await,
        MirrorEnd::NotAttached
    );
    assert!(began.elapsed() >= ATTACH_TIMEOUT - Duration::from_millis(100));
    stopped_once(&phone, stream).await;
    let mut late = session(&phone, &who).await;
    let refused = late.request(Message::MirrorAttach { stream_id: stream }).await;
    assert_eq!(code(&refused), ErrorCode::Unsupported);
}

#[tokio::test]
async fn end_by_a_stalled_decoder() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    // One call that does not return.
    phone.sink.set_delay(Duration::from_secs(4));
    media.picture(1, &[SPS_PPS, IDR].concat(), true, false).await;
    let began = Instant::now();
    assert_eq!(
        told(&mut control, stream, Duration::from_secs(3)).await,
        MirrorEnd::DecoderStalled
    );
    assert!(began.elapsed() >= SINK_STALL);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_a_decoder_without_input_buffers() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    for _ in 0..5000 {
        phone.sink.fail_next(FrameResult::NoInput);
    }
    media.picture(1, &[SPS_PPS, IDR].concat(), true, false).await;
    assert_eq!(
        told(&mut control, stream, Duration::from_secs(8)).await,
        MirrorEnd::DecoderStalled
    );
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn end_by_the_platform() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    phone
        .agent
        .mirror_failed(stream, MirrorEnd::SurfaceLost, "the picture was taken away");
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::SurfaceLost);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
    phone.agent.mirror_failed(stream, MirrorEnd::SurfaceLost, "again");
    control.never(Duration::from_millis(200), is_stopped).await;
}

#[tokio::test]
async fn end_with_the_control_session() {
    let phone = phone().await;
    let (who, mut control, mut media, stream) = mirroring(&phone).await;
    control.send(0, Message::Bye).await;
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
    let mut next = session(&phone, &who).await;
    next.start_mirror(8).await;
    next.never(Duration::from_millis(200), is_stopped).await;
}

#[tokio::test]
async fn end_by_agent_shutdown() {
    let phone = phone().await;
    let (_who, mut control, mut media, stream) = mirroring(&phone).await;
    phone.agent.shutdown();
    assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::AgentStopped);
    media.closed(SOON).await;
    stopped_once(&phone, stream).await;
}

#[tokio::test]
async fn a_display_removed_while_starting_refuses_the_start() {
    let phone = phone().await;
    phone.sink.set_start_delay(Duration::from_millis(500));
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    control
        .send(
            1,
            Message::MirrorStart {
                display_id: 8,
                config: CONFIG,
            },
        )
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    phone.agent.set_displays(list(&[]));
    let reply = control.recv(Duration::from_secs(3)).await.unwrap();
    assert_eq!(
        reply.msg,
        Message::Error {
            code: ErrorCode::DisplayFailed,
            message: "the monitor was disconnected".into()
        }
    );
    let stream = *phone.sink.logs().keys().next().unwrap();
    stopped_once(&phone, stream).await;
}

/// The sink thread reports why and goes away, closing its queue: the reason must win over
/// "the video queue closed", whichever the media session notices first.
#[tokio::test]
async fn the_sinks_reason_is_not_lost_when_its_queue_is_full() {
    let phone = phone().await;
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    // The race is a coin toss when it is lost: repeated until a regression cannot pass.
    for _ in 0..20 {
        let stream = control.start_mirror(8).await;
        let mut media = session(&phone, &who).await;
        assert_eq!(
            media.request(Message::MirrorAttach { stream_id: stream }).await,
            Message::Done
        );
        phone.sink.set_delay(Duration::from_millis(40));
        phone.sink.fail_next(FrameResult::Failed(
            MirrorEnd::SurfaceLost,
            "the picture was taken away".into(),
        ));
        // One in the sink, three queued and one waiting for room.
        for seq in 1..=6 {
            media.picture(seq, &[SPS_PPS, IDR].concat(), true, false).await;
        }
        assert_eq!(told(&mut control, stream, SOON).await, MirrorEnd::SurfaceLost);
        media.closed(SOON).await;
    }
}

#[tokio::test]
async fn a_start_that_timed_out_keeps_the_display_until_the_sink_returns() {
    let phone = phone().await;
    let who = paired(&phone, "mac").await;
    let mut control = session(&phone, &who).await;
    let start = Message::MirrorStart {
        display_id: 8,
        config: CONFIG,
    };
    phone.sink.set_start_delay(START_TIMEOUT + Duration::from_millis(1500));
    let late = control.request(start.clone()).await;
    assert_eq!(code(&late), ErrorCode::DisplayFailed);
    assert!(matches!(&late, Message::Error { message, .. } if message == "the phone did not show the picture in time"));
    // The phone is still opening its window.
    phone.sink.set_start_delay(Duration::ZERO);
    assert_eq!(code(&control.request(start).await), ErrorCode::Busy);
    // Once that start returned, its picture is removed and the display is free.
    timeout(Duration::from_secs(5), async {
        while !phone.sink.logs().values().any(|l| l.stops == 1) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the late start was not stopped");
    let abandoned = *phone.sink.logs().keys().next().unwrap();
    stopped_once(&phone, abandoned).await;
    control.start_mirror(8).await;
    control.never(Duration::from_millis(200), is_stopped).await;
}

/// The display can go between the first look at the list and the claim: `set_displays` has
/// already looked for streams to end then, so the claim has to look at the list again.
#[tokio::test]
async fn a_display_removed_before_the_claim_is_refused() {
    let phone = phone().await;
    phone.agent.set_displays(list(&[9]));
    let refused = phone.agent.reserve_display(1, 1, "mac".into(), 8).unwrap_err();
    assert_eq!(code(&refused), ErrorCode::UnknownDisplay);
    assert!(phone.agent.inner.streams.lock().unwrap().is_empty());
    phone.agent.reserve_display(2, 1, "mac".into(), 9).unwrap();
}
