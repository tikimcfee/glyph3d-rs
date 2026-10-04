//! Hierarchical subsegment block culling and line offset calculation.

use super::FileView;
use crate::glyph_scene::{
    BlockCull, GlyphInstance, RenderSlot, BLOCK_CULL_PAD_MAX, BLOCK_CULL_PAD_MIN,
    SUBSEG_BLOCK_SIZE,
};

/// Live loop resolves envelope folds against owned content with it.
pub fn line_starts_of(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1),
    );
    starts
}

/// The line index containing `byte` (the LAST start at/before it). Pub:
/// the style walk resolves folded records with it.
pub fn line_of_byte(byte: usize, line_starts: &[usize]) -> u32 {
    match line_starts.binary_search(&byte) {
        Ok(i) => i as u32,
        Err(0) => 0,
        Err(i) => (i - 1) as u32,
    }
}

pub(super) fn build_file_blocks(
    v: &FileView,
    mapped_slots: Option<&[RenderSlot]>,
    chunks: &[&[GlyphInstance]],
) -> Vec<BlockCull> {
    if v.slot_count <= SUBSEG_BLOCK_SIZE {
        return Vec::new();
    }
    let num_blocks = v.slot_count.div_ceil(SUBSEG_BLOCK_SIZE);
    let mut blocks = Vec::with_capacity(num_blocks);

    if let Some(slots) = mapped_slots {
        let file_slots = &slots[v.slot_base..v.slot_base + v.slot_count];
        for (b_idx, chunk) in file_slots.chunks(SUBSEG_BLOCK_SIZE).enumerate() {
            let b_start = v.slot_base + b_idx * SUBSEG_BLOCK_SIZE;
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut min_z = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            let mut max_z = f32::NEG_INFINITY;
            for s in chunk {
                let qw = s.advance.max(s.height);
                let half_h = 0.5 * s.height;
                let px = s.pos[0];
                let py = s.pos[1];
                let pz = s.pos[2];

                if px < min_x {
                    min_x = px;
                }
                let x_hi = px + qw;
                if x_hi > max_x {
                    max_x = x_hi;
                }
                let y_lo = py - half_h;
                let y_hi = py + half_h;
                if y_lo < min_y {
                    min_y = y_lo;
                }
                if y_hi > max_y {
                    max_y = y_hi;
                }
                if pz < min_z {
                    min_z = pz;
                }
                if pz > max_z {
                    max_z = pz;
                }
            }
            if min_x <= max_x && min_y <= max_y {
                blocks.push(BlockCull {
                    min: [
                        v.offset[0] + min_x - BLOCK_CULL_PAD_MIN[0],
                        v.offset[1] + min_y - BLOCK_CULL_PAD_MIN[1],
                        v.offset[2] + min_z - BLOCK_CULL_PAD_MIN[2],
                    ],
                    max: [
                        v.offset[0] + max_x + BLOCK_CULL_PAD_MAX[0],
                        v.offset[1] + max_y + BLOCK_CULL_PAD_MAX[1],
                        v.offset[2] + max_z + BLOCK_CULL_PAD_MAX[2],
                    ],
                    slot_base: b_start as u32,
                    slot_count: chunk.len() as u32,
                });
            }
        }
    } else if !chunks.is_empty() {
        let mut base = 0usize;
        let want = v.slot_base..v.slot_base + v.slot_count;
        for c in chunks {
            let lo = want.start.max(base);
            let hi = want.end.min(base + c.len());
            if lo < hi {
                let slice = &c[lo - base..hi - base];
                for (b_idx, chunk) in slice.chunks(SUBSEG_BLOCK_SIZE).enumerate() {
                    let b_start = lo + b_idx * SUBSEG_BLOCK_SIZE;
                    let mut min_x = f32::INFINITY;
                    let mut min_y = f32::INFINITY;
                    let mut min_z = f32::INFINITY;
                    let mut max_x = f32::NEG_INFINITY;
                    let mut max_y = f32::NEG_INFINITY;
                    let mut max_z = f32::NEG_INFINITY;
                    for s in chunk {
                        let qw = s.advance.max(s.height);
                        let half_h = 0.5 * s.height;
                        let px = s.pos[0];
                        let py = s.pos[1];
                        let pz = s.pos[2];

                        if px < min_x {
                            min_x = px;
                        }
                        let x_hi = px + qw;
                        if x_hi > max_x {
                            max_x = x_hi;
                        }
                        let y_lo = py - half_h;
                        let y_hi = py + half_h;
                        if y_lo < min_y {
                            min_y = y_lo;
                        }
                        if y_hi > max_y {
                            max_y = y_hi;
                        }
                        if pz < min_z {
                            min_z = pz;
                        }
                        if pz > max_z {
                            max_z = pz;
                        }
                    }
                    if min_x <= max_x && min_y <= max_y {
                        blocks.push(BlockCull {
                            min: [
                                v.offset[0] + min_x - BLOCK_CULL_PAD_MIN[0],
                                v.offset[1] + min_y - BLOCK_CULL_PAD_MIN[1],
                                v.offset[2] + min_z - BLOCK_CULL_PAD_MIN[2],
                            ],
                            max: [
                                v.offset[0] + max_x + BLOCK_CULL_PAD_MAX[0],
                                v.offset[1] + max_y + BLOCK_CULL_PAD_MAX[1],
                                v.offset[2] + max_z + BLOCK_CULL_PAD_MAX[2],
                            ],
                            slot_base: b_start as u32,
                            slot_count: chunk.len() as u32,
                        });
                    }
                }
            }
            base += c.len();
        }
    }

    blocks
}
