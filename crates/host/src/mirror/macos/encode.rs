//! H.264 in hardware with VideoToolbox in low-latency mode: no frame reordering, keyframes only
//! when forced, one sample out per picture in. Samples leave as Annex B.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};
use tracing::{info, warn};

use crate::mirror::annexb;
use crate::mirror::source::{EncodeConfig, EncodedFrame};

/// What came out of the encoder.
#[derive(Debug)]
pub enum Output {
    /// A config frame precedes the first keyframe and every change of the parameter sets.
    Frame(EncodedFrame),
    /// A submitted picture produced nothing; `forced` says whether a keyframe was asked for.
    Dropped { forced: bool },
}

type Handler = Box<dyn Fn(Output) + Send + Sync>;

struct Context {
    handler: Handler,
    /// The parameter sets sent last.
    parameter_sets: Mutex<Vec<Vec<u8>>>,
}

/// Travels through VideoToolbox with one picture.
struct Picture {
    captured: Instant,
    forced: bool,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// The callback holds a pointer to it until the session is invalidated.
    context: *mut Context,
    began: Instant,
    last_pts: i64,
    profile: &'static str,
}

// SAFETY: VideoToolbox sessions may be used from any thread; `Encoder` is used by one at a
// time (`&mut self`), and the context is `Sync`.
unsafe impl Send for Encoder {}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    // SAFETY: a compression session is a VideoToolbox session (a CF type).
    let session: &VTSession = unsafe { &*ptr::from_ref(session).cast() };
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

impl Encoder {
    pub fn new(config: EncodeConfig, handler: impl Fn(Output) + Send + Sync + 'static) -> Result<Encoder> {
        let yes: &CFType = CFBoolean::new(true).as_ref();
        let no: &CFType = CFBoolean::new(false).as_ref();
        // SAFETY: reading framework constants.
        let low_latency = unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl };
        let specification = CFDictionary::<CFString, CFType>::from_slices(&[low_latency], &[yes]);
        let context = Box::into_raw(Box::new(Context {
            handler: Box::new(handler),
            parameter_sets: Mutex::new(Vec::new()),
        }));
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: the out pointer is valid; the callback matches its context, which lives
        // until after `invalidate` in `Drop`.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                kCMVideoCodecType_H264,
                Some(specification.as_opaque()),
                None,
                None,
                Some(output_callback),
                context.cast(),
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            // SAFETY: no session holds the context.
            drop(unsafe { Box::from_raw(context) });
            bail!("cannot create the H.264 encoder (VideoToolbox error {status})");
        };
        // SAFETY: `create` returned a session we own.
        let session = unsafe { CFRetained::from_raw(raw) };
        let mut encoder = Encoder {
            session,
            context,
            began: Instant::now(),
            last_pts: 0,
            profile: "ConstrainedHigh",
        };
        encoder.configure(config, yes, no)?;
        Ok(encoder)
    }

    fn configure(&mut self, config: EncodeConfig, yes: &CFType, no: &CFType) -> Result<()> {
        let s = &self.session;
        let bitrate = CFNumber::new_i64(i64::from(config.bitrate_kbps) * 1000);
        let fps = CFNumber::new_i32(config.fps as i32);
        // SAFETY: reading framework constants.
        unsafe {
            for (name, key, value) in [
                ("RealTime", kVTCompressionPropertyKey_RealTime, yes),
                (
                    "AllowFrameReordering",
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    no,
                ),
                (
                    "AverageBitRate",
                    kVTCompressionPropertyKey_AverageBitRate,
                    bitrate.as_ref(),
                ),
                (
                    "ExpectedFrameRate",
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    fps.as_ref(),
                ),
            ] {
                let status = set(s, key, value);
                if status != 0 {
                    bail!("the H.264 encoder refused {name} (VideoToolbox error {status})");
                }
            }
            // BT.709, so that the phone decodes the colors right.
            for (key, value) in [
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
            ] {
                let status = set(s, key, value.as_ref());
                if status != 0 {
                    warn!(status, "the H.264 encoder refused a color tag");
                }
            }
            let profile = kVTCompressionPropertyKey_ProfileLevel;
            let status = set(s, profile, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel.as_ref());
            if status != 0 {
                info!(status, "ConstrainedHigh refused, using High");
                self.profile = "High";
                let status = set(s, profile, kVTProfileLevel_H264_High_AutoLevel.as_ref());
                if status != 0 {
                    bail!("the H.264 encoder refused the High profile (VideoToolbox error {status})");
                }
            }
            let status = s.prepare_to_encode_frames();
            if status != 0 {
                bail!("the H.264 encoder cannot start (VideoToolbox error {status})");
            }
        }
        info!(
            profile = self.profile,
            width = config.width,
            height = config.height,
            "H.264 encoder ready"
        );
        Ok(())
    }

    /// The profile the encoder accepted.
    pub fn profile(&self) -> &'static str {
        self.profile
    }

    /// Submit a picture. On an error no output follows for it.
    pub fn encode(&mut self, picture: &CVPixelBuffer, force_keyframe: bool, captured: Instant) -> Result<()> {
        // Strictly increasing, also when the same picture is encoded again.
        let pts = (self.began.elapsed().as_micros() as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        let options = force_keyframe.then(|| {
            // SAFETY: reading a framework constant.
            let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            let yes: &CFType = CFBoolean::new(true).as_ref();
            CFDictionary::<CFString, CFType>::from_slices(&[key], &[yes])
        });
        let info = Box::into_raw(Box::new(Picture {
            captured,
            forced: force_keyframe,
        }));
        let mut flags = VTEncodeInfoFlags(0);
        // SAFETY: `info` is reclaimed by the callback, or below when no callback will come.
        let status = unsafe {
            self.session.encode_frame(
                picture,
                CMTime::new(pts, 1_000_000),
                kCMTimeInvalid,
                options.as_deref().map(CFDictionary::as_opaque),
                info.cast(),
                &mut flags,
            )
        };
        if status != 0 {
            // SAFETY: a refused picture never reaches the callback.
            drop(unsafe { Box::from_raw(info) });
            bail!("cannot encode a picture (VideoToolbox error {status})");
        }
        Ok(())
    }

    /// Wait until every submitted picture came out.
    pub fn finish(&mut self) {
        // SAFETY: a valid session.
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            warn!(status, "the encoder did not finish");
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.finish();
        // SAFETY: after `invalidate` no callback runs, so nothing else holds the context.
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self.context));
        }
    }
}

/// Whether the sample is a keyframe: it lacks the "not sync" attachment.
unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else {
            return true;
        };
        if attachments.count() == 0 {
            return true;
        }
        let first = attachments.value_at_index(0).cast::<CFDictionary>();
        let Some(first) = first.as_ref() else {
            return true;
        };
        let key: *const CFString = kCMSampleAttachmentKey_NotSync;
        let value = first.value(key.cast());
        value
            .cast::<CFBoolean>()
            .as_ref()
            .is_none_or(|not_sync| !not_sync.value())
    }
}

/// The parameter sets of the sample and the size of its NAL length fields.
unsafe fn parameter_sets(sample: &CMSampleBuffer) -> Option<(Vec<Vec<u8>>, usize)> {
    unsafe {
        let description = sample.format_description()?;
        let mut sets = Vec::new();
        let (mut count, mut length_size, mut index) = (0usize, 0i32, 0usize);
        loop {
            let mut data: *const u8 = ptr::null();
            let mut size = 0usize;
            let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &description,
                index,
                &mut data,
                &mut size,
                &mut count,
                &mut length_size,
            );
            if status != 0 || data.is_null() {
                return None;
            }
            sets.push(std::slice::from_raw_parts(data, size).to_vec());
            index += 1;
            if index >= count {
                break;
            }
        }
        Some((sets, usize::try_from(length_size).ok()?))
    }
}

unsafe fn sample_bytes(sample: &CMSampleBuffer) -> Option<Vec<u8>> {
    unsafe {
        let block = sample.data_buffer()?;
        let len = block.data_length();
        let mut bytes = vec![0u8; len];
        let target = NonNull::new(bytes.as_mut_ptr().cast::<c_void>())?;
        (len > 0 && block.copy_data_bytes(0, len, target) == 0).then_some(bytes)
    }
}

/// Emits the frames of one sample. False when it cannot be used.
unsafe fn deliver(context: &Context, sample: &CMSampleBuffer, captured: Instant) -> bool {
    let (Some((sets, length_size)), Some(bytes)) = (unsafe { parameter_sets(sample) }, unsafe { sample_bytes(sample) })
    else {
        warn!("an encoded sample has no data or parameter sets");
        return false;
    };
    let keyframe = unsafe { is_keyframe(sample) };
    let data = match annexb::to_annexb(&bytes, length_size, keyframe, &sets) {
        Ok(data) => data,
        Err(e) => {
            warn!("an encoded sample is malformed: {e}");
            return false;
        }
    };
    let mut last = context
        .parameter_sets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *last != sets {
        (context.handler)(Output::Frame(EncodedFrame {
            data: annexb::config_frame(&sets),
            keyframe: false,
            config: true,
            captured,
        }));
        *last = sets;
    }
    drop(last);
    (context.handler)(Output::Frame(EncodedFrame {
        data,
        keyframe,
        config: false,
        captured,
    }));
    true
}

unsafe extern "C-unwind" fn output_callback(
    context: *mut c_void,
    picture: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: both pointers are the ones handed to VideoToolbox; every picture comes back once.
    let (context, picture) = unsafe { (&*context.cast::<Context>(), Box::from_raw(picture.cast::<Picture>())) };
    let delivered = status == 0
        && !flags.contains(VTEncodeInfoFlags::FrameDropped)
        && unsafe { sample.as_ref() }.is_some_and(|sample| unsafe { deliver(context, sample, picture.captured) });
    if !delivered {
        if status != 0 {
            warn!(status, "the encoder failed on a picture");
        }
        (context.handler)(Output::Dropped { forced: picture.forced });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hubbridge_proto::h264::{self, NAL_IDR, NAL_PPS, NAL_SPS};
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };

    use super::*;

    /// `420v`.
    const FORMAT: u32 = u32::from_be_bytes(*b"420v");

    /// A gray ramp with a bright bar that moves with `n`.
    fn picture(width: usize, height: usize, n: usize) -> CFRetained<CVPixelBuffer> {
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: a valid out pointer; the planes are written within their rows and height.
        unsafe {
            let status = CVPixelBufferCreate(None, width, height, FORMAT, None, NonNull::from(&mut raw));
            assert_eq!(status, 0);
            let buffer = CFRetained::from_raw(NonNull::new(raw).unwrap());
            assert_eq!(CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)), 0);
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                for y in 0..CVPixelBufferGetHeightOfPlane(&buffer, plane) {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                    for (x, byte) in row.iter_mut().enumerate() {
                        *byte = match plane {
                            0 if (n * 16..n * 16 + 64).contains(&x) => 235,
                            0 => 16 + ((x + y) % 200) as u8,
                            _ => 128,
                        };
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            buffer
        }
    }

    /// Needs the hardware encoder: `cargo test -p hubbridge-host -- --ignored encode`.
    #[test]
    #[ignore = "uses the VideoToolbox encoder of this Mac"]
    fn encode_synthetic_pictures() {
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&outputs);
        let config = EncodeConfig {
            width: 1108,
            height: 720,
            fps: 30,
            bitrate_kbps: 5983,
        };
        let mut encoder = Encoder::new(config, move |o| sink.lock().unwrap().push(o)).unwrap();
        println!("profile used: {}", encoder.profile());
        for n in 0..60 {
            encoder
                .encode(&picture(1108, 720, n), n == 0 || n == 30, Instant::now())
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        encoder.finish();
        drop(encoder);

        let outputs = outputs.lock().unwrap();
        let dropped = outputs.iter().filter(|o| matches!(o, Output::Dropped { .. })).count();
        let frames: Vec<&EncodedFrame> = outputs
            .iter()
            .filter_map(|o| match o {
                Output::Frame(f) => Some(f),
                Output::Dropped { .. } => None,
            })
            .collect();
        println!(
            "{} frames, {dropped} dropped, {} bytes",
            frames.len(),
            frames.iter().map(|f| f.data.len()).sum::<usize>()
        );
        assert_eq!(frames.iter().filter(|f| !f.config).count() + dropped, 60);

        assert!(frames[0].config);
        let config = h264::validate(&frames[0].data).unwrap();
        assert!(config.has_parameter_sets() && !config.has_slice(), "{config:?}");
        let first = h264::validate(&frames[1].data).unwrap();
        assert!(frames[1].keyframe);
        assert_eq!(&first.types[..2], [NAL_SPS, NAL_PPS]);
        assert!(first.types.contains(&NAL_IDR), "{first:?}");
        let mut keyframes = Vec::new();
        for (n, f) in frames.iter().filter(|f| !f.config).enumerate() {
            let unit = h264::validate(&f.data).unwrap();
            assert!(unit.has_slice());
            assert_eq!(unit.is_keyframe(), f.keyframe);
            if f.keyframe {
                assert!(unit.has_parameter_sets());
                keyframes.push(n);
            }
        }
        if dropped == 0 {
            assert_eq!(keyframes, [0, 30], "keyframes only where forced");
        } else {
            assert!(keyframes.len() >= 2, "{keyframes:?}");
        }
    }
}
