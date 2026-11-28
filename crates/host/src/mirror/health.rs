//! When a video encoder that produces nothing is given up on. Pure.
//!
//! The pacer offers a picture that came out of the encoder with nothing again at the next
//! kick, so an encoder that keeps failing would be retried for ever and the mirror would
//! freeze without a word. Here the failures in a row are counted: past a limit (or at once
//! for a session the system calls invalid) the encoder is made anew, and when the new one
//! fares no better the source fails with a reason.

/// Errors in a row before the encoder is made anew (about half a second of retries).
pub const ERROR_LIMIT: u32 = 25;
/// Pictures dropped in a row without an error (rate control does that now and then) before
/// the same: several seconds.
pub const DROP_LIMIT: u32 = 250;

/// Why a picture gave no output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The encoder dropped it on purpose.
    Dropped,
    /// The encoder reported an error, or its output could not be used.
    Error,
    /// The encoder session is gone: only a new one can work.
    InvalidSession,
}

/// What to do after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Go on: the pacer offers the picture again.
    Retry,
    /// Replace the encoder before the next picture.
    Recreate,
    /// The new encoder failed too.
    GiveUp,
}

#[derive(Debug, Default)]
pub struct EncoderHealth {
    errors: u32,
    drops: u32,
    /// The encoder was replaced and has not produced anything since.
    recreated: bool,
    gave_up: bool,
    /// Which encoder counts now: one more with every [`Verdict::Recreate`].
    generation: u64,
}

impl EncoderHealth {
    pub fn new() -> EncoderHealth {
        EncoderHealth::default()
    }

    /// The number of the encoder to make now; its results are reported with it. What an
    /// encoder already being replaced still reports does not count: it had pictures in it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A picture came out.
    pub fn delivered(&mut self, generation: u64) {
        if !self.gave_up && generation == self.generation {
            self.errors = 0;
            self.drops = 0;
            self.recreated = false;
        }
    }

    pub fn failed(&mut self, generation: u64, failure: Failure) -> Verdict {
        if self.gave_up {
            return Verdict::GiveUp;
        }
        if generation != self.generation {
            return Verdict::Retry;
        }
        match failure {
            Failure::Dropped => self.drops += 1,
            Failure::Error | Failure::InvalidSession => self.errors += 1,
        }
        let over = failure == Failure::InvalidSession || self.errors >= ERROR_LIMIT || self.drops >= DROP_LIMIT;
        if !over {
            return Verdict::Retry;
        }
        if self.recreated {
            self.gave_up = true;
            return Verdict::GiveUp;
        }
        self.recreated = true;
        self.errors = 0;
        self.drops = 0;
        self.generation += 1;
        Verdict::Recreate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::pace::Gate;

    /// A failure of the current encoder.
    fn fail(health: &mut EncoderHealth, failure: Failure) -> Verdict {
        health.failed(health.generation(), failure)
    }

    #[test]
    fn what_the_replaced_encoder_still_reports_does_not_count() {
        let mut health = EncoderHealth::new();
        let old = health.generation();
        assert_eq!(health.failed(old, Failure::InvalidSession), Verdict::Recreate);
        // The second picture that was in the lost session.
        assert_eq!(health.failed(old, Failure::InvalidSession), Verdict::Retry);
        health.delivered(old);
        assert_eq!(fail(&mut health, Failure::InvalidSession), Verdict::GiveUp);
    }

    /// An encoder that fails on every picture, driven the way the capture drives it: the
    /// pacer keeps offering the picture, and the policy ends that.
    #[test]
    fn a_failing_encoder_is_replaced_once_and_then_given_up() {
        let gate = Gate::new();
        let mut health = EncoderHealth::new();
        let mut verdicts = Vec::new();
        let mut next = gate.captured();
        while let Some(encode) = next {
            gate.dropped(encode.keyframe);
            let verdict = health.failed(health.generation(), Failure::Error);
            verdicts.push(verdict);
            assert!(verdicts.len() <= 1000, "retried for ever");
            next = (verdict != Verdict::GiveUp).then(|| gate.poll()).flatten();
        }
        assert_eq!(verdicts.len(), 2 * ERROR_LIMIT as usize);
        assert_eq!(verdicts.iter().filter(|v| **v == Verdict::Recreate).count(), 1);
        assert_eq!(verdicts[ERROR_LIMIT as usize - 1], Verdict::Recreate);
        assert_eq!(verdicts.last(), Some(&Verdict::GiveUp));
        // Nothing brings it back.
        health.delivered(health.generation());
        assert_eq!(fail(&mut health, Failure::Error), Verdict::GiveUp);
    }

    #[test]
    fn an_invalid_session_is_replaced_at_once() {
        let mut health = EncoderHealth::new();
        assert_eq!(fail(&mut health, Failure::InvalidSession), Verdict::Recreate);
        assert_eq!(fail(&mut health, Failure::InvalidSession), Verdict::GiveUp);
    }

    #[test]
    fn output_forgets_the_failures() {
        let mut health = EncoderHealth::new();
        for _ in 0..10 {
            for _ in 1..ERROR_LIMIT {
                assert_eq!(fail(&mut health, Failure::Error), Verdict::Retry);
            }
            health.delivered(health.generation());
        }
        // The new encoder works, so a later loss of the session gets a new one again (the
        // Mac slept twice).
        for _ in 0..3 {
            assert_eq!(fail(&mut health, Failure::InvalidSession), Verdict::Recreate);
            health.delivered(health.generation());
        }
    }

    #[test]
    fn drops_without_an_error_have_far_more_patience() {
        let mut health = EncoderHealth::new();
        for _ in 1..DROP_LIMIT {
            assert_eq!(fail(&mut health, Failure::Dropped), Verdict::Retry);
        }
        assert_eq!(fail(&mut health, Failure::Dropped), Verdict::Recreate);
    }
}
