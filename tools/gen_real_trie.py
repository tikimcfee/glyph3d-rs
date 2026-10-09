#!/usr/bin/env python3
"""gen_real_trie.py — bake the engine trie from the REAL atlas.

PORTED FROM tools/gen-real-trie.mjs (2026-09-02), which this replaces. The wire
format is UNCHANGED and byte-identical: the port drops the language, never the
artifact the language boundary forced into the open. `node` is no longer needed
to build any engine input.

Reads the Stage B export (assets/atlas/codepoints.bin + glyphs.bin) and emits
assets/atlas/engine-trie.bin — a 'G3TR' blob in the engine's trie container
layout, so `glyph_engine_load_trie_file` resolves every source byte to the REAL
FontChain global slot (the id the renderer's glyph-map texture is keyed by) with
REAL advances, instead of the conformance fixtures' toy `(cp % 4093) + 1` ids.

── The metric conversion (the whole point of the file) ─────────────────────

codepoints.bin carries measures as INTEGER primary-font units (lossless); the
engine trie, like the web's GlyphTrie, carries them as f32 WORLD units. The
conversion is FORMAT.md's "World-space conversion", anchored on the same
constant text.rs uses (CELL_HEIGHT_WORLD = 1.0):

    advance_world = f32(advance_fu * CELL_HEIGHT_WORLD / em_height_fu)
    height_world  = f32(height_fu  * CELL_HEIGHT_WORLD / em_height_fu)  # = 1.0

i.e. cell advance 1229/2320 ~ 0.52974 world, glyph quad height exactly 1.0.
Computed in f64 and rounded ONCE on the store. In JS that rounding point was
`Math.fround` / `DataView.setFloat32`; here it is `struct.pack('<f', v)`. Both
are IEEE-754 round-to-nearest-even, so the Rust side reproduces the bits with
`(advance_fu as f64 / em_height_fu as f64) as f32` exactly as before.

THIS IS A SECOND REALIZATION ON PURPOSE. `native/src/text.rs::fu_to_world` is
the other one: it converts from font units itself, while this file bakes the
world-unit values into the blob. No Rust code reads the blob today (every
layout backend resolves through codepoints.bin), so nothing currently diffs the
two — but any check that does is a check only while they stay separate. If
they ever become one shared function, the comparison stops being a check and
starts comparing a function to itself. DO NOT DEDUPLICATE ACROSS THAT SEAM.

Note (documented, not copied): the web's liveTrie.js uses
`(ax/upem) * worldScale * charSize.height` — upem (2048), not emHeight (2320),
in the denominator, which is ~13% wider than the geometric cell ratio (its own
header calls this deliberate). The native port anchors on the geometric ratio
(FORMAT.md), which is what the Stage C renderer stages.

── G3TR blob format (little-endian, packed u32 words) ──────────────────────

  word 0   magic 'G3TR' (0x52543347)
  word 1   version = 2
  word 2   headerBytes = 68
  word 3   blockShift = 8
  word 4   blockIndexLength (4352)
  word 5   blockCount (content-deduplicated blocks)
  word 6   entryStride = 4 (u32 lanes per entry)
  word 7   mappedCount (informational)
  word 8   primaryUpem (informational)
  word 9   primaryEmHeightFu (informational — the conversion denominator)
  word 10  cellHeightWorld as f32 BITS (a measure rides an f32 carrier)
  word 11  bitmapAdvance — the cluster head's advance, f32 BITS (v2)
  word 12  sequenceCount (v2)
  word 13  seqMax (v2)
  word 14  seqOff — word offset of the sequence section (v2)
  word 15  classOff — word offset of the class section (v2)
  word 16  classWords — the class section's size in words (v2)
  word 17+ blockIndex u32[blockIndexLength]
  then     blocks: blockCount x 256 entries x 4 words, lane order
           [GLYPH_ID u32 native][ADVANCE f32 bits][HEIGHT f32 bits][FLAGS u32 native]
  then     sequence section: sequenceCount x (2+seqMax) words,
           [slot, len, cp0..cp(len-1), 0-pad], sorted by sequence
  then     class section: assets/atlas/cluster-classes.bin VERBATIM (its own
           'G3CC' header included — byte-equality with that artifact is the
           provenance proof)

v1 blobs (44 B header, no sections) still LOAD — the loader refuses only ≥ 3.

The identity and the bitfield cross as NATIVE u32; the two measures as bitcast
f32 — exactly the web trie's container (GlyphTrie.js ENTRY_STRIDE), which a
loader splits by carrier (blocks_m f32 x2, blocks_c u32 x2 — the split
native/src/fixture.rs's FixtureTrie makes).

Run: python3 tools/gen_real_trie.py [--verify-only]
"""

import struct
import sys
from fractions import Fraction
from pathlib import Path

HERE = Path(__file__).resolve().parent
ATLAS = HERE.parent / "assets" / "atlas"

CELL_HEIGHT_WORLD = 1.0  # == text.rs CELL_HEIGHT_WORLD

MAGIC_CP = 0x50433347  # 'G3CP'
MAGIC_GL = 0x4C473347  # 'G3GL'
MAGIC_TR = 0x52543347  # 'G3TR'

HEADER_WORDS = 17


def fbits(v: float) -> int:
    """f64 -> the u32 BITS of its f32 narrowing. THE rounding point: exactly one
    narrowing per stored measure, matching Math.fround/setFloat32 in the JS this
    replaced and `as f32` on the Rust side."""
    return struct.unpack("<I", struct.pack("<f", v))[0]


def fval(u: int) -> float:
    """The inverse — u32 bits read back as f32 (what the engine's loader sees)."""
    return struct.unpack("<f", struct.pack("<I", u & 0xFFFFFFFF))[0]


def read_words(name: str) -> list[int]:
    raw = (ATLAS / name).read_bytes()
    if len(raw) % 4:
        raise SystemExit(f"{name}: not a u32 array")
    return list(struct.unpack(f"<{len(raw) // 4}I", raw))


def main() -> int:
    verify_only = "--verify-only" in sys.argv[1:]

    # ── load + cross-check the two sources ──────────────────────────────────
    cp = read_words("codepoints.bin")
    if cp[0] != MAGIC_CP:
        raise SystemExit("codepoints.bin: bad magic")
    if cp[1] != 2:
        raise SystemExit("codepoints.bin: version != 2 (regenerate: node tools/export-atlas.mjs)")
    (block_shift, block_index_len, block_count, entry_stride, mapped_count,
     missing_advance_fu, missing_height_fu, cp_primary_upem) = cp[3:11]
    # v2 (the sequence pass): the appended header words + the two sections.
    (bitmap_advance_fu, seq_count, seq_max, seq_off, class_off,
     class_words) = cp[11:17]
    if block_shift != 8 or entry_stride != 4:
        raise SystemExit(
            f"codepoints.bin: unexpected shape (shift {block_shift}, stride {entry_stride})")

    gl = read_words("glyphs.bin")
    if gl[0] != MAGIC_GL:
        raise SystemExit("glyphs.bin: bad magic")
    gl_primary_upem, primary_advance_fu, primary_em_height_fu = gl[5], gl[6], gl[7]
    if gl_primary_upem != cp_primary_upem:
        raise SystemExit(
            f"upem mismatch: glyphs.bin {gl_primary_upem} vs codepoints.bin {cp_primary_upem}")
    if missing_advance_fu != primary_advance_fu or missing_height_fu != primary_em_height_fu:
        raise SystemExit(
            "codepoints.bin missing-block metrics disagree with glyphs.bin primary metrics")

    block_index = cp[HEADER_WORDS:HEADER_WORDS + block_index_len]
    blocks_start = HEADER_WORDS + block_index_len
    n_entries = block_count * (1 << block_shift) * entry_stride
    blocks = cp[blocks_start:blocks_start + n_entries]
    if len(blocks) != n_entries:
        raise SystemExit(
            f"codepoints.bin: truncated blocks ({len(blocks)} words, want {n_entries})")

    # The v2 sections, carried VERBATIM (pure u32 data; the f32 conversion
    # below touches only the blocks). seq_off must land exactly after the
    # blocks — the sections are appended, not interleaved.
    if seq_off != blocks_start + n_entries:
        raise SystemExit(
            f"codepoints.bin: sequence section at word {seq_off}, blocks end at {blocks_start + n_entries}")
    seq_stride = 2 + seq_max
    seq_section = cp[seq_off:seq_off + seq_count * seq_stride]
    class_section = cp[class_off:class_off + class_words]
    if len(seq_section) != seq_count * seq_stride or len(class_section) != class_words:
        raise SystemExit("codepoints.bin: truncated v2 sections")
    if class_section[0] != 0x43433347:  # 'G3CC'
        raise SystemExit("codepoints.bin: class section is not a G3CC table")
    # The sequence section's slots are base+index by construction
    # (export-atlas step 4c); the base is glyphs.bin's slotCount minus the
    # sequence count — the two artifacts' agreement is checkable, so check it.
    seq_slot_base = gl[4] - seq_count
    if seq_section[0] != seq_slot_base or seq_section[(seq_count - 1) * seq_stride] != gl[4] - 1:
        raise SystemExit(
            f"codepoints.bin: sequence slots [{seq_section[0]}..{seq_section[(seq_count - 1) * seq_stride]}] "
            f"disagree with glyphs.bin slotCount {gl[4]}")

    # ── convert: integer font units -> f32 world units, rounded once per store ─
    em = primary_em_height_fu

    def to_world(fu: int) -> int:
        return fbits((fu * CELL_HEIGHT_WORLD) / em)

    out_blocks = [0] * len(blocks)
    for e in range(0, len(blocks), 4):
        out_blocks[e + 0] = blocks[e + 0]              # GLYPH_ID — identity, native u32
        out_blocks[e + 1] = to_world(blocks[e + 1])    # ADVANCE  — measure, bitcast f32
        out_blocks[e + 2] = to_world(blocks[e + 2])    # HEIGHT   — measure, bitcast f32
        out_blocks[e + 3] = blocks[e + 3]              # FLAGS    — bitfield, native u32

    # ── build the blob ──────────────────────────────────────────────────────
    # v2: the v1 header grows 44 -> 68 B (words 11..16 below) and the blob gains
    # the sequence + class sections after the blocks, byte-identical to the
    # codepoints.bin sections they were copied from. bitmap_advance is the ONE
    # converted word (the cluster head's advance: a measure, so it takes the
    # f32 carrier like every measure here).
    out = [
        MAGIC_TR, 2, HEADER_WORDS * 4, block_shift, block_index_len, block_count,
        entry_stride, mapped_count, gl_primary_upem, primary_em_height_fu,
        fbits(CELL_HEIGHT_WORLD),
        to_world(bitmap_advance_fu), seq_count, seq_max, seq_off, class_off,
        class_words,
    ] + list(block_index) + out_blocks + seq_section + class_section
    blob = struct.pack(f"<{len(out)}I", *out)

    out_path = ATLAS / "engine-trie.bin"

    # ── verification: walk the BUILT blob like the engine does ──────────────
    def lookup(codepoint: int) -> dict:
        block = out[HEADER_WORDS + (codepoint >> block_shift)]
        e = HEADER_WORDS + block_index_len + (((block << block_shift) | (codepoint & 0xFF)) * 4)
        return {"glyphId": out[e], "advance": fval(out[e + 1]),
                "height": fval(out[e + 2]), "flags": out[e + 3]}

    def expect(cp_want: int, gid: int, adv_fu: int, flags: int, label: str) -> None:
        t = lookup(cp_want)
        adv_want = fval(fbits(adv_fu * CELL_HEIGHT_WORLD / em))
        if (t["glyphId"] != gid or t["advance"] != adv_want
                or t["height"] != 1.0 or t["flags"] != flags):
            raise SystemExit(
                f"{label}: got {t}, want gid={gid} adv={adv_want} h=1 flags={flags}")
        print(f"  {label}: slot {t['glyphId']}, advance {t['advance']}, "
              f"height {t['height']}, flags {t['flags']}")

    print("[verify] worked-example codepoints through the built blob:")
    expect(0x0041, 34, 1229, 0, "'A'      (outline, one cell)")
    expect(0x0020, 1, 1229, 0, "' '      (empty slot, one cell)")
    expect(0x1F400, 3839, 2458, 2, "'RAT'    (bitmap, double advance)")
    # Appended by the emoji sheet (2026-09-10): the rocket was the FORMAT.md
    # example of a MISSING codepoint until the native slots landed after the
    # web's 4,431. Slot 4759 is a pin, like RAT's 3839: appended slots are
    # allocated in codepoint order, so this number moves only if the sheet or
    # the web prefix does — and that is the point of pinning it.
    expect(0x1F680, 4759, 2458, 2, "'ROCKET' (bitmap, appended slot, double advance)")
    expect(0xE0020, 0, 1229, 1, "'TAG SPACE' (missing — shared missing block; the font has no bitmap)")

    # The sequence section's worked example: the family ZWJ sequence resolves
    # to slot-base + its sorted-table index. 6819 is a pin like the rocket's
    # 4759 — it moves only if the sheet's sequence table does, which is the
    # point of pinning it.
    fam = (0x1F468, 0x200D, 0x1F469, 0x200D, 0x1F467)
    fam_slot = None
    for i in range(seq_count):
        o = i * seq_stride
        ln, slot = seq_section[o + 1], seq_section[o]
        if ln == len(fam) and tuple(seq_section[o + 2:o + 2 + ln]) == fam:
            fam_slot = slot
            break
    if fam_slot != 6819:
        raise SystemExit(f"sequence pin: the family resolves to slot {fam_slot}, want 6819")
    print(f"  'FAMILY'   (ZWJ sequence, v2 section) -> slot {fam_slot}")

    # invariants over every entry: height is the constant cell height; block 0
    # is the shared missing block; every unmapped index slot points at it.
    for e in range(0, len(out_blocks), 4):
        if fval(out_blocks[e + 2]) != 1.0:
            raise SystemExit(f"entry {e // 4}: height != 1.0")
    for i, b in enumerate(block_index):
        if b == 0 and i != 0 and lookup(i << 8)["flags"] != 1:
            raise SystemExit("a blockIndex slot points at block 0 without FLAG_MISSING")

    # ── exhaustive identity sweep: EVERY codepoint, source vs written blob ──
    # Ported from the JS oracle's trie tests (glyph-pipeline.test.mjs rows 1-2),
    # which had no counterpart on this side: no Mojo suite sweeps a codepoint
    # range, and the fixture tries only ever contain the codepoints their
    # fixture bytes happen to use. Two properties, and the second is the one
    # that bites silently:
    #
    #   EXHAUSTIVE — every codepoint resolves through the WRITTEN blob to the
    #                same GLYPH_ID and FLAGS the source table gives it.
    #   NO ALIASING — no codepoint the source leaves unmapped comes back mapped.
    #                 A dedup or block-index error does not throw; it hands out
    #                 a PLAUSIBLE glyph for the wrong character, which is
    #                 invisible in any rendering and in any bit-compare that
    #                 only covers the codepoints a fixture happens to contain.
    #
    # Walked the way the engine walks it (two dependent loads through the blob
    # we just built), compared against the source table read independently.
    def src_entry(codepoint: int) -> tuple[int, int]:
        blk = block_index[codepoint >> block_shift]
        e = blocks_start + (((blk << block_shift) | (codepoint & 0xFF)) * 4)
        return cp[e + 0], cp[e + 3]           # GLYPH_ID, FLAGS from codepoints.bin

    swept_cp = 0
    for codepoint in range(0x110000):
        got = lookup(codepoint)
        want_gid, want_flags = src_entry(codepoint)
        if got["glyphId"] != want_gid or got["flags"] != want_flags:
            raise SystemExit(
                f"identity sweep: U+{codepoint:04X} resolves to gid={got['glyphId']} "
                f"flags={got['flags']} but the source table says gid={want_gid} "
                f"flags={want_flags}")
        swept_cp += 1
    # NO SEPARATE ALIASING COUNTER, deliberately. The JS original checks
    # no-aliasing against an INDEPENDENT source map, where it is a real second
    # property. Here the source of truth IS this table, so the identity
    # comparison above subsumes it: if the source says missing (gid 0) and
    # identity holds, the blob returned 0 too. I wrote the counter anyway, then
    # disabled it and watched the sweep stay green — it could not fire. Removed
    # rather than left standing, because a second assertion that cannot add
    # anything reads like twice the coverage.
    if swept_cp != 0x110000:
        raise SystemExit(f"identity sweep is vacuous — only {swept_cp} codepoints walked")
    mapped_now = sum(1 for c in range(0x110000) if not (src_entry(c)[1] & 1))
    print(f"[verify] identity sweep: all {swept_cp} codepoints agree with the source "
          f"table ({mapped_now} mapped)")

    # ── domain sweep: the conversion over its WHOLE input range ─────────────
    # The atlas cannot certify this function. Measured 2026-09-02: across all
    # 7168 entries the trie carries THREE distinct font-unit values (advance
    # 1229/2458, height 2320) — it is a monospace atlas, so "exhaustive over the
    # corpus" is exhaustive over three numbers, one of which is the identity
    # case. Sweeping the representable range is what pins the rounding
    # discipline, and it costs milliseconds.
    #
    # The oracle is EXACT RATIONAL ARITHMETIC, not the same expression a second
    # time: Fraction(fu, em) is the true quotient with no rounding at all, and
    # the assertion is that what we stored is the NEAREST f32 to it — no
    # neighbouring float is closer. That is independent of struct/IEEE
    # semantics, so it catches a wrong denominator, a double rounding, or a
    # truncation. (The first draft of this loop compared to_world against an
    # inlined copy of itself and could not fail; kept as a comment because that
    # is the single easiest verification mistake to make and it was made here.)
    swept = 0
    for fu in range(0, 65536):
        exact = Fraction(fu) * Fraction(CELL_HEIGHT_WORLD) / Fraction(em)
        bits = to_world(fu)
        got = fval(bits)
        err = abs(Fraction(got) - exact)
        for nb in (bits - 1, bits + 1):
            if 0 <= nb <= 0x7F7FFFFF and abs(Fraction(fval(nb)) - exact) < err:
                raise SystemExit(
                    f"domain sweep: fu={fu} stored {got!r} but {fval(nb)!r} is nearer "
                    f"the exact quotient {float(exact)!r} — rounding discipline broken")
        swept += 1
    distinct = len({blocks[e + 1] for e in range(0, len(blocks), 4)} |
                   {blocks[e + 2] for e in range(0, len(blocks), 4)})
    print(f"[verify] domain sweep: {swept} font-unit values, 0 disagreements "
          f"(the atlas itself only exercises {distinct} distinct values)")

    # ── write (or prove byte-identity without writing) ──────────────────────
    if verify_only:
        existing = out_path.read_bytes() if out_path.exists() else b""
        same = existing == blob
        print(f"[verify-only] {out_path.name}: "
              f"{'BYTE-IDENTICAL to the committed blob' if same else 'DIFFERS'} "
              f"({len(blob)} bytes built, {len(existing)} on disk)")
        return 0 if same else 1

    out_path.write_bytes(blob)
    print(f"[done] {out_path}")
    print(f"[done] {mapped_count} mapped codepoints, {block_count} unique blocks, "
          f"{len(blob)} bytes ({len(blob) / 1024:.1f} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
