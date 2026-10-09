//! Device buffer allocation and staging for unified and discrete memory architectures.
//!
//! Generic over the emitted slot format ([`SlotEmit`]): the buffer is sized
//! `slots × size_of::<E::Slot>()` and Pass 2 writes straight into it — mapped
//! device memory on unified Metal, a mapped staging buffer + one copy
//! elsewhere. Neither path ever materializes the 48 B host record.

use crate::atlas::TrieTable;
use crate::gpu::SharedDevice;
use crate::layout::{DeviceSlotChunk, LayoutItem};
use super::pass2_device::{emoji_tint_pairs, layout_pass2_device, SlotEmit};
use super::types::{ItemPrepass, Pass2DeviceOutput};

/// What a device emission hands back: the chunked slot buffers, uniform chunk
/// capacity, host mapping (if unified memory), Pass 2's per-item outputs, and
/// the emoji tint pairs.
pub(crate) struct DeviceEmission {
    pub chunks: Vec<DeviceSlotChunk>,
    pub chunk_slots: usize,
    pub mapped_base: Option<usize>,
    pub pass2: Pass2DeviceOutput,
    pub emoji_tint_pairs: Vec<Vec<u32>>,
}

/// The per-run inputs every emission shares.
pub(crate) struct EmitInputs<'a, 'b> {
    pub items: &'a [LayoutItem<'b>],
    pub chunks: &'a [super::chunk::LayoutChunk<'b>],
    pub item_chunk_ranges: &'a [std::ops::Range<usize>],
    pub prepasses: &'a [ItemPrepass],
    pub slot_bases: &'a [u32],
    pub chunk_slot_bases: &'a [u32],
    pub chunk_base_rows: &'a [i64],
    pub chunk_record_bases: &'a [usize],
    pub chunk_initial_cols: &'a [i64],
    pub chunk_initial_seg_advs: &'a [f32],
    pub chunk_initial_line_advs: &'a [f64],
    pub trie: &'a TrieTable,
    pub bitmap_adv: f32,
    pub em_height_fu: u32,
}

impl EmitInputs<'_, '_> {
    /// Pass 2 into `dest_addr` (any writable memory sized for the total
    /// survivors of `E::Slot` — a mapped GPU buffer, or a host Vec in tests).
    pub(crate) fn run<E: SlotEmit>(&self, dest_addr: usize) -> (Pass2DeviceOutput, Vec<Vec<u32>>) {
        let sp_pass2 = tracing::info_span!("hyper.pass2").entered();
        let out = layout_pass2_device::<E>(self, dest_addr);
        drop(sp_pass2);
        let pairs = emoji_tint_pairs::<E>(dest_addr, &out);
        (out, pairs)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn create_mapped_slot_buffer(
    device: &wgpu::Device,
    size: u64,
    label: &str,
) -> (*mut u8, wgpu::Buffer) {
    use wgpu::hal::Device as HalDevice;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUses::STORAGE_READ_ONLY
                | wgpu::BufferUses::COPY_DST
                | wgpu::BufferUses::COPY_SRC
                | wgpu::BufferUses::MAP_READ,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal arena buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal arena map");
    let ptr = mapping.ptr.as_ptr();
    let buf = unsafe {
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        )
    };
    (ptr, buf)
}

pub(crate) struct ChunkPlan {
    pub slot_bytes: usize,
    pub chunk_cap: usize,
    pub chunk_counts: Vec<u32>,
    pub total_bytes: u64,
}

impl ChunkPlan {
    pub fn new<E: SlotEmit>(dev: &SharedDevice, total_survivors: usize, explicit_chunk_cap: Option<usize>) -> Self {
        let slot_bytes = std::mem::size_of::<E::Slot>();
        let chunk_cap = explicit_chunk_cap.unwrap_or_else(|| {
            let binding_limit = dev.device.limits().max_storage_buffer_binding_size as usize;
            (binding_limit / slot_bytes).max(1)
        });

        let num_chunks = total_survivors.div_ceil(chunk_cap).max(1);
        let mut chunk_counts: Vec<u32> = (0..num_chunks)
            .map(|k| (total_survivors.saturating_sub(k * chunk_cap)).min(chunk_cap) as u32)
            .collect();
        for c in &mut chunk_counts {
            if *c == 0 {
                *c = 1;
            }
        }
        let total_bytes = (total_survivors * slot_bytes) as u64;

        Self {
            slot_bytes,
            chunk_cap,
            chunk_counts,
            total_bytes,
        }
    }
}

fn create_chunk_buffer(
    dev: &SharedDevice,
    size: u64,
    label: &str,
) -> wgpu::Buffer {
    dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

#[inline]
pub(crate) fn layout_device_unified<E: SlotEmit>(
    dev: &SharedDevice,
    total_survivors: usize,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
) -> DeviceEmission {
    #[cfg(target_os = "macos")]
    {
        let plan = ChunkPlan::new::<E>(dev, total_survivors, None);

        if plan.chunk_counts.len() == 1 {
            let count = total_survivors.max(1);
            let size = (count * plan.slot_bytes) as u64;
            let (mapped_ptr, buffer) = create_mapped_slot_buffer(&dev.device, size, label);
            let addr = mapped_ptr as usize;
            let (pass2, emoji_tint_pairs) = inputs.run::<E>(addr);
            DeviceEmission {
                chunks: vec![DeviceSlotChunk {
                    buffer,
                    offset: 0,
                    slots: count as u32,
                }],
                chunk_slots: plan.chunk_cap,
                mapped_base: Some(addr),
                pass2,
                emoji_tint_pairs,
            }
        } else {
            layout_device_discrete_chunked::<E>(dev, total_survivors, inputs, label, plan.chunk_cap)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        layout_device_discrete::<E>(dev, total_survivors, inputs, label)
    }
}

#[inline]
pub(crate) fn layout_device_discrete<E: SlotEmit>(
    dev: &SharedDevice,
    total_survivors: usize,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
) -> DeviceEmission {
    let plan = ChunkPlan::new::<E>(dev, total_survivors, None);
    layout_device_discrete_chunked::<E>(dev, total_survivors, inputs, label, plan.chunk_cap)
}

pub(crate) fn layout_device_discrete_chunked<E: SlotEmit>(
    dev: &SharedDevice,
    total_survivors: usize,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
    chunk_cap: usize,
) -> DeviceEmission {
    let plan = ChunkPlan::new::<E>(dev, total_survivors, Some(chunk_cap));
    let max_buf = dev.device.limits().max_buffer_size;

    let (pass2, emoji_tint_pairs, chunks) = if plan.total_bytes <= max_buf {
        stage_single_buffer::<E>(dev, inputs, label, &plan)
    } else {
        stage_host_memory::<E>(dev, inputs, label, &plan)
    };

    DeviceEmission {
        chunks,
        chunk_slots: plan.chunk_cap,
        mapped_base: None,
        pass2,
        emoji_tint_pairs,
    }
}

fn stage_single_buffer<E: SlotEmit>(
    dev: &SharedDevice,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
    plan: &ChunkPlan,
) -> (Pass2DeviceOutput, Vec<Vec<u32>>, Vec<DeviceSlotChunk>) {
    let staging_size = plan.total_bytes.max(plan.slot_bytes as u64);
    let staging_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph slots (staging)"),
        size: staging_size,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    let (pass2, emoji_tint_pairs) = {
        let mut mapped = staging_buf
            .slice(..)
            .get_mapped_range_mut()
            .expect("staging mapped range");
        let addr = mapped.slice(..).as_raw_element_ptr().as_ptr() as usize;
        inputs.run::<E>(addr)
    };
    staging_buf.unmap();

    let mut encoder = dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("glyph_hyper_staging_copy"),
    });

    let chunks: Vec<DeviceSlotChunk> = plan.chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let chunk_size = (count as usize * plan.slot_bytes) as u64;
            let chunk_label = if plan.chunk_counts.len() == 1 {
                label.to_string()
            } else {
                format!("{label} {i}/{}", plan.chunk_counts.len())
            };
            
            let buffer = create_chunk_buffer(dev, chunk_size, &chunk_label);
            
            let src_offset = (i * plan.chunk_cap * plan.slot_bytes) as u64;
            let copy_size = (count as usize * plan.slot_bytes)
                .min(plan.total_bytes.saturating_sub(src_offset) as usize) as u64;
            if copy_size > 0 {
                encoder.copy_buffer_to_buffer(&staging_buf, src_offset, &buffer, 0, copy_size);
            }
            DeviceSlotChunk {
                buffer,
                offset: 0,
                slots: count,
            }
        })
        .collect();

    dev.queue.submit([encoder.finish()]);
    (pass2, emoji_tint_pairs, chunks)
}

fn stage_host_memory<E: SlotEmit>(
    dev: &SharedDevice,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
    plan: &ChunkPlan,
) -> (Pass2DeviceOutput, Vec<Vec<u32>>, Vec<DeviceSlotChunk>) {
    let alloc_size = (plan.total_bytes as usize).max(plan.slot_bytes);
    let layout = std::alloc::Layout::from_size_align(
        alloc_size,
        std::mem::align_of::<E::Slot>().max(64),
    )
    .expect("valid host staging layout");
    let host_ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!host_ptr.is_null(), "failed to allocate host staging memory");

    let (pass2, emoji_tint_pairs) = inputs.run::<E>(host_ptr as usize);

    let chunks: Vec<DeviceSlotChunk> = plan.chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let chunk_size = (count as usize * plan.slot_bytes) as u64;
            let chunk_label = format!("{label} {i}/{}", plan.chunk_counts.len());
            let src_offset = i * plan.chunk_cap * plan.slot_bytes;
            let copy_bytes = (count as usize * plan.slot_bytes)
                .min((plan.total_bytes as usize).saturating_sub(src_offset));

            let staging_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glyph slots chunk (staging)"),
                size: chunk_size,
                usage: wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            });
            if copy_bytes > 0 {
                let mut mapped = staging_buf
                    .slice(..)
                    .get_mapped_range_mut()
                    .expect("chunk staging mapped range");
                let dest_ptr = mapped.slice(..).as_raw_element_ptr().as_ptr();
                let dst = unsafe { std::slice::from_raw_parts_mut(dest_ptr, copy_bytes) };
                let src = unsafe { std::slice::from_raw_parts(host_ptr.add(src_offset), copy_bytes) };
                dst.copy_from_slice(src);
            }
            staging_buf.unmap();

            let buffer = create_chunk_buffer(dev, chunk_size, &chunk_label);

            let mut encoder = dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glyph_hyper_chunk_staging_copy"),
            });
            encoder.copy_buffer_to_buffer(&staging_buf, 0, &buffer, 0, chunk_size);
            dev.queue.submit([encoder.finish()]);

            DeviceSlotChunk {
                buffer,
                offset: 0,
                slots: count,
            }
        })
        .collect();

    unsafe {
        std::alloc::dealloc(host_ptr, layout);
    }

    (pass2, emoji_tint_pairs, chunks)
}
