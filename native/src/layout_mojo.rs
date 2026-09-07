//! layout_mojo.rs — the Mojo backend behind the layout seam.
//!
//! Adapter, not algorithm: it owns an `Engine` handle (the C ABI wrapper in
//! `engine.rs`), decides how to hand a corpus to it, and hands the records it
//! gets back to the seam's one compaction. The fold itself is in Mojo.
//!
//! THE BATCH/PER-ITEM CHOICE LIVES HERE, and that is the point of the module.
//! It used to be a parameter of `load_repo` — `batch: bool` threaded from the
//! CLI down through the loader — which made an FFI strategy look like a
//! property of loading a repository. It is not: it is one backend's answer to
//! "how many times do I cross into Mojo?", invisible above the seam, and the
//! Rust backend will have no equivalent question. `--repo-verify` still picks
//! both, because two strategies that must agree bit-for-bit is exactly the
//! shape `--repo-verify` was built to check — and with the Rust backend the same
//! machinery diffs Mojo against Rust instead, with nothing new written.
//!
//! WHAT ITEM 3 OF THE PLAN CHANGES HERE. `layout_validated_items` currently
//! ends with `engine.read_back()` — the 32 B-per-record readback the seam
//! exists to delete — followed by a host-side `compact_records_into`. The
//! device path replaces BOTH with a compaction kernel writing the arena
//! directly and a bounds kernel filling the extents, and
//! `layout_validated_items_recording` keeps the readback for the gates that
//! ask for it. The signatures do not move.
//!
//! It replaces both because the measurement says the halves are not worth
//! separating: 2026-09-07, the readback is 9-22% of backend time and
//! compaction 14-41%, and killing the copy alone (a borrowing accessor in
//! place of `vec![default; n]` + `copy_slots`) buys only the half that dies
//! anyway when compaction moves. `BackendPhases` below is what measured it.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::layout::{
    compact_records_into, GlyphArena, GlyphRecord, ItemPlacement, LayoutError, LayoutGlyphs,
    LayoutItem, VerifyLayout,
};

/// How many times one corpus crosses into Mojo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// One `glyph_engine_load_items` call over a concatenated blob. Item
    /// boundaries are prefix sums — contiguous and ascending, which is the
    /// pipeline's documented requirement.
    Batched,
    /// One `glyph_engine_load_item` call per item. Older, slower, and kept
    /// because two independent routes to the same bits is a gate.
    PerItem,
}

/// Where a load's time goes inside the backend, accumulated across items.
///
/// This exists because the plan has been reasoning from ONE number. The
/// 1.438 s it quotes for staging came from `out/g-windowed-smoke.log`
/// (2026-08-31, pre-seam, per-item route) and brackets the fold, the readback
/// and compaction together — three costs with three different fixes. Splitting
/// them is what decides whether the readback is worth attacking by handing out
/// a pointer, or only by moving compaction to the device.
///
/// The sum of these four is the whole of `backend` in the phases line, so a
/// gap between them is itself a finding.
#[derive(Clone, Copy, Debug, Default)]
pub struct BackendPhases {
    /// The Mojo fold, across the FFI: `load_items` or `load_item`.
    pub fold: Duration,
    /// Host allocation + zero-fill of the readback buffer. See
    /// [`crate::engine::Engine::read_back`] — lazily backed, so this is a floor.
    pub readback_alloc: Duration,
    /// The FFI memcpy of the wire stream.
    pub readback_copy: Duration,
    /// `compact_records_into`: drop blanks, repack 32 -> 48 B, reduce extents.
    pub compact: Duration,
}

impl BackendPhases {
    /// What the readback costs in total — the number the plan wants, and the
    /// only one of the two halves that is trustworthy alone.
    pub fn readback(&self) -> Duration {
        self.readback_alloc + self.readback_copy
    }
}

pub struct MojoLayout {
    engine: Engine,
    strategy: Strategy,
    phases: BackendPhases,
}

impl MojoLayout {
    pub fn new(strategy: Strategy) -> Self {
        Self { engine: Engine::new(), strategy, phases: BackendPhases::default() }
    }

    /// Where the last load's time went. Accumulates across calls; a caller
    /// timing one load owns a fresh backend, which every caller today does.
    pub fn phases(&self) -> BackendPhases {
        self.phases
    }

    /// The one implementation both trait methods delegate to. `records_out`,
    /// when present, accumulates the full wire stream — which the per-item
    /// route cannot otherwise provide (the handle holds only the last item's
    /// records) and a device route could not provide for free at all. Asking
    /// for it is asking for work, so it is an explicit parameter rather than
    /// something that happens to be lying around.
    fn run(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
        mut records_out: Option<&mut Vec<GlyphRecord>>,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        let mut placements = Vec::with_capacity(items.len());
        match self.strategy {
            Strategy::Batched => {
                let total_bytes = items.iter().map(|i| i.bytes.len()).sum();
                let mut blob = Vec::with_capacity(total_bytes);
                let mut descs = Vec::with_capacity(items.len());
                for item in items {
                    descs.push((blob.len() as u64, item.bytes.len() as u64, item.params));
                    blob.extend_from_slice(item.bytes);
                }
                let t = Instant::now();
                let counts = self.engine.load_items(&blob, &descs)?;
                self.phases.fold += t.elapsed();
                let back = self.engine.read_back();
                self.phases.readback_alloc += back.alloc;
                self.phases.readback_copy += back.copy;
                let all = back.records;
                assert_eq!(
                    counts.iter().sum::<u64>() as usize,
                    all.len(),
                    "batch per-item counts do not sum to the record count",
                );
                let t = Instant::now();
                let mut record_base = 0usize;
                for (index, item) in items.iter().enumerate() {
                    let n = counts[index] as usize;
                    let records = &all[record_base..record_base + n];
                    placements.push(compact_records_into(
                        records,
                        item.paint,
                        item.group_id,
                        arena,
                    ));
                    record_base += n;
                }
                self.phases.compact += t.elapsed();
                if let Some(sink) = records_out.as_deref_mut() {
                    *sink = all;
                }
            }
            Strategy::PerItem => {
                for item in items {
                    let t = Instant::now();
                    self.engine.load_item(item.bytes, &item.params)?;
                    self.phases.fold += t.elapsed();
                    let back = self.engine.read_back();
                    self.phases.readback_alloc += back.alloc;
                    self.phases.readback_copy += back.copy;
                    let records = back.records;
                    let t = Instant::now();
                    placements.push(compact_records_into(
                        &records,
                        item.paint,
                        item.group_id,
                        arena,
                    ));
                    self.phases.compact += t.elapsed();
                    if let Some(sink) = records_out.as_deref_mut() {
                        sink.extend_from_slice(&records);
                    }
                }
            }
        }
        Ok(placements)
    }
}

impl LayoutGlyphs for MojoLayout {
    fn name(&self) -> &'static str {
        match self.strategy {
            Strategy::Batched => "mojo-cpu/batched",
            Strategy::PerItem => "mojo-cpu/per-item",
        }
    }

    fn load_trie_file(&mut self, path: &Path) -> Result<(), LayoutError> {
        self.engine.load_trie_file(path)
    }

    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        self.run(items, arena, None)
    }
}

impl VerifyLayout for MojoLayout {
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
        let mut records = Vec::new();
        let placements = self.run(items, arena, Some(&mut records))?;
        Ok((placements, records))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ItemParams, Paint};

    fn trie() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin")
    }

    /// Three items with different shapes: an ordinary file, one whose last
    /// line has no terminator, and one that is nothing but blanks (so the
    /// compaction has something to drop and the page extent has a blank to be
    /// widened by).
    const SAMPLES: [&[u8]; 3] = [
        b"fn main() {\n    let x = 1;\n}\n",
        b"no trailing newline",
        b"   \t   \n\n   \n",
    ];

    fn sample_items(colors: &[Vec<u32>]) -> Vec<LayoutItem<'_>> {
        SAMPLES
            .iter()
            .enumerate()
            .map(|(i, bytes)| LayoutItem {
                bytes: *bytes,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i as u32,
                paint: Paint::PerRecord(&colors[i]),
            })
            .collect()
    }

    fn sample_colors() -> Vec<Vec<u32>> {
        SAMPLES.iter().map(|b| crate::text::colorize_leaders(b)).collect()
    }

    /// `--repo-verify` in miniature, and the acceptance test for putting the
    /// strategy choice below the seam: the two routes must agree on the ARENA
    /// and the PLACEMENTS, not merely on the records. Today's gate compares
    /// records only, which cannot see a compaction or paint difference at all.
    #[test]
    fn the_two_strategies_agree_on_instances_and_placements() {
        let colors = sample_colors();
        let items = sample_items(&colors);

        let mut batched = MojoLayout::new(Strategy::Batched);
        batched.load_trie_file(&trie()).expect("trie");
        let mut batched_arena = GlyphArena::new();
        let batched_places = batched.layout_items(&items, &mut batched_arena).expect("batched");

        let mut per_item = MojoLayout::new(Strategy::PerItem);
        per_item.load_trie_file(&trie()).expect("trie");
        let mut per_item_arena = GlyphArena::new();
        let per_item_places = per_item.layout_items(&items, &mut per_item_arena).expect("per-item");

        assert_eq!(batched_places.len(), per_item_places.len());
        for (index, (a, b)) in batched_places.iter().zip(per_item_places.iter()).enumerate() {
            // bit_eq, not PartialEq: see `ItemPlacement::bit_eq`.
            assert!(a.bit_eq(b), "item {index} placement differs:\n  {a:?}\n  {b:?}");
        }
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(batched_arena.instances()),
            bytemuck::cast_slice::<_, u8>(per_item_arena.instances()),
            "arenas differ between strategies",
        );
        // ANTI-VACUITY: a comparison of two empty arenas passes for free.
        assert!(!batched_arena.is_empty(), "the samples must produce instances");
        assert!(
            batched_places.iter().any(|p| p.record_count > p.slot_count),
            "the samples must contain blanks, or the compaction is untested here",
        );
    }

    /// The seam-level twin of `engine::tests::a_nan_pitch_cannot_reach_the_
    /// engine_through_load_item`: proves the trait's PROVIDED validation runs,
    /// which is what makes forgetting impossible for every future backend.
    ///
    /// IT RUNS ON BOTH STRATEGIES, AND THE PER-ITEM ONE IS THE LOAD-BEARING
    /// HALF. `Engine` validates too, so deleting the trait's loop would leave
    /// the batched route still refusing — and still naming item 1, because
    /// `load_items` validates each descriptor by its own index. The per-item
    /// route calls `load_item`, which only ever knows "item 0". So under
    /// `PerItem`, "item 1" can only have come from the seam, and that is the
    /// sentence that makes this test able to fail.
    #[test]
    fn a_nan_pitch_cannot_reach_a_backend_through_layout_items() {
        for strategy in [Strategy::PerItem, Strategy::Batched] {
            let mut backend = MojoLayout::new(strategy);
            backend.load_trie_file(&trie()).expect("trie");
            let colors = vec![0u32; 6];
            let items = [
                LayoutItem {
                    bytes: b"ok\n",
                    params: ItemParams { line_height: 1.0, ..Default::default() },
                    group_id: 0,
                    paint: Paint::Flat(0),
                },
                LayoutItem {
                    bytes: b"bad\n",
                    params: ItemParams { line_height: f64::NAN, ..Default::default() },
                    group_id: 1,
                    paint: Paint::PerRecord(&colors),
                },
            ];
            let mut arena = GlyphArena::new();
            let e = backend
                .layout_items(&items, &mut arena)
                .expect_err("a NaN pitch must be refused at the seam");
            assert!(
                e.what.contains("item 1"),
                "{strategy:?}: must name the offending item: {}",
                e.what,
            );
            assert!(
                arena.is_empty(),
                "{strategy:?}: nothing may be staged from a refused batch",
            );
        }
    }

    #[test]
    fn the_backend_names_itself_by_strategy() {
        // The verify report prints these; two strategies that report the same
        // name would make a mismatch unattributable.
        assert_ne!(
            MojoLayout::new(Strategy::Batched).name(),
            MojoLayout::new(Strategy::PerItem).name(),
        );
    }
}
