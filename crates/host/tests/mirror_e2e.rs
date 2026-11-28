//! Mirroring end to end over real TCP on localhost: the agent core with a fake video decoder,
//! the host's mirror session and the synthetic frame source.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use hubbridge_agent::fake::FakeVideoSink;
use hubbridge_agent::video::{FrameResult, SinkStats, VideoSink};
use hubbridge_agent::{Agent, AgentConfig};
use hubbridge_host::mirror::pace::Gate;
use hubbridge_host::mirror::synthetic::{Controls, SyntheticSource};
use hubbridge_host::mirror::{
    EncodeConfig, Ending, FrameOutput, FrameSource, MirrorHandle, MirrorOptions, MirrorSession,
};
use hubbridge_host::{Client, ClientEvent, Store};
use hubbridge_proto::{DisplayInfo, DisplayList, ErrorCode, Message, VideoConfig, VideoFrame, h264};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hubbridge-test-{tag}-{}", hubbridge_link::random_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// When a frame reached the decoder and what it was.
#[derive(Debug, Clone, Copy)]
struct Seen {
    at: Instant,
    keyframe: bool,
    config: bool,
    parameter_sets: bool,
}

/// The fake decoder, with a record of what reached it.
#[derive(Default)]
struct Recorder {
    fake: FakeVideoSink,
    seen: Mutex<Vec<Seen>>,
}

impl VideoSink for Recorder {
    fn start(&self, stream: u64, display: &DisplayInfo, config: &VideoConfig) -> Result<(), String> {
        self.fake.start(stream, display, config)
    }

    fn frame(&self, stream: u64, frame: &VideoFrame) -> FrameResult {
        self.seen.lock().unwrap().push(Seen {
            at: Instant::now(),
            keyframe: frame.keyframe,
            config: frame.config,
            parameter_sets: h264::has_parameter_sets(&frame.data),
        });
        self.fake.frame(stream, frame)
    }

    fn stats(&self, stream: u64) -> SinkStats {
        self.fake.stats(stream)
    }

    fn stop(&self, stream: u64) {
        self.fake.stop(stream);
    }
}

fn displays(ids: &[u32]) -> DisplayList {
    DisplayList {
        displays: ids
            .iter()
            .map(|&id| DisplayInfo {
                id,
                name: "HDMI Screen".into(),
                width: 1280,
                height: 720,
                refresh_mhz: 60_000,
                modes: vec![],
                presentation: true,
                secure: false,
            })
            .collect(),
        desktop_mode: false,
    }
}

struct Phone {
    agent: Agent,
    addr: SocketAddr,
    sink: Arc<Recorder>,
    dir: PathBuf,
}

impl Drop for Phone {
    fn drop(&mut self) {
        self.agent.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn phone_with_version(app_version: &str) -> Phone {
    let dir = temp_dir("mirror-agent");
    let agent = Agent::new(AgentConfig {
        state_dir: dir.clone(),
        name: "Test Phone".into(),
        model: "test".into(),
        os: "test".into(),
        app_version: app_version.into(),
        usb_host: true,
    })
    .unwrap();
    agent.set_displays(displays(&[8]));
    let sink = Arc::new(Recorder::default());
    agent.set_video_sink(sink.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = agent.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    Phone { agent, addr, sink, dir }
}

async fn phone() -> Phone {
    phone_with_version("0.2.0").await
}

async fn paired(phone: &Phone, addr: SocketAddr) -> (Arc<Client>, Store) {
    let mut store = Store::load(&temp_dir("mirror-host").join("host.json")).unwrap();
    let pin = phone.agent.start_pairing();
    Client::pair(&[phone.addr], &mut store, &pin)
        .await
        .unwrap()
        .close()
        .await;
    let control = Client::connect(&[addr], &mut store).await.unwrap();
    (Arc::new(control), store)
}

/// A running mirror whose control events reach the handle, as in the window and the CLI.
struct Mirror {
    handle: Arc<MirrorHandle>,
    source: Controls,
}

fn forward(control: &Client, handle: &Arc<MirrorHandle>) {
    let mut events = control.take_events().unwrap();
    let handle = Arc::clone(handle);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            handle.on_control_event(&event);
        }
    });
}

async fn mirror_with(control: &Arc<Client>, store: &mut Store, source: Box<dyn FrameSource>) -> Result<MirrorHandle> {
    MirrorSession::start(control, store, 8, MirrorOptions::default(), source).await
}

async fn mirror(control: &Arc<Client>, store: &mut Store) -> Mirror {
    let source = SyntheticSource::new(3024, 1964);
    let controls = source.controls();
    let handle = Arc::new(mirror_with(control, store, Box::new(source)).await.unwrap());
    forward(control, &handle);
    Mirror {
        handle,
        source: controls,
    }
}

async fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    timeout(limit, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within {limit:?}: {what}"));
}

fn decoded(phone: &Phone, stream: u64) -> u64 {
    phone.sink.fake.log(stream).map_or(0, |l| l.decoded)
}

async fn ended(mirror: &Mirror, limit: Duration) -> Ending {
    let ending = timeout(limit, mirror.handle.done())
        .await
        .expect("the mirror did not end");
    wait_until(Duration::from_secs(2), "the source stops", || {
        mirror.source.stopped.load(Ordering::SeqCst)
    })
    .await;
    ending
}

async fn next_stats(handle: &MirrorHandle) -> hubbridge_host::mirror::stats::Snapshot {
    let mut stats = handle.stats();
    stats.mark_unchanged();
    timeout(Duration::from_secs(3), stats.changed()).await.unwrap().unwrap();
    stats.borrow().clone().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn start_frames_acks_stats_stop() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let began = Instant::now();
    let m = mirror(&control, &mut store).await;
    let stream = m.handle.stream_id();
    // A 3024×1964 picture on 1280×720.
    let config = m.handle.config();
    assert_eq!((config.width, config.height, config.fps), (1108, 720, 30));
    assert_eq!(config.bitrate_kbps, 5983);
    assert_eq!(m.handle.display().name, "HDMI Screen");

    wait_until(Duration::from_secs(3), "frames arrive", || {
        decoded(&phone, stream) >= 10
    })
    .await;
    assert!(began.elapsed() < Duration::from_secs(3));
    {
        let seen = phone.sink.seen.lock().unwrap();
        assert!(seen[0].config && seen[0].parameter_sets);
        assert!(seen[1].keyframe && seen[1].parameter_sets);
    }
    next_stats(&m.handle).await;
    next_stats(&m.handle).await;
    let stats = next_stats(&m.handle).await;
    assert!(stats.fps > 15.0 && stats.kbps > 100.0, "{stats}");
    assert!(stats.delay_ms.is_some_and(|d| d < 400.0), "{stats}");
    assert!(stats.phone_fps.is_some_and(|f| f > 15.0), "{stats}");
    assert!(stats.keyframes >= 1);
    assert_eq!(stats.phone_bad, 0);

    let ending = m.handle.stop().await;
    assert!(ending.by_user, "{ending:?}");
    assert!(m.source.stopped.load(Ordering::SeqCst));
    wait_until(Duration::from_secs(1), "the phone removes the picture", || {
        phone.sink.fake.log(stream).unwrap().stops == 1
    })
    .await;
    wait_until(Duration::from_secs(2), "the video session closes", || {
        phone.agent.status().sessions.iter().all(|s| !s.media)
    })
    .await;
    control.ping(1).await.expect("the control session stays");
    assert_eq!(phone.sink.fake.log(stream).unwrap().bad, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_decoder_skips_frames_and_keeps_the_delay_bounded() {
    let phone = phone().await;
    phone.sink.fake.set_delay(Duration::from_millis(100));
    let (control, mut store) = paired(&phone, phone.addr).await;
    let m = mirror(&control, &mut store).await;
    let mut last = None;
    let watch_window = async {
        loop {
            assert!(m.handle.in_flight() <= 2);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    let read = async {
        next_stats(&m.handle).await;
        let first = next_stats(&m.handle).await;
        for _ in 0..2 {
            last = Some(next_stats(&m.handle).await);
        }
        first
    };
    let first = tokio::select! {
        _ = watch_window => unreachable!(),
        first = read => first,
    };
    let last = last.unwrap();
    assert!(last.skipped > first.skipped + 20, "{first}\n{last}");
    assert!(last.fps > 5.0 && last.fps < 13.0, "{last}");
    assert!(last.delay_max_ms.is_some_and(|d| d < 400.0), "{last}");
    m.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keyframe_request_is_served_within_half_a_second() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let m = mirror(&control, &mut store).await;
    let stream = m.handle.stream_id();
    // Past the half second after the first keyframe.
    tokio::time::sleep(Duration::from_millis(800)).await;
    for still in [false, true] {
        m.source.still.store(still, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let before = phone.sink.seen.lock().unwrap().len();
        let asked = Instant::now();
        phone.agent.request_keyframe(stream);
        wait_until(Duration::from_secs(2), "a keyframe", || {
            phone.sink.seen.lock().unwrap()[before..].iter().any(|s| s.keyframe)
        })
        .await;
        let key = *phone.sink.seen.lock().unwrap()[before..]
            .iter()
            .find(|s| s.keyframe)
            .unwrap();
        assert!(key.parameter_sets);
        assert!(
            key.at - asked < Duration::from_millis(500),
            "still={still}: {:?}",
            key.at - asked
        );
    }
    // The decoder's own request after an error.
    m.source.still.store(false, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let keyframes = phone.sink.fake.log(stream).unwrap().keyframes;
    phone.sink.fake.fail_next(FrameResult::NeedKeyframe);
    wait_until(Duration::from_secs(1), "the picture recovers", || {
        phone.sink.fake.log(stream).unwrap().keyframes > keyframes
    })
    .await;
    let recovered = decoded(&phone, stream);
    wait_until(Duration::from_secs(1), "frames flow again", || {
        decoded(&phone, stream) > recovered + 5
    })
    .await;
    m.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unplugged_display_ends_the_mirror() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let m = mirror(&control, &mut store).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    phone.agent.set_displays(displays(&[]));
    let ending = ended(&m, Duration::from_secs(2)).await;
    assert_eq!(ending.reason, "the monitor was disconnected");
    assert!(!ending.by_user);
    control.ping(1).await.unwrap();
}

/// A proxy for any number of connections; each can be killed or silenced on its own.
struct Proxy {
    addr: SocketAddr,
    links: Arc<Mutex<Vec<watch::Sender<Link>>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Link {
    Up,
    /// Nothing passes and nothing is closed, as when the Wi-Fi is gone.
    Silent,
    Dead,
}

async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    mut link: watch::Receiver<Link>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = tokio::select! {
            n = from.read(&mut buf) => match n {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            },
            _ = link.wait_for(|l| *l == Link::Dead) => break,
        };
        if *link.borrow() == Link::Silent && link.wait_for(|l| *l != Link::Silent).await.is_err() {
            break;
        }
        if *link.borrow() == Link::Dead || to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
}

impl Proxy {
    async fn start(agent: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let links = Arc::new(Mutex::new(Vec::new()));
        let all = Arc::clone(&links);
        tokio::spawn(async move {
            while let Ok((host, _)) = listener.accept().await {
                let Ok(phone) = TcpStream::connect(agent).await else {
                    break;
                };
                let (link, state) = watch::channel(Link::Up);
                all.lock().unwrap().push(link);
                let (host_r, host_w) = host.into_split();
                let (phone_r, phone_w) = phone.into_split();
                tokio::spawn(pipe(host_r, phone_w, state.clone()));
                tokio::spawn(pipe(phone_r, host_w, state));
            }
        });
        Proxy { addr, links }
    }

    fn set(&self, connection: usize, link: Link) {
        self.links.lock().unwrap()[connection].send_replace(link);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_video_session_reports_the_phone_reason() {
    let phone = phone().await;
    let proxy = Proxy::start(phone.addr).await;
    let (control, mut store) = paired(&phone, proxy.addr).await;
    let m = mirror(&control, &mut store).await;
    let stream = m.handle.stream_id();
    wait_until(Duration::from_secs(2), "frames", || decoded(&phone, stream) > 3).await;
    // Connection 0 is the control session, 1 the video session.
    proxy.set(1, Link::Dead);
    let ending = ended(&m, Duration::from_secs(3)).await;
    assert_eq!(ending.reason, "the video connection was lost");
    wait_until(Duration::from_secs(1), "the picture is removed", || {
        phone.sink.fake.log(stream).unwrap().stops == 1
    })
    .await;
    control.ping(1).await.expect("the control session stays up");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_network_ends_the_mirror_within_six_seconds() {
    let phone = phone().await;
    let proxy = Proxy::start(phone.addr).await;
    let (control, mut store) = paired(&phone, proxy.addr).await;
    let m = mirror(&control, &mut store).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lost = Instant::now();
    proxy.set(0, Link::Silent);
    proxy.set(1, Link::Silent);
    let ending = ended(&m, Duration::from_secs(8)).await;
    assert_eq!(ending.reason, "the connection to the phone was lost");
    assert!(lost.elapsed() < Duration::from_secs(6), "{:?}", lost.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_control_session_stops_the_source() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let source = SyntheticSource::new(1920, 1080);
    let controls = source.controls();
    let handle = Arc::new(mirror_with(&control, &mut store, Box::new(source)).await.unwrap());
    let stream = handle.stream_id();
    let m = Mirror {
        handle,
        source: controls,
    };
    Arc::try_unwrap(control)
        .ok()
        .expect("the mirror does not keep the control session")
        .close()
        .await;
    let ending = ended(&m, Duration::from_secs(3)).await;
    assert!(ending.reason.contains("connection"), "{ending:?}");
    wait_until(Duration::from_secs(2), "the picture is removed", || {
        phone.sink.fake.log(stream).unwrap().stops == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_host_cannot_attach_and_a_busy_display_is_refused() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let m = mirror(&control, &mut store).await;
    let stream = m.handle.stream_id();

    let (_other, mut other_store) = paired(&phone, phone.addr).await;
    let intruder = Client::connect(&[phone.addr], &mut other_store).await.unwrap();
    let err = intruder
        .request(Message::MirrorAttach { stream_id: stream })
        .await
        .unwrap_err();
    let code = err.downcast_ref::<hubbridge_host::client::AgentError>().unwrap().code;
    assert_eq!(code, ErrorCode::Unsupported);

    let second = mirror_with(&control, &mut store, Box::new(SyntheticSource::new(640, 360))).await;
    let why = second.err().expect("refused").to_string();
    assert_eq!(why, "this display is already showing a mirror");
    let unknown = MirrorSession::start(
        &control,
        &mut store,
        3,
        MirrorOptions::default(),
        Box::new(SyntheticSource::new(640, 360)),
    )
    .await;
    assert_eq!(unknown.err().unwrap().to_string(), "the phone has no display 3");

    // The first mirror goes on.
    let before = decoded(&phone, stream);
    wait_until(Duration::from_secs(2), "frames still flow", || {
        decoded(&phone, stream) > before + 5
    })
    .await;
    assert_eq!(phone.sink.fake.log(stream).unwrap().stops, 0);
    m.handle.stop().await;
}

/// Longer than the agent's idle timeout, in real time.
#[tokio::test(flavor = "multi_thread")]
async fn a_still_screen_keeps_the_mirror_alive() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let m = mirror(&control, &mut store).await;
    let stream = m.handle.stream_id();
    // The control session must not idle out either: the window and the CLI ping it.
    let pinger = Arc::clone(&control);
    let pings = tokio::spawn(async move {
        for seq in 1.. {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if pinger.ping(seq).await.is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    m.source.still.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = decoded(&phone, stream);
    assert!(hubbridge_agent::IDLE_TIMEOUT < Duration::from_secs(25));
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert!(timeout(Duration::from_millis(50), m.handle.done()).await.is_err());
    assert_eq!(decoded(&phone, stream), before, "a still screen sends nothing");
    let stats = next_stats(&m.handle).await;
    assert_eq!(stats.fps, 0.0, "{stats}");
    assert!(stats.rtt_ms.is_some(), "keepalive pings: {stats}");
    assert_eq!(phone.sink.fake.log(stream).unwrap().stops, 0);
    // The next change appears.
    m.source.still.store(false, Ordering::Relaxed);
    wait_until(Duration::from_secs(1), "the next change", || {
        decoded(&phone, stream) > before
    })
    .await;
    pings.abort();
    m.handle.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn video_does_not_delay_control_traffic() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let source = SyntheticSource::new(3024, 1964).with_frame_bytes(25_000);
    let handle = mirror_with(&control, &mut store, Box::new(source)).await.unwrap();
    let mut rtts = Vec::new();
    for seq in 1..=60 {
        rtts.push(control.ping(seq).await.unwrap());
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    rtts.sort();
    let median = rtts[rtts.len() / 2];
    assert!(
        median < Duration::from_millis(20),
        "median {median:?}, worst {:?}",
        rtts.last()
    );
    assert!(decoded(&phone, handle.stream_id()) > 20);
    assert_eq!(control.list_devices().await.unwrap(), vec![]);
    handle.stop().await;
}

/// A source that records whether it was started.
struct Untouched(Arc<Mutex<bool>>);

impl FrameSource for Untouched {
    fn native_size(&mut self) -> Result<(u32, u32)> {
        Ok((1280, 720))
    }

    fn start(&mut self, _: EncodeConfig, _: Gate, _: FrameOutput) -> Result<()> {
        *self.0.lock().unwrap() = true;
        Ok(())
    }

    fn kick(&mut self) {}
    fn request_keyframe(&mut self) {}
    fn stop(&mut self) {}
}

#[tokio::test]
async fn an_old_phone_app_is_never_asked_about_displays() {
    let phone = phone_with_version("0.1.0").await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    let started = Arc::new(Mutex::new(false));
    let err = mirror_with(&control, &mut store, Box::new(Untouched(started.clone())))
        .await
        .err()
        .unwrap();
    assert!(err.to_string().starts_with("update the phone app"), "{err}");
    assert!(!*started.lock().unwrap());
    // Had the host sent GetDisplays, this change would reach it as an event.
    let mut events = control.take_events().unwrap();
    phone.agent.set_displays(displays(&[9]));
    if let Ok(event) = timeout(Duration::from_millis(500), events.recv()).await {
        assert!(
            !matches!(event, Some(ClientEvent::Message(Message::DisplaysChanged(_)))),
            "the agent saw GetDisplays"
        );
        panic!("unexpected event {event:?}");
    }
    control.ping(1).await.expect("the session works");
}

/// `send_event` frames keep their order among requests, and `displays` returns the list.
#[tokio::test]
async fn events_keep_their_order_and_displays_are_listed() {
    let phone = phone().await;
    let (control, mut store) = paired(&phone, phone.addr).await;
    assert_eq!(control.displays().await.unwrap(), displays(&[8]));

    // On a video session the acknowledgements show the order in which frames arrived.
    let config = VideoConfig {
        codec: hubbridge_proto::VideoCodec::H264,
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 2000,
    };
    let Message::MirrorStarted { stream_id } = control
        .request(Message::MirrorStart { display_id: 8, config })
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    let media = Client::connect(&[phone.addr], &mut store).await.unwrap();
    assert_eq!(
        media.request(Message::MirrorAttach { stream_id }).await.unwrap(),
        Message::Done
    );
    let mut events = media.take_events().unwrap();
    let sps_pps = vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2];
    for seq in 1..=20 {
        let frame = VideoFrame {
            seq,
            pts_us: seq,
            keyframe: false,
            config: true,
            data: sps_pps.clone(),
        };
        media.send_event(Message::VideoFrame(frame)).await.unwrap();
        if seq % 5 == 0 {
            media.ping(seq as u32).await.unwrap();
        }
    }
    let mut acked = Vec::new();
    while acked.len() < 20 {
        match timeout(Duration::from_secs(2), events.recv()).await.unwrap() {
            Some(ClientEvent::Message(Message::VideoAck { seq })) => acked.push(seq),
            Some(ClientEvent::Message(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(acked, (1..=20).collect::<Vec<_>>());
}
