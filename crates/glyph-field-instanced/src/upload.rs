//! Host records → device slots: the 48 B engine records transcoded into
//! 32 B `RenderSlot`s and uploaded in binding-sized chunks.
//!
//! Moved verbatim from `native/src/glyph_scene/buffers.rs` (Stage E/F/G);
//! only the input changed shape — a list of host slices instead of the
//! `GlyphArena` that produced them, so this crate does not depend on the
//! layout engine.

use bytemuck::Zeroable;
use glyph_field::GlyphInstance;

use crate::slot::RenderSlot;

#[derive(Clone, Copy)]
struct SendPtr(*mut RenderSlot);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    #[inline(always)]
    unsafe fn add(self, offset: usize) -> *mut RenderSlot {
        self.0.add(offset)
    }
}

#[inline(always)]
unsafe fn transcode_instance(src: *const GlyphInstance, dst: *mut RenderSlot) {
    let s = src as *const u8;
    let d = dst as *mut u8;
    std::ptr::copy_nonoverlapping(s, d, 16);
    std::ptr::copy_nonoverlapping(s.add(24), d.add(16), 16);
}

const PARALLEL_TRANSCODE_THRESHOLD: usize = 16384;
const CHUNK_SIZE: usize = 65536;

unsafe fn transcode_sub_slices(
    dest_ptr: *mut RenderSlot,
    sub_slices: &[(usize, &[GlyphInstance])],
    written: usize,
    count: usize,
) {
    let dest = SendPtr(dest_ptr);
    for &(dst_off, slice) in sub_slices {
        if slice.len() >= PARALLEL_TRANSCODE_THRESHOLD {
            use rayon::prelude::*;
            slice
                .par_chunks(CHUNK_SIZE)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let out_ptr = unsafe { dest.add(dst_off + chunk_idx * CHUNK_SIZE) };
                    for (k, g) in chunk.iter().enumerate() {
                        unsafe {
                            transcode_instance(g, out_ptr.add(k));
                        }
                    }
                });
        } else {
            let out_ptr = unsafe { dest.0.add(dst_off) };
            for (k, g) in slice.iter().enumerate() {
                unsafe {
                    transcode_instance(g, out_ptr.add(k));
                }
            }
        }
    }
    // Pad any remainder (e.g. mapped-empty arena zeroed slot):
    if count > written {
        for k in written..count {
            unsafe {
                std::ptr::write(dest_ptr.add(k), RenderSlot::zeroed());
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[inline]
fn upload_direct_metal(
    device: &wgpu::Device,
    label: &str,
    usage: wgpu::BufferUsages,
    sub_slices: &[(usize, &[GlyphInstance])],
    written: usize,
    count: usize,
) -> wgpu::Buffer {
    use wgpu::hal::Device as HalDevice;
    let hal_usage = wgpu::BufferUses::STORAGE_READ_ONLY
        | wgpu::BufferUses::COPY_DST
        | wgpu::BufferUses::COPY_SRC
        | wgpu::BufferUses::MAP_READ;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let size = (count * std::mem::size_of::<RenderSlot>()) as u64;
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size,
            usage: hal_usage,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal instance buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal instance map");
    let dest_ptr = mapping.ptr.as_ptr() as *mut RenderSlot;

    unsafe {
        transcode_sub_slices(dest_ptr, sub_slices, written, count);
        hal_dev.unmap_buffer(&hal_buf);
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: usage | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        )
    }
}

#[inline]
fn upload_staged_discrete(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    sub_slices: &[(usize, &[GlyphInstance])],
    written: usize,
    count: usize,
) -> wgpu::Buffer {
    // Discrete GPU (NVIDIA RTX 5090, AMD Radeon) or non-Mappable fallback:
    // Stream parallel transcode directly into a mapped staging buffer,
    // completely eliminating intermediate heap Vec<RenderSlot> allocations.
    let size = (count * std::mem::size_of::<RenderSlot>()) as u64;
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph instance staging"),
        size,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        let mut mapped = staging_buf
            .slice(..)
            .get_mapped_range_mut()
            .expect("staging mapped range");
        let dest_ptr = mapped.slice(..).as_raw_element_ptr().as_ptr() as *mut RenderSlot;
        unsafe {
            transcode_sub_slices(dest_ptr, sub_slices, written, count);
        }
    }
    staging_buf.unmap();
    let vram_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("glyph_instance_staging_copy"),
    });
    encoder.copy_buffer_to_buffer(&staging_buf, 0, &vram_buf, 0, size);
    queue.submit([encoder.finish()]);
    vram_buf
}

/// The uploaded chunks of a host-sourced field.
pub struct HostUpload {
    /// Slots per chunk.
    pub chunk_capacity: usize,
    /// Live slots per chunk (an empty field still binds one zeroed slot).
    pub chunk_counts: Vec<u32>,
    /// One storage buffer per chunk, slot 0 at byte 0.
    pub buffers: Vec<wgpu::Buffer>,
}

/// Chunk the host records so no bound RANGE exceeds the binding limit, then
/// transcode and upload each chunk.
///
/// `direct_host_upload`: unified-memory upload — with MAPPABLE_PRIMARY_BUFFERS
/// on Metal the storage buffer is created mapped and written straight — wgpu's
/// default path instead zero-fills a full-size staging buffer AND then
/// memcpy's into it AND blits on the GPU timeline (measured ~2.4 s of the
/// glyph3d-js repo load; the zero-fill alone was ~0.7 s). Metal only:
/// discrete adapters advertise the feature too, but a host-visible storage
/// buffer pays the saved time back in per-frame shader reads across the bus.
/// The caller decides (it knows the adapter profile).
pub fn upload_host_slots(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    slices: &[&[GlyphInstance]],
    glyph_count: usize,
    direct_host_upload: bool,
) -> HostUpload {
    let binding_limit = device.limits().max_storage_buffer_binding_size as usize;
    let chunk_cap = (binding_limit / std::mem::size_of::<RenderSlot>()).max(1);
    let mut chunk_counts: Vec<u32> = (0..glyph_count.div_ceil(chunk_cap).max(1))
        .map(|k| (glyph_count.saturating_sub(k * chunk_cap)).min(chunk_cap) as u32)
        .collect();
    // The mapped-empty arena binds one zeroed slot (nothing draws, but
    // the safety-net segment reads slot 0).
    for c in &mut chunk_counts {
        if *c == 0 {
            *c = 1;
        }
    }
    // The transcode walk: render chunks (RenderSlot stride) intersect
    // the arena's own chunks (host Vec or mapped slices) — the two
    // chunkings do not in general coincide, and a straddling range must
    // transcode bit-identically to a contiguous one (same values, field
    // order fixed by From<&GlyphInstance>).
    let arena_chunks = slices;
    let mut ac = 0usize;
    let mut arena_base = 0usize;
    let buffers: Vec<wgpu::Buffer> = chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let label = if chunk_counts.len() == 1 {
                "glyph instances".to_string()
            } else {
                format!("glyph instances {i}/{}", chunk_counts.len())
            };
            // Stage G: COPY_DST for partial per-slot edit uploads;
            // COPY_SRC for the GLYPH_G_DUMP verification readback.
            let usage = wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC;
            let first = i * chunk_cap;
            let need_end = first + count as usize;
            // Gather the arena slice sub-ranges covering this chunk:
            let mut sub_slices: Vec<(usize, &[GlyphInstance])> = Vec::new();
            let mut written = 0usize;
            while written < count as usize && ac < arena_chunks.len() {
                let c = arena_chunks[ac];
                if arena_base + c.len() <= first {
                    arena_base += c.len();
                    ac += 1;
                    continue;
                }
                let lo = first.saturating_sub(arena_base);
                let hi = (need_end - arena_base).min(c.len());
                if hi > lo {
                    sub_slices.push((written, &c[lo..hi]));
                    written += hi - lo;
                }
                if hi == c.len() {
                    arena_base += c.len();
                    ac += 1;
                }
            }

            if direct_host_upload {
                #[cfg(target_os = "macos")]
                {
                    upload_direct_metal(device, &label, usage, &sub_slices, written, count as usize)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    upload_staged_discrete(device, queue, &label, usage, &sub_slices, written, count as usize)
                }
            } else {
                upload_staged_discrete(device, queue, &label, usage, &sub_slices, written, count as usize)
            }
        })
        .collect();
    HostUpload { chunk_capacity: chunk_cap, chunk_counts, buffers }
}
