//! Discovery and Indexing of Agent Sessions across local and global environments.
//!
//! Scans configured directories for Claude Code (`~/.claude/projects/`) and
//! Antigravity (`~/.gemini/antigravity/brain/`) sessions, extracting lightweight
//! preview metadata (ID, prompt title, timestamp, harness kind, project scope).

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::launch_config::LaunchConfig;
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
    /// Project name or repository scope (if inferrable).
    pub project_name: Option<String>,
    /// Last modification timestamp on disk.
    pub modified: Option<SystemTime>,
    /// File size in bytes.
    pub file_size_bytes: u64,
}

/// Filter criteria for the agent session browser UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SessionHarnessFilter {
    #[default]
    All,
    ClaudeCode,
    Antigravity,
}

/// Scan all configured and default directories for agent sessions.
pub fn scan_agent_sessions(config: &LaunchConfig) -> Vec<DiscoveredSession> {
    let mut sessions = Vec::new();

    // 1. Claude Code sessions
    let claude_dir = config.resolved_claude_projects_dir();
    if claude_dir.is_dir() {
        scan_claude_projects_dir(&claude_dir, &mut sessions);
    }

    // 2. Antigravity sessions
    let agy_dir = config.resolved_antigravity_brain_dir();
    if agy_dir.is_dir() {
        scan_antigravity_brain_dir(&agy_dir, &mut sessions);
    }

    // 3. Local working directory checks (e.g. ./.claude or ./.gemini)
    if let Ok(cwd) = std::env::current_dir() {
        let local_claude = cwd.join(".claude");
        if local_claude.is_dir() {
            scan_claude_projects_dir(&local_claude, &mut sessions);
        }
        let local_gemini = cwd.join(".gemini/antigravity/brain");
        if local_gemini.is_dir() {
            scan_antigravity_brain_dir(&local_gemini, &mut sessions);
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
            let project_folder = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // Clean up project name from slug like "-Users-lugo-localdev-viz-web-glyph3d-js"
            let project_hint = project_folder
                .rsplit('-')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(project_folder)
                .to_string();

            if let Ok(files) = fs::read_dir(&path) {
                for file_entry in files.flatten() {
                    let file_path = file_entry.path();
                    if file_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                        if let Some(session) = probe_claude_session(&file_path, Some(&project_hint)) {
                            out.push(session);
                        }
                    }
                }
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(session) = probe_claude_session(&path, None) {
                out.push(session);
            }
        }
    }
}

/// Fast metadata probe for a single Claude Code session.
fn probe_claude_session(file_path: &Path, project_hint: Option<&str>) -> Option<DiscoveredSession> {
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
    let mut detected_project = project_hint.map(|s| s.to_string());

    for line_res in reader.lines().take(35) {
        let Ok(line) = line_res else { continue };

        // 1. Look for explicit ai-title
        if line.contains("\"ai-title\"") || line.contains("\"aiTitle\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(t) = val.get("aiTitle").and_then(|v| v.as_str()) {
                    if !t.trim().is_empty() {
                        title = Some(t.trim().to_string());
                        break;
                    }
                }
            }
        }

        // 2. Look for cwd for project name
        if detected_project.is_none() && line.contains("\"cwd\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                    let name = Path::new(cwd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(cwd);
                    detected_project = Some(name.to_string());
                }
            }
        }

        // 3. Fallback: prompt snippet from user message
        if title.is_none() && line.contains("\"role\":\"user\"") {
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
        let prefix = if id.len() > 8 { &id[..8] } else { &id };
        format!("Session {prefix}")
    });

    Some(DiscoveredSession {
        id,
        harness: HarnessKind::ClaudeCode,
        path: file_path.to_path_buf(),
        title: final_title,
        project_name: detected_project,
        modified,
        file_size_bytes,
    })
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
    let mut project_hint = None;

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

        if project_hint.is_none() && (line.contains("\"workspaceUris\"") || line.contains("\"cwd\"")) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(uris) = val.get("workspaceUris").and_then(|v| v.as_array()) {
                    if let Some(first_uri) = uris.first().and_then(|u| u.as_str()) {
                        let name = Path::new(first_uri)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(first_uri);
                        project_hint = Some(name.to_string());
                    }
                } else if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                    let name = Path::new(cwd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(cwd);
                    project_hint = Some(name.to_string());
                }
            }
        }

        if title.is_some() && project_hint.is_some() {
            break;
        }
    }

    let final_title = title.unwrap_or_else(|| {
        let prefix = if conv_id.len() > 8 { &conv_id[..8] } else { conv_id };
        format!("Conversation {prefix}")
    });

    Some(DiscoveredSession {
        id: conv_id.to_string(),
        harness: HarnessKind::Antigravity,
        path: file_path.to_path_buf(),
        title: final_title,
        project_name: project_hint,
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
        let dir = std::env::temp_dir().join("test_claude_scan");
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("session_1.jsonl");
        fs::write(&file, "{\"type\":\"mode\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Fix compiler warnings\"}\n").unwrap();

        let s = probe_claude_session(&file, Some("my_project")).unwrap();
        assert_eq!(s.id, "session_1");
        assert_eq!(s.title, "Fix compiler warnings");
        assert_eq!(s.project_name.as_deref(), Some("my_project"));
        assert_eq!(s.harness, HarnessKind::ClaudeCode);

        let _ = fs::remove_dir_all(&dir);
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

        let mut config = LaunchConfig::default();
        config.claude_projects_dir = Some(claude_root);
        config.antigravity_brain_dir = Some(agy_root);

        let sessions = scan_agent_sessions(&config);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.title == "Claude Task" && s.harness == HarnessKind::ClaudeCode));
        assert!(sessions.iter().any(|s| s.title == "Agy Task" && s.harness == HarnessKind::Antigravity));

        let _ = fs::remove_dir_all(temp_root);
    }
}
