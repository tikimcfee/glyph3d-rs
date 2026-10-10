use crate::layout::{FileTintAccum, ItemPlacement};

pub struct Pass2DeviceOutput {
    pub placements: Vec<ItemPlacement>,
    pub file_tints: Vec<FileTintAccum>,
    pub file_blocks: Vec<Vec<crate::glyph_scene::BlockCull>>,
}

#[repr(align(64))]
#[derive(Clone, Copy, Debug)]
pub struct ItemPrepass {
    pub survivor_count: u32,
    pub leader_count: u32,
    pub max_row_extent: f64,
    /// Rows the item's records occupy (`max row + 1`, counting a trailing
    /// unterminated line): the sum of `rows_for_line` over its lines. The
    /// Derived emitter reserves exactly this many line-table entries per item
    /// BEFORE Pass 2, so a slot's `line_idx` is `line_base + row` with no
    /// fix-up pass. Pass 2 asserts every row it emits is below it.
    pub row_count: u32,
    /// Whether this item contains any static zero characters or cluster sequence candidates.
    pub has_cluster: bool,
    /// Whether any of its glyphs draws from the emoji sheet
    /// ([`crate::atlas::TrieTable::is_emoji_glyph`]): such an item's tint
    /// needs its `(glyph, colour)` pairs, which a staging path that writes
    /// write-combined memory must capture during emission (C22).
    pub has_emoji: bool,
    /// The widest line in LEADERS (its newline's column; trailers counted),
    /// over every line of the item, chunk cuts joined. Read once per load by
    /// `derived_lane_limits`: a column-paged item's widest column page is
    /// this over `page_cols`, and the Derived slot's row lane holds 8 bits
    /// of it.
    pub max_line_cols: u32,
}

/// Output of Pass 1 prepass on a single chunk.
#[repr(align(64))]
#[derive(Clone, Copy, Debug)]
pub struct ChunkPrepass {
    pub survivor_count: u32,
    pub leader_count: u32,
    pub max_row_extent: f64,
    pub has_cluster: bool,
    pub has_emoji: bool,
    /// Total rows from lines completed inside this chunk.
    pub completed_rows: u32,
    /// Does this chunk contain at least one newline?
    pub has_newline: bool,
    /// If has_newline is false: leaders in this chunk.
    pub delta_col: i64,
    pub delta_line_adv: f64,
    pub delta_seg_adv: f32,
    /// If has_newline is true: leaders in the segment before the first newline.
    pub first_seg_col: i64,
    /// If has_newline is true: leaders in the segment after the last newline.
    pub last_seg_col: i64,
    pub last_seg_line_adv: f64,
    pub last_seg_seg_adv: f32,
    /// The widest line of this chunk in leaders, measured once per line (a
    /// line the chunk only continues counts the part it holds; the
    /// aggregation adds the inherited column).
    pub max_line_cols: u32,
}

#[derive(Clone, Copy)]
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
