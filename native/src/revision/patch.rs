//! Pure delta and patch algebra for file revision reconstruction.
//!
//! Provides forward application of agent edits (`old_string` -> `new_string`, hunks)
//! and reverse reconstruction (`reconstructBase`: $R_{k-1} \leftarrow \text{reversePatch}(R_k)$).

use crate::agent_transcript::DiffHunkRecord;
use super::types::DiffStats;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    TargetNotFound,
    AmbiguousTarget(usize),
    LineOutOfBounds { line: usize, total: usize },
    ContextMismatch { expected: String, actual: String },
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound => write!(f, "Target string to replace not found in source text"),
            Self::AmbiguousTarget(count) => {
                write!(f, "Target string matched {count} times with replace_all=false")
            }
            Self::LineOutOfBounds { line, total } => {
                write!(f, "Hunk line {line} exceeds source total lines {total}")
            }
            Self::ContextMismatch { expected, actual } => {
                write!(
                    f,
                    "Hunk context mismatch: expected {:?}, got {:?}",
                    expected, actual
                )
            }
        }
    }
}

impl std::error::Error for PatchError {}

/// Apply exact string search and replace forward on source text.
pub fn apply_string_replace(
    source: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Result<String, PatchError> {
    if old_str.is_empty() {
        if source.is_empty() {
            return Ok(new_str.to_string());
        }
        return Ok(source.to_string());
    }

    if !source.contains(old_str) {
        return Err(PatchError::TargetNotFound);
    }

    if replace_all {
        return Ok(source.replace(old_str, new_str));
    }

    let count = source.matches(old_str).count();
    if count > 1 {
        // If there are multiple occurrences but replace_all is false, replace first unique match
        // or signal ambiguous if desirable. In Claude Code, single replacement replaces the first match.
        let mut result = String::with_capacity(source.len() + new_str.len() - old_str.len());
        if let Some(pos) = source.find(old_str) {
            result.push_str(&source[..pos]);
            result.push_str(new_str);
            result.push_str(&source[pos + old_str.len()..]);
            return Ok(result);
        }
    }

    Ok(source.replacen(old_str, new_str, 1))
}

/// Reverse-apply exact string replacement backwards ($R_k \rightarrow R_{k-1}$).
pub fn reverse_string_replace(
    head_source: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Result<String, PatchError> {
    apply_string_replace(head_source, new_str, old_str, replace_all)
}

/// Invert unified diff hunks: `+` becomes `-`, `-` becomes `+`, positions swap.
pub fn reverse_hunks(hunks: &[DiffHunkRecord]) -> Vec<DiffHunkRecord> {
    let mut reversed = Vec::with_capacity(hunks.len());
    for h in hunks.iter().rev() {
        let mut new_lines = Vec::with_capacity(h.lines.len());
        for line in &h.lines {
            if let Some(stripped) = line.strip_prefix('+') {
                new_lines.push(format!("-{stripped}"));
            } else if let Some(stripped) = line.strip_prefix('-') {
                new_lines.push(format!("+{stripped}"));
            } else {
                new_lines.push(line.clone());
            }
        }
        reversed.push(DiffHunkRecord {
            old_start: h.new_start,
            old_lines: h.new_lines,
            new_start: h.old_start,
            new_lines: h.old_lines,
            lines: new_lines,
        });
    }
    reversed
}

/// Apply a slice of unified diff hunks to source lines.
pub fn apply_hunks(source: &str, hunks: &[DiffHunkRecord]) -> Result<String, PatchError> {
    if hunks.is_empty() {
        return Ok(source.to_string());
    }

    let source_lines: Vec<&str> = source.lines().collect();
    let mut out_lines: Vec<String> = Vec::new();
    let mut src_idx = 0;

    for hunk in hunks {
        // Hunk starts at old_start (1-based index)
        let target_idx = if hunk.old_start > 0 {
            hunk.old_start - 1
        } else {
            0
        };

        // Copy untouched lines prior to this hunk
        while src_idx < target_idx && src_idx < source_lines.len() {
            out_lines.push(source_lines[src_idx].to_string());
            src_idx += 1;
        }

        for line in &hunk.lines {
            if let Some(stripped) = line.strip_prefix(' ') {
                if src_idx >= source_lines.len() {
                    return Err(PatchError::LineOutOfBounds {
                        line: src_idx + 1,
                        total: source_lines.len(),
                    });
                }
                if source_lines[src_idx] != stripped {
                    return Err(PatchError::ContextMismatch {
                        expected: stripped.to_string(),
                        actual: source_lines[src_idx].to_string(),
                    });
                }
                out_lines.push(source_lines[src_idx].to_string());
                src_idx += 1;
            } else if let Some(stripped) = line.strip_prefix('-') {
                if src_idx >= source_lines.len() {
                    return Err(PatchError::LineOutOfBounds {
                        line: src_idx + 1,
                        total: source_lines.len(),
                    });
                }
                if source_lines[src_idx] != stripped {
                    return Err(PatchError::ContextMismatch {
                        expected: stripped.to_string(),
                        actual: source_lines[src_idx].to_string(),
                    });
                }
                // Skip deleted line
                src_idx += 1;
            } else if let Some(stripped) = line.strip_prefix('+') {
                out_lines.push(stripped.to_string());
            }
        }
    }

    // Append remainder of source lines
    while src_idx < source_lines.len() {
        out_lines.push(source_lines[src_idx].to_string());
        src_idx += 1;
    }

    let mut result = out_lines.join("\n");
    if source.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// Reverse-apply unified diff hunks against `head_text` to recover prior `base_text`.
pub fn reconstruct_base_from_hunks(
    head_text: &str,
    hunks: &[DiffHunkRecord],
) -> Result<String, PatchError> {
    let reversed = reverse_hunks(hunks);
    apply_hunks(head_text, &reversed)
}

/// Compute line-level diff between `base` and `head` text, returning DiffStats and hunks.
pub fn compute_line_diff(base: &str, head: &str) -> (DiffStats, Vec<DiffHunkRecord>) {
    let base_lines: Vec<&str> = base.lines().collect();
    let head_lines: Vec<&str> = head.lines().collect();

    // Standard LCS table for lines
    let n = base_lines.len();
    let m = head_lines.len();

    if n == 0 && m == 0 {
        return (DiffStats::default(), Vec::new());
    }

    if n == 0 {
        let hunks = vec![DiffHunkRecord {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: m,
            lines: head_lines.iter().map(|l| format!("+{l}")).collect(),
        }];
        return (DiffStats::new(m, 0), hunks);
    }

    if m == 0 {
        let hunks = vec![DiffHunkRecord {
            old_start: 1,
            old_lines: n,
            new_start: 0,
            new_lines: 0,
            lines: base_lines.iter().map(|l| format!("-{l}")).collect(),
        }];
        return (DiffStats::new(0, n), hunks);
    }

    // LCS table
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in 1..=n {
        for j in 1..=m {
            if base_lines[i - 1] == head_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to build diff ops
    enum DiffOp<'a> {
        Equal(&'a str),
        Insert(&'a str),
        Delete(&'a str),
    }

    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && base_lines[i - 1] == head_lines[j - 1] {
            ops.push(DiffOp::Equal(base_lines[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            ops.push(DiffOp::Insert(head_lines[j - 1]));
            j -= 1;
        } else if i > 0 && (j == 0 || dp[i][j - 1] < dp[i - 1][j]) {
            ops.push(DiffOp::Delete(base_lines[i - 1]));
            i -= 1;
        }
    }
    ops.reverse();

    let mut added = 0;
    let mut removed = 0;
    let mut hunk_lines = Vec::new();

    for op in &ops {
        match op {
            DiffOp::Equal(l) => hunk_lines.push(format!(" {l}")),
            DiffOp::Insert(l) => {
                added += 1;
                hunk_lines.push(format!("+{l}"));
            }
            DiffOp::Delete(l) => {
                removed += 1;
                hunk_lines.push(format!("-{l}"));
            }
        }
    }

    let stats = DiffStats::new(added, removed);
    let hunks = if stats.is_empty() {
        Vec::new()
    } else {
        vec![DiffHunkRecord {
            old_start: 1,
            old_lines: n,
            new_start: 1,
            new_lines: m,
            lines: hunk_lines,
        }]
    };

    (stats, hunks)
}
