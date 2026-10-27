//! glyph — build, test and run this project.
//!
//! Three verbs, because that is the loop:
//!
//!     glyph build          bring the runnable binary up to date
//!     glyph test [scope]   run the checks; nonzero if anything is wrong
//!     glyph run  [args]    launch the renderer
//!
//! Scope is an argument, not a family of verbs: `glyph test rust` after
//! touching Rust, `glyph test render` after touching a shader, and so on. The
//! twelve-item gate list this replaced was organised around the checks; this is
//! organised around what you just changed. The gates still exist — they are an
//! implementation detail behind `test`, and `glyph gates` prints them.
//!
//! `test` BUILDS what it needs, because that is the iterating intent — a stale
//! artifact silently tests the wrong build, which has cost this repo a day and
//! a bogus bisect. `test --frozen` is the other intent: assert everything is already
//! current and fail if it is not. A check that silently rebuilds can never tell
//! you your commit was incomplete.
//!
//! The manifest (`build.toml`) is parsed into the types in `manifest` with
//! `deny_unknown_fields`, so a key the code does not know is a hard error, and
//! an unread field is a `dead_code` warning against a zero-warning gate. Both
//! directions matter: this repo shipped a manifest whose `needs` edges were
//! declared and read by nothing.

mod artifacts;
mod gates;
mod golden;
mod manifest;
mod paths;
mod products;
mod prove;
mod schedule;
mod test_build;
mod prove_relax;

use clap::{Parser, Subcommand};
use std::process::{Command, ExitCode};

use crate::artifacts::verify_committed;
use crate::gates::run_gate;
use crate::schedule::{run_gates, time_line};
use crate::golden::cmd_drift;
use crate::manifest::{Gate, load, Manifest, Scope, Tier, validate};
use crate::paths::{resolve_gpu, root, step};
use crate::products::ensure_products;
use crate::prove::{cmd_prove, self_exe};

// ── CLI ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "glyph", about = "Build, test and run glyph3d-native.", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring the renderer and the committed artifacts up to date.
    Build,
    /// Run the checks. No scope runs all of them.
    Test {
        /// Only what this scope covers: rust, render, corpus.
        scope: Option<Scope>,
        /// Assert everything is already current instead of building it.
        /// Use this to validate a commit: if something is stale, that IS the finding.
        #[arg(long)]
        frozen: bool,
        /// Only the `inner` tier: what to run after each change. The `merge`
        /// gates (the compiler's diagnostics) are skipped, and named; a run
        /// without this flag runs every tier, and is the one to land on.
        #[arg(long)]
        inner: bool,
    },
    /// Bring the renderer up to date, then launch it. Arguments are passed
    /// through; none at all opens its launcher.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Prove the checks can fail: apply each declared mutation, require its
    /// gate to redden for its stated reason, restore. A gate that declares
    /// `relax` has its mutations proven under makepad's relax JIT, in
    /// parallel and without writing the tree (`GLYPH_RELAX` names the binary;
    /// prove_relax.rs); the rest on cargo.
    Prove {
        /// Only mutations targeting this gate.
        #[arg(long)]
        gate: Option<String>,
        /// Only these mutations (repeatable). The iteration form: prove the
        /// mutations YOUR change added or moved, not the whole manifest.
        #[arg(long = "mutation")]
        mutations: Vec<String>,
        /// Only mutations whose target file differs from HEAD — "prove what
        /// I touched" before committing. The same as `--since HEAD`.
        /// Combines with --gate/--mutation by intersection.
        #[arg(long, conflicts_with = "since")]
        changed: bool,
        /// Only mutations whose target file differs between the working tree
        /// and the merge-base of REF and HEAD — "prove what this branch
        /// touched", committed or not — plus every mutation of a gate whose
        /// own script changed. Combines with --gate/--mutation by intersection.
        #[arg(long, value_name = "REF")]
        since: Option<String>,
        /// Print the selected mutations and their gates, then stop: what a
        /// run would prove, before paying for it. Builds and mutates nothing.
        #[arg(long)]
        list: bool,
        /// How many relax mutants (and clean-tree runs) go at once. A relax
        /// process peaks near 3 GB; size it to RAM.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
    },
    /// Run a single check by name. Used by `prove`, and useful on its own.
    Gate {
        name: String,
        /// As `glyph test --inner` runs it: a gate's own merge-tier parts
        /// (pixel-ab's `golden_equivalents_merge`) are skipped, and named.
        #[arg(long)]
        inner: bool,
    },
    /// Check build.toml is internally consistent.
    Validate,
    /// What each check compares, and what it cannot see.
    Gates,
    /// The artifact graph.
    Graph,
    /// Render this host's golden views and lay them against every OTHER
    /// rasterizer's golden set: how many pixels differ and in what shape.
    /// An instrument, not a gate — cross-vendor equality is not a goal.
    Drift,
    /// The renderer's terminal launcher: `run --launcher` (C27, 2026-10-10;
    /// the launcher is part of the renderer now, `native/src/launcher/`).
    Tui,
}

/// `run`: the renderer, current, in YOUR directory. Until C27 (2026-10-10)
/// this ran whatever binary was there, current or not; the products step
/// costs a hash of the inputs when nothing changed.
fn cmd_run(m: &Manifest, args: &[String]) -> bool {
    if !ensure_products(m, false) {
        return false;
    }
    // Runs in YOUR directory, not native/. A file argument means what it says
    // relative to where you typed it — anything else would make
    // `--render-file main.rs` from your own project silently open
    // native/main.rs. The checks cd to native/ because they pass
    // native-relative fixture paths on purpose; that is their business, not
    // yours.
    //
    // stdio is inherited rather than captured: this launches a windowed app
    // (or the terminal launcher), and buffering its output is useless.
    let cwd = std::env::current_dir().unwrap_or_else(|_| root());
    Command::new(root().join("target/release/glyph3d-native"))
        .args(args)
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cmd_test(m: &Manifest, scope: Option<Scope>, frozen: bool, inner: bool) -> bool {
    let t_run = std::time::Instant::now();
    step(if frozen { "products: asserting currency (--frozen)" } else { "products" });
    let mut ok = ensure_products(m, frozen);
    let products = t_run.elapsed().as_secs_f64();
    if !ok && frozen {
        // Everything below would test an artifact we just said is wrong.
        println!("\nCHECK: FAILURES — see above");
        return false;
    }
    let in_scope: Vec<&Gate> = m.gate.iter().filter(|g| scope.is_none_or(|s| g.scope == s)).collect();
    let (selected, skipped): (Vec<&Gate>, Vec<&Gate>) =
        in_scope.into_iter().partition(|g| !inner || g.tier == Tier::Inner);
    if !skipped.is_empty() {
        step("skipped: the merge tier (--inner)");
        for g in &skipped {
            println!("SKIP  {} — tier merge: runs in a full `glyph test`, not after each change", g.name);
        }
    }
    if selected.is_empty() {
        // Say WHY nothing ran, so whoever reads this can tell a wrong scope
        // from a manifest that lost its gates.
        step("nothing to run");
        println!("      gates by scope:");
        for s in [Scope::Rust, Scope::Render, Scope::Corpus] {
            let names: Vec<&str> = m.gate.iter().filter(|g| g.scope == s).map(|g| g.name.as_str()).collect();
            println!("        {:<7} {}", scope_name(s), if names.is_empty() { "(none)".to_string() } else { names.join(", ") });
        }
    }
    let ran = run_gates(m, &selected, inner);
    ok &= ran.iter().all(|r| r.ok);
    let (ok, verdict) = test_verdict(selected.len(), m.gate.len(), scope, inner, ok);
    println!();
    println!("{}", time_line(&selected, &ran, products, t_run.elapsed().as_secs_f64()));
    println!("{verdict}");
    ok
}

fn scope_name(s: Scope) -> String {
    format!("{s:?}").to_lowercase()
}

/// The last line of `glyph test`, and whether the run passed.
///
/// A run that selected NO gate is not green, whatever else held: it checked
/// nothing, and "ALL GATES GREEN" over an empty set is the check-that-cannot-
/// fail shape AGENTS.md warns about. Every verdict also says how many gates
/// ran, so a reader (or an agent) can see an unexpected count even when the
/// run is green.
fn test_verdict(selected: usize, total: usize, scope: Option<Scope>, inner: bool, ok: bool) -> (bool, String) {
    let mut which = format!("{selected} of {total} gates ran");
    if let Some(s) = scope {
        which.push_str(&format!(", scope {}", scope_name(s)));
    }
    if inner {
        which.push_str(", tier inner — not a landing run");
    }
    if selected == 0 {
        return (false, format!("CHECK-ALL: NOTHING RAN — {which}; a run that checks nothing is not a pass"));
    }
    if ok {
        (true, format!("CHECK-ALL: ALL GATES GREEN — {which}"))
    } else {
        (false, format!("CHECK-ALL: FAILURES — see above ({which})"))
    }
}

/// What `cargo glyph` expands to — `.cargo/config.toml`'s alias, pinned to it
/// by `alias_expansion_matches_cargo_config`.
const ALIAS_EXPANSION: [&str; 6] = ["run", "--quiet", "--release", "-p", "glyph", "--"];

/// D2 (2026-10-09). Cargo merges `.cargo/config.toml` from every ancestor
/// directory and CONCATENATES alias arrays, so in a worktree nested inside the
/// repo (`.claude/worktrees/<name>`, where Claude Code puts its own) `cargo
/// glyph test` reaches us as `run --quiet --release -p glyph -- test`, once per
/// extra level. Strip every leading copy. Done here rather than by making the
/// alias a string (which cargo does not merge): a checkout on the array form
/// and a worktree on the string form make cargo refuse to load its config at
/// all (measured), so that fix breaks every nested worktree on another branch.
fn strip_doubled_alias(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let n = ALIAS_EXPANSION.len();
    while args.len() > n && args[1..=n].iter().zip(ALIAS_EXPANSION).all(|(a, b)| a == b) {
        args.drain(1..=n);
    }
    args
}

fn main() -> ExitCode {
    // Before anything can rebuild us out from under ourselves; see self_exe.
    let _ = self_exe();
    let cli = Cli::parse_from(strip_doubled_alias(std::env::args_os().collect()));

    let m = match load() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("FAIL  {e}");
            return ExitCode::from(1);
        }
    };
    let ok = match cli.cmd {
        Cmd::Build => {
            let mut ok = ensure_products(&m, false);
            step("committed artifacts");
            ok &= verify_committed(&m);
            ok
        }
        Cmd::Test { scope, frozen, inner } => cmd_test(&m, scope, frozen, inner),
        Cmd::Run { args } => cmd_run(&m, &args),
        Cmd::Prove { gate, mutations, changed, since, list, jobs } => {
            let since = if changed { Some("HEAD") } else { since.as_deref() };
            cmd_prove(&m, gate.as_deref(), &mutations, since, list, jobs)
        }
        Cmd::Drift => cmd_drift(&m),
        Cmd::Tui => cmd_run(&m, &["--launcher".to_string()]),
        Cmd::Gate { name, inner } => match m.gate.iter().find(|g| g.name == name) {
            Some(g) => run_gate(g, &m, inner),
            None => {
                println!("FAIL  no gate named {name}");
                false
            }
        },
        Cmd::Validate => {
            let problems = validate(&m);
            if problems.is_empty() {
                println!("PASS  build.toml is internally consistent");
                println!(
                    "      {} artifacts, {} gates, {} mutations, all `needs` resolved and ordered",
                    m.artifact.len(),
                    m.gate.len(),
                    m.mutation.len()
                );
                true
            } else {
                for p in &problems {
                    println!("FAIL  {p}");
                }
                println!("\n{} problem(s) in build.toml", problems.len());
                false
            }
        }
        Cmd::Gates => {
            for g in &m.gate {
                println!("  {}  [{:?}, tier {:?}]", g.name, g.scope, g.tier);
                if !g.apart_from.is_empty() {
                    println!("    apart from : {}", g.apart_from.join(", "));
                }
                if let Some(c) = &g.compare {
                    println!("    compares : {c}");
                }
                if let Some(b) = &g.blind_to {
                    println!("    blind to : {b}");
                }
                // The pixel gate's equivalents, and every view exempt from
                // one: an exemption is a named blind spot, listed here so it
                // is priced like the rest.
                if g.kind == manifest::Kind::GoldenVerify {
                    if !m.settings.golden_equivalents.is_empty() {
                        println!("    equivalents : {}", m.settings.golden_equivalents.join("; "));
                    }
                    if !m.settings.golden_equivalents_merge.is_empty() {
                        println!("    merge tier  : {}", m.settings.golden_equivalents_merge.join("; "));
                    }
                    for v in &m.golden_view {
                        for e in &v.exempt {
                            println!("    exempt   : {} under {} — {}", v.name, e.equivalent, e.why);
                        }
                    }
                }
            }
            println!("\n  test floor: {} (ratchet)", m.settings.test_floor);
            println!("  {} golden views, {} declared mutations", m.golden_view.len(), m.mutation.len());
            true
        }
        Cmd::Graph => {
            for (name, a) in &m.artifact {
                println!("  {name}  [{:?}]", a.class);
                for o in &a.outputs {
                    // Show the path this host would use when the key is known.
                    println!("    out:  {}", resolve_gpu(o).unwrap_or_else(|| o.clone()));
                }
                for i in &a.inputs {
                    println!("    in:   {i}");
                }
                if let Some(c) = &a.counts {
                    println!("    declared counts: {c:?}");
                }
                if let Some(n) = &a.note {
                    println!("    note: {n}");
                }
                println!();
            }
            for v in &m.golden_view {
                println!("  golden view {:<12} {}", v.name, v.cmd);
            }
            true
        }
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_gate_selection_is_refused() {
        let (ok, line) = test_verdict(0, 9, Some(Scope::Corpus), false, true);
        assert!(!ok, "a run that selected no gate must not pass");
        assert!(line.starts_with("CHECK-ALL: NOTHING RAN"), "{line}");
        assert!(line.contains("0 of 9 gates ran, scope corpus"), "{line}");
    }

    #[test]
    fn verdict_reports_how_many_gates_ran() {
        assert_eq!(
            test_verdict(4, 9, Some(Scope::Rust), false, true),
            (true, "CHECK-ALL: ALL GATES GREEN — 4 of 9 gates ran, scope rust".to_string())
        );
        assert_eq!(
            test_verdict(12, 17, None, true, true),
            (true, "CHECK-ALL: ALL GATES GREEN — 12 of 17 gates ran, tier inner — not a landing run".to_string())
        );
        assert_eq!(
            test_verdict(9, 9, None, false, false),
            (false, "CHECK-ALL: FAILURES — see above (9 of 9 gates ran)".to_string())
        );
    }

    /// The doubled alias of a nested worktree is stripped, at any depth, and
    /// nothing else is.
    #[test]
    fn doubled_alias_is_stripped() {
        let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let mut nested = vec!["glyph"];
        nested.extend(ALIAS_EXPANSION);
        nested.extend(ALIAS_EXPANSION);
        nested.push("test");
        assert_eq!(strip_doubled_alias(os(&nested)), os(&["glyph", "test"]));
        assert_eq!(strip_doubled_alias(os(&["glyph", "test", "rust"])), os(&["glyph", "test", "rust"]));
        // `glyph run` passes its arguments to the renderer: left alone.
        assert_eq!(strip_doubled_alias(os(&["glyph", "run", "--demo"])), os(&["glyph", "run", "--demo"]));
    }

    /// The constant is the alias cargo actually expands.
    #[test]
    fn alias_expansion_matches_cargo_config() {
        let cfg: toml::Table = std::fs::read_to_string(root().join(".cargo/config.toml"))
            .expect(".cargo/config.toml reads")
            .parse()
            .expect(".cargo/config.toml parses");
        let alias = cfg["alias"]["glyph"].as_array().expect("alias.glyph is an array");
        let alias: Vec<&str> = alias.iter().map(|v| v.as_str().expect("string")).collect();
        assert_eq!(alias, ALIAS_EXPANSION);
    }
}
