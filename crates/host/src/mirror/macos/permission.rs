//! The Screen Recording permission (System Settings → Privacy & Security → Screen & System
//! Audio Recording). From a terminal it is the terminal app's permission.

use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Granted,
    /// Not allowed yet. After the user allows it, macOS usually applies it only to a
    /// reopened app.
    Denied,
}

/// Lets one caller per process make the request.
#[derive(Default)]
pub struct RequestOnce(AtomicBool);

impl RequestOnce {
    pub const fn new() -> RequestOnce {
        RequestOnce(AtomicBool::new(false))
    }

    fn claim(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }
}

static REQUESTED: RequestOnce = RequestOnce::new();

/// `preflight` never prompts; `request` shows the system prompt (macOS does so only the first
/// time) and is called at most once through `once`.
fn ensure_with(once: &RequestOnce, preflight: impl Fn() -> bool, request: impl FnOnce() -> bool) -> Permission {
    if preflight() || (once.claim() && request()) {
        Permission::Granted
    } else {
        Permission::Denied
    }
}

/// Check without prompting; when missing, ask macOS once per app run.
pub fn ensure() -> Permission {
    ensure_with(
        &REQUESTED,
        || CGPreflightScreenCaptureAccess(),
        || CGRequestScreenCaptureAccess(),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn macos_is_asked_only_once() {
        let once = RequestOnce::new();
        let asked = Cell::new(0);
        let ask = || {
            asked.set(asked.get() + 1);
            false
        };
        for _ in 0..3 {
            assert_eq!(ensure_with(&once, || false, ask), Permission::Denied);
        }
        assert_eq!(asked.get(), 1);
        // Granted later (a reopened app, or macOS applied it): no request needed.
        assert_eq!(ensure_with(&once, || true, ask), Permission::Granted);
        assert_eq!(asked.get(), 1);
        assert_eq!(ensure_with(&RequestOnce::new(), || false, || true), Permission::Granted);
    }
}
