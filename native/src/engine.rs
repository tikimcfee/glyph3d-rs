//! engine.rs — safe Rust wrapper over the Mojo glyph engine's C ABI (Stage D).
//!
//! The FFI surface (engine-local/ffi.mojo) is scalars + one opaque handle:
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
