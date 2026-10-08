//! Device buffer allocation and staging for unified and discrete memory architectures.
//!
//! Generic over the emitted slot format ([`SlotEmit`]): the buffer is sized
//! `slots × size_of::<E::Slot>()` and Pass 2 writes straight into it — mapped
//! device memory on unified Metal, a mapped staging buffer + one copy
//! elsewhere. Neither path ever materializes the 48 B host record.

use crate::atlas::TrieTable;
use crate::gpu::SharedDevice;
use crate::layout::LayoutItem;
use super::pass2_device::{emoji_tint_pairs, layout_pass2_device, SlotEmit};
use super::types::{ItemPrepass, Pass2DeviceOutput};

/// What a device emission hands back: the bound buffer, its host mapping (if
/// it stays mapped), Pass 2's per-item outputs, and the emoji tint pairs.
pub(crate) struct DeviceEmission {
    pub buffer: wgpu::Buffer,
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
    pub line_bases: &'a [u32],
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

#[inline]
pub(crate) fn layout_device_unified<E: SlotEmit>(
    dev: &SharedDevice,
    total_survivors: usize,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
) -> DeviceEmission {
    #[cfg(target_os = "macos")]
    {
        let size = (total_survivors * std::mem::size_of::<E::Slot>()) as u64;
        let (mapped_ptr, buffer) = create_mapped_slot_buffer(&dev.device, size, label);
        let addr = mapped_ptr as usize;
        let (pass2, emoji_tint_pairs) = inputs.run::<E>(addr);
        DeviceEmission { buffer, mapped_base: Some(addr), pass2, emoji_tint_pairs }
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
    let size = (total_survivors * std::mem::size_of::<E::Slot>()) as u64;
    let staging_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph slots (staging)"),
        size,
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
    let buffer = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mut encoder = dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("glyph_hyper_staging_copy"),
    });
    encoder.copy_buffer_to_buffer(&staging_buf, 0, &buffer, 0, size);
    dev.queue.submit([encoder.finish()]);
    DeviceEmission { buffer, mapped_base: None, pass2, emoji_tint_pairs }
}
