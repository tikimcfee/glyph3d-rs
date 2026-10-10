# Views over bytes, and a transform tree — 2026-10-10

> **Status: agreed direction, not built.** Worked out with Ivan on 2026-10-10.
> Open: how the tree is flattened (CPU or GPU), pending the library-layout
> findings (`out/LIBRARY-LAYOUT-FINDINGS-2026-10-10.md`, a delegate's probe)
> and the GPU-hierarchy survey (`research/gpu-transform-hierarchies-2026-10.md`).
> Iterate one step at a time; each step says what it buys and what it costs.

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

- Flattening: CPU or GPU (above).
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
