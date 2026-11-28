//! A fake frame source: well-formed Annex B with patterned payloads at a chosen size and rate,
//! no encoder. For tests on every platform; the picture cannot be decoded by a real decoder.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hubbridge_proto::h264::START_CODE;

use super::pace::{Encode, Gate};
use super::source::{EncodeConfig, EncodedFrame, FrameOutput, FrameSource, SourceEvent};

enum Command {
    Kick,
    Keyframe,
    Stop,
}

/// Switches a test can flip while the source runs.
#[derive(Debug, Clone, Default)]
pub struct Controls {
    /// The "screen" does not change: no new pictures.
    pub still: Arc<AtomicBool>,
    /// Set once the source was stopped.
    pub stopped: Arc<AtomicBool>,
}

pub struct SyntheticSource {
    width: u32,
    height: u32,
    /// Bytes of a P frame; a keyframe is four times as long.
    frame_bytes: usize,
    controls: Controls,
    running: Option<(mpsc::Sender<Command>, JoinHandle<()>)>,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32) -> SyntheticSource {
        SyntheticSource {
            width,
            height,
            frame_bytes: 2_000,
            controls: Controls::default(),
            running: None,
        }
    }

    pub fn with_frame_bytes(mut self, bytes: usize) -> SyntheticSource {
        self.frame_bytes = bytes.max(1);
        self
    }

    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    fn send(&self, command: Command) {
        if let Some((tx, _)) = &self.running {
            let _ = tx.send(command);
        }
    }
}

/// A NAL unit with its start code. The payload has no zero bytes, so no false start codes.
fn nal(header: u8, len: usize, seed: u64) -> Vec<u8> {
    let mut out = START_CODE.to_vec();
    out.push(header);
    out.extend((0..len).map(|i| 0x80 | ((seed as usize + i) % 127) as u8));
    out
}

fn parameter_sets() -> Vec<u8> {
    [nal(0x67, 8, 1), nal(0x68, 3, 2)].concat()
}

struct Encoder {
    frame_bytes: usize,
    configured: bool,
    frames: u64,
    captured: Instant,
    output: FrameOutput,
}

impl Encoder {
    fn emit(&self, data: Vec<u8>, keyframe: bool, config: bool) {
        (self.output)(SourceEvent::Frame(EncodedFrame {
            data,
            keyframe,
            config,
            captured: self.captured,
        }));
    }

    fn encode(&mut self, encode: Encode) {
        self.frames += 1;
        if !self.configured {
            self.configured = true;
            self.emit(parameter_sets(), false, true);
        }
        let data = if encode.keyframe {
            [parameter_sets(), nal(0x65, self.frame_bytes * 4, self.frames)].concat()
        } else {
            nal(0x41, self.frame_bytes, self.frames)
        };
        self.emit(data, encode.keyframe, false);
    }
}

impl FrameSource for SyntheticSource {
    fn native_size(&mut self) -> Result<(u32, u32)> {
        Ok((self.width, self.height))
    }

    fn start(&mut self, config: EncodeConfig, gate: Gate, output: FrameOutput) -> Result<()> {
        if self.running.is_some() || config.fps == 0 {
            bail!("the source cannot start");
        }
        let (tx, rx) = mpsc::channel();
        let still = Arc::clone(&self.controls.still);
        let mut encoder = Encoder {
            frame_bytes: self.frame_bytes,
            configured: false,
            frames: 0,
            captured: Instant::now(),
            output,
        };
        let interval = Duration::from_secs(1) / config.fps;
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            loop {
                match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                    Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(command) => {
                        if matches!(command, Command::Keyframe) {
                            gate.want_keyframe();
                        }
                        // The held picture (or the last one again).
                        if let Some(encode) = gate.poll() {
                            encoder.encode(encode);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        next = (next + interval).max(Instant::now());
                        if !still.load(Ordering::Relaxed) {
                            encoder.captured = Instant::now();
                            if let Some(encode) = gate.captured() {
                                encoder.encode(encode);
                            }
                        }
                    }
                }
            }
        });
        self.running = Some((tx, thread));
        Ok(())
    }

    fn kick(&mut self) {
        self.send(Command::Kick);
    }

    fn request_keyframe(&mut self) {
        self.send(Command::Keyframe);
    }

    fn stop(&mut self) {
        self.send(Command::Stop);
        if let Some((_, thread)) = self.running.take() {
            let _ = thread.join();
        }
        self.controls.stopped.store(true, Ordering::SeqCst);
    }
}

impl Drop for SyntheticSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hubbridge_proto::h264;

    use super::*;

    const CONFIG: EncodeConfig = EncodeConfig {
        width: 640,
        height: 360,
        fps: 60,
        bitrate_kbps: 2000,
    };

    type Frames = Arc<Mutex<Vec<EncodedFrame>>>;

    /// Run the source, acknowledging every frame at once, for `time`.
    fn run(source: &mut SyntheticSource, time: Duration) -> (Frames, Gate) {
        let frames = Frames::default();
        let gate = Gate::new();
        let (sink, acks) = (Arc::clone(&frames), gate.clone());
        let output: FrameOutput = Arc::new(move |event| {
            let SourceEvent::Frame(frame) = event else {
                panic!("unexpected {event:?}")
            };
            if !frame.config {
                let seq = acks.output();
                acks.ack(seq);
            }
            sink.lock().unwrap().push(frame);
        });
        source.start(CONFIG, gate.clone(), output).unwrap();
        std::thread::sleep(time);
        (frames, gate)
    }

    #[test]
    fn the_output_is_valid_annex_b() {
        let mut source = SyntheticSource::new(640, 360).with_frame_bytes(500);
        let (frames, _gate) = run(&mut source, Duration::from_millis(300));
        source.stop();
        let frames = frames.lock().unwrap();
        assert!(frames.len() > 5, "{}", frames.len());
        assert!(frames[0].config);
        let config = h264::validate(&frames[0].data).unwrap();
        assert!(config.has_parameter_sets() && !config.has_slice());
        let first = h264::validate(&frames[1].data).unwrap();
        assert!(frames[1].keyframe && first.is_keyframe() && first.has_parameter_sets());
        for f in &frames[2..] {
            let unit = h264::validate(&f.data).unwrap();
            assert_eq!(unit.is_keyframe(), f.keyframe);
            assert!(unit.has_slice() && !f.config);
        }
        assert!(frames[2].data.len() >= 500);
        assert!(source.controls().stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn a_still_source_produces_no_frames() {
        let mut source = SyntheticSource::new(640, 360);
        source.controls().still.store(true, Ordering::Relaxed);
        let (frames, _gate) = run(&mut source, Duration::from_millis(200));
        source.request_keyframe();
        std::thread::sleep(Duration::from_millis(100));
        assert!(frames.lock().unwrap().is_empty(), "there is no picture yet");
    }

    #[test]
    fn a_picture_that_became_still_is_encoded_again_on_request() {
        let mut source = SyntheticSource::new(640, 360);
        let controls = source.controls();
        let (frames, _gate) = run(&mut source, Duration::from_millis(100));
        controls.still.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(600));
        let before = frames.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames.lock().unwrap().len(), before);
        source.request_keyframe();
        std::thread::sleep(Duration::from_millis(200));
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), before + 1);
        assert!(frames.last().unwrap().keyframe);
    }

    #[test]
    fn a_full_window_stops_the_encoder() {
        let mut source = SyntheticSource::new(640, 360);
        let frames = Frames::default();
        let sink = Arc::clone(&frames);
        let gate = Gate::new();
        source
            .start(
                CONFIG,
                gate.clone(),
                Arc::new(move |e| {
                    if let SourceEvent::Frame(f) = e {
                        sink.lock().unwrap().push(f);
                    }
                }),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // Nothing is acknowledged: the config frame and two pictures.
        assert_eq!(frames.lock().unwrap().len(), 3);
        assert_eq!(gate.in_flight(), 2);
        assert!(gate.skipped() > 3);
    }
}
