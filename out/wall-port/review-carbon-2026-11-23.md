# Critical Review of the Wall Port Survey & Plan (`wall-carbon`)

**Reviewed document:** `/private/tmp/claude-501/-Users-lugo-localdev-glyph3d-rs/be544915-3925-4176-abd7-f71faca80827/scratchpad/wall-port/survey.md`  
**Reviewer:** `wall-carbon` (read-only verification across `glyph3d-rs` branch `work`, `viz-web/glyph3d-js` branch `wall`, and `makepad` branch `work`)  
**Date:** 2026-11-23

> **Provenance convention:** Every finding below explicitly states **`[Verified: repo:path:lines]`** where checked directly against source, or **`[Inferred: ...]`** where deduced from verified facts without runtime execution.

---

## 0. Verdict & Recommended Plan Changes (Most Important First)

### Verdict

**Do not launch an unattended end-to-end goal run from this plan as written.**

The survey's high-level instinct is right — skip the Go WebSocket relay and xterm.js, speak `g3d-termd` Protocol 1.5 over the local Unix socket, reuse Ghostty's Rust port from the Makepad fork for VT state, render cells with Slug in the Makepad host, and render on demand from day one.

However, source verification uncovered **seven concrete architectural and build/runtime traps** in Steps 1–3 and Decisions 1–3 that would either **fail to compile**, **corrupt or drop keyboard input**, **re-upload 5.2 MB GPU chunks per keystroke**, **break off-center clicks and Monaspace rendering**, or **leave an unattended agent blind or hung on snapshots**. None of these require abandoning the port, and all of them are clean to fix *before* implementation starts.

### Top 9 Changes to Make to the Plan (Ranked by Impact)

1. **Replace "`g3d-wire`, `g3d` as Cargo dependencies" (Step 1) with an internal non-blocking `crates/termd-wire` + reader-thread client in `glyph3d-rs`.**  
   `g3d` and `g3d-wire` live in a separate, actively churning repo (`viz-web/glyph3d-js/crates/`), inherit `edition = "2024"` / `rust-version = "1.96"` / Rust 2024 `let`-chains (`viz-web/glyph3d-js/Cargo.toml:9-11`, `crates/g3d/src/start.rs:43-44`), pull CLI/Unix crates (`clap`, `signal-hook`, `rustix`), risk breaking `relax` (`libs/relax/compiler/Cargo.toml:4`, edition 2021) under `cargo glyph prove`, and — fatally for a multi-terminal GUI — `g3d::Client` (`crates/g3d/src/client.rs:50-56, 245-263`) is a **synchronous blocking single-socket client** that blocks on `REPLY` while stashing interleaved `OUTPUT`/`EVENT` frames in an unbounded `VecDeque`. Write a ~600-line edition-2021 `crates/termd-client` crate inside `glyph3d-rs` with a dedicated socket reader thread (waking Makepad via `Cx::post_action` / `SignalToUI`, `makepad:platform/src/os/hal/mod.rs:72-79`) and a non-blocking writer channel with async `req -> reply` correlation.

2. **Extract or vendor `makepad`'s `apps/terminal/src/term` + `makepad-cell-width` into a pure workspace crate (`crates/glyph-vt`), and fix its three `REPAINT` hazards before trusting it.**  
   Decision 1 ("use `apps/terminal/src/term`") and Decision 3 ("hold the emulator seam in `cargo-test`") contradict each other as written: `makepad/` is excluded from the `glyph3d-rs` workspace (`glyph3d-rs:Cargo.toml:22`), `cargo-check-no-ui` forbids UI deps (`glyph3d-rs:build.toml:458-465`), and `makepad-app-terminal` unconditionally pulls `makepad-widgets`, `makepad-ai-services`, `makepad-wm-api`, and `makepad-app-registry` (`makepad:apps/terminal/Cargo.toml:28-41`). Yet `apps/terminal/src/term/` itself only imports `makepad_cell_width` (`makepad:apps/terminal/src/term/terminal.rs:16`), and both are MIT/Unicode-3.0. Moving/vendoring `term` into `crates/glyph-vt` lets `cargo glyph test` run the 29 `vt-conf/*.vt` `REPAINT` conformance fixtures directly, and lets you fix `term`'s (a) unbounded `outbound` query-reply buffer (`terminal.rs:82, 172`), (b) incomplete `soft_reset` (`CSI ! p`) + Kitty keyboard stack reset (`terminal.rs:1604-1620, 1866-1880`), and (c) missing per-row dirty tracking (`terminal.rs:86`, `page.rs:115-121`).

3. **Do NOT implement terminals as a `GlyphFieldMode` inside `GlyphScene`'s static 256K-slot chunk arena (Decision 2). Build a dedicated per-terminal `TerminalSurface` / `WallRenderer` sharing the Atlas and Slug/Sprite/Emoji shaders.**  
   `GlyphScene` and `GlyphField` assume an immutable glyph count fixed at load (`glyph3d-rs:native/src/glyph_scene.rs:234`), and `makepad/src/g3_passes.rs:1637-1668, 1993-1998` uploads 256K-slot (~5.2 MB) `HostSlots` blocks keyed by `Arc::ptr_eq` — **changing a single cell in a block clones and re-uploads 5.2 MB**. In `glyph3d-js`, each terminal owns its own ~10K–15K-slot buffer (`viz-web/glyph3d-js:packages/glyph3d-core/src/collections/TerminalGrid.js:243-248`). Give each terminal its own compact GPU cell buffer (~64–256 KB) + row-ring uniform, rendered by a dedicated Makepad cell pass that shares the Slug curve/glyphmap/emoji textures.

4. **Redesign Step 1 keyboard & IME routing before writing `Op::Key` glue.**  
   `termd`'s `Op::Key` (`viz-web/glyph3d-js:crates/g3d-wire/src/msg.rs:180-203`, `docs/termd/PROTOCOL.md:541-629`) expects browser W3C `KeyboardEvent` fields (`code`, `key`, `mods`, `text`) **in one atomic event**. Makepad (`makepad:platform/src/event/keyboard.rs:7-9`, `platform/hal/src/backend/apple/appkit/classes.rs:638-680`) splits input across two separate events (`Hit::TextInput` with text only, followed by `Hit::KeyDown` with physical US `KeyCode` + `modifiers` only) — and emits **zero `TextInput` events** unless `cx.show_text_ime(...)` is active! Furthermore, `Glyph3dViewport` steals `` ` `` and `Escape` (`glyph3d-rs:makepad/src/viewport.rs:166, 170`) and `main.rs` steals `F1/F6/F7/F8` (`glyph3d-rs:makepad/src/main.rs:100-108`). Either encode terminal keys locally via `term`'s Ghostty encoder (`apps/terminal/src/term/key_encode.rs`, sending raw `INPUT` frames as `MpTerm` does in `apps/terminal/src/widget.rs:2088-2165`) or explicitly coalesce AppKit's `TextInput` + `KeyDown` pair per event turn.

5. **Fix two shader bugs in `glyph3d-rs` before Step 3 (Monaspace & terminal themes):**  
   - **Non-zero winding `abs(coverage)`:** `viz-web/glyph3d-js:packages/glyph3d-core/src/core/glyphMaterial.js:374-378` uses `clamp(abs(coverage) * 0.5, 0.0, 1.0)` because *"Monaspace winds its accents against its letters and a variable font overlaps its strokes."* Both `glyph3d-rs:makepad/src/g3_passes.rs:841` and `crates/glyph-field-derived/shaders/glyph_field_derived.wgsl:394` omit `abs()`, clamping negative winding to `0.0` (missing accents and holes in variable-font overlaps).  
   - **Dark-color crush (`pow(2.2)` vs IEC sRGB):** `makepad/src/g3_passes.rs:862-863` linearizes sRGB with `pow(c, 2.2)` while `DrawG3Composite` (`g3_passes.rs:900-904`) re-encodes with exact piecewise IEC sRGB (`12.92 * l` toe), crushing dark terminal backgrounds like `#1a1b26` (`26/255`) down to `#13141d` (`19/255`).

6. **Do not reuse `GlyphScene`'s perspective ray-picker or Euclidean-distance LOD for the orthographic Wall (Step 2).**  
   `pixel_ray` (`glyph3d-rs:native/src/glyph_scene/pick.rs:257-285`) hardcodes diverging perspective rays (`(fov_y_deg * 0.5).tan()`), and `cull.rs` (`glyph3d-rs:native/src/glyph_scene/cull.rs:137, 278-294`) computes `glyph_px` from 3D Euclidean distance to `eye`. On an orthographic 2D Z-layered wall, hit-testing (`terminal` + `(col, row)`) is exact 2D rectangle math in Z-order, and `glyph_px` is uniform across the viewport (`zoom * cell_height_world`).

7. **Replace "manual `makepad-remote snap`" (Decision 3) with an automated two-tier verification harness so an unattended agent can self-verify every step.**  
   In an unattended `/goal` run, there is no human looking at `snap` PNGs. Moreover, `makepad:docs/agents/app-remote.md:252-266` documents that under `MAKEPAD_HIDE_WINDOWS=1` on macOS, `grab` stalls ~2s per frame, `gseq` **times out when an app renders on demand**, and `MAKEPAD_HAL_BACKEND=headless` reports `working_set: 0` (`makepad:platform/hal/src/backend/headless.rs:45`), which triggers `std::process::exit(1)` in `glyph3d-rs:makepad/src/main.rs:127-134`! Put VT fidelity, `termd` wire state, and all ported `wall-*` pure logic in `crates/*` under `cargo glyph test`, and build a scripted `--remote` smoke runner that triggers a redraw before a single `snap` and asserts cell/pixel invariants.

8. **Move pure `wall-core` Rust logic + its 17 unit-test suites *ahead* of UI chrome, and reconcile the command router with `out/INTERFACE-SHELL-SPEC-2026-11-01.md`.**  
   The RS repo already has a live, canonical command-bus/console/palette spec for Makepad (`glyph3d-rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md:1-200`), missed by the survey (`survey.md:120-122, 227`). Unify the bus grammar early instead of inventing a second incompatible verb/palette system at Step 5.

9. **Keep `--wall` behind an explicit flag + isolated `--termd-namespace wall-dev` until Step 6; never touch live `wall` namespace during development.**  
   Flipping no-args `glyph3d-makepad` to open the wall in namespace `wall` only happens at the end of Step 6 after all gates pass.

---

## 1. Verification of Load-Bearing Claims in the Survey

| # | Survey Claim | Verdict | Source Evidence |
|---|---|---|---|
| 1.1 | "`wall/`: 46 files, 14,146 lines... `WallStore.js`, 13 `localStorage` keys per relay port" (§3) | **Confirmed** | `[Verified: viz-web/glyph3d-js:package.json:7-11, wall/TerminalWall.js:155-246, wall/WallStore.js:78-83]` — workspace `wall/` at repo root; exact 13 keys (`positions`, `names`, `hidden`, `groups`, `sidebar`, `status`, `config`, `settings`, `help`, `fullscreen`, `fullscreenText`, `agentViews`, `attached`). |
| 1.2 | "`g3d-wire` depends on serde only; `g3d`'s lib half is a blocking client... Workspace needs rustc 1.96, edition 2024" (§4) | **True in isolation, misleading as a Step 1 dependency plan** | `[Verified: viz-web/glyph3d-js:Cargo.toml:9-11, crates/g3d-wire/Cargo.toml:1-14, crates/g3d/Cargo.toml:1-27, crates/g3d/src/client.rs:50-56, 245-263]` — `g3d-wire` only uses `serde`/`serde_json`, **but** inherits workspace `edition = "2024"` / `rust-version = "1.96"`. `g3d` pulls `clap`, `signal-hook`, `rustix`, `libc`, uses Rust 2024 `let`-chains (`crates/g3d/src/start.rs:43-44`), and its `Client` cannot multiplex streaming `OUTPUT` + concurrent non-blocking UI requests across threads. |
| 1.3 | "Protocol 1.5: `subscribe` with `mode: 'raw'` gives one `REPAINT` then `OUTPUT`... `key`, `paste`, `mouse` ops... `0x86 CELLS` reserved" (§4) | **Confirmed, with major unread client obligations** | `[Verified: viz-web/glyph3d-js:docs/termd/PROTOCOL.md:83-105, 290-360, 503-695, 920-1055, crates/g3d-wire/src/lib.rs:40-95]` — framing and ops match, **but** Survey missed mandatory client rules in unread §"Flow control", §"Restarts", §"Paste", and §"Minimal client": (a) suppressing emulator query replies (`PROTOCOL.md:1038-1040`), (b) handling `lagged` events and `ERROR restart` + `instance` changes (`PROTOCOL.md:945-1015`), (c) chunked `paste` (`more: true`) and `unsafe_paste` rejection (`PROTOCOL.md:631-662`), and (d) W3C `KeyboardEvent` fields required by `Op::Key` (`PROTOCOL.md:541-629`). |
| 1.4 | "makepad's `apps/terminal/src/term` — a pure-Rust port of Ghostty's terminal subsystem... 'no Makepad, no PTY, no I/O'" (§7.1) | **Module header is true; crate boundary is NOT** | `[Verified: makepad:apps/terminal/src/term/mod.rs:1-8, apps/terminal/src/term/terminal.rs:16, apps/terminal/Cargo.toml:16, 28-41]` — `src/term/` only imports `makepad_cell_width` (MIT licensed), **but** it is a private module inside `makepad-app-terminal`, which depends on `makepad-widgets`, `makepad-ai-services`, `makepad-wm-api`, `makepad-app-registry`, and `makepad-studio-Tweaker`. Cannot be imported into `glyph3d-rs`'s gated workspace without extraction/vendoring. |
| 1.5 | "NOT VERIFIED: that makepad's core reproduces termd's REPAINT faithfully" (§7.1) | **Checked in source: ~90% match, 3 concrete bugs/gaps** | `[Verified: viz-web/glyph3d-js:crates/g3d-termd/src/repaint.rs:14-129 vs makepad:apps/terminal/src/term/{terminal.rs,sgr.rs,osc.rs}]` — `term` supports `1049`, `DECSTBM`/`DECSLRM`, colon SGR (`4:0..5`, `38:2::r:g:b`, `58:2::r:g:b` at `sgr.rs:111-174, 226-300`), `OSC 2/4/7/8/10/11/12`, `DECSCUSR`, G0–G3 charsets, and mode 2027 graphemes. **Fails/leaks on:** (1) `self.outbound` query replies (`terminal.rs:82, 172`), (2) `soft_reset` (`CSI ! p`) missing `modify_other_keys` / `current_hyperlink` / mode 2027 / Kitty stack depth (`terminal.rs:1604-1620, 1866-1880`) and unbounded `hyperlinks: Vec<String>` (`terminal.rs:75, 1688`), (3) no per-row dirty bit (`terminal.rs:86`, `page.rs:115-121`). |
| 1.6 | "`GlyphField` has in-place slot writes (`write_color`, `write_placements`...) ... `GlyphFieldMode` is the declared seam" (§5, §7.2) | **Overstated for dynamic multi-terminal grids** | `[Verified: glyph3d-rs:crates/glyph-field/src/field.rs:18-87, native/src/glyph_scene.rs:132-239, makepad/src/g3_passes.rs:1637-1668, 1993-1998]` — `GlyphField` writes into fixed-length scene-wide 256K-slot (`5.2 MB`) `HostSlots` blocks; `GlyphScene` fixes `glyph_count`, `item_table`, `Views`, `PickContext`, and `CullState` for the life of the scene; and Makepad's `G3GlyphBlock` re-uploads an entire 5.2 MB chunk whenever `Arc::ptr_eq` changes. |
| 1.7 | "The makepad viewport advances the scene on every next-frame event today. MSAA: not checked whether the makepad pass multisamples." (§6) | **Render-loop spin confirmed; MSAA already 1x (off)** | `[Verified: glyph3d-rs:makepad/src/viewport.rs:113-117, 215; makepad:platform/hal/src/backend/apple/metal/mod.rs:165, platform/hal/src/backend/apple/appkit/mod.rs:8-10, glyph3d-rs:makepad/src/g3_passes.rs:1951]` — `Glyph3dViewport` calls `self.next_frame = cx.new_next_frame(); self.area.redraw(cx);` unconditionally every frame. Makepad's AppKit/Metal HAL already sleeps at 0% CPU when idle and already sets `msaa: 1` (no multisampling on either the window drawable or `DrawG3`'s `Rgba16F` target). |
| 1.8 | "`tools/export-atlas.mjs` exports from pinned snapshot... No Monaspace Neon, no bold/italic, no sprites... Glyph ids are `u16`" (§5.3, §8) | **Understates what `export-atlas.mjs` does** | `[Verified: glyph3d-rs:tools/export-atlas.mjs:87-100, native/src/atlas.rs:1-190, build.toml:421; viz-web/glyph3d-js:packages/glyph3d-core/src/fonts/families.js:8-82, shaping/cellSprites.js:48-54]` — `export-atlas.mjs` **has no Slug curve encoder at all**; it merely gunzips a prebaked `slug-core.<key>.bin` from `tools/vendor/ref` and repacks headers! Baking Monaspace Neon + Argon variable TTFs (`wght 400/700`, `slnt 0/-11`) + mode-3 cell sprites requires invoking JS `SlugEncoder` + `cellSprites.js` (or porting them). `u16` (`65,535`) fits Neon(4) + Argon(4) + Cousine(4) (~20k–30k glyphs) **only if** upright NerdFont/DejaVu/sprite/emoji fallbacks are shared across faces rather than duplicated 12×. |
| 1.9 | "Agent sessions through gomuxai's `agent` CLI (`native/src/agent_transcript/`)" (§5, §8) | **Batch one-shot only; lacks live follow & bus** | `[Verified: glyph3d-rs:native/src/agent_transcript.rs:35-39 vs viz-web/glyph3d-js:wall/WallAgentViews.js:68-130, docs/termd/S2-PORT.md:76, 189]` — RS runs `agent transcript <session>` once to stdout and builds a static 3D Carrel scene. Wall's `WallAgentViews` + `WallBus` tail live incremental JSONL `add`/`update` streams (`256 KB` tail window + live follow + `loadEarlier`) and watch live `agent` fleet status. |
| 1.10 | "An egui command bus exists only as archive tags (`archive/egui-*`)" (§5.7, §8) | **Stale: a live Makepad command-bus spec already exists** | `[Verified: glyph3d-rs:out/INTERFACE-SHELL-SPEC-2026-11-01.md:1-200, out/history/ARCHIVED-BRANCHES.md:75]` — Ivan already commissioned a full Makepad-targeted spec for the command bus, console (F9), and Cmd-K palette (`INTERFACE-SHELL-SPEC-2026-11-01.md`). |

---

## 2. Argument on the Four Decisions (§7)

### Decision 1: Client Emulator
- **Survey recommendation:** (a) Makepad's `apps/terminal/src/term` behind a `"bytes in, dirty cell rows out"` seam, with (c) `termd` S3 packed cells as a possible later swap.
- **Is it right?** **Right engine, wrong packaging, and three subtle bugs if used raw.**
  - Choosing Ghostty's pure-Rust port over (b) `libghostty-vt` C/Zig FFI is right: `cargo glyph prove` and `cargo glyph test --relax` compile workspace Rust through Makepad's `relax` JIT (`glyph3d-rs:AGENTS.md`, `glyph/src/prove_relax.rs:28-31`), where a Zig build + C staticlib is serious friction, and option (c) S3 violates the freeze on live `glyph3d-js` (`survey.md:200`).
  - **What makes (a) wrong as stated:**
    1. `apps/terminal/src/term` lives inside `makepad-app-terminal`, which pulls the entire Makepad widget/AI/WM stack (`makepad:apps/terminal/Cargo.toml:28-41`), so `glyph3d-rs`'s gated workspace cannot depend on it.
    2. `term::Terminal` does **not** track dirty rows (`makepad:apps/terminal/src/term/terminal.rs:86` has one `dirty: bool`, and `Row::cells` in `page.rs:115-121` trims trailing default cells).
    3. `term::Terminal` queues query answers into `self.outbound: Vec<u8>` (`terminal.rs:82, 1377-1497, 1697-1703`), omits several state resets in `soft_reset` (`CSI ! p`, `terminal.rs:1866-1880`), and never shrinks `hyperlinks: Vec<String>` (`terminal.rs:75, 1688`).
- **Unlisted option — (d) RECOMMENDED:** Extract `apps/terminal/src/term` + `libs/cell_width` into a zero-UI crate (`libs/term_vt` in `makepad` or vendored as `crates/glyph-vt` in `glyph3d-rs`, preserving its MIT/Unicode-3.0 headers), add a `u64` per-row generation counter (or `u128`/`BitVec` dirty-row mask) + `discard_outbound()` mode for passive `termd` subscribers, fix `soft_reset` + hyperlink interning/GC, and gate `crates/glyph-vt` in `cargo glyph test` against the 29 `vt-conf/*.vt` fixtures passed through `REPAINT`.

### Decision 2: Who Draws the Cells
- **Survey recommendation:** A new cell-grid `GlyphFieldMode` beside `Derived`, drawn by a Makepad pass.
- **Is it right?** **Yes on Slug/glyph3d rendering over `MpTerm`'s widget; NO on squeezing it into `GlyphFieldMode` / `GlyphScene`.**
  - Using Makepad's `MpTerm` (`apps/terminal/src/widget.rs`) would throw away resolution-independent Slug outlines across continuous canvas zoom (`0.15×` to `4×+`), smooth greek transitions, 3D depth scrollback, and crisp cell-aligned box/braille/powerline sprites.
  - **What makes `GlyphFieldMode` wrong:** `GlyphField` (`glyph3d-rs:crates/glyph-field/src/field.rs:18-87`) is a scene-wide static glyph-slot table for `GlyphScene` (`native/src/glyph_scene.rs:132-239`), chunked into 256K-slot (5.2 MB) `Arc<[f32]>` blocks (`makepad/src/g3_passes.rs:1637-1668`). On the Wall, 10–40 terminals spawn, close, resize (`cols × rows` reallocations), Z-restack, and mutate a handful of cells at 60–120 Hz. Sharing one `GlyphField` forces slot-range fragmentation/compaction on resize and 5.2 MB buffer copies on single-cell edits (`g3_passes.rs:1993-1998`).
- **Unlisted option — (c) RECOMMENDED:** Factor out a standalone **`WallCanvas` / `TerminalSurface` renderer** beside `GlyphScene`:
  - Share the **immutable (or appendable) Slug Atlas textures** (`curves`, `glyphmap`, `emoji`) and Slug/greek fragment math.
  - Give **each terminal** its own compact cell buffer (`cols * (rows + depth_max)` cells × 16 bytes = `~77 KB` for `120×40` live or `~230 KB` with 80 depth rows):
    - `u32 glyph_and_flags` (`glyph_id:16 | style_bits:8 | face_and_mode:8`)
    - `u32 fg_rgba`
    - `u32 bg_rgba`
    - `u32 ul_rgba` (underline color + cursor/selection overlay bits)
  - Draw each visible terminal (frustum-culled in 2D world space, back-to-front by `zOrder`) in 2–3 lightweight instanced draws (opaque panel + non-default cell fills -> decorations/selection -> Slug ink/sprites/emoji -> cursor + border), uploading only dirty terminals (`~77 KB` max, or dirty row slices).

### Decision 3: Landing Bar for Wall Pixels
- **Survey recommendation:** Keep wall pixels ungated at first; hold engine half in `cargo-test`; use `makepad-remote app <pid> snap` manually.
- **Is it right?** **Fatal for an unattended `/goal` run.**
  - In an unattended run, "manual instrument" means **nobody checks the pixels**, or the agent improvises ad-hoc `makepad-remote` shell calls and hits documented macOS traps (`makepad:docs/agents/app-remote.md:252-266`):
    - `MAKEPAD_HIDE_WINDOWS=1` on macOS stalls ~2 seconds on every subsequent `grab`.
    - `gseq` **times out** when an app renders on demand (which the plan enables from the start!).
    - `MAKEPAD_HAL_BACKEND=headless` sets `working_set: 0` (`makepad:platform/hal/src/backend/headless.rs:45`) and `NO_RASTER` (`headless.rs:78, 116`), causing `glyph3d-makepad` to immediately `std::process::exit(1)` at `glyph3d-rs:makepad/src/main.rs:127-134`!
- **Unlisted option — (c) RECOMMENDED:**
  1. Put **all non-widget Wall crates** (`crates/termd-client`, `crates/glyph-vt`, `crates/wall-core` — layout, groups, splits, snap, store, keymap, verb router, orthographic camera + hit-test) inside the root `glyph3d-rs` workspace so `cargo glyph test` and `cargo glyph prove` enforce them automatically.
  2. Fix `makepad/src/main.rs:127-134` so `envelope == 0` uses a bounded fallback budget under headless/test mode instead of `exit(1)`.
  3. Build a single deterministic **`tools/wall-smoke.py` (or `cargo run -p glyph3d-makepad --bin ...`) harness** using Makepad's GPU-real offscreen window mode (`open_flags::OFFSCREEN_WINDOWS`, `makepad:platform/hal/src/backend/apple/metal/mod.rs:8-10, 133`) or `--remote --window 1280x800@1` + one explicit wake command before a single `snap`, asserting non-blank panel background, expected sRGB hex values, glyph ink density in known cell rects, and 0 idle frames over a 500 ms quiet window.

### Decision 4: What "Default" Means
- **Survey recommendation:** `glyph3d-makepad` with no arguments opens the wall (while `glyph3d-native` with no arguments keeps its TUI launcher).
- **Is it right?** **Mostly right, with two crucial guards:**
  1. Confirm with Ivan whether `cargo glyph run` (which invokes `glyph3d-native`, `glyph3d-rs:AGENTS.md`) stays untouched or whether a `cargo glyph wall` / `makepad` alias is wanted. Keeping `glyph3d-native` untouched preserves every existing gate and muscle memory.
  2. **During Steps 1–5**, keep the Wall behind `--wall` and require `--termd-namespace <ns>` (defaulting in dev to `wall-rs` or `wall-dev`, **never** `wall`), flipping no-args `glyph3d-makepad` to `wall` only as the final commit of Step 6. Otherwise an unattended test launch with no args will connect as `own` or `attach` to Ivan's live `wall` namespace and demote/resize his active browser sessions (`survey.md:197-199`, `PROTOCOL.md:290-335`).

---

## 3. Unnamed Risks (Especially Unattended-Run Stoppers)

### Risk 1: Cross-Repo `g3d` / `g3d-wire` Build, Edition 2024, `relax`, and Blocking Socket Deadlock (STOPPER in Step 1)
- **Evidence:** `[Verified: viz-web/glyph3d-js:Cargo.toml:9-11, crates/g3d/Cargo.toml:1-27, crates/g3d/src/start.rs:43-44, crates/g3d/src/client.rs:50-56, 245-263; glyph3d-rs:Cargo.toml:1-25, glyph/src/prove_relax.rs:28-31; makepad:libs/relax/compiler/Cargo.toml:4-5]`
- **Why it stops an unattended run:**
  1. Path & workspace mismatch: `viz-web/glyph3d-js` is not at `../glyph3d-js` (it is at `../../../viz-web/glyph3d-js` from the `.claude/worktrees/work` worktree!), uses `[lints] workspace = true` and `edition.workspace = true` (`2024`), and uses Rust 2024 `let`-chains. Adding path dependencies outside the repo breaks hermetic worktrees, `workstation` paths, and `relax` (edition 2021 parser/JIT).
  2. Threading/blocking mismatch: `g3d::Client::request_raw` blocks synchronously on `UnixStream::read_exact` until the matching `REPLY` arrives, buffering all intervening `OUTPUT` and `EVENT` frames in memory. If the UI thread calls `client.request("key", ...)` or `client.request("resize", ...)` while 10 terminals stream output, either the UI stalls on socket I/O or a reader thread holds `&mut Client` and locks out the UI thread!
- **Fix:** Implement `crates/termd-client` directly in `glyph3d-rs` (Protocol 1.5 framing is 60 lines; JSON messages are ~350 lines of `serde`), split the `UnixStream` via `try_clone()` into a reader thread (pushing `OUTPUT`/`REPAINT`/`EVENT`/`REPLY` into a channel + waking Makepad via `SignalToUI::set_ui_signal()`) and a non-blocking writer handle.

### Risk 2: Makepad `KeyDown` vs `TextInput` Split vs `termd`'s W3C `Op::Key` (STOPPER in Step 1)
- **Evidence:** `[Verified: viz-web/glyph3d-js:crates/g3d-wire/src/msg.rs:180-203, crates/g3d-termd/src/keys.rs:211-250, docs/termd/PROTOCOL.md:541-629; makepad:platform/src/event/keyboard.rs:7-9, platform/hal/src/backend/apple/appkit/keys.rs:50-186, platform/hal/src/backend/apple/appkit/ime.rs:12-82, 112-124, platform/hal/src/backend/apple/appkit/classes.rs:638-680; glyph3d-rs:makepad/src/viewport.rs:166-189, makepad/src/main.rs:100-108]`
- **Why it stops an unattended run:**
  - `Glyph3dViewport` never calls `cx.show_text_ime(...)`, so macOS AppKit skips `NSTextInputContext::handleEvent:` (`classes.rs:669`) and **never delivers `Hit::TextInput`**.
  - Once `cx.show_text_ime(...)` IS called, AppKit delivers **both** `Hit::TextInput` (inside `insertText:`, carrying layout-translated text/Option-chars/IME commit, with no `KeyCode` or `modifiers`) **and** `Hit::KeyDown` (carrying US scan-code `KeyCode` + `modifiers`, with no `text` or logical `key`, except during active marked-text composition). Meanwhile ISO/JIS keys like `0x0a` (`§`/`±`), `Yen`, `Eisuu`, `Kana`, `F13–F20` return `None` in `get_event_keycode` (`appkit/keys.rs:64, 148-168`), emitting *only* `TextInput`!
  - Furthermore, `viewport.rs:166, 170` intercepts `Backtick` and `Escape` for 3D mouse-look, and `main.rs:100-108` intercepts `F1`, `F6`, `F7`, `F8` globally!
- **Fix:** Settle keyboard encoding in a 150-line **Pre-Step-1 Spike**: either (Option A, simplest & proven in `MpTerm`, `makepad:apps/terminal/src/widget.rs:2088-2165`) use `glyph-vt`'s Ghostty key encoder (`term/key_encode.rs`) against the terminal's live modes (`kitty_flags`, `DECCKM`, `DECKPAM`, `modify_other_keys`, `macos_option_as_alt`) and send raw `INPUT` (`0x81`) frames + `PASTE` ops, OR (Option B) coalesce `TextInput` + `KeyDown` within the same HAL turn to synthesize W3C `{ code, key, mods, text }` for `Op::Key`. Gate out `Backtick`/`Escape`/`F1/F6/F7/F8` shortcuts whenever a terminal has focus.

### Risk 3: Emulator `outbound` Replies & `soft_reset` Drift Corrupting `termd` State (Silent Bug in Step 1/3)
- **Evidence:** `[Verified: viz-web/glyph3d-js:docs/termd/PROTOCOL.md:1038-1040, crates/g3d-termd/src/repaint.rs:14-129; makepad:apps/terminal/src/term/terminal.rs:75, 82, 172, 1604-1620, 1688, 1866-1880]`
- **Why it bites:**
  - `termd`'s own `libghostty-vt` instance answers terminal queries (`DA1`, `DSR`, `OSC 10/11 ?`, `DECRQM`, `XTVERSION`). If a client forwards `term.take_outbound()` into `INPUT`, every `vim`/`fish`/`tmux`/`nvim` startup receives **duplicate query responses** seen as garbage keystrokes. If the client never calls `take_outbound()`, `self.outbound` leaks unboundedly.
  - Every `view` scrollback op and reconnect sends a `REPAINT` starting with `CSI ! p` (`DECSTR`). Because `term::Terminal::soft_reset` omits `modify_other_keys`, `current_hyperlink`, mode 2027, and full Kitty keyboard stack clear, scrolling back and returning to live silently mutates terminal modes.

### Risk 4: Slug Non-Zero Winding (`abs(coverage)`) & Dark sRGB Crush (`pow(2.2)`) (Visual Failure in Step 3)
- **Evidence:** `[Verified: viz-web/glyph3d-js:packages/glyph3d-core/src/core/glyphMaterial.js:374-378, docs/architecture/rendering.md:198-216; glyph3d-rs:makepad/src/g3_passes.rs:841, 862-863, 900-904, crates/glyph-field-derived/shaders/glyph_field_derived.wgsl:394, 415-416]`
- **Why it bites:**
  - Without `abs(coverage)` in the Slug fragment shader, Monaspace Neon/Argon reverse-wound accents and overlapping variable-font contours evaluate to negative winding and clamp to `0.0` (holes/missing diacritics). Note: keep `glyph_field_derived.wgsl` untouched unless `pixel-ab` stays byte-equal; put `abs(coverage)` in the new Makepad cell shader!
  - Decoding sRGB colors with `pow(c, 2.2)` into `Rgba16F` and re-encoding with piecewise IEC `linear_to_srgb` (`12.92 * l` below `0.0031308`) shifts every dark terminal palette color downward by 20–30% (`#1a1b26` -> `#13141d`) and thins light-on-dark stems unless matched (exact `srgb_to_linear` or direct sRGB blending as in `glyphMaterial.js`).

### Risk 5: Atlas Bake Pipeline Gap & Missing Runtime Unicode/Cluster Fallback (STOPPER in Step 3)
- **Evidence:** `[Verified: glyph3d-rs:tools/export-atlas.mjs:87-100, makepad/src/frame_feed.rs:30, makepad/src/g3_passes.rs:1488-1494; viz-web/glyph3d-js:packages/glyph3d-core/src/shaping/LiveSlugAtlas.js:59-71, shaping/cellSprites.js:48-54; makepad:apps/terminal/src/term/page.rs:27]`
- **Why it stops an unattended run:**
  1. `tools/export-atlas.mjs` cannot encode fonts; an agent told "bake Monaspace Neon and Argon, four faces each, cell sprites" in Step 3 will discover there is no font->Slug compiler in `glyph3d-rs`. It must either run a generator script against `viz-web/glyph3d-js`'s `FontChain` + `SlugEncoder` + `cellSprites` (read-only import, writing output `.bin` into `glyph3d-rs`) or port quadratic contour extraction (`ttf-parser` / `skrifa` + `cellSprites`) in Rust.
  2. `glyph-vt` emits `CellContent::Codepoint(char)` and `CellContent::Cluster(ClusterId)` (`page.rs:27`). Meanwhile `frame_feed.rs:30` hardcodes `ATLAS_GENERATION: u64 = 1` and `g3_passes.rs:1488-1494` calls `discard_cpu_after_upload()`. Any terminal printing CJK, math symbols, or combining marks outside the prebaked set will render `.notdef` blanks unless either (a) dynamic atlas append (bumping `atlas_generation` + `swash`/`skrifa` fallback shaping) is added, or (b) the prebake explicitly covers the target blocks and defines deterministic `.notdef`/replacement behavior.

### Risk 6: Orthographic Camera Breaking `GlyphScene` Picking & LOD (Bug in Step 2)
- **Evidence:** `[Verified: glyph3d-rs:native/src/glyph_scene/pick.rs:257-285, native/src/glyph_scene/cull.rs:137, 278-294, makepad/src/g3_passes.rs:601-619, 745-769]`
- **Good news verified:** The Apple Silicon analytic pixel-footprint Jacobian in `g3_passes.rs:601-619, 745-769` uses projective derivatives (`clip.w = 1, foot_cu.w = 0, foot_cv.w = 0` under orthographic projection -> `w2 = 1`, `det = pu.x * pv.y - pv.x * pu.y`), which **works cleanly without division by zero**.
- **Bad news verified:** `pick.rs:257-285` (`pixel_ray`) and `cull.rs:278-294` both assume perspective projection. Do not route Wall pointer events through `GlyphScene::pick`; implement direct 2D world-space hit-testing (`screen_px -> world_xy -> top zOrder rect -> (col, row)`).

---

## 4. Judgment on the Order of Work, Pre-Flight Spikes, and Per-Step Checks

### Is Step 1 the right first proof?
**Yes in goal ("one interactive terminal on screen, rendering on demand"), NO in internal sequence.** Trying to build Step 1 before settling the emulator crate boundary, non-blocking `termd` client, and Makepad key/IME translation invites compounding hacks. And postponing the ~4.5k lines of pure `wall-*` state/layout/group/store logic until Steps 2, 4, and 5 misses an easy early win: that pure logic has **17 existing headless `bun:test` spec files** (`survey.md:57`) that translate cleanly into fast, deterministic `cargo test` unit tests!

### Two Small Front-Loaded Spikes (Run Before Step 1)

1. **Spike A — Input & `termd` Wire Proof (~200 LOC):**  
   In a scratch binary or `crates/termd-client` test against an isolated `g3d-termd --namespace wall-carbon-test` instance (cleaned up on exit), verify:
   - Non-blocking reader thread + writer channel (`hello`, `create`, `subscribe(own, raw)`, `resize`, `kill`).
   - Makepad `cx.show_text_ime(...)` event sequence on macOS for: plain ASCII, `Shift+symbol`, `Ctrl+C`, `Option+key`, `ArrowUp` (normal vs `DECCKM`), `Backtick`, `Escape`, and Kitty `CSI u` (`CSI > 1 u`), proving zero duplicate characters and zero dropped keys.
2. **Spike B — Atlas, Sprites & Shader Color/Winding Proof:**  
   - Decide & verify the atlas generator path (Rust `ttf-parser`/`skrifa` + `cellSprites` port in `tools/`, vs read-only Node generator), baking Monaspace Neon (4 faces) + Argon (4 faces) + shared NerdFont/sprites/emoji within `u16` glyph IDs.
   - Verify `abs(coverage)` on Monaspace accented/overlapping glyphs (`é`, `å`, `≠`, `===` ligatures if enabled) and exact IEC sRGB round-trip (`#1a1b26` -> `#1a1b26` ± 1 LSB).

### Recommended Revised Order of Work & Concrete Automated Checks

Every step below has an **automated self-check** executable by an unattended agent without human eyes:

| Step | Scope | Automated "Done" Gate (Agent Self-Check) |
|---|---|---|
| **Step 0** | **Foundations in workspace (`crates/termd-client`, `crates/glyph-vt`, shader/headless fixes)** | `cargo glyph test --inner` green + new unit tests: (1) all 29 `vt-conf/*.vt` fixtures match cell-for-cell before vs after `termd` `REPAINT` with `outbound.is_empty()`, (2) `termd-client` integration test spawns isolated `--namespace wall-test-<pid>`, runs shell `printf`, resizes, rides a simulated reconnect, and stops daemon cleanly. |
| **Step 1** | **One terminal on screen in `glyph3d-makepad --wall` + render-on-demand** (cell pass, fills, cursor, IME + keys, isolated namespace) | Scripted offscreen/remote smoke test: launch `glyph3d-makepad --wall --termd-namespace wall-test-<pid>`, run `printf '\e[48;2;26;27;38m\e[38;2;192;202;245mHELLO\n'`, assert: (1) snapshot background RGB equals `(26,27,38) ± 1`, (2) `HELLO` cells contain foreground ink, (3) typing keys via remote produces shell echo, (4) HAL frame counter advances `0` times during a 500 ms idle sleep. |
| **Step 2** | **Pure `wall-core` crate (`layout`, `splitTree`, `snap`, `store`, `keymap`, `chord`, `contextMenu`, `wallCommands` model) + Orthographic Canvas** (pan/zoom, spawn/close/move/resize, title bars, z-order, drafting paper, JSON persistence) | (1) Port all 17 `wall-*.test.mjs` suites as Rust unit tests in `crates/wall-core` (`cargo glyph test rust` green). (2) Smoke test: spawn 3 terminals, pan/zoom ortho camera, click off-center terminal `(col, row)`, verify exact hit-test + Z-raise + JSON state file reload after restart & dead-session pruning. |
| **Step 3** | **Text & Terminal Fidelity** (Monaspace Neon/Argon 4-face atlas + mode-3 cell sprites, underline styles/colors, dim/strike/overline, selection + `copyOnSelect` + chunked/safe paste + OSC 52, mouse reporting, 3D scrollback ring + `termd` `view` scrollback) | (1) Sprite & style unit/snapshot test: render box-drawing `┌─┐│└┘` (contiguous 0-gap joins across cells), curly colored underline `CSI 4:3m CSI 58:2::255:0:80m`, bold/italic faces, and Monaspace `é`/`å`. (2) Automated check of SGR-pixel/cell `Op::Mouse` encoding, `view` scrollback (`flags & 1` hides cursor), and `unsafe_paste` rejection notice on multiline paste. |
| **Step 4** | **Groups, Tiles, Splits, Fullscreen, Snap, Lasso & Hold-to-Close** | Deterministic state + geometry assertions through CLI/remote verb runner covering group creation (`tile`, `freeform`, `split`), split-pane divider drag + `neighbor` focus nav, fullscreen + `fullscreenText` zoom, and Escape/restore symmetry. |
| **Step 5** | **Command Bus (`INTERFACE-SHELL-SPEC` aligned) + Wall Panels & Overlays** (sidebar, Cmd-K palette, status chip/window, settings, help, context menu, map labels) | Bus parser/executor round-trip tests (every UI button/menu/chord emits a bus line; zero panics on arbitrary input; typing in console/palette/search never leaks keys to focused terminal) + remote UI interaction check. |
| **Step 6** | **Live `gomuxai` Bus, Attach & Agent View + Flip Default** | Live-tail reader test on synthetic incremental JSONL (`add` + `update` folding, `256 KB` tail window, `loadEarlier`, scrollback line-anchor jump, `shoveAside` footprint restore), `cargo glyph test` (full 15-gate merge bar) + `cargo glyph prove --since work` green, then flip no-args `glyph3d-makepad` default. |

---

## 5. What Is Missing From the Plan That a Finished Port Needs

1. **`termd` Daemon Lifecycle, Auto-Start, Restart-Riding, and `lagged` Resync** `[Verified: viz-web/glyph3d-js:docs/termd/PROTOCOL.md:945-1015, crates/g3d/src/start.rs:25-120]`  
   - When the user launches `glyph3d-makepad` with no args after a fresh boot, `g3d-termd` may not be running yet: the native app needs `connect_or_start` (`find_termd` binary discovery + socket readiness wait, ported cleanly without `let`-chains).
   - When `g3d-termd` restarts (`g3d restart` while holders stay alive), it sends `ERROR` (`code: "restart"`) and comes back with a **new `instance` UUID**. The client must reconnect, re-`subscribe` every terminal (re-claiming `own`), and re-prune only if `instance` lost sessions.
   - When a fast producer (`yes`, `cat huge.log`) overflows `termd`'s ring, `termd` emits `EVENT` `lagged` (`PROTOCOL.md:945-965`): the client must request a fresh `repaint` so its client-side emulator resynchronizes.

2. **Two Distinct Scrollback Systems in JS (`TerminalGrid` 3D Row Ring vs `termd` `view`)** `[Verified: viz-web/glyph3d-js:packages/glyph3d-core/src/collections/TerminalGrid.js:155-176, docs/termd/PROTOCOL.md:503-527]`  
   - Survey §7 Step 3 only mentions `scrollback (view)`. In JS, `TerminalGrid` *also* keeps `rows + depthMax` (80) rows in a ring (`detectVerticalScroll`) that can recede into `-Z` depth pages, while `termd`'s `view` op scrolls into `termd`'s server-side scrollback buffer (returning a `REPAINT` with `flags & 1 != 0`, during which the live cursor must be hidden and scroll-lock indicator shown). Decide explicitly whether RS ports `view` scrollback only (simpler for a pure 2D ortho wall) or both.

3. **Clipboard Security, Primary Selection (`copyOnSelect`), and Chunked/Safe Paste** `[Verified: viz-web/glyph3d-js:wall/TerminalWall.js:187-195, docs/termd/PROTOCOL.md:631-662, makepad:apps/terminal/src/term/osc.rs:200-260]`  
   - `termd`'s `paste` op caps single frames, supports streaming (`more: true`), and refuses multiline/control-char pastes unless `allow_unsafe: true` (`code: "unsafe_paste"`), which in the Wall surfaces a notice/confirmation banner (`TerminalWall.js:193-195`).
   - `copyOnSelect` + middle-click `selectionClipboard` (`TerminalWall.js:188-192`) and `OSC 52` clipboard write/query policy (`term/osc.rs` parses OSC 52; policy must wire into Makepad's `cx.copy_to_clipboard`).

4. **Live `status_follow` Integration (`TermdStatus`)** `[Verified: viz-web/glyph3d-js:wall/TerminalWall.js:139-141, 229, docs/termd/PROTOCOL.md:697-760]`  
   - Beyond listing terminals, `status_follow` drives: (a) live terminal `title` and `pwd`/`cwd` (used by `WallNames` `shortTitle` at `TerminalWall.js:96-98` AND by `WallAgentViews` session auto-pairing at `WallAgentViews.js:69-72`!), (b) scrollback row counts and alt-screen flags, and (c) daemon health/holder loss warnings in the topbar chip and status window.

5. **Live Font-Family Switching & Ligature/Variation Knobs** `[Verified: viz-web/glyph3d-js:wall/wallConfig.js, packages/glyph3d-core/src/fonts/families.js:8-82]`  
   - `wall.config fontFamily` switches live between `Monaspace Neon`, `Monaspace Argon`, and `Cousine`. If all three families (4 faces each) are prebaked into one combined Wall atlas with a per-family face table (`[family][bold | italic<<1]` -> glyph lookup table), switching font family is an instant uniform/LUT swap with zero atlas reload.

6. **Live `gomuxai` Event Stream & Fleet Bus Architecture** `[Verified: viz-web/glyph3d-js:wall/WallAgentViews.js:24-130, wall/TerminalWall.js:27, 233; glyph3d-rs:native/src/agent_transcript.rs:35-39]`  
   - In JS, the Go relay serves `agentSessions/*`, `agentEvents/read`, `agentEvents/follow`, and `WallBus` (`agent fleet --json`). Skipping the Go relay (§4) means **the Rust app must replace the relay's `gomuxai` RPCs natively** — spawning/polling `/Users/lugo/.local/bin/agent sessions --json` / `agent fleet --json` on a background worker and tailing `agent transcript` / transcript files incrementally without blocking the UI thread, plus porting `WallAgentViews`'s `shoveAside` canvas-footprint displacement (`TerminalWall.js:210-213`).

7. **Persistence Path, Atomic Writes, and Multi-Window / Namespace Scoping** `[Verified: viz-web/glyph3d-js:wall/WallStore.js:73-109]`  
   - In the browser, `WallStore` scopes keys by `relayPort` in `localStorage`. Natively (with no relay port!), state must be scoped by **`termd` namespace** (e.g., `~/.config/glyph3d/wall.<namespace>.json` or XDG state dir), written atomically (`tmp` + `rename`) with debouncing during window drags so pan/move at 120 Hz never hammers disk I/O.

8. **Window / DPI / Gesture Details in Makepad** `[Verified: viz-web/glyph3d-js:wall/TerminalWall.js:36-40; makepad:platform/hal/src/backend/apple/appkit/classes.rs:512-529]`  
   - Trackpad pinch-to-zoom vs two-finger pan vs wheel scroll (forwarding wheel to terminal when hovered/alt-screen/mouse-reporting vs zooming canvas on `Cmd+wheel` / pinch), Retina `dpi_factor` scaling on `EDGE_GRAB_PX = 7` and `FRAME_MAX_LINE_PX = 28`, cursor icon changes (`ResizeNWSE`, `Text`, `Hand`), and the animated hold-to-close ring (`BarButtons` — the one UI element that legitimately requests `new_next_frame()` while held, then stops on release).
