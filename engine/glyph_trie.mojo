# glyph_trie.mojo — the glyph trie: its container, its lane constants, and the
# two-load lookup, extracted from glyph_pipeline.mojo in the 2026-09 code-shape
# refactor (a pure move). Mirrors
# packages/glyph3d-core/src/compute/GlyphTrie.js (trie_lookup) — never diverge.

# ── Trie (GlyphTrie.js) ─────────────────────────────────────────────────────
comptime BLOCK_SHIFT = 8
# The last Unicode scalar. A lenient decoder can exceed it; see the
# out-of-range branch in decode_and_resolve for the contract.
comptime MAX_CODEPOINT = 0x10FFFF
comptime BLOCK_MASK = 255
# The trie's OWN container, split by carrier like everything downstream of it
# (2026-08-31, the GLYPH_ID settlement): two genuine measures in f32, an
# identity and a bitfield in u32. The upstream JS trie moved first (50fd6b8,
# one Uint32Array with measures bitcast); this port realizes the same kinds as
# two homogeneous arrays because its house rule is no bitcasts anywhere.
comptime TM_STRIDE = 2
comptime TM_ADVANCE = 0
comptime TM_HEIGHT = 1
comptime TC_STRIDE = 2
comptime TC_GLYPH_ID = 0
comptime TC_FLAGS = 1
comptime FLAG_MISSING = 1


struct Trie(Copyable, Movable):
    var block_index: List[UInt32]
    var blocks_m: List[Float32]  # TM_STRIDE per entry: ADVANCE, HEIGHT
    var blocks_c: List[UInt32]   # TC_STRIDE per entry: GLYPH_ID, FLAGS
    # The sequence pass (v2 trie blobs): the sequence section's raw words
    # ([slot, len, cps..] x sequenceCount, sorted by sequence) and the G3CC
    # class table VERBATIM (its own header included). Empty for v1 blobs and
    # every fixture trie — cluster resolution treats empty as "no sequences",
    # which is exactly the leader behavior. The lookup logic lives in
    # glyph_cluster.mojo; these are just the carried bytes.
    var seq: List[UInt32]
    var classes: List[UInt32]
    # The sequence entry stride is 2 + seq_max; seq_max is 0 when seq is empty.
    var seq_max: Int
    # The advance a resolved cluster head carries (the bitmap 2x cell, in this
    # trie's world units). Meaningful only when seq is non-empty; 0 reads as
    # "no sequences" because the pass early-returns on an empty table.
    var bitmap_advance: Float32

    def __init__(
        out self, var block_index: List[UInt32],
        var blocks_m: List[Float32], var blocks_c: List[UInt32],
        var seq: List[UInt32], var classes: List[UInt32],
    ):
        self.block_index = block_index^
        self.blocks_m = blocks_m^
        self.blocks_c = blocks_c^
        self.seq = seq^
        self.classes = classes^
        self.seq_max = 0
        self.bitmap_advance = Float32(0)

    def advance_at(self, entry: Int) -> Float32:
        return self.blocks_m[entry * TM_STRIDE + TM_ADVANCE]

    def height_at(self, entry: Int) -> Float32:
        return self.blocks_m[entry * TM_STRIDE + TM_HEIGHT]

    def glyph_id_at(self, entry: Int) -> UInt32:
        return self.blocks_c[entry * TC_STRIDE + TC_GLYPH_ID]

    def flags_at(self, entry: Int) -> Int:
        return Int(self.blocks_c[entry * TC_STRIDE + TC_FLAGS])


def trie_lookup_entry(trie: Trie, cp: Int) -> Int:
    """The exact two-load sequence the shader runs; returns the ENTRY INDEX
    (stride-free — callers go through the Trie accessors per carrier)."""
    # Same out-of-range contract as the hot path in decode_and_resolve.
    var block = Int(trie.block_index[cp >> BLOCK_SHIFT]) if cp <= MAX_CODEPOINT else 0
    return (block << BLOCK_SHIFT) | (cp & BLOCK_MASK)
