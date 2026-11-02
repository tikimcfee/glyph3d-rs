//! The gates: how each kind runs, and the order its `needs` enforce.

use crate::artifacts::verify_committed;
use crate::bounded::{run_within, sh_within};
use crate::golden::verify_golden;
use crate::manifest::{Class, Gate, Kind, Manifest};
use crate::paths::{expand, keep, resolve_gpu, root, sh, under_relax};
use crate::test_build::test_build;
use std::process::Command;

fn warn_count(out: &str) -> usize {
    out.lines().filter(|l| l.starts_with("warning") && !l.contains("generated")).count()
}

/// The command a `kind = "cargo"` gate's `cmd` names.
fn cargo_cmd(name: Option<&str>) -> Option<&'static str> {
    Some(match name {
        Some("build") => "cargo build --release",
        Some("clippy") => "cargo clippy --release --all-targets",
        // Doc links are the one rename hazard nothing else in this battery can
        // see: `[`Engine::records`]` kept pointing at a method that had been
        // renamed to `read_back`, through a full green run, because rustdoc is
        // never invoked. Found by an audit 2026-09-07, not by a check.
        // `--no-deps` keeps it to the workspace's own prose; `--workspace`
        // since 2026-10-11 (it documented the renderer alone, so links in
        // crates/ and glyph/ were never read). `--release`, like every other
        // cargo gate, so it never shares target/debug, and its lock, with an
        // editor's rust-analyzer, which checks there.
        Some("doc") => "cargo doc --workspace --release --no-deps",
        // The renderer without its default features, in two builds: the
        // window without the egui overlay, a supported build that nothing
        // compiled until 2026-10-09, when it was found broken (C24); and
        // since 2026-10-26 the bare library, no window at all, the build a
        // host that owns its window links. The bare build's tree must hold
        // no winit or egui, and since 2026-10-31 no wgpu (that host draws
        // the frames with its own renderer); and it must hold the renderer
        // crate: an empty tree proves nothing. Its own target dir, so the
        // changed feature sets never invalidate the main build.
        Some("check-no-ui") => concat!(
            "cargo check -p glyph3d-native --no-default-features --features windowed --all-targets --target-dir target/no-ui",
            " && cargo check -p glyph3d-native --no-default-features --all-targets --target-dir target/no-ui",
            " && cargo tree -p glyph3d-native --no-default-features -e no-dev --prefix none > target/no-ui/tree.txt",
            " && grep -q '^glyph3d-native ' target/no-ui/tree.txt",
            " && ! grep -E '^(winit|egui|wgpu)' target/no-ui/tree.txt",
        ),
        _ => return None,
    })
}

fn gate_cargo(g: &Gate) -> bool {
    let Some(cmd) = cargo_cmd(g.cmd.as_deref()) else {
        println!("FAIL  {} has unknown cargo cmd {:?}", g.name, g.cmd);
        return false;
    };
    let (good, out) = sh(cmd, &root());
    if !good {
        println!("{}", out.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        println!("      cargo's output kept: {}", keep(&g.name, &out));
        println!("FAIL  {} errored", g.name);
        return false;
    }
    let w = warn_count(&out);
    if w == 0 {
        println!("PASS  {} — 0 warnings", g.name);
        true
    } else {
        for l in out.lines().filter(|l| l.starts_with("warning")) {
            println!("      {l}");
        }
        println!("      cargo's output kept: {}", keep(&g.name, &out));
        println!("FAIL  {} — {w} warnings", g.name);
        false
    }
}

/// One test job: a harness binary the test build made, or the doctests.
struct TestJob {
    label: String,
    cmd: Command,
    /// The harness binary, asked for its test list when it times out (the
    /// doctests have none).
    list: Option<Command>,
}

/// One job's end: its label, success, output, seconds, and on a timeout
/// what was recorded of it (`bounded::Outcome`).
struct JobRun {
    label: String,
    ok: bool,
    out: String,
    secs: f64,
    timed_out: Option<String>,
    list: Option<Command>,
}

/// Every job at once, each in its own thread, each bounded by `limit`
/// (2026-11-01: a test binary that stopped making progress held the
/// battery until killed by hand); outputs back in job order. cargo test
/// runs its binaries one after another, so the eight GPU integration
/// binaries (1-4 s each) queued behind the renderer's unit tests: 29 s run
/// serially, 22 s at once (measured 2026-10-26, under sibling load).
/// `stacks`: on a timeout, record the stuck binary's threads and stacks
/// first (`bounded::run_within`).
fn run_test_jobs(jobs: Vec<TestJob>, limit: std::time::Duration, stacks: bool) -> Vec<JobRun> {
    std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|mut j| {
                s.spawn(move || {
                    // A panic's stack in its report: a bounded GPU wait that
                    // times out panics where it waited.
                    if std::env::var_os("RUST_BACKTRACE").is_none() {
                        j.cmd.env("RUST_BACKTRACE", "1");
                    }
                    let o = run_within(&mut j.cmd, limit, stacks);
                    JobRun { label: j.label, ok: o.ok, out: o.out, secs: o.secs, timed_out: o.timed_out, list: j.list }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a test job thread panicked")).collect()
    })
}

/// The tests a timed-out binary started and never finished: its list,
/// less every test its output reports an outcome for.
fn unfinished(list: Option<Command>, out: &str) -> Vec<String> {
    let Some(mut list) = list else { return Vec::new() };
    let Ok(o) = list.args(["--list", "--format", "terse"]).stdin(std::process::Stdio::null()).output() else {
        return Vec::new();
    };
    let reported: std::collections::BTreeSet<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("test ")?.split_once(" ... ").map(|(n, _)| n.trim_end_matches(" - should panic")))
        .collect();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .filter(|n| !reported.contains(n))
        .map(str::to_string)
        .collect()
}

/// The floor is a RATCHET, not an equality: tests are added constantly, so an
/// exact pin would redden on the most common good action in the repo. It exists
/// because the previous form counted "test result: ok" LINES and required two —
/// and there are exactly two test binaries, so it could not fail. A green run
/// above the floor prints the value to raise it to, so it cannot quietly decay.
///
/// The runner runs the binaries, not `cargo test` (2026-10-26): the same
/// binaries cargo test would run (its `--no-run` names them), each from its
/// package's directory with `CARGO_MANIFEST_DIR` set as cargo sets it, plus
/// `cargo test --doc` for the doctests, all at once. A failing binary no
/// longer stops the ones after it, and every binary must report a result.
/// Since 2026-11-01 each is bounded by `[settings] process_timeout_secs`.
fn gate_cargo_test(m: &Manifest) -> bool {
    if let Some(jobs) = under_relax() {
        return gate_cargo_test_under_relax(m, jobs);
    }
    let units = match test_build() {
        Ok(u) => u,
        Err(rendered) => {
            for l in rendered.lines().filter(|l| l.starts_with("error")).take(20) {
                println!("      {l}");
            }
            println!("      the build's output kept: {}", keep("cargo-test-build", &rendered));
            println!("FAIL  cargo test — the test build errored");
            return false;
        }
    };
    let mut jobs: Vec<TestJob> = units
        .iter()
        .filter(|u| u.harness)
        .filter_map(|u| {
            let exe = u.executable.as_ref()?;
            let at = |c: &mut Command| {
                c.current_dir(&u.package_dir).env("CARGO_MANIFEST_DIR", &u.package_dir);
            };
            let (mut cmd, mut list) = (Command::new(exe), Command::new(exe));
            at(&mut cmd);
            at(&mut list);
            Some(TestJob { label: u.label(), cmd, list: Some(list) })
        })
        .collect();
    let mut doc = Command::new("cargo");
    doc.args(["test", "--release", "--doc"]).current_dir(root());
    jobs.push(TestJob { label: "doctests".into(), cmd: doc, list: None });
    let (passed, report) = judge_jobs(m, jobs, true);
    print!("{report}");
    passed
}

/// Run `jobs`, each bounded by the manifest's `process_timeout_secs`, and
/// judge them as the gate does: every binary green and reporting, a timed
/// out one red by name with its evidence kept, then the floor. Returns the
/// verdict and the report to print.
fn judge_jobs(m: &Manifest, jobs: Vec<TestJob>, stacks: bool) -> (bool, String) {
    let limit = m.settings.process_timeout();
    let results = run_test_jobs(jobs, limit, stacks);
    let mut r = String::new();
    let mut good = true;
    let mut binaries = 0;
    let mut total: u32 = 0;
    let slowest = results.iter().map(|j| (j.secs, j.label.clone())).max_by(|a, b| a.0.total_cmp(&b.0));
    for j in results {
        let JobRun { label, ok, out, timed_out, list, .. } = j;
        let summaries: Vec<&str> = out.lines().filter(|l| l.contains("test result")).collect();
        for l in &summaries {
            r.push_str(&format!("{l}\n"));
        }
        binaries += summaries.iter().filter(|l| l.contains("test result: ok")).count();
        total += summaries
            .iter()
            .flat_map(|l| l.split_whitespace().collect::<Vec<_>>().windows(2).map(|w| (w[0], w[1])).collect::<Vec<_>>())
            .filter(|(_, w)| w.starts_with("passed"))
            .filter_map(|(n, _)| n.parse::<u32>().ok())
            .sum::<u32>();
        if let Some(record) = &timed_out {
            good = false;
            r.push_str(&format!("FAIL  {label} timed out after {} s\n", limit.as_secs()));
            let running = unfinished(list, &out);
            for t in &running {
                r.push_str(&format!("      still running: {t}\n"));
            }
            for l in out.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                r.push_str(&format!("      {l}\n"));
            }
            let mut evidence = out.clone();
            evidence.push_str(&format!("\n\n== {label}: timed out after {} s\n", limit.as_secs()));
            for t in &running {
                evidence.push_str(&format!("still running: {t}\n"));
            }
            evidence.push_str(&format!("\n== where it was, before the kill\n{record}"));
            r.push_str(&format!("        its output and stacks kept: {}\n", keep(&format!("cargo-test {label} timed out"), &evidence)));
            continue;
        }
        if !ok || summaries.is_empty() {
            good = false;
            // Name the failing tests: printing only the summary discards WHY it
            // reddened, and a mutation's `expect` needs the reason in the text.
            for l in out.lines().filter(|l| l.contains("FAILED") || l.starts_with("panicked")) {
                r.push_str(&format!("      {l}\n"));
            }
            if summaries.is_empty() {
                for l in out.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                    r.push_str(&format!("      {l}\n"));
                }
            }
            r.push_str(&format!("      ^ {label}{}\n", if summaries.is_empty() { " reported no result" } else { "" }));
            r.push_str(&format!("        its whole output kept: {}\n", keep(&format!("cargo-test {label}"), &out)));
        }
    }
    if !good {
        r.push_str("FAIL  cargo test\n");
        return (false, r);
    }
    let (passed, verdict) = floor_verdict(m, binaries, total);
    r.push_str(&verdict);
    // The figure the limit is chosen against, on every green run.
    if let Some((secs, label)) = slowest {
        r.push_str(&format!("      slowest: {label} {secs:.1} s, of its {} s limit\n", limit.as_secs()));
    }
    (passed, r)
}

/// cargo-test under `glyph test --relax`: every test target of the
/// workspace under `relax test`, `jobs` at once, judged by this gate's own
/// rule (`judge_tests`: every target green, then the floor). The same run
/// prove's suite route makes (`prove_relax::suite`). relax runs no doctests.
fn gate_cargo_test_under_relax(m: &Manifest, jobs: usize) -> bool {
    let (ok, out) = crate::prove_relax::suite(m, jobs);
    for l in out.lines().filter(|l| {
        l.contains("test result") || l.starts_with("PASS ") || l.starts_with("FAIL ") || l.starts_with("NOTE ") || l.contains("FAILED")
    }) {
        println!("{l}");
    }
    if !ok {
        println!("        its whole output kept: {}", keep("cargo-test under relax", &out));
    }
    ok
}

/// The floor's verdict on a green run: at least two binaries reporting, and
/// at least `test_floor` tests run.
fn floor_verdict(m: &Manifest, binaries: usize, total: u32) -> (bool, String) {
    let floor = m.settings.test_floor;
    if binaries < 2 {
        return (false, format!("FAIL  cargo test — only {binaries} test binary reported; one stopped running\n"));
    }
    if total < floor {
        return (
            false,
            format!(
                "FAIL  cargo test — {total} tests ran, floor is {floor}. Coverage DROPPED by {}.\n      \
                 A deleted test, an #[ignore] that outlived its reason, or a module\n      \
                 that stopped being compiled. Lower the floor only on purpose.\n",
                floor - total
            ),
        );
    }
    let mut s = format!("PASS  cargo test — {total} tests over {binaries} binaries (floor {floor})\n");
    if total > floor {
        s.push_str(&format!("NOTE  the floor is behind: raise test_floor to {total} in build.toml\n"));
    }
    (true, s)
}

/// This gate's verdict on the suite as relax ran it (prove's suite route):
/// every target green, then the floor, by the same rule as the gate's own
/// run. `good` is whether every target exited green.
pub(crate) fn judge_tests(m: &Manifest, good: bool, out: &str) -> (bool, String) {
    let mut s = String::new();
    for l in out.lines().filter(|l| l.contains("test result")) {
        s.push_str(&format!("{l}\n"));
    }
    if !good {
        for l in out.lines().filter(|l| l.contains("FAILED") || l.starts_with("panicked")) {
            s.push_str(&format!("      {l}\n"));
        }
        s.push_str("FAIL  cargo test\n");
        return (false, s);
    }
    let binaries = out.lines().filter(|l| l.contains("test result: ok")).count();
    let (passed, verdict) = floor_verdict(m, binaries, tests_ran(out));
    s.push_str(&verdict);
    (passed, s)
}

/// How many tests a run's libtest summaries say passed, summed.
pub(crate) fn tests_ran(out: &str) -> u32 {
    out.split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[1].starts_with("passed"))
        .filter_map(|w| w[0].parse::<u32>().ok())
        .sum()
}

/// The source files a dep-info file lists (make syntax: `target: dep dep`,
/// spaces in a path escaped `\ `), resolved against the workspace root,
/// where cargo runs rustc.
fn dep_info_sources(text: &str) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let Some((_, deps)) = line.split_once(": ").or_else(|| line.strip_suffix(':').map(|t| (t, ""))) else {
            continue;
        };
        let mut cur = String::new();
        let mut chars = deps.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => cur.push(chars.next().unwrap_or(' ')),
                ' ' => v.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        v.push(cur);
    }
    v.into_iter().filter(|p| !p.is_empty()).map(|p| root().join(p)).collect()
}

/// A `.rs` file under a package's `src/` that no unit of the test build
/// read: no `mod` reaches it, so rustc never compiled it and said nothing.
/// The reach is rustc's own (each unit's dep-info), over every unit cargo
/// test builds — lib, its cfg(test) harness, bins and theirs — so this needs
/// no second analysis of the workspace (cargo-modules, 24-48 s a run until
/// 2026-10-26) and covers the renderer's bin, which cargo-modules could not.
fn gate_orphans() -> bool {
    let units = match test_build() {
        Ok(u) => u,
        Err(_) => {
            println!("FAIL  orphans — the test build errored, so nothing says what was compiled");
            return false;
        }
    };
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut packages: std::collections::BTreeMap<std::path::PathBuf, (Vec<String>, std::collections::BTreeSet<std::path::PathBuf>)> =
        Default::default();
    let mut ok = true;
    for u in units.iter().filter(|u| u.package_dir.starts_with(root())) {
        let entry = packages.entry(u.package_dir.clone()).or_default();
        let Some(d) = u.dep_info() else {
            println!("FAIL  orphans — {} left no dep-info; what it compiled is unknown", u.label());
            ok = false;
            continue;
        };
        entry.0.push(u.label());
        let text = std::fs::read_to_string(&d).unwrap_or_default();
        entry.1.extend(dep_info_sources(&text).iter().map(|p| canon(p)));
    }
    if packages.is_empty() {
        println!("FAIL  orphans — the test build named no workspace unit; compared nothing");
        return false;
    }
    let mut files_total = 0;
    for (dir, (labels, reached)) in &packages {
        let rel = dir.strip_prefix(root()).unwrap_or(dir).to_string_lossy().into_owned();
        let files = expand(&format!("{rel}/src/**/*.rs"));
        files_total += files.len();
        let orphans: Vec<_> = files.iter().filter(|f| !reached.contains(&canon(f))).collect();
        if orphans.is_empty() {
            println!("PASS  orphans {rel} — {} files, every one read by its {} units", files.len(), labels.len());
        } else {
            ok = false;
            println!("FAIL  orphans {rel} — {} file(s) no unit compiled:", orphans.len());
            for f in orphans {
                println!("      {}", f.strip_prefix(root()).unwrap_or(f).display());
            }
        }
    }
    if ok {
        println!("ORPHANS OK — {} packages, {files_total} files, no unreachable module file", packages.len());
    }
    ok
}

/// A `kind = "cmd"` gate: its command, bounded by `process_timeout_secs`
/// (2026-11-01; pick-oracle, reference-port and hyper-oracle run the
/// renderer).
fn gate_cmd(g: &Gate, m: &Manifest) -> bool {
    let cmd = g.cmd.as_deref().unwrap_or_default();
    let limit = m.settings.process_timeout();
    let o = sh_within(cmd, &root(), limit);
    if let Some(record) = &o.timed_out {
        for l in o.out.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
            println!("      {l}");
        }
        let evidence = format!("{}\n\n== timed out after {} s; where it was, before the kill\n{record}", o.out, limit.as_secs());
        println!("      its output and stacks kept: {}", keep(&format!("{} timed out", g.name), &evidence));
        println!("FAIL  {} timed out after {} s", g.name, limit.as_secs());
        return false;
    }
    let (good, out) = (o.ok, o.out);
    let passed = match &g.pass_line {
        Some(p) => good && out.contains(p.as_str()),
        None => good,
    };
    if passed {
        // Surface the child's OWN result lines, not just its last one. The
        // reference-port script reports six halves with their volumes — 17
        // fixtures, 13568 trie entries, 1872012 lanes, 530 seed queries — and
        // those counts ARE the claim that the check is not vacuous. Collapsing
        // them to one line is the same loss a doc review caught earlier today.
        // A child whose summary is its declared pass_line (hyper-oracle's
        // "hyper-oracle PASS: 6 corpora, ... records") surfaces those lines
        // too: without them the summary was the LAST line, which for
        // hyper-oracle is a wgpu WARN log, timestamp and all (2026-10-26).
        let detail: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.starts_with("PASS ")
                    || l.starts_with("FAIL ")
                    || g.pass_line.as_deref().is_some_and(|p| l.starts_with(p))
            })
            .collect();
        if detail.is_empty() {
            println!("PASS  {} — {}", g.name, out.lines().last().unwrap_or("").trim());
        } else {
            for l in &detail {
                println!("  {l}");
            }
            println!("PASS  {}", g.name);
        }
    } else {
        // Surface the child's OWN result lines on failure too, not just a tail.
        // Truncating to the last N lines drops the line that names WHICH part
        // failed whenever the child is chatty afterwards — the same detail loss
        // as collapsing the success case to one line, and it made a mutation
        // report "red for an unstated reason" when the reason was printed.
        let detail: Vec<&str> =
            out.lines().filter(|l| l.starts_with("PASS ") || l.starts_with("FAIL ")).collect();
        for l in &detail {
            println!("  {l}");
        }
        // Plus the tail, always. A child's summary line does not necessarily
        // start with PASS/FAIL — check-pick-oracle.sh ends "PICK ORACLE CHECK:
        // FAILURES" — and filtering to result-shaped lines alone dropped the
        // one line that named the outcome.
        // Always a full tail. Shortening it when detail exists dropped
        // "conformance: 3 case(s) failed" — a marker that is neither PASS- nor
        // FAIL-shaped — and broke a mutation that had been passing.
        let tail = 12;
        for l in out.lines().rev().take(tail).collect::<Vec<_>>().into_iter().rev() {
            println!("      {l}");
        }
        println!("      output kept: {}", keep(&g.name, &out));
        println!("FAIL  {}", g.name);
    }
    passed
}

/// The prerequisite check, derived from the manifest instead of hand-written
/// per gate. The Python runner had this same guard copied into three separate
/// gate implementations, so a gate that declared `needs` and forgot its own
/// copy was unprotected while looking protected. Here `needs` does the work:
/// one place, and adding a gate cannot forget it.
fn needs_met(g: &Gate, m: &Manifest) -> bool {
    for n in &g.needs {
        let Some(a) = m.artifact.get(n) else { continue };
        // A golden's absence is a STATE the gate reports (no set for this
        // host's key, with the adoption commands), not a prerequisite the
        // runner can satisfy — there is no build path, by design. Let the
        // gate see it.
        if a.class == Class::Golden {
            continue;
        }
        // Under `glyph test --relax` the renderer is relax compiling the
        // tree (`GLYPH_RENDERER`), not the built product, which may be stale
        // or absent and is not read.
        if a.class == Class::Product && under_relax().is_some() {
            continue;
        }
        for o in &a.outputs {
            let Some(o) = resolve_gpu(o) else {
                println!(
                    "FAIL  {} needs '{n}', whose path is keyed by hardware ({o}) and the \
                     renderer is not built to say which — run `glyph build`.",
                    g.name
                );
                return false;
            };
            // outputs are glob PATTERNS (engine/fixtures/*.pipe.bin), so this
            // has to expand rather than stat.
            if expand(&o).is_empty() {
                println!("FAIL  {} needs '{n}', but nothing matches {o} — run `glyph build`.", g.name);
                return false;
            }
        }
    }
    true
}

/// `inner`: the run is `glyph test --inner`, for a gate with merge-tier
/// parts of its own (pixel-ab's merge-tier equivalents).
pub(crate) fn run_gate(g: &Gate, m: &Manifest, inner: bool) -> bool {
    if !needs_met(g, m) {
        return false;
    }
    match g.kind {
        Kind::VerifyCommitted => verify_committed(m),
        Kind::Cmd => gate_cmd(g, m),
        Kind::Cargo => gate_cargo(g),
        Kind::CargoTest => gate_cargo_test(m),
        Kind::Orphans => gate_orphans(),
        Kind::GoldenVerify => verify_golden(m, inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the zero-warning gates count, over each tool's own output (test
    /// diet step 2, 2026-10-26: this replaces four mutations that each paid
    /// a renderer rebuild to show a warning reddens its gate). One warning
    /// each from rustc, clippy and rustdoc, with the location lines and
    /// cargo's summary line that follow it, counts once; a clean build
    /// counts none; a warning from cargo itself counts, as every warning
    /// does against a zero-warning gate.
    #[test]
    fn warn_count_counts_each_tools_warnings_once() {
        let rustc = "warning: unused variable: `x`\n --> src/a.rs:1:5\n  |\n1 | let x = 1;\n  |     ^ help: prefix it\n\n\
                     warning: `crate` (lib) generated 1 warning\n    Finished `release` profile";
        let clippy = "warning: this `if` has identical blocks\n --> src/a.rs:3:9\n\
                      warning: `crate` (lib) generated 1 warning (run `cargo clippy --fix` to apply 1 suggestion)";
        let rustdoc = "warning: unresolved link to `Foo::bar`\n --> src/a.rs:2:7\n\
                       warning: `crate` (lib doc) generated 1 warning\n   Generated target/doc/crate/index.html";
        for (tool, out) in [("rustc", rustc), ("clippy", clippy), ("rustdoc", rustdoc)] {
            assert_eq!(warn_count(out), 1, "{tool}: {out}");
        }
        assert_eq!(warn_count("   Compiling crate v0.1.0\n    Finished `release` profile [optimized] target(s) in 1.0s"), 0);
        assert_eq!(warn_count("warning: unused manifest key: package.foo"), 1);
    }

    /// The doc gate documents every workspace crate, not the renderer alone:
    /// until 2026-10-11 a dead link in crates/ or glyph/ was read by nothing
    /// (the dead-doc-link-crate mutation measured it).
    #[test]
    fn the_doc_gate_documents_the_workspace() {
        let doc = cargo_cmd(Some("doc")).expect("the doc gate's command");
        assert!(doc.contains("--workspace"), "{doc}");
        assert!(cargo_cmd(Some("nonsense")).is_none());
    }

    /// The cargo-test gate's limit (2026-11-01): a binary past the
    /// manifest's `process_timeout_secs` is killed, red by name with the
    /// tests it never finished and the output it printed, kept; a binary
    /// beside it that finished is judged as before. Before this a binary
    /// that stopped making progress held the battery until killed by hand
    /// (353 s, monolith1).
    #[test]
    fn a_test_binary_past_its_limit_is_red_by_name_and_its_output_kept() {
        let mut m = crate::manifest::load().expect("build.toml loads");
        m.settings.process_timeout_secs = 1;
        let bash = |script: &str| {
            let mut c = Command::new("bash");
            c.arg("-c").arg(script);
            c
        };
        let hung = TestJob {
            label: "hung binary".into(),
            cmd: bash("echo 'test fast ... ok'; echo 'test refused - should panic ... ok'; sleep 30"),
            list: Some(bash("printf 'fast: test\\nrefused: test\\nslow: test\\n'")),
        };
        let done = TestJob {
            label: "done binary".into(),
            cmd: bash("echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'"),
            list: None,
        };
        let t = std::time::Instant::now();
        let (passed, report) = judge_jobs(&m, vec![hung, done], false);
        assert!(!passed, "{report}");
        assert!(t.elapsed().as_secs() < 15, "killed at its limit, not after its 30 s: {report}");
        assert!(report.contains("FAIL  hung binary timed out after 1 s\n"), "{report}");
        assert!(report.contains("      still running: slow\n") && report.matches("still running").count() == 1, "{report}");
        assert!(report.contains("test fast ... ok"), "its output until the kill: {report}");
        assert!(report.contains("test result: ok. 1 passed") && !report.contains("done binary"), "{report}");
        let kept = report
            .lines()
            .find_map(|l| l.trim().strip_prefix("its output and stacks kept: "))
            .unwrap_or_else(|| panic!("the evidence is kept: {report}"));
        let path = root().join(kept);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{kept}: {e}"));
        let _ = std::fs::remove_file(&path);
        assert!(text.contains("test fast ... ok") && text.contains("timed out after 1 s"), "{text}");
    }

    /// The limit is declared, and `validate` refuses one that would kill
    /// every process at once or that is no limit at all.
    #[test]
    fn the_process_limit_is_declared_and_validated() {
        let mut m = crate::manifest::load().expect("build.toml loads");
        assert!((1..=86_400).contains(&m.settings.process_timeout_secs));
        for bad in [0, 86_401] {
            m.settings.process_timeout_secs = bad;
            assert!(crate::manifest::validate(&m).iter().any(|p| p.contains("process_timeout_secs")), "{bad} is refused");
        }
    }

    /// Dep-info's make syntax: the target before `: `, escaped spaces kept
    /// inside a path, phony `path:` lines and `#` comments.
    #[test]
    fn dep_info_lists_every_source() {
        let text = "/t/deps/x-1.d: native/src/lib.rs native/src/a\\ b.rs /abs/c.rs\n\nnative/src/lib.rs:\n# env-dep:X=1\n";
        let got = dep_info_sources(text);
        let want: Vec<_> = ["native/src/lib.rs", "native/src/a b.rs", "/abs/c.rs"]
            .iter()
            .map(|p| root().join(p))
            .collect();
        assert_eq!(got, want);
    }
}
