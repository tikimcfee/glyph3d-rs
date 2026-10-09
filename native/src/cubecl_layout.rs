//! The CubeCL layout backend — the endpoint (note 23, E2b).
//!
//! Bytes and params through the device chain — the SAME `run_repo_chain`
//! that `--cubecl-repo-check` diffs bit-exactly against the CPU layout's
//! records and slot stream (a standing instrument, not currently a gate) — and
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

/// Precomputed data produced by a background prefetch thread during window/GPU initialization.
pub(crate) struct PrefetchedCubeclData {
    pub(crate) bytes: Vec<u8>,
    pub(crate) items: Vec<crate::fold::Item>,
    pub(crate) instance_inputs: crate::cubecl_chain::InstanceInputs,
    pub(crate) host_inputs: crate::cubecl_chain::prep::ChainHostInputs<'static>,
}

/// The device-chain backend. Construct it like any other;
/// the atlas tables are loaded per run inside the chain.
#[derive(Default)]
pub struct CubeclLayout {
    /// The renderer's shared device when the caller threaded one (rung 5a's
    /// device merge); `None` lets the chain construct its own.
    device: Option<crate::cubecl_chain::SharedDevice>,
    phases: CubeclPhases,
    pub field_mode: glyph_field::GlyphFieldMode,
    pub(crate) prefetched_inputs: Option<Box<PrefetchedCubeclData>>,
}

impl CubeclLayout {
    pub fn new(field_mode: glyph_field::GlyphFieldMode) -> Self {
        Self {
            device: None,
            phases: CubeclPhases::default(),
            field_mode,
            prefetched_inputs: None,
        }
    }

    /// Share the caller's (the renderer's) GPU device — rung 5a's merge: the
    /// chain computes on the same device/queue that draws, and the second
    /// device of the rung-4 compromise never exists.
    pub(crate) fn with_device(device: crate::cubecl_chain::SharedDevice, field_mode: glyph_field::GlyphFieldMode) -> Self {
        Self {
            device: Some(device),
            phases: CubeclPhases::default(),
            field_mode,
            prefetched_inputs: None,
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
    let total_bytes: usize = items.iter().map(|it| it.bytes.len()).sum();
    let n_words = total_bytes.div_ceil(4);
    let mut bytes = Vec::with_capacity(n_words * 4);
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
    bytes.resize(n_words * 4, 0x80);
    let inputs = crate::cubecl_chain::InstanceInputs {
        per_record_colors,
        color_base,
        is_per_record,
        flat_colors,
        groups,
    };
    (bytes, fis, inputs)
}

/// Pre-marshals and pre-computes chain tables from the filesystem walk in parallel with GPU init.
pub(crate) fn marshal_from_walk(
    walk: &crate::repo::WalkResult,
    file_params: &[crate::layout::ItemParams],
    color_mode: crate::repo::ColorMode,
) -> PrefetchedCubeclData {
    let total_bytes: usize = walk.total_bytes;
    let n_words = total_bytes.div_ceil(4);
    let mut bytes = Vec::with_capacity(n_words * 4);
    let mut fis = Vec::with_capacity(walk.files.len());
    let mut per_record_colors: Vec<u32> = Vec::new();
    let mut color_base = vec![0u32; walk.files.len()];
    let mut is_per_record = vec![0u32; walk.files.len()];
    let mut flat_colors = vec![0u32; walk.files.len()];
    let mut groups = Vec::with_capacity(walk.files.len());
    let mut off = 0usize;

    use rayon::prelude::*;
    let syntax_colors: Option<Vec<Vec<u32>>> = if color_mode == crate::repo::ColorMode::Syntax {
        Some(
            walk.files
                .par_iter()
                .map(|f| crate::text::colorize_leaders(&f.bytes))
                .collect(),
        )
    } else {
        None
    };

    for (index, file) in walk.files.iter().enumerate() {
        bytes.extend_from_slice(&file.bytes);
        let p = &file_params[index];
        fis.push(crate::fold::Item {
            byte_start: off as i64,
            byte_count: file.bytes.len() as i64,
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

        color_base[index] = per_record_colors.len() as u32;
        if let Some(ref colors) = syntax_colors {
            is_per_record[index] = 1;
            per_record_colors.extend_from_slice(&colors[index]);
        } else {
            flat_colors[index] = crate::layout::DEFAULT_COLOR_PACKED;
        }
        groups.push(index as u32);
        off += file.bytes.len();
    }
    bytes.resize(n_words * 4, 0x80);
    let inputs = crate::cubecl_chain::InstanceInputs {
        per_record_colors,
        color_base,
        is_per_record,
        flat_colors,
        groups,
    };

    let trie = crate::atlas::default_trie_ref();
    let host_inputs = crate::cubecl_chain::prep::prepare_chain_inputs(
        &bytes,
        &fis,
        trie,
        /* wants_instances = */ false,
        Some(&inputs),
    );

    PrefetchedCubeclData {
        bytes,
        items: fis,
        instance_inputs: inputs,
        host_inputs,
    }
}

impl LayoutGlyphs for CubeclLayout {
    fn name(&self) -> &'static str {
        "cubecl"
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
        let (bytes, fis, inputs, precomputed_host_inputs) = match self.prefetched_inputs.take() {
            Some(data) => (data.bytes, data.items, data.instance_inputs, Some(data.host_inputs)),
            None => {
                let (b, f, i) = marshal(items);
                (b, f, i, None)
            }
        };
        let marshal_dur = t_marshal.elapsed();
        let stream = crate::cubecl_chain::run_repo_chain(
            self.device.as_ref(),
            bytes,
            &fis,
            &inputs,
            false,
            self.field_mode,
            precomputed_host_inputs,
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
                format: self.field_mode,
                derived: if self.field_mode == glyph_field::GlyphFieldMode::Derived {
                    Some(crate::layout::DerivedDeviceSlots { mapped_base: None })
                } else {
                    None
                },
                emoji_tint_pairs: Vec::new(),
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
    /// The verify path: reconstructs 48 B GlyphInstance on host for diff_backends.
    /// The 32 B slot stream carries the render fields from GPU, and records
    /// are rederived on CPU to provide row/col.
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
            bytes,
            &fis,
            &inputs,
            true,
            self.field_mode,
            None,
        );
        assert!(
            arena.is_empty(),
            "the instance tail writes from slot 0 — a pre-filled arena would need the rebase the direct path carries"
        );
        let t_convert = Instant::now();
        let trie = crate::default_trie();
        let mut all_records = Vec::new();
        for item in items {
            all_records.extend(crate::layout_hyper::rederive_item_records(item.bytes, &item.params, &trie));
        }
        let convert = t_convert.elapsed();
        let mut insts: Vec<GlyphInstance> = Vec::with_capacity(stream.total_slots as usize);
        let sw = &stream.slots;
        let mut slot_index = 0usize;
        for record in &all_records {
            if record.counts[0] == 0 {
                continue;
            }
            let slot_word_offset = slot_index * 8;
            insts.push(GlyphInstance {
                pos: [
                    f32::from_bits(sw[slot_word_offset]),
                    f32::from_bits(sw[slot_word_offset + 1]),
                    f32::from_bits(sw[slot_word_offset + 2]),
                ],
                glyph_id: sw[slot_word_offset + 3],
                row: record.counts[1],
                col: record.counts[2],
                color: sw[slot_word_offset + 4],
                group_id: sw[slot_word_offset + 5],
                advance: f32::from_bits(sw[slot_word_offset + 6]),
                height: f32::from_bits(sw[slot_word_offset + 7]),
                flags: 0,
                _pad: 0,
            });
            slot_index += 1;
        }
        assert_eq!(
            slot_index,
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

