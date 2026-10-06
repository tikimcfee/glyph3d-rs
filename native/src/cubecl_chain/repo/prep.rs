//! Host-side table preparation and input prepass for the GPU scan chain.

use crate::atlas::TrieTable;
use crate::fold::{Item, WrapMode};
use crate::text::ResolveGlyph;
use rayon::prelude::*;
use super::super::cluster::{cluster_host_inputs, cluster_pair_filter};
use super::super::tail::{ordered_key_host, EXT_STRIDE};
use super::super::ITEM_DESC_STRIDE;
use super::InstanceInputs;

pub(crate) struct ChainHostInputs {
    pub threads_per_cube: usize,
    pub bytes_per_thread: usize,
    pub log: usize,
    pub n_tiles: usize,
    pub n_words: usize,
    pub seq: Vec<u32>,
    pub seq_max: u32,
    pub bitmap_advance: f32,
    pub bitmap: Vec<u32>,
    pub item_cluster_enabled: Vec<u32>,
    pub pair_secondary_offsets: Vec<u32>,
    pub pair_secondary_values: Vec<u32>,
    pub item_record_bounds: Vec<u32>,
    pub item_descriptors: Vec<u32>,
    /// `segment_entry_advances[k]`: k one-cell advances summed left to right
    /// from 0.0 in f32 — apply_and_emit's O(1) entry x for a clean segment.
    pub segment_entry_advances: Vec<f32>,
    pub walk_plan: Vec<u32>,
    pub ext_seed: Vec<u32>,
    pub extent_words: Vec<u32>,
    pub leader_totals: Vec<u32>,
    pub survivor_totals: Vec<u32>,
    pub item_record_bases: Vec<u32>,
    pub item_slot_bases: Vec<u32>,
    pub total_records: u32,
    pub total_slots: u32,
    pub has_cluster: bool,
}

pub(crate) struct ItemScan {
    pub max_row_extent: f32,
    pub leader_count: u32,
    pub survivor_count: u32,
}

#[inline]
pub(crate) fn prepare_chain_inputs(
    bytes: &[u8],
    items: &[Item],
    trie: &TrieTable,
    wants_instances: bool,
    instance_inputs: Option<&InstanceInputs>,
) -> ChainHostInputs {
    let item_count = items.len();
    let n = bytes.len();
    let threads_per_cube = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256usize);
    let bytes_per_thread = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8usize);
    let log = threads_per_cube.ilog2() as usize;
    let n_tiles = n.div_ceil(threads_per_cube * bytes_per_thread).max(1);
    let n_words = n.div_ceil(4);
    let (seq, seq_max, bitmap_advance) = match trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None => {
            eprintln!("cubecl-repo-check: atlas carries no sequence section");
            std::process::exit(1);
        }
    };
    let (bitmap, item_cluster_enabled) = cluster_host_inputs(&seq, seq_max, items);
    let (pair_secondary_offsets, pair_secondary_values) = cluster_pair_filter(&seq, seq_max);
    let mut item_record_bounds = Vec::with_capacity(item_count * 2);
    let mut item_descriptors = Vec::with_capacity(item_count * ITEM_DESC_STRIDE);
    // The one-cell advance, in the same conversion decode writes per byte,
    // and its left-to-right running sums out to the widest fold: the exact
    // f32 sequence the backward walk re-adds for an all-one-cell segment.
    let cell_advance = crate::text::fu_to_world(trie.metrics.advance_fu as i32, trie.metrics.em_height_fu);
    let widest_fold = items
        .iter()
        .map(|item| if item.wrap_width > 0 { item.wrap_width as usize } else if item.has_page { item.page_cols as usize } else { 0 })
        .max()
        .unwrap_or(0);
    let mut segment_entry_advances = Vec::with_capacity(widest_fold + 1);
    let mut running_advance = 0.0f32;
    segment_entry_advances.push(running_advance);
    for _ in 0..widest_fold {
        running_advance += cell_advance;
        segment_entry_advances.push(running_advance);
    }
    for (item_idx, item) in items.iter().enumerate() {
        // Note 24 Q3: apply is compiled inline_resolve=false unconditionally
        // and resolve_x writes lm only for fold > 0 leaders — a foldless
        // item would render uninitialized lm with every gate green. No
        // caller produces one today (repo wrap_cols is fixed at 100, and no
        // CLI flag reaches it); this keeps a future caller honest.
        assert!(
            item.wrap_width > 0,
            "run_repo_chain requires folded items (wrap_width > 0)"
        );
        item_record_bounds.push(item.byte_start as u32);
        item_record_bounds.push((item.byte_start + item.byte_count) as u32);

        // Consolidated item descriptor: 32 words (128 B, 16-byte aligned)
        // 0..2: Record bounds
        item_descriptors.push(item.byte_start as u32);
        item_descriptors.push((item.byte_start + item.byte_count) as u32);

        // 2..10: Layout configuration
        item_descriptors.push(item.page_rows as u32);
        item_descriptors.push(item.page_cols as u32);
        item_descriptors.push(item.scroll_rows as u32);
        item_descriptors.push(item.pages_wide as u32);
        item_descriptors.push(item.wrap_width as u32);
        item_descriptors.push(item.has_page as u32);
        item_descriptors.push(match item.wrap_mode {
            WrapMode::Down => 0u32,
            WrapMode::Back => 1,
        });
        item_descriptors.push(0u32); // config pad

        // 10..20: Spatial metrics (stored as f32 bits)
        item_descriptors.push((item.origin_y as f32).to_bits());
        item_descriptors.push((item.origin_z as f32).to_bits());
        item_descriptors.push((item.line_height as f32).to_bits());
        item_descriptors.push((item.z_step as f32).to_bits());
        item_descriptors.push((item.band_stride_y as f32).to_bits());
        item_descriptors.push((item.depth_per_band as f32).to_bits());
        item_descriptors.push((item.depth_per_col as f32).to_bits());
        item_descriptors.push(0.0f32.to_bits()); // metrics pad
        item_descriptors.push((item.origin_x as f32).to_bits());
        item_descriptors.push(((item.z_step - item.z_step as f32 as f64) as f32).to_bits());

        // 20: Page gap X
        item_descriptors.push((item.page_gap_x as f32).to_bits());

        // 21..25: Paint configuration (consolidated from former h_paint buffer)
        let (color_base, is_per_record, flat_color, group) = if let Some(inp) = instance_inputs {
            (inp.color_base[item_idx], inp.is_per_record[item_idx], inp.flat_colors[item_idx], inp.groups[item_idx])
        } else {
            (0, 0, 0, 0)
        };
        item_descriptors.push(color_base);
        item_descriptors.push(is_per_record);
        item_descriptors.push(flat_color);
        item_descriptors.push(group);

        // 25: the one-cell advance (leaf_of's clean-leader test)
        item_descriptors.push(cell_advance.to_bits());

        // 26..32: std430 16-byte alignment padding (6 zeros)
        item_descriptors.push(0u32);
        item_descriptors.push(0u32);
        item_descriptors.push(0u32);
        item_descriptors.push(0u32);
        item_descriptors.push(0u32);
        item_descriptors.push(0u32);
    }

    let mut walk_plan: Vec<u32> = Vec::with_capacity(item_count * 3);
    for (i, item) in items.iter().enumerate() {
        let width = if item.wrap_width > 0 {
            item.wrap_width
        } else if item.has_page {
            item.page_cols
        } else {
            0
        };
        walk_plan.push(item_record_bounds[i * 2]);
        walk_plan.push(item_record_bounds[i * 2 + 1]);
        walk_plan.push(width as u32);
    }

    let mut ext_seed = vec![0u32; item_count * EXT_STRIDE];
    if wants_instances {
        let zero_k = ordered_key_host(0.0f32);
        let inf_k = ordered_key_host(f32::INFINITY);
        let ninf_k = ordered_key_host(f32::NEG_INFINITY);
        for it in 0..item_count {
            let e = it * EXT_STRIDE;
            ext_seed[e] = zero_k;
            ext_seed[e + 1] = zero_k;
            ext_seed[e + 2] = zero_k;
            ext_seed[e + 3] = zero_k;
            ext_seed[e + 4] = inf_k;
            ext_seed[e + 5] = inf_k;
            ext_seed[e + 6] = ninf_k;
            ext_seed[e + 7] = ninf_k;
            ext_seed[e + 8] = inf_k;
            ext_seed[e + 9] = ninf_k;
        }
    }

    let scans: Vec<ItemScan> = items
        .par_iter()
        .map(|item| {
            ItemScan {
                max_row_extent: 0.0,
                leader_count: 0,
                survivor_count: item.byte_count as u32,
            }
        })
        .collect();

    let mut extent_words = Vec::with_capacity(item_count * 2);
    let mut leader_totals = Vec::with_capacity(item_count);
    let mut survivor_totals = Vec::with_capacity(item_count);
    let mut item_record_bases = Vec::with_capacity(item_count);
    let mut item_slot_bases = Vec::with_capacity(item_count);
    let mut total_records = 0u32;
    let mut total_slots = 0u32;

    for (i, scan) in scans.into_iter().enumerate() {
        let key = ordered_key_host(if items[i].has_page && items[i].page_rows > 0 {
            scan.max_row_extent
        } else {
            0.0f32
        });
        extent_words.push(key);
        extent_words.push(0x8000_0000u32);

        item_record_bases.push(total_records);
        item_slot_bases.push(total_slots);
        total_records += scan.leader_count;
        total_slots += scan.survivor_count;
        leader_totals.push(scan.leader_count);
        survivor_totals.push(scan.survivor_count);
    }

    ChainHostInputs {
        threads_per_cube,
        bytes_per_thread,
        log,
        n_tiles,
        n_words,
        seq,
        seq_max,
        bitmap_advance,
        bitmap,
        item_cluster_enabled,
        pair_secondary_offsets,
        pair_secondary_values,
        item_record_bounds,
        item_descriptors,
        segment_entry_advances,
        walk_plan,
        ext_seed,
        extent_words,
        leader_totals,
        survivor_totals,
        item_record_bases,
        item_slot_bases,
        total_records,
        total_slots,
        has_cluster: items.iter().any(|it| it.cluster_mode == crate::fold::ClusterMode::Cluster),
    }
}
