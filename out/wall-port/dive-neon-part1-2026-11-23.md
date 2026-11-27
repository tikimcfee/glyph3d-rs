# Wall Port — Part 1: Independent Investigation

**Evidence legend**
- **Repo roots**:
  - `js:` = `/Users/lugo/localdev/viz-web/glyph3d-js` (branch `wall`, HEAD `0db92d5b`, with 12 uncommitted modified files)
  - `rs:` = `/Users/lugo/localdev/glyph3d-rs/.claude/worktrees/work` (branch `work`)
  - `mp:` = `/Users/lugo/localdev/makepad` (branch `work`; symlinked at `rs:../makepad`)
- **Tags**: every claim is tagged `[verified: <path>:<lines>]`, `[measured: <what was run>]`, or `[inferred]`.

---

## 1. What the Wall Is Today: Parts, Data Flow, and Recent/Uncommitted Work

### 1.1 Parts of the JS Wall (`js:wall/`, 49 files)

The wall is an infinite-canvas, keyboard-first terminal and agent-session workspace built on Three.js / React Three Fiber (`js:wall/main.jsx`, `js:wall/TerminalWall.js`) `[verified: js:wall/TerminalWall.js:1-120]`. Its subsystems decompose cleanly into six layers:

1. **Coordinator & Shell Lifecycle**
   - `TerminalWall.js`: owns the terminal map, groups, camera, pointer, keyboard, fullscreen, store, names, bus, agent views, and exposes the global `window.__wall` debug/automation handle `[verified: js:wall/TerminalWall.js:1-250]`.
   - `wallConfig.js` / `wallSettings.js`: palette themes, font family registry (`cousine`, `monaspace-neon`, `monaspace-argon`), font weight/slant/size, cell padding, chrome metrics, snap/grid toggles, scroll physics, and `renderLoop: 'demand' | 'always'` `[verified: js:wall/wallConfig.js:1-160, js:wall/wallSettings.js:1-250]`.
   - `WallStore.js`: per-relay/namespace `localStorage` persistence of camera state, groups (tile/freeform/split trees), terminal geometries/titles/custom names, settings, and pinned agent views `[verified: js:wall/WallStore.js:1-200]`.
   - `wallCommands.js` + `packages/glyph3d-core/src/services/terminal/terminalCommands.js`: single command registry (`wall.*`, `terminal.*`, `camera.*`, `group.*`, `agent.*`) invoked identically by key chords, palette, context menus, titlebar buttons, and programmatic tests `[verified: js:wall/wallCommands.js:1-200, js:packages/glyph3d-core/src/services/terminal/terminalCommands.js:1-200]`.

2. **2D Orthographic World & Camera**
   - `WallCamera.js`: orthographic camera (`x, y, zoom`) with pointer-anchored wheel/pinch zoom, inertia/glide, exponential ease-to-target (`flyTo`, `fitBox`, `fitAll`, `framePane`), screen↔world projection, and an explicit `moving` state used to drive demand rendering `[verified: js:wall/WallCamera.js:1-250]`.
   - `WallPaper.js` + `wallSnap.js`: drafting-paper background grid (adaptive minor/major lines in world space) and snap-to-grid/edge alignment guides `[verified: js:wall/WallPaper.js:1-150]`.

3. **Layout, Groups, Stacking & Fullscreen**
   - `WallGroups.js` + `splitTree.js`: manages three container kinds—**freeform canvas**, **tiled grid** (row/column auto-flow), and **binary split trees** (horizontal/vertical ratios, drag-swap/dock drop zones, directional move/resize, proportional ratio fitting)—plus unified Z-stack ordering (`zStackLayout`) `[verified: js:wall/WallGroups.js:1-774]`.
   - `WallFullscreen.js`: single-pane and whole-group fullscreen (`⌘↵` for group, `⌘⇧↵` for solo zoom with `returnGroup` breadcrumb). Uses Z-elevation (`FULLSCREEN_Z`) plus a dimmed/blurred cutout veil so the WebGL canvas stays live behind the fullscreened group/pane without detaching scenes `[verified: js:wall/WallFullscreen.js:1-800]`.

4. **Input: Unified Pointer, Chords, Terminal Mouse & Selection**
   - `WallPointer.js`: unified pointer state machine arbitrating canvas pan/glide, pane/group drag, edge/corner resize, split-divider drag, drag-swap/dock previews, text selection, double/triple click word/line selection, and pass-through to terminal mouse reporting `[verified: js:wall/WallPointer.js:1-699]`.
   - `WallKeys.js` + `wallKeymap.js` + `chord.js`: intercepts wall chords (e.g. `⌘K` palette, `⌘[`/`]`, `⌃⇧[`/`]`, `⌃,`/`.`, `⌘\` spawn, `⌘↵`/`⌘⇧↵` fullscreen, `⌘⇧+arrows` pane move, zoom/fit chords) before forwarding non-chord keys to the focused terminal via `g3d-termd` `key` ops `[verified: js:wall/WallKeys.js:1-250, js:wall/wallKeymap.js:1-220]`.
   - `WallMouse.js`: translates pointer cell coordinates, buttons, wheel, and modifiers into `g3d-termd` `mouse` ops when the running app enables xterm mouse tracking (1000/1002/1003/1006/1016) or alt-scroll (1007) `[verified: js:wall/WallMouse.js:1-180]`.
   - `SelectionOverlay.js`: visual selection highlight rectangles and clipboard copy `[verified: js:wall/SelectionOverlay.js:1-150]`.

5. **Terminal Rendering (`packages/glyph3d-core/`)**
   - `TerminalEmulator.js` + `TerminalGrid.js`: wraps `@xterm/headless`, queues incoming byte chunks (`TerminalScreenQueue`), reads dirty rows, maintains ring-buffer scroll rows + `_diffLive` / `_writeSpans` partial buffer updates, and drives WebGL2 instanced cell buffers `[verified: js:packages/glyph3d-core/src/TerminalEmulator.js:1-220, js:packages/glyph3d-core/src/TerminalGrid.js:1-800]`.
   - `glyphMaterial.js`: two-pass WebGL2 shader (pass 1: per-cell background fills, cursor, selection; pass 2: Slug vector glyphs + mode-3 cell-clipped vector sprites + underline/double/curly/dotted/dashed + strikethrough + overline + dim/bold/italic + greeking below small pixel-per-row thresholds) `[verified: js:packages/glyph3d-core/src/glyphMaterial.js:1-450]`.
   - `shaping/FontChain.js` + `shaping/cellSprites.js` + `fonts/families.js`: runtime TTF/variable-font Slug curve baking (`Cousine-{Regular,Bold,Italic,BoldItalic}.ttf` 1.1 MB, `MonaspaceNeonVar.ttf` 1.6 MB, `MonaspaceArgonVar.ttf` 1.6 MB, `MesloLGS-NF-Mono.ttf` 2.7 MB, `DejaVuSans.ttf` 742 KB) plus procedural vector cell sprites (`cellSprites.js`: box drawing, block elements, powerline, braille, sextants, quadrants) that clip cleanly to cell edges `[verified: js:packages/glyph3d-core/src/fonts/families.js:1-120, js:packages/glyph3d-core/src/shaping/cellSprites.js:1-480]`.

6. **Chrome, Overlays & Gomuxai Agent Integration**
   - Chrome overlays: `TitleBar.js`, `BarButtons.js`, `WallNames.js`, `MapLabels.js` (zoomed-out readable labels), `WallMenu.js` + `ContextMenu.jsx`, `Palette.jsx` (fuzzy command/session launcher), `Sidebar.jsx` (sessions/groups/agents tree), `StatusWindow.jsx`, `SettingsPanel.jsx`, `HelpPanel.jsx` `[verified: js:wall/TitleBar.js:1-150, js:wall/Palette.jsx:1-120, js:wall/Sidebar.jsx:1-120, js:wall/StatusWindow.jsx:1-120, js:wall/ContextMenu.jsx:1-120, js:wall/HelpPanel.jsx:1-109]`.
   - `WallBus.js` + `WallAgentViews.js` + `wallAgentView.js` + `AgentViewOverlay.jsx`: tracks gomuxai agents across local and remote hosts, pairs each agent terminal 1:1 with its live transcript (`resolveTerminalSession`), and docks/positions live Agent View transcript cards alongside terminals—actively reserving real layout footprint (`footprintChanged`: tile metrics, split `agentInset` + `fitRatios`, canvas `shoveAside`) `[verified: js:wall/WallBus.js:1-220, js:wall/WallAgentViews.js:1-800, js:wall/wallAgentView.js:1-787]`.

---

### 1.2 Data Flow Today

#### Shell bytes → Pixels (JS path: **2 VT emulators + 2 IPC hops**)
1. **PTY → `g3d-termd` (VT emulator #1)**: `g3d-termd` (`js:crates/g3d-termd/src/owner.rs`) reads raw PTY bytes on the session owner thread, feeds them into vendored **`libghostty-vt`** (10 MB scrollback, full mode/screen state), and forwards raw `OUTPUT` (`0x81`) frames—or synthesized VT `REPAINT` (`0x82`) snapshots on attach/resync—over the Unix socket (`g3d-wire` protocol 1.5). **Important**: `g3d-termd` does *not* implement a binary `CELLS` snapshot protocol yet (`0x86` is not in `g3d-wire/src/msg.rs`); clients receive VT byte streams `[verified: js:docs/termd/PROTOCOL.md:1-1334, js:crates/g3d-wire/src/msg.rs:1-800, js:crates/g3d-termd/src/owner.rs:1-800, js:crates/g3d-termd/src/repaint.rs:1-200]`.
2. **`g3d-termd` → Go adapter → Go WebSocket relay → Browser**: `js:cli/attach_unix.go` connects to the termd Unix socket, stamps each `OUTPUT`/`REPAINT` chunk with the session's `(cols, rows)` (`0x01` framed message), and pushes it over WebSocket to the browser's `WebSocketBridge` `[verified: js:cli/attach_unix.go:1-600]`.
3. **Browser VT parse (VT emulator #2)**: `TerminalGrid.writeBytes` feeds bytes into **`@xterm/headless`** on the JS main thread; `TerminalScreenQueue` coalesces writes, `TerminalEmulator.readScreen` extracts visible rows/attributes, and `TerminalGrid.applyScreen` diffs live rows (`_diffLive`) and updates WebGL2 per-cell instance buffers (`_writeSpans`) `[verified: js:packages/glyph3d-core/src/TerminalGrid.js:1-800]`.
4. **GPU draw**: Three.js / r3f renders pass 1 (cell backgrounds/selection/cursor) and pass 2 (Slug vector curves + mode-3 cell sprites + decorations), composited under HTML/DOM chrome overlays (titlebars, palette, sidebar, agent cards) `[verified: js:packages/glyph3d-core/src/glyphMaterial.js:1-450]`.

#### Keypress / Mouse → Shell
1. `WallKeys` captures DOM `keydown` (and paste), checks wall chords first (`wallKeymap`), and if not consumed builds a structured key op (`key` W3C code, `mods`, `utf8`, `unshifted`, `action: press|repeat|release`) `[verified: js:wall/WallKeys.js:1-250]`.
2. `WallPointer` / `WallMouse` translate pointer/wheel events into structured `mouse` ops when mouse tracking or alt-scroll is active `[verified: js:wall/WallPointer.js:1-699, js:wall/WallMouse.js:1-180]`.
3. Ops travel Browser → Go WS relay → Go adapter → `g3d-termd` Unix socket (`key` / `mouse` / `paste` ops, protocol 1.2–1.5).
4. **`g3d-termd/src/keys.rs` encodes the op using `libghostty-vt`** against the terminal's live modes (Kitty keyboard protocol flags, modifyOtherKeys, DECCKM, DECNKM, mouse formats 1000–1016, bracketed paste 2004) and writes the resulting bytes to the PTY `[verified: js:crates/g3d-termd/src/keys.rs:1-250, js:docs/termd/PROTOCOL.md:400-800]`.

#### Gomuxai Agents & Agent Views
- In JS, the Go relay spawns `agent export --json --follow --all` (`js:cli/gomux.go`), polls `agent sessions --json`, and tails individual transcripts via `agent transcript <path> --follow` / `--until` (`js:cli/agentevents.go`), streaming JSON events over the WebSocket to `WallBus` and `WallAgentViews` `[verified: js:cli/gomux.go:1-582, js:cli/agentevents.go:1-800]`.
- **Crucial for the native port**: a native Rust wall needs **zero Go relay**. It speaks `g3d-wire` directly over Unix sockets to `g3d-termd` (including cross-namespace `--ns agents` for local gomuxai agents) and spawns `/Users/lugo/.local/bin/agent` directly—matching `rs:AGENTS.md`'s rule that `agent` CLI is the single reader of agent transcripts `[verified: js:crates/g3d/src/client.rs:1-200, rs:AGENTS.md:85-95]`.

---

### 1.3 Most Recent Changes (Ahead of `docs/architecture/wall.md`) and Uncommitted Work

Seven commits sit ahead of `origin/wall` and are *not yet reflected* in `docs/architecture/wall.md` `[verified: git log -n 15 in js:]`:
- **`c8b5c9a1`**: Focus chords (`⌘[`/`]`, `⌃⇧[`/`]`, `⌃,`/`.`), unified Z-stacking (`zStackLayout`), Z-elevated fullscreen (`FULLSCREEN_Z` + cutout veil keeping canvas live), proportional momentum scrolling (`terminalScroll.js`).
- **`0db92d5b`**: Group fullscreen (`⌘↵` fullscreens whole split/tile/freeform group; `⌘⇧↵` solo-zooms one pane with `returnGroup` return path), `⌘\` spawn chord, viewport-anchored placement.
- **`b293c1d3` & `033b70a4`**: Direct cross-namespace `g3d-termd` attach (`--ns agents`, session `gx-<name>`, `role: "own"`, `flow: "lossy"`, `detach_on_kill`) for local gomuxai agents; remote `name@host` agents continue to attach via `agent attach` over `ssh -t`.
- **`f3a07869`**: Drag-swap and dock targets for split panes and tiles, plus `⌘⇧+arrows` (`wall.pane.move`).
- **`ae49a869` & `467c0a71`**: Agent Views reserve real layout room (`footprintChanged`: tile metrics, split `agentInset` + `fitRatios` + growth/restore, canvas `shoveAside`) and enforce 1:1 session↔terminal pairing (`resolveTerminalSession`).

**Uncommitted in-flight edits (12 modified files in `js:`)** implement three performance changes `[verified: git diff in js:]`:
1. **Leading-edge coalescing timer in Go adapter** (`js:cli/attach_unix.go:496-555`): flushes immediately after a quiet spell (low interactive latency), then coalesces floods up to 16 ms or 16 KiB.
2. **Demand-driven render loop** (`renderLoop: 'demand' | 'always'`, r3f `frameloop="demand"`, `requestFrame`/`invalidate()`, keeping ticks active only while `viewport.moving || pointer.gliding || fullscreen.settling`, `MAX_FRAME_DT = 1/30`) `[verified: js:wall/TerminalWall.js, js:wall/WallCamera.js, js:wall/wallSettings.js]`.
3. **Disable WebGL2 4× MSAA** (`antialias: false` in `js:packages/glyph3d-r3f/src/GlyphCanvas.jsx:38-44`, motivated by measured M2 3024×1964 GPU time dropping from 9.7 ms to 5.8 ms in `js:docs/perf-swarm/wall-text-path-measured-2026-11-12.md`) `[verified: js:docs/perf-swarm/wall-text-path-measured-2026-11-12.md:1-150]`.

---

## 2. What `glyph3d-rs` Has Today to Stand On — and What It Lacks

### 2.1 Standing Infrastructure in `glyph3d-rs`
- **Host-side engine (`glyph3d-native --no-default-features`)**: device-free scene building (`SceneLoad::host()`), `SceneLike::prepare_frame` producing `FrameData` without wgpu/winit/egui, `HyperLayout`, `glyph-scene-graph` transform tree, and `agent_transcript::AgentCli` (wrapping `agent sessions --json` and `agent transcript`) `[verified: rs:AGENTS.md:20-95, rs:native/src/agent_transcript/agent_cli.rs:1-150]`.
- **Makepad host (`rs:makepad/`, standalone Cargo workspace)**:
  - Builds cleanly today against `mp:` (`cargo check --manifest-path rs:makepad/Cargo.toml` succeeded in 1m 06s, zero warnings) `[measured: cargo check]`.
  - Has `G3Renderer` (`rs:makepad/src/g3_passes.rs`, 2,143 lines), `BlockResidency` (`rs:makepad/src/residency.rs`), `EngineHost` (`rs:makepad/src/host.rs`), `G3UiLayer` (`rs:makepad/src/g3_ui.rs`), `SessionPicker` (`rs:makepad/src/sessions.rs`), `DefinitionView` (`rs:makepad/src/definition.rs`), and an unbuilt shell/command-bus architecture spec (`rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md`) `[verified: rs:makepad/src/main.rs:1-180, rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md:1-200]`.
  - **Workspace isolation**: `makepad/` is *excluded* from root `rs:Cargo.toml` and `rs:build.toml` gates `[verified: rs:Cargo.toml:1-40, rs:build.toml:1-150]`. New wall code in `makepad/` (and new dedicated crates) will not perturb `glyph3d-native` pixel-ab goldens unless root renderer defaults or shaders are modified.
- **Configuration**: `rs:config/defaults.toml` + `launch_config.toml` override pattern (`rs:native/src/launch_config.rs`) `[verified: rs:config/defaults.toml:1-120, rs:native/src/launch_config.rs:1-120]`.
- **Local toolchain & daemons already installed**: `rustc 1.98.0` (exceeds `js:`'s `rust-version = "1.96"` and supports Rust 2024 edition), plus `~/.local/bin/{g3d, g3d-termd, agent, agent-busd}` already present on this host `[measured: rustc --version; ls ~/.local/bin]`.

### 2.2 Concrete Gaps in `glyph3d-rs` (by Subsystem)

| Area | What `glyph3d-rs` has today | What is missing for the Wall |
|---|---|---|
| **Renderer** (`rs:makepad/src/g3_passes.rs`, `DerivedSlot`) | 20 B `DerivedSlot` (group + line/col + glyph + packed fg), group-level `gbg` fill, linear RGBA16F→sRGB composite, Slug curve pass `[verified: rs:crates/glyph-field-derived/src/slot.rs:1-100, rs:makepad/src/g3_passes.rs:700-950]` | No per-cell background fill pass; no per-cell SGR style/underline (5 styles + underline color)/strikethrough/overline/dim; no mode-3 cell-clipped vector sprites; **broken variable-font winding rule** (`g3_passes.rs:841` lacks `abs(coverage)`, which clips/inverts overlapping contours in variable fonts); linear-space blending diverges from sRGB terminal blending; **continuous spin loop** (`rs:makepad/src/viewport.rs:113-117` calls `new_next_frame` + `redraw` unconditionally every frame) `[verified: rs:makepad/src/g3_passes.rs:835-855, rs:makepad/src/viewport.rs:105-125]`. |
| **Fonts & Glyph Atlas** | Static immutable prebaked atlas (`ATLAS_GENERATION = 1`, `rs:assets/atlas/FORMAT.md`): **Cousine Regular only** `[verified: rs:assets/atlas/FORMAT.md:1-100]` | No Bold / Italic / Bold-Italic faces; no Monaspace Neon / Argon variable fonts (`wght`/`slnt`); no runtime glyph baking / fallback chain (CJK, Nerd Font icons, symbols, color emoji); no combining-mark clusters; no live font-family/size switching; no procedural cell sprites. |
| **Camera** (`rs:native/src/glyph_scene/camera.rs`) | 3D perspective camera (`Front`, `Orbit`, `Fly`) `[verified: rs:native/src/glyph_scene/camera.rs:1-180]` | No 2D orthographic wall camera (`x, y, zoom`), no pointer-anchored zoom, no inertia glide, no eased `fitBox`/`framePane`, no screen↔world cell picking for 2D panes. |
| **Live-Updating Text** (`HostSlots`, `SceneLoad`) | 5.2 MB immutable/CoW slot blocks (`262,144` slots/block) built for static/repo loads; fixed item table where adding/removing items rebuilds the whole scene (`relayout`) `[verified: rs:crates/glyph-field/src/host_slots.rs:1-120, rs:native/src/glyph_scene/frame.rs:1-150]` | No per-pane incremental row/cell updates without re-uploading megabytes; no dynamic add/remove/resize of panes without scene rebuild; zero `g3d-wire` / `g3d-termd` client code in `glyph3d-rs`. |
| **Input** (`rs:makepad/src/input.rs`) | Maps only 25 `SceneKey` variants for camera/nav `[verified: rs:makepad/src/input.rs:1-140]` | No chord dispatcher (`wallKeymap`), no terminal key encoding or `g3d-termd` `key`/`mouse`/`paste` forwarding, no unified pointer state machine (pan/drag/resize/swap/select/mouse-report), no IME positioning for terminals. |
| **UI & Chrome** (`rs:makepad/src/g3_ui.rs`) | Minimal HUD/session picker/definition panel `[verified: rs:makepad/src/g3_ui.rs:1-250, rs:makepad/src/sessions.rs:1-150]` | No pane titlebars/buttons, map labels, drafting grid/snap guides, command palette, context menu, collapsible sidebar, status window, settings/help panels, Z-elevated fullscreen cutout veil, or paired Agent View transcript cards with layout-room reservation. |
| **Commands** | `rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md` specifies a unified command bus/console/palette, but it is **unimplemented** `[verified: rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md:1-120]` | No live command registry, argument schema, keybinding table, or programmatic command dispatch. |
| **Persistence** | Read-only `launch_config.toml` + `definition-engine` JSON store `[verified: rs:native/src/launch_config.rs:1-120]` | No per-namespace wall state persistence (camera, groups, split trees, pane geometry/names, settings, pinned agent views). |

---

## 3. What `makepad` Itself Offers That Matters Here

Inspecting `mp:` uncovered major native capabilities that dramatically shrink the port—alongside critical HAL input traps that must be handled early.

### 3.1 Built-In Dynamic Slug Font Service + Variable Font Support
- `mp:platform/hal/src/font/{service.rs, atlas.rs, pool.rs}`, `mp:draw/src/text/{fonts.rs, rasterizer_hal.rs}`, `mp:draw/src/shader/draw_text.rs`, `mp:libs/font_core/src/slug.rs`, and `mp:platform/script/resources/glyph_coverage.splash` implement a **complete host-owned, multithreaded (`hal-font-0..N`), LRU-evictable Slug GPU font engine** `[verified: mp:platform/hal/src/font/service.rs:1-200, mp:platform/hal/src/font/pool.rs:1-200, mp:draw/src/text/fonts.rs:1-300]`.
- **Slug curve texture**: `2048 × 128` RGBA32F (131,072 curves, 4 MiB) with GPU-fence retirement, paired with a multi-plane BGRA8 raster atlas for color emoji / SDF / MSDF `[verified: mp:platform/hal/src/font/atlas.rs:1-200, mp:draw/src/text/slug_atlas.rs:1-150]`.
- **Variable fonts work out of the box**: `SlugCoverage::calc_coverage` **already uses `abs(...)`** (`mp:platform/script/resources/glyph_coverage.splash:272-281`), and `FontMember { res, weight, axes, lazy, system, italic }` passes variation axes (`wght`, `slnt`, etc.) through `ttf-parser` + `rustybuzz` `[verified: mp:platform/script/resources/glyph_coverage.splash:270-290, mp:draw/src/text/fonts.rs:1-200]`.
- **`DrawGlyph`** (`mp:draw/src/shader/draw_glyph.rs:1-250`) also renders arbitrary banded Slug vector paths, and `DrawText` supports pre-shaped cached `PreparedGlyph` (`draw_prepared_glyph_abs`) `[verified: mp:draw/src/shader/draw_glyph.rs:1-200, mp:apps/terminal/src/widget.rs:1741-1754]`.
- **Gaps in `DrawText`**: no built-in greeking below ~8 px/row (must be done at draw-list/cell-shader level when zoomed far out), and no mode-3 cell-edge-clipped vector sprite path (box drawing/powerline need procedural quads or a custom cell-sprite shader).

### 3.2 Retained `DrawList2d` + GPU `view_transform`
- Every `DrawList2d` carries a 4×4 `view_transform` uniform (`mp:draw/src/draw_list_2d.rs:35-150`), read directly by `slug_dilate` (`camera_projection * camera_view * view_transform`), and `begin_maybe(cx, false)` skips re-recording (`Redrawing::no()`) when a list is not invalidated `[verified: mp:draw/src/draw_list_2d.rs:35-180, mp:draw/src/shader/draw_text.rs:1-250]`.
- **Architectural consequence**: Each terminal pane can own its own retained `DrawList2d` in local pane coordinates. Panning, zooming, dragging, or Z-reordering updates **one `view_transform` matrix per pane** with zero CPU re-recording of unchanged terminals! Only terminals whose VT grid actually changed re-record their `DrawList2d`.

### 3.3 Demand-Driven Event Loop & No-MSAA Default
- Makepad HAL's event loop (`mp:platform/src/os/hal/event_loop.rs:4-9, 190-205, 334-343`) **already sleeps indefinitely (`timeout = u64::MAX`) when idle** and wakes only on OS input, `SignalToUI` / `ToUISender`, timers, or `NextFrame` (and AppKit pauses the `CVDisplayLink` after `LINK_IDLE = 4` idle callbacks in `mp:platform/hal/src/backend/apple/appkit/app.rs:43`) `[verified: mp:platform/src/os/hal/event_loop.rs:185-210, mp:platform/hal/src/backend/apple/appkit/app.rs:38-45]`. Only `rs:makepad/src/viewport.rs:113-117` breaks this by unconditionally requesting `new_next_frame()`.
- **MSAA is already off by default**: Metal hardcodes `msaa: 1` (`mp:platform/hal/src/backend/apple/metal/mod.rs:165`), and Vulkan defaults to `samples = 1` `[verified: mp:platform/hal/src/backend/apple/metal/mod.rs:165]`.

### 3.4 Complete Pure-Rust Ghostty-Port VT Core Already Inside Makepad (`mp:apps/terminal/`)
- `mp:apps/terminal/src/term/` (16 modules, zero Makepad/PTY/IO dependencies) is a **clean MIT-licensed Rust port of Ghostty's terminal subsystem**: DFA UTF-8, vt100.net parser, OSC, SGR (incl. colon subparams & 5 underline styles + underline color), 41 DEC/ANSI modes (incl. 2004 bracketed paste, 2026 synchronized output, 2027 grapheme clusters via `makepad-cell-width`, 2048 in-band size), wide chars, reflowing scrollback with `ScrollbackSink`, hyperlinks, and 88 KB `key_encode.rs` / `mouse_encode.rs` `[verified: mp:apps/terminal/src/term/mod.rs:1-49, mp:apps/terminal/src/term/modes.rs:1-155, mp:apps/terminal/src/term/page.rs:1-249, mp:apps/terminal/src/term/screen.rs:1-250, mp:apps/terminal/src/term/style.rs:1-152]`.
- `mp:apps/terminal/src/widget.rs` (`MpTerm`, 2,267 lines) demonstrates Makepad's exact terminal rendering idioms: horizontal bg-run merging, procedural box-drawing/braille, 5-style underline shader, block/bar/underline cursor, `PreparedGlyph` cache, `Event::Signal` wakeup from a background reader thread, and **transformed canvas IME anchoring (`canvas_ime_anchor`)** `[verified: mp:apps/terminal/src/widget.rs:1600-1927]`.
- **Caveats in `MpTerm` itself**: it only caches bold (no italic/faint face selection), ignores `Alt+key` and `KeyUp`, and suffers from Makepad HAL's `Ctrl+C`/`Ctrl+V` bugs (below). We should reuse/extract `term/` (pure VT core) and adapt rendering idioms—not drop `MpTerm` in unmodified.

### 3.5 Widgets, Storage, Trackpad Gestures & `--remote` Automation Harness
- **Gestures & mouse buttons**: All 5 mouse buttons (`1/2/4/8/16`), macOS trackpad `NSEventTypeMagnify` pinch (`Begin/Update/End`), Wayland `zwp_pointer_gesture_pinch_v1`, and macOS `NSScrollWheel` with `hasPreciseScrollingDeltas` + full `ScrollPhase` (`Began/Changed/Ended/Momentum/MomentumEnded`) are already wired end-to-end `[verified: mp:platform/hal/src/backend/apple/appkit/app.rs:554-606, mp:platform/hal/src/backend/apple/appkit/keys.rs:34-44, mp:platform/hal/src/backend/linux/wayland/input.rs:954-980]`.
- **Widgets**: `Dock`/`Splitter`, `PopupMenu`, `OverlayLayers`, `PortalList`, `TextInput`, `Modal`, `Popover` exist in `mp:widgets/` `[verified: mp:widgets/core/src/overlay_layers.rs:1-113, mp:widgets/core/src/popup_menu.rs:1-120]`.
- **Persistence**: `Cx::storage` (`mp:platform/src/storage.rs`) offers an async namespace-bound atomic KV store on disk (`~/.mpkv`), though synchronous JSON/TOML files in an XDG/Application Support wall directory are simpler for deterministic startup hydration `[verified: mp:platform/src/storage.rs:1-200]`.
- **Automation & headless GPU verification**: `mp:platform/src/remote.rs` (`--remote` / `MAKEPAD_REMOTE=1`), `MAKEPAD_NO_FOCUS`, `MAKEPAD_HIDE_WINDOWS`, and `open_flags::OFFSCREEN_WINDOWS` allow an unattended run to launch the real GPU app without stealing focus, inject real mouse/key/scroll/pinch events, run custom `register_app_query` inspection commands, and grab real rendered frames over Unix socket `[verified: mp:platform/src/remote.rs:1-120, mp:platform/hal/src/backend/apple/appkit/window.rs:281,1260-1282, mp:platform/hal/src/backend/apple/metal/mod.rs:8]`.

### 3.6 Critical Makepad HAL Input Traps (Verified in Source)
1. **`Ctrl+V` paste hijack on macOS**: `mp:platform/hal/src/backend/apple/appkit/app.rs:530-537` checks `(mods & (MOD_LOGO | MOD_CONTROL)) != 0 && key == KeyV` and injects `win.paste(pasteboard)` **ahead of** `KeyDown(KeyV, control)`. Pressing `Ctrl+V` in vim/bash/emacs on macOS pastes the clipboard *and* sends `^V` `[verified: mp:platform/hal/src/backend/apple/appkit/app.rs:530-537]`.
2. **`Ctrl+C` / `Ctrl+X` clipboard hijack on macOS**: `mp:platform/src/event/hal_intake.rs:361-367` fires `Event::TextCopy` / `Event::TextCut` whenever `modifiers.logo || modifiers.control` with `KeyC` / `KeyX`—on macOS, `Ctrl+C` must be SIGINT, not clipboard copy `[verified: mp:platform/src/event/hal_intake.rs:361-367]`.
3. **Only 4 modifier bits**: `MOD_SHIFT`, `MOD_CONTROL`, `MOD_ALT`, `MOD_LOGO`. No `CapsLock`, `NumLock`, `AltGraph`, or left/right modifier distinction (`LAlt`/`RAlt` are commented out in `mp:platform/hal/src/backend/apple/appkit/keys.rs:108-116`) `[verified: mp:platform/hal/src/backend/apple/appkit/keys.rs:9-24,108-116, mp:platform/hal/src/backend/linux/input.rs:127-141]`.
4. **IME gating & `TextInput` ordering on macOS**: `mp:platform/hal/src/backend/apple/appkit/classes.rs:666-681` only passes `keyDown:` to `NSTextInputContext` when `win.ime_active.get()` is true (so without `cx.show_text_ime(...)`, no `TextInput` fires at all!). When IME *is* active, `sendEvent:` queues `HalEvent::Text` *before* `app.rs:537` queues `HalEvent::KeyDown`, and `Option+letter` produces macOS composed text (e.g., `Option+b` → `∫`) rather than uncomposed `b` + `Alt` `[verified: mp:platform/hal/src/backend/apple/appkit/classes.rs:666-681, mp:platform/hal/src/backend/apple/appkit/app.rs:505-538]`.

---

## 4. Real Design Choices: Options, Costs, and Recommendations

### Choice A: Wall Rendering Architecture
| Option | Description | Pros | Costs / Risks |
|---|---|---|---|
| **A1. Extend `G3Renderer` / `DerivedField`** (`rs:makepad/src/g3_passes.rs`) | Add per-cell bg, SGR flags, cell sprites, dynamic atlas, and 2D ortho camera to `glyph3d-native` + `g3_passes.rs`. | Single rendering stack for 3D repos and 2D wall. | Massive refactor of `DerivedSlot` (20 B tightly packed), static atlas format, and `HostSlots` 5.2 MB blocks; risks breaking `glyph3d-native` golden pixel-ab gates; duplicates Makepad's dynamic HAL Slug engine and variable-font support; still has linear vs sRGB blend mismatch. |
| **A2. Makepad-native 2D Wall Renderer (Retained per-pane `DrawList2d` + HAL Slug + custom cell/sprite shaders)** *(Recommended)* | Keep `G3Renderer` untouched for 3D repo/carrel/definition modes; build the Wall as a dedicated 2D scene/widget in `rs:makepad/` using one retained `DrawList2d` per terminal (`set_view_transform`), HAL `FontService` Slug for glyphs (Cousine + Monaspace Neon/Argon + system fallback + emoji), plus dedicated Makepad shaders for cell bg runs, cursors, decorations, drafting grid, and procedural cell sprites. | Zero risk to `glyph3d-native` goldens; immediate variable fonts (`abs(coverage)` already works), emoji, CJK, dynamic glyph eviction, sRGB blending, demand rendering, and zero-re-record pan/zoom via `view_transform`. | Two renderers in `glyph3d-makepad` (3D repo/definition vs 2D wall), though they share window, chrome, command bus, and HAL. Need to add greeking LOD (draw solid ink-bar quads when `cell_h * zoom < ~7px`) and port `cellSprites.js` geometry. |

**Recommendation: A2.** Extending `DerivedField` for live multi-font SGR terminals fights its static-repo design at every layer while throwing away Makepad's already-working dynamic Slug + variable-font + retained `DrawList2d` engine `[inferred from verified renderer code]`.

### Choice B: VT State Synchronization — Client-Side Rust VT vs `g3d-termd` Protocol 1.6 `CELLS`
| Option | Description | Pros | Costs / Risks |
|---|---|---|---|
| **B1. Background-thread pure-Rust VT parser speaking existing termd 1.5 (`OUTPUT`/`REPAINT`)** *(Recommended for Phase 1)* | Connect directly to `g3d-termd` Unix sockets via `g3d-wire` (protocol 1.5); run a pure-Rust VT emulator (extracted `mp:apps/terminal/src/term/` Ghostty port) on a dedicated I/O+parse worker thread per terminal (or thread pool) and publish immutable/double-buffered dirty row snapshots to the UI thread via `SignalToUI`. | **Zero changes to `g3d-termd`**; immediately works with existing `~/.local/bin/g3d-termd` and live `--ns agents` sessions; keeps UI thread completely off VT parsing; Ghostty-port Rust VT matches `libghostty-vt` far more closely than `@xterm/headless`; gives instant client-side scrollback without round-trips. | Still runs two VT emulators (daemon `libghostty-vt` + client Rust Ghostty port); subtle edge-case divergence possible on esoteric CSI/reflow/REPAINT sequences; per-pane memory for client scrollback (keep modest, e.g. 2k–5k lines locally, or sync on scroll). |
| **B2. Add `CELLS` (`0x86`) binary grid stream to `g3d-termd` (protocol 1.6)** *(Recommended as Phase 2 optimization)* | Daemon serializes dirty `libghostty-vt` rows directly (codepoints/clusters + resolved SGR + cursor + modes). | True single VT emulator; zero parse divergence; lowest CPU under extreme flood; smaller client memory. | Requires editing `js:crates/g3d-termd` + `g3d-wire`, rebuilding Zig+`libghostty-vt`, and restarting/upgrading running daemons (including `agents` ns). |

**Recommendation: B1 first, design the wall's `TerminalModel` trait/snapshot boundary so B2 plugs in cleanly later.** This keeps the unattended run strictly inside `glyph3d-rs` (and read-only on live daemons) while already eliminating Go relay + WebSocket + JS/@xterm overhead `[inferred]`.

### Choice C: Sharing `g3d-wire` / `g3d` Client & Font Assets Across Repos
- **Problem**: `g3d-wire` and `g3d` live in `js:crates/` (Rust edition 2024, `rust-version = "1.96"`), NOT in `rs:` (Rust edition 2021). And `MonaspaceNeonVar.ttf` / `MonaspaceArgonVar.ttf` / `Cousine-*.ttf` live in `js:packages/glyph3d-core/assets/fonts/` (whereas `rs:` only has prebaked Cousine Regular in `assets/atlas/`).
- **Options**:
  1. **Local path dep to `js:crates/g3d-wire`** (fragile across machines/worktrees; couples two repos).
  2. **Copy/vendor pure-Rust `g3d-wire` protocol crate (or extract a small `crates/g3d-client` in `rs:` speaking the exact frozen 1.5 wire spec `js:docs/termd/PROTOCOL.md`)** + vendor open-licensed fonts (`Cousine`, `Monaspace Neon/Argon` under OFL/Apache-2.0) or fall back to system/bundled fonts.
- **Recommendation**: Create a small pure-Rust `crates/g3d-client` (wire codec + Unix socket client + `start`/`attach`/`list`/`new`/`kill`/`resize`/`key`/`mouse`/`paste` matching `js:docs/termd/PROTOCOL.md` 1.5, zero Zig/`libghostty` deps) inside `rs:`—or move `g3d-wire` into a shared repo/submodule once stabilized. Verify wire compatibility with integration tests against `~/.local/bin/g3d-termd` on a scratch namespace.

### Choice D: Chrome & Agent Views — Native Makepad Widgets vs World-Space `DrawList2d`
- **World-space chrome** (terminal frames, focus glow, resize handles, titlebars, action buttons, zoomed-out map labels, drag-swap drop overlays, Agent View transcript cards): render in Makepad's 2D canvas with `DrawList2d` (or screen-space projected overlays driven by `WallCamera::world_to_screen`), so they move in lockstep with the canvas with zero 1-frame DOM lag.
- **Screen-space chrome** (command palette, context menu, collapsible sidebar, status window, settings/help modals, fullscreen blur/dim veil): build with Makepad widgets (`OverlayLayers`, `PortalList`, `TextInput`, `PopupMenu`, `Modal`) and back every action by the unified Command Bus (`rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md`).
- **Agent View rendering**: Reuse `rs:native/src/agent_transcript/`'s `AgentCli` (extended with `--follow` / `agent export --json --follow --all` matching `js:cli/gomux.go` and `js:cli/agentevents.go`) and render transcript cards using Makepad's `DrawText` + `PortalList` (or retained `DrawList2d` cards) while preserving `WallAgentViews`'s layout-room reservation (`footprintChanged`, `agentInset`, `fitRatios`, `shoveAside`).

### Choice E: "Make the Wall the Default" Without Breaking `cargo glyph test`
- **Standing direction**: New UI/interaction goes in `rs:makepad/` (`glyph3d-makepad`).
- **Critical constraint**: `rs:build.toml` gates run `glyph3d-native` (headless/wgpu golden renders, `pick-oracle`, `hyper-oracle`). Changing `glyph3d-native`'s CLI defaults risks breaking existing gates/tests `[verified: rs:build.toml:1-300]`.
- **Recommendation**:
  1. Make **the Wall the default mode of `glyph3d-makepad`** when invoked with no scene flags (`--load-repo`, `--render-file`, `--agent-session`, `--definition-run` continue to open their respective 3D/definition views, or can open as panels/modes inside the app).
  2. Add `--wall [--ns <name>]` explicit flag, and update `cargo glyph run` (or document/add launcher entry) only after confirming zero gate regressions on `cargo glyph test`.

### Choice F: Fixing Makepad HAL Input Traps vs Workarounds in `glyph3d-makepad`
- **Options**:
  1. Work around in `glyph3d-makepad` alone: **impossible to cleanly fix `Ctrl+V`** because `app.rs:530` unconditionally reads NSPasteboard and injects a `TextInput { was_paste: true }` event before `KeyDown(KeyV, control)` (indistinguishable from `Cmd+V` unless you track live modifier state and suppress `was_paste` when `control && !logo`!). Similarly, `hal_intake.rs:361` unconditionally emits `TextCopy`/`TextCut` on `Ctrl+C`/`Ctrl+X`.
  2. **Fix the 2 macOS modifier guards directly in `mp:`** (gate `Ctrl+V` / `Ctrl+C` / `Ctrl+X` clipboard chords to `MOD_LOGO` on `target_os = "macos"` and `MOD_CONTROL` on non-macOS, or make clipboard-chord synthesis opt-out per focused widget; optionally surface `charactersIgnoringModifiers` / `Option`-bypass for terminal focus).
- **Recommendation**: Fix the macOS `Ctrl+C`/`X`/`V` chord bug cleanly in `mp:` (a ~15-line surgical change that also fixes `mp:apps/terminal`!), AND add a defensive guard in the wall's input handler so `KeyDown` + `TextInput` never double-fire.

---

## 5. Build Order & De-Risking Spikes

### Phase 0: Four Small Spikes (Prove Assumptions Before Building On Them)

1. **Spike 1 — HAL Keyboard, Option/Ctrl/Cmd & Gesture Fidelity** *(highest risk)*
   - Build a tiny diagnostic view in `rs:makepad/` logging every `KeyDown`, `KeyUp`, `TextInput`, `TextCopy`, `Scroll`, `Pinch`, and 5-button mouse event on macOS (and Linux if available).
   - Verify: (a) `Ctrl+C`, `Ctrl+V`, `Ctrl+X`, `Cmd+C`/`V`, `Option+b`/`f`, `Cmd+[`/`]`, `Cmd+Enter`, `Cmd+Shift+Enter`, `Cmd+Shift+Arrows`, `Ctrl+Shift+[`; (b) exact ordering of `TextInput` vs `KeyDown` with `show_text_ime` on vs off; (c) mapping Makepad `KeyCode` + modifiers + text to `g3d-termd` `key` ops and verifying Kitty/readline/vim/tmux behavior against a scratch `g3d-termd --ns wall-spike`.
2. **Spike 2 — Retained `DrawList2d` + HAL Slug Variable Fonts + `view_transform` + Cell Sprites + Greeking**
   - Render 12 synthetic 120×40 terminals (12 retained `DrawList2d`s) with `MonaspaceNeonVar.ttf` (regular, bold `wght=700`, italic `slnt=-11`), Cousine, box-drawing/powerline cell sprites, and 5 underline styles.
   - Measure: pan/zoom frame time (only updating `view_transform`, zero re-record), zoom-out legibility & greeking transition (<7 px/row), seam alignment of box-drawing/powerline across arbitrary fractional zoom levels, and idle CPU/GPU (~0% when stopped).
3. **Spike 3 — Direct `g3d-termd` Socket Client + Extracted `term/` VT Worker**
   - Connect directly to a scratch `g3d-termd` Unix socket (`g3d-wire` 1.5), run `mp:apps/terminal/src/term/` on a worker thread, wake Makepad via `SignalToUI`, and blast `yes` / `seq 1 500000` / `htop` / `nvim` / `claude`-like TUI redraws.
   - Verify: `REPAINT` replay fidelity, mode 2026 synchronized output (no tearing), leading-edge coalescing latency (<1 frame for typing, bounded frame rate under flood), and `resize` reflow parity.
4. **Spike 4 — Unattended Verification Harness (`--remote` + `OFFSCREEN_WINDOWS` + Scratch `g3d-termd`)**
   - Confirm `glyph3d-makepad --remote` with `MAKEPAD_NO_FOCUS=1` / `MAKEPAD_HIDE_WINDOWS=1` launches cleanly without stealing focus, accepts `makepad-remote` input/queries, captures deterministic PNGs, and cleanly stops its scratch `g3d-termd` namespace on exit.

### Phased Build Order (Vertical Slices)

1. **Slice 1: Core Architecture + Single Live Terminal on an Ortho Canvas**
   - Pure-Rust `g3d-client` (wire 1.5 + Unix socket + daemon auto-start), `WallCamera` (2D ortho, pointer-anchored zoom, wheel/pinch, ease-to, demand-driven tick), `WallPaper` drafting grid, retained `DrawList2d` terminal pane renderer (bg runs, HAL Slug text, decorations, cell sprites, cursor, greeking), worker-thread VT core (`term/`), and verified key/mouse/paste/selection routing.
   - Make Wall the default mode in `glyph3d-makepad` (preserving existing CLI flags for 3D/definition views).
2. **Slice 2: Command Bus, Multi-Pane Layout (`WallGroups` + `splitTree`), Pointer State Machine & Persistence**
   - Port `wallCommands` + `wallKeymap` + `WallPointer` (pan, drag, resize, split-divider drag, drag-swap/dock, `⌘⇧+arrows`, Z-stacking `zStackLayout`, snap guides), `TitleBar` / `BarButtons` / `WallNames` / `MapLabels`, and per-namespace JSON `WallStore`.
3. **Slice 3: Fullscreen (Pane & Group), Chrome Overlays & Settings**
   - Port `WallFullscreen` (Z-elevated group + solo fullscreen with `returnGroup` and blurred/dimmed cutout veil), `Palette`, `ContextMenu`, `Sidebar`, `StatusWindow`, `SettingsPanel`, `HelpPanel`, live font/theme switching.
4. **Slice 4: Gomuxai Bus & Paired Agent Views**
   - Spawn `agent export --json --follow --all`, `agent sessions --json`, and `agent transcript --follow`; direct cross-ns `--ns agents` termd attach (`gx-<name>`, `flow: "lossy"`, `detach_on_kill`) + SSH fallback for remote `name@host`; paired Agent View transcript cards with full layout-room reservation (`footprintChanged`, `agentInset`, `fitRatios`, `shoveAside`).
5. **Slice 5 (Optional / Follow-up): `g3d-termd` Protocol 1.6 `CELLS` Mode**
   - Only if profiling Slice 1–4 shows client-side Rust VT parsing is a meaningful bottleneck under many simultaneous floods.

---

## 6. Performance: What Changes Natively vs What Carries Over from JS

### What Changes Dramatically Natively
1. **IPC hops & serialization collapse**: Eliminates Go `attach_unix.go` + Go WebSocket relay + JSON/WS framing + Browser main-thread GC + `@xterm/headless` JS execution. Bytes go `g3d-termd` Unix socket → native Rust worker thread directly `[verified: js:cli/attach_unix.go, js:packages/glyph3d-core/src/TerminalGrid.js]`.
2. **Off-main-thread VT parsing**: `@xterm/headless` ran on the JS main thread and competed directly with r3f input/rendering. In Rust, each terminal's socket read + `term/` VT parse runs on background worker threads; the UI thread only swaps compact dirty-row snapshots on `SignalToUI` `[inferred]`.
3. **Retained `DrawList2d` + `view_transform`**: Panning, zooming, and dragging 20+ terminals becomes O(number of panes) matrix updates instead of JS scene traversal + React reconciliation + DOM titlebar layout thrash `[verified: mp:draw/src/draw_list_2d.rs:35-150]`.
4. **Multithreaded GPU-fenced Slug atlas**: Makepad's HAL `hal-font-0..N` worker pool rasterizes/bakes missing glyphs off the UI thread without stalling frames, and supports variable fonts natively without JS TTF parsing overhead `[verified: mp:platform/hal/src/font/pool.rs:1-150]`.
5. **No WebGL2 MSAA penalty by default**: Metal/Vulkan HAL already render at `msaa: 1` (unlike WebGL2's default 4× MSAA that cost 3.9 ms/frame on M2 in `js:docs/perf-swarm/wall-text-path-measured-2026-11-12.md`) `[verified: mp:platform/hal/src/backend/apple/metal/mod.rs:165]`.

### Which JS-Side Performance Changes Still Carry Over (Must Keep!)
1. **Demand-driven rendering (`frameloop="demand"`)**: **Essential.** `rs:makepad/src/viewport.rs:113-117` currently spins every vsync! The wall must *only* request `NextFrame` while `camera.moving || pointer.gliding || fullscreen.settling || cursor_blink`, and otherwise rely on Makepad HAL's native `timeout = u64::MAX` sleep + `SignalToUI` wakeup `[verified: rs:makepad/src/viewport.rs:113-117, mp:platform/src/os/hal/event_loop.rs:190-205]`.
2. **Leading-edge output coalescing (`js:cli/attach_unix.go:496-555`)**: **Essential.** Without it, a flooding terminal (`yes`, build logs) floods `SignalToUI` and forces 120 Hz full `DrawList2d` re-records. Keep the exact leading-edge timer (immediate wake after quiet spell; 8–16 ms / 16 KiB batching + budget-capped drain during floods, plus honor DEC mode 2026 synchronized output so TUI frames publish atomically).
3. **Incremental per-pane / per-row dirty tracking**: Only re-record the `DrawList2d` of terminals whose generation counter incremented (and merge adjacent identical-bg cells into horizontal runs as `MpTerm` already does at `mp:apps/terminal/src/widget.rs:1580-1645`).
4. **Zoomed-out greeking LOD**: Even with native Slug, evaluating cubic/quadratic Bezier curves for 20 panes × 4,800 cells when each glyph is 2×4 pixels wastes fragment bandwidth and shimmers. Switching sub-threshold panes (`cell_h_px < ~7`) to aggregated ink-bar quads (matching `glyphMaterial.js`'s greeking) preserves crispness and low GPU cost at overview zoom `[inferred from verified js:packages/glyph3d-core/src/glyphMaterial.js]`.
5. **`MAX_FRAME_DT` clamp & proportional momentum scroll**: Clamp animation dt (e.g. `1/30 s`) so waking from demand sleep never causes a huge animation jump.

---

## 7. What Could Stop an Unattended Implementation Run Partway

1. **Makepad HAL macOS keyboard traps (`Ctrl+V` paste, `Ctrl+C`/`X` copy, `Option+key` composition, missing `CapsLock`/`NumLock`)**: If not discovered and fixed in Spike 1, an unattended run will build terminal input that silently breaks `Ctrl+C`, `Ctrl+V`, `Option+b/f`, and Kitty key release—and unit tests on pure key tables won't catch it without live HAL/remote testing `[verified: mp:platform/hal/src/backend/apple/appkit/app.rs:530-537, mp:platform/src/event/hal_intake.rs:361-367, mp:platform/hal/src/backend/apple/appkit/classes.rs:666-681]`.
2. **Cross-repo write boundary**: `g3d-wire` / `g3d-termd` live in `glyph3d-js`, and the macOS `Ctrl+C`/`V` HAL fix lives in `makepad`. If an unattended run is scoped to `glyph3d-rs` only (cannot edit `makepad` or `glyph3d-js`), it will hit a wall unless the boundary is decided upfront (specifically: allowed to patch `makepad` HAL input, and vendoring/rewriting a pure-Rust `g3d-client` inside `glyph3d-rs` rather than depending on `glyph3d-js` paths) `[verified: repo layouts]`.
3. **Font asset availability & licensing**: `rs:` currently ships **zero** `.ttf` files (only prebaked `assets/atlas/` Cousine Regular). Monaspace Neon/Argon and Cousine Bold/Italic must either be copied into `rs:makepad/assets/fonts/` (check OFL/Apache-2.0 licenses in `js:packages/glyph3d-core/assets/fonts/`) or loaded from system/config paths; missing font files at runtime will break variable-font styling or bold/italic `[verified: rs:assets/atlas/FORMAT.md, js:packages/glyph3d-core/src/fonts/families.js]`.
4. **No automated gate coverage for `rs:makepad/` today**: `cargo glyph test` does **not** build or test `rs:makepad/` (`build.toml` excludes it). Without establishing a dedicated verification loop in Spike 4 (`cargo test --manifest-path makepad/Cargo.toml` + `--remote` smoke/screenshot script against a **scratch** `g3d-termd --ns <scratch>` namespace), an unattended run has no pixel/interaction feedback loop `[verified: rs:build.toml, rs:Cargo.toml]`.
5. **Live `g3d-termd` namespace safety**: Running tests against default namespaces (`wall`, `agents`, `default`) can kill or resize Ivan's live terminals/agents. Every test and dev launch in an unattended run MUST force an isolated scratch `--ns` (and temp `XDG_RUNTIME_DIR` or explicit socket) and stop it on teardown `[verified: js:crates/g3d/src/paths.rs]`.
6. **`publication_accounting().envelope == 0` fatal exit in `rs:makepad/src/main.rs:127-134`**: `glyph3d-makepad` currently exits if the backend reports `envelope == 0` (e.g., some headless/software/VM backends). The Wall renderer doesn't even use `BlockResidency` for terminals, so that guard must only apply to `G3Renderer` 3D scenes `[verified: rs:makepad/src/main.rs:127-134, mp:platform/src/os/hal/event_loop.rs:102-115]`.
7. **Build/iteration time on `rs:makepad/`**: A clean `cargo check` took 66 s on this M2 `[measured: task-162]`; linking `glyph3d-makepad` in release with `makepad-widgets` + `glyph3d-native` is heavier unless incremental dev builds or `relax` live-JIT are used. Put pure logic (layout/groups/splitTree/camera/commands/store/g3d-client/VT) in small workspace crates with fast `cargo test` loops (<2 s), keeping `makepad/` thin `[inferred + measured]`.
8. **Subtle VT / `REPAINT` divergence between `mp:apps/terminal/src/term/` and `g3d-termd`'s `libghostty-vt`**: `g3d-termd` emits `REPAINT` streams generated by `libghostty-vt` (`js:crates/g3d-termd/src/repaint.rs`) assuming Ghostty semantics (margins, SGR colon syntax, mode 2026/2027, cursor save/restore, OSC 8 hyperlinks). Extracting `mp:apps/terminal/src/term/` is close (both are Ghostty), but any missing CSI/DCS/OSC sequence (e.g., OSC 52, OSC 133, SGR 58 underline color, reflow edge cases) will manifest as garbled repaints on attach—Spike 3 must test real `REPAINT` dumps from `vim`, `tmux`, `claude`, and `htop` `[verified: js:crates/g3d-termd/src/repaint.rs, mp:apps/terminal/src/term/mod.rs]`.
9. **Extracting `mp:apps/terminal/src/term/` cleanly**: Right now `term/` lives inside `makepad-app-terminal` (`mp:apps/terminal/Cargo.toml`), which pulls in `makepad-widgets`, `makepad-ai-services`, `makepad-wm-api`, etc. Depending on `makepad-app-terminal` directly would bloat the dependency tree; either extract `term/` into `mp:libs/term_vt` (cleanest, zero Makepad UI deps beyond `makepad-cell-width`) or copy/sync it into an `rs:` crate `[verified: mp:apps/terminal/Cargo.toml:1-46]`.
10. **Fullscreen cutout blur / vibrancy shader & sRGB color calibration**: JS's `WallFullscreen` relies on CSS `backdrop-filter: blur(...)` with an SVG/clip-path cutout around the elevated group/pane. In Makepad, rendering a true blurred cutout veil requires either a simple dimmed scrim (fast, portable) or a capture/blur render pass behind the elevated `DrawList2d`s. Clarifying whether a dimmed scrim is acceptable for v1 avoids burning hours on multi-pass blur render targets `[verified: js:wall/WallFullscreen.js:1-300]`.
