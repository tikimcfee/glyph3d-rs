# Views, slabs, poses — untangling the concepts (2026-10-11)

> **Status: design proposal; step A is built (2026-10-11, § 8), the rest is
> not.** Written
> against the tree at `c3a9de5` (Derived the default; Instanced staying as an
> opt-in field, decided 2026-10-11). It follows
> `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md` (steps 1 and 2 built), the
> library findings (E6, E19, E20), `out/VISIBLE-MODE.md` § M3 and the
> GPU-hierarchy survey. Every claim about the code cites `file:line` as read
> on this date; anything not read is marked **[inferred]**. No code, shader,
> manifest or fixture was changed for this note.

Ivan's brief: "there's a lot of acyclic-pointing and dependencies that could
be very carefully unwound and rewound to settle on what we're building here.
Between views and slabs and poses, I can see either a collapsing of ideas, or
an explicit formulation of them." This note does the unwinding on the code as
it is, then proposes the smallest set of concepts that covers the target
scenes, and a build order that keeps every golden byte-equal.

## 0. The short version

**Six concepts, each with one sentence:**

| Concept | One sentence | Identity |
|---|---|---|
| **Buffer** | An append-only byte sequence, with its line table and its default style spans; where text comes from. | handle |
| **Layout** | The pure function `(buffer, recipe, byte) → (x, row, wrap segment, page)` with a measurable extent; evaluated per glyph ahead of time (stored fields) or per frame (Visible); never an object. | none (a value: `ItemParams` is its recipe) |
| **Node** | A pooled transform in a tree (similarity + leaf scale + appearance + a subtree-bounds slot); the only thing that rotates or inherits. | generation handle |
| **View** | A byte range of one buffer, a layout recipe with a frame rule, a node, an optional pose slab, flags; the unit of emission, culling, picking and selection — what a GPU "item row" is. | pooled handle; GPU row index |
| **Pose** | A per-glyph delta (offset, log-scale, colour, flags) in its view's layout frame, keyed by byte from the view's start; the only per-glyph state besides the slot. | (view, byte) |
| **Field** | How views are evaluated into drawable slots — Instanced (absolute, 32 B), Derived (compact, 20 B + vertex derivation), Visible (per frame, no slot). A deployment choice, orthogonal to the five above. | per scene (per view: a possible future) |

Everything else today is a **representation** of one of these (CPU/GPU, or a
cache) or an **artefact** of how the code grew. The collapses:

- **Item, group id, node index, segment index, pick-file index, Visible item,
  slot-range owner** are one integer wearing seven names (§ 1.3). It becomes
  the **view**; the node is a separate handle the view carries.
- **Override** (Derived's 12-bit lane, Visible's `GlyphOverride`, the stored
  modes' `geom_overrides` map) is three implementations of a **pose**, and
  today's `set-glyph-transform` is a one-glyph view with its own node,
  created in WORLD coordinates and parented to the root rather than to its
  file — so a later file move leaves it behind (measured, § 2.2). Poses
  replace all three; the group lane goes away.
- **Zone**, **region**, **library sheet/mount/volume**, **card**, **deck** are
  nodes (some with a layout policy attached, some drawn as a box). Not
  separate concepts.
- **Segment** (`SegCull`) and **block** (`BlockCull`) are the view's cull
  record and its sub-ranges; they stop being concepts once a view has a node
  with bounds. "Wrap segment" keeps its name inside layout only.
- **Page** (a layout page of a long file) **becomes a view** (E6), which is
  what makes the library turn pages and not files.
- **Group row** (96 B) is a GPU representation of (node world ∘ appearance ∘
  clip ∘ background), kept transitional; clip and background move to the view.

**Splits:** `ItemParams` splits into the layout recipe (stays with the
buffer/view) and the origin (moves to the node). `ItemParamsGpu.group`
becomes the view's node reference, and stops being assumed equal to the
view's own index.

**The slab verdict (§ 5):** the slab as DATA — a per-view, byte-indexed,
CPU-owned pose table — is stable and is the right home for every per-glyph
edit. The slab as a 12 B lane the Derived vertex stage reads is **not
reachable**: `DerivedSlot` has zero spare bits, and the one lane that could
carry an index (override:12) does not carry the byte. The honest consumption
path is: Visible applies poses in its layout kernel (the override walk already
exists); Derived draws posed glyphs through a **side list in the Instanced
format** and blanks their main slots — Ivan's "a derived side and an
instanced side of the world", with Instanced the oracle by construction.

**First two build steps (§ 6):** (A) a `Views` registry that is the one place
item/group/node/segment are related, with nothing else changing — proven by
a permuted-group-id load rendering byte-equal; (B) the pose slab as CPU state
behind the existing glyph verbs, with Instanced as its first consumer and the
Derived/Visible "not representable" replies unchanged — proven by the slab
surviving a re-layout.

## 1. Inventory — what exists today, in code

### 1.1 The per-glyph records (the three fields)

| Record | Size | Lanes | Where | Who writes |
|---|---|---|---|---|
| `GlyphInstance` | 48 B | pos xyz, glyph, row, col, color, group, advance, height, flags | `crates/glyph-field/src/records.rs:22-33` | every host producer (HyperLayout host Pass 2, `text::stage_file`, agent staging) |
| `RenderSlot` (Instanced) | 32 B | pos xyz, glyph, color, group, advance, height | `crates/glyph-field-instanced/src/slot.rs:18-25` | device Pass 2, or transcode from 48 B |
| `DerivedSlot` (Derived, and Visible's transient) | 20 B | x; row:24 \| x_page:8; glyph:16 \| wrap:16; color; item:20 \| override:12 | `crates/glyph-field-derived/src/slot.rs:15-21`, lanes `derive.rs:26-30, 67-71` | device Pass 2; Visible's layout kernel per frame |
| Visible resident | 1 B/byte + 24 B/line + 24 B/seed + 128 B/item + 12 B/span + 16 B/override | `LineEntryGpu`, `SegmentSeedGpu`, `ItemGpu`, `ByteSpanGpu`, `GlyphOverrideGpu` | `crates/glyph-field-visible/src/lib.rs:111-143`, `tables.rs:81-127` | Pass 1 (lines, seeds), loader (items), verbs (spans, overrides) |

The Derived slot has **no spare bit**: 32 + (24+8) + (16+16) + 32 + (20+12).
Every per-glyph property it does not carry is derived: y and z from
`item_table[item]` (`derive_yz`, `derive.rs:115-141`; the shader at
`crates/glyph-field-derived/shaders/glyph_field_derived.wgsl:100-130`), the
quad width from `glyph_advances[glyph_id]` and the height from the constant
1.0 (`glyph_field_derived.wgsl:181-187`). Instanced reads width and height
from the slot (`glyph_field.wgsl`, `quad_w = inst.advance`, `inst.height`).
That is why `write_extent` is a no-op in Derived
(`crates/glyph-field-derived/src/field.rs:146-148`) and `scale-glyph` /
`nudge-glyph` y/z reply "not representable" (`native/tests/derived_verbs.rs:58-88`).

### 1.2 Everything else, as it exists

Legend for "key": the integer or handle that names one instance. "Writer" is
who mutates it after load.

| Concept (today's name) | What it is | Key | Owner / lifetime | CPU form | GPU form | Writer(s) | Where |
|---|---|---|---|---|---|---|---|
| **bytes** (a file, a block) | the source text | item index | `RepoFile.bytes` → moved into `VisibleStaging.item_bytes` for Visible; borrowed by the seam otherwise | `Vec<u8>` | Visible: resident byte buffer at `byte_base` | nobody after load (append is a rebuild) | `native/src/layout_hyper/visible.rs:14-19, 49-60` |
| **line table** | one entry per line: byte start, item, base row, glyph count, cols, width cells; seeds at long-line cuts | line index; per item `first_line..` | built by Pass 1; lives in `VisibleStaging` / the Visible field | `LineEntry` 24 B, `SegmentSeed` 24 B | the same bytes (`bytemuck::cast_slice`) | Pass 1 only | `native/src/layout_hyper/line_table.rs:1-43, 64, 92` |
| **ItemParams** | the layout RECIPE for one item: origin xyz, line height, z step, wrap width/mode, cluster mode, paging | item index | seam contract (`LayoutItem.params`); copied into `FileView.item` and `PickFileInfo.item` | f64 fields | `ItemParamsGpu` 64 B (16 words; `group` at word 14) | load; the panel's relayout rebuilds the scene | `native/src/layout.rs:163-188, 216-239`; `records.rs:204-223` |
| **item** (Derived/Visible) | the row of the item table a slot derives y/z and its group from | `item:20` lane | the field, for its lifetime; never freed | `Vec<ItemParamsGpu>` (`StagedText.item_params`) | binding 8 (Derived); `ItemGpu` 128 B (Visible) | load; Visible: `override_base/count`, hidden, local box | `field.rs:43-60`; `tables.rs:81-113` |
| **FileView** | one file's slot range, ink extent, offset, params, group id | file index | `RepoLoad.files`, consumed by `into_staged` | struct | — | `layout_shelf` writes `offset` | `native/src/repo.rs:314-336, 759` |
| **ItemPlacement** | slot range + record count + page/ink extents, per item | item index | returned by the seam | struct | — | the layout backend | `native/src/layout.rs:399-410` |
| **group row** | 96 B: offset / quat / tint+alpha / scale+blend / clip / bg | group id | `GlyphScene.group_buf` (`max(groups, 65536)` rows) + `groups_cpu` mirror; never freed | `GroupRow` | storage binding 2, every field's shader | GPU resolve (cols 0-3); load and `allocate_group_row` (whole rows) | `records.rs:41-43`; `glyph_scene.rs:177-180`; `setup.rs:63-73` |
| **node** | pooled transform: parent, local similarity 32 B, post scale 16 B, appearance 32 B, topology 8 B; world 32 B on the GPU | `NodeHandle` (index + generation) | `NodeTables`; freed slots quarantined 3 frames | `NodeTables` | local/post/appearance/topology/world buffers | verbs, library ease, drags; the resolve pass writes world | `crates/glyph-scene-graph/src/tables.rs:1-39, 61-64, 129-165`; `transform.rs:24-32, 70-98` |
| **GroupNodes** | the renderer's map group id → node; every group row is a node under one identity root | group id (= index into `nodes: Vec<NodeHandle>`) | the scene | `Vec<NodeHandle>`, `ext` snapshots | — | `push_row`, `translate`, `scale_by`, `set_tint`, `set_alpha`, `adopt` | `native/src/glyph_scene/nodes.rs:60-76, 108-119, 176-190` |
| **segment** (`SegCull`) | a file's WORLD cull box + slot range + backdrop tint + sub-blocks | segment index (= group id) | `CullState.segments`, with `local_min/max`, `em_scale`, `hidden` parallel | struct, 48 B | not uploaded (the comment at `cull.rs:24-29` says so) | `sync_segment` per moved group | `native/src/glyph_scene/cull.rs:69-80, 418-465`; `pick.rs:898-949` |
| **block** (`BlockCull`) | a 512-slot sub-range of a segment with its own box | (segment, block) | inside `SegCull.blocks` | struct | — | `sync_segment` | `cull.rs:40, 61-66` |
| **tree-cull bounds** | per library node, its subtree's segment box in its own frame; refit along moved ancestors | node slot | `TreeCull` (library loads only) | `Vec<Node>` with `Aabb` | — | `note_moved` → refit before the cull | `native/src/glyph_scene/tree_cull.rs:1-32, 182-221` |
| **region** | a padded copy of a directory node's bounds, drawn as a translucent box and pickable | `Plan::dirs` index | `RegionTable` (library, `[library] regions`) | `Vec<Option<Aabb>>` + `RegionGpu` 48 B | composed on the GPU from the node's world | `sync` after refits | `native/src/glyph_scene/regions.rs:1-45, 132-136, 201-225` |
| **zone** | a named layout container with a strategy (`dir:<path>`, `desk:active`), pinned or packed | string id | `LayoutStack.zones`; a bevy entity (carrel) or a directory node (library) | `LayoutZone` | — | controller / library | `native/src/layout_stack.rs:44-61, 101-115` |
| **library plan** | dirs (pre-order, local pos, footprint, books) and files (dir, order, slot, fit) | `Plan::dirs` / file index | `Library.plan`, recomputed per retarget | `DirPlan`, `FilePlan` | — | `plan()` | `native/src/library/plan.rs` (`Plan`, `DirPlan`, `FilePlan`) |
| **dir_user** | a directory's summed user drags in its parent's frame; every plan target = slot + this | `Plan::dirs` index | `Library` | `Vec<[f64; 3]>` | — | `move_zone` | `native/src/library/runtime.rs:109-112, 276-286` |
| **card** (agent turn) | a bevy entity with `GlyphGroupBinding { group_id, tint }`; since 2026-10-11 one ITEM per text block with origin = the block's y/z | group id | the carrel's ECS world; the whole scene is rebuilt when the window slides | entity + `ItemParamsGpu` | item row + group row | `sync_to_group_rows` → `adopt` | `native/src/spatial_scene.rs:62-65`; `agent_transcript/staging.rs:115-146, 1100-1118`; `windowed/app.rs:25-40` |
| **override** (Derived) | a 12-bit lane naming a row in a 4,096-word table that holds a GROUP id | override index | `DerivedField.group_overrides`; `next_override` increments per call, never reused | `Cell<u32>` | binding 10 | `write_group_id` | `crates/glyph-field-derived/src/field.rs:26-33, 153-168` |
| **override** (Visible) | a 16 B per-(item, byte) record: colour, x nudge, group index; the group index dedups by group, never freed | (item, byte) | `glyph_overrides` map on the scene is the truth; the field holds a copy | `GlyphOverride` | `GlyphOverrideGpu`, one sorted run per item | `set_visible_override` | `lib.rs:236-245, 372-383`; `gpu.rs:517-553`; `pick.rs:257-271` |
| **geom_overrides** (stored modes) | slot → (pos, advance, height) so a later `recolor-line` rebuild keeps a nudge | slot | the scene | `HashMap` | — | nudge/scale verbs | `glyph_scene.rs:203-206` |
| **selection** | one glyph slot, a slot range, or (Visible) an item byte range | slot / (item, start, end) | the scene; replaced by the next pick | enum | the mask pass re-lays the range out (Visible) | `apply_pick` | `native/src/glyph_scene/target.rs:97-106` |
| **style** | byte-range colour: `ByteSpan`/`Paint::ByteSpans` at load; `StyleRun` over the seam; Visible spans resident | (item, byte range) | stored modes: baked into slot colours; Visible: span table | `ByteSpan`, `StyleRun` | `ByteSpanGpu` 12 B (Visible only) | `--highlight`, seam updates, `recolor-line` | `layout.rs:328-345`; `seam.rs:72-75`; `lib.rs:139-143` |
| **field** | the three evaluation strategies behind one trait | `GlyphFieldMode` | the scene (`FieldHandle`), fixed for its lifetime | trait object + Visible concrete | the slot buffers above | — | `crates/glyph-field/src/field.rs:16-103`; `mode.rs:14-30`; `glyph_scene.rs:124-147` |
| **pose slab** | (design only) 12 B per glyph, opt-in per view | — | — | — | — | — | `out/DESIGN-VIEWS-AND-TRANSFORMS-2026-10-10.md:99-109`; **not in the code** |

### 1.3 One integer, seven names

At a repo load file `i` is item `i`, group `i`, node `nodes[i]`, segment
`i`, `PickContext.files[i]`, Visible item `i`, and the owner of slot range
`files[i].slot_base..`. Each of these is a separate table indexed by the same
number, and the code relies on their equality in at least these places:

- `FileView.group_id = index` (`repo.rs:759`) and the item table's
  `p.group = f.group_id` (`repo.rs:1032-1039`).
- `write_placements` writes `p.group_id` straight into the Derived lane,
  which is correct only because "the item's own group … is its index"
  (`crates/glyph-field-derived/src/field.rs:170-186`).
- `visible_item_of(gid)` probes `files[gid].group_id == gid` first
  (`pick.rs:243-255`); the fallback scan was 217 ms a frame at 29k files
  (E3b).
- `sync_segment` indexes `cull.segments[gid]` and `hidden[gid]`
  (`pick.rs:908-911, 1242-1245`); `TreeCull::new` is told "segment i being
  `seg_nodes[i]`'s (a repo load: file i, group i)" (`tree_cull.rs:224-225`).
- `GroupNodes.nodes` is a `Vec` indexed by group id, and
  `allocate_group_row` asserts `nodes.len() == gid` before pushing
  (`nodes.rs:66-67`; `pick.rs:858`).
- The agent staging pads the item table with identity rows "the load-time
  identity the Derived lane relies on" (`staging.rs:136-146`).
- `derive.rs:52-64` records that until 2026-10-10 the lane was ONE number
  read as both item and group, "which held only because every item's group
  id equals its index at load".

The 2026-10-10 lane split made the GPU stop assuming it; the CPU still does
everywhere above. This is the knot the rest of the note pulls on.

*(2026-10-11: every citation above was re-read at `ed25d4b` and is exact for
that tree; step A removed each one. § 8 says where each went, and lists nine
more places that relied on the equality which this list did not name.)*

### 1.4 Names that mean two things; things with two names

| Word | Meanings in the code |
|---|---|
| **item** | the layout recipe (`ItemParams`); the GPU table row (`ItemParamsGpu`, `ItemGpu`, `VisibleItem`, `VisibleItemSeed` — four forms); the file index; the Derived lane; the Visible edit key |
| **group** | the 96 B row; the id; the node (`GroupNodes`); "the file's group" (its transform); the override's target; `GlyphGroupBinding` (an ECS component) |
| **override** | Derived's lane and table (a group index per call); Visible's per-glyph record (colour, nudge, group); `geom_overrides` (the stored modes' nudge/scale memory) |
| **segment** | `SegCull` (a file's cull record); a wrap segment (the depth staircase step in a slot's lane); `SegGpu` / `SegmentSeed` (a cut of a long line in the Visible kernel) |
| **page** | a layout page (`page_rows`, `pages_wide`, `x_page`); a library page/sheet (one per FILE today, E6); an agent card's 2-page turn |
| **zone** | `LayoutZone` (controller); a library directory (`dir:<path>`); a carrel |
| **view** | `FileView` (a file's slot range); `ViewTarget` (the render target); the design's View (a byte range with a node); none of them the same |
| **mirror** | `groups_cpu` (the CPU copy of the group table); the "deferred mirror" of node worlds into it |

And the stale corner found on the way: `records.rs:2` still calls the group
row "the 80 B group-table row"; it is 96 B (`records.rs:35, 117`). The design
doc's table says "Repo groups: one per directory (`repo/shelf.rs`)"; the shelf
makes one group per FILE, tinted by directory
(`repo/shelf.rs:100-105`) — the design doc is wrong there, the code is right.

## 2. The dependency graph

### 2.1 Arrows as they stand

```
bytes ─Pass 1─▶ line table ─┬─▶ Visible: per-frame cull+layout ─▶ transient DerivedSlots ─▶ Derived shader
                            └─▶ (nothing else reads it)
bytes ─Pass 2─▶ slots (RenderSlot | DerivedSlot) ──────────────────────────────▶ draw
ItemParams ─▶ ItemParamsGpu[item] ─▶ derive_yz (Derived/Visible vertex stage: y, z)
ItemParamsGpu[item].group ──────────┐
override lane ─▶ group_overrides[k] ┴─▶ group row ─▶ T·R·S in the vertex stage
group row (cols 0-3) ◀─ GPU resolve ◀─ node local + appearance ◀─ { verbs, library ease ⊕ dir_user,
                                                                      controller bevy sync ─▶ adopt }
groups_cpu (mirror) ◀─ nodes.mirror / sync_mirror ─▶ read by pick, sync_segment, the HUD, verbs
placements.ink ─▶ FileView.ink ─▶ SegCull LOCAL box ─▶ × group ─▶ SegCull WORLD box (flat cull)
                                                      └─▶ tree-cull bounds (library) ─▶ regions (padded, GPU-composed)
                                  └─▶ Visible item LOCAL box ─▶ × live group row in the GPU cull
placements ─▶ PickFileInfo (local AABB + ItemParams) ─▶ CPU pick (re-derives records) ─▶ (group_id, byte) ─▶ verbs
```

Frame order, for the record (`native/src/glyph_scene/render.rs`): nodes
flush + resolve (`:35`) → faces from world rows (`:38`) → `field.prepare`
(Visible's cull and layout; `:178`) → selection mask (`:202`) → CPU cull,
tree or flat (`:245-246`) → regions compose (`:304`) → draws (`:589-591`).

### 2.2 Which arrows are essential

Essential (they would exist in any formulation): bytes → layout → cell;
recipe → layout; node → world → draw; cell ∘ world → pixel; extent ∘ world →
cull; pick → (view, byte).

Accidental (they exist because of how the code grew):

- **override → group row → node under the root.** A per-glyph transform
  allocates a 96 B row by copying the FILE's current row and overwriting its
  offset, quaternion and scale with the verb's absolute values
  (`pick.rs:1211-1214`), finds-or-pushes it by byte-equality over a linear
  scan of every row (`pick.rs:845-863`), makes it a node under the identity
  root (`nodes.rs:108-119`), and points the slot at it through a lane. So a
  posed glyph's transform is WORLD-absolute and a sibling of its file, not a
  child. **Measured (2026-10-11, this tree, vulkan-nvidia, offscreen):**
  `g-pick-repo`, `--focus-file alpha.rs --zoom 1 --color-mode flat`, pick
  alpha.rs row 3 col 8 (`'a'`, byte 35, slot 32), then
  `set-glyph-background 1010e0` followed by `move-group 6 0 0`. The
  background-quad glyph stays at its ORIGINAL screen position — inside the
  quad's 53×100 px box every one of its 5,300 pixels equals the unmoved
  `set-glyph-background` frame and none equals the moved-only frame — and
  the moved file carries a 1,119 px hole (x 1192–1236) where that glyph
  should be. Identical bytes in all three fields (Derived, Instanced,
  Visible agree on every frame of the experiment), so this is the model's
  semantics, not a field's defect: the override is a one-glyph view whose
  node has the wrong parent.
- **groups_cpu as a second writer.** The controller's bevy sync writes rows
  into `groups_cpu`, `adopt` reads them into the node tables (transform
  always, appearance only if the `ext` snapshot changed), then `mirror`
  rewrites the row from the tables (`nodes.rs:176-190`). Two sources of truth
  negotiated through a snapshot — the transitional cost of E4, named as such
  in the design note.
- **item index ≡ everything** (§ 1.3).
- **two culls, two LOD rules.** The stored modes cull world `SegCull` boxes
  on the CPU with an em of 1 world unit × `em_scale`; the Visible field culls
  LOCAL item boxes through the live group row on the GPU with
  `line_height × gscale.y` (`visible_cull.wgsl:284-297`). E2's remainder.
- **`ItemParams.origin_{x,y,z}` and the group offset both position a file.**
  The shelf puts the file's placement in the GROUP (`shelf.rs:100-105`) and
  leaves the item origin at 0; the agent staging puts the block's y/z in the
  ITEM origin (`staging.rs:126-134`) with an identity group. Two homes for
  one quantity, chosen per producer.

### 2.3 Cycles and knots

- **K1 — the identity knot** (§ 1.3). Not a cycle; a missing arbiter.
- **K2 — groups_cpu ⇄ nodes** (above). A real loop, bounded by the snapshot.
- **K3 — the detached pose.** The override is a view-with-a-node parented
  wrong (above, measured).
- **K4 — override tables that only grow.** Derived: one override index per
  `write_group_id` call, never reused (`field.rs:157-163`) — the review's
  "4,095 calls per lifetime". Visible: dedups by group (`gpu.rs:517-531`)
  but never frees. Group rows: never freed (`allocate_group_row` only
  pushes). Nodes: the pool CAN free (`NodeTables::remove`, `tables.rs:332`)
  but nothing in the scene calls it.
- **K5 — "layout needs bounds, bounds need poses, poses need views, views
  need layout?"** Checked: it is a chain, not a cycle. Layout never reads
  bounds or poses; the line table gives a view's extent per line
  (`cols`, `width_cells`, `base_row`) WITHOUT running the layout; a pose
  adds to the extent and the slab's writer knows that addition. The only
  way to make it a cycle is to let layout read the slab (the design's
  `layout: none`), and even then the pose IS the layout and the extent IS
  the slab's bounds — still acyclic. What does need stating: who owns a
  view's extent (§ 3.9).

## 3. Collapse or formulate — the candidate merges

Each with both sides and a verdict.

### 3.1 item ≡ view?

*For:* an item already is "a byte range (the whole file) + a layout recipe +
a group + a slot range"; M3 keys every Visible edit by `(item, byte)`; the
agent staging's "one item per block" (2026-10-11) is a view in all but name;
E6 wants one item per page. *Against:* `ItemParams` carries the origin, which
belongs to the node; an item's range is always a whole buffer; the index is
the GPU lane and is assumed to equal the group id.

**Verdict: the item becomes the view, after a split.** `ItemParams` →
(recipe: line height, z step, wrap, cluster, paging) stays with the view;
(origin x/y/z) moves to the node (the shelf already does this; the agent
staging will stop using the item origin as a position). The view gains
`start, end` into a buffer and a frame rule. The Derived item lane becomes
the view index — the lane is already 20 bits of "which table row", which is
exactly a view id. `ItemParamsGpu.group` becomes the view's node row, set by
the view, never assumed.

### 3.2 override ≡ a one-glyph view, or a pose?

*As a view:* today's `set-glyph-transform` literally builds one (a row, a
node, a lane). *As a pose:* the design excludes per-glyph rotation and says
glyphs inherit the view's node; a one-glyph view with its own node costs
96 B + node tables per glyph and a scan to allocate; the Visible override is
already a pose record (colour, x nudge) plus a group pointer.

**Verdict: a pose.** All three "override" mechanisms are the same thing with
different capabilities: colour (all three), x offset (all three), y/z offset
and scale (Instanced only, via `geom_overrides`), transform (Derived/Visible
via a group row). A pose record carrying (offset xyz, log scale, colour,
flags) in the view's layout frame covers every one, parented RIGHT (the view's
node carries it). `set-glyph-background` is the one verb that needs more than
a pose — a background quad is the group row's col 5 — and it is better served
by a per-view background than a per-glyph one; keep it as a pose flag ("draw
cell background") if wanted. `reset-glyph-group` becomes "clear pose".

### 3.3 pose ≡ a per-glyph node?

*For:* one mechanism for all transforms. *Against:* nodes carry topology,
dirty sets and a 32 B world row each; cost would scale with glyph count; a
pose has no children and no rotation by decision.

**Verdict: distinct.** A pose is a leaf delta applied after layout and before
the view's node; a node composes. The rule "cost scales with what moves
independently" is kept by making the thing that moves independently a NODE
(a word, a label, a lifted scope) and the thing that only wiggles a POSE.

### 3.4 slab ≡ a field?

*For:* a slab-posed view behaves like Instanced (per-glyph state). *Against:*
a field is how layout is EVALUATED and STORED; a slab is a TERM in the
position function (`layout(byte) + pose`) and every field has to apply it.

**Verdict: orthogonal.** Field = evaluation strategy (ahead-of-time absolute;
ahead-of-time compact + vertex derivation; per frame). Slab = per-view optional
input consumed by all of them. The honest wrinkle is HOW Derived consumes it
(§ 5).

### 3.5 zone, region, sheet, mount, volume, card, deck ≡ node?

*For:* every one is "a transform with children" plus one attribute: a zone
has a layout policy and a title (`LayoutZone`, `layout_stack.rs:44-61`); a
region is a drawn, pickable copy of a node's bounds (`regions.rs:8-22`); the
library's sheet/mount/volume are nodes already (`runtime.rs:1-6`); a card is a
bevy entity bound to a group (`spatial_scene.rs:62-65`); a deck is a node with
a slot law. *Against:* the controller's zones are bevy entities today and the
carrel still flattens through bevy (`interaction.rs:206-218`).

**Verdict: all nodes.** A node optionally carries a *layout policy* (how its
child views are placed: shelf, deck slot law, serpentine, pinned) and
optionally a *decoration* (a face, a region box). Policy and decoration are
attributes, not concepts. E19's `dir_user` offset generalises to **every
node**: `local = planned + user`, where `user` is what drags write and plans
never touch. That one rule is "user-set transforms stick".

### 3.6 segment / block ≡ view bounds?

**Verdict: yes, and they stop being named.** A view has a local extent (from
the line table over its range, plus its slab's bounds) and its node has
subtree bounds; the cull walks nodes and tests views. `BlockCull` (512-slot
sub-ranges, `cull.rs:40`) is a sub-view split for culling large views; it
survives as "a view may be culled in line ranges", which the Visible cull
already does per line.

### 3.7 page ≡ view?

**Verdict: yes** (E6). A long file's layout pages become views over the same
buffer with `inherit` layout and one node each (the sheet). Paging a book
then moves nodes, never glyphs.

### 3.8 line table ≡ ?

A cache of the layout function over a buffer, keyed by line. It belongs to
the **buffer** (not the view): several views over one buffer share it, and an
append extends it. Keep it, as a derived structure.

### 3.9 Who owns a view's extent?

Three contributors: the layout (lines' `cols`/`width_cells`/rows → a box in
the view's layout frame), the slab (max |offset| and max scale over its
posed glyphs — a running bound the slab writer keeps, recomputed on shrink),
and the node (its world). **Verdict:** the view owns `extent = layout box ∪
slab box` in its own frame and publishes it to its node's bounds slot; the
node tree does the rest. No arrow points back.

### 3.10 style — buffer or view?

A highlight sidecar and AST/LSP tokens are properties of the TEXT; "two views
over the same bytes" (the design's instancing case) might want different
colours. **Verdict:** spans live on the buffer by default; a view may carry
an override span table later. Flagged as Ivan's call (§ 7). **Superseded**
by his decision (§ 7.1): meaning on the buffer, look on the view.

### 3.11 What splits

- `ItemParams` → recipe (view) + origin (node), above.
- The `group row` → node world (tree) + appearance (tree) + clip and
  background (view attributes). The design said this; the code still holds
  all six columns in one row that two writers touch.
- `GlyphFieldMode` is already clean; `FieldHandle::Visible` leaking three
  concrete verbs (`glyph_scene.rs:118-127`) says the trait wants `prepare`,
  `record_after_glyphs` (the wash), `stats` and `set_spans` as trait methods
  the stored modes answer trivially.

## 4. The target scenes, walked through the model

For each: objects, bytes per glyph, what is written per frame when it moves,
what the cull and pick see, which fields can host it.

### 4.1 A growing agent history, turns placed by the user and sticking

- **Objects.** One buffer per session (append-only). One node per turn, a
  child of a deck node that applies the slot law (the carrel's cascade); one
  view per text block (range in the session buffer, `rebase` frame rule, node
  = the turn node or a child of it). No slab.
- **Append.** The buffer grows; Pass 1 runs over the tail and pushes line
  entries (`LineEntry.byte_start` is item-relative; appending lines of a
  growing buffer is a push, and `item_first_line` a bump); a new view (or the
  last view's `end` moves). Derived: Pass 2 over the appended range into the
  arena's tail; Visible: nothing beyond the table.
- **Sliding window.** Views are a pool: the N newest blocks get repointed to
  the N window nodes; nothing is rebuilt (today `apply_relayout` rebuilds the
  whole scene, `windowed/app.rs:25-40`).
- **A drag of a turn** writes 32 B (its node's local row) and adds to the
  node's `user` offset; the deck's re-plan targets `slot + user`, so it
  sticks (E19's rule, generalised).
- **Per frame when a turn moves:** one local row up, one subtree resolved;
  zero per-glyph work.
- **Cull:** per view, extent from its lines under its world; the deck node's
  bounds refit along the moved node's ancestors (O(depth)). **Pick:** ray vs
  view boxes → view → byte by re-deriving the view's range.
- **Memory per glyph:** Derived 20 B; Instanced 32 B; Visible none (1 B/byte +
  24 B/line resident).
- **Fields:** all three. Visible needs a view to be a LINE RANGE of a buffer
  (today an item is a whole file's lines; `first_line/line_count` already
  exist per item, `lib.rs:172-174`).

### 4.2 A label following a node

- **Objects.** A small buffer (or a range in a labels buffer); one view,
  `rebase`, node = a child of the labelled node. No slab.
- **Per frame when the parent moves:** nothing for the label; the resolve
  composes it (the pass walks whole chains, `tables.rs:33-39`).
- **Cull:** its box under its world, inside its parent's subtree bounds.
- **Fields:** all three; ~20 glyphs is nothing either way.

### 4.3 An AST function lifted out of its file and animated

- **Objects.** The file buffer; three views over it: `[0, a)` and `[b, N)`
  with `inherit` on the file node, `[a, b)` with `inherit` on a child "lift"
  node (parent = file node). Under `inherit` the split moves nothing — layout
  is a function of the byte, so rows and x are the file's own; only the lift
  node's local translation (z by depth) moves the middle.
- **Derived:** the split is one 4 B write per glyph in `[a, b)` — the item
  lane renamed to the lift view's row. That is today's `write_group_id` over a
  range, except the lane names a real view row rather than an override; the
  override table dies here. **Visible:** a view row with the range's lines;
  a view that starts mid-line needs the kernel to filter by byte range, which
  the mask mode already does (`LAYOUT_MODE_MASK`, `filter_lo/hi`,
  `tables.rs:43, 215-230`). **Instanced:** the same 4 B group write per glyph.
- **Animation:** one 32 B local row per frame on the lift node.
- **Cull:** the lift view's extent (its lines) under its world; the two file
  views' extents shrink to their ranges. **Pick:** ray → lift view → byte.
- **Memory per glyph:** unchanged; per view ~64-128 B; per node 88 B of
  tables.
- **Fields:** all three.

### 4.4 A word cloud

- **Objects.** A buffer of words (one per line); one view per word
  (`rebase`), one node per word under a cloud node; positions, scales and
  rotations are node local rows. No slab: positions are per WORD, not per
  glyph, and a word of eight glyphs would need eight equal offsets and could
  not rotate.
- **Per frame animating N words:** N local rows (32 B each): coalesced
  writes, the scatter pass past a few dozen, the whole table past 15 %
  (`scene-graph`'s policy). 10k words ≈ 320 KB a frame worst case; the
  resolve is microseconds.
- **Cull:** 10k view boxes (a flat scan is microseconds; the tree cull from
  the cloud node is the same cost at this depth). **Pick:** ray → word view →
  byte.
- **Fields:** Derived/Instanced trivially; Visible's item cull handles 29k
  items today (E3 numbers), each here with one line.

### 4.5 The library of a large repo with bounding regions

- **Objects.** One buffer per file; one view per layout PAGE (E6; today one
  per file), node = the page's sheet → mount; the hierarchy root → dir →
  volume → sheet → mount → view is already nodes (`runtime.rs:1-6`). Regions
  are the dir nodes' bounds drawn (already). No slab.
- **Per frame page turn:** the turning sheets' local rows; faces and regions
  compose on the GPU from world rows (already).
- **Cull:** the tree cull with per-node bounds (already); it becomes the
  only cull once every scene is a tree.
- **Memory:** per glyph unchanged. 95k files × ~5 nodes × 88 B ≈ 42 MB of
  node tables plus a view table — fine on both hosts.
- **Fields:** Visible for the kernel tree (Instanced cannot load it, E17).

### 4.6 A per-glyph recolour and nudge

- **Objects.** The view's pose slab: sparse on the CPU (a map, the truth —
  replacing `geom_overrides` AND `glyph_overrides`), dense or sorted on the
  GPU per field. A record: offset (3 lanes), log scale, colour, flags.
- **On edit:** one record; then per field — Instanced: rewrite the slot (as
  today's `write_position`/`write_extent`/`write_color`); Visible: the
  override record (the kernel already walks it per segment,
  `visible_layout.wgsl:475-483`); Derived: § 5's side list.
- **Per frame:** nothing unless the slab animates; then the records that
  changed.
- **Cull:** the view's extent grows by the slab's bound (§ 3.9). **Pick:**
  the CPU pick re-derives the cell and adds the pose from the CPU slab — the
  pick path consults `geom_overrides` for exactly this today
  (`glyph_scene.rs:203-206`), so this is a rename.
- **Memory:** 12-16 B per POSED glyph, sparse; zero for the rest.
- **Fields:** all three, with Instanced the oracle: a posed glyph in Derived
  or Visible must land where Instanced's slot rewrite puts it. Today's
  relation between the two is "within the vertex-stage tolerance already
  accepted" (max 2.4e-7 on the pagination grid, `derived_vertex_stage`; bit
  for bit for host-staged sessions, `derived_positions_match_the_staged_ones`),
  not bit-exact in general — say so in the witness.

## 5. Is the slab approach stable enough to subsume per-glyph control?

**As data: yes.** A per-view, byte-indexed, CPU-owned table of (offset,
scale, colour, flags) covers every per-glyph verb the renderer has
(recolour, nudge, scale, "transform" minus rotation, reset), is what "sticks"
(the CPU owns it; a re-layout re-applies it), replaces two maps and three
override mechanisms, and is the same key M3 chose. The precision arithmetic
holds: f16 has 10 mantissa bits, so an offset's step is 2^-10 × 2^⌊log2 |v|⌋ —
about 0.001 under 2 units, 1/16 at 64, 0.5 at 512. Within a view (deltas of a
few cells) f16 is fine; anything farther is a node's job. f32 costs 6 B more
per posed glyph and removes the question; since the slab is sparse (per
posed glyph, not per glyph), I would start with f32 and shrink later if
anything measures.

**As a GPU lane the Derived vertex stage reads: no, not as drawn.** The
stated obstacle is structural: `DerivedSlot` has no spare bit (§ 1.1), the
vertex stage has no byte to index a slab with, and the only lane that could
carry an index (override:12) is capped at 4,095 and indexes a GROUP table.
Three ways out, with their costs:

| Option | What changes | Cost | Verdict |
|---|---|---|---|
| **A. Re-purpose override:12 as a per-view pose index** | the lane indexes a per-view compacted pose list | at most 4,095 posed glyphs PER VIEW; the slot still cannot carry y/z or scale, so the vertex stage reads them from the list — fine; but the cap is a real cap | workable, capped |
| **B. Widen `DerivedSlot` to 24 B with the glyph's byte offset in its view** | `pose[byte - view.start]` becomes a direct index; selection and `GLYPH_G_DUMP` become slot-native | +20 % Derived memory (1.9 → 2.3 GB on the 95 M-glyph flagship); Visible unaffected (transient); every Derived test and the oracle's slot tier change shape | clean, pays memory |
| **C. A posed-glyph side list in the Instanced format** | posed glyphs are drawn by the Instanced pipeline from a small list computed as `layout(byte) + pose`; their main slots are blanked (glyph id of a blank glyph, e.g. the space, id 1 — **[inferred]**: the atlas's `' '` resolves to slot 1 with no curves) and restored from the pose record, which keeps the original glyph id (→ 16 B records) | the Derived side stays 20 B and untouched; the posed side costs 32 B per posed glyph; two draws; blending order changes only where a posed glyph overlaps another | **recommended** |

C is Ivan's "a derived side and an instanced side of the world" made
literal: the Derived side draws what layout alone places; the Instanced side
draws what a pose moved; Visible's kernel can emit a posed glyph straight
into the side list as it lays the line out (it already branches on the
override record per glyph, `visible_layout.wgsl:479-483`). Instanced is the
oracle by construction, because the posed glyphs ARE drawn by Instanced's
shader from Instanced's record. Cost scales with what moves. The goldens
cannot move (no golden has a pose; a blanked slot draws nothing, which is
what a culled slot draws).

**What remains open under C:**

1. **Scale pivot.** Instanced today scales the quad about the pen origin:
   `x ∈ [0, advance]`, `y ∈ [-h/2, +h/2]` anchored at `pos`
   (`glyph_field.wgsl`, the `aligned` line). A pose scale about the pen origin
   keeps Instanced the oracle with no new rule; a scale about the cell centre
   would need an equal rule in both. Recommend the pen origin.
2. **Advance under scale.** A scaled glyph's advance does NOT move its
   neighbours (layout is a function of the byte). That is the right default
   for "every glyph individually addressable" — the glyph grows over its
   neighbours — and must be said, because a text layout engine's instinct is
   the opposite.
3. **Blanking.** C needs a slot the main draw skips. A blank glyph id works
   if the atlas guarantees one; a per-slot "hidden" bit would be cleaner and
   does not exist. Visible needs neither: its kernel decides at emission.
4. **Determinism of the two draws.** A posed glyph drawn after the main pass
   blends differently where it overlaps un-posed glyphs than Instanced's
   single pass would. The witness has to compare at a pose where nothing
   overlaps, or accept a tolerance band where it does — the depth-write
   fringe class the repo already knows (`repo-down`'s 1,400 px, root
   AGENTS.md).
5. **Pick with poses.** The CPU pick re-derives records and must add the
   pose; it must also test the posed quad's box, not the cell's. Both are
   CPU-side and cheap; both need the slab's CPU form.
6. **Cull bounds under animation.** A slab that animates every frame moves
   its view's extent every frame; the view republishes its bound to the
   node (O(1) with a running max; O(posed) when the max shrinks). This is
   the one place "cost scales with glyph count" leaks back in, and it is
   bounded by the posed count, not the glyph count.

So: the slab subsumes per-glyph control **as the data model**, and the
field-side consumption splits by field — Visible in the kernel, Instanced in
the slot, Derived through a side list in Instanced's format. The 12 B
record and "indexed by the vertex stage" parts of the design do not survive
contact with the 20 B slot; nothing else in the design needs them.

## 6. Build order

Small steps, each pixel-neutral unless it says so, each with what it buys,
what proves it, and what it must not break. The culture: a step lands with
its mutation; goldens stay byte-equal plain and under both equivalents
(`build.toml:45`); Instanced is the oracle for anything that moves a posed
glyph.

**A. The `Views` registry (no behaviour change).** One CPU table, one row
per item today: buffer id, `start..end` (0..len), the recipe, the node
handle, the slot range, the local extent. Every `files[gid]`,
`groups_cpu[gid]`, `segments[gid]`, `hidden[gid]`, `nodes[gid]`,
`visible_item_of(gid)` goes through it; `ItemParamsGpu.group` is written
FROM it. *Buys:* the identity knot has an arbiter; the probe at
`pick.rs:251` and the padding at `staging.rs:136-146` go away. *Proves:* a
unit test that loads `g-pick-repo` with group ids permuted against item
indices and renders byte-equal to the unpermuted load (today that load would
draw wrong); mutation: the registry hands back the item index as the node.
*Must not break:* `repo-highlight` under all equivalents; `visible_verbs`'
(item, byte) replies.

**B. The pose slab as CPU state.** `PoseSlab` per view (sparse map;
record = offset f32×3, log-scale u8, flags u8, colour RGBA8, glyph id u16).
The glyph verbs write it; `geom_overrides` and `glyph_overrides` are deleted
in its favour; the pick reads it. Consumers as today: Instanced applies all
lanes through its slot writes; Derived applies colour and x; Visible applies
colour and x through its override record. The "not representable" replies
stay. *Buys:* one source of truth for every per-glyph edit; a `recolor-line`
rebuild re-applies poses from the slab instead of a side map. *Proves:*
`derived_verbs.rs` unchanged and green; a new test that re-lays a view out
(a `recolor-line` after a nudge) and holds the nudge; mutation: the slab
dropped on re-layout. *Must not break:* `visible_verbs` pixel coincidence.

**C. The Instanced side list for Derived and Visible (§ 5 C).** A posed
glyph's main slot is blanked; a `RenderSlot` is built from
(slot x, `derive_yz`, glyph id, colour) + pose and drawn by the Instanced
pipeline after the main pass; Visible's kernel emits posed glyphs into the
same list. *Buys:* y/z nudge and scale in Derived and Visible; the Derived
override lane and `group_overrides` table retire; `set-glyph-transform`
becomes a pose (no group row, no node, parented right — K3 closes).
*Proves:* a pixel test in the shape of `visible_verbs.rs`: `nudge-glyph 0 0.5 0`
and `scale-glyph 1.5` in Derived and Visible against Instanced at the
`repo-zoom` camera (changed-pixel sets coincide within the fringe band);
mutation: the blanking removed (the glyph draws twice); the oracle's Derived
slot tier unchanged (the main slots are the same bytes until a pose exists).
*Must not break:* every golden (no golden has a pose). *Measure:* the side
list's build cost on the M2 with 10k posed glyphs animating.

**D. Views over sub-ranges; the item lane is the view lane.** A view may
cover `[a, b)` of a buffer with `inherit`; splitting a file's view writes
one lane per glyph in the middle range (Derived, Instanced) or a view row
with a byte filter (Visible). Origin leaves `ItemParams` for the node (the
agent staging stops using the item origin as a position; `block_item` goes).
*Buys:* AST lift (§ 4.3) and library pages (E6, § 4.5). *Proves:* a split of
`alpha.rs` at a function boundary with the lift node at identity renders
byte-equal to the unsplit frame in all three fields (a golden-equivalent-style
check against `repo-zoom`); then the lift node moved: Derived == Visible ==
Instanced within the fringe band; hyper-oracle unchanged (layout is
untouched). *Must not break:* `derived_lane_limits` (views, not files,
count against `ITEM_MAX`).

**E. Pools: views and nodes freed and repointed; the carrel on nodes.** The
scene's `nodes: Vec<NodeHandle>` indexed by group id becomes the registry's
node column; `NodeTables::remove`/`reparent` are called by someone; the
carrel's zones and cards leave bevy for nodes with a slot-law policy and the
`local = planned + user` rule (E19 for every node); the sliding window
repoints views. *Buys:* appending a session without a rebuild (§ 4.1).
*Proves:* a session appended by one block renders byte-equal to the same
session loaded whole; a dragged turn survives a window slide; mutation: the
user offset forgotten on re-plan (the library's `library-drag-offset-forgotten`
twin). *Must not break:* the agent-session byte-equality between fields
(`derived_positions_match_the_staged_ones`).

**F. Retire the group row.** Shaders read the world and appearance tables
(packed) directly; clip and background become view attributes; `groups_cpu`
and `adopt` go (K2 closes). *Buys:* one writer per table, the design's
original table. *Proves:* every golden byte-equal (the resolve already writes
the bits the row held; `resolve-group-tint-halved` moves to the new binding).
This is the step with the most shader text and should come after D and E
have shown the view table is right.

**G. A non-wall witness scene.** A word cloud (§ 4.4) with a golden in all
three fields, so the wall cannot quietly become the only shape tested (the
design's step 7). Needs Ivan to adopt a frame.

Not in this order, and deliberately: the Visible-only syntax decision, the
`pass2_device.rs` split, and anything that changes a slot format
(option B of § 5 is the fallback if C fails its measurement).

## 7. Open questions for Ivan, ranked

1. **Posed glyphs via an Instanced side list (§ 5 C) versus a 24 B Derived
   slot (§ 5 B).** C keeps Derived's memory and makes Instanced the oracle by
   construction; B is cleaner and costs 20 % of Derived's memory. This is the
   one architectural fork in the note.
2. **Does a pose scale about the pen origin (Instanced's rule today) and leave
   neighbours where they are?** My recommendation is yes to both; it is a
   visible choice.
3. **Style spans: on the buffer, with a per-view override later — or on the
   view from the start?** Decides whether two views over one region can be
   coloured differently from day one.
4. **Should "user-set transforms stick" be the rule for every node (`local =
   planned + user`), not only library directories?** It changes what every
   future layout policy may write.
5. **Do you want per-glyph BACKGROUND to survive as a pose flag, or is a
   background a per-view attribute only?** Today it is the only reason a
   glyph verb allocates a group row.
6. **Adopt the view-table permutation test and a word-cloud golden as the
   witnesses for steps A and G?** Both need a human to look at a frame once.
7. **f16 or f32 pose offsets?** Sparse slabs make f32 cheap; f16 was chosen
   when the slab was dense.

### 7.1 Decided (Ivan, 2026-10-11)

1. **Side list (C).** It can be somewhat dynamic, so it needs a tight
   controller to maintain it: one owner that adds, moves and frees side-list
   records and blanks or restores the main slots, so no verb does it by hand.
2. **Scale about the pen origin, neighbours unmoved.** An offset pivot may come
   later.
3. **Style: meaning on the buffer, look on the view.** The buffer carries what
   the text IS (token kinds from syntax, AST, LSP). The view carries how it
   LOOKS: a theme mapping kinds to colours, plus span layers (highlight,
   search hits, selection). A pose colour wins last. So two views over one
   region can look different from day one, and the analysis runs once per
   buffer.
4. **User-set transforms stick for every node** (`local = planned + user`):
   "if a user does something, you don't undo it."
5. **Per-glyph background stays, as the view's span layers.** Highlighting a
   subsection IS per-glyph background, so dropping it would need a new
   abstraction anyway. Highlights become rectangles a view's span layer
   draws (the editor shape, and the regions box pipeline's), not a group row
   per glyph. **A posed glyph's background travels with it**, because it is
   the glyph's background. What that asks of the build:
   - *One mechanism draws every background.* The span layer reads the view's
     pose slab, cuts a highlight's run around each posed glyph and draws that
     glyph's rectangle at layout + pose, under the same pen-origin scale. The
     cell a posed glyph left shows no background. Cost: one extra rectangle
     per posed glyph inside a highlight. The side list stays ink-only, with
     no background lane.
   - *Coplanar draw order.* Today the background is painted inside the
     glyph's own fragment and cannot depth-fight its ink. A rectangle pass
     sits on the text's plane, so it needs a stated order: rectangles first,
     depth test on and depth write off, then glyphs. The blend arithmetic is
     the same, but the GPU may round it differently, so the witness is
     today's in-shader background frame against the rectangle-layer frame,
     with any difference measured before it is accepted. (The library
     regions' depth order against the glyph pass is unwatched too; one
     answer should serve both.)
   - *Visible needs span x-extents.* It stores no records, so a span's
     rectangles come from a CPU re-derivation of the highlighted lines (the
     pick path already does this per item), or later from the layout kernel.
     CPU first, measured.
   - *Deferred:* block and column selection (per-row column ranges; an
     editor feature, the same rectangle layer later).
6. **Yes to both witnesses**: the permutation test (landed with step A, § 8)
   and a word-cloud golden for step G.
7. **f32 pose offsets.** Simplify ahead of time.

## 8. Step A: as built (2026-10-11)

Commits `ee84ffc` (the registry) and `832e156` (the `gid` → `group_row`
rename in `pick.rs`), on `ed25d4b`. `cargo glyph test`: all 15 gates green,
378 tests, every golden byte-equal plain and under both equivalents
(vulkan-nvidia).

**The registry** is `native/src/views.rs`. A `View` is what a producer
registers: `buffer` (its producer's buffer index), `bytes` (the whole buffer
today), `recipe` (an `ItemParamsGpu` whose `group` lane is never read),
`group_row`, `segment`, `slots`, `local_min`/`local_max`. `Views` holds the
rows and three reverse indices built once in `Views::new`: the views drawn
with each group row (`views_drawn_with`), the group row a segment's views
share (`group_row_of_segment`, `None` for a cover segment over several rows
or a segment no view uses) and, derived from those, `segments_following(g)`
(the segments a group edit re-derives and hides). Each view's node is
recorded by `attach_nodes` once the scene has made its group rows nodes.
**A view's index is its item row** — the GPU item table row, the Derived
item lane, the Visible item, `PickContext::files[view]`, the layout's
per-item outputs — and that is the one equality the code keeps, by
definition. Everything else is a field, read through an accessor.

**Writers.** The three producers register one view per item:
`RepoLoad::into_staged` (one per walked file, segment = the file's,
`recipe` from its `ItemParams`, extent = `segment_local_box`),
`stage_agent_session_with_options` (one per text block, in staging order;
group row, segment, slots and extent are its group's) and `text::stage_file`
(one per copy, all under the one cover segment). `StagedText::views`
replaces `StagedText::item_params`; the scene keeps it as
`GlyphScene::views`.

**Readers, and where each § 1.3 equality went:**

| § 1.3 site (at `ed25d4b`) | Now |
|---|---|
| `FileView.group_id = index` (`repo.rs:759`); `p.group = f.group_id` (`repo.rs:1032-1039`) | `FileView.group_id` is the loader's group row (`group_rows_of_files`, `repo.rs`); the item table is `Views::item_table`, the one place the group lane is written (setup's `FieldResources`, and `into_staged` for the Visible items) |
| Derived `write_placements` writes `p.group_id` as the item lane (`crates/glyph-field-derived/src/field.rs:170-186`) | `GlyphPlacement` gained `item` (the view); Derived writes `item_lane(p.item)`, Instanced still writes `group_id` (`crates/glyph-field/src/records.rs`) |
| `visible_item_of(gid)`'s probe and scan (`pick.rs:243-255`) | gone: `PickHit` carries the `view`, and `visible_item(view)` is a mode check returning it (`pick.rs`) |
| `sync_segment` indexing `segments[gid]`, `hidden[gid]` (`pick.rs:908-911, 1242-1245`) | both walk `Views::segments_following(group_row)` (`pick.rs`, `sync_segment` and `Verb::SetHidden`; the Visible side hides every view drawn with the row) |
| `TreeCull::new`'s "segment i being file i, group i" (`tree_cull.rs:224-225`) | setup passes each segment's node and staged colour through `group_row_of_segment` (`setup.rs`) |
| `GroupNodes.nodes` by group id; `allocate_group_row`'s `nodes.len() == gid` (`nodes.rs:66-67`; `pick.rs:858`) | kept, and correct: both tables are keyed by group row and no view enters (said so at both places); `GroupNodes::file_nodes` — the one method that read file `i` as node `i` — is deleted |
| the agent staging's padded item table (`staging.rs:136-146`) | gone: one item per block, dense (`set_block_item` and `session_item_params` deleted); the Derived host transcode asks `FieldResources::item_of_group` (`Views::item_of_group_rows`) which item a host record's group row belongs to |
| `derive.rs:52-64` (history) | unchanged; it is history and still true |

**Nine more places relied on the equality** and § 1.3 did not list them:
`CullState::new` read row `i` for segment `i`'s local box, em scale and
staged tint (`cull.rs:491-515`); `into_staged` read Pass 2's per-item tints,
blocks and emoji pairs by group id (`repo.rs:896, 912, 936`); the Derived
host transcode wrote a host record's group id as its item and recovered
the wrap and column page against `item_table[group]`
(`crates/glyph-field-derived/src/upload.rs:38-56` — the real reason for the
staging's padding); setup attached the library's files and built the tree
cull's segment nodes and colours by group order (`setup.rs:79, 373-374`);
the layouts return group rows in file order, which the loader used as the
table (`shelf.rs:105`, `algorithm.rs:259`, the library's `seated_rows` —
now scattered by `GroupRows::table`); the file cards' `GlyphGroupBinding`
was the file index (`layout_stack.rs:290`, `algorithm.rs:235`); the glyph
verbs passed the group id where `write_group_id` takes the item
(`pick.rs:1198, 1217, 1231`); every pick-record lookup searched by group id
(`ensure_pick_cache`, `pick_ray`, `selection_from_hit`, `style.rs:35`,
`interaction.rs:175`, the UI probe's rows); and the library verbs used the
picked group id as the library's file index (`glyph_scene/library.rs:38`).

**The witness** is `native/tests/view_registry.rs`.
`GLYPH_PERMUTE_GROUP_ROWS=1` makes the repo loader file `g-pick-repo`'s five
files under group rows 5..1 in reverse, with row 0 a spare identity row
(`repo::group_rows_of_files`), so no view's group row is its index and the
table has a row no view draws with. Four cases — the `repo-wide` frame, the
`repo-highlight` frame, a verb chain from the whole-field camera
(`hide-group` long.md; `recolor-line`, `set-glyph-background` alpha.rs;
`tint-group`, `move-group` sub/deep.py) and `--layout-mode library` — each
render byte-equal to the unpermuted load, in Derived, Instanced and Visible
(library: Derived and Visible); the verb chain is held to move pixels
(~91,800 against `repo-wide`) and every pick in it to name a group row that
is not its view. It needs no adopted frame (open question 6): it compares
two loads of the same tree. **On the parent tree it fails** — measured on
`ed25d4b` with only the loader hook added, vulkan-nvidia: repo-highlight
Derived 71,193 px differ (the sidecar's placement writes put each glyph in
another item's rows), verbs Derived 493 px and Instanced 264 px (hide and
move re-derived another file's segment), library Derived and Visible
371,375 px each (each file under another file's mount); the other six
pairs were byte-equal, the static load being correct wherever the GPU had
already stopped assuming (2026-10-10's lane split). The mutation
`view-registry-node-is-the-index` makes `Views::group_row` return the view:
under `cargo glyph prove` the gate reddens on the registry's unit test and
`derived_positions_match_the_staged_ones` (the failing lib binary stops
`cargo test` before the integration tests), and by hand the witness
reddens on 10 of its 11 pairs (46,727 to 349,373 px; only repo-wide under
Instanced stays equal, its slots' group lane being the loader's).

**Renames, within the files touched:** `PickHit::group_id` → `view`;
`PickCacheEntry::group_id` → `view`; `visible_item_of(gid)` →
`visible_item(view)`; in `pick.rs` every `gid` → `group_row` (`new_gid` →
`new_group_row`) and `sync_segment`'s `i` → `segment_index`; the loader's
local `views: Vec<FileView>` → `file_views`; `block_item` lost its group
parameter. Pick lines print `view=… group=…` (the pick oracle reads
`row=`/`col=`/`rec=` only).

**What the note had wrong or loose.** Nothing in § 1.3 was wrong for its
tree; it was a third of the list. The "Proves" of step A promised a test
that today's code "would draw wrong": true, but not everywhere — the
static repo load already drew right in all three fields, because the
slot's group lane comes from `LayoutItem::group_id` and the Derived lane
had been split; what drew wrong was every CPU path that edits after load
and the library. And `ItemParamsGpu.group` written from the registry
needed a second thing the note did not foresee: the group → item map for
host records, because `GlyphInstance` names its group and not its item.

**Left for later, deliberately:** `PickFileInfo::item` (the f64 recipe)
beside the registry's GPU recipe — they fold together at step D's
recipe/origin split; `FileView` keeps its name (it is the loader's per-file
layout record); `bytes` is the whole buffer and `buffer` a producer index,
not a handle (step D); `GroupNodes.nodes` stays the node column (step E);
`geom_overrides` stays keyed by slot and `glyph_overrides` by
(item, byte) (step B); the agent staging still puts the block's y/z in the
item origin (step D) and keeps its segments one per group row, empty ones
included.
