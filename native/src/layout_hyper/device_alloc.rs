//! Device buffer allocation and staging for unified and discrete memory architectures.

use crate::atlas::TrieTable;
use crate::glyph_scene::RenderSlot;
use crate::gpu::SharedDevice;
use crate::layout::LayoutItem;
use super::pass2_device::layout_pass2_device;
use super::types::{ItemPrepass, Pass2DeviceOutput, SendPtr};

#[cfg(target_os = "macos")]
pub(crate) fn create_mapped_render_slots(
    device: &wgpu::Device,
    slots: usize,
) -> (*mut RenderSlot, wgpu::Buffer) {
    use wgpu::hal::Device as HalDevice;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let size = (slots * std::mem::size_of::<RenderSlot>()) as u64;
    let label = "glyph render slots (direct-mapped)";
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
    let ptr = mapping.ptr.as_ptr() as *mut RenderSlot;
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

#[cfg(not(target_os = "macos"))]
pub(crate) fn create_mapped_render_slots(
    _device: &wgpu::Device,
    _slots: usize,
) -> (*mut RenderSlot, wgpu::Buffer) {
    unreachable!("Metal mapped primary buffers are only available on macOS");
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_device_unified(
    dev: &SharedDevice,
    total_survivors: usize,
    items: &[LayoutItem<'_>],
    prepasses: &[ItemPrepass],
    slot_bases: &[u32],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> (wgpu::Buffer, Option<usize>, Pass2DeviceOutput) {
    #[cfg(target_os = "macos")]
    {
        let (mapped_ptr, buf) = create_mapped_render_slots(&dev.device, total_survivors);
        let sp_pass2 = tracing::info_span!("hyper.pass2").entered();
        let pass2_out = layout_pass2_device(
            items,
            prepasses,
            slot_bases,
            trie,
            bitmap_adv,
            em_height_fu,
            SendPtr(mapped_ptr),
        );
        drop(sp_pass2);
        (buf, Some(mapped_ptr as usize), pass2_out)
    }
    #[cfg(not(target_os = "macos"))]
    {
        layout_device_discrete(
            dev,
            total_survivors,
            items,
            prepasses,
            slot_bases,
            trie,
            bitmap_adv,
            em_height_fu,
        )
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_device_discrete(
    dev: &SharedDevice,
    total_survivors: usize,
    items: &[LayoutItem<'_>],
    prepasses: &[ItemPrepass],
    slot_bases: &[u32],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> (wgpu::Buffer, Option<usize>, Pass2DeviceOutput) {
    let size = (total_survivors * std::mem::size_of::<RenderSlot>()) as u64;
    let staging_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph render slots (staging)"),
        size,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    let pass2_out = {
        let mut mapped = staging_buf
            .slice(..)
            .get_mapped_range_mut()
            .expect("staging mapped range");
        let mapped_ptr =
            mapped.slice(..).as_raw_element_ptr().as_ptr() as *mut RenderSlot;
        let sp_pass2 = tracing::info_span!("hyper.pass2").entered();
        let out = layout_pass2_device(
            items,
            prepasses,
            slot_bases,
            trie,
            bitmap_adv,
            em_height_fu,
            SendPtr(mapped_ptr),
        );
        drop(sp_pass2);
        out
    };
    staging_buf.unmap();
    let vram_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph render slots (vram)"),
        size,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let mut encoder = dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("glyph_hyper_staging_copy"),
    });
    encoder.copy_buffer_to_buffer(&staging_buf, 0, &vram_buf, 0, size);
    dev.queue.submit([encoder.finish()]);
    (vram_buf, None, pass2_out)
}
