//! The gates: how each kind runs, and the order its `needs` enforce.

use crate::artifacts::verify_committed;
use crate::golden::verify_golden;
use crate::manifest::{Class, Gate, Kind, Manifest};
use crate::paths::{expand, native, resolve_gpu, root, sh};

fn warn_count(out: &str) -> usize {
    out.lines().filter(|l| l.starts_with("warning") && !l.contains("generated")).count()
}

fn gate_cargo(g: &Gate) -> bool {
    let cmd = match g.cmd.as_deref() {
        Some("build") => "cargo build --release",
        Some("clippy") => "cargo clippy --release --all-targets",
        // Doc links are the one rename hazard nothing else in this battery can
        // see: `[`Engine::records`]` kept pointing at a method that had been
        // renamed to `read_back`, through a full green run, because rustdoc is
        // never invoked. Found by an audit 2026-09-07, not by a check.
        // `--no-deps` keeps it to this crate's own prose.
        Some("doc") => "cargo doc -p glyph3d-native --no-deps",
        // The renderer without its default features (no egui overlay): a
        // supported build that nothing compiled until 2026-10-09, when it
        // was found broken (C24). Its own target dir, so the changed feature
        // set never invalidates the main build.
        Some("check-no-ui") => {
            "cargo check -p glyph3d-native --no-default-features --all-targets --target-dir target/no-ui"
        }
        other => {
            println!("FAIL  {} has unknown cargo cmd {other:?}", g.name);
            return false;
        }
    };
    let (good, out) = sh(cmd, &root());
    if !good {
        println!("{}", out.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
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
        println!("FAIL  {} — {w} warnings", g.name);
        false
    }
}

/// The floor is a RATCHET, not an equality: tests are added constantly, so an
/// exact pin would redden on the most common good action in the repo. It exists
/// because the previous form counted "test result: ok" LINES and required two —
/// and there are exactly two test binaries, so it could not fail. A green run
/// above the floor prints the value to raise it to, so it cannot quietly decay.
fn gate_cargo_test(m: &Manifest) -> bool {
    let (good, out) = sh("cargo test --release", &root());
    for l in out.lines().filter(|l| l.contains("test result")) {
        println!("{l}");
    }
    if !good {
        // Name the failing tests: printing only the summary discards WHY it
        // reddened, and a mutation's `expect` needs the reason in the text.
        for l in out.lines().filter(|l| l.contains("FAILED") || l.starts_with("panicked")) {
            println!("      {l}");
        }
        println!("FAIL  cargo test");
        return false;
    }
    let binaries = out.lines().filter(|l| l.contains("test result: ok")).count();
    let total: u32 = out
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|w| w[1].starts_with("passed"))
        .filter_map(|w| w[0].parse::<u32>().ok())
        .sum();
    let floor = m.settings.test_floor;
    if binaries < 2 {
        println!("FAIL  cargo test — only {binaries} test binary reported; one stopped running");
        return false;
    }
    if total < floor {
        println!("FAIL  cargo test — {total} tests ran, floor is {floor}. Coverage DROPPED by {}.", floor - total);
        println!("      A deleted test, an #[ignore] that outlived its reason, or a module");
        println!("      that stopped being compiled. Lower the floor only on purpose.");
        return false;
    }
    println!("PASS  cargo test — {total} tests over {binaries} binaries (floor {floor})");
    if total > floor {
        println!("NOTE  the floor is behind: raise test_floor to {total} in build.toml");
    }
    true
}

fn gate_repo_verify(g: &Gate) -> bool {
    let mut ok = true;
    let template = g.cmd.as_deref().unwrap_or_default();
    for mode in &g.modes {
        let (good, out) = sh(
            &format!(
                "../target/release/glyph3d-native {}",
                template.replace("{mode}", mode)
            ),
            &native(),
        );
        // The result line is not necessarily the last one: the renderer logs
        // after it. Search, do not assume position.
        match out.lines().find(|l| l.contains("repo-verify PASS")) {
            Some(hit) if good => println!("PASS  --wrap-mode {mode} — {}", hit.trim()),
            _ => {
                for l in out.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
                    println!("      {l}");
                }
                println!("FAIL  --wrap-mode {mode}");
                ok = false;
            }
        }
    }
    ok
}

fn gate_cmd(g: &Gate) -> bool {
    let cmd = g.cmd.as_deref().unwrap_or_default();
    let (good, out) = sh(cmd, &root());
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
        let detail: Vec<&str> =
            out.lines().filter(|l| l.starts_with("PASS ") || l.starts_with("FAIL ")).collect();
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

pub(crate) fn run_gate(g: &Gate, m: &Manifest) -> bool {
    if !needs_met(g, m) {
        return false;
    }
    match g.kind {
        Kind::VerifyCommitted => verify_committed(m),
        Kind::Cmd => gate_cmd(g),
        Kind::Cargo => gate_cargo(g),
        Kind::CargoTest => gate_cargo_test(m),
        Kind::GoldenVerify => verify_golden(m),
        Kind::RepoVerify => gate_repo_verify(g),
    }
}
