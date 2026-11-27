//! Cell and line metrics of a font family, matching `core/cellMetrics.js`,
//! `shaping/fontTables.js`, and `shaping/FontChain.js`.

use ttf_parser::Face;

use crate::encode::{compute_bbox, OutlineCollector};
use crate::sprites::SpriteMetrics;

/// Multiplier of cell height for the line pitch (1.0 = contiguous rows).
pub const LINE_PITCH: f32 = 1.0;

/// Multiplier of cell width for inter-glyph tracking (0.0 = contiguous columns).
pub const LETTER_SPACING: f32 = 0.0;

/// Vertical horizontal-layout extents of a font face, in font units (y up from baseline).
///
/// Mirrors `HarfBuzzShaper.fontExtents()`: when `include_line_gap` is true and `line_gap > 0`,
/// half the line gap is added above the ascender and half below the descender.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontExtents {
    pub ascender: f64,
    pub descender: f64,
    pub line_gap: f64,
}

impl FontExtents {
    /// Read horizontal extents from a parsed `ttf_parser::Face` (`hhea` table).
    pub fn from_face(face: &Face<'_>, include_line_gap: bool) -> Self {
        let asc = face.ascender() as f64;
        let desc = face.descender() as f64;
        let gap = face.line_gap() as f64;
        if include_line_gap && gap > 0.0 {
            let half = gap * 0.5;
            Self {
                ascender: asc + half,
                descender: desc - half,
                line_gap: gap,
            }
        } else {
            Self {
                ascender: asc,
                descender: desc,
                line_gap: gap,
            }
        }
    }

    /// Total vertical em span (`ascender - descender`) in font units.
    #[inline]
    pub fn em_height(self) -> f64 {
        self.ascender - self.descender
    }

    /// Scale the cell's vertical em by `line_height_scale` (`1.0` = unchanged), distributing
    /// extra leading (or tightening) symmetrically above the ascender and below the descender
    /// so glyph outlines keep their true aspect ratio and stay vertically centered.
    pub fn with_line_height(self, line_height_scale: f64) -> Self {
        let scale = if line_height_scale.is_finite() && line_height_scale > 0.0 {
            line_height_scale
        } else {
            1.0
        };
        if (scale - 1.0).abs() <= 1e-6 {
            return self;
        }
        let extra = self.em_height() * (scale - 1.0);
        Self {
            ascender: self.ascender + extra * 0.5,
            descender: self.descender - extra * 0.5,
            line_gap: self.line_gap,
        }
    }
}

/// Raw underline / strikethrough / x-height values read from a font's `post` and `OS/2` tables,
/// in font units (y up from the baseline), matching `shaping/fontTables.js`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawLineMetrics {
    /// Top edge of the underline (`post.underlinePosition`), in font units.
    pub underline_top_fu: i16,
    /// Thickness of the underline (`post.underlineThickness`), in font units.
    pub underline_thickness_fu: i16,
    /// Top edge of the strikethrough (`OS/2.yStrikeoutPosition`), if the font has an `OS/2` table.
    pub strikeout_top_fu: Option<i16>,
    /// Thickness of the strikethrough (`OS/2.yStrikeoutSize`), if the font has an `OS/2` table.
    pub strikeout_size_fu: Option<i16>,
    /// x-height (`OS/2.sxHeight`, OS/2 version >= 2), if present.
    pub x_height_fu: Option<i16>,
}

impl RawLineMetrics {
    /// Read raw line metrics from `post` and `OS/2` tables.
    pub fn from_face(face: &Face<'_>) -> Option<Self> {
        let ul = face.underline_metrics()?;
        let st = face.strikeout_metrics();
        Some(Self {
            underline_top_fu: ul.position,
            underline_thickness_fu: ul.thickness,
            strikeout_top_fu: st.map(|m| m.position),
            strikeout_size_fu: st.map(|m| m.thickness),
            x_height_fu: face.x_height(),
        })
    }
}

/// Where a family draws its underline and strikethrough, in normalised cell coordinates
/// (`y` up from the cell bottom at `0.0` to the cell top at `1.0`), matching
/// `FontChain.lineMetrics()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    /// Centre Y of the underline in `[0, 1]` cell units.
    pub underline_y: f32,
    /// Centre Y of the strikethrough in `[0, 1]` cell units.
    pub strike_y: f32,
    /// Line thickness (the font's underline thickness divided by the cell's vertical em).
    pub thickness: f32,
    /// Strikethrough thickness divided by the cell's vertical em (falls back to `thickness`).
    pub strike_thickness: f32,
    /// Baseline Y in `[0, 1]` cell units (`-descender / em_height`).
    pub baseline_y: f32,
    /// Raw font-unit values read from `post` and `OS/2`.
    pub raw: RawLineMetrics,
}

impl LineMetrics {
    /// Compute normalised cell-space line metrics from raw table values and font extents.
    pub fn compute(raw: RawLineMetrics, extents: FontExtents) -> Option<Self> {
        let em = extents.em_height();
        if raw.underline_thickness_fu <= 0 || em <= 0.0 {
            return None;
        }
        let to_cell_y = |units: f64| -> f32 {
            ((units - extents.descender) / em).clamp(0.0, 1.0) as f32
        };
        let ul_center = raw.underline_top_fu as f64 - raw.underline_thickness_fu as f64 * 0.5;
        let strike_center = match (raw.strikeout_top_fu, raw.strikeout_size_fu) {
            (Some(top), Some(size)) if size > 0 => top as f64 - size as f64 * 0.5,
            _ => match raw.x_height_fu {
                Some(xh) => xh as f64 * 0.5,
                None => extents.ascender * 0.3,
            },
        };
        let thickness = (raw.underline_thickness_fu as f64 / em) as f32;
        let strike_thickness = match raw.strikeout_size_fu {
            Some(size) if size > 0 => (size as f64 / em) as f32,
            _ => thickness,
        };
        Some(Self {
            underline_y: to_cell_y(ul_center),
            strike_y: to_cell_y(strike_center),
            thickness,
            strike_thickness,
            baseline_y: to_cell_y(0.0),
            raw,
        })
    }
}

/// Complete cell and line metrics for a font family's primary font.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FamilyMetrics {
    /// Primary font units per em (e.g. 2048 for Cousine, 2000 for Monaspace Neon).
    pub upem: u16,
    /// Forced monospace cell advance in primary font units (`'M'` advance, else `0.6 * upem`).
    pub advance_fu: i32,
    /// Vertical extents in primary font units (with `line_gap` applied if configured).
    pub extents: FontExtents,
    /// Cell aspect ratio: `advance_fu / em_height`.
    pub aspect: f32,
    /// Underline and strikethrough placement in cell units (`None` if the font lacks `post` metrics).
    pub lines: Option<LineMetrics>,
    /// Cell aspect and box-drawing line weights for generating this family's cell sprites.
    pub sprites: SpriteMetrics,
}

impl FamilyMetrics {
    /// Extract a family's metrics from its primary font face.
    pub fn from_face(face: &Face<'_>, include_line_gap: bool) -> Self {
        Self::from_face_scaled(face, include_line_gap, 1.0)
    }

    /// Extract a family's metrics from its primary font face with a vertical line-height multiplier.
    pub fn from_face_scaled(
        face: &Face<'_>,
        include_line_gap: bool,
        line_height_scale: f64,
    ) -> Self {
        let upem = face.units_per_em();
        let extents =
            FontExtents::from_face(face, include_line_gap).with_line_height(line_height_scale);
        let advance_fu = face
            .glyph_index('M')
            .and_then(|gid| face.glyph_hor_advance(gid))
            .map(|adv| adv as i32)
            .unwrap_or_else(|| ((upem as f64) * 0.6).round() as i32);
        let em = extents.em_height();
        let aspect_f64 = if em > 0.0 {
            advance_fu as f64 / em
        } else {
            0.6
        };
        let raw_lines = RawLineMetrics::from_face(face);
        let lines = raw_lines.and_then(|raw| LineMetrics::compute(raw, extents));
        let sprites = compute_sprite_metrics(face, advance_fu, extents, raw_lines);
        Self {
            upem,
            advance_fu,
            extents,
            aspect: aspect_f64 as f32,
            lines,
            sprites,
        }
    }

    /// Cell pixel size `(width, height)` at `font_size_px`, matching `deriveCharSize` in
    /// `core/cellMetrics.js`.
    pub fn char_size(&self, font_size_px: f32) -> CharSize {
        derive_char_size(self.upem, self.advance_fu, self.extents, font_size_px)
    }

    /// Snapped world-space cell metrics at `font_size_px` and `world_scale`, matching
    /// `computeCellMetrics` in `core/cellMetrics.js`.
    pub fn world_cell_metrics(&self, font_size_px: f32, world_scale: f32) -> WorldCellMetrics {
        compute_cell_metrics(self.char_size(font_size_px), world_scale)
    }
}

/// Measure a horizontal box-drawing glyph's stroke thickness (`max_y - min_y` of its control hull),
/// matching `FontChain.spriteMetrics()`.
fn box_stroke_thickness_fu(face: &Face<'_>, ch: char) -> f64 {
    let Some(gid) = face.glyph_index(ch) else {
        return 0.0;
    };
    let mut collector = OutlineCollector::new(1.0);
    if face.outline_glyph(gid, &mut collector).is_none() {
        return 0.0;
    }
    let (curves, _) = collector.into_curves();
    if curves.is_empty() {
        return 0.0;
    }
    let bbox = compute_bbox(&curves);
    (bbox.max_y - bbox.min_y).max(0.0)
}

/// Compute `SpriteMetrics` from the family's primary font (`FontChain.spriteMetrics()`):
/// light is the thickness of `─` (else `post.underlineThickness`, else 5% of em);
/// heavy is the thickness of `━` (else twice light).
pub fn compute_sprite_metrics(
    face: &Face<'_>,
    advance_fu: i32,
    extents: FontExtents,
    raw_lines: Option<RawLineMetrics>,
) -> SpriteMetrics {
    let em = extents.em_height().max(1.0);
    let aspect = (advance_fu as f64 / em).max(0.1);
    let light_from_box = box_stroke_thickness_fu(face, '─');
    let light_fu = if light_from_box > 0.0 {
        light_from_box
    } else if let Some(ul) = raw_lines.filter(|r| r.underline_thickness_fu > 0) {
        ul.underline_thickness_fu as f64
    } else {
        em * 0.05
    };
    let heavy_from_box = box_stroke_thickness_fu(face, '━');
    let heavy_fu = if heavy_from_box > 0.0 {
        heavy_from_box
    } else {
        2.0 * light_fu
    };
    SpriteMetrics {
        aspect,
        light: light_fu / em,
        heavy: heavy_fu / em,
    }
}

/// Un-snapped cell dimensions in pixels for a given font size (`deriveCharSize`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharSize {
    pub width: f32,
    pub height: f32,
}

/// Snapped world-space cell dimensions (`computeCellMetrics`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldCellMetrics {
    pub char_width: f32,
    pub char_height: f32,
    pub line_spacing: f32,
    pub letter_spacing: f32,
    pub pixel_width: f32,
    pub pixel_height: f32,
}

/// Compute cell pixel size from font metrics and font size (`core/cellMetrics.js::deriveCharSize`).
pub fn derive_char_size(
    upem: u16,
    advance_fu: i32,
    extents: FontExtents,
    font_size_px: f32,
) -> CharSize {
    let upem_f = if upem > 0 { upem as f64 } else { 2048.0 };
    let fs = font_size_px as f64;
    let width = ((advance_fu as f64 / upem_f) * fs).ceil() as f32;
    let height = ((extents.em_height() / upem_f) * fs) as f32;
    CharSize { width, height }
}

/// Round a positive world-space cell dimension to 10 significant bits (error < 0.1%).
///
/// Mirrors `snapCellDimension` in `core/cellMetrics.js`: column `k`'s right edge
/// `(k * w + w)` and column `k + 1`'s left edge `((k + 1) * w)` become bit-identical
/// `f32` values for all `k < 16384`, eliminating cracks and double-blended seams between
/// adjacent cells at any zoom level.
pub fn snap_cell_dimension(v: f32) -> f32 {
    if !v.is_finite() || v <= 0.0 {
        return v;
    }
    let vf = v as f64;
    let exp = vf.log2().floor() as i32 - 9;
    let q = 2.0_f64.powi(exp);
    ((vf / q).round() * q) as f32
}

/// Compute snapped world-space cell metrics (`core/cellMetrics.js::computeCellMetrics`).
pub fn compute_cell_metrics(char_size: CharSize, world_scale: f32) -> WorldCellMetrics {
    let char_width = snap_cell_dimension(char_size.width * world_scale);
    let char_height = snap_cell_dimension(char_size.height * world_scale);
    WorldCellMetrics {
        char_width,
        char_height,
        line_spacing: char_height * LINE_PITCH,
        letter_spacing: char_width * LETTER_SPACING,
        pixel_width: char_size.width,
        pixel_height: char_size.height,
    }
}
