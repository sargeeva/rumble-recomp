// GX pixel pipeline as one shader: vertices arrive already in EFB clip
// space; the fragment stage evaluates the TEV stages described by the
// uniform block (layout: rumble_runtime::gx::draw::TEV_WORDS) and the alpha
// compare. Swap tables and indirect texturing are not applied.

struct Tev {
    // x: color env, y: alpha env, z: tref, w: ksel (see draw.rs).
    stages: array<vec4<u32>, 16>,
    // PREV, C0, C1, C2.
    regs: array<vec4<f32>, 4>,
    konst: array<vec4<f32>, 4>,
    // x: stage count, y: alpha compare (BP 0xF3).
    misc: vec4<u32>,
};

@group(0) @binding(0) var<uniform> tev: Tev;

@group(1) @binding(0) var t0: texture_2d<f32>;
@group(1) @binding(1) var t1: texture_2d<f32>;
@group(1) @binding(2) var t2: texture_2d<f32>;
@group(1) @binding(3) var t3: texture_2d<f32>;
@group(1) @binding(4) var t4: texture_2d<f32>;
@group(1) @binding(5) var t5: texture_2d<f32>;
@group(1) @binding(6) var t6: texture_2d<f32>;
@group(1) @binding(7) var t7: texture_2d<f32>;
@group(1) @binding(8) var s0: sampler;
@group(1) @binding(9) var s1: sampler;
@group(1) @binding(10) var s2: sampler;
@group(1) @binding(11) var s3: sampler;
@group(1) @binding(12) var s4: sampler;
@group(1) @binding(13) var s5: sampler;
@group(1) @binding(14) var s6: sampler;
@group(1) @binding(15) var s7: sampler;

struct VertexIn {
    @location(0) pos: vec4<f32>,
    @location(1) c0: vec4<f32>,
    @location(2) c1: vec4<f32>,
    @location(3) tc0: vec3<f32>,
    @location(4) tc1: vec3<f32>,
    @location(5) tc2: vec3<f32>,
    @location(6) tc3: vec3<f32>,
    @location(7) tc4: vec3<f32>,
    @location(8) tc5: vec3<f32>,
    @location(9) tc6: vec3<f32>,
    @location(10) tc7: vec3<f32>,
};

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) c0: vec4<f32>,
    @location(1) c1: vec4<f32>,
    @location(2) tc0: vec3<f32>,
    @location(3) tc1: vec3<f32>,
    @location(4) tc2: vec3<f32>,
    @location(5) tc3: vec3<f32>,
    @location(6) tc4: vec3<f32>,
    @location(7) tc5: vec3<f32>,
    @location(8) tc6: vec3<f32>,
    @location(9) tc7: vec3<f32>,
};

@vertex
fn vs(v: VertexIn) -> VertexOut {
    var o: VertexOut;
    o.pos = v.pos;
    o.c0 = v.c0;
    o.c1 = v.c1;
    o.tc0 = v.tc0;
    o.tc1 = v.tc1;
    o.tc2 = v.tc2;
    o.tc3 = v.tc3;
    o.tc4 = v.tc4;
    o.tc5 = v.tc5;
    o.tc6 = v.tc6;
    o.tc7 = v.tc7;
    return o;
}

fn coord(i: u32, v: VertexOut) -> vec2<f32> {
    var t: vec3<f32>;
    switch i {
        case 0u: { t = v.tc0; }
        case 1u: { t = v.tc1; }
        case 2u: { t = v.tc2; }
        case 3u: { t = v.tc3; }
        case 4u: { t = v.tc4; }
        case 5u: { t = v.tc5; }
        case 6u: { t = v.tc6; }
        default: { t = v.tc7; }
    }
    let q = select(t.z, 1.0, abs(t.z) < 1e-6);
    return t.xy / q;
}

fn sample_map(map: u32, uv: vec2<f32>) -> vec4<f32> {
    switch map {
        case 0u: { return textureSampleLevel(t0, s0, uv, 0.0); }
        case 1u: { return textureSampleLevel(t1, s1, uv, 0.0); }
        case 2u: { return textureSampleLevel(t2, s2, uv, 0.0); }
        case 3u: { return textureSampleLevel(t3, s3, uv, 0.0); }
        case 4u: { return textureSampleLevel(t4, s4, uv, 0.0); }
        case 5u: { return textureSampleLevel(t5, s5, uv, 0.0); }
        case 6u: { return textureSampleLevel(t6, s6, uv, 0.0); }
        default: { return textureSampleLevel(t7, s7, uv, 0.0); }
    }
}

// Konst selections: 0-7 fixed fractions, then K0-K3 whole or by channel.
fn fraction(sel: u32) -> f32 {
    return f32(8u - sel) / 8.0;
}

fn konst_color(sel: u32) -> vec3<f32> {
    if sel < 8u {
        return vec3<f32>(fraction(sel));
    }
    if sel >= 12u && sel < 16u {
        return tev.konst[sel - 12u].rgb;
    }
    if sel >= 16u {
        let k = tev.konst[(sel - 16u) % 4u];
        return vec3<f32>(k[(sel - 16u) / 4u]);
    }
    return vec3<f32>(0.0);
}

fn konst_alpha(sel: u32) -> f32 {
    if sel < 8u {
        return fraction(sel);
    }
    if sel >= 16u {
        return tev.konst[(sel - 16u) % 4u][(sel - 16u) / 4u];
    }
    return 0.0;
}

// Final step of a stage: d +/- lerp(a, b, c), bias, scale, clamp.
fn finish(lerp: vec4<f32>, d: vec4<f32>, env: u32) -> vec4<f32> {
    var v = select(d + lerp, d - lerp, ((env >> 18u) & 1u) == 1u);
    let bias = (env >> 16u) & 3u;
    if bias == 1u {
        v = v + vec4<f32>(0.5);
    } else if bias == 2u {
        v = v - vec4<f32>(0.5);
    }
    let scale = array<f32, 4>(1.0, 2.0, 4.0, 0.5);
    v = v * scale[(env >> 20u) & 3u];
    if ((env >> 19u) & 1u) == 1u {
        return clamp(v, vec4<f32>(0.0), vec4<f32>(1.0));
    }
    return clamp(v, vec4<f32>(-4.0), vec4<f32>(4.0));
}

fn compare(func: u32, a: f32, r: f32) -> bool {
    switch func {
        case 0u: { return false; }
        case 1u: { return a < r; }
        case 2u: { return a == r; }
        case 3u: { return a <= r; }
        case 4u: { return a > r; }
        case 5u: { return a != r; }
        case 6u: { return a >= r; }
        default: { return true; }
    }
}

@fragment
fn fs(v: VertexOut) -> @location(0) vec4<f32> {
    // PREV, C0, C1, C2.
    var reg = tev.regs;
    for (var i = 0u; i < min(tev.misc.x, 16u); i = i + 1u) {
        let st = tev.stages[i];
        let tref = st.z;
        var tex = vec4<f32>(1.0);
        if (tref & 0x40u) != 0u {
            tex = sample_map(tref & 7u, coord((tref >> 3u) & 7u, v));
        }
        let chan = (tref >> 7u) & 7u;
        var ras = vec4<f32>(0.0);
        if chan == 0u {
            ras = v.c0;
        } else if chan == 1u {
            ras = v.c1;
        }
        let kc = konst_color(st.w & 0x1fu);
        let ka = konst_alpha((st.w >> 8u) & 0x1fu);

        // Color inputs: PREV, APREV, C0, A0, C1, A1, C2, A2, TEXC, TEXA,
        // RASC, RASA, ONE, HALF, KONST, ZERO.
        var cin = array<vec3<f32>, 16>(
            reg[0].rgb, vec3<f32>(reg[0].a), reg[1].rgb, vec3<f32>(reg[1].a),
            reg[2].rgb, vec3<f32>(reg[2].a), reg[3].rgb, vec3<f32>(reg[3].a),
            tex.rgb, vec3<f32>(tex.a), ras.rgb, vec3<f32>(ras.a),
            vec3<f32>(1.0), vec3<f32>(0.5), kc, vec3<f32>(0.0),
        );
        // Alpha inputs: APREV, A0, A1, A2, TEXA, RASA, KONST, ZERO.
        var ain = array<f32, 8>(reg[0].a, reg[1].a, reg[2].a, reg[3].a, tex.a, ras.a, ka, 0.0);

        let ce = st.x;
        let ae = st.y;
        let ca = clamp(cin[(ce >> 12u) & 15u], vec3<f32>(0.0), vec3<f32>(1.0));
        let cb = clamp(cin[(ce >> 8u) & 15u], vec3<f32>(0.0), vec3<f32>(1.0));
        let cc = clamp(cin[(ce >> 4u) & 15u], vec3<f32>(0.0), vec3<f32>(1.0));
        let cd = cin[ce & 15u];
        let aa = clamp(ain[(ae >> 13u) & 7u], 0.0, 1.0);
        let ab = clamp(ain[(ae >> 10u) & 7u], 0.0, 1.0);
        let ac = clamp(ain[(ae >> 7u) & 7u], 0.0, 1.0);
        let ad = ain[(ae >> 4u) & 7u];

        let color = finish(vec4<f32>(mix(ca, cb, cc), 0.0), vec4<f32>(cd, 0.0), ce).rgb;
        let alpha = finish(vec4<f32>(mix(aa, ab, ac)), vec4<f32>(ad), ae).a;
        let cdst = (ce >> 22u) & 3u;
        let adst = (ae >> 22u) & 3u;
        reg[cdst] = vec4<f32>(color, reg[cdst].a);
        reg[adst].a = alpha;
    }

    let out = clamp(reg[0], vec4<f32>(0.0), vec4<f32>(1.0));
    let ac = tev.misc.y;
    let pass0 = compare((ac >> 16u) & 7u, out.a, f32(ac & 0xffu) / 255.0);
    let pass1 = compare((ac >> 19u) & 7u, out.a, f32((ac >> 8u) & 0xffu) / 255.0);
    var passed: bool;
    switch (ac >> 22u) & 3u {
        case 0u: { passed = pass0 && pass1; }
        case 1u: { passed = pass0 || pass1; }
        case 2u: { passed = pass0 != pass1; }
        default: { passed = pass0 == pass1; }
    }
    if !passed {
        discard;
    }
    return out;
}
