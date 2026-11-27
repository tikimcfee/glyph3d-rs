//! Locating and invoking gomuxai's `agent` CLI (`~/.local/bin/agent` or `PATH`), writing optional
//! `--backend-config` session-directory overrides, and parsing RFC 3339 timestamps.
//!
//! Mirrors `native/src/agent_transcript/agent_cli.rs` and `native/src/launch_config.rs` without
//! depending on the `native` crate, while also falling back to `~/.local/bin/agent` when a GUI
//! process is launched without `~/.local/bin` on `PATH`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Launch-config-equivalent settings for locating `agent` and configuring its session scan dirs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCliConfig {
    /// Explicit path to gomuxai's `agent` binary (`~` is expanded). When `None`, [`AgentCli`]
    /// searches `PATH` first and then `~/.local/bin/agent`.
    pub agent_binary: Option<PathBuf>,
    /// Override for Claude Code's projects directory (`""` disables the harness).
    pub claude_projects_dir: Option<PathBuf>,
    /// Override for Antigravity's brain directory (`""` disables the harness).
    pub agy_brain_dir: Option<PathBuf>,
    /// Override for Kimi Code's sessions directory (`""` disables the harness).
    pub kimi_sessions_dir: Option<PathBuf>,
}

/// How to run gomuxai's `agent` binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCli {
    /// Resolved executable path or bare program name.
    pub program: PathBuf,
    /// Optional `--backend-config` JSON file naming session directories.
    pub app_config: Option<PathBuf>,
}

impl Default for AgentCli {
    fn default() -> Self {
        Self::discover()
    }
}

impl AgentCli {
    /// Construct an [`AgentCli`] for an explicit binary path or name, expanding a leading `~`.
    pub fn new(program: impl AsRef<Path>) -> Self {
        Self {
            program: expand_home(program.as_ref()),
            app_config: None,
        }
    }

    /// Discover `agent` on `PATH`, falling back to `~/.local/bin/agent` if it exists there, and
    /// finally bare `"agent"` so command failure diagnostics still name `agent`.
    pub fn discover() -> Self {
        Self {
            program: find_agent_binary(),
            app_config: None,
        }
    }

    /// Build an [`AgentCli`] from [`AgentCliConfig`], writing any configured session directories
    /// into a content-hashed gomuxai `--backend-config` JSON file in the system temp dir.
    pub fn from_config(cfg: &AgentCliConfig) -> Result<Self, String> {
        let cli = match &cfg.agent_binary {
            Some(bin) => Self::new(bin),
            None => Self::discover(),
        };
        cli.with_session_dirs(
            cfg.claude_projects_dir.as_deref(),
            cfg.agy_brain_dir.as_deref(),
            cfg.kimi_sessions_dir.as_deref(),
        )
    }

    /// Attach session-directory overrides as a content-hashed `--backend-config` file.
    ///
    /// Empty paths (`""`) are preserved verbatim because gomuxai treats `""` as turning that
    /// harness off, and leading `~` is kept as written so gomuxai expands it itself.
    pub fn with_session_dirs(
        mut self,
        claude_projects_dir: Option<&Path>,
        agy_brain_dir: Option<&Path>,
        kimi_sessions_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let mut dirs = serde_json::Map::new();
        for (key, dir) in [
            ("claude_projects_dir", claude_projects_dir),
            ("agy_brain_dir", agy_brain_dir),
            ("kimi_sessions_dir", kimi_sessions_dir),
        ] {
            if let Some(dir) = dir {
                dirs.insert(
                    key.to_string(),
                    serde_json::Value::String(dir.to_string_lossy().into_owned()),
                );
            }
        }
        if dirs.is_empty() {
            return Ok(self);
        }
        let json = serde_json::to_string_pretty(&serde_json::Value::Object(dirs))
            .expect("a JSON object map always serializes");
        let hash = json.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        let path = std::env::temp_dir().join(format!("glyph3d-gomuxai-{hash:016x}.json"));
        std::fs::write(&path, json).map_err(|e| {
            format!(
                "could not write the gomuxai app config {}: {e}",
                path.display()
            )
        })?;
        self.app_config = Some(path);
        Ok(self)
    }

    /// Build a [`Command`] for `args`, placing `--backend-config <file>` before the subcommand.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.program);
        if let Some(cfg) = &self.app_config {
            cmd.arg("--backend-config").arg(cfg);
        }
        cmd.args(args);
        cmd
    }

    /// Render the command line as a person would type it, for error messages.
    pub fn describe(&self, args: &[&str]) -> String {
        let mut words = vec![self.program.display().to_string()];
        if let Some(cfg) = &self.app_config {
            words.push("--backend-config".to_string());
            words.push(cfg.display().to_string());
        }
        words.extend(args.iter().map(|a| (*a).to_string()));
        words.join(" ")
    }

    /// Run `agent <args>` to completion, returning stdout or a descriptive error naming stderr.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let line = self.describe(args);
        let mut cmd = self.command(args);
        let out = output_command(&mut cmd).map_err(|e| {
            format!(
                "could not run `{line}`: {e} (gomuxai's `agent` must be on PATH or at ~/.local/bin/agent, or set agent_binary)"
            )
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("`{line}` failed ({}): {}", out.status, stderr.trim()));
        }
        Ok(out.stdout)
    }

    /// Query `agent sessions --dirs --json` for the per-harness directories gomuxai scans.
    pub fn dirs(&self) -> Result<Vec<SessionDir>, String> {
        let args = ["sessions", "--dirs", "--json"];
        let stdout = self.run(&args)?;
        serde_json::from_slice(&stdout).map_err(|e| {
            format!(
                "`{}` printed something that is not a directory list: {e}",
                self.describe(&args)
            )
        })
    }
}

/// Spawn `cmd`, retrying briefly on `ETXTBSY` (`ExecutableFileBusy`): when one
/// thread finishes writing an executable stub while another forks a child
/// process, the forked child briefly holds the inherited write descriptor until
/// its `execve`.
pub(crate) fn spawn_command(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    for _ in 0..10 {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(2));
            }
            res => return res,
        }
    }
    cmd.spawn()
}

fn output_command(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    for _ in 0..10 {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(2));
            }
            res => return res,
        }
    }
    cmd.output()
}

/// One directory gomuxai scans for one harness's sessions (`agent sessions --dirs --json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDir {
    pub harness: String,
    pub path: String,
    /// `"config"` (named in `--backend-config`) or `"default"` (the harness's default location).
    pub origin: String,
    pub exists: bool,
}

/// Format `dirs` as a single human-readable status line (`claude ~/.claude/projects (default); ...`).
pub fn describe_dirs(dirs: &[SessionDir]) -> String {
    if dirs.is_empty() {
        return "no session directories: none configured and no app default found (set claude_projects_dir, \
                agy_brain_dir or kimi_sessions_dir in launch_config.toml)"
            .to_string();
    }
    let home = home_dir();
    dirs.iter()
        .map(|d| {
            let path = match home
                .as_deref()
                .and_then(|h| Path::new(&d.path).strip_prefix(h).ok())
            {
                Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                Some(rest) => format!("~/{}", rest.display()),
                None => d.path.clone(),
            };
            let missing = if d.exists { "" } else { ", missing" };
            format!("{} {path} ({}{missing})", d.harness, d.origin)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Return the current user's home directory (`$HOME`, falling back to `%USERPROFILE%`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Expand a leading `~` or `~/` (`~\` on Windows) against [`home_dir`].
pub fn expand_home(path: &Path) -> PathBuf {
    expand_home_with(path, home_dir().as_deref())
}

fn expand_home_with(path: &Path, home: Option<&Path>) -> PathBuf {
    let s = path.to_string_lossy();
    let Some(home) = home else {
        return path.to_path_buf();
    };
    if s == "~" {
        return home.to_path_buf();
    }
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => path.to_path_buf(),
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Locate gomuxai's `agent` binary: first on `PATH`, then `~/.local/bin/agent`, else `"agent"`.
pub fn find_agent_binary() -> PathBuf {
    resolve_agent_binary_with(std::env::var_os("PATH").as_deref(), home_dir().as_deref())
}

pub(crate) fn resolve_agent_binary_with(
    path_var: Option<&OsStr>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(paths) = path_var {
        for dir in std::env::split_paths(paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("agent");
            if is_executable_file(&candidate) {
                return candidate;
            }
        }
    }
    if let Some(home) = home {
        let local_bin = home.join(".local/bin/agent");
        if is_executable_file(&local_bin) {
            return local_bin;
        }
    }
    PathBuf::from("agent")
}

/// Parse an RFC 3339 timestamp (`2026-10-26T16:38:34.551436199Z` or `±hh:mm`) as Unix nanoseconds
/// relative to `1970-01-01T00:00:00Z`.
pub fn parse_rfc3339_nanos(s: &str) -> Option<i128> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut nanos = 0u32;
    if b[i] == b'.' {
        let digits = s[i + 1..].bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let frac = &s[i + 1..i + 1 + digits.min(9)];
        nanos = frac.parse::<u32>().ok()? * 10u32.pow(9 - frac.len() as u32);
        i += 1 + digits;
    }
    let offset_secs = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if b.len() == i + 6 && b[i + 3] == b':' => {
            let off = num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60;
            if *sign == b'+' {
                off
            } else {
                -off
            }
        }
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(i128::from(secs) * 1_000_000_000 + i128::from(nanos))
}

/// Parse an RFC 3339 timestamp as positive Unix milliseconds (`transcriptMtime` on fleet nodes).
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let nanos = parse_rfc3339_nanos(s)?;
    let ms = nanos.div_euclid(1_000_000);
    if ms <= 0 {
        return None;
    }
    u64::try_from(ms).ok()
}

/// Parse an RFC 3339 timestamp into a [`SystemTime`] (`None` if malformed or before Unix epoch).
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let nanos = parse_rfc3339_nanos(s)?;
    if nanos < 0 {
        return None;
    }
    let secs = u64::try_from(nanos / 1_000_000_000).ok()?;
    let sub = (nanos % 1_000_000_000) as u32;
    Some(SystemTime::UNIX_EPOCH + Duration::new(secs, sub))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agent-feed-cli-{label}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_exec(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn expand_home_resolves_tilde_and_preserves_other_paths() {
        let home = Path::new("/Users/tester");
        assert_eq!(expand_home_with(Path::new("~"), Some(home)), home);
        assert_eq!(
            expand_home_with(Path::new("~/.local/bin/agent"), Some(home)),
            PathBuf::from("/Users/tester/.local/bin/agent")
        );
        assert_eq!(
            expand_home_with(Path::new("relative/bin/agent"), Some(home)),
            PathBuf::from("relative/bin/agent")
        );
        assert_eq!(
            expand_home_with(Path::new("~/.local/bin/agent"), None),
            PathBuf::from("~/.local/bin/agent")
        );
    }

    #[test]
    fn binary_discovery_prefers_path_then_home_local_bin_then_bare_name() {
        let scratch = Scratch::new("discover");
        let path_dir = scratch.0.join("path-bin");
        let fake_home = scratch.0.join("home");
        fs::create_dir_all(&path_dir).unwrap();
        fs::create_dir_all(&fake_home).unwrap();

        // Neither PATH nor ~/.local/bin/agent has an executable -> bare "agent".
        assert_eq!(
            resolve_agent_binary_with(Some(path_dir.as_os_str()), Some(&fake_home)),
            PathBuf::from("agent")
        );

        // ~/.local/bin/agent present -> picked when PATH misses.
        let home_agent = fake_home.join(".local/bin/agent");
        write_exec(&home_agent, "#!/bin/sh\nexit 0\n");
        assert_eq!(
            resolve_agent_binary_with(Some(path_dir.as_os_str()), Some(&fake_home)),
            home_agent
        );

        // Executable on PATH wins over ~/.local/bin/agent.
        let path_agent = path_dir.join("agent");
        write_exec(&path_agent, "#!/bin/sh\nexit 0\n");
        assert_eq!(
            resolve_agent_binary_with(Some(path_dir.as_os_str()), Some(&fake_home)),
            path_agent
        );
    }

    #[test]
    fn from_config_writes_backend_config_before_subcommand() {
        let cfg = AgentCliConfig {
            agent_binary: Some(PathBuf::from("/opt/gomuxai/agent")),
            claude_projects_dir: Some(PathBuf::from("~/my_claude_projects")),
            agy_brain_dir: Some(PathBuf::from("~/my_agy_brain")),
            kimi_sessions_dir: Some(PathBuf::new()),
        };
        let cli = AgentCli::from_config(&cfg).unwrap();
        assert_eq!(cli.program, PathBuf::from("/opt/gomuxai/agent"));
        let cfg_path = cli.app_config.as_ref().expect("backend config path written");
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cfg_path).unwrap()).unwrap();
        assert_eq!(
            written,
            serde_json::json!({
                "claude_projects_dir": "~/my_claude_projects",
                "agy_brain_dir": "~/my_agy_brain",
                "kimi_sessions_dir": "",
            })
        );
        let desc = cli.describe(&["sessions", "--json"]);
        let bc_idx = desc.find("--backend-config").unwrap();
        let sub_idx = desc.find("sessions --json").unwrap();
        assert!(bc_idx < sub_idx, "global flag precedes subcommand: {desc}");
    }

    #[test]
    fn dirs_command_and_formatting_work_against_stub() {
        let scratch = Scratch::new("dirs");
        let stub = scratch.0.join("agent");
        write_exec(
            &stub,
            "#!/bin/sh\nif [ \"$1\" = \"sessions\" ] && [ \"$2\" = \"--dirs\" ] && [ \"$3\" = \"--json\" ]; then\n  printf '[{\"harness\":\"claude\",\"path\":\"/tmp/c\",\"origin\":\"config\",\"exists\":true},{\"harness\":\"kimi\",\"path\":\"/tmp/k\",\"origin\":\"config\",\"exists\":false}]\\n'\n  exit 0\nfi\nexit 1\n",
        );
        let cli = AgentCli::new(&stub);
        let dirs = cli.dirs().unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(
            describe_dirs(&dirs),
            "claude /tmp/c (config); kimi /tmp/k (config, missing)"
        );
    }

    #[test]
    fn rfc3339_parsing_handles_z_offsets_and_fractional_seconds() {
        assert_eq!(
            parse_rfc3339_ms("2026-10-12T15:04:05Z"),
            Some(1_791_817_445_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-10-12T10:04:05.250-05:00"),
            Some(1_791_817_445_250)
        );
        assert!(parse_rfc3339("2026-10-26T16:38:34.551436199Z").is_some());
        assert_eq!(parse_rfc3339_ms("not-a-timestamp"), None);
    }
}
