//! Draws a session's frames into a `CAMetalLayer` owned by the UI, on a dedicated thread.
//!
//! The decoder publishes each frame into a [`ViewSlot`] that holds only the newest one; the
//! render thread wakes, imports it zero-copy, draws and presents right away. Nothing waits for
//! the UI's main thread.

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use platform_mac::{clock, gpu};

use crate::client::{ReadyFrame, Shared};
use crate::render::VideoRenderer;

struct Gpu {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: wgpu::Adapter,
}

fn gpu() -> Result<&'static Gpu> {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    if let Some(g) = GPU.get() {
        return Ok(g);
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .context("no Metal adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
    Ok(GPU.get_or_init(|| Gpu { instance, device, queue, adapter }))
}

#[derive(Default)]
struct SlotState {
    frame: Option<ReadyFrame>,
    size: Option<(u32, u32)>,
    stop: bool,
}

/// Newest-frame hand-off between the decoder and the render thread.
#[derive(Default)]
pub struct ViewSlot {
    state: Mutex<SlotState>,
    wake: Condvar,
}

impl ViewSlot {
    pub fn publish(&self, frame: ReadyFrame) {
        self.state.lock().unwrap().frame = Some(frame);
        self.wake.notify_one();
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.state.lock().unwrap().size = Some((width.max(1), height.max(1)));
        self.wake.notify_one();
    }
}

/// Keeps the layer alive while the render thread uses it.
struct LayerRef(#[allow(dead_code)] Retained<AnyObject>);
// SAFETY: CAMetalLayer may be used from a background thread for rendering.
unsafe impl Send for LayerRef {}

pub struct ViewHandle {
    slot: Arc<ViewSlot>,
    thread: Option<JoinHandle<()>>,
}

impl ViewHandle {
    /// # Safety
    /// `layer` must be a valid `CAMetalLayer`.
    pub unsafe fn attach(layer: *mut c_void, width: u32, height: u32, shared: Arc<Shared>) -> Result<Self> {
        let gpu = gpu()?;
        let retained = unsafe { Retained::retain(layer.cast::<AnyObject>()) }.context("null layer")?;
        let surface = unsafe { gpu.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer)) }?;
        let caps = surface.get_capabilities(&gpu.adapter);
        // Non-sRGB: decoded pixels are already display-encoded, pass them straight through.
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let slot = shared.slot.clone();
        {
            let mut state = slot.state.lock().unwrap();
            state.stop = false;
            state.size = None;
        }
        let layer = LayerRef(retained);
        let thread = std::thread::Builder::new()
            .name("lankvm-render".into())
            .spawn(move || {
                let _layer = layer;
                render_loop(gpu, surface, config, &shared);
            })?;
        Ok(Self { slot, thread: Some(thread) })
    }
}

impl Drop for ViewHandle {
    fn drop(&mut self) {
        self.slot.state.lock().unwrap().stop = true;
        self.slot.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn render_loop(gpu: &Gpu, surface: wgpu::Surface<'static>, mut config: wgpu::SurfaceConfiguration, shared: &Shared) {
    surface.configure(&gpu.device, &config);
    let mut renderer = VideoRenderer::new(&gpu.device, config.format);
    let slot = &shared.slot;
    loop {
        let (frame, size) = {
            let mut state = slot.state.lock().unwrap();
            while state.frame.is_none() && state.size.is_none() && !state.stop {
                state = slot.wake.wait(state).unwrap();
            }
            if state.stop {
                return;
            }
            (state.frame.take(), state.size.take())
        };
        if let Some((w, h)) = size {
            config.width = w;
            config.height = h;
            surface.configure(&gpu.device, &config);
        }
        let mut timing = None;
        if let Some(ReadyFrame { pixel_buffer, timing: t }) = frame {
            match gpu::import_nv12(&gpu.device, pixel_buffer) {
                Ok(nv12) => {
                    renderer.set_frame(&gpu.device, nv12);
                    timing = Some(t);
                }
                Err(e) => tracing::warn!("import frame: {e:#}"),
            }
        }
        if !draw(gpu, &surface, &config, &renderer) {
            continue;
        }
        if let Some(t) = timing {
            let now = clock::now_us();
            let mut stats = shared.stats.lock().unwrap();
            stats.present.add(now.saturating_sub(t.decoded_us) as f64);
            if let Some(captured) = t.capture_local_us {
                stats.total.add(now.saturating_sub(captured) as f64);
            }
            stats.frames_shown += 1;
        }
    }
}

/// Draws the current frame and presents it. Returns false if no drawable was available.
fn draw(gpu: &Gpu, surface: &wgpu::Surface<'static>, config: &wgpu::SurfaceConfiguration, renderer: &VideoRenderer) -> bool {
    let texture = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            surface.configure(&gpu.device, config);
            return false;
        }
        other => {
            tracing::debug!("skipping frame: {other:?}");
            return false;
        }
    };
    let view = texture.texture.create_view(&Default::default());
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.draw(&mut pass, config.width, config.height);
    }
    gpu.queue.submit([encoder.finish()]);
    gpu.queue.present(texture);
    true
}
