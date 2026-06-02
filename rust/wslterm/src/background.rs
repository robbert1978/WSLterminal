//! Optional terminal-pane background image support.
//!
//! The renderer keeps terminal chrome/editor drawing unchanged. When configured,
//! this module decodes one bitmap, caches pane-sized rasters, and alpha-composites
//! them under the terminal cells.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use image::imageops::{self, FilterType};
use image::RgbaImage;

/// How the background image is sized inside each terminal pane.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BackgroundFit {
    Cover,
    Contain,
    Stretch,
    Tile,
    Center,
}

impl BackgroundFit {
    pub fn parse(value: &str) -> BackgroundFit {
        match value.trim().to_ascii_lowercase().as_str() {
            "contain" => BackgroundFit::Contain,
            "stretch" | "fill" => BackgroundFit::Stretch,
            "tile" => BackgroundFit::Tile,
            "center" | "centre" => BackgroundFit::Center,
            _ => BackgroundFit::Cover,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BackgroundFit::Cover => "cover",
            BackgroundFit::Contain => "contain",
            BackgroundFit::Stretch => "stretch",
            BackgroundFit::Tile => "tile",
            BackgroundFit::Center => "center",
        }
    }
}

/// Resolved background-image settings.
#[derive(Clone, Debug)]
pub struct BackgroundConfig {
    pub path: Option<PathBuf>,
    pub opacity: f32,
    pub fit: BackgroundFit,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        BackgroundConfig {
            path: None,
            opacity: 0.35,
            fit: BackgroundFit::Cover,
        }
    }
}

struct SourceImage {
    rgba: RgbaImage,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct CacheKey {
    w: usize,
    h: usize,
    fit: BackgroundFit,
}

/// Decoded image plus pane-size render cache.
pub struct BackgroundImage {
    config: BackgroundConfig,
    source: Option<SourceImage>,
    cache: HashMap<CacheKey, Vec<u32>>,
}

impl BackgroundImage {
    pub fn load(config: BackgroundConfig) -> BackgroundImage {
        let source = config.path.as_deref().and_then(load_source);
        BackgroundImage {
            config,
            source,
            cache: HashMap::new(),
        }
    }

    pub fn config(&self) -> &BackgroundConfig {
        &self.config
    }

    pub fn is_active(&self) -> bool {
        self.source.is_some() && self.config.opacity > 0.0
    }

    pub fn paint_rect(
        &mut self,
        buf: &mut [u32],
        fb_w: u32,
        fb_h: u32,
        x: usize,
        y: usize,
        rw: usize,
        rh: usize,
    ) {
        if !self.is_active() || rw == 0 || rh == 0 {
            return;
        }
        let src = match self.rendered(rw, rh) {
            Some(src) => src,
            None => return,
        };
        let x1 = (x + rw).min(fb_w as usize);
        let y1 = (y + rh).min(fb_h as usize);
        for py in y..y1 {
            let dst_base = py * fb_w as usize;
            let src_base = (py - y) * rw;
            for px in x..x1 {
                let si = src_base + (px - x);
                let spx = src[si];
                if spx >> 24 != 0 {
                    let di = dst_base + px;
                    if di < buf.len() {
                        buf[di] = composite_over(buf[di], spx);
                    }
                }
            }
        }
    }

    fn rendered(&mut self, w: usize, h: usize) -> Option<&[u32]> {
        let key = CacheKey {
            w,
            h,
            fit: self.config.fit,
        };
        if !self.cache.contains_key(&key) {
            if self.cache.len() > 8 {
                self.cache.clear();
            }
            let pixels = self.render_pixels(w, h)?;
            self.cache.insert(key, pixels);
        }
        self.cache.get(&key).map(Vec::as_slice)
    }

    fn render_pixels(&self, w: usize, h: usize) -> Option<Vec<u32>> {
        let source = self.source.as_ref()?;
        let src_w = source.rgba.width() as usize;
        let src_h = source.rgba.height() as usize;
        if src_w == 0 || src_h == 0 || w == 0 || h == 0 {
            return None;
        }

        let mut canvas = vec![0u32; w * h];
        let opacity = self.config.opacity.clamp(0.0, 1.0);
        match self.config.fit {
            BackgroundFit::Stretch => {
                let resized =
                    imageops::resize(&source.rgba, w as u32, h as u32, FilterType::Triangle);
                copy_rgba(&mut canvas, w, h, &resized, 0, 0, opacity);
            }
            BackgroundFit::Cover | BackgroundFit::Contain => {
                let cover = matches!(self.config.fit, BackgroundFit::Cover);
                let (sw, sh) = scaled_size(src_w, src_h, w, h, cover);
                let resized =
                    imageops::resize(&source.rgba, sw as u32, sh as u32, FilterType::Triangle);
                let ox = (w as i32 - sw as i32) / 2;
                let oy = (h as i32 - sh as i32) / 2;
                copy_rgba(&mut canvas, w, h, &resized, ox, oy, opacity);
            }
            BackgroundFit::Center => {
                let ox = (w as i32 - src_w as i32) / 2;
                let oy = (h as i32 - src_h as i32) / 2;
                copy_rgba(&mut canvas, w, h, &source.rgba, ox, oy, opacity);
            }
            BackgroundFit::Tile => {
                tile_rgba(&mut canvas, w, h, &source.rgba, opacity);
            }
        }
        Some(canvas)
    }
}

fn load_source(path: &Path) -> Option<SourceImage> {
    match image::open(path) {
        Ok(img) => Some(SourceImage {
            rgba: img.into_rgba8(),
        }),
        Err(e) => {
            eprintln!(
                "[wslterm] background image load failed for {}: {e}",
                path.display()
            );
            None
        }
    }
}

fn scaled_size(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    cover: bool,
) -> (usize, usize) {
    let sx = dst_w as f32 / src_w as f32;
    let sy = dst_h as f32 / src_h as f32;
    let scale = if cover { sx.max(sy) } else { sx.min(sy) };
    let w = ((src_w as f32 * scale).round() as usize).max(1);
    let h = ((src_h as f32 * scale).round() as usize).max(1);
    (w, h)
}

fn copy_rgba(
    canvas: &mut [u32],
    dst_w: usize,
    dst_h: usize,
    rgba: &RgbaImage,
    off_x: i32,
    off_y: i32,
    opacity: f32,
) {
    let src_w = rgba.width() as usize;
    let src_h = rgba.height() as usize;
    let raw = rgba.as_raw();
    for sy in 0..src_h {
        let dy = off_y + sy as i32;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w {
            let dx = off_x + sx as i32;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let si = (sy * src_w + sx) * 4;
            canvas[dy as usize * dst_w + dx as usize] = rgba_to_argb(&raw[si..si + 4], opacity);
        }
    }
}

fn tile_rgba(canvas: &mut [u32], dst_w: usize, dst_h: usize, rgba: &RgbaImage, opacity: f32) {
    let src_w = rgba.width() as usize;
    let src_h = rgba.height() as usize;
    let raw = rgba.as_raw();
    for y in 0..dst_h {
        for x in 0..dst_w {
            let sx = x % src_w;
            let sy = y % src_h;
            let si = (sy * src_w + sx) * 4;
            canvas[y * dst_w + x] = rgba_to_argb(&raw[si..si + 4], opacity);
        }
    }
}

fn rgba_to_argb(px: &[u8], opacity: f32) -> u32 {
    let a = ((px[3] as f32 * opacity).round() as u32).min(255);
    (a << 24) | ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | px[2] as u32
}

fn composite_over(dst: u32, src: u32) -> u32 {
    let sa = (src >> 24) & 0xff;
    if sa == 0 {
        return dst;
    }
    if sa == 255 {
        return src;
    }
    let da = (dst >> 24) & 0xff;
    let inv = 255 - sa;
    let out_a = sa + (da * inv + 127) / 255;
    if out_a == 0 {
        return 0;
    }
    let blend = |shift: u32| {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        let num = s * sa * 255 + d * da * inv;
        let den = out_a * 255;
        (num + den / 2) / den
    };
    (out_a << 24) | (blend(16) << 16) | (blend(8) << 8) | blend(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_background_fit_values() {
        assert_eq!(BackgroundFit::parse("contain"), BackgroundFit::Contain);
        assert_eq!(BackgroundFit::parse("fill"), BackgroundFit::Stretch);
        assert_eq!(BackgroundFit::parse("tile"), BackgroundFit::Tile);
        assert_eq!(BackgroundFit::parse("unknown"), BackgroundFit::Cover);
    }
}
