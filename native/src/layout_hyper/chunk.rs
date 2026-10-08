//! Chunking and parallel work distribution for HyperLayout.
//!
//! Large files (> CHUNK_THRESHOLD_BYTES) are sliced at line boundaries (`\n`)
//! into independent chunks. Because every chunk begins at column 0 with clean
//! line advance and independent syntax coloring state, chunks can be prepassed
//! and laid out in parallel across all CPU cores, eliminating Amdahl's Law
//! single-core tail latency bottlenecks on multi-megabyte files (such as parser.c).

use crate::layout::LayoutItem;

/// Chunk threshold: files larger than 64 KiB are split.
pub const CHUNK_THRESHOLD_BYTES: usize = 64 * 1024;
/// Maximum forward search window for a newline before falling back to UTF-8 intra-line slicing.
pub const MAX_NEWLINE_SEARCH_BYTES: usize = 64 * 1024;

/// A chunk of a LayoutItem, which is either line-aligned or a continuation of an ultra-long line.
#[derive(Clone, Copy, Debug)]
pub struct LayoutChunk<'a> {
    pub item_index: usize,
    pub bytes: &'a [u8],
    /// Byte offset within the item's original byte slice.
    pub byte_offset: usize,
}

/// Slices layout items into chunks (line-aligned when newlines exist, or wrap-aware intra-line for one-liners).
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
                let remaining = total_bytes - current_offset;
                if remaining <= CHUNK_THRESHOLD_BYTES {
                    chunks.push(LayoutChunk {
                        item_index,
                        bytes: &bytes[current_offset..total_bytes],
                        byte_offset: current_offset,
                    });
                    break;
                }

                let target_offset = current_offset + CHUNK_THRESHOLD_BYTES;
                let search_window_end = (target_offset + MAX_NEWLINE_SEARCH_BYTES).min(total_bytes);
                let search_slice = &bytes[target_offset..search_window_end];

                let chunk_end = match memchr::memchr(b'\n', search_slice) {
                    Some(newline_offset) => {
                        target_offset + newline_offset + 1
                    }
                    None => {
                        // No newline found within the search window — this is an ultra-long line!
                        // Slicing at target_offset, aligned to next UTF-8 character boundary.
                        let mut cut = target_offset;
                        while cut < total_bytes && (bytes[cut] & 0xC0) == 0x80 {
                            cut += 1;
                        }
                        cut
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ItemParams;

    #[test]
    fn test_short_file_single_chunk() {
        let text = b"hello world\nline 2\n";
        let params = ItemParams::default();
        let items = [LayoutItem {
            bytes: text,
            paint: crate::layout::Paint::Flat(0),
            params,
            group_id: 0,
        }];

        let (chunks, ranges) = slice_items_into_chunks(&items);
        assert_eq!(chunks.len(), 1);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..1);
        assert_eq!(chunks[0].bytes, text);
        assert_eq!(chunks[0].byte_offset, 0);
    }

    #[test]
    fn test_ultra_long_line_chunking_reconstruction() {
        // Construct a 250 KiB single line with no newlines, containing multi-byte UTF-8 sequences
        let mut text = Vec::with_capacity(250 * 1024);
        while text.len() < 250 * 1024 {
            text.extend_from_slice("The quick brown fox jumps over the lazy dog! 🚀 🦀 世界 ".as_bytes());
        }

        let params = ItemParams::default();
        let items = [LayoutItem {
            bytes: &text,
            paint: crate::layout::Paint::Flat(0),
            params,
            group_id: 0,
        }];

        let (chunks, ranges) = slice_items_into_chunks(&items);
        assert!(chunks.len() >= 3, "Expected at least 3 chunks for 250 KiB single line, got {}", chunks.len());
        assert_eq!(ranges[0], 0..chunks.len());

        // Verify each chunk is valid UTF-8
        let mut reconstructed = Vec::with_capacity(text.len());
        for chunk in &chunks {
            std::str::from_utf8(chunk.bytes).expect("Each chunk must be valid UTF-8 boundary");
            reconstructed.extend_from_slice(chunk.bytes);
        }
        assert_eq!(reconstructed, text, "Concatenated chunks must match original bytes byte-for-byte");
    }
}

