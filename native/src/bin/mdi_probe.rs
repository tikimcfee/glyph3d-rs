//! mdi-probe — Stage K proof tool: does an indirect draw with
//! `first_instance != 0` rasterize on this adapter, and under which device
//! configuration?
//!
//! Background (the Stage F "Metal bug"): glyph_scene.rs recorded that wgpu
//! 30.0.1's Metal backend "silently rasterizes nothing for any indirect draw
//! with first_instance != 0", and Stage F fell back to CPU culling + direct
//! range draws. Reading wgpu-core's indirect validation
//! (wgpu-core/src/indirect_validation/validate_draw.wgsl, line 67) shows a
//! WebGPU spec rule rather than a Metal bug: unless the device was created
//! with `Features::INDIRECT_FIRST_INSTANCE` ("indirect-first-instance"), the
//! validation compute pass that wgpu runs before every render pass with
//! indirect draws rewrites each draw whose first_instance is nonzero into a
//! zero-count draw in wgpu's own copy of the args — which is why Stage F's
//! readbacks of the APPLICATION buffer showed correct args while nothing
//! drew. gpu.rs requested `Features::empty()`; Metal advertises the feature.
//!
//! This tool pins that diagnosis empirically and repeatably: a matrix of
//! device configurations × draw forms, each rendering into a 64-column
//! offscreen target where every instance paints its own column with a tag
//! looked up by `instance_index` from a storage buffer (the glyph field's
//! access pattern). The readback yields the exact SET of instance indices
//! that rasterized, compared against the spec expectation:
//!   A  features=EMPTY, indirect validation on   → nonzero first_instance
//!                                                  draws are no-ops (spec)
//!   B  features=INDIRECT_FIRST_INSTANCE          → every draw lands
//!   C  features=EMPTY, validation OFF            → informational only (wgpu
//!                                                  documents this as UB; on
//!                                                  Metal base instance is
//!                                                  native and it just works)
//!
//! Exit 0 iff every A and B expectation holds. `--json` prints a machine-
//! readable summary; `--verbose` also dumps the compute-written args.
//!
//! Run: `cargo run --release --bin mdi-probe` (also executed by
//! `cargo test` via tests/mdi_probe.rs).

use std::collections::BTreeSet;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use wgpu::util::{DeviceExt, DrawIndexedIndirectArgs, DrawIndirectArgs};

/// Target width in columns == instances per wrap (instance_index % WIDTH).
const WIDTH: u32 = 64;
/// A few rows so the readback is a real 2D copy (rows are identical).
const HEIGHT: u32 = 2;
/// The payload buffer must cover the "large first_instance" case.
const PAYLOAD_LEN: u32 = 1_600_000;
const LARGE_FIRST_INSTANCE: u32 = 1_500_000;
/// Segments in the compute-cull case: seg i covers instances 8i..8i+4 and
/// occupies "position" [i, i+1) on a 1-D axis the view window tests against.
const CULL_SEGS: u32 = 8;

/// Render side: one column per instance, tagged with payload[instance_index].
const RENDER_WGSL: &str = r#"
struct Params { width: u32, _p0: u32, _p1: u32, _p2: u32 };
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> payload: array<u32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) tag: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let col = ii % params.width;
    let x0 = f32(col) / f32(params.width) * 2.0 - 1.0;
    let x1 = f32(col + 1u) / f32(params.width) * 2.0 - 1.0;
    var xs = array<f32, 6>(x0, x1, x1, x0, x1, x0);
    var ys = array<f32, 6>(-1.0, -1.0, 1.0, -1.0, 1.0, 1.0);
    var out: VsOut;
    out.pos = vec4<f32>(xs[vi], ys[vi], 0.0, 1.0);
    out.tag = payload[ii];
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<u32> {
    return vec4<u32>(in.tag, 1u, 0u, 0u);
}
"#;

/// Compute side: the cull-pass shape the glyph field uses — one thread per
/// segment writes a DrawIndirectArgs slot, zero instances when culled.
const CULL_WGSL: &str = r#"
struct Seg { base: u32, count: u32, lo: f32, hi: f32 };
struct View { lo: f32, hi: f32, seg_count: u32, _pad: u32 };
struct Args { vertex_count: u32, instance_count: u32, first_vertex: u32, first_instance: u32 };
@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<storage, read> segs: array<Seg>;
@group(0) @binding(2) var<storage, read_write> args: array<Args>;

@compute @workgroup_size(64)
fn cs_cull(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if i >= view.seg_count { return; }
    let s = segs[i];
    let visible = s.hi > view.lo && s.lo < view.hi;
    args[i] = Args(6u, select(0u, s.count, visible), 0u, s.base);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    width: u32,
    _p: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Seg {
    base: u32,
    count: u32,
    lo: f32,
    hi: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct View {
    lo: f32,
    hi: f32,
    seg_count: u32,
    _pad: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Config {
    /// What gpu.rs did through Stage J: no features, default instance flags.
    NoFeature,
    /// The fix: request INDIRECT_FIRST_INSTANCE.
    FirstInstance,
    /// No features but wgpu's indirect validation switched off (informational).
    NoValidation,
}

impl Config {
    const ALL: [Config; 3] = [Config::NoFeature, Config::FirstInstance, Config::NoValidation];

    fn letter(self) -> char {
        match self {
            Config::NoFeature => 'A',
            Config::FirstInstance => 'B',
            Config::NoValidation => 'C',
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Config::NoFeature => "features=EMPTY                   validation=on  (the Stage F-J device)",
            Config::FirstInstance => "features=INDIRECT_FIRST_INSTANCE validation=on  (the fix)",
            Config::NoValidation => "features=EMPTY                   validation=OFF (informational; wgpu: UB)",
        }
    }

    fn features(self) -> wgpu::Features {
        match self {
            Config::FirstInstance => wgpu::Features::INDIRECT_FIRST_INSTANCE,
            _ => wgpu::Features::empty(),
        }
    }

    /// Explicit, not from_env: WGPU_VALIDATION_INDIRECT_CALL in the
    /// environment must not silently change what this tool measures.
    fn instance_flags(self) -> wgpu::InstanceFlags {
        match self {
            Config::NoValidation => wgpu::InstanceFlags::empty(),
            _ => wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL,
        }
    }

    /// Does this configuration carry a hard expectation (affects exit code)?
    fn is_graded(self) -> bool {
        self != Config::NoValidation
    }
}

enum Form {
    Direct { first: u32, count: u32 },
    Indirect(DrawIndirectArgs),
    Multi(Vec<DrawIndirectArgs>),
    IndexedIndirect(DrawIndexedIndirectArgs),
    /// Compute-written args (CULL_SEGS slots) consumed by multi_draw_indirect.
    Cull { lo: f32, hi: f32 },
}

struct Case {
    name: &'static str,
    form: Form,
    /// Instance ids that must paint when the feature is enabled.
    with_feature: BTreeSet<u32>,
    /// Instance ids that must paint WITHOUT the feature (spec: only draws
    /// whose first_instance == 0 survive validation).
    without_feature: BTreeSet<u32>,
}

/// Instance-id set for one contiguous range.
fn ids(range: Range<u32>) -> BTreeSet<u32> {
    range.collect()
}

/// Instance-id set for several ranges.
fn ids_n(ranges: &[Range<u32>]) -> BTreeSet<u32> {
    ranges.iter().flat_map(|r| r.clone()).collect()
}

/// The spec no-op: nothing rasterizes.
fn none() -> BTreeSet<u32> {
    BTreeSet::new()
}

fn args(count: u32, first_instance: u32) -> DrawIndirectArgs {
    DrawIndirectArgs {
        vertex_count: 6,
        instance_count: count,
        first_vertex: 0,
        first_instance,
    }
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "direct draw(0..6, 5..10) [control]",
            form: Form::Direct { first: 5, count: 5 },
            with_feature: ids(5..10),
            without_feature: ids(5..10),
        },
        Case {
            name: "draw_indirect first_instance=0",
            form: Form::Indirect(args(5, 0)),
            with_feature: ids(0..5),
            without_feature: ids(0..5),
        },
        Case {
            name: "draw_indirect first_instance=7",
            form: Form::Indirect(args(5, 7)),
            with_feature: ids(7..12),
            without_feature: none(),
        },
        Case {
            name: "draw_indexed_indirect first_instance=9",
            form: Form::IndexedIndirect(DrawIndexedIndirectArgs {
                index_count: 6,
                instance_count: 3,
                first_index: 0,
                base_vertex: 0,
                first_instance: 9,
            }),
            with_feature: ids(9..12),
            without_feature: none(),
        },
        Case {
            name: "multi_draw_indirect x2 [fi=0 | fi=50]",
            form: Form::Multi(vec![args(2, 0), args(2, 50)]),
            with_feature: ids_n(&[0..2, 50..52]),
            without_feature: ids(0..2),
        },
        Case {
            name: "multi_draw_indirect x4 (one zero-instance pad slot)",
            form: Form::Multi(vec![args(3, 20), args(0, 25), args(2, 30), args(4, 40)]),
            with_feature: ids_n(&[20..23, 30..32, 40..44]),
            without_feature: none(),
        },
        Case {
            name: "compute cull -> multi_draw_indirect(8), view [2,5)",
            form: Form::Cull { lo: 2.0, hi: 5.0 },
            with_feature: ids_n(&[16..20, 24..28, 32..36]),
            without_feature: none(),
        },
        Case {
            name: "compute cull -> multi_draw_indirect(8), view [0,1.5)",
            form: Form::Cull { lo: 0.0, hi: 1.5 },
            with_feature: ids_n(&[0..4, 8..12]),
            without_feature: ids(0..4),
        },
        Case {
            name: "draw_indirect first_instance=1_500_000 (chunk-scale offset)",
            form: Form::Indirect(args(3, LARGE_FIRST_INSTANCE)),
            with_feature: ids(LARGE_FIRST_INSTANCE..LARGE_FIRST_INSTANCE + 3),
            without_feature: none(),
        },
    ]
}

/// One device configuration's worth of GPU state.
struct Probe {
    device: wgpu::Device,
    queue: wgpu::Queue,
    render_pipeline: wgpu::RenderPipeline,
    render_bg: wgpu::BindGroup,
    cull_pipeline: wgpu::ComputePipeline,
    cull_bg: wgpu::BindGroup,
    view_buf: wgpu::Buffer,
    args_buf: wgpu::Buffer,
    iargs_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    args_readback: wgpu::Buffer,
}

struct AdapterReport {
    name: String,
    backend: String,
    supports_first_instance: bool,
    supports_mdi_count: bool,
}

impl Probe {
    fn new(config: Config) -> (Self, AdapterReport) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: config.instance_flags(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("mdi-probe: no wgpu adapter found");
        let info = adapter.get_info();
        let adapter_features = adapter.features();
        let report = AdapterReport {
            name: info.name.clone(),
            backend: format!("{:?}", info.backend),
            supports_first_instance: adapter_features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE),
            supports_mdi_count: adapter_features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
        };
        let required_features = config.features();
        assert!(
            adapter_features.contains(required_features),
            "mdi-probe: adapter {:?} lacks {:?}",
            info.name,
            required_features
        );
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mdi-probe device"),
            required_features,
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .expect("mdi-probe: device request failed");

        // --- render side -----------------------------------------------------
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("probe params"),
            contents: bytemuck::bytes_of(&Params { width: WIDTH, _p: [0; 3] }),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // payload[i] = 3i + 1: invertible, never 0, so a painted column
        // identifies its instance index exactly (see decode()).
        let payload: Vec<u32> = (0..PAYLOAD_LEN).map(|i| i * 3 + 1).collect();
        let payload_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("probe payload"),
            contents: bytemuck::cast_slice(&payload),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let render_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe render bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let render_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("probe render bg"),
            layout: &render_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: payload_buf.as_entire_binding(),
                },
            ],
        });
        let render_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("probe render"),
            source: wgpu::ShaderSource::Wgsl(RENDER_WGSL.into()),
        });
        let render_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe render pl"),
            bind_group_layouts: &[Some(&render_bgl)],
            immediate_size: 0,
        });
        let format = wgpu::TextureFormat::Rgba32Uint;
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("probe render pipeline"),
            layout: Some(&render_pl),
            vertex: wgpu::VertexState {
                module: &render_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &render_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // --- compute side ----------------------------------------------------
        let segs: Vec<Seg> = (0..CULL_SEGS)
            .map(|i| Seg {
                base: 8 * i,
                count: 4,
                lo: i as f32,
                hi: (i + 1) as f32,
            })
            .collect();
        let segs_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("probe segs"),
            contents: bytemuck::cast_slice(&segs),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let view_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe view"),
            size: std::mem::size_of::<View>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let args_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe draw args"),
            size: (CULL_SEGS as usize * std::mem::size_of::<DrawIndirectArgs>()) as u64,
            usage: wgpu::BufferUsages::INDIRECT
                | wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let cull_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe cull bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
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
        let cull_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("probe cull bg"),
            layout: &cull_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: view_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: segs_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: args_buf.as_entire_binding(),
                },
            ],
        });
        let cull_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("probe cull"),
            source: wgpu::ShaderSource::Wgsl(CULL_WGSL.into()),
        });
        let cull_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe cull pl"),
            bind_group_layouts: &[Some(&cull_bgl)],
            immediate_size: 0,
        });
        let cull_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("probe cull pipeline"),
            layout: Some(&cull_pl),
            module: &cull_shader,
            entry_point: Some("cs_cull"),
            compilation_options: Default::default(),
            cache: None,
        });

        // --- indexed form + target + readbacks -------------------------------
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("probe indices"),
            contents: bytemuck::cast_slice(&[0u32, 1, 2, 3, 4, 5]),
            usage: wgpu::BufferUsages::INDEX,
        });
        let iargs_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe indexed draw args"),
            size: std::mem::size_of::<DrawIndexedIndirectArgs>() as u64,
            usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let size = wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("probe target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe readback"),
            size: (Self::bytes_per_row() * HEIGHT) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let args_readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe args readback"),
            size: args_buf.size(),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        (
            Self {
                device,
                queue,
                render_pipeline,
                render_bg,
                cull_pipeline,
                cull_bg,
                view_buf,
                args_buf,
                iargs_buf,
                index_buf,
                target,
                target_view,
                readback,
                args_readback,
            },
            report,
        )
    }

    /// Rgba32Uint = 16 B/texel; 64 texels = 1024 B, already 256-aligned.
    fn bytes_per_row() -> u32 {
        (WIDTH * 16).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
    }

    /// Run one case: (painted instance ids, compute-written args if any).
    fn run(&self, form: &Form) -> (BTreeSet<u32>, Option<Vec<DrawIndirectArgs>>) {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        match form {
            Form::Direct { .. } => {}
            Form::Indirect(a) => self.queue.write_buffer(&self.args_buf, 0, bytemuck::bytes_of(a)),
            Form::Multi(v) => self.queue.write_buffer(&self.args_buf, 0, bytemuck::cast_slice(v)),
            Form::IndexedIndirect(a) => self.queue.write_buffer(&self.iargs_buf, 0, bytemuck::bytes_of(a)),
            Form::Cull { lo, hi } => {
                // Stale-slot guard: zero everything, then let the shader
                // rewrite all CULL_SEGS slots in the same command buffer.
                self.queue
                    .write_buffer(&self.args_buf, 0, &vec![0u8; self.args_buf.size() as usize]);
                self.queue.write_buffer(
                    &self.view_buf,
                    0,
                    bytemuck::bytes_of(&View {
                        lo: *lo,
                        hi: *hi,
                        seg_count: CULL_SEGS,
                        _pad: 0,
                    }),
                );
                let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("probe cull"),
                    timestamp_writes: None,
                });
                cpass.set_pipeline(&self.cull_pipeline);
                cpass.set_bind_group(0, &self.cull_bg, &[]);
                cpass.dispatch_workgroups(1, 1, 1);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("probe pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&self.render_pipeline);
            pass.set_bind_group(0, &self.render_bg, &[]);
            match form {
                Form::Direct { first, count } => pass.draw(0..6, *first..*first + *count),
                Form::Indirect(_) => pass.draw_indirect(&self.args_buf, 0),
                Form::Multi(v) => pass.multi_draw_indirect(&self.args_buf, 0, v.len() as u32),
                Form::IndexedIndirect(_) => {
                    pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed_indirect(&self.iargs_buf, 0);
                }
                Form::Cull { .. } => pass.multi_draw_indirect(&self.args_buf, 0, CULL_SEGS),
            }
        }
        let size = wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(Self::bytes_per_row()),
                    rows_per_image: Some(HEIGHT),
                },
            },
            size,
        );
        let wants_args = matches!(form, Form::Cull { .. });
        if wants_args {
            encoder.copy_buffer_to_buffer(&self.args_buf, 0, &self.args_readback, 0, self.args_buf.size());
        }
        self.queue.submit([encoder.finish()]);

        let pixels = self.map_and_copy(&self.readback);
        let painted = decode(&pixels);
        let args = wants_args.then(|| {
            let bytes = self.map_and_copy(&self.args_readback);
            bytemuck::pod_collect_to_vec::<u8, DrawIndirectArgs>(&bytes)
        });
        (painted, args)
    }

    /// Block until the GPU finished, copy a MAP_READ buffer out, unmap.
    fn map_and_copy(&self, buf: &wgpu::Buffer) -> Vec<u8> {
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("mdi-probe: device poll failed");
        rx.recv()
            .expect("mdi-probe: map_async callback dropped")
            .expect("mdi-probe: buffer map failed");
        let out = slice
            .get_mapped_range()
            .expect("mdi-probe: readback range not mapped")
            .to_vec();
        buf.unmap();
        out
    }
}

/// Row 0 of the target: texel x = [tag, painted, 0, 0]. tag = 3·id + 1.
fn decode(pixels: &[u8]) -> BTreeSet<u32> {
    let texels = bytemuck::pod_collect_to_vec::<u8, u32>(&pixels[..(WIDTH * 16) as usize]);
    let mut ids = BTreeSet::new();
    for x in 0..WIDTH as usize {
        let tag = texels[x * 4];
        let painted = texels[x * 4 + 1];
        if painted == 0 {
            continue;
        }
        assert!(
            painted == 1 && tag >= 1 && (tag - 1).is_multiple_of(3),
            "mdi-probe: corrupt texel at column {x}: tag={tag} painted={painted}"
        );
        let id = (tag - 1) / 3;
        assert!(
            id % WIDTH == x as u32,
            "mdi-probe: column {x} painted by instance {id} (expected id % {WIDTH} == {x})"
        );
        ids.insert(id);
    }
    ids
}

/// "{0..4, 8..12}" (half-open runs) or "∅".
fn fmt_ids(ids: &BTreeSet<u32>) -> String {
    if ids.is_empty() {
        return "∅".to_string();
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &id in ids {
        match runs.last_mut() {
            Some((_, end)) if *end == id => *end += 1,
            _ => runs.push((id, id + 1)),
        }
    }
    let parts: Vec<String> = runs
        .iter()
        .map(|(s, e)| if e - s == 1 { s.to_string() } else { format!("{s}..{e}") })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn json_ids(ids: &BTreeSet<u32>) -> String {
    let v: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    format!("[{}]", v.join(","))
}

struct Observation {
    painted: BTreeSet<u32>,
    expected: BTreeSet<u32>,
    args: Option<Vec<DrawIndirectArgs>>,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let json = argv.iter().any(|a| a.as_str() == "--json");
    let verbose = argv.iter().any(|a| a.as_str() == "--verbose");
    if let Some(bad) = argv
        .iter()
        .find(|a| a.as_str() != "--json" && a.as_str() != "--verbose")
    {
        eprintln!("mdi-probe: unknown argument {bad:?} (accepted: --json --verbose)");
        std::process::exit(2);
    }

    let cases = cases();
    let mut adapter_report: Option<AdapterReport> = None;
    // per config: per case observation
    let mut results: Vec<(Config, Vec<Observation>)> = Vec::new();
    for config in Config::ALL {
        let (probe, report) = Probe::new(config);
        adapter_report.get_or_insert(report);
        let mut obs = Vec::with_capacity(cases.len());
        for case in &cases {
            let (painted, args) = probe.run(&case.form);
            let expected = match config {
                Config::NoFeature => case.without_feature.clone(),
                // C is compared against B's contract for display only.
                Config::FirstInstance | Config::NoValidation => case.with_feature.clone(),
            };
            obs.push(Observation {
                painted,
                expected,
                args,
            });
        }
        results.push((config, obs));
    }
    let adapter = adapter_report.expect("mdi-probe: at least one configuration ran");

    let mut graded = 0usize;
    let mut met = 0usize;
    for (config, obs) in &results {
        if !config.is_graded() {
            continue;
        }
        for o in obs {
            graded += 1;
            if o.painted == o.expected {
                met += 1;
            }
        }
    }
    let pass = met == graded;

    if json {
        let mut configs_json = Vec::new();
        for (config, obs) in &results {
            let mut case_json = Vec::new();
            for (case, o) in cases.iter().zip(obs) {
                case_json.push(format!(
                    "{{\"name\":{:?},\"observed\":{},\"expected\":{},\"ok\":{}}}",
                    case.name,
                    json_ids(&o.painted),
                    json_ids(&o.expected),
                    o.painted == o.expected
                ));
            }
            let features = format!("{:?}", config.features());
            let validation = config
                .instance_flags()
                .contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
            configs_json.push(format!(
                "{{\"config\":\"{}\",\"graded\":{},\"features\":{features:?},\"indirect_validation\":{validation},\"cases\":[{}]}}",
                config.letter(),
                config.is_graded(),
                case_json.join(",")
            ));
        }
        println!(
            "{{\"adapter\":{:?},\"backend\":{:?},\"adapter_supports_indirect_first_instance\":{},\"adapter_supports_multi_draw_indirect_count\":{},\"configs\":[{}],\"expectations_met\":{met},\"expectations_graded\":{graded},\"pass\":{pass}}}",
            adapter.name,
            adapter.backend,
            adapter.supports_first_instance,
            adapter.supports_mdi_count,
            configs_json.join(",")
        );
    } else {
        println!("mdi-probe — indirect draws with first_instance != 0: device-configuration matrix");
        println!(
            "adapter: {} ({}) | adapter features: INDIRECT_FIRST_INSTANCE={} MULTI_DRAW_INDIRECT_COUNT={}",
            adapter.name, adapter.backend, adapter.supports_first_instance, adapter.supports_mdi_count
        );
        for config in Config::ALL {
            println!("  {}: {}", config.letter(), config.describe());
        }
        println!();
        let name_w = cases.iter().map(|c| c.name.len()).max().unwrap_or(4);
        println!(
            "{:<name_w$} | {:<30} | {:<30} | {:<30}",
            "case", "A observed", "B observed", "C observed [info]"
        );
        println!("{}", "-".repeat(name_w + 3 + 33 * 3));
        for (ci, case) in cases.iter().enumerate() {
            let cell = |config_idx: usize| -> String {
                let o = &results[config_idx].1[ci];
                if results[config_idx].0.is_graded() {
                    let mark = if o.painted == o.expected { "ok" } else { "MISMATCH" };
                    format!("{} {}", fmt_ids(&o.painted), mark)
                } else {
                    fmt_ids(&o.painted)
                }
            };
            println!("{:<name_w$} | {:<30} | {:<30} | {:<30}", case.name, cell(0), cell(1), cell(2));
        }
        println!();
        for (config, obs) in &results {
            for (case, o) in cases.iter().zip(obs) {
                if config.is_graded() && o.painted != o.expected {
                    println!(
                        "MISMATCH [{}] {}: observed {} expected {}",
                        config.letter(),
                        case.name,
                        fmt_ids(&o.painted),
                        fmt_ids(&o.expected)
                    );
                }
            }
        }
        if verbose {
            for (config, obs) in &results {
                for (case, o) in cases.iter().zip(obs) {
                    if let Some(args) = &o.args {
                        let dump: Vec<String> = args
                            .iter()
                            .map(|a| {
                                format!(
                                    "[vc={} ic={} fv={} fi={}]",
                                    a.vertex_count, a.instance_count, a.first_vertex, a.first_instance
                                )
                            })
                            .collect();
                        println!("args [{}] {}: {}", config.letter(), case.name, dump.join(" "));
                    }
                }
            }
        }
        println!(
            "RESULT: {} ({met}/{graded} graded expectations met; A = spec no-op without the feature, B = draws land with it)",
            if pass { "PASS" } else { "FAIL" }
        );
    }
    if !pass {
        std::process::exit(1);
    }
}
