//! Zero-copy import of decoded NV12 frames into wgpu: each IOSurface plane becomes a Metal
//! texture that wgpu samples directly.

use anyhow::{Context, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidthOfPlane,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
use wgpu::hal::api::Metal;

/// A decoded frame as two GPU textures. Holds the pixel buffer so the decoder's pool can't
/// recycle the IOSurface while the GPU may still be reading it.
pub struct Nv12Frame {
    pub y: wgpu::Texture,
    pub uv: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    _pixel_buffer: CFRetained<CVPixelBuffer>,
}

pub fn import_nv12(device: &wgpu::Device, pixel_buffer: CFRetained<CVPixelBuffer>) -> Result<Nv12Frame> {
    anyhow::ensure!(CVPixelBufferGetPlaneCount(&pixel_buffer) == 2, "expected a two-plane NV12 buffer");
    let surface = CVPixelBufferGetIOSurface(Some(&pixel_buffer)).context("pixel buffer has no IOSurface")?;
    let hal = unsafe { device.as_hal::<Metal>() }.context("wgpu device is not Metal")?;
    let mtl = hal.raw_device();

    let plane = |index: usize, mtl_format: MTLPixelFormat, format: wgpu::TextureFormat| -> Result<wgpu::Texture> {
        let width = CVPixelBufferGetWidthOfPlane(&pixel_buffer, index);
        let height = CVPixelBufferGetHeightOfPlane(&pixel_buffer, index);
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(mtl_format, width, height, false)
        };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        let raw = mtl
            .newTextureWithDescriptor_iosurface_plane(&desc, &surface, index)
            .context("create Metal texture from IOSurface")?;
        let size = wgpu::Extent3d { width: width as u32, height: height as u32, depth_or_array_layers: 1 };
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent { width: size.width, height: size.height, depth: 1 },
                None,
            )
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("nv12 plane"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        Ok(unsafe { device.create_texture_from_hal::<Metal>(hal_texture, &desc, wgpu::TextureUses::RESOURCE) })
    };

    let y = plane(0, MTLPixelFormat::R8Unorm, wgpu::TextureFormat::R8Unorm)?;
    let uv = plane(1, MTLPixelFormat::RG8Unorm, wgpu::TextureFormat::Rg8Unorm)?;
    let (width, height) = (y.width(), y.height());
    drop(hal);
    Ok(Nv12Frame { y, uv, width, height, _pixel_buffer: pixel_buffer })
}

/// Mean brightness (0-255) of a region of an NV12 frame's luma plane, for tests that watch the
/// video for a change. The region is normalized (0...1, top-left origin). Reads the frame on the
/// CPU, so only for diagnostics.
/// A decoded frame's size in pixels.
pub fn frame_size(pixel_buffer: &objc2_core_video::CVPixelBuffer) -> (u32, u32) {
    use objc2_core_video::{CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane};
    (CVPixelBufferGetWidthOfPlane(pixel_buffer, 0) as u32, CVPixelBufferGetHeightOfPlane(pixel_buffer, 0) as u32)
}

pub fn mean_luma(pixel_buffer: &objc2_core_video::CVPixelBuffer, x0: f64, y0: f64, x1: f64, y1: f64) -> Option<f64> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
        CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
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
