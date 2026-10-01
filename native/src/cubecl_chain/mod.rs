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

mod bench;
mod checks;
mod cluster;
mod decode;
mod monoid;
mod position;
mod repo;
mod repo_check;
mod scan;
mod tail;

pub use bench::bench;
pub use checks::{cluster_check, decode_check, run};
pub use repo_check::repo_check;
pub use repo::ChainPhases;
pub(crate) use repo::{ChainMode, InstanceInputs, SharedDevice, run_repo_chain};

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

const LM_STRIDE: usize = 4;
const LM_X: usize = 0;
const LM_Y: usize = 1;
const LM_Z: usize = 2;
const LM_BASE_X: usize = 3;
const LC_STRIDE: usize = 2;
const LC_ROW: usize = 0;
const LC_COL: usize = 1;
const IM_STRIDE: usize = 10;
const IM_ORIGIN_Y: usize = 0;
const IM_ORIGIN_Z: usize = 1;
const IM_LINE_HEIGHT: usize = 2;
const IM_Z_STEP: usize = 3;
const IM_BAND_STRIDE_Y: usize = 4;
const IM_DEPTH_PER_BAND: usize = 5;
const IM_DEPTH_PER_COL: usize = 6;
const IM_ORIGIN_X: usize = 8;
/// The z_step's f64 tail as a second f32 — the engine multiplies the FULL
/// f64 param and the correctly-rounded lane alone measurably diverges at
/// seg >= 3 (the wide-repo Z class: fl(3·0.15000000596) vs the engine's
/// fl(3·0.1499999999999999944), one ulp apart). The outer fma folds this
/// tail back in; see paginate's fma note.
const IM_Z_STEP_LO: usize = 9;
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

// ── the driver ────────────────────────────────────────────────────────────────

/// The corpus packed four bytes per u32 word for the device decode, tail
/// lanes of the final word filled with 0x80 — a CONTINUATION lead, which the
/// lenient classifier reads as a non-leader. Zero pads instead classify as
/// phantom 1-byte NUL leaders with a resolved advance: every byte-indexed
/// kernel bounds itself by the rounded-UP word count, so scan totals inflate
/// and phantom statics writes land past buffers sized by the real n —
/// discarded by WGSL's robustness on Metal (why every gate stayed green
/// through the bug), real out-of-bounds writes on non-robust backends.
/// Found by the rung-4 grounding review.
///
/// The fill is a SEPARATE pass because the first landing guarded it with
/// `if i < n` inside the loop over the real bytes — a branch that can never
/// fire — and every gate stayed green through the no-op: trailing phantom
/// records self-truncate past `total_records`, so no device gate can see the
/// class (the fork gate's attempted mutation was dropped for exactly this).
/// The reddening witness is the unit test in this file; the classifier's
/// continuation rule itself is fenced by the real-byte flags diffs.
fn pack_words(bytes: &[u8]) -> Vec<u32> {
    let n_words = bytes.len().div_ceil(4);
    let mut packed = vec![0u32; n_words];
    for (i, &b) in bytes.iter().enumerate() {
        packed[i >> 2] |= (b as u32) << ((i & 3) * 8);
    }
    for i in bytes.len()..(n_words * 4) {
        packed[i >> 2] |= 0x80u32 << ((i & 3) * 8);
    }
    packed
}

#[cfg(test)]
mod tests {
    /// The phantom-tail class (see pack_words): a non-word-aligned corpus
    /// must fill the final word's tail lanes with 0x80 continuation leads —
    /// zero pads decode on device as phantom NUL leaders. This test is the
    /// reddening witness for the `tail-pads-zero` mutation: the device gates
    /// cannot carry it (trailing phantom records self-truncate past the
    /// record count, so the cubecl gates stay green through the bug — the
    /// fork gate's attempted mutation was dropped for exactly that), while
    /// the classifier's continuation rule they DO fence is only half the
    /// class. Three of the five cubecl-chain gate fixtures are
    /// non-word-aligned; the standing fork corpus's 278,470 bytes are too.
    #[test]
    fn pack_words_fills_tail_lanes() {
        assert_eq!(super::pack_words(&[0x41]), vec![0x8080_8041u32]);
        assert_eq!(super::pack_words(&[0x41, 0x42]), vec![0x8080_4241u32]);
        // Word-aligned input: no fill, the words carry exactly the bytes.
        assert_eq!(
            super::pack_words(&[0x41, 0x42, 0x43, 0x44]),
            vec![0x4443_4241u32]
        );
        // Empty input: no words at all (the pre-helper behavior).
        assert!(super::pack_words(&[]).is_empty());
    }
}
