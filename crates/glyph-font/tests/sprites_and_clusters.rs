//! Tests for cell sprites (`sprites.rs`) and combining-mark clusters (`FontAtlas::cluster`),
//! ported from `tools/cell-sprites.test.mjs` and `tools/font-faces.test.mjs` (§6-7).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glyph_font::constants::{unpack_u16, BLANK_SLOT, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE};
use glyph_font::encode::{Curve, PackedCurve};
use glyph_font::metrics::{compute_cell_metrics, snap_cell_dimension, CharSize};
use glyph_font::sprites::{
    encode_sprite, is_sprite_codepoint, sprite_outline, SpriteMetrics, SPRITE_VERSION,
};
use glyph_font::{
    default_font_chain, Face, FontAtlas, FontSource, FontSpec, SlotKind, DEFAULT_FONT_FAMILY,
    MONASPACE_ARGON_FAMILY, MONASPACE_NEON_FAMILY,
};

fn assets_fonts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .join("assets/fonts")
}

/// Ray-to-+x signed winding number over quadratic curves, matching `winding()` in
/// `tools/cell-sprites.test.mjs` and `tools/font-faces.test.mjs`.
/// A clockwise contour (y up) gives `+1` inside.
fn winding_f64(curves: &[Curve], x: f64, y: f64) -> i32 {
    let mut w = 0;
    for c in curves {
        let mut px = c.p0x;
        let mut py = c.p0y;
        for s in 1..=12 {
            let t = s as f64 / 12.0;
            let u = 1.0 - t;
            let qx = u * u * c.p0x + 2.0 * u * t * c.p1x + t * t * c.p2x;
            let qy = u * u * c.p0y + 2.0 * u * t * c.p1y + t * t * c.p2y;
            if (py <= y) != (qy <= y) {
                let at = px + (y - py) / (qy - py) * (qx - px);
                if at > x {
                    w += if qy > py { -1 } else { 1 };
                }
            }
            px = qx;
            py = qy;
        }
    }
    w
}

fn packed_to_curves(packed: &[PackedCurve]) -> Vec<Curve> {
    packed
        .iter()
        .map(|c| Curve {
            p0x: unpack_u16(c.p0x) as f64,
            p0y: unpack_u16(c.p0y) as f64,
            p1x: unpack_u16(c.p1x) as f64,
            p1y: unpack_u16(c.p1y) as f64,
            p2x: unpack_u16(c.p2x) as f64,
            p2y: unpack_u16(c.p2y) as f64,
        })
        .collect()
}

fn inked(cp: u32, m: SpriteMetrics, x: f64, y: f64) -> bool {
    let o = sprite_outline(cp, m).expect("sprite outline");
    winding_f64(&o.curves, x, y) > 0
}

fn area(cp: u32, m: SpriteMetrics, n: usize) -> f64 {
    let o = sprite_outline(cp, m).expect("sprite outline");
    let mut hit = 0usize;
    for i in 0..n {
        for j in 0..n {
            if winding_f64(&o.curves, (i as f64 + 0.5) / n as f64, (j as f64 + 0.5) / n as f64) > 0
            {
                hit += 1;
            }
        }
    }
    hit as f64 / (n * n) as f64
}

// ── 1. Every sprite codepoint: unit cell, CW winding, curve count ────────────

#[test]
fn sprites_all_579_codepoints_valid_clockwise_and_in_cell() {
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).expect("load default chain");
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).unwrap();
    let m = atlas.sprite_metrics(cousine);

    // Check font's own convention: stem of Cousine 'l' winds +1.
    let l_slot = atlas.glyph(cousine, Face::Regular, 'l');
    let l_curves = packed_to_curves(&atlas.slot_curves(l_slot));
    let mut font_inside = 0;
    for k in 0..10 {
        let y = 0.3 + k as f64 * 0.05;
        font_inside = font_inside.max(winding_f64(&l_curves, 0.5, y));
    }
    assert_eq!(font_inside, 1, "font outline interior winds +1");
    assert_eq!(
        winding_f64(&sprite_outline(0x2588, m).unwrap().curves, 0.5, 0.5),
        1,
        "full block █ winds +1 like font outlines"
    );

    let mut count = 0usize;
    let mut out_of_range = 0usize;
    let mut empty = Vec::new();
    let mut negative = Vec::new();
    let mut max_curves = 0usize;

    for cp in 0x2500u32..=0x1FBFFu32 {
        if !is_sprite_codepoint(cp) {
            continue;
        }
        count += 1;
        let o = sprite_outline(cp, m).expect("sprite outline");
        if o.alpha == 0 {
            out_of_range += 1;
        }
        for c in &o.curves {
            for v in [c.p0x, c.p0y, c.p1x, c.p1y, c.p2x, c.p2y] {
                if !(0.0..=1.0).contains(&v) {
                    out_of_range += 1;
                }
            }
        }
        if o.curves.is_empty() && cp != 0x2800 {
            empty.push(cp);
        }
        max_curves = max_curves.max(o.curves.len());
        'grid: for i in 0..9 {
            for j in 0..9 {
                if winding_f64(&o.curves, (i as f64 + 0.5) / 9.0, (j as f64 + 0.5) / 9.0) < 0 {
                    negative.push(cp);
                    break 'grid;
                }
            }
        }
    }

    assert_eq!(count, 128 + 32 + 256 + 16 + 145 + 2);
    assert_eq!(out_of_range, 0, "all sprite coords must lie in [0, 1] with alpha > 0");
    assert!(empty.is_empty(), "unexpected empty sprites: {empty:X?}");
    assert!(negative.is_empty(), "sprites with backwards winding: {negative:X?}");
    assert!(max_curves <= 256, "busiest sprite has {max_curves} curves (limit 256)");
    assert!(!is_sprite_codepoint('A' as u32));
    assert!(!is_sprite_codepoint(0x25A0));
    assert!(!is_sprite_codepoint(0xE0A0));
    assert!(!is_sprite_codepoint(0x1F600));
    assert!(!is_sprite_codepoint(0x1FB95));
    assert!(sprite_outline('A' as u32, m).is_none());
    const { assert!(SPRITE_VERSION >= 1) };
}

// ── 2. Block elements ────────────────────────────────────────────────────────

#[test]
fn sprites_block_elements_areas_shades_and_quadrants() {
    let specs = default_font_chain(&assets_fonts_dir());
    let atlas = FontAtlas::load(&specs).unwrap();
    let m = atlas.sprite_metrics(atlas.default_family());

    let near = |a: f64, b: f64, tol: f64, msg: &str| {
        assert!((a - b).abs() <= tol, "{msg}: got {a:.4}, want {b:.4}");
    };
    near(area(0x2588, m, 60), 1.0, 0.0, "█ fills the cell");
    near(area(0x2580, m, 60), 0.5, 0.02, "▀ is half");
    assert!(inked(0x2580, m, 0.5, 0.9) && !inked(0x2580, m, 0.5, 0.1));
    assert!(inked(0x2584, m, 0.5, 0.1) && !inked(0x2584, m, 0.5, 0.9));
    for k in 1u32..=8 {
        near(area(0x2580 + k, m, 60), k as f64 / 8.0, 0.02, "lower k/8");
    }
    for k in 1u32..=7 {
        near(area(0x2590 - k, m, 60), k as f64 / 8.0, 0.02, "left k/8");
    }
    assert!(inked(0x258C, m, 0.1, 0.5) && !inked(0x258C, m, 0.9, 0.5));
    assert!(inked(0x2590, m, 0.9, 0.5) && !inked(0x2590, m, 0.1, 0.5));
    assert!(inked(0x2594, m, 0.5, 0.97) && !inked(0x2594, m, 0.5, 0.8));
    assert!(inked(0x2595, m, 0.97, 0.5) && !inked(0x2595, m, 0.8, 0.5));

    let shade_alphas: Vec<u8> = [0x2591, 0x2592, 0x2593]
        .iter()
        .map(|&cp| sprite_outline(cp, m).unwrap().alpha)
        .collect();
    assert_eq!(shade_alphas, vec![64, 128, 191]);
    near(area(0x2592, m, 60), 1.0, 0.0, "▒ covers whole cell");

    let quads: [(u32, &[&str]); 10] = [
        (0x2596, &["ll"]),
        (0x2597, &["lr"]),
        (0x2598, &["ul"]),
        (0x2599, &["ll", "lr", "ul"]),
        (0x259A, &["lr", "ul"]),
        (0x259B, &["ll", "ul", "ur"]),
        (0x259C, &["lr", "ul", "ur"]),
        (0x259D, &["ur"]),
        (0x259E, &["ll", "ur"]),
        (0x259F, &["ll", "lr", "ur"]),
    ];
    let pts = [("ll", 0.25, 0.25), ("lr", 0.75, 0.25), ("ul", 0.25, 0.75), ("ur", 0.75, 0.75)];
    for (cp, want) in quads {
        let got: Vec<&str> = pts
            .iter()
            .filter(|&&(_, x, y)| inked(cp, m, x, y))
            .map(|&(name, _, _)| name)
            .collect();
        assert_eq!(got, want, "quadrant U+{cp:04X}");
    }
}

// ── 3. Box drawing ───────────────────────────────────────────────────────────

#[test]
fn sprites_box_drawing_arms_weights_double_dashes_arcs_diagonals() {
    let specs = default_font_chain(&assets_fonts_dir());
    let atlas = FontAtlas::load(&specs).unwrap();
    let m = atlas.sprite_metrics(atlas.default_family());

    let edges = [
        ("up", 0.5, 0.995),
        ("right", 0.995, 0.5),
        ("down", 0.5, 0.005),
        ("left", 0.005, 0.5),
    ];
    let arms = |cp: u32| -> String {
        edges
            .iter()
            .filter(|&&(_, x, y)| inked(cp, m, x, y))
            .map(|&(name, _, _)| name)
            .collect::<Vec<_>>()
            .join(" ")
    };

    let expected = [
        (0x2500, "right left"),
        (0x2502, "up down"),
        (0x250C, "right down"),
        (0x2510, "down left"),
        (0x2514, "up right"),
        (0x2518, "up left"),
        (0x251C, "up right down"),
        (0x2524, "up down left"),
        (0x252C, "right down left"),
        (0x2534, "up right left"),
        (0x253C, "up right down left"),
        (0x2501, "right left"),
        (0x2503, "up down"),
        (0x254B, "up right down left"),
        (0x250F, "right down"),
        (0x2521, "up right down"),
        (0x256D, "right down"),
        (0x256E, "down left"),
        (0x256F, "up left"),
        (0x2570, "up right"),
        (0x2574, "left"),
        (0x2575, "up"),
        (0x2576, "right"),
        (0x2577, "down"),
        (0x257C, "right left"),
        (0x257F, "up down"),
    ];
    for (cp, want) in expected {
        assert_eq!(arms(cp), want, "box arms U+{cp:04X}");
    }

    assert!(inked(0x2500, m, 0.5, 0.5) && !inked(0x2500, m, 0.5, 0.5 + m.light) && !inked(0x2500, m, 0.5, 0.5 - m.light));
    assert!(inked(0x2501, m, 0.5, 0.5 + m.light * 0.8) && !inked(0x2501, m, 0.5, 0.5 + m.heavy));
    assert!((area(0x2500, m, 200) - m.light).abs() < 0.004);
    assert!((area(0x2502, m, 200) - m.light / m.aspect).abs() < 0.006);
    assert!(inked(0x250C, m, 0.5, 0.5) && !inked(0x250C, m, 0.5 - m.light, 0.5 + m.light));
    assert!(!inked(0x256D, m, 0.5, 0.5) && inked(0x256D, m, 0.5, 0.2));
    assert!(
        inked(0x2521, m, 0.5 + m.heavy / m.aspect * 0.4, 0.9)
            && !inked(0x2521, m, 0.5 + m.heavy / m.aspect * 0.4, 0.1)
    );

    // Double lines.
    let t = m.light;
    let tx = m.light / m.aspect;
    assert!(inked(0x2550, m, 0.1, 0.5 + t) && inked(0x2550, m, 0.1, 0.5 - t) && !inked(0x2550, m, 0.1, 0.5));
    assert!(inked(0x2551, m, 0.5 + tx, 0.1) && inked(0x2551, m, 0.5 - tx, 0.1) && !inked(0x2551, m, 0.5, 0.1));
    assert!(
        !inked(0x256C, m, 0.5, 0.5)
            && !inked(0x256C, m, 0.1, 0.5)
            && !inked(0x256C, m, 0.5, 0.1)
            && inked(0x256C, m, 0.5 - tx, 0.5 - t)
            && inked(0x256C, m, 0.5 + tx, 0.5 + t)
    );
    assert!(
        inked(0x2554, m, 0.5 - tx, 0.5 + t)
            && inked(0x2554, m, 0.5 + tx, 0.5 - t)
            && !inked(0x2554, m, 0.5, 0.5)
            && !inked(0x2554, m, 0.5 - tx, 0.99)
    );
    assert!(inked(0x2560, m, 0.5 - tx, 0.5) && !inked(0x2560, m, 0.5 + tx, 0.5) && inked(0x2560, m, 0.99, 0.5 + t));
    assert!(
        inked(0x2552, m, 0.5, 0.1)
            && inked(0x2552, m, 0.9, 0.5 + t)
            && inked(0x2552, m, 0.9, 0.5 - t)
            && !inked(0x2552, m, 0.9, 0.5)
            && !inked(0x2552, m, 0.1, 0.5 + t)
    );
    assert!(inked(0x256A, m, 0.5, 0.5) && inked(0x256A, m, 0.1, 0.5 + t) && !inked(0x256A, m, 0.1, 0.5));

    // Dashes and diagonals.
    for (cp, n_dash) in [(0x2504, 3), (0x2508, 4), (0x254C, 2)] {
        let mut runs = 0;
        let mut was = false;
        for i in 0..400 {
            let on = inked(cp, m, (i as f64 + 0.5) / 400.0, 0.5);
            if on && !was {
                runs += 1;
            }
            was = on;
        }
        assert_eq!(runs, n_dash, "dash count U+{cp:04X}");
        assert!(!inked(cp, m, 0.001, 0.5) && !inked(cp, m, 0.999, 0.5));
    }
    assert!(inked(0x2506, m, 0.5, 0.5) && !inked(0x2506, m, 0.2, 0.5));
    assert!(inked(0x2571, m, 0.02, 0.02) && inked(0x2571, m, 0.98, 0.98) && inked(0x2571, m, 0.5, 0.5) && !inked(0x2571, m, 0.1, 0.9));
    assert!(inked(0x2572, m, 0.02, 0.98) && inked(0x2572, m, 0.98, 0.02) && !inked(0x2572, m, 0.9, 0.9));
    assert!(inked(0x2573, m, 0.02, 0.02) && inked(0x2573, m, 0.02, 0.98) && inked(0x2573, m, 0.98, 0.02) && inked(0x2573, m, 0.98, 0.98));
}

// ── 4. Braille, legacy computing, powerline ──────────────────────────────────

#[test]
fn sprites_braille_legacy_computing_and_powerline() {
    let specs = default_font_chain(&assets_fonts_dir());
    let atlas = FontAtlas::load(&specs).unwrap();
    let m = atlas.sprite_metrics(atlas.default_family());

    // All 256 braille patterns.
    let dots = [
        (0.25, 7.0 / 8.0),
        (0.25, 5.0 / 8.0),
        (0.25, 3.0 / 8.0),
        (0.75, 7.0 / 8.0),
        (0.75, 5.0 / 8.0),
        (0.75, 3.0 / 8.0),
        (0.25, 1.0 / 8.0),
        (0.75, 1.0 / 8.0),
    ];
    for bits in 0u32..256 {
        for (d, &(x, y)) in dots.iter().enumerate() {
            assert_eq!(
                inked(0x2800 + bits, m, x, y),
                (bits & (1 << d)) != 0,
                "braille U+{:04X} dot {}",
                0x2800 + bits,
                d + 1
            );
        }
    }
    assert!(!inked(0x28FF, m, 0.5, 0.5) && !inked(0x28FF, m, 0.25, 0.5));

    // All 60 sextants.
    let mut bits = 1u32;
    for i in 0u32..60 {
        if bits == 21 || bits == 42 {
            bits += 1;
        }
        for b in 0..6 {
            let x = if (b & 1) != 0 { 0.75 } else { 0.25 };
            let y = 1.0 - ((b >> 1) as f64 + 0.5) / 3.0;
            assert_eq!(inked(0x1FB00 + i, m, x, y), (bits & (1 << b)) != 0);
        }
        bits += 1;
    }
    assert!(inked(0x1FB00, m, 0.01, 0.99) && inked(0x1FB3B, m, 0.99, 0.01));

    // Wedges and complements sum to 1.0.
    for i in 0u32..22 {
        let sum = area(0x1FB3C + i, m, 40) + area(0x1FB52 + i, m, 40);
        assert!((sum - 1.0).abs() <= 0.03, "wedge {i} + complement area = {sum}");
    }
    assert!(inked(0x1FB3C, m, 0.05, 0.05) && !inked(0x1FB3C, m, 0.9, 0.9));
    assert!((area(0x1FB3C, m, 60) - 1.0 / 12.0).abs() < 0.02);
    assert!(inked(0x1FB6C, m, 0.1, 0.5) && !inked(0x1FB6C, m, 0.9, 0.5));
    assert!((area(0x1FB6C, m, 60) - 0.25).abs() < 0.02);
    assert!((area(0x1FB68, m, 60) - 0.75).abs() < 0.02 && !inked(0x1FB68, m, 0.1, 0.5));
    assert!(inked(0x1FB70, m, 1.5 / 8.0, 0.5) && !inked(0x1FB70, m, 0.5 / 8.0, 0.5));
    assert!(inked(0x1FB76, m, 0.5, 1.0 - 1.5 / 8.0) && !inked(0x1FB76, m, 0.5, 1.0 - 0.5 / 8.0));
    assert_eq!(sprite_outline(0x1FB90, m).unwrap().alpha, 128);

    // Powerline.
    assert!(
        inked(0xE0B0, m, 0.01, 0.5)
            && inked(0xE0B0, m, 0.01, 0.99)
            && inked(0xE0B0, m, 0.01, 0.01)
            && !inked(0xE0B0, m, 0.9, 0.9)
            && (area(0xE0B0, m, 60) - 0.5).abs() < 0.02
    );
    assert!(inked(0xE0B2, m, 0.99, 0.99) && !inked(0xE0B2, m, 0.1, 0.9));
    assert!(
        (area(0xE0B4, m, 60) - std::f64::consts::FRAC_PI_4).abs() < 0.02
            && inked(0xE0B4, m, 0.01, 0.5)
            && !inked(0xE0B4, m, 0.9, 0.95)
    );
    assert!(
        (area(0xE0B8, m, 60) - 0.5).abs() < 0.02
            && inked(0xE0B8, m, 0.1, 0.1)
            && !inked(0xE0B8, m, 0.9, 0.9)
    );
    assert!(area(0xE0B1, m, 60) < 0.25 && inked(0xE0B1, m, 0.98, 0.5) && !inked(0xE0B1, m, 0.2, 0.5));
    assert!(area(0xE0B5, m, 60) < 0.3 && !inked(0xE0B5, m, 0.3, 0.5));
}

// ── 5. Atlas sprite routing, per-family sets, and cell lattice ───────────────

#[test]
fn sprites_route_before_fonts_per_family_in_mode_3() {
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).unwrap();
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).unwrap();
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).unwrap();

    // Direct encode_sprite checks.
    let m = atlas.sprite_metrics(cousine);
    let (full_c, full_a) = encode_sprite(0x2588, m).unwrap();
    let (shade_c, shade_a) = encode_sprite(0x2592, m).unwrap();
    let (blank_b, blank_a) = encode_sprite(0x2800, m).unwrap();
    assert_eq!((full_c.len(), full_a), (4, 255));
    assert_eq!((shade_c.len(), shade_a), (4, 128));
    assert_eq!((blank_b.len(), blank_a), (0, 255));

    // Sprites route before any font, carry mode 3 + ink alpha, have no faces, and are per-family.
    let box_c = atlas.glyph(cousine, Face::Regular, '─');
    let box_c_bold = atlas.glyph(cousine, Face::Bold, '─');
    let box_n = atlas.glyph(neon, Face::Regular, '─');
    assert_eq!(box_c, box_c_bold, "sprites have no bold/italic faces");
    assert_ne!(box_c, box_n, "each family owns its own sprite slots");
    assert!(atlas.is_sprite_slot(box_c));
    assert!(atlas.is_sprite_slot(box_n));
    assert_eq!(atlas.sprite_info(box_c), Some((cousine, 0x2500)));
    assert_eq!(atlas.sprite_info(box_n), Some((neon, 0x2500)));
    assert_eq!(
        atlas.slot_kind(box_c),
        SlotKind::Sprite { family: cousine, codepoint: 0x2500 }
    );
    assert_eq!(atlas.map_entry(box_c)[1], 8, "─ has two 4-curve arms");
    assert_eq!(atlas.map_entry(box_c)[2], GLYPH_MODE_SPRITE);
    assert_eq!(atlas.map_entry(box_c)[3], 255);

    let shade = atlas.glyph(cousine, Face::Regular, '▒');
    assert_eq!(atlas.map_entry(shade)[1], 4);
    assert_eq!(atlas.map_entry(shade)[2], GLYPH_MODE_SPRITE);
    assert_eq!(atlas.map_entry(shade)[3], 128);

    // Family aspects differ, so Cousine and Neon sprite metrics differ.
    assert!((atlas.sprite_metrics(cousine).aspect - atlas.sprite_metrics(neon).aspect).abs() > 1e-3);
    assert!(m.light > 0.0 && m.heavy >= 2.0 * m.light - 1e-12);

    // Non-sprite powerline branch glyph U+E0A0 still routes to Meslo font (mode 0).
    let branch = atlas.glyph_u32(cousine, Face::Regular, 0xE0A0);
    assert_ne!(branch, BLANK_SLOT);
    assert!(!atlas.is_sprite_slot(branch));
    assert_eq!(atlas.map_entry(branch)[2], GLYPH_MODE_OUTLINE);
    assert_eq!(atlas.describe_slot(branch).unwrap().font_name, "MesloLGS NF Mono");

    // Cell lattice: snapped cell edges are exact in f32 across 16,000 columns and rows.
    let cm = compute_cell_metrics(CharSize { width: 29.0, height: 54.375 }, 0.025);
    assert!((cm.char_width / (29.0 * 0.025) - 1.0).abs() < 1e-3);
    assert!((cm.char_height / (54.375 * 0.025) - 1.0).abs() < 1e-3);
    assert_eq!(snap_cell_dimension(cm.char_width), cm.char_width);
    for k in 0u32..16_000 {
        let kf = k as f32;
        let k1f = (k + 1) as f32;
        assert_eq!((kf * cm.char_width + cm.char_width).to_bits(), (k1f * cm.char_width).to_bits());
        let row_bottom = (-kf * cm.line_spacing) - (cm.char_height * 0.5);
        let next_top = (-k1f * cm.line_spacing) + (cm.char_height * 0.5);
        assert_eq!(row_bottom.to_bits(), next_top.to_bits());
    }
    // Teeth: unsnapped width does NOT hold that invariant.
    let unsnapped = 29.0_f32 * 0.025_f32;
    let cracks = (0u32..2000)
        .filter(|&k| ((k as f32) * unsnapped + unsnapped).to_bits() != (((k + 1) as f32) * unsnapped).to_bits())
        .count();
    assert!(cracks > 0, "unsnapped width must have float32 edge cracks");
}

// ── 6. Clusters: NFC, composites, mark placement, stacking, selectors ────────

struct RowBand {
    y: f64,
    count: usize,
    lo: f64,
    hi: f64,
}

struct GridInk {
    area: f64,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
    rows: Vec<RowBand>,
}

fn grid_ink(atlas: &FontAtlas, slot: u32, n: usize) -> GridInk {
    let curves = packed_to_curves(&atlas.slot_curves(slot));
    let mut hit = 0usize;
    let (mut x0, mut x1, mut y0, mut y1) = (1.0_f64, 0.0_f64, 1.0_f64, 0.0_f64);
    let mut rows = Vec::with_capacity(n);
    for j in 0..n {
        let y = (j as f64 + 0.5) / n as f64;
        let (mut lo, mut hi, mut c) = (1.0_f64, 0.0_f64, 0usize);
        for i in 0..n {
            let x = (i as f64 + 0.5) / n as f64;
            if winding_f64(&curves, x, y) == 0 {
                continue;
            }
            hit += 1;
            c += 1;
            lo = lo.min(x);
            hi = hi.max(x);
        }
        rows.push(RowBand { y, count: c, lo, hi });
        if c > 0 {
            x0 = x0.min(lo);
            x1 = x1.max(hi);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    GridInk {
        area: hit as f64 / (n * n) as f64,
        x0,
        x1,
        y0,
        y1,
        rows,
    }
}

#[test]
fn clusters_nfc_composites_mark_placement_and_stacking() {
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).expect("load default chain");

    let families = [
        (DEFAULT_FONT_FAMILY, "Cousine"),
        (MONASPACE_NEON_FAMILY, "Monaspace Neon"),
        (MONASPACE_ARGON_FAMILY, "Monaspace Argon"),
    ];
    let e_acute = "e\u{0301}";
    let viet_stack = "e\u{0302}\u{0301}";
    let x_acute = "x\u{0301}";
    let q_stack = "q\u{0308}\u{0304}";

    for (fam_name, label) in families {
        let fam = atlas.family_id(fam_name).unwrap();

        // NFC precomposed: e + U+0301 -> é in Regular and Bold.
        let pre = atlas.cluster(fam, Face::Regular, e_acute);
        assert_eq!(pre, atlas.glyph(fam, Face::Regular, 'é'), "{fam_name}: NFC é");
        assert!(!atlas.is_composite_slot(pre));
        let pre_bold = atlas.cluster(fam, Face::Bold, e_acute);
        assert_eq!(pre_bold, atlas.glyph(fam, Face::Bold, 'é'), "{fam_name}: NFC bold é");

        // Vietnamese stack that has a precomposed glyph (ế U+1EBF).
        let viet = atlas.cluster(fam, Face::Regular, viet_stack);
        assert_eq!(viet, atlas.glyph(fam, Face::Regular, '\u{1EBF}'));
        assert!(!atlas.is_composite_slot(viet));

        // No precomposed form: x + U+0301 -> 2-part composite.
        let comp = atlas.cluster(fam, Face::Regular, x_acute);
        assert!(atlas.is_composite_slot(comp), "{fam_name}: x́ must be composite");
        assert!(!atlas.is_sprite_slot(comp));
        assert!(atlas.describe_slot(comp).is_none());
        let parts = atlas.composite_parts(comp).expect("composite parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(atlas.font_name(parts[0].font_index), Some(label));
        assert_eq!(atlas.font_name(parts[1].font_index), Some(label));
        assert_eq!(atlas.cluster(fam, Face::Regular, x_acute), comp, "cached repeat");

        // Curve count equals base curves + mark curves.
        let x_slot = atlas.glyph(fam, Face::Regular, 'x');
        let mark_slot = atlas.glyph(fam, Face::Regular, '\u{0301}');
        assert_eq!(
            atlas.slot_curves(comp).len(),
            atlas.slot_curves(x_slot).len() + atlas.slot_curves(mark_slot).len()
        );

        // Bold composite uses Bold face parts.
        let comp_bold = atlas.cluster(fam, Face::Bold, x_acute);
        assert_ne!(comp_bold, comp);
        let bold_parts = atlas.composite_parts(comp_bold).unwrap();
        let bold_label = format!("{label} Bold");
        assert_eq!(atlas.font_name(bold_parts[0].font_index), Some(bold_label.as_str()));
        assert_eq!(atlas.font_name(bold_parts[1].font_index), Some(bold_label.as_str()));

        // Ink verification (matches tools/font-faces.test.mjs §6):
        // Base 'x' is intact, mark sits centred above it, clear of it, inside the cell.
        let base_ink = grid_ink(&atlas, x_slot, 80);
        let both_ink = grid_ink(&atlas, comp, 80);
        let base_rows_ink: usize = both_ink
            .rows
            .iter()
            .filter(|r| r.y >= base_ink.y0 && r.y <= base_ink.y1)
            .map(|r| r.count)
            .sum();
        assert!(
            base_rows_ink as f64 >= base_ink.area * 6400.0 * 0.98,
            "{fam_name}: composite lost base ink ({base_rows_ink} vs {:.0})",
            base_ink.area * 6400.0
        );
        let mark_rows: Vec<&RowBand> = both_ink
            .rows
            .iter()
            .filter(|r| r.y > base_ink.y1 + 0.01 && r.count > 0)
            .collect();
        assert!(!mark_rows.is_empty() && both_ink.area > base_ink.area);
        let mx0 = mark_rows.iter().map(|r| r.lo).fold(f64::INFINITY, f64::min);
        let mx1 = mark_rows.iter().map(|r| r.hi).fold(f64::NEG_INFINITY, f64::max);
        let my0 = mark_rows.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
        let center_diff = ((mx0 + mx1) * 0.5 - (base_ink.x0 + base_ink.x1) * 0.5).abs();
        assert!(
            center_diff < 0.04,
            "{fam_name}: mark not centred over base (diff={center_diff:.4})"
        );
        let gap = my0 - base_ink.y1;
        assert!(gap > 0.01 && gap < 0.2, "{fam_name}: mark gap out of range ({gap:.4})");
        assert!(mx0 >= 0.0 && mx1 <= 1.0 && both_ink.y1 < 1.0, "{fam_name}: mark outside cell");

        // Two stacked marks: q + U+0308 + U+0304 -> 3-part composite with a gap between marks.
        let stack = atlas.cluster(fam, Face::Regular, q_stack);
        assert_eq!(atlas.composite_parts(stack).unwrap().len(), 3);
        let q_slot = atlas.glyph(fam, Face::Regular, 'q');
        let q_ink = grid_ink(&atlas, q_slot, 80);
        let stack_ink = grid_ink(&atlas, stack, 80);
        let over: Vec<f64> = stack_ink
            .rows
            .iter()
            .filter(|r| r.y > q_ink.y1 + 0.005 && r.count > 0)
            .map(|r| r.y)
            .collect();
        let gaps = over.windows(2).filter(|w| w[1] - w[0] > 1.5 / 80.0).count();
        assert!(
            over.len() >= 3 && gaps >= 1,
            "{fam_name}: stacked marks must have a visible gap (over={}, gaps={gaps})",
            over.len()
        );
    }

    // Distinct composites per family.
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).unwrap();
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).unwrap();
    assert_ne!(
        atlas.cluster(cousine, Face::Regular, x_acute),
        atlas.cluster(neon, Face::Regular, x_acute)
    );

    // Variation selectors, joiners, uncovered marks, sprites, and emoji sequences.
    assert_eq!(
        atlas.cluster(cousine, Face::Regular, "\u{2764}\u{FE0F}"),
        atlas.glyph(cousine, Face::Regular, '\u{2764}'),
        "variation selector FE0F is dropped"
    );
    assert_eq!(
        atlas.cluster(cousine, Face::Regular, "a\u{FE0E}"),
        atlas.glyph(cousine, Face::Regular, 'a'),
        "text selector FE0E is dropped"
    );
    assert_eq!(
        atlas.cluster(cousine, Face::Regular, "a\u{1AB0}"),
        atlas.glyph(cousine, Face::Regular, 'a'),
        "uncovered combining mark is dropped; base still draws"
    );
    assert_eq!(
        atlas.cluster(cousine, Face::Regular, "─\u{0301}"),
        atlas.glyph(cousine, Face::Regular, '─'),
        "sprite draws alone"
    );
    assert_eq!(
        atlas.cluster(cousine, Face::Regular, "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        BLANK_SLOT,
        "uncovered emoji ZWJ sequence returns BLANK_SLOT"
    );
    assert_eq!(atlas.cluster(cousine, Face::Regular, ""), BLANK_SLOT);

    // Mixed-font composite: base in Cousine, combining mark only in DejaVu Sans.
    let c_bytes: Arc<[u8]> = Arc::from(std::fs::read(assets_fonts_dir().join("Cousine-Regular.ttf")).unwrap());
    let d_bytes: Arc<[u8]> = Arc::from(std::fs::read(assets_fonts_dir().join("DejaVuSans.ttf")).unwrap());
    let c_face = ttf_parser::Face::parse(&c_bytes, 0).unwrap();
    let d_face = ttf_parser::Face::parse(&d_bytes, 0).unwrap();
    let dejavu_only_mark = (0x0300u32..=0x036Fu32).find_map(|cp| {
        let ch = char::from_u32(cp)?;
        (c_face.glyph_index(ch).is_none() && d_face.glyph_index(ch).is_some()).then_some(ch)
    });
    if let Some(mark) = dejavu_only_mark {
        let mut two = FontAtlas::load(&[
            FontSpec::new("Cousine", FontSource::Bytes(c_bytes)),
            FontSpec::new("DejaVu Sans", FontSource::Bytes(d_bytes)),
        ])
        .unwrap();
        let text = format!("x{mark}");
        let mixed = two.cluster(two.default_family(), Face::Regular, &text);
        let parts = two.composite_parts(mixed).expect("mixed-font composite");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].font_index, 0);
        assert_eq!(parts[1].font_index, 1);
    }
}
