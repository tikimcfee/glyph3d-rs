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
use tail_emit::{emit_records_chunked, package_slot_device, scatter_slots_direct};
use tail_readback::{decode_placements, read_tint_store};

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

/// What the chain's TAIL emits. `Records` is the check/verify shape (the
/// 8-word wire stream, read back chunked); `Instances` is the product
/// shape (the pack kernel writes 48 B GlyphInstance slots, extents and
/// totals come back item_count-sized); `Both` runs the two tails in one
/// driver pass — the fork gate's mode, so the fence sees the product tail
/// and the record tier against the same dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainMode {
    Records,
    Instances,
    Both,
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

/// THE DEVICE LOAD PATH — bytes and per-file items in, the record stream
/// out. Both `repo_check` (the fence) and `CubeclLayout` (the product)
/// call THIS function: a second copy of the driver would mean the gate no
/// longer fences the path the renderer runs, which is the entire point of
/// the cubecl-fork gate.
pub(crate) struct ChainStream {
    /// 8 u32 per record (x, y, z, advance, height, gi, row, col), items in
    /// walk order, ordinal order within items. EMPTY unless Records/Both.
    pub records: Vec<u32>,
    /// Per-item first-record index (prefix sums over the DEVICE leader
    /// totals since rung 5b — the serial CPU scan is gone from every mode).
    pub rec_base: Vec<u32>,
    pub total_records: u32,
    pub total_slots: u32,
    /// Per-item placements, decoded from the scatter's extent lanes —
    /// the same reduction `compact_records_into` does on host (page over
    /// ALL records, ink over survivors, min/max order-free). EMPTY unless
    /// Instances/Both.
    pub placements: Vec<crate::layout::ItemPlacement>,
    /// The 32 B slots on device — the endpoint form the renderer binds.
    /// Some whenever Instances/Both ran and the survivor total is nonzero.
    pub slot_device: Option<SlotDevice>,
    /// The tint stream — (glyph_id, color) per slot, slot order.
    /// seg_tint's fold input now that no host arena exists. Instances/Both.
    pub tint: crate::layout::TintStore,
    /// 8 u32 per slot — the 32 B form READ BACK to host. EMPTY unless Both
    /// (the fork gate's lane tier reads it; the product never wants it).
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
    mode: ChainMode,
) -> ChainStream {
    let item_count = items.len();
    let n: usize = bytes.len();
    let _chain = tracing::info_span!("chain", n, items = item_count, ?mode).entered();
    let t_chain0 = std::time::Instant::now();
    let sp_prep = tracing::info_span!("chain.prep").entered();

    let t_tables = std::time::Instant::now();
    drop(sp_prep);
    let sp_tables = tracing::info_span!("chain.tables").entered();

    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());
    let wants_instances = matches!(mode, ChainMode::Instances | ChainMode::Both);
    let host_inputs = prepare_chain_inputs(bytes, items, &trie, wants_instances);

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
        wants_instances,
    );

    let chunk_env: Option<usize> = std::env::var("GLYPH_RECORD_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok());
    let chunk_recs_cap = chunk_env.unwrap_or(16_777_216);

    let t_dispatch = std::time::Instant::now();
    span_upload.record("live_bytes", live_bytes);
    drop(sp_upload);
    drop(span_upload);
    let sp_dispatch = tracing::info_span!("chain.dispatch").entered();

    let mut prof = ChainProfiler::new();

    launch_block1(&client, n, &host_inputs, &buf, &mut prof);

    launch_block2_totals(
        &client,
        n,
        item_count,
        &host_inputs,
        &buf,
        &mut prof,
    );

    launch_block2_geometry(
        &client,
        n,
        item_count,
        &host_inputs,
        &buf,
        &mut prof,
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

    let (ltot, stot, total_records, total_slots, rec_base, slot_base) = {
        let skip_totals_sync = std::env::var_os("GLYPH_TOTALS_READBACK").is_none();
        if skip_totals_sync {
            (
                host_inputs.ltot.clone(),
                host_inputs.stot.clone(),
                host_inputs.total_records,
                host_inputs.total_slots,
                host_inputs.rec_base.clone(),
                host_inputs.slot_base.clone(),
            )
        } else {
            let t_tsync = std::time::Instant::now();
            let tb = client
                .read_one(buf.h_totals.as_ref().unwrap().clone())
                .expect("totals readback");
            let tv: &[u32] = bytemuck::cast_slice(&tb);
            let mut ltot = Vec::with_capacity(item_count);
            let mut stot = Vec::with_capacity(item_count);
            for i in 0..item_count {
                ltot.push(tv[i * 2]);
                stot.push(tv[i * 2 + 1]);
            }
            prof.record_sync("sync:totals_readback", t_tsync.elapsed());

            let mut rec_base = vec![0u32; item_count];
            let mut slot_base = vec![0u32; item_count];
            let mut total_records = 0u32;
            let mut total_slots = 0u32;
            for i in 0..item_count {
                rec_base[i] = total_records;
                slot_base[i] = total_slots;
                total_records += ltot[i];
                total_slots += stot[i];
            }
            (ltot, stot, total_records, total_slots, rec_base, slot_base)
        }
    };

    if wants_instances {
        let mut expect = 0u32;
        for (it, &is_pr) in inputs.is_per_record.iter().enumerate() {
            if is_pr != 0 {
                assert_eq!(
                    inputs.color_base[it], expect,
                    "paint/record misalignment at item {it}: colors start at {} but the records say {expect}",
                    inputs.color_base[it],
                );
                expect += ltot[it];
            }
        }
        assert_eq!(
            inputs.per_record_colors.len(),
            expect as usize,
            "paint is indexed by record: {} colors for {} records",
            inputs.per_record_colors.len(),
            expect,
        );
    }
    drop(sp_totals);
    span_tail.record("total_records", total_records);
    span_tail.record("total_slots", total_slots);

    // The Ladder: drop all intermediate lanes whose last reader ran during geometry
    {
        let _sp_ladder = tracing::info_span!("tail.ladder").entered();
        buf.release_pre_survivor();
    }

    let h_base = client.create_from_slice(bytemuck::cast_slice(&rec_base));
    let mut recs_all: Vec<u32> = Vec::new();
    if matches!(mode, ChainMode::Records | ChainMode::Both) {
        recs_all = emit_records_chunked(
            &client,
            n,
            host_inputs.n_words,
            total_records,
            chunk_recs_cap,
            host_inputs.ir.len(),
            item_count,
            buf.h_fl.clone(),
            buf.h_wc.clone(),
            buf.h_ir.clone(),
            h_base,
            buf.h_lm.clone(),
            buf.h_lc.take().unwrap(),
            buf.h_sm.clone(),
            buf.h_hgt.clone(),
            buf.h_gi.clone(),
            &mut prof,
        );
    } else {
        buf.h_lc = None;
    }

    let mut placements: Vec<crate::layout::ItemPlacement> = Vec::new();
    let mut slots_all: Vec<u32> = Vec::new();
    let mut tint_store = crate::layout::TintStore::Host(Vec::new());
    let mut slot_device: Option<SlotDevice> = None;
    if wants_instances {
        assert!(
            (total_slots as u64) * 32 <= device_ref.max_buffer_size,
            "the endpoint needs one {} B slot buffer — over this device's \
             max_buffer_size ({}); chunked slot buffers are the named \
             follow-up",
            (total_slots as u64) * 32,
            device_ref.max_buffer_size,
        );

        placements = decode_placements(
            &client,
            buf.h_ext.clone(),
            item_count,
            &slot_base,
            &stot,
            &ltot,
        );

        let c = if let Some(ref h_ctotal) = buf.h_ctotal {
            let tb = client.read_one(h_ctotal.clone()).expect("candidate count");
            bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize
        } else {
            0
        };

        let needs_tint = matches!(mode, ChainMode::Both)
            || inputs.is_per_record.iter().any(|&x| x != 0);
        let (h_slots, h_tint) = scatter_slots_direct(
            &client,
            n,
            host_inputs.n_words,
            total_slots,
            host_inputs.ir.len(),
            item_count,
            inputs,
            &buf,
            needs_tint,
            &mut prof,
        );

        let sp_tint = tracing::info_span!("tail.tint").entered();
        tint_store = if needs_tint {
            read_tint_store(&client, device_ref, h_tint.clone(), total_slots, mode)
        } else {
            crate::layout::TintStore::Host(Vec::new())
        };
        drop(sp_tint);

        if matches!(mode, ChainMode::Both) {
            let sb = client.read_one(h_slots.clone()).expect("read slot scatter");
            slots_all = bytemuck::cast_slice::<u8, u32>(&sb)[..total_slots as usize * 8].to_vec();
        }

        slot_device = package_slot_device(&client, h_slots, total_slots);
        buf.release_survivor_scan();

        prof.print_summary();

        return ChainStream {
            records: recs_all,
            rec_base,
            total_records,
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
        };
    }

    let c = if let Some(ref h_ctotal) = buf.h_ctotal {
        let tb = client.read_one(h_ctotal.clone()).expect("candidate count");
        bytemuck::cast_slice::<u8, u32>(&tb)[0] as usize
    } else {
        0
    };
    buf.release_survivor_scan();

    prof.print_summary();

    ChainStream {
        records: recs_all,
        rec_base,
        total_records,
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
