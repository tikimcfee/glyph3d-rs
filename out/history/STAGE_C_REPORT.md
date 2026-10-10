> **History.** Moved to `out/history/` on 2026-10-10: a dated record, not current state. What is true now: `README.md`, root `AGENTS.md`, `out/MAINTENANCE-NOTES-2026-10-08.md`.

# Stage C — Slug analytic-coverage glyph renderer (WGSL / raw wgpu)

Status: **complete and verified**. `cargo build --release` is clean (0 warnings).

## What was built

| File | Lines | Role |
|---|---|---|
| `native/src/atlas.rs` | 227 | Parses `curves.bin`/`glyphmap.bin`/`glyphs.bin`/`codepoints.bin` per FORMAT.md (magic-checked, trie sanity-asserts 'A'→slot 34/advance 1229). Uploads curves (1024×161) and glyphmap (1024×5) verbatim as `Rgba32Uint` textures (`textureLoad`, no filtering). Trie stays on CPU. |
| `native/src/shaders/glyph_field.wgsl` | 306 | The Slug pipeline: shared vertex transform + fractional winding-number coverage fragment. |
| `native/src/text.rs` | 273 | UTF-8 file → instances: `str::chars` decode → trie lookup → monospace grid (cell advance 1229 fu, em 2320 fu → world cell height 1.0, line pitch 1.25), tab stops (4 cells), syntax-ish palette (keywords/numbers/strings/comments/punctuation, keyword recolor at word flush). Missing/bitmap codepoints occupy their advance, emit no instance. |
| `native/src/glyph_scene.rs` | 422 | Instance/group buffer upload, bind group, pipeline (premultiplied alpha, depth-test-no-write), camera (Front-fit + zoom, Orbit). |
| `native/src/main.rs` | 156 | CLI: `--render-file P`, `--screenshot O`, `--frames N`, `--zoom F`, `--copies N`, `--demo` (Stage A quad field kept behind the flag). Default windowed mode shows the text field (default file: this crate's `main.rs`) with orbiting camera. |
| `scene.rs` / `offscreen.rs` / `windowed.rs` | — | Both scenes sit behind a new `SceneLike` trait (`depth_format`/`instance_count`/`render`); offscreen + windowed drive either scene unchanged. |

## Shader design decisions

**Vertex** (port of `glyphVertex.js::buildGlyphVertexTransform`):
- Instances from a read-only storage buffer indexed by `instance_index` (Stage A access pattern kept); quad geometry from `vertex_index`, 6 verts, no vertex buffers. One `draw(0..6, 0..N)` per frame.
- Quad corners are `[0,1]²` with uv == position, v running bottom→top — matching the web's PlaneGeometry, so **no y-flip** against the y-up curve data (FORMAT.md).
- Glyph-map lookup in the vertex; bitmap slots get a square quad (`height` side), outline keeps `advance` (widthCompress dial `k=1`, not ported).
- Group TRS exactly as web: `rotate(quat, (aligned+iPos)*scale) + offset`, quat sandwich in cross-form. Rotation plumbing is live (identity quats in the staged data).
- Vertex culls degenerate to outside-NDC `vec4(2,2,2,1)`: group-id OOB (storage reads clamp, so the id is clamped *and* culled), group alpha ≤ 0.01, per-group clip window (col 4, tested against pre-transform anchor y).

**Fragment** (port of `GlyphField.js::_buildOutputNode` Slug branch):
- `compute_coverage` is a direct semantic port, including the **stable-root** quadratic solve (`q = b.y + sign(b.y)·√…`, roots `q/a.y`, `c.y/q`) and the **near-parallel line guard** (`|p0.y−p2.y| ≥ 1e-6`) — the two fixes the web comments call out ("spurious crossing" stray lines, the 'g' artifact).
- Two rays per pixel: +X directly, +Y via `rot90`; averaged, clamped. `MAX_CURVES = 256` loop cap kept (real cap ≈ 53 curves for '@').
- `fwidth(glyphUV)` footprint scaling; minification ramp `m = smoothstep(min_lo..min_hi)` with the web's exact defaults `dilatePx 0.75 / soften 0.45 / minLo 0.06 / minHi 0.20` as uniform `Params` — the "forgot my glasses" fuzzy silhouette at zoom-out, untouched magnified path.
- **Color pipeline**: instance colors are packed RGBA8 sRGB display values; shader decodes with `pow(2.2)` and the `Rgba8UnormSrgb` target's hardware encode round-trips them (matches the web's pow-then-encode intent).
- **Blending differs deliberately from the web**: premultiplied output (`rgb·alpha`, blend `ONE / OneMinusSrcAlpha`). The web emits `vColor·cov` with `alpha=cov` under three's NormalBlending, which applies coverage twice at edges; premultiplied is the correct coverage composite and looks identical at full coverage.

## Skipped (deliberately)

- **Bitmap emoji pixels** — no emoji atlas was exported. Staging skips BITMAP slots (they keep their 2× cell advance as blank space); the fragment still branches on `mode==1` *before* the `curveCount==0` empty test (FORMAT.md requirement) and discards.
- **Frame mode** (external video grid), **highlight tint/fill** (`vAddedColor`/`vFillAmount`), **stipple-dither LOD fade** (`ditherSpan`) — hard discard at alpha==0 instead.
- **Occluder LOD material** (opaque early-Z variant) — Stage D candidate for deep-stacked scenes.

## Verification (all via offscreen `--screenshot` + visual inspection)

1. **Whole file** (`glyphVertex.js`, 303 lines, 16,703 instances): upright, monospace-exact spacing, no overlap, no flipped glyphs, syntax colors correct — [out/stageC.png](../../out/stageC.png).
2. **Near camera** (4-line test file, zoom 4): beziers perfectly smooth at ~200 px glyph height — no polygonal edges; AA smooth on diagonals and rounds — [out/stageC_zoom.png](../../out/stageC_zoom.png), crop [out/crop_zoom4.png](../../out/crop_zoom4.png).
3. **Line-height A/B** (identical code vs comment lines): measured ink bands 63/61/63/61 px — identical (±2 px phase); heavier comment look at full-file zoom is the *intended* minification dilation, not a layout bug.
4. **Emoji/tab** (`🐀🚀` + tabs): 2 bitmap/missing codepoints → blank double-width cells; tab stops land on multiples of 4 — [out/stageC_emoji.png](../../out/stageC_emoji.png).
5. **Unicode fallback**: Ω é ü → ± × render via the Meslo/DejaVu chain slots.

## Performance (1600×1000 offscreen, release build)

Stress scene: 60 copies of `glyphVertex.js` = **1,002,180 glyph instances** (45.8 MiB storage buffer, 60 group rows), one draw call, 600 frames:

| View | CPU encode+submit | GPU wall (total/frames) | fps |
|---|---|---|---|
| All 60 blocks visible (minified, dilated) | 232 µs/frame | 6.18 s / 600 = **10.3 ms/frame** | **~97** |
| Zoomed 45× (fragment-heavy: full curve loop on most of 1.6 M px) | 224 µs/frame | 6.52 s / 600 = **10.9 ms/frame** | **~92** |

The render path holds ~90–97 fps at one million glyph instances — the "millions of glyphs" proof. (Offscreen `submit+render` is CPU-side encode; GPU wall is derived from the blocking readback that drains the whole 600-frame queue.)

## What Stage D/E need to know

- **Slot layout (48 B, `GlyphInstance` in glyph_scene.rs / `InstanceSlot` in WGSL)**: `pos: vec3f` (pen-origin left edge, cell-vertical center) · `glyph_id: u32` · `row, col: u32` · `color: u32` packed RGBA8 sRGB · `group_id: u32` · `advance, height: f32` (world units) · `flags: u32` · `_pad: u32`. Differs from the web's stride-11 bitcast-f32 layout by carrying color+group inline (they're per-instance attributes there); count lanes are native u32, float lanes native f32 — no bitcasting.
- **Group table**: storage buffer of `vec4f`, `GROUP_STRIDE = 5` per row — col0 offset.xyz · col1 quat xyzw · col2 color.rgb+alpha (alpha ≤ 0.01 culls) · col3 scale.xyz+colorBlend · col4 clipTop/clipBottom/clipEnabled. 80 B/row. Rotation is plumbed and tested with identity quats only.
- **Bindings** (`group(0)`): 0 camera uniform · 1 instances (RO storage, VERTEX) · 2 groups (RO storage, VERTEX) · 3 glyphmap `texture_2d<u32>` (VERTEX) · 4 curves `texture_2d<u32>` (FRAGMENT) · 5 Params uniform (max_groups + 4 LOD dials).
- **Atlas re-encode for live growth (Stage E)**: normalization denominators are in `glyphs.bin` slot records (parsed, not yet uploaded); `Atlas::lookup` + `FLAG_MISSING` already identify growth candidates. The trie/two-texture addressing contract (`(i%1024, i/1024)`, 2 texels/curve) is centralized in `atlas.rs` + the WGSL header comment.
- **Depth/blend**: glyph pass tests depth (`Less`) but does not write it, premultiplied blend; an opaque occluder variant (web's second material) should be a separate pipeline when deep scenes land.
