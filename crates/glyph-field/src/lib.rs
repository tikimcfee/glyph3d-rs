//! The glyph field's mode-neutral contract.
//!
//! A glyph field is the renderer's per-glyph storage plus the draw that reads
//! it. There is more than one way to lay that storage out, and they trade
//! memory and load time against how much the vertex stage has to derive:
//!
//! - [`GlyphFieldMode::Instanced`] stores every glyph's full placement
//!   (position, extent, glyph, color, group) — 32 B per glyph — and the
//!   vertex stage reads it as-is.
//! - [`GlyphFieldMode::Derived`] stores a compact record (20 B) and derives
//!   Y/Z in the vertex stage from per-item tables.
//! - [`GlyphFieldMode::Visible`] stores no record per glyph: the source bytes
//!   and a line table are resident, and the lines in view are laid out per
//!   frame on the GPU into a transient Derived-format buffer
//!   (`glyph-field-visible`).
//!
//! Each mode is its own crate with its own slot record, transcode, WGSL and
//! pipelines — no shader branches on the mode, and the modes' load flows never
//! share a code path that has to know which one it is in (the shared upload
//! is generic over the slot, not a branch on it). What they DO share lives
//! here:
//!
//! - [`GlyphField`]: the trait the scene draws and edits through.
//! - [`GlyphPlacement`]: the mode-neutral edit record (what a verb writes).
//! - [`GlyphInstance`] / [`GroupRow`]: the engine's neutral glyph record and
//!   the group table row every mode's shader reads.
//! - [`FieldResources`] / [`FieldTargets`] / [`SlotSource`] / [`SlotChunk`]:
//!   what a mode is built from.
//! - [`SlotStorage`] / [`upload_host_slots`]: the chunked slot buffers and the
//!   host upload, generic over a mode's [`SlotRecord`] and its [`Transcode`].
//! - [`FieldCore`]: the glyph and mask pipelines, the per-chunk bind groups,
//!   draw recording and the slot dump; a mode adds its WGSL and any extra
//!   bindings through a [`FieldShape`].
//!
//! The one invariant every mode must keep: **each glyph is individually
//! addressable** — its color and its placement can be edited without
//! touching any other glyph. The stored modes address it by slot; Visible,
//! whose slots are transient, by (item, byte) (M3, 2026-10-10), with the
//! edit applied by its layout kernel every frame.

mod copy;
mod field;
mod field_core;
mod mode;
mod records;
mod resources;
mod storage;
mod upload;

pub use copy::{copy_split, padded_staging_size, split_copy_size, FAST_COPY_ALIGN};
pub use field::{split_at_chunks, GlyphField};
pub use field_core::{FieldCore, FieldShape};
pub use mode::{GlyphFieldMode, ParseGlyphFieldModeError};
pub use records::{GlyphInstance, GlyphPlacement, GroupRow, ItemParamsGpu, LineRecord};
pub use resources::{
    shared_bind_group_entries, shared_layout_entries, FieldResources, FieldTargets, FramePrepare, SlotChunk,
    SlotSource, BINDING_FRAME_UNIFORM, BINDING_SLOTS,
};
pub use storage::{SlotRecord, SlotStorage};
pub use upload::{upload_host_slots, HostUpload, Transcode, UploadLabels};
#[cfg(target_os = "macos")]
pub use upload::create_mapped_slot_buffer;
