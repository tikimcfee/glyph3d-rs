# cluster_probe.mojo — the sequence pass's device probes: k_cluster_probe
# (thread per byte, candidacy) and k_decode_probe (the fused decode+probe,
# with the state-walk form), ported line-for-line from cluster_split.mojo's
# proven CPU split. Extracted from cluster_device.mojo in the 2026-09
# code-shape refactor (a pure move).

from max.gpu import global_idx
from std.utils import StaticTuple
from std.atomic import Atomic
from glyph_schema import SM_STRIDE, SM_ADVANCE, SM_HEIGHT
from glyph_trie import (
    BLOCK_SHIFT, BLOCK_MASK,
    TM_STRIDE, TM_ADVANCE, TM_HEIGHT,
    TC_STRIDE, TC_GLYPH_ID, TC_FLAGS, FLAG_MISSING,
    Trie,
)
from glyph_pipeline import (
    F_LEADER, F_NEWLINE, F_MISSING, F_CLUSTER_TRAILER, NEWLINE,
    item_search_device,
)
from cluster_tables import (
    KEY_CAP, HEAD_BMP_WORDS, BLOCK_LOG2,
    ST_STRIDE, ST_EMPTY, st_probe,
)

def k_cluster_probe(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    item_ranges: MutPointer[UInt32, MutAnyOrigin],    # 2 per item: byte_start, byte_end
    item_cluster: MutPointer[UInt32, MutAnyOrigin],   # per item: 1 = cluster mode
    seq: MutPointer[UInt32, MutAnyOrigin],          # the v2 sequence section, verbatim
    cand_bmp: MutPointer[UInt32, MutAnyOrigin],     # build_head_bitmap: the first members
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],  # presence per 128-byte block
    n_bytes: Int32,
    item_count: Int32,
    seq_count: Int32,
    seq_max: Int32,
):
    """One thread per byte — cluster_split.mojo's probe_clusters, ported.
    Candidacy is one bitmap load (the table's first members); a codepoint no
    sequence starts with exits there, never paying for the window or the
    search. The per-byte cluster/item-end arrays this once read are resolved
    on device now: one item_search_device per thread over the ranges the
    chain already uploads (no ownership check — the facts arrays it replaced
    encoded the same largest-start-≤-id rule)."""
    var id = Int(global_idx.x)
    var n = Int(n_bytes)
    if id >= n:
        return
    if Int(item_count) == 0:
        return  # unreachable through the harnesses — see gpu_pipeline's k_resolve_x
    var it = item_search_device(item_ranges, Int(item_count), id)
    if item_cluster[unsafe_offset=it] == 0:
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
    var stop = Int(item_ranges[unsafe_offset = it * 2 + 1])
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




def k_decode_probe[probe: Bool, walk: Bool = False](
    bytes: MutPointer[UInt8, MutAnyOrigin],
    block_index: MutPointer[UInt32, MutAnyOrigin],
    blocks_m: MutPointer[Float32, MutAnyOrigin],
    blocks_c: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    item_ranges: MutPointer[UInt32, MutAnyOrigin],    # 2 per item: byte_start, byte_end
    item_cluster: MutPointer[UInt32, MutAnyOrigin],   # per item: 1 = cluster mode
    seq: MutPointer[UInt32, MutAnyOrigin],          # the v2 sequence section, verbatim
    cand_bmp: MutPointer[UInt32, MutAnyOrigin],     # build_head_bitmap: the first members
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    cand_blocks: MutPointer[UInt32, MutAnyOrigin],  # presence per 128-byte block
    tab: MutPointer[UInt32, MutAnyOrigin],          # build_state_table's hash (walk only)
    tab_mask: UInt32,
    n_bytes: Int32,
    item_count: Int32,
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
        if Int(item_count) == 0:
            return  # unreachable through the harnesses — see k_cluster_probe
        var it = item_search_device(item_ranges, Int(item_count), id)
        if item_cluster[unsafe_offset=it] == 0:
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
            var stop = Int(item_ranges[unsafe_offset = it * 2 + 1])
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
            var stop = Int(item_ranges[unsafe_offset = it * 2 + 1])
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


