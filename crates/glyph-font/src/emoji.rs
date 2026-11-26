//! Colour-emoji bitmap source trait and lightweight `G3ES` (`emoji-sheet.bin`) lookup index.
//!
//! `glyph-font` stays independent of `native`'s GPU/PNG decoding code: a host hands [`crate::FontAtlas`]
//! any [`EmojiSource`] (trait object or closure) answering which bitmap-sheet cell index draws a
//! codepoint or sequence, and [`crate::FontAtlas`] writes `[0, 0, GLYPH_MODE_BITMAP (1), cell]` into its
//! glyph-map texture — the exact layout `assets/atlas/glyphmap.bin` and `wall_cells.wgsl` expect.
//!
//! [`EmojiSheetIndex`] parses only the cell, codepoint, and sequence lookup tables from a `G3ES`
//! file (`assets/atlas/FORMAT.md`) without decoding any PNGs, so either a host or a test can wire
//! `assets/atlas/emoji-sheet.bin` directly into [`crate::FontAtlas`].

use std::cmp::Ordering;
use std::fmt;
use std::path::Path;

/// Source of colour-emoji bitmap sheet cell indices for [`crate::FontAtlas`].
///
/// Any closure `Fn(&str) -> Option<u32> + Send + Sync` implements this trait automatically.
pub trait EmojiSource: Send + Sync {
    /// Return the bitmap sheet cell index that draws `text` (a single emoji scalar or an emoji
    /// sequence such as a ZWJ family, flag, skin-tone modifier, keycap, or VS16 presentation),
    /// or `None` if the sheet has no bitmap cell for it.
    fn cell_for(&self, text: &str) -> Option<u32>;
}

impl<F> EmojiSource for F
where
    F: Fn(&str) -> Option<u32> + Send + Sync,
{
    fn cell_for(&self, text: &str) -> Option<u32> {
        (self)(text)
    }
}

/// Errors returned when parsing a `G3ES` (`emoji-sheet.bin`) index.
#[derive(Debug)]
pub enum EmojiSheetError {
    Io(std::io::Error),
    Invalid(&'static str),
}

impl fmt::Display for EmojiSheetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read emoji sheet: {e}"),
            Self::Invalid(msg) => write!(f, "invalid G3ES emoji sheet: {msg}"),
        }
    }
}

impl std::error::Error for EmojiSheetError {}

const G3ES_HEADER_WORDS: usize = 40;
const G3ES_CELL_STRIDE: usize = 6;
const G3ES_MAGIC: &[u8; 4] = b"G3ES";

/// Lightweight CPU lookup index parsed from a `G3ES` (`assets/atlas/emoji-sheet.bin`) file.
///
/// Maps single codepoints and multi-codepoint sequences (with `U+FE0F` stripped, matching
/// `native/src/atlas.rs` and `native/src/wall/cell.rs`) to their cell index (`0..cell_count`)
/// in the sheet's cell table. Skips the PNG blob entirely.
#[derive(Clone, Debug)]
pub struct EmojiSheetIndex {
    cell_count: u32,
    /// Sorted `(codepoint, cell_index)` pairs for every single-codepoint cmap entry that has a cell.
    codepoints: Vec<(u32, u32)>,
    /// Sequence records packed as `[cell_index, len, cp_0..cp_{seq_max-1}]`, sorted by sequence.
    sequences: Vec<u32>,
    seq_max: usize,
}

impl EmojiSheetIndex {
    /// Load and parse the lookup tables of a `G3ES` file from disk.
    pub fn load(path: &Path) -> Result<Self, EmojiSheetError> {
        let bytes = std::fs::read(path).map_err(EmojiSheetError::Io)?;
        Self::parse(&bytes)
    }

    /// Parse the cell, codepoint, and sequence tables from raw `G3ES` bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, EmojiSheetError> {
        if bytes.len() < G3ES_HEADER_WORDS * 4 {
            return Err(EmojiSheetError::Invalid("shorter than 160-byte G3ES header"));
        }
        if &bytes[0..4] != G3ES_MAGIC {
            return Err(EmojiSheetError::Invalid("bad magic (expected G3ES)"));
        }
        let word = |i: usize| -> u32 {
            u32::from_le_bytes(
                bytes[i * 4..i * 4 + 4]
                    .try_into()
                    .expect("4-byte aligned word"),
            )
        };
        if word(1) != 1 {
            return Err(EmojiSheetError::Invalid("unsupported G3ES version"));
        }
        if word(2) as usize != G3ES_HEADER_WORDS * 4 {
            return Err(EmojiSheetError::Invalid("unexpected G3ES header size"));
        }

        let cell_count = word(5) as usize;
        let cp_count = word(21) as usize;
        let seq_count = word(22) as usize;
        let seq_max = word(23) as usize;
        let png_start = word(24) as usize;

        let seq_stride = 2 + seq_max;
        let needed_words = G3ES_HEADER_WORDS
            .saturating_add(cell_count.saturating_mul(G3ES_CELL_STRIDE))
            .saturating_add(cp_count.saturating_mul(2))
            .saturating_add(seq_count.saturating_mul(seq_stride));
        if needed_words.saturating_mul(4) > bytes.len() || png_start > bytes.len() {
            return Err(EmojiSheetError::Invalid("tables truncated"));
        }

        // Cell table: strictly ascending by glyph_id; position `i` IS the cell index.
        let mut at = G3ES_HEADER_WORDS;
        let mut cell_glyph_ids = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            cell_glyph_ids.push(word(at));
            at += G3ES_CELL_STRIDE;
        }
        let cell_of_glyph = |gid: u32| -> Option<u32> {
            cell_glyph_ids.binary_search(&gid).ok().map(|i| i as u32)
        };

        // Codepoint table: `[cp, gid]` sorted by cp. Keep only entries that have a bitmap cell.
        let mut codepoints = Vec::with_capacity(cp_count);
        for _ in 0..cp_count {
            let cp = word(at);
            let gid = word(at + 1);
            at += 2;
            if let Some(cell) = cell_of_glyph(gid) {
                codepoints.push((cp, cell));
            }
        }

        // Sequence table: `[len, gid, cp_0..cp_{seq_max-1}]`, sorted by codepoint sequence.
        let mut sequences = Vec::with_capacity(seq_count * seq_stride);
        for _ in 0..seq_count {
            let len = word(at);
            let gid = word(at + 1);
            let Some(cell) = cell_of_glyph(gid) else {
                at += seq_stride;
                continue;
            };
            sequences.push(cell);
            sequences.push(len);
            for k in 0..seq_max {
                sequences.push(word(at + 2 + k));
            }
            at += seq_stride;
        }

        Ok(Self {
            cell_count: cell_count as u32,
            codepoints,
            sequences,
            seq_max,
        })
    }

    /// Total number of bitmap cells in the sheet.
    #[inline]
    pub fn cell_count(&self) -> u32 {
        self.cell_count
    }

    /// Number of single-codepoint bitmap mappings.
    #[inline]
    pub fn codepoint_count(&self) -> usize {
        self.codepoints.len()
    }

    /// Number of multi-codepoint sequence bitmap mappings.
    #[inline]
    pub fn sequence_count(&self) -> usize {
        if self.seq_max == 0 {
            0
        } else {
            self.sequences.len() / (2 + self.seq_max)
        }
    }

    /// Look up a single codepoint's bitmap cell index.
    pub fn cell_for_codepoint(&self, cp: u32) -> Option<u32> {
        self.codepoints
            .binary_search_by_key(&cp, |&(k, _)| k)
            .ok()
            .map(|i| self.codepoints[i].1)
    }

    /// Look up an exact `U+FE0F`-stripped codepoint sequence in the sequence table.
    pub fn cell_for_sequence(&self, cps: &[u32]) -> Option<u32> {
        if self.sequences.is_empty() || cps.len() < 2 || cps.len() > self.seq_max {
            return None;
        }
        let stride = 2 + self.seq_max;
        let n = self.sequences.len() / stride;
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let o = mid * stride;
            let len = self.sequences[o + 1] as usize;
            let entry_cps = &self.sequences[o + 2..o + 2 + len];
            if cmp_seq(cps, entry_cps) == Ordering::Greater {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n {
            let o = lo * stride;
            let len = self.sequences[o + 1] as usize;
            let entry_cps = &self.sequences[o + 2..o + 2 + len];
            if cmp_seq(cps, entry_cps) == Ordering::Equal {
                return Some(self.sequences[o]);
            }
        }
        None
    }
}

impl EmojiSource for EmojiSheetIndex {
    fn cell_for(&self, text: &str) -> Option<u32> {
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
            return self.cell_for_codepoint(key[0]);
        }
        // Probe longest matching prefix >= 2, matching native/src/wall/cell.rs.
        let max_probe = len.min(self.seq_max);
        for probe_len in (2..=max_probe).rev() {
            if let Some(cell) = self.cell_for_sequence(&key[..probe_len]) {
                return Some(cell);
            }
        }
        None
    }
}

#[inline]
fn cmp_seq(a: &[u32], b: &[u32]) -> Ordering {
    for (&x, &y) in a.iter().zip(b.iter()) {
        match x.cmp(&y) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    a.len().cmp(&b.len())
}
