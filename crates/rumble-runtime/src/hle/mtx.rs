//! Widescreen at the source: the game's cameras build their projection
//! with `C_MTXPerspective`, and the game decides what to draw from the same
//! camera. Widening only the picture (`gx::draw::Draw::wide`) leaves things
//! at the sides culled away, so with widescreen on, the cameras the game
//! draws full-screen 3D with get the wider aspect here, and the game itself
//! draws (and keeps) the wider view.
//!
//! Which cameras: those whose projection the renderer last saw on a
//! full-width perspective draw (`Gx::full_width_proj`), recognised by the
//! matrix they built before; from then on they are widened (`Gx::
//! wide_cameras`, with the aspect ratio they ask for: the main camera's is
//! 1.25625, 4:3 corrected for the Wii's pixels). Cameras for portraits and
//! icons never draw full-width and are left alone.
//!
//! The shadow map is drawn by a light camera looking straight down, with
//! an orthographic projection (`C_MTXOrtho`) over a patch of ground sized
//! for the 4:3 view; past it there is no shadow. That camera is
//! recognised by its extent's size being the shadow pass's (`Gx::
//! shadow_proj`; the extent follows the player, its size stays), and its
//! left/right are widened about the middle. The game projects the map
//! onto the floor its own way, so the renderer narrows the floor's
//! coordinates into it to match (`gx::draw`).
//!
//! Shadows and reflections are projected from a camera with
//! `C_MTXLightPerspective`, at the camera's aspect ratio; that is widened
//! to match for the same cameras. So is the view frustum the 3D engine
//! culls models with (NW4R's `math::FRUSTUM::Set`, from the same camera
//! fields): only its aspect argument is changed, and the game's own
//! function then runs.

use super::Replacement;
use crate::cpu::Cpu;
use crate::mem::{Fault, Memory};

pub static REPLACEMENTS: &[Replacement] = &[
    Replacement {
        name: "C_MTXPerspective",
        sig: &[
            0x9421ffc0, 0x7c0802a6, 0xc0a20000, 0x90010044, 0xec250072, 0xc0020000,
            0xdbe10030, 0xf3e10038, 0xffe02090, 0xec200072, 0xdbc10020, 0xf3c10028,
            0xffc01890, 0xdba10010, 0xf3a10018, 0xffa01090, 0x93e1000c, 0x7c7f1b78,
            0x48000001, 0xfc800818, 0xc0620000, 0xc0a20000, 0xec5ff028, 0xc0020000,
        ],
        run: mtx_perspective,
    },
    Replacement {
        name: "C_MTXLightPerspective",
        sig: &[
            0x9421ffa0, 0x7c0802a6, 0xc0e20000, 0x90010064, 0xec270072, 0xc0020000,
            0xdbe10050, 0xf3e10058, 0xec200072, 0xffe03090, 0xdbc10040, 0xf3c10048,
            0xffc02890, 0xdba10030, 0xf3a10038, 0xffa02090, 0xdb810020, 0xf3810028,
            0xff801890, 0xdb610010, 0xf3610018, 0xff601090, 0x93e1000c, 0x7c7f1b78,
        ],
        run: mtx_light_perspective,
    },
    Replacement {
        name: "C_MTXOrtho",
        sig: &[
            0xed641828, 0xc1020000, 0xc1420000, 0xece11028, 0xec062828, 0xc1220000,
            0xed8a5824, 0xc0a20000, 0xd1030000, 0xd1030000, 0xd1030000, 0xd1030000,
            0xed6a3824, 0xd1030000, 0xd1030000, 0xd1030000, 0xd1030000, 0xd1030000,
            0xecea0024, 0xd1430000, 0xfc003050, 0xec21102a, 0xec64182a, 0xecc90332,
        ],
        run: mtx_ortho,
    },
    Replacement {
        name: "nw4r::math::FRUSTUM::Set",
        sig: &[
            0x9421ffc0, 0x7c0802a6, 0xc0020000, 0x90010044, 0xec200072, 0xc0020000,
            0xdbe10030, 0xf3e10038, 0xffe02090, 0xdbc10020, 0xf3c10028, 0xffc01890,
            0xdba10010, 0xf3a10018, 0xffa01090, 0xc0420000, 0x93e1000c, 0x7c9f2378,
            0xec220072, 0x93c10008, 0x7c7e1b78, 0xec200072, 0x48000001, 0xfc000818,
        ],
        run: frustum_set,
    },
];

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-3 * b.abs().max(1e-6)
}

/// How much wider than it asked for a full-screen projection with this
/// aspect ratio (x scale against y scale) was built, if it is a widened
/// camera's: by the current factor or a recent one.
pub fn widened_by(gx: &crate::gx::Gx, aspect: f32) -> Option<f32> {
    let factors = gx.widescreen_history.iter().chain([&gx.widescreen]).filter(|&&k| k != 1.0);
    factors.copied().find(|&k| gx.wide_cameras.iter().any(|&(_, base)| close(aspect, base * k)))
}

fn write_rows(mem: &mut Memory, m: u32, rows: &[[f32; 4]]) -> Result<(), Fault> {
    for (i, v) in rows.iter().flatten().enumerate() {
        mem.w32(m + 4 * i as u32, v.to_bits())?;
    }
    Ok(())
}

fn cot(fov_y: f32) -> f32 {
    1.0 / (fov_y * 0.5).to_radians().tan()
}

/// `C_MTXPerspective(m, fovY, aspect, near, far)`: a GX perspective
/// projection (4x4, row major), as the SDK builds it.
fn mtx_perspective(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let m = cpu.gpr[3];
    let [fov_y, mut aspect, n, f] = [1, 2, 3, 4].map(|r| cpu.fpr[r][0] as f32);
    let k = mem.gx.widescreen;
    if k != 1.0 {
        let known = mem.gx.wide_cameras.iter().any(|&(c, base)| c == m && close(aspect, base));
        // A camera is full-screen when the renderer saw the matrix it built
        // last on a full-width draw.
        let mut old = |i: u32| mem.r32(m + 4 * i).map(f32::from_bits).unwrap_or(f32::NAN);
        let (x, y) = (old(0), old(5));
        let full_screen = !known && mem.gx.full_width_proj.is_some_and(|p| close(x, p[0]) && close(y, p[2]));
        if full_screen {
            mem.gx.wide_cameras.retain(|&(c, _)| c != m);
            // Scenes make new cameras; the oldest are forgotten.
            if mem.gx.wide_cameras.len() >= 8 {
                mem.gx.wide_cameras.remove(0);
            }
            mem.gx.wide_cameras.push((m, aspect));
        }
        if known || full_screen {
            aspect *= k;
        }
    }
    let c = cot(fov_y);
    let depth = 1.0 / (f - n);
    write_rows(
        mem,
        m,
        &[
            [c / aspect, 0.0, 0.0, 0.0],
            [0.0, c, 0.0, 0.0],
            [0.0, 0.0, -n * depth, -(f * n) * depth],
            [0.0, 0.0, -1.0, 0.0],
        ],
    )
}

/// `C_MTXOrtho(m, top, bottom, left, right, near, far)`: a GX orthographic
/// projection (4x4), as the SDK builds it.
fn mtx_ortho(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let m = cpu.gpr[3];
    let [t, b, mut l, mut r, n, f] = [1, 2, 3, 4, 5, 6].map(|i| cpu.fpr[i][0] as f32);
    let k = mem.gx.widescreen;
    if k != 1.0 {
        let size = [r - l, t - b];
        let known = mem.gx.wide_lights.iter().any(|s| close(size[0], s[0]) && close(size[1], s[1]));
        let shadow = !known
            && mem.gx.shadow_proj.is_some_and(|p| close(2.0 / p[0], size[0]) && close(2.0 / p[2], size[1]));
        if shadow {
            if mem.gx.wide_lights.len() >= 4 {
                mem.gx.wide_lights.remove(0);
            }
            mem.gx.wide_lights.push(size);
        }
        if known || shadow {
            let (mid, half) = ((l + r) / 2.0, (r - l) / 2.0 * k);
            (l, r) = (mid - half, mid + half);
            mem.gx.shadow_light_widened();
        }
    }
    let (w, h, d) = (1.0 / (r - l), 1.0 / (t - b), 1.0 / (f - n));
    write_rows(
        mem,
        m,
        &[
            [2.0 * w, 0.0, 0.0, -(r + l) * w],
            [0.0, 2.0 * h, 0.0, -(t + b) * h],
            [0.0, 0.0, -d, -f * d],
            [0.0, 0.0, 0.0, 1.0],
        ],
    )
}

/// `FRUSTUM::Set(this, fovY, aspect, near, far, camMtx)`: widens a wide
/// camera's aspect argument, then carries on into the function itself
/// past its first two instructions, done here (`stwu r1, -0x40(r1)`,
/// `mflr r0`), as its entry holds the trap.
fn frustum_set(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let aspect = cpu.fpr[2][0] as f32;
    if mem.gx.widescreen != 1.0 && mem.gx.wide_cameras.iter().any(|&(_, base)| close(aspect, base)) {
        cpu.fpr[2][0] = (aspect * mem.gx.widescreen) as f64;
    }
    let entry = cpu.pc - 4;
    let sp = cpu.gpr[1];
    mem.w32(sp - 0x40, sp)?;
    cpu.gpr[1] = sp - 0x40;
    cpu.gpr[0] = cpu.spr[crate::cpu::SPR_LR];
    cpu.pc = entry + 8;
    Ok(())
}

/// `C_MTXLightPerspective(m, fovY, aspect, scaleS, scaleT, transS,
/// transT)`: a 3x4 texture projection from a camera's point of view, as
/// the SDK builds it.
fn mtx_light_perspective(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let m = cpu.gpr[3];
    let [fov_y, mut aspect, scale_s, scale_t, trans_s, trans_t] = [1, 2, 3, 4, 5, 6].map(|r| cpu.fpr[r][0] as f32);
    if mem.gx.widescreen != 1.0 && mem.gx.wide_cameras.iter().any(|&(_, base)| close(aspect, base)) {
        aspect *= mem.gx.widescreen;
    }
    let c = cot(fov_y);
    write_rows(
        mem,
        m,
        &[[c / aspect * scale_s, 0.0, -trans_s, 0.0], [0.0, c * scale_t, -trans_t, 0.0], [0.0, 0.0, -1.0, 0.0]],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u32 = 0x8000_4000;

    fn perspective(mem: &mut Memory, aspect: f64) -> f32 {
        let mut cpu = Cpu::default();
        cpu.gpr[3] = M;
        cpu.fpr[1][0] = 90.0;
        cpu.fpr[2][0] = aspect;
        cpu.fpr[3][0] = 1.0;
        cpu.fpr[4][0] = 100.0;
        mtx_perspective(&mut cpu, mem).unwrap();
        f32::from_bits(mem.r32(M).unwrap())
    }

    /// A camera is widened once the renderer has drawn full-screen with its
    /// projection, and stays widened; others are left alone.
    #[test]
    fn widens_full_screen_cameras() {
        let mut mem = Memory::default();
        assert!((perspective(&mut mem, 1.25) - 0.8).abs() < 1e-5);
        assert_eq!(f32::from_bits(mem.r32(M + 4 * 14).unwrap()), -1.0);

        mem.gx.widescreen = 4.0 / 3.0;
        assert!((perspective(&mut mem, 1.25) - 0.8).abs() < 1e-5, "not seen full-screen yet");
        mem.gx.full_width_proj = Some([0.8, 0.0, 1.0, 0.0, 0.0, 0.0]);
        assert!((perspective(&mut mem, 1.25) - 0.6).abs() < 1e-5);
        assert!((perspective(&mut mem, 1.25) - 0.6).abs() < 1e-5, "still, from the list");
        assert_eq!(widened_by(&mem.gx, 1.25 * 4.0 / 3.0), Some(4.0 / 3.0));
    }

    /// The texture projection of a widened camera's aspect is widened too.
    #[test]
    fn widens_light_projection_of_wide_cameras() {
        let mut mem = Memory::default();
        mem.gx.widescreen = 2.0;
        mem.gx.wide_cameras.push((M, 1.0));
        let mut cpu = Cpu::default();
        cpu.gpr[3] = 0x8000_5000;
        cpu.fpr[1][0] = 90.0;
        cpu.fpr[2][0] = 1.0;
        cpu.fpr[3][0] = 0.5;
        cpu.fpr[4][0] = 0.5;
        cpu.fpr[5][0] = 0.5;
        cpu.fpr[6][0] = 0.5;
        mtx_light_perspective(&mut cpu, &mut mem).unwrap();
        assert!((f32::from_bits(mem.r32(0x8000_5000).unwrap()) - 0.25).abs() < 1e-5);
        assert_eq!(f32::from_bits(mem.r32(0x8000_5000 + 8).unwrap()), -0.5);
    }
}
