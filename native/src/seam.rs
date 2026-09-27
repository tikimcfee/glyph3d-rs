//! The Zed-integration seam — envelope types and the join rule.
//!
//! Contract: `zed_integration_experiments/docs/seam.md` (frozen for P1).
//! Everything here is provider-neutral vocabulary — NO Zed types cross into
//! the renderer core; the provider implementation lives in the experiment
//! workspace and speaks these types over a channel.
//!
//! The two laws this module exists to enforce:
//!   1. VERSION-STAMPED JOINS, never ambient remapping — an update's offsets
//!      are valid against the content of exactly one `BufferVersion`, and the
//!      renderer checks equality against the version it folded. Mismatch is a
//!      visible drop, not a translation.
//!   2. NO CORPUS DUPLICATION — content crosses the seam ONCE per file
//!      (`ContentDelta::Opened`); afterwards only edits. The renderer owns
//!      its byte copy and applies deltas in place; the provider never ships
//!      the whole text again just to relocate glyphs.

use std::ops::Range;

/// Monotonic content version of one buffer, assigned by the provider at
/// every change. Opaque to the renderer beyond equality/ordering.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BufferVersion(pub u64);

/// v1 file identity: the repo scene's `rel_path`. Graduates to a Zed
/// `ProjectPath`-shaped key when the spatial-workspace grammar lands —
/// newtype now so that change is a type change, not a string-convention
/// change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileKey(pub String);

/// How content reaches the renderer. The corpus rule lives here:
/// `Opened` exactly once per file, `Edited` deltas thereafter, `Tombstone`
/// when the file is gone (records survive for undo-friendly revival).
#[derive(Clone, Debug)]
pub enum ContentDelta {
    /// The full bytes at `version` — sent once, on open.
    Opened(Vec<u8>),
    /// Replace `range` (byte offsets in the PREVIOUS version) with `text`.
    /// In-place on the renderer's copy; never a second corpus.
    Edited { range: Range<usize>, text: Vec<u8> },
    /// File deleted/unwatched.
    Tombstone,
}

impl ContentDelta {
    /// Apply this delta to the renderer's owned byte copy. Returns whether
    /// the copy is still live (`false` only for `Tombstone`).
    pub fn apply(&self, bytes: &mut Vec<u8>) -> bool {
        match self {
            ContentDelta::Opened(full) => {
                bytes.clear();
                bytes.extend_from_slice(full);
                true
            }
            ContentDelta::Edited { range, text } => {
                let start = range.start.min(bytes.len());
                let end = range.end.clamp(start, bytes.len());
                bytes.splice(start..end, text.iter().copied());
                true
            }
            ContentDelta::Tombstone => false,
        }
    }
}

/// One styled byte range at one version. sRGB bytes — the same packed form
/// `Verb::RecolorGlyph` and `--highlight` write today. Weight/italic arrive
/// when the field can honor them (P2+), as new fields, not a new type.
/// (Clone, not Copy: `Range` is move-iterator territory.)
#[derive(Clone, Debug, PartialEq)]
pub struct StyleRun {
    pub range: Range<usize>,
    pub rgb: [u8; 3],
}

/// The version join — THE rule of the seam. `folded` is the version whose
/// bytes the renderer folded; an update's offsets are meaningful against
/// that version and nothing else. Equality or drop; there is no translation
/// layer to drift.
pub fn joins(folded: BufferVersion, update: &SurfaceUpdate) -> bool {
    folded == update.version
}

/// Version for FILE-DRIVEN providers: content identity. A within-process
/// hash (not stable across rustc versions — it never needs to be; the join
/// happens in the process that folded). For static content, identity IS the
/// version; live providers use edit counters instead. The join (equality)
/// works for both.
pub fn content_hash_version(bytes: &[u8]) -> BufferVersion {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    BufferVersion(h.finish())
}

/// Structure-plane stages arrive as variants here — through the same
/// envelope, never a second pipeline. The first (P2a):
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructureDelta {
    /// Fold ONE byte range of the file at this version — DROP the records
    /// whose lines fall inside it and shift everything below up by the
    /// hidden pitch. v1 has no placeholder row: an ellipsis row is an
    /// INSERT, i.e. the inlay variant wearing a costume, and drop-then-
    /// rebuild is the primitive that makes later variants cheap. The
    /// renderer NORMALIZES each range outward to whole lines (see
    /// [`normalized_fold_lines`]), so providers may pass raw tree-sitter
    /// node spans. Overlaps, nesting and duplicates merge.
    Fold { range: Range<usize> },
}

/// Resolve fold byte-ranges against a content version's LINE STARTS: each
/// range expands to the whole lines containing its first and last bytes,
/// then the resulting line ranges are sorted and merged (overlap, nesting,
/// adjacency). This is the seam's only structure-side coordinate
/// conversion — byte space (the envelope's one coordinate system, your
/// P2a decision) to line space, as a PURE function so the renderer's
/// geometry never sees an unnormalized fold. `line_starts` is the byte
/// offset of every line's first byte, ascending, starting 0; empty fold
/// ranges vanish (they contain no lines).
pub fn normalized_fold_lines(folds: &[StructureDelta], line_starts: &[usize]) -> Vec<Range<u32>> {
    let mut lines: Vec<Range<u32>> = Vec::new();
    for delta in folds {
        let StructureDelta::Fold { range } = delta;
        if range.start >= range.end {
            continue; // empty or inverted — contains no lines
        }
        // The line containing a byte: the LAST line start at/before it.
        // (For range.end the byte that matters is end-1 — exclusive end.)
        // Bytes past the end clamp to the last line — a provider's span can
        // name EOF; it must not invent a line beyond it.
        let last_start = line_starts.last().copied().unwrap_or(0);
        let line_of = |byte: usize| -> u32 {
            match line_starts.binary_search(&byte) {
                Ok(i) => i as u32,
                Err(0) => 0,
                Err(i) => (i - 1) as u32,
            }
        };
        let first = line_of(range.start.min(last_start));
        let last = line_of(range.end.saturating_sub(1).min(last_start));
        // `last` inclusive → exclusive: fold lines first..=last.
        lines.push(first..last + 1);
    }
    // Sort, then merge overlap/nesting/adjacency — adjacency folds collapse
    // because two touching hidden line ranges are indistinguishable from
    // one. (Collection order must not matter: the merge happens entirely
    // after the sort.)
    lines.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<Range<u32>> = Vec::with_capacity(lines.len());
    for r in lines {
        match out.last_mut() {
            Some(prev) if r.start <= prev.end => prev.end = prev.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Decoration-plane items (selections, carets, search hits) — P4. Same law
/// as [`StructureDelta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoration {}

/// Everything the provider knows about one buffer at one version. Stage
/// scope is which fields are POPULATED — `structure`/`decorations` are
/// empty, not absent, in style-only v1.
#[derive(Clone, Debug)]
pub struct SurfaceUpdate {
    pub file: FileKey,
    pub version: BufferVersion,
    pub content: ContentDelta,
    /// Style runs over THIS version's byte space.
    pub style: Vec<StyleRun>,
    /// Applied in order at this version. Empty until the structure plane
    /// ships (P2).
    pub structure: Vec<StructureDelta>,
    /// Empty until P4.
    pub decorations: Vec<Decoration>,
    /// Parse-in-flight: apply the partial runs honestly rather than waiting.
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(version: u64) -> SurfaceUpdate {
        SurfaceUpdate {
            file: FileKey("a.rs".into()),
            version: BufferVersion(version),
            content: ContentDelta::Opened(b"fn main() {}".to_vec()),
            style: Vec::new(),
            structure: Vec::new(),
            decorations: Vec::new(),
            complete: true,
        }
    }

    #[test]
    fn join_is_version_equality_only() {
        assert!(joins(BufferVersion(7), &update(7)));
        // Either direction of mismatch drops: stale updates AND updates for
        // a fold the renderer hasn't made yet. No translation exists.
        assert!(!joins(BufferVersion(6), &update(7)));
        assert!(!joins(BufferVersion(8), &update(7)));
    }

    #[test]
    fn opened_replaces_and_edits_splice_in_place() {
        let mut bytes = Vec::new();
        assert!(ContentDelta::Opened(b"hello world".to_vec()).apply(&mut bytes));
        assert_eq!(bytes, b"hello world");
        // Replace "world" ([6..11)) with longer text — the corpus rule: the
        // same buffer object, no second copy, offsets applied in place.
        assert!(ContentDelta::Edited {
            range: 6..11,
            text: b"there".to_vec(),
        }
        .apply(&mut bytes));
        assert_eq!(bytes, b"hello there");
        // A shrinking edit replaces "there" with "t"; then an out-of-bounds
        // range clamps rather than panics — a provider bug must not take the
        // render thread down.
        assert!(ContentDelta::Edited { range: 6..11, text: b"t".to_vec() }.apply(&mut bytes));
        assert_eq!(bytes, b"hello t");
        assert!(ContentDelta::Edited { range: 100..200, text: Vec::new() }.apply(&mut bytes));
        assert_eq!(bytes, b"hello t");
    }

    #[test]
    fn tombstone_ends_the_copy() {
        let mut bytes = b"x".to_vec();
        assert!(!ContentDelta::Tombstone.apply(&mut bytes));
    }

    #[test]
    fn content_hash_versions_track_content_not_order() {
        use super::content_hash_version;
        let a = b"fn main() {}".to_vec();
        let b = a.clone();
        assert_eq!(content_hash_version(&a), content_hash_version(&b));
        let mut c = a.clone();
        c[0] = b'x';
        assert_ne!(content_hash_version(&a), content_hash_version(&c));
    }

    // ── P2a: folds ────────────────────────────────────────────────────────
    use super::normalized_fold_lines;

    fn fold(range: Range<usize>) -> StructureDelta {
        StructureDelta::Fold { range }
    }

    #[test]
    fn folds_expand_outward_to_whole_lines() {
        // 3 lines "abc\nDEFG\nhi\n": starts [0,4,9], len 12.
        let starts = [0usize, 4, 9];
        // Bytes 5..7 ("EF", mid-line 1) folds exactly line 1.
        assert_eq!(normalized_fold_lines(&[fold(5..7)], &starts), vec![1..2]);
        // Bytes 2..10 (mid-line-0 through mid-line-2) folds whole lines 0..2.
        assert_eq!(normalized_fold_lines(&[fold(2..10)], &starts), vec![0..3]);
        // A fold naming byte 0 expands to line 0 (not "before" anything).
        assert_eq!(normalized_fold_lines(&[fold(0..1)], &starts), vec![0..1]);
    }

    #[test]
    fn folds_merge_overlap_nesting_adjacency_regardless_of_order() {
        // 6 lines of 2 bytes each: starts [0,2,4,6,8,10].
        let starts = [0usize, 2, 4, 6, 8, 10];
        // Line 1 (bytes 2..3), line 2 (4..5), line 3 (6..7), given unsorted:
        // adjacent 1..2 and 2..3 collapse with 3..4 into one 1..4 range.
        let out = normalized_fold_lines(&[fold(6..7), fold(2..3), fold(4..5)], &starts);
        assert_eq!(out, vec![1..4]);
        // Overlap and nesting collapse to the union.
        let out = normalized_fold_lines(&[fold(4..5), fold(0..9)], &starts);
        assert_eq!(out, vec![0..5]);
        // Disjoint stays disjoint, sorted output order.
        let out = normalized_fold_lines(&[fold(8..9), fold(0..1)], &starts);
        assert_eq!(out, vec![0..1, 4..5]);
    }

    #[test]
    fn degenerate_folds_vanish_or_clamp() {
        let starts = [0usize, 4];
        // Empty (and inverted) ranges contain no lines.
        assert_eq!(
            normalized_fold_lines(&[fold(3..3), fold(5..2)], &starts),
            Vec::<Range<u32>>::new()
        );
        // A byte past EOF clamps to the LAST line — never invents one.
        assert_eq!(normalized_fold_lines(&[fold(99..100)], &starts), vec![1..2]);
        // The final line without a trailing newline still resolves.
        assert_eq!(normalized_fold_lines(&[fold(5..6)], &starts), vec![1..2]);
    }

    #[test]
    fn style_runs_carry_byte_ranges_and_srgb() {
        let u = update(1);
        assert_eq!(u.style, Vec::new());
        assert!(u.complete);
        let run = StyleRun { range: 0..3, rgb: [0xbc, 0x74, 0xd2] };
        assert_eq!(run.range, 0..3);
        assert_eq!(run.rgb, [188, 116, 210]);
    }
}
