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

/// Extract a `FileActionRecord` from an Antigravity tool call.
pub fn extract_antigravity_file_action(
    tool_id: &str,
    name: &str,
    args: &Value,
    output: Option<&str>,
) -> Option<FileActionRecord> {
    let (action, file_path) = match name {
        "replace_file_content" => {
            let path = args.get("TargetFile").and_then(|v| v.as_str())?;
            (FileActionKind::Edit, path.to_string())
        }
        "write_to_file" => {
            let path = args.get("TargetFile").and_then(|v| v.as_str())?;
            (FileActionKind::Write, path.to_string())
        }
        "view_file" => {
            let path = args.get("AbsolutePath").and_then(|v| v.as_str())?;
            (FileActionKind::Read, path.to_string())
        }
        _ => return None,
    };

    let mut old_content = None;
    let mut new_content = None;
    let mut original_file = None;
    let mut hunks = Vec::new();

    match action {
        FileActionKind::Edit => {
            if let Some(target) = args.get("TargetContent").and_then(|v| v.as_str()) {
                old_content = Some(target.to_string());
            }
            if let Some(rep) = args.get("ReplacementContent").and_then(|v| v.as_str()) {
                new_content = Some(rep.to_string());
            }
            let start = args.get("StartLine").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let end = args.get("EndLine").and_then(|v| v.as_u64()).unwrap_or(start as u64) as usize;
            hunks.push(DiffHunkRecord {
                old_start: start,
                old_lines: end.saturating_sub(start) + 1,
                new_start: start,
                new_lines: end.saturating_sub(start) + 1,
                lines: Vec::new(),
            });
        }
        FileActionKind::Write => {
            if let Some(code) = args.get("CodeContent").and_then(|v| v.as_str()) {
                new_content = Some(code.to_string());
            }
        }
        FileActionKind::Read => {
            if let Some(out) = output {
                new_content = Some(out.to_string());
                original_file = Some(out.to_string());
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
