//! Types for the $R(n)$ multi-edit file revision engine.
//!
//! Tracks the complete lifecycle of files across agent turns ($R_0, R_1, \dots, R_N$).

use std::sync::Arc;
use serde::{Deserialize, Serialize};
use crate::agent_transcript::DiffHunkRecord;
use crate::spatial_scene::workdesk::FileActionKind;

/// Added/removed line counts for a revision relative to its predecessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

impl DiffStats {
    pub fn new(added: usize, removed: usize) -> Self {
        Self { added, removed }
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// A specific snapshot and modification record of a file at revision $R_k$.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRevision {
    /// Revision index (0 = initial base $R_0$, 1 = edit 1, ..., N = edit N).
    pub revision_index: usize,
    /// Which agent turn produced or inspected this revision.
    pub turn_index: usize,
    /// Which atomic narrative beat / event produced or inspected this revision.
    #[serde(default)]
    pub event_index: Option<usize>,
    /// Kind of action that created or observed this revision.
    pub action: FileActionKind,
    /// Brief human-readable summary of the change.
    pub summary: String,
    /// Full contiguous text snapshot of the file at this revision.
    pub text: Arc<String>,
    /// Line changes (+added, -removed) compared to the prior revision.
    pub diff_stats: DiffStats,
    /// Diff hunks compared to the prior revision ($R_{k-1} \rightarrow R_k$).
    pub hunks: Vec<DiffHunkRecord>,
}

impl FileRevision {
    /// Total line count of this file snapshot.
    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }

    /// Total byte length of this file snapshot.
    pub fn byte_len(&self) -> usize {
        self.text.len()
    }
}

/// The complete revision history of a single file across all agent turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRevisionHistory {
    pub file_path: String,
    pub revisions: Vec<FileRevision>,
}

impl FileRevisionHistory {
    pub fn new(file_path: impl Into<String>) -> Self {
        Self {
            file_path: file_path.into(),
            revisions: Vec::new(),
        }
    }

    /// Total number of revisions recorded for this file ($N+1$).
    pub fn count(&self) -> usize {
        self.revisions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.revisions.is_empty()
    }

    /// Base snapshot $R_0$ if available.
    pub fn base(&self) -> Option<&FileRevision> {
        self.revisions.first()
    }

    /// Latest snapshot $R_N$.
    pub fn latest(&self) -> Option<&FileRevision> {
        self.revisions.last()
    }

    /// Get revision by its exact revision index $R_k$.
    pub fn get(&self, revision_index: usize) -> Option<&FileRevision> {
        self.revisions.get(revision_index)
    }

    /// Find the revision active at or immediately before a specific beat/event index.
    pub fn revision_for_event(&self, event_index: usize) -> Option<&FileRevision> {
        self.revisions
            .iter()
            .rev()
            .find(|r| r.event_index.is_some_and(|idx| idx <= event_index))
            .or_else(|| self.revisions.first())
    }

    /// Find the revision created or viewed during a specific agent turn index (latest in that turn).
    pub fn revision_for_turn(&self, turn_index: usize) -> Option<&FileRevision> {
        self.revisions.iter().rev().find(|r| r.turn_index == turn_index)
    }

    /// Push a new revision onto the history, returning its index.
    pub fn push_revision(&mut self, mut rev: FileRevision) -> usize {
        let index = self.revisions.len();
        rev.revision_index = index;
        self.revisions.push(rev);
        index
    }
}
