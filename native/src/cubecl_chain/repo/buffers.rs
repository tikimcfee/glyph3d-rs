//! Device buffer allocation, memory tracking, and lifecycle ladder for the GPU chain.

use std::cell::Cell;
use cubecl::client::Client;
use cubecl::server::Handle;
use crate::atlas::TrieTable;
use super::super::{pack_words, PARTIAL_COUNT_STRIDE};
use super::prep::ChainHostInputs;
use super::InstanceInputs;

/// All GPU buffer handles allocated for the scan chain.
pub(crate) struct ChainBuffers {
    // Input / constant tables
    pub h_bytes: Option<Handle>,
    pub h_trie_block_indices: Option<Handle>,
    pub h_trie_block_metrics: Option<Handle>,
    pub h_trie_block_codepoints: Option<Handle>,
    pub h_cluster_sequence_table: Option<Handle>,
    pub h_cluster_bitmap: Option<Handle>,
    pub h_cluster_secondary_offsets: Option<Handle>,
    pub h_cluster_secondary_values: Option<Handle>,
    pub h_item_record_bounds: Handle,
    pub h_item_cluster_enabled: Option<Handle>,
    pub h_item_descriptors: Option<Handle>,
    pub h_walk_plan: Option<Handle>,
    pub h_max_row_extents: Option<Handle>,
    pub trie_block_indices_len: usize,
    pub trie_block_metrics_len: usize,
    pub trie_block_codepoints_len: usize,
    pub trie_block_shift: u32,

    // Intermediate pass buffers
    pub h_glyph_flags: Handle,
    pub h_advance_widths: Handle,
    pub h_glyph_indices: Handle,
    pub h_candidate_slots: Option<Handle>,
    pub h_candidate_end_positions: Option<Handle>,
    pub h_tile_counts: Option<Handle>,
    pub h_tile_metrics: Option<Handle>,
    pub h_spine_counts: Option<Handle>,
    pub h_spine_metrics: Option<Handle>,
    pub h_candidate_total: Option<Handle>,
    pub h_candidate_head_positions: Option<Handle>,
    pub candidate_capacity: usize,
    pub candidate_stride: usize,
    pub cluster_allocs: Option<ClusterCandidateAllocs>,

    // Extent & tail inputs
    pub h_item_extents: Handle,
    pub h_per_record_semantic_colors: Handle,
    pub h_instance_slots: Handle,
    pub h_instance_tints: Handle,
    pub per_record_colors_words: usize,
    pub slots_words: usize,
    pub tint_words: usize,
}

#[derive(Clone)]
pub(crate) struct ClusterCandidateAllocs {
    pub h_lvl: Handle,
    pub h_parent: Handle,
    pub h_parent_b: Handle,
    pub h_d0: Handle,
    pub h_d_a: Handle,
    pub h_d_b: Handle,
    pub h_roots: Handle,
    pub candidate_capacity: usize,
    pub kmax: usize,
    pub candidate_stride: usize,
}

impl ChainBuffers {
    /// Drops all lanes whose last reader ran before or during the survivor pass.
    /// This frees ~28 B per corpus byte before slot scatter / records emission.
    pub(crate) fn release_pre_survivor(&mut self) {
        self.h_bytes = None;
        self.h_trie_block_indices = None;
        self.h_trie_block_metrics = None;
        self.h_trie_block_codepoints = None;
        self.h_cluster_sequence_table = None;
        self.h_cluster_bitmap = None;
        self.h_cluster_secondary_offsets = None;
        self.h_cluster_secondary_values = None;
        self.h_item_cluster_enabled = None;
        self.h_item_descriptors = None;
        self.h_walk_plan = None;
        self.h_max_row_extents = None;
        self.h_candidate_slots = None;
        self.h_candidate_end_positions = None;
        self.h_tile_counts = None;
        self.h_tile_metrics = None;
        self.h_spine_counts = None;
        self.h_spine_metrics = None;
        self.h_candidate_head_positions = None;
        self.cluster_allocs = None;
    }

    pub(crate) fn release_survivor_scan(&mut self) {
        self.h_candidate_total = None;
    }
}

pub(crate) struct BufferAllocationResult {
    pub buffers: ChainBuffers,
    pub live_bytes: u64,
}

#[inline]
pub(crate) fn allocate_chain_buffers(
    client: &Client,
    bytes: &[u8],
    item_count: usize,
    inputs: &ChainHostInputs,
    instance_inputs: &InstanceInputs,
    trie: &TrieTable,
    needs_tint: bool,
) -> BufferAllocationResult {
    let n = bytes.len();
    let n_words = inputs.n_words;
    let n_tiles = inputs.n_tiles;

    let live = Cell::new(0u64);
    let alloc_empty = |size: usize| {
        live.set(live.get() + size as u64);
        client.empty(size)
    };
    let alloc_upload = |b: &[u8]| {
        live.set(live.get() + b.len() as u64);
        client.create_from_slice(b)
    };

    let packed = pack_words(bytes);
    let (trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift) = trie.device_tables();
    let trie_block_indices_len = trie_block_indices.len();
    let trie_block_metrics_len = trie_block_metrics.len();
    let trie_block_codepoints_len = trie_block_codepoints.len();

    let h_bytes = alloc_upload(bytemuck::cast_slice(&packed));
    let h_trie_block_indices = alloc_upload(bytemuck::cast_slice(&trie_block_indices));
    let h_trie_block_metrics = alloc_upload(bytemuck::cast_slice(&trie_block_metrics));
    let h_trie_block_codepoints = alloc_upload(bytemuck::cast_slice(&trie_block_codepoints));
    let h_cluster_sequence_table = alloc_upload(bytemuck::cast_slice(&inputs.seq));
    let h_cluster_bitmap = alloc_upload(bytemuck::cast_slice(&inputs.bitmap));
    let h_cluster_secondary_offsets = alloc_upload(bytemuck::cast_slice(&inputs.pair_secondary_offsets));
    let h_cluster_secondary_values = alloc_upload(bytemuck::cast_slice(&inputs.pair_secondary_values));
    let h_item_record_bounds = alloc_upload(bytemuck::cast_slice(&inputs.item_record_bounds));
    let h_item_cluster_enabled = alloc_upload(bytemuck::cast_slice(&inputs.item_cluster_enabled));
    let h_item_descriptors = alloc_upload(bytemuck::cast_slice(&inputs.item_descriptors));
    let h_glyph_flags = alloc_empty(n_words * 4);
    let h_advance_widths = alloc_empty(n * 4);
    let h_glyph_indices = alloc_empty(n * 4);
    // Note: h_glyph_heights is eliminated; quad height is constant CELL_HEIGHT_WORLD = 1.0.
    let h_tile_counts = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tile_metrics = alloc_empty(n_tiles * 4);
    let h_spine_counts = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_spine_metrics = alloc_empty(n_tiles * 4);
    let h_max_row_extents = alloc_upload(bytemuck::cast_slice(&inputs.extent_words));
    let h_walk_plan = alloc_upload(bytemuck::cast_slice(&inputs.walk_plan));
    let candidate_capacity = 16384usize.min(n.max(256));
    let candidate_stride = candidate_capacity + 1;
    let h_candidate_head_positions = alloc_empty(candidate_stride * 4);
    let h_candidate_slots = alloc_empty(candidate_stride * 4);
    let h_candidate_end_positions = alloc_empty(candidate_stride * 4);
    let h_candidate_total = alloc_upload(&[0u8; 4]);
    let h_item_extents = alloc_upload(bytemuck::cast_slice(&inputs.ext_seed));
    let (h_per_record_semantic_colors, per_record_colors_words) = if !instance_inputs.per_record_colors.is_empty() {
        let len = instance_inputs.per_record_colors.len();
        (alloc_upload(bytemuck::cast_slice(&instance_inputs.per_record_colors)), len)
    } else {
        (alloc_empty(4), 1)
    };
    let total_slots = inputs.total_slots;
    let (h_instance_slots, slots_words) = (
        alloc_empty(total_slots.max(1) as usize * 8 * 4),
        total_slots.max(1) as usize * 8,
    );
    let (h_instance_tints, tint_words) = if needs_tint {
        (
            alloc_empty(total_slots.max(1) as usize * 2 * 4),
            total_slots.max(1) as usize * 2,
        )
    } else {
        (alloc_empty(4), 1)
    };
    let cluster_allocs = if inputs.has_cluster {
        let kmax = ((candidate_capacity as u32 + 1).next_power_of_two().trailing_zeros()) as usize;
        let h_lvl = alloc_empty(kmax * candidate_stride * 4);
        let h_parent = alloc_empty(candidate_stride * 4);
        let h_parent_b = alloc_empty(candidate_stride * 4);
        let h_d0 = alloc_empty(candidate_stride * 4);
        let h_d_a = alloc_empty(candidate_stride * 4);
        let h_d_b = alloc_empty(candidate_stride * 4);
        let h_roots = alloc_empty(item_count.max(1) * 4);
        Some(ClusterCandidateAllocs {
            h_lvl,
            h_parent,
            h_parent_b,
            h_d0,
            h_d_a,
            h_d_b,
            h_roots,
            candidate_capacity,
            kmax,
            candidate_stride,
        })
    } else {
        None
    };

    BufferAllocationResult {
        buffers: ChainBuffers {
            h_bytes: Some(h_bytes),
            h_trie_block_indices: Some(h_trie_block_indices),
            h_trie_block_metrics: Some(h_trie_block_metrics),
            h_trie_block_codepoints: Some(h_trie_block_codepoints),
            h_cluster_sequence_table: Some(h_cluster_sequence_table),
            h_cluster_bitmap: Some(h_cluster_bitmap),
            h_cluster_secondary_offsets: Some(h_cluster_secondary_offsets),
            h_cluster_secondary_values: Some(h_cluster_secondary_values),
            h_item_record_bounds,
            h_item_cluster_enabled: Some(h_item_cluster_enabled),
            h_item_descriptors: Some(h_item_descriptors),
            h_walk_plan: Some(h_walk_plan),
            h_max_row_extents: Some(h_max_row_extents),
            trie_block_indices_len,
            trie_block_metrics_len,
            trie_block_codepoints_len,
            trie_block_shift,
            h_glyph_flags,
            h_advance_widths,
            h_glyph_indices,
            h_candidate_slots: Some(h_candidate_slots),
            h_candidate_end_positions: Some(h_candidate_end_positions),
            h_tile_counts: Some(h_tile_counts),
            h_tile_metrics: Some(h_tile_metrics),
            h_spine_counts: Some(h_spine_counts),
            h_spine_metrics: Some(h_spine_metrics),
            h_candidate_total: Some(h_candidate_total),
            h_candidate_head_positions: Some(h_candidate_head_positions),
            candidate_capacity,
            candidate_stride,
            cluster_allocs,
            h_item_extents,
            h_per_record_semantic_colors,
            h_instance_slots,
            h_instance_tints,
            per_record_colors_words,
            slots_words,
            tint_words,
        },
        live_bytes: live.get(),
    }
}
