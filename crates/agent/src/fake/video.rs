//! A video output without a decoder: it checks that the stream is well-formed H.264 Annex B
//! with the decoder configuration before the first keyframe, counts what it "decoded" and can
//! write the received bytes to a file that standard tools play.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use hubbridge_proto::{DisplayInfo, VideoConfig, VideoFrame, h264};
use tracing::warn;

use crate::video::{FrameResult, SinkStats, VideoSink};

/// What one stream did, for tests and the simulator's printout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamLog {
    pub display_id: u32,
    /// Sequence numbers of the accepted frames, in order.
    pub seqs: Vec<u64>,
    pub decoded: u64,
    pub bad: u64,
    pub keyframes: u64,
    /// How many times the sink was stopped.
    pub stops: u32,
}

#[derive(Default)]
struct Stream {
    log: StreamLog,
    /// Only a keyframe can be decoded now.
    waiting_keyframe: bool,
    configured: bool,
    file: Option<std::fs::File>,
}

#[derive(Default)]
struct State {
    delay: Duration,
    start_delay: Duration,
    output: Option<PathBuf>,
    fail_start: Option<String>,
    /// Results the next picture frames get instead of being decoded.
    fail_next: VecDeque<FrameResult>,
    streams: BTreeMap<u64, Stream>,
}

#[derive(Default)]
pub struct FakeVideoSink {
    state: Mutex<State>,
}

impl FakeVideoSink {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Time every picture frame takes in the decoder.
    pub fn set_delay(&self, delay: Duration) {
        self.lock().delay = delay;
    }

    pub fn set_start_delay(&self, delay: Duration) {
        self.lock().start_delay = delay;
    }

    /// Write the accepted frames of each new stream to this file.
    pub fn set_output(&self, path: Option<PathBuf>) {
        self.lock().output = path;
    }

    /// Test hook: every later start fails with this message (`None` undoes it).
    pub fn fail_start(&self, message: Option<&str>) {
        self.lock().fail_start = message.map(str::to_string);
    }

    /// Test hook: the next picture frame gets `result`.
    pub fn fail_next(&self, result: FrameResult) {
        self.lock().fail_next.push_back(result);
    }

    pub fn log(&self, stream: u64) -> Option<StreamLog> {
        self.lock().streams.get(&stream).map(|s| s.log.clone())
    }

    /// Every stream ever started, by id.
    pub fn logs(&self) -> BTreeMap<u64, StreamLog> {
        self.lock().streams.iter().map(|(id, s)| (*id, s.log.clone())).collect()
    }
}

/// Whether the frame can be decoded in the stream's present state.
fn accept(stream: &mut Stream, frame: &VideoFrame) -> bool {
    let Ok(unit) = h264::validate(&frame.data) else {
        return false;
    };
    if frame.config {
        let ok = unit.has_parameter_sets() && !unit.has_slice();
        stream.configured |= ok;
        return ok;
    }
    if !unit.has_slice() || unit.is_keyframe() != frame.keyframe {
        return false;
    }
    if frame.keyframe {
        stream.configured |= unit.has_parameter_sets();
        if !stream.configured {
            return false;
        }
        stream.waiting_keyframe = false;
    }
    !stream.waiting_keyframe
}

impl VideoSink for FakeVideoSink {
    fn start(&self, stream: u64, display: &DisplayInfo, _config: &VideoConfig) -> Result<(), String> {
        let delay = self.lock().start_delay;
        std::thread::sleep(delay);
        let mut state = self.lock();
        if let Some(message) = &state.fail_start {
            return Err(message.clone());
        }
        let file = match &state.output {
            Some(path) => Some(std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?),
            None => None,
        };
        state.streams.insert(
            stream,
            Stream {
                log: StreamLog {
                    display_id: display.id,
                    ..StreamLog::default()
                },
                waiting_keyframe: true,
                configured: false,
                file,
            },
        );
        Ok(())
    }

    fn frame(&self, stream: u64, frame: &VideoFrame) -> FrameResult {
        let delay = self.lock().delay;
        if !frame.config {
            std::thread::sleep(delay);
        }
        let mut state = self.lock();
        if !frame.config
            && let Some(result) = state.fail_next.pop_front()
        {
            if let Some(s) = state.streams.get_mut(&stream) {
                s.waiting_keyframe |= result == FrameResult::NeedKeyframe;
            }
            return result;
        }
        let Some(s) = state.streams.get_mut(&stream) else {
            return FrameResult::NeedKeyframe;
        };
        if !accept(s, frame) {
            s.log.bad += 1;
            s.waiting_keyframe = true;
            return FrameResult::NeedKeyframe;
        }
        s.log.seqs.push(frame.seq);
        if !frame.config {
            s.log.decoded += 1;
            s.log.keyframes += u64::from(frame.keyframe);
        }
        if let Some(file) = &mut s.file
            && let Err(e) = file.write_all(&frame.data)
        {
            warn!("cannot write the video file: {e}");
            s.file = None;
        }
        FrameResult::Queued
    }

    fn stats(&self, stream: u64) -> SinkStats {
        let state = self.lock();
        let decode_us = state.delay.as_micros() as u32;
        state
            .streams
            .get(&stream)
            .map_or_else(SinkStats::default, |s| SinkStats {
                decoded: s.log.decoded,
                dropped: 0,
                bad: s.log.bad,
                decode_us,
            })
    }

    fn stop(&self, stream: u64) {
        if let Some(s) = self.lock().streams.get_mut(&stream) {
            s.log.stops += 1;
            s.file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use hubbridge_proto::VideoCodec;

    use super::*;

    const SPS_PPS: &[u8] = &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2];
    const IDR: &[u8] = &[0, 0, 0, 1, 0x65, 3, 4];
    const DELTA: &[u8] = &[0, 0, 0, 1, 0x41, 5];

    fn frame(seq: u64, data: &[u8], keyframe: bool, config: bool) -> VideoFrame {
        VideoFrame {
            seq,
            pts_us: seq * 1000,
            keyframe,
            config,
            data: data.to_vec(),
        }
    }

    /// A sink with stream 1 started.
    fn started_with(output: Option<PathBuf>) -> FakeVideoSink {
        let sink = FakeVideoSink::default();
        sink.set_output(output);
        let display = DisplayInfo {
            id: 8,
            name: "HDMI Screen".into(),
            width: 1280,
            height: 720,
            refresh_mhz: 60_000,
            modes: vec![],
            presentation: true,
            secure: false,
        };
        let config = VideoConfig {
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 6000,
        };
        sink.start(1, &display, &config).unwrap();
        sink
    }

    #[test]
    fn a_valid_sequence() {
        let dir = std::env::temp_dir().join(format!("hb-sink-{}", hubbridge_link::random_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = started_with(Some(dir.join("out.h264")));
        let keyframe: Vec<u8> = [SPS_PPS, IDR].concat();
        for f in [
            frame(1, SPS_PPS, false, true),
            frame(2, &keyframe, true, false),
            frame(3, DELTA, false, false),
        ] {
            assert_eq!(sink.frame(1, &f), FrameResult::Queued);
        }
        sink.stop(1);
        let log = sink.log(1).unwrap();
        assert_eq!(
            (log.seqs, log.decoded, log.keyframes, log.bad, log.stops),
            (vec![1, 2, 3], 2, 1, 0, 1)
        );
        let written = std::fs::read(dir.join("out.h264")).unwrap();
        assert_eq!(written, [SPS_PPS, keyframe.as_slice(), DELTA].concat());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_sequences() {
        let sink = started_with(None);
        let bad = |f| assert_eq!(sink.frame(1, &f), FrameResult::NeedKeyframe);
        // A delta before any keyframe, not Annex B, a keyframe without configuration, flags
        // that do not match the content, a configuration with a picture in it.
        bad(frame(1, DELTA, false, false));
        bad(frame(2, &[0x65, 1, 2], true, false));
        bad(frame(3, IDR, true, false));
        bad(frame(4, DELTA, true, false));
        bad(frame(5, IDR, false, false));
        bad(frame(6, &[SPS_PPS, IDR].concat(), false, true));
        assert_eq!(sink.log(1).unwrap().bad, 6);
        // It recovers with the configuration and a keyframe.
        assert_eq!(sink.frame(1, &frame(7, SPS_PPS, false, true)), FrameResult::Queued);
        assert_eq!(sink.frame(1, &frame(8, IDR, true, false)), FrameResult::Queued);
        assert_eq!(sink.frame(1, &frame(9, DELTA, false, false)), FrameResult::Queued);
        // After a malformed frame only a keyframe works.
        bad(frame(10, &[1, 2, 3], false, false));
        bad(frame(11, DELTA, false, false));
        assert_eq!(
            sink.stats(1),
            SinkStats {
                decoded: 2,
                dropped: 0,
                bad: 8,
                decode_us: 0
            }
        );
    }
}
