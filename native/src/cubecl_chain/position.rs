use cubecl::prelude::*;

use super::monoid::{
    advance_fixed, fixed_pair, flags_at, item_search_desc, key_to_float, ordered_key, wrap_segment_of,
};
use super::scan::fold_of;
use super::{
    F_LEADER, F_NEWLINE,
    ITEM_DESC_BAND_STRIDE_Y, ITEM_DESC_BYTE_START, ITEM_DESC_COLOR_BASE, ITEM_DESC_DEPTH_PER_BAND,
    ITEM_DESC_DEPTH_PER_COL, ITEM_DESC_FLAT_COLOR, ITEM_DESC_GROUP, ITEM_DESC_HAS_PAGE,
    ITEM_DESC_IS_PER_RECORD, ITEM_DESC_LINE_HEIGHT, ITEM_DESC_ORIGIN_X, ITEM_DESC_ORIGIN_Y,
    ITEM_DESC_ORIGIN_Z, ITEM_DESC_PAGE_COLS, ITEM_DESC_PAGE_GAP_X, ITEM_DESC_PAGE_ROWS,
    ITEM_DESC_PAGES_WIDE, ITEM_DESC_SCROLL_ROWS, ITEM_DESC_STRIDE, ITEM_DESC_WRAP_WIDTH,
    ITEM_DESC_Z_STEP, ITEM_DESC_Z_STEP_LO,
    LC_COL, LC_ROW, LC_STRIDE, LM_STRIDE, LM_X, LM_Y, LM_Z, RESOLVE_SLOTS,
};
use super::tail::EXT_STRIDE;

// ── dispatch 4: resolveX — the WRAPPED items' x, range workers ────────────────
//
// SKIPPED ENTIRELY when no item folds (the driver knows): foldless items
// resolve inside apply's chase, which holds x in a register.
//
// Each worker owns a `span`-byte range (apply's unit decomposition without
// the tree). At the first leader of a segment it enters, the worker walks
// BACKWARD once — at most fold dependent loads — to compute the entry x;
// from there it sweeps FORWARD through its range: every leader's x is the
// running sum, the same additions in the same left-fold order as the old
// per-leader backward re-sum, so the fold>0 X lanes stay BIT-exact (the
// check instrument witnesses that lane at bit level). Heads (col % fold ==
// 0; line and item starts are col==0) re-zero for free. Total backward
// work drops fold-fold (one entry walk per range, not per leader) and
// every worker stays busy — the first draft of this kernel gave each
// SEGMENT to its head and measured 4x WORSE (129ms vs 32): ~2% of threads
// active on long dependent chains is latency-bound with the machine idle.
// Both maxima reduce here (apply's are compiled out for this shape). An
// untouched slot flushes 0, which cannot beat a real value: rows count
// from 1 and every x >= 0 has an ordered key above 0.
#[cube(launch_unchecked)]
pub(super) fn resolve_x(
    sm: &[f32],
    fl: &[u32],
    lm: &mut [f32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    wc: &[u32],
    otb: &[u32],
    wm: &[f32],
    row_max: &mut [Atomic<u32>],
    x_max: &mut [Atomic<u32>],
    extent_words: &[u32],
    page_gap_x: &[f32],
    #[comptime] units: usize,
    #[comptime] span: usize,
) {
    let t = ABSOLUTE_POS;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let u = UNIT_POS as usize;
    let srow = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let sx = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let cube_lo = CUBE_POS * units * span;
    if u == 0 {
        // The item at this cube's first byte anchors the slot numbering.
        let probe = if cube_lo < n { cube_lo } else { n - 1 };
        let mut b = 0usize;
        if item_count > 0 {
            b = item_search(ir, item_count, probe);
        }
        *sbase = b as u32;
    }
    let mut z = u;
    while z < RESOLVE_SLOTS {
        srow[z].store(0u32);
        sx[z].store(0u32);
        z += units;
    }
    sync_cube();
    let it_base = *sbase as usize;

    let lo = t * span;
    if lo < n {
        let hi = if lo + span < n { lo + span } else { n };
        // The item walk, seeded at the range start.
        let mut it = 0usize;
        let mut start = 0usize;
        let mut nxt = n;
        let mut wrap = 0i32;
        let mut fold = 0i32;
        let mut lh = 0.0f32;
        let mut origin_x = 0.0f32;
        let mut origin_y = 0.0f32;
        let mut origin_z = 0.0f32;
        let mut z_step = 0.0f32;
        let mut z_step_lo = 0.0f32;
        let mut band_stride_y = 0.0f32;
        let mut depth_per_band = 0.0f32;
        let mut depth_per_col = 0.0f32;
        let mut rows = 0i32;
        let mut cols = 0i32;
        let mut scroll = 0i32;
        let mut pages_wide = 1i32;
        let mut stride_reach = 0.0f32;
        let mut stride_reach_tail = 0.0f32;
        let has = item_count > 0;
        if has {
            it = item_search(ir, item_count, lo);
            start = ir[it * 2] as usize;
            nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
            wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
            fold = fold_of(ie, it, wrap);
            let io = it * IM_STRIDE;
            lh = items[io + IM_LINE_HEIGHT];
            origin_x = items[io + IM_ORIGIN_X];
            origin_y = items[io + IM_ORIGIN_Y];
            origin_z = items[io + IM_ORIGIN_Z];
            z_step = items[io + IM_Z_STEP];
            z_step_lo = items[io + IM_Z_STEP_LO];
            band_stride_y = items[io + IM_BAND_STRIDE_Y];
            depth_per_band = items[io + IM_DEPTH_PER_BAND];
            depth_per_col = items[io + IM_DEPTH_PER_COL];
            let ie_off = it * IE_STRIDE;
            let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
            rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
            cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
            scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
            let pages_wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
            pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            if has_page && rows > 0 {
                let exact = advance_fixed(key_to_float(extent_words[it * 2]))
                    + advance_fixed(page_gap_x[it]);
                fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
            }
        }
        let mut x = 0.0f32;
        let mut in_seg = false;
        let mut cur_it = it;
        let mut loc_row_max = 0u32;
        let mut loc_x_max = 0u32;
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                if loc_row_max > 0 {
                    let slot = cur_it - it_base;
                    if slot < RESOLVE_SLOTS {
                        srow[slot].fetch_max(loc_row_max);
                        sx[slot].fetch_max(loc_x_max);
                    } else {
                        row_max[cur_it].fetch_max(loc_row_max);
                        x_max[cur_it].fetch_max(loc_x_max);
                    }
                    loc_row_max = 0u32;
                    loc_x_max = 0u32;
                }
                it += 1;
                cur_it = it;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                fold = fold_of(ie, it, wrap);
                let io = it * IM_STRIDE;
                lh = items[io + IM_LINE_HEIGHT];
                origin_x = items[io + IM_ORIGIN_X];
                origin_y = items[io + IM_ORIGIN_Y];
                origin_z = items[io + IM_ORIGIN_Z];
                z_step = items[io + IM_Z_STEP];
                z_step_lo = items[io + IM_Z_STEP_LO];
                band_stride_y = items[io + IM_BAND_STRIDE_Y];
                depth_per_band = items[io + IM_DEPTH_PER_BAND];
                depth_per_col = items[io + IM_DEPTH_PER_COL];
                let ie_off = it * IE_STRIDE;
                let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
                rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
                cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
                scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
                let pages_wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
                pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
                if has_page && rows > 0 {
                    let exact = advance_fixed(key_to_float(extent_words[it * 2]))
                        + advance_fixed(page_gap_x[it]);
                    fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
                } else {
                    stride_reach = 0.0f32;
                    stride_reach_tail = 0.0f32;
                }
            }
            let f = flags_at(fl, id);
            if (f & F_LEADER) != 0 {
                let col = lc[id * LC_STRIDE + LC_COL] as i32;
                if fold > 0 {
                    let head = col % fold == 0;
                    if !in_seg || head {
                        // Entry walk (backward, once per segment entry; free at
                        // a head where col % fold == 0 empties the loop).
                        x = 0.0f32;
                        let ord = wc[id] as i32;
                        let mut k = col % fold;
                        while k >= 1 {
                            let q = otb[start + (ord - k) as usize] as usize;
                            x += sm[q];
                            k -= 1;
                        }
                        in_seg = true;
                    }
                } else {
                    x = wm[id];
                }
                let row = lc[id * LC_STRIDE + LC_ROW] as i32;
                let wrap_segment = wrap_segment_of(col, wrap, (f & F_NEWLINE) != 0);
                let mo = id * LM_STRIDE;
                let base = x + origin_x;

                let mut final_x = base;
                let mut final_y = fma(-(row as f32), lh, origin_y);
                let depth_steps = -(wrap_segment as f32);
                let z_tail_folded = fma(depth_steps, z_step_lo, origin_z);
                let mut final_z = fma(depth_steps, z_step, z_tail_folded);

                if rows != 0 || cols != 0 || scroll != 0 {
                    let screen_row = row - scroll;
                    let mut y_page = 0;
                    if rows > 0 && screen_row >= rows {
                        y_page = screen_row / rows;
                    }
                    let mut x_page = 0;
                    if cols > 0 {
                        x_page = col / cols;
                    }
                    let band = y_page / pages_wide;
                    let page_col = (y_page % pages_wide) as f32;
                    let x_with_tail = fma(page_col, stride_reach_tail, base);
                    final_x = fma(page_col, stride_reach, x_with_tail);
                    let row_in_page = (screen_row - y_page * rows) as f32;
                    let y_row_folded = fma(-row_in_page, lh, origin_y);
                    final_y = fma(-(band as f32), band_stride_y, y_row_folded);
                    let z_stepped = fma(depth_steps, z_step, z_tail_folded);
                    let z_banded = fma(band as f32, depth_per_band, z_stepped);
                    final_z = fma(x_page as f32, depth_per_col, z_banded);
                }

                lm[mo + LM_X] = final_x;
                lm[mo + LM_Y] = final_y;
                lm[mo + LM_Z] = final_z;

                let r = (row + 1) as u32;
                if r > loc_row_max {
                    loc_row_max = r;
                }
                let k = ordered_key(x);
                if k > loc_x_max {
                    loc_x_max = k;
                }
                if (f & F_NEWLINE) == 0 && fold > 0 {
                    // This leader's advance feeds the next x — the same add
                    // the backward re-sum performed, one step forward.
                    x += sm[id];
                }
            }
            id += 1;
        }
        if loc_row_max > 0 {
            let slot = cur_it - it_base;
            if slot < RESOLVE_SLOTS {
                srow[slot].fetch_max(loc_row_max);
                sx[slot].fetch_max(loc_x_max);
            } else {
                row_max[cur_it].fetch_max(loc_row_max);
                x_max[cur_it].fetch_max(loc_x_max);
            }
        }
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let it = it_base + u;
        if it < item_count {
            row_max[it].fetch_max(srow[u].load());
            x_max[it].fetch_max(sx[u].load());
        }
    }
}

/// Fused resolveX and extent reduction: computes wrapped X, page layout, and folds
/// bounding boxes directly in registers, eliminating the separate extent_fold kernel.
#[cube(launch_unchecked)]
pub(super) fn resolve_x_fused(
    sm: &[f32],
    fl: &[u32],
    lm: &mut [f32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    wc: &[u32],
    otb: &[u32],
    wm: &[f32],
    extent_words: &[u32],
    page_gap_x: &[f32],
    gi: &[u32],
    hgt: &[f32],
    ext: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] span: usize,
) {
    let t = ABSOLUTE_POS;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let u = UNIT_POS as usize;
    let sext = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS * EXT_STRIDE);
    let sflags = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let cube_lo = CUBE_POS * units * span;
    if u == 0 {
        let probe = if cube_lo < n { cube_lo } else { n - 1 };
        let mut b = 0usize;
        if item_count > 0 {
            b = item_search(ir, item_count, probe);
        }
        *sbase = b as u32;
    }

    let zero_k = 0x8000_0000u32;
    let inf_k = 0xFF80_0000u32;
    let ninf_k = 0x007F_FFFFu32;

    let mut z = u;
    while z < RESOLVE_SLOTS {
        sflags[z].store(0u32);
        let e = z * EXT_STRIDE;
        sext[e].store(zero_k);
        sext[e + 1].store(zero_k);
        sext[e + 2].store(zero_k);
        sext[e + 3].store(zero_k);
        sext[e + 4].store(inf_k);
        sext[e + 5].store(inf_k);
        sext[e + 6].store(ninf_k);
        sext[e + 7].store(ninf_k);
        sext[e + 8].store(inf_k);
        sext[e + 9].store(ninf_k);
        z += units;
    }
    sync_cube();
    let it_base = *sbase as usize;

    let lo = t * span;
    if lo < n {
        let hi = if lo + span < n { lo + span } else { n };
        let mut it = 0usize;
        let mut start = 0usize;
        let mut nxt = n;
        let mut wrap = 0i32;
        let mut fold = 0i32;
        let has = item_count > 0;
        if has {
            it = item_search(ir, item_count, lo);
            start = ir[it * 2] as usize;
            nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
            wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
            fold = fold_of(ie, it, wrap);
        }
        let mut x = 0.0f32;
        let mut in_seg = false;
        let mut cur_it = it;

        let mut pg_rmax = f32::new(0.0f32);
        let mut pg_ymin = f32::new(0.0f32);
        let mut pg_zmin = f32::new(0.0f32);
        let mut pg_zmax = f32::new(0.0f32);
        let mut ink_xmin = f32::new(3.4028235e38f32);
        let mut ink_ymin = f32::new(3.4028235e38f32);
        let mut ink_rmax = f32::new(-3.4028235e38f32);
        let mut ink_ymax = f32::new(-3.4028235e38f32);
        let mut ink_zmin = f32::new(3.4028235e38f32);
        let mut ink_zmax = f32::new(-3.4028235e38f32);
        let mut any_leader = false;
        let mut any_survivor = false;

        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                if any_leader {
                    let slot = cur_it - it_base;
                    if slot < RESOLVE_SLOTS {
                        sflags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                        let e = slot * EXT_STRIDE;
                        sext[e].fetch_max(ordered_key(pg_rmax));
                        sext[e + 1].fetch_min(ordered_key(pg_ymin));
                        sext[e + 2].fetch_min(ordered_key(pg_zmin));
                        sext[e + 3].fetch_max(ordered_key(pg_zmax));
                        if any_survivor {
                            sext[e + 4].fetch_min(ordered_key(ink_xmin));
                            sext[e + 5].fetch_min(ordered_key(ink_ymin));
                            sext[e + 6].fetch_max(ordered_key(ink_rmax));
                            sext[e + 7].fetch_max(ordered_key(ink_ymax));
                            sext[e + 8].fetch_min(ordered_key(ink_zmin));
                            sext[e + 9].fetch_max(ordered_key(ink_zmax));
                        }
                    } else {
                        let e = cur_it * EXT_STRIDE;
                        ext[e].fetch_max(ordered_key(pg_rmax));
                        ext[e + 1].fetch_min(ordered_key(pg_ymin));
                        ext[e + 2].fetch_min(ordered_key(pg_zmin));
                        ext[e + 3].fetch_max(ordered_key(pg_zmax));
                        if any_survivor {
                            ext[e + 4].fetch_min(ordered_key(ink_xmin));
                            ext[e + 5].fetch_min(ordered_key(ink_ymin));
                            ext[e + 6].fetch_max(ordered_key(ink_rmax));
                            ext[e + 7].fetch_max(ordered_key(ink_ymax));
                            ext[e + 8].fetch_min(ordered_key(ink_zmin));
                            ext[e + 9].fetch_max(ordered_key(ink_zmax));
                        }
                    }
                    pg_rmax = f32::new(0.0f32);
                    pg_ymin = f32::new(0.0f32);
                    pg_zmin = f32::new(0.0f32);
                    pg_zmax = f32::new(0.0f32);
                    ink_xmin = f32::new(3.4028235e38f32);
                    ink_ymin = f32::new(3.4028235e38f32);
                    ink_rmax = f32::new(-3.4028235e38f32);
                    ink_ymax = f32::new(-3.4028235e38f32);
                    ink_zmin = f32::new(3.4028235e38f32);
                    ink_zmax = f32::new(-3.4028235e38f32);
                    any_leader = false;
                    any_survivor = false;
                }
                it += 1;
                cur_it = it;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                fold = fold_of(ie, it, wrap);
            }
            let f = flags_at(fl, id);
            if (f & F_LEADER) != 0 {
                let col = lc[id * LC_STRIDE + LC_COL] as i32;
                if fold > 0 {
                    let head = col % fold == 0;
                    if !in_seg || head {
                        x = 0.0f32;
                        let ord = wc[id] as i32;
                        let mut k = col % fold;
                        while k >= 1 {
                            let q = otb[start + (ord - k) as usize] as usize;
                            x += sm[q];
                            k -= 1;
                        }
                        in_seg = true;
                    }
                } else {
                    x = wm[id];
                }
                let row = lc[id * LC_STRIDE + LC_ROW] as i32;
                let io = it * IM_STRIDE;
                let wrap_segment = wrap_segment_of(col, wrap, (f & F_NEWLINE) != 0);
                let lh = items[io + IM_LINE_HEIGHT];
                let mo = id * LM_STRIDE;
                let base = x + items[io + IM_ORIGIN_X];

                let mut final_x = base;
                let mut final_y = fma(-(row as f32), lh, items[io + IM_ORIGIN_Y]);
                let depth_steps = -(wrap_segment as f32);
                let z_tail_folded = fma(
                    depth_steps,
                    items[io + IM_Z_STEP_LO],
                    items[io + IM_ORIGIN_Z],
                );
                let mut final_z = fma(depth_steps, items[io + IM_Z_STEP], z_tail_folded);

                let ie_off = it * IE_STRIDE;
                let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
                let rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
                let cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
                let scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
                if rows != 0 || cols != 0 || scroll != 0 {
                    let screen_row = row - scroll;
                    let mut y_page = 0;
                    if rows > 0 && screen_row >= rows {
                        y_page = screen_row / rows;
                    }
                    let mut x_page = 0;
                    if cols > 0 {
                        x_page = col / cols;
                    }
                    let pages_wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
                    let pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
                    let band = y_page / pages_wide;
                    let page_col = (y_page % pages_wide) as f32;
                    let mut stride_reach = 0.0f32;
                    let mut stride_reach_tail = 0.0f32;
                    if has_page && rows > 0 {
                        let exact = advance_fixed(key_to_float(extent_words[it * 2]))
                            + advance_fixed(page_gap_x[it]);
                        fixed_pair(exact, &mut stride_reach, &mut stride_reach_tail);
                    }
                    let x_with_tail = fma(page_col, stride_reach_tail, base);
                    final_x = fma(page_col, stride_reach, x_with_tail);
                    let row_in_page = (screen_row - y_page * rows) as f32;
                    let y_row_folded = fma(-row_in_page, lh, items[io + IM_ORIGIN_Y]);
                    final_y = fma(-(band as f32), items[io + IM_BAND_STRIDE_Y], y_row_folded);
                    let z_stepped = fma(depth_steps, items[io + IM_Z_STEP], z_tail_folded);
                    let z_banded = fma(band as f32, items[io + IM_DEPTH_PER_BAND], z_stepped);
                    final_z = fma(x_page as f32, items[io + IM_DEPTH_PER_COL], z_banded);
                }

                lm[mo + LM_X] = final_x;
                lm[mo + LM_Y] = final_y;
                lm[mo + LM_Z] = final_z;

                any_leader = true;
                let right = final_x + sm[id];
                if right > pg_rmax {
                    pg_rmax = right;
                }
                if final_y < pg_ymin {
                    pg_ymin = final_y;
                }
                if final_z < pg_zmin {
                    pg_zmin = final_z;
                }
                if final_z > pg_zmax {
                    pg_zmax = final_z;
                }
                if gi[id] != 0u32 {
                    any_survivor = true;
                    let half = hgt[id] * 0.5f32;
                    let y_lo = final_y - half;
                    let y_hi = final_y + half;
                    if final_x < ink_xmin {
                        ink_xmin = final_x;
                    }
                    if y_lo < ink_ymin {
                        ink_ymin = y_lo;
                    }
                    if right > ink_rmax {
                        ink_rmax = right;
                    }
                    if y_hi > ink_ymax {
                        ink_ymax = y_hi;
                    }
                    if final_z < ink_zmin {
                        ink_zmin = final_z;
                    }
                    if final_z > ink_zmax {
                        ink_zmax = final_z;
                    }
                }

                if (f & F_NEWLINE) == 0 && fold > 0 {
                    x += sm[id];
                }
            }
            id += 1;
        }
        if any_leader {
            let slot = cur_it - it_base;
            if slot < RESOLVE_SLOTS {
                sflags[slot].fetch_or(if any_survivor { 3u32 } else { 1u32 });
                let e = slot * EXT_STRIDE;
                sext[e].fetch_max(ordered_key(pg_rmax));
                sext[e + 1].fetch_min(ordered_key(pg_ymin));
                sext[e + 2].fetch_min(ordered_key(pg_zmin));
                sext[e + 3].fetch_max(ordered_key(pg_zmax));
                if any_survivor {
                    sext[e + 4].fetch_min(ordered_key(ink_xmin));
                    sext[e + 5].fetch_min(ordered_key(ink_ymin));
                    sext[e + 6].fetch_max(ordered_key(ink_rmax));
                    sext[e + 7].fetch_max(ordered_key(ink_ymax));
                    sext[e + 8].fetch_min(ordered_key(ink_zmin));
                    sext[e + 9].fetch_max(ordered_key(ink_zmax));
                }
            } else {
                let e = cur_it * EXT_STRIDE;
                ext[e].fetch_max(ordered_key(pg_rmax));
                ext[e + 1].fetch_min(ordered_key(pg_ymin));
                ext[e + 2].fetch_min(ordered_key(pg_zmin));
                ext[e + 3].fetch_max(ordered_key(pg_zmax));
                if any_survivor {
                    ext[e + 4].fetch_min(ordered_key(ink_xmin));
                    ext[e + 5].fetch_min(ordered_key(ink_ymin));
                    ext[e + 6].fetch_max(ordered_key(ink_rmax));
                    ext[e + 7].fetch_max(ordered_key(ink_ymax));
                    ext[e + 8].fetch_min(ordered_key(ink_zmin));
                    ext[e + 9].fetch_max(ordered_key(ink_zmax));
                }
            }
        }
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let it = it_base + u;
        if it < item_count {
            let flags = sflags[u].load();
            if (flags & 1u32) != 0 {
                let se = u * EXT_STRIDE;
                let e = it * EXT_STRIDE;
                ext[e].fetch_max(sext[se].load());
                ext[e + 1].fetch_min(sext[se + 1].load());
                ext[e + 2].fetch_min(sext[se + 2].load());
                ext[e + 3].fetch_max(sext[se + 3].load());
                if (flags & 2u32) != 0 {
                    ext[e + 4].fetch_min(sext[se + 4].load());
                    ext[e + 5].fetch_min(sext[se + 5].load());
                    ext[e + 6].fetch_max(sext[se + 6].load());
                    ext[e + 7].fetch_max(sext[se + 7].load());
                    ext[e + 8].fetch_min(sext[se + 8].load());
                    ext[e + 9].fetch_max(sext[se + 9].load());
                }
            }
        }
    }
}

// ── the extent walk ─────────────────────────────────────────────────────
// The page stride's input: the WIDEST wrap segment's exact advance sum.
// The engine reads the unrounded f64 prefix (fold.rs scalars[7]); the scan
// tree's keyed value is per-step-rounded and 1-2 ulp short — the census's
// X-at-m2 class (deviations at exact doublings, which only a wrong stride
// value produces). A glyph's stored x is the segment sum BEFORE its own
// advance; the segment resets when the incremented column fills it, and
// the filling glyph's advance never joins the segment it closes
// (fold.rs:758); terminators reset both.
//
// THE SEGMENT-PARALLEL FORM (2026-09-30, repriced by the chain profiler):
// the one-thread-per-item serial walk was 54% of the chain's GPU time at
// the flagship (827ms — the wall is the LARGEST item's latency chain, not
// bandwidth). The bit-exactness constraint binds only WITHIN a segment
// (the engine's f32 serial segment sums), and segments are bounded by the
// wrap width, so: one thread per SEGMENT START, the serial add order
// preserved byte-for-byte inside the walk, and the cross-segment max via
// an order-free ordered-key atomic (the seeded lanes pattern — the buffer
// arrives pre-seeded with ordered_key(0.0), which is also the right value
// for an item with no leaders). Segment starts read off lc's col — the
// fold's own fenced lane: col == 0 (line start) or col % width == 0 (the
// fill boundary); the serial kernel rediscovered the same boundaries by
// counting. 827ms -> ~15ms at the flagship shape.
//
// The segment width arrives pre-resolved in the walk plan (host-packed
// [start, stop, width] per item — fold.rs's rule: wrap if wrapped, else
// page columns if paged, else 0 = whole-line sums). It does NOT ride the
// ie buffer: a six-slice kernel shape misbinds that read (landmine 7's
// cousin, 2026-09-28 — the width came back zeroed, the reset never
// fired). Four input slices, one mutable output.
#[cube(launch_unchecked)]
pub(super) fn extent_pair(
    sm: &[f32],
    fl: &[u32],
    lc: &[u32],
    walk_plan: &[u32],
    extent_words: &mut [Atomic<u32>],
    min_sw: u32,
    uniform_sw: u32,
    #[comptime] units: usize,
    #[comptime] rake: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = lc.len() / LC_STRIDE;
    let tile_lo = tile * (units * rake);
    let item_count = walk_plan.len() / 3;

    let sext = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();

    if u == 0 {
        let mut b = 0usize;
        if item_count > 0 && n > 0 {
            let probe = if tile_lo < n { tile_lo } else { n - 1 };
            let mut lo = 0usize;
            let mut hi = item_count;
            while lo + 1 < hi {
                let mid = (lo + hi) / 2;
                if (walk_plan[mid * 3] as usize) <= probe {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            b = lo;
        }
        *sbase = b as u32;
    }

    let zero_k = 0x8000_0000u32;
    if u < RESOLVE_SLOTS {
        sext[u].store(zero_k);
    }
    sync_cube();
    let it_base = *sbase as usize;
    let nxt_start = if it_base + 1 < item_count {
        walk_plan[(it_base + 1) * 3] as usize
    } else {
        n
    };

    let mut k = 0usize;
    while k < rake {
        let b = tile_lo + k * units + u;
        if b < n && (flags_at(fl, b) & F_LEADER) != 0 {
            let col = lc[b * LC_STRIDE + LC_COL] as usize;
            let candidate = col == 0
                || (col >= (min_sw as usize)
                    && (uniform_sw == 0 || col.is_multiple_of(uniform_sw as usize)));
            if candidate {
                let mut lo = it_base;
                if b >= nxt_start {
                    let mut l = it_base;
                    let mut hi = item_count;
                    while l + 1 < hi {
                        let mid = (l + hi) / 2;
                        if (walk_plan[mid * 3] as usize) <= b {
                            l = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    lo = l;
                }
                let sw = walk_plan[lo * 3 + 2] as usize;
                if col == 0 || (sw != 0 && col.is_multiple_of(sw)) {
                    let stop = walk_plan[lo * 3 + 1] as usize;
                    let mut sum = 0.0f32;
                    let mut widest = 0.0f32;
                    let mut count = 0usize;
                    let mut id = b;
                    let mut cur_word_idx = id >> 2;
                    let mut fl_word = fl[cur_word_idx];
                    while id < stop {
                        let word_idx = id >> 2;
                        if word_idx != cur_word_idx {
                            fl_word = fl[word_idx];
                            cur_word_idx = word_idx;
                        }
                        let f = (fl_word >> (((id & 3) * 8) as u32)) & 0xFF;
                        if (f & F_LEADER) != 0 {
                            // The compare set is the running sum BEFORE this
                            // glyph's own advance — the stored-x rule.
                            if sum > widest {
                                widest = sum;
                            }
                            if (f & F_NEWLINE) != 0 {
                                break;
                            }
                            count += 1;
                            if sw != 0 && count >= sw {
                                // The fill closes the segment; its advance never
                                // joins (fold.rs:758).
                                break;
                            }
                            sum += sm[id];
                        }
                        id += 1;
                    }
                    if widest > 0.0f32 {
                        let key = ordered_key(widest);
                        let slot = lo - it_base;
                        if slot < RESOLVE_SLOTS {
                            sext[slot].fetch_max(key);
                        } else {
                            extent_words[lo * 2].fetch_max(key);
                        }
                    }
                }
            }
        }
        k += 1;
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let it = it_base + u;
        if it < item_count {
            let val = sext[u].load();
            if val > zero_k {
                extent_words[it * 2].fetch_max(val);
            }
        }
    }
}

#[allow(dead_code)]
#[cube(launch_unchecked)]
pub(super) fn derive_stride(
    extent_words: &[u32],
    ie: &[u32],
    page_gap_x: &[f32],
    strides: &mut [f32],
) {
    let i = ABSOLUTE_POS;
    let item_count = ie.len() / IE_STRIDE;
    if i < item_count {
        let ie_off = i * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = ie[ie_off + IE_PAGE_ROWS] as i32;
        if has_page && rows > 0 {
            // The gap folds EXACTLY in fixed-point — the engine holds
            // extent+gap as an f64 (an f32 value plus a small constant,
            // exact in f64), and the f32 add would round it whenever the
            // sum crosses a binade. The decode splits the exact value
            // into (fl(stride), sub-ulp tail) for paginate's fmas.
            let exact = advance_fixed(key_to_float(extent_words[i * 2]))
                + advance_fixed(page_gap_x[i]);
            let mut stride_sum = 0.0f32;
            let mut stride_tail = 0.0f32;
            fixed_pair(exact, &mut stride_sum, &mut stride_tail);
            strides[i * 2] = stride_sum;
            strides[i * 2 + 1] = stride_tail;
        } else {
            strides[i * 2] = 0.0;
            strides[i * 2 + 1] = 0.0;
        }
    }
}

// ── legacy dispatch: paginate (now fused directly into resolve_x) ────────────
#[allow(dead_code)]
#[cube(launch_unchecked)]
pub(super) fn paginate(
    lm: &mut [f32],
    fl: &[u32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    strides: &[f32],
) {
    let id = ABSOLUTE_POS;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    if id < n && (flags_at(fl, id) & F_LEADER) != 0 && item_count > 0 {
        let it = item_search(ir, item_count, id);
        let io = it * IM_STRIDE;
        let ie_off = it * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
        let cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
        let scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
        if rows != 0 || cols != 0 || scroll != 0 {
            let row = lc[id * LC_STRIDE + LC_ROW] as i32;
            let col = lc[id * LC_STRIDE + LC_COL] as i32;
            let screen_row = row - scroll;
            let mut y_page = 0;
            if rows > 0 && screen_row >= rows {
                y_page = screen_row / rows;
            }
            let mut x_page = 0;
            if cols > 0 {
                x_page = col / cols;
            }
            let pages_wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
            let pages_wide = if pages_wide_raw > 1 { pages_wide_raw } else { 1 };
            let band = y_page / pages_wide;
            let wrap = ie[ie_off + IE_WRAP_WIDTH] as i32;
            let wrap_segment = wrap_segment_of(col, wrap, (flags_at(fl, id) & F_NEWLINE) != 0);
            let line_height = items[io + IM_LINE_HEIGHT];
            let mo = id * LM_STRIDE;
            // The three position formulas, one nested OPAQUE fma per
            // term, folded TAIL-FIRST — the tiny correction words ride
            // INSIDE the dominant term's single rounding, which is the
            // engine's f64-store discipline reproduced in f32. Why fma
            // and not plain arithmetic: Metal's optimizer collapses
            // correction identities (`s - (s - a)` → a) on large kernels
            // and cubecl contracts loose mul+add into fma anyway
            // (landmines 8-9) — the builtin is the only shape that
            // survives with its rounding structure intact.
            //
            // X = column position + page column × page stride:
            //   which page column of the band this row lands on, times
            //   how far one page column reaches (the stride pair).
            let page_col = (y_page % pages_wide) as f32;
            let stride_reach_tail = strides[it * 2 + 1];
            let stride_reach = strides[it * 2];
            let x_with_tail = fma(page_col, stride_reach_tail, lm[mo + LM_X]);
            lm[mo + LM_X] = fma(page_col, stride_reach, x_with_tail);
            // Y = page top − row-in-page × line height − band × band stride.
            let row_in_page = (screen_row - y_page * rows) as f32;
            let y_row_folded = fma(-row_in_page, line_height, items[io + IM_ORIGIN_Y]);
            lm[mo + LM_Y] = fma(-(band as f32), items[io + IM_BAND_STRIDE_Y], y_row_folded);
            // Z = depth origin − wrap segment × depth step
            //       + band × band depth + page column × column depth.
            // The last two terms are zero in repo mode; the depth step
            // carries an f64 tail lane (IM_Z_STEP_LO) because the engine
            // multiplies the full f64 param.
            let depth_steps = -(wrap_segment as f32);
            let z_tail_folded = fma(depth_steps, items[io + IM_Z_STEP_LO], items[io + IM_ORIGIN_Z]);
            let z_stepped = fma(depth_steps, items[io + IM_Z_STEP], z_tail_folded);
            let z_banded = fma(band as f32, items[io + IM_DEPTH_PER_BAND], z_stepped);
            lm[mo + LM_Z] = fma(x_page as f32, items[io + IM_DEPTH_PER_COL], z_banded);
        }
    }
}
