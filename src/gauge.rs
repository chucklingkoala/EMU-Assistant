//! Jerry-can "fuel tank" gauge: the EMPTY can sprite with the FULL can sprite drawn
//! below the liquid surface (see `gauge_geom`). Space already used on the drive is the
//! base layer, this sync's share is stacked on top and gently pulses.

use eframe::egui::{self, epaint::Vertex, pos2, vec2, Color32, Mesh, Rect, RichText, TextureHandle};

use crate::fsutil::human_bytes;
use crate::gauge_geom::fill_quad;

pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0xdb, 0xe8);
const OVERFLOW_TINT: Color32 = Color32::from_rgb(255, 95, 110);
const USED_TINT: Color32 = Color32::from_rgb(150, 150, 175);

pub struct TankTextures {
    empty: TextureHandle,
    full: TextureHandle,
}

impl TankTextures {
    pub fn load(ctx: &egui::Context) -> Self {
        Self {
            empty: load_png(ctx, "tank_empty", include_bytes!("../assets/tank_empty.png"), None),
            full: load_png(ctx, "tank_full", include_bytes!("../assets/tank_full.png"), None),
        }
    }
}

/// Decodes an embedded PNG into a texture, optionally resized (square) for crisp small icons.
pub fn load_png(ctx: &egui::Context, name: &str, bytes: &[u8], size: Option<u32>) -> TextureHandle {
    let mut img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded PNG")
        .to_rgba8();
    if let Some(s) = size {
        img = image::imageops::resize(&img, s, s, image::imageops::FilterType::Lanczos3);
    }
    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
    ctx.load_texture(name, ci, egui::TextureOptions::LINEAR)
}

pub struct TankReading {
    /// e.g. `E:\ Games SSD`
    pub title: String,
    pub total: u64,
    pub used: u64,
    pub sync: u64,
    /// Upper estimate from the selection, not an analyzed plan.
    pub estimate: bool,
}

/// Draws the band of the FULL sprite between the liquid surfaces at `from` and `to`.
fn fill_band(painter: &egui::Painter, tex: &TextureHandle, rect: Rect, from: f32, to: f32, tint: Color32) {
    if to <= from {
        return;
    }
    let (lo, hi) = (fill_quad(from), fill_quad(to));
    let mut mesh = Mesh::with_texture(tex.id());
    let mut vert = |u: f32, v: f32| {
        let v = v.clamp(0.0, 1.0);
        mesh.vertices.push(Vertex {
            pos: pos2(rect.left() + u * rect.width(), rect.top() + v * rect.height()),
            uv: pos2(u, v),
            color: tint,
        });
    };
    // two quads: side face (left edge -> corner) and front face (corner -> right edge)
    for (u0, u1) in [(0.0, hi.corner.0), (hi.corner.0, 1.0)] {
        vert(u0, hi.v_at(u0));
        vert(u1, hi.v_at(u1));
        vert(u1, lo.v_at(u1));
        vert(u0, lo.v_at(u0));
    }
    for q in 0..2u32 {
        let b = q * 4;
        mesh.add_triangle(b, b + 1, b + 2);
        mesh.add_triangle(b, b + 2, b + 3);
    }
    painter.add(mesh);
}

fn level_text(level: f32) -> String {
    if level <= 0.005 {
        "EMPTY".into()
    } else if level >= 0.995 {
        "FULL".into()
    } else {
        format!("{:.0}%", level * 100.0)
    }
}

pub fn tank(ui: &mut egui::Ui, tex: &TankTextures, r: &TankReading, width: f32) {
    let size = tex.empty.size_vec2();
    let total = r.total.max(1) as f64;
    let after = r.used.saturating_add(r.sync);
    let overflow = after > r.total;
    let used_l = (r.used as f64 / total).min(1.0) as f32;
    let after_l = (after as f64 / total).min(1.0) as f32;

    ui.vertical_centered(|ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(width, width * size.y / size.x), egui::Sense::hover());
        let painter = ui.painter_at(rect.expand(2.0));
        let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(tex.empty.id(), rect, whole, Color32::WHITE);
        if overflow {
            fill_band(&painter, &tex.full, rect, 0.0, 1.0, OVERFLOW_TINT);
        } else {
            fill_band(&painter, &tex.full, rect, 0.0, used_l, USED_TINT);
            if r.sync > 0 {
                let t = ui.input(|i| i.time);
                let a = 0.72 + 0.28 * (t * 2.2).sin() as f32;
                let tint = Color32::from_white_alpha((a * 255.0) as u8);
                fill_band(&painter, &tex.full, rect, used_l, after_l.max(used_l + 0.01), tint);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        resp.on_hover_text(format!(
            "{}\nCapacity: {}\nUsed now: {}\nThis sync: +{}{}\nFree after: {}",
            r.title,
            human_bytes(r.total),
            human_bytes(r.used),
            human_bytes(r.sync),
            if r.estimate { " (estimate)" } else { "" },
            if overflow { format!("short by {}", human_bytes(after - r.total)) } else { human_bytes(r.total - after) }
        ));

        let (headline, color) = if overflow {
            ("WON'T FIT".to_string(), OVERFLOW_TINT)
        } else {
            (level_text(after_l), ACCENT)
        };
        ui.label(RichText::new(headline).size(20.0).strong().color(color));
        ui.label(RichText::new(&r.title).strong());
        let mut sync = format!("+{} this sync", human_bytes(r.sync));
        if r.estimate {
            sync += " (estimate)";
        }
        ui.label(RichText::new(sync).color(ACCENT));
        if overflow {
            ui.label(RichText::new(format!("short by {}", human_bytes(after - r.total))).color(OVERFLOW_TINT));
        } else {
            ui.label(RichText::new(format!("{} free after", human_bytes(r.total - after))).weak());
        }
    });
}
