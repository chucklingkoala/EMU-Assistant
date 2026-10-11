//! Liquid-surface geometry for the jerry-can sprites, in normalized sprite
//! coordinates (u = 0..1 left to right, v = 0..1 top to bottom). Shared by the
//! gauge widget and the sprite preview tool, so the tuning lives in one place.
//!
//! The can is isometric: the liquid surface runs along the narrow left side face
//! and the wide front face and is lowest at the vertical corner between them.
//! The surface is parallel to the can's base, so the slopes were measured from the
//! bottom contour of `assets/tank_empty.png` (lowest opaque pixel per column) and the
//! fill range from the base corner up to the shoulder crease.

/// Horizontal position of the vertical corner between side and front face.
pub const CORNER_U: f32 = 0.41;
/// Corner height of the liquid surface at 0% (base) and just below 100% (shoulder).
pub const CORNER_V_EMPTY: f32 = 0.92;
pub const CORNER_V_FULL: f32 = 0.45;
/// How much higher the surface sits at the left / right sprite edge than at the corner
/// (side face slope ~0.36 v per u, front face slope ~0.39 v per u).
pub const RISE_LEFT: f32 = 0.146;
pub const RISE_RIGHT: f32 = 0.228;

/// Liquid surface: points (u, v) at the left edge, the corner and the right edge.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub left: (f32, f32),
    pub corner: (f32, f32),
    pub right: (f32, f32),
}

/// Fill level 0..=1 to surface. Levels <= 0 show no liquid and levels >= 1 fill the
/// whole sprite (handle included), matching the EMPTY / FULL artwork.
pub fn fill_quad(level: f32) -> Surface {
    let cv = if level <= 0.0 {
        2.0
    } else if level >= 1.0 {
        -1.0
    } else {
        CORNER_V_EMPTY + (CORNER_V_FULL - CORNER_V_EMPTY) * level
    };
    Surface { left: (0.0, cv - RISE_LEFT), corner: (CORNER_U, cv), right: (1.0, cv - RISE_RIGHT) }
}

impl Surface {
    /// Surface height at horizontal position `u`.
    pub fn v_at(&self, u: f32) -> f32 {
        let ((u0, v0), (u1, v1)) = if u <= self.corner.0 { (self.left, self.corner) } else { (self.corner, self.right) };
        v0 + (v1 - v0) * (u - u0) / (u1 - u0)
    }
}

/// True when (u, v) is below the liquid surface (used by the sprite preview tool).
#[allow(dead_code)]
pub fn inside(s: &Surface, u: f32, v: f32) -> bool {
    v >= s.v_at(u)
}
