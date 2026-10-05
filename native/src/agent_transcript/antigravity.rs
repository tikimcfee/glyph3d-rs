//! Antigravity session JSONL transcript parser.
//!
//! Ingests transcript files from Antigravity agent runs
//! (`~/.gemini/antigravity/brain/<id>/.system_generated/logs/transcript.jsonl`).

use serde_json::Value;
use crate::spatial_scene::workdesk::FileActionKind;
use super::claude::parse_iso_ts;
use super::types::{
    AgentSession, AgentTurn, DiffHunkRecord, FileActionRecord, HarnessKind, ToolCallRecord,
};

/// Helper to unescape JSON string values and strip outer quotes.
pub fn clean_antigravity_string(val: &Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        // If it looks like a JSON string literal (enclosed in double quotes), try serde_json::from_str
        if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
        {
            if let Ok(decoded) = serde_json::from_str::<String>(trimmed) {
                return Some(decoded);
            }
        }
        // Fallback: if it contains escaped newlines or tabs, unescape them directly
        if trimmed.contains("\\n")
            || trimmed.contains("\\r")
            || trimmed.contains("\\t")
            || trimmed.contains("\\\"")
        {
            let mut unescaped = trimmed.to_string();
            if unescaped.starts_with('"') && unescaped.ends_with('"') && unescaped.len() >= 2 {
                unescaped = unescaped[1..unescaped.len() - 1].to_string();
            }
            let unescaped = unescaped
                .replace("\\r\\n", "\n")
                .replace("\\n", "\n")
                .replace("\\r", "\n")
                .replace("\\t", "\t")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            return Some(unescaped);
        }
        let stripped = trimmed.trim_matches('"').trim_matches('\'');
        Some(stripped.to_string())
    } else if let Some(n) = val.as_i64() {
        Some(n.to_string())
    } else {
        val.as_bool().map(|b| b.to_string())
    }
}

/// Helper to clean file paths (stripping file://, quotes, whitespace).
pub fn clean_antigravity_path(val: &Value) -> Option<String> {
    let raw = clean_antigravity_string(val)?;
    let clean = raw.trim().trim_matches('"').trim_matches('\'');
    let path_str = clean.strip_prefix("file://").unwrap_or(clean);
    if path_str.is_empty() {
        None
    } else {
        Some(path_str.to_string())
    }
}

/// Helper to parse u64 line numbers from JSON number or string.
pub fn get_antigravity_u64(args: &Value, key: &str) -> Option<u64> {
    let v = args.get(key)?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        let clean = s.trim_matches('"').trim_matches('\'').trim();
        return clean.parse::<u64>().ok();
    }
    None
}

/// Extract pristine source lines from Antigravity's view_file output.
///
/// Strips the 7-line diagnostic metadata header (`Showing lines...`, etc.)
/// and removes `<line_no>: ` prefixes so cards render actual code.
pub fn extract_code_from_view_file(output: &str) -> String {
    let mut code_lines = Vec::new();
    let mut past_header = false;

    for line in output.lines() {
        if !past_header {
            if let Some((num, _)) = line.split_once(':') {
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    past_header = true;
                }
            }
        }
        if past_header {
            if let Some((num, rest)) = line.split_once(':') {
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    let code_part = rest.strip_prefix(' ').unwrap_or(rest);
                    code_lines.push(code_part);
                    continue;
                }
            }
            code_lines.push(line);
        }
    }

    if code_lines.is_empty() {
        output.to_string()
    } else {
        code_lines.join("\n")
    }
}

/// Extract a `FileActionRecord` from an Antigravity tool call.
pub fn extract_antigravity_file_action(
    tool_id: &str,
    name: &str,
    args: &Value,
    output: Option<&str>,
) -> Option<FileActionRecord> {
    let (action, file_path) = match name {
        "replace_file_content" => {
            let path = args.get("TargetFile").and_then(clean_antigravity_path)?;
            (FileActionKind::Edit, path)
        }
        "write_to_file" => {
            let path = args.get("TargetFile").and_then(clean_antigravity_path)?;
            (FileActionKind::Write, path)
        }
        "view_file" => {
            let path = args.get("AbsolutePath").and_then(clean_antigravity_path)?;
            (FileActionKind::Read, path)
        }
        _ => return None,
    };

    let mut old_content = None;
    let mut new_content = None;
    let mut original_file = None;
    let mut hunks = Vec::new();

    match action {
        FileActionKind::Edit => {
            if let Some(target) = args.get("TargetContent").and_then(clean_antigravity_string) {
                old_content = Some(target);
            }
            if let Some(rep) = args.get("ReplacementContent").and_then(clean_antigravity_string) {
                new_content = Some(rep);
            }
            let start = get_antigravity_u64(args, "StartLine").unwrap_or(1) as usize;
            let end = get_antigravity_u64(args, "EndLine").unwrap_or(start as u64) as usize;
            hunks.push(DiffHunkRecord {
                old_start: start,
                old_lines: end.saturating_sub(start) + 1,
                new_start: start,
                new_lines: end.saturating_sub(start) + 1,
                lines: Vec::new(),
            });
        }
        FileActionKind::Write => {
            if let Some(code) = args.get("CodeContent").and_then(clean_antigravity_string) {
                new_content = Some(code);
            }
        }
        FileActionKind::Read => {
            if let Some(out) = output {
                let code = extract_code_from_view_file(out);
                new_content = Some(code.clone());
                original_file = Some(code);
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
        original_file,
        hunks,
    })
}

/// Parse Antigravity session transcript JSONL text into an `AgentSession`.
pub fn parse_antigravity_session(text: &str, session_id: &str) -> AgentSession {
    let mut session = AgentSession::new(HarnessKind::Antigravity, session_id);
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

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if !obj.is_object() {
            continue;
        }

        let step_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let step_idx = obj.get("step_index").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = obj
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(parse_iso_ts);
        update_ts(&mut session, ts);

        if session.cwd.is_none() {
            if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                if !cwd.is_empty() {
                    session.cwd = Some(cwd.to_string());
                }
            } else if let Some(uris) = obj.get("workspaceUris").and_then(|v| v.as_array()) {
                if let Some(first_uri) = uris.first().and_then(|v| v.as_str()) {
                    let clean = first_uri.strip_prefix("file://").unwrap_or(first_uri);
                    if !clean.is_empty() {
                        session.cwd = Some(clean.to_string());
                    }
                }
            }
        }

        match step_type {
            "USER_INPUT" => {
                let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    if session.cwd.is_none() && trimmed.contains("->") {
                        for l in trimmed.lines() {
                            if let Some((left, _)) = l.split_once("->") {
                                let candidate = left.trim();
                                if candidate.starts_with('/') && std::path::Path::new(candidate).is_dir() {
                                    session.cwd = Some(candidate.to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if has_emitted_in_turn {
                        session.turns.push(current_turn);
                        current_turn = AgentTurn::new(session.turns.len());
                    }
                    current_turn.prompt = Some(trimmed.to_string());
                    current_turn.timestamp = ts;
                    has_emitted_in_turn = true;
                }
            }
            "PLANNER_RESPONSE" => {
                if let Some(think) = obj.get("thinking").and_then(|v| v.as_str()) {
                    let trimmed = think.trim();
                    if !trimmed.is_empty() {
                        current_turn.thinking.push(trimmed.to_string());
                        if current_turn.timestamp.is_none() {
                            current_turn.timestamp = ts;
                        }
                        has_emitted_in_turn = true;
                    }
                }
                if let Some(msg) = obj.get("content").and_then(|v| v.as_str()) {
                    let trimmed = msg.trim();
                    if !trimmed.is_empty() {
                        current_turn.assistant_messages.push(trimmed.to_string());
                        if current_turn.timestamp.is_none() {
                            current_turn.timestamp = ts;
                        }
                        has_emitted_in_turn = true;
                    }
                }
                if let Some(tool_calls) = obj.get("tool_calls").and_then(|v| v.as_array()) {
                    for (tc_idx, tc) in tool_calls.iter().enumerate() {
                        let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args = tc.get("args").cloned().unwrap_or(Value::Object(Default::default()));
                        let tool_id = format!("step_{step_idx}_tool_{tc_idx}");

                        if let Some(fa) = extract_antigravity_file_action(&tool_id, &name, &args, None) {
                            current_turn.file_actions.push(fa);
                        }

                        current_turn.tool_calls.push(ToolCallRecord {
                            id: tool_id,
                            name,
                            input: args,
                            response: None,
                            is_error: false,
                            timestamp: ts,
                        });
                        has_emitted_in_turn = true;
                    }
                }
            }
            "GENERIC" => {
                // If previous tool call lacks response, record content here
                if let Some(last_tool) = current_turn.tool_calls.last_mut() {
                    if last_tool.response.is_none() {
                        if let Some(content) = obj.get("content") {
                            last_tool.response = Some(content.clone());

                            // If this was a view_file tool, also update the corresponding FileActionRecord
                            if last_tool.name == "view_file" {
                                if let Some(content_str) = content.as_str() {
                                    let cleaned_code = extract_code_from_view_file(content_str);
                                    if let Some(fa) = current_turn
                                        .file_actions
                                        .iter_mut()
                                        .find(|fa| fa.tool_id == last_tool.id)
                                    {
                                        fa.new_content = Some(cleaned_code.clone());
                                        fa.original_file = Some(cleaned_code);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if has_emitted_in_turn {
        session.turns.push(current_turn);
    }

    session
}
