#!/usr/bin/env python3
"""gen_schema.py — validate schema/glyph-identity.json and generate engine/glyph_schema.mjs.

PORTED FROM the web repo's tools/gen-schema.mjs (2026-09-02). The schema JSON is
vendored verbatim.

The schema is the source of truth; every layer generates from it. This is the
script that makes "say it once" true rather than aspirational: hand-editing a
generated file is pointless because the next run overwrites it.

Validation runs FIRST and raises. A schema that violates the invariants is a
build failure, not a review miss — which is the whole reason the schema exists.
The validation is ported WHOLE from the original — the rules are the asset, not
the emitter.

SCOPE NOTE. The JS original emits three files; this tree has one consumer, the
fixture generators, so this emits only their file, engine/glyph_schema.mjs.

Run: python3 tools/gen_schema.py [--check]
     --check regenerates in memory and diffs against the committed file
             without writing (exits 1 on any drift; build.toml's glyph-schema
             artifact runs it).
"""

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCHEMA_PATH = ROOT / "schema" / "glyph-identity.json"
JS_OUT = ROOT / "engine" / "glyph_schema.mjs"


def validate(s: dict) -> dict:
    errs: list[str] = []
    # s['buffers'] carries a $comment; iterate only entries that are actual buffers.
    slot_bufs = {k: v for k, v in s["buffers"].items()
                 if isinstance(v, dict) and v.get("lanes")}
    all_bufs = dict(slot_bufs)
    all_bufs.update({
        "partialCounts": s["scanPartial"]["counts"],
        "partialMeasures": s["scanPartial"]["measures"],
        "itemMeasures": s["itemTable"]["measures"],
        "itemExact": s["itemTable"]["exact"],
        "itemBounds": s["itemBounds"],
    })

    # A SECTION WITH NO CARRIER IS UNVALIDATABLE BY CONSTRUCTION — every carrier
    # rule silently skips it, so the hole reopens the moment someone adds a
    # section. itemBounds sat exactly there (TOTAL_ROWS, a count, riding an
    # undeclared f64) until 2026-08-31. f64 is a legitimate host carrier; an
    # exact kind on ANY float carrier still needs its 'misplaced' say-so.
    KNOWN_CARRIERS = {"f32", "u32", "f64"}
    FLOAT_CARRIERS = {"f32", "f64"}
    for name, buf in all_bufs.items():
        if buf.get("carrier") not in KNOWN_CARRIERS:
            errs.append(f"{name}: no declared carrier (or unknown '{buf.get('carrier')}') — "
                        f"an undeclared carrier makes every carrier rule skip this section silently")

    for name, buf in all_bufs.items():
        seen = set()
        for lane in buf["lanes"]:
            if not isinstance(lane.get("index"), int):
                errs.append(f"{name}.{lane['name']}: no index")
            if lane.get("index") in seen:
                errs.append(f"{name}: duplicate index {lane['index']}")
            seen.add(lane.get("index"))
            if lane.get("index", 0) >= buf["stride"]:
                errs.append(f"{name}.{lane['name']}: index {lane['index']} >= stride {buf['stride']}")
        if len(buf["lanes"]) != buf["stride"]:
            errs.append(f"{name}: {len(buf['lanes'])} lanes but stride {buf['stride']}")
        for i in range(buf["stride"]):
            if i not in seen:
                errs.append(f"{name}: index {i} unassigned")

    # EVERY lane declares a kind. A lane without one is a build failure, not a
    # review miss — that omission is how totalRows stayed a float for a year.
    KINDS = {"count", "identity", "bitfield", "measure"}
    for name, buf in all_bufs.items():
        for lane in buf["lanes"]:
            if lane.get("kind") not in KINDS:
                errs.append(f"{name}.{lane['name']}: kind '{lane.get('kind')}' "
                            f"is not one of {sorted(KINDS)}")
    for lane in s["itemBounds"]["lanes"]:
        if lane.get("kind") not in KINDS:
            errs.append(f"itemBounds.{lane['name']}: missing or invalid kind")

    # THE ENGINE'S CONTAINER IS DERIVED FROM KIND, AND ASSERTED AGAINST IT. Another
    # layer may realize the same kinds in a different container and stay conformant;
    # what it owes is this same assertion over its own mapping.
    #
    # Over EVERY carrier-bearing buffer, not just the slot buffers. This loop ran
    # over slotBufs alone until 2026-08-31, which is exactly why five counts sat
    # declared as measures in the item table for its whole life.
    for name, buf in all_bufs.items():
        for lane in buf["lanes"]:
            if (buf.get("carrier") in FLOAT_CARRIERS and lane.get("kind") != "measure"
                    and not lane.get("misplaced")):
                errs.append(f"{name}.{lane['name']}: kind '{lane.get('kind')}' is exact but sits "
                            f"in a {buf.get('carrier')} buffer with no 'misplaced' justification")
            if buf.get("carrier") == "u32" and lane.get("kind") == "measure":
                errs.append(f"{name}.{lane['name']}: a measure in a u32 buffer")

    # THE INVARIANT THIS FILE EXISTS FOR: no exact value may ride a float carrier.
    for name, buf in all_bufs.items():
        if buf.get("carrier") not in FLOAT_CARRIERS:
            continue
        for lane in buf["lanes"]:
            if lane.get("bitfield") and not lane.get("misplaced"):
                errs.append(f"{name}.{lane['name']}: a bitfield in a {buf.get('carrier')} "
                            f"buffer with no 'misplaced' justification")

    for ident in s["identities"]:
        if ident.get("carrier") != "u32":
            errs.append(f"identity {ident['name']}: carrier is '{ident.get('carrier')}', "
                        f"identities must be u32")

    # THE TRUNCATION INVARIANT. The record format is defined as a prefix of each
    # buffer, which is what makes emitting one a shortening rather than a gather.
    # slotBufs ON PURPOSE: only the slot buffers feed the record truncation.
    for name, buf in slot_bufs.items():
        seen_unread = None
        for lane in sorted(buf["lanes"], key=lambda l: l["index"]):
            if not lane.get("read_by_vertex"):
                if seen_unread is None:
                    seen_unread = lane["name"]
            elif seen_unread:
                errs.append(f"{name}.{lane['name']} is read by the vertex path but sits after "
                            f"{seen_unread}, which is not — the record format would stop "
                            f"being a truncation")

    # THE PARTITION MUST BE BINARY AND TOTAL.
    KIND_IS_EXACT = {"count": True, "identity": True, "bitfield": True, "measure": False}
    for name, buf in all_bufs.items():
        for lane in buf["lanes"]:
            if lane.get("kind") not in KIND_IS_EXACT:
                errs.append(f"{name}.{lane['name']}: kind '{lane.get('kind')}' has no side in "
                            f"the measure/exact partition — a consumer classifying in two "
                            f"buckets would have nowhere to put it")

    # THE RECORD IS A WIRE FORMAT, so its byte count must be DERIVED from field widths.
    CARRIER_BYTES = {"f32": 4, "u32": 4}
    for name, buf in slot_bufs.items():
        if CARRIER_BYTES.get(buf.get("carrier")) != 4:
            errs.append(f"{name}: carrier '{buf.get('carrier')}' is not a known 4-byte carrier "
                        f"— RECORD_BYTES is derived assuming 4 bytes per field and would be wrong")

    # THE SEMANTIC SET AND THIS LAYER'S TABLE MUST ACCOUNT FOR EACH OTHER, both ways.
    semantic = {p["name"] for p in s["itemParams"]["params"]}
    realized = set()
    item_lanes = list(s["itemTable"]["measures"]["lanes"]) + list(s["itemTable"]["exact"]["lanes"])
    for lane in item_lanes:
        kinds = [k for k in ("realizes", "realization_only", "orphan") if k in lane]
        if len(kinds) != 1:
            errs.append(f"itemTable.{lane['name']}: must declare exactly one of realizes / "
                        f"realization_only / orphan — has {len(kinds)}. A lane whose "
                        f"relationship to the semantic set is unstated is how a field ends up "
                        f"in one layer and not the other with nothing to notice.")
            continue
        if lane.get("realizes"):
            if lane["realizes"] not in semantic:
                errs.append(f"itemTable.{lane['name']}: realizes '{lane['realizes']}', "
                            f"which is not a semantic parameter")
            # KIND must agree across the tiers.
            sp2 = next((p for p in s["itemParams"]["params"] if p["name"] == lane["realizes"]), None)
            if sp2 and sp2.get("kind") != lane.get("kind"):
                errs.append(f"itemTable.{lane['name']}: kind '{lane['kind']}' but realizes "
                            f"'{lane['realizes']}' whose semantic kind is '{sp2.get('kind')}' "
                            f"— the tiers disagree")
            if lane["realizes"] in realized:
                errs.append(f"itemTable: '{lane['realizes']}' realized by more than one lane")
            realized.add(lane["realizes"])
    for name in semantic:
        if name not in realized:
            errs.append(f"itemParams.{name}: no lane of this layer's item table realizes it — "
                        f"a layout parameter this backend cannot express")

    # A DECLARATION MAY NOT OUTLIVE ITS JUSTIFICATION. A settled debt must FAIL
    # until it is removed, or the exemption silently covers whatever lands next.
    for name, buf in all_bufs.items():
        for lane in buf["lanes"]:
            if not lane.get("misplaced"):
                continue
            deviating = (buf.get("carrier") in FLOAT_CARRIERS and lane.get("kind") != "measure")
            if not deviating:
                errs.append(f"{name}.{lane['name']}: declares 'misplaced' but is not deviating "
                            f"— kind '{lane.get('kind')}' on a '{buf.get('carrier')}' carrier is "
                            f"correct. A settled debt must be REMOVED, not left as an "
                            f"exemption covering whatever lands in this slot next.")
    for lane in item_lanes:
        if (lane.get("realization_only") or lane.get("orphan")) and lane["name"] in semantic:
            kind = "orphan" if lane.get("orphan") else "realization_only"
            errs.append(f"itemTable.{lane['name']}: declared {kind} but IS in the semantic set "
                        f"— the declaration has outlived its reason")

    if errs:
        raise SystemExit("schema invalid:\n  " + "\n  ".join(errs))
    return s


# ── THE FIXTURE FORMAT — frozen on disk (format v2), NOT derived from the
# container. Fixtures carry the ORACLE'S VALUES in this order; the engine's
# working buffers may be re-laid at will and the fixtures do not move.
FIXTURE_MEASURES = ["X", "Y", "Z", "ADVANCE", "HEIGHT", "GLYPH_ID", "BASE_X", "LINE_ADV"]
FIXTURE_COUNTS = ["ROW", "COL", "FLAGS", "ORD"]

# ── THE WIRE, PINNED AS A LITERAL ───────────────────────────────────────────
# Every RECORD_* below is DERIVED from the schema's lane declarations — so a
# schema restructure would silently regenerate the contract with different bytes
# and nothing would fail. The one artifact that must not change would be
# regenerated by the change. This literal is the anchor: derivation still
# happens, and then must MATCH the pin, or the build fails.
WIRE_MEASURES = ["X", "Y", "Z", "ADVANCE", "HEIGHT"]
WIRE_COUNTS = ["GLYPH_ID", "ROW", "COL"]
WIRE_BYTES = 32


def emit_js(s: dict) -> str:
    """The JS half of the schema, for the fixture generators.

    WHY THIS EXISTS. `engine/fixtures/gen.mjs` imports the fixture-format
    strides. Until 2026-09-04 it imported them from the WEB repo, which meant
    the conformance corpus could not be regenerated in this tree at all — the
    port's entire correctness argument rested on 14 binaries whose generator
    could not be run where they live. Vendoring a copy of the web's
    `glyph_schema.mjs` would have fixed the import and recreated the exact
    problem this generator was written to solve: a generated file with no
    generator beside it is an ORPHAN, and the next schema change silently
    desynchronises it.

    So the JS constants are emitted here, from the `FIXTURE_MEASURES` /
    `FIXTURE_COUNTS` lists, and `--check` gates them.

    IT EMITS ONLY THE FIXTURE FORMAT, deliberately. The web's version of this
    file exports 54 names; the fixture generators import two. A declaration may
    not outlive its justification — the container strides belong here the day
    something in this tree reads them, and not before.
    """
    lines = [
        "// GENERATED by tools/gen_schema.py from schema/glyph-identity.json.",
        "// DO NOT EDIT — edit the schema and regenerate.",
        "//",
        "// For engine/fixtures/gen.mjs; the lanes come from the lists in the",
        "// generator, so the corpus and the schema cannot disagree.",
        "",
        "// ── THE FIXTURE FORMAT — frozen on disk (format v2), independent of the",
        "// container layout above it. gen.mjs writes these lanes in THIS order.",
        f"export const FIXTURE_MEASURE_STRIDE = {len(FIXTURE_MEASURES)};",
        *[f"export const FIX_M_{n} = {i};" for i, n in enumerate(FIXTURE_MEASURES)],
        "",
        f"export const FIXTURE_COUNT_STRIDE = {len(FIXTURE_COUNTS)};",
        *[f"export const FIX_C_{n} = {i};" for i, n in enumerate(FIXTURE_COUNTS)],
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    check = "--check" in sys.argv[1:]
    schema = json.loads(SCHEMA_PATH.read_text())
    validate(schema)
    outputs = [(JS_OUT, emit_js(schema))]

    if check:
        drifted = 0
        for path, generated in outputs:
            committed = path.read_text() if path.exists() else ""
            if committed == generated:
                print(f"[check] {path.name}: BYTE-IDENTICAL to the committed file "
                      f"({len(generated)} bytes)")
                continue
            drifted += 1
            print(f"[check] {path.name}: DRIFT "
                  f"(generated {len(generated)} B, committed {len(committed)} B)")
            import difflib
            for line in list(difflib.unified_diff(
                    committed.splitlines(), generated.splitlines(),
                    "committed", "generated", lineterm=""))[:40]:
                print("   " + line)
        if drifted:
            # Last line on purpose: the gate surfaces a check's final line.
            print(f"[check] FAIL: {drifted} generated file(s) DRIFT from the committed copy")
        return 1 if drifted else 0

    for path, generated in outputs:
        path.write_text(generated)
        print(f"[done] wrote {path} ({len(generated)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
