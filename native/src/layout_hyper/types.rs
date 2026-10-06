use crate::layout::{FileTintAccum, ItemPlacement};

pub struct Pass2DeviceOutput {
    pub placements: Vec<ItemPlacement>,
    pub file_tints: Vec<FileTintAccum>,
    pub file_blocks: Vec<Vec<crate::glyph_scene::BlockCull>>,
}

#[derive(Clone, Copy)]
pub struct ItemPrepass {
    pub survivor_count: u32,
    pub max_row_extent: f64,
    /// Rows the item's records occupy (`max row + 1`, counting a trailing
    /// unterminated line): the sum of `rows_for_line` over its lines. The
    /// Derived emitter reserves exactly this many line-table entries per item
    /// BEFORE Pass 2, so a slot's `line_idx` is `line_base + row` with no
    /// fix-up pass. Pass 2 asserts every row it emits is below it.
    pub row_count: u32,
}

#[derive(Clone, Copy)]
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
