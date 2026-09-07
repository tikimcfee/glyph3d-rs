//! The serial fold — part of the reference port.
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
//! IT DOES NOT REPLACE `text::reference_layout`, which computes a subset of
//! what this does. That is deliberate and the reason is at that function: this
//! file was ported FROM the Mojo, `reference_layout` was written independently
//! against the TSL kernel, and `--engine-check` is worth running precisely
//! because its two sides have different lineage. Merging them would leave the
//! Mojo checked against a port of itself.
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

/// THE WRAP MODE — an ITEM-LEVEL parameter, never per line and never per range.
///
/// | mode | a wrap advances the ROW | rows_for_line(n, wrap) | z |
/// |---|---|---|---|
/// | `Down` | yes | `ceil(n / wrap)` | `-segment * z_step` |
/// | `Back` | NO | `1`, always | `-segment * z_step` |
///
/// Under `Back` every wrap segment of a line shares ONE row and the segments stack
/// in DEPTH instead, so a line's row is just its line index. `col` still counts
/// within the LOGICAL line, `segment_advance` still resets at every fold boundary
/// (each segment starts at x = 0), and the wrap SEGMENT index still exists in both
/// modes — under `Back` it feeds z and no longer feeds row. Picking by (row, col)
/// still resolves uniquely because col differs between segments.
///
/// WHY IT IS ITEM-LEVEL AND MUST STAY THAT WAY: `scan::scan_combine`'s junction term
/// evaluates `rows_for_line` with `b`'s parameters, so it is not associative across a
/// change of them. Mode joins wrap in that term, which makes the non-associative
/// surface WIDER, not narrower. What keeps the scan form safe is structural and
/// unchanged: an item boundary emits a resetting leaf, so no interval without a reset
/// spans two items. See `scan::tests::mixed_mode_is_outside_the_monoid_s_domain`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WrapMode {
    /// A wrap advances the visual row. Today's behaviour, and the default.
    #[default]
    Down,
    /// A wrap keeps the row and steps only in depth.
    Back,
}

impl WrapMode {
    /// The wire encoding: 0 = Down, 1 = Back. Shared by the `.pipe.bin` item
    /// record, the FFI descriptor and the device item table.
    pub const fn code(self) -> i64 {
        match self {
            WrapMode::Down => 0,
            WrapMode::Back => 1,
        }
    }

    /// FAIL LOUD AT THE SEAM: an unknown code is malformed input, not a request
    /// for the default. Folding it silently to `Down` is how a caller's typo
    /// becomes an invisible layout.
    pub fn from_code(code: i64) -> Self {
        match code {
            0 => WrapMode::Down,
            1 => WrapMode::Back,
            other => panic!("wrap mode must be 0 (Down) or 1 (Back), got {other}"),
        }
    }
}

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
    /// Item-level, exactly like `wrap_width`. See [`WrapMode`].
    pub wrap_mode: WrapMode,
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
/// hiding when it cost the fold a wrong comparison.
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

/// Visual rows a line of `length` cells occupies under `wrap` and `mode`.
///
/// Under [`WrapMode::Down`] a CEILING with a floor of one, since an empty line
/// still occupies the row it sits on. Under [`WrapMode::Back`] it is ONE for
/// every line, whatever the length — that identity is the mode.
///
/// THE PHANTOM ROW (corrected 2026-09-04). This was `length / wrap + 1`, which
/// counts the row the terminating newline rides on. The newline rides at column
/// `length`, so when `wrap` divides `length` that column rolls onto a fresh row
/// holding nothing else: the line claimed a blank row and every later line moved
/// down one. The old rule and this one agree for every other length, which is
/// why the defect was invisible except at exact multiples — on `wide.txt` at
/// wrap 100 it displaced the 8th line by four rows.
///
/// The frozen JS oracle corpus was generated under the old rule, so it SPECIFIES
/// the phantom; see `engine/delta/phantom-row.md` for the fixtures and lanes
/// that must be regenerated.
pub(crate) fn rows_for_line(length: i64, wrap: i64, mode: WrapMode) -> i64 {
    if mode == WrapMode::Back {
        // A WrapBack line occupies exactly the row it sits on, whatever its
        // length: the folds go into depth, and depth costs no rows.
        return 1;
    }
    if wrap <= 0 || length <= 0 {
        1
    } else {
        (length - 1) / wrap + 1
    }
}

/// The WRAP SEGMENT index of a cell at column `col` — how many times its line
/// has already folded before reaching it. MODE-FREE: this is the DEPTH fan's
/// index and it exists in both modes; only its contribution to the ROW is a
/// mode question ([`wrap_row_of`]).
///
/// Two kinds of cell, and they differ at exactly one place. An ordinary glyph
/// at column `col` sits in segment `col / wrap`. A NEWLINE is a terminator
/// riding at one-past-the-last cell (`col == the line's glyph count`), so at an
/// exact multiple `col / wrap` would roll it into a segment that holds nothing
/// else. It belongs to the last segment its line reaches.
///
/// Every consumer of (col, wrap) -> segment goes through here. Deriving the two
/// cases from one expression is what let the terminator open a phantom row in
/// the first place, and the special case is worth a name.
pub(crate) fn wrap_segment_of(col: i64, wrap: i64, terminator: bool) -> i64 {
    if wrap <= 0 {
        0
    } else if terminator {
        // `rows_for_line(col, wrap, Down) - 1`, written out so the segment index
        // cannot pick up a mode through the helper it used to borrow.
        if col <= 0 {
            0
        } else {
            (col - 1) / wrap
        }
    } else {
        col / wrap
    }
}

/// The LINE-LOCAL ROW CONTRIBUTION of a cell at column `col`.
///
/// `Down` delegates to [`wrap_segment_of`] — which is the whole of the
/// default's proof: mode A's row IS the segment index, byte for byte, as it was
/// before modes existed. `Back` contributes ZERO, because a wrap does not
/// advance the row at all in that mode.
pub(crate) fn wrap_row_of(col: i64, wrap: i64, terminator: bool, mode: WrapMode) -> i64 {
    match mode {
        WrapMode::Down => wrap_segment_of(col, wrap, terminator),
        WrapMode::Back => 0,
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
    let mode = item.wrap_mode;
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
        // TWO indices, and under WrapBack they differ. `wrap_segment` is the DEPTH
        // fan's index and always exists; `wrap_row` is what that segment
        // contributes to the ROW, which WrapBack makes zero. The newline is a
        // TERMINATOR at one-past-the-last cell, so at an exact multiple it stays in
        // the segment it closes instead of opening the next.
        let terminator = slots.flags(id) & F_NEWLINE != 0;
        let wrap_segment = wrap_segment_of(col, wrap, terminator);
        let wrap_row = wrap_row_of(col, wrap, terminator, mode);
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
        slots.lm[id * 4 + 2] = (-(wrap_segment as f64) * z_step + origin_z) as f32;
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
            base_row += rows_for_line(col, wrap, mode);
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
    // The SAME rule the fold's Z used, terminator case included: paginate
    // recomputes Z from the COL lane, so a divergence here would put a
    // newline's depth one wrap step behind its own row's.
    // Z is the DEPTH fan, so it reads the SEGMENT index, not the row
    // contribution — under WrapBack those differ and the segments are all z has
    // left. The ROW lane it reads above already carries the mode.
    let wrap_segment =
        wrap_segment_of(col, item.wrap_width, slots.flags(id) & F_NEWLINE != 0);
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

/// DISPATCH 1, shared by both forms: decode every byte and collect the miss list.
///
/// Deduplicated deliberately, unlike `reference_layout` (see the note there):
/// this loop carries no verification value as a second copy — the two forms
/// were byte-identical, so a divergence between them could only ever be a typo,
/// never a finding.
///
/// Misses are collected in BYTE ORDER, one entry per occurrence rather than per
/// distinct codepoint; the Mojo reaches the same order by concatenating its
/// shards' lists in shard order, which is byte order.
pub(crate) fn decode_all<T: ResolveGlyph + ?Sized>(
    bytes: &[u8],
    slots: &mut Slots,
    trie: &T,
) -> (Vec<u32>, usize) {
    let mut misses = Vec::new();
    let mut leaders = 0usize;
    for id in 0..bytes.len() {
        if let Some(codepoint) = decode_and_resolve(bytes, slots, trie, id) {
            leaders += 1;
            if slots.flags(id) & F_MISSING != 0 {
                misses.push(codepoint);
            }
        }
    }
    (misses, leaders)
}

/// The batch union over per-item boxes: min on lanes 0-2, max on 3-7.
///
/// Lanes 6 and 7 (TOTAL_ROWS, MAX_ROW_EXTENT) ride the MAX side with the box's
/// upper corner — they are not box lanes at all, but the union is a max either
/// way, and keeping them in one loop is what the reference does.
pub(crate) fn batch_union(item_bounds: &[f64], item_count: usize) -> Vec<f64> {
    let mut batch = [
        f64::INFINITY,
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        0.0,
        0.0,
    ];
    for index in 0..item_count {
        for lane in 0..3 {
            if item_bounds[index * 8 + lane] < batch[lane] {
                batch[lane] = item_bounds[index * 8 + lane];
            }
        }
        for lane in 3..8 {
            if item_bounds[index * 8 + lane] > batch[lane] {
                batch[lane] = item_bounds[index * 8 + lane];
            }
        }
    }
    batch.to_vec()
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
    let (misses, leaders) = decode_all(bytes, &mut slots, trie);

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

    // ── batch union ───────────────────────────────────────────────────────────
    let batch_bounds = batch_union(&item_bounds, items.len());

    FoldResult {
        slots,
        misses,
        leaders,
        item_bounds,
        batch_bounds,
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
    // The fold's mutation battery ran 16 mutations; 14 reddened against the
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

    // ── The phantom row (2026-09-04) ───────────────────────────────────────
    //
    // `rows_for_line` used to be `length / wrap + 1`, which counts the row the
    // terminating newline rides on. At an exact multiple that column rolls onto
    // a fresh row holding nothing else, so the line claimed a blank row and
    // every later line moved down one. These four tests pin the corrected rule
    // and its two halves; the fixture corpus cannot, because the frozen oracle
    // has the same defect and therefore SPECIFIES it.

    /// THE RULE, stated as a table rather than as the formula under test.
    ///
    /// A line of `n` cells under wrap `w` covers the rows its cells reach and
    /// no more.
    #[test]
    fn rows_for_line_counts_only_rows_a_cell_reaches() {
        // (length, wrap, rows)
        let table = [
            (0i64, 4i64, 1i64), // an empty line still occupies its row
            (1, 4, 1),
            (3, 4, 1),
            (4, 4, 1), // EXACT MULTIPLE — the defect's whole domain
            (5, 4, 2),
            (8, 4, 2), // exact multiple
            (9, 4, 3),
            (12, 4, 3), // exact multiple
            (0, 1, 1),  // wrap 1 divides EVERYTHING, so it is all domain
            (1, 1, 1),
            (2, 1, 2),
            (100, 100, 1),
            (101, 100, 2),
            (200, 100, 2),
            (5000, 100, 50),
            (7, 0, 1), // wrap off: one row, always
            (0, 0, 1),
        ];
        for (length, wrap, want) in table {
            assert_eq!(
                rows_for_line(length, wrap, WrapMode::Down),
                want,
                "rows_for_line({length}, {wrap})"
            );
            // THE MODE, on the same table: WrapBack collapses every one of these
            // to a single row, including the entries where WrapDown counts 50.
            assert_eq!(
                rows_for_line(length, wrap, WrapMode::Back),
                1,
                "rows_for_line({length}, {wrap}, Back) is one row, always"
            );
        }
        // ANTI-VACUITY on that second claim: the table must contain a row where
        // the two modes actually differ, or "always 1" is agreeing with WrapDown.
        assert!(
            table.iter().any(|&(n, w, _)| rows_for_line(n, w, WrapMode::Down) > 1),
            "the table must contain a line that WrapDown folds"
        );
    }

    /// The terminator's own row: a newline sits on the row it CLOSES, never on
    /// the one after. Stated as its own table so this and `rows_for_line`
    /// cannot agree by sharing a bug.
    #[test]
    fn a_newline_rides_the_row_it_closes() {
        // (col == line length, wrap, line-local row of the newline)
        let table = [
            (0i64, 4i64, 0i64),
            (3, 4, 0),
            (4, 4, 0), // exact multiple: stays on row 0 with its four glyphs
            (5, 4, 1),
            (8, 4, 1),
            (9, 4, 2),
            (100, 100, 0),
            (5000, 100, 49),
        ];
        for (col, wrap, want) in table {
            assert_eq!(
                wrap_row_of(col, wrap, true, WrapMode::Down),
                want,
                "newline at col {col}, wrap {wrap}"
            );
            // An ORDINARY cell at the same column is unaffected: the two rules
            // differ only for the terminator, and only at a multiple.
            assert_eq!(
                wrap_row_of(col, wrap, false, WrapMode::Down),
                col / wrap,
                "glyph at col {col}"
            );
            // THE SEGMENT INDEX IS MODE-FREE, and WrapDown's row IS that index.
            // Both halves matter: the first is what keeps z alive under WrapBack,
            // the second is why mode A cannot have moved.
            assert_eq!(
                wrap_segment_of(col, wrap, true),
                wrap_row_of(col, wrap, true, WrapMode::Down),
                "WrapDown's row is the segment index, terminator at col {col}"
            );
            assert_eq!(
                wrap_segment_of(col, wrap, false),
                wrap_row_of(col, wrap, false, WrapMode::Down),
                "WrapDown's row is the segment index, glyph at col {col}"
            );
            assert_eq!(
                wrap_row_of(col, wrap, true, WrapMode::Back),
                0,
                "WrapBack spends no row, terminator at col {col}"
            );
            assert_eq!(
                wrap_row_of(col, wrap, false, WrapMode::Back),
                0,
                "WrapBack spends no row, glyph at col {col}"
            );
        }
        // ANTI-VACUITY: some column in the table must have a NONZERO segment, or
        // "WrapBack returns 0" is indistinguishable from "so does WrapDown here".
        assert!(
            table.iter().any(|&(col, wrap, _)| wrap_segment_of(col, wrap, false) > 0),
            "the table must contain a column past the first segment"
        );
    }

    /// THE DEFECT, on the fixture it was measured on.
    ///
    /// `wide.txt` at wrap 100 has six lines whose glyph counts are exact
    /// multiples of 100. Under `length / wrap + 1` each claimed one blank row
    /// too many and shoved everything below it down; the 8th line started at
    /// row 271 instead of 267, exactly the four exact-multiple lines above it.
    ///
    /// The expected rows are LITERALS, not recomputed from the formula: a test
    /// that re-derives its expectation from the code under test asserts nothing.
    #[test]
    fn exact_multiple_lines_do_not_push_later_lines_down() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/g-pick-repo/wide.txt");
        let bytes = std::fs::read(&path).expect("wide.txt");
        let t = trie();
        let item = Item {
            byte_start: 0,
            byte_count: bytes.len() as i64,
            wrap_width: 100,
            z_step: 0.25,
            line_height: 1.0,
            ..Item::default()
        };
        let r = run_pipeline(&bytes, &t, &[item]);

        // Byte offset of the first byte of each source line — found by scanning
        // for newlines, which is not the arithmetic under test.
        let mut line_starts = vec![0usize];
        for (id, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                line_starts.push(id + 1);
            }
        }
        // glyph counts: 80, 100, 101, 250, 1000, 5000, 20000, 120000, 250000
        // rows:          1,   1,   2,   3,   10,   50,   200,   1200,   2500
        let want_base_row = [0i64, 1, 2, 4, 7, 17, 67, 267, 1467];
        for (line, &want) in want_base_row.iter().enumerate() {
            let start = line_starts[line];
            assert_eq!(
                r.slots.row(start),
                want,
                "line {line} must start at row {want}, not {}",
                r.slots.row(start)
            );
            assert_eq!(r.slots.col(start), 0, "line {line}'s first glyph is column 0");
        }
        // ANTI-VACUITY: the sweep must really have reached the wide lines.
        assert_eq!(line_starts.len(), 10, "wide.txt has 9 newline-terminated lines");

        // TOTAL_ROWS (bounds lane 6) counts the rows the file actually reaches.
        assert_eq!(r.item_bounds[6], 3967.0, "TOTAL_ROWS: 1467 + 2500 for the last line");
    }

    /// Self-consistency at the seam the fix has TWO halves for: the newline's
    /// own lanes and the next line's base row must agree about where the line
    /// ended. Correcting only `rows_for_line` would leave the newline reporting
    /// a row that belongs to the following line — and its Y and Z reach the
    /// item's bounding box from there.
    #[test]
    fn a_newline_at_an_exact_multiple_shares_its_last_glyph_s_row_and_depth() {
        let t = trie();
        // Two lines of exactly 8 = 2 * wrap cells, then a short one.
        let bytes = b"aaaaaaaa\nbbbbbbbb\ncc";
        let item = Item {
            byte_start: 0,
            byte_count: bytes.len() as i64,
            wrap_width: 4,
            z_step: 0.25,
            line_height: 1.0,
            ..Item::default()
        };
        let r = run_pipeline(bytes, &t, &[item]);

        let last_glyph = 7; // the 8th 'a'
        let newline = 8;
        assert_eq!(r.slots.col(last_glyph), 7);
        assert_eq!(r.slots.col(newline), 8, "the newline still owns column 8");
        assert_eq!(
            r.slots.row(newline),
            r.slots.row(last_glyph),
            "the newline must ride the row it closes"
        );
        assert_eq!(r.slots.row(newline), 1, "8 cells at wrap 4 end on line-local row 1");
        assert_eq!(
            r.slots.y(newline).to_bits(),
            r.slots.y(last_glyph).to_bits(),
            "same row means the same Y"
        );
        assert_eq!(
            r.slots.z(newline).to_bits(),
            r.slots.z(last_glyph).to_bits(),
            "same wrap segment means the same Z"
        );
        // And the second line starts on the row after, not two after.
        assert_eq!(r.slots.row(9), 2, "line 1 starts immediately below line 0");
        assert_eq!(r.slots.row(18), 4, "line 2 starts immediately below line 1");
        // The box's lower edge is the deepest row the file reaches, and the
        // phantom used to drag it one line_height further.
        assert_eq!(r.item_bounds[6], 5.0, "TOTAL_ROWS = 2 + 2 + 1");
        assert_eq!(r.item_bounds[1], -4.0, "box min Y = -(last row) * line_height");
    }

    /// An EMPTY line still occupies the row it sits on — the floor of one in
    /// `rows_for_line`, stated where the pipeline can see it.
    ///
    /// This exists because of what mutation showed: replacing the rule with a
    /// bare ceiling `(n + w - 1) / w` reddened only the two lookup tables. Every
    /// other test compares one form of the fold against another, and both call
    /// the same function, so a shared-rule mutation moves both sides together
    /// and the comparison stays green. Absolute row numbers are the only thing
    /// that can see it from here.
    #[test]
    fn an_empty_line_still_occupies_a_row() {
        let t = trie();
        // Rows, counted by hand at wrap 4:
        //   "abcd"  -> row 0            (exact multiple: ONE row)
        //   ""      -> row 1            (empty: still one row)
        //   ""      -> row 2
        //   "efghi" -> rows 3 and 4
        //   ""      -> row 5
        //   "j"     -> row 6
        let bytes = b"abcd\n\n\nefghi\n\nj";
        let item = Item {
            byte_start: 0,
            byte_count: bytes.len() as i64,
            wrap_width: 4,
            line_height: 1.0,
            ..Item::default()
        };
        let r = run_pipeline(bytes, &t, &[item]);
        // (byte, row) for the first cell of each line, plus each bare newline.
        for (id, want) in [
            (0usize, 0i64), // 'a'
            (4, 0),         // the newline closing "abcd" — rides row 0
            (5, 1),         // the first empty line's newline
            (6, 2),         // the second empty line's newline
            (7, 3),         // 'e'
            (11, 4),        // 'i', wrapped onto row 4
            (12, 4),        // the newline closing "efghi" — rides row 4
            (13, 5),        // the third empty line's newline
            (14, 6),        // 'j'
        ] {
            assert_eq!(r.slots.row(id), want, "byte {id} must sit on row {want}");
        }
        assert_eq!(r.item_bounds[6], 7.0, "TOTAL_ROWS = 1 + 1 + 1 + 2 + 1 + 1");
    }

    /// PAGINATE'S Z, which recomputes the wrap segment from the COL lane and so
    /// needs the terminator rule of its own.
    ///
    /// FOUND BY MUTATION, not by reading: reverting paginate's `wrap_segment` to
    /// a plain `col / wrap` reddened NOTHING. The unpaged tests never reach
    /// paginate, and the paged tuning sweep uses 10-cell lines at wrap 7 — never
    /// a multiple. Worse, a scan-vs-fold comparison structurally cannot catch it:
    /// both forms call the same `paginate`, so the mutation moves both sides
    /// together. The claim has to be stated against something else, so it is
    /// stated against the fold's own rule — a newline at an exact multiple sits
    /// at the depth of the row it closes, not one wrap step behind it.
    ///
    /// `depth_per_band` and `depth_per_col` are zero so Z reduces to the wrap
    /// segment term alone; that is what makes the comparison to the last glyph
    /// meaningful rather than a coincidence of three cancelling terms.
    #[test]
    fn paginate_puts_a_newline_at_its_own_row_s_depth() {
        let t = trie();
        let bytes = b"aaaaaaaa\nbb";
        let item = Item {
            byte_start: 0,
            byte_count: bytes.len() as i64,
            origin_z: 2.0,
            wrap_width: 4,
            z_step: 0.5,
            line_height: 1.0,
            has_page: true,
            page_rows: 2,
            pages_wide: 1,
            ..Item::default() // depth_per_band / depth_per_col / page_cols all 0
        };
        assert!(page_active(&item), "paginate must actually run");
        let r = run_pipeline(bytes, &t, &[item]);

        let last_glyph = 7usize; // col 7, wrap segment 1
        let newline = 8usize; // col 8 — the exact multiple
        assert_eq!(r.slots.col(newline), 8);
        assert_eq!(
            r.slots.z(newline).to_bits(),
            r.slots.z(last_glyph).to_bits(),
            "a newline at an exact multiple shares its last glyph's depth"
        );
        // Stated absolutely too, so it cannot pass by both sides being wrong:
        // segment 1 of a 0.5 step from origin_z 2.0.
        assert_eq!(r.slots.z(newline), 1.5, "origin_z - 1 * z_step");
        // ANTI-VACUITY: Z must really vary with the segment here, or the
        // equality above is about a constant.
        assert_eq!(r.slots.z(0), 2.0, "segment 0 sits at origin_z");
        assert_ne!(r.slots.z(0), r.slots.z(last_glyph), "segments must separate in Z");
        // And the box's near edge follows: with the phantom, the newline
        // reached a segment no glyph occupies and dragged min Z with it.
        assert_eq!(r.item_bounds[2], 1.5, "box min Z = the deepest segment a cell reaches");
    }
}
