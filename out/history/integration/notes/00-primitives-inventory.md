> **History.** Moved to `out/history/integration/` on 2026-10-10: 2026-09 research
> and stage handoffs, all executed or superseded. What is true now: `README.md`,
> root `AGENTS.md`.

# glyph3d-native — hand-rolled primitives inventory (Sep 2026)

*Written from a full read of `glyph3d-native/native/src` + stage reports + `engine-local/README.md`.
Purpose: the ground truth against which "adopt a crate vs keep hand-rolled" decisions are made.
Companion docs live beside this file (see `README.md` for the index).*

## Stack facts (from `native/Cargo.toml`)

| dep | version | note |
|---|---|---|
| wgpu | `"30"` (30.0.1 in lockstep) | indirect-draw Metal bug documented; CPU cull instead |
| winit | 0.30 | `ApplicationHandler` style, `Arc<Window>` surface |
| glam | 0.30 | only math dep |
| bytemuck | 1 (derive) | all GPU structs are `#[repr(C)] Pod` |
| pollster / env_logger / log / image | — | init, logging, PNG readback |

Edition 2021. No CLI parser (hand-rolled), no UI toolkit, no ECS, no profiling crate,
no buffer-layout helper (raw bytemuck), no input abstraction.

## The primitives, by module

### `gpu.rs` — GpuContext (89 lines)
Instance/adapter/device/queue init. Adapter-max storage-binding limits (repo arenas
exceed 128 MiB defaults). Logs MULTI_DRAW_INDIRECT_COUNT feature. Clean seam: everything
downstream takes `&GpuContext`.

### `scene.rs` — `SceneLike` trait + Stage A demo Scene (371 lines)
- `SceneLike` is the **app's scene abstraction**: `render()` + interaction hooks
  (`on_key / on_mouse_look / on_scroll / on_cursor / on_click / tick / set_viewport /
  apply_pick / apply_verb / debug_dump_instances`), all default no-ops taking `&GpuContext`.
  Windowed + offscreen both drive scenes through exactly this trait.
- `Scene` = 1M-instance quad stress field with its own internal orbit camera
  (`CameraUniform { view_proj }` → uniform buffer).
- Also: `create_depth()` helper; deterministic xorshift PRNG for reproducible fields.

### `glyph_scene.rs` — GlyphScene (2,125 lines; the core)
Everything below is **hand-rolled and load-bearing**:
- **`GlyphInstance`** — 48 B storage-buffer slot (pos/glyph_id/row/col/packed color/
  group_id/advance/height/flags), chunked arena (chunks sized to max_storage_buffer_binding).
- **`GroupRow`** — 80 B group table row = 5×vec4: offset / quat / color+alpha /
  scale+colorBlend / clip. This is the **node/entity transform table** (one per file in
  repo mode; group_id == file index). GPU-updated by partial `write_buffer` (80 B/row).
- **CPU cull/LOD** — `cull_segments()`: per-frame 6-plane frustum test + 1px/em LOD
  classification over ~1.3k `SegCull` segments → per-segment range draws +
  CPU-compacted backdrop-quad stream (drawn by `cull.wgsl`).
  **Why CPU:** wgpu 30.0.1 Metal silently rasterizes nothing for indirect draws with
  `first_instance != 0` (verified by readback; documented in the module header + Stage F
  report). Revisit after a wgpu upgrade.
- **`FlyCamera` + `CameraMode { Front, Orbit, Fly }`** — yaw/pitch look, WASD/ERQF keys,
  scroll speed multiplier, exponential velocity damping (~63%/100 ms), dt clamped at 0.1.
- **Picking (Stage G)** — CPU ray pipeline: screen px → inverse view-proj → world ray →
  slab test vs live per-file AABBs (group TRS applied) → ray∩file plane → nearest record
  cell → (row,col) → `text::fold_leaders` → source line/byte/char. Independent Python
  oracle (`tools/g_pick_oracle.py`) gates it. One-entry pick cache (per-file re-derived
  records; deterministic engine re-run via `repo::rederive_records`).
- **Manipulation verbs** — recolor/nudge/scale glyph, recolor-line, move/scale/tint/hide
  group; all partial uploads; `geom_overrides` map preserves glyph geometry edits across
  row-rebuilds; `sync_segment()` re-derives world AABB + backdrop tint on group edits.
- `CullState` side-tables (local AABBs, base tints, orig group rgb, hidden flags).

### `atlas.rs` — Slug atlas loader (255 lines)
Parses 4 binary formats (FORMAT.md): `curves.bin` (quadratic-bezier payload texture,
Rgba32Uint), `glyphmap.bin` (per-slot header texel), `glyphs.bin` metrics,
`codepoints.bin` (two-level codepoint→slot trie, stays CPU-side). Textures uploaded
verbatim; `textureLoad`-only sampling in WGSL.

### `engine.rs` — Mojo FFI wrapper (349 lines)
Safe wrapper over the C ABI: scalars + one opaque handle; `GlyphRecord` = 32 B wire
record (f32 X Y Z ADVANCE HEIGHT | u32 GLYPH_ID ROW COL); `ItemParams` mirrors the
engine's layout params (f64 discipline). Link via build.rs → `libglyph_engine.dylib`.

### `engine-local/` — the Mojo text engine (NOT Rust; separate world)
Byte-in pipeline (decode → trie → fold → paginate → bounds), **bit-for-bit** proven
against a JS oracle (float discipline is contractual: kind-split arrays, no bitcasts,
f32-per-add segment advance, Metal has no f64 → conventions instead). Mid-item resume
(edit re-lays a line range). Streaming scan for whole trees. This is the differentiator;
any ecosystem adoption must not touch its contract.

### `text.rs` — staging + oracle (690 lines)
File → instances on the monospace grid; tiny tokenizer palette (VS Code dark+ flavored);
`fold_leaders` (CPU replica of the engine fold, for picking + cross-checks);
`reference_layout` + `diff_records` (the independent CPU oracle used by `--engine-check`).

### `repo.rs` — repository-scale loading (800 lines)
Walk (extension whitelist, skip dirs, 10 MB/file cap doubling as the engine's 2²⁴
ordinal-wall guard), UTF-8 gate, naive vs batched engine paths (bit-exact diff via
`--repo-verify`), one shared glyph arena with files as slot-range views (the web's
MegaGlyphField architecture), grid layout, per-directory tint palette.

### `windowed.rs` — window + input (343 lines)
winit 0.30 `ApplicationHandler`; sRGB surface preference; robust cursor-grab fallback
(Confined → Locked → stay grabbed + CursorMoved deltas; device-delta-vs-cursor
double-apply guard via `saw_device_delta`); left-click pick / right-drag look split;
backquote toggle; dt clamp; continuous redraw loop; FPS to stdout.

### `offscreen.rs` — the verification oracle (190 lines)
No window; render N frames to Rgba8UnormSrgb texture → PNG readback; deterministic
virtual clock; scripted op stream applied before frame 0; `GLYPH_G_DUMP` debug readback.

### `main.rs` — CLI + wiring (570 lines)
Hand-rolled arg parser (the comment says: "dev/verification tool, not a product CLI");
`SceneChoice` enum → `build_scene()` factory; op-stream parsing (`--pick-*`/`--verb`);
engine smoke + engine-check modes (no GPU).

### Shaders (WGSL, hand-ported from the web app)
- `glyph_field.wgsl` (314 lines) — Slug analytic-coverage glyph rendering: glyphmap
  lookup → quad → group TRS → MVP; fragment = fractional winding-number coverage over
  quadratic beziers, fwidth-scaled minification ramp.
- `cull.wgsl` (82) — backdrop flat-quad pipeline (mean ink × coverage).
- `quad_field.wgsl` (73) — Stage A demo.

## Seams where ecosystem crates could plug in (no order yet)

1. **`SceneLike` hook trait** — the natural interception point for an event-consuming
   UI layer (egui-winit wants `on_window_event`; input routing order matters).
2. **`GroupRow` table + `PickFileInfo` + `SegCull`** — parallel arrays keyed by
   group_id == file index. An ECS would re-express this as entities+components; the
   `sync_segment`/`geom_overrides` invariants would become observers/derived state.
3. **`FlyCamera`/`CameraMode`** — self-contained ~110 lines; replaceable or supplementable.
4. **`main.rs` CLI** — hand-rolled parser, explicitly flagged as non-product.
5. **Windowed loop** — FPS-to-stdout, no panels: an overlay UI pass slots in between
   scene render and `queue.present`.
6. **Buffer layout** — 48 B/80 B `#[repr(C)]` + bytemuck, layouts mirrored by hand in
   WGSL comments (the Stage G strided-color bug came from exactly this mirroring).
7. **Timing** — wall-clock `Instant` only; no GPU timestamps.

## Hard constraints any adoption must respect

- **wgpu 30 pin** — crates that track older wgpu majors are out or need vendoring;
  the indirect-draw Metal bug is a wgpu-30.0.1 fact (their CPU cull is the workaround).
- **Bit-exact determinism chain** — byte → engine → record → instance → pixel is gated
  by `--repo-verify`, `--engine-check`, `tools/check-stage-g.sh`, and PNG byte-compare
  A/Bs. Adopted code must either sit outside this chain (UI, camera, CLI, profiling,
  scene organization) or prove byte-identical output.
- **Mojo engine stays** — it's the differentiator and the oracle discipline lives there.
- **Edition 2021, MSRV per wgpu 30.**
- **The pick pipeline is a feature, not tech debt** — it was chosen over a GPU ID pass
  for exactness + determinism; an ECS/camera crate must not regress it.
