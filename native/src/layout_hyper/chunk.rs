//! Chunking and parallel work distribution for HyperLayout.
//!
//! Large files (> CHUNK_THRESHOLD_BYTES) are sliced at line boundaries (`\n`)
//! into independent chunks, so chunks can be prepassed and laid out in
//! parallel across all CPU cores, eliminating Amdahl's Law single-core tail
//! latency bottlenecks on multi-megabyte files (such as parser.c).
//!
//! A line with no newline within `MAX_NEWLINE_SEARCH_BYTES` of the threshold
//! (a minified file) is cut INSIDE the line, before an ASCII byte. Such a
//! chunk does not start at column 0: it inherits the column, line advance and
//! segment advance its line had reached, which Pass 1's aggregation computes
//! exactly (`aggregate_chunk_prepasses`) — the running f32 segment sum the
//! fold uses, not a product.

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

/// Lifetime-free specification of a chunk within an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkDef {
    pub item_index: usize,
    pub byte_offset: usize,
    pub byte_len: usize,
}

/// Slices byte buffers into lifetime-free ChunkDefs.
pub fn slice_byte_buffers_into_chunk_defs(
    buffers: &[impl AsRef<[u8]>],
) -> (Vec<ChunkDef>, Vec<std::ops::Range<usize>>) {
    let mut chunk_defs = Vec::with_capacity(buffers.len() + 256);
    let mut item_chunk_ranges = Vec::with_capacity(buffers.len());

    for (item_index, item) in buffers.iter().enumerate() {
        let chunk_start_index = chunk_defs.len();
        let bytes = item.as_ref();
        let total_bytes = bytes.len();

        if total_bytes <= CHUNK_THRESHOLD_BYTES {
            chunk_defs.push(ChunkDef {
                item_index,
                byte_offset: 0,
                byte_len: total_bytes,
            });
        } else {
            let mut current_offset = 0usize;

            while current_offset < total_bytes {
                let remaining = total_bytes - current_offset;
                if remaining <= CHUNK_THRESHOLD_BYTES {
                    chunk_defs.push(ChunkDef {
                        item_index,
                        byte_offset: current_offset,
                        byte_len: remaining,
                    });
                    break;
                }

                let target_offset = current_offset + CHUNK_THRESHOLD_BYTES;
                let search_window_end = (target_offset + MAX_NEWLINE_SEARCH_BYTES).min(total_bytes);
                let search_slice = &bytes[target_offset..search_window_end];

                let chunk_end = match memchr::memchr(b'\n', search_slice) {
                    Some(newline_offset) => target_offset + newline_offset + 1,
                    // An INTRA-LINE cut, and only ever BEFORE AN ASCII BYTE.
                    // No cluster sequence has an ASCII member after its first
                    // (asserted when the atlas loads), so no sequence and no
                    // trailer span reaches across such a byte: every chunk
                    // then resolves its bytes exactly as a walk of the whole
                    // item does, which is what lets Pass 1 count survivors per
                    // chunk and the device Pass 2 emit per chunk. A cut at a
                    // mere codepoint boundary split sequences (C15, measured
                    // 2026-10-09: a ZWJ family cut before its first ZWJ laid
                    // out as three glyphs on the device and left the host
                    // arena three committed slots it never wrote). A run with
                    // no ASCII byte at all is not cut.
                    None => match bytes[target_offset..].iter().position(|&b| b < 0x80) {
                        Some(ascii_offset) => target_offset + ascii_offset,
                        None => total_bytes,
                    },
                };

                chunk_defs.push(ChunkDef {
                    item_index,
                    byte_offset: current_offset,
                    byte_len: chunk_end - current_offset,
                });

                current_offset = chunk_end;
            }
        }
        item_chunk_ranges.push(chunk_start_index..chunk_defs.len());
    }

    (chunk_defs, item_chunk_ranges)
}

/// Slices layout items into chunks (line-aligned when newlines exist, or wrap-aware intra-line for one-liners).
///
/// Returns:
/// - `chunks`: Flat array of all chunks across all items.
/// - `item_chunk_ranges`: Slice index ranges in `chunks` belonging to each item.
pub fn slice_items_into_chunks<'a>(
    items: &[LayoutItem<'a>],
) -> (Vec<LayoutChunk<'a>>, Vec<std::ops::Range<usize>>) {
    let byte_slices: Vec<&[u8]> = items.iter().map(|it| it.bytes).collect();
    let (chunk_defs, item_chunk_ranges) = slice_byte_buffers_into_chunk_defs(&byte_slices);
    let chunks = chunk_defs
        .into_iter()
        .map(|def| LayoutChunk {
            item_index: def.item_index,
            bytes: &items[def.item_index].bytes[def.byte_offset..def.byte_offset + def.byte_len],
            byte_offset: def.byte_offset,
        })
        .collect();

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

    /// C15 (2026-10-09): an intra-line cut lands only before an ASCII byte,
    /// so a sequence that straddles the 64 KiB target stays whole in one
    /// chunk; and a run with no ASCII byte is not cut at all.
    #[test]
    fn an_intra_line_cut_never_splits_a_sequence() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".as_bytes();
        let mut text = vec![b'a'; CHUNK_THRESHOLD_BYTES - 4];
        text.extend_from_slice(family); // the target falls on its first ZWJ
        text.extend_from_slice(&[b'b'; 2 * CHUNK_THRESHOLD_BYTES]);
        let (defs, _) = slice_byte_buffers_into_chunk_defs(&[&text]);
        assert!(defs.len() >= 3, "{defs:?}");
        assert_eq!(defs[0].byte_len, CHUNK_THRESHOLD_BYTES - 4 + family.len(), "the family must stay in chunk 0");
        for d in &defs[1..] {
            assert!(text[d.byte_offset] < 0x80, "cut at a non-ASCII byte: {d:?}");
        }

        let cjk = "\u{4E16}".repeat(3 * CHUNK_THRESHOLD_BYTES / 3);
        let (defs, _) = slice_byte_buffers_into_chunk_defs(&[cjk.as_bytes()]);
        assert_eq!(defs.len(), 1, "a run with no ASCII byte must not be cut: {defs:?}");
    }
}

