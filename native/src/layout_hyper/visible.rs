//! What a `--field-mode visible` load hands the scene instead of slots
//! (M2 of `out/VISIBLE-MODE.md`, 2026-10-10).
//!
//! HyperLayout runs Pass 1 WITH the line table and no Pass 2: nothing is
//! emitted per glyph. The arena carries a [`VisibleStaging`] — the line
//! table, one [`VisibleItemSeed`] per item (its params, group and the fold's
//! `max_row_extent`, which is the paging stride's only input) and, once the
//! loader has moved them in, the items' own bytes. `repo::RepoLoad::into_staged`
//! turns the seeds into the [`VisibleItem`]s the field is built from (the
//! world boxes need the shelf layout's group offsets, which the layout seam
//! never sees), and `glyph_scene::setup` hands the whole thing to
//! `VisibleField::new` as a [`glyph_field_visible::VisibleInputs`].
//!
//! BYTES. A repo load's bytes live in the walk's `RepoFile.bytes` Vecs. The
//! layout seam borrows them (`LayoutItem<'_>`), so it cannot take ownership;
//! the loader does, AFTER the views are built, by moving each file's Vec into
//! `item_bytes` — no copy of the corpus (a 94 MB tree stays one allocation
//! per file). The field uploads them at `items[i].byte_base`, which is the
//! running sum of the lengths in item order.
//!
//! PLACEMENTS. With no Pass 2 there is no emitter to measure the extents, so
//! each item's `ItemPlacement` comes from `pass2_host::compute_single_item_placement`,
//! the serial single-item form that writes no slot; `tests::placements_agree_with_the_derived_device_pass2`
//! holds it bit-exact to the device Pass 2 over `g-pick-repo`.

use glyph_field::ItemParamsGpu;
use glyph_field_visible::{TrieUpload, VisibleItem, WRAP_BACK, WRAP_DOWN};

use super::LineTable;
use crate::atlas::TrieTable;
use crate::layout::ItemParams;

/// One item as the layout seam knows it; the loader and `into_staged` finish
/// it into a [`VisibleItem`].
#[derive(Clone, Copy, Debug)]
pub struct VisibleItemSeed {
    pub params: ItemParams,
    pub group_id: u32,
    /// Pass 1's scalar 7 (`ItemPrepass::max_row_extent`): the widest
    /// item-relative x, the row-paging stride's input.
    pub max_row_extent: f64,
    /// Glyph slots the item would produce (its placement's `slot_count`).
    pub slot_count: u32,
    pub byte_len: u32,
}

/// The visible mode's arena form.
#[derive(Default)]
pub struct VisibleStaging {
    /// Each item's bytes, in item order. Empty until the loader moves the
    /// walk's buffers in (`repo::load_repo_from_prefetched`).
    pub item_bytes: Vec<Vec<u8>>,
    pub line_table: LineTable,
    pub seeds: Vec<VisibleItemSeed>,
    /// The glyphs the items would produce, summed — what `--load-repo`
    /// prints as its instance count.
    pub glyph_count: u64,
    /// The field's items, built by `into_staged` (needs the group offsets).
    pub items: Vec<VisibleItem>,
}

impl VisibleStaging {
    /// Finish item `i` into the field's record. `bbox` is the world box the
    /// scene culls (the same as its `SegCull`, group offset applied).
    pub fn item(&self, i: usize, group_params: ItemParamsGpu, bbox_min: [f32; 3], bbox_max: [f32; 3]) -> VisibleItem {
        let seed = &self.seeds[i];
        let p = &seed.params;
        let byte_base: u64 = self.item_bytes[..i].iter().map(|b| b.len() as u64).sum();
        let lines = self.line_table.item_lines(i);
        VisibleItem {
            params: group_params,
            origin_x: p.origin_x,
            stride_x: if p.has_page && p.page_rows > 0 { seed.max_row_extent + p.page_gap_x } else { 0.0 },
            wrap_width: p.wrap_width,
            wrap_mode: match p.wrap_mode {
                crate::fold::WrapMode::Down => WRAP_DOWN,
                crate::fold::WrapMode::Back => WRAP_BACK,
            },
            cluster: u32::from(super::char_resolve::clusters(p)),
            byte_base,
            byte_len: seed.byte_len,
            first_line: lines.start as u32,
            line_count: (lines.end - lines.start) as u32,
            span_base: 0,
            span_count: 0,
            bbox_min,
            bbox_max,
            group_id: group_params.group,
        }
    }
}

/// The atlas trie as the kernel reads it: `codepoints.bin`'s sections as
/// plain words plus the metrics. Built from the renderer's own `TrieTable`,
/// so the kernel resolves exactly what HyperLayout resolved.
pub fn trie_upload(trie: &TrieTable) -> TrieUpload {
    let mut seq_first_bitmap = vec![0u32; 0x110000 / 32];
    if !trie.sequences.is_empty() {
        let stride = 2 + trie.seq_max as usize;
        for e in (0..trie.sequences.len()).step_by(stride) {
            let cp = trie.sequences[e + 2] as usize;
            if cp < 0x110000 {
                seq_first_bitmap[cp >> 5] |= 1 << (cp & 31);
            }
        }
    }
    let (block_shift, block_index, blocks, entry_stride) = trie.raw_sections();
    TrieUpload {
        block_shift,
        block_index: block_index.to_vec(),
        blocks: blocks.to_vec(),
        entry_stride,
        sequences: trie.sequences.clone(),
        seq_max: trie.seq_max,
        seq_first_bitmap,
        em_height_fu: trie.metrics.em_height_fu,
        primary_advance_fu: trie.metrics.advance_fu,
        bitmap_advance_fu: trie.bitmap_advance_fu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::ClusterMode;
    use crate::layout::{GlyphArena, ItemPlacement, LayoutGlyphs, LayoutItem, Paint, DEFAULT_COLOR_PACKED};
    use crate::layout_hyper::compute_single_item_placement;
    use glyph_field::GlyphFieldMode;
    use std::path::Path;

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    /// The corpora the placement witness runs over: the repo golden's
    /// corpus (what `--load-repo` loads), the immutable cubecl-fork set
    /// (paginate's m>=3 classes, clusters at wrap boundaries, an empty
    /// item) and the intra-line chunk-cut input, in both cluster modes.
    fn corpora() -> Vec<crate::hyper_oracle::Corpus> {
        let mut out = Vec::new();
        for mode in [ClusterMode::Cluster, ClusterMode::Leader] {
            for p in [
                "native/fixtures/g-pick-repo",
                "native/fixtures/cubecl-fork",
                "native/fixtures/g-cluster-repo",
                "native/fixtures/chunk-cut.txt",
                "native/fixtures/chunk-cut-paint.txt",
            ] {
                out.push(crate::hyper_oracle::load_corpus(&root().join(p), mode).expect(p));
            }
        }
        out
    }

    fn items_of(corpus: &crate::hyper_oracle::Corpus) -> Vec<LayoutItem<'_>> {
        corpus
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| LayoutItem { bytes: &it.bytes, params: it.params, group_id: i as u32, paint: Paint::Flat(DEFAULT_COLOR_PACKED) })
            .collect()
    }

    fn show(p: &ItemPlacement) -> String {
        format!(
            "slots {}+{} records {} page right {} bottom {} z [{}, {}] ink {:?}..{:?}",
            p.slot_base, p.slot_count, p.record_count, p.page.right, p.page.bottom, p.page.z_min, p.page.z_max, p.ink.min, p.ink.max
        )
    }

    /// THE PLACEMENT WITNESS. The visible load has no Pass 2 to measure an
    /// item's extents, so it uses `compute_single_item_placement` (serial,
    /// one item, no slot written); every field of every placement must be
    /// bit-equal to the device Pass 2's over the same items, slot bases
    /// included — the scene's cull boxes and pick AABBs are built from them.
    #[test]
    fn placements_agree_with_the_derived_device_pass2() {
        let trie = crate::default_trie();
        let em = trie.metrics.em_height_fu;
        let bitmap_adv = crate::text::fu_to_world(trie.bitmap_advance_fu, em);
        let mut compared = 0usize;
        for corpus in corpora() {
            let items = items_of(&corpus);
            let (_, device) = crate::layout_hyper::device_pass2_derived_on_host(&items, &trie);
            let (chunks, ranges) = crate::layout_hyper::chunk::slice_items_into_chunks(&items);
            let agg = crate::layout_hyper::pass1_prepass_chunks(&chunks, &ranges, &items, &trie, bitmap_adv, em);
            assert_eq!(device.len(), items.len());
            for (i, item) in items.iter().enumerate() {
                let slot_base = agg.chunk_slot_bases[ranges[i].start];
                let (single, _, _, _) = compute_single_item_placement(
                    &item.params,
                    item.bytes,
                    slot_base,
                    agg.prepasses[i].max_row_extent,
                    &trie,
                    bitmap_adv,
                    em,
                );
                assert!(
                    single.bit_eq(&device[i]),
                    "{} / {}: single-item placement differs from the device Pass 2\n  single: {}\n  device: {}",
                    corpus.name,
                    corpus.items[i].label,
                    show(&single),
                    show(&device[i])
                );
                compared += 1;
            }
        }
        // 5 + 4 + 1 + 1 + 1 items, in both cluster modes.
        assert!(compared >= 24, "the witness compared only {compared} items");
    }

    /// A device-less visible load end to end: the arena comes back in the
    /// visible form with one seed per item, a table whose glyph counts sum
    /// to the arena's, and the same placements the device Pass 2 gives.
    #[test]
    fn a_visible_load_stages_the_table_and_the_seeds() {
        let trie = crate::default_trie();
        let corpus = crate::hyper_oracle::load_corpus(&root().join("native/fixtures/g-pick-repo"), ClusterMode::Cluster).unwrap();
        let items = items_of(&corpus);
        let (_, device) = crate::layout_hyper::device_pass2_derived_on_host(&items, &trie);
        let mut engine = crate::layout_hyper::HyperLayout::with_field_mode(GlyphFieldMode::Visible);
        let mut arena = GlyphArena::new();
        let placements = engine.layout_items(&items, &mut arena).unwrap();
        assert!(arena.is_visible() && !arena.is_device());
        let staging = arena.visible_staging().unwrap();
        assert_eq!(staging.seeds.len(), items.len());
        assert!(staging.item_bytes.is_empty(), "the loader, not the seam, owns the bytes");
        let table_glyphs: u64 = staging.line_table.entries.iter().map(|e| e.glyph_count as u64).sum();
        assert_eq!(table_glyphs, staging.glyph_count);
        assert_eq!(arena.len() as u64, staging.glyph_count);
        assert!(staging.glyph_count > 1000, "g-pick-repo has thousands of glyphs");
        assert_eq!(staging.line_table.item_first_line.len(), items.len() + 1, "one first-line per item plus the sentinel");
        for (i, (got, want)) in placements.iter().zip(&device).enumerate() {
            assert!(got.bit_eq(want), "item {i}: {} vs device {}", show(got), show(want));
            assert_eq!(staging.seeds[i].slot_count, want.slot_count);
            assert_eq!(staging.seeds[i].byte_len as usize, items[i].bytes.len());
        }
        // The same engine asked for Instanced stages host records, as before.
        let mut plain = crate::layout_hyper::HyperLayout::with_field_mode(GlyphFieldMode::Instanced);
        let mut host = GlyphArena::new();
        plain.layout_items(&items, &mut host).unwrap();
        assert!(!host.is_visible() && !host.is_device());
        assert_eq!(host.len() as u64, staging.glyph_count);
    }

    /// The seam's table records ARE the kernel's: same size, same alignment,
    /// same lane order, so `bytemuck::cast_slice` in `glyph_scene::setup`
    /// hands them over without a copy or a transcode.
    #[test]
    fn visible_records_are_the_kernels() {
        use glyph_field_visible::{LineEntryGpu, SegmentSeedGpu};
        assert_eq!(std::mem::size_of::<super::super::LineEntry>(), std::mem::size_of::<LineEntryGpu>());
        assert_eq!(std::mem::align_of::<super::super::LineEntry>(), std::mem::align_of::<LineEntryGpu>());
        assert_eq!(std::mem::size_of::<super::super::SegmentSeed>(), std::mem::size_of::<SegmentSeedGpu>());
        let entries = [super::super::LineEntry { byte_start: 1, item: 2, base_row: 3, glyph_count: 4, cols: 5, width_cells: 6 }];
        let gpu: &[LineEntryGpu] = bytemuck::cast_slice(&entries);
        assert_eq!(gpu[0], LineEntryGpu { byte_start: 1, item: 2, base_row: 3, glyph_count: 4, cols: 5, width_cells: 6 });
        let seeds = [super::super::SegmentSeed { line: 1, byte_offset: 2, col: 3, seg_adv: 4.5, cells: 6, _pad: 0 }];
        let gpu: &[SegmentSeedGpu] = bytemuck::cast_slice(&seeds);
        assert_eq!(gpu[0], SegmentSeedGpu { line: 1, byte_offset: 2, col: 3, seg_adv: 4.5, cells: 6, _pad: 0 });
    }

    /// `VisibleStaging::item`: byte bases are the running sum in item order,
    /// line ranges come from the table, the stride is the pager's.
    #[test]
    fn staging_items_carry_bases_lines_and_stride() {
        let corpus = crate::hyper_oracle::load_corpus(&root().join("native/fixtures/g-pick-repo"), ClusterMode::Cluster).unwrap();
        let items = items_of(&corpus);
        let mut engine = crate::layout_hyper::HyperLayout::with_field_mode(GlyphFieldMode::Visible);
        let mut arena = GlyphArena::new();
        engine.layout_items(&items, &mut arena).unwrap();
        let staging = arena.visible_staging_mut().unwrap();
        staging.item_bytes = corpus.items.iter().map(|it| it.bytes.clone()).collect();
        let mut base = 0u64;
        for (i, item) in items.iter().enumerate() {
            let mut gp = ItemParamsGpu::from(&item.params);
            gp.group = i as u32;
            let v = staging.item(i, gp, [0.0; 3], [1.0; 3]);
            assert_eq!(v.byte_base, base, "item {i}");
            assert_eq!(v.byte_len as usize, item.bytes.len());
            base += v.byte_len as u64;
            let lines = staging.line_table.item_lines(i);
            assert_eq!((v.first_line as usize, v.line_count as usize), (lines.start, lines.end - lines.start));
            assert_eq!(v.group_id, i as u32);
            assert_eq!(v.cluster, 1, "the corpus loads in cluster mode");
            let p = &item.params;
            let want_stride = if p.has_page && p.page_rows > 0 { staging.seeds[i].max_row_extent + p.page_gap_x } else { 0.0 };
            assert_eq!(v.stride_x, want_stride);
            assert_eq!(v.wrap_mode, WRAP_BACK, "the repo's default wrap");
        }
    }

    /// The bitmap says exactly what `starts_a_sequence` says, over every
    /// codepoint — the kernel's cheap rejection is the host's.
    #[test]
    fn trie_upload_bitmap_matches_starts_a_sequence() {
        let trie = crate::default_trie();
        let up = trie_upload(&trie);
        assert_eq!(up.seq_first_bitmap.len(), 0x110000 / 32);
        let mut set = 0usize;
        for cp in 0..0x110000u32 {
            let bit = up.seq_first_bitmap[(cp >> 5) as usize] >> (cp & 31) & 1 == 1;
            assert_eq!(bit, trie.starts_a_sequence(cp), "codepoint {cp:#x}");
            set += usize::from(bit);
        }
        assert!(set > 0, "the atlas carries sequences; the bitmap must have bits");
        assert_eq!(up.entry_stride, 4);
        assert_eq!(up.em_height_fu, trie.metrics.em_height_fu);
        // A lookup through the uploaded words is the table's own lookup.
        for cp in [b'A' as u32, b' ' as u32, 0x1F400, 0x1F680, 0xE9, 0x4E16] {
            let block = up.block_index[(cp >> up.block_shift) as usize];
            let e = ((block << up.block_shift) | (cp & 0xFF)) as usize * up.entry_stride as usize;
            let want = trie.lookup(cp);
            assert_eq!(up.blocks[e], want.glyph_id, "glyph of {cp:#x}");
            assert_eq!(up.blocks[e + 1] as i32, want.advance_fu, "advance of {cp:#x}");
        }
    }
}
