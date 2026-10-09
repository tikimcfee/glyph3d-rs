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
    pub lod: LodSettings,
    pub camera: CameraSettings,
    pub repo: RepoSettings,
    pub agent_cards: AgentCardSettings,
    pub ui: UiSettings,
    pub environment: EnvironmentSettings,
    pub quad_demo: QuadDemoSettings,
}

/// The glyph field view (`--render-file`, `--load-repo`, windowed).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlyphSceneSettings {
    /// RGBA the glyph field pass clears to (`wgpu::Color`).
    pub clear_color: [f64; 4],
    /// File card background RGBA when `--file-backgrounds` is on and no
    /// `--file-bg-color` / launch `file_bg_color` was given.
    pub file_bg_color: [f32; 4],
}

/// Far-LOD substitution (`glyph_scene::cull`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodSettings {
    /// On-screen px per em below which a segment draws its backdrop quad
    /// instead of glyphs (default for `--lod-min-px`).
    pub min_px: f32,
    /// Backdrop coverage gain: backdrop alpha = ink fraction x gain.
    pub backdrop_gain: f32,
}

/// Glyph-scene cameras (`glyph_scene::camera`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraSettings {
    /// Vertical field of view, degrees, every camera mode.
    pub fov_y_deg: f32,
    /// Front framing: fit distance = half-extent / tan(fov/2) x margin + pad.
    pub frame_margin: f32,
    pub frame_pad: f32,
    /// Fly mouse-look, radians per pixel.
    pub look_sensitivity: f32,
    /// Fly speed at start, and its scroll range, as fractions of the fit distance.
    pub fly_speed: f32,
    pub fly_speed_min: f32,
    pub fly_speed_max: f32,
    /// Fly speed multiplier per scroll line.
    pub fly_scroll_step: f32,
    /// Fly velocity damping rate, 1/s (~63% of the way to target per 1/rate s).
    pub fly_damping: f32,
    /// Fly near plane, world units.
    pub fly_near: f32,
    /// Fly far plane = distance to field center + fit x fly_far_fit, clamped.
    pub fly_far_fit: f32,
    pub fly_far_min: f32,
    pub fly_far_max: f32,
}

/// Repo scenes (`repo::shelf`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSettings {
    /// Per-directory group tints, multiplied with the syntax colours; a
    /// directory hashes (FNV-1a) to one entry. The `tint-cycle` verb walks
    /// the same list. Must be non-empty.
    pub dir_tints: Vec<[f32; 3]>,
    /// Far-LOD backdrop tint by file extension (flat colour mode). First
    /// entry listing the extension wins.
    pub extension_tints: Vec<ExtensionTint>,
    /// Backdrop tint for an extension no entry lists.
    pub extension_tint_fallback: [f32; 3],
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionTint {
    pub extensions: Vec<String>,
    pub tint: [f32; 3],
}

/// Agent-transcript turn cards and workdesk plates (`spatial_scene`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCardSettings {
    /// Banner pair (mind page, impact page) for a card given no event colours.
    pub default_banners: [[f32; 4]; 2],
    pub mind_page: [f32; 4],
    pub impact_page: [f32; 4],
    pub spine: [f32; 4],
    pub banners: CardBanners,
    pub workdesk: WorkdeskAccents,
}

/// Banner pair (mind page, impact page) per transcript event kind.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardBanners {
    pub user_prompt: [[f32; 4]; 2],
    pub thinking: [[f32; 4]; 2],
    pub file_read: [[f32; 4]; 2],
    pub file_edit: [[f32; 4]; 2],
    pub file_write: [[f32; 4]; 2],
    pub command: [[f32; 4]; 2],
    pub tool_invocation: [[f32; 4]; 2],
    pub assistant_response: [[f32; 4]; 2],
}

/// Workdesk plate accent per file action kind.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkdeskAccents {
    pub read: [f32; 4],
    pub edit: [f32; 4],
    pub write: [f32; 4],
    pub ast_analysis: [f32; 4],
}

/// egui overlay label colours, sRGB 0-255.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSettings {
    pub grab_active: [u8; 3],
    pub history_back: [u8; 3],
    pub history_live: [u8; 3],
    pub beat_summary: [u8; 3],
    pub workdesk_bullet: [u8; 3],
    pub harness_claude: [u8; 3],
    pub harness_antigravity: [u8; 3],
    pub harness_kimi: [u8; 3],
    pub harness_generic: [u8; 3],
    pub project_name: [u8; 3],
}

/// Whether the glyph scene draws the ground/sky environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lower")]
pub enum EnvironmentMode {
    /// No environment: the pass clears to `[glyph_scene] clear_color`.
    #[default]
    Off,
    /// A ground plane with a grid below the scene, and a sky gradient.
    Ground,
}

/// The ground/sky environment pass (`glyph_scene::environment`). Colours are
/// linear (the scene target is sRGB-encoded on write), like `clear_color`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSettings {
    /// Default for `--environment`.
    pub mode: EnvironmentMode,
    /// World units between the scene's lowest point and the ground.
    pub ground_gap: f32,
    /// Grid line spacing, world units — fixed; it never adapts to the camera.
    pub minor_spacing: f32,
    pub major_spacing: f32,
    pub line_width_px: f32,
    pub axis_width_px: f32,
    /// Grid lines are at full strength while a cell spans at least
    /// `line_fade_start_px` on screen, and gone at `line_fade_end_px`.
    pub line_fade_start_px: f32,
    pub line_fade_end_px: f32,
    /// Fog toward the horizon colour, by distance from the eye (world units).
    /// The end is also capped at `fog_far_fraction` of the far plane.
    pub fog_start: f32,
    pub fog_end: f32,
    pub fog_far_fraction: f32,
    pub ground_color: [f32; 3],
    /// RGBA: alpha is the line's strength over the ground.
    pub minor_line_color: [f32; 4],
    pub major_line_color: [f32; 4],
    /// The z = 0 line (running along x) and the x = 0 line (along z).
    pub axis_x_color: [f32; 4],
    pub axis_z_color: [f32; 4],
    pub sky_horizon_color: [f32; 3],
    pub sky_zenith_color: [f32; 3],
    /// How far above the horizon (in sin of elevation) the sky reaches zenith.
    pub sky_gradient_height: f32,
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
        let s: Self = serde::Deserialize::deserialize(toml::Value::Table(table))
            .map_err(|e: toml::de::Error| e.to_string())?;
        if s.repo.dir_tints.is_empty() {
            return Err("[repo] dir_tints must not be empty".into());
        }
        Ok(s)
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
        assert_eq!(s.glyph_scene.file_bg_color, [0.10f32, 0.10, 0.13, 0.85]);
        assert_eq!(s.lod.min_px, 1.0f32);
        assert_eq!(s.lod.backdrop_gain, 0.7f32);
        let c = &s.camera;
        assert_eq!(c.fov_y_deg, 40f32);
        assert_eq!((c.frame_margin, c.frame_pad), (1.08f32, 2.0f32));
        assert_eq!(c.look_sensitivity, 0.0022f32);
        assert_eq!((c.fly_speed, c.fly_speed_min, c.fly_speed_max), (0.4f32, 0.005f32, 8.0f32));
        assert_eq!((c.fly_scroll_step, c.fly_damping), (1.15f32, 10.0f32));
        assert_eq!(c.fly_near, 0.05f32);
        assert_eq!((c.fly_far_fit, c.fly_far_min, c.fly_far_max), (4.0f32, 20_000.0f32, 100_000.0f32));

        let r = &s.repo;
        let dir_tints: &[[f32; 3]] = &[
            [1.00, 1.00, 1.00],
            [1.00, 0.93, 0.87],
            [0.88, 1.00, 0.92],
            [0.88, 0.95, 1.00],
            [1.00, 0.91, 1.00],
            [0.95, 1.00, 0.88],
            [1.00, 0.97, 0.86],
            [0.92, 0.92, 1.00],
            [0.90, 1.00, 1.00],
            [1.00, 0.88, 0.88],
        ];
        assert_eq!(r.dir_tints, dir_tints);
        let ext: &[(&[&str], [f32; 3])] = &[
            (&["rs"], [0.85, 0.40, 0.20]),
            (&["js", "mjs", "cjs"], [0.95, 0.85, 0.20]),
            (&["ts", "tsx"], [0.20, 0.50, 0.85]),
            (&["json"], [0.90, 0.75, 0.30]),
            (&["md", "markdown"], [0.40, 0.60, 0.80]),
            (&["toml", "yaml", "yml"], [0.70, 0.40, 0.60]),
            (&["py"], [0.25, 0.65, 0.55]),
            (&["c", "h", "cpp", "hpp", "cc"], [0.35, 0.55, 0.85]),
            (&["go"], [0.20, 0.70, 0.85]),
            (&["sh", "bash", "zsh"], [0.45, 0.75, 0.45]),
            (&["html", "htm"], [0.90, 0.45, 0.25]),
            (&["css", "scss", "less"], [0.30, 0.55, 0.90]),
        ];
        assert_eq!(r.extension_tints.len(), ext.len());
        for (got, (exts, tint)) in r.extension_tints.iter().zip(ext) {
            assert_eq!(got.extensions, *exts);
            assert_eq!(got.tint, *tint);
        }
        assert_eq!(r.extension_tint_fallback, [0.75f32, 0.75, 0.75]);

        let a = &s.agent_cards;
        assert_eq!(a.default_banners, [[0.20f32, 0.36, 0.60, 0.95], [0.18, 0.52, 0.35, 0.95]]);
        assert_eq!(a.mind_page, [0.08f32, 0.10, 0.14, 0.90]);
        assert_eq!(a.impact_page, [0.10f32, 0.12, 0.17, 0.90]);
        assert_eq!(a.spine, [0.18f32, 0.22, 0.30, 0.75]);
        let b = &a.banners;
        assert_eq!(b.user_prompt, [[0.22f32, 0.38, 0.65, 0.95], [0.18, 0.28, 0.48, 0.95]]);
        assert_eq!(b.thinking, [[0.48f32, 0.36, 0.15, 0.95], [0.38, 0.28, 0.12, 0.95]]);
        assert_eq!(b.file_read, [[0.15f32, 0.38, 0.58, 0.95], [0.12, 0.30, 0.48, 0.95]]);
        assert_eq!(b.file_edit, [[0.65f32, 0.38, 0.12, 0.95], [0.55, 0.30, 0.10, 0.95]]);
        assert_eq!(b.file_write, [[0.15f32, 0.55, 0.30, 0.95], [0.12, 0.45, 0.25, 0.95]]);
        assert_eq!(b.command, [[0.28f32, 0.28, 0.32, 0.95], [0.20, 0.20, 0.24, 0.95]]);
        assert_eq!(b.tool_invocation, [[0.25f32, 0.35, 0.45, 0.95], [0.18, 0.26, 0.35, 0.95]]);
        assert_eq!(b.assistant_response, [[0.18f32, 0.48, 0.38, 0.95], [0.14, 0.38, 0.30, 0.95]]);
        let w = &a.workdesk;
        assert_eq!(w.read, [0.15f32, 0.35, 0.55, 0.90]);
        assert_eq!(w.edit, [0.60f32, 0.40, 0.15, 0.90]);
        assert_eq!(w.write, [0.15f32, 0.50, 0.30, 0.90]);
        assert_eq!(w.ast_analysis, [0.45f32, 0.20, 0.55, 0.90]);
        // [ui] is integer sRGB, exact by construction; no pin needed.
        assert_eq!(s.quad_demo.clear_color, [0.02, 0.02, 0.04, 1.0]);
    }

    #[test]
    fn empty_dir_tints_refused() {
        let over: toml::Table = toml::from_str("[repo]\ndir_tints = []\n").unwrap();
        let err = Settings::with_overrides(over).unwrap_err();
        assert!(err.contains("dir_tints"), "{err}");
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
