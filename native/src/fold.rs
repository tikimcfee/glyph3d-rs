//! Stage 2 of the reference port — the serial fold.
//!
//! SOURCES. The oracle is `glyphPipelineReference.js` (779 lines, web repo
//! `packages/glyph3d-core/src/compute/`); the working reference is
//! `engine/glyph_pipeline.mojo`, which is already proven bit-exact against that
//! oracle across 16 suites and is this tree's settled contract in the corners no
//! fixture reaches (the out-of-range decode, the deleted per-glyph lineHeight
//! fallback). Porting from the Mojo would normally risk inheriting the Mojo's
//! faults — but the GATE here is the frozen fixture corpus, i.e. the oracle's own
//! output, so any Mojo-specific divergence the corpus can see will show.
//!
//! WHAT IT DOES, in order, matching `run_pipeline`:
//!
//!   decode        per byte: classify UTF-8, resolve through the trie, write the
//!                 STATIC lanes (advance, height, glyph_id, flags)
//!   fold          per item: the serial scan that assigns row/col and positions
//!   paginate      per active item: a pure remap of the base position, keyed on
//!                 the INTEGER row/col lanes and never on the float position
//!   bounds        per paged item: min/max over the rewritten positions
//!   batch         union over items
//!
//! THIS PORT IS SERIAL. The Mojo shards decode, fold, paginate and bounds across
//! cores; every one of those decompositions is over disjoint ranges or an exact
//! min/max, so the results are identical and the parallelism is not part of the
//! contract. Nothing here needs to reproduce it to be bit-exact.
//!
//! ── THE FLOAT DISCIPLINE IS HYBRID, ON PURPOSE ───────────────────────────────
//! Three regimes coexist in `layout_item`, and a port written naturally with f32
//! locals reproduces NONE of them. Landmine 2 of engine/PORT-PLAN.md:
//!
//! | quantity            | discipline                        | why |
//! |---------------------|-----------------------------------|-----|
//! | `seg_adv` (fold > 0 x) | genuine f32 arithmetic, rounding per add | matches the GPU's f32 summation order, which is what makes fold>0 lanes bit-exact across groupings |
//! | `line_adv` (foldless x) | accumulated in f64, narrowed ONCE on store | the oracle is the truth layer; the f64 prefix sits between CPU serial-f32 drift and the GPU's log-bounded tree |
//! | `M_Y`, `M_X`, `M_Z`  | computed in f64, narrowed ONCE     | single rounding, not two |
//!
//! Do not "simplify" these into one carrier. The compiler will not tell you
//! which one you got, and gate 9's corpus diff is what does — mutating
//! `line_adv` to f32 reddens X and BASE_X by one ulp.

use crate::text::ResolveGlyph;

pub const F_LEADER: u32 = 1;
pub const F_RENDERED: u32 = 2;
pub const F_NEWLINE: u32 = 4;
pub const F_MISSING: u32 = 8;
pub const NEWLINE: u32 = 0x0A;

/// The trie's own missing bit, distinct from the slot flag above.
pub const TRIE_FLAG_MISSING: u32 = 1;

/// One file in the arena: byte range + layout params.
///
/// FIELD ORDER IS LOAD-BEARING — `fixture::PipeFixture::manifest` hashes these
/// in declaration order and `engine/fixture_manifest.mojo` hashes its own struct
/// the same way, so reordering breaks gate 9's parse parity. That is intended.
///
/// `line_height` is REQUIRED: a NaN one is malformed input, not a request for a
/// per-glyph fallback. The five integer page-geometry params are integers here
/// and truncate at the boundary where f64 VALUES enter (the fixture loader),
/// never at a read site — the 2026-08-31 kind correction.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Item {
    pub byte_start: i64,
    pub byte_count: i64,
    pub origin_x: f64,
    pub origin_y: f64,
    pub origin_z: f64,
    pub wrap_width: i64,
    pub z_step: f64,
    pub line_height: f64,
    pub has_page: bool,
    pub page_rows: i64,
    pub page_cols: i64,
    pub scroll_rows: i64,
    pub pages_wide: i64,
    pub page_gap_x: f64,
    pub band_stride_y: f64,
    pub depth_per_band: f64,
    pub depth_per_col: f64,
    pub page_line_height: f64,
}

/// The slot buffers, SPLIT TWICE — who WRITES a lane decides where it lives,
/// who READS it decides whether it lives at all.
///
///   static      (`sm`, `gi`, `fl`)  decode's output; a pure function of the byte
///   positional  (`lm`, `lc`)        the fold's output; render-read
///   witness     (`wm`, `wc`, `otb`) fold interior no render path reads
///
/// Float carriers hold measures and u32 carriers hold counts — no bitcasts, and
/// a count cannot land in a float array by accident.
///
/// Allocated ZEROED, which subsumes two duties the Mojo performs explicitly: its
/// gap sweep (bytes no item claims) and the fold's non-leader zeroing. Zero is
/// the defined state of both, so the OUTPUT is identical; only the writes differ.
///
/// THE FIELD NAMES STAY SHORT HERE, and only here. `sm`/`gi`/`fl`/`lm`/`lc`/
/// `wm`/`wc` are the cross-layer schema's own spellings — they appear under
/// exactly these names in `engine/glyph_schema.mojo`, in the JS contract, and in
/// every lane constant (`LM_X`, `LC_ROW`, `SM_ADVANCE`) — so renaming them here
/// would break the correspondence that lets four layers be checked against each
/// other. Everything LOCAL is spelled out instead: an abbreviation is a poor
/// place to hide a carrier distinction, which is what `exp_ord` turned out to be
/// hiding when it cost stage 2 a wrong comparison.
pub struct Slots {
    /// 2 per byte: ADVANCE, HEIGHT
    pub sm: Vec<f32>,
    /// 1 per byte: GLYPH_ID (a native u32 identity since 2026-08-31)
    pub gi: Vec<u32>,
    /// 1 per byte
    pub fl: Vec<u32>,
    /// 4 per byte: X, Y, Z, BASE_X
    pub lm: Vec<f32>,
    /// 2 per byte: ROW, COL
    pub lc: Vec<u32>,
    /// 1 per byte: LINE_ADV (witness)
    pub wm: Vec<f32>,
    /// 1 per byte: ORD (witness)
    pub wc: Vec<u32>,
    /// ord -> byte, filled per item over [byte_start, byte_start + ord)
    pub ord_to_byte: Vec<u32>,
}

impl Slots {
    pub(crate) fn new(byte_len: usize) -> Self {
        Self {
            sm: vec![0.0; byte_len * 2],
            gi: vec![0; byte_len],
            fl: vec![0; byte_len],
            lm: vec![0.0; byte_len * 4],
            lc: vec![0; byte_len * 2],
            wm: vec![0.0; byte_len],
            wc: vec![0; byte_len],
            ord_to_byte: vec![0; byte_len],
        }
    }

    #[inline]
    pub fn advance(&self, id: usize) -> f32 {
        self.sm[id * 2]
    }
    #[inline]
    pub fn height(&self, id: usize) -> f32 {
        self.sm[id * 2 + 1]
    }
    #[inline]
    pub fn flags(&self, id: usize) -> u32 {
        self.fl[id]
    }
    #[inline]
    pub fn x(&self, id: usize) -> f32 {
        self.lm[id * 4]
    }
    #[inline]
    pub fn y(&self, id: usize) -> f32 {
        self.lm[id * 4 + 1]
    }
    #[inline]
    pub fn z(&self, id: usize) -> f32 {
        self.lm[id * 4 + 2]
    }
    #[inline]
    pub fn base_x(&self, id: usize) -> f32 {
        self.lm[id * 4 + 3]
    }
    /// The scan form writes these from `resolve_x`, a separate dispatch from
    /// the one that assigns row/col — hence setters the serial fold does not
    /// need, since it computes a position and its lanes in the same breath.
    #[inline]
    pub(crate) fn set_position(&mut self, id: usize, x: f32, y: f32, z: f32) {
        self.lm[id * 4] = x;
        self.lm[id * 4 + 1] = y;
        self.lm[id * 4 + 2] = z;
    }
    #[inline]
    pub(crate) fn set_base_x(&mut self, id: usize, v: f32) {
        self.lm[id * 4 + 3] = v;
    }
    #[inline]
    pub(crate) fn zero_positional(&mut self, id: usize) {
        self.lm[id * 4..id * 4 + 4].fill(0.0);
        self.lc[id * 2..id * 2 + 2].fill(0);
    }

    #[inline]
    pub fn row(&self, id: usize) -> i64 {
        self.lc[id * 2] as i64
    }
    #[inline]
    pub fn col(&self, id: usize) -> i64 {
        self.lc[id * 2 + 1] as i64
    }
}

pub struct FoldResult {
    pub slots: Slots,
    /// Miss CODEPOINTS in byte order, one per occurrence (not a set).
    pub misses: Vec<u32>,
    pub leaders: usize,
    /// item_count * 8: [min xyz, max xyz, TOTAL_ROWS, MAX_ROW_EXTENT]
    pub item_bounds: Vec<f64>,
    pub batch_bounds: Vec<f64>,
}

/// Bytes the sequence starting at `i` occupies — 0 for a continuation or invalid
/// byte, which is exactly the "am I a leader" test.
pub(crate) fn sequence_length(bytes: &[u8], index: usize) -> usize {
    if index >= bytes.len() {
        return 0;
    }
    let lead = bytes[index] as u32;
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

/// Decode the codepoint whose sequence starts at `id`.
///
/// A LENIENT CLASSIFIER: the sequence length comes from the lead byte's own bits
/// and the continuation bytes are NEVER VALIDATED. Reads past the end return 0
/// (the shader's bounds-checked read). This is deliberately NOT the conformant
/// decoder — `str::from_utf8`, `chars()` and `from_utf8_lossy` all implement the
/// conformant one and diverge here. See `fixture::fixture_codepoints`, which
/// wants the conformant decoder for a different job, and do not confuse them.
pub(crate) fn decode_codepoint_at(bytes: &[u8], slot: usize, sequence_len: usize) -> u32 {
    let byte_or_zero = |index: usize| -> u32 {
        if index < bytes.len() {
            bytes[index] as u32
        } else {
            0
        }
    };
    let lead = bytes[slot] as u32;
    match sequence_len {
        1 => lead,
        2 => ((lead & 0x1F) << 6) | (byte_or_zero(slot + 1) & 0x3F),
        3 => {
            ((lead & 0x0F) << 12)
                | ((byte_or_zero(slot + 1) & 0x3F) << 6)
                | (byte_or_zero(slot + 2) & 0x3F)
        }
        _ => {
            ((lead & 0x07) << 18)
                | ((byte_or_zero(slot + 1) & 0x3F) << 12)
                | ((byte_or_zero(slot + 2) & 0x3F) << 6)
                | (byte_or_zero(slot + 3) & 0x3F)
        }
    }
}

/// Visual rows a line occupies under `wrap`; the newline rides at column `len`,
/// so an exact-multiple line ends with a row holding only the newline.
pub(crate) fn rows_for_line(length: i64, wrap: i64) -> i64 {
    if wrap <= 0 {
        1
    } else {
        length / wrap + 1
    }
}

/// THE stride formula: a row-paged item fans page columns at
/// (widest item-relative row + page_gap_x); page_rows 0 derives 0.
pub(crate) fn derive_stride(max_row_extent: f64, item: &Item) -> f64 {
    if !item.has_page || item.page_rows <= 0 {
        0.0
    } else {
        max_row_extent + item.page_gap_x
    }
}

/// Whether paginate does anything for this item — an all-zero page is an
/// identity remap the kernel early-returns from, so the driver may skip it.
pub(crate) fn page_active(item: &Item) -> bool {
    item.has_page && (item.page_rows != 0 || item.page_cols != 0 || item.scroll_rows != 0)
}

/// KERNEL 1 — per byte: classify the sequence, resolve through the trie, write
/// the STATIC lanes. Returns the codepoint for a leader, `None` otherwise.
pub(crate) fn decode_and_resolve<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    slots: &mut Slots,
    trie: &T,
    id: usize,
) -> Option<u32> {
    let sequence_len = sequence_length(bytes, id);
    if sequence_len == 0 {
        // Non-leader (continuation byte, invalid lead byte). Static lanes zero;
        // the positional lanes are the fold's duty, not decode's.
        slots.sm[id * 2] = 0.0;
        slots.sm[id * 2 + 1] = 0.0;
        slots.gi[id] = 0;
        slots.fl[id] = 0;
        return None;
    }
    let codepoint = decode_codepoint_at(bytes, id, sequence_len);
    // The out-of-range contract lives inside `resolve` (see ResolveGlyph impls):
    // a codepoint past the last Unicode scalar resolves through the shared
    // missing block, so it comes back FLAG_MISSING with the missing advance and
    // still occupies its width.
    let resolved = trie.resolve(codepoint);
    slots.sm[id * 2] = resolved.advance;
    slots.sm[id * 2 + 1] = resolved.height;
    slots.gi[id] = resolved.glyph_id;
    let mut flags = F_LEADER;
    if codepoint == NEWLINE {
        flags |= F_NEWLINE;
    }
    if resolved.flags & TRIE_FLAG_MISSING != 0 {
        flags |= F_MISSING;
    }
    slots.fl[id] = flags;
    Some(codepoint)
}

/// THE FOLD — the serial scan over one item's bytes.
///
/// `write_bounds` is false for a paged item (paginate is about to rewrite every
/// position, so a box computed here would describe the pre-page layout) and for
/// a RESUMED range (a partial range must not publish a whole item's box).
#[allow(clippy::too_many_arguments)]
fn layout_item(
    slots: &mut Slots,
    item: &Item,
    scalars: &mut [f64],
    scalar_base: usize,
    write_bounds: bool,
) {
    let wrap = item.wrap_width;
    // The FOLD UNIT: wrap wins over page_cols when both are set, which is why
    // `paged + wrapped` is a dangerous pair — BASE_X then means something
    // different while paginate still divides the same col by cols for x_page and
    // by wrap for seg. Two query params, two divisors, one lane.
    let fold_unit: i64 = if wrap > 0 {
        wrap
    } else if item.has_page {
        item.page_cols
    } else {
        0
    };
    let origin_x = item.origin_x;
    let origin_y = item.origin_y;
    let origin_z = item.origin_z;
    let z_step = item.z_step;
    let line_height = item.line_height;

    let mut box_min_x = f64::INFINITY;
    let mut box_min_y = f64::INFINITY;
    let mut box_min_z = f64::INFINITY;
    let mut box_max_x = f64::NEG_INFINITY;
    let mut box_max_y = f64::NEG_INFINITY;
    let mut box_max_z = f64::NEG_INFINITY;

    let mut base_row: i64 = 0;
    let mut col: i64 = 0;
    // Named for the oracle's `lineAdv` and `segAdv` (glyphPipelineReference.js)
    // and for the LINE_ADV lane, spelled out here because the two carriers are the
    // entire subject of landmine 2 and an abbreviation is a bad place to hide it.
    let mut line_advance: f64 = 0.0; // f64 chain — the truth-layer prefix
    let mut segment_advance: f32 = 0.0; // genuine f32 — the GPU's summation order
    let mut ord: i64 = 0;

    let start = item.byte_start as usize;
    let stop = (item.byte_start + item.byte_count) as usize;
    for id in start..stop {
        if slots.flags(id) & F_LEADER == 0 {
            // The split moved non-leader zeroing here from decode: this loop
            // already visits every byte of its item. Zero-init makes these
            // stores unnecessary, and the lanes already hold zero.
            continue;
        }
        let advance = slots.advance(id);
        let wrap_row = if wrap > 0 { col / wrap } else { 0 };
        let row = base_row + wrap_row;
        // THE CARRIER CHOICE, and the whole of landmine 2 in one line.
        let item_relative_x: f64 = if fold_unit > 0 {
            segment_advance as f64
        } else {
            line_advance
        };
        // X and BASE_X carry the same value at fold time — paginate is what
        // later separates them — so this is one aligned 16-byte store in the
        // Mojo, with the same expressions and the same narrowing points.
        let position_x = (item_relative_x + origin_x) as f32;
        slots.lm[id * 4] = position_x;
        slots.lm[id * 4 + 1] = (-(row as f64) * line_height + origin_y) as f32;
        slots.lm[id * 4 + 2] = (-(wrap_row as f64) * z_step + origin_z) as f32;
        slots.lm[id * 4 + 3] = position_x;
        slots.lc[id * 2] = row as u32;
        slots.lc[id * 2 + 1] = col as u32;
        slots.fl[id] |= F_RENDERED;
        slots.wm[id] = line_advance as f32;
        slots.wc[id] = ord as u32;
        slots.ord_to_byte[start + ord as usize] = id as u32;

        if write_bounds {
            // Read back the STORED, ROUNDED lanes, never the f64 intermediates
            // — folding the wider values would shift box lanes 0-5 off the
            // oracle.
            let stored_x = slots.x(id) as f64;
            let stored_y = slots.y(id) as f64;
            let stored_z = slots.z(id) as f64;
            let stored_advance = slots.advance(id) as f64;
            let stored_height = slots.height(id) as f64;
            if stored_x < box_min_x {
                box_min_x = stored_x;
            }
            if stored_y < box_min_y {
                box_min_y = stored_y;
            }
            if stored_z < box_min_z {
                box_min_z = stored_z;
            }
            if stored_x + stored_advance > box_max_x {
                box_max_x = stored_x + stored_advance;
            }
            if stored_y + stored_height > box_max_y {
                box_max_y = stored_y + stored_height;
            }
            if stored_z > box_max_z {
                box_max_z = stored_z;
            }
        }
        // TOTAL_ROWS and MAX_ROW_EXTENT. Lane 7 accumulates the ITEM-RELATIVE,
        // pre-origin `x` — an f64 max over an f64 prefix, not an exact
        // selection, which is why it must not be "tightened" into one.
        if (row + 1) as f64 > scalars[scalar_base + 6] {
            scalars[scalar_base + 6] = (row + 1) as f64;
        }
        if item_relative_x > scalars[scalar_base + 7] {
            scalars[scalar_base + 7] = item_relative_x;
        }
        ord += 1;
        if slots.flags(id) & F_NEWLINE != 0 {
            base_row += rows_for_line(col, wrap);
            col = 0;
            line_advance = 0.0;
            segment_advance = 0.0;
        } else {
            col += 1;
            line_advance += advance as f64;
            // `col` is the INCREMENTED column here — a segment closes on the
            // glyph that fills it, not on the one after.
            if fold_unit > 0 && col % fold_unit == 0 {
                segment_advance = 0.0;
            } else {
                segment_advance += advance;
            }
        }
    }

    if write_bounds {
        scalars[scalar_base] = box_min_x;
        scalars[scalar_base + 1] = box_min_y;
        scalars[scalar_base + 2] = box_min_z;
        scalars[scalar_base + 3] = box_max_x;
        scalars[scalar_base + 4] = box_max_y;
        scalars[scalar_base + 5] = box_max_z;
    }
}

/// KERNEL — pagination as a PURE per-slot remap of the base position.
///
/// Every page decision reads the INTEGER row/col lanes, never the float
/// position. The page's own `line_height` is NOT consulted: that mirrored the
/// oracle's `resolved[i].lineHeight ?? it.page?.lineHeight`, deleted as
/// unreachable once the item's line height was guaranteed finite before
/// paginate reads it. A page pitch was never a feature — it was gated on a bug.
pub(crate) fn paginate(slots: &mut Slots, id: usize, item: &Item, page_stride_x: f64) {
    if slots.flags(id) & F_LEADER == 0 {
        return;
    }
    let rows = if item.has_page { item.page_rows } else { 0 };
    let cols = if item.has_page { item.page_cols } else { 0 };
    let scroll = if item.has_page { item.scroll_rows } else { 0 };
    if rows == 0 && cols == 0 && scroll == 0 {
        return;
    }
    let row = slots.row(id);
    let col = slots.col(id);
    let screen_row = row - scroll; // the conveyor; negative rows stay in flow

    let mut y_page = 0i64;
    if rows > 0 && screen_row >= rows {
        y_page = screen_row / rows; // exact, integer gate
    }
    let mut x_page = 0i64;
    if cols > 0 {
        x_page = col / cols; // exact
    }
    let pages_wide = if item.pages_wide > 1 { item.pages_wide } else { 1 };
    let band = y_page / pages_wide;
    let wrap_segment = if item.wrap_width > 0 { col / item.wrap_width } else { 0 };
    let line_height = item.line_height;

    slots.lm[id * 4] =
        (slots.base_x(id) as f64 + (y_page % pages_wide) as f64 * page_stride_x) as f32;
    slots.lm[id * 4 + 1] = (item.origin_y
        - (screen_row - y_page * rows) as f64 * line_height
        - band as f64 * item.band_stride_y) as f32;
    slots.lm[id * 4 + 2] = (item.origin_z - wrap_segment as f64 * item.z_step
        + band as f64 * item.depth_per_band
        + x_page as f64 * item.depth_per_col) as f32;
}

/// Min/max over one byte range, carried in registers and stored once.
pub(crate) fn bounds_range(slots: &Slots, start: usize, stop: usize) -> [f64; 6] {
    let mut box_lanes = [
        f64::INFINITY,
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for id in start..stop {
        if slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        let glyph_x = slots.x(id) as f64;
        let glyph_y = slots.y(id) as f64;
        let glyph_z = slots.z(id) as f64;
        let advance = slots.advance(id) as f64;
        let height = slots.height(id) as f64;
        if glyph_x < box_lanes[0] {
            box_lanes[0] = glyph_x;
        }
        if glyph_y < box_lanes[1] {
            box_lanes[1] = glyph_y;
        }
        if glyph_z < box_lanes[2] {
            box_lanes[2] = glyph_z;
        }
        if glyph_x + advance > box_lanes[3] {
            box_lanes[3] = glyph_x + advance;
        }
        if glyph_y + height > box_lanes[4] {
            box_lanes[4] = glyph_y + height;
        }
        if glyph_z > box_lanes[5] {
            box_lanes[5] = glyph_z;
        }
    }
    box_lanes
}

/// The whole pipeline: decode -> fold per item -> paginate the active items with
/// the DERIVED fan stride -> per-item boxes -> batch union.
pub fn run_pipeline<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    trie: &T,
    items: &[Item],
) -> FoldResult {
    let byte_len = bytes.len();
    for (index, item) in items.iter().enumerate() {
        // FAIL LOUD AT THE SEAM. An item range outside the buffer is malformed
        // input, and clamping it would turn that into a plausible-looking layout.
        assert!(
            item.byte_start >= 0
                && item.byte_count >= 0
                && (item.byte_start + item.byte_count) as usize <= byte_len,
            "item {index} covers [{}, {}) of a {byte_len}-byte buffer",
            item.byte_start,
            item.byte_start + item.byte_count
        );
    }
    let mut slots = Slots::new(byte_len);

    // ── decode ────────────────────────────────────────────────────────────────
    // Misses are collected in BYTE ORDER, one entry per occurrence rather than
    // per distinct codepoint — the Mojo reaches the same order by concatenating
    // its shards' lists in shard order, which is byte order.
    let mut misses = Vec::new();
    let mut leaders = 0usize;
    for id in 0..byte_len {
        if let Some(codepoint) = decode_and_resolve(bytes, &mut slots, trie, id) {
            leaders += 1;
            if slots.flags(id) & F_MISSING != 0 {
                misses.push(codepoint);
            }
        }
    }

    // ── the fold, per item ────────────────────────────────────────────────────
    let mut item_bounds = vec![0.0f64; items.len() * 8];
    for (index, item) in items.iter().enumerate() {
        layout_item(&mut slots, item, &mut item_bounds, index * 8, !page_active(item));
    }

    // ── paginate: stride DERIVED from the fold's own scalar 7 ─────────────────
    for (index, item) in items.iter().enumerate() {
        if !page_active(item) {
            continue;
        }
        let stride = derive_stride(item_bounds[index * 8 + 7], item);
        let start = item.byte_start as usize;
        let stop = (item.byte_start + item.byte_count) as usize;
        for id in start..stop {
            paginate(&mut slots, id, item, stride);
        }
    }

    // ── per-item boxes for the paged items only ───────────────────────────────
    // A non-paged item already has its box from the fold; only the items
    // paginate rewrote need this pass.
    for (index, item) in items.iter().enumerate() {
        if !page_active(item) {
            continue;
        }
        let start = item.byte_start as usize;
        let stop = (item.byte_start + item.byte_count) as usize;
        let box_lanes = bounds_range(&slots, start, stop);
        item_bounds[index * 8..index * 8 + 6].copy_from_slice(&box_lanes);
    }

    // ── batch union: min over 0-2, max over 3-7 (lanes 6/7 included) ──────────
    let mut batch_bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        0.0,
        0.0,
    ];
    for index in 0..items.len() {
        for lane in 0..3 {
            if item_bounds[index * 8 + lane] < batch_bounds[lane] {
                batch_bounds[lane] = item_bounds[index * 8 + lane];
            }
        }
        for lane in 3..8 {
            if item_bounds[index * 8 + lane] > batch_bounds[lane] {
                batch_bounds[lane] = item_bounds[index * 8 + lane];
            }
        }
    }

    FoldResult {
        slots,
        misses,
        leaders,
        item_bounds,
        batch_bounds: batch_bounds.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph_trie::{build_glyph_trie, BuiltTrie, GlyphMetrics};

    /// A trie mapping every ASCII letter to a flat advance.
    fn trie() -> BuiltTrie {
        build_glyph_trie(
            (0x20u32..0x7Fu32).chain(std::iter::once(0x0A)),
            |cp| Some(GlyphMetrics { glyph_id: cp + 1, advance: 0.5, height: 1.0 }),
            0.61,
            1.25,
        )
    }

    fn paged_item(bytes: usize) -> Item {
        Item {
            byte_start: 0,
            byte_count: bytes as i64,
            origin_x: 0.0,
            origin_y: 3.0,
            origin_z: 5.0,
            line_height: 1.0,
            has_page: true,
            page_cols: 4,
            pages_wide: 2,
            ..Item::default()
        }
    }

    // ── The two corpus ceilings, closed here because the fixtures cannot ────
    //
    // Stage 2's mutation battery ran 16 mutations; 14 reddened against the
    // corpus and these two did not, for reasons that are properties of the
    // CORPUS rather than of the code. Both are covered here instead. Neither
    // test is decoration: each was verified to fail under the mutation that the
    // corpus let through.

    /// CEILING 1 — paginate must skip non-leader bytes.
    ///
    /// Removing that guard is invisible to the corpus: the only paged fixture
    /// containing a non-leader byte (`real-kernels`) has origin_y = origin_z =
    /// 0, so remapping a non-leader whose row/col/base_x are all zero writes
    /// zeros back and changes nothing. With a nonzero origin it does not.
    #[test]
    fn paginate_leaves_non_leader_bytes_alone() {
        let t = trie();
        // 0x80 is a stray continuation byte — a non-leader.
        let bytes = [b'A', 0x80, b'B'];
        let r = run_pipeline(&bytes, &t, &[paged_item(bytes.len())]);
        assert_eq!(r.slots.flags(1) & F_LEADER, 0, "byte 1 must be a non-leader");
        assert_eq!(
            (r.slots.x(1), r.slots.y(1), r.slots.z(1), r.slots.base_x(1)),
            (0.0, 0.0, 0.0, 0.0),
            "a non-leader's positional lanes are defined ZERO, not the item origin"
        );
        // And the leaders around it did get paginated, so the test is not
        // passing because pagination never ran.
        assert_eq!(r.slots.y(0) as f64, 3.0, "leader 0 should sit at origin_y");
    }

    /// CEILING 2 — paginate must NOT consult the page's own line height.
    ///
    /// The oracle's `resolved[i].lineHeight ?? it.page?.lineHeight` was deleted
    /// as unreachable once the item's line height was guaranteed finite. The
    /// corpus cannot check that deletion: every paged fixture carries
    /// page_line_height EQUAL to line_height (1.0, 1.1, 1.2, 1.3), so consulting
    /// the wrong one is invisible. A page pitch was never a feature.
    #[test]
    fn paginate_ignores_page_line_height() {
        let t = trie();
        // NEWLINES MATTER HERE. The first version of this test used a single
        // unbroken line, so every glyph sat on row 0, screen_row was 0 for all
        // of them and paginate's Y expression never varied — the test passed
        // while exercising nothing. Its own anti-vacuity assertion is what said
        // so. Five rows against page_rows = 2 puts rows 2-4 on a second page.
        let bytes = b"ab\ncd\nef\ngh\nij";
        let mut a = paged_item(bytes.len());
        a.page_rows = 2;
        a.page_line_height = 1.0;
        let mut b = a;
        b.page_line_height = 99.0; // the ONLY difference
        let ra = run_pipeline(bytes, &t, &[a]);
        let rb = run_pipeline(bytes, &t, &[b]);
        assert_eq!(
            ra.slots.lm, rb.slots.lm,
            "page_line_height must not reach any position lane"
        );
        // Anti-vacuity, and the second attempt at it. Comparing row 0 to the
        // LAST row was also wrong: row 4 tops page 2 exactly as row 0 tops page
        // 1, so with band_stride_y = 0 they legitimately coincide. Two better
        // questions — do rows separate WITHIN a page, and did the y_page
        // subtraction actually move the second page's rows?
        assert!(page_active(&a));
        let last = bytes.len() - 1; // 'j', the fifth row
        assert_ne!(
            ra.slots.y(0),
            ra.slots.y(3),
            "row 0 and row 1 share a page and must separate by line_height"
        );
        assert!(ra.slots.row(last) >= a.page_rows, "the last row must reach page 2");
        let unpaginated = (a.origin_y - ra.slots.row(last) as f64 * a.line_height) as f32;
        assert_ne!(
            ra.slots.y(last), unpaginated,
            "the y_page term must have remapped the second page"
        );
    }

    /// CEILING 3 — scroll deeper than one page.
    ///
    /// `screen_row - y_page * rows` and `screen_row % rows` agree for every
    /// screen_row > -rows, which is the whole corpus (paged-rows scrolls 3 with
    /// 6 rows). Past that they differ, and only the first form keeps a
    /// deeply-scrolled row in flow.
    #[test]
    fn a_row_scrolled_past_a_whole_page_stays_in_flow() {
        let t = trie();
        let bytes = b"a\nb\nc";
        let mut it = paged_item(bytes.len());
        it.page_cols = 0;
        it.page_rows = 2;
        it.scroll_rows = 9; // row 0 lands at screen_row -9, far past -rows
        it.band_stride_y = 0.0;
        let r = run_pipeline(bytes, &t, &[it]);
        // y_page is 0 (the gate needs screen_row >= rows), so Y is a straight
        // origin_y - screen_row * lh with NO page subtraction.
        for (id, row) in [(0usize, 0i64), (2, 1), (4, 2)] {
            let screen_row = row - 9;
            let want = (it.origin_y - screen_row as f64 * it.line_height) as f32;
            assert_eq!(
                r.slots.y(id),
                want,
                "byte {id}: a row scrolled past a page must stay in flow"
            );
        }
    }
}
