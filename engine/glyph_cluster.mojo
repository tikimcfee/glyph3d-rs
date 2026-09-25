# glyph_cluster.mojo — the sequence pass's resolution rule.
#
# A TRANSCRIPTION of the oracle's resolveClusters
# (engine/fixtures/inputs/glyphPipelineReference.js) — the same rule, mirrored
# statement for statement. The oracle is the semantic reference; this file does
# not re-derive it. When they disagree the oracle is wrong only after the
# c9667ec protocol (fix the oracle, regenerate the corpus, let it red here).
#
# THE RULE, once: per item, a serial walk over its leaders between decode and
# the fold. The invisible-by-design ranges (ZWJ, the variation selectors, the
# tag characters) never occupy a cell. A codepoint some sequence starts with
# probes the table for the LONGEST prefix match (FE0F normalized out of the
# key — the font's GSUB strips it — while the VS16 leaders ride the trailer
# span); a match gives the head the sequence slot at the trie's bitmap advance
# and the span's trailing leaders glyph 0 + advance 0 + F_CLUSTER_TRAILER.
# RI pairs need no parity state: the serial skip-past pairs them greedily from
# the left, which is exactly GB12/GB13.
#
# What this deliberately does NOT read: the class table (trie.classes). Head
# candidacy comes from the table's first-member set and the zero set is three
# named Unicode ranges — the vendored GCB classes are carried for the general
# UAX #29 phase and read by nothing in this pass.

from std.collections import Dict
from std.collections.span import Span
from glyph_pipeline import (
    Slots, Trie, Item, sequence_length, decode_codepoint_at,
    F_CLUSTER_TRAILER, NEWLINE,
)


def is_static_zero_cp(cp: Int) -> Bool:
    """The invisible-by-design ranges: the ZERO WIDTH JOINER, the variation
    selectors, the tag characters. Under cluster mode they never occupy a
    cell — a sequence match claims them as trailers first."""
    return cp == 0x200D or (cp >= 0xFE00 and cp <= 0xFE0F) or (cp >= 0xE0020 and cp <= 0xE007F)


def resolve_clusters[o: ImmOrigin](
    bytes: Span[UInt8, o], slots: Slots, trie: Trie, item: Item
):
    """One item, one serial walk over its leaders. Rewrites the static lanes
    only; the fold reads them exactly as it always did.
    """
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
            # Build the probe: up to seqMax EFFECTIVE codepoints, with VS16
            # dropped from the key but riding along as a span member. A newline
            # or a continuation byte ends the candidate; VS15 breaks it (it
            # asks for text presentation — honoring the break is what lets text
            # presentation stay text).
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
            # The longest table prefix of the key wins, found by descending
            # length with a binary search per length — the section is sorted
            # elementwise, prefix-first (asserted at bake), which is the order
            # atlas.rs's sequence_lookup relies on too. ~50 words per probe
            # instead of the table's whole 4,166 entries; answers identical by
            # construction (the longest exact prefix is unique).
            var best_len = 0
            var best_slot = 0
            var ln = min(len(key), seq_max)
            while ln >= 2 and best_len == 0:
                var lo = 0
                var hi = len(trie.seq) // stride
                while lo < hi:
                    var mid = (lo + hi) // 2
                    var o = mid * stride
                    var elen = Int(trie.seq[o + 1])
                    var cmp = 0
                    var k = 0
                    var nmin = min(ln, elen)
                    while k < nmin:
                        var a = key[k]
                        var b = Int(trie.seq[o + 2 + k])
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
                if lo < len(trie.seq) // stride:
                    var o2 = lo * stride
                    if Int(trie.seq[o2 + 1]) == ln:
                        var ok = True
                        var k2 = 0
                        while k2 < ln:
                            if key[k2] != Int(trie.seq[o2 + 2 + k2]):
                                ok = False
                                break
                            k2 += 1
                        if ok:
                            best_len = ln
                            best_slot = Int(trie.seq[o2])
                ln -= 1
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
                slots.set_glyph_id(id, UInt32(best_slot))
                slots.set_advance(id, trie.bitmap_advance)
                var k2 = 1
                while k2 < span_members:
                    var to = members[k2]
                    slots.set_glyph_id(to, 0)
                    slots.set_advance(to, Float32(0))
                    slots.set_flags(to, slots.flags(to) | F_CLUSTER_TRAILER)
                    k2 += 1
                id = members[span_members - 1] + sequence_length(bytes, members[span_members - 1])
                continue
        id += n
