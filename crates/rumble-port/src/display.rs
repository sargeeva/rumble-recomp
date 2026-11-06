//! Output resolution, aspect ratio and widescreen handling.
//!
//! The game renders for a 4:3 frame. With the widescreen hack on, the
//! renderer's EFB is made wider and the game's full-screen scenes span it
//! with a wider horizontal field of view ("Hor+"), 2D kept in shape (see
//! `rumble_runtime::gx::draw::Draw::wide`); with it off, the 4:3 image is
//! pillarboxed.

use serde::{Deserialize, Serialize};

pub const GAME_ASPECT: f32 = 4.0 / 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Resolution {
    Named(NamedResolution),
    Fixed { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedResolution {
    /// The desktop resolution of the primary monitor.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aspect {
    /// Match the window's aspect ratio.
    Auto,
    /// Force the original 4:3 frame.
    #[serde(rename = "4:3")]
    Standard,
    #[serde(rename = "16:9")]
    Wide,
    #[serde(rename = "16:10")]
    Wide16x10,
    #[serde(rename = "21:9")]
    Ultrawide,
    #[serde(rename = "32:9")]
    SuperUltrawide,
    /// Stretch the 4:3 frame to fill the window.
    Stretch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub resolution: Resolution,
    pub fullscreen: bool,
    pub aspect: Aspect,
    pub widescreen_hack: bool,
    /// Internal render resolution relative to the output (e.g. 2.0 = SSAA).
    pub render_scale: f32,
    pub vsync: bool,
    /// Draw with 4x MSAA.
    pub msaa: bool,
    /// How the picture is scaled up to the window when it renders below
    /// the window's resolution.
    pub upscaling: Upscaler,
    /// FSR's sharpening, 0 (none) to 1.
    pub sharpness: f32,
}

/// See `DisplayConfig::upscaling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upscaler {
    Bilinear,
    /// AMD FidelityFX Super Resolution 1.
    Fsr,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::Named(NamedResolution::Native),
            fullscreen: true,
            aspect: Aspect::Auto,
            widescreen_hack: true,
            render_scale: 1.0,
            vsync: true,
            msaa: false,
            upscaling: Upscaler::Bilinear,
            sharpness: 0.8,
        }
    }
}

/// Pixel rectangle within the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedDisplay {
    pub window: (u32, u32),
    pub viewport: Viewport,
    pub render_target: (u32, u32),
    /// Aspect ratio the 3D scene is rendered at.
    pub scene_aspect: f32,
    /// Multiplier for the x scale of perspective projections.
    pub projection_x_scale: f32,
}

#[cfg(windows)]
pub fn native_resolution() -> (u32, u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SetProcessDPIAware, SM_CXSCREEN, SM_CYSCREEN,
    };
    // Without DPI awareness Windows reports scaled, not physical, pixels.
    unsafe {
        SetProcessDPIAware();
        (GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32)
    }
}

#[cfg(not(windows))]
pub fn native_resolution() -> (u32, u32) {
    (1920, 1080)
}

impl Aspect {
    fn ratio(self, window_aspect: f32) -> f32 {
        match self {
            Aspect::Auto | Aspect::Stretch => window_aspect,
            Aspect::Standard => GAME_ASPECT,
            Aspect::Wide => 16.0 / 9.0,
            Aspect::Wide16x10 => 16.0 / 10.0,
            Aspect::Ultrawide => 21.0 / 9.0,
            Aspect::SuperUltrawide => 32.0 / 9.0,
        }
    }
}

/// Largest rectangle of the given aspect centered in the window.
fn fit(window: (u32, u32), aspect: f32) -> Viewport {
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let (w, h) = if ww / wh > aspect { (wh * aspect, wh) } else { (ww, ww / aspect) };
    let (w, h) = (w.round() as u32, h.round() as u32);
    Viewport { x: (window.0 - w) / 2, y: (window.1 - h) / 2, width: w, height: h }
}

impl DisplayConfig {
    pub fn resolve(&self) -> ResolvedDisplay {
        let window = match self.resolution {
            Resolution::Named(NamedResolution::Native) => native_resolution(),
            Resolution::Fixed { width, height } => (width.max(1), height.max(1)),
        };
        self.resolve_for(window)
    }

    pub fn resolve_for(&self, window: (u32, u32)) -> ResolvedDisplay {
        let window_aspect = window.0 as f32 / window.1 as f32;
        let wanted = self.aspect.ratio(window_aspect);

        let (viewport, scene_aspect) = match self.aspect {
            Aspect::Stretch => (fit(window, window_aspect), GAME_ASPECT),
            _ if !self.widescreen_hack => (fit(window, GAME_ASPECT), GAME_ASPECT),
            _ => (fit(window, wanted), wanted),
        };
        let scale = self.render_scale.clamp(0.25, 8.0);
        ResolvedDisplay {
            window,
            viewport,
            render_target: (
                (viewport.width as f32 * scale).round() as u32,
                (viewport.height as f32 * scale).round() as u32,
            ),
            scene_aspect,
            projection_x_scale: GAME_ASPECT / scene_aspect,
        }
    }
}

impl ResolvedDisplay {
    /// How much wider than 4:3 the scene is drawn (1 without the
    /// widescreen hack).
    pub fn widescreen(&self) -> f32 {
        (self.scene_aspect / GAME_ASPECT).max(1.0)
    }

    /// Orthographic projections (HUD/2D) are kept 4:3 and centered so the
    /// UI is not stretched: returns the sub-viewport they should draw into.
    pub fn hud_viewport(&self) -> Viewport {
        let inner = fit((self.viewport.width, self.viewport.height), GAME_ASPECT);
        Viewport { x: self.viewport.x + inner.x, y: self.viewport.y + inner.y, ..inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(aspect: Aspect, hack: bool) -> DisplayConfig {
        DisplayConfig { aspect, widescreen_hack: hack, ..Default::default() }
    }

    #[test]
    fn widescreen_fills_1080p() {
        let d = cfg(Aspect::Auto, true).resolve_for((1920, 1080));
        assert_eq!(d.viewport, Viewport { x: 0, y: 0, width: 1920, height: 1080 });
        assert!((d.projection_x_scale - 0.75).abs() < 1e-6);
        assert_eq!(d.hud_viewport(), Viewport { x: 240, y: 0, width: 1440, height: 1080 });
    }

    #[test]
    fn hack_off_pillarboxes() {
        let d = cfg(Aspect::Auto, false).resolve_for((1920, 1080));
        assert_eq!(d.viewport, Viewport { x: 240, y: 0, width: 1440, height: 1080 });
        assert_eq!(d.projection_x_scale, 1.0);
    }

    #[test]
    fn forced_16_9_letterboxes_ultrawide_monitor() {
        let d = cfg(Aspect::Wide, true).resolve_for((3440, 1440));
        assert_eq!(d.viewport.height, 1440);
        assert_eq!(d.viewport.width, 2560);
        assert_eq!(d.viewport.x, 440);
    }

    #[test]
    fn config_parses() {
        let c: DisplayConfig = toml::from_str(
            "resolution = { width = 2560, height = 1440 }\naspect = \"21:9\"",
        )
        .unwrap();
        assert_eq!(c.resolution, Resolution::Fixed { width: 2560, height: 1440 });
        assert_eq!(c.aspect, Aspect::Ultrawide);
        let c: DisplayConfig = toml::from_str("resolution = \"native\"").unwrap();
        assert_eq!(c.resolution, Resolution::Named(NamedResolution::Native));
    }
}
