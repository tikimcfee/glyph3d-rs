//! The $R(n)$ multi-edit file revision engine.
//!
//! Maintains file revision histories across successive agent turns ($R_0 \dots R_N$)
//! with automatic forward delta derivation and backward reconstruction.

use std::collections::HashMap;
use std::sync::Arc;
use crate::agent_transcript::{AgentSession, AgentTurn, FileActionRecord};
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

    /// Ingest an entire `AgentSession` with all its linearized events and turns.
    pub fn ingest_session(&mut self, session: &AgentSession) {
        let events = session.linearize_events(None);
        let mut file_action_count = 0;
        for event in &events {
            match &event.kind {
                crate::agent_transcript::TranscriptEventKind::FileEdit { action_record, .. } => {
                    self.ingest_file_action_with_event(event.turn_index, Some(event.index), action_record);
                    file_action_count += 1;
                }
                crate::agent_transcript::TranscriptEventKind::FileRead { action_record, .. } => {
                    self.ingest_file_action_with_event(event.turn_index, Some(event.index), action_record);
                    file_action_count += 1;
                }
                crate::agent_transcript::TranscriptEventKind::FileWrite { action_record, .. } => {
                    self.ingest_file_action_with_event(event.turn_index, Some(event.index), action_record);
                    file_action_count += 1;
                }
                _ => {}
            }
        }

        if file_action_count == 0 {
            for turn in &session.turns {
                self.ingest_turn(turn);
            }
        }
    }

    /// Ingest a single `AgentTurn` and its contained file actions.
    pub fn ingest_turn(&mut self, turn: &AgentTurn) {
        for action in &turn.file_actions {
            self.ingest_file_action(turn.turn_index, action);
        }
    }

    /// Ingest a single file action record into the target file's revision history.
    pub fn ingest_file_action(&mut self, turn_index: usize, action: &FileActionRecord) {
        self.ingest_file_action_with_event(turn_index, None, action);
    }

    /// Ingest a single file action record with its associated atomic narrative beat index.
    pub fn ingest_file_action_with_event(
        &mut self,
        turn_index: usize,
        event_index: Option<usize>,
        action: &FileActionRecord,
    ) {
        let path = &action.file_path;
        let history = self
            .histories
            .entry(path.clone())
            .or_insert_with(|| FileRevisionHistory::new(path.clone()));

        match action.action {
            FileActionKind::Write => {
                let new_text = action
                    .new_content
                    .clone()
                    .unwrap_or_default();

                let (diff_stats, hunks) = if let Some(latest) = history.latest() {
                    compute_line_diff(&latest.text, &new_text)
                } else if let Some(orig) = action.original_file.as_ref().or(action.old_content.as_ref()) {
                    // Pre-existing file overwritten
                    let r0 = FileRevision {
                        revision_index: 0,
                        turn_index,
                        event_index,
                        action: FileActionKind::Read,
                        summary: format!("Base {path}"),
                        text: Arc::new(orig.clone()),
                        diff_stats: DiffStats::default(),
                        hunks: Vec::new(),
                    };
                    history.push_revision(r0);
                    compute_line_diff(orig, &new_text)
                } else {
                    (DiffStats::new(new_text.lines().count(), 0), Vec::new())
                };

                let rev = FileRevision {
                    revision_index: history.count(),
                    turn_index,
                    event_index,
                    action: FileActionKind::Write,
                    summary: action.summary.clone(),
                    text: Arc::new(new_text),
                    diff_stats,
                    hunks,
                };
                history.push_revision(rev);
            }
            FileActionKind::Edit => {
                if let Some(latest) = history.latest() {
                    let prev_text = &latest.text;
                    let derived_text = if let (Some(ref old_s), Some(ref new_s)) =
                        (&action.old_content, &action.new_content)
                    {
                        apply_string_replace(prev_text, old_s, new_s, false)
                            .unwrap_or_else(|_| prev_text.to_string())
                    } else if !action.hunks.is_empty() {
                        apply_hunks(prev_text, &action.hunks)
                            .unwrap_or_else(|_| prev_text.to_string())
                    } else if let Some(ref new_c) = action.new_content {
                        new_c.clone()
                    } else {
                        prev_text.to_string()
                    };

                    let (diff_stats, hunks) = compute_line_diff(prev_text, &derived_text);
                    let rev = FileRevision {
                        revision_index: history.count(),
                        turn_index,
                        event_index,
                        action: FileActionKind::Edit,
                        summary: action.summary.clone(),
                        text: Arc::new(derived_text),
                        diff_stats,
                        hunks: if !action.hunks.is_empty() {
                            action.hunks.clone()
                        } else {
                            hunks
                        },
                    };
                    history.push_revision(rev);
                } else {
                    // No prior revision in history! First time file is encountered in session.
                    // 1. Check if original_file snapshot was captured in transcript:
                    let base_opt = action.original_file.clone().or_else(|| {
                        // 2. Backward reconstruction from disk head state R_N via disk_resolver
                        self.disk_resolver.as_ref().and_then(|resolver| {
                            let disk_head = resolver(path)?;
                            if !action.hunks.is_empty() {
                                reconstruct_base_from_hunks(&disk_head, &action.hunks).ok()
                            } else if let (Some(ref old_s), Some(ref new_s)) = (&action.old_content, &action.new_content) {
                                apply_string_replace(&disk_head, new_s, old_s, false).ok()
                            } else {
                                Some(disk_head)
                            }
                        })
                    });

                    if let Some(base_text) = base_opt {
                        let r0 = FileRevision {
                            revision_index: 0,
                            turn_index,
                            event_index,
                            action: FileActionKind::Read,
                            summary: format!("Base {path}"),
                            text: Arc::new(base_text.clone()),
                            diff_stats: DiffStats::default(),
                            hunks: Vec::new(),
                        };
                        history.push_revision(r0);

                        let derived_text = if let (Some(ref old_s), Some(ref new_s)) =
                            (&action.old_content, &action.new_content)
                        {
                            apply_string_replace(&base_text, old_s, new_s, false)
                                .unwrap_or_else(|_| base_text.clone())
                        } else if !action.hunks.is_empty() {
                            apply_hunks(&base_text, &action.hunks)
                                .unwrap_or_else(|_| base_text.clone())
                        } else {
                            base_text.clone()
                        };

                        let (diff_stats, hunks) = compute_line_diff(&base_text, &derived_text);
                        let r1 = FileRevision {
                            revision_index: 1,
                            turn_index,
                            event_index,
                            action: FileActionKind::Edit,
                            summary: action.summary.clone(),
                            text: Arc::new(derived_text),
                            diff_stats,
                            hunks: if !action.hunks.is_empty() {
                                action.hunks.clone()
                            } else {
                                hunks
                            },
                        };
                        history.push_revision(r1);
                    } else if let Some(ref snippet) = action.old_content {
                        // Minimal snippet fallback only if neither originalFile nor disk was found
                        let r0 = FileRevision {
                            revision_index: 0,
                            turn_index,
                            event_index,
                            action: FileActionKind::Read,
                            summary: format!("Base (Snippet) {path}"),
                            text: Arc::new(snippet.clone()),
                            diff_stats: DiffStats::default(),
                            hunks: Vec::new(),
                        };
                        history.push_revision(r0);

                        let derived_text = action.new_content.clone().unwrap_or_else(|| snippet.clone());
                        let (diff_stats, hunks) = compute_line_diff(snippet, &derived_text);
                        let r1 = FileRevision {
                            revision_index: 1,
                            turn_index,
                            event_index,
                            action: FileActionKind::Edit,
                            summary: action.summary.clone(),
                            text: Arc::new(derived_text),
                            diff_stats,
                            hunks: if !action.hunks.is_empty() {
                                action.hunks.clone()
                            } else {
                                hunks
                            },
                        };
                        history.push_revision(r1);
                    }
                }
            }
            FileActionKind::Read => {
                let text = self
                    .disk_resolver
                    .as_ref()
                    .and_then(|resolver| resolver(path))
                    .or_else(|| {
                        action
                            .original_file
                            .as_ref()
                            .or(action.new_content.as_ref())
                            .or(action.old_content.as_ref())
                            .cloned()
                    });

                if let Some(content) = text {
                    if history.is_empty() {
                        let rev = FileRevision {
                            revision_index: 0,
                            turn_index,
                            event_index,
                            action: FileActionKind::Read,
                            summary: action.summary.clone(),
                            text: Arc::new(content),
                            diff_stats: DiffStats::default(),
                            hunks: Vec::new(),
                        };
                        history.push_revision(rev);
                    } else if history.latest().is_none_or(|lat| lat.text.as_str() != content) {
                        let rev = FileRevision {
                            revision_index: history.count(),
                            turn_index,
                            event_index,
                            action: FileActionKind::Read,
                            summary: action.summary.clone(),
                            text: Arc::new(content),
                            diff_stats: DiffStats::default(),
                            hunks: Vec::new(),
                        };
                        history.push_revision(rev);
                    }
                }
            }
            _ => {}
        }
    }
}


