//! A synthetic atlas trie for tests (this crate's and any caller's): ASCII
//! resolves to glyph = byte at one cell, a handful of non-ASCII codepoints
//! and three sequences stand in for the atlas's. Nothing here reads a file.
//!
//! | codepoint(s)                 | glyph | cells | note                          |
//! |------------------------------|-------|-------|-------------------------------|
//! | 0x20..=0x7E                  | byte  | 1     | printable ASCII               |
//! | other ASCII (controls, DEL)  | 0     | 1     | a cell, no slot               |
//! | U+00E9 é                     | 300   | 1     | two-byte                      |
//! | U+4E2D 中                    | 400   | 2     | three-byte, wide              |
//! | U+200D ZWJ                   | 0     | 1     | leader mode: a blank cell     |
//! | U+20E3 keycap combiner       | 0     | 1     | leader mode: a blank cell     |
//! | U+FE0F VS16                  | 0     | 1     | leader mode: a blank cell     |
//! | U+1F468, U+1F469             | 500/1 | 2     | four-byte emoji               |
//! | everything else              | 0     | 1     | the missing block (flag 1)    |
//! | `# U+20E3`, `1 U+20E3`       | 9000/1| 2     | keycaps (ASCII heads)         |
//! | `U+1F468 ZWJ U+1F469`        | 9002  | 2     | a ZWJ family                  |

use crate::TrieUpload;

pub const EM_HEIGHT_FU: u32 = 2320;
pub const ADVANCE_FU: u32 = 1229;

/// The trie above.
pub fn synthetic_trie() -> TrieUpload {
    let shift = 8u32;
    let block_count_index = 0x110000usize >> shift; // 4352
    // Blocks: 0 missing, 1 = cp 0x00..0xFF, 2 = 0x2000.., 3 = 0x4E00.., 4 = 0xFE00.., 5 = 0x1F400..
    let mut blocks: Vec<u32> = Vec::new();
    let mut push_block = |f: &dyn Fn(u32) -> (u32, u32, u32)| {
        for k in 0..256u32 {
            let (glyph, adv, flags) = f(k);
            blocks.extend_from_slice(&[glyph, adv, EM_HEIGHT_FU, flags]);
        }
    };
    push_block(&|_| (0, ADVANCE_FU, 1));
    push_block(&|k| match k {
        0x20..=0x7E => (k, ADVANCE_FU, 0),
        0xE9 => (300, ADVANCE_FU, 0),
        _ if k < 0x80 => (0, ADVANCE_FU, 0),
        _ => (0, ADVANCE_FU, 1),
    });
    push_block(&|k| match 0x2000 + k {
        0x200D | 0x20E3 => (0, ADVANCE_FU, 0),
        _ => (0, ADVANCE_FU, 1),
    });
    push_block(&|k| if 0x4E00 + k == 0x4E2D { (400, 2 * ADVANCE_FU, 0) } else { (0, ADVANCE_FU, 1) });
    push_block(&|k| if 0xFE00 + k == 0xFE0F { (0, ADVANCE_FU, 0) } else { (0, ADVANCE_FU, 1) });
    push_block(&|k| match 0x1F400 + k {
        0x1F468 => (500, 2 * ADVANCE_FU, 0),
        0x1F469 => (501, 2 * ADVANCE_FU, 0),
        _ => (0, ADVANCE_FU, 1),
    });
    let mut block_index = vec![0u32; block_count_index];
    block_index[0x00] = 1;
    block_index[0x20] = 2;
    block_index[0x4E] = 3;
    block_index[0xFE] = 4;
    block_index[0x1F4] = 5;

    let seq_max = 3u32;
    // Sorted prefix-lexicographically: [slot, len, cps…] padded to seq_max.
    let sequences: Vec<u32> = vec![
        9000, 2, 0x23, 0x20E3, 0, //
        9001, 2, 0x31, 0x20E3, 0, //
        9002, 3, 0x1F468, 0x200D, 0x1F469,
    ];
    let mut seq_first_bitmap = vec![0u32; 0x110000 / 32];
    for cp in [0x23u32, 0x31, 0x1F468] {
        seq_first_bitmap[(cp >> 5) as usize] |= 1 << (cp & 31);
    }
    TrieUpload {
        block_shift: shift,
        block_index,
        blocks,
        entry_stride: 4,
        sequences,
        seq_max,
        seq_first_bitmap,
        em_height_fu: EM_HEIGHT_FU,
        primary_advance_fu: ADVANCE_FU,
        bitmap_advance_fu: 2 * ADVANCE_FU as i32,
    }
}
