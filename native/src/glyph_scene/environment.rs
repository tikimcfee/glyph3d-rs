//! The ground/sky environment pass: `shaders/environment.wgsl`, drawn first
//! in the glyph field pass when the environment is on (`--environment
//! ground`, `[environment] mode`, or the windowed `B` toggle). Off by
//! default, and when off it records nothing — the pass's command stream is
//! exactly what it was before this module existed.
//!
//! The ground sits at `ground_y`: an explicit `--ground-y`, else the scene's
//! lowest point minus `[environment] ground_gap`, so nothing rests exactly on
//! the plane. (Text hangs DOWN from each file's origin, so a ground at y = 0
//! would put the corpus underground; lifting the layout onto y = 0 is a
//! separate, layout-moving change.)
//!
//! Every colour, spacing and distance comes from `[environment]` in
//! config/defaults.toml.

use std::cell::Cell;

use bytemuck::{Pod, Zeroable};
use glam::{DVec3, Mat4};
use wgpu::util::DeviceExt;

use super::target::{POOL_FORMAT, SCENE_SAMPLE_COUNT};
use crate::config::EnvironmentMode;

/// `struct Env` in environment.wgsl: two mat4 then eleven vec4, 304 B. Pinned
/// on both sides — here and in `native/tests/wgsl.rs` (naga's span).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct EnvUniform {
    inv_vp_rel: [f32; 16],
    vp_rel: [f32; 16],
    eye: [f32; 4],
    grid_origin: [f32; 4],
    spacing: [f32; 4],
    fade: [f32; 4],
    ground: [f32; 4],
    minor_line: [f32; 4],
    major_line: [f32; 4],
    axis_x: [f32; 4],
    axis_z: [f32; 4],
    sky_horizon: [f32; 4],
    sky_zenith: [f32; 4],
}

/// What the environment needs from the frame's camera.
pub(super) struct EnvCamera {
    /// The eye, world space.
    pub(super) eye: DVec3,
    /// proj × view with the view's translation zeroed (camera-relative).
    pub(super) vp_rel: Mat4,
    /// The frame's far plane distance (the fog ends before it).
    pub(super) far: f32,
    /// The scene's fit distance: the unit the fog is measured in.
    pub(super) fit: f32,
}

pub(super) struct Environment {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pub(super) mode: Cell<EnvironmentMode>,
    pub(super) ground_y_override: Cell<Option<f32>>,
}

impl Environment {
    pub(super) fn new(device: &wgpu::Device, depth_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("environment.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/environment.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("environment bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("environment pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("environment pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[], // full-screen triangle from vertex_index
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: POOL_FORMAT,
                    blend: None, // opaque: it IS the background
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(true),
                // Reverse-Z like every other pass; drawn first, so it only
                // ever meets the cleared 0.0 and always passes.
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: SCENE_SAMPLE_COUNT,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        });
        let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("environment uniform"),
            contents: bytemuck::bytes_of(&EnvUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("environment bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform_buf,
            bind_group,
            mode: Cell::new(EnvironmentMode::Off),
            ground_y_override: Cell::new(None),
        }
    }

    pub(super) fn is_on(&self) -> bool {
        self.mode.get() != EnvironmentMode::Off
    }

    /// Flip between off and ground (the windowed `B` key).
    pub(super) fn toggle(&self) -> EnvironmentMode {
        let next = match self.mode.get() {
            EnvironmentMode::Off => EnvironmentMode::Ground,
            EnvironmentMode::Ground => EnvironmentMode::Off,
        };
        self.mode.set(next);
        next
    }

    /// The plane's height: the override, else `scene_min_y` minus the gap.
    pub(super) fn ground_y(&self, scene_min_y: f32) -> f32 {
        self.ground_y_override
            .get()
            .unwrap_or_else(|| scene_min_y - crate::config::settings().environment.ground_gap)
    }

    /// Upload this frame's uniform. Call only when on.
    pub(super) fn write(&self, queue: &wgpu::Queue, cam: &EnvCamera, ground_y: f32) {
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniform(cam, ground_y)));
    }

    /// Record the draw. Call first in the glyph field pass, only when on.
    pub(super) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn uniform(cam: &EnvCamera, ground_y: f32) -> EnvUniform {
    let e = &crate::config::settings().environment;
    // The eye's xz phase within each grid period, reduced in f64 so a far
    // eye (~1e4) costs the grid no precision.
    let phase = |spacing: f32| {
        let s = spacing as f64;
        [cam.eye.x.rem_euclid(s) as f32, cam.eye.z.rem_euclid(s) as f32]
    };
    let [mx, mz] = phase(e.minor_spacing);
    let [jx, jz] = phase(e.major_spacing);
    let fog_end = (e.fog_end_fit * cam.fit).min(cam.far * e.fog_far_fraction);
    let rgb1 = |c: [f32; 3], a: f32| [c[0], c[1], c[2], a];
    EnvUniform {
        inv_vp_rel: cam.vp_rel.inverse().to_cols_array(),
        vp_rel: cam.vp_rel.to_cols_array(),
        eye: [cam.eye.x as f32, cam.eye.y as f32, cam.eye.z as f32, ground_y],
        grid_origin: [mx, mz, jx, jz],
        spacing: [e.minor_spacing, e.major_spacing, e.line_width_px, e.axis_width_px],
        fade: [(e.fog_start_fit * cam.fit).min(fog_end), fog_end, e.line_fade_start_px, e.line_fade_end_px],
        ground: rgb1(e.ground_color, 1.0),
        minor_line: e.minor_line_color,
        major_line: e.major_line_color,
        axis_x: e.axis_x_color,
        axis_z: e.axis_z_color,
        sky_horizon: rgb1(e.sky_horizon_color, e.sky_gradient_height),
        sky_zenith: rgb1(e.sky_zenith_color, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_uniform_is_304_bytes() {
        // Two mat4 (128) + eleven vec4 (176); naga's span for `struct Env`
        // is pinned to the same figure in native/tests/wgsl.rs.
        assert_eq!(std::mem::size_of::<EnvUniform>(), 304);
    }

    #[test]
    fn grid_phase_survives_a_far_eye() {
        let cam = EnvCamera {
            eye: DVec3::new(12_345.678, 50.0, -98_765.432_1),
            vp_rel: Mat4::IDENTITY,
            far: 100_000.0,
            fit: 50.0,
        };
        let u = uniform(&cam, 0.0);
        let minor = crate::config::settings().environment.minor_spacing as f64;
        let want = 12_345.678f64.rem_euclid(minor) as f32;
        assert_eq!(u.grid_origin[0], want);
        assert!(u.grid_origin.iter().all(|v| *v >= 0.0));
    }
}
