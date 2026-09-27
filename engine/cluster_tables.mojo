# cluster_tables.mojo — the sequence pass's host-built tables: the head
# candidacy bitmap, the block-presence granularity, and the chaining state
# table with its probe. Extracted from cluster_device.mojo in the 2026-09
# code-shape refactor (a pure move).

from std.utils import StaticTuple
from glyph_trie import Trie

comptime KEY_CAP = 32

# The candidacy bitmap: 4096 u32 words cover every codepoint below 0x20000
# (all of today's heads; the builder refuses a table that exceeds the cap —
# the day one appears, the cap moves deliberately, like KEY_CAP).
comptime HEAD_BMP_WORDS = 4096

# The block-presence table: one u32 per 128 source bytes, bumped (Atomic.max)
# by every written candidate. The chain skips empty blocks in one read — its
# walk costs O(candidate blocks), not O(bytes), which is the whole sparse-text
# case (measured: the byte walk was the cluster tax there, not the probes).
comptime BLOCK_LOG2 = 7
comptime BLOCK = 1 << BLOCK_LOG2


def build_head_bitmap(trie: Trie) raises -> List[UInt32]:
    """The table's first-members set as a bitmap — the probe kernel's one-load
    candidacy test (the CPU rule's `first` set, flattened for the device).
    Derived from the table itself, so it can never drift from it."""
    var bmp = List[UInt32](length=HEAD_BMP_WORDS, fill=0)
    var stride = 2 + trie.seq_max
    if trie.seq_max > KEY_CAP:
        raise Error("sequence table's seq_max exceeds the probe kernel's key cap")
    var i = 0
    while i < len(trie.seq):
        var cp = Int(trie.seq[i + 2])
        # A static-zero head never heads a probe (the kernel zeroes statics
        # before the candidacy read) — the one such entry in the table is the
        # font's dead (cancel-tag, black-flag) rule; skip it here.
        if cp == 0x200D or (cp >= 0xFE00 and cp <= 0xFE0F) or (cp >= 0xE0020 and cp <= 0xE007F):
            i += stride
            continue
        if cp >= 0x20000:
            raise Error("sequence head at/above 0x20000 — the candidacy bitmap's cap moves deliberately")
        bmp[cp >> 5] = bmp[cp >> 5] | UInt32(1 << (cp & 31))
        i += stride
    return bmp^


# ── the state walk's table ─────────────────────────────────────────────────
# The sequence table as a (state, cp) -> (next_state, accept_slot) hash: the
# trie of all 4,166 entries flattened for O(1)-per-step walking. Built at load
# time from the SAME table bytes the binary-search form reads (deterministic
# insertion order: entries in table order, codepoints in order), so the two
# forms can never drift. ST_STRIDE fields per slot: [key_state, key_cp,
# next_state, accept_slot]; accept_slot lives on the edge into the accepting
# node, which is the node (a trie node has exactly one path).
comptime ST_STRIDE = 4
comptime ST_EMPTY = 0xFFFFFFFF


def st_mix(s: UInt32, cp: UInt32) -> UInt32:
    """The one hash, shared by the builder (host) and st_probe (device) — same
    function both sides, so placement can never disagree with lookup."""
    var h = s * 0x9E3779B1
    h ^= cp * 0x85EBCA77
    h ^= h >> 16
    return h


def build_state_table(seq: List[UInt32], seq_count: Int, seq_max: Int) raises -> List[UInt32]:
    """Flatten the sorted sequence section into the walk's hash. Runs at load
    time (host); the kernel never builds. The static-zero-head entry is
    included verbatim — unreachable, exactly as in the search form, because
    the kernel's static-zero branch returns before any table read."""
    var edges = List[UInt32]()      # stride 4: from, cp, to, 0 (accept filled later)
    var accepts = List[UInt32](length=1, fill=0)  # by node id; node 0 is root
    var ids = Dict[UInt64, UInt32]()              # (from << 32) | cp -> to
    var next_id = UInt32(1)
    var stride = 2 + seq_max
    for i in range(seq_count):
        var o = i * stride
        var ln = Int(seq[o + 1])
        var slot = seq[o]
        if ln < 2:
            # All four implementations floor the match at len >= 2; a len-1
            # entry is dead in the search form but would go LIVE here (the
            # root edge would accept at depth 1). The floor moves deliberately.
            raise Error("sequence entry with len < 2 — the match floor moves deliberately")
        var s = UInt32(0)
        for d in range(ln):
            var cp = seq[o + 2 + d]
            var key = (UInt64(s) << 32) | UInt64(cp)
            var ns: UInt32
            if key in ids:
                ns = ids[key]
            else:
                ns = next_id
                next_id += 1
                ids[key] = ns
                edges.append(s)
                edges.append(cp)
                edges.append(ns)
                edges.append(0)
                accepts.append(0)
            if d == ln - 1:
                if accepts[ns] != 0 and accepts[ns] != slot:
                    # First-in-sorted-order is today's duplicate semantics;
                    # a real duplicate moves THAT deliberately too.
                    raise Error("duplicate sequence with a different slot — the tiebreak moves deliberately")
                accepts[ns] = slot
            s = ns
    var n_edges = len(edges) // ST_STRIDE
    var size = 1
    while size < n_edges * 2:
        size *= 2
    var tab = List[UInt32](length=size * ST_STRIDE, fill=0)
    var mask = UInt32(size - 1)
    for h in range(size):
        tab[h * ST_STRIDE] = ST_EMPTY
    for e in range(n_edges):
        var s = edges[e * ST_STRIDE]
        var cp = edges[e * ST_STRIDE + 1]
        var ns = edges[e * ST_STRIDE + 2]
        var h = st_mix(s, cp) & mask
        while tab[Int(h) * ST_STRIDE] != ST_EMPTY:
            h = (h + 1) & mask
        var o = Int(h) * ST_STRIDE
        tab[o] = s
        tab[o + 1] = cp
        tab[o + 2] = ns
        tab[o + 3] = accepts[Int(ns)]
    return tab^


def st_probe(
    tab: MutPointer[UInt32, MutAnyOrigin], mask: UInt32, s: UInt32, cp: UInt32
) -> StaticTuple[UInt32, 2]:
    """One table probe: (next_state, accept_slot), or (ST_EMPTY, 0) on a miss.
    Linear probing; the load factor is <= 1/2 by the builder's sizing."""
    var h = st_mix(s, cp) & mask
    while True:
        var o = Int(h) * ST_STRIDE
        var ks = tab[unsafe_offset=o]
        if ks == ST_EMPTY:
            var miss = StaticTuple[UInt32, 2](0)
            miss[0] = ST_EMPTY
            return miss
        if ks == s and tab[unsafe_offset = o + 1] == cp:
            var hit = StaticTuple[UInt32, 2](0)
            hit[0] = tab[unsafe_offset = o + 2]
            hit[1] = tab[unsafe_offset = o + 3]
            return hit
        h = (h + 1) & mask


