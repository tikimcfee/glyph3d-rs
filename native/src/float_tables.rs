//! The glyph shaders' lookup tables as float texels, for a host whose data
//! textures are f32 only (2026-10-26). The makepad port's draw shaders
//! sample `VecRGBAf32` and `VecRf32` textures where the wgpu renderer binds
//! `Rgba32Uint` textures and storage buffers; the makepad host calls this
//! module and uploads what it returns. The wgpu renderer reads none of it:
//! `atlas.rs` and the field crates upload the same tables in their integer
//! forms, from the same readers ([`SlugTexels`], [`atlas::slot_advances`]).
//!
//! **An integer travels as its exact f32 value, never as its bits.** Every
//! integer below 2^24 has an exact f32. A u32 whose bits are reinterpreted as
//! f32 does not survive a float texture: every small integer is a denormal
//! (557, where 'A''s curves start, is a subnormal as bits), which a GPU may flush to
//! zero on upload or sample, and a word with its top exponent bits set is a
//! NaN, whose payload may be canonicalised. Both corrupt the table silently.
//! So every integer lane below is `value as f32`, a converter PANICS on an
//! integer at or past 2^24 (naming the table, the index, the lane and the
//! value) rather than round it, and the one sentinel a table carries — the
//! glyph map's NO_CELL, 0xFFFFFFFF — becomes -1.0. f32 lanes (group rows, the
//! item table's floats, advances) are the same f32 values, bit for bit.
//!
//! The layouts, row-major, `width` texels a row, read nearest at texel
//! centres (the WGSL each one mirrors is `glyph_field_derived.wgsl`):
//!
//! | table | source | width | texels | lanes |
//! |---|---|---|---|---|
//! | glyph map | `glyphmap.bin`, 1024x10 | 1024 | 1 per atlas slot | `[curve_start, curve_count, mode, emoji_cell]` |
//! | curves | `curves.bin`, 1024x161 | 1024 | 2 per curve | `[p0.x, p0.y, p1.x, p1.y]`, `[p2.x, p2.y, 0, 0]` |
//! | groups | `GroupRow`, 96 B | 1020 | 6 per group | the row's six vec4s, in order |
//! | item table | `ItemParamsGpu`, 64 B | 1024 | 4 per item | its sixteen words, in order |
//! | advances | `atlas::slot_advances` | 1024 | 1 per slot, R | the advance |
//! | group overrides | the Derived field's u32 table | 1024 | 1 per override, R | the group row |
//!
//! The glyph map and curves keep their files' shapes (the shaders address
//! slot `g` at `(g % 1024, g / 1024)` and curve texel `t` the same way), zero
//! padding included. In the glyph map `mode` is 0 (outline: `emoji_cell` is
//! 0) or 1 (bitmap: no curves, `emoji_cell` a sheet cell or NO_CELL, which
//! the shader draws blank and here reads -1.0). A curve coordinate is a u16
//! (0..=65535): the shader keeps its `/ 65535.0`. The group table is 1020
//! wide so a group's six texels never straddle a row (170 groups a row),
//! which is what lets [`group_rows_rect`] name a dirty span as one rectangle.
//! The item table's ints are `page_rows`, `pages_wide`, `page_cols`,
//! `scroll_rows` (i32, texel 2), `has_page` and `group` (texel 3, lanes 0
//! and 2), and `_pad2` (texel 3 lane 3, 0); every other lane is f32.

use std::path::Path;

use glyph_field::{GroupRow, ItemParamsGpu};

use crate::atlas::{self, SlugTexels, TrieTable, ATLAS_TEX_WIDTH};

/// The first integer a table may not carry: 2^24. Below it every integer is
/// an exact f32 (2^24 itself is too, but 2^24 + 1 is not, and one bound for
/// both signs and every lane is the rule the host can state).
pub const EXACT_INT_LIMIT: u32 = 1 << 24;

/// The glyph map's "no sheet cell" sentinel (FORMAT.md, glyphmap.bin), and
/// what it reads as in a float glyph map.
pub const NO_CELL: u32 = 0xFFFF_FFFF;
pub const NO_CELL_TEXEL: f32 = -1.0;

/// Width of every table but the group table, in texels.
pub const TABLE_WIDTH: u32 = ATLAS_TEX_WIDTH;
/// Texels per group row (`GroupRow`'s six vec4s) and the group table's
/// width: 170 whole groups a row.
pub const GROUP_TEXELS: u32 = 6;
pub const GROUP_TABLE_WIDTH: u32 = 1020;
const GROUPS_PER_ROW: u32 = GROUP_TABLE_WIDTH / GROUP_TEXELS;
/// Texels per item (`ItemParamsGpu`'s sixteen words): 256 items a row.
pub const ITEM_TEXELS: u32 = 4;

/// One table as a float texture: `width x height` texels of `channels`
/// floats each (4 for `VecRGBAf32`, 1 for `VecRf32`), row-major. Texels past
/// the table's content are 0.0.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatTexture {
    pub width: u32,
    pub height: u32,
    pub channels: u32,
    pub data: Vec<f32>,
}

/// A texel rectangle of a [`FloatTexture`]: what a partial upload names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The three static tables the atlas directory makes.
pub struct AtlasTables {
    pub glyphmap: FloatTexture,
    pub curves: FloatTexture,
    pub glyph_advances: FloatTexture,
}

/// Read an atlas directory (`crate::atlas_dir()` is the committed one) and
/// convert its glyph map, curves and advances. The same checks as the wgpu
/// atlas load: magic, width, payload size, and the glyph map's slot count
/// against glyphs.bin's.
pub fn atlas_tables(dir: &Path) -> AtlasTables {
    let gm = SlugTexels::read(&dir.join("glyphmap.bin"), "G3GM");
    let cv = SlugTexels::read(&dir.join("curves.bin"), "G3CV");
    let trie = TrieTable::load(dir);
    assert_eq!(trie.slot_count, gm.count(), "glyphmap/glyphs slot count mismatch");
    AtlasTables {
        glyphmap: glyphmap_texels(gm.width(), gm.height(), gm.payload()),
        curves: curve_texels(cv.width(), cv.height(), cv.payload()),
        glyph_advances: advance_texels(&atlas::slot_advances(&trie)),
    }
}

/// [`atlas_tables`] from an atlas already loaded (a scene's, 2026-10-31:
/// `FrameData::atlas`), so a host renderer converts the tables the scene
/// was staged against without reading or decoding them again.
pub fn atlas_tables_of(atlas: &atlas::Atlas) -> AtlasTables {
    let (gm, cv) = (&atlas.glyphmap, &atlas.curves);
    AtlasTables {
        glyphmap: glyphmap_texels(gm.width(), gm.height(), gm.payload()),
        curves: curve_texels(cv.width(), cv.height(), cv.payload()),
        glyph_advances: advance_texels(&atlas.glyph_advances),
    }
}

/// The glyph map, one texel per slot: every lane exact, NO_CELL in the
/// `emoji_cell` lane as -1.0. NO_CELL in any other lane is refused like any
/// integer past 2^24.
pub fn glyphmap_texels(width: u32, height: u32, payload: &[u32]) -> FloatTexture {
    const LANES: [&str; 4] = ["curve_start", "curve_count", "mode", "emoji_cell"];
    assert_rgba_payload("glyphmap", width, height, payload);
    let data = payload
        .iter()
        .enumerate()
        .map(|(i, &v)| match i % 4 {
            3 if v == NO_CELL => NO_CELL_TEXEL,
            lane => exact_u32(v, "glyphmap", "slot", i / 4, LANES[lane]),
        })
        .collect();
    FloatTexture { width, height, channels: 4, data }
}

/// The curve texture, two texels per curve, every lane exact.
pub fn curve_texels(width: u32, height: u32, payload: &[u32]) -> FloatTexture {
    const LANES: [&str; 4] = ["x", "y", "z", "w"];
    assert_rgba_payload("curves", width, height, payload);
    let data = payload
        .iter()
        .enumerate()
        .map(|(i, &v)| exact_u32(v, "curves", "texel", i / 4, LANES[i % 4]))
        .collect();
    FloatTexture { width, height, channels: 4, data }
}

/// The group table, six texels per row, 1020 wide.
pub fn group_texels(rows: &[GroupRow]) -> FloatTexture {
    let mut texture = zeroed(GROUP_TABLE_WIDTH, 4, rows.len() * GROUP_TEXELS as usize);
    texture.data[..rows.len() * 24].copy_from_slice(bytemuck::cast_slice::<GroupRow, f32>(rows));
    texture
}

/// Write `rows` into a group table from group `first` on, and return the
/// rectangle the write dirtied (`None` for no rows). Refuses a write past the
/// texture's rows: growing it is a fresh [`group_texels`].
pub fn write_group_rows(texture: &mut FloatTexture, first: u32, rows: &[GroupRow]) -> Option<TexelRect> {
    assert!(
        texture.width == GROUP_TABLE_WIDTH && texture.channels == 4,
        "group table: a {}x{} texture of {} channels is not one",
        texture.width,
        texture.height,
        texture.channels
    );
    let end = first as usize + rows.len();
    let capacity = (texture.height * GROUPS_PER_ROW) as usize;
    assert!(end <= capacity, "group table: rows {first}..{end} past its {capacity} rows");
    texture.data[first as usize * 24..end * 24].copy_from_slice(bytemuck::cast_slice::<GroupRow, f32>(rows));
    group_rows_rect(first, end as u32)
}

/// The rectangle that covers group rows `start..end` (half-open, as the
/// scene graph's dirty ranges are): their exact span when they share a
/// texture row, else whole texture rows (makepad's dirty-rect idiom,
/// `slug_atlas.rs`'s `appended_dirty_rect`). `None` when the range is empty.
pub fn group_rows_rect(start: u32, end: u32) -> Option<TexelRect> {
    if end <= start {
        return None;
    }
    let (first_row, last_row) = (start / GROUPS_PER_ROW, (end - 1) / GROUPS_PER_ROW);
    Some(if first_row == last_row {
        TexelRect {
            x: (start % GROUPS_PER_ROW) * GROUP_TEXELS,
            y: first_row,
            width: (end - start) * GROUP_TEXELS,
            height: 1,
        }
    } else {
        TexelRect { x: 0, y: first_row, width: GROUP_TABLE_WIDTH, height: last_row - first_row + 1 }
    })
}

/// The item table, four texels per item: the f32 fields as they are, the
/// int fields exact.
pub fn item_texels(items: &[ItemParamsGpu]) -> FloatTexture {
    let mut texture = zeroed(TABLE_WIDTH, 4, items.len() * ITEM_TEXELS as usize);
    for (i, p) in items.iter().enumerate() {
        let int = |v: i32, lane| exact_i32(v, "item_table", "item", i, lane);
        let uint = |v: u32, lane| exact_u32(v, "item_table", "item", i, lane);
        let texels: [f32; 16] = [
            p.line_height,
            p.origin_y,
            p.origin_z,
            p.z_step,
            p.z_step_lo,
            p.band_stride_y,
            p.depth_per_band,
            p.depth_per_col,
            int(p.page_rows, "page_rows"),
            int(p.pages_wide, "pages_wide"),
            int(p.page_cols, "page_cols"),
            int(p.scroll_rows, "scroll_rows"),
            uint(p.has_page, "has_page"),
            p.line_height_lo,
            uint(p.group, "group"),
            uint(p._pad2, "_pad2"),
        ];
        texture.data[i * 16..][..16].copy_from_slice(&texels);
    }
    texture
}

/// The per-slot advances, one R texel each.
pub fn advance_texels(advances: &[f32]) -> FloatTexture {
    let mut texture = zeroed(TABLE_WIDTH, 1, advances.len());
    texture.data[..advances.len()].copy_from_slice(advances);
    texture
}

/// The Derived field's group overrides (index 0 is "none"), one R texel
/// each, every group row exact.
pub fn override_texels(overrides: &[u32]) -> FloatTexture {
    let mut texture = zeroed(TABLE_WIDTH, 1, overrides.len());
    for (i, &g) in overrides.iter().enumerate() {
        texture.data[i] = exact_u32(g, "group_overrides", "override", i, "group");
    }
    texture
}

/// A zero texture of `width` texels a row holding at least `texels` texels
/// (at least one row: a float texture of no texels is not a texture).
fn zeroed(width: u32, channels: u32, texels: usize) -> FloatTexture {
    let height = texels.div_ceil(width as usize).max(1) as u32;
    FloatTexture { width, height, channels, data: vec![0.0; (width * height * channels) as usize] }
}

fn assert_rgba_payload(table: &str, width: u32, height: u32, payload: &[u32]) {
    assert_eq!(
        payload.len(),
        (width * height * 4) as usize,
        "{table}: payload is not {width}x{height} RGBA texels"
    );
}

#[inline]
fn exact_u32(v: u32, table: &str, unit: &str, index: usize, lane: &str) -> f32 {
    assert!(
        v < EXACT_INT_LIMIT,
        "{table}: {unit} {index} {lane} = {v} (0x{v:08x}) is not below 2^24, so it has no exact f32"
    );
    v as f32
}

#[inline]
fn exact_i32(v: i32, table: &str, unit: &str, index: usize, lane: &str) -> f32 {
    assert!(
        v.unsigned_abs() < EXACT_INT_LIMIT,
        "{table}: {unit} {index} {lane} = {v} is not within 2^24 of zero, so it has no exact f32"
    );
    v as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inverse of an exact lane: the integer back, refusing a float that
    /// is not one (a lane that rounded, or was ever a bit pattern).
    fn back_u32(f: f32) -> u32 {
        assert!(f >= 0.0 && f.fract() == 0.0 && f < EXACT_INT_LIMIT as f32, "{f} is not an exact u32 lane");
        f as u32
    }

    fn back_i32(f: f32) -> i32 {
        assert!(f.fract() == 0.0 && f.abs() < EXACT_INT_LIMIT as f32, "{f} is not an exact i32 lane");
        f as i32
    }

    fn back_glyphmap(t: &FloatTexture) -> Vec<u32> {
        t.data
            .iter()
            .enumerate()
            .map(|(i, &f)| if i % 4 == 3 && f == NO_CELL_TEXEL { NO_CELL } else { back_u32(f) })
            .collect()
    }

    fn back_items(t: &FloatTexture, n: usize) -> Vec<ItemParamsGpu> {
        (0..n)
            .map(|i| {
                let d = &t.data[i * 16..][..16];
                ItemParamsGpu {
                    line_height: d[0],
                    origin_y: d[1],
                    origin_z: d[2],
                    z_step: d[3],
                    z_step_lo: d[4],
                    band_stride_y: d[5],
                    depth_per_band: d[6],
                    depth_per_col: d[7],
                    page_rows: back_i32(d[8]),
                    pages_wide: back_i32(d[9]),
                    page_cols: back_i32(d[10]),
                    scroll_rows: back_i32(d[11]),
                    has_page: back_u32(d[12]),
                    line_height_lo: d[13],
                    group: back_u32(d[14]),
                    _pad2: back_u32(d[15]),
                }
            })
            .collect()
    }

    /// An integer-sourced lane can never be what a bit cast makes: finite,
    /// a whole number, and 0 or a NORMAL float (the smallest nonzero is 1.0,
    /// 2^-126 above every denormal), or the -1.0 sentinel.
    fn assert_no_bit_sourced_lane(table: &str, data: &[f32]) {
        for (i, &f) in data.iter().enumerate() {
            assert!(
                f.is_finite() && f.fract() == 0.0 && (f == 0.0 || f.is_normal()),
                "{table}: lane {i} = {f:e} (0x{:08x}) looks bit-sourced",
                f.to_bits()
            );
        }
    }

    fn texture_eq_bits(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    #[test]
    fn the_shipped_atlas_round_trips_exactly() {
        let dir = crate::atlas_dir();
        let tables = atlas_tables(&dir);
        let gm = SlugTexels::read(&dir.join("glyphmap.bin"), "G3GM");
        let cv = SlugTexels::read(&dir.join("curves.bin"), "G3CV");

        assert_eq!((tables.glyphmap.width, tables.glyphmap.height), (gm.width(), gm.height()));
        assert_eq!(back_glyphmap(&tables.glyphmap), gm.payload(), "glyph map");
        assert_eq!((tables.curves.width, tables.curves.height), (cv.width(), cv.height()));
        let curves: Vec<u32> = tables.curves.data.iter().map(|&f| back_u32(f)).collect();
        assert_eq!(curves, cv.payload(), "curves");

        // The pins FORMAT.md documents, read through the float tables: 'A'
        // is slot 34, 14 curves from curve 557; RAT is bitmap cell 487; and
        // the atlas has sentinel slots, so the remap is exercised here.
        let slot = |g: usize| &tables.glyphmap.data[g * 4..][..4];
        assert_eq!(slot(34), [557.0, 14.0, 0.0, 0.0]);
        assert_eq!(slot(3839), [0.0, 0.0, 1.0, 487.0]);
        assert_eq!(&tables.curves.data[557 * 8..][..8], [55137.0, 17372.0, 51458.0, 22768.0, 47778.0, 28163.0, 0.0, 0.0]);
        let no_cell = tables.glyphmap.data.iter().skip(3).step_by(4).filter(|&&f| f == NO_CELL_TEXEL).count();
        assert!(no_cell > 0, "the shipped glyph map carries no NO_CELL slot: the remap went unexercised");

        let advances = atlas::slot_advances(&TrieTable::load(&dir));
        assert_eq!(tables.glyph_advances.channels, 1);
        assert!(texture_eq_bits(&tables.glyph_advances.data[..advances.len()], &advances), "advances, bit for bit");
        assert!(tables.glyph_advances.data[advances.len()..].iter().all(|&f| f == 0.0));
    }

    #[test]
    fn no_integer_lane_is_bit_sourced() {
        let tables = atlas_tables(&crate::atlas_dir());
        assert_no_bit_sourced_lane("glyphmap", &tables.glyphmap.data);
        assert_no_bit_sourced_lane("curves", &tables.curves.data);
        // What the rule is for: 'A''s curve start as bits is a denormal, and
        // the NO_CELL sentinel as bits is a NaN.
        assert!(f32::from_bits(557).is_subnormal());
        assert!(f32::from_bits(NO_CELL).is_nan());
        assert_eq!(tables.glyphmap.data[34 * 4], 557.0);

        let edges = [0, 1, 65_535, EXACT_INT_LIMIT - 1];
        let overrides = override_texels(&edges);
        assert_no_bit_sourced_lane("group_overrides", &overrides.data);
        let item = ItemParamsGpu {
            page_rows: (EXACT_INT_LIMIT - 1) as i32,
            scroll_rows: -((EXACT_INT_LIMIT - 1) as i32),
            has_page: 1,
            group: EXACT_INT_LIMIT - 1,
            ..ItemParamsGpu::default()
        };
        let items = item_texels(&[item]);
        let int_lanes: Vec<f32> = [8, 9, 10, 11, 12, 14, 15].iter().map(|&l| items.data[l]).collect();
        assert_no_bit_sourced_lane("item_table", &int_lanes);
    }

    #[test]
    fn edge_values_round_trip() {
        let top = EXACT_INT_LIMIT - 1;
        // Glyph map: every lane at each edge, NO_CELL in emoji_cell.
        let mut gm = vec![0u32; 1024 * 4];
        gm[..16].copy_from_slice(&[0, 0, 0, 0, top, top, top, top, 65_535, 65_535, 1, NO_CELL, 82_207, 468, 1, 3984]);
        let t = glyphmap_texels(1024, 1, &gm);
        assert_eq!(&t.data[..16], [0.0, 0.0, 0.0, 0.0, 16_777_215.0, 16_777_215.0, 16_777_215.0, 16_777_215.0, 65_535.0, 65_535.0, 1.0, -1.0, 82_207.0, 468.0, 1.0, 3984.0]);
        assert_eq!(back_glyphmap(&t), gm);

        let mut cv = vec![0u32; 1024 * 4];
        cv[..8].copy_from_slice(&[0, 65_535, top, 1, 65_535, 0, 0, 0]);
        let t = curve_texels(1024, 1, &cv);
        assert_eq!(t.data.iter().map(|&f| back_u32(f)).collect::<Vec<_>>(), cv);

        let overrides = [0, 1, 4095, 65_535, top];
        let t = override_texels(&overrides);
        assert_eq!((t.width, t.height, t.channels), (1024, 1, 1));
        assert_eq!(t.data[..5].iter().map(|&f| back_u32(f)).collect::<Vec<_>>(), overrides);
        // The Derived field's full table (index 0 "none" .. OVERRIDE_MAX).
        let full = override_texels(&vec![7; glyph_field_derived::OVERRIDE_MAX as usize + 1]);
        assert_eq!((full.width, full.height), (1024, 4));

        let advances = [0.0, 0.529_741_4, 1.059_482_8, f32::MIN_POSITIVE];
        let t = advance_texels(&advances);
        assert!(texture_eq_bits(&t.data[..4], &advances));
    }

    #[test]
    fn item_table_round_trips() {
        let top = (EXACT_INT_LIMIT - 1) as i32;
        let items = [
            ItemParamsGpu::default(),
            ItemParamsGpu {
                line_height: 1.0,
                origin_y: -2.5,
                origin_z: 3.0e-5,
                z_step: 0.05,
                z_step_lo: 1.4e-9,
                band_stride_y: 110.0,
                depth_per_band: -0.75,
                depth_per_col: 12.0,
                page_rows: 100,
                pages_wide: 4,
                page_cols: 0,
                scroll_rows: -3,
                has_page: 1,
                line_height_lo: -2.9e-8,
                group: 29_000,
                _pad2: 0,
            },
            ItemParamsGpu {
                page_rows: top,
                pages_wide: -top,
                page_cols: top,
                scroll_rows: -top,
                has_page: EXACT_INT_LIMIT - 1,
                group: EXACT_INT_LIMIT - 1,
                ..ItemParamsGpu::default()
            },
        ];
        let t = item_texels(&items);
        assert_eq!((t.width, t.height, t.channels), (1024, 1, 4));
        let back = back_items(&t, items.len());
        assert_eq!(bytemuck::cast_slice::<_, u8>(&back), bytemuck::cast_slice::<_, u8>(&items), "bit for bit");
        // 257 items need a second row: 256 a row.
        assert_eq!(item_texels(&vec![ItemParamsGpu::default(); 257]).height, 2);
    }

    #[test]
    fn group_table_round_trips_and_names_its_dirty_rect() {
        let row = |g: u32| {
            let f = g as f32;
            GroupRow { cols: [[f, -f, 0.5, 0.0], [0.0, 0.0, 0.0, 1.0], [1.0, 0.25, f32::MIN_POSITIVE, 1.0], [2.0, 2.0, 2.0, 0.0], [1e10, -1e10, 1.0, 0.0], [0.1, 0.2, 0.3, f]] }
        };
        let rows: Vec<GroupRow> = (0..400).map(row).collect();
        let mut t = group_texels(&rows);
        assert_eq!((t.width, t.height, t.channels), (1020, 3, 4), "170 groups a row");
        let back: &[GroupRow] = bytemuck::cast_slice(&t.data[..rows.len() * 24]);
        assert_eq!(bytemuck::cast_slice::<_, u8>(back), bytemuck::cast_slice::<_, u8>(&rows), "bit for bit");
        // Group 170 starts texture row 1 at x 0: no group straddles a row.
        assert_eq!(t.data[(1020 * 4)..][..4], [170.0, -170.0, 0.5, 0.0]);

        let moved = [row(1000), row(1001)];
        assert_eq!(write_group_rows(&mut t, 168, &moved), Some(TexelRect { x: 1008, y: 0, width: 12, height: 1 }));
        assert_eq!(write_group_rows(&mut t, 169, &moved), Some(TexelRect { x: 0, y: 0, width: 1020, height: 2 }));
        let back: &[GroupRow] = bytemuck::cast_slice(&t.data[168 * 24..171 * 24]);
        let expected = [row(1000), row(1000), row(1001)];
        assert_eq!(bytemuck::cast_slice::<_, u8>(back), bytemuck::cast_slice::<_, u8>(&expected));
        assert_eq!(t.data[171 * 24], 171.0, "the row after the write is untouched");
        assert_eq!(write_group_rows(&mut t, 5, &[]), None);

        assert_eq!(group_rows_rect(0, 1), Some(TexelRect { x: 0, y: 0, width: 6, height: 1 }));
        assert_eq!(group_rows_rect(3, 5), Some(TexelRect { x: 18, y: 0, width: 12, height: 1 }));
        assert_eq!(group_rows_rect(169, 170), Some(TexelRect { x: 1014, y: 0, width: 6, height: 1 }));
        assert_eq!(group_rows_rect(170, 171), Some(TexelRect { x: 0, y: 1, width: 6, height: 1 }));
        assert_eq!(group_rows_rect(169, 171), Some(TexelRect { x: 0, y: 0, width: 1020, height: 2 }));
        assert_eq!(group_rows_rect(10, 400), Some(TexelRect { x: 0, y: 0, width: 1020, height: 3 }));
        assert_eq!(group_rows_rect(7, 7), None);
        assert_eq!(group_texels(&[]).height, 1);
    }

    #[test]
    #[should_panic(expected = "rows 509..511 past its 510 rows")]
    fn a_group_write_past_the_texture_is_refused() {
        let mut t = group_texels(&[GroupRow::identity([0.0; 3]); 400]);
        write_group_rows(&mut t, 509, &[GroupRow::identity([0.0; 3]); 2]);
    }

    /// The first integer past the rule, in each kind of lane, is refused by
    /// name — 2^24 itself, the one an off-by-one bound would let through.
    #[test]
    fn the_first_inexact_integer_is_refused() {
        let refused = |f: &dyn Fn()| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .expect_err("converted an integer at 2^24")
                .downcast::<String>()
                .map(|s| *s)
                .unwrap_or_default()
        };
        let mut gm = vec![0u32; 1024 * 4];
        gm[41 * 4 + 1] = EXACT_INT_LIMIT;
        assert_eq!(
            refused(&|| drop(glyphmap_texels(1024, 1, &gm))),
            "glyphmap: slot 41 curve_count = 16777216 (0x01000000) is not below 2^24, so it has no exact f32"
        );
        let mut cv = vec![0u32; 1024 * 4];
        cv[9 * 4 + 2] = EXACT_INT_LIMIT;
        assert!(refused(&|| drop(curve_texels(1024, 1, &cv))).starts_with("curves: texel 9 z = 16777216"));
        assert!(refused(&|| drop(override_texels(&[0, 3, EXACT_INT_LIMIT]))).starts_with("group_overrides: override 2 group = 16777216"));
        let item = |p: ItemParamsGpu| move || drop(item_texels(&[ItemParamsGpu::default(), p]));
        let limit = EXACT_INT_LIMIT as i32;
        assert!(refused(&item(ItemParamsGpu { page_rows: limit, ..Default::default() })).starts_with("item_table: item 1 page_rows = 16777216"));
        assert!(refused(&item(ItemParamsGpu { scroll_rows: -limit, ..Default::default() })).starts_with("item_table: item 1 scroll_rows = -16777216"));
        assert!(refused(&item(ItemParamsGpu { group: EXACT_INT_LIMIT, ..Default::default() })).starts_with("item_table: item 1 group = 16777216"));
        assert!(refused(&item(ItemParamsGpu { page_cols: i32::MIN, ..Default::default() })).starts_with("item_table: item 1 page_cols"));
    }

    /// NO_CELL is a sentinel in the emoji_cell lane only; anywhere else it is
    /// an integer past 2^24.
    #[test]
    #[should_panic(expected = "glyphmap: slot 0 curve_start = 4294967295 (0xffffffff)")]
    fn no_cell_outside_the_emoji_lane_is_refused() {
        let mut gm = vec![0u32; 1024 * 4];
        gm[0] = NO_CELL;
        glyphmap_texels(1024, 1, &gm);
    }
}
