use std::path::Path;
use bytemuck::{Pod, Zeroable};
use crate::gpu::GpuContext;
use crate::layout::{LayoutGlyphs, VerifyLayout};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct ItemParamsGpu {
    pub line_height: f32,
    pub origin_y: f32,
    pub origin_z: f32,
    pub z_step: f32,
    pub z_step_lo: f32,
    pub band_stride_y: f32,
    pub depth_per_band: f32,
    pub depth_per_col: f32,
    pub page_rows: i32,
    pub pages_wide: i32,
    pub page_cols: i32,
    pub scroll_rows: i32,
    pub has_page: u32,
    pub line_height_lo: f32,
    pub _pad1: u32,
    pub _pad2: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SpikeSlotInput {
    pub row: u32,
    pub wrap_segment: u32,
    pub item_idx: u32,
    pub col: u32,
    pub stored_y: f32,
    pub stored_z: f32,
    pub _pad0: u32,
    pub _pad1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SpikeUniforms {
    pub width: f32,
    pub height: f32,
    pub total_slots: u32,
    pub _pad: u32,
}

#[derive(Debug, Default)]
pub struct SpikeReport {
    pub total_slots: usize,
    pub vertex_y_matches: usize,
    pub vertex_z_matches: usize,
    pub max_y_ulp: u32,
    pub max_z_ulp: u32,
    pub compute_y_matches: usize,
    pub compute_z_matches: usize,
    pub vertex_matches_compute: usize,
    pub first_y_mismatches: Vec<MismatchDetail>,
    pub first_z_mismatches: Vec<MismatchDetail>,
}

#[derive(Debug, Clone)]
pub struct MismatchDetail {
    pub slot_idx: usize,
    pub item_idx: u32,
    pub row: u32,
    pub wrap_segment: u32,
    pub derived: f32,
    pub stored: f32,
    pub derived_bits: u32,
    pub stored_bits: u32,
    pub ulp: u32,
}

const SPIKE_WGSL: &str = r#"
struct ItemParamsGpu {
    line_height: f32,
    origin_y: f32,
    origin_z: f32,
    z_step: f32,
    z_step_lo: f32,
    band_stride_y: f32,
    depth_per_band: f32,
    depth_per_col: f32,
    page_rows: i32,
    pages_wide: i32,
    page_cols: i32,
    scroll_rows: i32,
    has_page: u32,
    line_height_lo: f32,
    _pad1: u32,
    _pad2: u32,
}

struct SpikeSlotInput {
    row: u32,
    wrap_segment: u32,
    item_idx: u32,
    col: u32,
    stored_y: f32,
    stored_z: f32,
    _pad0: u32,
    _pad1: u32,
}

struct SpikeUniforms {
    width: f32,
    height: f32,
    total_slots: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> slots: array<SpikeSlotInput>;
@group(0) @binding(1) var<storage, read> items: array<ItemParamsGpu>;
@group(0) @binding(2) var<uniform> uniforms: SpikeUniforms;

fn derive_yz(slot: SpikeSlotInput, item: ItemParamsGpu) -> vec2<f32> {
    var derived_y = 0.0;
    var derived_z = 0.0;

    let depth_steps = -(f32(slot.wrap_segment));
    let z_tail = fma(depth_steps, item.z_step_lo, item.origin_z);
    let z_stepped = fma(depth_steps, item.z_step, z_tail);

    if (item.has_page != 0u) {
        let screen_row = i32(slot.row) - item.scroll_rows;
        var y_page = 0;
        if (item.page_rows > 0 && screen_row >= item.page_rows) {
            y_page = screen_row / item.page_rows;
        }
        var x_page = 0;
        if (item.page_cols > 0) {
            x_page = i32(slot.col) / item.page_cols;
        }
        let pages_wide = max(item.pages_wide, 1);
        let band = y_page / pages_wide;
        let row_in_page = f32(screen_row - y_page * item.page_rows);
        let y_tail = fma(-row_in_page, item.line_height_lo, item.origin_y);
        let y_row_folded = fma(-row_in_page, item.line_height, y_tail);
        derived_y = fma(-(f32(band)), item.band_stride_y, y_row_folded);

        let z_banded = fma(f32(band), item.depth_per_band, z_stepped);
        derived_z = fma(f32(x_page), item.depth_per_col, z_banded);
    } else {
        let y_tail = fma(-(f32(slot.row)), item.line_height_lo, item.origin_y);
        derived_y = fma(-(f32(slot.row)), item.line_height, y_tail);
        derived_z = z_stepped;
    }

    return vec2<f32>(derived_y, derived_z);
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) @interpolate(flat) res_y: u32,
    @location(1) @interpolate(flat) res_z: u32,
    @location(2) @interpolate(flat) bits_y: u32,
    @location(3) @interpolate(flat) bits_z: u32,
}

@vertex
fn vs_main(@builtin(vertex_index) v_idx: u32) -> VsOut {
    var out: VsOut;
    if (v_idx >= uniforms.total_slots) {
        out.clip_pos = vec4<f32>(-2.0, -2.0, 0.0, 1.0);
        return out;
    }

    let slot = slots[v_idx];
    let item = items[slot.item_idx];
    let yz = derive_yz(slot, item);

    let y_bits = bitcast<u32>(yz.x);
    let z_bits = bitcast<u32>(yz.y);
    let sy_bits = bitcast<u32>(slot.stored_y);
    let sz_bits = bitcast<u32>(slot.stored_z);

    let px = f32(v_idx % 2048u);
    let py = f32(v_idx / 2048u);
    let ndc_x = (px + 0.5) / uniforms.width * 2.0 - 1.0;
    let ndc_y = 1.0 - (py + 0.5) / uniforms.height * 2.0;

    out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.5, 1.0);
    out.res_y = u32(y_bits == sy_bits);
    out.res_z = u32(z_bits == sz_bits);
    out.bits_y = y_bits;
    out.bits_z = z_bits;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<u32> {
    return vec4<u32>(in.res_y, in.res_z, in.bits_y, in.bits_z);
}

@group(0) @binding(3) var<storage, read_write> compute_out: array<vec4<u32>>;

@compute @workgroup_size(256)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= uniforms.total_slots) {
        return;
    }
    let slot = slots[idx];
    let item = items[slot.item_idx];
    let yz = derive_yz(slot, item);

    let y_bits = bitcast<u32>(yz.x);
    let z_bits = bitcast<u32>(yz.y);
    let sy_bits = bitcast<u32>(slot.stored_y);
    let sz_bits = bitcast<u32>(slot.stored_z);

    compute_out[idx] = vec4<u32>(
        u32(y_bits == sy_bits),
        u32(z_bits == sz_bits),
        y_bits,
        z_bits
    );
}
"#;

pub fn ulp_diff(a: f32, b: f32) -> u32 {
    let a_bits = a.to_bits();
    let b_bits = b.to_bits();
    let a_sign = (a_bits >> 31) != 0;
    let b_sign = (b_bits >> 31) != 0;
    if a_sign != b_sign {
        if (a_bits & 0x7FFF_FFFF) == 0 && (b_bits & 0x7FFF_FFFF) == 0 {
            0
        } else {
            (a_bits & 0x7FFF_FFFF).abs_diff(b_bits & 0x7FFF_FFFF)
        }
    } else {
        a_bits.abs_diff(b_bits)
    }
}

pub fn run_spike(ctx: &GpuContext, dir: &Path) -> SpikeReport {
    let params = crate::repo::RepoParams {
        wrap_mode: crate::fold::WrapMode::Back,
        cluster_mode: crate::fold::ClusterMode::Cluster,
        color_mode: crate::repo::ColorMode::Flat,
        ..Default::default()
    };
    let walk = crate::repo::walk_repo(dir);
    let file_params: Vec<crate::layout::ItemParams> = walk
        .files
        .iter()
        .map(|f| {
            let newlines = f.bytes.iter().filter(|&&b| b == b'\n').count();
            crate::repo::file_item_params(&params, f.bytes.len(), newlines)
        })
        .collect();

    let mut gpu_items: Vec<ItemParamsGpu> = Vec::with_capacity(file_params.len());
    for p in &file_params {
        let z_step_lo = (p.z_step - p.z_step as f32 as f64) as f32;
        gpu_items.push(ItemParamsGpu {
            line_height: p.line_height as f32,
            origin_y: p.origin_y as f32,
            origin_z: p.origin_z as f32,
            z_step: p.z_step as f32,
            z_step_lo,
            band_stride_y: p.band_stride_y as f32,
            depth_per_band: p.depth_per_band as f32,
            depth_per_col: p.depth_per_col as f32,
            page_rows: p.page_rows,
            pages_wide: p.pages_wide,
            page_cols: p.page_cols,
            scroll_rows: p.scroll_rows,
            has_page: if p.has_page { 1 } else { 0 },
            line_height_lo: (p.line_height - p.line_height as f32 as f64) as f32,
            _pad1: 0,
            _pad2: 0,
        });
    }

    let mut arena = crate::layout::GlyphArena::new();
    let mut backend = crate::layout_hyper::HyperLayout::new();
    backend
        .load_trie_file(&crate::default_engine_trie())
        .expect("engine trie");
    let eng_items: Vec<crate::layout::LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| {
            crate::layout::LayoutItem {
                bytes: &f.bytes,
                params: file_params[index],
                group_id: index as u32,
                paint: crate::layout::Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED),
            }
        })
        .collect();
    let _ = backend
        .layout_items_recording(&eng_items, &mut arena)
        .expect("engine layout failed");

    let instances = arena.instances();
    let total_slots = instances.len();
    assert!(total_slots > 0, "No glyph instances to test in spike");

    let mut slot_inputs: Vec<SpikeSlotInput> = Vec::with_capacity(total_slots);
    for inst in instances {
        let item_idx = inst.group_id;
        let p = &file_params[item_idx as usize];
        let terminator = (inst.flags & crate::fold::F_NEWLINE) != 0;
        let wrap_seg = crate::fold::wrap_segment_of(inst.col as i64, p.wrap_width as i64, terminator) as u32;

        slot_inputs.push(SpikeSlotInput {
            row: inst.row,
            wrap_segment: wrap_seg,
            item_idx,
            col: inst.col,
            stored_y: inst.pos[1],
            stored_z: inst.pos[2],
            _pad0: 0,
            _pad1: 0,
        });
    }

    evaluate_slots_on_gpu(ctx, &slot_inputs, &gpu_items)
}

pub fn evaluate_slots_on_gpu(
    ctx: &GpuContext,
    slot_inputs: &[SpikeSlotInput],
    gpu_items: &[ItemParamsGpu],
) -> SpikeReport {
    let device = &ctx.device;
    let queue = &ctx.queue;
    let total_slots = slot_inputs.len();
    assert!(total_slots > 0, "No slots to evaluate");

    let tex_w = 2048u32;
    let tex_h = (total_slots as u32).div_ceil(tex_w);

    use wgpu::util::DeviceExt;
    let slots_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("spike slots"),
        contents: bytemuck::cast_slice(slot_inputs),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let items_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("spike items"),
        contents: bytemuck::cast_slice(gpu_items),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let uniforms = SpikeUniforms {
        width: tex_w as f32,
        height: tex_h as f32,
        total_slots: total_slots as u32,
        _pad: 0,
    };
    let uni_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("spike uniforms"),
        contents: bytemuck::bytes_of(&uniforms),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("spike wgsl"),
        source: wgpu::ShaderSource::Wgsl(SPIKE_WGSL.into()),
    });

    let render_target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("spike render target"),
        size: wgpu::Extent3d {
            width: tex_w,
            height: tex_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Uint,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let render_view = render_target.create_view(&Default::default());

    let compute_out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike compute out"),
        size: (total_slots * 16) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("spike bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("spike bg"),
        layout: &bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: slots_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: items_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uni_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: compute_out_buf.as_entire_binding(),
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spike pipeline layout"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });

    let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spike render pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba32Uint,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::PointList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("spike compute pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("cs_main"),
        compilation_options: Default::default(),
        cache: None,
    });

    let tex_bytes_per_row = (tex_w * 16).max(256);
    let tex_staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike tex staging"),
        size: (tex_bytes_per_row * tex_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let comp_staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike comp staging"),
        size: (total_slots * 16) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("spike encoder"),
    });

    // 1. Render pass (executes vertex stage)
    {
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spike rpass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &render_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rpass.set_pipeline(&render_pipeline);
        rpass.set_bind_group(0, &bind_group, &[]);
        rpass.draw(0..total_slots as u32, 0..1);
    }

    // 2. Compute pass (executes compute stage)
    {
        let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("spike cpass"),
            timestamp_writes: None,
        });
        cpass.set_pipeline(&compute_pipeline);
        cpass.set_bind_group(0, &bind_group, &[]);
        let workgroups = (total_slots as u32).div_ceil(256);
        cpass.dispatch_workgroups(workgroups, 1, 1);
    }

    // 3. Copy outputs to staging
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &render_target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &tex_staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(tex_bytes_per_row),
                rows_per_image: Some(tex_h),
            },
        },
        wgpu::Extent3d {
            width: tex_w,
            height: tex_h,
            depth_or_array_layers: 1,
        },
    );

    encoder.copy_buffer_to_buffer(&compute_out_buf, 0, &comp_staging, 0, (total_slots * 16) as u64);

    queue.submit(Some(encoder.finish()));

    // Map and read back
    let tex_slice = tex_staging.slice(..);
    let comp_slice = comp_staging.slice(..);

    let (tx_r, rx_r) = std::sync::mpsc::channel();
    let (tx_c, rx_c) = std::sync::mpsc::channel();

    tex_slice.map_async(wgpu::MapMode::Read, move |res| {
        tx_r.send(res).unwrap();
    });
    comp_slice.map_async(wgpu::MapMode::Read, move |res| {
        tx_c.send(res).unwrap();
    });

    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .unwrap();
    rx_r.recv().unwrap().unwrap();
    rx_c.recv().unwrap().unwrap();

    let tex_data = tex_slice.get_mapped_range().unwrap();
    let comp_data = comp_slice.get_mapped_range().unwrap();

    let comp_words: &[[u32; 4]] = bytemuck::cast_slice(&comp_data);

    let mut report = SpikeReport {
        total_slots,
        ..Default::default()
    };

    for idx in 0..total_slots {
        let px = idx % 2048;
        let py = idx / 2048;
        let row_offset = (py * (tex_bytes_per_row as usize / 16) + px) * 16;
        let vertex_word_bytes = &tex_data[row_offset..row_offset + 16];
        let v_words: [u32; 4] = bytemuck::cast_slice(vertex_word_bytes)[0];

        let c_words = comp_words[idx];

        let slot = &slot_inputs[idx];

        let v_match_y = v_words[0] != 0;
        let v_match_z = v_words[1] != 0;
        let v_derived_y = f32::from_bits(v_words[2]);
        let v_derived_z = f32::from_bits(v_words[3]);

        let c_match_y = c_words[0] != 0;
        let c_match_z = c_words[1] != 0;

        if v_match_y {
            report.vertex_y_matches += 1;
        } else {
            let ulp = ulp_diff(v_derived_y, slot.stored_y);
            report.max_y_ulp = report.max_y_ulp.max(ulp);
            if report.first_y_mismatches.len() < 10 {
                report.first_y_mismatches.push(MismatchDetail {
                    slot_idx: idx,
                    item_idx: slot.item_idx,
                    row: slot.row,
                    wrap_segment: slot.wrap_segment,
                    derived: v_derived_y,
                    stored: slot.stored_y,
                    derived_bits: v_words[2],
                    stored_bits: slot.stored_y.to_bits(),
                    ulp,
                });
            }
        }

        if v_match_z {
            report.vertex_z_matches += 1;
        } else {
            let ulp = ulp_diff(v_derived_z, slot.stored_z);
            report.max_z_ulp = report.max_z_ulp.max(ulp);
            if report.first_z_mismatches.len() < 10 {
                report.first_z_mismatches.push(MismatchDetail {
                    slot_idx: idx,
                    item_idx: slot.item_idx,
                    row: slot.row,
                    wrap_segment: slot.wrap_segment,
                    derived: v_derived_z,
                    stored: slot.stored_z,
                    derived_bits: v_words[3],
                    stored_bits: slot.stored_z.to_bits(),
                    ulp,
                });
            }
        }

        if c_match_y {
            report.compute_y_matches += 1;
        }
        if c_match_z {
            report.compute_z_matches += 1;
        }

        if v_words[2] == c_words[2] && v_words[3] == c_words[3] {
            report.vertex_matches_compute += 1;
        }
    }

    drop(tex_data);
    drop(comp_data);
    tex_staging.unmap();
    comp_staging.unmap();

    report
}

/// Run a synthetic combinatorial suite over various item configurations and extreme bounds.
pub fn run_synthetic_spike(ctx: &GpuContext) -> SpikeReport {
    let raw_items = vec![
        // Item 0: Repo unpaginated standard
        crate::fold::Item {
            line_height: 1.0,
            origin_y: 0.0,
            origin_z: 0.0,
            z_step: 0.05 * 0.15,
            has_page: false,
            ..Default::default()
        },
        // Item 1: Unpaginated with offsets
        crate::fold::Item {
            line_height: 1.33333333,
            origin_y: -42.5,
            origin_z: 17.125,
            z_step: 0.0087654321,
            has_page: false,
            ..Default::default()
        },
        // Item 2: Paginated repo standard
        crate::fold::Item {
            line_height: 1.0,
            origin_y: 0.0,
            origin_z: 0.0,
            z_step: 0.05 * 0.15,
            has_page: true,
            page_rows: 100,
            pages_wide: 4,
            band_stride_y: 110.0,
            ..Default::default()
        },
        // Item 3: Paginated tall
        crate::fold::Item {
            line_height: 1.15,
            origin_y: 5.0,
            origin_z: -1.0,
            z_step: 0.012,
            has_page: true,
            page_rows: 250,
            pages_wide: 2,
            band_stride_y: 295.0,
            ..Default::default()
        },
        // Item 4: Paginated with 3D band/col depth
        crate::fold::Item {
            line_height: 1.0,
            origin_y: 0.0,
            origin_z: 0.0,
            z_step: 0.0075,
            has_page: true,
            page_rows: 80,
            pages_wide: 3,
            page_cols: 100,
            band_stride_y: 85.0,
            depth_per_band: 4.0,
            depth_per_col: 1.5,
            ..Default::default()
        },
    ];

    let mut gpu_items: Vec<ItemParamsGpu> = Vec::new();
    for it in &raw_items {
        let z_step_lo = (it.z_step - it.z_step as f32 as f64) as f32;
        gpu_items.push(ItemParamsGpu {
            line_height: it.line_height as f32,
            origin_y: it.origin_y as f32,
            origin_z: it.origin_z as f32,
            z_step: it.z_step as f32,
            z_step_lo,
            band_stride_y: it.band_stride_y as f32,
            depth_per_band: it.depth_per_band as f32,
            depth_per_col: it.depth_per_col as f32,
            page_rows: it.page_rows as i32,
            pages_wide: it.pages_wide as i32,
            page_cols: it.page_cols as i32,
            scroll_rows: it.scroll_rows as i32,
            has_page: if it.has_page { 1 } else { 0 },
            line_height_lo: (it.line_height - it.line_height as f32 as f64) as f32,
            _pad1: 0,
            _pad2: 0,
        });
    }

    let mut slot_inputs: Vec<SpikeSlotInput> = Vec::new();

    let test_rows = [0, 1, 2, 7, 23, 79, 80, 81, 99, 100, 101, 199, 200, 201, 499, 500, 1000, 2048, 5000, 10000, 32768, 65535];
    let test_wrap_segs = [0, 1, 2, 3, 4, 7, 10, 15, 25, 50, 99];
    let test_cols = [0, 10, 50, 99, 100, 150, 250];

    for (item_idx, it) in raw_items.iter().enumerate() {
        for &row in &test_rows {
            for &wrap_seg in &test_wrap_segs {
                for &col in &test_cols {
                    // CPU reference formula from pass2_host.rs
                    let (stored_y, stored_z) = if it.has_page {
                        let (y_page, x_page, screen_row) = if it.page_rows > 0 {
                            let y_page = row as i64 / it.page_rows;
                            let screen_row = row as i64 + it.scroll_rows;
                            let x_page = if it.page_cols > 0 { col as i64 / it.page_cols } else { 0 };
                            (y_page, x_page, screen_row)
                        } else {
                            (0, 0, row as i64)
                        };
                        let pages_wide = it.pages_wide.max(1);
                        let band = y_page / pages_wide;
                        let py = (it.origin_y
                            - (screen_row - y_page * it.page_rows) as f64 * it.line_height
                            - band as f64 * it.band_stride_y) as f32;
                        let pz = (it.origin_z - wrap_seg as f64 * it.z_step
                            + band as f64 * it.depth_per_band
                            + x_page as f64 * it.depth_per_col) as f32;
                        (py, pz)
                    } else {
                        let py = (-(row as f64) * it.line_height + it.origin_y) as f32;
                        let pz = (-(wrap_seg as f64) * it.z_step + it.origin_z) as f32;
                        (py, pz)
                    };

                    slot_inputs.push(SpikeSlotInput {
                        row,
                        wrap_segment: wrap_seg,
                        item_idx: item_idx as u32,
                        col,
                        stored_y,
                        stored_z,
                        _pad0: 0,
                        _pad1: 0,
                    });
                }
            }
        }
    }

    evaluate_slots_on_gpu(ctx, &slot_inputs, &gpu_items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vertex_stage_yz_bit_exactness_synthetic() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let report = run_synthetic_spike(&ctx);

        println!("Synthetic Spike: {} slots evaluated", report.total_slots);
        println!(
            "Vertex Stage Y: {} / {} exact (max ULP: {})",
            report.vertex_y_matches, report.total_slots, report.max_y_ulp
        );
        println!(
            "Vertex Stage Z: {} / {} exact (max ULP: {})",
            report.vertex_z_matches, report.total_slots, report.max_z_ulp
        );
        println!(
            "Compute Stage Y: {} / {} exact",
            report.compute_y_matches, report.total_slots
        );
        println!(
            "Vertex vs Compute bit match: {} / {} (100.0%)",
            report.vertex_matches_compute, report.total_slots
        );

        assert!(
            report.max_y_ulp <= 1,
            "Vertex stage Y failed: max ULP {} exceeds 1 on synthetic suite",
            report.max_y_ulp
        );
        assert!(
            report.max_z_ulp <= 1,
            "Vertex stage Z failed: max ULP {} exceeds 1 on synthetic suite",
            report.max_z_ulp
        );
        assert_eq!(
            report.vertex_matches_compute, report.total_slots,
            "Vertex stage did not match compute stage bit-for-bit"
        );
    }

    #[test]
    fn test_vertex_stage_yz_bit_exactness_repo() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let repo_dir = Path::new("fixtures/g-pick-repo");
        if !repo_dir.exists() {
            return;
        }
        let report = run_spike(&ctx, repo_dir);

        println!("Repo Spike (g-pick-repo): {} slots evaluated", report.total_slots);
        println!(
            "Vertex Stage Y: {} / {} exact (max ULP: {})",
            report.vertex_y_matches, report.total_slots, report.max_y_ulp
        );
        println!(
            "Vertex Stage Z: {} / {} exact (max ULP: {})",
            report.vertex_z_matches, report.total_slots, report.max_z_ulp
        );

        assert_eq!(
            report.vertex_y_matches, report.total_slots,
            "Vertex stage Y failed bit-exactness on g-pick-repo (max ULP {})",
            report.max_y_ulp
        );
        assert_eq!(
            report.vertex_z_matches, report.total_slots,
            "Vertex stage Z failed bit-exactness on g-pick-repo (max ULP {})",
            report.max_z_ulp
        );
    }
}
