//! Runs on disk: one JSON file each, under `<root>/runs/`.
//!
//! The store directory is the state the `defn` CLI, an agent and a renderer
//! share; nothing else carries a run between them. A save writes a sibling
//! temporary file and renames it over the run, so a reader never sees half a
//! file. Two writers of the SAME run still race (the later save wins whole);
//! the loop has one writer per run at a time today (the reader's client, or
//! the agent answering), and a lock is the next step when that stops being
//! true.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::run::{Request, Run, Source};

/// The environment variable that names the store root, over the default.
pub const STORE_ENV: &str = "DEFN_STORE";

/// How often [`Store::wait_for_pending`] looks. A look reads only the runs
/// directory's listing and modification times until one moved, so a short
/// period costs nothing while the store is quiet.
pub const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(100);

pub struct Store {
    root: PathBuf,
}

/// One line of `defn list`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RunSummary {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub head: usize,
    pub tip: usize,
    pub pending: usize,
}

impl Store {
    /// `$DEFN_STORE`, else `$XDG_DATA_HOME/glyph3d/definition-runs`, else
    /// `~/.local/share/glyph3d/definition-runs`.
    pub fn default_root() -> Result<PathBuf, String> {
        if let Some(root) = std::env::var_os(STORE_ENV) {
            return Ok(PathBuf::from(root));
        }
        let data = match std::env::var_os("XDG_DATA_HOME") {
            Some(data) => PathBuf::from(data),
            None => PathBuf::from(std::env::var_os("HOME").ok_or("neither DEFN_STORE, XDG_DATA_HOME nor HOME is set")?).join(".local/share"),
        };
        Ok(data.join("glyph3d/definition-runs"))
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Store, String> {
        let root = root.into();
        let runs = root.join("runs");
        fs::create_dir_all(&runs).map_err(|e| format!("cannot create {}: {e}", runs.display()))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join("runs").join(format!("{id}.json"))
    }

    /// A fresh run id: the title's slug and the creation time's nanoseconds
    /// in base 36, so ids sort by title and never repeat in practice (a
    /// collision is refused by [`Store::create`], never overwritten).
    pub fn new_id(title: &str) -> String {
        let mut slug: String = title
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        slug.truncate(32);
        let slug = slug.trim_end_matches('-');
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock reads before 1970")
            .as_nanos();
        let mut stamp = String::new();
        let mut rest = nanos;
        while rest > 0 {
            let digit = (rest % 36) as u32;
            stamp.insert(0, char::from_digit(digit, 36).expect("a base-36 digit"));
            rest /= 36;
        }
        if slug.is_empty() {
            format!("run-{stamp}")
        } else {
            format!("{slug}-{stamp}")
        }
    }

    /// Make a run and write it, refusing to replace a run of the same id.
    pub fn create(&self, title: &str, source: Source, base: String) -> Result<Run, String> {
        let run = Run::new(Self::new_id(title), title.to_string(), source, base, crate::now_seconds());
        self.write_new(&run)?;
        Ok(run)
    }

    /// Write a run that must not exist yet (a created or forked run).
    pub fn write_new(&self, run: &Run) -> Result<(), String> {
        run.validate()?;
        let path = self.path(&run.id);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        file.write_all(&Self::encode(run)).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Replace a run on disk whole.
    pub fn save(&self, run: &Run) -> Result<(), String> {
        run.validate()?;
        let path = self.path(&run.id);
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&temporary, Self::encode(run)).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
        fs::rename(&temporary, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }

    fn encode(run: &Run) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(run).expect("a run serializes");
        bytes.push(b'\n');
        bytes
    }

    /// The run file's modification time and length: what a host watching
    /// one run compares to know it changed (a save renames a new file over
    /// it, so both move).
    pub fn stamp(&self, id: &str) -> Result<(std::time::SystemTime, u64), String> {
        let path = self.path(id);
        let metadata = fs::metadata(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        let modified = metadata.modified().map_err(|e| format!("{}: no modification time: {e}", path.display()))?;
        Ok((modified, metadata.len()))
    }

    /// The dictionaries this store reads: `<root>/dictionaries/*.json`, then
    /// `$DEFN_DICTIONARY` (`crate::dictionary`).
    pub fn dictionaries(&self) -> Result<crate::dictionary::Dictionaries, String> {
        crate::dictionary::Dictionaries::load(&self.root.join("dictionaries"))
    }

    /// Every run id in the store, sorted.
    pub fn ids(&self) -> Result<Vec<String>, String> {
        let runs = self.root.join("runs");
        let mut ids: Vec<String> = fs::read_dir(&runs)
            .map_err(|e| format!("cannot list {}: {e}", runs.display()))?
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                name.strip_suffix(".json").map(str::to_string)
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// The run named by `id`, or by a prefix of exactly one run's id.
    pub fn load(&self, id: &str) -> Result<Run, String> {
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            return Err(format!("{id:?} is not a run id"));
        }
        let ids = self.ids()?;
        let id = if ids.iter().any(|known| known == id) {
            id.to_string()
        } else {
            let matches: Vec<&String> = ids.iter().filter(|known| known.starts_with(id)).collect();
            match matches.as_slice() {
                [one] => one.to_string(),
                [] => return Err(format!("no run {id} in {}", self.root.display())),
                many => return Err(format!("{id} names {} runs: {}", many.len(), many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))),
            }
        };
        let path = self.path(&id);
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let run: Run = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        if run.id != id {
            return Err(format!("{} holds run {}", path.display(), run.id));
        }
        run.validate().map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(run)
    }

    pub fn summaries(&self) -> Result<Vec<RunSummary>, String> {
        let mut summaries = self
            .ids()?
            .iter()
            .map(|id| {
                let run = self.load(id)?;
                Ok(RunSummary {
                    pending: run.requests.iter().filter(|r| r.expansion.is_none()).count(),
                    id: run.id,
                    title: run.title,
                    created_at: run.created_at,
                    head: run.head,
                    tip: run.steps.len(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        summaries.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(summaries)
    }

    /// The runs directory's listing with each file's modification time and
    /// length: what changes when any run is created or saved (a save renames
    /// a whole new file over the run).
    fn fingerprint(&self) -> Result<Vec<(String, Option<std::time::SystemTime>, u64)>, String> {
        let runs = self.root.join("runs");
        let mut entries: Vec<_> = fs::read_dir(&runs)
            .map_err(|e| format!("cannot list {}: {e}", runs.display()))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                Some((entry.file_name().into_string().ok()?, metadata.modified().ok(), metadata.len()))
            })
            .collect();
        entries.sort();
        Ok(entries)
    }

    /// Block until at least one request is pending, then return them all;
    /// after `timeout` with none, return none. Looks every `poll`, and reads
    /// the runs only when the directory's fingerprint moved.
    pub fn wait_for_pending(&self, timeout: Option<std::time::Duration>, poll: std::time::Duration) -> Result<Vec<(String, Request)>, String> {
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut seen = None;
        loop {
            let now = self.fingerprint()?;
            if seen.as_ref() != Some(&now) {
                let pending = self.pending()?;
                if !pending.is_empty() {
                    return Ok(pending);
                }
                seen = Some(now);
            }
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return Ok(Vec::new());
            }
            std::thread::sleep(poll);
        }
    }

    /// Every request no agent has answered yet, with its run's id: the
    /// agent's work queue.
    pub fn pending(&self) -> Result<Vec<(String, Request)>, String> {
        let mut pending = Vec::new();
        for id in self.ids()? {
            let run = self.load(&id)?;
            pending.extend(run.requests.into_iter().filter(|r| r.expansion.is_none()).map(|r| (run.id.clone(), r)));
        }
        Ok(pending)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::run::Choice;
    use crate::text::Span;

    /// A fresh, empty directory under the system temp dir, removed when
    /// dropped.
    pub(crate) struct ScratchDir(pub PathBuf);

    impl ScratchDir {
        pub(crate) fn new(name: &str) -> ScratchDir {
            let path = std::env::temp_dir().join(format!("definition-engine-{name}-{}-{}", std::process::id(), Store::new_id("t")));
            fs::create_dir_all(&path).unwrap();
            ScratchDir(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_run_round_trips_and_is_found_by_prefix() {
        let scratch = ScratchDir::new("round-trip");
        let store = Store::open(&scratch.0).unwrap();
        let mut run = store.create("A Title!", Source::Inline, "one two".into()).unwrap();
        assert!(run.id.starts_with("a-title-"), "{}", run.id);
        let q = run.expand(Span { start: 0, end: 3 }, 1).unwrap().id.clone();
        run.substitute(&q, Choice::Text { text: "1".into() }, 2).unwrap();
        store.save(&run).unwrap();
        let loaded = store.load("a-title").unwrap();
        assert_eq!(loaded, run);
        assert_eq!(loaded.head_text(), "1 two");
        assert!(store.write_new(&run).is_err(), "create never overwrites");
    }

    #[test]
    fn a_corrupt_or_misplaced_file_is_refused_by_path() {
        let scratch = ScratchDir::new("corrupt");
        let store = Store::open(&scratch.0).unwrap();
        let run = store.create("x", Source::Inline, "text".into()).unwrap();
        let path = store.path(&run.id);
        let mut tampered: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        tampered["head"] = 3.into();
        fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        let error = store.load(&run.id).unwrap_err();
        assert!(error.contains(&path.display().to_string()) && error.contains("head 3"), "{error}");
        assert!(store.load("../etc").is_err());
    }

    #[test]
    fn waiting_wakes_when_a_reader_asks_and_times_out_when_none_does() {
        let scratch = ScratchDir::new("wait");
        let store = Store::open(&scratch.0).unwrap();
        let run = store.create("w", Source::Inline, "a word".into()).unwrap();
        let quiet = store.wait_for_pending(Some(std::time::Duration::from_millis(50)), std::time::Duration::from_millis(5)).unwrap();
        assert!(quiet.is_empty());

        let root = scratch.0.clone();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let store = Store::open(root).unwrap();
            let mut run = store.load(&run.id).unwrap();
            run.expand(Span { start: 2, end: 6 }, 1).unwrap();
            store.save(&run).unwrap();
        });
        let woken = store.wait_for_pending(Some(std::time::Duration::from_secs(10)), std::time::Duration::from_millis(5)).unwrap();
        reader.join().unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(woken[0].1.word, "word");
    }

    #[test]
    fn pending_lists_unanswered_requests_across_runs() {
        let scratch = ScratchDir::new("pending");
        let store = Store::open(&scratch.0).unwrap();
        for title in ["first", "second"] {
            let mut run = store.create(title, Source::Inline, "a word".into()).unwrap();
            run.expand(Span { start: 2, end: 6 }, 1).unwrap();
            store.save(&run).unwrap();
        }
        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|(_, request)| request.word == "word"));
    }
}
