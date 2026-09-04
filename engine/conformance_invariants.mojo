# conformance_invariants.mojo — the two properties nothing else asserts.
#
# WHY THIS EXISTS. Both were found missing by auditing the JS oracle's tests
# (2026-09-02). Neither is a comparison against a recorded answer, so neither can
# live in a .pipe.bin: a fixture pins ONE input's bytes, a property holds over
# every input. That is also why they survived so long unasserted — the fixture
# suites are the thorough ones, and these are not fixture-shaped.
#
#   1. BOUNDS CONTAINMENT. Every placed quad lies inside its item's reported box.
#      Asserted NOWHERE in this tree. conformance.mojo pins the bounds VALUES
#      against the oracle bit-for-bit, which catches a box that changed and says
#      nothing about a box that was always wrong. Culling reads these boxes; a
#      box too small drops glyphs on screen, and every existing gate passes.
#      The JS side had this, and its companion assertion ("the reduce equals a
#      second reduce") was a self-comparison that could not fail — under a
#      mutation that shrank the box, containment was the ONLY check that caught
#      it. So this is the half worth carrying over.
#
#   2. PAGINATION IDEMPOTENCE. Re-applying paginate over finished slots changes
#      nothing, because it is a reconstructive remap of BASE_X plus the integer
#      row/col lanes — never an accumulation onto the current position. Pinned
#      by NOTHING, in any layer: gpu_paginate.mojo and conformance_resume.mojo
#      both RELY on it in prose (resume skips paged items citing paginate's
#      purity) and neither tests it, and the JS assertion that meant to had been
#      dead since the carrier split — `cloneSlots` began returning {m, x}, so
#      `for i in range(again.length)` iterated zero times and the check could
#      not fail.
#
# Both carry an ANTI-VACUITY count: a property that never ran is not a property
# that held, and a suite that says so is the difference between these two.
#
# Run: mojo run -I engine --fp-mode contract=off engine/conformance_invariants.mojo \
#          engine/fixtures/*.pipe.bin

from std.sys import argv
from glyph_schema import (
    LM_STRIDE, LM_X, LM_Y, LM_Z, LC_STRIDE, BOUNDS_STRIDE,
    B_MIN_X, B_MIN_Y, B_MIN_Z, B_MAX_X, B_MAX_Y, B_MAX_Z, B_MAX_ROW_EXTENT,
)
from glyph_pipeline import (
    run_pipeline, Item, Trie, F_LEADER, page_active, paginate, derive_stride,
    WRAP_DOWN, WRAP_BACK,
)
from fixture_io import load_pipe_fixture

comptime MAX_PRINTED = 6


def containment(
    name: String, bytes: List[UInt8], trie: Trie, items: List[Item]
) -> Tuple[Int, Int]:
    """Every placed quad inside its item's box. Returns (defects, quads checked).

    EXACT, no epsilon, deliberately: _bounds_item accumulates in Float64 from the
    same f32 lanes read here, and min/max is an exact SELECTION among values that
    already exist — no arithmetic is introduced that could round. A tolerance
    here would be admitting the comparison is not the one being made.

    The box convention is _bounds_item's, read from its source rather than
    assumed: min is the anchor (x, y, z); max is (x + advance, y + height, z).
    """
    var r = run_pipeline(bytes, trie, items)
    var n = len(bytes)
    var bad = 0
    var checked = 0
    var printed = 0
    for i in range(len(items)):
        var b8 = i * BOUNDS_STRIDE
        var mnx = r.item_bounds[b8 + B_MIN_X]
        var mny = r.item_bounds[b8 + B_MIN_Y]
        var mnz = r.item_bounds[b8 + B_MIN_Z]
        var mxx = r.item_bounds[b8 + B_MAX_X]
        var mxy = r.item_bounds[b8 + B_MAX_Y]
        var mxz = r.item_bounds[b8 + B_MAX_Z]
        var stop = items[i].byte_start + items[i].byte_count
        for id in range(items[i].byte_start, stop):
            if id >= n:
                break
            if (Int(r.fl[id]) & F_LEADER) == 0:
                continue
            var x = Float64(r.lm[id * LM_STRIDE + LM_X])
            var y = Float64(r.lm[id * LM_STRIDE + LM_Y])
            var z = Float64(r.lm[id * LM_STRIDE + LM_Z])
            var w = Float64(r.sm[id * 2 + 0])
            var h = Float64(r.sm[id * 2 + 1])
            checked += 1
            if (x < mnx or y < mny or z < mnz
                    or x + w > mxx or y + h > mxy or z > mxz):
                bad += 1
                if printed < MAX_PRINTED:
                    print("  ", name, "item", i, "byte", id,
                          "quad [", x, y, z, "]+[", w, h, "] outside box [",
                          mnx, mny, mnz, "]..[", mxx, mxy, mxz, "]")
                    printed += 1
    return (bad, checked)


def idempotence(
    name: String, bytes: List[UInt8], trie: Trie, items: List[Item]
) -> Tuple[Int, Int]:
    """Re-applying paginate changes nothing. Returns (defects, paged items).

    Compared BIT-EXACTLY on the render-read lanes: paginate writes positions and
    reads integer lanes, so a reconstructive remap must reproduce its own output
    exactly. Anything short of bit-equality here would be an accumulation.
    """
    var r = run_pipeline(bytes, trie, items)
    var n = len(bytes)
    var before_lm = r.lm.copy()
    var before_lc = r.lc.copy()
    var slots = r.slots()
    var paged = 0
    for i in range(len(items)):
        if not page_active(items[i]):
            continue
        paged += 1
        var stride = derive_stride(
            r.item_bounds[i * BOUNDS_STRIDE + B_MAX_ROW_EXTENT], items[i]
        )
        var stop = items[i].byte_start + items[i].byte_count
        for id in range(items[i].byte_start, stop):
            if id >= n:
                break
            paginate(slots, id, items[i], stride)
    _ = len(r.sm)
    var bad = 0
    var printed = 0
    for k in range(len(before_lm)):
        if r.lm[k].to_bits() != before_lm[k].to_bits():
            bad += 1
            if printed < MAX_PRINTED:
                print("  ", name, "lm lane", k, "moved on re-application:",
                      before_lm[k], "->", r.lm[k])
                printed += 1
    for k in range(len(before_lc)):
        if r.lc[k] != before_lc[k]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  ", name, "lc lane", k, "moved on re-application:",
                      before_lc[k], "->", r.lc[k])
                printed += 1
    return (bad, paged)


def build_topologies(n: Int) -> List[List[Item]]:
    """Constructed item sets, because the FIXTURES are thin where these two
    properties are thickest.

    Idempotence needs PAGED items and the whole .pipe.bin corpus carries four —
    enough that the anti-vacuity guard cannot go quiet, nowhere near enough to
    trust a property that gates the entire page fan. Containment wants
    wrap+page+depth together, which NO fixture has: real-kernels.pipe.bin is the
    one that claims it and its page bag is typo'd inert (`rows`/`gapX` where the
    oracle reads `pageRows`/`pageGapX`).

    Neither property needs an oracle — that is the whole point of them — so
    constructing topologies costs nothing but the writing. Items get DISTINCT
    origins and MIXED page modes for the same reason conformance_matrix does it:
    same-origin items make a wrong item resolution a no-op.
    """
    var out = List[List[Item]]()
    var counts: List[Int] = [1, 3]
    var wraps: List[Int] = [0, 7]
    var modes: List[Int] = [1, 2]        # 1 row-paged, 2 column-paged
    # The WRAP mode, distinct from the page mode above: 0 = WrapDown, 1 = WrapBack.
    # Containment and paginate-idempotence must hold under both, and WrapBack moves
    # the ROW lane that paginate's page gate reads — so this is not a decorative
    # crossing, it changes which page every glyph lands on.
    var wrap_modes: List[Int] = [WRAP_DOWN, WRAP_BACK]
    for ci in range(len(counts)):
        var count = counts[ci]
        for wi in range(len(wraps)):
            for mi in range(len(modes)):
                for wmi in range(len(wrap_modes)):
                    for si in range(2):
                        var items = List[Item]()
                        var per = n // count
                        for i in range(count):
                            var it = Item()
                            it.byte_start = i * per
                            it.byte_count = per if i < count - 1 else n - i * per
                            it.line_height = 1.0 + Float64(i) * 0.25
                            it.origin_x = Float64(i) * 11.0
                            it.origin_y = Float64(i) * 101.0
                            it.origin_z = Float64(i) * 3.0
                            it.wrap_width = wraps[wi]
                            it.wrap_mode = wrap_modes[wmi]
                            it.z_step = 0.5
                            # EVERY item paged here, unlike conformance_matrix's odd-only
                            # rule: this sweep exists to feed idempotence, and an unpaged
                            # item contributes nothing to it. Mixture is covered there.
                            it.has_page = True
                            it.pages_wide = 2
                            it.page_gap_x = 1.0
                            it.band_stride_y = 2.0
                            it.depth_per_band = 0.25
                            if modes[mi] == 1:
                                it.page_rows = 4
                            else:
                                it.page_cols = 6
                                it.depth_per_col = 0.125
                            if si == 1:
                                it.scroll_rows = 2
                            items.append(it^)
                        out.append(items^)
    return out^


def main() raises:
    var args = argv()
    if len(args) < 2:
        print("usage: mojo run -I engine engine/conformance_invariants.mojo <fixture.pipe.bin> ...")
        return
    var bad = 0
    var total_quads = 0
    var total_paged = 0
    for a in range(1, len(args)):
        var path = String(args[a])
        var fx = load_pipe_fixture(path)
        var name = path
        var c = containment(name, fx.bytes, fx.trie, fx.items)
        bad += c[0]
        total_quads += c[1]
        var d = idempotence(name, fx.bytes, fx.trie, fx.items)
        bad += d[0]
        total_paged += d[1]

        # Constructed topologies on this fixture's bytes and trie. No oracle is
        # involved, so these cost nothing to invent and they are where the paged
        # coverage actually comes from.
        if a == 1:
            var tops = build_topologies(len(fx.bytes))
            for t in range(len(tops)):
                var tname = path + " [topology " + String(t) + "]"
                var tc = containment(tname, fx.bytes, fx.trie, tops[t])
                bad += tc[0]
                total_quads += tc[1]
                var td = idempotence(tname, fx.bytes, fx.trie, tops[t])
                bad += td[0]
                total_paged += td[1]

    # ANTI-VACUITY. Both properties are "nothing went wrong" shapes, which pass
    # loudest when nothing happened at all. real-kernels.pipe.bin is the standing
    # example: it advertises "wrapped AND paged at once" and its page bag is
    # typo'd inert, so a paged-items count taken on trust would have read zero
    # and reported success.
    if total_quads == 0:
        print("VACUOUS: containment checked no quads at all")
        bad += 1
    if total_paged == 0:
        print("VACUOUS: no fixture had a paged item, so idempotence tested nothing")
        bad += 1
    if bad != 0:
        raise Error("invariants conformance failed")
    print(
        "invariants: containment exact over", total_quads,
        "quads; paginate idempotent over", total_paged, "paged items",
    )
