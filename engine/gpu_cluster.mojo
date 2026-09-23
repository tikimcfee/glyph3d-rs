# gpu_cluster.mojo — the sequence pass on real GPU threads: probe + chain.
#
# The two dispatches port cluster_split.mojo's proven split, line for line:
#
#   k_cluster_probe — one thread per byte. Leaders of cluster items decode
#       their codepoint, zero the invisible-by-design ranges in place, and
#       probe the sequence table (longest prefix, FE0F dropped from the key
#       but riding the span), writing a candidate (slot, end) per position.
#       No cross-thread state anywhere.
#   k_cluster_chain — one thread per ITEM: the greedy commit with its
#       single-integer carry (the byte offset past the active match). The
#       phantom case (cluster-overlap) is why a windowed OR is not this.
#
# The harness mirrors gpu_decode's discipline: the DECODED lanes are uploaded
# (decode is proven there), the two dispatches run on device, and the static
# tier read back is compared bit-for-bit against run_pipeline[split=True] —
# the CPU split form, itself proven against the serial rule by
# conformance_split. The fold never touches the static tier, so a
# post-pipeline reference is a post-resolve reference.
#
# Run: mojo run -I engine engine/gpu_cluster.mojo engine/fixtures/*.pipe.bin

from std.sys import argv, has_accelerator
from std.time import perf_counter_ns
from std.gpu import global_idx
from std.utils import StaticTuple
from max.gpu.host import DeviceContext
from glyph_schema import SM_STRIDE, SM_ADVANCE
from glyph_pipeline import (
    Item, run_pipeline, CLUSTER_LEADER, CLUSTER_CLUSTER,
    F_CLUSTER_TRAILER, NEWLINE,
)
from fixture_io import load_pipe_fixture

comptime MAX_PRINTED = 8
# Stack cap for a probe's key buffer (a StaticTuple —: today's tables run seq_max = 9, and
# VS16 riders can at most double a span's members; the suite refuses a table
# that exceeds it, a louder failure than a silent cap).
comptime KEY_CAP = 32


def k_cluster_probe(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    cluster_of: MutPointer[UInt32, MutAnyOrigin],   # per byte: 1 in a cluster item
    item_end: MutPointer[UInt32, MutAnyOrigin],     # per byte: the item's end offset
    seq: MutPointer[UInt32, MutAnyOrigin],          # the v2 sequence section, verbatim
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    n_bytes: Int32,
    seq_count: Int32,
    seq_max: Int32,
):
    """One thread per byte — cluster_split.mojo's probe_clusters, ported. The
    head-candidacy set is skipped on purpose: a codepoint no sequence starts
    with fails the table search anyway, so the answers are identical and only
    the work differs (the bitmap belongs to the measured-optimization step)."""
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

    # The longest table prefix of the key wins (the serial scan's order).
    var best_len = 0
    var best_slot = UInt32(0)
    var stride = 2 + Int(seq_max)
    var i2 = 0
    while i2 < Int(seq_count) * stride:
        var elen = Int(seq[unsafe_offset = i2 + 1])
        if elen >= 2 and elen <= key_len and elen > best_len:
            var ok = True
            var k = 0
            while k < elen:
                if seq[unsafe_offset = i2 + 2 + k] != key[k]:
                    ok = False
                    break
                k += 1
            if ok:
                best_len = elen
                best_slot = seq[unsafe_offset = i2]
        i2 += stride
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


def k_cluster_chain(
    bytes: MutPointer[UInt8, MutAnyOrigin],
    item_ranges: MutPointer[UInt32, MutAnyOrigin],    # 2 per item: byte_start, byte_end
    item_cluster: MutPointer[UInt32, MutAnyOrigin],   # per item: 1 = cluster mode
    cand_slot: MutPointer[UInt32, MutAnyOrigin],
    cand_end: MutPointer[UInt32, MutAnyOrigin],
    gi: MutPointer[UInt32, MutAnyOrigin],
    sm: MutPointer[Float32, MutAnyOrigin],
    fl: MutPointer[UInt32, MutAnyOrigin],
    bitmap_advance: Float32,
    item_count: Int32,
):
    """One thread per ITEM — the split form's commit. The carry is one
    integer; items tile the blob, so no two threads share a byte."""
    var i = Int(global_idx.x)
    if i >= Int(item_count):
        return
    if item_cluster[unsafe_offset=i] == 0:
        return
    var commit_end = 0
    var id = Int(item_ranges[unsafe_offset = i * 2])
    var stop = Int(item_ranges[unsafe_offset = i * 2 + 1])
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
        if slot != 0 and id >= commit_end:
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
            commit_end = end
        id += nb


def item_for_byte(items: List[Item], id: Int) -> Int:
    for i in range(len(items)):
        if id >= items[i].byte_start and id < items[i].byte_start + items[i].byte_count:
            return i
    return -1


def check_case(path: String, ctx: DeviceContext, mut saw_cluster: List[Bool]) raises -> Int:
    var fx = load_pipe_fixture(path)
    var n = fx.byte_len
    if n == 0:
        return 0
    var any_cluster = False
    for i in range(len(fx.items)):
        if fx.items[i].cluster_mode == CLUSTER_CLUSTER:
            any_cluster = True
    if any_cluster:
        saw_cluster[0] = True
    if fx.trie.seq_max > KEY_CAP:
        raise Error("sequence table's seq_max exceeds the kernel's key cap")
    var seq_count = 0
    if fx.trie.seq_max > 0:
        seq_count = len(fx.trie.seq) // (2 + fx.trie.seq_max)

    # ── CPU references, both through the proven pipeline: the pure DECODE
    # (leader-forced items — the pass never runs), and the SPLIT form (the
    # device kernels' reference). ────────────────────────────────────────────
    var items_leader = List[Item]()
    for i in range(len(fx.items)):
        var t = fx.items[i].copy()
        t.cluster_mode = CLUSTER_LEADER
        items_leader.append(t^)
    var dec = run_pipeline[witness=False](fx.bytes, fx.trie, items_leader^)
    var want = run_pipeline[cluster_split=True](fx.bytes, fx.trie, fx.items)

    # Per-byte facts, the same flattened shape the scan pipeline uploads.
    var cluster_of = List[UInt32](length=n, fill=0)
    var item_end = List[UInt32](length=n, fill=0)
    for id in range(n):
        var i = item_for_byte(fx.items, id)
        if i >= 0:
            item_end[id] = UInt32(fx.items[i].byte_start + fx.items[i].byte_count)
            cluster_of[id] = UInt32(1) if fx.items[i].cluster_mode == CLUSTER_CLUSTER else UInt32(0)

    var item_count = len(fx.items)
    var item_ranges = List[UInt32](length=item_count * 2, fill=0)
    var item_cluster = List[UInt32](length=item_count, fill=0)
    for i in range(item_count):
        item_ranges[i * 2] = UInt32(fx.items[i].byte_start)
        item_ranges[i * 2 + 1] = UInt32(fx.items[i].byte_start + fx.items[i].byte_count)
        item_cluster[i] = UInt32(1) if fx.items[i].cluster_mode == CLUSTER_CLUSTER else UInt32(0)

    # ── upload ───────────────────────────────────────────────────────────────
    var n_seq = len(fx.trie.seq) if len(fx.trie.seq) > 0 else 1
    var h_bytes = ctx.enqueue_create_host_buffer[DType.uint8](n)
    var h_gi = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_sm = ctx.enqueue_create_host_buffer[DType.float32](n * SM_STRIDE)
    var h_fl = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_seq = ctx.enqueue_create_host_buffer[DType.uint32](n_seq)
    var h_ceof = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_cof = ctx.enqueue_create_host_buffer[DType.uint32](n)
    var h_ir = ctx.enqueue_create_host_buffer[DType.uint32](item_count * 2)
    var h_ic = ctx.enqueue_create_host_buffer[DType.uint32](item_count)
    ctx.synchronize()
    for i in range(n):
        h_bytes[i] = fx.bytes[i]
        h_gi[i] = dec.gi[i]
        h_fl[i] = dec.fl[i]
        h_ceof[i] = item_end[i]
        h_cof[i] = cluster_of[i]
    for i in range(n * SM_STRIDE):
        h_sm[i] = dec.sm[i]
    for i in range(len(fx.trie.seq)):
        h_seq[i] = fx.trie.seq[i]
    if len(fx.trie.seq) == 0:
        h_seq[0] = 0
    for i in range(item_count * 2):
        h_ir[i] = item_ranges[i]
    for i in range(item_count):
        h_ic[i] = item_cluster[i]

    var d_bytes = ctx.enqueue_create_buffer[DType.uint8](n)
    var d_gi = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_sm = ctx.enqueue_create_buffer[DType.float32](n * SM_STRIDE)
    var d_fl = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_seq = ctx.enqueue_create_buffer[DType.uint32](n_seq)
    var d_ceof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_ir = ctx.enqueue_create_buffer[DType.uint32](item_count * 2)
    var d_ic = ctx.enqueue_create_buffer[DType.uint32](item_count)
    var d_cslot = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cend = ctx.enqueue_create_buffer[DType.uint32](n)
    ctx.enqueue_copy(dst_buf=d_bytes, src_buf=h_bytes)
    ctx.enqueue_copy(dst_buf=d_gi, src_buf=h_gi)
    ctx.enqueue_copy(dst_buf=d_sm, src_buf=h_sm)
    ctx.enqueue_copy(dst_buf=d_fl, src_buf=h_fl)
    ctx.enqueue_copy(dst_buf=d_seq, src_buf=h_seq)
    ctx.enqueue_copy(dst_buf=d_ceof, src_buf=h_ceof)
    ctx.enqueue_copy(dst_buf=d_cof, src_buf=h_cof)
    ctx.enqueue_copy(dst_buf=d_ir, src_buf=h_ir)
    ctx.enqueue_copy(dst_buf=d_ic, src_buf=h_ic)
    d_cslot.enqueue_fill(0)
    d_cend.enqueue_fill(0)

    comptime B = 256
    var t0 = perf_counter_ns()
    ctx.enqueue_function[k_cluster_probe](
        d_bytes.unsafe_ptr(), d_cof.unsafe_ptr(), d_ceof.unsafe_ptr(),
        d_seq.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
        d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
        Int32(n), Int32(seq_count), Int32(fx.trie.seq_max),
        grid_dim=(n + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_cluster_chain](
        d_bytes.unsafe_ptr(), d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
        d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(),
        d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
        fx.trie.bitmap_advance, Int32(item_count),
        grid_dim=(item_count + 63) // 64, block_dim=64,
    )
    ctx.enqueue_copy(dst_buf=h_gi, src_buf=d_gi)
    ctx.enqueue_copy(dst_buf=h_sm, src_buf=d_sm)
    ctx.enqueue_copy(dst_buf=h_fl, src_buf=d_fl)
    ctx.synchronize()
    var gpu_ns = perf_counter_ns() - t0
    # Dispatch + readback timing per fixture — the baseline the fusion
    # experiments answer to. At fixture sizes this is dispatch latency, not
    # throughput; the trio's scale numbers come with the pipeline integration.
    print("  probe+chain:", Float64(gpu_ns) / 1e6, "ms on device")

    # ── bit-for-bit against the CPU split form, no tolerance ────────────────
    var bad = 0
    var printed = 0
    for i in range(n):
        if h_gi[i] != want.gi[i]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", i, "GLYPH_ID — gpu", h_gi[i], "cpu-split", want.gi[i])
                printed += 1
        if h_fl[i] != want.fl[i]:
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", i, "flags — gpu", h_fl[i], "cpu-split", want.fl[i])
                printed += 1
    for i in range(n * SM_STRIDE):
        if UInt32(h_sm[i].to_bits()) != UInt32(want.sm[i].to_bits()):
            bad += 1
            if printed < MAX_PRINTED:
                print("  byte", i // SM_STRIDE, "static lane", i % SM_STRIDE,
                      "— gpu", h_sm[i], "cpu-split", want.sm[i])
                printed += 1
    return bad


def main() raises:
    comptime assert has_accelerator(), "gpu_cluster requires a GPU"
    var args = argv()
    if len(args) < 2:
        print("usage: mojo run -I engine engine/gpu_cluster.mojo <fixture.pipe.bin> ...")
        return
    var ctx = DeviceContext()
    print("device:", ctx.name())
    var total_bad = 0
    var saw_cluster = List[Bool](length=1, fill=False)
    var cases = 0
    for i in range(1, len(args)):
        var path = String(args[i])
        var bad = check_case(path, ctx, saw_cluster)
        cases += 1
        if bad == 0:
            print("PASS", path)
        else:
            print("FAIL", path, "—", bad, "differing lanes")
        total_bad += bad
    if total_bad != 0:
        raise Error("gpu cluster diverged")
    if not saw_cluster[0]:
        raise Error("gpu_cluster ran no cluster fixture — the corpus moved")
    print("")
    print("gpu cluster: probe+chain on device is bit-exact with the CPU split")
    print("form on", cases, "fixtures (glyph id, advance, flags — every byte).")
