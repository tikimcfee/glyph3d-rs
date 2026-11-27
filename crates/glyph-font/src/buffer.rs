//! Growable CPU backing buffers for the two Slug RGBA32Uint textures (`curves` and `glyphmap`),
//! with generation counters and dirty-range tracking for minimal GPU uploads.
//!
//! Matches `SlugBuffer` in `slugData.js` and the texture layout in `assets/atlas/FORMAT.md`.

use std::ops::Range;

use crate::constants::{
    CURVE_INITIAL_ROWS, MAP_INITIAL_ROWS, TEXELS_PER_CURVE, TEXTURE_WIDTH, U32S_PER_CURVE,
    U32S_PER_TEXEL,
};
use crate::encode::PackedCurve;

/// A 2D texel rectangle inside a `TEXTURE_WIDTH`-wide RGBA32Uint texture, ready for
/// `wgpu::Queue::write_texture`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexelRect {
    /// Column in texels (`0..TEXTURE_WIDTH`).
    pub x: u32,
    /// Row in texels (`0..height`).
    pub y: u32,
    /// Width in texels (`1..=TEXTURE_WIDTH - x`).
    pub width: u32,
    /// Height in texels (`>= 1`).
    pub height: u32,
}

/// Decompose a flat texel range `[lo, hi)` in a `width`-wide row-major texture into at most
/// three axis-aligned upload rectangles:
/// 1. partial tail of the starting row,
/// 2. full middle rows,
/// 3. partial head of the ending row.
pub fn texel_range_to_rects(range: Range<u32>, width: u32) -> Vec<TexelRect> {
    if range.is_empty() || width == 0 {
        return Vec::new();
    }
    let start_row = range.start / width;
    let start_col = range.start % width;
    let end_inclusive = range.end - 1;
    let end_row = end_inclusive / width;
    let end_col = end_inclusive % width;

    if start_row == end_row {
        return vec![TexelRect {
            x: start_col,
            y: start_row,
            width: end_col - start_col + 1,
            height: 1,
        }];
    }

    let mut rects = Vec::with_capacity(3);
    let mut full_start = start_row;
    if start_col > 0 {
        rects.push(TexelRect {
            x: start_col,
            y: start_row,
            width: width - start_col,
            height: 1,
        });
        full_start += 1;
    }

    let full_end_exclusive = if end_col + 1 == width {
        end_row + 1
    } else {
        end_row
    };
    if full_end_exclusive > full_start {
        rects.push(TexelRect {
            x: 0,
            y: full_start,
            width,
            height: full_end_exclusive - full_start,
        });
    }

    if end_col + 1 < width {
        rects.push(TexelRect {
            x: 0,
            y: end_row,
            width: end_col + 1,
            height: 1,
        });
    }

    rects
}

/// Dirty state of one backing texture since the last [`SlugBuffer::take_dirty`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureDirty {
    /// Flat texel range `[lo, hi)` written since the last `take_dirty()`, or `None` if untouched.
    pub texel_range: Option<Range<u32>>,
    /// Backing-array generation counter; increments each time the texture doubled its rows.
    pub generation: u64,
    /// True if the backing array grew (doubled rows) since the last `take_dirty()`.
    pub grew: bool,
}

impl TextureDirty {
    /// Convert `texel_range` into at most three 2D upload rectangles for a `TEXTURE_WIDTH`-wide texture.
    pub fn rects(&self) -> Vec<TexelRect> {
        match &self.texel_range {
            Some(range) => texel_range_to_rects(range.clone(), TEXTURE_WIDTH),
            None => Vec::new(),
        }
    }

    /// True if either new texels were written or the texture grew.
    pub fn is_dirty(&self) -> bool {
        self.grew || self.texel_range.is_some()
    }
}

/// Combined dirty state for both Slug textures returned by [`SlugBuffer::take_dirty`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirtyUpdate {
    pub curves: TextureDirty,
    pub glyphmap: TextureDirty,
}

impl DirtyUpdate {
    /// True if either texture needs GPU work (reallocation or partial upload).
    pub fn any(&self) -> bool {
        self.curves.is_dirty() || self.glyphmap.is_dirty()
    }
}

/// Read-only view of one RGBA32Uint texture's backing array and dimensions.
#[derive(Clone, Copy, Debug)]
pub struct TextureBacking<'a> {
    pub data: &'a [u32],
    pub width: u32,
    pub height: u32,
    pub generation: u64,
}

/// Growable backing arrays for the Slug `curves` and `glyphmap` textures.
///
/// Both arrays allocate full rows of `TEXTURE_WIDTH` RGBA32Uint texels and double their row
/// count when an append would exceed capacity. Slot 0 is always reserved as `[0, 0, 0, 0]`.
pub struct SlugBuffer {
    curve_data: Vec<u32>,
    curve_rows: u32,
    curve_texels: u32,
    curve_count: u32,
    curve_gen: u64,
    curve_reported_gen: u64,
    curve_dirty_lo: u32,
    curve_dirty_hi: u32,

    map_data: Vec<u32>,
    map_rows: u32,
    max_slot: u32,
    map_gen: u64,
    map_reported_gen: u64,
    map_dirty_lo: u32,
    map_dirty_hi: u32,
}

impl Default for SlugBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SlugBuffer {
    /// Create a fresh buffer with default initial capacities (`CURVE_INITIAL_ROWS`, `MAP_INITIAL_ROWS`).
    pub fn new() -> Self {
        Self::with_initial_rows(CURVE_INITIAL_ROWS, MAP_INITIAL_ROWS)
    }

    /// Create a fresh buffer with explicit initial row capacities (each clamped to at least 1).
    pub fn with_initial_rows(curve_rows: u32, map_rows: u32) -> Self {
        let curve_rows = curve_rows.max(1);
        let map_rows = map_rows.max(1);
        let curve_len = (TEXTURE_WIDTH as usize) * (curve_rows as usize) * (U32S_PER_TEXEL as usize);
        let map_len = (TEXTURE_WIDTH as usize) * (map_rows as usize) * (U32S_PER_TEXEL as usize);
        Self {
            curve_data: vec![0u32; curve_len],
            curve_rows,
            curve_texels: 0,
            curve_count: 0,
            curve_gen: 0,
            curve_reported_gen: 0,
            curve_dirty_lo: u32::MAX,
            curve_dirty_hi: 0,

            map_data: vec![0u32; map_len],
            map_rows,
            max_slot: 0,
            map_gen: 0,
            map_reported_gen: 0,
            map_dirty_lo: u32::MAX,
            map_dirty_hi: 0,
        }
    }

    /// Number of curves stored so far.
    #[inline]
    pub fn curve_count(&self) -> u32 {
        self.curve_count
    }

    /// Highest slot index written so far.
    #[inline]
    pub fn max_slot(&self) -> u32 {
        self.max_slot
    }

    /// Current curve texture backing array and dimensions.
    pub fn curve_texture(&self) -> TextureBacking<'_> {
        TextureBacking {
            data: &self.curve_data,
            width: TEXTURE_WIDTH,
            height: self.curve_rows,
            generation: self.curve_gen,
        }
    }

    /// Current glyph-map texture backing array and dimensions.
    pub fn glyphmap_texture(&self) -> TextureBacking<'_> {
        TextureBacking {
            data: &self.map_data,
            width: TEXTURE_WIDTH,
            height: self.map_rows,
            generation: self.map_gen,
        }
    }

    /// Append `curves` and write slot `slot`'s glyph-map entry `[curve_start, curves.len(), mode, arg]`.
    ///
    /// Slot 0 is reserved for the blank glyph and ignored if passed.
    pub fn write_outline_slot(&mut self, slot: u32, curves: &[PackedCurve], mode: u32, arg: u32) {
        if slot == 0 {
            return;
        }
        let curve_start = self.curve_count;
        let count = curves.len() as u32;
        if count > 0 {
            let at_texel = self.curve_texels;
            let end_texel = at_texel + count * TEXELS_PER_CURVE;
            self.grow_curves(end_texel);

            let mut dst = (at_texel as usize) * (U32S_PER_TEXEL as usize);
            for curve in curves {
                let words = curve.to_texels();
                self.curve_data[dst..dst + U32S_PER_CURVE as usize].copy_from_slice(&words);
                dst += U32S_PER_CURVE as usize;
            }
            self.curve_dirty_lo = self.curve_dirty_lo.min(at_texel);
            self.curve_dirty_hi = self.curve_dirty_hi.max(end_texel);
            self.curve_texels = end_texel;
            self.curve_count += count;
        }
        self.write_map_entry(slot, [curve_start, count, mode, arg]);
    }

    /// Write a raw glyph-map entry `[curve_start, curve_count, mode, arg]` for `slot`.
    ///
    /// Useful for non-outline glyph sources such as colour emoji (`[0, 0, 1, emoji_cell]`).
    /// Slot 0 is reserved and ignored.
    pub fn write_map_entry(&mut self, slot: u32, entry: [u32; 4]) {
        if slot == 0 {
            return;
        }
        self.grow_map(slot);
        let offset = (slot as usize) * (U32S_PER_TEXEL as usize);
        self.map_data[offset..offset + 4].copy_from_slice(&entry);
        self.map_dirty_lo = self.map_dirty_lo.min(slot);
        self.map_dirty_hi = self.map_dirty_hi.max(slot + 1);
        self.max_slot = self.max_slot.max(slot);
    }

    /// Ensure the glyph-map texture has capacity for `slot` (leaving it `[0, 0, 0, 0]` if unwritten).
    pub fn reserve_slot(&mut self, slot: u32) {
        if slot > 0 {
            self.grow_map(slot);
        }
    }

    /// Read back a slot's glyph-map entry `[curve_start, curve_count, mode, arg]`.
    pub fn map_entry(&self, slot: u32) -> [u32; 4] {
        let offset = (slot as usize) * (U32S_PER_TEXEL as usize);
        if offset + 4 > self.map_data.len() {
            return [0, 0, 0, 0];
        }
        [
            self.map_data[offset],
            self.map_data[offset + 1],
            self.map_data[offset + 2],
            self.map_data[offset + 3],
        ]
    }

    /// Read back the packed curves of an outline slot.
    pub fn slot_curves(&self, slot: u32) -> Vec<PackedCurve> {
        let [start, count, _mode, _arg] = self.map_entry(slot);
        let mut out = Vec::with_capacity(count as usize);
        let base = (start as usize) * (U32S_PER_CURVE as usize);
        for i in 0..count as usize {
            let o = base + i * (U32S_PER_CURVE as usize);
            let mut words = [0u32; U32S_PER_CURVE as usize];
            words.copy_from_slice(&self.curve_data[o..o + U32S_PER_CURVE as usize]);
            out.push(PackedCurve::from_texels(words));
        }
        out
    }

    /// Return the texel spans written since the last call (and whether either texture grew),
    /// then reset the dirty markers.
    pub fn take_dirty(&mut self) -> DirtyUpdate {
        let curve_range = if self.curve_dirty_hi > self.curve_dirty_lo {
            Some(self.curve_dirty_lo..self.curve_dirty_hi)
        } else {
            None
        };
        let map_range = if self.map_dirty_hi > self.map_dirty_lo {
            Some(self.map_dirty_lo..self.map_dirty_hi)
        } else {
            None
        };
        let curve_grew = self.curve_gen != self.curve_reported_gen;
        let map_grew = self.map_gen != self.map_reported_gen;

        self.curve_dirty_lo = u32::MAX;
        self.curve_dirty_hi = 0;
        self.map_dirty_lo = u32::MAX;
        self.map_dirty_hi = 0;
        self.curve_reported_gen = self.curve_gen;
        self.map_reported_gen = self.map_gen;

        DirtyUpdate {
            curves: TextureDirty {
                texel_range: curve_range,
                generation: self.curve_gen,
                grew: curve_grew,
            },
            glyphmap: TextureDirty {
                texel_range: map_range,
                generation: self.map_gen,
                grew: map_grew,
            },
        }
    }

    fn grow_curves(&mut self, needed_texels: u32) {
        let cap_texels = self.curve_rows * TEXTURE_WIDTH;
        if needed_texels <= cap_texels {
            return;
        }
        let mut rows = self.curve_rows;
        while rows * TEXTURE_WIDTH < needed_texels {
            rows *= 2;
        }
        let new_len = (TEXTURE_WIDTH as usize) * (rows as usize) * (U32S_PER_TEXEL as usize);
        self.curve_data.resize(new_len, 0);
        self.curve_rows = rows;
        self.curve_gen += 1;
    }

    fn grow_map(&mut self, slot: u32) {
        let needed_texels = slot + 1;
        let cap_texels = self.map_rows * TEXTURE_WIDTH;
        if needed_texels <= cap_texels {
            return;
        }
        let mut rows = self.map_rows;
        while rows * TEXTURE_WIDTH < needed_texels {
            rows *= 2;
        }
        let new_len = (TEXTURE_WIDTH as usize) * (rows as usize) * (U32S_PER_TEXEL as usize);
        self.map_data.resize(new_len, 0);
        self.map_rows = rows;
        self.map_gen += 1;
    }
}
