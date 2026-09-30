//! layout_mojo.rs — the Mojo backend behind the layout seam.
//!
//! Adapter, not algorithm: it owns an `Engine` handle (the C ABI wrapper in
//! `engine.rs`) and decides how to hand a corpus to it. The fold itself is in
//! Mojo.
//!
//! THE STRATEGY CHOICE LIVES HERE, and that is the point of the module. It used
//! to be a parameter of `load_repo` — `batch: bool` threaded from the CLI down
//! through the loader — which made an FFI strategy look like a property of
//! loading a repository. It is not: it is one backend's answer to "how do I
//! cross into Mojo?", invisible above the seam, and the Rust backend will have
//! no equivalent question. It stopped being a bool when a third strategy landed;
//! `Strategy` below is the enumeration and the authority.
//!
//! `--repo-verify` picks a counterpart and diffs bit-for-bit, which is the shape
//! that check was built for — and with the Rust backend the same machinery diffs
//! Mojo against Rust with nothing new written.
//!
//! WHAT ITEM 3 OF THE PLAN DID HERE, and it is done on the CPU.
//! `Strategy::Direct` has the engine write render instances straight into the
//! caller's arena: no wire record on either side of the FFI, no host
//! compaction, one pass where there were three. The record strategies remain as
//! the verification form, because `VerifyLayout` needs a stream and the direct
//! path has none — it REFUSES that request rather than returning an empty Vec a
//! gate would read as agreement.
//!
//! It also folds in CHUNKS through one reused set of lane arrays, so the
//! per-byte fold lanes — ~40 B per source byte, and dead the instant the write
//! has read them — cost the chunk rather than the corpus. That is the memory
//! half of the same change and it is where the large-corpus win comes from.
//!
//! It took all three copies at once rather than the middle one, because the
//! measurement said the halves were not worth separating: 2026-09-07, the
//! readback was 9-22% of backend time and compaction 14-41%, so a borrowing
//! accessor in place of `vec![default; n]` + `copy_slots` would have bought
//! only the half that dies anyway when compaction moves. `BackendPhases` below
//! is what measured it, and `--repo-scan-only` prints it.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::engine::{Engine, ENGINE_STAGE_NAMES, PLACEMENT_U32S};
use crate::layout::{
    compact_records_into, GlyphArena, GlyphRecord, InkExtent, ItemPlacement, LayoutError,
    LayoutGlyphs, LayoutItem, PageExtent, Paint, VerifyLayout,
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
    /// One call, and the ENGINE writes render instances into the caller's
    /// arena. No wire record is materialized on either side of the FFI.
    ///
    /// This is the measured answer to 2026-09-07: `Batched` compacts in the
    /// engine, copies the stream across, then compacts again on the host —
    /// three walks that were 87% of a batched load's backend time, against 3%
    /// for computing the layout. `Direct` does the filter and the arithmetic
    /// once, where the glyphs will live.
    ///
    /// It cannot serve `VerifyLayout`: there is no record stream to hand back,
    /// which is the point. `--repo-verify` diffs it against `Batched` at the
    /// seam instead, where the comparison is instances and placements.
    Direct,
    /// Zero crossings into Mojo: the CubeCL device chain, records read back
    /// and compacted on host (rung 4's flip — rung 5 binds the buffers
    /// directly and the readback dies). CAN record: the stream exists until
    /// the compaction consumes it.
    Cubecl,
    /// Hyper-fast parallel pure-Rust engine.
    Hyper,
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
    /// `fold`, split by the ENGINE into what it was actually doing — lanes in
    /// `engine::ENGINE_STAGE_NAMES` order, nanoseconds, summed across calls.
    /// Without this the whole FFI call reads as fold cost, and it is not:
    /// `glyph_engine_load_items` also runs a second compaction into the engine
    /// arena and a SERIAL O(bytes) walk to attribute records to items.
    pub engine_stages: [u64; ENGINE_STAGE_NAMES.len()],
}

impl BackendPhases {
    /// What the readback costs in total — the number the plan wants, and the
    /// only one of the two halves that is trustworthy alone.
    pub fn readback(&self) -> Duration {
        self.readback_alloc + self.readback_copy
    }

    fn add_engine_stages(&mut self, lanes: [u64; ENGINE_STAGE_NAMES.len()]) {
        for (slot, add) in self.engine_stages.iter_mut().zip(lanes) {
            *slot += add;
        }
    }

    /// The engine lanes as (name, duration), largest first — the order a reader
    /// wants, since the point of the split is finding where the time went.
    pub fn engine_ranked(&self) -> Vec<(&'static str, Duration)> {
        let mut v: Vec<_> = ENGINE_STAGE_NAMES
            .iter()
            .zip(self.engine_stages)
            .map(|(n, ns)| (*n, Duration::from_nanos(ns)))
            .collect();
        v.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
        v
    }
}

/// Decode one item's placement block from the engine's u32 lanes.
///
/// The lane indices are `PL_*` in `ffi.mojo` and they are duplicated here by
/// necessity — two languages, one wire format. What keeps that honest is
/// `Engine::check_instance_shape`, which refuses the load if the two sides
/// disagree about the block's SIZE, and `--repo-verify`, which would show any
/// disagreement about its CONTENT as a bit difference against `Batched`.
fn placement_from_lanes(lanes: &[u32]) -> ItemPlacement {
    let f = |i: usize| f32::from_bits(lanes[i]);
    ItemPlacement {
        slot_base: lanes[0],
        slot_count: lanes[1],
        record_count: lanes[2],
        page: PageExtent { right: f(4), bottom: f(5), z_min: f(6), z_max: f(7) },
        ink: InkExtent {
            min: [f(8), f(9), f(10)],
            max: [f(11), f(12), f(13)],
        },
    }
}

impl Strategy {
    /// Whether this strategy can hand back the 32 B wire stream.
    ///
    /// `Direct` cannot, and that is the feature rather than a gap: no record is
    /// materialized anywhere on its path. Callers ASK — a caller that assumed
    /// and got an empty Vec would read "nothing to diff" as "nothing differed",
    /// which is the vacuous-pass shape this tree has been bitten by before.
    /// Whether this strategy's load ever materializes a wire record — the
    /// same fact as [`Strategy::can_record`], asked by the reporting side.
    /// Named separately because the report is not asking "may I request
    /// records", it is asking "is a zero in the readback lane an absence or a
    /// measurement", and conflating those is how a 0.000s gets read as fast.
    pub fn materializes_records(self) -> bool {
        self.can_record()
    }

    pub fn can_record(self) -> bool {
        match self {
            Strategy::Batched | Strategy::PerItem | Strategy::Cubecl => true,
            Strategy::Direct | Strategy::Hyper => false,
        }
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
            // The Cubecl variant never reaches MojoLayout (the seam routes
            // it to CubeclLayout); the arm exists for exhaustiveness.
            Strategy::Cubecl => unreachable!("CubeclLayout owns the cubecl strategy"),
            Strategy::Hyper => unreachable!("HyperLayout owns the hyper strategy"),
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
                self.phases.add_engine_stages(self.engine.stage_ns());
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
            Strategy::Direct => {
                // No record stream exists on this path, so a caller that asked
                // for one asked for something this strategy cannot produce.
                // Refusing beats silently returning an empty Vec that a gate
                // would read as "nothing differed".
                if records_out.is_some() {
                    return Err(LayoutError {
                        backend: "mojo-cpu/direct",
                        status: -1,
                        what: "the direct path materializes no wire records; use \
                               Batched or PerItem for VerifyLayout"
                            .to_string(),
                    });
                }
                Engine::check_instance_shape()?;

                let total_bytes: usize = items.iter().map(|i| i.bytes.len()).sum();
                let mut blob = Vec::with_capacity(total_bytes);
                let mut descs = Vec::with_capacity(items.len());
                let mut paint_ptrs = Vec::with_capacity(items.len());
                let mut paint_lens = Vec::with_capacity(items.len());
                let mut flat_colors = Vec::with_capacity(items.len());
                let mut group_ids = Vec::with_capacity(items.len());
                for item in items {
                    descs.push((blob.len() as u64, item.bytes.len() as u64, item.params));
                    blob.extend_from_slice(item.bytes);
                    group_ids.push(item.group_id);
                    match item.paint {
                        Paint::Flat(rgba) => {
                            paint_ptrs.push(std::ptr::null());
                            paint_lens.push(0);
                            flat_colors.push(rgba);
                        }
                        Paint::PerRecord(colors) => {
                            paint_ptrs.push(colors.as_ptr());
                            paint_lens.push(colors.len() as u64);
                            flat_colors.push(0);
                        }
                    }
                }

                // Leaders cannot exceed bytes — one leader per UTF-8 sequence,
                // and a sequence is at least one byte — so the byte count is a
                // sound upper bound without folding first. It is TIGHT for
                // ASCII and loose for multibyte text; the engine reports what
                // it actually wrote, and `GE_ARENA_TOO_SMALL` fires loudly if
                // this reasoning is ever wrong rather than writing past the end.
                // The engine's prefix is CALL-RELATIVE — it starts each load at
                // slot 0, because it knows nothing about what the arena already
                // holds. `compact_records_into` takes `arena.len()` instead, so
                // its bases are ABSOLUTE. Rebasing here is what makes the two
                // agree on a second load into the same arena; without it the
                // instances land correctly appended and every `slot_base` is
                // low by exactly the arena's prior length. Not reachable from
                // `load_repo` today (fresh arena, one call), which is precisely
                // why it needed a test rather than a reader.
                let base = arena.len() as u32;
                let (inst_ptr, cap) = arena.uninit_tail(total_bytes);
                let mut place = vec![0u32; items.len() * PLACEMENT_U32S];
                let t = Instant::now();
                // SAFETY: `inst_ptr` came from `uninit_tail(cap)` so it is
                // writable for `cap` instances; `place` is sized per item; each
                // paint pointer is null or borrows that item's colour slice,
                // which outlives the call.
                unsafe {
                    self.engine.load_items_direct(
                        &blob, &descs, inst_ptr as *mut u32, cap,
                        &paint_ptrs, &paint_lens, &flat_colors, &group_ids,
                        &mut place,
                    )?
                };
                self.phases.fold += t.elapsed();
                self.phases.add_engine_stages(self.engine.stage_ns());

                let mut written = 0usize;
                for i in 0..items.len() {
                    let mut p = placement_from_lanes(&place[i * PLACEMENT_U32S..]);
                    p.slot_base += base;
                    written += p.slot_count as usize;
                    placements.push(p);
                }
                // SAFETY: the engine wrote exactly `written` instances into the
                // tail, contiguous from its base — the placements it just
                // reported are how it says so.
                unsafe { arena.commit(written) };
            }
            Strategy::PerItem => {
                for item in items {
                    let t = Instant::now();
                    self.engine.load_item(item.bytes, &item.params)?;
                    self.phases.fold += t.elapsed();
                    self.phases.add_engine_stages(self.engine.stage_ns());
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
            Strategy::Direct => "mojo-cpu/direct",
            // Cubecl never runs ON MojoLayout — the seam routes it to
            // CubeclLayout — but the enum is shared, so the arm must say
            // what it would be. Unreachable in practice.
            Strategy::Cubecl => "cubecl",
            Strategy::Hyper => "hyper-rust",
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

    /// HARVESTED from the adversarial audit branch (`57d13b3`,
    /// `audit/direct-arena-2026-09-07`), which was written against the direct
    /// path before its defects were fixed and then deleted. Its FINDING tests
    /// are superseded by the fixed-behaviour ones below; these are the parts
    /// main never had — the invariants that were repaired without being tested.
    ///
    /// Deterministic xorshift so a failure is reproducible from the seed alone;
    /// a dev-dependency for this would be a dependency for one struct.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
    }

    /// Byte strings built to pull the two UTF-8 walkers apart: the host's
    /// `colorize_leaders` and the engine's decode. Lone continuations, leads
    /// with no continuation, truncation at EOF, out-of-Unicode leads, every
    /// single byte value on its own, and noise.
    fn adversarial_corpus() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\n".to_vec(),
            b"fn main() {\n    let x = 1;\n}\n".to_vec(),
            b"\xC3AB\n".to_vec(),      // lead, no continuation
            b"\xE0AB\n".to_vec(),
            b"\xF0ABC\n".to_vec(),
            b"ab\xC3".to_vec(),         // truncated at EOF
            b"ab\xF0\x9F".to_vec(),
            b"\x80\x80\x80\n".to_vec(),  // lone continuations
            b"a\xBFb\n".to_vec(),
            b"\xF7\xBF\xBF\xBF\n".to_vec(), // out-of-Unicode leads (block-0 contract)
            b"\xF5\xF6\xF7\n".to_vec(),
            "h\u{e9}llo w\u{f6}rld\n\u{f1}\n\u{1f30d}\u{1f30e}\n".as_bytes().to_vec(),
            b"a\tb\r\n\0c\n".to_vec(),
        ];
        for b in 0u16..=255 {
            out.push(vec![b as u8]);
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for len in [1usize, 2, 3, 7, 33, 257, 1024] {
            for _ in 0..8 {
                out.push((0..len).map(|_| rng.byte()).collect());
            }
        }
        out
    }

    /// THE INVARIANT THE PAINT BOUND RESTS ON, which was repaired before it was
    /// tested. Paint is indexed by RECORD, and the host builds that array with
    /// `colorize_leaders` while the engine counts records in its own decoder.
    /// If those two ever disagreed about which bytes are leaders, the direct
    /// path would read off the end of the caller's colour array — in parallel,
    /// across grains. `GE_PAINT_TOO_SHORT` refuses that at the boundary now;
    /// this says the refusal should never have cause to fire.
    ///
    /// The audit demonstrated the break: changing `colorize_leaders`'s
    /// `id += 1` to `id += n`, which is what "make it UTF-8-correct" looks
    /// like, over-reads on `[c3 41 42 0a]` — three modules from the unsafe read.
    #[test]
    fn the_paint_length_and_the_engine_record_count_agree_on_every_byte_string() {
        let mut backend = MojoLayout::new(Strategy::Batched);
        backend.load_trie_file(&trie()).expect("trie");
        let mut multibyte_seen = false;
        for bytes in adversarial_corpus() {
            let colors = crate::text::colorize_leaders(&bytes);
            let items = [LayoutItem {
                bytes: &bytes,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: 0,
                paint: Paint::Flat(0),
            }];
            let mut arena = GlyphArena::new();
            let places = backend.layout_items(&items, &mut arena).expect("layout");
            assert_eq!(
                places[0].record_count as usize,
                colors.len(),
                "record/colour disagreement on {bytes:02x?}",
            );
            // The arena bound the direct path sizes against: leaders <= bytes.
            assert!(
                places[0].record_count as usize <= bytes.len(),
                "leaders exceeded bytes on {bytes:02x?}",
            );
            assert!(places[0].slot_count <= places[0].record_count);
            if colors.len() < bytes.len() {
                multibyte_seen = true;
            }
        }
        // ANTI-VACUITY: if every string were one record per byte, the
        // interesting half of this claim was never exercised.
        assert!(multibyte_seen, "the corpus must contain multibyte sequences");
    }

    /// The whole render-visible contract over the adversarial corpus PLUS a
    /// ~5-grain item whose blanks are unevenly distributed, so each grain's
    /// survivor count differs from its record count — which is the scan's core
    /// invariant (slots advance by survivors, paint by records) under input
    /// designed to break it. Both wrap modes, wrap on and off.
    #[test]
    fn direct_and_batched_agree_bit_for_bit_on_adversarial_items() {
        use crate::fold::WrapMode;
        let mut corpus = adversarial_corpus();
        let target = 5 * 65536 + 977;
        let mut big = Vec::with_capacity(target);
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        while big.len() < target {
            match rng.next() % 8 {
                0 => big.push(b'\n'),
                1 => big.extend_from_slice(b"    "),
                2 => big.extend_from_slice("\u{e9}".as_bytes()),
                3 => big.push(b'\t'),
                _ => big.push(b'a' + (rng.byte() % 26)),
            }
        }
        corpus.push(big);

        for mode in [WrapMode::Down, WrapMode::Back] {
            for wrap in [0i32, 40] {
                let p = ItemParams {
                    line_height: 1.25,
                    wrap_width: wrap,
                    wrap_mode: mode,
                    ..Default::default()
                };
                let colors: Vec<Vec<u32>> =
                    corpus.iter().map(|b| crate::text::colorize_leaders(b)).collect();
                let items: Vec<LayoutItem<'_>> = corpus
                    .iter()
                    .enumerate()
                    .map(|(i, b)| LayoutItem {
                        bytes: b,
                        params: p,
                        group_id: i as u32,
                        paint: Paint::PerRecord(&colors[i]),
                    })
                    .collect();

                let mut direct = MojoLayout::new(Strategy::Direct);
                direct.load_trie_file(&trie()).expect("trie");
                let mut da = GlyphArena::new();
                let dp = direct.layout_items(&items, &mut da).expect("direct");

                let mut batched = MojoLayout::new(Strategy::Batched);
                batched.load_trie_file(&trie()).expect("trie");
                let mut ba = GlyphArena::new();
                let bp = batched.layout_items(&items, &mut ba).expect("batched");

                for (i, (d, b)) in dp.iter().zip(bp.iter()).enumerate() {
                    assert!(d.bit_eq(b), "{mode:?} wrap {wrap}: item {i}:\n  {d:?}\n  {b:?}");
                }
                assert_eq!(
                    bytemuck::cast_slice::<_, u8>(da.instances()),
                    bytemuck::cast_slice::<_, u8>(ba.instances()),
                    "{mode:?} wrap {wrap}: arenas differ",
                );
                assert!(!da.is_empty(), "the corpus must produce instances");
            }
        }
    }

    /// The CONTROL for the all-empty regression: one empty item AMONG real ones
    /// takes the grained path and was always correct. Without this, a fix that
    /// made every item look empty would satisfy the regression test alone.
    #[test]
    fn one_empty_item_among_real_ones_agrees() {
        let bytes: Vec<Vec<u8>> =
            vec![b"fn a() {}\n".to_vec(), Vec::new(), b"fn b() {}\n".to_vec()];
        let colors: Vec<Vec<u32>> =
            bytes.iter().map(|b| crate::text::colorize_leaders(b)).collect();
        let items: Vec<LayoutItem<'_>> = bytes
            .iter()
            .enumerate()
            .map(|(i, b)| LayoutItem {
                bytes: b,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i as u32,
                paint: Paint::PerRecord(&colors[i]),
            })
            .collect();

        let mut direct = MojoLayout::new(Strategy::Direct);
        direct.load_trie_file(&trie()).expect("trie");
        let mut da = GlyphArena::new();
        let dp = direct.layout_items(&items, &mut da).expect("direct");

        let mut batched = MojoLayout::new(Strategy::Batched);
        batched.load_trie_file(&trie()).expect("trie");
        let mut ba = GlyphArena::new();
        let bp = batched.layout_items(&items, &mut ba).expect("batched");

        for (i, (d, b)) in dp.iter().zip(bp.iter()).enumerate() {
            assert!(d.bit_eq(b), "item {i}:\n  {d:?}\n  {b:?}");
        }
        assert_eq!(dp[1].slot_count, 0, "the middle item must be the empty one");
        assert!(dp[2].slot_count > 0, "the item after the empty one must still lay out");
    }

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

    /// REGRESSION, found 2026-09-07 by an adversarial review and reproduced
    /// before fixing: a corpus whose TOTAL byte length is zero.
    ///
    /// `glyph_engine_load_items_direct` returned `GE_EMPTY` before writing any
    /// placement block, and the host — which maps `GE_EMPTY` to success —
    /// decoded its own zeroed buffer as if the engine had filled it. A zeroed
    /// ink extent is not an empty one: empty seeds at +/-infinity, zero is a box
    /// AT the origin. The fixture `g-pick-repo` has `empty.rs` in it and could
    /// never catch this, because ONE empty file among non-empty ones takes the
    /// normal path and gets the seed correctly.
    #[test]
    fn an_all_empty_corpus_agrees_across_strategies() {
        let colors: Vec<Vec<u32>> = vec![vec![], vec![]];
        let items: Vec<LayoutItem<'_>> = (0..2)
            .map(|i| LayoutItem {
                bytes: b"",
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i,
                paint: Paint::PerRecord(&colors[i as usize]),
            })
            .collect();

        let mut batched = MojoLayout::new(Strategy::Batched);
        batched.load_trie_file(&trie()).expect("trie");
        let mut a = GlyphArena::new();
        let want = batched.layout_items(&items, &mut a).expect("batched");

        let mut direct = MojoLayout::new(Strategy::Direct);
        direct.load_trie_file(&trie()).expect("trie");
        let mut b = GlyphArena::new();
        let got = direct.layout_items(&items, &mut b).expect("direct");

        assert_eq!(want.len(), got.len());
        for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
            assert!(w.bit_eq(g), "item {i} differs:\n  batched: {w:?}\n  direct:  {g:?}");
        }
        // ANTI-VACUITY: if the ink seed were ever changed to zero, the two
        // would agree trivially and this test would stop meaning anything.
        assert!(
            want[0].ink.min[0].is_infinite(),
            "an empty item's ink must seed at infinity, or this test is vacuous",
        );
    }

    /// REGRESSION, same review: a SECOND load into an arena that already holds
    /// instances.
    ///
    /// The engine's prefix is call-relative — it starts every load at slot 0,
    /// knowing nothing about what the arena already holds — while
    /// `compact_records_into` takes `arena.len()`, which is absolute. The
    /// instances land correctly appended either way; only `slot_base` was
    /// wrong, low by exactly the arena's prior length. Nothing in the battery
    /// could see it, because `load_repo` enters the arena exactly once — and
    /// `slot_base` is what the cull and the pick path use to name a file's
    /// glyph range, so the failure mode is silently picking the wrong file.
    #[test]
    fn a_second_load_into_one_arena_agrees_across_strategies() {
        let colors = sample_colors();
        let items = sample_items(&colors);

        let mut want: Vec<ItemPlacement> = Vec::new();
        let mut batched = MojoLayout::new(Strategy::Batched);
        batched.load_trie_file(&trie()).expect("trie");
        let mut a = GlyphArena::new();
        want.extend(batched.layout_items(&items, &mut a).expect("batched 1"));
        want.extend(batched.layout_items(&items, &mut a).expect("batched 2"));

        let mut got: Vec<ItemPlacement> = Vec::new();
        let mut direct = MojoLayout::new(Strategy::Direct);
        direct.load_trie_file(&trie()).expect("trie");
        let mut b = GlyphArena::new();
        got.extend(direct.layout_items(&items, &mut b).expect("direct 1"));
        got.extend(direct.layout_items(&items, &mut b).expect("direct 2"));

        for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
            assert!(w.bit_eq(g), "placement {i} differs:\n  batched: {w:?}\n  direct:  {g:?}");
        }
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(a.instances()),
            bytemuck::cast_slice::<_, u8>(b.instances()),
            "arenas differ after two loads",
        );
        // ANTI-VACUITY: the second load must actually start past the first, or
        // the rebase this test guards is never exercised.
        assert!(
            want[items.len()].slot_base > 0,
            "the second load must begin past slot 0",
        );
    }

    /// THE CHUNK BOUNDARY, which no gate reaches.
    ///
    /// `Strategy::Direct` folds the corpus in chunks of `DIRECT_CHUNK_BYTES`
    /// (4 MiB, `engine/ffi.mojo`), reusing one set of lane arrays across them so
    /// peak memory is set by the chunk and not the corpus. `repo-verify-direct`
    /// runs on `fixtures/g-pick-repo` — 0.4 MB, ONE chunk — so the gate that
    /// covers this path cannot see the boundary at all. Without this test the
    /// whole feature is unexercised by anything that runs.
    ///
    /// Items are ATOMIC to a chunk, so the shape that matters is several items
    /// straddling a boundary: this builds nine of them, ~5.4 MB total, which is
    /// two chunks with the split falling mid-corpus rather than at an edge.
    #[test]
    fn a_corpus_larger_than_one_chunk_agrees_across_strategies() {
        // Varied line lengths and a blank line, so the fold has wraps and the
        // compaction has something to drop on both sides of the boundary.
        let unit: Vec<u8> = (0..30_000u32)
            .flat_map(|i| format!("let x{i} = {i};\n\n").into_bytes())
            .collect();
        let bytes: Vec<Vec<u8>> = (0..9).map(|_| unit.clone()).collect();
        let total: usize = bytes.iter().map(|b| b.len()).sum();
        assert!(
            total > 4 * 1024 * 1024,
            "this corpus must exceed DIRECT_CHUNK_BYTES or the test is vacuous \
             — {total} bytes",
        );

        let colors: Vec<Vec<u32>> =
            bytes.iter().map(|b| crate::text::colorize_leaders(b)).collect();
        let items: Vec<LayoutItem<'_>> = bytes
            .iter()
            .enumerate()
            .map(|(i, b)| LayoutItem {
                bytes: b,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i as u32,
                paint: Paint::PerRecord(&colors[i]),
            })
            .collect();

        let mut batched = MojoLayout::new(Strategy::Batched);
        batched.load_trie_file(&trie()).expect("trie");
        let mut a = GlyphArena::new();
        let want = batched.layout_items(&items, &mut a).expect("batched");

        let mut direct = MojoLayout::new(Strategy::Direct);
        direct.load_trie_file(&trie()).expect("trie");
        let mut b = GlyphArena::new();
        let got = direct.layout_items(&items, &mut b).expect("direct");

        for (i, (w, g)) in want.iter().zip(got.iter()).enumerate() {
            assert!(w.bit_eq(g), "item {i} differs:\n  batched: {w:?}\n  direct:  {g:?}");
        }
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(a.instances()),
            bytemuck::cast_slice::<_, u8>(b.instances()),
            "arenas differ across the chunk boundary",
        );
        // ANTI-VACUITY: the corpus must actually produce glyphs, and the later
        // items must land past the first chunk's worth of slots — otherwise a
        // silently-truncated second chunk would compare equal to nothing.
        assert!(!a.is_empty(), "the corpus must produce instances");
        assert!(
            want[8].slot_base as usize > total / 2,
            "the last item must begin well past the first chunk",
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
