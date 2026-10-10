//! prove: break each check, require it to notice.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::{Manifest, Mutation};
use crate::paths::{root, sh, stamps, step};
use crate::products::{ensure_products, products_reading};

//
// A gate's value is what it REJECTS; its green only repeats what you already
// assumed. Each guard below exists because its absence has cost this repo real
// time:
//   green first     — a red on an already-red tree proves nothing
//   assert applied  — "I broke it and nothing failed" and "I failed to break
//                     it" print identically; a failed edit is silent
//   builds first    — a mutation that does not COMPILE reddens its gate for a
//                     reason the declaration did not state
//   right gate/text — a mutation reddening some OTHER gate is not evidence
//   restore proven  — byte-compare the snapshot back; never `git checkout` a
//                     directory, which once reverted generators alongside
//                     fixtures and ate a live edit
//
// Reported as COVERAGE, not a pass count: "N gates, C covered, U uncovered".
// "8 mutations passed" describes the size of what ran, not the size of what
// exists — the mistake the old ls-derived fixture count made.

/// Our own path, read ONCE at startup. Cargo keeps two `glyph` artifacts —
/// the workspace build and the `-p glyph` build the `cargo glyph` alias makes
/// resolve different feature sets — and re-points `target/release/glyph` at
/// whichever was asked for last, without recompiling. The cargo-build gate runs
/// the workspace build, so mid-`prove` the file this process was started from
/// is unlinked and replaced. On Linux `current_exe()` then reads
/// `/proc/self/exe` as `.../glyph (deleted)` and every later spawn fails with
/// ENOENT — measured 2026-09-07: 5 mutations proved, then 12 straight FAILs
/// ("could not run gate", then "ALREADY RED" for everything after). macOS
/// returns the plain path and never saw it. Reading the path before any gate
/// runs makes the spawn hit whichever artifact is there now, which is built
/// from the same source.
pub(crate) fn self_exe() -> &'static Path {
    static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("glyph")))
}

/// Runs one gate in a child of ourselves so its output can be read.
fn gate_output(name: &str) -> (bool, String) {
    let out = Command::new(self_exe()).arg("gate").arg(name).current_dir(root()).output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("could not run gate {name}: {e}")),
    }
}

/// Apply, returning the original bytes. Err if the edit did not land — a failed
/// replace is silent, and an unapplied mutation looks exactly like a check that
/// caught nothing.
/// Non-overlapping occurrences of `find` in `text` (an empty `find` counts
/// as absent: it would "match" everywhere and mutate nothing in particular).
fn find_matches(text: &str, find: &str) -> usize {
    if find.is_empty() {
        0
    } else {
        text.matches(find).count()
    }
}

fn apply_mutation(mu: &Mutation) -> Result<Vec<u8>, String> {
    let f = root().join(&mu.file);
    let before = std::fs::read(&f).map_err(|e| format!("{}: {e}", mu.file))?;
    match mu.op.as_str() {
        "append" => {
            let mut v = before.clone();
            v.extend_from_slice(mu.arg.as_deref().unwrap_or_default().as_bytes());
            std::fs::write(&f, &v).map_err(|e| e.to_string())?;
        }
        "replace" => {
            let text = String::from_utf8(before.clone()).map_err(|_| "not UTF-8".to_string())?;
            let find = mu.find.as_deref().unwrap_or_default();
            match find_matches(&text, find) {
                0 => return Err(format!("find-text absent from {}; mutation cannot land", mu.file)),
                1 => {}
                // Ambiguous: `replacen(.., 1)` would mutate whichever match comes
                // first, which is an accident of file order, not a choice. Until
                // 2026-10-09 (C11) two build.toml mutations matched their own
                // `find =` entry too and were right only because the target
                // came first; another picked the first of seven #[test]s.
                n => {
                    return Err(format!(
                        "find-text matches {n} times in {}; anchor it so it names one place",
                        mu.file
                    ))
                }
            }
            let with = mu.with_.as_deref().unwrap_or_default();
            std::fs::write(&f, text.replacen(find, with, 1)).map_err(|e| e.to_string())?;
        }
        // The file goes; the restore below writes `before` back, which is
        // what makes this uniform with the other ops. validate() accepted
        // `remove` and build.toml documented it, but until 2026-10-09 this
        // match did not, so a remove mutation failed as "unknown op" in prove.
        "remove" => std::fs::remove_file(&f).map_err(|e| format!("{}: {e}", mu.file))?,
        other => return Err(format!("unknown op {other}")),
    }
    if std::fs::read(&f).unwrap_or_default() == before {
        return Err(format!("mutation did not change {}", mu.file));
    }
    Ok(before)
}

pub(crate) fn cmd_prove(
    m: &Manifest,
    only: Option<&str>,
    only_mutations: &[String],
    changed: bool,
) -> bool {
    let covered: BTreeSet<&str> = m.mutation.iter().map(|mu| mu.gate.as_str()).collect();
    let uncovered: Vec<&str> =
        m.gate.iter().map(|g| g.name.as_str()).filter(|n| !covered.contains(n)).collect();
    let mut fail = false;

    // --changed: the mutation set whose target files differ from HEAD.
    // Repo-relative paths on both sides (git runs at the root).
    let changed_files: Option<std::collections::BTreeSet<String>> = changed.then(|| {
        let out = Command::new("git")
            .args(["diff", "--name-only", "HEAD"])
            .current_dir(root())
            .output()
            .expect("git diff --name-only runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect()
    });

    let scoped = only.is_some() || !only_mutations.is_empty() || changed;
    let selected: Vec<&Mutation> = m
        .mutation
        .iter()
        .filter(|mu| only.is_none_or(|g| mu.gate == g))
        .filter(|mu| only_mutations.is_empty() || only_mutations.contains(&mu.name))
        .filter(|mu| {
            changed_files
                .as_ref()
                .is_none_or(|cf| cf.contains(&mu.file))
        })
        .collect();
    if scoped {
        println!(
            "prove (scoped: {} of {} mutations{})",
            selected.len(),
            m.mutation.len(),
            if changed { ", files changed vs HEAD" } else { "" },
        );
        if selected.is_empty() {
            println!("  nothing selected — no mutation targets those files/names");
        }
    }

    for mu in selected.iter().copied() {
        step(&format!("mutation: {} → {}", mu.name, mu.gate));

        let (pre_ok, _) = gate_output(&mu.gate);
        if !pre_ok {
            println!("FAIL  {} — gate {} was ALREADY RED before mutating;", mu.name, mu.gate);
            println!("      a red here would prove nothing. Fix the tree first.");
            fail = true;
            continue;
        }

        let before = match apply_mutation(mu) {
            Ok(b) => b,
            Err(e) => {
                println!("FAIL  {} — {e}", mu.name);
                fail = true;
                continue;
            }
        };

        let mut verdict: Option<String> = None;
        let mut built = true;
        if let Some(rb) = &mu.rebuild {
            let (good, out) = sh(rb, &root());
            if !good {
                built = false;
                println!("FAIL  {} — the mutated tree does not BUILD, so this proves", mu.name);
                println!("      nothing about the gate; a red here is the compiler, not the");
                println!("      check. Make the mutation semantic, not syntactic.");
                for l in out.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
                    println!("      {l}");
                }
            }
        }
        if built {
            let (post_ok, out) = gate_output(&mu.gate);
            verdict = Some(if post_ok {
                format!(
                    "FAIL  {} — gate {} stayed GREEN under the mutation.\n      \
                     It does not catch what it claims: {}",
                    mu.name,
                    mu.gate,
                    mu.why.as_deref().unwrap_or("")
                )
            } else if !out.contains(&mu.expect) {
                // Say what it DID print. Without this the operator is told the
                // reason was unstated and given no way to state it.
                let got = out
                    .lines()
                    .rfind(|l| l.starts_with("FAIL"))
                    .or_else(|| out.lines().next_back())
                    .unwrap_or("(no output)");
                format!(
                    "FAIL  {} — {} went red, but for an unstated reason.\n      \
                     expected text containing: {:?}\n      got: {}",
                    mu.name, mu.gate, mu.expect, got.trim()
                )
            } else {
                format!("PASS  {} reddens on {} — {:?}", mu.gate, mu.name, mu.expect)
            });
        }

        // Restore, always, and prove it came back.
        let f = root().join(&mu.file);
        let _ = std::fs::write(&f, &before);
        let restored = std::fs::read(&f).unwrap_or_default() == before;
        if let Some(rb) = &mu.rebuild {
            let (good, out) = sh(rb, &root());
            if !good {
                println!("FATAL {} — restore rebuild FAILED. The tree now has original", mu.name);
                println!("      sources and a stale artifact; every later check would test the");
                println!("      wrong binary. Run: cargo glyph build");
                println!("      {}", out.lines().last().unwrap_or(""));
                return false;
            }
        } else {
            // No declared rebuild, but the gate may have built a product from
            // the mutated file anyway (found 2026-10-09: `tail-pads-zero` left
            // a mutated renderer for every later gate and every hand timing).
            // Its stamp still matches the restored sources, so drop it and
            // let currency rebuild.
            let stale = products_reading(m, &mu.file);
            if !stale.is_empty() {
                for name in &stale {
                    let _ = std::fs::remove_file(stamps().join(format!("{name}.sha256")));
                }
                if !ensure_products(m, false) {
                    println!("FATAL {} — rebuilding {} after restore FAILED; every later", mu.name, stale.join(", "));
                    println!("      check would test the mutated binary. Run: cargo glyph build");
                    return false;
                }
            }
        }
        match verdict {
            Some(v) => {
                println!("{v}");
                if v.starts_with("FAIL") {
                    fail = true;
                }
            }
            None => fail = true,
        }
        if !restored {
            println!("FAIL  {} — {} did NOT restore byte-exact", mu.name, mu.file);
            fail = true;
        }
    }

    println!();
    println!(
        "COVERAGE  {} gates, {} with mutations, {} uncovered",
        m.gate.len(),
        covered.len(),
        uncovered.len()
    );
    if !uncovered.is_empty() {
        println!("          uncovered: {}", uncovered.join(", "));
        println!("          an uncovered gate is an unproven claim, not a passing one.");
    }
    println!();
    println!(
        "{}",
        if fail {
            "PROVE: FAILURES — see above".to_string()
        } else if scoped {
            format!(
                "PROVE: {} selected mutation{} reddened — a scoped run proves its scope, not the manifest",
                selected.len(),
                if selected.len() == 1 { "" } else { "s" },
            )
        } else {
            "PROVE: every declared mutation reddened its gate".to_string()
        }
    );
    !fail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    /// A mutation's find must name ONE place (C11), and no declared one names two.
    #[test]
    fn mutation_finds_are_unambiguous() {
        assert_eq!(find_matches("a b a", "a"), 2);
        assert_eq!(find_matches("a b", ""), 0);
        let m = load().expect("build.toml loads");
        for mu in m.mutation.iter().filter(|mu| mu.op == "replace") {
            let text = std::fs::read_to_string(root().join(&mu.file)).expect("mutation target reads");
            let n = find_matches(&text, mu.find.as_deref().unwrap_or_default());
            // At most one: zero is the prover's own refusal ("find-text
            // absent"), and is also what this test sees for a mutation that is
            // APPLIED while cargo-test runs under prove.
            assert!(n <= 1, "mutation {} matches {n} times in {}", mu.name, mu.file);
        }
    }
}
