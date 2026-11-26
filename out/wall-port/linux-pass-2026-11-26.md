# Linux Verification Pass (Rebuilt `Wall` Window + `wallctl`) — 2026-11-26

- **Host:** `workstation` (CachyOS Linux, x86_64, 32 threads)
- **GPU / Driver:** NVIDIA GeForce RTX 5090 (`0x10de:0x2b85`), Vulkan `620.73.04` (`vulkan-nvidia`)
- **Desktop Session:** KDE Plasma 6 Wayland (`kwin_wayland`, `WAYLAND_DISPLAY=wayland-1`, `XWayland` on `DISPLAY=:0`)
- **Toolchain:** `rustc 1.98.1` / `clippy 1.98.1`
- **Base Commit:** `origin/wall` at `8d9d096` (`wall plan: where the milestones stand`), which includes `c703285` (`wall: Linux Wayland teardown, non-macOS chords, Wayland clipboard`) carrying forward Pass 1's window fixes onto the rewritten `wall_core::wall::Wall` + `Sessions` + `wall-control` host in `native/src/wall/app.rs`.

---

## 1. Build, Static Checks, and Test Suites

| Check / Target | Result | Details |
| :--- | :--- | :--- |
| `cargo build --release -p glyph3d-native -p wall-control` | **PASS** | `0` warnings (`glyph3d-native` + `wallctl`) |
| `cargo glyph gate cargo-clippy` (`--workspace --release --all-targets`) | **PASS** (after fix) | `rustc 1.98.1` flagged `px.chunks_exact(4)` (`clippy::chunks_exact_to_as_chunks`) in `native/src/wall/renderer.rs:2375`; fixed via `px.as_chunks::<4>().0.iter()` (`0` warnings) |
| `cargo glyph gate cargo-check-no-ui` | **PASS** | `0` warnings (`--no-default-features` across `glyph3d-native`) |
| `cargo glyph gate cargo-doc` | **PASS** | `0` rustdoc warnings |
| `cargo glyph gate orphans` | **PASS** | `13` packages, `249` files, `0` unreachable module files |
| `cargo test --release -p term-cells` | **PASS** | `2` passed |
| `cargo test --release -p termd-client` | **PASS** | `14` unit + `11` live `g3d-termd` integration (`25` total) |
| `cargo test --release -p glyph-vt` | **PASS** | `219` unit + `3` fuzz (`10,000` cases) + `12` seam + `1` conformance (`235` total) |
| `cargo test --release -p glyph-font` | **PASS** | `8` atlas + `3` emoji/cells + `6` sprites/clusters + `1` doctest (`18` total) |
| `cargo test --release -p wall-core` | **PASS** | `213` unit + `24` pointer + `19` scenario (`256` total) |
| `cargo test --release -p wall-control` | **PASS** | `7` socket/UID-peercred/`wallctl` integration tests (`SO_PEERCRED` on Linux) |
| `cargo test --release -p agent-feed` | **PASS** | `16` passed |
| `cargo test --release -p glyph3d-native --lib wall::` | **PASS** | `63` unit + Vulkan GPU offscreen tests (sparse emoji growth, sRGB round-trip, dirty-row uploads, shapes, labels, paper, egui panels) |

**Non-wall repo notes (left untouched):**
- `glyph/src/relax.rs:182` (`workspace_packages_are_the_members_alone`): still hardcodes the 6 pre-wall workspace crates rather than the 13 current workspace members.
- `pixel-ab`: 8 of 10 existing `GlyphScene` baselines show 1–8 px sub-LSB drift (max channel delta 1–2) under NVIDIA Vulkan driver `620.73.04` vs `615.78.08`; per `out/WALL-PLAN-2026-11-26.md`, no golden adoption was performed.
- `wgpu_core::instance` logs `WARN ... Feature MAPPABLE_PRIMARY_BUFFERS enabled on a discrete gpu` from shared `native/src/gpu.rs` initialization on the RTX 5090.

---

## 2. Rebuilt Window Live Runs: Wayland, X11, and `wallctl`

All live runs used isolated scratch `g3d-termd` directories (`G3D_TERMD_DIR=/tmp/g3d-...`, `0700` permissions); every scratch daemon was stopped via `g3d --ns ... stop` and removed afterward, leaving live namespaces (`wall`, `agents`, `default`) untouched.

### 2.1 Counted Live Runs (`--frames 60`, `GLYPH_WALL_TYPE`)
- **Wayland (`WAYLAND_DISPLAY=wayland-1`)**:
  - Command: `./target/release/glyph3d-native --wall --wall-ns probe --frames 60` with `GLYPH_WALL_TYPE=$'echo well hello wayland\n'` and `GLYPH_WALL_SHOT`.
  - Startup: `wall sparse emoji archive: 3985 cells (136x128, 4 mips), 1501 codepoints, 4166 sequences; parse 2.1 ms (0 decoded)` — confirms the baked 378 MB `emoji-sheet.cache` / `Atlas::load` is no longer loaded by the wall.
  - Output & Exit: `t1` (`100x30`) ran `echo well hello wayland` and printed `well hello wayland`; process exited cleanly (`exit=0`) after 60 frames with **zero coredumps** (`coredumpctl` verified).
- **X11 / XWayland (`WINIT_UNIX_BACKEND=x11 DISPLAY=:0 XAUTHORITY=...`)**:
  - Same `--frames 60` run with `GLYPH_WALL_TYPE=$'echo well hello x11\n'`.
  - Sparse emoji parse `2.7 ms (0 decoded)`; `t1` echoed `well hello x11`; exited cleanly (`exit=0`), **zero coredumps**.

### 2.2 New Shell Door (`wallctl`) on Wayland and X11
Executed against live uncounted `--wall --wall-ns probe` sessions on **both** Wayland (`wayland-1`) and X11 (`WINIT_UNIX_BACKEND=x11`):

1. `./target/release/wallctl --ns probe wall.spawn` → `OK: spawned pending` (`t2` `100x30` spawned and flowed beside `t1`).
2. `./target/release/wallctl --ns probe wall.split right` → `OK: split t2 right: pending` (`group 1` created with `layout: "split"`, `dir: "row"`, `ratio: 0.5`, rewrapping `t2` and `t3` to `49x30` side-by-side).
3. `./target/release/wallctl --ns probe wall.fit` → `OK: fitting 3 terminal(s)` (eased camera to frame `t1` + `group 1`).
4. `./target/release/wallctl --ns probe app.text t1` → returned `ivan@workstation ~ $`.
5. `./target/release/wallctl --ns probe app.shot /abs/path.png` → `OK: wrote /abs/path.png`.
   - **Visual inspection of saved PNGs (`1600x1000` RGBA)**:
     - Left: ungrouped `t1` (`~`) with dark slate title strip (`#1b2230`), subtle unfocused border, crisp prompt colours, and hollow green cursor.
     - Right: `group 1` outer container frame with dark green group header (`group 1`), containing left unfocused pane `t2` (`49x30`, hollow cursor) and right focused pane `t3` (`49x30`, bright mint-green `#6ee7a0` border and solid green block cursor), over the minor/major dark grid paper.
     - `Bgra8UnormSrgb` → RGBA channel swap in `app.rs:788-798` produced exact sRGB colours on both Wayland and X11 Vulkan surfaces.
6. `./target/release/wallctl --ns probe app.quit` → `OK: quitting`; `glyph3d-native` tore down `control` and `gfx` inside `App::exiting` while `wl_display` remained alive and exited with code **`0`** (zero crashes on both Wayland and X11).

*(Note: `App::scripted()` triggers `GLYPH_WALL_TYPE` at `self.frames == 30`, which is driven automatically in `--frames N` mode; in uncounted `ControlFlow::Wait` mode the loop sleeps once idle after initial redraws, so `wallctl` / `g3d send` is the right live input driver.)*

---

## 3. `WallStore` Persistence Paths on Linux

Verified all four branches of `store_dir()` (`native/src/wall/app.rs:224-234`) + `App::open`:

1. **`G3D_TERMD_DIR` set (non-empty):**
   - Landed at `<G3D_TERMD_DIR>/wall-<ns>.json` (e.g. `/tmp/g3d-linux-wayland-.../wall-probe.json`, mode `0600`), alongside `termd-<ns>.sock` and `wall-<ns>.sock`.
   - Verified JSON contents persisted `positions` (`t1`, `t2`, `t3`), `groups.g-1` (`layout: "split"`, `tree` with `dir: "row"`, `ratio: 0.5`, `a: t2`, `b: t3`), `sidebar`, `status`, `config`, `settings`, `help`, `fullscreen`, `fullscreenText`, and `agentViews`.
2. **`G3D_TERMD_DIR` unset, `XDG_STATE_HOME` set:**
   - Landed at `$XDG_STATE_HOME/glyph3d/wall/wall-<ns>.json` (directories created mode `0700`, file mode `0600`), while `termd` and `wall` sockets landed in `$XDG_RUNTIME_DIR/g3d/` (`/run/user/1000/g3d/`).
3. **Both `G3D_TERMD_DIR` and `XDG_STATE_HOME` unset (Linux default):**
   - Landed at `$HOME/.local/state/glyph3d/wall/wall-<ns>.json` (`0700` directories, `0600` file).
4. **`--wall-test-pattern` (`namespace: None`):**
   - Uses `Wall::in_memory(rules)` and writes no file.

---

## 4. Wayland Copy & Paste (Both Directions)

Tested live on `WAYLAND_DISPLAY=wayland-1` (KDE Plasma 6 `kwin_wayland` + `klipper` + two independent `winit`/`WallUi` Wayland client windows) and across X11 (`WINIT_UNIX_BACKEND=x11`):

1. **Paste into the wall (`External Wayland / Klipper / XWayland → Wall`):** **PASS**
   - Setting clipboard text externally on Wayland (`org.kde.klipper` `setClipboardContents`, as well as from an X11 `WallUi` client via XWayland) and invoking `wallctl --ns probe wall.paste` (`App::read_clipboard` → `WallUi::clipboard_text` → `smithay-clipboard`) delivered the exact text into `t1`, confirmed via `wallctl --ns probe app.text t1`.
2. **Copy out of the wall (`Wall → Wayland`) and Wayland seat/serial behaviour:**
   - Verified via `WAYLAND_DEBUG=1` wire trace that `App::write_clipboard` → `WallUi::set_clipboard_text` (`native/src/wall/app.rs:413-417`, `ui.rs:166-168`) → `egui_winit::State::set_clipboard_text` → `smithay-clipboard 0.7.3` creates a `wl_data_source` offering `text/plain;charset=utf-8`, `UTF8_STRING`, and `text/plain`, and sends `wl_data_device.set_selection(source, latest_serial)`.
   - **Two Wayland protocol nuances worth noting for `wallctl` scripting on Linux:**
     - **Seat focus required (`wl_data_device`):** On Wayland, `smithay-clipboard` requires the wall window to currently hold keyboard focus (`seat.has_focus == true`) for both `paste` (`load_selection`) and `copy` (`store_selection`). Calling `wallctl wall.paste` while another Wayland window holds seat focus returns `None` (`client doesn't have focus`) without crashing; as soon as KWin returns focus to the wall window, `wallctl wall.paste` succeeds immediately.
     - **KWin `wl_keyboard.modifiers` serial on focus entry:** When a window gains focus on KWin Wayland, KWin sends `wl_keyboard.enter(fresh_serial)` (e.g. `51892`) followed ~40 µs later by `wl_keyboard.modifiers(last_modifier_serial)` (e.g. `51752`). `smithay-clipboard 0.7.3` (`src/state.rs:520-527`) overwrites `seat_state.latest_serial` on `WlKeyboardEvent::Modifiers` with that older serial. In **interactive copy** (mouse click/drag to select text + `Ctrl+Shift+C` or `copyOnSelect = clipboard`), `wl_pointer.button` and `wl_keyboard.key` supply a fresh input serial (`> enter_serial`), so KWin accepts `set_selection`. However, if a window has just opened or regained focus with zero subsequent key/mouse presses inside it, calling `wallctl wall.copy` from an external shell sends the stale `modifiers` serial and KWin ignores `set_selection` until the next key or button press inside the wall window.
   - *(Minor non-default-feature note: under `--no-default-features --features windowed` where `egui-ui` is disabled, `write_clipboard` falls back to `arboard::Clipboard::new().and_then(|mut c| c.set_text(text))`, which drops the temporary `arboard::Clipboard` immediately and loses X11 selection ownership on Linux; default builds enable `egui-ui` and keep `egui-winit`'s `Clipboard` alive inside `Gfx`.)*

---

## 5. HiDPI Scale Factor

- **Physical monitor topology on `workstation` (`kscreen-doctor -o`):**
  - `HDMI-A-1`: **enabled**, connected, `1920x1080@74.97Hz`, **`Scale: 1.0`**.
  - `DP-3`: **disabled** (connected, `3840x2160`, configured `Scale: 1.5` — physical monitor configuration left untouched).
- **Live fractional (`1.5x`) and integer (`2.0x`) HiDPI verification (`WINIT_X11_SCALE_FACTOR=1.5` and `2`):**
  - Launched `--wall --wall-ns probe` with `GLYPH_WALL_HUD=1`, ran `wall.spawn`, `wall.split right`, `wall.fit`, and `app.shot`, and visually inspected the rendered PNGs (`1920x1022` physical surface clamped to the 1080p work area = `1280x681` logical at `1.5x` and `960x511` logical at `2.0x`).
  - **Findings:**
    - `App::resize` (`width / sf`, `height / sf`) and `App::draw` (`described.camera.zoom as f32 * sf`) agreed to the pixel: `wall.fit` produced identical **`6.35 px/u`** physical camera zoom on `1920x1022` under both `1.5x` and `2.0x`.
    - Terminal Slug glyphs, `1px * sf` / `2px * sf` borders, minor/major grid lines (`line_px * sf`, `min_px * sf`), rounded map-label pills (`corner_radius_px = 4.0 * sf` triggered automatically at `960x511` / `1280x681` logical widths when split panes narrowed below the logical title-bar floor), and the `egui` `Wall Debug HUD` all scaled crisply and proportionally at both `1.5x` and `2.0x`.

---

## 6. 8-Terminal Frame Time on RTX 5090

### 6.1 Live Windowed Session (8 × `100×30` Terminals = 24,000 Cells)
- Spawned `t1`..`t8` against a scratch `g3d-termd`, populated all 8 live shells with dense ANSI truecolor/box-drawing/braille/emoji (`term-test-pattern.vt`) and coloured `git log --graph`, ran `wall.fit` (`zoom 3.47 px/u` on `1600x1000`), and visually verified via `app.shot` + `GLYPH_WALL_HUD=1` (`uploads drawn 8 | uploaded 0 | rows 0 | writes 0` once settled).
- **180 live presented frames (`--frames 180`, `PresentMode::Fifo` VSync on `74.97 Hz` `HDMI-A-1`):**
  - **Wayland:** `2,529 ms` total wall-clock including Vulkan init, session attach, and teardown (`180 / 74.97 Hz = 2,401 ms` VSync interval → **`13.34 ms/frame`**, `0` missed vblanks).
  - **X11:** `2,547 ms` total wall-clock including startup and teardown (**`13.34 ms/frame`**, `0` missed vblanks).

### 6.2 Uncapped GPU + CPU Benchmark (RTX 5090, Vulkan Hardware Timestamps)
Measured 300 frames (after 30 warmup frames) rendering 8 dense `100×30` `term-test-pattern.vt` terminals (`24,000` cells) + paper grid + 8 title-bar shapes + 8 title labels with continuous sub-pixel camera pan, recording CPU encode+submit time, hardware Vulkan encoder timestamps (`wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS`), and synchronous `device.poll(Wait)` completion:

| Workload (8 × `100×30` = 24,000 cells) | Viewport | CPU Encode+Submit (`p50` / `mean`) | RTX 5090 GPU Pass (`p50` / `p99`) | Sync CPU+GPU Total (`p50` / `p99`) |
| :--- | :--- | :--- | :--- | :--- |
| **Steady-State** (`0` dirty rows/frame) | `1600×1000` (`1.0x`) | `45.8 µs` / `48.0 µs` (`0.048 ms`) | **`112.2 µs`** / `117.8 µs` (`0.112 ms`) | **`199.8 µs`** / `299.7 µs` (`0.200 ms`) |
| **Active Typing** (`1` dirty row/term = `800` cells/frame) | `1600×1000` (`1.0x`) | `63.7 µs` / `69.6 µs` (`0.070 ms`) | **`112.3 µs`** / `118.3 µs` (`0.112 ms`) | **`224.1 µs`** / `244.6 µs` (`0.224 ms`) |
| **Full Churn** (`240` dirty rows = `24,000` cells/frame) | `1600×1000` (`1.0x`) | `325.0 µs` / `326.8 µs` (`0.327 ms`) | **`112.2 µs`** / `117.7 µs` (`0.112 ms`) | **`490.3 µs`** / `547.8 µs` (`0.490 ms`) |
| **4K Steady-State** (`0` dirty rows/frame) | `3840×2160` (`1.5x`) | `45.8 µs` / `47.7 µs` (`0.048 ms`) | **`224.0 µs`** / `235.1 µs` (`0.224 ms`) | **`312.7 µs`** / `1055.8 µs` (`0.313 ms`) |
| **4K Full Churn** (`24,000` cells/frame) | `3840×2160` (`1.5x`) | `326.8 µs` / `333.6 µs` (`0.334 ms`) | **`223.6 µs`** / `234.7 µs` (`0.224 ms`) | **`603.4 µs`** / `661.6 µs` (`0.603 ms`) |

Even under worst-case full-screen churn across all 8 terminals simultaneously at 4K UHD (`3840×2160`), CPU cell re-packing + upload + GPU Slug evaluation completes in **`0.60 ms` p50** (`< 5%` of a 60 Hz frame budget).
