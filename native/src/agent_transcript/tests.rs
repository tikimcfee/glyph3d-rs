//! Unit tests for agent transcript ingestion and normalization.
//!
//! Includes behavioral parity test against `tools/sessionAdapter.test.mjs`
//! from `glyph3d-js`.

use super::claude::{parse_claude_session, parse_iso_ts, user_prompt_text};
use super::antigravity::parse_antigravity_session;
use super::types::HarnessKind;
use crate::spatial_scene::workdesk::FileActionKind;
use serde_json::json;

#[test]
fn test_parse_iso_ts() {
    assert_eq!(parse_iso_ts("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(parse_iso_ts("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_iso_ts("2026-08-01T10:00:00.000Z"),
        Some(1785578400000)
    );
    assert_eq!(
        parse_iso_ts("2026-08-01T10:00:05.500Z"),
        Some(1785578405500)
    );
    assert_eq!(parse_iso_ts("not a timestamp"), None);
}

#[test]
fn test_synthetic_claude_session_parity() {
    // Exact synthetic session from glyph3d-js/tools/sessionAdapter.test.mjs
    let t = |s: u32| format!("2026-08-01T10:00:0{s}.000Z");

    let lines = vec![
        // non-message bookkeeping line → no event
        json!({ "type": "mode", "mode": "normal" }).to_string(),
        // malformed line → skipped, parse survives
        "{this is not json".to_string(),
        // sidechain line → skipped wholesale: prose, tool_use, cwd must not leak
        json!({
            "type": "assistant",
            "isSidechain": true,
            "cwd": "/side/land",
            "timestamp": t(0),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "sidechain prose" },
                    { "type": "tool_use", "id": "tu-side", "name": "Bash", "input": { "command": "rm -rf /" } }
                ]
            }
        }).to_string(),
        // thinking + text blocks, in block order; first-seen cwd
        json!({
            "type": "assistant",
            "cwd": "/main/repo",
            "timestamp": t(0),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "let me think" },
                    { "type": "text", "text": "hello world" }
                ]
            }
        }).to_string(),
        // whitespace-only text dropped; the tool_use beside it still emits
        json!({
            "type": "assistant",
            "timestamp": t(1),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "   \n  " },
                    { "type": "tool_use", "id": "tu-edit", "name": "Edit", "input": { "file_path": "/main/repo/a.js" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "assistant",
            "timestamp": t(2),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "tu-bash", "name": "Bash", "input": { "command": "ls" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "assistant",
            "timestamp": t(3),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "tu-grep", "name": "Grep", "input": { "pattern": "x" } }
                ]
            }
        }).to_string(),
        // no timestamp on this line → ts null; its result never arrives → response null
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "tu-read", "name": "Read", "input": { "file_path": "/main/repo/b.js" } }
                ]
            }
        }).to_string(),
        // USER prompt: starts a new turn
        json!({
            "type": "user",
            "timestamp": t(5),
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "do the thing" }
                ]
            }
        }).to_string(),
        // structured toolUseResult HAS stdout → kept verbatim, result text NOT merged in
        json!({
            "type": "user",
            "timestamp": t(6),
            "toolUseResult": { "stdout": "one\ntwo\n", "interrupted": false },
            "message": {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "tu-bash", "content": [{ "type": "text", "text": "one" }, { "type": "text", "text": "two" }] }
                ]
            }
        }).to_string(),
        // no structured object → the plain result text IS the response
        json!({
            "type": "user",
            "timestamp": t(7),
            "message": {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "tu-grep", "content": "src/a.js:1:x" }
                ]
            }
        }).to_string(),
        // pairing across distance: first tool's result lands LAST; structured lacks text field → text merged in as `content`
        json!({
            "type": "user",
            "timestamp": t(8),
            "toolUseResult": { "structuredPatch": [{ "newStart": 1, "lines": ["+a"] }] },
            "message": {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "tu-edit", "content": "The file has been updated" }
                ]
            }
        }).to_string(),
        // trailing prose after the last tool
        json!({
            "type": "assistant",
            "timestamp": t(9),
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "all done" }
                ]
            }
        }).to_string(),
    ];

    let transcript = lines.join("\n") + "\n";
    let session = parse_claude_session(&transcript, "test_session");

    assert_eq!(session.harness, HarnessKind::ClaudeCode);
    assert_eq!(session.cwd.as_deref(), Some("/main/repo"));
    assert_eq!(session.first_ts, parse_iso_ts(&t(0)));
    assert_eq!(session.last_ts, parse_iso_ts(&t(9)));

    // Total tool invocations across turns: 4 tools (Edit, Bash, Grep, Read)
    assert_eq!(session.total_tool_calls(), 4);

    // Verify turn structure:
    // Turn 0: initial thinking, greeting, 4 tools
    // Turn 1: user prompt "do the thing", trailing prose "all done"
    assert_eq!(session.turns.len(), 2);

    let turn0 = &session.turns[0];
    assert_eq!(turn0.thinking, vec!["let me think"]);
    assert_eq!(turn0.assistant_messages, vec!["hello world"]);
    assert_eq!(turn0.tool_calls.len(), 4);
    assert_eq!(turn0.tool_calls[0].name, "Edit");
    assert_eq!(turn0.tool_calls[1].name, "Bash");
    assert_eq!(turn0.tool_calls[2].name, "Grep");
    assert_eq!(turn0.tool_calls[3].name, "Read");

    // Check response merge branches:
    // 1. Bash: kept verbatim { stdout: "one\ntwo\n", interrupted: false }
    let bash_resp = turn0.tool_calls[1].response.as_ref().unwrap();
    assert_eq!(bash_resp["stdout"], "one\ntwo\n");
    assert_eq!(bash_resp["interrupted"], false);

    // 2. Grep: bare string "src/a.js:1:x"
    let grep_resp = turn0.tool_calls[2].response.as_ref().unwrap();
    assert_eq!(grep_resp, "src/a.js:1:x");

    // 3. Edit: structuredPatch with merged "content": "The file has been updated"
    let edit_resp = turn0.tool_calls[0].response.as_ref().unwrap();
    assert_eq!(edit_resp["content"], "The file has been updated");
    assert!(edit_resp.get("structuredPatch").is_some());

    // 4. Read: result never arrived -> response is None
    assert!(turn0.tool_calls[3].response.is_none());

    // File actions:
    assert_eq!(turn0.file_actions.len(), 2);
    assert_eq!(turn0.file_actions[0].action, FileActionKind::Edit);
    assert_eq!(turn0.file_actions[0].file_path, "/main/repo/a.js");
    assert_eq!(turn0.file_actions[1].action, FileActionKind::Read);
    assert_eq!(turn0.file_actions[1].file_path, "/main/repo/b.js");

    let turn1 = &session.turns[1];
    assert_eq!(turn1.prompt.as_deref(), Some("do the thing"));
    assert_eq!(turn1.assistant_messages, vec!["all done"]);
}

#[test]
fn test_degenerate_inputs() {
    let empty_session = parse_claude_session("", "empty");
    assert_eq!(empty_session.turns.len(), 0);
    assert_eq!(empty_session.cwd, None);

    let garbage = "null\n42\n\"str\"\n{invalid json";
    let garbage_session = parse_claude_session(garbage, "garbage");
    assert_eq!(garbage_session.turns.len(), 0);
    assert_eq!(garbage_session.cwd, None);
}

#[test]
fn test_antigravity_transcript_parsing() {
    let lines = vec![
        json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "content": "Add a new button component",
            "created_at": "2026-10-04T12:00:00Z"
        }).to_string(),
        json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "thinking": "I need to edit Button.rs",
            "content": "Editing Button.rs now",
            "tool_calls": [
                {
                    "name": "replace_file_content",
                    "args": {
                        "TargetFile": "/path/Button.rs",
                        "TargetContent": "fn old() {}",
                        "ReplacementContent": "fn new() {}",
                        "StartLine": 10,
                        "EndLine": 12
                    }
                }
            ],
            "created_at": "2026-10-04T12:00:02Z"
        }).to_string(),
        json!({
            "step_index": 2,
            "type": "GENERIC",
            "content": "Success replacing content",
            "created_at": "2026-10-04T12:00:03Z"
        }).to_string(),
    ];

    let transcript = lines.join("\n");
    let session = parse_antigravity_session(&transcript, "ag_session");

    assert_eq!(session.harness, HarnessKind::Antigravity);
    assert_eq!(session.turns.len(), 1);

    let turn = &session.turns[0];
    assert_eq!(turn.prompt.as_deref(), Some("Add a new button component"));
    assert_eq!(turn.thinking, vec!["I need to edit Button.rs"]);
    assert_eq!(turn.assistant_messages, vec!["Editing Button.rs now"]);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "replace_file_content");
    assert_eq!(turn.tool_calls[0].response.as_ref().unwrap(), "Success replacing content");

    assert_eq!(turn.file_actions.len(), 1);
    assert_eq!(turn.file_actions[0].action, FileActionKind::Edit);
    assert_eq!(turn.file_actions[0].file_path, "/path/Button.rs");
    assert_eq!(turn.file_actions[0].old_content.as_deref(), Some("fn old() {}"));
    assert_eq!(turn.file_actions[0].new_content.as_deref(), Some("fn new() {}"));
}

#[test]
fn test_antigravity_encoded_strings_and_multiline_revisions() {
    use crate::revision::RevisionEngine;

    // Real-world Antigravity transcript format with double-encoded JSON string args:
    let lines = vec![
        json!({
            "step_index": 0,
            "type": "USER_INPUT",
            "content": "Add helper functions to file",
            "created_at": "2026-10-04T12:00:00Z"
        }).to_string(),
        json!({
            "step_index": 1,
            "type": "PLANNER_RESPONSE",
            "thinking": "Viewing file first",
            "content": "Viewing file now",
            "tool_calls": [
                {
                    "name": "view_file",
                    "args": {
                        "AbsolutePath": "\"/virtual/Workspace/file.rs\"",
                        "StartLine": "1",
                        "EndLine": "3"
                    }
                }
            ],
            "created_at": "2026-10-04T12:00:01Z"
        }).to_string(),
        json!({
            "step_index": 2,
            "type": "GENERIC",
            "content": "Created At: 2026-10-04T12:00:01Z\nShowing lines 1 to 3\nThe following code has been modified to include line numbers.\n1: fn line_one() {}\n2: fn line_two() {}\n3: fn line_three() {}",
            "created_at": "2026-10-04T12:00:02Z"
        }).to_string(),
        json!({
            "step_index": 3,
            "type": "PLANNER_RESPONSE",
            "thinking": "Editing file now",
            "content": "Replacing line_two with expanded helper",
            "tool_calls": [
                {
                    "name": "replace_file_content",
                    "args": {
                        "TargetFile": "\"/virtual/Workspace/file.rs\"",
                        "TargetContent": "\"fn line_two() {}\"",
                        "ReplacementContent": "\"fn line_two() {\\n    println!(\\\"expanded\\\");\\n}\"",
                        "StartLine": "2",
                        "EndLine": "2"
                    }
                }
            ],
            "created_at": "2026-10-04T12:00:03Z"
        }).to_string(),
    ];

    let transcript = lines.join("\n");
    let session = parse_antigravity_session(&transcript, "test_encoded_ag");

    assert_eq!(session.turns.len(), 1);
    let turn = &session.turns[0];
    assert_eq!(turn.file_actions.len(), 2);

    // 1. view_file: path cleaned without quotes, content extracted from GENERIC step
    let read_action = &turn.file_actions[0];
    assert_eq!(read_action.file_path, "/virtual/Workspace/file.rs");
    let read_content = read_action.original_file.as_ref().expect("view_file content extracted");
    assert_eq!(
        read_content,
        "fn line_one() {}\nfn line_two() {}\nfn line_three() {}"
    );

    // 2. replace_file_content: path cleaned, ReplacementContent unescaped with actual newlines
    let edit_action = &turn.file_actions[1];
    assert_eq!(edit_action.file_path, "/virtual/Workspace/file.rs");
    assert_eq!(edit_action.old_content.as_deref(), Some("fn line_two() {}"));
    assert_eq!(
        edit_action.new_content.as_deref(),
        Some("fn line_two() {\n    println!(\"expanded\");\n}")
    );

    // Ingest into RevisionEngine and verify revisions have real multi-line text
    let mut engine = RevisionEngine::new();
    engine.ingest_session(&session);

    let history = engine.history("/virtual/Workspace/file.rs").expect("history exists");
    assert_eq!(history.count(), 2);

    let r0 = history.get(0).unwrap();
    assert_eq!(r0.text.lines().count(), 3);

    let r1 = history.get(1).unwrap();
    // After replacing single line with 3-line function, total lines should be 5
    assert_eq!(r1.text.lines().count(), 5);
}

#[test]
fn test_multi_turn_file_actions() {
    let lines = vec![
        // Turn 1: user asks to create file
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Create server.rs" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "src/server.rs", "content": "fn main() {}" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "src/server.rs", "content": "fn main() {}" },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1" }] }
        }).to_string(),
        // Turn 2: user asks to add route
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Add route to server.rs" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t2", "name": "Edit", "input": {
                        "file_path": "src/server.rs",
                        "old_string": "fn main() {}",
                        "new_string": "fn main() { route(); }"
                    } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": {
                "filePath": "src/server.rs",
                "structuredPatch": [
                    { "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-fn main() {}", "+fn main() { route(); }"] }
                ]
            },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2" }] }
        }).to_string(),
    ];

    let session = parse_claude_session(&lines.join("\n"), "multi_turn");
    assert_eq!(session.turns.len(), 2);
    assert_eq!(session.touched_files(), vec!["src/server.rs"]);
    assert_eq!(session.total_file_actions(), 2);

    let t1 = &session.turns[0];
    assert_eq!(t1.prompt.as_deref(), Some("Create server.rs"));
    assert_eq!(t1.file_actions[0].action, FileActionKind::Write);
    assert_eq!(t1.file_actions[0].new_content.as_deref(), Some("fn main() {}"));

    let t2 = &session.turns[1];
    assert_eq!(t2.prompt.as_deref(), Some("Add route to server.rs"));
    assert_eq!(t2.file_actions[0].action, FileActionKind::Edit);
    assert_eq!(t2.file_actions[0].hunks.len(), 1);
    assert_eq!(t2.file_actions[0].hunks[0].lines, vec!["-fn main() {}", "+fn main() { route(); }"]);
}

#[test]
fn test_real_claude_session_if_present() {
    let home = std::env::var("HOME").unwrap_or_default();
    let session_path = std::path::PathBuf::from(home)
        .join(".claude/projects/-Users-lugo-localdev-viz-web-glyph3d-js/61cf579f-ad10-4705-be7b-06926c5f69c3.jsonl");

    if session_path.exists() {
        let content = std::fs::read_to_string(&session_path).expect("read real session file");
        let session = parse_claude_session(&content, "61cf579f");
        assert_eq!(session.harness, HarnessKind::ClaudeCode);
        assert_eq!(session.session_id, "61cf579f");
        assert!(session.cwd.is_some());
    }
}

#[test]
fn test_detect_and_parse_session_claude_and_antigravity() {
    use super::{detect_and_parse_session, HarnessKind};

    // Claude Code snippet
    let claude_snippet = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": "Claude prompt" }] }
    }).to_string();
    let s_claude = detect_and_parse_session(&claude_snippet, "claude_sess");
    assert_eq!(s_claude.harness, HarnessKind::ClaudeCode);
    assert_eq!(s_claude.session_id, "claude_sess");

    // Antigravity snippet
    let antigravity_snippet = json!({
        "step_index": 0,
        "type": "USER_INPUT",
        "content": "Antigravity prompt"
    }).to_string();
    let s_antigravity = detect_and_parse_session(&antigravity_snippet, "ag_sess");
    assert_eq!(s_antigravity.harness, HarnessKind::Antigravity);
    assert_eq!(s_antigravity.session_id, "ag_sess");
}

#[test]
fn test_stage_agent_session_creates_controller_and_navigates() {
    use super::{stage_agent_session, parse_claude_session};
    use crate::revision::RevisionEngine;

    let transcript = vec![
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Turn 0 prompt" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "a.rs", "content": "hello" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "a.rs", "content": "hello" },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1" }] }
        }).to_string(),
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Turn 1 prompt" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t2", "name": "Edit", "input": { "file_path": "a.rs", "old_string": "hello", "new_string": "world" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "filePath": "a.rs", "structuredPatch": [{ "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-hello", "+world"] }] },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2" }] }
        }).to_string(),
    ].join("\n");

    let session = parse_claude_session(&transcript, "staged_sess");
    let mut rev_engine = RevisionEngine::new();
    rev_engine.ingest_session(&session);

    let staged = stage_agent_session(None, &[], session, rev_engine);
    assert!(staged.controller.is_some());

    let mut ctrl = staged.controller.unwrap();
    assert!(ctrl.active_carrel.is_some());

    // Navigation checks: carrel starts at latest beat (4/4)
    let prev_msg = ctrl.carrel_prev().unwrap();
    assert!(prev_msg.contains("beat 3/4"));

    let next_msg = ctrl.carrel_next().unwrap();
    assert!(next_msg.contains("beat 4/4"));

    let jump_msg = ctrl.carrel_set_turn(1).unwrap();
    assert!(jump_msg.contains("jump to turn 2/2"));
}

#[test]
fn test_stage_agent_session_formats_and_populates_text() {
    use super::{parse_claude_session, stage_agent_session};
    use crate::revision::RevisionEngine;

    let transcript = vec![
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Plan refactor for rendering" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "We should first inspect the scene graph and then apply deltas." },
                    { "type": "text", "text": "I will inspect the workspace and begin the refactor." },
                    { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "main.rs", "content": "fn main() {\n    println!(\"hello\");\n}\n" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "main.rs", "content": "fn main() {\n    println!(\"hello\");\n}\n" },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1" }] }
        }).to_string(),
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Change greeting to hello world" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "Replacing hello with hello world in main.rs." },
                    { "type": "text", "text": "Replaced greeting." },
                    { "type": "tool_use", "id": "t2", "name": "Edit", "input": { "file_path": "main.rs", "old_string": "hello", "new_string": "hello world" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "filePath": "main.rs", "structuredPatch": [{ "oldStart": 2, "oldLines": 1, "newStart": 2, "newLines": 1, "lines": ["-    println!(\"hello\");", "+    println!(\"hello world\");"] }] },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2" }] }
        }).to_string(),
    ].join("\n");

    let session = parse_claude_session(&transcript, "text_sess");
    let mut rev_engine = RevisionEngine::new();
    rev_engine.ingest_session(&session);

    let staged = stage_agent_session(None, &[], session, rev_engine);
    // Groups allocated for: identity(0) + Turn0(1) + Turn1(2) + Rev0(3) + Rev1(4)
    assert!(staged.groups.len() >= 5);
}

#[test]
fn test_utf8_multibyte_truncation_no_panic() {
    use super::types::{truncate_chars, truncate_with_ellipsis, TranscriptEventKind, AgentTurn};

    // Construct a string where byte 52 lands in the middle of a 3-byte em-dash '—'
    // 50 ASCII bytes + '—' (bytes 50, 51, 52) + more text
    let prefix = "a".repeat(50);
    let hostile_str = format!("{prefix}—and more text that exceeds the limit");

    // Slicing &hostile_str[..52] would panic!
    let truncated = truncate_chars(&hostile_str, 51);
    assert_eq!(truncated, format!("{prefix}—"));

    let with_dots = truncate_with_ellipsis(&hostile_str, 55);
    assert!(with_dots.ends_with("..."));

    // Test AgentTurn::summary with hostile string
    let turn = AgentTurn {
        turn_index: 0,
        prompt: Some(hostile_str.clone()),
        thinking: Vec::new(),
        file_actions: Vec::new(),
        tool_calls: Vec::new(),
        assistant_messages: vec![hostile_str.clone()],
        timestamp: None,
        steps: Vec::new(),
    };
    let turn_summary = turn.summary();
    assert!(!turn_summary.is_empty());

    // Test TranscriptEventKind::summary with hostile string (reproduces the user's exact panic condition)
    let ev = TranscriptEventKind::AssistantResponse {
        message: hostile_str.clone(),
    };
    let ev_summary = ev.summary();
    assert!(ev_summary.starts_with("Assistant: "));

    let ev_user = TranscriptEventKind::UserPrompt {
        prompt: hostile_str.clone(),
    };
    assert!(ev_user.summary().starts_with("User: "));

    let ev_cmd = TranscriptEventKind::Command {
        name: "Bash".to_string(),
        command_line: hostile_str.clone(),
        output: None,
        is_error: false,
    };
    assert!(ev_cmd.summary().starts_with("$ "));
}

/// Typed prompts are STRING content; everything a user record carries that the
/// user did not type to the agent is not a prompt. Shapes from real
/// transcripts (Claude Code 2.1.x).
#[test]
fn user_prompt_text_classifies_real_record_shapes() {
    let rec = |content: serde_json::Value, extra: serde_json::Value| {
        let mut o = json!({ "type": "user", "message": { "role": "user", "content": content } });
        for (k, v) in extra.as_object().unwrap() {
            o[k] = v.clone();
        }
        o
    };
    let none = json!({});
    assert_eq!(user_prompt_text(&rec(json!("  Fix the fog  "), none.clone())).as_deref(), Some("Fix the fog"));
    assert_eq!(
        user_prompt_text(&rec(json!([{ "type": "text", "text": "pasted" }]), none.clone())).as_deref(),
        Some("pasted")
    );
    assert_eq!(
        user_prompt_text(&rec(
            json!("<command-name>/code-review</command-name>\n<command-message>code-review</command-message>\n<command-args>high</command-args>"),
            none.clone()
        ))
        .as_deref(),
        Some("/code-review high")
    );
    assert_eq!(
        user_prompt_text(&rec(json!("<command-name>/model</command-name>\n<command-args></command-args>"), none.clone())).as_deref(),
        Some("/model")
    );
    for not_a_prompt in [
        rec(json!("<local-command-caveat>Caveat: ...</local-command-caveat>"), json!({ "isMeta": true })),
        rec(json!("[Image: original 2876x792]"), json!({ "isMeta": true })),
        rec(json!("This session is being continued from a previous conversation..."), json!({ "isCompactSummary": true })),
        rec(json!("<local-command-stdout>Set model</local-command-stdout>"), none.clone()),
        rec(json!("<bash-input>claude stop</bash-input>"), none.clone()),
        rec(json!("<bash-stdout>stopped</bash-stdout><bash-stderr></bash-stderr>"), none.clone()),
        rec(json!([{ "type": "text", "text": "[Request interrupted by user]" }]), none.clone()),
        rec(json!([{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }]), none.clone()),
        rec(json!("   "), none.clone()),
    ] {
        assert_eq!(user_prompt_text(&not_a_prompt), None, "{not_a_prompt}");
    }
}

/// The folding regression: string prompts must start turns, and a turn's
/// thinking / tool calls / text must linearize in the order they happened,
/// not grouped by kind. One API message spans several lines (one block each).
#[test]
fn claude_turns_split_on_string_prompts_and_keep_block_order() {
    use super::types::TranscriptEventKind as K;
    let line = |v: serde_json::Value| v.to_string();
    let asst = |id: &str, block: serde_json::Value| {
        line(json!({ "type": "assistant", "message": { "id": id, "role": "assistant", "content": [block] } }))
    };
    let result = |id: &str| {
        line(json!({ "type": "user", "message": { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": id, "content": "ok" } ] } }))
    };
    let lines = [
        line(json!({ "type": "user", "message": { "role": "user", "content": "first prompt" } })),
        asst("m1", json!({ "type": "thinking", "thinking": "plan" })),
        asst("m1", json!({ "type": "tool_use", "id": "b1", "name": "Bash", "input": { "command": "ls" } })),
        result("b1"),
        asst("m2", json!({ "type": "text", "text": "listed" })),
        asst("m2", json!({ "type": "tool_use", "id": "r1", "name": "Read", "input": { "file_path": "/r/a.rs" } })),
        result("r1"),
        asst("m3", json!({ "type": "text", "text": "read it" })),
        line(json!({ "type": "user", "isMeta": true, "message": { "role": "user", "content": "<local-command-caveat>x</local-command-caveat>" } })),
        line(json!({ "type": "user", "message": { "role": "user", "content": "second prompt" } })),
        asst("m4", json!({ "type": "text", "text": "done" })),
    ];
    let session = parse_claude_session(&(lines.join("\n") + "\n"), "order");
    assert_eq!(session.turns.len(), 2, "each string prompt starts a turn; the meta record does not");
    assert_eq!(session.turns[0].prompt.as_deref(), Some("first prompt"));
    assert_eq!(session.turns[1].prompt.as_deref(), Some("second prompt"));

    let kinds: Vec<&str> = session
        .linearize_events(None)
        .iter()
        .map(|e| match &e.kind {
            K::UserPrompt { .. } => "prompt",
            K::Thinking { .. } => "thinking",
            K::Command { .. } => "command",
            K::FileRead { .. } => "read",
            K::AssistantResponse { .. } => "text",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        ["prompt", "thinking", "command", "text", "read", "text", "prompt", "text"],
        "events follow transcript order, not kind groups"
    );
}

