//! The view registry's witness (step A of
//! `out/DESIGN-VIEWS-SLABS-POSES-2026-10-11.md`, 2026-10-11): a view's group
//! row is a field the registry carries, never its index.
//!
//! Until step A one integer was a file's item row, its group row, its node,
//! its cull segment, its pick record and its Visible item, and code relied
//! on the equality. `GLYPH_PERMUTE_GROUP_ROWS=1` makes the repo loader file
//! `g-pick-repo`'s files under their group rows in REVERSE, with row 0 a
//! spare no file draws with (`repo::group_rows_of_views`): every view's
//! group row differs from its index. Rendered through the release binary
//! exactly as the golden runner drives it, each frame below must be
//! byte-equal to the same frame of the plain load, in every field:
//!
//! - the `repo-wide` frame (the load: slot group lanes, the Derived item
//!   table, the Visible items, the cull segments and their tints);
//! - the `repo-highlight` frame (the sidecar's placement writes: the Derived
//!   item lane, Instanced's group lane, the Visible item's spans);
//! - a scripted verb chain from the whole-field camera — `hide-group` on
//!   long.md (the cull's hidden flag, `set_item_hidden`), `recolor-line` and
//!   `set-glyph-background` on alpha.rs (placement writes, a group override
//!   keyed by item), `tint-group` and `move-group` on sub/deep.py (the node,
//!   the segment it syncs);
//! - the library layout (`--layout-mode library`: each file's node goes
//!   under its page's mount, the tree cull's segment nodes and tints).
//!
//! Before the registry the permuted load drew wrong in 5 of the 11
//! (case, field) pairs — repo-highlight under Derived, the verb chain under
//! Derived and Instanced, the library in both fields; the static load was
//! already right, its slots' lanes being the loader's (measured on the
//! parent commit; the step-A section of the design note has the figures). The `view-registry-node-is-the-index`
//! mutation hands back the view's index as its group row and reddens this
//! on 10 of its 11 pairs (by hand: under `cargo glyph prove` the registry's
//! unit test fails first, and the failing lib binary stops `cargo test`).
//! Needs a GPU, like every other render test in this crate.

use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "fixtures/g-pick-repo";

fn native_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glyph-view-registry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// One offscreen frame of `g-pick-repo` with `args`, the group rows
/// permuted or not; the PNG and stdout.
fn render(out: &Path, args: &[&str], permuted: bool) -> (image::RgbaImage, String) {
    let bin = env!("CARGO_BIN_EXE_glyph3d-native");
    let output = Command::new(bin)
        .current_dir(native_dir())
        .args(["--load-repo", REPO, "--frames", "2"])
        .args(args)
        .arg("--screenshot")
        .arg(out)
        .env("GLYPH_PERMUTE_GROUP_ROWS", if permuted { "1" } else { "0" })
        .output()
        .expect("spawn glyph3d-native");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "glyph3d-native failed ({args:?}, permuted {permuted}):\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let img = image::open(out).expect("read the screenshot").to_rgba8();
    (img, stdout)
}

/// The pixels that differ between two frames, and the bounding box of them.
fn difference(a: &image::RgbaImage, b: &image::RgbaImage) -> (usize, Option<(u32, u32, u32, u32)>) {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut count = 0usize;
    let mut bbox: Option<(u32, u32, u32, u32)> = None;
    for (x, y, pa) in a.enumerate_pixels() {
        if pa != b.get_pixel(x, y) {
            count += 1;
            bbox = Some(match bbox {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }
    (count, bbox)
}

/// `pick: <file> view=<v> group=<g> ...` → (v, g), for each pick line.
fn picked_views_and_groups(stdout: &str) -> Vec<(String, u32, u32)> {
    let field = |line: &str, key: &str| -> Option<u32> {
        line.split_whitespace().find_map(|w| w.strip_prefix(key)).and_then(|v| v.parse().ok())
    };
    stdout
        .lines()
        .filter(|l| l.starts_with("pick: "))
        .filter_map(|l| {
            let file = l.split_whitespace().nth(1)?.to_string();
            Some((file, field(l, "view=")?, field(l, "group=")?))
        })
        .collect()
}

struct Case {
    name: &'static str,
    args: &'static [&'static str],
    fields: &'static [&'static str],
}

const ALL_FIELDS: &[&str] = &["derived", "instanced", "visible"];

const CASES: &[Case] = &[
    Case { name: "repo-wide", args: &["--color-mode", "syntax"], fields: ALL_FIELDS },
    Case {
        name: "repo-highlight",
        args: &["--focus-file", "alpha.rs", "--zoom", "1", "--color-mode", "syntax", "--highlight", "fixtures/highlight-alpha.tsv"],
        fields: ALL_FIELDS,
    },
    Case {
        name: "verbs",
        args: &[
            "--color-mode", "syntax",
            "--pick-file", "long.md", "--verb", "hide-group",
            "--pick-file", "alpha.rs", "--pick-row", "3", "--pick-col", "8",
            "--verb", "recolor-line e01010", "--verb", "set-glyph-background 1010e0",
            "--pick-file", "deep.py", "--verb", "tint-group ff3333", "--verb", "move-group 4 -3 0",
        ],
        fields: ALL_FIELDS,
    },
    Case { name: "library", args: &["--layout-mode", "library"], fields: &["derived", "visible"] },
];

/// One (case, field) pair rendered plain and permuted: the pixels that
/// differ, their bounding box, and the permuted run's stdout.
struct Outcome {
    name: String,
    differing: usize,
    bbox: Option<(u32, u32, u32, u32)>,
    permuted_log: String,
}

#[test]
fn a_permuted_group_table_renders_the_unpermuted_frames() {
    let dir = scratch();
    // Every (case, field) pair renders twice; run the pairs side by side.
    let jobs: Vec<(&Case, &str)> = CASES.iter().flat_map(|c| c.fields.iter().map(move |f| (c, *f))).collect();
    let results: Vec<Outcome> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|&(case, field)| {
                let dir = dir.clone();
                s.spawn(move || {
                    let mut args = case.args.to_vec();
                    args.extend(["--field-mode", field]);
                    let (plain, _) = render(&dir.join(format!("{}-{field}-plain.png", case.name)), &args, false);
                    let (permuted, log) = render(&dir.join(format!("{}-{field}-permuted.png", case.name)), &args, true);
                    let (differing, bbox) = difference(&plain, &permuted);
                    Outcome { name: format!("{} / {field}", case.name), differing, bbox, permuted_log: log }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("render thread")).collect()
    });
    let mut failures = Vec::new();
    for Outcome { name, differing, bbox, .. } in &results {
        println!("{name:28} {differing:7} px differ{}", bbox.map_or(String::new(), |b| format!(", bbox {b:?}")));
        if *differing != 0 {
            failures.push(format!("{name}: {differing} px differ in {bbox:?}"));
        }
    }
    assert!(failures.is_empty(), "the permuted load drew differently from the plain one:\n{}", failures.join("\n"));
    // Not vacuous: the verb chain moved pixels (same camera and paint as
    // repo-wide, so every difference is the verbs').
    for field in ALL_FIELDS {
        let open = |case: &str| image::open(dir.join(format!("{case}-{field}-plain.png"))).expect("read the frame").to_rgba8();
        let (count, bbox) = difference(&open("repo-wide"), &open("verbs"));
        println!("verbs vs repo-wide / {field}: {count} px{}", bbox.map_or(String::new(), |b| format!(", bbox {b:?}")));
        assert!(count > 1_000, "{field}: the verb chain changed only {count} px of the frame");
    }
    // The permutation landed: every pick the verb chain made names a view
    // whose group row is not its index (rows run n..1 over files 0..n-1).
    let log = &results.iter().find(|o| o.name == "verbs / derived").expect("the verb chain ran").permuted_log;
    let picks = picked_views_and_groups(log);
    assert_eq!(picks.len(), 3, "three picks in the verb chain: {log}");
    for (file, view, group) in &picks {
        println!("permuted pick: {file} view={view} group={group}");
        assert_ne!(view, group, "{file}: the permuted load filed view {view} under its own index");
    }
}
