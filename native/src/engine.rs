//! engine.rs — safe Rust wrapper over the Mojo glyph engine's C ABI (Stage D).
//!
//! This is the SUBSTRATE half of the Mojo backend: the extern block, the 128 B
//! item descriptor, the FP-contract probe, and the handle's lifetime. The
//! layout CONTRACT it serves — `ItemParams`, `GlyphRecord`, `LayoutError` —
//! lives in `layout.rs`, because all three backends share it and none of them
//! is the FFI. The adapter that presents this handle as a `LayoutGlyphs` is
//! `layout_mojo.rs`.
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

use crate::layout::{GlyphRecord, ItemParams, LayoutError};

/// Who a `LayoutError` raised here came from.
pub const MOJO_BACKEND: &str = "mojo";

// Status codes from ffi.mojo.
const GE_OK: i32 = 0;
const GE_NO_TRIE: i32 = 2;
const GE_EMPTY: i32 = 4;
/// The descriptor's shape word is not the one this dylib expects — the .dylib on
/// disk was built from different source than this binary was compiled against.
const GE_ABI_MISMATCH: i32 = 9;

extern "C" {
    fn glyph_engine_new() -> *mut c_void;
    fn glyph_engine_free(handle: *mut c_void);
    fn glyph_engine_load_trie_file(
        handle: *mut c_void,
        path_ptr: *const u8,
        path_len: usize,
    ) -> i32;
    // ONE MARSHALLING FORMAT. This used to be a twenty-argument positional call,
    // and adding `wrap_mode` to it shifted every argument after it for any caller
    // linked against a dylib built from older source — silently, into a layout
    // that looked plausible. See ffi.mojo's ONE MARSHALLING FORMAT note for the
    // measurement and for the two guards that were tried and did not work.
    // Renamed on purpose: a stale dylib now fails to LINK rather than being
    // miscalled.
    fn glyph_engine_load_item_desc(
        handle: *mut c_void,
        bytes_ptr: *const u8,
        byte_len: usize,
        desc_ptr: *const u8,
    ) -> i32;
    fn glyph_engine_fp_probe(a: f32, b: f32, c: f32) -> u32;
    fn glyph_engine_slot_count(handle: *mut c_void) -> u64;
    fn glyph_engine_copy_slots(handle: *mut c_void, out_ptr: *mut u32, out_len: usize)
        -> u64;
    // Stage E2: batched load — one call for a whole corpus. Descriptors are
    // 128 B blocks (explicit byte layout documented in ffi.mojo): ten f64
    // params, SEVEN i32 params, then u64 byte_start/byte_count.
    fn glyph_engine_load_items(
        handle: *mut c_void,
        blob_ptr: *const u8,
        blob_len: usize,
        desc_ptr: *const u8,
        item_count: usize,
        counts_out: *mut u64, // per-item record counts (= per-item leaders)
    ) -> i32;
}

/// Byte size of one item descriptor (see ffi.mojo's layout comment). BOTH load
/// entries take one of these; there is no positional form any more.
pub const ITEM_DESC_SIZE: usize = 128;

/// How many i32 params ride inside the descriptor. Asserted against the actual
/// array in `write_item_desc`, so the constant cannot drift from the code.
pub const DESC_I32_COUNT: usize = 7;

/// Byte offset of the descriptor's shape word. FIXED, and that is the point: a
/// sentinel in a positional argument list can be shifted out of alignment (and
/// was, measurably — ffi.mojo has the numbers); one at a fixed offset in a
/// fixed-size block cannot.
const ABI_SHAPE_OFFSET: usize = 108;

/// The shape word itself, computed from THIS side's declarations. A dylib built
/// from different source computes a different one and refuses the call.
const fn abi_shape() -> u32 {
    ((ITEM_DESC_SIZE as u32) << 8) | DESC_I32_COUNT as u32
}

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
    // SEVEN i32s since the wrap mode joined them: 80..108, leaving 108..112 pad.
    // The block stayed 128 B — 10 f64 + 7 i32 + 2 u64 is 124 — which is why
    // ITEM_DESC_SIZE did not move. Asserted below rather than trusted.
    let i32s = [
        params.wrap_width,
        params.has_page as i32,
        params.page_rows,
        params.page_cols,
        params.scroll_rows,
        params.pages_wide,
        params.wrap_mode.code() as i32,
    ];
    // Derived from the array, not from a literal: adding an i32 without moving
    // ITEM_DESC_SIZE would silently overwrite byte_start at 112.
    assert!(
        80 + i32s.len() * 4 <= 112,
        "{} i32 params overrun the descriptor's byte_start at 112",
        i32s.len()
    );
    for (i, v) in i32s.iter().enumerate() {
        block[80 + i * 4..84 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    // And the count the shape word reports must be THIS array's length, not a
    // second opinion about it.
    assert_eq!(
        i32s.len(),
        DESC_I32_COUNT,
        "DESC_I32_COUNT is out of step with write_item_desc"
    );
    block[ABI_SHAPE_OFFSET..ABI_SHAPE_OFFSET + 4]
        .copy_from_slice(&abi_shape().to_le_bytes());
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

/// Name the stale side when the dylib and this binary disagree about the FFI.
///
/// `GE_ABI_MISMATCH` is the one status that is never the caller's data's fault:
/// the .dylib on disk was built from different source than this binary was
/// compiled against. That is a NORMAL state of this tree — `cargo build` does not
/// build the dylib (`native/build.rs` links whatever `pixi run build-engine` last
/// produced) — so it gets the remedy printed with it.
fn abi_mismatch_message(what: &str) -> String {
    format!(
        "{what}: libglyph_engine.dylib expects a different descriptor shape than \
         this binary writes (shape word {:#06x} at offset {ABI_SHAPE_OFFSET}).\n\
         The dylib is not built by cargo and is stale. Rebuild:  pixi run build-engine",
        abi_shape()
    )
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
    pub fn load_trie_file(&mut self, path: &Path) -> Result<(), LayoutError> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let status = unsafe {
            glyph_engine_load_trie_file(self.handle, bytes.as_ptr(), bytes.len())
        };
        if status == GE_OK {
            Ok(())
        } else {
            Err(LayoutError {
                backend: MOJO_BACKEND,
                status,
                what: format!("glyph_engine_load_trie_file ({})", path.display()),
            })
        }
    }

    /// Run the pipeline for one text file. Results stay in the handle until
    /// the next load; pull them with [`Engine::records`].
    ///
    /// Marshals the SAME 128 B descriptor the batched entry takes. It used to
    /// pass twenty positional arguments, and `wrap_mode` landing in the middle of
    /// them is what broke `--repo-verify` on 2026-09-04 against a stale dylib.
    /// One format means a new field takes descriptor pad instead of shifting a
    /// register, and means both strategies exercise `write_item_desc`.
    pub fn load_item(&mut self, bytes: &[u8], params: &ItemParams) -> Result<u64, LayoutError> {
        params.validate(0)?;
        // Vec<u64> backing keeps the block 8-byte aligned, as in `load_items`.
        let mut desc_words = vec![0u64; ITEM_DESC_SIZE / 8];
        let desc_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut desc_words);
        write_item_desc(desc_bytes, params, 0, bytes.len() as u64);
        let status = unsafe {
            glyph_engine_load_item_desc(
                self.handle,
                bytes.as_ptr(),
                bytes.len(),
                desc_bytes.as_ptr(),
            )
        };
        match status {
            GE_OK | GE_EMPTY => Ok(self.slot_count()),
            s => Err(LayoutError {
                backend: MOJO_BACKEND,
                status: s,
                what: if s == GE_NO_TRIE {
                    "glyph_engine_load_item_desc (no trie loaded)".to_string()
                } else if s == GE_ABI_MISMATCH {
                    abi_mismatch_message("glyph_engine_load_item_desc")
                } else {
                    "glyph_engine_load_item_desc".to_string()
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
    ) -> Result<Vec<u64>, LayoutError> {
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
            s => Err(LayoutError {
                backend: MOJO_BACKEND,
                status: s,
                what: if s == GE_NO_TRIE {
                    "glyph_engine_load_items (no trie loaded)".to_string()
                } else if s == GE_ABI_MISMATCH {
                    abi_mismatch_message("glyph_engine_load_items")
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
    use crate::layout::LAYOUT_BAD_PARAMS;

    /// `ItemParams::validate` is decided and unit-tested on the seam
    /// (`layout.rs`); this proves it is WIRED to the FFI entry point.
    ///
    /// The seam validates too — `LayoutGlyphs::layout_items` is a provided
    /// method that no backend can forget — so in the render path this guard
    /// never fires. It stays because `Engine` has callers that are NOT the
    /// seam: `repo::rederive_records` (the pick re-run) and
    /// `main::run_engine_smoke` both hold a handle directly. Removing it would
    /// leave those two paths reaching the FFI unguarded, and the FFI is where a
    /// NaN actually propagates.
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
        assert_eq!(e.status, LAYOUT_BAD_PARAMS, "must be the host refusal, not an engine status");
        assert!(e.what.contains("UNSET"), "{}", e.what);
    }
}
