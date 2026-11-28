//! A running mirror: `MirrorStart` on the control session, a second ("video") session to the
//! phone, frames from the source paced by the phone's acknowledgements, and the readout.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hubbridge_proto::{
    DisplayInfo, MIRROR_MIN_APP_VERSION, Message, MirrorEnd, VideoCodec, VideoConfig, VideoFrame, app_version_at_least,
};
use tokio::sync::{mpsc, watch};

use super::fit::preset_bitrate_kbps;
use super::pace::Gate;
use super::source::{EncodeConfig, FrameSource, SourceEvent};
use super::stats::{Snapshot, Stats};
use crate::client::{AgentError, Client, ClientEvent};
use crate::store::Store;

/// The agent sends a status every second: this much silence means the link is gone.
pub const MEDIA_SILENCE: Duration = Duration::from_secs(5);
/// Ping the video session this often, whether frames flow or not: the round trip of the
/// readout, and what keeps a still screen from being idle.
pub const KEEPALIVE: Duration = Duration::from_secs(2);
/// After the video session closed, how long the phone's reason may still be on its way.
const REASON_WAIT: Duration = Duration::from_secs(1);
/// How often the source is asked whether something is due (keyframe limit, encoder retry).
const KICK_EVERY: Duration = Duration::from_millis(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

pub const CONNECTION_LOST: &str = "the connection to the phone was lost";
/// The video session closed and no reason came on the control session.
pub const VIDEO_CLOSED: &str = "the video connection closed and the phone did not say why";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorOptions {
    /// 30 or 60 in the presets.
    pub fps: u32,
    /// `None`: the preset of the stream size.
    pub bitrate_kbps: Option<u32>,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        MirrorOptions {
            fps: 30,
            bitrate_kbps: None,
        }
    }
}

/// How a mirror ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// In words, to follow "Mirroring stopped: ".
    pub reason: String,
    /// [`MirrorHandle::stop`] did it.
    pub by_user: bool,
}

impl Ending {
    fn because(reason: impl Into<String>) -> Ending {
        Ending {
            reason: reason.into(),
            by_user: false,
        }
    }

    /// The phone's own reason in words.
    pub fn from_phone(reason: MirrorEnd, message: &str) -> Ending {
        Ending::because(match reason {
            MirrorEnd::DisplayRemoved => "the monitor was disconnected".to_string(),
            MirrorEnd::VideoSessionLost => "the phone lost the video connection".to_string(),
            MirrorEnd::NotAttached => "the video connection was not opened in time".to_string(),
            MirrorEnd::DecoderFailed => "the phone's video decoder failed".to_string(),
            MirrorEnd::DecoderStalled => "the phone's video decoder stopped".to_string(),
            MirrorEnd::SurfaceLost if !message.is_empty() => message.to_string(),
            MirrorEnd::SurfaceLost => "the phone took the picture off the display".to_string(),
            MirrorEnd::AgentStopped => "HubBridge on the phone was stopped".to_string(),
        })
    }
}

/// Fails with what to do when the phone app is too old for displays and mirroring.
pub fn require_new_app(app_version: &str) -> Result<()> {
    if !app_version_at_least(app_version, MIRROR_MIN_APP_VERSION) {
        let (a, b, c) = MIRROR_MIN_APP_VERSION;
        bail!(
            "update the phone app: it is version {app_version}, displays and mirroring need {a}.{b}.{c} or later \
             (`hubbridge install-apk`, or Install in the window)"
        );
    }
    Ok(())
}

pub struct MirrorHandle {
    stream_id: u64,
    display: DisplayInfo,
    config: VideoConfig,
    gate: Gate,
    commands: mpsc::UnboundedSender<Command>,
    stats: watch::Receiver<Option<Snapshot>>,
    ending: watch::Receiver<Option<Ending>>,
}

enum Command {
    Stop,
    /// `MirrorStopped` or the end of the control session.
    Ended(Ending),
}

impl MirrorHandle {
    pub fn stream_id(&self) -> u64 {
        self.stream_id
    }

    /// The display as it was when the mirror started.
    pub fn display(&self) -> &DisplayInfo {
        &self.display
    }

    pub fn config(&self) -> VideoConfig {
        self.config
    }

    /// Frames in the encoder or unacknowledged (never more than two).
    pub fn in_flight(&self) -> usize {
        self.gate.in_flight()
    }

    /// A new value every second while the mirror runs.
    pub fn stats(&self) -> watch::Receiver<Option<Snapshot>> {
        self.stats.clone()
    }

    /// Stop the mirror and wait until the phone was told and the source stopped.
    pub async fn stop(&self) -> Ending {
        let _ = self.commands.send(Command::Stop);
        self.done().await
    }

    /// Waits for the end of the mirror, however it comes.
    pub async fn done(&self) -> Ending {
        let mut ending = self.ending.clone();
        match ending.wait_for(Option::is_some).await {
            Ok(e) => e.clone().expect("waited for it"),
            Err(_) => Ending::because("the mirror task stopped"),
        }
    }

    /// Give every event of the control session to this: it picks out what concerns the mirror.
    pub fn on_control_event(&self, event: &ClientEvent) {
        match event {
            ClientEvent::Message(Message::MirrorStopped {
                stream_id,
                reason,
                message,
            }) if *stream_id == self.stream_id => {
                let _ = self.commands.send(Command::Ended(Ending::from_phone(*reason, message)));
            }
            ClientEvent::Disconnected(_) => {
                let _ = self.commands.send(Command::Ended(Ending::because(CONNECTION_LOST)));
            }
            ClientEvent::Message(_) => {}
        }
    }
}

impl Drop for MirrorHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}

/// The message of a refusal by the phone, without the protocol's wrapping.
fn refusal(e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<AgentError>() {
        Ok(agent) => anyhow!("{}", agent.message),
        Err(other) => other,
    }
}

async fn tell_stop(control: &Client, stream_id: u64) {
    let stop = control.request(Message::MirrorStop { stream_id });
    let _ = tokio::time::timeout(STOP_TIMEOUT, stop).await;
}

async fn stop_source(mut source: Box<dyn FrameSource>) {
    let _ = tokio::task::spawn_blocking(move || source.stop()).await;
}

pub struct MirrorSession;

impl MirrorSession {
    /// Mirror `source` onto display `display_id` of the phone behind `control`. `store` holds
    /// the key for the second session. Returns once the phone shows the surface and the
    /// source runs.
    pub async fn start(
        control: &Arc<Client>,
        store: &mut Store,
        display_id: u32,
        options: MirrorOptions,
        mut source: Box<dyn FrameSource>,
    ) -> Result<MirrorHandle> {
        // `GetAgentInfo` is old; everything below is not.
        require_new_app(&control.agent_info().await?.app_version)?;
        let display = control
            .displays()
            .await
            .map_err(refusal)?
            .displays
            .into_iter()
            .find(|d| d.id == display_id)
            .with_context(|| format!("the phone has no display {display_id}"))?;
        let (width, height) = source.stream_size(display.width, display.height)?;
        if width == 0 || height == 0 {
            bail!(
                "cannot fit the picture onto a {}x{} display",
                display.width,
                display.height
            );
        }
        let config = VideoConfig {
            codec: VideoCodec::H264,
            width,
            height,
            fps: options.fps,
            bitrate_kbps: options
                .bitrate_kbps
                .unwrap_or_else(|| preset_bitrate_kbps(width, height, options.fps)),
        };
        let stream_id = match control
            .request(Message::MirrorStart { display_id, config })
            .await
            .map_err(refusal)?
        {
            Message::MirrorStarted { stream_id } => stream_id,
            other => bail!("unexpected reply: {other:?}"),
        };

        let attached = async {
            let media = Client::connect(&[control.addr], store)
                .await
                .context("open the video connection")?;
            if media.agent_id != control.agent_id {
                bail!("the video connection reached another phone");
            }
            match media
                .request(Message::MirrorAttach { stream_id })
                .await
                .map_err(refusal)?
            {
                Message::Done => Ok(media),
                other => bail!("unexpected reply: {other:?}"),
            }
        };
        let media = match attached.await {
            Ok(media) => media,
            Err(e) => {
                tell_stop(control, stream_id).await;
                return Err(e);
            }
        };

        let gate = Gate::new();
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let encode = EncodeConfig {
            width,
            height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
        };
        let source_gate = gate.clone();
        let (source, started) = tokio::task::spawn_blocking(move || {
            let started = source.start(
                encode,
                source_gate,
                Arc::new(move |event| {
                    let _ = frames_tx.send(event);
                }),
            );
            (source, started)
        })
        .await
        .context("start the capture")?;
        if let Err(e) = started {
            tell_stop(control, stream_id).await;
            media.close().await;
            return Err(e);
        }

        let (commands, command_rx) = mpsc::unbounded_channel();
        let (stats_tx, stats) = watch::channel(None);
        let (ending_tx, ending) = watch::channel(None);
        let run = Run {
            stream_id,
            gate: gate.clone(),
            // Weak, so that the owner can still close the control session.
            control: Arc::downgrade(control),
            media,
            source,
            frames,
            commands: command_rx,
            stats: stats_tx,
            readout: Stats::new(width, height, Instant::now()),
        };
        tokio::spawn(run.run(Box::pin(control.closed()), ending_tx));
        Ok(MirrorHandle {
            stream_id,
            display,
            config,
            gate,
            commands,
            stats,
            ending,
        })
    }
}

struct Run {
    stream_id: u64,
    gate: Gate,
    control: Weak<Client>,
    media: Client,
    source: Box<dyn FrameSource>,
    frames: mpsc::UnboundedReceiver<SourceEvent>,
    commands: mpsc::UnboundedReceiver<Command>,
    stats: watch::Sender<Option<Snapshot>>,
    readout: Stats,
}

impl Run {
    async fn run(
        mut self,
        mut control_closed: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
        ending_tx: watch::Sender<Option<Ending>>,
    ) {
        let ending = self.pump(&mut control_closed).await;
        stop_source(self.source).await;
        // The phone knows already when it said so itself; the request is harmless then.
        if let Some(control) = self.control.upgrade()
            && ending.reason != CONNECTION_LOST
        {
            tell_stop(&control, self.stream_id).await;
        }
        self.media.close().await;
        ending_tx.send_replace(Some(ending));
    }

    async fn pump(&mut self, control_closed: &mut std::pin::Pin<Box<dyn Future<Output = ()> + Send>>) -> Ending {
        let Some(mut media_events) = self.media.take_events() else {
            return Ending::because("no event stream");
        };
        let began = Instant::now();
        let mut second = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let mut kick = tokio::time::interval(KICK_EVERY);
        kick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();
        let mut last_ping: Option<Instant> = None;
        let mut ping_seq = 0u32;
        let mut pending_ping = None;
        // Set when the video session closed: the reason may follow on the control session.
        let mut media_gone: Option<tokio::time::Instant> = None;
        loop {
            let gone_deadline = media_gone.unwrap_or_else(tokio::time::Instant::now);
            tokio::select! {
                biased;
                command = self.commands.recv() => match command {
                    Some(Command::Ended(ending)) => return ending,
                    Some(Command::Stop) | None => return Ending { reason: "stopped".into(), by_user: true },
                },
                _ = control_closed.as_mut() => return Ending::because(CONNECTION_LOST),
                _ = tokio::time::sleep_until(gone_deadline), if media_gone.is_some() => {
                    return Ending::because(VIDEO_CLOSED);
                }
                event = media_events.recv(), if media_gone.is_none() => match event {
                    Some(ClientEvent::Message(msg)) => {
                        last_heard = Instant::now();
                        match msg {
                            Message::VideoAck { seq } => {
                                self.gate.ack(seq);
                                self.readout.on_ack(seq, Instant::now());
                                self.source.kick();
                            }
                            Message::VideoStatus(status) => self.readout.on_status(status, Instant::now()),
                            Message::VideoNeedKeyframe => self.source.request_keyframe(),
                            _ => {}
                        }
                    }
                    Some(ClientEvent::Disconnected(_)) | None => {
                        media_gone = Some(tokio::time::Instant::now() + REASON_WAIT);
                    }
                },
                Some(event) = self.frames.recv() => match event {
                    SourceEvent::Failed(why) => return Ending::because(format!("the screen capture stopped: {why}")),
                    SourceEvent::Frame(frame) => {
                        let seq = if frame.config {
                            self.readout.on_config(frame.data.len());
                            self.gate.take_seq()
                        } else {
                            let seq = self.gate.output();
                            self.readout.on_sent(seq, frame.captured, frame.data.len(), frame.keyframe);
                            seq
                        };
                        let video = VideoFrame {
                            seq,
                            pts_us: frame.captured.saturating_duration_since(began).as_micros() as u64,
                            keyframe: frame.keyframe,
                            config: frame.config,
                            data: frame.data,
                        };
                        if media_gone.is_none() && self.media.send_event(Message::VideoFrame(video)).await.is_err() {
                            media_gone = Some(tokio::time::Instant::now() + REASON_WAIT);
                        }
                    }
                },
                reply = async { pending_ping.as_mut().expect("guarded").await }, if pending_ping.is_some() => {
                    let (sent, result): (Instant, Result<Message>) = reply;
                    pending_ping = None;
                    if matches!(result, Ok(Message::Pong { .. })) {
                        last_heard = Instant::now();
                        self.readout.on_rtt(sent.elapsed());
                    }
                }
                _ = kick.tick() => {
                    self.source.kick();
                    if last_heard.elapsed() >= MEDIA_SILENCE {
                        return Ending::because(CONNECTION_LOST);
                    }
                    let due = last_ping.is_none_or(|at| at.elapsed() >= KEEPALIVE);
                    if due && pending_ping.is_none() && media_gone.is_none() {
                        ping_seq += 1;
                        let sent = Instant::now();
                        last_ping = Some(sent);
                        if let Ok(pending) = self.media.submit(Message::Ping { seq: ping_seq, sent_us: 0 }).await {
                            pending_ping = Some(Box::pin(async move { (sent, pending.wait(MEDIA_SILENCE).await) }));
                        }
                    }
                }
                _ = second.tick() => {
                    let snapshot = self.readout.take(Instant::now(), self.gate.skipped());
                    self.stats.send_replace(Some(snapshot));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_in_words() {
        let words = |r| Ending::from_phone(r, "").reason;
        assert_eq!(words(MirrorEnd::DisplayRemoved), "the monitor was disconnected");
        assert_eq!(words(MirrorEnd::DecoderStalled), "the phone's video decoder stopped");
        for reason in [
            MirrorEnd::VideoSessionLost,
            MirrorEnd::NotAttached,
            MirrorEnd::DecoderFailed,
            MirrorEnd::SurfaceLost,
            MirrorEnd::AgentStopped,
        ] {
            assert!(!words(reason).is_empty());
        }
        assert_eq!(
            Ending::from_phone(MirrorEnd::SurfaceLost, "the window was closed").reason,
            "the window was closed"
        );
    }

    #[test]
    fn the_version_gate() {
        assert!(require_new_app("0.2.0").is_ok());
        let old = require_new_app("0.1.0").unwrap_err().to_string();
        assert!(old.starts_with("update the phone app"), "{old}");
    }
}
