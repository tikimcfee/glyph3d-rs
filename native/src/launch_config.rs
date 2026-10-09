//! Launch configuration for Glyph3D.
//!
//! Loads startup preferences from a configuration file (e.g. `launch_config.toml`
//! or a path specified via `--launch-config <path>`). Parsed with the `toml`
//! crate; an unknown key is an error naming the key, not a silently ignored
//! line — a mistyped setting otherwise does nothing and says nothing.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchConfig {
    pub file_backgrounds: Option<bool>,
    pub file_bg_color: Option<[f32; 4]>,
    pub lod_min_px: Option<f32>,
    pub wrap_mode: Option<String>,
    pub z_wrap_spacing: Option<f64>,
    pub cluster_mode: Option<String>,
    pub color_mode: Option<String>,
    pub no_cull: Option<bool>,
    pub no_ui: Option<bool>,
    pub greeking: Option<bool>,
    pub greek_pure: Option<bool>,
    pub greek_smooth: Option<bool>,
    pub greek_onset_px: Option<f32>,
    pub load_repo: Option<PathBuf>,
    pub repo_engine: Option<String>,
    pub field_mode: Option<String>,
    pub claude_projects_dir: Option<PathBuf>,
    pub antigravity_brain_dir: Option<PathBuf>,
    pub kimi_sessions_dir: Option<PathBuf>,
    /// Read by the `cargo glyph` launcher (its repo cycle), not the renderer.
    /// Declared so the one shared file parses here: unknown keys are errors.
    pub repo_presets: Option<Vec<String>>,
    pub agent_session: Option<PathBuf>,
    pub frames: Option<u32>,
    /// The file's `[section]` tables: runtime overrides for `config::Settings`,
    /// merged over `config/defaults.toml` (validated there, not here).
    #[serde(skip)]
    pub settings: toml::Table,
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
            lod_min_px = 1.5
            wrap_mode = "back"
            z_wrap_spacing = 0.20
            cluster_mode = "cluster"
            color_mode = "flat"
            no_cull = false
            no_ui = true
            greeking = true
            greek_pure = true
            greek_onset_px = 12.0
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
        assert_eq!(cfg.lod_min_px, Some(1.5));
        assert_eq!(cfg.wrap_mode.as_deref(), Some("back"));
        assert_eq!(cfg.z_wrap_spacing, Some(0.20));
        assert_eq!(cfg.cluster_mode.as_deref(), Some("cluster"));
        assert_eq!(cfg.color_mode.as_deref(), Some("flat"));
        assert_eq!(cfg.no_cull, Some(false));
        assert_eq!(cfg.no_ui, Some(true));
        assert_eq!(cfg.greeking, Some(true));
        assert_eq!(cfg.greek_pure, Some(true));
        assert_eq!(cfg.greek_onset_px, Some(12.0));
        assert_eq!(cfg.load_repo, Some(PathBuf::from("/path/to/repo")));
        assert_eq!(cfg.field_mode.as_deref(), Some("derived"));
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
        assert_eq!(cfg.wrap_mode.as_deref(), Some("down"));
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

    #[test]
    fn integer_literal_accepted_for_float_key() {
        let cfg = LaunchConfig::from_toml_str("lod_min_px = 2\n").expect("parse failed");
        assert_eq!(cfg.lod_min_px, Some(2.0));
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
