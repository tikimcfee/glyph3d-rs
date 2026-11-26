//! Runtime font chain and growable Slug atlas.
//!
//! Combines `FontChain` routing (families, bold/italic/bold-italic faces, variable-font
//! instances, shared fallback fonts, cell-generated terminal sprites, combining-mark composite
//! clusters, and optional colour-emoji bitmaps) with on-demand Slug curve encoding into a
//! growable [`SlugBuffer`].
//!
//! A [`Slot`] is a plain `u32` (slot 0 = blank). Cell sprites route before any font into
//! glyph-map mode `3` (`GLYPH_MODE_SPRITE`). Colour emoji routed through an attached
//! [`EmojiSource`] write `[0, 0, GLYPH_MODE_BITMAP (1), emoji_cell]` — the exact format
//! `assets/atlas/glyphmap.bin` and `wall_cells.wgsl` expect.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use icu_normalizer::{ComposingNormalizer, DecomposingNormalizer};
use term_cells::{attrs, Cell, Screen, CLUSTER_BASE};
use ttf_parser::{Face as TtfFace, GlyphId, Rect, Tag};

use crate::buffer::{DirtyUpdate, SlugBuffer, TextureBacking};
use crate::constants::{BLANK_SLOT, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE};
use crate::emoji::EmojiSource;
use crate::encode::{
    encode_composite_part_curves, encode_glyph, extract_raw_curves, CompositePart, Curve,
    PackedCurve,
};
use crate::families::{FaceSpec, FontSource, FontSpec, Variation};
use crate::metrics::{FamilyMetrics, FontExtents};
use crate::sprites::{encode_sprite, is_sprite_codepoint, SpriteMetrics};

/// Vertical gap between a base glyph's ink and a mark placed above or below it, in ems
/// (matches `MARK_GAP = 1 / 16` in `FontChain.js` and HarfBuzz's fallback mark positioning).
const MARK_GAP_EM: f64 = 1.0 / 16.0;

/// Dense atlas slot id. Slot `0` ([`BLANK_SLOT`]) is reserved for the blank/missing cell.
pub type Slot = u32;

/// Handle identifying a font family within a [`FontAtlas`].
/// `FamilyId(0)` is always the default family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct FamilyId(pub usize);

/// Which of a family's four faces draws a glyph.
///
/// Bit layout matches `term_cells::Face` (bit 0 = bold, bit 1 = italic).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Face {
    #[default]
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

impl Face {
    /// Convert the 2-bit face index (`0..=3`: bit 0 bold, bit 1 italic) into a [`Face`].
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 3 {
            0 => Self::Regular,
            1 => Self::Bold,
            2 => Self::Italic,
            _ => Self::BoldItalic,
        }
    }

    /// Array index `0..4`.
    #[inline]
    pub const fn as_index(self) -> usize {
        self as usize
    }
}

/// Result of resolving a [`term_cells::Cell`] for the cell renderer ([`FontAtlas::resolve_cell`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedCell {
    /// Atlas slot id (`BLANK_SLOT` / `0` for blank, space, uncovered, or trailing wide cells).
    pub slot: Slot,
    /// Terminal columns spanned by this cell (`0` = trailing half of a wide character,
    /// `1` = normal cell, `2` = wide character / wide emoji).
    pub span_cols: u8,
}

/// Errors produced when loading or configuring a [`FontAtlas`].
#[derive(Debug)]
pub enum FontError {
    /// No fonts were provided to [`FontAtlas::load`].
    EmptyChain,
    /// Duplicate family identifier in the chain.
    DuplicateFamily(String),
    /// Requested font family is neither bundled nor installed on the system.
    FamilyNotFound(String),
    /// Failed to read a font file from disk.
    Io { name: String, source: std::io::Error },
    /// Failed to parse font bytes with `ttf-parser`.
    Parse { name: String, error: ttf_parser::FaceParsingError },
    /// A face specified neither a file/bytes source nor variation axes.
    EmptyFaceSpec { name: String },
    /// Requested variation axis does not exist on the font (or the font is not variable).
    UnknownAxis { name: String, tag: String, available: Vec<String> },
    /// Requested variation value lies outside the axis's `[min, max]` range or is not finite.
    AxisOutOfRange { name: String, tag: String, value: f32, min: f32, max: f32 },
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyChain => write!(f, "font chain has no fonts"),
            Self::DuplicateFamily(id) => {
                write!(f, "duplicate font family '{id}' (a family has one primary font)")
            }
            Self::FamilyNotFound(name) => {
                write!(f, "font family '{name}' is not installed")
            }
            Self::Io { name, source } => write!(f, "failed to read font '{name}': {source}"),
            Self::Parse { name, error } => write!(f, "failed to parse font '{name}': {error}"),
            Self::EmptyFaceSpec { name } => {
                write!(f, "face '{name}' must provide a font source or variation axes")
            }
            Self::UnknownAxis { name, tag, available } => {
                if available.is_empty() {
                    write!(f, "font '{name}' has no '{tag}' axis (not a variable font)")
                } else {
                    write!(
                        f,
                        "font '{name}' has no '{tag}' axis (available: {})",
                        available.join(", ")
                    )
                }
            }
            Self::AxisOutOfRange { name, tag, value, min, max } => {
                write!(f, "font '{name}': {tag} = {value} is outside axis range [{min}, {max}]")
            }
        }
    }
}

impl std::error::Error for FontError {}

/// Provenance of a font-encoded slot (for diagnostics and tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotInfo {
    /// Index into the loaded font list (listed fonts first, then face fonts).
    pub font_index: usize,
    /// Human-readable font name (e.g. `"Monaspace Neon Bold"`).
    pub font_name: String,
    /// Per-font glyph index (`gid`).
    pub glyph_id: u16,
    /// Glyph name from the font's `post`/CFF table, if present.
    pub glyph_name: Option<String>,
}

/// Classification of any allocated [`Slot`] in a [`FontAtlas`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotKind<'a> {
    /// Slot 0 (blank / missing cell).
    Blank,
    /// Single font glyph outline (mode 0).
    Font(SlotInfo),
    /// Colour-emoji bitmap slot (mode 1) referencing a cell in the host's emoji sheet.
    Bitmap { cell: u32 },
    /// Generated cell sprite (mode 3), owned by `family`.
    Sprite { family: FamilyId, codepoint: u32 },
    /// Composite cluster outline (mode 0) built from a base and placed combining marks.
    Composite { parts: &'a [CompositePart] },
    /// Slot allocated by an external source via [`FontAtlas::allocate_slot`].
    External,
}

struct LoadedFont {
    name: String,
    bytes: Arc<[u8]>,
    face_index: u32,
    variations: Vec<Variation>,
    upem: u16,
    extents: FontExtents,
    /// Unicode scalar -> non-zero glyph id. Shared via `Arc` across variable instances of one file.
    cmap: Arc<HashMap<u32, u16>>,
    /// Index of the font whose bytes this one shares (`None` if it owns its own file).
    instance_of: Option<usize>,
    /// Face font indices `[regular, bold, italic, bold_italic]` (`None` when absent).
    faces: [Option<usize>; 4],
}

struct Family {
    id: Option<String>,
    /// Ordered indices into `FontAtlas::fonts`: primary first, then shared fallbacks.
    route: Vec<usize>,
    metrics: FamilyMetrics,
}

/// Runtime font chain and growable Slug atlas.
pub struct FontAtlas {
    fonts: Vec<LoadedFont>,
    families: Vec<Family>,
    buffer: SlugBuffer,
    next_slot: Slot,
    /// Optional colour-emoji bitmap sheet index / callback.
    emoji_source: Option<Arc<dyn EmojiSource>>,
    /// `(font_index, gid) -> Slot` for font-backed outline slots.
    slot_by_glyph: HashMap<(usize, u16), Slot>,
    /// `(family_idx, codepoint) -> Slot` for generated cell sprite slots (mode 3).
    sprite_by_key: HashMap<(usize, u32), Slot>,
    /// `emoji_cell -> Slot` deduplication for colour-emoji bitmap slots (mode 1).
    bitmap_by_cell: HashMap<u32, Slot>,
    /// `parts -> Slot` deduplication for composite clusters.
    composite_by_parts: HashMap<Vec<CompositePart>, Slot>,
    /// `(family_idx, face_bits, codepoint) -> Slot` fast cache (including `BLANK_SLOT` misses).
    lookup_cache: HashMap<(usize, u8, u32), Slot>,
    /// `(family_idx, face_bits, cluster_text) -> Slot` fast cache for multi-codepoint clusters.
    cluster_cache: HashMap<(usize, u8, String), Slot>,
    /// Per-slot provenance tables.
    slot_sources: HashMap<Slot, (usize, u16)>,
    sprite_sources: HashMap<Slot, (usize, u32)>,
    bitmap_sources: HashMap<Slot, u32>,
    composite_sources: HashMap<Slot, Vec<CompositePart>>,
}

impl FontAtlas {
    /// Load a font chain with default texture capacities (`CURVE_INITIAL_ROWS`, `MAP_INITIAL_ROWS`).
    pub fn load(specs: &[FontSpec]) -> Result<Self, FontError> {
        Self::load_with_initial_rows(
            specs,
            crate::constants::CURVE_INITIAL_ROWS,
            crate::constants::MAP_INITIAL_ROWS,
        )
    }

    /// Load a font chain with explicit initial curve/glyphmap row capacities.
    pub fn load_with_initial_rows(
        specs: &[FontSpec],
        curve_rows: u32,
        map_rows: u32,
    ) -> Result<Self, FontError> {
        if specs.is_empty() {
            return Err(FontError::EmptyChain);
        }

        let mut fonts: Vec<LoadedFont> = Vec::with_capacity(specs.len() * 2);

        // Pass 1: load every listed font so its index equals its position in `specs`.
        for spec in specs {
            let bytes = load_source(&spec.name, &spec.source)?;
            let mut loaded = build_loaded_font(
                spec.name.clone(),
                bytes,
                spec.face_index,
                spec.variations.clone(),
                spec.include_line_gap,
                spec.line_height_scale,
                None,
                None,
            )?;
            let idx = fonts.len();
            loaded.faces[0] = Some(idx);
            fonts.push(loaded);
        }

        // Pass 2: load each listed font's bold / italic / bold-italic faces after all listed fonts.
        const FACE_SUFFIXES: [(Face, &str); 3] = [
            (Face::Bold, " Bold"),
            (Face::Italic, " Italic"),
            (Face::BoldItalic, " Bold Italic"),
        ];
        for (upright_idx, spec) in specs.iter().enumerate() {
            let face_specs = [&spec.bold, &spec.italic, &spec.bold_italic];
            for ((face_kind, suffix), maybe_face) in FACE_SUFFIXES.iter().zip(face_specs) {
                let Some(face_spec) = maybe_face else {
                    continue;
                };
                let face_name = format!("{}{}", spec.name, suffix);
                let mut loaded = load_face_spec(
                    &face_name,
                    face_spec,
                    spec.include_line_gap,
                    spec.line_height_scale,
                    upright_idx,
                    &fonts[upright_idx],
                )?;
                let idx = fonts.len();
                loaded.faces[0] = Some(idx);
                fonts.push(loaded);
                fonts[upright_idx].faces[face_kind.as_index()] = Some(idx);
            }
        }

        // Build families: default (listed font 0), then every other listed font that names a family.
        let fallbacks: Vec<usize> = specs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.family.is_none().then_some(i))
            .collect();

        let make_family =
            |primary: usize, id: Option<String>, fonts: &[LoadedFont]| -> Result<Family, FontError> {
                let mut route = Vec::with_capacity(1 + fallbacks.len());
                route.push(primary);
                for &fb in &fallbacks {
                    if fb != primary {
                        route.push(fb);
                    }
                }
                let font = &fonts[primary];
                let face = parse_and_vary(&font.name, &font.bytes, font.face_index, &font.variations)?;
                let include_gap = specs[primary].include_line_gap;
                let line_height_scale = specs[primary].line_height_scale;
                let metrics = FamilyMetrics::from_face_scaled(&face, include_gap, line_height_scale);
                Ok(Family {
                    id,
                    route,
                    metrics,
                })
            };

        let mut families = Vec::new();
        families.push(make_family(0, specs[0].family.clone(), &fonts)?);
        for (i, spec) in specs.iter().enumerate().skip(1) {
            let Some(ref id) = spec.family else {
                continue;
            };
            if families.iter().any(|f| f.id.as_deref() == Some(id.as_str())) {
                return Err(FontError::DuplicateFamily(id.clone()));
            }
            families.push(make_family(i, Some(id.clone()), &fonts)?);
        }

        Ok(Self {
            fonts,
            families,
            buffer: SlugBuffer::with_initial_rows(curve_rows, map_rows),
            next_slot: 1,
            emoji_source: None,
            slot_by_glyph: HashMap::new(),
            sprite_by_key: HashMap::new(),
            bitmap_by_cell: HashMap::new(),
            composite_by_parts: HashMap::new(),
            lookup_cache: HashMap::new(),
            cluster_cache: HashMap::new(),
            slot_sources: HashMap::new(),
            sprite_sources: HashMap::new(),
            bitmap_sources: HashMap::new(),
            composite_sources: HashMap::new(),
        })
    }

    /// Attach a colour-emoji bitmap source (builder style).
    pub fn with_emoji_source(mut self, source: impl EmojiSource + 'static) -> Self {
        self.set_emoji_source(Some(Arc::new(source)));
        self
    }

    /// Replace or clear the attached colour-emoji bitmap source.
    ///
    /// Clears the per-codepoint/cluster routing caches so subsequent lookups reflect the new
    /// source; already-allocated slots stay valid.
    pub fn set_emoji_source(&mut self, source: Option<Arc<dyn EmojiSource>>) {
        self.emoji_source = source;
        self.lookup_cache.clear();
        self.cluster_cache.clear();
    }

    /// Look up a family handle by its string id (e.g. `"cousine"`, `"monaspace-neon"`, `"monaspace-argon"`).
    pub fn family_id(&self, id: &str) -> Option<FamilyId> {
        self.families
            .iter()
            .position(|f| f.id.as_deref() == Some(id))
            .map(FamilyId)
    }

    /// The default family handle (`FamilyId(0)`).
    #[inline]
    pub const fn default_family(&self) -> FamilyId {
        FamilyId(0)
    }

    /// Number of families registered in this atlas.
    #[inline]
    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    /// Registered family ids in order (`None` for an unnamed default chain).
    pub fn family_ids(&self) -> Vec<Option<&str>> {
        self.families.iter().map(|f| f.id.as_deref()).collect()
    }

    /// Number of loaded fonts (listed fonts plus their faces).
    #[inline]
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    /// Cell, line, and sprite metrics for `family`. Panics if `family` is out of range.
    pub fn family_metrics(&self, family: FamilyId) -> FamilyMetrics {
        self.families[family.0].metrics
    }

    /// Sprite geometry (cell aspect and light/heavy box-drawing weights) for `family`.
    pub fn sprite_metrics(&self, family: FamilyId) -> SpriteMetrics {
        self.families[family.0].metrics.sprites
    }

    /// Single entry point for a terminal cell renderer (replaces `native/src/wall/cell.rs::resolve_cell_glyph`).
    ///
    /// Given `family`, `screen`, and `cell`:
    /// - Trailing halves of wide characters (`cell.width == 0`) return `(BLANK_SLOT, 0)`.
    /// - Empty cells (`content == 0`) and ASCII spaces (`0x20`) return `(BLANK_SLOT, span_cols)`
    ///   so the shader skips the glyph-map texture load while still drawing style lines.
    /// - Extracts bold/italic [`Face`] from `cell.attrs` (`term_cells::attrs::face`).
    /// - Resolves scalars through [`Self::glyph_u32`] and clusters (`>= CLUSTER_BASE`) through
    ///   [`Self::cluster`], covering sprites, outline glyphs, composites, and colour emoji.
    pub fn resolve_cell(&mut self, family: FamilyId, screen: &Screen, cell: &Cell) -> ResolvedCell {
        let span_cols = cell.width.min(2);
        if span_cols == 0 || cell.content == 0 || cell.content == 0x20 {
            return ResolvedCell {
                slot: BLANK_SLOT,
                span_cols,
            };
        }

        let face = Face::from_bits(attrs::face(cell.attrs));
        let slot = if cell.content < CLUSTER_BASE {
            self.glyph_u32(family, face, cell.content)
        } else {
            let idx = (cell.content - CLUSTER_BASE) as usize;
            let text = screen.clusters.get(idx).map(String::as_str).unwrap_or("");
            if text.is_empty() || text == " " {
                BLANK_SLOT
            } else {
                // Fast-path cache probe without allocating a String on hit.
                let fam_idx = family.0;
                if fam_idx < self.families.len() {
                    self.cluster(family, face, text)
                } else {
                    BLANK_SLOT
                }
            }
        };

        ResolvedCell { slot, span_cols }
    }

    /// Look up (and encode on first use) the atlas [`Slot`] for `(family, face, codepoint)`.
    ///
    /// Precedence (matching `FontChain.js` and the baked atlas's BMP text-vs-emoji split):
    /// 1. **Cell sprites** (`is_sprite_codepoint`) route to a per-family mode-3 sprite slot
    ///    BEFORE any font or emoji source (ignoring `face`).
    /// 2. **Astral emoji planes (`cp >= 0x1F000`)** prefer colour emoji when an [`EmojiSource`]
    ///    is attached and has a cell (mode-1 bitmap slot `[0, 0, 1, cell]`), falling back to
    ///    outline fonts if the sheet has no cell.
    /// 3. **BMP codepoints (`cp < 0x1F000`)** prefer outline fonts first (so digits, `#`, `*`,
    ///    `©`, `®`, `❤`, `✓`, `★`, `☃` draw as crisp monochrome outlines in the terminal's fg
    ///    colour), routing to a mode-1 colour-emoji bitmap slot only when no outline font in
    ///    `family`'s route covers `cp`.
    /// 4. Returns [`BLANK_SLOT`] (`0`) when nothing covers `cp`.
    pub fn glyph(&mut self, family: FamilyId, face: Face, codepoint: char) -> Slot {
        self.glyph_u32(family, face, codepoint as u32)
    }

    /// Same as [`Self::glyph`], taking a Unicode scalar value as `u32`.
    pub fn glyph_u32(&mut self, family: FamilyId, face: Face, codepoint: u32) -> Slot {
        let fam_idx = family.0;
        let key = (fam_idx, face as u8, codepoint);
        if let Some(&slot) = self.lookup_cache.get(&key) {
            return slot;
        }

        let slot = if is_sprite_codepoint(codepoint) {
            self.ensure_sprite(fam_idx, codepoint)
        } else {
            let prefer_color = self.emoji_source.is_some() && codepoint >= 0x1F000;
            let mut resolved = BLANK_SLOT;
            if prefer_color {
                if let Some(cell) = self.emoji_cell_for_cp(codepoint) {
                    resolved = self.ensure_bitmap_slot(cell);
                }
            }
            if resolved == BLANK_SLOT {
                if let Some((font_idx, gid)) = self.resolve_font_glyph(fam_idx, face, codepoint) {
                    resolved = self.ensure_font_glyph(font_idx, gid);
                }
            }
            if resolved == BLANK_SLOT && !prefer_color {
                if let Some(cell) = self.emoji_cell_for_cp(codepoint) {
                    resolved = self.ensure_bitmap_slot(cell);
                }
            }
            resolved
        };
        self.lookup_cache.insert(key, slot);
        slot
    }

    /// Look up by family name string (convenience wrapper over [`Self::family_id`] + [`Self::glyph`]).
    /// Returns [`BLANK_SLOT`] if the family name is unknown.
    pub fn glyph_by_family_name(&mut self, family: &str, face: Face, codepoint: char) -> Slot {
        match self.family_id(family) {
            Some(id) => self.glyph(id, face, codepoint),
            None => BLANK_SLOT,
        }
    }

    /// Look up (and encode on first use) a grapheme cluster — a base and its combining marks
    /// (e.g. `"e\u{0301}"`, `"x\u{0301}"`, `"q\u{0308}\u{0304}"`) or an emoji / variation sequence
    /// (e.g. `"👨‍👩‍👧"`, `"🇺🇸"`, `"👍🏽"`, `"1️⃣"`, `"❤️"`, `"❤\u{FE0E}"`).
    ///
    /// Precedence:
    /// 1. If `text` has a single scalar (no selectors/marks), delegates directly to [`Self::glyph`].
    /// 2. If `text` does NOT request explicit text presentation (`U+FE0E`) and an [`EmojiSource`]
    ///    is attached, queries the emoji source first — so emoji presentation sequences (`U+FE0F`,
    ///    ZWJ families, flags, skin-tone modifiers, keycaps) route to a mode-1 bitmap slot
    ///    `[0, 0, 1, cell]`.
    /// 3. Otherwise strips variation selectors (`U+FE00..=FE0F`, `U+E0100..=E01EF`) and joiners
    ///    (`U+200C`, `U+200D`), NFC-normalises via `icu_normalizer`, and returns the precomposed
    ///    glyph in `face` when covered (or decomposes via NFD and builds an ink-placed composite
    ///    slot matching `FontChain.lookupCluster` and `slugData.encodeComposite`).
    pub fn cluster(&mut self, family: FamilyId, face: Face, text: &str) -> Slot {
        let fam_idx = family.0;
        if fam_idx >= self.families.len() || text.is_empty() {
            return BLANK_SLOT;
        }
        // Single-scalar fast path without allocating a String key for cluster_cache.
        let mut iter = text.chars();
        if let Some(first) = iter.next() {
            if iter.next().is_none() {
                return self.glyph(family, face, first);
            }
        }

        let cache_key = (fam_idx, face as u8, text.to_owned());
        if let Some(&slot) = self.cluster_cache.get(&cache_key) {
            return slot;
        }

        let slot = self.resolve_cluster_uncached(family, face, text);
        self.cluster_cache.insert(cache_key, slot);
        slot
    }

    fn resolve_cluster_uncached(&mut self, family: FamilyId, face: Face, text: &str) -> Slot {
        let fam_idx = family.0;

        // Explicit text presentation selector (VS15 U+FE0E) forces monochrome outline rendering.
        let force_text = text.contains('\u{FE0E}');
        if !force_text {
            if let Some(cell) = self.emoji_cell_for_str(text) {
                return self.ensure_bitmap_slot(cell);
            }
        }

        let stripped: String = text
            .chars()
            .filter(|&ch| !is_ignorable_cluster_char(ch))
            .collect();
        if stripped.is_empty() {
            return BLANK_SLOT;
        }

        let nfc = ComposingNormalizer::new_nfc().normalize(&stripped);
        let mut working = nfc.into_owned();
        let mut chars = working.chars();
        let Some(mut base) = chars.next() else {
            return BLANK_SLOT;
        };

        if chars.next().is_none() {
            let cp = base as u32;
            if force_text {
                return self.glyph_outline_or_sprite(fam_idx, face, cp);
            }
            if is_sprite_codepoint(cp) || self.route_upright(fam_idx, cp).is_some() {
                return self.glyph(family, face, base);
            }
            // No font has the precomposed scalar; try NFD so it can still compose from parts.
            let nfd = DecomposingNormalizer::new_nfd().normalize(&working);
            if nfd.as_ref() == working.as_str() {
                return self.glyph(family, face, base);
            }
            working = nfd.into_owned();
            let Some(new_base) = working.chars().next() else {
                return BLANK_SLOT;
            };
            base = new_base;
        }

        let base_cp = base as u32;
        if is_sprite_codepoint(base_cp) || (!force_text && base_cp >= 0x1F000) {
            return self.glyph(family, face, base);
        }
        let Some(upright_idx) = self.route_upright(fam_idx, base_cp) else {
            return if force_text {
                BLANK_SLOT
            } else {
                self.glyph(family, face, base)
            };
        };

        // Choose the face font for the base if available and covering `base`.
        let base_font_idx = if face != Face::Regular {
            self.face_font(upright_idx, face)
                .filter(|&fi| self.fonts[fi].cmap.contains_key(&base_cp))
                .unwrap_or(upright_idx)
        } else {
            upright_idx
        };
        let Some(&base_gid) = self.fonts[base_font_idx].cmap.get(&base_cp) else {
            return self.glyph(family, face, base);
        };

        let base_face_parsed = parse_and_vary(
            &self.fonts[base_font_idx].name,
            &self.fonts[base_font_idx].bytes,
            self.fonts[base_font_idx].face_index,
            &self.fonts[base_font_idx].variations,
        )
        .expect("already-validated font bytes must re-parse");
        let base_adv = base_face_parsed
            .glyph_hor_advance(GlyphId(base_gid))
            .map(|a| a as i32)
            .filter(|&a| a > 0)
            .unwrap_or(self.families[fam_idx].metrics.advance_fu);
        let base_upem = self.fonts[base_font_idx].upem as f64;

        struct DraftPart {
            font_index: usize,
            glyph_id: u16,
            offset_x_fu: i32,
            offset_y_fu: i32,
            units_per_cell: i32,
            raw_curves: Vec<Curve>,
            ink_rect: Option<Rect>,
        }

        let (base_curves, base_rect, _) = extract_raw_curves(
            &base_face_parsed,
            GlyphId(base_gid),
            self.fonts[base_font_idx].extents,
        );
        let mut drafts = vec![DraftPart {
            font_index: base_font_idx,
            glyph_id: base_gid,
            offset_x_fu: 0,
            offset_y_fu: 0,
            units_per_cell: base_adv,
            raw_curves: base_curves,
            ink_rect: base_rect,
        }];

        for mark_ch in working.chars().skip(1) {
            let mark_cp = mark_ch as u32;
            let resolved_mark = if let Some(&gid) = self.fonts[base_font_idx].cmap.get(&mark_cp) {
                Some((base_font_idx, gid, base_adv))
            } else if let Some(other_idx) = self.route_upright(fam_idx, mark_cp) {
                if other_idx != upright_idx {
                    self.fonts[other_idx].cmap.get(&mark_cp).map(|&gid| {
                        let other_upem = self.fonts[other_idx].upem as f64;
                        let units = ((base_adv as f64) * other_upem / base_upem).round() as i32;
                        (other_idx, gid, units.max(1))
                    })
                } else {
                    None
                }
            } else {
                None
            };

            let Some((m_font_idx, m_gid, m_units)) = resolved_mark else {
                continue; // uncovered mark is dropped; base still draws
            };

            let (m_curves, m_rect, _) = if m_font_idx == base_font_idx {
                extract_raw_curves(
                    &base_face_parsed,
                    GlyphId(m_gid),
                    self.fonts[m_font_idx].extents,
                )
            } else {
                let mf = parse_and_vary(
                    &self.fonts[m_font_idx].name,
                    &self.fonts[m_font_idx].bytes,
                    self.fonts[m_font_idx].face_index,
                    &self.fonts[m_font_idx].variations,
                )
                .expect("already-validated font bytes must re-parse");
                extract_raw_curves(&mf, GlyphId(m_gid), self.fonts[m_font_idx].extents)
            };

            drafts.push(DraftPart {
                font_index: m_font_idx,
                glyph_id: m_gid,
                offset_x_fu: 0,
                offset_y_fu: 0,
                units_per_cell: m_units,
                raw_curves: m_curves,
                ink_rect: m_rect,
            });
        }

        // Only the base survived: share the lone base glyph's slot.
        if drafts.len() == 1 {
            return self.ensure_font_glyph(base_font_idx, base_gid);
        }

        // Place marks relative to the base's ink (FontChain._placeMarks).
        #[derive(Clone, Copy)]
        struct CellInk {
            x0: f64,
            x1: f64,
            y0: f64,
            y1: f64,
            em: f64,
            baseline: f64,
            gap: f64,
        }

        let to_cell_ink = |d: &DraftPart, fonts: &[LoadedFont]| -> Option<CellInk> {
            let rect = d.ink_rect?;
            if rect.x_max <= rect.x_min || rect.y_max <= rect.y_min || d.units_per_cell <= 0 {
                return None;
            }
            let font = &fonts[d.font_index];
            let ext = font.extents;
            let em = ext.em_height();
            if em <= 0.0 {
                return None;
            }
            let units = d.units_per_cell as f64;
            Some(CellInk {
                x0: (d.offset_x_fu as f64 + rect.x_min as f64) / units,
                x1: (d.offset_x_fu as f64 + rect.x_max as f64) / units,
                y0: (d.offset_y_fu as f64 + rect.y_min as f64 - ext.descender) / em,
                y1: (d.offset_y_fu as f64 + rect.y_max as f64 - ext.descender) / em,
                em,
                baseline: -ext.descender / em,
                gap: MARK_GAP_EM * (font.upem as f64) / em,
            })
        };

        let mut acc_box = to_cell_ink(&drafts[0], &self.fonts);
        let mid = acc_box.map_or(0.0, |b| (b.y0 + b.y1) * 0.5);

        for d in drafts.iter_mut().skip(1) {
            let Some(mut m) = to_cell_ink(d, &self.fonts) else {
                continue;
            };
            let base_center = acc_box.map_or(0.5, |b| (b.x0 + b.x1) * 0.5);
            let dx = base_center - (m.x0 + m.x1) * 0.5;
            let mut dy = 0.0_f64;
            if let Some(b) = acc_box {
                if m.y0 >= mid && m.y0 > m.baseline {
                    // Mark above: foot sits `gap` clear of what's under it (lower halfway if floating higher).
                    dy = b.y1 + m.gap - m.y0;
                    if dy < 0.0 {
                        dy *= 0.5;
                    }
                } else if m.y1 <= m.baseline + m.gap * 0.25 {
                    // Mark below: cedilla/ogonek touches; detached mark hangs `gap` below. Never raised.
                    let attached = m.y1 >= m.baseline - m.gap * 0.25;
                    dy = (b.y0 - if attached { 0.0 } else { m.gap } - m.y1).min(0.0);
                }
            }
            d.offset_x_fu += (dx * d.units_per_cell as f64).round() as i32;
            d.offset_y_fu += (dy * m.em).round() as i32;
            m.y0 += dy;
            m.y1 += dy;
            if let Some(ref mut b) = acc_box {
                b.y0 = b.y0.min(m.y0);
                b.y1 = b.y1.max(m.y1);
            }
        }

        let parts: Vec<CompositePart> = drafts
            .iter()
            .map(|d| CompositePart {
                font_index: d.font_index,
                glyph_id: d.glyph_id,
                offset_x_fu: d.offset_x_fu,
                offset_y_fu: d.offset_y_fu,
                units_per_cell: d.units_per_cell,
            })
            .collect();

        if let Some(&existing) = self.composite_by_parts.get(&parts) {
            return existing;
        }

        let mut packed = Vec::new();
        for d in &drafts {
            encode_composite_part_curves(
                &d.raw_curves,
                d.offset_x_fu,
                d.offset_y_fu,
                d.units_per_cell,
                self.fonts[d.font_index].extents,
                &mut packed,
            );
        }

        let slot = self.allocate_slot();
        self.buffer
            .write_outline_slot(slot, &packed, GLYPH_MODE_OUTLINE, 0);
        self.composite_by_parts.insert(parts.clone(), slot);
        self.composite_sources.insert(slot, parts);
        slot
    }

    fn glyph_outline_or_sprite(&mut self, fam_idx: usize, face: Face, codepoint: u32) -> Slot {
        if is_sprite_codepoint(codepoint) {
            self.ensure_sprite(fam_idx, codepoint)
        } else {
            match self.resolve_font_glyph(fam_idx, face, codepoint) {
                Some((font_idx, gid)) => self.ensure_font_glyph(font_idx, gid),
                None => BLANK_SLOT,
            }
        }
    }

    fn emoji_cell_for_cp(&self, codepoint: u32) -> Option<u32> {
        let source = self.emoji_source.as_ref()?;
        let ch = char::from_u32(codepoint)?;
        let mut buf = [0u8; 4];
        source.cell_for(ch.encode_utf8(&mut buf))
    }

    fn emoji_cell_for_str(&self, text: &str) -> Option<u32> {
        self.emoji_source.as_ref()?.cell_for(text)
    }

    fn ensure_bitmap_slot(&mut self, cell: u32) -> Slot {
        if let Some(&slot) = self.bitmap_by_cell.get(&cell) {
            return slot;
        }
        let slot = self.allocate_slot();
        // Exact layout of assets/atlas/glyphmap.bin: [curveStart=0, curveCount=0, mode=1, emojiCell].
        self.buffer
            .write_map_entry(slot, [0, 0, GLYPH_MODE_BITMAP, cell]);
        self.bitmap_by_cell.insert(cell, slot);
        self.bitmap_sources.insert(slot, cell);
        slot
    }

    fn route_upright(&self, fam_idx: usize, codepoint: u32) -> Option<usize> {
        let family = self.families.get(fam_idx)?;
        family
            .route
            .iter()
            .copied()
            .find(|&fi| self.fonts[fi].cmap.contains_key(&codepoint))
    }

    /// Resolve `(family, face, codepoint)` to `(font_index, gid)` without encoding or allocating.
    fn resolve_font_glyph(&self, fam_idx: usize, face: Face, codepoint: u32) -> Option<(usize, u16)> {
        let upright_idx = self.route_upright(fam_idx, codepoint)?;
        let upright_gid = *self.fonts[upright_idx].cmap.get(&codepoint)?;

        if face != Face::Regular {
            if let Some(face_idx) = self.face_font(upright_idx, face) {
                if let Some(&face_gid) = self.fonts[face_idx].cmap.get(&codepoint) {
                    return Some((face_idx, face_gid));
                }
            }
        }
        Some((upright_idx, upright_gid))
    }

    fn face_font(&self, upright_idx: usize, face: Face) -> Option<usize> {
        let faces = self.fonts[upright_idx].faces;
        if let Some(idx) = faces[face.as_index()] {
            return Some(idx);
        }
        if face == Face::BoldItalic {
            return faces[Face::Italic.as_index()].or(faces[Face::Bold.as_index()]);
        }
        None
    }

    fn ensure_sprite(&mut self, fam_idx: usize, codepoint: u32) -> Slot {
        if let Some(&slot) = self.sprite_by_key.get(&(fam_idx, codepoint)) {
            return slot;
        }
        let metrics = self.families[fam_idx].metrics.sprites;
        let Some((curves, alpha)) = encode_sprite(codepoint, metrics) else {
            return BLANK_SLOT;
        };
        let slot = self.allocate_slot();
        self.buffer
            .write_outline_slot(slot, &curves, GLYPH_MODE_SPRITE, alpha as u32);
        self.sprite_by_key.insert((fam_idx, codepoint), slot);
        self.sprite_sources.insert(slot, (fam_idx, codepoint));
        slot
    }

    fn ensure_font_glyph(&mut self, font_idx: usize, gid: u16) -> Slot {
        if let Some(&slot) = self.slot_by_glyph.get(&(font_idx, gid)) {
            return slot;
        }
        let slot = self.allocate_slot();
        let font = &self.fonts[font_idx];
        let face = parse_and_vary(&font.name, &font.bytes, font.face_index, &font.variations)
            .expect("already-validated font bytes must re-parse");
        let encoded = encode_glyph(&face, GlyphId(gid), font.extents);
        self.buffer
            .write_outline_slot(slot, &encoded.curves, GLYPH_MODE_OUTLINE, 0);
        self.slot_by_glyph.insert((font_idx, gid), slot);
        self.slot_sources.insert(slot, (font_idx, gid));
        slot
    }

    // ── Extensibility for external glyph sources ────────────────────────────

    /// Allocate the next unused [`Slot`] id (`>= 1`).
    pub fn allocate_slot(&mut self) -> Slot {
        let slot = self.next_slot;
        self.next_slot += 1;
        slot
    }

    /// Write packed curves and a glyph-map entry `[curve_start, curves.len(), mode, arg]` for a
    /// slot previously obtained from [`Self::allocate_slot`].
    pub fn insert_outline_slot(
        &mut self,
        slot: Slot,
        curves: &[PackedCurve],
        mode: u32,
        arg: u32,
    ) {
        self.buffer.write_outline_slot(slot, curves, mode, arg);
    }

    /// Write a map-only entry `[curve_start, curve_count, mode, arg]` for a slot previously
    /// obtained from [`Self::allocate_slot`].
    pub fn insert_map_entry(&mut self, slot: Slot, entry: [u32; 4]) {
        self.buffer.write_map_entry(slot, entry);
    }

    // ── GPU texture backing & dirty ranges ──────────────────────────────────

    /// Backing RGBA32Uint array and dimensions for the Slug `curves` texture.
    pub fn curve_texture(&self) -> TextureBacking<'_> {
        self.buffer.curve_texture()
    }

    /// Backing RGBA32Uint array and dimensions for the Slug `glyphmap` texture.
    pub fn glyphmap_texture(&self) -> TextureBacking<'_> {
        self.buffer.glyphmap_texture()
    }

    /// Take the texel spans written since the last call (and whether either texture grew),
    /// resetting the dirty markers.
    pub fn take_dirty(&mut self) -> DirtyUpdate {
        self.buffer.take_dirty()
    }

    /// Total number of slots allocated so far (including slot 0, the blank slot).
    #[inline]
    pub fn slot_count(&self) -> u32 {
        self.next_slot
    }

    /// Total number of quadratic curves currently stored in the curve texture.
    #[inline]
    pub fn curve_count(&self) -> u32 {
        self.buffer.curve_count()
    }

    /// Read back a slot's glyph-map texel `[curve_start, curve_count, mode, arg]`.
    pub fn map_entry(&self, slot: Slot) -> [u32; 4] {
        self.buffer.map_entry(slot)
    }

    /// Read back a slot's packed curves from the curve texture.
    pub fn slot_curves(&self, slot: Slot) -> Vec<PackedCurve> {
        self.buffer.slot_curves(slot)
    }

    /// Returns true if `slot` is a generated cell sprite (mode 3).
    pub fn is_sprite_slot(&self, slot: Slot) -> bool {
        self.sprite_sources.contains_key(&slot)
    }

    /// Returns `(family, codepoint)` if `slot` is a generated cell sprite.
    pub fn sprite_info(&self, slot: Slot) -> Option<(FamilyId, u32)> {
        self.sprite_sources
            .get(&slot)
            .map(|&(fam, cp)| (FamilyId(fam), cp))
    }

    /// Returns true if `slot` is a colour-emoji bitmap slot (mode 1).
    pub fn is_bitmap_slot(&self, slot: Slot) -> bool {
        self.bitmap_sources.contains_key(&slot)
    }

    /// Returns the emoji-sheet cell index if `slot` is a colour-emoji bitmap slot.
    pub fn bitmap_cell(&self, slot: Slot) -> Option<u32> {
        self.bitmap_sources.get(&slot).copied()
    }

    /// Returns true if `slot` is a composite cluster slot (base + combining marks).
    pub fn is_composite_slot(&self, slot: Slot) -> bool {
        self.composite_sources.contains_key(&slot)
    }

    /// Returns the placed parts of a composite cluster slot, or `None` for other slots.
    pub fn composite_parts(&self, slot: Slot) -> Option<&[CompositePart]> {
        self.composite_sources.get(&slot).map(Vec::as_slice)
    }

    /// Classify any allocated slot (blank, font glyph, colour emoji bitmap, cell sprite,
    /// composite cluster, or external).
    pub fn slot_kind(&self, slot: Slot) -> SlotKind<'_> {
        if slot == BLANK_SLOT {
            return SlotKind::Blank;
        }
        if let Some(info) = self.describe_slot(slot) {
            return SlotKind::Font(info);
        }
        if let Some(&cell) = self.bitmap_sources.get(&slot) {
            return SlotKind::Bitmap { cell };
        }
        if let Some(&(fam, cp)) = self.sprite_sources.get(&slot) {
            return SlotKind::Sprite {
                family: FamilyId(fam),
                codepoint: cp,
            };
        }
        if let Some(parts) = self.composite_sources.get(&slot) {
            return SlotKind::Composite { parts };
        }
        SlotKind::External
    }

    /// Inspect which font and per-font glyph id produced `slot` (`None` for blank, bitmap,
    /// sprite, composite, or external slots).
    pub fn describe_slot(&self, slot: Slot) -> Option<SlotInfo> {
        let &(font_idx, gid) = self.slot_sources.get(&slot)?;
        let font = &self.fonts[font_idx];
        let glyph_name = TtfFace::parse(&font.bytes, font.face_index)
            .ok()
            .and_then(|f| f.glyph_name(GlyphId(gid)).map(str::to_owned));
        Some(SlotInfo {
            font_index: font_idx,
            font_name: font.name.clone(),
            glyph_id: gid,
            glyph_name,
        })
    }

    /// Human-readable name of a loaded font index (e.g. for describing composite parts).
    pub fn font_name(&self, font_index: usize) -> Option<&str> {
        self.fonts.get(font_index).map(|f| f.name.as_str())
    }

    /// Check whether two loaded font indices share the same underlying font byte buffer
    /// (true for variable-font instances of one file).
    pub fn shares_font_bytes(&self, a: usize, b: usize) -> bool {
        match (self.fonts.get(a), self.fonts.get(b)) {
            (Some(fa), Some(fb)) => Arc::ptr_eq(&fa.bytes, &fb.bytes),
            _ => false,
        }
    }

    /// For a loaded font index, return the font index whose file it is a variable instance of,
    /// or `None` if it owns its own file.
    pub fn instance_of(&self, font_index: usize) -> Option<usize> {
        self.fonts.get(font_index).and_then(|f| f.instance_of)
    }
}

/// Variation selectors and joiners ignored when normalizing a cluster (matches `IGNORABLE` in
/// `FontChain.js`).
#[inline]
fn is_ignorable_cluster_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x200C | 0x200D | 0xFE00..=0xFE0F | 0xE0100..=0xE01EF
    )
}

fn load_source(name: &str, source: &FontSource) -> Result<Arc<[u8]>, FontError> {
    match source {
        FontSource::Path(path) => {
            let bytes = std::fs::read(path).map_err(|source| FontError::Io {
                name: format!("{name} ({})", path.display()),
                source,
            })?;
            Ok(Arc::from(bytes))
        }
        FontSource::Bytes(bytes) => Ok(Arc::clone(bytes)),
    }
}

fn load_face_spec(
    name: &str,
    face_spec: &FaceSpec,
    include_line_gap: bool,
    line_height_scale: f64,
    upright_idx: usize,
    upright: &LoadedFont,
) -> Result<LoadedFont, FontError> {
    if let Some(ref source) = face_spec.source {
        let bytes = load_source(name, source)?;
        build_loaded_font(
            name.to_owned(),
            bytes,
            face_spec.face_index,
            face_spec.variations.clone(),
            include_line_gap,
            line_height_scale,
            None,
            None,
        )
    } else if !face_spec.variations.is_empty() {
        let base_idx = upright.instance_of.unwrap_or(upright_idx);
        build_loaded_font(
            name.to_owned(),
            Arc::clone(&upright.bytes),
            upright.face_index,
            face_spec.variations.clone(),
            include_line_gap,
            line_height_scale,
            Some(Arc::clone(&upright.cmap)),
            Some(base_idx),
        )
    } else {
        Err(FontError::EmptyFaceSpec {
            name: name.to_owned(),
        })
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper constructing one validated LoadedFont with face index and line-height scale"
)]
fn build_loaded_font(
    name: String,
    bytes: Arc<[u8]>,
    face_index: u32,
    variations: Vec<Variation>,
    include_line_gap: bool,
    line_height_scale: f64,
    shared_cmap: Option<Arc<HashMap<u32, u16>>>,
    instance_of: Option<usize>,
) -> Result<LoadedFont, FontError> {
    let face = parse_and_vary(&name, &bytes, face_index, &variations)?;
    let upem = face.units_per_em();
    let extents =
        FontExtents::from_face(&face, include_line_gap).with_line_height(line_height_scale);
    let cmap = match shared_cmap {
        Some(existing) => existing,
        None => Arc::new(collect_cmap(&face)),
    };
    Ok(LoadedFont {
        name,
        bytes,
        face_index,
        variations,
        upem,
        extents,
        cmap,
        instance_of,
        faces: [None; 4],
    })
}

fn parse_and_vary<'a>(
    name: &str,
    bytes: &'a [u8],
    face_index: u32,
    variations: &[Variation],
) -> Result<TtfFace<'a>, FontError> {
    let mut face = TtfFace::parse(bytes, face_index).map_err(|error| FontError::Parse {
        name: name.to_owned(),
        error,
    })?;
    if !variations.is_empty() {
        let axes: Vec<_> = face.variation_axes().into_iter().collect();
        for var in variations {
            let tag = Tag::from_bytes(&var.tag);
            let tag_str = String::from_utf8_lossy(&var.tag).into_owned();
            let axis = axes.iter().find(|a| a.tag == tag).ok_or_else(|| {
                FontError::UnknownAxis {
                    name: name.to_owned(),
                    tag: tag_str.clone(),
                    available: axes
                        .iter()
                        .map(|a| String::from_utf8_lossy(&a.tag.to_bytes()).into_owned())
                        .collect(),
                }
            })?;
            if !var.value.is_finite() || var.value < axis.min_value || var.value > axis.max_value {
                return Err(FontError::AxisOutOfRange {
                    name: name.to_owned(),
                    tag: tag_str,
                    value: var.value,
                    min: axis.min_value,
                    max: axis.max_value,
                });
            }
            face.set_variation(tag, var.value);
        }
    }
    Ok(face)
}

fn collect_cmap(face: &TtfFace<'_>) -> HashMap<u32, u16> {
    let mut map = HashMap::new();
    let Some(cmap) = face.tables().cmap else {
        return map;
    };
    for subtable in cmap.subtables {
        if !subtable.is_unicode() {
            continue;
        }
        subtable.codepoints(|cp| {
            if map.contains_key(&cp) {
                return;
            }
            if let Some(ch) = char::from_u32(cp) {
                if let Some(gid) = face.glyph_index(ch) {
                    if gid.0 != 0 {
                        map.insert(cp, gid.0);
                    }
                }
            }
        });
    }
    map
}
