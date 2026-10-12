//! prove: break each check, require it to notice.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::{Class, Manifest, Mutation};
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

/// The files that differ between the working tree and the merge-base of `since`
/// and HEAD, repo-relative (git runs at the root). Against the merge-base, not
/// `since` itself, so commits that landed on `since` after the branch left it
/// do not count as this branch's changes. `None`, said why, if git refuses.
fn changed_since(since: &str) -> Option<(String, ChangeSet)> {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root())
            .output()
            .map_err(|e| e.to_string())
            .and_then(|o| {
                if o.status.success() {
                    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
                } else {
                    Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
                }
            })
    };
    let base = match git(&["merge-base", since, "HEAD"]) {
        Ok(b) => b.trim().to_string(),
        Err(e) => {
            println!("FAIL  --since {since}: git merge-base refused: {e}");
            return None;
        }
    };
    let files: BTreeSet<String> = match git(&["diff", "--name-only", &base]) {
        Ok(out) => out.lines().map(str::to_string).collect(),
        Err(e) => {
            println!("FAIL  --since {since}: git diff refused: {e}");
            return None;
        }
    };
    let (mutations, gates) = if files.contains("build.toml") {
        let current = std::fs::read_to_string(root().join("build.toml")).unwrap_or_default();
        let before = git(&["show", &format!("{base}:build.toml")]).ok();
        declarations_changed(before.as_deref(), &current)
    } else {
        Default::default()
    };
    Some((base, ChangeSet { files, mutations, gates }))
}

/// What a change set touches. Files are repo-relative paths; `declarations`
/// names the `[[mutation]]` and `[[gate]]` entries of build.toml that are new
/// or differ from the base.
struct ChangeSet {
    files: BTreeSet<String>,
    mutations: BTreeSet<String>,
    gates: BTreeSet<String>,
}

/// The `[[mutation]]` and `[[gate]]` entries of `current` that are new or
/// differ from `base`, by name. Read as plain TOML, not as the manifest: the
/// base may predate a schema change and must still be comparable. A base
/// that does not parse (or has no build.toml) makes every entry new.
fn declarations_changed(base: Option<&str>, current: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let entries = |text: Option<&str>, key: &str| -> std::collections::BTreeMap<String, toml::Value> {
        text.and_then(|t| t.parse::<toml::Table>().ok())
            .and_then(|t| t.get(key).and_then(|v| v.as_array()).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| Some((v.get("name")?.as_str()?.to_string(), v)))
            .collect()
    };
    let changed = |key: &str| {
        let before = entries(base, key);
        entries(Some(current), key)
            .into_iter()
            .filter(|(name, v)| before.get(name) != Some(v))
            .map(|(name, _)| name)
            .collect()
    };
    (changed("mutation"), changed("gate"))
}

/// Whether a change set touches a mutation: its target file changed, the
/// script its gate runs did, or its own declaration or its gate's did. The
/// script clause is what makes a gate's own edit re-prove the gate: a change
/// to `tools/check-orphans.sh` cannot weaken a mutation's target, but it can
/// stop the gate noticing it. The declaration clauses are what make a NEW
/// mutation, or an edited one, prove itself on the branch that adds it,
/// whether or not its target file changed. Gates built into the runner
/// (cargo, cargo-test, golden) name no script; a change to the runner itself
/// selects the mutations that target `glyph/`.
fn touches(mu: &Mutation, gate_cmd: Option<&str>, changed: &ChangeSet) -> bool {
    changed.files.contains(&mu.file)
        || gate_cmd.is_some_and(|cmd| cmd.split_whitespace().any(|word| changed.files.contains(word)))
        || changed.mutations.contains(&mu.name)
        || changed.gates.contains(&mu.gate)
}

pub(crate) fn cmd_prove(
    m: &Manifest,
    only: Option<&str>,
    only_mutations: &[String],
    since: Option<&str>,
    list: bool,
) -> bool {
    let covered: BTreeSet<&str> = m.mutation.iter().map(|mu| mu.gate.as_str()).collect();
    let uncovered: Vec<&str> =
        m.gate.iter().map(|g| g.name.as_str()).filter(|n| !covered.contains(n)).collect();
    let mut fail = false;

    // --since REF (and --changed, which is --since HEAD): the mutations the
    // change set touches.
    let changed_files: Option<(String, ChangeSet)> = match since {
        Some(r) => match changed_since(r) {
            Some(c) => Some(c),
            None => return false,
        },
        None => None,
    };
    let gate_cmd = |gate: &str| m.gate.iter().find(|g| g.name == gate).and_then(|g| g.cmd.as_deref());

    let scoped = only.is_some() || !only_mutations.is_empty() || since.is_some();
    let selected: Vec<&Mutation> = m
        .mutation
        .iter()
        .filter(|mu| only.is_none_or(|g| mu.gate == g))
        .filter(|mu| only_mutations.is_empty() || only_mutations.contains(&mu.name))
        .filter(|mu| {
            changed_files
                .as_ref()
                .is_none_or(|(_, cf)| touches(mu, gate_cmd(&mu.gate), cf))
        })
        .collect();
    if scoped {
        let since_note = match (since, &changed_files) {
            (Some(r), Some((base, cf))) => format!(
                ", {} files, {} mutation and {} gate declarations changed since {r} (merge-base {})",
                cf.files.len(),
                cf.mutations.len(),
                cf.gates.len(),
                &base[..base.len().min(10)]
            ),
            _ => String::new(),
        };
        println!(
            "prove (scoped: {} of {} mutations{since_note})",
            selected.len(),
            m.mutation.len(),
        );
        if selected.is_empty() {
            println!("  nothing selected — no mutation targets those files/names");
        }
    }
    if list {
        for mu in &selected {
            println!("  {:<44} {:<20} {}", mu.name, mu.gate, mu.file);
        }
        println!("LIST      {} of {} mutations selected; nothing was built or mutated", selected.len(), m.mutation.len());
        return true;
    }

    // Whether a gate reads a built product (the renderer). `needs_met` only
    // checks that the product EXISTS, so such a gate runs whatever binary is
    // on disk — after a deferred restore, possibly the previous mutant.
    let needs_product = |gate: &str| {
        m.gate.iter().find(|g| g.name == gate).is_some_and(|g| {
            g.needs.iter().any(|n| m.artifact.get(n).is_some_and(|a| a.class == Class::Product))
        })
    };

    // The clean-tree check, ONCE per gate (C21): a gate that is already red
    // proves nothing under a mutation. Every mutation is restored byte-exact
    // (checked below, or the run reports it) and its products rebuilt before
    // any gate reads them, so the answer cannot change mid-run; it used to
    // be asked again for every mutation (13x for cargo-test).
    if !selected.is_empty() {
        step("products, then the clean-tree check of each selected gate");
        if !ensure_products(m, false) {
            println!("FATAL the products do not build on the clean tree; nothing can be proven.");
            return false;
        }
    }
    let t_checks = std::time::Instant::now();
    let mut clean: std::collections::BTreeMap<&str, bool> = std::collections::BTreeMap::new();
    for mu in selected.iter() {
        if !clean.contains_key(mu.gate.as_str()) {
            let (ok, _) = gate_output(&mu.gate);
            clean.insert(mu.gate.as_str(), ok);
        }
    }
    let checks_time = t_checks.elapsed();
    // Products a deferred restore left stale; rebuilt before a gate reads
    // them, and once at the end.
    let mut restore_pending = false;

    // Where a prove's time goes (C21): per mutation, the mutant build, the
    // gate under the mutation, and the restore (index 0, the clean-tree
    // check, is now paid once per gate above).
    let mut timings: Vec<(&str, [std::time::Duration; 4])> = Vec::new();
    for mu in selected.iter().copied() {
        step(&format!("mutation: {} → {}", mu.name, mu.gate));
        let mut phase = [std::time::Duration::ZERO; 4];

        let pre_ok = clean[mu.gate.as_str()];
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
        let t = std::time::Instant::now();
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
        // A gate that reads a product, under a mutation that declares no
        // rebuild of its own: bring products current first, so it never
        // runs a previous mutation's binary left by a deferred restore.
        if built && mu.rebuild.is_none() && needs_product(&mu.gate) && !ensure_products(m, false) {
            println!("FATAL {} — products do not build before its gate; run: cargo glyph build", mu.name);
            return false;
        }
        phase[1] = t.elapsed();
        let t = std::time::Instant::now();
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

        phase[2] = t.elapsed();
        let t = std::time::Instant::now();
        // Restore, always, and prove it came back.
        let f = root().join(&mu.file);
        let _ = std::fs::write(&f, &before);
        let restored = std::fs::read(&f).unwrap_or_default() == before;
        // The restore's rebuild is DEFERRED when the file feeds a product
        // (C21): its stamp is dropped, and currency rebuilds it before the
        // next gate that reads it — usually the next mutation's own mutant
        // build makes that rebuild moot — and once at the end. The stamp
        // must go either way: a declared rebuild or the gate itself may have
        // built the product from the mutated file (found 2026-10-09:
        // `tail-pads-zero` left a mutated renderer behind a stamp reading
        // current). A file no product reads (the runner's own sources) is
        // rebuilt at once: every later gate is spawned from that binary.
        let stale = products_reading(m, &mu.file);
        if !stale.is_empty() {
            for name in &stale {
                let _ = std::fs::remove_file(stamps().join(format!("{name}.sha256")));
            }
            restore_pending = true;
        } else if let Some(rb) = &mu.rebuild {
            let (good, out) = sh(rb, &root());
            if !good {
                println!("FATAL {} — restore rebuild FAILED. The tree now has original", mu.name);
                println!("      sources and a stale artifact; every later check would test the");
                println!("      wrong binary. Run: cargo glyph build");
                println!("      {}", out.lines().last().unwrap_or(""));
                return false;
            }
        }
        phase[3] = t.elapsed();
        timings.push((&mu.name, phase));
        println!(
            "      time: build {:.1}s, gate {:.1}s, restore {:.1}s",
            phase[1].as_secs_f64(),
            phase[2].as_secs_f64(),
            phase[3].as_secs_f64(),
        );
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

    // The deferred restores, settled once: the tree is the original and its
    // products are rebuilt from it before anything else runs.
    let t_final = std::time::Instant::now();
    if restore_pending {
        step("restoring products from the original tree");
        if !ensure_products(m, false) {
            println!("FATAL rebuilding the products after the last restore FAILED; run: cargo glyph build");
            return false;
        }
    }
    let final_restore = t_final.elapsed();

    if !timings.is_empty() {
        let total = |i: usize| timings.iter().map(|(_, p)| p[i].as_secs_f64()).sum::<f64>();
        let all: f64 = (1..4).map(total).sum::<f64>() + checks_time.as_secs_f64() + final_restore.as_secs_f64();
        println!();
        println!(
            "TIME      {:.0}s over {} mutations: clean-tree checks {:.0}s ({} gates, once each), mutant build {:.0}s, \
             gate under mutation {:.0}s, restore {:.0}s (+{:.0}s final)",
            all,
            timings.len(),
            checks_time.as_secs_f64(),
            clean.len(),
            total(1),
            total(2),
            total(3),
            final_restore.as_secs_f64(),
        );
        let mut slowest: Vec<_> = timings.iter().map(|(n, p)| (*n, p.iter().map(|d| d.as_secs_f64()).sum::<f64>())).collect();
        slowest.sort_by(|a, b| b.1.total_cmp(&a.1));
        let top: Vec<String> = slowest.iter().take(5).map(|(n, t)| format!("{n} {t:.0}s")).collect();
        println!("          slowest: {}", top.join(", "));
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

    /// `--since` selects a mutation when its target changed, when the script
    /// its gate runs did (a word of the command, never a substring of one), or
    /// when its own declaration or its gate's is new or edited.
    #[test]
    fn since_selects_by_target_or_gate_script() {
        let mu = |file: &str| Mutation {
            name: "m".into(),
            gate: "g".into(),
            why: None,
            file: file.into(),
            op: "replace".into(),
            expect: "x".into(),
            arg: None,
            find: None,
            with_: None,
            rebuild: None,
        };
        let set = |files: &[&str], mutations: &[&str], gates: &[&str]| ChangeSet {
            files: files.iter().map(|s| s.to_string()).collect(),
            mutations: mutations.iter().map(|s| s.to_string()).collect(),
            gates: gates.iter().map(|s| s.to_string()).collect(),
        };
        let changed = set(&["native/src/text.rs", "tools/check_atlas.py"], &[], &[]);
        assert!(touches(&mu("native/src/text.rs"), None, &changed), "its target changed");
        assert!(!touches(&mu("native/src/fold.rs"), None, &changed), "nothing it reads changed");
        assert!(touches(&mu("assets/atlas/codepoints.bin"), Some("python3 tools/check_atlas.py"), &changed), "its gate's script changed");
        assert!(!touches(&mu("assets/atlas/codepoints.bin"), Some("python3 tools/check_atlas.py.bak"), &changed), "a word, not a substring");
        assert!(!touches(&mu("assets/atlas/codepoints.bin"), Some("clippy"), &changed), "a built-in gate names no script");
        assert!(!touches(&mu("native/src/text.rs"), None, &set(&[], &[], &[])), "an empty change set selects nothing");
        assert!(touches(&mu("native/src/fold.rs"), None, &set(&["build.toml"], &["m"], &[])), "its declaration changed");
        assert!(touches(&mu("native/src/fold.rs"), None, &set(&["build.toml"], &[], &["g"])), "its gate's declaration changed");

        // Declarations are compared as TOML values, by name.
        let base = "[[gate]]\nname = \"g\"\ncmd = \"a\"\n[[gate]]\nname = \"h\"\n\
                    [[mutation]]\nname = \"same\"\nfile = \"f\"\n[[mutation]]\nname = \"edited\"\nfile = \"f\"\n";
        let current = "[[gate]]\nname = \"g\"\ncmd = \"b\"\n[[gate]]\nname = \"h\"\n\
                       [[mutation]]\nname = \"same\"\nfile = \"f\"\n[[mutation]]\nname = \"edited\"\nfile = \"F\"\n\
                       [[mutation]]\nname = \"new\"\nfile = \"f\"\n";
        let (mutations, gates) = declarations_changed(Some(base), current);
        assert_eq!(mutations.into_iter().collect::<Vec<_>>(), ["edited", "new"]);
        assert_eq!(gates.into_iter().collect::<Vec<_>>(), ["g"]);
        let (mutations, gates) = declarations_changed(None, current);
        assert_eq!((mutations.len(), gates.len()), (3, 2), "no base: every declaration is new");
    }

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
