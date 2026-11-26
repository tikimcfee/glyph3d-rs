//! Integration tests for colour-emoji routing (`EmojiSource` / `EmojiSheetIndex`) and the
//! single-entry-point cell resolver (`FontAtlas::resolve_cell`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glyph_font::constants::{BLANK_SLOT, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE};
use glyph_font::{
    default_font_chain, EmojiSheetIndex, EmojiSource, Face, FontAtlas, ResolvedCell, SlotKind,
    DEFAULT_FONT_FAMILY, MONASPACE_ARGON_FAMILY, MONASPACE_NEON_FAMILY,
};
use term_cells::{attrs, rgb, Cell, Screen, CLUSTER_BASE};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn assets_fonts_dir() -> PathBuf {
    workspace_root().join("assets/fonts")
}

fn assets_atlas_dir() -> PathBuf {
    workspace_root().join("assets/atlas")
}

fn read_baked_glyphmap_entry(slot: u32) -> [u32; 4] {
    let bytes = std::fs::read(assets_atlas_dir().join("glyphmap.bin")).expect("read glyphmap.bin");
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect();
    assert_eq!(words[0], u32::from_le_bytes(*b"G3GM"));
    let hdr = (words[2] / 4) as usize;
    let o = hdr + slot as usize * 4;
    [words[o], words[o + 1], words[o + 2], words[o + 3]]
}

#[test]
fn emoji_sheet_index_matches_baked_atlas_cells() {
    let index = EmojiSheetIndex::load(&assets_atlas_dir().join("emoji-sheet.bin"))
        .expect("load emoji-sheet.bin");
    assert_eq!(index.cell_count(), 3985);
    assert_eq!(index.sequence_count(), 4166);

    // Pinned against FORMAT.md and baked glyphmap.bin:
    //   '🐀' (U+1F400) -> baked slot 3839 -> [0, 0, 1, 487]
    //   '🚀' (U+1F680) -> baked slot 4759 -> [0, 0, 1, 958]
    //   ZWJ family U+1F468 U+200D U+1F469 U+200D U+1F467 -> baked slot 6819
    assert_eq!(index.cell_for("🐀"), Some(487));
    assert_eq!(read_baked_glyphmap_entry(3839), [0, 0, GLYPH_MODE_BITMAP, 487]);

    assert_eq!(index.cell_for("🚀"), Some(958));
    assert_eq!(read_baked_glyphmap_entry(4759), [0, 0, GLYPH_MODE_BITMAP, 958]);

    let zwj_family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let baked_family = read_baked_glyphmap_entry(6819);
    assert_eq!(baked_family[2], GLYPH_MODE_BITMAP);
    assert_eq!(index.cell_for(zwj_family), Some(baked_family[3]));

    // Flags, skin tones, keycaps (with or without VS16), and VS16 heart.
    assert!(index.cell_for("\u{1F1FA}\u{1F1F8}").is_some(), "US flag");
    assert!(index.cell_for("\u{1F44D}\u{1F3FD}").is_some(), "thumbs up medium skin tone");
    let keycap_vs16 = index.cell_for("1\u{FE0F}\u{20E3}");
    let keycap_plain = index.cell_for("1\u{20E3}");
    assert!(keycap_vs16.is_some());
    assert_eq!(keycap_vs16, keycap_plain);
    assert!(index.cell_for("\u{2764}\u{FE0F}").is_some(), "VS16 red heart");

    // NotoColorEmoji carries a bitmap cell for ASCII '1' (one of the 117 text-default overlaps
    // documented in FORMAT.md) — proving the outline-first test in FontAtlas has teeth.
    assert!(index.cell_for("1").is_some());

    // No-cell cmap entries (space, ZWJ, tag space) and non-emoji clusters must return None.
    assert_eq!(index.cell_for(" "), None);
    assert_eq!(index.cell_for("\u{200D}"), None);
    assert_eq!(index.cell_for("\u{E0020}"), None);
    assert_eq!(index.cell_for("e\u{0301}"), None);
    assert_eq!(index.cell_for("x\u{0301}"), None);
}

#[test]
fn emoji_routing_precedence_and_closure_source() {
    let specs = default_font_chain(&assets_fonts_dir());
    let index = EmojiSheetIndex::load(&assets_atlas_dir().join("emoji-sheet.bin"))
        .expect("load emoji-sheet.bin");
    let mut atlas = FontAtlas::load(&specs)
        .expect("load default font chain")
        .with_emoji_source(index);
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).unwrap();
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).unwrap();
    let argon = atlas.family_id(MONASPACE_ARGON_FAMILY).unwrap();

    // 1. Astral emoji (>= U+1F000): colour bitmap wins, shared across families & faces.
    let rocket_c = atlas.glyph(cousine, Face::Regular, '🚀');
    let rocket_n = atlas.glyph(neon, Face::Bold, '🚀');
    let rocket_a = atlas.glyph(argon, Face::Italic, '🚀');
    assert_eq!(rocket_c, rocket_n);
    assert_eq!(rocket_c, rocket_a);
    assert!(atlas.is_bitmap_slot(rocket_c));
    assert_eq!(atlas.bitmap_cell(rocket_c), Some(958));
    assert_eq!(atlas.map_entry(rocket_c), [0, 0, GLYPH_MODE_BITMAP, 958]);
    assert_eq!(atlas.slot_kind(rocket_c), SlotKind::Bitmap { cell: 958 });
    assert!(atlas.slot_curves(rocket_c).is_empty());

    // 2. BMP text-default symbols covered by outline fonts stay outline (mode 0).
    for ch in ['0', '1', '#', '*', '©', '®', '❤', '✓', '★', '☃'] {
        let slot = atlas.glyph(cousine, Face::Regular, ch);
        assert_ne!(slot, BLANK_SLOT);
        assert!(
            !atlas.is_bitmap_slot(slot),
            "BMP text-default '{ch}' (U+{:04X}) must stay outline",
            ch as u32
        );
        assert_eq!(atlas.map_entry(slot)[2], GLYPH_MODE_OUTLINE);
    }

    // 3. Explicit VS16 vs VS15 on BMP symbol ❤ (U+2764):
    //    "❤️" (VS16) -> colour emoji bitmap; "❤︎" (VS15) -> monochrome outline.
    let heart_outline = atlas.glyph(cousine, Face::Regular, '❤');
    let heart_color = atlas.cluster(cousine, Face::Regular, "\u{2764}\u{FE0F}");
    let heart_text = atlas.cluster(cousine, Face::Regular, "\u{2764}\u{FE0E}");
    assert!(atlas.is_bitmap_slot(heart_color), "VS16 heart must draw colour emoji");
    assert_eq!(atlas.map_entry(heart_color)[2], GLYPH_MODE_BITMAP);
    assert_eq!(heart_text, heart_outline, "VS15 heart must draw monochrome outline");
    assert_eq!(atlas.map_entry(heart_text)[2], GLYPH_MODE_OUTLINE);

    // 4. BMP emoji uncovered by any outline font (e.g. '✅' U+2705 or '✨' U+2728 or '⌚' U+231A)
    //    routes to a colour-emoji bitmap slot.
    let uncovered_bmp = ['⌚', '⌛', '✅', '✨', '❌', '❓'].into_iter().find(|&ch| {
        let s = atlas.glyph(cousine, Face::Regular, ch);
        atlas.is_bitmap_slot(s)
    });
    assert!(
        uncovered_bmp.is_some(),
        "expected at least one uncovered BMP emoji to fall through to the bitmap sheet"
    );

    // 5. Emoji sequences: ZWJ family, flag, skin tone, keycap.
    let zwj_family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let fam_slot = atlas.cluster(neon, Face::Bold, zwj_family);
    let baked_family = read_baked_glyphmap_entry(6819);
    assert_eq!(atlas.map_entry(fam_slot), baked_family);
    assert_eq!(atlas.cluster(cousine, Face::Regular, zwj_family), fam_slot);

    let keycap = atlas.cluster(neon, Face::Regular, "1\u{FE0F}\u{20E3}");
    assert!(atlas.is_bitmap_slot(keycap));
    assert_eq!(atlas.cluster(neon, Face::Regular, "1\u{20E3}"), keycap);

    // Unknown ZWJ emoji sequence falls back to drawing its first emoji in colour.
    let unknown_zwj = atlas.cluster(neon, Face::Regular, "🚀\u{200D}🌟");
    assert_eq!(unknown_zwj, rocket_c);

    // 6. Combining-mark clusters still work with emoji source attached.
    let e_acute = atlas.cluster(neon, Face::Regular, "e\u{0301}");
    assert_eq!(e_acute, atlas.glyph(neon, Face::Regular, 'é'));
    let x_acute = atlas.cluster(neon, Face::Regular, "x\u{0301}");
    assert!(atlas.is_composite_slot(x_acute));
    assert!(!atlas.is_bitmap_slot(x_acute));

    // 7. Closure EmojiSource + dynamic set_emoji_source invalidates routing cache.
    let mut custom = FontAtlas::load(&specs).expect("load plain chain");
    let fam = custom.default_family();
    assert_eq!(custom.glyph(fam, Face::Regular, '🚀'), BLANK_SLOT);
    custom.set_emoji_source(Some(Arc::new(|text: &str| {
        if text == "🚀" {
            Some(777)
        } else {
            None
        }
    })));
    let s = custom.glyph(fam, Face::Regular, '🚀');
    assert_eq!(custom.map_entry(s), [0, 0, GLYPH_MODE_BITMAP, 777]);
}

#[test]
fn resolve_cell_handles_all_terminal_cell_classes() {
    let specs = default_font_chain(&assets_fonts_dir());
    let index = EmojiSheetIndex::load(&assets_atlas_dir().join("emoji-sheet.bin"))
        .expect("load emoji-sheet.bin");
    let mut atlas = FontAtlas::load(&specs)
        .expect("load default font chain")
        .with_emoji_source(index);
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).unwrap();

    let mut screen = Screen::new(12, 2);
    let make_cell = |content: u32, cell_attrs: u16, width: u8| Cell {
        content,
        fg: rgb(0xff, 0xff, 0xff),
        bg: rgb(0x10, 0x20, 0x30),
        ul: 0,
        attrs: cell_attrs,
        width,
        _pad: 0,
    };

    // Row 0: blank, space, regular/bold/italic/bold-italic 'A', bold sprite '╭', wide emoji + trail.
    screen.row_mut(0)[0] = Cell::BLANK;
    screen.row_mut(0)[1] = make_cell(' ' as u32, attrs::STRIKE, 1);
    screen.row_mut(0)[2] = make_cell('A' as u32, 0, 1);
    screen.row_mut(0)[3] = make_cell('A' as u32, attrs::BOLD, 1);
    screen.row_mut(0)[4] = make_cell('A' as u32, attrs::ITALIC, 1);
    screen.row_mut(0)[5] = make_cell('A' as u32, attrs::BOLD | attrs::ITALIC, 1);
    screen.row_mut(0)[6] = make_cell('╭' as u32, attrs::BOLD, 1);
    screen.row_mut(0)[7] = make_cell('🚀' as u32, 0, 2);
    screen.row_mut(0)[8] = make_cell('X' as u32, attrs::STRIKE, 0); // trailing wide cell

    assert_eq!(
        atlas.resolve_cell(neon, &screen, &screen.row(0)[0]),
        ResolvedCell { slot: BLANK_SLOT, span_cols: 1 }
    );
    assert_eq!(
        atlas.resolve_cell(neon, &screen, &screen.row(0)[1]),
        ResolvedCell { slot: BLANK_SLOT, span_cols: 1 }
    );

    let r_reg = atlas.resolve_cell(neon, &screen, &screen.row(0)[2]);
    let r_bold = atlas.resolve_cell(neon, &screen, &screen.row(0)[3]);
    let r_ital = atlas.resolve_cell(neon, &screen, &screen.row(0)[4]);
    let r_bi = atlas.resolve_cell(neon, &screen, &screen.row(0)[5]);
    assert_eq!((r_reg.span_cols, r_bold.span_cols, r_ital.span_cols, r_bi.span_cols), (1, 1, 1, 1));
    assert_eq!(
        std::collections::HashSet::from([r_reg.slot, r_bold.slot, r_ital.slot, r_bi.slot]).len(),
        4,
        "cell attrs must route to four distinct faces"
    );

    let r_sprite = atlas.resolve_cell(neon, &screen, &screen.row(0)[6]);
    assert_eq!(r_sprite.span_cols, 1);
    assert!(atlas.is_sprite_slot(r_sprite.slot));
    assert_eq!(atlas.map_entry(r_sprite.slot)[2], GLYPH_MODE_SPRITE);

    let r_rocket = atlas.resolve_cell(neon, &screen, &screen.row(0)[7]);
    assert_eq!(r_rocket.span_cols, 2);
    assert_eq!(atlas.map_entry(r_rocket.slot), [0, 0, GLYPH_MODE_BITMAP, 958]);

    // Trailing half of wide cell always yields (BLANK_SLOT, 0).
    assert_eq!(
        atlas.resolve_cell(neon, &screen, &screen.row(0)[8]),
        ResolvedCell { slot: BLANK_SLOT, span_cols: 0 }
    );

    // Row 1: cluster entries (ZWJ family, bold composite x́, italic NFC é, VS16 ❤️, VS15 ❤︎, OOB).
    screen.clusters.push("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_owned());
    screen.clusters.push("x\u{0301}".to_owned());
    screen.clusters.push("e\u{0301}".to_owned());
    screen.clusters.push("\u{2764}\u{FE0F}".to_owned());
    screen.clusters.push("\u{2764}\u{FE0E}".to_owned());

    screen.row_mut(1)[0] = make_cell(CLUSTER_BASE, 0, 2);
    screen.row_mut(1)[1] = make_cell(0, 0, 0);
    screen.row_mut(1)[2] = make_cell(CLUSTER_BASE + 1, attrs::BOLD, 1);
    screen.row_mut(1)[3] = make_cell(CLUSTER_BASE + 2, attrs::ITALIC, 1);
    screen.row_mut(1)[4] = make_cell(CLUSTER_BASE + 3, 0, 2);
    screen.row_mut(1)[5] = make_cell(CLUSTER_BASE + 4, 0, 1);
    screen.row_mut(1)[6] = make_cell(CLUSTER_BASE + 99, 0, 1); // out-of-bounds cluster index

    let c_fam = atlas.resolve_cell(neon, &screen, &screen.row(1)[0]);
    assert_eq!(c_fam.span_cols, 2);
    assert_eq!(atlas.map_entry(c_fam.slot), read_baked_glyphmap_entry(6819));

    let c_x = atlas.resolve_cell(neon, &screen, &screen.row(1)[2]);
    assert_eq!(c_x.span_cols, 1);
    assert!(atlas.is_composite_slot(c_x.slot));
    assert_eq!(c_x.slot, atlas.cluster(neon, Face::Bold, "x\u{0301}"));

    let c_e = atlas.resolve_cell(neon, &screen, &screen.row(1)[3]);
    assert_eq!(c_e.span_cols, 1);
    assert_eq!(c_e.slot, atlas.glyph(neon, Face::Italic, 'é'));

    let c_heart_color = atlas.resolve_cell(neon, &screen, &screen.row(1)[4]);
    assert_eq!(c_heart_color.span_cols, 2);
    assert!(atlas.is_bitmap_slot(c_heart_color.slot));

    let c_heart_text = atlas.resolve_cell(neon, &screen, &screen.row(1)[5]);
    assert_eq!(c_heart_text.span_cols, 1);
    assert!(!atlas.is_bitmap_slot(c_heart_text.slot));
    assert_eq!(c_heart_text.slot, atlas.glyph(neon, Face::Regular, '❤'));

    assert_eq!(
        atlas.resolve_cell(neon, &screen, &screen.row(1)[6]),
        ResolvedCell { slot: BLANK_SLOT, span_cols: 1 }
    );
}
