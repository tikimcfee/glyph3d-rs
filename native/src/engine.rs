//! engine.rs — safe Rust wrapper over the Mojo glyph engine's C ABI (Stage D).
//!
//! The FFI surface (engine/ffi.mojo) is scalars + one opaque handle:
//! no Mojo types cross the boundary, records are copied into caller memory,
//! and the f32 lanes cross as raw bits inside the 32 B wire record.
//!
//! Wire record (32 B per rendered glyph, schema/glyph-identity.json):
//!   f32 X Y Z ADVANCE HEIGHT | u32 GLYPH_ID ROW COL
//!
//! Link mechanics live in build.rs; the engine .dylib is built by pixi/mojo,
//! not by cargo.

use std::ffi::c_void;
use std::path::Path;

/// One render-read record, exactly the engine's wire format.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphRecord {
    /// X, Y, Z, ADVANCE, HEIGHT (render-read measures).
    pub measures: [f32; 5],
    /// GLYPH_ID, ROW, COL.
    pub counts: [u32; 3],
}

impl GlyphRecord {
    pub fn x(self) -> f32 {
        self.measures[0]
    }
    pub fn y(self) -> f32 {
        self.measures[1]
    }
    pub fn z(self) -> f32 {
        self.measures[2]
    }
    pub fn advance(self) -> f32 {
        self.measures[3]
    }
    pub fn height(self) -> f32 {
        self.measures[4]
    }
    pub fn glyph_id(self) -> u32 {
        self.counts[0]
    }
    pub fn row(self) -> u32 {
        self.counts[1]
    }
    pub fn col(self) -> u32 {
        self.counts[2]
    }
}

const _: () = assert!(std::mem::size_of::<GlyphRecord>() == 32);

/// Layout params for one item (one text file). Mirrors the engine's `Item`.
/// f64 fields keep the oracle's float discipline; page geometry is integer.
#[derive(Clone, Copy, Debug)]
pub struct ItemParams {
    pub origin_x: f64,
    pub origin_y: f64,
    pub origin_z: f64,
    pub line_height: f64,
    pub z_step: f64,
    /// Fold unit in COLUMNS; 0 = no wrap.
    pub wrap_width: i32,
    pub has_page: bool,
    pub page_rows: i32,
    pub page_cols: i32,
    pub scroll_rows: i32,
    pub pages_wide: i32,
    pub page_gap_x: f64,
    pub band_stride_y: f64,
    pub depth_per_band: f64,
    pub depth_per_col: f64,
    pub page_line_height: f64,
}

impl Default for ItemParams {
    /// Plain text field: origin at 0, unit line height, no wrap, no pages.
    fn default() -> Self {
        Self {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            line_height: 1.0,
            z_step: 0.0,
            wrap_width: 0,
            has_page: false,
            page_rows: 0,
            page_cols: 0,
            scroll_rows: 0,
            pages_wide: 0,
            page_gap_x: 0.0,
            band_stride_y: 0.0,
            depth_per_band: 0.0,
            depth_per_col: 0.0,
            page_line_height: 0.0,
        }
    }
}

/// Rust-side rejection, raised BEFORE the FFI call — deliberately outside the
/// engine's status range so it can never be confused with one.
const GE_BAD_PARAMS: i32 = -1;

impl ItemParams {
    /// Refuse a layout the engine would silently turn into NaN.
    ///
    /// WHY THIS EXISTS. The Mojo engine performs NO input validation:
    /// `Item.line_height` is a raw `Float64` and `glyph_pipeline.mojo` says in
    /// as many words that "an unset line_height is NaN here and propagates".
    /// Every gate in this tree then compares layouts BY BITS — and two NaNs
    /// compare bit-equal. So a NaN pitch produces a NaN layout that
    /// `--engine-check`, all fifteen conformance suites, and the byte-equal
    /// render A/B would every one of them pass. The JS oracle has carried an
    /// `assertLineHeight` for exactly this since before the port; the native
    /// side never grew one. (Found 2026-09-02 auditing the JS oracle's tests.)
    ///
    /// NaN specifically is not "some invalid float": it is the `.pipe.bin`
    /// wire encoding for UNSET. Reaching this function means an unset pitch
    /// travelled all the way to the layout call without anyone resolving it,
    /// which is a different bug from a corrupt one and says so.
    ///
    /// ZERO IS LEGAL, and that is the subtle half. A zero pitch collapses
    /// every row onto one baseline — a degenerate layout, but a CHOICE, and
    /// not the same thing as an omission. The idiomatic Rust reflex
    /// (`if lh == 0.0 { default }`, or `Option::unwrap_or`) quietly conflates
    /// the two; this does not.
    pub fn validate(&self, item: usize) -> Result<(), EngineError> {
        let bad = |what: &str, why: &str| {
            Err(EngineError {
                status: GE_BAD_PARAMS,
                what: format!("item {item}: {what} — {why}"),
            })
        };
        if self.line_height.is_nan() {
            return bad(
                "line_height is NaN",
                "NaN is the wire encoding for UNSET, so an unresolved pitch reached                  the layout call. Every gate here compares by bits and two NaNs are                  bit-equal, so nothing downstream would notice",
            );
        }
        for (name, v) in [
            ("line_height", self.line_height),
            ("origin_x", self.origin_x),
            ("origin_y", self.origin_y),
            ("origin_z", self.origin_z),
            ("z_step", self.z_step),
            ("page_gap_x", self.page_gap_x),
            ("band_stride_y", self.band_stride_y),
            ("depth_per_band", self.depth_per_band),
            ("depth_per_col", self.depth_per_col),
            ("page_line_height", self.page_line_height),
        ] {
            if !v.is_finite() {
                return bad(
                    &format!("{name} is {v}"),
                    "a non-finite measure propagates into every position it touches",
                );
            }
        }
        for (name, v) in [
            ("wrap_width", self.wrap_width),
            ("page_rows", self.page_rows),
            ("page_cols", self.page_cols),
            ("scroll_rows", self.scroll_rows),
            ("pages_wide", self.pages_wide),
        ] {
            if v < 0 {
                return bad(&format!("{name} is {v}"), "page geometry counts are non-negative");
            }
        }
        Ok(())
    }
}

// Status codes from ffi.mojo.
const GE_OK: i32 = 0;
const GE_NO_TRIE: i32 = 2;
const GE_EMPTY: i32 = 4;

/// Error from an engine call (the C ABI returns status ints, not exceptions).
#[derive(Debug)]
pub struct EngineError {
    pub status: i32,
    pub what: String,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed with status {}", self.what, self.status)
    }
}
impl std::error::Error for EngineError {}

extern "C" {
    fn glyph_engine_new() -> *mut c_void;
    fn glyph_engine_free(handle: *mut c_void);
    fn glyph_engine_load_trie_file(
        handle: *mut c_void,
        path_ptr: *const u8,
        path_len: usize,
    ) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn glyph_engine_load_item(
        handle: *mut c_void,
        bytes_ptr: *const u8,
        byte_len: usize,
        origin_x: f64,
        origin_y: f64,
        origin_z: f64,
        line_height: f64,
        z_step: f64,
        wrap_width: i32,
        has_page: i32,
        page_rows: i32,
        page_cols: i32,
        scroll_rows: i32,
        pages_wide: i32,
        page_gap_x: f64,
        band_stride_y: f64,
        depth_per_band: f64,
        depth_per_col: f64,
        page_line_height: f64,
    ) -> i32;
    fn glyph_engine_fp_probe(a: f32, b: f32, c: f32) -> u32;
    fn glyph_engine_slot_count(handle: *mut c_void) -> u64;
    fn glyph_engine_copy_slots(handle: *mut c_void, out_ptr: *mut u32, out_len: usize)
        -> u64;
    // Stage E2: batched load — one call for a whole corpus. Descriptors are
    // 128 B blocks (explicit byte layout documented in ffi.mojo): ten f64
    // params, six i32 params, then u64 byte_start/byte_count.
    fn glyph_engine_load_items(
        handle: *mut c_void,
        blob_ptr: *const u8,
        blob_len: usize,
        desc_ptr: *const u8,
        item_count: usize,
        counts_out: *mut u64, // per-item record counts (= per-item leaders)
    ) -> i32;
}

/// Byte size of one batch item descriptor (see ffi.mojo's layout comment).
pub const ITEM_DESC_SIZE: usize = 128;

/// Serialize one item's params + byte range into a 128 B descriptor block.
/// Explicit offsets — shared verbatim with the Mojo side, no repr(C) guessing.
/// The returned Vec<u64> backing keeps the block 8-byte aligned.
pub fn write_item_desc(block: &mut [u8], params: &ItemParams, byte_start: u64, byte_count: u64) {
    assert_eq!(block.len(), ITEM_DESC_SIZE);
    block.fill(0);
    let f64s = [
        params.origin_x,
        params.origin_y,
        params.origin_z,
        params.line_height,
        params.z_step,
        params.page_gap_x,
        params.band_stride_y,
        params.depth_per_band,
        params.depth_per_col,
        params.page_line_height,
    ];
    for (i, v) in f64s.iter().enumerate() {
        block[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    let i32s = [
        params.wrap_width,
        params.has_page as i32,
        params.page_rows,
        params.page_cols,
        params.scroll_rows,
        params.pages_wide,
    ];
    for (i, v) in i32s.iter().enumerate() {
        block[80 + i * 4..84 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    block[112..120].copy_from_slice(&byte_start.to_le_bytes());
    block[120..128].copy_from_slice(&byte_count.to_le_bytes());
}

/// Assert the linked dylib was built with `--fp-mode contract=off`.
///
/// The flag is load-bearing — FMA contraction fuses `a*b + c` across statements
/// and this pipeline is bit-exact against a CPU reference that does not fuse.
/// Until 2026-09-02 NOTHING in this tree could detect its absence: the Mojo
/// suites pass either way (engine/check.sh's own header says so), and
/// `--engine-check` is blind because it runs with origin (0,0,0), which makes
/// its only fusable multiply-add FMA-invariant. Raising the origin would not
/// help — the fold computes in f64 and narrows to f32, so an f64-ulp difference
/// survives that narrowing only by luck (~2^-29 per record).
///
/// So this asks the compiler directly instead of hoping a corpus notices.
/// The operands are passed IN, so the expression cannot be constant-folded:
/// the fusion decision is made in the dylib's emitted code, which is the thing
/// under test. Measured against dylibs built both ways, 2026-09-02.
const FP_PROBE_A_BITS: u32 = 0x3f80_0002; // 1.0 + 2 ulp
const FP_PROBE_UNFUSED: u32 = 0x3500_0000; // product rounded, then added
const FP_PROBE_FUSED: u32 = 0x3500_0001; // single rounding — contraction ON

fn assert_fp_contract_off() {
    let a = f32::from_bits(FP_PROBE_A_BITS);
    let got = unsafe { glyph_engine_fp_probe(a, a, -1.0) };
    if got == FP_PROBE_UNFUSED {
        return;
    }
    let how = if got == FP_PROBE_FUSED {
        "it FUSED the multiply-add, so it was built with FP contraction ON"
    } else {
        "it returned neither the fused nor the unfused value"
    };
    panic!(
        "libglyph_engine.dylib was built WITHOUT `--fp-mode contract=off`.\n\
         The fp probe returned {got:#010x}; expected {FP_PROBE_UNFUSED:#010x} \
         (fused would be {FP_PROBE_FUSED:#010x}) — {how}.\n\
         Every float this engine produces is therefore suspect against the CPU \
         reference. Rebuild:  pixi run build-engine"
    );
}

/// A live engine handle. NOT Send/Sync: the Mojo runtime shards work onto its
/// own thread pool, but the handle itself is plain mutable state — keep it on
/// one thread (Stage E concern if we ever want N handles).
pub struct Engine {
    handle: *mut c_void,
}

impl Engine {
    pub fn new() -> Self {
        let handle = unsafe { glyph_engine_new() };
        assert!(!handle.is_null(), "glyph_engine_new returned null");
        // Fail loud at the substrate seam: a dylib with the wrong FP semantics
        // produces plausible-looking wrong numbers, which is the worst failure
        // mode available. Cheap (one FFI call per handle) and deterministic.
        assert_fp_contract_off();
        Engine { handle }
    }

    /// Load the trie (font metric tables). Stage E1: the engine dispatches on
    /// magic — a G3TR blob (`assets/atlas/engine-trie.bin`, the REAL atlas
    /// mapping; the default) or a legacy G3DF .pipe.bin conformance fixture.
    /// The path string crosses the FFI; the engine reads the file itself.
    pub fn load_trie_file(&mut self, path: &Path) -> Result<(), EngineError> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let status = unsafe {
            glyph_engine_load_trie_file(self.handle, bytes.as_ptr(), bytes.len())
        };
        if status == GE_OK {
            Ok(())
        } else {
            Err(EngineError {
                status,
                what: format!("glyph_engine_load_trie_file ({})", path.display()),
            })
        }
    }

    /// Run the pipeline for one text file. Results stay in the handle until
    /// the next load; pull them with [`Engine::records`].
    pub fn load_item(&mut self, bytes: &[u8], params: &ItemParams) -> Result<u64, EngineError> {
        params.validate(0)?;
        let status = unsafe {
            glyph_engine_load_item(
                self.handle,
                bytes.as_ptr(),
                bytes.len(),
                params.origin_x,
                params.origin_y,
                params.origin_z,
                params.line_height,
                params.z_step,
                params.wrap_width,
                params.has_page as i32,
                params.page_rows,
                params.page_cols,
                params.scroll_rows,
                params.pages_wide,
                params.page_gap_x,
                params.band_stride_y,
                params.depth_per_band,
                params.depth_per_col,
                params.page_line_height,
            )
        };
        match status {
            GE_OK | GE_EMPTY => Ok(self.slot_count()),
            s => Err(EngineError {
                status: s,
                what: if s == GE_NO_TRIE {
                    "glyph_engine_load_item (no trie loaded)".to_string()
                } else {
                    "glyph_engine_load_item".to_string()
                },
            }),
        }
    }

    /// Stage E2: run the pipeline for MANY items in one call. `blob` is the
    /// concatenation of all item bytes; `items` are (byte_start, byte_count,
    /// params) triples — ranges contiguous and ascending (the pipeline's
    /// documented requirement). Each item carries FULL params (per-item
    /// pagination included). Results stay in the handle; pull them with
    /// [`Engine::records`]. Returns the per-item record counts (= per-item
    /// leader counts, computed from the pipeline's flag lanes).
    pub fn load_items(
        &mut self,
        blob: &[u8],
        items: &[(u64, u64, ItemParams)],
    ) -> Result<Vec<u64>, EngineError> {
        // Vec<u64> backing keeps the descriptor array 8-byte aligned.
        let mut desc_words = vec![0u64; items.len() * (ITEM_DESC_SIZE / 8)];
        let desc_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut desc_words);
        for (i, (start, count, params)) in items.iter().enumerate() {
            // Per item, and the error names WHICH — with a whole corpus in one
            // arena, an unnamed refusal is unactionable.
            params.validate(i)?;
            write_item_desc(
                &mut desc_bytes[i * ITEM_DESC_SIZE..(i + 1) * ITEM_DESC_SIZE],
                params,
                *start,
                *count,
            );
        }
        let mut counts = vec![0u64; items.len()];
        let status = unsafe {
            glyph_engine_load_items(
                self.handle,
                blob.as_ptr(),
                blob.len(),
                desc_bytes.as_ptr(),
                items.len(),
                counts.as_mut_ptr(),
            )
        };
        match status {
            GE_OK | GE_EMPTY => Ok(counts),
            s => Err(EngineError {
                status: s,
                what: if s == GE_NO_TRIE {
                    "glyph_engine_load_items (no trie loaded)".to_string()
                } else {
                    "glyph_engine_load_items".to_string()
                },
            }),
        }
    }

    /// Records produced by the last [`Engine::load_item`].
    pub fn slot_count(&self) -> u64 {
        unsafe { glyph_engine_slot_count(self.handle) }
    }

    /// Copy the last load's records out of the engine.
    pub fn records(&self) -> Vec<GlyphRecord> {
        let n = self.slot_count() as usize;
        let mut out = vec![GlyphRecord::default(); n];
        if n > 0 {
            let written =
                unsafe { glyph_engine_copy_slots(self.handle, out.as_mut_ptr() as *mut u32, n) };
            debug_assert_eq!(written as usize, n);
            out.truncate(written as usize);
        }
        out
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { glyph_engine_free(self.handle) }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ok_params() -> ItemParams {
        ItemParams { line_height: 1.2, ..Default::default() }
    }

    #[test]
    fn a_stated_pitch_still_runs() {
        // THE COUNTERFACTUAL FOR THE GUARD. Without this, `return Err(..)` at
        // the top of validate() would pass every negative test below.
        assert!(ok_params().validate(0).is_ok());
    }

    #[test]
    fn zero_is_legal_a_degenerate_pitch_is_a_choice() {
        // Every row on one baseline is a layout, not an omission. The reflex
        // `if lh == 0.0 { default }` conflates them; we must not.
        let p = ItemParams { line_height: 0.0, ..Default::default() };
        assert!(p.validate(0).is_ok(), "zero pitch must be accepted, not defaulted");
    }

    #[test]
    fn nan_pitch_is_refused_and_named_as_the_unset_encoding() {
        let p = ItemParams { line_height: f64::NAN, ..Default::default() };
        let e = p.validate(3).expect_err("NaN pitch must be refused");
        assert_eq!(e.status, GE_BAD_PARAMS);
        assert!(e.what.contains("item 3"), "must name the item: {}", e.what);
        assert!(e.what.contains("line_height"), "must name the field: {}", e.what);
        assert!(e.what.contains("UNSET"), "must say NaN means unset: {}", e.what);
    }

    #[test]
    fn infinite_pitch_is_refused() {
        for lh in [f64::INFINITY, f64::NEG_INFINITY] {
            let p = ItemParams { line_height: lh, ..Default::default() };
            assert!(p.validate(0).is_err(), "{lh} must be refused");
        }
    }

    #[test]
    fn every_measure_is_checked_not_just_the_pitch() {
        // A non-finite anywhere propagates into positions. Walk them all so a
        // newly added measure that skips validate() shows up as a gap here.
        let mut n = 0;
        for mutate in [
            (|p: &mut ItemParams| p.origin_x = f64::NAN) as fn(&mut ItemParams),
            |p: &mut ItemParams| p.origin_y = f64::INFINITY,
            |p: &mut ItemParams| p.origin_z = f64::NAN,
            |p: &mut ItemParams| p.z_step = f64::NAN,
            |p: &mut ItemParams| p.page_gap_x = f64::NAN,
            |p: &mut ItemParams| p.band_stride_y = f64::NAN,
            |p: &mut ItemParams| p.depth_per_band = f64::NAN,
            |p: &mut ItemParams| p.depth_per_col = f64::NAN,
            |p: &mut ItemParams| p.page_line_height = f64::NAN,
        ] {
            let mut p = ok_params();
            mutate(&mut p);
            assert!(p.validate(0).is_err(), "a non-finite measure slipped through");
            n += 1;
        }
        assert_eq!(n, 9, "the sweep must cover every f64 measure but the pitch");
    }

    #[test]
    fn negative_page_geometry_is_refused() {
        let mut n = 0;
        for mutate in [
            (|p: &mut ItemParams| p.wrap_width = -1) as fn(&mut ItemParams),
            |p: &mut ItemParams| p.page_rows = -1,
            |p: &mut ItemParams| p.page_cols = -1,
            |p: &mut ItemParams| p.scroll_rows = -1,
            |p: &mut ItemParams| p.pages_wide = -1,
        ] {
            let mut p = ok_params();
            mutate(&mut p);
            assert!(p.validate(0).is_err(), "a negative count slipped through");
            n += 1;
        }
        assert_eq!(n, 5, "the sweep must cover every integer count");
    }

    /// The unit tests above prove `validate()` decides correctly; this proves it
    /// is actually WIRED to the entry point. Without it the `?` in `load_item`
    /// would be trusted by inspection, which is the habit this repo keeps
    /// catching itself in. Uses a live engine + the real atlas trie.
    #[test]
    fn a_nan_pitch_cannot_reach_the_engine_through_load_item() {
        let trie = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin");
        let mut eng = Engine::new();
        eng.load_trie_file(&trie).expect("trie");

        let good = ItemParams { line_height: 1.2, ..Default::default() };
        assert!(eng.load_item(b"ab\ncd\n", &good).is_ok(), "the guard must not reject valid work");

        let bad = ItemParams { line_height: f64::NAN, ..Default::default() };
        let e = eng.load_item(b"ab\ncd\n", &bad).expect_err("NaN pitch must be refused AT the seam");
        assert_eq!(e.status, GE_BAD_PARAMS, "must be the Rust-side refusal, not an engine status");
        assert!(e.what.contains("UNSET"), "{}", e.what);
    }

    #[test]
    fn the_default_params_are_themselves_valid() {
        // repo.rs and main.rs both build on ..Default::default(); if the default
        // were invalid every caller would fail at the seam.
        assert!(ItemParams::default().validate(0).is_ok());
    }
}
