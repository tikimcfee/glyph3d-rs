//! The CubeCL layout backend — the endpoint (note 23, E2b).
//!
//! Bytes and params through the device chain — the SAME `run_repo_chain`
//! the cubecl-fork gate fences bit-exactly against the Mojo engine — and
//! the tail is the scatter's: ONE pass writes the 32 B slots directly
//! into the buffer the renderer binds, the placements decode from its
//! extent lanes, and the tint stream (glyph_id, color per slot) is the
//! only slot-derived readback. There is no hop, no pack-window loop, no
//! host instance materialization anywhere on the product path; the
//! records/48 B forms exist only under `--repo-verify` (reconstructed
//! from the two fenced streams — the records carry row/col, the slots
//! the render fields).
//!
//! Compromise ledger, all resolved:
//! - ~~The records cross to the host (the chunked readback)~~ — 5b.
//! - ~~The packed slots cross to the host (the readback hop)~~ — E2b.
//! - ~~The slots copy on device into a mapped arena (the copy hop)~~ —
//!   E2b: the scatter writes the renderer-bound buffer directly.
//! - ~~`run_repo_chain` constructs its own GPU device~~ — 5a
//!   (`SharedDevice` through `with_device`; `new()` keeps the fallback
//!   for callers with none).

use crate::glyph_scene::GlyphInstance;
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
pub struct CubeclPhases {
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
    let _sp = tracing::info_span!("cubecl.marshal", items = items.len()).entered();
    let mut bytes = Vec::new();
    let mut fis = Vec::with_capacity(items.len());
    let mut per_record_colors: Vec<u32> = Vec::new();
    let mut color_base = vec![0u32; items.len()];
    let mut is_per_record = vec![0u32; items.len()];
    let mut flat_colors = vec![0u32; items.len()];
    let mut groups = Vec::with_capacity(items.len());
    let mut off = 0usize;
    use rayon::prelude::*;
    let syntax_colors: Option<Vec<Option<Vec<u32>>>> = if items
        .iter()
        .any(|it| matches!(it.paint, Paint::SyntaxHeuristic))
    {
        Some(
            items
                .par_iter()
                .map(|it| {
                    if matches!(it.paint, Paint::SyntaxHeuristic) {
                        Some(crate::text::colorize_leaders(it.bytes))
                    } else {
                        None
                    }
                })
                .collect(),
        )
    } else {
        None
    };

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
            Paint::SyntaxHeuristic => {
                if let Some(Some(ref colors)) = syntax_colors.as_ref().map(|v| &v[index]) {
                    is_per_record[index] = 1;
                    per_record_colors.extend_from_slice(colors);
                } else {
                    flat_colors[index] = crate::layout::DEFAULT_COLOR_PACKED;
                }
            }
            Paint::Flat(rgba) => flat_colors[index] = rgba,
            Paint::ByteSpans(_) => flat_colors[index] = crate::layout::DEFAULT_COLOR_PACKED,
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

    /// THE PRODUCT PATH since E2b (note 23): the endpoint. One scatter
    /// pass writes the 32 B slots into the buffer the renderer binds —
    /// `run_repo_chain` hands the device buffer across and the arena
    /// becomes it. No host copy, no hop, no pack windows.
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
        );
        assert!(
            arena.is_empty(),
            "the instance tail writes from slot 0 — a pre-filled arena would need the rebase the direct path carries"
        );
        if let Some(sd) = stream.slot_device {
            let len = sd.chunk.slots as usize;
            *arena = GlyphArena::from_device(crate::layout::DeviceSlots {
                chunk_slots: len,
                len,
                tint: stream.tint,
                chunks: vec![sd.chunk],
                keep_alive: vec![sd.keep_alive],
                mapped_slots: None,
                file_tints: Vec::new(),
                file_blocks: Vec::new(),
            });
        }
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
    /// The verify path: BOTH tails. The arena gets the endpoint's device
    /// slots exactly as the product path, and the 48 B wire form is
    /// RECONSTRUCTED on host so `diff_backends` still sees instances:
    /// the records stream carries row/col (and the blank lanes the slot
    /// stream drops), the slot stream carries the render fields — both
    /// bit-fenced against the engine, so their zip is the fenced instance.
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
        );
        assert!(
            arena.is_empty(),
            "the instance tail writes from slot 0 — a pre-filled arena would need the rebase the direct path carries"
        );
        // The 48 B host arena the seam's diff expects, reconstructed from
        // the two fenced streams: survivors are the records with gi != 0,
        // in order (the survivor filter's own definition), zipped with the
        // slot stream. The device buffer is NOT taken here — verify paths
        // diff arenas host-side.
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
        let mut insts: Vec<GlyphInstance> = Vec::with_capacity(stream.total_slots as usize);
        let sw = &stream.slots;
        let mut s = 0usize;
        for r in &all_records {
            if r.counts[0] == 0 {
                continue;
            }
            let b = s * 8;
            insts.push(GlyphInstance {
                pos: [
                    f32::from_bits(sw[b]),
                    f32::from_bits(sw[b + 1]),
                    f32::from_bits(sw[b + 2]),
                ],
                glyph_id: sw[b + 3],
                row: r.counts[1],
                col: r.counts[2],
                color: sw[b + 4],
                group_id: sw[b + 5],
                advance: f32::from_bits(sw[b + 6]),
                height: f32::from_bits(sw[b + 7]),
                flags: 0,
                _pad: 0,
            });
            s += 1;
        }
        assert_eq!(
            s,
            stream.total_slots as usize,
            "record/slot survivor zip drifted — the streams' own tiers should have caught it first"
        );
        *arena = GlyphArena::from_vec(insts);
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

