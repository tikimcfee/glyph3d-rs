//! Device buffer allocation, memory tracking, and lifecycle ladder for the GPU chain.

use std::cell::Cell;
use cubecl::client::Client;
use cubecl::server::Handle;
use crate::atlas::TrieTable;
use super::super::PARTIAL_COUNT_STRIDE;
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
    pub h_tile_item_base: Option<Handle>,
    pub h_walk_plan: Option<Handle>,
    pub h_max_row_extents: Option<Handle>,
    pub trie_block_indices_len: usize,
    pub trie_block_metrics_len: usize,
    pub trie_block_codepoints_len: usize,
    pub trie_block_shift: u32,

    // Intermediate pass buffers
    pub h_glyph_flags: Handle,
    pub glyph_flags_words: usize,
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
    pub h_segment_entry_advances: Handle,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn allocate_chain_buffers(
    client: &Client,
    bytes: Vec<u8>,
    item_count: usize,
    inputs: &ChainHostInputs<'_>,
    instance_inputs: &InstanceInputs,
    trie: &TrieTable,
    needs_tint: bool,
    is_derived: bool,
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

    let (trie_block_indices, trie_block_metrics, trie_block_codepoints, trie_block_shift) = trie.device_tables();
    let trie_block_indices_len = trie_block_indices.len();
    let trie_block_metrics_len = trie_block_metrics.len();
    let trie_block_codepoints_len = trie_block_codepoints.len();

    let t_start = std::time::Instant::now();
    let mut bytes = bytes;
    let target_len = n_words * 4;
    if bytes.len() < target_len {
        bytes.resize(target_len, 0x80);
    } else if bytes.len() > target_len {
        bytes.truncate(target_len);
    }
    let bytes_len = bytes.len();
    live.set(live.get() + bytes_len as u64);
    let bytes_data = cubecl_common::bytes::Bytes::from_bytes_vec(bytes);
    let h_bytes = client.create(bytes_data);
    let t_bytes = t_start.elapsed();
    let h_trie_block_indices = alloc_upload(bytemuck::cast_slice(trie_block_indices));
    let h_trie_block_metrics = alloc_upload(bytemuck::cast_slice(trie_block_metrics));
    let h_trie_block_codepoints = alloc_upload(bytemuck::cast_slice(trie_block_codepoints));
    let h_cluster_sequence_table = alloc_upload(bytemuck::cast_slice(inputs.seq));
    let h_cluster_bitmap = alloc_upload(bytemuck::cast_slice(inputs.bitmap));
    let h_cluster_secondary_offsets = alloc_upload(bytemuck::cast_slice(inputs.pair_secondary_offsets));
    let h_cluster_secondary_values = alloc_upload(bytemuck::cast_slice(inputs.pair_secondary_values));
    let h_item_record_bounds = alloc_upload(bytemuck::cast_slice(&inputs.item_record_bounds));
    let h_item_cluster_enabled = alloc_upload(bytemuck::cast_slice(&inputs.item_cluster_enabled));
    let h_item_descriptors = alloc_upload(bytemuck::cast_slice(&inputs.item_descriptors));
    let h_tile_item_base = alloc_upload(bytemuck::cast_slice(&inputs.tile_item_base));
    let (h_glyph_flags, glyph_flags_words) = if inputs.has_cluster {
        (alloc_empty(n_words * 4), n_words)
    } else {
        (alloc_empty(4), 1)
    };
    // Quad height is constant CELL_HEIGHT_WORLD = 1.0, and trie lookup is evaluated inline.
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
    let h_segment_entry_advances = alloc_upload(bytemuck::cast_slice(&inputs.segment_entry_advances));
    let t_meta = t_start.elapsed() - t_bytes;
    let total_slots = inputs.total_slots;
    let slot_words = if is_derived { 5 } else { 8 };
    let t_before_slots = std::time::Instant::now();
    let (h_instance_slots, slots_words) = (
        alloc_empty(total_slots.max(1) as usize * slot_words * 4),
        total_slots.max(1) as usize * slot_words,
    );
    let t_slots = t_before_slots.elapsed();
    let (h_instance_tints, tint_words) = if needs_tint {
        (
            alloc_empty(total_slots.max(1) as usize * 2 * 4),
            total_slots.max(1) as usize * 2,
        )
    } else {
        (alloc_empty(4), 1)
    };
    let mut cluster_allocs = None;
    let t_cluster = if inputs.has_cluster {
        let t_c = std::time::Instant::now();
        let kmax = ((candidate_capacity as u32 + 1).next_power_of_two().trailing_zeros()) as usize;
        let stride_bytes = (candidate_stride * 4).next_multiple_of(256);
        let roots_bytes = (item_count.max(1) * 4).next_multiple_of(256);
        let total_scratch_bytes = (kmax + 5) * stride_bytes + roots_bytes;
        let h_scratch = alloc_empty(total_scratch_bytes);

        let mut offset = 0u64;
        let h_lvl = h_scratch.clone().offset_start(offset);
        offset += (kmax * stride_bytes) as u64;

        let h_parent = h_scratch.clone().offset_start(offset);
        offset += stride_bytes as u64;

        let h_parent_b = h_scratch.clone().offset_start(offset);
        offset += stride_bytes as u64;

        let h_d0 = h_scratch.clone().offset_start(offset);
        offset += stride_bytes as u64;

        let h_d_a = h_scratch.clone().offset_start(offset);
        offset += stride_bytes as u64;

        let h_d_b = h_scratch.clone().offset_start(offset);
        offset += stride_bytes as u64;

        let h_roots = h_scratch.offset_start(offset);

        let elapsed = t_c.elapsed();
        cluster_allocs = Some(ClusterCandidateAllocs {
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
        });
        elapsed
    } else {
        std::time::Duration::ZERO
    };

    log::info!(
        "allocate_chain_buffers: total {:?}, bytes upload {:?}, meta {:?}, slots empty {:?}, cluster empty {:?}",
        t_start.elapsed(),
        t_bytes,
        t_meta,
        t_slots,
        t_cluster
    );

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
            h_tile_item_base: Some(h_tile_item_base),
            h_walk_plan: Some(h_walk_plan),
            h_max_row_extents: Some(h_max_row_extents),
            trie_block_indices_len,
            trie_block_metrics_len,
            trie_block_codepoints_len,
            trie_block_shift,
            h_glyph_flags,
            glyph_flags_words,
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
            h_segment_entry_advances,
            h_instance_slots,
            h_instance_tints,
            per_record_colors_words,
            slots_words,
            tint_words,
        },
        live_bytes: live.get(),
    }
}
