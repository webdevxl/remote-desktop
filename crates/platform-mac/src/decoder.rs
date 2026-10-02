//! Hardware video decoder (VideoToolbox). Output frames are IOSurface-backed, Metal-compatible
//! NV12 pixel buffers, so the renderer can sample them without copying.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

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

pub struct DecodedFrame {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    /// The tag passed to [`Decoder::decode`].
    pub tag: u64,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for DecodedFrame {}

type FrameFn = dyn Fn(DecodedFrame) + Send + Sync;

pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    ctx: *mut Box<FrameFn>,
    codec: Codec,
    param_sets: Vec<Vec<u8>>,
}

// SAFETY: decode calls are serialized by the owner; `ctx` is freed only after invalidation.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(
        codec: Codec,
        param_sets: &[Vec<u8>],
        nal_length_size: u8,
        on_frame: impl Fn(DecodedFrame) + Send + Sync + 'static,
    ) -> Result<Self> {
        let format = format_description(codec, param_sets, nal_length_size)?;
        let ctx: *mut Box<FrameFn> = Box::into_raw(Box::new(Box::new(on_frame)));

        let surface_props: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
        let attrs = unsafe {
            dict(&[
                (kCVPixelBufferPixelFormatTypeKey, CFNumber::new_i32(PIXEL_FORMAT_NV12_FULL).as_ref()),
                (kCVPixelBufferIOSurfacePropertiesKey, surface_props.as_ref()),
                (kCVPixelBufferMetalCompatibilityKey, CFBoolean::new(true).as_ref()),
            ])
        };
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
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

    /// Decodes one access unit synchronously; the callback runs before this returns.
    pub fn decode(&self, data: &[u8], tag: u64) -> Result<()> {
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

        let mut info = VTDecodeInfoFlags::empty();
        check(
            unsafe {
                self.session.decode_frame(
                    &sample,
                    VTDecodeFrameFlags::Frame_1xRealTimePlayback,
                    tag as *mut c_void,
                    &mut info,
                )
            },
            "VTDecompressionSessionDecodeFrame",
        )
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
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

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    if status != 0 {
        tracing::warn!("decode failed (OSStatus {status})");
        return;
    }
    let Some(image) = NonNull::new(image) else { return };
    let on_frame = unsafe { &*(refcon as *const Box<FrameFn>) };
    // SAFETY: VideoToolbox hands us a borrowed, live image buffer; retaining keeps it alive.
    let pixel_buffer = unsafe { CFRetained::retain(image) };
    on_frame(DecodedFrame { pixel_buffer, tag: source_refcon as u64 });
}
