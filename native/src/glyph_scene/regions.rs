//! The library's bounding regions (2026-10-10; `[library] regions`, the
//! retired JS renderer's `ContentTreeMarkers`): one translucent box per
//! directory below the root, enclosing its whole subtree — its pages and
//! every child directory — so you can SEE what contains what and where it
//! sits. Clicking inside a box (not on a file) picks the innermost
//! directory there, and `g` drags it with everything in it.
//!
//! - **The box is the tree cull's.** `TreeCull` already keeps every library
//!   node's subtree box in its own frame, refit along the ancestors of
//!   whatever moved; a region is its directory's box padded
//!   (`region_box`). The cull logs the slots each refit rewrites
//!   (`TreeCull::log_refits`), and `RegionTable::sync` re-pads exactly
//!   those regions — so the host's work per frame is O(refit directories),
//!   never O(directories), and a settled library does none. When regions
//!   are on, each sheet's box also takes its page face
//!   (`TreeCull::with_own_box`), so a box encloses the pages, not only the
//!   ink on them.
//! - **The world is the GPU's.** The box stays in its directory's frame;
//!   a compute pass (`shaders/library_regions.wgsl`) composes it with the
//!   directory's resolved world after any resolve or box upload, so a drag,
//!   an ease, a page turn or a form change moves every region for free.
//!   `region_instance` is its CPU twin (`tests`).
//! - **Draw order.** After the page faces, before the glyphs; depth tested,
//!   not written; shallowest first (the JS `BACKDROP_BASE + depth`). A
//!   page hides the box faces behind it, a box face in front of a page
//!   tints it, and the glyphs, drawn last, read over every face.
//! - **Pick.** Content out-picks regions (a file under the ray wins, as the
//!   JS group channel lost to cards); otherwise `RegionTable::pick` walks
//!   the directory tree top-down, the ray against each region in its
//!   directory's frame, descending only into regions it enters, and keeps
//!   the deepest (the nearest entry among equals; on an exact tie — two
//!   siblings sharing a front plane — the later drawn). A child region is
//!   reached through
//!   its parent's: the padding shrinks with depth so a child's walls sit
//!   inside its parent's, and only a child scaled up past its parent pokes
//!   out (reachable where it overlaps).
//! - **Which directories.** Every `Plan::dirs` entry but the root (path
//!   ''), whose box would be the whole library — the JS skipped it too.
//!   Single-child chains are already compressed by the plan
//!   (`plan::build_tree`), so no pass-through directory duplicates its
//!   tail's box.
//!
//! The colour is a gradient over nesting depth (`[library] region_color_a`
//! at depth 1 to `region_color_b` at the deepest directory in the tree),
//! the JS recipe; depth counts boxes (a compressed chain is one level).

use std::cell::{Cell, RefCell};

use bytemuck::{Pod, Zeroable};
use glam::{DQuat, DVec3};
use glyph_scene_graph::{NodeHandle, NodeTables, Similarity, NONE};

use super::tree_cull::{is_empty, Aabb, TreeCull};
use crate::library::plan::Plan;
use crate::library::LibNodes;

/// Every `[library] region_*` dial, colours linear.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RegionSpec {
    pub pad: f32,
    pub z_pad: f32,
    pub pad_step: f32,
    pub pad_min: f32,
    pub min_thickness: f32,
    pub color_a: [f32; 3],
    pub color_b: [f32; 3],
    pub opacity: f32,
    pub opacity_decay: f32,
    pub min_opacity: f32,
    pub edge_opacity: f32,
    pub edge_px: f32,
    pub picked_opacity: f32,
    pub picked_edge_opacity: f32,
}

impl RegionSpec {
    /// None when `[library] regions` is off.
    pub(super) fn from_settings(s: &crate::config::LibrarySettings) -> Option<Self> {
        s.regions.then_some(RegionSpec {
            pad: s.region_pad,
            z_pad: s.region_z_pad,
            pad_step: s.region_pad_step,
            pad_min: s.region_pad_min,
            min_thickness: s.region_min_thickness,
            color_a: s.region_color_a,
            color_b: s.region_color_b,
            opacity: s.region_opacity,
            opacity_decay: s.region_opacity_decay,
            min_opacity: s.region_min_opacity,
            edge_opacity: s.region_edge_opacity,
            edge_px: s.region_edge_px,
            picked_opacity: s.region_picked_opacity,
            picked_edge_opacity: s.region_picked_edge_opacity,
        })
    }
}

/// The region of a directory at nesting `depth` (1: a child of the root)
/// whose subtree box, in its own frame, is `b`: padded `pad` in x/y and
/// `z_pad` in z, each `pad_step` less per level below the first (never
/// under `pad_min`, nor over the level-1 pad), and at least
/// `min_thickness` deep about its centre. None for an empty subtree.
pub(super) fn region_box(b: &Aabb, depth: u32, spec: &RegionSpec) -> Option<Aabb> {
    if is_empty(b) {
        return None;
    }
    let shrink = spec.pad_step * depth.saturating_sub(1) as f32;
    let p = (spec.pad - shrink).max(spec.pad_min.min(spec.pad));
    let pz = (spec.z_pad - shrink).max(spec.pad_min.min(spec.z_pad));
    let mut lo = [b[0][0] - p, b[0][1] - p, b[0][2] - pz];
    let mut hi = [b[1][0] + p, b[1][1] + p, b[1][2] + pz];
    if hi[2] - lo[2] < spec.min_thickness {
        let c = (lo[2] + hi[2]) * 0.5;
        lo[2] = c - spec.min_thickness * 0.5;
        hi[2] = c + spec.min_thickness * 0.5;
    }
    Some([lo, hi])
}

/// A region's colour at `depth` of a tree whose deepest region is at
/// `max_depth`: linear rgb on the gradient, fill opacity in alpha.
pub(super) fn region_color(depth: u32, max_depth: u32, spec: &RegionSpec) -> [f32; 4] {
    let t = if max_depth > 1 { (depth.max(1) - 1) as f32 / (max_depth - 1) as f32 } else { 0.0 };
    let c = [0, 1, 2].map(|k| spec.color_a[k] + (spec.color_b[k] - spec.color_a[k]) * t);
    let a = (spec.opacity * spec.opacity_decay.powi(depth.max(1) as i32 - 1)).max(spec.min_opacity);
    [c[0], c[1], c[2], a]
}

/// `Region` in library_regions.wgsl, 48 B.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
struct RegionGpu {
    lo: [f32; 4],
    hi: [f32; 4],
    color: [f32; 4],
}

/// `RegionInstance` in library_regions.wgsl, 64 B.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub(super) struct RegionInstance {
    pub col0: [f32; 4],
    pub col1: [f32; 4],
    pub col2: [f32; 4],
    pub color: [f32; 4],
}

/// `RegionParams` in library_regions.wgsl, 32 B.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RegionParamsGpu {
    edge_opacity: f32,
    edge_px: f32,
    picked_opacity: f32,
    picked_edge_opacity: f32,
    picked: u32,
    count: u32,
    _pad: [u32; 2],
}

const _: () = assert!(std::mem::size_of::<RegionGpu>() == 48, "Region in library_regions.wgsl is 48 B");
const _: () = assert!(std::mem::size_of::<RegionInstance>() == 64, "RegionInstance in library_regions.wgsl is 64 B");
const _: () = assert!(std::mem::size_of::<RegionParamsGpu>() == 32, "RegionParams in library_regions.wgsl is 32 B");

/// The region box `lo..hi` (its directory's frame) under the directory's
/// world `w`: library_regions.wgsl's `write_regions`, term for term.
#[cfg(test)]
pub(super) fn region_instance(w: &Similarity, lo: [f32; 3], hi: [f32; 3], color: [f32; 4]) -> RegionInstance {
    let (s, q) = (w.scale, w.rotation);
    let size = [0, 1, 2].map(|k| hi[k] - lo[k]);
    let centre = [0, 1, 2].map(|k| (lo[k] + hi[k]) * 0.5);
    let (x2, y2, z2) = (q[0] + q[0], q[1] + q[1], q[2] + q[2]);
    let (xx, xy, xz) = (q[0] * x2, q[0] * y2, q[0] * z2);
    let (yy, yz, zz) = (q[1] * y2, q[1] * z2, q[2] * z2);
    let (wx, wy, wz) = (q[3] * x2, q[3] * y2, q[3] * z2);
    let mx = [(1.0 - (yy + zz)) * s, (xy + wz) * s, (xz - wy) * s];
    let my = [(xy - wz) * s, (1.0 - (xx + zz)) * s, (yz + wx) * s];
    let mz = [(xz + wy) * s, (yz - wx) * s, (1.0 - (xx + yy)) * s];
    let r = glyph_scene_graph::transform::rotate(q, centre.map(|c| c * s));
    let at = [0, 1, 2].map(|k| w.translation[k] + r[k]);
    RegionInstance {
        col0: [mx[0] * size[0], mx[1] * size[0], mx[2] * size[0], at[0]],
        col1: [my[0] * size[1], my[1] * size[1], my[2] * size[1], at[1]],
        col2: [mz[0] * size[2], mz[1] * size[2], mz[2] * size[2], at[2]],
        color,
    }
}

/// The ray `ro + t·rd` in the frame of a node whose world is `w` (same t).
fn ray_to_local(w: &Similarity, ro: DVec3, rd: DVec3) -> (DVec3, DVec3) {
    let q = w.rotation;
    let inv = DQuat::from_xyzw(f64::from(q[0]), f64::from(q[1]), f64::from(q[2]), f64::from(q[3])).inverse();
    let s = f64::from(w.scale);
    let t = DVec3::new(f64::from(w.translation[0]), f64::from(w.translation[1]), f64::from(w.translation[2]));
    ((inv * (ro - t)) / s, (inv * rd) / s)
}

/// The regions of one library, host side: which directory each draws,
/// their boxes, and the pick. Draw order: shallowest first, then plan
/// order (pre-order).
pub(super) struct RegionTable {
    spec: RegionSpec,
    /// Per region: its `Plan::dirs` index, node slot, depth (1 = a child
    /// of the root), colour.
    dir: Vec<u32>,
    slot: Vec<u32>,
    depth: Vec<u32>,
    color: Vec<[f32; 4]>,
    /// Per region: its box in its directory's frame (None: empty subtree).
    boxes: Vec<Option<Aabb>>,
    /// Per region: the row the GPU reads (an empty box is a zero box).
    rows: Vec<RegionGpu>,
    /// `Plan::dirs` index → region (NONE: the root).
    region_of_dir: Vec<u32>,
    /// Pool slot → region (NONE: not a region's directory).
    region_of_slot: Vec<u32>,
    /// `Plan::dirs` index → its child directories (the pick's descent).
    kids: Vec<Vec<u32>>,
    /// Every directory's node (`LibNodes::dir`).
    dir_nodes: Vec<NodeHandle>,
    /// Regions whose row changed since the last `take_dirty`.
    dirty: Vec<u32>,
    dirty_mark: Vec<bool>,
    scratch: Vec<u32>,
}

impl RegionTable {
    /// The regions of `plan` (attached as `nodes`), boxed from `tree`.
    pub(super) fn new(plan: &Plan, nodes: &LibNodes, tree: &TreeCull, spec: RegionSpec) -> Self {
        let n_dirs = plan.dirs.len();
        let mut depth_of = vec![0u32; n_dirs];
        let mut kids = vec![Vec::new(); n_dirs];
        for (i, d) in plan.dirs.iter().enumerate() {
            if let Some(p) = d.parent {
                depth_of[i] = depth_of[p] + 1;
                kids[p].push(i as u32);
            }
        }
        let mut order: Vec<u32> = (0..n_dirs as u32).filter(|&i| plan.dirs[i as usize].parent.is_some()).collect();
        order.sort_by_key(|&i| (depth_of[i as usize], i));
        let max_depth = order.iter().map(|&i| depth_of[i as usize]).max().unwrap_or(1);
        let slot_count = nodes.dir.iter().map(|h| h.index() + 1).max().unwrap_or(0) as usize;
        let mut region_of_dir = vec![NONE; n_dirs];
        let mut region_of_slot = vec![NONE; slot_count];
        let mut this = RegionTable {
            spec,
            dir: Vec::with_capacity(order.len()),
            slot: Vec::with_capacity(order.len()),
            depth: Vec::with_capacity(order.len()),
            color: Vec::with_capacity(order.len()),
            boxes: Vec::with_capacity(order.len()),
            rows: Vec::with_capacity(order.len()),
            region_of_dir: Vec::new(),
            region_of_slot: Vec::new(),
            kids,
            dir_nodes: nodes.dir.clone(),
            dirty: Vec::new(),
            dirty_mark: vec![false; order.len()],
            scratch: Vec::new(),
        };
        for (r, &d) in order.iter().enumerate() {
            let slot = nodes.dir[d as usize].index();
            region_of_dir[d as usize] = r as u32;
            region_of_slot[slot as usize] = r as u32;
            let depth = depth_of[d as usize];
            let color = region_color(depth, max_depth, &spec);
            let b = tree.bounds_of_slot(slot).and_then(|b| region_box(&b, depth, &spec));
            this.dir.push(d);
            this.slot.push(slot);
            this.depth.push(depth);
            this.color.push(color);
            this.boxes.push(b);
            this.rows.push(Self::row(slot, b, color));
        }
        this.region_of_dir = region_of_dir;
        this.region_of_slot = region_of_slot;
        this
    }

    fn row(slot: u32, b: Option<Aabb>, color: [f32; 4]) -> RegionGpu {
        let [lo, hi] = b.unwrap_or([[0.0; 3]; 2]);
        RegionGpu { lo: [lo[0], lo[1], lo[2], f32::from_bits(slot)], hi: [hi[0], hi[1], hi[2], 0.0], color }
    }

    pub(super) fn len(&self) -> usize {
        self.dir.len()
    }

    /// The region drawing directory `d` (a `Plan::dirs` index).
    pub(super) fn region_of_dir(&self, d: usize) -> Option<u32> {
        self.region_of_dir.get(d).copied().filter(|&r| r != NONE)
    }

    /// Region `r`'s box in its directory's frame.
    #[cfg(test)]
    pub(super) fn region_box_of(&self, r: u32) -> Option<Aabb> {
        self.boxes.get(r as usize).copied().flatten()
    }

    /// Region `r`'s `Plan::dirs` index and depth.
    pub(super) fn dir_of(&self, r: u32) -> (usize, u32) {
        (self.dir[r as usize] as usize, self.depth[r as usize])
    }

    /// Re-pad the regions whose directory boxes the tree's refits rewrote
    /// since the last sync. O(refit directories). Returns the refit slots
    /// taken (every library node, region or not).
    pub(super) fn sync(&mut self, tree: &mut TreeCull) -> usize {
        self.scratch.clear();
        tree.take_refits(&mut self.scratch);
        for k in 0..self.scratch.len() {
            let slot = self.scratch[k];
            let r = self.region_of_slot.get(slot as usize).copied().unwrap_or(NONE);
            if r == NONE {
                continue;
            }
            let ri = r as usize;
            let b = tree.bounds_of_slot(slot).and_then(|b| region_box(&b, self.depth[ri], &self.spec));
            if b == self.boxes[ri] {
                continue;
            }
            self.boxes[ri] = b;
            self.rows[ri] = Self::row(slot, b, self.color[ri]);
            if !self.dirty_mark[ri] {
                self.dirty_mark[ri] = true;
                self.dirty.push(r);
            }
        }
        self.scratch.len()
    }

    /// The regions whose row changed since the last call.
    fn take_dirty(&mut self, out: &mut Vec<u32>) {
        for &r in &self.dirty {
            self.dirty_mark[r as usize] = false;
        }
        out.append(&mut self.dirty);
    }

    /// The innermost region the ray `ro + t·rd` (world) enters: the deepest
    /// directory whose region it passes through, the nearest entry among
    /// equals (the later drawn on an exact tie), by a top-down walk that descends only into regions the ray
    /// enters. Returns the region and its entry t. Boxes are as of the last
    /// `sync`.
    pub(super) fn pick(&self, t: &NodeTables, ro: DVec3, rd: DVec3) -> Option<(u32, f64)> {
        let root = *self.dir_nodes.first()?;
        let w0 = t.world(root).ok()?.0;
        let local = t.local_rows();
        let mut stack: Vec<(u32, Similarity)> =
            self.kids[0].iter().map(|&k| (k, Similarity::compose(&w0, &local[self.dir_nodes[k as usize].index() as usize]))).collect();
        let mut best: Option<(u32, f64, u32)> = None;
        while let Some((d, w)) = stack.pop() {
            let r = self.region_of_dir[d as usize];
            let Some(b) = self.boxes.get(r as usize).copied().flatten() else { continue };
            let (o, v) = ray_to_local(&w, ro, rd);
            let lo = DVec3::new(f64::from(b[0][0]), f64::from(b[0][1]), f64::from(b[0][2]));
            let hi = DVec3::new(f64::from(b[1][0]), f64::from(b[1][1]), f64::from(b[1][2]));
            let Some(tt) = super::pick::ray_aabb(o, v, lo, hi) else { continue };
            let depth = self.depth[r as usize];
            if best.is_none_or(|(bd, bt, br)| depth > bd || (depth == bd && (tt < bt || (tt == bt && r > br)))) {
                best = Some((depth, tt, r));
            }
            for &k in &self.kids[d as usize] {
                stack.push((k, Similarity::compose(&w, &local[self.dir_nodes[k as usize].index() as usize])));
            }
        }
        best.map(|(_, tt, r)| (r, tt))
    }
}

const WG: u32 = 64;

/// What one frame's region work did and cost on the CPU (`REGIONTIME`).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RegionCost {
    /// Refit slots taken from the tree cull (any library node).
    pub refits: usize,
    /// Region rows changed, and the bytes written for them (the whole
    /// table when more than a quarter changed: one write, not thousands).
    pub uploaded: usize,
    pub bytes: usize,
    /// Whether the compose pass was encoded.
    pub composed: bool,
    pub ms: f64,
}

/// The regions on the GPU: the boxes (host-written when a refit changes
/// one), the instances (GPU-written), and the draw.
pub(super) struct LibraryRegions {
    pub(super) table: RefCell<RegionTable>,
    count: u32,
    /// The picked region (NONE: none), and what the uniform last carried.
    picked: Cell<u32>,
    picked_written: Cell<u32>,
    params_cpu: RegionParamsGpu,
    params: wgpu::Buffer,
    regions: wgpu::Buffer,
    pub(super) instances: wgpu::Buffer,
    compute: wgpu::ComputePipeline,
    compute_bgl: wgpu::BindGroupLayout,
    compute_bg: RefCell<Option<wgpu::BindGroup>>,
    render: wgpu::RenderPipeline,
    render_bg: wgpu::BindGroup,
    /// Regions uploaded but not yet composed (the first frame: all).
    pending_compose: Cell<bool>,
    dirty: RefCell<Vec<u32>>,
}

impl LibraryRegions {
    pub(super) fn new(device: &wgpu::Device, camera_buf: &wgpu::Buffer, color_format: wgpu::TextureFormat, table: RegionTable) -> Self {
        use wgpu::util::DeviceExt;
        let count = table.len() as u32;
        let spec = table.spec;
        let rows: &[RegionGpu] = if table.rows.is_empty() { &[RegionGpu { lo: [0.0; 4], hi: [0.0; 4], color: [0.0; 4] }] } else { &table.rows };
        let regions = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("library regions"),
            contents: bytemuck::cast_slice(rows),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("library region instances"),
            size: u64::from(count.max(1)) * std::mem::size_of::<RegionInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_cpu = RegionParamsGpu {
            edge_opacity: spec.edge_opacity,
            edge_px: spec.edge_px,
            picked_opacity: spec.picked_opacity,
            picked_edge_opacity: spec.picked_edge_opacity,
            picked: NONE,
            count,
            _pad: [0; 2],
        };
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("library region params"),
            contents: bytemuck::bytes_of(&params_cpu),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("library_regions.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/library_regions.wgsl").into()),
        });
        let uniform = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let compute_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("library regions compute bgl"),
            entries: &[uniform(0, wgpu::ShaderStages::COMPUTE), storage(1, true), storage(2, true), storage(3, false)],
        });
        let compute_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("library regions compute pl"),
            bind_group_layouts: &[Some(&compute_bgl)],
            immediate_size: 0,
        });
        let compute = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("library regions"),
            layout: Some(&compute_layout),
            module: &module,
            entry_point: Some("write_regions"),
            compilation_options: Default::default(),
            cache: None,
        });
        let render_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("library regions render bgl"),
            entries: &[uniform(0, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT), uniform(4, wgpu::ShaderStages::VERTEX)],
        });
        let render_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("library regions render bg"),
            layout: &render_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: camera_buf.as_entire_binding() },
            ],
        });
        let render_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("library regions render pl"),
            bind_group_layouts: &[Some(&render_bgl)],
            immediate_size: 0,
        });
        let render = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("library regions"),
            layout: Some(&render_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_region"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RegionInstance>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_region"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            // Double-sided: the far walls show through the near ones.
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            // Tested (a page hides the walls behind it), never written (the
            // glyphs drawn after read over every wall).
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        LibraryRegions {
            table: RefCell::new(table),
            count,
            picked: Cell::new(NONE),
            picked_written: Cell::new(NONE),
            params_cpu,
            params,
            regions,
            instances,
            compute,
            compute_bgl,
            compute_bg: RefCell::new(None),
            render,
            render_bg,
            pending_compose: Cell::new(true),
            dirty: RefCell::new(Vec::new()),
        }
    }

    /// Show region `r` as picked (None: none); the uniform follows at the
    /// next `prepare`.
    pub(super) fn set_picked(&self, r: Option<u32>) {
        self.picked.set(r.unwrap_or(NONE));
    }

    /// The frame's region work, after the cull (whose refit logged what
    /// changed) and before the glyph pass: take the refits, upload the rows
    /// they changed and the picked index if it moved, and compose the
    /// instances when the frame's flush resolved anything or a row was
    /// uploaded. A settled frame does nothing. `world` is the scene graph's
    /// world table, rebound when the flush says it was replaced (`grew`).
    pub(super) fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, tree: &mut TreeCull, world: &wgpu::Buffer, flush: Option<&glyph_scene_graph::FlushStats>) -> RegionCost {
        let t0 = std::time::Instant::now();
        let mut cost = RegionCost::default();
        if self.count == 0 {
            return cost;
        }
        let (resolved, rebind) = flush.map_or((false, false), |st| (st.dispatched, st.grew));
        let mut table = self.table.borrow_mut();
        cost.refits = table.sync(tree);
        let mut dirty = self.dirty.borrow_mut();
        dirty.clear();
        table.take_dirty(&mut dirty);
        cost.uploaded = dirty.len();
        if !dirty.is_empty() {
            let stride = std::mem::size_of::<RegionGpu>() as u64;
            if dirty.len() * 4 > table.len() {
                queue.write_buffer(&self.regions, 0, bytemuck::cast_slice(&table.rows));
                cost.bytes = std::mem::size_of_val(table.rows.as_slice());
            } else {
                cost.bytes = dirty.len() * stride as usize;
                for &r in dirty.iter() {
                    queue.write_buffer(&self.regions, u64::from(r) * stride, bytemuck::bytes_of(&table.rows[r as usize]));
                }
            }
        }
        if self.picked.get() != self.picked_written.get() {
            let p = RegionParamsGpu { picked: self.picked.get(), ..self.params_cpu };
            queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&p));
            self.picked_written.set(self.picked.get());
        }
        if !(resolved || rebind || !dirty.is_empty() || self.pending_compose.get()) {
            cost.ms = t0.elapsed().as_secs_f64() * 1e3;
            return cost;
        }
        self.pending_compose.set(false);
        let mut bg = self.compute_bg.borrow_mut();
        if rebind || bg.is_none() {
            *bg = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("library regions compute bg"),
                layout: &self.compute_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: world.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.regions.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: self.instances.as_entire_binding() },
                ],
            }));
        }
        let groups = self.count.div_ceil(WG);
        assert!(groups <= 65_535, "library regions: {} regions is past one dispatch row", self.count);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("library regions"), timestamp_writes: None });
        pass.set_pipeline(&self.compute);
        pass.set_bind_group(0, bg.as_ref().expect("just built"), &[]);
        pass.dispatch_workgroups(groups, 1, 1);
        cost.composed = true;
        cost.ms = t0.elapsed().as_secs_f64() * 1e3;
        cost
    }

    /// Draw every region (inside the glyph field pass, after the page faces).
    pub(super) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.count == 0 {
            return;
        }
        pass.set_pipeline(&self.render);
        pass.set_bind_group(0, &self.render_bg, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..36, 0..self.count);
    }
}

#[cfg(test)]
mod tests;
