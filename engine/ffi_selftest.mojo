# ffi_selftest.mojo — proves the SHIPPED dylib (native/libglyph_engine.dylib)
# carries the pipeline's answers across the C ABI EXACTLY, against the
# fixtures' oracle answers.
#
# Three checks per single-item fixture:
#   1. BOUNDARY: drive the pipeline through the dylib's exported functions
#      (new → load_trie_file → load_item_desc → slot_count → copy_slots),
#      compare the 32 B wire records against the fixture's expected lanes,
#      BIT-EXACT (f32 lanes as u32 bits — the same discipline as
#      conformance.mojo).
#   2. LEADERS: record count == fixture's leader count.
#   3. STRESS: 1000 load/copy cycles on one handle (leak/crash check).
#      Plus one negative probe: a descriptor with a wrong shape word must be
#      refused with GE_ABI_MISMATCH — that guard is what the 2026-09-04
#      positional-arity incident bought, and nothing else exercises it.
#
# ── WHY THIS LINKS THE DYLIB INSTEAD OF IMPORTING ffi.mojo ──────────────────
#
# Until 2026-09-06 this file did `from ffi import glyph_engine_*` and called the
# wrapper IN-PROCESS. That form never crossed a boundary — it compiled ffi.mojo
# into the test binary and asserted on the compiler's output, not on the
# artifact the product loads. And under this pinned toolchain
# (mojo 1.1.0.dev2026083005) the in-process form is not merely weak, it is
# UNSOUND: executable codegen miscompiles offset-indexed accesses through
# `unsafe_bitcast`'d pointers at SOME inlined call sites, per compilation unit.
# Measured: `_item_from_desc` read origin_x back as a heap address
# (~2.2e-314 as f64, per-run varying under ASLR — an uninitialized read, not
# deterministic wrong-code), while the SAME function at another call site in
# the SAME binary read it correctly; the selftest accordingly failed with every
# record's X missing the origin (ascii-basic rec 0: got bits 0, expected
# 1069547520). The same ffi.mojo source built as a shared library — the actual
# shipping form — is bit-exact on all 14 single-item fixtures through the real
# ABI (verified via ctypes, including the nonzero-origin fixtures gate 6's
# (0,0,0) origin cannot see). A green coaxed out of the in-process form by
# source layout would be luck, not a property; this suite exists to convert
# exactly that class of luck into a property, so it now tests the dylib.
#
# DLHandle and external_function are gone from this nightly's stdlib;
# external_call + `-Xlinker <dylib> -Xlinker -rpath -Xlinker <dir>` is what
# remains. check.sh performs the build; by hand:
#
#   pixi run build-engine   # the artifact under test — must exist first
#   pixi run mojo build --fp-mode contract=off -I engine engine/ffi_selftest.mojo \
#       -o /tmp/ffi_selftest \
#       -Xlinker "$PWD/native/libglyph_engine.dylib" \
#       -Xlinker -rpath -Xlinker "$PWD/native"
#   /tmp/ffi_selftest engine/fixtures/*.pipe.bin
#
# The ABI constants are RESTATED below, not imported from ffi.mojo: a foreign
# caller pins the ABI literally, so a drift on the engine side turns this suite
# red instead of silently tracking the change.

from std.sys import argv
from std.ffi import external_call, c_int
from std.memory import bitcast
from std.time import perf_counter_ns

from glyph_pipeline import F_LEADER, Item
from glyph_schema import (
    FIXTURE_MEASURE_STRIDE, FIXTURE_COUNT_STRIDE,
    FIX_M_GLYPH_ID, FIX_C_ROW, FIX_C_COL, FIX_C_FLAGS,
)
from fixture_io import load_pipe_fixture

comptime Handle = Pointer[NoneType, MutUntrackedOrigin]

# ── THE ABI, RESTATED LITERALLY (see header) ─────────────────────────────────
comptime ITEM_DESC_SIZE: Int = 128
comptime ABI_SHAPE_OFFSET: Int = 108
comptime ABI_DESC_I32S: Int = 7
comptime ABI_SHAPE: Int = (ITEM_DESC_SIZE << 8) | ABI_DESC_I32S
comptime GE_OK: c_int = 0
comptime GE_ABI_MISMATCH: c_int = 9


def _desc_for(it: Item, byte_count: Int) raises -> List[UInt8]:
    """Serialize one Item into the 128 B descriptor both load entries take.

    A THIRD writer of this block, and that is a real cost — native/src/engine.rs's
    `write_item_desc` is the one the product uses. It is here because this suite
    exists to call the FFI the way a foreign caller would, and a foreign caller
    marshals its own block. If the layout moves, this must move with it; the
    shape word at ABI_SHAPE_OFFSET is what refuses the call if it does not."""
    var d = List[UInt8](unsafe_uninit_length=ITEM_DESC_SIZE)
    for i in range(ITEM_DESC_SIZE):
        d[i] = 0
    var f64s = d.unsafe_ptr().unsafe_bitcast[Float64]()
    var i32s = d.unsafe_ptr().unsafe_bitcast[Int32]()
    var u64s = d.unsafe_ptr().unsafe_bitcast[UInt64]()
    f64s[unsafe_offset = 0] = it.origin_x
    f64s[unsafe_offset = 1] = it.origin_y
    f64s[unsafe_offset = 2] = it.origin_z
    f64s[unsafe_offset = 3] = it.line_height
    f64s[unsafe_offset = 4] = it.z_step
    f64s[unsafe_offset = 5] = it.page_gap_x
    f64s[unsafe_offset = 6] = it.band_stride_y
    f64s[unsafe_offset = 7] = it.depth_per_band
    f64s[unsafe_offset = 8] = it.depth_per_col
    f64s[unsafe_offset = 9] = it.page_line_height
    i32s[unsafe_offset = 20] = Int32(it.wrap_width)
    i32s[unsafe_offset = 21] = Int32(1 if it.has_page else 0)
    i32s[unsafe_offset = 22] = Int32(it.page_rows)
    i32s[unsafe_offset = 23] = Int32(it.page_cols)
    i32s[unsafe_offset = 24] = Int32(it.scroll_rows)
    i32s[unsafe_offset = 25] = Int32(it.pages_wide)
    i32s[unsafe_offset = 26] = Int32(it.wrap_mode)
    i32s[unsafe_offset = ABI_SHAPE_OFFSET // 4] = Int32(ABI_SHAPE)
    u64s[unsafe_offset = 14] = UInt64(0)
    u64s[unsafe_offset = 15] = UInt64(byte_count)

    # MARSHALLING SELF-CHECK, and why it earns its place: this toolchain has
    # already miscompiled the bitcast-pointer access pattern above TWICE in two
    # compilation units (see header), so re-read the descriptor BYTE-WISE — a
    # form the offset-0 fold bug cannot reach — and refuse to run against a
    # descriptor this process did not actually write. A failure here means the
    # TEST HARNESS miscompiled, not the engine; without it the two are
    # indistinguishable from the red they print.
    var want_f64 = List[Float64]()
    want_f64.append(it.origin_x)
    want_f64.append(it.origin_y)
    want_f64.append(it.origin_z)
    want_f64.append(it.line_height)
    want_f64.append(it.z_step)
    want_f64.append(it.page_gap_x)
    want_f64.append(it.band_stride_y)
    want_f64.append(it.depth_per_band)
    want_f64.append(it.depth_per_col)
    want_f64.append(it.page_line_height)
    for j in range(10):
        var bits: UInt64 = 0
        for i in range(8):
            bits |= UInt64(d[j * 8 + i]) << UInt64(8 * i)
        if bits != UInt64(bitcast[DType.uint64](want_f64[j])):
            raise Error(
                "ffi_selftest: descriptor marshalling miscompiled at f64 lane "
                + String(j) + " — the harness, not the engine"
            )
    var want_i32 = List[Int32]()
    want_i32.append(Int32(it.wrap_width))
    want_i32.append(Int32(1 if it.has_page else 0))
    want_i32.append(Int32(it.page_rows))
    want_i32.append(Int32(it.page_cols))
    want_i32.append(Int32(it.scroll_rows))
    want_i32.append(Int32(it.pages_wide))
    want_i32.append(Int32(it.wrap_mode))
    for j in range(7):
        var bits: UInt32 = 0
        for i in range(4):
            bits |= UInt32(d[80 + j * 4 + i]) << UInt32(8 * i)
        if bits != UInt32(bitcast[DType.uint32](want_i32[j])):
            raise Error(
                "ffi_selftest: descriptor marshalling miscompiled at i32 lane "
                + String(j) + " — the harness, not the engine"
            )
    return d^


def _mut[T: AnyType, o: Origin, //](p: Pointer[T, o]) -> Pointer[T, MutUntrackedOrigin]:
    """Erase a pointer's origin for the C ABI surface (test-side mirror of
    what a foreign caller's raw pointer looks like)."""
    return p.unsafe_mut_cast[True]().unsafe_origin_cast[MutUntrackedOrigin]()


# ── the dylib's exports, via external_call ───────────────────────────────────
# The call types below ARE the C header, restated: Handle in, raw pointers and
# c_size_t across, c_int status / u64 counts back.


def _eng_new() -> Handle:
    return external_call["glyph_engine_new", Handle]()


def _eng_free(h: Handle):
    external_call["glyph_engine_free", NoneType](h)


def _eng_load_trie(h: Handle, path: String) -> c_int:
    return external_call["glyph_engine_load_trie_file", c_int](
        h, _mut(path.unsafe_ptr()), UInt(path.byte_length())
    )


def _eng_load_item(
    h: Handle, bytes: List[UInt8], desc: List[UInt8]
) -> c_int:
    return external_call["glyph_engine_load_item_desc", c_int](
        h,
        _mut(bytes.unsafe_ptr()),
        UInt(len(bytes)),
        _mut(desc.unsafe_ptr()),
    )


def _eng_slot_count(h: Handle) -> UInt64:
    return external_call["glyph_engine_slot_count", UInt64](h)


def _eng_copy_slots(h: Handle, buf: List[UInt32], cap: Int) -> UInt64:
    return external_call["glyph_engine_copy_slots", UInt64](
        h, _mut(buf.unsafe_ptr()), UInt(cap)
    )


def check_fixture(path: String) raises -> Int:
    var fx = load_pipe_fixture(path)
    if fx.item_count != 1:
        print("SKIP ", path, "(multi-item; the per-item ABI is one item per load)")
        return 0
    var it = fx.items[0].copy()

    var h = _eng_new()
    var st = _eng_load_trie(h, path)
    if st != GE_OK:
        print("FAIL ", path, ": load_trie_file status", st)
        _eng_free(h)
        return 1

    var desc_a = _desc_for(it, len(fx.bytes))
    st = _eng_load_item(h, fx.bytes, desc_a)
    if st != GE_OK:
        print("FAIL ", path, ": load_item status", st)
        _eng_free(h)
        return 1

    var n = Int(_eng_slot_count(h))
    var buf = List[UInt32](unsafe_uninit_length=n * 8)
    var copied = Int(_eng_copy_slots(h, buf, n))
    _eng_free(h)

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
    var covered = 0
    for f in fixtures:
        var fx = load_pipe_fixture(f)
        if fx.item_count == 1:
            covered += 1
        total_bad += check_fixture(f)

    # NEGATIVE PROBE: a descriptor whose shape word is wrong must be refused
    # with GE_ABI_MISMATCH — the guard bought by the 2026-09-04 arity incident,
    # exercised by nothing else. Uses the first single-item fixture's item.
    if covered > 0:
        var path = fixtures[0]
        var fx = load_pipe_fixture(path)
        var it = fx.items[0].copy()
        var h = _eng_new()
        _ = _eng_load_trie(h, path)
        var bad_desc = _desc_for(it, len(fx.bytes))
        bad_desc[ABI_SHAPE_OFFSET] = 0xFF  # corrupt the shape word
        var st = _eng_load_item(h, fx.bytes, bad_desc)
        if st != GE_ABI_MISMATCH:
            print("FAIL shape-word probe: expected GE_ABI_MISMATCH (9), got", st)
            total_bad += 1
        _eng_free(h)

        # STRESS: 1000 loads on one handle, full record copy each cycle.
        h = _eng_new()
        _ = _eng_load_trie(h, path)
        var desc_b = _desc_for(it, len(fx.bytes))
        var cap = fx.byte_len  # leaders <= bytes
        var buf = List[UInt32](unsafe_uninit_length=cap * 8)
        var t0 = perf_counter_ns()
        for i in range(1000):
            st = _eng_load_item(h, fx.bytes, desc_b)
            if st != GE_OK:
                print("FAIL stress iter", i, "status", st)
                total_bad += 1
                break
            var n = _eng_slot_count(h)
            _ = _eng_copy_slots(h, buf, Int(n))
        var dt = Float64(perf_counter_ns() - t0) / 1e9
        _eng_free(h)
        var total_mb = Float64(fx.byte_len) * 1000.0 / 1e6
        print(
            "stress: 1000 loads x", fx.byte_len, "B in", dt, "s  (",
            total_mb / dt, "MB/s through the FFI, copy included)",
        )

    if total_bad != 0:
        raise Error("ffi_selftest: FAILURES above")
    print(
        "ffi_selftest: all green —", covered,
        "single-item fixtures bit-exact through the dylib C ABI",
    )
