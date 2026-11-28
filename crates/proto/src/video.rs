//! Phone displays and video streams (protocol messages of roadmap step 3a).
//! These structs are final once released: postcard cannot grow them.

use serde::{Deserialize, Serialize};

/// First phone app version that knows the display and mirror messages. A host sends them to
/// no older one: an unknown variant ends the old agent's session.
pub const MIRROR_MIN_APP_VERSION: (u16, u16, u16) = (0, 2, 0);

/// Lenient `X.Y.Z` comparison: the leading digits of each part count (`0.2.0-dev` is 0.2.0),
/// a missing part is 0; anything unparsable is older than everything.
pub fn app_version_at_least(version: &str, min: (u16, u16, u16)) -> bool {
    let mut parts = [0u16; 3];
    let mut given = version.trim().split('.');
    for slot in &mut parts {
        let Some(part) = given.next() else { break };
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        match digits.parse() {
            Ok(n) => *slot = n,
            Err(_) => return false,
        }
    }
    (parts[0], parts[1], parts[2]) >= min
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayList {
    pub displays: Vec<DisplayInfo>,
    /// Samsung DeX is on.
    pub desktop_mode: bool,
}

/// A presentation display of the phone. Its id is only valid until the list changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Size of the current mode.
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz.
    pub refresh_mhz: u32,
    pub modes: Vec<DisplayMode>,
    pub presentation: bool,
    pub secure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    /// H.264, Annex B byte stream.
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// One access unit (or the decoder configuration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    pub seq: u64,
    /// Capture time on the host's clock, microseconds.
    pub pts_us: u64,
    pub keyframe: bool,
    /// Only the parameter sets (SPS, PPS).
    pub config: bool,
    /// Annex B.
    pub data: Vec<u8>,
}

/// What the phone's decoder did so far (counters since the stream started).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoStatus {
    pub decoded: u64,
    /// Decoded but not shown.
    pub dropped: u64,
    /// Malformed or undecodable frames.
    pub bad: u64,
    /// Average time from decoder input to output over the last second, microseconds.
    pub decode_us: u32,
    pub phone_locked: bool,
}

/// Why the phone ended a mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorEnd {
    DisplayRemoved,
    VideoSessionLost,
    NotAttached,
    DecoderFailed,
    DecoderStalled,
    SurfaceLost,
    AgentStopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_versions() {
        let new = |v| app_version_at_least(v, MIRROR_MIN_APP_VERSION);
        for ok in ["0.2.0", "0.10.1", "0.2.0-dev", "1.0.0", "0.3", " 0.2.1 "] {
            assert!(new(ok), "{ok}");
        }
        for old in ["0.1.0", "0.1.99", "?", "", "v0.2.0", "0..1", "0.x.0", "999999.0.0"] {
            assert!(!new(old), "{old}");
        }
    }
}
