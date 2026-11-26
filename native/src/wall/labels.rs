//! World-space and screen-pixel text labels drawn through the cell pass's Slug shader.
//!
//! Used for terminal title bars, group headers, and map labels. All glyph quads
//! share the exact `fs_ink` fragment shader in `native/src/shaders/wall_cells.wgsl`
//! via `vs_label`, preserving Slug vector coverage, colour emoji sampling, and
//! exact world-rect clipping with zero duplicated outline code.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use glam::Vec2;
use glyph_font::{FamilyId, FontAtlas};

use super::cell::{GLYPH_ID_MASK, resolve_str_glyphs};
use super::metrics::{CellMetrics, WorldRect};

/// Line height specification for a [`Label`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LabelHeight {
    /// Fixed line height in world units (scales with camera zoom).
    World(f32),
    /// Fixed line height in physical screen pixels (constant visual size at any zoom).
    ScreenPx(f32),
    /// Fixed physical screen-pixel height capped at `max_world` world units
    /// (title bars stay constant pixel height until zooming far out).
    ScreenPxCapped { px: f32, max_world: f32 },
}

impl LabelHeight {
    /// Resolve this height specification to world units at `zoom` (physical px per world unit).
    pub fn resolve_world(self, zoom: f32) -> f32 {
        match self {
            Self::World(h) => {
                if h.is_finite() && h > 0.0 {
                    h
                } else {
                    0.0
                }
            }
            Self::ScreenPx(px) => {
                if px.is_finite() && px > 0.0 && zoom.is_finite() && zoom > 0.0 {
                    px / zoom
                } else {
                    0.0
                }
            }
            Self::ScreenPxCapped { px, max_world } => {
                if px.is_finite()
                    && px > 0.0
                    && max_world.is_finite()
                    && max_world > 0.0
                    && zoom.is_finite()
                    && zoom > 0.0
                {
                    (px / zoom).min(max_world)
                } else {
                    0.0
                }
            }
        }
    }
}

/// One single-line text label drawn in world space at a world `z`.
#[derive(Clone, Debug, PartialEq)]
pub struct Label<'a> {
    /// UTF-8 text to draw.
    pub text: Cow<'a, str>,
    /// Top-left corner of the text line in world space (+X right, +Y up).
    pub position: Vec2,
    /// Line height in world units or physical screen pixels.
    pub height: LabelHeight,
    /// Foreground colour (sRGB 0..1 RGBA).
    pub color: [f32; 4],
    /// World depth (larger `z` is nearer and covers smaller `z`).
    pub z: f32,
    /// Optional world-space clip rectangle; glyphs outside are skipped and
    /// glyphs crossing an edge are clipped at exact world boundaries.
    pub clip: Option<WorldRect>,
}

impl<'a> Label<'a> {
    /// Construct an unclipped label from borrowed or owned text.
    pub fn new(
        text: impl Into<Cow<'a, str>>,
        position: Vec2,
        height: LabelHeight,
        color: [f32; 4],
        z: f32,
    ) -> Self {
        Self {
            text: text.into(),
            position,
            height,
            color,
            z,
            clip: None,
        }
    }

    /// Clip this label to `rect` in world space.
    pub fn with_clip(mut self, rect: WorldRect) -> Self {
        self.clip = Some(rect);
        self
    }
}

/// Compact 48-byte per-glyph GPU instance read by `vs_label` in `wall_cells.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct LabelGlyphGpu {
    /// Unclipped glyph quad in world space: `[x0, y0, x1, y1]`.
    pub rect: [f32; 4],
    /// Clipped UV sub-window inside `[0, 1]^2`: `[u0, v0, u1, v1]`.
    pub uv_clip: [f32; 4],
    /// World depth.
    pub z: f32,
    /// Atlas glyph slot id.
    pub glyph_id: u32,
    /// Packed sRGB RGBA8 foreground colour (red in low byte, alpha in high byte).
    pub fg_rgba: u32,
    /// Column span (`1` = normal, `2` = wide / emoji).
    pub span_cols: u32,
}

/// Pack an sRGB `[f32; 4]` colour (`0.0..=1.0`) into RGBA8 (`red` in low byte).
pub fn pack_srgb_rgba8(color: [f32; 4]) -> u32 {
    let r = (color[0].clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (color[1].clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (color[2].clamp(0.0, 1.0) * 255.0).round() as u32;
    let a = (color[3].clamp(0.0, 1.0) * 255.0).round() as u32;
    r | (g << 8) | (b << 16) | (a << 24)
}

/// Measure the unclipped world width of `text` drawn at `height_world`.
pub fn measure_label_width(
    metrics: CellMetrics,
    atlas: &mut FontAtlas,
    family: FamilyId,
    text: &str,
    height_world: f32,
) -> f32 {
    if !height_world.is_finite() || height_world <= 0.0 || text.is_empty() {
        return 0.0;
    }
    let aspect = metrics.cell_width / metrics.cell_height.max(1e-6);
    let cell_w = height_world * aspect;
    let mut cols = 0u32;
    resolve_str_glyphs(atlas, family, text, |_gid, span| {
        cols += u32::from(span);
    });
    cols as f32 * cell_w
}

/// Expand `label` into clipped [`LabelGlyphGpu`] instances, invoking `emit` for
/// each visible non-blank glyph.
pub fn pack_label_glyphs(
    metrics: CellMetrics,
    atlas: &mut FontAtlas,
    family: FamilyId,
    label: &Label<'_>,
    zoom: f32,
    mut emit: impl FnMut(LabelGlyphGpu),
) {
    let height_world = label.height.resolve_world(zoom);
    if height_world <= 0.0 || label.color[3] <= 0.0 || label.text.is_empty() {
        return;
    }

    let aspect = metrics.cell_width / metrics.cell_height.max(1e-6);
    let cell_w = height_world * aspect;
    let y1 = label.position.y;
    let y0 = y1 - height_world;
    let fg_rgba = pack_srgb_rgba8(label.color);

    // Early vertical clip check for the whole single-line label.
    if let Some(clip) = label.clip {
        if y0 >= clip.max.y || y1 <= clip.min.y {
            return;
        }
    }

    let mut col = 0u32;
    resolve_str_glyphs(atlas, family, &label.text, |gid, span| {
        let span_u32 = u32::from(span);
        let x0 = label.position.x + col as f32 * cell_w;
        let x1 = label.position.x + (col + span_u32) as f32 * cell_w;
        col += span_u32;

        if gid == 0 {
            return;
        }

        let uv_clip = if let Some(clip) = label.clip {
            let nx0 = x0.max(clip.min.x);
            let nx1 = x1.min(clip.max.x);
            let ny0 = y0.max(clip.min.y);
            let ny1 = y1.min(clip.max.y);
            if nx0 >= nx1 || ny0 >= ny1 {
                return;
            }
            let w = (x1 - x0).max(1e-6);
            let h = (y1 - y0).max(1e-6);
            [
                (nx0 - x0) / w,
                (ny0 - y0) / h,
                (nx1 - x0) / w,
                (ny1 - y0) / h,
            ]
        } else {
            [0.0, 0.0, 1.0, 1.0]
        };

        emit(LabelGlyphGpu {
            rect: [x0, y0, x1, y1],
            uv_clip,
            z: label.z,
            glyph_id: gid & GLYPH_ID_MASK,
            fg_rgba,
            span_cols: span_u32,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_glyph_gpu_size_is_48_bytes() {
        assert_eq!(std::mem::size_of::<LabelGlyphGpu>(), 48);
    }

    #[test]
    fn label_height_resolves_world_screen_px_and_capped() {
        assert_eq!(LabelHeight::World(1.5).resolve_world(20.0), 1.5);
        assert!((LabelHeight::ScreenPx(12.0).resolve_world(24.0) - 0.5).abs() < 1e-6);
        // Uncapped when zoomed in (12 / 24 = 0.5 <= 2.0), capped when zoomed out (12 / 2 = 6.0 -> 2.0).
        let capped = LabelHeight::ScreenPxCapped {
            px: 12.0,
            max_world: 2.0,
        };
        assert!((capped.resolve_world(24.0) - 0.5).abs() < 1e-6);
        assert_eq!(capped.resolve_world(2.0), 2.0);
    }

    #[test]
    fn pack_label_glyphs_clips_partially_and_drops_outside_glyphs() {
        let specs = glyph_font::default_font_chain(&crate::wall::glyphs::default_fonts_dir());
        let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
        let family = atlas
            .family_id(glyph_font::MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());
        let fm = atlas.family_metrics(family);
        let raw_w = (f64::from(fm.advance_fu) * f64::from(crate::text::CELL_HEIGHT_WORLD)
            / fm.extents.em_height()) as f32;
        let metrics = CellMetrics::snapped(raw_w, crate::text::CELL_HEIGHT_WORLD);
        let cw = metrics.cell_width;
        // 4 characters "ABCD" at height 1.0 -> x spans [0, 4*cw], y spans [-1, 0].
        // Clip at x = [0.5*cw, 2.5*cw]: A is right-half clipped [0.5, 1.0], B is full [0, 1],
        // C is left-half clipped [0, 0.5], D is completely dropped.
        let label = Label::new("ABCD", Vec2::ZERO, LabelHeight::World(1.0), [1.0; 4], 3.0)
            .with_clip(WorldRect::new(Vec2::new(0.5 * cw, -1.0), Vec2::new(2.5 * cw, 0.0)));
        let mut out = Vec::new();
        pack_label_glyphs(metrics, &mut atlas, family, &label, 20.0, |g| out.push(g));
        assert_eq!(out.len(), 3, "D should be clipped out");
        assert!((out[0].uv_clip[0] - 0.5).abs() < 1e-4 && (out[0].uv_clip[2] - 1.0).abs() < 1e-4);
        assert_eq!(out[1].uv_clip, [0.0, 0.0, 1.0, 1.0]);
        assert!((out[2].uv_clip[0] - 0.0).abs() < 1e-4 && (out[2].uv_clip[2] - 0.5).abs() < 1e-4);

        let full_w = measure_label_width(metrics, &mut atlas, family, "ABCD", 1.0);
        assert!((full_w - 4.0 * cw).abs() < 1e-5);
    }
}
