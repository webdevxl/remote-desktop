//! Screen capture with ScreenCaptureKit. Frames arrive as IOSurface-backed NV12 pixel buffers
//! that go straight to the hardware encoder without a CPU copy.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{CVPixelBuffer, kCVImageBufferYCbCrMatrix_ITU_R_709_2};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoDisplayTime, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};

use crate::clock;

const PIXEL_FORMAT_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const SC_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a display that just appeared (a new virtual display) may take to be listed by
/// ScreenCaptureKit.
const LISTING_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub id: u32,
    /// Native size in pixels (points × backing scale).
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub show_cursor: bool,
}

pub struct CapturedFrame {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    /// Host clock (µs) when the frame was shown on the host display.
    pub capture_time_us: u64,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for CapturedFrame {}

/// The main display, with its native pixel size.
pub fn main_display() -> Result<DisplayInfo> {
    let content = shareable_content()?;
    let displays = unsafe { content.0.displays() };
    let main_id = CGMainDisplayID();
    let display = displays
        .iter()
        .find(|d| unsafe { d.displayID() } == main_id)
        .or_else(|| displays.firstObject())
        .context("no displays")?;
    let id = unsafe { display.displayID() };
    let (width, height) = native_pixel_size(id)
        .unwrap_or_else(|| unsafe { (display.width() as u32, display.height() as u32) });
    Ok(DisplayInfo { id, width, height })
}

/// The main display's id and pixel size without asking ScreenCaptureKit (which needs Screen
/// Recording permission). Enough to map input; not to capture.
pub fn main_display_bounds() -> DisplayInfo {
    let id = CGMainDisplayID();
    let (width, height) = native_pixel_size(id).unwrap_or((1920, 1080));
    DisplayInfo { id, width, height }
}

/// The display `id` with its native pixel size, once ScreenCaptureKit lists it: a display that
/// just appeared can take a moment.
pub fn display(id: u32) -> Result<DisplayInfo> {
    let (_content, display) = find_display(id)?;
    let (width, height) = native_pixel_size(id).unwrap_or_else(|| unsafe { (display.width() as u32, display.height() as u32) });
    Ok(DisplayInfo { id, width, height })
}

/// Looks `id` up among the displays ScreenCaptureKit can capture, asking again for a while if it
/// isn't there yet.
fn find_display(id: u32) -> Result<(SendRetained<SCShareableContent>, Retained<SCDisplay>)> {
    let deadline = std::time::Instant::now() + LISTING_TIMEOUT;
    loop {
        let content = shareable_content()?;
        let displays = unsafe { content.0.displays() };
        if let Some(display) = displays.iter().find(|d| unsafe { d.displayID() } == id) {
            return Ok((content, display));
        }
        if std::time::Instant::now() >= deadline {
            bail!("display {id} not found");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn native_pixel_size(display_id: u32) -> Option<(u32, u32)> {
    let mode = CGDisplayCopyDisplayMode(display_id)?;
    let w = CGDisplayMode::pixel_width(Some(&mode));
    let h = CGDisplayMode::pixel_height(Some(&mode));
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

struct SendRetained<T>(Retained<T>);
// SAFETY: only used to hand ScreenCaptureKit results from its completion queue to the caller.
unsafe impl<T> Send for SendRetained<T> {}

fn shareable_content() -> Result<SendRetained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = match unsafe { Retained::retain(content) } {
            Some(content) => Ok(SendRetained(content)),
            None => Err(error_text(error)),
        };
        let _ = tx.send(result);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(SC_TIMEOUT)
        .context("ScreenCaptureKit did not answer")?
        .map_err(|e| anyhow!("list displays: {e} (is Screen Recording permission granted?)"))
}

fn error_text(error: *mut NSError) -> String {
    match unsafe { error.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "unknown error".into(),
    }
}

/// Runs an SCStream start/stop call and waits for its completion handler.
fn wait_completion(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(if error.is_null() { Ok(()) } else { Err(error_text(error)) });
    });
    call(&block);
    rx.recv_timeout(SC_TIMEOUT).context("ScreenCaptureKit did not answer")?.map_err(|e| anyhow!(e))
}

type FrameFn = dyn Fn(CapturedFrame) + Send + Sync;

struct OutputIvars {
    on_frame: Box<FrameFn>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "LanKvmStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            if let Some(frame) = captured_frame(sample) {
                (self.ivars().on_frame)(frame);
            }
        }
    }
);

impl StreamOutput {
    fn new(on_frame: Box<FrameFn>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { on_frame });
        unsafe { msg_send![super(this), init] }
    }
}

type StoppedFn = dyn Fn(String) + Send + Sync;

struct DelegateIvars {
    on_stopped: Box<StoppedFn>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "LanKvmStreamDelegate"]
    #[ivars = DelegateIvars]
    struct StreamDelegate;

    unsafe impl NSObjectProtocol for StreamDelegate {}

    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            (self.ivars().on_stopped)(error.localizedDescription().to_string());
        }
    }
);

impl StreamDelegate {
    fn new(on_stopped: Box<StoppedFn>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { on_stopped });
        unsafe { msg_send![super(this), init] }
    }
}

/// Extracts the pixel buffer and timing from a complete frame; idle frames (nothing changed on
/// screen) carry no new pixels and are skipped.
fn captured_frame(sample: &CMSampleBuffer) -> Option<CapturedFrame> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: CFArray of CFDictionary is toll-free bridged to NSArray of NSDictionary.
    let attachments: &NSArray<NSDictionary<NSString, NSNumber>> =
        unsafe { &*(&*attachments as *const _ as *const NSArray<NSDictionary<NSString, NSNumber>>) };
    let info = attachments.firstObject()?;
    let status = info.objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    if status.integerValue() != SCFrameStatus::Complete.0 {
        return None;
    }
    let capture_time_us = info
        .objectForKey(unsafe { SCStreamFrameInfoDisplayTime })
        .map(|t| clock::mach_to_us(t.unsignedLongLongValue()))
        .unwrap_or_else(clock::now_us);
    let pixel_buffer = unsafe { sample.image_buffer() }?;
    Some(CapturedFrame { pixel_buffer, capture_time_us })
}

pub struct Capturer {
    stream: Retained<SCStream>,
    /// Kept to change settings later: `updateConfiguration` replaces the whole configuration,
    /// and a fresh one would silently fall back to 1080p video-range frames.
    config: Retained<SCStreamConfiguration>,
    _output: Retained<StreamOutput>,
    _delegate: Retained<StreamDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: SCStream is documented as usable from any thread; we only start/stop it.
unsafe impl Send for Capturer {}
unsafe impl Sync for Capturer {}

impl Capturer {
    /// `on_stopped` is told if ScreenCaptureKit stops the capture by itself (the display went away
    /// or changed under it...), with the reason.
    pub fn start(
        cfg: &CaptureConfig,
        on_frame: impl Fn(CapturedFrame) + Send + Sync + 'static,
        on_stopped: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            bail!("invalid capture config {cfg:?}");
        }
        let (_content, display) = find_display(cfg.display_id)?;

        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(cfg.width as usize);
            config.setHeight(cfg.height as usize);
            // Full-range NV12: the encoder's native input, no conversion pass.
            config.setPixelFormat(PIXEL_FORMAT_NV12_FULL);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: cfg.fps as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            // One buffer is held back for keyframe re-encodes; keep slack for the pipeline.
            config.setQueueDepth(5);
            config.setShowsCursor(cfg.show_cursor);
            // `colorMatrix` is an unretained (assign) property, so it needs a CFString that
            // outlives the config; SCStream copies it later.
            config.setColorMatrix(kCVImageBufferYCbCrMatrix_ITU_R_709_2);

            let delegate = StreamDelegate::new(Box::new(on_stopped));
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*delegate)),
            );
            let output = StreamOutput::new(Box::new(on_frame));
            let queue = DispatchQueue::new("lankvm.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("add stream output: {}", e.localizedDescription()))?;
            wait_completion(|block| stream.startCaptureWithCompletionHandler(Some(block)))
                .context("start capture")?;
            Ok(Self { stream, config, _output: output, _delegate: delegate, _queue: queue })
        }
    }

    /// Draws the cursor into the frames or leaves it out, without restarting capture. Blocks
    /// until ScreenCaptureKit applies it, so call it off the async runtime.
    pub fn set_shows_cursor(&self, show: bool) -> Result<()> {
        unsafe {
            if self.config.showsCursor() == show {
                return Ok(());
            }
            self.config.setShowsCursor(show);
            let (stream, config) = (&self.stream, &self.config);
            wait_completion(|block| stream.updateConfiguration_completionHandler(config, Some(block)))
                .context("update capture configuration")
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        let stream = &self.stream;
        if let Err(e) = wait_completion(|block| unsafe { stream.stopCaptureWithCompletionHandler(Some(block)) }) {
            tracing::warn!("stop capture: {e:#}");
        }
    }
}
