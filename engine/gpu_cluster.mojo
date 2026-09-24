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
    F_CLUSTER_TRAILER, NEWLINE, item_for_byte,
)
from fixture_io import load_pipe_fixture

comptime MAX_PRINTED = 8
# The kernels and their key-buffer cap moved to cluster_device.mojo, shared
# with gpu_pipeline.mojo — one definition so the harnesses can't drift the rule.
from cluster_device import (
    KEY_CAP, HEAD_BMP_WORDS, build_head_bitmap, k_cluster_probe, k_cluster_chain,
)



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
    var h_bmp = ctx.enqueue_create_host_buffer[DType.uint32](HEAD_BMP_WORDS)
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
    var head_bmp = build_head_bitmap(fx.trie)
    for i in range(HEAD_BMP_WORDS):
        h_bmp[i] = head_bmp[i]
    for i in range(item_count * 2):
        h_ir[i] = item_ranges[i]
    for i in range(item_count):
        h_ic[i] = item_cluster[i]

    var d_bytes = ctx.enqueue_create_buffer[DType.uint8](n)
    var d_gi = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_sm = ctx.enqueue_create_buffer[DType.float32](n * SM_STRIDE)
    var d_fl = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_seq = ctx.enqueue_create_buffer[DType.uint32](n_seq)
    var d_bmp = ctx.enqueue_create_buffer[DType.uint32](HEAD_BMP_WORDS)
    var d_ceof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cof = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_ir = ctx.enqueue_create_buffer[DType.uint32](item_count * 2)
    var d_ic = ctx.enqueue_create_buffer[DType.uint32](item_count)
    var d_cslot = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cend = ctx.enqueue_create_buffer[DType.uint32](n)
    var d_cblk = ctx.enqueue_create_buffer[DType.uint32]((n + 127) >> 7)
    ctx.enqueue_copy(dst_buf=d_bytes, src_buf=h_bytes)
    ctx.enqueue_copy(dst_buf=d_gi, src_buf=h_gi)
    ctx.enqueue_copy(dst_buf=d_sm, src_buf=h_sm)
    ctx.enqueue_copy(dst_buf=d_fl, src_buf=h_fl)
    ctx.enqueue_copy(dst_buf=d_seq, src_buf=h_seq)
    ctx.enqueue_copy(dst_buf=d_bmp, src_buf=h_bmp)
    ctx.enqueue_copy(dst_buf=d_ceof, src_buf=h_ceof)
    ctx.enqueue_copy(dst_buf=d_cof, src_buf=h_cof)
    ctx.enqueue_copy(dst_buf=d_ir, src_buf=h_ir)
    ctx.enqueue_copy(dst_buf=d_ic, src_buf=h_ic)
    d_cslot.enqueue_fill(0)
    d_cend.enqueue_fill(0)
    d_cblk.enqueue_fill(0)

    comptime B = 256
    var t0 = perf_counter_ns()
    ctx.enqueue_function[k_cluster_probe](
        d_bytes.unsafe_ptr(), d_cof.unsafe_ptr(), d_ceof.unsafe_ptr(),
        d_seq.unsafe_ptr(), d_bmp.unsafe_ptr(), d_gi.unsafe_ptr(), d_sm.unsafe_ptr(), d_fl.unsafe_ptr(),
        d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
        Int32(n), Int32(seq_count), Int32(fx.trie.seq_max),
        grid_dim=(n + B - 1) // B, block_dim=B,
    )
    ctx.enqueue_function[k_cluster_chain](
        d_bytes.unsafe_ptr(), d_ir.unsafe_ptr(), d_ic.unsafe_ptr(),
        d_cslot.unsafe_ptr(), d_cend.unsafe_ptr(), d_cblk.unsafe_ptr(),
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
