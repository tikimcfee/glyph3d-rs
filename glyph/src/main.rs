//! xtask — the typed half of the build/verify tooling.
//!
//! `build.toml` is currently consumed by `tools/glyph.py`, which reads it with
//! string keys. That means the manifest can declare a field nobody reads and a
//! reader can look up a field nobody declares, and neither is an error. Both
//! happened: `needs` and `compare` are declared on every gate and read by
//! nothing, so gate ordering is still line order wearing a dependency graph's
//! clothes.
//!
//! Typing closes both directions at once. The structs below are the single
//! statement of what a gate IS: `deny_unknown_fields` makes a key the code does
//! not know a hard parse error, and an unread field is a `dead_code` warning
//! against a repo that gates on zero warnings. A declaration that nothing
//! consumes stops being possible to commit rather than merely being discouraged.
//!
//! This is the read-only half — it parses, validates, and prints. Running gates
//! stays in glyph.py until each one is ported with its mutation as the
//! acceptance test.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ── the manifest, as types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    settings: Settings,
    artifact: BTreeMap<String, Artifact>,
    golden_view: Vec<GoldenView>,
    gate: Vec<Gate>,
    #[serde(default)]
    mutation: Vec<Mutation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    test_floor: u32,
}

/// How an artifact is verified is a property of its CLASS, not a per-artifact
/// choice — which is what stops "verify it by rebuilding it" from being applied
/// to the pixel baselines, whose expected bytes cannot be derived from anything.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Class {
    /// Untracked build output. Only has to be CURRENT.
    Product,
    /// Tracked; verified by rebuilding and byte-comparing.
    Committed,
    /// Tracked; CANNOT be rebuilt. Verify only — re-baselining is a human act.
    Golden,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    class: Class,
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
    Products,
    VerifyCommitted,
    Cmd,
    Cargo,
    CargoTest,
    EngineCheck,
    GoldenVerify,
    RepoVerify,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Gate {
    name: String,
    kind: Kind,
    #[serde(default)]
    needs: Vec<String>,
    cmd: Option<String>,
    pass_line: Option<String>,
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

// ── loading ──────────────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    // The crate sits at <root>/xtask; it is not a workspace member, so cargo
    // cannot hand us the repo root and we derive it from the manifest dir.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ must have a parent")
        .to_path_buf()
}

fn load() -> Result<Manifest, String> {
    let p = repo_root().join("build.toml");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    toml::from_str(&text).map_err(|e| format!("build.toml is not valid against the schema:\n{e}"))
}

// ── validation: the part that makes `needs` load-bearing ─────────────────

/// Which artifacts a gate makes usable by the gates after it.
///
/// This is derived from `kind` rather than declared, deliberately: a `provides`
/// field would be one more thing that can be written and never read, which is
/// the defect this file exists to remove. The mapping is the actual semantics —
/// the products gate is what makes products current, the committed gate is what
/// establishes committed artifacts match their sources, and the golden gate is
/// the only thing that ever looks at a golden.
fn provides(kind: Kind, m: &Manifest) -> Vec<&str> {
    let of_class = |c: Class| {
        m.artifact
            .iter()
            .filter(|(_, a)| a.class == c)
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
    };
    match kind {
        Kind::Products => of_class(Class::Product),
        Kind::VerifyCommitted => of_class(Class::Committed),
        Kind::GoldenVerify => of_class(Class::Golden),
        _ => vec![],
    }
}

fn validate(m: &Manifest) -> Vec<String> {
    let mut problems = Vec::new();
    let known: BTreeSet<&str> = m.artifact.keys().map(|s| s.as_str()).collect();
    let gate_names: BTreeSet<&str> = m.gate.iter().map(|g| g.name.as_str()).collect();

    // Every artifact must state how it is verified, or be a product.
    for (name, a) in &m.artifact {
        match a.class {
            Class::Golden => {
                if a.build.is_some() {
                    problems.push(format!(
                        "artifact {name} is golden but declares a build command; a golden \
                         cannot be derived and must never be regenerated by the tool"
                    ));
                }
            }
            Class::Committed => {
                if a.verify_cmd.is_none() && a.verify_scratch.is_none() && a.counts.is_none() {
                    problems.push(format!(
                        "artifact {name} is committed but declares no way to verify it"
                    ));
                }
                if a.build.is_none() {
                    problems.push(format!("artifact {name} is committed but has no build command"));
                }
            }
            Class::Product => {
                if a.build.is_none() {
                    problems.push(format!("artifact {name} is a product with no build command"));
                }
                if a.inputs.is_empty() {
                    problems.push(format!(
                        "artifact {name} is a product with no inputs; its currency stamp \
                         would hash nothing and always compare equal"
                    ));
                }
            }
        }
    }

    // `needs` must name real artifacts, and they must be available by the time
    // the gate runs. Availability comes from an EARLIER gate in the list.
    //
    // A gate MAY satisfy its own needs. `needs` carries two relationships that
    // reading the manifest does not distinguish and implementing it does:
    // "someone before me must make this ready" (engine-check needs a built
    // renderer) and "this is the artifact I operate on" (products-current needs
    // the dylib it is itself responsible for; pixel-ab needs the goldens it is
    // the only reader of). Both are legitimate. What is NOT legitimate, and is
    // what this check exists to catch, is needing an artifact that no gate
    // provides at all, or one that only a LATER gate provides.
    let mut available: BTreeSet<&str> = BTreeSet::new();
    for g in &m.gate {
        for p in provides(g.kind, m) {
            available.insert(p);
        }
        for n in &g.needs {
            if !known.contains(n.as_str()) {
                problems.push(format!(
                    "gate {} needs '{n}', which is not a declared artifact",
                    g.name
                ));
            } else if !available.contains(n.as_str()) {
                problems.push(format!(
                    "gate {} needs '{n}' but no earlier gate provides it — the declared \
                     order does not satisfy the declared dependencies",
                    g.name
                ));
            }
        }
    }

    // A gate's command fields must match its kind. `pass_line` without a `cmd`
    // is a string nothing can compare against.
    for g in &m.gate {
        if g.kind == Kind::Cmd && g.cmd.is_none() {
            problems.push(format!("gate {} is kind=cmd but declares no cmd", g.name));
        }
        if g.pass_line.is_some() && g.cmd.is_none() {
            problems.push(format!(
                "gate {} declares pass_line but has no cmd whose output it could match",
                g.name
            ));
        }
    }

    // Every golden view must correspond to a committed baseline, and every
    // baseline to a view. A view with no baseline renders into nothing; a
    // baseline with no view is never re-rendered and silently stops being
    // checked while still looking like coverage.
    if let Some(golden) = m.artifact.values().find(|a| a.class == Class::Golden) {
        let stems: BTreeSet<String> = golden
            .outputs
            .iter()
            .filter_map(|o| Path::new(o).file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        let views: BTreeSet<String> = m.golden_view.iter().map(|v| v.name.clone()).collect();
        for v in views.difference(&stems) {
            problems.push(format!("golden_view '{v}' has no committed baseline image"));
        }
        for s in stems.difference(&views) {
            problems.push(format!(
                "baseline '{s}' has no golden_view that renders it — it is compared \
                 against nothing and is not coverage"
            ));
        }
        for v in &m.golden_view {
            if v.cmd.trim().is_empty() {
                problems.push(format!("golden_view '{}' has an empty render command", v.name));
            }
        }
    }

    // Mutations must name a real gate, and be internally coherent: an
    // unrunnable mutation is worse than an absent one, because it is counted
    // as coverage until the day someone runs it.
    for mu in &m.mutation {
        if !gate_names.contains(mu.gate.as_str()) {
            problems.push(format!(
                "mutation {} targets gate '{}', which does not exist",
                mu.name, mu.gate
            ));
        }
        if !repo_root().join(&mu.file).exists() {
            problems.push(format!(
                "mutation {} targets {}, which does not exist",
                mu.name, mu.file
            ));
        }
        if mu.expect.trim().is_empty() {
            problems.push(format!(
                "mutation {} declares no expected text; it would accept ANY red, \
                 including one from an unrelated cause",
                mu.name
            ));
        }
        match mu.op.as_str() {
            "replace" => {
                if mu.find.is_none() {
                    problems.push(format!("mutation {} is op=replace with no find text", mu.name));
                } else if mu.find.as_deref() == mu.with_.as_deref() {
                    problems.push(format!(
                        "mutation {} replaces text with itself and can never land",
                        mu.name
                    ));
                }
            }
            "append" => {
                if mu.arg.as_deref().unwrap_or("").is_empty() {
                    problems.push(format!(
                        "mutation {} is op=append with nothing to append; it would not \
                         change the file and the harness would reject it at run time",
                        mu.name
                    ));
                }
            }
            "remove" => {}
            other => problems.push(format!("mutation {} has unknown op '{other}'", mu.name)),
        }
        if mu.why.as_deref().unwrap_or("").trim().is_empty() {
            problems.push(format!(
                "mutation {} has no `why`; a mutation whose motivating defect is \
                 unstated cannot be judged when it later fails",
                mu.name
            ));
        }
        if let Some(r) = &mu.rebuild {
            if r.trim().is_empty() {
                problems.push(format!("mutation {} has an empty rebuild command", mu.name));
            }
        }
    }

    problems
}

// ── commands ─────────────────────────────────────────────────────────────

fn cmd_graph(m: &Manifest) {
    println!("artifacts — edges flow inputs → artifact\n");
    for (name, a) in &m.artifact {
        println!("  {name}  [{:?}]", a.class);
        for o in &a.outputs {
            println!("    out:  {o}");
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
    println!("golden views — rendered to scratch and byte-compared, never rebuilt:\n");
    for v in &m.golden_view {
        println!("  {:<12} {}", v.name, v.cmd);
    }
    println!();
    println!("gate order, with what each makes available to the gates after it:\n");
    for g in &m.gate {
        let p = provides(g.kind, m);
        let needs = if g.needs.is_empty() { "—".into() } else { g.needs.join(", ") };
        let provs = if p.is_empty() { "—".into() } else { p.join(", ") };
        println!("  {:<20} needs: {:<24} provides: {}", g.name, needs, provs);
    }
}

fn cmd_gates(m: &Manifest) {
    for g in &m.gate {
        println!("  {}", g.name);
        if let Some(c) = &g.compare {
            println!("    compares : {c}");
        }
        if let Some(b) = &g.blind_to {
            println!("    blind to : {b}");
        }
    }
    println!("\n  test floor: {} (ratchet)", m.settings.test_floor);
    println!("  {} golden views, {} declared mutations", m.golden_view.len(), m.mutation.len());
}

fn cmd_validate(m: &Manifest) -> i32 {
    let problems = validate(m);
    if problems.is_empty() {
        println!("PASS  build.toml is internally consistent");
        println!("      {} artifacts, {} gates, {} mutations, all `needs` resolved and ordered",
                 m.artifact.len(), m.gate.len(), m.mutation.len());
        0
    } else {
        for p in &problems {
            println!("FAIL  {p}");
        }
        println!("\n{} problem(s) in build.toml", problems.len());
        1
    }
}

fn main() -> std::process::ExitCode {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "validate".into());
    let m = match load() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("FAIL  {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let rc = match arg.as_str() {
        "graph" => {
            cmd_graph(&m);
            0
        }
        "gates" => {
            cmd_gates(&m);
            0
        }
        "validate" => cmd_validate(&m),
        other => {
            eprintln!("unknown command {other}; try: validate | graph | gates");
            2
        }
    };
    std::process::ExitCode::from(rc as u8)
}
