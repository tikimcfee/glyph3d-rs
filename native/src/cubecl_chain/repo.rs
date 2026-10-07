use cubecl::wgpu::{AutoGraphicsApi, GraphicsApi, WgpuSetup};

mod buffers;
mod dispatch;
mod prep;
mod tail_emit;
mod tail_readback;

use buffers::allocate_chain_buffers;
use dispatch::{
    launch_block1, launch_block2_geometry, launch_block2_totals,
    ChainProfiler,
};
use prep::prepare_chain_inputs;
use tail_emit::package_slot_device;
use tail_readback::{decode_placements, read_tint_store};

pub(crate) use dispatch::prewarm_pipelines;

// ── the repo parity driver — phase 4, rung 3 ─────────────────────────────────
//
// `--cubecl-repo-check <dir>`: the full chain over a REAL repository — walk,
// per-file items carrying the engine path's OWN ItemParams (file_item_params,
// default origins), decode-from-bytes through the ranked cluster pass, the
// scan, resolve_x (the repo's wrap-back default keeps it live), paginate, and
// the record emitter — diffed against the ENGINE's batched records for the
// same items. Tier contract: glyph_id/row/col EXACT; advance/height/X/Y/Z
// reported as bit-deviation counts and max relative deviation, gated at the
// 1e-4 tier (the chain's f32 Blelloch reassociation vs the engine's f64
// running sums is the documented eps tier — scan-vs-fold was already eps
// there). The per-item record bases come from the ENGINE's own
// ItemPlacement.record_count, so the stream alignment itself is part of the
// fence: a divergent count shifts every later record and the exact lanes
// catch it wholesale.
/// The chain's wall-clock decomposition inside run_repo_chain — rung 5's
/// yardstick. `chain_dur` above is the whole span (and today INCLUDES the
/// emit/readback loop, which readback_dur reports separately); these five
/// carve the pre-readback part so a single number never has to answer for
/// the serial host prelude, the device acquisition, and the launches at
/// once. Wall clock, not GPU timestamps — the bench instrument owns the
/// per-dispatch device view; this one answers "where did the load go".
#[derive(Clone, Copy, Default)]
pub struct ChainPhases {
    /// Serial host prelude: the leader scan + rec_base prefix sums.
    pub prep: std::time::Duration,
    /// Atlas trie load + cluster host inputs + pair filter + item tables.
    pub tables: std::time::Duration,
    /// Device acquisition + the cubecl client. Only the no-caller-device
    /// fallback pays a construction here (`--repo-scan-only`, GPU-less
    /// checks); since rung 5a the render path shares the renderer's device
    /// and this span is near zero.
    pub init: std::time::Duration,
    /// pack_words + buffer allocation + uploads.
    pub upload: std::time::Duration,
    /// The launch block's wall time. First-launch kernel JIT hides here on
    /// a cold process; a cold/warm pair of runs separates it.
    pub dispatch: std::time::Duration,
}

/// The paint/group tables the instance tail needs — the two lanes
/// `compact_records_into` folds in on host (layout.rs's `Paint` doc:
/// compaction destroys the index that names a byte, so paint rides
/// THROUGH it). PerRecord lengths are host-known (one color per leader by
/// construction); Flat paint needs no count at all — the kernel picks
/// `flat_colors[it]` when `is_per_record[it]` is zero. The driver
/// cross-checks the PerRecord total against the device's own leader
/// totals and fails loud, the successor of compact's record/colors
/// length assert.
pub(crate) struct InstanceInputs {
    /// Concatenated per-record colors of the PerRecord items, walk order.
    pub per_record_colors: Vec<u32>,
    /// Per-item start inside `per_record_colors`.
    pub color_base: Vec<u32>,
    /// Per-item 1/0: take the jagged table (indexed by the record ordinal)
    /// or the flat one.
    pub is_per_record: Vec<u32>,
    /// Flat color per item.
    pub flat_colors: Vec<u32>,
    /// group_id per item — the renderer's per-file group row key.
    pub groups: Vec<u32>,
}

/// The renderer's own device, handed to the chain so both run on ONE
/// instance/adapter/device/queue — rung 5a's device merge. The wgpu handles
/// clone as Arcs, so this is a cheap by-value pass; `run_repo_chain` accepts
/// `None` and constructs a device of its own for callers that have none
/// (`--repo-scan-only`, GPU-less checks), which keeps that path exactly as it
/// was.
pub(crate) use crate::gpu::SharedDevice;

/// The endpoint's render-bound output (note 23, E2b): the 32 B slots on
/// device, extracted from the chain's allocator, ready to bind as-is. The
/// guard keeps the pool slice from being handed to a later allocation —
/// the renderer holds it for the scene's lifetime.
pub(crate) struct SlotDevice {
    pub chunk: crate::layout::DeviceSlotChunk,
    pub keep_alive: Box<dyn std::any::Any + Send>,
}

/// THE DEVICE LOAD PATH — bytes and per-file items in, the direct instance
/// slots and placements out. Both `repo_check` (the fence) and `CubeclLayout`
/// (the product) call THIS function.
pub(crate) struct ChainStream {
    pub total_slots: u32,
    /// Per-item placements, decoded from the scatter's extent lanes —
    /// the same reduction `compact_records_into` does on host (page over
    /// ALL records, ink over survivors, min/max order-free).
    pub placements: Vec<crate::layout::ItemPlacement>,
    /// The 32 B slots on device — the endpoint form the renderer binds.
    /// Some whenever the survivor total is nonzero.
    pub slot_device: Option<SlotDevice>,
    /// The tint stream — (glyph_id, color) per slot, slot order.
    /// seg_tint's fold input now that no host arena exists.
    pub tint: crate::layout::TintStore,
    /// 8 u32 per slot — the 32 B form READ BACK to host. EMPTY unless readback_slots
    /// was requested (the fork gate's lane tier reads it; the product never wants it).
    pub slots: Vec<u32>,
    /// The ranked chain's candidate count (diagnostics).
    pub candidates: usize,
    pub chain_dur: std::time::Duration,
    pub readback_dur: std::time::Duration,
    pub phases: ChainPhases,
}

pub(crate) fn run_repo_chain(
    device: Option<&SharedDevice>,
    bytes: &[u8],
    items: &[crate::fold::Item],
    inputs: &InstanceInputs,
    readback_slots: bool,
    field_mode: glyph_field::GlyphFieldMode,
) -> ChainStream {
    let item_count = items.len();
    let n: usize = bytes.len();
    let _chain = tracing::info_span!("chain", n, items = item_count, readback_slots).entered();
    let t_chain0 = std::time::Instant::now();
    let sp_prep = tracing::info_span!("chain.prep").entered();

    let t_tables = std::time::Instant::now();
    drop(sp_prep);
    let sp_tables = tracing::info_span!("chain.tables").entered();

    let trie = crate::atlas::default_trie();
    let host_inputs = prepare_chain_inputs(
        bytes,
        items,
        &trie,
        readback_slots,
        Some(inputs),
    );

    let t_init = std::time::Instant::now();
    drop(sp_tables);
    let sp_init = tracing::info_span!("chain.init").entered();

    let owned_ctx;
    let owned_dev;
    let device_ref = match device {
        Some(d) => d,
        None => {
            owned_ctx = pollster::block_on(crate::gpu::init(None));
            owned_dev = SharedDevice::from_ctx(&owned_ctx);
            &owned_dev
        }
    };
    let client = match device_ref.cubecl_device {
        Some(ref cd) => cubecl::Device::Wgpu(cd.clone()).client(),
        None => {
            let setup = WgpuSetup {
                instance: device_ref.instance.clone(),
                adapter: device_ref.adapter.clone(),
                device: device_ref.device.clone(),
                queue: device_ref.queue.clone(),
                backend: AutoGraphicsApi::backend(),
            };
            let cdev = cubecl::wgpu::init_device(setup, Default::default());
            cubecl::Device::Wgpu(cdev).client()
        }
    };

    let t_upload = std::time::Instant::now();
    drop(sp_init);

    let span_upload = tracing::info_span!("chain.upload", live_bytes = tracing::field::Empty);
    let sp_upload = span_upload.enter();

    if let Some(dev) = device {
        if let Some(h) = dev.prewarm_handle.lock().unwrap().take() {
            let t_prewarm_wait = std::time::Instant::now();
            h.join().expect("cubecl prewarm thread panicked");
            log::info!("joined prewarm thread before buffer allocation in {:?}", t_prewarm_wait.elapsed());
        }
    }

    let needs_tint = readback_slots
        || (field_mode != glyph_field::GlyphFieldMode::Derived
            && inputs.is_per_record.iter().any(|&x| x != 0));
    let buffers::BufferAllocationResult {
        buffers: mut buf,
        live_bytes,
    } = allocate_chain_buffers(
        &client,
        bytes,
        item_count,
        &host_inputs,
        inputs,
        &trie,
        needs_tint,
        field_mode == glyph_field::GlyphFieldMode::Derived,
    );

    let t_dispatch = std::time::Instant::now();
    span_upload.record("live_bytes", live_bytes);
    drop(sp_upload);
    drop(span_upload);

    let sp_dispatch = tracing::info_span!("chain.dispatch").entered();

    let mut prof = ChainProfiler::new();

    let t_b1 = std::time::Instant::now();
    launch_block1(&client, n, &host_inputs, &buf, &mut prof);
    let dur_b1 = t_b1.elapsed();

    let t_b2t = std::time::Instant::now();
    launch_block2_totals(
        &client,
        n,
        item_count,
        &host_inputs,
        &buf,
        &mut prof,
    );
    let dur_b2t = t_b2t.elapsed();

    let t_b2g = std::time::Instant::now();
    launch_block2_geometry(
        &client,
        n,
        item_count,
        &host_inputs,
        &buf,
        &mut prof,
        field_mode == glyph_field::GlyphFieldMode::Derived,
    );
    let dur_b2g = t_b2g.elapsed();

    let t_fl = std::time::Instant::now();
    let _ = client.flush();
    let dur_fl = t_fl.elapsed();

    tracing::info!(
        "dispatch breakdown: b1 {:?}, b2_totals {:?}, b2_geo {:?}, flush {:?}",
        dur_b1, dur_b2t, dur_b2g, dur_fl
    );

    let t_rb = std::time::Instant::now();
    drop(sp_dispatch);
    let span_tail = tracing::info_span!(
        "tail",
        live_bytes,
        total_records = tracing::field::Empty,
        total_slots = tracing::field::Empty,
    );
    let _sp_tail = span_tail.enter();
    let sp_totals = tracing::info_span!("tail.totals").entered();

    let (leader_totals, survivor_totals, total_records, total_slots, _item_record_bases, item_slot_bases) = (
        host_inputs.leader_totals.clone(),
        host_inputs.survivor_totals.clone(),
        host_inputs.total_records,
        host_inputs.total_slots,
        host_inputs.item_record_bases.clone(),
        host_inputs.item_slot_bases.clone(),
    );

    let mut expect = 0u32;
    for (it, &is_pr) in inputs.is_per_record.iter().enumerate() {
        if is_pr != 0 {
            assert_eq!(
                inputs.color_base[it], expect,
                "paint/record misalignment at item {it}: colors start at {} but the records say {expect}",
                inputs.color_base[it],
            );
            expect += leader_totals[it];
        }
    }
    assert_eq!(
        inputs.per_record_colors.len(),
        expect as usize,
        "paint is indexed by record: {} colors for {} records",
        inputs.per_record_colors.len(),
        expect,
    );
    drop(sp_totals);
    span_tail.record("total_records", total_records);
    span_tail.record("total_slots", total_slots);

    // The Ladder: drop all intermediate lanes whose last reader ran during geometry
    {
        let _sp_ladder = tracing::info_span!("tail.ladder").entered();
        buf.release_pre_survivor();
    }

    let bytes_per_slot = if field_mode == glyph_field::GlyphFieldMode::Derived { 20u64 } else { 32u64 };
    let required_slot_buffer_bytes = (total_slots as u64) * bytes_per_slot;
    assert!(
        required_slot_buffer_bytes <= device_ref.max_buffer_size,
        "the endpoint needs one {required_slot_buffer_bytes} B slot buffer ({field_mode:?} mode) — over this device's \
         max_buffer_size ({}); chunked slot buffers are the named \
         follow-up{}",
        device_ref.max_buffer_size,
        if field_mode != glyph_field::GlyphFieldMode::Derived && (total_slots as u64) * 20 <= device_ref.max_buffer_size {
            format!("; try running with `--field-mode derived` (requires only {} B)", (total_slots as u64) * 20)
        } else {
            String::new()
        }
    );

    let placements = if readback_slots {
        decode_placements(
            &client,
            device_ref,
            buf.h_item_extents.clone(),
            item_count,
            &item_slot_bases,
            &survivor_totals,
            &leader_totals,
        )
    } else {
        host_inputs.placements
    };

    let c = if readback_slots && host_inputs.has_cluster {
        if let Some(ref h_candidate_total) = buf.h_candidate_total {
            let tb = client.read_one(h_candidate_total.clone()).expect("candidate count");
            bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize
        } else {
            0
        }
    } else {
        0
    };

    let h_instance_slots = buf.h_instance_slots.clone();
    let h_instance_tints = buf.h_instance_tints.clone();

    let sp_tint = tracing::info_span!("tail.tint").entered();
    let tint_store = if needs_tint {
        read_tint_store(&client, device_ref, h_instance_tints.clone(), total_slots, readback_slots)
    } else {
        crate::layout::TintStore::Host(Vec::new())
    };
    drop(sp_tint);

    let mut slots_all = Vec::new();
    if readback_slots {
        let slot_words = if field_mode == glyph_field::GlyphFieldMode::Derived { 5 } else { 8 };
        let sb = client.read_one(h_instance_slots.clone()).expect("read slot scatter");
        slots_all = bytemuck::cast_slice::<u8, u32>(&sb)[..total_slots as usize * slot_words].to_vec();
    }

    let t_pre_pkg = std::time::Instant::now();
    let slot_device = package_slot_device(&client, h_instance_slots, total_slots);
    let dur_pkg = t_pre_pkg.elapsed();

    let t_pre_rel = std::time::Instant::now();
    buf.release_survivor_scan();
    let dur_rel = t_pre_rel.elapsed();

    let t_pre_prof = std::time::Instant::now();
    prof.print_summary();
    let dur_prof = t_pre_prof.elapsed();

    tracing::info!(
        "tail probes: pkg {} µs, rel {} µs, prof {} µs",
        dur_pkg.as_micros(),
        dur_rel.as_micros(),
        dur_prof.as_micros(),
    );

    ChainStream {
        total_slots,
        placements,
        slot_device,
        tint: tint_store,
        slots: slots_all,
        candidates: c,
        chain_dur: t_chain0.elapsed(),
        readback_dur: t_rb.elapsed(),
        phases: ChainPhases {
            prep: t_tables.duration_since(t_chain0),
            tables: t_init.duration_since(t_tables),
            init: t_upload.duration_since(t_init),
            upload: t_dispatch.duration_since(t_upload),
            dispatch: t_rb.duration_since(t_dispatch),
        },
    }
}
