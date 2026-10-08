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
}

/// Output of Pass 1 prepass on a single chunk.
#[repr(align(64))]
#[derive(Clone, Copy, Debug)]
pub struct ChunkPrepass {
    pub survivor_count: u32,
    pub leader_count: u32,
    pub max_row_extent: f64,
    pub has_cluster: bool,
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
}

#[derive(Clone, Copy)]
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
