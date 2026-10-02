// Draws a decoded NV12 frame (full-range BT.709) as a quad filling the viewport.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // Triangle strip covering the viewport.
    let x = f32(i & 1u);
    let y = f32((i >> 1u) & 1u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let c = textureSample(tex_uv, samp, in.uv).rg - vec2<f32>(0.5, 0.5);
    let r = y + 1.5748 * c.y;
    let g = y - 0.1873 * c.x - 0.4681 * c.y;
    let b = y + 1.8556 * c.x;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
