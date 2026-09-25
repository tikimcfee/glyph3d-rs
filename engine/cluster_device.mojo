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
from glyph_schema import SM_STRIDE, SM_ADVANCE
from glyph_pipeline import F_CLUSTER_TRAILER, NEWLINE, Trie

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
        _ = Atomic.max(cand_blocks + (id >> BLOCK_LOG2), UInt32(1))


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

