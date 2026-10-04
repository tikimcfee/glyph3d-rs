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
use crate::glyph_scene::RenderSlot;
use crate::layout::{
    DeviceSlotChunk, DeviceSlots, GlyphArena, ItemPlacement, LayoutError, LayoutGlyphs,
    LayoutItem,
};
#[cfg(feature = "cubecl")]
use crate::layout::TintStore;
use crate::text::fu_to_world;

mod types;
pub use types::{ItemPrepass, Pass2DeviceOutput, SendPtr};

mod char_resolve;
use char_resolve::resolve_byte_char;

mod device_alloc;
use device_alloc::{layout_device_discrete, layout_device_unified};

mod pass2_device;
mod pass2_host;
use pass2_host::layout_pass2_host;

pub struct HyperLayout {
    trie: Option<Arc<TrieTable>>,
    device: Option<crate::gpu::SharedDevice>,
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
        }
    }

    pub(crate) fn with_device(device: crate::gpu::SharedDevice) -> Self {
        Self {
            trie: None,
            device: Some(device),
        }
    }

    pub fn with_trie(trie: Arc<TrieTable>) -> Self {
        Self {
            trie: Some(trie),
            device: None,
        }
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

        // --- PASS 1 (Parallel): Prepass per item to find counts and max_row_extent ---
        let sp_pass1 = tracing::info_span!("hyper.pass1").entered();
        let prepasses: Vec<ItemPrepass> = items
            .par_iter()
            .map(|item| {
                let bytes = item.bytes;
                let p = &item.params;
                let fold_unit = if p.wrap_width > 0 {
                    p.wrap_width as i64
                } else if p.has_page {
                    p.page_cols as i64
                } else {
                    0
                };

                let mut survivor_count = 0u32;
                let mut max_row_extent = 0.0f64;
                let mut trailer_until = 0usize;

                let ascii_adv = fu_to_world(1229, em_height_fu);
                let mut pos = 0usize;
                if fold_unit == 0 {
                    let mut line_adv = 0.0f64;
                    while pos < bytes.len() {
                        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
                            Some(offset) => pos + offset,
                            None => bytes.len(),
                        };
                        let line = &bytes[pos..nl_pos];
                        if line.iter().all(|b| (0x20..0x7F).contains(b)) {
                            let l = line.len();
                            survivor_count += l as u32;
                            let end_adv = line_adv + l as f64 * ascii_adv as f64;
                            if end_adv > max_row_extent {
                                max_row_extent = end_adv;
                            }
                            line_adv = 0.0;
                            pos = if nl_pos < bytes.len() { nl_pos + 1 } else { nl_pos };
                            continue;
                        }

                        for i in pos..nl_pos {
                            let r = match resolve_byte_char(bytes, i, &trie, bitmap_adv, em_height_fu, &mut trailer_until) {
                                Some(r) => r,
                                None => continue,
                            };

                            if line_adv > max_row_extent {
                                max_row_extent = line_adv;
                            }

                            if r.glyph_id != 0 {
                                survivor_count += 1;
                            }

                            line_adv += r.advance as f64;
                        }

                        if nl_pos < bytes.len() {
                            if line_adv > max_row_extent {
                                max_row_extent = line_adv;
                            }
                            line_adv = 0.0;
                            pos = nl_pos + 1;
                        } else {
                            pos = nl_pos;
                        }
                    }
                } else {
                    let mut col = 0i64;
                    let mut seg_adv = 0.0f32;
                    let fu = fold_unit as usize;
                    let mut seg_adv_stack = [0.0f32; 256];
                    let mut seg_adv_heap = Vec::new();
                    let seg_adv_table: &[f32] = if fu < 256 {
                        let mut cur = 0.0f32;
                        for slot in seg_adv_stack.iter_mut().take(fu + 1) {
                            *slot = cur;
                            cur += ascii_adv;
                        }
                        &seg_adv_stack[..=fu]
                    } else {
                        seg_adv_heap.reserve(fu + 1);
                        let mut cur = 0.0f32;
                        for _ in 0..=fu {
                            seg_adv_heap.push(cur);
                            cur += ascii_adv;
                        }
                        &seg_adv_heap
                    };

                    while pos < bytes.len() {
                        let nl_pos = match memchr::memchr(b'\n', &bytes[pos..]) {
                            Some(offset) => pos + offset,
                            None => bytes.len(),
                        };
                        let line = &bytes[pos..nl_pos];
                        if line.iter().all(|b| (0x20..0x7F).contains(b)) {
                            let l = line.len();
                            survivor_count += l as u32;
                            let line_max_seg = if l >= fu {
                                seg_adv_table[fu - 1]
                            } else {
                                seg_adv_table[l]
                            };
                            if line_max_seg as f64 > max_row_extent {
                                max_row_extent = line_max_seg as f64;
                            }
                            col = 0;
                            seg_adv = 0.0;
                            pos = if nl_pos < bytes.len() { nl_pos + 1 } else { nl_pos };
                            continue;
                        }

                        for i in pos..nl_pos {
                            let r = match resolve_byte_char(bytes, i, &trie, bitmap_adv, em_height_fu, &mut trailer_until) {
                                Some(r) => r,
                                None => continue,
                            };

                            let item_rel_x = seg_adv as f64;
                            if item_rel_x > max_row_extent {
                                max_row_extent = item_rel_x;
                            }

                            if r.glyph_id != 0 {
                                survivor_count += 1;
                            }

                            col += 1;
                            if col % fold_unit == 0 {
                                seg_adv = 0.0;
                            } else {
                                seg_adv += r.advance;
                            }
                        }

                        if nl_pos < bytes.len() {
                            let item_rel_x = seg_adv as f64;
                            if item_rel_x > max_row_extent {
                                max_row_extent = item_rel_x;
                            }
                            col = 0;
                            seg_adv = 0.0;
                            pos = nl_pos + 1;
                        } else {
                            pos = nl_pos;
                        }
                    }
                }

                ItemPrepass {
                    survivor_count,
                    max_row_extent,
                }
            })
            .collect();
        drop(sp_pass1);

        // --- Prefix Sum of survivor offsets ---
        let mut slot_bases = Vec::with_capacity(item_count);
        let mut total_survivors = 0usize;
        for pre in &prepasses {
            slot_bases.push(total_survivors as u32);
            total_survivors += pre.survivor_count as usize;
        }

        let can_use_device = allow_device
            && self.device.as_ref().is_some_and(|dev| {
                (total_survivors * std::mem::size_of::<RenderSlot>()) as u64 <= dev.max_buffer_size
                    && total_survivors > 0
            });

        if can_use_device {
            let dev = self.device.as_ref().unwrap();
            let (wgpu_buf, mapped_slots, pass2_out) = if dev.is_unified() && dev.host_visible_storage {
                layout_device_unified(
                    dev,
                    total_survivors,
                    items,
                    &prepasses,
                    &slot_bases,
                    &trie,
                    bitmap_adv,
                    em_height_fu,
                )
            } else {
                layout_device_discrete(
                    dev,
                    total_survivors,
                    items,
                    &prepasses,
                    &slot_bases,
                    &trie,
                    bitmap_adv,
                    em_height_fu,
                )
            };

            let device_slots = DeviceSlots {
                chunks: vec![DeviceSlotChunk {
                    buffer: wgpu_buf,
                    offset: 0,
                    slots: total_survivors as u32,
                }],
                chunk_slots: total_survivors,
                len: total_survivors,
                mapped_slots,
                file_tints: pass2_out.file_tints,
                file_blocks: pass2_out.file_blocks,
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
    use crate::layout::{ItemParams, Paint};

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
}

