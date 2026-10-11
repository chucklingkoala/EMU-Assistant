//! Cuts the EMPTY and FULL jerry cans out of `emu tank.jpg` into transparent sprites
//! (`assets/tank_empty.png`, `assets/tank_full.png`) and renders a preview strip.
//!
//! cargo run --example make_tank_sprites [-- --measure]

#[path = "../src/gauge_geom.rs"]
mod gauge_geom;

use image::{Rgba, RgbaImage};

const SRC: &str = "emu tank.jpg";
/// Background colour of the artwork.
const BG: [f32; 3] = [27.0, 29.0, 41.0];
/// Horizontal ranges of the EMPTY and FULL columns in the source image.
const EMPTY_COLS: (u32, u32) = (40, 400);
const FULL_COLS: (u32, u32) = (1000, 1380);
/// Captions start below this row.
const MAX_Y: u32 = 500;

fn main() {
    let src = image::open(SRC).expect("open emu tank.jpg").to_rgb8();
    let measure = std::env::args().any(|a| a == "--measure");

    let bb_e = bbox(&src, EMPTY_COLS);
    let bb_f = bbox(&src, FULL_COLS);
    println!("empty bbox {bb_e:?}\nfull  bbox {bb_f:?}");
    if measure {
        return;
    }

    // same-sized crops, aligned on each can's bounding-box centre / bottom
    let w = (bb_e.2 - bb_e.0).max(bb_f.2 - bb_f.0) + 2 * PAD;
    let h = (bb_e.3 - bb_e.1).max(bb_f.3 - bb_f.1) + 2 * PAD;
    let empty = crop_alpha(&src, bb_e, w, h);
    let mut full = crop_alpha(&src, bb_f, w, h);
    // The fill only covers the can body: drop the FULL can's outer glow (the EMPTY can's
    // glow is always drawn underneath), otherwise the cut surface shows a halo streak.
    for (x, y, p) in full.enumerate_pixels_mut() {
        let body = empty.get_pixel(x, y).0[3] as f32 / 255.0;
        let m = ((body - 0.62) / 0.12).clamp(0.0, 1.0);
        p.0[3] = (p.0[3] as f32 * m).round() as u8;
    }
    std::fs::create_dir_all("assets").unwrap();
    empty.save("assets/tank_empty.png").unwrap();
    full.save("assets/tank_full.png").unwrap();
    println!("sprites {w}x{h} written to assets/");

    // preview: 0%, 30%, 70%, 100% composited on the app background
    let levels = [0.0, 0.3, 0.7, 1.0];
    let mut strip = RgbaImage::from_pixel(w * levels.len() as u32, h, Rgba([0x1a, 0x1c, 0x28, 255]));
    for (i, lvl) in levels.iter().enumerate() {
        let ox = i as u32 * w;
        blend_all(&mut strip, &empty, ox, |_, _| true);
        let poly = gauge_geom::fill_quad(*lvl);
        blend_all(&mut strip, &full, ox, |u, v| gauge_geom::inside(&poly, u, v));
    }
    strip.save("target/tank_preview.png").unwrap();
    println!("preview written to target/tank_preview.png");
}

const PAD: u32 = 24;

/// Bounding box (x0, y0, x1, y1) of non-background pixels within a column range.
fn bbox(img: &image::RgbImage, cols: (u32, u32)) -> (u32, u32, u32, u32) {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..MAX_Y.min(img.height()) {
        for x in cols.0..cols.1.min(img.width()) {
            let p = img.get_pixel(x, y).0;
            let d: f32 = (0..3).map(|c| (p[c] as f32 - BG[c]).abs()).fold(0.0, f32::max);
            if d > 40.0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    (x0, y0, x1, y1)
}

/// Crops a w*h box centred on the bounding box and keys the background to alpha
/// (GIMP-style "colour to alpha"), keeping soft glows.
fn crop_alpha(src: &image::RgbImage, bb: (u32, u32, u32, u32), w: u32, h: u32) -> RgbaImage {
    let cx = (bb.0 + bb.2) / 2;
    let cy = (bb.1 + bb.3) / 2;
    let left = cx as i64 - w as i64 / 2;
    let top = cy as i64 - h as i64 / 2;
    let mut out = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (left + x as i64, top + y as i64);
            if sx < 0 || sy < 0 || sx >= src.width() as i64 || sy >= MAX_Y as i64 {
                continue;
            }
            let p = src.get_pixel(sx as u32, sy as u32).0;
            let mut a: f32 = 0.0;
            for c in 0..3 {
                let v = p[c] as f32;
                let ac = if v > BG[c] { (v - BG[c]) / (255.0 - BG[c]) } else { (BG[c] - v) / BG[c] };
                a = a.max(ac);
            }
            // drop JPEG noise around the background
            let a = ((a - 0.03) / 0.97).clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let mut rgb = [0u8; 3];
            for c in 0..3 {
                rgb[c] = ((p[c] as f32 - BG[c]) / a + BG[c]).round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(x, y, Rgba([rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8]));
        }
    }
    out
}

fn blend_all(dst: &mut RgbaImage, src: &RgbaImage, ox: u32, mask: impl Fn(f32, f32) -> bool) {
    let (w, h) = src.dimensions();
    for y in 0..h {
        for x in 0..w {
            if !mask(x as f32 / w as f32, y as f32 / h as f32) {
                continue;
            }
            let s = src.get_pixel(x, y).0;
            let a = s[3] as f32 / 255.0;
            let d = dst.get_pixel_mut(ox + x, y);
            for c in 0..3 {
                d.0[c] = (s[c] as f32 * a + d.0[c] as f32 * (1.0 - a)).round() as u8;
            }
        }
    }
}
