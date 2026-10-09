//! Host records → device slots: 48 B GlyphInstance transcoded into 20 B DerivedSlot.

use bytemuck::Zeroable;
use glyph_field::{GlyphInstance, ItemParamsGpu};

use crate::slot::DerivedSlot;

#[derive(Clone, Copy)]
struct SendPtr(*mut DerivedSlot);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    #[inline(always)]
    unsafe fn add(self, offset: usize) -> *mut DerivedSlot {
        self.0.add(offset)
    }
}

pub struct HostUploadDerived {
    pub chunk_capacity: usize,
    pub chunk_counts: Vec<u32>,
    pub buffers: Vec<wgpu::Buffer>,
}

const CHUNK_SIZE: usize = 65536;

#[inline(always)]
fn transcode_one(inst: &GlyphInstance, item_params: &[ItemParamsGpu]) -> DerivedSlot {
    let wrap_segment = if inst.flags != 0 {
        (inst.flags & 0xFFFF) as u16
    } else if let Some(item) = item_params.get(inst.group_id as usize) {
        if item.z_step.abs() > 1e-6 {
            ((item.origin_z - inst.pos[2]) / item.z_step).round().max(0.0) as u16
        } else {
            0
        }
    } else {
        0
    };

    let glyph_id = (inst.glyph_id & 0xFFFF) as u16;
    let row = inst.row;
    let item_and_group = inst.group_id;

    DerivedSlot::with_item_and_group(
        inst.pos[0],
        row,
        glyph_id,
        wrap_segment,
        inst.color,
        item_and_group,
    )
}

unsafe fn transcode_derived_sub_slices(
    dest_ptr: *mut DerivedSlot,
    sub_slices: &[(usize, &[GlyphInstance])],
    written: usize,
    count: usize,
    item_params: &[ItemParamsGpu],
) {
    let dest = SendPtr(dest_ptr);
    for &(dst_off, slice) in sub_slices {
        if slice.len() >= 16384 {
            use rayon::prelude::*;
            slice
                .par_chunks(CHUNK_SIZE)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let out_ptr = unsafe { dest.add(dst_off + chunk_idx * CHUNK_SIZE) };
                    for (k, g) in chunk.iter().enumerate() {
                        unsafe {
                            std::ptr::write(out_ptr.add(k), transcode_one(g, item_params));
                        }
                    }
                });
        } else {
            let out_ptr = unsafe { dest.0.add(dst_off) };
            for (k, g) in slice.iter().enumerate() {
                unsafe {
                    std::ptr::write(out_ptr.add(k), transcode_one(g, item_params));
                }
            }
        }
    }
    if count > written {
        for k in written..count {
            unsafe {
                std::ptr::write(dest_ptr.add(k), DerivedSlot::zeroed());
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
    item_params: &[ItemParamsGpu],
) -> wgpu::Buffer {
    use wgpu::hal::Device as HalDevice;
    let hal_usage = wgpu::BufferUses::STORAGE_READ_ONLY
        | wgpu::BufferUses::COPY_DST
        | wgpu::BufferUses::COPY_SRC
        | wgpu::BufferUses::MAP_READ;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let size = (count * std::mem::size_of::<DerivedSlot>()) as u64;
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size,
            usage: hal_usage,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal derived buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal derived map");
    let dest_ptr = mapping.ptr.as_ptr() as *mut DerivedSlot;

    unsafe {
        transcode_derived_sub_slices(dest_ptr, sub_slices, written, count, item_params);
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
#[allow(clippy::too_many_arguments)]
fn upload_staged_discrete(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    sub_slices: &[(usize, &[GlyphInstance])],
    written: usize,
    count: usize,
    item_params: &[ItemParamsGpu],
) -> wgpu::Buffer {
    let size = (count * std::mem::size_of::<DerivedSlot>()) as u64;
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("derived glyph instance staging"),
        size,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        let mut mapped = staging_buf
            .slice(..)
            .get_mapped_range_mut()
            .expect("staging mapped range");
        let dest_ptr = mapped.slice(..).as_raw_element_ptr().as_ptr() as *mut DerivedSlot;
        unsafe {
            transcode_derived_sub_slices(dest_ptr, sub_slices, written, count, item_params);
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
        label: Some("derived_instance_staging_copy"),
    });
    encoder.copy_buffer_to_buffer(&staging_buf, 0, &vram_buf, 0, size);
    queue.submit([encoder.finish()]);
    vram_buf
}

pub fn upload_derived_slots(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    slices: &[&[GlyphInstance]],
    glyph_count: usize,
    direct_host_upload: bool,
    item_params: &[ItemParamsGpu],
) -> HostUploadDerived {
    let binding_limit = device.limits().max_storage_buffer_binding_size as usize;
    let chunk_cap = (binding_limit / std::mem::size_of::<DerivedSlot>()).max(1);
    let mut chunk_counts: Vec<u32> = (0..glyph_count.div_ceil(chunk_cap).max(1))
        .map(|k| (glyph_count.saturating_sub(k * chunk_cap)).min(chunk_cap) as u32)
        .collect();

    for c in &mut chunk_counts {
        if *c == 0 {
            *c = 1;
        }
    }

    let mut ac = 0usize;
    let mut arena_base = 0usize;
    let buffers: Vec<wgpu::Buffer> = chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let label = if chunk_counts.len() == 1 {
                "derived glyph instances".to_string()
            } else {
                format!("derived glyph instances {i}/{}", chunk_counts.len())
            };
            let usage = wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC;
            let first = i * chunk_cap;
            let need_end = first + count as usize;

            let mut sub_slices: Vec<(usize, &[GlyphInstance])> = Vec::new();
            let mut written = 0usize;
            while written < count as usize && ac < slices.len() {
                let c = slices[ac];
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
                    upload_direct_metal(device, &label, usage, &sub_slices, written, count as usize, item_params)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    upload_staged_discrete(device, queue, &label, usage, &sub_slices, written, count as usize, item_params)
                }
            } else {
                upload_staged_discrete(device, queue, &label, usage, &sub_slices, written, count as usize, item_params)
            }
        })
        .collect();

    HostUploadDerived {
        chunk_capacity: chunk_cap,
        chunk_counts,
        buffers,
    }
}
