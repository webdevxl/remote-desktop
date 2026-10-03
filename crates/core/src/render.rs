//! Draws decoded tiles with Metal: copies each tile's NV12 planes into a stream-sized canvas on
//! the GPU (the decoder's surfaces are read directly, no upload), then converts the canvas to RGB
//! in a shader, letterboxed to the target.

use std::ptr::NonNull;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBarrierScope, MTLClearColor, MTLComputeCommandEncoder, MTLComputePipelineState, MTLDispatchType, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice,
    MTLDevice, MTLFunction, MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSize, MTLStorageMode, MTLStoreAction,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage, MTLViewport,
};
use platform_mac::CVPixelBuffer;
use platform_mac::gpu;
use protocol::TileRect;

/// What the layer shows. Non-sRGB: decoded pixels are already display-encoded, so they pass
/// straight through (as with wgpu before).
pub const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

pub type Texture = Retained<ProtocolObject<dyn MTLTexture>>;

/// The Metal device and what every view shares: queue and pipelines.
pub struct Gpu {
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    pub queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Canvas → screen.
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// Tile plane → canvas.
    copy: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
}

pub fn gpu() -> Result<&'static Gpu> {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    if let Some(g) = GPU.get() {
        return Ok(g);
    }
    let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
    let queue = device.newCommandQueue().context("create Metal command queue")?;
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(include_str!("nv12.metal")), None)
        .map_err(|e| anyhow!("compile nv12.metal: {}", e.localizedDescription()))?;
    let function = |name: &str| -> Result<Retained<ProtocolObject<dyn MTLFunction>>> {
        library.newFunctionWithName(&NSString::from_str(name)).with_context(|| format!("no {name} in nv12.metal"))
    };
    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&*function("vs_main")?));
    desc.setFragmentFunction(Some(&*function("fs_main")?));
    // SAFETY: attachment 0 always exists.
    unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(TARGET_FORMAT);
    let pipeline = device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| anyhow!("create nv12 pipeline: {}", e.localizedDescription()))?;
    let copy = device
        .newComputePipelineStateWithFunction_error(&*function("copy_plane")?)
        .map_err(|e| anyhow!("create copy pipeline: {}", e.localizedDescription()))?;
    Ok(GPU.get_or_init(|| Gpu { device, queue, pipeline, copy }))
}

/// The stream's picture on the GPU: tiles are copied in as they arrive, and the screen is drawn
/// from it.
struct Canvas {
    y: Texture,
    uv: Texture,
    width: u32,
    height: u32,
}

/// One decoded image to copy into the canvas.
pub struct Part<'a> {
    pub pixel_buffer: &'a CVPixelBuffer,
    /// Where it goes in the stream.
    pub tile: TileRect,
    /// Places it must not overwrite because they already show something newer (a full frame that
    /// came late).
    pub keep: &'a [TileRect],
}

/// What [`VideoRenderer::apply`] encoded.
pub struct Applied {
    /// The textures the copies read: the caller keeps them and the pixel buffers alive until the
    /// command buffer completes.
    pub textures: Vec<Texture>,
    /// Tiles that couldn't be read ([`TileRect::bit`]): the canvas misses their content.
    pub failed: u64,
}

/// Keeps the canvas between frames. Shared by the views of a session (see `view.rs`), used by
/// one thread at a time.
#[derive(Default)]
pub struct VideoRenderer {
    canvas: Option<Canvas>,
}

// SAFETY: Metal textures may be used from any thread; the renderer is only ever used by one
// thread at a time (behind the slot's mutex).
unsafe impl Send for VideoRenderer {}

impl VideoRenderer {
    /// Encodes copies of `parts` into the canvas of a `stream`-sized picture, in order (a later
    /// part wins where they overlap), recreating the canvas, cleared to black, when the size
    /// changes. A tile that can't be read is skipped and reported; one outside the stream is
    /// clipped away. An error means the canvas is gone.
    pub fn apply<'a>(
        &mut self,
        gpu: &Gpu,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        stream: (u32, u32),
        parts: impl IntoIterator<Item = Part<'a>>,
    ) -> Result<Applied> {
        if self.canvas.as_ref().is_none_or(|c| (c.width, c.height) != stream) {
            self.canvas = None;
            self.canvas = Some(Canvas::new(gpu, cb, stream)?);
        }
        let canvas = self.canvas.as_ref().expect("canvas");
        let mut applied = Applied { textures: Vec::new(), failed: 0 };
        let mut compute: Option<Retained<ProtocolObject<dyn MTLComputeCommandEncoder>>> = None;
        // Places written since the last barrier.
        let mut written: Vec<TileRect> = Vec::new();
        for part in parts {
            let tile = part.tile;
            let [y, uv] = match gpu::nv12_planes(&gpu.device, part.pixel_buffer) {
                Ok(planes) => planes,
                Err(e) => {
                    tracing::warn!("tile {}: {e:#}", tile.index);
                    applied.failed |= tile.bit();
                    continue;
                }
            };
            // A compute kernel rather than blits: the tiles' copies run concurrently (they write
            // disjoint parts of the canvas), about 20% faster for a full 6144×2560 update.
            if compute.is_none() {
                let encoder = cb.computeCommandEncoderWithDispatchType(MTLDispatchType::Concurrent).context("no compute encoder")?;
                encoder.setComputePipelineState(&gpu.copy);
                compute = Some(encoder);
            }
            let encoder = compute.as_deref().expect("compute encoder");
            // Concurrent copies to the same place would race: the later one must win.
            if written.iter().any(|w| overlap(w, &tile)) {
                encoder.memoryBarrierWithScope(MTLBarrierScope::Textures);
                written.clear();
            }
            written.push(tile);
            let (x, w) = (tile.x as usize, tile.width as usize);
            let (top, h) = (tile.y as usize, tile.height as usize);
            let keep = |half: bool| -> Vec<[u32; 4]> {
                let scale = |v: u32| if half { v / 2 } else { v };
                let size = |v: u32| if half { v.div_ceil(2) } else { v };
                part.keep.iter().map(|k| [scale(k.x), scale(k.y), scale(k.x).saturating_add(size(k.width)), scale(k.y).saturating_add(size(k.height))]).collect()
            };
            copy_region(encoder, &gpu.copy, &y, &canvas.y, (x, top), (w, h), &keep(false));
            // Chroma is subsampled 2×2; odd sizes round up (the canvas does the same).
            copy_region(encoder, &gpu.copy, &uv, &canvas.uv, (x / 2, top / 2), (w.div_ceil(2), h.div_ceil(2)), &keep(true));
            applied.textures.extend([y, uv]);
        }
        if let Some(encoder) = compute {
            encoder.endEncoding();
        }
        Ok(applied)
    }

    /// Encodes drawing the canvas centered in `target` (any [`TARGET_FORMAT`] texture), aspect
    /// preserved, black around it, or all black before the first tile.
    pub fn draw(&self, gpu: &Gpu, cb: &ProtocolObject<dyn MTLCommandBuffer>, target: &ProtocolObject<dyn MTLTexture>) {
        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: attachment 0 always exists.
        let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color.setTexture(Some(target));
        color.setLoadAction(MTLLoadAction::Clear);
        color.setStoreAction(MTLStoreAction::Store);
        color.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 });
        let Some(encoder) = cb.renderCommandEncoderWithDescriptor(&pass) else {
            tracing::warn!("no render encoder");
            return;
        };
        if let Some(canvas) = &self.canvas {
            let (x, y, w, h) = letterbox(canvas.width, canvas.height, target.width() as u32, target.height() as u32);
            encoder.setViewport(MTLViewport {
                originX: x.into(),
                originY: y.into(),
                width: w.into(),
                height: h.into(),
                znear: 0.0,
                zfar: 1.0,
            });
            encoder.setRenderPipelineState(&gpu.pipeline);
            // SAFETY: indices 0 and 1 are the shader's texture slots; the canvas outlives the
            // command buffer's use of it (Metal retains bound textures).
            unsafe {
                encoder.setFragmentTexture_atIndex(Some(&canvas.y), 0);
                encoder.setFragmentTexture_atIndex(Some(&canvas.uv), 1);
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
            }
        }
        encoder.endEncoding();
    }
}

#[cfg(test)]
impl VideoRenderer {
    pub fn stream_size(&self) -> Option<(u32, u32)> {
        self.canvas.as_ref().map(|c| (c.width, c.height))
    }
}

impl Canvas {
    fn new(gpu: &Gpu, cb: &ProtocolObject<dyn MTLCommandBuffer>, (width, height): (u32, u32)) -> Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "empty stream {width}x{height}");
        let plane = |format, w: u32, h: u32, clear: f64| -> Result<Texture> {
            // SAFETY: plain descriptor constructor; the format and mip flag are valid.
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w as usize, h as usize, false)
            };
            desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite | MTLTextureUsage::RenderTarget);
            desc.setStorageMode(MTLStorageMode::Private);
            let texture = gpu.device.newTextureWithDescriptor(&desc).with_context(|| format!("create {w}x{h} canvas"))?;
            // Clear by a render pass that only loads with the clear color.
            let pass = MTLRenderPassDescriptor::renderPassDescriptor();
            // SAFETY: attachment 0 always exists.
            let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor { red: clear, green: clear, blue: 0.0, alpha: 1.0 });
            cb.renderCommandEncoderWithDescriptor(&pass).context("no render encoder")?.endEncoding();
            Ok(texture)
        };
        // Black: luma 0, chroma neutral.
        let y = plane(MTLPixelFormat::R8Unorm, width, height, 0.0)?;
        let uv = plane(MTLPixelFormat::RG8Unorm, width.div_ceil(2), height.div_ceil(2), 0.5)?;
        Ok(Self { y, uv, width, height })
    }
}

/// Copies `src` to `origin` in `dst`, clipped to `size` and to both textures, so a tile with an
/// unexpected size can't write outside its place or the canvas. Leaves `keep` (rects in `dst`
/// texels) alone.
fn copy_region(
    encoder: &ProtocolObject<dyn MTLComputeCommandEncoder>,
    pipeline: &ProtocolObject<dyn MTLComputePipelineState>,
    src: &ProtocolObject<dyn MTLTexture>,
    dst: &ProtocolObject<dyn MTLTexture>,
    (x, y): (usize, usize),
    (w, h): (usize, usize),
    keep: &[[u32; 4]],
) {
    let w = w.min(src.width()).min(dst.width().saturating_sub(x));
    let h = h.min(src.height()).min(dst.height().saturating_sub(y));
    if w == 0 || h == 0 {
        return;
    }
    let origin = [x as u32, y as u32];
    // `setBytes` takes at most 4 KB; there are fewer tiles than that anyway.
    let keep = &keep[..keep.len().min(protocol::MAX_TILES)];
    let count = keep.len() as u32;
    // The kernel's buffer must not be empty even when it reads none of it.
    let none = [[0u32; 4]];
    let rects = if keep.is_empty() { &none[..] } else { keep };
    let tw = pipeline.threadExecutionWidth();
    let th = (pipeline.maxTotalThreadsPerThreadgroup() / tw).clamp(1, 8);
    // SAFETY: slots 0 and 1 and buffers 0-2 are the kernel's; the bytes are copied right away and
    // `count` rects are there; the grid is the clipped size, so every write lies within `dst` and
    // every read within `src`. Both textures stay alive until the command buffer completes (Metal
    // retains them, and the caller holds them too).
    unsafe {
        encoder.setTexture_atIndex(Some(src), 0);
        encoder.setTexture_atIndex(Some(dst), 1);
        encoder.setBytes_length_atIndex(NonNull::from(&origin).cast(), 8, 0);
        encoder.setBytes_length_atIndex(NonNull::from(rects).cast(), std::mem::size_of_val(rects), 1);
        encoder.setBytes_length_atIndex(NonNull::from(&count).cast(), 4, 2);
        encoder.dispatchThreads_threadsPerThreadgroup(MTLSize { width: w, height: h, depth: 1 }, MTLSize { width: tw, height: th, depth: 1 });
    }
}

/// Whether two rects share a pixel.
fn overlap(a: &TileRect, b: &TileRect) -> bool {
    let (a_right, a_bottom) = (a.x.saturating_add(a.width), a.y.saturating_add(a.height));
    let (b_right, b_bottom) = (b.x.saturating_add(b.width), b.y.saturating_add(b.height));
    a.x < b_right && b.x < a_right && a.y < b_bottom && b.y < a_bottom
}

/// Largest rect with the frame's aspect ratio centered in the target. When the frame fits
/// exactly it's drawn 1:1, pixel-aligned, which keeps text sharp.
fn letterbox(fw: u32, fh: u32, tw: u32, th: u32) -> (f32, f32, f32, f32) {
    let scale = (tw as f32 / fw as f32).min(th as f32 / fh as f32);
    let (w, h) = ((fw as f32 * scale).round(), (fh as f32 * scale).round());
    (((tw as f32 - w) / 2.0).floor(), ((th as f32 - h) / 2.0).floor(), w, h)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::time::Instant;

    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    };
    use objc2_metal::{MTLOrigin, MTLRegion};

    use super::*;

    #[test]
    fn letterbox_fits_and_centers() {
        assert_eq!(letterbox(1920, 1080, 1920, 1080), (0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(letterbox(1920, 1080, 1920, 1200), (0.0, 60.0, 1920.0, 1080.0));
        assert_eq!(letterbox(1000, 1000, 2000, 1000), (500.0, 0.0, 1000.0, 1000.0));
    }

    /// An NV12 (full range) IOSurface buffer, like the decoder's, filled by `y(x, row)` and a
    /// constant chroma.
    pub(crate) fn nv12(width: usize, height: usize, y: impl Fn(usize, usize) -> u8, (cb, cr): (u8, u8)) -> CFRetained<CVPixelBuffer> {
        let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
        let yes = CFBoolean::new(true);
        let attrs: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }, unsafe { kCVPixelBufferMetalCompatibilityKey }],
            &[empty.as_ref(), yes.as_ref()],
        );
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(None, width, height, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
        };
        assert_eq!(status, 0);
        let pb = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(&pb, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, plane);
                let (w, h) = (CVPixelBufferGetWidthOfPlane(&pb, plane), CVPixelBufferGetHeightOfPlane(&pb, plane));
                for row in 0..h {
                    let line = std::slice::from_raw_parts_mut(base.add(row * stride), stride);
                    for col in 0..w {
                        if plane == 0 {
                            line[col] = y(col, row);
                        } else {
                            line[col * 2] = cb;
                            line[col * 2 + 1] = cr;
                        }
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        }
        pb
    }

    /// What the shader makes of one NV12 sample, as BGRA8 bytes.
    pub(crate) fn expected(y: u8, cb: u8, cr: u8) -> [f32; 3] {
        let y = f32::from(y) / 255.0;
        let (u, v) = (f32::from(cb) / 255.0 - 0.5, f32::from(cr) / 255.0 - 0.5);
        let rgb = [y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u];
        rgb.map(|c| c.clamp(0.0, 1.0) * 255.0)
    }

    pub(crate) struct Target {
        texture: Texture,
        width: usize,
    }

    impl Target {
        pub(crate) fn new(gpu: &Gpu, width: usize, height: usize) -> Self {
            let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(TARGET_FORMAT, width, height, false) };
            desc.setUsage(MTLTextureUsage::RenderTarget);
            desc.setStorageMode(MTLStorageMode::Shared);
            Self { texture: gpu.device.newTextureWithDescriptor(&desc).unwrap(), width }
        }

        /// RGB at (x, y).
        pub(crate) fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
            let mut bgra = [0u8; 4];
            let region = MTLRegion { origin: MTLOrigin { x, y, z: 0 }, size: MTLSize { width: 1, height: 1, depth: 1 } };
            unsafe {
                self.texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(NonNull::from(&mut bgra).cast::<c_void>(), 4 * self.width, region, 0)
            };
            [bgra[2], bgra[1], bgra[0]]
        }

        /// Draws the renderer's canvas, waiting for the GPU.
        pub(crate) fn show(&self, gpu: &Gpu, renderer: &VideoRenderer) {
            let cb = gpu.queue.commandBuffer().unwrap();
            renderer.draw(gpu, &cb, &self.texture);
            cb.commit();
            cb.waitUntilCompleted();
            assert!(cb.error().is_none(), "{:?}", cb.error());
        }
    }

    pub(crate) fn assert_near(got: [u8; 3], want: [f32; 3], at: &str) {
        let close = got.iter().zip(want).all(|(g, w)| (f32::from(*g) - w).abs() <= 1.5);
        assert!(close, "{at}: got {got:?}, want {want:?}");
    }

    /// Applies `tiles` to the canvas and draws it into `target`, waiting for the GPU. Returns the
    /// tiles that failed.
    fn render_keeping(
        gpu: &Gpu,
        renderer: &mut VideoRenderer,
        stream: (u32, u32),
        tiles: &[(&CVPixelBuffer, TileRect, &[TileRect])],
        target: &Target,
    ) -> u64 {
        let cb = gpu.queue.commandBuffer().unwrap();
        let parts = tiles.iter().map(|&(pixel_buffer, tile, keep)| Part { pixel_buffer, tile, keep });
        let applied = renderer.apply(gpu, &cb, stream, parts).unwrap();
        renderer.draw(gpu, &cb, &target.texture);
        cb.commit();
        cb.waitUntilCompleted();
        assert!(cb.error().is_none(), "{:?}", cb.error());
        applied.failed
    }

    fn render(gpu: &Gpu, renderer: &mut VideoRenderer, stream: (u32, u32), tiles: &[(&CVPixelBuffer, TileRect)], target: &Target) {
        let tiles: Vec<_> = tiles.iter().map(|&(pb, tile)| (pb, tile, &[][..])).collect();
        assert_eq!(render_keeping(gpu, renderer, stream, &tiles, target), 0, "failed tiles");
    }

    fn rect(index: u8, x: u32, y: u32, width: u32, height: u32) -> TileRect {
        TileRect { index, x, y, width, height }
    }

    #[test]
    fn tiles_land_in_place_and_convert_to_rgb() {
        let gpu = gpu().expect("Metal");
        let mut renderer = VideoRenderer::default();
        let target = Target::new(gpu, 96, 64);
        let black = [0.0; 3];

        // A flat color in the top-left tile, a luma ramp in the bottom-right one; the rest of the
        // stream has never been sent.
        let red = nv12(32, 32, |_, _| 90, (100, 220));
        let ramp = nv12(32, 32, |x, y| (x * 6 + y) as u8, (128, 128));
        render(gpu, &mut renderer, (96, 64), &[(&red, rect(0, 0, 0, 32, 32)), (&ramp, rect(5, 64, 32, 32, 32))], &target);
        assert_near(target.pixel(10, 10), expected(90, 100, 220), "red tile");
        // (Not the last row/column: chroma there blends with the black next to it.)
        assert_near(target.pixel(30, 30), expected(90, 100, 220), "red tile corner");
        assert_near(target.pixel(40, 10), black, "never sent");
        assert_near(target.pixel(10, 40), black, "never sent");
        // 1:1: every luma sample comes through exactly, at its own pixel.
        for y in 0..32 {
            for x in 0..32 {
                let v = (x * 6 + y) as u8;
                assert_near(target.pixel(64 + x, 32 + y), expected(v, 128, 128), &format!("ramp {x},{y}"));
            }
        }

        // A later update with only the first tile keeps the other one.
        let blue = nv12(32, 32, |_, _| 60, (220, 110));
        render(gpu, &mut renderer, (96, 64), &[(&blue, rect(0, 0, 0, 32, 32))], &target);
        assert_near(target.pixel(10, 10), expected(60, 220, 110), "replaced tile");
        assert_near(target.pixel(64 + 7, 32 + 3), expected(45, 128, 128), "kept tile");

        // A new stream size starts from black.
        render(gpu, &mut renderer, (64, 64), &[(&blue, rect(0, 32, 32, 32, 32))], &target);
        assert_near(target.pixel(16 + 4, 4), black, "new stream, never sent");
        assert_near(target.pixel(16 + 40, 40), expected(60, 220, 110), "new stream tile (letterboxed by 16)");
    }

    #[test]
    fn odd_and_mismatched_tiles_stay_inside_the_canvas() {
        let gpu = gpu().expect("Metal");
        let mut renderer = VideoRenderer::default();
        let target = Target::new(gpu, 33, 17);
        let white = nv12(17, 9, |_, _| 255, (128, 128));
        let big = nv12(64, 64, |_, _| 128, (128, 128));
        let tiles = [
            (&*white, rect(1, 16, 8, 17, 9)),
            // Bigger than its place and the stream: clipped.
            (&*big, rect(0, 0, 0, 16, 8)),
            // Outside the stream: skipped.
            (&*big, rect(2, 40, 0, 16, 8)),
        ];
        render(gpu, &mut renderer, (33, 17), &tiles, &target);
        assert_near(target.pixel(32, 16), expected(255, 128, 128), "odd tile corner");
        assert_near(target.pixel(15, 7), expected(128, 128, 128), "clipped tile");
        assert_near(target.pixel(20, 2), [0.0; 3], "never sent");
    }

    #[test]
    fn late_full_frame_keeps_newer_tiles() {
        let gpu = gpu().expect("Metal");
        let mut renderer = VideoRenderer::default();
        let target = Target::new(gpu, 64, 32);
        let (left_rect, right_rect) = (rect(0, 0, 0, 32, 32), rect(1, 32, 0, 32, 32));
        let left = nv12(32, 32, |_, _| 40, (128, 128));
        let right = nv12(32, 32, |x, _| x as u8, (90, 160));
        render(gpu, &mut renderer, (64, 32), &[(&left, left_rect), (&right, right_rect)], &target);

        // A full frame older than the right tile: everything but that tile.
        let full = nv12(64, 32, |_, _| 180, (200, 60));
        let full_rect = rect(protocol::FULL_FRAME_TILE, 0, 0, 64, 32);
        render_keeping(gpu, &mut renderer, (64, 32), &[(&full, full_rect, &[right_rect])], &target);
        assert_near(target.pixel(5, 5), expected(180, 200, 60), "covered by the full frame");
        // (Not the columns next to the kept tile: chroma there blends with it.)
        assert_near(target.pixel(30, 31), expected(180, 200, 60), "full frame up to the kept tile");
        for x in [33, 40, 63] {
            assert_near(target.pixel(x, 10), expected((x - 32) as u8, 90, 160), &format!("kept tile at {x}"));
        }

        // In one update: a full frame, then a newer tile over part of it; the tile wins.
        let newer = nv12(32, 32, |_, _| 100, (128, 128));
        render_keeping(gpu, &mut renderer, (64, 32), &[(&full, full_rect, &[]), (&newer, left_rect, &[])], &target);
        assert_near(target.pixel(5, 5), expected(100, 128, 128), "newer tile over the full frame");
        assert_near(target.pixel(40, 5), expected(180, 200, 60), "full frame");
    }

    #[test]
    fn unreadable_tiles_are_reported() {
        let gpu = gpu().expect("Metal");
        let mut renderer = VideoRenderer::default();
        let target = Target::new(gpu, 64, 32);
        let good = nv12(32, 32, |_, _| 120, (128, 128));
        let bgra = bgra(32, 32);
        let tiles: [(&CVPixelBuffer, TileRect, &[TileRect]); 2] = [(&bgra, rect(1, 32, 0, 32, 32), &[]), (&good, rect(0, 0, 0, 32, 32), &[])];
        assert_eq!(render_keeping(gpu, &mut renderer, (64, 32), &tiles, &target), 0b10);
        assert_near(target.pixel(5, 5), expected(120, 128, 128), "readable tile");
        assert_near(target.pixel(40, 5), [0.0; 3], "unreadable tile");
    }

    /// A pixel buffer that isn't NV12 (a renderer can't read it).
    pub(crate) fn bgra(width: usize, height: usize) -> CFRetained<CVPixelBuffer> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferCreate(None, width, height, u32::from_be_bytes(*b"BGRA"), None, NonNull::from(&mut raw)) };
        assert_eq!(status, 0);
        unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) }
    }

    /// Prints the CPU and GPU cost of showing an update of a 6144×2560 stream (16 tiles): all
    /// tiles, one, none (only the draw), all without the draw, and a full frame (alone, and late
    /// with one newer tile kept).
    /// `cargo test --release -p lankvm-core render::tests::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_full_update() {
        let gpu = gpu().expect("Metal");
        let (w, h) = (6144u32, 2560u32);
        let layout = protocol::tile_layout(w, h, None);
        let buffers: Vec<_> = layout.iter().map(|t| nv12(t.width as usize, t.height as usize, |x, y| (x ^ y) as u8, (128, 128))).collect();
        let full = nv12(w as usize, h as usize, |x, y| (x ^ y) as u8, (128, 128));
        let full_rect = TileRect { index: protocol::FULL_FRAME_TILE, x: 0, y: 0, width: w, height: h };
        let target = Target::new(gpu, w as usize, h as usize);
        let mut renderer = VideoRenderer::default();
        let tiles = |n: usize| -> Vec<Part<'_>> { buffers.iter().zip(&layout).take(n).map(|(b, t)| Part { pixel_buffer: b, tile: *t, keep: &[] }).collect() };
        let cases: [(&str, Vec<Part<'_>>, bool); 6] = [
            ("all tiles, copy + draw", tiles(layout.len()), true),
            ("1 tile, copy + draw", tiles(1), true),
            ("no tiles, draw", tiles(0), true),
            ("all tiles, copy only", tiles(layout.len()), false),
            ("full frame, copy + draw", vec![Part { pixel_buffer: &full, tile: full_rect, keep: &[] }], true),
            ("late full frame keeping 1 tile, copy + draw", vec![Part { pixel_buffer: &full, tile: full_rect, keep: &layout[..1] }], true),
        ];
        for (what, parts, draw) in cases {
            let (mut encode, mut total) = (Vec::new(), Vec::new());
            for _ in 0..60 {
                let started = Instant::now();
                let cb = gpu.queue.commandBuffer().unwrap();
                let parts = parts.iter().map(|p| Part { pixel_buffer: p.pixel_buffer, tile: p.tile, keep: p.keep });
                let keep = renderer.apply(gpu, &cb, (w, h), parts).unwrap();
                if draw {
                    renderer.draw(gpu, &cb, &target.texture);
                }
                cb.commit();
                encode.push(started.elapsed());
                cb.waitUntilCompleted();
                total.push(started.elapsed());
                drop(keep);
            }
            encode.sort();
            total.sort();
            println!("{what}: encode (CPU) {:?}, encode+GPU {:?} (median)", encode[encode.len() / 2], total[total.len() / 2]);
        }
    }
}
