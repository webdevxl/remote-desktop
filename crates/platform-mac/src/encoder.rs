//! Hardware video encoder (VideoToolbox) configured for interactive latency: low-latency rate
//! control, no frame reordering, keyframes only on request.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use objc2_core_foundation::{CFArray, CFBoolean, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMVideoCodecType_H264,
    kCMVideoCodecType_HEVC,
};
use objc2_core_video::CVPixelBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};
use protocol::Codec;

use crate::nal;
use crate::util::{check, dict};

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Preferred codec; the encoder falls back to H.264 if it can't be created.
    pub codec: Codec,
}

pub struct EncodedFrame {
    pub codec: Codec,
    /// Length-prefixed NAL units.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Parameter sets, filled on keyframes only.
    pub param_sets: Vec<Vec<u8>>,
    pub nal_length_size: u8,
    pub capture_time_us: u64,
}

type OutputFn = dyn Fn(EncodedFrame) + Send + Sync;

struct Ctx {
    codec: Codec,
    on_output: Box<OutputFn>,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    ctx: *mut Ctx,
    codec: Codec,
    low_latency: bool,
    last_pts_us: AtomicU64,
}

// SAFETY: VTCompressionSession is thread-safe for encode calls; `ctx` is only read by callbacks
// and freed in Drop after the session is invalidated.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    pub fn new(cfg: &EncoderConfig, on_output: impl Fn(EncodedFrame) + Send + Sync + 'static) -> Result<Self> {
        let mut on_output: Option<Box<OutputFn>> = Some(Box::new(on_output));
        let attempts = [(cfg.codec, true), (Codec::H264, true), (Codec::H264, false)];
        let mut last_err = None;
        for (codec, low_latency) in attempts {
            let ctx = Box::into_raw(Box::new(Ctx { codec, on_output: on_output.take().expect("set") }));
            match create_session(cfg, codec, low_latency, ctx) {
                Ok(session) => {
                    tracing::info!(?codec, low_latency, cfg.width, cfg.height, cfg.bitrate_bps, "encoder ready");
                    return Ok(Self { session, ctx, codec, low_latency, last_pts_us: AtomicU64::new(0) });
                }
                Err(e) => {
                    tracing::warn!(?codec, low_latency, "encoder unavailable: {e:#}");
                    // SAFETY: no session was created, so nothing else references `ctx`.
                    on_output = Some(unsafe { Box::from_raw(ctx) }.on_output);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.expect("at least one attempt"))
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    /// Submits a frame. Output arrives on the callback, usually within a few milliseconds.
    pub fn encode(&self, pixel_buffer: &CVPixelBuffer, capture_time_us: u64, force_keyframe: bool) -> Result<()> {
        // Presentation timestamps must strictly increase, even when re-encoding a held frame.
        let prev = self.last_pts_us.load(Ordering::Relaxed);
        let pts_us = capture_time_us.max(prev + 1);
        self.last_pts_us.store(pts_us, Ordering::Relaxed);
        let pts = CMTime { value: pts_us as i64, timescale: 1_000_000, flags: CMTimeFlags::Valid, epoch: 0 };
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags::empty(), epoch: 0 };

        let props = force_keyframe.then(|| {
            dict(&[(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, CFBoolean::new(true).as_ref())])
        });
        let status = unsafe {
            self.session.encode_frame(
                pixel_buffer,
                pts,
                invalid,
                props.as_deref().map(|d| d.as_opaque()),
                capture_time_us as *mut c_void,
                ptr::null_mut(),
            )
        };
        check(status, "VTCompressionSessionEncodeFrame")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags::empty(), epoch: 0 };
            self.session.complete_frames(invalid);
            self.session.invalidate();
            // SAFETY: the session is invalidated, so no callback can still see `ctx`.
            drop(Box::from_raw(self.ctx));
        }
    }
}

fn codec_type(codec: Codec) -> CMVideoCodecType {
    match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
    }
}

fn create_session(cfg: &EncoderConfig, codec: Codec, low_latency: bool, ctx: *mut Ctx) -> Result<CFRetained<VTCompressionSession>> {
    let spec = low_latency.then(|| {
        dict(&[(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }, CFBoolean::new(true).as_ref())])
    });
    let mut raw: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            None,
            cfg.width as i32,
            cfg.height as i32,
            codec_type(codec),
            spec.as_deref().map(|d| d.as_opaque()),
            None,
            None,
            Some(output_callback),
            ctx.cast(),
            NonNull::from(&mut raw),
        )
    };
    check(status, "VTCompressionSessionCreate")?;
    let session = unsafe { CFRetained::from_raw(NonNull::new(raw).context("null session")?) };
    // SAFETY: a VTCompressionSession is a VTSession.
    let vt: &VTSession = unsafe { &*(&*session as *const VTCompressionSession as *const VTSession) };

    let set = |key: &CFString, value: &CFType, required: bool| -> Result<()> {
        let status = unsafe { VTSessionSetProperty(vt, key, Some(value)) };
        if status != 0 {
            if required {
                bail!("set {key} failed (OSStatus {status})");
            }
            tracing::debug!("optional encoder property {key} not supported ({status})");
        }
        Ok(())
    };

    unsafe {
        set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true).as_ref(), true)?;
        set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false).as_ref(), true)?;
        let profile = match (codec, low_latency) {
            (Codec::Hevc, _) => kVTProfileLevel_HEVC_Main_AutoLevel,
            (Codec::H264, true) => kVTProfileLevel_H264_ConstrainedHigh_AutoLevel,
            (Codec::H264, false) => kVTProfileLevel_H264_High_AutoLevel,
        };
        set(kVTCompressionPropertyKey_ProfileLevel, profile.as_ref(), false)?;
        set(kVTCompressionPropertyKey_AverageBitRate, CFNumber::new_i32(cfg.bitrate_bps as i32).as_ref(), true)?;
        set(kVTCompressionPropertyKey_ExpectedFrameRate, CFNumber::new_i32(cfg.fps as i32).as_ref(), false)?;
        set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, CFBoolean::new(true).as_ref(), false)?;
        // Hard cap: at most 2x the average over any one-second window.
        let bytes = CFNumber::new_i64(i64::from(cfg.bitrate_bps) / 8 * 2);
        let seconds = CFNumber::new_f64(1.0);
        let limits = CFArray::from_objects(&[&*bytes, &*seconds]);
        set(kVTCompressionPropertyKey_DataRateLimits, limits.as_ref(), false)?;
        if !low_latency {
            // Low-latency mode already uses an infinite GOP; otherwise keep keyframes rare.
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, CFNumber::new_i32((cfg.fps * 600) as i32).as_ref(), false)?;
        }
        check(session.prepare_to_encode_frames(), "VTCompressionSessionPrepareToEncodeFrames")?;
    }
    Ok(session)
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    source_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let ctx = unsafe { &*(refcon as *const Ctx) };
    if status != 0 {
        tracing::warn!("encode failed (OSStatus {status})");
        return;
    }
    if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        return;
    }
    let Some(sample) = (unsafe { sample.as_ref() }) else { return };
    match encoded_frame(ctx.codec, sample, source_refcon as u64) {
        Ok(frame) => (ctx.on_output)(frame),
        Err(e) => tracing::warn!("read encoded frame: {e:#}"),
    }
}

fn encoded_frame(codec: Codec, sample: &CMSampleBuffer, capture_time_us: u64) -> Result<EncodedFrame> {
    let block = unsafe { sample.data_buffer() }.context("no data buffer")?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    check(
        unsafe { block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr()).context("buffer")?.cast()) },
        "CMBlockBufferCopyDataBytes",
    )?;
    let format = unsafe { sample.format_description() }.context("no format description")?;
    let (param_sets, nal_length_size) = parameter_sets(codec, &format)?;
    let keyframe = nal::is_keyframe(codec, &data, nal_length_size as usize);
    Ok(EncodedFrame {
        codec,
        data,
        keyframe,
        param_sets: if keyframe { param_sets } else { Vec::new() },
        nal_length_size,
        capture_time_us,
    })
}

/// Reads VPS/SPS/PPS (or SPS/PPS) and the NAL length-prefix size from a format description.
pub(crate) fn parameter_sets(codec: Codec, format: &CMFormatDescription) -> Result<(Vec<Vec<u8>>, u8)> {
    let get = |index: usize, ptr: *mut *const u8, size: *mut usize, count: *mut usize, nal_len: *mut i32| unsafe {
        match codec {
            Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, index, ptr, size, count, nal_len),
            Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, index, ptr, size, count, nal_len),
        }
    };
    let mut count = 0usize;
    let mut nal_len = 0i32;
    check(get(0, ptr::null_mut(), ptr::null_mut(), &mut count, &mut nal_len), "get parameter set count")?;
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        let mut p: *const u8 = ptr::null();
        let mut size = 0usize;
        check(get(i, &mut p, &mut size, ptr::null_mut(), ptr::null_mut()), "get parameter set")?;
        sets.push(unsafe { std::slice::from_raw_parts(p, size) }.to_vec());
    }
    Ok((sets, nal_len as u8))
}
