//! Tail readback utilities: tint stream extraction, mapped shared buffers, and extent placements decoding.

use cubecl::client::Client;
use cubecl::server::Handle;
use cubecl::wgpu::{AutoCompiler, WgpuServer};
use crate::gpu::SharedDevice;
use crate::layout::{InkExtent, ItemPlacement, PageExtent, TintMapped, TintStore};
use super::super::tail::{key_to_float_host, EXT_STRIDE};

/// Fallback host-staged read of the tint stream via cubecl read_one.
#[inline]
pub(crate) fn read_tint_store_host(
    client: &Client,
    h_instance_tints: Handle,
    total_slots: u32,
) -> TintStore {
    let tint_bytes = client.read_one(h_instance_tints).expect("read tint stream");
    TintStore::Host(
        bytemuck::cast_slice::<u8, u32>(&tint_bytes)[..total_slots as usize * 2].to_vec(),
    )
}

/// Zero-copy readback on Unified Memory Architectures (Apple Silicon Metal).
/// Direct mapped storage in unified DRAM without staging buffer hops.
#[inline]
pub(crate) fn read_tint_store_unified(
    client: &Client,
    device: &SharedDevice,
    h_instance_tints: Handle,
    total_slots: u32,
) -> TintStore {
    let res = client
        .get_resource::<WgpuServer<AutoCompiler>>(h_instance_tints)
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

/// Single-copy host memory staging for Discrete GPU Architectures (NVIDIA RTX 5090, AMD Radeon).
/// Directly streams VRAM storage into a mapped host staging buffer via PCIe DMA transfer,
/// avoiding cubecl internal staging reallocations.
#[inline]
pub(crate) fn read_tint_store_discrete(
    client: &Client,
    device: &SharedDevice,
    h_instance_tints: Handle,
    total_slots: u32,
) -> TintStore {
    let res = client
        .get_resource::<WgpuServer<AutoCompiler>>(h_instance_tints)
        .expect("tint stream resource");
    let (src, src_off) = {
        let r = res.resource();
        (r.buffer.clone(), r.offset)
    };
    let bytes = (total_slots as usize * 8) as u64;
    let staging_buf = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tint stream discrete staging"),
        size: bytes.max(4),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("tint stream discrete copy"),
        });
    enc.copy_buffer_to_buffer(&src, src_off, &staging_buf, 0, bytes.max(4));
    device.queue.submit([enc.finish()]);

    let slice = staging_buf.slice(..bytes.max(4));
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        tx.send(res).expect("map_async send failed");
    });
    device
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("tint discrete copy poll");
    rx.recv()
        .expect("map_async recv failed")
        .expect("tint discrete staging map failed");

    let mapped = slice.get_mapped_range().expect("tint discrete get_mapped_range failed");
    let words: &[u32] = bytemuck::cast_slice(&mapped);
    let result = words[..total_slots as usize * 2].to_vec();
    drop(mapped);
    staging_buf.unmap();
    TintStore::Host(result)
}

/// Composable multi-architecture tint stream readback router.
/// Dispatches to unified zero-copy, discrete host staging, or fallback based on device profile.
#[inline]
pub(crate) fn read_tint_store(
    client: &Client,
    device: &SharedDevice,
    h_instance_tints: Handle,
    total_slots: u32,
    readback_host: bool,
) -> TintStore {
    if total_slots == 0 {
        return TintStore::Host(Vec::new());
    }
    if readback_host {
        return read_tint_store_host(client, h_instance_tints, total_slots);
    }
    if device.is_unified() && device.host_visible_storage {
        read_tint_store_unified(client, device, h_instance_tints, total_slots)
    } else if device.is_discrete() {
        read_tint_store_discrete(client, device, h_instance_tints, total_slots)
    } else {
        read_tint_store_host(client, h_instance_tints, total_slots)
    }
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
    client: &Client,
    device: &SharedDevice,
    h_item_extents: Handle,
    item_count: usize,
    slot_base: &[u32],
    survivor_totals: &[u32],
    leader_totals: &[u32],
) -> Vec<ItemPlacement> {
    let _sp_placements = tracing::info_span!("tail.placements").entered();
    let _ = client.flush();
    let extent_bytes_opt = if device.is_unified() && device.host_visible_storage {
        let res = client
            .get_resource::<WgpuServer<AutoCompiler>>(h_item_extents.clone())
            .ok();
        if let Some(res) = res {
            let (src, src_off) = {
                let r = res.resource();
                (r.buffer.clone(), r.offset)
            };
            let bytes = (item_count * EXT_STRIDE * 4) as u64;
            let (buf, ptr) = mapped_read_buffer(&device.device, bytes.max(4), "item extents");
            let mut enc = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("item extents copy"),
                });
            enc.copy_buffer_to_buffer(&src, src_off, &buf, 0, bytes.max(4));
            device.queue.submit([enc.finish()]);
            device
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .expect("extents copy poll");
            let slice = unsafe { std::slice::from_raw_parts(ptr, item_count * EXT_STRIDE) };
            Some((slice.to_vec(), buf))
        } else {
            None
        }
    } else {
        None
    };

    let extent_values_storage;
    let extent_values: &[u32] = if let Some((ref v, _)) = extent_bytes_opt {
        v.as_slice()
    } else {
        extent_values_storage = client.read_one(h_item_extents).expect("extent lanes");
        bytemuck::cast_slice(&extent_values_storage)
    };
    let mut placements = Vec::with_capacity(item_count);
    for item_idx in 0..item_count {
        let e = item_idx * EXT_STRIDE;
        placements.push(ItemPlacement {
            slot_base: slot_base[item_idx],
            slot_count: survivor_totals[item_idx],
            record_count: leader_totals[item_idx],
            page: PageExtent {
                right: key_to_float_host(extent_values[e]),
                bottom: key_to_float_host(extent_values[e + 1]),
                z_min: key_to_float_host(extent_values[e + 2]),
                z_max: key_to_float_host(extent_values[e + 3]),
            },
            ink: InkExtent {
                min: [
                    key_to_float_host(extent_values[e + 4]),
                    key_to_float_host(extent_values[e + 5]),
                    key_to_float_host(extent_values[e + 8]),
                ],
                max: [
                    key_to_float_host(extent_values[e + 6]),
                    key_to_float_host(extent_values[e + 7]),
                    key_to_float_host(extent_values[e + 9]),
                ],
            },
        });
    }
    placements
}
