//! Tail readback utilities: tint stream extraction, mapped shared buffers, and extent placements decoding.

use cubecl::wgpu::{AutoCompiler, WgpuServer};
use crate::gpu::SharedDevice;
use crate::layout::{InkExtent, ItemPlacement, PageExtent, TintMapped, TintStore};
use super::super::tail::{key_to_float_host, EXT_STRIDE};

#[inline]
pub(crate) fn read_tint_store_host(
    client: &cubecl::client::Client,
    h_tint: cubecl::server::Handle,
    total_slots: u32,
) -> TintStore {
    let tb = client.read_one(h_tint).expect("read tint stream");
    TintStore::Host(
        bytemuck::cast_slice::<u8, u32>(&tb)[..total_slots as usize * 2].to_vec(),
    )
}

#[inline]
pub(crate) fn read_tint_store_unified(
    client: &cubecl::client::Client,
    device: &SharedDevice,
    h_tint: cubecl::server::Handle,
    total_slots: u32,
) -> TintStore {
    let res = client
        .get_resource::<WgpuServer<AutoCompiler>>(h_tint)
        .expect("tint stream resource");
    let (src, src_off) = {
        let r = res.resource();
        (r.buffer.clone(), r.offset)
    };
    let bytes = (total_slots as usize * 8) as u64;
    let (buf, ptr) = mapped_read_buffer(&device.device, bytes.max(4), "tint stream");
    let mut enc = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("tint stream copy"),
        });
    enc.copy_buffer_to_buffer(&src, src_off, &buf, 0, bytes.max(4));
    device.queue.submit([enc.finish()]);
    device
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("tint copy poll");
    // `res` drops here — the cubecl tint slice returns to the pool.
    // Queue order already moved the bytes; nothing pending reads it.
    TintStore::Mapped(TintMapped {
        buffer: buf,
        ptr,
        words: total_slots as usize * 2,
    })
}

/// A hal-mapped shared-storage readback target (Metal hosts only — the
/// caller gates on the profile). MAP_READ makes wgpu-hal pick
/// StorageModeShared with the DEFAULT cache mode (write-combining is
/// MAP_WRITE-only), so host reads of the GPU-written bytes stay cached.
/// The mapping lives as long as the buffer (Metal's unmap is a no-op) —
/// the same lifecycle as the mapped arena's chunks.
pub(crate) fn mapped_read_buffer(device: &wgpu::Device, bytes: u64, label: &str) -> (wgpu::Buffer, *const u32) {
    use wgpu::hal::Device as HalDevice;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("host-visible storage behind a non-Metal device");
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size: bytes,
            usage: wgpu::BufferUses::MAP_READ | wgpu::BufferUses::COPY_DST,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal readback buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..bytes) }.expect("hal readback map");
    let ptr = mapping.ptr.as_ptr() as *const u32;
    // SAFETY: same device, desc matches the hal request, nonzero size, and
    // every byte is written by the caller's copy before the poll publishes.
    let buf = unsafe {
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        )
    };
    (buf, ptr)
}

/// Decodes item placements from GPU extent lanes in key space.
pub(crate) fn decode_placements(
    client: &cubecl::client::Client,
    h_ext: cubecl::server::Handle,
    item_count: usize,
    slot_base: &[u32],
    stot: &[u32],
    ltot: &[u32],
) -> Vec<ItemPlacement> {
    let _sp_placements = tracing::info_span!("tail.placements").entered();
    let tb_e = client.read_one(h_ext).expect("extent lanes");
    let ev: &[u32] = bytemuck::cast_slice(&tb_e);
    let mut placements = Vec::with_capacity(item_count);
    for it in 0..item_count {
        let e = it * EXT_STRIDE;
        placements.push(ItemPlacement {
            slot_base: slot_base[it],
            slot_count: stot[it],
            record_count: ltot[it],
            page: PageExtent {
                right: key_to_float_host(ev[e]),
                bottom: key_to_float_host(ev[e + 1]),
                z_min: key_to_float_host(ev[e + 2]),
                z_max: key_to_float_host(ev[e + 3]),
            },
            ink: InkExtent {
                min: [
                    key_to_float_host(ev[e + 4]),
                    key_to_float_host(ev[e + 5]),
                    key_to_float_host(ev[e + 8]),
                ],
                max: [
                    key_to_float_host(ev[e + 6]),
                    key_to_float_host(ev[e + 7]),
                    key_to_float_host(ev[e + 9]),
                ],
            },
        });
    }
    placements
}
