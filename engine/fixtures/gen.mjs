/**
 * gen.mjs — conformance fixture generator: the JS oracle's answers, serialized.
 *
 * Runs `runPipeline` (glyphPipelineReference — the semantic oracle) over a corpus
 * that hits every precision cliff the spec documents, and dumps each case as one
 * self-contained little-endian binary: inputs (bytes, trie, item table) followed by
 * expected outputs (slots, ordinal map, misses, leaders, per-item + batch bounds).
 *
 * The native engine's conformance runner (engine/conformance.mojo) replays the
 * inputs through its own port and diffs bit-for-bit. Floats are compared as bit
 * patterns, not tolerances: the oracle is deterministic and the port is required to
 * reproduce its exact f32/f64 rounding discipline — a tolerance would hide exactly
 * the class of bug (grouping-dependent float drift) this rig exists to catch.
 *
 * Format (all little-endian, packed, no alignment):
 *   u32 magic 'G3DF' (0x46443347)   u32 version=5
 *
 * v5: the item record gains CLUSTER MODE beside wrapMode — leader (0, today's
 * behaviour and the default) or cluster (1, the sequence pass) — and the
 * fixture gains a SEQUENCE PAYLOAD between the blocks and the item records:
 * the synthetic sequence table a cluster-mode fixture resolves against
 * ([slot, len, cps..] entries) plus the bitmap advance a resolved head
 * carries. seqCount 0 with a NaN advance = "no sequences" (the 25 pre-v5
 * fixtures' shape); the NaN poisons any read of a value that must be absent.
 *
 * v4: the item record gains WRAP MODE — WrapDown (0, today's behaviour and the
 * default) or WrapBack (1, where a wrap keeps the row and steps only in depth).
 * It is an ITEM-level parameter and rides beside wrapWidth for that reason.
 *
 * v2 CARRIER NOTE: float payloads (trie blocks, slots) are stored as f64 VALUES,
 * not as the buffer's current representation. f64 holds every f32 exactly (and
 * every u32, with 2^53 headroom), so the corpus survives a change of slot-lane
 * representation without regenerating. Which lanes are counts and which are
 * genuine floats is a property of the PIPELINE, so it lives in the differ, not
 * in this file. v1 stored raw f32 bits and was hostage to the buffer's type.
 *   u32 byteLen  u32 itemCount  u32 blockIndexLen  u32 blocksFloatLen
 *   u8[byteLen] bytes
 *   u32[blockIndexLen] blockIndex
 *   f64[blocksFloatLen] blocks (VALUES — the v2 carrier note above)
 *   u32 seqCount  u32 seqMax  f64 bitmapAdvance (NaN when seqCount == 0)
 *   seqCount x { u32 slot  u32 len  u32 cps[seqMax] (0-padded) }   [v5]
 *   itemCount x item record:
 *     u32 byteStart  u32 byteCount
 *     f64 originX originY originZ
 *     f64 wrapWidth  f64 wrapMode (0 = WrapDown, 1 = WrapBack)
 *     f64 clusterMode (0 = leader, 1 = cluster)                    [v5]
 *     f64 zStep  f64 lineHeight (NaN = unset)
 *     f64 hasPage (0|1)
 *     f64 pageRows pageCols scrollRows pagesWide pageGapX bandStrideY
 *         depthPerBand depthPerColumn
 *     f64 pageLineHeight (NaN = unset)
 *   u32 leaders
 *   u32 missCount  u32[missCount] misses (codepoints, byte order, dups kept)
 *   u32[byteLen] ordToByte
 *   f64[byteLen*8] measures  (VALUES — X Y Z ADVANCE HEIGHT GLYPH_ID BASE_X LINE_ADV)
 *   u32[byteLen*4] counts    (EXACT   — ROW COL FLAGS ORD)
 *   itemCount × f64[8] item bounds row (minX minY minZ maxX maxY maxZ totalRows
 *     maxRowExtent; an item with no leaders is +inf/+inf/+inf/-inf/-inf/-inf/0/0)
 *   f64[8] batch bounds row (same shape/sentinel)
 *
 * Run: bun engine/fixtures/gen.mjs   (writes *.pipe.bin beside this file)
 */

import { mkdirSync, writeFileSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runPipeline, SLOT_STRIDE, FLOAT_LANES, fval,
    S_GLYPH_ID, S_ADVANCE, S_HEIGHT, S_X, S_Y, S_Z,
    S_ROW, S_COL, S_FLAGS, S_BASE_X, S_LINE_ADV, S_ORD,
} from './inputs/glyphPipelineReference.js';
import { FIXTURE_MEASURE_STRIDE as MEASURE_STRIDE, FIXTURE_COUNT_STRIDE as COUNT_STRIDE } from '../glyph_schema.mjs';
import { buildGlyphTrie, trieLaneValue } from './inputs/GlyphTrie.js';

const HERE = dirname(fileURLToPath(import.meta.url));
const utf8 = (s) => new TextEncoder().encode(s);

// ── v5: the sequence payload. A case with `seqs: [[slot, [cps..]], ..]`
//    resolves them under clusterMode 1; the table is the fixture's whole
//    synthetic world for the sequence pass. Slots are allocated from 50000 up
//    — clearly outside the codepoint-derived id space (cp % 4093 + 1), so a
//    head/trailer mix-up can never hide behind a plausible-looking id.
//    bitmapAdvance is the head's advance under cluster mode, per fixture; the
//    awkward mantissa is on purpose (the f32 chain must carry it bit-exactly).
const SEQ_SLOT_BASE = 50000;

// ── Trie: synthetic metrics with awkward f32 mantissas, so every advance-sum
//    exercises real rounding. '@' is deliberately unmapped (the F_MISSING path);
//    emoji advance is doubled (the "x is a lookup, not a multiply" case).
const MISSING_ADVANCE = Math.fround(0.61);
const MISSING_HEIGHT = Math.fround(1.25);
function metricsFor(cp) {
    if (cp === 0x40 /* '@' */) return null;
    const emoji = cp >= 0x1F300;
    const advance = Math.fround((0.6 + (cp % 13) * 0.0173) * (emoji ? 2 : 1));
    const height = Math.fround(1.2 + (cp % 7) * 0.031);
    return { glyphId: (cp % 4093) + 1, advance, height };
}
function buildTrieFor(bytesList) {
    const cps = new Set();
    for (const bytes of bytesList) {
        for (const ch of new TextDecoder('utf-8', { fatal: false }).decode(bytes)) {
            cps.add(ch.codePointAt(0));
        }
    }
    cps.delete(0xFFFD);
    return buildGlyphTrie(cps, metricsFor, {
        missingAdvance: MISSING_ADVANCE, missingHeight: MISSING_HEIGHT,
    });
}

// ── The corpus ──────────────────────────────────────────────────────────────
const repoFile = readFileSync(
    join(HERE, 'inputs/foldGeometry.js'),
).subarray(0, 4096);

const longLine = 'const x = ' + 'ab(1, 2.5) + '.repeat(400) + '0;';

const malformed = new Uint8Array([
    0x61, 0x80, 0x80, 0xFF, 0x0A,             // stray continuations + invalid byte
    0xE2, 0x82,                                // truncated 3-byte sequence (€ minus a byte)
    0x62, 0xC3, 0x0A,                          // 2-byte leader whose continuation is \n
    0xF0, 0x9F, 0x9A, 0x80,                    // valid 🚀 to prove recovery
    0x63,
]);

const CASES = [
    {
        name: 'ascii-basic',
        bytes: utf8('hello world\nsecond line\n\nfourth line, longer than the rest'),
        items: [{ origin: { x: 1.5, y: 2.25, z: -3 }, lineHeight: 1.2 }],
    },
    {
        name: 'utf8-emoji',
        bytes: utf8('naïve café ☂ 🚀🌍 @mixed@\nsecond ✨ line\nüber-line 🎉 end\n'),
        items: [{ origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.3 }],
    },
    {
        name: 'wrap-exact',
        bytes: utf8('abcd\nabcdefgh\nab\nabcdefghijkl\n\nxy'),
        items: [{ origin: { x: -2, y: 4, z: 0.5 }, wrapWidth: 4, zStep: 0.25, lineHeight: 1.1 }],
    },
    {
        name: 'wrap-emoji',
        bytes: utf8('a🚀b🌍cdef\nxy✨z✨✨\n🎉🎉🎉🎉'),
        items: [{ origin: { x: 0, y: 0, z: 0 }, wrapWidth: 3, zStep: 0.4, lineHeight: 1.2 }],
    },
    {
        name: 'paged-rows',
        bytes: utf8(Array.from({ length: 40 }, (_, i) => `line ${i} of the paged body`).join('\n')),
        items: [{
            origin: { x: 0.5, y: -1, z: 2 }, lineHeight: 1.1,
            page: { pageRows: 6, pagesWide: 2, pageGapX: 0.8, bandStrideY: 9.5, depthPerBand: -2.5, scrollRows: 3, lineHeight: 1.1 },
        }],
    },
    {
        name: 'paged-cols',
        bytes: utf8('0123456789abcdefghij\nshort\nanother-long-line-here-with-cols\n'),
        items: [{
            origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.0,
            page: { pageCols: 8, depthPerColumn: 0.75, lineHeight: 1.0 },
        }],
    },
    {
        name: 'scroll-only',
        bytes: utf8(Array.from({ length: 12 }, (_, i) => `row ${i}`).join('\n')),
        items: [{ origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.3, page: { scrollRows: 5, lineHeight: 1.3 } }],
    },
    {
        name: 'long-line',
        bytes: utf8(longLine),
        items: [{ origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.0 }],
    },
    {
        name: 'malformed',
        bytes: malformed,
        items: [{ origin: { x: 1, y: 1, z: 1 }, lineHeight: 1.0 }],
    },
    {
        name: 'repo-file',
        bytes: new Uint8Array(repoFile),
        items: [{ origin: { x: 0, y: 0, z: 0 }, wrapWidth: 100, zStep: 0.1, lineHeight: 1.0 }],
    },
    // ── GNARLY REAL INPUTS. The constructed fixtures are polite: short lines,
    //    small counts, properties chosen one at a time. Real files are the fuzz
    //    tier — long files, long lines, mixed content — and a fixture is ~100x
    //    its source on disk (it carries every expected value in f64), so these
    //    are SLICES sized to keep the corpus checked-in-able, not whole trees.
    //    Whole-tree coverage is the oracle-free cross-form runner's job.
    {
        // A real production source file, WHOLE: 1,665 lines through the actual
        // oracle, wrapped AND paged at once — the co-occurrence the census
        // found missing from every polite fixture.
        name: 'real-kernels',
        bytes: new Uint8Array(readFileSync(
            join(HERE, 'inputs/glyphPipelineKernels.js'))),
        items: [{
            origin: { x: 0, y: 0, z: 0 }, wrapWidth: 80, zStep: 0.25, lineHeight: 1.2,
            page: { rows: 40, pagesWide: 3, lineHeight: 1.2, gapX: 2 },
        }],
    },
    {
        // A REAL minified bundle slice, VENDORED: engine/fixtures/inputs/
        // minified-sample.js is 48KB of one-enormous-line JS (47 newlines in
        // 49,152 bytes), checked in and frozen. It began life as the app
        // bundle's first 48KB (index-GRwQWdWg.js) — a content-hashed dist/
        // artifact that was never tracked, so the day a rebuild replaced it,
        // this generator died with ENOENT and the fixture became permanently
        // unreproducible. The input was RECOVERED from the committed fixture
        // itself (format v2 carries the source bytes) and vendored 2026-08-31.
        // A fixture input must be TRACKED: an untracked input makes the
        // fixture a snapshot of an accident.
        //
        // Why this shape: wrap at 120 makes hundreds of visual rows from a
        // single source line — the exact shape rowsUnderWrap exists for and
        // no constructed fixture dared.
        name: 'real-minified',
        bytes: new Uint8Array(readFileSync(
            join(HERE, 'inputs/minified-sample.js'))),
        items: [{
            origin: { x: 0, y: 0, z: 0 }, wrapWidth: 120, zStep: 0.1, lineHeight: 1.0,
        }],
    },
    (() => {
        const a = utf8('item A\nplain text body\n');
        const b = utf8('item B wraps at five and steps in z 🚀🚀\nmore\n');
        const c = utf8(Array.from({ length: 20 }, (_, i) => `C line ${i}`).join('\n'));
        const bytes = new Uint8Array(a.length + b.length + c.length);
        bytes.set(a, 0); bytes.set(b, a.length); bytes.set(c, a.length + b.length);
        return {
            name: 'multi-item',
            bytes,
            items: [
                { byteStart: 0, byteCount: a.length, origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.2 },
                { byteStart: a.length, byteCount: b.length, origin: { x: 10, y: 0, z: -1 }, wrapWidth: 5, zStep: 0.3, lineHeight: 1.0 },
                {
                    byteStart: a.length + b.length, byteCount: c.length,
                    origin: { x: -8, y: 3, z: 0 }, lineHeight: 1.1,
                    page: { pageRows: 4, pagesWide: 3, pageGapX: 0.5, depthPerBand: -1.5, lineHeight: 1.1 },
                },
            ],
        };
    })(),
    // ── WRAP MODE B. Added rather than flipped: every mode-A expectation above
    //    stays exactly where it was, so a regression in the default cannot hide
    //    behind a re-baselined fixture.
    {
        // The CONTROLLED A/B. Byte-for-byte the `wrap-exact` input with the same
        // wrap, zStep, lineHeight and origin — the ONLY difference from that
        // fixture is the mode, so any lane that differs between the two differs
        // because of the mode and nothing else. Its lines are 4, 8, 2, 12, 0 and 2
        // cells at wrap 4: three exact multiples, one partial, one empty.
        name: 'wrapback-mixed',
        bytes: utf8('abcd\nabcdefgh\nab\nabcdefghijkl\n\nxy'),
        items: [{
            origin: { x: -2, y: 4, z: 0.5 }, wrapWidth: 4, wrapMode: 1,
            zStep: 0.25, lineHeight: 1.1,
        }],
    },
    {
        // THE CASE THE MODE EXISTS FOR: one 5,212-cell line and no other, at wrap
        // 40. WrapDown gives it 131 rows; WrapBack gives it ONE, with 131 segments
        // receding in z. TOTAL_ROWS is the lane that says so.
        name: 'wrapback-long-line',
        bytes: utf8(longLine),
        items: [{ origin: { x: 0, y: 0, z: 0 }, wrapWidth: 40, wrapMode: 1, zStep: 0.3, lineHeight: 1.0 }],
    },
    (() => {
        // MIXED MODES IN ONE ARENA — the shape the monoid's precondition is about.
        // Three items, modes down/back/down, and the wrap changes across every
        // boundary too. combine is not associative across either change; what makes
        // this safe is that each item's first byte emits a RESETTING leaf, so no
        // interval without a reset ever spans two of these. The scan gate sweeps 8
        // chunk/group/shard tunings over exactly this file.
        const a = utf8('down item, wraps at five\nsecond\n');
        const b = utf8('back item 🚀 wraps at five and stacks in z 🌍\nmore back\n');
        const c = utf8('tail item down again, wrap seven\nlast\n');
        const bytes = new Uint8Array(a.length + b.length + c.length);
        bytes.set(a, 0); bytes.set(b, a.length); bytes.set(c, a.length + b.length);
        return {
            name: 'wrapback-items',
            bytes,
            items: [
                { byteStart: 0, byteCount: a.length, origin: { x: 0, y: 0, z: 0 }, wrapWidth: 5, wrapMode: 0, zStep: 0.2, lineHeight: 1.2 },
                { byteStart: a.length, byteCount: b.length, origin: { x: 10, y: 0, z: -1 }, wrapWidth: 5, wrapMode: 1, zStep: 0.3, lineHeight: 1.0 },
                { byteStart: a.length + b.length, byteCount: c.length, origin: { x: -8, y: 3, z: 0 }, wrapWidth: 7, wrapMode: 0, zStep: 0.1, lineHeight: 1.1 },
            ],
        };
    })(),
    (() => {
        const a = utf8('ab\n');
        const cont = new Uint8Array([0x80, 0x80, 0x80, 0x80]);   // leaderless item → null bounds
        const bytes = new Uint8Array(a.length + cont.length);
        bytes.set(a, 0); bytes.set(cont, a.length);
        return {
            name: 'cont-only-item',
            bytes,
            items: [
                { byteStart: 0, byteCount: a.length, origin: { x: 0, y: 0, z: 0 }, lineHeight: 1.0 },
                { byteStart: a.length, byteCount: cont.length, origin: { x: 5, y: 5, z: 5 }, lineHeight: 1.0 },
            ],
        };
    })(),
    // ── THE SEQUENCE PASS (clusterMode 1). Synthetic sequence tables in the
    //    fixture's own id space (SEQ_SLOT_BASE 50000, so a head/trailer mix-up
    //    can never hide behind a plausible-looking id); the rule is the
    //    oracle's resolveClusters. bitmapAdvance carries an awkward mantissa on
    //    purpose — the f32 chain must move it bit-exactly.
    (() => {
        // The controlled A/B: the same content twice, cluster item then leader
        // item. The diff between the two IS the resolution — anti-vacuity by
        // construction (if the pass never fired, the items would agree).
        const a = utf8('🚀\u200D🌍 x\n');
        const bytes = new Uint8Array(a.length * 2);
        bytes.set(a, 0); bytes.set(a, a.length);
        return {
            name: 'cluster-zwj',
            bytes,
            seqs: [[50000, [0x1F680, 0x200D, 0x1F30D]]],
            bitmapAdvance: Math.fround(1.318),
            items: [
                { byteStart: 0, byteCount: a.length, origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 },
                { byteStart: a.length, byteCount: a.length, origin: { x: 0, y: -3, z: 0 }, clusterMode: 0, lineHeight: 1.0 },
            ],
        };
    })(),
    {
        // RI pairing, greedy from the left — GB12/GB13 with no parity state:
        // the table pair (A C) resolves, the lone RI stays single, and a
        // NON-table pair (A D) stays two singles even though A starts a known
        // sequence — the fallback is pinned, not assumed.
        name: 'cluster-flags',
        bytes: utf8('🇦🇨 🇩 🇦🇩 x\n'),
        seqs: [[50001, [0x1F1E6, 0x1F1E8]]],
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 }],
    },
    {
        // Keycaps, both spellings: '1' FE0F 20E3 and '1' 20E3 resolve to the
        // SAME slot — the FE0F normalization pin (the font's GSUB strips VS16;
        // real text carries it).
        name: 'cluster-keycap',
        bytes: utf8('1️⃣ 1⃣ x\n'),
        seqs: [[50002, [0x31, 0x20E3]]],
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 }],
    },
    {
        // The fallback: a ZWJ chain the table does NOT have, against a table
        // that exists but matches nothing here. Pieces render per codepoint
        // and the ZWJ goes zero-width — the invisible-by-design rule firing
        // without a match.
        name: 'cluster-unmatched',
        bytes: utf8('🚀\u200D🌍 x\n'),
        seqs: [[50005, [0x1F600, 0x200D, 0x1F601]]],   // a chain this item lacks
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 }],
    },
    {
        // Skin tone: the two-codepoint modifier sequence resolves whole.
        name: 'cluster-skin',
        bytes: utf8('👍🏽 x\n'),
        seqs: [[50003, [0x1F44D, 0x1F3FD]]],
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 }],
    },
    {
        // The probe stops at a newline (GB4/GB5): the chain is cut by the line
        // break, so nothing resolves — the pieces render on their own rows and
        // the ZWJ is zero-width. If a match ever crossed the newline, this
        // fixture's bounds move.
        name: 'cluster-newline',
        bytes: utf8('🚀\u200D\n🌍\n'),
        seqs: [[50000, [0x1F680, 0x200D, 0x1F30D]]],
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 }],
    },
    (() => {
        // LONGEST-MATCH: the table holds both (🚀, ZWJ) and (🚀, ZWJ, 🌍); the
        // full chain resolves to the longer entry, the cut one to the shorter.
        // A first-match-wins implementation gets this fixture wrong.
        const a = utf8('🚀\u200D🌍\n');
        const b = utf8('🚀\u200D x\n');
        const bytes = new Uint8Array(a.length + b.length);
        bytes.set(a, 0); bytes.set(b, a.length);
        return {
            name: 'cluster-longest',
            bytes,
            seqs: [[50000, [0x1F680, 0x200D, 0x1F30D]], [50004, [0x1F680, 0x200D]]],
            bitmapAdvance: Math.fround(1.318),
            items: [
                { byteStart: 0, byteCount: a.length, origin: { x: 0, y: 0, z: 0 }, clusterMode: 1, lineHeight: 1.0 },
                { byteStart: a.length, byteCount: b.length, origin: { x: 0, y: -3, z: 0 }, clusterMode: 1, lineHeight: 1.0 },
            ],
        };
    })(),
    {
        // A cluster inside a WRAPPED line: row/col count leaders (unchanged),
        // while the x positions compress — the fold reads the rewritten static
        // lanes exactly as it always did.
        name: 'cluster-wrap',
        bytes: utf8('ab 🚀\u200D🌍 cd ef gh\n'),
        seqs: [[50000, [0x1F680, 0x200D, 0x1F30D]]],
        bitmapAdvance: Math.fround(1.318),
        items: [{ origin: { x: 0, y: 0, z: 0 }, wrapWidth: 4, zStep: 0.2, clusterMode: 1, lineHeight: 1.0 }],
    },
];


// The slot buffer is u32: COUNT lanes (S_ROW/S_COL/S_FLAGS/S_ORD) are stored
// natively, FLOAT lanes are bitcast. The corpus carries VALUES, so each lane is
// decoded by kind before it is written — otherwise a float lane serializes its
// BIT PATTERN as an f64 value and every fixture shifts while nothing semantic
// moved. S_GLYPH_ID is deferred (still a trie float), so it decodes as a float.
// FLOAT_LANES is imported — the lane kinds live in ONE place (the oracle).
// v3: the oracle still carries 12 mixed lanes in one u32 array; the SCHEMA says
// measures and counts are different buffers. Split here — the generator is the
// seam, exactly as it was for the f64 carrier in v2. Measures go out as f64
// VALUES (representation-independent); counts go out as exact u32.
const MEASURE_FROM = [S_X, S_Y, S_Z, S_ADVANCE, S_HEIGHT, S_GLYPH_ID, S_BASE_X, S_LINE_ADV];
const COUNT_FROM = [S_ROW, S_COL, S_FLAGS, S_ORD];
if (MEASURE_FROM.length !== MEASURE_STRIDE || COUNT_FROM.length !== COUNT_STRIDE) {
    throw new Error(`fixture lane map disagrees with the schema (${MEASURE_FROM.length}/${MEASURE_STRIDE}, `
        + `${COUNT_FROM.length}/${COUNT_STRIDE}) — run bun tools/gen-schema.mjs`);
}
function writeSlotValues(w, slots) {
    const nb = slots.length / SLOT_STRIDE;
    for (let i = 0; i < nb; i++) {
        const base = i * SLOT_STRIDE;
        for (const lane of MEASURE_FROM) {
            w.f64(FLOAT_LANES.has(lane) ? fval(slots[base + lane]) : slots[base + lane]);
        }
    }
    for (let i = 0; i < nb; i++) {
        const base = i * SLOT_STRIDE;
        for (const lane of COUNT_FROM) w.u32(slots[base + lane]);
    }
}


// ── Binary writer ───────────────────────────────────────────────────────────
class Writer {
    constructor() { this.chunks = []; this.len = 0; }
    _push(buf) { this.chunks.push(new Uint8Array(buf)); this.len += buf.byteLength; }
    u32(v) { const b = new DataView(new ArrayBuffer(4)); b.setUint32(0, v >>> 0, true); this._push(b.buffer); }
    f32(v) { const b = new DataView(new ArrayBuffer(4)); b.setFloat32(0, v, true); this._push(b.buffer); }
    f64(v) { const b = new DataView(new ArrayBuffer(8)); b.setFloat64(0, v, true); this._push(b.buffer); }
    bytes(arr) { this._push(arr.buffer ? arr.slice().buffer : arr); }
    u32array(arr) { for (const v of arr) this.u32(v); }
    f32array(arr) { for (const v of arr) this.f32(v); }
    f64array(arr) { for (const v of arr) this.f64(v); }
    done() {
        const out = new Uint8Array(this.len);
        let at = 0;
        for (const c of this.chunks) { out.set(c, at); at += c.length; }
        return out;
    }
}

const boundsRow = (b) => b === null
    ? [Infinity, Infinity, Infinity, -Infinity, -Infinity, -Infinity, 0, 0]
    : [b.min.x, b.min.y, b.min.z, b.max.x, b.max.y, b.max.z, b.totalRows, b.maxRowExtent];

mkdirSync(HERE, { recursive: true });
for (const c of CASES) {
    const items = c.items.map((it, i) => ({
        byteStart: it.byteStart ?? 0,
        byteCount: it.byteCount ?? c.bytes.length,
        ...it,
    }));
    const trie = buildTrieFor([c.bytes]);
    // v5: the sequence payload rides the trie the runPipeline sees, exactly
    // where the engine's Trie carries it. No `seqs` on the case → no table —
    // resolveClusters early-returns even for cluster items.
    if (c.seqs && c.seqs.length) {
        if (!(typeof c.bitmapAdvance === 'number' && Number.isFinite(c.bitmapAdvance))) {
            throw new Error(`${c.name}: seqs without a finite bitmapAdvance`);
        }
        trie.seqMax = Math.max(...c.seqs.map(([, cps]) => cps.length));
        trie.bitmapAdvance = c.bitmapAdvance;
        trie.seq = [];
        for (const [slot, cps] of c.seqs) {
            trie.seq.push(slot, cps.length, ...cps, ...new Array(trie.seqMax - cps.length).fill(0));
        }
    }
    const r = runPipeline(c.bytes, trie, { items });

    const w = new Writer();
    w.u32(0x46443347); w.u32(5);
    w.u32(c.bytes.length); w.u32(items.length);
    w.u32(trie.blockIndex.length); w.u32(trie.blocks.length);
    w.bytes(c.bytes);
    w.u32array(trie.blockIndex);
    // Per LANE, not raw: blocks are u32 with the measures BITCAST, so the raw word
    // for advance/height is a bit pattern, not a value. The format carries VALUES
    // precisely so a container change leaves the corpus untouched — decoding here
    // is what makes that true.
    for (let i = 0; i < trie.blocks.length; i++) w.f64(trieLaneValue(trie.blocks, i));
    // v5: the sequence payload, between the blocks and the item records.
    w.u32(trie.seq ? c.seqs.length : 0);
    w.u32(trie.seq ? trie.seqMax : 0);
    w.f64(trie.seq ? trie.bitmapAdvance : NaN);
    if (trie.seq) {
        for (const [slot, cps] of c.seqs) {
            w.u32(slot); w.u32(cps.length);
            for (let k = 0; k < trie.seqMax; k++) w.u32(cps[k] ?? 0);
        }
    }
    for (const it of items) {
        w.u32(it.byteStart); w.u32(it.byteCount);
        w.f64(it.origin?.x || 0); w.f64(it.origin?.y || 0); w.f64(it.origin?.z || 0);
        w.f64(it.wrapWidth ?? 0); w.f64(it.wrapMode ?? 0);
        w.f64(it.clusterMode ?? 0);
        w.f64(it.zStep ?? 0); w.f64(it.lineHeight ?? NaN);
        const p = it.page;
        w.f64(p ? 1 : 0);
        w.f64(p?.pageRows || 0); w.f64(p?.pageCols || 0); w.f64(p?.scrollRows || 0);
        w.f64(p?.pagesWide || 0); w.f64(p?.pageGapX || 0); w.f64(p?.bandStrideY || 0);
        w.f64(p?.depthPerBand || 0); w.f64(p?.depthPerColumn || 0);
        w.f64(p?.lineHeight ?? NaN);
    }
    w.u32(r.leaders);
    w.u32(r.misses.length); w.u32array(r.misses);
    w.u32array(r.ordToByte);
    writeSlotValues(w, r.slots);
    for (const b of r.itemBounds) for (const v of boundsRow(b)) w.f64(v);
    for (const v of boundsRow(r.bounds)) w.f64(v);

    const path = join(HERE, `${c.name}.pipe.bin`);
    writeFileSync(path, w.done());
    console.log(`${c.name}: ${c.bytes.length} bytes, ${items.length} item(s), ` +
        `${r.leaders} leaders, ${r.misses.length} misses → ${w.len} B fixture`);
}
