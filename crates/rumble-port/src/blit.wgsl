// Presenting the external framebuffer: a full-screen triangle sampling the
// copied rectangle. Scaled down (supersampling), each pixel averages the
// source pixels it covers, with up to 4x4 bilinear taps (so up to 8x down
// in each direction); scaled up, it is one bilinear tap.

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
    let size = vec2<f32>(textureDimensions(xfb));
    // Source pixels per output pixel, in each direction.
    let span = fwidth(v.uv) * size;
    let taps = clamp(ceil(span / 2.0), vec2<f32>(1.0), vec2<f32>(4.0));
    if taps.x * taps.y <= 1.0 && max(span.x, span.y) <= 1.0 {
        return vec4<f32>(textureSampleLevel(xfb, xfb_sampler, v.uv, 0.0).rgb, 1.0);
    }
    // Bilinear taps two source pixels apart average 2x2 each.
    var sum = vec3<f32>(0.0);
    for (var y = 0.0; y < taps.y; y += 1.0) {
        for (var x = 0.0; x < taps.x; x += 1.0) {
            let off = ((vec2<f32>(x, y) + vec2<f32>(0.5)) / taps - vec2<f32>(0.5)) * span / size;
            sum += textureSampleLevel(xfb, xfb_sampler, v.uv + off, 0.0).rgb;
        }
    }
    return vec4<f32>(sum / (taps.x * taps.y), 1.0);
}
