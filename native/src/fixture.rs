//! Stage 0 of the reference port — the Rust `.pipe.bin` reader and the
//! bit-exact differ everything after it is gated on.
//!
//! WHY THIS EXISTS BEFORE ANY PORTED CODE. `engine/PORT-PLAN.md` stages the
//! port by ACCEPTANCE TEST, and the first line of ported fold has to land
//! against a working differ rather than before one. Rust could not read a
//! fixture at all until this file: it passed fixture PATHS through to Mojo's
//! `load_trie_auto` and never looked inside.
//!
//! THE FORMAT is `engine/fixture_io.mojo`, mirrored section for section. It is
//! frozen on disk (v3) and deliberately independent of any layer's container:
//! it carries the oracle's VALUES, and each loader realizes its own carriers.
//! This one performs the same carrier split the Mojo loader does — trie
//! measures narrowed f64 -> f32, identity and bitfield to native u32 — because
//! a checker that carries values differently from the thing it checks is a
//! second implementation with its own bugs.
//!
//! HOW THE PARSE IS CHECKED, given there is no second Rust parser to disagree
//! with it: `manifest()` emits FNV-1a checksums over the PARSED, TYPED values
//! of every section, and `engine/fixture_manifest.mojo` emits the same lines
//! from the Mojo loader. `tools/check-fixture-parity.sh` diffs the two. That is
//! a genuine two-implementation agreement rather than a file hashed against
//! itself: the checksums are computed after the strides, field order, and
//! carrier splits have been applied, so getting any of them wrong diverges.

use std::fmt::Write as _;
use std::path::Path;

use crate::fold::{run_pipeline, Item};
use crate::scan::run_scan_pipeline;
use crate::glyph_trie::{build_glyph_trie, GlyphMetrics, BLOCK_SHIFT, ENTRY_LANES};
use crate::text::{ResolveGlyph, WorldEntry};

/// 'G3DF' — a pipeline fixture.
const PIPE_MAGIC: u32 = 0x4644_3347;
const PIPE_VERSION: u32 = 3;

/// Fixture lane strides. These are the ON-DISK strides from
/// `schema/glyph-identity.json` (FIXTURE_MEASURE_STRIDE / FIXTURE_COUNT_STRIDE),
/// NOT any engine buffer's stride. Do not "unify" them with the container
/// strides in glyph_schema — they are frozen for different reasons.
pub const FIXTURE_MEASURE_STRIDE: usize = 8;
pub const FIXTURE_COUNT_STRIDE: usize = 4;

// The lane sets below are COMPLETE on purpose, not as-needed: they are the
// on-disk schema, and a partial transcription is how a stride quietly means two
// things. LINE_ADV, ORD and F_MISSING have no non-test caller yet — stages 2-4
// bring them in — and dropping them now would only mean re-deriving them later
// from the same source.
#[allow(dead_code)]
pub const FIX_M_X: usize = 0;
#[allow(dead_code)]
pub const FIX_M_Y: usize = 1;
#[allow(dead_code)]
pub const FIX_M_Z: usize = 2;
#[allow(dead_code)]
pub const FIX_M_ADVANCE: usize = 3;
#[allow(dead_code)]
pub const FIX_M_HEIGHT: usize = 4;
#[allow(dead_code)]
pub const FIX_M_GLYPH_ID: usize = 5;
#[allow(dead_code)]
pub const FIX_M_BASE_X: usize = 6;
#[allow(dead_code)]
pub const FIX_M_LINE_ADV: usize = 7;

#[allow(dead_code)]
pub const FIX_C_ROW: usize = 0;
#[allow(dead_code)]
pub const FIX_C_COL: usize = 1;
#[allow(dead_code)]
pub const FIX_C_FLAGS: usize = 2;
#[allow(dead_code)]
pub const FIX_C_ORD: usize = 3;

/// Fold flags, from `glyph_pipeline.mojo`.
pub const F_LEADER: u32 = 1;
#[allow(dead_code)]
pub const F_MISSING: u32 = 8;

/// FIXTURE lane name for diagnostics — fixture order, not container order.
/// Mirrors `fixture_measure_lane_name` in glyph_schema.mojo.
pub fn measure_lane_name(lane: usize) -> &'static str {
    match lane {
        0 => "X",
        1 => "Y",
        2 => "Z",
        3 => "ADVANCE",
        4 => "HEIGHT",
        5 => "GLYPH_ID",
        6 => "BASE_X",
        7 => "LINE_ADV",
        _ => "M_?",
    }
}

pub fn count_lane_name(lane: usize) -> &'static str {
    match lane {
        0 => "ROW",
        1 => "COL",
        2 => "FLAGS",
        3 => "ORD",
        _ => "C_?",
    }
}

/// `trunc_nonneg` from glyph_pipeline.mojo: `max(0, trunc(v || 0))`, the
/// oracle's boundary semantics. Applied ONCE here, where f64 VALUES enter,
/// rather than at every read site.
fn trunc_nonneg(v: f64) -> i64 {
    if v.is_nan() || v <= 0.0 {
        0
    } else {
        v.trunc() as i64
    }
}

/// Little-endian packed reader. Every read is bounds-checked and reports the
/// offset it wanted, so a truncated or misparsed file names its own failure
/// instead of panicking somewhere downstream.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn need(&self, n: usize, what: &str) -> Result<(), String> {
        if self.at + n > self.data.len() {
            Err(format!(
                "truncated at offset {} reading {} ({} bytes needed, {} left)",
                self.at,
                what,
                n,
                self.data.len() - self.at
            ))
        } else {
            Ok(())
        }
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.need(4, "u32")?;
        let v = u32::from_le_bytes(self.data[self.at..self.at + 4].try_into().unwrap());
        self.at += 4;
        Ok(v)
    }

    fn u64(&mut self) -> Result<u64, String> {
        let lo = self.u32()? as u64;
        let hi = self.u32()? as u64;
        Ok(lo | (hi << 32))
    }

    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn take_bytes(&mut self, n: usize) -> Result<Vec<u8>, String> {
        self.need(n, "byte block")?;
        let v = self.data[self.at..self.at + n].to_vec();
        self.at += n;
        Ok(v)
    }
}

/// The fixture's own trie, in the carriers this layer realizes.
///
/// On disk a v3 fixture stores blocks as f64 VALUES in entry-major lane order
/// [GLYPH_ID, ADVANCE, HEIGHT, FLAGS] — which is exactly why the corpus
/// survived the trie's container moving on BOTH sides of the oracle. The split
/// below is this loader's realization and matches `fixture_io.mojo`'s.
pub struct FixtureTrie {
    pub block_index: Vec<u32>,
    /// measures, 2 per entry: [ADVANCE, HEIGHT]
    pub blocks_m: Vec<f32>,
    /// identity + bitfield, 2 per entry: [GLYPH_ID, FLAGS]
    pub blocks_c: Vec<u32>,
}

impl ResolveGlyph for FixtureTrie {
    /// The same two dependent loads as `TrieTable::lookup`, including the
    /// out-of-range contract settled in 0ae7010: a codepoint past the last
    /// Unicode scalar resolves through storage block 0, the shared missing
    /// block, rather than reading off the end of the block index.
    fn resolve(&self, cp: u32) -> WorldEntry {
        let block = if cp <= 0x10FFFF {
            self.block_index[(cp >> BLOCK_SHIFT) as usize]
        } else {
            0
        };
        let e = ((block << BLOCK_SHIFT) | (cp & 0xFF)) as usize;
        WorldEntry {
            glyph_id: self.blocks_c[e * 2],
            advance: self.blocks_m[e * 2],
            height: self.blocks_m[e * 2 + 1],
            flags: self.blocks_c[e * 2 + 1],
        }
    }
}

/// One 'G3DF' case: inputs (bytes, trie, items) + the oracle's expected outputs.
pub struct PipeFixture {
    pub name: String,
    pub byte_len: usize,
    pub item_count: usize,
    pub bytes: Vec<u8>,
    pub trie: FixtureTrie,
    pub items: Vec<Item>,
    pub exp_leaders: u32,
    pub exp_misses: Vec<u32>,
    pub exp_ord_to_byte: Vec<u32>,
    /// VALUES (f64 carrier) — narrowed to f32 at COMPARISON, once.
    pub exp_measures: Vec<f64>,
    /// EXACT — counts have no carrier question.
    pub exp_counts: Vec<u32>,
    pub exp_item_bounds: Vec<u64>,
    pub exp_batch: Vec<u64>,
}

pub fn load_pipe_fixture(path: &Path) -> Result<PipeFixture, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    load_pipe_bytes(&raw, name).map_err(|e| format!("{}: {e}", path.display()))
}

fn load_pipe_bytes(raw: &[u8], name: String) -> Result<PipeFixture, String> {
    let mut r = Reader::new(raw);

    if r.u32()? != PIPE_MAGIC {
        return Err("bad magic (not a .pipe.bin fixture)".into());
    }
    let version = r.u32()?;
    if version != PIPE_VERSION {
        return Err(format!(
            "unknown fixture version {version} (expected v{PIPE_VERSION} — regenerate)"
        ));
    }

    let byte_len = r.u32()? as usize;
    let item_count = r.u32()? as usize;
    let block_index_len = r.u32()? as usize;
    let blocks_len = r.u32()? as usize;

    let bytes = r.take_bytes(byte_len)?;

    let mut block_index = Vec::with_capacity(block_index_len);
    for _ in 0..block_index_len {
        block_index.push(r.u32()?);
    }

    let entries = blocks_len / 4;
    let mut blocks_m = Vec::with_capacity(entries * 2);
    let mut blocks_c = Vec::with_capacity(entries * 2);
    for _ in 0..entries {
        let gid = r.f64()?;
        let adv = r.f64()?;
        let h = r.f64()?;
        let fl = r.f64()?;
        // THE CARRIER SPLIT, matching fixture_io.mojo: measures narrow to f32
        // (exact for anything that was f32 to begin with), identity and
        // bitfield become native u32.
        blocks_m.push(adv as f32);
        blocks_m.push(h as f32);
        blocks_c.push(gid as u32);
        blocks_c.push(fl as u32);
    }

    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        items.push(Item {
            byte_start: r.u32()? as i64,
            byte_count: r.u32()? as i64,
            origin_x: r.f64()?,
            origin_y: r.f64()?,
            origin_z: r.f64()?,
            // THE BOUNDARY: v3 carries item params as f64 VALUES; the five
            // integer page-geometry params truncate HERE, once.
            wrap_width: trunc_nonneg(r.f64()?),
            z_step: r.f64()?,
            line_height: r.f64()?,
            has_page: r.f64()? > 0.5,
            page_rows: trunc_nonneg(r.f64()?),
            page_cols: trunc_nonneg(r.f64()?),
            scroll_rows: trunc_nonneg(r.f64()?),
            pages_wide: trunc_nonneg(r.f64()?),
            page_gap_x: r.f64()?,
            band_stride_y: r.f64()?,
            depth_per_band: r.f64()?,
            depth_per_col: r.f64()?,
            page_line_height: r.f64()?,
        });
    }

    let exp_leaders = r.u32()?;
    let miss_count = r.u32()? as usize;
    let mut exp_misses = Vec::with_capacity(miss_count);
    for _ in 0..miss_count {
        exp_misses.push(r.u32()?);
    }
    let mut exp_ord_to_byte = Vec::with_capacity(byte_len);
    for _ in 0..byte_len {
        exp_ord_to_byte.push(r.u32()?);
    }
    let mut exp_measures = Vec::with_capacity(byte_len * FIXTURE_MEASURE_STRIDE);
    for _ in 0..byte_len * FIXTURE_MEASURE_STRIDE {
        exp_measures.push(r.f64()?);
    }
    let mut exp_counts = Vec::with_capacity(byte_len * FIXTURE_COUNT_STRIDE);
    for _ in 0..byte_len * FIXTURE_COUNT_STRIDE {
        exp_counts.push(r.u32()?);
    }
    let mut exp_item_bounds = Vec::with_capacity(item_count * 8);
    for _ in 0..item_count * 8 {
        exp_item_bounds.push(r.u64()?);
    }
    let mut exp_batch = Vec::with_capacity(8);
    for _ in 0..8 {
        exp_batch.push(r.u64()?);
    }

    // THE STRUCTURAL CHECK the checksums cannot make. Every section length is
    // derived from a header field, so a wrong stride or a swapped section reads
    // plausible values and stops in the wrong place. Full consumption is the
    // one statement that catches it without a second opinion.
    if r.at != raw.len() {
        return Err(format!(
            "parse consumed {} of {} bytes — {} left over (stride or section order wrong)",
            r.at,
            raw.len(),
            raw.len() - r.at
        ));
    }

    Ok(PipeFixture {
        name,
        byte_len,
        item_count,
        bytes,
        trie: FixtureTrie {
            block_index,
            blocks_m,
            blocks_c,
        },
        items,
        exp_leaders,
        exp_misses,
        exp_ord_to_byte,
        exp_measures,
        exp_counts,
        exp_item_bounds,
        exp_batch,
    })
}

// ── The parity manifest ───────────────────────────────────────────────────
//
// FNV-1a 64, fed the LITTLE-ENDIAN BIT PATTERN of each parsed value in section
// order. Two properties are wanted and a plain sum has neither: it must be
// order-sensitive (a section read in the wrong order must diverge) and
// bit-sensitive (a value narrowed one step too early must diverge). Hashing
// the FILE would have neither property that matters here — it would agree no
// matter how wrongly either side parsed it.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(FNV_OFFSET)
    }
}

impl Fnv {
    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME);
    }
    fn u8(&mut self, v: u8) {
        self.byte(v);
    }
    fn u32(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }
    fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }
    fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
    fn hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl PipeFixture {
    /// One canonical line per fixture. `engine/fixture_manifest.mojo` emits the
    /// identical line from the Mojo loader; `tools/check-fixture-parity.sh`
    /// diffs them. Keep the two emitters in lockstep — the format is the
    /// comparison.
    pub fn manifest(&self) -> String {
        let mut h_bytes = Fnv::default();
        for &b in &self.bytes {
            h_bytes.u8(b);
        }
        let mut h_tindex = Fnv::default();
        for &v in &self.trie.block_index {
            h_tindex.u32(v);
        }
        let mut h_tm = Fnv::default();
        for &v in &self.trie.blocks_m {
            h_tm.f32(v);
        }
        let mut h_tc = Fnv::default();
        for &v in &self.trie.blocks_c {
            h_tc.u32(v);
        }
        let mut h_items = Fnv::default();
        for it in &self.items {
            h_items.i64(it.byte_start);
            h_items.i64(it.byte_count);
            h_items.f64(it.origin_x);
            h_items.f64(it.origin_y);
            h_items.f64(it.origin_z);
            h_items.i64(it.wrap_width);
            h_items.f64(it.z_step);
            h_items.f64(it.line_height);
            h_items.u8(u8::from(it.has_page));
            h_items.i64(it.page_rows);
            h_items.i64(it.page_cols);
            h_items.i64(it.scroll_rows);
            h_items.i64(it.pages_wide);
            h_items.f64(it.page_gap_x);
            h_items.f64(it.band_stride_y);
            h_items.f64(it.depth_per_band);
            h_items.f64(it.depth_per_col);
            h_items.f64(it.page_line_height);
        }
        let mut h_miss = Fnv::default();
        for &v in &self.exp_misses {
            h_miss.u32(v);
        }
        let mut h_otb = Fnv::default();
        for &v in &self.exp_ord_to_byte {
            h_otb.u32(v);
        }
        let mut h_meas = Fnv::default();
        for &v in &self.exp_measures {
            h_meas.f64(v);
        }
        let mut h_cnt = Fnv::default();
        for &v in &self.exp_counts {
            h_cnt.u32(v);
        }
        let mut h_bnds = Fnv::default();
        for &v in &self.exp_item_bounds {
            h_bnds.u64(v);
        }
        let mut h_batch = Fnv::default();
        for &v in &self.exp_batch {
            h_batch.u64(v);
        }

        let mut s = String::new();
        let _ = write!(
            s,
            "{} bytes={} items={} tindex={} tentries={} leaders={} misses={}",
            self.name,
            self.byte_len,
            self.item_count,
            self.trie.block_index.len(),
            self.trie.blocks_c.len() / 2,
            self.exp_leaders,
            self.exp_misses.len()
        );
        let _ = write!(
            s,
            " h.bytes={} h.tindex={} h.tm={} h.tc={} h.items={} h.miss={} h.otb={} h.meas={} h.cnt={} h.bnds={} h.batch={}",
            h_bytes.hex(),
            h_tindex.hex(),
            h_tm.hex(),
            h_tc.hex(),
            h_items.hex(),
            h_miss.hex(),
            h_otb.hex(),
            h_meas.hex(),
            h_cnt.hex(),
            h_bnds.hex(),
            h_batch.hex()
        );
        s
    }

    /// Byte offsets the oracle marked as leaders, in order. Taken from the
    /// fixture's own FLAGS lane rather than re-deriving them with a second copy
    /// of the decode — a differ that recomputes its own reference points can
    /// agree with a port that shares the mistake.
    pub fn leader_bytes(&self) -> Vec<usize> {
        (0..self.byte_len)
            .filter(|&i| self.exp_counts[i * FIXTURE_COUNT_STRIDE + FIX_C_FLAGS] & F_LEADER != 0)
            .collect()
    }
}

// ── The differ ────────────────────────────────────────────────────────────

/// One disagreeing lane, named the way the Mojo suites name them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mismatch {
    pub byte: usize,
    pub lane: usize,
    pub lane_name: &'static str,
    /// f32 bits for a measure lane, the value itself for a count lane.
    pub got_bits: u32,
    pub exp_bits: u32,
}

/// Compare measure lanes at selected byte offsets.
///
/// THE NARROWING POINT: the fixture carries f64 VALUES and the engine carries
/// f32, so the expectation narrows to f32 ONCE, here, and the comparison is on
/// BITS. That is the same comparison every Mojo suite makes, and it is
/// deliberate: bit equality is the contract, not approximate agreement.
///
/// `got` is row-major `[position][lane]` over `positions` x `lanes`.
pub fn diff_measures_at(
    fx: &PipeFixture,
    positions: &[usize],
    lanes: &[usize],
    got: &[f32],
) -> Vec<Mismatch> {
    assert_eq!(
        got.len(),
        positions.len() * lanes.len(),
        "diff_measures_at: got buffer is {} for {} positions x {} lanes",
        got.len(),
        positions.len(),
        lanes.len()
    );
    let mut out = Vec::new();
    for (p, &byte) in positions.iter().enumerate() {
        for (l, &lane) in lanes.iter().enumerate() {
            let g = got[p * lanes.len() + l];
            let e = fx.exp_measures[byte * FIXTURE_MEASURE_STRIDE + lane] as f32;
            if g.to_bits() != e.to_bits() {
                out.push(Mismatch {
                    byte,
                    lane,
                    lane_name: measure_lane_name(lane),
                    got_bits: g.to_bits(),
                    exp_bits: e.to_bits(),
                });
            }
        }
    }
    out
}

/// Compare count lanes at selected byte offsets. Counts are exact on both
/// sides, so this is a plain integer comparison with no carrier question.
pub fn diff_counts_at(
    fx: &PipeFixture,
    positions: &[usize],
    lanes: &[usize],
    got: &[u32],
) -> Vec<Mismatch> {
    assert_eq!(
        got.len(),
        positions.len() * lanes.len(),
        "diff_counts_at: got buffer is {} for {} positions x {} lanes",
        got.len(),
        positions.len(),
        lanes.len()
    );
    let mut out = Vec::new();
    for (p, &byte) in positions.iter().enumerate() {
        for (l, &lane) in lanes.iter().enumerate() {
            let g = got[p * lanes.len() + l];
            let e = fx.exp_counts[byte * FIXTURE_COUNT_STRIDE + lane];
            if g != e {
                out.push(Mismatch {
                    byte,
                    lane,
                    lane_name: count_lane_name(lane),
                    got_bits: g,
                    exp_bits: e,
                });
            }
        }
    }
    out
}

/// Render mismatches for a human, capped so a systematic break does not bury
/// the count that says how systematic it is.
pub fn report(fx: &PipeFixture, bad: &[Mismatch], cap: usize) -> String {
    let mut s = format!("{}: {} lane(s) differ\n", fx.name, bad.len());
    for m in bad.iter().take(cap) {
        let _ = writeln!(
            s,
            "  byte {:>6} lane {} ({}): got {:#010x} ({}) expected {:#010x} ({})",
            m.byte,
            m.lane,
            m.lane_name,
            m.got_bits,
            f32::from_bits(m.got_bits),
            m.exp_bits,
            f32::from_bits(m.exp_bits),
        );
    }
    if bad.len() > cap {
        let _ = writeln!(s, "  ... and {} more", bad.len() - cap);
    }
    s
}

// ── Driving the existing Rust fold against the corpus ─────────────────────

/// Measure lanes `reference_layout` can actually produce.
///
/// LINE_ADV (lane 7) is absent ON PURPOSE and this list is the honest
/// statement of that: it is the fold's WITNESS lane, written only under the
/// witness instantiation, and `RefGlyph` does not carry it. Seven of eight
/// measure lanes are compared; claiming eight would be a label doing work the
/// code does not.
const REF_MEASURE_LANES: [usize; 7] = [
    FIX_M_X,
    FIX_M_Y,
    FIX_M_Z,
    FIX_M_ADVANCE,
    FIX_M_HEIGHT,
    FIX_M_GLYPH_ID,
    FIX_M_BASE_X,
];
const REF_COUNT_LANES: [usize; 2] = [FIX_C_ROW, FIX_C_COL];

pub struct DiffOutcome {
    /// `None` when the fixture is inside `reference_layout`'s domain.
    pub skipped: Option<String>,
    pub records: usize,
    pub compared_lanes: usize,
    pub bad: Vec<Mismatch>,
}

/// Is this fixture inside `reference_layout`'s domain — one item covering the
/// whole buffer, no wrap, no pagination?
///
/// The reason to state this as a predicate rather than a hardcoded file list:
/// stage 2 widens the fold, and the set of fixtures it can be held to should
/// widen with it automatically. A list would have to be remembered.
fn out_of_domain(fx: &PipeFixture) -> Option<String> {
    if fx.item_count != 1 {
        return Some(format!("{} items (reference_layout lays one)", fx.item_count));
    }
    let it = &fx.items[0];
    if it.byte_start != 0 || it.byte_count != fx.byte_len as i64 {
        return Some(format!(
            "item covers [{}, {}) of {} bytes",
            it.byte_start,
            it.byte_start + it.byte_count,
            fx.byte_len
        ));
    }
    if it.wrap_width != 0 {
        return Some(format!("wrap_width={}", it.wrap_width));
    }
    if it.has_page {
        return Some("paged".to_string());
    }
    // wrap=0 makes wrap_row 0, so Z is `Float32(-0.0 * z_step + oz)`. That is
    // oz for every finite z_step and NaN for an infinite one — a case no
    // fixture has, and one this reference does not model.
    if !it.z_step.is_finite() {
        return Some(format!("z_step={} (non-finite)", it.z_step));
    }
    None
}

/// Lay the fixture with `text::reference_layout` and diff BIT-EXACT against the
/// oracle's own expected lanes.
///
/// This is what makes stage 0 more than plumbing. The differ could have been
/// landed with nothing to point at but the corpus compared to itself — which is
/// the shape of check this repo has shipped three times and had to go back and
/// break. Instead it runs against a real implementation on real fixtures the
/// day it exists.
pub fn diff_against_reference_layout(fx: &PipeFixture) -> DiffOutcome {
    if let Some(why) = out_of_domain(fx) {
        return DiffOutcome {
            skipped: Some(why),
            records: 0,
            compared_lanes: 0,
            bad: Vec::new(),
        };
    }
    let it = &fx.items[0];
    let glyphs = crate::text::reference_layout(
        &fx.trie,
        &fx.bytes,
        [it.origin_x, it.origin_y, it.origin_z],
        it.line_height,
    );

    let positions = fx.leader_bytes();
    if glyphs.len() != positions.len() {
        return DiffOutcome {
            skipped: None,
            records: glyphs.len(),
            compared_lanes: 0,
            // A record-count disagreement is not a lane disagreement, so it gets
            // reported as its own thing rather than smuggled in as a fake lane.
            bad: vec![Mismatch {
                byte: usize::MAX,
                lane: usize::MAX,
                lane_name: "RECORD_COUNT",
                got_bits: glyphs.len() as u32,
                exp_bits: positions.len() as u32,
            }],
        };
    }

    let mut got_m = Vec::with_capacity(glyphs.len() * REF_MEASURE_LANES.len());
    let mut got_c = Vec::with_capacity(glyphs.len() * REF_COUNT_LANES.len());
    for g in &glyphs {
        for &lane in &REF_MEASURE_LANES {
            got_m.push(match lane {
                FIX_M_X => g.x,
                FIX_M_Y => g.y,
                FIX_M_Z => g.z,
                FIX_M_ADVANCE => g.advance,
                FIX_M_HEIGHT => g.height,
                // GLYPH_ID rides a measure lane in the FIXTURE format (the
                // container it moved out of on 2026-08-31 is this layer's, not
                // the file's). It is an identity: convert, do not reinterpret.
                FIX_M_GLYPH_ID => g.glyph_id as f32,
                // BASE_X and X carry the same value at fold time — paginate is
                // what later separates them, and nothing paginates here.
                FIX_M_BASE_X => g.x,
                _ => unreachable!("lane not produced by reference_layout"),
            });
        }
        got_c.push(g.row);
        got_c.push(g.col);
    }

    let mut bad = diff_measures_at(fx, &positions, &REF_MEASURE_LANES, &got_m);
    bad.extend(diff_counts_at(fx, &positions, &REF_COUNT_LANES, &got_c));
    DiffOutcome {
        skipped: None,
        records: glyphs.len(),
        compared_lanes: positions.len() * (REF_MEASURE_LANES.len() + REF_COUNT_LANES.len()),
        bad,
    }
}

// ── Stage 1: rebuilding a fixture's trie from its own bytes ───────────────
//
// THE RECIPE lives in engine/fixtures/gen.mjs, which is vendored in this tree,
// so the whole input is reconstructible here: the codepoint set comes from the
// fixture's own bytes and the metrics are a pure function of the codepoint.
// That makes this a real acceptance test rather than a round trip — the input
// is raw bytes and the expected output is the oracle's trie, with nothing of
// the trie's own structure handed back to the builder.

/// `MISSING_ADVANCE` / `MISSING_HEIGHT` from gen.mjs — `Math.fround(0.61)` and
/// `Math.fround(1.25)`, which are these f32 literals.
pub const FIXTURE_MISSING_ADVANCE: f32 = 0.61;
pub const FIXTURE_MISSING_HEIGHT: f32 = 1.25;

/// gen.mjs's `metricsFor`: synthetic metrics with awkward f32 mantissas, so
/// every advance-sum exercises real rounding. `'@'` is deliberately unmapped
/// (the F_MISSING path) and emoji advance is doubled (the "x is a lookup, not a
/// multiply" case).
///
/// FLOAT DISCIPLINE: `Math.fround(expr)` evaluates `expr` in f64 and narrows
/// ONCE. Writing these in f32 throughout would round at every operator and
/// diverge — the same hazard as landmine 2, one layer earlier.
pub fn fixture_metrics(cp: u32) -> Option<GlyphMetrics> {
    if cp == 0x40 {
        return None; // '@'
    }
    let emoji = cp >= 0x1F300;
    let advance = ((0.6 + (cp % 13) as f64 * 0.0173) * if emoji { 2.0 } else { 1.0 }) as f32;
    let height = (1.2 + (cp % 7) as f64 * 0.031) as f32;
    Some(GlyphMetrics {
        glyph_id: (cp % 4093) + 1,
        advance,
        height,
    })
}

/// The codepoints gen.mjs would feed the builder, IN ORDER.
///
/// Order is load-bearing (landmine 1), and this is where it comes from: gen.mjs
/// decodes with `new TextDecoder('utf-8', {fatal: false})` and collects into a
/// JS `Set`, which preserves first-insertion order.
///
/// `from_utf8_lossy` is the right counterpart precisely BECAUSE it is the
/// conformant WHATWG replacement decoder, unlike the engine's lenient
/// classifier — it is deriving the trie's key set, not folding text. The two
/// decoders must not be confused for each other, and `malformed.pipe.bin`
/// (stray continuations, an invalid byte, a truncated sequence) is what proves
/// this one is right.
///
/// gen.mjs adds U+FFFD to the set and then deletes it; skipping it here is the
/// same net effect, including for a literal U+FFFD in the source.
pub fn fixture_codepoints(bytes: &[u8]) -> Vec<u32> {
    let text = String::from_utf8_lossy(bytes);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for ch in text.chars() {
        let cp = ch as u32;
        if cp == 0xFFFD {
            continue;
        }
        if seen.insert(cp) {
            out.push(cp);
        }
    }
    out
}

/// What a rebuild found, for reporting.
pub struct TrieRebuild {
    pub entries: usize,
    pub block_count: usize,
    pub mapped: usize,
    pub bad: Vec<String>,
}

/// Rebuild this fixture's trie from its BYTES and diff against the trie the
/// oracle stored.
///
/// The comparison goes through `wire_value`, not through the storage arrays
/// directly. That is deliberate: the fixture format IS wire order
/// ([GLYPH_ID, ADVANCE, HEIGHT, FLAGS]) and `wire_value` is the single place
/// that mapping lives, so a transposed lane in the serializer fails here.
/// Comparing the split arrays element-wise would have skipped the mapping
/// entirely — checking the values while assuming the thing that orders them.
///
/// Widening the fixture's side to f64 is lossless (f32 and u32 both fit
/// exactly), so no rounding is introduced by the comparison itself.
pub fn rebuild_trie_and_diff(fx: &PipeFixture) -> TrieRebuild {
    let built = build_glyph_trie(
        fixture_codepoints(&fx.bytes),
        fixture_metrics,
        FIXTURE_MISSING_ADVANCE,
        FIXTURE_MISSING_HEIGHT,
    );
    let mut bad = Vec::new();
    let entries = fx.trie.blocks_c.len() / 2;

    if built.block_index.len() != fx.trie.block_index.len() {
        bad.push(format!(
            "block_index length: built {} vs fixture {}",
            built.block_index.len(),
            fx.trie.block_index.len()
        ));
    } else {
        for (b, (&g, &e)) in built
            .block_index
            .iter()
            .zip(fx.trie.block_index.iter())
            .enumerate()
        {
            if g != e {
                bad.push(format!(
                    "block_index[{b}] (cp {:#x}..): built {g} vs fixture {e}",
                    b << BLOCK_SHIFT
                ));
            }
        }
    }

    if built.wire_len() != entries * ENTRY_LANES {
        bad.push(format!(
            "entry count: built {} vs fixture {entries}",
            built.wire_len() / ENTRY_LANES
        ));
        return TrieRebuild {
            entries: 0,
            block_count: built.block_count,
            mapped: built.mapped,
            bad,
        };
    }

    for i in 0..entries * ENTRY_LANES {
        let e = i / ENTRY_LANES;
        let want: f64 = match i % ENTRY_LANES {
            0 => fx.trie.blocks_c[e * 2] as f64,
            1 => fx.trie.blocks_m[e * 2] as f64,
            2 => fx.trie.blocks_m[e * 2 + 1] as f64,
            _ => fx.trie.blocks_c[e * 2 + 1] as f64,
        };
        let got = built.wire_value(i);
        if got.to_bits() != want.to_bits() {
            bad.push(format!(
                "entry {e} lane {} ({}): built {got} vs fixture {want}",
                i % ENTRY_LANES,
                ["GLYPH_ID", "ADVANCE", "HEIGHT", "FLAGS"][i % ENTRY_LANES],
            ));
        }
    }

    TrieRebuild {
        entries,
        block_count: built.block_count,
        mapped: built.mapped,
        bad,
    }
}

// ── Stage 2: the whole fold against the whole corpus ─────────────────────

/// What a full-fold comparison found.
pub struct FoldDiff {
    pub bytes: usize,
    pub leaders: usize,
    /// Per-byte lanes compared: 8 measure + 4 count.
    pub lanes: usize,
    pub bad: Vec<String>,
}

/// The number of per-byte lanes this comparison covers. All of them — stage 0
/// could not produce BASE_X's twin LINE_ADV or the FLAGS/ORD witness lanes, and
/// stage 2 has no such gap, so nothing here is "compared where convenient."
const FOLD_MEASURE_LANES: usize = FIXTURE_MEASURE_STRIDE;
const FOLD_COUNT_LANES: usize = FIXTURE_COUNT_STRIDE;

/// Run the ported fold over a fixture and compare EVERY lane of EVERY byte,
/// plus the miss list, leader count, per-item boxes and batch union.
///
/// Non-leader bytes are compared too, not skipped: zero is their DEFINED state
/// (decode zeroes their static lanes, the fold zeroes their positional ones,
/// the gap sweep covers bytes no item claims), so a port that left them dirty
/// must fail. Skipping them would have quietly excused a whole third of the
/// coverage contract.
pub fn diff_full_fold(fx: &PipeFixture) -> FoldDiff {
    let r = run_pipeline(&fx.bytes, &fx.trie, &fx.items);
    let mut bad = Vec::new();

    if r.leaders != fx.exp_leaders as usize {
        bad.push(format!("leaders: got {} vs fixture {}", r.leaders, fx.exp_leaders));
    }
    if r.misses != fx.exp_misses {
        bad.push(format!(
            "miss list: got {} entries vs fixture {} (first divergence at {:?})",
            r.misses.len(),
            fx.exp_misses.len(),
            r.misses
                .iter()
                .zip(fx.exp_misses.iter())
                .position(|(a, b)| a != b)
        ));
    }

    for id in 0..fx.byte_len {
        for lane in 0..FOLD_MEASURE_LANES {
            let exp = fx.exp_measures[id * FIXTURE_MEASURE_STRIDE + lane];
            if lane == FIX_M_GLYPH_ID {
                // GLYPH_ID IS EXACT. The Mojo's `m_at` deliberately REFUSES to
                // return an f32 view of this lane — "a checker must carry it the
                // way the pipeline does" — so it is compared as u32 here. An f32
                // comparison would be a second carrier with its own rounding,
                // and the whole reason this lane moved out of a float array is
                // that a container's mistake travels to everything it feeds.
                let got = r.slots.gi[id];
                let want = exp as u32;
                if got != want {
                    bad.push(format!("byte {id} GLYPH_ID: got {got} vs fixture {want}"));
                }
                continue;
            }
            // The narrowing point: the fixture carries f64 VALUES, the engine
            // carries f32, so the expectation narrows ONCE and the comparison
            // is on BITS.
            let got = match lane {
                FIX_M_X => r.slots.x(id),
                FIX_M_Y => r.slots.y(id),
                FIX_M_Z => r.slots.z(id),
                FIX_M_ADVANCE => r.slots.advance(id),
                FIX_M_HEIGHT => r.slots.height(id),
                FIX_M_BASE_X => r.slots.base_x(id),
                _ => r.slots.wm[id], // LINE_ADV, the witness lane
            };
            let want = exp as f32;
            if got.to_bits() != want.to_bits() {
                bad.push(format!(
                    "byte {id} lane {lane} ({}): got {:#010x} ({got}) vs fixture {:#010x} ({want})",
                    measure_lane_name(lane),
                    got.to_bits(),
                    want.to_bits()
                ));
            }
        }
        for lane in 0..FOLD_COUNT_LANES {
            let want = fx.exp_counts[id * FIXTURE_COUNT_STRIDE + lane];
            let got = match lane {
                FIX_C_ROW => r.slots.lc[id * 2],
                FIX_C_COL => r.slots.lc[id * 2 + 1],
                FIX_C_FLAGS => r.slots.fl[id],
                _ => r.slots.wc[id], // ORD, the witness lane
            };
            if got != want {
                bad.push(format!(
                    "byte {id} lane {lane} ({}): got {got} vs fixture {want}",
                    count_lane_name(lane)
                ));
            }
        }
        // ordToByte is the INVERSE map (ord -> byte), not a second copy of the
        // ORD lane. It is indexed by `byte_start + ord` within each item, and
        // the fold fills only that prefix — the tail stays zero, which is why
        // the array is zero-initialized rather than left uninitialized.
        //
        // Comparing it to the ORD lane is what the first version of this check
        // did, on the strength of the loader field being named `exp_ord`. Every
        // multi-byte fixture reddened and the FIX_C_ORD lane beside it passed,
        // which is what named the mistake. The field is `exp_ord_to_byte` now.
        if r.slots.ord_to_byte[id] != fx.exp_ord_to_byte[id] {
            bad.push(format!(
                "byte {id} ordToByte: got {} vs fixture {}",
                r.slots.ord_to_byte[id], fx.exp_ord_to_byte[id]
            ));
        }
    }

    // Boxes and the batch union arrive as u64 BIT PATTERNS, so they compare as
    // bits — which also means an infinity or a NaN is caught like any other
    // value rather than slipping through a float compare.
    for (i, &want_bits) in fx.exp_item_bounds.iter().enumerate() {
        let got = r.item_bounds[i];
        if got.to_bits() != want_bits {
            bad.push(format!(
                "item {} box lane {}: got {} ({:#018x}) vs fixture {} ({:#018x})",
                i / 8,
                i % 8,
                got,
                got.to_bits(),
                f64::from_bits(want_bits),
                want_bits
            ));
        }
    }
    for (l, &want_bits) in fx.exp_batch.iter().enumerate() {
        let got = r.batch_bounds[l];
        if got.to_bits() != want_bits {
            bad.push(format!(
                "batch lane {l}: got {got} ({:#018x}) vs fixture {} ({:#018x})",
                got.to_bits(),
                f64::from_bits(want_bits),
                want_bits
            ));
        }
    }

    FoldDiff {
        bytes: fx.byte_len,
        leaders: r.leaders,
        lanes: fx.byte_len * (FOLD_MEASURE_LANES + FOLD_COUNT_LANES),
        bad,
    }
}

// ── Stage 3: the scan form against the corpus, tiered ────────────────────

/// Relative tolerance for the one tiered lane family.
const REL_EPS: f64 = 1e-4;

/// Bits-equal first, which covers +-inf and exact equality; otherwise relative
/// against the EXPECTED magnitude, floored at 1 so small values get an absolute
/// tolerance rather than a divide-by-nothing.
fn rel_close(expected: f64, got: f64) -> bool {
    if expected.to_bits() == got.to_bits() {
        return true;
    }
    let magnitude = expected.abs().max(1.0);
    (expected - got).abs() / magnitude <= REL_EPS
}

fn item_fold_unit(item: &Item) -> i64 {
    if item.wrap_width > 0 {
        item.wrap_width
    } else if item.has_page {
        item.page_cols
    } else {
        0
    }
}

pub struct ScanDiff {
    pub chunk_size: usize,
    pub group_size: usize,
    pub shards: usize,
    /// Leader slots whose fold unit is > 0, i.e. held to the BIT-equal tier.
    pub bit_exact_leaders: usize,
    /// Leader slots on the tolerant tier.
    pub tiered_leaders: usize,
    pub bad: Vec<String>,
}

/// Run the scan form at one tuning and compare against the fixture under the
/// repo's tiered contract (`engine/conformance_scan.mojo`,
/// `tools/scan-layout.test.mjs`).
///
/// THE TOLERANT ROW IS TOLERANT BY CONSTRUCTION. A foldless X is an f64 prefix
/// in the serial fold and an f32 monoid lane here, so the GROUPING differs and
/// the rounding must. Every integer lane is exact in both forms, and the
/// `fold > 0` position lanes are BIT-equal because `resolve_x` is the serial
/// re-sum rescheduled. `bit_exact_leaders` is reported so a run cannot claim the
/// strict tier while holding nothing to it.
pub fn diff_scan(fx: &PipeFixture, chunk_size: usize, group_size: usize, shards: usize) -> ScanDiff {
    let got = run_scan_pipeline(&fx.bytes, &fx.trie, &fx.items, chunk_size, group_size, shards);
    let mut bad = Vec::new();
    let mut bit_exact_leaders = 0usize;
    let mut tiered_leaders = 0usize;

    if got.leaders != fx.exp_leaders as usize {
        bad.push(format!("leaders: got {} vs fixture {}", got.leaders, fx.exp_leaders));
    }
    if got.misses != fx.exp_misses {
        bad.push(format!(
            "miss list: got {} entries vs fixture {}",
            got.misses.len(),
            fx.exp_misses.len()
        ));
    }

    // Per-byte fold unit decides which tier a position lane gets.
    let mut fold_of_byte = vec![0i64; fx.byte_len];
    for item in &fx.items {
        let unit = item_fold_unit(item);
        let start = item.byte_start.max(0) as usize;
        let stop = ((item.byte_start + item.byte_count) as usize).min(fx.byte_len);
        fold_of_byte[start..stop].fill(unit);
    }

    for (slot, &slot_fold_unit) in fold_of_byte.iter().enumerate() {
        if got.slots.ord_to_byte[slot] != fx.exp_ord_to_byte[slot] {
            bad.push(format!(
                "ordToByte[{slot}]: got {} vs fixture {}",
                got.slots.ord_to_byte[slot], fx.exp_ord_to_byte[slot]
            ));
        }
        let is_leader =
            fx.exp_counts[slot * FIXTURE_COUNT_STRIDE + FIX_C_FLAGS] & F_LEADER != 0;
        if is_leader {
            if slot_fold_unit > 0 {
                bit_exact_leaders += 1;
            } else {
                tiered_leaders += 1;
            }
        }
        for lane in 0..FIXTURE_MEASURE_STRIDE {
            let expected = fx.exp_measures[slot * FIXTURE_MEASURE_STRIDE + lane];
            if lane == FIX_M_GLYPH_ID {
                // EXACT, and compared as the u32 it is — same reason as stage 2.
                if got.slots.gi[slot] != expected as u32 {
                    bad.push(format!(
                        "slot {slot} GLYPH_ID: got {} vs fixture {}",
                        got.slots.gi[slot], expected as u32
                    ));
                }
                continue;
            }
            let got_lane = match lane {
                FIX_M_X => got.slots.x(slot),
                FIX_M_Y => got.slots.y(slot),
                FIX_M_Z => got.slots.z(slot),
                FIX_M_ADVANCE => got.slots.advance(slot),
                FIX_M_HEIGHT => got.slots.height(slot),
                FIX_M_BASE_X => got.slots.base_x(slot),
                _ => got.slots.wm[slot],
            };
            let expected_lane = expected as f32;
            let bits_equal = got_lane.to_bits() == expected_lane.to_bits();
            let lane_ok = if !is_leader {
                bits_equal // a non-leader's lanes never differ between the forms
            } else if matches!(lane, FIX_M_X | FIX_M_Y | FIX_M_Z | FIX_M_BASE_X) {
                if slot_fold_unit > 0 {
                    bits_equal
                } else {
                    rel_close(expected_lane as f64, got_lane as f64)
                }
            } else if lane == FIX_M_LINE_ADV {
                rel_close(expected_lane as f64, got_lane as f64)
            } else {
                bits_equal // the EXACT lanes
            };
            if !lane_ok {
                bad.push(format!(
                    "slot {slot} lane {lane} ({}, fold={slot_fold_unit}): got {got_lane} vs fixture {expected_lane}",
                    measure_lane_name(lane)
                ));
            }
        }
        for lane in 0..FIXTURE_COUNT_STRIDE {
            let expected = fx.exp_counts[slot * FIXTURE_COUNT_STRIDE + lane];
            let got_lane = match lane {
                FIX_C_ROW => got.slots.lc[slot * 2],
                FIX_C_COL => got.slots.lc[slot * 2 + 1],
                FIX_C_FLAGS => got.slots.fl[slot],
                _ => got.slots.wc[slot],
            };
            if got_lane != expected {
                bad.push(format!(
                    "slot {slot} lane {lane} ({}): got {got_lane} vs fixture {expected}",
                    count_lane_name(lane)
                ));
            }
        }
    }

    // Bounds: TOTAL_ROWS (lane 6) exact, every other lane relative.
    for (i, &want_bits) in fx.exp_item_bounds.iter().enumerate() {
        let expected = f64::from_bits(want_bits);
        let got_lane = got.item_bounds[i];
        let ok = if i % 8 == 6 {
            got_lane.to_bits() == want_bits
        } else {
            rel_close(expected, got_lane)
        };
        if !ok {
            bad.push(format!(
                "itemBounds[{}][{}]: got {got_lane} vs fixture {expected}",
                i / 8,
                i % 8
            ));
        }
    }
    for (lane, &want_bits) in fx.exp_batch.iter().enumerate() {
        let expected = f64::from_bits(want_bits);
        let got_lane = got.batch_bounds[lane];
        let ok = if lane == 6 {
            got_lane.to_bits() == want_bits
        } else {
            rel_close(expected, got_lane)
        };
        if !ok {
            bad.push(format!("batchBounds[{lane}]: got {got_lane} vs fixture {expected}"));
        }
    }

    ScanDiff {
        chunk_size,
        group_size,
        shards,
        bit_exact_leaders,
        tiered_leaders,
        bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/fixtures")
    }

    fn all_fixtures() -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
            .expect("fixtures dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().ends_with(".pipe.bin"))
            .collect();
        v.sort();
        assert!(!v.is_empty(), "no .pipe.bin fixtures found — the suite below would pass vacuously");
        v
    }

    /// `unwrap_err` would demand Debug on the whole fixture; the failure text
    /// is what these tests are about.
    fn err_of(r: Result<PipeFixture, String>) -> String {
        match r {
            Ok(fx) => panic!("expected a parse failure, got {} bytes", fx.byte_len),
            Err(e) => e,
        }
    }

    fn load(name: &str) -> PipeFixture {
        load_pipe_fixture(&fixtures_dir().join(name)).expect("fixture should load")
    }

    #[test]
    fn every_fixture_parses_and_consumes_its_whole_file() {
        // Full consumption is asserted inside the loader, so reaching Ok() here
        // IS the structural check — a wrong stride cannot get this far.
        let paths = all_fixtures();
        assert_eq!(paths.len(), 14, "corpus size changed — update the expectation deliberately");
        for p in &paths {
            let fx = load_pipe_fixture(p).unwrap_or_else(|e| panic!("{}", e));
            assert_eq!(fx.exp_measures.len(), fx.byte_len * FIXTURE_MEASURE_STRIDE);
            assert_eq!(fx.exp_counts.len(), fx.byte_len * FIXTURE_COUNT_STRIDE);
            assert_eq!(fx.exp_item_bounds.len(), fx.item_count * 8);
            assert_eq!(fx.leader_bytes().len(), fx.exp_leaders as usize);
        }
    }

    #[test]
    fn a_truncated_fixture_is_refused() {
        let raw = std::fs::read(fixtures_dir().join("ascii-basic.pipe.bin")).unwrap();
        let err = err_of(load_pipe_bytes(&raw[..raw.len() - 8], "cut".into()));
        assert!(err.contains("truncated"), "wrong failure: {err}");
    }

    #[test]
    fn trailing_garbage_is_refused() {
        let mut raw = std::fs::read(fixtures_dir().join("ascii-basic.pipe.bin")).unwrap();
        raw.push(0);
        let err = err_of(load_pipe_bytes(&raw, "extra".into()));
        assert!(err.contains("left over"), "wrong failure: {err}");
    }

    #[test]
    fn bad_magic_is_refused() {
        let mut raw = std::fs::read(fixtures_dir().join("ascii-basic.pipe.bin")).unwrap();
        raw[0] ^= 0xFF;
        assert!(err_of(load_pipe_bytes(&raw, "x".into())).contains("bad magic"));
    }

    /// The differ's own mutation test. A differ that never reports is the
    /// classic cannot-fail check, so plant a difference and demand it be found
    /// — at the right byte, in the right lane, by name.
    #[test]
    fn the_differ_finds_a_planted_measure_difference() {
        let fx = load("ascii-basic.pipe.bin");
        let positions = fx.leader_bytes();
        let lanes = [FIX_M_ADVANCE];
        let mut got: Vec<f32> = positions
            .iter()
            .map(|&b| fx.exp_measures[b * FIXTURE_MEASURE_STRIDE + FIX_M_ADVANCE] as f32)
            .collect();
        assert!(diff_measures_at(&fx, &positions, &lanes, &got).is_empty());

        let victim = 3.min(got.len() - 1);
        assert!(got[victim].is_finite(), "planted victim must be a real value");
        got[victim] = f32::from_bits(got[victim].to_bits() ^ 1); // one ulp

        let bad = diff_measures_at(&fx, &positions, &lanes, &got);
        assert_eq!(bad.len(), 1, "exactly one lane was perturbed");
        assert_eq!(bad[0].byte, positions[victim]);
        assert_eq!(bad[0].lane_name, "ADVANCE");
        assert_ne!(bad[0].got_bits, bad[0].exp_bits);
    }

    #[test]
    fn the_differ_finds_a_planted_count_difference() {
        let fx = load("ascii-basic.pipe.bin");
        let positions = fx.leader_bytes();
        let lanes = [FIX_C_ROW, FIX_C_COL];
        let mut got: Vec<u32> = positions
            .iter()
            .flat_map(|&b| {
                [
                    fx.exp_counts[b * FIXTURE_COUNT_STRIDE + FIX_C_ROW],
                    fx.exp_counts[b * FIXTURE_COUNT_STRIDE + FIX_C_COL],
                ]
            })
            .collect();
        assert!(diff_counts_at(&fx, &positions, &lanes, &got).is_empty());
        got[5] = got[5].wrapping_add(1);
        let bad = diff_counts_at(&fx, &positions, &lanes, &got);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].lane_name, "COL");
        assert_eq!(bad[0].byte, positions[2]);
    }

    /// THE STAGE 0 ACCEPTANCE TEST, and the first time the existing Rust fold
    /// has been held to the fixture corpus rather than only to the Mojo engine
    /// through the FFI.
    #[test]
    fn reference_layout_is_bit_exact_on_every_in_domain_fixture() {
        let mut in_domain = 0;
        let mut lanes = 0usize;
        let mut records = 0usize;
        for p in all_fixtures() {
            let fx = load_pipe_fixture(&p).unwrap();
            let outcome = diff_against_reference_layout(&fx);
            if outcome.skipped.is_some() {
                continue;
            }
            in_domain += 1;
            lanes += outcome.compared_lanes;
            records += outcome.records;
            assert!(
                outcome.bad.is_empty(),
                "{}",
                report(&fx, &outcome.bad, 10)
            );
        }
        // ANTI-VACUITY. Every fixture drifting out of domain would leave this
        // test green with nothing compared, which is the exact failure mode the
        // corpus census exists to catch. Pin the counts.
        assert_eq!(in_domain, 4, "in-domain fixture count changed");
        // Predicted from the corpus census BEFORE running this, not read back
        // off it: exp_leaders of the four in-domain fixtures is
        // 58 + 5212 + 8 + 54, and each record compares 7 measure + 2 count lanes.
        assert_eq!(records, 58 + 5212 + 8 + 54, "records compared changed");
        assert_eq!(lanes, (58 + 5212 + 8 + 54) * 9, "lanes compared changed");
    }

    /// A trie miss must actually occur in the compared set, or the missing-glyph
    /// path is unexercised while the suite above reads green.
    #[test]
    fn the_in_domain_set_exercises_a_trie_miss() {
        let fx = load("utf8-emoji.pipe.bin");
        assert!(out_of_domain(&fx).is_none());
        let missing = fx
            .leader_bytes()
            .iter()
            .filter(|&&b| fx.exp_counts[b * FIXTURE_COUNT_STRIDE + FIX_C_FLAGS] & F_MISSING != 0)
            .count();
        assert!(missing > 0, "no miss in utf8-emoji — the miss path is uncompared");
    }
}
