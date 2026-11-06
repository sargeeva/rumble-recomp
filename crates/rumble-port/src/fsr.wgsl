// AMD FidelityFX Super Resolution 1.0, ported from ffx_fsr1.h (MIT license,
// Copyright (c) 2021 Advanced Micro Devices, Inc.) in plain 32-bit maths:
// EASU (edge adaptive spatial upsampling) scales the external framebuffer's
// picture up to the window, then RCAS (robust contrast adaptive sharpening)
// sharpens it. Both work on display-ready (gamma) colour, as the EFB holds.

struct Fsr {
    // EASU: the source rectangle in xfb pixels (x, y, width, height).
    src: vec4<f32>,
    // The output size in pixels (width, height), then RCAS sharpness (0-1)
    // and an unused slot.
    out: vec4<f32>,
};

@group(0) @binding(0) var<uniform> fsr: Fsr;
@group(0) @binding(1) var tex: texture_2d<f32>;

@vertex
fn fsr_vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
}

// A source pixel, clamped to the source rectangle.
fn src_at(p: vec2<f32>) -> vec3<f32> {
    let lo = fsr.src.xy;
    let hi = fsr.src.xy + fsr.src.zw - vec2<f32>(1.0);
    return textureLoad(tex, vec2<i32>(clamp(p, lo, hi)), 0).rgb;
}

fn luma(c: vec3<f32>) -> f32 {
    return c.g + 0.5 * (c.r + c.b);
}

// Accumulates direction and length from one of the four quadrants around
// the sample: a above, b left, c centre, d right, e below.
fn easu_set(dir: ptr<function, vec2<f32>>, len: ptr<function, f32>, w: f32, la: f32, lb: f32, lc: f32, ld: f32, le: f32) {
    let dc = ld - lc;
    let cb = lc - lb;
    var len_x = max(abs(dc), abs(cb));
    len_x = select(0.0, 1.0 / len_x, len_x > 0.0);
    let dir_x = ld - lb;
    (*dir).x += dir_x * w;
    len_x = clamp(abs(dir_x) * len_x, 0.0, 1.0);
    len_x *= len_x;
    *len += len_x * w;
    let ec = le - lc;
    let ca = lc - la;
    var len_y = max(abs(ec), abs(ca));
    len_y = select(0.0, 1.0 / len_y, len_y > 0.0);
    let dir_y = le - la;
    (*dir).y += dir_y * w;
    len_y = clamp(abs(dir_y) * len_y, 0.0, 1.0);
    len_y *= len_y;
    *len += len_y * w;
}

// Adds one tap of the lanczos-like kernel, stretched along `dir`.
fn easu_tap(acc: ptr<function, vec4<f32>>, off: vec2<f32>, dir: vec2<f32>, len: vec2<f32>, lob: f32, clp: f32, c: vec3<f32>) {
    var v = vec2<f32>(off.x * dir.x + off.y * dir.y, off.x * -dir.y + off.y * dir.x);
    v *= len;
    let d2 = min(dot(v, v), clp);
    var wb = 0.4 * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb *= wb;
    wa *= wa;
    wb = 1.5625 * wb - 0.5625;
    let w = wb * wa;
    *acc += vec4<f32>(c * w, w);
}

@fragment
fn easu_fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let k = fsr.src.zw / fsr.out.xy;
    var pp = (floor(frag.xy) + vec2<f32>(0.5)) * k - vec2<f32>(0.5) + fsr.src.xy;
    let fp = floor(pp);
    pp -= fp;
    // The 12 taps: b c above, e f g h and i j k l, n o below.
    let b = src_at(fp + vec2<f32>(0.0, -1.0));
    let c = src_at(fp + vec2<f32>(1.0, -1.0));
    let e = src_at(fp + vec2<f32>(-1.0, 0.0));
    let f = src_at(fp);
    let g = src_at(fp + vec2<f32>(1.0, 0.0));
    let h = src_at(fp + vec2<f32>(2.0, 0.0));
    let i = src_at(fp + vec2<f32>(-1.0, 1.0));
    let j = src_at(fp + vec2<f32>(0.0, 1.0));
    let kk = src_at(fp + vec2<f32>(1.0, 1.0));
    let l = src_at(fp + vec2<f32>(2.0, 1.0));
    let n = src_at(fp + vec2<f32>(0.0, 2.0));
    let o = src_at(fp + vec2<f32>(1.0, 2.0));
    let bl = luma(b);
    let cl = luma(c);
    let el = luma(e);
    let fl = luma(f);
    let gl = luma(g);
    let hl = luma(h);
    let il = luma(i);
    let jl = luma(j);
    let kl = luma(kk);
    let ll = luma(l);
    let nl = luma(n);
    let ol = luma(o);
    var dir = vec2<f32>(0.0);
    var len = 0.0;
    easu_set(&dir, &len, (1.0 - pp.x) * (1.0 - pp.y), bl, el, fl, gl, jl);
    easu_set(&dir, &len, pp.x * (1.0 - pp.y), cl, fl, gl, hl, kl);
    easu_set(&dir, &len, (1.0 - pp.x) * pp.y, fl, il, jl, kl, nl);
    easu_set(&dir, &len, pp.x * pp.y, gl, jl, kl, ll, ol);
    let dir2 = dir * dir;
    var dir_r = dir2.x + dir2.y;
    let zero = dir_r < 1.0 / 32768.0;
    dir_r = select(inverseSqrt(dir_r), 1.0, zero);
    dir.x = select(dir.x, 1.0, zero);
    dir *= dir_r;
    len = len * 0.5;
    len *= len;
    let stretch = dot(dir, dir) / max(abs(dir.x), abs(dir.y));
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;
    // No ringing: the result stays within the four nearest.
    let min4 = min(min(f, g), min(j, kk));
    let max4 = max(max(f, g), max(j, kk));
    var acc = vec4<f32>(0.0);
    easu_tap(&acc, vec2<f32>(0.0, -1.0) - pp, dir, len2, lob, clp, b);
    easu_tap(&acc, vec2<f32>(1.0, -1.0) - pp, dir, len2, lob, clp, c);
    easu_tap(&acc, vec2<f32>(-1.0, 1.0) - pp, dir, len2, lob, clp, i);
    easu_tap(&acc, vec2<f32>(0.0, 1.0) - pp, dir, len2, lob, clp, j);
    easu_tap(&acc, vec2<f32>(0.0, 0.0) - pp, dir, len2, lob, clp, f);
    easu_tap(&acc, vec2<f32>(-1.0, 0.0) - pp, dir, len2, lob, clp, e);
    easu_tap(&acc, vec2<f32>(1.0, 1.0) - pp, dir, len2, lob, clp, kk);
    easu_tap(&acc, vec2<f32>(2.0, 1.0) - pp, dir, len2, lob, clp, l);
    easu_tap(&acc, vec2<f32>(2.0, 0.0) - pp, dir, len2, lob, clp, h);
    easu_tap(&acc, vec2<f32>(1.0, 0.0) - pp, dir, len2, lob, clp, g);
    easu_tap(&acc, vec2<f32>(1.0, 2.0) - pp, dir, len2, lob, clp, o);
    easu_tap(&acc, vec2<f32>(0.0, 2.0) - pp, dir, len2, lob, clp, n);
    let pix = min(max4, max(min4, acc.rgb / acc.w));
    return vec4<f32>(pix, 1.0);
}

// The most RCAS takes away from a pixel's neighbours' weight.
const RCAS_LIMIT: f32 = 0.25 - 1.0 / 16.0;

// RCAS reads EASU's output, the same size as its own, at the same place
// (the output's viewport origin is in `src.xy`).
@fragment
fn rcas_fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(tex));
    let p = vec2<i32>(floor(frag.xy - fsr.src.xy));
    let bp = textureLoad(tex, clamp(p + vec2<i32>(0, -1), vec2<i32>(0), size - 1), 0).rgb;
    let dp = textureLoad(tex, clamp(p + vec2<i32>(-1, 0), vec2<i32>(0), size - 1), 0).rgb;
    let ep = textureLoad(tex, clamp(p, vec2<i32>(0), size - 1), 0).rgb;
    let fp = textureLoad(tex, clamp(p + vec2<i32>(1, 0), vec2<i32>(0), size - 1), 0).rgb;
    let hp = textureLoad(tex, clamp(p + vec2<i32>(0, 1), vec2<i32>(0), size - 1), 0).rgb;
    let mn4 = min(min(bp, dp), min(fp, hp));
    let mx4 = max(max(bp, dp), max(fp, hp));
    let hit_min = min(mn4, ep) / (4.0 * mx4 + vec3<f32>(1e-5));
    let hit_max = (vec3<f32>(1.0) - max(mx4, ep)) / (4.0 * mn4 - vec3<f32>(4.0 + 1e-5));
    let lobe3 = max(-hit_min, hit_max);
    let lobe = max(-RCAS_LIMIT, min(max(lobe3.r, max(lobe3.g, lobe3.b)), 0.0)) * fsr.out.z;
    let pix = (lobe * (bp + dp + fp + hp) + ep) / (4.0 * lobe + 1.0);
    return vec4<f32>(pix, 1.0);
}
