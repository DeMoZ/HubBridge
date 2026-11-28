//! Mirroring the host's screen onto a phone display (roadmap step 3a).
//!
//! The pure parts: [`fit`] (the stream size), [`annexb`] (encoder output to the wire format),
//! [`pace`] (which captured frames are encoded) and [`stats`] (the once-a-second readout).
//! [`session`] runs a mirror: it takes encoded frames from a [`source::FrameSource`] and sends
//! them over a video session of its own. Sources: the screen (macOS), [`synthetic`] (fake
//! frames for tests).

pub mod annexb;
pub mod fit;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod pace;
pub mod session;
pub mod source;
pub mod stats;
pub mod synthetic;

pub use session::{Ending, MirrorHandle, MirrorOptions, MirrorSession};
pub use source::{EncodeConfig, EncodedFrame, FrameOutput, FrameSource, SourceEvent};

/// Whether this platform can capture its screen.
pub const SCREEN_SUPPORTED: bool = cfg!(target_os = "macos");

/// Shown where screen mirroring is not available.
pub const NEEDS_MACOS: &str = "mirroring needs macOS for now";

/// The screen may not be captured yet. The message says what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "no Screen Recording permission: allow it in System Settings → Privacy & Security → Screen & System Audio \
     Recording, then quit and reopen the app (a program started from a terminal uses the terminal app's \
     permission)"
)]
pub struct ScreenPermissionDenied;

/// The screen of this computer. Checks the Screen Recording permission first and asks macOS
/// for it once per run.
#[cfg(target_os = "macos")]
pub fn screen_source() -> anyhow::Result<Box<dyn FrameSource>> {
    match macos::permission::ensure() {
        macos::permission::Permission::Granted => Ok(Box::new(macos::capture::ScreenSource::new())),
        macos::permission::Permission::Denied => Err(ScreenPermissionDenied.into()),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn screen_source() -> anyhow::Result<Box<dyn FrameSource>> {
    anyhow::bail!(NEEDS_MACOS)
}
