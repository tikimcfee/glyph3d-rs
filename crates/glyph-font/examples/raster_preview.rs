//! Render a sample strip of glyphs from Cousine and Monaspace Neon to a PGM image
//! using the CPU Slug rasteriser (`glyph_font::raster`).
//!
//! Run with:
//! `cargo run -p glyph-font --example raster_preview -- /tmp/glyph-preview.pgm`

use std::path::Path;

use glyph_font::raster::{rasterize_glyph, write_pgm};
use glyph_font::{default_font_chain, Face, FontAtlas, DEFAULT_FONT_FAMILY, MONASPACE_NEON_FAMILY};

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

    let sample = "AgéH@09─│";
    let rows = [
        (cousine, Face::Regular),
        (cousine, Face::Bold),
        (cousine, Face::Italic),
        (neon, Face::Regular),
        (neon, Face::Bold),
        (neon, Face::Italic),
        (neon, Face::BoldItalic),
    ];

    let cell_w = 48usize;
    let cell_h = 80usize;
    let cols = sample.chars().count();
    let img_w = cols * cell_w;
    let img_h = rows.len() * cell_h;
    let mut img = vec![0u8; img_w * img_h];

    for (r, &(fam, face)) in rows.iter().enumerate() {
        for (c, ch) in sample.chars().enumerate() {
            let slot = atlas.glyph(fam, face, ch);
            let curves = atlas.slot_curves(slot);
            let tile = rasterize_glyph(&curves, cell_w, cell_h, true);
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
