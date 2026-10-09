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
    pub agent_session: Option<PathBuf>,
    pub frames: Option<u32>,
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

    /// Return the resolved Claude Code projects directory, falling back to `~/.claude/projects`.
    pub fn resolved_claude_projects_dir(&self) -> PathBuf {
        if let Some(dir) = &self.claude_projects_dir {
            expand_home(dir)
        } else {
            expand_home(Path::new("~/.claude/projects"))
        }
    }

    /// Return the resolved Antigravity brain directory, falling back to `~/.gemini/antigravity/brain`.
    pub fn resolved_antigravity_brain_dir(&self) -> PathBuf {
        if let Some(dir) = &self.antigravity_brain_dir {
            expand_home(dir)
        } else {
            expand_home(Path::new("~/.gemini/antigravity/brain"))
        }
    }

    /// Load from a file path. Returns Err with a message if reading or parsing fails.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read launch config '{}': {e}", path.display()))?;
        Self::from_toml_str(&content)
            .map_err(|e| format!("invalid launch config '{}': {e}", path.display()))
    }

    /// Parse a TOML string into LaunchConfig.
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }
}

/// Expand leading `~` or `~/` to the user's home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(stripped) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

/// Retrieve the user's home directory via HOME or USERPROFILE environment variable.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

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
        assert_eq!(cfg.agent_session, Some(PathBuf::from("~/sessions/my_session.jsonl")));
        assert_eq!(cfg.frames, Some(1));
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
