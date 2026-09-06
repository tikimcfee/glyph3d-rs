#!/usr/bin/env python3
"""vendor-manifest.py — regenerate tools/vendor/{SHA256SUMS,PROVENANCE.md}.

Every file this repo copied out of the web repo, with the upstream path, the
upstream commit, and a hash. A vendored file with no recorded origin is
indistinguishable from a local invention six months later — this is the record
that keeps "where did this come from" answerable after the web repo goes quiet.

Two files (tools/vendor/hb.cjs, hb.wasm) are not upstream copies but
DERIVATIONS from a vendored ref file. They are hashed into SHA256SUMS like
everything else, and --check additionally re-runs the documented derivation
in memory: the hash catches a local edit, the re-derivation catches the ref
copy and the derived file drifting apart.

Two different questions, deliberately kept separate:

  LOCAL drift   — did someone edit a vendored copy in THIS repo?
                  `shasum -a 256 -c tools/vendor/SHA256SUMS`, no web repo needed.
                  This is a GATE (the battery's generator-reproduction step;
                  build.toml `vendor-hashes`): editing a vendored file is
                  an undeclared fork and should fail.

  UPSTREAM drift — did the web repo move on?
                  Needs the web repo present. Run this script; the "upstream
                  matches" column says. A difference is INFORMATION, NOT A
                  FAILURE: the native tree is trunk and is not obliged to track
                  the web repo. Refreshing is a decision, never a reflex.

Run:  python3 tools/vendor-manifest.py            regenerate both files
      python3 tools/vendor-manifest.py --check    verify local hashes only, exit 1 on drift
"""

import hashlib
import subprocess
import sys
import tomllib
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = Path("/Users/lugo/localdev/viz-web/glyph3d-js")  # only needed to REFRESH
BUILD_TOML = ROOT / "build.toml"

# local path in this repo -> path it came from in the web repo
VENDORED = {
    "tools/vendor/ref/tools/headlessFontChain.mjs": "tools/headlessFontChain.mjs",
    "tools/vendor/ref/packages/glyph3d-r3f/src/coreRanges.js": "packages/glyph3d-r3f/src/coreRanges.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/FontChain.js": "packages/glyph3d-core/src/shaping/FontChain.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/HarfBuzzShaper.js": "packages/glyph3d-core/src/shaping/HarfBuzzShaper.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/MonospaceShapeCache.js": "packages/glyph3d-core/src/shaping/MonospaceShapeCache.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/harfbuzz.js": "packages/glyph3d-core/src/shaping/vendor/harfbuzz.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.js": "packages/glyph3d-core/src/shaping/vendor/hb.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hbjs.js": "packages/glyph3d-core/src/shaping/vendor/hbjs.js",
    "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.wasm": "packages/glyph3d-core/src/shaping/vendor/hb.wasm",
    "tools/vendor/ref/packages/glyph3d-core/src/fonts/Cousine-Regular.ttf": "packages/glyph3d-core/src/fonts/Cousine-Regular.ttf",
    "tools/vendor/ref/packages/glyph3d-core/src/fonts/MesloLGS-NF-Mono.ttf": "packages/glyph3d-core/src/fonts/MesloLGS-NF-Mono.ttf",
    "tools/vendor/ref/packages/glyph3d-core/src/fonts/DejaVuSans.ttf": "packages/glyph3d-core/src/fonts/DejaVuSans.ttf",
    "tools/vendor/ref/app/public/slug-core/slug-core.1tstke3lync.bin": "app/public/slug-core/slug-core.1tstke3lync.bin",
    "engine/fixtures/inputs/foldGeometry.js": "packages/glyph3d-core/src/core/foldGeometry.js",
    "engine/fixtures/inputs/glyphPipelineKernels.js": "packages/glyph3d-core/src/compute/glyphPipelineKernels.js",
    "engine/fixtures/inputs/glyphPipelineReference.js": "packages/glyph3d-core/src/compute/glyphPipelineReference.js",
    "engine/fixtures/inputs/glyphPipelineScan.js": "packages/glyph3d-core/src/compute/glyphPipelineScan.js",
    "engine/fixtures/inputs/glyphBake.js": "packages/glyph3d-core/src/compute/glyphBake.js",
    "engine/fixtures/inputs/GlyphTrie.js": "packages/glyph3d-core/src/compute/GlyphTrie.js",
    "schema/glyph-identity.json": "schema/glyph-identity.json",
}


# DERIVED — committed files made FROM a vendored ref file by a stated,
# mechanical transformation, not copied from upstream. They join SHA256SUMS
# (a bare hash still catches a local edit even if the derivation below is
# ever wrong), and --check re-runs the derivation in memory: that semantic
# pin is what catches the ref copy and the derived file drifting apart.
#
# Why they exist at all: tools/export-atlas.mjs (:87-91) must require()
# HarfBuzz from Node, and Node >=22 refuses the ref hb.js — UMD with a
# trailing ESM `export default` (ERR_AMBIGUOUS_MODULE_SYNTAX). Bun, the web
# repo's runtime, accepts it, so the ref copy stays byte-verbatim.

def _verbatim(ref: bytes) -> bytes:
    return ref


def _strip_esm_default_export(ref: bytes) -> bytes:
    """ref hb.js -> hb.cjs: drop the trailing `export default` line.

    Verified by diff 2026-09-06: hb.js is 4 lines, hb.cjs is byte-identical
    except for the final line `export default createHarfBuzz;` (diff `4d3`).
    Fail loudly if the ref stops carrying that exact tail — a silent
    pass-through would re-pin garbage.
    """
    tail = b"export default createHarfBuzz;\n"
    if not ref.endswith(tail):
        raise SystemExit(
            "ERROR  ref hb.js no longer ends with `export default createHarfBuzz;` "
            "— the documented hb.cjs derivation does not hold"
        )
    return ref[: -len(tail)]


DERIVED = {
    "tools/vendor/hb.cjs": {
        "source": "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.js",
        "derive": _strip_esm_default_export,
        "how": "drop the trailing `export default createHarfBuzz;` line",
        "why": "Node >=22 cannot parse the ref's UMD + ESM-export hybrid",
    },
    "tools/vendor/hb.wasm": {
        "source": "tools/vendor/ref/packages/glyph3d-core/src/shaping/vendor/hb.wasm",
        "derive": _verbatim,
        "how": "verbatim copy",
        "why": "export-atlas.mjs loads the wasm from beside hb.cjs",
    },
}

# THE FIXTURE ORACLE IS PINNED PER FILE, NOT TO ONE UPSTREAM HEAD.
#
# `engine/fixtures/{gen,gen-bake}.mjs` reproduce the committed fixtures
# BYTE-FOR-BYTE from these files, and only from these REVISIONS of them. The
# corpus size is NOT restated here: it is declared in build.toml
# (`[artifact.fixtures] counts`) and regenerate() reads it from there —
# hardcoding it in this comment is how the prose below rotted. The
# web repo has moved on: today's `glyphPipelineReference.js` does not even
# export `FLOAT_LANES` (removed by 3da6542), so today's copy cannot run the
# generators at all, let alone reproduce their output.
#
# The pin is per file because the corpus was not generated in one sitting.
# `real-kernels.pipe.bin` embeds 90,515 bytes of `glyphPipelineKernels.js`,
# which is that file at 59a2a44 — while the copy vendored here until
# 2026-09-04 was 78,567 bytes, a LATER revision that could not have produced
# it. That copy had a recorded sha256 and passed `--check` every run: the gate
# proved the file matched its own hash and never asked whether it was the file
# the corpus came from. Byte-identical regeneration is the check that can fail.
FIXTURE_ORACLE_PINS = {
    "engine/fixtures/inputs/glyphPipelineReference.js": "70ce30e",
    "engine/fixtures/inputs/glyphPipelineScan.js": "70ce30e",
    "engine/fixtures/inputs/glyphBake.js": "70ce30e",
    "engine/fixtures/inputs/GlyphTrie.js": "70ce30e",
    "engine/fixtures/inputs/foldGeometry.js": "70ce30e",
    "engine/fixtures/inputs/glyphPipelineKernels.js": "59a2a44",
}

SUMS = ROOT / "tools/vendor/SHA256SUMS"
PROV = ROOT / "tools/vendor/PROVENANCE.md"


def sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def fixture_counts() -> tuple[int, int]:
    """The declared corpus size, from build.toml [artifact.fixtures] counts.

    DERIVED, never hand-written: the template below used to say "all 22
    committed fixtures" while the corpus was 25, and a generated artifact
    repeating a stale number is exactly the bug this file exists to prevent.
    Fail loudly rather than emit a count we could not verify.
    """
    try:
        data = tomllib.loads(BUILD_TOML.read_text())
        counts = data["artifact"]["fixtures"]["counts"]
        pipe, bake = int(counts["pipe"]), int(counts["bake"])
        if pipe <= 0 or bake <= 0:
            raise ValueError(f"non-positive counts: pipe={pipe}, bake={bake}")
    except (OSError, KeyError, TypeError, ValueError, tomllib.TOMLDecodeError) as e:
        raise SystemExit(
            f"ERROR  cannot read fixture counts from {BUILD_TOML.name} "
            f'(expected [artifact.fixtures] counts = {{ "pipe" = N, "bake" = M }}): {e}\n'
            "       refusing to write PROVENANCE.md with an unverifiable corpus size"
        )
    return pipe, bake


def check() -> int:
    """Local drift only. Deliberately does NOT need the web repo."""
    if not SUMS.exists():
        print("FAIL  tools/vendor/SHA256SUMS is missing")
        return 1
    want = {}
    for line in SUMS.read_text().splitlines():
        if line.strip():
            h, _, path = line.partition("  ")
            want[path] = h
    tracked = set(VENDORED) | set(DERIVED)
    missing = sorted(tracked - set(want))
    extra = sorted(set(want) - tracked)
    bad = []
    for path, h in want.items():
        p = ROOT / path
        if not p.exists():
            bad.append(f"{path}: MISSING")
        elif sha(p) != h:
            bad.append(f"{path}: hash differs — a tracked file was edited in place")
    for m in missing:
        bad.append(f"{m}: tracked but absent from SHA256SUMS (regenerate the manifest)")
    for e in extra:
        bad.append(f"{e}: in SHA256SUMS but no longer tracked (regenerate the manifest)")
    # Semantic pin: re-derive each DERIVED file from its ref source in memory.
    # The hash above catches a local edit; THIS catches the ref copy and the
    # derived file drifting apart (e.g. someone refreshes the ref and forgets
    # the derivation).
    for path, spec in DERIVED.items():
        dp, sp = ROOT / path, ROOT / spec["source"]
        if not dp.exists() or not sp.exists():
            continue  # the hash loop already reported the MISSING side
        if spec["derive"](sp.read_bytes()) != dp.read_bytes():
            bad.append(f"{path}: no longer matches re-derivation from {spec['source']}")
    if bad:
        print("FAIL  vendored-file drift:")
        for b in bad:
            print("        " + b)
        return 1
    print(f"[check] {len(want)} files match tools/vendor/SHA256SUMS "
          f"({len(DERIVED)} also re-derived from their ref sources)")
    return 0


def regenerate() -> int:
    pipe, bake = fixture_counts()  # fail BEFORE writing anything
    upstream = "unknown (web repo not present)"
    if (WEB / ".git").exists():
        upstream = subprocess.run(["git", "-C", str(WEB), "rev-parse", "HEAD"],
                                  capture_output=True, text=True).stdout.strip()
    rows, drows, sum_pairs = [], [], []
    for local, up in sorted(VENDORED.items()):
        lp = ROOT / local
        ls = sha(lp)
        upp = WEB / up
        us = sha(upp) if upp.exists() else None
        rows.append((local, up, ls, us, lp.stat().st_size))
        sum_pairs.append((local, ls))
    for local, spec in sorted(DERIVED.items()):
        lp = ROOT / local
        ls = sha(lp)
        holds = spec["derive"]((ROOT / spec["source"]).read_bytes()) == lp.read_bytes()
        drows.append((local, spec["source"], spec["how"], spec["why"], ls,
                      lp.stat().st_size, holds))
        sum_pairs.append((local, ls))
    sum_pairs.sort()
    SUMS.write_text("".join(f"{h}  {p}\n" for p, h in sum_pairs))

    pins = "".join(
        f"  {local:<52} {rev}\n" for local, rev in sorted(FIXTURE_ORACLE_PINS.items()))
    body = "".join(
        f"| `{l}` | `{u}` | {sz} | `{ls[:16]}…` | "
        f"{'yes' if us == ls else ('MISMATCH' if us else 'n/a')} |\n"
        for l, u, ls, us, sz in rows)
    dbody = "".join(
        f"| `{l}` | `{s}` | {how} | {sz} | `{ls[:16]}…` | "
        f"{'yes' if ok else 'NO — re-derive'} |\n"
        for l, s, how, why, ls, sz, ok in drows)
    PROV.write_text(f"""# Vendored from the web repo — provenance

Everything in this tree that was copied out of `viz-web/glyph3d-js`, with the
commit it came from and its hash. Recorded because a vendored file with no
recorded origin is indistinguishable from a local invention six months later.

  upstream repo    viz-web/glyph3d-js
  upstream commit  {upstream}
  regenerated      {date.today().isoformat()}  (tools/vendor-manifest.py)

## The fixture oracle is pinned PER FILE

`engine/fixtures/{{gen,gen-bake}}.mjs` reproduce all {pipe + bake} committed
fixtures ({pipe} `.pipe.bin` + {bake} `.bake.bin`; counts read from
`build.toml [artifact.fixtures]`) byte-for-byte, in this tree, with no web
repo present — but only from these
revisions. Today's upstream cannot: `glyphPipelineReference.js` stopped
exporting `FLOAT_LANES` at 3da6542 and the generators do not even load.

{pins}
A single "upstream commit" is the wrong model for these files and hid a real
defect: the `glyphPipelineKernels.js` vendored here until 2026-09-04 was
78,567 bytes, but `real-kernels.pipe.bin` embeds 90,515 — the file at 59a2a44.
The old copy matched its own recorded hash on every `--check` run. The gate
asked whether the file had been edited locally; it could not ask whether it
was the file the corpus came from. Byte-identical regeneration of the whole
corpus from these inputs — run by the battery's fixture gate on every pass —
is that check.

## Verifying

  LOCAL drift (did someone edit a vendored copy?) — no web repo needed:
      python3 tools/vendor-manifest.py --check
  This runs in the battery's generator-reproduction gate (`tools/check-all.sh`
  step 1; build.toml gate `vendor-hashes`).

  UPSTREAM drift (did the web repo move on?) — needs the web repo present:
      python3 tools/vendor-manifest.py     and read the last column.
  A difference is INFORMATION, not a failure: the native tree is trunk and is
  not obliged to track the web repo. Refreshing is a decision, never a reflex.

## What is here

| local path | upstream path | bytes | sha256 (local) | upstream matches |
|---|---|---:|---|:--:|
{body}
## Derived, not copied: `tools/vendor/hb.*`

These two are not upstream mirror entries — each is DERIVED from a vendored
ref file by the stated transformation, and `--check` re-runs that derivation
in memory on every pass. The bare hash catches a local edit; the
re-derivation catches the ref copy and the derived file drifting apart (e.g.
a refreshed ref with a stale derived twin). They exist because
`tools/export-atlas.mjs` (:87-91) must `require()` HarfBuzz from Node >= 22,
which refuses the ref `hb.js` — UMD with a trailing ESM `export default`
(ERR_AMBIGUOUS_MODULE_SYNTAX; Bun, the web repo's bake runtime, accepts it).

| local path | derived from | transformation | bytes | sha256 (local) | derivation holds |
|---|---|---|---:|---|:--:|
{dbody}
## Notes

- `tools/vendor/ref/**` mirrors the web repo's LAYOUT exactly so every file stays
  byte-verbatim and `export-atlas.mjs` needs only one path constant. Do not edit
  these; a vendored file you edited is a fork you did not declare, which is what
  the `--check` gate exists to catch.
- `engine/fixtures/inputs/foldGeometry.js` and `glyphPipelineKernels.js` are
  FIXTURE CORPUS inputs, and they ARE read: `engine/fixtures/gen.mjs` loads
  `inputs/foldGeometry.js` at :93 (the `repo-file` fixture's source bytes) and
  `inputs/glyphPipelineKernels.js` at :175 (the `real-kernels` fixture). The
  generators DO run in this tree — `engine/glyph_schema.mjs` is emitted here by
  `tools/gen_schema.py` — and the battery's fixture gate regenerates the whole
  corpus from these inputs BYTE-IDENTICALLY on every pass. They are vendored so
  the corpus input is frozen at the pinned revisions above rather than drifting
  with upstream: today's upstream `glyphPipelineKernels.js` differs from the
  59a2a44 pin (`3da6542` landed after it), so an unpinned input would regenerate
  a DIFFERENT corpus and redden that gate.
- `schema/glyph-identity.json` is the source of truth for `tools/gen_schema.py`.
""")
    print(f"[done] {len(rows) + len(drows)} files -> {SUMS.name} + {PROV.name}")
    mism = [l for l, u, ls, us, sz in rows if us and us != ls]
    print("upstream mismatches:", ", ".join(mism) if mism else "none")
    broken = [l for l, s, how, why, ls, sz, ok in drows if not ok]
    if broken:
        print("WARNING  DERIVED re-derivation mismatch:", ", ".join(broken))
        print("         the recorded hash now pins bytes the documented derivation")
        print("         does not produce — re-derive the file or fix DERIVED")
    return 0


if __name__ == "__main__":
    raise SystemExit(check() if "--check" in sys.argv[1:] else regenerate())
