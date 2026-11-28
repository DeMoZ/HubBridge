//! Where the frames of a mirror come from.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use super::pace::Gate;

/// What the source has to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// One encoded frame, Annex B.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Only the parameter sets: sent first and whenever they change.
    pub config: bool,
    /// When the picture was captured.
    pub captured: Instant,
}

#[derive(Debug)]
pub enum SourceEvent {
    Frame(EncodedFrame),
    /// The source cannot go on (e.g. the system stopped the capture).
    Failed(String),
}

/// Receives the source's output, from any thread; must not block.
pub type FrameOutput = Arc<dyn Fn(SourceEvent) + Send + Sync>;

/// Encoded frames of a picture that changes over time.
///
/// The source keeps the latest picture and encodes it only when the [`Gate`] says so:
/// [`Gate::captured`] for each new picture, [`Gate::poll`] on [`FrameSource::kick`]. Every
/// picture the gate let through ends as one `Frame` that is not a config frame, or as
/// [`Gate::dropped`]. A config frame (outside the gate) comes before the first keyframe.
pub trait FrameSource: Send {
    /// Size of the captured picture in pixels.
    fn native_size(&mut self) -> Result<(u32, u32)>;

    /// Start delivering frames of `config`'s size. May block.
    fn start(&mut self, config: EncodeConfig, gate: Gate, output: FrameOutput) -> Result<()>;

    /// The window or a waiting keyframe may allow encoding now: ask [`Gate::poll`]. Cheap;
    /// called after every acknowledgement and a few times a second.
    fn kick(&mut self);

    /// The phone asks for a keyframe. With a still picture the last one is encoded again.
    fn request_keyframe(&mut self);

    /// Stop capturing. May block; calling it again does nothing.
    fn stop(&mut self);
}
