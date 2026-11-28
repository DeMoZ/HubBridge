//! The screen of this Mac as a frame source: ScreenCaptureKit captures the main display, scaled
//! on the GPU to the stream size, as `420v` pictures that VideoToolbox takes without conversion.
//!
//! Everything that touches the pictures and the encoder runs on one serial dispatch queue: the
//! capture callbacks arrive there, and [`FrameSource::kick`] only posts to it. ScreenCaptureKit
//! delivers a picture only when the screen changed, so the last one is kept for keyframes asked
//! for while the screen is still.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use tracing::{debug, warn};

use super::encode::{Encoder, Output};
use crate::mirror::pace::{Encode, Gate};
use crate::mirror::source::{EncodeConfig, FrameOutput, FrameSource, SourceEvent};

/// `420v`: two planes, video range.
const PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"420v");
/// Pictures ScreenCaptureKit may keep waiting.
const QUEUE_DEPTH: isize = 3;
/// Time macOS gets to answer a request of ScreenCaptureKit.
const ANSWER: Duration = Duration::from_secs(5);
const STOP_ANSWER: Duration = Duration::from_secs(2);

/// A reference that is handed between threads. ScreenCaptureKit objects and pixel buffers may
/// be used from any thread; the bindings cannot say so.
struct Handoff<T>(T);

// SAFETY: see the type's comment; each value has one user at a time.
unsafe impl<T> Send for Handoff<T> {}

struct Pictures {
    encoder: Encoder,
    /// The last complete picture and when it was captured.
    last: Option<Handoff<(CFRetained<CVPixelBuffer>, Instant)>>,
}

struct Shared {
    gate: Gate,
    output: FrameOutput,
    stopped: AtomicBool,
    pictures: Mutex<Option<Pictures>>,
}

impl Shared {
    fn pictures(&self) -> MutexGuard<'_, Option<Pictures>> {
        self.pictures.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Encode the last picture as the gate said. On the capture queue.
    fn encode(&self, pictures: &mut Pictures, encode: Encode) {
        let Some(Handoff((buffer, captured))) = &pictures.last else {
            self.gate.dropped(encode.keyframe);
            return;
        };
        if let Err(e) = pictures.encoder.encode(buffer, encode.keyframe, *captured) {
            warn!("{e:#}");
            self.gate.dropped(encode.keyframe);
        }
    }

    fn captured(&self, buffer: CFRetained<CVPixelBuffer>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        if let Some(pictures) = self.pictures().as_mut() {
            pictures.last = Some(Handoff((buffer, Instant::now())));
            if let Some(encode) = self.gate.captured() {
                self.encode(pictures, encode);
            }
        }
    }

    fn poll(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        if let Some(pictures) = self.pictures().as_mut()
            && let Some(encode) = self.gate.poll()
        {
            self.encode(pictures, encode);
        }
    }
}

/// The status ScreenCaptureKit attached to a sample; only complete samples carry a picture.
unsafe fn is_complete(sample: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(attachments) = sample.sample_attachments_array(false) else {
            return false;
        };
        if attachments.count() == 0 {
            return false;
        }
        let Some(first) = attachments.value_at_index(0).cast::<CFDictionary>().as_ref() else {
            return false;
        };
        // `NSString` and `CFString` are the same object.
        let key: *const NSString = SCStreamFrameInfoStatus;
        first
            .value(key.cast::<CFString>().cast())
            .cast::<CFNumber>()
            .as_ref()
            .and_then(CFNumber::as_isize)
            .is_some_and(|status| status == SCFrameStatus::Complete.0)
    }
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the class has no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "HubBridgeStreamOutput"]
    #[ivars = Arc<Shared>]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            // SAFETY: a sample handed to us is valid for the duration of the call.
            unsafe {
                if kind != SCStreamOutputType::Screen || !sample.is_valid() || !is_complete(sample) {
                    return;
                }
                if let Some(buffer) = sample.image_buffer() {
                    self.ivars().captured(buffer);
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let shared = self.ivars();
            if !shared.stopped.load(Ordering::SeqCst) {
                (shared.output)(SourceEvent::Failed(error.localizedDescription().to_string()));
            }
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: the initializer of NSObject.
        unsafe { msg_send![super(this), init] }
    }
}

fn describe(error: *mut NSError) -> Option<String> {
    // SAFETY: a completion handler's error is null or valid during the call.
    unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string())
}

/// The main display as ScreenCaptureKit sees it.
fn main_display() -> Result<Retained<SCDisplay>> {
    let id = CGMainDisplayID();
    let (tx, rx) = mpsc::channel::<Result<Handoff<Retained<SCDisplay>>, String>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: the content is null or valid during the call.
        let found = match unsafe { content.as_ref() } {
            Some(content) => unsafe { content.displays() }
                .iter()
                .find(|d| unsafe { d.displayID() } == id)
                .map(Handoff)
                .ok_or_else(|| "the main display cannot be captured".to_string()),
            None => Err(describe(error).unwrap_or_else(|| "no content to capture".into())),
        };
        let _ = tx.send(found);
    });
    // SAFETY: the block matches the signature and is copied by the callee.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(ANSWER) {
        Ok(Ok(display)) => Ok(display.0),
        Ok(Err(why)) => Err(anyhow!("cannot capture the screen: {why}")),
        Err(_) => bail!("cannot capture the screen: macOS did not answer"),
    }
}

/// Run an asynchronous start or stop of a stream and wait for its answer.
fn wait_for(limit: Duration, call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(describe(error));
    });
    call(&handler);
    match rx.recv_timeout(limit) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(anyhow!(why)),
        Err(_) => Err(anyhow!("macOS did not answer")),
    }
}

struct Running {
    stream: Retained<SCStream>,
    /// The stream keeps only weak references to some of these in older systems.
    _output: Retained<StreamOutput>,
    queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
}

#[derive(Default)]
pub struct ScreenSource {
    running: Option<Handoff<Running>>,
}

impl ScreenSource {
    pub fn new() -> ScreenSource {
        ScreenSource::default()
    }

    fn post(&self, work: impl FnOnce(&Shared) + Send + 'static) {
        if let Some(Handoff(running)) = &self.running {
            let shared = Arc::clone(&running.shared);
            running.queue.exec_async(move || work(&shared));
        }
    }
}

impl FrameSource for ScreenSource {
    /// The main display's size in pixels.
    fn native_size(&mut self) -> Result<(u32, u32)> {
        // Also initializes CoreGraphics, which a command-line tool must do before any
        // ScreenCaptureKit call.
        let mode = CGDisplayCopyDisplayMode(CGMainDisplayID()).context("cannot read the main display's mode")?;
        let width = CGDisplayMode::pixel_width(Some(&mode));
        let height = CGDisplayMode::pixel_height(Some(&mode));
        Ok((u32::try_from(width)?, u32::try_from(height)?))
    }

    fn start(&mut self, config: EncodeConfig, gate: Gate, output: FrameOutput) -> Result<()> {
        if self.running.is_some() {
            bail!("the capture is already running");
        }
        let display = main_display()?;
        let (encoded, drops) = (Arc::clone(&output), gate.clone());
        let encoder = Encoder::new(config, move |result| match result {
            Output::Frame(frame) => encoded(SourceEvent::Frame(frame)),
            Output::Dropped { forced } => drops.dropped(forced),
        })?;
        let shared = Arc::new(Shared {
            gate,
            output,
            stopped: AtomicBool::new(false),
            pictures: Mutex::new(Some(Pictures { encoder, last: None })),
        });
        let delegate = StreamOutput::new(Arc::clone(&shared));
        let queue = DispatchQueue::new("com.demoz.hubbridge.capture", None);
        // SAFETY: plain ScreenCaptureKit setup; the objects outlive the calls, and the color
        // matrix is a constant.
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::<SCWindow>::new(),
            );
            let settings = SCStreamConfiguration::new();
            settings.setWidth(config.width as usize);
            settings.setHeight(config.height as usize);
            settings.setPixelFormat(PIXEL_FORMAT);
            settings.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
            settings.setMinimumFrameInterval(CMTime::new(1, config.fps as i32));
            settings.setQueueDepth(QUEUE_DEPTH);
            settings.setShowsCursor(true);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &settings,
                Some(ProtocolObject::from_ref(&*delegate)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*delegate),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("cannot receive the screen: {}", e.localizedDescription()))?;
            stream
        };
        // SAFETY: the block matches the signature.
        wait_for(ANSWER, |done| unsafe {
            stream.startCaptureWithCompletionHandler(Some(done))
        })
        .context("cannot start the screen capture")?;
        self.running = Some(Handoff(Running {
            stream,
            _output: delegate,
            queue,
            shared,
        }));
        Ok(())
    }

    fn kick(&mut self) {
        self.post(Shared::poll);
    }

    fn request_keyframe(&mut self) {
        self.post(|shared| {
            shared.gate.want_keyframe();
            shared.poll();
        });
    }

    fn stop(&mut self) {
        let Some(Handoff(running)) = self.running.take() else {
            return;
        };
        running.shared.stopped.store(true, Ordering::SeqCst);
        // SAFETY: the block matches the signature.
        if let Err(e) = wait_for(STOP_ANSWER, |done| unsafe {
            running.stream.stopCaptureWithCompletionHandler(Some(done))
        }) {
            debug!("stopping the capture: {e:#}");
        }
        // After the callbacks already queued: release the pictures and the encoder.
        let shared = Arc::clone(&running.shared);
        running.queue.exec_sync(move || {
            let pictures = shared.pictures().take();
            drop(pictures);
        });
    }
}

impl Drop for ScreenSource {
    fn drop(&mut self) {
        self.stop();
    }
}
