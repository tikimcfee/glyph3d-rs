//! The renderer's transform tree (2026-10-10; step 1 of
//! `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md`, § "Groups: a pooled
//! transform tree").
//!
//! A node is a HANDLE into parallel tables, each with one writer — bevy's `Transform` / `GlobalTransform` / `Visibility`
//! split, outside bevy:
//!
//! | table | contents | writer |
//! |---|---|---|
//! | topology | parent, depth-first position | CPU ([`NodeTables`]) |
//! | local | translation + quaternion + uniform scale (32 B) | layout, drags, animation |
//! | post | per-axis scale, NOT inherited (16 B) | the same, for leaves |
//! | appearance | tint, alpha (inherited, multiplied), blend (32 B) | verbs, styling |
//! | world | the composed similarity + inherited alpha (32 B + 4 B) | the resolve only |
//!
//! The CPU owns the tree, the allocator and the resolve ([`tables`]): each
//! frame [`NodeTables::resolve_frame`] composes the world rows over the
//! dirty depth-first ranges and says which nodes it reached, so the
//! renderer rebuilds and uploads only the rows derived from them
//! ([`upload`]: coalesced writes or the whole table). A directory drag is
//! one local row written and one subtree resolved, a settled scene resolves
//! nothing. Until 2026-10-26 the resolve was a GPU compute pass over GPU
//! mirrors of the tables; it moved onto the CPU when the renderer dropped
//! compute from its default path (the makepad port draws with no compute
//! dispatch), and the crate no longer touches a device.
//!
//! ## Sources (techniques adapted, not code linked)
//!
//! - **bevy** (MIT OR Apache-2.0): the component split above; the two-level
//!   dirty bits and the sparse-upload policy of `AtomicSparseBufferVec`
//!   (`bevy_render/src/render_resource/sparse_buffer_vec.rs`, 0.16+);
//!   `set_parent` semantics for reparenting; and the top-down summation
//!   order of its transform propagation.
//! - **Unity Entities**: uniform scale in `LocalTransform`, anything else in
//!   a non-inherited `PostTransformMatrix` — here [`PostScale`].
//! - **slotmap** / Weissflog, "Handles are the better pointers" (2018):
//!   index + generation handles.
//!
//! `research/gpu-transform-hierarchies-2026-10.md` is the survey behind the
//! choices (written for the GPU resolve); the design note's "Step 1: as
//! built" section has the GPU form's measured cost, and `resolve-bench`
//! (`src/bin/resolve_bench.rs`) times the CPU resolve.

pub mod dirty;
pub mod tables;
pub mod transform;
pub mod upload;

pub use tables::{FramePlan, NodeError, NodeHandle, NodeTables, NONE};
pub use transform::{Appearance, PostScale, Similarity};
