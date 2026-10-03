//! Zero-copy access to decoded NV12 frames on the GPU: each IOSurface plane becomes a Metal
//! texture sharing its memory. Plus CPU helpers that read frames for tests.

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidthOfPlane,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage};

/// The planes of an IOSurface-backed NV12 pixel buffer as textures: `R8Unorm` luma and
/// `RG8Unorm` chroma (half size). They don't keep the pixel buffer itself alive: hold it until the
/// GPU is done with them, or its pool may reuse the surface for a newer frame meanwhile.
pub fn nv12_planes(
    device: &ProtocolObject<dyn MTLDevice>,
    pixel_buffer: &CVPixelBuffer,
) -> Result<[Retained<ProtocolObject<dyn MTLTexture>>; 2]> {
    anyhow::ensure!(CVPixelBufferGetPlaneCount(pixel_buffer) == 2, "expected a two-plane NV12 buffer");
    let surface = CVPixelBufferGetIOSurface(Some(pixel_buffer)).context("pixel buffer has no IOSurface")?;
    let plane = |index: usize, format: MTLPixelFormat| {
        let width = CVPixelBufferGetWidthOfPlane(pixel_buffer, index);
        let height = CVPixelBufferGetHeightOfPlane(pixel_buffer, index);
        anyhow::ensure!(width > 0 && height > 0, "empty plane {index}");
        // SAFETY: plain descriptor constructor; the format, size and mip flag are valid.
        let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, width, height, false) };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        device.newTextureWithDescriptor_iosurface_plane(&desc, &surface, index).context("create Metal texture from IOSurface")
    };
    Ok([plane(0, MTLPixelFormat::R8Unorm)?, plane(1, MTLPixelFormat::RG8Unorm)?])
}

/// A decoded frame's size in pixels.
pub fn frame_size(pixel_buffer: &CVPixelBuffer) -> (u32, u32) {
    (CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) as u32, CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) as u32)
}

/// Mean brightness (0-255) of a region of an NV12 frame's luma plane, for tests that watch the
/// video for a change. The region is normalized (0...1, top-left origin). Reads the frame on the
/// CPU, so only for diagnostics.
pub fn mean_luma(pixel_buffer: &CVPixelBuffer, x0: f64, y0: f64, x1: f64, y1: f64) -> Option<f64> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    let read_only = CVPixelBufferLockFlags::ReadOnly;
    unsafe {
        if CVPixelBufferLockBaseAddress(pixel_buffer, read_only) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0) as *const u8;
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0);
        let (w, h) = (CVPixelBufferGetWidthOfPlane(pixel_buffer, 0), CVPixelBufferGetHeightOfPlane(pixel_buffer, 0));
        let mut result = None;
        if !base.is_null() && w > 0 && h > 0 {
            let px = |v: f64, n: usize| ((v.clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
            let (ax, bx, ay, by) = (px(x0, w), px(x1, w), px(y0, h), px(y1, h));
            let (mut sum, mut count) = (0u64, 0u64);
            for y in (ay..=by).step_by(2) {
                let row = base.add(y * stride);
                for x in (ax..=bx).step_by(2) {
                    sum += u64::from(*row.add(x));
                    count += 1;
                }
            }
            if count > 0 {
                result = Some(sum as f64 / count as f64);
            }
        }
        CVPixelBufferUnlockBaseAddress(pixel_buffer, read_only);
        result
    }
}
