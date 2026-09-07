# ffi.mojo — C ABI wrapper around the glyph pipeline (Stage D smoke test).
#
# GOAL: prove the Mojo engine can be linked IN-PROCESS into the Rust+wgpu app.
# The surface is deliberately scalars + one opaque pointer (ABI-stability
# discipline): no Mojo types, no exceptions, no ownership transfer of Mojo
# containers across the boundary. Records are copied OUT into caller memory.
#
# Wire record (32 B per rendered glyph), per schema/glyph-identity.json:
#   f32 X, Y, Z, ADVANCE, HEIGHT   (20 B — render-read measures)
#   u32 GLYPH_ID, ROW, COL         (12 B — counts)
#
# Portability notes for this toolchain (Mojo 1.1.0.dev2026083005):
#   - std.runtime.asyncrt went private; the engine copies were patched to
#     the engine copies moved to max.algorithm.parallelize, the PUBLIC parallel
#     primitive (tagged MOJO-1.1-PORT). TaskGroup exists only in the private
#     std.runtime._asyncrt and has no public counterpart.
#   - std.runtime.initialize_runtime() MUST be called before any parallel
#     work when the host process is not Mojo — without it the first parallel
#     dispatch segfaults on a null async runtime (GEX-3993).
#   - Heap handles use std.memory.alloc's Allocation/Layout (Pointer.alloc is
#     gone in this nightly).

from std.collections.span import Span
from std.ffi import c_int, c_size_t, c_double
from std.memory import bitcast, Allocation
from std.memory.alloc import alloc, dealloc, Layout
from std.runtime import initialize_runtime
from std.time import perf_counter_ns

from glyph_pipeline import Item, Trie, run_pipeline, PipelineResult, F_LEADER, ST_COUNT  # NATIVE-PORT: F_LEADER for load_items per-item counts
from glyph_record import RecordSet, compact, direct_write_all, INST_U32S
from fixture_io import load_trie_auto  # NATIVE-PORT: G3DF fixture or G3TR blob


struct EngineState(Movable):
    """One engine handle: the loaded trie, plus the last item's records.

    NATIVE-PORT (Stage E1): the trie is owned DIRECTLY (it may come from a
    'G3TR' blob now, not only from a pipe fixture), instead of sitting inside
    a discarded-fixture field."""

    var trie: Trie
    var has_trie: Bool
    var records: RecordSet
    var byte_len: Int
    var leaders: Int
    # Nanoseconds for the last load, ST_* indexed, plus two lanes this entry
    # owns that run_pipeline knows nothing about. They exist because the Rust
    # side's `fold` timer brackets THIS WHOLE CALL, and the 2026-09-07 numbers
    # showed most of it is not the fold: `compact` repacks the wire stream into
    # the engine arena, and EG_COUNTS is a SERIAL O(bytes) walk of the flag
    # lanes to attribute records to items. Without these lanes, both are
    # invisible and get read as fold cost.
    var stage_ns: List[Int]

    def __init__(out self):
        self.trie = Trie(List[UInt32](), List[Float32](), List[UInt32]())
        self.has_trie = False
        self.records = RecordSet()
        self.byte_len = 0
        self.leaders = 0
        self.stage_ns = List[Int](length=EG_STAGE_COUNT, fill=0)


# Engine-side stage lanes: run_pipeline's ST_* first, then this entry's own.
comptime EG_COMPACT = ST_COUNT       # repack the wire stream into the arena
comptime EG_COUNTS = ST_COUNT + 1    # per-item record counts — SERIAL O(bytes)
comptime EG_DIRECT = ST_COUNT + 2    # the direct write: lanes straight to instances
comptime EG_STAGE_COUNT = ST_COUNT + 3

comptime Handle = Pointer[NoneType, MutUntrackedOrigin]

# Status codes (c_int): keep them greppable.
comptime GE_OK: c_int = 0
comptime GE_NULL_HANDLE: c_int = 1
comptime GE_NO_TRIE: c_int = 2
comptime GE_RAISED: c_int = 3
comptime GE_EMPTY: c_int = 4
comptime GE_ABI_MISMATCH: c_int = 9
# The direct path's own failure: the caller's arena cannot hold what the fold
# produced. LOUD, because the alternative is writing past it. The caller sizes
# from the leader count, which it does not know until the fold has run, so it
# reserves the byte count as the upper bound; this fires only if that bound is
# ever wrong, which would be a fold defect worth stopping for.
comptime GE_ARENA_TOO_SMALL: c_int = 10
"""The descriptor carries a shape word this dylib does not recognise — the caller
was built against a different FFI surface. See ABI_SHAPE."""


# ── ONE MARSHALLING FORMAT, AND WHY ──────────────────────────────────────────
#
# `glyph_engine_load_item` used to take its 20 params POSITIONALLY. Adding one —
# `wrap_mode`, 2026-09-04 — shifts every argument after it by a register on the
# caller's side, so a binary built against the new signature and linked against a
# dylib built from the old source handed `wrap_mode` to `has_page`, `has_page` to
# `page_rows`, and so on. Pagination silently switched OFF and every paged item
# laid out somewhere else.
#
# THAT IS NOT AN EXOTIC STATE. `cargo build` does not build this dylib —
# native/build.rs says in as many words that it is built outside cargo, by
# `pixi run build-engine` — so a stale dylib is a normal condition of the tree.
# check-all's gate 0 rebuilds it for exactly this reason; an ad-hoc
# `cargo build && ./glyph3d-native ...` does not. Measured 2026-09-04 by building
# this file at its pre-wrap-mode arity and leaving the Rust side alone:
# `--repo-verify` reported "item 2 placement differs", the per-item path
# unpaginated at ~58,000 rows against the batched path's 127, and the pick
# oracle's fold cross-check failed on 43,901 of 44,001 records. Loud symptoms,
# silent cause, all far from the seam that produced them.
#
# TWO GUARDS WERE TRIED AND MEASURED USELESS BEFORE THIS ONE, which is the part
# worth keeping:
#   1. a separate `glyph_engine_abi_probe()` returning a hand-written constant —
#      green under the stale dylib, because its numbers were declarations rather
#      than consequences of the signature;
#   2. a shape word as the LAST positional parameter, on the theory that an arity
#      change would misalign it — also green, measured: the shift was consumed
#      among the register-passed ints in the MIDDLE of the list and the trailing
#      argument still arrived intact (`abi_shape got 268896010 want 268896010`
#      while `has_page` got 0 and `page_rows` got 1).
# A positional argument list cannot self-describe. A sentinel is either before the
# insertion point, where it never moves, or after it, where it may not either.
#
# So the per-item entry stopped being positional. Both load entries now marshal
# the SAME 128 B descriptor block, and a new field can never shift an argument
# again — it takes pad. The block carries ABI_SHAPE at a FIXED offset (108), which
# cannot shift by construction, and the entry point was RENAMED
# (`glyph_engine_load_item_desc`) so a dylib predating this change fails to LINK
# rather than being miscalled: a symbol that does not exist cannot be called wrong.
comptime ABI_DESC_BYTES: Int = 128             # the descriptor block, both entries
comptime ABI_DESC_I32S: Int = 7                # i32 params inside that block
comptime ABI_SHAPE_OFFSET: Int = 108           # byte offset of the shape word
comptime ABI_SHAPE: Int = (ABI_DESC_BYTES << 8) | ABI_DESC_I32S


def _state(h: Handle) -> Pointer[EngineState, MutUntrackedOrigin]:
    return h.unsafe_bitcast[EngineState]()


@export("glyph_engine_new")
def glyph_engine_new() abi("C") -> Handle:
    # MANDATORY when the host process is not Mojo: creates the async runtime
    # that run_pipeline's parallelize shards dispatch onto.
    initialize_runtime()
    var a = alloc(Layout[EngineState](count=1))
    a.unsafe_ptr().unsafe_write(EngineState())
    return a^.unsafe_leak().unsafe_bitcast[NoneType]()


@export("glyph_engine_free")
def glyph_engine_free(h: Handle) abi("C"):
    var p = _state(h)
    var v = p.unsafe_take_pointee()
    _ = v^  # drops trie + records (frees their Lists)
    dealloc(
        Allocation[EngineState](
            unsafe_owned_ptr=p, layout=Layout[EngineState](count=1)
        )
    )


@export("glyph_engine_load_trie_file")
def glyph_engine_load_trie_file(
    h: Handle, path_ptr: Pointer[UInt8, MutUntrackedOrigin], path_len: c_size_t
) abi("C") -> c_int:
    """Load the trie (font metric tables). NATIVE-PORT (Stage E1): dispatches
    on magic — a 'G3TR' blob (the app atlas's real codepoint→slot mapping,
    tools/gen_real_trie.py) or a legacy 'G3DF' .pipe.bin fixture (the
    conformance corpus; its expected-output sections are parsed and discarded).
    """
    try:
        var span = Span[UInt8, ImmUntrackedOrigin](
            unsafe_ptr=path_ptr, length=Int(path_len)
        )
        var path = String(unsafe_from_utf8=span)
        _state(h)[].trie = load_trie_auto(path)
        _state(h)[].has_trie = True
        return GE_OK
    except:
        return GE_RAISED


def _item_from_desc(
    base: Pointer[UInt8, MutUntrackedOrigin], byte_start: Int, byte_count: Int
) -> Item:
    """Deserialize ONE 128 B descriptor block into an Item.

    THE ONLY PLACE either entry point reads item params from. Two readers is how
    the per-item and batched paths get to disagree about a field, which is what
    `--repo-verify` exists to catch and what it did catch on 2026-09-04."""
    var f64s = base.unsafe_bitcast[Float64]()
    var i32s = base.unsafe_bitcast[Int32]()
    var it = Item()
    it.origin_x = f64s[unsafe_offset = 0]
    it.origin_y = f64s[unsafe_offset = 1]
    it.origin_z = f64s[unsafe_offset = 2]
    it.line_height = f64s[unsafe_offset = 3]
    it.z_step = f64s[unsafe_offset = 4]
    it.page_gap_x = f64s[unsafe_offset = 5]
    it.band_stride_y = f64s[unsafe_offset = 6]
    it.depth_per_band = f64s[unsafe_offset = 7]
    it.depth_per_col = f64s[unsafe_offset = 8]
    it.page_line_height = f64s[unsafe_offset = 9]
    it.wrap_width = Int(i32s[unsafe_offset = 20])
    it.has_page = i32s[unsafe_offset = 21] != 0
    it.page_rows = Int(i32s[unsafe_offset = 22])
    it.page_cols = Int(i32s[unsafe_offset = 23])
    it.scroll_rows = Int(i32s[unsafe_offset = 24])
    it.pages_wide = Int(i32s[unsafe_offset = 25])
    it.wrap_mode = Int(i32s[unsafe_offset = 26])  # offset 104
    it.byte_start = byte_start
    it.byte_count = byte_count
    return it^


def _desc_shape_ok(base: Pointer[UInt8, MutUntrackedOrigin]) -> Bool:
    """The shape word at a FIXED offset — it cannot shift, which is the whole
    point after two guards that could."""
    return (
        Int(base.unsafe_bitcast[Int32]()[unsafe_offset = ABI_SHAPE_OFFSET // 4])
        == ABI_SHAPE
    )


@export("glyph_engine_load_item_desc")
def glyph_engine_load_item_desc(
    h: Handle,
    bytes_ptr: Pointer[UInt8, MutUntrackedOrigin],
    byte_len: c_size_t,
    desc_ptr: Pointer[UInt8, MutUntrackedOrigin],
) abi("C") -> c_int:
    """Run decode → fold → paginate → compact for ONE item (one text file).

    Params arrive in the SAME 128 B descriptor block the batched entry takes —
    see the ONE MARSHALLING FORMAT note above for why this stopped being a
    twenty-argument positional call. Results are kept in the handle as 32 B wire
    records; retrieve with glyph_engine_slot_count / glyph_engine_copy_slots."""
    if not _desc_shape_ok(desc_ptr):
        return GE_ABI_MISMATCH
    initialize_runtime()  # idempotent; guards against free/new reordering
    var s = _state(h)
    if not s[].has_trie:
        return GE_NO_TRIE
    s[].records.glyphs = 0  # reuse the arena across loads
    var n = Int(byte_len)
    s[].byte_len = n
    if n == 0:
        s[].leaders = 0
        return GE_EMPTY

    var items = List[Item]()
    items.append(_item_from_desc(desc_ptr, 0, n))

    # The span borrows the CALLER's buffer; run_pipeline only reads it, and the
    # Rust side holds the Vec alive for the whole call.
    var span = Span[UInt8, ImmUntrackedOrigin](unsafe_ptr=bytes_ptr, length=n)
    # Elided instantiation: production form — no witness tier, render-read
    # arrays bit-identical to the witnessed form (conformance_elide pins it).
    var r = run_pipeline[witness=False](span, s[].trie, items)
    s[].leaders = r.leaders
    var _c = perf_counter_ns()
    compact(r, n, r.leaders, s[].records)
    _record_stages(s[], r, perf_counter_ns() - _c, 0, 0)
    _ = len(items)
    return GE_OK


# NATIVE-PORT (Stage E2) — batched load: ONE call runs the whole corpus.
#
# Per-file load_item calls pay the parallel dispatch + scratch setup per
# call (~50 MB/s at repo-file sizes); the pipeline itself is built for
# multi-item arenas (Item list over one byte span), so this entry amortizes
# ALL per-call overhead: the caller concatenates the corpus into one blob and
# passes one 128-byte descriptor block per item, and gets back one record
# stream plus per-item record counts (computed from the flag lanes, so they
# are EXACT: one record per leader byte, as compact emits).
#
# Descriptor block layout (128 B, little-endian, serialized EXPLICITLY on both
# sides — no repr(C) guessing):
#   0..80    ten f64: origin_x, origin_y, origin_z, line_height, z_step,
#            page_gap_x, band_stride_y, depth_per_band, depth_per_col,
#            page_line_height
#   80..108  SEVEN i32: wrap_width, has_page, page_rows, page_cols,
#            scroll_rows, pages_wide, wrap_mode
#   108      u32 ABI_SHAPE — a FIXED-offset shape word; it cannot shift, unlike
#            a positional sentinel (two of which were measured useless first)
#
# The wrap mode landed in the pad rather than growing the block: 10 f64 +
# 7 i32 + 2 u64 is 124 B, so ITEM_DESC_SIZE stays 128 and the Rust side's
# `const _: () = assert!(...)` in write_item_desc pins that it still fits.
#   112      u64 byte_start
#   120      u64 byte_count
#
# Items must be contiguous and ascending by byte_start (the pipeline's
# documented requirement). Per-item ordinals stay per item, so the 2^24-byte
# ordinal wall remains a PER-ITEM bound, not a per-blob one.

comptime ITEM_DESC_SIZE: Int = ABI_DESC_BYTES


def _record_stages(
    mut st: EngineState, r: PipelineResult,
    compact_ns: Int, counts_ns: Int, direct_ns: Int,
):
    """Fold run_pipeline's own stage lanes into the handle, then add the three
    this file owns. Kept as one writer so the lane layout has a single author.

    The lanes are per-ENTRY, not shared: the record path fills compact+counts
    and the direct path fills direct. An entry that left them merged would put
    two different costs under one name, which is the defect this whole
    instrumentation line exists to stop making."""
    for i in range(ST_COUNT):
        st.stage_ns[i] = r.stage_ns[i]
    st.stage_ns[EG_COMPACT] = compact_ns
    st.stage_ns[EG_COUNTS] = counts_ns
    st.stage_ns[EG_DIRECT] = direct_ns


@export("glyph_engine_load_items")
def glyph_engine_load_items(
    h: Handle,
    blob_ptr: Pointer[UInt8, MutUntrackedOrigin],
    blob_len: c_size_t,
    desc_ptr: Pointer[UInt8, MutUntrackedOrigin],
    item_count: c_size_t,
    counts_out: Pointer[UInt64, MutUntrackedOrigin],
) abi("C") -> c_int:
    """Run decode → fold → paginate → compact for N items in ONE call.
    counts_out receives item_count u64s: records (= leaders) per item, in
    descriptor order. Retrieve the stream with glyph_engine_copy_slots."""
    # Same fixed-offset shape word as the per-item entry, read from the FIRST
    # descriptor. The batched path was immune to the arity shift that broke the
    # other one — it always marshalled a block — and that asymmetry is what made
    # the failure look like a layout bug: the two strategies disagreed because
    # one of them was fine.
    if item_count > 0 and not _desc_shape_ok(desc_ptr):
        return GE_ABI_MISMATCH
    initialize_runtime()
    var s = _state(h)
    if not s[].has_trie:
        return GE_NO_TRIE
    s[].records.glyphs = 0  # reuse the arena across loads
    var n = Int(blob_len)
    var m = Int(item_count)
    s[].byte_len = n
    if n == 0 or m == 0:
        s[].leaders = 0
        return GE_EMPTY

    var items = List[Item]()
    for i in range(m):
        var base = desc_ptr.unsafe_offset(i * ITEM_DESC_SIZE)
        var u64s = base.unsafe_bitcast[UInt64]()
        items.append(
            _item_from_desc(
                base,
                Int(u64s[unsafe_offset = 14]),  # offset 112
                Int(u64s[unsafe_offset = 15]),  # offset 120
            )
        )

    var span = Span[UInt8, ImmUntrackedOrigin](unsafe_ptr=blob_ptr, length=n)
    var r = run_pipeline[witness=False](span, s[].trie, items)
    s[].leaders = r.leaders
    var _c = perf_counter_ns()
    compact(r, n, r.leaders, s[].records)
    var compact_ns = perf_counter_ns() - _c

    # Per-item record counts: one pass over the flag lanes, advancing the item
    # cursor at descriptor boundaries (items are contiguous + ascending).
    var _k = perf_counter_ns()
    for i in range(m):
        counts_out[unsafe_offset = i] = 0
    var it_i = 0
    var it_end = Int(desc_ptr.unsafe_bitcast[UInt64]()[unsafe_offset = 14]) + Int(
        desc_ptr.unsafe_bitcast[UInt64]()[unsafe_offset = 15]
    )
    for id in range(n):
        while id >= it_end and it_i + 1 < m:
            it_i += 1
            var base = desc_ptr.unsafe_offset(it_i * ITEM_DESC_SIZE)
            it_end = Int(base.unsafe_bitcast[UInt64]()[unsafe_offset = 14]) + Int(base.unsafe_bitcast[UInt64]()[unsafe_offset = 15])
        if (Int(r.fl[id]) & F_LEADER) != 0:
            counts_out[unsafe_offset = it_i] += 1
    _record_stages(s[], r, compact_ns, perf_counter_ns() - _k, 0)
    _ = len(items)
    return GE_OK


# Placement block, u32 lanes with the float fields bitcast: the engine half of
# ItemPlacement. 16 rather than the 14 it needs, so the block stays 64 B and a
# future lane costs no ABI change.
comptime PLACE_U32S = 16
comptime PL_SLOT_BASE = 0
comptime PL_SLOT_COUNT = 1
comptime PL_RECORD_COUNT = 2
comptime PL_PAGE_RIGHT = 4
comptime PL_PAGE_BOTTOM = 5
comptime PL_PAGE_ZMIN = 6
comptime PL_PAGE_ZMAX = 7
comptime PL_INK_MIN = 8   # 3 lanes
comptime PL_INK_MAX = 11  # 3 lanes


@export("glyph_engine_instance_shape")
def glyph_engine_instance_shape() abi("C") -> UInt64:
    """(instance u32 lanes << 32) | placement u32 lanes.

    The host asserts this against its own `size_of::<GlyphInstance>()` before
    handing over an arena pointer. Same discipline as the per-item descriptor's
    shape word (`cc814b3`): a dylib that disagrees about the layout should be a
    loud refusal, not a silently mis-strided buffer. The Stage G strided-colour
    bug is precisely this class, and it was found by pixels rather than by a
    check."""
    return (UInt64(INST_U32S) << 32) | UInt64(PLACE_U32S)


@export("glyph_engine_load_items_direct")
def glyph_engine_load_items_direct(
    h: Handle,
    blob_ptr: Pointer[UInt8, MutUntrackedOrigin],
    blob_len: c_size_t,
    desc_ptr: Pointer[UInt8, MutUntrackedOrigin],
    item_count: c_size_t,
    inst_ptr: Pointer[UInt32, MutUntrackedOrigin],
    inst_cap: c_size_t,
    paint_ptrs: Pointer[Pointer[UInt32, ImmUntrackedOrigin], MutUntrackedOrigin],
    flat_colors: Pointer[UInt32, MutUntrackedOrigin],
    group_ids: Pointer[UInt32, MutUntrackedOrigin],
    place_out: Pointer[UInt32, MutUntrackedOrigin],
) abi("C") -> c_int:
    """Fold N items and write RENDER INSTANCES straight into the caller's arena.

    THE POINT, measured: `glyph_engine_load_items` + `copy_slots` +
    `compact_records_into` walk the record stream three times and account for
    87% of a batched load's backend time (2026-09-07, 47.1 MB). This entry does
    the same filtering and the same arithmetic in ONE pass and never
    materializes a wire record at all — no engine arena, no FFI copy, no host
    repack.

    It does not retire the other entries. They are the verification form, and
    `--repo-verify` diffs this against them bit-for-bit at the seam, which is
    what makes a second writer of one truth safe rather than a liability."""
    if item_count > 0 and not _desc_shape_ok(desc_ptr):
        return GE_ABI_MISMATCH
    initialize_runtime()
    var s = _state(h)
    if not s[].has_trie:
        return GE_NO_TRIE
    s[].records.glyphs = 0
    var n = Int(blob_len)
    var m = Int(item_count)
    s[].byte_len = n
    if n == 0 or m == 0:
        s[].leaders = 0
        return GE_EMPTY

    var items = List[Item]()
    for i in range(m):
        var base = desc_ptr.unsafe_offset(i * ITEM_DESC_SIZE)
        var u64s = base.unsafe_bitcast[UInt64]()
        items.append(
            _item_from_desc(
                base,
                Int(u64s[unsafe_offset = 14]),
                Int(u64s[unsafe_offset = 15]),
            )
        )

    var span = Span[UInt8, ImmUntrackedOrigin](unsafe_ptr=blob_ptr, length=n)
    var r = run_pipeline[witness=False](span, s[].trie, items)
    s[].leaders = r.leaders
    if r.leaders > Int(inst_cap):
        return GE_ARENA_TOO_SMALL

    var _w = perf_counter_ns()
    var places = direct_write_all(
        r, items, group_ids, paint_ptrs, flat_colors, inst_ptr
    )
    # slot_base is re-derived here rather than returned, because the writer's
    # own prefix is over GRAINS and this one is over items: the same running
    # sum, read at a different granularity. Deriving it twice from one source
    # (each item's slot_count) beats carrying a second copy that could drift.
    var slot_base = 0
    for i in range(m):
        ref p = places[i]
        var o = i * PLACE_U32S
        place_out[unsafe_offset = o + PL_SLOT_BASE] = UInt32(slot_base)
        place_out[unsafe_offset = o + PL_SLOT_COUNT] = UInt32(p.slot_count)
        place_out[unsafe_offset = o + PL_RECORD_COUNT] = UInt32(p.record_count)
        place_out[unsafe_offset = o + PL_PAGE_RIGHT] = bitcast[DType.uint32](p.page_right)
        place_out[unsafe_offset = o + PL_PAGE_BOTTOM] = bitcast[DType.uint32](p.page_bottom)
        place_out[unsafe_offset = o + PL_PAGE_ZMIN] = bitcast[DType.uint32](p.page_z_min)
        place_out[unsafe_offset = o + PL_PAGE_ZMAX] = bitcast[DType.uint32](p.page_z_max)
        for k in range(3):
            place_out[unsafe_offset = o + PL_INK_MIN + k] = bitcast[DType.uint32](p.ink_min[k])
            place_out[unsafe_offset = o + PL_INK_MAX + k] = bitcast[DType.uint32](p.ink_max[k])
        slot_base += p.slot_count
    _record_stages(s[], r, 0, 0, perf_counter_ns() - _w)
    _ = len(items)
    return GE_OK


@export("glyph_engine_stage_ns")
def glyph_engine_stage_ns(
    h: Handle,
    out_ptr: Pointer[UInt64, MutUntrackedOrigin],
    cap: c_size_t,
) abi("C") -> c_size_t:
    """Per-stage nanoseconds for the LAST load. Writes min(cap, EG_STAGE_COUNT)
    lanes and returns how many it wrote, so the caller learns the engine's lane
    count rather than assuming it — a stale dylib then reports fewer lanes
    instead of scribbling past the buffer.

    This is the engine half of the attribution the Rust side does with
    BackendPhases. It exists because `fold` on that side brackets this whole
    call, and measurement showed the fold is the minority of it."""
    var st = _state(h)
    var k = Int(cap)
    if k > EG_STAGE_COUNT:
        k = EG_STAGE_COUNT
    for i in range(k):
        out_ptr[unsafe_offset = i] = UInt64(st[].stage_ns[i])
    return c_size_t(k)


@export("glyph_engine_slot_count")
def glyph_engine_slot_count(h: Handle) abi("C") -> UInt64:
    """Number of 32 B wire records from the last load_item (= rendered glyphs)."""
    return UInt64(_state(h)[].records.glyphs)


@export("glyph_engine_copy_slots")
def glyph_engine_copy_slots(
    h: Handle, out_ptr: Pointer[UInt32, MutUntrackedOrigin], out_len: c_size_t
) abi("C") -> UInt64:
    """Copy up to out_len records into out_ptr as packed 32 B records:
    [f32 X Y Z ADVANCE HEIGHT][u32 GLYPH_ID ROW COL]. Returns records written.
    The u32 view is deliberate: the f32 lanes cross the boundary as BITS,
    so no float reformatting can happen at the seam."""
    var s = _state(h)
    var n = s[].records.glyphs
    if Int(out_len) < n:
        n = Int(out_len)
    var mp = s[].records.measures.unsafe_ptr()
    var cp = s[].records.counts.unsafe_ptr()
    for i in range(n):
        var mo = i * 5
        var co = i * 3
        var wo = i * 8
        for k in range(5):
            out_ptr[unsafe_offset = wo + k] = bitcast[DType.uint32](
                mp[unsafe_offset = mo + k]
            )
        for k in range(3):
            out_ptr[unsafe_offset = wo + 5 + k] = cp[unsafe_offset = co + k]
    return UInt64(n)


@export("glyph_engine_fp_probe")
def glyph_engine_fp_probe(a: Float32, b: Float32, c: Float32) abi("C") -> UInt32:
    """NATIVE-PORT (2026-09-02): report whether THIS DYLIB was built with
    `--fp-mode contract=off`, by doing the one thing the flag governs.

    WHY THIS EXISTS. The flag is load-bearing (README-FFI.md, build.rs,
    check.sh's header all say so) and until now NOTHING COULD DETECT ITS
    ABSENCE. check.sh's own header admits the Mojo suites pass either way; the
    Rust `--engine-check` is blind for a separate reason, measured 2026-09-02:
    it runs with origin (0,0,0), which makes its only fusable multiply-add
    (`-row*lh + oy`) FMA-invariant, and the pagination terms that DO have a
    nonzero addend never execute under its params. Even with a nonzero origin
    it would stay blind — the fold computes in Float64 and narrows to Float32,
    and an FMA/non-FMA difference at f64 ulp survives that narrowing only when
    the result lands within ~2^-53 of an f32 rounding boundary (~2^-29 per
    record). That is a lottery, not a gate.

    So: stop hoping a corpus notices, and ask the compiler directly. `a * b + c`
    is exactly the shape contraction fuses. The operands are PARAMETERS, not
    literals, so the expression cannot be constant-folded at compile time — the
    fusion decision is made in emitted code, which is the thing under test.

    Caller passes a = b = 0x3f800002 (1.0 + 2 ulp), c = -1.0 and expects:
        0x35000000  built with contract=off  (product rounded, then added)
        0x35000001  built with contract=fast (single rounding — FORBIDDEN)
    One ulp apart, deterministic, no fixtures involved."""
    return UInt32((a * b + c).to_bits())
