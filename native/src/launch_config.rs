//! Launch configuration for Glyph3D.
//!
//! Loads startup preferences from a configuration file (e.g. `launch_config.toml`
//! or a path specified via `--launch-config <path>`). Parsed with the `toml`
//! crate; an unknown key is an error naming the key, not a silently ignored
//! line — a mistyped setting otherwise does nothing and says nothing. The
//! same holds for a value: a mode is the CLI's own enum (C27, 2026-10-10).

use std::path::{Path, PathBuf};

use crate::cli::{DebugTint, PresentMode};
use crate::fold::{ClusterMode, WrapMode};
use crate::repo::{ColorMode, RepoLayoutMode, Strategy};
use glyph_field::GlyphFieldMode;

/// The launch options a file can set. A mode is typed as the flag's own
/// `ValueEnum` and spelled as the flag spells it, so a bad value is refused
/// naming it, as an unknown key is. Until C27 (2026-10-10) the modes were
/// strings whose bad values only logged at merge time, and the launcher read
/// the same file through its own looser copy of these keys.
///
/// The launcher writes this struct back out (`to_toml_string`) as the one
/// argument of the renderer it starts, so every choice the launcher offers is
/// a key here (`render_file`, `demo`, `focus_file`, `layout_mode`,
/// `present_mode` and `debug_tint` arrived for that).
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub file_backgrounds: Option<bool>,
    pub file_bg_color: Option<[f32; 4]>,
    /// "Show glyphs", px per text row (`--show-glyphs-px`). Was `lod_min_px`
    /// until 2026-10-10 (C26); the old key still parses as an alias.
    #[serde(alias = "lod_min_px")]
    pub show_glyphs_px: Option<f32>,
    #[serde(default, with = "value_enum")]
    pub wrap_mode: Option<WrapMode>,
    pub z_wrap_spacing: Option<f64>,
    #[serde(default, with = "value_enum")]
    pub cluster_mode: Option<ClusterMode>,
    #[serde(default, with = "value_enum")]
    pub color_mode: Option<ColorMode>,
    pub no_cull: Option<bool>,
    pub no_ui: Option<bool>,
    pub greeking: Option<bool>,
    pub greek_pure: Option<bool>,
    pub greek_smooth: Option<bool>,
    /// "Text detail", px per text row (`--text-detail-px`). Was
    /// `greek_onset_px`; the old key still parses as an alias.
    #[serde(alias = "greek_onset_px")]
    pub text_detail_px: Option<f32>,
    pub load_repo: Option<PathBuf>,
    /// `--focus-file`: frame the first repo file whose path contains this.
    pub focus_file: Option<String>,
    #[serde(default, with = "value_enum")]
    pub layout_mode: Option<RepoLayoutMode>,
    #[serde(default, with = "value_enum")]
    pub repo_engine: Option<Strategy>,
    /// `--render-file`: the text scene's file.
    pub render_file: Option<PathBuf>,
    /// `--demo`: the quad-field demo scene.
    pub demo: Option<bool>,
    #[serde(default, with = "value_enum")]
    pub field_mode: Option<GlyphFieldMode>,
    #[serde(default, with = "value_enum")]
    pub debug_tint: Option<DebugTint>,
    #[serde(default, with = "value_enum")]
    pub present_mode: Option<PresentMode>,
    pub claude_projects_dir: Option<PathBuf>,
    pub antigravity_brain_dir: Option<PathBuf>,
    pub kimi_sessions_dir: Option<PathBuf>,
    /// The launcher's repo cycle (◄/► on its repo field); the scene never
    /// reads it.
    pub repo_presets: Option<Vec<String>>,
    pub agent_session: Option<PathBuf>,
    pub frames: Option<u32>,
    /// The file's `[section]` tables: runtime overrides for `config::Settings`,
    /// merged over `config/defaults.toml` (validated there, not here).
    #[serde(skip)]
    pub settings: toml::Table,
}

/// A `ValueEnum` variant's CLI spelling. No option enum of the renderer
/// skips a variant, so every variant has one.
pub fn value_name<T: clap::ValueEnum>(v: &T) -> String {
    v.to_possible_value().expect("no option enum skips a variant").get_name().to_string()
}

/// Serde for an optional clap `ValueEnum`: written as its CLI name, read
/// case-insensitively; a name that is no variant is an error listing the
/// names that are.
mod value_enum {
    use clap::ValueEnum;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<T: ValueEnum, S: Serializer>(value: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => s.serialize_some(&super::value_name(v)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: ValueEnum, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
        let Some(name) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        T::from_str(&name, true).map(Some).map_err(|_| {
            let names: Vec<String> = T::value_variants().iter().map(super::value_name).collect();
            D::Error::custom(format!("unknown value '{name}' (expected {})", names.join(" | ")))
        })
    }
}

impl LaunchConfig {
    /// Load launch config from default locations (`launch_config.toml` or `../launch_config.toml`),
    /// or return default configuration if neither exists. A file that exists but does not
    /// parse is logged as an error and the defaults are used — this is the mid-session
    /// re-read; startup refuses the same file (`cli::args::parse_cli_from`).
    pub fn load_or_default() -> Self {
        let path = [Path::new("launch_config.toml"), Path::new("../launch_config.toml")]
            .into_iter()
            .find(|p| p.is_file());
        match path.map(Self::from_file) {
            Some(Ok(cfg)) => cfg,
            Some(Err(e)) => {
                log::error!("{e}");
                Self::default()
            }
            None => Self::default(),
        }
    }

    /// The per-harness session locations this config names (unexpanded).
    pub fn session_overrides(&self) -> glyph_session_dirs::Overrides {
        glyph_session_dirs::Overrides {
            claude: self.claude_projects_dir.clone(),
            antigravity: self.antigravity_brain_dir.clone(),
            kimi: self.kimi_sessions_dir.clone(),
        }
    }

    /// The directories session discovery scans: per harness, the configured
    /// path if set (an empty value disables that harness), else the app's own
    /// default locations that exist on this machine. See `glyph-session-dirs`.
    pub fn session_dirs(&self) -> Vec<glyph_session_dirs::SessionDir> {
        glyph_session_dirs::resolve(&self.session_overrides())
    }

    /// Load from a file path. Returns Err with a message if reading or parsing fails.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read launch config '{}': {e}", path.display()))?;
        Self::from_toml_str(&content)
            .map_err(|e| format!("invalid launch config '{}': {e}", path.display()))
    }

    /// Parse a TOML string into LaunchConfig. Top-level keys are launch
    /// options; `[section]` tables are settings overrides, split off whole.
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let mut table: toml::Table = toml::from_str(s).map_err(|e| e.to_string())?;
        let sections: Vec<String> = table
            .iter()
            .filter(|(_, v)| v.is_table())
            .map(|(k, _)| k.clone())
            .collect();
        let mut settings = toml::Table::new();
        for key in sections {
            if let Some(v) = table.remove(&key) {
                settings.insert(key, v);
            }
        }
        let mut cfg: Self = serde::Deserialize::deserialize(toml::Value::Table(table))
            .map_err(|e: toml::de::Error| e.to_string())?;
        cfg.settings = settings;
        Ok(cfg)
    }

    /// The config as a file `from_toml_str` reads back to an equal value:
    /// the set keys, then the `[section]` overrides. Comments are not kept;
    /// this writes the launcher's per-launch file, never the user's own.
    pub fn to_toml_string(&self) -> String {
        let mut table = toml::Table::try_from(self).expect("a launch config serializes");
        for (k, v) in &self.settings {
            table.insert(k.clone(), v.clone());
        }
        toml::to_string(&table).expect("a toml table serializes")
    }
}

/// Expand leading `~` or `~/` to the user's home directory. One definition,
/// shared with the launcher through `glyph-session-dirs`.
pub use glyph_session_dirs::{expand_home, home_dir};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_launch_config_toml() {
        let toml = r#"
            # Glyph3D launch config
            file_backgrounds = true
            file_bg_color = [0.12, 0.13, 0.18, 0.90]
            show_glyphs_px = 1.5
            wrap_mode = "back"
            z_wrap_spacing = 0.20
            cluster_mode = "cluster"
            color_mode = "flat"
            no_cull = false
            no_ui = true
            greeking = true
            greek_pure = true
            text_detail_px = 12.0
            load_repo = "/path/to/repo"
            field_mode = "derived"
            claude_projects_dir = "~/my_claude_projects"
            antigravity_brain_dir = "~/my_antigravity_brain"
            kimi_sessions_dir = ""
            repo_presets = [".", "~/src/big-repo"]
            agent_session = "~/sessions/my_session.jsonl"
            frames = 1
        "#;
        let cfg = LaunchConfig::from_toml_str(toml).expect("parse failed");
        assert_eq!(cfg.file_backgrounds, Some(true));
        assert_eq!(cfg.file_bg_color, Some([0.12, 0.13, 0.18, 0.90]));
        assert_eq!(cfg.show_glyphs_px, Some(1.5));
        assert_eq!(cfg.wrap_mode, Some(WrapMode::Back));
        assert_eq!(cfg.z_wrap_spacing, Some(0.20));
        assert_eq!(cfg.cluster_mode, Some(ClusterMode::Cluster));
        assert_eq!(cfg.color_mode, Some(ColorMode::Flat));
        assert_eq!(cfg.no_cull, Some(false));
        assert_eq!(cfg.no_ui, Some(true));
        assert_eq!(cfg.greeking, Some(true));
        assert_eq!(cfg.greek_pure, Some(true));
        assert_eq!(cfg.text_detail_px, Some(12.0));
        assert_eq!(cfg.load_repo, Some(PathBuf::from("/path/to/repo")));
        assert_eq!(cfg.field_mode, Some(GlyphFieldMode::Derived));
        assert_eq!(cfg.claude_projects_dir, Some(PathBuf::from("~/my_claude_projects")));
        assert_eq!(cfg.antigravity_brain_dir, Some(PathBuf::from("~/my_antigravity_brain")));
        // An empty value parses as an empty path, which disables that harness.
        assert_eq!(cfg.kimi_sessions_dir, Some(PathBuf::new()));
        let dirs = cfg.session_dirs();
        assert!(dirs.iter().all(|d| d.harness != glyph_session_dirs::Harness::KimiCode));
        assert!(dirs.iter().any(|d| d.harness == glyph_session_dirs::Harness::ClaudeCode
            && d.origin == glyph_session_dirs::Origin::Config));
        assert_eq!(cfg.repo_presets, Some(vec![".".to_string(), "~/src/big-repo".to_string()]));
        assert_eq!(cfg.agent_session, Some(PathBuf::from("~/sessions/my_session.jsonl")));
        assert_eq!(cfg.frames, Some(1));
    }

    #[test]
    fn sections_are_settings_overrides() {
        let cfg = LaunchConfig::from_toml_str(
            "wrap_mode = \"down\"\n[glyph_scene]\nclear_color = [1.0, 0.0, 0.0, 1.0]\n",
        )
        .expect("parse failed");
        assert_eq!(cfg.wrap_mode, Some(WrapMode::Down));
        assert!(cfg.settings.contains_key("glyph_scene"));
        let s = crate::config::Settings::with_overrides(cfg.settings).expect("merge failed");
        assert_eq!(s.glyph_scene.clear_color, [1.0, 0.0, 0.0, 1.0]);
    }

    /// The committed example is what people copy; it must keep parsing as the
    /// keys change, sections and all.
    #[test]
    fn example_file_parses() {
        let cfg = LaunchConfig::from_toml_str(include_str!("../../launch_config.example.toml"))
            .expect("launch_config.example.toml must parse");
        crate::config::Settings::with_overrides(cfg.settings)
            .expect("launch_config.example.toml sections must merge");
    }

    #[test]
    fn unknown_key_is_an_error_naming_it() {
        let err = LaunchConfig::from_toml_str("file_backgroundz = true\n").unwrap_err();
        assert!(err.contains("file_backgroundz"), "error should name the key: {err}");
    }

    /// A mode the CLI does not know is refused at load, naming the value and
    /// the ones it could have been — it used to log a warning at merge time
    /// and run in the default mode.
    #[test]
    fn unknown_mode_value_is_an_error_naming_it() {
        let err = LaunchConfig::from_toml_str("field_mode = \"visable\"\n").unwrap_err();
        assert!(err.contains("visable"), "error should name the value: {err}");
        assert!(err.contains("instanced | derived | visible"), "error should list the modes: {err}");
        // Case is forgiven, as the launcher always forgave it.
        let cfg = LaunchConfig::from_toml_str("repo_engine = \"Hyper\"\n").expect("case-insensitive");
        assert_eq!(cfg.repo_engine, Some(Strategy::Hyper));
    }

    /// What the launcher writes is what the renderer reads: every key, each
    /// mode by its CLI name, and the sections.
    #[test]
    fn to_toml_round_trips() {
        let cfg = LaunchConfig::from_toml_str(
            r#"
            file_backgrounds = true
            file_bg_color = [0.1, 0.2, 0.3, 0.5]
            show_glyphs_px = 1.5
            text_detail_px = 9.0
            wrap_mode = "down"
            z_wrap_spacing = 0.25
            cluster_mode = "leader"
            color_mode = "syntax"
            greeking = false
            load_repo = "/abs/repo"
            focus_file = "alpha"
            layout_mode = "carrel"
            repo_engine = "direct"
            field_mode = "visible"
            debug_tint = "cull"
            present_mode = "mailbox"
            kimi_sessions_dir = ""
            repo_presets = ["."]
            frames = 3
            [glyph_scene]
            clear_color = [1.0, 0.0, 0.0, 1.0]
            "#,
        )
        .expect("parse failed");
        let text = cfg.to_toml_string();
        assert!(text.contains("field_mode = \"visible\""), "modes are written by CLI name: {text}");
        assert_eq!(LaunchConfig::from_toml_str(&text).expect("re-parse failed"), cfg);
        let other = LaunchConfig { demo: Some(true), render_file: Some("/f.rs".into()), ..Default::default() };
        assert_eq!(LaunchConfig::from_toml_str(&other.to_toml_string()).expect("re-parse failed"), other);
    }

    #[test]
    fn integer_literal_accepted_for_float_key() {
        let cfg = LaunchConfig::from_toml_str("show_glyphs_px = 2\n").expect("parse failed");
        assert_eq!(cfg.show_glyphs_px, Some(2.0));
    }

    /// C26 (2026-10-10) renamed `lod_min_px` and `greek_onset_px`; a file
    /// written before it still parses, into the new fields.
    #[test]
    fn old_lod_keys_are_aliases() {
        let cfg = LaunchConfig::from_toml_str("lod_min_px = 2\ngreek_onset_px = 12.0\n").expect("parse failed");
        assert_eq!(cfg.show_glyphs_px, Some(2.0));
        assert_eq!(cfg.text_detail_px, Some(12.0));
    }

    #[test]
    fn test_expand_home_logic() {
        let p = Path::new("relative/path/file.txt");
        assert_eq!(expand_home(p), PathBuf::from("relative/path/file.txt"));

        if let Some(home) = home_dir() {
            let tilde_path = Path::new("~/sub/dir");
            assert_eq!(expand_home(tilde_path), home.join("sub/dir"));
            assert_eq!(expand_home(Path::new("~")), home);
        }
    }
}
