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
 *   3. APPENDS the native colour-emoji slots (2026-09-10, step 4b below): every
 *      single-codepoint emoji the vendored Noto Color Emoji sheet can draw and
 *      no outline font covers gets a bitmap slot AFTER the web's 4,431, so no
 *      existing slot id moves and every text frame stays byte-equal. The web's
 *      own 897 bitmap slots keep their ids and get their `emojiCell` re-pointed
 *      at the sheet (NO_CELL where the font has no bitmap for them — the web's
 *      canvas indices meant nothing here). Slots 0..4430 of the glyph map are
 *      still the web's bytes; the tail is ours.
 *      Step 4c (2026-09-20, the sequence pass) appends one bitmap slot per
 *      SHEET SEQUENCE after those, in the sheet's sorted table order, so a
 *      resolved cluster head is an ordinary slot id and the shader needs
 *      nothing new. Same append-only rule: slot = base + table index, a pure
 *      function of (web bake, sheet).
 *
 * Usage:  node tools/export-atlas.mjs [--out <dir>]
 * Requires: node ≥ 18 (CompressionStream-free; we use zlib). The reference repo
 * is vendored under tools/vendor/ref (see REF_ROOT below); the web repo is not
 * read at all any more.
 */
import { readFileSync, writeFileSync, mkdirSync, readdirSync } from 'fs';
import { gunzipSync } from 'zlib';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const HERE = dirname(fileURLToPath(import.meta.url));
// VENDORED 2026-09-02. This was an absolute path into the web repo
// ('/Users/lugo/localdev/viz-web/glyph3d-js') — a live dependency on a tree that
// is no longer trunk, invisible until someone moved a directory. tools/vendor/ref
// now mirrors that repo's LAYOUT exactly, so every vendored file is byte-verbatim
// and this is the only line that changed. To refresh from the web repo, copy the
// same relative paths over; to see what is vendored, `find tools/vendor/ref -type f`.
//
// The gate that makes this safe: re-run this script and `cmp` its four outputs
// against assets/atlas/*.bin. Slot allocation is deterministic (dense counter in
// prime order) and step 3 already asserts the reproduced ids against the baked
// envelope's encodedIds before writing, so byte-identity is a real check, not a
// coincidence. Verified byte-identical against the pre-vendoring baseline.
const REF_ROOT = join(HERE, 'vendor', 'ref');
const SLUG_CORE_DIR = join(REF_ROOT, 'app/public/slug-core');
const OUT_DIR = process.argv.includes('--out')
    ? process.argv[process.argv.indexOf('--out') + 1]
    : join(HERE, '..', 'assets', 'atlas');

const TEXTURE_WIDTH = 1024;
const CURVE_TEXELS_PER_CURVE = 2;
// The committed emoji sheet is an INPUT (build.toml [artifact.atlas] inputs):
// a scratch rebuild reads the committed one, so regenerate it first.
const SHEET_PATH = join(HERE, '..', 'assets', 'atlas', 'emoji-sheet.bin');
const SHEET_MAGIC = 0x53453347; // 'G3ES'
const SHEET_HEADER_WORDS = 40, SHEET_CELL_STRIDE = 6, SHEET_CP_STRIDE = 2;
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
const rowsFor = (n) => Math.max(1, Math.ceil(n / TEXTURE_WIDTH));

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
// Direct module imports, NOT shaping/index.js: that barrel also re-exports
// SlugEncoder and LiveSlugAtlas, which import three. Nothing here needs them,
// and going direct keeps three out of this tool's dependency graph.
const FontChain = (await import(`${REF_ROOT}/packages/glyph3d-core/src/shaping/FontChain.js`)).default;
const MonospaceShapeCache = (await import(`${REF_ROOT}/packages/glyph3d-core/src/shaping/MonospaceShapeCache.js`)).default;
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

// ── step 4b: the native emoji slots, appended from the committed sheet ───────
//
// Policy, the web's own: a codepoint an outline font draws stays outline (the
// digits, #, *, ©, ®, ❤ … are emoji in the font and text here); a codepoint
// nothing draws that the sheet can draw becomes a bitmap slot. Existing web
// bitmap slots keep their ids. Appended slots are allocated in codepoint
// order after the web's last slot, so the allocation is a pure function of
// (web bake, sheet) and the rebuild-and-compare gate stays meaningful.
const sheetRaw = readFileSync(SHEET_PATH);
const sheet = new Uint32Array(sheetRaw.buffer.slice(sheetRaw.byteOffset, sheetRaw.byteOffset + (sheetRaw.byteLength & ~3)));
if (sheet[0] !== SHEET_MAGIC || sheet[1] !== 1) throw new Error(`${SHEET_PATH}: not a G3ES v1 sheet`);
const sheetCells = sheet[5], sheetCps = sheet[21];
const cellIndexOfGlyph = new Map();
for (let i = 0; i < sheetCells; i++) cellIndexOfGlyph.set(sheet[SHEET_HEADER_WORDS + i * SHEET_CELL_STRIDE], i);
const cpToCell = new Map();   // codepoint → sheet cell index, only where a bitmap exists
{
    const o = SHEET_HEADER_WORDS + sheetCells * SHEET_CELL_STRIDE;
    for (let i = 0; i < sheetCps; i++) {
        const cp = sheet[o + i * SHEET_CP_STRIDE], g = sheet[o + i * SHEET_CP_STRIDE + 1];
        if (cellIndexOfGlyph.has(g)) cpToCell.set(cp, cellIndexOfGlyph.get(g));
    }
}
const webSlotCount = slotCount;
const cpOfSlot = new Map();
for (const [cp, g] of cpEntries) if (g > 0) cpOfSlot.set(g, cp);
const cpIndex = new Map(cpEntries.map(([cp], i) => [cp, i]));
const primaryAdvanceForBitmap = 2 * primaryAdvanceFu;   // FormatMD: bitmap entries carry 2× cell

// (a) the web's bitmap slots: re-point at the sheet, or say there is no cell
let repointed = 0, noCell = 0;
for (let s = 0; s < webSlotCount; s++) {
    const m = slotMeta[s];
    if (m.fontIdx !== FONTIDX_BITMAP) continue;
    const cp = cpOfSlot.get(s);
    const cell = cpToCell.get(cp);
    m.emojiCell = cell === undefined ? NO_CELL : cell;
    m.name = cell === undefined ? `<emoji U+${cp.toString(16).toUpperCase().padStart(4, '0')}, no cell>`
                                : `<emoji U+${cp.toString(16).toUpperCase().padStart(4, '0')}>`;
    if (cell === undefined) noCell++; else repointed++;
}
// (b) append a slot per drawable codepoint nothing else covers
const appended = [];
for (const cp of [...cpToCell.keys()].sort((a, b) => a - b)) {
    const i = cpIndex.get(cp);
    if (i !== undefined && cpEntries[i][1] > 0) continue;      // outline or web bitmap: keep
    const slot = slotMeta.length;
    slotMeta.push({
        fontIdx: FONTIDX_BITMAP, gid: 0,
        name: `<emoji U+${cp.toString(16).toUpperCase().padStart(4, '0')}>`,
        advanceFu: 0, asc: 0, desc: 0, flags: SLOT_FLAG_BITMAP, emojiCell: cpToCell.get(cp),
        curveStart: 0, curveCount: 0, bbox: [0, 0, 0, 0],
    });
    if (i !== undefined) cpEntries[i] = [cp, slot, primaryAdvanceForBitmap];   // was BLANK in a mapped block
    else cpEntries.push([cp, slot, primaryAdvanceForBitmap]);
    appended.push(cp);
}
cpEntries.sort((a, b) => a[0] - b[0]);

// ── step 4c: the sequence slots — one bitmap slot per sheet sequence ────────
// The sequence pass's render half: every sequence the sheet draws gets a slot
// whose glyph-map texel is [0, 0, 1, cell], so a resolved cluster head is just
// a slot id in the instance stream and the shader needs nothing new. The
// sheet's sequence table is sorted by codepoint sequence, so the slot id of
// sequence i is SEQ_SLOT_BASE + i — a pure function of (web bake, sheet), the
// same append-only rule as 4b. gen_real_trie.py carries this section VERBATIM
// into engine-trie.bin and cross-checks the slot base against glyphs.bin
// (gen_real_trie.py:165-173): no mapping artifact exists to drift.
const seqCount = sheet[22], seqMax = sheet[23];
const seqStride = 2 + seqMax;
const seqTableOff = SHEET_HEADER_WORDS + sheetCells * SHEET_CELL_STRIDE + sheetCps * SHEET_CP_STRIDE;
const seqSlotBase = slotMeta.length;
const seqEntries = [];   // [codepoints array, slot] — the codepoints.bin v2 section's rows
for (let i = 0; i < seqCount; i++) {
    const o = seqTableOff + i * seqStride;
    const len = sheet[o], gid = sheet[o + 1];
    if (len < 2 || len > seqMax) throw new Error(`sequence ${i}: len ${len} out of range`);
    const cell = cellIndexOfGlyph.get(gid);
    if (cell === undefined) throw new Error(`sequence ${i} targets glyph ${gid}, which has no cell`);
    const cps = [];
    for (let k = 0; k < len; k++) cps.push(sheet[o + 2 + k]);
    seqEntries.push([cps, seqSlotBase + i]);
    slotMeta.push({
        fontIdx: FONTIDX_BITMAP, gid: 0,
        name: `<emoji seq ${cps.map((c) => c.toString(16).toUpperCase()).join('.')}>`,
        advanceFu: 0, asc: 0, desc: 0, flags: SLOT_FLAG_BITMAP, emojiCell: cell,
        curveStart: 0, curveCount: 0, bbox: [0, 0, 0, 0],
    });
}
console.log(`[emoji] sequences: ${seqCount} slots appended (${seqSlotBase} -> ${slotMeta.length})`);

const slotCountOut = slotMeta.length;
// (c) the glyph-map texels: the web's prefix verbatim, .w re-pointed for its
// bitmap slots, then one [0, 0, 1, cell] texel per appended slot.
const mapHeight = rowsFor(slotCountOut);
const mapTexelsOut = new Uint32Array(TEXTURE_WIDTH * mapHeight * 4);
mapTexelsOut.set(mapTexels.subarray(0, webSlotCount * 4));
for (let s = 0; s < slotCountOut; s++) {
    const m = slotMeta[s];
    if (m.fontIdx !== FONTIDX_BITMAP) continue;
    if (s >= webSlotCount) { mapTexelsOut[s * 4 + 2] = 1; }
    mapTexelsOut[s * 4 + 3] = m.emojiCell;
}
console.log(`[emoji] sheet: ${sheetCells} cells, ${cpToCell.size} single-codepoint; web bitmap slots ` +
    `${repointed} re-pointed + ${noCell} with no cell; ${appended.length} slots appended ` +
    `(${webSlotCount} -> ${seqSlotBase}); glyph map ${mapHeight} rows`);

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
        else if (slotMeta[g].fontIdx === FONTIDX_BITMAP) flags = FLAG_BITMAP;
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

// glyphmap.bin — the web's texels for slots 0..webSlotCount-1 (bitmap .w
// re-pointed), then the appended emoji slots (row-aligned).
writeU32('glyphmap.bin',
    header(M('G3GM'), 1, [TEXTURE_WIDTH, mapHeight, slotCountOut, 0, 0]),
    mapTexelsOut);

// glyphs.bin — font table (64B each) + slot records (56B each) + name table.
{
    const FONT_REC = 16;             // u32 words: upem, asc, desc, lineGap + name[48] = 4 + 12 words
    const SLOT_REC = 14;             // u32 words (f32 lanes stored via bitcast below)
    const namesBlob = Buffer.concat(slotMeta.map((s) => Buffer.from(s.name, 'utf8')));
    const nameOffsets = new Uint32Array(slotCountOut);
    { let acc = 0; for (let i = 0; i < slotCountOut; i++) { nameOffsets[i] = acc; acc += Buffer.byteLength(slotMeta[i].name); } }

    const f32 = new Float32Array(1); const bits = (x) => { f32[0] = x; return new Uint32Array(f32.buffer)[0]; };

    const fontRecs = new Uint32Array(fontsMeta.length * FONT_REC);
    fontsMeta.forEach((f, i) => {
        const o = i * FONT_REC;
        fontRecs[o] = f.upem; fontRecs[o + 1] = f.ascender >>> 0; fontRecs[o + 2] = f.descender >>> 0; fontRecs[o + 3] = f.lineGap >>> 0;
        const nb = Buffer.from(f.name.slice(0, 47), 'utf8');
        for (let j = 0; j < nb.length; j++) fontRecs[o + 4 + (j >> 2)] |= nb[j] << ((j & 3) * 8);
    });

    const slotRecs = new Uint32Array(slotCountOut * SLOT_REC);
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
        header(M('G3GL'), 1, [fontsMeta.length, slotCountOut, primaryUpem, primaryAdvanceFu, primaryEmHeightFu,
            FONT_REC * 4, SLOT_REC * 4, 0]),
        fontRecs, slotRecs, nameOffsets, namesWords);
}

// codepoints.bin — v2: the v1 trie PLUS the two sections the sequence pass
// reads. Header grows 44 -> 68 B (words 11..16 below); the blockIndex/blocks
// layout is unchanged. The two sections are pure u32 data, byte-identical to
// what gen_real_trie.py re-containers into engine-trie.bin (G3TR v2):
//   sequence section: sequenceCount x (2 + seqMax) words, [slot, len, cps..],
//                     sorted by the codepoint sequence (the sheet's own order,
//                     asserted below) so readers binary-search it.
//   class section:    cluster-classes.bin VERBATIM (its own 'G3CC' header
//                     included — self-describing, and byte-equality against the
//                     committed artifact is the provenance proof).
{
    const classRaw = readFileSync(join(HERE, '..', 'assets', 'atlas', 'cluster-classes.bin'));
    if (classRaw.length % 4) throw new Error('cluster-classes.bin is not a u32 array');
    const classWords = new Uint32Array(classRaw.buffer.slice(classRaw.byteOffset, classRaw.byteOffset + classRaw.byteLength));
    if (classWords[0] !== M('G3CC')) throw new Error('cluster-classes.bin: bad magic');

    // the sheet's sequence table is the sort source — verify it IS sorted
    const seqLt = (a, b) => {
        for (let k = 0; k < Math.min(a.length, b.length); k++) {
            if (a[k] !== b[k]) return a[k] < b[k];
        }
        return a.length < b.length;
    };
    for (let i = 1; i < seqEntries.length; i++) {
        if (!seqLt(seqEntries[i - 1][0], seqEntries[i][0])) {
            throw new Error(`sequence table not sorted at row ${i} — the sheet's order changed`);
        }
    }
    const seqStride2 = 2 + seqMax;
    const seqSection = new Uint32Array(seqEntries.length * seqStride2);
    seqEntries.forEach(([cps, slot], i) => {
        const o = i * seqStride2;
        seqSection[o] = slot;
        seqSection[o + 1] = cps.length;
        cps.forEach((cp, k) => { seqSection[o + 2 + k] = cp; });
    });
    const seqOff = 17 + blockIndex.length + blocks.length;
    const classOff = seqOff + seqSection.length;
    const hdr2 = new Uint32Array(17);
    hdr2[0] = M('G3CP'); hdr2[1] = 2; hdr2[2] = 68;
    hdr2.set([BLOCK_SHIFT, BLOCK_INDEX_LENGTH, built.length, ENTRY_STRIDE, mapped,
        primaryAdvanceFu, primaryEmHeightFu, primaryUpem], 3);
    hdr2[11] = primaryAdvanceForBitmap;   // the cluster head's advance, fu
    hdr2[12] = seqEntries.length;
    hdr2[13] = seqMax;
    hdr2[14] = seqOff;
    hdr2[15] = classOff;
    hdr2[16] = classWords.length;
    writeU32('codepoints.bin', hdr2, blockIndex, blocks, seqSection, classWords);
}

// ── step 7: export summary (for the report) ──────────────────────────────────

const a = cpLookup.get(0x41), g = cpLookup.get(0x67), at = cpLookup.get(0x40), hash = cpLookup.get(0x23), sp = cpLookup.get(0x20);
const rat = cpLookup.get(0x1F400), rocket = cpLookup.get(0x1F680), flagA = cpLookup.get(0x1F1E6);
console.log('\n[summary] worked-example codepoints:');
for (const [label, e] of [["'A'", a], ["'g'", g], ["'@'", at], ["'#'", hash], ["' '", sp], ["'🐀'", rat], ["'🚀'", rocket], ["RI-A", flagA]]) {
    if (!e) { console.log(`  ${label}: NOT MAPPED`); continue; }
    const s = slotMeta[e.g];
    console.log(`  ${label} → slot ${e.g} (${s.name}), ax=${e.ax} fu, curves=[${s.curveStart}..${s.curveStart + s.curveCount}) flags=${s.flags}` +
        (s.flags & SLOT_FLAG_BITMAP ? ` cell=${s.emojiCell === NO_CELL ? 'none' : s.emojiCell}` : ''));
}
console.log(`\n[done] assets in ${OUT_DIR}`);
