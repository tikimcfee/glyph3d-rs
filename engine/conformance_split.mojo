# conformance_split.mojo — the sequence pass's SPLIT form, proven against the
# serial rule it decomposes, at the whole-pipeline level.
#
# resolve_clusters (glyph_cluster.mojo) is a serial left-to-right walk; the
# device kernels cannot be one (a thread per leader cannot know whether an
# earlier match swallowed it). cluster_split.mojo splits the rule into a
# per-position probe and a commit chain, and run_pipeline's comptime `split`
# param swaps it in. This suite runs both instantiations over every fixture
# and diffs every lane — statics (gi, sm, fl) AND the fold's output (lm, lc),
# so it proves the fold downstream reads identical input, not just that the
# statics match.
#
# The witness case is cluster-overlap: with (A C) and (C A) both in the table,
# position 1's candidate is a phantom under position 0's committed span; a
# chain that commits every candidate swallows the lone A. If the split ever
# regresses to a windowed OR, that fixture says so.
#
# ANTI-VACUITY: a run over a corpus with no cluster items proves nothing, so
# the suite fails if it never saw one (the bake suite's "no query ran" guard,
# same pattern).
#
# Run: mojo run -I engine engine/conformance_split.mojo engine/fixtures/*.pipe.bin

from std.sys import argv
from glyph_schema import SM_STRIDE, LM_STRIDE, LC_STRIDE
from glyph_pipeline import run_pipeline, CLUSTER_CLUSTER
from fixture_io import load_pipe_fixture

comptime MAX_PRINTED = 8


def main() raises:
    var args = argv()
    if len(args) < 2:
        print("usage: mojo run -I engine engine/conformance_split.mojo <fixture.pipe.bin> ...")
        return
    var failures = 0
    var cluster_cases = 0
    for a in range(1, len(args)):
        var fx = load_pipe_fixture(String(args[a]))
        var ser = run_pipeline(fx.bytes, fx.trie, fx.items)
        var spl = run_pipeline[split=True](fx.bytes, fx.trie, fx.items)
        var bad = 0
        var printed = 0
        var has_cluster = False
        for i in range(len(fx.items)):
            if fx.items[i].cluster_mode == CLUSTER_CLUSTER:
                has_cluster = True
                break
        if has_cluster:
            cluster_cases += 1
        if ser.leaders != spl.leaders:
            bad += 1
            print("  leaders", ser.leaders, "vs split", spl.leaders)
            printed += 1
        for i in range(fx.byte_len * SM_STRIDE):
            if ser.sm[i].to_bits() != spl.sm[i].to_bits():
                bad += 1
                if printed < MAX_PRINTED:
                    print("  sm[", i, "] serial", ser.sm[i], "split", spl.sm[i])
                    printed += 1
        for i in range(fx.byte_len):
            if ser.gi[i] != spl.gi[i]:
                bad += 1
                if printed < MAX_PRINTED:
                    print("  gi[", i, "] serial", ser.gi[i], "split", spl.gi[i])
                    printed += 1
            if ser.fl[i] != spl.fl[i]:
                bad += 1
                if printed < MAX_PRINTED:
                    print("  fl[", i, "] serial", ser.fl[i], "split", spl.fl[i])
                    printed += 1
        for i in range(fx.byte_len * LM_STRIDE):
            if ser.lm[i].to_bits() != spl.lm[i].to_bits():
                bad += 1
                if printed < MAX_PRINTED:
                    print("  lm[", i, "] serial", ser.lm[i], "split", spl.lm[i])
                    printed += 1
        for i in range(fx.byte_len * LC_STRIDE):
            if ser.lc[i] != spl.lc[i]:
                bad += 1
                if printed < MAX_PRINTED:
                    print("  lc[", i, "] serial", ser.lc[i], "split", spl.lc[i])
                    printed += 1
        if bad == 0:
            print("PASS", String(args[a]))
        else:
            print("FAIL", String(args[a]), "—", bad, "lane mismatches")
        failures += bad
    if failures != 0:
        raise Error("split-form conformance failed")
    if cluster_cases == 0:
        raise Error("split-form conformance saw no cluster item — the corpus moved")
    print("")
    print("split-form conformance: probe+chain is bit-exact with the serial rule")
    print("across the corpus,", cluster_cases, "cluster-bearing fixtures included.")
