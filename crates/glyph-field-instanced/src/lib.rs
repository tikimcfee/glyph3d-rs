//! The Instanced glyph field — [`glyph_field::GlyphFieldMode::Instanced`].
//!
//! One full placement record per glyph, the 32 B [`RenderSlot`], read as-is by
//! `shaders/glyph_field.wgsl` (`instances: array<InstanceSlot>`, binding 1).
//! Works for any glyph sizes and any placement; every edit is a partial write
//! of the affected slot bytes.
//!
//! Module map:
//! - [`slot`]: the `RenderSlot` record and its layout pins.
//! - [`upload`]: host records → device slots (transcode 48 B → 32 B, direct
//!   mapped write on unified memory or staging + copy on discrete).
//! - [`storage`]: the chunked slot buffers and per-slot byte addressing.
//! - [`pipeline`]: the bind group layout, glyph pipeline and mask pipeline.
//! - [`field`]: [`InstancedField`], the `GlyphField` implementation.
//!
//! Moved out of `native/src/glyph_scene/{instance,buffers,pipelines}.rs` when
//! the field split into modes (2026-10) — a pure move: same labels, same
//! upload paths, same pipeline state, and the WGSL byte-identical (git mv).

pub mod field;
pub mod pipeline;
pub mod slot;
pub mod storage;
pub mod upload;

pub use field::InstancedField;
pub use slot::RenderSlot;

/// The shader source, for validation tests and tooling.
pub const GLYPH_FIELD_WGSL: &str = include_str!("../shaders/glyph_field.wgsl");
