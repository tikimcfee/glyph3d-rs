//! Stage L — the pooled view target + composite machinery (L3) and the
//! selection mask/tint resources (L4): the ping-pong pool the phase lists
//! draw into, the per-size `ViewTarget`, and the windowed-only selection
//! visuals. Extracted from `glyph_scene.rs` in the 2026-09 code-shape
//! refactor — a pure move; `pub(super)` stands in for the same-module
//! privacy these items had (the scene's new/render drive the fields and
//! `ViewTarget::new` directly).

use std::cell::Cell;

// ── Stage L (L3): pooled view target + composite (the ViewBuilder borrow) ──
//
// re_renderer's ViewBuilder renders each view into a pooled target, then
// composites into whatever pass the host provides. L3 lands the skeleton for
// GlyphScene only (the demo Scene stays direct — it is the minimal template
// by design): the phase lists draw into a ping-pong pair of POOL_FORMAT
// textures + one depth, then composite into the driver's view:
//   - same format (offscreen oracle, Rgba8UnormSrgb):
//     copy_texture_to_texture — 1:1, no scaling, bit-exact BY CONSTRUCTION;
//   - different format (windowed, Bgra8UnormSrgb — component-order
//     incompatible with the pool, so a copy is invalid):
//     a fullscreen shader composite through composite.wgsl.
// The split exists for the component-order copy incompatibility; a future
// scaled/sub-rect composite (minimap inset) extends the shader path only.
// egui wrinkle (recorded, not solved): register_native_texture demands
// Rgba8Unorm (NON-sRGB) — an egui-hosted view would want its own non-sRGB
// pool or a conversion pass.

/// Pooled view-target format: matches the offscreen oracle's target exactly.
pub(super) const POOL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Both scene pipelines (glyph + backdrop) are non-MSAA. The pool's copy
/// composite is invalid on multisample targets — this was a silent
/// assumption before L3; now it is loud at both ends (the pipeline
/// multisample states and ViewTarget::new's assert).
pub(super) const SCENE_SAMPLE_COUNT: u32 = 1;

/// The persistent half of the composite: pipeline (targets the DRIVER's
/// color format), bind group layout, sampler. `target` is sized by
/// set_viewport; `parity` selects which pool texture the frame draws into.
pub(super) struct CompositeState {
    pub(super) pipeline: wgpu::RenderPipeline,
    pub(super) bind_group_layout: wgpu::BindGroupLayout,
    pub(super) sampler: wgpu::Sampler,
    pub(super) target: Option<ViewTarget>,
    pub(super) parity: Cell<u8>,
    /// Stage L (L4): selection mask/tint machinery — only on the
    /// shader-composite path (windowed). None when the driver composites by
    /// copy (offscreen), which therefore never renders selection visuals.
    pub(super) selection_fx: Option<SelectionFx>,
}

/// The per-size half: the ping-pong pair of pool textures (re_renderer's
/// DynamicResourcePool is reference material, not a dependency — two
/// textures, not a pool), one depth, and the composite bind groups that
/// sample each pool texture.
pub(super) struct ViewTarget {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) colors: [wgpu::Texture; 2],
    pub(super) color_views: [wgpu::TextureView; 2],
    pub(super) depth: wgpu::TextureView,
    pub(super) bind_groups: [wgpu::BindGroup; 2],
    /// Stage L (L4): the selection mask target + per-pool-slot tint bind
    /// groups. Only created on the shader-composite path (windowed) — the
    /// copy path (offscreen) never renders selection visuals, so offscreen
    /// stays byte-identical by construction.
    pub(super) mask: Option<MaskSet>,
}

/// Stage L (L4): the mask target for the selection pass. (No texture field:
/// a wgpu TextureView keeps its texture alive internally.)
pub(super) struct MaskSet {
    pub(super) view: wgpu::TextureView,
    /// Tint-pass bind groups, one per pool slot: `pool[slot]` + mask + tint
    /// uniform.
    pub(super) tint_bgs: [wgpu::BindGroup; 2],
}

/// Stage L (L4): selection state — replaces the Stage G click-flash hack
/// (instance-byte write + restore). Set by apply_pick (the exact API the
/// CLI op-stream and the windowed click share): a glyph pick with a real
/// slot selects that glyph; a file-level pick (or a blank-glyph pick, which
/// has no slot) selects the whole segment; a pick MISS clears. Verbs never
/// touch it. Persistent until the next pick — this closes the "sticky
/// flash" gap.
pub(super) enum Selection {
    /// One glyph slot (arena-global split into chunk + chunk-local index).
    Glyph { chunk: u32, local: u32 },
    /// A whole segment/file (arena slot range; split per chunk at draw).
    Segment { slot_base: u32, slot_count: u32 },
}

/// Stage L (L4): the selection tint (warm yellow, 45% additive) — the
/// flash's bright-yellow legacy, but as a coverage-weighted tint that keeps
/// the glyph readable underneath.
pub(super) const SELECTION_TINT: [f32; 4] = [1.0, 0.85, 0.25, 0.45];

/// Stage L (L4): mask/tint pass resources (windowed shader path only).
pub(super) struct SelectionFx {
    /// Glyph geometry drawn into the mask: same glyph_field.wgsl and the
    /// same per-chunk bind groups, blend disabled, Rgba8Unorm target.
    pub(super) mask_pipeline: wgpu::RenderPipeline,
    /// composite.wgsl's fs_tint: pool + mask → pool (ping-pong), additive
    /// tint.
    pub(super) tint_pipeline: wgpu::RenderPipeline,
    pub(super) tint_bgl: wgpu::BindGroupLayout,
    /// Static tint uniform (written once at creation).
    pub(super) tint_buf: wgpu::Buffer,
}

/// Stage L (L4): mask target format. Rgba8Unorm (not the sRGB pool format):
/// the mask is data (coverage in alpha), and the mask pipeline needs a
/// non-pool format to coexist with the glyph pipeline's sRGB target.
pub(super) const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl ViewTarget {
    pub(super) fn new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        selection_fx: Option<&SelectionFx>,
    ) -> Self {
        assert_eq!(
            SCENE_SAMPLE_COUNT, 1,
            "L3: copy composite requires non-MSAA scene pipelines/targets"
        );
        let colors: [wgpu::Texture; 2] = std::array::from_fn(|i| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(if i == 0 { "view target A" } else { "view target B" }),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: SCENE_SAMPLE_COUNT,
                dimension: wgpu::TextureDimension::D2,
                format: POOL_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        });
        // TextureView is an owned handle (Arc inside) — no borrow ties.
        let color_views: [wgpu::TextureView; 2] =
            std::array::from_fn(|i| colors[i].create_view(&Default::default()));
        let bind_groups: [wgpu::BindGroup; 2] = std::array::from_fn(|i| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(if i == 0 { "composite bg A" } else { "composite bg B" }),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&color_views[i]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        });
        let depth = crate::scene::create_depth(device, wgpu::TextureFormat::Depth32Float, width, height);
        // Stage L (L4): the selection mask target + tint bind groups
        // (shader-composite path only — the caller passes the fx only then).
        let mask = selection_fx.map(|fx| {
            let mask_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("selection mask"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: MASK_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = mask_texture.create_view(&Default::default());
            let tint_bgs = std::array::from_fn(|i| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(if i == 0 { "tint bg A" } else { "tint bg B" }),
                    layout: &fx.tint_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&color_views[i]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: fx.tint_buf.as_entire_binding(),
                        },
                    ],
                })
            });
            MaskSet { view, tint_bgs }
        });
        Self { width, height, colors, color_views, depth, bind_groups, mask }
    }
}
