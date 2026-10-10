> **History.** Moved to `out/history/integration/` on 2026-10-10: 2026-09 research
> and stage handoffs, all executed or superseded. What is true now: `README.md`,
> root `AGENTS.md`.

# 04 — The UI layer: egui is the free lunch

*Research date 2026-09-01; versions verified via docs.rs/crates.io dependency tables
that day. This is the highest-leverage note: it closes Stage G's "remaining gaps" #1
(selection UI), #4 (IME), and #6 (flash restore needs a UI/tick story) with one dep.*

## TL;DR

**egui 0.36.1 + egui-wgpu + egui-winit is a verified exact match for your stack**
(`wgpu ^30.0`, `winit ^0.30.13`, MIT/Apache, released 2026-08-07 — weeks after wgpu 30).
It renders as an **overlay pass into your existing encoder** — zero changes to the glyph
pipeline — and brings panels, docking, tree views, text input, desktop IME, and
AccessKit accessibility. For world-space labels, **glyphon 0.12** (also verified
`wgpu ^30.0.0`) does screen-space text now, with cosmic-text as the shaping engine;
everything else (iced, masonry/xilem, vizia, floem, Slint, gpui) is wgpu-lagged,
alpha, Skia-based, or license-blocked in 2026.

## The verdict table

| toolkit | version / date | wgpu-30? | winit-0.30? | verdict |
|---|---|---|---|---|
| **egui + egui-wgpu + egui-winit** | 0.36.1, 2026-08-07 | **YES** (`^30.0`; 0.35 lagged at 29 — issue #8312 closed by 0.36) | **YES** (`^0.30.13`) | **ADOPT** |
| iced | 0.14.0, 2025-12-07 | **NO — `^27`** (iced_wgpu + its cryoglyph text renderer) | YES | WAIT — would mean a second GPU device + composite hack; re-check after their next wgpu bump |
| masonry / xilem | 0.4.0 / 0.4.0, 2025-10-29 | NO — via vello ^0.6 (vello 0.10 current is `wgpu ^29.0.3`) | YES (masonry_winit) | WAIT — xilem docs say "alpha state" verbatim; most likely future retained option |
| vizia | 0.4.0, 2026-04-23 | n/a — **moved to Skia** | own shell | reference-only |
| floem (Lapce) | 0.2.0, 2024-11-14 | **NO — wgpu ^22 + forked winit 0.29** | forked | reference-only; Lapce-coupled, no release in 2025–26 |
| Slint | 1.12/1.13 line | femtovg-wgpu renderer exists | — | license-blocked for most products (GPL/commercial dual) — check terms before any use |
| gpui (Zed) | mid blade→wgpu migration | transitioning | own shell | reference-only |
| glyphon | **0.12.0, 2026-07-09** | **YES `^30.0.0`** | dev-deps 0.30.12 | **ADOPT for overlay/HUD + projected labels** |
| cosmic-text | 0.19.0, 2026-04-22 | n/a (CPU shaping/raster) | — | **ADOPT as shaping engine** (HarfRust shaping, color emoji via swash, Chromium/Firefox-derived fallback, bidi, selection/editing primitives) |
| accesskit | 0.25.0, 2026-08-29 | — | accesskit_winit exists | ADOPT (via egui, or standalone on the hand-rolled UI) |

egui ecosystem re-synced to 0.36 in Aug 2026: **egui_dock 0.21.1** (new maintainers,
active), **egui_flex 0.8.0**, **egui_ltreeview 0.9.0** (tree view — a file tree for
free). 2D graph insets when wanted: egui_graphs 0.31.0 (Jun 2026), egui-snarl (active);
egui_node_graph itself is dormant (use trevyn/egui_node_graph2).

## Integration shape (established pattern, exactly your case)

1. `egui_winit::State` consumes your existing `WindowEvent` stream **before** your
   scene hooks (`SceneLike::on_key` etc.) — egui marks events consumed, you skip
   routing consumed input to the fly camera/verbs. This is the one real wiring task.
2. Each frame: `egui::Context::run_ui` builds panels (0.36 renamed `run` → `run_ui`; corrected
   against the vendored source, `glyph3d-native/integration/egui-integration-report.md` §2.3) → `egui_wgpu::Renderer` paints
   egui's triangulated output **into the same render pass after your glyph/backdrop
   draws** (or to an offscreen texture if you want it through a composite chain —
   the bevy_egui/bevy_iced-proven mechanism).
3. `Renderer::update_egui_texture_from_wgpu_texture` lets panels *display* your
   renders (pick-cache close-ups, atlas inspector).
4. `egui_kittest` gives snapshot/AccessKit-based UI tests.

What it hands you immediately: an inspector panel for picks (file/row/col/char/slot),
verb buttons replacing `h/g/t/x`-by-memory, the FPS line moved into-window, cull/LOD
tuning sliders (`LOD_MIN_PX`, `BACKDROP_GAIN` are consts today), group-table browser
(egui_ltreeview over rel paths), text input + desktop IME (winit 0.30 `Ime` events →
`egui::Event::Ime`, preedit rendering included), accessibility via AccessKit.

Caveats (verified): egui-wgpu disables wgpu default features — re-enable your backends;
candidate-window positioning quirks are the remaining IME rough edge; WASM IME is
winit's open gap (#4424) — irrelevant to you.

## Embedding the canvas: two supported patterns (fullscreen modal vs framed)

Both of the team's floated layouts map onto established egui integrations:

1. **Overlay (canvas is the world, UI floats on top).** Scene renders first, egui
   draws after, same encoder. Fullscreen "modal" = `egui::CentralPanel` over everything,
   or a side panel with the field visible beside it. Pros: canvas pixels are the
   swapchain directly (crisp, no extra pass, DPI-free), cheapest. Con: UI is always
   *over* the canvas; canvas can't be one widget among others.
2. **Framed (canvas is a texture inside egui's layout).** Render the glyph scene to an
   offscreen color target each frame, register it
   (`Renderer::update_egui_texture_from_wgpu_texture`), display via `egui::Image` in any
   panel — canvas becomes a widget next to directory trees, pick inspectors, whatever;
   multiple canvases/PiP trivially. This is the bevy_egui-proven mechanism. Costs: one
   extra render target + sampling pass, and you must size the offscreen target to the
   panel's physical rect (or accept blur) — DPI handling is yours.

Recommendation: build the egui layer as overlay first (it exercises the same
`apply_pick`/`apply_verb` command surface either way — panels call the exact API the
CLI ops use), and add the framed/texture path only when a layout actually needs the
canvas as a sibling widget. The `SceneLike` pick/verb hooks are precisely the seam:
egui reads scene state (group table, picks, cull stats) and issues the existing verbs;
the determinism chain never learns egui exists.

## World-space text labels (file names above groups, HUD anchors)

- **glyphon is strictly 2D screen-space** (pixel `TextArea`/`TextBounds` rects, no
  projection uniforms — verified from API): project world anchors through your
  view-proj per frame, feed pixel rects. Works today, zero shader work, and gets
  emoji/fallback/shaping for *label* text.
- **True world-space labels** (part of the 3D scene, occluded/culled with it): keep
  your glyph pipeline; use **cosmic-text as the front-end** (shaping/fallback/raster
  via swash) emitting into your existing atlas machinery. This is also the designated
  long-term path if the Mojo engine's *known gaps* (emoji atlas — Stage G gap #3,
  IME) ever need a CPU-reference or fallback: cosmic-text 0.19 does color emoji and
  font fallback today.
- Watch: **Glifo 0.3.0** (Linebender's new glyph-rendering crate, 2026-08-07; atlas
  cache + `GlyphRenderer`/`DrawSink` traits, backend-agnostic, no wgpu) — explicitly
  "experimental"; the designated extension point if the Linebender stack matures onto
  wgpu 30.

## What stays yours

In-scene code text rendering stays the Mojo engine + Slug WGSL — that's the
differentiator, bit-exact and verified. egui/glyphon/cosmic-text live at the UI layer
(chrome, labels, inspector) and as the shaping *front-end* if you ever re-rasterize;
none of them touches the byte→record→instance determinism chain.

*Sources: docs.rs dependency tables for egui-wgpu 0.36.1, iced_wgpu 0.14, vello 0.10,
masonry 0.4, floem 0.2, glyphon 0.12, cryoglyph 0.1; egui CHANGELOG + issue #8312;
winit issues #1497/#4424; cosmic-text docs + issue #327; 2026 Rust GUI survey
(blog.wybxc.cc) + boringcactus 2025 survey; Slint CHANGELOG; docs.rs/glifo 0.3.0 —
all retrieved 2026-09-01.*
