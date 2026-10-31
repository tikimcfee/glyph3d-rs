//! The scene's GPU half (2026-10-31): every wgpu object the glyph scene
//! draws with, gathered in one place so the CPU scene holds none. A frame
//! is computed on the CPU (`frame.rs`, `FrameData`); `render.rs` uploads
//! what it changed into these and draws them. This is the same contract a
//! host renderer draws from (the makepad port), which never builds one.
//!
//! Subsystems move in one at a time; each keeps its CPU state on the scene
//! and its device objects here.

use std::cell::{Cell, RefCell};

use super::environment::EnvironmentGpu;
use super::mesh::MeshPipeline;
use super::target::CompositeState;

pub(crate) struct SceneGpu {
    /// For the pooled targets `set_viewport` (re)creates.
    pub(super) device: wgpu::Device,
    pub(super) environment: EnvironmentGpu,
    /// The frame uniform (`FrameData::uniform`), written every frame.
    pub(super) camera_buf: wgpu::Buffer,
    /// The glyph params (`FrameData::params`) and the generation it holds.
    pub(super) params_buf: wgpu::Buffer,
    pub(super) params_uploaded: Cell<u64>,
    /// The group table (96 B rows), written by the frame's runs.
    pub(super) group_buf: wgpu::Buffer,
    pub(super) mesh_pipeline: RefCell<MeshPipeline>,
    pub(super) mesh_frame_bg: wgpu::BindGroup,
    /// Keeps the emoji sheet's view alive for the field's bind groups.
    pub(super) _emoji_view: wgpu::TextureView,
    /// Stage L (L3): composite machinery — the ONLY release path (the
    /// --no-composite A/B escape hatch proved neutrality and was removed at
    /// stage end; see out/history/STAGE_L_REPORT.md).
    pub(super) composite: CompositeState,
}
