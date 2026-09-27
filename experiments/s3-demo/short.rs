//! The fold demo's non-paginated target: under one page (128 rows), so the
//! renderer-side compaction applies. The tall files in this demo SKIP folds
//! with a loud log — see the pagination boundary in repo.rs's fold block.

/// Adds two numbers the slow way, to have a body worth folding.
pub fn add_slow(a: u32, b: u32) -> u32 {
    let mut acc = a;
    for _ in 0..b {
        acc += 1;
    }
    acc
}

/// Wraps a label in brackets, trimming as it goes.
pub fn bracket(label: &str) -> String {
    let trimmed = label.trim();
    let mut out = String::with_capacity(trimmed.len() + 2);
    out.push('[');
    out.push_str(trimmed);
    out.push(']');
    out
}

/// Counts folds in a list — the demo measuring itself.
pub fn count_folds(items: &[u32]) -> u32 {
    let mut n = 0;
    for it in items {
        if *it % 2 == 0 {
            n += 1;
        }
    }
    n
}

/// A tiny main so this file has an entry point shape.
fn main() {
    let total = add_slow(2, 3);
    let label = bracket("fold demo");
    println!("{label}: {total} from {} folds", count_folds(&[2, 3, 4]));
}
