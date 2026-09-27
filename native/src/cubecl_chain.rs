//! CubeCL scan chain — dev-only (`--cubecl-chain-check`), the standard
//! parallel structure.
//!
//!   tileScan (rake + workgroup Blelloch) -> spineScan (one cube)
//!                -> apply (rake + Blelloch + chase) -> resolveX
//!                -> deriveStride -> paginate
//!
//! The note-16 phase-2 skeleton transcribed the Mojo device chain
//! line-for-line — thread-per-chunk serial 64-byte folds, a single-thread
//! spine — which proved the monoid but measured transcription quality, not
//! the algorithm. This is the textbook hierarchical scan instead: each cube
//! owns a `units x rake`-byte tile; every unit rakes its `rake` bytes into
//! one monoid element; a workgroup Blelloch scan over the per-unit partials
//! (shared memory, `sync_cube` between rounds) produces exclusive prefixes;
//! the spine is one cube doing the same over tile totals; `apply` re-rakes
//! and chases its bytes seeded with (global tile prefix + own micro prefix).
//! The CPU reference stays `scan.rs::run_scan_pipeline` — a DIFFERENT tree
//! shape at a different tuning, so agreement is associativity checked in
//! situ, the same evidence pattern as scan.rs's own chunk-sweep tests.
//!
//! Precision contract, deliberately restated for this structure: integer
//! lanes (lc/wc/otb) diff BIT-EXACT — the monoid's count lanes are exact and
//! any order-preserving association agrees. `tail_adv` is f32-per-add and the
//! Blelloch tree REASSOCIATES those adds within a tile, so line_advance
//! leaves bit-parity with scan.rs and lands in the ORACLE's existing 1e-4
//! eps tier (scan-vs-serial-fold was already eps there — only this
//! instrument's same-tuning bit check is loosened, max deviation reported).
//! The fold>0 X lanes stay bit-exact: resolve_x still re-sums each wrap
//! segment serially, in the same left-fold order as the serial recurrence.
//!
//! Two tree-specific invariants, both load-bearing:
//! - PAD elements (tail units of the last tile, empty spine blocks) carry the
//!   wrap/mode in force at their position, NOT pure identity — combine copies
//!   wrap/mode off its RIGHT operand unconditionally, and a zero-wrap pad at
//!   the end of a tile would clobber the tile total's wrap and mis-junction
//!   every later combine that reads it.
//! - The exclusive prefix at the tree root is seeded with pure identity; that
//!   is safe because a prefix element's OWN wrap/mode lanes are never read —
//!   combine only reads them off the right operand, which is always a real
//!   leaf or a real-derived element on every path that matters.

use std::path::Path;

use cubecl::prelude::*;
use cubecl::wgpu::{AutoGraphicsApi, GraphicsApi, WgpuSetup};

use crate::fold::WrapMode;
use crate::text::ResolveGlyph;
use crate::gpu::GpuContext;
use crate::scan::{DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, run_scan_pipeline};

// ── lane layout (glyph-identity.json, hash-pinned) ──────────────────────────
const PARTIAL_COUNT_STRIDE: usize = 8;
const P_RESET: usize = 0;
const P_NL: usize = 1;
const P_GLYPHS: usize = 2;
const P_ROWS: usize = 3;
const P_HEAD_LEN: usize = 4;
const P_TAIL_LEN: usize = 5;
const P_WRAP: usize = 6;
const P_MODE: usize = 7;
/// The measure static is ADVANCE ONLY. Height is renderer statics the
/// scan never reads; carrying it here doubled the per-byte measure traffic.
const SM_STRIDE: usize = 1;
const SM_ADVANCE: usize = 0;
/// resolveX's per-cube shared reduction slots, item-relative from the item
/// at the cube's first byte. A 2 KB tile spanning more than this many items
/// (never in practice) takes the global-atomic overflow path instead.
const RESOLVE_SLOTS: usize = 16;
/// The trie's missing flag (fold::TRIE_FLAG_MISSING) and the decode's
/// passthrough lane (fixture::F_MISSING), both in the packed low byte.
const TRIE_FLAG_MISSING: u32 = 1;
const F_MISSING: u32 = 8;
/// The cluster trailer lane (fold::F_CLUSTER_TRAILER) — bit 16, inside the
/// packed byte lane. Nothing in the chain reads it; the corpus FLAGS
/// comparison is its only witness, so it rides anyway.
const F_CLUSTER_TRAILER: u32 = 16;

// ── dispatch 0: decode — thread per 4-byte word ──────────────────────────────
//
// The phase-3a port of fold::decode_all's leader-mode half. Every byte is
// INDEPENDENT: the lenient classifier reads only the byte's own bits
// (continuations self-identify, leads declare their length, continuation
// bytes are never validated — transcribed exactly), the codepoint reads at
// most three bytes forward, and the resolve is the same two dependent loads
// as TrieTable::lookup (block_index[cp >> shift], then entry
// (block<<shift)|(cp & 0xFF)). One thread per WORD so the packed flag word
// is written whole — four byte-lane writers to one u32 would race.
//
// Scope, deliberately: this produces the chain's inputs (packed fl, advance
// f32) only. gi/height are renderer statics the CPU still owns; the miss
// list is a CPU product concern; and cluster resolution is the separate
// phase-3b pass. The tables arrive PRE-CONVERTED to world units (fixtures
// store world values; the atlas path converts once at upload) so no
// device-side division — and no fast-math question — ever touches an
// advance bit.
#[cube(launch_unchecked)]
fn decode(
    bytes: &[u32],
    block_index: &[u32],
    blocks_m: &[f32],
    blocks_c: &[u32],
    fl: &mut [u32],
    sm: &mut [f32],
    #[comptime] block_shift: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    if w < fl.len() {
        let mut word = 0u32;
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                let b = byte_at(bytes, id, n);
                // sequence_length, transcribed: the lenient classifier.
                let len = if b & 0x80u32 == 0u32 {
                    1u32
                } else if b & 0xE0u32 == 0xC0u32 {
                    2u32
                } else if b & 0xF0u32 == 0xE0u32 {
                    3u32
                } else if b & 0xF8u32 == 0xF0u32 {
                    4u32
                } else {
                    0u32
                };
                if len > 0u32 {
                    // decode_codepoint_at, transcribed (reads past the end
                    // are zero, continuations never validated).
                    let b1 = byte_at(bytes, id + 1, n);
                    let b2 = byte_at(bytes, id + 2, n);
                    let b3 = byte_at(bytes, id + 3, n);
                    let cp = if len == 1u32 {
                        b
                    } else if len == 2u32 {
                        ((b & 0x1Fu32) << 6u32) | (b1 & 0x3Fu32)
                    } else if len == 3u32 {
                        ((b & 0x0Fu32) << 12u32) | ((b1 & 0x3Fu32) << 6u32) | (b2 & 0x3Fu32)
                    } else {
                        ((b & 0x07u32) << 18u32) | ((b1 & 0x3Fu32) << 12u32) | ((b2 & 0x3Fu32) << 6u32) | (b3 & 0x3Fu32)
                    };
                    let block = if cp <= 0x10FFFFu32 {
                        block_index[(cp >> block_shift) as usize]
                    } else {
                        0u32
                    };
                    let e = ((block << block_shift) | (cp & 0xFFu32)) as usize;
                    sm[id] = blocks_m[e * 2];
                    let flag = F_LEADER
                        | (if b == 10u32 {
                            F_NEWLINE
                        } else {
                            0u32
                        })
                        | (if blocks_c[e * 2 + 1] & TRIE_FLAG_MISSING != 0 {
                            F_MISSING
                        } else {
                            0u32
                        });
                    word |= flag << ((lane as u32) * 8u32);
                } else {
                    // decode_and_resolve zeroes the statics of a non-leader.
                    sm[id] = f32::from_bits(0u32);
                }
            }
            lane += 1;
        }
        fl[w] = word;
    }

}

// ── the cluster pass, phase 3b: probe (parallel) + chain (per item) ───────────
//
// Re-derived scan-shaped per the design session: the PROBE is pure per byte
// (its outcome depends only on the byte's own decode, forward bytes, the
// tables, and the item end — verified against resolve_clusters), so it runs
// one thread per WORD like decode. The CONSUMPTION is serial per item with
// a single integer of state (the resume pointer) — the brief's counterexample
// killed the naive max-scan: with W=[0,10), Y=[5,15), X=[12,14) the prefix-
// max sees Y's 15 and suppresses X, which greedy commits. v1 runs the chain
// one thread per ITEM (the product corpus is 1,306 items — item-parallel);
// the chunked function-composition form (the Mojo zone tables) is the
// follow-up only if a dense-single-item measurement demands it.
//
// Lookup is DESCENDING-LENGTH BINARY SEARCH over the sorted sequence
// section (the section order is asserted at bake; the longest exact prefix
// is unique) — semantics-neutral vs the Mojo st_probe hash by that same
// argument, and it reuses the item_search pattern already proven here. The
// comparator re-walks the probe's effective codepoints from the head byte
// instead of storing a key array: the walk is deterministic, so no local
// storage exists to spill.
//
// GAP-BYTE GUARD: the CPU walks only [start, stop); item_search attributes
// bytes to the largest start <= id WITHOUT an end check, so every per-byte
// test here carries its own id < stop guard — no fixture pins this (items
// tile every fixture's blob), the landmine list does.

/// sequence_length at i: the lenient classifier over the packed corpus.
#[cube]
fn seq_len_at(bytes: &[u32], i: usize, n: usize) -> u32 {
    let b = byte_at(bytes, i, n);
    if b & 0x80u32 == 0u32 {
        1u32
    } else if b & 0xE0u32 == 0xC0u32 {
        2u32
    } else if b & 0xF0u32 == 0xE0u32 {
        3u32
    } else if b & 0xF8u32 == 0xF0u32 {
        4u32
    } else {
        0u32
    }
}

/// decode_codepoint_at at i for a known length.
#[cube]
fn cp_at(bytes: &[u32], i: usize, len: u32, n: usize) -> u32 {
    let b = byte_at(bytes, i, n);
    let b1 = byte_at(bytes, i + 1, n);
    let b2 = byte_at(bytes, i + 2, n);
    let b3 = byte_at(bytes, i + 3, n);
    if len == 1u32 {
        b
    } else if len == 2u32 {
        ((b & 0x1Fu32) << 6u32) | (b1 & 0x3Fu32)
    } else if len == 3u32 {
        ((b & 0x0Fu32) << 12u32) | ((b1 & 0x3Fu32) << 6u32) | (b2 & 0x3Fu32)
    } else {
        ((b & 0x07u32) << 18u32) | ((b1 & 0x3Fu32) << 12u32) | ((b2 & 0x3Fu32) << 6u32) | (b3 & 0x3Fu32)
    }
}

/// is_static_zero_cp: ZWJ, the variation selectors, the tag characters.
#[cube]
// clippy:manual-range-contains allowed here — the cube macro has no
// RangeInclusive::contains expansion, and this is the spelled-out form it
// takes on device.
#[allow(clippy::manual_range_contains)]
fn is_static_zero(cp: u32) -> u32 {
    if cp == 0x200Du32 || (cp >= 0xFE00u32 && cp <= 0xFE0Fu32) || (cp >= 0xE0020u32 && cp <= 0xE007Fu32) {
        1u32
    } else {
        0u32
    }
}


/// The cluster PROBE: thread per word. Static-zero bytes of cluster items
/// are marked here (unconditionally — match-independent, fold.rs:543-549);
/// candidates build their EFFECTIVE key into per-unit shared scratch (the
/// head's own codepoint first, FE0F skipped but riding, newline/VS15/
/// continuation/item-end breaking) and run a descending-length binary
/// search over the sorted sequence section — the longest exact prefix is
/// unique, so this is answer-identical to the CPU's linear scan and the
/// Mojo's hash probe alike. The span end re-walks counting CONSUMED key
/// elements, so trailing FE0Fs past the last consumer stay outside.
///
/// All of this lives INLINE in the kernel with Shared scratch because the
/// walk/search shapes only compile in kernel context — loops in HELPERS
/// break the macro's assign typing (recorded landmine; the deleted helper
/// drafts are in the commit history).
#[cube(launch_unchecked)]
fn cluster_probe(
    bytes: &[u32],
    bitmap: &[u32],
    seq: &[u32],
    ir: &[u32],
    ic: &[u32],
    fl: &mut [u32],
    sm: &mut [f32],
    cslot: &mut [u32],
    cend: &mut [u32],
    #[comptime] units: usize,
    #[comptime] seq_max: u32,
) {
    let w = ABSOLUTE_POS;
    let n = bytes.len() * 4;
    let u = UNIT_POS as usize;
    let item_count = ir.len() / 2;
    // Per-unit key scratch: seq_max effective codepoints.
    let mut skey = Shared::<[u32]>::new_slice(units * seq_max as usize);
    if w < fl.len() {
        let mut word = fl[w];
        let mut lane = 0usize;
        while lane < 4 {
            let id = w * 4 + lane;
            if id < n {
                let len = seq_len_at(bytes, id, n);
                if len > 0u32 {
                    let mut stop = 0usize;
                    let mut cluster = false;
                    if item_count > 0 {
                        let it = item_search(ir, item_count, id);
                        stop = ir[it * 2 + 1] as usize;
                        cluster = ic[it] != 0;
                    }
                    // The gap-byte guard: only bytes INSIDE the item range.
                    if cluster && id < stop {
                        let cp = cp_at(bytes, id, len, n);
                        if is_static_zero(cp) != 0u32 {
                            sm[id] = f32::from_bits(0u32);
                            word |= F_CLUSTER_TRAILER << ((lane as u32) * 8u32);
                        } else {
                            let bit = (bitmap[(cp >> 5u32) as usize] >> (cp & 0x1Fu32)) & 1u32;
                            if bit != 0u32 {
                                // Key build: the head's own cp is element 0.
                                let mut klen = 0u32;
                                let mut p = id;
                                let mut alive = 1u32;
                                while alive == 1u32 && p < stop && klen < seq_max {
                                    let len2 = seq_len_at(bytes, p, n);
                                    let cp2 = cp_at(bytes, p, len2, n);
                                    let dead = if len2 == 0u32 || cp2 == 0x0Au32 || cp2 == 0xFE0Eu32 {
                                        1u32
                                    } else {
                                        0u32
                                    };
                                    if dead == 1u32 {
                                        alive = 0u32;
                                    }
                                    if dead == 0u32 {
                                        if cp2 != 0xFE0Fu32 {
                                            skey[u * seq_max as usize + klen as usize] = cp2;
                                            klen += 1u32;
                                        }
                                        p += len2 as usize;
                                    }
                                }
                                // Descending-length binary search.
                                let stride = 2u32 + seq_max;
                                let seq_count = (seq.len() / stride as usize) as u32;
                                let mut elen = if klen < seq_max { klen } else { seq_max };
                                let mut slot = 0u32;
                                let mut need = 0u32;
                                while elen >= 2u32 && slot == 0u32 {
                                    let mut lo = 0u32;
                                    let mut hi = seq_count;
                                    while lo < hi {
                                        let mid = (lo + hi) / 2u32;
                                        let eoff = mid as usize * stride as usize;
                                        let entry_len = seq[eoff + 1];
                                        let kmax = if entry_len < elen { entry_len } else { elen };
                                        let mut ord = 0i32;
                                        let mut k = 0u32;
                                        while k < kmax && ord == 0i32 {
                                            let want = seq[eoff + 2 + k as usize];
                                            let probe = skey[u * seq_max as usize + k as usize];
                                            if probe < want {
                                                ord = -1i32;
                                            }
                                            if ord == 0i32 && probe > want {
                                                ord = 1i32;
                                            }
                                            k += 1u32;
                                        }
                                        if ord == 0i32 {
                                            // Shorter-prefix-first order: with equal
                                            // elements the SHORTER sequence sorts first,
                                            // so a longer entry is GREATER than the probe.
                                            if entry_len < elen {
                                                ord = 1i32;
                                            }
                                            if entry_len > elen {
                                                ord = -1i32;
                                            }
                                        }
                                        if ord < 0i32 {
                                            hi = mid;
                                        }
                                        if ord > 0i32 {
                                            lo = mid + 1u32;
                                        }
                                        if ord == 0i32 {
                                            slot = seq[eoff];
                                            need = elen;
                                            lo = hi;
                                        }
                                    }
                                    elen -= 1u32;
                                }
                                if slot != 0u32 {
                                    // Span end: re-walk, counting consumers.
                                    let mut got2 = 0u32;
                                    let mut p2 = id;
                                    let mut send = id as u32;
                                    let mut alive2 = 1u32;
                                    while alive2 == 1u32 && p2 < stop {
                                        let len3 = seq_len_at(bytes, p2, n);
                                        let cp3 = cp_at(bytes, p2, len3, n);
                                        let dead3 = if len3 == 0u32 || cp3 == 0x0Au32 || cp3 == 0xFE0Eu32 {
                                            1u32
                                        } else {
                                            0u32
                                        };
                                        if dead3 == 1u32 {
                                            alive2 = 0u32;
                                        }
                                        if dead3 == 0u32 {
                                            if cp3 != 0xFE0Fu32 {
                                                got2 += 1u32;
                                                if got2 == need {
                                                    send = (p2 + len3 as usize) as u32;
                                                }
                                            }
                                            p2 += len3 as usize;
                                        }
                                    }
                                    cslot[id] = slot;
                                    cend[id] = send;
                                }
                            }
                        }
                    }
                }
            }
            lane += 1usize;
        }
        fl[w] = word;
    }
}

/// The cluster CHAIN: thread per item, the greedy walk. A candidate commits
/// iff the walk VISITS it (suppressed candidates never claim their spans —
/// the overlap fixture's rule); the walk resumes past each committed span.
/// Trailer marking ORs F_CLUSTER_TRAILER into packed flag words through
/// atomics — a word can straddle items, so byte-lane writes from different
/// item threads race otherwise.
#[cube(launch_unchecked)]
fn cluster_chain(
    bytes: &[u32],
    ir: &[u32],
    ic: &[u32],
    cslot: &[u32],
    cend: &[u32],
    sm: &mut [f32],
    fl_atomic: &mut [Atomic<u32>],
    bitmap_advance: f32,
) {
    let it = ABSOLUTE_POS;
    let item_count = ir.len() / 2;
    let n = bytes.len() * 4;
    if it < item_count && ic[it] != 0 {
        let mut p = ir[it * 2] as usize;
        let stop = ir[it * 2 + 1] as usize;
        while p < stop {
            let slot = cslot[p];
            if slot != 0u32 {
                let e = cend[p] as usize;
                sm[p] = bitmap_advance;
                let mut t = p + 1usize;
                while t < e {
                    if flags_at_from_atomic(fl_atomic, t) & F_LEADER != 0 {
                        sm[t] = f32::from_bits(0u32);
                        fl_atomic[t >> 2].fetch_or(F_CLUSTER_TRAILER << (((t & 3) as u32) * 8u32));
                    }
                    t += 1usize;
                }
                p = e;
            } else {
                let len = seq_len_at(bytes, p, n);
                if len > 0u32 {
                    p += len as usize;
                } else {
                    p += 1usize;
                }
            }
        }
    }
}

/// flags_at over the atomic view of the packed flag buffer (the chain reads
/// leader bits while OR-ing trailer bits into the same words — the fetch_or
/// never touches bit 0, so reads stay consistent).
#[cube]
fn flags_at_from_atomic(fl: &mut [Atomic<u32>], i: usize) -> u32 {
    fl[i >> 2].load() >> (((i & 3) as u32) * 8u32) & 0xFFu32
}

/// Byte i of the packed corpus, zero past the end (the reference's
/// bounds-checked read).
#[cube]
fn byte_at(bytes: &[u32], i: usize, n: usize) -> u32 {
    if i < n {
        (bytes[i >> 2] >> (((i & 3) * 8) as u32)) & 0xFFu32
    } else {
        0u32
    }
}
const LM_STRIDE: usize = 4;
const LM_X: usize = 0;
const LM_Y: usize = 1;
const LM_Z: usize = 2;
const LM_BASE_X: usize = 3;
const LC_STRIDE: usize = 2;
const LC_ROW: usize = 0;
const LC_COL: usize = 1;
const IM_STRIDE: usize = 9;
const IM_ORIGIN_Y: usize = 0;
const IM_ORIGIN_Z: usize = 1;
const IM_LINE_HEIGHT: usize = 2;
const IM_Z_STEP: usize = 3;
const IM_BAND_STRIDE_Y: usize = 4;
const IM_DEPTH_PER_BAND: usize = 5;
const IM_DEPTH_PER_COL: usize = 6;
const IM_ORIGIN_X: usize = 8;
const IE_STRIDE: usize = 8;
const IE_PAGE_ROWS: usize = 0;
const IE_PAGE_COLS: usize = 1;
const IE_SCROLL_ROWS: usize = 2;
const IE_PAGES_WIDE: usize = 3;
const IE_WRAP_WIDTH: usize = 4;
const IE_HAS_PAGE: usize = 5;
const IE_WRAP_MODE: usize = 6;

const F_LEADER: u32 = 1;
const F_NEWLINE: u32 = 4;
const WRAP_BACK: i32 = 1;

// ── the monoid, device-side ──────────────────────────────────────────────────

/// scan.rs's ScanElem, register-resident. i32 lanes where the CPU rides i64:
/// counts, every one — no lane approaches 2^31 inside a chunk.
#[derive(CubeType, Clone, Copy)]
struct ChainElem {
    reset: i32,
    nl: i32,
    glyphs: i32,
    rows: i32,
    head_len: i32,
    tail_len: i32,
    wrap: i32,
    mode: i32,
    tail_adv: f32,
}

#[cube]
fn identity() -> ChainElem {
    ChainElem {
        reset: 0,
        nl: 0,
        glyphs: 0,
        rows: 0,
        head_len: 0,
        tail_len: 0,
        wrap: 0,
        mode: 0,
        tail_adv: 0.0,
    }
}

/// rows_for_line, transcribed (the phantom-row correction included).
#[cube]
fn rows_for(length: i32, wrap: i32, mode: i32) -> i32 {
    if mode == WRAP_BACK || wrap <= 0 || length <= 0 {
        1
    } else {
        (length - 1) / wrap + 1
    }
}

/// wrap_segment_of, transcribed — mode-free depth fan.
#[cube]
fn wrap_segment_of(col: i32, wrap: i32, terminator: bool) -> i32 {
    if wrap <= 0 {
        0
    } else if terminator {
        if col <= 0 {
            0
        } else {
            (col - 1) / wrap
        }
    } else {
        col / wrap
    }
}

/// wrap_row_of, transcribed — WRAP_DOWN: the segment index; WRAP_BACK: zero.
#[cube]
fn wrap_row_of(col: i32, wrap: i32, terminator: bool, mode: i32) -> i32 {
    if mode == WRAP_BACK {
        0
    } else {
        wrap_segment_of(col, wrap, terminator)
    }
}

/// scan_combine, transcribed line-for-line — the GENERAL form (spine-grade:
/// b may carry rows and a real head line, not just a leaf's).
#[cube]
fn combine(a: &mut ChainElem, b: &ChainElem) {
    if b.reset != 0 {
        a.reset = 1;
        a.nl = b.nl;
        a.glyphs = b.glyphs;
        a.rows = b.rows;
        a.head_len = b.head_len;
        a.tail_len = b.tail_len;
        a.tail_adv = b.tail_adv;
        a.wrap = b.wrap;
        a.mode = b.mode;
    } else {
        a.wrap = b.wrap;
        a.mode = b.mode;
        if b.nl == 0 {
            a.tail_len += b.tail_len;
            a.tail_adv += b.tail_adv; // f32 per add — the oracle's chain
            if a.nl == 0 {
                a.head_len = a.tail_len;
            }
        } else {
            if a.nl == 0 {
                a.head_len += b.head_len;
                a.rows = b.rows;
            } else {
                a.rows += rows_for(a.tail_len + b.head_len, b.wrap, b.mode) + b.rows;
            }
            a.tail_len = b.tail_len;
            a.tail_adv = b.tail_adv;
        }
        a.nl += b.nl;
        a.glyphs += b.glyphs;
    }
}

/// leaf_of, transcribed: reset/wrap/mode always; the rest only for leaders.
/// `fl` is the PACKED flag array — one byte per byte position, four per u32
/// word (the chain consumes only F_LEADER/F_NEWLINE, both in the low byte;
/// the full flags live CPU-side for the renderer). fl.len() is WORDS; every
/// byte-count in the kernels multiplies by 4.
#[cube]
fn flags_at(fl: &[u32], i: usize) -> u32 {
    (fl[i >> 2] >> (((i & 3) * 8) as u32)) & 0xFF
}

#[cube]
fn leaf_of(fl: &[u32], sm: &[f32], wrap: i32, mode: i32, reset: i32, id: usize) -> ChainElem {
    let mut e = identity();
    e.reset = reset;
    e.wrap = wrap;
    e.mode = mode;
    let f = flags_at(fl, id);
    if (f & F_LEADER) != 0 {
        e.glyphs = 1;
        if (f & F_NEWLINE) != 0 {
            e.nl = 1;
        } else {
            e.head_len = 1;
            e.tail_len = 1;
            e.tail_adv = sm[id * SM_STRIDE + SM_ADVANCE];
        }
    }
    e
}

#[cube]
fn p_load(pc: &[u32], pm: &[f32], i: usize) -> ChainElem {
    let o = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: pc[o + P_RESET] as i32,
        nl: pc[o + P_NL] as i32,
        glyphs: pc[o + P_GLYPHS] as i32,
        rows: pc[o + P_ROWS] as i32,
        head_len: pc[o + P_HEAD_LEN] as i32,
        tail_len: pc[o + P_TAIL_LEN] as i32,
        wrap: pc[o + P_WRAP] as i32,
        mode: pc[o + P_MODE] as i32,
        tail_adv: pm[i],
    }
}

#[cube]
fn p_store(pc: &mut [u32], pm: &mut [f32], i: usize, e: &ChainElem) {
    let o = i * PARTIAL_COUNT_STRIDE;
    pc[o + P_RESET] = e.reset as u32;
    pc[o + P_NL] = e.nl as u32;
    pc[o + P_GLYPHS] = e.glyphs as u32;
    pc[o + P_ROWS] = e.rows as u32;
    pc[o + P_HEAD_LEN] = e.head_len as u32;
    pc[o + P_TAIL_LEN] = e.tail_len as u32;
    pc[o + P_WRAP] = e.wrap as u32;
    pc[o + P_MODE] = e.mode as u32;
    pm[i] = e.tail_adv;
}

/// item_search_device: the largest item whose byte_start <= id.
#[cube]
fn item_search(ir: &[u32], item_count: usize, id: usize) -> usize {
    let mut low = 0usize;
    let mut high = item_count - 1;
    while low < high {
        let mid = (low + high + 1) >> 1;
        if (ir[mid * 2] as usize) <= id {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// ordered_key: the monotonic float -> u32 map for the x_max atomic.
#[cube]
fn ordered_key(v: f32) -> u32 {
    let b = v.to_bits();
    if (b & 0x80000000) != 0 {
        !b
    } else {
        b | 0x80000000
    }
}

/// key_to_float: its inverse (for derive_stride's readback of x_max).
#[cube]
fn key_to_float(k: u32) -> f32 {
    let b = if (k & 0x80000000) != 0 { k & 0x7FFFFFFF } else { !k };
    f32::from_bits(b)
}

/// Load a monoid element from the shared tile arrays (the pc lane layout, i32).
#[cube]
fn s_load(sc: &[i32], sf: &[f32], i: usize) -> ChainElem {
    let o = i * PARTIAL_COUNT_STRIDE;
    ChainElem {
        reset: sc[o + P_RESET],
        nl: sc[o + P_NL],
        glyphs: sc[o + P_GLYPHS],
        rows: sc[o + P_ROWS],
        head_len: sc[o + P_HEAD_LEN],
        tail_len: sc[o + P_TAIL_LEN],
        wrap: sc[o + P_WRAP],
        mode: sc[o + P_MODE],
        tail_adv: sf[i],
    }
}

/// Store a monoid element into the shared tile arrays.
#[cube]
fn s_store(sc: &mut [i32], sf: &mut [f32], i: usize, e: &ChainElem) {
    let o = i * PARTIAL_COUNT_STRIDE;
    sc[o + P_RESET] = e.reset;
    sc[o + P_NL] = e.nl;
    sc[o + P_GLYPHS] = e.glyphs;
    sc[o + P_ROWS] = e.rows;
    sc[o + P_HEAD_LEN] = e.head_len;
    sc[o + P_TAIL_LEN] = e.tail_len;
    sc[o + P_WRAP] = e.wrap;
    sc[o + P_MODE] = e.mode;
    sf[i] = e.tail_adv;
}

// ── dispatch 1: tileScan — one cube per tile; rake + workgroup Blelloch ──────
//
// Each unit rakes its `rake` bytes into one monoid element (a serial,
// order-preserving micro-fold); the cube Blelloch-scans the `units` partials
// in shared memory into exclusive micro prefixes; unit units-1 publishes the
// tile total. The critical path per tile is rake + 2·log(units) combines —
// against the old thread-per-chunk serial fold's one-thread `chunk`-deep
// chain with units-fold less parallelism.
#[cube(launch_unchecked)]
fn tile_scan(
    fl: &[u32],
    sm: &[f32],
    ir: &[u32],
    ie: &[u32],
    tc: &mut [u32],
    tm: &mut [f32],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    // ItemWalk seed. For pad units (lo >= n) the seed clamps to the last byte
    // so the pad element carries the wrap/mode in force at the tile's end —
    // see the module header for why a pure-identity pad would poison the
    // tile total's wrap lanes. (n == 0 makes the clamp wrap; the seed then
    // feeds only an unused walk, and every ir/ie read stays in bounds.)
    let mut seed = lo;
    if seed >= n {
        seed = n - 1;
    }
    let mut it = 0usize;
    let mut start = 0usize;
    let mut nxt = n;
    let mut w_wrap = 0i32;
    let mut w_mode = 0i32;
    let has = item_count > 0;
    if has {
        it = item_search(ir, item_count, seed);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
    }
    let mut acc = identity();
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
            }
            let reset = if has && id == start { 1i32 } else { 0i32 };
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, reset, id);
            combine(&mut acc, &leaf);
            id += 1;
        }
    } else {
        acc.wrap = w_wrap;
        acc.mode = w_mode;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    // Up-sweep: x[u] = combine(x[u-s], x[u]) at the ends of 2s-blocks. The
    // LEFT operand is the lower element — the fold order.
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    // Exclusive: hold the total in a register (it publishes below), then seed
    // the root with identity.
    sync_cube();
    if u == units - 1 {
        let total = s_load(&sc, &sf, u);
        p_store(tc, tm, tile, &total);
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    // Down-sweep — NON-COMMUTATIVE form, derived and unit-tested by hand on
    // n=8: t = x[u]; x[u] = combine(x[u], x[u-s]); x[u-s] = t. The carried
    // prefix is the LEFT operand and the left child's TOTAL the right; the
    // commutative textbook form (combine(x[u-s], x[u])) silently scrambles
    // reset/head/tail lanes.
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();
}

// ── dispatch 2: spineScan — ONE cube over the tile totals ────────────────────
//
// Each unit owns a CONTIGUOUS block of tile totals (order-preserving for the
// non-commutative monoid — a strided rake would interleave blocks across
// units and the workgroup scan could not recompose them), rakes them into
// one partial, the cube Blelloch-scans the units, then each unit chases its
// block writing global exclusive tile prefixes. One cube handles
// units·(tiles/units) tiles by raking deeper; the scaling path beyond that
// is a second spine level, not built yet.
#[cube(launch_unchecked)]
fn spine_scan(
    tc: &[u32],
    tm: &[f32],
    xc: &mut [u32],
    xm: &mut [f32],
    #[comptime] units: usize,
    #[comptime] log: usize,
) {
    let u = UNIT_POS as usize;
    let n_tiles = tc.len() / PARTIAL_COUNT_STRIDE;
    let per = n_tiles.div_ceil(units);
    let first = u * per;
    let last = if first + per < n_tiles { first + per } else { n_tiles };
    let mut acc = identity();
    if first < n_tiles {
        for t in first..last {
            let e = p_load(tc, tm, t);
            combine(&mut acc, &e);
        }
    } else {
        // Pad: identity counts, the LAST tile's wrap/mode — see module header.
        let o = (n_tiles - 1) * PARTIAL_COUNT_STRIDE;
        acc.wrap = tc[o + P_WRAP] as i32;
        acc.mode = tc[o + P_MODE] as i32;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    sync_cube();
    if u == units - 1 {
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();

    // Chase: this unit's block of tiles gets its global exclusive prefixes.
    let mut pre = s_load(&sc, &sf, u);
    if first < n_tiles {
        for t in first..last {
            p_store(xc, xm, t, &pre);
            let e = p_load(tc, tm, t);
            combine(&mut pre, &e);
        }
    }
}

/// The fold width in force for item `it`: its wrap width, or its page
/// columns when it has no wrap. fold==0 means x IS the line-advance lane —
/// no segment re-sum exists.
#[cube]
fn fold_of(ie: &[u32], it: usize, wrap: i32) -> i32 {
    if wrap > 0 {
        wrap
    } else if ie[it * IE_STRIDE + IE_HAS_PAGE] != 0 {
        ie[it * IE_STRIDE + IE_PAGE_COLS] as i32
    } else {
        0
    }
}

// ── dispatch 3: apply — rake + Blelloch + per-byte chase ─────────────────────
//
// Same tile decomposition as tile_scan (the rake and tree are re-derived).
// The publish-the-micro-prefixes alternative was BUILT AND MEASURED
// (2026-09-27) and LOST: apply 21.0 -> 31.4ms, chain 28.6 -> 40.2. The
// rake's "redundant" re-read of fl/sm rides L1 — the chase re-reads the
// same 8 bytes it just raked — so it is nearly free, while 36B/unit of
// published prefixes are cold global traffic, and the tree's 18 barriers
// are cheap. Reverted; do not re-derive this trade, re-measure it. Each
// unit chases its `rake` bytes seeded with combine(global tile prefix, own
// exclusive micro prefix), emitting the per-byte lanes.
//
// THE TREE ITSELF costs ~2.3-2.6ms per kernel that carries one (measured
// same day by emptying the loops: tile_scan 5.6 -> 3.3ms), so ~5ms of the
// ~25.6ms chain — the C6 plane-op restructure's ceiling. It stays parked:
// plane ops are subgroup intrinsics on the WGSL path (a pinned-naga
// behavior, absent on weaker backends), they diverge per compute target
// (the CPU runtime's PLANE_DIM differs), and they are scalar-numeric-only,
// so this monoid would need a manual shuffle tree with the non-commutative
// down-swap rebuilt lane by lane. If taken: as a #[comptime] capability-
// gated variant beside this default, the inline_resolve pattern — never as
// the foundation.
//
// FOLDLESS ITEMS RESOLVE HERE: for fold==0 the chase already holds
// everything resolve_x would recompute — x IS run.tail_adv, and the old
// kernel read ~700MB back (fl, lc, wm, wc, otb) to recover values that were
// in registers at this exact point. The chase writes their lm lanes and
// reduces their maxima directly; wm/wc/otb are written ONLY for fold>0
// items, whose segment re-sum genuinely needs the cross-unit ordinal table,
// and the resolve_x dispatch is skipped entirely when no item folds. Row
// maxima reduce here for EVERY item (rows are final in the chase); x maxima
// only where x is final (fold==0) — resolve_x owns the fold>0 x maxima.
//
// `inline_resolve` is COMPTIME (the k_decode_probe[probe,walk] pattern): the
// inline branch's live state (four item-parameter loads, the lm writes)
// inflates the whole kernel's register set, and measured on a pure-wrapped
// corpus that costs 2x on the division-heavy fold>0 path — stalls that the
// memory-bound foldless path hides. Corpora where EVERY item folds compile
// the inline branch out entirely; mixed and foldless corpora compile it in.
#[cube(launch_unchecked)]
fn apply(
    fl: &[u32],
    sm: &[f32],
    lc: &mut [u32],
    lm: &mut [f32],
    ir: &[u32],
    ie: &[u32],
    items: &[f32],
    xc: &[u32],
    xm: &[f32],
    wm: &mut [f32],
    wc: &mut [u32],
    otb: &mut [u32],
    row_max: &mut [Atomic<u32>],
    x_max: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] rake: usize,
    #[comptime] log: usize,
    #[comptime] inline_resolve: bool,
) {
    let tile = CUBE_POS;
    let u = UNIT_POS as usize;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let lo = tile * (units * rake) + u * rake;
    let hi = if lo + rake < n { lo + rake } else { n };
    // Maxima-reduction slots, seeded before the tree so its barriers cover
    // visibility (see resolve_x's header for the slot protocol). The whole
    // apparatus is compiled out for pure-wrapped corpora: measured there,
    // the per-leader RMWs cost ~8ms inside apply's division-stalled chase,
    // while riding FREE inside resolve_x, whose re-sum already dominates —
    // so resolve_x owns both maxima for that shape (fetch_max idempotence
    // makes the mixed-corpus double reduction harmless).
    let srow = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let sx = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let tile_lo = tile * (units * rake);
    let mut it_base = 0usize;
    if inline_resolve {
        if u == 0 {
            let probe = if tile_lo < n { tile_lo } else { n - 1 };
            let mut b = 0usize;
            if item_count > 0 {
                b = item_search(ir, item_count, probe);
            }
            *sbase = b as u32;
        }
        let mut z = u;
        while z < RESOLVE_SLOTS {
            srow[z].store(0u32);
            sx[z].store(0u32);
            z += units;
        }
    }
    let mut seed = lo;
    if seed >= n {
        seed = n - 1;
    }
    let mut it = 0usize;
    let mut start = 0usize;
    let mut nxt = n;
    let mut w_wrap = 0i32;
    let mut w_mode = 0i32;
    let has = item_count > 0;
    if has {
        it = item_search(ir, item_count, seed);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
    }
    let mut acc = identity();
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
            }
            let reset = if has && id == start { 1i32 } else { 0i32 };
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, reset, id);
            combine(&mut acc, &leaf);
            id += 1;
        }
    } else {
        acc.wrap = w_wrap;
        acc.mode = w_mode;
    }

    let mut sc = Shared::<[i32]>::new_slice(units * PARTIAL_COUNT_STRIDE);
    let mut sf = Shared::<[f32]>::new_slice(units);
    s_store(&mut sc, &mut sf, u, &acc);

    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = 1usize << d;
        if (u + 1) & (2 * s - 1) == 0 {
            let mut a = s_load(&sc, &sf, u - s);
            let b = s_load(&sc, &sf, u);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
        }
    }
    sync_cube();
    if u == units - 1 {
        let e = identity();
        s_store(&mut sc, &mut sf, u, &e);
    }
    #[unroll]
    for d in 0..log {
        sync_cube();
        let s = units >> (d + 1);
        if (u + 1) & (2 * s - 1) == 0 {
            let t = s_load(&sc, &sf, u);
            let mut a = s_load(&sc, &sf, u);
            let b = s_load(&sc, &sf, u - s);
            combine(&mut a, &b);
            s_store(&mut sc, &mut sf, u, &a);
            s_store(&mut sc, &mut sf, u - s, &t);
        }
    }
    sync_cube();

    // The chase: the running prefix at this unit's first byte is the global
    // tile prefix combined with the exclusive micro prefix from the tree.
    // The walk is RE-SEEDED first — the rake advanced it past this unit's
    // range, and each byte's reset/wrap/mode must come from ITS item, not
    // the range's last one (the multi-item fixtures caught exactly this).
    let mut run = p_load(xc, xm, tile);
    let micro = s_load(&sc, &sf, u);
    combine(&mut run, &micro);
    if inline_resolve {
        it_base = *sbase as usize;
    }
    it = 0usize;
    start = 0usize;
    nxt = n;
    w_wrap = 0i32;
    w_mode = 0i32;
    let mut w_fold = 0i32;
    if has {
        it = item_search(ir, item_count, lo);
        start = ir[it * 2] as usize;
        nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
        w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
        w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
        w_fold = fold_of(ie, it, w_wrap);
    }
    if lo < n {
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                w_wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                w_mode = ie[it * IE_STRIDE + IE_WRAP_MODE] as i32;
                w_fold = fold_of(ie, it, w_wrap);
            }
            let reset = has && id == start;
            if reset {
                // run = identity(), then the item's params ride again.
                run.reset = 0;
                run.nl = 0;
                run.glyphs = 0;
                run.rows = 0;
                run.head_len = 0;
                run.tail_len = 0;
                run.tail_adv = 0.0;
                run.wrap = w_wrap;
                run.mode = w_mode;
            }
            let f = flags_at(fl, id);
            if (f & F_LEADER) != 0 {
                // lanes_from_prefix, inline.
                let col = run.tail_len;
                let mut closed = 0i32;
                if run.nl > 0 {
                    closed = rows_for(run.head_len, w_wrap, w_mode) + run.rows;
                }
                let wr = wrap_row_of(col, w_wrap, (f & F_NEWLINE) != 0, w_mode);
                let row = closed + wr;
                let co = id * LC_STRIDE;
                lc[co + LC_ROW] = row as u32;
                lc[co + LC_COL] = col as u32;
                if inline_resolve {
                    // Rows are final here for every item — reduce them all.
                    // (Compiled out for pure-wrapped corpora: resolve_x owns
                    // both maxima there — see the slot-seeding note above.)
                    let slot = it - it_base;
                    if slot < RESOLVE_SLOTS {
                        srow[slot].fetch_max((row + 1) as u32);
                    } else {
                        row_max[it].fetch_max((row + 1) as u32);
                    }
                }
                if w_fold > 0 || !inline_resolve {
                    // The re-sum's inputs: ordinal table + line advance.
                    wc[id] = run.glyphs as u32;
                    wm[id] = run.tail_adv;
                    otb[start + run.glyphs as usize] = id as u32;
                } else {
                    // Foldless: x IS the line-advance lane — resolve_x's
                    // whole per-item computation, in registers, now.
                    let x = run.tail_adv;
                    let io = it * IM_STRIDE;
                    let seg = wrap_segment_of(col, w_wrap, (f & F_NEWLINE) != 0);
                    let lh = items[io + IM_LINE_HEIGHT];
                    let mo = id * LM_STRIDE;
                    let base = x + items[io + IM_ORIGIN_X];
                    lm[mo + LM_BASE_X] = base;
                    lm[mo + LM_X] = base;
                    lm[mo + LM_Y] = (row as f32) * (-lh) + items[io + IM_ORIGIN_Y];
                    lm[mo + LM_Z] = (seg as f32) * (-items[io + IM_Z_STEP]) + items[io + IM_ORIGIN_Z];
                    let slot = it - it_base;
                    if slot < RESOLVE_SLOTS {
                        sx[slot].fetch_max(ordered_key(x));
                    } else {
                        x_max[it].fetch_max(ordered_key(x));
                    }
                }
            }
            let leaf = leaf_of(fl, sm, w_wrap, w_mode, if reset { 1i32 } else { 0i32 }, id);
            combine(&mut run, &leaf);
            id += 1;
        }
    }
    if inline_resolve {
        sync_cube();
        if u < RESOLVE_SLOTS {
            let it2 = it_base + u;
            if it2 < item_count {
                row_max[it2].fetch_max(srow[u].load());
                x_max[it2].fetch_max(sx[u].load());
            }
        }
    }
}

// ── dispatch 4: resolveX — the WRAPPED items' x, range workers ────────────────
//
// SKIPPED ENTIRELY when no item folds (the driver knows): foldless items
// resolve inside apply's chase, which holds x in a register.
//
// Each worker owns a `span`-byte range (apply's unit decomposition without
// the tree). At the first leader of a segment it enters, the worker walks
// BACKWARD once — at most fold dependent loads — to compute the entry x;
// from there it sweeps FORWARD through its range: every leader's x is the
// running sum, the same additions in the same left-fold order as the old
// per-leader backward re-sum, so the fold>0 X lanes stay BIT-exact (the
// check instrument witnesses that lane at bit level). Heads (col % fold ==
// 0; line and item starts are col==0) re-zero for free. Total backward
// work drops fold-fold (one entry walk per range, not per leader) and
// every worker stays busy — the first draft of this kernel gave each
// SEGMENT to its head and measured 4x WORSE (129ms vs 32): ~2% of threads
// active on long dependent chains is latency-bound with the machine idle.
// Both maxima reduce here (apply's are compiled out for this shape). An
// untouched slot flushes 0, which cannot beat a real value: rows count
// from 1 and every x >= 0 has an ordered key above 0.
#[cube(launch_unchecked)]
fn resolve_x(
    sm: &[f32],
    fl: &[u32],
    lm: &mut [f32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    wc: &[u32],
    otb: &[u32],
    row_max: &mut [Atomic<u32>],
    x_max: &mut [Atomic<u32>],
    #[comptime] units: usize,
    #[comptime] span: usize,
) {
    let t = ABSOLUTE_POS;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    let u = UNIT_POS as usize;
    let srow = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let sx = Shared::<[Atomic<u32>]>::new_slice(RESOLVE_SLOTS);
    let mut sbase = Shared::<u32>::new();
    let cube_lo = CUBE_POS * units * span;
    if u == 0 {
        // The item at this cube's first byte anchors the slot numbering.
        let probe = if cube_lo < n { cube_lo } else { n - 1 };
        let mut b = 0usize;
        if item_count > 0 {
            b = item_search(ir, item_count, probe);
        }
        *sbase = b as u32;
    }
    let mut z = u;
    while z < RESOLVE_SLOTS {
        srow[z].store(0u32);
        sx[z].store(0u32);
        z += units;
    }
    sync_cube();
    let it_base = *sbase as usize;

    let lo = t * span;
    if lo < n {
        let hi = if lo + span < n { lo + span } else { n };
        // The item walk, seeded at the range start.
        let mut it = 0usize;
        let mut start = 0usize;
        let mut nxt = n;
        let mut wrap = 0i32;
        let mut fold = 0i32;
        let has = item_count > 0;
        if has {
            it = item_search(ir, item_count, lo);
            start = ir[it * 2] as usize;
            nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
            wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
            fold = fold_of(ie, it, wrap);
        }
        let mut x = 0.0f32;
        let mut in_seg = false;
        let mut id = lo;
        while id < hi {
            while has && nxt <= id {
                it += 1;
                start = ir[it * 2] as usize;
                nxt = if it + 1 < item_count { ir[(it + 1) * 2] as usize } else { n };
                wrap = ie[it * IE_STRIDE + IE_WRAP_WIDTH] as i32;
                fold = fold_of(ie, it, wrap);
            }
            let f = flags_at(fl, id);
            if (f & F_LEADER) != 0 && fold > 0 {
                let col = lc[id * LC_STRIDE + LC_COL] as i32;
                let head = col % fold == 0;
                if !in_seg || head {
                    // Entry walk (backward, once per segment entry; free at
                    // a head where col % fold == 0 empties the loop).
                    x = 0.0f32;
                    let ord = wc[id] as i32;
                    let mut k = col % fold;
                    while k >= 1 {
                        let q = otb[start + (ord - k) as usize] as usize;
                        x += sm[q];
                        k -= 1;
                    }
                    in_seg = true;
                }
                let row = lc[id * LC_STRIDE + LC_ROW] as i32;
                let io = it * IM_STRIDE;
                let seg = wrap_segment_of(col, wrap, (f & F_NEWLINE) != 0);
                let lh = items[io + IM_LINE_HEIGHT];
                let mo = id * LM_STRIDE;
                let base = x + items[io + IM_ORIGIN_X];
                lm[mo + LM_BASE_X] = base;
                lm[mo + LM_X] = base;
                lm[mo + LM_Y] = (row as f32) * (-lh) + items[io + IM_ORIGIN_Y];
                lm[mo + LM_Z] = (seg as f32) * (-items[io + IM_Z_STEP]) + items[io + IM_ORIGIN_Z];
                let slot = it - it_base;
                if slot < RESOLVE_SLOTS {
                    srow[slot].fetch_max((row + 1) as u32);
                    sx[slot].fetch_max(ordered_key(x));
                } else {
                    row_max[it].fetch_max((row + 1) as u32);
                    x_max[it].fetch_max(ordered_key(x));
                }
                if (f & F_NEWLINE) == 0 {
                    // This leader's advance feeds the next x — the same add
                    // the backward re-sum performed, one step forward.
                    x += sm[id];
                }
            }
            id += 1;
        }
    }
    sync_cube();
    if u < RESOLVE_SLOTS {
        let it = it_base + u;
        if it < item_count {
            row_max[it].fetch_max(srow[u].load());
            x_max[it].fetch_max(sx[u].load());
        }
    }
}

// ── dispatch 5: derive the fan stride ON DEVICE — thread per item ───────────
#[cube(launch_unchecked)]
fn derive_stride(x_max: &[u32], ie: &[u32], page_gap_x: &[f32], strides: &mut [f32]) {
    let i = ABSOLUTE_POS;
    let item_count = ie.len() / IE_STRIDE;
    if i < item_count {
        let ie_off = i * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = ie[ie_off + IE_PAGE_ROWS] as i32;
        if has_page && rows > 0 {
            strides[i] = key_to_float(x_max[i]) + page_gap_x[i];
        } else {
            strides[i] = 0.0;
        }
    }
}

// ── dispatch 6: paginate — thread per byte, leaders only ─────────────────────
#[cube(launch_unchecked)]
fn paginate(
    lm: &mut [f32],
    fl: &[u32],
    lc: &[u32],
    items: &[f32],
    ie: &[u32],
    ir: &[u32],
    strides: &[f32],
) {
    let id = ABSOLUTE_POS;
    let n = fl.len() * 4; // packed: words -> bytes
    let item_count = ir.len() / 2;
    if id < n && (flags_at(fl, id) & F_LEADER) != 0 && item_count > 0 {
        let it = item_search(ir, item_count, id);
        let io = it * IM_STRIDE;
        let ie_off = it * IE_STRIDE;
        let has_page = ie[ie_off + IE_HAS_PAGE] != 0;
        let rows = if has_page { ie[ie_off + IE_PAGE_ROWS] as i32 } else { 0 };
        let cols = if has_page { ie[ie_off + IE_PAGE_COLS] as i32 } else { 0 };
        let scroll = if has_page { ie[ie_off + IE_SCROLL_ROWS] as i32 } else { 0 };
        if rows != 0 || cols != 0 || scroll != 0 {
            let row = lc[id * LC_STRIDE + LC_ROW] as i32;
            let col = lc[id * LC_STRIDE + LC_COL] as i32;
            let screen_row = row - scroll;
            let mut y_page = 0;
            if rows > 0 && screen_row >= rows {
                y_page = screen_row / rows;
            }
            let mut x_page = 0;
            if cols > 0 {
                x_page = col / cols;
            }
            let wide_raw = ie[ie_off + IE_PAGES_WIDE] as i32;
            let wide = if wide_raw > 1 { wide_raw } else { 1 };
            let band = y_page / wide;
            let wrap = ie[ie_off + IE_WRAP_WIDTH] as i32;
            let seg = wrap_segment_of(col, wrap, (flags_at(fl, id) & F_NEWLINE) != 0);
            let lh = items[io + IM_LINE_HEIGHT];
            let mo = id * LM_STRIDE;
            lm[mo + LM_X] = lm[mo + LM_BASE_X] + (y_page % wide) as f32 * strides[it];
            lm[mo + LM_Y] = items[io + IM_ORIGIN_Y]
                - (screen_row - y_page * rows) as f32 * lh
                - band as f32 * items[io + IM_BAND_STRIDE_Y];
            lm[mo + LM_Z] = items[io + IM_ORIGIN_Z]
                - seg as f32 * items[io + IM_Z_STEP]
                + band as f32 * items[io + IM_DEPTH_PER_BAND]
                + x_page as f32 * items[io + IM_DEPTH_PER_COL];
        }
    }
}

// ── the driver ────────────────────────────────────────────────────────────────

pub fn run(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let item_count = fx.items.len();
    // The tile shape, env-overridable so a (units, rake) sweep runs through
    // the same instrument — cross-shape agreement with the CPU scan's
    // (64, 256) chunks is associativity checked in situ.
    let units: usize = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let rake: usize = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    assert!(units.is_power_of_two(), "GLYPH_CHAIN_TILE must be a power of two");
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    // resolve_x worker span (bytes per worker; entry walks scale with
    // worker count, sweeps with span).
    let rspan: usize = std::env::var("GLYPH_CHAIN_SPAN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    // The CPU reference — a different tree shape at the (64, 256) tuning.
    let r = run_scan_pipeline(&fx.bytes, &fx.trie, &fx.items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);

    // Uploads: statics from the CPU decode (the bench's mode 0 shape). The
    // measure static is ADVANCE ONLY — the scan never reads height.
    // Statics: advance (f32/byte) + PACKED flags (u8/byte, four per word —
    // the chain reads fl three-to-four passes and consumes only the low
    // byte; the full flags stay CPU-side for the renderer).
    let n_words = n.div_ceil(4);
    let mut fl = Vec::with_capacity(n_words);
    let mut sm = Vec::with_capacity(n);
    for w in 0..n_words {
        let mut word = 0u32;
        for b in 0..4 {
            let i = w * 4 + b;
            if i < n {
                word |= (r.slots.flags(i) & 0xFF) << (b * 8);
            }
        }
        fl.push(word);
    }
    for i in 0..n {
        sm.push(r.slots.advance(i));
    }
    let mut ir = Vec::with_capacity(item_count * 2);
    let mut ie = Vec::with_capacity(item_count * IE_STRIDE);
    let mut im = Vec::with_capacity(item_count * IM_STRIDE);
    let mut page_gap_x = Vec::with_capacity(item_count);
    for item in &fx.items {
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
        ie.push(item.page_rows as u32);
        ie.push(item.page_cols as u32);
        ie.push(item.scroll_rows as u32);
        ie.push(item.pages_wide as u32);
        ie.push(item.wrap_width as u32);
        ie.push(item.has_page as u32);
        ie.push(match item.wrap_mode {
            WrapMode::Down => 0u32,
            WrapMode::Back => 1,
        });
        ie.push(0u32);
        im.push(item.origin_y as f32);
        im.push(item.origin_z as f32);
        im.push(item.line_height as f32);
        im.push(item.z_step as f32);
        im.push(item.band_stride_y as f32);
        im.push(item.depth_per_band as f32);
        im.push(item.depth_per_col as f32);
        im.push(0.0f32); // IM_PAGE_STRIDE_X: the device chain derives it on device
        im.push(item.origin_x as f32);
        page_gap_x.push(item.page_gap_x as f32);
    }

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();

    let h_fl = client.create_from_slice(bytemuck::cast_slice(&fl));
    let h_sm = client.create_from_slice(bytemuck::cast_slice(&sm));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ie = client.create_from_slice(bytemuck::cast_slice(&ie));
    let h_im = client.create_from_slice(bytemuck::cast_slice(&im));
    let h_gap = client.create_from_slice(bytemuck::cast_slice(&page_gap_x));
    let h_tc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = client.empty(n_tiles * 4);
    let h_xc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_tiles * 4);
    let h_lc = client.empty(n * LC_STRIDE * 4);
    let h_wm = client.empty(n * 4);
    let h_wc = client.empty(n * 4);
    let h_otb = client.empty(n * 4);
    let h_lm = client.empty(n * LM_STRIDE * 4);
    let h_strides = client.empty(item_count * 4);
    let zeroes = vec![0u32; item_count];
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&zeroes));

    // This M2's ADAPTER caps workgroups per grid dimension at 65535 (verified
    // live: a 94075-cube dispatch was rejected) — not a wgpu default to lift.
    // Spill into Y; ABSOLUTE_POS is the flattened id across axes, so the
    // kernels need no index change. gpu.rs still requests the adapter's value,
    // so an adapter with a higher cap takes the plain grid automatically.
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    // GLYPH_CHAIN_STAGES=N runs only the first N dispatches (bisection aid).
    let stages: usize = std::env::var("GLYPH_CHAIN_STAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    // Foldless corpora never dispatch resolve_x at all — apply's chase
    // resolves them (x in a register, no ordinal round trip). Pure-wrapped
    // corpora compile the inline branch out (see apply's header).
    let needs_resolve = fx.items.iter().any(|it| {
        it.wrap_width > 0 || (it.has_page && it.page_cols > 0)
    });
    let all_fold = fx
        .items
        .iter()
        .all(|it| it.wrap_width > 0 || (it.has_page && it.page_cols > 0));
    let inline_resolve = !all_fold;
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        tile_scan::launch_unchecked(
            &client,
            tiles_grid(n_tiles),
            CubeDim::new_1d(units as u32),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
            BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
            BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
            BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
            units,
            rake,
            log,
        );
        if stages >= 2 {
            spine_scan::launch_unchecked(
                &client,
                CubeCount::new_single(),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                units,
                log,
            );
        }
        if stages >= 3 {
            if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
                // Force stages 1-2 to land in their own submission, so a later
                // batch failing cannot take tile_scan's write down with it.
                let probe = client.read_one(h_tc.clone()).expect("pre-apply probe");
                let pv: &[u32] = bytemuck::cast_slice(&probe);
                println!("  dbg pre-apply tc[{} {} {} {}]", pv[0], pv[1], pv[2], pv[3]);
            }
            apply::launch_unchecked(
                &client,
                tiles_grid(n_tiles),
                CubeDim::new_1d(units as u32),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                BufferArg::from_raw_parts(h_wm.clone(), n),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                BufferArg::from_raw_parts(h_rmax.clone(), item_count),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                units,
                rake,
                log,
                inline_resolve,
            );
        }
        if stages >= 4 && needs_resolve {
            resolve_x::launch_unchecked(
                &client,
                cubes_of(n.div_ceil(rspan)),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_sm.clone(), n),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_wc.clone(), n),
                BufferArg::from_raw_parts(h_otb.clone(), n),
                BufferArg::from_raw_parts(h_rmax.clone(), item_count),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                256,
                rspan,
            );
        }
        if stages >= 5 {
            derive_stride::launch_unchecked(
                &client,
                cubes_of(item_count),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_xmax.clone(), item_count),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_gap.clone(), item_count),
                BufferArg::from_raw_parts(h_strides.clone(), item_count),
            );
        }
        if stages >= 6 {
            paginate::launch_unchecked(
                &client,
                cubes_of(n),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                BufferArg::from_raw_parts(h_fl.clone(), n_words),
                BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                BufferArg::from_raw_parts(h_im.clone(), item_count * IM_STRIDE),
                BufferArg::from_raw_parts(h_ie.clone(), item_count * IE_STRIDE),
                BufferArg::from_raw_parts(h_ir.clone(), item_count * 2),
                BufferArg::from_raw_parts(h_strides.clone(), item_count),
            );
        }
    }
    let lc_bytes = client.read_one(h_lc).expect("read lc");
    let wc_bytes = client.read_one(h_wc).expect("read wc");
    let wm_bytes = client.read_one(h_wm).expect("read wm");
    let lm_bytes = client.read_one(h_lm).expect("read lm");
    let rmax_bytes = client.read_one(h_rmax).expect("read rmax");
    let xmax_bytes = client.read_one(h_xmax).expect("read xmax");
    let dt = t0.elapsed();
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let fl_bytes = client.read_one(h_fl).expect("read fl");
        let flb: &[u32] = bytemuck::cast_slice(&fl_bytes);
        println!(
            "  dbg fl readback: [{} {} {} {} {} {} {} {}]",
            flb[0], flb[1], flb[2], flb[3], flb[4], flb[5], flb[6], flb[7]
        );
        let tc_bytes = client.read_one(h_tc).expect("read tc");
        let xc_bytes = client.read_one(h_xc).expect("read xc");
        let tc: &[u32] = bytemuck::cast_slice(&tc_bytes);
        let xc: &[u32] = bytemuck::cast_slice(&xc_bytes);
        for c in 0..n_tiles.min(3) {
            let o = c * PARTIAL_COUNT_STRIDE;
            println!(
                "  dbg tile {c} tc[reset={} nl={} glyphs={} rows={} head={} tail={} wrap={} mode={}] xc[same={}]",
                tc[o], tc[o + 1], tc[o + 2], tc[o + 3], tc[o + 4], tc[o + 5], tc[o + 6], tc[o + 7],
                xc[o] == tc[o] && xc[o + 2] == tc[o + 2]
            );
        }
    }
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    let wc: &[u32] = bytemuck::cast_slice(&wc_bytes);
    let wm: &[f32] = bytemuck::cast_slice(&wm_bytes);
    let lm: &[f32] = bytemuck::cast_slice(&lm_bytes);
    let rmax: &[u32] = bytemuck::cast_slice(&rmax_bytes);
    let xmax: &[u32] = bytemuck::cast_slice(&xmax_bytes);

    // The item maxima, diffed DIRECTLY against the CPU fold's item_bounds
    // lanes (TOTAL_ROWS, MAX_ROW_EXTENT) — apply produces every row maximum
    // (and the foldless x maxima); resolve_x the folding x maxima, so the x
    // tier waits for stage 4 only when the corpus folds.
    let host_key_to_float = |k: u32| -> f32 {
        let b = if (k & 0x8000_0000) != 0 { k & 0x7FFF_FFFF } else { !k };
        f32::from_bits(b)
    };
    let mut bad = 0usize;
    let mut max_x_dev = 0.0f64;
    if stages >= 3 && (!all_fold || stages >= 4) {
        // Pure-wrapped corpora compile apply's row maxima out — their rows
        // arrive with resolve_x (stage 4). Earlier bisection stages would
        // fail this diff spuriously.
        for (i, got) in rmax.iter().take(item_count).enumerate() {
            let want_rows = r.item_bounds[i * 8 + 6];
            if *got as f64 != want_rows {
                if bad < 8 {
                    println!("  MISMATCH item {i} total_rows: cpu {want_rows} gpu {got}");
                }
                bad += 1;
            }
        }
    }
    if stages >= 3 && (!needs_resolve || stages >= 4) {
        for (i, got) in xmax.iter().take(item_count).enumerate() {
            let want_x = r.item_bounds[i * 8 + 7];
            let got_x = host_key_to_float(*got) as f64;
            let x_dev = (got_x - want_x).abs() / want_x.abs().max(1.0);
            if x_dev > max_x_dev {
                max_x_dev = x_dev;
            }
        }
    }

    // Foldless items leave wm/wc/otb unwritten on purpose (apply resolves
    // them in-register) — the ord/line_adv diffs apply only to folding items.
    let fold_of_item: Vec<i64> = fx
        .items
        .iter()
        .map(|it| {
            if it.wrap_width > 0 {
                it.wrap_width
            } else if it.has_page && it.page_cols > 0 {
                it.page_cols
            } else {
                0
            }
        })
        .collect();
    let fold_at = |byte: usize| -> i64 {
        let mut f = 0i64;
        for (k, item) in fx.items.iter().enumerate() {
            if (item.byte_start as usize) <= byte
                && byte < ((item.byte_start + item.byte_count) as usize)
            {
                f = fold_of_item[k];
            }
        }
        f
    };

    // The diff: counts bit-exact; line_advance and positions reported with
    // max deviation and held to the oracle's 1e-4 eps tier (the module
    // header's contract note — the Blelloch tree reassociates tail_adv).
    let mut max_pos_dev = 0.0f64;
    let mut max_line_dev = 0.0f64;
    let mut leaders = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        leaders += 1;
        let folds = fold_at(id);
        let mut checks = vec![
            (r.slots.row(id), lc[id * LC_STRIDE + LC_ROW] as i64, "row"),
            (r.slots.col(id), lc[id * LC_STRIDE + LC_COL] as i64, "col"),
        ];
        if folds > 0 {
            checks.push((r.slots.wc[id] as i64, wc[id] as i64, "ord"));
        }
        for (want, got, name) in checks {
            if want != got {
                if bad < 8 {
                    println!("  MISMATCH byte {id} {name}: cpu {want} gpu {got}");
                }
                bad += 1;
            }
        }
        let la_cpu = r.slots.wm[id] as f64;
        if folds > 0 {
            let la_rel = (wm[id] as f64 - la_cpu).abs() / la_cpu.abs().max(1.0);
            if la_rel > max_line_dev {
                max_line_dev = la_rel;
            }
            // fold>0 X is a BIT-tier lane — the segment walk performs the
            // same re-sum adds in the same left-fold order, and this witness
            // holds it to that. (The eps-tier position diff below would hide
            // an order change; this cannot.) lm arrives with resolve_x
            // (stage 4) — partial-stage bisection skips it.
            if stages >= 4 && lm[id * LM_STRIDE + LM_X].to_bits() != r.slots.x(id).to_bits() {
                if bad < 8 {
                    println!(
                        "  MISMATCH byte {id} fold_x: cpu {:e} gpu {:e}",
                        r.slots.x(id),
                        lm[id * LM_STRIDE + LM_X]
                    );
                }
                bad += 1;
            }
        }
        // lm lanes exist from resolve_x (stage 4) on — partial-stage
        // bisection diffs the scan lanes only.
        if stages >= 4 {
            for (k, acc) in [(LM_X, r.slots.x(id)), (LM_Y, r.slots.y(id)), (LM_Z, r.slots.z(id))] {
                let dev = (lm[id * LM_STRIDE + k] as f64 - acc as f64).abs();
                let rel = dev / (acc as f64).abs().max(1.0);
                if rel > max_pos_dev {
                    max_pos_dev = rel;
                }
            }
        }
    }

    println!(
        "cubecl-chain-check: {} ({} B, {} items, {} leaders, tile {}x{}) — {} count-lane mismatches, \
         max line_adv deviation {:.2e}, max x-extent deviation {:.2e}, max position deviation {:.2e}; \
         chain+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        item_count,
        leaders,
        units,
        rake,
        bad,
        max_line_dev,
        max_x_dev,
        max_pos_dev,
        dt
    );
    if bad > 0 || max_line_dev > 1e-4 || max_x_dev > 1e-4 || max_pos_dev > 1e-4 {
        eprintln!(
            "cubecl-chain-check FAIL: {bad} count mismatches, {max_line_dev:.2e} line_adv, {max_x_dev:.2e} x-extent, {max_pos_dev:.2e} position deviation"
        );
        std::process::exit(1);
    }
    println!("cubecl-chain-check PASS: counts + rows exact, fold>0 X bit-exact, line_adv + foldless positions inside 1e-4");
    std::process::exit(0);
}

// ── the decode-check driver ──────────────────────────────────────────────────

/// `--cubecl-decode-check <fixture.pipe.bin>`: the device decode over one
/// fixture — packed flags and advance lanes diffed BIT-EXACT per byte
/// against `fold::decode_all` on a fresh Slots (leader mode; cluster
/// resolution is the separate phase-3b pass and the fixtures' cluster
/// lanes belong to it, not to decode).
pub fn decode_check(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-decode-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let mut slots = crate::fold::Slots::new(n);
    let _ = crate::fold::decode_all(&fx.bytes, &mut slots, &fx.trie);

    let n_words = n.div_ceil(4);
    let mut packed = vec![0u32; n_words];
    for (i, &b) in fx.bytes.iter().enumerate() {
        packed[i >> 2] |= (b as u32) << ((i & 3) * 8);
    }
    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&fx.trie.block_index));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_m));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_c));
    let h_fl = client.empty(n_words * 4);
    let h_sm = client.empty(n * 4);
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        decode::launch_unchecked(
            &client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_bi.clone(), fx.trie.block_index.len()),
            BufferArg::from_raw_parts(h_bm.clone(), fx.trie.blocks_m.len()),
            BufferArg::from_raw_parts(h_bc.clone(), fx.trie.blocks_c.len()),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            crate::glyph_trie::BLOCK_SHIFT,
        );
    }
    let fl_bytes = client.read_one(h_fl).expect("read fl");
    let sm_bytes = client.read_one(h_sm).expect("read sm");
    let dt = t0.elapsed();
    let flw: &[u32] = bytemuck::cast_slice(&fl_bytes);
    let sm: &[f32] = bytemuck::cast_slice(&sm_bytes);

    let mut bad = 0usize;
    for id in 0..n {
        let want_f = slots.flags(id) & 0xFF;
        let got_f = (flw[id >> 2] >> (((id & 3) * 8) as u32)) & 0xFF;
        if want_f != got_f {
            if bad < 8 {
                println!("  MISMATCH byte {id} flags: cpu {want_f:#04x} gpu {got_f:#04x}");
            }
            bad += 1;
        }
        if slots.advance(id).to_bits() != sm[id].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH byte {id} advance: cpu {:e} gpu {:e}",
                    slots.advance(id),
                    sm[id]
                );
            }
            bad += 1;
        }
    }
    println!(
        "cubecl-decode-check: {} ({} B, {} words) — {} lane mismatches; decode+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        n_words,
        bad,
        dt
    );
    if bad > 0 {
        eprintln!("cubecl-decode-check FAIL: {bad} lane mismatches vs decode_all");
        std::process::exit(1);
    }
    println!("cubecl-decode-check PASS: flags + advance bit-exact vs decode_all");
    std::process::exit(0);
}

// ── the cluster-check driver ──────────────────────────────────────────────────

/// Host-side cluster inputs from a flat sequence table: the candidacy
/// bitmap (one bit per codepoint, set for every sequence's first member)
/// and the per-item cluster flags.
pub(crate) fn cluster_host_inputs(
    seq: &[u32],
    seq_max: u32,
    items: &[crate::fold::Item],
) -> (Vec<u32>, Vec<u32>) {
    let stride = 2 + seq_max as usize;
    let mut bitmap = vec![0u32; 0x110000 / 32 + 1];
    for i in (0..seq.len()).step_by(stride) {
        let cp = seq[i + 2] as usize;
        bitmap[cp >> 5] |= 1 << (cp & 31);
    }
    let ic = items
        .iter()
        .map(|it| u32::from(it.cluster_mode == crate::fold::ClusterMode::Cluster))
        .collect();
    (bitmap, ic)
}

/// `--cubecl-cluster-check <fixture.pipe.bin>`: decode + cluster on device
/// vs `decode_all` + `resolve_clusters` on CPU — packed flags (low byte,
/// trailer bit included) and advance diffed BIT-EXACT per byte. Non-cluster
/// fixtures verify the pass is a no-op for leader items.
pub fn cluster_check(ctx: &GpuContext, fixture_path: &Path) -> ! {
    let fx = crate::fixture::load_pipe_fixture(fixture_path).unwrap_or_else(|e| {
        eprintln!("cubecl-cluster-check: {e}");
        std::process::exit(1);
    });
    let n = fx.bytes.len();
    let mut slots = crate::fold::Slots::new(n);
    let _ = crate::fold::decode_all(&fx.bytes, &mut slots, &fx.trie);
    for item in &fx.items {
        if item.cluster_mode == crate::fold::ClusterMode::Cluster {
            crate::fold::resolve_clusters(&fx.bytes, &mut slots, &fx.trie, item);
        }
    }

    let n_words = n.div_ceil(4);
    let mut packed = vec![0u32; n_words];
    for (i, &b) in fx.bytes.iter().enumerate() {
        packed[i >> 2] |= (b as u32) << ((i & 3) * 8);
    }
    let (seq, seq_max, bitmap_advance) = match fx.trie.cluster_table() {
        Some((s, m, a)) => (s.to_vec(), m, a),
        None => (Vec::new(), 2u32, f32::NAN),
    };
    // The probe's Shared key scratch is units x seq_max u32 per workgroup;
    // seq_max is TRIE DATA, so a pathological table could exceed Metal's
    // 32KB threadgroup budget at pipeline creation. Fail here, loudly,
    // instead of coupling kernel viability to atlas content.
    assert!(
        256 * seq_max as usize * 4 < 32 * 1024,
        "seq_max {seq_max} would exceed the 32KB shared budget"
    );
    let (bitmap, ic) = cluster_host_inputs(&seq, seq_max, &fx.items);
    let mut ir = Vec::with_capacity(fx.items.len() * 2);
    for item in &fx.items {
        ir.push(item.byte_start as u32);
        ir.push((item.byte_start + item.byte_count) as u32);
    }

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&fx.trie.block_index));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_m));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&fx.trie.blocks_c));
    let h_seq = client.create_from_slice(bytemuck::cast_slice(&seq));
    let h_bmap = client.create_from_slice(bytemuck::cast_slice(&bitmap));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ic = client.create_from_slice(bytemuck::cast_slice(&ic));
    let h_fl = client.empty(n_words * 4);
    let h_sm = client.empty(n * 4);
    let h_cslot = client.create_from_slice(bytemuck::cast_slice(&vec![0u32; n]));
    let h_cend = client.empty(n * 4);
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    let t0 = std::time::Instant::now();
    unsafe {
        decode::launch_unchecked(
            &client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_bi.clone(), fx.trie.block_index.len()),
            BufferArg::from_raw_parts(h_bm.clone(), fx.trie.blocks_m.len()),
            BufferArg::from_raw_parts(h_bc.clone(), fx.trie.blocks_c.len()),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            crate::glyph_trie::BLOCK_SHIFT,
        );
        cluster_probe::launch_unchecked(
            &client,
            cubes_of(n_words),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_bmap.clone(), bitmap.len()),
            BufferArg::from_raw_parts(h_seq.clone(), seq.len()),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            256,
            seq_max,
        );
        cluster_chain::launch_unchecked(
            &client,
            cubes_of(fx.items.len().max(1)),
            CubeDim::new_1d(256),
            BufferArg::from_raw_parts(h_bytes.clone(), n_words),
            BufferArg::from_raw_parts(h_ir.clone(), ir.len()),
            BufferArg::from_raw_parts(h_ic.clone(), ic.len()),
            BufferArg::from_raw_parts(h_cslot.clone(), n),
            BufferArg::from_raw_parts(h_cend.clone(), n),
            BufferArg::from_raw_parts(h_sm.clone(), n),
            BufferArg::from_raw_parts(h_fl.clone(), n_words),
            bitmap_advance,
        );
    }
    let fl_bytes = client.read_one(h_fl).expect("read fl");
    let sm_bytes = client.read_one(h_sm).expect("read sm");
    let dt = t0.elapsed();
    let flw: &[u32] = bytemuck::cast_slice(&fl_bytes);
    let sm: &[f32] = bytemuck::cast_slice(&sm_bytes);
    if std::env::var_os("GLYPH_CHAIN_DEBUG").is_some() {
        let cs = client.read_one(h_cslot).expect("read cslot");
        let ce = client.read_one(h_cend).expect("read cend");
        let csv: &[u32] = bytemuck::cast_slice(&cs);
        let cev: &[u32] = bytemuck::cast_slice(&ce);
        for i in 0..n {
            if csv[i] != 0 {
                println!("  dbg cslot[{i}]={} cend[{i}]={}", csv[i], cev[i]);
            }
        }
        let stride = 2 + seq_max as usize;
        println!("  dbg seq table stride {stride}, {} entries", seq.len() / stride);
        for e in 0..(seq.len() / stride).min(4) {
            let o = e * stride;
            let l = seq[o + 1] as usize;
            println!("  dbg entry {e}: slot {} len {} cps {:?}", seq[o], l, &seq[o + 2..o + 2 + l]);
        }
    }

    let mut bad = 0usize;
    for id in 0..n {
        let want_f = slots.flags(id) & 0xFF;
        let got_f = (flw[id >> 2] >> (((id & 3) * 8) as u32)) & 0xFF;
        if want_f != got_f {
            if bad < 8 {
                println!("  MISMATCH byte {id} flags: cpu {want_f:#04x} gpu {got_f:#04x}");
            }
            bad += 1;
        }
        if slots.advance(id).to_bits() != sm[id].to_bits() {
            if bad < 8 {
                println!(
                    "  MISMATCH byte {id} advance: cpu {:e} gpu {:e}",
                    slots.advance(id),
                    sm[id]
                );
            }
            bad += 1;
        }
    }
    println!(
        "cubecl-cluster-check: {} ({} B, {} items, {} seq entries) — {} lane mismatches; decode+cluster+readbacks {:?} (smoke timing only)",
        fx.name,
        n,
        fx.items.len(),
        seq.len() / (2 + seq_max as usize),
        bad,
        dt
    );
    if bad > 0 {
        eprintln!("cubecl-cluster-check FAIL: {bad} lane mismatches vs decode_all + resolve_clusters");
        std::process::exit(1);
    }
    println!("cubecl-cluster-check PASS: flags + advance bit-exact, cluster trailers and head advances included");
    std::process::exit(0);
}

// ── the bench driver ──────────────────────────────────────────────────────────

/// `--cubecl-chain-bench <corpus>`: the chain over a raw file as ONE item.
///
/// Timing is PER-DISPATCH GPU WINDOWS (`profile_start`/`profile_end` — device
/// timestamps when the shared device carries TIMESTAMP_QUERY, which gpu.rs
/// requests unconditionally where supported), `GLYPH_CHAIN_LOOP` samples per
/// dispatch with the MINIMUM kept (the "run it a few times" rule, automated).
/// Each window flushes, so stages cannot overlap: these are per-dispatch
/// latencies in the same posture as the Mojo bench's `mark()` table, and the
/// sum of minima is the chain estimate — the chain is dependency-serialized,
/// so nothing is lost to that. The old batched wall-clock loop mode is
/// retired: it measured repeat-overlap, not the chain.
///
/// `GLYPH_CHAIN_WRAP=<width>` swaps the single item to wrap_width>0 /
/// WRAP_DOWN so the segment re-sum in resolve_x actually executes — the plain
/// shape leaves that path dead at fold==0 and measures only atomic
/// throughput.
pub fn bench(ctx: &GpuContext, corpus_path: &Path) -> ! {
    let bytes = std::fs::read(corpus_path).unwrap_or_else(|e| {
        eprintln!("cubecl-chain-bench: {e}");
        std::process::exit(1);
    });
    let n = bytes.len();
    // The tile shape (the same dials as the check instrument, so a sweep
    // measures exactly what the fixtures verify).
    let units: usize = std::env::var("GLYPH_CHAIN_TILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let rake: usize = std::env::var("GLYPH_CHAIN_RAKE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    assert!(units.is_power_of_two(), "GLYPH_CHAIN_TILE must be a power of two");
    let log = units.ilog2() as usize;
    let n_tiles = n.div_ceil(units * rake).max(1);
    // resolve_x worker span (bytes per worker).
    let rspan: usize = std::env::var("GLYPH_CHAIN_SPAN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    // The wrapped shape: fold>0 makes resolve_x take the re-sum path.
    let wrap_width: i64 = std::env::var("GLYPH_CHAIN_WRAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let item = crate::fold::Item {
        byte_start: 0,
        byte_count: n as i64,
        origin_x: 0.0,
        origin_y: 0.0,
        origin_z: 0.0,
        wrap_width,
        wrap_mode: WrapMode::Down,
        cluster_mode: crate::fold::ClusterMode::Leader,
        z_step: 2.0,
        line_height: 1.25,
        has_page: false,
        page_rows: 0,
        page_cols: 0,
        scroll_rows: 0,
        pages_wide: 1,
        page_gap_x: 0.0,
        band_stride_y: 0.0,
        depth_per_band: 0.0,
        depth_per_col: 0.0,
        page_line_height: 1.25,
    };
    let items = [item];
    let trie = crate::atlas::TrieTable::load(&crate::atlas_dir());
    // The bench item folds iff GLYPH_CHAIN_WRAP is set — foldless runs skip
    // the resolve_x dispatch entirely (apply resolves them), and pure-wrapped
    // runs compile apply's inline branch out.
    let needs_resolve = wrap_width > 0;
    let inline_resolve = wrap_width == 0;
    // GLYPH_CHAIN_DECODE=1: the chain starts from raw BYTES — the device
    // decode produces fl/sm (phase 3a) and the CPU statics upload dies; the
    // CPU reference still runs for verification. Decode is stage 1 then, and
    // GLYPH_CHAIN_STAGES counts it.
    let decode_mode = std::env::var_os("GLYPH_CHAIN_DECODE").is_some();

    let t_decode = std::time::Instant::now();
    let r = run_scan_pipeline(&bytes, &trie, &items, DEFAULT_CHUNK_SIZE, DEFAULT_GROUP_SIZE, 1);
    let decode_dt = t_decode.elapsed();

    let ir: Vec<u32> = vec![0, n as u32];
    let ie: Vec<u32> = vec![0, 0, 0, 1, wrap_width as u32, 0, 0, 0];
    let im: Vec<f32> = vec![0.0, 0.0, 1.25, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let page_gap_x: Vec<f32> = vec![0.0];

    let setup = WgpuSetup {
        instance: ctx.instance.clone(),
        adapter: ctx.adapter.clone(),
        device: ctx.device.clone(),
        queue: ctx.queue.clone(),
        backend: AutoGraphicsApi::backend(),
    };
    let cdev = cubecl::wgpu::init_device(setup, Default::default());
    let client = cubecl::Device::Wgpu(cdev).client();
    let n_words = n.div_ceil(4);
    let (h_fl, h_sm) = if decode_mode {
        (client.empty(n_words * 4), client.empty(n * 4))
    } else {
        // Statics: advance (f32/byte) + PACKED flags (u8/byte, four per word —
        // the chain reads fl three-to-four passes and consumes only the low
        // byte; the full flags stay CPU-side for the renderer).
        let mut fl = Vec::with_capacity(n_words);
        let mut sm = Vec::with_capacity(n);
        for w in 0..n_words {
            let mut word = 0u32;
            for b in 0..4 {
                let i = w * 4 + b;
                if i < n {
                    word |= (r.slots.flags(i) & 0xFF) << (b * 8);
                }
            }
            fl.push(word);
        }
        for i in 0..n {
            sm.push(r.slots.advance(i));
        }
        (
            client.create_from_slice(bytemuck::cast_slice(&fl)),
            client.create_from_slice(bytemuck::cast_slice(&sm)),
        )
    };
    // The decode's inputs: the corpus packed four bytes per word, and the
    // atlas trie's tables pre-converted to world units.
    let mut packed = vec![0u32; n_words];
    for (i, &b) in bytes.iter().enumerate() {
        packed[i >> 2] |= (b as u32) << ((i & 3) * 8);
    }
    let (bi, bm, bc, bshift) = trie.device_tables();
    let h_bytes = client.create_from_slice(bytemuck::cast_slice(&packed));
    let h_bi = client.create_from_slice(bytemuck::cast_slice(&bi));
    let h_bm = client.create_from_slice(bytemuck::cast_slice(&bm));
    let h_bc = client.create_from_slice(bytemuck::cast_slice(&bc));
    let h_ir = client.create_from_slice(bytemuck::cast_slice(&ir));
    let h_ie = client.create_from_slice(bytemuck::cast_slice(&ie));
    let h_im = client.create_from_slice(bytemuck::cast_slice(&im));
    let h_gap = client.create_from_slice(bytemuck::cast_slice(&page_gap_x));
    let h_tc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_tm = client.empty(n_tiles * 4);
    let h_xc = client.empty(n_tiles * PARTIAL_COUNT_STRIDE * 4);
    let h_xm = client.empty(n_tiles * 4);
    let h_lc = client.empty(n * LC_STRIDE * 4);
    let h_wm = client.empty(n * 4);
    let h_wc = client.empty(n * 4);
    let h_otb = client.empty(n * 4);
    let h_lm = client.empty(n * LM_STRIDE * 4);
    let h_strides = client.empty(4);
    let h_rmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));
    let h_xmax = client.create_from_slice(bytemuck::cast_slice(&[0u32]));

    // This M2's ADAPTER caps workgroups per grid dimension at 65535 (verified
    // live: a 94075-cube dispatch was rejected) — not a wgpu default to lift.
    // Spill into Y; ABSOLUTE_POS is the flattened id across axes, so the
    // kernels need no index change. gpu.rs still requests the adapter's value,
    // so an adapter with a higher cap takes the plain grid automatically.
    let cubes_of = |threads: usize| {
        let cubes = threads.div_ceil(256);
        CubeCount::Static(cubes.min(65535) as u32, cubes.div_ceil(65535) as u32, 1)
    };
    // GLYPH_CHAIN_STAGES is an ABSOLUTE dispatch count (the check driver's
    // semantics): with GLYPH_CHAIN_DECODE=1 it counts the decode stage.
    let stages: usize = match std::env::var("GLYPH_CHAIN_STAGES").ok().and_then(|v| v.parse().ok()) {
        Some(v) => v,
        None => 6 + decode_mode as usize,
    };
    // One cube per tile (dim = units); the byte-wide kernels stay 256-unit.
    let tiles_grid = |tiles: usize| {
        CubeCount::Static(tiles.min(65535) as u32, tiles.div_ceil(65535) as u32, 1)
    };
    // Timed samples per dispatch; the minimum is reported.
    let samples: usize = std::env::var("GLYPH_CHAIN_LOOP").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let launch = |s: usize| {
        if decode_mode && s == 0 {
            unsafe {
                decode::launch_unchecked(
                    &client,
                    cubes_of(n_words),
                    CubeDim::new_1d(256),
                    BufferArg::from_raw_parts(h_bytes.clone(), n_words),
                    BufferArg::from_raw_parts(h_bi.clone(), bi.len()),
                    BufferArg::from_raw_parts(h_bm.clone(), bm.len()),
                    BufferArg::from_raw_parts(h_bc.clone(), bc.len()),
                    BufferArg::from_raw_parts(h_fl.clone(), n_words),
                    BufferArg::from_raw_parts(h_sm.clone(), n),
                    bshift,
                );
            }
            return;
        }
        let t = s - decode_mode as usize;
        unsafe {
            match t {
                0 => {
                    tile_scan::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        units,
                        rake,
                        log,
                    );
                }
                1 => {
                    spine_scan::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_tc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_tm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        units,
                        log,
                    );
                }
                2 => {
                    apply::launch_unchecked(
                        &client,
                        tiles_grid(n_tiles),
                        CubeDim::new_1d(units as u32),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_sm.clone(), n),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_xc.clone(), n_tiles * PARTIAL_COUNT_STRIDE),
                        BufferArg::from_raw_parts(h_xm.clone(), n_tiles),
                        BufferArg::from_raw_parts(h_wm.clone(), n),
                        BufferArg::from_raw_parts(h_wc.clone(), n),
                        BufferArg::from_raw_parts(h_otb.clone(), n),
                        BufferArg::from_raw_parts(h_rmax.clone(), 1),
                        BufferArg::from_raw_parts(h_xmax.clone(), 1),
                        units,
                        rake,
                        log,
                        inline_resolve,
                    );
                }
                3 => {
                    if needs_resolve {
                        resolve_x::launch_unchecked(
                            &client,
                            cubes_of(n.div_ceil(rspan)),
                            CubeDim::new_1d(256),
                            BufferArg::from_raw_parts(h_sm.clone(), n),
                            BufferArg::from_raw_parts(h_fl.clone(), n_words),
                            BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                            BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                            BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                            BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                            BufferArg::from_raw_parts(h_ir.clone(), 2),
                            BufferArg::from_raw_parts(h_wc.clone(), n),
                            BufferArg::from_raw_parts(h_otb.clone(), n),
                            BufferArg::from_raw_parts(h_rmax.clone(), 1),
                            BufferArg::from_raw_parts(h_xmax.clone(), 1),
                            256,
                            rspan,
                        );
                    }
                }
                4 => {
                    derive_stride::launch_unchecked(
                        &client,
                        CubeCount::new_single(),
                        CubeDim::new_1d(1),
                        BufferArg::from_raw_parts(h_xmax.clone(), 1),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_gap.clone(), 1),
                        BufferArg::from_raw_parts(h_strides.clone(), 1),
                    );
                }
                _ => {
                    paginate::launch_unchecked(
                        &client,
                        cubes_of(n),
                        CubeDim::new_1d(256),
                        BufferArg::from_raw_parts(h_lm.clone(), n * LM_STRIDE),
                        BufferArg::from_raw_parts(h_fl.clone(), n_words),
                        BufferArg::from_raw_parts(h_lc.clone(), n * LC_STRIDE),
                        BufferArg::from_raw_parts(h_im.clone(), IM_STRIDE),
                        BufferArg::from_raw_parts(h_ie.clone(), IE_STRIDE),
                        BufferArg::from_raw_parts(h_ir.clone(), 2),
                        BufferArg::from_raw_parts(h_strides.clone(), 1),
                    );
                }
            }
        }
    };
    // The per-dispatch GPU windows. Every stage runs `samples` times; the
    // minimum survives. A window that resolved to no measurement counts as a
    // missing sample, never as a zero.
    let mut stage_names: Vec<&'static str> = Vec::new();
    if decode_mode {
        stage_names.push("decode");
    }
    stage_names.extend([
        "tile_scan",
        "spine_scan",
        "apply",
        "resolve_x",
        "derive_stride",
        "paginate",
    ]);
    let stage_meta = |s: usize| -> (&'static str, usize, u32) {
        if decode_mode && s == 0 {
            return ("decode", n_words.div_ceil(256), 256);
        }
        let t = s - decode_mode as usize;
        let (cubes, dim) = match t {
            0 | 2 => (n_tiles, units as u32),
            1 => (1, units as u32),
            3 => (n.div_ceil(rspan).div_ceil(256), 256),
            4 => (1, 1),
            _ => (n.div_ceil(256), 256),
        };
        (stage_names[s], cubes, dim)
    };
    let mut mins: Vec<Option<std::time::Duration>> = vec![None; stages];
    let mut missing_windows = 0usize;
    let mut timing_method = String::new();
    for _ in 0..samples {
        for (s, slot) in mins.iter_mut().enumerate() {
            if s == 3 + decode_mode as usize && !needs_resolve {
                // The resolve_x slot; foldless corpora skip the dispatch
                // (and its window) entirely.
                continue;
            }
            let window = client.profile_start().expect("profile_start");
            launch(s);
            let dur = client.profile_end(window).expect("profile_end");
            if timing_method.is_empty() {
                timing_method = format!("{}", dur.timing_method());
            }
            match pollster::block_on(dur.resolve()) {
                Some(ticks) => {
                    let d = ticks.duration();
                    *slot = Some(slot.map_or(d, |cur| cur.min(d)));
                }
                None => missing_windows += 1,
            }
        }
    }

    // Readbacks: host-side, wall clock (the product flow binds instead).
    let t1 = std::time::Instant::now();
    let lc_bytes = client.read_one(h_lc.clone()).expect("read lc");
    let _wc = client.read_one(h_wc.clone()).expect("read wc");
    let _wm = client.read_one(h_wm.clone()).expect("read wm");
    let _lm = client.read_one(h_lm.clone()).expect("read lm");
    let readback_dt = t1.elapsed();
    // Correctness at speed: the bench's whole number is worthless if the fast
    // path is wrong — diff the leader row/col lanes against the CPU reference.
    let lc: &[u32] = bytemuck::cast_slice(&lc_bytes);
    if stages < 3 + decode_mode as usize {
        println!(
            "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}) stages {} — pre-apply stages only, no verification",
            corpus_path.display(),
            n,
            n_tiles,
            units,
            rake,
            wrap_width,
            stages
        );
        for (s, m) in mins.iter().enumerate() {
            let (name, cubes, dim) = stage_meta(s);
            println!("  {name:<14} cubes={cubes} units={dim} min={m:?}");
        }
        std::process::exit(0);
    }
    let mut bad = 0usize;
    for id in 0..n {
        if r.slots.flags(id) & F_LEADER == 0 {
            continue;
        }
        if r.slots.row(id) != lc[id * LC_STRIDE + LC_ROW] as i64
            || r.slots.col(id) != lc[id * LC_STRIDE + LC_COL] as i64
        {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "bench verification failed: {bad} leader lane mismatches");

    let chain: std::time::Duration = mins.iter().filter_map(|d| *d).sum();
    let total = chain + readback_dt;
    println!(
        "cubecl-chain-bench: {} ({} B, {} tiles @ {}x{}, wrap {}, samples {}) timing={} missing_windows={} — \
         cpu decode+scan {:?} | chain (sum of per-dispatch minima) {:?} readbacks {:?} total {:?} ({:.1} MB/s)",
        corpus_path.display(),
        n,
        n_tiles,
        units,
        rake,
        wrap_width,
        samples,
        timing_method,
        missing_windows,
        decode_dt,
        chain,
        readback_dt,
        total,
        n as f64 / 1e6 / total.as_secs_f64()
    );
    for (s, m) in mins.iter().enumerate() {
        let (name, cubes, dim) = stage_meta(s);
        println!("  {name:<14} cubes={cubes:>7} units={dim} min={m:?}");
    }
    std::process::exit(0);
}
