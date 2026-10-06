//! Launch configuration for Glyph3D.
//!
//! Loads startup preferences from a configuration file (e.g. `launch_config.toml`
//! or a path specified via `--launch-config <path>`).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
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
}

impl LaunchConfig {
    /// Load launch config from default locations (`launch_config.toml` or `../launch_config.toml`),
    /// or return default configuration if neither exists.
    pub fn load_or_default() -> Self {
        if Path::new("launch_config.toml").is_file() {
            Self::from_file(Path::new("launch_config.toml")).unwrap_or_default()
        } else if Path::new("../launch_config.toml").is_file() {
            Self::from_file(Path::new("../launch_config.toml")).unwrap_or_default()
        } else {
            Self::default()
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
    }

    /// Parse a simple TOML-compatible string into LaunchConfig.
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let mut cfg = LaunchConfig::default();
        for (line_idx, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let val = val.trim();
            match key {
                "file_backgrounds" => {
                    cfg.file_backgrounds = Some(parse_bool(val, line_idx)?);
                }
                "file_bg_color" => {
                    cfg.file_bg_color = Some(parse_color(val, line_idx)?);
                }
                "lod_min_px" => {
                    cfg.lod_min_px = Some(val.parse::<f32>().map_err(|e| {
                        format!("line {}: invalid float for lod_min_px: {e}", line_idx + 1)
                    })?);
                }
                "wrap_mode" => {
                    cfg.wrap_mode = Some(strip_quotes(val));
                }
                "z_wrap_spacing" => {
                    cfg.z_wrap_spacing = Some(val.parse::<f64>().map_err(|e| {
                        format!("line {}: invalid float for z_wrap_spacing: {e}", line_idx + 1)
                    })?);
                }
                "cluster_mode" => {
                    cfg.cluster_mode = Some(strip_quotes(val));
                }
                "color_mode" => {
                    cfg.color_mode = Some(strip_quotes(val));
                }
                "no_cull" => {
                    cfg.no_cull = Some(parse_bool(val, line_idx)?);
                }
                "no_ui" => {
                    cfg.no_ui = Some(parse_bool(val, line_idx)?);
                }
                "greeking" => {
                    cfg.greeking = Some(parse_bool(val, line_idx)?);
                }
                "greek_pure" => {
                    cfg.greek_pure = Some(parse_bool(val, line_idx)?);
                }
                "greek_smooth" => {
                    cfg.greek_smooth = Some(parse_bool(val, line_idx)?);
                }
                "greek_onset_px" => {
                    cfg.greek_onset_px = Some(val.parse::<f32>().map_err(|e| {
                        format!("line {}: invalid float for greek_onset_px: {e}", line_idx + 1)
                    })?);
                }
                "load_repo" => {
                    cfg.load_repo = Some(PathBuf::from(strip_quotes(val)));
                }
                "repo_engine" => {
                    cfg.repo_engine = Some(strip_quotes(val));
                }
                "field_mode" => {
                    cfg.field_mode = Some(strip_quotes(val));
                }
                "claude_projects_dir" => {
                    cfg.claude_projects_dir = Some(PathBuf::from(strip_quotes(val)));
                }
                "antigravity_brain_dir" => {
                    cfg.antigravity_brain_dir = Some(PathBuf::from(strip_quotes(val)));
                }
                "agent_session" => {
                    cfg.agent_session = Some(PathBuf::from(strip_quotes(val)));
                }
                _ => {
                    // Unknown keys are ignored for forward-compatibility
                }
            }
        }
        Ok(cfg)
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

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

fn parse_bool(s: &str, line: usize) -> Result<bool, String> {
    match s.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("line {}: invalid boolean '{other}'", line + 1)),
    }
}

fn parse_color(s: &str, line: usize) -> Result<[f32; 4], String> {
    let clean = s.trim().trim_start_matches('[').trim_end_matches(']').trim_matches('"');
    let parts: Vec<&str> = clean.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        return Err(format!("line {}: expected 4 floats [r,g,b,a], got '{s}'", line + 1));
    }
    let mut out = [0.0f32; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse::<f32>().map_err(|e| {
            format!("line {}: invalid float '{p}': {e}", line + 1)
        })?;
    }
    Ok(out)
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
