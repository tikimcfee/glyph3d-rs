//! The Instanced glyph field — [`glyph_field::GlyphFieldMode::Instanced`].
//!
//! One full placement record per glyph, the 32 B [`RenderSlot`], read as-is by
//! `shaders/glyph_field.wgsl` (`instances: array<InstanceSlot>`, binding 1).
//! Works for any glyph sizes and any placement; every edit is a partial write
//! of the affected slot bytes.
//!
//! Module map:
//! - [`slot`]: the `RenderSlot` record and its layout pins.
//! - [`upload`]: the 48 B → 32 B transcode; the upload itself (direct mapped
//!   write on unified memory, staging + copy on discrete) and the chunked slot
//!   storage are `glyph_field`'s, shared with the Derived mode (C1).
//! - [`field`]: [`InstancedField`], the `GlyphField` implementation: this
//!   mode's slot writes over `glyph_field::FieldCore` (pipelines, chunk bind
//!   groups, draws).
//!
//! Moved out of `native/src/glyph_scene/{instance,buffers,pipelines}.rs` when
//! the field split into modes (2026-10) — a pure move: same labels, same
//! upload paths, same pipeline state, and the WGSL byte-identical (git mv).
//! The mode-neutral half then moved again, into `glyph_field` (C1, 2026-10-09),
//! with the same labels and state.

pub mod field;
pub mod slot;
pub mod upload;

pub use field::InstancedField;
pub use slot::RenderSlot;

/// The shader source, for validation tests and tooling.
pub const GLYPH_FIELD_WGSL: &str = include_str!("../shaders/glyph_field.wgsl");
