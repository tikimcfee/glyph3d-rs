# Interaction-layer delta — groups, transforms, picking, highlight, per-glyph mutation

Web (reference): `/Users/lugo/localdev/viz-web/glyph3d-js` — JS, three/webgpu, TSL.
Native (current): `/Users/lugo/localdev/viz-native/glyph3d-native` — Rust, wgpu, Mojo engine.

## Summary

The **group table is a faithful port, byte for byte** — same 5-vec4 / 80 B row, same
column meanings, same T·R·S + quat-sandwich, same two vertex culls. The GPU side of
groups has no delta worth the name.

The **draw structure is the same two-level design in both trees**, contrary to the
framing in the brief. The web does *not* issue one draw over the arena: it holds one
arena-wide instance buffer and, every frame, CPU-frustum-culls per view range and emits
**multi-draw-indirect** records with nonzero `firstInstance`. Native holds the same
arena (chunked by the storage-binding limit) and emits **direct range draws** per visible
segment. Same cull, same granularity, different submission verb — because the web's
mechanism is unavailable on wgpu 30/Metal. On this platform native's choice is the
better one.

The real deltas are three: **highlight is entirely absent** (native has one screen-space
selection tint instead of a per-glyph two-mode highlight lane); **picking trades
structural correctness for latency** (native re-derives geometry on the CPU rather than
reading a GPU ID pass, which buys a synchronous, semantically richer, headless-capable
pick but gives up the web's "pick and render cannot drift" guarantee — and already
ignores group rotation and clip); and **the instance byte offsets are untied literals**.

One outright defect: `SegCull` cannot represent z, but `MoveGroup` writes z. Cull and
pick then disagree about where a moved file is.

## Difference table

| # | Difference | Bucket |
|---|---|---|
| 1 | Group row: 5 vec4 / 80 B, identical columns and identical shader TRS | parity |
| 2 | Native owns `wgpu::BindGroup` directly; the web needs two whole seams (`rebindByteSlots`, `disposeGroupMaterials`) to work around three's texture-keyed bind-group cache | 1 — better |
| 3 | Group row upload: web `addUpdateRange` whole row vs native `queue.write_buffer` 80 B — same technique, native without the dirty-set/version dance | 1 — better |
| 4 | No group allocator: no free list, no dead group 0, no capacity growth; groups are staged once | 3 — missing |
| 5 | Group quaternion (col 1), clip window (col 4), colorBlend (col 3.w) are never written CPU-side, though the shader honors all three | 3 — missing |
| 6 | No `setGlyphGroupRange` equivalent: an instance's `group_id` is fixed at stage time | 3 — missing |
| 7 | Direct range draws instead of indirect-with-firstInstance | 2 — platform (wgpu 30 Metal), and better here |
| 8 | Instance arena split into chunks + one bind group per chunk | 2 — platform (`max_storage_buffer_binding_size`) |
| 9 | Picking is a CPU re-derivation, not a GPU ID pass | 2 + 1 — synchronous, headless, semantically richer |
| 10 | Pick ignores the group quaternion and the clip window; render does not | 3 — missing (latent 4) |
| 11 | One pick "channel" (files+glyphs) vs the web's four (glyph/grid/handle/group) | 3 — missing |
| 12 | Pick cost is O(file bytes) per cache fill and O(records) per click, with a **one-entry** cache | 4 — unfinished |
| 13 | Per-glyph highlight (RGBA8, alpha byte = tint/fill mode) entirely absent | 3 — missing |
| 14 | `RecolorLine`, keybound as "highlight line", is a destructive base-color overwrite with no clear path | 4 — a stub wearing a capability's name |
| 15 | Selection is a screen-space mask + one global tint color, windowed-path only | 3 — missing (offscreen never shows it) |
| 16 | Instance byte offsets (`0`, `24`, `32`) and strides (`48`, `80`) are untied literals | 4 — `offset_of!` is the fix |
| 17 | `RecolorLine` rebuilds whole records and coalesces contiguous runs into one `write_buffer` per run | 1 — better than a per-slot loop |
| 18 | Cull AABB is 2D + a hardcoded z slab of ±1; `MoveGroup` writes z; `sync_segment` cannot carry it | 4 — real defect |
| 19 | LOD by px/em → flat backdrop quad vs the web's instance-budget → panel fallback | parity of intent, both defensible |
| 20 | Fragment stipple-dither alpha gate not ported; hard `discard` at alpha 0 | 3 — missing, low severity |
| 21 | `GLYPH_CELL_AREA` bakes a font metric (`1229.0/2320.0`) as a literal | 4 — minor; the web forbids this by convention |

## 1. The group table

**Identical schema.** `packages/glyph3d-core/src/core/glyphVertex.js:36-53` documents the
row: col 0 offset.xyz, col 1 quaternion, col 2 color.rgb+alpha, col 3 scale.xyz+colorBlend,
col 4 clipTop/clipBottom/clipEnabled. `GROUP_STRIDE = 5` (`glyphVertex.js:53`), 80 B.
Native mirrors it exactly: `GroupRow { cols: [[f32;4];5] }`
(`native/src/glyph_scene.rs:119-121`), `GROUP_STRIDE: u32 = 5u`
(`native/src/shaders/glyph_field.wgsl:34`).

**Identical addressing and identical shader math.** Web:
`groups.element(grow.mul(GROUP_STRIDE).add(col))` (`glyphVertex.js:275-281`), then
`local = (quad + iPos) * gScale`, quat sandwich `v + 2·q.xyz × (q.xyz × v + q.w·v)`,
`+ gPos.xyz`, MVP (`glyphVertex.js:288-295`). Native: `groups[grow * GROUP_STRIDE + n]`
and the same three lines (`glyph_field.wgsl:134-147`). Both apply the same two vertex
culls by degenerating clip to `(2,2,2,1)`: OOB group id / `gcolor.a <= 0.01`, and the
per-group clip window on the pre-transform anchor y (`glyphVertex.js:301-307`,
`glyph_field.wgsl:149-155`). Both clamp the OOB read *and* cull, for the same stated
reason (robust storage access clamps rather than zeroing).

**Moving / recoloring / hiding.**

Web — nine typed setters, each writing its own columns into the `Float32Array` mirror and
marking one whole-row update range: `setGroupOffset` (`GlyphField.js:1567`),
`setGroupQuaternion` (`:1590`), `setGroupClipY` (`:1615`), `setGroupColor` (`:1648`),
`setGroupVisibility` (`:1660`), `setGroupAlpha` (`:1675`), `setGroupScale` (`:1687`),
`setGroupColorBlend` (`:1702`), plus `setGlyphGroupRange` (`:1636`) to re-point a slot
range at a different group. All funnel through `_touchGroup` (`GlyphField.js:918-926`),
which dedupes range-adds per upload cycle and bumps the version unconditionally — with a
comment recording the deadlock a conditional bump caused.

Native — the same writes, but open-coded inside `apply_verb` against `groups_cpu`, then
`write_group_row` → one 80 B `queue.write_buffer` (`glyph_scene.rs:2249-2255`).
`MoveGroup` writes col 0 (`:2442-2444`), `ScaleGroup` col 3.xyz (`:2456-2458`),
`TintGroup` col 2.rgb (`:2469-2471`), `SetHidden` col 2.w (`:2493`). **Never written:
col 1 (quaternion), col 4 (clip), col 3.w (colorBlend).** The shader reads all three;
nothing on the CPU side can produce a value for them.

**Bucket 1 — the seam that isn't there.** `glyphVertex.js:64-120` and `:122-149` are two
full seams (`registerByteSlotsNode`/`rebindByteSlots`, `registerGroupMaterial`/
`disposeGroupMaterials`) that exist for one reason: three's WebGPU backend keys its
bind-group cache on bound *textures* only, so replacing a storage buffer leaves every
built bind group pointing at a destroyed one, forever, with `material.dispose()` as the
only lever. Native holds `bind_groups: Vec<wgpu::BindGroup>` (`glyph_scene.rs:887`) and
rebuilds them when it wants to. Roughly 90 lines of the web's group/slot machinery has no
native counterpart because the problem does not exist. This is the port's point.

**Bucket 3 — no allocator.** Web has `createGroup`/`releaseGroup` with a free list, a
permanent dead group 0 at alpha 0 so a stale pointer cannot ghost, `_resetGroupRow` so a
reused row starts like a fresh one (`GlyphField.js:1516-1564`), and `_growGroupBuffer`
capacity doubling. Native builds `Vec<GroupRow>` once at stage
(`glyph_scene.rs:1195-1198`) and never allocates, frees, or grows.
Fine for a scene staged once; nothing to build a live scene graph on.

## 2. Draw structure — verified, and the judgment

**Web is not one draw.** It is one *mesh* — one `InstancedBufferGeometry` over the whole
arena, `MegaGlyphField` (`packages/glyph3d-core/src/MegaGlyphField.js:1-33`), with one
group per view. But `_cullRanges` (`MegaGlyphField.js:498-624`) runs every pass, in
`onBeforeRender`, with that pass's camera, and writes an
`IndirectStorageBufferAttribute` of 5-uint draw records — `{indexCount:6, instanceCount,
firstIndex:0, baseVertex:0, firstInstance: range.base}` (`:604-609`) — then
`geometry.setIndirect(attr, offsets)` (`:621`). Adjacent visible ranges coalesce into one
record within `MERGE_GAP = 4096` dead slots (`:514`, `:598-601`). Nonzero `firstInstance`
is load-bearing, and the web guards it at the substrate seam: `_detectIndirect`
(`MegaGlyphField.js:464-482`) checks for the `indirect-first-instance` device feature and,
lacking it, logs loudly and falls back to drawing the *full* field.

**Native is per-segment direct draws.** `cull_segments` (`glyph_scene.rs:393-465`) walks
the segment table and emits `(chunk, chunk-local range)` pairs; recording issues
`pass.draw(0..6, base..base+count)` per range (`glyph_scene.rs:3095`). The module header
(`glyph_scene.rs:52-63`) states why: on wgpu 30.0.1's Metal backend, **any indirect draw
with `first_instance != 0` rasterizes nothing** — verified by readback (args correct) and
per-offset A/Bs. That is exactly the hazard the web's feature check exists to detect, on
a platform where the feature reports present and lies.

**Judgment.** Native's approach is better here, not merely acceptable. Same cull, same
draw count, same blend order (segment ranges ascend in arena order — `glyph_field.wgsl:24-30`),
minus a 20 B/record buffer write and a submit per frame, minus the feature gate, minus an
entire class of silent no-op. The web's indirect path buys one thing native doesn't have:
run-merging across adjacent ranges. Native could add the same coalescing to
`cull_segments` for free, and should — its ranges are already chunk-major and ascending.

**Chunking (bucket 2).** Native splits the arena at
`max_storage_buffer_binding_size / 48` (`glyph_scene.rs:1201-1203`) with one buffer and one
bind group per chunk, and every slot-addressed operation carries the `slot / chunk_cap`,
`slot % chunk_cap` split (`:2242-2243`, `:2225-2226`, `:2366`, `:2787-2788`, and the
per-chunk split inside the selection pass at `:3156-3166`). Legitimate and correctly
threaded, but it is now the fifth place that division is hand-written; one
`fn locate(slot) -> (usize, u64)` would close it.

## 3. Picking

**Web — GPU ID pass, four channels.** `packages/glyph3d-core/src/picking/PickingSystem.js:1-33`:
each channel is a THREE render layer with its own first-fit u32 ID space — `glyph`
(layer 7), `grid` (8), `handle` (9), `group` (10) (`:54-59`). `_renderChannelPass`
(`:454-502`) swaps every registered mesh to its pick material, isolates the camera to the
channel's layer, clears to black (id 0 = miss), renders, restores in a `finally`.
`readPixelAsync` (`:510-524`) does an async 1×1 `readRenderTargetPixelsAsync`. ID =
`base + instanceIndex`, computed in the shader; `instancePickingId` exists only as a
harness mirror, and is a `Uint32Array` specifically so it cannot disagree with the shader
past 2^24 (`:397-405`).

The correctness property is structural: the pick material calls the **same**
`buildGlyphVertexTransform` the render material does (`glyphVertex.js:1-22`,
`PickingSystem.js:200-240`). Group scale, group quaternion, the clip window, width
compression, and emoji square-quad sizing are therefore identical by construction — the
file names the drift class it was built to retire. What it picks: the exact rasterized
glyph cell quad, depth-correct, front-most wins; empty space is id 0. Latency: one render
pass + a readback, ~a frame, amortized by a dirty latch and per-channel result caching
(`:96`, `:540-552`); consumers `markDirty()` between channels in one frame
(`app/client/CanvasInteraction.jsx:184`, `:334-406`).

**Native — analytic ray + CPU re-derivation.** `glyph_scene.rs:11-26` states the contract.
`pixel_ray` (`:1950-1980`) builds the ray **analytically in f64** from the camera's own
eye/forward/fov/aspect, deliberately not from the inverted f32 view-proj — the comment
records that at Fly's near=0.05 / far≈1.7e6 the inverse unprojects to w≈0 and *every*
windowed click returned MISS, and that even at far=20000 the angular error crosses the
0.8-unit acceptance at D≈50. That is a genuinely better ray than a matrix inverse, and
the reasoning is recorded with its repro.

Then: `ray_file` (`:1985-2013`) tests the ray against each file's live world AABB
(local AABB × group scale + offset, z = `off.z ± 1.0`), nearest wins; intersect the
`z = off.z` plane; undo group translate+scale to a local point; `ensure_pick_cache`
(`:1862-1917`) re-reads the file from disk, reloads the trie, re-runs the engine
(`repo.rs:598-609`) and cross-checks CPU fold row/col against the engine's ROW/COL lanes
on *every* fill; then a linear scan for the nearest record cell, accepted within 0.8 world
units (`:2095-2099`).

**What it buys (bucket 1/2).** Synchronous — the answer is available in the same call,
with no fence, no readback, no frame of latency, which is what makes the CLI op-stream and
offscreen repro work at all. And it resolves *further*: `PickGlyph` (`:206-227`) carries
record index, arena slot, row, col, **source line, byte offset, and the char** — the web
returns `{token, slotIndex}` and resolves to a character separately through the grid's own
layout query (`CodeGrid.js:1645-1740`). It is also self-verifying in a way the ID pass is
not: the fold cross-check runs on every cache fill and fails loud.

**What it costs.**

- *Bucket 3 (latent 4)* — `pick_ray:2041` states "group quats are identity" and intersects
  a plane. The shader applies a full quaternion (`glyph_field.wgsl:145-146`). The
  clip window (col 4) is likewise not consulted, so a clipped-away glyph stays pickable.
  Both are latent only because nothing writes those columns; the *first* write to either
  silently divorces pick from render. This is precisely the drift the web's shared
  transform builder was written to make impossible.
- *Bucket 3* — one channel. No grid/panel channel, no handle channel, no container-volume
  channel, and nothing that answers "what is under here" for non-glyph geometry.
- *Bucket 3* — no depth. `ray_file` takes the nearest AABB; a nearer file that is fully
  transparent at that pixel still wins. The ID pass is per-fragment.
- *Bucket 4* — the cache is a single entry keyed by `group_id` (`glyph_scene.rs:927`,
  filled at `:1863`). Alternating clicks between two files re-reads both files from disk
  and re-runs the engine on every click. The header's "microseconds" holds for the AABB
  test, not for the cache fill; the code's own comment says ~10 ms for a 10 MB file
  (`:2066`). A small LRU is the whole fix.
- *Bucket 4, minor* — `0.8` (accept radius) and the `± 1.0` z half-slab are magic. The web's
  pickable surface is the quad, exactly.

## 4. Highlight

**Confirmed absent.** `glyph_field.wgsl:20` lists "highlight tint/fill
(vAddedColor/vFillAmount)" under *Skipped vs the web*. `InstanceSlot` is 48 B / 12 lanes,
all spoken for (`glyph_field.wgsl:36-60`) — there is no highlight lane, no highlight
texture, and no `vAddedColor`/`vFillAmount` varyings anywhere in the native tree.

**What the web has.** Per glyph, RGBA8, where **the alpha byte is a mode carrier**:

- alpha 0 → **TINT**: additive on the ink, `vColor·cov + vAddedColor` (`GlyphField.js:399-425`).
- alpha > 0 → **FILL**: a background bar. `addK = 1 − cov` so cov=0 is pure fill and cov=1
  is pure ink; the quad spans the full advance cell so adjacent FILL cells tile into a
  seamless bar, one draw, no extra pass. The zero-curve `Discard` is suppressed when
  `vFillAmount > 0` (`GlyphField.js:337-343`) — otherwise the bar would gap at every space.

`encodeHighlightAlpha` (`GlyphField.js:1345-1348`) is the single encoder, shared by the
per-slot and bulk writers so they cannot drift, and it clamps a fill to [1,255] so a tiny
opacity never silently degrades to tint at the 0 boundary. Storage is the RGBA8 texture on
classic fields and the `instanceHighlight` **instance attribute** on byte-pipeline fields
(`GlyphField.js:1300-1332`), with a 4-byte update range per write. Bulk writers get raw
buffer access: `highlightBuffer(count)` + one `markHighlightDirty()`
(`GlyphField.js:1355-1375`) — that is how `TerminalGrid` projects per-cell ANSI background
colors every frame.

Consumers: `CodeGrid.highlightRange` / `highlightNode` / `clearLineHighlight` /
`clearAllHighlights` (`CodeGrid.js:1770-1821`), all addressing through the layout's
`slotForChar` so a highlight survives relayout by re-projection.

**Native's nearest thing is not a substitute.** The L4 selection path
(`glyph_scene.rs:1034-1069`, `:3108-3205`) draws the selected glyph quads into an
`Rgba8Unorm` mask target with the same glyph shader and blending off, then runs a
full-screen additive tint pass with one constant `SELECTION_TINT = [1.0, 0.85, 0.25, 0.45]`
(`:1052`). It is a screen-space post-process. It cannot express: per-glyph distinct
colors; more than one highlight at a time; the FILL mode at all (a coverage mask has no
ink where a space is, so a background bar is unreachable); a terminal's per-cell ANSI
background; or anything at all offscreen — `selection_fx`/`mask` are `None` on the copy
path, so captures never carry it (`:1001-1004`, `:3110-3113`).

**What would express it natively.** `InstanceSlot` already carries `_pad: u32` at byte
offset 44 (`glyph_field.wgsl:48`, `glyph_scene.rs:113`). That is exactly one packed RGBA8
highlight, in the existing 48 B stride, at no size cost: rename it `highlight`, unpack in
the vertex to `vAddedColor` (rgb/255) and `vFillAmount` (a/255), and carry the web's two
branches into `fs_main` — the `addK = 1 − cov` lerp, and the `curve_count == 0` discard
suppressed when `vFillAmount > 0`. The write path is `write_instance(ctx, slot, 44, ...)`,
identical in shape to the existing recolor. The one-glyph-per-write mode is then already
built; a range writer is the same run-coalescing loop `RecolorLine` already has.

**Bucket 4 — the name.** `verb_key` binds `KeyH` to `Verb::RecolorLine` and the doc
comment calls it "h highlight line" (`glyph_scene.rs:2618-2622`). It is not a highlight:
it overwrites the base `color` lane of every glyph on the row (`:2337-2384`). There is no
clear, no restore, and no separate layer — the original color is gone for the session
(the record rebuild reads `r.glyph_id()` etc. fresh from the engine but takes `packed` for
color unconditionally). A capability named after one it does not have is worse than an
absent one.

## 5. Per-glyph mutation — every hardcoded literal

| Site | Literal | Means |
|---|---|---|
| `glyph_scene.rs:2245` | `local * 48 + field_off` | `size_of::<GlyphInstance>()` |
| `glyph_scene.rs:2252` | `gid as u64 * 80` | `size_of::<GroupRow>()` |
| `glyph_scene.rs:2310` | `write_instance(ctx, slot, 24, …)` | `offset_of!(GlyphInstance, color)` |
| `glyph_scene.rs:2379` | `local * 48` | stride, again |
| `glyph_scene.rs:2382` | `insts.len() * 48` | stride, again |
| `glyph_scene.rs:2397` | `write_instance(ctx, slot, 0, …)` | `offset_of!(GlyphInstance, pos)` |
| `glyph_scene.rs:2420` | `write_instance(ctx, slot, 32, …)` | `offset_of!(GlyphInstance, advance)` |
| `glyph_scene.rs:2799` | `local * 48` | stride, again |
| `glyph_field.wgsl:34` | `GROUP_STRIDE: u32 = 5u` | the row width, hand-mirrored |

**Nothing ties them to the struct.** `layout_tests::glyph_instance_size_and_offsets`
(`glyph_scene.rs:3316-3341`) does pin the offsets — but as a **second hand-written table
of the same numbers**, checked against `encase`'s derived metadata. That guards
Rust-vs-WGSL agreement, which is real and valuable (`encase_bytes_match_bytemuck` at
`:3346-3377` is a genuinely decisive check). It does **not** guard `write_instance`'s
callers. Reorder a field and the test fails; the natural fix is to update the test's
expected table; `24` and `32` at the call sites are then wrong and everything is green.
Per this repo's own rule, ask what would have to break: the counterfactual here is "someone
edits the struct and the test table together", which is the *likely* edit, not an unlikely
one.

The fix is one line each — `std::mem::offset_of!(GlyphInstance, color)` and
`size_of::<GlyphInstance>() as u64` — after which the encase test still earns its keep as
the Rust↔WGSL guard, and the byte offsets stop being a second source of truth.

**How the web does the equivalent.** It has no byte offsets at all. Each lane is its own
typed `InstancedBufferAttribute`; a write is an index into the typed array plus
`addUpdateRange(slot*itemSize, n)`, and three derives the upload offset from `itemSize`:
`setGlyphColor` (`GlyphField.js:1391-1403`), `setGlyphHighlight` (`:1300-1332`),
`setGlyphColorRange` (`:1409`), `setGlyphPaletteRange` (`:1437`),
`moveGlyphPaintRange` (`:1261-1285`, which `copyWithin`s the paint lanes when a slot range
relocates). Group rows do use a column index — `(groupId * GROUP_STRIDE + col) * 4` — but
`GROUP_STRIDE` is imported from `glyphVertex.js`, the same module the shader's row schema
is documented and read in, so both ends move together in one file.

**Bucket 1 — the run-coalescing rebuild.** `RecolorLine` (`glyph_scene.rs:2316-2384`) is
better engineering than the naive version, and the comment names the exact bug it avoids:
the color field is strided 48 B apart, so a color-only range write stomps neighbors.
It rebuilds whole 48 B records from the pick cache, coalesces contiguous same-chunk slots
into runs, and issues **one `write_buffer` per run**. The web's `highlightRange`
(`CodeGrid.js:1770-1780`) loops `setGlyphHighlight` per glyph, and `_touchGroup`'s comment
(`GlyphField.js:911-915`) records that the upload path issues one `writeBuffer` per range
with no merging — so per-call, native submits fewer. Per-capability the web has
`setGlyphColorRange`/`setGlyphPaletteRange` for exactly this, so this is a nicer
implementation of a narrower surface.

**Bucket 1 — override bookkeeping.** `geom_overrides: HashMap<u32, ([f32;3], f32, f32)>`
(`glyph_scene.rs:925`) makes a record rebuild preserve earlier nudge/scale edits
(`:2346-2350`). The web's answer to the same problem is more general — `CodeGrid._decorations`
(`CodeGrid.js:195-203`) re-projects the caret and every registered overlay after every
fold — but native has no relayout, so the side map is the right size for the problem it has.

**Bucket 3 — what native cannot mutate.** No per-glyph highlight (§4). No per-glyph group
reassignment (`setGlyphGroupRange`). No text edit at all: `rederive_records` re-reads the
file from disk (`repo.rs:604`), so there is no editable buffer for an insert or a
backspace to write into, no cursor, no caret, and no `_relayoutPreservingCursor`
equivalent. That is correct for a demonstration and is the largest single body of missing
work if the native tree ever becomes an editor.

## 6. Culling and LOD

**Native.** `cull_segments` (`glyph_scene.rs:393-465`), CPU, ~1.3k segments:
6-plane Gribb-Hartmann positive-vertex test against a **2D** AABB with a hardcoded z of
`±1` (`:413`), then an LOD test — `glyph_px = px_scale / dist` at the AABB's nearest point
(conservative), against `LOD_MIN_PX = 1.0` (`:84`). Below threshold, the whole segment is
replaced by one flat backdrop quad tinted with the file's mean linear ink colour ×
`BACKDROP_GAIN = 0.7` (`:96`) — a gain fitted by matching post-LOD mean linear pixel value
against a pre-LOD render to ~3%, with the rejected candidates recorded. `hidden` skips a
group's glyphs *and* its backdrop (`:405-407`).

**Web — three mechanisms plus a fragment ramp.**

1. three's built-in per-object frustum cull, fed a **pushed, never computed** extent:
   `setLayoutExtent` (`GlyphField.js:2013-2032`) writes `geometry.boundingBox/Sphere`, and
   the layout owner states the box in the same breath as the layout change. Fields whose
   glyphs ride group offsets construct `frustumCulled: false` and never call it
   (`GlyphField.js:796-798`).
2. `_cullRanges` (`MegaGlyphField.js:498-624`): per-view **3D** `Box3` frustum test
   (`view.bounds × node.matrixWorld`), then a per-frame **instance budget** —
   `DEFAULT_INSTANCE_BUDGET = 2_000_000` (`:90`), survivors ranked by angular size squared,
   biggest first, `PROMOTION_HYSTERESIS = 1.15` (`:96`) so a boundary view does not flicker
   between glyphs and panel. A demoted view draws zero glyphs and falls back to the
   `PanelField` rectangle already being drawn. The budget is measured, not chosen, and the
   comment records the sweep that corrected the first (optimistic) derivation.
3. `OcclusionCuller` (`services/visual/OcclusionCuller.js:49-`): GPU occlusion queries with
   an 8-frame hold and a fault guard that disables culling once on a failed resolve.
4. Fragment side: the continuous minification ramp (dilate + soften over
   `[MIN_LO, MIN_HI]`, `GlyphField.js:355-380`) — which native **ported exactly**
   (`glyph_field.wgsl:271-282`) — plus a hashed **stipple-dither** alpha gate
   (`GlyphField.js:381-397`) that native did **not** port. The web comment is explicit that
   a hard `alpha < ε` discard makes whole glyphs, or coherent stripes of them, blink frame
   to frame; native has exactly that cliff at `glyph_field.wgsl:303`. Native's backdrop
   substitution covers most of the same regime, so severity is low — but the band between
   "still drawing glyphs" and "1 px/em" is where it bites.

**Which suits the native platform.** Native's is the right shape for its scene: a flat wall
of files at z≈0, ~1.3k segments, no allocation per frame, no readback, and a backdrop that
preserves the field's mean brightness rather than dropping it. The web's is 3D because its
scene genuinely is — grids at arbitrary poses, carrels, docked cameras. Neither should
adopt the other's. Two native gaps worth closing on merit: coalesce adjacent ranges the
way `_cullRanges` does (`MERGE_GAP`), and consider the web's *budget* idea as a second
gate — the LOD threshold alone cannot bound worst-case frame time when every segment is
genuinely above 1 px/em.

**Bucket 4 — the z defect.** `SegCull.min`/`max` are `[f32; 2]` (`glyph_scene.rs:255-256`).
The cull substitutes a constant `pz = ±1` (`:413`) and clamps the eye to `z ∈ [-1, 1]`
for the distance (`:427`). But `Verb::MoveGroup` takes a 3-component delta and writes
`g.cols[0][2] += d[2]` (`:2444`), and `sync_segment` (`:2260-2290`) re-derives only x and y
from `cols[0][0..1]` — **z is structurally unrepresentable in the segment table**. Move a
group past ±1 in z and the frustum test evaluates a box the geometry has left: the segment
can be culled while on screen (text vanishes) or drawn while outside it.

The pick path meanwhile *does* follow z — `ray_file` builds `off.z ± 1.0`
(`glyph_scene.rs:1996-2004`). So after a z-move, pick and cull disagree about where the
file is. This is not a web-parity question; it is an internal inconsistency. Fix is
either `min`/`max` as `[f32;3]` with `sync_segment` carrying `cols[0][2]`, or dropping
`d[2]` from `MoveGroup` and saying so.

**Bucket 4, minor — a baked font metric.** `GLYPH_CELL_AREA: f32 = (1229.0 / 2320.0) * 1.25`
(`glyph_scene.rs:269`) hardcodes an em advance the web forbids by convention ("all glyph
metrics come from the atlas at runtime"). The comment correctly scopes it — it feeds the
backdrop haze alpha and no layout decision — so severity is low, but it will be wrong the
moment the font chain changes, silently, in a value that was empirically fitted.

## Aside (web tree, not a native delta)

`GlyphField.js:66` and `:820` both still say the group row is "112 B" / "~112B". It is
80 B since the far-tier columns were deleted — `glyphVertex.js:48-50` says so explicitly.
Stale comments in the read-only tree; noted, not touched.
