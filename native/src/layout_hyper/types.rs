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
}

#[derive(Clone, Copy)]
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
