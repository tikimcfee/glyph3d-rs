//! Kimi Code session transcript parser.
//!
//! A Kimi Code session is a DIRECTORY, not one file:
//!
//! ```text
//! <sessions root>/wd_<project>_<hash>/session_<uuid>/
//!     state.json                 title, workDir (or cwd), timestamps
//!     agents/main/wire.jsonl     the main agent's event log  <- the transcript
//!     agents/agent-N/wire.jsonl  subagents (not read here)
//! ```
//!
//! `wire.jsonl` is an event log, one JSON object per line with a `type`.
//! The ones that carry the conversation (shapes surveyed 2026-10-09 over 96
//! logs on the Linux box; every other type is bookkeeping and is skipped):
//!
//! - `turn.prompt` / `turn.steer`: user input, `input: [{type, text}]`.
//!   A prompt opens a turn; a steer is user text mid-turn and opens one too,
//!   so it reads as its own beat.
//! - `context.append_loop_event` with `event.type` of
//!   - `content.part`: `part: {type: "think"|"text", think|text}`, whole
//!     blocks (one per step and kind), not stream deltas;
//!   - `tool.call`: `toolCallId`, `name`, `args`;
//!   - `tool.result`: `toolCallId`, `result: {output, isError?}`.
//! - `llm.request`: `model`.
//!
//! Times are epoch milliseconds, the unit every parser here produces.

use std::path::Path;

use serde_json::Value;

use super::types::{AgentSession, AgentTurn, FileActionRecord, HarnessKind, ToolCallRecord};
use crate::spatial_scene::workdesk::FileActionKind;

/// True when `content`'s first lines look like a Kimi Code `wire.jsonl`.
pub fn looks_like_kimi(content: &str) -> bool {
    content.lines().take(30).any(|line| {
        line.contains("\"context.append_loop_event\"")
            || line.contains("\"turn.prompt\"")
            || (line.contains("\"protocol_version\"") && line.contains("\"metadata\""))
    })
}

/// The session id for a `.../session_<uuid>/agents/<agent>/wire.jsonl` path:
/// the uuid. `None` when the path does not have that shape.
pub fn session_id_for(wire_path: &Path) -> Option<String> {
    let session_dir = wire_path.parent()?.parent()?.parent()?;
    let name = session_dir.file_name()?.to_str()?;
    Some(name.strip_prefix("session_").unwrap_or(name).to_string())
}

/// `state.json` beside a wire log's `agents/` directory.
pub fn state_json_for(wire_path: &Path) -> Option<std::path::PathBuf> {
    Some(wire_path.parent()?.parent()?.parent()?.join("state.json"))
}

/// Title and working directory from a session's `state.json`, when present.
pub fn read_state(state_path: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(state_path) else { return (None, None) };
    let Ok(state) = serde_json::from_str::<Value>(&text) else { return (None, None) };
    let non_empty = |v: Option<&Value>| {
        v.and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let title = non_empty(state.get("title")).or_else(|| non_empty(state.get("lastPrompt")));
    let cwd = non_empty(state.get("workDir")).or_else(|| non_empty(state.get("cwd")));
    (title, cwd)
}

/// Strip the `cat -n` style `<line>\t` prefix Kimi's Read tool puts on every
/// output line, so a card shows the code itself.
pub fn strip_read_line_numbers(output: &str) -> String {
    output
        .lines()
        .map(|line| match line.split_once('\t') {
            Some((num, rest)) if !num.trim().is_empty() && num.trim().chars().all(|c| c.is_ascii_digit()) => rest,
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn input_text(input: Option<&Value>) -> Option<String> {
    let parts = input?.as_array()?;
    let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn file_action(tool_id: &str, name: &str, args: &Value) -> Option<FileActionRecord> {
    let path = args.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty())?.to_string();
    let text = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let (action, old_content, new_content) = match name {
        "Read" => (FileActionKind::Read, None, None),
        "Write" => (FileActionKind::Write, None, text("content")),
        "Edit" => (FileActionKind::Edit, text("old_string"), text("new_string")),
        _ => return None,
    };
    Some(FileActionRecord {
        tool_id: tool_id.to_string(),
        summary: format!("{name} {path}"),
        file_path: path,
        action,
        old_content,
        new_content,
        original_file: None,
        hunks: Vec::new(),
    })
}

/// Parse a Kimi Code `wire.jsonl` into an [`AgentSession`].
pub fn parse_kimi_session(text: &str, session_id: &str) -> AgentSession {
    let mut session = AgentSession::new(HarnessKind::KimiCode, session_id);
    let mut turn = AgentTurn::new(0);
    let mut turn_has_content = false;

    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let ts = rec.get("time").or_else(|| rec.get("created_at")).and_then(|v| v.as_i64());
        if let Some(t) = ts {
            session.first_ts.get_or_insert(t);
            session.last_ts = Some(t);
        }

        match rec.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "turn.prompt" | "turn.steer" => {
                let Some(prompt) = input_text(rec.get("input")) else { continue };
                if turn_has_content {
                    let next = AgentTurn::new(session.turns.len() + 1);
                    session.turns.push(std::mem::replace(&mut turn, next));
                }
                turn.prompt = Some(prompt);
                turn.timestamp = ts;
                turn_has_content = true;
            }
            "llm.request" => {
                if session.model.is_none() {
                    session.model = rec.get("model").and_then(|v| v.as_str()).map(str::to_string);
                }
            }
            "context.append_loop_event" => {
                let Some(event) = rec.get("event") else { continue };
                match event.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "content.part" => {
                        let Some(part) = event.get("part") else { continue };
                        let kind = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let body = part.get(kind).and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
                        if body.is_empty() {
                            continue;
                        }
                        match kind {
                            "think" => turn.thinking.push(body.to_string()),
                            "text" => turn.assistant_messages.push(body.to_string()),
                            _ => continue,
                        }
                        turn.timestamp = turn.timestamp.or(ts);
                        turn_has_content = true;
                    }
                    "tool.call" => {
                        let id = event.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = event.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args = event.get("args").cloned().unwrap_or(Value::Object(Default::default()));
                        if let Some(fa) = file_action(&id, &name, &args) {
                            turn.file_actions.push(fa);
                        }
                        turn.tool_calls.push(ToolCallRecord {
                            id,
                            name,
                            input: args,
                            response: None,
                            is_error: false,
                            timestamp: ts,
                        });
                        turn_has_content = true;
                    }
                    "tool.result" => {
                        let id = event.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
                        let result = event.get("result");
                        let output = result.and_then(|r| r.get("output")).cloned();
                        let is_error = result.and_then(|r| r.get("isError")).and_then(|v| v.as_bool()).unwrap_or(false);
                        let Some(call) = turn.tool_calls.iter_mut().rev().find(|c| c.id == id) else { continue };
                        call.response = output.clone();
                        call.is_error = is_error;
                        if call.name == "Read" && !is_error {
                            if let (Some(out), Some(fa)) = (
                                output.as_ref().and_then(|v| v.as_str()),
                                turn.file_actions.iter_mut().find(|fa| fa.tool_id == id),
                            ) {
                                let code = strip_read_line_numbers(out);
                                fa.new_content = Some(code.clone());
                                fa.original_file = Some(code);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if turn_has_content {
        session.turns.push(turn);
    }
    session
}

/// Parse a Kimi `wire.jsonl` read from `wire_path`, filling the id, title and
/// working directory from the session directory around it.
pub fn parse_kimi_session_at(text: &str, wire_path: &Path, fallback_id: &str) -> AgentSession {
    let id = session_id_for(wire_path).unwrap_or_else(|| fallback_id.to_string());
    let mut session = parse_kimi_session(text, &id);
    if let Some(state) = state_json_for(wire_path) {
        let (title, cwd) = read_state(&state);
        session.title = title;
        session.cwd = cwd;
    }
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(lines: &[Value]) -> String {
        lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn loop_event(event: Value, time: i64) -> Value {
        json!({ "type": "context.append_loop_event", "event": event, "time": time })
    }

    #[test]
    fn parses_turns_parts_tools_and_file_actions() {
        let text = wire(&[
            json!({ "type": "metadata", "protocol_version": "1", "created_at": 1000 }),
            json!({ "type": "llm.request", "model": "kimi-k2", "time": 1001 }),
            json!({ "type": "turn.prompt", "input": [{ "type": "text", "text": "Fix the bug" }], "time": 1002 }),
            loop_event(json!({ "type": "content.part", "part": { "type": "think", "think": "Look first." } }), 1003),
            loop_event(json!({ "type": "tool.call", "toolCallId": "c1", "name": "Read", "args": { "path": "/r/a.rs" } }), 1004),
            loop_event(json!({ "type": "tool.result", "toolCallId": "c1", "result": { "output": "1\tfn a() {}\n2\t" } }), 1005),
            loop_event(json!({ "type": "tool.call", "toolCallId": "c2", "name": "Edit",
                "args": { "path": "/r/a.rs", "old_string": "fn a() {}", "new_string": "fn a() { b(); }" } }), 1006),
            loop_event(json!({ "type": "tool.result", "toolCallId": "c2", "result": { "output": "ok", "isError": true } }), 1007),
            loop_event(json!({ "type": "content.part", "part": { "type": "text", "text": "Done." } }), 1008),
            json!({ "type": "turn.steer", "input": [{ "type": "text", "text": "Also add a test" }], "time": 1009 }),
            loop_event(json!({ "type": "tool.call", "toolCallId": "c3", "name": "Write", "args": { "path": "/r/t.rs", "content": "#[test]" } }), 1010),
        ]);
        assert!(looks_like_kimi(&text));
        let s = parse_kimi_session(&text, "abc");

        assert_eq!(s.harness, HarnessKind::KimiCode);
        assert_eq!(s.model.as_deref(), Some("kimi-k2"));
        assert_eq!((s.first_ts, s.last_ts), (Some(1000), Some(1010)));
        assert_eq!(s.turns.len(), 2);

        let t0 = &s.turns[0];
        assert_eq!(t0.prompt.as_deref(), Some("Fix the bug"));
        assert_eq!(t0.thinking, vec!["Look first."]);
        assert_eq!(t0.assistant_messages, vec!["Done."]);
        assert_eq!(t0.tool_calls.len(), 2);
        assert!(t0.tool_calls[1].is_error);
        assert_eq!(t0.file_actions[0].action, FileActionKind::Read);
        assert_eq!(t0.file_actions[0].original_file.as_deref(), Some("fn a() {}\n"));
        assert_eq!(t0.file_actions[1].action, FileActionKind::Edit);
        assert_eq!(t0.file_actions[1].old_content.as_deref(), Some("fn a() {}"));
        assert_eq!(t0.file_actions[1].new_content.as_deref(), Some("fn a() { b(); }"));

        let t1 = &s.turns[1];
        assert_eq!((t1.turn_index, t1.prompt.as_deref()), (1, Some("Also add a test")));
        assert_eq!(t1.file_actions[0].action, FileActionKind::Write);
        assert_eq!(t1.file_actions[0].new_content.as_deref(), Some("#[test]"));
    }

    #[test]
    fn claude_and_antigravity_lines_are_not_kimi() {
        assert!(!looks_like_kimi(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#));
        assert!(!looks_like_kimi(r#"{"step_index":0,"type":"USER_INPUT","content":"hi"}"#));
    }

    #[test]
    fn session_directory_supplies_id_title_and_cwd() {
        let root = std::env::temp_dir().join(format!("glyph-kimi-{}", std::process::id()));
        let session_dir = root.join("wd_proj_0123/session_1234-abcd");
        let wire_dir = session_dir.join("agents/main");
        std::fs::create_dir_all(&wire_dir).unwrap();
        std::fs::write(session_dir.join("state.json"), r#"{"title":"Refactor","workDir":"/src/proj"}"#).unwrap();
        let wire_path = wire_dir.join("wire.jsonl");

        assert_eq!(session_id_for(&wire_path).as_deref(), Some("1234-abcd"));
        let s = parse_kimi_session_at("", &wire_path, "wire");
        assert_eq!(s.session_id, "1234-abcd");
        assert_eq!(s.title.as_deref(), Some("Refactor"));
        assert_eq!(s.cwd.as_deref(), Some("/src/proj"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
