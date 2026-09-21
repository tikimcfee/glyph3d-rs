# glyph_record.mojo — the record format, the scratch pool it makes possible, and
# the direct write that skips the record entirely.
#
# THE PROBLEM. A source byte costs slot lanes held for the corpus's entire
# lifetime. But the render path reads only the render-read prefixes; BASE_X is
# FOLD SCRATCH (paginate's input), and LINE_ADV/ORD/ord_to_byte are WITNESS
# TIER - the read-axis split moved them out of the render-read arrays entirely,
# and run_streaming below runs the fold ELIDED (witness=False), so they are
# never even written here. The arena otherwise pays corpus-scale memory for
# job-scale temporaries, on every byte.
#
# THE PREFIX RULE MAKES EMIT CHEAP. The schema orders render-read lanes FIRST
# in every phase array, so emitting a record is a concatenation of THREE runs
# (posMeasures 0-3, staticMeasures 0-3, posCounts WHOLE) - a truncation per
# array, never a gather through a lane map. Checked twice over: gen-schema.mjs
# refuses a schema where a render-read lane sorts after an unread one, and pins
# the wire order as a literal so a container re-layout cannot move the bytes.
#
# THE DECOUPLING, AND IT IS NO LONGER A DEMONSTRATION. Once records exist, slots
# become a SCRATCH POOL sized to the JOB, not the corpus. run_streaming below
# proved it for the record path (and conformance_record runs it). The direct
# path SHIPS it: `glyph_engine_load_items_direct` folds in chunks through one
# reused set of lanes, so corpus size stops determining lane memory. Measured
# 2026-09-07 on 47.1 MB, peak RSS 4.088 -> 2.694 GB; on 151.8 MB, where the old
# form was thrashing, 32.0 -> 130.0 MB/s. It is also what makes streaming edits
# a range re-run rather than a reload.
#
# THE DIRECT WRITE lives here too, below the record machinery it bypasses:
# `direct_write_all` is a count/prefix/scatter over grains that turns per-byte
# fold lanes into 48 B render instances in ONE pass, writing into an arena the
# CALLER owns. `compact` above is the same filter and the same arithmetic
# materializing a 32 B wire record first; it remains as the verification form,
# and `repo-verify-direct` diffs the two.

from std.collections.span import Span
from std.math import inf
from std.memory import bitcast, unsafe_memcpy
from glyph_schema import (
    SM_STRIDE, LM_STRIDE, LC_STRIDE,
    RECORD_MEASURE_STRIDE, RECORD_COUNT_STRIDE, RECORD_BYTES,
)
from glyph_pipeline import (
    Item, Trie, run_pipeline, F_LEADER, PipelineResult, BOUNDS_GRAIN,
)
from max.algorithm import parallelize
from std.time import perf_counter_ns


struct RecordSet(Copyable, Movable):
    """Resident render state: one record per RENDERED glyph, not per source byte."""

    var measures: List[Float32]  # glyphs × RECORD_MEASURE_STRIDE
    var counts: List[UInt32]  # glyphs × RECORD_COUNT_STRIDE
    var glyphs: Int
    var cap: Int

    def __init__(out self):
        self.measures = List[Float32]()
        self.counts = List[UInt32]()
        self.glyphs = 0
        self.cap = 0

    def reserve(mut self, want_glyphs: Int):
        """Size the arena. Call this ONCE with the best estimate you have — the
        leader count is bounded above by the byte count, which a caller holding a
        manifest already knows.

        Sizing once is the whole trick. An earlier attempt grew geometrically and
        memcpy'd on every growth, and lost to plain append; it was recorded here as
        a negative result, and that conclusion was WRONG. Measured over the
        dictionary corpus (693 files, 83.4 M glyphs), compaction only, bit-identical
        output: append 1889 ms, pre-sized arena 951 ms — 1.99x. The regrowth copies
        were the cost, not the appends."""
        if want_glyphs <= self.cap:
            return
        var c = want_glyphs if want_glyphs > self.cap * 2 else self.cap * 2
        var m = List[Float32](unsafe_uninit_length=c * RECORD_MEASURE_STRIDE)
        var k = List[UInt32](unsafe_uninit_length=c * RECORD_COUNT_STRIDE)
        if self.glyphs > 0:
            unsafe_memcpy(dest=m.unsafe_ptr(), src=self.measures.unsafe_ptr(),
                   count=self.glyphs * RECORD_MEASURE_STRIDE)
            unsafe_memcpy(dest=k.unsafe_ptr(), src=self.counts.unsafe_ptr(),
                   count=self.glyphs * RECORD_COUNT_STRIDE)
        self.measures = m^
        self.counts = k^
        self.cap = c


def compact(
    r: PipelineResult, byte_len: Int, leaders: Int,
    mut out: RecordSet,
):
    """KERNEL — thread per byte in the GPU form: leaders emit a record, others don't.

    Serial here because the append order IS the ordinal order, and making that
    deterministic is worth more than the parallelism (the same reason the miss
    rebuild is a serial pass). On the GPU this is a prefix-sum over the leader
    flag plus a scatter, which the scan machinery already computes."""
    # `leaders` comes from the caller's PipelineResult — it is already computed,
    # and recomputing it here costs a pass over the whole flags array.
    out.reserve(out.glyphs + leaders)
    var mp = out.measures.unsafe_ptr()
    var cp = out.counts.unsafe_ptr()
    var w = out.glyphs
    for id in range(byte_len):
        if (Int(r.fl[id]) & F_LEADER) == 0:
            continue
        # THE TRUNCATION, after both splits and the settlement: FOUR runs,
        # still no lane map, and STILL THE SAME BYTES — GLYPH_ID moved from the
        # measure run's tail to the exact run's head, which is the same offset
        # (20) either way. Runs: posMeasures' render-read prefix, staticMeasures
        # whole, staticIdentities whole, posCounts whole. gen-schema pins the
        # order as a literal and fails the build if it stops deriving.
        var wm = w * RECORD_MEASURE_STRIDE
        var lo = id * LM_STRIDE
        var so = id * SM_STRIDE
        for k in range(3):
            mp[unsafe_offset = wm + k] = r.lm[lo + k]          # X, Y, Z
        for k in range(2):
            mp[unsafe_offset = wm + 3 + k] = r.sm[so + k]      # ADVANCE, HEIGHT
        var wc = w * RECORD_COUNT_STRIDE
        var co = id * LC_STRIDE
        cp[unsafe_offset = wc + 0] = r.gi[id]                  # GLYPH_ID — the exact
        for k in range(2):                                     # run's head, offset 20:
            cp[unsafe_offset = wc + 1 + k] = r.lc[co + k]      # same wire BYTE as ever
        w += 1
    out.glyphs = w


def run_streaming[o: ImmOrigin](
    bytes: Span[UInt8, o], trie: Trie, items: List[Item], chunk_bytes: Int
) raises -> RecordSet:
    """The decoupling, demonstrated: lay one item at a time through a scratch pool
    that is never larger than the biggest item, and keep only records.

    `chunk_bytes` is the pool's ceiling. An item larger than it is still laid whole
    — the fold is sequential within an item, so an item is the true unit of work.
    Splitting one further needs the bake's checkpoints (prefix_at resumes mid-file
    from a saved state, already conformance-proven), which is the next step and not
    this function's job."""
    var out = RecordSet()
    for i in range(len(items)):
        var one = List[Item]()
        var it = items[i].copy()
        var span = it.byte_count
        var sub = Item()
        sub.byte_start = 0
        sub.byte_count = span
        sub.origin_x = it.origin_x
        sub.origin_y = it.origin_y
        sub.origin_z = it.origin_z
        sub.wrap_width = it.wrap_width
        sub.wrap_mode = it.wrap_mode
        # Every per-item field must move — a hand-copy that drops one is how
        # this path laid out unclustered while the whole-corpus lay resolved
        # (caught by conformance_record's stream-vs-whole diff on cluster-zwj).
        sub.cluster_mode = it.cluster_mode
        sub.z_step = it.z_step
        sub.line_height = it.line_height
        sub.has_page = it.has_page
        sub.page_rows = it.page_rows
        sub.page_cols = it.page_cols
        sub.scroll_rows = it.scroll_rows
        sub.pages_wide = it.pages_wide
        sub.page_gap_x = it.page_gap_x
        sub.band_stride_y = it.band_stride_y
        sub.depth_per_band = it.depth_per_band
        sub.depth_per_col = it.depth_per_col
        sub.page_line_height = it.page_line_height
        one.append(sub^)

        # ZERO COPY: a view into the caller's buffer, not a copy of it.
        var slice = bytes[it.byte_start : it.byte_start + span]

        # The scratch: allocated per job, dropped at the end of this iteration —
        # and ELIDED: no witness tier is written at all. compact() reads only
        # render-read arrays + flags, and conformance_record pins the records
        # byte-identical to the witnessed whole-corpus lay, so this is the
        # elision's production proof, not an unverified fast path.
        var r = run_pipeline[witness=False](slice, trie, one)
        compact(r, span, r.leaders, out)
        _ = chunk_bytes
    return out^


# ── Mid-item resume ─────────────────────────────────────────────────────────
#
# run_streaming above lays one ITEM at a time. That already decouples the arena
# from the corpus, but an edit still re-lays a whole file. To re-lay a RANGE we
# need the fold's accumulators at an arbitrary byte, which is exactly what the
# bake's checkpoints are for: prefix_at is "nearest checkpoint + a <= K tail fold",
# already bit-exact across the bake suite.
#
# The monoid carries the two pure-count accumulators (base_row, ord); col,
# seg_adv and line_adv cannot ride it (see seed_at's docstring for why).
# It deliberately does NOT carry line_adv, and that omission is correct: line_adv
# is an f64 chain, and f64 addition is not associative, so putting it in a monoid
# that gets regrouped in parallel would drift. (This is the same reason segAdv is
# summed f32-per-add.)
#
# It doesn't need to be carried, because line_adv RESETS at every newline — it is a
# per-LINE accumulator, not a per-item one. So recovering it exactly costs a re-sum
# over the current partial line, bounded by line length rather than file length.

from glyph_bake import BakeRecord, prefix_at, lanes_from_prefix
from glyph_pipeline import (
    LayoutSeed, sequence_length, decode_codepoint_at, NEWLINE,
)


def is_line_start[o: ImmOrigin](bytes: Span[UInt8, o], at: Int) -> Bool:
    """Byte 0, or the byte immediately after a newline."""
    if at == 0:
        return True
    if at > len(bytes):
        return False
    var j = at - 1
    while j >= 0:
        var n = sequence_length(bytes, j)
        if n > 0:
            return decode_codepoint_at(bytes, j, n) == NEWLINE and j + n == at
        j -= 1
    return False


def seed_at[o: ImmOrigin](
    bytes: Span[UInt8, o], trie: Trie, record: BakeRecord, wrap: Int, at: Int,
    base_row_hint: Int = -1,
) raises -> LayoutSeed:
    """The fold state at a LINE START — checkpoint lookup, nothing re-summed.

    WHY LINE STARTS. The fold carries five accumulators. The monoid carries the two
    that are pure counts (base_row, ord). It cannot carry the other three:

      line_adv  an f64 chain — f64 addition is not associative, so a monoid that
                gets regrouped in parallel would drift (the same reason segAdv is
                summed f32-per-add rather than in f64).
      seg_adv   resets at every FOLD boundary, and fold is wrap-or-page-cols — a
                QUERY parameter. The bake is deliberately wrap-agnostic, so a
                per-segment sum cannot live in it.
      col       likewise per-line.

    At a line start all three are zero BY DEFINITION, so the monoid carries
    everything that is left and the seed needs no re-summing at all. That is not a
    workaround: a line is the natural resume unit, it is what the arena's own
    design study already proposed ("large files chunked at newline boundaries"),
    and it is how edits actually arrive.

    Resuming mid-line is possible — re-sum line_adv from the line start and seg_adv
    from the segment start, both bounded spans — but it buys nothing an edit needs
    and costs two more things that can silently disagree with the fold.

    WHY base_row_hint EXISTS, and it is a real limit rather than an oversight.
    `scan_combine` accumulates `rows` using the wrap carried in the element, and
    `bake_file` bakes at wrap = 0 ON PURPOSE — wrap is a QUERY parameter, which is
    exactly why `rows_under_wrap` is a separate histogram query rather than a field.
    So for an UNWRAPPED item (fold == 0) the prefix's row count is exact and the
    bake alone can seed a resume. For a wrapped or paged item it cannot, because
    the answer depends on a wrap the bake deliberately does not know.

    Rather than return a plausible wrong number, the caller must supply the row.
    That is not a burden in practice: anything re-laying a range has already laid
    the document once, and the row of the line being edited is in the previous
    layout. Passing -1 with wrap > 0 raises instead of guessing."""
    if not is_line_start(bytes, at):
        raise Error("seed_at: resume points must be line starts (byte 0 or after a newline)")
    var seed = LayoutSeed()
    var prefix = prefix_at(bytes, trie, record, at)
    var lanes = lanes_from_prefix(prefix, wrap)
    # col / seg_adv / line_adv are all zero at a line start; only the counts carry.
    seed.ord = lanes.ord
    var folds = wrap > 0
    if base_row_hint >= 0:
        seed.base_row = base_row_hint
    elif folds:
        raise Error(
            "seed_at: a wrapped item needs base_row_hint — the bake is wrap-agnostic"
            " by design, so its prefix row count is only exact at wrap 0"
        )
    else:
        seed.base_row = lanes.row
    return seed^


# ── THE DIRECT WRITE: fold output straight to render instances ───────────────
#
# WHY THIS EXISTS. `compact` above materializes a 32 B wire record per glyph in
# the engine's arena; the host then copies that whole stream across the FFI and
# repacks it into 48 B instances. Measured 2026-09-07 on a 47.1 MB corpus, those
# three passes are 87% of a batched load's backend time and the layout
# computation is 3%. This function is the same filter and the same arithmetic
# with the intermediate representations removed: one pass, per-byte lanes in,
# render instances out.
#
# It does NOT replace `compact`. The wire record is what the corpus gates and
# the pick path are written against, so it stays as the verification form —
# exactly the relationship `witness` already has with the elided fold. Two
# writers of one truth is a hazard only if nothing adjudicates them; here
# `--repo-verify` diffs the strategies bit-for-bit at the seam.
#
# THE LAYOUT IS THE HOST'S. Twelve 4-byte fields, `GlyphInstance` in
# glyph_scene.rs, mirroring `InstanceSlot` in glyph_field.wgsl. It is written
# here as u32 lanes with the float fields bitcast, because the alternative — a
# struct defined twice — is the correlated-fault shape this tree keeps finding.
# glyph_engine_instance_shape() lets the caller assert the agreement rather than
# assume it.

comptime F32_INF = inf[DType.float32]()

# Grain scratch lanes for the direct write's boxes. Flat Float32 rather than a
# struct per grain: a struct with Lists heap-allocates inside the parallel
# region, which measured slower than not parallelizing at all.
comptime DP_PAGE_RIGHT = 0
comptime DP_PAGE_BOTTOM = 1
comptime DP_PAGE_ZMIN = 2
comptime DP_PAGE_ZMAX = 3
comptime DP_INK_MIN = 4   # 3 lanes
comptime DP_INK_MAX = 7   # 3 lanes
comptime DP_LANES = 10

# The direct write's own phases, ns, written into the caller's lane block. These
# REPLACE a single `eg_direct` total rather than sitting beside it, so the engine
# line still sums to the FFI call and `unattributed` stays meaningful.
comptime DW_BUILD = 0     # decompose items into grains
comptime DW_COUNT = 1     # pass A, parallel
comptime DW_PREFIX = 2    # the serial scan over grains
comptime DW_WRITE = 3     # pass B, parallel — the scatter
comptime DW_MERGE = 4     # fold grain boxes into item placements
comptime DW_LANES = 5

comptime INST_U32S = 12          # 48 B / 4
comptime INST_POS_X = 0
comptime INST_POS_Y = 1
comptime INST_POS_Z = 2
comptime INST_GLYPH_ID = 3
comptime INST_ROW = 4
comptime INST_COL = 5
comptime INST_COLOR = 6
comptime INST_GROUP_ID = 7
comptime INST_ADVANCE = 8
comptime INST_HEIGHT = 9
comptime INST_FLAGS = 10
comptime INST_PAD = 11


struct DirectPlacement(Copyable, Movable):
    """What one item's direct write produced — the engine half of
    `ItemPlacement`. Page is seeded at the ORIGIN over ALL records; ink is
    seeded EMPTY over survivors only. Those seeds are not interchangeable and
    the host learned that the hard way (`d6f33ff`), so they are stated here
    rather than left to whoever reads the loop."""

    var slot_count: Int
    var record_count: Int
    var page_right: Float32
    var page_bottom: Float32
    var page_z_min: Float32
    var page_z_max: Float32
    var ink_min: List[Float32]
    var ink_max: List[Float32]

    def __init__(out self):
        self.slot_count = 0
        self.record_count = 0
        self.page_right = 0.0
        self.page_bottom = 0.0
        self.page_z_min = 0.0
        self.page_z_max = 0.0
        self.ink_min = List[Float32](length=3, fill=F32_INF)
        self.ink_max = List[Float32](length=3, fill=-F32_INF)


def count_direct_range(r: PipelineResult, start: Int, stop: Int) -> Tuple[Int, Int]:
    """(records, survivors) over the byte range — PASS A of the scan/scatter.

    Counting before writing is what buys the parallel write: a grain cannot know
    where its instances go until every grain before it has said how many it
    produces, and that prefix is over grains, not items, so a single big file
    parallelizes too. Two passes over the flag lanes beat one serial pass as
    soon as there is more than one core, which is the same trade the bounds pass
    already made (`BOUNDS_GRAIN`, worth 2.11x on a heavy-tailed batch)."""
    # HOISTED. `r.fl[id]` is a List indexing: it reloads the List's data
    # pointer on every byte, and this loop runs once per source byte. Taking the
    # two pointers before the loop is the whole of the change, and it is worth
    # 2.4x on this pass alone (47 MB: 24.0 ms -> 10.1 ms, 2026-09-07).
    var flp = r.fl.unsafe_ptr()
    var gip = r.gi.unsafe_ptr()
    var rec = 0
    var surv = 0
    for id in range(start, stop):
        if (Int(flp[unsafe_offset = id]) & F_LEADER) == 0:
            continue
        rec += 1
        if gip[unsafe_offset = id] != 0:
            surv += 1
    return (rec, surv)


def write_instances_direct[bo: Origin[mut=True]](
    r: PipelineResult,
    group_id: UInt32,
    paint: Pointer[UInt32, ImmUntrackedOrigin],
    has_paint: Bool,
    flat_color: UInt32,
    out_ptr: Pointer[UInt32, MutUntrackedOrigin],
    out_slot_base: Int,
    byte_start: Int,
    byte_stop: Int,
    paint_base: Int,
    box_ptr: Pointer[Float32, bo],
    box_base: Int,
) -> Tuple[Int, Int]:
    """One BYTE RANGE's records, written as render instances into `out_ptr`
    starting at instance `out_slot_base`. Returns the counts and both extents.

    The range is a parameter rather than the item's whole span because the
    caller decomposes items into grains: a 22.9 MB file and a 1 KB file are both
    one item, and a writer that took items would put the big one on the critical
    path while other cores idled — measured on the linux tree, which has 9,584
    files under 1 KB and one of 22.9 MB.

    `paint_base` is where this range starts in its ITEM's colour array, since
    paint is indexed per item and a grain is not an item.

    THE FILTER IS THE HOST'S, restated: a record whose GLYPH_ID is 0 updates the
    page extent and is then dropped — it occupies a cell but inks nothing. The
    two extents therefore run over different sets, which is why they are
    accumulated in one pass rather than derived from each other.

    `paint` is indexed by RECORD, not by surviving instance. That indexing is
    the reason the blank check happens after the paint lookup would have: the
    host's colorize_leaders emits one colour per leader including blanks, and
    an index that skipped them would tint every glyph after the first blank."""
    # HOISTED, and this is what makes the pass bandwidth-bound rather than
    # indirection-bound. Every `r.<lane>[i]` below was a List indexing — a
    # reload of the List's data pointer per access, in a loop that runs once per
    # source byte. Taking the five pointers once turns the body into five
    # strided loads and a 48 B store, which is what the traffic model says it
    # should be. Measured 2026-09-07 over 47.0 MB, five interleaved samples per
    # arm: `eg_direct` 204.5 ms -> 117.8 ms median (1.74x), spread 196.7-206.3
    # against 114.6-122.2. `pass B` alone 180.0 -> 107.3 ms, which is 38.7 GB/s
    # of read+write traffic against 39.8 GB/s for a hand-written C kernel of the
    # same shape on four threads — i.e. at the memory system's rate, not above
    # it and no longer well below it.
    #
    # Hoisting the ink extents out of `DirectPlacement`'s two Lists as well was
    # measured SEPARATELY and is worth nothing (118.6 vs 117.8 ms median, inside
    # the spread): those Lists are re-read every glyph and stay in L1, while the
    # lane arrays stream. They are left as they were, because a change that
    # buys nothing should not be carried as if it did.
    var flp = r.fl.unsafe_ptr()
    var gip = r.gi.unsafe_ptr()
    var lmp = r.lm.unsafe_ptr()
    var smp = r.sm.unsafe_ptr()
    var lcp = r.lc.unsafe_ptr()

    var p = DirectPlacement()
    var w = out_slot_base
    var rec = paint_base

    for id in range(byte_start, byte_stop):
        if (Int(flp[unsafe_offset = id]) & F_LEADER) == 0:
            continue
        var lo = id * LM_STRIDE
        var so = id * SM_STRIDE
        var x = lmp[unsafe_offset = lo + 0]
        var y = lmp[unsafe_offset = lo + 1]
        var z = lmp[unsafe_offset = lo + 2]
        var adv = smp[unsafe_offset = so + 0]
        var hgt = smp[unsafe_offset = so + 1]
        var gid = gip[unsafe_offset = id]

        # Page: over ALL records, seeded at the origin.
        var right = x + adv
        if right > p.page_right:
            p.page_right = right
        if y < p.page_bottom:
            p.page_bottom = y
        if z < p.page_z_min:
            p.page_z_min = z
        if z > p.page_z_max:
            p.page_z_max = z

        var color = paint[unsafe_offset = rec] if has_paint else flat_color
        rec += 1
        if gid == 0:
            continue

        # Ink: over SURVIVORS, seeded empty, on the QUAD not the baseline.
        var half = hgt * 0.5
        if x < p.ink_min[0]:
            p.ink_min[0] = x
        if right > p.ink_max[0]:
            p.ink_max[0] = right
        if y - half < p.ink_min[1]:
            p.ink_min[1] = y - half
        if y + half > p.ink_max[1]:
            p.ink_max[1] = y + half
        # Depth is a point, not a span: a glyph quad has no thickness.
        if z < p.ink_min[2]:
            p.ink_min[2] = z
        if z > p.ink_max[2]:
            p.ink_max[2] = z

        var co = id * LC_STRIDE
        var o = w * INST_U32S
        out_ptr[unsafe_offset = o + INST_POS_X] = bitcast[DType.uint32](x)
        out_ptr[unsafe_offset = o + INST_POS_Y] = bitcast[DType.uint32](y)
        out_ptr[unsafe_offset = o + INST_POS_Z] = bitcast[DType.uint32](z)
        out_ptr[unsafe_offset = o + INST_GLYPH_ID] = gid
        out_ptr[unsafe_offset = o + INST_ROW] = lcp[unsafe_offset = co + 0]
        out_ptr[unsafe_offset = o + INST_COL] = lcp[unsafe_offset = co + 1]
        out_ptr[unsafe_offset = o + INST_COLOR] = color
        out_ptr[unsafe_offset = o + INST_GROUP_ID] = group_id
        out_ptr[unsafe_offset = o + INST_ADVANCE] = bitcast[DType.uint32](adv)
        out_ptr[unsafe_offset = o + INST_HEIGHT] = bitcast[DType.uint32](hgt)
        out_ptr[unsafe_offset = o + INST_FLAGS] = 0
        out_ptr[unsafe_offset = o + INST_PAD] = 0
        w += 1

    box_ptr[unsafe_offset = box_base + DP_PAGE_RIGHT] = p.page_right
    box_ptr[unsafe_offset = box_base + DP_PAGE_BOTTOM] = p.page_bottom
    box_ptr[unsafe_offset = box_base + DP_PAGE_ZMIN] = p.page_z_min
    box_ptr[unsafe_offset = box_base + DP_PAGE_ZMAX] = p.page_z_max
    for k in range(3):
        box_ptr[unsafe_offset = box_base + DP_INK_MIN + k] = p.ink_min[k]
        box_ptr[unsafe_offset = box_base + DP_INK_MAX + k] = p.ink_max[k]
    return (rec - paint_base, w - out_slot_base)


def direct_write_all[po: Origin[mut=True]](
    r: PipelineResult,
    items: List[Item],
    group_ids: Pointer[UInt32, MutUntrackedOrigin],
    paint_ptrs: Pointer[Pointer[UInt32, ImmUntrackedOrigin], MutUntrackedOrigin],
    paint_lens: Pointer[UInt64, MutUntrackedOrigin],
    flat_colors: Pointer[UInt32, MutUntrackedOrigin],
    out_ptr: Pointer[UInt32, MutUntrackedOrigin],
    mut paint_bad: Int,
    prof: Pointer[Int, po],
) -> List[DirectPlacement]:
    """THE DIRECT WRITE, grained and parallel: count, prefix, scatter.

    The serial form this replaces walked every item in order because each item's
    `slot_base` depends on how many instances every earlier item produced. That
    dependency is a prefix sum, not a sequence, and the standard answer is to
    pay one cheap counting pass to break it — which is also, word for word, what
    `compact`'s docstring says the GPU form of this is ("a prefix-sum over the
    leader flag plus a scatter"). Doing it this way on the CPU means the device
    version is a port rather than a redesign.

    GRAIN, NOT ITEM. Decomposing by item is size-blind, and this tree already
    measured what that costs (see BOUNDS_GRAIN's note: 2.11x on a heavy-tailed
    batch). Grains also mean a SINGLE large file parallelizes, which item-level
    decomposition can never do and which `fold_profile` showed is a real
    ceiling — 8 MB as one item folds at 164 MB/s against 661 as 64.

    Every grain writes its own disjoint scratch and both merges are serial. That
    is not caution about ordering: min/max being exact under regrouping says
    nothing about a concurrent read-modify-write on a shared location, which is
    a race an earlier form of the bounds pass actually had.

    PAINT IS BOUNDS-CHECKED HERE, between the count and the scatter, because
    that is the first moment the record count exists and the last moment before
    anything reads the caller's colour array. The record path has always
    asserted this on the host (`compact_records_into`: "a silent fallback here
    could paint a whole file the wrong colour for a decade"); dropping it on
    this path would have been worse than what that assert prevents, since here
    the failure is an out-of-bounds READ from parallel tasks rather than a wrong
    colour. `paint_bad` comes back set to the offending item + 1; the caller
    turns it into a status."""
    var item_count = len(items)
    var placements = List[DirectPlacement]()
    if item_count == 0:
        return placements^

    # Phase timing is ALWAYS ON, five `perf_counter_ns` calls against a pass that
    # runs in milliseconds. The lanes ACCUMULATE rather than assign, because the
    # caller drives this once per CHUNK: assigning made every lane report the
    # last chunk only, which read as a 30x speedup and left the rest in
    # `unattributed` — a number meaning something other than its name, found by
    # the unattributed lane it was hiding in. The alternative considered and rejected was an
    # env-var profile flag: it would have been a second code path through the
    # hottest loop in the load, reachable from no verb, and off by default —
    # which is how an instrument becomes one nobody runs.
    var _t0 = perf_counter_ns()

    # ── grains, in item order so a grain's index locates it in both tables ──
    var gr_at = List[Int]()
    var gr_end = List[Int]()
    var gr_item = List[Int]()
    for i in range(item_count):
        var start = items[i].byte_start
        var stop = start + items[i].byte_count
        var at = start
        # An empty item still gets one grain, so every item has a slot_base and
        # the merge below never reads an empty range.
        if at >= stop:
            gr_at.append(at)
            gr_end.append(at)
            gr_item.append(i)
            continue
        while at < stop:
            var end = at + BOUNDS_GRAIN
            if end > stop:
                end = stop
            gr_at.append(at)
            gr_end.append(end)
            gr_item.append(i)
            at = end
    var n_gr = len(gr_at)
    prof[unsafe_offset = DW_BUILD] += perf_counter_ns() - _t0
    var _t = perf_counter_ns()

    # ── pass A: count, in parallel ──────────────────────────────────────────
    var gr_rec = List[Int](length=n_gr, fill=0)
    var gr_surv = List[Int](length=n_gr, fill=0)
    var rp = gr_rec.unsafe_ptr()
    var sp = gr_surv.unsafe_ptr()
    def _count_task(t: Int) {imm}:
        var rs = count_direct_range(r, gr_at[t], gr_end[t])
        rp[unsafe_offset = t] = rs[0]
        sp[unsafe_offset = t] = rs[1]
    parallelize(_count_task, n_gr)
    prof[unsafe_offset = DW_COUNT] += perf_counter_ns() - _t
    _t = perf_counter_ns()

    # ── the prefix: serial over GRAINS, which is O(items + bytes/grain) and
    # not O(bytes). Slots run across the whole corpus; paint indices restart at
    # each item, because the host's colour array is per item.
    var gr_slot = List[Int](length=n_gr, fill=0)
    var gr_paint = List[Int](length=n_gr, fill=0)
    var slot_at = 0
    var paint_at = 0
    var prev_item = -1
    for t in range(n_gr):
        if gr_item[t] != prev_item:
            paint_at = 0
            prev_item = gr_item[t]
        gr_slot[t] = slot_at
        gr_paint[t] = paint_at
        slot_at += gr_surv[t]
        paint_at += gr_rec[t]

    prof[unsafe_offset = DW_PREFIX] += perf_counter_ns() - _t
    _t = perf_counter_ns()

    # Paint is indexed by RECORD and the host sizes it per item, so the check is
    # per item against the record total the counting pass just produced.
    for i in range(item_count):
        if Int(paint_ptrs[unsafe_offset = i]) == 0:
            continue          # flat colour: no array to overrun
        var need = 0
        for t in range(n_gr):
            if gr_item[t] == i:
                need += gr_rec[t]
        if UInt64(need) > paint_lens[unsafe_offset = i]:
            paint_bad = i + 1
            return placements^

    # ── pass B: scatter, in parallel, into slots nothing else can touch ─────
    var gbox = List[Float32](unsafe_uninit_length=n_gr * DP_LANES)
    var bp = gbox.unsafe_ptr()
    # The scatter's core invariant, ASSERTED rather than assumed: the count pass
    # and the write pass apply the same predicate over the same immutable range,
    # so a grain must write exactly the survivors it counted. If it ever writes
    # fewer, `commit` publishes slots nothing wrote — uninitialized memory as
    # glyphs. `wrote_bad` carries the first offender out; one integer compare per
    # grain turns a silent corruption into a refusal.
    var wrote = List[Int](length=n_gr, fill=0)
    var wp = wrote.unsafe_ptr()
    def _write_task(t: Int) {imm}:
        var i = gr_item[t]
        var pp = paint_ptrs[unsafe_offset = i]
        var rs = write_instances_direct(
            r, group_ids[unsafe_offset = i], pp, Int(pp) != 0,
            flat_colors[unsafe_offset = i], out_ptr, gr_slot[t],
            gr_at[t], gr_end[t], gr_paint[t], bp, t * DP_LANES,
        )
        wp[unsafe_offset = t] = rs[1]
    parallelize(_write_task, n_gr)
    prof[unsafe_offset = DW_WRITE] += perf_counter_ns() - _t
    _t = perf_counter_ns()
    for t in range(n_gr):
        if wrote[t] != gr_surv[t]:
            paint_bad = -(t + 1)   # negative: a scatter fault, not a paint one
            return placements^
    _ = len(gr_slot)
    _ = len(gr_paint)

    # ── serial merge: a grain's boxes fold into its item's ──────────────────
    for _ in range(item_count):
        placements.append(DirectPlacement())
    for t in range(n_gr):
        var i = gr_item[t]
        var b = t * DP_LANES
        placements[i].record_count += gr_rec[t]
        placements[i].slot_count += gr_surv[t]
        if gbox[b + DP_PAGE_RIGHT] > placements[i].page_right:
            placements[i].page_right = gbox[b + DP_PAGE_RIGHT]
        if gbox[b + DP_PAGE_BOTTOM] < placements[i].page_bottom:
            placements[i].page_bottom = gbox[b + DP_PAGE_BOTTOM]
        if gbox[b + DP_PAGE_ZMIN] < placements[i].page_z_min:
            placements[i].page_z_min = gbox[b + DP_PAGE_ZMIN]
        if gbox[b + DP_PAGE_ZMAX] > placements[i].page_z_max:
            placements[i].page_z_max = gbox[b + DP_PAGE_ZMAX]
        for k in range(3):
            if gbox[b + DP_INK_MIN + k] < placements[i].ink_min[k]:
                placements[i].ink_min[k] = gbox[b + DP_INK_MIN + k]
            if gbox[b + DP_INK_MAX + k] > placements[i].ink_max[k]:
                placements[i].ink_max[k] = gbox[b + DP_INK_MAX + k]
    prof[unsafe_offset = DW_MERGE] += perf_counter_ns() - _t
    return placements^
