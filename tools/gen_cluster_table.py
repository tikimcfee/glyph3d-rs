#!/usr/bin/env python3
"""gen_cluster_table.py — bake the grapheme-cluster class table.

The class data for cluster-mode segmentation, GENERATED FROM THE VENDORED UCD
(tools/vendor/third-party/unicode-ucd/, pinned in tools/vendor-manifest.py) —
never hand-written. A hand-rolled state machine broke keycaps in the 2026-09-20
experiment and was caught only because the test expectation was wrong in the
other direction.

WHO READS THIS TABLE: the landed sequence pass deliberately reads NO classes
(engine/glyph_cluster.mojo says why — matching is table-driven, not
class-driven). The table covers the FULL Grapheme_Cluster_Break space plus the
emoji-data properties because the general UAX #29 phase (combining marks,
Hangul, Indic) rides it next — it is baked and carried verbatim into both
tries now so that phase finds its data already placed; until then the tests
are its only readers.

Format (little-endian u32 words):

  word 0   magic 'G3CC' (0x43433347)
  word 1   version = 1
  word 2   headerBytes = 40
  word 3   rangeCount
  word 4   unicodeMajor (17)
  word 5   unicodeMinor (0)
  word 6   classBitCount (15)
  word 7..9 reserved (0)
  then     rangeCount x 3 words: [start, endInclusive, classBits], sorted by
           start, non-overlapping, coalesced (no adjacent equal-bit ranges).
           A codepoint in no range is class 0 ("Other").

Class bits (GraphemeBreakProperty.txt, then emoji-data.txt):
  0 EXTEND  1 ZWJ  2 REGIONAL_INDICATOR  3 CONTROL (Control|CR|LF)
  4 PREPEND  5 SPACINGMARK
  6 HANGUL_L  7 HANGUL_V  8 HANGUL_T  9 HANGUL_LV  10 HANGUL_LVT
  11 EXTENDED_PICTOGRAPHIC  12 EMOJI_MODIFIER  13 EMOJI_COMPONENT
  14 EMOJI_MODIFIER_BASE

Run:  python3 tools/gen_cluster_table.py               bake the artifact
      python3 tools/gen_cluster_table.py --check       rebuild in memory, byte-compare,
                                                       re-assert structure with a fresh reader
"""

import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
UCD = HERE / "vendor" / "third-party" / "unicode-ucd"
OUT = HERE.parent / "assets" / "atlas" / "cluster-classes.bin"

MAGIC = 0x43433347  # 'G3CC'
VERSION = 1
HEADER_WORDS = 10
UCD_MAJOR, UCD_MINOR = 17, 0

BIT_EXTEND = 0
BIT_ZWJ = 1
BIT_REGIONAL_INDICATOR = 2
BIT_CONTROL = 3
BIT_PREPEND = 4
BIT_SPACINGMARK = 5
BIT_HANGUL_L = 6
BIT_HANGUL_V = 7
BIT_HANGUL_T = 8
BIT_HANGUL_LV = 9
BIT_HANGUL_LVT = 10
BIT_EXTENDED_PICTOGRAPHIC = 11
BIT_EMOJI_MODIFIER = 12
BIT_EMOJI_COMPONENT = 13
BIT_EMOJI_MODIFIER_BASE = 14
CLASS_BIT_COUNT = 15

GCB_BITS = {
    "Extend": BIT_EXTEND,
    "ZWJ": BIT_ZWJ,
    "Regional_Indicator": BIT_REGIONAL_INDICATOR,
    "Prepend": BIT_PREPEND,
    "SpacingMark": BIT_SPACINGMARK,
    "L": BIT_HANGUL_L,
    "V": BIT_HANGUL_V,
    "T": BIT_HANGUL_T,
    "LV": BIT_HANGUL_LV,
    "LVT": BIT_HANGUL_LVT,
}
# CR and LF fold into CONTROL: the segmentation rule only ever asks "hard
# break here?", never which control it is.
GCB_CONTROL = {"Control", "CR", "LF"}

EMOJI_BITS = {
    "Extended_Pictographic": BIT_EXTENDED_PICTOGRAPHIC,
    "Emoji_Modifier": BIT_EMOJI_MODIFIER,
    "Emoji_Component": BIT_EMOJI_COMPONENT,
    "Emoji_Modifier_Base": BIT_EMOJI_MODIFIER_BASE,
}
# emoji-data also carries Emoji and Emoji_Presentation, which the rule does
# not read; they are deliberately not bits (a bit nobody reads is a claim
# nobody checks). They are NAMED here so the parser's loud refusal on an
# unknown property stays live for genuinely new ones.
EMOJI_IGNORED = {"Emoji", "Emoji_Presentation"}

MAX_CP = 0x10FFFF


def parse_ucd(path: Path, field_bits: dict, control_names: set) -> list:
    """Parse 'start..end ; Property' lines into (start, end, bit) ranges.
    Refuses an unknown property or a malformed line: the UCD is the contract,
    and a generator that guesses is how the keycap bug happened."""
    ranges = []
    for lineno, raw in enumerate(path.read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        span, sep, prop = line.partition(";")
        if not sep:
            raise SystemExit(f"ERROR  {path.name}:{lineno}: no ';' in {raw!r}")
        prop = prop.strip()
        lo_s, dots, hi_s = span.strip().partition("..")
        try:
            lo = int(lo_s, 16)
            hi = int(hi_s, 16) if dots else lo
        except ValueError:
            raise SystemExit(f"ERROR  {path.name}:{lineno}: bad span in {raw!r}")
        if prop in control_names:
            bit = BIT_CONTROL
        elif prop in field_bits:
            bit = field_bits[prop]
        elif prop in EMOJI_IGNORED:
            continue
        else:
            raise SystemExit(
                f"ERROR  {path.name}:{lineno}: unknown property {prop!r} — "
                "a UCD revision added or renamed one; say where it goes"
            )
        ranges.append((lo, hi, bit))
    return ranges


def build() -> bytes:
    gcb_path = UCD / "GraphemeBreakProperty.txt"
    emoji_path = UCD / "emoji-data.txt"
    for p in (gcb_path, emoji_path):
        if not p.exists():
            raise SystemExit(f"ERROR  {p} is missing — the vendored UCD is the input")
    # Version pins: the header comments name the revision; a silent re-vendor
    # is exactly what the vendor-hashes gate cannot see if the manifest moved
    # with it, so the generator re-asserts it here.
    gcb_head = gcb_path.read_text().splitlines()[0]
    emoji_head = emoji_path.read_text().splitlines()[:8]
    if f"GraphemeBreakProperty-{UCD_MAJOR}.{UCD_MINOR}" not in gcb_head:
        raise SystemExit(f"ERROR  GraphemeBreakProperty header is {gcb_head!r}, expected {UCD_MAJOR}.{UCD_MINOR}")
    if not any(f"Version: {UCD_MAJOR}.{UCD_MINOR}" in h for h in emoji_head):
        raise SystemExit(f"ERROR  emoji-data header lacks Version: {UCD_MAJOR}.{UCD_MINOR}")

    bits = [0] * (MAX_CP + 1)
    for lo, hi, bit in parse_ucd(gcb_path, GCB_BITS, GCB_CONTROL):
        for cp in range(lo, hi + 1):
            bits[cp] |= 1 << bit
    for lo, hi, bit in parse_ucd(emoji_path, EMOJI_BITS, set()):
        for cp in range(lo, hi + 1):
            bits[cp] |= 1 << bit

    # ── pins: named answers the table must give ─────────────────────────
    # The generator's self-test (the gen_real_trie.py rocket precedent): if
    # any of these moves, the data moved, and refusing to write is correct.
    def has(cp, mask):
        return bits[cp] & mask == mask

    pins = [
        (0x200D, 1 << BIT_ZWJ, "ZWJ"),
        (0x1F1E6, 1 << BIT_REGIONAL_INDICATOR, "regional indicator A"),
        (0x1F600, 1 << BIT_EXTENDED_PICTOGRAPHIC, "grinning face"),
        (0xFE0F, 1 << BIT_EXTEND, "VS16"),
        (0xFE0F, 1 << BIT_EMOJI_COMPONENT, "VS16 as emoji component"),
        (0x1F3FB, (1 << BIT_EXTEND) | (1 << BIT_EMOJI_MODIFIER), "light skin tone"),
        (0x20E3, 1 << BIT_EXTEND, "combining enclosing keycap"),
        (0x20E3, 1 << BIT_EMOJI_COMPONENT, "keycap as emoji component"),
        (0xE0020, 1 << BIT_EXTEND, "tag space"),
        (0xE0020, 1 << BIT_EMOJI_COMPONENT, "tag space as emoji component"),
        (0x0A, 1 << BIT_CONTROL, "LF"),
        (0x0D, 1 << BIT_CONTROL, "CR"),
        (0x1100, 1 << BIT_HANGUL_L, "Hangul choseong kiyeok"),
        (0xAC00, 1 << BIT_HANGUL_LV, "Hangul syllable GA"),
        (0x1F44D, 1 << BIT_EXTENDED_PICTOGRAPHIC, "thumbs up"),
        (0x1F44D, 1 << BIT_EMOJI_MODIFIER_BASE, "thumbs up as modifier base"),
    ]
    for cp, mask, name in pins:
        if not has(cp, mask):
            raise SystemExit(
                f"ERROR  pin failed: U+{cp:04X} ({name}) lacks bits {mask:#x} "
                f"(has {bits[cp]:#x}) — the vendored UCD is not the one this table was written against"
            )
    if bits[0x41] != 0:
        raise SystemExit("ERROR  pin failed: 'A' U+0041 carries class bits (must be Other)")

    # Coalesce: sorted, non-overlapping, no adjacent equal-bit ranges. Zero-bit
    # codepoints are simply absent (class 0 = Other by the format's rule).
    ranges = []
    cp = 0
    while cp <= MAX_CP:
        if bits[cp] == 0:
            cp += 1
            continue
        b = bits[cp]
        start = cp
        while cp + 1 <= MAX_CP and bits[cp + 1] == b:
            cp += 1
        ranges.append((start, cp, b))
        cp += 1

    header = struct.pack(
        "<10I", MAGIC, VERSION, HEADER_WORDS * 4, len(ranges),
        UCD_MAJOR, UCD_MINOR, CLASS_BIT_COUNT, 0, 0, 0,
    )
    body = b"".join(struct.pack("<3I", s, e, b) for s, e, b in ranges)
    return header + body


def check_structure(data: bytes):
    """A fresh reader over the BYTES — sharing no variables with the writer
    (the gen_emoji_sheet.py check-mode pattern): the writer's own structures
    agreeing with the writer is not a check."""
    if len(data) < HEADER_WORDS * 4:
        raise SystemExit("ERROR  cluster-classes.bin: shorter than its header")
    magic, version, header_bytes, n, maj, minor, nbits, *_ = struct.unpack_from("<10I", data, 0)
    if magic != MAGIC:
        raise SystemExit(f"ERROR  bad magic {magic:#x}")
    if version != VERSION:
        raise SystemExit(f"ERROR  version {version}, expected {VERSION}")
    if (maj, minor) != (UCD_MAJOR, UCD_MINOR):
        raise SystemExit(f"ERROR  UCD {maj}.{minor}, expected {UCD_MAJOR}.{UCD_MINOR}")
    if nbits != CLASS_BIT_COUNT:
        raise SystemExit(f"ERROR  {nbits} class bits, expected {CLASS_BIT_COUNT}")
    if len(data) != header_bytes + n * 12:
        raise SystemExit("ERROR  byte size disagrees with rangeCount")
    prev_end, prev_bits = -1, -1
    for i in range(n):
        s, e, b = struct.unpack_from("<3I", data, header_bytes + i * 12)
        if not (0 <= s <= e <= MAX_CP):
            raise SystemExit(f"ERROR  range {i} is malformed: ({s:#x}, {e:#x})")
        if s <= prev_end:
            raise SystemExit(f"ERROR  range {i} overlaps or is unsorted")
        if s == prev_end + 1 and b == prev_bits:
            # Only CONTIGUOUS equal-bit ranges are uncoalesced; equal bits
            # across a zero-bit gap are separate ranges by definition.
            raise SystemExit(f"ERROR  range {i} should have coalesced into its neighbour")
        if b == 0 or b >> CLASS_BIT_COUNT:
            raise SystemExit(f"ERROR  range {i} carries zero or unknown bits {b:#x}")
        prev_end, prev_bits = e, b


def main() -> int:
    data = build()
    check_structure(data)
    print(f"[gen] {len(data)} B, {(len(data) - HEADER_WORDS * 4) // 12} ranges, "
          f"UCD {UCD_MAJOR}.{UCD_MINOR}, {CLASS_BIT_COUNT} class bits")
    if "--check" in sys.argv[1:]:
        if not OUT.exists():
            print(f"FAIL  {OUT} does not exist")
            return 1
        committed = OUT.read_bytes()
        if committed != data:
            print(f"FAIL  {OUT.name} differs from a fresh bake — regenerate it "
                  "(python3 tools/gen_cluster_table.py)")
            return 1
        check_structure(committed)
        print(f"[check] {OUT.name} byte-identical to a fresh bake")
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(data)
    print(f"[done] wrote {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
