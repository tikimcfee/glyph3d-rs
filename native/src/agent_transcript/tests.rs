//! The reader held to gomuxai's conformance corpus (the event streams it
//! promises for every harness, vendored at a pinned commit under
//! `tools/vendor/gomuxai/`), and the staging of what it reads.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::agent_cli::{describe_dirs, nest_subagents, parse_rfc3339, AgentCli, SessionDir};
use super::events::{AgentSession, Applied, PromptOrigin, ToolStatus};
use super::test_stream::Stream;
use super::types::{truncate_chars, truncate_with_ellipsis, TranscriptEventKind};
use crate::revision::RevisionEngine;

macro_rules! corpus {
    ($($case:literal),* $(,)?) => {
        [$(($case, include_str!(concat!("../../../tools/vendor/gomuxai/internal/transcript/testdata/corpus/", $case, ".events.jsonl")))),*]
    };
}

/// Every expected stream in the vendored corpus. Pinned by count below, so
/// a stream dropped from the vendor dir (or this list) is seen.
const CORPUS: [(&str, &str); 13] = corpus![
    "agy/conversation",
    "claude/basic-turn",
    "claude/derived-turns",
    "claude/held-delivery",
    "claude/mid-turn-queued",
    "claude/notification-and-commands",
    "claude/requests",
    "claude/session-meta",
    "claude/subagent",
    "claude/subagent/subagents/agent-a3f9c2e17b4d5e608",
    "claude/system-records",
    "claude/tool-results",
    "kimi/interleaved",
];

fn corpus(case: &str) -> &'static str {
    CORPUS.iter().find(|(c, _)| *c == case).unwrap_or_else(|| panic!("no corpus case {case}")).1
}

fn read(case: &str) -> AgentSession {
    AgentSession::from_stream_str(corpus(case)).unwrap_or_else(|e| panic!("{case}: {e}"))
}

/// A beat's kind, as a word.
fn kind_name(kind: &TranscriptEventKind) -> &'static str {
    match kind {
        TranscriptEventKind::UserPrompt { .. } => "prompt",
        TranscriptEventKind::Thinking { .. } => "think",
        TranscriptEventKind::FileRead { .. } => "read",
        TranscriptEventKind::FileEdit { .. } => "edit",
        TranscriptEventKind::FileWrite { .. } => "write",
        TranscriptEventKind::Command { .. } => "command",
        TranscriptEventKind::ToolInvocation { .. } => "tool",
        TranscriptEventKind::AssistantResponse { .. } => "say",
        TranscriptEventKind::System { .. } => "system",
    }
}

/// The stream's event lines, parsed apart from the reader: the test's own
/// account of what the fold must hold.
fn event_lines(case: &str) -> Vec<Value> {
    corpus(case)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("corpus lines are JSON"))
        .filter(|v| v["type"] == "event")
        .collect()
}

/// One beat per event id, in the order the adds came, each holding its
/// event's LAST line: for every stream of the corpus.
#[test]
fn every_corpus_stream_folds_to_one_beat_per_event() {
    assert_eq!(CORPUS.len(), 13, "the vendored corpus is 13 streams");
    for (case, text) in CORPUS {
        let session = read(case);
        let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(session.session_id(), first["session_id"], "{case}: the session line");
        let lines = event_lines(case);
        let mut ids: Vec<&str> = Vec::new();
        for l in &lines {
            let id = l["id"].as_str().unwrap();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let beats = session.linearize_events(None);
        assert_eq!(beats.len(), ids.len(), "{case}: one beat per event id");
        for (i, (beat, id)) in beats.iter().zip(&ids).enumerate() {
            assert_eq!((beat.index, beat.id.as_str()), (i, *id), "{case}: beat {i} is the {i}th event added");
            let last = lines.iter().rev().find(|l| l["id"] == *id).unwrap();
            assert_eq!(session.events[i].seq, last["seq"].as_u64().unwrap(), "{case}: {id} holds its last line");
            let expected = match last["kind"].as_str().unwrap() {
                "tool" => {
                    let tool = &last["tool"];
                    let returned = tool["result"]["status"] == "ok" && tool["target"].as_str().is_some_and(|t| !t.is_empty());
                    match tool["action"].as_str().unwrap() {
                        a @ ("read" | "edit" | "write") if returned => a,
                        "bash" => "command",
                        _ => "tool",
                    }
                }
                other => other,
            };
            assert_eq!(kind_name(&beat.kind), expected, "{case}: {id}'s beat kind");
        }
    }
}

/// Kimi's thinking, text and tool calls interleave across a turn; the beats
/// keep that order (a grouped order, all thinking then all tools then all
/// text, is the bug `kimi/interleaved` exists for).
#[test]
fn kimi_beats_keep_the_wire_order() {
    let beats = read("kimi/interleaved").linearize_events(None);
    let kinds: Vec<&str> = beats.iter().map(|b| kind_name(&b.kind)).collect();
    assert_eq!(kinds, ["prompt", "think", "read", "say", "think", "command", "say"]);
}

/// agy's three calls in one step each get their own result (positional
/// pairing gave only the last one a result).
#[test]
fn agy_calls_in_one_step_each_hold_their_result() {
    let beats = read("agy/conversation").linearize_events(None);
    let by_id = |id: &str| &beats.iter().find(|b| b.id == id).unwrap().kind;
    match by_id("step_1_tool_0") {
        TranscriptEventKind::Command { status, output, .. } => {
            assert_eq!(*status, ToolStatus::Ok);
            assert!(output.as_deref().is_some_and(|o| !o.is_empty()));
        }
        k => panic!("run_command is a command beat, got {k:?}"),
    }
    for (id, target) in [("step_1_tool_1", "main.go"), ("step_1_tool_2", "spin.go")] {
        match by_id(id) {
            TranscriptEventKind::FileRead { file_path, action_record, .. } => {
                assert_eq!((file_path.as_str(), action_record.tool_id.as_str()), (target, id));
            }
            k => panic!("{id} is a read that returned, got {k:?}"),
        }
    }
}

/// A tool's result is an update of its event: applied line by line (as a
/// live feed would apply them), the call's beat stays where its add put it
/// and changes from pending to its result.
#[test]
fn a_tool_result_updates_its_beat_in_place() {
    let text = corpus("claude/tool-results");
    let mut lines = text.lines();
    let mut session = AgentSession::default();
    assert_eq!(session.apply_line(lines.next().unwrap()), Ok(Applied::Session));
    let bash = "toolu_011tw69VnI2pYtiPQnqaknd1";
    let multi = "toolu_01zwI74ek5gSV6soBvRy3h61";
    let mut bash_at = None;
    let mut multi_updated = None;
    for line in lines {
        let applied = session.apply_line(line).unwrap();
        let id = serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string();
        match applied {
            Applied::Added(i) if id == bash => {
                bash_at = Some(i);
                let beats = session.linearize_events(None);
                assert!(matches!(beats[i].kind, TranscriptEventKind::Command { status: ToolStatus::Pending, .. }));
            }
            Applied::Updated(i) if id == bash => assert_eq!(Some(i), bash_at, "the update replaces the add's beat"),
            Applied::Updated(i) if id == multi => multi_updated = Some(i),
            _ => {}
        }
    }
    let beats = session.linearize_events(None);
    assert_eq!(beats.len(), 11);
    let i = bash_at.expect("the Bash call was added");
    match &beats[i].kind {
        TranscriptEventKind::Command { status, output, .. } => {
            assert_eq!(*status, ToolStatus::Error);
            assert!(output.is_some(), "an error's output lands with it");
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(multi_updated, Some(1), "the MultiEdit is the second event");
    match &beats[1].kind {
        TranscriptEventKind::FileEdit { file_path, post_edit_content, action_record } => {
            assert_eq!(file_path, "spin.go");
            assert!(post_edit_content.as_deref().unwrap().contains("func Spin(ctx context.Context, n int) error {"));
            assert_eq!(action_record.hunks.len(), 2, "meta.patch's two hunks");
            assert_eq!(action_record.added_removed, Some((2, 2)));
        }
        k => panic!("the MultiEdit returned: a file edit, got {k:?}"),
    }
}

/// Turns are the events' own: a message queued mid-turn stays in its turn,
/// a later update stays in the turn of the call it completes, and turn
/// numbers are the transcript's (a notification's turn shares its index).
#[test]
fn turns_come_from_the_events() {
    let session = read("claude/mid-turn-queued");
    let beats = session.linearize_events(None);
    assert_eq!(session.turn_count(), 2);
    let turns: Vec<usize> = beats.iter().map(|b| b.turn_index).collect();
    assert_eq!(turns, [0, 0, 0, 0, 0, 0, 1, 1, 1, 1]);
    assert!(matches!(beats[2].kind, TranscriptEventKind::UserPrompt { origin: PromptOrigin::Queued, .. }), "the mid-turn message");
    assert!(matches!(beats[6].kind, TranscriptEventKind::UserPrompt { origin: PromptOrigin::Queued, .. }), "the message that opened turn 2");
    assert_eq!(beats[6].turn_label(), "Turn 2");

    let session = read("claude/notification-and-commands");
    let beats = session.linearize_events(None);
    assert_eq!(session.turn_count(), 4);
    let labels: Vec<String> = (0..4)
        .map(|t| beats.iter().find(|b| b.turn_index == t).unwrap().turn_label())
        .collect();
    assert_eq!(labels, ["Turn 1", "Turn 1", "Turn 1", "Turn 2"]);
    let bash = beats.iter().find(|b| b.id.starts_with("toolu_01AnWev7")).unwrap();
    assert_eq!(bash.turn_index, 0, "completed by a notification in turn 1, it stays in turn 0");
    assert!(matches!(bash.kind, TranscriptEventKind::Command { status: ToolStatus::Ok, .. }));

    let session = read("claude/derived-turns");
    assert_eq!(session.turn_count(), 3);
    assert!(session.events.iter().all(|e| e.turn.as_ref().unwrap().derived));
    assert_eq!(session.linearize_events(None)[0].turn_label(), "Turn 1 (derived)");
}

/// Every origin a prompt can have reaches its beat, and the minor beats are
/// there too: redacted thinking, noise tools, harness records.
#[test]
fn prompt_origins_and_minor_beats_reach_the_deck() {
    let mut origins = Vec::new();
    for (case, _) in CORPUS {
        for beat in read(case).linearize_events(None) {
            if let TranscriptEventKind::UserPrompt { origin, command, .. } = &beat.kind {
                if !origins.contains(origin) {
                    origins.push(*origin);
                }
                assert_eq!(*origin == PromptOrigin::Command, command.is_some(), "{case}: {}", beat.id);
                assert!(beat.summary().starts_with(origin.label()), "{case}: {}", beat.summary());
            }
        }
    }
    for o in [
        PromptOrigin::Human,
        PromptOrigin::Queued,
        PromptOrigin::Delivered,
        PromptOrigin::TaskNotification,
        PromptOrigin::Peer,
        PromptOrigin::Command,
    ] {
        assert!(origins.contains(&o), "no {o:?} prompt in the corpus");
    }
    let beats = read("claude/notification-and-commands").linearize_events(None);
    assert!(beats.iter().any(|b| matches!(&b.kind, TranscriptEventKind::UserPrompt { command: Some(c), .. } if c == "/context")));

    let beats = read("claude/basic-turn").linearize_events(None);
    assert!(matches!(beats[1].kind, TranscriptEventKind::Thinking { redacted: true, .. }));
    assert_eq!(beats[1].summary(), "Thinking: (redacted)");
    let noise = beats.iter().find(|b| b.kind.is_noise()).expect("ToolSearch is shown");
    assert_eq!(noise.summary(), "Noise: ToolSearch");
    let command = beats.iter().find(|b| kind_name(&b.kind) == "command").unwrap();
    assert_ne!(noise.banner_colors(), command.banner_colors());
    let systems: Vec<String> = read("claude/system-records")
        .linearize_events(None)
        .iter()
        .filter(|b| kind_name(&b.kind) == "system")
        .map(|b| b.summary())
        .collect();
    assert!(systems.iter().any(|s| s == "System: compact_boundary") && systems.iter().any(|s| s == "System: api_error"), "{systems:?}");
}

/// An edit whose event carries the file after it builds its revisions from
/// that text alone: the base is the file with the edit's patch undone, and
/// the disk is never asked (a partial read before it is a window, not a
/// revision).
#[test]
fn an_edit_with_file_text_builds_its_revisions_without_disk() {
    let session = read("claude/tool-results");
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = asked.clone();
    let mut engine = RevisionEngine::new().with_disk_resolver(move |p: &str| {
        log.lock().unwrap().push(p.to_string());
        None
    });
    engine.ingest_session(&session);
    assert!(asked.lock().unwrap().is_empty(), "asked the disk for {:?}", asked.lock().unwrap());

    let lines = event_lines("claude/tool-results");
    let tool_of = |id: &str| lines.iter().rev().find(|l| l["id"] == id).unwrap()["tool"].clone();
    for (path, id, original_key) in [
        ("spin.go", "toolu_01zwI74ek5gSV6soBvRy3h61", "originalFileContents"),
        ("spin_test.go", "toolu_01rVkaQppDqV2ZGzzkSAkAJw", "originalFile"),
    ] {
        let tool = tool_of(id);
        let history = engine.history(path).unwrap_or_else(|| panic!("{path} has a history"));
        assert_eq!(history.count(), 2, "{path}: the base and the edit: {:#?}", history.revisions.iter().map(|r| (r.action, r.text.as_str())).collect::<Vec<_>>());
        assert_eq!(history.get(0).unwrap().text.as_str(), tool["result"]["structured"][original_key], "{path}: the base is the file before the edit");
        assert_eq!(history.get(1).unwrap().text.as_str(), tool["file"]["text"], "{path}: the edit's revision is its file text");
    }
}

/// agy's and Kimi's reads carry the lines they read (gomuxai 7238f18): the
/// read beats show that text, and the revision engine never goes to the disk
/// for them. agy's reads are whole files (their revision); Kimi's says no
/// file length, so it is a window, not a revision.
#[test]
fn agy_and_kimi_reads_use_their_file_text_not_the_disk() {
    for (case, reads) in [
        ("agy/conversation", &[("step_1_tool_1", "main.go", true), ("step_1_tool_2", "spin.go", true)][..]),
        ("kimi/interleaved", &[("tc-read-1", "config/load.go", false)][..]),
    ] {
        let session = read(case);
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = asked.clone();
        let mut engine = RevisionEngine::new().with_disk_resolver(move |p: &str| {
            log.lock().unwrap().push(p.to_string());
            None
        });
        engine.ingest_session(&session);
        assert!(asked.lock().unwrap().is_empty(), "{case}: asked the disk for {:?}", asked.lock().unwrap());

        let lines = event_lines(case);
        let beats = session.linearize_events(None);
        for &(id, target, whole) in reads {
            let text = lines.iter().rev().find(|l| l["id"] == id).unwrap()["tool"]["file"]["text"].clone();
            assert!(text.is_string(), "{case}: {id} carries its file text");
            match &beats.iter().find(|b| b.id == id).unwrap().kind {
                TranscriptEventKind::FileRead { file_path, content, first_line, .. } => {
                    assert_eq!((file_path.as_str(), *first_line), (target, 1));
                    assert_eq!(content.as_deref(), text.as_str(), "{case}: {id}'s beat shows the lines it read");
                }
                k => panic!("{case}: {id} is a read, got {k:?}"),
            }
            match engine.history(target) {
                Some(h) if whole => assert_eq!(h.get(0).unwrap().text.as_str(), text, "{case}: {target}'s revision is the read"),
                None if !whole => {}
                h => panic!("{case}: {target}: whole read {whole}, history {h:?}"),
            }
        }
    }
}

/// An edit whose event has no file text (agy's, or an Edit whose result
/// lacked the file) falls back to the disk: the target, joined to the
/// session's cwd, read now and taken back through the edit.
#[test]
fn an_edit_without_file_text_rebuilds_from_the_disk() {
    let dir = std::env::temp_dir().join(format!("glyph3d-no-file-edit-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg/a.go"), "func a() int {\n\treturn 2\n}\n").unwrap();
    let mut stream = Stream::new("agy-like", dir.to_str().unwrap());
    stream.prompt("p", 1, "make a return 2").tool(
        "step_1_tool_0",
        1,
        json!({ "name": "replace_file_content", "action": "edit", "target": "pkg/a.go",
                "input": { "TargetFile": dir.join("pkg/a.go"), "TargetContent": "return 1", "ReplacementContent": "return 2" } }),
        json!({ "status": "ok", "output": "replaced 1 occurrence" }),
        json!({ "meta": { "added": 1, "removed": 1 } }),
    );
    let session = stream.session();
    let engine = RevisionEngine::for_session(&session);
    let history = engine.history("pkg/a.go").expect("the edit has a history");
    assert_eq!(history.get(0).unwrap().text.as_str(), "func a() int {\n\treturn 1\n}\n", "the base, taken back from the disk");
    assert_eq!(history.get(1).unwrap().text.as_str(), "func a() int {\n\treturn 2\n}\n");
    match &session.linearize_events(Some(&engine))[1].kind {
        TranscriptEventKind::FileEdit { post_edit_content, .. } => {
            assert_eq!(post_edit_content.as_deref(), Some("func a() int {\n\treturn 2\n}\n"));
        }
        k => panic!("{k:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A subagent is a session of its own, linked both ways: `parent_session`
/// on it, `sub_sessions` on the call that started it; a listing puts it
/// under its parent.
#[test]
fn a_subagent_is_a_session_of_its_own() {
    let parent = read("claude/subagent");
    let sub = read("claude/subagent/subagents/agent-a3f9c2e17b4d5e608");
    let link = sub.info.parent_session.as_ref().expect("a subagent names its parent");
    assert_eq!(link.session_id, parent.session_id());
    let call = parent
        .linearize_events(None)
        .into_iter()
        .find(|b| b.id == link.tool_id)
        .expect("the call that started it");
    match call.kind {
        TranscriptEventKind::ToolInvocation { sub_sessions, status, .. } => {
            assert_eq!(sub_sessions, [sub.session_id().to_string()]);
            assert_eq!(status, ToolStatus::Ok);
        }
        k => panic!("{k:?}"),
    }
    assert!(sub.events.iter().all(|e| !e.agent.is_empty()), "a subagent's events carry its id");
    assert!(matches!(sub.linearize_events(None)[0].kind, TranscriptEventKind::UserPrompt { origin: PromptOrigin::Peer, .. }));

    let mut ghost = parent.info.clone();
    ghost.session_id = "no-transcript".into();
    ghost.path.clear();
    let listed = vec![sub.info.clone(), parent.info.clone(), ghost];
    let nested = nest_subagents(&listed);
    assert_eq!(nested.len(), 1, "the subagent sits under its parent; a session with no transcript isn't listed");
    assert_eq!(nested[0].0.session_id, parent.session_id());
    assert_eq!(nested[0].1.len(), 1);
}

/// No `agent` to run, or a run that fails, is an error naming the command
/// (and what it printed), never an empty list.
#[test]
fn a_missing_agent_binary_fails_loud() {
    let missing = AgentCli::new("/nonexistent/glyph3d-test-agent");
    let e = missing.sessions().unwrap_err();
    assert!(e.contains("could not run `/nonexistent/glyph3d-test-agent sessions --json`"), "{e}");
    let e = missing.dirs().unwrap_err();
    assert!(e.contains("glyph3d-test-agent sessions --dirs --json"), "{e}");
    let e = missing.transcript("abc").unwrap_err();
    assert!(e.contains("glyph3d-test-agent transcript abc"), "{e}");

    // `sh sessions --json` runs and fails: sh can't open a script named `sessions`.
    let e = AgentCli::new("sh").sessions().unwrap_err();
    assert!(e.contains("`sh sessions --json` failed") && e.contains("sessions"), "{e}");
    assert!(AgentCli::default().transcript(" ").is_err(), "an empty session name is refused");
}

/// A stream that breaks the contract is refused, naming the line.
#[test]
fn a_stream_off_the_contract_is_refused() {
    let session = r#"{"type":"session","session_id":"s","harness":"claude"}"#.to_string();
    let event = |id: &str, op: &str, kind: &str| {
        format!(r#"{{"type":"event","id":"{id}","op":"{op}","seq":0,"at":0,"kind":"{kind}","{kind}":{{"text":"x","origin":"human"}}}}"#)
    };
    assert!(AgentSession::from_stream_str("").unwrap_err().contains("no session line"));
    assert!(AgentSession::from_stream_str(&event("a", "add", "prompt")).unwrap_err().contains("before the session line"));
    let e = AgentSession::from_stream_str(&[session.clone(), event("a", "add", "request")].join("\n")).unwrap_err();
    assert!(e.contains("line 2") && e.contains("\"request\""), "{e}");
    let twice = [session.clone(), event("a", "add", "prompt"), event("a", "add", "prompt")].join("\n");
    assert!(AgentSession::from_stream_str(&twice).unwrap_err().contains("added twice"));
    assert!(AgentSession::from_stream_str(&[session, "{not json".to_string()].join("\n")).unwrap_err().contains("line 2"));
}

/// `agent sessions --dirs --json` as gomuxai prints it, in one line.
#[test]
fn the_dirs_report_reads_gomuxais_scan_list() {
    let dirs: Vec<SessionDir> = serde_json::from_str(
        r#"[{"harness":"claude","path":"/nowhere/.claude/projects","origin":"default","exists":true},
            {"harness":"kimi","path":"/nowhere/k","origin":"config","exists":false}]"#,
    )
    .unwrap();
    assert_eq!(describe_dirs(&dirs), "claude /nowhere/.claude/projects (default); kimi /nowhere/k (config, missing)");
    assert!(describe_dirs(&[]).contains("no session directories"));
}

#[test]
fn rfc3339_times_read_as_system_times() {
    let t = parse_rfc3339("2026-10-26T16:38:34.551436199Z").unwrap();
    let d = t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap();
    assert_eq!((d.as_secs(), d.subsec_nanos()), (1_793_032_714, 551_436_199));
    assert_eq!(parse_rfc3339("2026-10-26T09:38:34.551436199-07:00"), Some(t));
    assert_eq!(parse_rfc3339("2026-10-26 16:38:34Z").unwrap(), t - std::time::Duration::new(0, 551_436_199));
    assert_eq!(parse_rfc3339("2026-10-26"), None);
    assert_eq!(parse_rfc3339("2026-13-26T16:38:34Z"), None);
}

/// Every corpus session stages: each beat kind formats into card text.
#[test]
fn every_corpus_session_stages() {
    for (case, _) in CORPUS {
        let session = read(case);
        let engine = RevisionEngine::for_session(&session);
        let beats = session.linearize_events(Some(&engine)).len();
        let staged = super::stage_agent_session(None, &[], session, engine);
        let ctrl = staged.controller.expect("a carrel controller");
        let carrel = ctrl.scene.world.get::<crate::spatial_scene::AgentCarrel>(ctrl.active_carrel.unwrap()).unwrap();
        assert_eq!(carrel.beat_count, beats, "{case}");
    }
}

/// Two turns: a write, then an edit of the written file.
fn two_turn_session() -> AgentSession {
    let mut s = Stream::new("staged_sess", "/work");
    s.prompt("p0", 1, "Turn 0 prompt")
        .write("t1", 1, "a.rs", "hello\n")
        .prompt("p1", 2, "Turn 1 prompt")
        .edit("t2", 2, "a.rs", ("hello", "world"), "world\n", json!({
            "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-hello", "+world"]
        }));
    s.session()
}

#[test]
fn test_stage_agent_session_creates_controller_and_navigates() {
    let session = two_turn_session();
    let rev_engine = RevisionEngine::for_session(&session);
    let staged = super::stage_agent_session(None, &[], session, rev_engine);
    let mut ctrl = staged.controller.expect("a controller");
    assert!(ctrl.active_carrel.is_some());

    // Navigation checks: carrel starts at latest beat (4/4)
    let prev_msg = ctrl.carrel_prev().unwrap();
    assert!(prev_msg.contains("beat 3/4"), "{prev_msg}");
    let next_msg = ctrl.carrel_next().unwrap();
    assert!(next_msg.contains("beat 4/4"), "{next_msg}");
    let jump_msg = ctrl.carrel_set_turn(1).unwrap();
    assert!(jump_msg.contains("jump to turn 2/2"), "{jump_msg}");
}

#[test]
fn test_stage_agent_session_formats_and_populates_text() {
    let mut s = Stream::new("text_sess", "/work");
    s.prompt("p0", 1, "Plan refactor for rendering")
        .think("k0", 1, "We should first inspect the scene graph and then apply deltas.")
        .say("s0", 1, "I will inspect the workspace and begin the refactor.")
        .write("t1", 1, "main.rs", "fn main() {\n    println!(\"hello\");\n}\n")
        .prompt("p1", 2, "Change greeting to hello world")
        .edit("t2", 2, "main.rs", ("hello", "hello world"), "fn main() {\n    println!(\"hello world\");\n}\n", json!({
            "oldStart": 2, "oldLines": 1, "newStart": 2, "newLines": 1,
            "lines": ["-    println!(\"hello\");", "+    println!(\"hello world\");"]
        }));
    let session = s.session();
    let rev_engine = RevisionEngine::for_session(&session);
    let staged = super::stage_agent_session(None, &[], session, rev_engine);
    // Groups allocated for: identity(0) + Turn0(1) + Turn1(2) + Rev0(3) + Rev1(4)
    assert!(staged.groups.len() >= 5);
}

#[test]
fn test_utf8_multibyte_truncation_no_panic() {
    // Byte 52 lands in the middle of a 3-byte em-dash: slicing [..52] would panic.
    let prefix = "a".repeat(50);
    let hostile_str = format!("{prefix}—and more text that exceeds the limit");
    assert_eq!(truncate_chars(&hostile_str, 51), format!("{prefix}—"));
    assert!(truncate_with_ellipsis(&hostile_str, 55).ends_with("..."));

    let ev = TranscriptEventKind::AssistantResponse { message: hostile_str.clone(), is_final: false };
    assert!(ev.summary().starts_with("Assistant: "));
    let ev_user = TranscriptEventKind::UserPrompt { prompt: hostile_str.clone(), origin: PromptOrigin::Human, command: None, images: 0 };
    assert!(ev_user.summary().starts_with("You: "));
    let ev_cmd = TranscriptEventKind::Command {
        name: "Bash".to_string(),
        command_line: hostile_str.clone(),
        output: None,
        status: ToolStatus::Ok,
    };
    assert!(ev_cmd.summary().starts_with("$ "));
}

/// The Carrel stages one item per text block, so the Derived field (the
/// default) derives each glyph's y/z from its item where the staged
/// (absolute) positions put it: the card blocks (each at its group's origin)
/// and the stack labels (each at its stack's position, in its own group
/// since that day; they shared group 0 before, which no single item can
/// describe).
#[test]
fn derived_positions_match_the_staged_ones() {
    use glyph_field::Transcode;

    let mut s = Stream::new("derived_sess", "/work");
    s.prompt("p0", 1, "Create two files")
        .say("s1", 1, "Writing src/main.rs.")
        .write("t1", 1, "src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n")
        .say("s2", 1, "Writing README.md.")
        .write("t2", 1, "README.md", "# Title\n\nSome prose, é and ✓.\n");
    let session = s.session();
    let rev_engine = RevisionEngine::for_session(&session);

    let staged = super::stage_agent_session(Some(crate::atlas::default_trie_ref()), &[], session, rev_engine);
    // The item table and the group → item map, as the scene builds them from
    // the view registry (`GlyphScene::new`).
    let items = &staged.views.item_table();
    let item_of_group = staged.views.item_of_group_rows(staged.groups.len());
    let instances = staged.instances.instances();
    assert!(!instances.is_empty(), "the session staged glyphs");
    // One item per block, no padding (until the view registry, 2026-10-11,
    // the table had one row per GROUP, so a group id could be read as an
    // item): row 0 and every group that holds no block have none.
    assert_eq!(items.len(), staged.views.len(), "one item per view");
    assert!(items.len() < staged.groups.len(), "{} items for {} group rows: the table is padded again", items.len(), staged.groups.len());
    let item_groups: Vec<u32> = items.iter().map(|p| p.group).collect();
    let transcode = glyph_field_derived::upload::DerivedTranscode { item_params: items, item_of_group: &item_of_group, no_items: false };

    let mut off_origin_groups = std::collections::BTreeSet::new();
    let mut rows_below_origin = 0usize;
    for (i, inst) in instances.iter().enumerate() {
        let mut slot = std::mem::MaybeUninit::<glyph_field_derived::DerivedSlot>::uninit();
        // SAFETY: `slot` is valid for one write of a DerivedSlot.
        let slot = unsafe {
            transcode.transcode(inst, slot.as_mut_ptr());
            slot.assume_init()
        };
        let item = glyph_field_derived::item_of(slot.item_and_group) as usize;
        let [y, z] = glyph_field_derived::derive_yz(slot.row, slot.wrap_segment() as u32, &items[item]);
        assert_eq!(slot.x.to_bits(), inst.pos[0].to_bits(), "glyph {i}: x is stored as staged");
        assert_eq!(
            (y.to_bits(), z.to_bits()),
            (inst.pos[1].to_bits(), inst.pos[2].to_bits()),
            "glyph {i} (group {}, row {}): derived y/z ({y}, {z}) vs staged ({}, {})",
            inst.group_id,
            inst.row,
            inst.pos[1],
            inst.pos[2]
        );
        assert_eq!(
            glyph_field_derived::resolve_group(slot.item_and_group, &item_groups, &[0]),
            inst.group_id,
            "glyph {i}: drawn with the group its block was staged in"
        );
        if items[item].origin_y != 0.0 || items[item].origin_z != 0.0 {
            off_origin_groups.insert(inst.group_id);
        }
        rows_below_origin += usize::from(inst.pos[1] < 0.0 && items[item].origin_y == 0.0);
    }
    // Not vacuous: the card blocks have rows below their origin, and both
    // stack labels sit off it, each in a group (and item) of its own.
    assert!(rows_below_origin > 0, "no card glyph below its block's first row");
    assert_eq!(off_origin_groups.len(), 2, "one off-origin group per stack label: {off_origin_groups:?}");
}
