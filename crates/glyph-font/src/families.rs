//! Font chain configuration and the default font families shipped in `assets/fonts/`,
//! matching `packages/glyph3d-core/src/fonts/families.js`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default family identifier (index 0 of [`default_font_chain`]).
pub const DEFAULT_FONT_FAMILY: &str = "cousine";

/// Monaspace Neon family identifier.
pub const MONASPACE_NEON_FAMILY: &str = "monaspace-neon";

/// Monaspace Argon family identifier.
pub const MONASPACE_ARGON_FAMILY: &str = "monaspace-argon";

/// One variable-font axis coordinate (`tag` is a 4-byte OpenType axis tag like `*b"wght"`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Variation {
    pub tag: [u8; 4],
    pub value: f32,
}

impl Variation {
    /// Construct a variation setting from a 4-byte OpenType axis tag and value.
    pub const fn new(tag: [u8; 4], value: f32) -> Self {
        Self { tag, value }
    }
}

/// Where a font file's bytes come from.
#[derive(Clone, Debug)]
pub enum FontSource {
    /// Load bytes from a path on disk.
    Path(PathBuf),
    /// Use already-loaded font bytes.
    Bytes(Arc<[u8]>),
}

impl FontSource {
    /// Construct a `FontSource::Path`.
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    /// Construct a `FontSource::Bytes`.
    pub fn bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::Bytes(bytes.into())
    }
}

/// Specification for a bold, italic, or bold-italic face of a font.
///
/// Either provides its own `source` file/bytes, or sets `source: None` with `variations`
/// to create a variable-font instance sharing its upright font's file bytes and cmap.
#[derive(Clone, Debug)]
pub struct FaceSpec {
    pub source: Option<FontSource>,
    /// Face index inside a TrueType/OpenType collection (`.ttc`/`.otc`); `0` for standalone files.
    pub face_index: u32,
    pub variations: Vec<Variation>,
}

impl FaceSpec {
    /// A face backed by its own static font file.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::file_face(path, 0)
    }

    /// A face backed by face `face_index` of a font file or `.ttc`/`.otc` collection.
    pub fn file_face(path: impl Into<PathBuf>, face_index: u32) -> Self {
        Self {
            source: Some(FontSource::path(path)),
            face_index,
            variations: Vec::new(),
        }
    }

    /// A face backed by in-memory bytes.
    pub fn bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            source: Some(FontSource::bytes(bytes)),
            face_index: 0,
            variations: Vec::new(),
        }
    }

    /// A variable-font instance sharing its upright font's bytes.
    pub fn instance(variations: impl Into<Vec<Variation>>) -> Self {
        Self {
            source: None,
            face_index: 0,
            variations: variations.into(),
        }
    }

    /// Attach variable-font axis settings to a file/bytes-backed face (e.g. a separate italic variable file).
    pub fn with_variations(mut self, variations: impl Into<Vec<Variation>>) -> Self {
        self.variations = variations.into();
        self
    }
}

/// Specification for a listed font in the chain.
///
/// A font with `family: Some(id)` is that family's primary font (routed for that family alone,
/// ahead of the fallbacks). A font with `family: None` is a fallback font shared by every family.
#[derive(Clone, Debug)]
pub struct FontSpec {
    pub name: String,
    pub family: Option<String>,
    pub source: FontSource,
    /// Face index inside a TrueType/OpenType collection (`.ttc`/`.otc`); `0` for standalone files.
    pub face_index: u32,
    pub variations: Vec<Variation>,
    /// Whether this font's cell includes its `hhea.line_gap` (half above, half below).
    pub include_line_gap: bool,
    /// Vertical line-height multiplier (`1.0` = natural cell height; >1.0 adds symmetric leading).
    pub line_height_scale: f64,
    /// When `true`, defer reading and parsing this font (and its faces) until a glyph lookup
    /// or family metric query actually reaches it.
    pub lazy: bool,
    pub bold: Option<FaceSpec>,
    pub italic: Option<FaceSpec>,
    pub bold_italic: Option<FaceSpec>,
}

impl FontSpec {
    /// Create a plain upright font specification (either a fallback or a family primary).
    pub fn new(name: impl Into<String>, source: FontSource) -> Self {
        Self {
            name: name.into(),
            family: None,
            source,
            face_index: 0,
            variations: Vec::new(),
            include_line_gap: false,
            line_height_scale: 1.0,
            lazy: false,
            bold: None,
            italic: None,
            bold_italic: None,
        }
    }

    /// Defer reading and parsing this font until a lookup actually reaches it.
    pub fn with_lazy(mut self, lazy: bool) -> Self {
        self.lazy = lazy;
        self
    }

    /// Select face index inside a `.ttc`/`.otc` collection.
    pub fn with_face_index(mut self, face_index: u32) -> Self {
        self.face_index = face_index;
        self
    }

    /// Set the vertical line-height multiplier (`1.0` = default).
    pub fn with_line_height_scale(mut self, scale: f64) -> Self {
        self.line_height_scale = scale;
        self
    }

    /// Mark this font as the primary of `family`.
    pub fn with_family(mut self, family: impl Into<String>) -> Self {
        self.family = Some(family.into());
        self
    }

    /// Set variable-font axis values for this font's upright face.
    pub fn with_variations(mut self, variations: impl Into<Vec<Variation>>) -> Self {
        self.variations = variations.into();
        self
    }

    /// Include `hhea.line_gap` in the cell's vertical em (half above, half below).
    pub fn with_line_gap(mut self, include: bool) -> Self {
        self.include_line_gap = include;
        self
    }

    /// Attach bold, italic, and bold-italic face specs.
    pub fn with_faces(
        mut self,
        bold: Option<FaceSpec>,
        italic: Option<FaceSpec>,
        bold_italic: Option<FaceSpec>,
    ) -> Self {
        self.bold = bold;
        self.italic = italic;
        self.bold_italic = bold_italic;
        self
    }
}

/// Build a Monaspace variable-font family spec (`wght` 400/700, `slnt` 0/-11, `line_gap: false`),
/// matching `monaspace(...)` in `fonts/families.js`.
pub fn monaspace_family_spec(
    family: impl Into<String>,
    name: impl Into<String>,
    path: impl Into<PathBuf>,
) -> FontSpec {
    const REGULAR_WEIGHT: f32 = 400.0;
    const BOLD_WEIGHT: f32 = 700.0;
    const ITALIC_SLANT: f32 = -11.0;

    FontSpec::new(name, FontSource::path(path))
        .with_family(family)
        .with_variations(vec![
            Variation::new(*b"wght", REGULAR_WEIGHT),
            Variation::new(*b"slnt", 0.0),
        ])
        .with_line_gap(false)
        .with_faces(
            Some(FaceSpec::instance(vec![
                Variation::new(*b"wght", BOLD_WEIGHT),
                Variation::new(*b"slnt", 0.0),
            ])),
            Some(FaceSpec::instance(vec![
                Variation::new(*b"wght", REGULAR_WEIGHT),
                Variation::new(*b"slnt", ITALIC_SLANT),
            ])),
            Some(FaceSpec::instance(vec![
                Variation::new(*b"wght", BOLD_WEIGHT),
                Variation::new(*b"slnt", ITALIC_SLANT),
            ])),
        )
}

/// Build the Cousine family spec (four static TrueType files in `fonts_dir`).
pub fn cousine_family_spec(fonts_dir: &Path) -> FontSpec {
    FontSpec::new("Cousine", FontSource::path(fonts_dir.join("Cousine-Regular.ttf")))
        .with_family(DEFAULT_FONT_FAMILY)
        .with_faces(
            Some(FaceSpec::file(fonts_dir.join("Cousine-Bold.ttf"))),
            Some(FaceSpec::file(fonts_dir.join("Cousine-Italic.ttf"))),
            Some(FaceSpec::file(fonts_dir.join("Cousine-BoldItalic.ttf"))),
        )
}

/// Shared fallback fonts routed after every primary family (`MesloLGS NF Mono`, `DejaVu Sans`).
pub fn fallback_font_chain(fonts_dir: &Path) -> Vec<FontSpec> {
    vec![
        FontSpec::new(
            "MesloLGS NF Mono",
            FontSource::path(fonts_dir.join("MesloLGS-NF-Mono.ttf")),
        ),
        FontSpec::new(
            "DejaVu Sans",
            FontSource::path(fonts_dir.join("DejaVuSans.ttf")),
        ),
    ]
}

/// Match one of the three bundled wall font families (`"cousine"`, `"monaspace-neon"`,
/// `"monaspace-argon"`, or their spaced/title-cased names) from `fonts_dir`.
pub fn bundled_family_spec(fonts_dir: &Path, name: &str) -> Option<FontSpec> {
    let key: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match key.as_str() {
        "cousine" => Some(cousine_family_spec(fonts_dir)),
        "monaspaceneon" => Some(monaspace_family_spec(
            MONASPACE_NEON_FAMILY,
            "Monaspace Neon",
            fonts_dir.join("MonaspaceNeonVar.ttf"),
        )),
        "monaspaceargon" => Some(monaspace_family_spec(
            MONASPACE_ARGON_FAMILY,
            "Monaspace Argon",
            fonts_dir.join("MonaspaceArgonVar.ttf"),
        )),
        _ => None,
    }
}

/// Build the default wall font chain from `fonts_dir` (normally `assets/fonts/`):
///
/// 1. Cousine (default family `"cousine"`, four static faces)
/// 2. MesloLGS NF Mono (fallback)
/// 3. DejaVu Sans (fallback)
/// 4. Monaspace Neon (family `"monaspace-neon"`, variable font with four instances)
/// 5. Monaspace Argon (family `"monaspace-argon"`, variable font with four instances)
///
/// Matches the ordering rule of `fonts/families.js::fontChain()`: default family first,
/// fallbacks next (so their indices are stable), then every additional family.
pub fn default_font_chain(fonts_dir: &Path) -> Vec<FontSpec> {
    let mut chain = vec![cousine_family_spec(fonts_dir)];
    chain.extend(fallback_font_chain(fonts_dir));
    chain.push(monaspace_family_spec(
        MONASPACE_NEON_FAMILY,
        "Monaspace Neon",
        fonts_dir.join("MonaspaceNeonVar.ttf"),
    ));
    chain.push(monaspace_family_spec(
        MONASPACE_ARGON_FAMILY,
        "Monaspace Argon",
        fonts_dir.join("MonaspaceArgonVar.ttf"),
    ));
    chain
}

/// Like [`default_font_chain`], but marks every fallback font and every family other than
/// `eager_family` as [`FontSpec::lazy`] so startup reads and parses only the active family.
pub fn default_font_chain_lazy(fonts_dir: &Path, eager_family: &str) -> Vec<FontSpec> {
    default_font_chain(fonts_dir)
        .into_iter()
        .map(|spec| {
            let is_eager = spec.family.as_deref() == Some(eager_family);
            spec.with_lazy(!is_eager)
        })
        .collect()
}
