#!/usr/bin/env node
/**
 * export-atlas.mjs — Stage B exporter: glyph3d-js prebaked slug core → native binary assets.
 *
 * Produces (in ../../assets/atlas/, override with --out):
 *   curves.bin     'G3CV' — curve texture texels (RGBA32Uint payload) + dims
 *   glyphmap.bin   'G3GM' — glyph-map texture texels (RGBA32Uint payload) + dims
 *   glyphs.bin     'G3GL' — per-slot metrics/flags + font table + name table
 *   codepoints.bin 'G3CP' — codepoint → glyph two-level trie (u32 blocks)
 *
 * Method:
 *   1. STATICALLY decodes the web app's build-time baked asset
 *      app/public/slug-core/slug-core.<key>.bin (gzip of the 'SLGC' envelope —
 *      see packages/glyph3d-core/src/shaping/slugCoreCache.js). These are the exact
 *      bytes the web renderer uploads; we re-pack them unchanged.
 *   2. Reproduces the bake boot headlessly (tools/headlessFontChain.mjs in the
 *      reference repo, READ-ONLY) to recover the codepoint → slot mapping and the
 *      per-slot metrics (they are NOT in the envelope). Slot allocation is
 *      deterministic (dense counter in prime order), so reproducing the exact bake
 *      sequence yields the same slot ids — and we ASSERT that against the baked
 *      envelope's encodedIds before writing anything.
 *
 * Usage:  node tools/export-atlas.mjs [--out <dir>]
 * Requires: node ≥ 18 (CompressionStream-free; we use zlib). Reference repo must
 * exist at REF_ROOT (below) and is never written to.
 */
import { readFileSync, writeFileSync, mkdirSync, readdirSync } from 'fs';
import { gunzipSync } from 'zlib';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REF_ROOT = '/Users/lugo/localdev/viz-web/glyph3d-js';
const SLUG_CORE_DIR = join(REF_ROOT, 'app/public/slug-core');
const OUT_DIR = process.argv.includes('--out')
    ? process.argv[process.argv.indexOf('--out') + 1]
    : join(HERE, '..', 'assets', 'atlas');

const TEXTURE_WIDTH = 1024;
const CURVE_TEXELS_PER_CURVE = 2;
const SLUG_MAGIC = 0x43474c53; // 'SLGC'

// Trie constants (mirror packages/glyph3d-core/src/compute/GlyphTrie.js)
const BLOCK_SHIFT = 8, BLOCK_SIZE = 256, BLOCK_MASK = 255;
const BLOCK_INDEX_LENGTH = 0x110000 >> BLOCK_SHIFT; // 4352
const ENTRY_STRIDE = 4;
const FLAG_MISSING = 1, FLAG_BITMAP = 2, FLAG_BLANK = 4; // BLANK = covered but slot 0

// glyphs.bin slot flags
const SLOT_FLAG_BITMAP = 1, SLOT_FLAG_EMPTY = 2;
const FONTIDX_BLANK = 0xffffffff, FONTIDX_BITMAP = 0xfffffffe, NO_CELL = 0xffffffff;

// ── step 1: decode the baked envelope ────────────────────────────────────────

const binName = readdirSync(SLUG_CORE_DIR).find((f) => f.endsWith('.bin'));
if (!binName) throw new Error(`no baked slug-core in ${SLUG_CORE_DIR}`);
const gz = readFileSync(join(SLUG_CORE_DIR, binName));
const raw = gunzipSync(gz);
const env = new Uint32Array(raw.buffer, raw.byteOffset, raw.byteLength / 4);
if (env[0] !== SLUG_MAGIC) throw new Error('bad SLGC magic');
const [, envVer, payloadFmt, curveCount, entryCount, encodedLen, curveLen, mapLen] = env;
let eo = 8;
const encodedIds = env.slice(eo, eo + encodedLen); eo += encodedLen;
const curveTexels = env.slice(eo, eo + curveLen); eo += curveLen;
const mapTexels = env.slice(eo, eo + mapLen);
console.log(`[bake] ${binName}: envelope v${envVer} fmt v${payloadFmt}, ` +
    `${encodedLen} glyphs, ${curveCount} curves, maxGlyphId=${entryCount - 1}`);

const curveHeight = Math.max(1, Math.ceil((curveCount * CURVE_TEXELS_PER_CURVE) / TEXTURE_WIDTH));
const mapHeight = Math.max(1, Math.ceil(entryCount / TEXTURE_WIDTH));

// ── step 2: reproduce the bake boot (read-only imports from the reference repo) ──
//
// The repo's vendor/hb.js is UMD with an appended `export default`; Node ≥22 refuses
// to parse it (ERR_AMBIGUOUS_MODULE_SYNTAX — top-level `require` + ESM export). Bun
// (the repo's bake runtime) accepts it. We therefore keep a copy at
// tools/vendor/hb.cjs (verbatim EXCEPT the trailing ESM `export default` line is
// stripped so require() parses it as CommonJS) plus a verbatim hb.wasm, initialize
// hbjs from the repo (ESM-safe), and hand the shared hb module into the repo's real
// FontChain/HarfBuzzShaper — mirroring tools/headlessFontChain.mjs (FONTS
// names/order, stub emoji atlas) exactly so slot allocation reproduces the baked
// core bit-for-bit.

import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const createHarfBuzz = require('./vendor/hb.cjs');

const { FONTS, stubEmojiAtlas } = await import(`${REF_ROOT}/tools/headlessFontChain.mjs`);
const { LARGE_CORE_RANGES } = await import(`${REF_ROOT}/packages/glyph3d-r3f/src/coreRanges.js`);
const { FontChain, MonospaceShapeCache } = await import(`${REF_ROOT}/packages/glyph3d-core/src/shaping/index.js`);
const HarfBuzzShaper = (await import(`${REF_ROOT}/packages/glyph3d-core/src/shaping/HarfBuzzShaper.js`)).default;
const hbjs = (await import(`${REF_ROOT}/packages/glyph3d-core/src/shaping/vendor/hbjs.js`)).default;

const hbInstance = await createHarfBuzz({
    locateFile: (p) => (p.endsWith('.wasm') ? join(HERE, 'vendor', 'hb.wasm') : p),
});
const hb = hbjs(hbInstance);

const fonts = FONTS.map((f) => ({ url: join(REF_ROOT, f.file), name: f.name }));
const chain = new FontChain();
for (const spec of fonts) {                 // == FontChain.init's loop, minus fetch()
    const buf = readFileSync(spec.url);
    const shaper = new HarfBuzzShaper();
    await shaper.init(buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength),
        { hb, name: spec.name });
    chain._fonts.push({ shaper, coverage: new Set(shaper.collectUnicodes()), name: spec.name });
}
chain._ready = true;
chain.setEmojiAtlas(stubEmojiAtlas());

const codepointsFromRanges = (ranges) => {
    let s = '';
    for (const [lo, hi] of ranges) for (let cp = lo; cp <= hi; cp++) s += String.fromCodePoint(cp);
    return s;
};
const text = codepointsFromRanges(LARGE_CORE_RANGES);
const shapeCache = new MonospaceShapeCache(chain);
shapeCache.prime(text);   // ← this is what allocates the global slots, in bake order

// codepoint → {slot, ax} for every primed codepoint (sorted for determinism)
const cpEntries = [];   // [cp, slot, advanceFu]
{
    const cps = [...shapeCache.codepoints()].sort((a, b) => a - b);
    for (const cp of cps) {
        const e = shapeCache.lookup(cp);       // guaranteed cache hits (primed)
        cpEntries.push([cp, e.g, e.ax]);
    }
}

// ── step 3: cross-check the reproduction against the baked envelope ──────────

const encodedSet = new Set(encodedIds);
const shapedSlots = new Set(cpEntries.map(([, g]) => g).filter((g) => g > 0));
const missingFromBake = [...shapedSlots].filter((g) => !encodedSet.has(g));
const extraInBake = [...encodedSet].filter((g) => !shapedSlots.has(g));
if (missingFromBake.length || extraInBake.length) {
    throw new Error(`slot-set mismatch vs baked core: shaped-not-baked=[${missingFromBake.slice(0, 8)}…] ` +
        `baked-not-shaped=[${extraInBake.slice(0, 8)}…] — font files or ranges drifted; aborting.`);
}
console.log(`[check] slot set reproduced exactly (${shapedSlots.size} slots)`);

const slotCount = chain.slotCount;
if (slotCount !== entryCount) {
    throw new Error(`slotCount ${slotCount} != baked entryCount ${entryCount} — allocation drifted; aborting.`);
}
// Bitmap emojiCell must match the baked map texels (slot*4+3).
for (let s = 0; s < slotCount; s++) {
    if (chain.isBitmapSlot(s)) {
        const bakedCell = mapTexels[s * 4 + 3];
        if (bakedCell !== chain.emojiCellOf(s)) {
            throw new Error(`bitmap slot ${s}: baked cell ${bakedCell} != reproduced ${chain.emojiCellOf(s)}`);
        }
        if (mapTexels[s * 4 + 2] !== 1) throw new Error(`bitmap slot ${s}: baked mode != 1`);
    }
}
console.log(`[check] bitmap slot cells match the baked glyph-map texels`);

// ── step 4: per-slot metrics + normalized bboxes ─────────────────────────────

const slotMeta = [];  // per slot: {fontIdx, gid, name, advanceFu, asc, desc, flags, emojiCell, curveStart, curveCount, bbox}
for (let s = 0; s < slotCount; s++) {
    const m = chain._slotMeta[s];
    const isBitmap = m.fontIdx === -2;
    const isBlank = m.fontIdx === -1;
    const curveStart = mapTexels[s * 4 + 0], curveCnt = mapTexels[s * 4 + 1];
    let bbox = [0, 0, 0, 0];
    if (!isBitmap && curveCnt > 0) {
        let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
        for (let c = 0; c < curveCnt; c++) {
            const t = (curveStart + c) * CURVE_TEXELS_PER_CURVE * 4;
            for (const v of [curveTexels[t], curveTexels[t + 2], curveTexels[t + 4]]) {
                if (v < x0) x0 = v; if (v > x1) x1 = v;
            }
            for (const v of [curveTexels[t + 1], curveTexels[t + 3], curveTexels[t + 5]]) {
                if (v < y0) y0 = v; if (v > y1) y1 = v;
            }
        }
        bbox = [x0 / 65535, y0 / 65535, x1 / 65535, y1 / 65535];
    }
    const flags = (isBitmap ? SLOT_FLAG_BITMAP : 0) | (!isBitmap && curveCnt === 0 ? SLOT_FLAG_EMPTY : 0);
    slotMeta.push({
        fontIdx: isBlank ? FONTIDX_BLANK : isBitmap ? FONTIDX_BITMAP : m.fontIdx,
        gid: isBitmap ? 0 : m.gid,
        name: isBlank ? '.blank' : isBitmap ? `<emoji cell ${m.cell}>` : chain._fonts[m.fontIdx].shaper.glyphName(m.gid),
        advanceFu: isBlank || isBitmap ? 0 : chain.glyphAdvance(s),
        asc: isBlank || isBitmap ? 0 : chain.fontExtents(s).ascender,
        desc: isBlank || isBitmap ? 0 : chain.fontExtents(s).descender,
        flags,
        emojiCell: isBitmap ? chain.emojiCellOf(s) : NO_CELL,
        curveStart, curveCount: curveCnt, bbox,
    });
}

const fontsMeta = chain._fonts.map((f) => ({
    name: f.name, upem: f.shaper.upem, ...f.shaper.fontExtents(),
}));
const primaryUpem = chain.upem;
const primaryAdvanceFu = chain.shape('M')[0].ax;          // == FontChain._primaryAx()
const primaryEmHeightFu = fontsMeta[0].ascender - fontsMeta[0].descender;
console.log(`[fonts] primary=${fontsMeta[0].name} upem=${primaryUpem} cellAdvance=${primaryAdvanceFu} emHeight=${primaryEmHeightFu}`);
for (const f of fontsMeta) console.log(`        ${f.name}: upem=${f.upem} asc=${f.ascender} desc=${f.descender}`);

// ── step 5: build the codepoint trie (GlyphTrie layout, extended flags) ──────

const missingBlock = new Uint32Array(BLOCK_SIZE * ENTRY_STRIDE);
for (let i = 0; i < BLOCK_SIZE; i++) {
    const o = i * ENTRY_STRIDE;
    missingBlock[o + 0] = 0;
    missingBlock[o + 1] = primaryAdvanceFu;   // missing occupies one cell — layout stays right
    missingBlock[o + 2] = primaryEmHeightFu;
    missingBlock[o + 3] = FLAG_MISSING;
}
const byBlock = new Map();
for (const [cp] of cpEntries) {
    const b = cp >> BLOCK_SHIFT;
    if (!byBlock.has(b)) byBlock.set(b, []);
    byBlock.get(b).push(cp);
}
const blockIndex = new Uint32Array(BLOCK_INDEX_LENGTH);   // 0 = missing block
const built = [missingBlock];
const seen = new Map();
let mapped = 0;
const cpLookup = new Map(cpEntries.map(([cp, g, ax]) => [cp, { g, ax }]));
for (const [b, cps] of [...byBlock.entries()].sort((a, b2) => a[0] - b2[0])) {
    const block = new Uint32Array(BLOCK_SIZE * ENTRY_STRIDE);
    block.set(missingBlock);
    for (const cp of cps) {
        const { g, ax } = cpLookup.get(cp);
        const o = (cp & BLOCK_MASK) * ENTRY_STRIDE;
        let flags = 0;
        if (g === 0) flags = FLAG_BLANK;
        else if (chain.isBitmapSlot(g)) flags = FLAG_BITMAP;
        block[o + 0] = g;
        block[o + 1] = ax;                 // primary-font units; bitmap slots carry 2× cell
        block[o + 2] = primaryEmHeightFu;  // constant per-glyph height (em box)
        block[o + 3] = flags;
        mapped++;
    }
    const key = block.join(',');
    let slot = seen.get(key);
    if (slot === undefined) { slot = built.length; built.push(block); seen.set(key, slot); }
    blockIndex[b] = slot;
}
const blocks = new Uint32Array(built.length * BLOCK_SIZE * ENTRY_STRIDE);
for (let i = 0; i < built.length; i++) blocks.set(built[i], i * BLOCK_SIZE * ENTRY_STRIDE);
console.log(`[trie] ${mapped} codepoints mapped, ${built.length} unique blocks (${(blocks.byteLength / 1024).toFixed(1)}KB)`);

// ── step 6: write the four assets ────────────────────────────────────────────

mkdirSync(OUT_DIR, { recursive: true });

function header(magic, version, words) {
    const h = new Uint32Array(words.length + 3);
    h[0] = magic; h[1] = version; h[2] = (words.length + 3) * 4;
    h.set(words, 3);
    return h;
}
const writeU32 = (name, ...arrays) => {
    const total = arrays.reduce((n, a) => n + a.length, 0);
    const out = new Uint32Array(total);
    let off = 0;
    for (const a of arrays) { out.set(a, off); off += a.length; }
    writeFileSync(join(OUT_DIR, name), Buffer.from(out.buffer));
    console.log(`[write] ${name}: ${out.byteLength} bytes`);
};
const M = (s) => (s.charCodeAt(0)) | (s.charCodeAt(1) << 8) | (s.charCodeAt(2) << 16) | (s.charCodeAt(3) << 24);

// curves.bin — payload is the exact RGBA32Uint texture image (row-aligned).
writeU32('curves.bin',
    header(M('G3CV'), 1, [TEXTURE_WIDTH, curveHeight, curveCount, CURVE_TEXELS_PER_CURVE, 0]),
    curveTexels);

// glyphmap.bin — payload is the exact RGBA32Uint texture image (row-aligned).
writeU32('glyphmap.bin',
    header(M('G3GM'), 1, [TEXTURE_WIDTH, mapHeight, entryCount, 0, 0]),
    mapTexels);

// glyphs.bin — font table (64B each) + slot records (56B each) + name table.
{
    const FONT_REC = 16;             // u32 words: upem, asc, desc, lineGap + name[48] = 4 + 12 words
    const SLOT_REC = 14;             // u32 words (f32 lanes stored via bitcast below)
    const namesBlob = Buffer.concat(slotMeta.map((s) => Buffer.from(s.name, 'utf8')));
    const nameOffsets = new Uint32Array(slotCount);
    { let acc = 0; for (let i = 0; i < slotCount; i++) { nameOffsets[i] = acc; acc += Buffer.byteLength(slotMeta[i].name); } }

    const f32 = new Float32Array(1); const bits = (x) => { f32[0] = x; return new Uint32Array(f32.buffer)[0]; };

    const fontRecs = new Uint32Array(fontsMeta.length * FONT_REC);
    fontsMeta.forEach((f, i) => {
        const o = i * FONT_REC;
        fontRecs[o] = f.upem; fontRecs[o + 1] = f.ascender >>> 0; fontRecs[o + 2] = f.descender >>> 0; fontRecs[o + 3] = f.lineGap >>> 0;
        const nb = Buffer.from(f.name.slice(0, 47), 'utf8');
        for (let j = 0; j < nb.length; j++) fontRecs[o + 4 + (j >> 2)] |= nb[j] << ((j & 3) * 8);
    });

    const slotRecs = new Uint32Array(slotCount * SLOT_REC);
    slotMeta.forEach((s, i) => {
        const o = i * SLOT_REC;
        slotRecs[o + 0] = s.fontIdx;
        slotRecs[o + 1] = s.gid;
        slotRecs[o + 2] = s.flags;
        slotRecs[o + 3] = s.emojiCell;
        slotRecs[o + 4] = s.advanceFu >>> 0;
        slotRecs[o + 5] = s.asc >>> 0;
        slotRecs[o + 6] = s.desc >>> 0;
        slotRecs[o + 7] = s.curveStart;
        slotRecs[o + 8] = s.curveCount;
        slotRecs[o + 9] = bits(s.bbox[0]);
        slotRecs[o + 10] = bits(s.bbox[1]);
        slotRecs[o + 11] = bits(s.bbox[2]);
        slotRecs[o + 12] = bits(s.bbox[3]);
        slotRecs[o + 13] = 0;
    });

    const namesWords = new Uint32Array(Math.ceil(namesBlob.length / 4) + 1);
    namesWords[0] = namesBlob.length;
    Buffer.from(namesWords.buffer, 4).fill(0);
    namesBlob.copy(Buffer.from(namesWords.buffer, 4));

    writeU32('glyphs.bin',
        header(M('G3GL'), 1, [fontsMeta.length, slotCount, primaryUpem, primaryAdvanceFu, primaryEmHeightFu,
            FONT_REC * 4, SLOT_REC * 4, 0]),
        fontRecs, slotRecs, nameOffsets, namesWords);
}

// codepoints.bin — blockIndex + blocks.
writeU32('codepoints.bin',
    header(M('G3CP'), 1, [BLOCK_SHIFT, BLOCK_INDEX_LENGTH, built.length, ENTRY_STRIDE, mapped,
        primaryAdvanceFu, primaryEmHeightFu, primaryUpem]),
    blockIndex, blocks);

// ── step 7: export summary (for the report) ──────────────────────────────────

const a = cpLookup.get(0x41), g = cpLookup.get(0x67), at = cpLookup.get(0x40), hash = cpLookup.get(0x23), sp = cpLookup.get(0x20);
console.log('\n[summary] worked-example codepoints:');
for (const [label, e] of [["'A'", a], ["'g'", g], ["'@'", at], ["'#'", hash], ["' '", sp]]) {
    if (!e) { console.log(`  ${label}: NOT MAPPED`); continue; }
    const s = slotMeta[e.g];
    console.log(`  ${label} → slot ${e.g} (${s.name}), ax=${e.ax} fu, curves=[${s.curveStart}..${s.curveStart + s.curveCount}) flags=${s.flags}`);
}
console.log(`\n[done] assets in ${OUT_DIR}`);
