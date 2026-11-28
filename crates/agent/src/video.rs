//! Phone displays and mirror streams (roadmap step 3a).
//!
//! The platform feeds the display list with [`Agent::set_displays`] and shows pictures behind
//! the [`VideoSink`] trait. A host starts a stream on its control session, then attaches a
//! second ("media") session to it and sends the frames there.
//!
//! A stream ends exactly once, through [`Agent::end_stream`]: whoever removes it from the
//! registry stops the sink, tells the owner (when the phone side ended it) and drops the
//! command channel, which closes the media session.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hubbridge_link::{LinkError, SecureSender};
use hubbridge_proto::{
    DisplayInfo, DisplayList, ErrorCode, Frame, Message, MirrorEnd, VideoCodec, VideoConfig, VideoFrame, VideoStatus,
};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::{Agent, IDLE_TIMEOUT, stopped};

/// Time the platform gets to show the surface and prepare the decoder.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
/// A started stream without a media session ends after this.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// A decoder that takes no frame for this long ends the mirror.
pub const SINK_STALL: Duration = Duration::from_secs(2);
/// How often the host gets a [`VideoStatus`].
pub const STATUS_EVERY: Duration = Duration::from_secs(1);
/// Frames between the media session and the sink thread: the host's window of two, and a
/// decoder configuration.
const SINK_QUEUE: usize = 3;
const MAX_FPS: u32 = 60;

/// What the decoder did with a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameResult {
    /// The decoder input took it.
    Queued,
    /// No input buffer in time: the same frame is offered again.
    NoInput,
    /// The frame could not be decoded: the host must send a keyframe.
    NeedKeyframe,
    /// The stream cannot go on.
    Failed(MirrorEnd, String),
}

/// Decoder counters since the stream started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkStats {
    pub decoded: u64,
    pub dropped: u64,
    pub bad: u64,
    /// Average input-to-output time over the last second.
    pub decode_us: u32,
}

/// The platform's video output. All methods may block and are only called from threads that
/// may. `stop` can arrive while `frame` of the same stream is still running.
pub trait VideoSink: Send + Sync {
    /// Show a surface on `display` and prepare a decoder. The error says why, and what to do.
    fn start(&self, stream: u64, display: &DisplayInfo, config: &VideoConfig) -> Result<(), String>;
    fn frame(&self, stream: u64, frame: &VideoFrame) -> FrameResult;
    fn stats(&self, stream: u64) -> SinkStats;
    /// Remove the picture. Called once per started stream.
    fn stop(&self, stream: u64);
}

/// What the platform asks of a running media session.
#[derive(Debug)]
pub(crate) enum MediaCmd {
    NeedKeyframe,
}

pub(crate) struct Stream {
    display_id: u32,
    host_id: String,
    /// Control session that started it.
    owner: u64,
    /// The sink accepted the start (only then does the host know the stream).
    started: bool,
    /// Why it ended before the start finished.
    failed: Option<String>,
    attached: bool,
    cmd: mpsc::UnboundedSender<MediaCmd>,
    cmd_rx: Option<mpsc::UnboundedReceiver<MediaCmd>>,
}

/// A media session that attached to its stream.
pub(crate) struct Attachment {
    pub(crate) stream: u64,
    cmd: mpsc::UnboundedReceiver<MediaCmd>,
}

fn error(code: ErrorCode, message: impl Into<String>) -> Message {
    Message::Error {
        code,
        message: message.into(),
    }
}

fn new_stream_id() -> u64 {
    let id = hubbridge_link::random_id();
    u64::from_str_radix(&id[..16], 16).expect("random_id is hex")
}

impl Agent {
    /// Where mirrored pictures go (Android: the monitor; simulator: a fake).
    pub fn set_video_sink(&self, sink: Arc<dyn VideoSink>) {
        *self.inner.sink.write().unwrap() = Some(sink);
    }

    pub fn displays(&self) -> DisplayList {
        self.inner.displays.read().unwrap().clone()
    }

    /// Replace the display list. Hosts that asked for displays get `DisplaysChanged`;
    /// mirrors on a display that is gone end.
    pub fn set_displays(&self, list: DisplayList) {
        let gone: BTreeSet<u32> = {
            let mut current = self.inner.displays.write().unwrap();
            if *current == list {
                return;
            }
            let gone = current
                .displays
                .iter()
                .map(|d| d.id)
                .filter(|id| !list.displays.iter().any(|d| d.id == *id))
                .collect();
            *current = list.clone();
            // Sent under the lock, so events arrive in the order of the changes.
            let _ = self.inner.events.send(Message::DisplaysChanged(list));
            gone
        };
        for stream in self.streams_where(|s| gone.contains(&s.display_id)) {
            self.end_stream(
                stream,
                Some((MirrorEnd::DisplayRemoved, "the monitor was disconnected")),
            );
        }
    }

    /// The platform cannot go on showing a stream.
    pub fn mirror_failed(&self, stream: u64, reason: MirrorEnd, message: &str) {
        self.end_stream(stream, Some((reason, message)));
    }

    /// The platform's decoder needs a keyframe.
    pub fn request_keyframe(&self, stream: u64) {
        if let Some(s) = self.inner.streams.lock().unwrap().get(&stream) {
            let _ = s.cmd.send(MediaCmd::NeedKeyframe);
        }
    }

    /// Reported to hosts in every `VideoStatus`.
    pub fn set_phone_locked(&self, locked: bool) {
        self.inner.phone_locked.store(locked, Ordering::Relaxed);
    }

    fn streams_where(&self, which: impl Fn(&Stream) -> bool) -> Vec<u64> {
        self.inner
            .streams
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| which(s))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Stop the sink on a thread of its own: it may block, and callers may not.
    fn stop_sink(&self, stream: u64) {
        if let Some(sink) = self.inner.sink.read().unwrap().clone() {
            std::thread::spawn(move || sink.stop(stream));
        }
    }

    /// End a stream. With `told`, the owner gets `MirrorStopped`.
    pub(crate) fn end_stream(&self, stream: u64, told: Option<(MirrorEnd, &str)>) {
        let ended = {
            let mut streams = self.inner.streams.lock().unwrap();
            match streams.get_mut(&stream) {
                None => return,
                // Still starting: the starter cleans up and answers with the reason.
                Some(s) if !s.started => {
                    let why = told.map_or("the mirror was cancelled", |(_, m)| m);
                    s.failed.get_or_insert_with(|| why.to_string());
                    return;
                }
                Some(_) => streams.remove(&stream),
            }
        };
        let Some(ended) = ended else { return };
        info!(stream, reason = ?told.map(|t| t.0), "mirror ended");
        self.stop_sink(stream);
        if let Some((reason, message)) = told
            && let Some(owner) = self.inner.outboxes.lock().unwrap().get(&ended.owner)
        {
            let _ = owner.send(Frame {
                req: 0,
                msg: Message::MirrorStopped {
                    stream_id: stream,
                    reason,
                    message: message.to_string(),
                },
            });
        }
    }

    /// The control session `sid` ended: its streams end without an event.
    pub(crate) fn end_streams_of(&self, sid: u64) {
        for stream in self.streams_where(|s| s.owner == sid) {
            self.end_stream(stream, None);
        }
    }

    pub(crate) fn end_all_streams(&self) {
        for stream in self.streams_where(|_| true) {
            self.end_stream(
                stream,
                Some((MirrorEnd::AgentStopped, "HubBridge on the phone was stopped")),
            );
        }
    }

    pub(crate) fn mirror_stop(&self, sid: u64, stream: u64) -> Message {
        match self.inner.streams.lock().unwrap().get(&stream) {
            Some(s) if s.owner != sid => {
                return error(ErrorCode::Unsupported, "this stream belongs to another session");
            }
            _ => {}
        }
        // Unknown: it ended already.
        self.end_stream(stream, None);
        Message::Done
    }

    /// Checks and reserves the display at once; the future asks the platform to show it.
    // The error is the reply itself, as in `usb_request`.
    #[allow(clippy::result_large_err)]
    pub(crate) fn mirror_start(
        &self,
        sid: u64,
        display_id: u32,
        config: VideoConfig,
    ) -> Result<impl Future<Output = Message> + Send + 'static, Message> {
        let Some(sink) = self.inner.sink.read().unwrap().clone() else {
            return Err(error(ErrorCode::Unsupported, "this agent has no video output"));
        };
        let supported = match config.codec {
            VideoCodec::H264 => config.width > 0 && config.height > 0 && (1..=MAX_FPS).contains(&config.fps),
        };
        if !supported {
            return Err(error(
                ErrorCode::Unsupported,
                format!("cannot show {}x{} at {} fps", config.width, config.height, config.fps),
            ));
        }
        let Some(display) = self.displays().displays.into_iter().find(|d| d.id == display_id) else {
            return Err(error(
                ErrorCode::UnknownDisplay,
                format!("the phone has no display {display_id}"),
            ));
        };
        let Some(host_id) = self.inner.sessions.lock().unwrap().get(&sid).map(|s| s.host_id.clone()) else {
            return Err(error(ErrorCode::Internal, "unknown session"));
        };
        let stream = new_stream_id();
        {
            let mut streams = self.inner.streams.lock().unwrap();
            if streams.values().any(|s| s.display_id == display_id) {
                return Err(error(ErrorCode::Busy, "this display is already showing a mirror"));
            }
            let (cmd, cmd_rx) = mpsc::unbounded_channel();
            streams.insert(
                stream,
                Stream {
                    display_id,
                    host_id,
                    owner: sid,
                    started: false,
                    failed: None,
                    attached: false,
                    cmd,
                    cmd_rx: Some(cmd_rx),
                },
            );
        }
        let agent = self.clone();
        Ok(async move {
            let starter = agent.clone();
            let start = tokio::task::spawn_blocking(move || starter.start_sink(sink, stream, &display, &config));
            match tokio::time::timeout(START_TIMEOUT, start).await {
                Ok(Ok(Ok(()))) => {
                    let watch = agent.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(ATTACH_TIMEOUT).await;
                        let waiting = watch
                            .inner
                            .streams
                            .lock()
                            .unwrap()
                            .get(&stream)
                            .is_some_and(|s| !s.attached);
                        if waiting {
                            watch.end_stream(
                                stream,
                                Some((MirrorEnd::NotAttached, "the video connection was not opened in time")),
                            );
                        }
                    });
                    info!(stream, display = display_id, "mirror started");
                    Message::MirrorStarted { stream_id: stream }
                }
                Ok(Ok(Err(why))) => error(ErrorCode::DisplayFailed, why),
                Ok(Err(e)) => {
                    agent.inner.streams.lock().unwrap().remove(&stream);
                    error(ErrorCode::Internal, format!("video output failed: {e}"))
                }
                Err(_) => {
                    // A start that still succeeds finds no entry and stops the sink.
                    let removed = agent.inner.streams.lock().unwrap().remove(&stream);
                    if removed.is_some_and(|s| s.started) {
                        agent.stop_sink(stream);
                    }
                    error(ErrorCode::DisplayFailed, "the phone did not show the picture in time")
                }
            }
        })
    }

    /// Blocking: ask the sink, then settle the registry entry.
    fn start_sink(
        &self,
        sink: Arc<dyn VideoSink>,
        stream: u64,
        display: &DisplayInfo,
        config: &VideoConfig,
    ) -> Result<(), String> {
        let result = sink.start(stream, display, config);
        let mut streams = self.inner.streams.lock().unwrap();
        let cancelled = match streams.get_mut(&stream) {
            Some(s) => match (&result, s.failed.take()) {
                (Ok(()), None) => {
                    s.started = true;
                    return Ok(());
                }
                (_, failed) => failed,
            },
            None => Some("the phone did not show the picture in time".to_string()),
        };
        streams.remove(&stream);
        drop(streams);
        match result {
            Ok(()) => {
                sink.stop(stream);
                Err(cancelled.unwrap_or_else(|| "the mirror was cancelled".into()))
            }
            Err(why) => Err(why),
        }
    }

    /// `MirrorAttach` from a session of `host_id` that has sent nothing else.
    // The error is the reply itself.
    #[allow(clippy::result_large_err)]
    pub(crate) fn attach(&self, host_id: &str, stream: u64) -> Result<Attachment, Message> {
        let mut streams = self.inner.streams.lock().unwrap();
        // Another host's stream looks like no stream at all.
        let Some(s) = streams.get_mut(&stream).filter(|s| s.host_id == host_id && s.started) else {
            return Err(error(ErrorCode::Unsupported, "no such video stream"));
        };
        match s.cmd_rx.take() {
            Some(cmd) => {
                s.attached = true;
                Ok(Attachment { stream, cmd })
            }
            None => Err(error(ErrorCode::Busy, "this stream already has its video connection")),
        }
    }

    /// The rest of a session that attached to a stream: frames in; acknowledgements, keyframe
    /// requests and the status out.
    pub(crate) async fn run_media<W>(
        &self,
        attachment: Attachment,
        tx: &mut SecureSender<W>,
        in_rx: &mut mpsc::Receiver<Result<Frame, LinkError>>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), LinkError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let Attachment { stream, mut cmd } = attachment;
        let sink = self.inner.sink.read().unwrap().clone();
        let result = match sink {
            Some(sink) => self.media_loop(stream, sink, &mut cmd, tx, in_rx, &mut stop).await,
            None => Ok(()),
        };
        // Still registered: the video session is what failed.
        self.end_stream(
            stream,
            Some((MirrorEnd::VideoSessionLost, "the video connection was lost")),
        );
        result
    }

    async fn media_loop<W>(
        &self,
        stream: u64,
        sink: Arc<dyn VideoSink>,
        cmd: &mut mpsc::UnboundedReceiver<MediaCmd>,
        tx: &mut SecureSender<W>,
        in_rx: &mut mpsc::Receiver<Result<Frame, LinkError>>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<(), LinkError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let busy_since = Arc::new(Mutex::new(None));
        let frames = spawn_sink_thread(stream, Arc::clone(&sink), out_tx.clone(), Arc::clone(&busy_since));
        let status = tokio::spawn(report_status(self.clone(), stream, sink, out_tx));

        // A frame waiting for room in the sink queue; nothing more is read meanwhile.
        let mut waiting: Option<VideoFrame> = None;
        let mut idle = tokio::time::Instant::now() + IDLE_TIMEOUT;
        let mut watchdog = tokio::time::interval(Duration::from_millis(100));
        let result = loop {
            tokio::select! {
                _ = stopped(stop) => break Ok(()),
                // `None`: the stream ended and dropped the sender.
                c = cmd.recv() => match c {
                    Some(MediaCmd::NeedKeyframe) => {
                        if let Err(e) = tx.send(&Frame { req: 0, msg: Message::VideoNeedKeyframe }).await {
                            break Err(e);
                        }
                    }
                    None => break Ok(()),
                },
                permit = frames.reserve(), if waiting.is_some() => match permit {
                    Ok(permit) => permit.send(waiting.take().expect("checked by the guard")),
                    Err(_) => break Err(LinkError::Protocol("video output stopped")),
                },
                incoming = in_rx.recv(), if waiting.is_none() => {
                    idle = tokio::time::Instant::now() + IDLE_TIMEOUT;
                    let frame = match incoming {
                        None => break Err(LinkError::Closed),
                        Some(Err(e)) => break Err(e),
                        Some(Ok(f)) => f,
                    };
                    match frame.msg {
                        Message::VideoFrame(f) => waiting = Some(f),
                        Message::Ping { seq, sent_us } => {
                            let pong = Frame { req: frame.req, msg: Message::Pong { seq, sent_us } };
                            if let Err(e) = tx.send(&pong).await {
                                break Err(e);
                            }
                        }
                        Message::Bye => break Ok(()),
                        _ => break Err(LinkError::Protocol("unexpected message on a video session")),
                    }
                }
                Some(msg) = out_rx.recv() => {
                    // The sink thread reports a failure as the event it becomes.
                    if let Message::MirrorStopped { reason, message, .. } = &msg {
                        self.end_stream(stream, Some((*reason, message)));
                        continue;
                    }
                    if let Err(e) = tx.send(&Frame { req: 0, msg }).await {
                        break Err(e);
                    }
                }
                _ = watchdog.tick() => {
                    let stuck = busy_since.lock().unwrap().is_some_and(|at: Instant| at.elapsed() >= SINK_STALL);
                    if stuck {
                        warn!(stream, "the video decoder stopped taking frames");
                        self.end_stream(stream, Some((MirrorEnd::DecoderStalled, STALLED)));
                    }
                }
                _ = tokio::time::sleep_until(idle) => break Err(LinkError::Protocol("idle timeout")),
            }
        };
        status.abort();
        let _ = tx
            .send(&Frame {
                req: 0,
                msg: Message::Bye,
            })
            .await;
        result
    }
}

const STALLED: &str = "the phone's video decoder stopped";

/// The thread that feeds the decoder, one frame at a time and never dropping one.
fn spawn_sink_thread(
    stream: u64,
    sink: Arc<dyn VideoSink>,
    out: mpsc::UnboundedSender<Message>,
    busy_since: Arc<Mutex<Option<Instant>>>,
) -> mpsc::Sender<VideoFrame> {
    let (tx, mut rx) = mpsc::channel::<VideoFrame>(SINK_QUEUE);
    std::thread::spawn(move || {
        let failed = |reason, message: String| {
            let _ = out.send(Message::MirrorStopped {
                stream_id: stream,
                reason,
                message,
            });
        };
        // A keyframe was asked for after a decoder error: the next one must work.
        let mut recovering = false;
        while let Some(frame) = rx.blocking_recv() {
            let began = Instant::now();
            *busy_since.lock().unwrap() = Some(began);
            let result = loop {
                match sink.frame(stream, &frame) {
                    FrameResult::NoInput if began.elapsed() < SINK_STALL && !out.is_closed() => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    r => break r,
                }
            };
            *busy_since.lock().unwrap() = None;
            if out.is_closed() {
                break;
            }
            let ack = Message::VideoAck { seq: frame.seq };
            match result {
                FrameResult::Queued => {
                    recovering &= !frame.keyframe;
                    let _ = out.send(ack);
                }
                FrameResult::NoInput => {
                    failed(MirrorEnd::DecoderStalled, STALLED.into());
                    break;
                }
                FrameResult::NeedKeyframe if recovering && frame.keyframe => {
                    failed(MirrorEnd::DecoderFailed, "the phone's video decoder failed".into());
                    break;
                }
                FrameResult::NeedKeyframe => {
                    debug!(stream, seq = frame.seq, "decoder asks for a keyframe");
                    recovering = true;
                    let _ = out.send(Message::VideoNeedKeyframe);
                    // Consumed: the host's window must not stay full.
                    let _ = out.send(ack);
                }
                FrameResult::Failed(reason, message) => {
                    failed(reason, message);
                    break;
                }
            }
        }
    });
    tx
}

async fn report_status(agent: Agent, stream: u64, sink: Arc<dyn VideoSink>, out: mpsc::UnboundedSender<Message>) {
    let mut tick = tokio::time::interval(STATUS_EVERY);
    loop {
        tick.tick().await;
        let sink = Arc::clone(&sink);
        let Ok(stats) = tokio::task::spawn_blocking(move || sink.stats(stream)).await else {
            return;
        };
        let status = VideoStatus {
            decoded: stats.decoded,
            dropped: stats.dropped,
            bad: stats.bad,
            decode_us: stats.decode_us,
            phone_locked: agent.inner.phone_locked.load(Ordering::Relaxed),
        };
        if out.send(Message::VideoStatus(status)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
