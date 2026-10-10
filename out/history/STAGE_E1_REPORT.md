> **History.** Moved to `out/history/` on 2026-10-10: a dated record, not current state. What is true now: `README.md`, root `AGENTS.md`, `out/MAINTENANCE-NOTES-2026-10-08.md`.

# Stage E1 — real atlas trie in the Mojo engine

**Goal**: replace the toy fixture trie (`GLYPH_ID = (cp % 4093) + 1`, toy
metrics) with the REAL codepoint→slot mapping from the app's glyph atlas, so
engine records key the renderer's glyph-map texture directly.

**Result**: done and verified. `assets/atlas/engine-trie.bin` (a new `G3TR`
blob) is generated from the Stage B export, the engine loads it through the
existing FFI entry point, and on every test input the engine's records are
**bit-exact** against an independent CPU layout that reads the same atlas —
GLYPH_ID/ROW/COL as integers, X/Y/Z/ADVANCE/HEIGHT as f32 bit patterns, no
tolerance.

## Answers to the brief's key questions (from source)

1. **On-disk trie format the engine loader expects.** The Mojo engine's
   in-memory `Trie` (engine-local/glyph_pipeline.mojo:72) is split by carrier:
   `blocks_m: f32 × 2` (ADVANCE, HEIGHT — genuine measures) and
   `blocks_c: u32 × 2` (GLYPH_ID — identity; FLAGS — bitfield), plus a
   `block_index: u32[4352]`, two-load lookup `blocks[(blockIndex[cp>>8]<<8 | cp&0xFF)]`.
   Until E1 the only on-disk form was the `G3DF` v3 pipe fixture
   (engine-local/fixture_io.mojo): blocks serialized entry-major as **f64
   VALUES** `[GLYPH_ID, ADVANCE, HEIGHT, FLAGS]` — a representation-independent
   carrier the loader narrows (f32 for measures, u32 for identity/bitfield).
   The fixture values were toys: `metricsFor(cp)` in the reference repo's
   engine/fixtures/gen.mjs (`(cp%4093)+1`, awkward-mantissa f32 advances).

2. **How the web derives trie advance/height.** The runtime trie builder is
   `packages/glyph3d-core/src/compute/liveTrie.js`: `advance = (ax/upem) ×
   worldScale × charSize.height` per codepoint (ax = raw HarfBuzz font units
   from the MonospaceShapeCache), `height = charSize.height × worldScale`
   (constant per glyph), with the same formula for the missing block. The
   **unit of measure is world space; the rounding happens once per store**
   into the bitcast-f32 lanes (GlyphTrie.js `trieFbits`). NOTE — the web's
   denominator is upem (2048), making its advance ~13% wider than the
   geometric cell ratio (`advanceFu/emHeightFu`); liveTrie.js's own header
   calls this deliberate. The native port anchors on the **geometric ratio**
   per assets/atlas/FORMAT.md ("World-space conversion"), which is what the
   Stage C renderer stages; see below.

3. **What glyph_id the renderer expects.** The FontChain global slot (Stage B
   numbering, FORMAT.md "Glyph identity model") — the same id that indexes
   `glyphmap.bin`. The native renderer's `atlas.rs` maps codepoints through
   `codepoints.bin` to exactly these slots; the WGSL shader does
   `textureLoad(glyphmap, gid)` with no remap. So the engine trie's GLYPH_ID
   lane must carry the slot verbatim — which codepoints.bin already holds.

## What was built

### 1. `tools/gen-real-trie.mjs` → `assets/atlas/engine-trie.bin` (`G3TR`)

Reads `codepoints.bin` (block index + integer-font-unit blocks) and
`glyphs.bin` (header cross-check: upem / cell advance / em height must agree),
rewrites the measure lanes to f32 world units, copies identity/flags/blockIndex
verbatim. Contract discipline kept: counts/identity native u32, measures f32
rounded once (`fround(advanceFu × cellHeightWorld / emHeightFu)`, computed in
f64; `cellHeightWorld = 1.0` = text.rs `CELL_HEIGHT_WORLD`). Self-verifies by
walking the written blob: 'A' → slot 34 / 0.5297414…; ' ' → slot 1; '🐀' →
bitmap slot 3839, double advance 1.0594828… (exactly 2× the cell — f32 doubling
is exact); '🚀' → missing block (slot 0, MISSING); HEIGHT == 1.0 on every entry.
129 KiB out (4352-word index + 28 deduplicated blocks).

Format spec (also added to assets/atlas/FORMAT.md): magic `G3TR`, version 1,
44-byte header (blockShift, blockIndexLength, blockCount, entryStride,
mappedCount, primaryUpem, primaryEmHeightFu, cellHeightWorld-as-f32-bits),
then blockIndex, then blocks `[GLYPH_ID u32][ADVANCE f32][HEIGHT f32][FLAGS u32]`
— the web trie's container (GlyphTrie.js ENTRY_STRIDE), which the loader splits
by carrier.

### 2. engine-local changes (minimal, tagged NATIVE-PORT)

The existing loader could not read a real trie — it only parsed pipe fixtures,
and `EngineState` owned the trie through a discarded `PipeFixture`. Patched:

- `fixture_io.mojo`: `TRIE_MAGIC` + `load_trie_blob()` (G3TR → `Trie`, doing
  the same carrier split the fixture loader does per-entry) + `load_trie_auto()`
  (dispatch on magic: G3DF → fixture path unchanged, G3TR → blob path).
- `ffi.mojo`: `glyph_engine_load_trie_file` now calls `load_trie_auto`;
  `EngineState` owns the `Trie` directly instead of a whole fixture. One FFI
  entry point serves both trie sources; the C ABI is unchanged.

**Conformance: all suites still pass against the fixture tries** (G3DF path
untouched): `ffi_selftest` (bit-exact through the C ABI + 1000-load stress
loop), `conformance` (bit-exact), `conformance_scan` (tiered), `conformance_bake`
(bit-exact), `conformance_elide`, `conformance_record`, `conformance_resume`,
`conformance_matrix` (48 combos), `conformance_gaps`, `ordinal_invariant`, and
`conformance_real` over 12 real source files (104,480 B, serial vs scan agree).

### 3. Rust wiring

- `atlas.rs`: split `TrieTable` (metrics + block index + blocks + `lookup`)
  out of `Atlas` — loadable **without a GPU context** so the cross-check stays
  CPU-only. `Atlas` owns one; `Atlas::lookup` delegates. Same 'A'→(34, 1229)
  sanity assert, now in `TrieTable::load`.
- `engine.rs`: doc update only (the TODO(stage-e) it carried is done).
- `text.rs`: two Stage E1 paths —
  - `reference_layout()`: an independent CPU re-implementation of the engine's
    decode+fold (byte-level UTF-8 leader logic identical to
    `decode_and_resolve`, f64 `line_adv` chain, same f32 narrowing points),
    reading the atlas trie directly.
  - `diff_records()`: bit-exact diff (counts equal, measures equal **as bits**).
  - `stage_records()`: engine records → `StagedText` for the existing Slug
    renderer (slot-0 records dropped — they carry no ink; their advance is
    already baked into the survivors' X).
- `main.rs`: `--engine-file` now defaults to `assets/atlas/engine-trie.bin`
  (`--engine-trie` overrides, fixtures still accepted); new `--engine-check`
  (cross-validation, exit 1 on divergence) and `--engine-render` (records →
  Slug renderer, works with `--screenshot`).

### 4. Cross-validation (mandatory) — results

Crafted input `out/e1-check.txt` (154 B): ASCII prose, newline, leading tab,
fallback glyph Ω (U+03A9 → slot 860, a fallback-font slot), bitmap emoji 🐀
(U+1F400 → slot 3839, double advance), missing codepoint 🚀 (U+1F680 → slot 0,
MISSING), 2-byte UTF-8 (ï, é):

```
$ glyph3d-native --engine-check out/e1-check.txt
engine-check PASS: 154 B → 145 records bit-exact vs the CPU reference
```

145 records = 154 bytes − 9 continuation bytes (every UTF-8 leader emits one
record, newline/tab/missing included — the engine's `compact` emits per
leader). Two real source files as the fuzz tier:

```
engine-check PASS: native/src/engine.rs (7,633 B) — 7,627 records bit-exact
engine-check PASS: native/src/main.rs  (13,799 B) — 13,781 records bit-exact
```

Negative control (the checker must be able to fail): `--engine-check …
--engine-trie engine-local/fixtures/ascii-basic.pipe.bin` FAILS 145/145 with
exit 1, printing per-record engine-vs-expected diffs.

Rendered proof (`--engine-render … --screenshot`):
- `out/e1-engine-check-full.png` — the crafted string: upright, even monospace
  spacing, no holes in the ASCII prose; Ω inked from its fallback slot; 🐀
  leaves a clean double-width gap (bitmap discard — no emoji atlas exported);
  🚀 leaves a single-cell gap (missing, advance kept); tab = one cell.
- `out/e1-engine-file.png` / `out/e1-engine-file-zoom.png` — atlas.rs (8,660
  instances) laid out by the engine: dense, regular, legible; visually
  identical layout to the CPU-staged `out/e1-cpu-file-zoom.png` (same file,
  same region, zoom 5 — engine render is single-color; the CPU path adds
  syntax colors).

### Convention reconciliation (engine vs text.rs `stage_file`)

The engine's fold and the production CPU staging differ by design; E1 does NOT
unify them, it documents and routes around:

| concern | engine fold (and `reference_layout`) | text.rs `stage_file` |
|---|---|---|
| X accumulation | f64 running sum of per-glyph f32 advances (oracle discipline) | `col × cell_w` (single multiply) |
| COL unit | leader glyphs per line (newline rides at col=len) | grid cells (tab stops, double-width) |
| tab | ordinary codepoint: one cell via the (missing) trie entry | 4-cell tab stops |
| record stream | one per leader byte (blanks/missing/newline included) | blanks/missing/bitmap dropped |

For pure-ASCII, tab-free text the two agree to a few ULP in X and exactly in
ROW; `reference_layout` exists precisely so the cross-check compares LIKE
conventions, which is what makes bit-exactness achievable. If the app later
wants the engine's layout to drive the production scene, `stage_records` is
already that path (it renders); adopting tab stops in the engine would be an
oracle change (the web oracle has none), not a port fix.

Known engine edge (pre-existing, not E1's): a malformed 4-byte UTF-8 lead can
decode a codepoint above 0x10FFFF, past `block_index`'s 4352 entries — the
fixtures never produce one. `reference_layout` asserts the bound instead of
silently diverging.

## File diffs

- `tools/gen-real-trie.mjs` — NEW (generator + self-verify).
- `assets/atlas/engine-trie.bin` — NEW (generated, 129 KiB).
- `assets/atlas/FORMAT.md` — `G3TR` section added before "Scope limits".
- `engine-local/fixture_io.mojo` — `TRIE_MAGIC`, `load_trie_blob`,
  `load_trie_auto` (NATIVE-PORT tags); `BLOCK_SHIFT` import.
- `engine-local/ffi.mojo` — `EngineState` owns `Trie` directly;
  `glyph_engine_load_trie_file` dispatches via `load_trie_auto`; the
  TODO(stage-e) is resolved (NATIVE-PORT tags).
- `engine-local/README-FFI.md` — trie-source dispatch documented.
- `native/src/atlas.rs` — `TrieTable` split out (GPU-free); `Atlas` delegates.
- `native/src/text.rs` — `reference_layout`, `diff_records`, `stage_records`.
- `native/src/engine.rs` — doc comment (FFI unchanged).
- `native/src/main.rs` — default trie → engine-trie.bin; `--engine-check`,
  `--engine-render`; shared `engine_layout`/`engine_item_params` helpers.
- `native/libglyph_engine.dylib` — rebuilt (Mojo, `--fp-mode contract=off`).

## What E2 (repo-scale loading) inherits

- A real, self-verifying trie artifact + its generator; regenerating after any
  atlas rebake is `node tools/gen-real-trie.mjs` (it aborts on metric drift
  between codepoints.bin and glyphs.bin).
- An FFI that already streams whole files correctly through the real trie;
  E2's work is scale, not correctness: `--engine-file`/`--engine-check` take
  any path, and the engine's own streaming machinery (`run_streaming`, the
  scratch-pool record path, mid-item resume seeds) is conformance-proven in
  engine-local and needs only a multi-item FFI surface (load N items per call,
  per-item origins) — the C ABI's single-item `glyph_engine_load_item` is the
  one shim to widen.
- `--engine-check` as a standing regression gate: any future trie/atlas/engine
  change that shifts a bit fails it loudly (negative control proven).
- The render path (`--engine-render`) already draws engine output through the
  production Slug pipeline, so E2 screenshots are one flag away at any size.
- Open items E2 should pick up: flags are not in the wire record (the renderer
  re-derives blank/bitmap from the glyphmap, so nothing is lost today; a
  live-growth miss-reporting channel would want them, or the miss list);
  emoji bitmap pixels remain unexported (bitmap slots discard by design).
