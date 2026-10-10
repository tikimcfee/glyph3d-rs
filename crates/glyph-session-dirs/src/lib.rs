//! Where agent harnesses keep their session transcripts.
//!
//! One table, read by the renderer's session browser (F7) and its
//! launcher, so the two never disagree about where to look. (The launcher
//! lived in the build tool until C27, 2026-10-10, which is why this is a
//! std-only crate of its own.)
//!
//! Resolution is per harness:
//! - a path set in `launch_config.toml` wins, and is the ONLY place scanned
//!   for that harness (kept even when it does not exist, so a UI can say so);
//! - an EMPTY string turns that harness off;
//! - unset falls back to the app's own default locations, keeping only the
//!   ones that exist on this machine.
//!
//! Every resolved directory carries its [`Origin`], so a UI can report
//! exactly what it scanned and why.

use std::path::{Path, PathBuf};

/// An agent harness whose transcripts the renderer can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    ClaudeCode,
    Antigravity,
    KimiCode,
}

impl Harness {
    /// Scan order, which is also report order.
    pub const ALL: [Harness; 3] = [Harness::ClaudeCode, Harness::Antigravity, Harness::KimiCode];

    /// Short name for reports.
    pub fn label(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude",
            Harness::Antigravity => "antigravity",
            Harness::KimiCode => "kimi",
        }
    }

    /// The `launch_config.toml` key that overrides this harness's location.
    pub fn config_key(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude_projects_dir",
            Harness::Antigravity => "antigravity_brain_dir",
            Harness::KimiCode => "kimi_sessions_dir",
        }
    }
}

/// Why a directory is in the scan list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Named in `launch_config.toml`.
    Config,
    /// The app's own default location, found on this machine.
    Default,
}

/// One directory to scan for one harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDir {
    pub harness: Harness,
    pub path: PathBuf,
    pub origin: Origin,
}

/// The per-harness values from `launch_config.toml`, as written (`~` not yet
/// expanded). `None` = unset (use the app defaults); `Some("")` = disabled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub claude: Option<PathBuf>,
    pub antigravity: Option<PathBuf>,
    pub kimi: Option<PathBuf>,
}

impl Overrides {
    fn get(&self, harness: Harness) -> Option<&Path> {
        match harness {
            Harness::ClaudeCode => self.claude.as_deref(),
            Harness::Antigravity => self.antigravity.as_deref(),
            Harness::KimiCode => self.kimi.as_deref(),
        }
    }
}

/// The app-default locations for `harness` under `home`, in scan order.
///
/// The paths are the same relative to the home directory on every OS these
/// apps ship for: each keeps a dot-directory in the user's home on macOS,
/// Linux and Windows alike, so the OS only decides what `home` is (see
/// [`home_dir`]). Observed 2026-10: Claude Code and the Antigravity desktop
/// app on macOS; Claude Code, the Antigravity CLI and Kimi Code on Linux.
///
/// `claude_config_dir` is `$CLAUDE_CONFIG_DIR`, which Claude Code itself
/// honours in place of `~/.claude`.
pub fn default_candidates(harness: Harness, home: &Path, claude_config_dir: Option<&Path>) -> Vec<PathBuf> {
    match harness {
        Harness::ClaudeCode => match claude_config_dir {
            Some(dir) => vec![dir.join("projects")],
            None => vec![home.join(".claude").join("projects")],
        },
        Harness::Antigravity => vec![
            // The desktop app.
            home.join(".gemini").join("antigravity").join("brain"),
            // The CLI.
            home.join(".gemini").join("antigravity-cli").join("brain"),
        ],
        Harness::KimiCode => vec![home.join(".kimi-code").join("sessions")],
    }
}

/// Resolve the scan list against this machine: its home directory and
/// `$CLAUDE_CONFIG_DIR`.
pub fn resolve(overrides: &Overrides) -> Vec<SessionDir> {
    let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    resolve_with(overrides, home_dir().as_deref(), claude_config_dir.as_deref())
}

/// [`resolve`] with the machine-dependent inputs passed in, for tests.
pub fn resolve_with(overrides: &Overrides, home: Option<&Path>, claude_config_dir: Option<&Path>) -> Vec<SessionDir> {
    let mut dirs = Vec::new();
    for harness in Harness::ALL {
        match overrides.get(harness) {
            Some(path) if path.as_os_str().is_empty() => {}
            Some(path) => dirs.push(SessionDir {
                harness,
                path: expand_home_with(path, home),
                origin: Origin::Config,
            }),
            None => {
                let Some(home) = home else { continue };
                for path in default_candidates(harness, home, claude_config_dir) {
                    if path.is_dir() {
                        dirs.push(SessionDir { harness, path, origin: Origin::Default });
                    }
                }
            }
        }
    }
    dirs
}

/// One line saying what will be scanned, e.g.
/// `claude ~/.claude/projects (default); kimi ~/k (config, missing)`.
pub fn describe(dirs: &[SessionDir]) -> String {
    if dirs.is_empty() {
        return "no session directories: none configured and no app default found \
                (set claude_projects_dir, antigravity_brain_dir or kimi_sessions_dir in launch_config.toml)"
            .to_string();
    }
    let home = home_dir();
    dirs.iter()
        .map(|d| {
            let origin = match d.origin {
                Origin::Config => "config",
                Origin::Default => "default",
            };
            let missing = if d.path.is_dir() { "" } else { ", missing" };
            format!("{} {} ({origin}{missing})", d.harness.label(), contract_home(&d.path, home.as_deref()))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The user's home directory: `$HOME`, else `%USERPROFILE%` (Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Expand a leading `~` or `~/` to the home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    expand_home_with(path, home_dir().as_deref())
}

fn expand_home_with(path: &Path, home: Option<&Path>) -> PathBuf {
    let s = path.to_string_lossy();
    let Some(home) = home else { return path.to_path_buf() };
    if s == "~" {
        return home.to_path_buf();
    }
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => path.to_path_buf(),
    }
}

fn contract_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("glyph-session-dirs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn defaults_keep_only_existing_locations_in_harness_order() {
        let home = temp_home("defaults");
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli/brain")).unwrap();
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();

        let dirs = resolve_with(&Overrides::default(), Some(&home), None);
        assert_eq!(
            dirs,
            vec![
                SessionDir { harness: Harness::ClaudeCode, path: home.join(".claude/projects"), origin: Origin::Default },
                SessionDir {
                    harness: Harness::Antigravity,
                    path: home.join(".gemini/antigravity-cli/brain"),
                    origin: Origin::Default,
                },
            ]
        );
        assert!(describe(&dirs).starts_with("claude "));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn config_wins_per_harness_and_empty_disables() {
        let home = temp_home("config");
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        std::fs::create_dir_all(home.join(".kimi-code/sessions")).unwrap();

        let overrides = Overrides {
            claude: Some(PathBuf::from("~/elsewhere")),
            antigravity: None,
            kimi: Some(PathBuf::new()),
        };
        let dirs = resolve_with(&overrides, Some(&home), None);
        // Claude: the configured path only (kept although missing), not the default.
        // Antigravity: no default exists. Kimi: disabled although its default exists.
        assert_eq!(
            dirs,
            vec![SessionDir { harness: Harness::ClaudeCode, path: home.join("elsewhere"), origin: Origin::Config }]
        );
        assert!(describe(&dirs).contains("(config, missing)"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn claude_config_dir_replaces_dot_claude() {
        let home = temp_home("ccd");
        let ccd = home.join("cc");
        std::fs::create_dir_all(ccd.join("projects")).unwrap();
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let dirs = resolve_with(&Overrides::default(), Some(&home), Some(&ccd));
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, ccd.join("projects"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_home_means_no_defaults_and_an_explaining_report() {
        let dirs = resolve_with(&Overrides::default(), None, None);
        assert!(dirs.is_empty());
        assert!(describe(&dirs).contains("kimi_sessions_dir"));
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/h");
        assert_eq!(expand_home_with(Path::new("~"), Some(home)), PathBuf::from("/h"));
        assert_eq!(expand_home_with(Path::new("~/a/b"), Some(home)), PathBuf::from("/h/a/b"));
        assert_eq!(expand_home_with(Path::new("/abs"), Some(home)), PathBuf::from("/abs"));
        assert_eq!(expand_home_with(Path::new("~/a"), None), PathBuf::from("~/a"));
        assert_eq!(contract_home(Path::new("/h/x"), Some(home)), "~/x");
    }
}
