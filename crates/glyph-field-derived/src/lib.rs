//! The Derived glyph field — [`glyph_field::GlyphFieldMode::Derived`].
//!
//! Compact 20 B [`DerivedSlot`] per glyph, Y and Z positions derived in the vertex stage
//! from line and item tables.

pub mod derive;
pub mod field;
pub mod pipeline;
pub mod slot;
pub mod upload;

pub use derive::{
    derive_yz, item_lane, item_of, override_lane, override_of, pack_row, resolve_group, row_of, x_page_of, ITEM_MAX,
    OVERRIDE_MAX, ROW_MAX, X_PAGE_MAX,
};
pub use field::DerivedField;
pub use slot::DerivedSlot;

pub const GLYPH_FIELD_DERIVED_WGSL: &str = include_str!("../shaders/glyph_field_derived.wgsl");
