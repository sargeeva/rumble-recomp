//! wgpu renderer for the draw commands the runtime records from the GX
//! FIFO (`rumble_runtime::gx::draw`). The EFB is a render target at an
//! integer multiple of 640x528; a copy to the external framebuffer
//! snapshots it into the XFB texture, which is what gets presented. A copy
//! to a texture in RAM stays on the GPU (`efb_copies`): textures sampled
//! from its address use it, which post effects that redraw the scene
//! through a copy depend on. Each copy is converted to its pixel format on
//! the way (`copy.wgsl`): the game's shadows live in the alpha channel. A
//! texture whose address falls inside a copy's RAM footprint (the shadow
//! receivers sample a window into the shadow map) gets that part of the
//! copy (`copy_window`).
//! Nothing is written back to RAM.
//!
//! Not yet: logic ops, lines and points.

use anyhow::{Context, Result};
use rumble_runtime::gx::draw::{
    copy_layout, copy_row_bytes, CopySpan, Draw, EfbCopy, GpuCommand, TexBinding, EFB_HEIGHT, EFB_WIDTH, TEV_WORDS,
    VERTEX_FLOATS,
};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::Path;
use wgpu::util::DeviceExt;

const EFB_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// An EFB copy to RAM kept on the GPU, with the texture size the game
/// sees (half the copied rectangle for half-size copies).
struct EfbTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: (u32, u32),
    /// The copy's pixel format and row stride in bytes, for where its
    /// pixels lie in RAM.
    format: u32,
    stride: u32,
}

/// Prints a line about EFB copies when `RUMBLE_COPY_LOG` is set, for
/// tracking down which copy a texture shows.
fn diag(line: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("RUMBLE_COPY_LOG").is_some()) {
        println!("{}", line());
    }
}

/// With `RUMBLE_DRAW_LOG=FRAME`, prints every draw and copy of that frame:
/// for tracking down a draw that goes wrong.
fn log_draws(frame: u64, commands: &[GpuCommand]) {
    static AT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    if *AT.get_or_init(|| std::env::var("RUMBLE_DRAW_LOG").ok().and_then(|v| v.parse().ok())) != Some(frame) {
        return;
    }
    for c in commands {
        match c {
            GpuCommand::Draw(d) => {
                let (mut lo, mut hi) = ([f32::MAX; 4], [f32::MIN; 4]);
                for v in d.vertices.chunks(VERTEX_FLOATS) {
                    let w = v[3];
                    for (k, x) in [v[0] / w, v[1] / w, v[2] / w, w].into_iter().enumerate() {
                        lo[k] = lo[k].min(x);
                        hi[k] = hi[k].max(x);
                    }
                }
                let tex: Vec<String> = d.textures.iter().flatten().map(|t| format!("{:#x}", t.addr)).collect();
                println!(
                    "draw v{} cull {} wide {} scissor {:?} x {:.2}..{:.2} y {:.2}..{:.2} z {:.3}..{:.3} w {:.2}..{:.2} blend {:#x} z {:#x} tex {:?}",
                    d.vertices.len() / VERTEX_FLOATS, d.cull, d.wide as u8, d.scissor, lo[0], hi[0], lo[1], hi[1], lo[2], hi[2], lo[3], hi[3], d.blend, d.zmode, tex
                );
            }
            GpuCommand::Copy(c) => println!("copy {:#x} rect {:?} fmt {} xfb {}", c.dest, c.rect, c.format, c.to_xfb),
            GpuCommand::Forget(d) => println!("forget {d:#x}"),
        }
    }
}

/// Whether a copy spans the game's full EFB width, and so the whole of a
/// widescreen EFB.
fn copy_full_width(copy: &EfbCopy) -> bool {
    copy.rect[0] == 0 && copy.rect[2] >= EFB_WIDTH as u32
}

/// What a draw's pipeline depends on: blend mode, z mode, culling.
fn pipeline_key(d: &Draw) -> (u32, u32, u32) {
    (d.blend & 0xfff, d.zmode & 0x1f, d.cull)
}

impl EfbTexture {
    fn layout(&self) -> (u32, u32, u32, u32) {
        copy_layout(self.format)
    }

    fn row_bytes(&self) -> u32 {
        copy_row_bytes(self.format, self.size, self.stride)
    }

    /// The RAM it covers, copied to `dest`.
    fn span(&self, dest: u32) -> CopySpan {
        CopySpan::new(dest, self.format, self.size, self.stride)
    }
}

/// Bits per pixel of a GX texture format (0 I4 ... 6 RGBA8), for matching
/// a texture against a copy's layout.
fn texture_bpp(format: u32) -> Option<u32> {
    Some(match format {
        0 | 14 => 4,
        1 | 2 => 8,
        3..=5 => 16,
        6 => 32,
        _ => return None,
    })
}

/// A texture whose address lies inside an EFB copy (a window into it, as
/// the game's shadow receivers sample the shadow map): the copy's pixels
/// from `origin` on, refreshed after every copy to `source`.
struct CopyWindow {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    source: u32,
    /// Top-left in the source copy, in texture pixels.
    origin: (u32, u32),
    used: u64,
}
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Per-draw uniform slot: the TEV block, padded to the offset alignment.
const UNIFORM_STRIDE: u64 = 512;
/// GPU textures not used for this many frames are dropped.
const TEXTURE_IDLE_FRAMES: u64 = 600;

pub async fn request_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            force_fallback_adapter: false,
        })
        .await
        .context("no suitable GPU adapter")?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("gx"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        })
        .await
        .context("creating the GPU device")?;
    Ok((adapter, device, queue))
}

struct Blit {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    scale: u32,
    /// How much wider than 4:3 the EFB is (widescreen; 1 for 4:3). The
    /// game's 640-pixel EFB sits centered in it; scissors and copies across
    /// the full 640 pixels span all of it (see `Draw::wide`).
    wide: f32,
    efb_view: wgpu::TextureView,
    efb: wgpu::Texture,
    depth_view: wgpu::TextureView,
    xfb: wgpu::Texture,
    xfb_view: wgpu::TextureView,
    /// Last copy to the external framebuffer, in EFB pixels.
    xfb_rect: Option<[u32; 4]>,
    shader: wgpu::ShaderModule,
    tev_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: HashMap<(u32, u32, u32), wgpu::RenderPipeline>,
    textures: HashMap<u64, (wgpu::TextureView, u64)>,
    /// EFB copies to RAM, by destination address: the copy stays on the GPU
    /// and a texture sampled from that address (at the copy's size) uses it.
    efb_copies: HashMap<u32, EfbTexture>,
    /// Converts an EFB copy to its pixel format (`copy.wgsl`); made on
    /// first use.
    copy_pipeline: Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
    /// Textures that read from inside an EFB copy, by (address, width,
    /// height).
    copy_windows: HashMap<(u32, u32, u32), CopyWindow>,
    /// Old address -> new, for a copy the game moved: a newer copy of the
    /// same size and format overwrote it from a nearby address
    /// (`drop_overwritten`).
    moved_copies: HashMap<u32, u32>,
    samplers: HashMap<(u32, u32, bool), wgpu::Sampler>,
    white: wgpu::TextureView,
    frame: u64,
    blit: Option<Blit>,
}

fn efb_size(scale: u32, wide: f32) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: (EFB_WIDTH * wide).round() as u32 * scale,
        height: EFB_HEIGHT as u32 * scale,
        depth_or_array_layers: 1,
    }
}

/// EFB color, EFB depth and XFB targets at `scale` and `wide`.
fn targets(device: &wgpu::Device, scale: u32, wide: f32) -> [wgpu::Texture; 3] {
    let target = |label, format, usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: efb_size(scale, wide),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    use wgpu::TextureUsages as U;
    [
        target("efb", EFB_FORMAT, U::RENDER_ATTACHMENT | U::COPY_SRC | U::TEXTURE_BINDING),
        target("efb depth", DEPTH_FORMAT, U::RENDER_ATTACHMENT),
        target("xfb", EFB_FORMAT, U::COPY_DST | U::COPY_SRC | U::TEXTURE_BINDING),
    ]
}

impl Renderer {
    /// Changes the EFB's resolution multiple. The picture is lost until
    /// the game's next frame.
    pub fn set_scale(&mut self, scale: u32) {
        self.set_targets(scale.max(1), self.wide);
    }

    /// Makes the EFB about `wide` times as wide as 4:3 (see `Draw::wide`).
    pub fn set_widescreen(&mut self, wide: f32) {
        self.set_targets(self.scale, wide.max(1.0));
    }

    /// How much wider than 4:3 the EFB is exactly (its width is a whole
    /// number of pixels): the factor draws must be made with.
    pub fn widescreen(&self) -> f32 {
        efb_size(1, self.wide).width as f32 / EFB_WIDTH
    }

    fn set_targets(&mut self, scale: u32, wide: f32) {
        if (scale, wide) == (self.scale, self.wide) {
            return;
        }
        let [efb, depth, xfb] = targets(&self.device, scale, wide);
        self.efb_view = efb.create_view(&Default::default());
        self.efb = efb;
        self.depth_view = depth.create_view(&Default::default());
        self.xfb_view = xfb.create_view(&Default::default());
        self.xfb = xfb;
        self.scale = scale;
        self.wide = wide;
    }

    /// An EFB rectangle (x, y, width, height in EFB pixels) in target
    /// pixels, clamped to the target: one spanning the full EFB width
    /// spans the whole (possibly wider) target, others keep their place in
    /// the 4:3 EFB centered in it.
    fn efb_rect(&self, r: [u32; 4], full_width: bool) -> [u32; 4] {
        let size = efb_size(self.scale, self.wide);
        let s = self.scale;
        let (x, w) = if full_width {
            let k = size.width as f32 / EFB_WIDTH;
            ((r[0] as f32 * k).round() as u32, (r[2] as f32 * k).round() as u32)
        } else {
            let margin = (size.width - EFB_WIDTH as u32 * s) / 2;
            (r[0] * s + margin, r[2] * s)
        };
        let (x, y) = (x.min(size.width), (r[1] * s).min(size.height));
        [x, y, w.min(size.width - x), (r[3] * s).min(size.height - y)]
    }

    pub fn new(device: wgpu::Device, queue: wgpu::Queue, scale: u32) -> Self {
        let scale = scale.max(1);
        let [efb, depth, xfb] = targets(&device, scale, 1.0);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tev"),
            source: wgpu::ShaderSource::Wgsl(include_str!("tev.wgsl").into()),
        });
        let tev_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tev"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new((TEV_WORDS * 4) as u64),
                },
                count: None,
            }],
        });
        let mut entries = Vec::new();
        for k in 0..8 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: k,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
        }
        for k in 0..8 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 8 + k,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            });
        }
        let tex_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("textures"), entries: &entries });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tev"),
            bind_group_layouts: &[&tev_layout, &tex_layout],
            push_constant_ranges: &[],
        });
        let white = device
            .create_texture_with_data(
                &queue,
                &wgpu::TextureDescriptor {
                    label: Some("white"),
                    size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: EFB_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &[255; 4],
            )
            .create_view(&Default::default());

        Self {
            efb_view: efb.create_view(&Default::default()),
            efb,
            depth_view: depth.create_view(&Default::default()),
            xfb_view: xfb.create_view(&Default::default()),
            xfb,
            xfb_rect: None,
            device,
            queue,
            scale,
            wide: 1.0,
            shader,
            tev_layout,
            tex_layout,
            pipeline_layout,
            pipelines: HashMap::new(),
            textures: HashMap::new(),
            efb_copies: HashMap::new(),
            copy_pipeline: None,
            copy_windows: HashMap::new(),
            moved_copies: HashMap::new(),
            samplers: HashMap::new(),
            white,
            frame: 0,
            blit: None,
        }
    }

    /// Pipeline for a blend mode (BP 0x41) and depth mode (BP 0x40).
    fn create_pipeline(&self, blend: u32, zmode: u32, cull: u32) -> wgpu::RenderPipeline {
        use wgpu::BlendFactor as F;
        let factor = |v: u32, src: bool| match v {
            0 => F::Zero,
            1 => F::One,
            2 if src => F::Dst,
            2 => F::Src,
            3 if src => F::OneMinusDst,
            3 => F::OneMinusSrc,
            4 => F::SrcAlpha,
            5 => F::OneMinusSrcAlpha,
            6 => F::DstAlpha,
            _ => F::OneMinusDstAlpha,
        };
        let blend_state = if blend & (1 << 11) != 0 {
            let c = wgpu::BlendComponent { src_factor: F::One, dst_factor: F::One, operation: wgpu::BlendOperation::ReverseSubtract };
            Some(wgpu::BlendState { color: c, alpha: c })
        } else if blend & 1 != 0 {
            let c = wgpu::BlendComponent {
                src_factor: factor((blend >> 8) & 7, true),
                dst_factor: factor((blend >> 5) & 7, false),
                operation: wgpu::BlendOperation::Add,
            };
            Some(wgpu::BlendState { color: c, alpha: c })
        } else {
            None
        };
        let mut write = wgpu::ColorWrites::empty();
        if blend & (1 << 3) != 0 {
            write |= wgpu::ColorWrites::COLOR;
        }
        if blend & (1 << 4) != 0 {
            write |= wgpu::ColorWrites::ALPHA;
        }
        use wgpu::CompareFunction as C;
        let (compare, depth_write) = if zmode & 1 != 0 {
            let f = [C::Never, C::Less, C::Equal, C::LessEqual, C::Greater, C::NotEqual, C::GreaterEqual, C::Always];
            (f[((zmode >> 1) & 7) as usize], zmode & (1 << 4) != 0)
        } else {
            (C::Always, false)
        };
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4,
            3 => Float32x3, 4 => Float32x3, 5 => Float32x3, 6 => Float32x3,
            7 => Float32x3, 8 => Float32x3, 9 => Float32x3, 10 => Float32x3,
        ];
        self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tev"),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: (VERTEX_FLOATS * 4) as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                }],
            },
            primitive: wgpu::PrimitiveState {
                front_face: wgpu::FrontFace::Cw,
                cull_mode: match cull {
                    1 => Some(wgpu::Face::Back),
                    2 => Some(wgpu::Face::Front),
                    _ => None,
                },
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: depth_write,
                depth_compare: compare,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: EFB_FORMAT, blend: blend_state, write_mask: write })],
            }),
            multiview: None,
            cache: None,
        })
    }

    fn sampler(&mut self, t: &TexBinding) -> wgpu::Sampler {
        let key = (t.wrap_s, t.wrap_t, t.linear);
        if let Some(s) = self.samplers.get(&key) {
            return s.clone();
        }
        let mode = |w: u32| match w {
            1 => wgpu::AddressMode::Repeat,
            2 => wgpu::AddressMode::MirrorRepeat,
            _ => wgpu::AddressMode::ClampToEdge,
        };
        let filter = if t.linear { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest };
        let s = self.device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: mode(t.wrap_s),
            address_mode_v: mode(t.wrap_t),
            mag_filter: filter,
            min_filter: filter,
            ..Default::default()
        });
        self.samplers.insert(key, s.clone());
        s
    }

    fn texture_view(&mut self, t: &TexBinding) -> wgpu::TextureView {
        let tex = &t.texture;
        if let Some(copy) = self.efb_copies.get(&t.addr) {
            if copy.size == (tex.width, tex.height) {
                return copy.view.clone();
            }
        }
        if let Some(copy) = self.moved_copies.get(&t.addr).and_then(|to| self.efb_copies.get(to)) {
            if copy.size == (tex.width, tex.height) {
                diag(|| format!("f{} moved {:#x} {}x{}", self.frame, t.addr, tex.width, tex.height));
                return copy.view.clone();
            }
        }
        if let Some(view) = self.copy_window(t.addr, tex) {
            return view;
        }
        if let Some((view, used)) = self.textures.get_mut(&tex.id) {
            *used = self.frame;
            return view.clone();
        }
        let view = self
            .device
            .create_texture_with_data(
                &self.queue,
                &wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width: tex.width, height: tex.height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: EFB_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &tex.rgba,
            )
            .create_view(&Default::default());
        self.textures.insert(tex.id, (view.clone(), self.frame));
        view
    }

    fn texture_group(&mut self, draw: &Draw) -> wgpu::BindGroup {
        let mut views = Vec::new();
        let mut samplers = Vec::new();
        for t in &draw.textures {
            match t {
                Some(t) => {
                    views.push(self.texture_view(t));
                    samplers.push(self.sampler(t));
                }
                None => {
                    views.push(self.white.clone());
                    samplers.push(self.sampler(&TexBinding {
                        addr: 0,
                        texture: std::sync::Arc::new(rumble_runtime::gx::texture::Texture {
                            id: 0,
                            width: 1,
                            height: 1,
                            key: rumble_runtime::gx::texture::TextureKey {
                                addr: 0,
                                width: 1,
                                height: 1,
                                format: 0,
                                tlut_offset: 0,
                                tlut_format: 0,
                            },
                            rgba: Vec::new(),
                        }),
                        wrap_s: 0,
                        wrap_t: 0,
                        linear: false,
                    }));
                }
            }
        }
        let mut entries: Vec<wgpu::BindGroupEntry> = views
            .iter()
            .enumerate()
            .map(|(k, v)| wgpu::BindGroupEntry { binding: k as u32, resource: wgpu::BindingResource::TextureView(v) })
            .collect();
        entries.extend(
            samplers
                .iter()
                .enumerate()
                .map(|(k, s)| wgpu::BindGroupEntry { binding: 8 + k as u32, resource: wgpu::BindingResource::Sampler(s) }),
        );
        self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.tex_layout, entries: &entries })
    }

    /// The GPU texture an EFB copy to RAM goes into, (re)made at the copy's
    /// size. It is copied at EFB resolution even for half-size copies: the
    /// game samples it with normalized coordinates either way.
    fn efb_copy_target(&mut self, copy: &EfbCopy) {
        let size = copy.texture_size();
        let r = self.efb_rect(copy.rect, copy_full_width(copy));
        let scaled = (r[2].max(1), r[3].max(1));
        self.drop_overwritten(copy);
        self.moved_copies.remove(&copy.dest);
        if let Some(t) = self.efb_copies.get_mut(&copy.dest) {
            if t.size == size && (t.texture.width(), t.texture.height()) == scaled {
                t.format = copy.format;
                t.stride = copy.stride;
                return;
            }
        }
        self.copy_windows.retain(|_, w| w.source != copy.dest);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("efb copy"),
            size: wgpu::Extent3d { width: scaled.0, height: scaled.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: EFB_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        self.efb_copies.insert(copy.dest, EfbTexture { texture, view, size, format: copy.format, stride: copy.stride });
    }

    /// Forgets older copies (and windows into them) whose RAM `copy`
    /// overwrites: a texture at their address now reads the new copy.
    ///
    /// The game moves its shadow map between scenes, and textures can
    /// still name the old buffer. Back from the Battle Royale it renders
    /// the hub's into a buffer 64 KB + 32 bytes below the last one but
    /// samples the old address, which in RAM would read the new map
    /// shifted by 8 x 128 texels: misplaced shadows. On a console the two
    /// buffers presumably coincide (the heap is laid out differently when,
    /// say, the save there takes its real time), and after a restart they
    /// do here too. So an old copy of the same size and format that a new
    /// one overwrites is taken as moved, and its address shows the new
    /// copy whole.
    fn drop_overwritten(&mut self, copy: &EfbCopy) {
        let span = copy.span();
        let stale: Vec<u32> = self
            .efb_copies
            .iter()
            .filter(|(&dest, c)| dest != copy.dest && span.overlaps(&c.span(dest)))
            .map(|(&dest, _)| dest)
            .collect();
        let size = copy.texture_size();
        diag(|| format!("f{} copy {:#x} rect {:?} fmt {} stride {} drops {:x?}", self.frame, copy.dest, copy.rect, copy.format, copy.stride, stale));
        for dest in stale {
            let old = &self.efb_copies[&dest];
            if old.size == size && old.format == copy.format {
                self.moved_copies.insert(dest, copy.dest);
                for to in self.moved_copies.values_mut().filter(|to| **to == dest) {
                    *to = copy.dest;
                }
            }
            self.efb_copies.remove(&dest);
            self.copy_windows.retain(|_, w| w.source != dest);
        }
    }

    /// Drops the copy at `dest`, windows into it and its moved address: the
    /// game has written over its RAM (`GpuCommand::Forget`).
    fn forget_copy(&mut self, dest: u32) {
        diag(|| format!("f{} forget {dest:#x}", self.frame));
        self.efb_copies.remove(&dest);
        self.copy_windows.retain(|_, w| w.source != dest);
        self.moved_copies.retain(|&from, &mut to| from != dest && to != dest);
    }

    /// The texture for `tex`, read from `addr`, if that lies inside an EFB
    /// copy's RAM with the same bits per pixel: the copy seen from that
    /// offset, as reading the RAM would give. Past the copy's edge it reads
    /// as zero.
    fn copy_window(&mut self, addr: u32, tex: &rumble_runtime::gx::texture::Texture) -> Option<wgpu::TextureView> {
        let key = (addr, tex.width, tex.height);
        if let Some(w) = self.copy_windows.get_mut(&key) {
            w.used = self.frame;
            return Some(w.view.clone());
        }
        let bpp = texture_bpp(tex.key.format)?;
        let (source, copy) = self.efb_copies.iter().find(|(&dest, c)| {
            addr != dest && c.span(dest).contains(addr) && c.layout().3 == bpp
        })?;
        let (tw, th, bytes, _) = copy.layout();
        let offset = addr - source;
        let row = copy.row_bytes();
        let origin = ((offset % row) / bytes * tw, offset / row * th);
        diag(|| format!("f{} window {:#x} {}x{} fmt {} from {source:#x} at {origin:?}", self.frame, addr, tex.width, tex.height, tex.key.format));
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("efb copy window"),
            size: wgpu::Extent3d { width: tex.width * self.scale, height: tex.height * self.scale, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: EFB_FORMAT,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let source = *source;
        self.copy_windows.insert(key, CopyWindow { texture, view: view.clone(), source, origin, used: self.frame });
        Some(view)
    }

    /// Refreshes the windows into the copy just made at `dest`.
    fn update_copy_windows(&self, enc: &mut wgpu::CommandEncoder, dest: u32) {
        let Some(copy) = self.efb_copies.get(&dest) else { return };
        // The copy texture is at render resolution; windows count in the
        // copy's own texture pixels.
        let (cw, ch) = (copy.texture.width(), copy.texture.height());
        let (sx, sy) = (cw as f32 / copy.size.0.max(1) as f32, ch as f32 / copy.size.1.max(1) as f32);
        for w in self.copy_windows.values().filter(|w| w.source == dest) {
            let (x, y) = ((w.origin.0 as f32 * sx) as u32, (w.origin.1 as f32 * sy) as u32);
            if x >= cw || y >= ch {
                continue;
            }
            let width = (cw - x).min(w.texture.width());
            let height = (ch - y).min(w.texture.height());
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &copy.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                w.texture.as_image_copy(),
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
    }

    /// Renders an EFB copy into its texture, converted to the copy's pixel
    /// format (`copy.wgsl`).
    fn encode_efb_copy(&mut self, enc: &mut wgpu::CommandEncoder, copy: &EfbCopy, target: &wgpu::TextureView) {
        if self.copy_pipeline.is_none() {
            self.copy_pipeline = Some(self.make_copy_pipeline());
        }
        let Some((pipeline, layout)) = self.copy_pipeline.as_ref() else { return };
        let r = self.efb_rect(copy.rect, copy_full_width(copy));
        // With widescreen, the floor samples the shadow map (A8) past the
        // patch it covers, which was sized for 4:3; a clear border makes
        // that read as no shadow rather than the edge stretched out.
        let border = self.wide > 1.0 && copy.format == 7;
        let flags = copy.intensity as u32 | (!copy.efb_alpha as u32) << 1 | (border as u32) << 2;
        let params: [u32; 8] = [r[0], r[1], copy.format, flags, r[2], r[3], 0, 0];
        let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("efb copy"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let efb_view = self.efb.create_view(&Default::default());
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("efb copy"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&efb_view) },
            ],
        });
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("efb copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn make_copy_pipeline(&self) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
        let shader = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("efb copy"),
            source: wgpu::ShaderSource::Wgsl(include_str!("copy.wgsl").into()),
        });
        let layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("efb copy"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("efb copy"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("efb copy"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("copy_vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("copy_fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: EFB_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview: None,
            cache: None,
        });
        (pipeline, layout)
    }

    /// Scissor rectangle scaled to the EFB target, or None if empty.
    fn scissor(&self, d: &Draw) -> Option<[u32; 4]> {
        let [x, y, w, h] = self.efb_rect(d.scissor, d.wide);
        (w > 0 && h > 0).then_some([x, y, w, h])
    }

    /// Renders one frame's commands into the EFB, snapshotting it at each
    /// copy to the external framebuffer.
    pub fn render_frame(&mut self, commands: &[GpuCommand]) {
        self.frame += 1;
        // Bindings are made before encoding, in command order: a draw sees
        // the copies made before it, as in RAM. A scratch buffer the game
        // copies to, draws from and copies to again within a frame (the
        // befriended Pokemon's portrait, then the minimap) must give each
        // draw its own copy. Each copy keeps the target it was given.
        let mut tex_groups = Vec::new();
        let mut copy_targets = Vec::new();
        log_draws(self.frame, commands);
        for c in commands {
            match c {
                GpuCommand::Draw(d) => {
                    let key = pipeline_key(d);
                    if !self.pipelines.contains_key(&key) {
                        let p = self.create_pipeline(key.0, key.1, key.2);
                        self.pipelines.insert(key, p);
                    }
                    tex_groups.push(self.texture_group(d));
                }
                GpuCommand::Copy(copy) if !copy.to_xfb => {
                    self.efb_copy_target(copy);
                    copy_targets.push(self.efb_copies.get(&copy.dest).map(|t| t.view.clone()));
                }
                GpuCommand::Copy(_) => {}
                GpuCommand::Forget(dest) => self.forget_copy(*dest),
            }
        }
        let mut copy_targets = copy_targets.into_iter();
        let draws: Vec<&Draw> = commands
            .iter()
            .filter_map(|c| match c {
                GpuCommand::Draw(d) => Some(d),
                GpuCommand::Copy(_) | GpuCommand::Forget(_) => None,
            })
            .collect();

        // One vertex buffer and one uniform buffer (a slot per draw).
        let mut vertices: Vec<f32> = Vec::new();
        let mut ranges = Vec::new();
        let mut uniforms = vec![0u8; draws.len().max(1) * UNIFORM_STRIDE as usize];
        for (k, d) in draws.iter().enumerate() {
            let first = (vertices.len() / VERTEX_FLOATS) as u32;
            vertices.extend_from_slice(&d.vertices);
            ranges.push(first..(vertices.len() / VERTEX_FLOATS) as u32);
            let slot = k * UNIFORM_STRIDE as usize;
            uniforms[slot..slot + TEV_WORDS * 4].copy_from_slice(bytemuck::cast_slice(&d.tev));
        }
        if vertices.is_empty() {
            vertices.resize(VERTEX_FLOATS, 0.0);
        }
        let vbuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ubuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tev"),
            contents: &uniforms,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let tev_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tev"),
            layout: &self.tev_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &ubuf,
                    offset: 0,
                    size: NonZeroU64::new((TEV_WORDS * 4) as u64),
                }),
            }],
        });
        let scissors: Vec<Option<[u32; 4]>> = draws.iter().map(|d| self.scissor(d)).collect();

        let mut enc = self.device.create_command_encoder(&Default::default());
        let mut draw_index = 0;
        let mut i = 0;
        while i < commands.len() {
            match &commands[i] {
                GpuCommand::Draw(_) => {
                    let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("efb"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &self.efb_view,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &self.depth_view,
                            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    pass.set_vertex_buffer(0, vbuf.slice(..));
                    while let Some(GpuCommand::Draw(d)) = commands.get(i) {
                        let k = draw_index;
                        draw_index += 1;
                        i += 1;
                        let Some([x, y, w, h]) = scissors[k] else { continue };
                        if ranges[k].is_empty() {
                            continue;
                        }
                        if d.cull == 3 {
                            continue;
                        }
                        pass.set_pipeline(&self.pipelines[&pipeline_key(d)]);
                        pass.set_bind_group(0, &tev_group, &[(k as u64 * UNIFORM_STRIDE) as u32]);
                        pass.set_bind_group(1, &tex_groups[k], &[]);
                        pass.set_scissor_rect(x, y, w, h);
                        pass.draw(ranges[k].clone(), 0..1);
                    }
                }
                GpuCommand::Forget(_) => i += 1,
                GpuCommand::Copy(copy) => {
                    i += 1;
                    if copy.to_xfb {
                        let [x, y, w, h] = self.efb_rect(copy.rect, copy_full_width(copy));
                        if w > 0 && h > 0 {
                            let origin = wgpu::Origin3d { x, y, z: 0 };
                            enc.copy_texture_to_texture(
                                wgpu::TexelCopyTextureInfo {
                                    texture: &self.efb,
                                    mip_level: 0,
                                    origin,
                                    aspect: wgpu::TextureAspect::All,
                                },
                                wgpu::TexelCopyTextureInfo {
                                    texture: &self.xfb,
                                    mip_level: 0,
                                    origin,
                                    aspect: wgpu::TextureAspect::All,
                                },
                                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                            );
                            self.xfb_rect = Some(copy.rect);
                        }
                    } else if let Some(target) = copy_targets.next().flatten() {
                        self.encode_efb_copy(&mut enc, copy, &target);
                        self.update_copy_windows(&mut enc, copy.dest);
                    }
                    if let Some((color, depth)) = copy.clear {
                        let c = |v: f32| v as f64;
                        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("efb clear"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &self.efb_view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: c(color[0]),
                                        g: c(color[1]),
                                        b: c(color[2]),
                                        a: c(color[3]),
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                                view: &self.depth_view,
                                depth_ops: Some(wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(depth),
                                    store: wgpu::StoreOp::Store,
                                }),
                                stencil_ops: None,
                            }),
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                    }
                }
            }
        }
        self.queue.submit([enc.finish()]);

        let frame = self.frame;
        self.textures.retain(|_, (_, used)| frame - *used < TEXTURE_IDLE_FRAMES);
        self.copy_windows.retain(|_, w| frame - w.used < TEXTURE_IDLE_FRAMES);
    }

    /// The last XFB copy's rectangle in the XFB texture, as UV x, y, w, h.
    fn xfb_uv(&self) -> [f32; 4] {
        let r = self.xfb_rect.unwrap_or([0, 0, 640, 480]);
        let [x, y, w, h] = self.efb_rect(r, r[0] == 0 && r[2] >= EFB_WIDTH as u32);
        let size = efb_size(self.scale, self.wide);
        let (sw, sh) = (size.width as f32, size.height as f32);
        [x as f32 / sw, y as f32 / sh, w as f32 / sw, h as f32 / sh]
    }

    fn blit(&mut self, format: wgpu::TextureFormat) -> &Blit {
        if self.blit.as_ref().is_some_and(|b| b.format == format) {
            return self.blit.as_ref().unwrap();
        }
        let shader = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("blit_vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("blit_fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview: None,
            cache: None,
        });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.blit = Some(Blit { format, pipeline, layout, sampler, uniform });
        self.blit.as_ref().unwrap()
    }

    /// Draws the external framebuffer into `target` (a window surface),
    /// within `viewport` (x, y, width, height); the rest is black.
    pub fn present(&mut self, target: &wgpu::TextureView, format: wgpu::TextureFormat, viewport: [f32; 4]) {
        let uv = self.xfb_uv();
        self.blit(format);
        let blit = self.blit.as_ref().unwrap();
        self.queue.write_buffer(&blit.uniform, 0, bytemuck::cast_slice(&uv));
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit"),
            layout: &blit.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: blit.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.xfb_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&blit.sampler) },
            ],
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let [x, y, w, h] = viewport;
            if w > 0.0 && h > 0.0 {
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
                pass.set_pipeline(&blit.pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
    }

    /// Writes the last XFB copy to a PNG.
    pub fn save_png(&self, path: &Path) -> Result<()> {
        let r = self.xfb_rect.unwrap_or([0, 0, 640, 480]);
        let [x, y, w, h] = self.efb_rect(r, r[0] == 0 && r[2] >= EFB_WIDTH as u32);
        let (w, h) = (w.max(1), h.max(1));
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.xfb,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::Wait)?;
        let data = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for line in data.chunks(row as usize).take(h as usize) {
            // Opaque output: the EFB's alpha is not part of the picture.
            for px in line[..(w * 4) as usize].chunks(4) {
                pixels.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&pixels)?;
        Ok(())
    }
}
