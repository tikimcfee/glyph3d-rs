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
//! `test` BUILDS what it needs, because that is the iterating intent and
//! because `cargo build` does not build the Mojo dylib — a stale artifact
//! silently tests the wrong engine, which has cost this repo a day and a bogus
//! bisect. `test --frozen` is the other intent: assert everything is already
//! current and fail if it is not. A check that silently rebuilds can never tell
//! you your commit was incomplete.
//!
//! The manifest (`build.toml`) is parsed into the types below with
//! `deny_unknown_fields`, so a key the code does not know is a hard error, and
//! an unread field is a `dead_code` warning against a zero-warning gate. Both
//! directions matter: this repo shipped a manifest whose `needs` edges were
//! declared and read by nothing.

use clap::{Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod tui;

// ── the manifest, as types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) settings: Settings,
    pub(crate) artifact: BTreeMap<String, Artifact>,
    pub(crate) golden_view: Vec<GoldenView>,
    pub(crate) gate: Vec<Gate>,
    #[serde(default)]
    pub(crate) mutation: Vec<Mutation>,
}


#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    test_floor: u32,
}

/// How an artifact is verified is a property of its CLASS, not a per-artifact
/// choice — which is what stops "verify by rebuilding" being applied to the
/// pixel baselines, whose bytes cannot be derived from anything.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Class {
    /// Untracked build output. Only has to be CURRENT.
    Product,
    /// Tracked; verified by rebuilding and byte-comparing.
    Committed,
    /// Tracked; CANNOT be rebuilt. Verify only — re-baselining is a human act.
    Golden,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub(crate) class: Class,

    outputs: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    build: Option<String>,
    verify_cmd: Option<String>,
    verify_scratch: Option<String>,
    counts: Option<BTreeMap<String, u32>>,
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenView {
    name: String,
    cmd: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    VerifyCommitted,
    Cmd,
    Cargo,
    CargoTest,
    EngineCheck,
    GoldenVerify,
    RepoVerify,
}

/// What you changed, and therefore what is worth running.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
enum Scope {
    // `engine` (engine/*.mojo and the FFI) retired with the Mojo engine: no
    // gate carried it, so `glyph test engine` ran nothing and printed ALL
    // GATES GREEN (measured 2026-10-09). cmd_test now refuses an empty
    // selection, so a scope that loses its last gate cannot do that again.
    /// Rust code: native/, crates/, glyph/
    Rust,
    /// layout, shaders, anything that moves a pixel
    Render,
    /// fixtures, generators, vendored inputs
    Corpus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Gate {
    name: String,
    scope: Scope,
    kind: Kind,
    #[serde(default)]
    needs: Vec<String>,
    cmd: Option<String>,
    pass_line: Option<String>,
    /// engine-check: the inputs to diff. Declared, not hardcoded in the runner.
    #[serde(default)]
    targets: Vec<String>,
    /// repo-verify: the wrap modes `cmd`'s {mode} is substituted with.
    #[serde(default)]
    modes: Vec<String>,
    compare: Option<String>,
    blind_to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    name: String,
    gate: String,
    why: Option<String>,
    file: String,
    op: String,
    expect: String,
    arg: Option<String>,
    find: Option<String>,
    #[serde(rename = "with")]
    with_: Option<String>,
    rebuild: Option<String>,
}

// ── paths and process ────────────────────────────────────────────────────

pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn native() -> PathBuf {
    root().join("native")
}
fn stamps() -> PathBuf {
    root().join("target/.glyph-stamps")
}
fn sweep() -> PathBuf {
    root().join("out/tooling-ab/sweep")
}

/// Every external command goes through here so failures look the same.
fn sh(cmd: &str, cwd: &Path) -> (bool, String) {
    let out = Command::new("bash").arg("-c").arg(cmd).current_dir(cwd).output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("could not spawn: {e}")),
    }
}

fn expand(pattern: &str) -> Vec<PathBuf> {
    let p = root().join(pattern);
    let mut v: Vec<PathBuf> = glob::glob(&p.to_string_lossy())
        .map(|g| g.filter_map(Result::ok).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn step(msg: &str) {
    println!("\n── {msg}");
}

// ── products: current, or not ────────────────────────────────────────────

/// A product's currency is the hash of its inputs' CONTENT, not their mtimes.
/// mtime is what cargo uses for the dylib edge, and it is why `cargo build`
/// happily links an engine built from different source.
fn input_digest(a: &Artifact) -> String {
    let mut h = Sha256::new();
    for pat in &a.inputs {
        for f in expand(pat) {
            h.update(f.strip_prefix(root()).unwrap_or(&f).to_string_lossy().as_bytes());
            h.update([0]);
            h.update(std::fs::read(&f).unwrap_or_default());
            h.update([0]);
        }
    }
    format!("{:x}", h.finalize())
}

fn stamp_of(name: &str) -> Option<String> {
    std::fs::read_to_string(stamps().join(format!("{name}.sha256"))).ok()
}

fn write_stamp(name: &str, digest: &str) {
    let _ = std::fs::create_dir_all(stamps());
    let _ = std::fs::write(stamps().join(format!("{name}.sha256")), digest);
}

pub(crate) fn is_current(name: &str, a: &Artifact) -> bool {
    stamp_of(name).as_deref() == Some(input_digest(a).as_str())
        && a.outputs.iter().all(|o| root().join(o).exists())
}


/// `build` achieves currency; `--frozen` only asserts it. Keeping those apart
/// is the whole reason `test` no longer has a gate that quietly rebuilds.
fn ensure_products(m: &Manifest, frozen: bool) -> bool {
    let mut ok = true;
    for (name, a) in m.artifact.iter().filter(|(_, a)| a.class == Class::Product) {
        if is_current(name, a) {
            println!("PASS  {name} current (input hash unchanged)");
            continue;
        }
        if frozen {
            println!("FAIL  {name} is stale or unbuilt — run `glyph build`.");
            println!("      --frozen asserts currency rather than achieving it, so that a");
            println!("      commit which forgot to rebuild fails here instead of passing.");
            ok = false;
            continue;
        }
        let Some(build) = &a.build else {
            println!("FAIL  {name} has no build command");
            ok = false;
            continue;
        };
        let (good, out) = sh(build, &root());
        if !good {
            println!("FAIL  {name} build errored — every check below would test the wrong artifact");
            for l in out.lines().rev().take(6).collect::<Vec<_>>().iter().rev() {
                println!("      {l}");
            }
            ok = false;
            continue;
        }
        write_stamp(name, &input_digest(a));
        println!("PASS  {name} rebuilt");
    }
    ok
}

// ── committed artifacts ──────────────────────────────────────────────────

fn verify_committed(m: &Manifest) -> bool {
    let mut ok = true;
    for (name, a) in m.artifact.iter().filter(|(_, a)| a.class == Class::Committed) {
        if let Some(c) = &a.verify_cmd {
            let (good, out) = sh(c, &root());
            let last = out.lines().last().unwrap_or("").to_string();
            if good {
                println!("PASS  {name} — {last}");
            } else {
                println!("FAIL  {name} — {last}");
                ok = false;
            }
        } else if a.counts.is_some() {
            ok &= verify_corpus(name, a);
        } else if let Some(c) = &a.verify_scratch {
            ok &= verify_scratch(name, a, c);
        } else {
            println!("FAIL  {name} declares no way to verify it");
            ok = false;
        }
    }
    ok
}

/// Rebuild into a scratch dir and byte-compare. The committed files are never
/// touched, which is the difference between this and the gate it replaced.
fn verify_scratch(name: &str, a: &Artifact, cmd: &str) -> bool {
    let scratch = std::env::temp_dir().join(format!("glyph-verify-{name}"));
    let _ = std::fs::remove_dir_all(&scratch);
    if std::fs::create_dir_all(&scratch).is_err() {
        println!("FAIL  {name} — could not make a scratch dir");
        return false;
    }
    let (good, out) = sh(&cmd.replace("{scratch}", &scratch.to_string_lossy()), &root());
    if !good {
        println!("FAIL  {name} — rebuild errored: {}", out.lines().last().unwrap_or(""));
        return false;
    }
    for o in &a.outputs {
        let file = Path::new(o).file_name().unwrap();
        let (built, committed) = (scratch.join(file), root().join(o));
        if std::fs::read(&built).ok() != std::fs::read(&committed).ok() {
            println!("FAIL  {name} — {} differs from the committed asset", file.to_string_lossy());
            return false;
        }
    }
    println!("PASS  {name} — {} rebuilt BYTE-IDENTICAL to scratch", a.outputs.len());
    true
}

/// The corpus is regenerated in a COPY. The counts it is checked against are
/// declared in the manifest, never counted off the tree being checked — a
/// deleted case would otherwise lower both sides and stay green.
fn verify_corpus(name: &str, a: &Artifact) -> bool {
    let scratch = std::env::temp_dir().join("glyph-verify-corpus");
    let _ = std::fs::remove_dir_all(&scratch);
    let (good, out) = sh(
        &format!(
            "rm -rf {0} && mkdir -p {0} && cp -R engine/fixtures {0}/fixtures && cp engine/glyph_schema.mjs {0}/ \
             && rm -f {0}/fixtures/*.pipe.bin {0}/fixtures/*.bake.bin \
             && cd {0}/fixtures && node gen.mjs >/dev/null && node gen-bake.mjs >/dev/null",
            scratch.to_string_lossy()
        ),
        &root(),
    );
    if !good {
        println!("FAIL  {name} — a fixture generator errored; the corpus is not rebuildable");
        println!("      {}", out.lines().last().unwrap_or(""));
        return false;
    }
    let counts = a.counts.as_ref().unwrap();
    for (ext, want) in counts {
        let built: Vec<_> = glob::glob(&format!("{}/fixtures/*.{ext}.bin", scratch.to_string_lossy()))
            .map(|g| g.filter_map(Result::ok).collect())
            .unwrap_or_default();
        if built.len() as u32 != *want {
            println!(
                "FAIL  {name} — regenerated {} .{ext}.bin, build.toml declares {want} \
                 (a generator's case list changed; update the count deliberately)",
                built.len()
            );
            return false;
        }
        for b in &built {
            let committed = root().join("engine/fixtures").join(b.file_name().unwrap());
            if std::fs::read(b).ok() != std::fs::read(&committed).ok() {
                println!(
                    "FAIL  {name} — {} differs from the committed fixture",
                    b.file_name().unwrap().to_string_lossy()
                );
                return false;
            }
        }
    }
    let total: u32 = counts.values().sum();
    println!(
        "PASS  {name} — {total} fixtures ({}) regenerated BYTE-IDENTICAL in scratch; \
         counts declared in build.toml, not ls-derived",
        counts.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(" + ")
    );
    true
}

/// Goldens are re-rendered and compared. There is no build path, by design:
/// re-baselining is a human decision, and the tool refuses to make it.
/// Render every golden view into the sweep dir. Shared by the gate and by
/// `drift`, so both compare the same fresh frames.
fn render_views(m: &Manifest) -> bool {
    let _ = std::fs::create_dir_all(sweep());
    let mut ok = true;
    for v in &m.golden_view {
        let shot = sweep().join(format!("{}.png", v.name));
        let (good, out) = sh(
            &format!(
                "../target/release/glyph3d-native {} --screenshot {}",
                v.cmd,
                shot.to_string_lossy()
            ),
            &native(),
        );
        if !good {
            println!("FAIL  {} render errored — {}", v.name, out.lines().last().unwrap_or(""));
            ok = false;
        }
    }
    ok
}

/// The directory a golden set lives in for `key`, from the artifact's own
/// declared outputs rather than a second spelling of the path here.
fn golden_dir(a: &Artifact, key: &str) -> PathBuf {
    let first = a.outputs.first().expect("a golden artifact declares outputs");
    root().join(first.replace("{gpu}", key)).parent().unwrap().to_path_buf()
}

/// Provenance: the record beside a golden set versus the adapter that is
/// about to be compared against it. A NOTE, never a failure — a driver
/// update that moves nothing is fine, and one that moves a pixel is then
/// explained by the line that differs.
fn adapter_note(key: &str, dir: &Path) {
    let rec = dir.join("ADAPTER.txt");
    let Some(live) = gpu_profile() else { return };
    match std::fs::read_to_string(&rec) {
        Err(_) => {
            println!("NOTE  no ADAPTER.txt recorded for the {key} golden set. On the machine the");
            println!("      baselines came from:  target/release/glyph3d-native --gpu-profile > {}", rec.strip_prefix(root()).unwrap_or(&rec).display());
        }
        Ok(recorded) if recorded == live => {}
        Ok(recorded) => {
            println!("NOTE  the {key} baselines were recorded on different hardware or driver than this:");
            for (r, l) in recorded.lines().zip(live.lines()).filter(|(r, l)| r != l) {
                println!("      recorded  {r}");
                println!("      live      {l}");
            }
        }
    }
}

fn verify_golden(m: &Manifest) -> bool {
    let a = m.artifact.values().find(|a| a.class == Class::Golden).unwrap();
    let Some(key) = gpu_key() else {
        println!("FAIL  pixel-ab — the renderer did not answer --gpu-key, so this host's golden set is unknown");
        return false;
    };
    let dir = golden_dir(a, key);
    if !render_views(m) {
        return false;
    }
    let mut ok = true;
    let mut missing: Vec<&str> = Vec::new();
    for v in &m.golden_view {
        let shot = sweep().join(format!("{}.png", v.name));
        let baseline = a
            .outputs
            .iter()
            .find(|o| o.ends_with(&format!("/{}.png", v.name)))
            .and_then(|o| resolve_gpu(o))
            .map(|o| root().join(o));
        match baseline {
            Some(b) if !b.exists() => {
                println!("FAIL  {}.png — no {key} baseline", v.name);
                missing.push(&v.name);
                ok = false;
            }
            Some(b) if std::fs::read(&b).ok() == std::fs::read(&shot).ok() => {
                println!("PASS  {}.png BYTE-EQUAL ({key})", v.name)
            }
            Some(_) => {
                println!("FAIL  {}.png diverges from baseline — the renderer changed.", v.name);
                println!("      If that was intended, re-baseline by hand and say so in the commit.");
                ok = false;
            }
            None => {
                println!("FAIL  {} has no declared baseline", v.name);
                ok = false;
            }
        }
    }
    if !missing.is_empty() {
        let rel = |p: &Path| p.strip_prefix(root()).unwrap_or(p).display().to_string();
        println!("      This host's key is {key} and no golden set exists for it. The frames it");
        println!("      would be compared against are in {}/. Adopting them is a human act —", rel(&sweep()));
        println!("      look at them, and run `glyph drift` to see how far they sit from the");
        println!("      other sets — then:");
        println!("        mkdir -p {}", rel(&dir));
        println!(
            "        cp {}/{{{}}}.png {}/",
            rel(&sweep()),
            m.golden_view.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(","),
            rel(&dir)
        );
        println!("        target/release/glyph3d-native --gpu-profile > {}/ADAPTER.txt", rel(&dir));
        println!("      and say why in the commit.");
    } else {
        adapter_note(key, &dir);
    }
    ok
}

// ── drift: this host against every other rasterizer's golden set ─────────

/// Decode a PNG to RGBA8. Only what the offscreen path writes (8-bit RGB or
/// RGBA) is accepted; anything else is a wrong file, not a format to widen for.
fn png_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let size = reader.output_buffer_size().ok_or_else(|| format!("{}: size overflow", path.display()))?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{}: {:?} bit depth, expected 8", path.display(), info.bit_depth));
    }
    let px = (info.width * info.height) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => {
            buf[..px * 3].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        }
        other => return Err(format!("{}: {other:?} colour type, expected RGB/RGBA", path.display())),
    };
    Ok((info.width, info.height, rgba))
}

/// How two frames of the same size differ, in the terms that separate
/// rasterizer edge noise from a layout change.
struct Drift {
    differing: usize,
    max_delta: u8,
    high: usize,
    /// High-delta pixels with no high-delta 8-neighbour: single edge flips.
    isolated: usize,
    /// High-delta pixels with three or more: the shape of something that moved.
    clustered: usize,
}

const DRIFT_HIGH: u8 = 16;

fn drift_stats(w: u32, h: u32, a: &[u8], b: &[u8]) -> Drift {
    let (w, h) = (w as usize, h as usize);
    let delta: Vec<u8> = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(p, q)| p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0))
        .collect();
    let high_at = |x: isize, y: isize| {
        x >= 0 && y >= 0 && x < w as isize && y < h as isize && delta[y as usize * w + x as usize] >= DRIFT_HIGH
    };
    let mut d = Drift { differing: 0, max_delta: 0, high: 0, isolated: 0, clustered: 0 };
    for y in 0..h {
        for x in 0..w {
            let v = delta[y * w + x];
            if v == 0 {
                continue;
            }
            d.differing += 1;
            d.max_delta = d.max_delta.max(v);
            if v >= DRIFT_HIGH {
                d.high += 1;
                let (x, y) = (x as isize, y as isize);
                let n = [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)]
                    .iter()
                    .filter(|(dx, dy)| high_at(x + dx, y + dy))
                    .count();
                if n == 0 {
                    d.isolated += 1;
                } else if n >= 3 {
                    d.clustered += 1;
                }
            }
        }
    }
    d
}

/// An instrument, not a gate: render this host's views and lay them against
/// every OTHER rasterizer's golden set. Prints where the two disagree and in
/// what shape; exits nonzero only when it could not compare. Matching across
/// vendors is not a goal — this exists so that the day a cross-vendor
/// difference stops looking like isolated edge flips, someone sees it.
fn cmd_drift(m: &Manifest) -> bool {
    let a = m.artifact.values().find(|a| a.class == Class::Golden).unwrap();
    let Some(key) = gpu_key() else {
        println!("FAIL  the renderer did not answer --gpu-key; run `glyph build` first");
        return false;
    };
    step(&format!("drift: this host ({key}) against the other golden sets"));
    if !render_views(m) {
        return false;
    }
    let base = golden_dir(a, key).parent().unwrap().to_path_buf();
    let mut others: Vec<String> = std::fs::read_dir(&base)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != key)
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    if others.is_empty() {
        println!("NOTE  no other golden set under {} to compare against", base.display());
        return true;
    }
    let mut ok = true;
    for other in &others {
        let dir = base.join(other);
        let device = std::fs::read_to_string(dir.join("ADAPTER.txt"))
            .ok()
            .and_then(|t| t.lines().find(|l| l.starts_with("device:")).map(|l| l.to_string()))
            .unwrap_or_else(|| "device: (no ADAPTER.txt recorded)".to_string());
        println!("\n{other}  —  {device}");
        println!("  {:<10} {:>9} {:>7} {:>5} {:>7} {:>9} {:>9}", "view", "differing", "%", "max", ">=16", "isolated", "clustered");
        let mut clustered_total = 0usize;
        for v in &m.golden_view {
            let mine = sweep().join(format!("{}.png", v.name));
            let theirs = dir.join(format!("{}.png", v.name));
            if !theirs.exists() {
                println!("  {:<10} (no {other} baseline)", v.name);
                continue;
            }
            let (pa, pb) = match (png_rgba(&mine), png_rgba(&theirs)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    println!("  {:<10} FAIL {e}", v.name);
                    ok = false;
                    continue;
                }
            };
            if (pa.0, pa.1) != (pb.0, pb.1) {
                println!("  {:<10} FAIL size {}x{} vs {}x{}", v.name, pa.0, pa.1, pb.0, pb.1);
                ok = false;
                continue;
            }
            let d = drift_stats(pa.0, pa.1, &pa.2, &pb.2);
            let total = (pa.0 * pa.1) as f64;
            println!(
                "  {:<10} {:>9} {:>6.2}% {:>5} {:>7} {:>9} {:>9}",
                v.name, d.differing, 100.0 * d.differing as f64 / total, d.max_delta, d.high, d.isolated, d.clustered
            );
            clustered_total += d.clustered;
        }
        if clustered_total == 0 {
            println!("  no clustered high-delta pixels: the difference has the shape of rasterizer edge noise");
        } else {
            println!("  {clustered_total} clustered high-delta pixels: something has a shape — look at the frames");
        }
    }
    ok
}

// ── the gates ────────────────────────────────────────────────────────────

fn warn_count(out: &str) -> usize {
    out.lines().filter(|l| l.starts_with("warning") && !l.contains("generated")).count()
}

fn gate_cargo(g: &Gate) -> bool {
    let cmd = match g.cmd.as_deref() {
        Some("build") => "cargo build --release",
        Some("clippy") => "cargo clippy --release",
        // Doc links are the one rename hazard nothing else in this battery can
        // see: `[`Engine::records`]` kept pointing at a method that had been
        // renamed to `read_back`, through a full green run, because rustdoc is
        // never invoked. Found by an audit 2026-09-07, not by a check.
        // `--no-deps` keeps it to this crate's own prose.
        Some("doc") => "cargo doc -p glyph3d-native --no-deps",
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
        // Name the failing tests — the engine-check gate's failure path
        // carries the same note: printing only the summary discards WHY it
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

fn gate_engine_check(g: &Gate) -> bool {
    let mut ok = true;
    for target in &g.targets {
        let (_, out) = sh(&format!("../target/release/glyph3d-native --engine-check {target}"), &native());
        let lines: Vec<&str> = out.lines().collect();
        let hit = lines.iter().find(|l| l.contains("engine-check PASS")).copied();
        println!("{}", hit.unwrap_or_else(|| lines.last().copied().unwrap_or("")));
        if hit.is_some() {
            println!("PASS  engine-check ({target})");
        } else {
            // The record diff is the interesting part on failure; printing only
            // the summary discarded WHY it reddened before anything could read it.
            for l in lines.iter().rev().skip(1).take(10).collect::<Vec<_>>().into_iter().rev() {
                println!("      {l}");
            }
            println!("FAIL  engine-check ({target})");
            ok = false;
        }
    }
    ok
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

fn run_gate(g: &Gate, m: &Manifest) -> bool {
    if !needs_met(g, m) {
        return false;
    }
    match g.kind {
        Kind::VerifyCommitted => verify_committed(m),
        Kind::Cmd => gate_cmd(g),
        Kind::Cargo => gate_cargo(g),
        Kind::CargoTest => gate_cargo_test(m),
        Kind::EngineCheck => gate_engine_check(g),
        Kind::GoldenVerify => verify_golden(m),
        Kind::RepoVerify => gate_repo_verify(g),
    }
}

// ── validation (see `glyph validate`) ────────────────────────────────────

/// Which artifacts a gate makes usable by the gates after it. Derived from
/// `kind` rather than declared: a `provides` field would be one more thing that
/// can be written and never read, which is the defect this validation exists
/// to remove.
fn provides(kind: Kind, m: &Manifest) -> Vec<&str> {
    let of = |c: Class| {
        m.artifact.iter().filter(|(_, a)| a.class == c).map(|(n, _)| n.as_str()).collect::<Vec<_>>()
    };
    match kind {
        Kind::VerifyCommitted => of(Class::Committed),
        Kind::GoldenVerify => of(Class::Golden),
        // Products are made current by `build`, which runs before any gate.
        _ => vec![],
    }
}

fn validate(m: &Manifest) -> Vec<String> {
    let mut p = Vec::new();
    let known: BTreeSet<&str> = m.artifact.keys().map(|s| s.as_str()).collect();
    let gates: BTreeSet<&str> = m.gate.iter().map(|g| g.name.as_str()).collect();

    for (name, a) in &m.artifact {
        if a.class == Class::Golden {
            for o in a.outputs.iter().filter(|o| !o.contains("{gpu}")) {
                p.push(format!(
                    "golden {name} output {o} is not keyed by hardware ({{gpu}}); a pixel baseline \
                     proves the renderer only on the rasterizer that made it"
                ));
            }
        }
        match a.class {
            Class::Golden if a.build.is_some() => p.push(format!(
                "artifact {name} is golden but declares a build command; a golden cannot be \
                 derived and must never be regenerated by the tool"
            )),
            Class::Committed => {
                if a.verify_cmd.is_none() && a.verify_scratch.is_none() && a.counts.is_none() {
                    p.push(format!("artifact {name} is committed but declares no way to verify it"));
                }
                if a.build.is_none() {
                    p.push(format!("artifact {name} is committed but has no build command"));
                }
            }
            Class::Product => {
                if a.build.is_none() {
                    p.push(format!("artifact {name} is a product with no build command"));
                }
                if a.inputs.is_empty() {
                    p.push(format!(
                        "artifact {name} is a product with no inputs; its currency stamp would \
                         hash nothing and always compare equal"
                    ));
                }
                // A non-empty input list can still hash nothing. `input_digest`
                // filters to FILES, and glob's `**` matches DIRECTORIES, so
                // `native/src/**` contributed zero bytes — from build.toml's
                // first commit (598205b) until 2026-09-10, the renderer's
                // stamp never saw a line of renderer source, and `--frozen`
                // greened on a stale binary. The empty-list check above was
                // written for this exact failure and could not see it, because
                // the list was three patterns long.
                //
                // A LITERAL path that names no file is the same failure without
                // a glob: `native/Cargo.lock` outlived the workspace move by a
                // month (2026-09-06 to 2026-10-09) because this scan once looked
                // only at patterns containing `*`. Every input is scanned now.
                for pat in &a.inputs {
                    if expand(pat).is_empty() {
                        p.push(format!(
                            "artifact {name} input pattern {pat} matches no file; the currency \
                             stamp ignores everything it was meant to cover"
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Products are current before any gate runs, so they are available from the
    // start. A gate may satisfy its own needs — `needs` carries both "someone
    // before me must make this ready" and "this is what I operate on".
    let mut avail: BTreeSet<&str> =
        m.artifact.iter().filter(|(_, a)| a.class == Class::Product).map(|(n, _)| n.as_str()).collect();
    for g in &m.gate {
        for pr in provides(g.kind, m) {
            avail.insert(pr);
        }
        for n in &g.needs {
            if !known.contains(n.as_str()) {
                p.push(format!("gate {} needs '{n}', which is not a declared artifact", g.name));
            } else if !avail.contains(n.as_str()) {
                p.push(format!(
                    "gate {} needs '{n}' but nothing before it provides that — the declared \
                     order does not satisfy the declared dependencies",
                    g.name
                ));
            }
        }
        if g.kind == Kind::EngineCheck && g.targets.is_empty() {
            p.push(format!(
                "gate {} is kind=engine-check but declares no targets; it would diff \
                 nothing and report success",
                g.name
            ));
        }
        if g.kind == Kind::RepoVerify && (g.cmd.is_none() || g.modes.is_empty()) {
            p.push(format!(
                "gate {} is kind=repo-verify but declares no cmd/modes; it would run \
                 nothing and report success",
                g.name
            ));
        }
        if g.kind == Kind::Cmd && g.cmd.is_none() {
            p.push(format!("gate {} is kind=cmd but declares no cmd", g.name));
        }
        if g.pass_line.is_some() && g.cmd.is_none() {
            p.push(format!("gate {} declares pass_line but has no cmd to match it against", g.name));
        }
    }

    if let Some(golden) = m.artifact.values().find(|a| a.class == Class::Golden) {
        let stems: BTreeSet<String> = golden
            .outputs
            .iter()
            .filter_map(|o| Path::new(o).file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        let views: BTreeSet<String> = m.golden_view.iter().map(|v| v.name.clone()).collect();
        for v in views.difference(&stems) {
            p.push(format!("golden_view '{v}' has no committed baseline image"));
        }
        for s in stems.difference(&views) {
            p.push(format!(
                "baseline '{s}' has no golden_view that renders it — it is compared against \
                 nothing and is not coverage"
            ));
        }
        for v in &m.golden_view {
            if v.cmd.trim().is_empty() {
                p.push(format!("golden_view '{}' has an empty render command", v.name));
            }
        }
    }

    for mu in &m.mutation {
        if !gates.contains(mu.gate.as_str()) {
            p.push(format!("mutation {} targets gate '{}', which does not exist", mu.name, mu.gate));
        }
        if !root().join(&mu.file).exists() {
            p.push(format!("mutation {} targets {}, which does not exist", mu.name, mu.file));
        }
        if mu.expect.trim().is_empty() {
            p.push(format!(
                "mutation {} declares no expected text; it would accept ANY red, including \
                 one from an unrelated cause",
                mu.name
            ));
        }
        match mu.op.as_str() {
            "replace" => {
                if mu.find.is_none() {
                    p.push(format!("mutation {} is op=replace with no find text", mu.name));
                } else if mu.find.as_deref() == mu.with_.as_deref() {
                    p.push(format!("mutation {} replaces text with itself and can never land", mu.name));
                }
            }
            "append" => {
                if mu.arg.as_deref().unwrap_or("").is_empty() {
                    p.push(format!("mutation {} is op=append with nothing to append", mu.name));
                }
            }
            "remove" => {}
            other => p.push(format!("mutation {} has unknown op '{other}'", mu.name)),
        }
        if mu.why.as_deref().unwrap_or("").trim().is_empty() {
            p.push(format!(
                "mutation {} has no `why`; a mutation whose motivating defect is unstated \
                 cannot be judged when it later fails",
                mu.name
            ));
        }
        if mu.rebuild.as_deref().map(str::trim) == Some("") {
            p.push(format!("mutation {} has an empty rebuild command", mu.name));
        }
    }
    p
}


// ── prove: break each check, require it to notice ────────────────────────
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
fn self_exe() -> &'static Path {
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
            if !text.contains(find) {
                return Err(format!("find-text absent from {}; mutation cannot land", mu.file));
            }
            let with = mu.with_.as_deref().unwrap_or_default();
            std::fs::write(&f, text.replacen(find, with, 1)).map_err(|e| e.to_string())?;
        }
        other => return Err(format!("unknown op {other}")),
    }
    if std::fs::read(&f).unwrap_or_default() == before {
        return Err(format!("mutation did not change {}", mu.file));
    }
    Ok(before)
}

fn cmd_prove(
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
                println!("      wrong binary. Run: pixi run build-engine");
                println!("      {}", out.lines().last().unwrap_or(""));
                return false;
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

// ── CLI ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "glyph", about = "Build, test and run glyph3d-native.", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring the runnable binary and the engine dylib up to date.
    Build,
    /// Run the checks. No scope runs all of them.
    Test {
        /// Only what this scope covers: rust, render, corpus.
        scope: Option<Scope>,
        /// Assert everything is already current instead of building it.
        /// Use this to validate a commit: if something is stale, that IS the finding.
        #[arg(long)]
        frozen: bool,
    },
    /// Launch the renderer. Arguments are passed through.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Prove the checks can fail: apply each declared mutation, require its
    /// gate to redden for its stated reason, restore.
    Prove {
        /// Only mutations targeting this gate.
        #[arg(long)]
        gate: Option<String>,
        /// Only these mutations (repeatable). The iteration form: prove the
        /// mutations YOUR change added or moved, not the whole manifest.
        #[arg(long = "mutation")]
        mutations: Vec<String>,
        /// Only mutations whose target file differs from HEAD — "prove what
        /// I touched". Combines with --gate/--mutation by intersection.
        #[arg(long)]
        changed: bool,
    },
    /// Run a single check by name. Used by `prove`, and useful on its own.
    Gate { name: String },
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
    /// Launch the interactive terminal UI (mission-control launcher).
    Tui,
}

/// The engine shared library's extension on THIS host: `dylib` on macOS, `so`
/// on Linux. build.toml names it `native/libglyph_engine.{dylib}` and the
/// token is resolved here, once, so every path the runner stats or hashes is
/// the real file. Same idiom as `{scratch}` and `{mode}`.
fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// This host's golden-set key (`<backend>-<vendor>`), asked of the renderer
/// ONCE. `None` when the renderer is not built or refused to answer — a
/// caller that needs it says which. Not resolved at manifest load like
/// `{dylib}`: the answer needs the product that `build` is about to make.
pub(crate) fn gpu_key() -> Option<&'static str> {
    static KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let bin = root().join("target/release/glyph3d-native");
        if !bin.exists() {
            return None;
        }
        let out = Command::new(&bin).arg("--gpu-key").current_dir(native()).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let safe = !s.is_empty()
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        (out.status.success() && safe).then_some(s)
    })
    .as_deref()
}

/// The live hardware record (`--gpu-profile`), for the provenance NOTE.
pub(crate) fn gpu_profile() -> Option<&'static str> {
    static TEXT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let bin = root().join("target/release/glyph3d-native");
        let out = Command::new(&bin).arg("--gpu-profile").current_dir(native()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .as_deref()
}


/// A manifest path with `{gpu}` filled in for this host, or None if the key
/// is unknown. Paths without the token pass through unchanged.
fn resolve_gpu(path: &str) -> Option<String> {
    if path.contains("{gpu}") {
        Some(path.replace("{gpu}", gpu_key()?))
    } else {
        Some(path.to_string())
    }
}

fn load() -> Result<Manifest, String> {
    let p = root().join("build.toml");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut m: Manifest = toml::from_str(&text)
        .map_err(|e| format!("build.toml is not valid against the schema:\n{e}"))?;
    for a in m.artifact.values_mut() {
        for p in a.outputs.iter_mut().chain(a.inputs.iter_mut()) {
            *p = p.replace("{dylib}", dylib_ext());
        }
    }
    Ok(m)
}

fn cmd_test(m: &Manifest, scope: Option<Scope>, frozen: bool) -> bool {
    step(if frozen { "products: asserting currency (--frozen)" } else { "products" });
    let mut ok = ensure_products(m, frozen);
    if !ok && frozen {
        // Everything below would test an artifact we just said is wrong.
        println!("\nCHECK: FAILURES — see above");
        return false;
    }
    let selected: Vec<&Gate> = m.gate.iter().filter(|g| scope.is_none_or(|s| g.scope == s)).collect();
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
    for g in &selected {
        step(&format!("{} — {}", g.name, g.compare.as_deref().unwrap_or("")));
        ok &= run_gate(g, m);
    }
    let (ok, verdict) = test_verdict(selected.len(), m.gate.len(), scope, ok);
    println!();
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
fn test_verdict(selected: usize, total: usize, scope: Option<Scope>, ok: bool) -> (bool, String) {
    let which = match scope {
        Some(s) => format!("{selected} of {total} gates ran, scope {}", scope_name(s)),
        None => format!("{selected} of {total} gates ran"),
    };
    if selected == 0 {
        return (false, format!("CHECK-ALL: NOTHING RAN — {which}; a run that checks nothing is not a pass"));
    }
    if ok {
        (true, format!("CHECK-ALL: ALL GATES GREEN — {which}"))
    } else {
        (false, format!("CHECK-ALL: FAILURES — see above ({which})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_gate_selection_is_refused() {
        let (ok, line) = test_verdict(0, 9, Some(Scope::Corpus), true);
        assert!(!ok, "a run that selected no gate must not pass");
        assert!(line.starts_with("CHECK-ALL: NOTHING RAN"), "{line}");
        assert!(line.contains("0 of 9 gates ran, scope corpus"), "{line}");
    }

    #[test]
    fn verdict_reports_how_many_gates_ran() {
        assert_eq!(
            test_verdict(4, 9, Some(Scope::Rust), true),
            (true, "CHECK-ALL: ALL GATES GREEN — 4 of 9 gates ran, scope rust".to_string())
        );
        assert_eq!(
            test_verdict(9, 9, None, false),
            (false, "CHECK-ALL: FAILURES — see above (9 of 9 gates ran)".to_string())
        );
    }
}

fn main() -> ExitCode {
    // Before anything can rebuild us out from under ourselves; see self_exe.
    let _ = self_exe();
    let cli = Cli::parse();


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
        Cmd::Test { scope, frozen } => cmd_test(&m, scope, frozen),
        Cmd::Run { args } => {
            // Runs in YOUR directory, not native/. A file argument means what
            // it says relative to where you typed it — anything else would make
            // `--render-file main.rs` from your own project silently open
            // native/main.rs. The checks cd to native/ because they pass
            // native-relative fixture paths on purpose; that is their business,
            // not yours.
            //
            // stdio is inherited rather than captured: this launches a windowed
            // app, and buffering its output until the window closes is useless.
            let cwd = std::env::current_dir().unwrap_or_else(|_| root());
            let exe = root().join("target/release/glyph3d-native");
            if !exe.exists() {
                println!("FAIL  {} does not exist — run `cargo glyph build`.", exe.display());
                return ExitCode::from(1);
            }
            Command::new(exe)
                .args(&args)
                .current_dir(cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        Cmd::Prove { gate, mutations, changed } => {
            cmd_prove(&m, gate.as_deref(), &mutations, changed)
        }
        Cmd::Drift => cmd_drift(&m),
        Cmd::Tui => tui::run(&m),
        Cmd::Gate { name } => match m.gate.iter().find(|g| g.name == name) {
            Some(g) => run_gate(g, &m),
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
                println!("  {}  [{:?}]", g.name, g.scope);
                if let Some(c) = &g.compare {
                    println!("    compares : {c}");
                }
                if let Some(b) = &g.blind_to {
                    println!("    blind to : {b}");
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
