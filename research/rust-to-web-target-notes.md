# Rust → wasm Web Target — Research Notes (2026)

Companion to `wasm-port-audit.md`. Team context: pure-JS WebGPU implementation already exists (viz-web/glyph3d-js), so web platform knowledge is assumed; this covers only the **Rust-relative** state.

## 1. Mojo → wasm: NOT SUPPORTED — no horizon

- Mojo/MAX compiles to native CPU (Linux/macOS) + NVIDIA/AMD GPU only. No wasm target, no dated roadmap item. Only a 2023 speculation thread and third-party write-ups saying it "*would*" be nice. [mojo Discussion #71](https://github.com/modularml/mojo/discussions/71), [HackerNoon Oct 2025](https://hackernoon.com/mojo-aims-to-unite-the-computing-stackfrom-cloud-to-edgewith-mlir-power)
- Implication: dylib FFI is native-only forever-ish. Plan the `cfg` boundary now; Mojo-side logic must be ported to Rust or excluded on web. There is no "wait for wasm-Mojo" option.

## 2. Toolchain: own your pipeline (rustwasm is gone)

- rustwasm org sunset 2025-07-21, archived Sept 2025. wasm-bindgen moved to a new org and is healthy (0.2.105 Oct 2025 → 0.2.118 Apr 2026). wasm-pack → community-maintained (drager), not recommended for new setups. twiggy/gloo/walrus archived (twiggy still works). [Inside Rust blog](https://blog.rust-lang.org/inside-rust/2025/07/21/sunsetting-the-rustwasm-github-org/), [nickb.dev](https://nickb.dev/blog/life-after-wasm-pack-an-opinionated-deconstruction/)
- **Pick: explicit pipeline** — `cargo build --target wasm32-unknown-unknown` + pinned `wasm-bindgen-cli` + `wasm-opt` (binaryen 129+, Apr 2026), wrapped in just/xtask recipes. This is what Bevy (`build-wasm-example` xtask), wgpu (`run-wasm` xtask), and eframe do. Trunk's value is dev-server/asset-pipeline for framework SPAs — little benefit for an engine app that owns its HTML/canvas. Trunk itself: 0.21.14 stable (May 2025), 0.22.0-beta.2 (Jul 2026). [crates.io/crates/trunk](https://crates.io/api/v1/crates/trunk)
- **Hard requirement:** `wasm-bindgen-cli` version must EXACTLY match the `wasm-bindgen` crate in Cargo.lock. Pin both (`wasm-bindgen = "=0.2.x"`), e.g. via mise or cargo-run-bin.

## 3. wgpu 30.x on wasm

- Features: `webgpu` (default, browser WebGPU) vs `webgl` (WebGL2 fallback, reduced). `fragile-send-sync-non-atomic-wasm` fakes Send/Sync for single-threaded wasm. [wgpu features doc](https://wgpu.rs/doc/wgpu/documentation/features/index.html)
- WGSL: on webgpu backend there is **no naga in the binary** — strings pass straight to the browser. Big size win; keep the naga dev-dep native-side only.
- **Features split (changed since 2024):** wgpu now separates `features_wgpu` (native-only: MAPPABLE_PRIMARY_BUFFERS, PUSH_CONSTANTS, TIMESTAMP_QUERY_INSIDE_PASSES, …) from `features_webgpu` (portable: TIMESTAMP_QUERY, DEPTH_CLIP_CONTROL, FLOAT32_FILTERABLE, SHADER_F16, …). Web-gating features is now mechanical. [wgpu::Features docs](https://wgpu.rs/doc/wgpu/struct.Features.html)
- TIMESTAMP_QUERY exists on web but browser availability is inconsistent (Chrome has gated it). Inside-passes timestamps are native-only — so wgpu-profiler stays native-only or degrades.
- **Polling model (the big one):** `Device::poll(Wait)` is a **no-op** on the WebGPU backend — callbacks fire from the browser event loop. Any `map_async` + `poll(Wait)` + channel-recv readback **deadlocks the tab** on wasm. [oxicuda changelog — exactly this bug](https://github.com/cool-japan/oxicuda/blob/master/CHANGELOG.md)
- map_async takes a `'static` callback since wgpu 22 — design readbacks around callback→EventLoopProxy or oneshot+spawn_local.
- Surface from winit 0.30: `instance.create_surface(Arc<Window>)` just works — raw handle IS the HtmlCanvasElement on web.

## 4. winit 0.30 on web

- `ApplicationHandler` + window created in `resumed()`; bind existing canvas via `WindowAttributesExtWebSys::with_canvas(...)`.
- Use `EventLoopExtWebSys::spawn_app`, NOT `run_app` (which unwinds via JS exception). [rust-lang forum Sept 2025](https://users.rust-lang.org/t/how-to-integrate-winit-0-30-with-async/133747)
- Gotchas: `with_prevent_default(true)` (else wheel scrolls page); `with_focusable(true)` required for keyboard; scale_factor = devicePixelRatio with ResizeObserver-driven `Resized`/`ScaleFactorChanged`; no CSS transform/border/padding on canvas (breaks pointer coords); `cfg!(target_os = "macos")` is FALSE on wasm — Cmd-vs-Ctrl needs runtime UA detection. [buiy winit-web notes](https://github.com/intendednull/buiy/blob/main/docs/prior-art/web-rendering/winit-web.md)
- winit 0.31 (if upgrading): `resumed/suspended` → `can_create_surfaces()/destroy_surfaces()`, `user_event` → `proxy_wake_up`, MSRV 1.85.

## 5. Async/entry structure

- `pollster::block_on` on wasm: panics (std condvar unsupported) or freezes the event loop that must deliver the WebGPU promises you're awaiting. Any custom executor dies on the first non-ready future. [bevy Discussion #3239](https://github.com/bevyengine/bevy/discussions/3239)
- Pattern: browser event loop IS the runtime. `wasm-bindgen-futures` (`spawn_local`, `JsFuture`, `future_to_promise`). With winit: in `resumed()`, `spawn_local(async { State::new(window).await })` → deliver state back via `EventLoopProxy` → user_event handler. Or a `#[wasm_bindgen(start)]` that owns everything and drives winit via spawn_app. [learn-wgpu tutorial1](https://sotrh.github.io/learn-wgpu/beginner/tutorial1-window/)
- Keep `pollster` as `cfg(not(target_arch = "wasm32"))` dep only — pulling it into wasm can break module loading.

## 6. Testing in CI (changed since 2024 — headless WebGPU works now)

- Harness: `wasm-bindgen-test` + `wasm-bindgen-test-runner` (`CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER`), `wasm_bindgen_test_configure!(run_in_browser)`, CHROMEDRIVER + `webdriver.json` for flags. [wasm-bindgen guide](https://rustwasm.github.io/docs/wasm-bindgen/wasm-bindgen-test/browsers.html)
- **Headless Chrome WebGPU flag set (Linux, GPU-less):** `--headless=new --enable-unsafe-webgpu --enable-features=Vulkan --use-angle=vulkan --use-vulkan=swiftshader --use-webgpu-adapter=swiftshader --disable-vulkan-surface` (SwiftShader→Dawn). macOS runners: hardware Metal with just `--enable-unsafe-webgpu`. Gotchas: secure context required (`http://localhost`); Puppeteer's `--use-angle=swiftshader-webgl` must be removed; Dawn adapter blocklist needs `--enable-dawn-features=allow_unsafe_apis,disable_adapter_blocklist`. [agent-browser webgpu preset](https://agent-browser.dev/webgpu), [tigerabrodi.blog Feb 2026](https://tigerabrodi.blog/how-to-get-webgpu-in-headless-chrome-on-cloud-gpus)
- Landscape: wgpu's own CI finds wasm-bindgen-test flaky (browser startup, panic poisoning, flag plumbing) — Dec 2025 issue proposes Playwright + per-test iframes. Bevy does pixel tests native-side; Ruffle tests via browser automation at scale. [wgpu #8709](https://github.com/gfx-rs/wgpu/issues/8709)
- Practical strategy: logic tests in wasm (cheap, reliable); GPU smoke (adapter + trivial render + readback assertion) via SwiftShader on Linux / Metal on macOS runners; full pixel comparisons through a JS-side harness (the JS implementation already has frame-capture checks) or `#[ignore]` locally. Page screenshots of a presenting canvas are unreliable headless on Linux/Windows — prefer offscreen readback assertions.

## 7. Binary size + panics

- Recipe: `opt-level = "z"`, fat `lto`, `codegen-units = 1`, `strip = true`, then `wasm-opt -Oz` AFTER wasm-bindgen (10–20% further). Typical offenders: core::fmt panicking machinery, dlmalloc, serde derive, regex, image codecs, clap — keep all out via target-gated deps.
- New since 2024: Binaryen `wasm-split` (--multi-split, profile-guided) = real lazy loading; Leptos shipped wasm code splitting in 2025. [wasm-split man page](https://manpages.debian.org/testing/binaryen/wasm-split.1)
- twiggy (archived but working) for size attribution (`twiggy top`).
- **Panics (2024 wisdom changed):** wasm EH stabilized mid-2025; `panic=unwind` possible via nightly build-std; wasm-bindgen added abort handling (`wasm_bindgen::handler`: `set_on_abort`, `schedule_reinit`) to detect a poisoned instance and transparently reinit. Practical default still: `panic=abort` + `console_error_panic_hook`, treat panic as fatal-to-instance (or wire schedule_reinit). Don't use catch_unwind. [wasm-bindgen: Handling Aborts](https://wasm-bindgen.github.io/wasm-bindgen/reference/handling-aborts.html)

## 8. Threads

- Not needed. wgpu is single-threaded on wasm (WebGPU JS objects can't cross workers). Wasm threads require nightly build-std with atomics + COOP/COEP cross-origin isolation headers — only worth it for CPU-parallel work (e.g. layout/shaping off-thread); GPU path gains nothing.

## Biggest deltas from 2024-era wisdom

1. rustwasm gone — own the build pipeline, pin wasm-bindgen crate+CLI together.
2. Panic semantics moved: wasm EH stabilized mid-2025, wasm-bindgen has abort detection/reinit APIs; abort remains the practical default.
3. Headless-Chrome WebGPU in CI genuinely works (SwiftShader on Linux, Metal on macOS runners).
4. wasm-split = real code splitting if size demands it.
5. wgpu `Features` explicitly split native-only vs portable — web-gating is mechanical now.
