# Engine FFI (Stage D) — building the Mojo shared library

`ffi.mojo` wraps the glyph pipeline in a C ABI (scalars + one opaque handle).
Build the dylib from the workspace root:

```sh
export PATH="/opt/homebrew/bin:$PATH"
pixi run mojo build --fp-mode contract=off -I engine \
    engine/ffi.mojo -o native/libglyph_engine.dylib --emit shared-lib
install_name_tool -id @rpath/libglyph_engine.dylib native/libglyph_engine.dylib
```

- `--fp-mode contract=off` is load-bearing (FMA contraction would break the
  bit-exact float discipline; see the header of engine/check.sh, which
  spells out the cross-statement FMA contraction this prevents). It is
  ENFORCED as of 2026-09-02: `glyph_engine_fp_probe` computes `a*b + c` from
  caller-supplied operands (so it cannot be constant-folded) and Rust's
  `Engine::new()` panics unless the dylib returns the UNFUSED result
  (0x35000000; fused is 0x35000001, one ulp away). Before this, nothing in
  the tree could detect a dylib built without the flag — the Mojo suites pass
  either way, and `--engine-check` runs with origin (0,0,0) which makes its
  only fusable multiply-add FMA-invariant. Prefer `pixi run build-engine`.
- The dylib links `@rpath/libKGENCompilerRTShared.dylib`; `native/build.rs`
  adds both rpaths (`native/` and `.pixi/envs/default/lib`) to the Rust binary.
- `glyph_engine_new` calls `std.runtime.initialize_runtime()` — MANDATORY
  when the host process is not Mojo, else the first TaskGroup dispatch
  segfaults on a null async runtime.

## C ABI

```c
void  *glyph_engine_new(void);
void   glyph_engine_free(void *h);
int32  glyph_engine_load_trie_file(void *h, const uint8_t *path, size_t len);

// ONE MARSHALLING FORMAT: no positional params cross the seam any more. Every
// item-loading entry takes descriptor blocks (layout below). The per-item
// entry was RENAMED on purpose — a dylib predating that fails to LINK rather
// than being miscalled (ffi.mojo's ONE MARSHALLING FORMAT note has the
// positional-shift measurement that forced this).
int32  glyph_engine_load_item_desc(void *h, const uint8_t *bytes, size_t len,
           const uint8_t *desc);
// Stage E2 (NATIVE-PORT): whole-corpus batch load.
int32  glyph_engine_load_items(void *h, const uint8_t *blob, size_t blob_len,
           const uint8_t *descs, size_t item_count, uint64_t *counts_out);
// The direct path (the renderer's DEFAULT strategy): fold, then write render
// instances into the CALLER's arena — no wire record is materialized anywhere.
int32  glyph_engine_load_items_direct(void *h, const uint8_t *blob, size_t blob_len,
           const uint8_t *descs, size_t item_count,
           uint32_t *inst, size_t inst_cap,   // cap in INSTANCES, not bytes
           const uint32_t **paint_ptrs, const uint64_t *paint_lens,
           const uint32_t *flat_colors, const uint32_t *group_ids,
           uint32_t *place_out);

uint64 glyph_engine_slot_count(void *h);
uint64 glyph_engine_copy_slots(void *h, uint32_t *out, size_t out_len);
// Per-stage nanoseconds for the last load; returns how many lanes it wrote.
size_t glyph_engine_stage_ns(void *h, uint64_t *out, size_t cap);
// (instance u32 lanes << 32) | placement u32 lanes.
uint64 glyph_engine_instance_shape(void);
// a*b + c from caller-supplied operands: the fp-contract guard (0x35000000
// unfused, 0x35000001 fused — Engine::new panics unless unfused).
uint32 glyph_engine_fp_probe(float a, float b, float c);
```

`glyph_engine_load_items` runs N items in ONE pipeline call over a
concatenated blob. `descs` is one **136-byte block per item**, serialized
explicitly (no repr(C) guessing — `engine.rs::write_item_desc` is the
writer): ten f64 params at 0..80 (origin xyz, line_height, z_step,
page_gap_x, band_stride_y, depth_per_band, depth_per_col, page_line_height),
eight i32 params at 80..112 (wrap_width, has_page, page_rows, page_cols,
scroll_rows, pages_wide, wrap_mode, cluster_mode), u32 ABI_SHAPE at 112
(computed from the block's size and i32 count, so a caller built against a
different layout is refused, never misparsed), u64 byte_start at 120, u64
byte_count at 128. The block was 128 B until 2026-09-20: cluster_mode could
not take pad, so the block grew and the shape word moved with it.
Items must be contiguous and ascending by byte_start. `counts_out` receives
the exact per-item record counts (one record per leader byte, counted from
the pipeline's flag lanes). Keep each item under 2^24 bytes (the per-item
ordinal wall — the 10 MiB walker cap in repo.rs enforces it).
`--repo-verify` diffs this path against per-file `load_item_desc` bit-exact
over the whole corpus; measured, the per-file loop is FASTER at ~97 MB
(whole-corpus lane arrays hit memory pressure) — see out/STAGE_E2_REPORT.md.

Status: 0 ok, 2 no trie loaded, 3 engine raised, 4 empty input, 9 ABI shape
mismatch (the dylib was built from different source), 10 the direct path's
output arena is too small, 11 a per-item paint array is shorter than the
item's record count, 12 item ranges do not tile the blob.
Record = 32 B: `[f32 X Y Z ADVANCE HEIGHT][u32 GLYPH_ID ROW COL]` —
f32 lanes cross as raw bits (copied through a u32 view), so no float
reformatting can happen at the seam.

`glyph_engine_load_trie_file` dispatches on magic (Stage E1): a `G3TR` blob
(`assets/atlas/engine-trie.bin` — the app atlas's REAL codepoint→slot mapping,
generated by `tools/gen_real_trie.py`; spec in `assets/atlas/FORMAT.md`) or a
legacy `G3DF` `.pipe.bin` conformance fixture. The Rust default is the G3TR
blob; pass `--engine-trie <fixture>` for the toy fixture trie.

## Verification

```sh
# Mojo-side: fixtures through the FFI, bit-exact vs the oracle + stress loop
pixi run mojo build --fp-mode contract=off -I engine \
    engine/ffi_selftest.mojo -o out/ffi_selftest
./out/ffi_selftest engine/fixtures/*.pipe.bin

# Rust-side
cargo build --release --manifest-path native/Cargo.toml
./target/release/glyph3d-native --engine-file <text-file> [--engine-loop N]
```

Port note (MOJO-1.1-PORT): Mojo 1.1.0.dev2026083005 made `std.runtime.asyncrt`
private; the engine copies of glyph_pipeline/glyph_scan (+2 benches)
import TaskGroup from `std.runtime._asyncrt` instead. One line each, tagged.
