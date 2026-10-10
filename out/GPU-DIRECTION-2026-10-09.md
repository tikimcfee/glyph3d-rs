# GPU layout: rewrite, retire, or something else — 2026-10-09

**Question.** Should the GPU layout path (the CubeCL chain, `--repo-engine cubecl`)
be rewritten to be genuinely faster than the CPU path (HyperLayout), or retired?
Ivan's intuition going in: GPU parallelism for runs and glyphs is attractive, but
the current shape (runs, prefix scans, many dispatches) is wrong — "think of it
like a mesh": task/mesh shaders, vertex pulling, placement computed just in time
at draw time rather than materialised.

**Answer in one paragraph.** Retire the CubeCL chain as a production engine; do
not rewrite it in the same whole-corpus shape; and take the just-in-time idea
seriously at the granularity where it actually pays, which is *lines and files*,
not vertices. The measurements below show that on this discrete GPU the chain's
kernels are already fast (about 12 ms of GPU time for 93 MB) and the chain still
loses two hundred milliseconds to host-side overhead; that the CPU path's own
460 ms is 85 % data movement of a 1.8 GB slot stream through a wgpu staging path
that zero-fills and re-reads write-combined memory; that flipping HyperLayout to
its *existing* host-memory staging strategy brings it to 204 ms, a dead tie with
the chain, with no GPU layout at all; and that the design with a different
ceiling is one where the slot stream does not exist as a whole-corpus artefact —
resident source bytes plus a line table, and placement computed for the visible
set only. That is Ivan's intuition, confirmed in substance and corrected in
shape: the vertex stage cannot do it (the fold is serial per line and the vertex
stage has no shared memory and, on Metal, no subgroups), mesh shaders are
experimental in wgpu 30 and buy nothing over compute + indirect here, and a
compute pre-pass over visible lines is the form the idea survives in.

Everything marked **measured** below was run on this box today; everything marked
**quoted** comes from the tree's own prose or from a prior report and was not
re-measured; **inferred** is arithmetic or code reading. The box: RTX 5090 /
Vulkan / driver 615.78.08, 32 cores, 60 GB, resizable BAR on (BAR1 = 32 GiB).
The GPU was shared with another agent's runs throughout: load average ran between
1 and 10 across the session and is printed next to every table that matters;
every A/B is round-robin interleaved with the first round discarded, and I quote
min and median, never a single run.

Corpus: a fresh 93.0 MB tree of crates.io sources built the documented way
(263 crates, 7,141 files, 92,974,417 engine records, 90,234,603 glyph instances
after blank/missing drop, 74 files carrying emoji). Not byte-identical to the
"102 MB tree" of the earlier notes, so compare ratios across days, not
milliseconds.

Branch and files: this report; `experiments/gpu-direction/` (the two bench
scripts and the `jit-layout` prototype); four `tracing` spans and three
env-gated measurement switches in `native/src/layout_hyper/device_alloc.rs`
(default off, bytes unchanged, battery green — see §8).

---

## 1. Where the time goes

### 1a. HyperLayout, the production path (`--repo-engine hyper`)

Shape, from the code (`native/src/layout_hyper.rs`, `layout_hyper/*.rs`):
Pass 1 is a Rayon pass over 64–128 KiB chunks that counts survivors and leaders
and measures the widest row; it reads about 2 B per source byte (memchr plus an
8-byte SWAR printable-ASCII check) and writes about 200 B per chunk — pure-ASCII
lines are counted in closed form. Pass 2 walks the chunks again and writes one
slot per survivor straight into the destination memory: 32 B `RenderSlot`
(Instanced) or 20 B `DerivedSlot` (Derived). On macOS unified memory the
destination is the final Metal buffer, mapped. On a discrete GPU it is a
`mapped_at_creation` `COPY_SRC` staging buffer, and here the path gets
expensive in ways the code's own spans did not show until today.

**Measured** (this box, `--field-mode derived`, syntax colour, 5 kept rounds,
load average 1.5–4.3; `experiments/gpu-direction/bench_staging.py`):

| stage (span) | min / median ms | what it is |
|---|---|---|
| `hyper.pass1` | 4.2 / 5.2 | measured with `--repo-engine batch`, which does not prefetch; on the default path it runs on the prefetch thread and is fully hidden behind GPU init |
| `hyper.staging.create` | 147 / 158 | **wgpu-core allocating a hidden host-visible staging buffer of its own and zero-filling 1.80 GB of it on one thread** (`wgpu-core/src/device/resource.rs` `create_buffer` → `StagingBuffer::write_zeros`); the buffer we asked for is `COPY_SRC` only, so it is not mappable and wgpu maps *its* staging instead |
| `hyper.pass2` | 63.2 / 63.5 | 32 threads writing 1.80 GB of `DerivedSlot` into that staging memory: 28 GB/s |
| `hyper.emoji_tint_pairs` (new span) | 184 / 193 | **re-reading the slots of the 74 emoji-bearing files back out of the staging memory** to fold their LOD backdrop tint. With ReBAR the staging lands in device-local host-visible memory; reads from write-combined memory run at a few hundred MB/s. This was the "unattributed" gap |
| `hyper.staging.unmap` | 0.0 | records wgpu-core's copy (hidden staging → our staging, 1.8 GB) into the queue's pending writes |
| `hyper.staging.copy_submit` | 18.9 / 19.5 | creating the STORAGE chunk buffer and encoding our copy (staging → chunk) |
| `hyper.staging.poll` | 20.4 / 21.3 | waiting for both copies |
| **backend total** | **447 / 461** | `phases:` line; Instanced (32 B slots): 540 / 559 |

So of 461 ms, Pass 1 + Pass 2 is 68 ms. The other 390 ms is moving and
re-reading one 1.8 GB artefact. Flat colour mode is 448 / 465: the syntax
colouriser is not measurable here.

Bytes per source byte (r ≈ 0.97 survivors per byte, so 19.4 B of Derived slots
or 31 B of Instanced per byte; **inferred** from code, placement of the hidden
staging **inferred** from ReBAR being on): host writes 19.4 (zero-fill) +
19.4 (slots) ≈ 39; crosses PCIe 19.4 without ReBAR placement, 38.8 with; written
to VRAM 39–78; and the emoji re-read reads the slots of every emoji-bearing file
once more over the bus. Instanced is 1.6× all of that. Pass 1 and Pass 2 read
about 5 B per byte from cache and about 2 from DRAM. On the M2 (unified,
`create_mapped_slot_buffer`) the host writes 19.4 or 31 once into the final
buffer and nothing is copied; **quoted**: 168 ms flat / 215 ms syntax for the
97 MB flagship there.

**Two existing strategies, measured against each other.** `device_alloc.rs` has a
second staging path, `stage_host_memory`, used today only when the slots exceed
`max_buffer_size`: Pass 2 writes into ordinary host RAM, then a 64 MiB
`MAP_WRITE` staging buffer streams it across. Forced on with the new
`GLYPH_EXPERIMENT_STAGING=host` switch:

| configuration (Derived, syntax) | backend min / median ms | pass2 | emoji re-read | notes |
|---|---|---|---|---|
| baseline (single mapped staging) | 447 / 461 | 63 | 193 | today's default on discrete |
| `GLYPH_EXPERIMENT_SKIP_TINT_REREAD=1` | 264 / 269 | 63 | 0 | not output-neutral (emoji files lose backdrop tint); prices the re-read only |
| `GLYPH_EXPERIMENT_STAGING=mapwrite` | 444 / 457 | 63 | 207 | adding MAP_WRITE removes wgpu's hidden copy but not its zero-fill, and the re-read is still from WC memory: no gain |
| `GLYPH_EXPERIMENT_STAGING=host` | **201 / 204** | 86 | 1.2 | Pass 2 into cacheable RAM is slower (read-for-ownership) but the re-read is free and there is no zero-fill of WC memory; the remaining ~115 ms is the serial 64 MiB memcpy + DMA stream |
| host + skip | 201 / 202 | 86 | 0 | confirms the re-read costs nothing in host memory |

That is the CPU path at **2.26× its current speed on this box with a one-line
policy change**, and its floor is lower still: the streaming loop is serial
(map → memcpy → unmap → submit → wait), so double-buffering two staging buffers
to overlap memcpy with DMA would bring the movement cost toward
max(memcpy, PCIe) ≈ 60–80 ms, and backend to roughly 150 ms. The PCIe floor for
1.8 GB on this bus is about 40–70 ms; nothing that keeps the 1.8 GB artefact can
go below that.

### 1b. The CubeCL chain (`--repo-engine cubecl`)

Shape, from the code (`native/src/cubecl_chain/`, 12,572 lines including
`cubecl_layout.rs`, `cubecl_scan.rs`, `cubecl_smoke.rs`): the CPU still runs the
walk, a serial marshal of every byte into one Vec, the syntax colouriser over
every byte (4 B/byte of colours uploaded in syntax mode), and **HyperLayout's own
Pass 1** (`prep.rs` calls `layout_hyper::pass1_prepass`) to size every buffer —
all on the prefetch thread, hidden behind GPU init exactly as HyperLayout's Pass 1
is. On the GPU, **3 dispatches** without clusters (`tile_scan` → `spine_scan` →
`apply_and_emit[_derived]`) or **23** with them (`decode_probe`, `cand_sort` on one
thread, `jump_build`, 15 × `rank_step`, `item_roots`, `cluster_mark`, then the
three). **Zero GPU syncs** on the Derived or flat path; one blocking tint
readback (8 B per slot across PCIe) in Instanced + syntax. The slots are emitted
directly into the buffer the field draws — no copy. Placements are closed-form on
the host. Device buffers: ≈ 21 B/byte Derived flat, 25 Derived syntax, 33
Instanced flat, 45 Instanced syntax; GPU traffic ≈ 6–9 B/byte read, ≈ 20 (Derived)
or 40 (Instanced + tints) written, all VRAM-internal.

**Measured** (Derived, same corpus, same session, `bench_ab.py`, 4 kept rounds,
load 1–8):

| stage | min / median ms | measured how |
|---|---|---|
| `chain.upload` | 68 / 108 (syntax), 97 / 112 (flat) | span; 93 MB of bytes + tables through cubecl's `create` at ≈ 1 GB/s — slow for what it is |
| prewarm join | 23 / 47 | log line; waiting for the pipeline prewarm thread, i.e. shader compilation |
| `chain.dispatch` | 52.9 / 53.5 | span; host enqueue of 23 launches. `b1` is 47 ms of it, and the three kernels prewarm compiles are not in block 1, so this is **inferred** to be mostly JIT compilation of the un-prewarmed cluster kernels |
| GPU execution, all kernels | ≈ 12.2 | device timestamps (`GLYPH_CHAIN_PROF=stages`): `apply_and_emit` 3.98, `tile_scan` 0.43, `spine_scan` 0.41, `decode_probe` 0.23, cluster kernels < 0.1 total. Sanity: 1.8 GB written in 3.98 ms = 450 GB/s and 186 MB in 0.23 ms = 800 GB/s, both under the 5090's 1.79 TB/s. (`blocks` mode reads 5.1 ms; the stage windows serialise.) |
| **backend total** | **198 / 205 (syntax), 196 / 200 (flat)** | `phases:` line |

Three more facts the numbers alone do not show:

- **Cold start.** The first run of the session compiled `apply_and_emit` for
  568 ms and the 93 MB upload waited behind it (the cubecl client is serialised),
  for a 670 ms backend. Every later run hit the NVIDIA driver's on-disk shader
  cache. cubecl-wgpu's `CompilationCache` is in-memory only without its `spirv`
  feature ("WGSL is compiled by the driver on every run", `cubecl-wgpu/src/compute/server.rs`).
  A renderer that opens a repo once pays this once per driver-cache miss; the
  M2 has a comparable Metal cache. **Measured** once, not interleaved.
- **Instanced cannot load this corpus here.** The chain allocates one slot
  buffer of `slots × 32 B` = 2.89 GB and cubecl panics: `can't allocate buffer of
  size: 2887507456` (`cubecl-wgpu/src/compute/server.rs:358`, reached from
  `tail_emit.rs:22`). HyperLayout chunks at the binding limit; the chain does
  not. **Measured**, 5 of 5 runs.
- **M2, quoted** from `out/history/CUBECL-PERFORMANCE-HANDOFF-2026-10-05.md`: 605–719 ms
  backend on the flagship, 235 ms of GPU kernel time (171 ms in
  `apply_and_emit`: 3 GB in 171 ms is 18 GB/s on a ~100 GB/s part — that kernel
  is not bandwidth-bound on Metal, or the timestamps include something else; I
  cannot tell from here). HyperLayout there: 168 ms. The chain has never been
  within 3× of the CPU on unified memory.

### 1c. Per frame

**Measured** (`GLYPH_PROFILE=1`, offscreen default repo camera, 3 frames): glyph
field pass 9–10 µs of GPU time, CPU cull 0.11 ms, 250 µs submit per frame, in
both field modes. The default camera sees almost nothing (every file is
LOD-culled to a backdrop), so this is a floor, not a typical view; the point is
that nothing is re-uploaded per frame and the draw side is not where the time
is. Culling is CPU-side per file (AABB vs frustum, then LOD) and per 512-slot
block only for files straddling a plane; draws are `(chunk, slot range)` pairs.

---

## 2. Is the scan the right shape?

The prefix-scan formulation treats the corpus as one 93 M-element sequence and
pays the textbook three-level structure (tile scan, spine, apply-and-chase) so
that every byte's row/column/x can be computed in parallel from a monoid. It is
correct — the gates say so — and the kernels run at memory speed. But it is
shaped for the wrong problem in three ways:

1. **It materialises what nobody reads.** 90 M slots, 1.8 GB, so that at most a
   few hundred thousand of them (bounded by screen pixels) are drawn in any
   frame. Both engines pay this; the chain pays it in VRAM at 450 GB/s, the CPU
   path in host memory and over PCIe. The scan cannot escape it because it has
   no notion of "visible".
2. **The hard parts are serial per line anyway.** Variable-width UTF-8, the
   greedy emoji-sequence chain (a later match depends on which earlier match was
   committed — `cluster.rs` has the counterexample), the syntax heuristic's
   `in_string`/`in_comment` state, and the f32 running x that must stay
   bit-exact to the oracle's summation order: all of these reset at `\n` and
   none of them carries across lines (block comments do not continue; the next
   line is a comment only if it starts with `*`). Code lines average ~30 bytes.
   The natural parallel unit is the line, with a thread per line doing the fold
   serially, and the 15-step `rank_step` ladder exists only because the monoid
   insists on byte-granular parallelism inside a line.
3. **Its CPU prologue is the CPU path.** The chain runs HyperLayout's Pass 1 to
   size its buffers, marshals every byte, and colours every byte on the CPU.
   What it offloads is Pass 2 — the 63 ms slot write — and what it saves on a
   discrete GPU is the slot transfer. The host-staging measurement above shows
   the CPU path can give that back without a GPU.

### 2a. Alternatives

**Per-line / per-chunk parallelism with CPU-computed offsets.** This is
HyperLayout. Pass 1 computes per-chunk slot bases; Pass 2 writes disjoint ranges
without locks. 68 ms of compute for 93 MB on 32 threads, memory-bound as the
earlier note says. Already the right shape for a whole-corpus layout; its cost is
the artefact, not the algorithm.

**Draw-time derivation / vertex pulling (upload bytes + a line table, resolve
glyph, advance and x in the vertex shader; no slot buffer).** Checked against
what the vertex stage can do (`$REG/naga-30.0.1/src/valid/mod.rs:608-616`,
`wgpu-types/src/features.rs:1142-1162`): no workgroup memory, no barriers;
subgroup ops in the vertex stage exist on Vulkan (`SUBGROUP_VERTEX`, NVIDIA
reports arithmetic subgroups in the vertex stage) and **not on Metal** (wgpu-hal
sets only `SUBGROUP | SUBGROUP_BARRIER`). So each vertex has to find its glyph's
x alone: for a pure-ASCII line that is `col × adv` (the clean-run trick the
chain's monoid already uses), for anything else it is a serial walk from the
line start or from a precomputed per-line exception table. Emoji trailers and
syntax colour need the per-line chain too. The design therefore degenerates into
"per-line tables, computed somewhere, plus a shader that reads them" — which is
design B with the compute moved into the vertex stage and multiplied by the
vertex count. Nobody ships this: Slug takes CPU-placed quads; Vello/Parley shape
and wrap on the CPU; GPUI lays out on the CPU and sprites the glyphs. The reason
is exactly the serial fold. Vertex pulling of a *compact placed record* — what
Derived mode does today — is sound; vertex pulling of *bytes* is not.

**Task/mesh shaders and compute-emitted indirect draws.** Status, from source
and this box (`vulkaninfo`): wgpu 30 has `EXPERIMENTAL_MESH_SHADER` with WGSL
`enable wgpu_mesh_shader;` (`@task`, `@mesh`) compiled through naga on Vulkan,
Metal and DX12 (the feature doc saying "naga only on vulkan" is stale; naga 30
ships MSL and HLSL mesh backends), `create_mesh_pipeline`, `draw_mesh_tasks`,
`draw_mesh_tasks_indirect`; the docs recommend `create_shader_module_trusted`
because workgroup zero-init is single-threaded. RTX 5090: `VK_EXT_mesh_shader`,
128 invocations per task/mesh workgroup, 256 vertices / 256 primitives, 32 KiB
output, preferred 32 invocations, NVIDIA's guidance 64 vertices / 126 primitives
per meshlet. Apple M2 (Apple8): mesh shading yes (Apple7+), **indirect mesh draw
arguments Apple9 only** — wgpu-hal calls `drawMeshThreadgroupsWithIndirectBuffer`
without a guard — and 1,024 mesh workgroups per dispatch (Apple9: 2^20). Indirect
draws: `multi_draw_indirect` everywhere; the `_count` variants are not on Metal;
wgpu's indirect-call validation pre-pass is on by default in release
(`InstanceFlags::VALIDATION_INDIRECT_CALL`) and lowers `max_buffer_size` to u32.
A "one meshlet per line" pipeline is expressible, but it still needs the per-line
table, its workgroup is a 32–64-glyph slice of a line, and it is experimental on
the one platform and capped at 1,024 workgroups without a task stage on the
other. Compute + a CPU-known indirect draw count (the CPU knows the visible set;
it culls today) does the same job on stable API.

**Design B — visible-set compute layout.** Resident on the GPU: the source bytes
(1 B/byte), a line table (16 B/line: byte start, length, item, base row ≈ 0.5
B/byte at 30 B/line), the 64 B `ItemParamsGpu` per file that Derived mode already
has. Per frame (or per change of the visible set): CPU culls files and lines as
now, uploads the visible-line list with slot bases from the per-line glyph counts
Pass 1 already produces, one compute invocation per visible line does the serial
fold — UTF-8, trie, sequence chain, wrap, paint — and emits `DerivedSlot`s into a
transient buffer sized by the screen; the existing Derived shader draws them.
Long lines (64 KiB minified JS) are the known pathology for a thread-per-line
kernel; HyperLayout's intra-line chunk cuts with seeded continuation
(`chunk.rs`, `chunk_initial_*`) are the CPU-side answer and translate directly
into "a visible-line entry may be a segment of a line with a seed".

### 2b. What each design breaks

From a read of every consumer of the slot index (details in the agent notes
behind this report; verdicts are mine):

| consumer | today | A (vertex pulling) | B (visible-set compute) |
|---|---|---|---|
| picking (`glyph_scene/pick.rs`) | CPU ray vs per-file AABB, then `rederive_item_records` for one file; the slot is only the output label | re-keyable | re-keyable |
| single-glyph verbs (`write_color/position/extent/group_id`), `geom_overrides` | keyed by slot; event-driven, never per frame | need a sparse override map keyed (item, byte) read by the shader | same |
| line/file styling (`write_placements`, `style_file`, highlight sidecars) | rebuilds placements from re-derived records to change colour | re-keyable to (item, line) / byte spans | same |
| selection mask | draws `(chunk, slot..slot+1)` through `record_draws` | needs a glyph address from (line, col) | the transient buffer has one: the visible line's slot base + col |
| culling | per-file AABB + 512-slot `BlockCull` boxes from Pass 2 | blocks become line ranges | same; the file AABB still needs Pass 1's extents |
| emoji sequences | `char_resolve.rs`, greedy, ≤ 9 codepoints lookahead, 380 KB trie | per-line pre-pass or trailer mask, else infeasible per vertex | the kernel runs the chain serially per line, as the CPU does |
| syntax paint | per line, no cross-line state | per-line pre-pass | in the kernel |
| hyper-oracle, repo-verify(-direct), cubecl-chain/-fork | compare slot bytes / records | need a new witness | **the same witness**: run the visible emitter over *every* line in a check and diff against `pass2_device` (the kernel's output is `DerivedSlot`, which the oracle already holds lane by lane) |
| pick-oracle, pixel-ab | pixels / pick log | survive | survive; `golden_equivalents` is the template for holding a new mode byte-equal to the baseline |

Nothing fundamentally needs per-glyph persistent storage. Two things need
per-glyph *work*: x and the sequence chain, both per-line serial. B does them
where they belong.

Also found on the way (not the question, but worth a ticket each): Derived's
shader reads `item_idx` and `group_id` from the same word, so a group override
also changes the item-table row; `derive_yz` omits `x_page × depth_per_col`
(harmless for repo loads, `page_cols = 0`); Derived's `write_extent` is a no-op
and `write_position` keeps only x; `LineRecord` and the Derived shader's header
comment describe a line table nothing builds; `GLYPH_CHAIN_SYNC` is read and
never used; the cubecl Derived path hands the renderer empty file tints
(code reading, not verified by running); `--repo-verify --field-mode derived`
on cubecl indexes read-back slots at stride 8 and **panics**
(`cubecl_layout.rs:408`, run on `g-pick-repo`, measured);
`bench_hyper.py` never captures Pass 1 on the default path and treats `ns` as
`ms`.

---

## 3. Bandwidth arithmetic, side by side

Per source byte, ASCII-dominated code (0.97 survivors per byte). "Load" is once
per repo; "frame" is per rendered frame. V = visible glyphs (≤ a few × 10^5 at
1600×1000), L_v = visible lines.

| design | resident GPU bytes | load: host writes | load: crosses PCIe | load: VRAM writes | per frame |
|---|---|---|---|---|---|
| HyperLayout Instanced, discrete (today) | 31 | 62 (zero + slots) | 31–62 | 62–124 | 0 |
| HyperLayout Derived, discrete (today) | 19.4 | 39 | 19.4–39 | 39–78 | 0 |
| HyperLayout Derived, host staging (measured switch) | 19.4 | 19.4 + 19.4 memcpy | 19.4 | 19.4 | 0 |
| HyperLayout Derived, M2 unified | 19.4 | 19.4 | 0 | 19.4 (mapped) | 0 |
| CubeCL Derived, discrete | 21–25 (+ bytes, flags, colours) | 1 (marshal) + 4 (colours, syntax) | 1 + 4 + per-item | ≈ 20 written, ≈ 6–9 read, GPU-internal | 0 |
| **B: visible-set** | **≈ 1.5** (bytes + line table) | ≈ 0 (bytes are already in RAM; a line table write ≈ 0.5) | **≈ 1.5** | ≈ 1.5 | read ≈ 30 L_v B, write 20 V B (≤ ~10 MB), both corpus-independent |
| A: vertex pulling | ≈ 1.5 | same as B | same as B | same as B | per frame per *vertex*: a walk from the line start unless per-line tables exist, i.e. ≥ B's work × 4–6 |

For this corpus that is: today 1.8–3.6 GB over the bus at load; B about 140 MB
once, then a few MB per frame. On the M2 the bytes are already in unified memory,
so B's load-time transfer is the line table alone.

---

## 4. Prototype: `experiments/gpu-direction/jit-layout`

A standalone wgpu 30 program (its own `[workspace]`, excluded from the
`experiments/` root — that workspace does not resolve on this box because two
members path-depend on a gitignored Zed checkout — and outside every gate;
~570 lines of Rust, a compute shader and a draw shader). It does design B in
miniature and nothing else: walk a corpus, build a 16 B/line table on the CPU,
upload bytes + table once, then per "frame" pick N visible lines at a random
offset, upload their list with CPU-prefixed slot bases, run one compute
invocation per line that folds the line serially (UTF-8 lead/continuation,
a 256-entry advance table, running f32 x, wrap at 120 columns with x reset, a
per-line `//`-comment colour state) into 20 B slots, and draw them as flat
quads. **What it does not model**, stated plainly: emoji sequences and the
trie, Slug coverage (identical for both designs, not under test), pagination,
the per-item table in the vertex stage, depth, a real camera (windows are
contiguous line ranges). Its walk keeps every valid-UTF-8 file, so it sees
100.3 MB / 9,144 files / 2,861,583 lines / 97.5 M glyphs where the renderer's
walk keeps 93 MB / 7,141 files.

**Measured** (5 repeats, ms, min / median; load average 0.7–1.6 throughout):

| what | min | median |
|---|---|---|
| CPU line table + per-line glyph counts, one thread, memchr | 54.1 | — |
| resident upload, bytes + line table (146 MB): `create_buffer_init` / submit+poll | 16.3 / 2.4 | 16.6 / 2.5 |
| status quo's movement for the same corpus: create 1,949 MB mapped staging + device buffer / 8-thread zero fill / unmap + copy + poll | 170 / 62 / 51 | 170 / 62 / 51 |
| N = 10 k visible lines (329 k slots): compute / draw / frame wall | 0.026 / 0.107 / 0.19 | 0.045 / 0.165 / 0.32 |
| **N = 100 k (3.37 M slots): compute / draw / frame wall** | **0.10** / 1.01 / 1.33 | **0.28** / 1.66 / 2.12 |
| N = 100 k plus one 64 KiB line: compute / frame wall | **8.98** / 10.1 | 9.05 / 10.9 |
| N = 1 M (33.9 M slots): compute / draw / frame wall | 3.07 / 16.6 / 20.2 | 3.71 / 16.7 / 22.9 |
| CPU on demand instead, N = 100 k: layout 1 thread / 8 threads / `write_buffer` of 67 MB | 2.84 / 0.76 / 2.54 | 4.81 / 1.34 / 8.06 |
| CPU on demand, N = 1 M: 1 / 8 threads / upload 680 MB | 48.7 / 23.5 / 65.9 | 50.0 / 24.6 / 69.2 |
| CPU, the single 64 KiB line, one thread | 0.072 | 0.072 |

Correctness: the whole 3.6 M-slot window of one N = 100 k frame read back
bit-equal to the CPU fold (x, row, glyph|wrap, colour, item) — **PASS**. The
timings mean something because of that line.

Reading it:

- **Load.** Resident footprint 146 MB against 1,949 MB; the one-time upload is
  ~19 ms against ~280 ms of pure movement for the status quo on this box (which
  is the same ~390 ms I measured inside HyperLayout, minus its re-read). The CPU
  newline scan is 54 ms single-threaded; Pass 1 already visits every line in
  5 ms on 32 threads, so the real line table is a by-product, not a new pass.
- **Per frame.** At 100 k visible lines — more lines than a 1600×1000 screen can
  show legibly — the compute layout costs 0.1–0.3 ms and the flat draw of the
  same 3.4 M quads 5–10× more. At 1 M lines it is still 3–4 ms. The GPU reads
  the visible bytes (3.4 MB) and writes 67 MB of transient slots; the host
  uploads 0.8 MB.
- **CPU on demand is competitive on layout and loses on transport**: 8 threads
  lay out the 100 k window in 0.8–1.3 ms, then spend 2.5–8 ms pushing 67 MB
  of slots through `write_buffer`. On unified memory that upload is a memcpy,
  and the CPU alternative is the one to compare against there (M2 item (c) in §5).
- **The long line is the finding that matters for the design.** One 64 KiB
  line is one serial GPU invocation: 9.0 ms, thirty to ninety times the whole
  100 k-line pass, and 125× the CPU's 0.072 ms for the same line. A thread per
  line is the right shape for code and the wrong one for minified JS, so the
  visible-line entry has to be a *segment* with a seed — exactly the intra-line
  chunk cut HyperLayout grew for C15 — or the kernel needs a cooperative
  per-workgroup scan for lines over a threshold. Untested here; it is the first
  thing milestone 2 below must get right, and `native/fixtures/chunk-cut.txt`
  is already the fixture for it.

---

## 5. Recommendation

**Retire the CubeCL chain as a production engine; keep the GPU, in a different
role.** Confidence: high (I would put 0.85 on "no shape of whole-corpus GPU
layout beats the fixed CPU path by enough to carry 12.6 k lines and a
pre-release dependency"); medium-high (0.7) on the visible-set design as the
next structural step — §4 puts its per-frame cost at a fraction of a
millisecond for any realistic view and its load-time movement at ~19 ms
against ~280, with the long-line segmentation as the one open design item and
the M2 still unmeasured.

Why retire, in order of weight:

1. On this discrete GPU the chain's whole advantage is not moving 1.8 GB of
   slots across PCIe. The CPU path's existing host-staging strategy recovers
   that advantage in full (204 vs 205 ms, measured interleaved), and its
   remaining movement cost has a clear path to ~150 ms. On unified memory the
   CPU path already wins by 3–4× (quoted).
2. The chain's kernels are already at memory speed (12 ms). The 190 ms it
   spends elsewhere is cubecl's upload path, shader JIT, and launch overhead —
   none of which a rewrite of the scan would touch, and a 568 ms cold compile on
   a driver-cache miss is a user-visible cost a renderer cannot schedule around.
3. It cannot load a corpus whose slots exceed `max_buffer_size` in Instanced
   mode (measured panic), it pins `cubecl =0.11.0-pre.4` (a pre-release whose
   wgpu line has to match ours on every bump), adds 232 crates to the dependency
   graph (655 → 423 without the feature), and its correctness witness
   (`cubecl-fork`) is HyperLayout against the chain — agreement with the oracle
   at one remove, which `hyper-oracle` now provides directly.

What would be deleted, and what is lost:

- `native/src/cubecl_chain/` (12 files, ~11.6 k lines), `cubecl_layout.rs`,
  `cubecl_scan.rs`, `cubecl_smoke.rs`; the `cubecl` Cargo feature and the two
  dependency lines in `native/Cargo.toml`; 54 `#[cfg(feature = "cubecl")]`
  sites outside those modules (`repo.rs`, `layout.rs` — `TintStore`,
  `keep_alive` —, `lib.rs`, `main.rs`, `cli/`, `gpu.rs` — `cubecl_device` on
  `SharedDevice` —, `windowed/state.rs`, `layout_hyper.rs`); `Strategy::Cubecl`
  and the `--repo-engine cubecl` spelling (keep the parser's refusal test);
  the `--cubecl-*` exit drivers and `GLYPH_CHAIN_*`/`GLYPH_RECORD_CHUNK`/
  `GLYPH_REPO_CHECK_*` env vars; gates `cubecl-chain` and `cubecl-fork` with
  `tools/check-cubecl.sh` and the mutations `emitter-ordinal-zeroed` and the
  one on `cubecl_chain/mod.rs`; `tools/gpu_profile/`; and the prose in
  `AGENTS.md`, `native/AGENTS.md`, `.agents/`, `README.md`,
  `research/desktop-platform-audit.md`. `out/history/CUBECL-PERFORMANCE-HANDOFF-2026-10-05.md`
  stays as history.
- **Keep** `native/fixtures/cubecl-fork/` (IMMUTABLE; `hyper-oracle` reads it in
  eight places and it is the only corpus with the m ≥ 3 / seg ≥ 3 paginate
  classes), `scan.rs` (the CPU scan reference, not cubecl), and the
  double-single fixed-point idea in `monoid.rs` if the visible-set kernel needs
  it (it should not: the kernel folds serially in the oracle's order).
- Lost: a second, independently written implementation of the fold that found
  a real HyperLayout bug once (the keycap class, 2026-10-09 — though it was
  `hyper-oracle` that adjudicated); `--cubecl-chain-bench`'s per-dispatch
  table (which, per the code, benchmarks the pre-fusion kernel set anyway);
  the Metal timestamp data in the handoff doc.

**Do first, this week, on the CPU path (no decision needed, output-neutral):**

- Make `stage_host_memory` the discrete default, or at least the default when
  ReBAR places the staging in device memory. 461 → 204 ms here. Then
  double-buffer its 64 MiB stream.
- Fold the emoji tint pairs during emission (the emitter has glyph and colour
  in registers) instead of re-reading slots. Free on the M2, 190 ms here.
- Fix `bench_hyper.py` to see Pass 1 on the default path (or print the prefetch
  thread's Pass 1 time), and treat `ns` correctly.

**Then the real question, Ivan's call: visible-set layout.** The design
sketch is §2a's B; what it changes is the ceiling, not a constant. Load time
becomes walk + Pass 1 + a ~140 MB upload (tens of ms here; on the M2 the bytes
are already resident). Memory becomes 1.5 B/byte instead of 20–31, so the
flagship's 1.9 GB field becomes 150 MB and a 1 GB monorepo becomes thinkable.
Per-frame cost is bounded by the screen. The staged milestones, each with a
gate:

0. Land the two CPU fixes above (battery green).
1. Pass 1 emits a line table (byte start, length, item, base row, glyph count)
   — it already visits every line; a unit test holds it to `fold::rows_for_line`.
2. A `--field-mode visible` behind a flag: a WGSL kernel, one invocation per
   visible-line entry (entries may be seeded segments for long lines), emitting
   `DerivedSlot` into a transient buffer, drawn by the existing Derived shader.
   Witness: `hyper-oracle` grows a sixth tier that runs the kernel over *all*
   lines of its corpora and diffs against `pass2_device` lane by lane (same
   record, same oracle). Not green until emoji sequences and paint agree.
3. Re-key the slot consumers by (item, byte): overrides map, selection mask,
   line-range culling. The pick path needs nothing.
4. `golden_equivalents` holds `--field-mode visible` byte-equal to the Instanced
   goldens on both rasterizers.
5. Default flip; retire Pass 2's whole-corpus device emission or keep it as the
   fallback for GPUs without the needed limits.

Risks, named: a second implementation of the fold in WGSL (the thing §2 says is
hard about the oracle, now in two languages — the gate in step 2 is the whole
answer, and the kernel is small compared with `pass2_device.rs`'s 1,807 lines
because it has no burst paths); long lines (the seed table exists on the CPU);
Metal has no `multi_draw_indirect_count` (the CPU knows the count); wgpu's
indirect validation (clear the flag or accept the pre-pass); and the Derived
shader's `item_and_group` conflation, which should be fixed before anything
builds on it.

**What would change my mind.** A corpus or machine where the chain beats the
host-staging CPU path by more than 2× end to end (I found none; the chain's
remaining 190 ms is not kernel time). An M2 measurement showing the visible-set
kernel above ~5 ms per frame for a realistic reading view, or that the M2's
unified Pass 2 is already fast enough that nobody cares about 168 ms (then the
visible-set design is a memory argument, not a latency one — still real at
1.9 GB). A feature that genuinely needs every glyph resident on the GPU at once;
I looked for one and found only things that work from bytes.

**For the M2** (not touched today; write down, measure later): (a) `bench_hyper.py`
flat and syntax on the flagship to re-baseline the 168 / 215 ms, with Pass 1
made visible; (b) `--repo-engine cubecl --field-mode derived` on the flagship
with `GLYPH_CHAIN_PROF=stages`, to see whether `apply_and_emit`'s 171 ms is real
(18 GB/s would mean the kernel, not the bus, is the limit on Metal — relevant
only if anyone argues for keeping the chain); (c) once §4's prototype exists,
run it on the M2 for the 10 k / 100 k / 1 M visible-line points, since that is
the platform where the bytes are already resident and the design has the most
to gain.

---

## 6. Method notes and caveats

- All HyperLayout and chain numbers are from the renderer's own log lines
  (`RUST_LOG=glyph3d_native=info`, `phases:` and span-close lines) parsed by
  `experiments/gpu-direction/bench_ab.py` and `bench_staging.py`; both print the
  load average before and after. The `hyper.staging.*` and
  `hyper.emoji_tint_pairs` spans were added today and are timing only.
- The default path hides Pass 1 (and, for cubecl, marshal and colouring) on the
  prefetch thread; "backend" excludes them for both engines alike. Pass 1 was
  measured through `--repo-engine batch`, which does not prefetch and is
  otherwise the same HyperLayout.
- cubecl's per-stage timestamps are the only kernel-time source; I believed them
  only after checking each implied bandwidth against the part's peak.
- The emoji re-read's cost depends on where wgpu's hidden staging lands; with
  ReBAR off it would be in host RAM and cheap. I did not toggle ReBAR.
- The corpus is pure crates.io Rust/markdown/etc.; a minified-JS-heavy corpus
  would exercise the long-line path, which none of today's numbers do.

## 7. Agent notes

Four read-only investigations fed §1–§3 (chain anatomy; HyperLayout anatomy and
the wgpu-core staging mechanics; the renderer's consumers of the slot index; the
platform survey of mesh shaders, indirect draws, vertex pulling and subgroups,
with `vulkaninfo` on this box), and one built §4. Their claims were checked
against the code where a number depended on them; where I could not run a
claim it is marked as code reading above.

## 8. Battery

`cargo glyph test` on this branch, after every change in it: `CHECK-ALL: ALL
GATES GREEN — 16 of 16 gates ran`, every golden view BYTE-EQUAL on
`vulkan-nvidia` in both field modes, `cargo glyph validate` PASS. The code
changes to `native/` are four spans and three env-gated switches defaulting to
the previous behaviour; the prototype and bench scripts live in `experiments/`,
outside every gate. `cargo glyph prove` was not run: no mutation's target moved.

---

## Linux scale (2026-10-09)

Follow-up, same day, after the retire decision: does the visible-set design
hold at Ivan's target — the whole Linux tree as one resident byte buffer plus a
line table, laid out per frame for what is in view? Measured with the
`jit-layout` prototype extended for it (`experiments/gpu-direction/jit-layout/`,
still its own workspace, outside every gate; nothing under `native/` was
touched in this pass). Corpus: a Linux checkout at HEAD 2026-10-09 (1.8 GB on
disk, 95,954 files), walked with the renderer's own rules (its `SKIP_DIRS`,
`SOURCE_EXTENSIONS`, 10 MiB cap, UTF-8 check): **74,313 files kept, 1,370 MB,
38.19 M lines, 1,331.8 M glyphs** (10 files skipped for size, 0 non-UTF-8).
Same box as above; the GPU was shared with the coordinator's battery
throughout — load average 5.8–10.5 across these runs — and every figure is the
min / median of 5 interleaved repeats.

### What HyperLayout would need for this tree (inferred, not attempted)

1,331.8 M glyphs × 20 B = **26.6 GB** of Derived slots, × 32 B = **42.6 GB**
Instanced. `max_buffer_size` here is 4.3 GB, so 7 or 10 chunk buffers;
Derived would take 82 % of this card's 32.6 GB VRAM plus, on the discrete
staging path, the same again in host RAM for the duration of the load;
Instanced does not fit in VRAM at all, and neither fits a 32 GB Mac. I did
not try: on a shared 60 GB box the attempt would have paged. The arithmetic is
the result. For comparison the **visible-set design's resident set is
1,678 MB** — bytes 1,370 + line table 305.5 (8 B/line: `byte_start`, glyph
count) + item table 2.4 (32 B/file) — in two 1 GiB byte chunks because the
binding limit is 2^31−4 (items padded so none straddles; the kernel picks the
chunk with a shift).

### The part that does not go away: Pass 1 (measured)

Walk and read, like the renderer: enumerate 247 ms (serial), read + UTF-8 check
334 ms on 32 threads (4.1 GB/s from page cache), concatenate 84 ms. Then the
line table and per-file extents over all 1.37 GB, one file per task under
`std::thread::scope`:

| threads | min / median ms | GB/s at min |
|---|---|---|
| 1 | 759 / 879 | 1.8 |
| 4 | 255 / 276 | 5.4 |
| 8 | 150 / 156 | 9.1 |
| 16 | 92 / 106 | 15.0 |
| 32 | **79 / 86** | **17.5** |

It scales almost linearly to 16 threads and 3.4× from 4 to 32, so this pass
is **compute-bound per thread, not bandwidth-bound**; it reaches DRAM speed
only with every core. That is consistent with the earlier memory note once you
read it the right way round: HyperLayout saturates DRAM at one thread because
it *writes* 20–31 B per source byte, and this pass writes ~0.2. (HyperLayout's
own Pass 1 on the 93 MB tree was 5 ms = 18.6 GB/s on 32 threads — the same
ceiling, reached by its pure-ASCII closed form.) **Inferred for the M2** at the
4-thread rate: 250–350 ms for the whole Linux tree; `m2/03_jit_layout.sh`
measures it.

Resident upload, 1,678 MB through `create_buffer_init`: 206 / 212 ms, plus
21 ms submit + poll — wgpu's write path at ~8 GB/s, and the one load-time cost
in this design that is proportional to the corpus and crosses the bus. On
unified memory it should be a memcpy or nothing; that is the M2 question.

So the whole load at Linux scale on this box is roughly **1.0 s**: 0.67 s of
file I/O and concatenation that every design pays, 0.08 s of Pass 1, 0.23 s of
upload. The status-quo path cannot complete at all.

### Per frame (measured, segments at 2 KiB)

| view | lines | slots | compute GPU | draw GPU (flat quads) | CPU list + upload | frame wall |
|---|---|---|---|---|---|---|
| **page** — 60 lines of `kernel/bpf/verifier.c` | 60 | 1,380 | **0.017 / 0.017** | 0.003 | 0.003 | 0.074 / 0.081 |
| **overview** — first 50 lines of 2,000 files | 91,199 | 2.53 M | **0.215 / 0.216** | 1.24 | 1.26 / 1.30 | 1.64 / 1.67 |
| **worst** — the 1,000 longest lines in the tree | 1,000 (1,041 segments) | 1.06 M | **0.50 / 0.51** | 0.52 | 0.06 | 1.13 / 1.24 |
| random, 100 k lines | 100,000 | 2.56 M | 0.13 / 0.13 | 1.44 | 0.36 | 1.76 / 1.77 |
| random, 1 M lines | 1,000,000 | 26.7 M | 2.5 / 2.6 | 16.3 | 3.9 / 4.1 | 19.4 / 19.5 |

Layout is never the frame's cost: at the overview the compute pass is 0.2 ms
and the 2.5 M-quad flat draw six times that; at a million lines (more than any
screen) it is 2.5 ms against a 16 ms draw. The CPU's share is building and
uploading the visible list (32 B per visible line here; 1.3 ms at the
overview), which a GPU-side cull over the resident line table would remove —
the table is uploaded and resident, this kernel just does not read it yet.

### Long lines: seeded segments (implemented, measured, verified)

The kernel now takes **segments**, not lines. Pass 1 cuts any line longer than
`--segment-bytes` on HyperLayout's rule — only before an ASCII byte, so no
sequence can straddle a cut — and snapshots the continuation's seeds during
its serial walk: the column, the running **f32** segment advance since the
last wrap-unit boundary (the fold's own carrier, as `continued_segment_advance`
does it, never `col × adv`), and the paint state. Every slot of every window
read back **bit-equal to the single-threaded whole-line CPU fold, x bits
included**: the 100 k random window (2.56 M slots), the worst view at S =
65536 / 2048 / 512 (1.06 M slots each), and the repo's own `native/` tree,
whose fixtures carry 250 KB lines (1,529 segments). That is the witness
milestone 2 needs, in miniature.

What segmentation buys on the worst view: S = 64 KiB (off) 1.41 ms → S = 2 KiB
0.50 ms → S = 512 B **0.14 ms**, ten times. And a fact about the corpus: **the
Linux tree has no 64 KiB-line pathology** — its five longest lines are
6,833 / 4,496 / 4,496 / 4,022 / 3,542 B, all in `tools/testing/selftests/hid/`
test data. The earlier 9 ms single-thread figure was a synthetic minified
line; real code trees are short-lined, and the segment seed makes the
remaining long ones cheap.

### Pixels

`experiments/gpu-direction/linux-overview.png` (2,000 files, per-file tint,
quads clamped to 1 px) and `linux-page.png` (60 lines of `verifier.c`,
indentation and comment colour visible): the prototype's own offscreen frames,
800×500, flat quads where the real renderer's Slug pass would fill glyph
coverage. No text or paths in either.

### Does the design hold at this scale?

Yes, on this box, for everything the prototype models: 1.68 GB resident where
the current path needs 26–43 GB and cannot load; a one-second load that is two
thirds file I/O; sub-millisecond layout for any realistic view; long lines
handled by the seed rule HyperLayout already owns, bit-exact.

What it does not model, so the biggest unknowns left, in order:

1. **The M2.** Unified memory is where the design has the most to gain (no
   upload) and where Pass 1 has four performance cores, not 32: the 250–350 ms
   estimate for Linux is inferred. `m2/run_all.sh` is the plan.
2. **Emoji sequences and the atlas trie in the kernel.** Non-ASCII here is one
   codepoint, advance 1.0. The 380 KB trie binds trivially; the greedy
   sequence chain runs serially per segment as on the CPU; untested in WGSL.
3. **Paint parity.** A `//` flag stands in for `colorize_leaders`'s five-state
   heuristic; the C17 lesson (per-line colouring must agree with the whole-item
   colouriser at every cut) applies to segments exactly as it did to chunks.
4. **Culling from the GPU side.** The CPU currently builds the visible list;
   at a million lines that is 4 ms of CPU and a 32 MB upload per frame.
   Resident line table + a cull kernel + an indirect draw is the obvious next
   step and was not built.
5. **The draw itself.** At 1 M visible lines a 26.7 M-quad draw is 16 ms even
   flat; the real limit on "how much can be on screen" is coverage, not
   layout, and LOD/backdrop substitution (which the renderer already does per
   file) is what keeps the overview honest.
6. **Walk time.** 0.67 s of the 1.0 s load is reading 74 k files. Nothing
   GPU-side helps that; mmap or a persistent corpus cache would.

---

## Round 2 (2026-10-09/10)

After the retire decision and the Linux-scale section: the M2 for a round, real
Slug text instead of quads, the atlas and emoji sequences resident on the GPU,
colour and per-glyph attributes by byte range with transient slots, and
culling made explicit. Two machines: this box (RTX 5090 / Vulkan, 32 cores,
shared with the coordinator's battery, load 1–15) and the M2 (Apple M2, 4P+4E,
16 GB unified, Metal, fanless; jobs run through a tmux runner with cool-downs
between runs and alternating order; load 2–15 because the research build and
the Linux clone ran minutes before — the A/A floor there is ~8–9 ms on a full
load, and every M2 figure below should be re-run on an idle machine before it
is quoted as a constant). Everything **measured** unless marked; min / median
of 5 repeats unless noted. Code: `experiments/gpu-direction/jit-layout` (cull,
spans, dense attributes) and the new `experiments/gpu-direction/jit-text`
(Slug, trie, emoji), both outside every gate; nothing under `native/` changed.

### 1. M2: the production path, re-baselined (measured)

Flagship (97 MB, 1,306 files, 95.2 M instances), `bench_ab.py`, 6 rounds
alternating, 12 s cool-down, load 6.7 → 15.6:

| configuration | backend min / median ms | Pass 2 | emoji tint re-read | visual init |
|---|---|---|---|---|
| hyper Derived syntax | **152 / 156** | 136 / 147 | 8.1 / 8.7 | 163 / 171 |
| hyper Derived flat | 150 / 156 | 136 / 147 | 7.9 / 8.5 | 167 / 173 |
| hyper Instanced syntax | 232 / 255 | 223 / 233 | 9.1 / 21.7 | 240 / 273 |
| batch Derived syntax (no prefetch; Pass 1 visible) | 167 / 170 | 145 / 148 | 8.0 / 8.7 | 175 / 178 |

Pass 1: 10.6 / 10.8 ms on 8 cores. So on unified memory **Pass 2 — writing
1.9 GB of Derived slots — is 94 % of the backend**, the Instanced 32 B slot
costs 90 ms more than the 20 B one for the same glyphs (write-bound), the
emoji re-read that cost 193 ms on the discrete box costs 8 ms here (cacheable
mapped memory), and syntax colour is free on both machines. The quoted 168 ms
is today's 156.

### 2. M2: the prototypes on unified memory (measured; see §6 for the caveats)

**Linux tree** (the M2's clone is a day newer than this box's: 74,319 files,
1,370 MB, 38.20 M lines, 1,294.8 M glyph slots in `jit-text`'s count, which
resolves real glyphs):

| stage | M2 | this box, for reference |
|---|---|---|
| walk: enumerate / read + UTF-8 (8 threads, first touch) | 457 / **2,284** ms | 247 / 334 (warm cache) |
| Pass 1 with real glyph resolution, 8 threads | **310–334 ms = 4.1–4.4 GB/s** | 128–138 ms = 10 GB/s on 32 threads |
| resident upload, 1,678 MB (`create_buffer_init`) | **1,004–1,106 ms** | 206–227 ms |
| page (60 lines of `verifier.c`, 1,326 slots, 12.8 px/em): kernel / Slug draw / flat draw | 0.060 / 0.29 / 0.05 | 0.030 / 0.017 / 0.004 |
| overview (2,000 files × 50 lines, 2.46 M slots, 0.44 px/em): kernel / Slug / flat | 1.5 / **13.7** / 9.8 | 0.31 / 1.23 / 1.20 |
| worst (1,000 longest lines, 1.06 M slots, 2 KiB segments): kernel / Slug / flat | 1.7 / 6.4 / 4.4 | 0.78 / 0.50 / 0.50 |

The M2's Pass 1 lands where the previous section inferred (250–350 ms). Two
numbers are the finding: the **resident upload is a second** — wgpu's Metal
`create_buffer_init` path for 1.68 GB, where the renderer's own unified path
(`create_mapped_slot_buffer`) would have Pass 1 write the table straight into
a mapped buffer and the bytes need not be copied at all; and **the overview's
Slug draw is 14 ms** for 2.5 M sub-pixel glyphs (flat quads 10 ms), i.e. on
the M2 the draw of a zoomed-out view, not its layout, is the frame — exactly
what the renderer's per-file LOD backdrop exists for, and with LOD on (§5)
that view lays out nothing.

**Flagship** on the M2 (97 MB; its JS lines are long, so 60 lines are 15 k
glyphs): Pass 1 20.6–22.7 ms = 4.3–4.7 GB/s, resident upload 110 MB in 20 ms —
**against HyperLayout's 156 ms backend, the visible-set load is ~45 ms**.
Page: kernel 1.36–1.46 ms (63 segments of ~240 glyphs: too few invocations to
fill the GPU, latency-bound — 512 B segments are the fix), Slug 2.4–3.1 ms,
flat 0.13. Overview (3.0 M slots): kernel 2.4, Slug 16.8, flat 12.0. Worst
(1,000 lines, 4.1 M slots): kernel 4.7–4.9, Slug 27, flat 18.6.

### 2a. M2 `jit-layout` rows (Pass-1 sweep, GPU-cull cameras, views) — measured, load 1.3–2.9

**Pass 1 (line table + extents, no glyph resolution) by thread count**, two
runs each: flagship 97.1–97.8 / 32.6–32.7 / 19.8–20.5 ms at 1 / 4 / 8 threads;
Linux 1,459 / 438–443 / 299–301 ms (0.94 → 3.1 → 4.6 GB/s). Four performance
cores give 3.0–3.3×, the four efficiency cores another 1.5×. The walk's
`read + UTF-8` of the fresh Linux clone is the slow part of the M2 load:
4.9 s on one thread, 1.5–2.5 s on 4–8 (280–940 MB/s from disk, not cache);
the flagship, cached, reads at 7 GB/s in 14 ms.

**Views (CPU-chosen lines, flat quads, 800×500, no LOD)**, min / median ms:

| corpus, view | slots | layout GPU | draw GPU | CPU list + submit | frame wall |
|---|---|---|---|---|---|
| Linux page (60 lines) | 1,380 | 0.113 | 0.084 | 0.09 / 0.11 | 0.48 / 0.49 |
| Linux overview (2,000 × 50) | 2.51 M | 2.79 / 2.82 | 24.8 / 25.4 | 1.15 / 1.19 | 28.2 / 28.9 |
| Linux worst (1,000 longest, 2 KiB segments) | 1.06 M | 1.53 / 3.31 | 10.1 / 14.8 | 0.19 | 12.2 / 18.8 |
| Linux random 100 k lines | 2.58 M | 1.86 / 2.21 | 21.3 / 24.2 | 0.78 / 0.82 | 23.7 / 29.0 |
| flagship page (60 JS lines) | 15,234 | 1.14 / 1.17 | 0.19 / 0.21 | 0.08 / 0.10 | 1.62 / 1.76 |
| flagship overview | 3.00 M | 1.72 / 1.78 | 18.2 | 0.60 / 0.62 | 20.6 / 20.7 |
| flagship worst (1,000 longest) | 4.11 M | 2.92 / 3.06 | 36.9 | 0.21 / 0.24 | 40.6 |
| flagship random 100 k lines | 5.83 M | 2.02 / 2.09 | 35.6 | 0.74 / 0.85 | 38.4 / 38.6 |

The flagship's worst view, layout only, by segment size: **64 KiB segments
(14 minified lines as single invocations) 31.4 / 31.8 ms; 2 KiB 3.68 / 3.88;
512 B 4.04 / 8.07** — on the M2 the segment seed is not a refinement, it is
the difference between 31 ms and 4. Every readback check passed on Metal
(worst at 64 KiB / 2 KiB / 512 B, random 100 k, Linux worst and random: all
slots bit-equal, x bits and colour included); every pick check passed (CPU
pick 0.09 ms per 1,000 probes; GPU pick 0.5–1.7 ms per 1,000 in one dispatch,
0.24–0.27 ms submit-to-poll for one). A fixed ~0.4–0.8 ms of submit-to-poll
sits under every M2 frame (the page view's wall is 0.48 ms around 0.2 ms of
GPU work): Metal's command-buffer round trip, the floor for any design.

**GPU cull cameras (LOD 1 px), 800×500**, min ms:

| corpus, camera | files / backdrops | visible lines | slots | cull A / prefix / B / final | layout | draw | CPU | wall |
|---|---|---|---|---|---|---|---|---|
| Linux page (one row = 8 px) | 1 / 0 | 64 | 1,509 | 0.056 / 0.096 / 0.026 / 0.007 | 0.119 | 0.091 | 0.21 | 1.02 |
| Linux overview (0.045 px) | 0 / 2,782 | 0 | 0 | 0.059 / 0.096 / 0.008 / 0.007 | 0.008 | 0.111 | 0.20 | 0.95 |
| Linux zoomout (0.014 px) | 0 / 20,636 | 0 | 0 | 0.066 / 0.096 / 0.008 / 0.007 | 0.008 | 0.489 | 0.21 | 1.35 |
| Linux far (0.006 px) | 0 / 74,319 | 0 | 0 | 0.075 / 0.096 / 0.008 / 0.007 | 0.008 | 1.519 | 0.20 | 2.47 |
| flagship page (3.2 px) | 5 / 0 | 640 | 220,049 | 0.006 / 0.045 / 0.024 / 0.004 | 1.31 | 1.89 | 0.19 | 4.04 |
| flagship overview / far | 0 / 2 ; 0 / 1,306 | 0 | 0 | ~0.06–0.12 total | 0.004–0.008 | 0.03 / 0.08 | 0.18–0.19 | 0.78–0.80 |

Cull check PASS at every camera on Metal (the GPU's visible set equals a CPU
frustum test as a set). With LOD on, the M2 frame for any zoomed-out view is
the backdrop draw plus the fixed round trip — 1–2.5 ms at Linux scale with
74 k backdrop quads — and a reading view is about a millisecond. The 2,782 /
20,636 / 74,319-backdrop rows are also the honest answer to "what does the
whole tree cost per frame": the draw of one quad per file.

### 3. Real text: Slug shading, resident atlas, emoji sequences on the GPU (this box, measured)

`jit-text` draws the kernel's `DerivedSlot`s with a verbatim copy of
`glyph_field_derived.wgsl` and the real atlas (82,239 curves 2.5 MiB, glyph
map 160 KiB, 9,427 advances, the 361 MiB emoji sheet with mips, decoded and
uploaded in ~86 ms here / 136 ms on the M2), depth and blend state from
`field_core.rs`, reverse-Z perspective camera, 1600×1000.

**Correctness first.** `--check` links `glyph3d-native` as a library and holds
every slot's glyph id, advance bits, x bits and wrap segment to HyperLayout's
own answer: **44 of 44 PASS** over `g-cluster-repo/`, `emoji-corpus-small.txt`,
`emoji-view.txt`, `cubecl-fork/clusters.txt` (39,595 slots), `overflow-leads.txt`
and `g-pick-repo/` (`wide.txt` 396,531 slots with intra-line cuts; `empty.rs`
0), each × {wrap 0, wrap 100 back} × {lookup A, lookup B}. A deliberate fault
(FE0F left in the sequence key) reddens 8 of them (`#️⃣` → `#`), so the check
can fail. **The same 44 of 44 PASS on the M2 under Metal** (run as a separate
job; `wide.txt` 396,531 slots, `clusters.txt` 39,595), so the kernel's answer
is the CPU's on both rasterizers — the property the pixel gates could never
claim across vendors, held here at the slot level. Emoji sequences — ZWJ families, flags, keycaps, skin tones, the
keycap-base guard on the ASCII fast path — run serially per segment on the
GPU, ported from `char_resolve.rs`. Bit-equal x without f64: every atlas
advance is 0, 1 or 2 cells (asserted at load), so the f64 sum of f32 advances
is exact and `f32(cells) × cell_adv` rounds identically.

**Lookup variants, resident bytes.** A, the trie as shipped: index 17,408 +
packed blocks 39,936 + head bitmap 139,268 + sequence section 183,304 =
**380 KB**. B, Ivan's shape: direct 65,536-entry BMP table 262,144 + astral
hash 32,768 + sequence-key hash 131,072 + sequence section = **609 KB**. Both
pack one u32 per entry (glyph | cells << 16 | starts-a-sequence); A is two
dependent loads, B one. Either is trivially resident.

| view (Linux) | slots | px/em | kernel A | kernel B | Slug draw | flat draw | real-text delta |
|---|---|---|---|---|---|---|---|
| page | 1,326 | 12.84 | 0.030 | 0.030 | 0.017 | 0.004 | 0.013 |
| overview | 2,480,612 | 0.44 | 0.314 / 0.327 | 0.314 / 0.318 | 1.23 | 1.20 | 0.026 |
| worst, 2 KiB segments | 1,059,948 | 0.77 | 0.780 | 0.781 | 0.504 | 0.503 | 0.002 |
| worst, 512 B segments | 1,059,948 | 0.77 | 0.215 | 0.215 | 0.561 | 0.522 | 0.039 |

Binding the emoji sheet versus a 1-texel stand-in: no measurable difference on
any view. On plain text the two lookups tie exactly; on emoji-heavy input they
part: `emoji-corpus-large.txt` (9,538 slots) A 0.79 vs **B 0.30 ms**, its page
A 0.60 vs B 0.27, `g-cluster-repo` A 0.165 vs B 0.066. The trie's cost is not
its two loads but the per-probe binary search over 4,166 sequences; B's hashed
sequence keys remove it. So: Ivan's O(1) shape is right for sequences and a
wash for codepoints, at +230 KB.

### 4. Colour and per-glyph attributes by byte range, slots transient (this box, measured)

Design, in `jit-layout`: a global span table of 8 B entries
`{start, len:24 | palette:8}` with `span_base / span_count` per item and a
4 B per-line index of the first span that can cover the line; the kernel
walks spans and bytes together (current span in registers, next prefetched; a
continuation segment binary-searches once), so colouring is O(bytes + spans in
the line). Per item a **representation**: none / spans / dense colour / dense
colour + transform. Dense files carry a 4 B colour per glyph and optionally an
8 B f16 transform (dx, dy, dz, scale), indexed by the glyph's ordinal in the
file — the line table's glyph prefix plus the column — so the "deranged 1:1"
file pays today's per-glyph price and nobody else does; the kernel branches
once per segment, and the transform rides a parallel transient stream the
draw reads.

Resident, Linux, with demo spans (the `//` heuristic plus every identifier,
0.83 spans per 10 B — denser than real LSP tokens are likely to be):
**114.2 M spans, 913 MB used / 1,051 MB allocated** (12.5 % + 16 slack per
file) **+ 153 MB per-line index**; total resident 2.89 GB. The span table is
within sight of the 2 GiB binding limit — the next scaling wall if token
density doubles; real spans should be measured before that is a worry.

| frame | layout, no spans | layout, spans |
|---|---|---|
| page | 0.023 | 0.028 |
| view overview (2.5 M slots) | 0.358 / 0.394 | 0.423 / 0.496 |
| view 1 M lines (26.5 M slots) | 1.93 / 1.96 | 2.23 / 2.61 |
| camera overview, LOD off (50.9 M slots) | 3.79 | 4.26 |

Dense cases: (a) `verifier.c` as a normal span file, 75,106 spans, 0.6 MB,
page layout 0.028; (b) `verifier.c` fully dense, colour + transform, 635,781 ×
12 B = **7.6 MB**, page layout **0.039** (vs 0.028 spans, 0.023 none); (c)
camera overview with **700 of the 2,782 in-frame files dense** (17.2 M glyphs,
**207 MB**): **5.52 / 5.54 ms** vs 4.26 spans-only; colour-only dense 69 MB,
4.35 / 5.57. Edit cost: `verifier.c` spans rewritten in place (75,106 → 67,714,
within its slack) 0.14–0.60 ms CPU for 21,550 line indices + 0.06–0.16 ms
`write_buffer`; the dense array (7.6 MB) 9–11 ms CPU to regenerate + 0.6 ms
upload; a span set that outgrows its slack is appended and remapped, the old
range becomes a hole, nothing compacts. **Picking by (file, byte) with no
per-glyph storage: PASS** — a CPU function and a GPU micro-kernel (binary
search on `byte_start` for the line, the covering segment, a count from the
segment start) agree on 1,000 random probes at every camera, including 219
probes into dense files and the 2.1 M-segment far frame; the read-back slot
carries the probe's item, row and glyph. Layout with colour and transforms:
bit-exact against the whole-line CPU fold before and after the edits.

### 5. Explicit culling (this box, measured)

**What the earlier per-frame numbers assumed, plainly:** the visible list was
*chosen* on the CPU from a view definition — a line window, or 2,000 files ×
50 lines — with no frustum test. "What's displayed" was picked, not culled.
Those `--view` paths still exist and are labelled that way in the tool.

Now, `--cull gpu` with `--camera page | overview | zoomout | far | dense`:
every file sits on a fixed shelf (821 columns, ~98 k × 98 k world units) with
a resident 32 B box; a perspective camera looks down; four passes — per-file
box test, a one-workgroup prefix, per-line box test dispatched indirectly,
finalize — then indirect layout and indirect draw, **no readback on the frame
path** (the counter readback used for the tables is timed apart: 0.03–0.17 ms).
Files whose rows project under `--lod-px` (default 1) become one backdrop quad
and are not laid out. Segment and slot order is set by atomics and is **not
stable** between frames.

| camera, LOD 1 px | cull | files / backdrops | visible lines | slots | cull A / B | layout | draw | CPU | wall |
|---|---|---|---|---|---|---|---|---|---|
| page | gpu | 1 / 0 | 64 | 1,509 | 0.003 / 0.007 | 0.025 | 0.002 | 0.09 / 0.13 | 0.13 / 0.15 |
| overview | gpu | 0 / 2,782 | 0 | 0 | 0.004 / 0.006 | – | 0.004 | 0.09 | 0.11 |
| zoomout | gpu | 0 / 20,498 | 0 | 0 | 0.003 / 0.005 | – | 0.011 | 0.05 | 0.11 |
| far | gpu | 0 / 74,313 | 0 | 0 | 0.004 / 0.005 | – | 0.038 | 0.04 | 0.13 |
| far | cpu | 0 / 74,313 | 0 | 0 | 0.40 (CPU) | – | 0.038 | 0.43 | 0.09 |
| dense (rows 1.5 px) | gpu | 7 / 0 | 1,996 | 54,282 | 0.003 / 0.008 | 0.033 | 0.028 | 0.04 | 0.16 |
| dense, 2560×1440 | gpu | 51 / 0 | 16,184 | 458,322 | 0.003 / 0.011 | 0.046 | 0.227 | 0.09 | 0.39 / 0.40 |

With honest LOD, 2,000 files is sub-pixel at any resolution (one row is
0.045 px at 800×500, still sub-pixel at 2560×1440), so overview, zoomout and
far lay out nothing and cost under 0.05 ms of GPU. The LOD-off rows are the
worst case — every line in the frustum laid out and drawn as 1 px quads:

| camera, LOD off | cull | visible lines | slot demand | cull | layout | draw | CPU | wall |
|---|---|---|---|---|---|---|---|---|
| overview (2,782 files) | cpu | 1,867,742 | 50.9 M | 15.6 (CPU) | 3.75 | 24.96 | 30.0 / 31.0 | 29.3 |
| overview | gpu | 1,867,742 | 50.9 M | B 0.28 | 3.79 | 24.96 | **0.11 / 0.21** | 29.6 |
| zoomout (20,498 files) | gpu | 12.9 M | 476 M (10.8 M lines dropped at the cap) | B 2.08 | 5.4 | 32.9 | 0.17 | 41.0 |
| far (74,313 files) | cpu | 38.2 M | 1.33 G | 291 (CPU) | 4.9 | 37.1 | 570 / 632 | 43.6 |
| far | gpu | 38.2 M | 1.33 G (35.9 M dropped) | B 5.99 | 4.8 | 33.0 | 0.21 / 0.25 | 45.2 |

GPU culling removes the CPU from the frame (31 ms → 0.1–0.2 at the overview;
290 ms of single-threaded CPU cull at far → 6 ms of cull B). With LOD off the
frame is the **draw** of 50–67 M sub-pixel quads (25–33 ms), not cull or
layout. The caps (64 M slots = 1.34 GB, 4 M segments) bind at zoomout and far;
a line that does not fit is dropped in atomic order, so the picture has random
holes — the cap is a safety net, LOD is the policy. **Cull check PASS**: at the
overview with LOD off, the GPU's visible set equals a CPU frustum test as a set
(2,782 files, 1,867,742 lines, 50,900,152 slots); page and dense pass as full
sets; zoomout and far, where the cap binds, pass on counts.

### 6. What still does not hold, and what needs Ivan

Holds now, on both machines: layout from resident bytes bit-equal to
HyperLayout including emoji sequences; real Slug text at 0.01–0.04 ms over flat
quads here and 0.24–8 ms on the M2 depending on how many sub-pixel glyphs a
view draws; colour and dense attributes by (file, byte) with transient slots;
picking by (file, byte) with no per-glyph storage; culling on the GPU with no
readback. Does not hold yet, or needs a decision:

1. **The M2's resident upload (1.0–1.1 s for 1.68 GB) and first-touch walk
   (2.3 s)** are the load. The upload is a wgpu Metal write path the renderer
   already avoids for slots (`create_mapped_slot_buffer`): Pass 1 should write
   the line table into a mapped buffer and the byte buffer should be the
   mapped read target itself. **Inferred**, not measured: that takes the upload
   to ~0. The walk is file I/O; a persistent corpus cache or mmap is the lever.
2. **Zoomed-out views on the M2 draw-bound at 14–27 ms** for 2.5–4 M sub-pixel
   glyphs. LOD backdrops (§5) make those views free, and the renderer already
   has per-file LOD; the open question is policy — how much real text Ivan
   wants at the zoom levels where glyphs are a fraction of a pixel. Decision.
3. **Small visible sets with long lines are latency-bound on the M2** (63
   segments → 1.4 ms kernel). Shorter segments (512 B: 3.6× faster at the
   worst view here) fix it; a seed table per 512 B of every line is ~0.1 % of
   the bytes. Not yet measured on the M2.
4. **Span table size at Linux scale** with demo density: 1.05 GB + 153 MB. Real
   LSP spans are probably sparser; dense files add 4–12 B/glyph each. Policy
   needed: which producers get spans, which files go dense, and whether the
   load-time syntax heuristic survives as a span *producer* (it can: it is
   cheap and would keep today's colouring until LSP spans arrive). Decision.
5. **Unstable slot order from atomic culling.** Picking and selection are
   addressed by (file, byte) and do not care, but anything that caches by slot
   index between frames (the Derived field's in-place colour writes, the
   selection mask by slot range) has to be re-keyed. Known, designed around in
   §4, not yet implemented in the renderer.
6. **Pagination** (every repo file is paged in the renderer) is modelled in no
   prototype; the per-item page frame is a pure function of row/col and the
   fold already owns it (`layout_hyper/page.rs`), so this is porting, not
   research — but it is on the list.
7. **Noise.** The M2 ran at load 3–15 during these jobs; the per-frame medians
   agree with their mins to within the A/A floor except where noted, but the
   headline M2 numbers should be re-taken idle before anyone quotes them.
8. **`glyph3d-native` does not build with `default-features = false`**
   (`windowed/state.rs` uses `egui` ungated); `jit-text` pins `egui-ui`.
   Worth a small fix on main.

Frames from this round: `experiments/gpu-direction/linux-text-page.png` (real
Slug text, 60 lines of `verifier.c`) and `linux-text-overview.png` (2,000
files at 0.44 px/em, greeked by the shader's own rule). M2 frames are in the
M2's `out/gpu-research/` and were not copied back.
