#!/usr/bin/env python3
"""glyph.py — the build/verify/check runner over build.toml.

Single entry point for what used to be spread across tools/check-all.sh,
tools/check-pick-oracle.sh, engine/check.sh, pixi run suites, and ad-hoc
--repo-verify / --engine-check invocations. The DECLARATIVE part lives in
build.toml (artifacts, inputs, classes, gates); this file is the mechanism.

  python3 tools/glyph.py build [artifact...]   products current + regenerate committed
  python3 tools/glyph.py verify                assert currency; byte-verify committed + goldens
  python3 tools/glyph.py check                 the full battery (was tools/check-all.sh)
  python3 tools/glyph.py gate <name>           run one gate by name
  python3 tools/glyph.py gates                 list gates: what each compares, what it cannot see
  python3 tools/glyph.py graph                 print the artifact dependency graph
  python3 tools/glyph.py suites [cpu|gpu|all|bench]

Classes (build.toml has the full definitions):
  committed — verified by rebuilding to a scratch path and byte-comparing.
  golden    — verified the same way, but the tool has NO build path for them:
              re-baselining the four pixel masters is a human act, by design.
  product   — untracked; only has to be CURRENT. Currency is a content hash
              of the declared inputs, stamped at build time — not an mtime,
              not a guess.

Counts are declared in build.toml, never derived from the tree under test.
"""
from __future__ import annotations

import argparse
import contextlib
import io
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "build.toml"
STAMP_DIR = ROOT / "native" / "target" / ".glyph-stamps"
BIN = ROOT / "native" / "target" / "release" / "glyph3d-native"
BASE = ROOT / "out" / "tooling-ab" / "baseline"
SWEEP = ROOT / "out" / "tooling-ab" / "sweep"

GREEN_FINAL = "CHECK-ALL: ALL GATES GREEN"
RED_FINAL = "CHECK-ALL: FAILURES — see above"


def load_manifest() -> dict:
    with open(MANIFEST, "rb") as f:
        return tomllib.load(f)


# ── small utilities ──────────────────────────────────────────────────────

def run(cmd: str, cwd: Path = ROOT) -> tuple[int, str]:
    """Run a shell command, return (rc, combined output). The exit code is the
    COMMAND's, never a pipe's — output is captured, not piped through tail."""
    p = subprocess.run(
        cmd, shell=True, cwd=cwd,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    return p.returncode, p.stdout


def step(msg: str) -> None:
    print(f"\n── {msg}")


def expand(pattern: str) -> list[Path]:
    """Manifest glob: literal file, 'dir/**' (recursive, files only), or glob."""
    if pattern.endswith("/**"):
        base = ROOT / pattern[:-3]
        return sorted(p for p in base.rglob("*") if p.is_file())
    if "*" in pattern:
        return sorted(p for p in ROOT.glob(pattern) if p.is_file())
    p = ROOT / pattern
    return [p] if p.is_file() else []


def hash_inputs(patterns: list[str]) -> str:
    h = hashlib.sha256()
    for pat in patterns:
        for p in expand(pat):
            h.update(str(p.relative_to(ROOT)).encode())
            h.update(b"\0")
            h.update(p.read_bytes())
            h.update(b"\0")
    return h.hexdigest()


def stamp_path(name: str) -> Path:
    return STAMP_DIR / f"{name}.json"


def read_stamp(name: str) -> str | None:
    try:
        return json.loads(stamp_path(name).read_text())["inputs_sha256"]
    except (OSError, json.JSONDecodeError, KeyError):
        return None


def write_stamp(name: str, digest: str) -> None:
    STAMP_DIR.mkdir(parents=True, exist_ok=True)
    stamp_path(name).write_text(json.dumps({"inputs_sha256": digest}, indent=2) + "\n")


def warn_count(out: str) -> int:
    return sum(1 for line in out.splitlines() if line.startswith("warning"))


# ── artifact verification mechanisms ─────────────────────────────────────
# Three mechanisms cover every committed artifact. The gates that used to be
# three hand-written shell variants (each with its own restore logic, its own
# coverage counting, its own failure text) are these, once.

def verify_check_mode(name: str, art: dict) -> bool:
    """The generator's own check mode (schema, trie)."""
    rc, out = run(art["verify_cmd"])
    if rc == 0:
        print(f"PASS  {name} — {out.strip().splitlines()[-1] if out.strip() else 'ok'}")
        return True
    print(f"FAIL  {name}")
    print("\n".join(out.splitlines()[-6:]))
    return False


def verify_scratch(name: str, art: dict) -> bool:
    """Rebuild into a scratch dir with the declared command, cmp every output
    (atlas). The committed files are never touched."""
    outputs = art["outputs"]
    with tempfile.TemporaryDirectory() as td:
        rc, out = run(art["verify_scratch"].format(scratch=td))
        if rc != 0:
            print(f"FAIL  {name} — rebuild errored")
            print("\n".join(out.splitlines()[-6:]))
            return False
        ok = True
        for rel in outputs:
            want, got = ROOT / rel, Path(td) / Path(rel).name
            if not got.is_file():
                print(f"FAIL  {name} — rebuild did not produce {Path(rel).name}")
                ok = False
            elif want.read_bytes() != got.read_bytes():
                print(f"FAIL  {name} — {Path(rel).name} differs from the committed asset")
                ok = False
        if ok:
            print(f"PASS  {name} — {len(outputs)} bins BYTE-IDENTICAL (rebuilt to scratch, committed files untouched)")
        return ok


def verify_fixtures(name: str, art: dict) -> bool:
    """Regenerate the corpus in a scratch COPY of engine/fixtures.

    The old gate deleted all 25 committed fixtures in place, rebuilt, and
    restored them with git checkout — a failure mode that needed the restore
    to be exactly right. This rebuilds beside copies of the generators and
    their vendored inputs instead (plus ../glyph_schema.mjs, which gen.mjs
    imports at :60), and compares. Counts are DECLARED in build.toml — the
    old gate ls-derived its expected count from the tree it was checking, so
    a deleted fixture lowered both sides of the comparison and stayed green
    (measured 2026-09-06: eleven of twelve gates green on a shrunken corpus).
    """
    fx = ROOT / "engine" / "fixtures"
    counts = art["counts"]
    with tempfile.TemporaryDirectory() as td:
        sdir = Path(td) / "engine" / "fixtures"
        sdir.mkdir(parents=True)
        for f in ("gen.mjs", "gen-bake.mjs"):
            shutil.copy2(fx / f, sdir / f)
        shutil.copytree(fx / "inputs", sdir / "inputs")
        # gen.mjs:60 imports ../glyph_schema.mjs — preserve the relative layout.
        shutil.copy2(ROOT / "engine" / "glyph_schema.mjs", sdir.parent / "glyph_schema.mjs")
        rc, out = run("node gen.mjs && node gen-bake.mjs", cwd=sdir)
        if rc != 0:
            print(f"FAIL  {name} — a fixture generator errored; the corpus is not rebuildable")
            print("\n".join(out.splitlines()[-6:]))
            return False

        ok = True
        for ext, want_n in (("pipe", counts["pipe"]), ("bake", counts["bake"])):
            produced = sorted(sdir.glob(f"*.{ext}.bin"))
            committed = sorted(fx.glob(f"*.{ext}.bin"))
            if len(produced) != want_n:
                print(f"FAIL  {name} — regenerated {len(produced)} .{ext}.bin, build.toml declares {want_n}"
                      " (a generator's case list changed; update the count deliberately)")
                ok = False
                continue
            p_names = {p.name for p in produced}
            c_names = {p.name for p in committed}
            if p_names != c_names:
                print(f"FAIL  {name} — regenerated set differs from committed:"
                      f" only regenerated: {sorted(p_names - c_names)}, only committed: {sorted(c_names - p_names)}")
                ok = False
                continue
            for p in produced:
                if p.read_bytes() != (fx / p.name).read_bytes():
                    print(f"FAIL  {name} — {p.name} differs from the committed fixture")
                    ok = False
        if ok:
            total = counts["pipe"] + counts["bake"]
            print(f"PASS  {name} — {total} fixtures ({counts['pipe']} pipe + {counts['bake']} bake) regenerated"
                  " BYTE-IDENTICAL in scratch; counts declared in build.toml, not ls-derived")
        return ok


def verify_golden(name: str, art: dict, views: list[dict]) -> bool:
    """Re-render the four views and byte-compare against the golden masters.

    This LOOKS like the other rebuild-and-compare mechanisms and is not one:
    the expected bytes cannot be derived from the declared inputs — the
    'rebuild' is the whole renderer. That is why build.toml gives this
    artifact no build command: teaching the tool to regenerate these would be
    a re-baseline button, and re-baselining is a human act.
    """
    if not BIN.is_file():
        print(f"FAIL  {name} — {BIN} does not exist; run: python3 tools/glyph.py build")
        return False
    SWEEP.mkdir(parents=True, exist_ok=True)
    ok = True
    for v in views:
        rc, out = run(f"./target/release/glyph3d-native {v['cmd']} --screenshot ../{SWEEP.relative_to(ROOT)}/{v['name']}.png",
                      cwd=ROOT / "native")
        if rc != 0:
            print(f"FAIL  {name} — {v['name']} render errored")
            print("\n".join(out.splitlines()[-4:]))
            ok = False
            continue
        if (BASE / f"{v['name']}.png").read_bytes() == (SWEEP / f"{v['name']}.png").read_bytes():
            print(f"PASS  {v['name']}.png BYTE-EQUAL")
        else:
            print(f"FAIL  {v['name']}.png diverges from baseline — the renderer changed; the commit is wrong")
            ok = False
    return ok


def verify_artifact(name: str, art: dict, manifest: dict) -> bool:
    if "verify_cmd" in art:
        return verify_check_mode(name, art)
    if "verify_scratch" in art:
        return verify_scratch(name, art)
    if art.get("counts"):
        return verify_fixtures(name, art)
    if art["class"] == "golden":
        return verify_golden(name, art, manifest["golden_view"])
    print(f"FAIL  {name} — no verification mechanism declared")
    return False


# ── build ────────────────────────────────────────────────────────────────

def build_product(name: str, art: dict, assert_only: bool = False) -> bool:
    digest = hash_inputs(art["inputs"])
    prior = read_stamp(name)
    current = prior == digest and all(expand(o) for o in art["outputs"])
    if current:
        print(f"PASS  {name} current (input hash unchanged)")
        return True
    if assert_only:
        print(f"FAIL  {name} is stale or unbuilt — run: python3 tools/glyph.py build {name}")
        return False
    rc, out = run(art["build"])
    if rc != 0:
        print(f"FAIL  {name} build errored — every gate below would test the wrong binary")
        print("\n".join(out.splitlines()[-6:]))
        return False
    write_stamp(name, digest)
    why = "no prior stamp" if prior is None else "inputs changed"
    print(f"PASS  {name} rebuilt ({why})")
    return True


def cmd_build(args) -> int:
    m = load_manifest()
    arts = m["artifact"]
    names = args.targets or list(arts)
    fail = False
    for name in names:
        if name not in arts:
            print(f"unknown artifact: {name} (have: {', '.join(arts)})")
            return 2
        art = arts[name]
        cls = art["class"]
        if cls == "golden":
            # No build command exists for goldens, BY DESIGN — see build.toml.
            # Asking for one BY NAME is an error; a bare `build` just notes the skip.
            if args.targets:
                print(f"REFUSED  {name} is golden: re-baselining is a deliberate human act, not a build step.")
                print("         Render by hand, eyeball the diff, commit with a note saying why.")
                fail = True
            else:
                print(f"SKIP  {name} — golden: verified, never built (re-baselining is a human act)")
        elif cls == "product":
            step(f"build {name} (product — must be current)")
            fail |= not build_product(name, art)
        else:
            step(f"build {name} (committed — regenerating in place; commit the result deliberately)")
            rc, out = run(art["build"])
            if rc != 0:
                print(f"FAIL  {name}"); print("\n".join(out.splitlines()[-6:])); fail = True
            else:
                print(f"PASS  {name} regenerated — verify with: python3 tools/glyph.py verify")
    return 1 if fail else 0


# ── gates ────────────────────────────────────────────────────────────────

def gate_committed(m: dict) -> bool:
    ok = True
    for name, art in m["artifact"].items():
        if art["class"] == "committed":
            ok &= verify_artifact(name, art, m)
    return ok


def gate_cmd(spec: dict) -> bool:
    rc, out = run(spec["cmd"])
    if spec.get("pass_line"):
        last = out.strip().splitlines()[-1] if out.strip() else ""
        if rc == 0 and spec["pass_line"] in last:
            print(f"PASS  {spec['name']} — {last}")
            # The pick oracle rewrites its tracked scratch proofs; keep them pristine.
            if spec["name"] == "pick-oracle":
                run("git checkout -- out/g-check-*.png 2>/dev/null || true")
            return True
        print("\n".join(out.splitlines()[-20:]))
        print(f"FAIL  {spec['name']}")
        return False
    print(out, end="" if out.endswith("\n") else "\n")
    if rc == 0:
        print(f"PASS  {spec['name']}")
        return True
    print(f"FAIL  {spec['name']}")
    return False


def gate_cargo(spec: dict) -> tuple[bool, bool]:
    cmd = {"build": "cargo build --release", "clippy": "cargo clippy --release"}[spec["cmd"]]
    rc, out = run(cmd, cwd=ROOT / "native")
    if rc != 0:
        print(out)
        print(f"FAIL  {spec['name']} errored")
        return False, True  # a broken build makes the gates below meaningless
    w = warn_count(out)
    if w == 0:
        print(f"PASS  {spec['cmd']} — 0 warnings")
        return True, False
    lines = out.splitlines()
    for i, line in enumerate(lines):
        if line.startswith("warning"):
            print("\n".join(lines[i:i + 5]))
    print(f"FAIL  {spec['cmd']} — {w} warnings")
    return False, False


def gate_cargo_test(m: dict) -> bool:
    # The floor is a RATCHET, not an equality — tests are added constantly, so
    # an exact pin would redden on the most common good action in the repo.
    # A green run whose real count exceeds the floor prints a NOTE naming the
    # value to raise it to, so the floor cannot quietly decay. This step also
    # holds the two corpus-size pins (fixture.rs, bake.rs — "update
    # deliberately"), which is why the floor sums tests that ACTUALLY RAN
    # rather than trusting the tree's shape.
    floor = m["settings"]["test_floor"]
    rc, out = run("cargo test --release", cwd=ROOT / "native")
    for line in out.splitlines():
        if "test result" in line:
            print(line)
    binaries = out.count("test result: ok")
    total = sum(int(x) for x in re.findall(r"(\d+) passed", out))
    if rc != 0:
        print(f"FAIL  cargo test (rc={rc})")
        return False
    if binaries < 2:
        print(f"FAIL  cargo test — only {binaries} test binary reported; a whole binary stopped running")
        return False
    if total < floor:
        print(f"FAIL  cargo test — {total} tests ran, floor is {floor}. Coverage DROPPED by {floor - total};")
        print("      a deleted test, or a module that stopped being compiled. Lower the floor only on purpose.")
        return False
    print(f"PASS  tests green — {total} tests over {binaries} binaries (floor {floor})")
    if total > floor:
        print(f"NOTE  the floor is behind: raise test_floor to {total} in build.toml [settings]")
    return True


def gate_engine_check() -> bool:
    ok = True
    # Two inputs: src/main.rs is well-formed UTF-8 by construction and can
    # never reach the out-of-range decode path where the two implementations
    # actually disagreed; overflow-leads.txt is the only input that does.
    for target in ("src/main.rs", "fixtures/overflow-leads.txt"):
        rc, out = run(f"./target/release/glyph3d-native --engine-check {target}", cwd=ROOT / "native")
        last = out.strip().splitlines()[-1] if out.strip() else ""
        print(last)
        if "engine-check PASS" in last:
            print(f"PASS  engine-check ({target})")
        else:
            print(f"FAIL  engine-check ({target})")
            ok = False
    return ok


def gate_repo_verify() -> bool:
    # BOTH wrap modes: the per-item and batched paths could differ about
    # wrap_mode specifically and one mode would never show it.
    ok = True
    for mode in ("down", "back"):
        rc, out = run(
            f"./target/release/glyph3d-native --load-repo fixtures/g-pick-repo"
            f" --wrap-mode {mode} --repo-verify --repo-scan-only",
            cwd=ROOT / "native")
        passes = [l for l in out.splitlines() if "repo-verify PASS" in l]
        if rc == 0 and passes:
            print(f"PASS  --wrap-mode {mode} — {passes[0]}")
        elif rc == 0:
            print(f"FAIL  --wrap-mode {mode} — ran clean but printed no repo-verify PASS line")
            print("\n".join(out.splitlines()[-4:]))
            ok = False
        else:
            print(f"FAIL  --wrap-mode {mode} — the two FFI strategies disagree:")
            print("\n".join(out.splitlines()[-6:]))
            ok = False
    return ok


def run_gate(spec: dict, m: dict) -> tuple[bool, bool]:
    """Returns (ok, fatal)."""
    kind = spec["kind"]
    if kind == "products":
        ok = build_product("dylib", m["artifact"]["dylib"])
        return ok, not ok  # stale dylib that cannot be rebuilt: stop
    if kind == "verify-committed":
        return gate_committed(m), False
    if kind == "cmd":
        return gate_cmd(spec), False
    if kind == "cargo":
        return gate_cargo(spec)
    if kind == "cargo-test":
        return gate_cargo_test(m), False
    if kind == "engine-check":
        return gate_engine_check(), False
    if kind == "golden-verify":
        return verify_golden(spec["name"], m["artifact"]["pixel-baselines"], m["golden_view"]), False
    if kind == "repo-verify":
        return gate_repo_verify(), False
    print(f"FAIL  unknown gate kind {kind}")
    return False, False


# ── mutation coverage ────────────────────────────────────────────────────
# A gate's value is what it REJECTS; its green only repeats what you already
# assumed. These declared mutations make that an assertion instead of an
# anecdote: apply a named defect, require the named gate to go red for the
# named reason, restore, and prove the tree came back.
#
# Every guard below exists because its absence cost this repo real time:
#   green first     — a red on an already-red tree proves nothing
#   assert applied  — "I broke it and nothing failed" and "I failed to break
#                     it" print identically; a failed edit is silent
#   right gate/text — a mutation reddening some OTHER gate is not evidence
#   restore proven  — byte-compare the snapshot back; never `git checkout` a
#                     directory, which once reverted generators alongside
#                     fixtures and ate a live edit
#
# Reported as COVERAGE, not as a pass count: "N gates, C covered, U uncovered"
# — because "9 mutations passed" describes the size of what you ran instead of
# the size of what exists, which is the mistake the old ls-derived fixture
# count made.


def apply_mutation(mu: dict) -> bytes:
    """Apply, returning the original bytes. Raises if the edit did not land."""
    f = ROOT / mu["file"]
    before = f.read_bytes()
    op = mu["op"]
    if op == "append":
        f.write_bytes(before + mu["arg"].encode())
    elif op == "replace":
        text = before.decode()
        if mu["find"] not in text:
            raise RuntimeError(f"find-text absent from {mu['file']}; mutation cannot land")
        f.write_bytes(text.replace(mu["find"], mu.get("with", ""), 1).encode())
    elif op == "remove":
        f.rename(f.with_suffix(f.suffix + ".mutaside"))
    else:
        raise RuntimeError(f"unknown mutation op {op}")
    after = f.read_bytes() if f.exists() else b""
    if after == before:
        raise RuntimeError(f"mutation {mu['name']} did not change {mu['file']}")
    return before


def restore_mutation(mu: dict, before: bytes) -> bool:
    f = ROOT / mu["file"]
    aside = f.with_suffix(f.suffix + ".mutaside")
    if aside.exists():
        aside.rename(f)
    else:
        f.write_bytes(before)
    return f.read_bytes() == before


def cmd_mutate(args) -> int:
    m = load_manifest()
    gates = {g["name"]: g for g in m["gate"]}
    muts = [mu for mu in m.get("mutation", []) if not args.gate or mu["gate"] == args.gate]
    covered = {mu["gate"] for mu in m.get("mutation", [])}
    uncovered = [n for n in gates if n not in covered]

    fail = False
    for mu in muts:
        spec = gates.get(mu["gate"])
        if spec is None:
            print(f"FAIL  {mu['name']} names gate {mu['gate']}, which does not exist")
            fail = True
            continue
        step(f"mutation: {mu['name']} → {mu['gate']}")

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            pre_ok, _ = run_gate(spec, m)
        if not pre_ok:
            print(f"FAIL  {mu['name']} — gate {mu['gate']} was ALREADY RED before mutating;")
            print("      a red here would prove nothing. Fix the tree first.")
            fail = True
            continue

        try:
            before = apply_mutation(mu)
        except RuntimeError as e:
            print(f"FAIL  {mu['name']} — {e}")
            fail = True
            continue

        try:
            if mu.get("rebuild"):
                run(mu["rebuild"])
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                post_ok, _ = run_gate(spec, m)
            out = buf.getvalue()
        finally:
            restored = restore_mutation(mu, before)
            if mu.get("rebuild"):
                run(mu["rebuild"])

        if post_ok:
            print(f"FAIL  {mu['name']} — gate {mu['gate']} stayed GREEN under the mutation.")
            print(f"      It does not catch what it claims: {mu.get('why', '')}")
            fail = True
        elif mu["expect"] not in out:
            print(f"FAIL  {mu['name']} — {mu['gate']} went red, but for an unstated reason.")
            print(f"      expected text containing: {mu['expect']!r}")
            print(f"      got: {out.strip().splitlines()[-1] if out.strip() else '(no output)'}")
            fail = True
        else:
            print(f"PASS  {mu['gate']} reddens on {mu['name']} — {mu['expect']!r}")
        if not restored:
            print(f"FAIL  {mu['name']} — {mu['file']} did NOT restore byte-exact")
            fail = True

    print()
    print(f"COVERAGE  {len(gates)} gates, {len(covered)} with mutations, {len(uncovered)} uncovered")
    if uncovered:
        print(f"          uncovered: {', '.join(sorted(uncovered))}")
        print("          an uncovered gate is an unproven claim, not a passing one.")
    print()
    print("MUTATE: FAILURES — see above" if fail else "MUTATE: every declared mutation reddened its gate")
    return 1 if fail else 0


def cmd_check(args) -> int:
    m = load_manifest()
    fail = False
    for spec in m["gate"]:
        step(f"gate: {spec['name']} — {spec.get('compare', '')}")
        ok, fatal = run_gate(spec, m)
        fail |= not ok
        if fatal:
            print(f"\n{RED_FINAL}")
            return 1
    print()
    print(RED_FINAL if fail else GREEN_FINAL)
    return 1 if fail else 0


def cmd_gate(args) -> int:
    m = load_manifest()
    for spec in m["gate"]:
        if spec["name"] == args.name:
            step(f"gate: {spec['name']} — {spec.get('compare', '')}")
            ok, _ = run_gate(spec, m)
            return 0 if ok else 1
    print(f"unknown gate: {args.name} (have: {', '.join(g['name'] for g in m['gate'])})")
    return 2


def cmd_gates(args) -> int:
    m = load_manifest()
    for spec in m["gate"]:
        print(f"  {spec['name']}")
        print(f"    compares : {spec.get('compare', '—')}")
        if spec.get("blind_to"):
            print(f"    blind to : {spec['blind_to']}")
    print("\nThe full account — what makes each red and what each cannot see — is")
    print("root AGENTS.md, 'Verification'. Run one with: python3 tools/glyph.py gate <name>")
    return 0


def cmd_verify(args) -> int:
    """Assert currency of products, then byte-verify committed artifacts and
    goldens. Builds nothing: `glyph build` is the thing that builds."""
    m = load_manifest()
    fail = False
    step("products current (asserting, not building)")
    fail |= not build_product("dylib", m["artifact"]["dylib"], assert_only=True)
    if not BIN.is_file():
        print("FAIL  renderer binary missing — run: python3 tools/glyph.py build")
        fail = True
    step("committed artifacts (rebuild to scratch / generator check mode, byte-compare)")
    fail |= not gate_committed(m)
    step("goldens (re-render, byte-compare — never rebuilt)")
    fail |= not verify_golden("pixel-baselines", m["artifact"]["pixel-baselines"], m["golden_view"])
    print("\nVERIFY: FAILURES — see above" if fail else "\nVERIFY: all artifacts byte-verified")
    return 1 if fail else 0


def cmd_graph(args) -> int:
    m = load_manifest()
    print("artifact graph (build.toml) — edges flow inputs → artifact\n")
    for name, art in m["artifact"].items():
        print(f"  {name}  [{art['class']}]")
        for o in art.get("outputs", []):
            print(f"    out:  {o}")
        for i in art.get("inputs", []):
            print(f"    in:   {i}")
        if art.get("counts"):
            print(f"    declared counts: {art['counts']}")
        if art.get("note"):
            print(f"    note: {art['note']}")
        print()
    print("The two schema edges (both leave tools/gen_schema.py):")
    print("  engine/glyph_schema.mojo → dylib        (mojo -I engine: the schema is an input)")
    print("  engine/glyph_schema.mjs  → fixtures     (gen.mjs:60 imports ../glyph_schema.mjs)")
    print("Editing schema/glyph-identity.json staleness-invalidates the corpus AS WELL as the dylib.")
    return 0


def cmd_suites(args) -> int:
    mode = args.mode or "all"
    rc, out = run(f"./engine/check.sh {mode}")
    print(out, end="" if out.endswith("\n") else "\n")
    return rc


def main() -> int:
    ap = argparse.ArgumentParser(prog="glyph", description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("build", help="bring products current; regenerate committed artifacts")
    p.add_argument("targets", nargs="*")
    sub.add_parser("verify", help="assert currency; byte-verify committed artifacts and goldens")
    sub.add_parser("check", help="the full gate battery (what check-all.sh runs)")
    p = sub.add_parser("gate", help="run one gate by name")
    p.add_argument("name")
    sub.add_parser("gates", help="list gates: what each compares and cannot see")
    sub.add_parser("graph", help="print the artifact dependency graph")
    p = sub.add_parser("mutate", help="prove each gate rejects what it claims to")
    p.add_argument("--gate", help="only mutations targeting this gate")
    p = sub.add_parser("suites", help="engine conformance suites")
    p.add_argument("mode", nargs="?", choices=["cpu", "gpu", "all", "bench"], default="all")
    args = ap.parse_args()
    os.chdir(ROOT)
    return {
        "build": cmd_build, "verify": cmd_verify, "check": cmd_check,
        "gate": cmd_gate, "gates": cmd_gates, "graph": cmd_graph, "mutate": cmd_mutate,
        "suites": cmd_suites,
    }[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
