# Library layout: findings (2026-10-10)

`--layout-mode library` is a probe. It ports the retired JS renderer's
"library" scheme and its Book carrier so that, before the ownership redesign
(byte buffers, item views over byte ranges, pooled groups, optional per-glyph
poses), there is one layout on today's code that is not a flat wall: files
scaled by a contain-fit, stacked in depth, nested in a parent's frame, and
animated. This note records where today's architecture held, where it bent,
and where it broke, with numbers. Code: `native/src/library.rs` (+ `book`,
`plan`, `runtime`), `native/src/glyph_scene/library.rs`; dials in
`[library]` of `config/defaults.toml`.

Host for every number: the Linux box (RTX 5090, vulkan-nvidia), offscreen,
1600x1000, release build. CPU figures are medians over the animated frames
of one run (`GLYPH_LIBRARY_TIMING=1`, script below); single runs, not
interleaved A/Bs, so read the small ones (< 0.1 ms) as orders of magnitude.

## What was built

- Every file is a book: a uniform contain-fit (never skewed, capped at
  `max_upscale` = 4) of its ink box onto a `page_w` x `page_h` page (60 x 80
  world units; the JS 900 x 1200 scaled to about one 100-column file page).
- Each directory with files has a volume. Stack `z` (default) pages it as a
  rolodex deck (page order, `gap` = 3 deep per page) or lays it open as a
  splay grid (`Book.splayGrid`, head lifted); `x` shelves the books abreast,
  `y` piles them. Sort by name, size or ext, reversible. Child directories
  pack serpentine under their parent's stack, `depth_z` = 32 further back,
  in the parent's frame. Single-child directory chains compress.
- Transform hierarchy on the existing bevy_ecs spatial scene: library root
  -> directory -> volume -> sheet (slot) -> mount (fit scale + centring) ->
  file card (`GlyphGroupBinding`, identity). The file's GroupRow is its
  flattened world transform, written by the existing `sync_to_group_rows`.
  One group per file (group id = file index), exactly as the shelf. The
  group-row and slot formats are untouched.
- Runtime: verbs `page-next|page-prev|page-first|page-last|page-to N`,
  `form deck|splay|toggle`, `library-stack x|y|z`,
  `library-sort name|size|ext [reverse]` (no pick needed; paging and form
  address the picked file's volume, else every volume). Windowed keys
  `]`/`[` (and `n p . ,` and the arrows), Home/End, `v`. Every change is a
  retarget: the plan is recomputed and every directory and sheet eases from
  its live transform (`1 - e^(-lerp*dt)`, lerp 9/s, dt clamped to 0.1 s).
- Page faces: one scene-mesh quad per sheet, on the page plane.

Unit tests (`library::tests`, 13) hold the arithmetic to the JS test numbers
(`tools/contenttree.test.mjs` tests 35-41 in the JS repo: splayGrid shapes,
slot positions `[-12,0,8] [12,0,0] [-12,-36,0]`, deck z `0, -gap`, shelf x
`-25 0 25`, pile y `-15 -50`, footprints `20x30x10`, `70x30x0`, `20x65x0`,
child tier one `depth_z` back and below, sort orders, load-order
independence) and the scene binding (group rows equal the composed plan; a
page turn syncs only its volume's files and settles on the slot law).

## Group counts per load (one group per file; no limit hit)

| corpus | files = groups | dirs | volumes | ECS entities* | load `layout` phase |
|---|---|---|---|---|---|
| `native/fixtures/g-pick-repo` | 5 | 2 | 2 | 25 | — |
| this repo | 276 | 72 | 68 | ~1,250 | 2 ms |
| the retired JS repo, as checked out | 564 | 54 | 51 | ~2,370 | 1 ms |
| a crates.io source tree (`~/.cargo/registry/src`, 451.6 MB, 434 M glyphs) | 29,377 | 5,804 | 5,767 | ~129,000 | 57 ms (shelf: 7 ms) |

\* dirs + volumes + 4 per file (sheet, mount, card, face) + root.

The group buffer is `max(groups, 65536)` rows of 96 B, so 29,377 groups fit
in the default 6 MiB with room for the per-glyph group verbs. The Derived
slot's 20-bit item / 12-bit override lane is not touched: the library
allocates no group beyond one per file. The JS repo is no longer the 97 MB
flagship the root AGENTS.md quotes (1,306 files); this checkout walks to 564
files, 9.4 MB.

Fit scales, crates.io tree: min 0.0086, median 0.5047, max 4.0 (623 files at
`max_upscale`). The minimum is a file whose layout fans into many
side-by-side pages; see E6.

## Per-frame cost of animating N groups

`form toggle` moves every directory and sheet (the footprint changes);
`page-next` with nothing picked turns every volume of two or more pages.
CPU stages per animated frame: `ease` (the easing loop), `propagate`
(bevy transform propagation + scene-mesh re-extraction), `sync`
(`sync_to_group_rows`), `upload` (`write_group_rows`), `seg` (`sync_segment`
per moved group: CPU cull box, sub-file blocks, and in Visible the item box).

| corpus / verb | mode | groups/frame | ease | propagate | sync | upload | seg | CPU total |
|---|---|---|---|---|---|---|---|---|
| this repo / form | instanced | 276 | 0.003 | 0.023 | 0.003 | 0.002 | 0.016 | 0.048 ms |
| JS repo / form | instanced | 564 | 0.005 | 0.040 | 0.005 | 0.004 | 0.027 | 0.080 ms |
| JS repo / form | derived | 564 | | | | | | 0.090 ms |
| JS repo / form | visible | 564 | | 0.049 | | | 0.509 | 0.608 ms |
| crates.io / form | instanced | 29,377 | 0.33 | 4.82 | 0.21 | 0.12 | 1.70 | 7.20 ms |
| crates.io / form | derived | 29,377 | 0.33 | 4.59 | 0.21 | 0.12 | 1.70 | 6.96 ms |
| crates.io / form | visible (before E3b) | 29,377 | 0.36 | 6.01 | 0.22 | 0.14 | **217.3** | **224.1 ms** |
| crates.io / form | visible | 29,377 | 0.36 | 6.06 | 0.22 | 0.15 | 25.2 | 31.9 ms |
| crates.io / page-next | instanced | 28,016 | 0.32 | 5.24 | 0.21 | 0.13 | 1.74 | 7.66 ms |
| crates.io / page-next | derived | 28,016 | 0.31 | 4.68 | 0.21 | 0.12 | 1.75 | 7.09 ms |
| crates.io / page-next | visible | 28,016 | 0.32 | 5.86 | 0.21 | 0.13 | 22.7 | 29.3 ms |
| crates.io / page-next, ONE volume (11 files) | visible | 11 | 0.087 | 2.15 | 0.012 | 0.003 | 0.010 | 2.27 ms |
| crates.io / form, no page faces | instanced | 29,377 | 0.33 | 3.21 | | | | 5.50 ms |

Bytes queued per animated frame, crates.io tree, everything moving: group
rows 2,820,192 B in ONE `write_buffer` (the whole table: `write_group_rows`
uploads the min..max span when more than 8 rows move); Visible item boxes
705,048 B in 29,377 separate `write_buffer`s of 24 B; page-face mesh
instances 2,350,160 B (29,377 x 80 B, re-extracted and re-uploaded whenever
any sheet moved). With one 11-file volume turning: 1,248 B of group rows,
264 B of item boxes, and still the full 2,350,160 B of faces.

Cull, the same crates.io load: the CPU segment cull is 0.30 ms per frame at
the fitted (whole-library) camera and 0.06 ms at a near pose
(`--cam-pose 0 -250 300 0 0`), animating or not; in Visible it keeps only the
backdrops and the hidden flags (0.45 ms). Visible's GPU cull
(`GLYPH_VISIBLE_TIMING=1`, near pose, 150 frames): cull 0.036 ms median
static and 0.036 ms during the page turn (max 0.25 ms on the turn's first
frames), layout 0.169 ms either way, 7,100 glyph-tier lines, 254,718 slots.
Moving groups costs the GPU nothing measurable; it is all host work.

Static frames cost nothing: a settled library's `tick` returns before
touching anything (`SceneLike::animate` is a no-op for every other scene).

## Edges, one by one

### E1. Staging assumed every group is at scale 1 — fixed

What: `RepoLoad::into_staged` built each file's world cull box as
`offset + local` (sub-file blocks too, in two places), and `CullState::new`
derived the local box as `world - offset` ("at staging time every group is
at scale 1"). A group that carries the fit scale got cull boxes of the wrong
size, and `sync_segment`'s later `local * scale + offset` compounded it.
All modes (Visible's item boxes are the same segments).
Did: boxes are `offset + local * s`, local `(world - offset) / s`
(commit b9ffdcf). At s = 1 these are the old expressions bit for bit; every
golden is byte-equal plain and under both equivalents.

### E2. The stored modes' LOD ignored the group scale; Visible's did not — fixed

What: `cull_segments` judged an em as one world unit (`px_scale / dist`)
whatever the group's scale, while Visible's GPU cull uses
`line_height * gscale.y`. With fit scales from 0.0086 to 4, the stored modes
backdropped enlarged pages that were readable and drew subpixel glyphs of
shrunk ones, and disagreed with Visible.
Did: `CullState::em_scale` (each segment's group y scale, refreshed by
`sync_segment`) multiplies into the LOD. Test `lod_reads_the_group_scale`,
mutation `lod-ignores-group-scale` (reddens, proven). Any verb that scales a
group (`scale-group`, the grab wheel) now changes the LOD tier too, which it
should have.
Left: the two LODs still differ in shape — the CPU uses an em of 1 world
unit and one threshold (backdrop under `show_glyphs_px`), Visible uses the
line height (1.25) and two (wash between `visible_backdrop_px` and
`show_glyphs_px`). At a near pose over the crates.io library, Instanced and
Visible differ by 98,834 px, all of it pages whose fit is tiny enough to sit
between the tiers (a whole-file backdrop in one, washed lines in the other).
A pre-existing difference the library makes common.

### E3. Visible item boxes are host-set; every moved group needs one — fixed (6ac88f0)

What: `item_visible` tests a host-written world box per item, while the line
cull reads the live group table. A group that moves without its box is
culled where it used to be. Witness (temporary edit, reverted): with the
`set_item_bbox` call in `sync_segment` removed, `form splay` on g-pick-repo
seen from `--cam-pose 85 -30 45 0 0` loses all of long.md's text (117,946 px
differ from the correct frame; the page face, a scene mesh, is still there).
The existing mutation on that line (proved by `a_moved_item_is_drawn_where_it_went`)
covers the class for the grab verb.
Did: the library's animation goes through `sync_segment` like every group
edit, which calls `VisibleField::set_item_bbox` per moved item.
Cost: one 24 B `write_buffer` per item per frame. 29,377 of them are
~23 ms per frame (seg 25.2 ms in Visible vs 1.7 ms in the stored modes).
Left: there is no batched form. `ItemGpu` is a 128 B row with no host
mirror, so the boxes cannot be uploaded as a span without one. The
architectural fix is to stop host-setting them: give the item a LOCAL box
and let `item_visible` apply the group row it already reads for the LOD
(`groups[group_base(it.group) + 3u]`), or keep world boxes in their own SoA
table uploaded as one span. Either is a change to the Visible crate and its
cull shader, out of scope here.
Fixed (6ac88f0, the follow-up pass): `VisibleItem.bbox_min/max` is the
item's LOCAL box (`repo::segment_local_box`, the SegCull box before the
group's T·R·S) and `cull_items` carries it through
`to_world(group_base(...))` as the line cull does; `sync_segment` uploads
nothing to the field. Goldens byte-equal plain and under both equivalents.
Measured, `form toggle`, Visible: the kernel tree's include/ (6,714
files) seg 4.92 -> 0.12 ms per animated frame; the whole kernel tree
(74,313 files) seg 1.36 ms after (about 54 ms before, at include/'s per-write
cost). Mutation `visible-item-cull-ignores-group` replaces
`visible-moved-item-box-stale`, whose line is gone.

### E3b. `visible_item_of` was a linear scan per call — fixed

What: `sync_segment` asks `visible_item_of(gid)` for every group, and it
scanned the pick files: O(N) per moved group, O(N²) per frame. 217 ms of a
224 ms frame at 29,377 files.
Did: probe `files[gid]` first (a repo load files item i under group i);
commit e63ac80. 217 -> 25 ms.

### E4. The group sync owns colour and alpha — fixed on main by step 1 (37bc076)

What: `sync_to_group_rows` writes `cols[2] = binding.tint` (alpha 1) for
every changed group, so any animation erases a `tint-group`, `tint-cycle` or
`hide-group` edit on a file that moves. Measured: g-pick-repo,
`--pick-file alpha.rs --verb "tint-group ff3030" --verb "form splay"`
renders byte-identical to the same run without the tint. Hide is worse than
lost: the stored modes keep their CPU hidden flag and the Visible field its
hidden table, so the GPU row says visible while the cull says hidden, and
the next `toggle-hidden` reads alpha 1 and inverts the user's intent.
Left: placement and appearance share one row and one writer. In the
redesign they want separate owners (transform from the layout, colour and
visibility from verbs), or a sync that writes only the TRS columns.
Since step 1 (node tables, on main at 37bc076) `write_group_rows` ADOPTS an
external writer's rows: transform always, appearance only when the writer
itself changed it, so an animation no longer erases a verb's tint or hide.
Not re-measured in this pass; it is not the blank-page bug (E14, E15).

### E5. Uploads are whole-table or per-row — measured

`write_group_rows` sends the min..max span when more than 8 rows moved, so
one animated frame of the crates.io library uploads the full 2.8 MB table;
scattered small edits fall back to one write per row. Cheap at 30k groups
(0.12-0.15 ms CPU), but it is one heuristic for every pattern. Mesh
instances (the page faces) are worse: `extract_mesh_instances` rebuilds and
re-uploads every quad (2.35 MB) when ANY transform under a mesh changed,
which is why turning one 11-file volume still costs 2.15 ms of `propagate`.
Without page faces the everything-moves frame drops from 4.82 to 3.21 ms of
`propagate`: about 1.6 ms is mesh re-extraction, about 3.2 ms bevy
propagation over ~35,000 moved nodes. Left.

### E6. One group per file: a file's layout pages cannot be sheets — left (the key finding)

What: HyperLayout fans a long file into side-by-side pages (`page_rows` 128,
`pages_wide` up to 32, bands stacked) INSIDE one item with one group. The
book contain-fits that whole fan onto one page: the crates.io library's
smallest fit is 0.0086 (rows about 0.01 world units tall); in g-pick-repo
long.md, the smallest fit there (0.50), is three columns of tiny text. A book wants each layout page
on its own sheet (the JS Book's verso/recto), which needs one transform per
PAGE, not per file. Today that means one item per page — a byte-range view
of the file's bytes with its own group — which is exactly the item/view
split the redesign is about. Measured consequence beyond legibility: the
tiny fits are what land files between the LOD tiers in E2.

### E7. Depth content versus pages — worked around, dial added

What: HyperLayout's wrap staircase (`--wrap-mode back`, the default) spends a
long line's wraps in depth; g-pick-repo's wide.txt alone makes that library
434 world units deep (`layout [library]: field 60x166x434`).
The JS centred content depth on the page plane (its content was flat), which
stood half of wide.txt in front of every page of its volume (seen: a fan of
text over the front page).
Did: `[library] depth_align = "front"` (default) puts the content's reading
surface on the page plane so depth recedes into the book; `"center"` is the
JS. The page face sits on the page plane and writes depth, so the receding
staircase is hidden from the front and visible past the page edges at an
angle. A deep file still passes through the pages behind it in a deck.
Left: a page abstraction for 3D content (clip to the page? fit depth to the
deck pitch?) is a design question.

### E8. The camera frames the load, not the layout — left

Bounds come from the load-time plan (deck). A form or stack change that
widens the footprint (splay, shelf) leaves the Front camera where it was;
the candidate frames below pass `--cam-pose` for that reason. The
`field bounds: x [0, …]` log line also assumes a field starting at x = 0
(the library is centred on x = 0); cosmetic.

### E9. Offscreen had no animation clock — added

`SceneLike::tick` was windowed-only and carried no `GpuContext`. Added
`SceneLike::animate(ctx, dt)`, called before every frame by both drivers
(offscreen with the fixed 1/60 s clock), a no-op for every scene without an
animation. So `--verb page-next --frames N` renders frame N of the ease
deterministically, and the goldens are untouched (byte-equal, 15/15).

### E10. Grabbing a file inside a hierarchy moves it in the wrong space — fixed (56fdc5d)

`g` drag adds the world-space delta to the file card's LOCAL translation;
under a mount scaled by s the file moves s times the cursor. The carrel and
shelf never nest a card under a scaled parent, so it never showed. `c`
(grab a zone) works and moves a whole directory subtree with its nested
children — the relative layout doing its job; the next relayout glides it
home from where it was left (targets ease from live transforms).
Fixed: `interaction::card_local_delta` converts the world delta into the
card's parent frame (inverse of the parent's world affine on the vector).
Test `a_drag_under_a_scaled_parent_moves_the_card_by_the_world_delta`,
mutation `drag-delta-in-world-space`.

### E11. Picking survives the scale — checked

`--pick-px 760 300` on the front page resolves alpha.rs row 4 col 8 ('t' of
`println`), and `recolor-line` lands on that row: the pick path already
undoes the full group TRS.

### E12. Where the modes disagree visually

- Flat colour (the default for repo loads): Instanced, Derived and Visible
  are byte-identical on g-pick-repo, settled (splay, 120 frames) and
  mid-animation (12 frames into `form splay`), and Instanced = Derived on
  the crates.io library at both poses measured.
- `--color-mode syntax`: Visible renders flat by decision (the known
  exemption); 3,287 px on the g-pick-repo front view, the same pixels the
  flat Instanced frame differs by.
- Near the LOD tiers: E2's remainder, 98,834 px at a near pose over the
  crates.io library, 929 px at the fitted camera.

### E13. Smaller notes

- The page turn law is the JS's: `pageTo` clamps at both ends (no wrap);
  a library volume reads in page order (`order = +1`); a turned page goes to
  the back.
- The JS rebuilt each volume per relayout and seeded only sheet poses, so
  its directories jumped on a form flip; here the structure never changes
  (every directory has a volume node, shelf and pile are slot laws beside
  deck and splay) and directories glide too.
- Loading the library costs 57 ms of `layout` at 29,377 files (spawning
  ~129,000 entities and one propagation) against the shelf's 7 ms.

### E14. Far-LOD backdrops sat behind their own page face — fixed (e736226)

What (Ivan, the kernel tree: pages BLANK until clicked, then shown in the
highlight colour): the CPU cull anchors a far-LOD backdrop at the
segment's far z (`seg.min[2]`), and every page face sits 0.05 behind the
content's FRONT and writes depth. A file with a wrap staircase put its
backdrop behind its own face, so at LOD distance the page drew only the
face; a pick draws the glyphs into the selection mask, which has no depth.
Witness (g-pick-repo): `--verb "page-to 4" --frames 150 --cam-pose 0 -60
6000 0 0` puts wide.txt (fit 4, 424 units deep, 0.26 px/em) in front: its
page is the face alone (486 px differ from the same frame with
`page_faces = false`); after the fix its backdrop band is on the page.
How common on the kernel: rare at Ivan's `z_wrap_spacing = 0.15` (one deep
backdropped segment at a near pose), so it may not be all of what he saw.
Did: the library sets `CullState::backdrop_at_front` (backdrop at
`seg.max[2]`); every other layout keeps its far-z backdrops. Test
`library_backdrop_anchors_to_the_front`, mutation `library-backdrop-at-far-z`.

### E15. Visible's wash boxes past the cap were dropped silently — counted (20258d2)

What: the Visible cull reserves one wash box per wash-tier line and writes
none past `VisibleLimits::max_wash` (1 Mi): those lines draw nothing, the
last in (item, line) order, every frame — while the HUD said "0 dropped".
A near view of the kernel tree's biggest volume
(drivers/gpu/drm/amd/include/asic_reg/dcn) reserved 1,264,446 boxes. A
deck lays out every page behind its head (they are in the frustum, behind
the head's face), which is what fills the budget: a second way a page can
read blank until a pick lays it out into the mask. At the one pose
checked, raising the cap moved only 355 px (the dropped lines were mostly
occluded), so this is a candidate, not a witnessed cause.
Did: `VisibleStats::wash_dropped` on the F8 HUD, the windowed HUD line and
CULLDBG; `max_wash` 1 Mi -> 4 Mi (144 MiB). Mutation
`visible-wash-drops-uncounted`.
Left: a deck's hidden pages are laid out at all. Not laying out sheets
behind a deck's head (or culling by the head's face) would cut the budget
and the cost; it would also drop the sliver of text visible past the page
edges at an angle, which is a design call.

### E16. A grab drag ran once per cursor event — fixed (7a76d4a)

What (Ivan: dragging a file on the kernel library drops frames): every
cursor event applied the drag at once — transform write,
`update_transforms` (bevy propagation over every entity, then mesh
re-extraction), row sync, a segment per moved group — and a mouse reports
several moves per frame. On a 9,241-file library (the kernel tree's arch/,
instanced) one `g` step is 0.41 ms, 0.39 of it bevy propagation for ONE
moved card; at 8 moves per frame 3.3 ms, now 0.42 (the drag is applied
once per frame, from `animate`, as one delta; the final frame is
byte-identical). Instruments: `GLYPH_DRAG_TIMING`, `GLYPH_DRAG_SCRIPT`,
`GLYPH_DRAG_PER_EVENT` (native/AGENTS.md).
Left: propagation is O(every entity) per frame of any motion. On the
74,313-file kernel tree, Visible, everything moving: `propagate_ms`
11.8 ms median per animated frame (seg 1.36, upload 1.77, sync 0.56), and
a 41-file volume turning on arch/ still pays 0.38 ms bevy + 0.13-0.16 ms
re-extracting every face. The fix is the library's hierarchy on
`glyph-scene-graph` nodes (a move writes only the moved nodes' local rows;
the GPU resolve does the rest) and faces re-extracted per moved sheet.

### E17. Smaller findings of the follow-up pass

- `--field-mode instanced` (the default) cannot load the whole kernel tree:
  1.29 G glyphs x 32 B is past VRAM, and the load dies (wgpu OutOfMemory,
  then a panic on the staging poll, `device_alloc.rs`) instead of falling
  back the way the Derived lane limits do. Derived (20 B) loads on a 32 GB
  card; Visible is the mode for it.
- `VisibleStaging::item` sums every earlier item's bytes for `byte_base`:
  O(N²) at load, about half a second of `staged` at 74,313 items.
- The CPU LOD (stored modes) and the GPU LOD (Visible) still disagree near
  the tiers (E2's remainder): an em of 1 world unit against a line of 1.25.
  At a near pose over the kernel tree a page Visible washes is a backdrop in
  Derived.

## Seeing it

From `native/` (the fixture paths are native-relative), with
`target/release/glyph3d-native` from `cargo glyph build`. Add
`--field-mode instanced|derived|visible` to any of them.

```sh
# static: the deck form (each directory's first page fronts its volume)
../target/release/glyph3d-native --load-repo fixtures/g-pick-repo --layout-mode library
# the same at an angle, to see the decks and the nesting depth
../target/release/glyph3d-native --load-repo fixtures/g-pick-repo --layout-mode library --cam-pose 170 10 150 -48 -16
# page turning / deck <-> splay, windowed: ] [ (n p . , arrows), Home/End, v;
# click a file first to address only its volume
# scripted (offscreen): frame 120 is settled, frame ~12 is mid-flight
../target/release/glyph3d-native --load-repo fixtures/g-pick-repo --layout-mode library --verb page-next --frames 120 --screenshot /tmp/page.png
../target/release/glyph3d-native --load-repo fixtures/g-pick-repo --layout-mode library --verb "form splay" --frames 120 --cam-pose 0 -110 260 0 0 --screenshot /tmp/splay.png
../target/release/glyph3d-native --load-repo fixtures/g-pick-repo --layout-mode library --verb "library-stack x" --frames 120 --cam-pose 0 -110 330 0 0 --screenshot /tmp/shelf.png
# a big tree, near pose, every volume turning, with the per-frame instrument
GLYPH_LIBRARY_TIMING=1 ../target/release/glyph3d-native --load-repo <big tree> --layout-mode library --verb page-next --frames 120 --cam-pose 0 -250 300 0 0 --screenshot /tmp/big.png
```

The `[library]` dials override from a launch config, e.g. a file holding
`[library]` / `stack = "y"` / `sort = "size"` passed with `--launch-config`.

Candidate frames for a future library golden (untracked,
`out/tooling-ab/sweep/candidates/library/`, vulkan-nvidia), each made from
`native/` with the command above plus `--screenshot <name>.png`:

| frame | arguments after `--load-repo fixtures/g-pick-repo --layout-mode library` |
|---|---|
| `gpick-instanced.png` | `--frames 2` |
| `gpick-oblique.png` | `--frames 2 --cam-pose 170 10 150 -48 -16` |
| `gpick-page3.png` | `--verb page-next --verb page-next --frames 120` |
| `gpick-splay-{instanced,derived,visible}.png` (byte-identical) | `--field-mode <m> --verb "form splay" --frames 120 --cam-pose 0 -110 260 0 0` |
| `mid-{instanced,derived,visible}.png` (byte-identical) | same with `--frames 12` |
| `gpick-shelf-x.png` | `--verb "library-stack x" --frames 120 --cam-pose 0 -110 330 0 0` |
| `gpick-pile-y-size.png` | `--verb "library-stack y" --verb "library-sort size" --frames 120 --cam-pose 0 -200 420 0 0` |
| `exp-box-visible.png` / `exp-nobox-visible.png` | E3's witness: `--field-mode visible --verb "form splay" --frames 120 --cam-pose 85 -30 45 0 0`, with and without the item-box write |

A golden of the library would be the only frame that sees page faces,
nested group transforms and a scaled LOD; `gpick-splay-*` (three modes, one
picture) and `mid-*` (the ease at a fixed frame) are the obvious pair. Not
adopted: that is Ivan's call.

## What this says for the redesign

1. Placement wants to be a transform per VIEW, and a view is smaller than a
   file (E6). Pages, not files, are the unit a book turns.
2. Transform and appearance want separate owners (E4); a layout that
   animates must not be able to erase a verb's edit.
3. Anything culled by a host-written world box must be re-written on every
   move (E3): 23 ms/frame at 30k items through today's per-item API. Cull
   from local boxes and the live group table instead, or upload boxes as a
   span.
4. The per-frame host cost of 30k moving groups, outside Visible, is ~7 ms,
   two thirds of it bevy propagation and mesh re-extraction (E5); the GPU
   side does not notice. Dirty-subtree tracking already works (11 moving
   groups: 0.01 ms of sync), but every whole-list rebuild downstream of it
   (mesh instances, the span upload) undoes it.
5. LOD must be computed in the space the glyphs end up in (E2): any per-group
   or per-glyph scale has to reach every tier test, and the two LOD
   implementations should become one.
