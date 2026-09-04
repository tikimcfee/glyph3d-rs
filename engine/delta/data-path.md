# Delta: the per-glyph data path and its buffers

Web (reference): `/Users/lugo/localdev/viz-web/glyph3d-js` — JS / three-webgpu / TSL.
Native (current): `/Users/lugo/localdev/viz-native/glyph3d-native` — Rust / wgpu / Mojo.

Scope: how per-glyph data gets from "computed" to "on screen" — record shape, carrier,
upload, update, addressing, chunking. Not: shaping, atlas, fold semantics, camera.

## Summary

Both trees arrived at the same architecture independently enough that the agreements are
worth stating first: **one arena, files as views into it, one group table of 5 vec4 rows
carrying a full TRS + color + clip, per-view range draws, and vertex-stage culling by
degenerating clip to (2,2,2,1)**. `glyph_field.wgsl` is a semantics-faithful port of
`glyphVertex.js` down to the quat sandwich and the OOB-group clamp-and-cull pairing. The
group row is byte-identical in schema (80 B, same five columns) on both sides.

The differences are all in the *carrier and its lifecycle*, and they split cleanly:

- Native wins where owning memory is the point: no host mirror after upload, no
  bind-group-cache workaround, a mixed-kind AoS record with zero bitcasts, chunking past
  the binding limit instead of a hard ceiling, per-instance bytes spent on neither picking
  nor highlight.
- Native is behind in exactly one structural place, and its own source says so: **layout
  still comes home**. `engine.records()` hauls 32 B per source byte across the FFI, the
  CPU compacts and repacks 32 → 48 B, and the whole arena uploads once. The web computes
  layout on the GPU into the buffer the vertex shader reads and never reads a position
  back. `layout.rs`'s header is written *about* this defect; stage 3 is designed to delete
  it.
- Two smaller native regressions are unforced: 16 of 48 bytes per instance are read by no
  shader, and the arena cannot grow, shrink, or be edited after `GlyphScene::new`.

Bucket counts: **1 (better) — 7 · 2 (platform constraint) — 3 · 3 (missing) — 3 ·
4 (worse, no reason) — 3**.

## The differences

| # | Difference | Bucket |
|---|---|---|
| 1 | Record carrier: two single-kind flat arrays (web) vs one typed 48-B AoS struct (native) | 1 |
| 2 | Host mirror: web keeps a full host copy of the slot buffer forever; native drops the `Vec` after upload | 1 |
| 3 | Storage-buffer layout pinned by encase-vs-bytemuck byte-equality tests; web has no equivalent at the shader boundary | 1 |
| 4 | Chunking past `max_storage_buffer_binding_size`; web refuses instead (`assertSlotBufferFits`) | 1 |
| 5 | Picking costs 0 B/glyph (CPU ray + AABB + re-derive) vs the web's `instancePickingId` u32 attribute + ID render pass | 1 |
| 6 | Selection costs 0 B/glyph (mask pass + composite) vs the web's `instanceHighlight` RGBA8 attribute | 1 |
| 7 | No bind-group rebind seam needed; the web needs `registerByteSlotsNode`/`rebindByteSlots`/`material.dispose()` | 1 |
| 8 | Direct CPU-culled range draws instead of indirect multi-draw (wgpu 30 Metal: `first_instance != 0` rasterizes nothing) | 2 |
| 9 | Arena is chunked at all — the binding limit is real on both sides | 2 |
| 10 | AoS forces a whole-record rebuild for a multi-glyph recolor; the web's separate color attribute makes it one contiguous range | 2 |
| 11 | No content edit / restage / growth: `instance_bufs` is built once and never resized | 3 |
| 12 | No arbitrary multi-range per-glyph highlight (only one `Selection`) | 3 |
| 13 | No drawn-instance budget; a mid-zoom view draws every non-subpixel segment's glyphs | 3 |
| 14 | Layout comes home: 32 B/source byte FFI readback + CPU compaction + one whole-arena upload | 4 |
| 15 | 16 of 48 bytes per instance (`row`, `col`, `flags`, `_pad`) are read by no shader | 4 |
| 16 | Compaction drops `glyph_id == 0` — buys 1.73% of VRAM, costs the byte↔slot address space | 4 |

---

## 1. The per-glyph record

### Native — one 48 B AoS struct

`native/src/glyph_scene.rs:102-114` (`GlyphInstance`), mirrored in
`native/src/shaders/glyph_field.wgsl:47-58` (`InstanceSlot`). Carrier: **one
`var<storage, read> array<InstanceSlot>`**, `@group(0) @binding(1)`
(`glyph_field.wgsl:78`). There are no vertex attributes at all — the quad is six
`corners` constants indexed by `@builtin(vertex_index)` (`glyph_field.wgsl:97-105`).

| field | width | offset | read by a shader? |
|---|---|---|---|
| `pos` | vec3 f32 | 0 | yes |
| `glyph_id` | u32 | 12 | yes |
| `row` | u32 | 16 | **no** |
| `col` | u32 | 20 | **no** |
| `color` | u32 (packed sRGB RGBA8) | 24 | yes |
| `group_id` | u32 | 28 | yes |
| `advance` | f32 | 32 | yes |
| `height` | f32 | 36 | yes |
| `flags` | u32 | 40 | **no** |
| `_pad` | u32 | 44 | **no** |

**48 B/glyph**, and the offsets above are not read off the struct — they are asserted
against encase's WGSL-layout metadata and against bytemuck's raw bytes in
`glyph_scene.rs:3285-3374`.

Group table: `GroupRow` = `[[f32;4];5]` = **80 B/group**, `glyph_scene.rs:119-121`, its own
storage buffer at `@binding(2)`, CPU mirror kept in `groups_cpu`.

### Web — two single-kind flat arrays, one slot per SOURCE BYTE

`packages/glyph3d-core/src/compute/glyphPipelineReference.js:124-142`. Carrier: **two
storage buffers**, an `f32` measures array and a `u32` exact array, allocated as
`byteLength × stride` in `allocSlots` (`glyphPipelineReference.js:292-296`), read in the
vertex stage at `instanceIndex × stride + namedLane`
(`core/glyphVertex.js:217-234`).

| array | lanes | width | render-read? |
|---|---|---|---|
| measures (f32) | `M_X`,`M_Y`,`M_Z`,`M_ADVANCE`,`M_HEIGHT` | 20 B | yes |
| measures (f32) | `M_BASE_X`,`M_LINE_ADV` | 8 B | fold scratch (GPU-only) |
| exact (u32) | `E_GLYPH_ID`,`E_ROW`,`E_COL` | 12 B | `GLYPH_ID` yes; `ROW`/`COL` → varying |
| exact (u32) | `E_FLAGS`,`E_ORD` | 8 B | fold scratch (GPU-only) |

= `SLOT_BYTES_PER_SOURCE_BYTE` **48 B per source byte**
(`compute/glyphPipelineKernels.js:127`). Plus four per-instance vertex attributes the slot
buffer does not carry, allocated at arena capacity in
`GlyphField.js:980-1023`: `instanceColor` RGBA8 (4 B), `instanceGroupId` f32 (4 B),
`instancePickingId` u32 (4 B), `instanceHighlight` RGBA8 (4 B).

**Web total: 64 B of GPU per source byte, plus a 48 B/byte host mirror** — three's
`instancedArray` allocates a host typed array beside every storage buffer, which
`glyphPipelineKernels.js:139-148` records as the reason the index wall is unreachable
("2^26 source bytes builds (3GB host), 2^27 throws").

**Native total: 48 B of GPU per surviving glyph, host copy dropped.** `create_buffer_init`
consumes the chunk slices (`glyph_scene.rs:1205-1223`) and `GlyphScene` keeps no
`Vec<GlyphInstance>` field — only the group mirror and the per-segment cull table survive
staging.

### Why the web split and native did not — **bucket 1**

`glyphPipelineReference.js:101-110` is a confession: the slot record used to be one
`Uint32Array` with f32 measures held as bitcasts and a lane-kind table on the side, and
that container cost five separate defects, "every one of them an exact value on a float
carrier". The fix was to make the kind *be* the container — two arrays.

That is a JS/TSL constraint, not a truth about mixed records. A WGSL struct has typed
fields and Rust has `#[repr(C)]`, so native gets one mixed-kind record with **zero
bitcasts** and one contiguous 48-B fetch per instance where the web issues eight scalar
loads across two buffers, plus the `_byteSlotsNodes = { m, x }` two-registry machinery
(`core/glyphVertex.js:83-114`) that exists only to keep a float node from being rebound to
the integer buffer. Native does not need any of it. This is the port exploiting its
platform, and it should stay.

Native's layout tests (`glyph_scene.rs:3285-3374`) are the stronger half: encase generates
the WGSL layout independently, bytemuck generates the wire bytes, and
`encase_bytes_match_bytemuck` compares them for distinctive bit patterns. The web's
nearest equivalent is a lane-permutation conformance gate at the JS tier plus a
comment-enforced "stride-4 from BIRTH" rule (`GlyphField.js:984-996`) whose documented
failure mode is a stale vertex-input layout that renders glyphs splayed while
`layout.verify` reads the buffer as correct. **Bucket 1.**

---

## 2. Upload, and update after upload

### Native

**Upload: once, whole arena, at scene construction.** `glyph_scene.rs:1205-1223` —
`create_buffer_init` per chunk, `STORAGE | COPY_DST | COPY_SRC`. Group table likewise
(`:1224-1229`). Measured for the glyph3d-js corpus: 95,181,245 instances = **4,357 MiB in
2 chunks** (`out/g-windowed-smoke.log:10`).

**Update: partial, per field, at 48 B stride.** `write_instance(slot, field_off, data)`
(`glyph_scene.rs:2241-2246`) resolves the chunk, then `queue.write_buffer` at
`local * 48 + field_off`. Actual verb costs, from the smoke log and the code:

- recolor one glyph: **4 B** at `+24` (`glyph_scene.rs:2306`)
- nudge one glyph: **12 B** at `+0` (`:2395`)
- scale one glyph: **8 B** at `+32` (`:2412`)
- move/scale/tint a group: **80 B**, one row (`write_group_row`, `:2249-2255`)
- recolor a line: contiguous slots coalesced into runs, each run written as **rebuilt
  whole 48-B records** (`:2330-2384`) — "the color field is strided 48 B apart, a raw
  color-only byte range would stomp neighboring fields"

That last one is the AoS tax (**bucket 2**, and modest): 26 glyphs cost 1,248 B where the
web's separate `instanceColor` attribute makes the same edit one contiguous
`addUpdateRange(start*4, n*4)` of 104 B. It also forces `geom_overrides:
HashMap<u32, ([f32;3], f32, f32)>` (`glyph_scene.rs:925-927`) so a run rebuild does not
silently discard an earlier nudge — a bookkeeping structure that exists because there is
no CPU mirror of the arena to read the current record from.

**No growth path.** `instance_bufs` appears at `glyph_scene.rs:908` (field), `1205`
(construction), `1326-1327` (bind groups), `2245`/`2378` (writes), `2799` (debug readback)
— and nowhere else. There is no append, no realloc, no restage, no free list, no content
rewrite. Files are fixed at `GlyphScene::new` (`main.rs:203/215/228`). **Bucket 3**, and
the largest missing capability on this path.

### Web

**Upload: incremental, per staged range.** `stage()` allocates a byte range synchronously
(best-fit from a coalescing free list, else the high-water mark —
`GlyphPipelineArena.js:415-455`) with **no dispatch and no CPU layout**; every stage in one
macrotask coalesces into one `requestFlush()` that re-syncs the item table and uploads only
the newly staged ranges (`writeBytes` → masked word writes,
`glyphPipelineKernels.js:1144-1152`), then runs nine dispatches for the whole storm.

**Update:** `addUpdateRange` on the typed-array-backed attribute; three uploads only the
marked span.

- per-glyph color: 4 B at `slot*4` (`GlyphField.js:1409`, range-marked at `:1277`)
- group row: 80 B (`_touchGroup` → `addUpdateRange(groupId*GROUP_STRIDE*4, …)`,
  `GlyphField.js:918-925`)
- **content edit in place**: `rewriteItemBytes` (`glyphPipelineKernels.js:1296`) masked
  word writes into the byte buffer within the item's pre-staged capacity slack, then one
  coalesced re-fold. No new item, no growth, no view invalidation, and paint lanes are
  untouched so colors survive the edit (`GlyphPipelineArena.js:333-350`).
- **range move**: `copyGlyphLanes` (`GlyphField.js:1268-1285`) carries the colorizer's and
  highlight's finished work `copyWithin` when a view's slot range relocates.

**Growth/realloc: rare and loud.** ×2 (or ×1.25 past a single oversized file), re-upload
every live item, re-dispatch, re-attach every live field
(`GlyphPipelineArena.js:36-38`, `:293-301`).

### Layout transport — **bucket 4, and the tree says so itself**

This is the real delta, and it is not about buffers.

- **Web:** bytes go up, nine compute dispatches write the slot buffer, the vertex shader
  reads that same buffer. Positions never touch the CPU. The only readback is one per-item
  bounds record (`GlyphPipelineArena.js:14-25`).
- **Native:** `MojoLayout::run` ends with `self.engine.records()` — the full 32 B wire
  stream over the FFI — then `compact_records_into` repacks 32 → 48 B on the CPU
  (`layout_mojo.rs:63-116` → `layout.rs:526-596`), and only then does anything reach the
  GPU.

For the measured corpus (`out/g-windowed-smoke.log:5-7`): 96.9 MB of source →
**96,860,762 records × 32 B = 3.10 GB across the FFI**, then 4.36 GB of upload; stage
1.438 s on top of engine 1.805 s; `out/STAGE_E2_REPORT.md` records 8.53 GB peak RSS /
10.35 GB footprint for the full render.

`layout.rs:9-30` is written about exactly this ("its timed region ends with six
`enqueue_copy` calls hauling 36 B of output per source byte… so the number is a
readback-bandwidth measurement with the kernels somewhere underneath it"), and
`layout_mojo.rs:17-24` names the fix: a compaction kernel writing the arena directly, with
the readback kept behind the separate `VerifyLayout` trait so gates can pay for it and
frames cannot. The seam is already shaped for it — `GlyphArena` is passed in as a
destination precisely so its interior can become a device buffer without moving a call
site (`layout.rs:382-393`). So: worse than the web today, deliberately, with the exit
already cut. Judge it as an unfinished stage, not a mistake.

---

## 3. Addressing — the identity the web keeps and native spends

### The web's claim is true, and it is load-bearing

Verified. `decodeAndResolve` gives **every source byte a slot**, including continuation
bytes: a non-leader returns early after explicitly zeroing `M_ADVANCE`/`M_HEIGHT`
(`glyphPipelineReference.js:330-341`), and `core/glyphVertex.js:210-215` states the
consequence — "size (0,0) collapses the quad to a point: invisible, unpickable". The zeroing
is not left to fresh-array zero-init: a rewritten range's edit slack was a real glyph last
run, so the write must happen every run.

`instance index == arena byte offset == slot index` is relied on by:

- **Picking**: one registration for the whole mega-field, `ID = base + absolute slot`;
  `resolveSlot()` binary-searches live ranges back to `(view, view-local slot)`
  (`MegaGlyphField.js:21-22`).
- **Color and highlight**: `setGlyphColorRange` / `setGlyphHighlight` /
  `setGlyphPaletteRange` all take FILE byte offsets, and the view's *one* translation is
  `- sourceBase` for a windowed grid (`MegaGlyphField.js:666-689`). A palette indexed by
  file byte lands as one write.
- **Edit in place**: `rewriteItemBytes` addresses `[byteStart, byteStart+capacity)`
  directly (`glyphPipelineKernels.js:1296-1310`).
- **Allocation**: the free list traffics in *byte* ranges and an item's `byteStart` never
  moves once staged, so reuse needs no GPU moves and no view invalidation
  (`GlyphPipelineArena.js:40-49`).
- **Verification**: `layout.verify` diffs the GPU slice against the CPU mirror per byte
  (`GlyphPipelineArena.js:664-690`).
- **Fail-loud**: `attachBytePipeline` refuses a nonzero `slotBase` outright rather than let
  a field silently render another file's bytes (`GlyphField.js:1977-1984`).

### What native's compaction costs and buys

`layout.rs:564` — `if record.glyph_id() == 0 { continue; }` — drops blanks; the fold has
already baked their advance into the survivors' X, so nothing moves.

**Buys, measured:** 1,679,517 of 96,860,762 records dropped = **1.73%**, i.e. ~76 MiB of
the 4,357 MiB arena (`out/g-windowed-smoke.log:6`). It also makes the ink extent exact by
construction (`layout.rs:557-570`), which the page extent — computed over *all* records —
deliberately is not.

**Costs:** the address space. Nothing on the GPU or in the arena can name a source byte, so:

- A pick must rebuild the mapping from scratch: re-read the file, re-run the whole engine
  over it with the exact staged `ItemParams`, re-fold, and *count survivors* to fill
  `slot_of: Vec<u32>` with `u32::MAX` for blanks (`glyph_scene.rs:1864-1918`). Measured at
  999.6 µs for a 3,697 B file (`out/g-windowed-smoke.log:15`) — fine at interaction rate,
  but it is a full re-layout to answer "which slot is this byte".
- `PickGlyph.slot` is `Option<u32>` (`glyph_scene.rs:212`) and three verbs have to
  special-case "this character is blank (no instance)" (`:2303`, `:2392`, `:2409`).
- `geom_overrides` exists because there is no readable mirror to rebuild a record from.

**Verdict: bucket 4 on the trade as struck** — 1.73% of VRAM for the whole address space,
and the correctness discipline that makes the re-derivation safe (bit-identical engine
re-run) is itself substantial machinery. The *dropping of the CPU mirror* is a separate and
genuinely good decision (**bucket 1**) and does not require compaction; an uncompacted
arena with `slot = record index` would keep both.

One honest caveat: the web's slots are per *byte* and native's records are per *codepoint*,
so on mostly-ASCII source the two counts are within a rounding error of each other and the
1.73% figure is the whole of what compaction buys.

---

## 4. Chunking

### Native — real limit, chunking response

`native/src/gpu.rs:254-259` requests the adapter's **full** `max_storage_buffer_binding_size`
and `max_buffer_size` rather than the WebGPU defaults, then
`glyph_scene.rs:1201-1203`:

```rust
let binding_limit = ctx.device.limits().max_storage_buffer_binding_size as usize;
let chunk_cap = (binding_limit / std::mem::size_of::<GlyphInstance>()).max(1);
let chunks: Vec<&[GlyphInstance]> = instances.chunks(chunk_cap).collect();
```

Measured on an M2 (`out/g-windowed-smoke.log:3,10`): limit 4,294,967,292 B (4,095 MiB) →
`chunk_cap` 89,478,485 instances → **2 chunks** for 95.18M. One bind group per chunk
(`glyph_scene.rs:1327`), and `instance_index` is chunk-local, which is exactly right since
each chunk buffer starts at 0.

The cost is arithmetic and one invariant, all of it visible:
`slot / chunk_cap` + `slot % chunk_cap` at every write site (`:2242-2243`, `:2787-2788`), a
run must not straddle a chunk boundary (`*start / self.chunk_cap == s / self.chunk_cap`,
`:2366`), the cull splits each segment's slot range per chunk (`:442-451`), and the draw
loop rebinds on chunk change (`:3091-3095`).

### The web has the same problem and answers it with a ceiling

`GlyphCanvas.jsx:61-66` requests `min(adapter, 2 GB)` for both limits and **throws** rather
than boot on defaults (128 MB ≈ 2.79 MB of source). The arena's real ceiling is then
derived, one buffer only:

```js
export const ARENA_MAX_BYTES = Math.min(
    KERNEL_MAX_BYTES,                                     // 2^32/7 — the u32 index wall
    Math.floor(REQUESTED_BINDING_CAP / SLOT_BYTES_PER_SOURCE_BYTE),  // 2^31/48
);                                                        // = 44,739,242 source bytes
```
(`glyphPipelineKernels.js:124-153`), and `assertSlotBufferFits`
(`glyphPipelineKernels.js:169-183`) **refuses** a larger request at the seam, naming the
request and the limit.

So the constraint is identical (**bucket 2**) and the responses differ. Native's is
strictly more capable: it staged 96.9 MB of source in one arena; the web's hard ceiling is
44.7 MB and it would have refused this corpus. **Chunking is a choice, and it is the better
one — bucket 1.**

Two things keep that honest. First, the web mitigates by **windowing**: a grid stages only
`[sourceBase, sourceBase + byteCount)` of its file (`MegaGlyphField.js:640-645`), so the
arena holds the visible window rather than the repo, and the ceiling binds far less often
than the raw comparison suggests. Native stages every byte of every file and pays 4.36 GB
of VRAM and 8.53 GB of RSS for it. Second, chunking and windowing are orthogonal — a
chunked arena that also windowed would need neither the ceiling nor the 4 GB.

---

## 5. Draw submission and the cull

**Web:** multi-draw indirect. One 5-uint record per visible view —
`{indexCount: 6, instanceCount: len, firstIndex: 0, baseVertex: 0, firstInstance: slotBase}`
— written into an `IndirectStorageBufferAttribute` each frame by a CPU frustum test, with
adjacent live ranges merged (`MegaGlyphField.js:169-176`, `:583-618`). `firstInstance`
carries the slot base, "so the slot==index address space survives with ZERO shader
changes".

**Native:** the same design, blocked. `glyph_scene.rs:50-60` and `cull.wgsl:27-31` record
that wgpu 30.0.1's Metal backend **silently rasterizes nothing for any indirect draw with
`first_instance != 0`** — verified by readback (args correct), per-chunk and per-offset
A/Bs, and the observation that `first_instance = 0` always works. So the cull is CPU
(`cull_segments`, `glyph_scene.rs:393-465` — ~1,305 AABB tests) and the draws are direct
ranges, `pass.draw(0..6, base..base+count)`, chunk-major so blend order matches the legacy
full draws (`:3090-3096`). **Bucket 2**: a documented platform defect with a repro, an
answer that costs microseconds, and a stated re-entry point after a wgpu upgrade.

Two consequences worth noting rather than filing:

- Native draws 6 vertices with no index buffer; the web draws a 4-vertex indexed quad
  (`indexCount: 6`). Wash.
- The LOD tiers differ in kind. Web: a **measured drawn-instance budget** (2,000,000 —
  `MegaGlyphField.js:74-95`) with hysteresis, demoting whole views to a panel. Native: a
  per-segment subpixel test substituting one flat backdrop quad of mean ink colour × ink
  coverage (`glyph_scene.rs:80-96`, `cull.wgsl:12-22`). Native's is continuous and
  distance-driven, which is nicer at the extremes, but there is **no ceiling on drawn
  instances** — at a mid zoom where most segments clear 1 px/em, every one of their glyphs
  is drawn. **Bucket 3**, modest.

---

## 6. Where native is straightforwardly better

Beyond the record carrier, the dropped host mirror, the layout tests and chunking already
covered:

**No bind-group rebind seam (bucket 1).** `core/glyphVertex.js:57-81` documents that
three's WebGPU backend keys its `GPUBindGroup` cache on bound *textures* only — storage
buffer identity is not in the key — so when the arena reallocates and the old slot buffer
is destroyed, every already-built bind group keeps handing the render pass a destroyed
buffer, once per frame, forever. The workaround is two node registries, a material
registry, and `material.dispose()` to reach the cache. Native builds bind groups from
buffers it owns and there is nothing to poison. Pure platform tax on the web's side —
though note native has not had to face the question at all, because it never reallocates
(§2).

**Picking costs zero per-instance bytes (bucket 1, with a stated limit).** The web spends a
capacity-sized `instancePickingId` Uint32Array (`GlyphField.js:1021-1022`) and a
multi-channel ID render pass. Native spends 0 B and resolves screen px → analytic f64 ray →
nearest visible file AABB → group-TRS inverse → nearest record cell
(`glyph_scene.rs:11-26`, `1954-2108`). At 1,305 segments that is microseconds. The
limitation is declared, not hidden: it assumes glyphs are coplanar at the group's z, and the
ray is deliberately analytic rather than an inverse view-proj because at near=0.05 /
far≈1.7e6 the f32 inverse unprojected every pick to w≈0 (`glyph_scene.rs:1943-1953`, with
`tools/repro_pick_oblique.py` cited). The web's GPU pass is layout-agnostic; native's is
faster and cheaper for the layouts it has.

**Selection costs zero per-instance bytes (bucket 1 / bucket 3).** The web carries
`instanceHighlight` RGBA8 at capacity — 4 B/glyph, ~363 MiB at this scale — whose alpha byte
selects tint-vs-fill mode (`GlyphField.js:980`, `1296+`). Native draws the selected quads
into a mask target and composites a tint (`glyph_scene.rs:1041-1068`, `3110-3190`), which
its own comment describes as replacing an "instance-byte write + restore" hack: "no buffer
writes, nothing to restore". That is the better technique for *a* selection. It is also
only one selection — `enum Selection { Glyph, Segment }` — so syntax fill bars, search
hits, and multi-range hover are not expressible. Bucket 1 for the mechanism, **bucket 3**
for the capability.

---

## 7. The two unforced regressions

**16 of 48 bytes per instance are read by no shader (bucket 4).** Grepping
`native/src/shaders/*.wgsl` for `inst.` fields yields `pos`, `glyph_id`, `advance`,
`height`, `color`, `group_id` — and nothing else. `row`, `col`, `flags` and `_pad` are
never read by the field shader, the mask pipeline (same bind group) or the backdrop
pipeline (its own `BackdropInst`). The only GPU-side reader is the `GLYPH_G_DUMP`
verification readback (`glyph_scene.rs:2786-2799`); picking reads `row`/`col` from the
CPU-side re-derived records, never from the buffer. A 32-B record (pos 12 + glyph_id 4 +
color 4 + group_id 4 + advance 4 + height 4) already satisfies WGSL's 16-B struct alignment
with no padding. At 95.18M instances the dead lanes are **1,452 MiB of the 4,357 MiB
arena**, and they are the widest thing the vertex stage fetches per invocation.

The web treats this as a rule, not a preference: `core/glyphVertex.js:52-56` records that
`GroupRow` cols 5-6 were *deleted* rather than zeroed when the far tier went away — "dead
lanes in a hot per-group row are the compatibility shim this repo forbids, and the row is
80B instead of 112B". The same argument applies here with 95 million times the multiplier.

**The arena cannot change after construction (bucket 3).** Covered in §2. In web terms
native is missing: `stage`, `dispose`, the free list, `rewrite`, `adoptField`, realloc, and
the group-table growth seam. Everything the current demo does — recolor, nudge, scale,
move, tint, hide — operates on instances that already exist. The seam is shaped to accept
these (`GlyphArena` is a caller-owned destination, `layout.rs:382-424`); none of them are
written.

---

## What I would fix first, in order

1. **Drop the dead lanes** — 48 B → 32 B is a third of the largest buffer in the process,
   for a struct edit plus the four asserted offsets in `layout_tests`. If the
   `GLYPH_G_DUMP` gate needs `row`/`col`, it can re-derive them the same way picking
   already does.
2. **Land stage 3** (compaction kernel writes the arena; bounds kernel fills the extents).
   `layout.rs` and `layout_mojo.rs` already describe it precisely and the signatures do not
   move. This deletes 3.1 GB of FFI traffic and 1.4 s of staging.
3. **Reconsider compaction** while doing (2). If the compaction kernel keeps
   `slot = record index` instead of stream-compacting, native gains the web's byte↔slot
   address space, `ensure_pick_cache` collapses to arithmetic, `slot_of` and
   `geom_overrides` disappear, and the measured cost is 1.73% of VRAM — which the fix in
   (1) more than pays for twice over.
4. **Then** growth/restage, which is a real feature and should be designed against a
   device-resident arena rather than retrofitted onto the current one.
