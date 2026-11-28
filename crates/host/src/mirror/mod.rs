//! Mirroring the host's screen onto a phone display (roadmap step 3a).
//!
//! The pure parts: [`fit`] (the stream size), [`annexb`] (encoder output to the wire format),
//! [`pace`] (which captured frames are encoded) and [`stats`] (the once-a-second readout).

pub mod annexb;
pub mod fit;
pub mod pace;
pub mod stats;
