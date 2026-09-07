# fold_profile.mojo — THE FOLD INSTRUMENT: where run_pipeline's time goes.
#
# WHY THIS EXISTS. `engine/BACKEND-PLAN.md` item 4 asks for an honest fold
# measurement, and the 2026-09-07 readback split made it the expensive question:
# the CPU fold is 58-77% of a load's backend time, so the fold IS the load. What
# nothing could answer was which of its seven stages that time is in.
#
# WHAT IT IS NOT. It asserts nothing — it is an instrument in the sense
# fixture_census is, and it prints for a human. But it runs (engine/check.sh:100:
# "an instrument nothing runs is not a parked instrument, it is an absent one"),
# and the stage timers live inside run_pipeline rather than being reconstructed
# here on purpose. A profiler that rebuilds the stages profiles a second
# implementation and can be wrong in ways the real one is not.
#
# WHAT IT COMPARES. Two things the plan has been reasoning about by argument:
#
#   1. THE STAGE SPLIT of run_pipeline, which is the fold the FFI actually
#      ships (`ffi.mojo` calls run_pipeline, twice, at :236 and :320).
#   2. run_pipeline VS run_scan_pipeline over the SAME corpus. These are two
#      conformance-proven-equivalent forms with different parallel structure:
#      the serial form parallelizes ACROSS items (one core per item), the scan
#      form shards WITHIN an item by chunk. The existing GPU benchmark
#      (gpu_pipeline.mojo --bench) compares the device against the SCAN form,
#      which is not what ships — so its CPU baseline has never been the
#      shipped fold.
#
# THE ITEM SHAPE IS THE DIAL, and it is why this takes a count rather than
# reading one file. Splitting the same bytes across N items changes nothing
# about the work and everything about the serial form's parallelism, so a
# sweep over N separates "the fold is expensive" from "the fold is serial".
#
# Run:
#   mojo run -I engine --fp-mode contract=off engine/fold_profile.mojo \
#       engine/fixtures/repo-file.pipe.bin <file> [file ...]

from std.sys import argv
from std.time import perf_counter_ns
from glyph_pipeline import (
    run_pipeline, Item, Trie, PipelineResult, WRAP_BACK,
    ST_ALLOC, ST_GAPSWEEP, ST_DECODE, ST_MISSCAT, ST_FOLD, ST_PAGINATE,
    ST_BOUNDS, ST_COUNT,
)
from glyph_scan import run_scan_pipeline
from fixture_io import load_pipe_fixture

comptime CHUNK = 256
comptime GROUP = 256
def stage_name(i: Int) -> String:
    """ST_* index to label. A list rather than a lookup table because the
    indices are comptime and this runs seven times per shape."""
    if i == ST_ALLOC:
        return "alloc"
    if i == ST_GAPSWEEP:
        return "gapsweep"
    if i == ST_DECODE:
        return "decode"
    if i == ST_MISSCAT:
        return "misscat"
    if i == ST_FOLD:
        return "fold"
    if i == ST_PAGINATE:
        return "paginate"
    return "bounds"


def split_items(byte_len: Int, n_items: Int) -> List[Item]:
    """The same bytes as `n_items` contiguous items, ascending — the shape
    run_pipeline requires and the shape a repo load produces. Params are fixed
    across the sweep so the only thing that varies is the item COUNT."""
    var items = List[Item]()
    var per = (byte_len + n_items - 1) // n_items
    var at = 0
    for _ in range(n_items):
        if at >= byte_len:
            break
        var it = Item()
        it.byte_start = at
        it.byte_count = per if at + per <= byte_len else byte_len - at
        it.line_height = 1.1
        it.origin_x = 2.0
        it.origin_y = 3.0
        it.origin_z = 0.5
        it.z_step = 0.2
        it.wrap_width = 120
        it.wrap_mode = WRAP_BACK
        items.append(it^)
        at += per
    return items^


def profile(bytes: List[UInt8], trie: Trie, n_items: Int) raises:
    var n = len(bytes)
    var mb = Float64(n) / 1048576.0
    var items = split_items(n, n_items)

    # The SHIPPED form, witness elided — ffi.mojo runs run_pipeline[witness=False].
    var t = perf_counter_ns()
    var r = run_pipeline[witness=False](Span(bytes), trie, items)
    var serial_ns = perf_counter_ns() - t

    # The scan form over the same bytes and the same items.
    t = perf_counter_ns()
    var sr = run_scan_pipeline(Span(bytes), trie, items, CHUNK, GROUP)
    var scan_ns = perf_counter_ns() - t

    # Agreement is not this instrument's job (conformance_real owns it), but a
    # leader-count mismatch would mean the two runs did different work and every
    # ratio below would be meaningless. Cheap, so check it.
    if r.leaders != sr.leaders:
        raise Error("forms disagree on leader count — the comparison is void")

    print("  items", n_items, " serial", Float64(serial_ns) / 1e6, "ms =",
          mb / (Float64(serial_ns) / 1e9), "MB/s   |   scan",
          Float64(scan_ns) / 1e6, "ms =", mb / (Float64(scan_ns) / 1e9),
          "MB/s   |   scan/serial x", Float64(scan_ns) / Float64(serial_ns))

    var attributed = 0
    for i in range(ST_COUNT):
        attributed += r.stage_ns[i]
    print("    serial stages:", end="")
    for i in range(ST_COUNT):
        print(" ", stage_name(i), Float64(r.stage_ns[i]) / 1e6, "ms (",
              100.0 * Float64(r.stage_ns[i]) / Float64(serial_ns), "% )", end="")
    # Unattributed is the self-check: the seven stages must be the whole call.
    # A gap means a timer brackets the wrong span, which is not visible from
    # reading the placement.
    print("  | unattributed", Float64(serial_ns - attributed) / 1e6, "ms")


def main() raises:
    var args = argv()
    if len(args) < 3:
        print("usage: fold_profile <trie-fixture.pipe.bin> <file> [file ...]")
        return
    var fx = load_pipe_fixture(String(args[1]))

    # One corpus, concatenated — a repo load is one blob of many items, and
    # measuring one file at a time would measure per-call overhead instead.
    var bytes = List[UInt8]()
    var files = 0
    for i in range(2, len(args)):
        try:
            var f = open(String(args[i]), "r")
            var b = f.read_bytes()
            f.close()
            for k in range(len(b)):
                bytes.append(b[k])
            files += 1
        except:
            pass
    if len(bytes) == 0:
        print("fold-profile: no readable input")
        return

    print("fold-profile:", files, "files,", len(bytes), "bytes concatenated")
    for n_items in [1, 8, 64, 512, 4096]:
        if n_items > len(bytes):
            break
        profile(bytes, fx.trie, n_items)
    print("fold-profile: done —", files, "files,", len(bytes),
          "bytes, 5 item shapes, serial vs scan (asserts nothing by design)")
