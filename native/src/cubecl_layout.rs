//! The CubeCL layout backend — rung 4's flip, rung 5b's instance tail.
//!
//! Bytes and params through the device chain — the SAME `run_repo_chain`
//! the cubecl-fork gate fences bit-exactly against the Mojo engine — and
//! since rung 5b the TAIL is the pack kernel's: instances, placements,
//! per-item totals all come back from the device (the survivor pass), and
//! the arena hand-off is one `uninit_tail` + memcpy + `commit`. The
//! per-item GlyphRecord materialization and `compact_records_into` are
//! GONE from the product path; they run only under `--repo-verify`, where
//! the wire stream is the point. Identical bytes in, identical staging
//! out: the fork gate's instance and placement tiers hold the pack kernel
//! to the engine-batched arena, word for word.
//!
//! Compromise ledger:
//! - ~~The records cross to the host (the chunked readback)~~ — HALF
//!   resolved 5b: no RECORD crosses, but the packed slots still do (the
//!   readback hop). Rung 5c's copy hop moves even that on device.
//! - ~~`run_repo_chain` constructs its own GPU device~~ — resolved 5a
//!   (`SharedDevice` through `with_device`; `new()` keeps the fallback
//!   for callers with none).

use crate::layout::{
    GlyphArena, GlyphRecord, LayoutError, LayoutGlyphs, LayoutItem, Paint, VerifyLayout,
    ItemPlacement,
};
use std::path::Path;
use std::time::{Duration, Instant};

/// The product path's wall-clock decomposition — rung 5's yardstick,
/// reported through `LoadStats`. The chain's five spans come from
/// `run_repo_chain`; the three host spans are this backend's own tail:
/// `emit_readback` is the chain's TAIL span (totals readback + emission
/// windows + the instance readback hop — 5c's device copy replaces the
/// readback), and `convert`/`compact` are now VERIFY-ONLY costs (the
/// wire-stream materialization under `--repo-verify`; the product path
/// reads them as ~0, which is the yardstick claim).
#[derive(Clone, Copy, Default)]
pub(crate) struct CubeclPhases {
    /// Bytes concat + fold::Item building + the paint tables.
    pub marshal: Duration,
    /// run_repo_chain's spans (the device totals' prefix sums / tables /
    /// device init / pack+upload / launches).
    pub chain: crate::cubecl_chain::ChainPhases,
    /// The chain's tail span: emission windows + readbacks + the arena
    /// hand-off memcpy.
    pub emit_readback: Duration,
    /// Per-item GlyphRecord building + the all_records accumulation —
    /// verify only.
    pub convert: Duration,
    /// compact_records_into across all items — verify only.
    pub compact: Duration,
}

/// The device-chain backend. Construct and `load_trie_file` like any other;
/// the atlas tables are loaded per run inside the chain.
#[derive(Default)]
pub struct CubeclLayout {
    /// The renderer's shared device when the caller threaded one (rung 5a's
    /// device merge); `None` lets the chain construct its own.
    device: Option<crate::cubecl_chain::SharedDevice>,
    phases: CubeclPhases,
}

impl CubeclLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Share the caller's (the renderer's) GPU device — rung 5a's merge: the
    /// chain computes on the same device/queue that draws, and the second
    /// device of the rung-4 compromise never exists.
    pub(crate) fn with_device(device: crate::cubecl_chain::SharedDevice) -> Self {
        Self {
            device: Some(device),
            ..Default::default()
        }
    }

    /// The last run's decomposition; zeroed before the first layout.
    pub(crate) fn phases(&self) -> CubeclPhases {
        self.phases
    }
}

/// Marshal the seam's items into the chain's inputs: the byte concat, the
/// fold::Items, and the paint/group tables the pack kernel reads (the
/// Paint-at-compaction argument, moved to the upload — per-record colors
/// for PerRecord items at host-known lengths, a flat color otherwise).
fn marshal(
    items: &[LayoutItem<'_>],
) -> (
    Vec<u8>,
    Vec<crate::fold::Item>,
    crate::cubecl_chain::InstanceInputs,
) {
    let mut bytes = Vec::new();
    let mut fis = Vec::with_capacity(items.len());
    let mut per_record_colors: Vec<u32> = Vec::new();
    let mut color_base = vec![0u32; items.len()];
    let mut is_per_record = vec![0u32; items.len()];
    let mut flat_colors = vec![0u32; items.len()];
    let mut groups = Vec::with_capacity(items.len());
    let mut off = 0usize;
    for (index, item) in items.iter().enumerate() {
        bytes.extend_from_slice(item.bytes);
        let p = &item.params;
        fis.push(crate::fold::Item {
            byte_start: off as i64,
            byte_count: item.bytes.len() as i64,
            origin_x: p.origin_x,
            origin_y: p.origin_y,
            origin_z: p.origin_z,
            wrap_width: p.wrap_width as i64,
            wrap_mode: p.wrap_mode,
            cluster_mode: p.cluster_mode,
            z_step: p.z_step,
            line_height: p.line_height,
            has_page: p.has_page,
            page_rows: p.page_rows as i64,
            page_cols: p.page_cols as i64,
            scroll_rows: p.scroll_rows as i64,
            pages_wide: p.pages_wide as i64,
            page_gap_x: p.page_gap_x,
            band_stride_y: p.band_stride_y,
            depth_per_band: p.depth_per_band,
            depth_per_col: p.depth_per_col,
            page_line_height: p.page_line_height,
        });
        // Paint rides THROUGH the tail exactly as it rode through
        // compaction: the record ordinal that colors are indexed by is
        // destroyed by the blank drop, so the colors must travel in record
        // space and be applied where the ordinal still exists (inside the
        // pack kernel, at `rec_base[it] + wc[b]`).
        color_base[index] = per_record_colors.len() as u32;
        match item.paint {
            Paint::PerRecord(colors) => {
                is_per_record[index] = 1;
                per_record_colors.extend_from_slice(colors);
            }
            Paint::Flat(rgba) => flat_colors[index] = rgba,
        }
        groups.push(item.group_id);
        off += item.bytes.len();
    }
    let inputs = crate::cubecl_chain::InstanceInputs {
        per_record_colors,
        color_base,
        is_per_record,
        flat_colors,
        groups,
    };
    (bytes, fis, inputs)
}

/// The arena hand-off. Copy hop (`on_device`, rung 5c): the driver already
/// landed the windows in the mapped arena GPU-side — only the `commit`
/// remains, and no slot byte ever crossed to host. Readback hop otherwise:
/// mapped arena takes one memcpy into its tail; Vec arena TAKES the
/// readback's own allocation over (`GlyphArena::from_vec`) — zero copies,
/// the pages touched exactly once (the readback write; hence the driver's
/// `reserve_exact`). pub(crate): repo_check hands its own chain-side arena
/// off the same way, so the fence's instance tier compares arena against
/// arena whichever hop ran.
pub(crate) fn hand_off(words: Vec<u32>, slots: usize, on_device: bool, arena: &mut GlyphArena) {
    assert!(
        arena.is_empty(),
        "the instance tail writes from slot 0 — a pre-filled arena would need the rebase the direct path carries"
    );
    if on_device {
        assert!(
            arena.is_mapped(),
            "the device copy landed in the arena's buffers — an unmapped arena has none"
        );
        unsafe { arena.commit(slots) };
        return;
    }
    let mut words = words;
    if slots == 0 {
        return;
    }
    let byte_len = slots * std::mem::size_of::<crate::glyph_scene::GlyphInstance>();
    debug_assert_eq!(words.len() * 4, byte_len);
    if arena.is_mapped() {
        // One host write, split across chunk buffers where they meet.
        arena.write_bytes_at(0, bytemuck::cast_slice(&words[..slots * 12]));
        unsafe { arena.commit(slots) };
    } else {
        debug_assert!(words.capacity() >= words.len());
        let cap_words = words.capacity();
        let ptr = words.as_mut_ptr();
        std::mem::forget(words);
        // Same allocation, retyped 12 u32 → one 48 B slot. Sound because the
        // driver reserve_exact's slots*12 words upstream: capacity ≡ 0 mod 12
        // makes the dealloc layout match the alloc layout EXACTLY, and the
        // chunked windows fill every word (len is only ever slots).
        let inst: Vec<crate::glyph_scene::GlyphInstance> = unsafe {
            Vec::from_raw_parts(
                ptr as *mut crate::glyph_scene::GlyphInstance,
                slots,
                cap_words / 12,
            )
        };
        *arena = GlyphArena::from_vec(inst);
    }
}

impl LayoutGlyphs for CubeclLayout {
    fn name(&self) -> &'static str {
        "cubecl"
    }

    fn load_trie_file(&mut self, _path: &Path) -> Result<(), LayoutError> {
        // The chain reads the ATLAS tables (`TrieTable::load(atlas_dir())`)
        // inside `run_repo_chain`; the engine-trie path the seam hands
        // across is the Mojo backend's serialization. Accepting the call
        // keeps the trait uniform — the frames' loader does not care which
        // trie file a backend was nominally handed.
        Ok(())
    }

    /// THE PRODUCT PATH since rung 5b: the instance tail. No record is
    /// materialized on host; the packed slots move into the arena's tail
    /// in one memcpy and `commit` publishes them.
    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        let t_marshal = Instant::now();
        let (bytes, fis, inputs) = marshal(items);
        let marshal_dur = t_marshal.elapsed();
        let stream = crate::cubecl_chain::run_repo_chain(
            self.device.as_ref(),
            &bytes,
            &fis,
            &inputs,
            crate::cubecl_chain::ChainMode::Instances,
            arena.mapped_target(),
        );
        // The arena hand-off — see `hand_off`: the copy hop's commit, one
        // memcpy on the mapped readback path, an allocation hand-over on
        // the Vec path.
        hand_off(
            stream.instances,
            stream.total_slots as usize,
            stream.instances_on_device,
            arena,
        );
        self.phases = CubeclPhases {
            marshal: marshal_dur,
            chain: stream.phases,
            emit_readback: stream.readback_dur,
            convert: Duration::ZERO,
            compact: Duration::ZERO,
        };
        Ok(stream.placements)
    }
}

impl VerifyLayout for CubeclLayout {
    /// The verify path: BOTH tails. The instance/placement hand-off is the
    /// product's own, and the wire records are materialized on top so
    /// `diff_backends` still sees the 32 B stream — the readback and the
    /// GlyphRecord building are the price of the check, deliberately not
    /// of the product.
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
        let t_marshal = Instant::now();
        let (bytes, fis, inputs) = marshal(items);
        let marshal_dur = t_marshal.elapsed();
        let stream = crate::cubecl_chain::run_repo_chain(
            self.device.as_ref(),
            &bytes,
            &fis,
            &inputs,
            crate::cubecl_chain::ChainMode::Both,
            arena.mapped_target(),
        );
        hand_off(
            stream.instances,
            stream.total_slots as usize,
            stream.instances_on_device,
            arena,
        );
        // The records tier's materialization — the rung-4 convert loop,
        // now a verify-only cost.
        let mut convert = Duration::ZERO;
        let mut all_records =
            Vec::with_capacity(stream.total_records as usize);
        let words = &stream.records;
        for index in 0..items.len() {
            let base = stream.rec_base[index] as usize;
            let next = stream
                .rec_base
                .get(index + 1)
                .map(|&b| b as usize)
                .unwrap_or(stream.total_records as usize);
            let t = Instant::now();
            all_records.extend((base..next).map(|r| {
                let w = r * 8;
                GlyphRecord {
                    measures: [
                        f32::from_bits(words[w]),
                        f32::from_bits(words[w + 1]),
                        f32::from_bits(words[w + 2]),
                        f32::from_bits(words[w + 3]),
                        f32::from_bits(words[w + 4]),
                    ],
                    counts: [words[w + 5], words[w + 6], words[w + 7]],
                }
            }));
            convert += t.elapsed();
        }
        self.phases = CubeclPhases {
            marshal: marshal_dur,
            chain: stream.phases,
            emit_readback: stream.readback_dur,
            convert,
            compact: Duration::ZERO,
        };
        Ok((stream.placements, all_records))
    }
}

