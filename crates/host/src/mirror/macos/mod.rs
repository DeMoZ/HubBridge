//! The macOS screen source: ScreenCaptureKit ([`capture`]) into VideoToolbox ([`encode`]),
//! behind the Screen Recording permission ([`permission`]).

pub mod capture;
pub mod encode;
pub mod permission;
