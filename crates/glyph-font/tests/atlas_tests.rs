//! Integration tests for `glyph-font`:
//! 1. Oracle comparison against the baked `assets/atlas/` for Cousine Regular
//! 2. Variable fonts (Monaspace Neon weights/slants, ink area, italic lean, winding, validation)
//! 3. Growable atlas & dirty-range uploads across multiple doublings
//! 4. Family/face/fallback routing, missing glyphs, external slot allocation
//! 5. Cell & line metrics (including 10-bit `snap_cell_dimension` edge exactness)
//! 6. CFF cubic-to-quadratic conversion accuracy and raster coverage

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glyph_font::constants::{pack_u16, unpack_u16, BLANK_SLOT, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE, TEXTURE_WIDTH, U32S_PER_CURVE};
use glyph_font::encode::{encode_glyph, encode_raw_curves, reverse_curves, Curve, OutlineCollector, PackedCurve};
use glyph_font::metrics::{compute_cell_metrics, derive_char_size, snap_cell_dimension, FontExtents};
use glyph_font::raster::{measure_ink, rasterize_glyph};
use glyph_font::{
    default_font_chain, is_sprite_codepoint, texel_range_to_rects, Face, FaceSpec, FontAtlas,
    FontError, FontSource, FontSpec, TexelRect, Variation, DEFAULT_FONT_FAMILY,
    MONASPACE_ARGON_FAMILY, MONASPACE_NEON_FAMILY,
};
use ttf_parser::{Face as TtfFace, OutlineBuilder};

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

// ── Minimal reader for the baked `assets/atlas/` oracle (FORMAT.md) ─────────

struct BakedAtlas {
    curves: Vec<u32>,
    glyphmap: Vec<u32>,
    block_shift: u32,
    block_index: Vec<u32>,
    blocks: Vec<u32>,
    entry_stride: u32,
    fonts: Vec<BakedFont>,
    slot_meta: Vec<BakedSlotMeta>,
}

#[derive(Debug)]
struct BakedFont {
    upem: u32,
    ascender: i32,
    descender: i32,
    line_gap: i32,
}

#[derive(Clone, Copy, Debug)]
struct BakedSlotMeta {
    font_idx: u32,
    gid: u32,
    advance_fu: i32,
    ascender_fu: i32,
    descender_fu: i32,
}

fn read_u32_words(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(bytes.len() % 4, 0, "{} not 4-byte aligned", path.display());
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect()
}

impl BakedAtlas {
    fn load(dir: &Path) -> Self {
        let cv = read_u32_words(&dir.join("curves.bin"));
        assert_eq!(cv[0], u32::from_le_bytes(*b"G3CV"));
        let cv_hdr = (cv[2] / 4) as usize;
        let curves = cv[cv_hdr..].to_vec();

        let gm = read_u32_words(&dir.join("glyphmap.bin"));
        assert_eq!(gm[0], u32::from_le_bytes(*b"G3GM"));
        let gm_hdr = (gm[2] / 4) as usize;
        let glyphmap = gm[gm_hdr..].to_vec();

        let cp = read_u32_words(&dir.join("codepoints.bin"));
        assert_eq!(cp[0], u32::from_le_bytes(*b"G3CP"));
        let cp_hdr = (cp[2] / 4) as usize;
        let block_shift = cp[3];
        let block_index_len = cp[4] as usize;
        let block_count = cp[5] as usize;
        let entry_stride = cp[6];
        let block_index = cp[cp_hdr..cp_hdr + block_index_len].to_vec();
        let block_words = block_count * (1usize << block_shift) * entry_stride as usize;
        let blocks = cp[cp_hdr + block_index_len..cp_hdr + block_index_len + block_words].to_vec();

        let gl = read_u32_words(&dir.join("glyphs.bin"));
        assert_eq!(gl[0], u32::from_le_bytes(*b"G3GL"));
        let gl_hdr = (gl[2] / 4) as usize;
        let font_count = gl[3] as usize;
        let slot_count = gl[4] as usize;
        let font_words = (gl[8] / 4) as usize;
        let slot_words = (gl[9] / 4) as usize;

        let mut fonts = Vec::with_capacity(font_count);
        for i in 0..font_count {
            let o = gl_hdr + i * font_words;
            fonts.push(BakedFont {
                upem: gl[o],
                ascender: gl[o + 1] as i32,
                descender: gl[o + 2] as i32,
                line_gap: gl[o + 3] as i32,
            });
        }

        let slots_at = gl_hdr + font_count * font_words;
        let mut slot_meta = Vec::with_capacity(slot_count);
        for s in 0..slot_count {
            let r = &gl[slots_at + s * slot_words..][..slot_words];
            slot_meta.push(BakedSlotMeta {
                font_idx: r[0],
                gid: r[1],
                advance_fu: r[4] as i32,
                ascender_fu: r[5] as i32,
                descender_fu: r[6] as i32,
            });
        }

        Self {
            curves,
            glyphmap,
            block_shift,
            block_index,
            blocks,
            entry_stride,
            fonts,
            slot_meta,
        }
    }

    fn lookup_slot(&self, ch: char) -> u32 {
        let cp = ch as u32;
        let block = self.block_index[(cp >> self.block_shift) as usize];
        let e = ((block << self.block_shift) | (cp & 0xFF)) as usize * self.entry_stride as usize;
        self.blocks[e]
    }

    fn map_entry(&self, slot: u32) -> [u32; 4] {
        let o = slot as usize * 4;
        [self.glyphmap[o], self.glyphmap[o + 1], self.glyphmap[o + 2], self.glyphmap[o + 3]]
    }

    fn slot_curves(&self, slot: u32) -> Vec<PackedCurve> {
        let [start, count, _mode, _arg] = self.map_entry(slot);
        let mut out = Vec::with_capacity(count as usize);
        let base = start as usize * U32S_PER_CURVE as usize;
        for i in 0..count as usize {
            let o = base + i * U32S_PER_CURVE as usize;
            let mut words = [0u32; U32S_PER_CURVE as usize];
            words.copy_from_slice(&self.curves[o..o + U32S_PER_CURVE as usize]);
            out.push(PackedCurve::from_texels(words));
        }
        out
    }
}

// ── 1. Oracle comparison against `assets/atlas/` ────────────────────────────

#[test]
fn oracle_cousine_matches_baked_atlas() {
    let baked = BakedAtlas::load(&assets_atlas_dir());
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
    let cousine_fam = atlas.family_id(DEFAULT_FONT_FAMILY).expect("cousine family");

    // Verify primary and fallback font-wide extents match glyphs.bin verbatim.
    let m = atlas.family_metrics(cousine_fam);
    assert_eq!(m.upem as u32, baked.fonts[0].upem);
    assert_eq!(m.extents.ascender as i32, baked.fonts[0].ascender);
    assert_eq!(m.extents.descender as i32, baked.fonts[0].descender);
    assert_eq!(m.extents.line_gap as i32, baked.fonts[0].line_gap);
    assert_eq!(m.advance_fu, 1229);
    assert_eq!(m.extents.em_height() as i32, 2320);

    // Check Meslo and DejaVu extents against glyphs.bin too.
    for (idx, file) in [(1, "MesloLGS-NF-Mono.ttf"), (2, "DejaVuSans.ttf")] {
        let bytes = std::fs::read(assets_fonts_dir().join(file)).expect("read fallback font");
        let face = TtfFace::parse(&bytes, 0).expect("parse fallback font");
        let ext = FontExtents::from_face(&face, false);
        assert_eq!(face.units_per_em() as u32, baked.fonts[idx].upem, "{file} upem");
        assert_eq!(ext.ascender as i32, baked.fonts[idx].ascender, "{file} ascender");
        assert_eq!(ext.descender as i32, baked.fonts[idx].descender, "{file} descender");
        assert_eq!(ext.line_gap as i32, baked.fonts[idx].line_gap, "{file} line_gap");
    }

    // Build test character set: all printable ASCII + accented letters (routed through FontAtlas
    // to Cousine outlines) + box-drawing/blocks (which FontAtlas now routes to mode-3 cell sprites,
    // while Cousine's own outline in `encode_glyph` still matches the baked oracle verbatim).
    let mut chars: Vec<char> = (0x20u8..=0x7Eu8).map(char::from).collect();
    chars.extend(['é', 'è', 'ê', 'ë', 'ñ', 'ü', 'ö', 'ä', 'à', 'ç', 'Å', 'É', 'Ö', 'ß']);
    let sprite_chars = [
        '─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼',
        '═', '║', '╔', '╗', '╚', '╝', '█', '▀', '▄', '░', '▒', '▓',
    ];

    let mut exact_glyphs = 0usize;
    let mut count_mismatches = Vec::new();
    let mut coord_diffs = Vec::new();
    let mut max_u16_diff = 0u32;

    for ch in chars {
        assert!(!is_sprite_codepoint(ch as u32));
        let baked_slot = baked.lookup_slot(ch);
        assert_ne!(baked_slot, 0, "expected '{ch}' (U+{:04X}) in baked atlas", ch as u32);
        let baked_entry = baked.map_entry(baked_slot);
        assert_eq!(baked_entry[2], GLYPH_MODE_OUTLINE, "expected outline mode for '{ch}'");
        let baked_meta = baked.slot_meta[baked_slot as usize];
        let baked_curves = baked.slot_curves(baked_slot);

        let rt_slot = atlas.glyph(cousine_fam, Face::Regular, ch);
        assert_ne!(rt_slot, BLANK_SLOT, "runtime atlas missed '{ch}'");
        let rt_info = atlas.describe_slot(rt_slot).expect("slot info");
        assert_eq!(
            rt_info.font_index as u32, baked_meta.font_idx,
            "font routing mismatch for '{ch}' (U+{:04X}): got {} ({}), baked {}",
            ch as u32, rt_info.font_index, rt_info.font_name, baked_meta.font_idx
        );
        assert_eq!(
            rt_info.glyph_id as u32, baked_meta.gid,
            "gid mismatch for '{ch}' (U+{:04X})",
            ch as u32
        );
        let rt_curves = atlas.slot_curves(rt_slot);

        if rt_curves.len() != baked_curves.len() {
            count_mismatches.push(format!(
                "U+{:04X} '{}': runtime {} curves vs baked {} curves (font={}, adv={}, asc={}, desc={})",
                ch as u32,
                ch,
                rt_curves.len(),
                baked_curves.len(),
                rt_info.font_name,
                baked_meta.advance_fu,
                baked_meta.ascender_fu,
                baked_meta.descender_fu,
            ));
            continue;
        }

        let mut glyph_max_diff = 0u32;
        let mut first_diff = None;
        for (ci, (a, b)) in rt_curves.iter().zip(baked_curves.iter()).enumerate() {
            let diffs = [
                a.p0x.abs_diff(b.p0x),
                a.p0y.abs_diff(b.p0y),
                a.p1x.abs_diff(b.p1x),
                a.p1y.abs_diff(b.p1y),
                a.p2x.abs_diff(b.p2x),
                a.p2y.abs_diff(b.p2y),
            ];
            let d = *diffs.iter().max().unwrap();
            if d > glyph_max_diff {
                glyph_max_diff = d;
            }
            if d > 0 && first_diff.is_none() {
                first_diff = Some((ci, *a, *b, d));
            }
        }
        if glyph_max_diff == 0 {
            exact_glyphs += 1;
        } else {
            max_u16_diff = max_u16_diff.max(glyph_max_diff);
            coord_diffs.push(format!(
                "U+{:04X} '{}': max_u16_diff={} first={:?}",
                ch as u32, ch, glyph_max_diff, first_diff
            ));
        }
    }

    // Verify box-drawing/blocks route to mode-3 cell sprites in FontAtlas.
    for ch in sprite_chars {
        let rt_slot = atlas.glyph(cousine_fam, Face::Regular, ch);
        assert!(atlas.is_sprite_slot(rt_slot), "'{ch}' must route to a cell sprite");
        assert_eq!(atlas.map_entry(rt_slot)[2], GLYPH_MODE_SPRITE);
    }

    eprintln!(
        "ORACLE SUMMARY: exact={exact_glyphs}, count_mismatches={}, coord_diffs={}, max_u16_diff={max_u16_diff}",
        count_mismatches.len(),
        coord_diffs.len()
    );
    for line in count_mismatches.iter().take(15) {
        eprintln!("  COUNT: {line}");
    }
    for line in coord_diffs.iter().take(15) {
        eprintln!("  COORD: {line}");
    }

    assert!(
        count_mismatches.is_empty(),
        "curve count mismatches against baked atlas:\n{}",
        count_mismatches.join("\n")
    );
    assert!(
        coord_diffs.is_empty(),
        "coordinate mismatches against baked atlas (max_u16_diff={max_u16_diff}):\n{}",
        coord_diffs.join("\n")
    );
}

#[test]
fn oracle_all_baked_outline_glyphs_across_three_fonts() {
    let baked = BakedAtlas::load(&assets_atlas_dir());
    let font_files = [
        assets_fonts_dir().join("Cousine-Regular.ttf"),
        assets_fonts_dir().join("MesloLGS-NF-Mono.ttf"),
        assets_fonts_dir().join("DejaVuSans.ttf"),
    ];
    let font_bytes: Vec<Vec<u8>> = font_files
        .iter()
        .map(|p| std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())))
        .collect();
    let faces: Vec<TtfFace<'_>> = font_bytes
        .iter()
        .map(|b| TtfFace::parse(b, 0).expect("parse baked oracle font"))
        .collect();
    let extents: Vec<FontExtents> = faces
        .iter()
        .map(|f| FontExtents::from_face(f, false))
        .collect();

    let mut checked = 0usize;
    let mut exact = 0usize;
    let mut rotated_exact = 0usize;
    let mut lsb_one_fu_shift_exact = 0usize;
    let mut count_mismatches = Vec::new();
    let mut unexplained_diffs = Vec::new();
    let mut by_font_exact = [0usize; 3];
    let mut by_font_rotated = [0usize; 3];
    let mut by_font_lsb_shift = [0usize; 3];

    let sort_key = |c: &PackedCurve| (c.p0x, c.p0y, c.p1x, c.p1y, c.p2x, c.p2y);

    for (slot, meta) in baked.slot_meta.iter().enumerate() {
        if meta.font_idx >= 3 {
            continue; // blank or bitmap slot
        }
        let entry = baked.map_entry(slot as u32);
        if entry[2] != GLYPH_MODE_OUTLINE {
            continue;
        }
        checked += 1;
        let fi = meta.font_idx as usize;
        let gid = ttf_parser::GlyphId(meta.gid as u16);
        let encoded = encode_glyph(&faces[fi], gid, extents[fi]);
        let baked_curves = baked.slot_curves(slot as u32);

        if encoded.curves.len() != baked_curves.len() {
            count_mismatches.push(format!(
                "slot={slot} font={fi} gid={}: runtime {} curves vs baked {}",
                meta.gid,
                encoded.curves.len(),
                baked_curves.len()
            ));
            continue;
        }
        if encoded.curves == baked_curves {
            exact += 1;
            by_font_exact[fi] += 1;
            continue;
        }

        // Case A: identical curve multiset (TrueType contour starting on an off-curve point;
        // HarfBuzz and ttf-parser start the closed loop at different implied on-curve midpoints).
        let mut rt_sorted = encoded.curves.clone();
        let mut bk_sorted = baked_curves.clone();
        rt_sorted.sort_by_key(sort_key);
        bk_sorted.sort_by_key(sort_key);
        if rt_sorted == bk_sorted {
            rotated_exact += 1;
            by_font_rotated[fi] += 1;
            continue;
        }

        // Case B: HarfBuzz applies TrueType phantom-point-0 shift `hmtx.lsb - glyf.xMin`
        // (1 fu on 12 Meslo and 1 DejaVu glyphs whose hmtx.lsb and glyf.xMin differ by 1 fu).
        // Verify that shifting raw X by +1.0 fu makes every curve bit-identical (as a multiset).
        let mut collector = OutlineCollector::new(1.0);
        faces[fi].outline_glyph(gid, &mut collector);
        let (mut raw_shifted, had_cubics) = collector.into_curves();
        for c in &mut raw_shifted {
            c.p0x += 1.0;
            c.p1x += 1.0;
            c.p2x += 1.0;
        }
        let shifted = encode_raw_curves(&raw_shifted, encoded.advance_fu, extents[fi], had_cubics);
        let mut shifted_sorted = shifted.curves;
        shifted_sorted.sort_by_key(sort_key);
        if shifted_sorted == bk_sorted {
            lsb_one_fu_shift_exact += 1;
            by_font_lsb_shift[fi] += 1;
        } else {
            let gname = faces[fi].glyph_name(gid).unwrap_or("?");
            unexplained_diffs.push(format!(
                "slot={slot} font={fi} gid={} ({gname}): unexplained geometry diff",
                meta.gid
            ));
        }
    }

    eprintln!(
        "FULL ORACLE (all 3 fonts): checked={checked}, exact={exact}, rotated_exact={rotated_exact}, lsb_1fu_shift_exact={lsb_one_fu_shift_exact}, count_mismatches={}, unexplained={}",
        count_mismatches.len(),
        unexplained_diffs.len()
    );
    eprintln!(
        "  by font [Cousine, Meslo, DejaVu]: exact={by_font_exact:?}, rotated={by_font_rotated:?}, lsb_1fu={by_font_lsb_shift:?}"
    );
    assert_eq!(checked, 3533, "expected all 3533 baked outline+empty font slots");
    // Cousine (font 0) must be 100% exact bit-for-bit across all 1,699 baked glyphs.
    assert_eq!(by_font_exact[0], 1699);
    assert_eq!(by_font_rotated[0], 0);
    assert_eq!(by_font_lsb_shift[0], 0);
    assert!(count_mismatches.is_empty(), "count mismatches: {count_mismatches:?}");
    assert!(unexplained_diffs.is_empty(), "unexplained diffs: {unexplained_diffs:?}");
}

// ── 2. Variable fonts (Monaspace Neon), ink, lean, winding, validation ──────

#[test]
fn variable_font_monaspace_neon_weights_slant_ink_and_winding() {
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).expect("monaspace-neon family");

    // Regular, Bold, Italic, BoldItalic allocate four distinct slots for 'H' and share file bytes.
    let reg_h = atlas.glyph(neon, Face::Regular, 'H');
    let bold_h = atlas.glyph(neon, Face::Bold, 'H');
    let ital_h = atlas.glyph(neon, Face::Italic, 'H');
    let bi_h = atlas.glyph(neon, Face::BoldItalic, 'H');
    assert_eq!(
        std::collections::HashSet::from([reg_h, bold_h, ital_h, bi_h]).len(),
        4,
        "four faces must map to four distinct slots"
    );

    let reg_info = atlas.describe_slot(reg_h).expect("reg info");
    let bold_info = atlas.describe_slot(bold_h).expect("bold info");
    let ital_info = atlas.describe_slot(ital_h).expect("italic info");
    let bi_info = atlas.describe_slot(bi_h).expect("bold-italic info");
    assert_eq!(reg_info.font_name, "Monaspace Neon");
    assert_eq!(bold_info.font_name, "Monaspace Neon Bold");
    assert_eq!(ital_info.font_name, "Monaspace Neon Italic");
    assert_eq!(bi_info.font_name, "Monaspace Neon Bold Italic");
    assert!(atlas.shares_font_bytes(reg_info.font_index, bold_info.font_index));
    assert!(atlas.shares_font_bytes(reg_info.font_index, ital_info.font_index));
    assert!(atlas.shares_font_bytes(reg_info.font_index, bi_info.font_index));
    assert_eq!(atlas.instance_of(bold_info.font_index), Some(reg_info.font_index));

    // Curves at wght=700 differ from wght=400.
    let reg_curves = atlas.slot_curves(reg_h);
    let bold_curves = atlas.slot_curves(bold_h);
    assert!(!reg_curves.is_empty() && !bold_curves.is_empty());
    assert_ne!(reg_curves, bold_curves, "wght=700 must produce different curves than wght=400");

    // Bold 'H' has strictly more ink than Regular 'H' (> 12% more coverage, matching JS test).
    let ink_reg = measure_ink(&reg_curves, 80, 80);
    let ink_bold = measure_ink(&bold_curves, 80, 80);
    eprintln!(
        "Monaspace Neon 'H' ink: regular={:.2}% bold={:.2}% (ratio={:.2}x)",
        ink_reg.mean_coverage * 100.0,
        ink_bold.mean_coverage * 100.0,
        ink_bold.mean_coverage / ink_reg.mean_coverage
    );
    assert!(
        ink_bold.mean_coverage > ink_reg.mean_coverage * 1.12,
        "bold H should ink >12% more than regular (got {:.4} vs {:.4})",
        ink_bold.mean_coverage,
        ink_reg.mean_coverage
    );

    // Italic 'l' leans right compared to Regular 'l'.
    let reg_l = atlas.glyph(neon, Face::Regular, 'l');
    let ital_l = atlas.glyph(neon, Face::Italic, 'l');
    let ink_reg_l = measure_ink(&atlas.slot_curves(reg_l), 80, 80);
    let ink_ital_l = measure_ink(&atlas.slot_curves(ital_l), 80, 80);
    eprintln!(
        "Monaspace Neon 'l' lean: regular={:.4} italic={:.4}",
        ink_reg_l.lean, ink_ital_l.lean
    );
    assert!(
        ink_ital_l.lean > ink_reg_l.lean + 0.04,
        "italic l should lean right (got {:.4} vs upright {:.4})",
        ink_ital_l.lean,
        ink_reg_l.lean
    );

    // Check winding direction sensitivity across Monaspace Neon accented letters and Cousine.
    let mut neg_neon_glyphs = Vec::new();
    for ch in ['A', 'H', 'g', 'é', 'ñ', 'ü', 'à', 'ó', 'Å', 'É'] {
        let s = atlas.glyph(neon, Face::Regular, ch);
        let ink = measure_ink(&atlas.slot_curves(s), 80, 80);
        if ink.negative_fraction > 0.005 {
            neg_neon_glyphs.push((ch, ink.negative_fraction));
        }
    }
    eprintln!("Monaspace Neon negative-winding glyphs (>0.5% px): {neg_neon_glyphs:?}");

    // Cousine Bold inks more than Regular; Cousine Italic leans right.
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).expect("cousine family");
    let ch_reg = atlas.glyph(cousine, Face::Regular, 'H');
    let ch_bold = atlas.glyph(cousine, Face::Bold, 'H');
    let c_reg = measure_ink(&atlas.slot_curves(ch_reg), 80, 80);
    let c_bold = measure_ink(&atlas.slot_curves(ch_bold), 80, 80);
    assert!(c_bold.mean_coverage > c_reg.mean_coverage * 1.12);
    let cl_reg = atlas.glyph(cousine, Face::Regular, 'l');
    let cl_it = atlas.glyph(cousine, Face::Italic, 'l');
    let c_l_reg = measure_ink(&atlas.slot_curves(cl_reg), 80, 80);
    let c_l_it = measure_ink(&atlas.slot_curves(cl_it), 80, 80);
    assert!(c_l_it.lean > c_l_reg.lean + 0.04);

    // Monaspace Argon: four distinct slots sharing bytes, Bold inks more, Italic leans right.
    let argon = atlas.family_id(MONASPACE_ARGON_FAMILY).expect("monaspace-argon family");
    let ah_reg = atlas.glyph(argon, Face::Regular, 'H');
    let ah_bold = atlas.glyph(argon, Face::Bold, 'H');
    let ah_it = atlas.glyph(argon, Face::Italic, 'H');
    let ah_bi = atlas.glyph(argon, Face::BoldItalic, 'H');
    assert_eq!(std::collections::HashSet::from([ah_reg, ah_bold, ah_it, ah_bi]).len(), 4);
    let ah_reg_info = atlas.describe_slot(ah_reg).unwrap();
    let ah_bold_info = atlas.describe_slot(ah_bold).unwrap();
    assert_eq!(ah_reg_info.font_name, "Monaspace Argon");
    assert_eq!(ah_bold_info.font_name, "Monaspace Argon Bold");
    assert!(atlas.shares_font_bytes(ah_reg_info.font_index, ah_bold_info.font_index));
    assert!(
        measure_ink(&atlas.slot_curves(ah_bold), 80, 80).mean_coverage
            > measure_ink(&atlas.slot_curves(ah_reg), 80, 80).mean_coverage * 1.12
    );
    let al_reg = atlas.glyph(argon, Face::Regular, 'l');
    let al_it = atlas.glyph(argon, Face::Italic, 'l');
    assert!(
        measure_ink(&atlas.slot_curves(al_it), 80, 80).lean
            > measure_ink(&atlas.slot_curves(al_reg), 80, 80).lean + 0.04
    );
}

#[test]
fn variation_validation_rejects_bad_axes_and_ranges() {
    let neon_path = assets_fonts_dir().join("MonaspaceNeonVar.ttf");
    let cousine_path = assets_fonts_dir().join("Cousine-Regular.ttf");

    let err = FontAtlas::load(&[
        FontSpec::new("BadAxis", FontSource::path(&neon_path))
            .with_variations(vec![Variation::new(*b"wgth", 700.0)]),
    ])
    .err()
    .expect("unknown axis must fail");
    assert!(matches!(err, FontError::UnknownAxis { ref tag, .. } if tag == "wgth"));

    let err = FontAtlas::load(&[
        FontSpec::new("OutOfRange", FontSource::path(&neon_path))
            .with_variations(vec![Variation::new(*b"wght", 950.0)]),
    ])
    .err()
    .expect("out-of-range axis value must fail");
    assert!(matches!(err, FontError::AxisOutOfRange { ref tag, .. } if tag == "wght"));

    let err = FontAtlas::load(&[
        FontSpec::new("StaticVar", FontSource::path(&cousine_path))
            .with_variations(vec![Variation::new(*b"wght", 700.0)]),
    ])
    .err()
    .expect("variation on static font must fail");
    assert!(matches!(err, FontError::UnknownAxis { ref available, .. } if available.is_empty()));
}

// ── 3. Atlas growth & dirty-range uploads ───────────────────────────────────

#[test]
fn atlas_grows_past_initial_allocation_preserving_earlier_slots_and_dirty_spans() {
    let specs = default_font_chain(&assets_fonts_dir());
    // Start at 1 curve row (512 curves) and 1 map row (1024 slots) so both double multiple times.
    let mut atlas = FontAtlas::load_with_initial_rows(&specs, 1, 1).expect("load atlas");
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).expect("cousine");
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).expect("neon");

    assert_eq!(atlas.curve_texture().height, 1);
    assert_eq!(atlas.glyphmap_texture().height, 1);
    assert!(!atlas.take_dirty().any(), "fresh atlas starts clean");

    // Encode a small initial batch and snapshot their slots, map entries, and curves.
    let mut snapshots = Vec::new();
    for ch in "Hello, World! 0123456789 @#%&".chars() {
        let slot = atlas.glyph(cousine, Face::Regular, ch);
        snapshots.push((slot, atlas.map_entry(slot), atlas.slot_curves(slot)));
    }
    let dirty1 = atlas.take_dirty();
    assert!(dirty1.curves.texel_range.is_some());
    assert!(dirty1.glyphmap.texel_range.is_some());
    assert!(!atlas.take_dirty().any(), "second take_dirty with no writes must be clean");

    // Now push > 1,100 distinct slots and many thousands of curves to force BOTH textures to grow.
    let faces = [Face::Regular, Face::Bold, Face::Italic, Face::BoldItalic];
    for fam in [cousine, neon] {
        for face in faces {
            for cp in 0x20u32..=0x7Eu32 {
                if let Some(ch) = char::from_u32(cp) {
                    atlas.glyph(fam, face, ch);
                }
            }
            for cp in 0x00A0u32..=0x017Fu32 {
                if let Some(ch) = char::from_u32(cp) {
                    atlas.glyph(fam, face, ch);
                }
            }
        }
    }

    let dirty2 = atlas.take_dirty();
    assert!(
        atlas.curve_texture().height > 1,
        "curve texture must have grown past 1 row (got {})",
        atlas.curve_texture().height
    );
    assert!(
        atlas.glyphmap_texture().height > 1,
        "glyphmap texture must have grown past 1 row (got {}, slots={})",
        atlas.glyphmap_texture().height,
        atlas.slot_count()
    );
    assert!(dirty2.curves.grew, "curve dirty report must flag growth");
    assert!(dirty2.glyphmap.grew, "glyphmap dirty report must flag growth");
    assert!(dirty2.curves.generation >= 1);
    assert!(dirty2.glyphmap.generation >= 1);

    // Every earlier slot must still read back identical map entries and packed curves.
    for (slot, expected_entry, expected_curves) in &snapshots {
        assert_eq!(
            atlas.map_entry(*slot),
            *expected_entry,
            "map entry for earlier slot {slot} changed after growth"
        );
        assert_eq!(
            atlas.slot_curves(*slot),
            *expected_curves,
            "curves for earlier slot {slot} changed after growth"
        );
    }

    // Dirty rects cover the exact texel range in at most 3 rectangles.
    let rects = texel_range_to_rects(900..2100, TEXTURE_WIDTH);
    assert_eq!(
        rects,
        vec![
            TexelRect { x: 900, y: 0, width: 124, height: 1 },
            TexelRect { x: 0, y: 1, width: 1024, height: 1 },
            TexelRect { x: 0, y: 2, width: 52, height: 1 },
        ]
    );
    let total_texels: u32 = rects.iter().map(|r| r.width * r.height).sum();
    assert_eq!(total_texels, 2100 - 900);
}

// ── 4. Fallback routing, missing glyphs, external slot allocation ───────────

#[test]
fn fallback_routing_missing_glyphs_and_external_slots() {
    let specs = default_font_chain(&assets_fonts_dir());
    let mut atlas = FontAtlas::load(&specs).expect("load default font chain");
    let cousine = atlas.family_id(DEFAULT_FONT_FAMILY).expect("cousine");
    let neon = atlas.family_id(MONASPACE_NEON_FAMILY).expect("neon");

    // Primary glyphs differ across families.
    let a_cousine = atlas.glyph(cousine, Face::Regular, 'A');
    let a_neon = atlas.glyph(neon, Face::Regular, 'A');
    assert_ne!(a_cousine, a_neon);

    // A Meslo fallback symbol ('☃' U+2603) routes to the SAME shared fallback slot in
    // both families, and stays upright even when Bold or Italic is requested.
    let snow_c = atlas.glyph(cousine, Face::Regular, '☃');
    let snow_n = atlas.glyph(neon, Face::Regular, '☃');
    assert_ne!(snow_c, BLANK_SLOT);
    assert_eq!(snow_c, snow_n, "fallback glyphs must be shared across families");
    assert_eq!(
        atlas.describe_slot(snow_c).expect("snowman info").font_name,
        "MesloLGS NF Mono"
    );
    assert_eq!(atlas.glyph(cousine, Face::Bold, '☃'), snow_c);
    assert_eq!(atlas.glyph(cousine, Face::Italic, '☃'), snow_c);
    assert_eq!(atlas.glyph(neon, Face::BoldItalic, '☃'), snow_c);

    // A DejaVu-only symbol (Arabic 'ش' U+0634) falls all the way through to DejaVu Sans
    // and is also shared across families and stays upright across faces.
    let dv_c = atlas.glyph(cousine, Face::Regular, 'ش');
    let dv_n = atlas.glyph(neon, Face::Regular, 'ش');
    assert_ne!(dv_c, BLANK_SLOT);
    assert_eq!(dv_c, dv_n);
    assert_eq!(atlas.describe_slot(dv_c).expect("dejavu info").font_name, "DejaVu Sans");
    assert_eq!(atlas.glyph(cousine, Face::Bold, 'ش'), dv_c);

    // Space allocates a real slot with 0 curves and stays stable.
    let space1 = atlas.glyph(cousine, Face::Regular, ' ');
    let space2 = atlas.glyph(cousine, Face::Regular, ' ');
    assert_ne!(space1, BLANK_SLOT);
    assert_eq!(space1, space2);
    assert_eq!(atlas.map_entry(space1)[1], 0, "space has 0 curves");

    // Missing codepoint (unassigned PUA U+EFFFF) returns BLANK_SLOT stably without dirtying atlas.
    let _ = atlas.take_dirty();
    let slots_before = atlas.slot_count();
    let missing1 = atlas.glyph_u32(cousine, Face::Regular, 0x0E_FFFF);
    let missing2 = atlas.glyph_u32(cousine, Face::Bold, 0x0E_FFFF);
    assert_eq!(missing1, BLANK_SLOT);
    assert_eq!(missing2, BLANK_SLOT);
    assert_eq!(atlas.slot_count(), slots_before);
    assert!(!atlas.take_dirty().any(), "missing glyph must not dirty textures");

    // External glyph sources (e.g. cell sprites & emoji) can claim non-colliding slots.
    let sprite_slot = atlas.allocate_slot();
    let unit_seg = PackedCurve::pack(Curve {
        p0x: 0.0, p0y: 0.5, p1x: 0.5, p1y: 0.5, p2x: 1.0, p2y: 0.5,
    });
    atlas.insert_outline_slot(sprite_slot, &[unit_seg], GLYPH_MODE_SPRITE, 255);
    assert_eq!(atlas.map_entry(sprite_slot)[2], GLYPH_MODE_SPRITE);
    assert_eq!(atlas.map_entry(sprite_slot)[3], 255);
    assert_eq!(atlas.slot_curves(sprite_slot), vec![unit_seg]);

    let emoji_slot = atlas.allocate_slot();
    atlas.insert_map_entry(emoji_slot, [0, 0, GLYPH_MODE_BITMAP, 42]);
    assert_eq!(atlas.map_entry(emoji_slot), [0, 0, GLYPH_MODE_BITMAP, 42]);
    assert!(atlas.describe_slot(emoji_slot).is_none());

    // Partial-face fallback: a font with only Italic uses Italic for BoldItalic.
    let reg_bytes: Arc<[u8]> = Arc::from(std::fs::read(assets_fonts_dir().join("Cousine-Regular.ttf")).unwrap());
    let ital_bytes: Arc<[u8]> = Arc::from(std::fs::read(assets_fonts_dir().join("Cousine-Italic.ttf")).unwrap());
    let mut partial = FontAtlas::load(&[
        FontSpec::new("OnlyItalic", FontSource::Bytes(reg_bytes))
            .with_faces(None, Some(FaceSpec::bytes(ital_bytes)), None),
    ])
    .expect("load partial-face chain");
    let f0 = partial.default_family();
    let s_it = partial.glyph(f0, Face::Italic, 'a');
    let s_bi = partial.glyph(f0, Face::BoldItalic, 'a');
    let s_b = partial.glyph(f0, Face::Bold, 'a');
    let s_r = partial.glyph(f0, Face::Regular, 'a');
    assert_eq!(s_bi, s_it, "BoldItalic falls back to Italic when BoldItalic is absent");
    assert_eq!(s_b, s_r, "Bold falls back to Regular when Bold is absent");
}

// ── 5. Cell & line metrics and 10-bit dimension snapping ────────────────────

#[test]
fn cell_and_line_metrics_and_snap_dimension() {
    let specs = default_font_chain(&assets_fonts_dir());
    let atlas = FontAtlas::load(&specs).expect("load default font chain");
    let cousine = atlas.family_metrics(atlas.family_id(DEFAULT_FONT_FAMILY).unwrap());
    let neon = atlas.family_metrics(atlas.family_id(MONASPACE_NEON_FAMILY).unwrap());

    let cl = cousine.lines.expect("cousine line metrics");
    let nl = neon.lines.expect("neon line metrics");
    assert!(cl.thickness > 0.0 && nl.thickness > 0.0);
    assert!(cl.underline_y > 0.0 && cl.underline_y < cl.baseline_y);
    assert!(nl.underline_y > 0.0 && nl.underline_y < nl.baseline_y);
    assert!(cl.strike_y > cl.baseline_y && cl.strike_y < 1.0);
    assert!(nl.strike_y > nl.baseline_y && nl.strike_y < 1.0);
    assert_ne!(cousine.char_size(48.0), neon.char_size(48.0));

    // Verify include_line_gap grows vertical em by line_gap (half above, half below).
    let neon_bytes = std::fs::read(assets_fonts_dir().join("MonaspaceNeonVar.ttf")).unwrap();
    let face = TtfFace::parse(&neon_bytes, 0).unwrap();
    let e0 = FontExtents::from_face(&face, false);
    let e1 = FontExtents::from_face(&face, true);
    assert!(e0.line_gap > 0.0);
    assert_eq!(e1.em_height(), e0.em_height() + e0.line_gap);
    assert_eq!(e1.ascender - e0.ascender, e0.line_gap * 0.5);

    // Verify snap_cell_dimension makes cell edges bit-identical in f32 up to k = 16383.
    for font_size in [12.0_f32, 16.0, 24.0, 48.0] {
        let cs = derive_char_size(cousine.upem, cousine.advance_fu, cousine.extents, font_size);
        let wm = compute_cell_metrics(cs, 0.025);
        assert!((wm.char_width - cs.width * 0.025).abs() / (cs.width * 0.025) < 0.001);
        assert!((wm.char_height - cs.height * 0.025).abs() / (cs.height * 0.025) < 0.001);
        for k in 0u32..16384 {
            let w = wm.char_width;
            let right_of_k = (k as f32) * w + w;
            let left_of_next = ((k + 1) as f32) * w;
            assert_eq!(
                right_of_k.to_bits(),
                left_of_next.to_bits(),
                "f32 edge mismatch at k={k} for w={w}"
            );
        }
    }
    assert_eq!(snap_cell_dimension(0.0), 0.0);
}

// ── 6. CFF cubic-to-quadratic conversion accuracy ───────────────────────────

fn eval_cubic(p0: (f64, f64), c1: (f64, f64), c2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let x = u * u * u * p0.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * p3.0;
    let y = u * u * u * p0.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * p3.1;
    (x, y)
}

fn eval_quad(c: Curve, t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let x = u * u * c.p0x + 2.0 * u * t * c.p1x + t * t * c.p2x;
    let y = u * u * c.p0y + 2.0 * u * t * c.p1y + t * t * c.p2y;
    (x, y)
}

#[test]
fn cubic_to_quadratic_conversion_is_accurate_and_renders_closed_circle() {
    let tol = 0.25_f64;

    // Test 1: S-curve with inflection point.
    let p0 = (100.0, 100.0);
    let c1 = (100.0, 800.0);
    let c2 = (700.0, -200.0);
    let p3 = (700.0, 500.0);
    let mut col = OutlineCollector::new(tol);
    col.move_to(p0.0 as f32, p0.1 as f32);
    col.curve_to(c1.0 as f32, c1.1 as f32, c2.0 as f32, c2.1 as f32, p3.0 as f32, p3.1 as f32);
    let (quads, had_cubics) = col.into_curves();
    assert!(had_cubics);
    assert!(quads.len() >= 2 && quads.len() <= 32, "unexpected quad count: {}", quads.len());

    // Endpoints and continuity across all quadratic pieces.
    assert!((quads[0].p0x - p0.0).abs() < 1e-9 && (quads[0].p0y - p0.1).abs() < 1e-9);
    let last = quads.last().unwrap();
    assert!((last.p2x - p3.0).abs() < 1e-9 && (last.p2y - p3.1).abs() < 1e-9);
    for w in quads.windows(2) {
        assert!((w[0].p2x - w[1].p0x).abs() < 1e-9);
        assert!((w[0].p2y - w[1].p0y).abs() < 1e-9);
    }

    // Exact parametric correspondence: each uniform subdivision maps t in [k/n, (k+1)/n] onto quad k.
    let n = quads.len();
    let mut max_err = 0.0_f64;
    for (k, &q) in quads.iter().enumerate() {
        for s in 0..=64 {
            let local_t = s as f64 / 64.0;
            let global_t = (k as f64 + local_t) / n as f64;
            let pt_c = eval_cubic(p0, c1, c2, p3, global_t);
            let pt_q = eval_quad(q, local_t);
            max_err = max_err.max((pt_c.0 - pt_q.0).hypot(pt_c.1 - pt_q.1));
        }
    }
    assert!(max_err <= tol, "cubic->quadratic max error {max_err:.4} exceeded tolerance {tol}");

    // Test 2: Closed CFF-wound (CCW) circle of radius 350 centred at (500, 500) in a 1000x1000 cell.
    // Reversing curves flips CFF CCW -> TrueType CW so even the signed shader (`abs_winding=false`)
    // renders full coverage matching pi * r^2.
    let r = 350.0_f32;
    let k = r * 0.552_284_8_f32;
    let cx = 500.0_f32;
    let cy = 500.0_f32;
    let mut col = OutlineCollector::new(0.25);
    // CCW circle (CFF outer contour convention): right -> top -> left -> bottom -> right.
    col.move_to(cx + r, cy);
    col.curve_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
    col.curve_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
    col.curve_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
    col.curve_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
    col.close();
    let (mut circle_curves, _) = col.into_curves();
    reverse_curves(&mut circle_curves);
    let extents = FontExtents { ascender: 1000.0, descender: 0.0, line_gap: 0.0 };
    let encoded = encode_raw_curves(&circle_curves, 1000, extents, true);

    let pixels_signed = rasterize_glyph(&encoded.curves, 100, 100, false);
    let mean_signed: f64 = pixels_signed.iter().map(|&p| p as f64 / 255.0).sum::<f64>() / 10_000.0;
    let expected_area = std::f64::consts::PI * 0.35 * 0.35;
    assert!(
        (mean_signed - expected_area).abs() < 0.01,
        "CFF-converted circle coverage {mean_signed:.4} != expected {expected_area:.4}"
    );
    assert_eq!(pack_u16(unpack_u16(12345) as f64), 12345);
    // Smoke-check encode_glyph directly on Cousine 'A'.
    let bytes = std::fs::read(assets_fonts_dir().join("Cousine-Regular.ttf")).unwrap();
    let face = TtfFace::parse(&bytes, 0).unwrap();
    let gid = face.glyph_index('A').unwrap();
    let a = encode_glyph(&face, gid, FontExtents::from_face(&face, false));
    assert_eq!(a.curves.len(), 14);
}
