//! Exe icons. The setup exe gets `assets/setup-icon.png` when it is
//! compiled (`build.rs` includes this file); setup gives each exe it builds
//! that game's title logo instead, by replacing the same resources. Both
//! use one icon group (resource 1) holding `SIZES`, each image a PNG.

/// Sizes in each icon, largest first. Icon images are resources 1..=6.
pub const SIZES: [u32; 6] = [256, 64, 48, 32, 24, 16];

/// The icon group's resource name, which windows load their icon from.
pub const GROUP: u16 = 1;

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn from_png(data: &[u8]) -> Result<Image, String> {
        let mut decoder = png::Decoder::new(data);
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let px = |c: &[u8]| -> [u8; 4] {
            match info.color_type {
                png::ColorType::Rgba => [c[0], c[1], c[2], c[3]],
                png::ColorType::Rgb => [c[0], c[1], c[2], 255],
                png::ColorType::GrayscaleAlpha => [c[0], c[0], c[0], c[1]],
                _ => [c[0], c[0], c[0], 255],
            }
        };
        let channels = info.color_type.samples();
        let rgba = buf[..info.buffer_size()].chunks_exact(channels).flat_map(px).collect();
        Ok(Image { width: info.width, height: info.height, rgba })
    }

    /// The smallest part holding every visible pixel.
    pub fn trimmed(&self) -> Image {
        let (w, h) = (self.width as usize, self.height as usize);
        let seen = |x: usize, y: usize| self.rgba[(y * w + x) * 4 + 3] != 0;
        let rows: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| seen(x, y))).collect();
        let cols: Vec<usize> = (0..w).filter(|&x| (0..h).any(|y| seen(x, y))).collect();
        let (Some(&y0), Some(&y1), Some(&x0), Some(&x1)) = (rows.first(), rows.last(), cols.first(), cols.last()) else {
            return Image { width: self.width, height: self.height, rgba: self.rgba.clone() };
        };
        let rgba = (y0..=y1).flat_map(|y| self.rgba[(y * w + x0) * 4..(y * w + x1 + 1) * 4].iter().copied()).collect();
        Image { width: (x1 - x0 + 1) as u32, height: (y1 - y0 + 1) as u32, rgba }
    }

    /// The image fitted in a `size` square, centred, with transparent
    /// margins. Each pixel averages the source pixels under it.
    pub fn square(&self, size: u32) -> Vec<u8> {
        let scale = self.width.max(self.height) as f32 / size as f32;
        let x0 = (self.width as f32 - size as f32 * scale) / 2.0;
        let y0 = (self.height as f32 - size as f32 * scale) / 2.0;
        // Samples per pixel along each axis.
        let n = (scale.ceil() as u32).max(1) * 2;
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let mut sum = [0f32; 4];
                for sy in 0..n {
                    for sx in 0..n {
                        let fx = x0 + (x as f32 + (sx as f32 + 0.5) / n as f32) * scale;
                        let fy = y0 + (y as f32 + (sy as f32 + 0.5) / n as f32) * scale;
                        if fx < 0.0 || fy < 0.0 || fx >= self.width as f32 || fy >= self.height as f32 {
                            continue;
                        }
                        let o = ((fy as u32 * self.width + fx as u32) * 4) as usize;
                        let a = self.rgba[o + 3] as f32;
                        for c in 0..3 {
                            sum[c] += self.rgba[o + c] as f32 * a;
                        }
                        sum[3] += a;
                    }
                }
                let a = sum[3] / (n * n) as f32;
                let color = |c: usize| if sum[3] > 0.0 { (sum[c] / sum[3]).round() as u8 } else { 0 };
                out.extend([color(0), color(1), color(2), a.round() as u8]);
            }
        }
        out
    }

    /// The icon's images, as PNGs, in `SIZES` order.
    pub fn icon_images(&self) -> Vec<Vec<u8>> {
        SIZES.iter().map(|&s| png_rgba(&self.square(s), s)).collect()
    }
}

fn png_rgba(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG header");
    writer.write_image_data(rgba).expect("PNG data");
    writer.finish().expect("PNG end");
    out
}

/// The icon group resource (GRPICONDIR) for `images`, which are resources
/// 1, 2, ... in `SIZES` order.
pub fn group(images: &[Vec<u8>]) -> Vec<u8> {
    let mut g = Vec::new();
    g.extend(0u16.to_le_bytes());
    g.extend(1u16.to_le_bytes());
    g.extend((images.len() as u16).to_le_bytes());
    for (k, (img, &size)) in images.iter().zip(&SIZES).enumerate() {
        // 256 is written as 0.
        let side = if size >= 256 { 0 } else { size as u8 };
        g.extend([side, side, 0, 0]);
        g.extend(1u16.to_le_bytes());
        g.extend(32u16.to_le_bytes());
        g.extend((img.len() as u32).to_le_bytes());
        g.extend((k as u16 + 1).to_le_bytes());
    }
    g
}

/// A compiled resource file (.res) holding the icon, for the linker.
#[allow(dead_code)] // only build.rs uses it
pub fn res_file(images: &[Vec<u8>]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let mut out = Vec::new();
    let mut add = |kind: u16, name: u16, flags: u16, data: &[u8]| {
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(32u32.to_le_bytes());
        for v in [0xffff, kind, 0xffff, name] {
            out.extend(u16::to_le_bytes(v));
        }
        out.extend(0u32.to_le_bytes());
        out.extend(flags.to_le_bytes());
        // Language neutral, as setup writes them.
        out.extend(0u16.to_le_bytes());
        out.extend([0; 8]);
        out.extend(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    };
    // A .res file starts with an empty entry.
    add(0, 0, 0, &[]);
    for (k, img) in images.iter().enumerate() {
        add(RT_ICON, k as u16 + 1, 0x1010, img);
    }
    add(RT_GROUP_ICON, GROUP, 0x1030, &group(images));
    out
}
