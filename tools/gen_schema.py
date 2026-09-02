#!/usr/bin/env python3
"""gen_schema.py — generate engine/glyph_schema.mojo from schema/glyph-identity.json.

PORTED FROM the web repo's tools/gen-schema.mjs (2026-09-02). The schema JSON is
vendored verbatim; the emitted Mojo is byte-identical to what the JS produced,
which is this port's acceptance test (`--check`).

The schema is the source of truth; every layer generates from it. This is the
script that makes "say it once" true rather than aspirational: hand-editing a
generated file is pointless because the next run overwrites it.

Validation runs FIRST and raises. A schema that violates the invariants is a
build failure, not a review miss — which is the whole reason the schema exists.
Until this file existed here, engine/glyph_schema.mojo was an ORPHAN: a
generated artifact with no generator in its tree, and none of the rules below
were enforced on the native side at all.

SCOPE NOTE. The JS original emits three files: the Mojo constants, the renderer's
glyphContract.js, and engine/glyph_schema.mjs for the JS fixture tooling. This
port emits ONLY the Mojo, because only the Mojo has a consumer in this tree. The
validation is ported WHOLE regardless — the rules are the asset, not the emitter.

Run: python3 tools/gen_schema.py [--check]
     --check regenerates in memory and diffs against the committed file
             without writing (the gate; exits 1 on any drift).
"""

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCHEMA_PATH = ROOT / "schema" / "glyph-identity.json"
MOJO_OUT = ROOT / "engine" / "glyph_schema.mojo"


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


def emit_mojo(s: dict) -> str:
    def head(cmt):
        return [
            f"{cmt} GENERATED by tools/gen-schema.mjs from schema/glyph-identity.json.",
            f"{cmt} DO NOT EDIT — edit the schema and regenerate.",
            f"{cmt}",
        ]

    def banner(cmt):
        return "\n".join(head(cmt) + [
            f"{cmt} THIS LAYER'S REALIZATION. Six arrays, split twice — who WRITES a lane",
            f"{cmt} decides where it lives; who READS it decides whether it lives at all:",
            f"{cmt}   static (sm f32 + fl u32)      decode's output; a pure function of the byte",
            f"{cmt}   positional (lm f32 + lc u32)  the fold's output; render-read",
            f"{cmt}   witness (wm f32 + wc u32)     fold interior no render path reads; the",
            f"{cmt}                                 serial form writes it only when witnessed",
            f"{cmt} Float carriers hold measures, u32 carriers hold counts — no bitcasts, and",
            f"{cmt} a count cannot land in a float array by accident.",
            f"{cmt}",
            f"{cmt} Strides and lane indices below are THIS BACKEND'S and are not prescribed",
            f"{cmt} to anyone. What is shared lives in glyphContract.js: the record format and",
            f"{cmt} the kind of each field. Another layer may realize the same kinds in a",
            f"{cmt} different container and stay conformant; what it owes is the assertion",
            f"{cmt} that its own mapping respects them, which validate() performs for this one.",
        ])

    b = s["buffers"]
    st_m, st_c = b["staticMeasures"], b["staticCounts"]
    po_m, po_c = b["posMeasures"], b["posCounts"]
    w_m, w_c = b["witnessMeasures"], b["witnessCounts"]
    g_i = b["staticIdentities"]
    sp, it, ib = s["scanPartial"], s["itemTable"], s["itemBounds"]

    def rbv(buf):
        return [l["name"] for l in sorted(
            (l for l in buf["lanes"] if l.get("read_by_vertex")), key=lambda l: l["index"])]

    got_m = rbv(po_m) + rbv(st_m)
    got_c = rbv(g_i) + rbv(po_c) + rbv(st_c)
    if got_m != WIRE_MEASURES or got_c != WIRE_COUNTS:
        raise SystemExit(
            "THE RECORD WIRE FORMAT MOVED.\n"
            f"  pinned:  [{','.join(WIRE_MEASURES)}] + [{','.join(WIRE_COUNTS)}]\n"
            f"  derived: [{','.join(got_m)}] + [{','.join(got_c)}]\n"
            "  The schema restructure changed which lanes are render-read or their order. "
            "If the wire format change is INTENDED, update WIRE_* in this file in the same "
            "commit and say so; otherwise the restructure broke the record and this error "
            "is doing its job.")
    if (len(got_m) + len(got_c)) * 4 != WIRE_BYTES:
        raise SystemExit(f"record is {(len(got_m) + len(got_c)) * 4} B, "
                         f"wire pin says {WIRE_BYTES}")

    rec_m, rec_c = len(WIRE_MEASURES), len(WIRE_COUNTS)
    bounds_count_lanes = "(" + ",".join(
        str(l["index"]) for l in ib["lanes"] if l.get("kind") != "measure") + ")"

    lines = [
        banner("#"), "",
        "# ── The SLOT BUFFERS: six arrays, split twice ─────────────────────────────",
        "# static  = decode's output (pure function of the byte); positional = the",
        "# fold's render-read output; witness = fold interior no render path reads.",
        "# Decode NEVER touches positional or witness — that is the write-axis split.",
        "# LM/LC = layout measures/counts; SM = static measures; WM/WC = witness;",
        "# FLAGS is its own stride-1 array (flags[id], no lane constant needed).",
        f"comptime SM_STRIDE = {st_m['stride']}",
        *[f"comptime SM_{l['name']} = {l['index']}" for l in st_m["lanes"]],
        f"comptime FLAGS_STRIDE = {st_c['stride']}", "",
        f"comptime LM_STRIDE = {po_m['stride']}",
        *[f"comptime LM_{l['name']} = {l['index']}" for l in po_m["lanes"]], "",
        f"comptime LC_STRIDE = {po_c['stride']}",
        *[f"comptime LC_{l['name']} = {l['index']}" for l in po_c["lanes"]], "",
        "# Witness arrays (read-axis split): stride-1, indexed by byte. The scan form",
        "# allocates them; the serial form only under its witness instantiation.",
        f"comptime WM_STRIDE = {w_m['stride']}",
        f"comptime WC_STRIDE = {w_c['stride']}", "",
        "# GLYPH_ID: a native u32 identity in its own stride-1 array since 2026-08-31",
        "# (the trie format moved first). The oldest deviation, settled.",
        f"comptime GI_STRIDE = {g_i['stride']}", "",
        "# The RECORD format (the wire): byte order unchanged through both splits AND",
        "# the GLYPH_ID settlement. A record is four runs — posMeasures[0..3),",
        "# staticMeasures whole, staticIdentities whole, posCounts whole — a",
        "# concatenation of truncations, still no lane map.",
        f"comptime RECORD_MEASURE_STRIDE = {rec_m}",
        f"comptime RECORD_COUNT_STRIDE = {rec_c}",
        f"comptime RECORD_BYTES = {rec_m * 4 + rec_c * 4}", "",
        "# ── THE FIXTURE FORMAT — frozen on disk (format v2), independent of the",
        "#    container. Fixtures carry the oracle's VALUES in this order; the engine's",
        "#    buffers may be re-laid at will and the fixtures do not move.",
        f"comptime FIXTURE_MEASURE_STRIDE = {len(FIXTURE_MEASURES)}",
        *[f"comptime FIX_M_{n} = {i}" for i, n in enumerate(FIXTURE_MEASURES)], "",
        f"comptime FIXTURE_COUNT_STRIDE = {len(FIXTURE_COUNTS)}",
        *[f"comptime FIX_C_{n} = {i}" for i, n in enumerate(FIXTURE_COUNTS)], "",
        "# The scan partial (ScanElem) in a GPU buffer — same kind rule.",
        f"comptime PARTIAL_COUNT_STRIDE = {sp['counts']['stride']}",
        *[f"comptime P_{l['name']} = {l['index']}" for l in sp["counts"]["lanes"]], "",
        f"comptime PARTIAL_MEASURE_STRIDE = {sp['measures']['stride']}",
        *[f"comptime PM_{l['name']} = {l['index']}" for l in sp["measures"]["lanes"]], "",
        "# Per-item layout params on device, split by carrier (the kind correction):",
        "# world-unit distances f32 (Metal has no f64), integer page geometry native u32.",
        f"comptime IM_STRIDE = {it['measures']['stride']}",
        *[f"comptime IM_{l['name']} = {l['index']}" for l in it["measures"]["lanes"]], "",
        f"comptime IE_STRIDE = {it['exact']['stride']}",
        *[f"comptime IE_{l['name']} = {l['index']}" for l in it["exact"]["lanes"]], "",
        "# Per-item bounds + fold scalars. Lane kinds matter on device: Metal has no f64,",
        "# so counts go to native u32 atomics and measures to f32 ordered keys.",
        f"comptime BOUNDS_STRIDE = {ib['stride']}",
        *[f"comptime B_{l['name']} = {l['index']}" for l in ib["lanes"]],
        f"comptime BOUNDS_COUNT_LANES = {bounds_count_lanes}", "",
        "",
        "def fixture_measure_lane_name(lane: Int) -> String:",
        "    '''FIXTURE lane name for diagnostics (fixture order, not container order).'''",
        *[f'    if lane == {i}:\n        return "{n}"' for i, n in enumerate(FIXTURE_MEASURES)],
        '    return "M_?"', "",
        "",
        "def fixture_count_lane_name(lane: Int) -> String:",
        *[f'    if lane == {i}:\n        return "{n}"' for i, n in enumerate(FIXTURE_COUNTS)],
        '    return "C_?"', "",
    ]
    return "\n".join(lines)


def main() -> int:
    check = "--check" in sys.argv[1:]
    schema = json.loads(SCHEMA_PATH.read_text())
    validate(schema)
    mojo = emit_mojo(schema)

    if check:
        committed = MOJO_OUT.read_text() if MOJO_OUT.exists() else ""
        if committed == mojo:
            print(f"[check] {MOJO_OUT.name}: BYTE-IDENTICAL to the committed file "
                  f"({len(mojo)} bytes)")
            return 0
        print(f"[check] {MOJO_OUT.name}: DRIFT "
              f"(generated {len(mojo)} B, committed {len(committed)} B)")
        import difflib
        for line in list(difflib.unified_diff(
                committed.splitlines(), mojo.splitlines(),
                "committed", "generated", lineterm=""))[:40]:
            print("   " + line)
        return 1

    MOJO_OUT.write_text(mojo)
    print(f"[done] wrote {MOJO_OUT} ({len(mojo)} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
