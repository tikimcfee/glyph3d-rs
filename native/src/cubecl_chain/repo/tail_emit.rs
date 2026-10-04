//! Tail emission passes: chunked wire records emission, slot scattering, and slot device binding.

use cubecl::client::Client;
use cubecl::prelude::*;
use cubecl::server::Handle;
use cubecl::wgpu::{AutoCompiler, WgpuServer};

use super::super::tail::{emit_records, scatter_slots};
use super::super::{LC_STRIDE, LM_STRIDE};
use super::buffers::ChainBuffers;
use super::dispatch::{cubes_of, ChainProfiler};
use super::{InstanceInputs, SlotDevice};

/// Emits the 8-word wire stream chunked in rolling windows.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_records_chunked(
    client: &Client,
    n: usize,
    n_words: usize,
    total_records: u32,
    chunk_recs_cap: usize,
    ir_len: usize,
    item_count: usize,
    h_fl: Handle,
    h_wc: Handle,
    h_ir: Handle,
    h_base: Handle,
    h_lm: Handle,
    h_lc: Handle,
    h_sm: Handle,
    h_hgt: Handle,
    h_gi: Handle,
    prof: &mut ChainProfiler,
) -> Vec<u32> {
    let mut recs_all = Vec::with_capacity(total_records as usize * 8);
    let chunk_recs = chunk_recs_cap.min(total_records as usize).max(1);
    let h_recs = client.empty(chunk_recs * 8 * 4);
    let mut first = 0usize;

    while first < total_records as usize {
        let take = chunk_recs.min(total_records as usize - first);
        let span_win = tracing::info_span!(
            "tail.window",
            tail = "records",
            first,
            take,
        );
        let _sp_win = span_win.enter();
        let h_win = client.create_from_slice(bytemuck::cast_slice(&[first as u32]));
        let sp_emit = tracing::info_span!("tail.window.emit").entered();
        unsafe {
            prof.begin(client, "emit_records");
            emit_records::launch_unchecked(
                client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_ir.clone(), ir_len),
                BufferArg::from_raw_parts(h_base.clone(), item_count),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_hgt.clone(), n),
                BufferArg::from_raw_parts(h_gi.clone(), n),
                BufferArg::from_raw_parts(h_recs.clone(), take * 8),
                BufferArg::from_raw_parts(h_win, 1),
            );
            prof.end(client, "emit_records");
        }
        drop(sp_emit);

        let sp_rb = tracing::info_span!("tail.window.readback").entered();
        let cb = client.read_one(h_recs.clone()).expect("read chunk");
        let csv: &[u32] = bytemuck::cast_slice(&cb);
        recs_all.extend_from_slice(&csv[..take * 8]);
        drop(sp_rb);
        first += take;
    }

    recs_all
}

/// Executes slot scattering directly into the device buffer that the renderer will bind.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scatter_slots_direct(
    client: &Client,
    n: usize,
    n_words: usize,
    total_slots: u32,
    ir_len: usize,
    item_count: usize,
    inputs: &InstanceInputs,
    buf: &ChainBuffers,
    prof: &mut ChainProfiler,
) -> (Handle, Handle) {
    let sp_scatter = tracing::info_span!("tail.scatter").entered();
    let h_slots = client.empty(total_slots.max(1) as usize * 8 * 4);
    let h_tint = client.empty(total_slots.max(1) as usize * 2 * 4);

    unsafe {
        prof.begin(client, "scatter_slots");
        scatter_slots::launch_unchecked(
            client,
            cubes_of(n),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(buf.h_fl.clone(), n_words),
            BufferArg::from_raw_parts(buf.h_wc.clone(), n),
            BufferArg::from_raw_parts(buf.h_ir.clone(), ir_len),
            BufferArg::from_raw_parts(buf.h_lm.clone(), n * LM_STRIDE),
            BufferArg::from_raw_parts(buf.h_sm.clone(), n),
            BufferArg::from_raw_parts(buf.h_hgt.clone(), n),
            BufferArg::from_raw_parts(buf.h_gi.clone(), n),
            BufferArg::from_raw_parts(buf.h_pr_colors.clone(), inputs.per_record_colors.len()),
            BufferArg::from_raw_parts(buf.h_color_base.clone(), item_count),
            BufferArg::from_raw_parts(buf.h_is_pr.clone(), item_count),
            BufferArg::from_raw_parts(buf.h_flat_colors.clone(), item_count),
            BufferArg::from_raw_parts(buf.h_groups.clone(), item_count),
            BufferArg::from_raw_parts(buf.h_sv.clone(), n),
            BufferArg::from_raw_parts(h_slots.clone(), total_slots.max(1) as usize * 8),
            BufferArg::from_raw_parts(h_tint.clone(), total_slots.max(1) as usize * 2),
        );
        prof.end(client, "scatter_slots");
    }
    drop(sp_scatter);
    (h_slots, h_tint)
}

/// Consumes the device slot handle into a renderer `SlotDevice`.
pub(crate) fn package_slot_device(
    client: &Client,
    h_slots: Handle,
    total_slots: u32,
) -> Option<SlotDevice> {
    if total_slots == 0 {
        return None;
    }
    let res = client
        .get_resource::<WgpuServer<AutoCompiler>>(h_slots)
        .expect("slot buffer resource");
    let (buffer, offset) = {
        let r = res.resource();
        (r.buffer.clone(), r.offset)
    };
    assert!(
        offset % 16 == 0,
        "slot buffer's pool offset {offset} breaks the storage binding alignment"
    );
    Some(SlotDevice {
        chunk: crate::layout::DeviceSlotChunk {
            buffer,
            offset,
            slots: total_slots,
        },
        keep_alive: Box::new(res),
    })
}
