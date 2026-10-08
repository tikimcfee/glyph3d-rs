//! Chunking and parallel work distribution for HyperLayout.
//!
//! Large files (> CHUNK_THRESHOLD_BYTES) are sliced at line boundaries (`\n`)
//! into independent chunks. Because every chunk begins at column 0 with clean
//! line advance and independent syntax coloring state, chunks can be prepassed
//! and laid out in parallel across all CPU cores, eliminating Amdahl's Law
//! single-core tail latency bottlenecks on multi-megabyte files (such as parser.c).

use crate::layout::LayoutItem;

/// Chunk threshold: files larger than 64 KiB are split at line boundaries.
pub const CHUNK_THRESHOLD_BYTES: usize = 64 * 1024;

/// A line-aligned chunk of a LayoutItem.
#[derive(Clone, Copy, Debug)]
pub struct LayoutChunk<'a> {
    pub item_index: usize,
    pub bytes: &'a [u8],
    /// Byte offset within the item's original byte slice.
    pub byte_offset: usize,
}

/// Slices layout items into line-aligned chunks.
///
/// Returns:
/// - `chunks`: Flat array of all chunks across all items.
/// - `item_chunk_ranges`: Slice index ranges in `chunks` belonging to each item.
pub fn slice_items_into_chunks<'a>(
    items: &[LayoutItem<'a>],
) -> (Vec<LayoutChunk<'a>>, Vec<std::ops::Range<usize>>) {
    let mut chunks = Vec::with_capacity(items.len() + 256);
    let mut item_chunk_ranges = Vec::with_capacity(items.len());

    for (item_index, item) in items.iter().enumerate() {
        let chunk_start_index = chunks.len();
        let bytes = item.bytes;
        let total_bytes = bytes.len();

        if total_bytes <= CHUNK_THRESHOLD_BYTES {
            chunks.push(LayoutChunk {
                item_index,
                bytes,
                byte_offset: 0,
            });
        } else {
            let mut current_offset = 0usize;
            while current_offset < total_bytes {
                let target_offset = current_offset + CHUNK_THRESHOLD_BYTES;
                let chunk_end = if target_offset >= total_bytes {
                    total_bytes
                } else {
                    match memchr::memchr(b'\n', &bytes[target_offset..]) {
                        Some(newline_offset) => target_offset + newline_offset + 1,
                        None => total_bytes,
                    }
                };
                chunks.push(LayoutChunk {
                    item_index,
                    bytes: &bytes[current_offset..chunk_end],
                    byte_offset: current_offset,
                });
                current_offset = chunk_end;
            }
        }
        item_chunk_ranges.push(chunk_start_index..chunks.len());
    }

    (chunks, item_chunk_ranges)
}
