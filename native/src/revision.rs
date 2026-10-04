//! $R(n)$ multi-edit file revision engine and patch algebra.
//!
//! Provides snapshot history tracking across agent turns:
//! - [`engine`]: [`RevisionEngine`] for multi-file revision management.
//! - [`types`]: [`FileRevisionHistory`], [`FileRevision`], and [`DiffStats`].
//! - [`patch`]: Pure forward and reverse patch/delta algebra.

pub mod engine;
pub mod patch;
pub mod types;

#[cfg(test)]
mod tests;

pub use engine::{DiskResolver, RevisionEngine};
pub use patch::{apply_hunks, apply_string_replace, compute_line_diff, reconstruct_base_from_hunks, reverse_hunks, reverse_string_replace, PatchError};
pub use types::{DiffStats, FileRevision, FileRevisionHistory};
