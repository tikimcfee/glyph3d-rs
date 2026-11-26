# Native Wall Fast Loop Under `relax` (2026-11-26)

Measured on `laptop-2` (Apple M2, macOS, Metal `PresentMode::Fifo` 60 Hz) on branch `wall-font` (merged `wall` at `00bff8e`, including `d93fa82`), using stock upstream `/Users/lugo/localdev/makepad/target/release/relax` (read-only; unmodified).

> **Note on repo tooling:** `glyph/src/prove_relax.rs` and `MAKEPAD_RUN_OVERLAY` were retired on 2026-11-24 (`AGENTS.md` § "The tool"). The repo now invokes stock `relax` via `glyph/src/relax.rs` using `GLYPH_RELAX` (default `relax` on `PATH`) and `--allow-c`, with no overlay or makepad patch.

---

## How to Run

### One-command launch (`tools/wall-relax.sh`)

```sh
# Isolated scratch daemon (creates /tmp/g3d-font-$$, exports G3D_TERMD_DIR,
# targets namespace `probe`, prints ready time, stops + removes daemon on exit):
tools/wall-relax.sh

# Named namespace (uses caller's G3D_TERMD_DIR or default; does not stop daemon):
tools/wall-relax.sh --ns my-dev

# Pass relax options before `--` and wall flags after `--`:
tools/wall-relax.sh --ns my-dev --ahead=minimal -- --frames 120
```

`tools/wall-relax.sh` exports:
- `GLYPH_WALL_SHADER_DIR=<repo>/native/src/shaders` so `native/src/wall/renderer.rs` reads `wall_paper.wgsl`, `wall_shapes.wgsl`, and `wall_cells.wgsl` from disk on startup (falling back to `include_str!` if unset or unreadable).
- `RELAX_BG_TYPECK=0` by default (override with `RELAX_BG_TYPECK=1`) to avoid a warm-boot host SIGSEGV in `relax`'s `pmhost`/`const_drain` interaction (details below).

### Driving the window (`wallctl`)

```sh
# In scratch mode, tools/wall-relax.sh prints the exact G3D_TERMD_DIR to export:
G3D_TERMD_DIR=/tmp/g3d-font-<pid> ./target/release/wallctl --ns probe app.text t1
G3D_TERMD_DIR=/tmp/g3d-font-<pid> ./target/release/wallctl --ns probe app.shot /tmp/wall.png
G3D_TERMD_DIR=/tmp/g3d-font-<pid> ./target/release/wallctl --ns probe wall.snap
G3D_TERMD_DIR=/tmp/g3d-font-<pid> ./target/release/wallctl --ns probe app.quit
```

### Fast WGSL iteration without Rust rebuilds

Because `native/src/wall/renderer.rs` now honours `GLYPH_WALL_SHADER_DIR`, editing `wall_cells.wgsl` / `wall_shapes.wgsl` / `wall_paper.wgsl` only needs a **1.8 s restart of the release binary** with zero `cargo build`:

```sh
GLYPH_WALL_SHADER_DIR=native/src/shaders ./target/release/glyph3d-native --wall --wall-ns probe
```

---

## Startup & Edit-to-Seen Results

Crate graph under `glyph3d-native`: 343 crates, 5,001 files, 2.60M lines, 38 build scripts, 11 app-scope crates.

| Case | Time (save / start → seen) | Live-patched? | Terminals kept? | Notes |
| --- | --- | --- | --- | --- |
| **Cold start** (`target/relax` absent) | **367.50 s** to socket (`256.47 s` to `main`) | — | — | Build scripts + C deps `72.9 s`, load+parse `131.1 s`, app check `23.6 s`. Higher than earlier 159 s because `wall` merged `wall-core` (+4.6k lines), `panels/` (+8.5k lines), `egui`/`egui-wgpu`, and `wall-control`, plus concurrent host load. |
| **Warm start** (cache settled, `RELAX_BG_TYPECK=0`) | **8.67–16.81 s** (quiet/light host); **20.45–28.09 s** (heavy sibling builds) | — | **Yes** (`app.text t1`) | First two warm runs populate `collected_graph` (`133 s`) and the ~29 MB startup image (`150 s`); once settled, `load+parse` is `~3–5 s` and prelink is `~6–9 s`. |
| **`native/src/wall/app.rs`** (`App::draw` title bar & `App::serve_control`) | **2.47–4.44 s** | **Yes (live)** | **Yes** (no restart) | `App::draw`: `4.19 s` 1st edit (`work 3.73 s`, 5 fns + 7 inlined-closure guards), `2.47 s` repeat. `App::serve_control`: `4.39 s` patch / `4.44 s` confirmed via `wallctl app.text` reply changing live (`ERR: LIVE-APP ...`). |
| **`native/src/wall/panels/sidebar.rs`** | **1.14–3.77 s** | **Yes (live)** | **Yes** (no restart) | `rung: "patch"`, `work 0.32–2.01 s`, `0 fns installed` only because `panels/` is not mounted into `app.rs` yet (uncalled/lazy). |
| **`native/src/wall/ui.rs`** (mounted HUD `show_debug_hud`) | **5.18–8.72 s** | **Yes (live)** | **Yes** (no restart) | `rung: "patch"`, `work 5.18–6.61 s`, 2 fns installed & borrow-checked in `45 ms` when HUD active. |
| **`crates/wall-core/src/wall.rs`** (`build_frame` & `run_with_screens`) | **0.89–4.85 s** | **Yes (live, cross-crate)** | **Yes** (no restart) | `build_frame` title-bar `fill_color`: `4.85 s` 1st / **`0.89 s`** repeat (`work 177 ms`). `run_with_screens`: `2.44 s` patch / `2.53 s` confirmed via `wallctl wall.snap` changing live (`OK: snap off` → `LIVE-CORE OK: snap on`). |
| **`crates/wall-core/src/commands.rs`** (startup-boxed closures) | **0.74–4.32 s** (patch) / **restart** (effect) | **Partial** (needs restart for boxed closures) | **Yes** on restart | `register_wall_commands` patches live (`0.74 s` warm), but closures boxed into `self.router` once during `Wall::open` keep their startup closure pointer until restart. Named helpers called from handlers patch live. |
| **`native/src/shaders/wall_cells.wgsl`** | **49.0 s** (`relax` restart) / **1.83 s** (release binary restart) | **No — needs restart** | **Yes** (`app.text t1`) | `relax` *does* watch `include_str!` files (`detect 4.0 ms`) and live-patches `WallRenderer::new` in `0.82 s`, but `WallRenderer::new` only runs once at startup so live `wgpu::RenderPipeline`s are not recreated. Under `relax`, editing a `include_str!`-fallback shader also invalidates `glyph3d-native`'s startup image (`~49 s` restart); restarting the release binary with `GLYPH_WALL_SHADER_DIR` reloads from disk in **`1.83 s`** with zero Rust rebuild. |

### Terminal persistence across restarts

Verified end-to-end with a scratch daemon (`G3D_TERMD_DIR=/tmp/g3d-font-*`, `--ns probe`):
1. Sent `echo RELAX_KEEP_MARKER_42` into `t1`; `wallctl --ns probe app.text t1` returned the marker.
2. Quit Run 1 via `wallctl --ns probe app.quit`; `g3d --ns probe ls` confirmed `t1` stayed alive in `g3d-termd`.
3. Restarted under `relax` (`tools/wall-relax.sh --ns probe`) and again under `./target/release/glyph3d-native`: `wallctl --ns probe app.text t1` returned `➜  ~ echo RELAX_KEEP_MARKER_42\nRELAX_KEEP_MARKER_42\n➜  ~` in both runs.

---

## Frame Rate: `relax` vs Release Binary

Measured with counted runs (`--frames 60/120` vs `--frames 240/360`, `GLYPH_WALL_HUD=1`, `GLYPH_WALL_SPAWN=0` for 1 terminal and `GLYPH_WALL_SPAWN=7` for 8 terminals) on an unoccluded window, measuring steady-state FPS from the frame-count delta (`Δframes / Δt_draw`, cancelling startup, frame-10 shell spawn, and exit cache flush) as well as total counted-run average FPS. Display is VSync-capped at 60 Hz (`PresentMode::Fifo`).

| Runner | Terminals | Steady-State FPS (ms/frame) | Total Counted Avg FPS | Notes |
| --- | --- | --- | --- | --- |
| **Release** (`target/release/glyph3d-native`) | 1 | **59.1 fps** (`16.92 ms/f`) | 56.9 fps (240f) | VSync-locked at 60 Hz (`PresentMode::Fifo`). |
| **Release** (`target/release/glyph3d-native`) | 8 | **59.1 fps** (`16.93 ms/f`) | 46.1 fps (240f) | VSync-locked; total avg reflects spawning 7 zsh shells at frame 10. |
| **`relax`** (`--ahead=balanced`, `RELAX_BG_TYPECK=0`) | 1 | **42.2–49.6 fps** (`20.17–23.67 ms/f`) | 40.1–42.6 fps (240–360f) | `--ahead=minimal` measured `44.3 fps` (`22.55 ms/f`). |
| **`relax`** (`--ahead=balanced`, `RELAX_BG_TYPECK=0`) | 8 | **52.7 fps** (`18.98 ms/f`) | 33.4 fps (360f) | Measured across frames 120–360 after initial ~4 s 7-shell spawn + JIT tier-up burst. |

Under `PresentMode::Fifo`, an occasional JIT/GC frame crossing 16.7 ms quantizes to 33.3 ms, giving 42–53 fps steady-state with `GLYPH_WALL_HUD=1` (interactive and smooth for editing).

---

## Caveats & Relax Findings

1. **Warm-boot host SIGSEGV without `RELAX_BG_TYPECK=0` (reported to avoid surprises):**
   With background type-checking left on, warm boots reproducibly crashed in host code (`signal 11`) at `relax_compiler::jit::Unit::vtable+0x80` or `Unit::const_addr+0x114`, called from `jit::const_drain` → `owner::serve` → `owner::pump_wait_at` → `QueryLatch::wait`, while the proc-macro host (`pmhost.rs`, `proc_macro::__relax::bridge`, `syn::parse`) was active on the owner thread. `const_drain` reads the process-global `CONSTS_WANTED` queue against `cur_unit()` and only guards by `collect_epoch`, which is `0` in both the program unit and the proc-macro world's auxiliary unit — so a `Wanted` item from a background worker of the program unit gets resolved against the proc-macro world's `Unit`. Even when it did not crash, `typeck_bg` + `pmhost` stalled the owner thread for ~42 s (`typeck_bg.rs:399` 26 s + `pmhost.rs:1031` 16 s). Exporting `RELAX_BG_TYPECK=0` (now the default in `tools/wall-relax.sh`) eliminated both the SIGSEGV and the stall across 10+ consecutive runs and live patches.
2. **Non-fatal third-party Relax compile notices:** When background typeck is enabled, `bevy_transform 0.20.0-rc.2` (`E0477`), `lock_api 0.4.14`, `typewit 1.15.2`, and `serde 1.0.229` emit background gap notices; none affect wall execution.
3. **macOS window occlusion:** When a maximized/fullscreen app completely covers the `1600×1000` background wall window, macOS suppresses `drawRect:` and `Surface::get_current_texture()` returns `Occluded`, so `App::draw()` returns before `pending_shot` and `self.frames` stays `0`. `wallctl` non-render verbs (`app.text`, `wall.snap`, `app.quit`) still work while occluded, but `app.shot` and `--frames` runs need the window unoccluded.
4. **Exit flush time:** After `app.quit` prints `the wall drew N frames`, `relax` takes ~4–30 s to spill updated `.rcs` stores and rewrite the ~29 MB startup image before the process exits.
5. **Possible future speed-up for live WGSL reload:** Right now `wall_cells.wgsl` needs a restart because `WallRenderer::new` builds pipelines once at startup. Adding a `renderer.reload_shaders` verb (calling `load_wall_wgsl` + rebuilding the 3 pipelines) would turn WGSL iteration into <100 ms live reload in both `relax` and the release binary.
