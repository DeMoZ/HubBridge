//! A real, decodable H.264 stream that needs no capture and no encoder: a built-in 2 s
//! 640×360 30 fps test picture, looped from its keyframe. For checking the phone's decoder
//! before (or without) the screen capture, also from Windows.
//!
//! The clip (`crates/host/assets/test-pattern.h264`, no B-frames, one keyframe at the start)
//! was made once with:
//!
//! ```text
//! ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 2 -c:v libx264 -profile:v high -bf 0 \
//!   -g 60 -pix_fmt yuv420p -f h264 crates/host/assets/test-pattern.h264
//! ```

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hubbridge_proto::h264::{self, NAL_IDR, NAL_NON_IDR, NAL_PPS, NAL_SPS, START_CODE};

use super::pace::{Encode, Gate};
use super::source::{EncodeConfig, EncodedFrame, FrameOutput, FrameSource, SourceEvent};

const CLIP: &[u8] = include_bytes!("../../assets/test-pattern.h264");
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 360;

/// The clip cut into access units; the parameter sets apart.
struct Clip {
    config: Vec<u8>,
    /// The first is the keyframe, with the parameter sets in front.
    frames: Vec<Vec<u8>>,
}

fn parse(data: &[u8]) -> Result<Clip> {
    let mut sets = Vec::new();
    let mut frames = Vec::new();
    let mut unit = Vec::new();
    for nal in h264::split_nals(data) {
        let kind = h264::nal_type(nal);
        unit.extend_from_slice(&START_CODE);
        unit.extend_from_slice(nal);
        if kind == NAL_SPS || kind == NAL_PPS {
            sets.extend_from_slice(&START_CODE);
            sets.extend_from_slice(nal);
        }
        if (NAL_NON_IDR..=NAL_IDR).contains(&kind) {
            // One slice per picture: `first_mb_in_slice` is 0, a single set bit.
            if nal.get(1).is_none_or(|b| b & 0x80 == 0) {
                bail!("the test clip has pictures of several slices");
            }
            frames.push(std::mem::take(&mut unit));
        }
    }
    let first = frames.first().context("the test clip has no picture")?;
    if !(h264::is_keyframe(first) && h264::has_parameter_sets(first)) {
        bail!("the test clip does not start with SPS, PPS and a keyframe");
    }
    Ok(Clip { config: sets, frames })
}

enum Command {
    Kick,
    Keyframe,
    Stop,
}

#[derive(Default)]
pub struct TestPattern {
    running: Option<(mpsc::Sender<Command>, JoinHandle<()>)>,
}

impl TestPattern {
    pub fn new() -> TestPattern {
        TestPattern::default()
    }

    fn send(&self, command: Command) {
        if let Some((tx, _)) = &self.running {
            let _ = tx.send(command);
        }
    }
}

/// Plays the clip one access unit per encoded picture: nothing is lost when the gate holds
/// back, and a forced keyframe starts the clip over.
struct Player {
    clip: Clip,
    next: usize,
    configured: bool,
    captured: Instant,
    output: FrameOutput,
}

impl Player {
    fn emit(&self, data: Vec<u8>, keyframe: bool, config: bool) {
        (self.output)(SourceEvent::Frame(EncodedFrame {
            data,
            keyframe,
            config,
            captured: self.captured,
        }));
    }

    fn play(&mut self, encode: Encode) {
        if encode.keyframe {
            self.next = 0;
        }
        if !self.configured {
            self.configured = true;
            self.emit(self.clip.config.clone(), false, true);
        }
        let data = self.clip.frames[self.next].clone();
        let keyframe = h264::is_keyframe(&data);
        self.next = (self.next + 1) % self.clip.frames.len();
        self.emit(data, keyframe, false);
    }
}

impl FrameSource for TestPattern {
    fn native_size(&mut self) -> Result<(u32, u32)> {
        Ok((WIDTH, HEIGHT))
    }

    /// The pictures are encoded already: the stream has their size, the phone scales it.
    fn stream_size(&mut self, _display_width: u32, _display_height: u32) -> Result<(u32, u32)> {
        Ok((WIDTH, HEIGHT))
    }

    fn start(&mut self, config: EncodeConfig, gate: Gate, output: FrameOutput) -> Result<()> {
        if self.running.is_some() || config.fps == 0 {
            bail!("the source cannot start");
        }
        let mut player = Player {
            clip: parse(CLIP)?,
            next: 0,
            configured: false,
            captured: Instant::now(),
            output,
        };
        let (tx, rx) = mpsc::channel();
        let interval = Duration::from_secs(1) / config.fps;
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            loop {
                let encode = match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                    Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(Command::Kick) => gate.poll(),
                    Ok(Command::Keyframe) => {
                        gate.want_keyframe();
                        gate.poll()
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        next = (next + interval).max(Instant::now());
                        player.captured = Instant::now();
                        gate.captured()
                    }
                };
                if let Some(encode) = encode {
                    player.play(encode);
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
    }
}

impl Drop for TestPattern {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn the_embedded_clip_is_a_looping_annex_b_stream() {
        assert!(CLIP.len() <= 256 * 1024);
        let whole = h264::validate(CLIP).unwrap();
        // SPS, PPS and an IDR slice come first (SEI may sit between them).
        let first: Vec<u8> = whole.types.iter().copied().filter(|t| *t != 6).take(3).collect();
        assert_eq!(first, [NAL_SPS, NAL_PPS, NAL_IDR]);
        let clip = parse(CLIP).unwrap();
        assert_eq!(clip.frames.len(), 60);
        let config = h264::validate(&clip.config).unwrap();
        assert!(config.has_parameter_sets() && !config.has_slice());
        for (i, frame) in clip.frames.iter().enumerate() {
            let unit = h264::validate(frame).unwrap();
            assert!(unit.has_slice(), "frame {i}");
            assert_eq!(unit.is_keyframe(), i == 0, "frame {i}");
        }
    }

    #[test]
    fn it_plays_in_order_loops_and_restarts_for_a_keyframe() {
        let clip = parse(CLIP).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);
        let mut player = Player {
            clip: parse(CLIP).unwrap(),
            next: 0,
            configured: false,
            captured: Instant::now(),
            output: Arc::new(move |e| {
                if let SourceEvent::Frame(f) = e {
                    sink.lock().unwrap().push(f);
                }
            }),
        };
        for _ in 0..62 {
            player.play(Encode { keyframe: false });
        }
        player.play(Encode { keyframe: true });
        let frames = frames.lock().unwrap();
        assert!(frames[0].config);
        assert_eq!(frames[1].data, clip.frames[0]);
        assert_eq!(frames[60].data, clip.frames[59]);
        assert_eq!(frames[61].data, clip.frames[0], "the loop");
        assert!(frames[61].keyframe);
        assert_eq!(frames[63].data, clip.frames[0], "a forced keyframe restarts");
        assert!(frames[63].keyframe && !frames[62].keyframe);
    }
}
