# cluster_device.mojo — the sequence pass's device kernels, shared.
#
# k_cluster_probe (thread per byte) + k_cluster_chain (thread per item),
# ported line-for-line from cluster_split.mojo's proven CPU split. The
# consumers: gpu_cluster.mojo (the standalone proof vs the CPU split) and
# gpu_pipeline.mojo (the full-device pipeline, where the pass is computed on
# device instead of uploaded). One definition so the two harnesses can never
# drift the rule between them.

from max.gpu import global_idx
from std.utils import StaticTuple
from std.atomic import Atomic
from glyph_schema import SM_STRIDE, SM_ADVANCE, SM_HEIGHT
from glyph_pipeline import (
    BLOCK_SHIFT,
    BLOCK_MASK,
    TM_STRIDE,
    TM_ADVANCE,
    TM_HEIGHT,
    TC_STRIDE,
    TC_GLYPH_ID,
    TC_FLAGS,
    FLAG_MISSING,
    F_LEADER,
    F_NEWLINE,
    F_MISSING,
    F_CLUSTER_TRAILER,
    NEWLINE,
    Trie,
)

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


def k_cluster_probe(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cluster_of: MutPointer[UInt32, MutAnyOrigin],   # per byte: 1 in a cluster item
    item_end: MutPointer[UInt32, MutAnyOrigin],     # per byte: the item's end offset
    seq: MutPointer[UInt32, MutAnyOrigin],          # the v2 sequence section, verbatim
    cand_bmp: MutPointer[UInt32, MutAnyOrigin],     # build_head_bitmap: the first members
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],  # presence per 128-byte block
    n_bytes: Int32,
    seq_count: Int32,
    seq_max: Int32,
):
    """One thread per byte — cluster_split.mojo's probe_clusters, ported.
    Candidacy is one bitmap load (the table's first members); a codepoint no
    sequence starts with exits there, never paying for the window or the
    search."""
    var id = Int(global_idx.x)
    var n = Int(n_bytes)
    if id >= n:
        return
    if cluster_of[unsafe_offset=id] == 0:
        return
    if Int(seq_count) == 0:
        return  # the rule early-returns on an empty table — statics included
    var b0 = Int(bytes[unsafe_offset=id])
    var nb: Int
    if (b0 & 0x80) == 0x00:
        nb = 1
    elif (b0 & 0xE0) == 0xC0:
        nb = 2
    elif (b0 & 0xF0) == 0xE0:
        nb = 3
    elif (b0 & 0xF8) == 0xF0:
        nb = 4
    else:
        nb = 0
    if nb == 0:
        return  # a continuation byte is never a leader

    # decode, bounds-clamped (the decode kernel's read discipline)
    var b1 = 0
    var b2 = 0
    var b3 = 0
    if id + 1 < n:
        b1 = Int(bytes[unsafe_offset = id + 1])
    if id + 2 < n:
        b2 = Int(bytes[unsafe_offset = id + 2])
    if id + 3 < n:
        b3 = Int(bytes[unsafe_offset = id + 3])
    var cp: Int
    if nb == 1:
        cp = b0
    elif nb == 2:
        cp = ((b0 & 0x1F) << 6) | (b1 & 0x3F)
    elif nb == 3:
        cp = ((b0 & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F)
    else:
        cp = ((b0 & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F)

    # is_static_zero_cp (glyph_cluster.mojo), inlined for the device.
    if cp == 0x200D or (cp >= 0xFE00 and cp <= 0xFE0F) or (cp >= 0xE0020 and cp <= 0xE007F):
        gi[unsafe_offset=id] = 0
        sm[unsafe_offset = id * SM_STRIDE + SM_ADVANCE] = Float32(0)
        fl[unsafe_offset=id] = fl[unsafe_offset=id] | UInt32(F_CLUSTER_TRAILER)
        return

    # Candidacy: one bitmap load instead of a search. The bitmap is the
    # table's own first members (build_head_bitmap), so false negatives are
    # impossible by construction; above the cap is never a head (the builder
    # refuses such a table).
    if cp >= 0x20000:
        return
    if (Int(cand_bmp[unsafe_offset = cp >> 5]) & (1 << (cp & 31))) == 0:
        return

    # The probe window: up to seq_max EFFECTIVE codepoints, VS16 dropped from
    # the key but riding the span; newline/VS15/continuation/item-end break it.
    var key = StaticTuple[UInt32, KEY_CAP](0)
    key[0] = UInt32(cp)
    var key_len = 1
    var p = id + nb
    var stop = Int(item_end[unsafe_offset=id])
    while p < stop and key_len < Int(seq_max):
        var c0 = Int(bytes[unsafe_offset=p])
        var nb2: Int
        if (c0 & 0x80) == 0x00:
            nb2 = 1
        elif (c0 & 0xE0) == 0xC0:
            nb2 = 2
        elif (c0 & 0xF0) == 0xE0:
            nb2 = 3
        elif (c0 & 0xF8) == 0xF0:
            nb2 = 4
        else:
            nb2 = 0
        if nb2 == 0:
            break
        var d1 = 0
        var d2 = 0
        var d3 = 0
        if p + 1 < n:
            d1 = Int(bytes[unsafe_offset = p + 1])
        if p + 2 < n:
            d2 = Int(bytes[unsafe_offset = p + 2])
        if p + 3 < n:
            d3 = Int(bytes[unsafe_offset = p + 3])
        var cp2: Int
        if nb2 == 1:
            cp2 = c0
        elif nb2 == 2:
            cp2 = ((c0 & 0x1F) << 6) | (d1 & 0x3F)
        elif nb2 == 3:
            cp2 = ((c0 & 0x0F) << 12) | ((d1 & 0x3F) << 6) | (d2 & 0x3F)
        else:
            cp2 = ((c0 & 0x07) << 18) | ((d1 & 0x3F) << 12) | ((d2 & 0x3F) << 6) | (d3 & 0x3F)
        if cp2 == Int(NEWLINE) or cp2 == 0xFE0E:
            break
        if cp2 != 0xFE0F:
            key[key_len] = UInt32(cp2)
            key_len += 1
        p += nb2

    # The longest table prefix of the key wins, found by descending length
    # with a binary search per length — the section is sorted elementwise,
    # prefix-first (asserted at bake). The CPU rule's same move is
    # glyph_cluster.mojo's; atlas.rs's sequence_lookup is the third writer of
    # this ordering contract.
    var best_len = 0
    var best_slot = UInt32(0)
    var stride = 2 + Int(seq_max)
    var ln = min(key_len, Int(seq_max))
    while ln >= 2 and best_len == 0:
        var lo = 0
        var hi = Int(seq_count)
        while lo < hi:
            var mid = (lo + hi) // 2
            var o = mid * stride
            var elen = Int(seq[unsafe_offset = o + 1])
            var cmp = 0
            var k = 0
            var nmin = min(ln, elen)
            while k < nmin:
                var a = Int(key[k])
                var b = Int(seq[unsafe_offset = o + 2 + k])
                if a < b:
                    cmp = -1
                    break
                if a > b:
                    cmp = 1
                    break
                k += 1
            if cmp == 0:
                cmp = -1 if ln < elen else (1 if ln > elen else 0)
            if cmp > 0:
                lo = mid + 1
            else:
                hi = mid
        if lo < Int(seq_count):
            var o2 = lo * stride
            if Int(seq[unsafe_offset = o2 + 1]) == ln:
                var ok = True
                var k2 = 0
                while k2 < ln:
                    if Int(key[k2]) != Int(seq[unsafe_offset = o2 + 2 + k2]):
                        ok = False
                        break
                    k2 += 1
                if ok:
                    best_len = ln
                    best_slot = seq[unsafe_offset = o2]
        ln -= 1
    if best_len > 0:
        # The span end: re-walk counting key-consumers, so skipped VS16s stay
        # inside the trailer span (within a matched span no break can occur —
        # a break would have ended the probe window before the member).
        var need = best_len
        var p2 = id
        while need > 0:
            var e0 = Int(bytes[unsafe_offset=p2])
            var nb3: Int
            if (e0 & 0x80) == 0x00:
                nb3 = 1
            elif (e0 & 0xE0) == 0xC0:
                nb3 = 2
            elif (e0 & 0xF0) == 0xE0:
                nb3 = 3
            else:
                nb3 = 4
            var f1 = 0
            var f2 = 0
            var f3 = 0
            if p2 + 1 < n:
                f1 = Int(bytes[unsafe_offset = p2 + 1])
            if p2 + 2 < n:
                f2 = Int(bytes[unsafe_offset = p2 + 2])
            if p2 + 3 < n:
                f3 = Int(bytes[unsafe_offset = p2 + 3])
            var cp3: Int
            if nb3 == 1:
                cp3 = e0
            elif nb3 == 2:
                cp3 = ((e0 & 0x1F) << 6) | (f1 & 0x3F)
            elif nb3 == 3:
                cp3 = ((e0 & 0x0F) << 12) | ((f1 & 0x3F) << 6) | (f2 & 0x3F)
            else:
                cp3 = ((e0 & 0x07) << 18) | ((f1 & 0x3F) << 12) | ((f2 & 0x3F) << 6) | (f3 & 0x3F)
            if cp3 != 0xFE0F:
                need -= 1
            p2 += nb3
        cand_slot[unsafe_offset=id] = best_slot
        cand_end[unsafe_offset=id] = UInt32(p2)
        _ = Atomic.max(cand_blocks.unsafe_offset(id >> BLOCK_LOG2), UInt32(1))


def k_cluster_chain(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    item_ranges: MutPointer[UInt32, MutAnyOrigin],    # 2 per item: byte_start, byte_end
    item_cluster: MutPointer[UInt32, MutAnyOrigin],   # per item: 1 = cluster mode
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],    # presence per 128-byte block
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    item_count: Int32,
):
    """One thread per ITEM — the split form's commit. The carry is the resume
    itself: after a commit the walk jumps past the span, so a phantom
    candidate inside it is never read — suppression is by construction, and
    the suite's mutation proves it by removing the resume. Items tile the
    blob, so no two threads share a byte. The block-presence table skips
    candidate-free 128-byte blocks in one read."""
    var i = Int(global_idx.x)
    if i >= Int(item_count):
        return
    if item_cluster[unsafe_offset=i] == 0:
        return
    var id = Int(item_ranges[unsafe_offset = i * 2])
    var stop = Int(item_ranges[unsafe_offset = i * 2 + 1])
    while id < stop:
        if cand_blocks[unsafe_offset = id >> BLOCK_LOG2] == 0:
            # No candidate in this 128-byte block — jump it. (A block may span
            # the item's end; zero there means nothing for either side.)
            id = min((id & ~(BLOCK - 1)) + BLOCK, stop)
            continue
        var b0 = Int(bytes[unsafe_offset=id])
        var nb: Int
        if (b0 & 0x80) == 0x00:
            nb = 1
        elif (b0 & 0xE0) == 0xC0:
            nb = 2
        elif (b0 & 0xF0) == 0xE0:
            nb = 3
        elif (b0 & 0xF8) == 0xF0:
            nb = 4
        else:
            nb = 0
        if nb == 0:
            id += 1
            continue
        var slot = Int(cand_slot[unsafe_offset=id])
        if slot != 0:
            gi[unsafe_offset=id] = UInt32(slot)
            sm[unsafe_offset = id * SM_STRIDE + SM_ADVANCE] = bitmap_advance
            var end = Int(cand_end[unsafe_offset=id])
            var p = id + nb
            while p < end:
                var c0 = Int(bytes[unsafe_offset=p])
                var nb2: Int
                if (c0 & 0x80) == 0x00:
                    nb2 = 1
                elif (c0 & 0xE0) == 0xC0:
                    nb2 = 2
                elif (c0 & 0xF0) == 0xE0:
                    nb2 = 3
                else:
                    nb2 = 4
                gi[unsafe_offset=p] = 0
                sm[unsafe_offset = p * SM_STRIDE + SM_ADVANCE] = Float32(0)
                fl[unsafe_offset=p] = fl[unsafe_offset=p] | UInt32(F_CLUSTER_TRAILER)
                p += nb2
            id = end  # the span's members are written; resume past it
        else:
            id += nb



def k_decode_probe[probe: Bool, walk: Bool = False](
    bytes: MutPointer[UInt8, MutAnyOrigin],
    block_index: MutPointer[UInt32, MutAnyOrigin],
    blocks_m: MutPointer[Float32, MutAnyOrigin],
    blocks_c: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    cluster_of: MutPointer[UInt32, MutAnyOrigin],   # per byte: 1 in a cluster item
    item_end: MutPointer[UInt32, MutAnyOrigin],     # per byte: the item's end offset
    seq: MutPointer[UInt32, MutAnyOrigin],          # the v2 sequence section, verbatim
    cand_bmp: MutPointer[UInt32, MutAnyOrigin],     # build_head_bitmap: the first members
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],  # presence per 128-byte block
    tab: MutPointer[UInt32, MutAnyOrigin],          # build_state_table's hash (walk only)
    tab_mask: UInt32,
    n_bytes: Int32,
    seq_count: Int32,
    seq_max: Int32,
):
    """One thread per byte — gpu_decode.mojo's decode_kernel with
    k_cluster_probe fused behind comptime `probe`. The decode half is
    line-for-line decode_kernel; the probe half is line-for-line
    k_cluster_probe MINUS the head decode, which arrives in registers (cp, nb)
    instead of being re-walked from bytes. The probe reads no decode output —
    bytes, host metadata and tables only (verified against k_cluster_probe's
    body) — so dropping the dispatch boundary between the halves changes
    nothing; the chain stays a separate dispatch and keeps the global sync IT
    needs. With probe=False this is device decode alone (the bench's mode 1),
    so the A/B attributes the delta: upload elimination vs dispatch fusion."""
    var id = Int(global_idx.x)
    var n = Int(n_bytes)
    if id >= n:
        return

    # ── decode (gpu_decode.mojo's decode_kernel, line-for-line) ─────────────
    var b0 = Int(bytes[unsafe_offset=id])
    var nb: Int
    if (b0 & 0x80) == 0x00:
        nb = 1
    elif (b0 & 0xE0) == 0xC0:
        nb = 2
    elif (b0 & 0xF0) == 0xE0:
        nb = 3
    elif (b0 & 0xF8) == 0xF0:
        nb = 4
    else:
        nb = 0

    var mo = id * SM_STRIDE
    if nb == 0:
        sm[unsafe_offset = mo + SM_ADVANCE] = 0
        sm[unsafe_offset = mo + SM_HEIGHT] = 0
        gi[unsafe_offset=id] = 0
        return  # a continuation byte is never a leader — in either half

    # Bounds-checked continuation reads (the shader reads 0 past the end).
    var b1 = 0
    var b2 = 0
    var b3 = 0
    if id + 1 < n:
        b1 = Int(bytes[unsafe_offset = id + 1])
    if id + 2 < n:
        b2 = Int(bytes[unsafe_offset = id + 2])
    if id + 3 < n:
        b3 = Int(bytes[unsafe_offset = id + 3])

    var cp: Int
    if nb == 1:
        cp = b0
    elif nb == 2:
        cp = ((b0 & 0x1F) << 6) | (b1 & 0x3F)
    elif nb == 3:
        cp = ((b0 & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F)
    else:
        cp = ((b0 & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F)

    var block = Int(block_index[unsafe_offset = cp >> BLOCK_SHIFT])
    var entry = (block << BLOCK_SHIFT) | (cp & BLOCK_MASK)

    # A REAL bit test on a REAL integer — the settlement ended the Int(f32)
    # coercion this line used to be.
    var missing = (Int(blocks_c[unsafe_offset = entry * TC_STRIDE + TC_FLAGS]) & FLAG_MISSING) != 0
    gi[unsafe_offset=id] = blocks_c[unsafe_offset = entry * TC_STRIDE + TC_GLYPH_ID]
    sm[unsafe_offset = mo + SM_ADVANCE] = blocks_m[unsafe_offset = entry * TM_STRIDE + TM_ADVANCE]
    sm[unsafe_offset = mo + SM_HEIGHT] = blocks_m[unsafe_offset = entry * TM_STRIDE + TM_HEIGHT]
    var flags = F_LEADER
    if cp == NEWLINE:
        flags |= F_NEWLINE
    if missing:
        flags |= F_MISSING
    fl[unsafe_offset=id] = UInt32(flags)

    # ── probe (k_cluster_probe, minus the head decode it now inherits) ──────
    comptime if probe:
        if cluster_of[unsafe_offset=id] == 0:
            return
        if Int(seq_count) == 0:
            return  # the rule early-returns on an empty table — statics included

        # is_static_zero_cp (glyph_cluster.mojo), inlined for the device.
        if cp == 0x200D or (cp >= 0xFE00 and cp <= 0xFE0F) or (cp >= 0xE0020 and cp <= 0xE007F):
            gi[unsafe_offset=id] = 0
            sm[unsafe_offset = mo + SM_ADVANCE] = Float32(0)
            fl[unsafe_offset=id] = fl[unsafe_offset=id] | UInt32(F_CLUSTER_TRAILER)
            return

        # Candidacy: one bitmap load instead of a search. The bitmap is the
        # table's own first members (build_head_bitmap), so false negatives are
        # impossible by construction; above the cap is never a head (the builder
        # refuses such a table).
        if cp >= 0x20000:
            return
        if (Int(cand_bmp[unsafe_offset = cp >> 5]) & (1 << (cp & 31))) == 0:
            return

        comptime if walk:
            # The state walk: one table probe per EFFECTIVE codepoint, the last
            # accepting state IS the longest match — the key array, the
            # descending searches, and the span re-walk are all gone. The
            # window and its break/skip rules are the search form's, codepoint
            # for codepoint; VS16 advances the byte pointer without probing.
            var r0 = st_probe(tab, tab_mask, 0, UInt32(cp))
            if r0[0] == ST_EMPTY:
                return  # unreachable: the bitmap and the table are one source
            var s = r0[0]
            var last_slot = r0[1]
            var last_end = 0
            if r0[1] != 0:
                last_end = id + nb
            var p = id + nb
            var depth = 1
            var stop = Int(item_end[unsafe_offset=id])
            while p < stop and depth < Int(seq_max):
                var c0 = Int(bytes[unsafe_offset=p])
                var nb2: Int
                if (c0 & 0x80) == 0x00:
                    nb2 = 1
                elif (c0 & 0xE0) == 0xC0:
                    nb2 = 2
                elif (c0 & 0xF0) == 0xE0:
                    nb2 = 3
                elif (c0 & 0xF8) == 0xF0:
                    nb2 = 4
                else:
                    nb2 = 0
                if nb2 == 0:
                    break
                var d1 = 0
                var d2 = 0
                var d3 = 0
                if p + 1 < n:
                    d1 = Int(bytes[unsafe_offset = p + 1])
                if p + 2 < n:
                    d2 = Int(bytes[unsafe_offset = p + 2])
                if p + 3 < n:
                    d3 = Int(bytes[unsafe_offset = p + 3])
                var cp2: Int
                if nb2 == 1:
                    cp2 = c0
                elif nb2 == 2:
                    cp2 = ((c0 & 0x1F) << 6) | (d1 & 0x3F)
                elif nb2 == 3:
                    cp2 = ((c0 & 0x0F) << 12) | ((d1 & 0x3F) << 6) | (d2 & 0x3F)
                else:
                    cp2 = ((c0 & 0x07) << 18) | ((d1 & 0x3F) << 12) | ((d2 & 0x3F) << 6) | (d3 & 0x3F)
                if cp2 == Int(NEWLINE) or cp2 == 0xFE0E:
                    break
                if cp2 == 0xFE0F:
                    p += nb2
                    continue
                var r = st_probe(tab, tab_mask, s, UInt32(cp2))
                if r[0] == ST_EMPTY:
                    break
                s = r[0]
                p += nb2
                depth += 1
                if r[1] != 0:
                    last_slot = r[1]
                    last_end = p
            if last_slot != 0:
                cand_slot[unsafe_offset=id] = last_slot
                cand_end[unsafe_offset=id] = UInt32(last_end)
                _ = Atomic.max(cand_blocks.unsafe_offset(id >> BLOCK_LOG2), UInt32(1))
        else:
            # The probe window: up to seq_max EFFECTIVE codepoints, VS16 dropped from
            # the key but riding the span; newline/VS15/continuation/item-end break it.
            var key = StaticTuple[UInt32, KEY_CAP](0)
            key[0] = UInt32(cp)
            var key_len = 1
            var p = id + nb
            var stop = Int(item_end[unsafe_offset=id])
            while p < stop and key_len < Int(seq_max):
                var c0 = Int(bytes[unsafe_offset=p])
                var nb2: Int
                if (c0 & 0x80) == 0x00:
                    nb2 = 1
                elif (c0 & 0xE0) == 0xC0:
                    nb2 = 2
                elif (c0 & 0xF0) == 0xE0:
                    nb2 = 3
                elif (c0 & 0xF8) == 0xF0:
                    nb2 = 4
                else:
                    nb2 = 0
                if nb2 == 0:
                    break
                var d1 = 0
                var d2 = 0
                var d3 = 0
                if p + 1 < n:
                    d1 = Int(bytes[unsafe_offset = p + 1])
                if p + 2 < n:
                    d2 = Int(bytes[unsafe_offset = p + 2])
                if p + 3 < n:
                    d3 = Int(bytes[unsafe_offset = p + 3])
                var cp2: Int
                if nb2 == 1:
                    cp2 = c0
                elif nb2 == 2:
                    cp2 = ((c0 & 0x1F) << 6) | (d1 & 0x3F)
                elif nb2 == 3:
                    cp2 = ((c0 & 0x0F) << 12) | ((d1 & 0x3F) << 6) | (d2 & 0x3F)
                else:
                    cp2 = ((c0 & 0x07) << 18) | ((d1 & 0x3F) << 12) | ((d2 & 0x3F) << 6) | (d3 & 0x3F)
                if cp2 == Int(NEWLINE) or cp2 == 0xFE0E:
                    break
                if cp2 != 0xFE0F:
                    key[key_len] = UInt32(cp2)
                    key_len += 1
                p += nb2

            # The longest table prefix of the key wins, found by descending length
            # with a binary search per length — the section is sorted elementwise,
            # prefix-first (asserted at bake). The CPU rule's same move is
            # glyph_cluster.mojo's; atlas.rs's sequence_lookup is the third writer of
            # this ordering contract.
            var best_len = 0
            var best_slot = UInt32(0)
            var stride = 2 + Int(seq_max)
            var ln = min(key_len, Int(seq_max))
            while ln >= 2 and best_len == 0:
                var lo = 0
                var hi = Int(seq_count)
                while lo < hi:
                    var mid = (lo + hi) // 2
                    var o = mid * stride
                    var elen = Int(seq[unsafe_offset = o + 1])
                    var cmp = 0
                    var k = 0
                    var nmin = min(ln, elen)
                    while k < nmin:
                        var a = Int(key[k])
                        var b = Int(seq[unsafe_offset = o + 2 + k])
                        if a < b:
                            cmp = -1
                            break
                        if a > b:
                            cmp = 1
                            break
                        k += 1
                    if cmp == 0:
                        cmp = -1 if ln < elen else (1 if ln > elen else 0)
                    if cmp > 0:
                        lo = mid + 1
                    else:
                        hi = mid
                if lo < Int(seq_count):
                    var o2 = lo * stride
                    if Int(seq[unsafe_offset = o2 + 1]) == ln:
                        var ok = True
                        var k2 = 0
                        while k2 < ln:
                            if Int(key[k2]) != Int(seq[unsafe_offset = o2 + 2 + k2]):
                                ok = False
                                break
                            k2 += 1
                        if ok:
                            best_len = ln
                            best_slot = seq[unsafe_offset = o2]
                ln -= 1
            if best_len > 0:
                # The span end: re-walk counting key-consumers, so skipped VS16s stay
                # inside the trailer span (within a matched span no break can occur —
                # a break would have ended the probe window before the member).
                var need = best_len
                var p2 = id
                while need > 0:
                    var e0 = Int(bytes[unsafe_offset=p2])
                    var nb3: Int
                    if (e0 & 0x80) == 0x00:
                        nb3 = 1
                    elif (e0 & 0xE0) == 0xC0:
                        nb3 = 2
                    elif (e0 & 0xF0) == 0xE0:
                        nb3 = 3
                    else:
                        nb3 = 4
                    var f1 = 0
                    var f2 = 0
                    var f3 = 0
                    if p2 + 1 < n:
                        f1 = Int(bytes[unsafe_offset = p2 + 1])
                    if p2 + 2 < n:
                        f2 = Int(bytes[unsafe_offset = p2 + 2])
                    if p2 + 3 < n:
                        f3 = Int(bytes[unsafe_offset = p2 + 3])
                    var cp3: Int
                    if nb3 == 1:
                        cp3 = e0
                    elif nb3 == 2:
                        cp3 = ((e0 & 0x1F) << 6) | (f1 & 0x3F)
                    elif nb3 == 3:
                        cp3 = ((e0 & 0x0F) << 12) | ((f1 & 0x3F) << 6) | (f2 & 0x3F)
                    else:
                        cp3 = ((e0 & 0x07) << 18) | ((f1 & 0x3F) << 12) | ((f2 & 0x3F) << 6) | (f3 & 0x3F)
                    if cp3 != 0xFE0F:
                        need -= 1
                    p2 += nb3
                cand_slot[unsafe_offset=id] = best_slot
                cand_end[unsafe_offset=id] = UInt32(p2)
                _ = Atomic.max(cand_blocks.unsafe_offset(id >> BLOCK_LOG2), UInt32(1))


# ── the chain, chunk-parallel ───────────────────────────────────────────────
# k_cluster_chain is thread-per-ITEM: one thread walks an item's whole byte
# range, so a single-item corpus runs the commit serially while the device
# idles (measured 2026-09-25: 90% of the dense-emoji device phase). The three
# kernels below run the same greedy rule per 128-byte BLOCK (the presence
# table's own partition) with a one-integer carry stitched between blocks:
#
#   k_chain_free   (thread/block) — the greedy walk with a free in-carry;
#                    out[c] = the last committed span's end iff it crosses the
#                    block end, else 0.
#   k_chain_stitch (one thread)   — the carry recurrence r_{c+1} = f_c(r_c):
#                    r <= block start: free entry, r = out[c]; r inside: the
#                    block gets r as its in-carry and a bounded re-walk (<= one
#                    block) computes the new carry; r past the block end: the
#                    span covers the whole block and passes through, O(1).
#   k_chain_commit (thread/block) — re-walks with the RESOLVED in-carry and
#                    writes the commits. The re-walk is required, not
#                    conservative: a nonzero carry can ENABLE a candidate the
#                    free walk suppressed (W=[0,10), Y=[5,15), X=[12,14) — the
#                    free walk commits W and Y; carry 11 skips both and commits
#                    X), so the committed set genuinely depends on the carry.
#
# The walk's whole state is the resume position, which is why one integer is
# the whole carry. Every block's statics are written ONLY by the block
# containing the byte (a crossing span's trailers are written by the successor
# blocks, per leader) — the serial form's race-freedom ("items tile the blob")
# holds under the new partition. Spans clamp at item ends in the probe, so the
# carry never crosses an item boundary and the walk needs no item logic at all.


def chain_walk[write: Bool](
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    start: Int,
    stop: Int,
) -> Int:
    """k_cluster_chain's greedy walk over [start, stop), chunked. Returns the
    last committed span's end when it crosses `stop`, else 0. With write=True
    the commits rewrite statics — but only for bytes < stop (the successor
    block writes the rest of a crossing span: the ownership rule)."""
    var id = start
    # Align to the first leader at/after start: a block can start mid-codepoint
    # (UTF-8 self-synchronizes within 3 bytes).
    while id < stop and (Int(bytes[unsafe_offset=id]) & 0xC0) == 0x80:
        id += 1
    var open_end = 0
    while id < stop:
        var b0 = Int(bytes[unsafe_offset=id])
        var nb: Int
        if (b0 & 0x80) == 0x00:
            nb = 1
        elif (b0 & 0xE0) == 0xC0:
            nb = 2
        elif (b0 & 0xF0) == 0xE0:
            nb = 3
        elif (b0 & 0xF8) == 0xF0:
            nb = 4
        else:
            nb = 0
        if nb == 0:
            id += 1
            continue
        var slot = Int(cand_slot[unsafe_offset=id])
        if slot != 0:
            var end = Int(cand_end[unsafe_offset=id])
            comptime if write:
                gi[unsafe_offset=id] = UInt32(slot)
                sm[unsafe_offset = id * SM_STRIDE + SM_ADVANCE] = bitmap_advance
                var p = id + nb
                var limit = min(end, stop)
                while p < limit:
                    var c0 = Int(bytes[unsafe_offset=p])
                    var nb2: Int
                    if (c0 & 0x80) == 0x00:
                        nb2 = 1
                    elif (c0 & 0xE0) == 0xC0:
                        nb2 = 2
                    elif (c0 & 0xF0) == 0xE0:
                        nb2 = 3
                    else:
                        nb2 = 4
                    gi[unsafe_offset=p] = 0
                    sm[unsafe_offset = p * SM_STRIDE + SM_ADVANCE] = Float32(0)
                    fl[unsafe_offset=p] = fl[unsafe_offset=p] | UInt32(F_CLUSTER_TRAILER)
                    p += nb2
            open_end = end
            id = end  # resume past the span — the chunked carry's suppressor
        else:
            id += nb
    return open_end if open_end > stop else 0


# The compact candidate list: per block, (start, end, slot) triples in start
# order, so the stitch and the commit walk a handful of entries instead of
# re-decoding the block's bytes (measured: the stitch's byte re-walks were
# 193 of the chain's 199 ms). Collection does NOT span-jump — a candidate
# nested inside another's span is dead in the free walk but live under a
# nonzero carry, so it must be recorded too. A block holding more than
# CLIST_CAP candidates marks ccount = OVERFLOW and the stitch/commit fall
# back to the byte walk for that block (correct, just slower; unseen in the
# corpus — 64 candidates needs candidate heads every 2 bytes).
comptime CLIST_CAP = 64
comptime CLIST_STRIDE = 4
comptime CLIST_OVERFLOW = 0xFFFFFFFF


def k_chain_free(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    span_out: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    n_chunks: Int32,
):
    """Thread per 128-byte block: collect the block's candidates in start
    order AND compute the zone table in one pass. Zone j is the greedy from
    candidate j onward — the out for an incoming carry that suppresses
    candidates before j. All zone sims ride the single walk (zone j's first
    commit is candidate j by definition; later candidates commit iff they
    clear the zone's committed end); zone 0 is the free walk, so span_out is
    zone_out[0]. The list carries every candidate regardless, for the carry
    re-walks and the commit."""
    var c = Int(global_idx.x)
    if c >= Int(n_chunks):
        return
    var start = c * BLOCK
    var stop = min(start + BLOCK, Int(n_bytes))
    var base = c * CLIST_CAP * CLIST_STRIDE
    if cand_blocks[unsafe_offset=c] == 0:
        ccount[unsafe_offset=c] = 0
        span_out[unsafe_offset=c] = 0
        return
    var cnt = 0
    var overflow = False
    var sims = StaticTuple[UInt32, CLIST_CAP](0)
    var id = start
    while id < stop and (Int(bytes[unsafe_offset=id]) & 0xC0) == 0x80:
        id += 1
    while id < stop:
        var b0 = Int(bytes[unsafe_offset=id])
        var nb: Int
        if (b0 & 0x80) == 0x00:
            nb = 1
        elif (b0 & 0xE0) == 0xC0:
            nb = 2
        elif (b0 & 0xF0) == 0xE0:
            nb = 3
        elif (b0 & 0xF8) == 0xF0:
            nb = 4
        else:
            nb = 0
        if nb == 0:
            id += 1
            continue
        # The zone of every leader is the count when it's visited: the first
        # candidate index with start >= id (its own index when it IS one).
        czidx[unsafe_offset = c * BLOCK + (id - start)] = UInt8(min(cnt, 255))
        var slot = Int(cand_slot[unsafe_offset=id])
        if slot != 0:
            var end = Int(cand_end[unsafe_offset=id])
            var lim = min(cnt, CLIST_CAP)
            for j in range(lim):
                if id >= Int(sims[j]):
                    sims[j] = UInt32(end)
            if cnt < CLIST_CAP:
                sims[cnt] = UInt32(end)
                var o = base + cnt * CLIST_STRIDE
                clist[unsafe_offset=o] = UInt32(id)
                clist[unsafe_offset = o + 1] = UInt32(end)
                clist[unsafe_offset = o + 2] = UInt32(slot)
            else:
                overflow = True
            cnt += 1
        id += nb
    if not overflow:
        for j in range(cnt):
            clist[unsafe_offset = base + j * CLIST_STRIDE + 3] = (
                sims[j] if Int(sims[j]) > stop else UInt32(0)
            )
    ccount[unsafe_offset=c] = CLIST_OVERFLOW if overflow else UInt32(cnt)
    span_out[unsafe_offset=c] = (
        sims[0] if (cnt > 0 and Int(sims[0]) > stop) else UInt32(0)
    )


def block_eval(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    c: Int,
    r: Int,
    stop: Int,
) -> Int:
    """The block's exit carry for an incoming carry r, from the ZONE TABLE:
    czidx maps the byte offset to the first candidate with start >= r, and
    that candidate's recorded zone out IS the greedy from there — one load,
    no scan. Byte-walk fallback on overflow."""
    if Int(ccount[unsafe_offset=c]) == Int(CLIST_OVERFLOW):
        return chain_walk[False](
            bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance, r, stop
        )
    var cnt = Int(ccount[unsafe_offset=c])
    var j = Int(czidx[unsafe_offset = c * BLOCK + (r - c * BLOCK)])
    if j >= cnt:
        return 0
    return Int(clist[unsafe_offset = (c * CLIST_CAP + j) * CLIST_STRIDE + 3])


# The stitch, two levels: the carry recurrence over blocks is associative, so
# blocks group into 32-block super-blocks whose free-entry exits are computed
# in parallel (level 1); one thread then runs the recurrence over the
# super-blocks, evaluating a composed super-block on demand when the carry
# lands inside it (level 2); and each super-block re-stitches internally with
# the true entry (level 3). Same integer rule in the same order — the flat
# stitch's carries, bit for bit, with the serial pass 32x shorter.
comptime SB_BLOCKS = 32


def k_chain_sb_free(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    span_out: MutPointer[UInt32, MutAnyOrigin],
    sb_out: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    n_chunks: Int32,
):
    """Thread per super-block: the exit carry under free entry — the flat
    stitch over SB_BLOCKS blocks, in parallel across super-blocks."""
    var nsb = (Int(n_chunks) + SB_BLOCKS - 1) // SB_BLOCKS
    var sb = Int(global_idx.x)
    if sb >= nsb:
        return
    var n = Int(n_bytes)
    var c0 = sb * SB_BLOCKS
    var c1 = min(c0 + SB_BLOCKS, Int(n_chunks))
    var r = 0
    for c in range(c0, c1):
        var start = c * BLOCK
        var stop = min(start + BLOCK, n)
        if r <= start:
            r = Int(span_out[unsafe_offset=c])
        else:
            if r < stop:
                r = block_eval(
                    bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                    ccount, clist, czidx, c, r, stop,
                )
    sb_out[unsafe_offset=sb] = UInt32(r)


def k_chain_sb_stitch(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    span_out: MutPointer[UInt32, MutAnyOrigin],
    sb_out: MutPointer[UInt32, MutAnyOrigin],
    sb_resolved: MutPointer[UInt32, MutAnyOrigin],
    csc: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    n_chunks: Int32,
):
    """One thread over the super-blocks: the carry recurrence. Free entry uses
    the level-1 exit; a carry landing inside reads the CASCADED zone out —
    the composed super-block's exit for that entry, precomputed, one lookup —
    with the uncascaded chain loop kept for overflow blocks."""
    var n = Int(n_bytes)
    var nsb = (Int(n_chunks) + SB_BLOCKS - 1) // SB_BLOCKS
    var r = 0
    for sb in range(nsb):
        var sb_start = sb * SB_BLOCKS * BLOCK
        var sb_end = min(sb_start + SB_BLOCKS * BLOCK, n)
        if r <= sb_start:
            sb_resolved[unsafe_offset=sb] = 0
            r = Int(sb_out[unsafe_offset=sb])
        else:
            sb_resolved[unsafe_offset=sb] = UInt32(r)
            if r < sb_end:
                var c = min(r // BLOCK, Int(n_chunks) - 1)
                var start_c = c * BLOCK
                if Int(ccount[unsafe_offset=c]) != Int(CLIST_OVERFLOW):
                    # The cascaded zone out: the super-block's exit for this
                    # entry, precomputed — ONE lookup, no chain.
                    var jj = Int(czidx[unsafe_offset = c * BLOCK + (r - start_c)])
                    r = Int(csc[unsafe_offset = c * (CLIST_CAP + 1) + jj])
                else:
                    var stop_c = min((c + 1) * BLOCK, n)
                    var v = r
                    if r < stop_c:
                        v = block_eval(
                            bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                            ccount, clist, czidx, c, r, stop_c,
                        )
                    var c1 = min((sb + 1) * SB_BLOCKS, Int(n_chunks))
                    var cc = c + 1
                    while cc < c1:
                        var start_cc = cc * BLOCK
                        var stop_cc = min(start_cc + BLOCK, n)
                        if v <= start_cc:
                            v = Int(span_out[unsafe_offset=cc])
                        elif v >= stop_cc:
                            pass
                        else:
                            v = block_eval(
                                bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                                ccount, clist, czidx, cc, v, stop_cc,
                            )
                        cc += 1
                    r = v
            # r >= sb_end: the span covers the whole super-block — pass


def k_chain_sb_apply(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    span_out: MutPointer[UInt32, MutAnyOrigin],
    sb_resolved: MutPointer[UInt32, MutAnyOrigin],
    resolved: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    n_chunks: Int32,
):
    """Thread per super-block: the internal stitch with the TRUE entry carry,
    writing every block's resolved in-carry for k_chain_commit."""
    var nsb = (Int(n_chunks) + SB_BLOCKS - 1) // SB_BLOCKS
    var sb = Int(global_idx.x)
    if sb >= nsb:
        return
    var n = Int(n_bytes)
    var c0 = sb * SB_BLOCKS
    var c1 = min(c0 + SB_BLOCKS, Int(n_chunks))
    var r = Int(sb_resolved[unsafe_offset=sb])
    for c in range(c0, c1):
        var start = c * BLOCK
        var stop = min(start + BLOCK, n)
        if r <= start:
            resolved[unsafe_offset=c] = 0
            r = Int(span_out[unsafe_offset=c])
        else:
            resolved[unsafe_offset=c] = UInt32(r)
            if r < stop:
                r = block_eval(
                    bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                    ccount, clist, czidx, c, r, stop,
                )


def k_chain_commit(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],
    resolved: MutPointer[UInt32, MutAnyOrigin],
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    n_bytes: Int32,
    n_chunks: Int32,
):
    """Thread per block: the commit with the resolved in-carry. An incoming
    span's trailer leaders in [start, min(r, stop)) are zeroed HERE (the
    ownership rule: every byte's statics are written only by the block
    containing it); the greedy then runs over the compact list."""
    var c = Int(global_idx.x)
    if c >= Int(n_chunks):
        return
    var start = c * BLOCK
    var stop = min(start + BLOCK, Int(n_bytes))
    var r = Int(resolved[unsafe_offset=c])
    if r <= start and cand_blocks[unsafe_offset=c] == 0:
        return  # free block, no candidates — nothing to write
    if r > start:
        var p = start
        while p < min(r, stop) and (Int(bytes[unsafe_offset=p]) & 0xC0) == 0x80:
            p += 1
        var limit = min(r, stop)
        while p < limit:
            var c0 = Int(bytes[unsafe_offset=p])
            var nb2: Int
            if (c0 & 0x80) == 0x00:
                nb2 = 1
            elif (c0 & 0xE0) == 0xC0:
                nb2 = 2
            elif (c0 & 0xF0) == 0xE0:
                nb2 = 3
            else:
                nb2 = 4
            gi[unsafe_offset=p] = 0
            sm[unsafe_offset = p * SM_STRIDE + SM_ADVANCE] = Float32(0)
            fl[unsafe_offset=p] = fl[unsafe_offset=p] | UInt32(F_CLUSTER_TRAILER)
            p += nb2
    if r < stop:
        if Int(ccount[unsafe_offset=c]) == Int(CLIST_OVERFLOW):
            _ = chain_walk[True](
                bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                max(r, start), stop,
            )
        else:
            var base = c * CLIST_CAP * CLIST_STRIDE
            var cnt = Int(ccount[unsafe_offset=c])
            var ce = max(r, start)
            for k in range(cnt):
                var o = base + k * CLIST_STRIDE
                var s = Int(clist[unsafe_offset=o])
                if s >= ce:
                    # the greedy commit rule: a candidate inside the last
                    # committed span is never read — suppression by resume
                    var e = Int(clist[unsafe_offset = o + 1])
                    var slot = clist[unsafe_offset = o + 2]
                    gi[unsafe_offset=s] = slot
                    sm[unsafe_offset = s * SM_STRIDE + SM_ADVANCE] = bitmap_advance
                    var b0 = Int(bytes[unsafe_offset=s])
                    var nb: Int
                    if (b0 & 0x80) == 0x00:
                        nb = 1
                    elif (b0 & 0xE0) == 0xC0:
                        nb = 2
                    elif (b0 & 0xF0) == 0xE0:
                        nb = 3
                    else:
                        nb = 4
                    var p = s + nb
                    var limit = min(e, stop)
                    while p < limit:
                        var c0 = Int(bytes[unsafe_offset=p])
                        var nb2: Int
                        if (c0 & 0x80) == 0x00:
                            nb2 = 1
                        elif (c0 & 0xE0) == 0xC0:
                            nb2 = 2
                        elif (c0 & 0xF0) == 0xE0:
                            nb2 = 3
                        else:
                            nb2 = 4
                        gi[unsafe_offset=p] = 0
                        sm[unsafe_offset = p * SM_STRIDE + SM_ADVANCE] = Float32(0)
                        fl[unsafe_offset=p] = fl[unsafe_offset=p] | UInt32(F_CLUSTER_TRAILER)
                        p += nb2
                    ce = e


def k_chain_sb_cascade(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    ccount: MutPointer[UInt32, MutAnyOrigin],
    clist: MutPointer[UInt32, MutAnyOrigin],
    czidx: MutPointer[UInt8, MutAnyOrigin],
    span_out: MutPointer[UInt32, MutAnyOrigin],
    csc: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    n_chunks: Int32,
):
    """Thread per block: for every zone j (0..cnt), the SUPER-BLOCK's exit
    carry for an incoming carry in that zone — the block's zone out chained
    through the rest of the super-block with zone lookups (byte-walk fallback
    past an overflow block). Zone cnt is "no candidate >= r": the block's
    re-walk commits nothing and the remaining blocks chain free. Level 2 then
    reads ONE value per carried super-block instead of cascading."""
    var c = Int(global_idx.x)
    if c >= Int(n_chunks):
        return
    var n = Int(n_bytes)
    var sb_end_c = min(((c // SB_BLOCKS) + 1) * SB_BLOCKS, Int(n_chunks))
    var cnt = Int(ccount[unsafe_offset=c])
    if cnt == Int(CLIST_OVERFLOW) or cnt == 0:
        return
    var base = c * CLIST_CAP * CLIST_STRIDE
    var j = 0
    while j <= cnt:
        var v = 0
        if j < cnt:
            v = Int(clist[unsafe_offset = base + j * CLIST_STRIDE + 3])
        var k = c + 1
        while k < sb_end_c:
            var start_k = k * BLOCK
            var stop_k = min(start_k + BLOCK, n)
            if v <= start_k:
                v = Int(span_out[unsafe_offset=k])
            elif v >= stop_k:
                pass
            else:
                var kcnt = Int(ccount[unsafe_offset=k])
                if kcnt == Int(CLIST_OVERFLOW):
                    v = chain_walk[False](
                        bytes, cand_slot, cand_end, gi, sm, fl, bitmap_advance,
                        v, stop_k,
                    )
                else:
                    var jj = Int(czidx[unsafe_offset = k * BLOCK + (v - start_k)])
                    if jj >= kcnt:
                        v = 0
                    else:
                        v = Int(clist[unsafe_offset = (k * CLIST_CAP + jj) * CLIST_STRIDE + 3])
            k += 1
        csc[unsafe_offset = c * (CLIST_CAP + 1) + j] = UInt32(v)
        j += 1
