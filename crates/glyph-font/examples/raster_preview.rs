//! Render a sample grid of font faces, clusters, and generated cell sprites from Cousine,
//! Monaspace Neon, and Monaspace Argon to a PGM image using the CPU Slug rasteriser.
//!
//! Run with:
//! `cargo run -p glyph-font --example raster_preview -- /tmp/glyph-preview.pgm`

use std::path::Path;

use glyph_font::constants::GLYPH_MODE_SPRITE;
use glyph_font::raster::{rasterize_glyph, write_pgm};
use glyph_font::{
    default_font_chain, Face, FamilyId, FontAtlas, DEFAULT_FONT_FAMILY, MONASPACE_ARGON_FAMILY,
    MONASPACE_NEON_FAMILY,
};

enum CellSpec {
    Char(char),
    Cluster(&'static str),
}

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/glyph-font-preview.pgm".to_owned());
    let fonts_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .join("assets/fonts");

    let specs = default_font_chain(&fonts_dir);
    let mut atlas = FontAtlas::load(&specs).expect("load default fonts");
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).expect("cousine");
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).expect("monaspace-neon");
    let argon = atlas.family_id(MONASPACE_ARGON_FAMILY).expect("monaspace-argon");

    let text_cells = || {
        vec![
            CellSpec::Char('A'),
            CellSpec::Char('g'),
            CellSpec::Char('H'),
            CellSpec::Char('@'),
            CellSpec::Char('0'),
            CellSpec::Cluster("e\u{0301}"),
            CellSpec::Cluster("x\u{0301}"),
            CellSpec::Cluster("q\u{0308}\u{0304}"),
            CellSpec::Char('╭'),
            CellSpec::Char('─'),
            CellSpec::Char('╮'),
            CellSpec::Char('╬'),
        ]
    };

    // Cell sprites: box drawing (light/heavy/double/rounded/diag), blocks & shades,
    // braille, legacy sextants/wedges, and powerline separators.
    let sprite_row_1 = vec![
        CellSpec::Char('╭'),
        CellSpec::Char('─'),
        CellSpec::Char('┬'),
        CellSpec::Char('─'),
        CellSpec::Char('╮'),
        CellSpec::Char('╔'),
        CellSpec::Char('═'),
        CellSpec::Char('╬'),
        CellSpec::Char('═'),
        CellSpec::Char('╗'),
        CellSpec::Char('╱'),
        CellSpec::Char('╳'),
    ];
    let sprite_row_2 = vec![
        CellSpec::Char('█'),
        CellSpec::Char('▓'),
        CellSpec::Char('▒'),
        CellSpec::Char('░'),
        CellSpec::Char('▀'),
        CellSpec::Char('▄'),
        CellSpec::Char('▚'),
        CellSpec::Char('⣿'),
        CellSpec::Char('🬼'),
        CellSpec::Char('\u{E0B0}'),
        CellSpec::Char('\u{E0B4}'),
        CellSpec::Char('\u{E0B5}'),
    ];

    let rows: Vec<(FamilyId, Face, Vec<CellSpec>)> = vec![
        (cousine, Face::Regular, text_cells()),
        (cousine, Face::Bold, text_cells()),
        (neon, Face::Regular, text_cells()),
        (neon, Face::Bold, text_cells()),
        (neon, Face::Italic, text_cells()),
        (argon, Face::Regular, text_cells()),
        (argon, Face::BoldItalic, text_cells()),
        (neon, Face::Regular, sprite_row_1),
        (neon, Face::Regular, sprite_row_2),
    ];

    let cell_w = 48usize;
    let cell_h = 80usize;
    let cols = rows[0].2.len();
    let img_w = cols * cell_w;
    let img_h = rows.len() * cell_h;
    let mut img = vec![0u8; img_w * img_h];

    for (r, (fam, face, cells)) in rows.iter().enumerate() {
        for (c, cell) in cells.iter().enumerate() {
            let slot = match *cell {
                CellSpec::Char(ch) => atlas.glyph(*fam, *face, ch),
                CellSpec::Cluster(s) => atlas.cluster(*fam, *face, s),
            };
            let entry = atlas.map_entry(slot);
            let curves = atlas.slot_curves(slot);
            let mut tile = rasterize_glyph(&curves, cell_w, cell_h, true);
            // Apply sprite ink alpha (e.g. shades ░ ▒ ▓) just as the shader does for mode 3.
            if entry[2] == GLYPH_MODE_SPRITE && entry[3] < 255 {
                let alpha = entry[3] as u32;
                for px in &mut tile {
                    *px = ((*px as u32 * alpha + 127) / 255) as u8;
                }
            }
            for y in 0..cell_h {
                let dst = (r * cell_h + y) * img_w + c * cell_w;
                let src = y * cell_w;
                img[dst..dst + cell_w].copy_from_slice(&tile[src..src + cell_w]);
            }
        }
    }

    write_pgm(Path::new(&out_path), img_w, img_h, &img).expect("write PGM");
    println!("wrote {img_w}x{img_h} preview to {out_path}");
}
