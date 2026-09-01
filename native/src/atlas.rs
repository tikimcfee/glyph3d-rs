//! Stage C — glyph atlas loader.
//!
//! Parses the four Stage B export files in `assets/atlas/` per FORMAT.md:
//!   curves.bin     (G3CV) — Slug curve texture payload, 1024x161 RGBA32Uint
//!   glyphmap.bin   (G3GM) — glyph-map texture payload,   1024x5   RGBA32Uint
//!   glyphs.bin     (G3GL) — per-slot metrics (parsed for validation; not uploaded)
//!   codepoints.bin (G3CP) — codepoint → slot two-level trie (CPU side)
//!
//! curves/glyphmap are uploaded verbatim as `Rgba32Uint` textures and sampled
//! in WGSL with `textureLoad` (integer textures: no filtering, no sRGB).
//! The trie stays on the CPU — text staging (text.rs) does byte→slot decode.

use std::path::{Path, PathBuf};

use crate::gpu::GpuContext;

/// Both Slug textures are row-major, 1024 texels wide (slug-constants.js
/// TEXTURE_WIDTH). Texel `i` lives at `(i % 1024, i / 1024)`.
pub const ATLAS_TEX_WIDTH: u32 = 1024;

/// Trie entry flags (FORMAT.md, codepoints.bin). FLAG_BLANK (4) — a covered
/// codepoint resolving to slot 0 — exists in the format but is not yet
/// consumed here (no growth logic); add it back when a reader lands.
pub const FLAG_MISSING: u32 = 1;
pub const FLAG_BITMAP: u32 = 2;

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

        let cp = read_words(&dir.join("codepoints.bin"));
        check_magic(&cp, "G3CP", &dir.join("codepoints.bin"));
        let block_shift = cp[3];
        let block_index_len = cp[4] as usize;
        let block_count = cp[5] as usize;
        let entry_stride = cp[6];
        let mapped_count = cp[7];
        let block_index = cp[11..11 + block_index_len].to_vec();
        let block_words = block_count * (1usize << block_shift) * entry_stride as usize;
        let blocks = cp[11 + block_index_len..11 + block_index_len + block_words].to_vec();
        let t = Self {
            metrics,
            block_shift,
            block_index,
            blocks,
            entry_stride,
            mapped_count,
            slot_count,
        };
        // Sanity: 'A' must resolve to slot 34 / advance 1229 (FORMAT.md worked example).
        let a = t.lookup(0x41);
        assert_eq!((a.glyph_id, a.advance_fu), (34, 1229), "trie sanity check failed for 'A'");
        t
    }

    /// Codepoint → trie entry (the two dependent loads of FORMAT.md).
    pub fn lookup(&self, cp: u32) -> TrieEntry {
        let block = self.block_index[(cp >> self.block_shift) as usize];
        let e = ((block << self.block_shift) | (cp & 0xFF)) as usize * self.entry_stride as usize;
        TrieEntry {
            glyph_id: self.blocks[e],
            advance_fu: self.blocks[e + 1] as i32,
            height_fu: self.blocks[e + 2] as i32,
            flags: self.blocks[e + 3],
        }
    }
}

pub struct Atlas {
    pub curves: wgpu::Texture,
    pub glyphmap: wgpu::Texture,
    pub metrics: PrimaryMetrics,
    /// The CPU-side codepoint→slot trie (Stage E1: shared with the engine
    /// cross-check via [`TrieTable`]).
    pub trie: TrieTable,
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
    ctx: &GpuContext,
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
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
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
    ctx.queue.write_texture(
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
    /// Load from the workspace atlas directory (`<crate>/../assets/atlas`).
    pub fn load(ctx: &GpuContext) -> Self {
        Self::load_from(
            ctx,
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../assets/atlas"),
        )
    }

    pub fn load_from(ctx: &GpuContext, dir: &Path) -> Self {
        // --- curves.bin (G3CV): 8-word header, then width*height*4 texels ---
        let cv = read_words(&dir.join("curves.bin"));
        check_magic(&cv, "G3CV", &dir.join("curves.bin"));
        let (cv_w, cv_h, curve_count) = (cv[3], cv[4], cv[5]);
        assert_eq!(cv_w, ATLAS_TEX_WIDTH);
        let curves = upload_uint_texture(ctx, "slug curves", cv_w, cv_h, &cv[8..]);

        // --- glyphmap.bin (G3GM): 8-word header, then texels ---
        let gm = read_words(&dir.join("glyphmap.bin"));
        check_magic(&gm, "G3GM", &dir.join("glyphmap.bin"));
        let (gm_w, gm_h, entry_count) = (gm[3], gm[4], gm[5]);
        assert_eq!(gm_w, ATLAS_TEX_WIDTH);
        let glyphmap = upload_uint_texture(ctx, "slug glyphmap", gm_w, gm_h, &gm[8..]);

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

        Self {
            curves,
            glyphmap,
            metrics,
            trie,
        }
    }

    /// Codepoint → trie entry (the two dependent loads of FORMAT.md).
    pub fn lookup(&self, cp: u32) -> TrieEntry {
        self.trie.lookup(cp)
    }
}
