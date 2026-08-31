# Native Rendering Stack Evaluation for GPU Code Visualization
**As of: August 2026**

## Verdict (TL;DR)

- **Primary: Rust + raw `wgpu` (v30.x) with your existing WGSL.** Zero shader rewrite, first-class compute, native Metal on macOS (no MoltenVK), <5% CPU overhead vs raw Vulkan, trivial Mojo interop via C ABI, pure Cargo build.
- **Fallback: C/C++ against `webgpu.h` using `wgpu-native` prebuilt binaries (or prebuilt Dawn via the WebGPU-distribution/gpu.cpp route).** Same WGSL, same WebGPU API, direct Mojo `external_call`/DLHandle integration.
- Do **not** bet on Godot (no GPU-driven instanced draw path), bgfx (no WGSL, older shader model), Mach (pre-1.0, abandoned its WebGPU mirror), or Sokol (compute landed 2025 but still lacks GPU→CPU readback and indirect draw).

---

## 1. Your three.js/WebGPU baseline — what actually ports

three.js `WebGPURenderer` compiles TSL node graphs to **WGSL** when running on WebGPU, and compute work goes through `renderer.compute()`/`computeAsync()` on `instancedArray` buffers; raw WGSL can be injected via `wgslFn()`. Source: Maxime Heckel's field guide: 2025-10-14 (https://blog.maximeheckel.com/posts/field-guide-to-tsl-and-webgpu/); three.js transitioning docs (https://salivity.github.io/three.js/article/three-js-webgpurenderer-transitioning-to-webgpu).

**Implication:** any hand-written WGSL (`wgslFn`, or shaders you can dump from TSL output) runs verbatim on wgpu, wgpu-native, and Dawn. TSL graphs themselves do not port — only their WGSL output. If the app is WebGL2/GLSL, everything needs a rewrite regardless of target, which removes a differentiator between candidates.

## 2. wgpu (Rust) — the frontrunner

**Current state:** wgpu is at **v30.0.1** (2026); v28 added mesh/task shaders with WGSL support, v29 reworked surface/error APIs, v30 added HDR surface color spaces and 16-bit shader ints. Breaking releases ship every ~3 months; MSRV 1.87. Source: gfx-rs/wgpu releases: 2026 (https://github.com/gfx-rs/wgpu/releases).

**Compute:** first-class since the beginning (WebGPU compute model). Also supports multi-draw-indirect, indirect-count, subgroups/cooperative ops (v29), and raw-backend interop (`as_raw`/hal) for exotic needs.

**Performance reality:** overhead vs raw Vulkan "typically under 5%, often zero for GPU-bound workloads" because translation happens at pipeline/bind time, not per-draw. Source: rustify.rs analysis: 2026-08-19 (https://rustify.rs/articles/rust-gpu-computing-wgpu-2026). wgpu is Firefox's production WebGPU implementation (wgpu-core + naga), used in production by Deno, Bevy, the Rerun viewer, and COSMIC Terminal.

**Million-glyph proof:** the exact "instanced glyph field" pattern — texture atlas + instanced quads + per-instance storage-buffer data — is the standard wgpu text approach (glyphon on cosmic-text+etagere; SDF-atlas instancing pattern documented at tchayen.com: https://tchayen.com/drawing-text-in-webgpu-using-just-the-font-file; glyphon: https://github.com/grovesNL/glyphon). COSMIC Terminal ships glyphon+wgpu in production with performance "similar to Alacritty" on 8 MB text files: System76 (https://system76.com/blog/post/cosmic-the-road-to-alpha). For scale headroom, Bevy 0.17's wgpu-based virtual geometry renders **>1M instances (~900B triangles) in ~4.5 ms on an RTX 4070**: Bevy 0.17 release notes: 2025-09-30 (https://bevy.org/news/bevy-0-17/). Glyph quads are trivially cheaper. GPU-side culling→indirect-draw compaction is a straightforward WebGPU pattern: mysimulator.uk: 2026-04-05 (https://mysimulator.uk/content/articles/instanced-rendering-lod.html).

**macOS:** wgpu-hal targets **Metal natively** — no MoltenVK in the path, satisfying the "no direct Metal API" constraint while keeping Metal performance.

### Raw wgpu vs Bevy for this app

Bevy is in the best shape of its life: 0.16 (Apr 2025) shipped GPU-driven rendering (3× on the Caldera stress scene: https://bevy.org/news/bevy-0-16/), and 0.17 decoupled the public API from `bevy_render` so you can use ECS/assets/windowing without its renderer, or replace the renderer wholesale. **But** a bespoke instanced-glyph-field renderer is exactly the case where Bevy's render graph, extract/prepare/queue phases, and material system are pure overhead — you'd write custom `RenderCommand`s and compute nodes anyway (see the sheer boilerplate in Bevy's own custom-instancing example: https://bevy.org/examples/shaders/custom-shader-instancing/). **Recommendation: raw wgpu + winit.** Use Bevy only if you want its ECS/asset/ecosystem badly enough to pay the abstraction tax.

## 3. wgpu-native / webgpu.h (C API) — the interop play

wgpu-native exposes the same wgpu-core engine behind the standard `webgpu.h` C header plus a small wgpu-specific extension header, with **prebuilt binaries on the release page** — no Rust toolchain required for consumers. Sources: Stack Overflow comparison: (https://stackoverflow.com/questions/74434480/wgpu-and-dawn-webgpu); gfx-rs org listing (https://github.com/gfx-rs). C++ RAII wrappers exist (eliemichel/WebGPU-Cpp: https://github.com/eliemichel/WebGPU-Cpp). This is the smoothest path if the app shell is C/C++ or if you want Mojo to drive rendering directly via `external_call`/`OwnedDLHandle`.

## 4. Dawn (C++, Google)

Production-proven (Chrome's WebGPU), WGSL-native via Tint, full compute. The problem is the **build**: depot_tools + GN + Ninja + Python, long compile, std-type/linker friction on MSVC. Source: cwoffenden/hello-webgpu notes (https://github.com/cwoffenden/hello-webgpu). This is largely **mooted by prebuilts**: Google's own Dawn releases, the eliemichel WebGPU-distribution (precompiled Dawn or wgpu-native selected by `WEBGPU_BACKEND`: https://eliemichel.github.io/LearnWebGPU/next/getting-started/hello-webgpu.html), and answer.ai's gpu.cpp which ships prebuilt `libdawn` shared libraries for macOS/Linux (https://www.answer.ai/posts/2024-07-11--gpu-cpp.html). **Verdict: viable fallback, but if you're going prebuilt-C-API anyway, wgpu-native gives you the same `webgpu.h` with less vendor churn; Dawn wins only if you specifically want Chrome's exact conformance behavior or already live in a C++/CMake world.**

## 5. Godot 4.x — eliminate

RenderingDevice compute shaders (GLSL→SPIR-V) are real and documented since 4.0, and the engine is mature: 4.5 (Sep 2025) added the shader baker and Vulkan foveated rendering; 4.6 (Jan 2026; 4.6.3 by Apr 2026) made Jolt default, rewrote SSR, and shipped **LibGodot** for embedding the engine as a library. Sources: oflight recap: 2026-05-09 (https://www.oflight.co.jp/en/columns/godot-4-5-and-4-6-feature-update-2026); Godot release blog (https://godotengine.org/blog/release/).

**But the specific pattern fails:** MultiMesh instance buffers can be written by compute, yet (a) there is **no draw-indirect path** — GPU-generated data must round-trip GPU→CPU→GPU to drive MultiMesh draws (open proposal #8647, still unresolved: https://github.com/godotengine/godot-proposals/discussions/8647), and (b) CPU-side reads of GPU-updated MultiMesh buffers hit stale caches (issue #108847: 2025-07-21, https://github.com/godotengine/godot/issues/108847). MultiMesh also culls as one object — no per-instance culling without chunking hacks. Add GDExtension call overhead and MoltenVK underneath (ray-query etc. disabled under MoltenVK per the RenderingDevice docs: https://docs.godotengine.org/en/stable/classes/class_renderingdevice.html), and Godot cannot hit "millions of GPU-culled glyphs" without fighting the engine. **Out.**

## 6. bgfx — capable but wrong shader ecosystem

bgfx genuinely has everything on the checklist: `BGFX_CAPS_COMPUTE`, instancing, draw-indirect and draw-indirect-count, plus a shipped GPU-driven-rendering example (example 37: https://github.com/bkaradzic/bgfx/blob/master/examples/37-gpudrivenrendering/gpudrivenrendering.cpp; API ref: https://bkaradzic.github.io/bgfx/bgfx.html). GPU-driven occlusion culling has been ported to it: Interplay of Light: 2018 (https://interplayoflight.wordpress.com/2018/03/05/porting-gpu-driven-occlusion-culling-to-bgfx/). It's battle-tested across D3D11/12, Vulkan, Metal, GL.

**Dealbreakers:** shaders are bgfx's GLSL-flavored dialect compiled by `shaderc` — **no WGSL path**, so your shader code needs full porting and its offline-compile step; its buffer model predates structured buffers (vertex/index buffers double as UAVs, with quirks documented in that port). Also single-maintainer risk (Branimir Karadžić). **Fallback-tier at best; only if the team strongly prefers C++ and accepts a GLSL rewrite.**

## 7. Raw Vulkan (+MoltenVK) / Vulkano — not worth it

wgpu already sits on Vulkan/Metal/DX12 with ~0–5% overhead and gives you Metal-without-MoltenVK on macOS. Raw Vulkan buys you: device-generated commands, exact memory control, no validation-layer compromises — none of which matter for instanced textured quads, which are bandwidth-trivial. Meanwhile you inherit: MoltenVK as a dependency on macOS (a translation layer with documented feature gaps — Godot's ray-query disablement is one), Vulkan's ~10× boilerplate, and you still write your own portability layer for DX12/driver quirks. Vulkano/gfx-rs: Vulkano is fine but strictly fewer users than wgpu; gfx (pre-wgpu) is dead. **The extra control is not worth it for this workload.** Revisit only if profiling on wgpu shows a concrete wall (you can drop to `wgpu-hal` raw handles without leaving the ecosystem — that's how Bevy integrated DLSS via Vulkan interop: jms55.github.io: 2025-09-03, https://jms55.github.io/posts/2025-09-03-bevy-fifth-birthday/).

## 8. Others

- **Mach (Zig):** pre-1.0, experimental; **v0.5 was the last version mirroring WebGPU/WGSL** — it has since moved to its own GPU backend/windowing and is evaluating Zig-as-shading-language. Community threads (Mar 2026) point users at forks for newer Zig. Sources: machengine.org docs (https://machengine.org/docs/stdlib/); ziggit thread: 2026-03-20 (https://ziggit.dev/t/mach-engine/14662). **Not a production foundation.**
- **Sokol:** compute only landed **March 2025**, storage images May 2025, and it **still lacks GPU→CPU readback/copy and indirect draw** — explicitly acknowledged by the author. Sources: compute update: 2025-03-03 (https://floooh.github.io/2025/03/03/sokol-gfx-compute-update.html); ms2: 2025-05-19 (https://floooh.github.io/2025/05/19/sokol-gfx-compute-ms2.html); issue #1246 (https://github.com/floooh/sokol/issues/1246). Charming, wrong fit.
- **rust-gpu (Rust→SPIR-V):** Embark archived the repo **October 2025** — do not build shader strategy on it (rustify.rs, op. cit.).
- **WebGPU standard itself:** now W3C Candidate Recommendation Draft (May 2026) — the API surface you target is stabilizing, de-risking the wgpu/Dawn bet (rustify.rs, op. cit.).

## 9. Mojo interop — concrete paths

Mojo's FFI surface today: `external_call` and `DLHandle`/`OwnedDLHandle` for calling into C shared libraries, static linking via `-Xlinker` object files, and `@export` + `abi("c")` to expose Mojo functions to C (`mojo build --emit object`). Source: Mojo C-interop docs mirror: 2026-07-12 (https://ruhati.net/mojo/_c_interoperability.html). **Caveat (important):** a Modular team member states Mojo "doesn't really offer guarantees on what is C ABI compatible... a lot of stuff which happens to work, but that could change at any time" — verify layouts with ABI tooling and pin compiler versions. Mojo is planned to go open-source during 2026. Source: Modular forum: 2025-02-23 (https://forum.modular.com/t/mojo-roadmap-questions-building-a-library-and-ios-support/629).

**Recommended architecture (path A):** Rust executable owns window + wgpu; Mojo parser/high-perf lib compiles to a `cdylib`/object with `@export abi("c")` entry points; Rust calls it via `extern "C"` (standard, well-trodden — cbindgen/safer-ffi patterns: https://docs.rust-embedded.org/book/interoperability/rust-with-c.html). Hand off text data via shared buffers/pointers — zero-copy into the same address space that uploads to GPU buffers.

**Path B:** Mojo owns the process and calls **wgpu-native** (`webgpu.h`) directly through `OwnedDLHandle` — works today, but you write all GPU orchestration in Mojo against a C API; more painful than Rust's wgpu.

**Path C (C++ shell):** C/C++ main + wgpu-native or prebuilt Dawn + Mojo objects via `cc`/`mojo -Xlinker`. Fine, but strictly worse tooling than path A.

## 10. Build/tooling burden summary

| Stack | Build cost | Notes |
|---|---|---|
| Rust + wgpu | `cargo add wgpu` | Shader WGSL embedded/included; no external toolchain |
| C/C++ + wgpu-native | Download prebuilt binary + header | Zero compilation of the engine |
| C/C++ + Dawn | Prebuilt (WebGPU-distribution/gpu.cpp) or painful depot_tools source build | Source build only if you must patch Dawn |
| Godot | SCons engine build or export templates; GDExtension per-release rebuilds | Plus engine upgrade treadmill |
| bgfx | GENie/Make + shaderc offline shader pipeline | Custom shader toolchain forever |
| Raw Vulkan | SDK + your own everything | MoltenVK on macOS |

## 11. Final recommendation

**Primary: Rust + raw wgpu (v30), winit for windowing, your WGSL carried over verbatim, glyphon/cosmic-text (or your own SDF-atlas pipeline, which you're 90% of the way to having) for glyph rasterization, Mojo parser linked as a C-ABI cdylib.** This is the only option that satisfies all four hard requirements with zero shader rewrite, native Metal on macOS, proven million-instance throughput (Bevy 0.17 numbers on the same wgpu stack), and the lightest build story. Do not use Bevy's renderer for the glyph field — borrow its patterns (indirect compaction, gpu-driven draw) as reference code.

**Fallback: C/C++ shell + `webgpu.h` against wgpu-native prebuilt binaries** (Dawn prebuilt if you hit a wgpu-specific bug or need Chrome-exact conformance). Same shaders, same API concepts, direct Mojo `external_call` integration.

**Explicitly rejected:** Godot (no GPU-driven instanced draw, MultiMesh round-trip bugs), bgfx (no WGSL), Mach (immature, left WebGPU), Sokol (compute too young, no readback/indirect), raw Vulkan+MoltenVK (cost without payoff), rust-gpu (archived).

**Key risk to track:** Mojo's C ABI is not yet contractually stable — pin the Mojo toolchain, add ABI smoke tests to CI, and keep the FFI surface to scalars + opaque pointers until Modular formalizes it (open-sourcing planned 2026).
