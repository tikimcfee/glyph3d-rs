//! engine.rs — safe Rust wrapper over the Mojo glyph engine's C ABI (Stage D).
//!
//! This is the SUBSTRATE half of the Mojo backend: the extern block, the 136 B
//! item descriptor, the FP-contract probe, and the handle's lifetime. The
//! layout CONTRACT it serves — `ItemParams`, `GlyphRecord`, `LayoutError` —
//! lives in `layout.rs`, because all three backends share it and none of them
//! is the FFI. The adapter that presents this handle as a `LayoutGlyphs` is
//! `layout_mojo.rs`.
//!
//! The FFI surface (engine/ffi.mojo) is scalars + one opaque handle: no Mojo
//! types cross the boundary, and f32 lanes cross as raw bits. There are two
//! output shapes — the record entries copy a 32 B wire record per glyph into
//! caller memory, while `load_items_direct` writes 48 B render instances into
//! an arena the caller owns and materializes no record.
//!
//! Wire record (32 B per rendered glyph, schema/glyph-identity.json):
//!   f32 X Y Z ADVANCE HEIGHT | u32 GLYPH_ID ROW COL
//!
//! Link mechanics live in build.rs; the engine .dylib is built by pixi/mojo,
//! not by cargo.

use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

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
/// The direct path could not fit its output in the arena the caller sized.
const GE_ARENA_TOO_SMALL: i32 = 10;
/// A per-item colour array is shorter than that item's record count.
const GE_PAINT_TOO_SHORT: i32 = 11;
/// Items do not tile the blob — a range past its end, or overlapping ranges.
const GE_BAD_ITEM_RANGE: i32 = 12;

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
    // 136 B blocks (explicit byte layout documented in ffi.mojo): ten f64
    // params, EIGHT i32 params, then u64 byte_start/byte_count.
    fn glyph_engine_load_items(
        handle: *mut c_void,
        blob_ptr: *const u8,
        blob_len: usize,
        desc_ptr: *const u8,
        item_count: usize,
        counts_out: *mut u64, // per-item record counts (= per-item leaders)
    ) -> i32;

    // Per-stage nanoseconds for the last load. Returns how many lanes it wrote,
    // so a dylib with fewer lanes reports fewer rather than being assumed.
    fn glyph_engine_stage_ns(handle: *mut c_void, out_ptr: *mut u64, cap: usize) -> usize;

    // The direct path: fold, then write render instances into the CALLER's
    // arena. No wire record is materialized anywhere.
    #[allow(clippy::too_many_arguments)]
    fn glyph_engine_load_items_direct(
        handle: *mut c_void,
        blob_ptr: *const u8,
        blob_len: usize,
        desc_ptr: *const u8,
        item_count: usize,
        inst_ptr: *mut u32,
        inst_cap: usize,   // in INSTANCES, not bytes
        paint_ptrs: *const *const u32,
        paint_lens: *const u64,
        flat_colors: *const u32,
        group_ids: *const u32,
        place_out: *mut u32,
    ) -> i32;

    // (instance u32 lanes << 32) | placement u32 lanes.
    fn glyph_engine_instance_shape() -> u64;
}

/// Marshal item descriptors into the 136 B blocks the engine reads.
///
/// ONE writer of that layout, shared by every entry that takes items. Written
/// twice it would be the correlated-fault shape this tree keeps finding: two
/// copies agree with each other and both drift from the engine, and the
/// descriptor is exactly where that already happened once (`cc814b3`, the
/// arity shift that broke the per-item entry while the batched one stayed fine).
///
/// Returns `Vec<u64>` rather than bytes because the u64 backing is what keeps
/// the array 8-byte aligned for the engine's bitcast reads.
fn build_descs(items: &[(u64, u64, ItemParams)]) -> Result<Vec<u64>, LayoutError> {
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
    Ok(desc_words)
}

/// `GlyphInstance` as u32 lanes — 48 B / 4. The engine writes this many per
/// instance; asserted against the dylib at load rather than assumed.
pub const INSTANCE_U32S: usize = 12;
/// Placement block lanes, u32, floats bitcast. 64 B per item.
pub const PLACEMENT_U32S: usize = 16;

/// Engine-side stage lanes, in the order `ffi.mojo` writes them: `run_pipeline`'s
/// seven, then the two the FFI entry owns. Names are the engine's, kept verbatim
/// so a reader can grep one string across both languages.
pub const ENGINE_STAGE_NAMES: [&str; 14] = [
    "alloc", "gapsweep", "decode", "misscat", "fold", "paginate", "bounds",
    "eg_compact", "eg_counts",
    // The direct write's five phases. They replace a single `eg_direct` total,
    // which was the largest lane on the line and said nothing about which part
    // of a count/prefix/scatter it was.
    "dw_build", "dw_count", "dw_prefix", "dw_write", "dw_merge",
];

/// Byte size of one item descriptor (see ffi.mojo's layout comment). BOTH load
/// entries take one of these; there is no positional form any more.
/// 2026-09-20, the sequence pass: 128 -> 136 — the descriptor was FULL, and
/// cluster_mode (the 8th i32) could not take pad. ABI_SHAPE moved with it.
pub const ITEM_DESC_SIZE: usize = 136;

/// How many i32 params ride inside the descriptor. Asserted against the actual
/// array in `write_item_desc`, so the constant cannot drift from the code.
pub const DESC_I32_COUNT: usize = 8;

/// Byte offset of the descriptor's shape word. FIXED, and that is the point: a
/// sentinel in a positional argument list can be shifted out of alignment (and
/// was, measurably — ffi.mojo has the numbers); one at a fixed offset in a
/// fixed-size block cannot.
const ABI_SHAPE_OFFSET: usize = 112;

/// The shape word itself, computed from THIS side's declarations. A dylib built
/// from different source computes a different one and refuses the call.
const fn abi_shape() -> u32 {
    ((ITEM_DESC_SIZE as u32) << 8) | DESC_I32_COUNT as u32
}

/// Serialize one item's params + byte range into a 136 B descriptor block.
/// Explicit offsets — shared verbatim with the Mojo side, no repr(C) guessing.
/// The returned `Vec<u64>` backing keeps the block 8-byte aligned.
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
    // EIGHT i32s since the cluster mode joined them: 80..112. The block grew
    // to 136 B for it — the descriptor was full (10 f64 + 7 i32 + 2 u64 = 124),
    // so the sequence pass could not take pad the way the wrap mode did.
    // Asserted below rather than trusted.
    let i32s = [
        params.wrap_width,
        params.has_page as i32,
        params.page_rows,
        params.page_cols,
        params.scroll_rows,
        params.pages_wide,
        params.wrap_mode.code() as i32,
        params.cluster_mode.code() as i32,
    ];
    // Derived from the array, not from a literal: adding an i32 without moving
    // ITEM_DESC_SIZE would silently overwrite byte_start at 120.
    assert!(
        80 + i32s.len() * 4 <= ABI_SHAPE_OFFSET,
        "{} i32 params overrun the descriptor's shape word",
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
    block[120..128].copy_from_slice(&byte_start.to_le_bytes());
    block[128..136].copy_from_slice(&byte_count.to_le_bytes());
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
    /// the next load; pull them with [`Engine::read_back`].
    ///
    /// Marshals the SAME 136 B descriptor the batched entry takes. It used to
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
    /// [`Engine::read_back`]. Returns the per-item record counts (= per-item
    /// leader counts, computed from the pipeline's flag lanes).
    pub fn load_items(
        &mut self,
        blob: &[u8],
        items: &[(u64, u64, ItemParams)],
    ) -> Result<Vec<u64>, LayoutError> {
        let mut desc_words = build_descs(items)?;
        let desc_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut desc_words);
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
                } else if s == GE_BAD_ITEM_RANGE {
                    "glyph_engine_load_items: the items do not tile the blob — \
                     each must be in range, ascending and non-overlapping"
                        .to_string()
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

    /// Fold `items` and have the ENGINE write render instances into `inst_ptr`.
    ///
    /// The caller owns the destination, sizes it, and learns from the return
    /// how many slots were actually filled — blanks are dropped by the engine,
    /// so the count offered and the count written are different numbers and
    /// conflating them would publish uninitialized memory as glyphs.
    ///
    /// # Safety
    /// `inst_ptr` must be writable for `inst_cap * INSTANCE_U32S` u32s, and
    /// `place_out` for `items * PLACEMENT_U32S`. Every `paint_ptrs[i]` is
    /// either null or readable for that item's RECORD count.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn load_items_direct(
        &mut self,
        blob: &[u8],
        items: &[(u64, u64, ItemParams)],
        inst_ptr: *mut u32,
        inst_cap: usize,
        paint_ptrs: &[*const u32],
        paint_lens: &[u64],
        flat_colors: &[u32],
        group_ids: &[u32],
        place_out: &mut [u32],
    ) -> Result<(), LayoutError> {
        let mut desc_words = build_descs(items)?;
        let desc_bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut desc_words);
        let status = unsafe {
            glyph_engine_load_items_direct(
                self.handle,
                blob.as_ptr(),
                blob.len(),
                desc_bytes.as_ptr(),
                items.len(),
                inst_ptr,
                inst_cap,
                paint_ptrs.as_ptr(),
                paint_lens.as_ptr(),
                flat_colors.as_ptr(),
                group_ids.as_ptr(),
                place_out.as_mut_ptr(),
            )
        };
        match status {
            GE_OK | GE_EMPTY => Ok(()),
            s => Err(LayoutError {
                backend: MOJO_BACKEND,
                status: s,
                what: match s {
                    GE_NO_TRIE => "glyph_engine_load_items_direct (no trie loaded)".to_string(),
                    GE_ABI_MISMATCH => abi_mismatch_message("glyph_engine_load_items_direct"),
                    GE_BAD_ITEM_RANGE => "glyph_engine_load_items_direct: the \
                         items do not tile the blob — each must be in range, \
                         ascending and non-overlapping. The fold and the arena \
                         bound both assume it"
                        .to_string(),
                    GE_PAINT_TOO_SHORT => "glyph_engine_load_items_direct: an \
                         item's paint array is shorter than its record count. \
                         Paint is indexed by RECORD, blanks included — a colour \
                         array built per surviving glyph is the likely cause"
                        .to_string(),
                    GE_ARENA_TOO_SMALL => format!(
                        "glyph_engine_load_items_direct: the fold produced more \
                         records than the {inst_cap}-slot arena the caller sized. \
                         The caller's bound is the byte count and leaders cannot \
                         exceed bytes, so this is a fold defect, not a sizing one",
                    ),
                    _ => "glyph_engine_load_items_direct".to_string(),
                },
            }),
        }
    }

    /// Assert the dylib agrees with this build about the instance layout.
    ///
    /// Called before the first direct load, not at construction: a caller that
    /// never uses the direct path should not be refused service by a dylib
    /// that predates it. A disagreement here is the Stage G strided-colour bug
    /// class — found last time by looking at pixels — so it is a hard error
    /// with both numbers in it, not a warning.
    pub fn check_instance_shape() -> Result<(), LayoutError> {
        let packed = unsafe { glyph_engine_instance_shape() };
        let (inst, place) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        if inst == INSTANCE_U32S && place == PLACEMENT_U32S {
            return Ok(());
        }
        Err(LayoutError {
            backend: MOJO_BACKEND,
            status: GE_ABI_MISMATCH,
            what: format!(
                "instance layout disagreement: this build says {INSTANCE_U32S} \
                 instance lanes and {PLACEMENT_U32S} placement lanes, the engine \
                 says {inst} and {place}. Rebuild the dylib (`cargo glyph build`)",
            ),
        })
    }

    /// Per-stage nanoseconds for the last load, engine-side.
    ///
    /// The lane COUNT comes from the engine, not from this side: a dylib built
    /// before a lane was added returns fewer, and the extra entries stay zero
    /// rather than reading whatever the engine did not write. That is the same
    /// discipline `cc814b3` established for the per-item descriptor — a stale
    /// dylib should degrade legibly instead of lying.
    pub fn stage_ns(&self) -> [u64; ENGINE_STAGE_NAMES.len()] {
        let mut out = [0u64; ENGINE_STAGE_NAMES.len()];
        unsafe { glyph_engine_stage_ns(self.handle, out.as_mut_ptr(), out.len()) };
        out
    }

    /// Copy the last load's records out of the engine — THE READBACK the plan
    /// (`engine/BACKEND-PLAN.md` item 3) exists to delete.
    ///
    /// It returns its own cost split in two, because the plan has always
    /// reasoned about this as one number and the two halves have different
    /// fixes. `alloc` is the host `Vec` — allocation plus the zero-fill of
    /// `n * 32 B` that `copy` then immediately overwrites; it dies if the
    /// engine hands out a pointer instead of filling a buffer. `copy` is the
    /// FFI memcpy itself; it dies only when compaction moves to where the data
    /// already is. Measuring them apart is what decides which fix is worth
    /// building, so the split is part of the API rather than a probe someone
    /// has to remember to add.
    ///
    /// CAVEAT, and it is the reason `alloc` is reported rather than trusted:
    /// a large allocation is lazily backed on macOS, so some of the zero-fill
    /// is paid as page faults DURING `copy`. `alloc` is therefore a floor on
    /// the buffer's cost, not the whole of it, and `copy` is an inflated
    /// measure of the memcpy alone. Their SUM is honest; each alone is not.
    pub fn read_back(&self) -> Readback {
        let n = self.slot_count() as usize;
        let t = Instant::now();
        let mut records = vec![GlyphRecord::default(); n];
        let alloc = t.elapsed();
        let t = Instant::now();
        if n > 0 {
            let written = unsafe {
                glyph_engine_copy_slots(self.handle, records.as_mut_ptr() as *mut u32, n)
            };
            debug_assert_eq!(written as usize, n);
            records.truncate(written as usize);
        }
        let copy = t.elapsed();
        Readback { records, alloc, copy }
    }
}

/// The readback and what it cost, from [`Engine::read_back`]. The two duration
/// fields are documented there, including why only their sum is trustworthy.
pub struct Readback {
    pub records: Vec<GlyphRecord>,
    pub alloc: Duration,
    pub copy: Duration,
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

    /// The item ranges are the caller's DATA, and until 2026-09-07 nothing
    /// checked them. A range past the blob end faulted inside the fold, which
    /// sizes every per-byte array to the blob and then walks each item's own
    /// range — a SAFE `pub fn` segfaulting on data it was handed. Overlapping
    /// ranges were worse than a fault: the leader count is taken over the BLOB
    /// while the direct writer runs over the ITEMS, so the arena bound computed
    /// from one does not bound the other, and the guard was unsound exactly
    /// where it was needed. Found by an adversarial audit, not by a caller.
    #[test]
    fn item_ranges_are_refused_rather_than_trusted() {
        let mut eng = Engine::new();
        let trie = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/atlas/engine-trie.bin");
        eng.load_trie_file(&trie).expect("trie");
        let blob = b"abcdef";
        let p = ItemParams { line_height: 1.25, ..Default::default() };

        // Past the end.
        assert!(
            eng.load_items(blob, &[(0, 99, p)]).is_err(),
            "a range past the blob end must be refused, not folded",
        );
        // Overlapping — the shape that makes the arena bound unsound.
        assert!(
            eng.load_items(blob, &[(0, 6, p), (0, 6, p)]).is_err(),
            "overlapping items must be refused",
        );
        // Descending.
        assert!(
            eng.load_items(blob, &[(3, 3, p), (0, 3, p)]).is_err(),
            "descending items must be refused",
        );
        // ANTI-VACUITY: the legal tiling must still be accepted, or the three
        // assertions above would pass against a function that refuses everything.
        assert!(
            eng.load_items(blob, &[(0, 3, p), (3, 3, p)]).is_ok(),
            "a legal tiling must still load",
        );
    }

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

// ── Gate drivers (moved from main.rs in the 2026-09 code-shape refactor) ──

pub fn run_engine_smoke(file: &Path, trie: Option<&Path>, loops: u32) {
    let default_trie = crate::default_engine_trie();
    let trie = trie.unwrap_or(&default_trie);
    let bytes = std::fs::read(file).expect("failed to read --engine-file");

    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("failed to load engine trie");

    let params = ItemParams::default();
    let mut last_count = 0u64;
    let t0 = std::time::Instant::now();
    for _ in 0..loops {
        last_count = eng.load_item(&bytes, &params).expect("engine load_item failed");
    }
    let dt = t0.elapsed();

    let records = eng.read_back().records;
    assert_eq!(records.len() as u64, last_count, "record copy count mismatch");

    let total_mb = bytes.len() as f64 * loops as f64 / 1e6;
    println!(
        "engine: {} ({} B) x {} loads -> {} records in {:.3} s ({:.1} MB/s pipeline)",
        file.display(),
        bytes.len(),
        loops,
        records.len(),
        dt.as_secs_f64(),
        total_mb / dt.as_secs_f64(),
    );
    for (i, r) in records.iter().take(8).enumerate() {
        println!(
            "  rec[{}]: X={} Y={} Z={} ADVANCE={} HEIGHT={} GLYPH_ID={} ROW={} COL={}",
            i,
            r.x(),
            r.y(),
            r.z(),
            r.advance(),
            r.height(),
            r.glyph_id(),
            r.row(),
            r.col(),
        );
    }
}

/// Stage E1 cross-validation: run the engine through the FFI on `file`, then
/// independently compute the expected records with text.rs's CPU reference
/// (same atlas trie, engine fold conventions) and diff BIT-EXACT — counts and
/// measure bit patterns alike, no tolerance. Exit 1 on any divergence.
pub fn run_engine_check(file: &Path, trie_path: Option<&Path>) -> ! {
    let default_trie = crate::default_engine_trie();
    let trie_path = trie_path.unwrap_or(&default_trie);
    let bytes = std::fs::read(file).expect("failed to read --engine-check file");
    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());

    // TWO origins, and the non-zero one is the point. At (0,0,0) an
    // uninitialised origin read is invisible: garbage added to zero on both
    // sides of the comparison agrees with itself. A Mojo nightly was caught
    // doing exactly that (see engine_item_params_at), and for a while
    // ffi_selftest was the only instrument that could see it.
    for origin in [[0.0, 0.0, 0.0], [-3.5, 11.25, 2.75]] {
        let records = crate::engine_layout_records_at(file, trie_path, origin);
        let p = crate::engine_item_params_at(origin);
        let expected = crate::text::reference_layout(
            &trie,
            &bytes,
            [p.origin_x, p.origin_y, p.origin_z],
            p.line_height,
        );
        if let Err(report) = crate::text::diff_records(&records, &expected) {
            eprintln!(
                "engine-check FAIL: {} at origin {origin:?} (trie: {})\n{report}",
                file.display(),
                trie_path.display(),
            );
            std::process::exit(1);
        }
        println!(
            "engine-check PASS: {} ({} B) at origin {origin:?} — {} records bit-exact vs the \
             CPU reference [fp contract=off verified at the dylib] (trie: {})",
            file.display(),
            bytes.len(),
            records.len(),
            trie_path.display(),
        );
    }
    std::process::exit(0);
}
