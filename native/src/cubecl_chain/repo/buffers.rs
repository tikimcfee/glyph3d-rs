//! Device buffer allocation, memory tracking, and lifecycle ladder for the GPU chain.

use std::cell::Cell;
use cubecl::client::Client;
use cubecl::server::Handle;
use crate::atlas::TrieTable;
use super::super::{pack_words, LC_STRIDE, LM_STRIDE, PARTIAL_COUNT_STRIDE};
use super::prep::ChainHostInputs;
use super::InstanceInputs;

/// All GPU buffer handles allocated for the scan chain.
pub(crate) struct ChainBuffers {
    // Input / constant tables
    pub h_bytes: Option<Handle>,
    pub h_bi: Option<Handle>,
    pub h_bm: Option<Handle>,
    pub h_bc: Option<Handle>,
    pub h_seq: Option<Handle>,
    pub h_bmap: Option<Handle>,
    pub h_poff: Option<Handle>,
    pub h_pval: Option<Handle>,
    pub h_ir: Handle,
    pub h_ic: Option<Handle>,
    pub h_ie: Option<Handle>,
    pub h_im: Option<Handle>,
    pub h_gap: Option<Handle>,
    pub h_plan: Option<Handle>,
    pub h_rmax: Option<Handle>,
    pub h_xmax: Option<Handle>,
    pub h_extent: Option<Handle>,
    pub bi_len: usize,
    pub bm_len: usize,
    pub bc_len: usize,
    pub bshift: u32,

    // Intermediate pass buffers
    pub h_fl: Handle,
    pub h_sm: Handle,
    pub h_gi: Handle,
    pub h_hgt: Handle,
    pub h_cslot: Option<Handle>,
    pub h_cend: Option<Handle>,
    pub h_tc: Option<Handle>,
    pub h_tm: Option<Handle>,
    pub h_xc: Option<Handle>,
    pub h_xm: Option<Handle>,
    pub h_lc: Option<Handle>,
    pub h_wm: Option<Handle>,
    pub h_wc: Handle,
    pub h_otb: Option<Handle>,
    pub h_lm: Handle,
    pub h_ctc: Option<Handle>,
    pub h_cup: Option<Handle>,
    pub h_cxc: Option<Handle>,
    pub h_ctotal: Option<Handle>,
    pub h_hp: Option<Handle>,

    // Survivor scan buffers
    pub h_ltc: Option<Handle>,
    pub h_stc: Option<Handle>,
    pub h_lup: Option<Handle>,
    pub h_sup: Option<Handle>,
    pub h_lxc: Option<Handle>,
    pub h_sxc: Option<Handle>,
    pub h_lgrand: Option<Handle>,
    pub h_sgrand: Option<Handle>,
    pub h_totals: Option<Handle>,

    // Extent & tail inputs
    pub h_ext: Handle,
    pub h_pr_colors: Handle,
    pub h_color_base: Handle,
    pub h_is_pr: Handle,
    pub h_flat_colors: Handle,
    pub h_groups: Handle,
}

impl ChainBuffers {
    /// Drops all lanes whose last reader ran before or during the survivor pass.
    /// This frees ~28 B per corpus byte before slot scatter / records emission.
    pub(crate) fn release_pre_survivor(&mut self) {
        self.h_bytes = None;
        self.h_bi = None;
        self.h_bm = None;
        self.h_bc = None;
        self.h_seq = None;
        self.h_bmap = None;
        self.h_poff = None;
        self.h_pval = None;
        self.h_ic = None;
        self.h_ie = None;
        self.h_im = None;
        self.h_gap = None;
        self.h_plan = None;
        self.h_rmax = None;
        self.h_xmax = None;
        self.h_extent = None;
        self.h_cslot = None;
        self.h_cend = None;
        self.h_tc = None;
        self.h_tm = None;
        self.h_xc = None;
        self.h_xm = None;
        self.h_wm = None;
        self.h_otb = None;
        self.h_hp = None;
        self.h_ctc = None;
        self.h_cup = None;
        self.h_cxc = None;
        self.h_ctotal = None;
        self.h_ltc = None;
        self.h_stc = None;
        self.h_lup = None;
        self.h_lxc = None;
        self.h_lgrand = None;
        self.h_sgrand = None;
        self.h_totals = None;
    }

    pub(crate) fn release_survivor_scan(&mut self) {
        self.h_sup = None;
        self.h_sxc = None;
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
    wants_instances: bool,
) -> BufferAllocationResult {
    let n = bytes.len();
    let n_words = inputs.n_words;
    let n_tiles = inputs.n_tiles;
    let units = inputs.units;

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
    let (bi, bm, bc, bshift) = trie.device_tables();
    let bi_len = bi.len();
    let bm_len = bm.len();
    let bc_len = bc.len();

    let h_bytes = alloc_upload(bytemuck::cast_slice(&packed));
    let h_bi = alloc_upload(bytemuck::cast_slice(&bi));
    let h_bm = alloc_upload(bytemuck::cast_slice(&bm));
    let h_bc = alloc_upload(bytemuck::cast_slice(&bc));
    let h_seq = alloc_upload(bytemuck::cast_slice(&inputs.seq));
    let h_bmap = alloc_upload(bytemuck::cast_slice(&inputs.bitmap));
    let h_poff = alloc_upload(bytemuck::cast_slice(&inputs.poff));
    let h_pval = alloc_upload(bytemuck::cast_slice(&inputs.pval));
    let h_ir = alloc_upload(bytemuck::cast_slice(&inputs.ir));
    let h_ic = alloc_upload(bytemuck::cast_slice(&inputs.ic));
    let h_ie = alloc_upload(bytemuck::cast_slice(&inputs.ie));
    let h_im = alloc_upload(bytemuck::cast_slice(&inputs.im));
    let h_gap = alloc_upload(bytemuck::cast_slice(&inputs.page_gap_x));
    let h_fl = alloc_empty(n_words * 4);
    let h_sm = alloc_empty(n * 4);
    let h_gi = alloc_empty(n * 4);
    let h_hgt = alloc_empty(n * 4);
    let h_cslot = alloc_empty(n * 4);
    let h_cend = alloc_empty(n * 4);
    let h_tc = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = alloc_empty(n_tiles * 4);
    let h_xc = alloc_empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = alloc_empty(n_tiles * 4);
    let h_lc = alloc_empty(n * LC_STRIDE * 4);
    let h_wm = alloc_empty(4);
    let h_wc = alloc_empty(n * 4);
    let h_otb = alloc_empty(n * 4);
    let h_lm = alloc_empty(n * LM_STRIDE * 4);
    let h_rmax = alloc_upload(bytemuck::cast_slice(&vec![0u32; item_count]));
    let h_xmax = alloc_upload(bytemuck::cast_slice(&vec![0u32; item_count]));
    let h_extent = alloc_upload(bytemuck::cast_slice(&inputs.extent_words));
    let h_plan = alloc_upload(bytemuck::cast_slice(&inputs.walk_plan));
    let h_ctc = alloc_empty(n_tiles * 4);
    let h_cup = alloc_empty(n_tiles * units * 4);
    let h_cxc = alloc_empty(n_tiles * 4);
    let h_ctotal = alloc_empty(4);
    let h_hp = alloc_empty(n * 4);
    let h_ltc = alloc_empty(n_tiles * 4);
    let h_stc = alloc_empty(n_tiles * 4);
    let h_lup = alloc_empty(n_tiles * units * 4);
    let h_sup = alloc_empty(n_tiles * units * 4);
    let h_lxc = alloc_empty(n_tiles * 4);
    let h_sxc = alloc_empty(n_tiles * 4);
    let h_lgrand = alloc_empty(4);
    let h_sgrand = alloc_empty(4);
    let h_totals = alloc_empty(item_count.max(1) * 2 * 4);
    let h_ext = alloc_upload(bytemuck::cast_slice(&inputs.ext_seed));
    let h_pr_colors = if wants_instances && !instance_inputs.per_record_colors.is_empty() {
        alloc_upload(bytemuck::cast_slice(&instance_inputs.per_record_colors))
    } else {
        alloc_empty(4)
    };
    let h_color_base = alloc_upload(bytemuck::cast_slice(&instance_inputs.color_base));
    let h_is_pr = alloc_upload(bytemuck::cast_slice(&instance_inputs.is_per_record));
    let h_flat_colors = alloc_upload(bytemuck::cast_slice(&instance_inputs.flat_colors));
    let h_groups = alloc_upload(bytemuck::cast_slice(&instance_inputs.groups));

    BufferAllocationResult {
        buffers: ChainBuffers {
            h_bytes: Some(h_bytes),
            h_bi: Some(h_bi),
            h_bm: Some(h_bm),
            h_bc: Some(h_bc),
            h_seq: Some(h_seq),
            h_bmap: Some(h_bmap),
            h_poff: Some(h_poff),
            h_pval: Some(h_pval),
            h_ir,
            h_ic: Some(h_ic),
            h_ie: Some(h_ie),
            h_im: Some(h_im),
            h_gap: Some(h_gap),
            h_plan: Some(h_plan),
            h_rmax: Some(h_rmax),
            h_xmax: Some(h_xmax),
            h_extent: Some(h_extent),
            bi_len,
            bm_len,
            bc_len,
            bshift,
            h_fl,
            h_sm,
            h_gi,
            h_hgt,
            h_cslot: Some(h_cslot),
            h_cend: Some(h_cend),
            h_tc: Some(h_tc),
            h_tm: Some(h_tm),
            h_xc: Some(h_xc),
            h_xm: Some(h_xm),
            h_lc: Some(h_lc),
            h_wm: Some(h_wm),
            h_wc,
            h_otb: Some(h_otb),
            h_lm,
            h_ctc: Some(h_ctc),
            h_cup: Some(h_cup),
            h_cxc: Some(h_cxc),
            h_ctotal: Some(h_ctotal),
            h_hp: Some(h_hp),
            h_ltc: Some(h_ltc),
            h_stc: Some(h_stc),
            h_lup: Some(h_lup),
            h_sup: Some(h_sup),
            h_lxc: Some(h_lxc),
            h_sxc: Some(h_sxc),
            h_lgrand: Some(h_lgrand),
            h_sgrand: Some(h_sgrand),
            h_totals: Some(h_totals),
            h_ext,
            h_pr_colors,
            h_color_base,
            h_is_pr,
            h_flat_colors,
            h_groups,
        },
        live_bytes: live.get(),
    }
}
