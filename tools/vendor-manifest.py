#!/usr/bin/env python3
"""vendor-manifest.py — regenerate tools/vendor/{SHA256SUMS,PROVENANCE.md}.

Every file this repo copied out of the web repo, with the upstream path, the
upstream commit, and a hash. A vendored file with no recorded origin is
indistinguishable from a local invention six months later — this is the record
that keeps "where did this come from" answerable after the web repo goes quiet.

Two different questions, deliberately kept separate:

  LOCAL drift   — did someone edit a vendored copy in THIS repo?
                  `shasum -a 256 -c tools/vendor/SHA256SUMS`, no web repo needed.
                  This is a GATE (check-all step 1): editing a vendored file is
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
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = Path("/Users/lugo/localdev/viz-web/glyph3d-js")  # only needed to REFRESH

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
    "schema/glyph-identity.json": "schema/glyph-identity.json",
}

SUMS = ROOT / "tools/vendor/SHA256SUMS"
PROV = ROOT / "tools/vendor/PROVENANCE.md"


def sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


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
    missing = sorted(set(VENDORED) - set(want))
    extra = sorted(set(want) - set(VENDORED))
    bad = []
    for path, h in want.items():
        p = ROOT / path
        if not p.exists():
            bad.append(f"{path}: MISSING")
        elif sha(p) != h:
            bad.append(f"{path}: hash differs — a vendored file was edited in place")
    for m in missing:
        bad.append(f"{m}: in VENDORED but absent from SHA256SUMS (regenerate the manifest)")
    for e in extra:
        bad.append(f"{e}: in SHA256SUMS but no longer vendored (regenerate the manifest)")
    if bad:
        print("FAIL  vendored-file drift:")
        for b in bad:
            print("        " + b)
        return 1
    print(f"[check] {len(want)} vendored files match tools/vendor/SHA256SUMS")
    return 0


def regenerate() -> int:
    upstream = "unknown (web repo not present)"
    if (WEB / ".git").exists():
        upstream = subprocess.run(["git", "-C", str(WEB), "rev-parse", "HEAD"],
                                  capture_output=True, text=True).stdout.strip()
    rows, sums = [], []
    for local, up in sorted(VENDORED.items()):
        lp = ROOT / local
        ls = sha(lp)
        upp = WEB / up
        us = sha(upp) if upp.exists() else None
        rows.append((local, up, ls, us, lp.stat().st_size))
        sums.append(f"{ls}  {local}")
    SUMS.write_text("\n".join(sums) + "\n")

    body = "".join(
        f"| `{l}` | `{u}` | {sz} | `{ls[:16]}…` | "
        f"{'yes' if us == ls else ('MISMATCH' if us else 'n/a')} |\n"
        for l, u, ls, us, sz in rows)
    PROV.write_text(f"""# Vendored from the web repo — provenance

Everything in this tree that was copied out of `viz-web/glyph3d-js`, with the
commit it came from and its hash. Recorded because a vendored file with no
recorded origin is indistinguishable from a local invention six months later.

  upstream repo    viz-web/glyph3d-js
  upstream commit  {upstream}
  regenerated      {date.today().isoformat()}  (tools/vendor-manifest.py)

## Verifying

  LOCAL drift (did someone edit a vendored copy?) — no web repo needed:
      python3 tools/vendor-manifest.py --check
  This runs as part of `tools/check-all.sh` gate 1.

  UPSTREAM drift (did the web repo move on?) — needs the web repo present:
      python3 tools/vendor-manifest.py     and read the last column.
  A difference is INFORMATION, not a failure: the native tree is trunk and is
  not obliged to track the web repo. Refreshing is a decision, never a reflex.

## What is here

| local path | upstream path | bytes | sha256 (local) | upstream matches |
|---|---|---:|---|:--:|
{body}
## Notes

- `tools/vendor/ref/**` mirrors the web repo's LAYOUT exactly so every file stays
  byte-verbatim and `export-atlas.mjs` needs only one path constant. Do not edit
  these; a vendored file you edited is a fork you did not declare, which is what
  the `--check` gate exists to catch.
- `engine/fixtures/inputs/foldGeometry.js` and `glyphPipelineKernels.js` are
  FIXTURE CORPUS inputs, not code. The fixture generators read live web-repo
  source for these two; `minified-sample.js` was already vendored for exactly
  this reason and these two complete the set. NOTHING READS THEM YET — the
  generators cannot run in this tree (they need a `packages/` path that does not
  exist here, and `engine/glyph_schema.mjs`, which this tree does not emit).
  They are vendored now so the corpus input is frozen at a known commit rather
  than drifting until someone gets to the Rust port.
- The committed fixtures CANNOT be reproduced from these inputs:
  `glyphPipelineKernels.js` changed upstream in `3da6542` after those fixtures
  were generated. Regeneration will produce a NEW corpus, deliberately. The old
  bytes live in git history, which is where superseded evidence belongs.
- `schema/glyph-identity.json` is the source of truth for `tools/gen_schema.py`.
""")
    print(f"[done] {len(rows)} files -> {SUMS.name} + {PROV.name}")
    mism = [l for l, u, ls, us, sz in rows if us and us != ls]
    print("upstream mismatches:", ", ".join(mism) if mism else "none")
    return 0


if __name__ == "__main__":
    raise SystemExit(check() if "--check" in sys.argv[1:] else regenerate())
