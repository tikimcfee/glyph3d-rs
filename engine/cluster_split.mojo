# cluster_split.mojo — the sequence pass, SPLIT: probe everywhere, then commit.
#
# The serial rule (glyph_cluster.mojo's resolve_clusters) is a left-to-right
# walk that never revisits a consumed position. That shape cannot run one GPU
# thread per leader — a thread at an interior position cannot know whether an
# earlier match swallowed it. The split decomposes the rule into a
# per-position PROBE (no cross-position state — the shape of a device kernel)
# and a COMMIT chain (one small carry: the byte offset past the active match).
#
# THE CASE THAT FORCES THE CHAIN (cluster-overlap.pipe.bin): with both (A C)
# and (C A) in the table, 🇦🇨🇦 probes candidates at 0 AND 1, but the greedy
# rule commits only position 0 — position 1's candidate is a phantom and its
# span must NOT swallow position 2. A commit built as "any candidate covering
# me is a trailer" gets this wrong; the carry gets it right.
#
# The two functions write the same three lanes resolve_clusters writes (glyph
# id, advance, flags), bit-exact — conformance_split proves it against the
# serial rule over the whole corpus. gpu_cluster.mojo carries the device port
# of both (probe per byte, chain per item); keep these free of allocation and
# cross-position state outside the carry so the port stays a transcription.

from std.collections import Dict
from std.collections.span import Span
from glyph_pipeline import (
    Slots, Item, sequence_length, decode_codepoint_at, F_CLUSTER_TRAILER,
    NEWLINE
)
from glyph_trie import Trie
from glyph_cluster import is_static_zero_cp


def probe_clusters[o: ImmOrigin](
    bytes: Span[UInt8, o],
    slots: Slots,
    trie: Trie,
    item: Item,
    cand_slot: Pointer[UInt32, MutUntrackedOrigin],
    cand_end: Pointer[UInt32, MutUntrackedOrigin],
):
    """PASS 1 — every leader probed independently. Statics are zeroed in place
    (unconditional, exactly as the serial rule); a head whose probe matches
    writes its candidate: the slot and the byte offset PAST its last span
    member. cand_* must be zeroed by the caller (the device harnesses
    zero-fill the buffers). Reads nothing it does not own: the probe window
    clamps to the item's end, as the serial rule's `stop` does."""
    if len(trie.seq) == 0:
        return
    var seq_max = trie.seq_max
    var stride = 2 + seq_max
    var stop = item.byte_start + item.byte_count

    # Instant rejection: a codepoint no sequence starts with is never a head.
    var first = Dict[Int, Bool]()
    var i = 0
    while i < len(trie.seq):
        first[Int(trie.seq[i + 2])] = True
        i += stride

    var id = item.byte_start
    while id < stop:
        var n = sequence_length(bytes, id)
        if n == 0:
            id += 1
            continue
        var cp = decode_codepoint_at(bytes, id, n)
        if is_static_zero_cp(cp):
            slots.set_glyph_id(id, 0)
            slots.set_advance(id, Float32(0))
            slots.set_flags(id, slots.flags(id) | F_CLUSTER_TRAILER)
            id += n
            continue
        if cp in first:
            # The probe: up to seqMax EFFECTIVE codepoints, VS16 dropped from
            # the key but riding the span; newline/VS15/continuation/item-end
            # break it. Verbatim the serial rule's window.
            var members = List[Int]()
            members.append(id)
            var key = List[Int]()
            key.append(cp)
            var p = id + n
            while p < stop and len(key) < seq_max:
                var n2 = sequence_length(bytes, p)
                if n2 == 0:
                    break
                var cp2 = decode_codepoint_at(bytes, p, n2)
                if cp2 == NEWLINE or cp2 == 0xFE0E:
                    break
                members.append(p)
                if cp2 != 0xFE0F:
                    key.append(cp2)
                p += n2
            var best_len = 0
            var best_slot = 0
            var i2 = 0
            while i2 < len(trie.seq):
                var elen = Int(trie.seq[i2 + 1])
                if elen >= 2 and elen <= len(key) and elen > best_len:
                    var is_match = True
                    var k = 0
                    while k < elen:
                        if Int(trie.seq[i2 + 2 + k]) != key[k]:
                            is_match = False
                            break
                        k += 1
                    if is_match:
                        best_len = elen
                        best_slot = Int(trie.seq[i2])
                i2 += stride
            if best_len > 0:
                # The span runs through the leader that contributed the key's
                # last matched member — count key-consumers, not members, so
                # the skipped VS16s stay inside the trailer span.
                var span_members = 0
                var need = best_len
                while need > 0:
                    var mid = members[span_members]
                    if decode_codepoint_at(bytes, mid, sequence_length(bytes, mid)) != 0xFE0F:
                        need -= 1
                    span_members += 1
                var last = members[span_members - 1]
                cand_slot[unsafe_offset=id] = UInt32(best_slot)
                cand_end[unsafe_offset=id] = UInt32(last + sequence_length(bytes, last))
        id += n


def chain_clusters[o: ImmOrigin](
    bytes: Span[UInt8, o],
    slots: Slots,
    trie: Trie,
    item: Item,
    cand_slot: Pointer[UInt32, MutUntrackedOrigin],
    cand_end: Pointer[UInt32, MutUntrackedOrigin],
):
    """PASS 2 — the greedy commit. One walk per item, one carry: the byte
    offset the walk resumes at after a commit. A candidate inside a committed
    span is a phantom and is never read — the walk jumps from the head past
    the span, so suppression is by construction (cluster-overlap proves it;
    the suite's mutation removes the resume line and watches the phantom
    fire). A candidate the walk reaches commits: the head gets the sequence
    slot at the trie's bitmap advance, and every leader in (head, end) is
    zeroed + flagged — the same set the serial rule's members list carried
    (statics inside it were already zeroed by the probe; the flag OR is
    idempotent)."""
    var stop = item.byte_start + item.byte_count
    var id = item.byte_start
    while id < stop:
        var n = sequence_length(bytes, id)
        if n == 0:
            id += 1
            continue
        var slot = Int(cand_slot[unsafe_offset=id])
        if slot != 0:
            slots.set_glyph_id(id, UInt32(slot))
            slots.set_advance(id, trie.bitmap_advance)
            var end = Int(cand_end[unsafe_offset=id])
            var p = id + n
            while p < end:
                var n2 = sequence_length(bytes, p)
                if n2 == 0:
                    p += 1
                    continue
                slots.set_glyph_id(p, 0)
                slots.set_advance(p, Float32(0))
                slots.set_flags(p, slots.flags(p) | F_CLUSTER_TRAILER)
                p += n2
            id = end  # the span's members are written; resume past it
        else:
            id += n
