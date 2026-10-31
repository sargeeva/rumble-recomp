// Presenting the external framebuffer: a full-screen triangle sampling the
// copied rectangle.

struct Blit {
    // Source rectangle in UV space: x, y, width, height.
    rect: vec4<f32>,
};

@group(0) @binding(0) var<uniform> blit: Blit;
@group(0) @binding(1) var xfb: texture_2d<f32>;
@group(0) @binding(2) var xfb_sampler: sampler;

struct BlitOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn blit_vs(@builtin(vertex_index) i: u32) -> BlitOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: BlitOut;
    o.pos = vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    o.uv = blit.rect.xy + p * blit.rect.zw;
    return o;
}

@fragment
fn blit_fs(v: BlitOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(xfb, xfb_sampler, v.uv).rgb, 1.0);
}
