//! Settings: every named, tunable value the renderer and UI read — colors,
//! spacings, speeds — as distinct from CONTRACTS (layout metrics, slot and
//! atlas formats, anything a reference check was computed against), which
//! stay compiled constants because a config edit must never be able to
//! silently disagree with a fixture. The rule is
//! `.agents/rules/rust-engineering.md` §10.
//!
//! Two layers, one schema:
//! - `config/defaults.toml`, compiled in with `include_str!`. COMPLETE: every
//!   field is required, so a key missing from it fails to parse (the
//!   `defaults_parse` test, and first access) instead of reading as zero.
//! - Runtime overrides: the `[section]` tables of `launch_config.toml`,
//!   deep-merged over the defaults before deserializing. Partial; an unknown
//!   section or key is an error naming it.
//!
//! Screenshot runs — every gate that launches the binary — never auto-discover
//! `launch_config.toml` (`cli::args::parse_cli_from`), so they read the
//! compiled defaults only. That is what keeps a personal override out of the
//! golden views.

use std::sync::OnceLock;

/// The compiled-in defaults, verbatim.
pub const DEFAULTS_TOML: &str = include_str!("../../config/defaults.toml");

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub glyph_scene: GlyphSceneSettings,
    pub quad_demo: QuadDemoSettings,
}

/// The glyph field view (`--render-file`, `--load-repo`, windowed).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlyphSceneSettings {
    /// RGBA the glyph field pass clears to (`wgpu::Color`).
    pub clear_color: [f64; 4],
}

/// The 1M-instance quad field (`--demo`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuadDemoSettings {
    /// RGBA the quad field pass clears to (`wgpu::Color`).
    pub clear_color: [f64; 4],
}

impl Settings {
    /// The compiled defaults alone.
    pub fn defaults() -> Result<Self, String> {
        Self::with_overrides(toml::Table::new())
    }

    /// The compiled defaults with `overrides` deep-merged over them.
    pub fn with_overrides(overrides: toml::Table) -> Result<Self, String> {
        let mut table: toml::Table = toml::from_str(DEFAULTS_TOML)
            .map_err(|e| format!("config/defaults.toml: {e}"))?;
        merge(&mut table, overrides);
        Self::deserialize_table(table)
    }

    fn deserialize_table(table: toml::Table) -> Result<Self, String> {
        serde::Deserialize::deserialize(toml::Value::Table(table)).map_err(|e| e.to_string())
    }
}

/// Deep merge: tables merge key by key, anything else replaces. A key the
/// defaults do not have is inserted as-is, so `deny_unknown_fields` sees —
/// and names — it.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(key, v);
            }
        }
    }
}

/// An `[r, g, b, a]` setting as a `wgpu::Color`.
pub fn wgpu_color([r, g, b, a]: [f64; 4]) -> wgpu::Color {
    wgpu::Color { r, g, b, a }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// The process's settings. Defaults unless [`install`] ran first.
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| {
        Settings::defaults().unwrap_or_else(|e| panic!("compiled config defaults: {e}"))
    })
}

/// Install runtime overrides. Call once, at startup, before anything reads
/// [`settings`]: a read that happened first has already fixed the defaults
/// in place, and this refuses rather than letting the overrides vanish.
pub fn install(overrides: toml::Table) -> Result<(), String> {
    let merged = Settings::with_overrides(overrides)?;
    SETTINGS
        .set(merged)
        .map_err(|_| "settings were read before the launch config was applied".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        Settings::defaults().expect("config/defaults.toml must parse into Settings");
    }

    /// Migration pin: each value moved out of the code must come back from
    /// the defaults file bit-identical to the literal it replaced (a TOML
    /// float goes decimal -> f64 -> f32, which can land one ulp away from a
    /// decimal -> f32 literal). Only the golden views see these values, so a
    /// drift here would otherwise surface as an unexplained pixel diff.
    #[test]
    fn defaults_match_migrated_literals() {
        let s = Settings::defaults().unwrap();
        assert_eq!(s.glyph_scene.clear_color, [0.07, 0.07, 0.09, 1.0]);
        assert_eq!(s.quad_demo.clear_color, [0.02, 0.02, 0.04, 1.0]);
    }

    #[test]
    fn override_merges_over_defaults() {
        let over: toml::Table = toml::from_str("[glyph_scene]\nclear_color = [1.0, 0.0, 0.0, 1.0]\n").unwrap();
        let s = Settings::with_overrides(over).unwrap();
        assert_eq!(s.glyph_scene.clear_color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(s.quad_demo, Settings::defaults().unwrap().quad_demo);
    }

    #[test]
    fn unknown_override_key_is_an_error_naming_it() {
        let over: toml::Table = toml::from_str("[glyph_scene]\nclear_colour = [1.0, 0.0, 0.0, 1.0]\n").unwrap();
        let err = Settings::with_overrides(over).unwrap_err();
        assert!(err.contains("clear_colour"), "error should name the key: {err}");
        let over: toml::Table = toml::from_str("[glyph_scen]\n").unwrap();
        let err = Settings::with_overrides(over).unwrap_err();
        assert!(err.contains("glyph_scen"), "error should name the section: {err}");
    }
}
