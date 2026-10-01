//! Stage E/F/G — instance buffers and GPU allocation machinery.
//! Extracted from `glyph_scene.rs` to modularize buffer upload and transcoding.

use bytemuck::Zeroable;
use crate::gpu::GpuContext;
use crate::glyph_scene::instance::{GlyphInstance, RenderSlot};
use crate::layout::GlyphArena;
use wgpu::util::DeviceExt;

/// Chunk the arena so no bound RANGE exceeds the binding limit.
/// Returns `(chunk_cap, chunk_counts, instance_bufs, chunk_offsets)`.
pub(super) fn build_instance_buffers(
    ctx: &GpuContext,
    arena: &GlyphArena,
    instances_len: usize,
) -> (usize, Vec<u32>, Vec<wgpu::Buffer>, Vec<u64>) {
    let device = &ctx.device;
    let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;

    match arena.device_slots() {
        Some(dev) => (
            dev.chunk_slots,
            dev.chunks.iter().map(|c| c.slots).collect(),
            dev.chunks.iter().map(|c| c.buffer.clone()).collect(),
            dev.chunks.iter().map(|c| c.offset).collect(),
        ),
        None => {
            let chunk_cap = (binding_limit / std::mem::size_of::<RenderSlot>()).max(1);
            let mut chunk_counts: Vec<u32> = (0..instances_len.div_ceil(chunk_cap).max(1))
                .map(|k| (instances_len.saturating_sub(k * chunk_cap)).min(chunk_cap) as u32)
                .collect();
            // The mapped-empty arena binds one zeroed slot (nothing draws, but
            // the safety-net segment reads slot 0).
            for c in &mut chunk_counts {
                if *c == 0 {
                    *c = 1;
                }
            }
            // Unified-memory upload: with MAPPABLE_PRIMARY_BUFFERS on Metal the
            // storage buffer is created mapped and written straight — wgpu's
            // default path instead zero-fills a full-size staging buffer AND then
            // memcpy's into it AND blits on the GPU timeline (measured ~2.4 s of
            // the glyph3d-js repo load; the zero-fill alone was ~0.7 s). Metal
            // only: discrete adapters advertise the feature too, but a
            // host-visible storage buffer pays the saved time back in per-frame
            // shader reads across the bus.
            let direct_upload = ctx.profile.backend == wgpu::Backend::Metal
                && ctx.profile.mappable_primary_buffers;
            // The transcode walk: render chunks (RenderSlot stride) intersect
            // the arena's own chunks (host Vec or mapped slices) — the two
            // chunkings do not in general coincide, and a straddling range must
            // transcode bit-identically to a contiguous one (same values, field
            // order fixed by From<&GlyphInstance>).
            let arena_chunks = arena.instance_chunks();
            let mut ac = 0usize;
            let mut arena_base = 0usize;
            let instance_bufs: Vec<wgpu::Buffer> = chunk_counts
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

                    if direct_upload {
                        use wgpu::hal::Device as HalDevice;
                        let hal_usage = wgpu::BufferUses::STORAGE_READ_ONLY
                            | wgpu::BufferUses::COPY_DST
                            | wgpu::BufferUses::COPY_SRC
                            | wgpu::BufferUses::MAP_READ;
                        let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
                            .expect("Metal profile behind a non-Metal device");
                        let size = (count as usize * std::mem::size_of::<RenderSlot>()) as u64;
                        let hal_buf = unsafe {
                            hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
                                label: Some(&label),
                                size,
                                usage: hal_usage,
                                memory_flags: wgpu::hal::MemoryFlags::empty(),
                            })
                        }
                        .expect("hal instance buffer");
                        let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }
                            .expect("hal instance map");
                        let dest_ptr = mapping.ptr.as_ptr() as *mut RenderSlot;

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

                        let dest = SendPtr(dest_ptr);
                        const PARALLEL_TRANSCODE_THRESHOLD: usize = 16384;
                        const CHUNK_SIZE: usize = 65536;

                        for &(dst_off, slice) in &sub_slices {
                            if slice.len() >= PARALLEL_TRANSCODE_THRESHOLD {
                                use rayon::prelude::*;
                                slice.par_chunks(CHUNK_SIZE).enumerate().for_each(|(chunk_idx, chunk)| {
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
                        if count as usize > written {
                            for k in written..count as usize {
                                unsafe {
                                    std::ptr::write(dest_ptr.add(k), RenderSlot::zeroed());
                                }
                            }
                        }

                        unsafe { hal_dev.unmap_buffer(&hal_buf) };
                        // SAFETY: same device, desc matches the hal request, every
                        // byte just written, nonzero size.
                        unsafe {
                            device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
                                hal_buf,
                                &wgpu::BufferDescriptor {
                                    label: Some(&label),
                                    size,
                                    usage: usage | wgpu::BufferUsages::MAP_READ,
                                    mapped_at_creation: false,
                                },
                            )
                        }
                    } else {
                        let mut slots: Vec<RenderSlot> = Vec::with_capacity(count as usize);
                        for &(_dst_off, slice) in &sub_slices {
                            slots.extend(slice.iter().map(RenderSlot::from));
                        }
                        debug_assert!(instances_len == 0 || slots.len() == count as usize);
                        slots.resize(count as usize, RenderSlot::zeroed());
                        let bytes: &[u8] = bytemuck::cast_slice(&slots);
                        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some(&label),
                            contents: bytes,
                            usage,
                        })
                    }
                })
                .collect();
            let zero_offsets = vec![0u64; instance_bufs.len()];
            (chunk_cap, chunk_counts, instance_bufs, zero_offsets)
        }
    }
}
