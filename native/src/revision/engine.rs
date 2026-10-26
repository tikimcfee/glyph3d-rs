//! The $R(n)$ multi-edit file revision engine.
//!
//! Maintains file revision histories across successive agent turns ($R_0 \dots R_N$)
//! with automatic forward delta derivation and backward reconstruction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::agent_transcript::types::Replacement;
use crate::agent_transcript::{AgentSession, FileActionRecord, TranscriptEventKind};
use crate::spatial_scene::workdesk::FileActionKind;
use super::patch::{apply_hunks, apply_string_replace, compute_line_diff, reconstruct_base_from_hunks};
use super::types::{DiffStats, FileRevision, FileRevisionHistory};

/// Callback type for resolving current on-disk file content.
pub type DiskResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Multi-file revision engine tracking $R_0 \dots R_N$ across all agent turns.
#[derive(Clone)]
pub struct RevisionEngine {
    histories: HashMap<String, FileRevisionHistory>,
    disk_resolver: Option<DiskResolver>,
}

impl std::fmt::Debug for RevisionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevisionEngine")
            .field("histories_count", &self.histories.len())
            .finish()
    }
}

impl Default for RevisionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl RevisionEngine {
    pub fn new() -> Self {
        Self {
            histories: HashMap::new(),
            disk_resolver: None,
        }
    }

    /// Provide a callback to read file contents from disk (for backwards reconstruction).
    pub fn with_disk_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.disk_resolver = Some(Arc::new(resolver));
        self
    }

    /// Access the revision history for a specific file.
    pub fn history(&self, file_path: &str) -> Option<&FileRevisionHistory> {
        self.histories.get(file_path)
    }

    /// Access the mutable revision history for a specific file.
    pub fn history_mut(&mut self, file_path: &str) -> Option<&mut FileRevisionHistory> {
        self.histories.get_mut(file_path)
    }

    /// All recorded file revision histories.
    pub fn all_histories(&self) -> &HashMap<String, FileRevisionHistory> {
        &self.histories
    }

    /// List of all touched file paths.
    pub fn file_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.histories.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// The engine for one session: its file actions ingested in beat order,
    /// with a disk resolver for the actions whose events carry no file text
    /// (a target relative to the session's cwd is read there).
    pub fn for_session(session: &AgentSession) -> Self {
        let cwd = session.cwd().map(PathBuf::from);
        let mut engine = Self::new().with_disk_resolver(move |target: &str| {
            let path = Path::new(target);
            let full = if path.is_absolute() { path.to_path_buf() } else { cwd.as_ref()?.join(path) };
            std::fs::read_to_string(full).ok()
        });
        engine.ingest_session(session);
        engine
    }

    /// Ingest every file beat of a session, in beat order.
    pub fn ingest_session(&mut self, session: &AgentSession) {
        for beat in session.linearize_events(None) {
            if let TranscriptEventKind::FileRead { action_record, .. }
            | TranscriptEventKind::FileEdit { action_record, .. }
            | TranscriptEventKind::FileWrite { action_record, .. } = &beat.kind
            {
                self.ingest_file_action_with_event(beat.turn_index, Some(beat.index), action_record);
            }
        }
    }

    /// Ingest a single file action record into the target file's revision history.
    pub fn ingest_file_action(&mut self, turn_index: usize, action: &FileActionRecord) {
        self.ingest_file_action_with_event(turn_index, None, action);
    }

    /// Ingest a single file action record with its beat's index.
    ///
    /// An event's `file` text is the revision itself: a whole read's text,
    /// a write's, an edit's file after it (whose base is that text with the
    /// edit's `meta.patch` hunks reverse-applied, or else its replacements
    /// undone: no disk). A partial read's text is a window, not a revision.
    /// Only an action whose event has no file text falls back to replaying
    /// its input against the latest revision, or, for the first sight of a
    /// file, against the file on disk now (the disk resolver), reconstructed
    /// backward through the edit.
    pub fn ingest_file_action_with_event(
        &mut self,
        turn_index: usize,
        event_index: Option<usize>,
        action: &FileActionRecord,
    ) {
        let path = &action.file_path;
        let resolver = self.disk_resolver.clone();
        let disk = |p: &str| resolver.as_ref().and_then(|resolve| resolve(p));
        let base_rev = |text: String| FileRevision {
            revision_index: 0,
            turn_index,
            event_index,
            action: FileActionKind::Read,
            summary: format!("Base {path}"),
            text: Arc::new(text),
            diff_stats: DiffStats::default(),
            hunks: Vec::new(),
        };
        let changed_rev = |kind: FileActionKind, prev: Option<&str>, text: String| {
            let (diff_stats, computed) = match prev {
                Some(prev) => compute_line_diff(prev, &text),
                None => {
                    let (added, removed) = action.added_removed.unwrap_or((text.lines().count(), 0));
                    (DiffStats::new(added, removed), Vec::new())
                }
            };
            FileRevision {
                revision_index: 0,
                turn_index,
                event_index,
                action: kind,
                summary: action.summary.clone(),
                text: Arc::new(text),
                diff_stats,
                hunks: if action.hunks.is_empty() { computed } else { action.hunks.clone() },
            }
        };

        match action.action {
            FileActionKind::Write => {
                let new_text = action
                    .file
                    .as_ref()
                    .map(|f| f.text.clone())
                    .or_else(|| action.written.clone())
                    .unwrap_or_default();
                let history = self.history_entry(path);
                let prev = history.latest().map(|r| r.text.clone());
                history.push_revision(changed_rev(FileActionKind::Write, prev.as_deref().map(String::as_str), new_text));
            }
            FileActionKind::Edit => {
                if let Some(post) = action.file.as_ref().map(|f| f.text.clone()) {
                    let history = self.history_entry(path);
                    if history.is_empty() {
                        let base = if !action.hunks.is_empty() {
                            reconstruct_base_from_hunks(&post, &action.hunks).ok()
                        } else {
                            undo_replacements(&post, &action.replacements)
                        };
                        if let Some(base) = base {
                            history.push_revision(base_rev(base));
                        }
                    }
                    let prev = history.latest().map(|r| r.text.clone());
                    history.push_revision(changed_rev(FileActionKind::Edit, prev.as_deref().map(String::as_str), post));
                    return;
                }
                // No file text: replay the edit's input.
                let latest = self.history(path).and_then(|h| h.latest()).map(|r| r.text.clone());
                if let Some(prev) = latest {
                    let derived = if !action.replacements.is_empty() {
                        redo_replacements(&prev, &action.replacements).unwrap_or_else(|| prev.to_string())
                    } else if !action.hunks.is_empty() {
                        apply_hunks(&prev, &action.hunks).unwrap_or_else(|_| prev.to_string())
                    } else {
                        prev.to_string()
                    };
                    self.history_entry(path).push_revision(changed_rev(FileActionKind::Edit, Some(prev.as_str()), derived));
                    return;
                }
                // The first sight of the file: backward from the disk head.
                let base = disk(path).and_then(|head| {
                    if !action.hunks.is_empty() {
                        reconstruct_base_from_hunks(&head, &action.hunks).ok()
                    } else if !action.replacements.is_empty() {
                        undo_replacements(&head, &action.replacements)
                    } else {
                        Some(head)
                    }
                });
                let (base, summary_base) = match base {
                    Some(base) => (base, None),
                    // Neither the event nor the disk has the file: the
                    // replaced snippets are all there is.
                    None if !action.replacements.is_empty() => {
                        let join = |f: fn(&Replacement) -> &str| action.replacements.iter().map(f).collect::<Vec<_>>().join("\n");
                        (join(|r| r.old.as_str()), Some(join(|r| r.new.as_str())))
                    }
                    None => return,
                };
                let derived = match summary_base {
                    Some(snippet) => snippet,
                    None if !action.replacements.is_empty() => {
                        redo_replacements(&base, &action.replacements).unwrap_or_else(|| base.clone())
                    }
                    None if !action.hunks.is_empty() => apply_hunks(&base, &action.hunks).unwrap_or_else(|_| base.clone()),
                    None => base.clone(),
                };
                let history = self.history_entry(path);
                history.push_revision(base_rev(base.clone()));
                history.push_revision(changed_rev(FileActionKind::Edit, Some(&base), derived));
            }
            FileActionKind::Read => {
                let text = match &action.file {
                    Some(f) if f.is_whole() => Some(f.text.clone()),
                    Some(_) => None,
                    None => disk(path),
                };
                if let Some(content) = text {
                    let history = self.history_entry(path);
                    if history.latest().is_none_or(|lat| lat.text.as_str() != content) {
                        let mut rev = base_rev(content);
                        rev.summary = action.summary.clone();
                        history.push_revision(rev);
                    }
                }
            }
        }
    }

    fn history_entry(&mut self, path: &str) -> &mut FileRevisionHistory {
        self.histories
            .entry(path.to_string())
            .or_insert_with(|| FileRevisionHistory::new(path.to_string()))
    }
}

/// An edit's replacements made, in order; None if one doesn't apply.
fn redo_replacements(text: &str, replacements: &[Replacement]) -> Option<String> {
    replacements
        .iter()
        .try_fold(text.to_string(), |t, r| apply_string_replace(&t, &r.old, &r.new, r.all).ok())
}

/// An edit's replacements undone, last first; None if one doesn't apply.
fn undo_replacements(text: &str, replacements: &[Replacement]) -> Option<String> {
    if replacements.is_empty() {
        return None;
    }
    replacements
        .iter()
        .rev()
        .try_fold(text.to_string(), |t, r| apply_string_replace(&t, &r.new, &r.old, r.all).ok())
}
