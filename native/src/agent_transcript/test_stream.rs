//! Event streams written in EVENTS.md's form, for tests that need a session
//! of a particular shape (ten turns, two writes) that gomuxai's corpus
//! doesn't hold. The corpus itself (`tools/vendor/gomuxai/`) is what holds
//! the reader to the contract; these only build sessions to stage.

use serde_json::{json, Value};

use super::events::AgentSession;

/// A stream being written: its lines, with `seq` and `at` counted.
pub struct Stream {
    lines: Vec<String>,
    seq: u64,
}

impl Stream {
    /// A stream that opens with a claude session in `cwd`.
    pub fn new(session_id: &str, cwd: &str) -> Self {
        let session = json!({
            "type": "session", "session_id": session_id, "harness": "claude",
            "path": format!("{cwd}/{session_id}.jsonl"), "cwd": cwd, "project": "widget",
        });
        Self { lines: vec![session.to_string()], seq: 0 }
    }

    fn event(&mut self, id: &str, op: &str, turn: usize, kind: &str, body: Value) -> &mut Self {
        let mut line = json!({
            "type": "event", "id": id, "op": op, "seq": self.seq, "at": self.seq * 100, "line": self.seq,
            "ts": "2026-10-20T15:00:00.000Z",
            "turn": { "id": format!("turn-{turn}"), "index": turn },
            "kind": kind,
        });
        line[kind] = body;
        self.lines.push(line.to_string());
        self.seq += 1;
        self
    }

    /// A typed prompt that opens (or, mid-turn, joins) turn `turn` (1-based).
    pub fn prompt(&mut self, id: &str, turn: usize, text: &str) -> &mut Self {
        self.event(id, "add", turn, "prompt", json!({ "text": text, "origin": "human" }))
    }

    pub fn think(&mut self, id: &str, turn: usize, text: &str) -> &mut Self {
        self.event(id, "add", turn, "think", json!({ "text": text }))
    }

    pub fn say(&mut self, id: &str, turn: usize, text: &str) -> &mut Self {
        self.event(id, "add", turn, "say", json!({ "text": text }))
    }

    /// A tool call added pending, then its result as an update.
    pub fn tool(&mut self, id: &str, turn: usize, tool: Value, result: Value, update: Value) -> &mut Self {
        let mut pending = tool.clone();
        pending["result"] = json!({ "status": "pending" });
        self.event(id, "add", turn, "tool", pending);
        let mut done = tool;
        done["result"] = result;
        if let (Some(done), Some(more)) = (done.as_object_mut(), update.as_object()) {
            done.extend(more.clone());
        }
        self.event(id, "update", turn, "tool", done)
    }

    /// A Write of `body` to `target`, its file text the result's.
    pub fn write(&mut self, id: &str, turn: usize, target: &str, body: &str) -> &mut Self {
        self.tool(
            id,
            turn,
            json!({ "name": "Write", "input": { "file_path": target, "content": body }, "action": "write", "target": target }),
            json!({ "status": "ok" }),
            json!({ "file": { "text": body, "start": 1, "total": body.lines().count() } }),
        )
    }

    /// An Edit replacing `old` with `new` in `target`, whose result held
    /// the file: its file text is `after`, its patch `hunk`.
    pub fn edit(&mut self, id: &str, turn: usize, target: &str, (old, new): (&str, &str), after: &str, hunk: Value) -> &mut Self {
        self.tool(
            id,
            turn,
            json!({ "name": "Edit", "input": { "file_path": target, "old_string": old, "new_string": new }, "action": "edit", "target": target }),
            json!({ "status": "ok" }),
            json!({
                "meta": { "added": 1, "removed": 1, "patch": [hunk] },
                "file": { "text": after, "start": 1, "total": after.lines().count() },
            }),
        )
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn session(&self) -> AgentSession {
        AgentSession::from_stream_str(&self.text()).expect("a written stream reads")
    }
}
