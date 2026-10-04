//! Host-side table preparation and input prepass for the GPU scan chain.

use crate::atlas::TrieTable;
use crate::fold::{Item, WrapMode};
use crate::text::ResolveGlyph;
use super::super::cluster::{cluster_host_inputs, cluster_pair_filter};
use super::super::tail::{ordered_key_host, EXT_STRIDE};
use super::super::{IE_STRIDE, IM_STRIDE};

pub(crate) struct ChainHostInputs {
    pub units: usize,
    pub rake: usize,
    pub log: usize,
    pub n_tiles: usize,
    pub n_words: usize,
    pub rspan: usize,
    pub seq: Vec<u32>,
    pub seq_max: u32,
    pub bitmap_advance: f32,
    pub bitmap: Vec<u32>,
    pub ic: Vec<u32>,
    pub poff: Vec<u32>,
    pub pval: Vec<u32>,
    pub ir: Vec<u32>,
    pub ie: Vec<u32>,
    pub im: Vec<f32>,
    pub page_gap_x: Vec<f32>,
    pub walk_plan: Vec<u32>,
    pub min_sw: u32,
    pub uniform_sw: u32,
    pub ext_seed: Vec<u32>,
}

#[inline]
pub(crate) fn prepare_chain_inputs(
    bytes: &[u8],
    items: &[Item],
    trie: &TrieTable,
    wants_instances: bool,
) -> ChainHostInputs {
    let item_count = items.len();
    let n = bytes.len();
    let (units, rake) = (256usize, 8usize);
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    let n_words = n.div_ceil(4);
    let rspan = 32usize;
    let (seq, seq_max, bitmap_advance) = match trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None => {
            eprintln!("cubecl-repo-check: atlas carries no sequence section");
            std::process::exit(1);
        }
    };
    let (bitmap, ic) = cluster_host_inputs(&seq, seq_max, items);
    let (poff, pval) = cluster_pair_filter(&seq, seq_max);
    let mut ir = Vec::with_capacity(item_count * 2);
    let mut ie = Vec::with_capacity(item_count * IE_STRIDE);
    let mut im = Vec::with_capacity(item_count * IM_STRIDE);
    let mut page_gap_x = Vec::with_capacity(item_count);
    for item in items {
        // Note 24 Q3: apply is compiled inline_resolve=false unconditionally
        // and resolve_x writes lm only for fold > 0 leaders — a foldless
        // item would render uninitialized lm with every gate green. No
        // caller produces one today (repo wrap_cols is fixed at 100, and no
        // CLI flag reaches it); this keeps a future caller honest.
        assert!(
            item.wrap_width > 0,
            "run_repo_chain requires folded items (wrap_width > 0)"
        );
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
        ie.push(item.page_rows as u32);
        ie.push(item.page_cols as u32);
        ie.push(item.scroll_rows as u32);
        ie.push(item.pages_wide as u32);
        ie.push(item.wrap_width as u32);
        ie.push(item.has_page as u32);
        ie.push(match item.wrap_mode {
            WrapMode::Down => 0u32,
            WrapMode::Back => 1,
        });
        ie.push(0u32);
        im.push(item.origin_y as f32);
        im.push(item.origin_z as f32);
        im.push(item.line_height as f32);
        im.push(item.z_step as f32);
        im.push(item.band_stride_y as f32);
        im.push(item.depth_per_band as f32);
        im.push(item.depth_per_col as f32);
        im.push(0.0f32);
        im.push(item.origin_x as f32);
        im.push((item.z_step - item.z_step as f32 as f64) as f32);
        page_gap_x.push(item.page_gap_x as f32);
    }

    let mut walk_plan: Vec<u32> = Vec::with_capacity(item_count * 3);
    let mut min_sw = u32::MAX;
    for (i, item) in items.iter().enumerate() {
        let width = if item.wrap_width > 0 {
            item.wrap_width
        } else if item.has_page {
            item.page_cols
        } else {
            0
        };
        if width > 0 && (width as u32) < min_sw {
            min_sw = width as u32;
        }
        walk_plan.push(ir[i * 2]);
        walk_plan.push(ir[i * 2 + 1]);
        walk_plan.push(width as u32);
    }
    let uniform_sw = if min_sw != u32::MAX && items.iter().all(|item| {
        let width = if item.wrap_width > 0 {
            item.wrap_width
        } else if item.has_page {
            item.page_cols
        } else {
            0
        };
        width == 0 || (width as u32) == min_sw
    }) {
        min_sw
    } else {
        0
    };

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

    ChainHostInputs {
        units,
        rake,
        log,
        n_tiles,
        n_words,
        rspan,
        seq,
        seq_max,
        bitmap_advance,
        bitmap,
        ic,
        poff,
        pval,
        ir,
        ie,
        im,
        page_gap_x,
        walk_plan,
        min_sw,
        uniform_sw,
        ext_seed,
    }
}
