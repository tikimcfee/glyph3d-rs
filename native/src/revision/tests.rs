//! Tests for the $R(n)$ multi-edit file revision engine and patch algebra.

use super::engine::RevisionEngine;
use super::patch::{
    apply_hunks, apply_string_replace, compute_line_diff, reconstruct_base_from_hunks,
    reverse_string_replace,
};
use crate::agent_transcript::{claude::parse_claude_session, DiffHunkRecord, FileActionRecord};
use crate::spatial_scene::workdesk::FileActionKind;
use serde_json::json;

#[test]
fn test_forward_and_reverse_string_replace() {
    let source = "fn hello() {\n    println!(\"old\");\n}\n";
    let old_str = "println!(\"old\");";
    let new_str = "println!(\"new\");";

    let modified = apply_string_replace(source, old_str, new_str, false).unwrap();
    assert_eq!(modified, "fn hello() {\n    println!(\"new\");\n}\n");

    let restored = reverse_string_replace(&modified, old_str, new_str, false).unwrap();
    assert_eq!(restored, source);
}

#[test]
fn test_forward_and_reverse_hunks() {
    let base = "line 1\nline 2\nline 3\n";
    let hunks = vec![DiffHunkRecord {
        old_start: 1,
        old_lines: 3,
        new_start: 1,
        new_lines: 4,
        lines: vec![
            " line 1".to_string(),
            "-line 2".to_string(),
            "+line 2 modified".to_string(),
            "+line 2.5 inserted".to_string(),
            " line 3".to_string(),
        ],
    }];

    let head = apply_hunks(base, &hunks).unwrap();
    assert_eq!(head, "line 1\nline 2 modified\nline 2.5 inserted\nline 3\n");

    let reconstructed = reconstruct_base_from_hunks(&head, &hunks).unwrap();
    assert_eq!(reconstructed, base);
}

#[test]
fn test_line_diff_computation() {
    let base = "apple\nbanana\ncherry\n";
    let head = "apple\nblueberry\ncherry\ndate\n";

    let (stats, hunks) = compute_line_diff(base, head);
    assert_eq!(stats.added, 2);   // blueberry, date
    assert_eq!(stats.removed, 1); // banana
    assert_eq!(hunks.len(), 1);
}

#[test]
fn test_revision_engine_multi_edit_sequence() {
    // Test a file created and edited 5 times: R0 -> R1 -> R2 -> R3 -> R4 -> R5
    let mut engine = RevisionEngine::new();
    let file = "src/counter.rs";

    // Turn 0: Write initial file (R0)
    let a0 = FileActionRecord {
        tool_id: "t0".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Write,
        summary: "Write src/counter.rs".to_string(),
        old_content: None,
        new_content: Some("pub struct Counter {\n    count: usize,\n}\n".to_string()),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(0, &a0);

    // Turn 1: Add new() constructor (R1)
    let a1 = FileActionRecord {
        tool_id: "t1".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Edit,
        summary: "Add new() constructor".to_string(),
        old_content: Some("pub struct Counter {\n    count: usize,\n}".to_string()),
        new_content: Some(
            "pub struct Counter {\n    count: usize,\n}\n\nimpl Counter {\n    pub fn new() -> Self {\n        Self { count: 0 }\n    }\n}"
                .to_string(),
        ),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(1, &a1);

    // Turn 2: Add inc() method (R2)
    let a2 = FileActionRecord {
        tool_id: "t2".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Edit,
        summary: "Add inc() method".to_string(),
        old_content: Some("    pub fn new() -> Self {\n        Self { count: 0 }\n    }".to_string()),
        new_content: Some(
            "    pub fn new() -> Self {\n        Self { count: 0 }\n    }\n\n    pub fn inc(&mut self) {\n        self.count += 1;\n    }"
                .to_string(),
        ),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(2, &a2);

    // Turn 3: Add dec() method (R3)
    let a3 = FileActionRecord {
        tool_id: "t3".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Edit,
        summary: "Add dec() method".to_string(),
        old_content: Some("    pub fn inc(&mut self) {\n        self.count += 1;\n    }".to_string()),
        new_content: Some(
            "    pub fn inc(&mut self) {\n        self.count += 1;\n    }\n\n    pub fn dec(&mut self) {\n        self.count = self.count.saturating_sub(1);\n    }"
                .to_string(),
        ),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(3, &a3);

    // Turn 4: Add get() method (R4)
    let a4 = FileActionRecord {
        tool_id: "t4".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Edit,
        summary: "Add get() method".to_string(),
        old_content: Some("        Self { count: 0 }\n    }".to_string()),
        new_content: Some(
            "        Self { count: 0 }\n    }\n\n    pub fn get(&self) -> usize {\n        self.count\n    }"
                .to_string(),
        ),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(4, &a4);

    // Turn 5: Add doc comments (R5)
    let a5 = FileActionRecord {
        tool_id: "t5".to_string(),
        file_path: file.to_string(),
        action: FileActionKind::Edit,
        summary: "Add doc comments".to_string(),
        old_content: Some("pub struct Counter {".to_string()),
        new_content: Some("/// Thread-safe counter structure.\npub struct Counter {".to_string()),
        original_file: None,
        hunks: Vec::new(),
    };
    engine.ingest_file_action(5, &a5);

    let history = engine.history(file).expect("history exists");
    assert_eq!(history.count(), 6); // R0, R1, R2, R3, R4, R5

    // Validate R0
    let r0 = history.get(0).unwrap();
    assert_eq!(r0.revision_index, 0);
    assert_eq!(r0.turn_index, 0);
    assert!(r0.text.contains("pub struct Counter"));
    assert!(!r0.text.contains("pub fn new"));

    // Validate R1
    let r1 = history.get(1).unwrap();
    assert_eq!(r1.revision_index, 1);
    assert_eq!(r1.turn_index, 1);
    assert!(r1.text.contains("pub fn new"));
    assert!(!r1.text.contains("pub fn inc"));

    // Validate R5 (latest)
    let r5 = history.latest().unwrap();
    assert_eq!(r5.revision_index, 5);
    assert_eq!(r5.turn_index, 5);
    assert!(r5.text.contains("/// Thread-safe counter structure."));
    assert!(r5.text.contains("pub fn get"));
    assert!(r5.text.contains("pub fn dec"));

    // Turn lookup
    assert_eq!(history.revision_for_turn(3).unwrap().revision_index, 3);
}

#[test]
fn test_backward_reconstruction_via_disk_resolver() {
    let mut engine = RevisionEngine::new().with_disk_resolver(|_path| {
        Some("fn foo() {\n    println!(\"disk version\");\n}\n".to_string())
    });

    let hunks = vec![DiffHunkRecord {
        old_start: 1,
        old_lines: 3,
        new_start: 1,
        new_lines: 3,
        lines: vec![
            " fn foo() {".to_string(),
            "-    println!(\"original\");".to_string(),
            "+    println!(\"disk version\");".to_string(),
            " }".to_string(),
        ],
    }];

    let action = FileActionRecord {
        tool_id: "t_edit".to_string(),
        file_path: "src/foo.rs".to_string(),
        action: FileActionKind::Edit,
        summary: "Update println".to_string(),
        old_content: None,
        new_content: None,
        original_file: None,
        hunks,
    };

    engine.ingest_file_action(1, &action);

    let history = engine.history("src/foo.rs").unwrap();
    assert_eq!(history.count(), 2);

    let r0 = history.get(0).unwrap();
    assert!(r0.text.contains("println!(\"original\");"));

    let r1 = history.get(1).unwrap();
    assert!(r1.text.contains("println!(\"disk version\");"));
}

#[test]
fn test_engine_ingest_parsed_session() {
    let transcript = vec![
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Create lib.rs" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "lib.rs", "content": "pub fn a() -> u32 { 1 }" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "lib.rs", "content": "pub fn a() -> u32 { 1 }" },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1" }] }
        }).to_string(),
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Modify lib.rs" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t2", "name": "Edit", "input": {
                        "file_path": "lib.rs",
                        "old_string": "1",
                        "new_string": "2"
                    } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "filePath": "lib.rs", "structuredPatch": [{ "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-pub fn a() -> u32 { 1 }", "+pub fn a() -> u32 { 2 }"] }] },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2" }] }
        }).to_string(),
    ].join("\n");

    let session = parse_claude_session(&transcript, "sess");
    let mut engine = RevisionEngine::new();
    engine.ingest_session(&session);

    let hist = engine.history("lib.rs").expect("lib.rs history");
    assert_eq!(hist.count(), 2);
    assert_eq!(hist.get(0).unwrap().text.as_str(), "pub fn a() -> u32 { 1 }");
    assert_eq!(hist.get(1).unwrap().text.as_str(), "pub fn a() -> u32 { 2 }");
}
