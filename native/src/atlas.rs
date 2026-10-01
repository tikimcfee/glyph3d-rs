//! Stage C — glyph atlas loader.
//!
//! Parses the four Stage B export files in `assets/atlas/` per FORMAT.md:
//!   curves.bin     (G3CV) — Slug curve texture payload, 1024x161 RGBA32Uint
//!   glyphmap.bin   (G3GM) — glyph-map texture payload,   1024xN   RGBA32Uint (N from the header)
//!   glyphs.bin     (G3GL) — per-slot metrics (parsed for validation; not uploaded)
//!   codepoints.bin (G3CP) — codepoint → slot two-level trie (CPU side)
//!
//! curves/glyphmap are uploaded verbatim as `Rgba32Uint` textures and sampled
//! in WGSL with `textureLoad` (integer textures: no filtering, no sRGB).
//! The trie stays on the CPU — text staging (text.rs) does byte→slot decode.
//!
//! Plus the colour-emoji sheet (2026-09-10, `out/EMOJI.md`):
//!   emoji-sheet.bin (G3ES) — the vendored font's PNG bytes verbatim, one cell
//!                            per bitmap glyph, keyed by the font's glyph id
//! [`EmojiSheet`] parses it; [`EmojiTexture`] decodes every cell into an
//! `Rgba8UnormSrgb` 2D-array texture with box-filtered mips and uploads it.
//! The sheet is the ONE thing here that is decoded rather than copied, so the
//! decode is where platform differences could enter — which is why the
//! decoder is the `image` crate's PNG path and nothing else, the filter is
//! integer arithmetic, and a unit test pins both on a known 2×2 image.

use std::path::{Path, PathBuf};

use crate::gpu::GpuContext;

/// Both Slug textures are row-major, 1024 texels wide (slug-constants.js
/// TEXTURE_WIDTH). Texel `i` lives at `(i % 1024, i / 1024)`.
pub const ATLAS_TEX_WIDTH: u32 = 1024;

/// Trie entry flags (FORMAT.md, codepoints.bin). FLAG_BITMAP (2) and
/// FLAG_BLANK (4) exist in the format and are not read here: the renderer
/// learns "bitmap" from the glyph map's mode lane (the shader's branch), and
/// staging drops only MISSING — a bitmap slot is staged like any glyph since
/// 2026-09-10. Add a constant back when a reader lands.
pub const FLAG_MISSING: u32 = 1;

/// Primary-font layout metrics (glyphs.bin header).
#[derive(Clone, Copy, Debug)]
pub struct PrimaryMetrics {
    pub upem: u32,
    pub advance_fu: u32,    // 1229 — monospace cell advance
    pub em_height_fu: u32,  // 2320 — ascender - descender
}

/// One decoded trie entry.
#[derive(Clone, Copy, Debug)]
pub struct TrieEntry {
    pub glyph_id: u32,
    pub advance_fu: i32,
    /// Constant = primaryEmHeightFu for every entry today; kept for Stage E.
    pub height_fu: i32,
    pub flags: u32,
}

/// Stage E1 — the CPU-side codepoint→slot trie + primary metrics, loadable
/// WITHOUT a GPU context (the engine cross-check in main.rs has no device).
/// `Atlas` owns one of these for the renderer path; both parse the same files.
pub struct TrieTable {
    pub metrics: PrimaryMetrics,
    block_shift: u32,
    block_index: Vec<u32>,
    blocks: Vec<u32>,
    entry_stride: u32,
    /// Informational header fields (mapped codepoint count).
    pub mapped_count: u32,
    pub slot_count: u32,
    /// Per slot, the emoji sheet cell a bitmap slot draws from (glyphs.bin
    /// slot record word 3), or `None` for outline/blank slots and for the
    /// web-era bitmap slots the font has no cell for (NO_CELL).
    pub emoji_cell: Vec<Option<u32>>,
    /// v2: the cluster head's advance (the bitmap 2x cell), fu. On a v1 file
    /// this is 2 x the primary advance — the same value by the export rule.
    // Read by the sequence pass: ResolveGlyph::cluster_table hands it to the
    // fold (fold.rs's resolve_clusters) and text.rs reads it for the head cell.
    pub bitmap_advance_fu: i32,
    /// v2: the sequence section's raw words — sequenceCount x (2 + seq_max)
    /// of [slot, len, cps..], sorted by the codepoint sequence (prefix-
    /// lexicographic, length tiebreak). Empty on v1 ("no sequences" — the
    /// leader behavior).
    // Read by the sequence pass: sequence_lookup binary-searches it per probe.
    pub sequences: Vec<u32>,
    /// The probe window's length cap (the section stride is 2 + seq_max).
    pub seq_max: u32,
    /// v2: the G3CC class table verbatim (its own header included). Empty on v1.
    // Carried for the general UAX #29 phase: the landed sequence pass reads no
    // classes by design (glyph_cluster.mojo says why), so today only
    // trie_v2_tests exercise it. class_of is its reader.
    #[allow(dead_code)]
    pub classes: Vec<u32>,
    /// The table's first members as a set — the probe's cheap rejection. Built
    /// once at load from the sequence section itself.
    // starts_a_sequence reads it per probe.
    seq_first: std::collections::HashSet<u32>,
}

impl TrieTable {
    /// Parse glyphs.bin (primary metrics) + codepoints.bin (the trie) from an
    /// atlas directory. Pure CPU — no textures.
    pub fn load(dir: &Path) -> Self {
        let gl = read_words(&dir.join("glyphs.bin"));
        check_magic(&gl, "G3GL", &dir.join("glyphs.bin"));
        let metrics = PrimaryMetrics {
            upem: gl[5],
            advance_fu: gl[6],
            em_height_fu: gl[7],
        };
        let slot_count = gl[4];
        // Slot records follow the font records (FORMAT.md glyphs.bin): read
        // each one's emojiCell, which the backdrop tint needs to know what a
        // bitmap slot's pixels look like.
        let (font_count, font_rec_words, slot_rec_words) = (gl[3] as usize, gl[8] as usize / 4, gl[9] as usize / 4);
        let slots_at = 11 + font_count * font_rec_words;
        const SLOT_FLAG_BITMAP: u32 = 1;
        const NO_CELL: u32 = 0xFFFF_FFFF;
        let emoji_cell: Vec<Option<u32>> = (0..slot_count as usize)
            .map(|s| {
                let r = &gl[slots_at + s * slot_rec_words..][..slot_rec_words];
                (r[2] & SLOT_FLAG_BITMAP != 0 && r[3] != NO_CELL).then_some(r[3])
            })
            .collect();

        let cp = read_words(&dir.join("codepoints.bin"));
        check_magic(&cp, "G3CP", &dir.join("codepoints.bin"));
        // v1: 44 B header, no sections. v2 (the sequence pass): 68 B header
        // with the section descriptors at words 11..16, and the sequence +
        // class sections appended after the blocks.
        let version = cp[1];
        assert!(version == 1 || version == 2, "codepoints.bin: version {version} (expected 1 or 2)");
        let header_words = if version == 2 { 17 } else { 11 };
        let block_shift = cp[3];
        let block_index_len = cp[4] as usize;
        let block_count = cp[5] as usize;
        let entry_stride = cp[6];
        let mapped_count = cp[7];
        let (mut bitmap_advance_fu, mut seq_max, mut sequences, mut classes) =
            (2 * metrics.advance_fu as i32, 0, Vec::new(), Vec::new());
        if version == 2 {
            bitmap_advance_fu = cp[11] as i32;
            let seq_count = cp[12] as usize;
            seq_max = cp[13];
            let seq_off = cp[14] as usize;
            let class_off = cp[15] as usize;
            let class_words = cp[16] as usize;
            let seq_words = seq_count * (2 + seq_max as usize);
            assert_eq!(seq_off, header_words + block_index_len + block_count * 256 * entry_stride as usize,
                "codepoints.bin: sequence section is not appended after the blocks");
            sequences = cp[seq_off..seq_off + seq_words].to_vec();
            classes = cp[class_off..class_off + class_words].to_vec();
            assert_eq!(classes[0], u32::from_le_bytes(*b"G3CC"), "codepoints.bin: class section is not a G3CC table");
        }
        let block_index = cp[header_words..header_words + block_index_len].to_vec();
        let block_words = block_count * (1usize << block_shift) * entry_stride as usize;
        let blocks = cp[header_words + block_index_len..header_words + block_index_len + block_words].to_vec();
        // The probe's rejection set: the sequence table's own first members.
        let seq_first = sequences
            .chunks_exact(2 + seq_max as usize)
            .map(|e| e[2])
            .collect();
        let t = Self {
            metrics,
            block_shift,
            block_index,
            blocks,
            entry_stride,
            mapped_count,
            slot_count,
            emoji_cell,
            bitmap_advance_fu,
            sequences,
            seq_max,
            classes,
            seq_first,
        };
        // Sanity: 'A' must resolve to slot 34 / advance 1229 (FORMAT.md worked example).
        let a = t.lookup(0x41);
        assert_eq!((a.glyph_id, a.advance_fu), (34, 1229), "trie sanity check failed for 'A'");
        t
    }

    /// The decode KERNEL's tables, pre-converted to world units: the block
    /// index, per-entry measures [ADVANCE, HEIGHT], per-entry identity +
    /// bitfield [GLYPH_ID, FLAGS], and the block shift. The fu→world
    /// conversion runs through the SAME f64-rounding function the CPU
    /// resolve uses, computed once here — so the device's advance bits are
    /// the CPU's advance bits, and no device-side division (with fast-math
    /// questions attached) ever runs.
    pub fn device_tables(&self) -> (Vec<u32>, Vec<f32>, Vec<u32>, u32) {
        let em = self.metrics.em_height_fu;
        let stride = self.entry_stride as usize;
        let n = self.blocks.len() / stride;
        let mut measures = Vec::with_capacity(n * 2);
        let mut counts = Vec::with_capacity(n * 2);
        for e in 0..n {
            let o = e * stride;
            counts.push(self.blocks[o]);
            measures.push(crate::text::fu_to_world(self.blocks[o + 1] as i32, em));
            measures.push(crate::text::fu_to_world(self.blocks[o + 2] as i32, em));
            counts.push(self.blocks[o + 3]);
        }
        (self.block_index.clone(), measures, counts, self.block_shift)
    }

    /// Codepoint → trie entry (the two dependent loads of FORMAT.md).
    /// Resolve a codepoint. OUT-OF-RANGE values resolve through the shared
    /// missing block (storage block 0), matching `decode_and_resolve` in
    /// glyph_pipeline.mojo exactly — including which entry of that block is
    /// read (`cp & 0xFF`), which does not change the VALUE since every entry in
    /// the missing block is identical, but does keep the two implementations
    /// literally the same computation.
    ///
    /// The engine's decode is a LENIENT classifier that never validates
    /// continuation bytes, so lead bytes 0xF5-0xF7 (and 0xF4 with a continuation
    /// above 0x8F) produce codepoints past the last Unicode scalar. Guarding
    /// here rather than at one call site means every caller gets the contract.
    pub fn lookup(&self, cp: u32) -> TrieEntry {
        let block = if cp <= 0x10FFFF {
            self.block_index[(cp >> self.block_shift) as usize]
        } else {
            0
        };
        let e = ((block << self.block_shift) | (cp & 0xFF)) as usize * self.entry_stride as usize;
        TrieEntry {
            glyph_id: self.blocks[e],
            advance_fu: self.blocks[e + 1] as i32,
            height_fu: self.blocks[e + 2] as i32,
            flags: self.blocks[e + 3],
        }
    }

    /// Sequence → slot, binary search over the v2 sequence section. Entries
    /// are [slot, len, cps..] sorted by the codepoint sequence (elementwise,
    /// shorter-prefix-first) — the sheet's own table order, asserted at bake.
    /// The caller probes with the FE0F-normalized codepoints of a candidate
    /// cluster; None means no such sequence (the fallback is per-codepoint).
    pub fn sequence_lookup(&self, cps: &[u32]) -> Option<u32> {
        if self.sequences.is_empty() {
            return None;
        }
        let stride = 2 + self.seq_max as usize;
        let n = self.sequences.len() / stride;
        let entry = |i: usize| -> (u32, &[u32]) {
            let o = i * stride;
            let len = self.sequences[o + 1] as usize;
            (self.sequences[o], &self.sequences[o + 2..o + 2 + len])
        };
        // Ordering: compare the live cps elementwise; a strict prefix sorts
        // first (matches the sheet's table order — the writer asserts it).
        let cmp = |probe: &[u32], entry_cps: &[u32]| {
            for k in 0..probe.len().min(entry_cps.len()) {
                match probe[k].cmp(&entry_cps[k]) {
                    std::cmp::Ordering::Equal => {}
                    ord => return ord,
                }
            }
            probe.len().cmp(&entry_cps.len())
        };
        let mut lo = 0;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(cps, entry(mid).1) == std::cmp::Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n && cmp(cps, entry(lo).1) == std::cmp::Ordering::Equal {
            Some(entry(lo).0)
        } else {
            None
        }
    }

    /// Whether this codepoint starts any sequence in the v2 table — the head
    /// candidacy test the probe runs before paying for a lookup. The set is
    /// derived from the table's own first members at load, so it can never
    /// drift from it.
    pub fn starts_a_sequence(&self, cp: u32) -> bool {
        self.seq_first.contains(&cp)
    }

    /// Codepoint → class bits (the G3CC table embedded in the v2 class
    /// section; range-compressed, binary search by range start). 0 = Other.
    // The general UAX #29 phase's reader, carried with the table — the landed
    // sequence pass reads no classes by design, so tests exercise this today.
    #[allow(dead_code)]
    pub fn class_of(&self, cp: u32) -> u32 {
        if self.classes.is_empty() {
            return 0;
        }
        let w = &self.classes;
        let header_bytes = w[2] as usize / 4;
        let n = w[3] as usize;
        let mut lo = 0;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let s = w[header_bytes + mid * 3];
            if s <= cp {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return 0;
        }
        let (s, e, b) = (w[header_bytes + (lo - 1) * 3], w[header_bytes + (lo - 1) * 3 + 1], w[header_bytes + (lo - 1) * 3 + 2]);
        if cp >= s && cp <= e { b } else { 0 }
    }
}

pub struct Atlas {
    pub curves: wgpu::Texture,
    pub glyphmap: wgpu::Texture,
    pub metrics: PrimaryMetrics,
    /// The CPU-side codepoint→slot trie (Stage E1: shared with the engine
    /// cross-check via [`TrieTable`]).
    pub trie: TrieTable,
    /// The colour-emoji sheet, decoded and resident; the shader's mode-1
    /// branch samples it.
    pub emoji: EmojiTexture,
    /// Per SLOT, what a bitmap glyph's pixels average to — linear rgb,
    /// alpha-weighted, plus its mean alpha — for the far-LOD backdrop tint
    /// (`glyph_scene::seg_tint`), which otherwise only knows the instance's
    /// syntax colour, a colour an emoji does not display. `None` for every
    /// slot that is not a bitmap with a cell.
    pub slot_ink: Vec<Option<[f32; 4]>>,
}

// ── the colour-emoji sheet (G3ES) ─────────────────────────────────────────

const SHEET_HEADER_WORDS: usize = 40;
const SHEET_CELL_STRIDE: usize = 6;
/// Row pitch alignment wgpu requires of `write_texture`, in BYTES; the sheet
/// generator pads `layer_w` so every mip level's pitch is a multiple of it.
const UPLOAD_PITCH_ALIGN: u32 = 256;
/// The most mip levels the sheet gets. Level `k` averages 2^k × 2^k texels;
/// [`mip_levels_for`] caps `k` so no filter footprint crosses a cell edge —
/// a bled mip would paint a neighbour's colour into a cell's border. For the
/// baked 136×128 cell that is 4 levels (a 17×16-px cell at level 3); below
/// that size the LOD backdrop replaces the segment anyway (`LOD_MIN_PX`).
pub const EMOJI_MAX_MIP_LEVELS: u32 = 4;

/// Mip levels such that every level's 2^k footprint tiles the cell exactly:
/// the largest `k ≤ EMOJI_MAX_MIP_LEVELS` with `2^(k-1)` dividing both cell
/// dimensions. Pure, so the choice is unit-tested rather than trusted.
pub fn mip_levels_for(cell_w: u32, cell_h: u32) -> u32 {
    let mut k = 1;
    while k < EMOJI_MAX_MIP_LEVELS
        && cell_w.is_multiple_of(1 << k)
        && cell_h.is_multiple_of(1 << k)
    {
        k += 1;
    }
    k
}

/// One cell of the sheet: where it sits and where its PNG is.
#[derive(Clone, Copy, Debug)]
pub struct EmojiCell {
    pub glyph_id: u32,
    pub layer: u32,
    pub x: u32,
    pub y: u32,
    png_offset: u32,
    png_len: u32,
}

/// The parsed sheet: geometry, the cell and codepoint tables, and the PNG
/// blob the cells point into. Sequence and name tables are skipped over
/// (read by nothing yet; their counts are checked so the file's shape is).
pub struct EmojiSheet {
    pub cell_w: u32,
    pub cell_h: u32,
    pub cols: u32,
    pub rows_per_layer: u32,
    pub layers: u32,
    pub layer_w: u32,
    pub layer_h: u32,
    pub ppem: u32,
    pub bearing_y: i32,
    pub advance_px: u32,
    pub cells: Vec<EmojiCell>,
    /// Every cmap entry of the font, `(codepoint, glyph id)`, sorted. A glyph
    /// id with no cell means "known to the font, no bitmap" (ZWJ, tags).
    pub codepoints: Vec<(u32, u32)>,
    pub sequence_count: u32,
    pub font_sha256: [u32; 8],
    png: Vec<u8>,
}

impl EmojiSheet {
    pub fn load(path: &Path) -> Self {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        Self::parse(&bytes, &path.display().to_string())
    }

    /// Parse and assert the invariants the upload relies on. Fail-loud: a
    /// sheet that does not hold them is the wrong file, not a degraded one.
    pub fn parse(bytes: &[u8], what: &str) -> Self {
        assert!(bytes.len() >= SHEET_HEADER_WORDS * 4, "{what}: shorter than a G3ES header");
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(&bytes[0..4], b"G3ES", "{what}: bad magic");
        assert_eq!(word(1), 1, "{what}: unknown G3ES version");
        assert_eq!(word(2) as usize, SHEET_HEADER_WORDS * 4, "{what}: unexpected header size");
        let (cell_w, cell_h, n) = (word(3), word(4), word(5) as usize);
        let (cols, rows_per_layer, layers, layer_w, layer_h) = (word(6), word(7), word(8), word(9), word(10));
        let (ppem, bearing_y, advance_px) = (word(11), word(15) as i32, word(16));
        let (n_cp, n_seq, seq_max) = (word(21) as usize, word(22), word(23) as usize);
        let (png_start, png_len) = (word(24) as usize, word(25) as usize);
        assert!(cell_w > 0 && cell_h > 0 && layers > 0, "{what}: degenerate geometry");
        assert!(cols * cell_w <= layer_w && rows_per_layer * cell_h == layer_h, "{what}: layer geometry");
        assert!(
            (layer_w * 4).is_multiple_of(UPLOAD_PITCH_ALIGN),
            "{what}: layer width {layer_w} px gives a row pitch that is not {UPLOAD_PITCH_ALIGN}-byte aligned; \
             the generator pads it — this sheet was not made by gen_emoji_sheet.py"
        );
        assert!((cols * rows_per_layer * layers) as usize >= n, "{what}: layers cannot hold every cell");
        assert_eq!(png_start + png_len, bytes.len(), "{what}: PNG blob does not end the file");
        let mut font_sha256 = [0u32; 8];
        for (i, w) in font_sha256.iter_mut().enumerate() {
            *w = word(32 + i);
        }

        let mut at = SHEET_HEADER_WORDS;
        let mut cells = Vec::with_capacity(n);
        let mut last = None;
        for i in 0..n {
            let c = EmojiCell {
                glyph_id: word(at),
                layer: word(at + 1),
                x: word(at + 2),
                y: word(at + 3),
                png_offset: word(at + 4),
                png_len: word(at + 5),
            };
            at += SHEET_CELL_STRIDE;
            assert!(last.is_none_or(|g| c.glyph_id > g), "{what}: cell {i} glyph ids not ascending");
            last = Some(c.glyph_id);
            assert!(c.layer < layers && c.x + cell_w <= layer_w && c.y + cell_h <= layer_h, "{what}: cell {i} outside its layer");
            assert!((c.png_offset + c.png_len) as usize <= png_len, "{what}: cell {i} PNG range outside the blob");
            let p = &bytes[png_start + c.png_offset as usize..][..8.min(c.png_len as usize)];
            assert_eq!(p, b"\x89PNG\r\n\x1a\n", "{what}: cell {i} is not a PNG");
            cells.push(c);
        }
        let mut codepoints = Vec::with_capacity(n_cp);
        for _ in 0..n_cp {
            codepoints.push((word(at), word(at + 1)));
            at += 2;
        }
        assert!(codepoints.windows(2).all(|w| w[0].0 < w[1].0), "{what}: codepoints not ascending");
        // Skip the sequence table and the name table; assert they fit.
        at += n_seq as usize * (2 + seq_max);
        at += n + 1; // name offsets + blob length
        assert!(at * 4 <= png_start, "{what}: tables overrun the PNG blob");

        Self {
            cell_w,
            cell_h,
            cols,
            rows_per_layer,
            layers,
            layer_w,
            layer_h,
            ppem,
            bearing_y,
            advance_px,
            cells,
            codepoints,
            sequence_count: n_seq,
            font_sha256,
            png: bytes[png_start..].to_vec(),
        }
    }

    pub fn png_of(&self, cell: &EmojiCell) -> &[u8] {
        &self.png[cell.png_offset as usize..][..cell.png_len as usize]
    }

    /// Decode every cell into per-layer RGBA8 (straight alpha, sRGB-encoded
    /// as the PNGs are). Cells are laid in rows of `cols`; each row of cells
    /// is one contiguous band of the layer buffer, so the bands are handed
    /// out to threads with no shared writes. Also returns, per cell, the
    /// alpha-weighted mean of its pixels in LINEAR rgb and its mean alpha —
    /// what the cell looks like from far away, for the backdrop tint.
    pub fn decode_layers(&self) -> (Vec<Vec<u8>>, Vec<[f32; 4]>) {
        // sRGB byte → linear, the shader's pow(2.2) decode, tabulated once.
        let lut: Vec<f32> = (0..256).map(|b| (b as f32 / 255.0).powf(2.2)).collect();
        let (lw, lh, cw, ch) = (self.layer_w as usize, self.layer_h as usize, self.cell_w as usize, self.cell_h as usize);
        let per_layer = (self.cols * self.rows_per_layer) as usize;
        let mut layers: Vec<Vec<u8>> = (0..self.layers).map(|_| vec![0u8; lw * lh * 4]).collect();
        let band_bytes = ch * lw * 4;
        let mut bands: Vec<(usize, &mut [u8])> = Vec::new();
        for (li, layer) in layers.iter_mut().enumerate() {
            for (ri, band) in layer.chunks_mut(band_bytes).enumerate() {
                bands.push((li * per_layer + ri * self.cols as usize, band));
            }
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(bands.len().max(1));
        let bands_ref = std::sync::Mutex::new(bands);
        let ink: Vec<std::sync::Mutex<[f32; 4]>> = (0..self.cells.len()).map(|_| std::sync::Mutex::new([0.0; 4])).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let band = {
                        let mut b = bands_ref.lock().expect("band list mutex");
                        if i >= b.len() {
                            return;
                        }
                        // Take the band out so the lock is not held across decodes.
                        std::mem::take(&mut b[i])
                    };
                    let (first_cell, band) = band;
                    for col in 0..self.cols as usize {
                        let ci = first_cell + col;
                        let Some(cell) = self.cells.get(ci) else { break };
                        let img = image::load_from_memory(self.png_of(cell))
                            .unwrap_or_else(|e| panic!("emoji cell {ci} (glyph {}): {e}", cell.glyph_id))
                            .into_rgba8();
                        assert_eq!(img.dimensions(), (self.cell_w, self.cell_h), "emoji cell {ci}: size");
                        let x0 = cell.x as usize * 4;
                        let mut acc = [0f64; 4];
                        for (r, row) in img.as_raw().chunks_exact(cw * 4).enumerate() {
                            band[r * lw * 4 + x0..r * lw * 4 + x0 + cw * 4].copy_from_slice(row);
                            for px in row.as_chunks::<4>().0 {
                                let a = px[3] as f64 / 255.0;
                                acc[0] += lut[px[0] as usize] as f64 * a;
                                acc[1] += lut[px[1] as usize] as f64 * a;
                                acc[2] += lut[px[2] as usize] as f64 * a;
                                acc[3] += a;
                            }
                        }
                        let n = (cw * ch) as f64;
                        let mean = if acc[3] > 0.0 {
                            [(acc[0] / acc[3]) as f32, (acc[1] / acc[3]) as f32, (acc[2] / acc[3]) as f32, (acc[3] / n) as f32]
                        } else {
                            [0.0; 4]
                        };
                        *ink[ci].lock().expect("ink mutex") = mean;
                    }
                });
            }
        });
        (layers, ink.into_iter().map(|m| m.into_inner().expect("ink mutex")).collect())
    }
}

/// One 2×2 box-filter level of a straight-alpha RGBA8 image, computed in
/// PREMULTIPLIED space and returned straight. Averaging straight alpha lets a
/// fully transparent texel's colour (arbitrary in a palette PNG) bleed into
/// its opaque neighbours' average; weighting by alpha first is the fix, and
/// un-premultiplying after keeps the storage format one thing. Integer
/// arithmetic with round-half-up, so the result is the same on every host.
pub fn box_down_straight(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    assert!(w.is_multiple_of(2) && h.is_multiple_of(2), "box filter needs even dimensions, got {w}x{h}");
    let (w, h) = (w as usize, h as usize);
    let (ow, oh) = (w / 2, h / 2);
    let mut out = vec![0u8; ow * oh * 4];
    for y in 0..oh {
        for x in 0..ow {
            let mut pm = [0u32; 4];
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let i = ((2 * y + dy) * w + 2 * x + dx) * 4;
                let a = src[i + 3] as u32;
                pm[0] += src[i] as u32 * a;
                pm[1] += src[i + 1] as u32 * a;
                pm[2] += src[i + 2] as u32 * a;
                pm[3] += a;
            }
            let a = (pm[3] + 2) / 4;
            let o = (y * ow + x) * 4;
            out[o + 3] = a as u8;
            if a > 0 {
                // pm[c] is Σ c·a over four texels; the straight channel of the
                // average is (Σ c·a / 4) / (Σ a / 4) = Σ c·a / Σ a.
                for c in 0..3 {
                    out[o + c] = ((pm[c] + pm[3] / 2) / pm[3]).min(255) as u8;
                }
            }
        }
    }
    out
}

/// The sheet on the device: an `Rgba8UnormSrgb` 2D-array texture, one layer
/// per sheet layer, `mip_levels` levels, straight alpha. The shader (step 5)
/// samples with filtering and premultiplies after the sample. Views are the
/// scene's to make (a D2Array view per bind group, as for the Slug textures).
pub struct EmojiTexture {
    pub texture: wgpu::Texture,
    pub sheet: EmojiSheet,
    pub mip_levels: u32,
    /// Bytes uploaded across all layers and levels.
    pub texture_bytes: u64,
    /// Per cell: alpha-weighted mean linear rgb + mean alpha (see
    /// `EmojiSheet::decode_layers`).
    pub cell_ink: Vec<[f32; 4]>,
}

impl EmojiTexture {
    #[allow(dead_code)]
    pub fn load(ctx: &GpuContext, path: &Path) -> Self {
        Self::load_device(&ctx.device, &ctx.queue, path)
    }

    pub fn load_device(device: &wgpu::Device, queue: &wgpu::Queue, path: &Path) -> Self {
        let t0 = std::time::Instant::now();
        let sheet = EmojiSheet::load(path);
        let t_parse = t0.elapsed();
        let (level0, cell_ink) = sheet.decode_layers();
        let t_decode = t0.elapsed() - t_parse;
        let mip_levels = mip_levels_for(sheet.cell_w, sheet.cell_h);
        let mut levels: Vec<Vec<Vec<u8>>> = Vec::with_capacity(sheet.layers as usize); // [layer][level]
        for base in level0 {
            let mut chain = vec![base];
            let (mut w, mut h) = (sheet.layer_w, sheet.layer_h);
            for _ in 1..mip_levels {
                let next = box_down_straight(chain.last().unwrap(), w, h);
                w /= 2;
                h /= 2;
                chain.push(next);
            }
            levels.push(chain);
        }
        let t_mips = t0.elapsed() - t_parse - t_decode;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("emoji sheet"),
            size: wgpu::Extent3d { width: sheet.layer_w, height: sheet.layer_h, depth_or_array_layers: sheet.layers },
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut texture_bytes = 0u64;
        for (layer, chain) in levels.iter().enumerate() {
            let (mut w, mut h) = (sheet.layer_w, sheet.layer_h);
            for (level, data) in chain.iter().enumerate() {
                assert!((w * 4).is_multiple_of(UPLOAD_PITCH_ALIGN), "emoji mip {level}: pitch {} not aligned", w * 4);
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: level as u32,
                        origin: wgpu::Origin3d { x: 0, y: 0, z: layer as u32 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
                texture_bytes += data.len() as u64;
                w /= 2;
                h /= 2;
            }
        }
        let t_upload = t0.elapsed() - t_parse - t_decode - t_mips;
        log::info!(
            "emoji sheet: {} cells ({}x{}, {} ppem, bearing y {}, advance {} px) in {} layer(s) of {}x{}, \
             {} mip levels, {:.1} MiB of texture; {} codepoints, {} sequences carried; font sha256 {:08x}…; \
             parse {:.1} ms, decode {:.1} ms on {} threads, mips {:.1} ms, upload enqueue {:.1} ms",
            sheet.cells.len(),
            sheet.cell_w,
            sheet.cell_h,
            sheet.ppem,
            sheet.bearing_y,
            sheet.advance_px,
            sheet.layers,
            sheet.layer_w,
            sheet.layer_h,
            mip_levels,
            texture_bytes as f64 / (1 << 20) as f64,
            sheet.codepoints.len(),
            sheet.sequence_count,
            sheet.font_sha256[0].swap_bytes(),
            t_parse.as_secs_f64() * 1e3,
            t_decode.as_secs_f64() * 1e3,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            t_mips.as_secs_f64() * 1e3,
            t_upload.as_secs_f64() * 1e3,
        );
        Self { texture, sheet, mip_levels, texture_bytes, cell_ink }
    }
}

fn read_words(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    assert!(bytes.len().is_multiple_of(4), "{}: not a u32 array", path.display());
    // as_chunks is byte-identical to chunks_exact(4) here: the assert above
    // guarantees no remainder, so both yield every consecutive 4-byte group.
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

fn check_magic(words: &[u32], expected: &str, path: &Path) {
    let magic = words.first().copied().unwrap_or(0);
    let exp = u32::from_le_bytes(expected.as_bytes().try_into().expect("magic constants are exactly 4 ASCII bytes"));
    assert_eq!(
        magic, exp,
        "{}: bad magic 0x{magic:08x}, expected {expected}",
        path.display()
    );
}

/// Upload a raw u32 texel payload as a 1024-wide Rgba32Uint texture.
fn upload_uint_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    width: u32,
    height: u32,
    payload: &[u32],
) -> wgpu::Texture {
    assert_eq!(
        payload.len(),
        (width * height * 4) as usize,
        "{label}: payload size mismatch"
    );
    let texture = device.create_texture(&wgpu::TextureDescriptor {
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
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(payload),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 16), // RGBA32Uint = 16 B/texel; 1024*16 is 256-aligned
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

impl Atlas {
    /// Load from the workspace atlas directory (`<crate>/../assets/atlas`),
    /// with the emoji sheet at `emoji_sheet` (the `--emoji-sheet` flag; the
    /// default is the committed one beside the other bins).
    pub fn load(ctx: &GpuContext, emoji_sheet: &Path) -> Self {
        Self::load_device(&ctx.device, &ctx.queue, emoji_sheet)
    }

    pub fn load_device(device: &wgpu::Device, queue: &wgpu::Queue, emoji_sheet: &Path) -> Self {
        Self::load_from_device(
            device,
            queue,
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/atlas"),
            emoji_sheet,
        )
    }

    #[allow(dead_code)]
    pub fn load_from(ctx: &GpuContext, dir: &Path, emoji_sheet: &Path) -> Self {
        Self::load_from_device(&ctx.device, &ctx.queue, dir, emoji_sheet)
    }

    pub fn load_from_device(device: &wgpu::Device, queue: &wgpu::Queue, dir: &Path, emoji_sheet: &Path) -> Self {
        // --- curves.bin (G3CV): 8-word header, then width*height*4 texels ---
        let cv = read_words(&dir.join("curves.bin"));
        check_magic(&cv, "G3CV", &dir.join("curves.bin"));
        let (cv_w, cv_h, curve_count) = (cv[3], cv[4], cv[5]);
        assert_eq!(cv_w, ATLAS_TEX_WIDTH);
        let curves = upload_uint_texture(device, queue, "slug curves", cv_w, cv_h, &cv[8..]);

        // --- glyphmap.bin (G3GM): 8-word header, then texels ---
        let gm = read_words(&dir.join("glyphmap.bin"));
        check_magic(&gm, "G3GM", &dir.join("glyphmap.bin"));
        let (gm_w, gm_h, entry_count) = (gm[3], gm[4], gm[5]);
        assert_eq!(gm_w, ATLAS_TEX_WIDTH);
        let glyphmap = upload_uint_texture(device, queue, "slug glyphmap", gm_w, gm_h, &gm[8..]);

        // --- glyphs.bin (G3GL) + codepoints.bin (G3CP): the CPU-side trie
        //     (Stage E1: shared with the GPU-free cross-check via TrieTable). ---
        let trie = TrieTable::load(dir);
        let metrics = trie.metrics;

        // --- glyphmap/glyphs slot-count agreement --------------------------
        let slot_count = trie.slot_count;
        assert_eq!(slot_count, entry_count, "glyphmap/glyphs slot count mismatch");

        log::info!(
            "atlas: {} slots, {} curves ({}x{}), {} mapped codepoints, fonts upem={} cell={}fu em={}fu",
            slot_count,
            curve_count,
            cv_w,
            cv_h,
            trie.mapped_count,
            metrics.upem,
            metrics.advance_fu,
            metrics.em_height_fu,
        );

        let emoji = EmojiTexture::load_device(device, queue, emoji_sheet);
        let slot_ink: Vec<Option<[f32; 4]>> = trie
            .emoji_cell
            .iter()
            .map(|c| c.map(|c| emoji.cell_ink[c as usize]))
            .collect();
        log::info!(
            "slot ink: {} of {} slots are bitmap cells with a mean colour for the backdrop",
            slot_ink.iter().filter(|s| s.is_some()).count(),
            slot_ink.len()
        );

        Self {
            curves,
            glyphmap,
            metrics,
            trie,
            emoji,
            slot_ink,
        }
    }

    /// Codepoint → trie entry (the two dependent loads of FORMAT.md).
    pub fn lookup(&self, cp: u32) -> TrieEntry {
        self.trie.lookup(cp)
    }
}

#[cfg(test)]
mod emoji_sheet_tests {
    use super::*;

    /// A 2×2 RGBA PNG: (255,0,0,255) (0,255,0,128) / (0,0,255,0) (255,255,255,255).
    /// The third texel is fully transparent BLUE — the case a straight-alpha
    /// box filter gets wrong.
    const PNG_2X2: [u8; 78] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xb6, 0x0d,
        0x24, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
        0x1f, 0x08, 0x1b, 0x18, 0xc0, 0xf4, 0xff, 0xff, 0xff, 0x01, 0x3f, 0xd7, 0x08, 0x79, 0x6e, 0x0c,
        0xe2, 0x91, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const TEXELS: [[u8; 4]; 4] = [[255, 0, 0, 255], [0, 255, 0, 128], [0, 0, 255, 0], [255, 255, 255, 255]];

    /// A G3ES blob with `n` copies of the 2×2 PNG as cells, laid 32 per row
    /// (32 × 2 px = 64 px, the pitch alignment), one layer.
    fn synthetic(n: u32, magic: &[u8; 4]) -> Vec<u8> {
        let cols = 32u32;
        let rows = n.div_ceil(cols).max(1);
        let mut hdr = [0u32; SHEET_HEADER_WORDS];
        hdr[0] = u32::from_le_bytes(*magic);
        hdr[1] = 1;
        hdr[2] = (SHEET_HEADER_WORDS * 4) as u32;
        hdr[3] = 2;
        hdr[4] = 2;
        hdr[5] = n;
        hdr[6] = cols;
        hdr[7] = rows;
        hdr[8] = 1;
        hdr[9] = 64;
        hdr[10] = rows * 2;
        hdr[11] = 109;
        hdr[15] = 101;
        hdr[16] = 136;
        hdr[21] = 2; // two codepoints
        hdr[22] = 0;
        hdr[23] = 9;
        let cells: Vec<u32> = (0..n)
            .flat_map(|i| [i + 10, 0, (i % cols) * 2, (i / cols) * 2, i * 78, 78])
            .collect();
        let cps = [0x41u32, 10, 0x1F600, 11];
        let names: Vec<u32> = (0..=n).collect(); // offsets + blob len 0
        let tables = SHEET_HEADER_WORDS as u32 + cells.len() as u32 + cps.len() as u32 + names.len() as u32;
        hdr[24] = tables * 4;
        hdr[25] = 78 * n;
        for (i, w) in hdr.iter_mut().skip(32).enumerate() {
            *w = 0xA0 + i as u32;
        }
        let mut out: Vec<u8> = Vec::new();
        for w in hdr.iter().chain(&cells).chain(&cps).chain(&names) {
            out.extend_from_slice(&w.to_le_bytes());
        }
        for _ in 0..n {
            out.extend_from_slice(&PNG_2X2);
        }
        out
    }

    #[test]
    fn parses_a_synthetic_sheet_and_hands_back_the_png() {
        let sheet = EmojiSheet::parse(&synthetic(3, b"G3ES"), "synthetic");
        assert_eq!((sheet.cell_w, sheet.cell_h, sheet.layers, sheet.layer_w, sheet.layer_h), (2, 2, 1, 64, 2));
        assert_eq!(sheet.cells.len(), 3);
        assert_eq!(sheet.cells[2].glyph_id, 12);
        assert_eq!((sheet.cells[2].x, sheet.cells[2].y), (4, 0));
        assert_eq!(sheet.codepoints, vec![(0x41, 10), (0x1F600, 11)]);
        assert_eq!(sheet.font_sha256[0], 0xA0);
        assert_eq!(sheet.png_of(&sheet.cells[1]), &PNG_2X2[..]);
    }

    #[test]
    #[should_panic(expected = "bad magic")]
    fn refuses_a_wrong_magic() {
        EmojiSheet::parse(&synthetic(1, b"G3CV"), "synthetic");
    }

    /// The decoder pins the `image` crate's PNG path on known bytes; a
    /// palette or bit-depth expansion that changed a texel would show here.
    #[test]
    fn decodes_cells_into_their_layer_positions() {
        let sheet = EmojiSheet::parse(&synthetic(3, b"G3ES"), "synthetic");
        let (layers, ink) = sheet.decode_layers();
        assert_eq!(layers.len(), 1);
        // Mean over the 2×2: alpha-weighted linear rgb of (red@1, green@.5,
        // blue@0, white@1) and mean alpha (255+128+0+255)/4/255.
        let a = [1.0f64, 128.0 / 255.0, 0.0, 1.0];
        let lin = |b: u8| (b as f64 / 255.0).powf(2.2);
        let want_r = (lin(255) * a[0] + lin(255) * a[3]) / a.iter().sum::<f64>();
        assert!((ink[0][0] as f64 - want_r).abs() < 1e-5, "mean r {} vs {want_r}", ink[0][0]);
        assert!((ink[0][2] as f64 - lin(255) * a[3] / a.iter().sum::<f64>()).abs() < 1e-5, "transparent blue must not count");
        assert!((ink[0][3] as f64 - a.iter().sum::<f64>() / 4.0).abs() < 1e-6);
        assert_eq!(ink.len(), 3);
        let l = &layers[0];
        assert_eq!(l.len(), 64 * 2 * 4);
        let texel = |x: usize, y: usize| <[u8; 4]>::try_from(&l[(y * 64 + x) * 4..][..4]).unwrap();
        // cell 0 at (0,0), cell 2 at (4,0); row-major inside a cell
        assert_eq!(texel(0, 0), TEXELS[0]);
        assert_eq!(texel(1, 0), TEXELS[1]);
        assert_eq!(texel(0, 1), TEXELS[2]);
        assert_eq!(texel(1, 1), TEXELS[3]);
        assert_eq!(texel(4, 1), TEXELS[2]);
        assert_eq!(texel(5, 1), TEXELS[3]);
        assert_eq!(texel(6, 0), [0, 0, 0, 0], "padding past the last cell stays clear");
    }

    /// The transparent blue texel must contribute nothing to the average's
    /// colour: a straight-alpha average would put blue at (0+0+255+255)/4.
    #[test]
    fn box_filter_weights_by_alpha() {
        let src: Vec<u8> = TEXELS.concat();
        let out = box_down_straight(&src, 2, 2);
        // premultiplied sums: r 510, g 383, b 255, a 638 → a=160, straight r=510*255/638...
        assert_eq!(out, vec![204, 153, 102, 160]);
    }

    #[test]
    fn mip_levels_never_cross_a_cell_edge() {
        assert_eq!(mip_levels_for(136, 128), 4);
        assert_eq!(mip_levels_for(2, 2), 2);
        assert_eq!(mip_levels_for(7, 7), 1);
        assert_eq!(mip_levels_for(1024, 1024), EMOJI_MAX_MIP_LEVELS);
    }
}

/// The v2 sequence/class sections in the committed codepoints.bin — the real
/// artifact, not a synthetic one, because the thing being pinned is exactly
/// that the artifact carries what the generator promised.
#[cfg(test)]
mod trie_v2_tests {
    use super::*;

    fn load() -> TrieTable {
        TrieTable::load(&crate::atlas_dir())
    }

    #[test]
    fn family_sequence_resolves_to_its_slot() {
        let t = load();
        assert!(!t.sequences.is_empty(), "the v2 sequence section is present");
        // The gen_real_trie.py pin, checked on this side too: the family's
        // slot moves only if the sheet's sequence table does.
        let fam = [0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467];
        assert_eq!(t.sequence_lookup(&fam), Some(6819));
        // A partial prefix is NOT a table entry (longest-match is the rule).
        assert_eq!(t.sequence_lookup(&fam[..3]), None);
        // The singleton-codepoint case is not a sequence either.
        assert_eq!(t.sequence_lookup(&[0x1F600]), None);
        // A flag pair resolves; an unpaired triple does not.
        assert!(t.sequence_lookup(&[0x1F1E6, 0x1F1E8]).is_some());
        assert_eq!(t.sequence_lookup(&[0x1F1E6, 0x1F1E8, 0x1F1E9]), None);
    }

    #[test]
    fn class_table_answers_the_named_pins() {
        let t = load();
        assert!(!t.classes.is_empty(), "the v2 class section is present");
        let bit = |b: u32| 1 << b;
        assert_eq!(t.class_of(0x200D) & bit(1), bit(1), "ZWJ");
        assert_eq!(t.class_of(0x1F1E6) & bit(2), bit(2), "regional indicator");
        assert_eq!(t.class_of(0x1F600) & bit(11), bit(11), "extended pictographic");
        assert_eq!(t.class_of(0xFE0F) & bit(0), bit(0), "VS16 is Extend");
        assert_eq!(t.class_of(0x1F3FB) & (bit(0) | bit(12)), bit(0) | bit(12), "skin tone is Extend+Modifier");
        assert_eq!(t.class_of(0x0A) & bit(3), bit(3), "LF is Control");
        assert_eq!(t.class_of(0x41), 0, "'A' is Other");
        assert_eq!(t.class_of(0x110000), 0, "past the last scalar is Other");
    }
}
