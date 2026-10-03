//! Hardware video decoder (VideoToolbox). Output frames are IOSurface-backed, Metal-compatible
//! NV12 pixel buffers, so the renderer can sample them without copying.
//!
//! Decoding is asynchronous: [`Decoder::decode`] only queues the frame, so the network task never
//! waits for the hardware and the decoders of several tiles run in parallel.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use anyhow::{Context, Result};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSession, VTSessionSetProperty, kVTDecompressionPropertyKey_RealTime,
};
use protocol::Codec;

use crate::util::{check, dict};

const PIXEL_FORMAT_NV12_FULL: i32 = i32::from_be_bytes(*b"420f");

/// What came out of the decoder for one submitted frame.
pub struct DecodedFrame<T = u64> {
    /// The picture, or the OSStatus saying why there is none (0 if the decoder dropped it).
    /// After a failure the following frames reference a broken picture: ask for a keyframe.
    pub image: Result<CFRetained<CVPixelBuffer>, i32>,
    /// The tag passed to [`Decoder::decode`] with this frame.
    pub tag: T,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl<T: Send> Send for DecodedFrame<T> {}

type FrameFn<T> = dyn Fn(DecodedFrame<T>) + Send + Sync;

/// Frames inside the decoder, in decode order, matched to their output by `id` (the
/// `sourceFrameRefCon`), so any tag type can travel with them.
struct InFlight<T> {
    next_id: u64,
    frames: VecDeque<(u64, T)>,
}

struct Ctx<T> {
    on_frame: Box<FrameFn<T>>,
    in_flight: Mutex<InFlight<T>>,
}

impl<T> Ctx<T> {
    /// Takes a frame's tag out of the decoder. None if it already was, so a frame is never
    /// finished twice whichever of the callback and a failed submit gets here first.
    fn finish(&self, id: u64) -> Option<T> {
        let mut in_flight = self.in_flight.lock().unwrap();
        // Output comes in decode order, so this is almost always the front.
        let index = in_flight.frames.iter().position(|(i, _)| *i == id)?;
        in_flight.frames.remove(index).map(|(_, tag)| tag)
    }
}

pub struct Decoder<T = u64> {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    ctx: *mut Ctx<T>,
    codec: Codec,
    param_sets: Vec<Vec<u8>>,
}

// SAFETY: decode calls are serialized by the owner (`&self` but not `Sync`); `ctx` is shared with
// the callbacks only through its mutex and freed only after invalidation.
unsafe impl<T: Send> Send for Decoder<T> {}

impl<T: Send + 'static> Decoder<T> {
    /// `on_frame` gets the result of every frame [`Decoder::decode`] accepted, on a VideoToolbox
    /// thread, in decode order (see `decode` for a frame it refused).
    pub fn new(
        codec: Codec,
        param_sets: &[Vec<u8>],
        nal_length_size: u8,
        on_frame: impl Fn(DecodedFrame<T>) + Send + Sync + 'static,
    ) -> Result<Self> {
        let format = format_description(codec, param_sets, nal_length_size)?;
        let ctx = Box::into_raw(Box::new(Ctx {
            on_frame: Box::new(on_frame) as Box<FrameFn<T>>,
            in_flight: Mutex::new(InFlight { next_id: 0, frames: VecDeque::new() }),
        }));

        let surface_props: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
        let attrs = unsafe {
            dict(&[
                (kCVPixelBufferPixelFormatTypeKey, CFNumber::new_i32(PIXEL_FORMAT_NV12_FULL).as_ref()),
                (kCVPixelBufferIOSurfacePropertiesKey, surface_props.as_ref()),
                (kCVPixelBufferMetalCompatibilityKey, CFBoolean::new(true).as_ref()),
            ])
        };
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback::<T>),
            decompressionOutputRefCon: ctx.cast(),
        };
        let mut raw: *mut VTDecompressionSession = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(attrs.as_opaque()),
                &record,
                NonNull::from(&mut raw),
            )
        };
        if let Err(e) = check(status, "VTDecompressionSessionCreate") {
            // SAFETY: no session holds `ctx`.
            drop(unsafe { Box::from_raw(ctx) });
            return Err(e);
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(raw).context("null session")?) };
        let vt: &VTSession = unsafe { &*(&*session as *const VTDecompressionSession as *const VTSession) };
        unsafe { VTSessionSetProperty(vt, kVTDecompressionPropertyKey_RealTime, Some(CFBoolean::new(true).as_ref())) };

        Ok(Self { session, format, ctx, codec, param_sets: param_sets.to_vec() })
    }

    /// Whether this decoder was built for the given stream parameters.
    pub fn matches(&self, codec: Codec, param_sets: &[Vec<u8>]) -> bool {
        self.codec == codec && self.param_sets == param_sets
    }

    fn ctx(&self) -> &Ctx<T> {
        // SAFETY: `ctx` lives until Drop.
        unsafe { &*self.ctx }
    }

    /// Queues one access unit and returns without waiting for it: the result reaches the
    /// callback with `tag` once decoded. The callback sees `tag` at most once. If this returns an
    /// error, it may already have: VideoToolbox can hand a frame's failure to the callback before
    /// `DecodeFrame` returns it, so treat both as the same failure.
    pub fn decode(&self, data: &[u8], tag: T) -> Result<()> {
        let len = data.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        check(
            unsafe {
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    len,
                    None,
                    ptr::null(),
                    0,
                    len,
                    kCMBlockBufferAssureMemoryNowFlag,
                    NonNull::from(&mut block),
                )
            },
            "CMBlockBufferCreateWithMemoryBlock",
        )?;
        let block = unsafe { CFRetained::from_raw(NonNull::new(block).context("null block")?) };
        check(
            unsafe { CMBlockBuffer::replace_data_bytes(NonNull::new(data.as_ptr() as *mut c_void).context("data")?, &block, 0, len) },
            "CMBlockBufferReplaceDataBytes",
        )?;

        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        check(
            unsafe {
                CMSampleBuffer::create_ready(
                    None,
                    Some(&block),
                    Some(&self.format),
                    1,
                    0,
                    ptr::null(),
                    1,
                    &len,
                    NonNull::from(&mut sample),
                )
            },
            "CMSampleBufferCreateReady",
        )?;
        let sample = unsafe { CFRetained::from_raw(NonNull::new(sample).context("null sample")?) };

        let id = {
            let mut in_flight = self.ctx().in_flight.lock().unwrap();
            let id = in_flight.next_id;
            in_flight.next_id = id.wrapping_add(1);
            in_flight.frames.push_back((id, tag));
            id
        };
        let mut info = VTDecodeInfoFlags::empty();
        // Not under the lock: VideoToolbox may still run the callback on this thread.
        let status = unsafe {
            // No `1xRealTimePlayback` hint: it allows a low-power mode that decodes no faster
            // than the frame rate, which adds latency to every frame.
            self.session.decode_frame(
                &sample,
                VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression,
                id as *mut c_void,
                &mut info,
            )
        };
        if status != 0 {
            // Unless the callback already had it.
            self.ctx().finish(id);
        }
        check(status, "VTDecompressionSessionDecodeFrame")
    }
}

impl<T> Drop for Decoder<T> {
    fn drop(&mut self) {
        unsafe {
            // Every queued frame reaches the callback before this returns.
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
            // SAFETY: the session is invalidated, so no callback can still see `ctx`.
            drop(Box::from_raw(self.ctx));
        }
    }
}

fn format_description(codec: Codec, param_sets: &[Vec<u8>], nal_length_size: u8) -> Result<CFRetained<CMFormatDescription>> {
    anyhow::ensure!(!param_sets.is_empty(), "missing parameter sets");
    let ptrs: Vec<NonNull<u8>> = param_sets
        .iter()
        .map(|s| NonNull::new(s.as_ptr() as *mut u8).context("empty parameter set"))
        .collect::<Result<_>>()?;
    let sizes: Vec<usize> = param_sets.iter().map(Vec::len).collect();
    let mut out: *const CMFormatDescription = ptr::null();
    let status = unsafe {
        let ptrs = NonNull::new(ptrs.as_ptr() as *mut NonNull<u8>).context("ptrs")?;
        let sizes = NonNull::new(sizes.as_ptr() as *mut usize).context("sizes")?;
        match codec {
            Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None, param_sets.len(), ptrs, sizes, i32::from(nal_length_size), None, NonNull::from(&mut out),
            ),
            Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None, param_sets.len(), ptrs, sizes, i32::from(nal_length_size), NonNull::from(&mut out),
            ),
        }
    };
    check(status, "create video format description")?;
    Ok(unsafe { CFRetained::from_raw(NonNull::new(out as *mut CMFormatDescription).context("null format")?) })
}

unsafe extern "C-unwind" fn output_callback<T>(
    refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let ctx = unsafe { &*(refcon as *const Ctx<T>) };
    let Some(tag) = ctx.finish(source_refcon as u64) else { return };
    let image = match NonNull::new(image) {
        // SAFETY: VideoToolbox hands us a borrowed, live image buffer; retaining keeps it alive
        // for as long as the renderer needs it, independent of the decoder's buffer pool.
        Some(image) if status == 0 => Ok(unsafe { CFRetained::retain(image) }),
        _ => {
            if status != 0 {
                tracing::warn!("decode failed (OSStatus {status})");
            } else {
                tracing::debug!(?flags, "decoder dropped a frame");
            }
            Err(status)
        }
    };
    (ctx.on_frame)(DecodedFrame { image, tag });
}
