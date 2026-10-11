//! The library's page faces, drawn from the transform tree (2026-10-10).
//!
//! One quad behind every sheet (`[library] page_faces`), drawn by the scene
//! mesh pipeline (`mesh.wgsl`, unchanged) from an instance buffer that the
//! HOST NEVER WRITES: after the scene graph's resolve pass, a compute pass
//! (`shaders/library_faces.wgsl`) reads each face's sheet world row and
//! writes its 80 B `MeshInstance`. A page turn, a form change or a directory
//! drag therefore costs the faces nothing on the CPU and no bytes on the
//! queue; before, every face was a bevy scene mesh and any moved sheet
//! re-extracted and re-uploaded all of them (2.35 MB at 29k files; findings
//! E5, E16). The pass runs on frames whose flush dispatched a resolve (one
//! thread per face — microseconds at the kernel tree's 74k) and never on a
//! settled frame. `face_instance` is its CPU twin, held to bevy's extraction
//! in `library::tests`.

use std::cell::RefCell;

use bytemuck::{Pod, Zeroable};


use super::mesh::MeshInstance;
use crate::library::FaceSpec;

/// `FaceParams` in library_faces.wgsl, 64 B.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FaceParamsGpu {
    offset: [f32; 4],
    quad: [f32; 4],
    color: [f32; 4],
    count: u32,
    _pad: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<FaceParamsGpu>() == 64, "FaceParams in library_faces.wgsl is 64 B");

const WG: u32 = 64;

/// The face of a sheet whose world is `sheet`: library_faces.wgsl's
/// arithmetic, term for term, on the CPU.
#[cfg(test)]
pub(crate) fn face_instance(sheet: &glyph_scene_graph::Similarity, spec: &FaceSpec) -> MeshInstance {
    let s = sheet.scale;
    let q = sheet.rotation;
    let off = [0.0, 0.0, -spec.gap];
    let r = glyph_scene_graph::transform::rotate(q, [off[0] * s, off[1] * s, off[2] * s]);
    let t = [sheet.translation[0] + r[0], sheet.translation[1] + r[1], sheet.translation[2] + r[2]];
    let (x2, y2, z2) = (q[0] + q[0], q[1] + q[1], q[2] + q[2]);
    let (xx, xy, xz) = (q[0] * x2, q[0] * y2, q[0] * z2);
    let (yy, yz, zz) = (q[1] * y2, q[1] * z2, q[2] * z2);
    let (wx, wy, wz) = (q[3] * x2, q[3] * y2, q[3] * z2);
    let mx = [(1.0 - (yy + zz)) * s, (xy + wz) * s, (xz - wy) * s];
    let my = [(xy - wz) * s, (1.0 - (xx + zz)) * s, (yz + wx) * s];
    let mz = [(xz + wy) * s, (yz - wx) * s, (1.0 - (xx + yy)) * s];
    let dot = |v: [f32; 3]| v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if dot(mx) + dot(my) + dot(mz) < 1e-6 {
        return MeshInstance::zeroed();
    }
    let (size, origin) = (spec.size, [-spec.size[0] / 2.0, -spec.size[1] / 2.0]);
    let centre = [origin[0] + size[0] * 0.5, origin[1] + size[1] * 0.5];
    let at = [0, 1, 2].map(|k| t[k] + (mx[k] * centre[0] + my[k] * centre[1]));
    MeshInstance {
        world_col0: [mx[0] * size[0], mx[1] * size[0], mx[2] * size[0], at[0]],
        world_col1: [my[0] * size[1], my[1] * size[1], my[2] * size[1], at[1]],
        world_col2: [mz[0], mz[1], mz[2], at[2]],
        color: spec.color,
        material_params: [0.0; 4],
    }
}

pub(crate) struct LibraryFaces {
    count: u32,
    /// The instances mesh.wgsl draws (VERTEX) and the pass writes (STORAGE).
    pub(crate) instances: wgpu::Buffer,
    sheets: wgpu::Buffer,
    params: wgpu::Buffer,
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    /// Rebuilt when the scene graph's world table is replaced (it grows).
    bind_group: RefCell<Option<wgpu::BindGroup>>,
}

impl LibraryFaces {
    /// One face per entry of `sheet_slots` (each a sheet node's pool slot),
    /// in draw order.
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue, spec: &FaceSpec, sheet_slots: &[u32]) -> Self {
        use wgpu::util::DeviceExt;
        let count = sheet_slots.len() as u32;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("library face instances"),
            size: (u64::from(count.max(1))) * std::mem::size_of::<MeshInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let sheets = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("library face sheets"),
            contents: bytemuck::cast_slice(if sheet_slots.is_empty() { &[0u32] } else { sheet_slots }),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let p = FaceParamsGpu {
            offset: [0.0, 0.0, -spec.gap, 0.0],
            quad: [spec.size[0], spec.size[1], -spec.size[0] / 2.0, -spec.size[1] / 2.0],
            color: spec.color,
            count,
            _pad: [0; 3],
        };
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("library face params"),
            size: std::mem::size_of::<FaceParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params, 0, bytemuck::bytes_of(&p));
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("library faces bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                storage(1, true),
                storage(2, true),
                storage(3, false),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("library_faces.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/library_faces.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("library faces pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("library faces"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("write_faces"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { count, instances, sheets, params, pipeline, bgl, bind_group: RefCell::new(None) }
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Encode the face pass after the resolve: every face from its sheet's
    /// world row. `world` is the scene graph's world table; `rebind` when
    /// it was replaced since the last pass.
    pub(crate) fn encode(&self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, world: &wgpu::Buffer, rebind: bool) {
        if self.count == 0 {
            return;
        }
        let mut bg = self.bind_group.borrow_mut();
        if rebind || bg.is_none() {
            *bg = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("library faces bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: world.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.sheets.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: self.instances.as_entire_binding() },
                ],
            }));
        }
        let groups = self.count.div_ceil(WG);
        assert!(groups <= 65_535, "library faces: {} faces is past one dispatch row", self.count);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("library faces"), timestamp_writes: None });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bg.as_ref().expect("just built"), &[]);
        pass.dispatch_workgroups(groups, 1, 1);
    }
}

#[cfg(test)]
mod tests {
    use glyph_scene_graph::gpu::read_buffer_words;
    use glyph_scene_graph::{Appearance, NodeHandle, SceneGraph, Similarity};

    use super::{face_instance, LibraryFaces, MeshInstance};
    use crate::glyph_scene::GroupRow;
    use crate::library::plan::LibFile;
    use crate::library::{Form, FormCmd, Library};

    /// The GPU half of the library on the tree (2026-10-10): the resolve's
    /// group rows and the face pass's instances, read back, against their
    /// CPU twins (`seated_rows`, `face_instance`) bit for bit — seated and
    /// mid-ease. The CPU twins are held to bevy's composition and extraction
    /// by `library::tests::the_tree_resolves_the_rows_bevy_composed`, so
    /// together they say the GPU draws what bevy drew. Every step of a
    /// library chain is a product with 1 or 0 or a sum with 0 bar the one
    /// offset sum, so nothing the GPU fuses can round differently.
    #[test]
    fn the_gpu_resolves_the_library_rows_and_faces_the_cpu_does() {
        let ctx = pollster::block_on(crate::gpu::init(None));
        let s = &crate::config::settings().library;
        let file = |p: &str, w: f32, h: f32| LibFile { rel_path: p.to_string(), ink_min: [0.0, -h, -3.0], ink_max: [w, 0.0, 0.0] };
        let files = vec![
            file("a.rs", 41.3, 17.9),
            file("src/b.rs", 512.7, 1033.1),
            file("src/c.rs", 3.1, 1.25),
            file("src/deep/x/y.py", 97.0, 211.6),
            file("src/deep/z.md", 13.37, 900.0),
            file("docs/readme.md", 77.7, 66.6),
        ];
        let tints: Vec<[f32; 3]> = (0..files.len()).map(|i| [0.15 * i as f32, 0.5, 0.9]).collect();
        let mut lib = Library::new(files.clone(), s);
        let mut g = SceneGraph::new(&ctx.device);
        let n = files.len();
        let table = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test group table"),
            size: (n * std::mem::size_of::<GroupRow>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let start: Vec<GroupRow> = tints.iter().map(|t| GroupRow::tinted([0.0; 3], *t)).collect();
        ctx.queue.write_buffer(&table, 0, bytemuck::cast_slice(&start));
        g.gpu.set_group_output(&ctx.device, &table, n as u32);
        let root = g.tables.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
        let file_nodes: Vec<NodeHandle> = tints
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let h = g.tables.insert(Some(root), Similarity::IDENTITY, Appearance { tint: [t[0], t[1], t[2], 1.0], ..Appearance::IDENTITY }).unwrap();
                g.tables.set_group_row(h, Some(i as u32)).unwrap();
                h
            })
            .collect();
        lib.attach(&mut g.tables, Some(root), &file_nodes);
        let spec = lib.face.expect("the default settings draw faces");
        let nodes = lib.nodes.clone().unwrap();
        let slots: Vec<u32> = nodes.sheet.iter().map(|h| h.index()).collect();
        let faces = LibraryFaces::new(&ctx.device, &ctx.queue, &spec, &slots);

        let check = |g: &mut SceneGraph, what: &str| {
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("test frame") });
            let st = g.flush(&ctx.device, &ctx.queue, &mut encoder);
            assert!(st.dispatched, "{what}: something moved");
            faces.encode(&ctx.device, &mut encoder, g.gpu.world_buffer(), true);
            ctx.queue.submit([encoder.finish()]);
            let rows: Vec<GroupRow> = bytemuck::cast_slice(&read_buffer_words(&ctx.device, &ctx.queue, &table, (n * 96) as u64)).to_vec();
            let want = crate::library::seated_rows(&g.tables, &nodes, &tints);
            for (i, (a, b)) in rows.iter().zip(&want).enumerate() {
                let (a, b): (&[u32; 24], &[u32; 24]) = (bytemuck::cast_ref(&a.cols), bytemuck::cast_ref(&b.cols));
                assert_eq!(a[..16], b[..16], "{what}: file {i}'s group row, GPU vs CPU");
            }
            let got: Vec<MeshInstance> =
                bytemuck::cast_slice(&read_buffer_words(&ctx.device, &ctx.queue, &faces.instances, (n * 80) as u64)).to_vec();
            for (i, (a, &sheet)) in got.iter().zip(&nodes.sheet).enumerate() {
                let b = face_instance(&g.tables.world(sheet).unwrap().0, &spec);
                assert_eq!(bytemuck::bytes_of(a), bytemuck::bytes_of(&b), "{what}: face {i}, GPU {a:?} vs CPU {b:?}");
            }
        };
        check(&mut g, "seated");
        lib.form(&g.tables, None, FormCmd::Set(Form::Splay));
        for _ in 0..7 {
            lib.tick(&mut g.tables, 1.0 / 60.0).expect("animating");
        }
        check(&mut g, "mid splay");
    }
}
