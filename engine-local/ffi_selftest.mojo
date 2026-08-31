# ffi_selftest.mojo — proves the C ABI wrapper (ffi.mojo) carries the pipeline's
# answers across the boundary EXACTLY, against the fixtures' oracle answers.
#
# Three checks per single-item fixture:
#   1. BOUNDARY: drive the pipeline through the exported functions (new →
#      load_trie_file → load_item → slot_count → copy_slots), compare the 32 B
#      wire records against the fixture's expected lanes, BIT-EXACT (f32 lanes
#      as u32 bits — the same discipline as conformance.mojo).
#   2. LEADERS: record count == fixture's leader count.
#   3. STRESS: 1000 load/free cycles on one handle, full record copy each time
#      (leak/crash check; run under `time -l` to watch RSS).
#
# Run:  mojo run  --fp-mode contract=off -I engine-local engine-local/ffi_selftest.mojo
# Build: mojo build --fp-mode contract=off -I engine-local engine-local/ffi_selftest.mojo -o out/ffi_selftest

from std.sys import argv
from std.memory import bitcast
from std.time import perf_counter_ns

from ffi import (
    glyph_engine_new, glyph_engine_free, glyph_engine_load_trie_file,
    glyph_engine_load_item, glyph_engine_slot_count, glyph_engine_copy_slots,
)
from glyph_pipeline import F_LEADER
from glyph_schema import (
    FIXTURE_MEASURE_STRIDE, FIXTURE_COUNT_STRIDE,
    FIX_M_X, FIX_M_ADVANCE, FIX_M_GLYPH_ID, FIX_C_ROW, FIX_C_COL, FIX_C_FLAGS,
)
from fixture_io import load_pipe_fixture


def _mut[T: AnyType, o: Origin, //](p: Pointer[T, o]) -> Pointer[T, MutUntrackedOrigin]:
    """Erase a pointer's origin for the C ABI surface (test-side mirror of
    what a foreign caller's raw pointer looks like)."""
    return p.unsafe_mut_cast[True]().unsafe_origin_cast[MutUntrackedOrigin]()


def check_fixture(path: String) raises -> Int:
    var fx = load_pipe_fixture(path)
    if fx.item_count != 1:
        print("SKIP ", path, "(multi-item; FFI surface is one item per load)")
        return 0
    var it = fx.items[0].copy()

    var h = glyph_engine_new()
    var st = glyph_engine_load_trie_file(h, _mut(path.unsafe_ptr()), UInt(path.byte_length()))
    if st != 0:
        print("FAIL ", path, ": load_trie_file status", st)
        glyph_engine_free(h)
        return 1

    st = glyph_engine_load_item(
        h, _mut(fx.bytes.unsafe_ptr()), UInt(len(fx.bytes)),
        it.origin_x, it.origin_y, it.origin_z,
        it.line_height, it.z_step,
        Int32(it.wrap_width), Int32(1 if it.has_page else 0),
        Int32(it.page_rows), Int32(it.page_cols), Int32(it.scroll_rows),
        Int32(it.pages_wide),
        it.page_gap_x, it.band_stride_y, it.depth_per_band, it.depth_per_col,
        it.page_line_height,
    )
    if st != 0:
        print("FAIL ", path, ": load_item status", st)
        glyph_engine_free(h)
        return 1

    var n = Int(glyph_engine_slot_count(h))
    var buf = List[UInt32](unsafe_uninit_length=n * 8)
    var copied = Int(glyph_engine_copy_slots(h, _mut(buf.unsafe_ptr()), UInt(n)))
    glyph_engine_free(h)

    # Expected leader count: bytes with F_LEADER in the fixture's FLAGS lane.
    var exp_leaders = 0
    for id in range(fx.byte_len):
        if (Int(fx.exp_counts[id * FIXTURE_COUNT_STRIDE + FIX_C_FLAGS]) & F_LEADER) != 0:
            exp_leaders += 1

    var bad = 0
    if n != exp_leaders or copied != exp_leaders:
        print("FAIL ", path, ": record count", n, "copied", copied,
              "expected", exp_leaders)
        bad += 1

    # Records are leaders in byte order (compact's walk). Compare every lane.
    var k = 0
    var printed = 0
    for id in range(fx.byte_len):
        var mbase = id * FIXTURE_MEASURE_STRIDE
        var cbase = id * FIXTURE_COUNT_STRIDE
        if (Int(fx.exp_counts[cbase + FIX_C_FLAGS]) & F_LEADER) == 0:
            continue
        if k >= n:
            break
        var wo = k * 8
        for lane in range(5):  # X Y Z ADVANCE HEIGHT — bit-exact
            var got = buf[wo + lane]
            var exp = UInt32(
                Float32(fx.exp_measures[mbase + lane]).to_bits()
            )
            if got != exp:
                bad += 1
                if printed < 4:
                    print("FAIL ", path, " rec", k, "measure lane", lane,
                          "got bits", got, "expected bits", exp)
                    printed += 1
        var gid = buf[wo + 5]
        var gid_exp = UInt32(fx.exp_measures[mbase + FIX_M_GLYPH_ID])
        if gid != gid_exp:
            bad += 1
            if printed < 4:
                print("FAIL ", path, " rec", k, "GLYPH_ID got", gid,
                      "expected", gid_exp)
                printed += 1
        for lane in range(2):  # ROW, COL — exact
            var got = buf[wo + 6 + lane]
            var exp = fx.exp_counts[cbase + lane]
            if got != exp:
                bad += 1
                if printed < 4:
                    print("FAIL ", path, " rec", k, "count lane", lane,
                          "got", got, "expected", exp)
                    printed += 1
        k += 1

    if bad == 0:
        print("PASS ", path, "(", n, "records, bit-exact through the C ABI)")
    else:
        print("FAIL ", path, ":", bad, "mismatched lanes")
    return bad


def main() raises:
    var fixtures = List[String]()
    for arg in argv():
        if arg.endswith(".pipe.bin"):
            fixtures.append(String(arg))

    var total_bad = 0
    for f in fixtures:
        total_bad += check_fixture(f)

    # STRESS: 1000 loads on one handle, full record copy each cycle.
    if len(fixtures) > 0:
        var path = fixtures[0]
        var fx = load_pipe_fixture(path)
        var it = fx.items[0].copy()
        var h = glyph_engine_new()
        _ = glyph_engine_load_trie_file(h, _mut(path.unsafe_ptr()), UInt(path.byte_length()))
        var cap = fx.byte_len  # leaders <= bytes
        var buf = List[UInt32](unsafe_uninit_length=cap * 8)
        var t0 = perf_counter_ns()
        for i in range(1000):
            var st = glyph_engine_load_item(
                h, _mut(fx.bytes.unsafe_ptr()), UInt(len(fx.bytes)),
                it.origin_x, it.origin_y, it.origin_z,
                it.line_height, it.z_step,
                Int32(it.wrap_width), Int32(1 if it.has_page else 0),
                Int32(it.page_rows), Int32(it.page_cols), Int32(it.scroll_rows),
                Int32(it.pages_wide),
                it.page_gap_x, it.band_stride_y, it.depth_per_band,
                it.depth_per_col, it.page_line_height,
            )
            if st != 0:
                print("FAIL stress iter", i, "status", st)
                total_bad += 1
                break
            var n = glyph_engine_slot_count(h)
            _ = glyph_engine_copy_slots(h, _mut(buf.unsafe_ptr()), UInt(n))
        var dt = Float64(perf_counter_ns() - t0) / 1e9
        glyph_engine_free(h)
        var total_mb = Float64(fx.byte_len) * 1000.0 / 1e6
        print(
            "stress: 1000 loads x", fx.byte_len, "B in", dt, "s  (",
            total_mb / dt, "MB/s through the FFI, copy included)",
        )

    if total_bad != 0:
        raise Error("ffi_selftest: FAILURES above")
    print("ffi_selftest: all green")
