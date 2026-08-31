#!/usr/bin/env node
/**
 * gen-real-trie.mjs — Stage E1: bake the engine trie from the REAL atlas.
 *
 * Reads the Stage B export (assets/atlas/codepoints.bin + glyphs.bin) and emits
 * assets/atlas/engine-trie.bin — a 'G3TR' blob in the engine's trie container
 * layout, so `glyph_engine_load_trie_file` resolves every source byte to the
 * REAL FontChain global slot (the id the renderer's glyph-map texture is keyed
 * by) with REAL advances, instead of the conformance fixtures' toy
 * `(cp % 4093) + 1` ids.
 *
 * ── The metric conversion (the whole point of the file) ─────────────────────
 *
 * codepoints.bin carries measures as INTEGER primary-font units (lossless);
 * the engine trie, like the web's GlyphTrie, carries them as f32 WORLD units.
 * The conversion is FORMAT.md's "World-space conversion", anchored on the same
 * constant text.rs uses (CELL_HEIGHT_WORLD = 1.0):
 *
 *     advance_world = Math.fround(advanceFu * CELL_HEIGHT_WORLD / emHeightFu)
 *     height_world  = Math.fround(heightFu  * CELL_HEIGHT_WORLD / emHeightFu)  // = 1.0
 *
 * i.e. cell advance 1229/2320 ≈ 0.52974 world, glyph quad height exactly 1.0.
 * Computed in f64 and rounded ONCE on the store (Math.fround / setFloat32 —
 * the repo's rounding discipline), so the Rust side reproduces the bits with
 * `(advance_fu as f64 / em_height_fu as f64) as f32`.
 *
 * Note (documented, not copied): the web's liveTrie.js uses
 * `(ax/upem) * worldScale * charSize.height` — upem (2048), not emHeight
 * (2320), in the denominator, which is ~13% wider than the geometric cell
 * ratio (its own header calls this deliberate). The native port anchors on the
 * geometric ratio (FORMAT.md), which is what the Stage C renderer stages.
 *
 * ── G3TR blob format (little-endian, packed u32 words) ──────────────────────
 *
 *   word 0   magic 'G3TR' (0x52543347)
 *   word 1   version = 1
 *   word 2   headerBytes = 44
 *   word 3   blockShift = 8
 *   word 4   blockIndexLength (4352)
 *   word 5   blockCount (content-deduplicated blocks)
 *   word 6   entryStride = 4 (u32 lanes per entry)
 *   word 7   mappedCount (informational)
 *   word 8   primaryUpem (informational)
 *   word 9   primaryEmHeightFu (informational — the conversion denominator)
 *   word 10  cellHeightWorld as f32 BITS (a measure rides an f32 carrier)
 *   word 11… blockIndex u32[blockIndexLength]
 *   then     blocks: blockCount × 256 entries × 4 words, lane order
 *            [GLYPH_ID u32 native][ADVANCE f32 bits][HEIGHT f32 bits][FLAGS u32 native]
 *
 * The identity and the bitfield cross as NATIVE u32; the two measures as
 * bitcast f32 — exactly the web trie's container (GlyphTrie.js ENTRY_STRIDE),
 * which the Mojo loader splits by carrier (blocks_m f32 ×2, blocks_c u32 ×2).
 *
 * Run: node tools/gen-real-trie.mjs
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ATLAS = join(HERE, '../assets/atlas');

const CELL_HEIGHT_WORLD = 1.0; // == text.rs CELL_HEIGHT_WORLD

const MAGIC_CP = 0x50433347; // 'G3CP'
const MAGIC_GL = 0x4c473347; // 'G3GL'
const MAGIC_TR = 0x52543347; // 'G3TR'

function readWords(name) {
    const buf = readFileSync(join(ATLAS, name));
    if (buf.byteLength % 4) throw new Error(`${name}: not a u32 array`);
    return new Uint32Array(buf.buffer, buf.byteOffset, buf.byteLength / 4);
}

// ── load + cross-check the two sources ──────────────────────────────────────

const cp = readWords('codepoints.bin');
if (cp[0] !== MAGIC_CP) throw new Error('codepoints.bin: bad magic');
if (cp[1] !== 1) throw new Error('codepoints.bin: version != 1');
const [blockShift, blockIndexLen, blockCount, entryStride, mappedCount,
    missingAdvanceFu, missingHeightFu, cpPrimaryUpem] =
    [cp[3], cp[4], cp[5], cp[6], cp[7], cp[8], cp[9], cp[10]];
if (blockShift !== 8 || entryStride !== 4) {
    throw new Error(`codepoints.bin: unexpected shape (shift ${blockShift}, stride ${entryStride})`);
}

const gl = readWords('glyphs.bin');
if (gl[0] !== MAGIC_GL) throw new Error('glyphs.bin: bad magic');
const [glPrimaryUpem, primaryAdvanceFu, primaryEmHeightFu] = [gl[5], gl[6], gl[7]];
if (glPrimaryUpem !== cpPrimaryUpem) {
    throw new Error(`upem mismatch: glyphs.bin ${glPrimaryUpem} vs codepoints.bin ${cpPrimaryUpem}`);
}
if (missingAdvanceFu !== primaryAdvanceFu || missingHeightFu !== primaryEmHeightFu) {
    throw new Error('codepoints.bin missing-block metrics disagree with glyphs.bin primary metrics');
}

const blockIndex = cp.subarray(11, 11 + blockIndexLen);
const blocks = cp.subarray(11 + blockIndexLen,
    11 + blockIndexLen + blockCount * (1 << blockShift) * entryStride);

// ── convert: integer font units → f32 world units, rounded once per store ───

const f32 = new DataView(new ArrayBuffer(4));
const fbits = (v) => { f32.setFloat32(0, v, true); return f32.getUint32(0, true); };

const em = primaryEmHeightFu;
const toWorld = (fu) => fbits((fu * CELL_HEIGHT_WORLD) / em);

const outBlocks = new Uint32Array(blocks.length);
for (let e = 0; e < blocks.length; e += 4) {
    outBlocks[e + 0] = blocks[e + 0];                 // GLYPH_ID — identity, native u32
    outBlocks[e + 1] = toWorld(blocks[e + 1]);        // ADVANCE — measure, bitcast f32
    outBlocks[e + 2] = toWorld(blocks[e + 2]);        // HEIGHT  — measure, bitcast f32
    outBlocks[e + 3] = blocks[e + 3];                 // FLAGS   — bitfield, native u32
}

// ── write the blob ──────────────────────────────────────────────────────────

const HEADER_WORDS = 11;
const out = new Uint32Array(HEADER_WORDS + blockIndexLen + outBlocks.length);
out.set([
    MAGIC_TR, 1, HEADER_WORDS * 4, blockShift, blockIndexLen, blockCount,
    entryStride, mappedCount, glPrimaryUpem, primaryEmHeightFu,
    fbits(CELL_HEIGHT_WORLD),
]);
out.set(blockIndex, HEADER_WORDS);
out.set(outBlocks, HEADER_WORDS + blockIndexLen);
const outPath = join(ATLAS, 'engine-trie.bin');
writeFileSync(outPath, new Uint8Array(out.buffer, out.byteOffset, out.byteLength));

// ── verification: walk the WRITTEN blob like the engine does ────────────────

const fval = (u) => { f32.setUint32(0, u >>> 0, true); return f32.getFloat32(0, true); };
const lookup = (codepoint) => {
    const block = out[HEADER_WORDS + (codepoint >> blockShift)];
    const e = HEADER_WORDS + blockIndexLen + ((block << blockShift) | (codepoint & 0xff)) * 4;
    return {
        glyphId: out[e], advance: fval(out[e + 1]), height: fval(out[e + 2]), flags: out[e + 3],
    };
};
const expect = (cpWant, gid, advFu, flags, label) => {
    const t = lookup(cpWant);
    const advWant = Math.fround(advFu * CELL_HEIGHT_WORLD / em);
    if (t.glyphId !== gid || t.advance !== advWant || t.height !== 1.0 || t.flags !== flags) {
        throw new Error(`${label}: got ${JSON.stringify(t)}, want gid=${gid} adv=${advWant} h=1 flags=${flags}`);
    }
    console.log(`  ${label}: slot ${t.glyphId}, advance ${t.advance}, height ${t.height}, flags ${t.flags}`);
};
console.log('[verify] worked-example codepoints through the written blob:');
expect(0x0041, 34, 1229, 0, "'A'      (outline, one cell)");
expect(0x0020, 1, 1229, 0, "' '      (empty slot, one cell)");
expect(0x1f400, 3839, 2458, 2, "'🐀'     (bitmap, double advance)");
expect(0x1f680, 0, 1229, 1, "'🚀'     (missing — shared missing block)");
// invariants over every entry: height is the constant cell height; block 0 is
// the shared missing block; every unmapped index slot points at it.
for (let e = 0; e < outBlocks.length; e += 4) {
    if (fval(outBlocks[e + 2]) !== 1.0) throw new Error(`entry ${e / 4}: height != 1.0`);
}
if (blockIndex.some((b, i) => b === 0 && lookup(i << 8).flags !== 1 && i !== 0)) {
    throw new Error('a blockIndex slot points at block 0 without FLAG_MISSING');
}
console.log(`[done] ${outPath}`);
console.log(`[done] ${mappedCount} mapped codepoints, ${blockCount} unique blocks, ` +
    `${out.length * 4} bytes (${((out.length * 4) / 1024).toFixed(1)} KiB)`);
