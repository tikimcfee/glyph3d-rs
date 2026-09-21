# fixture_io.mojo — shared loader for the .pipe.bin conformance fixtures.
#
# One Reader (little-endian, packed) + the 'G3DF' pipeline-fixture parse, shared by
# conformance.mojo (oracle-form) and conformance_scan.mojo (scan-form) so the two
# runners can never drift on the format. Format spec: engine/fixtures/gen.mjs.

from std.memory import bitcast
# FIXTURE strides, not container strides: the on-disk format is frozen at 8+4
# per byte regardless of how the engine lays its working buffers.
from glyph_schema import FIXTURE_MEASURE_STRIDE, FIXTURE_COUNT_STRIDE
from glyph_pipeline import Trie, Item, trunc_nonneg, BLOCK_SHIFT, WRAP_DOWN, WRAP_BACK
from glyph_pipeline import CLUSTER_LEADER, CLUSTER_CLUSTER

comptime PIPE_MAGIC = 0x46443347
# NATIVE-PORT (Stage E1): the app's own trie blob — magic 'G3TR'. Spec: the
# header of tools/gen_real_trie.py. Same in-memory Trie as a fixture, but the
# blocks cross as the web trie's container (GlyphTrie.js): GLYPH_ID/FLAGS are
# native u32, ADVANCE/HEIGHT are bitcast f32 WORLD units (font units ×
# cellHeightWorld / emHeightFu, rounded once by the generator). This loader
# does the carrier split the fixture loader does per-entry.
comptime TRIE_MAGIC = 0x52543347


struct Reader(Movable):
    var data: List[UInt8]
    var at: Int

    def __init__(out self, var data: List[UInt8]):
        self.data = data^
        self.at = 0

    def u32(mut self) -> UInt32:
        var v = (
            Int(self.data[self.at])
            | (Int(self.data[self.at + 1]) << 8)
            | (Int(self.data[self.at + 2]) << 16)
            | (Int(self.data[self.at + 3]) << 24)
        )
        self.at += 4
        return UInt32(v)

    def u64(mut self) -> UInt64:
        var lo = UInt64(self.u32())
        var hi = UInt64(self.u32())
        return lo | (hi << 32)

    def f32(mut self) -> Float32:
        return bitcast[DType.float32](self.u32())

    def f64(mut self) -> Float64:
        return bitcast[DType.float64](self.u64())

    def take_bytes(mut self, n: Int) -> List[UInt8]:
        var out = List[UInt8](capacity=n)
        var i = 0
        while i < n:
            out.append(self.data[self.at + i])
            i += 1
        self.at += n
        return out^


struct PipeFixture(Movable):
    """One 'G3DF' case: inputs (bytes, trie, items) + the oracle's expected outputs."""
    var byte_len: Int
    var item_count: Int
    var bytes: List[UInt8]
    var trie: Trie
    var items: List[Item]
    var exp_leaders: Int
    var exp_misses: List[UInt32]
    var exp_ord_to_byte: List[UInt32]
    var exp_measures: List[Float64]  # VALUES (f64 carrier)
    var exp_counts: List[UInt32]     # EXACT — counts have no carrier question
    var exp_item_bounds: List[UInt64]
    var exp_batch: List[UInt64]

    def __init__(out self):
        self.byte_len = 0
        self.item_count = 0
        self.bytes = List[UInt8]()
        self.trie = Trie(List[UInt32](), List[Float32](), List[UInt32](), List[UInt32](), List[UInt32]())
        self.items = List[Item]()
        self.exp_leaders = 0
        self.exp_misses = List[UInt32]()
        self.exp_ord_to_byte = List[UInt32]()
        self.exp_measures = List[Float64]()
        self.exp_counts = List[UInt32]()
        self.exp_item_bounds = List[UInt64]()
        self.exp_batch = List[UInt64]()


def load_pipe_fixture(path: String) raises -> PipeFixture:
    var f = open(path, "r")
    var raw = f.read_bytes()
    f.close()
    var r = Reader(raw^)

    if Int(r.u32()) != PIPE_MAGIC:
        raise Error(path + ": bad magic (not a .pipe.bin fixture)")
    if Int(r.u32()) != 5:
        raise Error(path + ": unknown fixture version (expected v5 — regenerate)")

    var fx = PipeFixture()
    fx.byte_len = Int(r.u32())
    fx.item_count = Int(r.u32())
    var block_index_len = Int(r.u32())
    var blocks_len = Int(r.u32())

    fx.bytes = r.take_bytes(fx.byte_len)
    var block_index = List[UInt32](capacity=block_index_len)
    for _ in range(block_index_len):
        block_index.append(r.u32())
    # v2 stores trie blocks as f64 VALUES in entry-major lane order
    # [GLYPH_ID, ADVANCE, HEIGHT, FLAGS] — which is why the corpus survived the
    # trie's container moving on BOTH sides of the oracle: the format carries
    # values, and each loader realizes its own container. This one splits by
    # carrier: measures to f32 (exact for anything that was f32 to begin with),
    # the identity and bitfield to native u32.
    var entries = blocks_len // 4
    var blocks_m = List[Float32](capacity=entries * 2)
    var blocks_c = List[UInt32](capacity=entries * 2)
    for _ in range(entries):
        var gid = r.f64()
        var adv = r.f64()
        var h = r.f64()
        var fl = r.f64()
        blocks_m.append(Float32(adv))
        blocks_m.append(Float32(h))
        blocks_c.append(UInt32(gid))
        blocks_c.append(UInt32(fl))
    # v5: the sequence payload, between the blocks and the item records. It
    # rides the Trie, exactly like the v2 blob's sections — one carrier shape
    # for both sources.
    var seq_count = Int(r.u32())
    var seq_max = Int(r.u32())
    var bitmap_advance = r.f64()
    if seq_count == 0 and bitmap_advance == bitmap_advance:
        raise Error(path + ": seq payload with 0 entries must carry a NaN advance")
    if seq_count > 0 and bitmap_advance != bitmap_advance:
        raise Error(path + ": seq payload with entries must carry a finite advance")
    var seq = List[UInt32](capacity=seq_count * (2 + seq_max))
    for _ in range(seq_count):
        seq.append(r.u32())  # slot
        seq.append(r.u32())  # len
        for _ in range(seq_max):
            seq.append(r.u32())  # cps, 0-padded
    fx.trie = Trie(block_index^, blocks_m^, blocks_c^, seq^, List[UInt32]())
    fx.trie.bitmap_advance = Float32(bitmap_advance)
    fx.trie.seq_max = seq_max

    for _ in range(fx.item_count):
        var it = Item()
        it.byte_start = Int(r.u32())
        it.byte_count = Int(r.u32())
        it.origin_x = r.f64()
        it.origin_y = r.f64()
        it.origin_z = r.f64()
        # THE BOUNDARY: v2+ carries item params as f64 VALUES; the five integer
        # page-geometry params truncate HERE, once, instead of at every read.
        it.wrap_width = trunc_nonneg(r.f64())
        # v4: the wrap MODE, beside the wrap because it is the same kind of
        # parameter — item-level, never per line. Refused rather than defaulted:
        # an out-of-range code is malformed input, and folding it to WRAP_DOWN is
        # how a generator's typo becomes an invisible layout.
        var mode_raw = r.f64()
        it.wrap_mode = Int(mode_raw)
        if it.wrap_mode != WRAP_DOWN and it.wrap_mode != WRAP_BACK:
            raise Error(
                path + ": wrap mode must be 0 (WrapDown) or 1 (WrapBack), got "
                + String(mode_raw)
            )
        # v5: the cluster MODE, beside the wrap mode — same kind, same rule.
        var cluster_raw = r.f64()
        it.cluster_mode = Int(cluster_raw)
        if it.cluster_mode != CLUSTER_LEADER and it.cluster_mode != CLUSTER_CLUSTER:
            raise Error(
                path + ": cluster mode must be 0 (leader) or 1 (cluster), got "
                + String(cluster_raw)
            )
        it.z_step = r.f64()
        it.line_height = r.f64()
        it.has_page = r.f64() > 0.5
        it.page_rows = trunc_nonneg(r.f64())
        it.page_cols = trunc_nonneg(r.f64())
        it.scroll_rows = trunc_nonneg(r.f64())
        it.pages_wide = trunc_nonneg(r.f64())
        it.page_gap_x = r.f64()
        it.band_stride_y = r.f64()
        it.depth_per_band = r.f64()
        it.depth_per_col = r.f64()
        it.page_line_height = r.f64()
        fx.items.append(it.copy())

    fx.exp_leaders = Int(r.u32())
    var miss_count = Int(r.u32())
    for _ in range(miss_count):
        fx.exp_misses.append(r.u32())
    for _ in range(fx.byte_len):
        fx.exp_ord_to_byte.append(r.u32())
    for _ in range(fx.byte_len * FIXTURE_MEASURE_STRIDE):
        fx.exp_measures.append(r.f64())
    for _ in range(fx.byte_len * FIXTURE_COUNT_STRIDE):
        fx.exp_counts.append(r.u32())
    for _ in range(fx.item_count * 8):
        fx.exp_item_bounds.append(r.u64())
    for _ in range(8):
        fx.exp_batch.append(r.u64())
    return fx^


def load_trie_blob(path: String) raises -> Trie:
    """NATIVE-PORT (Stage E1): load a 'G3TR' trie blob — the app atlas's REAL
    codepoint→slot mapping, written by tools/gen_real_trie.py from
    assets/atlas/codepoints.bin. Returns the same Trie the fixture loader
    produces; the on-disk difference is only the container (identity/bitfield
    native u32, measures bitcast f32) and the absence of expected-output
    sections."""
    var f = open(path, "r")
    var raw = f.read_bytes()
    f.close()
    var r = Reader(raw^)

    if Int(r.u32()) != TRIE_MAGIC:
        raise Error(path + ": bad magic (not a .bin G3TR trie blob)")
    var version = Int(r.u32())
    if version != 1 and version != 2:
        raise Error(path + ": unknown trie blob version (expected 1 or 2)")
    var header_bytes = Int(r.u32())
    if version == 1 and header_bytes != 44:
        raise Error(path + ": unexpected header size (expected 44)")
    if version == 2 and header_bytes != 68:
        raise Error(path + ": unexpected v2 header size (expected 68)")
    var block_shift = Int(r.u32())
    var block_index_len = Int(r.u32())
    var block_count = Int(r.u32())
    var entry_stride = Int(r.u32())
    if block_shift != BLOCK_SHIFT or entry_stride != 4:
        raise Error(path + ": trie shape mismatch (blockShift/entryStride)")
    var _mapped_count = r.u32()  # informational
    var _primary_upem = r.u32()  # informational
    var _em_height_fu = r.u32()  # informational — the conversion denominator
    var _cell_height_world = r.f32()  # informational — the world cell height
    # v2 header words: the sequence pass's section descriptors.
    var seq_count = 0
    var seq_max = 0
    var seq_words = 0
    var class_words = 0
    var bitmap_advance = Float32(0)
    if version == 2:
        bitmap_advance = r.f32()  # the cluster head's advance — the pass reads it
        seq_count = Int(r.u32())
        seq_max = Int(r.u32())
        _ = r.u32()  # seqOff — the sections are appended in order; the offset is a cross-check
        _ = r.u32()  # classOff — same
        class_words = Int(r.u32())
        seq_words = seq_count * (2 + seq_max)

    var block_index = List[UInt32](capacity=block_index_len)
    for _ in range(block_index_len):
        block_index.append(r.u32())
    var entries = block_count * (1 << BLOCK_SHIFT)
    var blocks_m = List[Float32](capacity=entries * 2)
    var blocks_c = List[UInt32](capacity=entries * 2)
    for _ in range(entries):
        var gid = r.u32()  # identity — native
        var adv = r.f32()  # measure — bitcast f32, already rounded once
        var h = r.f32()  # measure — bitcast f32
        var fl = r.u32()  # bitfield — native
        blocks_m.append(adv)
        blocks_m.append(h)
        blocks_c.append(gid)
        blocks_c.append(fl)
    # v2 sections, carried verbatim. v1 leaves them empty, and empty reads as
    # "no sequences" downstream — exactly the leader behavior.
    var seq = List[UInt32](capacity=seq_words)
    for _ in range(seq_words):
        seq.append(r.u32())
    var classes = List[UInt32](capacity=class_words)
    for _ in range(class_words):
        classes.append(r.u32())
    var t = Trie(block_index^, blocks_m^, blocks_c^, seq^, classes^)
    t.bitmap_advance = bitmap_advance
    t.seq_max = seq_max
    return t^


def load_trie_auto(path: String) raises -> Trie:
    """NATIVE-PORT (Stage E1): dispatch on magic — a 'G3DF' pipe fixture keeps
    the conformance path (trie carried as f64 VALUES), a 'G3TR' blob takes the
    app-atlas path above. One FFI entry point, both trie sources."""
    var f = open(path, "r")
    var raw = f.read_bytes()
    f.close()
    if len(raw) < 4:
        raise Error(path +": too short to be a trie file")
    var magic = (
        Int(raw[0])
        | (Int(raw[1]) << 8)
        | (Int(raw[2]) << 16)
        | (Int(raw[3]) << 24)
    )
    if magic == TRIE_MAGIC:
        return load_trie_blob(path)
    return load_pipe_fixture(path).trie.copy()


def nan_lanes(measures: List[Float32], total_lanes: Int, mut first: Int) -> Int:
    """Count measure lanes holding NaN. `first` receives the first offending index.

    WHY THIS EXISTS, and it is not hypothetical. Every suite compares measures BY
    BITS — `got.to_bits() != Float32(exp).to_bits()`. That is deliberate: bit
    equality is the contract. But it means two NaNs COMPARE EQUAL AND PASS, and a
    NaN is never a correct value for any lane in this buffer: X/Y/Z/BASE_X/LINE_ADV
    /ADVANCE/HEIGHT are real quantities and GLYPH_ID is an identity.

    Found by the render side, not by us: deleting the oracle's lineHeight fallback
    turned three test lanes to NaN, and the oracle and the scan produced IDENTICAL
    NaN bit patterns (2143289344 on both sides). Their float comparison caught it
    only by the accident of NaN != NaN. Ours is a bit comparison and has no such
    accident — it would have reported GREEN on two equally-wrong values.

    So the equality check cannot police this and a separate invariant must. This is
    the same family as every other trap this week: a comparison that agrees is not
    the same as a comparison that is right."""
    var bad = 0
    first = -1
    for i in range(total_lanes):
        var b = UInt32(measures[i].to_bits())
        # NaN: exponent all ones AND a nonzero mantissa. Infinity is NOT NaN and is
        # caught by the bit comparison like any other value, so do not fold it in.
        if (b & 0x7F800000) == 0x7F800000 and (b & 0x007FFFFF) != 0:
            bad += 1
            if first < 0:
                first = i
    return bad
