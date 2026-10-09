//! layout_hyper.rs — Ultra-fast parallel pure-Rust layout engine.
//!
//! Direct-to-arena streaming layout with Rayon parallelism:
//! - Zero SoA 52 B/byte intermediate heap allocations
//! - Zero intermediate GlyphRecord wire materialization
//! - Cache-resident register evaluation of positions, wrap, and pagination
//! - Parallel execution across files

use std::path::Path;
use std::sync::Arc;
use rayon::prelude::*;

use crate::atlas::TrieTable;
use crate::fold::rows_for_line;
use crate::layout::{
    DerivedDeviceSlots, DeviceSlots, GlyphArena, ItemPlacement, LayoutError, LayoutGlyphs,
    LayoutItem,
};
#[cfg(feature = "cubecl")]
use crate::layout::TintStore;
use crate::text::fu_to_world;
use glyph_field::GlyphFieldMode;

mod types;
pub use types::{ChunkPrepass, ItemPrepass, Pass2DeviceOutput, SendPtr};

pub(crate) mod chunk;
pub(crate) mod char_resolve;
use char_resolve::resolve_byte_char_cluster;

mod device_alloc;
use device_alloc::{layout_device_discrete, layout_device_unified, DeviceEmission, EmitInputs};

mod pass2_device;
use pass2_device::{DerivedEmit, RenderEmit};
mod pass2_host;
pub use pass2_host::{compute_single_item_placement, layout_pass2_host, scan_item_max_row_extent};

pub struct HyperLayout {
    trie: Option<Arc<TrieTable>>,
    device: Option<crate::gpu::SharedDevice>,
    /// Whose slot format the device path emits. Host (no-device) runs
    /// always produce the neutral 48 B records regardless.
    field_mode: GlyphFieldMode,
    pub(crate) prefetched_data: Option<PrefetchedHyperData>,
}

impl Default for HyperLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperLayout {
    pub fn new() -> Self {
        Self {
            trie: None,
            device: None,
            field_mode: GlyphFieldMode::Instanced,
            prefetched_data: None,
        }
    }

    pub(crate) fn with_device(device: crate::gpu::SharedDevice, field_mode: GlyphFieldMode) -> Self {
        Self {
            trie: None,
            device: Some(device),
            field_mode,
            prefetched_data: None,
        }
    }

    pub fn with_trie(trie: Arc<TrieTable>) -> Self {
        Self {
            trie: Some(trie),
            device: None,
            field_mode: GlyphFieldMode::Instanced,
            prefetched_data: None,
        }
    }

    pub fn set_prefetched(&mut self, data: PrefetchedHyperData) {
        self.prefetched_data = Some(data);
    }
}

impl LayoutGlyphs for HyperLayout {
    fn name(&self) -> &'static str {
        "hyper-rust"
    }

    fn load_trie_file(&mut self, _path: &Path) -> Result<(), LayoutError> {
        self.trie = Some(crate::default_trie());
        Ok(())
    }

    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        self.layout_items_internal(items, arena, true)
    }
}

impl HyperLayout {
    fn layout_items_internal(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
        allow_device: bool,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        let trie = match &self.trie {
            Some(t) => Arc::clone(t),
            None => {
                let arc = crate::default_trie();
                self.trie = Some(Arc::clone(&arc));
                arc
            }
        };

        let item_count = items.len();
        if item_count == 0 {
            return Ok(Vec::new());
        }

        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);

        // --- PASS 1 (Parallel): Prepass per chunk to find counts and max_row_extent ---
        let (chunks, item_chunk_ranges, agg) = if let Some(pre) = self.prefetched_data.take() {
            let chunks: Vec<chunk::LayoutChunk<'_>> = pre
                .chunk_defs
                .iter()
                .map(|def| chunk::LayoutChunk {
                    item_index: def.item_index,
                    bytes: &items[def.item_index].bytes[def.byte_offset..def.byte_offset + def.byte_len],
                    byte_offset: def.byte_offset,
                })
                .collect();
            (chunks, pre.item_chunk_ranges, pre.aggregate)
        } else {
            let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(items);
            let sp_pass1 = tracing::info_span!("hyper.pass1").entered();
            let agg = pass1_prepass_chunks(
                &chunks,
                &item_chunk_ranges,
                items,
                &trie,
                bitmap_adv,
                em_height_fu,
            );
            drop(sp_pass1);
            (chunks, item_chunk_ranges, agg)
        };

        let prepasses = agg.prepasses;
        let chunk_slot_bases = agg.chunk_slot_bases;
        let chunk_base_rows = agg.chunk_base_rows;
        let chunk_record_bases = agg.chunk_record_bases;
        let chunk_initial_cols = agg.chunk_initial_cols;
        let chunk_initial_seg_advs = agg.chunk_initial_seg_advs;
        let chunk_initial_line_advs = agg.chunk_initial_line_advs;
        let total_survivors = agg.total_survivors;

        let mut slot_bases = Vec::with_capacity(item_count);
        for range in &item_chunk_ranges {
            slot_bases.push(chunk_slot_bases[range.start]);
        }

        let derived = self.field_mode == GlyphFieldMode::Derived;
        let can_use_device = allow_device && self.device.is_some() && total_survivors > 0;

        if can_use_device {
            let dev = self.device.as_ref().unwrap();
            let unified = dev.is_unified() && dev.host_visible_storage;

            // Derived: each item owns `row_count` consecutive line-table
            // entries; their bases are known now, so Pass 2 writes final
            let inputs = EmitInputs {
                items,
                chunks: &chunks,
                item_chunk_ranges: &item_chunk_ranges,
                prepasses: &prepasses,
                slot_bases: &slot_bases,
                chunk_slot_bases: &chunk_slot_bases,
                chunk_base_rows: &chunk_base_rows,
                chunk_record_bases: &chunk_record_bases,
                chunk_initial_cols: &chunk_initial_cols,
                chunk_initial_seg_advs: &chunk_initial_seg_advs,
                chunk_initial_line_advs: &chunk_initial_line_advs,
                trie: &trie,
                bitmap_adv,
                em_height_fu,
            };

            let (emission, derived_extras) = if derived {
                let label = "derived glyph slots (direct)";
                // The table depends only on Pass 1; build it alongside Pass 2.
                let emission = if unified {
                    layout_device_unified::<DerivedEmit>(dev, total_survivors, &inputs, label)
                } else {
                    layout_device_discrete::<DerivedEmit>(dev, total_survivors, &inputs, label)
                };
                let extras = DerivedDeviceSlots { mapped_base: emission.mapped_base };
                (emission, Some(extras))
            } else {
                let label = "glyph render slots (direct)";
                let emission = if unified {
                    layout_device_unified::<RenderEmit>(dev, total_survivors, &inputs, label)
                } else {
                    layout_device_discrete::<RenderEmit>(dev, total_survivors, &inputs, label)
                };
                (emission, None)
            };
            let DeviceEmission { chunks, chunk_slots, mapped_base, pass2: pass2_out, emoji_tint_pairs } = emission;

            let device_slots = DeviceSlots {
                chunks,
                chunk_slots,
                len: total_survivors,
                // RenderSlot-typed readers key off this; never hand them a
                // DerivedSlot mapping.
                mapped_slots: if derived { None } else { mapped_base },
                file_tints: pass2_out.file_tints,
                file_blocks: pass2_out.file_blocks,
                format: self.field_mode,
                derived: derived_extras,
                emoji_tint_pairs,
                #[cfg(feature = "cubecl")]
                tint: TintStore::Host(Vec::new()),
                #[cfg(feature = "cubecl")]
                keep_alive: Vec::new(),
            };
            *arena = GlyphArena::from_device(device_slots);
            Ok(pass2_out.placements)
        } else {
            let (tail_ptr, capacity) = arena.uninit_tail(total_survivors);
            assert!(capacity >= total_survivors);
            let placements = layout_pass2_host(
                items,
                &prepasses,
                &slot_bases,
                &trie,
                bitmap_adv,
                em_height_fu,
                SendPtr(tail_ptr),
            );
            unsafe {
                arena.commit(total_survivors);
            }
            Ok(placements)
        }
    }
}


/// Prepass aggregation output holding per-item prepasses and per-chunk starting offsets.
#[derive(Clone, Debug)]
pub struct PrepassAggregate {
    pub prepasses: Vec<ItemPrepass>,
    pub chunk_slot_bases: Vec<u32>,
    pub chunk_base_rows: Vec<i64>,
    pub chunk_record_bases: Vec<usize>,
    pub chunk_initial_cols: Vec<i64>,
    pub chunk_initial_seg_advs: Vec<f32>,
    pub chunk_initial_line_advs: Vec<f64>,
    pub total_survivors: usize,
}

/// Precomputed chunks and Pass 1 metadata from background prefetch.
#[derive(Clone, Debug)]
pub struct PrefetchedHyperData {
    pub chunk_defs: Vec<chunk::ChunkDef>,
    pub item_chunk_ranges: Vec<std::ops::Range<usize>>,
    pub aggregate: PrepassAggregate,
}

/// Evaluates Pass 1 on a single byte slice (whole file or chunk).
pub(crate) fn pass1_prepass_chunk_bytes(
    bytes: &[u8],
    p: &crate::layout::ItemParams,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    global_seg_adv_table: &[f32],
) -> ChunkPrepass {
    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };

    let mut survivor_count = 0u32;
    let mut leader_count = 0u32;
    let mut max_row_extent = 0.0f64;
    let mut trailer_until = 0usize;
    let mut has_cluster = false;
    let wrap_w = p.wrap_width as i64;
    let ascii_adv = fu_to_world(1229, em_height_fu);

    let fu = fold_unit as usize;
    let seg_adv_table: &[f32] = if fu > 0 && fu < global_seg_adv_table.len() {
        &global_seg_adv_table[..=fu]
    } else {
        &[]
    };

    let has_newline = memchr::memchr(b'\n', bytes).is_some();

    if !has_newline {
        let mut col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;

        let is_pure_ascii = crate::text::is_pure_printable_ascii(bytes);
        if is_pure_ascii {
            let l = bytes.len();
            survivor_count = l as u32;
            leader_count = l as u32;
            col = l as i64;
            line_adv = l as f64 * ascii_adv as f64;
            if fold_unit > 0 {
                let rem = l % fu;
                seg_adv = seg_adv_table[rem];
                let max_seg = if l >= fu { seg_adv_table[fu - 1] } else { seg_adv_table[l] };
                if max_seg as f64 > max_row_extent {
                    max_row_extent = max_seg as f64;
                }
            } else if line_adv > max_row_extent {
                max_row_extent = line_adv;
            }
        } else {
            for i in 0..bytes.len() {
                let r = match resolve_byte_char_cluster(bytes, i, trie, bitmap_adv, em_height_fu, &mut trailer_until, &mut has_cluster) {
                    Some(r) => r,
                    None => continue,
                };
                leader_count += 1;
                if r.glyph_id != 0 {
                    survivor_count += 1;
                }
                col += 1;
                line_adv += r.advance as f64;
                if fold_unit > 0 {
                    if col % fold_unit == 0 {
                        seg_adv = 0.0;
                    } else {
                        seg_adv += r.advance;
                    }
                    if (seg_adv as f64) > max_row_extent {
                        max_row_extent = seg_adv as f64;
                    }
                } else if line_adv > max_row_extent {
                    max_row_extent = line_adv;
                }
            }
        }

        return ChunkPrepass {
            survivor_count,
            leader_count,
            max_row_extent,
            has_cluster,
            completed_rows: 0,
            has_newline: false,
            delta_col: col,
            delta_line_adv: line_adv,
            delta_seg_adv: seg_adv,
            first_seg_col: 0,
            last_seg_col: 0,
            last_seg_line_adv: 0.0,
            last_seg_seg_adv: 0.0,
        };
    }

    let mut pos = 0usize;
    let mut line_index = 0usize;
    let mut completed_rows = 0u32;
    let mut first_seg_col = 0i64;
    let mut last_seg_col = 0i64;
    let mut last_seg_line_adv = 0.0f64;
    let mut last_seg_seg_adv = 0.0f32;

    while pos < bytes.len() {
        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
            Some(offset) => pos + offset,
            None => bytes.len(),
        };
        let line = &bytes[pos..nl_pos];
        let mut line_survivors = 0u32;
        let mut line_leaders = 0u32;
        let mut line_col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;

        let is_pure_ascii = crate::text::is_pure_printable_ascii(line);
        if is_pure_ascii {
            let l = line.len();
            line_survivors = l as u32;
            line_leaders = l as u32;
            line_col = l as i64;
            line_adv = l as f64 * ascii_adv as f64;
            if fold_unit > 0 {
                let rem = l % fu;
                seg_adv = seg_adv_table[rem];
                let line_max_seg = if l >= fu { seg_adv_table[fu - 1] } else { seg_adv_table[l] };
                if line_max_seg as f64 > max_row_extent {
                    max_row_extent = line_max_seg as f64;
                }
            } else if line_adv > max_row_extent {
                max_row_extent = line_adv;
            }
        } else {
            for i in pos..nl_pos {
                let r = match resolve_byte_char_cluster(bytes, i, trie, bitmap_adv, em_height_fu, &mut trailer_until, &mut has_cluster) {
                    Some(r) => r,
                    None => continue,
                };
                line_leaders += 1;
                if r.glyph_id != 0 {
                    line_survivors += 1;
                }
                line_col += 1;
                line_adv += r.advance as f64;
                if fold_unit > 0 {
                    if line_col % fold_unit == 0 {
                        seg_adv = 0.0;
                    } else {
                        seg_adv += r.advance;
                    }
                    if (seg_adv as f64) > max_row_extent {
                        max_row_extent = seg_adv as f64;
                    }
                } else if line_adv > max_row_extent {
                    max_row_extent = line_adv;
                }
            }
        }

        survivor_count += line_survivors;

        if nl_pos < bytes.len() {
            line_leaders += 1;
            leader_count += line_leaders;

            if line_index == 0 {
                first_seg_col = line_col;
            } else {
                completed_rows += rows_for_line(line_col, wrap_w, p.wrap_mode) as u32;
            }
            pos = nl_pos + 1;
            line_index += 1;
        } else {
            leader_count += line_leaders;
            last_seg_col = line_col;
            last_seg_line_adv = line_adv;
            last_seg_seg_adv = seg_adv;
            break;
        }
    }

    ChunkPrepass {
        survivor_count,
        leader_count,
        max_row_extent,
        has_cluster,
        completed_rows,
        has_newline: true,
        delta_col: 0,
        delta_line_adv: 0.0,
        delta_seg_adv: 0.0,
        first_seg_col,
        last_seg_col,
        last_seg_line_adv,
        last_seg_seg_adv,
    }
}

/// Evaluates Pass 1 across all chunks in parallel and aggregates per-item results.
pub(crate) fn pass1_prepass_chunks(
    chunks: &[chunk::LayoutChunk<'_>],
    item_chunk_ranges: &[std::ops::Range<usize>],
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> PrepassAggregate {
    let max_fold_unit = items
        .iter()
        .map(|it| {
            let p = &it.params;
            if p.wrap_width > 0 {
                p.wrap_width as usize
            } else if p.has_page {
                p.page_cols as usize
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0);

    let ascii_adv = fu_to_world(1229, em_height_fu);
    let mut global_seg_adv_table = Vec::with_capacity(max_fold_unit + 1);
    if max_fold_unit > 0 {
        let mut cur = 0.0f32;
        for _ in 0..=max_fold_unit {
            global_seg_adv_table.push(cur);
            cur += ascii_adv;
        }
    }

    let chunk_prepasses: Vec<ChunkPrepass> = chunks
        .par_iter()
        .map(|chunk| {
            let item = &items[chunk.item_index];
            pass1_prepass_chunk_bytes(
                chunk.bytes,
                &item.params,
                trie,
                bitmap_adv,
                em_height_fu,
                &global_seg_adv_table,
            )
        })
        .collect();

    let file_params: Vec<crate::layout::ItemParams> = items.iter().map(|it| it.params).collect();
    aggregate_chunk_prepasses(
        &chunk_prepasses,
        item_chunk_ranges,
        chunks.len(),
        &file_params,
        em_height_fu,
    )
}

pub(crate) fn aggregate_chunk_prepasses(
    chunk_prepasses: &[ChunkPrepass],
    item_chunk_ranges: &[std::ops::Range<usize>],
    total_chunks: usize,
    file_params: &[crate::layout::ItemParams],
    em_height_fu: u32,
) -> PrepassAggregate {
    let mut prepasses = Vec::with_capacity(file_params.len());
    let mut chunk_slot_bases = Vec::with_capacity(total_chunks);
    let mut chunk_base_rows = Vec::with_capacity(total_chunks);
    let mut chunk_record_bases = Vec::with_capacity(total_chunks);
    let mut chunk_initial_cols = Vec::with_capacity(total_chunks);
    let mut chunk_initial_seg_advs = Vec::with_capacity(total_chunks);
    let mut chunk_initial_line_advs = Vec::with_capacity(total_chunks);
    let mut total_survivors = 0usize;

    for (item_idx, range) in item_chunk_ranges.iter().enumerate() {
        let p = &file_params[item_idx];
        let wrap_w = p.wrap_width as i64;
        let fold_unit = if p.wrap_width > 0 {
            p.wrap_width as i64
        } else if p.has_page {
            p.page_cols as i64
        } else {
            0
        };
        let ascii_adv = fu_to_world(1229, em_height_fu);

        let mut item_survivor_count = 0u32;
        let mut item_leader_count = 0u32;
        let mut item_max_row_extent = 0.0f64;
        let mut item_has_cluster = false;

        let mut cur_base_row = 0i64;
        let mut cur_record_base = 0usize;
        let mut cur_col = 0i64;
        let mut cur_line_adv = 0.0f64;
        let mut cur_seg_adv = 0.0f32;

        for chunk_index in range.clone() {
            let cp = &chunk_prepasses[chunk_index];

            chunk_slot_bases.push(total_survivors as u32);
            chunk_base_rows.push(cur_base_row);
            chunk_record_bases.push(cur_record_base);
            chunk_initial_cols.push(cur_col);
            chunk_initial_seg_advs.push(cur_seg_adv);
            chunk_initial_line_advs.push(cur_line_adv);

            total_survivors += cp.survivor_count as usize;
            cur_record_base += cp.leader_count as usize;

            item_survivor_count += cp.survivor_count;
            item_leader_count += cp.leader_count;
            if cp.max_row_extent > item_max_row_extent {
                item_max_row_extent = cp.max_row_extent;
            }
            if cp.has_cluster {
                item_has_cluster = true;
            }

            if !cp.has_newline {
                cur_col += cp.delta_col;
                cur_line_adv += cp.delta_line_adv;
                if fold_unit > 0 {
                    let rem = (cur_col % fold_unit) as usize;
                    cur_seg_adv = rem as f32 * ascii_adv;
                } else {
                    cur_seg_adv += cp.delta_seg_adv;
                }
            } else {
                let first_line_total = cur_col + cp.first_seg_col;
                cur_base_row += rows_for_line(first_line_total, wrap_w, p.wrap_mode);
                cur_base_row += cp.completed_rows as i64;

                cur_col = cp.last_seg_col;
                cur_line_adv = cp.last_seg_line_adv;
                cur_seg_adv = cp.last_seg_seg_adv;
            }
        }

        if cur_col > 0 {
            cur_base_row += rows_for_line(cur_col, wrap_w, p.wrap_mode);
        }

        prepasses.push(ItemPrepass {
            survivor_count: item_survivor_count,
            leader_count: item_leader_count,
            max_row_extent: item_max_row_extent,
            row_count: u32::try_from(cur_base_row).expect("item row count exceeds u32"),
            has_cluster: item_has_cluster,
        });
    }

    PrepassAggregate {
        prepasses,
        chunk_slot_bases,
        chunk_base_rows,
        chunk_record_bases,
        chunk_initial_cols,
        chunk_initial_seg_advs,
        chunk_initial_line_advs,
        total_survivors,
    }
}

/// Computes Pass 1 chunk prepasses ahead of time during repository prefetching.
pub fn prefetch_hyper(
    files: &[crate::repo::RepoFile],
    file_params: &[crate::layout::ItemParams],
) -> PrefetchedHyperData {
    let trie = crate::default_trie();
    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);

    let byte_slices: Vec<&[u8]> = files.iter().map(|f| f.bytes.as_slice()).collect();
    let (chunk_defs, item_chunk_ranges) = chunk::slice_byte_buffers_into_chunk_defs(&byte_slices);

    let max_fold_unit = file_params
        .iter()
        .map(|p| {
            if p.wrap_width > 0 {
                p.wrap_width as usize
            } else if p.has_page {
                p.page_cols as usize
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0);

    let ascii_adv = fu_to_world(1229, em_height_fu);
    let mut global_seg_adv_table = Vec::with_capacity(max_fold_unit + 1);
    if max_fold_unit > 0 {
        let mut cur = 0.0f32;
        for _ in 0..=max_fold_unit {
            global_seg_adv_table.push(cur);
            cur += ascii_adv;
        }
    }

    let chunk_prepasses: Vec<ChunkPrepass> = chunk_defs
        .par_iter()
        .map(|def| {
            let p = &file_params[def.item_index];
            let chunk_bytes = &files[def.item_index].bytes[def.byte_offset..def.byte_offset + def.byte_len];
            pass1_prepass_chunk_bytes(
                chunk_bytes,
                p,
                &trie,
                bitmap_adv,
                em_height_fu,
                &global_seg_adv_table,
            )
        })
        .collect();

    let aggregate = aggregate_chunk_prepasses(
        &chunk_prepasses,
        &item_chunk_ranges,
        chunk_defs.len(),
        file_params,
        em_height_fu,
    );

    PrefetchedHyperData {
        chunk_defs,
        item_chunk_ranges,
        aggregate,
    }
}

/// PASS 1 (parallel): per item, survivor count, widest row extent, and the
/// FOLDED row count (the Derived line table's size, known before Pass 2).
pub(crate) fn pass1_prepass(
    items: &[LayoutItem<'_>],
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
) -> Vec<ItemPrepass> {
    let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(items);
    let aggregate = pass1_prepass_chunks(
        &chunks,
        &item_chunk_ranges,
        items,
        trie,
        bitmap_adv,
        em_height_fu,
    );
    aggregate.prepasses
}


impl crate::layout::VerifyLayout for HyperLayout {
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<crate::layout::GlyphRecord>), LayoutError> {
        let placements = self.layout_items_internal(items, arena, false)?;
        let trie = match &self.trie {
            Some(t) => Arc::clone(t),
            None => {
                let arc = crate::default_trie();
                self.trie = Some(Arc::clone(&arc));
                arc
            }
        };
        let mut all_records = Vec::new();
        for item in items {
            all_records.extend(rederive_item_records(item.bytes, &item.params, &trie));
        }
        Ok((placements, all_records))
    }
}

mod rederive;
pub use rederive::{rederive_item_records, resolve_spans_to_slot_colors};


#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph_scene::RenderSlot;
    use crate::layout::{ItemParams, Paint};
    use glyph_field_derived::DerivedSlot;

    #[test]
    fn hyper_and_batched_agree_on_samples() {
        const SAMPLES: [&[u8]; 3] = [
            b"fn main() {\n    let x = 1;\n}\n",
            b"no trailing newline",
            b"   \t   \n\n   \n",
        ];
        let colors: Vec<Vec<u32>> =
            SAMPLES.iter().map(|b| crate::text::colorize_leaders(b)).collect();
        let items: Vec<LayoutItem<'_>> = SAMPLES
            .iter()
            .enumerate()
            .map(|(i, bytes)| LayoutItem {
                bytes,
                params: ItemParams { line_height: 1.25, ..Default::default() },
                group_id: i as u32,
                paint: Paint::PerRecord(&colors[i]),
            })
            .collect();

        let mut hyper = HyperLayout::new();
        let trie_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin");
        hyper.load_trie_file(&trie_path).expect("hyper trie");
        let mut hyper_arena = GlyphArena::new();
        let hyper_places = hyper
            .layout_items(&items, &mut hyper_arena)
            .expect("hyper layout");

        assert_eq!(hyper_places.len(), 3);
        assert_eq!(hyper_places[0].record_count, 29);
        assert_eq!(hyper_places[0].slot_count, 26);
        assert_eq!(hyper_places[0].slot_base, 0);

        assert_eq!(hyper_places[1].record_count, 19);
        assert_eq!(hyper_places[1].slot_count, 19);
        assert_eq!(hyper_places[1].slot_base, 26);

        assert_eq!(hyper_places[2].record_count, 13);
        assert_eq!(hyper_places[2].slot_count, 9);
        assert_eq!(hyper_places[2].slot_base, 45);
    }

    #[test]
    fn hyper_byte_spans_painting() {
        use crate::layout::ByteSpan;
        let text = b"fn main() {\n    let x = 42;\n}\n";
        let spans = [
            ByteSpan { start: 0, end: 2, color: 0x1111_1111 },  // "fn"
            ByteSpan { start: 16, end: 19, color: 0x2222_2222 }, // "let"
            ByteSpan { start: 24, end: 26, color: 0x3333_3333 }, // "42"
        ];
        let item = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::ByteSpans(&spans),
        };
        let mut hyper = HyperLayout::new();
        let trie_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin");
        hyper.load_trie_file(&trie_path).expect("hyper trie");
        let mut arena = GlyphArena::new();
        let places = hyper
            .layout_items(&[item], &mut arena)
            .expect("hyper layout");
        assert_eq!(places.len(), 1);
        let instances = arena.instances();
        // Instance 0 is 'f', 1 is 'n': color should be 0x1111_1111
        assert_eq!(instances[0].color, 0x1111_1111);
        assert_eq!(instances[1].color, 0x1111_1111);
        // ' ' is dropped/blank, 'm' is default color
        assert_eq!(instances[2].color, crate::layout::DEFAULT_COLOR_PACKED);
    }

    #[test]
    fn resolve_spans_and_inplace_update_slot_colors() {
        use crate::layout::ByteSpan;
        let text = b"fn main() {\n    let x = 42;\n}\n";
        let spans = [
            ByteSpan { start: 0, end: 2, color: 0x1111_1111 },  // "fn"
            ByteSpan { start: 16, end: 19, color: 0x2222_2222 }, // "let"
            ByteSpan { start: 24, end: 26, color: 0x3333_3333 }, // "42"
        ];
        let trie_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin");
        let mut hyper = HyperLayout::new();
        hyper.load_trie_file(&trie_path).expect("hyper trie");
        let trie = crate::default_trie();

        // 1. Layout directly with Paint::ByteSpans
        let item_spanned = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::ByteSpans(&spans),
        };
        let mut arena_spanned = GlyphArena::new();
        let places_spanned = hyper
            .layout_items(&[item_spanned], &mut arena_spanned)
            .expect("layout spanned");

        // 2. Layout with Paint::Flat
        let item_flat = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED),
        };
        let mut arena_flat = GlyphArena::new();
        let places_flat = hyper
            .layout_items(&[item_flat], &mut arena_flat)
            .expect("layout flat");

        assert_eq!(places_flat[0].slot_count, places_spanned[0].slot_count);

        // 3. Resolve spans to slot colors
        let resolved = resolve_spans_to_slot_colors(text, &spans, &trie);
        assert_eq!(resolved.len(), places_flat[0].slot_count as usize);

        // Verify resolved colors exactly match the layout-time Paint::ByteSpans colors
        let spanned_colors: Vec<u32> = arena_spanned.instances().iter().map(|s| s.color).collect();
        assert_eq!(resolved, spanned_colors);

        // 4. Update flat arena in-place
        let updated = arena_flat.update_slot_colors(places_flat[0].slot_base as usize, &resolved);
        assert_eq!(updated, resolved.len());

        // Verify arena_flat now has identical colors to arena_spanned
        let flat_updated_colors: Vec<u32> = arena_flat.instances().iter().map(|s| s.color).collect();
        assert_eq!(flat_updated_colors, spanned_colors);
    }

    /// The direct Derived emission (Pass 2 writing 20 B `DerivedSlot`s with
    /// final `line_idx` from Pass 1's row counts) must agree slot for slot
    /// with the host path it replaced: 48 B records transcoded afterwards.
    /// Covers unfolded, WrapBack and WrapDown folds, a wrap-heavy line, an
    /// empty item, and an emoji (non-ASCII) item.
    #[test]
    fn derived_direct_emission_matches_host_transcode() {
        use bytemuck::Zeroable;
        use crate::fold::WrapMode;
        use crate::layout::{ItemParams, Paint};
        let long_line: Vec<u8> = b"0123456789abcdefghij".repeat(40);
        let mut wrapped = b"short\n".to_vec();
        wrapped.extend_from_slice(&long_line);
        wrapped.extend_from_slice(b"\n\n  tail line\nno newline at end");
        let emoji = "smile \u{1F600} ok\nflag \u{1F1FA}\u{1F1F8} x\n".as_bytes().to_vec();
        let plain = b"fn main() {\n    let x = 1;\n}\n".to_vec();
        let bodies: Vec<(&[u8], ItemParams)> = vec![
            (&plain, ItemParams { line_height: 1.25, ..Default::default() }),
            (
                &wrapped,
                ItemParams {
                    line_height: 1.25,
                    wrap_width: 37,
                    wrap_mode: WrapMode::Back,
                    z_step: 0.15,
                    origin_z: 2.0,
                    ..Default::default()
                },
            ),
            (
                &wrapped,
                ItemParams {
                    line_height: 1.25,
                    wrap_width: 23,
                    wrap_mode: WrapMode::Down,
                    z_step: 0.15,
                    ..Default::default()
                },
            ),
            (b"", ItemParams { line_height: 1.25, ..Default::default() }),
            (&emoji, ItemParams { line_height: 1.25, wrap_width: 8, z_step: 0.1, ..Default::default() }),
        ];
        let items: Vec<LayoutItem<'_>> = bodies
            .iter()
            .enumerate()
            .map(|(i, (bytes, params))| LayoutItem {
                bytes,
                params: *params,
                group_id: i as u32,
                paint: Paint::Flat(0x1234_5678 + i as u32),
            })
            .collect();

        // Host path: neutral 48 B records.
        let mut hyper = HyperLayout::new();
        let mut arena = GlyphArena::new();
        let host_places = hyper.layout_items(&items, &mut arena).expect("host layout");
        let host = arena.instances();

        // Direct path: Pass 1 + DerivedEmit into a host Vec.
        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        let mut slot_bases = Vec::new();
        let mut slots_acc = 0u32;
        for p in &prepasses {
            slot_bases.push(slots_acc);
            slots_acc += p.survivor_count;
        }
        assert_eq!(slots_acc as usize, host.len());
        let mut direct = vec![DerivedSlot::zeroed(); host.len()];
        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };
        let (out, _pairs) = inputs.run::<DerivedEmit>(direct.as_mut_ptr() as usize);

        assert_eq!(out.placements.len(), host_places.len());
        for (a, b) in out.placements.iter().zip(&host_places) {
            assert_eq!((a.slot_base, a.slot_count), (b.slot_base, b.slot_count));
        }
        for (k, (d, h)) in direct.iter().zip(host).enumerate() {
            let p = &items[h.group_id as usize].params;
            let want_wrap = if p.z_step.abs() > 1e-6 {
                ((p.origin_z as f32 - h.pos[2]) / p.z_step as f32).round().max(0.0) as u32
            } else {
                0
            };
            assert_eq!(d.x.to_bits(), h.pos[0].to_bits(), "slot {k}: x");
            assert_eq!(d.glyph_and_wrap & 0xFFFF, h.glyph_id & 0xFFFF, "slot {k}: glyph");
            assert_eq!(d.glyph_and_wrap >> 16, want_wrap, "slot {k}: wrap segment");
            assert_eq!(d.color, h.color, "slot {k}: color");
            assert_eq!(d.group_id(), h.group_id, "slot {k}: group");
            assert_eq!(d.row, h.row, "slot {k}: row");
        }
    }

    #[test]
    fn block_cull_bounds_enclose_all_slots() {
        let mut sample = String::new();
        for line_index in 0..40 {
            let indent = if line_index % 3 == 0 { "                    " } else { "    " };
            let content = format!("let variable_{line_index} = compute_value_{line_index}(foo, bar, baz); // comment\n");
            sample.push_str(indent);
            sample.push_str(&content);
        }
        let bytes = sample.as_bytes();
        let items = [LayoutItem {
            bytes,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::SyntaxHeuristic,
        }];

        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        assert!(prepasses[0].survivor_count > crate::glyph_scene::SUBSEG_BLOCK_SIZE as u32);

        let mut slot_bases = Vec::new();
        let mut slots_acc = 0u32;
        for p in &prepasses {
            slot_bases.push(slots_acc);
            slots_acc += p.survivor_count;
        }
        let mut direct: Vec<RenderSlot> = vec![bytemuck::Zeroable::zeroed(); slots_acc as usize];
        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };
        let (out, _pairs) = inputs.run::<RenderEmit>(direct.as_mut_ptr() as usize);
        assert!(!out.file_blocks.is_empty());
        let blocks = &out.file_blocks[0];
        assert!(blocks.len() >= 2, "must have multiple blocks for > 512 survivors, got {}", blocks.len());

        for (block_index, block) in blocks.iter().enumerate() {
            assert!(
                block.min[0] <= block.max[0],
                "block {block_index} has inverted X: min {} > max {}",
                block.min[0],
                block.max[0]
            );
            assert!(
                block.min[1] <= block.max[1],
                "block {block_index} has inverted Y: min {} > max {}",
                block.min[1],
                block.max[1]
            );
            assert!(
                block.min[2] <= block.max[2],
                "block {block_index} has inverted Z: min {} > max {}",
                block.min[2],
                block.max[2]
            );

            let start = block.slot_base as usize;
            let end = (block.slot_base + block.slot_count) as usize;
            for (slot_idx, slot) in direct[start..end].iter().enumerate() {
                assert!(
                    slot.pos[0] >= block.min[0] - 1e-4,
                    "block {block_index}, slot {slot_idx}: x {} < min_x {}",
                    slot.pos[0],
                    block.min[0]
                );
                assert!(
                    slot.pos[0] <= block.max[0] + 1e-4,
                    "block {block_index}, slot {slot_idx}: x {} > max_x {}",
                    slot.pos[0],
                    block.max[0]
                );
                let half_h = 0.5 * slot.height;
                assert!(
                    slot.pos[1] - half_h >= block.min[1] - 1e-4,
                    "block {block_index}, slot {slot_idx}: y_lo {} < min_y {}",
                    slot.pos[1] - half_h,
                    block.min[1]
                );
                assert!(
                    slot.pos[1] + half_h <= block.max[1] + 1e-4,
                    "block {block_index}, slot {slot_idx}: y_hi {} > max_y {}",
                    slot.pos[1] + half_h,
                    block.max[1]
                );
            }
        }
    }

    #[test]
    fn hyper_device_chunking_splits_and_binds_across_limits() {
        use glyph_field::{FieldResources, FieldTargets, GlyphField, SlotSource};
        use glyph_field_derived::DerivedField;

        let ctx = pollster::block_on(crate::gpu::init(None));
        let dev = crate::gpu::SharedDevice::from_ctx(&ctx);

        let text = b"line 1: hello world\nline 2: testing chunked emission\nline 3: across buffer boundaries\n";
        let item = LayoutItem {
            bytes: text,
            params: ItemParams { line_height: 1.25, ..Default::default() },
            group_id: 0,
            paint: Paint::Flat(0xFFFFFFFF),
        };

        let trie = crate::default_trie();
        let em_height_fu = trie.metrics.em_height_fu;
        let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);
        let items = [item];
        let (chunks, item_chunk_ranges) = chunk::slice_items_into_chunks(&items);
        let agg = pass1_prepass_chunks(&chunks, &item_chunk_ranges, &items, &trie, bitmap_adv, em_height_fu);
        let prepasses = agg.prepasses;
        let slot_bases = vec![0u32];
        let total_survivors = prepasses[0].survivor_count as usize;
        assert!(total_survivors > 20);

        let inputs = EmitInputs {
            items: &items,
            chunks: &chunks,
            item_chunk_ranges: &item_chunk_ranges,
            prepasses: &prepasses,
            slot_bases: &slot_bases,
            chunk_slot_bases: &agg.chunk_slot_bases,
            chunk_base_rows: &agg.chunk_base_rows,
            chunk_record_bases: &agg.chunk_record_bases,
            chunk_initial_cols: &agg.chunk_initial_cols,
            chunk_initial_seg_advs: &agg.chunk_initial_seg_advs,
            chunk_initial_line_advs: &agg.chunk_initial_line_advs,
            trie: &trie,
            bitmap_adv,
            em_height_fu,
        };

        // Force chunk_cap = 16 slots to test multi-chunk emission on real GPU buffers
        let chunk_cap = 16;
        let emission = device_alloc::layout_device_discrete_chunked::<pass2_device::DerivedEmit>(
            &dev,
            total_survivors,
            &inputs,
            "test derived chunked",
            chunk_cap,
        );

        let expected_chunks = total_survivors.div_ceil(chunk_cap);
        assert_eq!(emission.chunks.len(), expected_chunks);
        assert_eq!(emission.chunk_slots, chunk_cap);

        let total_chunk_slots: u32 = emission.chunks.iter().map(|c| c.slots).sum();
        assert_eq!(total_chunk_slots as usize, total_survivors);

        // Verify DerivedField builds bind groups for all chunks without validation error
        let dummy_advances = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy advances"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let dummy_frame_uniform = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy frame uniform"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let dummy_group_table = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy group table"),
            size: 256,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let dummy_glyph_map = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy glyph map"),
            size: wgpu::Extent3d { width: 1024, height: 161, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_glyph_map_view = dummy_glyph_map.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_emoji_sheet = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy emoji sheet"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_emoji_sheet_view = dummy_emoji_sheet.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_emoji_sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor::default());

        let dummy_curves = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy curves"),
            size: wgpu::Extent3d { width: 1024, height: 161, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_curves_view = dummy_curves.create_view(&wgpu::TextureViewDescriptor::default());

        let dummy_params = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy params"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });

        let resources = FieldResources {
            frame_uniform: &dummy_frame_uniform,
            group_table: &dummy_group_table,
            glyph_map: &dummy_glyph_map_view,
            curves: &dummy_curves_view,
            params: &dummy_params,
            emoji_sheet: &dummy_emoji_sheet_view,
            emoji_sampler: &dummy_emoji_sampler,
            glyph_advances: &dummy_advances,
            item_params: &[],
        };

        let source = SlotSource::Device {
            chunk_capacity: emission.chunk_slots,
            chunks: &emission.chunks,
            glyph_count: total_survivors,
            mapped_base: emission.mapped_base,
        };

        let targets = FieldTargets {
            color_format: wgpu::TextureFormat::Rgba8UnormSrgb,
            depth_format: wgpu::TextureFormat::Depth32Float,
            sample_count: 1,
        };

        let field = DerivedField::new(&ctx.device, &ctx.queue, source, &resources, targets);
        assert_eq!(field.chunk_count(), expected_chunks as u32);
        assert_eq!(field.chunk_capacity(), chunk_cap as u32);
        assert_eq!(field.glyph_count(), total_survivors as u32);
    }
}

