//! Host records → device slots: the 48 B engine records transcoded into a
//! mode's slot record and uploaded in binding-sized chunks.
//!
//! Moved from `native/src/glyph_scene/buffers.rs` into each mode crate when
//! the field split (2026-10), then hoisted here from both (C1, 2026-10-09):
//! the two copies differed only in the slot type, the per-record transcode
//! and their labels, which are now [`Transcode`] and [`UploadLabels`]. The
//! input is a list of host slices rather than the `GlyphArena` that produced
//! them, so no field crate depends on the layout engine.

use crate::storage::SlotRecord;
use crate::GlyphInstance;

/// How a mode turns one engine record into its slot.
///
/// Implementations carry whatever the transcode reads beyond the record
/// (Derived's item table), and are shared across the parallel transcode.
pub trait Transcode: Sync {
    type Slot: SlotRecord;

    /// Write the slot for `src` to `dst`.
    ///
    /// # Safety
    /// `dst` must be valid for a write of one `Self::Slot`; it need not be
    /// initialised.
    unsafe fn transcode(&self, src: &GlyphInstance, dst: *mut Self::Slot);
}

/// A mode's buffer labels, so captures and validation errors name the mode.
pub struct UploadLabels {
    /// The slot buffers: this, or `"{buffers} {i}/{n}"` when chunked.
    pub buffers: &'static str,
    /// The discrete path's staging buffer.
    pub staging: &'static str,
    /// The discrete path's staging → VRAM copy encoder.
    pub staging_copy: &'static str,
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

/// COPY_DST for partial per-slot edit uploads; COPY_SRC for the slot-dump
/// verification readback.
const SLOT_USAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::COPY_DST)
    .union(wgpu::BufferUsages::COPY_SRC);

const PARALLEL_TRANSCODE_THRESHOLD: usize = 16384;
const CHUNK_SIZE: usize = 65536;

struct SendPtr<S>(*mut S);
impl<S> Clone for SendPtr<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for SendPtr<S> {}
unsafe impl<S> Send for SendPtr<S> {}
unsafe impl<S> Sync for SendPtr<S> {}
impl<S> SendPtr<S> {
    #[inline(always)]
    unsafe fn add(self, offset: usize) -> *mut S {
        self.0.add(offset)
    }
}

/// One binding chunk's share of the host records: the arena pieces that
/// cover it, each at its slot offset within the chunk.
struct ChunkSources<'s> {
    pieces: Vec<(usize, &'s [GlyphInstance])>,
    /// Slots the pieces cover.
    written: usize,
    /// Slots the chunk binds (`written`, or 1 for the empty field's zeroed slot).
    count: usize,
}

impl ChunkSources<'_> {
    fn size<S>(&self) -> u64 {
        (self.count * std::mem::size_of::<S>()) as u64
    }

    /// Transcode every piece into `dest_ptr`, then zero the slots no piece
    /// covers.
    unsafe fn transcode_into<T: Transcode>(&self, dest_ptr: *mut T::Slot, transcode: &T) {
        let dest = SendPtr(dest_ptr);
        for &(dst_off, slice) in &self.pieces {
            if slice.len() >= PARALLEL_TRANSCODE_THRESHOLD {
                use rayon::prelude::*;
                slice
                    .par_chunks(CHUNK_SIZE)
                    .enumerate()
                    .for_each(|(chunk_idx, chunk)| {
                        let out_ptr = unsafe { dest.add(dst_off + chunk_idx * CHUNK_SIZE) };
                        for (k, g) in chunk.iter().enumerate() {
                            unsafe {
                                transcode.transcode(g, out_ptr.add(k));
                            }
                        }
                    });
            } else {
                let out_ptr = unsafe { dest.0.add(dst_off) };
                for (k, g) in slice.iter().enumerate() {
                    unsafe {
                        transcode.transcode(g, out_ptr.add(k));
                    }
                }
            }
        }
        // Pad any remainder (e.g. mapped-empty arena zeroed slot):
        for k in self.written..self.count {
            unsafe {
                std::ptr::write(dest_ptr.add(k), <T::Slot as bytemuck::Zeroable>::zeroed());
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[inline]
fn upload_direct_metal<T: Transcode>(
    device: &wgpu::Device,
    label: &str,
    chunk: &ChunkSources<'_>,
    transcode: &T,
) -> wgpu::Buffer {
    use wgpu::hal::Device as HalDevice;
    let hal_usage = wgpu::BufferUses::STORAGE_READ_ONLY
        | wgpu::BufferUses::COPY_DST
        | wgpu::BufferUses::COPY_SRC
        | wgpu::BufferUses::MAP_READ;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let size = chunk.size::<T::Slot>();
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(label),
            size,
            usage: hal_usage,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal slot buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal slot map");
    let dest_ptr = mapping.ptr.as_ptr() as *mut T::Slot;

    unsafe {
        chunk.transcode_into(dest_ptr, transcode);
        hal_dev.unmap_buffer(&hal_buf);
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: SLOT_USAGE | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        )
    }
}

#[inline]
fn upload_staged_discrete<T: Transcode>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    chunk: &ChunkSources<'_>,
    transcode: &T,
    labels: &UploadLabels,
) -> wgpu::Buffer {
    // Discrete GPU or non-Mappable fallback: stream the parallel transcode
    // directly into a mapped staging buffer — no intermediate heap Vec of
    // slots.
    //
    // A 20 B slot stream is 16-aligned only when its count is a multiple of
    // 4, and an off-16 copy runs at about half speed (crate::copy, C16): pad
    // the staging buffer (wgpu-core copies all of it at `unmap`) and split
    // the copy out; the VRAM buffer keeps its exact size and bytes. A 32 B
    // stream is always aligned, and both are then one plain copy.
    let size = chunk.size::<T::Slot>();
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(labels.staging),
        size: crate::padded_staging_size(size),
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        let mut mapped = staging_buf
            .slice(..)
            .get_mapped_range_mut()
            .expect("staging mapped range");
        let dest_ptr = mapped.slice(..).as_raw_element_ptr().as_ptr() as *mut T::Slot;
        unsafe {
            chunk.transcode_into(dest_ptr, transcode);
        }
    }
    staging_buf.unmap();
    let vram_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: SLOT_USAGE,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some(labels.staging_copy),
    });
    crate::copy_split(&mut encoder, &staging_buf, 0, &vram_buf, 0, size);
    queue.submit([encoder.finish()]);
    vram_buf
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
pub fn upload_host_slots<T: Transcode>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    slices: &[&[GlyphInstance]],
    glyph_count: usize,
    direct_host_upload: bool,
    transcode: &T,
    labels: &UploadLabels,
) -> HostUpload {
    let binding_limit = device.limits().max_storage_buffer_binding_size as usize;
    let chunk_cap = (binding_limit / std::mem::size_of::<T::Slot>()).max(1);
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
    // The transcode walk: render chunks (slot stride) intersect the arena's
    // own chunks (host Vec or mapped slices) — the two chunkings do not in
    // general coincide, and a straddling range must transcode bit-identically
    // to a contiguous one (the transcode is per record).
    let mut ac = 0usize;
    let mut arena_base = 0usize;
    let buffers: Vec<wgpu::Buffer> = chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let label = if chunk_counts.len() == 1 {
                labels.buffers.to_string()
            } else {
                format!("{} {i}/{}", labels.buffers, chunk_counts.len())
            };
            let first = i * chunk_cap;
            let need_end = first + count as usize;
            // Gather the arena slice sub-ranges covering this chunk:
            let mut chunk = ChunkSources { pieces: Vec::new(), written: 0, count: count as usize };
            while chunk.written < chunk.count && ac < slices.len() {
                let c = slices[ac];
                if arena_base + c.len() <= first {
                    arena_base += c.len();
                    ac += 1;
                    continue;
                }
                let lo = first.saturating_sub(arena_base);
                let hi = (need_end - arena_base).min(c.len());
                if hi > lo {
                    chunk.pieces.push((chunk.written, &c[lo..hi]));
                    chunk.written += hi - lo;
                }
                if hi == c.len() {
                    arena_base += c.len();
                    ac += 1;
                }
            }

            if direct_host_upload {
                #[cfg(target_os = "macos")]
                {
                    upload_direct_metal(device, &label, &chunk, transcode)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    upload_staged_discrete(device, queue, &label, &chunk, transcode, labels)
                }
            } else {
                upload_staged_discrete(device, queue, &label, &chunk, transcode, labels)
            }
        })
        .collect();
    HostUpload { chunk_capacity: chunk_cap, chunk_counts, buffers }
}
