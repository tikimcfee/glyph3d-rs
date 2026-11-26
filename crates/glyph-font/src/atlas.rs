//! Runtime font chain and growable Slug atlas.
//!
//! Combines `FontChain` routing (families, bold/italic/bold-italic faces, variable-font
//! instances, shared fallback fonts) with on-demand Slug curve encoding into a growable
//! [`SlugBuffer`].
//!
//! A [`Slot`] is a plain `u32` (slot 0 = blank). Other glyph sources (cell sprites,
//! combining-mark clusters, colour emoji) can allocate slots from the same atlas via
//! [`FontAtlas::allocate_slot`] and write their curves or glyph-map entries directly into
//! its buffer without colliding with font glyph slots.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use ttf_parser::{Face as TtfFace, GlyphId, Tag};

use crate::buffer::{DirtyUpdate, SlugBuffer, TextureBacking};
use crate::constants::{BLANK_SLOT, GLYPH_MODE_OUTLINE};
use crate::encode::{encode_glyph, PackedCurve};
use crate::families::{FaceSpec, FontSource, FontSpec, Variation};
use crate::metrics::{FamilyMetrics, FontExtents};

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

/// Errors produced when loading or configuring a [`FontAtlas`].
#[derive(Debug)]
pub enum FontError {
    /// No fonts were provided to [`FontAtlas::load`].
    EmptyChain,
    /// Duplicate family identifier in the chain.
    DuplicateFamily(String),
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

struct LoadedFont {
    name: String,
    bytes: Arc<[u8]>,
    variations: Vec<Variation>,
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
    /// `(font_index, gid) -> Slot` for font-backed outline slots.
    slot_by_glyph: HashMap<(usize, u16), Slot>,
    /// `(family_idx, face_bits, codepoint) -> Slot` fast cache (including `BLANK_SLOT` misses).
    lookup_cache: HashMap<(usize, u8, u32), Slot>,
    /// Optional metadata per font-backed slot.
    slot_sources: HashMap<Slot, (usize, u16)>,
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
            let loaded = build_loaded_font(
                spec.name.clone(),
                bytes,
                spec.variations.clone(),
                spec.include_line_gap,
                None,
                None,
            )?;
            let idx = fonts.len();
            let mut loaded = loaded;
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
                let loaded = load_face_spec(
                    &face_name,
                    face_spec,
                    spec.include_line_gap,
                    upright_idx,
                    &fonts[upright_idx],
                )?;
                let idx = fonts.len();
                let mut loaded = loaded;
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

        let make_family = |primary: usize, id: Option<String>, fonts: &[LoadedFont]| -> Result<Family, FontError> {
            let mut route = Vec::with_capacity(1 + fallbacks.len());
            route.push(primary);
            for &fb in &fallbacks {
                if fb != primary {
                    route.push(fb);
                }
            }
            let font = &fonts[primary];
            let face = parse_and_vary(&font.name, &font.bytes, &font.variations)?;
            let include_gap = specs[primary].include_line_gap;
            let metrics = FamilyMetrics::from_face(&face, include_gap);
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
            slot_by_glyph: HashMap::new(),
            lookup_cache: HashMap::new(),
            slot_sources: HashMap::new(),
        })
    }

    /// Look up a family handle by its string id (e.g. `"cousine"`, `"monaspace-neon"`).
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

    /// Cell and line metrics for `family`. Panics if `family` is out of range.
    pub fn family_metrics(&self, family: FamilyId) -> FamilyMetrics {
        self.families[family.0].metrics
    }

    /// Look up (and encode on first use) the atlas [`Slot`] for `(family, face, codepoint)`.
    ///
    /// - Routes through `family`'s primary font first, then the shared fallbacks.
    /// - For non-regular faces, uses that font's matching face (falling back from `BoldItalic` to
    ///   `Italic` then `Bold`, then upright if absent or uncovered). Fallback fonts stay upright.
    /// - Returns [`BLANK_SLOT`] (`0`) when no font in the family's route covers `codepoint`.
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

        let resolved = self.resolve_font_glyph(fam_idx, face, codepoint);
        let slot = match resolved {
            None => BLANK_SLOT,
            Some((font_idx, gid)) => self.ensure_font_glyph(font_idx, gid),
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

    /// Resolve `(family, face, codepoint)` to `(font_index, gid)` without encoding or allocating.
    fn resolve_font_glyph(&self, fam_idx: usize, face: Face, codepoint: u32) -> Option<(usize, u16)> {
        let family = self.families.get(fam_idx)?;
        let mut upright = None;
        for &font_idx in &family.route {
            if let Some(&gid) = self.fonts[font_idx].cmap.get(&codepoint) {
                upright = Some((font_idx, gid));
                break;
            }
        }
        let (upright_idx, upright_gid) = upright?;

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

    fn ensure_font_glyph(&mut self, font_idx: usize, gid: u16) -> Slot {
        if let Some(&slot) = self.slot_by_glyph.get(&(font_idx, gid)) {
            return slot;
        }
        let slot = self.allocate_slot();
        let font = &self.fonts[font_idx];
        let face = parse_and_vary(&font.name, &font.bytes, &font.variations)
            .expect("already-validated font bytes must re-parse");
        let encoded = encode_glyph(&face, GlyphId(gid), font.extents);
        self.buffer
            .write_outline_slot(slot, &encoded.curves, GLYPH_MODE_OUTLINE, 0);
        self.slot_by_glyph.insert((font_idx, gid), slot);
        self.slot_sources.insert(slot, (font_idx, gid));
        slot
    }

    // ── Extensibility for external glyph sources (sprites, clusters, emoji) ──

    /// Allocate the next unused [`Slot`] id (`>= 1`).
    ///
    /// External glyph sources (cell sprites, combining-mark clusters, colour emoji) use this
    /// to claim slots from the same id space and then populate them with
    /// [`Self::insert_outline_slot`] or [`Self::insert_map_entry`].
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

    /// Write a map-only entry `[curve_start, curve_count, mode, arg]` (e.g. a mode-1 emoji slot)
    /// for a slot previously obtained from [`Self::allocate_slot`].
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

    /// Inspect which font and per-font glyph id produced `slot` (`None` for blank or external slots).
    pub fn describe_slot(&self, slot: Slot) -> Option<SlotInfo> {
        let &(font_idx, gid) = self.slot_sources.get(&slot)?;
        let font = &self.fonts[font_idx];
        let glyph_name = TtfFace::parse(&font.bytes, 0)
            .ok()
            .and_then(|f| f.glyph_name(GlyphId(gid)).map(str::to_owned));
        Some(SlotInfo {
            font_index: font_idx,
            font_name: font.name.clone(),
            glyph_id: gid,
            glyph_name,
        })
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
    upright_idx: usize,
    upright: &LoadedFont,
) -> Result<LoadedFont, FontError> {
    if let Some(ref source) = face_spec.source {
        let bytes = load_source(name, source)?;
        build_loaded_font(
            name.to_owned(),
            bytes,
            face_spec.variations.clone(),
            include_line_gap,
            None,
            None,
        )
    } else if !face_spec.variations.is_empty() {
        let base_idx = upright.instance_of.unwrap_or(upright_idx);
        build_loaded_font(
            name.to_owned(),
            Arc::clone(&upright.bytes),
            face_spec.variations.clone(),
            include_line_gap,
            Some(Arc::clone(&upright.cmap)),
            Some(base_idx),
        )
    } else {
        Err(FontError::EmptyFaceSpec {
            name: name.to_owned(),
        })
    }
}

fn build_loaded_font(
    name: String,
    bytes: Arc<[u8]>,
    variations: Vec<Variation>,
    include_line_gap: bool,
    shared_cmap: Option<Arc<HashMap<u32, u16>>>,
    instance_of: Option<usize>,
) -> Result<LoadedFont, FontError> {
    let face = parse_and_vary(&name, &bytes, &variations)?;
    let extents = FontExtents::from_face(&face, include_line_gap);
    let cmap = match shared_cmap {
        Some(existing) => existing,
        None => Arc::new(collect_cmap(&face)),
    };
    Ok(LoadedFont {
        name,
        bytes,
        variations,
        extents,
        cmap,
        instance_of,
        faces: [None; 4],
    })
}

fn parse_and_vary<'a>(
    name: &str,
    bytes: &'a [u8],
    variations: &[Variation],
) -> Result<TtfFace<'a>, FontError> {
    let mut face = TtfFace::parse(bytes, 0).map_err(|error| FontError::Parse {
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
