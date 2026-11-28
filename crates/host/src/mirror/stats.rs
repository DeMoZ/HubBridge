//! The once-a-second readout of a running mirror. Pure: the clock comes in with every call.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hubbridge_proto::VideoStatus;
use serde::Serialize;

/// What the window shows and the CLI prints every second.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    /// Frames sent per second.
    pub fps: f64,
    pub kbps: f64,
    /// Captured frames that were not sent (since the start).
    pub skipped: u64,
    /// Keyframes sent (since the start).
    pub keyframes: u64,
    /// Capture to the phone decoder's acknowledgement, over the last second; none without acks.
    pub delay_ms: Option<f64>,
    pub delay_max_ms: Option<f64>,
    /// Round trip of the video session (the latest ping, also while frames flow).
    pub rtt_ms: Option<f64>,
    /// Frames the phone decoded per second.
    pub phone_fps: Option<f64>,
    /// Frames the phone decoded but did not show (since the start).
    pub phone_dropped: u64,
    pub phone_bad: u64,
    pub decode_ms: Option<f64>,
    pub phone_locked: bool,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Debug)]
pub struct Stats {
    width: u32,
    height: u32,
    since: Instant,
    frames: u64,
    bytes: u64,
    keyframes: u64,
    /// Sent frames waiting for their acknowledgement: capture time.
    captured: HashMap<u64, Instant>,
    delay_sum: Duration,
    delay_max: Duration,
    delays: u32,
    rtt: Option<Duration>,
    status: Option<(VideoStatus, Instant)>,
    phone_fps: Option<f64>,
}

impl Stats {
    pub fn new(width: u32, height: u32, now: Instant) -> Stats {
        Stats {
            width,
            height,
            since: now,
            frames: 0,
            bytes: 0,
            keyframes: 0,
            captured: HashMap::new(),
            delay_sum: Duration::ZERO,
            delay_max: Duration::ZERO,
            delays: 0,
            rtt: None,
            status: None,
            phone_fps: None,
        }
    }

    /// A picture frame (not the decoder configuration) was sent.
    pub fn on_sent(&mut self, seq: u64, captured: Instant, bytes: usize, keyframe: bool) {
        self.frames += 1;
        self.bytes += bytes as u64;
        self.keyframes += u64::from(keyframe);
        self.captured.insert(seq, captured);
    }

    /// Bytes of a frame that is not a picture.
    pub fn on_config(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
    }

    pub fn on_ack(&mut self, seq: u64, now: Instant) {
        if let Some(at) = self.captured.remove(&seq) {
            let delay = now.saturating_duration_since(at);
            self.delay_sum += delay;
            self.delay_max = self.delay_max.max(delay);
            self.delays += 1;
        }
    }

    pub fn on_rtt(&mut self, rtt: Duration) {
        self.rtt = Some(rtt);
    }

    pub fn on_status(&mut self, status: VideoStatus, now: Instant) {
        if let Some((before, at)) = self.status {
            let secs = now.saturating_duration_since(at).as_secs_f64();
            if secs > 0.0 {
                self.phone_fps = Some(status.decoded.saturating_sub(before.decoded) as f64 / secs);
            }
        }
        self.status = Some((status, now));
    }

    /// The readout since the previous call; starts the next interval.
    pub fn take(&mut self, now: Instant, skipped: u64) -> Snapshot {
        let secs = now.saturating_duration_since(self.since).as_secs_f64().max(0.001);
        let status = self.status.map(|(s, _)| s);
        let snapshot = Snapshot {
            width: self.width,
            height: self.height,
            fps: self.frames as f64 / secs,
            kbps: self.bytes as f64 * 8.0 / 1000.0 / secs,
            skipped,
            keyframes: self.keyframes,
            delay_ms: (self.delays > 0).then(|| ms(self.delay_sum) / f64::from(self.delays)),
            delay_max_ms: (self.delays > 0).then(|| ms(self.delay_max)),
            rtt_ms: self.rtt.map(ms),
            phone_fps: self.phone_fps,
            phone_dropped: status.map_or(0, |s| s.dropped),
            phone_bad: status.map_or(0, |s| s.bad),
            decode_ms: status.map(|s| f64::from(s.decode_us) / 1000.0),
            phone_locked: status.is_some_and(|s| s.phone_locked),
        };
        self.since = now;
        self.frames = 0;
        self.bytes = 0;
        self.delay_sum = Duration::ZERO;
        self.delay_max = Duration::ZERO;
        self.delays = 0;
        snapshot
    }
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let opt = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.0}"));
        write!(
            f,
            "{}x{} H.264  {:.0} fps  {:.0} kbit/s  skipped {}  keyframes {}  delay {} ms (max {})  \
             rtt {} ms  phone {} fps, dropped {}, decode {} ms",
            self.width,
            self.height,
            self.fps,
            self.kbps,
            self.skipped,
            self.keyframes,
            opt(self.delay_ms),
            opt(self.delay_max_ms),
            opt(self.rtt_ms),
            opt(self.phone_fps),
            self.phone_dropped,
            opt(self.decode_ms),
        )?;
        if self.phone_bad > 0 {
            write!(f, "  bad frames {}", self.phone_bad)?;
        }
        if self.phone_locked {
            write!(f, "  phone locked")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_of_frames_with_a_fake_clock() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut s = Stats::new(1108, 720, t0);
        s.on_config(40);
        for seq in 1..=30u64 {
            let captured = at(seq * 33);
            s.on_sent(seq, captured, 1_000, seq == 1);
            let delay = if seq == 30 { 80 } else { 20 };
            s.on_ack(seq, captured + Duration::from_millis(delay));
        }
        s.on_rtt(Duration::from_millis(4));
        let status = |decoded, dropped| VideoStatus {
            decoded,
            dropped,
            bad: 0,
            decode_us: 5_500,
            phone_locked: false,
        };
        s.on_status(status(0, 0), at(0));
        s.on_status(status(29, 1), at(1000));
        let got = s.take(at(1000), 3);
        assert_eq!((got.width, got.height), (1108, 720));
        assert_eq!(got.fps, 30.0);
        assert_eq!(got.kbps, (30_000.0 + 40.0) * 8.0 / 1000.0);
        assert_eq!((got.skipped, got.keyframes), (3, 1));
        assert_eq!(got.delay_ms, Some(22.0));
        assert_eq!(got.delay_max_ms, Some(80.0));
        assert_eq!(got.rtt_ms, Some(4.0));
        assert_eq!(got.phone_fps, Some(29.0));
        assert_eq!((got.phone_dropped, got.decode_ms), (1, Some(5.5)));

        // A still second: zero rates, no delay, totals stay.
        s.on_status(
            VideoStatus {
                phone_locked: true,
                ..status(29, 1)
            },
            at(2000),
        );
        let still = s.take(at(2000), 3);
        assert_eq!((still.fps, still.kbps, still.delay_ms), (0.0, 0.0, None));
        assert_eq!((still.keyframes, still.phone_fps), (1, Some(0.0)));
        assert!(still.phone_locked);
        let line = still.to_string();
        assert!(
            line.contains("0 fps") && line.contains("delay - ms") && line.ends_with("phone locked"),
            "{line}"
        );
    }
}
