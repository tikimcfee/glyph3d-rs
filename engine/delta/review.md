# Adversarial review of the four delta reports

Reviewed: `data-path.md`, `layout-compute.md`, `interaction.md`, `atlas-emoji.md`
(all in `engine/delta/`), plus `engine/BACKEND-PLAN.md` and `engine/PLAN-DRAFT.md`.

Method: every load-bearing claim below was re-checked against source in both trees.
Where a compiler or a parse could settle it, one did — `naga` 30 computed the WGSL
struct layouts, `encase` computed the Rust ones, the atlas `.bin` files were re-parsed
from `FORMAT.md`'s record layouts by an independent script, and `nm -gU` read the
built dylib's exports. Nothing here rests on a grep alone except where said so.

**The reports are accurate.** Line citations are correct to within a few lines
everywhere I spot-checked, the measured numbers reproduce, and the four independently
converged on the same architecture picture. The findings below are corrections at the
edges and one place where three documents disagree about the same four bytes.

Counts: **6 contradictions** between the reports, **8 claims refuted or materially
weakened**, and in the two older plan documents **7 dead claims** (D-B1, D-B2, D-B5,
D-P1 through D-P4) plus **4 corrections** that supersede rather than falsify
(D-B3, D-B4, D-P5, D-P6).

---

## (a) Contradictions between the reports

### C1 — `instancePickingId` is not a per-glyph GPU cost, and the pick id is computed in-shader

`data-path.md` difference #5 scores "Picking costs 0 B/glyph … vs the web's
`instancePickingId` u32 attribute + ID render pass" as bucket 1, and rolls the 4 B into
its **"Web total: 64 B of GPU per source byte"**.

`interaction.md` §3 says the opposite and is right: "`instancePickingId` exists only as
a harness mirror". The source is explicit —
`packages/glyph3d-core/src/GlyphField.js:1019-1021`:

> `// instancePickingId — written by PickingSystem.register() after flush.`
> `// Uint32Array: a pick id is an exact identity and f32 aliases it past 2^24.`
> `// No shader reads this attribute; it is the CPU-side mirror harnesses check`
> `// the pick pass against, so it must carry ids exactly or it lies about them.`

The ID is `base + instanceIndex`, computed in the pick material
(`packages/glyph3d-core/src/picking/PickingSystem.js:397-405`). So the web *also*
reconstructs rather than stores. `data-path.md`'s 64 B/source-byte should be 60 B of
shader-consumed data plus a host-side mirror, and the bucket-1 framing on this row does
not survive. `PLAN-DRAFT.md:447-452` reaches the same conclusion as `interaction.md`
("glyph3d already reconstructs. Do not regress"), so `data-path.md` is the outlier of
three documents.

### C2 — Three documents propose three mutually exclusive fates for the same four bytes, and only two of the three are physically reachable

- `data-path.md` §7 + "fix first" #1: drop `row`/`col`/`flags`/`_pad` → **32 B**.
- `interaction.md` §4: keep 48 B and **repurpose `_pad` at offset 44 as the highlight
  lane** ("rename it `highlight`, unpack in the vertex to `vAddedColor`/`vFillAmount`").
- `PLAN-DRAFT.md:148-160`: drop `row`/`col`/`_pad` → **36 B**, then add `highlight` →
  **40 B**, "so the arena needs one binding instead of two and the chunking stops firing".

These cannot all be done. Worse, the middle one is not achievable as written — see D-P3
below; `naga` says a struct containing `pos: vec3<f32>` has `AlignOf` 16, so 36 and 40
both round to 48. The reachable sizes are **32** and **48**. Which means the real choice
is exactly the two the reports proposed, and it is binary:

| option | size | consequence |
|---|---:|---|
| `data-path.md` — drop row/col/flags/_pad | **32 B** | 95,181,245 × 32 = 2,904 MiB < 4,095 MiB binding limit → **one chunk**, chunking arithmetic deletes itself. No room for highlight. |
| `interaction.md` — `_pad` → highlight | **48 B** | highlight parity at zero size cost; arena stays 4,357 MiB / 2 chunks. |

Both cannot be had without redeclaring `pos` as three scalar `f32` in WGSL (`naga`
then gives 36 and 40; see D-P3). No report names that as the hinge, and it is the
decision the byte question actually turns on.

### C3 — Pick latency: `data-path.md` generalises from a 3.7 KB file; `interaction.md` corrects it and is right

`data-path.md` §6: "At 1,305 segments that is microseconds", and §3 cites
"999.6 µs for a 3,697 B file (`out/g-windowed-smoke.log:15`) — fine at interaction rate".

`interaction.md` §3 bucket 4: "The header's 'microseconds' holds for the AABB test, not
for the cache fill; the code's own comment says ~10 ms for a 10 MB file". Verified —
`native/src/glyph_scene.rs:2064-2066`:

> `// Nearest record cell: … O(records of ONE file) — microseconds for typical files,`
> `// ~10 ms for a 10 MB monster.`

and the cache is one entry, `native/src/glyph_scene.rs:927` (`cache: Option<PickCacheEntry>`),
tested at `:1865`. `interaction.md` is correct.

Neither report notices the further problem: the 999.6 µs figure covers a *cold* fill,
which does `fs::read` + `Engine::new()` + `load_trie_file` + `load_item`
(`native/src/repo.rs:598-609`) — a fixed cost that dominates at 3.7 KB — so it
extrapolates to nothing, and the "~10 ms" comment covers only the linear nearest-cell
scan, not the re-derivation. **Nobody has measured a pick on a large file.**

### C4 — `data-path.md` contradicts its own numbers on record granularity

§3, closing caveat: "the web's slots are per *byte* and native's records are per
*codepoint*". Its own §2 cites `out/g-windowed-smoke.log:5-6`: **96.9 MB of source →
96,860,762 engine records**. That is one record per byte, not per codepoint — as it must
be, since `F_LEADER` exists precisely to mark which of the per-byte records is the
leader (`engine/glyph_schema.mojo`; the web twin at
`packages/glyph3d-core/src/compute/glyphPipelineReference.js:330-341`).

The caveat was offered as an honesty hedge and is simply a wrong premise. It does not
change the 1.73 % figure (measured, not derived), and removing it makes the byte↔slot
comparison *cleaner*, not weaker.

### C5 — `data-path.md` and `interaction.md` both overstate what `encase_bytes_match_bytemuck` guards

`data-path.md` §1: "encase generates the WGSL layout independently, bytemuck generates
the wire bytes, and `encase_bytes_match_bytemuck` compares them" — scored bucket 1, "the
stronger half", and offered as the thing the web lacks.
`interaction.md` §5: "`encase_bytes_match_bytemuck` at `:3346-3377` is a genuinely
decisive check".

Both are describing it as a Rust↔WGSL guard. It is not one. Measured (see (b) V6):
`encase` derives its layout from the **Rust** declaration, not from `glyph_field.wgsl`,
and it does *not* apply vector alignment to `[f32; 3]`. The repo's own test says so —
`native/src/glyph_scene.rs:3316-3319`:

> `// NOTE: repr(C) arrays are align-4 in Rust (no GPU vec alignment), so there`
> `// is NO tail padding — Rust and encase agree at 104 B.`

and `native/tests/wgsl.rs:24-54` — the only thing that reads the `.wgsl` files — does
`naga::front::wgsl::parse_str` + `Validator::validate` and nothing else. **No test in
the tree compares `InstanceSlot`'s layout to `GlyphInstance`'s.** The check is decisive
for what it covers (two Rust views of one Rust declaration agreeing byte-for-byte);
it is blind to the boundary both reports credit it with holding.

### C6 — bucket labels diverge for the same difference (minor, but it breaks a coalesced table)

- Direct-vs-indirect draw: `data-path.md` #8 = bucket 2; `interaction.md` #7 = "2 —
  platform … and better here".
- Chunking: `data-path.md` #9 = bucket 2, then §4 concludes "**Chunking is a choice, and
  it is the better one — bucket 1**"; `interaction.md` #8 = bucket 2 flat, and adds the
  cost `data-path.md` omits ("the fifth place that division is hand-written").
- Selection/highlight: `data-path.md` #6 = bucket 1 in the table, hedged to "bucket 1 for
  the mechanism, **bucket 3** for the capability" in §6; `interaction.md` #13 = bucket 3
  flat.

Anyone merging the four tables gets double entries with conflicting numbers. The
substance agrees; the scores do not.

---

## (b) Claims verified

### V1 — The web issues MULTI-DRAW INDIRECT with nonzero `firstInstance`, not one draw. CONFIRMED.

Both `data-path.md` §5 and `interaction.md` §2 are right, and `interaction.md` is right
to say so "contrary to the framing in the brief".

`packages/glyph3d-core/src/MegaGlyphField.js:498` `_cullRanges(renderer, camera)`, called
from `onBeforeRender` at `:190`. Per pass: a `THREE.Frustum` test of each view's
`_worldBox` (`:512-513`, `:524`), then a **drawn-instance budget** —
`DEFAULT_INSTANCE_BUDGET = 2_000_000` (`:90`), survivors ranked by angular size squared
(`:533-534`), `PROMOTION_HYSTERESIS = 1.15` (`:96`, applied `:573`). Then 5-uint indirect
records (`:605-611`):

```js
a[base]     = 6;                 // indexCount
a[base + 1] = r[i].end - r[i].base;
a[base + 2] = 0;                 // firstIndex
a[base + 3] = 0;                 // baseVertex
a[base + 4] = r[i].base;         // firstInstance — slot address space intact
```

run-merged within `MERGE_GAP = 4096` (`:514`, `:600-601`), installed by
`geometry.setIndirect(this._indirect.attr, offsets)` (`:621`). The feature gate
`_detectIndirect` (`:469`) fails loud on a device lacking `indirect-first-instance`.

Native's direct-range answer to the same design is correctly attributed to the
wgpu-30/Metal `first_instance != 0` no-op (`native/src/glyph_scene.rs:50-63`,
`native/src/shaders/cull.wgsl:27-31`), and `interaction.md`'s judgment ("that is exactly
the hazard the web's feature check exists to detect, on a platform where the feature
reports present and lies") is the sharpest sentence in the four reports.

### V2 — The web's arrangers are DORMANT. CONFIRMED, and more completely than `layout-compute.md` states.

`packages/glyph3d-core/src/collections/CodeGrid.js:218-219`:

```js
registerArranger(a) {
    throw new Error('CodeGrid.registerArranger: arrangers are not on the byte pipeline yet (deferred — see the Layer 2 wiring plan, M5)');
}
```

Unconditional. Both arrangers call it on their activation path —
`StructureLayout.js:86` (inside `grid()`), `StrataLayout.js:105` (inside `start()`) —
and both are reachable from live command verbs (`app/commands/handlers/structureCommands.js:21`,
`:29`). So `structure.grid` and the strata verbs **throw at runtime**; this is not dead
code behind a dead door, it is a registered verb that errors.

Two further confirmations `layout-compute.md` does not have:
- `CodeGrid` has **no `setDisplacements` method at all** (grep over
  `packages/glyph3d-core/src`; only two comments at `CodeGrid.js:210` and `:215` mention
  displacement). `StructureLayout.js:100` / `StrataLayout.js:119`'s
  `this._grid.setDisplacements?.(null)` is an optional-call no-op. (Both arrangers also
  call it NON-optionally inside `arrange()` — `StructureLayout.js:199`,
  `StrataLayout.js:172` — which would `TypeError` if that path were ever reached.)
- `syncGpuLayout` (`packages/glyph3d-core/src/compute/GlyphLayoutCompute.js:77`) has one
  definition, one mention in a comment (`GlyphField.js:2043`), and **no callers** in
  `packages/` or `app/`. `GlyphField.setGpuLayout` (`:2131`) likewise.

`layout-compute.md`'s scoring — "native is missing a capability the web has designed,
proven and currently has switched off" — is correct, and is the right way to score it.

### V3 — The native atlas DOES contain emoji, and the "2 advances / 1 height" is caused by them. CONFIRMED by independent re-parse.

Re-parsed `assets/atlas/{glyphs,codepoints,engine-trie,glyphmap}.bin` from
`assets/atlas/FORMAT.md`'s record layouts, enumerating all 1,114,112 codepoints through
the two-load trie algorithm at `native/src/atlas.rs:110-122`:

- `glyphs.bin`: 4431 slots × 56 B — **3511 outline / 897 `FLAG_BITMAP` / 23 `EMPTY`**
  (3511+897+23 = 4431 exactly), fonts 1699/957/877, 175 distinct `advanceFu`, 920 slots
  with `curveCount == 0` = 897 + 23.
- `codepoints.bin`: 5349 mapped, 897 `FLAG_BITMAP`, advances **exactly two**
  (`1229 ×4452`, `2458 ×897`), height **exactly one** (`2320 ×5349`).
- **The set equality holds**: `{cp : advance == 2458}` and `{cp : FLAG_BITMAP}` are the
  same 897 codepoints, 0 in either difference; 0 of the 4452 at advance 1229 carry
  `FLAG_BITMAP`.
- Spot checks all reproduce: U+1F400 → slot 3839 / 2458 / BITMAP / `emojiCell` 305;
  U+1F600 → 4351 / 2458 / BITMAP / cell 817; U+1F680 → slot 0 `FLAG_MISSING`;
  U+2764 → slot 3112 / 1229 / flags 0; U+4E00, U+3042 → `FLAG_MISSING`.
- `engine-trie.bin` world units confirm: `{0.5297414064407349: 4452,
  1.0594828128814697: 897}`, heights `{1.0: 5349}` — and the second is exactly 2×.

So `atlas-emoji.md`'s headline is right on both halves: emoji are present as *entries*,
and removing them would make the measurement **stronger** (one advance), not weaker.
`PLAN-DRAFT.md:110-121` had already withdrawn the conclusion it drew from that
measurement, on independent grounds.

One bracketing correction: the "1 blank slot (`fontIdx == 0xFFFFFFFF`)" is not a fourth
disjoint bucket — slot 0 carries `flags == 2`, so it is one of the 23 `EMPTY`. The four
numbers as listed sum to 4432.

### V4 — Native compaction drops 1.73 %, and what it costs. CONFIRMED, with the mechanism nailed down.

`out/g-windowed-smoke.log:6`: `96860762 engine records -> 95181245 glyph instances
(1679517 blank/missing dropped)` = **1.7339 %**. Independently corroborated at
`out/STAGE_E2_REPORT.md:69-70` and, on a 4-file corpus, `out/STAGE_H_REPORT.md:32`
(11,168 → 10,857 = 2.785 %). The predicate is `native/src/layout.rs:564`.

What it drops is essentially **newlines**: U+000A and U+0009 resolve to `glyph_id 0`
(`FLAG_MISSING`); newlines are ~2.4 % of bytes in the measured corpus, which brackets
1.73 % once continuation bytes and file endings are accounted for. **U+0020 resolves to
`glyph_id = 1`** — nonzero, `flags == 0`, `curveCount == 0`, Cousine gid 3, name
`space` — verified in both `codepoints.bin` and `engine-trie.bin`. Space survives
compaction and is discarded per-fragment at `native/src/shaders/glyph_field.wgsl:267`.
`PLAN-DRAFT.md:126` is correct on this, and it is the larger prize by an order of
magnitude.

The addressing cost `data-path.md` describes is real and slightly understated. The
record→slot map becomes non-affine, and nothing stores it, so `ensure_pick_cache`
(`native/src/glyph_scene.rs:1864-1920`) rebuilds the prefix sum from scratch —
`vec![u32::MAX; records.len()]` at `:1890-1897` — after a disk read, a fresh `Engine`,
a trie reload and a full CPU fold cross-check. Unmentioned by any report: **the
predicate is duplicated** at `layout.rs:564` and `glyph_scene.rs:1893` and must stay
byte-identical or picking silently misaddresses; the only guard is a `debug_assert_eq!`
at `:1898`, which is compiled out of release.

### V5 — 16 of 48 instance bytes are read by no shader. CONFIRMED across all `.wgsl`.

The shader set is pinned by test, not by a find: `native/tests/wgsl.rs:37` asserts
exactly `["composite.wgsl", "cull.wgsl", "glyph_field.wgsl", "quad_field.wgsl"]`.
Of those, only `glyph_field.wgsl` binds `array<InstanceSlot>` (`:76`). `cull.wgsl:45`
binds `array<BackdropInst>`, an unrelated 32 B struct; `quad_field.wgsl:14-24` declares a
*different* `InstanceSlot` (the Stage A stress shader); `composite.wgsl` binds no storage
buffers. The 17 further `.wgsl` files in the tree are byte-identical copies under
`.claude/worktrees/` plus three vendored egui shaders.

There is exactly one whole-struct read — `let inst = instances[ii];`
(`glyph_field.wgsl:112`) — and it is the only `instances[` in the file. Every use after
it is an explicit `inst.<field>`:

| field | offset | read at |
|---|---:|---|
| `pos` | 0 | `glyph_field.wgsl:129`, `:153` |
| `glyph_id` | 12 | `:115` |
| **`row`** | **16** | **nowhere** |
| **`col`** | **20** | **nowhere** |
| `color` | 24 | `:161-163` |
| `group_id` | 28 | `:134`, `:150` |
| `advance` | 32 | `:121` |
| `height` | 36 | `:123`, `:129` |
| **`flags`** | **40** | **nowhere** |
| **`_pad`** | **44** | **nowhere** |

Rust side: every `.row`/`.col` hit is `GlyphRecord::row()/col()` (the 32 B wire record),
`RefGlyph`, or `PickGlyph` — never `GlyphInstance`. All four `GlyphInstance` construction
sites write `flags: 0, _pad: 0`. `write_instance` (`glyph_scene.rs:2241`) is called with
offsets 24, 0 and 32 only. **1,452 MiB of 4,357 MiB carries nothing.**

One caveat worth recording: `cargo check --all-targets` being clean is *not* evidence
here. `#[derive(encase::ShaderType)]` (`glyph_scene.rs:101`) generates a `write_into`
that reads all ten fields by name, so `dead_code`'s "field is never read" is
structurally unable to fire on this struct. `BACKEND-PLAN.md:262-266`'s rename
experiment was the right method for exactly this reason.

### V6 — The WGSL/Rust size question, settled by naga and encase

`naga` 30 (the compiler `wgpu` 30 bundles, and the one `native/tests/wgsl.rs` already
uses), `Layouter` over four candidate declarations:

| declaration | naga size | naga align |
|---|---:|---:|
| current 10 fields, `pos: vec3<f32>` | **48** | 16 |
| minus `row`,`col`,`_pad` (PLAN-DRAFT's "36") | **48** | 16 |
| that plus `highlight` (PLAN-DRAFT's "40") | **48** | 16 |
| minus `row`,`col`,`flags`,`_pad` (data-path's "32") | **32** | 16 |
| the "36" variant with `pos` as three scalar `f32` | **36** | 4 |
| the "40" variant with `pos` as three scalar `f32` | **40** | 4 |

and `encase` 0.12 on the same Rust declaration as the "36" variant reports
`rust_size = 36, encase_size = 36`. So the encase↔bytemuck test stays green at 36 while
the shader strides at 48 — silent corruption, undetected. This is the finding behind
C2 and D-P3.

### V7 — The FFI does NOT expose the per-item bounds. CONFIRMED at three levels.

`engine/ffi.mojo` exports exactly eight `@export` + `abi("C")` symbols —
`glyph_engine_new` (`:69`), `_free` (`:79`), `_load_trie_file` (`:91`), `_load_item`
(`:112`), `_load_items` (`:210`), `_slot_count` (`:285`), `_copy_slots` (`:291`),
`_fp_probe` (`:318`). `native/src/engine.rs:34-70` declares exactly those eight.
`nm -gU native/libglyph_engine.dylib` lists exactly those eight. Three independent
surfaces agree.

The bounds are computed — `engine/glyph_schema.mojo:113-120` defines
`B_MIN_X..B_MAX_Z`, `B_TOTAL_ROWS`, `B_MAX_ROW_EXTENT`; `engine/glyph_pipeline.mojo`
fills `item_bounds` and reduces `batch_bounds` — and then discarded: `EngineState`
(`ffi.mojo:34-52`) has five fields (`trie`, `has_trie`, `records`, `byte_len`,
`leaders`) and **no bounds field**, so `r.item_bounds`/`r.batch_bounds` die as locals at
`ffi.mojo:175-177` and `:262-264`. Exposing one is a struct change, not just a new
export — `layout-compute.md`'s "cheapest thing on this list … the values are already in
`PipelineResult`" is slightly optimistic on that point.

`layout-compute.md`'s downstream consequence is exact: every native bound is 2-D
(`native/src/layout.rs:320-338`, `native/src/glyph_scene.rs:255-256`) while `z_step` is
now wired.

### V8 — `rows_for_line`: two rules, two quantities, no live inconsistency, and the corpus DOES discriminate

The three cited sites are not three rules.

| site | expression | quantity |
|---|---|---|
| `native/src/fold.rs:273` | `length / wrap + 1` | **row count** |
| `engine/glyph_pipeline.mojo:603-608` | `length // wrap + 1` | **row count** |
| `packages/glyph3d-core/src/compute/glyphPipelineReference.js:387` | `Math.floor(len / wrap) + 1` | **row count** |
| `packages/glyph3d-core/src/compute/glyphPipelineKernels.js:621-622`, `:852-853` | `len.div(wrap).add(1)` | **row count** |
| `packages/glyph3d-core/src/core/foldGeometry.js:279` | `Math.floor((slotCount - 1) / wrapWidth)` | **deepest segment INDEX** (its own JSDoc at `:275` says so) |

`glyphPipelineKernels.js:600` is a comment, but it documents live TSL two lines below it
at `:621-622`, and the same expression recurs at `:698-699` and `:852-853`. So the
running kernel, the JS oracle, `fold.rs` and `glyph_pipeline.mojo` all use `n/w + 1`,
bit-for-bit.

`foldGeometry.lineSegments` returns an index over a **newline-free slot space** — the
glyph-list model has no newline slot (`packages/glyph3d-core/src/workers/builders/index.js:192-193`) —
so `+1` recovers a count and the remaining offset is the newline's phantom row. For every
*visible* glyph the two agree exactly. It is also **not on the running path**: its single
call site is `packages/glyph3d-core/src/core/LayoutDescription.js:170` (a deliberate
caret-affinity clamp), and `LayoutDescription` is imported only by
`tools/layout-fuzz.test.mjs` and `tools/layout-mirror.test.mjs`; production `CodeGrid`
builds a `ByteLayoutDescription` (`CodeGrid.js:1656`), which contains no row arithmetic.
**No live inconsistency.**

Fixtures discriminate, deliberately. Parsing the `.pipe.bin` v3 binaries directly:
`wrap-exact` (wrap 4, generator `engine/fixtures/gen.mjs:113-116`) carries three
exact-multiple lines and one empty line; `wrap-emoji` (wrap 3) one; `real-kernels`
(wrap 80) twelve. Decoding `wrap-exact`'s stored `S_ROW` values, five of six lines match
`⌊n/w⌋+1` and mismatch `⌊(n−1)/w⌋+1`; line 0's newline sits **alone on row 1**, the
phantom row materialised in the corpus. `engine/conformance.mojo:140-142` diffs ROW/COL/
FLAGS as exact integers, so the comparison is not structural. Empty lines do **not**
discriminate (both rules give 1).

### V9 — Verified without qualification (spot checks that all held)

- Native's z defect (`interaction.md` #18) is real and exactly described:
  `SegCull.min/max` are `[f32; 2]` (`glyph_scene.rs:255-256`), the cull substitutes
  `pz = ±1.0` (`:413`) and clamps the eye to `z ∈ [-1,1]` (`:427`),
  `Verb::MoveGroup` writes `g.cols[0][2] += d[2]` (`:2444`), and `sync_segment`
  (`:2270-2278`) re-derives only x and y — while `ray_file` *does* follow z
  (`:1996-2004`, `off.z ± 1.0`). Pick and cull disagree after a z-move.
- `interaction.md`'s aside is right: `GlyphField.js:66` and `:820` still say "112B"
  for a row that `glyphVertex.js:48-50` records as 80 B.
- `layout-compute.md`'s unwired-parameter table is line-accurate:
  `native/src/repo.rs:238` `page_cols: 0`, `:239` `scroll_rows: 0`, `:250-251`
  `depth_per_band`/`depth_per_col: 0` with the stated (and correct) reason, `:252`
  `page_line_height: 0.0`; `native/src/main.rs:92-96` sets only `line_height`;
  `native/src/fold.rs:759 paginate_ignores_page_line_height` exists and is
  anti-vacuity-guarded.
- Both `pageCols` and `depthPerColumn` are **read but never written** on the web:
  `glyphPipelineScan.js:261` and `glyphPipelineKernels.js:1325,1331` read them;
  `CodeGrid._pageParams` (`CodeGrid.js:1457-1475`) sets neither. `layout-compute.md`'s
  "unwired on *both* sides" is correct and is the most useful line in that report.
- `loadStats.kernelMs` (`GlyphPipelineArena.js:527-548`) brackets `writeBytes` +
  `run()` with no await and no fence — submit + upload cost, not GPU work. Confirmed.
- `ARENA_MAX_BYTES = 44,739,242` and `assertSlotBufferFits`'s refusal
  (`glyphPipelineKernels.js:150-183`). Confirmed.
- `attachBytePipeline` refuses a nonzero `slotBase` at `GlyphField.js:1977-1984`. Confirmed.
- `engine/gpu_pipeline.mojo:617-619`: `k_spine_scan` really does dispatch
  `grid_dim=1, block_dim=1`. And the stride derivation really is a device→host→device
  round trip: `:648` copies `d_xmax` home, `:652` computes `derive_stride` on the host
  per item, `:654` copies back. `layout-compute.md` #22/#23 hold.
- `PLAN-DRAFT.md`'s web-side `row`/`col` audit holds: `iRowCol`/`vRowCol` occur at
  `glyphVertex.js:211,233,239,309` and `GlyphField.js:162,185,222,245,697,713` — all
  declaration, assignment or pass-through. Nothing reads them in a fragment expression.
- `BACKEND-PLAN.md:19-24`'s "zero production callers" for `fold`/`scan`/`bake`/
  `glyph_trie` still holds: the only non-self references are `native/src/fixture.rs:29-31`
  and the modules' own `#[cfg(test)]` blocks.

---

## (c) Claims refuted or weakened

### R1 — `atlas-emoji.md` #9: "Regenerating the atlas hard-depends on the web repo being checked out at `REF_ROOT`". **REFUTED.**

`tools/export-atlas.mjs:46`: `const REF_ROOT = join(HERE, 'vendor', 'ref');` — i.e.
`tools/vendor/ref`, **inside this repo and git-tracked** (`git ls-files tools/vendor/ref`
returns the fonts and the slug core). The file's own header says so at `:25`: "…is
vendored under tools/vendor/ref (see REF_ROOT below); the web repo is not [required]".
The bucket-2 row and the closing "the only growth mechanism is a re-bake that requires
the web repo present" are both false.

### R2 — `atlas-emoji.md` #12 and the fallback-chain section: "no shaper, no fonts in the dependency graph". **PARTLY REFUTED.**

The literal scoping is true — no `.ttf`/`.otf` under `native/` or `assets/`, and none of
the six named crates in `native/Cargo.toml`'s direct dependencies. What that scoping
misses:

- The three chain TTFs **are** in the repo, git-tracked, at
  `tools/vendor/ref/packages/glyph3d-core/src/fonts/` (`Cousine-Regular.ttf`,
  `DejaVuSans.ttf`, `MesloLGS-NF-Mono.ttf`).
- `native/Cargo.lock` contains **`harfrust`, `skrifa`, `read-fonts`, `font-types`,
  `epaint_default_fonts`**, pulled by `epaint` ← `egui`, and `egui-ui` is a **default
  feature** in `native/Cargo.toml`. These are compiled: `native/target/{debug,release}`
  holds build artifacts for `harfrust`, `skrifa`, `font_types` and
  `epaint_default_fonts`. (`ab_glyph` and `owned_ttf_parser` are locked via
  `winit → sctk-adwaita` but have zero artifacts on macOS; `rustybuzz`, `swash`,
  `cosmic-text`, `fontdue` are genuinely absent.)

None of it is *wired* to the glyph pipeline — it is egui's own text stack — so the
capability gap the report describes is real. But "a HarfBuzz-class shaper is not in the
dependency graph" is false as written, and it matters, because it changes what closing
the gap would cost.

### R3 — `data-path.md` #5: picking bytes. **WEAKENED.** See C1.

### R4 — `data-path.md` §3: "native's records are per codepoint". **REFUTED** by its own cited count. See C4.

### R5 — `data-path.md` §6: `instanceHighlight` "~363 MiB at this scale". **SCOPING ERROR.**

363 MiB is 95.18 M × 4 B — native's instance count applied to a web attribute. The web's
arena ceiling is `ARENA_MAX_BYTES = 44,739,242` source bytes
(`glyphPipelineKernels.js:150-153`), so its `instanceHighlight` tops out at ~171 MiB and
`assertSlotBufferFits` would have refused this corpus outright. The report says as much
in §4; §6 then compares across the refusal.

### R6 — `data-path.md` #3 and `interaction.md` §5: what the encase test guards. **REFUTED.** See C5 and V6.

`interaction.md`'s prescription (`offset_of!` for the call-site literals) is right and
should be done, but it closes only half the hole: it ties the *Rust* writers to the
*Rust* struct. Nothing ties either to `glyph_field.wgsl`.

### R7 — `layout-compute.md` #12: "No closed-form extent. `foldExtent` is O(1); native walks every record." **WEAKENED.**

`foldExtent` (`packages/glyph3d-core/src/core/foldGeometry.js:145-202`) is on the same
dormant Layer-1 substrate as `lineSegments` (V8) — reachable only through
`GlyphLayoutKernel.configure` ← `syncGpuLayout`, which has no production caller (V2).
The web's **live** answer is the one the same report cites two paragraphs later: the
`itemBoxes` reduce fused onto the paginate pass with six atomics
(`glyphPipelineKernels.js:1083-1088`), read back once per coalesced flush. That is O(n)
on the device, not O(1). The honest comparison is *device-fused reduction vs host
re-walk*, which is still a real difference and still favours the web — but it is not a
closed form, and the report scores it as one.

### R8 — Confidence exceeding evidence, across the reports

Not errors; claims stated more firmly than what was measured supports.

- `atlas-emoji.md` #5 scores "a pre-rendered deterministic emoji sheet is available to
  native" as **bucket 1 — better (unrealized)**. Nothing was built or measured; the
  bucket is an argument, and the report says "unrealized" in the same cell. A bucket for
  an unbuilt thing invites it to be counted in a total, and it is (5 bucket-1s).
- `data-path.md` §4: "Native's is strictly more capable: it staged 96.9 MB of source in
  one arena; the web's hard ceiling is 44.7 MB and it would have refused this corpus."
  True, and the report's own mitigation note (windowing —
  `MegaGlyphField.js:640-645`) plus the 2 M drawn-instance budget mean the web's ceiling
  binds on a quantity it never intends to hold. "Strictly more capable" compares a
  capability neither design wants to exercise the same way.
- `data-path.md` §7's proposed 48 → 32 B is described as "a struct edit plus the four
  asserted offsets in `layout_tests`". It is also a `glyph_field.wgsl` edit (a
  `READ-ONLY without a dedicated stage` file per `native/AGENTS.md`), a change to every
  `write_instance` offset literal, and — since the `GLYPH_G_DUMP` readback is the only
  reader of the dead lanes — a change to `offscreen.rs:89-96`. The size claim is right;
  the cost claim is optimistic.
- `interaction.md` §2 asserts "Native could add the same coalescing to `cull_segments`
  for free, and should". Plausible, unmeasured — and the web's own comment at
  `MegaGlyphField.js:590-596` records the *opposite* measured finding for its own case
  ("Draw-call count is INVERSELY correlated with frame time here, while instances are
  linear in it, so the trade flips"). The web went the other way after measuring.
- `layout-compute.md` #22 and #23 are inherited from `BACKEND-PLAN.md:98-102` rather than
  independently checked. Both happen to be true (V9), but they describe a code path
  nothing executes, so they cannot change a decision today.
- All four reports treat the four bucket totals as arithmetic. They are judgments about
  differences that overlap across reports (C6), and the totals do not compose.

---

## (d) Dead claims in `BACKEND-PLAN.md` and `PLAN-DRAFT.md`

### `BACKEND-PLAN.md`

**D-B1 — `:150-152` (and its twin in the source at `native/src/layout.rs:57-60`):
"The `Vec<GlyphRecord>` readback survives only behind `VerifyLayout`, a separate trait,
so it is unreachable from the render path rather than merely discouraged." FALSE.**

`MojoLayout::run` calls `self.engine.records()` **unconditionally** in both strategies —
`native/src/layout_mojo.rs:79` (batched) and `:110` (per-item) — before the optional
`records_out` sink is even consulted. `Engine::records()` (`native/src/engine.rs:295-302`)
allocates `vec![GlyphRecord::default(); n]` and calls `glyph_engine_copy_slots`: a full
32 B/record copy across the FFI. What `VerifyLayout` gates is *handing the records to a
caller*, not the readback. Measured cost on the render path:
96,860,762 × 32 B = **3.10 GB** (`out/g-windowed-smoke.log:5-6`).
`data-path.md` §2 is right and this document is wrong. The line in `layout.rs`'s header
is the same claim and is equally wrong; the surrounding paragraph is otherwise the best
statement of the problem in either tree.

**D-B2 — `:262-266`: "renaming `GlyphInstance::row`/`col` produces four compile errors
and ALL FOUR ARE STRUCT-LITERAL WRITES." INCOMPLETE, and the omission is the important
one.** `PLAN-DRAFT.md:314-321` re-ran the same experiment with `--all-targets` and found
**six** sites: five writes (adding `glyph_scene.rs:3349-3350`, the `layout_tests` bit-
pattern fixture) and **one READ** — `layout.rs:964`, `bent[1].col ^= 1`, verified present:

```rust
// 2. instance — one byte of one slot.
let mut bent = instances.clone();
bent[1].col ^= 1;
```

That read is load-bearing: it is how
`diff_backends_sees_a_difference_in_each_of_the_three_things_it_compares` proves the
differ sees instance-level differences, and it must be retargeted deliberately, not
mechanically. `BACKEND-PLAN`'s "ALL FOUR" reads as a completed search; it was run
without `--all-targets`. Prefer `PLAN-DRAFT`'s table.

**D-B3 — `:229-247`: the measured host-path block (`1306 files, 97.0 MB`,
`95207113 instances (4358 MiB)`, `backend 2.515s | stage 0.968s | total 3.676s`) is a
DIFFERENT RUN from the one all four reports cite.** `out/g-windowed-smoke.log:5-7` has
1305 files, 96.9 MB, 95,181,245 instances, 4357 MiB, and `engine 1.805s | stage 1.438s |
total 3.442s`. Neither is wrong; they are two runs of a moving tree with the phase split
drawn differently (`BACKEND-PLAN`'s "stage 0.968s" is `colorize_leaders`, per `:252`;
the smoke log's "stage 1.438s" is compaction + upload). `PLAN-DRAFT.md:15-19` reproduces
`BACKEND-PLAN`'s numbers verbatim, so the same figures now circulate under two phase
definitions. Anyone quoting "0.968 s of 3.676 s" must not mix it with the smoke log's
breakdown.

**D-B4 — `:277-280` ("Suggested reorder … settle the instance payload and run-length
paint") is superseded** by `PLAN-DRAFT`'s own measurement that the payload cannot shrink
much, which relocates the plan from "how many bytes" to "who writes them". `PLAN-DRAFT`
says so at `:172-177`. `BACKEND-PLAN`'s reorder paragraph should be deleted or marked
superseded rather than left to be read as current advice.

**D-B5 — `:88-92`, the "renderer's real needs are METADATA" table, lists picking as
"send a click coord, get one ID from an ID pass".** Native has no ID pass and did not
build one: `native/src/glyph_scene.rs:11-26` and `:1950-2108` describe an analytic f64
CPU ray, chosen for stated reasons (`:1943-1953`: an f32 inverse view-proj unprojects to
w≈0 at near 0.05 / far ≈1.7e6). The row describes a design that was abandoned. Harmless
as an abstraction, misleading as a description.

**Still standing in `BACKEND-PLAN`, verified:** `:19-24` (zero production callers for the
Rust modules), `:40-42` (GPU kernels unwired — corroborated by the dylib's eight CPU
symbols, V7), `:44-66` (the readback-benchmark finding), `:98-102` (`k_spine_scan`
`grid_dim=1, block_dim=1` and the stride round-trip — both re-verified, V9),
`:203-220` (the M6 ceiling and its fix), `:309-319` (the verification habits).

### `PLAN-DRAFT.md`

**D-P1 — `:57-66` finding #1: "`collections/StructureLayout.js:24,144-145` parks glyphs
by zeroing size height … That is not a sparse override; it is a bulk per-glyph write,
today, in the app." The EVIDENCE IS DEAD.** `CodeGrid.registerArranger` throws
unconditionally (`CodeGrid.js:218-219`); `StructureLayout.grid()` calls it at `:86`,
`StrataLayout.start()` at `:105`; `CodeGrid` has no `setDisplacements` at all;
`syncGpuLayout` has no callers (V2). Nothing in the app parks a glyph today. The
*conclusion* — advance/height stay per-glyph — survives on the other ground the same
paragraph gives (the kernel writes `M_ADVANCE`/`M_HEIGHT` per leader,
`glyphPipelineKernels.js:450-451`, `:483-494`), and on the general rule the document
itself states at `:39-48`. But "today, in the app" must go, and the same phrase recurs
at `:180-187` ("This is the piece the native port has not built") where it reads as if
the web has it running.

**D-P2 — `:69-74` finding #2: "The web's `MegaGlyphField` is one instanced draw over the
whole arena with groups interleaved." FALSE as a statement about draw submission.**
See V1: multi-draw-indirect, per-view CPU frustum cull, a 2 M drawn-instance budget with
hysteresis, run-merged records with nonzero `firstInstance`. The quoted comment
(`glyphVertex.js:206-208`) is about the *address space* — "instance index == arena byte
offset == slot index" — not the draw count, and it sits in a paragraph about slot reads.
The conclusion (`group_id` stays per instance) survives: a merged run spans up to
`MERGE_GAP = 4096` dead slots and several views, so a per-draw uniform cannot carry it.
But the follow-on sentence — "Native only gets away with per-segment draws because its
CPU cull happens to emit them" — inverts the situation. The web culls per view too;
native draws directly because wgpu 30/Metal silently rasterizes nothing for
`first_instance != 0` (`glyph_scene.rs:50-63`).

**D-P3 — `:148-160`, the payload table. The 36 B and 40 B rows are NOT ACHIEVABLE as
declared, and the conclusion drawn from them is false.**

> | `row`/`col`/`_pad` out | 36 | 3269 MiB (**1 binding**) |
> | …plus `highlight` in | 40 | 3632 MiB (**1 binding**) |
> **"So the payload win is 48 → 40 B … It still crosses the important threshold: 3632 MiB
> is under `max_storage_buffer_binding_size` (4095 MiB here), so the arena needs one
> binding instead of two and the chunking stops firing."**

`naga` 30 over `struct { pos: vec3<f32>, … }`: `AlignOf` is 16, so `SizeOf` is rounded to
a multiple of 16. Measured (V6): the 36-field variant is **48**, the 40-field variant is
**48**. Both arenas stay 4,357 MiB and both keep two chunks. Nothing crosses the
threshold. And `encase` on the same *Rust* declaration reports **36**, so
`encase_bytes_match_bytemuck` would stay green while the shader read at stride 48 —
the exact silent corruption the document's own hazard section (`:264-296`) is about,
arriving through the gate it trusts.

Two ways out, both real:
- **32 B** (drop `flags` too, as `data-path.md` proposes): `naga` size 32.
  95,181,245 × 32 = **2,904 MiB → one chunk**. This is the variant that delivers the
  single-binding win the document wanted; it costs the reserved `flags` lane and leaves
  no room for `highlight`.
- **Redeclare `pos` as three scalar `f32` in the WGSL** (align 4): `naga` then gives 36
  and 40 exactly as the table claims. This is a `glyph_field.wgsl` edit the document does
  not mention and every `inst.pos` use has to follow.

**D-P4 — `:310-312`: "the naga + encase tests are what make it safe." FALSE.**
`native/tests/wgsl.rs:24-54` only parses and validates; it never compares
`InstanceSlot` to `GlyphInstance`. `encase` models the Rust declaration, not the WGSL
source, and does not apply vec alignment to `[f32; 3]` (`glyph_scene.rs:3316-3319` says
so in the tree's own words). **There is no Rust↔WGSL layout gate.** The document's own
hazard section is right and is stronger than it knows: deriving the `24`/`32`/`48`
literals from the struct closes the Rust half and leaves the WGSL half open. What would
close it is a test that parses `glyph_field.wgsl` with `naga`'s `Layouter` and asserts
`InstanceSlot`'s size and per-member offsets equal `GlyphInstance::METADATA`'s —
about fifteen lines, using a crate already in `native/Cargo.toml:55`.

**D-P5 — `:5-8`: "Replaces `engine/BACKEND-PLAN.md`."** It replaces part of it. The
stage-0 record (`BACKEND-PLAN.md:141-227`, including the nine-mutation table and the M6
ceiling finding), the backend/target model (`:103-116`) and the verification habits
(`:309-319`) have no counterpart in `PLAN-DRAFT` and are not superseded. Deleting
`BACKEND-PLAN.md` on the strength of that line would lose them.

**D-P6 — citation drift, both documents.** The `GlyphInstance` construction sites have
moved ~3 lines: current positions are `layout.rs:573`, `text.rs:260`,
`glyph_scene.rs:1182`, `glyph_scene.rs:2351` — against `BACKEND-PLAN.md:264-265`'s
`text.rs:263 / layout.rs:576 / glyph_scene.rs:1185 / glyph_scene.rs:2354` and
`PLAN-DRAFT.md:314-320`'s same set. `PLAN-DRAFT.md:44` cites `repo.rs:211-224` for
`file_item_params`, which is now `repo.rs:223-255`. Harmless individually; worth one
sweep before either file is used as a map.

**Still standing in `PLAN-DRAFT`, verified:** `:126-133` (U+0020 → `glyph_id` 1,
survives compaction, ~28 % of bytes — independently re-measured at 28.34 % over
`native/src/**/*.rs`, V4); `:85-91` (the web does not compact; non-leaders are zero-size
instances); `:93-98` (`row`/`col` dead in the web's instance too — re-verified, V9);
`:264-296` (the hardcoded-offset hazard, and "no gate catches it"); `:212-225` (paint is
a pure function of one line, so it parallelises); `:373` (the f64 wall);
`:426-476` (the rerun comparison, which is the most carefully scoped section in either
document).

---

## (e) What is now solidly established

1. **The web draws multi-draw-indirect with nonzero `firstInstance`, per-view CPU
   frustum cull plus a measured 2 M drawn-instance budget.** Native's direct range draws
   are the same design blocked by a real wgpu-30/Metal defect, and are the better answer
   on this platform. (`MegaGlyphField.js:498-624`, `glyph_scene.rs:50-63`, `:3095`.)
2. **The web's arrangers, displacement table and Layer-1 fold substrate are dormant** —
   `registerArranger` throws unconditionally, `syncGpuLayout` has no callers, `CodeGrid`
   has no `setDisplacements`, and the `structure.grid`/strata verbs therefore error.
   Any claim resting on "the web does this today" needs re-grounding.
   (`CodeGrid.js:218-219`, `GlyphLayoutCompute.js:77`.)
3. **Sixteen of forty-eight instance bytes are read by nothing**, on the GPU or the CPU;
   `flags` and `_pad` are constant zero at all four write sites. 1,452 MiB of 4,357 MiB.
   (`glyph_field.wgsl:112-163`, four construction sites.)
4. **With `pos: vec3<f32>`, the only reachable instance sizes are 32 B and 48 B.**
   32 B fits the whole arena in one binding (2,904 MiB); 40 B does not exist. The
   encase↔bytemuck test cannot see the difference, and no test in the tree ties the Rust
   struct to the WGSL one. (naga 30 + encase 0.12, measured.)
5. **Layout still comes home on the render path**, unconditionally, 3.10 GB of it —
   `VerifyLayout` gates the API, not the copy. (`layout_mojo.rs:79`, `:110`,
   `engine.rs:295-302`.)
6. **The engine computes per-item and batch bounds and cannot say so**: eight FFI
   symbols, none returning a bound, and `EngineState` does not even retain them. Every
   native extent is 2-D while `z_step` is wired. (`ffi.mojo`, `nm -gU`,
   `glyph_schema.mojo:113-120`.)
7. **The native atlas contains 897 emoji entries and no emoji pixels.** The doubled
   advance is exactly the emoji set; the identity path (slot, `mode == 1`, `emojiCell`,
   2× advance) is complete; `emojiCell` never reaches `VsOut`
   (`glyph_field.wgsl:85-93`). Everything needed to bake a deterministic sheet — the
   three TTFs and the whole reference chain — is vendored in this repo at
   `tools/vendor/ref`, so the re-bake has no external dependency.
8. **`rows_for_line` is `⌊n/w⌋+1` in every running implementation** — TSL kernel, JS
   oracle, `fold.rs`, `glyph_pipeline.mojo` — and `foldGeometry.lineSegments`'
   `⌊(n−1)/w⌋` is a segment index over a newline-free slot space on a dead path. Three
   fixtures (`wrap-exact`, `wrap-emoji`, `real-kernels`) carry the discriminating
   exact-multiple case and their stored ROW lanes match the first rule.
9. **Compaction drops 1.73 %** (newlines and missing), not spaces: U+0020 is
   `glyph_id 1` and survives, at ~28 % of bytes. The predicate is duplicated between
   `layout.rs:564` and `glyph_scene.rs:1893` with only a release-stripped
   `debug_assert_eq!` holding them together.
10. **Native's `SegCull` cannot represent z while `MoveGroup` writes it**, and the pick
    path does follow z — so cull and pick disagree about where a moved file is. An
    internal defect, not a parity question. (`glyph_scene.rs:255-256`, `:413`, `:2444`,
    `:1996-2004`.)
