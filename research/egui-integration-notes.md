# egui Integration Prep Notes

Date: 2026-09-01 · Vendored source: `integration/egui` (shallow clone, master @ `dadf573`, egui 0.36.1; removed 2026-10-10, upstream at that commit has it)
Companion deep-dive: `out/history/integration/egui-integration-report.md` (330 lines, API-level detail, all names verified against the checked-out source)

---

## TL;DR

Version stars align: our crate pins **wgpu 30 / winit 0.30**, and egui 0.36.1's workspace pins **wgpu 30.0 / winit 0.30.13**. No shim or downgrade needed — this matches the plan in `out/history/STAGE_I_REPORT.md` ("egui stage — next stage", unblocked after the glam bump).

Integration shape: **embed egui manually** (egui + egui-winit + egui-wgpu), **not** eframe. We keep our own event loop, surface, and scene pass; egui becomes a second render pass with `LoadOp::Load` on top. The heavy work is *input-consumption gating* (we currently have none), not rendering.

⚠️ Pre-flight blocker: `native/src/scene.rs` + `glyph_scene.rs` have uncommitted in-flight `FrameTarget` refactor changes whose call sites (`windowed.rs:125`, `offscreen.rs:111`) still use the old signature — tree likely doesn't compile. Finish or revert before branching for egui. (That refactor actually helps us: it bundles target views, which the overlay pass also needs.)

---

## 1. Crate map (what each piece owns)

| Crate | Role | Need it? |
|---|---|---|
| `emath` / `ecolor` | 2D math, colors | transitive |
| `epaint` | shapes → textured triangles, font atlas, `TexturesDelta`, `PaintCallback` | transitive |
| `egui` | GUI core: `Context`, widgets, layout. Depends only on emath+epaint | **yes** |
| `egui-winit` | winit event → egui translation, clipboard/IME/cursor, `State` | **yes** |
| `egui-wgpu` | triangle painting, `Renderer`, texture mgmt, callbacks | **yes**, without its `winit` feature (that pulls in `Painter`, which owns the surface and clears — wrong for an overlay) |
| `eframe` | owns event loop + surface | **no** — we have our own |
| `egui_extras`, `egui_plot` | tables, strips, plots | later, as needed |
| `egui_kittest` | Harness + snapshot tests (wgpu feature = headless real-GPU) | when UI tests arrive |

Note: `puffin_egui` (profiler UI we'd like) is still pinned to egui 0.33 — blocked until a release targeting 0.36 (per STAGE_I report).

## 2. Per-frame lifecycle (embedding recipe)

Construction (once, in `App::resumed` after `surface.configure`):
```rust
let egui_ctx = egui::Context::default();
let egui_state = egui_winit::State::new(egui_ctx.clone(), ViewportId::ROOT, &window,
                                        Some(scale_factor), theme, max_texture_side);
let egui_renderer = egui_wgpu::Renderer::new(&device, surface_format, RendererOptions::default());
```

Per window event (top of `App::window_event`, before our match):
```rust
let resp = egui_state.on_window_event(&window, &event);   // -> EventResponse { consumed, repaint }
```
Forward to scene/camera only when `!consumed` / `!egui_ctx.wants_keyboard_input()` / `!wants_pointer_input()`.

Per frame (in `WindowState::render`, between scene pass and profiler resolve/submit):
1. `let raw_input = egui_state.take_egui_input(&window);`
2. `let output = egui_ctx.run_ui(raw_input, |ui| { /* build UI */ });` — **0.36 naming gotcha**: old `Context::run` is now `run_ui`; `begin_pass`/`end_pass` still exist for the split form.
3. `egui_state.handle_platform_output(&window, output.platform_output);`
4. `let prims = egui_ctx.tessellate(output.shapes, output.pixels_per_point);`
5. `ScreenDescriptor { size_in_pixels, pixels_per_point }` — ppp = zoom_factor × scale_factor, must match.
6. `update_texture` per `textures_delta.set`, then `update_buffers` (**mandatory** — `render` panics otherwise).
7. Scene pass already ran with `LoadOp::Clear`; egui pass on the **same encoder**, same color view, **`LoadOp::Load`**, **no depth attachment**.
8. `queue.submit(...)`, then `free_texture` per `textures_delta.free` **after submit**, then present.
9. Skipping a frame? `TexturesDelta` panics if dropped unapplied — call `output.drop_without_applying_deltas()`.

Repaint policy: eframe uses `ControlFlow::Wait` + `WaitUntil(next_repaint)`; our app already redraws continuously (unconditional `request_redraw` in `about_to_wait`, vsync-capped by Fifo), which is exactly what egui wants — nothing to change.

## 3. Two-way patterns (for later stages)

- **UI over 3D** (our stage): overlay pass as above. Canonical reference: `egui-wgpu`'s own `paint_and_update_textures` for submit ordering.
- **3D inside egui** (future viewport panels): either
  - `egui_wgpu::CallbackTrait` (`prepare`/`paint` with `RenderPass<'static>`) via `Callback::new_paint_callback(rect, cb)` + `renderer.callback_resources` — canonical sample: `crates/egui_demo_app/src/apps/custom3d_wgpu.rs` (no wgpu custom-3d under `examples/`, only glow); or
  - render-to-texture: `register_native_texture(&device, &view, filter)` → `ui.image(...)`. Texture must be `Rgba8Unorm`; rebind with `update_egui_texture_from_wgpu_texture` after resize.

## 4. Our attachment points (from codebase recon)

| Hook | File | Location |
|---|---|---|
| Event feed + consumption gating | `native/src/windowed.rs` | `App::window_event` (line 299); grab logic lines 76–103; `device_event` line 368 |
| Egui render pass (`LoadOp::Load`, no depth) | `native/src/windowed.rs` | `WindowState::render` lines 125–137; renderer init in `App::resumed` ~line 246 |
| Resize → ScreenDescriptor | `native/src/windowed.rs` | `WindowState::resize` line 57 (call site 303) |

Frame today: acquire → one clear-pass per scene → submit → `queue.present`. Egui pass slots in as pass #2 on the same encoder. `WindowState` is where egui state lives.

## 5. Pitfalls specific to our code

1. **No input consumption exists today.** Every key falls through to the fly camera + verb keys (`h/g/t/x` are bare letters!) — typing in an egui text field *will* fly the camera and fire verbs. Gating (`wants_keyboard_input` / `wants_pointer_input`) is the main integration work.
2. **Pointer grab vs UI**: right-drag look confines+hides the cursor; egui is unusable while grabbed. Rule: opening/focusing UI ungrabs (`state.ungrab()`). Raw `DeviceEvent::MouseMotion` bypasses egui entirely — fine, it's only active while grabbed.
3. **sRGB surface**: we prefer an sRGB format; pass the *actual* surface format to `Renderer::new` (don't hardcode). egui prefers non-sRGB but handles sRGB targets correctly.
4. **Depth**: omit depth attachment on the egui pass entirely; sharing `self.depth` buys nothing.
5. **wgpu-profiler**: wrap the egui pass in its own query block mirroring the scene scheme so it shows in the once-a-second profile line.
6. **Protect the oracle**: offscreen mode (`offscreen.rs`) is the deterministic byte-compare oracle for every stage — keep egui strictly inside `windowed.rs`, behind a cargo feature (e.g. `egui-ui`, default-on) plus a `--no-ui` runtime flag for windowed smoke tests. Offscreen output must stay byte-identical.
7. **egui CPU cost**: tessellation is cheap; large scroll areas are the classic trap (~1–2 ms/frame is typical). Continuous redraw means egui animations run free.
8. **IDs**: window titles collide across same-titled windows — use `.id(Id::new(...))` when that matters.

## 6. Docs & resources

- Vendored: `integration/egui/` — `ARCHITECTURE.md`, `README.md`, `CHANGELOG.md`, `docs/accessibility.md` (repo docs are thin; the real API walkthrough "Integrating with egui" lives in the egui crate rustdoc).
- Web: egui.rs (live demo), docs.rs/egui, docs.rs/egui-wgpu, docs.rs/egui-winit (0.36 pages).
- Testing when we get there: `egui_kittest::Harness`, `snapshot` feature (image diffing, per-OS thresholds, `kittest.toml`), `wgpu` feature for headless real-GPU; on macOS the `eframe` feature needs the main thread.

## 7. Suggested stage shape (when backend refactor lands)

1. Resolve/commit the in-flight `FrameTarget` refactor (pre-flight blocker).
2. Add deps `egui 0.36`, `egui-winit 0.36`, `egui-wgpu 0.36` (no `winit` feature) behind `egui-ui` feature.
3. `resumed`: construct Context/State/Renderer with actual surface format.
4. `window_event`: feed egui first; gate scene input on consumption/wants-*.
5. `render`: egui pass with `LoadOp::Load`, no depth, same encoder; profiler query block.
6. Minimal debug UI (FPS + camera readout) as the first widget; verify offscreen oracle still byte-identical; `--no-ui` flag for smoke tests.
