//! The renderer's transform tree (2026-10-10; step 1 of
//! `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md`, § "Groups: a pooled
//! transform tree").
//!
//! A node is a HANDLE into parallel tables, each with one writer and its own
//! dirty tracking — bevy's `Transform` / `GlobalTransform` / `Visibility`
//! split, outside bevy:
//!
//! | table | contents | writer |
//! |---|---|---|
//! | topology | parent, depth-first position | CPU ([`NodeTables`]) |
//! | local | translation + quaternion + uniform scale (32 B) | layout, drags, animation |
//! | post | per-axis scale, NOT inherited (16 B) | the same, for leaves |
//! | appearance | tint, alpha (inherited, multiplied), blend (32 B) | verbs, styling |
//! | world | the composed similarity (32 B) | the GPU resolve pass only |
//!
//! The CPU owns the tree and the allocator ([`tables`]); each frame it
//! uploads only the rows that changed ([`upload`]: coalesced writes, a
//! scatter pass, or the whole table) and one compute pass derives the world
//! rows over the dirty depth-first ranges ([`gpu`]), skipped when nothing
//! moved. A directory drag is one local row up and one subtree resolved on
//! the GPU — not one world row per descendant, which is what an engine that
//! flattens on the CPU uploads.
//!
//! ## Sources (techniques adapted, not code linked)
//!
//! - **bevy** (MIT OR Apache-2.0): the component split above; the two-level
//!   dirty bits and the sparse-upload policy of `AtomicSparseBufferVec`
//!   (`bevy_render/src/render_resource/sparse_buffer_vec.rs`, 0.16+), and its
//!   scatter shader (`sparse_buffer_update.wesl`), which `shaders/scatter.wgsl`
//!   adapts; `set_parent` semantics for reparenting.
//! - **Wicked Engine** (MIT): `Scene::RunHierarchyUpdateSystem`
//!   (`wiScene.cpp`) resolves every node independently by walking its own
//!   parent chain; `shaders/resolve.wgsl` is that loop on the GPU.
//! - **Unity Entities**: uniform scale in `LocalTransform`, anything else in
//!   a non-inherited `PostTransformMatrix` — here [`PostScale`].
//! - **slotmap** / Weissflog, "Handles are the better pointers" (2018):
//!   index + generation handles; the frame-in-flight quarantine is the usual
//!   deferred free for GPU-visible rows.
//!
//! `research/gpu-transform-hierarchies-2026-10.md` is the survey behind the
//! choices; the design note's "Step 1: as built" section has the measured
//! cost (`resolve-bench`, `src/bin/resolve_bench.rs`).

pub mod dirty;
pub mod gpu;
pub mod tables;
pub mod transform;
pub mod upload;

pub use gpu::{FlushStats, SceneGraphGpu};
pub use tables::{FramePlan, NodeError, NodeHandle, NodeTables, NONE};
pub use transform::{Appearance, PostScale, Similarity};

/// The tables and their GPU mirror, flushed together.
pub struct SceneGraph {
    pub tables: NodeTables,
    pub gpu: SceneGraphGpu,
}

impl SceneGraph {
    pub fn new(device: &wgpu::Device) -> Self {
        Self { tables: NodeTables::default(), gpu: SceneGraphGpu::new(device) }
    }

    /// Upload this frame's edits and encode the resolve (if anything moved)
    /// into `encoder`, before any pass that reads the results.
    pub fn flush(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder) -> FlushStats {
        self.gpu.flush(device, queue, encoder, &mut self.tables)
    }
}
