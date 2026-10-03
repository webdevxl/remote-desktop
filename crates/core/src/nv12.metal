// Copies decoded NV12 tiles into the canvas, and draws the canvas (full-range BT.709) as a quad
// filling the viewport.

#include <metal_stdlib>
using namespace metal;

struct VsOut {
    float4 pos [[position]];
    float2 uv;
};

vertex VsOut vs_main(uint i [[vertex_id]]) {
    // Triangle strip covering the viewport.
    float x = float(i & 1u);
    float y = float((i >> 1u) & 1u);
    VsOut out;
    out.pos = float4(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = float2(x, y);
    return out;
}

fragment float4 fs_main(VsOut in [[stage_in]],
                        texture2d<float> tex_y [[texture(0)]],
                        texture2d<float> tex_uv [[texture(1)]]) {
    constexpr sampler samp(filter::linear, address::clamp_to_edge);
    float y = tex_y.sample(samp, in.uv).r;
    float2 c = tex_uv.sample(samp, in.uv).rg - float2(0.5, 0.5);
    float r = y + 1.5748 * c.y;
    float g = y - 0.1873 * c.x - 0.4681 * c.y;
    float b = y + 1.8556 * c.x;
    return float4(clamp(float3(r, g, b), float3(0.0), float3(1.0)), 1.0);
}

// Copies a tile's plane into the canvas, `origin` texels in. The grid is the copied size. Canvas
// texels inside one of the `keep` rects (x0, y0, x1, y1; end exclusive) are left alone: they show
// something newer than this image (a full frame that came late).
kernel void copy_plane(texture2d<float, access::read> src [[texture(0)]],
                       texture2d<float, access::write> dst [[texture(1)]],
                       constant uint2 &origin [[buffer(0)]],
                       constant uint4 *keep [[buffer(1)]],
                       constant uint &keep_count [[buffer(2)]],
                       uint2 gid [[thread_position_in_grid]]) {
    uint2 at = gid + origin;
    for (uint i = 0; i < keep_count; i++) {
        if (all(at >= keep[i].xy) && all(at < keep[i].zw)) {
            return;
        }
    }
    dst.write(src.read(gid), at);
}
