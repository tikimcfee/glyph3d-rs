// layout.wgsl — one invocation per VISIBLE SEGMENT: a serial walk over its
// bytes that resolves every leader through the resident atlas tables, in
// cluster mode, and emits the Derived slot (20 B) of every glyph it draws.
//
// This is a transcription of HyperLayout's `char_resolve.rs` (the ASCII fast
// table with its seq_lead guard, the lookahead probe that drops FE0F from the
// key and stops at a newline / FE0E / a non-leader, longest prefix first, the
// head taking two cells and its trailers zero) into one invocation. The CPU
// twin is `trie.rs::Trie::resolve`; the readback checks hold them bit-equal,
// and `--check` holds both to HyperLayout itself.
//
// Two lookup layouts, chosen at pipeline build by the LOOKUP_VARIANT constant
// (the host substitutes every at-sign placeholder below):
//   0  the trie as the file has it: blockIndex[cp >> 8], then the block entry
//      (two dependent loads), a head-candidacy bitmap, and a binary search of
//      the sequence section for a probe;
//   1  a direct 65,536-entry table for the BMP (one load), an open-addressing
//      hash for cp >= 0x10000, and a hash of every sequence key (verified
//      against the section on a hit).
// A resolved entry is PACKED: glyph in bits 0..16, advance in CELLS in bits
// 16..18, bit 18 = first member of some sequence, bit 19 = missing.
//
// x: with wrap_cols == 0 (the renderer's single-file form) HyperLayout sums
// the f32 advances in f64 and rounds once — bit-equal to f32(cells) * cell_adv
// here, because every advance is 0, 1 or 2 primary cells. With wrap_cols > 0
// it is the f32 running sum since the last wrap-unit boundary, in order.

struct Seg { byte_start: u32, byte_len: u32, row: u32, group: u32, lim: u32, slot_base: u32, col_seed: u32, cells_seed: u32, x_seed: f32 }
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, real_len: u32, max_len: u32, longest_line: u32, _pad: u32 }
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, group: u32 }
struct Params { segment_count: u32, wrap_cols: u32, chunk_shift: u32, chunk_mask: u32, cell_adv: f32, emit_adv: u32, seq_max: u32, color: u32 }

@group(0) @binding(0) var<storage, read> bytes0: array<u32>;       // the corpus, 1 B per source byte, chunk 0
@group(0) @binding(1) var<storage, read> bytes1: array<u32>;       // chunk 1 (a 4 B dummy when unused)
@group(0) @binding(2) var<storage, read> bytes2: array<u32>;
@group(0) @binding(3) var<storage, read> bytes3: array<u32>;
@group(0) @binding(4) var<storage, read> items: array<Item>;       // 32 B per item, resident (the footprint; a segment carries what the kernel needs)
@group(0) @binding(5) var<storage, read> segs: array<Seg>;         // 36 B per visible segment, per frame
@group(0) @binding(6) var<storage, read_write> slots: array<Slot>; // transient output
@group(0) @binding(7) var<storage, read_write> advs: array<f32>;   // per-slot advance, written only when params.emit_adv (the check)
@group(0) @binding(8) var<storage, read> tables: array<u32>;       // the lookup tables (trie.rs GpuTables)
@group(0) @binding(9) var<uniform> params: Params;

const LOOKUP_VARIANT: u32 = @VARIANT@u;
const T_ASCII: u32 = @T_ASCII@u;
const T_INDEX: u32 = @T_INDEX@u;
const T_BLOCKS: u32 = @T_BLOCKS@u;
const T_BITMAP: u32 = @T_BITMAP@u;
const T_BMP: u32 = @T_BMP@u;
const T_CP_HASH: u32 = @T_CP_HASH@u;
const CP_HASH_MASK: u32 = @CP_HASH_MASK@u;
const T_SEQ_HASH: u32 = @T_SEQ_HASH@u;
const SEQ_HASH_MASK: u32 = @SEQ_HASH_MASK@u;
const T_SEQ: u32 = @T_SEQ@u;
const SEQ_COUNT: u32 = @SEQ_COUNT@u;
const SEQ_STRIDE: u32 = @SEQ_STRIDE@u;

const PK_GLYPH_MASK: u32 = 0xFFFFu;
const PK_K_SHIFT: u32 = 16u;
const PK_SEQ_FIRST: u32 = 1u << 18u;
const PK_MISSING: u32 = 1u << 19u;
const PK_RESOLVED_MASK: u32 = 0x3FFFFu;   // glyph + cells
const HASH_EMPTY: u32 = 0xFFFFFFFFu;
const MAX_SEQ_KEY: u32 = 16u;

fn word_at(i: u32) -> u32 {
    let c = i >> params.chunk_shift;
    let w = (i & params.chunk_mask) >> 2u;
    var r = 0u;
    switch c {
        case 0u: { r = bytes0[w]; }
        case 1u: { r = bytes1[w]; }
        case 2u: { r = bytes2[w]; }
        default: { r = bytes3[w]; }
    }
    return r;
}

// Byte i of an item whose true end is `lim`; 0 past it, as the reference's
// bounds-checked decode reads 0 past its slice.
fn byte_at(i: u32, lim: u32) -> u32 {
    if (i >= lim) { return 0u; }
    return (word_at(i) >> ((i & 3u) * 8u)) & 0xFFu;
}

fn seq_len(b: u32) -> u32 {
    if ((b & 0x80u) == 0u) { return 1u; }
    if ((b & 0xE0u) == 0xC0u) { return 2u; }
    if ((b & 0xF0u) == 0xE0u) { return 3u; }
    if ((b & 0xF8u) == 0xF0u) { return 4u; }
    return 0u;
}

fn decode(i: u32, len: u32, lim: u32) -> u32 {
    let b0 = byte_at(i, lim);
    if (len == 1u) { return b0; }
    let b1 = byte_at(i + 1u, lim) & 0x3Fu;
    if (len == 2u) { return ((b0 & 0x1Fu) << 6u) | b1; }
    let b2 = byte_at(i + 2u, lim) & 0x3Fu;
    if (len == 3u) { return ((b0 & 0x0Fu) << 12u) | (b1 << 6u) | b2; }
    let b3 = byte_at(i + 3u, lim) & 0x3Fu;
    return ((b0 & 0x07u) << 18u) | (b1 << 12u) | (b2 << 6u) | b3;
}

fn is_static_zero(cp: u32) -> bool {
    return cp == 0x200Du || (cp >= 0xFE00u && cp <= 0xFE0Fu) || (cp >= 0xE0020u && cp <= 0xE007Fu);
}

// ---- variant 0: the trie as-is ---------------------------------------------

fn lookup_trie(cp: u32) -> u32 {
    var block = 0u;
    if (cp <= 0x10FFFFu) { block = tables[T_INDEX + (cp >> 8u)]; }
    return tables[T_BLOCKS + ((block << 8u) | (cp & 0xFFu))];
}

fn starts_seq_trie(cp: u32) -> bool {
    if (cp > 0x10FFFFu) { return false; }
    return ((tables[T_BITMAP + (cp >> 5u)] >> (cp & 31u)) & 1u) != 0u;
}

// Binary search over the sorted section: prefix-lexicographic, shorter first.
// Returns slot | 0x80000000 on a hit, 0 on a miss.
fn seq_lookup_trie(key: ptr<function, array<u32, 16>>, n: u32) -> u32 {
    var lo = 0u;
    var hi = SEQ_COUNT;
    while (lo < hi) {
        let mid = (lo + hi) >> 1u;
        if (cmp_key(key, n, mid) > 0) { lo = mid + 1u; } else { hi = mid; }
    }
    if (lo < SEQ_COUNT && cmp_key(key, n, lo) == 0) {
        return tables[T_SEQ + lo * SEQ_STRIDE] | 0x80000000u;
    }
    return 0u;
}

// -1 / 0 / 1 for key[0..n) against entry `e` of the section.
fn cmp_key(key: ptr<function, array<u32, 16>>, n: u32, e: u32) -> i32 {
    let o = T_SEQ + e * SEQ_STRIDE;
    let elen = tables[o + 1u];
    let m = min(n, elen);
    for (var k = 0u; k < m; k = k + 1u) {
        let a = (*key)[k];
        let b = tables[o + 2u + k];
        if (a < b) { return -1; }
        if (a > b) { return 1; }
    }
    if (n < elen) { return -1; }
    if (n > elen) { return 1; }
    return 0;
}

// ---- variant 1: direct BMP table + hashes -----------------------------------

fn hash_cp(cp: u32) -> u32 {
    var h = cp * 0x9E3779B1u;
    h = h ^ (h >> 15u);
    h = h * 0x2C1B3C6Du;
    h = h ^ (h >> 12u);
    return h;
}

fn lookup_direct(cp: u32) -> u32 {
    if (cp < 0x10000u) { return tables[T_BMP + cp]; }
    var idx = hash_cp(cp) & CP_HASH_MASK;
    loop {
        let k = tables[T_CP_HASH + idx * 2u];
        if (k == HASH_EMPTY) { return PK_MISSING | (1u << PK_K_SHIFT); }
        if (k == cp) { return tables[T_CP_HASH + idx * 2u + 1u]; }
        idx = (idx + 1u) & CP_HASH_MASK;
    }
    return 0u;   // unreachable: the table is never full
}

fn hash_seq_final(h0: u32) -> u32 {
    var h = h0 ^ (h0 >> 15u);
    h = h * 0x2C1B3C6Du;
    return h ^ (h >> 12u);
}

// `hashes[k]` is the running FNV fold over key[0..=k]; the final mix happens here.
fn seq_lookup_hash(key: ptr<function, array<u32, 16>>, hashes: ptr<function, array<u32, 16>>, n: u32) -> u32 {
    let h = hash_seq_final((*hashes)[n - 1u]);
    var idx = h & SEQ_HASH_MASK;
    loop {
        let si = tables[T_SEQ_HASH + idx * 2u + 1u];
        if (si == HASH_EMPTY) { return 0u; }
        if (tables[T_SEQ_HASH + idx * 2u] == h) {
            let o = T_SEQ + si * SEQ_STRIDE;
            if (tables[o + 1u] == n) {
                var same = true;
                for (var k = 0u; k < n; k = k + 1u) {
                    if (tables[o + 2u + k] != (*key)[k]) { same = false; break; }
                }
                if (same) { return tables[o] | 0x80000000u; }
            }
        }
        idx = (idx + 1u) & SEQ_HASH_MASK;
    }
    return 0u;   // unreachable: the table is never full
}

// ---- the resolver -----------------------------------------------------------

fn lookup(cp: u32) -> u32 {
    if (LOOKUP_VARIANT == 0u) { return lookup_trie(cp); }
    return lookup_direct(cp);
}

fn starts_seq(cp: u32, entry: u32) -> bool {
    if (LOOKUP_VARIANT == 0u) { return starts_seq_trie(cp); }
    return (entry & PK_SEQ_FIRST) != 0u;
}

// The slow path (char_resolve.rs resolve_leader), cluster mode. `end` bounds the
// probe at the segment's end (a cut before an ASCII byte, or the line's newline:
// either ends every candidate the whole-item walk would have built); `lim` is
// the item's true end for the byte reads. Returns glyph | cells << 16.
fn resolve_leader(i: u32, len: u32, end: u32, lim: u32, trailer_until: ptr<function, u32>) -> u32 {
    let cp = decode(i, len, lim);
    let entry = lookup(cp);
    if (i < *trailer_until || is_static_zero(cp)) { return 0u; }
    if (starts_seq(cp, entry)) {
        var key: array<u32, 16>;
        var key_end: array<u32, 16>;
        var hashes: array<u32, 16>;
        key[0] = cp;
        key_end[0] = i + len;
        hashes[0] = (0x811C9DC5u ^ cp) * 0x01000193u;
        var n = 1u;
        let max_key = min(params.seq_max, MAX_SEQ_KEY);
        var p = i + len;
        while (p < end && n < max_key) {
            let n2 = seq_len(byte_at(p, lim));
            if (n2 == 0u) { break; }
            let cp2 = decode(p, n2, lim);
            if (cp2 == 0x0Au || cp2 == 0xFE0Eu) { break; }
            if (cp2 != 0xFE0Fu) {
                key[n] = cp2;
                key_end[n] = p + n2;
                hashes[n] = (hashes[n - 1u] ^ cp2) * 0x01000193u;
                n = n + 1u;
            }
            p = p + n2;
        }
        // The longest table prefix of the key wins.
        for (var try_len = n; try_len >= 2u; try_len = try_len - 1u) {
            var hit = 0u;
            if (LOOKUP_VARIANT == 0u) { hit = seq_lookup_trie(&key, try_len); }
            else { hit = seq_lookup_hash(&key, &hashes, try_len); }
            if (hit != 0u) {
                *trailer_until = key_end[try_len - 1u];
                return (hit & 0xFFFFu) | (2u << PK_K_SHIFT);
            }
        }
    }
    return entry & PK_RESOLVED_MASK;
}

@compute @workgroup_size(64)
fn layout_segments(@builtin(global_invocation_id) gid: vec3<u32>) {
    let v = gid.x;
    if (v >= params.segment_count) { return; }
    let seg = segs[v];
    let row = seg.row;
    let lim = seg.lim;
    let _items = arrayLength(&items);
    var i = seg.byte_start;
    let end = seg.byte_start + seg.byte_len;
    var col = seg.col_seed;
    var cells = seg.cells_seed;
    var x = seg.x_seed;
    var k = seg.slot_base;
    var trailer_until = 0u;
    let wrap_cols = params.wrap_cols;
    while (i < end) {
        let b = byte_at(i, lim);
        var r = 0u;
        var len = 1u;
        if (b < 0x80u) {
            let e = tables[T_ASCII + b];
            // The fast answer, unless this seq_lead byte is followed by a non-ASCII byte.
            let next_non_ascii = (i + 1u < end) && (byte_at(i + 1u, lim) >= 0x80u);
            if (i >= trailer_until && !((e & PK_SEQ_FIRST) != 0u && next_non_ascii)) {
                r = e & PK_RESOLVED_MASK;
            } else {
                r = resolve_leader(i, 1u, end, lim, &trailer_until);
            }
        } else {
            len = seq_len(b);
            if (len == 0u) { i = i + 1u; continue; }   // a continuation or invalid lead: no record
            r = resolve_leader(i, len, end, lim, &trailer_until);
        }
        let glyph = r & PK_GLYPH_MASK;
        let kk = (r >> PK_K_SHIFT) & 3u;
        let adv = f32(kk) * params.cell_adv;
        var wrap = 0u;
        var xs = f32(cells) * params.cell_adv;
        if (wrap_cols > 0u) {
            if (col % wrap_cols == 0u) { x = 0.0; }
            wrap = col / wrap_cols;
            xs = x;
        }
        if (glyph != 0u) {
            slots[k] = Slot(xs, row, glyph | (wrap << 16u), params.color, seg.group);
            if (params.emit_adv != 0u) { advs[k] = adv; }
            k = k + 1u;
        }
        col = col + 1u;
        cells = cells + kk;
        x = x + adv;
        i = i + len;
    }
}
