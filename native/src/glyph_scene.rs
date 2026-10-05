//! Stage C — glyph field scene: Slug pipeline, group table, text camera.
//! Stage F — fly camera + two-level CPU culling with LOD backdrops (the GPU
//! indirect-draw design is documented as broken-in-wgpu-30 below).
//! Stage G — CPU picking + live instance/group manipulation.
//!
//! Mode-agnostic like `scene.rs`: windowed and offscreen both build one
//! `GlyphScene` and call `render()` into their own target.
//!
//! ## Stage G pick/manipulation contract
//!
//! PICKING IS CPU-SIDE, deliberately: every glyph's geometry is derivable
//! deterministically (a picked file's records are re-produced by re-running
//! the engine with the SAME ItemParams it was staged with — bit-identical by
//! the Stage E2 verify discipline), the segment table already holds per-file
//! world AABBs, and ~1.3k ray-AABB tests cost microseconds. A GPU ID pass
//! would buy nothing at this scale and would re-enter the wgpu/Metal
//! indirect-draw morass documented below. Resolution order:
//!   screen px → ray (ANALYTIC, f64: camera basis + fov/aspect — never the
//!   inverse of the f32 view-proj, whose near/far conditioning is fatal at
//!   Fly's near=0.05/far≈1.7e6; see tools/repro_pick_oblique.py)
//!   → nearest visible file AABB →
//!   ray ∩ file plane (z = group offset.z) → local point (undo group TRS) →
//!   nearest record cell → (row, col) → source line/byte/char via
//!   text::fold_leaders, cross-checked against the engine's ROW/COL lanes.
//!
//! EDITS write through to the GPU with PARTIAL uploads only:
//!   - instance fields (color/pos/advance/height): queue.write_buffer into
//!     the affected arena chunk at slot granularity (32 B RenderSlot stride,
//!     4-aligned);
//!   - group TRS/color/alpha: one 80 B GroupRow write per edit, plus a CPU
//!     segment-table sync (AABB follows the group; backdrop tint follows the
//!     group color; hidden groups are skipped by the cull entirely).
//!
//! ## Stage F cull/LOD contract (see shaders/cull.wgsl header for the backdrop side)
//!
//! The staged instances are partitioned into SEGMENTS (one per file in repo
//! mode; a single cover segment for text scenes). Each frame, `cull_segments`
//! (CPU, ~1.3k AABBs ≈ microseconds) tests each segment's world AABB against
//! the frustum and its on-screen glyph size against LOD_MIN_PX, producing:
//!   - per-chunk vertex-range draw lists for visible segments
//!     (`pass.draw(0..6, base..base+count)` — instance_index stays
//!     chunk-local, exactly like the legacy per-chunk draws), and
//!   - a compacted backdrop list (flat quads: mean ink color × ink coverage)
//!     for segments whose whole em is below one screen pixel.
//!
//! Culling is visually lossless: a culled segment is entirely outside the
//! frustum, and the LOD tier only substitutes subpixel glyphs.
//!
//! Stage L (L2): the cull output is organized as PHASE LISTS (`enum Phase`
//! with `PhaseDraws` — re_renderer's DrawPhase borrow). Recording iterates
//! phases in declaration order (Backdrop, then Glyphs — exactly the order
//! above); behavior and profiler query names are unchanged.
//!
//! WHY CPU + DIRECT DRAWS: the original Stage F design was the standard
//! WebGPU pattern (compute cull pass → indirect multi-draws). It is
//! empirically BROKEN in wgpu 30.0.1's Metal backend: any indirect draw with
//! `first_instance != 0` rasterizes nothing (verified via GLYPH_CULL_DEBUG
//! readbacks — args correct — plus per-chunk/per-offset A/Bs; draws with
//! first_instance = 0 work, nonzero never do, through the indirect-validation
//! batcher). 1,305 tiny CPU AABB tests + direct range draws are ~free, fully
//! deterministic, and need no indirect machinery. The GPU compute path can
//! be revisited after a wgpu upgrade.

use glam::Vec3;
use std::cell::Cell;



use crate::gpu::GpuContext;
use crate::scene::SceneLike;


mod camera;
pub use camera::{CameraMode, FlyCamera, FOV_Y};
use camera::CamFrame;

mod cull;
pub use cull::{
    BackdropInst, BlockCull, SegCull, BACKDROP_GAIN, BLOCK_CULL_PAD_MAX, BLOCK_CULL_PAD_MIN,
    GLYPH_CELL_AREA, LOD_MIN_PX, PICK_AABB_PAD_Z, SEG_CULL_PAD_MAX, SEG_CULL_PAD_MIN,
    SUBSEG_BLOCK_SIZE,
};
use cull::CullState;

mod pick;
pub use pick::{PickCommand, PickContext, PickFileInfo, PickHit, Verb};
use pick::PickCacheEntry;

mod tint;
pub use tint::{SegTintAccum, seg_tint, srgb_to_linear_table};

mod instance;
pub use instance::{GlyphInstance, GroupRow, RenderSlot};
use instance::Params;

pub use glyph_field::{GlyphField, GlyphFieldMode, GlyphPlacement};

mod target;
use target::{CompositeState, Selection, ViewTarget, SCENE_SAMPLE_COUNT};

mod ui_probe;
pub use ui_probe::UiProbe;

mod interaction;
mod style;
mod setup;
mod pipelines;
mod render;
pub mod mesh;

pub struct GlyphScene {
    /// The glyph field: per-glyph slot storage, the pipeline that draws it,
    /// and the single-glyph verbs — behind the mode-neutral contract, so the
    /// scene never touches a slot's bytes or knows which mode it holds
    /// (2026-10 field-mode split; see `crates/glyph-field`). Storage is
    /// chunked (a repo-scale field exceeds one storage binding); the cull and
    /// pick slot math key on `field.chunk_capacity()`.
    pub(crate) field: Box<dyn GlyphField>,
    /// The colour-emoji sheet's view, held so the texture outlives the bind
    /// groups that sample it (binding 6 of every chunk's bind group).
    pub(crate) _emoji_view: wgpu::TextureView,
    pub(crate) camera_buf: wgpu::Buffer,
    pub(crate) depth_format: wgpu::TextureFormat,
    pub(crate) center: Vec3,
    pub(crate) half_w: f32,
    pub(crate) half_h: f32,
    /// Fit distance of the Front camera — scales the Fly camera's near/far
    /// and initial speed.
    pub(crate) fit: f32,
    pub(crate) camera_mode: CameraMode,
    pub(crate) fly: FlyCamera,
    /// Stage F: present unless culling is disabled (`--no-cull`); None keeps
    /// the legacy per-chunk draws.
    pub(in crate::glyph_scene) cull: Option<CullState>,
    // ── Stage G: picking & live manipulation ────────────────────────────
    /// Group table buffer, kept for partial per-row uploads (80 B/row).
    pub(crate) group_buf: wgpu::Buffer,
    /// CPU mirror of the group table — the pick path reads the LIVE TRS from
    /// here and every group verb writes it back (then uploads just that row).
    pub(crate) groups_cpu: Vec<GroupRow>,
    /// Repo-mode pick context (None for text/engine scenes).
    pub(crate) pick: Option<PickContext>,
    /// The panel's cluster-toggle seed for scenes WITHOUT a pick context
    /// (text): the staging choice carries the mode and nothing else on the
    /// scene remembers it. Repo scenes leave this None — their probe seeds
    /// from the pick context's uniform ItemParams (the two agree there).
    pub(crate) probe_cluster_mode: Option<bool>,
    /// The panel's layout mode seed for repo scenes.
    pub(crate) probe_layout_mode: Option<crate::repo::RepoLayoutMode>,
    /// The last resolved pick (verbs operate on it).
    pub(crate) picked: Option<PickHit>,
    /// Stage L (L4): the current selection (drives the mask pass). Replaces
    /// the Stage G click-flash hack (instance-byte write + restore) — no
    /// buffer writes, nothing to restore; the tint lives entirely in the
    /// windowed composite path.
    pub(in crate::glyph_scene) selection: Option<Selection>,
    /// Geometry overrides from nudge/scale-glyph verbs (slot → pos/advance/
    /// height), so a later recolor-line rebuild preserves them.
    pub(crate) geom_overrides: std::collections::HashMap<u32, ([f32; 3], f32, f32)>,
    /// One-entry cache of the last pick's re-derived file data.
    pub(in crate::glyph_scene) cache: Option<PickCacheEntry>,
    /// Windowed grab verb: the group being dragged with the mouse.
    pub(crate) grabbed_group: Option<u32>,
    /// Hierarchical layout controller and spatial scene graph (repo scenes).
    pub controller: Option<crate::layout_stack::LayoutController>,
    /// Windowed grab verb: the carrel/zone ID being dragged with the mouse.
    pub grabbed_zone: Option<String>,
    /// Last known cursor position, physical px (click pick + grab drag).
    pub(crate) cursor: (f32, f32),
    /// Viewport in physical px, refreshed every render() (ray unprojection).
    pub(crate) viewport: Cell<(u32, u32)>,
    /// Per-group position in the DIR_TINTS cycle (t verb).
    pub(crate) tint_step: Vec<u32>,
    /// Stage K: windowed debug-UI probe (None offscreen / under --no-ui).
    pub(crate) ui_probe: Option<UiProbe>,
    /// Stage L (L3): device handle for pool (re)creation in set_viewport
    /// (which has no ctx param — the trait shape is fenced).
    pub(crate) device: wgpu::Device,
    /// Stage L (L3): composite machinery — the ONLY release path (the
    /// --no-composite A/B escape hatch proved neutrality and was removed at
    /// stage end; see out/STAGE_L_REPORT.md).
    pub(in crate::glyph_scene) composite: CompositeState,
    pub(in crate::glyph_scene) params_buf: wgpu::Buffer,
    pub(in crate::glyph_scene) params: Cell<Params>,
    pub(crate) mesh_pipeline: std::cell::RefCell<mesh::MeshPipeline>,
    pub(crate) mesh_frame_bg: wgpu::BindGroup,
}

// ── Stage K (K4): what the live controls change, and what stays const ────
//
// LOD_MIN_PX becomes live in windowed runs via `CullState::lod_min_px`
// (a Cell seeded from the const — the `viewport: Cell` precedent for
// render(&self) immutability). The Debug-panel slider writes the shared
// probe cell; render() copies it into the Cell before culling. That copy is
// the SINGLE write site, and it runs only when a probe is installed
// (windowed) — offscreen never installs one, so offscreen culls with the
// const by construction (the byte-equal PNG gates prove it).
//
// BACKDROP_GAIN stays a compile-time const. The K4 handoff assumed it lived
// in the Params uniform — it does NOT: Params carries only the Slug
// minification dials (glyph_field.wgsl), while the gain is baked into
// SegCull.tint's alpha at STAGING time by seg_tint (text.rs/repo.rs share
// that path). A live gain would need either a cull.wgsl edit (fence 4) or
// an ink_frac plumbing redesign across staging + sync_segment. Cut from K4;
// the feasible future seam is recorded in out/STAGE_K_REPORT.md.

impl GlyphScene {

    /// Update slot colors in-place on the GPU.
    /// If direct-mapped GPU memory is available (e.g. Apple Silicon Metal),
    /// writes directly into host-visible mapped slots without queue uploads.
    /// Otherwise, dispatches wgpu queue write_buffer commands.
    fn camera_eye_target(&self, t: f32, aspect: f32) -> (Vec3, Vec3) {
        let fov = FOV_Y.to_radians();
        let half_h_needed = (self.half_h).max(self.half_w / aspect);
        let fit = half_h_needed / (fov * 0.5).tan() * 1.08 + 2.0;
        match self.camera_mode {
            CameraMode::Front { zoom } => {
                let d = fit / zoom.max(0.01);
                (self.center + Vec3::new(0.0, 0.0, d), self.center)
            }
            CameraMode::Orbit => {
                let r = fit * 1.05;
                let a = t * 0.12; // slow orbit
                (
                    self.center + Vec3::new(r * a.cos(), r * 0.18, r * a.sin()),
                    self.center,
                )
            }
            CameraMode::Fly => (self.fly.eye, self.fly.eye + self.fly.forward()),
        }
    }

    pub(in crate::glyph_scene) fn camera_frame(&self, t: f32, aspect: f32) -> CamFrame {
        let fov = FOV_Y.to_radians();
        let half_h_needed = (self.half_h).max(self.half_w / aspect);
        let fit = half_h_needed / (fov * 0.5).tan() * 1.08 + 2.0;
        let (near, far) = match self.camera_mode {
            CameraMode::Front { zoom } => {
                let d = fit / zoom.max(0.01);
                (d * 0.01, d * 20.0)
            }
            CameraMode::Orbit => {
                let r = fit * 1.05;
                (r * 0.01, r * 20.0)
            }
            CameraMode::Fly => {
                // Fly depth conditioning: keep near at 0.05 for single-glyph
                // closeups, while conditioning far to the scene bounds and distance
                // from the field center to prevent f32 depth precision collapse and Z-fighting.
                let d_center = (self.fly.eye - self.center).length();
                let far = (d_center + self.fit * 4.0).clamp(20_000.0, 100_000.0);
                (0.05, far)
            }
        };
        let (eye, target) = self.camera_eye_target(t, aspect);
        // Fly: build the view from the DIRECTION directly. look_at(eye,
        // eye+fwd) forms `eye - (eye+fwd)` in f32; with eye ~1e4 world units
        // that cancellation rounds the forward vector to ~1e-3 rad, rotating
        // the whole render away from fly.forward() — the pick ray derives
        // from the same direction, so both paths must skip the roundtrip.
        // (Oracle screenshots are all the Front camera, where eye−target is
        // exact; this changes nothing they cover.)
        let view = match self.camera_mode {
            CameraMode::Fly => {
                glam::camera::rh::view::look_to_mat4(eye, self.fly.forward(), Vec3::Y)
            }
            _ => glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y),
        };
        let proj = glam::camera::rh::proj::directx::perspective(fov, aspect, far, near);
        CamFrame {
            view_proj: proj * view,
            eye,
        }
    }

    // ── Stage G: picking & live manipulation ───────────────────────────────

    /// Group TRS from the CPU mirror: (offset, scale, rgb, alpha).
    pub(crate) fn group_trs(&self, gid: u32) -> Option<(Vec3, Vec3, [f32; 3], f32)> {
        let g = self.groups_cpu.get(gid as usize)?;
        Some((
            Vec3::new(g.cols[0][0], g.cols[0][1], g.cols[0][2]),
            Vec3::new(g.cols[3][0], g.cols[3][1], g.cols[3][2]),
            [g.cols[2][0], g.cols[2][1], g.cols[2][2]],
            g.cols[2][3],
        ))
    }

    /// Group Affine3A transform from the CPU mirror.
    pub(crate) fn group_affine(&self, gid: u32) -> Option<glam::Affine3A> {
        let g = self.groups_cpu.get(gid as usize)?;
        let t = Vec3::new(g.cols[0][0], g.cols[0][1], g.cols[0][2]);
        let q = glam::Quat::from_xyzw(g.cols[1][0], g.cols[1][1], g.cols[1][2], g.cols[1][3]);
        let q = if q.length_squared() > 1e-4 { q.normalize() } else { glam::Quat::IDENTITY };
        let s = Vec3::new(g.cols[3][0], g.cols[3][1], g.cols[3][2]);
        let s = if s.length_squared() > 1e-6 { s } else { Vec3::ONE };
        Some(glam::Affine3A::from_scale_rotation_translation(s, q, t))
    }

    pub(crate) fn group_hidden(&self, gid: u32) -> bool {
        self.group_trs(gid).is_some_and(|(_, _, _, a)| a <= 0.01)
    }

}

impl SceneLike for GlyphScene {
    fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }

    fn instance_count(&self) -> u32 {
        self.field.glyph_count()
    }

    fn on_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode, pressed: bool) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_key(key, pressed);
        }
        if pressed {
            self.verb_key(ctx, key);
        }
    }

    fn on_mouse_look(&mut self, _ctx: &GpuContext, dx: f32, dy: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_look(dx, dy);
        }
    }

    fn on_scroll(&mut self, ctx: &GpuContext, lines: f32) {
        // Stage G: scroll scales a grabbed group; otherwise camera speed.
        self.scroll_or_scale(ctx, lines);
    }

    fn on_cursor(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        self.cursor_moved(ctx, x, y);
    }

    fn on_click(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        if let Some(line) = self.click_pick(ctx, x, y) {
            println!("{line}");
        }
    }

    fn set_viewport(&mut self, w: u32, h: u32) {
        self.viewport.set((w, h));
        // Stage L (L3): (re)size the pooled view target to the viewport.
        // Both drivers call set_viewport before the first render (windowed:
        // on window creation and every resize; offscreen: once at startup).
        let comp = &mut self.composite;
        let stale = comp
            .target
            .as_ref()
            .is_none_or(|t| t.width != w || t.height != h);
        if stale && w > 0 && h > 0 {
            let target = ViewTarget::new(
                &self.device,
                w,
                h,
                &comp.bind_group_layout,
                &comp.sampler,
                comp.selection_fx.as_ref(), // Stage L (L4): mask + tint bind groups
            );
            comp.target = Some(target);
        }
    }

    fn set_cam_pose(&mut self, eye: [f32; 3], yaw: f32, pitch: f32) {
        GlyphScene::set_cam_pose(self, eye, yaw, pitch);
    }

    fn apply_pick(&mut self, ctx: &GpuContext, cmd: &PickCommand) -> Option<String> {
        GlyphScene::apply_pick(self, ctx, cmd)
    }

    fn apply_verb(&mut self, ctx: &GpuContext, verb: &Verb) -> Option<String> {
        Some(GlyphScene::apply_verb(self, ctx, verb))
    }

    fn apply_highlight_sidecar(&self, ctx: &GpuContext, path: &std::path::Path) -> Option<String> {
        Some(GlyphScene::apply_highlight_sidecar(self, ctx, path))
    }

    fn apply_surface_updates(
        &self,
        ctx: &GpuContext,
        updates: &[crate::seam::SurfaceUpdate],
    ) -> Option<String> {
        Some(GlyphScene::apply_surface_updates(self, ctx, updates))
    }

    fn debug_dump_instances(&self, ctx: &GpuContext, slot: u64, out: &mut [u32]) {
        self.field.read_slot_words(&ctx.device, &ctx.queue, slot as u32, out);
    }

    fn tick(&mut self, dt: f32) {
        if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.tick(dt);
        }
    }

    fn render(
        &self,
        ctx: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        target: &crate::scene::FrameTarget<'_>,
        t: f32,
    ) {
        render::render_scene(self, ctx, encoder, target, t);
    }
}

// ── Stage H (Phase 5) — encase layout assertions ────────────────────────────
// The WGSL lane maps (glyph_field.wgsl header: 12×4 B lanes; GROUP_STRIDE=5
