//! Claude Code session JSONL transcript parser.
//!
//! Replicates the 2-pass pairing and response merging behavior of `glyph3d-core`'s
//! `sessionAdapter.js` (`parseClaudeSession`), mapping events into structured `AgentTurn`s.

use std::collections::HashMap;
use serde_json::Value;
use crate::spatial_scene::workdesk::FileActionKind;
use super::types::{
    AgentSession, AgentTurn, DiffHunkRecord, FileActionRecord, HarnessKind, ToolCallRecord,
};

/// Parse an ISO-8601 UTC timestamp string (e.g. `2026-08-01T10:00:00.000Z`) into epoch milliseconds.
pub fn parse_iso_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    // Expected format: YYYY-MM-DDTHH:MM:SS[.sss][Z]
    let year: i64 = s.get(0..4)?.parse().ok()?;
    if s.as_bytes().get(4)? != &b'-' {
        return None;
    }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    if s.as_bytes().get(7)? != &b'-' {
        return None;
    }
    let day: u32 = s.get(8..10)?.parse().ok()?;

    let t_sep = *s.as_bytes().get(10)?;
    if t_sep != b'T' && t_sep != b' ' {
        return None;
    }

    let hour: u32 = s.get(11..13)?.parse().ok()?;
    if s.as_bytes().get(13)? != &b':' {
        return None;
    }
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    if s.as_bytes().get(16)? != &b':' {
        return None;
    }
    let second: u32 = s.get(17..19)?.parse().ok()?;

    let mut millis: u32 = 0;
    if s.len() > 19 && s.as_bytes()[19] == b'.' {
        let rest = &s[20..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let frac_str = &rest[..end];
        if !frac_str.is_empty() {
            let num: u32 = frac_str.parse().ok()?;
            let denom = 10u32.pow(frac_str.len() as u32);
            millis = (num * 1000) / denom;
        }
    }

    // Howard Hinnant's algorithm for days from 1970-01-01
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    let epoch_secs = days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    Some(epoch_secs * 1000 + millis as i64)
}

/// Flatten a tool_result content block (string or array of text blocks) to plain text.
pub fn result_text_of(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let lines: Vec<&str> = arr
            .iter()
            .filter_map(|x| {
                if x.get("type").and_then(|t| t.as_str()) == Some("text") {
                    x.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        return lines.join("\n");
    }
    String::new()
}

/// Merge structured `toolUseResult` with plain result text.
pub fn merge_response(tur: Option<Value>, text: &str) -> Option<Value> {
    if let Some(mut obj) = tur {
        if let Some(map) = obj.as_object_mut() {
            let has_text = map.contains_key("stdout")
                || map.contains_key("content")
                || map.contains_key("result")
                || map.contains_key("output");
            if !text.is_empty() && !has_text {
                map.insert("content".to_string(), Value::String(text.to_string()));
            }
            return Some(obj);
        }
        return Some(obj);
    }
    if !text.is_empty() {
        Some(Value::String(text.to_string()))
    } else {
        None
    }
}

/// Parse structuredPatch array into a list of `DiffHunkRecord`s.
pub fn extract_diff_hunks(sp: &Value) -> Vec<DiffHunkRecord> {
    let mut hunks = Vec::new();
    if let Some(arr) = sp.as_array() {
        for h in arr {
            let old_start = h.get("oldStart").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let old_lines = h
                .get("oldLines")
                .or_else(|| h.get("oldCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let new_start = h.get("newStart").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let new_lines = h
                .get("newLines")
                .or_else(|| h.get("newCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let lines = h
                .get("lines")
                .and_then(|v| v.as_array())
                .map(|larr| {
                    larr.iter()
                        .filter_map(|l| l.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            hunks.push(DiffHunkRecord {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines,
            });
        }
    }
    hunks
}

/// Extract a `FileActionRecord` from tool input and merged response.
pub fn extract_file_action(
    tool_id: &str,
    name: &str,
    input: &Value,
    response: Option<&Value>,
    is_error: bool,
) -> Option<FileActionRecord> {
    if is_error {
        return None;
    }

    let (action, target_key) = match name {
        "Edit" | "MultiEdit" => (FileActionKind::Edit, "file_path"),
        "NotebookEdit" => (FileActionKind::Edit, "notebook_path"),
        "Write" => (FileActionKind::Write, "file_path"),
        "Read" | "View" => (FileActionKind::Read, "file_path"),
        _ => return None,
    };

    let file_path = input
        .get(target_key)
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())?
        .to_string();

    let mut old_content = None;
    let mut new_content = None;
    let mut hunks = Vec::new();

    match action {
        FileActionKind::Edit => {
            if let Some(old_s) = input.get("old_string").and_then(|v| v.as_str()) {
                old_content = Some(old_s.to_string());
            }
            if let Some(new_s) = input.get("new_string").and_then(|v| v.as_str()) {
                new_content = Some(new_s.to_string());
            }
            if let Some(resp) = response {
                if let Some(sp) = resp.get("structuredPatch") {
                    hunks = extract_diff_hunks(sp);
                }
                if old_content.is_none() {
                    if let Some(orig) = resp.get("originalFile").and_then(|v| v.as_str()) {
                        old_content = Some(orig.to_string());
                    }
                }
            }
        }
        FileActionKind::Write => {
            if let Some(c) = input.get("content").and_then(|v| v.as_str()) {
                new_content = Some(c.to_string());
            } else if let Some(resp) = response {
                if let Some(c) = resp.get("content").and_then(|v| v.as_str()) {
                    new_content = Some(c.to_string());
                }
            }
            if let Some(resp) = response {
                if let Some(orig) = resp.get("originalFile").and_then(|v| v.as_str()) {
                    old_content = Some(orig.to_string());
                }
                if let Some(sp) = resp.get("structuredPatch") {
                    hunks = extract_diff_hunks(sp);
                }
            }
        }
        FileActionKind::Read => {
            if let Some(resp) = response {
                if let Some(file_obj) = resp.get("file") {
                    if let Some(c) = file_obj.get("content").and_then(|v| v.as_str()) {
                        old_content = Some(c.to_string());
                    }
                }
            }
        }
        _ => {}
    }

    let summary = format!("{name} {file_path}");
    Some(FileActionRecord {
        tool_id: tool_id.to_string(),
        file_path,
        action,
        summary,
        old_content,
        new_content,
        hunks,
    })
}

/// Parse Claude Code session JSONL text into an `AgentSession` with ordered `AgentTurn`s.
pub fn parse_claude_session(text: &str, fallback_session_id: &str) -> AgentSession {
    let mut parsed: Vec<Value> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if !val.is_object() {
            continue;
        }
        if val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        parsed.push(val);
    }

    // Pass 1: Pair tool_result by tool_use_id
    // Map tool_use_id -> (Option<structured_tool_use_result>, result_text, is_error)
    let mut paired_results: HashMap<String, (Option<Value>, String, bool)> = HashMap::new();
    for obj in &parsed {
        let content = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());
        let Some(blocks) = content else { continue };

        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(tid) = b.get("tool_use_id").and_then(|id| id.as_str()) {
                    let text = b.get("content").map(result_text_of).unwrap_or_default();
                    let structured = obj.get("toolUseResult").cloned();
                    let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    paired_results.insert(tid.to_string(), (structured, text, is_error));
                }
            }
        }
    }

    // Pass 2: Emit events, harvest metadata, group into turns
    let mut session = AgentSession::new(HarnessKind::ClaudeCode, fallback_session_id);
    let mut current_turn = AgentTurn::new(0);
    let mut has_emitted_in_turn = false;

    let update_ts = |session: &mut AgentSession, ts: Option<i64>| {
        if let Some(t) = ts {
            if session.first_ts.is_none() {
                session.first_ts = Some(t);
            }
            session.last_ts = Some(t);
        }
    };

    for obj in &parsed {
        // Metadata harvesting (first seen wins)
        if session.cwd.is_none() {
            if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                if !cwd.is_empty() {
                    session.cwd = Some(cwd.to_string());
                }
            }
        }
        if session.slug.is_none() {
            if let Some(s) = obj.get("slug").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    session.slug = Some(s.to_string());
                }
            }
        }
        if session.version.is_none() {
            if let Some(v) = obj.get("version").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    session.version = Some(v.to_string());
                }
            }
        }
        if session.git_branch.is_none() {
            if let Some(gb) = obj.get("gitBranch").and_then(|v| v.as_str()) {
                if !gb.is_empty() {
                    session.git_branch = Some(gb.to_string());
                }
            }
        }
        if session.title.is_none() && obj.get("type").and_then(|t| t.as_str()) == Some("ai-title") {
            if let Some(t) = obj.get("aiTitle").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    session.title = Some(t.to_string());
                }
            }
        }

        let content = obj
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());
        let Some(blocks) = content else { continue };

        let role = obj
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let ts = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_iso_ts);
        update_ts(&mut session, ts);

        let assistant = role == "assistant";
        if assistant && session.model.is_none() {
            if let Some(m) = obj
                .get("message")
                .and_then(|msg| msg.get("model"))
                .and_then(|v| v.as_str())
            {
                if !m.is_empty() {
                    session.model = Some(m.to_string());
                }
            }
        }

        for b in blocks {
            let b_type = b.get("type").and_then(|t| t.as_str()).unwrap_or("");

            if role == "user" && b_type == "text" {
                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    // New user prompt begins a new turn
                    if has_emitted_in_turn {
                        session.turns.push(current_turn);
                        current_turn = AgentTurn::new(session.turns.len());
                    }
                    current_turn.prompt = Some(trimmed.to_string());
                    current_turn.timestamp = ts;
                    has_emitted_in_turn = true;
                }
            } else if assistant && b_type == "thinking" {
                if let Some(think) = b.get("thinking").and_then(|t| t.as_str()) {
                    let trimmed = think.trim();
                    if !trimmed.is_empty() {
                        current_turn.thinking.push(trimmed.to_string());
                        if current_turn.timestamp.is_none() {
                            current_turn.timestamp = ts;
                        }
                        has_emitted_in_turn = true;
                    }
                }
            } else if assistant && b_type == "text" {
                if let Some(txt) = b.get("text").and_then(|t| t.as_str()) {
                    let trimmed = txt.trim();
                    if !trimmed.is_empty() {
                        current_turn.assistant_messages.push(trimmed.to_string());
                        if current_turn.timestamp.is_none() {
                            current_turn.timestamp = ts;
                        }
                        has_emitted_in_turn = true;
                    }
                }
            } else if assistant && b_type == "tool_use" {
                let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = b.get("input").cloned().unwrap_or(Value::Object(Default::default()));

                let (structured, text, is_error) = paired_results
                    .remove(&id)
                    .unwrap_or((None, String::new(), false));

                let response = merge_response(structured, &text);

                if let Some(fa) = extract_file_action(&id, &name, &input, response.as_ref(), is_error) {
                    current_turn.file_actions.push(fa);
                }

                current_turn.tool_calls.push(ToolCallRecord {
                    id,
                    name,
                    input,
                    response,
                    is_error,
                    timestamp: ts,
                });
                if current_turn.timestamp.is_none() {
                    current_turn.timestamp = ts;
                }
                has_emitted_in_turn = true;
            }
        }
    }

    if has_emitted_in_turn {
        session.turns.push(current_turn);
    }

    session
}
