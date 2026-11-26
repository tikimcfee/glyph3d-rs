//! Shared constants and packing helpers matching `assets/atlas/FORMAT.md` and
//! `crates/glyph-field-derived/shaders/glyph_field_derived.wgsl`.

/// Width in texels of both Slug RGBA32Uint textures (`curves` and `glyphmap`).
/// Texel `i` sits at `(i % TEXTURE_WIDTH, i / TEXTURE_WIDTH)`.
pub const TEXTURE_WIDTH: u32 = 1024;

/// Number of RGBA32Uint texels consumed by one quadratic Bézier curve:
/// - texel `2*c`: `[P0.x, P0.y, P1.x, P1.y]`
/// - texel `2*c + 1`: `[P2.x, P2.y, 0, 0]`
pub const TEXELS_PER_CURVE: u32 = 2;

/// Number of `u32` channels per RGBA32Uint texel.
pub const U32S_PER_TEXEL: u32 = 4;

/// Number of `u32` values one curve occupies in the curve texture backing array.
pub const U32S_PER_CURVE: u32 = TEXELS_PER_CURVE * U32S_PER_TEXEL;

/// Default initial row capacity of the curve texture (64 rows = 32,768 curves).
pub const CURVE_INITIAL_ROWS: u32 = 64;

/// Default initial row capacity of the glyph-map texture (4 rows = 4,096 slots).
pub const MAP_INITIAL_ROWS: u32 = 4;

/// Reserved blank slot (uncovered codepoints). Its glyph-map entry is `[0, 0, 0, 0]`.
pub const BLANK_SLOT: u32 = 0;

/// Glyph-map mode lane for a Slug quadratic-curve font outline.
pub const GLYPH_MODE_OUTLINE: u32 = 0;

/// Glyph-map mode lane for a colour-emoji bitmap slot (arg = emoji sheet cell).
pub const GLYPH_MODE_BITMAP: u32 = 1;

/// Glyph-map mode lane for a cell-generated sprite outline (arg = ink alpha 0..=255).
pub const GLYPH_MODE_SPRITE: u32 = 3;

/// Pack a `[0, 1]` normalised coordinate into a `u16` (stored in a `u32` channel).
///
/// Matches `packUint16` in `slug-constants.js`: `Math.round(Math.max(0, Math.min(1, v)) * 65535)`.
/// For non-negative finite values, Rust's `f64::round` matches JS `Math.round` (ties away from zero).
#[inline]
pub fn pack_u16(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= 1.0 {
        return 65535;
    }
    (value * 65535.0).round() as u32
}

/// Unpack a packed `u16` channel back to `[0, 1]`.
#[inline]
pub fn unpack_u16(bits: u32) -> f32 {
    bits as f32 / 65535.0
}
