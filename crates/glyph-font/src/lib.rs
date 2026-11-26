//! Run-time font loading and Slug quadratic-curve atlas (`out/WALL-PLAN-2026-11-26.md`, package C).
//!
//! Loads TrueType (`glyf`), variable TrueType (`fvar`/`gvar`), and OpenType CFF font files at
//! run time, extracts and normalises glyph outlines into the engine's Slug RGBA32Uint texture
//! layout (`assets/atlas/FORMAT.md`), and grows the backing textures on demand while reporting
//! exact dirty texel spans for incremental GPU upload.
//!
//! # Quick start
//!
//! ```no_run
//! use std::path::Path;
//! use glyph_font::{default_font_chain, Face, FontAtlas, MONASPACE_NEON_FAMILY};
//!
//! let specs = default_font_chain(Path::new("assets/fonts"));
//! let mut atlas = FontAtlas::load(&specs).expect("load default fonts");
//! let neon = atlas.family_id(MONASPACE_NEON_FAMILY).expect("monaspace-neon family");
//! let slot = atlas.glyph(neon, Face::Bold, 'A');
//! let dirty = atlas.take_dirty();
//! # let _ = (slot, dirty);
//! ```

pub mod atlas;
pub mod buffer;
pub mod constants;
pub mod encode;
pub mod families;
pub mod metrics;
pub mod raster;

pub use atlas::{Face, FamilyId, FontAtlas, FontError, Slot, SlotInfo};
pub use buffer::{texel_range_to_rects, DirtyUpdate, SlugBuffer, TexelRect, TextureBacking, TextureDirty};
pub use constants::{
    pack_u16, unpack_u16, BLANK_SLOT, CURVE_INITIAL_ROWS, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE,
    GLYPH_MODE_SPRITE, MAP_INITIAL_ROWS, TEXELS_PER_CURVE, TEXTURE_WIDTH, U32S_PER_CURVE,
    U32S_PER_TEXEL,
};
pub use encode::{
    compute_bbox, encode_glyph, encode_raw_curves, is_cff_face, reverse_curves, Curve, CurveBBox,
    EncodedGlyph, OutlineCollector, PackedCurve,
};
pub use families::{
    cousine_family_spec, default_font_chain, monaspace_family_spec, FaceSpec, FontSource, FontSpec,
    Variation, DEFAULT_FONT_FAMILY, MONASPACE_NEON_FAMILY,
};
pub use metrics::{
    compute_cell_metrics, derive_char_size, snap_cell_dimension, CharSize, FamilyMetrics,
    FontExtents, LineMetrics, RawLineMetrics, WorldCellMetrics, LETTER_SPACING, LINE_PITCH,
};
