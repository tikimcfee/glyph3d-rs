//! Running gomuxai's `agent` CLI: the one way this renderer learns what
//! agent sessions exist (`agent sessions --json`) and what one of them did
//! (`agent transcript <session>`, read by [`super::events`]).
//!
//! The binary is `agent` on PATH unless the launch config names one
//! (`agent_binary`). Where gomuxai looks for each harness's sessions is its
//! own table; a launch config that sets `claude_projects_dir`,
//! `agy_brain_dir` or `kimi_sessions_dir` has them written into a gomuxai
//! app config (JSON, the same three keys, `""` turning a harness off) that
//! every call passes as `--backend-config`.
//!
//! A binary that can't be run, or a call that exits nonzero, is an error
//! naming the command and what it printed on stderr — never an empty list.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

use super::events::{AgentSession, SessionInfo};
use crate::launch_config::LaunchConfig;

/// How to run gomuxai's `agent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCli {
    /// The binary: a bare name is looked up on PATH.
    pub program: PathBuf,
    /// A gomuxai app config naming session directories, when the launch
    /// config sets any.
    pub app_config: Option<PathBuf>,
}

impl Default for AgentCli {
    fn default() -> Self {
        Self::new("agent")
    }
}

impl AgentCli {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), app_config: None }
    }

    /// The `agent` a launch config names (else `agent` on PATH), with its
    /// session directories, if it sets any, written into a gomuxai app
    /// config in the temp dir, named by its content's hash so two
    /// renderers with different configs never share one.
    pub fn from_launch_config(cfg: &LaunchConfig) -> Result<Self, String> {
        let mut cli = Self::new(cfg.agent_binary.clone().unwrap_or_else(|| PathBuf::from("agent")));
        let mut dirs = serde_json::Map::new();
        for (key, dir) in [
            ("claude_projects_dir", &cfg.claude_projects_dir),
            ("agy_brain_dir", &cfg.agy_brain_dir),
            ("kimi_sessions_dir", &cfg.kimi_sessions_dir),
        ] {
            if let Some(dir) = dir {
                // As written: gomuxai expands a leading `~` itself.
                dirs.insert(key.to_string(), serde_json::Value::String(dir.to_string_lossy().into_owned()));
            }
        }
        if dirs.is_empty() {
            return Ok(cli);
        }
        let json = serde_json::to_string_pretty(&serde_json::Value::Object(dirs)).expect("a JSON map serializes");
        let hash = json.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
        let path = std::env::temp_dir().join(format!("glyph3d-gomuxai-{hash:016x}.json"));
        std::fs::write(&path, json)
            .map_err(|e| format!("could not write the gomuxai app config {}: {e}", path.display()))?;
        cli.app_config = Some(path);
        Ok(cli)
    }

    /// The command for `args`, with the app config when there is one
    /// (gomuxai's global flags go before the subcommand).
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.program);
        if let Some(cfg) = &self.app_config {
            cmd.arg("--backend-config").arg(cfg);
        }
        cmd.args(args);
        cmd
    }

    /// The command line, as a person would type it.
    pub fn describe(&self, args: &[&str]) -> String {
        let mut words = vec![self.program.display().to_string()];
        if let Some(cfg) = &self.app_config {
            words.push("--backend-config".into());
            words.push(cfg.display().to_string());
        }
        words.extend(args.iter().map(|a| a.to_string()));
        words.join(" ")
    }

    /// Run `agent <args>` to completion: its stdout, or an error naming the
    /// command and its stderr.
    fn run(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let line = self.describe(args);
        let out = self.command(args).output().map_err(|e| {
            format!("could not run `{line}`: {e} (gomuxai's `agent` must be on PATH, or set agent_binary in launch_config.toml)")
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("`{line}` failed ({}): {}", out.status, stderr.trim()));
        }
        Ok(out.stdout)
    }

    /// Every agent session gomuxai can see on this host, newest first.
    pub fn sessions(&self) -> Result<Vec<SessionInfo>, String> {
        let args = ["sessions", "--json"];
        let stdout = self.run(&args)?;
        serde_json::from_slice(&stdout)
            .map_err(|e| format!("`{}` printed something that is not a session list: {e}", self.describe(&args)))
    }

    /// One session's events: `session` is a session id (or the start of
    /// only one), a workgroup agent's name, or a transcript's path.
    pub fn transcript(&self, session: &str) -> Result<AgentSession, String> {
        if session.trim().is_empty() {
            return Err("no agent session named: give a session id, an agent's name or a transcript path".into());
        }
        let args = ["transcript", session];
        let stdout = self.run(&args)?;
        AgentSession::read_stream(stdout.as_slice()).map_err(|e| format!("`{}`: {e}", self.describe(&args)))
    }
}

/// The sessions a listing shows: those with a transcript (a workgroup agent
/// that hasn't written one has nothing to open), each top-level session
/// followed by its subagents, as `agent sessions` prints them. A subagent
/// whose parent isn't listed stands on its own.
pub fn nest_subagents(sessions: &[SessionInfo]) -> Vec<(&SessionInfo, Vec<&SessionInfo>)> {
    let openable: Vec<&SessionInfo> = sessions.iter().filter(|s| !s.path.is_empty()).collect();
    let listed = |id: &str, harness: &str| openable.iter().any(|s| s.session_id == id && s.harness == harness);
    let mut out: Vec<(&SessionInfo, Vec<&SessionInfo>)> = Vec::new();
    for s in &openable {
        match &s.parent_session {
            Some(p) if listed(&p.session_id, &s.harness) => {}
            _ => out.push((s, Vec::new())),
        }
    }
    for s in &openable {
        if let Some(p) = &s.parent_session {
            if let Some((_, subs)) = out
                .iter_mut()
                .find(|(top, _)| top.session_id == p.session_id && top.harness == s.harness)
            {
                subs.push(s);
            }
        }
    }
    out
}

/// An RFC 3339 time (gomuxai's `mtime`: `2026-10-26T16:38:34.551436199Z`,
/// or with a `+hh:mm` offset) as a SystemTime; None if it isn't one.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
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
        // Nanoseconds from at most nine digits; finer ones are dropped.
        let frac = &s[i + 1..i + 1 + digits.min(9)];
        nanos = frac.parse::<u32>().ok()? * 10u32.pow(9 - frac.len() as u32);
        i += 1 + digits;
    }
    let offset_secs = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if b.len() == i + 6 && b[i + 3] == b':' => {
            let off = num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60;
            if *sign == b'+' { off } else { -off }
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
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
    let secs = u64::try_from(secs).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos))
}
