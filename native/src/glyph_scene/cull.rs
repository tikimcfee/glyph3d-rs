//! Stage F — the cull/LOD cluster: the CPU segment cull, the far-LOD
//! backdrop stream (compacted quad list + flat-tint pipeline), and the
//! per-segment record type the staging code fills. Extracted from
//! `glyph_scene.rs` in the 2026-09 code-shape refactor — a pure move;
//! `pub(super)` stands in for the same-module privacy these items had
//! (the scene's render and group-edit paths drive `CullState`'s fields
//! directly). See the crate module header for why the cull is CPU-side.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::cell::Cell;

use super::{GroupRow, SCENE_SAMPLE_COUNT};
use crate::gpu::GpuContext;

/// Stage F: LOD threshold in on-screen pixels per em cell (cell height = 1.0
/// world unit). Below 1 px/em individual glyphs are raster-lottery subpixel
/// blobs; the segment's backdrop quad (mean ink color × ink coverage) is the
/// visually equivalent representation. Never substitutes at ≥1 px/em, so
/// legible text is always drawn glyph-by-glyph.
pub const LOD_MIN_PX: f32 = 1.0;

/// Stage F: backdrop coverage gain — fitted against the Stage E2 full-field
/// render (out/f-field-before.png): with E = ink_frac × GAIN the post-LOD
/// wide shot's mean linear pixel value matches the pre-LOD one within ~3%
/// (GAIN 1.9 overshot +45%, 1.3 overshot +29%, 1.0 +17%, 0.7 lands even; the residual
/// difference is the flat-per-file haze vs real per-page texture — the
/// backdrop fills a file's intra-page gaps). See out/STAGE_F_REPORT.md.
pub const BACKDROP_GAIN: f32 = 0.7;

/// Per-segment cull record, 48 B. One segment per FILE in repo mode; text
/// scenes stage a single segment covering the whole block. Bounds are
/// WORLD-space with the group offset already applied.
///
/// NOT a GPU struct, despite the `Pod` derive and the comment this replaced,
/// which claimed it mirrored a `SegCull` in cull.wgsl. It does not: that shader
/// declares only `BackdropInst` and `Camera`, culling is entirely CPU-side
/// (`cull_segments`), and this type is never uploaded to any buffer — no
/// `write_buffer`, no `cast_slice`, no test pinning its layout. The derive and
/// the old `_pad` lane were vestigial.
///
/// The z lanes replace that `_pad`, so the struct is still 48 B and `tint` is
/// still at offset 32. They exist because the old code did not merely lack
/// depth — it ASSERTED a false one, testing every segment as though it spanned
/// z ∈ [-1, 1]. That was true while all instances lived in the z=0 plane, and
/// WrapBack made it false by spending wraps in depth.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SegCull {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub slot_base: u32,
    pub slot_count: u32,
    /// rgb = mean LINEAR ink color (sRGB bytes pow-2.2 decoded at staging);
    /// w = effective per-pixel ink coverage E at deep minification
    /// (`ink_frac × BACKDROP_GAIN`, clamped to 1) — the backdrop alpha.
    pub tint: [f32; 4],
}

/// Stage F — world cell footprint used for ink-density estimates: em advance
/// 1229/2320 ≈ 0.53 world units × 1.25 line pitch. Approximation only; it
/// feeds the backdrop haze alpha, never any layout decision.
pub const GLYPH_CELL_AREA: f32 = (1229.0 / 2320.0) * 1.25;

/// Stage F — one far-LOD backdrop quad, 48 B, mirrors `BackdropInst` in
/// cull.wgsl: world rect + premultiplied-ready color (rgb linear, a = E) + far-Z reading depth.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BackdropInst {
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub rgba: [f32; 4],
    pub depth: [f32; 4],
}

/// Stage L (L2): draw phases — re_renderer's DrawPhase borrow (a flat enum
/// partitioning draw order, per-phase work lists, no render graph).
/// Recording iterates phases in declaration order: Backdrop first, Glyphs
/// second — exactly the Stage F order. Stage L (L4) adds Selection: it
/// renders selected glyph quads into a separate MASK target after the glyph
/// field pass — own target ⇒ own pass, so it is not an in-pass arm; the
/// variant marks the phase ordering (Selection renders after Glyphs) and
/// gates the mask pass. (`Overlay` arrives WITH its phase; no dead
/// variants — Stage J rule.)
pub(super) enum Phase {
    /// Far-LOD backdrop quads (one instanced draw over the compacted list).
    Backdrop,
    /// The glyph instances (per-chunk range draws).
    Glyphs,
    /// Stage L (L4): the selection mask (selected segments' glyph quads).
    Selection,
}

/// Stage L (L2): one frame's draw work, partitioned by phase. Built during
/// cull (culled path) or straight from the chunk counts (legacy --no-cull
/// path).
pub(super) struct PhaseDraws {
    /// Backdrop phase: the compacted far-LOD quads.
    pub(super) backdrops: Vec<BackdropInst>,
    /// Glyphs phase: (chunk, chunk-local slot range), CHUNK-major —
    /// ascending chunk, arena-ascending ranges within a chunk — so the
    /// record order (and thus the within-pixel blend order) is identical to
    /// the pre-L2 per-chunk loops.
    pub(super) glyph_ranges: Vec<(u32, std::ops::Range<u32>)>,
}

/// Per-frame, view-derived cull inputs (everything the segment table doesn't
/// provide). Bundled so `cull_segments` stays under the argument-count lint.
pub(super) struct CullView {
    pub(super) planes: [[f32; 4]; 6],
    pub(super) eye: Vec3,
    /// px per world unit at distance 1 (height / 2·tan(fov/2)).
    pub(super) px_scale: f32,
    /// Stage K (K4): the LOD threshold (px/em) is per-frame data (was the
    /// LOD_MIN_PX const read directly) so windowed runs can tune it live;
    /// offscreen always carries the const (see CullState::lod_min_px).
    pub(super) lod_min_px: f32,
    /// Whether to render file background bounding quads behind glyphs when near.
    pub(super) file_backgrounds: bool,
    /// RGBA color for near file background cards.
    pub(super) file_bg_color: [f32; 4],
}

/// Stage F — CPU cull: frustum + LOD over the segment table. Returns the
/// frame's draw work as phase lists (Stage L, L2): the Backdrop phase's
/// compacted quad list and the Glyphs phase's (chunk, chunk-local range)
/// list — chunk-major, arena-ascending within a chunk, so blending matches
/// the legacy full draws exactly. See the module header for the contract
/// and for why this runs on the CPU. Stage G: `hidden` (parallel to
/// `segments`, empty = nothing hidden) skips user-hidden groups entirely —
/// no glyph draws AND no backdrop.
pub(super) fn cull_segments(
    segments: &[SegCull],
    hidden: &[bool],
    view: &CullView,
    chunk_cap: u32,
    chunk_count: u32,
) -> PhaseDraws {
    let CullView {
        planes,
        eye,
        px_scale,
        lod_min_px,
        file_backgrounds,
        file_bg_color,
    } = *view;
    let mut draws: Vec<Vec<std::ops::Range<u32>>> =
        (0..chunk_count).map(|_| Vec::new()).collect();
    let mut backdrops = Vec::new();
    for (si, seg) in segments.iter().enumerate() {
        if hidden.get(si).copied().unwrap_or(false) {
            continue;
        }
        // Frustum: positive-vertex test per plane, over the segment's real AABB.
        // The z lane used to be the constant ±1 — true while every instance
        // lived in the z=0 plane, false since WrapBack began spending wraps in
        // depth, and wrong in the direction that KEEPS what it should drop.
        let mut visible = true;
        for pl in planes {
            let px = if pl[0] >= 0.0 { seg.max[0] } else { seg.min[0] };
            let py = if pl[1] >= 0.0 { seg.max[1] } else { seg.min[1] };
            let pz = if pl[2] >= 0.0 { seg.max[2] } else { seg.min[2] };
            if pl[0] * px + pl[1] * py + pl[2] * pz + pl[3] < 0.0 {
                visible = false;
                break;
            }
        }
        if !visible {
            continue;
        }
        // LOD: em-cell pixel height at the AABB's nearest point to the eye
        // (conservative — a segment only drops to backdrop when even its
        // CLOSEST glyphs are subpixel).
        let nx = eye.x.clamp(seg.min[0], seg.max[0]);
        let ny = eye.y.clamp(seg.min[1], seg.max[1]);
        // Clamped into the segment, like x and y. Clamping into the old ±1 slab
        // computed the distance to a place the segment is not, understating it
        // whenever the content had depth — so far content measured near, and
        // was drawn at full detail instead of collapsing to a backdrop.
        let nz = eye.z.clamp(seg.min[2], seg.max[2]);
        let dist = ((eye.x - nx).powi(2) + (eye.y - ny).powi(2) + (eye.z - nz).powi(2))
            .sqrt()
            .max(0.001);
        let glyph_px = px_scale / dist;
        if glyph_px < lod_min_px {
            if seg.slot_count > 0 {
                backdrops.push(BackdropInst {
                    // Backdrops are flat quads anchored at the file space's
                    // far-Z reading surface (seg.min[2]).
                    min: [seg.min[0], seg.min[1]],
                    max: [seg.max[0], seg.max[1]],
                    rgba: seg.tint,
                    depth: [seg.min[2], 0.0, 0.0, 0.0],
                });
            }
            continue;
        }
        // Near reading mode: if file_backgrounds is enabled, push a background quad
        // behind the glyphs anchored slightly behind far-Z to avoid Z-fighting.
        if file_backgrounds && seg.slot_count > 0 {
            backdrops.push(BackdropInst {
                min: [seg.min[0], seg.min[1]],
                max: [seg.max[0], seg.max[1]],
                rgba: file_bg_color,
                depth: [seg.min[2] - 0.02, 0.0, 0.0, 0.0],
            });
        }
        // Glyph stream: split the slot range across arena chunks.
        let slot_end = seg.slot_base + seg.slot_count;
        for c in 0..chunk_count {
            let c_lo = c * chunk_cap;
            let lo = seg.slot_base.max(c_lo);
            let hi = slot_end.min(c_lo + chunk_cap);
            if hi > lo {
                draws[c as usize].push((lo - c_lo)..(hi - c_lo));
            }
        }
    }
    // Stage L (L2): flatten chunk-major into the Glyphs phase list — the
    // per-chunk vectors are already segment/arena-ascending, so the flat
    // list's record order matches the pre-L2 loops exactly.
    let glyph_ranges = draws
        .into_iter()
        .enumerate()
        .flat_map(|(c, rs)| rs.into_iter().map(move |r| (c as u32, r)))
        .collect();
    PhaseDraws { backdrops, glyph_ranges }
}

/// Extract the 6 frustum planes from a view-proj matrix (Gribb-Hartmann;
/// wgpu clip space has z ∈ `[0,w]`, so the near plane is row2, not row3+row2).
/// glam's to_cols_array is column-major: row r = `(m[r], m[4+r], m[8+r], m[12+r])`.
pub(super) fn frustum_planes(vp: &Mat4) -> [[f32; 4]; 6] {
    let m = vp.to_cols_array();
    let row = |r: usize| [m[r], m[4 + r], m[8 + r], m[12 + r]];
    let (r0, r1, r2, r3) = (row(0), row(1), row(2), row(3));
    let add = |a: [f32; 4], b: [f32; 4]| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
    let sub = |a: [f32; 4], b: [f32; 4]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
    let mut planes = [
        add(r3, r0), // left
        sub(r3, r0), // right
        add(r3, r1), // bottom
        sub(r3, r1), // top
        r2,          // near (z ≥ 0)
        sub(r3, r2), // far
    ];
    for p in &mut planes {
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-12);
        for c in p.iter_mut() {
            *c /= len;
        }
    }
    planes
}

/// Stage F — the cull/LOD subsystem. The per-frame segment cull itself runs
/// on the CPU (`cull_segments`, ~1.3k AABB tests ≈ microseconds — see the
/// module header for why the GPU-indirect design was abandoned); what remains
/// here is the far-LOD BACKDROP stream: a CPU-compacted quad list uploaded
/// per frame plus the flat-tint pipeline drawing it.
pub(super) struct CullState {
    /// CPU copy of the segment table (the per-frame cull input).
    pub(super) segments: Vec<SegCull>,
    /// Stage G: per-segment LOCAL (pre-TRS) AABBs + the as-staged backdrop
    /// tints + hidden flags — group edits re-sync `segments` from these
    /// (`GlyphScene::sync_segment`).
    pub(super) local_min: Vec<[f32; 3]>,
    pub(super) local_max: Vec<[f32; 3]>,
    pub(super) base_tint: Vec<[f32; 4]>,
    /// Group color rgb at staging time — tint edits scale the backdrop by
    /// pow(new)/pow(orig) so an untouched segment keeps its Stage F tint.
    pub(super) orig_group_rgb: Vec<[f32; 3]>,
    pub(super) hidden: Vec<bool>,
    /// Stage K (K4): live LOD threshold (px/em), seeded from the LOD_MIN_PX
    /// const. Cell because render(&self) is immutable (the `viewport: Cell`
    /// precedent). The ONLY write site is the UI-controls application in
    /// render(), which runs solely when a windowed probe is installed —
    /// offscreen never writes it, so offscreen culls with the const.
    pub(super) lod_min_px: Cell<f32>,
    pub(super) file_backgrounds: Cell<bool>,
    pub(super) file_bg_color: Cell<[f32; 4]>,
    /// seg_count × 32 B staging target for the per-frame backdrop list.
    pub(super) backdrop_insts_buf: wgpu::Buffer,
    pub(super) backdrop_pipeline: wgpu::RenderPipeline,
    pub(super) backdrop_bind_group: wgpu::BindGroup,
}

impl CullState {
    pub(super) fn new(
        ctx: &GpuContext,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_buf: &wgpu::Buffer,
        segments: &[SegCull],
        groups: &[GroupRow],
    ) -> Self {
        let device = &ctx.device;
        let seg_count = segments.len() as u32;

        // Stage G: derive the local (pre-TRS) segment AABBs — at staging time
        // every group is at scale 1, so local = world − group offset.
        let mut local_min = Vec::with_capacity(segments.len());
        let mut local_max = Vec::with_capacity(segments.len());
        let mut base_tint = Vec::with_capacity(segments.len());
        let mut orig_group_rgb = Vec::with_capacity(segments.len());
        for (i, seg) in segments.iter().enumerate() {
            let off = groups
                .get(i)
                .map(|g| [g.cols[0][0], g.cols[0][1], g.cols[0][2]])
                .unwrap_or([0.0, 0.0, 0.0]);
            local_min.push([seg.min[0] - off[0], seg.min[1] - off[1], seg.min[2] - off[2]]);
            local_max.push([seg.max[0] - off[0], seg.max[1] - off[1], seg.max[2] - off[2]]);
            base_tint.push(seg.tint);
            orig_group_rgb.push(
                groups
                    .get(i)
                    .map(|g| [g.cols[2][0], g.cols[2][1], g.cols[2][2]])
                    .unwrap_or([1.0, 1.0, 1.0]),
            );
        }

        let backdrop_stride = std::mem::size_of::<BackdropInst>() as u64;
        let max_backdrops = (seg_count * 2 + 512).max(1024) as u64;
        let backdrop_insts_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("backdrop instances"),
            size: max_backdrops * backdrop_stride,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        log::info!(
            "cull: {} segments (CPU frustum+LOD) | backdrop buffer {} B (KB-scale, no instance-sized buffers added)",
            seg_count,
            seg_count as u64 * backdrop_stride,
        );

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cull.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/cull.wgsl").into()),
        });

        // --- backdrop render pipeline ----------------------------------------
        // Flat tinted quads for far (subpixel-glyph) segments. Same camera
        // uniform, same premultiplied blend and the SAME depth state as the
        // glyph pass: test AND write, LessEqual. This used to test without
        // writing, on the premise that all glyph-plane content lives at z=0
        // and segments do not overlap, so draw order between streams was
        // immaterial. `--wrap-mode back` (the default since dde3f82) ended
        // that premise: a wrapped line steps BACK in z, so a long file's
        // column recedes behind the pages beside it, and with nothing writing
        // depth the later-drawn file simply painted over the nearer one —
        // measured 2026-09-07, wide.txt's column over long.md's pages from a
        // front-on camera pitched down. LessEqual rather than Less keeps
        // every coplanar fragment passing, so within a z=0 page the blend
        // order is exactly what it was.
        let ro_storage = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let backdrop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                ro_storage(1, wgpu::ShaderStages::VERTEX),
            ],
        });
        let backdrop_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("backdrop bg"),
            layout: &backdrop_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: camera_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: backdrop_insts_buf.as_entire_binding(),
                },
            ],
        });
        let backdrop_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("backdrop pl"),
            bind_group_layouts: &[Some(&backdrop_bgl)],
            immediate_size: 0,
        });
        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop pipeline"),
            layout: Some(&backdrop_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_backdrop"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_backdrop"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: Default::default(),
                bias: wgpu::DepthBiasState {
                    constant: -100,
                    slope_scale: -1.5,
                    clamp: 0.0,
                },
            }),
            multisample: wgpu::MultisampleState {
                count: SCENE_SAMPLE_COUNT, // Stage L (L3): loud non-MSAA pin
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        });

        Self {
            segments: segments.to_vec(),
            local_min,
            local_max,
            base_tint,
            orig_group_rgb,
            hidden: vec![false; segments.len()],
            lod_min_px: Cell::new(LOD_MIN_PX),
            file_backgrounds: Cell::new(false),
            file_bg_color: Cell::new(crate::DEFAULT_FILE_BG_COLOR),
            backdrop_insts_buf,
            backdrop_pipeline,
            backdrop_bind_group,
        }
    }
}

#[cfg(test)]
mod cull_depth_tests {
    use super::*;

    #[test]
    fn reversed_z_math() {
        let fov = 60f32.to_radians();
        let aspect = 1.6f32;
        let near = 0.05f32;
        let far = 1000.0f32;

        let p_rev = glam::camera::rh::proj::directx::perspective(fov, aspect, far, near);
        let v_near = p_rev.project_point3(Vec3::new(0.0, 0.0, -near));
        let v_far = p_rev.project_point3(Vec3::new(0.0, 0.0, -far));
        assert!((v_near.z - 1.0).abs() < 1e-4, "near z must be 1.0, got {}", v_near.z);
        assert!((v_far.z - 0.0).abs() < 1e-4, "far z must be 0.0, got {}", v_far.z);

        // Frustum planes check: a point at z = -500 (inside frustum)
        let planes = frustum_planes(&p_rev);
        let pt_inside = Vec3::new(0.0, 0.0, -500.0);
        for (i, pl) in planes.iter().enumerate() {
            let dist = pl[0] * pt_inside.x + pl[1] * pt_inside.y + pl[2] * pt_inside.z + pl[3];
            assert!(dist >= 0.0, "plane {} should contain pt_inside, dist = {}", i, dist);
        }

        // A point behind the camera (z = +10) must be culled
        let pt_behind = Vec3::new(0.0, 0.0, 10.0);
        let culled_behind = planes.iter().any(|pl| {
            pl[0] * pt_behind.x + pl[1] * pt_behind.y + pl[2] * pt_behind.z + pl[3] < 0.0
        });
        assert!(culled_behind, "point behind camera must be culled");

        // A point beyond far plane (z = -2000) must be culled
        let pt_beyond_far = Vec3::new(0.0, 0.0, -2000.0);
        let culled_far = planes.iter().any(|pl| {
            pl[0] * pt_beyond_far.x + pl[1] * pt_beyond_far.y + pl[2] * pt_beyond_far.z + pl[3] < 0.0
        });
        assert!(culled_far, "point beyond far plane must be culled");
    }

    /// A segment somewhere in space, with the tint the cull path never reads.
    fn seg(min: [f32; 3], max: [f32; 3]) -> SegCull {
        SegCull { min, max, slot_base: 0, slot_count: 16, tint: [0.0; 4] }
    }

    /// A frustum that keeps everything except what is behind the near plane,
    /// which here is a plane at z = -10 facing +z. Every other plane is placed
    /// far enough away to be irrelevant, so a cull decision is attributable to
    /// depth alone.
    fn view_clipping_behind_z(near_z: f32, eye: Vec3, lod_min_px: f32) -> CullView {
        let far = |a: f32, b: f32, c: f32, d: f32| [a, b, c, d];
        CullView {
            planes: [
                far(1.0, 0.0, 0.0, 1.0e6),
                far(-1.0, 0.0, 0.0, 1.0e6),
                far(0.0, 1.0, 0.0, 1.0e6),
                far(0.0, -1.0, 0.0, 1.0e6),
                // keep z >= near_z
                [0.0, 0.0, 1.0, -near_z],
                far(0.0, 0.0, -1.0, 1.0e6),
            ],
            eye,
            px_scale: 1000.0,
            lod_min_px,
            file_backgrounds: false,
            file_bg_color: [0.10, 0.10, 0.13, 0.85],
        }
    }

    fn drew_glyphs(d: &PhaseDraws) -> bool {
        d.glyph_ranges.iter().any(|(_, r)| !r.is_empty())
    }

    /// A segment BEHIND the near plane must be culled.
    ///
    /// The old arithmetic tested every segment as though it spanned z ∈ [-1, 1]
    /// regardless of where it was, so a segment at z = -50 was judged at z ≈ 0 —
    /// comfortably inside — and drawn. True while everything lived in the z=0
    /// plane; false since WrapBack started spending wraps in depth.
    #[test]
    fn a_segment_behind_the_near_plane_is_culled() {
        let s = seg([-1.0, -1.0, -50.0], [1.0, 1.0, -49.0]);
        let v = view_clipping_behind_z(-10.0, Vec3::new(0.0, 0.0, 5.0), 0.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(
            !drew_glyphs(&d),
            "a segment at z=-50, behind a near plane at z=-10, was drawn: the \
             frustum test is ignoring the segment's depth"
        );
    }

    /// ...and one in front of it must survive, so the test above cannot pass by
    /// culling everything.
    #[test]
    fn a_segment_in_front_of_the_near_plane_survives() {
        let s = seg([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        let v = view_clipping_behind_z(-10.0, Vec3::new(0.0, 0.0, 5.0), 0.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(drew_glyphs(&d), "a segment inside the frustum was culled");
    }

    /// LOD distance must include depth.
    ///
    /// `nz` was `eye.z.clamp(-1.0, 1.0)`, so the nearest point of a segment 50
    /// units away in z was computed as if it were adjacent: the distance came
    /// out ~0 instead of ~50, the glyphs measured far larger than a pixel, and
    /// a segment that should have collapsed to a backdrop quad was drawn in
    /// full. The error is in the expensive direction — it draws what it should
    /// have skipped.
    #[test]
    fn lod_distance_accounts_for_depth() {
        let s = seg([-1.0, -1.0, -50.0], [1.0, 1.0, -49.0]);
        // px_scale/dist at dist≈50 is 20 px/em; ask for 100 so it must drop.
        let v = view_clipping_behind_z(-1.0e6, Vec3::new(0.0, 0.0, 0.0), 100.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(
            !drew_glyphs(&d) && !d.backdrops.is_empty(),
            "a segment 50 units away in z was drawn at full detail: the LOD \
             distance is ignoring depth"
        );
    }

    #[test]
    fn backdrop_quad_anchors_to_far_z() {
        assert_eq!(std::mem::size_of::<BackdropInst>(), 48);
        let s = seg([-1.0, -1.0, -50.0], [1.0, 1.0, -40.0]);
        let v = view_clipping_behind_z(-1.0e6, Vec3::new(0.0, 0.0, 0.0), 100.0);
        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert_eq!(d.backdrops.len(), 1);
        assert_eq!(
            d.backdrops[0].depth[0], -50.0,
            "backdrop must anchor to seg.min[2] far-z reading surface"
        );
    }

    #[test]
    fn file_background_emits_in_near_reading_mode() {
        let s = seg([-1.0, -1.0, -5.0], [1.0, 1.0, -4.0]);
        let mut v = view_clipping_behind_z(-10.0, Vec3::new(0.0, 0.0, 0.0), 0.0);
        v.file_backgrounds = true;
        v.file_bg_color = [0.2, 0.3, 0.4, 0.5];

        let d = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(drew_glyphs(&d), "glyphs must be drawn in near mode");
        assert_eq!(d.backdrops.len(), 1, "background quad must be emitted behind glyphs");
        assert_eq!(d.backdrops[0].rgba, [0.2, 0.3, 0.4, 0.5]);
        assert_eq!(d.backdrops[0].depth[0], -5.0 - 0.02);

        // When disabled, no backdrop is emitted in near mode
        v.file_backgrounds = false;
        let d2 = cull_segments(&[s], &[false], &v, 1024, 1);
        assert!(drew_glyphs(&d2));
        assert!(d2.backdrops.is_empty());
    }
}

