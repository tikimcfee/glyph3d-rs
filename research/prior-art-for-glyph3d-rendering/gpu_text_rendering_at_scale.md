# GPU text rendering techniques and text at massive scale (10^7–10^8+ glyphs), compared to glyph3d

Researched 2026-10-10. Verification key: **[fetched]** means the page itself was read for this note. **[search]** means the claim comes from a search-result summary of the page, which was not opened. **[local]** means a file in this repo. Figures quoted from news coverage are labelled as claims.

glyph3d reference points used for comparison (all [local], from `AGENTS.md`, `out/history/VISIBLE-MODE.md`, `out/GPU-DIRECTION-2026-10-09.md` in this worktree):
- Slug-style analytic coverage from prebaked quadratic outlines, in WGSL on wgpu 30.
- 95.2 M glyph instances for a 97.0 MB repo (1,306 files), laid out in about 168 ms backend / about 177 ms to first visual (flat colour) on Apple Silicon. These are late-2026-09 figures, not re-measured since; AGENTS.md says "re-measure before quoting".
- Three field modes (Instanced 32 B/slot, Derived 20 B/slot, Visible: GPU per-frame layout of in-view lines from resident bytes).
- Per-line "wash" boxes below a pixel threshold (`[lod] show_glyphs_px`); `greek_mode` exists as a panel option.
- 3D perspective world with pages and depth wrapping.

---

## 0. What the author's prior-art page already covers, and the technique landscape it does not

### Takeaway
The author's page (Volume 05, "Rendering Text at Scale") already covers the SDF/MSDF lineage, Dobbie, Slug (including its 2026 public-domain dedication, slug-webgpu and Sluggish), Pathfinder, Vello, troika, three-mesh-ui, the Limberger 2024 instancing paper, Zed/GPUI, Makepad, Lapce, Glamorous Toolkit, HarfBuzz, cosmic-text/swash/fontdue, PABR kernel3d and editor minimaps. Not listed there, and covered below: Loop–Blinn, Kokojima/NV_path_rendering, Evan Wallace, GLyphy-era successors (gllabel, faf-text, osor.io), the 2026 Slug ports (sluggrs, JSlug, slughorn, EricLengyel/Slug reference shaders), Skia's perspective-text fallback, glyphon/glyph_brush/wgpu_glyph, terminal renderers, Mapbox/deck.gl label rendering, Seesoft, VS Code minimap blocks, and, the most important omission, **Rik Arends' September 2026 Makepad 3D code viewer**.

### Cited Findings
**Already on the prior-art page** [fetched]: https://ivanlugo.dev/prior-art/ and https://ivanlugo.dev/prior-art/gpu-text/
- Volume 05 lists: Valve SDF (Green 2007), GLyphy (github.com/behdad/glyphy), MSDF/msdfgen/msdf-atlas-gen/awesome-msdf, Will Dobbie 2016, Slug (i3D 2018 PDF, sluglibrary.com, "A Decade of Slug", public domain 17 Mar 2026), slug-webgpu (github.com/diffusionstudio/slug-webgpu), Sluggish (github.com/mightycow/Sluggish), Pathfinder, Vello, troika-three-text (issue #117: slows at about 500 `Text` instances), countertype/three-text, three-mesh-ui, Limberger et al. Web3D 2024 (about 50,000 instanced glyphs), Zed/GPUI ("million-line files at native refresh"), Makepad (MSDF code editor), Lapce, Glamorous Toolkit, HarfBuzz, cosmic-text/swash/fontdue, PABR kernel3d (2002), Sublime/VS Code minimaps. The page labels itself a Claude-assembled survey. — [ivanlugo.dev/prior-art/gpu-text](https://ivanlugo.dev/prior-art/gpu-text/)

**Not on the page: analytic and stencil curve techniques**
- **Loop–Blinn (Microsoft, 2005).** US7564459B2, "Resolution-independent curve rendering using programmable graphics hardware" (Loop, Blinn; Microsoft). Each Bézier gets a control triangle with texture coordinates, and a pixel shader runs an inside/outside test. A Hacker News comment claims the patent expires 25 March (2026 implied); I could not confirm this against the patent record. — [Google Patents US7564459B2](https://patents.google.com/patent/US7564459B2) [search]; [HN "A Decade of Slug" mirror](https://hn.nuxt.dev/item/47416736) [search]
  - *vs glyph3d:* this is per-curve triangles (interior plus curve hulls), so geometry scales with outline complexity per glyph. glyph3d draws one quad per glyph and evaluates curves per fragment.
- **Kokojima et al. 2006 (stencil fan + quadratic discard) and Kilgard & Bolz 2012 (NV_path_rendering).** Max Crofts measured both against Lengyel without bands (20 Mar 2026; GTX 1070, 1080p, 16x MSAA, Arial 16 px): at 15,914 glyphs, 13.9 ms (Kokojima), 8.33 ms (NV_path), 105.3 ms (Lengyel, no band optimisation). — [maxcrofts.com/gpu-text-rendering](https://maxcrofts.com/gpu-text-rendering/) [fetched]
  - *vs glyph3d:* this is the only head-to-head timing I found. It shows that a Slug-style shader without banding and without small-glyph LOD collapses past about 10^4 glyphs at 16x MSAA. glyph3d's tiered LOD (wash below a px threshold) is what keeps 10^7–10^8 glyph scenes away from that regime.
- **Evan Wallace, "Easy Scalable Text Rendering on the GPU" (Medium).** Fills glyph outlines on the GPU with no texture cache: much less memory than SDFs, resolution-independent. — [Medium](https://medium.com/@evanwallace/easy-scalable-text-rendering-on-the-gpu-c3f4d782c5ac) [search; direct fetch returned 403]
  - *vs glyph3d:* multi-pass accumulation per string. It suits a few strings, not 10^8 instances in one depth-tested 3D pass.
- **gllabel (azsn).** "GPU Vector Text Rendering (WIP)", a Dobbie-lineage GLSL renderer. — [github.com/azsn/gllabel](https://github.com/azsn/gllabel) [search]
- **faf-text (meawoppl).** Built on plain wgpu. Quadratic outlines are evaluated per pixel in the fragment shader (non-zero winding), with no atlas, SDF or MSAA, and band tables for glyphs with more than 16 curves. **Explicitly supports 3D-tilted panes**: coverage is measured on the projected glyph. MIT/Apache-2.0. No glyph-count or timing figures on the page. — [meawoppl.github.io/faf-text](https://meawoppl.github.io/faf-text/) [fetched]
  - *vs glyph3d:* this is the closest technique match in wgpu (analytic, atlas-free, perspective-correct), but at panel scale. No LOD tier, no corpus-scale layout.
- **Rubén Osorio, "Rendering Crispy Text On The GPU" (12 Jun 2025).** A compute shader casts horizontal rays per pixel through quadratic outlines and accumulates winding into a Morton-packed glyph atlas. It refines over frames by temporal accumulation (8→4→2→1 new samples per frame, capped at 512) and adds per-subpixel AA. Full-screen intro text peaks at about 0.1 ms on an AMD 9070 at 4K, and drops to zero once converged. 2D screen-space only. — [osor.io/text](https://osor.io/text) [fetched]
  - *vs glyph3d:* this is analytic curves rasterized into a cache, a hybrid of Slug and an atlas. The cache works because 2D screen-space text repeats at fixed sizes. glyph3d's perspective world has continuously varying size per glyph, which defeats a size-keyed cache. That is the same reason Skia gives up on SDF for perspective text (below).

**Not on the page: production 2D text stacks**
- **Skia perspective text.** A Skia commit "Draw perspective text as paths": perspective glyphs vary in screen size, so no single SDF level fits a subrun and big glyphs show artifacts, so Skia falls back to path rendering. — [skia.googlesource.com commit 76826fc](https://skia.googlesource.com/skia/+/76826fc91c0abcbd1c8aa4dbc3db4a9d9419e73f) [search]
  - *vs glyph3d:* this is direct evidence that atlas/SDF pipelines treat perspective text as a slow path. glyph3d's analytic shader makes perspective the normal path.
- **glyphon (wgpu).** cosmic-text for shaping and rasterization, etagere atlas packing, sampled in existing render passes; latest 0.12.0. Fork cryoglyph is used by iced. — [docs.rs/crate/glyphon](https://docs.rs/crate/glyphon/latest) [search]; [docs.rs/cryoglyph](https://docs.rs/cryoglyph) [search]
- **glyph_brush / wgpu_glyph / gfx_glyph.** CPU layout with cached glyph positioning and draw calls, plus a GPU texture cache of rasterized glyphs. — [github.com/alexheretic/glyph-brush](https://github.com/alexheretic/glyph-brush) [search]; [docs.rs/wgpu_glyph](https://docs.rs/wgpu_glyph) [search]
  - *vs glyph3d (both):* bitmap atlas plus CPU layout per frame or per change, sized for UI text. They blur under magnification and have no 3D story.
- **Vello glyph caching (2025).** Glifo caches hinting instances and outline paths (PR #1215 reports an informal "100+ FPS boost" on a text field in Vello Hybrid). A newer atlas module rasterizes glyphs once into shared atlas pages with age-based eviction. Sparse strips (CPU/SIMD geometry, GPU compositing) is "not yet suitable for production". — [Vello PR #1215 mirror](https://skia.googlesource.com/external/github.com/linebender/vello/+/289f4919b58e349ccbca39fe23e8cd3d0acadc86) [search]; [glifo atlas source](https://doc.servo.org/src/glifo/atlas/mod.rs.html) [search]; [Linebender Oct 2025](https://linebender.org/blog/tmil-22/) [search]
  - *vs glyph3d:* Vello is a general 2D vector renderer whose direction for text is now caching and atlases, the opposite of per-glyph analytic shading at 10^8 scale. `out/GPU-DIRECTION-2026-10-09.md` [local] also notes that Vello/Parley shape and wrap on the CPU.
- **Zed/GPUI (beyond what the page says).** GPUI rasterizes the whole window on the GPU. On macOS it uses a glyph atlas with CoreText shaping and rasterization (statement from a Zed HN comment). — [GamingOnLinux quoting Zed](https://gamingonlinux.com/2024/07/popular-multiplayer-code-editor-zed-gets-a-linux-release/page=3) [search]; [HN via brianlovin.com](https://brianlovin.com/hn/35078464) [search]
- **Warp.** Blog post "Adventures in Text Rendering: Kerning and Glyph Atlases", an atlas-based renderer. — [warp.dev blog](https://www.warp.dev/blog/adventures-text-rendering-kerning-glyph-atlases) [search; title only]
- **Rio terminal.** Sugarloaf renderer on WebGPU/wgpu. Its text path is ported from glyph-brush/wgpu_glyph, and it reuses Alacritty's parser and grid. — [docs.rs/rioterm](https://docs.rs/rioterm/0.2.34/rio) [search]
- **Other terminals.** par-term-render (wgpu): glyph atlas, instance buffers, three passes (backgrounds, glyphs, cursor). beamterm: OpenGL instanced grid with a 2D texture array. Ghostty (Metal/OpenGL, CoreText): an early commit adds partial cell uploads; the pre-alpha README reported about 120 fps while cat-ing a 6 MB file. — [par-term-render](https://docs.rs/crate/par-term-render/0.9.1) [search]; [beamterm-core](https://docs.rs/beamterm-core/1.0.0/beamterm_core/gl/struct.TerminalGrid.html) [search]; [Ghostty performance page mirror](https://www.mintlify.com/ghostty-org/ghostty/features/performance) [search]
  - *vs glyph3d (terminals):* all are fixed cell grids drawn from a bitmap atlas, with at most screen-sized instance counts (about 10^4–10^5 cells). The scale problem they solve is throughput of incoming bytes, not glyphs resident in view.

**Not on the page: maps and data-vis labels**
- **Mapbox GL.** Glyphs are SDFs drawn as instanced quads; SDF was chosen because rotated bitmap quads blurred and re-rasterizing on rotation was too costly. Placement and collision run on the CPU (CrossTileSymbolIndex, GridIndex), and labels never overlap. — [Mapbox blog: SDF text (2014)](https://blog.mapbox.com/drawing-text-with-signed-distance-fields-in-mapbox-gl-b0933af6f817) [search]; [Map Label Placement in Mapbox GL](https://blog.mapbox.com/map-label-placement-in-mapbox-gl-c6f843a7caaa) [search]; [mapbox-gl-native Collision Detection wiki](https://github.com/mapbox/mapbox-gl-native/wiki/Collision-Detection) [search]
- **deck.gl TextLayer.** A CompositeLayer over IconLayer with an auto-built font atlas. deck.gl's general guide: basic layers stay at 60 fps up to about 1 M items, fall to 10–20 fps near 10 M, and most layers crash in buffer generation between 10 M and 100 M items (2015 MacBook Pro). No TextLayer-specific benchmark is published. — [deck.gl performance guide](https://deck.gl/docs/developer-guide/performance) [search]; [TextLayer API](https://deck.gl/docs/api-reference/layers/text-layer) [search]
  - *vs glyph3d (maps and data-vis):* labels are sparse, culled by collision, and capped at what fits on screen. glyph3d shows every glyph of a corpus with no decimation by collision, and per-line LOD instead.

### Inferences
- Every production stack found (Skia, Zed, Warp, terminals, glyphon, Mapbox, deck.gl, Vello's direction) uses a bitmap or SDF atlas and CPU layout. Analytic per-fragment curves remain a specialist niche (Slug and its ports, faf-text, osor.io's hybrid).

### Gaps
- No primary sources were fetched on internals for Chrome/WebRender, Alacritty, WezTerm, contour, Microsoft Terminal (AtlasEngine), Skia Graphite, Pathfinder or Unreal/Unity text. They are omitted rather than described from memory.
- Evan Wallace's post returned 403, so its technique is described only at the level of the search summary.

---

## 1. Who has published a Slug-style analytic glyph renderer in WGSL/WebGPU/wgpu, and what is Slug's patent status?

### Takeaway
**Verified: Lengyel dedicated the Slug patent (US 10,373,352) to the public domain on 17 March 2026** and released MIT reference shaders. Within months, WGSL/WebGPU/wgpu ports appeared: **sluggrs** (Rust, wgpu 28, iced drop-in), **slug-webgpu** (diffusionstudio; already on the author's page), **slughorn** (AlphaPixel, C++20, WebGPU among its backends), **JSlug** (three.js) and **faf-text** (wgpu, Slug-like with bands). None found operates beyond UI or document scale, and none pairs the shader with corpus-scale layout or LOD.

### Cited Findings
- "A Decade of Slug" (17 Mar 2026): the author is "permanently and irrevocably dedicating the Slug patent to the public domain". Patent US #10,373,352 was granted in 2019 and would have run to 2038. Form SB/43 was filed with the USPTO to disclaim the remaining term, effective that day. — [terathon.com/blog/decade-slug.html](https://terathon.com/blog/decade-slug.html) [fetched]
- The same post: reference vertex and pixel shaders were released at github.com/EricLengyel/Slug under MIT. Changes since JCGT 2017: band *split* optimisation removed (curve lists were stored twice and it added divergence; band texture is now two 16-bit components); adaptive supersampling removed; multi-colour emoji are now stacked independent glyphs; **dynamic dilation**, where the vertex shader computes a per-vertex offset of about half a pixel from the MVP matrix and viewport, with an inverse Jacobian for em-space sampling. No performance numbers. — [terathon.com/blog/decade-slug.html](https://terathon.com/blog/decade-slug.html) [fetched]; [github.com/EricLengyel/Slug](https://github.com/EricLengyel/Slug) (named in the post)
- Algorithm background: winding number over closed quadratic contours, antialiased per pixel. — [Lengyel i3D 2018 slides](https://terathon.com/i3d2018_lengyel.pdf) [search]
- **sluggrs (folknor)**, github.com/folknor/sluggrs. "Drop-in replacement for cryoglyph in iced's wgpu text rendering pipeline". wgpu 28, cosmic-text 0.18, skrifa 0.40. `simple_shader.wgsl` (no dilation) is the one in use; the full dilation shader is not wired up. WGSL is translated from the MIT reference, with a band acceleration structure. RTX 3080, 92 glyphs: about 753 µs CPU prepare, about 11 µs GPU headless. Scanned 14 Jul 2026, 9 stars. — [tomevault mirror of sluggrs README](https://tomevault.io/tome/folknor/sluggrs) [fetched; GitHub repo itself not opened]
- **slug-webgpu (diffusionstudio).** Ports the HLSL shaders to WGSL, uses text-shaper for CPU shaping, and bands curves on the CPU. — [mirror listing](https://bulgariahome.duckdns.org/diffusionstudio/slug-webgpu) [search]; canonical URL github.com/diffusionstudio/slug-webgpu per the author's page
- **slughorn (AlphaPixel).** "our implementation of the Slug technique in modern C++20", with Python bindings and OpenGL, Vulkan, **WebGPU** and DirectX backends; it also renders filled and stroked vectors. MIT per its FAQ. Article dated 23 Sep 2026 compares atlas, SDF, MSDF, Rive and Slug visually, with no timings. — [alphapixeldev.com SDF vs MSDF vs Slug](https://alphapixeldev.com/sdf-vs-msdf-vs-slug-vs-rive-gpu-text-rendering/) [fetched]; [github.com/alphapixel/slughorn](https://github.com/alphapixel/slughorn) [search]
- **JSlug / THREE.SlugText (manthrax, 25 Mar 2026).** three.js port with curves in a texture, about one quad per glyph, and a StandardMaterial-injected variant that is lit and casts shadows. The author suspects it "is not the cheapest to render". Mentions a separate TSL port. — [three.js forum](https://discourse.threejs.org/t/three-slugtext-gpu-text-renderer/90599) [fetched]; demo [manthrax.github.io/JSlug/demo](https://manthrax.github.io/JSlug/demo/)
- Metal by Example published a Slug article in March 2026. — [metalbyexample.com/slug](https://metalbyexample.com/slug/) [search; not opened]
- A Stride-engine guide (pre-dedication) recommended Slug as best for 3D text but "requires a separate license". — [thegraybook.vvvv.org text rendering](https://thegraybook.vvvv.org/reference/best-practice/text-rendering.html) [search]

### Inferences
- glyph3d predates the dedication. Its WGSL Slug-style shader running at 10^7–10^8 instances is, among everything found, the only Slug-lineage renderer operating at corpus scale. The 2026 ports benchmark at 10^2 glyphs (sluggrs: 92).
- Lengyel's **dynamic dilation** (vertex-stage, MVP-aware half-pixel expansion) addresses exactly glyph3d's perspective case. Its absence or presence in glyph3d's shader is worth checking against the new MIT reference.
- Lengyel removed band splitting as a modest win. Max Crofts' numbers suggest banding itself (not the split) remains essential at scale.

### Gaps
- The sluggrs GitHub page and its licence were not opened directly (mirror only). slug-webgpu was seen only through a mirror.
- No WGSL port publishes any figure above a few hundred glyphs.

---

## 2. What is the largest documented interactive glyph count, and with what technique?

### Takeaway
No source documents an interactive renderer with 10^8 glyph *instances resident*, apart from glyph3d itself (95.2 M, [local]). The nearest public claim is **Rik Arends' Makepad-based 3D code viewer (Sept 2026)**: 2.5 M lines at 120+ fps, then **Chromium's 51 M lines at 120 Hz on an M3 Max using about 3 GB**. That is likely 10^8–10^9 characters of source, but its glyph count, text technique and LOD scheme are unpublished. Every other system tops out between 10^4 and 10^7 drawn items.

### Cited Findings
- **Rik Arends (Makepad), 3D source-code viewer.** Video posted 12 Sep 2026. Flies through 2.5 M lines at "120+ FPS" (video capped at 60 by X). Memory went from 21 GB to 3.5 GB after adding indexes and streaming compression; source files appear as "layers of colored code blocks arranged in a spatial layout"; readable code up close. Not public; planned as a paid product. — [LavX News, 15 Sep 2026](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) [fetched]; [Tom's Hardware headline](https://www.tomshardware.com/tech-industry/developer-builds-viral-3d-source-code-visualizer-that-consumes-21gb-of-ram-flies-around-2-5-million-lines-of-code-at-over-120-frames-per-second) [fetched; body truncated, headline only]
- Arends on X: "51 million lines of code. Chromium codebase. Here you go. 120hz, 3gb ram use." — [x.com/rikarends/status/2100295688881852508](https://x.com/rikarends/status/2100295688881852508) [search snippet]
- Techtroduce (17 Sep 2026): the Chromium test ran on an **Apple M3 Max**; memory started around 8 GB and fell to about 3.5 GB after moving **level-of-detail data** and search indexes to a disk-backed cache. It zooms from repo-wide view to readable source. No rendering or font technique given. — [techtroduce.com](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/) [fetched]
- A community clone, code-map ("Inspired by Rik Arends' 'Makepad Scope' demo"), draws files as boxes sized by line count and shows code only when zoomed in; 3D city mode puts code on tower roofs. — [github.com/abyss464/code-map](https://github.com/abyss464/code-map) [fetched]
- Makepad's code editor renders with MSDF (author's page). — [ivanlugo.dev/prior-art/gpu-text](https://ivanlugo.dev/prior-art/gpu-text/) [fetched]
- Max Crofts: 15,914 glyphs is the top of his test, at 8–105 ms per frame. — [maxcrofts.com](https://maxcrofts.com/gpu-text-rendering/) [fetched]
- deck.gl: about 1 M items at 60 fps, 10–20 fps near 10 M, crashes between 10 M and 100 M (all layers, not text-specific). — [deck.gl performance](https://deck.gl/docs/developer-guide/performance) [search]
- Limberger et al. (Web3D 2024): instanced 3D glyphs, tested to about 50,000. troika: about 500 `Text` meshes. — [ivanlugo.dev/prior-art/gpu-text](https://ivanlugo.dev/prior-art/gpu-text/) [fetched]
- Non-text instancing reference: ChartGPU (WebGPU) reports 1 M points at 60 fps and a 35 M-point demo at about 72 fps. — [webgpu.com showcase](https://www.webgpu.com/showcase/chartgpu-webgpu-charts/) [search]
- CSS-Tricks: extruded 3D font geometry costs about 185,000 triangles for a 259-letter paragraph, which is why atlases are used for volume. — [css-tricks.com](https://css-tricks.com/techniques-for-rendering-text-with-webgl/) [search]
- glyph3d's own measurement: in a visible-mode spike, a Slug draw of 2.5 M sub-pixel glyphs cost about 14 ms against 10 ms for flat quads (overview pose, 0.44 px/em). — `out/GPU-DIRECTION-2026-10-09.md` lines 736–756 [local]

### Inferences
- **Flag: closest match to glyph3d.** Arends' viewer matches glyph3d on four axes: readable source text, whole-repo scale (bigger corpus: 51 M lines), true 3D fly-through, and an LOD system. It differs or is unknown on two: text technique (Makepad lineage suggests SDF/MSDF, not analytic curves; unverified) and openness (closed, unreleased). It is the most important item missing from the prior-art page.
- Rough scale (inference, not a published figure): at about 40 bytes per line, 51 M lines is about 2 GB of source, around 20× glyph3d's 97 MB flagship. That viewer cannot hold a slot per glyph in 3 GB. It must render via LOD or streaming, conceptually like glyph3d's Visible mode, not Instanced mode.
- For resident per-glyph instances, glyph3d's 95.2 M appears to be the largest documented figure found.

### Gaps
- No primary technical write-up from Arends was found: font technique, LOD tiers, glyphs drawn per frame and GPU work per frame are all unknown. Load-time reports conflict (60 s vs under 3 min indexing).
- No published TextLayer, Mapbox or game-engine benchmark at 10^7+ glyphs was found.

---

## 3. Has anyone done level-of-detail text where far text collapses into per-line or per-word quads ("washes"/greeking)?

### Takeaway
The *idea* is old and well established in 2D: Seesoft (1992) draws each line of code as a coloured pixel row; VS Code's minimap has drawn colour blocks instead of characters since 2017; tldraw swaps shapes for plain rects at low zoom. Arends' viewer has an LOD cache. But **no source describes a real-time 3D GPU renderer that substitutes a per-line wash quad for glyphs inside a perspective scene, by a pixel threshold, per frame.** "Greeking" as a GPU or real-time term turned up nothing.

### Cited Findings
- **Seesoft (Eick, Steffen, Sumner; IEEE TSE 18(11), 1992).** Each line of code becomes a thin coloured row of pixels (colour = age or statistic); up to 50,000 lines at once. — [TU Graz softvis survey](https://courses.isds.tugraz.at/ivis/surveys/ss2018/ivis-ss2018-g4-softvis-survey.pdf) [search]; [Saarland lecture slides](https://www.rw.cdl.uni-saarland.de/people/diehl/private/SoftVisVorles/slides/10_SV_SoftwareEvolution.pdf) [search]; [GMU lecture citation](https://people.cs.gmu.edu/~tlatoza/teaching/swe795f19/Lecture%207%20-%20Software%20Visualization.pdf) [search]
- **VS Code minimap, `editor.minimap.renderCharacters: false`** (v1.10, Feb 2017): "render blocks instead of characters in the Minimap", a "more schematic view". — [VS Code 1.10 release notes](https://code.visualstudio.com/updates/v1_10) [search]
- **tldraw.** Text outlines turn off below zoom 0.35, and the docs show a custom shape returning a plain `rect` below zoom 0.15. `getEfficientZoomLevel` holds steady while the camera moves on documents with more than 500 shapes. — [tldraw performance docs](https://tldraw.dev/sdk-features/performance) [search]; [tldraw canvas concept](https://tldraw-tldraw.mintlify.app/concepts/canvas) [search]
- **Greeking definition**: placeholders for unavailable content, or form over detail. — [Wikipedia: Greeking](https://en.wikipedia.org/wiki/Greeking) [search]
- **Arends' viewer** keeps "level-of-detail data" in a disk-backed cache; files read as "colored code blocks" from afar. — [techtroduce.com](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/) [fetched]; [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) [fetched]
- **code-map**: file boxes at overview, text only when zoomed. — [github.com/abyss464/code-map](https://github.com/abyss464/code-map) [fetched]
- **Makepad code editor**: alt-click minimap zoom (author's page). — [ivanlugo.dev/prior-art/gpu-text](https://ivanlugo.dev/prior-art/gpu-text/) [fetched]
- **glyph3d's wash tier**: a line under the glyph px threshold becomes one wash *box* (since C28, a box per line that recedes with back-wrapped depth). It is classified per frame by a GPU cull (`cull_lines` → glyph tier, wash tier, nothing) and costs about 0.005 ms layout for 137 k all-wash candidate lines on an RTX 5090. — `out/history/VISIBLE-MODE.md` lines 120–130, 382–411, 541–572 [local]

### Inferences
- Seesoft and the minimap are the conceptual ancestors of the wash tier, and both belong in the prior-art lineage: they are 2D, offline or per-file, and CPU-drawn. glyph3d's contribution is doing it per line, per frame, by projected pixel size, in 3D perspective, with a GPU cull and depth-correct boxes.
- Arends' viewer may do something similar, but nothing published says whether its far LOD is per line, per file or per block.

### Gaps
- No hits for "greeking" in GPU, game-engine or real-time rendering literature. Game-engine text LOD (e.g. distance fade of world-space signage) was not searched in depth.
- Figma's behaviour for zoomed-out text is undocumented in the sources found (one weak source says only that it is tile-based C++/WASM/WebGL).

---

## 4. Has anyone placed text in true 3D perspective at that scale (not billboards or labels)?

### Takeaway
Perspective-correct *technique* exists in several places: Dobbie 2016 (footnote), faf-text (tilted panes), JSlug (lit, shadow-casting), Slug's dynamic dilation, and Skia's path fallback. *Scale* in true 3D exists only in Arends' 2026 viewer (2.5 M to 51 M lines; technique unknown) and in glyph3d. The author's page already notes that the earlier rendered-text-in-3D prototypes (Code Park and others) were small and not GPU-instanced at repo scale.

### Cited Findings
- Dobbie: screen-space derivatives let the method antialias stretched, skewed, rotated or perspective-projected glyphs (footnote). Demo atlas of 377 glyphs over the PDF *Optimizing C++*; no performance figures (2 Jan 2016). — [wdobbie.com](https://wdobbie.com/post/gpu-text-rendering-with-vector-textures/) [fetched]
- faf-text: a pane tilted in 3D costs one matrix per frame; coverage is measured on the projected glyph, so tilted text is "as smooth as a flat one". — [meawoppl.github.io/faf-text](https://meawoppl.github.io/faf-text/) [fetched]
- Slug dynamic dilation uses the MVP matrix and viewport per vertex. — [terathon.com/blog/decade-slug.html](https://terathon.com/blog/decade-slug.html) [fetched]
- JSlug: a StandardMaterial variant "obeys lighting/casts shadows". — [three.js forum](https://discourse.threejs.org/t/three-slugtext-gpu-text-renderer/90599) [fetched]
- Skia: perspective glyphs are drawn as paths because no single SDF level fits. — [Skia commit](https://skia.googlesource.com/skia/+/76826fc91c0abcbd1c8aa4dbc3db4a9d9419e73f) [search]
- Arends: 3D fly-through of 2.5 M lines (51 M for Chromium), readable code up close. — [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) [fetched]; [x.com/rikarends](https://x.com/rikarends/status/2100295688881852508) [search]
- Lineage of 3D code text (Code Park, CodeHouse, VRIDE, etc.) and PABR kernel3d (boxes, no text) are on the author's page. — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/) [fetched]
- Popular-press lineage: SGI's File System Navigator (IRIX, 1993, *Jurassic Park*) is cited as the ancestor of Arends' viewer. — [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) [fetched]

### Inferences
- **Flag on the full combination.** Analytic curves + about 10^8 glyphs + true 3D perspective (pages, depth wrapping) + per-line LOD washes: **no source found matches all four.** Arends' viewer plausibly matches three (scale, 3D, LOD), with its text technique unknown. faf-text, JSlug and the Slug ports match two (analytic + perspective) at tiny scale.
- The z-order and depth problem glyph3d hit (a blended glyph pass that must write depth, coplanar-quad ulp losses; AGENTS.md pixel-ab notes [local]) is not discussed by any source found. It may be genuinely under-documented territory.

### Gaps
- No game-engine or demoscene example of large-volume world-space text (as opposed to labels) was found. Not searched: Shadertoy, pouet.net.

---

## 5. Has anyone loaded about 100 MB of text to a renderable state in under a second?

### Takeaway
No public system documents it. The only nearby figures are Arends' Chromium load (about 51 M lines in about 60 s, or "indexing under 3 minutes", reports differ) and the qualitative claims of large-file viewers (klogg: "10+ GB is not a problem", no throughput published). glyph3d's claimed about 177 ms for 97 MB / 95.2 M glyph instances to first visual (576.9 MB/s backend, late 2026-09, not re-measured) has no published peer.

### Cited Findings
- Arends/Chromium: the whole codebase loaded in 60 seconds (Tom's Hardware via LavX and Techtroduce); another report says indexing was cut to under 3 minutes. Hardware: M3 Max. — [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) [fetched]; [techtroduce.com](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/) [fetched]; [gotburnout.io](https://gotburnout.io/en/news/developer-loads-entire-chromium-codebase-into-3d-code-viewer-3417) [fetched]
- klogg: reads from disk without loading into memory, multithreaded with SIMD; "10+ Gb is not a problem"; faster than glogg at opening (maintainer claim). No GB/s figure. — [korben.info](https://korben.info/en/klogg-log-viewer-tool.html) [search]; [telecomhall](https://www.telecomhall.net/t/klogg-a-fast-advanced-log-explorer-based-on-glogg-project/11824) [search]
- Ghostty pre-alpha: about 120 fps while cat-ing a 6 MB file (throughput of terminal output, not load to resident). — [Ghostty performance mirror](https://www.mintlify.com/ghostty-org/ghostty/features/performance) [search]
- glyph3d: about 168 ms backend / about 177 ms visual init (flat), about 215/226 ms (syntax colour), 97.0 MB, 1,306 files, 95.2 M instances, Apple Silicon Metal; flagged in-repo as stale until re-measured. — `AGENTS.md` "Performance characteristics" [local]
- GPU layout on the GPU: `out/GPU-DIRECTION-2026-10-09.md` [local] states "Nobody ships this: Slug takes CPU-placed quads; Vello/Parley shape and wrap on the CPU; GPUI lays out on the CPU and sprites the glyphs." This search found nothing contradicting it: glyph_brush, glyphon and Slug Library all do layout on the CPU. — [docs.rs/gfx_glyph](https://docs.rs/gfx_glyph) [search]; [sluglibrary.com](https://sluglibrary.com/) [search]

### Inferences
- Large-file viewers render only what is in view (the Visible-mode idea in 2D), so "load" for them means indexing line offsets, not laying out every glyph. glyph3d's Instanced-mode figure is a different and stronger claim: every glyph placed in a 3D world. The comparison should say which one is meant.
- A GPU compute pass laying out only the visible lines from resident bytes (Visible mode) has no documented public precedent in the sources found.

### Gaps
- No published load-to-render benchmarks for Zed, Sublime, EmEditor or Makepad on 100 MB inputs were found.
- No source on GPU-side line breaking or prefix-sum text layout in a shipping product turned up (one targeted search).
