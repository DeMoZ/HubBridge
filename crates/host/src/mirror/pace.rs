//! Which captured frames are encoded, and when a keyframe is forced. A pure state machine:
//! the clock comes in with every call.
//!
//! The rule: frames in the encoder plus frames sent but not acknowledged never exceed
//! [`WINDOW`]. A picture captured while the window is full waits as the "latest" one and is
//! replaced by newer captures (each replacement counts as a skipped frame). Frames are only
//! ever dropped **before** encoding: a missing P frame would break all that follow.
//!
//! The pictures themselves stay with the frame source; this machine only says whether to
//! encode the latest one now ([`Encode`]). After encoding it the source reports
//! [`Pacer::on_encoder_submitted`] at once (see [`Gate`], which does both in one step).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Frames in the encoder or unacknowledged.
pub const WINDOW: usize = 2;
/// Least time between two forced keyframes: at most two per second, and a request that has to
/// wait is still served within half a second.
pub const KEYFRAME_SPACING: Duration = Duration::from_millis(500);
/// A captured picture is forced to a keyframe when the last one is this old.
pub const KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);

/// Encode the latest captured picture now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encode {
    pub keyframe: bool,
}

#[derive(Debug, Default)]
pub struct Pacer {
    /// The latest picture still has to be encoded.
    held: bool,
    /// Some picture was captured (it can be encoded again for a keyframe).
    have_picture: bool,
    in_encoder: usize,
    unacked: BTreeSet<u64>,
    next_seq: u64,
    /// A keyframe is owed: the start of the stream or a request.
    keyframe_wanted: bool,
    last_keyframe: Option<Instant>,
    /// `last_keyframe` before the keyframe submitted last, for when the encoder drops it.
    keyframe_before: Option<Instant>,
    skipped: u64,
    started: bool,
}

impl Pacer {
    pub fn new() -> Pacer {
        Pacer {
            keyframe_wanted: true,
            ..Pacer::default()
        }
    }

    /// Frames in the encoder or sent and not acknowledged.
    pub fn in_flight(&self) -> usize {
        self.in_encoder + self.unacked.len()
    }

    /// Captured frames that were never encoded, plus frames the encoder dropped.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    fn keyframe_allowed(&self, now: Instant) -> bool {
        self.last_keyframe
            .is_none_or(|at| now.saturating_duration_since(at) >= KEYFRAME_SPACING)
    }

    /// What to do now, if anything. Changes nothing.
    pub fn next(&self, now: Instant) -> Option<Encode> {
        if !self.have_picture || self.in_flight() >= WINDOW {
            return None;
        }
        let allowed = self.keyframe_allowed(now);
        let periodic = self
            .last_keyframe
            .is_some_and(|at| now.saturating_duration_since(at) >= KEYFRAME_INTERVAL);
        if self.held {
            // A still screen is not encoded again just because ten seconds passed: the
            // periodic keyframe rides on the next captured picture.
            Some(Encode {
                keyframe: allowed && (self.keyframe_wanted || periodic),
            })
        } else {
            (self.keyframe_wanted && allowed).then_some(Encode { keyframe: true })
        }
    }

    /// A new picture was captured (it replaces the held one).
    pub fn on_captured(&mut self, now: Instant) -> Option<Encode> {
        if self.held {
            self.skipped += 1;
        }
        self.held = true;
        self.have_picture = true;
        self.next(now)
    }

    /// The latest picture went into the encoder as `encode` said.
    pub fn on_encoder_submitted(&mut self, encode: Encode, now: Instant) {
        self.held = false;
        self.in_encoder += 1;
        self.started = true;
        if encode.keyframe {
            self.keyframe_wanted = false;
            self.keyframe_before = self.last_keyframe;
            self.last_keyframe = Some(now);
        }
    }

    /// The encoder produced a frame that will be sent: returns its sequence number.
    pub fn on_encoder_output(&mut self) -> u64 {
        self.in_encoder = self.in_encoder.saturating_sub(1);
        let seq = self.take_seq();
        self.unacked.insert(seq);
        seq
    }

    /// A sequence number for a frame outside the window (the decoder configuration).
    pub fn take_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// The encoder dropped (or refused) a frame: it takes no sequence number. The picture is
    /// encoded again on the next event unless a newer one arrives first.
    pub fn on_encoder_dropped(&mut self, keyframe: bool) {
        self.in_encoder = self.in_encoder.saturating_sub(1);
        self.skipped += 1;
        self.held = true;
        if keyframe {
            // It never came out: it does not count against the limit, or the retry would
            // be a delta frame.
            self.keyframe_wanted = true;
            self.last_keyframe = self.keyframe_before;
        }
    }

    pub fn on_ack(&mut self, seq: u64, now: Instant) -> Option<Encode> {
        self.unacked.remove(&seq);
        self.next(now)
    }

    /// The phone asks for a keyframe. Served at once if the limit and the window allow,
    /// else remembered. With a still screen the last picture is encoded again.
    pub fn on_keyframe_request(&mut self, now: Instant) -> Option<Encode> {
        self.keyframe_wanted = true;
        self.next(now)
    }

    /// Time passed: a remembered request may be due.
    pub fn on_tick(&mut self, now: Instant) -> Option<Encode> {
        self.next(now)
    }
}

/// The [`Pacer`] shared by the frame source (captures) and the session (acknowledgements).
/// Every method that returns [`Encode`] has already counted the frame as submitted: the
/// caller must encode it, or report [`Gate::dropped`].
#[derive(Debug, Clone)]
pub struct Gate(Arc<Mutex<Pacer>>);

impl Default for Gate {
    fn default() -> Self {
        Gate(Arc::new(Mutex::new(Pacer::new())))
    }
}

impl Gate {
    pub fn new() -> Gate {
        Gate::default()
    }

    fn lock(&self) -> MutexGuard<'_, Pacer> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn commit(&self, decide: impl FnOnce(&mut Pacer, Instant) -> Option<Encode>) -> Option<Encode> {
        let mut pacer = self.lock();
        let now = Instant::now();
        let encode = decide(&mut pacer, now);
        if let Some(e) = encode {
            pacer.on_encoder_submitted(e, now);
        }
        encode
    }

    /// A new picture was captured.
    pub fn captured(&self) -> Option<Encode> {
        self.commit(Pacer::on_captured)
    }

    /// May the held (or last) picture be encoded now?
    pub fn poll(&self) -> Option<Encode> {
        self.commit(Pacer::on_tick)
    }

    pub fn dropped(&self, keyframe: bool) {
        self.lock().on_encoder_dropped(keyframe);
    }

    pub fn output(&self) -> u64 {
        self.lock().on_encoder_output()
    }

    pub fn take_seq(&self) -> u64 {
        self.lock().take_seq()
    }

    /// An acknowledgement frees a slot: call [`Gate::poll`] (through the source) after it.
    pub fn ack(&self, seq: u64) {
        self.lock().on_ack(seq, Instant::now());
    }

    /// Remember a keyframe request; the source encodes through [`Gate::poll`].
    pub fn want_keyframe(&self) {
        self.lock().on_keyframe_request(Instant::now());
    }

    pub fn skipped(&self) -> u64 {
        self.lock().skipped()
    }

    pub fn in_flight(&self) -> usize {
        self.lock().in_flight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        p: Pacer,
        t0: Instant,
    }

    impl Run {
        fn new() -> Run {
            Run {
                p: Pacer::new(),
                t0: Instant::now(),
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        /// Apply a decision the way a source does.
        fn submit(&mut self, e: Option<Encode>, ms: u64) -> Option<Encode> {
            if let Some(e) = e {
                let now = self.at(ms);
                self.p.on_encoder_submitted(e, now);
            }
            e
        }

        fn capture(&mut self, ms: u64) -> Option<Encode> {
            let e = self.p.on_captured(self.at(ms));
            self.submit(e, ms)
        }

        fn ack(&mut self, seq: u64, ms: u64) -> Option<Encode> {
            let e = self.p.on_ack(seq, self.at(ms));
            self.submit(e, ms)
        }

        fn request(&mut self, ms: u64) -> Option<Encode> {
            let e = self.p.on_keyframe_request(self.at(ms));
            self.submit(e, ms)
        }

        fn tick(&mut self, ms: u64) -> Option<Encode> {
            let e = self.p.on_tick(self.at(ms));
            self.submit(e, ms)
        }

        /// Capture, encode and acknowledge one frame.
        fn frame(&mut self, ms: u64) -> Encode {
            let e = self.capture(ms).expect("window is free");
            let seq = self.p.on_encoder_output();
            assert_eq!(self.ack(seq, ms), None);
            e
        }
    }

    const KEY: Option<Encode> = Some(Encode { keyframe: true });
    const DELTA: Option<Encode> = Some(Encode { keyframe: false });

    #[test]
    fn the_stream_starts_with_a_keyframe() {
        let mut r = Run::new();
        assert_eq!(r.tick(0), None, "nothing captured yet");
        assert_eq!(r.capture(0), KEY);
        assert_eq!(r.capture(33), DELTA);
    }

    #[test]
    fn never_more_than_two_in_flight_and_skips_are_counted() {
        let mut r = Run::new();
        let mut encoded = 0;
        for i in 0..30 {
            encoded += usize::from(r.capture(i * 33).is_some());
            assert!(r.p.in_flight() <= WINDOW);
        }
        assert_eq!(encoded, 2);
        // 28 captures could not be encoded; the last of them is still held.
        assert_eq!(r.p.skipped(), 27);
        // In the encoder or sent: both count.
        let first = r.p.on_encoder_output();
        assert_eq!(r.p.in_flight(), 2);
        assert_eq!(r.tick(1000), None);
        assert!(r.ack(first, 1000).is_some(), "the held picture goes now");
        assert_eq!(r.p.in_flight(), 2);
    }

    #[test]
    fn the_held_picture_is_encoded_once_after_an_ack() {
        let mut r = Run::new();
        r.capture(0);
        r.capture(10);
        assert_eq!(r.capture(20), None, "the screen changes once while the window is full");
        let a = r.p.on_encoder_output();
        let b = r.p.on_encoder_output();
        assert_eq!(r.ack(a, 40), DELTA);
        assert_eq!(r.ack(b, 50), None, "nothing new is waiting");
        assert_eq!(r.p.skipped(), 0);
    }

    #[test]
    fn a_still_screen_and_a_keyframe_request() {
        let mut r = Run::new();
        r.frame(0);
        assert_eq!(r.tick(5_000), None);
        assert_eq!(r.request(5_000), KEY, "the last picture is encoded again");
    }

    #[test]
    fn a_still_screen_stays_still_after_ten_seconds() {
        let mut r = Run::new();
        r.frame(0);
        for ms in [9_999, 10_000, 25_000, 60_000] {
            assert_eq!(r.tick(ms), None);
        }
        assert_eq!(r.capture(60_000), KEY, "the next change is the periodic keyframe");
    }

    #[test]
    fn keyframes_every_ten_seconds_while_frames_flow() {
        let mut r = Run::new();
        let keys: Vec<u64> = (0..=750).map(|i| i * 40).filter(|ms| r.frame(*ms).keyframe).collect();
        assert_eq!(keys, [0, 10_000, 20_000, 30_000]);
    }

    #[test]
    fn forced_keyframes_are_limited_and_a_waiting_request_is_served() {
        let mut r = Run::new();
        assert!(r.frame(0).keyframe);
        assert_eq!(r.request(100), None, "too soon, remembered");
        assert!(!r.frame(200).keyframe, "other frames go on");
        assert_eq!(r.tick(499), None);
        assert_eq!(r.tick(500), KEY, "served within half a second of the request");
        let seq = r.p.on_encoder_output();
        r.ack(seq, 510);
        // A burst of requests: never closer than 500 ms apart.
        let mut forced = Vec::new();
        for ms in (520..=3_000).step_by(20) {
            if r.request(ms).is_some() {
                forced.push(ms);
                let seq = r.p.on_encoder_output();
                r.p.on_ack(seq, r.at(ms));
            }
        }
        assert!(forced.windows(2).all(|w| w[1] - w[0] >= 500), "{forced:?}");
        assert!(forced.len() >= 4, "{forced:?}");
    }

    #[test]
    fn a_request_with_a_full_window_waits_for_the_ack() {
        let mut r = Run::new();
        r.capture(0);
        r.capture(10);
        assert_eq!(r.request(900), None);
        let a = r.p.on_encoder_output();
        assert_eq!(r.ack(a, 910), KEY);
    }

    #[test]
    fn an_encoder_drop_frees_the_slot_and_takes_no_sequence_number() {
        let mut r = Run::new();
        r.capture(0);
        r.p.on_encoder_dropped(true);
        assert_eq!((r.p.in_flight(), r.p.skipped()), (0, 1));
        // The picture and the owed keyframe are tried again.
        assert_eq!(r.tick(500), KEY);
        assert_eq!(r.p.on_encoder_output(), 1);
    }

    #[test]
    fn a_dropped_keyframe_is_retried_as_a_keyframe_at_once() {
        // The first one.
        let mut r = Run::new();
        assert_eq!(r.capture(0), KEY);
        r.p.on_encoder_dropped(true);
        assert_eq!(r.tick(20), KEY);
        // A later one: the one that did come out still counts against the limit.
        let mut r = Run::new();
        r.frame(0);
        assert_eq!(r.request(600), KEY);
        r.p.on_encoder_dropped(true);
        assert_eq!(r.tick(620), KEY);
        let seq = r.p.on_encoder_output();
        r.ack(seq, 630);
        assert_eq!(r.request(700), None, "a keyframe came out 80 ms ago");
        // The periodic one stays due.
        let mut r = Run::new();
        r.frame(0);
        assert_eq!(r.capture(10_000), KEY);
        r.p.on_encoder_dropped(true);
        assert_eq!(r.tick(10_020), KEY);
    }

    #[test]
    fn sequence_numbers_and_unknown_acks() {
        let mut r = Run::new();
        assert_eq!(r.p.take_seq(), 1, "the config frame");
        r.capture(0);
        assert_eq!(r.p.on_encoder_output(), 2);
        assert_eq!(r.p.in_flight(), 1);
        r.ack(1, 1);
        r.ack(99, 1);
        assert_eq!(r.p.in_flight(), 1, "acks outside the window change nothing");
        r.ack(2, 2);
        assert_eq!(r.p.in_flight(), 0);
    }
}
