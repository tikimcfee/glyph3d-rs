//! The field mode: which glyph-field implementation a scene is built with.

use std::fmt;
use std::str::FromStr;

/// Which glyph-field implementation the renderer builds at load.
///
/// The mode is chosen once, before staging, and fixed for the scene's
/// lifetime: each mode has its own slot format, emission, upload and shaders,
/// so switching means rebuilding the field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GlyphFieldMode {
    /// One full 32 B placement record per glyph (`RenderSlot`), read by the
    /// vertex stage as-is. Works for any glyph sizes and any placement — the
    /// general mode, and the default.
    #[default]
    Instanced,
    /// A compact record per glyph with the placement derived in the vertex
    /// stage from per-line tables. Smaller and faster to emit, but leans on
    /// the layout's structure (rows on a grid, mostly uniform advances) to
    /// stay cheap. Planned; not yet implemented.
    Derived,
}

impl GlyphFieldMode {
    /// Every mode, in declaration order (for help text and tests).
    pub const ALL: [GlyphFieldMode; 2] = [GlyphFieldMode::Instanced, GlyphFieldMode::Derived];

    /// The CLI / log spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            GlyphFieldMode::Instanced => "instanced",
            GlyphFieldMode::Derived => "derived",
        }
    }
}

impl fmt::Display for GlyphFieldMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A field-mode name that is not one of [`GlyphFieldMode::ALL`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseGlyphFieldModeError(pub String);

impl fmt::Display for ParseGlyphFieldModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown glyph field mode '{}' (expected instanced | derived)", self.0)
    }
}

impl std::error::Error for ParseGlyphFieldModeError {}

impl FromStr for GlyphFieldMode {
    type Err = ParseGlyphFieldModeError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        GlyphFieldMode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == name)
            .ok_or_else(|| ParseGlyphFieldModeError(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for mode in GlyphFieldMode::ALL {
            assert_eq!(mode.as_str().parse::<GlyphFieldMode>(), Ok(mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
        assert!("Instanced".parse::<GlyphFieldMode>().is_err(), "names are lowercase only");
        assert_eq!(GlyphFieldMode::default(), GlyphFieldMode::Instanced);
    }
}
