//! The glyph field's mode-neutral contract.
//!
//! A glyph field is the renderer's per-glyph storage plus the draw that reads
//! it. There is more than one way to lay that storage out, and they trade
//! memory and load time against how much the vertex stage has to derive:
//!
//! - [`GlyphFieldMode::Instanced`] stores every glyph's full placement
//!   (position, extent, glyph, color, group) — 32 B per glyph — and the
//!   vertex stage reads it as-is.
//! - [`GlyphFieldMode::Derived`] (planned) stores a compact record and derives
//!   the rest of the placement in the vertex stage.
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
//!
//! The one invariant every mode must keep: **each glyph is individually
//! addressable** by its slot — its color and its placement can be edited
//! without touching any other glyph.

mod copy;
mod field;
mod mode;
mod records;
mod resources;
mod storage;
mod upload;

pub use copy::{copy_split, padded_staging_size, split_copy_size, FAST_COPY_ALIGN};
pub use field::{split_at_chunks, GlyphField};
pub use mode::{GlyphFieldMode, ParseGlyphFieldModeError};
pub use records::{GlyphInstance, GlyphPlacement, GroupRow, ItemParamsGpu, LineRecord};
pub use resources::{
    shared_bind_group_entries, shared_layout_entries, FieldResources, FieldTargets, SlotChunk,
    SlotSource, BINDING_FRAME_UNIFORM, BINDING_SLOTS,
};
pub use storage::{SlotRecord, SlotStorage};
pub use upload::{upload_host_slots, HostUpload, Transcode, UploadLabels};
