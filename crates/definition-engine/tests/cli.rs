//! The `defn` binary end to end, as an agent drives it: the whole loop
//! through the command line against a scratch store, so the argument wiring
//! (one selection, one choice, numbers from 1) is held, not just the library.

use std::path::PathBuf;
use std::process::{Command, Output};

struct Store(PathBuf);

impl Store {
    fn new(name: &str) -> Store {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("defn-cli-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Store(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_defn")).arg("--store").arg(&self.0).args(args).output().unwrap()
    }

    /// Run, require success, return stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "defn {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut with_json = vec!["--json"];
        with_json.extend_from_slice(args);
        serde_json::from_str(&self.ok(&with_json)).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_definition_loop_through_the_command_line() {
    let store = Store::new("loop");
    let run = store.json(&["new", "Letter", "--text", "An ephemeral joy. An ephemeral grief."])["run"].as_str().unwrap().to_string();

    // The second occurrence, by --nth.
    let asked = store.json(&["expand", &run, "--word", "ephemeral", "--nth", "2"]);
    assert_eq!(asked["request"]["span"]["start"], 21);
    assert_eq!(asked["request"]["context"], "An ephemeral grief.");

    let pending = store.json(&["pending"]);
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(pending[0]["word"], "ephemeral");
    assert_eq!(pending[0]["marked"], "An ⟦ephemeral⟧ grief.");
    assert_eq!(pending[0]["version"], 0);

    store.ok(&["answer", &run, "q1", r#"{"substitutes": ["fleeting", "brief"], "related": [{"name": "impermanence", "category": "philosophy"}]}"#]);
    assert_eq!(store.json(&["pending"]).as_array().unwrap().len(), 0);
    assert!(store.ok(&["options", &run, "q1"]).contains("2. brief"));

    // --pick counts from 1.
    let taken = store.json(&["substitute", &run, "q1", "--pick", "2"]);
    assert_eq!(taken["text"], "An ephemeral joy. An brief grief.");

    // Recurse: a tap on a byte inside the inserted word selects it whole.
    let asked = store.json(&["expand", &run, "--at", "23"]);
    assert_eq!(asked["request"]["word"], "brief");
    store.ok(&["substitute", &run, "q2", "--text", "short"]);
    assert_eq!(store.ok(&["show", &run]), "An ephemeral joy. An short grief.\n");
    assert_eq!(store.ok(&["show", &run, "--at", "0"]), "An ephemeral joy. An ephemeral grief.\n");

    let log = store.ok(&["log", &run]);
    assert!(log.contains("[-ephemeral-]{+brief+}") && log.contains("* v2"), "{log}");

    // Back to v1, fork it, and take a different path there.
    store.ok(&["checkout", &run, "1"]);
    let refused = store.run(&["substitute", &run, "q2", "--text", "x"]);
    assert!(!refused.status.success());
    let fork = store.json(&["fork", &run])["run"].as_str().unwrap().to_string();
    assert_eq!(store.ok(&["show", &fork]), "An ephemeral joy. An brief grief.\n");
    let list = store.json(&["list"]);
    assert_eq!(list.as_array().unwrap().len(), 2);
}

#[test]
fn a_store_dictionary_answers_at_expand_and_is_taken_by_number() {
    let store = Store::new("dictionary");
    let dir = store.0.join("dictionaries");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("webster.json"), r#"{"joy": "1. The passion excited by good. 2. That which causes joy."}"#).unwrap();
    assert!(store.ok(&["lookup", "Joys"]).contains("webster (joy):"));
    let run = store.json(&["new", "t", "--text", "An ephemeral joy."])["run"].as_str().unwrap().to_string();
    let asked = store.json(&["expand", &run, "--word", "joy"]);
    assert_eq!(asked["request"]["dictionary"]["senses"][1], "That which causes joy.");
    let pending = store.json(&["pending"]);
    assert_eq!(pending[0]["dictionary"]["senses"], 2, "the agent is told what the reader already has");
    let taken = store.json(&["substitute", &run, "q1", "--dictionary", "2"]);
    assert_eq!(taken["text"], "An ephemeral That which causes joy..");
}

#[test]
fn the_command_line_refuses_ambiguous_or_zero_numbers() {
    let store = Store::new("refusals");
    let run = store.json(&["new", "t", "--text", "one two"])["run"].as_str().unwrap().to_string();
    assert!(!store.run(&["expand", &run, "--word", "one", "--at", "4"]).status.success(), "two selections");
    assert!(!store.run(&["expand", &run, "--at", "0", "--nth", "2"]).status.success(), "--nth without --word");
    assert!(!store.run(&["expand", &run]).status.success(), "no selection");
    store.ok(&["expand", &run, "--span", "0", "3"]);
    store.ok(&["answer", &run, "q1", r#"{"substitutes": ["1"]}"#]);
    let zero = store.run(&["substitute", &run, "q1", "--pick", "0"]);
    assert!(String::from_utf8_lossy(&zero.stderr).contains("counts from 1"));
    let typo = store.run(&["answer", &run, "q1", r#"{"substitute": ["1"]}"#]);
    assert!(!typo.status.success(), "a misspelt key is refused");
}
