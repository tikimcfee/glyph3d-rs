# GPU transform hierarchies, pooled group rows, hierarchical culling — prior art (2026-10-10)

Background survey for the group-table redesign: a transform TREE (directory
groups parent file groups), pooled/freeable rows with generation handles,
many "views" (byte-range emitters) each with a transform, optional per-glyph
pose. Constraint: cost follows what moved, not what is loaded. Targets: wgpu
30 / WGSL on Metal (unified memory) and Vulkan (NVIDIA); no bindless-only,
vendor-only or 64-bit-atomic assumptions. Research note, not a plan; nothing
here was measured in this repo unless it says so.

Scale of the problem, counted on a recent mainline Linux checkout (2026-10-10):
95,548 files, 6,282 directories, deepest directory at depth 10 (so file
groups sit at depth ~11, plus whatever root/view levels we add). Most
directories are at depth 2-5. `drivers/` alone holds 37,988 files: dragging it
is the worst realistic "move a subtree" case.

## Summary and recommendation

1. Nobody we found resolves a deep, general transform tree on the GPU in a shipping engine; Unreal, Unity Entities, bevy and Wicked all compose world transforms on the CPU and upload results. The GPU-side pieces they ship are sparse UPLOAD (compute scatter) and culling.
2. Their CPU cost is O(changed subtree) at best (bevy 0.16's dirty bits), but their UPLOAD is O(descendants) — every descendant's world matrix changes when an ancestor moves. That is exactly the cost we must avoid for a 38k-file drag.
3. So for us: CPU stays authoritative for the tree and the allocator; it uploads only LOCAL rows that changed (one 96 B row when a directory is dragged), and the GPU derives WORLD rows.
4. Resolve world rows in one compute dispatch, one thread per group, each walking its parent chain (Wicked Engine's CPU algorithm, moved to the GPU). At depth <= ~12 and ~100k groups it is ~0.5-1.2 M quaternion compositions with the hot ancestors in cache — estimated well under 1 ms, unmeasured. No ordering constraint, so pooled/reused rows need nothing special.
5. Restrict the INHERITED transform to a similarity (translation + quaternion + uniform scale: closed under composition, 8 floats). Keep today's per-axis scale as a non-inherited leaf factor, as Unity Entities does (`LocalTransform` uniform scale + `PostTransformMatrix`).
6. Limit resolve work to what moved: the CPU keeps a pre-order (DFS) index list; a dirty subtree is a contiguous range of it, so the dispatch covers only dirty ranges. Skip the pass when nothing is dirty. Level-ordered dispatches and pointer jumping are fallbacks only if measurement says so.
7. Allocation: CPU free list + generation per slot (slotmap / "handles are the better pointers"). Freed rows sit in a quarantine for the frames in flight before reuse; the GPU never allocates group rows. GPU "dead lists" (particle systems) solve a problem we do not have.
8. Upload: coalesce dirty rows into contiguous `write_buffer` runs; past a few dozen scattered rows, use bevy's pattern (one staging upload of rows + indices, and a WGSL scatter shader, with a full re-upload above ~15 % changed).
9. Culling: store each directory's SUBTREE bounds in its own local frame. A move then changes only the bounds of the moved node's ancestors (O(depth), CPU), and any node's world box is Arvo's transform of its local box by its world transform, computed on demand. Cull top-down: directories (coarse), then files/lines (fine).
10. Per-glyph pose: an optional sparse override (local offset/quaternion keyed by (item, byte), like the M3 re-keying), applied before the group's world transform in the same vertex path. Slug-style dilation needs only the final MVP, so it is unaffected.

## 1. Composing parent∘child world transforms on the GPU

### What engines actually do

- **Unreal (GPU Scene, Nanite).** Component hierarchies (`USceneComponent`) are
  resolved on the game thread. GPU Scene holds per-primitive and per-instance
  data at offsets recorded on `FPrimitiveSceneInfo` (`InstanceSceneDataOffset`,
  `NumInstanceSceneDataEntries`), uploaded through scatter-upload helpers
  (`FScatterUploadBuffer`, `FRDGScatterUploadBuffer`, `ScatterCopyResource`
  taking a scatter-index SRV, an upload-data SRV, and bytes/elements per
  scatter). `FMeshBatchDynamicPrimitiveData` can bind a GPU "writer delegate"
  that fills instance data in a compute shader, so some instance data is
  GPU-only. The one hierarchical GPU case found: **Nanite Assemblies**
  (UE 5.8 docs, experimental, Nanite Foliage). An assembly holds up to 65k part
  instances, and Nanite computes "their final transform on-demand during
  cluster culling as it descends the hierarchy", at "a small performance
  trade-off during cluster culling". That is chain composition during a
  top-down cull, not a separate resolve pass. Storage format and depth limit
  are not documented.
  Sources: [FPrimitiveSceneInfo](https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Renderer/FPrimitiveSceneInfo),
  [FScatterUploadBuffer](https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/RenderCore/FScatterUploadBuffer),
  [ScatterCopyResource](https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/RenderCore/ScatterCopyResource),
  [FMeshBatchDynamicPrimitiveData](https://dev.epicgames.com/documentation/unreal-engine/API/Runtime/Engine/FMeshBatchDynamicPrimitiveData),
  [Nanite Foliage / Assemblies](https://dev.epicgames.com/documentation/en-us/unreal-engine/nanite-foliage).
  Unverified: how GPU Scene chooses which primitives to re-upload (the
  `GPUScene.cpp` internals were not read).
- **Unity Entities / Entities Graphics.** `LocalToWorldSystem` computes
  `LocalToWorld` on the CPU, "recursively descending each hierarchy and
  composing the entity's LocalTransform and PostTransformMatrix with its
  parent's LocalToWorld". `LocalTransform` supports uniform scale only;
  non-uniform scale and shear go in `PostTransformMatrix`. The results reach the
  GPU through `SparseUploader` (source read from the package mirror):
  Burst threads write (operation, data) records into a mapped upload buffer
  with a lock-free bump marker (operations from the front, data from the back).
  One compute dispatch (`CopyKernel`, at most 65,535 groups per dispatch) then
  applies them. Operation types include `Matrix_4x4`, `Matrix_3x4` and their
  `_Inverse` forms, so the GPU expands or inverts matrices during the scatter
  as well as copying them.
  Sources: [Entities transform concepts](https://docs.unity3d.com/Packages/com.unity.entities@1.4/manual/transforms-concepts.html),
  [Entities Graphics API (SparseUploader)](https://docs.unity3d.com/Packages/com.unity.entities.graphics@1.4/api/Unity.Rendering.html),
  [SparseUploader.cs (needle-mirror)](https://github.com/needle-mirror/com.unity.entities.graphics/blob/master/Unity.Entities.Graphics/SparseUploader.cs).
- **bevy.** Transform propagation is CPU-only (`bevy_transform`). 0.16 added a
  dirty bit propagated UP the hierarchy (`mark_dirty_trees`,
  `TransformTreeChanged`) so `propagate_parent_transforms` skips whole static
  subtrees, plus better parallel work sharing (upstreamed from `big_space`).
  Measured by bevy: 127,515 objects (Caldera Hotel), propagation 1.1 ms → 0.1 ms
  on an M4 Max. `StaticTransformOptimizations` can turn it off, because "in very
  dynamic scenes" the tracking can cost more than it saves. The GPU side
  (`gpu_preprocess.rs`) expands a small CPU-written `MeshInputUniform` into the
  full `MeshUniform` in a compute pass; `previous_input_index` links each entry to
  last frame's slot, because slots move as entities spawn and despawn. No GPU
  propagation found.
  Sources: [Bevy 0.16 release notes](https://bevy.org/news/bevy-0-16/),
  [mark_dirty_trees](https://doc.qu1x.dev/bevy_trackball/bevy_transform/systems/fn.mark_dirty_trees.html),
  [MeshInputUniform](https://dev-docs.bevyengine.org/bevy/pbr/struct.MeshInputUniform.html).
- **Wicked Engine.** `Scene::RunHierarchyUpdateSystem` (`wiScene.cpp`, read
  2026-10-10) dispatches one job per hierarchy component. Each job walks its own
  parent chain (`while (parentID != INVALID_ENTITY) worldmatrix *= parent local`)
  and also ANDs layer masks down the chain. Every node is resolved
  independently, and nothing depends on the order of the component arrays. This
  is chain walking, on CPU worker threads.
  Source: [wiScene.cpp](https://github.com/turanszkij/WickedEngine/blob/master/WickedEngine/wiScene.cpp).
- **AC Unity / Ubisoft (Haar & Aaltonen, SIGGRAPH 2015).** Per-instance data
  ("transform, LOD factor...") is "updated in GPU ring buffer, persistent for
  static instances". Hierarchy, if any, is resolved before upload. Benchmark:
  250,000 separate moving objects; on Xbox One, object culling + LOD took 0.28 ms
  (first phase) + 0.26 ms (second phase), the whole GPU pipeline 2.3 ms, and the
  CPU 0.2 ms on one Jaguar core.
  Source: [GPU-Driven Rendering Pipelines (slides)](https://advances.realtimerendering.com/s2015/aaltonenhaar_siggraph2015_combined_final_footer_220dpi.pdf).
- **The Forge, niagara (zeux) and similar GPU-driven samples.** These do GPU
  culling and submission over flat instance lists. No hierarchy resolution was
  found in either. Unverified: only the READMEs and summaries were checked.
- **Academic.** Wörister et al., "Lazy Incremental Computation for Efficient
  Scene Graph Rendering" (HPG 2013, Aardvark): a dependency graph so a change
  re-evaluates only what depends on it. CPU-side, but it states our "cost
  follows what moved" goal precisely.
  [Draft PDF](https://www.cg.tuwien.ac.at/courses/RendEng/2015/RendEng-2015-11-16-paper2.pdf).
  Levien, "Fast GPU bounding boxes on tree-structured scenes" (2022) resolves a
  clip/blend TREE on the GPU in portable compute shaders, using a new parallel
  parentheses-matching algorithm (the "stack monoid"). It is the most
  directly relevant published GPU tree algorithm; the abstract gives no figures.
  [arXiv:2205.11659](https://arxiv.org/abs/2205.11659).

### The candidate techniques, with costs

| Technique | Work | Dispatches | Needs | Notes |
|---|---|---|---|---|
| CPU flatten + upload world rows | O(dirty subtree) CPU | 0 | — | what every engine ships; upload is O(descendants): a `drivers/` drag would re-upload ~38k rows (~3.6 MB at 96 B) per frame |
| Chain walk per node (one thread per group) | O(N·d) | 1 | parent index per row; depth cap | Wicked's algorithm on the GPU; no ordering; ancestors are few and stay cached |
| Level-ordered (one dispatch per depth) | O(N) | d | nodes grouped by depth (contiguous ranges or per-level index lists) | wgpu already inserts a barrier between dispatches that write the same buffer ([wgpu#2659](https://github.com/gfx-rs/wgpu/issues/2659)), which is exactly the sync this needs; pooled rows break contiguity, so a per-level index list is needed |
| Pointer jumping (path doubling) | O(N log d) | ⌈log2 d⌉ (4 for d=12) | ping-pong buffers; associative compose | wins only for very deep chains; for d ≤ 12 it is not cheaper than chain walking ([Wikipedia](https://en.wikipedia.org/wiki/Pointer_jumping)) |
| Euler-tour + prefix scan | O(N) | a few (scan passes) | inverse transforms on subtree exit, or Levien's stack monoid | the inverse form accumulates float error; the stack monoid avoids inverses but is far more machinery than d ≤ 12 warrants |
| Per-glyph chain walk in the vertex shader | O(glyphs·d) per frame | 0 | — | rejected: millions of glyphs × ~11 levels, every frame |
| On-demand during top-down cull (Nanite Assemblies) | O(visited nodes) | d (one per level, indirect) | child lists (CSR), per-level work queues via atomics | the most "cost follows visibility" form; needs indirect dispatch per level |

Depth limits: no GPU form has a hard limit except the loop cap you choose.
Chain walking should cap iterations (e.g. 32) so a corrupted parent cycle
cannot hang the GPU. Precision: composing in f32 is fine near the origin. A
kernel-sized layout spread over a large world extent may need camera-relative
or origin-rebased transforms; bevy's `big_space` exists for this reason.

**For us:** keep the CPU-authoritative tree, upload LOCAL rows, and resolve
WORLD rows on the GPU with a single chain-walk dispatch over the dirty
pre-order ranges. Measure before considering level-ordered dispatches. Reserve
the on-demand form (Nanite-style) for when even O(moved descendants) is too
much, e.g. dragging the root of a loaded kernel every frame.

## 2. Dirty updates to large persistent GPU tables

- **wgpu `Queue::write_buffer`.** Data is copied into staging memory during
  the call and transferred at the next `submit`. "Currently on native
  platforms, the staging memory will be a new allocation" per call, freed after
  that submission completes. The docs point to `write_buffer_with` (build the
  data in staging memory), `StagingBelt`, or self-managed mapped buffers to
  avoid the short-lived allocations.
  [Queue docs](https://docs.rs/wgpu/latest/wgpu/struct.Queue.html),
  [StagingBelt](https://wgpu.rs/doc/wgpu/util/struct.StagingBelt.html).
  So N scattered rows written as N calls cost N allocations and N copy
  commands. Coalesce adjacent dirty rows into runs. No published per-call cost
  figure was found (unverified; measure).
- **Mapped staging on unified memory.** `MAPPABLE_PRIMARY_BUFFERS` (native:
  Vulkan, DX12, Metal) allows MAP_WRITE on STORAGE buffers; it "is only
  beneficial on systems that share memory between CPU and GPU" and "can severely
  hinder performance" elsewhere. wgpu has no persistent mapping: submitting work
  that uses a mapped buffer panics, so a CPU-written table on unified memory
  means a ring of buffers, each mapped with `map_async` after the GPU has
  finished with it.
  [Features](https://docs.rs/wgpu/latest/wgpu/struct.Features.html),
  [Buffer mapping](https://docs.rs/wgpu/latest/wgpu/struct.Buffer.html).
  (This repo already uses this for Pass 2's bulk output.)
- **Compute scatter ("patch") pass, the common engine pattern.**
  - bevy (main, `sparse_buffer_vec.rs` + `sparse_buffer_update.wesl`, read
    2026-10-10): `AtomicSparseBufferVec` tracks dirty elements with per-element
    bits that CPU threads set atomically. If at most 15 % of elements changed
    ("obtained experimentally by testing very large scenes and roughly matches
    the values used by other engines"), it uploads only the changed elements
    plus a destination-index array, and a WGSL shader (one thread per 32-bit
    word, workgroup 256) scatters them; otherwise one bulk `write_buffer`. It
    also keeps a CPU free list (`free_uniform_indices`) for slot reuse. This is
    WGSL/wgpu code we can read directly.
    [sparse_buffer_vec.rs](https://github.com/bevyengine/bevy/blob/main/crates/bevy_render/src/render_resource/sparse_buffer_vec.rs),
    [sparse_buffer_update.wesl](https://github.com/bevyengine/bevy/blob/main/crates/bevy_render/src/render_resource/sparse_buffer_update.wesl).
  - Unity `SparseUploader`: operation records plus payload in one mapped
    buffer, applied by a compute copy kernel that can also derive data (3x4
    packing, inverses). See §1.
  - Unreal `ScatterCopyResource`: a scatter-index buffer plus an upload-data
    buffer, applied by a compute copy. See §1.
  - AC Unity: a per-instance ring buffer, persistent for static instances. See §1.

**For us:** a directory drag is one LOCAL row per frame (96 B), so plain
`write_buffer` is right for the common case. Batch structural edits (open
a repo, collapse a tree) through a bevy-style scatter pass, falling back to a
full upload above roughly 15 %. Keep LOCAL rows (CPU-written) and WORLD rows
(GPU-written) in separate buffers so the two writers never share a buffer.

## 3. Free lists, pooling, and generation handles

- **GPU dead lists (particles).** AMD's DirectCompute particle talk (2014): a
  persistent dead list of free indices, plus an alive list rebuilt by the
  simulation each frame. Emission consumes dead indices; the alive count lives
  only on the GPU, so drawing uses indirect arguments. Wicked Engine's write-up
  uses the same structure, with counter buffers feeding the indirect arguments.
  [AMD slides (SlideShare)](https://www.slideshare.net/slideshow/holy-smoke-faster-particle-rendering-using-direct-compute-by-gareth-thomas/35656271),
  [gamedev.net: append/consume buffers](https://gamedev.net/blogs/entry/2250245-direct3d-11-programming-tip-9-append-and-consume-buffers).
  (The Wicked particle article URL now 404s; its content is from search excerpts only, so unverified.)
  WGSL has no append/consume buffers; the equivalent is an index array plus an
  `atomic<u32>` counter. Popping from an empty list underflows, so the shader
  must `atomicSub`, check, and then restore or clamp. 32-bit atomics are enough.
- **CPU-authoritative allocation with a GPU mirror.** bevy's
  `free_uniform_indices` and `previous_input_index` (§1, §2). Rows are allocated
  and freed by the CPU; the GPU sees only indices. Cross-frame references (TAA,
  for bevy) carry an explicit link to the previous slot, because slots get
  reused.
- **Generation handles.** Weissflog, "Handles are the better pointers" (2018):
  a handle is an index plus a tag. A per-slot generation counter is bumped on
  free, and a lookup compares the handle against the slot's current value, so
  a freed or reused slot is detected. A slot whose counter would overflow is
  retired. LIFO vs FIFO reuse changes how often handles collide.
  [floooh.github.io](https://floooh.github.io/2018/06/17/handles-vs-pointers.html).
  The `slotmap` crate is the Rust form of the same idea.
- **Stale references on the GPU.** Handles protect the CPU. A GPU frame still
  in flight can read a row the CPU has just freed and reused. The standard fix
  is deferred free: a freed row waits in a quarantine for the number of frames
  in flight (or until `Queue::on_submitted_work_done`) before reuse. Optionally,
  store the generation in the row and in every record that references it
  (view/emitter records), and have the shader compare them, drawing nothing (or
  a debug colour) on mismatch. That is a cheap debug assertion, not a
  correctness mechanism. (Synthesised from the sources above, not one source.)

**For us:** groups are created and destroyed by the CPU (load, close,
regroup), so allocation stays CPU-authoritative: a free list with a
generation per slot and a frame-in-flight quarantine. Transient per-frame
emission (visible lines → slots) is already bump-allocated per frame and needs
no free list. A GPU dead list would only earn its place if the GPU itself
spawned persistent groups, which nothing here plans.

## 4. Culling with hierarchies

- **Two-level culling is standard.** AC Unity: instance culling (frustum and
  occlusion) → cluster-chunk expansion → cluster culling (64-triangle
  clusters), with two-phase occlusion (cull against last frame's depth pyramid,
  then retest the culled set against a refreshed pyramid). Nanite: instance
  culling, then a cluster hierarchy (BVH), with assembly part transforms
  composed during that descent (§1).
  [AC Unity slides](https://advances.realtimerendering.com/s2015/aaltonenhaar_siggraph2015_combined_final_footer_220dpi.pdf),
  [Nanite Foliage](https://dev.epicgames.com/documentation/en-us/unreal-engine/nanite-foliage).
- **Keeping bounds current.** Engines store a LOCAL bound per instance and
  transform it by the current world matrix during culling, so animation never
  re-uploads bounds. Arvo's method (Graphics Gems, 1990) transforms an AABB by
  an affine matrix; zeux's centre/extent form (new extent = |M₃ₓ₃|·extent) uses
  about a quarter of the flops of transforming eight corners.
  [zeux: AABB from OBB with component-wise abs](https://zeux.io/2010/10/17/aabb-from-obb-with-component-wise-abs/),
  [Arvo citation](https://gameenginegems.com/gemsdb/article.php?id=768).
- **GPU BVH refit.** Karras (HPG 2012): bottom-up, one thread per leaf
  climbing toward the root. An atomic flag per node stops the first thread to
  arrive, so the second processes the node with both children ready. NVIDIA
  measured 0.06 ms for bounding boxes over 12K objects. WGSL's 32-bit
  `atomicAdd` is enough for this.
  [Thinking Parallel III](https://developer.nvidia.com/blog/thinking-parallel-part-iii-tree-construction-gpu/),
  [Karras 2012](https://research.nvidia.com/publication/2012-06_maximizing-parallelism-construction-bvhs-octrees-and-k-d-trees).
  Ray-tracing practice: NVIDIA recommends rebuilding the TLAS (the structure
  over instances) every frame, and refitting the BLAS only after limited
  deformation. A top-level structure over ~10^5 instances is cheap enough to
  rebuild every frame.
  [NVIDIA RTX best practices](https://developer.nvidia.com/blog/best-practices-for-using-nvidia-rtx-ray-tracing-updated/).
- **Subtree bounds in the local frame.** If a directory's bound encloses its
  whole subtree expressed in the DIRECTORY'S OWN frame, moving that directory
  (or anything inside it rigidly) leaves every bound inside it unchanged. Only
  the moved node's ancestors need a refit, which is O(depth) and can run on the
  CPU, since the CPU holds the local transforms and the child bounds. Culling
  descends from the root, composing transforms and testing Arvo(world, local
  bound) at each node. This is the Nanite-assembly shape applied to directories.
  (Synthesis; no single source states it for scene-graph culling.)

**For us:** keep each directory's subtree bound in its own frame, refit only
along the path to the root on the CPU, and cull top-down: directories
(~6k, cheap enough on the CPU or in a small GPU pass), then the files and lines
inside visible directories on the GPU, as the visible layout does now. A
glyph's world position never has to exist anywhere except in the vertex stage.

## 5. Massive text / glyph instancing

- **Zed / GPUI.** One instanced draw per primitive kind per layer. Glyphs are
  quads sampled from an alpha-only atlas packed with etagere, with up to 16
  subpixel variants per glyph; each instance carries origin, atlas origin, size
  and colour. Layout runs on the CPU and there is no transform hierarchy (2D
  layers and stacking contexts only). Shaping and rasterization are cached
  across frames.
  [Zed: render UIs at 120 FPS](https://zed.dev/blog/videogame).
- **Slug.** Glyphs are rendered from outlines with no prebaked textures, as a
  per-glyph bounding polygon (2-6 triangles). Dynamic dilation (2019) pushes
  each vertex out by about half a pixel using the MVP and viewport, per vertex,
  so it works under any 3D transform. HarfBuzz's Slug-derived `hb-gpu` exposes
  an `hb_gpu_dilate` that takes the MVP, a Jacobian and the viewport.
  [JCGT 2017](https://jcgt.org/published/0006/02/02/paper.pdf),
  [Dynamic Glyph Dilation](https://terathon.com/blog/glyph-dilation.html),
  [hb-gpu-vertex.hlsl](https://arai.searchfox.org/firefox-main/source/gfx/harfbuzz/src/hb-gpu-vertex.hlsl).
  For us: dilation needs the FINAL world∘view∘projection per vertex. That is
  available as soon as the group's world row is resolved, so a hierarchy changes
  nothing in the glyph shader beyond which table it reads.
- **Vello / Levien.** The one GPU text/vector renderer with published parallel
  TREE algorithms (clip and blend nodes resolved by parentheses matching in
  compute; see §1). Transforms in Vello's encoding are a flat stream, and
  `Scene::append` with a transform composes on the CPU (from memory of the
  Vello source; unverified this session).
- **Per-glyph pose.** No public source found for 3D glyph instancing with
  hierarchical transforms at our scale. Per-character animation in game UI text
  works by modifying per-glyph vertex data (unverified, general practice). For
  us: an optional sparse override keyed by (item, byte), applied in local space
  before the group's world transform, keeps the common path unchanged.

## Open questions

- **Measure the resolve pass.** Chain-walk resolve over ~102k groups at d ≈ 11
  on M2 (Metal) and NVIDIA (Vulkan): is it really sub-millisecond, and does
  restricting it to dirty pre-order ranges beat running it over everything?
- **Inherited style.** Alpha multiplies and visibility ANDs naturally. Clip
  rectangles do not compose across frames (the intersection of two boxes in
  different frames is not a box). Is clip local-only, or does it need an
  inherited screen-space form?
- **Non-uniform scale.** Does any verb need inherited non-uniform scale?
  Allowing it forces 3x4 world matrices (48 B, shear) in place of 8-float
  similarities.
- **Pre-order list maintenance.** Reparenting a subtree moves a range of the
  pre-order list. Is a CPU rebuild (~100k u32) on structure change acceptable,
  or does it need a gap-buffer or order-maintenance scheme?
- **Views.** Should views be tree nodes (a view is a group whose parent is the
  file's group) or a separate table with a group handle? Tree nodes keep one
  resolve path.
- **Per-dispatch cost.** What does a dispatch with an implied barrier cost on
  Metal and on NVIDIA Vulkan through wgpu 30? This decides whether
  level-ordered or on-demand top-down (d indirect dispatches) is ever
  affordable.
- **Precision.** Does a whole-kernel layout reach world extents where f32 world
  positions visibly jitter? If so, resolve camera-relative.
- **GPU-side generation check.** Worth adding at all, or is the quarantine
  enough, given that the pixel goldens would show a reused-row bug only if a
  golden frame frees and reuses a row?
