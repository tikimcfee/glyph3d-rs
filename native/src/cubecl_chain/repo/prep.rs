//! Host-side table preparation and input prepass for the GPU scan chain.

use rayon::prelude::*;
use crate::atlas::TrieTable;
use crate::fold::{Item, WrapMode};
use crate::text::ResolveGlyph;
use super::super::tail::{ordered_key_host, EXT_STRIDE};
use super::super::{ITEM_DESC_BYTE_START, ITEM_DESC_STRIDE};
use super::InstanceInputs;

pub(crate) struct ChainHostInputs<'a> {
    pub threads_per_cube: usize,
    pub bytes_per_thread: usize,
    pub log: usize,
    pub n_tiles: usize,
    pub n_words: usize,
    pub seq: &'a [u32],
    pub seq_max: u32,
    pub bitmap_advance: f32,
    pub bitmap: &'a [u32],
    pub item_cluster_enabled: Vec<u32>,
    pub pair_secondary_offsets: &'a [u32],
    pub pair_secondary_values: &'a [u32],
    pub item_record_bounds: Vec<u32>,
    pub item_descriptors: Vec<u32>,
    pub tile_item_base: Vec<u32>,
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
    pub placements: Vec<crate::layout::ItemPlacement>,
}

#[inline]
pub(crate) fn prepare_chain_inputs<'a>(
    bytes: &[u8],
    items: &[Item],
    trie: &'a TrieTable,
    wants_instances: bool,
    instance_inputs: Option<&InstanceInputs>,
) -> ChainHostInputs<'a> {
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
        Some((s, m, a)) => (s, m, a),
        None => {
            eprintln!("cubecl-repo-check: atlas carries no sequence section");
            std::process::exit(1);
        }
    };
    let item_cluster_enabled = items
        .iter()
        .map(|it| u32::from(it.cluster_mode == crate::fold::ClusterMode::Cluster))
        .collect();
    let bitmap = &trie.cluster_bitmap[..];
    let pair_secondary_offsets = &trie.pair_secondary_offsets[..];
    let pair_secondary_values = &trie.pair_secondary_values[..];
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

    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = crate::text::fu_to_world(trie.bitmap_advance_fu, em_height_fu);

    let layout_items: Vec<crate::layout::LayoutItem<'_>> = items
        .iter()
        .map(|it| {
            let start = it.byte_start as usize;
            let end = (it.byte_start + it.byte_count) as usize;
            crate::layout::LayoutItem {
                bytes: &bytes[start..end],
                params: crate::layout::ItemParams {
                    origin_x: it.origin_x,
                    origin_y: it.origin_y,
                    origin_z: it.origin_z,
                    line_height: it.line_height,
                    z_step: it.z_step,
                    wrap_width: it.wrap_width as i32,
                    wrap_mode: it.wrap_mode,
                    cluster_mode: it.cluster_mode,
                    has_page: it.has_page,
                    page_rows: it.page_rows as i32,
                    page_cols: it.page_cols as i32,
                    scroll_rows: it.scroll_rows as i32,
                    pages_wide: it.pages_wide as i32,
                    page_gap_x: it.page_gap_x,
                    band_stride_y: it.band_stride_y,
                    depth_per_band: it.depth_per_band,
                    depth_per_col: it.depth_per_col,
                    page_line_height: it.page_line_height,
                },
                group_id: 0,
                paint: crate::layout::Paint::Flat(0),
            }
        })
        .collect();

    let t_prepass0 = std::time::Instant::now();
    let prepasses = crate::layout_hyper::pass1_prepass(&layout_items, trie, bitmap_adv, em_height_fu);
    let dur_prepass = t_prepass0.elapsed();

    let mut extent_words = Vec::with_capacity(item_count * 2);
    let mut leader_totals = Vec::with_capacity(item_count);
    let mut survivor_totals = Vec::with_capacity(item_count);
    let mut item_record_bases = Vec::with_capacity(item_count);
    let mut item_slot_bases = Vec::with_capacity(item_count);
    let mut total_records = 0u32;
    let mut total_slots = 0u32;

    for (i, pre) in prepasses.iter().enumerate() {
        let max_row_extent = pre.max_row_extent as f32;
        let key = ordered_key_host(if items[i].has_page && items[i].page_rows > 0 {
            max_row_extent
        } else {
            0.0f32
        });
        extent_words.push(key);
        extent_words.push(0x8000_0000u32);

        item_record_bases.push(total_records);
        item_slot_bases.push(total_slots);
        total_records += pre.leader_count;
        total_slots += pre.survivor_count;
        leader_totals.push(pre.leader_count);
        survivor_totals.push(pre.survivor_count);
    }
    let cluster_count = prepasses
        .iter()
        .zip(items)
        .filter(|(pre, it)| pre.has_cluster && it.cluster_mode == crate::fold::ClusterMode::Cluster)
        .count();
    let has_cluster = cluster_count > 0;

    let t_place0 = std::time::Instant::now();
    let placements: Vec<crate::layout::ItemPlacement> = if wants_instances {
        layout_items
            .par_iter()
            .zip(prepasses.par_iter())
            .zip(item_slot_bases.par_iter())
            .map(|((item, pre), &slot_base)| {
                let (mut placement, _, _, _) = crate::layout_hyper::compute_single_item_placement(
                    &item.params,
                    item.bytes,
                    slot_base,
                    pre.max_row_extent,
                    trie,
                    bitmap_adv,
                    em_height_fu,
                );
                placement.slot_count = pre.survivor_count;
                placement.record_count = pre.leader_count;
                placement
            })
            .collect()
    } else {
        layout_items
            .iter()
            .zip(prepasses.iter())
            .zip(item_slot_bases.iter())
            .map(|((item, pre), &slot_base)| {
                closed_form_placement(item, pre, slot_base, trie, em_height_fu)
            })
            .collect()
    };
    let dur_placements = t_place0.elapsed();

    tracing::info!(
        "tables breakdown: prepass {:?}, placements {:?} (has_cluster: {}, cluster_items: {})",
        dur_prepass,
        dur_placements,
        has_cluster,
        cluster_count,
    );

    let tile_size = threads_per_cube * bytes_per_thread;
    let tile_item_base = compute_tile_item_base(n_tiles, tile_size, n, &item_descriptors, item_count);

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
        tile_item_base,
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
        has_cluster,
        placements,
    }
}

/// Precomputes the base item index for each tile, eliminating threadgroup binary searches and barriers on the GPU.
pub(crate) fn compute_tile_item_base(
    n_tiles: usize,
    tile_size: usize,
    total_bytes: usize,
    item_descriptors: &[u32],
    item_count: usize,
) -> Vec<u32> {
    let mut tile_item_base = Vec::with_capacity(n_tiles);
    let mut current_item_index = 0usize;
    for tile_index in 0..n_tiles {
        let probe_byte = (tile_index * tile_size).min(if total_bytes > 0 { total_bytes - 1 } else { 0 });
        while current_item_index + 1 < item_count
            && (item_descriptors[(current_item_index + 1) * ITEM_DESC_STRIDE + ITEM_DESC_BYTE_START] as usize) <= probe_byte
        {
            current_item_index += 1;
        }
        tile_item_base.push(current_item_index as u32);
    }
    tile_item_base
}

fn closed_form_placement(
    item: &crate::layout::LayoutItem<'_>,
    pre: &crate::layout_hyper::ItemPrepass,
    slot_base: u32,
    trie: &TrieTable,
    em_height_fu: u32,
) -> crate::layout::ItemPlacement {
    let p = &item.params;
    let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);
    let page_stride_x = if p.has_page && p.page_rows > 0 {
        pre.max_row_extent + p.page_gap_x
    } else {
        0.0
    };
    let cell_advance = crate::text::fu_to_world(trie.metrics.advance_fu as i32, em_height_fu) as f64;
    let page_rows = p.page_rows.max(1) as i64;
    let pages_wide = (p.pages_wide as i64).max(1);
    let scroll_rows = p.scroll_rows as i64;
    let max_row = (pre.row_count as i64).saturating_sub(1);
    let y_page = if page_rows > 0 { max_row / page_rows } else { 0 };
    let band = if pages_wide > 0 { y_page / pages_wide } else { 0 };
    let total_pages = y_page + 1;
    let max_page_col = (total_pages - 1).min(pages_wide - 1);

    let page_right = if page_active {
        ((p.origin_x as f32) as f64 + max_page_col as f64 * page_stride_x + pre.max_row_extent) as f32
    } else {
        (p.origin_x + pre.max_row_extent) as f32
    };

    let screen_row = max_row + scroll_rows;
    let page_bottom = if page_active {
        if max_row < 0 {
            p.origin_y as f32
        } else if total_pages == 1 {
            (p.origin_y - screen_row as f64 * p.line_height) as f32
        } else {
            // With multiple pages (total_pages > 1), every page prior to the last
            // is a full page reaching row depth (page_rows - 1).
            let full_page_row = page_rows - 1;
            let last_band_pages = total_pages - band * pages_wide;

            // Deepest baseline in preceding band (if any band < band existed)
            let prev_band_bottom = if band > 0 {
                let prev_band = band - 1;
                let full_screen_row = full_page_row + scroll_rows;
                p.origin_y - full_screen_row as f64 * p.line_height - prev_band as f64 * p.band_stride_y
            } else {
                f64::INFINITY
            };

            // Deepest baseline in the current (last) band
            let last_band_bottom = if last_band_pages > 1 {
                // The current band has at least one preceding full page
                let full_screen_row = full_page_row + scroll_rows;
                p.origin_y - full_screen_row as f64 * p.line_height - band as f64 * p.band_stride_y
            } else {
                // The current band contains only the partial last page
                let last_screen_row = (max_row - (total_pages - 1) * page_rows) + scroll_rows;
                p.origin_y - last_screen_row as f64 * p.line_height - band as f64 * p.band_stride_y
            };

            prev_band_bottom.min(last_band_bottom) as f32
        }
    } else {
        (p.origin_y - max_row as f64 * p.line_height) as f32
    };

    let wrap_w = p.wrap_width as f64;
    let max_wrap_segment = if p.wrap_mode == crate::fold::WrapMode::Back && wrap_w > 0.0 && cell_advance > 0.0 {
        ((pre.max_row_extent / (wrap_w * cell_advance)).ceil() as i64).saturating_sub(1).max(0)
    } else {
        0
    };

    let page_z_min = (p.origin_z - max_wrap_segment as f64 * p.z_step + band as f64 * p.depth_per_band) as f32;
    let page_z_max = (p.origin_z + band as f64 * p.depth_per_band) as f32;

    let cell_height = crate::text::fu_to_world(em_height_fu as i32, em_height_fu);
    let half_height = cell_height * 0.5;
    let ink = if pre.survivor_count == 0 {
        crate::layout::InkExtent {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    } else {
        crate::layout::InkExtent {
            min: [
                p.origin_x as f32,
                page_bottom - half_height,
                page_z_min,
            ],
            max: [
                page_right,
                p.origin_y as f32 + half_height,
                page_z_max,
            ],
        }
    };

    crate::layout::ItemPlacement {
        slot_base,
        slot_count: pre.survivor_count,
        record_count: pre.leader_count,
        page: crate::layout::PageExtent {
            right: page_right,
            bottom: page_bottom,
            z_min: page_z_min,
            z_max: page_z_max,
        },
        ink,
    }
}

