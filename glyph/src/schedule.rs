//! `glyph test`'s gates, run at once where their declarations allow
//! (2026-10-26). Until then they ran one after another: about 95 s with
//! nothing changed and 111 s after a one-line edit, most of it gates that
//! wait on nothing but a CPU or the GPU.
//!
//! Each gate runs as a child `glyph gate <name>`, as prove runs one, so its
//! output is its own; the blocks are printed in manifest order as each one
//! finishes, so a run reads the same as a serial one. A gate starts when
//! every earlier gate that provides an artifact it `needs` has finished (the
//! order `validate` holds the manifest to), and never beside a gate either
//! one declares `apart_from` (a measured contention: the GPU gates beside
//! cargo-test's GPU binaries). Nothing else is ordered here: cargo's lock
//! orders the cargo gates that share `target/`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use crate::manifest::{provides, Gate, Manifest};
use crate::paths::{keep, root, scratch, step};
use crate::prove::self_exe;

/// What one gate did.
pub(crate) struct Ran {
    pub(crate) ok: bool,
    pub(crate) secs: f64,
}

/// The runner, at a path no build moves. The workspace build and the `-p
/// glyph` build the alias makes link different `glyph` binaries and re-point
/// `target/release/glyph` at whichever ran last (prove's `self_exe` has the
/// history); with gates running at once, cargo-build can do that while a
/// child is being spawned. A hard link keeps this binary's inode.
fn stable_runner() -> PathBuf {
    let link = scratch().join(format!("runner-{}", std::process::id()));
    let _ = std::fs::create_dir_all(scratch());
    let _ = std::fs::remove_file(&link);
    if std::fs::hard_link(self_exe(), &link).is_ok() || std::fs::copy(self_exe(), &link).is_ok() {
        link
    } else {
        self_exe().to_path_buf()
    }
}

fn run_child(runner: &Path, name: &str, inner: bool) -> (bool, String) {
    let mut cmd = Command::new(runner);
    cmd.arg("gate").arg(name).current_dir(root());
    if inner {
        cmd.arg("--inner");
    }
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("FAIL  could not run gate {name}: {e}")),
    }
}

/// For each gate, the earlier gates it waits for: those providing an
/// artifact it needs.
fn waits_for(m: &Manifest, gates: &[&Gate]) -> Vec<Vec<usize>> {
    gates
        .iter()
        .enumerate()
        .map(|(i, g)| {
            (0..i)
                .filter(|&j| provides(gates[j].kind, m).iter().any(|a| g.needs.iter().any(|n| n == a)))
                .collect()
        })
        .collect()
}

/// Whether `a` and `b` may run at the same time: neither declares itself
/// apart from the other.
fn may_overlap(a: &Gate, b: &Gate) -> bool {
    !a.apart_from.contains(&b.name) && !b.apart_from.contains(&a.name)
}

/// Run `gates`, printing each one's block in order; one `Ran` per gate.
/// `inner` passes `--inner` to each child (a gate's own merge-tier parts).
pub(crate) fn run_gates(m: &Manifest, gates: &[&Gate], inner: bool) -> Vec<Ran> {
    let runner = stable_runner();
    let waits = waits_for(m, gates);
    let mut started = vec![false; gates.len()];
    let mut done: Vec<Option<(bool, String, f64)>> = (0..gates.len()).map(|_| None).collect();
    let mut running = vec![false; gates.len()];
    let mut printed = 0;
    let (tx, rx) = mpsc::channel::<(usize, bool, String, f64)>();
    std::thread::scope(|s| {
        while printed < gates.len() {
            for i in 0..gates.len() {
                let ready = !started[i] && waits[i].iter().all(|&j| done[j].is_some());
                let apart = (0..gates.len()).any(|j| running[j] && !may_overlap(gates[i], gates[j]));
                if !ready || apart {
                    continue;
                }
                started[i] = true;
                running[i] = true;
                let (tx, runner, name) = (tx.clone(), &runner, gates[i].name.as_str());
                s.spawn(move || {
                    let t = Instant::now();
                    let (ok, out) = run_child(runner, name, inner);
                    let _ = tx.send((i, ok, out, t.elapsed().as_secs_f64()));
                });
            }
            let Ok((i, ok, out, secs)) = rx.recv() else { break };
            running[i] = false;
            // The child's whole output, beside whatever its own gate kept
            // of the commands under it.
            let out = if ok { out } else { format!("{out}      full output kept: {}\n", keep(&gates[i].name, &out)) };
            done[i] = Some((ok, out, secs));
            while printed < gates.len() {
                let Some((_, out, secs)) = &done[printed] else { break };
                let g = gates[printed];
                step(&format!("{} — {}", g.name, g.compare.as_deref().unwrap_or("")));
                print!("{out}");
                if !out.ends_with('\n') && !out.is_empty() {
                    println!();
                }
                println!("      {secs:.1}s");
                printed += 1;
            }
        }
    });
    if runner != self_exe() {
        let _ = std::fs::remove_file(&runner);
    }
    done.into_iter().map(|d| d.map_or(Ran { ok: false, secs: 0.0 }, |(ok, _, secs)| Ran { ok, secs })).collect()
}

/// The TIME line: where a run's seconds went.
pub(crate) fn time_line(gates: &[&Gate], ran: &[Ran], products: f64, wall: f64) -> String {
    let summed: f64 = ran.iter().map(|r| r.secs).sum();
    let mut slowest: Vec<(&str, f64)> = gates.iter().zip(ran).map(|(g, r)| (g.name.as_str(), r.secs)).collect();
    slowest.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top: Vec<String> = slowest.iter().take(4).map(|(n, t)| format!("{n} {t:.0}s")).collect();
    format!(
        "TIME      {wall:.0}s wall: products {products:.0}s, then {} gates in {:.0}s ({summed:.0}s of gate time at once); slowest: {}",
        gates.len(),
        wall - products,
        top.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;

    /// The manifest's own waits: the fixture readers wait for the gate that
    /// verifies the committed artifacts, the pixel gate for nothing before it
    /// (its golden set is a state it reports), the cargo gates for nothing.
    #[test]
    fn gates_wait_only_for_what_provides_their_needs() {
        let m = load().expect("build.toml loads");
        let gates: Vec<&Gate> = m.gate.iter().collect();
        let waits = waits_for(&m, &gates);
        let named = |name: &str| {
            let i = gates.iter().position(|g| g.name == name).expect("a declared gate");
            waits[i].iter().map(|&j| gates[j].name.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(named("reference-port"), ["committed-artifacts"]);
        assert_eq!(named("hyper-oracle"), ["committed-artifacts"]);
        assert!(named("pixel-ab").is_empty());
        assert!(named("cargo-test").is_empty());
    }

    /// The measured contention, as declared: the GPU gates never beside
    /// cargo-test, either way round, and beside each other freely;
    /// hyper-oracle, CPU-only since the Visible tier went, beside anything.
    #[test]
    fn the_gpu_gates_stay_apart_from_cargo_test_only() {
        let m = load().expect("build.toml loads");
        let g = |name: &str| m.gate.iter().find(|g| g.name == name).expect("a declared gate");
        for name in ["pixel-ab", "pick-oracle"] {
            assert!(!may_overlap(g(name), g("cargo-test")), "{name} beside cargo-test");
            assert!(!may_overlap(g("cargo-test"), g(name)), "cargo-test beside {name}");
        }
        assert!(may_overlap(g("pixel-ab"), g("pick-oracle")));
        assert!(may_overlap(g("hyper-oracle"), g("cargo-test")));
        assert!(may_overlap(g("cargo-test"), g("cargo-clippy")));
    }
}
