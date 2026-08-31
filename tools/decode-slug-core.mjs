#!/usr/bin/env node
/**
 * decode-slug-core.mjs — statically decode the web app's prebaked slug-core asset.
 *
 * Reads the gzipped 'SLGC' envelope produced by tools/bake-slug-core.mjs in the
 * reference repo (see packages/glyph3d-core/src/shaping/slugCoreCache.js for the
 * envelope layout) and prints a structural summary. Does NOT write anything into
 * the reference repo.
 *
 * Envelope (Uint32 array, little-endian, gzipped):
 *   [0] MAGIC 'SLGC' = 0x43474C53
 *   [1] envelopeVersion (=1)
 *   [2] payloadFormat  (SLUG_BUFFER_FORMAT, =2)
 *   [3] curveCount
 *   [4] entryCount (= maxGlyphId+1)
 *   [5] encodedLen     — u32 count of encodedIds
 *   [6] curveLen       — u32 count of the row-aligned curve texel array
 *   [7] mapLen         — u32 count of the row-aligned glyph-map array
 *   [ encodedIds … ][ curve … ][ map … ]
 */
import { readFileSync } from 'fs';
import { gunzipSync } from 'zlib';

const SRC = '/Users/lugo/localdev/viz-web/glyph3d-js/app/public/slug-core/slug-core.1tstke3lync.bin';
const TEXTURE_WIDTH = 1024;
const MAGIC = 0x43474c53;

const gz = readFileSync(SRC);
if (gz[0] !== 0x1f || gz[1] !== 0x8b) throw new Error('not gzip');
const raw = gunzipSync(gz);
const u32 = new Uint32Array(raw.buffer, raw.byteOffset, raw.byteLength / 4);

if (u32[0] !== MAGIC) throw new Error('bad magic');
const [, envVer, fmt, curveCount, entryCount, encodedLen, curveLen, mapLen] = u32;
console.log(`envelope v${envVer}, payload format v${fmt}`);
console.log(`curveCount=${curveCount} entryCount=${entryCount} (maxGlyphId=${entryCount - 1})`);
console.log(`encodedLen=${encodedLen} curveLen=${curveLen} mapLen=${mapLen}`);
console.log(`raw bytes=${raw.byteLength} gz bytes=${gz.byteLength}`);

let o = 8;
const encodedIds = u32.slice(o, o + encodedLen); o += encodedLen;
const curve = u32.slice(o, o + curveLen); o += curveLen;
const map = u32.slice(o, o + mapLen);

// Structural checks mirroring SlugBuffer.deserialize's _validateDescriptor.
const curveTexels = curveCount * 2;
const curveRows = Math.max(1, Math.ceil(curveTexels / TEXTURE_WIDTH));
const mapRows = Math.max(1, Math.ceil(entryCount / TEXTURE_WIDTH));
console.log(`curve texture: ${TEXTURE_WIDTH}x${curveRows} (${TEXTURE_WIDTH * curveRows * 4} u32, want==got: ${TEXTURE_WIDTH * curveRows * 4 === curveLen})`);
console.log(`map texture:   ${TEXTURE_WIDTH}x${mapRows} (${TEXTURE_WIDTH * mapRows * 4} u32, want==got: ${TEXTURE_WIDTH * mapRows * 4 === mapLen})`);

// Map sanity: modes, bitmap slots, curve-range stats.
let bitmap = 0, outline = 0, empty = 0, maxEnd = 0, nonMono = 0, prevStart = -1;
for (let g = 1; g < entryCount; g++) {
    const s = map[g * 4], c = map[g * 4 + 1], mode = map[g * 4 + 2];
    if (mode === 1) bitmap++;
    else if (c > 0) {
        outline++;
        if (s < prevStart) nonMono++;
        prevStart = s;
        maxEnd = Math.max(maxEnd, s + c);
    } else empty++;
}
console.log(`slots: outline-with-curves=${outline} empty=${empty} bitmap(emoji)=${bitmap}`);
console.log(`max curve end=${maxEnd} (curveCount=${curveCount}), non-monotonic curveStart entries=${nonMono}`);
console.log(`encodedIds: min=${encodedIds[0]} max=${encodedIds[encodedIds.length - 1]} count=${encodedIds.length}`);
