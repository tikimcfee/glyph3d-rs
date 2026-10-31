//! The field mode: which glyph-field implementation a scene is built with.

use std::fmt;
use std::str::FromStr;

/// Which glyph-field implementation the renderer builds at load.
///
/// The mode is chosen once, before staging, and fixed for the scene's
/// lifetime: each mode has its own slot format, emission, upload and shaders,
/// so switching means rebuilding the field. One mode remains since the
/// Instanced field (one 32 B `RenderSlot` per glyph) was retired on
/// 2026-10-31; the enum is the seam a next mode lands in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", value(rename_all = "lower"))]
pub enum GlyphFieldMode {
    /// Compact 20 B record per glyph (`DerivedSlot`) with placement Y/Z derived
    /// in the vertex stage from per-line tables and ItemParamsGpu. The
    /// default since 2026-10-11.
    #[default]
    Derived,
}

impl GlyphFieldMode {
    /// Every mode, in declaration order (for help text and tests).
    pub const ALL: [GlyphFieldMode; 1] = [GlyphFieldMode::Derived];

    /// The CLI / log spelling.
    pub fn as_str(self) -> &'static str {
        match self {
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
        if self.0 == "instanced" {
            write!(f, "glyph field mode 'instanced' was retired on 2026-10-31 (expected derived)")
        } else {
            write!(f, "unknown glyph field mode '{}' (expected derived)", self.0)
        }
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
        assert!("Derived".parse::<GlyphFieldMode>().is_err(), "names are lowercase only");
        let retired = "instanced".parse::<GlyphFieldMode>().expect_err("instanced is retired");
        assert!(retired.to_string().contains("retired"), "{retired}");
        assert_eq!(GlyphFieldMode::default(), GlyphFieldMode::Derived);
    }
}
