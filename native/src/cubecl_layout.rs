//! The CubeCL layout backend — rung 4's flip.
//!
//! Bytes and params through the device chain — the SAME `run_repo_chain`
//! the cubecl-fork gate fences bit-exactly against the Mojo engine — with
//! the records compacted into the arena by the shared
//! `compact_records_into`. Identical records in, identical staging out:
//! extents, paint and drops are the engine path's own, which is what makes
//! byte-equal goldens an arithmetic claim rather than a hope.
//!
//! Two rung-4 compromises, both named:
//! - The records cross to the host (the chunked readback). Rung 5's direct
//!   bind consumes the device buffers in place and this readback dies.
//! - `run_repo_chain` constructs its own GPU device when handed no context
//!   (a second device alongside the renderer's). Also rung 5's business.

use crate::layout::{
    compact_records_into, GlyphArena, GlyphRecord, LayoutError, LayoutGlyphs, LayoutItem,
    ItemPlacement, VerifyLayout,
};
use std::path::Path;
use std::time::{Duration, Instant};

/// The product path's wall-clock decomposition — rung 5's yardstick,
/// reported through `LoadStats`. The chain's five spans come from
/// `run_repo_chain`; the three host spans are this backend's own tail
/// (the record-materialization cost the rung-4 desk note priced as one
/// lump). Wall clock, always on — a handful of `Instant::now()` calls is
/// not a load-time cost.
#[derive(Clone, Copy, Default)]
pub(crate) struct CubeclPhases {
    /// Bytes concat + fold::Item building.
    pub marshal: Duration,
    /// run_repo_chain's spans (leader scan / tables / device init /
    /// pack+upload / launches).
    pub chain: crate::cubecl_chain::ChainPhases,
    /// The chunked emit + record readback (== the stream's readback_dur).
    pub emit_readback: Duration,
    /// Per-item GlyphRecord building + the all_records accumulation.
    pub convert: Duration,
    /// compact_records_into across all items.
    pub compact: Duration,
}

/// The device-chain backend. Construct and `load_trie_file` like any other;
/// the atlas tables are loaded per run inside the chain.
#[derive(Default)]
pub struct CubeclLayout {
    phases: CubeclPhases,
}

impl CubeclLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last run's decomposition; zeroed before the first layout.
    pub(crate) fn phases(&self) -> CubeclPhases {
        self.phases
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

    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        let (placements, _) = self.layout_validated_items_recording(items, arena)?;
        Ok(placements)
    }
}

impl VerifyLayout for CubeclLayout {
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
        let t_marshal = Instant::now();
        let mut bytes = Vec::new();
        let mut fis = Vec::with_capacity(items.len());
        let mut off = 0usize;
        for item in items {
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
            off += item.bytes.len();
        }

        let t_chain = Instant::now();
        let stream = crate::cubecl_chain::run_repo_chain(None, &bytes, &fis);

        let mut convert = Duration::ZERO;
        let mut compact = Duration::ZERO;
        let mut placements = Vec::with_capacity(items.len());
        let mut all_records = Vec::new();
        let words = &stream.records;
        for (index, item) in items.iter().enumerate() {
            let base = stream.rec_base[index] as usize;
            let next = stream
                .rec_base
                .get(index + 1)
                .map(|&b| b as usize)
                .unwrap_or(stream.total_records as usize);
            let t = Instant::now();
            let records: Vec<GlyphRecord> = (base..next)
                .map(|r| {
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
                })
                .collect();
            all_records.extend_from_slice(&records);
            convert += t.elapsed();
            let t = Instant::now();
            placements.push(compact_records_into(
                &records,
                item.paint,
                item.group_id,
                arena,
            ));
            compact += t.elapsed();
        }
        self.phases = CubeclPhases {
            marshal: t_chain.duration_since(t_marshal),
            chain: stream.phases,
            emit_readback: stream.readback_dur,
            convert,
            compact,
        };
        Ok((placements, all_records))
    }
}
