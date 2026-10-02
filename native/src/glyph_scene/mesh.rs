use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// A single vertex of a generic 3D mesh.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

/// Instance data for rendering a generic 3D mesh.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MeshInstance {
    pub world_col0: [f32; 4],
    pub world_col1: [f32; 4],
    pub world_col2: [f32; 4],
    pub color: [f32; 4],
    pub material_params: [f32; 4],
}

impl MeshInstance {
    /// Create instance data from an affine transform, color, and material parameters.
    pub fn from_affine(affine: glam::Affine3A, color: [f32; 4], material_params: [f32; 4]) -> Self {
        Self {
            world_col0: [
                affine.matrix3.x_axis.x,
                affine.matrix3.x_axis.y,
                affine.matrix3.x_axis.z,
                affine.translation.x,
            ],
            world_col1: [
                affine.matrix3.y_axis.x,
                affine.matrix3.y_axis.y,
                affine.matrix3.y_axis.z,
                affine.translation.y,
            ],
            world_col2: [
                affine.matrix3.z_axis.x,
                affine.matrix3.z_axis.y,
                affine.matrix3.z_axis.z,
                affine.translation.z,
            ],
            color,
            material_params,
        }
    }
}

/// The unit geometry to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitMesh {
    /// A quad centered at origin, lying in the XY plane (Z=0), size 1x1.
    Quad,
    /// A cube centered at origin, size 1x1x1.
    Cube,
}

const QUAD_VERTS: &[MeshVertex] = &[
    MeshVertex { position: [-0.5,  0.5, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0, 0.0] },
    MeshVertex { position: [-0.5, -0.5, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0, 1.0] },
    MeshVertex { position: [ 0.5, -0.5, 0.0], normal: [0.0, 0.0, 1.0], uv: [1.0, 1.0] },
    MeshVertex { position: [ 0.5,  0.5, 0.0], normal: [0.0, 0.0, 1.0], uv: [1.0, 0.0] },
];
const QUAD_IDXS: &[u32] = &[0, 1, 2, 0, 2, 3];

const CUBE_VERTS: &[MeshVertex] = &[
    // +Z Front
    MeshVertex { position: [-0.5,  0.5,  0.5], normal: [0.0, 0.0, 1.0], uv: [0.0, 0.0] },
    MeshVertex { position: [-0.5, -0.5,  0.5], normal: [0.0, 0.0, 1.0], uv: [0.0, 1.0] },
    MeshVertex { position: [ 0.5, -0.5,  0.5], normal: [0.0, 0.0, 1.0], uv: [1.0, 1.0] },
    MeshVertex { position: [ 0.5,  0.5,  0.5], normal: [0.0, 0.0, 1.0], uv: [1.0, 0.0] },
    // -Z Back
    MeshVertex { position: [ 0.5,  0.5, -0.5], normal: [0.0, 0.0, -1.0], uv: [0.0, 0.0] },
    MeshVertex { position: [ 0.5, -0.5, -0.5], normal: [0.0, 0.0, -1.0], uv: [0.0, 1.0] },
    MeshVertex { position: [-0.5, -0.5, -0.5], normal: [0.0, 0.0, -1.0], uv: [1.0, 1.0] },
    MeshVertex { position: [-0.5,  0.5, -0.5], normal: [0.0, 0.0, -1.0], uv: [1.0, 0.0] },
    // -X Left
    MeshVertex { position: [-0.5,  0.5, -0.5], normal: [-1.0, 0.0, 0.0], uv: [0.0, 0.0] },
    MeshVertex { position: [-0.5, -0.5, -0.5], normal: [-1.0, 0.0, 0.0], uv: [0.0, 1.0] },
    MeshVertex { position: [-0.5, -0.5,  0.5], normal: [-1.0, 0.0, 0.0], uv: [1.0, 1.0] },
    MeshVertex { position: [-0.5,  0.5,  0.5], normal: [-1.0, 0.0, 0.0], uv: [1.0, 0.0] },
    // +X Right
    MeshVertex { position: [ 0.5,  0.5,  0.5], normal: [1.0, 0.0, 0.0], uv: [0.0, 0.0] },
    MeshVertex { position: [ 0.5, -0.5,  0.5], normal: [1.0, 0.0, 0.0], uv: [0.0, 1.0] },
    MeshVertex { position: [ 0.5, -0.5, -0.5], normal: [1.0, 0.0, 0.0], uv: [1.0, 1.0] },
    MeshVertex { position: [ 0.5,  0.5, -0.5], normal: [1.0, 0.0, 0.0], uv: [1.0, 0.0] },
    // +Y Top
    MeshVertex { position: [-0.5,  0.5, -0.5], normal: [0.0, 1.0, 0.0], uv: [0.0, 0.0] },
    MeshVertex { position: [-0.5,  0.5,  0.5], normal: [0.0, 1.0, 0.0], uv: [0.0, 1.0] },
    MeshVertex { position: [ 0.5,  0.5,  0.5], normal: [0.0, 1.0, 0.0], uv: [1.0, 1.0] },
    MeshVertex { position: [ 0.5,  0.5, -0.5], normal: [0.0, 1.0, 0.0], uv: [1.0, 0.0] },
    // -Y Bottom
    MeshVertex { position: [-0.5, -0.5,  0.5], normal: [0.0, -1.0, 0.0], uv: [0.0, 0.0] },
    MeshVertex { position: [-0.5, -0.5, -0.5], normal: [0.0, -1.0, 0.0], uv: [0.0, 1.0] },
    MeshVertex { position: [ 0.5, -0.5, -0.5], normal: [0.0, -1.0, 0.0], uv: [1.0, 1.0] },
    MeshVertex { position: [ 0.5, -0.5,  0.5], normal: [0.0, -1.0, 0.0], uv: [1.0, 0.0] },
];
const CUBE_IDXS: &[u32] = &[
    0, 1, 2, 0, 2, 3,       // Front
    4, 5, 6, 4, 6, 7,       // Back
    8, 9, 10, 8, 10, 11,    // Left
    12, 13, 14, 12, 14, 15, // Right
    16, 17, 18, 16, 18, 19, // Top
    20, 21, 22, 20, 22, 23, // Bottom
];

pub struct MeshPipeline {
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    quad_indices: std::ops::Range<u32>,
    cube_indices: std::ops::Range<u32>,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    last_revision: u64,
}

impl MeshPipeline {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        frame_bgl: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mesh.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/mesh.wgsl").into()),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mesh pipeline layout"),
            bind_group_layouts: &[Some(frame_bgl)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mesh pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<MeshVertex>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Float32x3,
                            1 => Float32x3,
                            2 => Float32x2,
                        ],
                    }),
                    Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<MeshInstance>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![
                            3 => Float32x4,
                            4 => Float32x4,
                            5 => Float32x4,
                            6 => Float32x4,
                            7 => Float32x4,
                        ],
                    }),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState {
                    constant: -50,
                    slope_scale: -1.0,
                    clamp: 0.0,
                },
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let mut all_verts = Vec::new();
        let mut all_idxs = Vec::new();

        let quad_start_idx = all_idxs.len() as u32;
        let quad_base_vert = all_verts.len() as u32;
        all_verts.extend_from_slice(QUAD_VERTS);
        for &i in QUAD_IDXS {
            all_idxs.push(i + quad_base_vert);
        }
        let quad_end_idx = all_idxs.len() as u32;

        let cube_start_idx = all_idxs.len() as u32;
        let cube_base_vert = all_verts.len() as u32;
        all_verts.extend_from_slice(CUBE_VERTS);
        for &i in CUBE_IDXS {
            all_idxs.push(i + cube_base_vert);
        }
        let cube_end_idx = all_idxs.len() as u32;

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh static vertices"),
            contents: bytemuck::cast_slice(&all_verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh static indices"),
            contents: bytemuck::cast_slice(&all_idxs),
            usage: wgpu::BufferUsages::INDEX,
        });

        let instance_capacity = 64;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh instances"),
            size: (instance_capacity * std::mem::size_of::<MeshInstance>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            vertex_buffer,
            index_buffer,
            quad_indices: quad_start_idx..quad_end_idx,
            cube_indices: cube_start_idx..cube_end_idx,
            instance_buffer,
            instance_capacity,
            last_revision: 0,
        }
    }

    pub fn ensure_instance_capacity(&mut self, device: &wgpu::Device, count: usize) {
        if self.instance_capacity >= count {
            return;
        }
        let mut new_cap = self.instance_capacity * 2;
        while new_cap < count {
            new_cap *= 2;
        }
        self.instance_capacity = new_cap;
        self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh instances"),
            size: (new_cap * std::mem::size_of::<MeshInstance>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
    }

    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, draws: &crate::spatial_scene::SceneMeshDraws) {
        if self.last_revision == draws.revision && draws.revision != 0 {
            return;
        }
        self.last_revision = draws.revision;
        let quads = &draws.quads;
        let cubes = &draws.cubes;
        
        self.ensure_instance_capacity(device, quads.len() + cubes.len());
        if !quads.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(quads));
        }
        if !cubes.is_empty() {
            let offset = std::mem::size_of_val(quads.as_slice()) as u64;
            queue.write_buffer(&self.instance_buffer, offset, bytemuck::cast_slice(cubes));
        }
    }

    /// Renders instances of a unit mesh.
    /// Expected usage: call prepare before this.
    pub fn render<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        mesh_type: UnitMesh,
        instances_range: std::ops::Range<u32>,
        bind_group: &'a wgpu::BindGroup,
    ) {
        if instances_range.is_empty() {
            return;
        }

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);

        let indices = match mesh_type {
            UnitMesh::Quad => self.quad_indices.clone(),
            UnitMesh::Cube => self.cube_indices.clone(),
        };

        pass.draw_indexed(indices, 0, instances_range);
    }
}
