//! prove's relax route: mutations proved under makepad's relax JIT, in
//! parallel, through its source overlay, so the working tree is never written
//! (out/PROVE-ON-RELAX-2026-10-26.md).
//!
//! A gate declares how its mutations are proven (`[[gate]] relax`), never a
//! guess at run time. Gates that declare nothing — the compiler-diagnostic
//! gates and the corpus gates — are proven on cargo (prove.rs). The routes:
//!   test      — cargo-test: the one `#[test]` fn the mutation's `expect`
//!               names, under `relax test <pkg> <target> -- <name>`; when it
//!               names none, the whole suite under relax, judged as the gate
//!               judges cargo's (`gates::judge_tests`: the floor)
//!   renderer  — a gate that runs the renderer: the gate itself, with
//!               `GLYPH_RENDERER` pointing at the renderer under `relax run`
//! A mutation of a file relax's compiler reads (Rust, or WGSL an
//! `include_str!` pulls in) is served through `MAKEPAD_RUN_OVERLAY`. A
//! renderer gate's mutation of a file the renderer reads at RUN time (a
//! fixture, the atlas) is applied on disk, after everything else and one at a
//! time, with its gate still under relax. A mutation of a relax gate that
//! neither can carry FAILS, naming why: it is never handed to cargo.
//!
//! relax is required for the mutations it proves: `GLYPH_RELAX` names the
//! binary (default: `relax` on PATH), a makepad checkout's
//! `cargo build --release -p relax-runner --bin relax`. Without one that
//! runs, prove stops before proving anything, and says so.
//!
//! Per route and per run, the guards prove's cargo route carries, plus one:
//!   two compilers agree — the clean tree runs natively AND under relax, and
//!                      the two must agree: both green, the same result lines.
//!                      A disagreement FAILS, naming the route and the diff;
//!                      it never falls back to cargo, which would launder a
//!                      relax defect into a green verdict
//!   assert applied   — the mutant text differs from the file's
//!   right test/text  — the route goes red with `expect` in its output (the
//!                      test route: that test fails)
//!   restore proven   — the overlay never writes the file, and it is
//!                      byte-compared with what it was; a disk mutation is
//!                      restored and byte-compared
//! The test route runs one test, so it proves that test catches the mutant,
//! which is what `expect` claims. A golden gate's mutant renders the view its
//! `expect` names (`GLYPH_GOLDEN_VIEWS`), for the same reason.
//!
//! Mutants run `jobs` at once, each in its own scratch directory: its
//! overlay, its golden renders (`GLYPH_SWEEP`) and the gates' scratch
//! (`GLYPH_SCRATCH`).
//! A route's mutants start the moment its own clean check agrees. relax's
//! code cache is content-keyed. A mutant that is one relax process reads the
//! shared clean cache read-only (`MAKEPAD_RUN_CACHE=readonly`), so the clean
//! cache never holds a mutant. A mutant that is many processes — a renderer
//! gate (pixel-ab renders frames, reference-port runs five instruments), a
//! test that spawns the renderer (`CARGO_BIN_EXE_*`), the suite — would
//! compile the mutant again in each, so it runs on its own copy-on-write
//! clone of the clean cache (`MAKEPAD_RUN_CACHE_DIR`, `clone_dir`),
//! deleted with the mutant. Only `cache/` is cloned: since makepad
//! a5299abd2 a redirected run dir keeps its build outputs at the
//! workspace's. Before then a clone's first process re-collected 334 crates
//! (20 s on the renderer); now it re-collects the 31 the shared cache does
//! (8.3 s against 6.0 s read-only on the shared one, then 1.2 s a process;
//! measured 2026-10-26).
//! The cargo route's Rust mutations run BESIDE the relax phase: relax reads
//! their files from untouched copies through the overlay (`Pristine`), so it
//! never sees a cargo mutant. The cargo route's other mutations write files
//! the renderer reads at run time, so they run after it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::gates::{judge_tests, tests_ran};
use crate::manifest::{Manifest, Mutation, Relax};
use crate::paths::{root, step};
use crate::products::ensure_products;
use crate::prove::{apply_mutation, find_matches, gate_output_with};

/// The relax binary: `GLYPH_RELAX`, or `relax` on PATH.
fn relax_bin() -> PathBuf {
    std::env::var_os("GLYPH_RELAX").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("relax"))
}

/// Whether the relax binary runs; Err says what is missing and how to get it.
fn relax_available() -> Result<(), String> {
    let bin = relax_bin();
    match Command::new(&bin).arg("--version").stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("{} --version exited {}", bin.display(), o.status)),
        Err(e) => Err(format!("{} does not run: {e}", bin.display())),
    }
}

/// relax's run directory for this workspace: its code cache, among the rest.
fn relax_run_dir() -> PathBuf {
    root().join("target/makepad-run")
}

/// One test, as both compilers run it: the package, the target selection,
/// the name; and the file that declares it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TestAt {
    package: String,
    target: Vec<String>,
    name: String,
    file: PathBuf,
}

/// What a routed mutation runs, which is also what its clean-tree check is
/// keyed by.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Route {
    Test(TestAt),
    /// every test target of the workspace, judged by the cargo-test gate's rule
    Suite,
    Renderer(String),
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Test(t) => write!(f, "{} {} {}", t.package, t.target.join(" "), t.name),
            Route::Suite => write!(f, "the whole suite under relax"),
            Route::Renderer(g) => write!(f, "gate {g}, renderer under relax"),
        }
    }
}

/// How a mutant reaches the route: served to relax's compiler, or written to
/// the file the renderer reads at run time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Apply {
    Overlay,
    Disk,
}

/// The workspace's packages, by name and directory: the root manifest's
/// `[workspace].members`, each expanded as cargo expands it. (Until
/// 2026-10-31 this globbed every `*/Cargo.toml`, which took the makepad
/// host, a crate the root `exclude`s, so the whole-suite route ran `relax
/// test glyph3d-makepad`, red as "not a member", on every tree.)
fn workspace_packages() -> Vec<(String, PathBuf)> {
    let root_manifest = std::fs::read_to_string(root().join("Cargo.toml")).expect("the root Cargo.toml reads");
    let members: Vec<String> = root_manifest
        .parse::<toml::Table>()
        .expect("the root Cargo.toml parses")
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("the root Cargo.toml declares [workspace].members")
        .iter()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect();
    members
        .iter()
        .flat_map(|m| glob::glob(&root().join(m).join("Cargo.toml").to_string_lossy()).into_iter().flatten().flatten())
        .filter_map(|manifest| {
            let text = std::fs::read_to_string(&manifest).ok()?;
            let name = text.parse::<toml::Table>().ok()?.get("package")?.get("name")?.as_str()?.to_string();
            Some((name, manifest.parent()?.to_path_buf()))
        })
        .collect()
}

/// The members whose build reads `file`: the member it lies in and every
/// member depending on that one by path, directly or through another
/// (2026-10-31): the members a suite run overlays the file for
/// (`member_overlays`).
fn packages_reading(file: &Path) -> std::collections::BTreeSet<String> {
    let packages = workspace_packages();
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let file = canon(file);
    let owner = packages.iter().filter(|(_, dir)| file.starts_with(canon(dir))).max_by_key(|(_, dir)| dir.components().count());
    let mut reading: std::collections::BTreeSet<String> = owner.map(|(n, _)| n.clone()).into_iter().collect();
    // Each member's path dependencies, by member name.
    let deps: Vec<(String, Vec<String>)> = packages
        .iter()
        .map(|(name, dir)| {
            let table = std::fs::read_to_string(dir.join("Cargo.toml")).ok().and_then(|t| t.parse::<toml::Table>().ok()).unwrap_or_default();
            let paths: Vec<String> = ["dependencies", "dev-dependencies", "build-dependencies"]
                .iter()
                .filter_map(|k| table.get(*k).and_then(|d| d.as_table()))
                .flat_map(|d| d.values())
                .filter_map(|v| v.get("path").and_then(|p| p.as_str()).map(|p| canon(&dir.join(p))))
                .filter_map(|dep_dir| packages.iter().find(|(_, d)| canon(d) == dep_dir).map(|(n, _)| n.clone()))
                .collect();
            (name.clone(), paths)
        })
        .collect();
    loop {
        let before = reading.len();
        for (name, on) in &deps {
            if on.iter().any(|d| reading.contains(d)) {
                reading.insert(name.clone());
            }
        }
        if reading.len() == before {
            return reading;
        }
    }
}

/// Per member of the suite's `targets`, its env with `MAKEPAD_RUN_OVERLAY`
/// narrowed to the overlay list's files that member reads
/// (`packages_reading`), or dropped when it reads none of them; a member
/// whose env would not change is absent (2026-10-31). relax refuses a run
/// whose first overlaid file was never read, so a list naming a renderer
/// file (a mutant, or a cargo mutation's shield) failed every crate the
/// renderer is built on.
fn member_overlays<'e>(targets: &[(String, Vec<String>)], env: &[(&'e str, OsString)]) -> BTreeMap<String, Vec<(&'e str, OsString)>> {
    let mut out = BTreeMap::new();
    let Some(list) = env.iter().find(|(k, _)| *k == "MAKEPAD_RUN_OVERLAY").map(|(_, v)| PathBuf::from(v)) else { return out };
    let Ok(text) = std::fs::read_to_string(&list) else { return out };
    let entries: Vec<(&str, std::collections::BTreeSet<String>)> = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(file, _)| (file, packages_reading(Path::new(file))))
        .collect();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains('\t')).collect();
    for package in targets.iter().map(|(p, _)| p).collect::<std::collections::BTreeSet<_>>() {
        let kept: Vec<&str> = lines.iter().zip(&entries).filter(|(_, (_, readers))| readers.contains(package)).map(|(l, _)| *l).collect();
        if kept.len() == lines.len() {
            continue;
        }
        let mut narrowed: Vec<(&'e str, OsString)> = env.iter().filter(|(k, _)| *k != "MAKEPAD_RUN_OVERLAY").cloned().collect();
        if !kept.is_empty() {
            let path = list.with_file_name(format!("overlay.{package}.tsv"));
            if std::fs::write(&path, kept.iter().map(|l| format!("{l}\n")).collect::<String>()).is_ok() {
                narrowed.push(("MAKEPAD_RUN_OVERLAY", path.into()));
            }
        }
        out.insert(package.clone(), narrowed);
    }
    out
}

/// Every test target of the workspace, as `relax test` selects it: a lib,
/// the bins, each `tests/*.rs` (cargo's own discovery; no package here
/// declares a `[[test]]` or turns discovery off).
fn workspace_test_targets() -> Vec<(String, Vec<String>)> {
    let mut v = Vec::new();
    for (package, dir) in workspace_packages() {
        if dir.join("src/lib.rs").exists() {
            v.push((package.clone(), vec!["--lib".to_string()]));
        }
        let declares_bin = std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|t| t.contains("[[bin]]"));
        if dir.join("src/main.rs").exists() || dir.join("src/bin").is_dir() || declares_bin {
            v.push((package.clone(), vec!["--bins".to_string()]));
        }
        for f in glob::glob(&dir.join("tests/*.rs").to_string_lossy()).into_iter().flatten().flatten() {
            let stem = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            v.push((package.clone(), vec!["--test".to_string(), stem]));
        }
    }
    v
}

/// The Rust files of the workspace's own crates.
fn workspace_sources() -> Vec<PathBuf> {
    ["native/src/**/*.rs", "crates/*/src/**/*.rs"]
        .iter()
        .flat_map(|p| glob::glob(&root().join(p).to_string_lossy()).into_iter().flatten().flatten())
        .collect()
}

/// `file`'s name, as a path's last component.
fn base_name(file: &str) -> String {
    Path::new(file).file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Whether some `include_str!` of the workspace pulls `file` in, so the
/// overlay reaches it.
fn included(file: &str) -> bool {
    let tail = format!("{}\")", base_name(file));
    workspace_sources().iter().any(|f| {
        std::fs::read_to_string(f)
            .is_ok_and(|t| t.lines().any(|l| l.contains("include_str!(") && l.contains(&tail)))
    })
}

/// The test `expect` names: Ok(None) when it names no test fn, Err when it
/// names several. Searched across the workspace, not the mutated file's
/// package: a crate's code is often held by a test of the renderer that uses
/// it (glyph-field-derived's, in native/tests/).
fn test_at(mu: &Mutation) -> Result<Option<TestAt>, String> {
    let decl = format!("fn {}(", mu.expect);
    let mut found: Vec<TestAt> = Vec::new();
    for (package, dir) in workspace_packages() {
        for sub in ["src", "tests"] {
            let pattern = dir.join(sub).join("**/*.rs");
            for f in glob::glob(&pattern.to_string_lossy()).into_iter().flatten().flatten() {
                if !std::fs::read_to_string(&f).is_ok_and(|t| t.contains(&decl)) {
                    continue;
                }
                let target = if sub == "tests" {
                    let stem = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    vec!["--test".to_string(), stem]
                } else if dir.join("src/lib.rs").exists() {
                    vec!["--lib".to_string()]
                } else {
                    vec!["--bins".to_string()]
                };
                found.push(TestAt { package: package.clone(), target, name: mu.expect.clone(), file: f });
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0))),
        n => Err(format!("expect {:?} names {n} test fns of the workspace", mu.expect)),
    }
}

/// The route a mutation takes: Ok(None) when its gate declares none (cargo
/// proves it), Err when its gate declares relax and relax cannot carry it.
fn route(m: &Manifest, mu: &Mutation) -> Result<Option<(Route, Apply)>, String> {
    let gate = m.gate.iter().find(|g| g.name == mu.gate).ok_or_else(|| format!("gate {} is not declared", mu.gate))?;
    let Some(relax) = gate.relax else {
        return Ok(None);
    };
    let wgsl = mu.file.ends_with(".wgsl");
    let compiled = mu.op == "replace" && (mu.file.ends_with(".rs") || (wgsl && included(&mu.file)));
    match relax {
        Relax::Test if !compiled => Err(format!(
            "a {} of {}, which relax's compiler does not read: a test sees only what the overlay serves",
            mu.op, mu.file
        )),
        // A test that read the shader with `std::fs` rather than through its
        // crate's `include_str!` would not see the mutant, stay green and fail
        // the run (curve_cap.rs did until 2026-10-26).
        Relax::Test => match test_at(mu)? {
            None => Ok(Some((Route::Suite, Apply::Overlay))),
            Some(t) => Ok(Some((Route::Test(t), Apply::Overlay))),
        },
        Relax::Renderer => {
            Ok(Some((Route::Renderer(mu.gate.clone()), if compiled { Apply::Overlay } else { Apply::Disk })))
        }
    }
}

/// A command's success and its stdout+stderr.
fn output(c: &mut Command) -> (bool, String) {
    match c.stdin(Stdio::null()).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("could not run {:?}: {e}", c.get_program())),
    }
}

/// The named tests as cargo builds them, in ONE run: release, at WORKSPACE
/// scope, the scope the cargo-build and cargo-test gates build at. A `-p`
/// build resolves the renderer's features alone and re-links
/// `target/release/glyph3d-native` (an integration test needs the bin) with
/// different bytes behind a stamp that still reads current: measured here, a
/// pixel-ab clean check went red mid-run. libtest takes the names as filters,
/// so every test binary runs and each keeps the tests it has.
fn cargo_tests(names: &[&str]) -> (bool, String) {
    let mut c = Command::new("cargo");
    c.args(["test", "--release", "--workspace", "--lib", "--bins", "--tests", "--"]).args(names);
    output(c.current_dir(root()))
}

/// What libtest said of the test `name`: (path, outcome) for every test
/// whose path is `name` or ends in `::name`.
fn test_lines(out: &str, name: &str) -> Vec<(String, String)> {
    let tail = format!("::{name}");
    let mut v: Vec<(String, String)> = out
        .lines()
        .filter_map(|l| l.strip_prefix("test ")?.split_once(" ... "))
        .filter(|(path, _)| *path == name || path.ends_with(&tail))
        .map(|(path, outcome)| (path.to_string(), outcome.trim().to_string()))
        .collect();
    v.sort();
    v
}

/// The renderer under relax, as one executable for `GLYPH_RENDERER`: the
/// gates spawn it from their own directories, and relax finds the workspace
/// above them.
fn relax_renderer() -> std::io::Result<PathBuf> {
    let path = root().join("target/prove-relax/glyph3d-native-relax");
    std::fs::create_dir_all(path.parent().unwrap_or(&root()))?;
    let relax = relax_bin().to_string_lossy().replace('\'', "'\\''");
    std::fs::write(&path, format!("#!/bin/sh\nexec '{relax}' run --quiet --allow-c glyph3d-native -- \"$@\"\n"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Whether a route's run is many relax processes.
fn spawns_relax(r: &Route) -> bool {
    match r {
        Route::Renderer(_) | Route::Suite => true,
        Route::Test(t) => std::fs::read_to_string(&t.file).is_ok_and(|s| s.contains("CARGO_BIN_EXE")),
    }
}

/// One `relax test` run.
fn relax_test(package: &str, target: &[String], filters: &[&str], env: &[(&str, OsString)]) -> (bool, String) {
    let mut c = Command::new(relax_bin());
    c.arg("--allow-c").arg("test").arg(package).args(target).arg("--").args(filters);
    c.current_dir(root()).envs(env.iter().map(|(k, v)| (k, v)));
    output(&mut c)
}

/// Runs a route under relax with `env` added. The suite's verdict is the
/// cargo-test gate's rule over every target's output, its report appended.
/// Each suite target gets the overlay list narrowed to the files its member
/// reads (`member_overlays`).
fn run(m: &Manifest, r: &Route, renderer: &Path, env: &[(&str, OsString)], jobs: usize) -> (bool, String) {
    match r {
        Route::Test(t) => relax_test(&t.package, &t.target, &[&t.name], env),
        Route::Suite => {
            let targets = workspace_test_targets();
            let per_member = member_overlays(&targets, env);
            let outs: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());
            let mut tasks: Vec<&(String, Vec<String>)> = targets.iter().collect();
            tasks.reverse();
            let tasks = Mutex::new(tasks);
            std::thread::scope(|s| {
                for _ in 0..jobs.clamp(1, targets.len().max(1)) {
                    s.spawn(|| loop {
                        let Some((package, target)) = tasks.lock().unwrap_or_else(|e| e.into_inner()).pop() else { break };
                        let env = per_member.get(package).map_or(env, Vec::as_slice);
                        let (ok, out) = relax_test(package, target, &[], env);
                        // A target that fails without a libtest FAILED line
                        // (relax refused it, or it died before reporting)
                        // would leave the judge's bare `FAIL  cargo test`
                        // unexplained: name it, with its last line.
                        let out = if ok || out.contains("FAILED") {
                            out
                        } else {
                            let last = out.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("(no output)").trim();
                            format!("{out}\n      FAILED  relax test {package} {}: {last}", target.join(" "))
                        };
                        outs.lock().unwrap_or_else(|e| e.into_inner()).push((ok, out));
                    });
                }
            });
            let outs = outs.into_inner().unwrap_or_else(|e| e.into_inner());
            let good = outs.iter().all(|(ok, _)| *ok);
            let out: String = outs.into_iter().map(|(_, o)| o).collect::<Vec<_>>().join("\n");
            // The judge repeats libtest's summary lines; only its verdict is
            // appended, so the run's tests are counted once.
            let (ok, judged) = judge_tests(m, good, &out);
            let verdict: String = judged.lines().filter(|l| !l.contains("test result")).map(|l| format!("{l}\n")).collect();
            (ok, format!("{out}\n{verdict}"))
        }
        Route::Renderer(gate) => {
            let mut all: Vec<(&str, &std::ffi::OsStr)> = vec![("GLYPH_RENDERER", renderer.as_os_str())];
            all.extend(env.iter().map(|(k, v)| (*k, v.as_os_str())));
            gate_output_with(gate, &all)
        }
    }
}

/// What two runs of one gate are compared by: its result lines, PASS/FAIL
/// with their volumes and the child's summaries the gate surfaces
/// ("hyper-oracle PASS: 6 corpora, ... records"); never a log line, whose
/// timestamp differs run to run.
fn gate_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| (l.contains("PASS") || l.contains("FAIL")) && !l.contains('\u{1b}'))
        .map(str::to_string)
        .collect()
}

/// A work queue `jobs` wide whose tasks may queue more: `f` runs a task and
/// returns the tasks it unblocks. Returns when the queue is empty and no
/// worker is busy.
fn queue<T: Send>(seed: Vec<T>, jobs: usize, f: impl Fn(T) -> Vec<T> + Sync) {
    let state = Mutex::new((std::collections::VecDeque::from(seed), 0usize));
    let wake = std::sync::Condvar::new();
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| loop {
                let task = {
                    let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(t) = g.0.pop_front() {
                            g.1 += 1;
                            break Some(t);
                        }
                        if g.1 == 0 {
                            break None;
                        }
                        g = wake.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                };
                let Some(task) = task else {
                    wake.notify_all();
                    break;
                };
                let more = f(task);
                let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
                g.1 -= 1;
                g.0.extend(more);
                drop(g);
                wake.notify_all();
            });
        }
    });
}

/// A finished job's report, printed whole so parallel jobs never interleave.
fn report(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// One run of the clean tree, native or relax: its success, its output and
/// its time. A batch serves several routes.
struct Ran {
    ok: bool,
    out: String,
    time: Duration,
}

/// Times one run.
fn timed(f: impl FnOnce() -> (bool, String)) -> Ran {
    let t = Instant::now();
    let (ok, out) = f();
    Ran { ok, out, time: t.elapsed() }
}

/// A route's clean tree, native against relax: (agreed, report). A test
/// route compares the named test's own lines, test by test (its batch's
/// exit says nothing about it alone); the suite, how many tests ran (cargo
/// also reports doc-test binaries, which relax does not run); a gate route
/// the gate's result lines.
fn clean_check(r: &Route, native: &Ran, relax: &Ran) -> (bool, String) {
    let (native_lines, relax_lines, native_ok, relax_ok, ran) = match r {
        Route::Test(t) => {
            let as_lines = |v: Vec<(String, String)>| v.into_iter().map(|(p, o)| format!("test {p} ... {o}")).collect::<Vec<_>>();
            let (n, x) = (test_lines(&native.out, &t.name), test_lines(&relax.out, &t.name));
            let all_ok = |v: &[(String, String)]| !v.is_empty() && v.iter().all(|(_, o)| o == "ok");
            let (n_ok, x_ok) = (all_ok(&n), all_ok(&x));
            (as_lines(n), as_lines(x.clone()), n_ok, x_ok, !x.is_empty())
        }
        Route::Suite => {
            let count = |out: &str| vec![format!("{} tests ran", tests_ran(out))];
            (count(&native.out), count(&relax.out), native.ok, relax.ok, tests_ran(&relax.out) > 0)
        }
        Route::Renderer(_) => {
            let (n, x) = (gate_lines(&native.out), gate_lines(&relax.out));
            let ran = !x.is_empty();
            (n, x, native.ok, relax.ok, ran)
        }
    };
    let good = native_ok && relax_ok && ran && native_lines == relax_lines;
    let mut s = format!(
        "{}  {r} — native {} ({:.1}s run), relax {} ({:.1}s run){}\n",
        if good { "PASS" } else { "FAIL" },
        if native_ok { "green" } else { "RED" },
        native.time.as_secs_f64(),
        if relax_ok { "green" } else { "RED" },
        relax.time.as_secs_f64(),
        if good { format!(", {} result lines identical", relax_lines.len()) } else { String::new() },
    );
    if !good {
        if native_ok && relax_ok && native_lines != relax_lines {
            s.push_str("      native and relax DISAGREE on the clean tree; nothing on this route is proven:\n");
        } else if !ran {
            s.push_str("      relax ran nothing on this route; nothing on it is proven:\n");
        }
        for l in native_lines.iter().filter(|l| !relax_lines.contains(l)) {
            s.push_str(&format!("      - native: {l}\n"));
        }
        for l in relax_lines.iter().filter(|l| !native_lines.contains(l)) {
            s.push_str(&format!("      + relax:  {l}\n"));
        }
        if !relax_ok || !native_ok {
            let out = if relax_ok { &native.out } else { &relax.out };
            for l in out.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                s.push_str(&format!("      {l}\n"));
            }
        }
    }
    (good, s)
}

/// The golden view an `expect` names (`"emoji.png diverges from baseline"`
/// → `emoji`), if it names one.
fn expect_view(expect: &str) -> Option<String> {
    let stem = expect.split_whitespace().find_map(|w| w.strip_suffix(".png"))?;
    (!stem.is_empty() && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| stem.to_string())
}

/// A copy of `src` at `dst`, file by file. `std::fs::copy` is
/// `copy_file_range` on Linux, which btrfs serves as a reflink (a renderer
/// mutant, clone included, ran in 12.5 s). Other relax runs write the
/// cache while it is cloned (a file goes to a temp name and is renamed), so
/// an entry that vanishes between the listing and the copy is skipped: `cp`
/// failed on exactly that, measured. What the clone holds is then a mix of
/// complete entries, each content-keyed.
fn clone_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = match entry.file_type() {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let r = if kind.is_dir() { clone_dir(&from, &to) } else { std::fs::copy(&from, &to).map(|_| ()) };
        match r {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            r => r?,
        }
    }
    Ok(())
}

/// A mutant's scratch directory and the environment that isolates it.
fn scratch_env(mu: &Mutation, r: &Route, dir: &Path) -> Result<Vec<(&'static str, OsString)>, String> {
    std::fs::create_dir_all(dir.join("scratch")).map_err(|e| format!("its scratch did not make: {e}"))?;
    let mut env: Vec<(&str, OsString)> = vec![("GLYPH_SCRATCH", dir.join("scratch").into())];
    if spawns_relax(r) {
        let cache = dir.join("run-dir");
        // Only the code cache: relax keeps a redirected run dir's build
        // outputs (build/, build-scripts/, native/) at the workspace's own
        // (makepad a5299abd2), so the clone shares them.
        clone_dir(&relax_run_dir().join("cache"), &cache.join("cache")).map_err(|e| format!("the relax cache did not clone: {e}"))?;
        env.push(("MAKEPAD_RUN_CACHE_DIR", cache.into()));
        env.push(("GLYPH_SWEEP", dir.join("sweep").into()));
        if let Some(view) = expect_view(&mu.expect) {
            env.push(("GLYPH_GOLDEN_VIEWS", view.into()));
        }
    } else {
        env.push(("MAKEPAD_RUN_CACHE", "readonly".into()));
    }
    Ok(env)
}

/// The verdict on a mutant's run: (proved, report lines).
fn verdict(mu: &Mutation, r: &Route, ok: bool, out: &str) -> (bool, String) {
    let reported = match r {
        Route::Test(_) => out.lines().any(|l| l.starts_with("test result")),
        Route::Suite | Route::Renderer(_) => !gate_lines(out).is_empty(),
    };
    if ok {
        let why = mu.why.as_deref().unwrap_or("");
        (false, format!("FAIL  {} — {r} stayed GREEN under the mutation.\n      It does not catch what it claims: {why}\n", mu.name))
    } else if !reported {
        // A relax that died before the test or gate reported is not a verdict.
        let got = out.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("(no output)");
        (false, format!("FAIL  {} — no result was reported; got: {}\n", mu.name, got.trim()))
    } else if !out.contains(&mu.expect) || (matches!(r, Route::Test(_)) && !out.contains("FAILED")) {
        let got = out
            .lines()
            .rfind(|l| l.starts_with("test result") || l.starts_with("FAIL"))
            .unwrap_or("(no output)");
        (false, format!(
            "FAIL  {} — went red, but for an unstated reason.\n      expected text containing: {:?}\n      got: {}\n{}",
            mu.name,
            mu.expect,
            got.trim(),
            // What failed, so a wrong reason can be read without a rerun.
            out.lines()
                .filter(|l| l.contains("FAILED") || l.trim_start().starts_with("panicked"))
                .take(6)
                .map(|l| format!("      {}\n", l.trim()))
                .collect::<String>()
        ))
    } else {
        (true, format!("PASS  {r} reddens under relax on {} — {:?}\n", mu.name, mu.expect))
    }
}

/// One overlay mutant: (proved, report, time under relax).
fn prove_one(m: &Manifest, mu: &Mutation, r: &Route, renderer: &Path, pristine: &Pristine, jobs: usize) -> (bool, String, Duration) {
    let mut s = format!("\n── mutation: {} → {r}\n", mu.name);
    let fail = |mut s: String, why: String| {
        s.push_str(&format!("FAIL  {} — {why}\n", mu.name));
        (false, s, Duration::ZERO)
    };
    let file = root().join(&mu.file);
    let Ok(before) = std::fs::read(&file) else {
        return fail(s, format!("{} does not read", mu.file));
    };
    let text = String::from_utf8_lossy(&before).into_owned();
    let find = mu.find.as_deref().unwrap_or_default();
    let n = find_matches(&text, find);
    if n != 1 {
        return fail(s, format!("find-text matches {n} times in {}; mutation cannot land", mu.file));
    }
    let mutant = text.replacen(find, mu.with_.as_deref().unwrap_or_default(), 1);
    if mutant == text {
        return fail(s, format!("mutation did not change {}", mu.file));
    }
    let dir = root().join("target/prove-relax").join(&mu.name);
    let _ = std::fs::remove_dir_all(&dir);
    let replacement = dir.join(file.file_name().unwrap_or_default());
    let list = dir.join("overlay.tsv");
    let t = Instant::now();
    let mut env = match scratch_env(mu, r, &dir) {
        Ok(e) => e,
        Err(e) => return fail(s, e),
    };
    let written = std::fs::write(&replacement, &mutant).and_then(|_| std::fs::write(&list, pristine.overlay_with(&file, &replacement)));
    if let Err(e) = written {
        return fail(s, format!("the overlay did not write: {e}"));
    }
    env.push(("MAKEPAD_RUN_OVERLAY", list.into()));
    let (ok, out) = run(m, r, renderer, &env, jobs);
    let took = t.elapsed();
    let _ = std::fs::remove_dir_all(dir.join("run-dir"));
    s.push_str(&format!("      time: {:.1}s\n", took.as_secs_f64()));
    let (mut good, v) = verdict(mu, r, ok, &out);
    s.push_str(&v);
    if std::fs::read(&file).unwrap_or_default() != before {
        s.push_str(&format!("FAIL  {} — {} CHANGED on disk; the overlay route must never write it\n", mu.name, mu.file));
        good = false;
    }
    (good, s, took)
}

/// One disk mutant (a file the renderer reads at run time), its gate still
/// under relax: applied, run, restored and proven restored.
fn prove_on_disk(m: &Manifest, mu: &Mutation, r: &Route, renderer: &Path, jobs: usize) -> (bool, String, Duration) {
    let mut s = format!("\n── mutation: {} → {r}, on disk\n", mu.name);
    let dir = root().join("target/prove-relax").join(&mu.name);
    let _ = std::fs::remove_dir_all(&dir);
    let env = match scratch_env(mu, r, &dir) {
        Ok(e) => e,
        Err(e) => return (false, format!("{s}FAIL  {} — {e}\n", mu.name), Duration::ZERO),
    };
    let before = match apply_mutation(mu) {
        Ok(b) => b,
        Err(e) => return (false, format!("{s}FAIL  {} — {e}\n", mu.name), Duration::ZERO),
    };
    let t = Instant::now();
    let (ok, out) = run(m, r, renderer, &env, jobs);
    let took = t.elapsed();
    let file = root().join(&mu.file);
    let _ = std::fs::write(&file, &before);
    let _ = std::fs::remove_dir_all(dir.join("run-dir"));
    s.push_str(&format!("      time: {:.1}s\n", took.as_secs_f64()));
    let (mut good, v) = verdict(mu, r, ok, &out);
    s.push_str(&v);
    if std::fs::read(&file).unwrap_or_default() != before {
        s.push_str(&format!("FAIL  {} — {} did NOT restore byte-exact\n", mu.name, mu.file));
        good = false;
    }
    (good, s, took)
}

/// The files the cargo route mutates on disk while relax runs beside it,
/// each served to relax from an untouched copy: relax reads sources through
/// its overlay, so it never sees a cargo mutant. Only a Rust file can be
/// shielded this way; a file the renderer reads at RUN time (the atlas, the
/// fixtures) reaches relax from the disk, so its mutations run after relax.
struct Pristine {
    files: Vec<(PathBuf, PathBuf)>,
    list: Option<PathBuf>,
}

impl Pristine {
    fn of(beside: &[&Mutation]) -> std::io::Result<Pristine> {
        let dir = root().join("target/prove-relax/pristine");
        let _ = std::fs::remove_dir_all(&dir);
        let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
        for mu in beside {
            let file = root().join(&mu.file);
            if files.iter().any(|(f, _)| *f == file) {
                continue;
            }
            let copy = dir.join(&mu.file);
            std::fs::create_dir_all(copy.parent().unwrap_or(&dir))?;
            std::fs::copy(&file, &copy)?;
            files.push((file, copy));
        }
        let list = if files.is_empty() {
            None
        } else {
            let list = dir.join("overlay.tsv");
            std::fs::write(&list, Pristine { files: files.clone(), list: None }.overlay_with(Path::new(""), Path::new("")))?;
            Some(list)
        };
        Ok(Pristine { files, list })
    }

    /// An overlay list: `file` served from `replacement` (none when `file` is
    /// empty), and every shielded file but that one from its copy.
    fn overlay_with(&self, file: &Path, replacement: &Path) -> String {
        let mut s = String::new();
        if !file.as_os_str().is_empty() {
            s.push_str(&format!("{}\t{}\n", file.display(), replacement.display()));
        }
        for (f, copy) in self.files.iter().filter(|(f, _)| f != file) {
            s.push_str(&format!("{}\t{}\n", f.display(), copy.display()));
        }
        s
    }
}

/// Proves `selected`, each mutation as its gate declares: under relax, in
/// parallel, the relax gates' (their disk mutants after everything else);
/// through `cargo` (prove's cargo route) the rest, its Rust mutations BESIDE
/// the relax phase and shielded from it (`Pristine`), the others after it.
/// True when every mutation reddened.
pub(crate) fn prove_selected(
    m: &Manifest,
    selected: &[&Mutation],
    jobs: usize,
    cargo: impl Fn(&[&Mutation]) -> bool + Sync,
) -> bool {
    let wall = Instant::now();
    let mut fail = false;
    let mut routed: Vec<(&Mutation, Route)> = Vec::new();
    let mut on_disk: Vec<(&Mutation, Route)> = Vec::new();
    let mut rest: Vec<&Mutation> = Vec::new();
    for mu in selected {
        match route(m, mu) {
            Ok(Some((r, Apply::Overlay))) => routed.push((mu, r)),
            Ok(Some((r, Apply::Disk))) => on_disk.push((mu, r)),
            Ok(None) => rest.push(mu),
            Err(why) => {
                println!("FAIL  {} — gate {} is proven under relax, which cannot carry this mutation: {why}", mu.name, mu.gate);
                println!("      rework the mutation or its gate's declaration; it is not handed to cargo.");
                fail = true;
            }
        }
    }
    let (beside, after): (Vec<&Mutation>, Vec<&Mutation>) = rest.iter().copied().partition(|mu| mu.file.ends_with(".rs"));
    let under_relax = routed.len() + on_disk.len();
    println!(
        "ROUTES    {} under relax ({} of them on disk), {} on cargo ({} beside relax, {} after it)",
        under_relax,
        on_disk.len(),
        rest.len(),
        beside.len(),
        after.len()
    );
    if under_relax == 0 {
        return cargo(&rest) && !fail;
    }
    if let Err(e) = relax_available() {
        println!("FATAL {under_relax} selected mutations are proven under makepad's relax JIT, and it is not available: {e}.");
        println!("      Set GLYPH_RELAX to a relax binary (in a makepad checkout:");
        println!("      `cargo build --release -p relax-runner --bin relax`, then target/release/relax). Nothing was proven.");
        return false;
    }
    let renderer = match relax_renderer() {
        Ok(p) => p,
        Err(e) => {
            println!("FATAL the relax renderer wrapper did not write: {e}");
            return false;
        }
    };
    let pristine = match Pristine::of(&beside) {
        Ok(p) => p,
        Err(e) => {
            println!("FATAL the shielded copies for the cargo route did not write: {e}");
            return false;
        }
    };
    if routed.iter().chain(&on_disk).any(|(_, r)| matches!(r, Route::Renderer(_))) {
        step("products (the native side of the renderer routes' clean-tree check)");
        if !ensure_products(m, false) {
            println!("FATAL the products do not build on the clean tree; nothing can be proven.");
            return false;
        }
    }

    // The two compilers agree on the clean tree, once per route.
    let mut routes: Vec<Route> = routed.iter().chain(&on_disk).map(|(_, r)| r.clone()).collect();
    routes.sort();
    routes.dedup();
    step(&format!(
        "clean tree, natively and under relax ({}), once per route: {} routes, {jobs} at a time",
        relax_bin().display(),
        routes.len()
    ));
    let t_checks = Instant::now();
    // The native side first and one run at a time: the named tests in one
    // cargo run, the suite as the cargo-test gate, then each gate, so a
    // native gate never runs while a cargo build re-links what it runs. The
    // relax side then runs in parallel, batched as its runs are: one per test
    // target, one per gate, the suite.
    let names: Vec<&str> = routes
        .iter()
        .filter_map(|r| if let Route::Test(t) = r { Some(t.name.as_str()) } else { None })
        .collect();
    let native_tests = (!names.is_empty()).then(|| timed(|| cargo_tests(&names)));
    let native_suite = routes.contains(&Route::Suite).then(|| timed(|| gate_output_with("cargo-test", &[])));
    // A golden gate's clean check renders the views its mutants judge, the
    // union of what their expects name, on both sides; all of them when any
    // names none.
    let gate_views = |g: &str| -> Vec<(&'static str, OsString)> {
        let views: Option<Vec<String>> = routed
            .iter()
            .chain(&on_disk)
            .filter(|(_, r)| matches!(r, Route::Renderer(x) if x == g))
            .map(|(mu, _)| expect_view(&mu.expect))
            .collect();
        match views {
            Some(mut v) if !v.is_empty() => {
                v.sort();
                v.dedup();
                vec![("GLYPH_GOLDEN_VIEWS", v.join(",").into())]
            }
            _ => Vec::new(),
        }
    };
    let native_gates: BTreeMap<&str, Ran> = routes
        .iter()
        .filter_map(|r| if let Route::Renderer(g) = r { Some(g.as_str()) } else { None })
        .map(|g| {
            let env = gate_views(g);
            let env: Vec<(&str, &std::ffi::OsStr)> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
            (g, timed(|| gate_output_with(g, &env)))
        })
        .collect();
    let mut batches: BTreeMap<(String, Vec<String>), Vec<&str>> = BTreeMap::new();
    for r in &routes {
        if let Route::Test(t) = r {
            batches.entry((t.package.clone(), t.target.clone())).or_default().push(&t.name);
        }
    }
    let pristine_env: Vec<(&str, OsString)> =
        pristine.list.iter().map(|l| ("MAKEPAD_RUN_OVERLAY", l.clone().into())).collect();
    // From here the cargo route's Rust mutations run beside relax: its
    // native clean-tree runs are done, and relax reads their files from
    // the shielded copies.
    let mut clean_ok: BTreeMap<Route, bool> = BTreeMap::new();
    std::thread::scope(|s| {
        let beside_cargo = (!beside.is_empty()).then(|| s.spawn(|| cargo(&beside)));
        enum Job<'a> {
            Tests(&'a (String, Vec<String>), &'a [&'a str]),
            Suite,
            Gate(&'a str),
        }
        let jobs_list: Vec<Job> = batches
            .iter()
            .map(|(k, v)| Job::Tests(k, v))
            .chain(native_suite.iter().map(|_| Job::Suite))
            .chain(native_gates.keys().map(|g| Job::Gate(g)))
            .collect();
        // Which routes each relax clean job serves, and which mutants each
        // route holds. A route's mutants start the moment ITS clean check
        // agrees, not when every route's has: the long renderer mutants are
        // what the wall time is made of.
        let served: Vec<Vec<usize>> = jobs_list
            .iter()
            .map(|j| {
                (0..routes.len())
                    .filter(|&i| match (j, &routes[i]) {
                        (Job::Tests((p, tg), _), Route::Test(t)) => *p == t.package && *tg == t.target,
                        (Job::Suite, Route::Suite) => true,
                        (Job::Gate(g), Route::Renderer(r)) => g == r,
                        _ => false,
                    })
                    .collect()
            })
            .collect();
        let held: Vec<Vec<usize>> =
            routes.iter().map(|r| (0..routed.len()).filter(|&i| routed[i].1 == *r).collect()).collect();
        enum Task {
            Clean(usize),
            Mutant(usize),
        }
        let clean_done = Mutex::new(Duration::ZERO);
        let agreed: Mutex<Vec<bool>> = Mutex::new(vec![false; routes.len()]);
        let results: Mutex<Vec<Option<(bool, Duration)>>> = Mutex::new(vec![None; routed.len()]);
        step(&format!("{} clean-tree routes, then {} mutants as each route agrees, {jobs} at a time", routes.len(), routed.len()));
        // The slow clean runs first (the suite, the gates); their mutants are the slow mutants.
        let mut seed: Vec<usize> = (0..jobs_list.len()).collect();
        seed.sort_by_key(|&j| matches!(jobs_list[j], Job::Tests(..)));
        queue(seed.into_iter().map(Task::Clean).collect(), jobs, |task| match task {
            Task::Clean(j) => {
                let relax = match &jobs_list[j] {
                    Job::Tests((package, target), names) => timed(|| relax_test(package, target, names, &pristine_env)),
                    Job::Suite => timed(|| run(m, &Route::Suite, &renderer, &pristine_env, jobs)),
                    Job::Gate(g) => {
                        let mut env = pristine_env.clone();
                        env.extend(gate_views(g));
                        timed(|| run(m, &Route::Renderer(g.to_string()), &renderer, &env, jobs))
                    }
                };
                let mut next = Vec::new();
                for &i in &served[j] {
                    let r = &routes[i];
                    let native = match r {
                        Route::Test(_) => native_tests.as_ref().expect("a test route has a native batch"),
                        Route::Suite => native_suite.as_ref().expect("the suite route has a native run"),
                        Route::Renderer(g) => &native_gates[g.as_str()],
                    };
                    let (good, s) = clean_check(r, native, &relax);
                    report(&s);
                    agreed.lock().unwrap_or_else(|e| e.into_inner())[i] = good;
                    if good {
                        next.extend(held[i].iter().map(|&k| Task::Mutant(k)));
                    } else {
                        for &k in &held[i] {
                            let mu = routed[k].0;
                            report(&format!(
                                "\n── mutation: {} → {r}\nFAIL  {} — its route failed the clean-tree check; a red here would prove nothing.\n",
                                mu.name, mu.name
                            ));
                            results.lock().unwrap_or_else(|e| e.into_inner())[k] = Some((false, Duration::ZERO));
                        }
                    }
                }
                *clean_done.lock().unwrap_or_else(|e| e.into_inner()) = t_checks.elapsed();
                next
            }
            Task::Mutant(k) => {
                let (mu, r) = &routed[k];
                let (good, s, took) = prove_one(m, mu, r, &renderer, &pristine, jobs);
                report(&s);
                results.lock().unwrap_or_else(|e| e.into_inner())[k] = Some((good, took));
                Vec::new()
            }
        });
        clean_ok = routes.iter().cloned().zip(agreed.into_inner().unwrap_or_else(|e| e.into_inner())).collect();
        let checks_time = clean_done.into_inner().unwrap_or_else(|e| e.into_inner());
        let mutants_time = t_checks.elapsed();
        let results: Vec<(&str, bool, Duration)> = routed
            .iter()
            .zip(results.into_inner().unwrap_or_else(|e| e.into_inner()))
            .map(|((mu, _), r)| {
                let (good, took) = r.unwrap_or((false, Duration::ZERO));
                (mu.name.as_str(), good, took)
            })
            .collect();
        fail |= results.iter().any(|(_, good, _)| !good);

        if !results.is_empty() {
            let summed: f64 = results.iter().map(|(_, _, d)| d.as_secs_f64()).sum();
            println!();
            println!(
                "RELAX     {:.0}s wall over {} mutations, {jobs} at a time: the last clean-tree route agreed at {:.0}s ({} routes, native + relax), \
                 all done at {:.0}s ({:.0}s of mutants summed)",
                wall.elapsed().as_secs_f64(),
                results.len(),
                checks_time.as_secs_f64(),
                routes.len(),
                mutants_time.as_secs_f64(),
                summed
            );
            let mut slowest: Vec<(&str, Duration)> = results.iter().map(|(n, _, d)| (*n, *d)).collect();
            slowest.sort_by_key(|t| std::cmp::Reverse(t.1));
            let top: Vec<String> = slowest.iter().take(5).map(|(n, d)| format!("{n} {:.0}s", d.as_secs_f64())).collect();
            println!("          slowest: {}", top.join(", "));
        }
        if let Some(h) = beside_cargo {
            fail |= !h.join().unwrap_or(false);
        }
    });
    if !after.is_empty() {
        step(&format!("{} mutation{} on cargo, after relax", after.len(), if after.len() == 1 { "" } else { "s" }));
        fail |= !cargo(&after);
    }
    // Disk mutants last and one at a time: they write files the renderer
    // reads at run time, which nothing else may be reading.
    for (mu, r) in &on_disk {
        let (good, s, _) = if clean_ok.get(r).copied().unwrap_or(false) {
            prove_on_disk(m, mu, r, &renderer, jobs)
        } else {
            (false, format!("\n── mutation: {} → {r}\nFAIL  {} — its route failed the clean-tree check.\n", mu.name, mu.name), Duration::ZERO)
        };
        report(&s);
        fail |= !good;
    }
    println!(
        "{}",
        if fail {
            "RELAX     FAILURES — see above".to_string()
        } else {
            format!("RELAX     {under_relax} mutation{} reddened under relax", if under_relax == 1 { "" } else { "s" })
        }
    );
    !fail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    /// The route follows the gate's declaration and the test `expect` names.
    /// A mutant's file is overlaid for the members that read it alone: a
    /// renderer file for the renderer, a contract-crate file for it and
    /// every member built on it.
    #[test]
    fn a_file_is_read_by_its_member_and_the_members_built_on_it() {
        let names = |f: &str| packages_reading(&root().join(f)).into_iter().collect::<Vec<_>>();
        assert_eq!(names("native/src/glyph_trie.rs"), ["glyph3d-native"]);
        assert_eq!(names("crates/glyph-field/src/host_slots.rs"), ["glyph-field", "glyph-field-derived", "glyph3d-native"]);
        assert_eq!(names("glyph/src/gates.rs"), ["glyph"]);
    }

    /// The suite route runs the workspace's members and nothing else: the
    /// makepad host beside them is `exclude`d, and running it under relax
    /// failed the clean tree every time (2026-10-31).
    #[test]
    fn workspace_packages_are_the_members_alone() {
        let mut names: Vec<String> = workspace_packages().into_iter().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(names, ["definition-engine", "glyph", "glyph-field", "glyph-field-derived", "glyph-scene-graph", "glyph3d-native"]);
    }

    #[test]
    fn routes_follow_the_declaration_and_the_test_expect_names() {
        let m = load().expect("build.toml loads");
        let by = |name: &str| m.mutation.iter().find(|mu| mu.name == name).expect("declared");
        let test = |name: &str| match route(&m, by(name)) {
            Ok(Some((Route::Test(t), Apply::Overlay))) => t,
            other => panic!("{name}: {other:?}"),
        };
        let t = test("scene-graph-stale-handle-aliases");
        assert_eq!((t.package.as_str(), t.target.clone()), ("glyph-scene-graph", vec!["--lib".to_string()]));
        let t = test("since-matches-whole-gate-command");
        assert_eq!((t.package.as_str(), t.target.clone()), ("glyph", vec!["--bins".to_string()]));
        let t = test("derived-empty-load-panics");
        assert_eq!((t.package.as_str(), t.target.clone()), ("glyph3d-native", vec!["--test".to_string(), "empty_load".to_string()]));
        assert_eq!(test("derived-curve-cap-128").target, ["--test", "curve_cap"], "curve_cap reads the embedded shader");
        assert_eq!(route(&m, by("tests-silently-ignored")), Ok(Some((Route::Suite, Apply::Overlay))), "the floor's text: the suite");
        assert_eq!(route(&m, by("hyper-head-advance")), Ok(Some((Route::Renderer("hyper-oracle".into()), Apply::Overlay))));
        assert_eq!(route(&m, by("emoji-uv-flip")), Ok(Some((Route::Renderer("pixel-ab".into()), Apply::Overlay))), "include_str!'d WGSL");
        assert_eq!(route(&m, by("fixture-deleted")), Ok(Some((Route::Renderer("reference-port".into()), Apply::Disk))), "a fixture, read at run time");
        assert_eq!(route(&m, by("tests-mod-dropped")), Ok(None), "orphans declares no route: cargo proves it");
        let out = "test a::tests::t ... ok\ntest b::t_longer ... ok\ntest c::tests::t ... FAILED\n";
        assert_eq!(test_lines(out, "t"), [("a::tests::t".to_string(), "ok".to_string()), ("c::tests::t".to_string(), "FAILED".to_string())]);
        assert_eq!(expect_view("emoji.png diverges from baseline").as_deref(), Some("emoji"));
        assert_eq!(expect_view("hyper-oracle FAIL"), None);
        let targets = workspace_test_targets();
        assert!(targets.contains(&("glyph".to_string(), vec!["--bins".to_string()])), "a bin-only package");
        assert!(targets.contains(&("glyph3d-native".to_string(), vec!["--test".to_string(), "tab_stops".to_string()])));
    }
}
