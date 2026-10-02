//! Draws decoded frames: samples the decoder's NV12 surfaces directly (no upload) and converts
//! to RGB in a shader, letterboxed to the window.

use std::collections::VecDeque;

use platform_mac::gpu::Nv12Frame;

/// Frames kept alive after being replaced, so the GPU can finish any in-flight reads.
const RETAINED_FRAMES: usize = 3;

pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    current: Option<(Nv12Frame, wgpu::BindGroup)>,
    retired: VecDeque<Nv12Frame>,
}

impl VideoRenderer {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("nv12.wgsl"));
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(target.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { pipeline, layout, sampler, current: None, retired: VecDeque::new() }
    }

    pub fn set_frame(&mut self, device: &wgpu::Device, frame: Nv12Frame) {
        let y = frame.y.create_view(&Default::default());
        let uv = frame.uv.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&y) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&uv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        if let Some((old, _)) = self.current.replace((frame, bind_group)) {
            self.retired.push_back(old);
            while self.retired.len() > RETAINED_FRAMES {
                self.retired.pop_front();
            }
        }
    }

    /// Draws the current frame centered in a `target_w`×`target_h` surface, aspect preserved.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, target_w: u32, target_h: u32) {
        let Some((frame, bind_group)) = &self.current else { return };
        let (x, y, w, h) = letterbox(frame.width, frame.height, target_w, target_h);
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..4, 0..1);
        pass.set_viewport(0.0, 0.0, target_w as f32, target_h as f32, 0.0, 1.0);
    }
}

/// Largest rect with the frame's aspect ratio centered in the target. When the frame fits
/// exactly it's drawn 1:1, pixel-aligned, which keeps text sharp.
fn letterbox(fw: u32, fh: u32, tw: u32, th: u32) -> (f32, f32, f32, f32) {
    let scale = (tw as f32 / fw as f32).min(th as f32 / fh as f32);
    let (w, h) = ((fw as f32 * scale).round(), (fh as f32 * scale).round());
    (((tw as f32 - w) / 2.0).floor(), ((th as f32 - h) / 2.0).floor(), w, h)
}

#[cfg(test)]
mod tests {
    use super::letterbox;

    #[test]
    fn letterbox_fits_and_centers() {
        assert_eq!(letterbox(1920, 1080, 1920, 1080), (0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(letterbox(1920, 1080, 1920, 1200), (0.0, 60.0, 1920.0, 1080.0));
        assert_eq!(letterbox(1000, 1000, 2000, 1000), (500.0, 0.0, 1000.0, 1000.0));
    }
}
