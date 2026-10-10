//! The GPU side: the layout kernel in both lookup variants, the renderer's
//! Derived-mode Slug pipeline (its WGSL verbatim, its bind layout, its blend
//! and depth state from `glyph-field/src/field_core.rs`) and the flat-quad
//! control, the per-corpus buffers, one frame, the camera, the screenshot.

use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use wgpu::{BufferUsages as U, ShaderStages as S};

use crate::atlas::{Atlas, Params};
use crate::corpus::{Corpus, Seg, Slot, Visible, LINE_HEIGHT, SEG_BYTES, SLOT_BYTES, Z_STEP};
use crate::gpu::{ms, Gpu};
use crate::trie::GpuTables;

pub const TARGET: (u32, u32) = (1600, 1000);
pub const MAX_BYTE_CHUNKS: usize = 4;
/// The renderer's front camera: fov 40°, near/far as fractions of the fit distance (config defaults).
pub const FOV_Y_DEG: f32 = 40.0;
const NEAR_FRACTION: f32 = 0.01;
const FAR_FACTOR: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant { Trie = 0, Direct = 1 }
impl Variant {
    pub const ALL: [Variant; 2] = [Variant::Trie, Variant::Direct];
    pub fn name(self) -> &'static str { match self { Variant::Trie => "A trie (index+block)", Variant::Direct => "B direct BMP + hash" } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DrawMode { SlugEmoji, SlugNoEmoji, Flat, None }
impl DrawMode {
    pub fn name(self) -> &'static str {
        match self { DrawMode::SlugEmoji => "Slug, emoji sheet bound", DrawMode::SlugNoEmoji => "Slug, 1-texel stand-in for the sheet", DrawMode::Flat => "flat quads (control)", DrawMode::None => "no draw" }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct KernelParams { pub segment_count: u32, pub wrap_cols: u32, pub chunk_shift: u32, pub chunk_mask: u32, pub cell_adv: f32, pub emit_adv: u32, pub seq_max: u32, pub color: u32 }

/// `glyph_field::GroupRow`: offset / quat / color+alpha / scale+colorBlend / clip / bg_color.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct GroupRow { pub cols: [[f32; 4]; 6] }
impl GroupRow {
    pub fn tinted(offset: [f32; 3], rgb: [f32; 3]) -> GroupRow {
        GroupRow { cols: [[offset[0], offset[1], offset[2], 0.0], [0.0, 0.0, 0.0, 1.0], [rgb[0], rgb[1], rgb[2], 1.0], [1.0, 1.0, 1.0, 0.0], [0.0; 4], [0.0; 4]] }
    }
}

/// `glyph_field::ItemParamsGpu`, 64 B; the no-page form every view here uses.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ItemParamsGpu { pub line_height: f32, pub origin_y: f32, pub origin_z: f32, pub z_step: f32, pub z_step_lo: f32, pub band_stride_y: f32, pub depth_per_band: f32, pub depth_per_col: f32, pub page_rows: i32, pub pages_wide: i32, pub page_cols: i32, pub scroll_rows: i32, pub has_page: u32, pub line_height_lo: f32, pub _pad1: u32, pub _pad2: u32 }
impl ItemParamsGpu {
    pub fn flat_page() -> ItemParamsGpu {
        let (lh, zs) = (LINE_HEIGHT as f64, Z_STEP as f64);
        ItemParamsGpu { line_height: lh as f32, origin_y: 0.0, origin_z: 0.0, z_step: zs as f32, z_step_lo: (zs - zs as f32 as f64) as f32, band_stride_y: 0.0, depth_per_band: 0.0, depth_per_col: 0.0, page_rows: 0, pages_wide: 0, page_cols: 0, scroll_rows: 0, has_page: 0, line_height_lo: (lh - lh as f32 as f64) as f32, _pad1: 0, _pad2: 0 }
    }
}

/// The pipelines (built once) and the atlas-side bindings every corpus shares.
pub struct Pipelines {
    pub kernel: [wgpu::ComputePipeline; 2],
    pub kernel_bgl: wgpu::BindGroupLayout,
    pub slug: wgpu::RenderPipeline,
    pub flat: wgpu::RenderPipeline,
    pub field_bgl: wgpu::BindGroupLayout,
    pub tables_buf: wgpu::Buffer,
    pub target: wgpu::Texture,
    pub target_view: wgpu::TextureView,
    pub depth_view: wgpu::TextureView,
    pub query_set: Option<wgpu::QuerySet>,
    pub ts_resolve: wgpu::Buffer,
    pub ts_read: wgpu::Buffer,
    pub quad_index: wgpu::Buffer,
}

fn substitute(src: &str, pairs: &[(&str, String)]) -> String {
    let mut s = src.to_string();
    for (k, v) in pairs { s = s.replace(&format!("@{k}@"), v) }
    // WGSL's own attributes are `@lowercase`; a placeholder is `@UPPER_CASE@`.
    assert!(!s.as_bytes().windows(2).any(|w| w[0] == b'@' && w[1].is_ascii_uppercase()), "an unsubstituted @NAME@ remains in the kernel source");
    s
}

impl Pipelines {
    pub fn build(gpu: &Gpu, tables: &GpuTables) -> Pipelines {
        let d = &gpu.device;
        let tables_buf = gpu.init_buffer("lookup tables", bytemuck::cast_slice(&tables.words), U::STORAGE);
        let kernel_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("kernel bgl"), entries: &[
            Gpu::storage_entry(0, true, S::COMPUTE), Gpu::storage_entry(1, true, S::COMPUTE), Gpu::storage_entry(2, true, S::COMPUTE), Gpu::storage_entry(3, true, S::COMPUTE),
            Gpu::storage_entry(4, true, S::COMPUTE), Gpu::storage_entry(5, true, S::COMPUTE), Gpu::storage_entry(6, false, S::COMPUTE), Gpu::storage_entry(7, false, S::COMPUTE),
            Gpu::storage_entry(8, true, S::COMPUTE), Gpu::uniform_entry(9, S::COMPUTE),
        ] });
        let kernel_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&kernel_bgl)], immediate_size: 0 });
        let kernel = Variant::ALL.map(|v| {
            let src = substitute(include_str!("layout.wgsl"), &[
                ("VARIANT", (v as u32).to_string()), ("T_ASCII", tables.off_ascii.to_string()), ("T_INDEX", tables.off_index.to_string()),
                ("T_BLOCKS", tables.off_blocks.to_string()), ("T_BITMAP", tables.off_bitmap.to_string()), ("T_BMP", tables.off_bmp.to_string()),
                ("T_CP_HASH", tables.off_cp_hash.to_string()), ("CP_HASH_MASK", tables.cp_hash_mask.to_string()), ("T_SEQ_HASH", tables.off_seq_hash.to_string()),
                ("SEQ_HASH_MASK", tables.seq_hash_mask.to_string()), ("T_SEQ", tables.off_seq.to_string()), ("SEQ_COUNT", tables.seq_count.to_string()),
                ("SEQ_STRIDE", tables.seq_stride.to_string()),
            ]);
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(v.name()), source: wgpu::ShaderSource::Wgsl(src.into()) });
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(v.name()), layout: Some(&kernel_layout), module: &module, entry_point: Some("layout_segments"), compilation_options: Default::default(), cache: None })
        });

        // The Derived field's bind group layout: glyph-field's shared map (0, 2..7), its slot binding (1), its extras (8, 9).
        let uint_tex = |binding, stages| wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Uint, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None };
        let field_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("derived glyph field bgl"), entries: &[
            Gpu::uniform_entry(0, S::VERTEX),
            Gpu::storage_entry(1, true, S::VERTEX),
            Gpu::storage_entry(2, true, S::VERTEX),
            uint_tex(3, S::VERTEX),
            uint_tex(4, S::FRAGMENT),
            Gpu::uniform_entry(5, S::VERTEX | S::FRAGMENT),
            wgpu::BindGroupLayoutEntry { binding: 6, visibility: S::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2Array, multisampled: false }, count: None },
            wgpu::BindGroupLayoutEntry { binding: 7, visibility: S::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            Gpu::storage_entry(8, true, S::VERTEX),
            Gpu::storage_entry(9, true, S::VERTEX),
        ] });
        let field_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("glyph field pl"), bind_group_layouts: &[Some(&field_bgl)], immediate_size: 0 });
        let color_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let build = |label: &str, src: &'static str, blend: Option<wgpu::BlendState>| {
            let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) });
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label), layout: Some(&field_layout),
                vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: color_format, blend, write_mask: wgpu::ColorWrites::ALL })] }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                // field_core.rs: a blended coverage pass that WRITES depth; GreaterEqual (reverse-Z).
                depth_stencil: Some(wgpu::DepthStencilState { format: depth_format, depth_write_enabled: Some(true), depth_compare: Some(wgpu::CompareFunction::GreaterEqual), stencil: Default::default(), bias: Default::default() }),
                multisample: Default::default(), multiview_mask: None, cache: None,
            })
        };
        let premul = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
        };
        let slug = build("derived glyph field pipeline", include_str!("glyph_field_derived.wgsl"), Some(premul));
        let flat = build("flat quads", include_str!("flat.wgsl"), Some(premul));

        let tex = |label, format, usage| d.create_texture(&wgpu::TextureDescriptor { label: Some(label), size: wgpu::Extent3d { width: TARGET.0, height: TARGET.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format, usage, view_formats: &[] });
        let target = tex("target", color_format, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let target_view = target.create_view(&Default::default());
        let depth_view = tex("depth", depth_format, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        let query_set = gpu.timestamps.then(|| d.create_query_set(&wgpu::QuerySetDescriptor { label: Some("ts"), ty: wgpu::QueryType::Timestamp, count: 4 }));
        let ts_resolve = gpu.buffer("ts-resolve", 32, U::QUERY_RESOLVE | U::COPY_SRC);
        let ts_read = gpu.buffer("ts-read", 32, U::MAP_READ | U::COPY_DST);
        let quad_index = gpu.init_buffer("quad index", bytemuck::cast_slice(&[0u16, 1, 2, 0, 2, 3]), U::INDEX);
        Pipelines { kernel, kernel_bgl, slug, flat, field_bgl, tables_buf, target, target_view, depth_view, query_set, ts_resolve, ts_read, quad_index }
    }
}

/// The resident set of one corpus (bytes in up to four chunks, the line and item
/// tables) plus the per-frame buffers and both bind groups, sized to the
/// largest view.
pub struct Buffers {
    /// Held so the resident set's footprint is what the bind group binds (chunks) plus the line table nothing on the GPU reads yet.
    pub _chunks: Vec<wgpu::Buffer>,
    pub _lines: wgpu::Buffer,
    pub _items: wgpu::Buffer,
    pub segs: wgpu::Buffer,
    pub slots: wgpu::Buffer,
    pub advs: wgpu::Buffer,
    pub kparams: wgpu::Buffer,
    pub camera: wgpu::Buffer,
    pub groups: wgpu::Buffer,
    pub item_table: wgpu::Buffer,
    pub params: wgpu::Buffer,
    pub kernel_bg: wgpu::BindGroup,
    pub field_bg_emoji: wgpu::BindGroup,
    pub field_bg_plain: wgpu::BindGroup,
    pub chunk_shift: u32,
    pub max_slots: usize,
    pub upload_ms: f64,
}

impl Buffers {
    pub fn new(gpu: &Gpu, p: &Pipelines, atlas: &Atlas, c: &Corpus, chunk_size: usize, max_segments: usize, max_slots: usize, max_groups: usize) -> Buffers {
        let t = Instant::now();
        let chunks: Vec<wgpu::Buffer> = c.bytes.chunks(chunk_size).map(|ch| gpu.init_buffer("bytes", ch, U::STORAGE)).collect();
        assert!(chunks.len() <= MAX_BYTE_CHUNKS, "{} bytes need {} chunks; the kernel binds {}", c.bytes.len(), chunks.len(), MAX_BYTE_CHUNKS);
        let lines = gpu.init_buffer("lines", bytemuck::cast_slice(&c.lines), U::STORAGE);
        let items = gpu.init_buffer("items", bytemuck::cast_slice(&c.items), U::STORAGE);
        gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
        let upload_ms = ms(t);
        let max_slots = max_slots.max(1);
        let segs = gpu.buffer("segments", max_segments.max(1) as u64 * SEG_BYTES, U::STORAGE | U::COPY_DST);
        let slots = gpu.buffer("slots", max_slots as u64 * SLOT_BYTES, U::STORAGE | U::COPY_SRC);
        let advs = gpu.buffer("advances (check)", max_slots as u64 * 4, U::STORAGE | U::COPY_SRC);
        let kparams = gpu.buffer("kernel params", 32, U::UNIFORM | U::COPY_DST);
        let camera = gpu.buffer("camera", 64, U::UNIFORM | U::COPY_DST);
        let groups = gpu.buffer("groups", max_groups.max(1) as u64 * 96, U::STORAGE | U::COPY_DST);
        let item_table = gpu.buffer("item table", max_groups.max(1) as u64 * 64, U::STORAGE | U::COPY_DST);
        let params = gpu.buffer("glyph params", 64, U::UNIFORM | U::COPY_DST);
        let dummy = gpu.buffer("dummy-chunk", 4, U::STORAGE);
        let chunk = |i: usize| chunks.get(i).unwrap_or(&dummy).as_entire_binding();
        let kernel_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("kernel bg"), layout: &p.kernel_bgl, entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: chunk(0) }, wgpu::BindGroupEntry { binding: 1, resource: chunk(1) },
            wgpu::BindGroupEntry { binding: 2, resource: chunk(2) }, wgpu::BindGroupEntry { binding: 3, resource: chunk(3) },
            wgpu::BindGroupEntry { binding: 4, resource: items.as_entire_binding() }, wgpu::BindGroupEntry { binding: 5, resource: segs.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 6, resource: slots.as_entire_binding() }, wgpu::BindGroupEntry { binding: 7, resource: advs.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 8, resource: p.tables_buf.as_entire_binding() }, wgpu::BindGroupEntry { binding: 9, resource: kparams.as_entire_binding() },
        ] });
        let glyphmap_view = atlas.glyphmap.create_view(&Default::default());
        let curves_view = atlas.curves.create_view(&Default::default());
        let field_bg = |real: bool| {
            let emoji_view = atlas.emoji_view(real);
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("derived glyph bg"), layout: &p.field_bgl, entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: camera.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: slots.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: groups.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&glyphmap_view) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&curves_view) },
                wgpu::BindGroupEntry { binding: 5, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&emoji_view) },
                wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::Sampler(&atlas.sampler) },
                wgpu::BindGroupEntry { binding: 8, resource: item_table.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 9, resource: atlas.glyph_advances.as_entire_binding() },
            ] })
        };
        let field_bg_emoji = field_bg(true);
        let field_bg_plain = field_bg(false);
        Buffers { _chunks: chunks, _lines: lines, _items: items, segs, slots, advs, kparams, camera, groups, item_table, params, kernel_bg, field_bg_emoji, field_bg_plain, chunk_shift: chunk_size.trailing_zeros(), max_slots, upload_ms }
    }

    pub fn resident_bytes(&self, c: &Corpus) -> u64 { c.bytes.len() as u64 + c.lines.len() as u64 * 8 + c.items.len() as u64 * 32 }
}

/// A view's placement: one group per visible line (a reading view) or per
/// item (the overview), each with an origin and a tint; `group_of_line` is
/// parallel to the view's lines.
pub struct Placement { pub origins: Vec<[f32; 3]>, pub tints: Vec<[f32; 3]>, pub group_of_line: Vec<u32>, pub bounds: ([f32; 2], [f32; 2]), pub px_per_em: f32 }

pub fn write_placement(gpu: &Gpu, b: &Buffers, atlas: &Atlas, pl: &Placement) {
    let rows: Vec<GroupRow> = pl.origins.iter().zip(&pl.tints).map(|(o, t)| GroupRow::tinted(*o, *t)).collect();
    gpu.queue.write_buffer(&b.groups, 0, bytemuck::cast_slice(&rows));
    let table: Vec<ItemParamsGpu> = (0..pl.origins.len()).map(|_| ItemParamsGpu::flat_page()).collect();
    gpu.queue.write_buffer(&b.item_table, 0, bytemuck::cast_slice(&table));
    gpu.queue.write_buffer(&b.params, 0, bytemuck::bytes_of(&Params::renderer_defaults(pl.origins.len() as u32, atlas.geometry())));
    gpu.queue.write_buffer(&b.camera, 0, bytemuck::bytes_of(&camera_for(pl.bounds).to_cols_array()));
}

/// The renderer's front camera over the bounds: perspective, fov 40°, reverse-Z
/// (`directx::perspective(fov, aspect, far, near)` as glyph_scene.rs builds it),
/// eye on +Z looking at the bounds' centre, fitted so the bounds fill 96%.
pub fn fit_distance(bounds: ([f32; 2], [f32; 2])) -> f32 {
    let (lo, hi) = bounds;
    let aspect = TARGET.0 as f32 / TARGET.1 as f32;
    let half_h = ((hi[1] - lo[1]) * 0.5).max(1e-3);
    let half_w = ((hi[0] - lo[0]) * 0.5).max(1e-3);
    let t = (FOV_Y_DEG.to_radians() * 0.5).tan();
    (half_h / t).max(half_w / (t * aspect)) / 0.96
}

pub fn px_per_em(bounds: ([f32; 2], [f32; 2])) -> f32 {
    let d = fit_distance(bounds);
    (TARGET.1 as f32 * 0.5) / (d * (FOV_Y_DEG.to_radians() * 0.5).tan())
}

pub fn camera_for(bounds: ([f32; 2], [f32; 2])) -> Mat4 {
    let (lo, hi) = bounds;
    let d = fit_distance(bounds);
    let center = Vec3::new((lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5, 0.0);
    let eye = center + Vec3::new(0.0, 0.0, d);
    let view = glam::camera::rh::view::look_at_mat4(eye, center, Vec3::Y);
    let aspect = TARGET.0 as f32 / TARGET.1 as f32;
    let proj = glam::camera::rh::proj::directx::perspective(FOV_Y_DEG.to_radians(), aspect, d * FAR_FACTOR, d * NEAR_FRACTION);
    proj * view
}

pub struct Frame { pub cpu_ms: f64, pub wall_ms: f64, pub compute_ms: f64, pub draw_ms: f64, pub slots: usize, pub bytes_read: u64, pub segments: usize }

/// One frame: visible list + upload, the layout kernel (`variant`), then the draw (`mode`).
pub fn run_frame(gpu: &Gpu, p: &Pipelines, b: &Buffers, c: &Corpus, trie_cell_adv: f32, seq_max: u32, lines: &[u32], group_of_line: &[u32], variant: Variant, mode: DrawMode, emit_adv: bool) -> Frame {
    let t_cpu = Instant::now();
    let vis: Visible = crate::corpus::visible_list(c, lines, group_of_line);
    assert!(vis.slots <= b.max_slots, "view needs {} slots, buffers hold {}", vis.slots, b.max_slots);
    gpu.queue.write_buffer(&b.segs, 0, bytemuck::cast_slice::<Seg, u8>(&vis.segs));
    gpu.queue.write_buffer(&b.kparams, 0, bytemuck::bytes_of(&KernelParams { segment_count: vis.segs.len() as u32, wrap_cols: c.wrap, chunk_shift: b.chunk_shift, chunk_mask: (1u32 << b.chunk_shift).wrapping_sub(1), cell_adv: trie_cell_adv, emit_adv: u32::from(emit_adv), seq_max, color: crate::corpus::FLAT_COLOR }));
    let cpu_ms = ms(t_cpu);

    let mut enc = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("layout"), timestamp_writes: p.query_set.as_ref().and_then(|qs| gpu.compute_ts(qs, 0)) });
        pass.set_pipeline(&p.kernel[variant as usize]);
        pass.set_bind_group(0, &b.kernel_bg, &[]);
        pass.dispatch_workgroups((vis.segs.len() as u32).div_ceil(64).max(1), 1, 1);
    }
    if mode != DrawMode::None {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &p.target_view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.012, g: 0.014, b: 0.02, a: 1.0 }), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment { view: &p.depth_view, depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0.0), store: wgpu::StoreOp::Store }), stencil_ops: None }),
            timestamp_writes: p.query_set.as_ref().and_then(|qs| gpu.render_ts(qs, 2)),
            occlusion_query_set: None, multiview_mask: None,
        });
        let (pipeline, bg) = match mode {
            DrawMode::SlugEmoji => (&p.slug, &b.field_bg_emoji),
            DrawMode::SlugNoEmoji => (&p.slug, &b.field_bg_plain),
            _ => (&p.flat, &b.field_bg_plain),
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[]);
        // The renderer draws an indexed quad (0 1 2, 0 2 3); the vertex stage's
        // `corners[vi]` is written for that, so index the same way.
        pass.set_index_buffer(p.quad_index.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..6, 0, 0..vis.slots as u32);
    }
    let n_queries = if mode != DrawMode::None { 4 } else { 2 };
    if let Some(qs) = &p.query_set {
        enc.resolve_query_set(qs, 0..n_queries, &p.ts_resolve, 0);
        enc.copy_buffer_to_buffer(&p.ts_resolve, 0, &p.ts_read, 0, 32);
    }
    let wall_ms = gpu.submit_wait(enc.finish());
    let (compute_ms, draw_ms) = if p.query_set.is_some() {
        let ts: Vec<u64> = bytemuck::pod_collect_to_vec(&gpu.map_read(&p.ts_read, 32)[..32]);
        let tick = gpu.ts_period as f64 / 1e6;
        (ts[1].wrapping_sub(ts[0]) as f64 * tick, if mode != DrawMode::None { ts[3].wrapping_sub(ts[2]) as f64 * tick } else { 0.0 })
    } else { (f64::NAN, f64::NAN) };
    Frame { cpu_ms, wall_ms, compute_ms, draw_ms, slots: vis.slots, bytes_read: vis.bytes_read, segments: vis.segs.len() }
}

/// The slots (and, when emitted, advances) the last frame wrote.
pub fn read_slots(gpu: &Gpu, b: &Buffers, n: usize, with_adv: bool) -> (Vec<Slot>, Vec<f32>) {
    // pod_collect_to_vec: a readback Vec<u8> carries no alignment promise (an empty one has none at all).
    let slots: Vec<Slot> = bytemuck::pod_collect_to_vec(&gpu.read_back(&b.slots, n as u64 * SLOT_BYTES));
    let advs: Vec<f32> = if with_adv { bytemuck::pod_collect_to_vec(&gpu.read_back(&b.advs, n as u64 * 4)) } else { Vec::new() };
    (slots, advs)
}

/// The offscreen target as an RGB PNG (no text of any kind drawn into it beyond the glyphs).
pub fn screenshot(gpu: &Gpu, p: &Pipelines, path: &std::path::Path) {
    let (w, h) = TARGET;
    let pitch = (w * 4).next_multiple_of(256);
    let buf = gpu.buffer("shot", (pitch * h) as u64, U::MAP_READ | U::COPY_DST);
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(p.target.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(pitch), rows_per_image: None } }, wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 });
    gpu.submit_wait(enc.finish());
    let data = gpu.map_read(&buf, (pitch * h) as u64);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        for px in data[y * pitch as usize..][..(w * 4) as usize].chunks_exact(4) { rgb.extend_from_slice(&px[..3]) }
    }
    let file = std::fs::File::create(path).expect("screenshot path");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    println!("screenshot: {} ({} bytes)", path.display(), std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
}
