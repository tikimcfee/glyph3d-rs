# wasm32-unknown-unknown readiness audit — glyph3d-native

> **Historical (2026-10-09).** A Mojo-era survey of the then-current tree; line
> references and engine assumptions predate the engine's retirement. Kept for its
> reasoning, not as current state.

Audited: native/ @ main (dirty), wgpu 30.0.1, winit 0.30.13. All line refs verified against working tree.

## 1. pollster usage
Exactly ONE call site: `main.rs:577` — `pollster::block_on(gpu::init(None))`, main()-only.
No per-frame/test/readback pollster. Readbacks block differently: `device.poll(PollType::Wait)` + `std::sync::mpsc::channel().recv()` at `offscreen.rs:163-176` and `glyph_scene.rs:2128-2138` (`debug_dump_instances`, env-gated GLYPH_G_DUMP). On wasm `PollType::Wait` is unsupported (browser event loop owns completion) — the map_async callback only fires when you yield. Restructure scope is small: one init await + two readback paths.

## 2. engine.rs FFI — the elephant
extern "C" imports (engine.rs:121-165): `glyph_engine_new/free/load_trie_file/load_item/slot_count/copy_slots/load_items`. Boundary data: opaque `*mut c_void` handle; text bytes + f64/i32 layout params by value; results copied out as 32 B wire records (GlyphRecord, engine.rs:17-53). NOTE: `load_trie_file` passes a PATH and the Mojo dylib does its own file read (engine.rs:220-238). `Engine` is `!Send` raw-pointer state.

Call sites — all init-time or per-pick, NONE per-frame:
- main.rs:88-98 `engine_layout` — scene build for EngineText
- main.rs:427-470 `run_engine_smoke` — CLI-only, no GPU
- main.rs:476-515 `run_engine_check` — cross-check Mojo vs text.rs `reference_layout` (pure Rust CPU reference), bit-exact
- repo.rs:456-624 `load_repo` — scene build (naive per-file or one batched call)
- repo.rs:633-644 `rederive_records` ← glyph_scene.rs:1234 `ensure_pick_cache` — PER-PICK engine re-run (one-entry cache)

VERDICT: Mojo is NOT on the render path. Rendering (glyph_scene.rs render/cull/draws, WGSL pipelines) is self-contained Rust+wgpu; Mojo produces layout records at load and re-derives per-pick geometry. `SceneChoice::Text` (the default scene) never touches the engine — text.rs stages from the atlas trie directly. A wasm gate is plausible:
- Text scene: works engine-free today.
- Repo scene: needs a Rust port of the paginated layout (wrap/pages). text.rs:347 `reference_layout(trie, bytes, origin, line_height)` is the proven bit-exact reference but covers NO wrap/pagination — the port must extend it to full ItemParams (repo.rs:200-225 params). Medium-large.
- Pick: ensure_pick_cache already cross-checks engine rows/cols against pure-Rust `text::fold_leaders` (glyph_scene.rs:1239-1249) — a Rust-layout fallback slots in at repo.rs:633.
Also unix-only: engine.rs:225 `std::os::unix::ffi::OsStrExt`; usize in FFI sigs (32-bit on wasm32) — moot once gated. build.rs:28-42 PANICS if the dylib/Mojo runtime is missing — must be target-gated.

## 3. File I/O
- Atlas: runtime `std::fs::read` of 4 bins (curves ~2.6 MB, glyphmap, glyphs, codepoints) via atlas.rs:119-131 `read_words(&Path)`, from `<crate>/../assets/atlas` (atlas.rs:194-198, main.rs:68-70). Parsers take `&[u32]` already — trivial to re-source as bytes (include_bytes! or fetch).
- Shaders: `include_str!` at scene.rs:216, glyph_scene.rs:616, glyph_scene.rs:982 — already embedded. 
- Engine trie: read BY THE DYLIB from a path (engine.rs:224) — wasm needs a Rust-side G3TR parser or reuse of TrieTable data.
- text.rs:105 `stage_file` — read_to_string (parsers below take &str; split I/O from parse).
- repo.rs:64-151 `walk_repo` — read_dir/metadata/read (whole walker is fs); repo.rs:639 pick re-read.
- offscreen.rs:189-199 create_dir_all + `image::save_buffer` PNG.
- main.rs:89,430,479 engine-input reads; main.rs:504,512 `std::process::exit` (CLI-only paths).

## 4. winit 0.30
Correct modern API: `ApplicationHandler` impl (windowed.rs:205-393), window created in `resumed`, `event_loop.run_app` (windowed.rs:405). No deprecated closure API, no EventLoopProxy, no native-only extension traits. Web deltas:
- `run_app` must become `EventLoopExtWebSys::spawn_app` (winit::platform::web) — run_app throws on web.
- Window must attach to a canvas: `WindowAttributesExtWebSys::with_canvas` (windowed.rs:210-218), else winit appends its own.
- `set_cursor_grab(Confined)` unsupported on web; Locked → pointer lock (needs user gesture). The grab() fallback (windowed.rs:76-94) already tolerates failure gracefully.
- `DeviceEvent::MouseMotion` only fires under pointer lock on web; the `saw_device_delta` CursorMoved fallback (windowed.rs:344-357) covers this.
- `surface.get_current_texture` / `queue.present` paths are web-fine.

## 5. Logging init
main.rs:518: `env_logger::Builder::from_env(...default_filter_or("info")).init()` — single init. Web replacement: `console_log::init_with_level` or tracing-wasm. Also heavy `println!` for FPS/stats (windowed.rs:175, offscreen.rs:215-244, repo.rs:761+) — silent no-ops on wasm; surface via console hook or HUD if wanted.

## 6. clap/CLI coupling
clap is fully contained in main.rs: Cli derive (182-247), op-stream interleave (359-408), parse_cli (417). Everything downstream is clap-free: `gpu::init`, `build_scene(ctx, format, &SceneChoice, CameraMode, cull)` (main.rs:101-149), `windowed::run(ctx, choice, cull, ops)` (windowed.rs:395), `offscreen::run(...)` — all take plain data. A web entry can bypass clap entirely. Config the web entry needs from URL/JS: scene choice (demo/text/repo), file bytes + copies, zoom, cull flag, ops list. STRUCTURAL CATCH: the crate is bin-only (main.rs:28-36 private `mod`s, no lib.rs) — nothing is importable until a lib+bin split. Also `default_text_file` (main.rs:421) points at the crate's own main.rs as default content — on web, embed or fetch.

## 7. wgpu web-relevant usage
- gpu.rs:103-109 `Backends::PRIMARY` + `new_without_display_handle()` — web-fine (resolves to WebGPU/GL).
- Limits gpu.rs:136-141: requests adapter's FULL max_storage_buffer_binding_size + max_buffer_size (repo arenas are huge — 48 B × tens of millions; chunked per-binding at glyph_scene.rs:730-741). Works on web but browser limits are lower; chunking already absorbs this. Watch: browsers that report small max_buffer_size will hurt.
- Features: only TIMESTAMP_QUERY (+INSIDE_PASSES), requested ONLY under GLYPH_PROFILE env + adapter support (gpu.rs:154-168). WebGPU's timestamp-query is optional-but-real; env var absent on wasm → profiler auto-off. wgpu-profiler 0.28 works on web.
- Vertex-stage read-only storage buffers (scene.rs:187-196, glyph_scene backdrop bgl 624-633, arena chunks) — WebGPU core. Rgba32Uint atlas textures + textureLoad — core. Depth32Float — core. No storage textures, no push constants, no subgroups.
- NO compute pipeline in use: the GPU cull/indirect design was abandoned (broken first_instance!=0 indirect draws on Metal, glyph_scene.rs:48-56); cull.wgsl is only vs_backdrop/fs_backdrop render entry points. Culling is CPU (glyph_scene.rs:2182) — actually good news for web.
- Surface: windowed.rs:221-225 create_surface(Arc<Window>) → 'static — web-OK once canvas attached.
- Readback: offscreen.rs:163-176 + glyph_scene.rs:2128-2138 (debug) use map_async + `PollType::Wait` + mpsc::recv — blocking; unsupported on web. Offscreen mode can stay native-only (it's the verification oracle); debug dump is env-gated.
- Non-blocking `PollType::Poll` pumps (windowed.rs:150, offscreen.rs:152) are profiler-only and harmless.

## 8. WGSL
3 shaders, all include_str!; validated in cargo test via naga dev-dep (native/tests/wgsl.rs). Naga never ships — web backend passes WGSL strings to the browser's Tint. Clean.

## 9. Other wasm-hostile
- `std::time::Instant::now()` PANICS on wasm32-unknown-unknown. Uses: windowed.rs ×10, repo.rs ×8, glyph_scene.rs ×2, offscreen.rs ×2, main.rs:437. Fix: `web-time` crate (drop-in Instant/Duration, works native too) — mechanical, Medium.
- `std::env::var_os` gates (GLYPH_PROFILE gpu.rs:154, GLYPH_G_DUMP offscreen.rs:71, GLYPH_PICK_DEBUG glyph_scene.rs:1379/1516, GLYPH_CULL_DEBUG 2201) — safe (None on wasm), silently off.
- No std::thread anywhere in src/. Mojo's internal thread pool goes away with the FFI gate.
- build.rs panics without dylib — gate on CARGO_CFG_TARGET_ARCH.
- Deps: pollster/env_logger/clap/clap_complete/image → native-only candidates. image 0.25 pulls a heavy encoder stack (ravif/rav1e/rayon in lock) — only needed for offscreen PNG; gate it. getrandom is in the tree — if it lands in the wasm graph it needs its `js` feature. js-sys/wasm-bindgen/web-sys already in Cargo.lock via wgpu+winit — no version-skew risk.
- Panic behavior: asserts/panics throughout (atlas magic, trie sanity atlas.rs:93) → add console_error_panic_hook on wasm.

## (a) Top 5 blockers
1. Mojo FFI in load + pick paths, plus build.rs hard-link — cfg-gate + Rust-native layout fallback (extend text::reference_layout to wrap/pagination; pick fallback already half-built via fold_leaders). LARGE.
2. Bin-only crate — no lib target, web entry can't import modules; SceneChoice/build_scene/windowed::run are ready-made once exposed. SMALL.
3. Blocking time & completion model: Instant::now() panics; PollType::Wait + mpsc::recv readbacks; single pollster block_on → async init spawned into the event loop. MEDIUM.
4. File I/O re-sourcing: 4 atlas bins + engine trie + text input + repo walk + PNG out → include_bytes!/fetch/JS bridge; parsers already byte-oriented, I/O split is shallow. MEDIUM (repo walk needs a JS file-list manifest — design decision).
5. winit web deltas: spawn_app, with_canvas, pointer-lock grab semantics. SMALL.

## (b) Minimal-diff restructure plan
1. **lib+bin in place** (no workspace needed yet): add src/lib.rs (`pub mod atlas, gpu, glyph_scene, scene, text, repo, offscreen, windowed` + the main.rs free fns: SceneChoice, Op, build_scene, atlas_dir...). main.rs keeps clap/CLI/modes and calls into the lib. engine.rs stays lib-internal.
2. **Feature gate the engine**: `mojo-engine` default feature. engine.rs: `#[cfg(feature)]` real FFI; `#[cfg(not)]` same-API Rust fallback (Engine::new/load_item(s)/records over a ported layout pipeline; v1 may support Text-mode params only, Repo paginated port as the follow-up). build.rs: early-return when `CARGO_CFG_TARGET_ARCH == wasm32` or feature off.
3. **Target-gated deps**:
   - `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`: pollster, env_logger, clap, clap_complete, image (keep offscreen native-only).
   - `[target.'cfg(target_arch = "wasm32")'.dependencies]`: wasm-bindgen, wasm-bindgen-futures, web-sys (HtmlCanvasElement, Url, etc.), console_log, console_error_panic_hook.
   - Both: replace std::time::Instant with web-time::Instant crate-wide (one import swap per file).
4. **Byte-source shim**: change atlas.rs `read_words(&Path)`→`read_words_bytes(&[u8])`, TrieTable::load(dir)→from four byte slices, text::stage_file(path)→stage_str(&str). Provide `asset_bytes(name)` — native: fs read; wasm: include_bytes! for the 4 atlas bins (~3 MB total — acceptable) + fetch for user content.
5. **Web entry** `src/web.rs` (wasm32-only): `#[wasm_bindgen(start)]` → panic hook + console_log → parse `web_sys::Url` query (scene, cull, zoom) → `wasm_bindgen_futures::spawn_local(async { let ctx = gpu::init(None).await; let event_loop = EventLoop::new(); let window attrs with_canvas(existing <canvas id="glyph3d">); surface; let scene = build_scene(ctx, format, &choice_from_params, CameraMode::Fly, true); windowed::run_app_web(...) via EventLoopExtWebSys::spawn_app })`. windowed.rs: extract App::new(ctx, choice, cull, ops) so both native run() and web spawn share it; cfg the `run_app` vs `spawn_app` line.
6. **HTML shell**: ~30-line index.html (full-viewport canvas + wasm-bindgen JS glue), the app owns the canvas.
7. **Repo mode on web (phase 2)**: JS supplies a file manifest (paths + fetch URLs) or a zipped corpus; walk_repo becomes "walk manifest", rederive_records reads from an in-memory HashMap<String, Vec<u8>> instead of fs — swap repo.rs:639's read for a lookup behind a `ContentSource` trait. Offscreen mode + GLYPH_* debug dumps stay native-only (`#[cfg]`).
8. **Gates**: keep native `cargo build/test` green at every step; add `cargo check --target wasm32-unknown-unknown --no-default-features` to tools/check-all.sh.

## (c) Picking deep-dive (2026-09-01 follow-up: "does no engine mean no picking?")

Yes for repo mode today — but the dependency is narrower than it looks, and the Rust port is already half-proven:

- **Picking is repo-mode-only by design**: `StagedScene.pick` is `None` for text/engine scenes (text.rs:80, :290, :583); `apply_pick` bails with "repo mode only" (glyph_scene.rs:1546). So the phase-1 wasm Text demo loses nothing — it has no picking natively either.
- **The engine's role in picking** is exactly one thing: `rederive_records` (repo.rs:633) re-runs `load_item` per picked file to get the 32 B wire records — the *measures* (x/y/z/advance/height), glyph_id lanes (blank-drop slot mapping), and ROW/COL lanes. Char resolution itself (leaders/rows/cols/lines via `text::fold_leaders`, colors via `colorize_leaders`) is already pure Rust (glyph_scene.rs:1239-1250).
- **The fold is already ported and continuously verified**: `fold_leaders` implements the engine's exact wrap fold (ROW = base_row + col // wrap; newline rides at col == line length; text.rs:435). The pick cache cross-checks CPU rows/cols against engine ROW/COL lanes on EVERY record of EVERY cache fill (glyph_scene.rs:1240-1249) — the standing gate. So the wrapped row/col math is proven bit-exact in Rust today.
- **The actual delta is measures under wrap + pagination**: `reference_layout` (text.rs:347) is bit-exact (`--engine-check` diffs it against the FFI) but explicitly wrap=0, no pages (text.rs:341). Porting = extend it with the wrap fold (positions: wrap returns line_adv to 0 and bumps row — same f64-chain discipline) and the pagination terms in ItemParams (page_rows/cols, scroll_rows, pages_wide, page_gap_x, band_stride_y, depth_per_band/col). All inputs already in Rust: trie lookup (atlas::TrieTable), fu_to_world (text.rs:337), the f64→f32 narrowing points.
- **Verification story is the project's own culture**: once extended, `--engine-check` + the pick fold cross-check + `--repo-verify` prove the port bit-exact against Mojo on native, and that same Rust code then compiles to wasm unchanged. The engine becomes a native-only oracle, not a runtime dependency.
- **Web actually simplifies rederive_records**: no fs re-read (repo.rs:639) — file bytes are already in memory from the fetched corpus; it becomes a pure function over bytes behind the ContentSource shim (plan step 7).
- Effort estimate stands: MEDIUM-LARGE, dominated by pagination semantics; the wrap fold is mostly transcribing fold_leaders' math into measure accumulation.
