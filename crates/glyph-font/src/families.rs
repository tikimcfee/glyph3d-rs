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
    pub variations: Vec<Variation>,
}

impl FaceSpec {
    /// A face backed by its own static font file.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            source: Some(FontSource::path(path)),
            variations: Vec::new(),
        }
    }

    /// A face backed by in-memory bytes.
    pub fn bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            source: Some(FontSource::bytes(bytes)),
            variations: Vec::new(),
        }
    }

    /// A variable-font instance sharing its upright font's bytes.
    pub fn instance(variations: impl Into<Vec<Variation>>) -> Self {
        Self {
            source: None,
            variations: variations.into(),
        }
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
    pub variations: Vec<Variation>,
    /// Whether this font's cell includes its `hhea.line_gap` (half above, half below).
    pub include_line_gap: bool,
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
            variations: Vec::new(),
            include_line_gap: false,
            bold: None,
            italic: None,
            bold_italic: None,
        }
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
    vec![
        cousine_family_spec(fonts_dir),
        FontSpec::new(
            "MesloLGS NF Mono",
            FontSource::path(fonts_dir.join("MesloLGS-NF-Mono.ttf")),
        ),
        FontSpec::new(
            "DejaVu Sans",
            FontSource::path(fonts_dir.join("DejaVuSans.ttf")),
        ),
        monaspace_family_spec(
            MONASPACE_NEON_FAMILY,
            "Monaspace Neon",
            fonts_dir.join("MonaspaceNeonVar.ttf"),
        ),
        monaspace_family_spec(
            MONASPACE_ARGON_FAMILY,
            "Monaspace Argon",
            fonts_dir.join("MonaspaceArgonVar.ttf"),
        ),
    ]
}
