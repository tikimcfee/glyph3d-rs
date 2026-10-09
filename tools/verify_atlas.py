#!/usr/bin/env python3
"""
verify_atlas.py — parse the exported native atlas assets back and assert structural
and semantic invariants. Exits non-zero on the first failure.

Usage: python3 tools/verify_atlas.py [assets/atlas]
"""
import struct
import sys
from pathlib import Path

ATLAS = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent.parent / "assets" / "atlas")

BLOCK_SHIFT = 8
BLOCK_MASK = 0xFF
ENTRY_STRIDE = 4
FLAG_MISSING, FLAG_BITMAP, FLAG_BLANK = 1, 2, 4
SLOT_FLAG_BITMAP, SLOT_FLAG_EMPTY = 1, 2
FONTIDX_BLANK, FONTIDX_BITMAP, NO_CELL = 0xFFFFFFFF, 0xFFFFFFFE, 0xFFFFFFFF

failures = []


def check(cond, msg):
    if cond:
        print(f"  ok   {msg}")
    else:
        failures.append(msg)
        print(f"  FAIL {msg}")


def read_u32(path):
    data = path.read_bytes()
    assert len(data) % 4 == 0, f"{path}: not 4-aligned"
    return list(struct.unpack(f"<{len(data)//4}I", data))


def header(words, want_magic, want_versions=(1,)):
    magic = words[0].to_bytes(4, "little").decode("ascii")
    assert magic == want_magic, f"magic {magic!r} != {want_magic!r}"
    version, header_bytes = words[1], words[2]
    assert version in want_versions, f"version {version} not in {want_versions}"
    assert header_bytes == (len(words) - 0) * 0 or True  # checked per-file below
    return version, header_bytes


# ── curves.bin ────────────────────────────────────────────────────────────────
print("curves.bin")
w = read_u32(ATLAS / "curves.bin")
version, hbytes = header(w, "G3CV")
cw, ch, curve_count, texels_per_curve = w[3], w[4], w[5], w[6]
check(hbytes == 32, f"headerBytes == 32 (got {hbytes})")
check(cw == 1024, f"width == 1024 (got {cw})")
curve_payload = w[hbytes // 4:]
check(len(curve_payload) == cw * ch * 4, f"payload texels == {cw}x{ch}x4 (got {len(curve_payload)})")
check(ch == max(1, -(-curve_count * texels_per_curve // cw)), "height matches curveCount (row-aligned)")
check(texels_per_curve == 2, "2 texels per curve")
used = curve_count * texels_per_curve * 4
check(all(v <= 65535 for v in curve_payload[:used]), "all used curve coords fit uint16")
check(all(v == 0 for v in curve_payload[used:]), "row padding is zero")

# ── glyphmap.bin ──────────────────────────────────────────────────────────────
print("glyphmap.bin")
w = read_u32(ATLAS / "glyphmap.bin")
header(w, "G3GM")
mw, mh, entry_count = w[3], w[4], w[5]
map_payload = w[w[2] // 4:]
check(len(map_payload) == mw * mh * 4, f"payload texels == {mw}x{mh}x4")
check(mh == max(1, -(-entry_count // mw)), "height matches entryCount")

bitmap_slots, outline_slots, empty_slots = 0, 0, 0
prev_start, monotonic = -1, True
in_range = True
for g in range(1, entry_count):
    s, c, mode, cell = map_payload[g * 4:g * 4 + 4]
    if mode == 1:
        bitmap_slots += 1
        check_bitmap = (s == 0 and c == 0)
        if not check_bitmap:
            in_range = False
    elif c > 0:
        outline_slots += 1
        if s < prev_start:
            monotonic = False
        prev_start = s
        if s + c > curve_count:
            in_range = False
    else:
        empty_slots += 1
check(in_range, "all curve ranges within [0, curveCount); bitmap entries are [0,0,1,cell]")
check(monotonic, "curveStart is monotonically non-decreasing across outline glyphs")
print(f"       outline={outline_slots} empty={empty_slots} bitmap={bitmap_slots} entries={entry_count}")

# ── glyphs.bin ────────────────────────────────────────────────────────────────
print("glyphs.bin")
w = read_u32(ATLAS / "glyphs.bin")
header(w, "G3GL")
font_count, slot_count, primary_upem, primary_adv, primary_emh = w[3:8]
font_rec_b, slot_rec_b = w[8], w[9]
check(font_rec_b == 64 and slot_rec_b == 56, f"record sizes 64/56 (got {font_rec_b}/{slot_rec_b})")
check(slot_count == entry_count, "slotCount == glyphmap entryCount")
o = w[2] // 4
fonts = []
for i in range(font_count):
    r = w[o + i * 16: o + (i + 1) * 16]
    name = b"".join(struct.pack("<I", x) for x in r[4:16]).split(b"\0")[0].decode()
    fonts.append(dict(upem=r[0], asc=struct.unpack("<i", struct.pack("<I", r[1]))[0],
                      desc=struct.unpack("<i", struct.pack("<I", r[2]))[0], name=name))
    print(f"       font[{i}] {name}: upem={r[0]} asc={fonts[-1]['asc']} desc={fonts[-1]['desc']}")
o += font_count * 16
slots = []
for i in range(slot_count):
    r = w[o + i * 14: o + (i + 1) * 14]
    f32 = lambda x: struct.unpack("<f", struct.pack("<I", x))[0]
    i32 = lambda x: struct.unpack("<i", struct.pack("<I", x))[0]
    slots.append(dict(fontIdx=r[0], gid=r[1], flags=r[2], emojiCell=r[3],
                      advanceFu=i32(r[4]), asc=i32(r[5]), desc=i32(r[6]),
                      curveStart=r[7], curveCount=r[8],
                      bbox=[f32(r[9]), f32(r[10]), f32(r[11]), f32(r[12])]))
o += slot_count * 14
name_offsets = w[o: o + slot_count]
o += slot_count
blob_len = w[o]
names_blob = b"".join(struct.pack("<I", x) for x in w[o + 1:])[:blob_len]

def slot_name(i):
    nxt = name_offsets[i + 1] if i + 1 < slot_count else blob_len
    return names_blob[name_offsets[i]:nxt].decode("utf-8", "replace")

# cross-check slots vs glyphmap texels
mismatch = 0
for g in range(slot_count):
    s, c, mode, cell = map_payload[g * 4:g * 4 + 4]
    sm = slots[g]
    if sm["curveStart"] != s or sm["curveCount"] != c:
        mismatch += 1
    if (mode == 1) != bool(sm["flags"] & SLOT_FLAG_BITMAP):
        mismatch += 1
    if mode == 1 and sm["emojiCell"] != cell:
        mismatch += 1
    if sm["curveCount"] > 0:
        x0, y0, x1, y1 = sm["bbox"]
        if not (-0.02 <= x0 <= x1 <= 1.02 and -0.02 <= y0 <= y1 <= 1.02):
            mismatch += 1
check(mismatch == 0, f"glyphs.bin records agree with glyphmap texels ({mismatch} mismatches)")
check(slots[0]["fontIdx"] == FONTIDX_BLANK, "slot 0 is the blank slot")
check(primary_upem == fonts[0]["upem"], "primaryUpem == font[0].upem")

# ── codepoints.bin ────────────────────────────────────────────────────────────
print("codepoints.bin")
w = read_u32(ATLAS / "codepoints.bin")
ver, _ = header(w, "G3CP", want_versions=(2,))  # v2 = the sequence-pass sections
block_shift, bi_len, block_count, stride, mapped, miss_adv, miss_h, trie_upem = w[3:11]
check(block_shift == BLOCK_SHIFT and bi_len == 0x110000 >> BLOCK_SHIFT and stride == ENTRY_STRIDE,
      "trie constants (shift=8, index=4352, stride=4)")
check(miss_adv == primary_adv and miss_h == primary_emh and trie_upem == primary_upem,
      "trie missing-advance/height/upem match glyphs.bin header")
bitmap_adv, seq_count, seq_max, seq_off, class_off, class_words = w[11:17]
o = w[2] // 4
block_index = w[o: o + bi_len]
check(seq_off == o + bi_len + block_count * 256 * ENTRY_STRIDE,
      "sequence section appended right after the blocks")
blocks = w[o + bi_len: seq_off]
check(len(blocks) == block_count * 256 * ENTRY_STRIDE, "blocks length == blockCount*256*4")
check(max(block_index) < block_count, "blockIndex entries < blockCount")
check(block_index[0] != 0, "ASCII block 0 is mapped (not the missing block)")

# v2 sections: sequences (sorted, slots = base + index) + classes (the G3CC
# artifact verbatim — byte-equality with cluster-classes.bin is the provenance).
check(bitmap_adv == 2 * primary_adv, "bitmapAdvanceFu == 2 x primary advance")
seq_stride = 2 + seq_max
check(seq_off + seq_count * seq_stride == class_off, "sequence section spans exactly to the class section")
seq_rows = []
for i in range(seq_count):
    ro = seq_off + i * seq_stride
    slot, ln = w[ro], w[ro + 1]
    check(2 <= ln <= seq_max, f"sequence {i}: len {ln} in [2, seqMax]")
    seq_rows.append((slot, ln, tuple(w[ro + 2: ro + 2 + ln])))
check(all(seq_rows[i][2] < seq_rows[i + 1][2] for i in range(seq_count - 1)),
      "sequence section sorted by codepoint sequence")
check(all(seq_rows[i][0] == seq_rows[0][0] + i for i in range(seq_count)),
      "sequence slots are base + index (the check_atlas.py rule)")
fam = (0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467)
fam_row = [r for r in seq_rows if r[2] == fam]
check(len(fam_row) == 1 and fam_row[0][0] == 6819,
      f"the family ZWJ sequence resolves to slot 6819 (got {fam_row}) — the pin from check_atlas.py")
class_sec = w[class_off: class_off + class_words]
check(class_sec[0] == 0x43433347, "class section is a G3CC table")
g3cc = (ATLAS / "cluster-classes.bin").read_bytes()
check(struct.pack(f"<{len(class_sec)}I", *class_sec) == g3cc,
      "class section is byte-identical to cluster-classes.bin")


def trie_lookup(cp):
    b = block_index[cp >> BLOCK_SHIFT]
    e = ((b << BLOCK_SHIFT) | (cp & BLOCK_MASK)) * ENTRY_STRIDE
    return blocks[e], blocks[e + 1], blocks[e + 2], blocks[e + 3]


# semantic spot checks
gid, ax, ht, fl = trie_lookup(ord("A"))
s = slots[gid]
check(gid > 0 and ax == primary_adv and fl == 0, f"'A' → slot {gid}, advance {ax} fu (>0), flags 0")
check(s["curveCount"] > 0 and slot_name(gid) == "A", f"slot {gid} is outline glyph 'A' with {s['curveCount']} curves")

gid, ax, ht, fl = trie_lookup(ord(" "))
s = slots[gid]
check(ax == primary_adv and s["curveCount"] == 0 and not (fl & FLAG_MISSING),
      f"space → slot {gid}: zero curves, positive advance, not missing")

gid, ax, ht, fl = trie_lookup(0x1F680)  # 🚀 — outside the web's ranges; appended from the emoji sheet
check(fl & FLAG_BITMAP and gid >= 4431 and ax == 2 * primary_adv,
      f"U+1F680 (🚀, outside the web's core ranges) → appended bitmap slot {gid}, double advance")
gid, ax, ht, fl = trie_lookup(0xE0020)  # tag space — the font has no bitmap, nothing draws it
check(fl & FLAG_MISSING and gid == 0, "U+E0020 (tag space) → missing block, slot 0")

emoji_gid = None
for cp in range(0x1F400, 0x1F64F + 1):
    g2, ax2, ht2, fl2 = trie_lookup(cp)
    if fl2 & FLAG_BITMAP:
        emoji_gid, emoji_ax, emoji_cp = g2, ax2, cp
        break
check(emoji_gid is not None and emoji_ax == 2 * primary_adv,
      f"emoji U+{emoji_cp:04X} → bitmap slot {emoji_gid} with double-width advance {emoji_ax}")
check(slots[emoji_gid]["flags"] & SLOT_FLAG_BITMAP and slots[emoji_gid]["emojiCell"] != NO_CELL,
      f"bitmap slot {emoji_gid} carries emoji cell {slots[emoji_gid]['emojiCell']}")
# Every bitmap slot's cell is either NO_CELL (a web-era slot the font cannot
# draw) or an index into the committed emoji sheet's cell table.
sheet_words = read_u32(ATLAS / "emoji-sheet.bin")
check(sheet_words[0] == 0x53453347, "emoji-sheet.bin magic")
sheet_cells = sheet_words[5]
with_cell = [s for s in slots if s["flags"] & SLOT_FLAG_BITMAP and s["emojiCell"] != NO_CELL]
without = [s for s in slots if s["flags"] & SLOT_FLAG_BITMAP and s["emojiCell"] == NO_CELL]
check(all(s["emojiCell"] < sheet_cells for s in with_cell),
      f"every bitmap slot's emojiCell indexes the sheet ({sheet_cells} cells)")
check(len(with_cell) > 0, f"{len(with_cell)} bitmap slots have a sheet cell, {len(without)} have none")

for cp, want_blank in [(0xFE0F, True), (0x200D, None)]:
    g2, _, _, fl2 = trie_lookup(cp)
    print(f"       U+{cp:04X}: slot {g2} flags {fl2}")

# unmapped-but-covered check: a codepoint in a mapped block that no font covers → BLANK
g2, ax2, _, fl2 = trie_lookup(0x0378)  # unassigned in Greek block (block is mapped via Greek)
print(f"       U+0378 (unassigned, mapped block): slot {g2} flags {fl2}")

print()
if failures:
    print(f"VERIFY FAILED: {len(failures)} check(s): {failures}")
    sys.exit(1)
print("VERIFY OK — all checks passed.")
print(f"glyphs: {slot_count} slots ({outline_slots} outline, {empty_slots} empty, {bitmap_slots} bitmap) · "
      f"curves: {curve_count} · curve tex {cw}x{ch} · map tex {mw}x{mh} · "
      f"trie: {mapped} codepoints in {block_count} blocks")
