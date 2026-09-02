# egui 0.36.1 Integration Study Report

Repo studied: `/Users/lugo/localdev/viz-native/glyph3d-native/integration/egui` (commit dadf573, 2026-09-01).
Workspace pins: `wgpu = "30.0"`, `winit = "0.30.13"` (root `Cargo.toml` lines 160–162). MSRV 1.95 (0.36.0 changelog).

---

## 1. Architecture & crate layering

Sources: `ARCHITECTURE.md`, `README.md`, `crates/*/Cargo.toml`.

Dependency graph (each layer's ownership):

| Crate | Owns | Depends on |
|---|---|---|
| `emath` | Minimal 2D math: `Vec2, Pos2, Rect, lerp, remap` | (nothing) |
| `ecolor` | Color types: `Color32, Rgba, Hsva` (exists in tree; not mentioned in ARCHITECTURE.md) | emath |
| `epaint` | 2D shapes & text that tessellate into textured triangles: `Shape`, `ClippedShape`, `ClippedPrimitive`, `Vertex`, `Tessellator`, `TextureId`, `TexturesDelta`, `ImageDelta`, `PaintCallback`, font atlas | emath, ecolor, epaint_default_fonts (feature `default_fonts`) |
| `epaint_default_fonts` | Embedded fonts via `include_bytes!()` (Ubuntu-Light, Hack, NotoEmoji, emoji-icon-font); separated for licensing | — |
| `egui` | The GUI library: widgets, layout, `Context`, input state, `Ui`, `Window`, panels. "Depends only on emath and epaint" (ARCHITECTURE.md; ecolor comes via epaint) | emath, epaint |
| `egui-winit` | Translation of winit events → `egui::RawInput`; clipboard, cursor icons/bitmaps, IME, opening URLs, AccessKit adapter (`accesskit` feature) | egui, winit 0.30 |
| `egui-wgpu` | Paints egui triangles with wgpu 30. Two layers: low-level `renderer.rs` (`Renderer`) and an optional high-level `winit.rs` (`Painter`, feature `winit`) that also owns surfaces. Feature `capture` = screenshot capture | egui, epaint, wgpu |
| `egui_glow` | glow (OpenGL) painter; also `egui_glow::CallbackFn` for GL paint callbacks | egui, glow |
| `eframe` | Full framework (native+web) tying egui + egui-winit + egui-wgpu/egui_glow; owns the event loop, surfaces, storage, multi-viewport window management | all of the above |
| `egui_extras` | Extra widgets/features on top of egui (image loaders, tables, strips, syntax highlighting) | egui |
| `egui_plot` | Plot widget | egui |
| `egui_kittest` | Test harness over `kittest` + AccessKit; optional wgpu snapshot rendering | egui, kittest, egui-wgpu (opt), eframe (opt) |
| `egui_inspection` | UI inspection/debug tooling (new in 0.35) | egui |
| `egui_web` | Web-specific glue used by eframe web | egui |
| `egui_demo_lib` / `egui_demo_app` | Demo content + thin wrapper | egui / eframe |

**Minimal set for embedding into an existing wgpu 30 + winit 0.30 app (no eframe):**
`egui` + `egui-winit` + `egui-wgpu` (default features fine; **do not** enable egui-wgpu's `winit` feature — you own the surface; use `egui_wgpu::Renderer` directly). You do NOT need eframe, egui_glow, egui_extras, or egui_plot. You own: the `wgpu::Surface`, its configuration, the event loop, and frame presentation. egui-wgpu's `Painter` (`crates/egui-wgpu/src/winit.rs`) duplicates surface ownership, so skip it.

Cargo sketch:
```toml
egui = "0.36.1"
egui-winit = "0.36.1"          # default-features = false possible; "accesskit" optional
egui-wgpu = "0.36.1"           # without feature "winit"
wgpu = "30.0"
winit = "0.30.13"
```
Note: egui-wgpu re-exports its wgpu (`egui_wgpu::wgpu`) — using that re-export avoids version drift.

---

## 2. Manual integration path (existing wgpu + winit app, egui as overlay)

### 2.1 Construction (once)

```rust
let egui_ctx = egui::Context::default();

let mut egui_state = egui_winit::State::new(
    egui_ctx.clone(),
    egui::ViewportId::ROOT,
    &window,                       // &dyn HasDisplayHandle
    Some(window.scale_factor() as f32), // native_pixels_per_point
    None,                          // Option<winit::window::Theme> (system theme hint)
    Some(device.limits().max_texture_dimension_2d as usize), // max_texture_side
);
```
(`crates/egui-winit/src/lib.rs:138-194`. `State` is per viewport/window. Also `State::set_max_texture_side` if the device isn't known yet.)

```rust
let mut egui_renderer = egui_wgpu::Renderer::new(
    &device,
    surface_config.format,         // should match your surface format
    egui_wgpu::RendererOptions::default(),
);
```
(`crates/egui-wgpu/src/renderer.rs:273`.) `RendererOptions { msaa_samples, depth_stencil_format, dithering: true, predictable_texture_filtering: false }`. egui warns if the format is sRGB (`Rgba8UnormSrgb` etc.) and prefers gamma-space `Rgba8Unorm`/`Bgra8Unorm` (`renderer.rs:435-440`); the helper `egui_wgpu::preferred_framebuffer_format(&caps.formats)` implements that preference (`lib.rs:422`).

Optional: to let egui wake your event loop from other threads:
```rust
let proxy = event_loop.create_proxy();
egui_ctx.set_request_repaint_callback(move |info| { proxy.send_event(...); });
```
(`Context::set_request_repaint_callback`, `context.rs:1965`.)

### 2.2 Per-event

In `ApplicationHandler::window_event`, before your own input handling:
```rust
let response: egui_winit::EventResponse = egui_state.on_window_event(&window, &event);
// EventResponse { consumed: bool, repaint: bool }  — #[must_use]
```
(`lib.rs:301-531`.) Meaning of `consumed`: egui wants exclusive use of the event (click on an egui window, typing into a text field). **Forward to your game/3D input only when `!consumed`.** Details per event kind:

- `MouseInput`, `MouseWheel`: consumed = `ctx.egui_wants_pointer_input()` (pointer over an egui area OR egui is using it).
- `CursorMoved`: consumed = `ctx.egui_is_using_pointer()` (e.g. mid-drag of a slider).
- `KeyboardInput`: consumed = `ctx.egui_wants_keyboard_input()` OR key is Tab (Tab = focus movement, always consumed). Synthetic key presses (`is_synthetic` on focus change) are ignored.
- `Ime`, `Touch`: analogous.
- `Resized`, `CloseRequested`, `Focused`, `HoveredFile`, `DroppedFile`, `ModifiersChanged`, `RedrawRequested`, `ScaleFactorChanged`: consumed = false, repaint = true. Safe to pass every event through; your own resize handling happens independently (note: egui-winit does NOT resize your wgpu surface — you do that yourself).

Almost everything returns `repaint: true`; you may use it to decide whether a redraw is warranted.

The underlying queries live on `Context` (`context.rs:3084-3099`): `egui_wants_pointer_input()`, `egui_is_using_pointer()`, `egui_wants_keyboard_input()`, `text_edit_focused()` — you can also query these directly for finer-grained decisions (e.g. "camera orbit only when egui isn't using the pointer").

### 2.3 Per-frame (exact ordering)

Runs inside `WindowEvent::RedrawRequested` (after rendering the 3D scene's CPU-side prep; GPU ordering below).

```rust
// 1. Gather accumulated input (sets time + screen_rect in points)
let raw_input: egui::RawInput = egui_state.take_egui_input(&window);   // lib.rs:270

// 2+3. Run one or more passes and build the UI
let full_output: egui::FullOutput = egui_ctx.run_ui(raw_input, |ui| {
    // ui: &mut egui::Ui, root Ui covering the viewport
    egui::Window::new("Controls").show(ui.ctx(), |ui| { /* widgets */ });
    // or egui::CentralPanel::default().show(ui.ctx(), ...) / SidePanel / TopBottomPanel
});
```
**Naming gotcha in 0.36:** the classic `Context::run(raw_input, |ctx| ...)` is gone/renamed. Public entry points are `Context::run_ui(raw_input, FnMut(&mut Ui)) -> FullOutput` (`context.rs:800`) and `Context::run_logic(&raw_input, FnOnce(&Context)) -> LogicOutput` (`context.rs:919`, a tick that shows no UI). The manual split form also exists: `Context::begin_pass(raw_input)` (`context.rs:968`) … `Context::end_pass() -> FullOutput` (`context.rs:2554`). `run_ui` internally loops passes up to `Options::max_passes` when a pass calls `Context::request_discard` (first-frame sizing) — prefer `run_ui` unless you need custom pass control.

`FullOutput` (`crates/egui/src/data/output.rs:13`): `platform_output`, `textures_delta` (apply `set` BEFORE painting, `free` AFTER submit), `shapes: Vec<ClippedShape>`, `pixels_per_point`, `viewport_output`. NOTE: `TexturesDelta` panics if dropped with unapplied deltas (0.36 fix #8356); if you skip painting, call `full_output.drop_without_applying_deltas()`.

```rust
// 4. Side effects: clipboard copy, cursor icon, open URL, IME
egui_state.handle_platform_output(&window, full_output.platform_output);   // lib.rs:1092
// (or handle_platform_output_with_event_loop(&window, event_loop, po) to enable
//  bitmap custom cursors via winit::CustomCursor, lib.rs:1107)

// 5. Tessellate shapes -> textured triangles
let clipped_primitives: Vec<epaint::ClippedPrimitive> =
    egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);  // context.rs:2970

let screen_descriptor = egui_wgpu::ScreenDescriptor {
    size_in_pixels: [surface_config.width, surface_config.height],
    pixels_per_point: full_output.pixels_per_point,   // = zoom_factor * native scale factor
};

// 6. Upload changed egui textures (font atlas, images)
for (id, image_delta) in &full_output.textures_delta.set {
    egui_renderer.update_texture(&device, &queue, *id, image_delta);  // renderer.rs:638
}

// 7. Upload uniforms/vertex/index buffers; also runs CallbackTrait::prepare / finish_prepare
let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
let user_cmd_bufs = egui_renderer.update_buffers(
    &device, &queue, &mut encoder, &clipped_primitives, &screen_descriptor,
);   // renderer.rs:964 — MUST be called before render(), render() panics otherwise

// 8. GPU passes: your 3D scene first (LoadOp::Clear), then egui overlay with LoadOp::Load
{
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &frame_view,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear_color), store: wgpu::StoreOp::Store },
            ..Default::default()
        })],
        ..Default::default()
    });
    // ... draw 3D scene ...
}

{
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &frame_view,
            // CRITICAL: Load, not Clear — egui draws on top of the scene
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            ..Default::default()
        })],
        depth_stencil_attachment: None, // unless RendererOptions.depth_stencil_format was set
        ..Default::default()
    });
    egui_renderer.render(
        &mut pass.forget_lifetime(),     // render() wants RenderPass<'static>
        &clipped_primitives,
        &screen_descriptor,
    );   // renderer.rs:506
}

// 9. Submit: user callback buffers first, then the main buffer
queue.submit(user_cmd_bufs.into_iter().chain(std::iter::once(encoder.finish())));

// 10. Free textures AFTER submit (they may still be referenced by in-flight commands)
for id in &full_output.textures_delta.free {
    egui_renderer.free_texture(id);   // renderer.rs:790
}

window.pre_present_notify();
frame.present();
```

Render-pass sequencing notes:
- egui is just another render pass on the same surface texture. egui's own `Painter::paint_and_update_textures` (`egui-wgpu/src/winit.rs:476`) uses `LoadOp::Clear` because eframe owns the whole frame; **for an overlay you must use `LoadOp::Load`** and draw egui after your scene.
- Blend state baked into egui's pipeline: premultiplied alpha (`One, OneMinusSrcAlpha`), consistent with egui's premultiplied color convention (`renderer.rs:443-454`).
- `RenderPass::forget_lifetime()` caveat (`renderer.rs:496-502`): after forgetting, touching the parent encoder while the pass is alive is a runtime error instead of compile-time; drop the pass before continuing with the encoder.
- Depth: egui itself needs no depth (`depth_write_enabled: false`, compare `Always` when a depth format is configured). If you attach your scene's depth buffer to the egui pass, egui draws always pass depth — fine for overlay; otherwise attach none.
- If your surface is sRGB (`*Srgb`), egui will use `fs_main_linear_framebuffer` and log a warning once per pipeline creation; prefer non-sRGB `Rgba8Unorm`/`Bgra8Unorm` surface formats so egui's gamma-space blending matches (helper: `egui_wgpu::preferred_framebuffer_format`).

### 2.4 Repaint / request_redraw semantics (winit 0.30)

eframe's own policy (`crates/eframe/src/native/run.rs:189-255`) — good template:

- Event loop is `ControlFlow::Wait` (never `Poll`; issue #8326 — `Poll` busy-loops a CPU core and on Wayland RedrawRequested only arrives via frame callbacks).
- egui runs + paints on `WindowEvent::RedrawRequested`.
- In `about_to_wait`: compute the earliest pending repaint time across windows (from `Context::request_repaint_after` calls, surfaced per-viewport via `FullOutput.viewport_output[id].repaint_delay`), call `window.request_redraw()` when due, and set `event_loop.set_control_flow(ControlFlow::WaitUntil(next_repaint_time))` or `ControlFlow::Wait`.
- `Context::request_repaint()` / `request_repaint_after(Duration)` / `request_repaint_after_secs(f32)` (`context.rs:1825-1922`): calling at least once per pass forces the next frame; the smallest duration wins. `ctx.has_requested_repaint()` lets the integration check the current pass's request.
- egui only repaints on interaction or animation when idle (README); in a 3D app you likely already render every frame (vsync), so just fold the egui pass in each frame — the egui overhead is ~1-2 ms per frame (README FAQ).
- `EventResponse.repaint` from `on_window_event` tells you the event makes an egui repaint necessary (relevant in a reactive/event-driven loop; moot if you redraw every frame).

---

## 3. Two-way integration patterns

### 3a. egui overlay on top of custom 3D (your primary use case)

Covered above: separate render pass after the scene pass, `LoadOp::Load`, egui clipped to `screen_rect` in points. Input routing via `EventResponse.consumed` / `ctx.egui_wants_pointer_input()` etc. This is exactly how eframe's wgpu integration works and how `bevy_egui`/`three-d`-style integrations are described in README §"How do I render 3D stuff in an egui area".

### 3b. Custom 3D inside an egui panel — PaintCallback (in-repo reference)

Two supported mechanisms (README §FAQ):

**(1) `Shape::Callback` / `PaintCallback`** — draw your own GPU content into a rect of the egui pass.

- egui core: `ui.painter().add(egui::PaintCallback { rect, callback: Arc<dyn Any + Send + Sync> })`. Allocate the rect first: `let (rect, response) = ui.allocate_exact_size(size, egui::Sense::drag());` — the `response` gives you drag/hover for rotating the 3D content.
- wgpu flavor (`crates/egui-wgpu/src/renderer.rs:24-121`): implement `egui_wgpu::CallbackTrait` and wrap with `egui_wgpu::Callback::new_paint_callback(rect, callback)` → `epaint::PaintCallback` → `ui.painter().add(...)`.
  - `CallbackTrait::prepare(device, queue, &ScreenDescriptor, &mut egui_encoder, &mut CallbackResources) -> Vec<wgpu::CommandBuffer>` — called during `Renderer::update_buffers`, before the egui render pass. Update uniform buffers here; may return extra command buffers.
  - `CallbackTrait::finish_prepare(...)` — once per frame after all prepares; buffers from `finish_prepare` are submitted after those from `prepare`.
  - `CallbackTrait::paint(info: PaintCallbackInfo, render_pass: &mut wgpu::RenderPass<'static>, &CallbackResources)` — called inside egui's render pass, in shape order. egui pre-sets the viewport to the callback rect (in pixels) as a courtesy; scissor clipping to the egui clip rect applies.
  - `PaintCallbackInfo` fields: `viewport: Rect` (points), `clip_rect`, `pixels_per_point`, `screen_size_px`; helpers `viewport_in_pixels()`, `clip_rect_in_pixels()`.
  - Shared GPU resources (pipeline, bind groups, buffers) go in `egui_renderer.callback_resources` (a `type_map::TypeMap`; `Send+Sync` required on native): `renderer.callback_resources.insert(TriangleRenderResources {...})` then `resources.get().unwrap()` inside callbacks.
  - Reference implementation: `crates/egui_demo_app/src/apps/custom3d_wgpu.rs` (rotating triangle, drag-to-rotate) and `examples/custom_3d_glow/src/main.rs` (glow equivalent using `egui_glow::CallbackFn::new(move |info, painter| ...)`).
  - If the 3D-in-egui content wants MSAA, set `RendererOptions.msaa_samples` (affects the whole egui pass). NOTE: there's no standalone `custom_3d_wgpu` example under `examples/` — only the glow one; the wgpu one lives in egui_demo_app.
  - How apps like rerun do it: rerun renders its 3D view via `CallbackTrait` too (same `prepare`/`paint` split), effectively a heavyweight callback that runs a full 3D pipeline in `paint` inside egui's pass, with UI panels around it.

**(2) Render-to-texture** — render the scene offscreen and show it as an egui image (`ui.image(...)`).

- `egui_wgpu::Renderer::register_native_texture(&device, &texture_view, wgpu::FilterMode) -> egui::TextureId::User(u64)` (`renderer.rs:809`). **Texture must be `wgpu::TextureFormat::Rgba8Unorm`.**
- Variants: `register_native_texture_with_sampler_options` (custom `wgpu::SamplerDescriptor`, e.g. mipmap/address modes; `compare` is ignored), and `update_egui_texture_from_wgpu_texture[_with_sampler_options]` to re-bind a NEW view to an EXISTING `TextureId` after the offscreen target is recreated (resize) — create one `TextureId` up front and update it each resize.
- Then `ui.image((texture_id, size))` / `egui::Image::new(...)` in the panel. Hit-testing/drag on the image via `response` for camera control.
- This is the better pattern when you need the scene at panel resolution, want egui clipping, or don't want your 3D pass coupled to egui's render pass.

### 3c. Two-way combination

Overlay windows (egui `Window`s, panels floating over the 3D scene pass) + optionally a `TextureId::User` viewport panel + `CallbackTrait` for gizmos drawn directly in the egui pass. Input disambiguation: `ctx.egui_wants_pointer_input()` gates camera controls; `EventResponse.consumed` gates event forwarding.

---

## 4. Best practices & pitfalls

Sources: README FAQ, `crates/egui/src/lib.rs` crate docs ("Integrating with egui", "Debugging your renderer"), source comments.

**Immediate-mode gotchas**
- No retained state; widgets re-created every frame. State you must keep (window positions, scroll offsets, collapsing headers, text-edit state) is keyed by `egui::Id`.
- ID management: window titles are used as ID seeds by default — two windows with the same title collide; give a unique seed: `egui::Window::new(title).id(egui::Id::new(unique))`. Use `ui.push_id(...)` in loops. Widget interaction IDs (dragged slider etc.) are auto-generated — two buttons with the same label are fine (unlike Dear ImGui).
- First-frame jitter: layout of windows/grids uses last frame's size. egui mitigates via `Context::request_discard` (an extra pass); `run_ui` handles this automatically up to `max_passes`. Avoid extra passes in hot frames.
- Async: never `.await` in UI code; use channels (`try_recv`), `Arc<Mutex<T>>`, `poll_promise`, `poll_promise::Promise`, `tokio::sync::watch`.
- `Context` uses a single `RwLock`; access via closures (`ctx.input(|i| ...)`) — don't hold guards across UI code or you'll deadlock.

**Input focus / keyboard capture**
- Route events through `State::on_window_event` FIRST, respect `EventResponse.consumed`.
- Tab is always consumed (focus navigation).
- `ctx.egui_wants_keyboard_input()` = any widget has focus; `ctx.text_edit_focused()` = a TextEdit specifically. Gate your game's keyboard shortcuts on these.
- Synthetic key presses on focus change are dropped by egui-winit; focus loss clears modifiers (no sticky-modifier bug).
- IME is handled by `State` + `handle_platform_output` (calls `window.set_ime_allowed/cursor_area`); don't fight it.

**Texture management**
- Apply `textures_delta.set` via `Renderer::update_texture` BEFORE painting; drain `textures_delta.free` via `Renderer::free_texture` AFTER `queue.submit` (`winit.rs:738-747` explains: destroying a texture referenced by in-flight command buffers invalidates them).
- `TexturesDelta` panics on drop if unapplied (0.36) — use `FullOutput::drop_without_applying_deltas()` when skipping a frame.
- User (native) textures must be `Rgba8Unorm`; there is no sRGB user-texture support in the shader path.
- Call `State::set_max_texture_side(device.limits().max_texture_dimension_2d)` so the font atlas doesn't exceed device limits.
- `ctx.load_texture(...)` (via epaint) for CPU images; `register_native_texture` for GPU-rendered targets.

**HiDPI / zoom**
- All egui coordinates are logical points; physical size = points × pixels_per_point.
- `egui_winit::pixels_per_point(&ctx, &window)` = `ctx.zoom_factor() * window.scale_factor()` (`lib.rs:54`). `State::take_egui_input` feeds `native_pixels_per_point` from `window.scale_factor()` each frame and on `ScaleFactorChanged`.
- Zoom: `ctx.set_zoom_factor(f)` (Cmd +/- handled by egui if `Options::zoom_with_keyboard`); `ctx.set_pixels_per_point` now just translates to `set_zoom_factor` (`context.rs:2348`).
- `ScreenDescriptor.pixels_per_point` must be `full_output.pixels_per_point` — mismatch = blurry or mis-scaled UI and wrong scissor rects.
- Colors: premultiplied alpha everywhere unless stated otherwise.

**Performance**
- Expect ~1-2 ms CPU per frame for egui (README); full layout runs every frame.
- Tessellation runs every frame; `Context::tessellate` comment (`context.rs:2977`): comparing shapes to skip re-tessellation costs ~50% of tessellating — not worth caching.
- Large scroll areas are the classic CPU trap: layout all children each frame; only lay out visible rows for huge lists (use `egui_extras::TableBody` rows or manual `ScrollArea::show_rows`).
- Skip repaints when idle if you are event-driven: gate redraw on `EventResponse.repaint`, `ctx.has_requested_repaint()`, and your own scene animation state. If you render at vsync anyway, just include the egui pass unconditionally.
- `RendererOptions::dithering` (default true) reduces banding on gradients; set `predictable_texture_filtering`/`RendererOptions::PREDICTABLE` only for snapshot tests (software filtering in shader).
- egui anti-aliases via tessellation feathering (`TessellationOptions`), not MSAA; MSAA only matters for embedded 3D callbacks.

**Renderer integration details**
- `Renderer::render` panics if `update_buffers` wasn't called with the same primitives.
- egui pipeline: cull_mode None, triangle list, u32 indices, vertex layout 20 bytes (`Float32x2, Float32x2, Uint32`).
- `Renderer::texture(&TextureId)` exposes the wgpu texture+bind group for custom paint hooks.
- egui-wgpu `winit` feature's `Painter` owns the surface and clears with `LoadOp::Clear(clear_color)` — not suitable when you own the frame; also it drives `present` itself. Use raw `Renderer`.

---

## 5. Testing — egui_kittest (brief)

`crates/egui_kittest` (authors: Lucas Meurer, Emil Ernerfeldt). Built on `kittest` + AccessKit.

- Core: `Harness::new_ui(|ui| ...)`, `Harness::new_ui_state(|ui, state| ...)`, `Harness::builder()` (set `screen_rect`, `pixels_per_point`, theme, OS, `max_steps`, `step_dt`). Holds an `egui::Context`; disables cursor blink/scroll animation for determinism; runs one initial frame so the AccessKit tree is queryable immediately.
- Interaction: query by accessibility role/label (`harness.get_by_role_and_label(Role::CheckBox, "Accept terms").click()`), `Harness::step/run/run_steps` (run = step until the UI stops requesting repaint; `ExceededMaxStepsError` with `RepaintCause` diagnostics if it never settles).
- `snapshot` feature: image snapshot testing via `dify`. `harness.try_snapshot("name")`, `SnapshotOptions { threshold, max_failed_pixels, output_path }`, per-OS thresholds (`OsThreshold`), `SnapshotResults` aggregation, `debug_open_snapshot`. Configured via `kittest.toml` (repo's: `output_path = "tests/snapshots"`, threshold 0.6 macOS CI source-of-truth, 2.0 elsewhere). Update flow: `./scripts/update_snapshots_from_ci.sh` or `kitdiff`.
- `wgpu` feature: real GPU rendering headless (`crates/egui_kittest/src/wgpu.rs`). `default_wgpu_setup()` creates an instance without display handle, prefers CPU/software adapters (Metal > Vulkan > Dx12 backend ordering; `DeviceType::Cpu` first), removes BROWSER_WEBGPU (blocking screenshots unsupported). Uses `egui_wgpu::Renderer` with `RendererOptions::PREDICTABLE` (`msaa_samples: 1, depth_stencil_format: None, dithering: false, predictable_texture_filtering: true`) and a 10s GPU wait timeout. `texture_to_image.rs` reads back to `image::RgbaImage`.
- `eframe` feature: spawn a real eframe app for integration tests (`spawn_eframe_app`); on macOS must run on the main thread — requires `[[test]] harness = false` in Cargo.toml.
- Accessibility testing is the same tree as screen readers — see `docs/accessibility.md`.

---

## 6. External docs

- **https://egui.rs** — homepage + live web demo (compiled from `egui_demo_lib` via eframe); source links inside the demo.
- **docs.rs**: `docs.rs/egui` (crate docs include "Integrating with egui" walkthrough — same content as `crates/egui/src/lib.rs` doc comments, incl. the RawInput → run_ui → FullOutput → tessellate loop and a "Debugging your renderer" checklist), `docs.rs/egui-winit`, `docs.rs/egui-wgpu` (documented features via document-features), `docs.rs/eframe`, `docs.rs/egui_kittest`, `docs.rs/epaint`, `docs.rs/emath`.
- Repo `docs/` folder contains ONLY `docs/accessibility.md` (AccessKit labeling, custom widget `WidgetInfo`, kittest testing). It complements rather than duplicates the online docs — everything else lives in crate-level rustdoc + README FAQ.
- Wiki: "3rd party egui integrations" and "3rd-party egui crates" (github.com/emilk/egui/wiki); GitHub Discussions Q&A; Discord. `eframe_template` repo for new web/native apps (not relevant for embedding).
- CHANGELOG.md is per-release and detailed (useful for breaking-change review when upgrading; e.g. 0.36 removed `Modifiers` from `RawInput` — it's now an `egui::Event::ModifiersChanged`).

---

## Appendix: key file paths in the studied repo

- `ARCHITECTURE.md`, `README.md` (FAQ, integrations, immediate-mode rationale)
- `crates/egui/src/lib.rs` — crate docs: "Integrating with egui" (lines 97-137), conventions
- `crates/egui/src/context.rs` — `run_ui` (800), `run_logic` (919), `begin_pass` (968), `end_pass` (2554), `request_repaint*` (1825-1922), `set_request_repaint_callback` (1965), `tessellate` (2970), `egui_wants_*` (3084-3103), zoom (2340-2397)
- `crates/egui/src/data/output.rs` — `FullOutput` (13), `PlatformOutput`, `OutputCommand`
- `crates/egui-winit/src/lib.rs` — `EventResponse` (62), `State` (83): `new` (138), `take_egui_input` (270), `on_window_event` (301), `handle_platform_output[_with_event_loop]` (1092/1107), `pixels_per_point` (54), `screen_size_in_pixels` (41), `WindowSettings` re-export (28)
- `crates/egui-wgpu/src/renderer.rs` — `Renderer` (237): `new` (273), `render` (506), `update_texture` (638), `free_texture` (790), `update_buffers` (964), `register_native_texture[_with_sampler_options]` (809/859), `update_egui_texture_from_wgpu_texture` (830), `CallbackTrait` (88), `Callback::new_paint_callback` (33), `CallbackResources` (16), `ScreenDescriptor` (124), `RendererOptions` (175, `PREDICTABLE` at 217)
- `crates/egui-wgpu/src/lib.rs` — `RenderState` (107), `WgpuConfiguration` (334), `SurfaceConfig` (71, `LOW_LATENCY`/`HIGH_THROUGHPUT`), `preferred_framebuffer_format` (422), `depth_format_from_bits` (441), `SurfaceErrorAction` (314)
- `crates/egui-wgpu/src/winit.rs` — `Painter` (30) + `paint_and_update_textures` (476): reference for submit-order, free-after-submit, surface error recovery, MSAA/depth attachments (study even if unused)
- `crates/egui-wgpu/src/capture.rs` — screenshot capture (feature `capture`)
- `crates/egui_demo_app/src/apps/custom3d_wgpu.rs` — canonical wgpu paint-callback example
- `examples/custom_3d_glow/src/main.rs` — glow paint-callback example (the only `custom_3d_*` under examples/)
- `crates/eframe/src/native/run.rs` — `WinitAppWrapper`: ApplicationHandler, `check_redraw_requests` (189), ControlFlow policy (245-254), `about_to_wait` (528)
- `crates/egui_kittest/src/{lib.rs,builder.rs,snapshot.rs,renderer.rs,wgpu.rs}`, `kittest.toml`
- `docs/accessibility.md`
