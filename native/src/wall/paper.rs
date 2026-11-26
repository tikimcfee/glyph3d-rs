//! Drafting-paper canvas backdrop (`WallPaper.js` port).
//!
//! Computes discrete minor/major grid visibility through [`wall_core::snap::grid_levels`]
//! (hard switch at `min_px`, never a fade) and packs the camera-reduced uniform
//! consumed by `native/src/shaders/wall_paper.wgsl`.

use bytemuck::{Pod, Zeroable};
use wall_core::snap::{GridLevels, GridSpec, grid_levels};

use crate::config::WallSettings;

use super::camera::WallCamera;

/// Drafting paper style: canvas clear colour and discrete minor/major grid spec.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaperStyle {
    /// Whether to draw grid lines over `background`.
    pub show: bool,
    /// Canvas background colour (sRGB 0..1 RGBA).
    pub background: [f32; 4],
    /// Minor grid line colour (sRGB 0..1 RGBA).
    pub minor_color: [f32; 4],
    /// Major grid line colour (sRGB 0..1 RGBA).
    pub major_color: [f32; 4],
    /// World units between adjacent minor lines.
    pub minor_spacing: f32,
    /// Every Nth minor line is a major line (`1` makes every line major).
    pub major_every: u32,
    /// Line thickness in physical screen pixels (rounded to at least 1 px).
    pub line_px: f32,
    /// Hard-switch visibility floor in screen pixels (`grid_levels`).
    pub min_px: f32,
}

impl From<&WallSettings> for PaperStyle {
    fn from(s: &WallSettings) -> Self {
        Self {
            show: s.paper_show,
            background: s.paper_background,
            minor_color: s.paper_minor_color,
            major_color: s.paper_major_color,
            minor_spacing: s.paper_minor_spacing,
            major_every: s.paper_major_every,
            line_px: s.paper_line_px,
            min_px: s.paper_min_px,
        }
    }
}

impl Default for PaperStyle {
    fn default() -> Self {
        Self::from(&crate::config::settings().wall)
    }
}

impl PaperStyle {
    /// Build a [`wall_core::snap::GridSpec`] for this paper style.
    pub fn grid_spec(&self) -> GridSpec {
        GridSpec::new(
            f64::from(self.minor_spacing),
            self.major_every.max(1),
            f64::from(self.min_px),
        )
    }

    /// Decide which grid levels draw at `zoom` (pixels per world unit),
    /// delegating to [`wall_core::snap::grid_levels`].
    pub fn levels_at(&self, zoom: f32) -> GridLevels {
        grid_levels(self.grid_spec(), f64::from(zoom))
    }
}

/// GPU uniform block for `native/src/shaders/wall_paper.wgsl` (80 B).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct WallPaperUniform {
    /// Camera centre reduced modulo `major_step` in `f64` so float coordinates stay small.
    pub origin: [f32; 2],
    /// Half the viewport size in physical pixels.
    pub half_buf: [f32; 2],
    /// Physical pixels per world unit.
    pub zoom: f32,
    /// Half of the snapped grid line width in physical pixels.
    pub half_line: f32,
    /// Minor line spacing in world units (`0.0` when hidden).
    pub minor_step: f32,
    /// Major line spacing in world units (`0.0` when hidden).
    pub major_step: f32,
    /// Minor line sRGB RGBA.
    pub minor_color: [f32; 4],
    /// Major line sRGB RGBA.
    pub major_color: [f32; 4],
    /// Non-zero when the colour target is sRGB (shader decodes sRGB to linear).
    pub target_is_srgb: u32,
    pub _pad0: u32,
    pub _pad1: u32,
    pub _pad2: u32,
}

/// Build the paper shader uniform for `camera` and `viewport`, or `None` when
/// the grid is disabled or both levels sit below `min_px`.
pub fn build_paper_uniform(
    paper: &PaperStyle,
    camera: &WallCamera,
    viewport: (u32, u32),
    target_is_srgb: bool,
) -> Option<WallPaperUniform> {
    if !paper.show
        || !paper.minor_spacing.is_finite()
        || paper.minor_spacing <= 0.0
        || !camera.zoom.is_finite()
        || camera.zoom <= 0.0
    {
        return None;
    }

    let levels = paper.levels_at(camera.zoom);
    if !levels.minor && !levels.major {
        return None;
    }

    let major_every = paper.major_every.max(1);
    let major_step_f64 = f64::from(paper.minor_spacing) * f64::from(major_every);
    let origin_x = f64::from(camera.center.x).rem_euclid(major_step_f64) as f32;
    let origin_y = f64::from(camera.center.y).rem_euclid(major_step_f64) as f32;
    let half_line = paper.line_px.round().max(1.0) * 0.5;

    Some(WallPaperUniform {
        origin: [origin_x, origin_y],
        half_buf: [viewport.0.max(1) as f32 * 0.5, viewport.1.max(1) as f32 * 0.5],
        zoom: camera.zoom,
        half_line,
        minor_step: if levels.minor { paper.minor_spacing } else { 0.0 },
        major_step: if levels.major { major_step_f64 as f32 } else { 0.0 },
        minor_color: paper.minor_color,
        major_color: paper.major_color,
        target_is_srgb: u32::from(target_is_srgb),
        _pad0: 0,
        _pad1: 0,
        _pad2: 0,
    })
}

#[cfg(test)]
mod tests {
    use glam::Vec2;

    use super::*;

    #[test]
    fn paper_uniform_size_is_80_bytes() {
        assert_eq!(std::mem::size_of::<WallPaperUniform>(), 80);
    }

    #[test]
    fn paper_hard_switches_minor_and_major_levels_using_wall_core_snap() {
        let paper = PaperStyle {
            show: true,
            minor_spacing: 4.0,
            major_every: 5,
            min_px: 6.0,
            line_px: 1.0,
            ..PaperStyle::default()
        };
        let vp = (1600, 1000);

        // Both levels visible: minor_px = 4 * 2 = 8 >= 6, major_px = 40 >= 6.
        let both = build_paper_uniform(&paper, &WallCamera::new(Vec2::ZERO, 2.0), vp, true)
            .expect("both levels visible");
        assert_eq!(both.minor_step, 4.0);
        assert_eq!(both.major_step, 20.0);
        assert_eq!(both.half_line, 0.5);

        // Minor just below floor (4 * 1.49 = 5.96 < 6): minor hard-switches off, major stays on.
        let major_only = build_paper_uniform(&paper, &WallCamera::new(Vec2::ZERO, 1.49), vp, true)
            .expect("major still visible");
        assert_eq!(major_only.minor_step, 0.0);
        assert_eq!(major_only.major_step, 20.0);

        // Major just below floor (20 * 0.29 = 5.8 < 6): whole grid hidden.
        assert!(build_paper_uniform(&paper, &WallCamera::new(Vec2::ZERO, 0.29), vp, true).is_none());

        // Grid explicitly disabled -> None even when zoomed in.
        let off = PaperStyle { show: false, ..paper };
        assert!(build_paper_uniform(&off, &WallCamera::new(Vec2::ZERO, 24.0), vp, true).is_none());
    }

    #[test]
    fn paper_origin_reduces_large_world_coordinates_modulo_major_step() {
        let paper = PaperStyle {
            minor_spacing: 4.0,
            major_every: 5,
            ..PaperStyle::default()
        };
        // Major step = 20.0. 100_003.5 mod 20 = 3.5; -27.25 mod 20 = 12.75.
        let cam = WallCamera::new(Vec2::new(100_003.5, -27.25), 10.0);
        let u = build_paper_uniform(&paper, &cam, (800, 600), true).expect("grid draws");
        assert!((u.origin[0] - 3.5).abs() < 1e-4, "origin.x={}", u.origin[0]);
        assert!((u.origin[1] - 12.75).abs() < 1e-4, "origin.y={}", u.origin[1]);
    }
}
