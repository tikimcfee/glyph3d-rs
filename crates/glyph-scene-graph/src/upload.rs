//! How a host table's changed rows reach the GPU: nothing, a few coalesced
//! `write_buffer` runs, or the whole table.
//!
//! `Queue::write_buffer` stages each call in its own allocation and copy
//! command, so N scattered rows written as N calls cost N of each. Rows join
//! a run across gaps of up to [`RUN_GAP`] clean rows; past [`MAX_RUNS`] runs
//! the smallest gaps are filled until that many remain (the host holds every
//! row's truth, so re-sending a clean row is harmless); and past
//! [`SPARSE_FRACTION`] of the table the whole table goes up in one write.
//! 15 % is bevy's figure for its sparse uploads (`AtomicSparseBufferVec`,
//! `bevy_render/src/render_resource/sparse_buffer_vec.rs`, bevy 0.16+, MIT
//! OR Apache-2.0: "obtained experimentally by testing very large scenes and
//! roughly matches the values used by other engines"); the run limit and gap
//! are ours. Until 2026-10-26 a batch past the run limit went up as one
//! staging upload and a compute pass scattered it; the renderer now runs no
//! compute pass for its tables, so the gaps are filled instead.

/// At most this many `write_buffer` calls per table and frame.
pub const MAX_RUNS: usize = 16;
/// Clean rows a run may swallow to join its neighbour.
pub const RUN_GAP: u32 = 2;
/// Above this fraction of the table, upload all of it.
pub const SPARSE_FRACTION: f64 = 0.15;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadPlan {
    None,
    /// `(first row, rows)` per `write_buffer`, ascending, disjoint.
    Runs(Vec<(u32, u32)>),
    /// Rows `0..len` in one `write_buffer`.
    Full,
}

impl UploadPlan {
    /// The rows this plan writes in a table of `len` rows.
    pub fn rows(&self, len: u32) -> u64 {
        match self {
            UploadPlan::None => 0,
            UploadPlan::Runs(runs) => runs.iter().map(|r| u64::from(r.1)).sum(),
            UploadPlan::Full => u64::from(len),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            UploadPlan::None => "none",
            UploadPlan::Runs(_) => "runs",
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
            _ => runs.push((r, 1)),
        }
    }
    UploadPlan::Runs(fill_gaps(runs, MAX_RUNS))
}

/// Fill the smallest gaps between `runs` until at most `max` remain.
fn fill_gaps(runs: Vec<(u32, u32)>, max: usize) -> Vec<(u32, u32)> {
    if runs.len() <= max {
        return runs;
    }
    let mut gaps: Vec<(u32, usize)> = runs.windows(2).enumerate().map(|(k, w)| (w[1].0 - (w[0].0 + w[0].1), k)).collect();
    gaps.sort_unstable();
    let mut fill = vec![false; runs.len()];
    for &(_, k) in gaps.iter().take(runs.len() - max) {
        fill[k] = true; // merge run k with k + 1
    }
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(max);
    let mut start = runs[0].0;
    for (k, r) in runs.iter().enumerate() {
        if !fill[k] {
            out.push((start, r.0 + r.1 - start));
            if let Some(next) = runs.get(k + 1) {
                start = next.0;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn few_rows_are_runs_many_runs_fill_their_smallest_gaps_and_a_large_share_goes_whole() {
        assert_eq!(plan_upload(&[], 100), UploadPlan::None);
        // Adjacent rows and gaps up to RUN_GAP join one run.
        assert_eq!(plan_upload(&[5], 1000), UploadPlan::Runs(vec![(5, 1)]));
        assert_eq!(plan_upload(&[5, 6, 7, 10, 20], 1000), UploadPlan::Runs(vec![(5, 6), (20, 1)]));
        // MAX_RUNS runs stay apart; one more fills the smallest gap.
        let spaced: Vec<u32> = (0..MAX_RUNS as u32).map(|k| k * 100).collect();
        assert!(matches!(plan_upload(&spaced, 100_000), UploadPlan::Runs(r) if r.len() == MAX_RUNS));
        let mut spaced: Vec<u32> = (0..=MAX_RUNS as u32).map(|k| k * 100).collect();
        spaced[3] = 205;
        let UploadPlan::Runs(runs) = plan_upload(&spaced, 100_000) else { panic!("runs") };
        assert_eq!(runs.len(), MAX_RUNS);
        assert_eq!(runs[2], (200, 6), "rows 200 and 205 share the smallest gap");
        // Past 15 % of the table: the whole table, even if the rows are adjacent.
        let many: Vec<u32> = (0..151).collect();
        assert_eq!(plan_upload(&many, 1000), UploadPlan::Full);
        let at: Vec<u32> = (0..150).collect();
        assert_eq!(plan_upload(&at, 1000), UploadPlan::Runs(vec![(0, 150)]));
        assert_eq!(UploadPlan::Runs(vec![(1, 3), (9, 2)]).rows(100), 5);
        assert_eq!(UploadPlan::Full.rows(100), 100);
    }
}
