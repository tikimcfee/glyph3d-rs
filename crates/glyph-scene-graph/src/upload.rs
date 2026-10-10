//! How a table's dirty rows reach the GPU: nothing, a few coalesced
//! `write_buffer` runs, a scatter pass, or the whole table.
//!
//! `Queue::write_buffer` stages each call in its own allocation and copy
//! command, so N scattered rows written as N calls cost N of each. A few runs
//! are cheapest as runs; past [`MAX_RUNS`] the rows go up in ONE staging
//! upload (rows + destination indices) and a compute pass scatters them —
//! bevy's `AtomicSparseBufferVec` pattern (`sparse_buffer_vec.rs`, bevy 0.16+,
//! MIT OR Apache-2.0); and past [`SPARSE_FRACTION`] of the table the scatter's
//! index overhead and dispatch stop paying, and the whole table goes up in one
//! write. 15 % is bevy's figure ("obtained experimentally by testing very
//! large scenes and roughly matches the values used by other engines"); the
//! run limit and gap are ours, unmeasured beyond `resolve-bench`.

/// Above this many runs, scatter instead.
pub const MAX_RUNS: usize = 16;
/// Clean rows a run may swallow to join its neighbour (the CPU holds every
/// row's truth, so re-sending a clean row is harmless).
pub const RUN_GAP: u32 = 2;
/// Above this fraction of the table, upload all of it.
pub const SPARSE_FRACTION: f64 = 0.15;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadPlan {
    None,
    /// `(first row, rows)` per `write_buffer`.
    Runs(Vec<(u32, u32)>),
    /// One staging upload of these rows (ascending), scattered on the GPU.
    Scatter(Vec<u32>),
    /// Rows `0..len` in one `write_buffer`.
    Full,
}

impl UploadPlan {
    /// Bytes this plan puts on the queue for rows of `row_bytes` in a table
    /// of `len` rows (the scatter counts its index words too).
    pub fn bytes(&self, row_bytes: u64, len: u32) -> u64 {
        match self {
            UploadPlan::None => 0,
            UploadPlan::Runs(runs) => runs.iter().map(|r| u64::from(r.1) * row_bytes).sum(),
            UploadPlan::Scatter(rows) => rows.len() as u64 * (row_bytes + 4),
            UploadPlan::Full => u64::from(len) * row_bytes,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            UploadPlan::None => "none",
            UploadPlan::Runs(_) => "runs",
            UploadPlan::Scatter(_) => "scatter",
            UploadPlan::Full => "full",
        }
    }
}

/// Choose how `rows` (ascending, unique) of a `len`-row table go up.
pub fn plan_upload(rows: &[u32], len: u32) -> UploadPlan {
    if rows.is_empty() || len == 0 {
        return UploadPlan::None;
    }
    if rows.len() as f64 > SPARSE_FRACTION * f64::from(len) {
        return UploadPlan::Full;
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &r in rows {
        match runs.last_mut() {
            Some(run) if r <= run.0 + run.1 + RUN_GAP => run.1 = r - run.0 + 1,
            _ => {
                if runs.len() == MAX_RUNS {
                    return UploadPlan::Scatter(rows.to_vec());
                }
                runs.push((r, 1));
            }
        }
    }
    UploadPlan::Runs(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn few_rows_are_runs_scattered_rows_scatter_and_a_large_share_goes_whole() {
        assert_eq!(plan_upload(&[], 100), UploadPlan::None);
        // Adjacent rows and gaps up to RUN_GAP join one run.
        assert_eq!(plan_upload(&[5], 1000), UploadPlan::Runs(vec![(5, 1)]));
        assert_eq!(plan_upload(&[5, 6, 7, 10, 20], 1000), UploadPlan::Runs(vec![(5, 6), (20, 1)]));
        // MAX_RUNS runs still go as runs; one more scatters.
        let spaced: Vec<u32> = (0..MAX_RUNS as u32).map(|k| k * 100).collect();
        assert!(matches!(plan_upload(&spaced, 100_000), UploadPlan::Runs(r) if r.len() == MAX_RUNS));
        let spaced: Vec<u32> = (0..=MAX_RUNS as u32).map(|k| k * 100).collect();
        assert_eq!(plan_upload(&spaced, 100_000), UploadPlan::Scatter(spaced.clone()));
        // Past 15 % of the table: the whole table, even if the rows are adjacent.
        let many: Vec<u32> = (0..151).collect();
        assert_eq!(plan_upload(&many, 1000), UploadPlan::Full);
        let at: Vec<u32> = (0..150).collect();
        assert_eq!(plan_upload(&at, 1000), UploadPlan::Runs(vec![(0, 150)]));
        assert_eq!(UploadPlan::Scatter(vec![1, 9]).bytes(32, 100), 72);
        assert_eq!(UploadPlan::Full.bytes(32, 100), 3200);
    }
}
