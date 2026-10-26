//! The viewport's stand-in until the makepad draw passes land
//! (`g3_passes.rs`): the engine renders its frame with its own wgpu device
//! into a texture of the viewport's size, the frame is copied back without
//! the UI thread ever waiting on it (one readback in flight; a frame lands
//! when its map completes, a frame or two late), and the viewport shows it
//! as a makepad texture. It exists so the host, its input and its panels
//! run against real pixels during bring-up; it goes when the passes draw
//! the scene, and nothing else depends on it.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use glyph3d_native::scene::{create_depth, FrameTarget};

use crate::host::{EngineHost, COLOR_FORMAT};

/// One BGRA frame, rows tight, ready for `Texture::set_data_u32`.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

struct Target {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    depth: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Bytes per readback row: the width's, padded to wgpu's copy alignment.
    padded_row: u32,
}

impl Target {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("makepad stand-in target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            // COPY_DST: the composite's copy path, when the pool's format
            // matches; COPY_SRC: the readback.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let depth = create_depth(device, wgpu::TextureFormat::Depth32Float, width, height);
        let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("makepad stand-in readback"),
            size: u64::from(padded_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { width, height, texture, view, depth, readback, padded_row }
    }
}

/// A readback's state, as its map callback leaves it.
const PENDING: u8 = 0;
const MAPPED: u8 = 1;
const FAILED: u8 = 2;

#[derive(Default)]
pub struct StandIn {
    target: Option<Target>,
    /// The readback in flight: PENDING until its map callback says MAPPED
    /// (read it) or FAILED (drop the frame and render the next).
    mapped: Option<Arc<AtomicU8>>,
    start: Option<std::time::Instant>,
}

impl StandIn {
    /// One step: hand over the frame whose readback completed, if one did,
    /// and start the next render when no readback is in flight. Never
    /// blocks; the host's motion (`EngineHost::advance`) runs before it.
    pub fn step(&mut self, host: &mut EngineHost) -> Option<Frame> {
        let device = &host.ctx.device;
        let _ = device.poll(wgpu::PollType::Poll);
        let mut landed = None;
        if let Some(flag) = &self.mapped {
            let state = flag.load(Ordering::Acquire);
            if state == PENDING {
                return None;
            }
            let t = self.target.as_ref().expect("a readback in flight has its target");
            if state == MAPPED {
                let bytes = t.readback.slice(..).get_mapped_range().expect("a mapped readback reads");
                let mut pixels = Vec::with_capacity((t.width * t.height) as usize);
                for row in bytes.chunks_exact(t.padded_row as usize) {
                    let row = &row[..(t.width * 4) as usize];
                    pixels.extend(row.chunks_exact(4).map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]])));
                }
                landed = Some(Frame { width: t.width as usize, height: t.height as usize, pixels });
                drop(bytes);
                t.readback.unmap();
            }
            self.mapped = None;
        }
        let (width, height) = host.viewport();
        if self.target.as_ref().is_none_or(|t| (t.width, t.height) != (width, height)) {
            self.target = Some(Target::new(device, width, height));
        }
        let t = self.target.as_ref().expect("set above");
        let time = self.start.get_or_insert_with(std::time::Instant::now).elapsed().as_secs_f32();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("makepad stand-in frame") });
        host.scene.render(
            &host.ctx,
            &mut encoder,
            &FrameTarget {
                color_view: &t.view,
                color_texture: &t.texture,
                color_format: COLOR_FORMAT,
                depth_view: &t.depth,
                width,
                height,
            },
            time,
        );
        encoder.copy_texture_to_buffer(
            t.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &t.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(t.padded_row), rows_per_image: Some(height) },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        host.ctx.queue.submit([encoder.finish()]);
        let flag = Arc::new(AtomicU8::new(PENDING));
        let set = flag.clone();
        t.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            set.store(if r.is_ok() { MAPPED } else { FAILED }, Ordering::Release);
        });
        self.mapped = Some(flag);
        landed
    }
}
