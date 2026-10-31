// EFB copies to textures: converts the copied rectangle into what a texture
// in the copy's pixel format reads back as when sampled, so the game's own
// tricks work: shadow maps drawn into the alpha channel and copied as A8 or
// IA8, intensity copies for blur and glow, and so on.

struct Copy {
    // The EFB texel the rectangle starts at (render target resolution).
    origin: vec2<i32>,
    // GX copy format (from control bits 3-6): 0 R4/I4, 1 R8/I8, 2 RA4/IA4,
    // 3 RA8/IA8, 4 RGB565, 5 RGB5A3, 6 RGBA8, 7 A8, 8 R8, 9 G8, 10 B8,
    // 11 RG8, 12 GB8.
    format: u32,
    // Bit 0: intensity formats take luminance rather than red. Bit 1: the
    // EFB has no alpha channel (it reads as opaque). Bit 2: the outermost
    // texels are cleared (a shadow map sampled past its edge reads as no
    // shadow).
    flags: u32,
    // The copy's size in texels.
    size: vec2<u32>,
};

@group(0) @binding(0) var<uniform> cp: Copy;
@group(0) @binding(1) var efb: texture_2d<f32>;

@vertex
fn copy_vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
}

@fragment
fn copy_fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<u32>(pos.xy);
    if (cp.flags & 4u) != 0u && (p.x == 0u || p.y == 0u || p.x + 1u >= cp.size.x || p.y + 1u >= cp.size.y) {
        return vec4<f32>(0.0);
    }
    var c = textureLoad(efb, cp.origin + vec2<i32>(pos.xy), 0);
    if (cp.flags & 2u) != 0u {
        c.a = 1.0;
    }
    // GX's RGB to Y conversion for the intensity formats.
    let y = clamp(dot(c.rgb, vec3<f32>(0.257, 0.504, 0.098)) + 16.0 / 255.0, 0.0, 1.0);
    let i = select(c.r, y, (cp.flags & 1u) != 0u && cp.format < 4u);
    // One-channel formats read back as I8 (the value in every channel),
    // two-channel ones as IA8.
    switch cp.format {
        case 0u, 1u, 8u: { return vec4<f32>(i); }
        case 2u, 3u: { return vec4<f32>(i, i, i, c.a); }
        case 4u: { return vec4<f32>(c.rgb, 1.0); }
        case 7u: { return vec4<f32>(c.a); }
        case 9u: { return vec4<f32>(c.g); }
        case 10u: { return vec4<f32>(c.b); }
        case 11u: { return vec4<f32>(c.g, c.g, c.g, c.r); }
        case 12u: { return vec4<f32>(c.b, c.b, c.b, c.g); }
        default: { return c; }
    }
}
