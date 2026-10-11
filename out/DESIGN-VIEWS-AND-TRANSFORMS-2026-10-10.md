# Views over bytes, and a transform tree — 2026-10-10

> **Status: agreed direction; step 1 built in its transitional form (§ "Step
> 1: as built", 2026-10-10), the M2 measurement pending.** Worked out with Ivan on
> 2026-10-10. Inputs: the library-layout probe
> (`out/LIBRARY-LAYOUT-FINDINGS-2026-10-10.md`) and the GPU-hierarchy survey
> (`research/gpu-transform-hierarchies-2026-10.md`). Iterate one step at a
> time; each step says what it buys and what it costs.

## Decision: our own engine, with bevy as a source of techniques

We considered hosting the renderer in bevy (its 0.20 is on our wgpu 30 and
winit 0.30) and decided against it: its render internals churn every release,
its frame processing (tonemapping, HDR, pipelined rendering) would cost us the
byte-exact goldens and deterministic offscreen renders for a long while, and
it resolves transform trees on the CPU, uploading every moved descendant. So
the renderer becomes an engine in its own right, shaped like one: transforms,
visibility, materials, shaders, extensions. bevy (and Unity, Wicked, the
GPU-driven literature) is where we read techniques and adapt them, outside
any engine's core loop: its ECS and `bevy_transform` stay where they serve,
its scatter-upload WGSL is ours to subsume. Every wall we hit becomes a door
or a floor, the way it has so far.

## Why

The renderer is fast because it assumed a flat wall of files: a glyph's
position is a function of its row, item boxes are fixed at load, the tables
are built once, and group rows are allocated once and never freed. Visible
mode loads nearly every file of the Linux tree at a locked frame rate on an
M2 Air. That is the bar, and nothing below may lower it.

The target is wider than the wall: arbitrarily placed, nested, scaled and
moving groups of text, often code-shaped. Agent histories; labels that follow
their node; code scopes lifted off the page by AST depth; a word cloud whose
positions come from a runtime calculation. Every glyph must stay individually
addressable (the reason Instanced survived so long: recolouring or moving a
glyph was one write into indexed memory).

## The model: a particle system whose particles are data

| A game world | Here |
|---|---|
| Terrain / vegetation seeds | **Buffer**: bytes. Written once, appended to. |
| A spatial index (chunks) | **Line table**: built from the bytes, extended over an appended tail. |
| Procedural placement | **Layout**: a function of (buffer, byte). Nothing stored per glyph; run each frame for what is in view. |
| An emitter | **View**: a range of a buffer with a transform and optional style and pose. It emits its glyphs each frame. |
| The shared transform path | **Transform tree**: view → parent groups → camera. Every glyph goes through it. |
| Per-instance state | **Pose slab**: opt-in per view, indexed directly by byte offset. |
| GPU wind / particle animation | A compute pass writing poses or transforms, for bulk animation. |

Visible mode is already the emitter half: each frame it culls items and lines,
emits transient `DerivedSlot`s for what is in view, and draws them through
Derived's shader with the group's T·R·S. What is missing is the scene half:
a tree of transforms you can manipulate, views you can create, split, repoint
and free at runtime, and per-glyph state where it is asked for.

Glyph text is usually linear (a view's glyphs are a contiguous byte range),
and that is what keeps per-glyph state cheap: a glyph's instance index IS its
byte offset, so `pose[view.pose_base + (byte - view.start)]` is a direct
index, with no lookup and no sorted override table.

## A view

```
View {
  buffer, start, end   // a window onto bytes
  layout               // inherit: the buffer's own row/col coordinates
                       // rebase:  the range starts at row 0, col 0
                       // none:    position from the pose alone (free text)
  transform            // a group handle: where layout's local frame sits
  style                // colour spans, or none
  pose                 // slab base, or none
  flags                // hidden, LOD hints
}                      // ~32-48 B; a file is one view
```

Per glyph: position = transform ∘ (layout(byte) + pose offset); colour =
pose colour, else span colour, else default. Rotation and scale live on the
transform; a pose only offsets, scales and recolours within it (glyphs do not
rotate on their own; pages do).

What falls out:
- **Detaching needs no mask.** Split a file's view [0, N) into [0, a), [a, b),
  [b, N) and give the middle its own transform. Under `inherit` the three
  pieces lay out in the file's own coordinates (layout is a function of the
  byte, not of the view), so nothing moves until the middle's transform does.
  AST lift = nested scopes flattened into runs, each a view whose transform is
  its parent's plus a lift by depth.
- **Several views over the same bytes** is instancing: an agent card quoting a
  file region is a `rebase` view of the file's buffer, live, not a copy.
- **Appending** grows the buffer, extends the line table over the tail, and
  moves the newest view's `end`. The carrel's sliding window repoints a pooled
  view instead of rebuilding the scene.
- **Culling** is one box per view: the layout extent from the line table under
  the view's world transform, plus a posed view's slab bounds (kept by whoever
  writes the slab).

## Pose

12 B per glyph, opt-in per view: offset 3 × f16 (precision negotiable; f32 if
f16 deltas prove too coarse), scale as an 8-bit log code (e.g. 2^((c-128)/16):
about 1/256× to 256× in ~4% steps; nest under a scaled transform for more),
one flags byte, colour RGBA8. Instanced's slot is 32 B for comparison; a view
with `layout: none` and a full slab is instancing, at under half the memory.

Cost scales with what moves independently, not with glyph count: a scope lift
is one view per run, a label is one small view in a child group, a word cloud
is one view and group per word. A dense slab is for genuinely per-glyph motion.

## Groups: a pooled transform tree

**The group row is the wrong abstraction** (2026-10-10). Its 96 B bundle
things with different owners, update rates and composition rules: the
transform (layout and drags write it; composes down a tree), tint/alpha/blend
(verbs write it; alpha multiplies), clip (does not compose across frames; a
page's property) and background (a decoration). It also has no world
transform, because nothing has a parent. The probe measured the cost: the
animation's writer owns the whole row and erased tints and hides (E4). So a
node is a HANDLE into parallel tables, each with one writer and its own dirty
tracking — bevy's `Transform` / `GlobalTransform` / `Visibility` /
`InheritedVisibility` split:

| Table | Contents | Writer |
|---|---|---|
| topology | parent, depth-first position | CPU |
| local | translation + quaternion + uniform scale (32 B; composes closed) | layout, drags, animation |
| world | the composed transform (32 B) | GPU resolve pass only |
| appearance | tint, alpha, blend (~8 B) | verbs, styling |
| bounds | the subtree's box in its own local frame | CPU, refit along the moved node's ancestors |

Per-axis scale becomes a leaf-only, non-inherited factor (as Unity Entities
does). Clip and background move to the view or page.

**Step 1 design** (from the survey, consistent with the probe's numbers):
the CPU owns the tree and the allocator (free list, generation per slot, a
freed row quarantined for the frames in flight); it uploads only changed
LOCAL rows (a directory drag is one row; batches through a scatter pass
adapted from bevy's `sparse_buffer_update` WGSL, full upload above ~15 %
changed); one compute pass derives WORLD rows, one thread per node walking its
parent chain (depth ~11 for the Linux tree), over the dirty ranges of the
depth-first order only, skipped when nothing moved. First task: measure that
pass on the M2 and the Linux box. Transitional form: the pass writes the
existing group buffer's transform columns, so the draw path and every golden
stay as they are until the shaders read the new tables.

The rest of this section is the 2026-10-10 sketch it refines:

- **Rows from a pool**, freed and reused, addressed by handles carrying a
  generation so a stale handle can never hit a reused row (slotmap style).
- **Parent pointer.** A file's group's parent is its directory's group;
  dragging a directory writes one transform and everything below follows.
- **No small cap.** The ~4,095 figure today is not the group table's limit: it
  is the 12-bit per-glyph override lane in Derived's packed slot (item:20 |
  override:12), whose rows are never freed. The group table is indexed by a u32
  and bounded by buffer size (96 B a row today; a million rows ≈ 96 MB). With
  views, a range takes its view's group, so the override lane goes away.
- **Row contents are worth slimming** before they are pooled: today's row
  carries clip and background on every group.
- **Flattening: open.** CPU (bevy_transform already does it for the carrel;
  cost is every descendant row re-uploaded per moved ancestor) or GPU (a
  per-frame pass composing parent ∘ child level by level; one row written per
  drag). Decide from the library findings and the survey.

## Step 1: as built (2026-10-10)

`crates/glyph-scene-graph` (the tables, the upload policy, the resolve and
scatter shaders, `resolve-bench`) and `native/src/glyph_scene/nodes.rs` (the
renderer's group rows as nodes). The crate header credits its sources:
bevy (MIT/Apache-2.0: the component split, the two-level dirty bits and
sparse-upload policy of `sparse_buffer_vec.rs`, the scatter shader adapted
from `sparse_buffer_update.wesl`), Wicked Engine's per-node chain walk,
Unity Entities' post-transform scale, slotmap-style handles.

**What runs.** Handles are index + generation with a FIFO free list; a freed
slot waits three frames before reuse, and a stale handle is refused before
and after its slot is reused. The CPU keeps parent/child/sibling links and a
depth-first order (a subtree is one contiguous range), rebuilt once per frame
after a structural edit. Each table — local (32 B), post scale (16 B),
appearance (32 B), topology (8 B) — has its own setter and dirty set. Per
frame the dirty subtrees become merged depth-first ranges (at most 64, in a
uniform); dirty rows go up as coalesced `write_buffer` runs (up to 16 runs,
gaps of 2 rows absorbed), as one staged batch for the scatter pass beyond
that, or as the whole table above 15 %; then one compute pass (scatter
dispatches, then the resolve) runs, and nothing at all when nothing moved.

**Measured** (`resolve-bench`, RTX 5090 / Vulkan, this box; ONE run of five
iterations per case, median shown; a ballpark, not an A/B). GPU time is the
whole pass (scatter + resolve) between pass-boundary timestamps. "Bytes" is
everything queued: table rows, order span, the 528 B resolve uniform. Every
case's world rows were held to the CPU reference afterwards (worst relative
error 0).

| case | Linux topology (102,236 nodes: 95,954 files, 6,282 dirs; leaves ≤ depth 11) | synthetic (100,351 nodes, binary tree 10 deep, 96 leaves per bottom dir) |
|---|---|---|
| nothing dirty | no dispatch, 0 B | no dispatch, 0 B |
| one leaf moved | 0.003 ms GPU, 560 B, 1 node | 0.004 ms, 560 B, 1 node |
| a subtree moved | `drivers/` (40,494 nodes): 0.005 ms, 560 B | one half (50,175 nodes): 0.006 ms, 560 B |
| root moved | 0.009 ms, 560 B, 102,236 nodes | 0.010 ms, 560 B, 100,351 nodes |
| subtree reparented | 0.005 ms GPU; 0.33 ms host (order rebuild); 361 KB (order span) | 0.006 ms; 0.33 ms host; 302 KB |
| everything moved (every local row written) | 0.009 ms GPU; 0.35 ms host plan + 0.12 ms encode; 3.27 MB (full local table) | 0.010 ms; 3.21 MB |
| first flush (all new) | 0.010 ms GPU, 2.0 ms host, 9.4 MB | 0.010 ms, 2.6 ms host, 9.2 MB |

Those are with `--group-rows` (every leaf also writes a 96 B row of a group
table, the transitional form); without it the resolve is a few µs less. The
host side of a one-node edit is ~3 µs. The pass costs nothing worth
optimising at this size; the uploads are the cost, and a moved directory is
one 32 B row. On the M2 (unmeasured), from the repo root:

```sh
cargo run --release -p glyph-scene-graph --bin resolve-bench -- --tree <linux checkout> --group-rows
```

(`GLYPH_BENCH_TREE=<checkout>` works in place of `--tree`; `--subtree <dir>`
picks the moved directory, default `drivers`; `--iters N`, default 5.)

**Deviations from the design above, and why.**
- Appearance is 32 B of f32 (tint rgb, alpha, blend, flags), not ~8 B of
  RGBA8: the transitional group rows hold f32 columns, and only f32 makes
  them round-trip exactly. Pack it when the shaders read the table.
- The resolve walks every node's WHOLE chain from LOCAL rows; it never
  stops at a clean ancestor and reads that ancestor's world row. A node's
  world bits are then a function of the current tables alone, never of
  which edits came in which frame (deterministic offscreen renders depend
  on that), and resolving a clean node is harmless, which is what lets the
  host coalesce ranges into a 64-entry uniform. Measured cost of the full
  walk: ~10 µs for 100k nodes at depth 11.
- Alpha inherits (multiplies down the chain); tint and blend are the node's
  own. Under the identity root this changes nothing.
- The order is rebuilt in one O(n) walk after any structural edit (0.33 ms
  at 102k) rather than spliced; nothing restructures per frame yet.
- No bounds table yet: culling still uses the host-set boxes (E3); it is
  the next step's (pick, drag, cull from local boxes).
- The topology row carries an output group row (transitional), so the
  resolve can write the renderer's existing table directly.
- Reparenting keeps the LOCAL transform (bevy's `set_parent`): the subtree's
  world follows the new parent. A keep-world form is not written yet.

**The transitional integration covers**: every group row of every scene
(repo, text, transcript) is a node under one identity root; the resolve
writes columns 0-3 of the group table (clip and background stay the
host's); `move-group`, `scale-group`, `tint-group`, `tint-cycle`,
`hide-group` / `show-group` / `toggle-hidden`, and the `g` drag and grab
wheel on groups without a controller entity write the node tables; rows the
glyph verbs allocate become nodes; and rows the layout controller's bevy
sync writes (carrel zones, decks, `c` grabs, the library's animation) are
ADOPTED: their transform always, their appearance only when that writer
changed it. That last rule fixes E4 for the verbs: a moved file keeps its
`tint-group` and `hide-group` (`group-adopt-takes-unchanged-appearance`),
while a carrel that hides a card still hides it. Pixel-neutral: every golden
is byte-equal plain and under both equivalents, and the group verbs rendered
by the pre-integration binary and this one agree byte for byte in all three
field modes (29 of 30 cases; the 30th, `hide-group` then `show-group` in
visible mode, differs from run to run on the OLD binary too — six runs, six
hashes — a pre-existing nondeterminism, not looked into here).

**It does not cover**: the library's and the controller's own hierarchies
(still flattened by bevy on the CPU; one node per group, flat under the
root — wiring the library onto these tables is the next step); directory
nodes; the cull boxes and Visible item boxes (still one host write per
moved group, E3); `LIBTIME`'s `group_bytes` (in `glyph_scene/library.rs`,
not touched here) still mirrors the old span-or-rows upload rule, so it now
reports bytes that no longer go up; a GPU profiler scope for the pass (only
a CPU scope under `GLYPH_PROFILE`); freeing any group row (the glyph verbs'
override rows still accumulate). The CPU mirror (`groups_cpu`) is computed
with the resolve's arithmetic; under a non-identity parent the GPU may fuse
multiply-adds the CPU rounds separately, so pick and cull may differ from
the drawn frame by an ulp once groups nest.

**Proven by mutation**: `scene-graph-stale-handle-aliases`,
`resolve-compose-order-swapped` (the GPU test on a random 5,000-node tree
against the CPU reference; every golden group sits under an identity root,
where both orders agree, so no golden can see it),
`group-adopt-takes-unchanged-appearance`, and `resolve-group-tint-halved`
(pixel-ab: the draw path reads what the resolve writes).

## What exists today, and what changes

| Today | Becomes |
|---|---|
| Item = a file's bytes + `ItemParamsGpu` (origin, wrap, paging) + one group | View over a buffer; origin moves into the transform |
| Repo groups: one per directory (`repo/shelf.rs`), files placed by item origin | One group per file, parented to its directory's group |
| Item cull box: a world box set on the host at load (`visible_cull.wgsl` `item_visible`) | Derived per frame from the view's local extent under its world transform |
| Per-glyph overrides: a sorted (item, byte) table, 4,095 group rows never freed | Pose slabs, indexed by byte |
| Agent cards: host-staged absolute positions, Instanced only | Views over transcript buffers |
| Tables built once at load; any change rebuilds the scene | Pools: create, split, repoint, free, append |

Derived stays as the draw shader. Instanced retires once nothing needs
host-staged positions.

## Order of work

Each step is checkable by eye, and keeps every existing golden byte-equal
unless it says otherwise.

1. **Transform tree.** Pooled group rows with handles and parents; a group per
   file under its directory's group. Flattening per the decision above.
2. **Pick and drag.** A pick already resolves to (item, byte) and the item's
   group. Drag a file; drag its directory and everything inside follows.
   Witness: Ivan drags both on the Linux load in visible mode, frame time
   unchanged.
3. **Views.** Buffers split from items; `inherit` / `rebase` / `none`; split,
   repoint, append without a rebuild.
4. **Range ownership.** AST-lift demo: scopes as split views.
5. **Pose slabs.** Free views (`layout: none`).
6. **Agent cards** onto views, including the sliding window.
7. **A non-wall witness scene** (word cloud, animated) with a golden, so the
   wall cannot quietly become the only shape tested.
8. **Retire Instanced.**

## The guard rail

Alongside the goldens, every step reports a Linux-tree load and frame times on
the M2 and the Linux box against the step before (interleaved A/B, load
reported). A step that costs frame time says how much and why, and Ivan decides.

## Open questions

- Flattening: decided (GPU resolve, CPU-owned tree; above). Measured on
  NVIDIA at ~10 µs for the whole Linux topology (§ Step 1: as built); the
  M2 is not measured yet (the command is there).
- Overlapping views at arbitrary depths: draw order and blending. The wall
  rarely overlapped; a scene will. Sort per view, or order-independent
  transparency; research before choosing.
- Edits in the middle of a buffer shift every view and slab past the edit.
  Append-only (transcripts) is free; editing files will want something like a
  piece table later.
- Whether views may share bytes (my lean: yes, it is instancing) and whether a
  new view defaults to `inherit` (my lean: yes, so a split never moves
  anything). Not yet confirmed.
- LOD for scattered text: a line wash means nothing for a posed view.
