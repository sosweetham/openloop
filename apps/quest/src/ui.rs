//! A small software canvas for the headset menu: premultiplied-alpha sRGB RGBA, anti-aliased
//! rounded rectangles and text. Drawing calls take layout units, which `scale` maps to pixels.

use fontdue::{Font, FontSettings};

/// sRGB colour with straight alpha.
pub type Rgba = [u8; 4];

const SYSTEM_FONT: &str = "/system/fonts/Roboto-Regular.ttf";

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// Rows top to bottom, premultiplied.
    pub pixels: Vec<u8>,
    scale: f32,
    font: Option<Font>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, scale: f32) -> Self {
        let font = std::fs::read(SYSTEM_FONT)
            .map_err(|e| e.to_string())
            .and_then(|data| Font::from_bytes(data, FontSettings::default()).map_err(str::to_owned))
            .map_err(|e| log::warn!("menu font {SYSTEM_FONT}: {e}; text is not drawn"))
            .ok();
        Self {
            width,
            height,
            pixels: vec![0; width * height * 4],
            scale,
            font,
        }
    }

    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }

    fn blend(&mut self, x: usize, y: usize, color: Rgba, coverage: f32) {
        let alpha = f32::from(color[3]) / 255.0 * coverage;
        let i = (y * self.width + x) * 4;
        for (dst, &src) in self.pixels[i..i + 3].iter_mut().zip(&color[..3]) {
            *dst = (f32::from(src) * alpha + f32::from(*dst) * (1.0 - alpha)).round() as u8;
        }
        let dst = f32::from(self.pixels[i + 3]);
        self.pixels[i + 3] = (255.0 * alpha + dst * (1.0 - alpha)).round() as u8;
    }

    /// Coverage comes from the signed distance to the rounded rectangle, so edges are smooth.
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgba) {
        let k = self.scale;
        let (x, y, w, h, radius) = (x * k, y * k, w * k, h * k, radius * k);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let rows = (y.max(0.0) as usize)..((y + h).ceil() as usize).min(self.height);
        let cols = (x.max(0.0) as usize)..((x + w).ceil() as usize).min(self.width);
        for py in rows {
            for px in cols.clone() {
                let qx = (px as f32 + 0.5 - cx).abs() - (w / 2.0 - radius);
                let qy = (py as f32 + 0.5 - cy).abs() - (h / 2.0 - radius);
                let distance = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(px, py, color, coverage);
                }
            }
        }
    }

    pub fn text(&mut self, x: f32, baseline: f32, size: f32, text: &str, color: Rgba) {
        let k = self.scale;
        let (x, baseline, size) = (x * k, baseline * k, size * k);
        let Some(font) = self.font.take() else {
            return;
        };
        let mut pen = x;
        for ch in text.chars() {
            let (metrics, bitmap) = font.rasterize(ch, size);
            let left = (pen + metrics.xmin as f32).round() as i64;
            let top = (baseline - metrics.height as f32 - metrics.ymin as f32).round() as i64;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let (px, py) = (left + col as i64, top + row as i64);
                    let coverage = f32::from(bitmap[row * metrics.width + col]) / 255.0;
                    if coverage > 0.0
                        && (0..self.width as i64).contains(&px)
                        && (0..self.height as i64).contains(&py)
                    {
                        self.blend(px as usize, py as usize, color, coverage);
                    }
                }
            }
            pen += metrics.advance_width;
        }
        self.font = Some(font);
    }
}
