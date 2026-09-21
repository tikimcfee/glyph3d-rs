#!/usr/bin/env python3
"""gen_emoji_corpus.py — the cluster demo corpus, generated from the shipped table.

Two fixtures for pointing the demo at the sequence pass (engine/delta/cluster-mode.md):

  native/fixtures/emoji-corpus-small.txt — the quick check: one line per
      sequence class plus the edge cases (RI triplet, over-long chain,
      VS15-broken keycap, unlisted chains), each asserted against the table.
  native/fixtures/emoji-corpus-large.txt — the pattern-match field: EVERY
      sequence in the table exactly once (asserted), grouped so the eye can
      compare — all the flags, every base x its five skin tones, every ZWJ
      role beside its tone variants, families, couples, keycaps, tag flags.

GENERATED FROM assets/atlas/codepoints.bin's v2 sequence section — the same
bytes the trie resolves against — never hand-written. A sequence on a
"resolves" line is in the table by construction; a "stays pieces" line is
asserted NOT to resolve at bake time (a font/table change that moves one of
those answers refuses the bake, it does not silently change the demo).
Regenerate by hand after a font/table change, like any committed artifact.

Run:  python3 tools/gen_emoji_corpus.py                 bake both fixtures
      python3 tools/gen_emoji_corpus.py --check         rebuild in memory, byte-compare
      python3 tools/gen_emoji_corpus.py --check-render  render both fixtures in both
                                                        modes and diff the binary's staged
                                                        counts against this file's simulator
                                                        (calibrated on emoji-cluster-view.txt)
"""

from __future__ import annotations

import re
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CODEPOINTS = ROOT / "assets" / "atlas" / "codepoints.bin"
SMALL = ROOT / "native" / "fixtures" / "emoji-corpus-small.txt"
LARGE = ROOT / "native" / "fixtures/emoji-corpus-large.txt"
CALIBRATION = ROOT / "native" / "fixtures" / "emoji-cluster-view.txt"
BINARY = ROOT / "target" / "release" / "glyph3d-native"

MAGIC_G3CP = 0x50433347
FLAG_MISSING = 1

ZWJ = 0x200D
VS15 = 0xFE0E
VS16 = 0xFE0F
KEYCAP = 0x20E3
RI_LO, RI_HI = 0x1F1E6, 0x1F1FF
TONE_LO, TONE_HI = 0x1F3FB, 0x1F3FF
TAG_LO, TAG_HI = 0xE0020, 0xE007F
PERSONS = {0x1F468, 0x1F469, 0x1F9D1, 0x1F466, 0x1F467, 0x1F476}  # 👨 👩 🧑 👦 👧 👶
HEARTS = {0x2764, 0x1F48B}  # ❤ 💋

# The known answers the calibration fixture carries, as printed by the binary
# (leader, then cluster): codepoints / cells / missing. A renderer change that
# moves these is a finding, not a regeneration.
CALIBRATION_COUNTS = {"leader": (406, 400, 6), "cluster": (403, 375, 0)}


def is_static_zero(cp: int) -> bool:
    """fold.rs::is_static_zero_cp, verbatim ranges."""
    return cp == ZWJ or 0xFE00 <= cp <= VS16 or TAG_LO <= cp <= TAG_HI


def is_ri(cp: int) -> bool:
    return RI_LO <= cp <= RI_HI


def is_tone(cp: int) -> bool:
    return TONE_LO <= cp <= TONE_HI


def is_tag(cp: int) -> bool:
    return TAG_LO <= cp <= TAG_HI


class Trie:
    """The codepoints.bin v2 reader: the lookup the corpus's simulator mirrors,
    and the sequence section the corpus is generated from. FORMAT.md is the
    contract; this shares no bytes with the writer (export-atlas.mjs)."""

    def __init__(self, data: bytes):
        if len(data) % 4:
            raise SystemExit("ERROR  codepoints.bin: not a u32 word stream")
        self.words = struct.unpack(f"<{len(data) // 4}I", data)
        w = self.words
        if w[0] != MAGIC_G3CP:
            raise SystemExit(f"ERROR  bad magic {w[0]:#x}, expected G3CP")
        if w[1] != 2 or w[2] != 68:
            raise SystemExit(f"ERROR  version {w[1]} headerBytes {w[2]} — the v2 sections are a v2 header")
        self.block_shift = w[3]
        self.block_index_len = w[4]
        self.entry_stride = w[6]
        self.seq_count, self.seq_max, self.seq_off = w[12], w[13], w[14]
        if self.block_shift != 8 or self.entry_stride != 4:
            raise SystemExit("ERROR  block geometry moved; FORMAT.md says 8/4")

    def lookup(self, cp: int) -> tuple[int, int, int, int]:
        """(slot, advance_fu, height_fu, flags) — the two dependent loads."""
        w = self.words
        block = w[17 + (cp >> self.block_shift)] if cp <= 0x10FFFF else 0
        e = 17 + self.block_index_len + ((block << self.block_shift) | (cp & 0xFF)) * self.entry_stride
        return w[e], w[e + 1], w[e + 2], w[e + 3]

    def sequences(self) -> list[tuple[int, tuple[int, ...]]]:
        w, stride = self.words, 2 + self.seq_max
        out = []
        for i in range(self.seq_count):
            o = self.seq_off + i * stride
            ln = w[o + 1]
            if not (2 <= ln <= self.seq_max):
                raise SystemExit(f"ERROR  sequence {i}: len {ln} outside 2..{self.seq_max}")
            out.append((w[o], tuple(w[o + 2 : o + 2 + ln])))
        keys = [c for _, c in out]
        if len(set(keys)) != len(keys):
            raise SystemExit("ERROR  duplicate sequence keys — the fold's last-writer-wins tie is not a corpus")
        return out


def probe(cps: list[int], i: int, entries: dict, seq_first: set, seq_max: int):
    """text.rs's cluster path, verbatim: longest table prefix of the
    FE0F-normalized key; VS15/newline break the candidate; the span counts
    key-consumers so skipped VS16s ride it. Returns (slot, covered) or None."""
    cp = cps[i]
    if cp not in seq_first:
        return None
    key, span = [cp], 1
    while i + span < len(cps) and len(key) < seq_max:
        c2 = cps[i + span]
        if c2 == 0x0A or c2 == VS15:
            break
        if c2 != VS16:
            key.append(c2)
        span += 1
    for ln in range(min(len(key), seq_max), 1, -1):
        slot = entries.get(tuple(key[:ln]))
        if slot is not None:
            covered, need = 0, ln
            while need > 0:
                if cps[i + covered] != VS16:
                    need -= 1
                covered += 1
            return slot, covered
    return None


def simulate(text: str, mode: str, trie: Trie, entries: dict, seq_first: set):
    """stage_file's three counters (codepoints, cells, missing), both modes.
    Calibrated against emoji-cluster-view.txt's printed counts — see
    --check-render."""
    cps = [ord(c) for c in text]
    codepoints = cells = missing = 0
    i = 0
    while i < len(cps):
        cp = cps[i]
        if mode == "cluster":
            if is_static_zero(cp):
                i += 1
                continue
            hit = probe(cps, i, entries, seq_first, trie.seq_max)
            if hit:
                _, covered = hit
                codepoints += covered
                cells += 1
                i += covered
                continue
        if cp in (0x0A, 0x09):  # LF / TAB: advance, never a cell
            i += 1
            continue
        codepoints += 1
        slot, _, _, flags = trie.lookup(cp)
        if flags & FLAG_MISSING:
            missing += 1
        elif slot != 0:
            cells += 1
        i += 1
    return codepoints, cells, missing


def s(cps) -> str:
    return "".join(chr(cp) for cp in cps)


def rows(items, per_row: int) -> list[str]:
    return [" ".join(s(c) for c in items[i : i + per_row])
            for i in range(0, len(items), per_row)]


def classify(seqs):
    """The disjoint classes (the partition is asserted exact at bake).
    Dead-headed entries — a static-zero first member — are returned separately:
    the main loop skips such a codepoint before any probe, so they can never
    resolve (the font's GSUB carries one such rule: U+E007F U+1F3F4)."""
    live = [c for _, c in seqs if not is_static_zero(c[0])]
    dead = sorted(c for _, c in seqs if is_static_zero(c[0]))
    keycaps = sorted(c for c in live if KEYCAP in c)
    tags = sorted(c for c in live if KEYCAP not in c and any(is_tag(x) for x in c))
    flags = sorted(c for c in live if len(c) == 2 and all(is_ri(x) for x in c))
    tones = sorted(c for c in live if any(is_tone(x) for x in c) and ZWJ not in c)
    zwj_tone = sorted(c for c in live if ZWJ in c and any(is_tone(x) for x in c))
    zwj = sorted(c for c in live if ZWJ in c
                 and not any(is_tone(x) for x in c) and KEYCAP not in c and not any(is_tag(x) for x in c))
    total = sum(map(len, (keycaps, tags, flags, tones, zwj_tone, zwj, dead)))
    if total != len(seqs):
        raise SystemExit(f"ERROR  class partition {total} != {len(seqs)} sequences — a class overlap")
    return keycaps, tags, flags, tones, zwj_tone, zwj, dead


def group_by_tone(seqs) -> dict[tuple[int, ...], list[tuple[int, ...]]]:
    groups: dict[tuple[int, ...], list[tuple[int, ...]]] = {}
    for c in seqs:
        groups.setdefault(tuple(x for x in c if not is_tone(x)), []).append(c)
    return groups


def unlisted_chains(entries, seq_first, seq_max) -> list[tuple[int, int, int]]:
    """X ZWJ Y pairs the table provably does not resolve: X starts no sequence."""
    out = []
    for cpa, cpb in [(0x1F680, 0x1F30D), (0x1F422, 0x1F40D), (0x1F388, 0x1F335),
                     (0x1F3D6, 0x1F32E), (0x1F98A, 0x1F30E), (0x1F427, 0x1F32D)]:
        chain = (cpa, ZWJ, cpb)
        if cpa not in seq_first and probe(list(chain), 0, entries, seq_first, seq_max) is None:
            out.append(chain)
    if len(out) < 2:
        raise SystemExit("ERROR  unlisted-chain candidates exhausted — the table grew them all")
    return out


def not_a_flag(entries) -> tuple[int, int]:
    pair = next((a, b) for a in range(RI_LO, RI_HI + 1) for b in range(RI_LO, RI_HI + 1)
                if (a, b) not in entries)
    return pair


def build_small(trie, entries, seq_first, keycaps, tags, flags, tones, zwj_tone, zwj, dead) -> str:
    """The quick check. Every claim a line makes is asserted against the table
    right here — a line that stopped being true stops the bake. The picks are
    curated for recognizability (🇨🇦 over 🇦🇨); the assertions are what makes
    curation safe."""

    def resolves(text: str):
        return probe([ord(c) for c in text], 0, entries, seq_first, trie.seq_max)

    def pick(what: str, *cands: tuple[int, ...]) -> tuple[int, ...]:
        for c in cands:
            if c in entries:
                return c
        raise SystemExit(f"ERROR  {what}: none of the curated picks is in the table")

    def toned(c: tuple[int, ...]) -> list[tuple[int, ...]]:
        """The plain chain + its five tone variants (the tone rides the first
        member), all required in the table."""
        row = [c] + [(c[0], t, *c[1:]) for t in range(TONE_LO, TONE_HI + 1)]
        missing = [r for r in row if r not in entries]
        if missing:
            raise SystemExit(f"ERROR  {s(c)!r}: tone variants missing from the table: "
                             + ", ".join(" ".join(f"U+{x:04X}" for x in m) for m in missing))
        return row

    fams = [
        pick("family man-woman-girl", (0x1F468, ZWJ, 0x1F469, ZWJ, 0x1F467)),
        pick("family man-woman-girl-boy", (0x1F468, ZWJ, 0x1F469, ZWJ, 0x1F467, ZWJ, 0x1F466)),
        pick("family man-man-boy", (0x1F468, ZWJ, 0x1F468, ZWJ, 0x1F466)),
        pick("family woman-woman-girl", (0x1F469, ZWJ, 0x1F469, ZWJ, 0x1F467)),
    ]
    fa = pick("flag CA", (0x1F1E8, 0x1F1E6))
    fb = pick("flag JP", (0x1F1EF, 0x1F1F5))
    fc = pick("flag US", (0x1F1FA, 0x1F1F8))
    heart = pick("heart on fire", (0x2764, ZWJ, 0x1F525))
    couple = pick("couple with heart", (0x1F469, ZWJ, 0x2764, ZWJ, 0x1F468),
                  (0x1F468, ZWJ, 0x2764, ZWJ, 0x1F469))
    tone_row = toned(pick("a toned role", (0x1F9D1, ZWJ, 0x1F4BB), (0x1F468, ZWJ, 0x1F680),
                          (0x1F469, ZWJ, 0x1F373)))

    skin_row = None
    for base in (0x1F44D, 0x1F44B, 0x1F44F, 0x1F64F, 0x261D):  # 👍 👋 👏 🙏 ☝
        variants = [(base, t) for t in range(TONE_LO, TONE_HI + 1)]
        if all(v in entries for v in variants):
            slot, _, _, fl = trie.lookup(base)
            if slot != 0 and not fl & FLAG_MISSING:
                skin_row = (base, variants)
                break
    if skin_row is None:
        raise SystemExit("ERROR  no curated modifier base with a bitmap and all five tones")
    base_cp, skin_variants = skin_row
    base_char, tone_variants = chr(base_cp), " ".join(s(v) for v in skin_variants)

    nf = not_a_flag(entries)
    if resolves(s(nf)) is not None:
        raise SystemExit("ERROR  the not-a-flag pair now resolves")
    triplet = fc + (fc[1],)
    if resolves(s(triplet)) != (entries[fc], 2):
        raise SystemExit("ERROR  triplet no longer resolves as flag + lone RI")

    overlong = fams[1] + (ZWJ, 0x1F467)  # a full family + ZWJ + girl: the prefix wins
    hit = resolves(s(overlong))
    if hit is None or hit[1] >= len(overlong):
        raise SystemExit("ERROR  over-long chain: expected a strict-prefix win")
    if hit[0] != entries[fams[1]]:
        raise SystemExit("ERROR  over-long chain: the winning prefix is not the full family")

    bare_keycap = next(c for c in keycaps if c[0] == 0x31)
    if resolves(s(bare_keycap)) is None:
        raise SystemExit("ERROR  bare keycap (no VS16) stopped resolving — the table normalizes VS16 out")
    vs15_keycap = "1" + chr(VS15) + chr(KEYCAP)
    if resolves(vs15_keycap) is not None:
        raise SystemExit("ERROR  VS15 no longer breaks the keycap probe")

    heart_vs16 = (heart[0], VS16, *heart[1:])
    if resolves(s(heart)) is None or resolves(s(heart_vs16)) is None:
        raise SystemExit("ERROR  heart chain fails with or without its VS16")
    if resolves(s(heart))[0] != resolves(s(heart_vs16))[0]:
        raise SystemExit("ERROR  VS16 on/off resolve to different slots — normalization moved")

    unlisted = unlisted_chains(entries, seq_first, trie.seq_max)
    lone_zwj = "a" + chr(ZWJ) + "b " + "a" + chr(VS16) + "b"

    lines = [
        "# emoji corpus — SMALL — the quick cluster check (generated: tools/gen_emoji_corpus.py).",
        "# cluster mode: every sequence on a resolve line is ONE glyph (a double-width cell).",
        "# leader mode: the same lines show the pieces. The field: emoji-corpus-large.txt.",
        "",
        "family: " + " ".join(s(c) for c in fams),
        f"flag: {s(fa)} {s(fb)} {s(fc)}  not-a-flag-pair: {s(nf)}  triplet (flag + lone RI): {s(triplet)}",
        f"skin tone: {base_char} then its five tones: {tone_variants}  lone modifiers (no base — swatches): "
        + "".join(chr(c) for c in range(TONE_LO, TONE_HI + 1)),
        "role + tones: " + " ".join(s(c) for c in tone_row),
        f"keycap: {' '.join(s(c) for c in keycaps[:4])}  bare (no VS16, same slot): {s(bare_keycap)}"
        f"  VS15-broken: {vs15_keycap}",
        "tag flags: " + " ".join(s(c) for c in tags),
        f"couple: {s(couple)}  heart chain: {s(heart)}",
        f"VS16 on/off, one slot: {s(heart)} {s(heart_vs16)}",
        f"over-long chain (the full family wins, then a lone girl): {s(overlong)}",
        "unlisted chains, stay pieces: " + " ".join(s(c) for c in unlisted),
        f"lone ZWJ / VS16, zero-width: {lone_zwj}",
        "adjacent clusters, no spaces: " + s(fa) + s(fb) + s(fams[1]) + s(skin_variants[2]) + s(bare_keycap),
        f'code beside clusters: fn main() {{ let f = "{s(fams[0])}"; }} // {s(fa)} {s(tone_row[0])}',
        "",
    ]
    return "\n".join(lines)


def build_large(trie, entries, seq_first, keycaps, tags, flags, tones, zwj_tone, zwj, dead) -> str:
    """The pattern-match field: every sequence in the table exactly once — the
    dead-headed ones too, on their own honestly-labeled line in section 10."""
    # Plain role forms ride their tone group's row (consumed from the pure-ZWJ
    # pool); what remains splits into families / couples & hearts / other chains.
    role_rows: list[list[tuple[int, ...]]] = []
    consumed: set[tuple[int, ...]] = set()
    zwj_set = set(zwj)
    for key, variants in sorted(group_by_tone(zwj_tone).items(), key=lambda kv: (kv[0][2:], kv[0])):
        row = []
        if key in zwj_set:
            row.append(key)
            consumed.add(key)
        row.extend(sorted(variants))
        role_rows.append(row)
    remaining = sorted(zwj_set - consumed)
    families = [c for c in remaining if set(c) <= PERSONS | {ZWJ}]
    couples = [c for c in remaining if c not in set(families) and HEARTS & set(c)]
    others = [c for c in remaining if c not in set(families) and c not in set(couples)]
    # The code section's example family: 👨‍👩‍👧 when the table has it (it does), else the first.
    fam0 = next((c for c in families if c == (0x1F468, ZWJ, 0x1F469, ZWJ, 0x1F467)), families[0])

    emitted = (len(keycaps) + len(tags) + len(flags) + len(tones) + len(zwj_tone)
               + len(consumed) + len(families) + len(couples) + len(others) + len(dead))
    if emitted != trie.seq_count:
        raise SystemExit(f"ERROR  coverage {emitted} != {trie.seq_count}: the large corpus must carry "
                         "every sequence exactly once")

    out: list[str] = [
        f"# emoji corpus — LARGE — the pattern-match field (generated: tools/gen_emoji_corpus.py).",
        f"# Every one of the table's {trie.seq_count} sequences appears exactly once (asserted at bake).",
        "# cluster mode: each is ONE glyph. leader mode: each is its pieces.",
        "# 1 flags · 2 keycaps · 3 tag flags · 4 base x five tones · 5 roles x tones",
        "# (plain form first when the font has it) · 6 families · 7 couples & hearts ·",
        "# 8 remaining chains · 9 VS16 on/off · 10 stays pieces · 11 beside code.",
        "",
        "######################## 1. flags — every RI pair in the table ########################",
        *rows(flags, 10),
        "",
        "######################## 2. keycaps ########################",
        *rows(keycaps, 12),
        "",
        "######################## 3. tag flags ########################",
        *rows(tags, 5),
        "",
        "########### 4. base x five tones (the bare base leads each row) ###########",
    ]
    for key, variants in sorted(group_by_tone(tones).items()):
        if len(key) != 1:
            raise SystemExit(f"ERROR  tone group keyed by {key!r} — expected a lone base codepoint")
        slot, _, _, fl = trie.lookup(key[0])
        if slot == 0 or fl & FLAG_MISSING:
            raise SystemExit(f"ERROR  tone base U+{key[0]:04X} has no bitmap to lead its row")
        out.append(chr(key[0]) + " " + " ".join(s(c) for c in sorted(variants)))
    out += [
        "",
        "########### 5. roles x tones (same role adjacent, plain form first) ###########",
    ]
    for row in role_rows:
        out.extend(rows(row, 10))
    out += [
        "",
        "######################## 6. families ########################",
        *rows(families, 8),
        "",
        "######################## 7. couples & hearts ########################",
        *rows(couples, 8),
        "",
        "######################## 8. remaining chains ########################",
        *rows(others, 8),
        "",
        "############ 9. VS16 on/off — one slot either way ############",
    ]
    for c in (keycaps[0], couples[0] if couples else keycaps[1], others[0] if others else keycaps[2]):
        out.append(s(c) + "   " + s((c[0], VS16, *c[1:])))
    out += [
        "",
        "########### 10. stays pieces (asserted non-resolving at bake) ###########",
    ]
    nf = not_a_flag(entries)
    ri_run = s(flags[0]) + s(flags[1]) + chr(flags[2][0])
    cps = [ord(c) for c in ri_run]
    if (probe(cps, 0, entries, seq_first, trie.seq_max) != (entries[flags[0]], 2)
            or probe(cps, 2, entries, seq_first, trie.seq_max) != (entries[flags[1]], 2)
            or probe(cps, 4, entries, seq_first, trie.seq_max) is not None):
        raise SystemExit("ERROR  the RI run of 5 no longer resolves as flag + flag + lone")
    unlisted = unlisted_chains(entries, seq_first, trie.seq_max)
    lone_zwj = "a" + chr(ZWJ) + "b " + "a" + chr(VS16) + "b"
    vs15_keycap = "1" + chr(VS15) + chr(KEYCAP)
    if probe([ord(c) for c in vs15_keycap], 0, entries, seq_first, trie.seq_max) is not None:
        raise SystemExit("ERROR  VS15 no longer breaks the keycap probe")
    out += [
        "not-a-flag-pair: " + s(nf) + "   RI singleton: " + chr(RI_LO) + "   RI run of 5 (two flags + a lone): " + ri_run,
        "unlisted chains: " + " ".join(s(c) for c in unlisted),
        "lone modifiers: " + "".join(chr(c) for c in range(TONE_LO, TONE_HI + 1)),
        f"VS15-broken keycap: {vs15_keycap}   lone ZWJ / VS16: {lone_zwj}",
        "dead table entries (a static-zero head never probes): " + " ".join(s(c) for c in dead),
        "",
        "########### 11. beside code ###########",
        f'fn family() -> &\'str {{ "{s(fam0)}" }}  // one glyph in cluster mode',
        f'let route = "{s(flags[0])}{s(flags[1])}";  // two flags, no spaces',
        f'// a toned role row in a comment: {" ".join(s(c) for c in role_rows[0])}',
        "",
    ]
    return "\n".join(out)


def build() -> tuple[str, str]:
    trie = Trie(CODEPOINTS.read_bytes())
    seqs = trie.sequences()
    entries = {c: slot for slot, c in seqs}
    seq_first = {c[0] for _, c in seqs}
    classes = classify(seqs)
    return (
        build_small(trie, entries, seq_first, *classes),
        build_large(trie, entries, seq_first, *classes),
    )


STAGED = re.compile(r"staged (\S+): (\d+) codepoints → (\d+) glyph instances \((\d+) copies, (\d+) missing/bitmap\)")


def check_render() -> int:
    """The simulator against the binary's own staged counts, on the corpora AND
    the calibration fixture (whose counts are pinned above). Sim == renderer on
    three files x two modes is the faithfulness claim."""
    if not BINARY.exists():
        raise SystemExit(f"ERROR  {BINARY} is missing — cargo build --release first")
    trie = Trie(CODEPOINTS.read_bytes())
    seqs = trie.sequences()
    entries = {c: slot for slot, c in seqs}
    seq_first = {c[0] for _, c in seqs}
    rc = 0
    for path in (CALIBRATION, SMALL, LARGE):
        text = path.read_text()
        for mode in ("leader", "cluster"):
            want = simulate(text, mode, trie, entries, seq_first)
            with tempfile.NamedTemporaryFile(suffix=".png", delete=True) as tmp:
                run = subprocess.run(
                    [str(BINARY), "--render-file", str(path.relative_to(ROOT / "native")),
                     "--frames", "1", "--screenshot", tmp.name]
                    + (["--cluster-mode", "cluster"] if mode == "cluster" else []),
                    cwd=ROOT / "native", capture_output=True, text=True)
            m = STAGED.search(run.stdout + run.stderr)
            if not m:
                print(f"FAIL  {path.name} {mode}: no staged line in the binary's output")
                rc = 1
                continue
            got = (int(m.group(2)), int(m.group(3)), int(m.group(5)))
            status = "ok" if got == want else f"MISMATCH (simulator says {want})"
            if got != want:
                rc = 1
            if path == CALIBRATION and want != CALIBRATION_COUNTS[mode]:
                print(f"FAIL  calibration: simulator moved off the known answer {CALIBRATION_COUNTS[mode]} "
                      f"(now {want}) — the fixture or the counter semantics changed")
                rc = 1
            print(f"  {path.name:28} {mode:8} codepoints {got[0]:>6}  cells {got[1]:>6}  missing {got[2]:>3}  {status}")
    print("check-render: the simulator and the renderer agree" if rc == 0 else "check-render: FAIL")
    return rc


def main() -> int:
    if "--check-render" in sys.argv[1:]:
        return check_render()
    small, large = build()
    if "--check" in sys.argv[1:]:
        rc = 0
        for path, fresh in ((SMALL, small), (LARGE, large)):
            if not path.exists() or path.read_text() != fresh:
                print(f"FAIL  {path.name} differs from a fresh bake — regenerate it "
                      "(python3 tools/gen_emoji_corpus.py)")
                rc = 1
        if rc == 0:
            print(f"[check] emoji-corpus-small.txt ({len(small)} B) + emoji-corpus-large.txt "
                  f"({len(large)} B) byte-identical to a fresh bake")
        return rc
    SMALL.write_text(small)
    LARGE.write_text(large)
    print(f"[done] wrote {SMALL} ({len(small)} B, {small.count(chr(10))} lines)")
    print(f"[done] wrote {LARGE} ({len(large)} B, {large.count(chr(10))} lines)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
