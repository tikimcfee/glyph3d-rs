//! Runtime glyph and sparse colour-emoji source for the terminal wall (`GlyphSource`).
//!
//! Bridges `crates/glyph-font` and `assets/atlas/emoji-sheet.bin` into `native/src/wall/`
//! without loading the engine's baked `Atlas` (`curves.bin`, `glyphmap.bin`, `codepoints.bin`,
//! or the 361 MiB pre-decoded emoji mip cache):
//! - [`EmojiSheetArchive`] parses only the `G3ES` v1 header, cell table, codepoint table,
//!   sequence table, and raw PNG blob from `emoji-sheet.bin` (~4 ms, zero PNG decoding).
//! - [`SparseEmojiSource`] implements [`glyph_font::EmojiSource`] by allocating dense
//!   on-demand indices (`0, 1, 2, ...`) only for emoji cells actually requested, so
//!   `FontAtlas` glyph-map entries `[0, 0, 1, sparse_idx]` point directly into the wall's
//!   sparse GPU emoji texture.
//! - Under the `wgpu` feature, `GlyphSource` owns the [`glyph_font::FontAtlas`], the
//!   `Rgba32Uint` `glyphmap` and `curves` textures (`wall_cells.wgsl` bindings 1 & 2),
//!   and the sparse growable `Rgba8UnormSrgb` `D2Array` emoji texture (binding 3, starting
//!   at 1×1×1 until the first emoji is used, then growing `8×2 -> 8×4 -> ... -> 8×32`
//!   and adding array layers beyond 256 unique emoji, with 4 linear-premultiplied box-filter
//!   mip levels per decoded 136×128 cell).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use glyph_font::EmojiSource;
#[cfg(feature = "wgpu")]
use glyph_font::{
    default_font_chain, normalize_family_key, resolve_family_chain, FamilyId, FontAtlas, FontError,
    LineMetrics, ResolvedCell, TextureBacking, TextureDirty, CURVE_INITIAL_ROWS, MAP_INITIAL_ROWS,
    MONASPACE_NEON_FAMILY, TEXTURE_WIDTH, U32S_PER_TEXEL,
};
#[cfg(feature = "wgpu")]
use term_cells::{Cell, Screen};

#[cfg(feature = "wgpu")]
use crate::text::CELL_HEIGHT_WORLD;

#[cfg(feature = "wgpu")]
use super::metrics::CellMetrics;
#[cfg(feature = "wgpu")]
use super::renderer::WallStyle;

/// `emoji-sheet.bin` (`G3ES` v1) binary constants (`assets/atlas/FORMAT.md`).
const SHEET_MAGIC: &[u8; 4] = b"G3ES";
const SHEET_VERSION: u32 = 1;
const SHEET_HEADER_WORDS: usize = 40;
const SHEET_CELL_STRIDE: usize = 6;

/// Bytes per `Rgba32Uint` texel (`4 * sizeof(u32)`).
#[cfg(feature = "wgpu")]
const BYTES_PER_TEXEL: u32 = U32S_PER_TEXEL * 4;

/// Default sparse emoji texture grid parameters:
/// - `8` columns (`8 * 136 = 1088` px; exact integer cell boundaries across all 4 mips)
/// - `2` initial rows (`16` cells = `1088 * 256 * 1` × 4 mips ≈ `1.41 MiB`)
/// - `32` max rows per layer (`256` cells/layer = `1088 * 4096`), then array layers double
///   while preserving every previously allocated sparse cell's `(layer, col, row)` index.
#[cfg(feature = "wgpu")]
pub const SPARSE_EMOJI_COLS: u32 = 8;
#[cfg(feature = "wgpu")]
pub const SPARSE_EMOJI_INIT_ROWS: u32 = 2;
#[cfg(feature = "wgpu")]
pub const SPARSE_EMOJI_MAX_ROWS: u32 = 32;

/// Default directory holding the wall's runtime font files (`<repo>/assets/fonts`).
pub fn default_fonts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts")
}

/// One cell record in [`EmojiSheetArchive`] (`[glyph_id, png_offset, png_len]`).
#[derive(Clone, Copy, Debug)]
struct ArchiveCell {
    glyph_id: u32,
    png_offset: u32,
    png_len: u32,
}

/// Lightweight, lazy reader for `assets/atlas/emoji-sheet.bin` (`G3ES` v1).
///
/// Parses the codepoint table, sequence table, and per-cell PNG offsets in a few
/// milliseconds without decoding any PNG or loading the engine's baked `Atlas`.
/// Individual cells are decoded on first use via [`Self::decode_cell_mips`].
pub struct EmojiSheetArchive {
    cell_w: u32,
    cell_h: u32,
    mip_levels: u32,
    cells: Vec<ArchiveCell>,
    codepoints: Vec<(u32, u32)>,
    seq_max: u32,
    sequences: Vec<u32>,
    seq_leads: HashSet<u32>,
    png: Vec<u8>,
}

impl EmojiSheetArchive {
    /// Load and validate `G3ES` v1 tables + compressed PNG blob from `path`.
    pub fn load(path: &Path) -> Self {
        let t0 = std::time::Instant::now();
        let bytes = std::fs::read(path)
            .unwrap_or_else(|e| panic!("failed to read emoji sheet {}: {e}", path.display()));
        let archive = Self::from_bytes(&bytes, &path.display().to_string());
        let seq_count = archive.sequences.len() / (2 + archive.seq_max as usize);
        log::info!(
            "wall sparse emoji archive: {} cells ({}x{}, {} mips), {} codepoints, {} sequences; parse {:.1} ms (0 decoded)",
            archive.cells.len(),
            archive.cell_w,
            archive.cell_h,
            archive.mip_levels,
            archive.codepoints.len(),
            seq_count,
            t0.elapsed().as_secs_f64() * 1e3,
        );
        archive
    }

    /// Parse a `G3ES` v1 file from raw bytes.
    pub fn from_bytes(bytes: &[u8], what: &str) -> Self {
        assert!(
            bytes.len() >= SHEET_HEADER_WORDS * 4,
            "{what}: truncated emoji-sheet header ({} bytes)",
            bytes.len()
        );
        assert_eq!(&bytes[0..4], SHEET_MAGIC, "{what}: bad G3ES magic");
        let word = |i: usize| -> u32 {
            u32::from_le_bytes(
                bytes[i * 4..i * 4 + 4]
                    .try_into()
                    .expect("4-byte u32 slice"),
            )
        };
        let version = word(1);
        let header_bytes = word(2);
        assert_eq!(version, SHEET_VERSION, "{what}: unsupported G3ES version {version}");
        assert_eq!(
            header_bytes as usize,
            SHEET_HEADER_WORDS * 4,
            "{what}: unexpected G3ES headerBytes {header_bytes}"
        );

        let cell_w = word(3);
        let cell_h = word(4);
        let cell_count = word(5) as usize;
        let cp_count = word(21) as usize;
        let seq_count = word(22) as usize;
        let seq_max = word(23);
        let png_start = word(24) as usize;
        let png_len = word(25) as usize;
        assert!(cell_w > 0 && cell_h > 0, "{what}: zero cell dimensions");
        assert_eq!(png_start + png_len, bytes.len(), "{what}: PNG blob does not end file");

        let mut at = SHEET_HEADER_WORDS;
        let mut cells = Vec::with_capacity(cell_count);
        let mut last_gid = None;
        for i in 0..cell_count {
            let glyph_id = word(at);
            let png_offset = word(at + 4);
            let cell_png_len = word(at + 5);
            at += SHEET_CELL_STRIDE;
            assert!(
                last_gid.is_none_or(|prev| glyph_id > prev),
                "{what}: cell {i} glyph ids not strictly ascending"
            );
            last_gid = Some(glyph_id);
            assert!(
                (png_offset as usize) + (cell_png_len as usize) <= png_len,
                "{what}: cell {i} PNG range outside blob"
            );
            cells.push(ArchiveCell {
                glyph_id,
                png_offset,
                png_len: cell_png_len,
            });
        }

        let mut codepoints = Vec::with_capacity(cp_count);
        for _ in 0..cp_count {
            codepoints.push((word(at), word(at + 1)));
            at += 2;
        }
        assert!(
            codepoints.windows(2).all(|w| w[0].0 < w[1].0),
            "{what}: codepoint table not ascending"
        );

        let seq_stride = 2 + seq_max as usize;
        let seq_words = seq_count * seq_stride;
        assert!(
            (at + seq_words) * 4 <= png_start,
            "{what}: sequence table overruns PNG blob"
        );
        let mut sequences = Vec::with_capacity(seq_words);
        let mut seq_leads = HashSet::new();
        for s in 0..seq_count {
            let base = at + s * seq_stride;
            let len = word(base);
            assert!(len >= 2 && len <= seq_max, "{what}: invalid sequence length {len}");
            seq_leads.insert(word(base + 2));
            for k in 0..seq_stride {
                sequences.push(word(base + k));
            }
        }

        let mip_levels = mip_levels_for_cell(cell_w, cell_h);
        Self {
            cell_w,
            cell_h,
            mip_levels,
            cells,
            codepoints,
            seq_max,
            sequences,
            seq_leads,
            png: bytes[png_start..].to_vec(),
        }
    }

    /// Cell pixel dimensions (`136, 128` for the vendored Noto sheet).
    #[inline]
    pub fn cell_size(&self) -> (u32, u32) {
        (self.cell_w, self.cell_h)
    }

    /// Number of exact box-filter mip levels supported by each cell (`4` for `136×128`).
    #[inline]
    pub fn mip_levels(&self) -> u32 {
        self.mip_levels
    }

    /// Total number of bitmap cells stored in the archive.
    #[inline]
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Compressed PNG bytes for archive cell index `sheet_cell`.
    pub fn png_of(&self, sheet_cell: u32) -> &[u8] {
        let c = &self.cells[sheet_cell as usize];
        &self.png[c.png_offset as usize..][..c.png_len as usize]
    }

    fn cell_index_for_glyph(&self, glyph_id: u32) -> Option<u32> {
        self.cells
            .binary_search_by_key(&glyph_id, |c| c.glyph_id)
            .ok()
            .map(|idx| idx as u32)
    }

    /// Look up the archive cell index for a single Unicode scalar.
    pub fn sheet_cell_for_single(&self, cp: u32) -> Option<u32> {
        let idx = self.codepoints.binary_search_by_key(&cp, |&(k, _)| k).ok()?;
        let glyph_id = self.codepoints[idx].1;
        self.cell_index_for_glyph(glyph_id)
    }

    /// Look up the archive cell index for a sequence of codepoints (with `U+FE0F` stripped).
    pub fn sheet_cell_for_sequence(&self, cps: &[u32]) -> Option<u32> {
        if cps.len() < 2 || self.sequences.is_empty() || !self.seq_leads.contains(&cps[0]) {
            return None;
        }
        let max_len = cps.len().min(self.seq_max as usize);
        for probe_len in (2..=max_len).rev() {
            if let Some(glyph_id) = self.sequence_glyph_lookup(&cps[..probe_len]) {
                if let Some(cell) = self.cell_index_for_glyph(glyph_id) {
                    return Some(cell);
                }
            }
        }
        None
    }

    fn sequence_glyph_lookup(&self, probe: &[u32]) -> Option<u32> {
        let stride = 2 + self.seq_max as usize;
        let n = self.sequences.len() / stride;
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let o = mid * stride;
            let len = self.sequences[o] as usize;
            let entry_cps = &self.sequences[o + 2..o + 2 + len];
            match probe.cmp(entry_cps) {
                std::cmp::Ordering::Equal => return Some(self.sequences[o + 1]),
                std::cmp::Ordering::Less => hi = mid,
                std::cmp::Ordering::Greater => lo = mid + 1,
            }
        }
        None
    }

    /// Resolve a UTF-8 cluster/emoji string (stripping `U+FE0F`) to its archive cell index.
    pub fn sheet_cell_for_text(&self, text: &str) -> Option<u32> {
        let mut key = [0u32; 16];
        let mut len = 0usize;
        for ch in text.chars() {
            let cp = ch as u32;
            if cp == 0xFE0F {
                continue;
            }
            if len == key.len() {
                return None;
            }
            key[len] = cp;
            len += 1;
        }
        if len == 0 {
            return None;
        }
        if len == 1 {
            return self.sheet_cell_for_single(key[0]);
        }
        self.sheet_cell_for_sequence(&key[..len])
    }

    /// Decode ONLY `sheet_cell`'s PNG into straight-alpha sRGB RGBA8 and generate its
    /// `mip_levels` linear-light premultiplied 2×2 box-filter mip chain.
    pub fn decode_cell_mips(&self, sheet_cell: u32) -> Vec<(u32, u32, Vec<u8>)> {
        let png = self.png_of(sheet_cell);
        let img = image::load_from_memory(png)
            .unwrap_or_else(|e| panic!("failed to decode emoji cell {sheet_cell}: {e}"))
            .into_rgba8();
        assert_eq!(
            img.dimensions(),
            (self.cell_w, self.cell_h),
            "emoji cell {sheet_cell} dimensions mismatch"
        );
        let mut chain = Vec::with_capacity(self.mip_levels as usize);
        let mut w = self.cell_w;
        let mut h = self.cell_h;
        chain.push((w, h, img.into_raw()));
        for _ in 1..self.mip_levels {
            let prev = &chain.last().expect("mip 0 exists").2;
            let next = box_down_linear_premul(prev, w, h);
            w /= 2;
            h /= 2;
            chain.push((w, h, next));
        }
        chain
    }
}

/// Maximum number of 2×2 mip levels whose footprint still tiles `(cell_w, cell_h)` with
/// integer cell boundaries (`4` for `136×128`: `136×128 -> 68×64 -> 34×32 -> 17×16`).
fn mip_levels_for_cell(mut w: u32, mut h: u32) -> u32 {
    let mut levels = 1u32;
    while w >= 2 && h >= 2 && w.is_multiple_of(2) && h.is_multiple_of(2) {
        w /= 2;
        h /= 2;
        levels += 1;
    }
    levels
}

fn srgb_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0.0f32; 256];
        for (i, slot) in lut.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *slot = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        lut
    })
}

#[inline]
fn linear_to_srgb_u8(l: f32) -> u8 {
    let clamped = l.clamp(0.0, 1.0);
    let s = if clamped <= 0.0031308 {
        clamped * 12.92
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// 2×2 box downsample of a straight-alpha sRGB RGBA8 cell in **premultiplied linear-light**
/// space, returned as straight-alpha sRGB RGBA8.
///
/// Premultiplied weighting keeps transparent texels' arbitrary palette RGB from bleeding dark
/// fringes into opaque edges, and linear-light averaging matches `Rgba8UnormSrgb` hardware
/// sampling so bright saturated emoji don't darken when minified.
fn box_down_linear_premul(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    assert!(w.is_multiple_of(2) && h.is_multiple_of(2));
    let lut = srgb_to_linear_lut();
    let (w, h) = (w as usize, h as usize);
    let (ow, oh) = (w / 2, h / 2);
    let mut out = vec![0u8; ow * oh * 4];
    let src_stride = w * 4;
    for y in 0..oh {
        let row0 = &src[(2 * y) * src_stride..(2 * y + 1) * src_stride];
        let row1 = &src[(2 * y + 1) * src_stride..(2 * y + 2) * src_stride];
        let dst_row = &mut out[y * ow * 4..(y + 1) * ow * 4];
        for x in 0..ow {
            let x8 = x * 8;
            let p = [
                &row0[x8..x8 + 4],
                &row0[x8 + 4..x8 + 8],
                &row1[x8..x8 + 4],
                &row1[x8 + 4..x8 + 8],
            ];
            let mut sum_r = 0.0f32;
            let mut sum_g = 0.0f32;
            let mut sum_b = 0.0f32;
            let mut sum_a = 0.0f32;
            for px in p {
                let a = px[3] as f32 / 255.0;
                sum_r += lut[px[0] as usize] * a;
                sum_g += lut[px[1] as usize] * a;
                sum_b += lut[px[2] as usize] * a;
                sum_a += a;
            }
            let o = x * 4;
            let avg_a = sum_a * 0.25;
            let a_u8 = (avg_a * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
            dst_row[o + 3] = a_u8;
            if sum_a > 1e-6 {
                let inv = 1.0 / sum_a;
                dst_row[o] = linear_to_srgb_u8(sum_r * inv);
                dst_row[o + 1] = linear_to_srgb_u8(sum_g * inv);
                dst_row[o + 2] = linear_to_srgb_u8(sum_b * inv);
            }
        }
    }
    out
}

#[derive(Default)]
struct SparseEmojiState {
    sheet_to_sparse: HashMap<u32, u32>,
    sparse_to_sheet: Vec<u32>,
    pending: Vec<u32>,
}

/// On-demand [`glyph_font::EmojiSource`] backed by [`EmojiSheetArchive`].
///
/// Assigns dense sparse cell indices (`0, 1, 2, ...`) only to emoji cells actually queried,
/// queuing newly allocated cells for [`GlyphSource::sync`] to decode and upload into the
/// wall's sparse GPU emoji texture.
pub struct SparseEmojiSource {
    archive: Arc<EmojiSheetArchive>,
    state: Mutex<SparseEmojiState>,
}

impl SparseEmojiSource {
    pub fn new(archive: Arc<EmojiSheetArchive>) -> Self {
        Self {
            archive,
            state: Mutex::new(SparseEmojiState::default()),
        }
    }

    pub fn archive(&self) -> &EmojiSheetArchive {
        &self.archive
    }

    /// Number of unique emoji cells allocated so far.
    pub fn allocated_count(&self) -> u32 {
        self.state
            .lock()
            .expect("sparse emoji state lock")
            .sparse_to_sheet
            .len() as u32
    }

    /// Archive cell index backing a previously allocated `sparse_idx`.
    pub fn sheet_cell_of(&self, sparse_idx: u32) -> Option<u32> {
        self.state
            .lock()
            .expect("sparse emoji state lock")
            .sparse_to_sheet
            .get(sparse_idx as usize)
            .copied()
    }

    /// Drain newly allocated `(sparse_idx, sheet_cell)` pairs and return `(pending, total_allocated)`.
    #[cfg(feature = "wgpu")]
    fn take_pending(&self) -> (Vec<(u32, u32)>, u32) {
        let mut guard = self.state.lock().expect("sparse emoji state lock");
        let st = &mut *guard;
        let total = st.sparse_to_sheet.len() as u32;
        if st.pending.is_empty() {
            return (Vec::new(), total);
        }
        let sparse_to_sheet = &st.sparse_to_sheet;
        let pending = st
            .pending
            .drain(..)
            .map(|sparse_idx| (sparse_idx, sparse_to_sheet[sparse_idx as usize]))
            .collect();
        (pending, total)
    }
}

impl EmojiSource for SparseEmojiSource {
    fn cell_for(&self, text: &str) -> Option<u32> {
        let sheet_cell = self.archive.sheet_cell_for_text(text)?;
        let mut st = self.state.lock().expect("sparse emoji state lock");
        if let Some(&sparse_idx) = st.sheet_to_sparse.get(&sheet_cell) {
            return Some(sparse_idx);
        }
        let sparse_idx = st.sparse_to_sheet.len() as u32;
        st.sheet_to_sparse.insert(sheet_cell, sparse_idx);
        st.sparse_to_sheet.push(sheet_cell);
        st.pending.push(sparse_idx);
        Some(sparse_idx)
    }
}

/// Live geometry and memory footprint of [`GlyphSource`]'s sparse GPU emoji texture.
#[cfg(feature = "wgpu")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SparseEmojiLayout {
    pub cell_w: u32,
    pub cell_h: u32,
    pub cols: u32,
    pub rows: u32,
    pub layers: u32,
    pub layer_w: u32,
    pub layer_h: u32,
    pub mip_levels: u32,
    pub allocated_cells: u32,
    pub gpu_bytes: u64,
}

/// Per-frame GPU upload report returned by [`GlyphSource::sync`].
#[cfg(feature = "wgpu")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphSyncStats {
    /// True when the `curves`, `glyphmap`, or sparse `emoji` texture was recreated after growing.
    /// Callers holding a `BindGroup` over [`GlyphSource::glyphmap_view`] /
    /// [`GlyphSource::curves_view`] / [`GlyphSource::emoji_view`] must rebuild that bind group.
    pub rebound: bool,
    /// Number of `queue.write_texture` calls issued across Slug and sparse emoji textures.
    pub write_calls: usize,
    /// Total `Rgba32Uint` texels uploaded across the `curves` and `glyphmap` textures.
    pub texels_uploaded: u64,
    /// Number of newly decoded sparse emoji cells uploaded this frame.
    pub emoji_cells_uploaded: usize,
    /// Total bytes uploaded across `curves`, `glyphmap`, and sparse `emoji` textures.
    pub bytes_uploaded: u64,
}

/// Sparse growable GPU emoji texture backing `wall_cells.wgsl` group 0 binding 3.
#[cfg(feature = "wgpu")]
struct SparseEmojiGpu {
    source: Arc<SparseEmojiSource>,
    cols_cfg: u32,
    init_rows_cfg: u32,
    max_rows_cfg: u32,
    cell_w: u32,
    cell_h: u32,
    mip_levels: u32,
    /// Active grid dimensions (`0, 0, 0` while the 1×1×1 placeholder is bound).
    cols: u32,
    rows: u32,
    layers: u32,
    allocated_cells: u32,
    gpu_bytes: u64,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

#[cfg(feature = "wgpu")]
impl SparseEmojiGpu {
    fn new(
        device: &wgpu::Device,
        source: Arc<SparseEmojiSource>,
        cols_cfg: u32,
        init_rows_cfg: u32,
        max_rows_cfg: u32,
    ) -> Self {
        assert!(cols_cfg > 0 && init_rows_cfg > 0 && max_rows_cfg >= init_rows_cfg);
        let (cell_w, cell_h) = source.archive().cell_size();
        let mip_levels = source.archive().mip_levels();
        // Start with a 1x1x1 (4-byte) placeholder until an emoji is actually used.
        let (texture, view) = create_emoji_texture(device, "wall sparse emoji (empty)", 1, 1, 1, 1);
        Self {
            source,
            cols_cfg,
            init_rows_cfg,
            max_rows_cfg,
            cell_w,
            cell_h,
            mip_levels,
            cols: 0,
            rows: 0,
            layers: 0,
            allocated_cells: 0,
            gpu_bytes: 4,
            texture,
            view,
        }
    }

    fn layout(&self) -> SparseEmojiLayout {
        let (layer_w, layer_h) = if self.cols == 0 || self.rows == 0 {
            (1, 1)
        } else {
            (self.cols * self.cell_w, self.rows * self.cell_h)
        };
        SparseEmojiLayout {
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            cols: self.cols,
            rows: self.rows,
            layers: self.layers,
            layer_w,
            layer_h,
            mip_levels: if self.layers == 0 { 1 } else { self.mip_levels },
            allocated_cells: self.allocated_cells,
            gpu_bytes: self.gpu_bytes,
        }
    }

    fn required_dims(&self, total_cells: u32) -> (u32, u32, u32) {
        if total_cells == 0 {
            return (0, 0, 0);
        }
        let cols = self.cols_cfg;
        let max_per_layer = cols * self.max_rows_cfg;
        if total_cells <= max_per_layer {
            let mut rows = self.init_rows_cfg;
            while cols * rows < total_cells {
                rows = (rows * 2).min(self.max_rows_cfg);
            }
            (cols, rows, 1)
        } else {
            let needed_layers = total_cells.div_ceil(max_per_layer);
            (cols, self.max_rows_cfg, needed_layers.next_power_of_two())
        }
    }

    fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, stats: &mut GlyphSyncStats) {
        let (pending, total) = self.source.take_pending();
        if pending.is_empty() {
            return;
        }
        self.allocated_cells = total;

        let (need_cols, need_rows, need_layers) = self.required_dims(total);
        if need_cols > self.cols || need_rows > self.rows || need_layers > self.layers {
            let new_w = need_cols * self.cell_w;
            let new_h = need_rows * self.cell_h;
            let (new_tex, new_view) = create_emoji_texture(
                device,
                "wall sparse emoji",
                new_w,
                new_h,
                need_layers,
                self.mip_levels,
            );
            // Preserve all already-uploaded cells via GPU-to-GPU blit across every mip level.
            if self.cols > 0 && self.rows > 0 && self.layers > 0 {
                let old_w = self.cols * self.cell_w;
                let old_h = self.rows * self.cell_h;
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("wall sparse emoji grow copy"),
                });
                for mip in 0..self.mip_levels {
                    encoder.copy_texture_to_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &self.texture,
                            mip_level: mip,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::TexelCopyTextureInfo {
                            texture: &new_tex,
                            mip_level: mip,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::Extent3d {
                            width: old_w >> mip,
                            height: old_h >> mip,
                            depth_or_array_layers: self.layers,
                        },
                    );
                }
                queue.submit([encoder.finish()]);
            }
            self.texture = new_tex;
            self.view = new_view;
            self.cols = need_cols;
            self.rows = need_rows;
            self.layers = need_layers;
            self.gpu_bytes = mip_chain_bytes(new_w, new_h, need_layers, self.mip_levels);
            log::info!(
                "wall sparse emoji texture: {}x{}x{} ({}x{} px, {} mips, {:.2} MiB; {} cell(s) allocated)",
                self.cols,
                self.rows,
                self.layers,
                new_w,
                new_h,
                self.mip_levels,
                self.gpu_bytes as f64 / (1 << 20) as f64,
                total,
            );
            stats.rebound = true;
        }

        let per_layer = self.cols * self.rows;
        let archive = self.source.archive();
        for (sparse_idx, sheet_cell) in pending {
            let layer = sparse_idx / per_layer;
            let rem = sparse_idx % per_layer;
            let col = rem % self.cols;
            let row = rem / self.cols;
            let mips = archive.decode_cell_mips(sheet_cell);
            for (mip, (mw, mh, pixels)) in mips.iter().enumerate() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.texture,
                        mip_level: mip as u32,
                        origin: wgpu::Origin3d {
                            x: col * mw,
                            y: row * mh,
                            z: layer,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(mw * 4),
                        rows_per_image: Some(*mh),
                    },
                    wgpu::Extent3d {
                        width: *mw,
                        height: *mh,
                        depth_or_array_layers: 1,
                    },
                );
                stats.write_calls += 1;
                stats.bytes_uploaded += pixels.len() as u64;
            }
            stats.emoji_cells_uploaded += 1;
        }
    }
}

#[cfg(feature = "wgpu")]
fn mip_chain_bytes(w: u32, h: u32, layers: u32, mip_levels: u32) -> u64 {
    let mut per_layer = 0u64;
    let (mut mw, mut mh) = (w, h);
    for _ in 0..mip_levels {
        per_layer += u64::from(mw) * u64::from(mh) * 4;
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
    }
    per_layer * u64::from(layers)
}

#[cfg(feature = "wgpu")]
fn create_emoji_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    layers: u32,
    mip_levels: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: layers,
        },
        mip_level_count: mip_levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    (tex, view)
}

/// Runtime Slug glyph and sparse emoji source owning a [`FontAtlas`] and its live `wgpu`
/// `glyphmap`, `curves` (`Rgba32Uint`), and sparse `emoji` (`Rgba8UnormSrgb` `D2Array`)
/// textures — a complete drop-in for `wall_cells.wgsl` group 0 bindings 1, 2, and 3.
#[cfg(feature = "wgpu")]
pub struct GlyphSource {
    fonts_dir: PathBuf,
    curve_rows_cfg: u32,
    map_rows_cfg: u32,
    atlas: FontAtlas,
    active_family: FamilyId,
    active_family_name: String,
    line_height_scale: f64,
    curves_tex: wgpu::Texture,
    curves_view: wgpu::TextureView,
    glyphmap_tex: wgpu::Texture,
    glyphmap_view: wgpu::TextureView,
    emoji: SparseEmojiGpu,
}

#[cfg(feature = "wgpu")]
impl GlyphSource {
    /// Load the wall's default font chain from `assets/fonts/` (selecting Monaspace Neon
    /// as the active family, with Cousine and Monaspace Argon available), parse only the
    /// index tables of `emoji_sheet` (zero PNG decoding at startup), and allocate the
    /// initial GPU textures.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, emoji_sheet: &Path) -> Self {
        Self::load(device, queue, &default_fonts_dir(), emoji_sheet)
            .expect("failed to load default wall font chain from assets/fonts")
    }

    /// Load the default font chain from `fonts_dir` and the sparse emoji archive from `emoji_sheet`.
    pub fn load(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fonts_dir: &Path,
        emoji_sheet: &Path,
    ) -> Result<Self, FontError> {
        let archive = Arc::new(EmojiSheetArchive::load(emoji_sheet));
        Self::load_with_capacities(
            device,
            queue,
            fonts_dir,
            archive,
            CURVE_INITIAL_ROWS,
            MAP_INITIAL_ROWS,
            SPARSE_EMOJI_COLS,
            SPARSE_EMOJI_INIT_ROWS,
            SPARSE_EMOJI_MAX_ROWS,
        )
    }

    /// Load with explicit curve/glyphmap row capacities and sparse emoji grid parameters.
    #[expect(
        clippy::too_many_arguments,
        reason = "testable constructor exposing initial Slug and sparse emoji texture capacities"
    )]
    pub fn load_with_capacities(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fonts_dir: &Path,
        archive: Arc<EmojiSheetArchive>,
        curve_rows: u32,
        map_rows: u32,
        emoji_cols: u32,
        emoji_init_rows: u32,
        emoji_max_rows: u32,
    ) -> Result<Self, FontError> {
        let specs = default_font_chain(fonts_dir);
        let mut atlas = FontAtlas::load_with_initial_rows(&specs, curve_rows, map_rows)?;
        let sparse_source = Arc::new(SparseEmojiSource::new(archive));
        let emoji_dyn: Arc<dyn EmojiSource> = sparse_source.clone();
        atlas.set_emoji_source(Some(emoji_dyn));

        // JS wall default: Monaspace Neon; fall back to chain default if absent.
        let active_family = atlas
            .family_id(MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());

        // Drain any initial clean state and upload the zero-initialised initial textures so
        // every texel on the device starts defined (slot 0 = [0, 0, 0, 0]).
        let _ = atlas.take_dirty();
        let (curves_tex, curves_view) =
            create_and_upload_full(device, queue, "wall slug curves", atlas.curve_texture());
        let (glyphmap_tex, glyphmap_view) = create_and_upload_full(
            device,
            queue,
            "wall slug glyphmap",
            atlas.glyphmap_texture(),
        );
        let emoji = SparseEmojiGpu::new(
            device,
            sparse_source,
            emoji_cols,
            emoji_init_rows,
            emoji_max_rows,
        );

        Ok(Self {
            fonts_dir: fonts_dir.to_path_buf(),
            curve_rows_cfg: curve_rows,
            map_rows_cfg: map_rows,
            atlas,
            active_family,
            active_family_name: MONASPACE_NEON_FAMILY.to_string(),
            line_height_scale: 1.0,
            curves_tex,
            curves_view,
            glyphmap_tex,
            glyphmap_view,
            emoji,
        })
    }

    /// Underlying [`FontAtlas`] (for diagnostics, family listing, or readback verification).
    pub fn atlas(&self) -> &FontAtlas {
        &self.atlas
    }

    /// Mutable access to the underlying [`FontAtlas`] (dirty tracking remains automatic;
    /// call [`Self::sync`] before drawing).
    pub fn atlas_mut(&mut self) -> &mut FontAtlas {
        &mut self.atlas
    }

    /// Currently selected font family (default: Monaspace Neon).
    #[inline]
    pub fn family(&self) -> FamilyId {
        self.active_family
    }

    /// Name of the currently loaded font family (bundled id or system family name).
    #[inline]
    pub fn active_family_name(&self) -> &str {
        &self.active_family_name
    }

    /// Currently active vertical line-height multiplier (`1.0` default).
    #[inline]
    pub fn line_height_scale(&self) -> f64 {
        self.line_height_scale
    }

    /// Load `family_name` (bundled id or installed system font name on macOS/Linux) with
    /// `line_height_scale` into a fresh [`FontAtlas`] and fresh GPU `curves`/`glyphmap`
    /// textures, dropping the previous atlas and textures immediately (zero leak).
    ///
    /// Returns `Ok(false)` if the requested family and line height are already active,
    /// `Ok(true)` if reloaded (callers must rebuild bind groups over `glyphmap_view`/`curves_view`
    /// and repack terminals), or `Err(FontError)` if the family is not installed (leaving the
    /// current atlas and textures untouched).
    pub fn reload_family(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        family_name: &str,
        line_height_scale: f64,
    ) -> Result<bool, FontError> {
        let scale = if line_height_scale.is_finite() && line_height_scale > 0.0 {
            line_height_scale
        } else {
            1.0
        };
        if normalize_family_key(family_name) == normalize_family_key(&self.active_family_name)
            && (scale - self.line_height_scale).abs() <= 1e-6
        {
            return Ok(false);
        }

        let specs = resolve_family_chain(&self.fonts_dir, family_name, scale)?;
        let mut new_atlas =
            FontAtlas::load_with_initial_rows(&specs, self.curve_rows_cfg, self.map_rows_cfg)?;
        let emoji_dyn: Arc<dyn EmojiSource> = self.emoji.source.clone();
        new_atlas.set_emoji_source(Some(emoji_dyn));
        let _ = new_atlas.take_dirty();

        let (curves_tex, curves_view) =
            create_and_upload_full(device, queue, "wall slug curves", new_atlas.curve_texture());
        let (glyphmap_tex, glyphmap_view) = create_and_upload_full(
            device,
            queue,
            "wall slug glyphmap",
            new_atlas.glyphmap_texture(),
        );

        let active_family = new_atlas.default_family();
        // Drop the previous FontAtlas and GPU textures in place — no old atlas retained.
        self.atlas = new_atlas;
        self.active_family = active_family;
        self.active_family_name = family_name.trim().to_owned();
        self.line_height_scale = scale;
        self.curves_tex = curves_tex;
        self.curves_view = curves_view;
        self.glyphmap_tex = glyphmap_tex;
        self.glyphmap_view = glyphmap_view;
        Ok(true)
    }

    /// Look up a family handle by id (`"monaspace-neon"`, `"cousine"`, `"monaspace-argon"`).
    pub fn family_id(&self, name: &str) -> Option<FamilyId> {
        self.atlas.family_id(name)
    }

    /// Switch the active font family by id (`"monaspace-neon"`, `"cousine"`, `"monaspace-argon"`).
    /// Returns the selected [`FamilyId`] or `None` if `name` is not registered.
    pub fn set_family(&mut self, name: &str) -> Option<FamilyId> {
        let id = self.atlas.family_id(name)?;
        self.active_family = id;
        self.active_family_name = name.to_string();
        Some(id)
    }

    /// Switch the active font family directly by [`FamilyId`].
    pub fn set_family_id(&mut self, family: FamilyId) {
        assert!(
            family.0 < self.atlas.family_count(),
            "family id {} out of range ({} families)",
            family.0,
            self.atlas.family_count()
        );
        self.active_family = family;
        if let Some(Some(id)) = self.atlas.family_ids().get(family.0) {
            self.active_family_name = (*id).to_string();
        }
    }

    /// Resolve `(slot, span_cols)` for `cell` in the active font family.
    ///
    /// Covers ASCII/Unicode scalars, bold/italic faces from `cell.attrs`, cell sprites,
    /// combining-mark clusters, colour emoji, spaces, blanks, and trailing wide cells.
    pub fn resolve_cell(&mut self, screen: &Screen, cell: &Cell) -> ResolvedCell {
        self.atlas.resolve_cell(self.active_family, screen, cell)
    }

    /// Resolve `(slot, span_cols)` for `cell` in an explicit font `family`.
    pub fn resolve_cell_in(
        &mut self,
        family: FamilyId,
        screen: &Screen,
        cell: &Cell,
    ) -> ResolvedCell {
        self.atlas.resolve_cell(family, screen, cell)
    }

    /// Resolve a UTF-8 label string into `(slot, span_cols)` pairs in the active family.
    pub fn resolve_str_glyphs(&mut self, text: &str, emit: impl FnMut(u32, u8)) {
        super::cell::resolve_str_glyphs(&mut self.atlas, self.active_family, text, emit);
    }

    /// Measure the unclipped world width of `text` drawn at `height_world` in the active family.
    pub fn measure_label_width(&mut self, text: &str, height_world: f32) -> f32 {
        let metrics = self.cell_metrics();
        super::labels::measure_label_width(metrics, &mut self.atlas, self.active_family, text, height_world)
    }

    /// Snapped world-space [`CellMetrics`] for the active family (`CELL_HEIGHT_WORLD = 1.0`).
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell_metrics_for(self.active_family)
    }

    /// Snapped world-space [`CellMetrics`] for `family`.
    pub fn cell_metrics_for(&self, family: FamilyId) -> CellMetrics {
        let m = self.atlas.family_metrics(family);
        let cell_height = CELL_HEIGHT_WORLD * self.line_height_scale as f32;
        let raw_width = (f64::from(m.advance_fu) * f64::from(cell_height)
            / m.extents.em_height()) as f32;
        CellMetrics::snapped(raw_width, cell_height)
    }

    /// Normalised `[0, 1]` cell-space underline/strikethrough metrics for the active family.
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        self.line_metrics_for(self.active_family)
    }

    /// Normalised `[0, 1]` cell-space underline/strikethrough metrics for `family`.
    pub fn line_metrics_for(&self, family: FamilyId) -> Option<LineMetrics> {
        self.atlas.family_metrics(family).lines
    }

    /// Copy the active family's underline/strikethrough position and thickness into `style`.
    pub fn apply_line_metrics(&self, style: &mut WallStyle) {
        self.apply_line_metrics_for(self.active_family, style);
    }

    /// Copy `family`'s underline/strikethrough position and thickness into `style`.
    pub fn apply_line_metrics_for(&self, family: FamilyId, style: &mut WallStyle) {
        if let Some(lines) = self.line_metrics_for(family) {
            style.underline_y = lines.underline_y;
            style.strike_y = lines.strike_y;
            style.line_thickness = lines.thickness;
        }
    }

    /// GPU `glyphmap` texture view (drop-in for `wall_cells.wgsl` group 0 binding 1).
    pub fn glyphmap_view(&self) -> &wgpu::TextureView {
        &self.glyphmap_view
    }

    /// GPU `curves` texture view (drop-in for `wall_cells.wgsl` group 0 binding 2).
    pub fn curves_view(&self) -> &wgpu::TextureView {
        &self.curves_view
    }

    /// GPU sparse `emoji` `D2Array` texture view (drop-in for `wall_cells.wgsl` group 0 binding 3).
    pub fn emoji_view(&self) -> &wgpu::TextureView {
        &self.emoji.view
    }

    /// Live layout and GPU memory footprint of the sparse emoji texture.
    pub fn emoji_layout(&self) -> SparseEmojiLayout {
        self.emoji.layout()
    }

    /// Underlying GPU `glyphmap` texture (`Rgba32Uint`, 1024 texels wide).
    pub fn glyphmap_texture(&self) -> &wgpu::Texture {
        &self.glyphmap_tex
    }

    /// Underlying GPU `curves` texture (`Rgba32Uint`, 1024 texels wide).
    pub fn curves_texture(&self) -> &wgpu::Texture {
        &self.curves_tex
    }

    /// Underlying GPU sparse `emoji` texture (`Rgba8UnormSrgb` `D2Array`).
    pub fn emoji_texture(&self) -> &wgpu::Texture {
        &self.emoji.texture
    }

    /// Underlying sparse emoji source (archive lookup and sparse-to-sheet index mapping).
    pub fn emoji_source(&self) -> &SparseEmojiSource {
        &self.emoji.source
    }

    /// Upload any Slug texels and newly allocated sparse emoji cells written since the last
    /// call to `sync`, recreating any GPU texture that grew. On an unchanged frame, issues
    /// zero GPU writes and returns [`GlyphSyncStats::default()`].
    pub fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> GlyphSyncStats {
        let dirty = self.atlas.take_dirty();
        let mut stats = GlyphSyncStats::default();

        if dirty.curves.is_dirty() {
            sync_one_texture(
                device,
                queue,
                "wall slug curves",
                &dirty.curves,
                self.atlas.curve_texture(),
                &mut self.curves_tex,
                &mut self.curves_view,
                &mut stats,
            );
        }
        if dirty.glyphmap.is_dirty() {
            sync_one_texture(
                device,
                queue,
                "wall slug glyphmap",
                &dirty.glyphmap,
                self.atlas.glyphmap_texture(),
                &mut self.glyphmap_tex,
                &mut self.glyphmap_view,
                &mut stats,
            );
        }
        self.emoji.sync(device, queue, &mut stats);

        stats
    }
}

#[cfg(feature = "wgpu")]
fn create_uint_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    (tex, view)
}

#[cfg(feature = "wgpu")]
fn create_and_upload_full(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    backing: TextureBacking<'_>,
) -> (wgpu::Texture, wgpu::TextureView) {
    let (tex, view) = create_uint_texture(device, label, backing.width, backing.height);
    upload_full_texture(queue, &tex, backing);
    (tex, view)
}

#[cfg(feature = "wgpu")]
fn upload_full_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    backing: TextureBacking<'_>,
) {
    assert_eq!(
        backing.data.len(),
        (backing.width as usize) * (backing.height as usize) * (U32S_PER_TEXEL as usize),
        "backing buffer length mismatch"
    );
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(backing.data),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(backing.width * BYTES_PER_TEXEL),
            rows_per_image: Some(backing.height),
        },
        wgpu::Extent3d {
            width: backing.width,
            height: backing.height,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(feature = "wgpu")]
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper syncing one Rgba32Uint texture from FontAtlas dirty state"
)]
fn sync_one_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    dirty: &TextureDirty,
    backing: TextureBacking<'_>,
    texture: &mut wgpu::Texture,
    view: &mut wgpu::TextureView,
    stats: &mut GlyphSyncStats,
) {
    if dirty.grew {
        let (new_tex, new_view) = create_and_upload_full(device, queue, label, backing);
        *texture = new_tex;
        *view = new_view;
        let texels = u64::from(backing.width) * u64::from(backing.height);
        stats.rebound = true;
        stats.write_calls += 1;
        stats.texels_uploaded += texels;
        stats.bytes_uploaded += texels * u64::from(BYTES_PER_TEXEL);
        return;
    }

    for rect in dirty.rects() {
        // texel_range_to_rects yields height == 1 for partial rows and full width for multi-row spans,
        // so every rect maps to one contiguous slice of `backing.data`.
        assert!(
            rect.height == 1 || (rect.x == 0 && rect.width == TEXTURE_WIDTH),
            "multi-row upload rect must be full-width"
        );
        let start_texel = (rect.y as usize) * (backing.width as usize) + (rect.x as usize);
        let texel_count = (rect.width as usize) * (rect.height as usize);
        let start_word = start_texel * (U32S_PER_TEXEL as usize);
        let end_word = start_word + texel_count * (U32S_PER_TEXEL as usize);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&backing.data[start_word..end_word]),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rect.width * BYTES_PER_TEXEL),
                rows_per_image: Some(rect.height),
            },
            wgpu::Extent3d {
                width: rect.width,
                height: rect.height,
                depth_or_array_layers: 1,
            },
        );
        stats.write_calls += 1;
        stats.texels_uploaded += texel_count as u64;
        stats.bytes_uploaded += (texel_count as u64) * u64::from(BYTES_PER_TEXEL);
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    /// Verifies `EmojiSheetArchive` agrees with the baked `Atlas` on every single
    /// codepoint and every single multi-codepoint sequence in `emoji-sheet.bin`.
    #[test]
    fn archive_matches_baked_atlas_codepoints_and_sequences() {
        let sheet_path = crate::default_emoji_sheet();
        let archive = EmojiSheetArchive::load(&sheet_path);
        let baked = crate::atlas::Atlas::load(&sheet_path);

        assert_eq!(archive.cell_size(), (baked.emoji.sheet.cell_w, baked.emoji.sheet.cell_h));
        assert_eq!(archive.cell_count(), baked.emoji.sheet.cells.len());
        assert_eq!(archive.mip_levels(), baked.emoji.mip_levels);

        // Every codepoint in EmojiSheet matches.
        for &(cp, gid) in &baked.emoji.sheet.codepoints {
            let expected = baked
                .emoji
                .sheet
                .cells
                .binary_search_by_key(&gid, |c| c.glyph_id)
                .ok()
                .map(|i| i as u32);
            assert_eq!(
                archive.sheet_cell_for_single(cp),
                expected,
                "single-codepoint mismatch for U+{cp:04X}"
            );
        }

        // Every sequence in the baked trie matches.
        let stride = 2 + baked.trie.seq_max as usize;
        let seq_count = baked.trie.sequences.len() / stride;
        assert_eq!(seq_count, baked.emoji.sheet.sequence_count as usize);
        for i in 0..seq_count {
            let o = i * stride;
            let slot = baked.trie.sequences[o];
            let len = baked.trie.sequences[o + 1] as usize;
            let cps = &baked.trie.sequences[o + 2..o + 2 + len];
            let expected = baked.trie.emoji_cell[slot as usize];
            assert_eq!(
                archive.sheet_cell_for_sequence(cps),
                expected,
                "sequence mismatch at index {i} ({cps:X?})"
            );
        }
    }
}

#[cfg(all(test, feature = "wgpu"))]
mod tests {
    use super::*;
    use glyph_font::{
        BLANK_SLOT, DEFAULT_FONT_FAMILY, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE,
        MONASPACE_ARGON_FAMILY,
    };
    use term_cells::{attrs, rgb, CLUSTER_BASE};

    /// Read back an entire `Rgba32Uint` 2D texture from `device` into a flat `Vec<u32>`
    /// matching `TextureBacking::data` layout (`width * height * 4`).
    fn readback_uint_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Vec<u32> {
        let size = texture.size();
        let width = size.width;
        let height = size.height;
        let unpadded_bpr = width * BYTES_PER_TEXEL;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyphs test uint readback"),
            size: u64::from(padded_bpr * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("glyphs test uint copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        glyph_field::map_read(
            device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "glyphs test uint readback",
        );
        let mapped = slice.get_mapped_range().expect("uint readback mapped");
        let mut out = Vec::with_capacity((width * height * U32S_PER_TEXEL) as usize);
        for row in 0..height {
            let start = (row * padded_bpr) as usize;
            let row_bytes = &mapped[start..start + unpadded_bpr as usize];
            out.extend_from_slice(bytemuck::cast_slice::<u8, u32>(row_bytes));
        }
        drop(mapped);
        readback.unmap();
        out
    }

    /// Read back a single cell sub-rectangle `(col, row, layer)` at `mip` from an RGBA8 texture.
    #[expect(
        clippy::too_many_arguments,
        reason = "test readback helper addressing one cell in a 2D array mip level"
    )]
    fn readback_emoji_cell(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        cell_w: u32,
        cell_h: u32,
        col: u32,
        row: u32,
        layer: u32,
        mip: u32,
    ) -> Vec<u8> {
        let mw = cell_w >> mip;
        let mh = cell_h >> mip;
        let unpadded_bpr = mw * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyphs test emoji readback"),
            size: u64::from(padded_bpr * mh),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("glyphs test emoji copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: mip,
                origin: wgpu::Origin3d {
                    x: col * mw,
                    y: row * mh,
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(mh),
                },
            },
            wgpu::Extent3d {
                width: mw,
                height: mh,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        glyph_field::map_read(
            device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "glyphs test emoji readback",
        );
        let mapped = slice.get_mapped_range().expect("emoji readback mapped");
        let mut out = Vec::with_capacity((unpadded_bpr * mh) as usize);
        for r in 0..mh {
            let start = (r * padded_bpr) as usize;
            out.extend_from_slice(&mapped[start..start + unpadded_bpr as usize]);
        }
        drop(mapped);
        readback.unmap();
        out
    }

    fn assert_gpu_matches_atlas(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyphs: &GlyphSource,
    ) {
        let gpu_map = readback_uint_texture(device, queue, glyphs.glyphmap_texture());
        let cpu_map = glyphs.atlas().glyphmap_texture();
        assert_eq!(
            gpu_map.len(),
            cpu_map.data.len(),
            "glyphmap GPU length != CPU length"
        );
        assert_eq!(gpu_map, cpu_map.data, "glyphmap GPU texels != FontAtlas");

        let gpu_curves = readback_uint_texture(device, queue, glyphs.curves_texture());
        let cpu_curves = glyphs.atlas().curve_texture();
        assert_eq!(
            gpu_curves.len(),
            cpu_curves.data.len(),
            "curves GPU length != CPU length"
        );
        assert_eq!(gpu_curves, cpu_curves.data, "curves GPU texels != FontAtlas");
    }

    fn assert_emoji_cell_matches_archive(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyphs: &GlyphSource,
        sparse_idx: u32,
    ) {
        let layout = glyphs.emoji_layout();
        let per_layer = layout.cols * layout.rows;
        let layer = sparse_idx / per_layer;
        let rem = sparse_idx % per_layer;
        let col = rem % layout.cols;
        let row = rem / layout.cols;
        let sheet_cell = glyphs
            .emoji_source()
            .sheet_cell_of(sparse_idx)
            .expect("allocated sparse cell");
        let expected_mips = glyphs.emoji_source().archive().decode_cell_mips(sheet_cell);
        for (mip, (_mw, _mh, expected_px)) in expected_mips.iter().enumerate() {
            let got = readback_emoji_cell(
                device,
                queue,
                glyphs.emoji_texture(),
                layout.cell_w,
                layout.cell_h,
                col,
                row,
                layer,
                mip as u32,
            );
            assert_eq!(
                got, *expected_px,
                "sparse emoji cell {sparse_idx} (sheet {sheet_cell}) mip {mip} mismatch"
            );
        }
    }

    #[test]
    fn gpu_screenful_upload_matches_font_atlas_and_idle_frame_uploads_nothing() {
        let ctx = crate::gpu::test_device::context();
        let mut glyphs = GlyphSource::new(&ctx.device, &ctx.queue, &crate::default_emoji_sheet());

        // Starts with a 4-byte placeholder emoji texture and zero decoded emoji.
        assert_eq!(glyphs.emoji_layout().allocated_cells, 0);
        assert_eq!(glyphs.emoji_layout().gpu_bytes, 4);

        // Default family is Monaspace Neon; Cousine and Argon are selectable with distinct metrics.
        let neon_id = glyphs.family_id(MONASPACE_NEON_FAMILY).expect("neon");
        let cousine_id = glyphs.family_id(DEFAULT_FONT_FAMILY).expect("cousine");
        let argon_id = glyphs.family_id(MONASPACE_ARGON_FAMILY).expect("argon");
        assert_eq!(glyphs.family(), neon_id);
        let neon_metrics = glyphs.cell_metrics();
        let cousine_metrics = glyphs.cell_metrics_for(cousine_id);
        assert_ne!(neon_metrics.cell_width, cousine_metrics.cell_width);
        assert_eq!(
            cousine_metrics,
            CellMetrics::from_primary(crate::atlas::default_trie().metrics)
        );

        let mut style = WallStyle::default();
        glyphs.apply_line_metrics(&mut style);
        let neon_lines = glyphs.line_metrics().expect("neon line metrics");
        assert_eq!(style.underline_y, neon_lines.underline_y);
        assert_eq!(style.strike_y, neon_lines.strike_y);
        assert_eq!(style.line_thickness, neon_lines.thickness);

        // Fresh source starts clean (0 uploads).
        assert_eq!(
            glyphs.sync(&ctx.device, &ctx.queue),
            GlyphSyncStats::default()
        );

        // Build a screenful of mixed cells: ASCII, bold, italic, bold-italic, box drawing,
        // accented NFC + composite clusters, single emoji, ZWJ emoji sequence, VS16/VS15, blank.
        let mut screen = Screen::new(16, 3);
        let make = |content: u32, cell_attrs: u16, width: u8| Cell {
            content,
            fg: rgb(0xe0, 0xe0, 0xe0),
            bg: rgb(0x10, 0x14, 0x1c),
            ul: 0,
            attrs: cell_attrs,
            width,
            _pad: 0,
        };
        screen.clusters.push("e\u{0301}".to_owned()); // CLUSTER_BASE + 0 (NFC é)
        screen.clusters.push("x\u{0301}".to_owned()); // CLUSTER_BASE + 1 (composite x́)
        screen.clusters.push("q\u{0308}\u{0304}".to_owned()); // CLUSTER_BASE + 2 (stacked q̈̄)
        screen.clusters.push("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_owned()); // +3 ZWJ
        screen.clusters.push("\u{2764}\u{FE0F}".to_owned()); // +4 VS16 colour heart
        screen.clusters.push("\u{2764}\u{FE0E}".to_owned()); // +5 VS15 text heart

        for (i, ch) in "Hello, World!09@".chars().enumerate() {
            screen.row_mut(0)[i] = make(ch as u32, 0, 1);
        }
        screen.row_mut(1)[0] = make('B' as u32, attrs::BOLD, 1);
        screen.row_mut(1)[1] = make('I' as u32, attrs::ITALIC, 1);
        screen.row_mut(1)[2] = make('Z' as u32, attrs::BOLD | attrs::ITALIC, 1);
        screen.row_mut(1)[3] = make('╭' as u32, 0, 1);
        screen.row_mut(1)[4] = make('─' as u32, attrs::BOLD, 1);
        screen.row_mut(1)[5] = make('╬' as u32, 0, 1);
        screen.row_mut(1)[6] = make('▒' as u32, 0, 1);
        screen.row_mut(1)[7] = make(CLUSTER_BASE, 0, 1);
        screen.row_mut(1)[8] = make(CLUSTER_BASE + 1, attrs::BOLD, 1);
        screen.row_mut(1)[9] = make(CLUSTER_BASE + 2, 0, 1);
        screen.row_mut(1)[10] = make('🚀' as u32, 0, 2);
        screen.row_mut(1)[11] = make(0, 0, 0);
        screen.row_mut(1)[12] = make(CLUSTER_BASE + 3, 0, 2);
        screen.row_mut(1)[13] = make(0, 0, 0);
        screen.row_mut(1)[14] = make(CLUSTER_BASE + 4, 0, 2);
        screen.row_mut(1)[15] = make(0, 0, 0);
        screen.row_mut(2)[0] = make(CLUSTER_BASE + 5, 0, 1);
        screen.row_mut(2)[1] = make(' ' as u32, 0, 1);
        screen.row_mut(2)[2] = Cell::BLANK;

        let mut resolved = Vec::new();
        for r in 0..screen.rows {
            for cell in screen.row(r) {
                resolved.push(glyphs.resolve_cell(&screen, cell));
            }
        }

        // Spot-check resolved modes, spans, and dense sparse emoji indices (0, 1, 2).
        let r1 = |col: usize| resolved[16 + col];
        assert_eq!(r1(0).span_cols, 1);
        assert_eq!(glyphs.atlas().map_entry(r1(0).slot)[2], GLYPH_MODE_OUTLINE);
        assert_eq!(glyphs.atlas().map_entry(r1(3).slot)[2], GLYPH_MODE_SPRITE);
        assert_eq!(glyphs.atlas().map_entry(r1(6).slot)[2], GLYPH_MODE_SPRITE);
        assert_eq!(glyphs.atlas().map_entry(r1(6).slot)[3], 128);
        assert!(glyphs.atlas().is_composite_slot(r1(8).slot));
        assert_eq!(r1(10).span_cols, 2);
        assert_eq!(glyphs.atlas().map_entry(r1(10).slot), [0, 0, GLYPH_MODE_BITMAP, 0]);
        assert_eq!(r1(11), ResolvedCell { slot: BLANK_SLOT, span_cols: 0 });
        assert_eq!(r1(12).span_cols, 2);
        assert_eq!(glyphs.atlas().map_entry(r1(12).slot), [0, 0, GLYPH_MODE_BITMAP, 1]);
        assert_eq!(glyphs.atlas().map_entry(r1(14).slot), [0, 0, GLYPH_MODE_BITMAP, 2]);
        assert_eq!(glyphs.atlas().map_entry(resolved[32].slot)[2], GLYPH_MODE_OUTLINE);
        assert_eq!(resolved[33], ResolvedCell { slot: BLANK_SLOT, span_cols: 1 });
        assert_eq!(resolved[34], ResolvedCell { slot: BLANK_SLOT, span_cols: 1 });

        // First sync: allocates 8x2 sparse emoji texture (rebound = true), uploads 3 emoji cells.
        let s1 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s1.rebound, "first emoji allocation grows 1x1 placeholder -> 8x2");
        assert_eq!(s1.emoji_cells_uploaded, 3);
        assert_eq!(glyphs.emoji_layout().allocated_cells, 3);
        assert!(
            glyphs.emoji_layout().gpu_bytes < 2 * 1024 * 1024,
            "sparse emoji GPU bytes should stay < 2 MiB for 3 cells, got {}",
            glyphs.emoji_layout().gpu_bytes
        );
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);
        for sparse_idx in 0..3 {
            assert_emoji_cell_matches_archive(&ctx.device, &ctx.queue, &glyphs, sparse_idx);
        }

        // Unchanged second frame: uploads nothing.
        let s2 = glyphs.sync(&ctx.device, &ctx.queue);
        assert_eq!(s2, GlyphSyncStats::default());

        // Switch family to Argon, resolve one new glyph (no new emoji), verify incremental upload.
        assert_eq!(glyphs.set_family(MONASPACE_ARGON_FAMILY), Some(argon_id));
        let omega = make('Ω' as u32, attrs::BOLD, 1);
        let r_omega = glyphs.resolve_cell(&screen, &omega);
        assert_ne!(r_omega.slot, BLANK_SLOT);
        let s3 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(!s3.rebound);
        assert_eq!(s3.emoji_cells_uploaded, 0);
        assert!(s3.write_calls >= 2 && s3.texels_uploaded < 512);
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);
    }

    #[test]
    fn gpu_texture_growth_recreates_textures_and_matches_font_atlas() {
        let ctx = crate::gpu::test_device::context();
        let archive = Arc::new(EmojiSheetArchive::load(&crate::default_emoji_sheet()));

        // Start at 1 curve row (512 curves) and 1 map row (1024 slots) so both double on GPU.
        let mut glyphs = GlyphSource::load_with_capacities(
            &ctx.device,
            &ctx.queue,
            &default_fonts_dir(),
            archive,
            1,
            1,
            SPARSE_EMOJI_COLS,
            SPARSE_EMOJI_INIT_ROWS,
            SPARSE_EMOJI_MAX_ROWS,
        )
        .expect("load small-capacity GlyphSource");
        assert_eq!(glyphs.curves_texture().height(), 1);
        assert_eq!(glyphs.glyphmap_texture().height(), 1);

        let screen = Screen::new(1, 1);
        let cell_of = |cp: u32, cell_attrs: u16| Cell {
            content: cp,
            fg: rgb(0xff, 0xff, 0xff),
            bg: 0,
            ul: 0,
            attrs: cell_attrs,
            width: 1,
            _pad: 0,
        };

        // Initial small batch: no growth yet; verify GPU readback.
        for ch in "AtlasGrowth!".chars() {
            glyphs.resolve_cell(&screen, &cell_of(ch as u32, 0));
        }
        let s1 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(!s1.rebound);
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);

        // Push > 1,100 distinct slots across all 3 families and all 4 faces so BOTH
        // curves and glyphmap textures double past their initial 1-row allocation.
        let families = [
            glyphs.family_id(DEFAULT_FONT_FAMILY).unwrap(),
            glyphs.family_id(MONASPACE_NEON_FAMILY).unwrap(),
            glyphs.family_id(MONASPACE_ARGON_FAMILY).unwrap(),
        ];
        let attr_faces = [0, attrs::BOLD, attrs::ITALIC, attrs::BOLD | attrs::ITALIC];
        for fam in families {
            for &a in &attr_faces {
                for cp in 0x21u32..=0x7Eu32 {
                    glyphs.resolve_cell_in(fam, &screen, &cell_of(cp, a));
                }
                for cp in 0x00A1u32..=0x0120u32 {
                    glyphs.resolve_cell_in(fam, &screen, &cell_of(cp, a));
                }
            }
        }

        let s2 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s2.rebound, "growth must flag rebound so caller rebuilds shared_bg");
        assert!(
            glyphs.curves_texture().height() > 1,
            "curves texture must have grown past 1 row (got {})",
            glyphs.curves_texture().height()
        );
        assert!(
            glyphs.glyphmap_texture().height() > 1,
            "glyphmap texture must have grown past 1 row (got {})",
            glyphs.glyphmap_texture().height()
        );
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);

        // Subsequent unchanged frame uploads nothing.
        assert_eq!(
            glyphs.sync(&ctx.device, &ctx.queue),
            GlyphSyncStats::default()
        );
    }

    #[test]
    fn gpu_sparse_emoji_growth_preserves_existing_cells_across_row_and_layer_growth() {
        let ctx = crate::gpu::test_device::context();
        let archive = Arc::new(EmojiSheetArchive::load(&crate::default_emoji_sheet()));

        // Tiny emoji grid: 4 cols, 1 initial row (4 cells), max 2 rows/layer (8 cells/layer),
        // so 3 -> 6 -> 11 emoji exercise placeholder -> 4x1 -> 4x2 (row growth) -> 4x2x2 (layer growth).
        let mut glyphs = GlyphSource::load_with_capacities(
            &ctx.device,
            &ctx.queue,
            &default_fonts_dir(),
            archive,
            CURVE_INITIAL_ROWS,
            MAP_INITIAL_ROWS,
            4,
            1,
            2,
        )
        .expect("load tiny sparse emoji GlyphSource");

        let screen = Screen::new(1, 1);
        let emoji_cell = |ch: char| Cell {
            content: ch as u32,
            fg: rgb(0xff, 0xff, 0xff),
            bg: 0,
            ul: 0,
            attrs: 0,
            width: 2,
            _pad: 0,
        };

        // Frame 1: 3 emoji -> allocates 4x1x1.
        for ch in ['🚀', '🐀', '🔥'] {
            glyphs.resolve_cell(&screen, &emoji_cell(ch));
        }
        let s1 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s1.rebound);
        assert_eq!(s1.emoji_cells_uploaded, 3);
        let l1 = glyphs.emoji_layout();
        assert_eq!((l1.cols, l1.rows, l1.layers, l1.allocated_cells), (4, 1, 1, 3));

        // Frame 2: 3 more emoji (total 6) -> grows rows 1 -> 2 (4x2x1), preserving cells 0..3.
        for ch in ['🎉', '🦀', '🌈'] {
            glyphs.resolve_cell(&screen, &emoji_cell(ch));
        }
        let s2 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s2.rebound);
        assert_eq!(s2.emoji_cells_uploaded, 3);
        let l2 = glyphs.emoji_layout();
        assert_eq!((l2.cols, l2.rows, l2.layers, l2.allocated_cells), (4, 2, 1, 6));
        for idx in 0..6 {
            assert_emoji_cell_matches_archive(&ctx.device, &ctx.queue, &glyphs, idx);
        }

        // Frame 3: 5 more emoji (total 11) -> grows layers 1 -> 2 (4x2x2), preserving cells 0..6.
        for ch in ['🍕', '🎸', '🟥', '🟦', '🐙'] {
            glyphs.resolve_cell(&screen, &emoji_cell(ch));
        }
        let s3 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s3.rebound);
        assert_eq!(s3.emoji_cells_uploaded, 5);
        let l3 = glyphs.emoji_layout();
        assert_eq!((l3.cols, l3.rows, l3.layers, l3.allocated_cells), (4, 2, 2, 11));
        for idx in 0..11 {
            assert_emoji_cell_matches_archive(&ctx.device, &ctx.queue, &glyphs, idx);
        }

        // Frame 4: re-requesting existing emoji uploads nothing.
        for ch in ['🚀', '🐙'] {
            glyphs.resolve_cell(&screen, &emoji_cell(ch));
        }
        assert_eq!(
            glyphs.sync(&ctx.device, &ctx.queue),
            GlyphSyncStats::default()
        );
    }

    #[test]
    fn gpu_reload_family_swaps_atlas_without_leak_scales_line_height_and_keeps_old_on_missing() {
        let ctx = crate::gpu::test_device::context();
        let mut glyphs = GlyphSource::new(&ctx.device, &ctx.queue, &crate::default_emoji_sheet());
        let base_metrics = glyphs.cell_metrics();
        assert_eq!(glyphs.active_family_name(), MONASPACE_NEON_FAMILY);

        // Encode some glyphs in the initial family so the atlas is non-empty.
        let screen = Screen::new(4, 1);
        for ch in ['A', 'g', '─', '🚀'] {
            glyphs.resolve_cell(
                &screen,
                &Cell {
                    content: ch as u32,
                    fg: rgb(0xff, 0xff, 0xff),
                    bg: 0,
                    ul: 0,
                    attrs: 0,
                    width: if ch == '🚀' { 2 } else { 1 },
                    _pad: 0,
                },
            );
        }
        let _ = glyphs.sync(&ctx.device, &ctx.queue);
        let initial_slots = glyphs.atlas().slot_count();
        assert!(initial_slots >= 4);

        // No-op reload when family & line height match.
        assert!(!glyphs
            .reload_family(&ctx.device, &ctx.queue, MONASPACE_NEON_FAMILY, 1.0)
            .unwrap());

        // Switch across families (including a macOS system font when present) + line height.
        let targets: &[(&str, f64)] = if cfg!(target_os = "macos") {
            &[("Menlo", 1.0), ("monaspace-argon", 1.35), ("cousine", 1.0)]
        } else {
            &[("monaspace-argon", 1.35), ("cousine", 1.0)]
        };
        for &(fam, lh) in targets {
            let changed = glyphs
                .reload_family(&ctx.device, &ctx.queue, fam, lh)
                .unwrap_or_else(|e| panic!("reload {fam} ({lh}x) failed: {e}"));
            assert!(changed, "expected reload for {fam} ({lh}x)");
            // Fresh atlas starts clean (only slot 0 allocated before resolving cells) — no leak.
            assert_eq!(glyphs.atlas().slot_count(), 1);
            assert!((glyphs.line_height_scale() - lh).abs() < 1e-6);
            let m = glyphs.cell_metrics();
            assert!(
                (m.cell_height - base_metrics.cell_height * lh as f32).abs() < 2e-3,
                "cell_height {} should scale by {lh}x from {} (within binary-snap tolerance)",
                m.cell_height,
                base_metrics.cell_height
            );
            // Resolve ASCII + box sprite + emoji in the newly loaded family and verify GPU matches.
            let g_a = glyphs.resolve_cell(
                &screen,
                &Cell {
                    content: 'A' as u32,
                    fg: rgb(0xff, 0xff, 0xff),
                    bg: 0,
                    ul: 0,
                    attrs: attrs::BOLD,
                    width: 1,
                    _pad: 0,
                },
            );
            let g_box = glyphs.resolve_cell(
                &screen,
                &Cell {
                    content: '╭' as u32,
                    fg: rgb(0xff, 0xff, 0xff),
                    bg: 0,
                    ul: 0,
                    attrs: 0,
                    width: 1,
                    _pad: 0,
                },
            );
            assert!(g_a.slot > 0 && g_box.slot > 0);
            let _ = glyphs.sync(&ctx.device, &ctx.queue);
            assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);
        }

        // Missing family returns FamilyNotFound and leaves active family, metrics, and atlas intact.
        let before_name = glyphs.active_family_name().to_string();
        let before_metrics = glyphs.cell_metrics();
        let before_slots = glyphs.atlas().slot_count();
        let err = glyphs
            .reload_family(&ctx.device, &ctx.queue, "NoSuchFontFamilyXYZ", 1.0)
            .unwrap_err();
        assert!(
            matches!(err, glyph_font::FontError::FamilyNotFound(_)),
            "expected FamilyNotFound, got {err:?}"
        );
        assert_eq!(glyphs.active_family_name(), before_name);
        assert_eq!(glyphs.cell_metrics().cell_width, before_metrics.cell_width);
        assert_eq!(
            glyphs.cell_metrics().cell_height,
            before_metrics.cell_height
        );
        assert_eq!(glyphs.atlas().slot_count(), before_slots);
    }
}
