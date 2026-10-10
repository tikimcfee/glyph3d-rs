//! The manifest (`build.toml`) as types, its loading, and its validation
//! (`glyph validate`).

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::golden::equivalent_tag;
use crate::paths::{expand, root};

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
pub(crate) struct Settings {
    pub(crate) test_floor: u32,
    /// Argument lists under which every golden view must render its
    /// baseline's exact bytes: each view is rendered again with each list
    /// appended and compared against the SAME baseline. For a variant that
    /// is supposed to be pixel-identical (`--field-mode derived`), so it is
    /// covered without a second golden set to adopt and keep in step.
    #[serde(default)]
    pub(crate) golden_equivalents: Vec<String>,
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

    pub(crate) outputs: Vec<String>,
    #[serde(default)]
    pub(crate) inputs: Vec<String>,
    pub(crate) build: Option<String>,
    pub(crate) verify_cmd: Option<String>,
    pub(crate) verify_scratch: Option<String>,
    pub(crate) counts: Option<BTreeMap<String, u32>>,
    pub(crate) note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoldenView {
    pub(crate) name: String,
    pub(crate) cmd: String,
    /// Golden equivalents this view is NOT held to (M4, 2026-10-10): the
    /// frame is still rendered under them and its distance from the baseline
    /// reported as a NOTE with the pixel count — a known, named difference
    /// kept in sight, never a quiet drop. Each names the equivalent (its
    /// exact argument string) and why, blind_to-style; `glyph gates` lists
    /// them under the pixel gate.
    #[serde(default)]
    pub(crate) exempt: Vec<Exemption>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exemption {
    pub(crate) equivalent: String,
    pub(crate) why: String,
}

impl GoldenView {
    /// The reason this view is exempt from `equivalent`, if it is.
    pub(crate) fn exemption(&self, equivalent: &str) -> Option<&str> {
        self.exempt.iter().find(|e| e.equivalent == equivalent).map(|e| e.why.as_str())
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Kind {
    VerifyCommitted,
    Cmd,
    Cargo,
    CargoTest,
    GoldenVerify,
    RepoVerify,
}

/// What you changed, and therefore what is worth running.
#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub(crate) enum Scope {
    // A fourth scope, `engine`, outlived its last gate: `glyph test engine`
    // ran nothing and printed ALL GATES GREEN (measured 2026-10-09). cmd_test
    // now refuses an empty selection, so a scope that loses its last gate
    // cannot do that again.
    /// Rust code: native/, crates/, glyph/
    Rust,
    /// layout, shaders, anything that moves a pixel
    Render,
    /// fixtures, generators, vendored inputs
    Corpus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Gate {
    pub(crate) name: String,
    pub(crate) scope: Scope,
    pub(crate) kind: Kind,
    #[serde(default)]
    pub(crate) needs: Vec<String>,
    pub(crate) cmd: Option<String>,
    pub(crate) pass_line: Option<String>,
    /// repo-verify: the wrap modes `cmd`'s {mode} is substituted with.
    #[serde(default)]
    pub(crate) modes: Vec<String>,
    pub(crate) compare: Option<String>,
    pub(crate) blind_to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mutation {
    pub(crate) name: String,
    pub(crate) gate: String,
    pub(crate) why: Option<String>,
    pub(crate) file: String,
    pub(crate) op: String,
    pub(crate) expect: String,
    pub(crate) arg: Option<String>,
    pub(crate) find: Option<String>,
    #[serde(rename = "with")]
    pub(crate) with_: Option<String>,
    pub(crate) rebuild: Option<String>,
}

pub(crate) fn load() -> Result<Manifest, String> {
    let p = root().join("build.toml");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    toml::from_str(&text).map_err(|e| format!("build.toml is not valid against the schema:\n{e}"))
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

pub(crate) fn validate(m: &Manifest) -> Vec<String> {
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
        let mut tags = BTreeSet::new();
        for e in &m.settings.golden_equivalents {
            if equivalent_tag(e).is_empty() || e.contains("--screenshot") {
                p.push(format!("golden equivalent '{e}' must be render arguments (and not --screenshot)"));
            }
            if !tags.insert(equivalent_tag(e)) {
                p.push(format!("golden equivalent '{e}' collides with another's file tag"));
            }
        }
        // An exemption names a declared equivalent, exactly, with a reason;
        // one that names nothing declared would exempt a view from nothing
        // and read as coverage.
        for v in &m.golden_view {
            let mut seen = BTreeSet::new();
            for e in &v.exempt {
                if !m.settings.golden_equivalents.contains(&e.equivalent) {
                    p.push(format!(
                        "golden_view '{}' is exempt from '{}', which is not a declared golden equivalent",
                        v.name, e.equivalent
                    ));
                }
                if e.why.trim().is_empty() {
                    p.push(format!("golden_view '{}' is exempt from '{}' with no why", v.name, e.equivalent));
                }
                if !seen.insert(e.equivalent.as_str()) {
                    p.push(format!("golden_view '{}' is exempt from '{}' twice", v.name, e.equivalent));
                }
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
