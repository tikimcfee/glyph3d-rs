//! Device buffer allocation and staging for unified and discrete memory architectures.
//!
//! Generic over the emitted slot format ([`SlotEmit`]): the buffer is sized
//! `slots × size_of::<E::Slot>()` and Pass 2 writes straight into it — mapped
//! device memory on unified Metal, a mapped staging buffer + one copy
//! elsewhere. Neither path ever materializes the 48 B host record.

use rayon::prelude::*;

use crate::atlas::TrieTable;
use crate::gpu::SharedDevice;
use crate::layout::{DeviceSlotChunk, LayoutItem};
use super::pass2_device::{emit_windows, emoji_tint_pairs, layout_pass2_device, plan_windows, SlotEmit, SlotWindow, WindowSink};
use super::types::{ItemPrepass, Pass2DeviceOutput};
#[cfg(target_os = "macos")]
use glyph_field::create_mapped_slot_buffer;

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
        // Timed apart from Pass 2: it READS the slots just written, which is
        // cheap from cached memory and was not from write-combined staging.
        let sp_tints = tracing::info_span!("hyper.emoji_tints").entered();
        let pairs = emoji_tint_pairs::<E>(dest_addr, &out);
        drop(sp_tints);
        (out, pairs)
    }
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

    // Three discrete staging strategies, measured on an RTX 5090 over a
    // 102 MB tree (derived backend medians, 2026-10-09):
    // - windowed (the DEFAULT, C22): Pass 2 writes each 64 MiB window
    //   straight into one of two mapped staging buffers, uploaded while the
    //   next fills — 121 ms;
    // - host (`GLYPH_STAGING=host`): Pass 2 writes host memory, streamed
    //   through one 64 MiB buffer — 194 ms (the full host -> staging copy);
    // - single (`GLYPH_STAGING=single`): one mapped-at-creation buffer, which
    //   wgpu-core zero-fills and copies, and whose emoji tint re-read hits
    //   write-combined memory — 482 ms.
    // The fallbacks stay for hardware nobody has measured (an integrated GPU).
    let staging = std::env::var("GLYPH_STAGING").unwrap_or_default();
    let (pass2, emoji_tint_pairs, chunks) = match staging.as_str() {
        "single" if plan.total_bytes <= max_buf => stage_single_buffer::<E>(dev, inputs, label, &plan),
        "host" => stage_host_memory::<E>(dev, inputs, label, &plan),
        _ => stage_windowed::<E>(dev, inputs, label, &plan),
    };

    DeviceEmission {
        chunks,
        chunk_slots: plan.chunk_cap,
        mapped_base: None,
        pass2,
        emoji_tint_pairs,
    }
}

/// Windowed direct-to-staging emission (C22): Pass 2 writes each 64 MiB
/// window of the slot stream straight into one of two mapped staging
/// buffers, which is unmapped and copied to its VRAM destination while the
/// next window fills the other. No host-sized allocation and no host ->
/// staging memcpy (the host path's ~70-100 ms on a 1.7 GB stream).
fn stage_windowed<E: SlotEmit>(
    dev: &SharedDevice,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
    plan: &ChunkPlan,
) -> (Pass2DeviceOutput, Vec<Vec<u32>>, Vec<DeviceSlotChunk>) {
    const WINDOW_BYTES: usize = 64 * 1024 * 1024;
    let total_slots = plan.total_bytes as usize / plan.slot_bytes;
    let windows = plan_windows(inputs.chunk_slot_bases, total_slots, (WINDOW_BYTES / plan.slot_bytes).max(1));
    let widest = windows.iter().map(|w| w.slots.len() * plan.slot_bytes).max().unwrap_or(0);
    let staging_size = glyph_field::padded_staging_size(widest.max(plan.slot_bytes) as u64);

    let chunks: Vec<DeviceSlotChunk> = plan
        .chunk_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let chunk_label = if plan.chunk_counts.len() == 1 {
                label.to_string()
            } else {
                format!("{label} {i}/{}", plan.chunk_counts.len())
            };
            DeviceSlotChunk {
                buffer: create_chunk_buffer(dev, (count as usize * plan.slot_bytes) as u64, &chunk_label),
                offset: 0,
                slots: count,
            }
        })
        .collect();

    let staging = [0, 1].map(|i| {
        dev.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(if i == 0 { "glyph slots window staging A" } else { "glyph slots window staging B" }),
            size: staging_size,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::MAP_WRITE,
            mapped_at_creation: true,
        })
    });
    let mut sink = GpuWindowSink {
        dev,
        staging: &staging,
        chunks: &chunks,
        chunk_cap: plan.chunk_cap,
        slot_bytes: plan.slot_bytes,
        mapped: [true, true],
        last_submit: [None, None],
        view: None,
        map_wait: std::time::Duration::ZERO,
    };
    let sp = tracing::info_span!("hyper.pass2.windowed", windows = windows.len()).entered();
    let (pass2, pairs) = emit_windows::<E>(inputs, &windows, total_slots, &mut sink);
    let map_wait = sink.map_wait;
    drop(sp);
    let t_drain = std::time::Instant::now();
    let _ = dev.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
    tracing::info!(
        windows = windows.len(),
        map_wait_ms = map_wait.as_secs_f64() * 1e3,
        drain_ms = t_drain.elapsed().as_secs_f64() * 1e3,
        "windowed staging"
    );
    (pass2, pairs, chunks)
}

/// The windowed path's sink: two mapped staging buffers used alternately.
struct GpuWindowSink<'a> {
    dev: &'a SharedDevice,
    staging: &'a [wgpu::Buffer; 2],
    chunks: &'a [DeviceSlotChunk],
    chunk_cap: usize,
    slot_bytes: usize,
    mapped: [bool; 2],
    /// The submission that copies out of each buffer: re-mapping a buffer
    /// waits on that copy alone, not on everything queued since.
    last_submit: [Option<wgpu::SubmissionIndex>; 2],
    view: Option<wgpu::BufferViewMut>,
    map_wait: std::time::Duration,
}

impl WindowSink for GpuWindowSink<'_> {
    fn begin(&mut self, k: usize, _window: &SlotWindow) -> usize {
        let b = k % 2;
        if !self.mapped[b] {
            let t = std::time::Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            self.staging[b].slice(..).map_async(wgpu::MapMode::Write, move |res| {
                let _ = tx.send(res);
            });
            self.dev
                .device
                .poll(wgpu::PollType::Wait { submission_index: self.last_submit[b].take(), timeout: None })
                .expect("staging poll failed");
            rx.recv().expect("staging callback dropped").expect("staging map_async failed");
            self.mapped[b] = true;
            self.map_wait += t.elapsed();
        }
        let mut view = self.staging[b].slice(..).get_mapped_range_mut().expect("window mapped range");
        let addr = view.slice(..).as_raw_element_ptr().as_ptr() as usize;
        self.view = Some(view);
        addr
    }

    fn end(&mut self, k: usize, window: &SlotWindow) {
        let b = k % 2;
        self.view = None;
        self.staging[b].unmap();
        self.mapped[b] = false;
        let mut encoder = self.dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("glyph_window_copy"),
        });
        // The window's slots may straddle VRAM chunk boundaries: one copy per
        // chunk it touches.
        let mut slot = window.slots.start;
        while slot < window.slots.end {
            let chunk = slot / self.chunk_cap;
            let chunk_end = ((chunk + 1) * self.chunk_cap).min(window.slots.end);
            let src = ((slot - window.slots.start) * self.slot_bytes) as u64;
            let dst = ((slot - chunk * self.chunk_cap) * self.slot_bytes) as u64;
            let len = ((chunk_end - slot) * self.slot_bytes) as u64;
            glyph_field::copy_split(&mut encoder, &self.staging[b], src, &self.chunks[chunk].buffer, dst, len);
            slot = chunk_end;
        }
        self.last_submit[b] = Some(self.dev.queue.submit([encoder.finish()]));
    }
}

fn stage_single_buffer<E: SlotEmit>(
    dev: &SharedDevice,
    inputs: &EmitInputs<'_, '_>,
    label: &str,
    plan: &ChunkPlan,
) -> (Pass2DeviceOutput, Vec<Vec<u32>>, Vec<DeviceSlotChunk>) {
    // Padded to the copy fast path (glyph_field::copy, C16): wgpu-core copies
    // this whole buffer from its own staging at `unmap`, and a size off 16 B
    // halves that copy's throughput. The pad is never copied out.
    let staging_size = glyph_field::padded_staging_size(plan.total_bytes.max(plan.slot_bytes as u64));
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
                glyph_field::copy_split(&mut encoder, &staging_buf, src_offset, &buffer, 0, copy_size);
            }
            DeviceSlotChunk {
                buffer,
                offset: 0,
                slots: count,
            }
        })
        .collect();

    dev.queue.submit([encoder.finish()]);
    let _ = dev.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
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

    // 64 MiB streaming staging buffer:
    // 1. PCIe DMA saturation: transfers >= 32-64 MiB saturate ~98% of line-rate bandwidth
    //    (~25 GB/s on PCIe 4.0/5.0 x16); larger buffers yield diminishing returns (<1%).
    // 2. Bounded VRAM overhead: on mega repos (e.g. Linux kernel, ~25 GB slot storage),
    //    staging chunk sizes equal to target chunks (1.9 GiB) risk immediate device OOM.
    //    Capping at 64 MiB limits staging VRAM overhead to 0.2% of 32 GB.
    // 3. Low sync latency: copying 64 MiB takes ~2.5 ms, making CPU/GPU map_async sync
    //    overhead (~30 µs) less than 1.5% of the transfer time.
    const STAGING_BYTES: usize = 64 * 1024 * 1024;

    // Allocate ONE reusable staging buffer mapped at creation.
    // Total VRAM overhead for staging is strictly capped at 64 MiB.
    let staging_buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glyph slots streaming staging"),
        size: STAGING_BYTES as u64,
        usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::MAP_WRITE,
        mapped_at_creation: true,
    });

    let mut is_mapped = true;

    // Where the stream's time goes (C22): `map_wait` is waiting for the
    // previous slice's copy to release the staging buffer, `memcpy` the CPU
    // copy host -> staging (every slot read and written again), `submit`
    // encoding and queueing the copy into VRAM, `drain` the final wait. One
    // span and one summary event per load, not one per slice.
    let sp_stream = tracing::info_span!("hyper.staging.stream", bytes = plan.total_bytes).entered();
    let (mut t_map_wait, mut t_memcpy, mut t_submit) =
        (std::time::Duration::ZERO, std::time::Duration::ZERO, std::time::Duration::ZERO);
    let mut slices = 0usize;

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
            let src_offset = i * plan.chunk_cap * plan.slot_bytes;
            let copy_bytes = (count as usize * plan.slot_bytes)
                .min((plan.total_bytes as usize).saturating_sub(src_offset));

            let buffer = create_chunk_buffer(dev, chunk_size, &chunk_label);

            let mut written = 0;
            while written < copy_bytes {
                let slice_len = (copy_bytes - written).min(STAGING_BYTES);

                if !is_mapped {
                    let t = std::time::Instant::now();
                    let (tx, rx) = std::sync::mpsc::channel();
                    staging_buf.slice(..).map_async(wgpu::MapMode::Write, move |res| {
                        let _ = tx.send(res);
                    });
                    dev.device
                        .poll(wgpu::PollType::Wait {
                            submission_index: None,
                            timeout: None,
                        })
                        .expect("staging poll failed");
                    rx.recv()
                        .expect("staging callback dropped")
                        .expect("staging map_async failed");
                    is_mapped = true;
                    t_map_wait += t.elapsed();
                }

                let t = std::time::Instant::now();

                {
                    let mut mapped = staging_buf
                        .slice(..)
                        .get_mapped_range_mut()
                        .expect("staging mapped range");
                    let dest_ptr = mapped.slice(..).as_raw_element_ptr().as_ptr();
                    // Parallel: one thread copied at ~15-19 GB/s, under the
                    // box's copy bandwidth (C22). Same bytes, same places.
                    let (src, dst) = unsafe {
                        (
                            std::slice::from_raw_parts(host_ptr.add(src_offset + written), slice_len),
                            std::slice::from_raw_parts_mut(dest_ptr, slice_len),
                        )
                    };
                    const PIECE: usize = 1 << 20;
                    dst.par_chunks_mut(PIECE)
                        .zip(src.par_chunks(PIECE))
                        .for_each(|(d, s)| d.copy_from_slice(s));
                }
                staging_buf.unmap();
                is_mapped = false;
                t_memcpy += t.elapsed();
                let t = std::time::Instant::now();

                let mut encoder = dev.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("glyph_hyper_chunk_slice_copy"),
                });
                glyph_field::copy_split(
                    &mut encoder,
                    &staging_buf,
                    0,
                    &buffer,
                    written as u64,
                    slice_len as u64,
                );
                dev.queue.submit([encoder.finish()]);
                t_submit += t.elapsed();
                slices += 1;

                written += slice_len;
            }

            DeviceSlotChunk {
                buffer,
                offset: 0,
                slots: count,
            }
        })
        .collect();

    // Ensure last copy completes before staging buffer is dropped.
    let t_drain = std::time::Instant::now();
    let _ = dev.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let t_drain = t_drain.elapsed();
    tracing::info!(
        slices,
        map_wait_ms = t_map_wait.as_secs_f64() * 1e3,
        memcpy_ms = t_memcpy.as_secs_f64() * 1e3,
        submit_ms = t_submit.as_secs_f64() * 1e3,
        drain_ms = t_drain.as_secs_f64() * 1e3,
        "staging stream"
    );
    drop(sp_stream);
    drop(staging_buf);

    unsafe {
        std::alloc::dealloc(host_ptr, layout);
    }

    (pass2, emoji_tint_pairs, chunks)
}

