# Wall Port — Part 2: Comparison Against `survey.md`

**Evidence legend**
- **Repo roots**:
  - `js:` = `~/localdev/viz-web/glyph3d-js` (branch `wall`, HEAD `0db92d5b` + 12 uncommitted modified files)
  - `rs:` = `~/localdev/glyph3d-rs/.claude/worktrees/work` (branch `work`)
  - `mp:` = `~/localdev/makepad` (branch `work`)
  - `survey.md` = `<session scratchpad>/wall-port/survey.md`
- **Tags**: `[verified: <path>:<lines>]`, `[measured: <what was run>]`, or `[inferred]`.
- `part1.md` was written in full before `survey.md` was opened and has not been modified since.

---

## 1. What We Found That `survey.md` Missed

1. **Makepad's built-in text renderer (`DrawText` + HAL `FontService`) is already a GPU Slug vector renderer with variable fonts and dynamic eviction.**
   - `mp:draw/src/text/fonts.rs:67-69,119,225-232` defaults `slug_min_dpxs_per_em: 0.0` ("all text is slug" on every platform), backed by a host-owned multithreaded (`hal-font-0..N`), GPU-fence-retired `2048×128` RGBA32F Slug curve store (`mp:platform/hal/src/font/atlas.rs:250-380`, `pool.rs:1-373`), multi-plane color-emoji raster atlas, `ttf-parser` + `rustybuzz` variable-font axis support (`FontMember { res, weight, axes, ... }`), and `SlugCoverage::calc_coverage` using `abs(...)` winding (`mp:platform/script/resources/glyph_coverage.splash:270-290`) `[verified]`.
   - **Plan impact**: Removes the need to rebuild `rs:tools/export-atlas.mjs` for Monaspace Neon/Argon or build a new `GlyphFieldMode` + static atlas inside `glyph3d-native`.

2. **Retained `DrawList2d` with GPU `view_transform`.**
   - Every `DrawList2d` holds a 4×4 `view_transform` read directly by `slug_dilate` (`camera_projection * camera_view * view_transform`), and `begin_maybe(cx, false)` skips re-recording unchanged lists (`mp:draw/src/draw_list_2d.rs:35-180`, `mp:draw/src/shader/draw_text.rs:1-250`) `[verified]`.
   - **Plan impact**: Giving each terminal pane its own retained `DrawList2d` makes pan/zoom/drag O(panes) uniform updates with zero CPU re-recording of quiet terminals, without fighting `HostSlots`'s 5.2 MB CoW blocks.

3. **Critical Makepad HAL terminal-input bugs (macOS and Linux).**
   - `mp:platform/hal/src/backend/apple/appkit/app.rs:530-537` injects `win.paste(pasteboard)` **ahead of `KeyDown`** whenever `(mods & (MOD_LOGO | MOD_CONTROL)) != 0 && key == KeyV`—so **`Ctrl+V` on macOS pastes the clipboard *and* sends `^V`** `[verified]`.
   - `mp:platform/src/event/hal_intake.rs:361-367` fires `Event::TextCopy` / `Event::TextCut` whenever `modifiers.logo || modifiers.control` with `KeyC` / `KeyX`—hijacking **`Ctrl+C` (SIGINT) and `Ctrl+X` on both macOS and Linux terminals** `[verified]`.
   - `mp:platform/hal/src/backend/apple/appkit/classes.rs:666-681` only passes `keyDown:` to `NSTextInputContext` when `win.ime_active.get()` is true (no `TextInput` without `show_text_ime`), queues `Text` before `KeyDown`, and composes `Option+letter` (e.g., `Option+b` → `∫`) `[verified]`.
   - Both AppKit (`mp:platform/hal/src/backend/apple/appkit/keys.rs:9-24,108-116`) and Linux (`mp:platform/hal/src/backend/linux/input.rs:127-141`) expose only 4 modifier bits (no `CapsLock`, `NumLock`, `AltGraph`, or left/right modifier distinction) `[verified]`.
   - **Plan impact**: Must be tested in Spike 1 and patched cleanly in `mp:` (~15 lines for `Ctrl+C`/`X`/`V`), or an unattended run will ship broken terminal input.

4. **Seven recent commits on `js:` ahead of `docs/architecture/wall.md`, plus a third uncommitted perf fix.**
   - `survey.md` relied on `docs/architecture/wall.md`, missing:
     - `c8b5c9a1`: focus chords (`⌘[`/`]`, `⌃⇧[`/`]`, `⌃,`/`.`), unified `zStackLayout`, Z-elevated fullscreen (`FULLSCREEN_Z` + cutout veil), proportional momentum scroll (`terminalScroll.js`) `[verified: git log/show in js:]`.
     - `0db92d5b`: group fullscreen (`⌘↵`) vs solo zoom (`⌘⇧↵` + `returnGroup`), `⌘\` spawn chord, viewport-anchored placement `[verified]`.
     - `b293c1d3` / `033b70a4`: direct cross-ns `--ns agents` termd attach (`gx-<name>`, `role: "own"`, `flow: "lossy"`, `detach_on_kill`) for local gomuxai agents vs SSH for remote `name@host` `[verified]`.
     - `f3a07869`: drag-swap/dock for split panes & tiles + `⌘⇧+arrows` (`wall.pane.move`) `[verified]`.
     - `ae49a869` / `467c0a71`: Agent Views actively reserve layout room (`footprintChanged`: tile metrics, split `agentInset` + `fitRatios` + restore, canvas `shoveAside`) and enforce 1:1 `resolveTerminalSession` pairing `[verified]`.
     - Uncommitted `js:cli/attach_unix.go:496-555`: leading-edge output coalescing timer (immediate flush after quiet spell, 16 ms / 16 KiB batching under flood)—a key perf design that *should* carry over into the Rust worker→UI wakeup path `[verified]`.

5. **Two latent renderer/host hazards in `rs:makepad/`.**
   - `rs:makepad/src/g3_passes.rs:841` lacks `abs(coverage)` in its Slug shader, which breaks overlapping contours in variable fonts (Monaspace Neon/Argon) `[verified]`.
   - `rs:makepad/src/main.rs:127-134` exits fatally if `publication_accounting().envelope == 0`—a guard that must not apply to the Wall (which doesn't need `BlockResidency`) `[verified]`.

6. **Makepad's unattended automation & headless GPU primitives.**
   - Beyond manual `makepad-remote snap`, Makepad HAL already provides `MAKEPAD_NO_FOCUS=1`, `MAKEPAD_HIDE_WINDOWS=1`, `open_flags::OFFSCREEN_WINDOWS`, real input injection, and `register_app_query` (`mp:platform/src/remote.rs:1-120`, `mp:platform/hal/src/backend/apple/appkit/window.rs:281,1260-1282`, `mp:platform/hal/src/backend/apple/metal/mod.rs:8`) `[verified]`.
   - Also `rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md` already defines the unbuilt command-bus/palette/dock architecture for the Makepad host `[verified]`.

7. **Font asset gap, macOS scratch socket path limit, and `makepad-app-terminal` dependency bloat.**
   - `rs:` ships zero `.ttf` files (only prebaked `assets/atlas/` Cousine Regular) `[verified: rs:assets/atlas/FORMAT.md]`.
   - `mp:apps/terminal/Cargo.toml:33-42` pulls in `makepad-widgets`, `makepad-ai-services`, `makepad-wm-api`, and `makepad-app-registry`, so `src/term/` must be extracted into a lean crate rather than depending on `makepad-app-terminal` `[verified]`.
   - `js:docs/termd/MACOS.md:45-67` documents that macOS `sun_path` is 104 B (termd caps at 100 B) and `$TMPDIR` is 49 B—so automated test scratch sockets **must** be created under `/tmp`, never `std::env::temp_dir()` `[verified]`.

---

## 2. Where `survey.md` Is Wrong, Overstated, or Stale

1. **WRONG (`survey.md:164-166`): Claims Makepad's text renderer "gives up Slug coverage at any zoom."**
   - **Evidence**: `mp:draw/src/text/fonts.rs:67-69,119,225-232` sets `slug_min_dpxs_per_em: 0.0` ("0 (the default, every platform): all text is slug"), backed by `SlugStore` (`mp:platform/hal/src/font/atlas.rs:250-380`) and `SlugCoverage` (`mp:platform/script/resources/glyph_coverage.splash:270-290`, which already uses `abs()` for variable fonts and transforms via `view_transform`).
   - **Correction**: Makepad's text renderer *does* give Slug coverage at any zoom (and dynamic variable fonts + CJK + emoji out of the box). What it actually lacks is a zoomed-out greeking LOD tier, mode-3 cell-edge-clipped vector sprites (needs procedural quads or custom sprite shader), and sharing one shader pass with 3D `glyph3d-native` repo scenes.

2. **OVERSTATED (`survey.md:157-166, 186`): Recommends a new `GlyphFieldMode` inside `glyph3d-native` plus a new static `export-atlas.mjs` bake for Monaspace Neon/Argon 4 faces + sprites.**
   - **Evidence**: `rs:tools/export-atlas.mjs:1-120` is a Stage-B re-packer of `tools/vendor/ref/app/public/slug-core/*.bin` (a Bun/HarfBuzz-baked `SLGC` envelope) with append-only slot IDs pinned to `glyph3d-native` goldens; `rs:AGENTS.md:36-50` notes two prior `GlyphFieldMode`s (`Visible`, `Instanced`) were deleted so `Derived` stays the single static-repo mode; and `rs:makepad/src/g3_passes.rs:841` doesn't even support variable-font winding yet (`abs()` missing) and blends in linear RGBA16F instead of sRGB.
   - **Correction**: Building the Wall as a dedicated Makepad-native 2D renderer (retained `DrawList2d` per pane + HAL Slug `FontService` + custom cell bg/decoration/sprite/greeking shaders) avoids touching `glyph3d-native`'s static atlas, `HostSlots`, or golden gates, and ships Monaspace Neon/Argon variable fonts + emoji + CJK immediately.

3. **INCOMPLETE (`survey.md:132-138`): Uncommitted JS work & MSAA status.**
   - `survey.md` missed the uncommitted leading-edge coalescing timer in `js:cli/attach_unix.go:496-555`.
   - `survey.md:137-138` left MSAA unchecked in Makepad; verified off by default (`msaa: 1` in `mp:platform/hal/src/backend/apple/metal/mod.rs:165`; Vulkan defaults to `samples = 1`).

4. **UNDERSTATED RISK (`survey.md:181, 189-191`): Dependency coupling and command-bus ordering.**
   - `survey.md:181` assumes `g3d-wire` and `g3d` can simply be added as dependencies (without addressing that they live in the live, dirty `js:crates/` repo on Rust edition 2024 vs `rs:` edition 2021). Either vendor/write a thin pure-Rust `crates/g3d-client` in `rs:` or intentionally extract a shared repo first.
   - `survey.md:189` defers the `wall.*` command bus to Step 5. In JS (`js:wall/wallCommands.js`), chords, buttons, menus, palette, and tests all route through the command bus—building the command router in Step 2 prevents rewiring every interaction twice `[inferred from verified js:wall/wallCommands.js]`.

5. **STALE SCOPE COUNT (`survey.md:42`):** `js:wall/` now has 49 files (not 46), reflecting the 7 post-doc commits (`WallAgentViews`, `wallAgentView`, `terminalScroll`, etc.) `[verified: ls js:wall/]`.

---

## 3. Where We Agree After Checking vs Where We Did Not Look

### Verified Agreement
- **Skip the Go relay + WebSocket + `@xterm/headless`**: `g3d-termd` protocol 1.5 (`OUTPUT`/`REPAINT`, `key`/`mouse`/`paste`/`view`, roles `watch`/`attach`/`own`, `0x86 CELLS` unbuilt) is already live at `~/.local/bin/g3d-termd`, and `rustc 1.98.0` is installed `[verified + measured]`.
- **Client VT core**: `mp:apps/terminal/src/term/` (pure-Rust Ghostty port, MIT + Unicode-3.0) behind a narrow `bytes → dirty rows` seam is the right Phase 1 choice; keep `g3d-termd` S3 `CELLS` as an optional Phase 2 swap behind the same seam `[verified]`.
- **Render on demand from day 1**: `rs:makepad/src/viewport.rs:113-117` spins every frame today and must be replaced with demand wakeup (Makepad HAL already sleeps at `timeout = u64::MAX`) `[verified]`.
- **Meaning of "default" & landing bar**: `glyph3d-makepad` with no args opens the Wall (keeping `glyph3d-native` CLI & `cargo glyph test` pixel-ab gates untouched); don't build a throwaway wgpu twin just to satisfy `pixel-ab`—gate pure logic with `cargo test` and verify the GUI via `makepad-remote` `[verified]`.
- **Live namespace safety**: Never touch `wall`, `agents`, or `default` termd namespaces during dev/tests; `rs:AGENTS.md`'s note calling `glyph3d-js` retired/read-only is stale `[verified]`.

### Where We Did Not Look
- Contents of the 17 `js:tools/wall-*.test.mjs` bun test suites and `js:tools/wall-e2e.mjs`.
- `js:docs/termd/S2-PORT.md` and `js:docs/termd/DESIGN.md` past line 533.
- `mp:apps/wm/` and upstream Makepad commits after the fork tip (2026-11-01).
- Archived egui branches (`archive/egui-interface-*`, `rs:out/history/ARCHIVED-BRANCHES.md`).
- Live execution on Linux `workstation` (checked Linux HAL source only).
- Calendar/time sizing.

---

## 4. Coverage of `survey.md` Section 8 ("What This Survey Did NOT Examine")

| Section 8 Thin Spot | Covered? | What We Found |
|---|---|---|
| 1. `TerminalGrid.js` / `TerminalEmulator.js` internals | **Yes** | Verified ring-buffer scroll, `TerminalScreenQueue`, `_diffLive`, `_writeSpans`, selection/cursor (`js:packages/glyph3d-core/src/TerminalGrid.js:1-800`, `TerminalEmulator.js:1-220`) `[verified]`. |
| 2. `glyphMaterial.js` cell paths | **Yes** | Verified 2-pass WebGL2 material (fill pass, Slug + mode-3 cell-clipped sprites, 5 underlines + color, strike/overline/dim, greeking); also found `rs:makepad/src/g3_passes.rs:841` lacks `abs()` winding and blends in linear RGBA16F (`js:packages/glyph3d-core/src/glyphMaterial.js:1-450`) `[verified]`. |
| 3. Font path (`FontChain`, `cellSprites`, `families.js`, `export-atlas.mjs`) | **Yes** | Verified `cellSprites.js` (480 lines procedural cell-edge vector sprites), JS font files (`Cousine-*`, `MonaspaceNeonVar.ttf`, `MonaspaceArgonVar.ttf`, `MesloLGS-NF-Mono.ttf`, `DejaVuSans.ttf`), and `rs:tools/export-atlas.mjs:1-120` (static `SLGC` re-packer). Discovered Makepad HAL Slug + variable fonts eliminates extending `export-atlas.mjs` `[verified]`. |
| 4. `DESIGN.md`, `MACOS.md`, rest of `PROTOCOL.md` | **Mostly** | Read full `PROTOCOL.md` (1,334 lines: `key`/`mouse`/`paste`/`view`/`status_follow`/`flow`/`detach_on_kill`), `MACOS.md` (1–120: `TIOCOUTQ`, `/tmp/g3d-<uid>` 100 B socket cap, `MAX_CANON` 1023 B, launchd env), and `DESIGN.md` (1–533) `[verified]`. |
| 5. `mp:apps/terminal` (`term/` deps/license), `libs/terminal_core`, `apps/wm` | **Mostly** (`apps/wm` & upstream not read) | `apps/terminal` is `MIT AND Unicode-3.0`; `src/term/` is pure Rust (only depends on `makepad-cell-width`), but `apps/terminal/Cargo.toml` pulls in `makepad-widgets`, `makepad-ai-services`, `makepad-wm-api`, `makepad-app-registry` → extract `term/` into a lean crate. `MpTerm` (`widget.rs`) lacks italic/faint, `Alt+key`, `KeyUp`, and hits HAL `Ctrl+C`/`V` bugs. `libs/terminal_core` is only PTY spawn/resize `[verified]`. |
| 6. Makepad host limits (partial updates, MSAA, IME, clipboard, keys, pointer, DPI) | **Yes** | MSAA is off (`msaa: 1` Metal, `samples = 1` Vulkan). `DrawList2d` has GPU `view_transform` + skip-re-record; `shared_instances` has append-prefix `partial_ok` (not random cell patch). All 5 mouse buttons, macOS/Wayland pinch, and macOS `ScrollPhase`/`precise` work. **Found critical input traps**: `Ctrl+V` paste (`app.rs:530`), `Ctrl+C`/`X` copy (`hal_intake.rs:361`), IME-gated `TextInput`, 4-bit-only modifiers, `Option+key` composition `[verified]`. |
| 7. RS scene model for dynamic add/remove/resize | **Yes** | `HostSlots` uses 5.2 MB CoW blocks (`262,144` slots) and fixed scene item tables requiring full `relayout` on add/remove (`rs:crates/glyph-field/src/host_slots.rs`, `rs:makepad/src/definition.rs`) — poor fit for 20+ dynamically resizing live terminals `[verified]`. |
| 8. Agent View data path vs `native/src/agent_transcript` | **Yes** | JS uses `agent export --json --follow --all`, `agent sessions --json`, `agent transcript --follow`/`--until` (`js:cli/{gomux,agentevents}.go`), direct `--ns agents` `gx-<name>` attach (`b293c1d3`), and layout-room reservation (`ae49a869`/`467c0a71`). `rs:native/src/agent_transcript/agent_cli.rs` only does one-shot queries today — needs `--follow` streaming `[verified]`. |
| 9. Archived egui command bus | **No** | Did not read archive tags; did verify the newer active spec `rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md` `[verified]`. |
| 10. Linux `workstation` | **No** | Checked `mp:platform/hal/src/backend/linux/` source only (did not run on Linux). |
| 11. Time sizing | **No** | Not attempted. |

---

## 5. Where We Would Still Want to Dig (and Why)

1. **Run the 29 `js:tools/fixtures/vt-conf/*.vt` snippets through `g3d-termd` `REPAINT` into `mp:apps/terminal/src/term/`** (`js:tools/termd-fidelity-check.mjs` already defines the corpus and known Ghostty-vs-xterm diffs). Verifying zero unexpected diffs between direct feed and `g3d-termd` `REPAINT` de-risks the entire VT seam in under an hour `[verified: js:tools/termd-fidelity-check.mjs:1-120]`.
2. **Inspect `mp:apps/wm/`**: `MpTerm` explicitly carries a `canvas_ime_anchor` transform hook for canvas hosts (`mp:apps/terminal/src/widget.rs:1917`). Reading `mp:apps/wm/` will show Makepad's proven conventions for `DrawList2d` canvas zooming, transformed pointer hit-testing, focus transfer, and IME anchoring `[verified]`.
3. **Audit `DrawText` glyph pixel-snapping under fractional `view_transform` zoom (Spike 2)**: Verify whether `DrawText::draw_prepared_glyph_abs` snaps glyph origins to screen pixels in a way that jitters during smooth canvas zoom, and confirm whether a thin custom wall-cell shader sampling Makepad's lent `slug_curve_texture()` + procedural cell sprites + greeking LOD is cleaner than layering `DrawText` + separate box-drawing/underline passes `[inferred]`.
4. **Catalog invariants from the 17 `js:tools/wall-*.test.mjs` suites**: Use them directly to derive the Rust unit tests for `splitTree`, `WallGroups`, `wallSnap`, `wallKeymap`, `zStackLayout`, and `WallAgentViews` footprint math before writing UI `[inferred]`.
5. **Confirm two scope boundaries with Ivan before starting an unattended run**:
   - May the unattended run commit small targeted changes in `mp:` (fixing `Ctrl+C`/`X`/`V` clipboard hijacking in `app.rs:530` & `hal_intake.rs:361` and extracting `mp:apps/terminal/src/term/` into `mp:libs/term_vt`)?
   - For `WallFullscreen`'s cutout veil, is a dimmed scrim acceptable for v1 instead of a multi-pass GPU Gaussian blur render target?
