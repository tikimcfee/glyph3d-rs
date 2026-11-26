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

    /// Reverse-Z NDC depth for world `z` (`z_min` → 0.0, `z_max` → 1.0).
    pub fn ndc_depth(&self, z: f32) -> f32 {
        let z_span = (self.z_max - self.z_min).max(1e-3);
        (z - self.z_min) / z_span
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

    /// Pan the canvas so world content shifts by `delta_px` physical screen pixels
    /// (+X right, +Y down).
    pub fn pan_by_screen_delta(&mut self, delta_px: Vec2) {
        self.center.x -= delta_px.x / self.zoom;
        self.center.y += delta_px.y / self.zoom;
    }

    /// World-space rectangle visible inside `viewport` (for 2D frustum culling).
    pub fn visible_world_rect(&self, viewport: (u32, u32)) -> WorldRect {
        let half_w = viewport.0.max(1) as f32 / (2.0 * self.zoom);
        let half_h = viewport.1.max(1) as f32 / (2.0 * self.zoom);
        WorldRect::new(
            Vec2::new(self.center.x - half_w, self.center.y - half_h),
            Vec2::new(self.center.x + half_w, self.center.y + half_h),
        )
    }

    /// On-screen height in physical pixels of a cell `cell_height_world` tall.
    pub fn cell_height_px(&self, cell_height_world: f32) -> f32 {
        self.zoom * cell_height_world
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_and_screen_conversions_round_trip() {
        let vp = (1600, 1000);
        let cam = WallCamera::new(Vec2::new(-42.5, 19.25), 31.5);
        for world in [
            Vec2::ZERO,
            cam.center,
            Vec2::new(-100.0, 55.0),
            Vec2::new(12.34, -78.9),
        ] {
            let screen = cam.world_to_screen(world, vp);
            let back = cam.screen_to_world(screen, vp);
            assert!(
                (back - world).length() < 1e-4,
                "world->screen->world drift: {world:?} -> {screen:?} -> {back:?}"
            );
        }
        assert_eq!(cam.world_to_screen(cam.center, vp), Vec2::new(800.0, 500.0));
    }

    #[test]
    fn view_proj_agrees_with_world_to_screen_and_reverse_z_depth() {
        let vp = (1280, 720);
        let cam = WallCamera::new(Vec2::new(15.0, -8.0), 20.0);
        let m = cam.view_proj(vp);

        let world = Vec2::new(27.5, -2.25);
        let clip = m * Vec4::new(world.x, world.y, 0.0, 1.0);
        assert_eq!(clip.w, 1.0, "orthographic clip.w is 1");
        let expected_px = cam.world_to_screen(world, vp);
        let from_ndc = Vec2::new(
            (clip.x * 0.5 + 0.5) * vp.0 as f32,
            (0.5 - clip.y * 0.5) * vp.1 as f32,
        );
        assert!(
            (from_ndc - expected_px).length() < 1e-4,
            "matrix {from_ndc:?} != world_to_screen {expected_px:?}"
        );

        // Reverse-Z: nearer (larger world z) maps to larger NDC depth inside [0, 1].
        let d_min = (m * Vec4::new(0.0, 0.0, cam.z_min, 1.0)).z;
        let d_far = (m * Vec4::new(0.0, 0.0, -5.0, 1.0)).z;
        let d_near = (m * Vec4::new(0.0, 0.0, 12.0, 1.0)).z;
        let d_max = (m * Vec4::new(0.0, 0.0, cam.z_max, 1.0)).z;
        assert!((d_min - 0.0).abs() < 1e-6);
        assert!((d_max - 1.0).abs() < 1e-6);
        assert!(d_near > d_far, "nearer z=12 ({d_near}) must beat farther z=-5 ({d_far})");
        assert!((d_near - cam.ndc_depth(12.0)).abs() < 1e-6);
    }

    #[test]
    fn zoom_about_screen_point_holds_world_anchor_fixed() {
        let vp = (1600, 1000);
        let mut cam = WallCamera::new(Vec2::new(10.0, -5.0), 24.0);
        let cursor = Vec2::new(1234.0, 275.0);
        let anchor_before = cam.screen_to_world(cursor, vp);

        cam.zoom_about_screen_point(cursor, 2.5, vp);
        assert!((cam.zoom - 60.0).abs() < 1e-4);
        let anchor_after = cam.screen_to_world(cursor, vp);
        assert!(
            (anchor_after - anchor_before).length() < 1e-4,
            "zoom-in moved anchor: {anchor_before:?} -> {anchor_after:?}"
        );
        let screen_after = cam.world_to_screen(anchor_before, vp);
        assert!((screen_after - cursor).length() < 1e-3);

        cam.zoom_about_screen_point(cursor, 0.2, vp);
        assert!((cam.zoom - 12.0).abs() < 1e-4);
        assert!((cam.screen_to_world(cursor, vp) - anchor_before).length() < 1e-4);

        // Extreme factor clamps cleanly without losing the anchor.
        cam.zoom_about_screen_point(cursor, 10_000.0, vp);
        assert_eq!(cam.zoom, MAX_ZOOM);
        assert!((cam.screen_to_world(cursor, vp) - anchor_before).length() < 1e-3);
    }

    #[test]
    fn pan_and_frame_rect_behave_as_expected() {
        let vp = (1000, 600);
        let mut cam = WallCamera::new(Vec2::new(5.0, 5.0), 25.0);
        let world_pt = Vec2::new(9.0, 2.0);
        let before = cam.world_to_screen(world_pt, vp);
        let drag = Vec2::new(75.0, -40.0);
        cam.pan_by_screen_delta(drag);
        let after = cam.world_to_screen(world_pt, vp);
        assert!(((after - before) - drag).length() < 1e-4);

        let rect = WorldRect::new(Vec2::new(-10.0, -20.0), Vec2::new(30.0, 4.0));
        let framed = WallCamera::frame_rect(rect, vp, 1.2);
        let visible = framed.visible_world_rect(vp);
        assert!(visible.contains(rect.min));
        assert!(visible.contains(rect.max));
        assert!((framed.center - rect.center()).length() < 1e-4);
    }
}
