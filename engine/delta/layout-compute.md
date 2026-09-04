# Delta: how glyph positions are computed

Web = `/Users/lugo/localdev/viz-web/glyph3d-js` (read-only reference).
Native = `/Users/lugo/localdev/viz-native/glyph3d-native`.

Scope: the fold — parameters, displacement, where it runs, what it costs, what it
computes but does not hand back, and the f64 question.

## Summary

Both sides run **the same fold**, from the same generated contract
(`schema/glyph-identity.json` → `packages/glyph3d-core/src/compute/glyphContract.js`
and `engine/glyph_schema.mojo`). The algorithm is not the delta. The delta is
**plumbing and lifetime**:

- Native's fold is *better implemented* and *worse connected*. The Mojo CPU port is
  the only one of the four layers that reproduces the oracle's `line_adv` carrier
  exactly, runs across every core off any frame budget, and measures 2.7× the JS
  oracle. It is reached through a C ABI that returns **one thing**: 32 B per rendered
  glyph. Everything else the pipeline computes — the per-item box, the fold scalars,
  the batch box — is computed and dropped on the floor.
- The consequence is not a missing feature, it is a **duplicated one**:
  `native/src/layout.rs:526` re-walks every record host-side to recompute a 2-D
  version of the 3-D box `engine/glyph_pipeline.mojo` already reduced in registers.
- Several native params are passed 0 at their call site with the algorithm live
  behind them. **Three of the five are passed 0 on the web too** — they are contract
  surface neither side has wired, not port artifacts. Two are native-only gaps.
- The web's own headline layout number is no more honest than native's. Native's
  `backend_dur` includes a 32 B/glyph readback and a host compaction
  (`native/src/repo.rs:281-284`, said out loud). The web's `loadStats.kernelMs`
  wraps nine `renderer.compute()` *enqueues* (`GlyphPipelineArena.js:527-547`) and
  measures submit cost, not GPU work. **Neither number measures the fold.**

Bucket counts: **1 (better) 6 · 2 (platform) 3 · 3 (missing) 8 · 4 (worse) 6**.

## THE UNWIRED PARAMETERS — lead finding

Every native call site that builds an `ItemParams`, and what it leaves at zero.

| param | native value | site | algorithm live? | corpus-gated? | web does what |
|---|---|---|---|---|---|
| `scroll_rows` | **0** | `repo.rs:239`, `main.rs:93` (default) | yes — `fold.rs:490,496`, `glyph_pipeline.mojo` paginate | yes | **wires it** — `CodeGrid._windowScroll()` (`CodeGrid.js:1485`) feeds `scrollRows` on every stage |
| `depth_per_band` | **0** | `repo.rs:250` | yes — `fold.rs:517` | yes | **wires it**, but only for `axis:'z'` (`CodeGrid.js:1464`) |
| `depth_per_col` | **0** | `repo.rs:251` | yes — `fold.rs:518` | yes | **also 0** — no web call site sets `depthPerColumn`; only `glyphPipelineKernels.js:1331` reads it |
| `page_cols` | **0** | `repo.rs:238` | yes — `fold.rs:489,504` and it is the *fold unit* when `wrap_width==0` (`glyph_pipeline.mojo:634-638`) | yes | **also 0** — `CodeGrid._pageParams` (`CodeGrid.js:1457-1476`) never sets `pageCols` |
| `page_line_height` | **0** | `repo.rs:252` | **no — provably inert** | — | no web counterpart at all; not in `ITEM_PARAMS` |
| `origin_x/y/z` | **0** | `repo.rs:253` (`..Default::default()`) | yes | yes | web sets `origin.y` per item (`CodeGrid.js:1361`, the filename row offset) |
| `wrap_width` | **0** | `main.rs:92-96` only | yes | yes | web default 200 (`workers/builders/index.js:40`); repo path wires 100 |
| `z_step` | **0** | `main.rs:92-96` only | yes | yes | wired (`CodeGrid.js:1365`) |
| `has_page` | **false** | `main.rs:92-96` only | yes | yes | web has no `HAS_PAGE` lane — `pageRows > 0` is the only gate |

Two call sites, two different stories:

- `repo::file_item_params` (`repo.rs:223-255`) is the live field path. As of `38b677f`
  it wires `z_step`; `depth_per_band`/`depth_per_col` are zeroed **with a stated
  reason** (an xy fan's page planes are coplanar, so the depth terms are a different
  feature) — that reason is correct and matches the web, which also only lifts
  `depthPerBand` under `axis:'z'`. `page_cols` and `scroll_rows` have no such reason
  written; they are simply unwired.
- `main::engine_item_params` (`main.rs:92-96`) sets **only** `line_height`. So
  `--engine-render` and `--engine-check` — the byte-equal render A/B and the FFI
  check — exercise a fold with no wrap, no pages, no staircase and origin (0,0,0).
  This is already flagged for a different reason at `engine.rs:118-128`: origin
  (0,0,0) is what makes `--engine-check` blind to FMA contraction. The same zeroes
  make it blind to the entire page/wrap half of the fold.

`page_line_height` deserves its own line: it is threaded through `ItemParams`
(`layout.rs:177`) → the 128 B descriptor (`engine.rs:99,221`) → `ffi.mojo:132,166,250`
→ the Mojo `Item` (`glyph_pipeline.mojo:125`) and **nothing reads it**.
`fold.rs:759 paginate_ignores_page_line_height` pins that: two items differing only
in `page_line_height` (1.0 vs 99.0) must produce identical position lanes. It is a
frozen `.pipe.bin` fixture field, not a parameter. Bucket 4.

## The mapping table

Anchored on the shared contract's `ITEM_PARAMS`
(`packages/glyph3d-core/src/compute/glyphContract.js:91`).

| contract param | web lane / value | native lane / value | wired native? |
|---|---|---|---|
| `ORIGIN_X` | `IM_ORIGIN_X`, `CodeGrid.js:1361` → 0 | `IM_ORIGIN_X` (`glyph_schema.mojo:100`), `Item.origin_x` | **0** — placement is a `GroupRow` offset (`repo.rs:400`) |
| `ORIGIN_Y` | `IM_ORIGIN_Y`, `= this._layoutOriginY` | `IM_ORIGIN_Y` (`:92`) | **0** |
| `ORIGIN_Z` | `IM_ORIGIN_Z` → 0 | `IM_ORIGIN_Z` (`:93`) | **0** |
| `WRAP_WIDTH` | `IE_WRAP_WIDTH`, `lp.wrapWidth` (default 200) | `IE_WRAP_WIDTH` (`:107`) | yes, `wrap_cols` 100 (`repo.rs:235`) |
| `Z_STEP` | `IM_Z_STEP`, `charHeight × zWrapSpacing` (`CodeGrid.js:1365`) | `IM_Z_STEP` (`:95`) | yes since `38b677f` — `CELL_HEIGHT_WORLD × 0.15` (`repo.rs:243`) |
| `LINE_HEIGHT` | `IM_LINE_HEIGHT`, `metrics.lineHeight` | `IM_LINE_HEIGHT` (`:94`) | yes (`repo.rs:234`) |
| `SCROLL_ROWS` | `IE_SCROLL_ROWS`, `_windowScroll()` | `IE_SCROLL_ROWS` (`:105`) | **no — 0** |
| `PAGE_ROWS` | `IE_PAGE_ROWS`, `lp.pageHeight` | `IE_PAGE_ROWS` (`:103`) | yes, 128 (`repo.rs:237`) |
| `PAGE_COLS` | `IE_PAGE_COLS` — **never set** | `IE_PAGE_COLS` (`:104`) | **no — 0** |
| `PAGES_WIDE` | `IE_PAGES_WIDE`, `lp.pagesWide` | `IE_PAGES_WIDE` (`:106`) | yes, derived per file (`repo.rs:229-240`) |
| `BAND_STRIDE_Y` | `IM_BAND_STRIDE_Y`, `rows·lh + pageGapY·lh` (`CodeGrid.js:1472`) | `IM_BAND_STRIDE_Y` (`:96`) | yes, `page_h + band_gap_y` (`repo.rs:242`) — note native's gap is **world units**, web's is a multiple of `lineSpacing` |
| `DEPTH_PER_BAND` | `IM_DEPTH_PER_BAND`, `pageDepth·lh` under `axis:'z'` | `IM_DEPTH_PER_BAND` (`:97`) | **no — 0** |
| `DEPTH_PER_COL` | `IM_DEPTH_PER_COL` — **never set** | `IM_DEPTH_PER_COL` (`:98`) | **no — 0** |
| `PAGE_GAP_X` | `IM_PAGE_GAP_X`, `pageGapX × charAdvance` | `Item.page_gap_x` — **no device lane**; the device table carries `IM_PAGE_STRIDE_X` (`:99`) instead | yes, 4.0 (`repo.rs:241`) |
| — (`HAS_PAGE`) | no lane; `pageRows > 0` is the gate | `IE_HAS_PAGE` (`:108`) | yes (`repo.rs:236`) |
| — (`PAGE_STRIDE_X`) | **derived on device**, `_buildDeriveStrides` kernel 8 (`glyphPipelineKernels.js:982-999`) | derived **on host** — `derive_stride` (`glyph_pipeline.mojo:752`) between fold and paginate | — |

Two structural notes fall out of that last pair:

- The web derives the page-fan stride in a GPU kernel from a reduced scalar, so the
  CPU never learns a content width. Native derives it on the host, which is free
  today (the fold is on the host too) but is the exact seam `BACKEND-PLAN.md:104-107`
  names as a device-drain defect once the GPU path lands.
- Native's `IE_STRIDE` carries `HAS_PAGE`; the web's carries `BYTE_COUNT`. Neither is
  in the shared `ITEM_PARAMS` list and both are legitimate per-layer realizations —
  the contract prescribes kinds, not containers (`glyphContract.js:5-11`).

## The displacement table

**Web mechanism.** `GlyphLayoutKernel` (Layer 1) carries a lazily-allocated
`displacements` storage buffer — flat `[dx,dy,dz]` per field-global slot,
CPU-authored, added *after* the fold inside the same kernel
(`GlyphLayoutKernel.js:352-353, 432-438, 452-464`). An arranger measures the
sub-range it is about to move against a transient `evaluateFold` scratch
(`core/foldEvaluate.js:70`), writes deltas into the table, states its own extent, and
re-dispatches. Nothing restreams. `LayoutDescription.positionAt` reads *the same
array* (`LayoutDescription.js:199-203`), so the caret and picking agree with the
buffer by construction. `StructureLayout._bake` (`StructureLayout.js:145-205`) is the
worked example: AST blocks move as rigid swathes, everything else is parked at the
box corner with its height zeroed.

**Native equivalent: none.** No displacement lane exists in `glyph_schema.mojo`, no
delta buffer in `glyph_pipeline.mojo`, no accessor in `ffi.mojo`. The only hit for
"displacement" in the whole native tree is `engine/PLAN-DRAFT.md`.

**But state the web's status honestly before scoring it.** The web's displacement
path is currently **dormant**:

- `CodeGrid.registerArranger` **throws** (`CodeGrid.js:218-219`) — arrangers are not
  on the byte pipeline, displacement tables are codepoint-indexed and the live
  pipeline is byte-indexed.
- `GlyphLayoutCompute.syncGpuLayout` — the only caller of `GlyphLayoutKernel` — has
  **no callers** (grep across `packages/` + `app/`).
- `GlyphField.setGpuLayout` (`GlyphField.js:2131`) has no external caller, so
  `field.gpuLayout` is never true.

So the correct scoring is: **native is missing a capability the web has designed,
proven and currently has switched off.** Bucket 3, with the note that porting it now
would put native ahead of the web on this axis.

**What it would take.** The pieces are all present:
1. One `Float32Array`/`List[Float32]` of `3 × byte_count`, uploaded per item.
2. Three adds at the end of `layout_item`'s per-leader store
   (`glyph_pipeline.mojo:691-697`, right at `slots.set_position(...)`) and the same three at
   the end of `paginate` — the web adds post-fold *and* post-paginate through one
   table, so one add site per position writer.
3. An FFI entry to arm/clear it — the descriptor block has 8 spare bytes and the
   handle already owns per-item state.
4. A native counterpart to `measureSlotSpan` so an arranger can size a block. `fold.rs`
   already has `bounds_range(slots, start, stop)` (`fold.rs:522`) — that *is* the
   primitive, sub-range and all. It is currently gate-only.

The blocker is not the fold, it is that native has no point-query mirror
(see below), so an arranger would have nothing to measure against and nothing to keep
the pick path in agreement afterwards.

## Where layout runs, and what it costs

| | web | native |
|---|---|---|
| where | WebGPU compute, TSL, 9 dispatches (`glyphPipelineKernels.js` `run()`) | **Mojo, CPU**, `parallelize` shards across cores (`glyph_pipeline.mojo:17-23, 30-35`) |
| CPU fallback | none — a failed dispatch renders visibly unlaid (`GlyphLayoutCompute.js:216-223`) | it *is* the CPU path |
| GPU kernels | the only path | **not wired.** `gpu_decode/scan/paginate/bounds/pipeline` are referenced only by each other and by `check.sh:39`. `ffi.mojo` imports no `max.gpu.host`; the built `libglyph_engine.dylib` exports 8 symbols, all CPU. Stated at `BACKEND-PLAN.md:40-42`, `layout.rs:47`. |
| positions leave the GPU? | **never.** The kernel writes the field's own `instancePosition` storage attribute. | **always.** `engine.records()` copies 32 B/rendered glyph to host, then `compact_records_into` repacks to 48 B instances and re-uploads. |
| readback per load | **O(items)** — one per-item bounds table, 8 lanes (`GlyphPipelineArena.js:637-661`) | **O(bytes)** — the whole record stream |
| re-layout | `setItemPage` (repaginate, no re-scan), `rewriteItemBytes` (in-place edit), `refold` (`glyphPipelineKernels.js:1336-1350, 1287-1310`) | one-shot `load_items`. The only re-run is `rederive_records` (`repo.rs:598-609`), which builds a **fresh `Engine`, reloads the trie, and re-lays the whole file — per pick.** |

**Honest evidence for the cost.**

- Native CPU fold: `engine/README.md:574-576` — 122.2 MB/s (serial-oracle form) vs
  46.0 MB/s JS, **2.7×**; scan form 70.2 MB/s vs 8.7, 8.1×. Real corpus:
  `README.md:304` — `torvalds/linux` at **117 MB/s** in 8 MB batches, 11.9 s for
  1.385 GB. These are compute numbers on owned memory; no GPU, no readback. They are
  the strongest single argument for the port.
- Native seam timing: `repo.rs:281-284` says outright that `backend_dur` *includes*
  compaction and the extent reductions, "so it is no longer comparable to the
  pre-seam engine number." Good disclosure; it also means the seam number is not a
  fold number.
- Native GPU benchmark: **do not use it.** `gpu_pipeline.mojo`'s timed region ends
  with six `enqueue_copy` calls hauling 36 B per source byte — ~864 MB for a 24 MB
  input. `BACKEND-PLAN.md:44-64` works this out: the reported ~100 MB/s is ~3.6 GB/s
  of readback bandwidth with the kernels underneath it, and the ~400 MB upload sits
  *outside* `g0`, so the asymmetry runs against the GPU twice. The file's own defence
  ("timing only the kernels would flatter the GPU") is right about today's caller and
  wrong about the kernel.
- Web: `loadStats.kernelMs` (`GlyphPipelineArena.js:527-547`) brackets nine
  `renderer.compute()` calls. Those are *enqueues* — `run()` has no awaits and no
  fence — so the number is submit + upload cost, not layout cost. The web has no
  honest fold-time measurement either; what it has instead is a structural guarantee
  that the fold's output never crosses the bus.

Net: today, native is a **fast CPU fold behind a slow contract**; the web is a
**GPU fold with no contract to cross**. The 32 B/glyph return type
(`engine.rs:11-16`, dating from the Stage D "print records to a terminal" era) is the
single largest cost item on the native side and is the thing `BACKEND-PLAN.md`
exists to delete.

## Computed but not exposed

`ffi.mojo` exports exactly 8 symbols; `native/src/engine.rs:34-77` declares exactly
those 8; the built dylib has exactly those 8. Nothing else crosses.

| engine output | computed at | FFI accessor | consumed by Rust |
|---|---|---|---|
| `B_MIN_X/Y/**Z**`, `B_MAX_X/Y/**Z**` (`glyph_schema.mojo:113-118`) | `glyph_pipeline.mojo:611` `layout_item`, `write_bounds` block (the six `bmnx..bmxz` registers, stored at the tail); GPU twin `gpu_bounds.mojo:106-111` | **none** | **no** — `layout.rs:526-588` re-walks the records |
| `B_TOTAL_ROWS` (`:119`) | `glyph_pipeline.mojo:611` `layout_item`, `scalars[base+6]` | **none** | **no** |
| `B_MAX_ROW_EXTENT` (`:120`) | `glyph_pipeline.mojo:611` `layout_item`, `scalars[base+7]` | **none** — used internally by `derive_stride` | **no** |
| `batch_bounds` (whole-corpus box) | `glyph_pipeline.mojo` batch merge | **none** | **no** |
| `LM_BASE_X` (`:34`) | `layout_item` | none *by design* (fold scratch; not on the wire) | no |
| `wm`/`wc` witness lanes (LINE_ADV, ORD) | only under `witness=True`; `ffi.mojo:175,262` call `run_pipeline[witness=False]` | none | **never computed on the FFI path at all** |
| raw `fl` flags (`F_LEADER/RENDERED/NEWLINE/MISSING`) | decode + fold | only the derived per-item leader count via `load_items`' `counts_out` (`ffi.mojo:266-281`) | count only |
| `glyph_bake.mojo` checkpoint lanes `CK_*` (`:40-45`) | `bake_file` / `fold_bytes` | `ffi.mojo` never imports `glyph_bake` | **entire streaming-bake path unreachable from the app** |

**Confirmed: `B_MIN_Z`/`B_MAX_Z` have no accessor.** The consequence is concrete,
not theoretical. `z_step` was wired last night (`38b677f`), so the wrap staircase now
has real depth — and every bound the native app carries is 2-D:
`InkExtent { min: [f32; 2], max: [f32; 2] }` (`layout.rs:337-338`),
`PageExtent { right, bottom }` (`layout.rs:320-325`),
`SegCull { min: [f32; 2], max: [f32; 2] }` (`glyph_scene.rs:254-256`). The engine
knows the Z reach of every item and cannot say so.

Contrast the web: `foldExtent` (`core/foldGeometry.js:145-202`) is a **closed form**
on three scalars, O(1) in glyph count, and the byte pipeline's `itemBoxes` reduce
(`glyphPipelineKernels.js:1083-1088`) is fused onto the paginate pass with six
atomics — six lanes, X/Y/**Z** — and read back once per coalesced flush. Same
reduction, both sides; one of them is reachable.

The bake path being unreachable is the same shape one level up: the web
self-bakes every file it loads (`CodeGrid.js:1291-1300`) and uses the record to
pre-size the panel, seed windows and gate the fold (`CodeGrid.js:1420-1438`).
`glyph_bake.mojo` is a complete port of that with no way in.

## f64: which lanes, and what each side does

`line_advance` is the only f64 prefix chain in the fold, and it is **not** on the
render path for wrapped or paged content. The fold unit decides:

```
fold = wrap_width > 0 ? wrap_width : (has_page ? page_cols : 0)
x    = fold > 0 ? seg_adv : line_adv          # glyph_pipeline.mojo, layout_item
```

- **`fold > 0`** (any wrapping or x-paging item): `x` is `seg_adv`, accumulated
  **f32 per add**, bounded by `fold` terms. Native declares this and holds
  `seg_adv: Float32` (`glyph_pipeline.mojo:158`); the web's oracle matches it with
  `Math.fround` per add (`glyphPipelineReference.js:466-467`) and the GPU re-sums
  forward from the segment start in the same order
  (`glyphPipelineKernels.js:926-935`). This lane is **bit-identical across all four
  layers**, on purpose, and f64 never enters.
- **`fold == 0`** (foldless — a long line, no wrap, no pages): `x` is `line_adv`, an
  unbounded prefix. Native accumulates it in `Float64`
  (`glyph_pipeline.mojo:157`, `line_adv += Float64(advance)`), stores `Float32`. The
  web's oracle does the same and says why (`glyphPipelineReference.js:459-465`): the
  f64 prefix sits between the CPU serial f32 grouping and the GPU's chunked tree, so
  it is the truth layer rather than either implementation.

**The web has exactly the same problem and handles it exactly the same way.** WGSL
has no f64 either, so `tailAdv` in the TSL scan monoid is `float(0)`
(`glyphPipelineKernels.js:558`) and combines with plain `addAssign`
(`:613, 626`). The web's live-scene check drops the foldless lane to a
magnitude-scaled epsilon and says so: "a foldless line prefix is an f32 sum whose
valid groupings differ by ~|x|·5e-5" (`GlyphPipelineArena.js:697-703`).

Native's GPU kernels do the same, tiered explicitly at the top of the file
(`gpu_pipeline.mojo:14-16`):

```
ROW / COL / ORD / ordToByte   exact
LINE_ADV                      eps    (foldless f64 prefix vs the scan's grouping)
```

and again at the comparison sites (`gpu_pipeline.mojo:702-717`), with `rel_close`
(`:447-457`) as the relation.

So:

- **Affected lanes, foldless items only:** `LINE_ADV` (witness) and, through it,
  `LM_BASE_X` and `LM_X`. `LM_Y`/`LM_Z`, `ROW`, `COL`, `ORD` and every page decision
  read integers and are exact everywhere — that is the design invariant both sides
  state ("EVERY page decision reads the integer lanes… keying this off the float
  position put 119 glyphs on the wrong page", `glyphPipelineKernels.js:1057-1059`).
- **Native's advantage:** its CPU layer *can* hold f64 and does, so the Mojo port is
  the only running implementation that is bit-exact to the oracle on this lane. The
  web has no such layer in the browser — its f64 evaluator is the oracle, which is
  test-only.
- **Native's exposure when the GPU path lands:** the moment `ffi.mojo` routes to
  `gpu_pipeline`, native loses that and joins the web at eps on foldless lines. This
  is bucket 2, unavoidable, and already documented in the tier — but note that
  today's `--engine-check` and the byte-equal render A/B run at `wrap_width == 0`
  (`main.rs:92-96`), i.e. **exactly the foldless case**, so they are the checks most
  exposed to the switch.

## The difference table

| # | difference | bucket |
|---|---|---|
| 1 | Fold runs CPU-parallel across all cores, off any frame budget; 122 MB/s vs the oracle's 46 (`README.md:574-576`), 117 MB/s on `torvalds/linux` (`README.md:304`) | **1** |
| 2 | Owned memory, no worker `postMessage` copies, no masked-word byte upload | **1** |
| 3 | Item placement is a `GroupRow` offset (`repo.rs:400`) — moving a file is an 80 B write, not an item-table re-upload + re-dispatch | **1** |
| 4 | `line_adv` held in true f64 — the only running layer bit-exact to the oracle on the foldless lane (`glyph_pipeline.mojo:157`) | **1** |
| 5 | Layout params validated at the seam with a NaN-is-unset rule (`layout.rs:204-260`); the web kernel validates only `axis` | **1** |
| 6 | Two independent FFI strategies diffed bit-for-bit as a standing gate (`layout_mojo.rs:195-225`) | **1** |
| 7 | Item table split by carrier because Metal has no f64 (`glyph_schema.mojo:89-91, 110-111`) — same split, same reason, as the web | **2** |
| 8 | GPU kernels drop `LINE_ADV` + position lanes to an eps tier (`gpu_pipeline.mojo:14-16`) — WGSL/Metal have no f64; the web lives under the identical constraint | **2** |
| 9 | C ABI is scalars + opaque handle + a 128 B descriptor block (`engine.rs:86-119`); no Mojo types cross | **2** |
| 10 | **No displacement table** — no lane, no buffer, no FFI. (Web's is designed and proven but currently dormant: `registerArranger` throws, `syncGpuLayout` has no caller.) | **3** |
| 11 | **No fold mirror.** No `positionAt`/`charForSlot`/`slotForChar` equivalent (`LayoutDescription.js:116-205`). Picking re-runs the whole engine. | **3** |
| 12 | **No closed-form extent.** `foldExtent` (`foldGeometry.js:145`) is O(1); native walks every record. | **3** |
| 13 | **No scroll conveyor at the app level** — `scroll_rows` 0 at every call site | **3** |
| 14 | **No re-layout path** — no repaginate, no in-place edit rewrite, no refold | **3** |
| 15 | **`axis:'z'` page mode not wired** (`depth_per_band`/`depth_per_col` 0) | **3** |
| 16 | **No named presets / runtime layout verb** — no `LAYOUT_PRESETS`, no `grid.layout`, no `setDefaultLayout` (`workers/builders/index.js:56-75`) | **3** |
| 17 | **All extents are 2-D** (`layout.rs:320-339`, `glyph_scene.rs:254-256`) while the fold now has real Z | **3** |
| 18 | **Per-item box + fold scalars computed then discarded** — no FFI accessor; Rust re-implements a worse version (`layout.rs:526-588`) | **4** |
| 19 | **32 B/glyph host readback inside the seam** — the `Vec<GlyphRecord>` contract (`engine.rs:11-16`); web reads back O(items) | **4** |
| 20 | **`page_line_height` is a dead param** carried end-to-end, proven inert (`fold.rs:759`) | **4** |
| 21 | **Pick builds a fresh `Engine` and reloads the trie per pick** (`repo.rs:605-607`) | **4** |
| 22 | **GPU stride derivation drains the device to host** for one float per item; the web has an on-device kernel (`glyphPipelineKernels.js:_buildDeriveStrides`) | **4** |
| 23 | **`k_spine_scan` dispatches `grid_dim=1, block_dim=1`** — one thread combining 1,536 supers at 24 MB (`BACKEND-PLAN.md:104-106`) | **4** |
| 24 | **`glyph_bake.mojo` unreachable from the app** — `ffi.mojo` never imports it; the web self-bakes every load and uses the record | **4** |

## What is worth doing first

Not a plan, just what the evidence orders:

1. **Expose the bounds.** One FFI accessor over `item_bounds` deletes #18 and #17 at
   once, and it is the cheapest thing on this list: the values are already in
   `PipelineResult`. It also removes a second implementation of a reduction, which is
   the failure mode the repo's own rules name (a checker that is a second
   implementation with its own bugs).
2. **Wire `scroll_rows`.** It is one line at `repo.rs:239` and it is the difference
   between a static field and a navigable one.
3. **Delete `page_line_height`,** or write down why a proven-inert param stays.
4. **Decide `page_cols`/`depth_per_col` deliberately.** Both are unwired on *both*
   sides. Either they are contract surface nobody wants, in which case the contract
   should say so, or they are the z-page/newspaper modes and both trees are missing
   the same feature.
