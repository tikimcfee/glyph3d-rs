//! Discovery and Indexing of Agent Sessions across local and global environments.
//!
//! Scans the session directories the launch config resolves to (see
//! `glyph-session-dirs`: a configured path per harness, else that app's
//! default locations on this machine) for Claude Code, Antigravity and Kimi
//! Code sessions, extracting lightweight preview metadata (ID, prompt title,
//! timestamp, harness kind, project scope). No working-directory probe.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::launch_config::LaunchConfig;
use glyph_session_dirs::{Harness, SessionDir};
use super::types::HarnessKind;

/// Lightweight summary metadata of a discovered agent session on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredSession {
    /// Session or conversation ID (e.g. UUID).
    pub id: String,
    /// Detected harness format.
    pub harness: HarnessKind,
    /// Absolute or canonical path to the session transcript JSONL file.
    pub path: PathBuf,
    /// Human-readable title or prompt preview.
    pub title: String,
    /// The working directory the PROVIDER recorded for this session (Claude
    /// Code's `cwd`, Antigravity's first `workspaceUris` entry or `cwd`, Kimi
    /// Code's `state.json` `cwd`), as written. Never read from where the file
    /// happens to live on disk: storage layouts are each app's business.
    pub workspace: Option<String>,
    /// The workspace's last path component — [`project_name_of`].
    pub project_name: Option<String>,
    /// Last modification timestamp on disk.
    pub modified: Option<SystemTime>,
    /// File size in bytes.
    pub file_size_bytes: u64,
}

/// A session's project, by one rule for every provider: the last component
/// of the workspace it recorded (`file://` URIs included). Until 2026-10-09
/// Claude Code sessions were named from their STORAGE folder's slug, split on
/// `-` (`…-glyph3d-js` showed as `js`), and that guess pre-empted the `cwd`
/// the session itself records (C9).
pub fn project_name_of(workspace: &str) -> Option<String> {
    let path = workspace.strip_prefix("file://").unwrap_or(workspace);
    Path::new(path.trim_end_matches('/'))
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Filter criteria for the agent session browser UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SessionHarnessFilter {
    #[default]
    All,
    ClaudeCode,
    Antigravity,
    KimiCode,
}

/// Scan the directories `config` resolves to for agent sessions.
pub fn scan_agent_sessions(config: &LaunchConfig) -> Vec<DiscoveredSession> {
    scan_session_dirs(&config.session_dirs())
}

/// Scan an explicit directory list, newest session first. Each directory is
/// read with its harness's layout; one that does not exist adds nothing.
pub fn scan_session_dirs(dirs: &[SessionDir]) -> Vec<DiscoveredSession> {
    let mut sessions = Vec::new();
    for dir in dirs.iter().filter(|d| d.path.is_dir()) {
        match dir.harness {
            Harness::ClaudeCode => scan_claude_projects_dir(&dir.path, &mut sessions),
            Harness::Antigravity => scan_antigravity_brain_dir(&dir.path, &mut sessions),
            Harness::KimiCode => scan_kimi_sessions_dir(&dir.path, &mut sessions),
        }
    }

    // Deduplicate by canonical path
    sessions.sort_by(|a, b| a.path.cmp(&b.path));
    sessions.dedup_by(|a, b| a.path == b.path);

    // Sort by modification time descending (newest sessions first)
    sessions.sort_by(|a, b| {
        match (a.modified, b.modified) {
            (Some(ta), Some(tb)) => tb.cmp(&ta),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        }
    });

    sessions
}

/// Scan a Claude Code projects root directory.
fn scan_claude_projects_dir(root: &Path, out: &mut Vec<DiscoveredSession>) {
    let Ok(entries) = fs::read_dir(root) else { return };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(files) = fs::read_dir(&path) {
                for file_entry in files.flatten() {
                    let file_path = file_entry.path();
                    if file_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                        if let Some(session) = probe_claude_session(&file_path) {
                            out.push(session);
                        }
                    }
                }
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(session) = probe_claude_session(&path) {
                out.push(session);
            }
        }
    }
}

/// Fast metadata probe for a single Claude Code session.
fn probe_claude_session(file_path: &Path) -> Option<DiscoveredSession> {
    let metadata = fs::metadata(file_path).ok()?;
    let file_size_bytes = metadata.len();
    if file_size_bytes == 0 {
        return None;
    }
    let modified = metadata.modified().ok();
    let file = fs::File::open(file_path).ok()?;
    let reader = BufReader::new(file);

    let id = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut title = None;
    let mut workspace: Option<String> = None;

    // Title from the first 35 lines; the recorded cwd may come later (a
    // session can open with dozens of snapshot and mode lines), up to 200.
    for (i, line_res) in reader.lines().take(200).enumerate() {
        let Ok(line) = line_res else { continue };
        if i >= 35 && workspace.is_some() {
            break;
        }
        let titling = i < 35;

        // 1. Look for explicit ai-title
        if titling && (line.contains("\"ai-title\"") || line.contains("\"aiTitle\"")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(t) = val.get("aiTitle").and_then(|v| v.as_str()) {
                    if !t.trim().is_empty() {
                        title = Some(t.trim().to_string());
                        if workspace.is_some() {
                            break;
                        }
                    }
                }
            }
        }

        // 2. The working directory the session recorded
        if workspace.is_none() && line.contains("\"cwd\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                workspace = val.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
            }
        }

        // 3. Fallback: prompt snippet from user message
        if titling && title.is_none() && line.contains("\"role\":\"user\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(msg) = val.get("message") {
                    if let Some(content) = msg.get("content") {
                        if let Some(s) = content.as_str() {
                            let clean = clean_preview_text(s);
                            if !clean.is_empty() {
                                title = Some(clean);
                            }
                        } else if let Some(arr) = content.as_array() {
                            for item in arr {
                                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                                    let clean = clean_preview_text(t);
                                    if !clean.is_empty() {
                                        title = Some(clean);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let final_title = title.unwrap_or_else(|| {
        let prefix = crate::agent_transcript::types::truncate_chars(&id, 8);
        format!("Session {prefix}")
    });

    Some(DiscoveredSession {
        id,
        harness: HarnessKind::ClaudeCode,
        path: file_path.to_path_buf(),
        title: final_title,
        project_name: workspace.as_deref().and_then(project_name_of),
        workspace,
        modified,
        file_size_bytes,
    })
}

/// Scan a Kimi Code sessions root: `<root>/wd_<project>_<hash>/session_<uuid>/`,
/// each with `state.json` and the main agent's `agents/main/wire.jsonl`.
fn scan_kimi_sessions_dir(root: &Path, out: &mut Vec<DiscoveredSession>) {
    let Ok(workspaces) = fs::read_dir(root) else { return };
    for workspace in workspaces.flatten() {
        let Ok(session_dirs) = fs::read_dir(workspace.path()) else { continue };
        for session_dir in session_dirs.flatten() {
            let wire = session_dir.path().join("agents").join("main").join("wire.jsonl");
            let Ok(metadata) = fs::metadata(&wire) else { continue };
            if metadata.len() == 0 {
                continue;
            }
            let id = super::kimi::session_id_for(&wire).unwrap_or_else(|| "unknown".to_string());
            let (title, cwd) = super::kimi::read_state(&session_dir.path().join("state.json"));
            let project_name = cwd.as_deref().and_then(project_name_of);
            let title = title.map(|t| clean_preview_text(&t)).filter(|t| !t.is_empty()).unwrap_or_else(|| {
                format!("Session {}", crate::agent_transcript::types::truncate_chars(&id, 8))
            });
            out.push(DiscoveredSession {
                id,
                harness: HarnessKind::KimiCode,
                path: wire,
                title,
                project_name,
                workspace: cwd,
                modified: metadata.modified().ok(),
                file_size_bytes: metadata.len(),
            });
        }
    }
}

/// Scan an Antigravity brain root directory.
fn scan_antigravity_brain_dir(root: &Path, out: &mut Vec<DiscoveredSession>) {
    let Ok(entries) = fs::read_dir(root) else { return };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let conv_id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if conv_id.is_empty() {
            continue;
        }

        // Transcript is at <conv_id>/.system_generated/logs/transcript.jsonl
        // or fallback at <conv_id>/transcript.jsonl
        let log_file = path.join(".system_generated/logs/transcript.jsonl");
        let transcript_path = if log_file.is_file() {
            log_file
        } else {
            let alt = path.join("transcript.jsonl");
            if alt.is_file() {
                alt
            } else {
                continue;
            }
        };

        if let Some(session) = probe_antigravity_session(&conv_id, &transcript_path) {
            out.push(session);
        }
    }
}

/// Fast metadata probe for an Antigravity session.
fn probe_antigravity_session(conv_id: &str, file_path: &Path) -> Option<DiscoveredSession> {
    let metadata = fs::metadata(file_path).ok()?;
    let file_size_bytes = metadata.len();
    if file_size_bytes == 0 {
        return None;
    }
    let modified = metadata.modified().ok();
    let file = fs::File::open(file_path).ok()?;
    let reader = BufReader::new(file);

    let mut title = None;
    let mut workspace: Option<String> = None;

    // The Antigravity CLI usually records no workspace at all (measured
    // 2026-10-09: 9 of 10 local sessions; the tenth only inside a tool's
    // output, describing another session). Such a session has no project,
    // rather than one guessed from paths in its tool output.
    for line_res in reader.lines().take(30) {
        let Ok(line) = line_res else { continue };

        if title.is_none() && line.contains("\"USER_INPUT\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                    let clean = clean_preview_text(content);
                    if !clean.is_empty() {
                        title = Some(clean);
                    }
                }
            }
        }

        if workspace.is_none() && (line.contains("\"workspaceUris\"") || line.contains("\"cwd\"")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                workspace = match val.get("workspaceUris").and_then(|v| v.as_array()) {
                    Some(uris) => uris.first().and_then(|u| u.as_str()).map(str::to_string),
                    None => val.get("cwd").and_then(|v| v.as_str()).map(str::to_string),
                };
            }
        }

        if title.is_some() && workspace.is_some() {
            break;
        }
    }

    let final_title = title.unwrap_or_else(|| {
        let prefix = crate::agent_transcript::types::truncate_chars(conv_id, 8);
        format!("Conversation {prefix}")
    });

    Some(DiscoveredSession {
        id: conv_id.to_string(),
        harness: HarnessKind::Antigravity,
        path: file_path.to_path_buf(),
        title: final_title,
        project_name: workspace.as_deref().and_then(project_name_of),
        workspace,
        modified,
        file_size_bytes,
    })
}

/// Clean up preview text by removing XML tags and excess whitespace.
fn clean_preview_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;

    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            if ch == '\n' || ch == '\r' || ch == '\t' {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            } else {
                out.push(ch);
            }
        }
    }

    let trimmed = out.trim();
    if trimmed.chars().count() > 80 {
        let prefix: String = trimmed.chars().take(77).collect();
        format!("{prefix}...")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_preview_text() {
        let raw = "<USER_REQUEST>\n  Heya! We're picking up here in the main worktree.  \n</USER_REQUEST>";
        assert_eq!(clean_preview_text(raw), "Heya! We're picking up here in the main worktree.");

        let long = "This is a very long prompt sentence that goes on and on and on and should be truncated cleanly after eighty characters for compact preview.";
        let cleaned = clean_preview_text(long);
        assert!(cleaned.ends_with("..."));
        assert!(cleaned.chars().count() <= 80);
    }

    #[test]
    fn test_probe_claude_with_ai_title() {
        // Stored under a slugged folder whose last `-` segment is NOT the
        // project: the name must come from the cwd the session recorded,
        // even when the ai-title line comes first (C9).
        let dir = std::env::temp_dir().join("test_claude_scan").join("-home-u-dev-glyph3d-js");
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("session_1.jsonl");
        fs::write(
            &file,
            "{\"type\":\"mode\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Fix compiler warnings\"}\n{\"type\":\"user\",\"cwd\":\"/home/u/dev/glyph3d-js\"}\n",
        )
        .unwrap();

        let s = probe_claude_session(&file).unwrap();
        assert_eq!(s.id, "session_1");
        assert_eq!(s.title, "Fix compiler warnings");
        assert_eq!(s.workspace.as_deref(), Some("/home/u/dev/glyph3d-js"));
        assert_eq!(s.project_name.as_deref(), Some("glyph3d-js"));
        assert_eq!(s.harness, HarnessKind::ClaudeCode);

        // No recorded cwd: no project, rather than a guess from the folder.
        let bare = dir.join("session_2.jsonl");
        fs::write(&bare, "{\"type\":\"ai-title\",\"aiTitle\":\"t\"}\n").unwrap();
        assert_eq!(probe_claude_session(&bare).unwrap().project_name, None);

        let _ = fs::remove_dir_all(std::env::temp_dir().join("test_claude_scan"));
    }

    #[test]
    fn project_is_the_workspace_last_component_for_every_provider() {
        assert_eq!(project_name_of("/home/u/dev/glyph3d-js").as_deref(), Some("glyph3d-js"));
        assert_eq!(project_name_of("/home/u/dev/repo/").as_deref(), Some("repo"));
        assert_eq!(project_name_of("file:///Users/u/src/repo-native").as_deref(), Some("repo-native"));
        assert_eq!(project_name_of("/"), None);
        assert_eq!(project_name_of(""), None);
    }

    #[test]
    fn test_probe_antigravity_session() {
        let dir = std::env::temp_dir().join("test_agy_scan").join("conv_123");
        let logs_dir = dir.join(".system_generated/logs");
        let _ = fs::create_dir_all(&logs_dir);
        let file = logs_dir.join("transcript.jsonl");
        fs::write(
            &file,
            "{\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>Refactor carrel HUD</USER_REQUEST>\",\"workspaceUris\":[\"file:///workspace/repo-native\"]}\n",
        ).unwrap();

        let s = probe_antigravity_session("conv_123", &file).unwrap();
        assert_eq!(s.id, "conv_123");
        assert_eq!(s.title, "Refactor carrel HUD");
        assert_eq!(s.project_name.as_deref(), Some("repo-native"));
        assert_eq!(s.harness, HarnessKind::Antigravity);

        let _ = fs::remove_dir_all(std::env::temp_dir().join("test_agy_scan"));
    }

    #[test]
    fn test_scan_agent_sessions_all_disabled_scans_nothing() {
        // An empty value turns a harness off, app default or not, so this
        // holds whatever this machine's home directory contains.
        let config = LaunchConfig {
            claude_projects_dir: Some(PathBuf::new()),
            antigravity_brain_dir: Some(PathBuf::new()),
            kimi_sessions_dir: Some(PathBuf::new()),
            ..LaunchConfig::default()
        };
        assert!(config.session_dirs().is_empty());
        assert!(scan_agent_sessions(&config).is_empty());
    }

    #[test]
    fn test_scan_kimi_sessions_dir() {
        let root = std::env::temp_dir().join(format!("test_kimi_scan_{}", std::process::id()));
        let session = root.join("wd_proj_abc/session_5678-ef");
        let _ = fs::create_dir_all(session.join("agents/main"));
        fs::write(session.join("state.json"), r#"{"title":"Kimi Task","workDir":"/src/proj"}"#).unwrap();
        fs::write(session.join("agents/main/wire.jsonl"), "{\"type\":\"metadata\"}\n").unwrap();

        let sessions = scan_session_dirs(&[SessionDir {
            harness: Harness::KimiCode,
            path: root.clone(),
            origin: glyph_session_dirs::Origin::Config,
        }]);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!((s.id.as_str(), s.title.as_str()), ("5678-ef", "Kimi Task"));
        assert_eq!(s.project_name.as_deref(), Some("proj"));
        assert_eq!(s.harness, HarnessKind::KimiCode);
        assert!(s.path.ends_with("agents/main/wire.jsonl"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_scan_agent_sessions_configured_dirs() {
        let temp_root = std::env::temp_dir().join("test_configured_scan");
        let claude_root = temp_root.join("claude");
        let agy_root = temp_root.join("agy");

        let claude_proj = claude_root.join("my-project");
        let _ = fs::create_dir_all(&claude_proj);
        fs::write(claude_proj.join("sess_c.jsonl"), "{\"type\":\"aiTitle\",\"aiTitle\":\"Claude Task\"}\n").unwrap();

        let agy_conv = agy_root.join("conv_a").join(".system_generated/logs");
        let _ = fs::create_dir_all(&agy_conv);
        fs::write(agy_conv.join("transcript.jsonl"), "{\"type\":\"USER_INPUT\",\"content\":\"Agy Task\"}\n").unwrap();

        let config = LaunchConfig {
            claude_projects_dir: Some(claude_root),
            antigravity_brain_dir: Some(agy_root),
            // Off, not unset: unset falls back to this machine's real Kimi
            // default, and the test must not depend on what this machine holds.
            kimi_sessions_dir: Some(PathBuf::new()),
            ..LaunchConfig::default()
        };

        let sessions = scan_agent_sessions(&config);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.title == "Claude Task" && s.harness == HarnessKind::ClaudeCode));
        assert!(sessions.iter().any(|s| s.title == "Agy Task" && s.harness == HarnessKind::Antigravity));

        let _ = fs::remove_dir_all(temp_root);
    }
}
