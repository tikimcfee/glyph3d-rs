//! Every glyph shader's `MAX_CURVES` covers the atlas's largest outline.
//!
//! The fragment stage loops over `min(curve_count, MAX_CURVES)` curves, so a
//! glyph with more curves than the bound is drawn cut off: parts missing, and
//! a winding left open smears coverage down the cell. The Derived shader
//! carried 128 against the Instanced shader's 256 from the initial snapshot
//! until 2026-10-11, and the atlas's largest outline has 468; with Derived
//! the default, ☃ lost its eyes and ☠ smeared a bar down its cell. No golden
//! frame holds a glyph over 128 curves, so no pixel check could see it.
//!
//! This test READS the shaders and the atlas; it never modifies them.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The glyph field shaders that run the curve loop (Visible draws through
/// the Derived one).
const SHADERS: [&str; 2] = [
    "crates/glyph-field-derived/shaders/glyph_field_derived.wgsl",
    "crates/glyph-field-instanced/shaders/glyph_field.wgsl",
];

fn max_curves(path: &Path) -> u32 {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let line = src
        .lines()
        .find(|l| l.trim_start().starts_with("const MAX_CURVES: u32 ="))
        .unwrap_or_else(|| panic!("{}: no `const MAX_CURVES: u32 =` line", path.display()));
    let value = line.split('=').nth(1).unwrap().trim().trim_end_matches(';').trim_end_matches('u');
    value.parse().unwrap_or_else(|e| panic!("{}: MAX_CURVES {value:?}: {e}", path.display()))
}

/// The largest curve count of any outline slot in `glyphmap.bin`
/// (`assets/atlas/FORMAT.md`: texel `[curveStart, curveCount, mode,
/// emojiCell]`, mode 0 = outline), and how many slots were read.
fn largest_outline() -> (u32, usize) {
    let path = repo().join("assets/atlas/glyphmap.bin");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!(&bytes[0..4], b"G3GM", "glyphmap.bin magic");
    let entries = word(5) as usize;
    let header_words = word(2) as usize / 4;
    let mut largest = 0;
    for g in 0..entries {
        let texel = header_words + g * 4;
        if word(texel + 2) == 0 {
            largest = largest.max(word(texel + 1));
        }
    }
    (largest, entries)
}

#[test]
fn every_glyph_shader_covers_the_largest_outline() {
    let (largest, entries) = largest_outline();
    // Refuse to pass over an atlas that read as nothing.
    assert!(entries > 4_000, "glyphmap.bin read {entries} slots");
    assert!(largest > 128, "largest outline read as {largest} curves; the atlas has outlines over 128");
    for rel in SHADERS {
        let cap = max_curves(&repo().join(rel));
        assert!(cap >= largest, "{rel}: MAX_CURVES {cap} cuts off the atlas's largest outline ({largest} curves)");
    }
}
