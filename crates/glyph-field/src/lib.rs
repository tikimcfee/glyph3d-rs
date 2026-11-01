//! The glyph field's mode-neutral contract.
//!
//! A glyph field is the renderer's per-glyph storage plus the draw that reads
//! it. There is more than one way to lay that storage out, and they trade
//! memory and load time against how much the vertex stage has to derive:
//!
//! - [`GlyphFieldMode::Derived`] stores a compact record (20 B) and derives
//!   Y/Z in the vertex stage from per-item tables.
//!
//! One mode remains: the Instanced field, which stored every glyph's full
//! placement (32 B) for the vertex stage to read as-is, was retired on
//! 2026-10-31 (`glyph-field-instanced`, deleted). The contract stays a
//! contract, the seam a next mode lands in.
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
//! - [`GlyphFieldGpu`]: a field's draw on a wgpu device, built from the
//!   field by its mode (2026-10-31: the two were one object until then).
//! - [`SlotSource`] / [`transcode_host_slots`]: what a mode's slots are
//!   built from, and the host records' transcode into a mode's
//!   [`SlotRecord`] by its [`Transcode`].
//! - [`HostSlots`]: a field's slots kept on the host in blocks that are its
//!   chunks (the Derived field, 2026-10-26); every write lands there.
//! - [`FieldResources`] / [`FieldTargets`] / [`SlotChunk`] / [`SlotStorage`]:
//!   what a field's draw binds, and the device copy of its blocks, synced
//!   by block version.
//! - [`FieldCore`]: the glyph and mask pipelines, the per-chunk bind groups,
//!   draw recording and the slot dump; a mode adds its WGSL and any extra
//!   bindings through a [`FieldShape`].
//!
//! The one invariant every mode must keep: **each glyph is individually
//! addressable** — its color and its placement can be edited without
//! touching any other glyph. Both modes address it by slot.

mod field;
#[cfg(feature = "wgpu")]
mod field_core;
mod host_slots;
mod mode;
mod records;
#[cfg(feature = "wgpu")]
mod resources;
#[cfg(feature = "wgpu")]
mod storage;
mod transcode;

pub use field::{split_at_chunks, GlyphField};
#[cfg(feature = "wgpu")]
pub use field::GlyphFieldGpu;
#[cfg(feature = "wgpu")]
pub use field_core::{FieldCore, FieldShape};
pub use host_slots::HostSlots;
pub use mode::{GlyphFieldMode, ParseGlyphFieldModeError};
pub use records::{GlyphInstance, GlyphPlacement, GroupRow, ItemParamsGpu, LineRecord, NO_ITEM};
#[cfg(feature = "wgpu")]
pub use resources::{
    shared_bind_group_entries, shared_layout_entries, FieldResources, FieldTargets, SlotChunk,
    BINDING_FRAME_UNIFORM, BINDING_SLOTS,
};
#[cfg(feature = "wgpu")]
pub use storage::{SlotStorage, UploadLabels};
pub use transcode::{transcode_host_slots, SlotRecord, SlotSource, Transcode};

/// The host-block upload's batch: the staging wgpu holds for an upload in
/// flight is at most two of these (`SlotStorage::from_host_slots`). Here,
/// not in the wgpu storage, because the load's memory budget counts it.
pub const UPLOAD_BATCH_BYTES: usize = 256 * 1024 * 1024;
