#!/usr/bin/env python3
"""check_atlas.py — the atlas lookup tables, checked for what they MEAN.

The committed-artifacts gate proves `codepoints.bin` and `glyphs.bin` are
byte-identical to a fresh `export-atlas.mjs` run. That says they are what they
were, never that what they were is right. This checks the right part: the two
tables agree with each other, carry the shape the loaders assume, and resolve
pinned codepoints and sequences to the slots and advances they must.

HISTORY. This was `gen_real_trie.py` (ported from `gen-real-trie.mjs`,
2026-09-02), which baked `engine-trie.bin`: the same tables re-containered in
world units for the retired engine. No Rust code read that blob after the
engine's retirement — every layout backend resolves through `codepoints.bin`
and converts with `text::fu_to_world` — so it was retired on 2026-10-09 (D9).
Its checks were the valuable part and stay here. Its domain sweep (the
font-unit -> world conversion is the NEAREST f32 to the exact quotient) moved
to Rust as `text::tests::fu_to_world_is_the_nearest_f32`, onto the conversion
that actually ships.

Run: python3 tools/check_atlas.py      (exit 0 and an "ATLAS TABLES OK" line)
"""

import struct
import sys
from pathlib import Path

ATLAS = Path(__file__).resolve().parent.parent / "assets" / "atlas"

MAGIC_CP = 0x50433347  # 'G3CP'
MAGIC_GL = 0x4C473347  # 'G3GL'
MAGIC_CC = 0x43433347  # 'G3CC'
HEADER_WORDS = 17
FLAG_MISSING = 1


def fail(msg: str) -> None:
    print(f"FAIL  {msg}")
    raise SystemExit(1)


def read_words(name: str) -> list[int]:
    raw = (ATLAS / name).read_bytes()
    if len(raw) % 4:
        fail(f"{name}: not a u32 array")
    return list(struct.unpack(f"<{len(raw) // 4}I", raw))


def main() -> int:
    # ── the two tables, and their agreement ─────────────────────────────────
    cp = read_words("codepoints.bin")
    if cp[0] != MAGIC_CP:
        fail("codepoints.bin: bad magic")
    if cp[1] != 2:
        fail("codepoints.bin: version != 2 (regenerate: node tools/export-atlas.mjs)")
    (block_shift, block_index_len, block_count, entry_stride, _mapped_count,
     missing_advance_fu, missing_height_fu, cp_primary_upem) = cp[3:11]
    (_bitmap_advance_fu, seq_count, seq_max, seq_off, class_off, class_words) = cp[11:17]
    if block_shift != 8 or entry_stride != 4:
        fail(f"codepoints.bin: unexpected shape (shift {block_shift}, stride {entry_stride})")

    gl = read_words("glyphs.bin")
    if gl[0] != MAGIC_GL:
        fail("glyphs.bin: bad magic")
    gl_slot_count, gl_primary_upem, primary_advance_fu, em = gl[4], gl[5], gl[6], gl[7]
    if gl_primary_upem != cp_primary_upem:
        fail(f"upem mismatch: glyphs.bin {gl_primary_upem} vs codepoints.bin {cp_primary_upem}")
    if missing_advance_fu != primary_advance_fu or missing_height_fu != em:
        fail("codepoints.bin missing-block metrics disagree with glyphs.bin primary metrics")

    block_index = cp[HEADER_WORDS:HEADER_WORDS + block_index_len]
    blocks_start = HEADER_WORDS + block_index_len
    n_entries = block_count * (1 << block_shift) * entry_stride
    blocks = cp[blocks_start:blocks_start + n_entries]
    if len(blocks) != n_entries:
        fail(f"codepoints.bin: truncated blocks ({len(blocks)} words, want {n_entries})")

    # The v2 sections are appended after the blocks, never interleaved.
    if seq_off != blocks_start + n_entries:
        fail(f"codepoints.bin: sequence section at word {seq_off}, blocks end at {blocks_start + n_entries}")
    seq_stride = 2 + seq_max
    seq_section = cp[seq_off:seq_off + seq_count * seq_stride]
    class_section = cp[class_off:class_off + class_words]
    if len(seq_section) != seq_count * seq_stride or len(class_section) != class_words:
        fail("codepoints.bin: truncated v2 sections")
    if class_section[0] != MAGIC_CC:
        fail("codepoints.bin: class section is not a G3CC table")
    # Sequence slots are base + index (export-atlas step 4c), and the base is
    # glyphs.bin's slotCount minus the sequence count: checkable, so checked.
    seq_slot_base = gl_slot_count - seq_count
    if seq_section[0] != seq_slot_base or seq_section[(seq_count - 1) * seq_stride] != gl_slot_count - 1:
        fail(f"codepoints.bin: sequence slots [{seq_section[0]}..{seq_section[(seq_count - 1) * seq_stride]}] "
             f"disagree with glyphs.bin slotCount {gl_slot_count}")

    # ── pinned lookups, walked the way every loader walks the table ─────────
    def lookup(codepoint: int) -> tuple[int, int, int, int]:
        blk = block_index[codepoint >> block_shift]
        e = blocks_start + (((blk << block_shift) | (codepoint & 0xFF)) * 4)
        return cp[e], cp[e + 1], cp[e + 2], cp[e + 3]  # glyph id, advance fu, height fu, flags

    def expect(codepoint: int, gid: int, adv_fu: int, flags: int, label: str) -> None:
        got = lookup(codepoint)
        if got != (gid, adv_fu, em, flags):
            fail(f"{label}: got (gid, adv, height, flags) {got}, want {(gid, adv_fu, em, flags)}")
        print(f"  PASS  {label}: slot {gid}, advance {adv_fu} fu, flags {flags}")

    print("pinned codepoints:")
    expect(0x0041, 34, 1229, 0, "'A'         outline, one cell")
    expect(0x0020, 1, 1229, 0, "' '         empty slot, one cell")
    expect(0x1F400, 3839, 2458, 2, "'RAT'       bitmap, double advance")
    # The rocket was FORMAT.md's example of a MISSING codepoint until the
    # native emoji slots landed after the web's 4,431 (2026-09-10). Appended
    # slots are allocated in codepoint order, so 4759 moves only if the sheet
    # or the web prefix does — which is the point of pinning it.
    expect(0x1F680, 4759, 2458, 2, "'ROCKET'    bitmap, appended slot, double advance")
    expect(0xE0020, 0, 1229, FLAG_MISSING, "'TAG SPACE' missing: the shared missing block")

    # The family ZWJ sequence resolves to slot base + its sorted-table index;
    # 6819 is a pin like the rocket's.
    fam = (0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467)
    fam_slot = None
    for i in range(seq_count):
        o = i * seq_stride
        ln = seq_section[o + 1]
        if ln == len(fam) and tuple(seq_section[o + 2:o + 2 + ln]) == fam:
            fam_slot = seq_section[o]
            break
    if fam_slot != 6819:
        fail(f"sequence pin: the family resolves to slot {fam_slot}, want 6819")
    print(f"  PASS  'FAMILY'    ZWJ sequence -> slot {fam_slot}")

    # ── invariants over every entry ─────────────────────────────────────────
    # Height is the em height everywhere (one cell tall); block 0 is the
    # shared missing block, and an index slot pointing at it means missing.
    for e in range(0, len(blocks), 4):
        if blocks[e + 2] != em:
            fail(f"entry {e // 4}: height {blocks[e + 2]} fu, want the em height {em}")
    for i, b in enumerate(block_index):
        if b == 0 and i != 0 and not (lookup(i << 8)[3] & FLAG_MISSING):
            fail(f"block index {i} points at block 0 without FLAG_MISSING")
    print(f"invariants: {len(blocks) // 4} entries one em tall; block 0 is the missing block")

    print(f"ATLAS TABLES OK: {block_count} blocks, {seq_count} sequences, {gl_slot_count} slots")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
