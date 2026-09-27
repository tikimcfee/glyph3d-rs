# cluster_chain.mojo — the sequence pass's chain kernels: the serial chain
# (thread per item), the free walk, the compact-list block forms and the
# stitch/apply/commit/cascade passes. Extracted from cluster_device.mojo in
# the 2026-09 code-shape refactor (a pure move).

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
from cluster_tables import BLOCK, BLOCK_LOG2, ST_STRIDE, ST_EMPTY, st_probe

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
