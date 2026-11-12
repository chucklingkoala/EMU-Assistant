//! Colours and the egui theme.

use eframe::egui::{self, Color32};

use crate::gauge::ACCENT;

// Palette: #1a1c28 base with the icon's cyan / purple / blue as highlights.
pub(super) const BASE: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x28);
pub(super) const DEEP: Color32 = Color32::from_rgb(0x12, 0x13, 0x1c);
pub(super) const FAINT: Color32 = Color32::from_rgb(0x20, 0x23, 0x33);
pub(super) const WIDGET: Color32 = Color32::from_rgb(0x26, 0x2a, 0x3d);
pub(super) const HOVER: Color32 = Color32::from_rgb(0x30, 0x35, 0x4d);
pub(super) const ACTIVE: Color32 = Color32::from_rgb(0x3a, 0x40, 0x60);
pub(super) const PURPLE: Color32 = Color32::from_rgb(0x8b, 0x5c, 0xf6);
pub(super) const BLUE: Color32 = Color32::from_rgb(0x5b, 0x8d, 0xef);
pub(super) const TEXT: Color32 = Color32::from_rgb(0xd8, 0xdc, 0xec);
pub(super) const WARN: Color32 = Color32::from_rgb(0xf5, 0xb9, 0x42);
pub(super) const BAD: Color32 = Color32::from_rgb(0xf0, 0x61, 0x6d);
pub(super) const GOOD: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);

pub(super) fn stroke(width: f32, color: Color32) -> egui::Stroke {
    egui::Stroke::new(width, color)
}

pub(super) fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BASE;
    v.window_fill = BASE;
    v.extreme_bg_color = DEEP;
    v.faint_bg_color = FAINT;
    v.code_bg_color = DEEP;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    v.window_stroke = stroke(1.0, ACTIVE);
    v.selection.bg_fill = PURPLE.linear_multiply(0.75);
    v.selection.stroke = stroke(1.0, ACCENT);
    let rounding = egui::Rounding::same(5.0);
    v.window_rounding = egui::Rounding::same(8.0);
    for (w, fill) in [
        (&mut v.widgets.noninteractive, BASE),
        (&mut v.widgets.inactive, WIDGET),
        (&mut v.widgets.hovered, HOVER),
        (&mut v.widgets.active, ACTIVE),
        (&mut v.widgets.open, ACTIVE),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.rounding = rounding;
    }
    v.widgets.noninteractive.bg_stroke = stroke(1.0, FAINT.gamma_multiply(1.6));
    v.widgets.noninteractive.fg_stroke = stroke(1.0, TEXT);
    v.widgets.inactive.fg_stroke = stroke(1.0, TEXT);
    v.widgets.hovered.bg_stroke = stroke(1.0, BLUE);
    v.widgets.hovered.fg_stroke = stroke(1.5, Color32::WHITE);
    v.widgets.active.bg_stroke = stroke(1.0, ACCENT);
    v.widgets.active.fg_stroke = stroke(2.0, ACCENT);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}
