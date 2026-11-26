//! Orthographic camera for the infinite terminal wall.
//!
//! Coordinates: world space is right-handed (+X right, +Y up, +Z toward the
//! camera). `zoom` is physical screen pixels per world unit (with cell height
//! 1.0, `zoom` equals screen pixels per cell row). Depth uses WebGPU reverse-Z
//! (`Depth32Float`, clear 0.0, `GreaterEqual`): a larger world `z` maps closer
//! to 1.0 and covers smaller `z`.

use glam::{Mat4, Vec2, Vec4};

use super::metrics::WorldRect;

/// Default world-Z bounds mapped onto NDC depth `[0, 1]` (reverse-Z).
pub const DEFAULT_Z_MIN: f32 = -1000.0;
pub const DEFAULT_Z_MAX: f32 = 1000.0;

/// Minimum and maximum allowed camera zoom (pixels per world unit).
pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 512.0;

/// Orthographic camera for the wall canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WallCamera {
    /// World point displayed at the centre of the viewport.
    pub center: Vec2,
    /// Physical screen pixels per world unit.
    pub zoom: f32,
    /// Farthest visible world `z` (maps to NDC depth 0.0).
    pub z_min: f32,
    /// Nearest visible world `z` (maps to NDC depth 1.0).
    pub z_max: f32,
}

impl Default for WallCamera {
    fn default() -> Self {
        Self {
            center: Vec2::ZERO,
            zoom: 24.0,
            z_min: DEFAULT_Z_MIN,
            z_max: DEFAULT_Z_MAX,
        }
    }
}

impl WallCamera {
    pub fn new(center: Vec2, zoom: f32) -> Self {
        Self {
            center,
            zoom: zoom.clamp(MIN_ZOOM, MAX_ZOOM),
            z_min: DEFAULT_Z_MIN,
            z_max: DEFAULT_Z_MAX,
        }
    }

    /// Frame `bounds` inside `viewport` (physical px) with `margin` (> 1 leaves
    /// breathing room around the rectangle).
    pub fn frame_rect(bounds: WorldRect, viewport: (u32, u32), margin: f32) -> Self {
        let vp_w = viewport.0.max(1) as f32;
        let vp_h = viewport.1.max(1) as f32;
        let m = margin.max(1.0);
        let w = bounds.width().max(1e-3) * m;
        let h = bounds.height().max(1e-3) * m;
        let zoom = (vp_w / w).min(vp_h / h);
        Self::new(bounds.center(), zoom)
    }

    /// Orthographic view-projection matrix for `viewport` (physical px),
    /// mapping `z ∈ [z_min, z_max]` linearly onto reverse-Z `[0.0, 1.0]`.
    pub fn view_proj(&self, viewport: (u32, u32)) -> Mat4 {
        let vp_w = viewport.0.max(1) as f32;
        let vp_h = viewport.1.max(1) as f32;
        let sx = 2.0 * self.zoom / vp_w;
        let sy = 2.0 * self.zoom / vp_h;
        let z_span = (self.z_max - self.z_min).max(1e-3);
        let sz = 1.0 / z_span;
        let tx = -self.center.x * sx;
        let ty = -self.center.y * sy;
        let tz = -self.z_min * sz;
        Mat4::from_cols(
            Vec4::new(sx, 0.0, 0.0, 0.0),
            Vec4::new(0.0, sy, 0.0, 0.0),
            Vec4::new(0.0, 0.0, sz, 0.0),
            Vec4::new(tx, ty, tz, 1.0),
        )
    }

    /// Project a world XY point into physical screen pixels (origin top-left, +Y down).
    pub fn world_to_screen(&self, world: Vec2, viewport: (u32, u32)) -> Vec2 {
        let vp_w = viewport.0.max(1) as f32;
        let vp_h = viewport.1.max(1) as f32;
        Vec2::new(
            (world.x - self.center.x) * self.zoom + vp_w * 0.5,
            (self.center.y - world.y) * self.zoom + vp_h * 0.5,
        )
    }

    /// Unproject a physical screen pixel (origin top-left, +Y down) into world XY.
    pub fn screen_to_world(&self, screen: Vec2, viewport: (u32, u32)) -> Vec2 {
        let vp_w = viewport.0.max(1) as f32;
        let vp_h = viewport.1.max(1) as f32;
        Vec2::new(
            self.center.x + (screen.x - vp_w * 0.5) / self.zoom,
            self.center.y - (screen.y - vp_h * 0.5) / self.zoom,
        )
    }

    /// Multiply `zoom` by `factor` while keeping the world point under `screen`
    /// fixed at the same pixel.
    pub fn zoom_about_screen_point(&mut self, screen: Vec2, factor: f32, viewport: (u32, u32)) {
        if !(factor > 0.0) || !factor.is_finite() {
            return;
        }
        let anchor = self.screen_to_world(screen, viewport);
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let vp_w = viewport.0.max(1) as f32;
        let vp_h = viewport.1.max(1) as f32;
        self.zoom = new_zoom;
        self.center = Vec2::new(
            anchor.x - (screen.x - vp_w * 0.5) / new_zoom,
            anchor.y + (screen.y - vp_h * 0.5) / new_zoom,
        );
    }
}
