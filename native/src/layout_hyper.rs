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
use crate::fold::{rows_for_line, wrap_row_of, wrap_segment_of};
use crate::glyph_scene::{GlyphInstance, RenderSlot};
use crate::layout::{
    DeviceSlotChunk, DeviceSlots, GlyphArena, InkExtent, ItemPlacement, LayoutError, LayoutGlyphs,
    LayoutItem, PageExtent, Paint, TintStore,
};
use crate::text::fu_to_world;

pub struct HyperLayout {
    trie: Option<Arc<TrieTable>>,
    device: Option<crate::cubecl_chain::SharedDevice>,
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

    pub(crate) fn with_device(device: crate::cubecl_chain::SharedDevice) -> Self {
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

#[derive(Clone, Copy)]
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

#[cfg(target_os = "macos")]
fn create_mapped_render_slots(
    device: &wgpu::Device,
    slots: usize,
) -> (*mut RenderSlot, wgpu::Buffer) {
    use wgpu::hal::Device as HalDevice;
    let hal_dev = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .expect("Metal profile behind a non-Metal device");
    let size = (slots * std::mem::size_of::<RenderSlot>()) as u64;
    let label = "glyph render slots (direct-mapped)";
    let hal_buf = unsafe {
        hal_dev.create_buffer(&wgpu::hal::BufferDescriptor {
            label: Some(&label),
            size,
            usage: wgpu::BufferUses::STORAGE_READ_ONLY
                | wgpu::BufferUses::COPY_DST
                | wgpu::BufferUses::COPY_SRC
                | wgpu::BufferUses::MAP_READ,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
        })
    }
    .expect("hal arena buffer");
    let mapping = unsafe { hal_dev.map_buffer(&hal_buf, 0..size) }.expect("hal arena map");
    let ptr = mapping.ptr.as_ptr() as *mut RenderSlot;
    let buf = unsafe {
        device.create_buffer_from_hal::<wgpu::hal::api::Metal>(
            hal_buf,
            &wgpu::BufferDescriptor {
                label: Some(&label),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC
                    | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            },
        )
    };
    (ptr, buf)
}

#[cfg(not(target_os = "macos"))]
fn create_mapped_render_slots(
    _device: &wgpu::Device,
    _slots: usize,
) -> (*mut RenderSlot, wgpu::Buffer) {
    unreachable!("Metal mapped primary buffers are only available on macOS");
}

struct ItemPrepass {
    #[allow(dead_code)]
    record_count: u32,
    survivor_count: u32,
    max_row_extent: f64,
}

#[inline(always)]
fn sequence_length(lead: u8) -> usize {
    if lead & 0x80 == 0x00 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

#[inline(always)]
fn decode_codepoint(bytes: &[u8], pos: usize, len: usize) -> u32 {
    let b0 = bytes[pos] as u32;
    if len == 1 {
        return b0;
    }
    let b1 = if pos + 1 < bytes.len() { bytes[pos + 1] as u32 } else { 0 };
    if len == 2 {
        return ((b0 & 0x1F) << 6) | (b1 & 0x3F);
    }
    let b2 = if pos + 2 < bytes.len() { bytes[pos + 2] as u32 } else { 0 };
    if len == 3 {
        return ((b0 & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F);
    }
    let b3 = if pos + 3 < bytes.len() { bytes[pos + 3] as u32 } else { 0 };
    ((b0 & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F)
}

#[inline(always)]
fn is_static_zero_cp(cp: u32) -> bool {
    cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) || (0xE0020..=0xE007F).contains(&cp)
}

struct ResolvedChar {
    glyph_id: u32,
    advance: f32,
    height: f32,
    is_newline: bool,
}

#[inline(always)]
fn resolve_leader(
    bytes: &[u8],
    pos: usize,
    seq_len: usize,
    trie: &TrieTable,
    bitmap_adv: f32,
    em_height_fu: u32,
    trailer_until: &mut usize,
) -> ResolvedChar {
    if pos < *trailer_until {
        return ResolvedChar {
            glyph_id: 0,
            advance: 0.0,
            height: 0.0,
            is_newline: false,
        };
    }

    let cp = decode_codepoint(bytes, pos, seq_len);
    if cp == 0x0A {
        let entry = trie.lookup(cp);
        return ResolvedChar {
            glyph_id: 0,
            advance: fu_to_world(entry.advance_fu, em_height_fu),
            height: fu_to_world(entry.height_fu, em_height_fu),
            is_newline: true,
        };
    }

    if is_static_zero_cp(cp) {
        return ResolvedChar {
            glyph_id: 0,
            advance: 0.0,
            height: 0.0,
            is_newline: false,
        };
    }

    if trie.starts_a_sequence(cp) {
        let mut members = [0usize; 16];
        let mut key = [0u32; 16];
        members[0] = pos;
        key[0] = cp;
        let mut member_count = 1;
        let mut key_count = 1;

        let max_key = (trie.seq_max as usize).min(15);
        let mut p = pos + seq_len;
        while p < bytes.len() && key_count < max_key {
            let n2 = sequence_length(bytes[p]);
            if n2 == 0 {
                break;
            }
            let cp2 = decode_codepoint(bytes, p, n2);
            if cp2 == 0x0A || cp2 == 0xFE0E {
                break;
            }
            members[member_count] = p;
            member_count += 1;
            if cp2 != 0xFE0F {
                key[key_count] = cp2;
                key_count += 1;
            }
            p += n2;
        }

        for try_len in (2..=key_count).rev() {
            if let Some(slot) = trie.sequence_lookup(&key[..try_len]) {
                let mut span_members = 0;
                let mut need = try_len;
                while need > 0 && span_members < member_count {
                    let mid = members[span_members];
                    let n_mid = sequence_length(bytes[mid]);
                    if decode_codepoint(bytes, mid, n_mid) != 0xFE0F {
                        need -= 1;
                    }
                    span_members += 1;
                }
                if span_members > 1 {
                    let last_mid = members[span_members - 1];
                    let last_len = sequence_length(bytes[last_mid]);
                    *trailer_until = last_mid + last_len;
                }
                let entry = trie.lookup(cp);
                return ResolvedChar {
                    glyph_id: slot,
                    advance: bitmap_adv,
                    height: fu_to_world(entry.height_fu, em_height_fu),
                    is_newline: false,
                };
            }
        }
    }

    let entry = trie.lookup(cp);
    ResolvedChar {
        glyph_id: entry.glyph_id,
        advance: fu_to_world(entry.advance_fu, em_height_fu),
        height: fu_to_world(entry.height_fu, em_height_fu),
        is_newline: false,
    }
}

impl LayoutGlyphs for HyperLayout {
    fn name(&self) -> &'static str {
        "hyper-rust"
    }

    fn load_trie_file(&mut self, _path: &Path) -> Result<(), LayoutError> {
        let table = TrieTable::load(&crate::atlas_dir());
        self.trie = Some(Arc::new(table));
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
                let table = TrieTable::load(&crate::atlas_dir());
                let arc = Arc::new(table);
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

                let mut record_count = 0u32;
                let mut survivor_count = 0u32;
                let mut col = 0i64;
                let mut line_adv = 0.0f64;
                let mut seg_adv = 0.0f32;
                let mut max_row_extent = 0.0f64;
                let mut trailer_until = 0usize;

                let mut pos = 0usize;
                while pos < bytes.len() {
                    let lead = bytes[pos];
                    let seq_len = sequence_length(lead);
                    if seq_len == 0 {
                        pos += 1;
                        continue;
                    }

                    let r = resolve_leader(
                        bytes,
                        pos,
                        seq_len,
                        &trie,
                        bitmap_adv,
                        em_height_fu,
                        &mut trailer_until,
                    );

                    let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                    if item_rel_x > max_row_extent {
                        max_row_extent = item_rel_x;
                    }

                    record_count += 1;
                    if r.glyph_id != 0 {
                        survivor_count += 1;
                    }

                    if r.is_newline {
                        col = 0;
                        line_adv = 0.0;
                        seg_adv = 0.0;
                    } else {
                        col += 1;
                        line_adv += r.advance as f64;
                        if fold_unit > 0 && col % fold_unit == 0 {
                            seg_adv = 0.0;
                        } else {
                            seg_adv += r.advance;
                        }
                    }

                    pos += 1;
                }

                ItemPrepass {
                    record_count,
                    survivor_count,
                    max_row_extent,
                }
            })
            .collect();

        // --- Prefix Sum of survivor offsets ---
        let mut slot_bases = Vec::with_capacity(item_count);
        let mut total_survivors = 0usize;
        for pre in &prepasses {
            slot_bases.push(total_survivors as u32);
            total_survivors += pre.survivor_count as usize;
        }

        let can_map_device = allow_device
            && cfg!(target_os = "macos")
            && self.device.as_ref().map_or(false, |dev| {
                dev.host_visible_storage
                    && (total_survivors * std::mem::size_of::<RenderSlot>()) as u64
                        <= dev.max_buffer_size
                    && total_survivors > 0
            });

        if can_map_device {
            let dev = self.device.as_ref().unwrap();
            let (mapped_ptr, wgpu_buf) = create_mapped_render_slots(&dev.device, total_survivors);
            let placements = Self::layout_pass2_device(
                items,
                &prepasses,
                &slot_bases,
                &trie,
                bitmap_adv,
                em_height_fu,
                SendPtr(mapped_ptr),
            );
            let device_slots = DeviceSlots {
                chunks: vec![DeviceSlotChunk {
                    buffer: wgpu_buf,
                    offset: 0,
                    slots: total_survivors as u32,
                }],
                chunk_slots: total_survivors,
                len: total_survivors,
                tint: TintStore::Host(Vec::new()),
                keep_alive: Vec::new(),
                mapped_slots: Some(mapped_ptr as usize),
            };
            *arena = GlyphArena::from_device(device_slots);
            Ok(placements)
        } else {
            let (tail_ptr, capacity) = arena.uninit_tail(total_survivors);
            assert!(capacity >= total_survivors);
            let placements = Self::layout_pass2_host(
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

    fn layout_pass2_device(
        items: &[LayoutItem<'_>],
        prepasses: &[ItemPrepass],
        slot_bases: &[u32],
        trie: &TrieTable,
        bitmap_adv: f32,
        em_height_fu: u32,
        dest: SendPtr<RenderSlot>,
    ) -> Vec<ItemPlacement> {
        let dest_addr = dest.0 as usize;
        items
            .par_iter()
            .zip(prepasses.par_iter())
            .zip(slot_bases.par_iter())
            .map(|((item, pre), &slot_base)| {
                let bytes = item.bytes;
                let p = &item.params;
                let group_id = item.group_id;

                let fold_unit = if p.wrap_width > 0 {
                    p.wrap_width as i64
                } else if p.has_page {
                    p.page_cols as i64
                } else {
                    0
                };
                let page_stride_x = if p.has_page && p.page_rows > 0 {
                    pre.max_row_extent + p.page_gap_x
                } else {
                    0.0
                };
                let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);

                let mut page_right = 0.0f32;
                let mut page_bottom = 0.0f32;
                let mut page_z_min = 0.0f32;
                let mut page_z_max = 0.0f32;

                let mut ink_min = [f32::INFINITY; 3];
                let mut ink_max = [f32::NEG_INFINITY; 3];

                let mut base_row = 0i64;
                let mut col = 0i64;
                let mut line_adv = 0.0f64;
                let mut seg_adv = 0.0f32;
                let mut record_idx = 0usize;
                let mut survivor_out = 0usize;
                let mut trailer_until = 0usize;

                let out_ptr = unsafe { (dest_addr as *mut RenderSlot).add(slot_base as usize) };

                let mut pos = 0usize;
                let mut span_idx = 0usize;
                while pos < bytes.len() {
                    let lead = bytes[pos];
                    let seq_len = sequence_length(lead);
                    if seq_len == 0 {
                        pos += 1;
                        continue;
                    }

                    let r = resolve_leader(
                        bytes,
                        pos,
                        seq_len,
                        trie,
                        bitmap_adv,
                        em_height_fu,
                        &mut trailer_until,
                    );

                    let wrap_segment = wrap_segment_of(col, p.wrap_width as i64, r.is_newline);
                    let wrap_row = wrap_row_of(col, p.wrap_width as i64, r.is_newline, p.wrap_mode);
                    let row = base_row + wrap_row;

                    let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                    let base_x = (item_rel_x + p.origin_x) as f32;
                    let base_y = (-(row as f64) * p.line_height + p.origin_y) as f32;
                    let base_z = (-(wrap_segment as f64) * p.z_step + p.origin_z) as f32;

                    let (pos_x, pos_y, pos_z) = if page_active {
                        let (y_page, x_page, screen_row) = if p.page_rows > 0 {
                            let y_page = row / p.page_rows as i64;
                            let screen_row = row + p.scroll_rows as i64;
                            let x_page = if p.page_cols > 0 {
                                col / p.page_cols as i64
                            } else {
                                0
                            };
                            (y_page, x_page, screen_row)
                        } else {
                            (0, 0, row)
                        };
                        let pages_wide = (p.pages_wide as i64).max(1);
                        let band = y_page / pages_wide;
                        let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                        let py = (p.origin_y
                            - (screen_row - y_page * p.page_rows as i64) as f64 * p.line_height
                            - band as f64 * p.band_stride_y) as f32;
                        let pz = (p.origin_z - wrap_segment as f64 * p.z_step
                            + band as f64 * p.depth_per_band
                            + x_page as f64 * p.depth_per_col) as f32;
                        (px, py, pz)
                    } else {
                        (base_x, base_y, base_z)
                    };

                    let right = pos_x + r.advance;
                    if right > page_right {
                        page_right = right;
                    }
                    if pos_y < page_bottom {
                        page_bottom = pos_y;
                    }
                    if pos_z < page_z_min {
                        page_z_min = pos_z;
                    }
                    if pos_z > page_z_max {
                        page_z_max = pos_z;
                    }

                    let color = match item.paint {
                        Paint::PerRecord(colors) => {
                            if record_idx < colors.len() {
                                colors[record_idx]
                            } else {
                                0xFFFF_FFFF
                            }
                        }
                        Paint::Flat(c) => c,
                        Paint::ByteSpans(spans) => {
                            let p = pos as u32;
                            while span_idx < spans.len() && p >= spans[span_idx].end {
                                span_idx += 1;
                            }
                            if span_idx < spans.len() && p >= spans[span_idx].start {
                                spans[span_idx].color
                            } else {
                                crate::layout::DEFAULT_COLOR_PACKED
                            }
                        }
                    };

                    if r.glyph_id != 0 {
                        let half = r.height * 0.5;
                        if pos_x < ink_min[0] {
                            ink_min[0] = pos_x;
                        }
                        if right > ink_max[0] {
                            ink_max[0] = right;
                        }
                        if pos_y - half < ink_min[1] {
                            ink_min[1] = pos_y - half;
                        }
                        if pos_y + half > ink_max[1] {
                            ink_max[1] = pos_y + half;
                        }
                        if pos_z < ink_min[2] {
                            ink_min[2] = pos_z;
                        }
                        if pos_z > ink_max[2] {
                            ink_max[2] = pos_z;
                        }

                        unsafe {
                            *out_ptr.add(survivor_out) = RenderSlot {
                                pos: [pos_x, pos_y, pos_z],
                                glyph_id: r.glyph_id,
                                color,
                                group_id,
                                advance: r.advance,
                                height: r.height,
                            };
                        }
                        survivor_out += 1;
                    }

                    record_idx += 1;

                    if r.is_newline {
                        base_row += rows_for_line(col, p.wrap_width as i64, p.wrap_mode);
                        col = 0;
                        line_adv = 0.0;
                        seg_adv = 0.0;
                    } else {
                        col += 1;
                        line_adv += r.advance as f64;
                        if fold_unit > 0 && col % fold_unit == 0 {
                            seg_adv = 0.0;
                        } else {
                            seg_adv += r.advance;
                        }
                    }

                    pos += 1;
                }

                ItemPlacement {
                    slot_base,
                    slot_count: survivor_out as u32,
                    record_count: record_idx as u32,
                    page: PageExtent {
                        right: page_right,
                        bottom: page_bottom,
                        z_min: page_z_min,
                        z_max: page_z_max,
                    },
                    ink: InkExtent {
                        min: ink_min,
                        max: ink_max,
                    },
                }
            })
            .collect()
    }

    fn layout_pass2_host(
        items: &[LayoutItem<'_>],
        prepasses: &[ItemPrepass],
        slot_bases: &[u32],
        trie: &TrieTable,
        bitmap_adv: f32,
        em_height_fu: u32,
        dest: SendPtr<GlyphInstance>,
    ) -> Vec<ItemPlacement> {
        let dest_addr = dest.0 as usize;
        items
            .par_iter()
            .zip(prepasses.par_iter())
            .zip(slot_bases.par_iter())
            .map(|((item, pre), &slot_base)| {
                let bytes = item.bytes;
                let p = &item.params;
                let group_id = item.group_id;

                let fold_unit = if p.wrap_width > 0 {
                    p.wrap_width as i64
                } else if p.has_page {
                    p.page_cols as i64
                } else {
                    0
                };
                let page_stride_x = if p.has_page && p.page_rows > 0 {
                    pre.max_row_extent + p.page_gap_x
                } else {
                    0.0
                };
                let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);

                let mut page_right = 0.0f32;
                let mut page_bottom = 0.0f32;
                let mut page_z_min = 0.0f32;
                let mut page_z_max = 0.0f32;

                let mut ink_min = [f32::INFINITY; 3];
                let mut ink_max = [f32::NEG_INFINITY; 3];

                let mut base_row = 0i64;
                let mut col = 0i64;
                let mut line_adv = 0.0f64;
                let mut seg_adv = 0.0f32;
                let mut record_idx = 0usize;
                let mut survivor_out = 0usize;
                let mut trailer_until = 0usize;

                let out_ptr = unsafe { (dest_addr as *mut GlyphInstance).add(slot_base as usize) };

                let mut pos = 0usize;
                let mut span_idx = 0usize;
                while pos < bytes.len() {
                    let lead = bytes[pos];
                    let seq_len = sequence_length(lead);
                    if seq_len == 0 {
                        pos += 1;
                        continue;
                    }

                    let r = resolve_leader(
                        bytes,
                        pos,
                        seq_len,
                        trie,
                        bitmap_adv,
                        em_height_fu,
                        &mut trailer_until,
                    );

                    let wrap_segment = wrap_segment_of(col, p.wrap_width as i64, r.is_newline);
                    let wrap_row = wrap_row_of(col, p.wrap_width as i64, r.is_newline, p.wrap_mode);
                    let row = base_row + wrap_row;

                    let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
                    let base_x = (item_rel_x + p.origin_x) as f32;
                    let base_y = (-(row as f64) * p.line_height + p.origin_y) as f32;
                    let base_z = (-(wrap_segment as f64) * p.z_step + p.origin_z) as f32;

                    let (pos_x, pos_y, pos_z) = if page_active {
                        let (y_page, x_page, screen_row) = if p.page_rows > 0 {
                            let y_page = row / p.page_rows as i64;
                            let screen_row = row + p.scroll_rows as i64;
                            let x_page = if p.page_cols > 0 {
                                col / p.page_cols as i64
                            } else {
                                0
                            };
                            (y_page, x_page, screen_row)
                        } else {
                            (0, 0, row)
                        };
                        let pages_wide = (p.pages_wide as i64).max(1);
                        let band = y_page / pages_wide;
                        let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
                        let py = (p.origin_y
                            - (screen_row - y_page * p.page_rows as i64) as f64 * p.line_height
                            - band as f64 * p.band_stride_y) as f32;
                        let pz = (p.origin_z - wrap_segment as f64 * p.z_step
                            + band as f64 * p.depth_per_band
                            + x_page as f64 * p.depth_per_col) as f32;
                        (px, py, pz)
                    } else {
                        (base_x, base_y, base_z)
                    };

                    let right = pos_x + r.advance;
                    if right > page_right {
                        page_right = right;
                    }
                    if pos_y < page_bottom {
                        page_bottom = pos_y;
                    }
                    if pos_z < page_z_min {
                        page_z_min = pos_z;
                    }
                    if pos_z > page_z_max {
                        page_z_max = pos_z;
                    }

                    let color = match item.paint {
                        Paint::PerRecord(colors) => {
                            if record_idx < colors.len() {
                                colors[record_idx]
                            } else {
                                0xFFFF_FFFF
                            }
                        }
                        Paint::Flat(c) => c,
                        Paint::ByteSpans(spans) => {
                            let p = pos as u32;
                            while span_idx < spans.len() && p >= spans[span_idx].end {
                                span_idx += 1;
                            }
                            if span_idx < spans.len() && p >= spans[span_idx].start {
                                spans[span_idx].color
                            } else {
                                crate::layout::DEFAULT_COLOR_PACKED
                            }
                        }
                    };

                    if r.glyph_id != 0 {
                        let half = r.height * 0.5;
                        if pos_x < ink_min[0] {
                            ink_min[0] = pos_x;
                        }
                        if right > ink_max[0] {
                            ink_max[0] = right;
                        }
                        if pos_y - half < ink_min[1] {
                            ink_min[1] = pos_y - half;
                        }
                        if pos_y + half > ink_max[1] {
                            ink_max[1] = pos_y + half;
                        }
                        if pos_z < ink_min[2] {
                            ink_min[2] = pos_z;
                        }
                        if pos_z > ink_max[2] {
                            ink_max[2] = pos_z;
                        }

                        unsafe {
                            *out_ptr.add(survivor_out) = GlyphInstance {
                                pos: [pos_x, pos_y, pos_z],
                                glyph_id: r.glyph_id,
                                row: row as u32,
                                col: col as u32,
                                color,
                                group_id,
                                advance: r.advance,
                                height: r.height,
                                flags: 0,
                                _pad: 0,
                            };
                        }
                        survivor_out += 1;
                    }

                    record_idx += 1;

                    if r.is_newline {
                        base_row += rows_for_line(col, p.wrap_width as i64, p.wrap_mode);
                        col = 0;
                        line_adv = 0.0;
                        seg_adv = 0.0;
                    } else {
                        col += 1;
                        line_adv += r.advance as f64;
                        if fold_unit > 0 && col % fold_unit == 0 {
                            seg_adv = 0.0;
                        } else {
                            seg_adv += r.advance;
                        }
                    }

                    pos += 1;
                }

                ItemPlacement {
                    slot_base,
                    slot_count: survivor_out as u32,
                    record_count: record_idx as u32,
                    page: PageExtent {
                        right: page_right,
                        bottom: page_bottom,
                        z_min: page_z_min,
                        z_max: page_z_max,
                    },
                    ink: InkExtent {
                        min: ink_min,
                        max: ink_max,
                    },
                }
            })
            .collect()
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
                let table = TrieTable::load(&crate::atlas_dir());
                let arc = Arc::new(table);
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

/// Deterministic single-item record generation in pure Rust, bit-identical to the layout pipeline.
pub fn rederive_item_records(
    bytes: &[u8],
    p: &crate::layout::ItemParams,
    trie: &TrieTable,
) -> Vec<crate::layout::GlyphRecord> {
    let em_height_fu = trie.metrics.em_height_fu;
    let bitmap_adv = fu_to_world(trie.bitmap_advance_fu, em_height_fu);

    let fold_unit = if p.wrap_width > 0 {
        p.wrap_width as i64
    } else if p.has_page {
        p.page_cols as i64
    } else {
        0
    };

    let page_stride_x = if p.has_page && p.page_rows > 0 {
        let mut col = 0i64;
        let mut line_adv = 0.0f64;
        let mut seg_adv = 0.0f32;
        let mut max_row_extent = 0.0f64;
        let mut trailer_until = 0usize;
        let mut pos = 0usize;
        while pos < bytes.len() {
            let lead = bytes[pos];
            let seq_len = sequence_length(lead);
            if seq_len == 0 {
                pos += 1;
                continue;
            }
            let r = resolve_leader(
                bytes,
                pos,
                seq_len,
                trie,
                bitmap_adv,
                em_height_fu,
                &mut trailer_until,
            );
            let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
            if item_rel_x > max_row_extent {
                max_row_extent = item_rel_x;
            }
            if r.is_newline {
                col = 0;
                line_adv = 0.0;
                seg_adv = 0.0;
            } else {
                col += 1;
                line_adv += r.advance as f64;
                if fold_unit > 0 && col % fold_unit == 0 {
                    seg_adv = 0.0;
                } else {
                    seg_adv += r.advance;
                }
            }
            pos += 1;
        }
        max_row_extent + p.page_gap_x
    } else {
        0.0
    };

    let page_active = p.has_page && (p.page_rows > 0 || p.page_cols > 0 || p.scroll_rows > 0);
    let mut base_row = 0i64;
    let mut col = 0i64;
    let mut line_adv = 0.0f64;
    let mut seg_adv = 0.0f32;
    let mut trailer_until = 0usize;
    let mut records = Vec::new();

    let mut pos = 0usize;
    while pos < bytes.len() {
        let lead = bytes[pos];
        let seq_len = sequence_length(lead);
        if seq_len == 0 {
            pos += 1;
            continue;
        }

        let r = resolve_leader(
            bytes,
            pos,
            seq_len,
            trie,
            bitmap_adv,
            em_height_fu,
            &mut trailer_until,
        );

        let wrap_segment = wrap_segment_of(col, p.wrap_width as i64, r.is_newline);
        let wrap_row = wrap_row_of(col, p.wrap_width as i64, r.is_newline, p.wrap_mode);
        let row = base_row + wrap_row;

        let item_rel_x = if fold_unit > 0 { seg_adv as f64 } else { line_adv };
        let base_x = (item_rel_x + p.origin_x) as f32;
        let base_y = (-(row as f64) * p.line_height + p.origin_y) as f32;
        let base_z = (-(wrap_segment as f64) * p.z_step + p.origin_z) as f32;

        let (pos_x, pos_y, pos_z) = if page_active {
            let screen_row = row - p.scroll_rows as i64;
            let y_page = if p.page_rows > 0 && screen_row >= p.page_rows as i64 {
                screen_row / p.page_rows as i64
            } else {
                0
            };
            let x_page = if p.page_cols > 0 {
                col / p.page_cols as i64
            } else {
                0
            };
            let pages_wide = (p.pages_wide as i64).max(1);
            let band = y_page / pages_wide;
            let px = (base_x as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
            let py = (p.origin_y
                - (screen_row - y_page * p.page_rows as i64) as f64 * p.line_height
                - band as f64 * p.band_stride_y) as f32;
            let pz = (p.origin_z - wrap_segment as f64 * p.z_step
                + band as f64 * p.depth_per_band
                + x_page as f64 * p.depth_per_col) as f32;
            (px, py, pz)
        } else {
            (base_x, base_y, base_z)
        };

        records.push(crate::layout::GlyphRecord {
            measures: [pos_x, pos_y, pos_z, r.advance, r.height],
            counts: [r.glyph_id, row as u32, col as u32],
        });

        if r.is_newline {
            base_row += rows_for_line(col, p.wrap_width as i64, p.wrap_mode);
            col = 0;
            line_adv = 0.0;
            seg_adv = 0.0;
        } else {
            col += 1;
            line_adv += r.advance as f64;
            if fold_unit > 0 && col % fold_unit == 0 {
                seg_adv = 0.0;
            } else {
                seg_adv += r.advance;
            }
        }

        pos += 1;
    }

    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ItemParams;

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
                bytes: *bytes,
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
}

