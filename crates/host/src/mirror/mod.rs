//! Mirroring the host's screen onto a phone display (roadmap step 3a).
//!
//! The pure parts: [`fit`] (the stream size), [`annexb`] (encoder output to the wire format),
//! [`pace`] (which captured frames are encoded) and [`stats`] (the once-a-second readout).
//! [`session`] runs a mirror: it takes encoded frames from a [`source::FrameSource`] and sends
//! them over a video session of its own. Sources: the screen (macOS), [`synthetic`] (fake
//! frames for tests).

pub mod annexb;
pub mod fit;
pub mod pace;
pub mod session;
pub mod source;
pub mod stats;
pub mod synthetic;

pub use session::{Ending, MirrorHandle, MirrorOptions, MirrorSession};
pub use source::{EncodeConfig, EncodedFrame, FrameOutput, FrameSource, SourceEvent};

/// Whether this platform can capture its screen.
pub const SCREEN_SUPPORTED: bool = false;

/// Shown where screen mirroring is not available.
pub const NEEDS_MACOS: &str = "mirroring needs macOS for now";
