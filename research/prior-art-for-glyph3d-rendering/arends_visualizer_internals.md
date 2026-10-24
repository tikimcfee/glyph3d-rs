# Rik Arends' code visualizer ("Makepad Scope"): what is known about its internals (as of 2026-10-10)

**Labels used throughout:**
- **[PRIMARY]**: Rik Arends' own words (his X posts and replies), Makepad's public GitHub repo, or the tool's own UI as seen in screenshots/video thumbnails he posted.
- **[SECONDARY]**: press coverage (Tom's Hardware, Techtroduce, dev.ua, LavX, gotburnout, techora).
- **[INFERENCE]**: my reasoning from the above. Not a fact.

**Method note.** X posts were read through the fxtwitter JSON mirror (`api.fxtwitter.com/status/<id>`, `/2/profile/rikarends/statuses`, `/2/conversation/<id>`); xcancel.com is suspended. Citations point at the canonical x.com URL. I pulled his full timeline from 2026-08-29 to 2026-10-10 (612 posts) and the reply threads under the 14 visualizer posts. I also downloaded four video thumbnails and looked at them myself. Tom's Hardware's article body did not render on fetch (headline only), so its claims are cited through Techtroduce and dev.ua, which cite it.

**The headline answer.** The visualizer is called **Makepad Scope**. It is closed and in private beta, and none of its code is public. Rik answers internals questions in X replies, though, and those replies plus the UI screenshots give a fairly clear picture:
- **Layout:** a squarified treemap of the repo. Area is token count. An optional 3D mode extrudes blocks by log(tokens).
- **When layout happens:** precomputed at index time.
- **What is on disk:** an index that holds the connection graph, search indices and per-LOD geometry "sliced per LOD level for continuous reads", stream-compressed with LZ4.
- **LOD chain:** text → token-colour blocks → line blocks → area blocks → one block per file. He has said "4 LODs for the code files".
- **Rendering path:** it moved from a realtime texture-tile cache to "GPU geometry zoom, not tile based".
- **Glyphs:** "AI generated vector stacks … a bit like the Slug algorithm". Makepad's public text stack has had a Slug atlas since April 2026.

One important caveat: asked "What is your index architecture?", Rik replied "I honestly have no idea. I just told it to make it fast". The code was largely written by AI agents.

---

## 1. Is any of the visualizer's code public?

### Takeaway
No. Makepad Scope is a closed, private-beta product that Rik says he intends to sell, then ship "as-source", then open-source later. Nothing named scope, codeviz or visualizer exists on any public branch of `makepad/makepad` (`main`, `dev`, `work`, `builder`), and Rik's own GitHub has only an old `tracegl` repo. The nearest public relatives are the Makepad text stack, whose Slug atlas is public, and a public treemap disk-usage app (`apps/files`) that Rik had an AI build on 2026-08-31.

### Cited Findings
- [PRIMARY] The product name is "Makepad Scope". It shows in the window title of the Chromium video thumbnail. Rik: "Its called makepad scope, in private beta atm." — [x.com/rikarends/status/2100323422135820638](https://x.com/rikarends/status/2100323422135820638); also "Its makepad scope. A new product im / making" — [x.com/rikarends/status/2098435265903652923](https://x.com/rikarends/status/2098435265903652923)
- [PRIMARY] The beta page says "Makepad Scope and Makepad Amp are in beta". Access is by email request; the Makepad Builder then builds the apps on the user's own computer, under the Makepad Commercial License. — [makepad.nl/beta](https://makepad.nl/beta) (linked by Rik at [x.com/rikarends/status/2103255938996302016](https://x.com/rikarends/status/2103255938996302016))
- [PRIMARY] On selling it: "strangely enough for me, something i intend to sell as a product" — [x.com/rikarends/status/2098774768157655402](https://x.com/rikarends/status/2098774768157655402); "i'll go sell it for a small fee" — [x.com/rikarends/status/2099019162509066717](https://x.com/rikarends/status/2099019162509066717)
- [PRIMARY] On releasing source: "i'll release it with-source so you can ai tweak it and oss is later this year likely" — [x.com/rikarends/status/2099233145904136703](https://x.com/rikarends/status/2099233145904136703); "Product first, then OSS" — [x.com/rikarends/status/2100324551565443093](https://x.com/rikarends/status/2100324551565443093); and "2-3" in reply to "so... 6 months 😂" — [x.com/rikarends/status/2100324841601524143](https://x.com/rikarends/status/2100324841601524143)
- [PRIMARY] Public repo check, done with `gh api` on 2026-10-10. `makepad/makepad` has branches `builder`, `dev`, `main` and `work`. The `examples/`, `apps/` and `tools/` listings on dev/work/builder contain nothing named scope, codeviz or visualizer. Commits since 2026-08-01 are UI, platform and text fixes. — [github.com/makepad/makepad](https://github.com/makepad/makepad)
- [PRIMARY] `tools/merge_rik2_into_dev.sh` exists on `dev`, which points to a non-public `rik2` branch or fork. — [github.com/makepad/makepad/tree/dev/tools](https://github.com/makepad/makepad/tree/dev/tools)
- [PRIMARY] Rik's GitHub (`rikarends`) lists only `tracegl`, last updated 2024-03-28. — [github.com/rikarends](https://github.com/rikarends)
- [PRIMARY] The public treemap app `apps/files` contains `treemap.rs` (111 KB) and `treemap_view.rs` (166 KB). It is a **disk-usage** treemap with a streaming scan, squarified layout and Van Wijk cushion shading. Its layout is "pixel-bounded, not depth-bounded … stops only where a rectangle gets too small to see. Siblings too small to draw are collapsed into one 'N smaller items' rectangle". — [makepad/makepad apps/files/src/treemap.rs (dev)](https://github.com/makepad/makepad/blob/dev/apps/files/src/treemap.rs). Rik's posts about it: "Hey fable write me a disk storage visualizer, but like, jurassic park style" (2026-08-31) — [x.com/rikarends/status/2094501992668287463](https://x.com/rikarends/status/2094501992668287463); and a wasm build with "Code: https://github.com/makepad/makepad" — [x.com/rikarends/status/2095255303541403765](https://x.com/rikarends/status/2095255303541403765)
- [PRIMARY] He calls Scope's layout a treemap: "its a treemap i think its called" — [x.com/rikarends/status/2098800191876858072](https://x.com/rikarends/status/2098800191876858072); "Yep treemaps." — [x.com/rikarends/status/2099422904026857506](https://x.com/rikarends/status/2099422904026857506)
- [SECONDARY] Press agrees it is not publicly available. — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/), [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639)
- [PRIMARY] I found no crates.io crate for it. Codescape's README notes that "Makepad is not published to crates.io any more". — [github.com/WIZARDspace/codescape](https://github.com/WIZARDspace/codescape)

### Inferences
- [INFERENCE] `apps/files` (2026-08-31, public) came eight days before the first Scope post (2026-09-08), and both are AI-built treemaps in Makepad. The disk app is a plausible ancestor of Scope's layout and LOD-culling ideas: a pixel-bounded recursion with small siblings collapsed into one rectangle. This is unconfirmed. The disk app sizes by bytes; Scope sizes by tokens.
- [INFERENCE] Rik says Scope will ship "with source" to paying users. Its internals may therefore become inspectable to beta or paying users before any OSS release.

### Gaps
- No public source, design doc, blog post or talk on Scope's internals.
- The private `rik2` branch's contents are unknown.

---

## 2. Text rendering technique

### Takeaway
Rik says the visualizer's text uses "AI generated vector stacks … a bit like the Slug algorithm", and that it is the same text renderer as his code editor. Makepad's public text stack (`draw/src/text/`) holds three outline paths, so a Slug-style analytic-coverage path is plausible but unverified for Scope:
- a grayscale **SDF** (ESDT from rasterized coverage);
- an **MSDF** path;
- a **Slug atlas**: curve texture plus band texture, cubics converted to quadratics. It arrived in PR #1042 (2026-04-16), "Switch to SLUG/DrawGlyph font drawing stack".

At distance, readable text is not drawn at all; it is replaced by coloured blocks (section 3).

### Cited Findings
- [PRIMARY] Asked about his font rendering under the Chromium post: "I have long replaced the font rendering with AI generated vector stacks. Currently i think its a bit like the Slug algorithm." — [x.com/rikarends/status/2100838964672286884](https://x.com/rikarends/status/2100838964672286884) (question: [x.com/danlynch/status/2100730740862595090](https://x.com/danlynch/status/2100730740862595090))
- [PRIMARY] "its using a textrenderer that i also have in my code editor. all i need to do is make the rendering line up so it can just 'swap in'". The context is turning the zoomed-in code views into real editors. — [x.com/rikarends/status/2100954892760142092](https://x.com/rikarends/status/2100954892760142092)
- [PRIMARY] `makepad/makepad` `draw/src/text/` on `dev` contains `sdfer.rs`, `msdfer.rs`, `slug_atlas.rs`, `rasterizer.rs`, `glyph_outline.rs`, `font_atlas.rs`, `layouter.rs`, `shaper.rs` and `coretext.rs`, among others. — [github.com/makepad/makepad/tree/dev/draw/src/text](https://github.com/makepad/makepad/tree/dev/draw/src/text)
- [PRIMARY] The constants in `slug_atlas.rs` describe the Slug layout:
  - `CURVE_TEX_WIDTH = 2048` and `BAND_TEX_WIDTH = 2048`;
  - `CUBIC_TO_QUAD_TOLERANCE = 0.05` and `MAX_CUBIC_SPLIT_DEPTH = 12`;
  - RGBA f32 texels;
  - each glyph records `curve_offset/curve_count` and `band_offset/band_count`;
  - an append-only `curve_data` soft-capped at about 4 MB, with this comment: "Slug glyphs are only used for very large text, so a realistic on-screen working set is far below this".
  
  — [draw/src/text/slug_atlas.rs (dev)](https://github.com/makepad/makepad/blob/dev/draw/src/text/slug_atlas.rs)
- [PRIMARY] PR #1042 (Kevin Boos, merged 2026-04-16): "because compiling the SLUG shaders can be incredibly slow, we currently use a hybrid approach that first uses SDF (very fast shader compilation, e.g., <200ms) … then switch to SLUG once the shaders are compiled asynchronously. Currently SLUG isn't actually used on Linux/Windows unless the font is quite large". — [github.com/makepad/makepad/pull/1042](https://github.com/makepad/makepad/pull/1042)
- [PRIMARY] In `rasterizer.rs`, `OutlineRasterizationMode` is `Sdf | Msdf`. MSDF falls back to SDF below `min_request_dpxs_per_em` or when outline complexity is too high, and MSDF jobs are queued for worker threads. The SDF path converts coverage to an SDF via `libs/sdfer` (`esdt::glyph_to_sdf`). — [draw/src/text/rasterizer.rs](https://github.com/makepad/makepad/blob/dev/draw/src/text/rasterizer.rs), [draw/src/text/sdfer.rs](https://github.com/makepad/makepad/blob/dev/draw/src/text/sdfer.rs)
- [PRIMARY] On syntax colouring: "it has like 4 lods for the code files, syntax highlights them on 16 cores and so on." — [x.com/rikarends/status/2097435401929498733](https://x.com/rikarends/status/2097435401929498733)
- [SECONDARY] Press gives no glyph technique; it only says lines stay readable up close. — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/)

### Inferences
- [INFERENCE] "AI generated vector stacks … a bit like Slug" plus "same textrenderer as my code editor" most likely means Scope draws near-field glyphs analytically from quadratic curves banded per glyph, as `slug_atlas.rs` does. That is the same family as glyph3d's Slug-style coverage. Whether Scope uses Makepad's public `SlugAtlas` or a private variant (perhaps on the `rik2` line) is unknown. The public PR says Slug is used only for large text on Linux/Windows. Scope's demos ran on macOS (M3 Max), where that restriction may not apply.
- [INFERENCE] Readable glyphs appear only at the nearest LOD. At every other zoom level the "text" is coloured rectangles (section 3), so glyph rendering is not where Scope's scale comes from. Its glyph workload is bounded by what is legible on screen.

### Gaps
- No statement on the glyph count per frame, the atlas budget, or whether glyph positions are computed on CPU or GPU.
- No statement on whether the near-field text uses Makepad's shaper/layouter (CPU) or something custom.

---

## 3. Layout, LOD, disk-backed caches, and what's drawn at distance

### Takeaway
**Layout.** A treemap, with area proportional to **token count**. A 3D mode extrudes blocks by log(tokens). The layout is computed at **index time** and is not relaid out in realtime: Rik says "it can't do that".

**What "disk-backed LOD caches" means.** At index time the tool builds a disk database holding:
- the connection (call/reference) graph;
- search indices;
- per-LOD render data, "sliced per LOD level for continuous reads", with streaming LZ4 compression.

The viewer then streams what it needs.

**What is drawn at distance.** An explicit chain: text → per-token coloured blocks → each line as a block → areas as blocks → each file as one block.

**How the renderer changed (2026-09-12 to 09-16):**
- a realtime texture-tile cache at first;
- then "geometry tiles into realtime image tiles";
- finally "GPU geometry zoom, not tile based".

He says most of the remaining RAM is GPU buffers.

### Cited Findings
**Layout**
- [PRIMARY] "This is indeed more just a fileview, except its on token count not filesize." — [x.com/rikarends/status/2097306939373850982](https://x.com/rikarends/status/2097306939373850982)
- [PRIMARY] Sep 8 screenshot: a perspective view of an extruded treemap (a "city" look), with the Inspector reading "Height · Tokens · logarithmic · directories at ground". The toolbar has 2D-grid, cube (3D) and perspective toggles. — image in [x.com/rikarends/status/2097393306762191147](https://x.com/rikarends/status/2097393306762191147) ([pbs.twimg.com/media/HRtwEGfakAAdXr9.jpg](https://pbs.twimg.com/media/HRtwEGfakAAdXr9.jpg?name=orig))
- [PRIMARY] Sep 12 and Sep 16 thumbnails: flat top-down 2D treemaps with directory labels.
  - The Makepad repo view's inspector shows "5947 files indexed", "entities 721803 · 1336912 edges", "lexical references 705150".
  - The Chromium view is titled "Makepad Scope". Selecting `tags_unittest.cc` shows "2355 source lines · 106758 bytes".
  
  — [x.com/rikarends/status/2098710248164868534](https://x.com/rikarends/status/2098710248164868534), [x.com/rikarends/status/2100295688881852508](https://x.com/rikarends/status/2100295688881852508)
- [PRIMARY] Sep 14 diff view thumbnail: a perspective-tilted, depth-of-field-blurred treemap plane, with the changed files of a commit lifted onto a floating layer above it and "wires" to their locations. Its on-screen help reads "Double-click: focus / source · Scroll: zoom · Drag: pan · Right drag: rotate · Esc: back · Enter: editor". Rik's captions: "I added git diff viewing" / "Of course with lots of gratuitous gaussian blurs" / "Cute wires". — [x.com/rikarends/status/2099537063628394499](https://x.com/rikarends/status/2099537063628394499), [x.com/rikarends/status/2099537514860085755](https://x.com/rikarends/status/2099537514860085755)
- [PRIMARY] "As long as you dont relayout the whole thing in realtime (it can't do that) you can visualise quite niceyl." — [x.com/rikarends/status/2098765277823373597](https://x.com/rikarends/status/2098765277823373597)
- [PRIMARY] "Going to have to do a lot more work at index time (layout, tile preparation, cache to disk), keep the graph in a disk based db. Can't really get away with doing things at runtime for a codebase this big." — [x.com/rikarends/status/2099030204962275409](https://x.com/rikarends/status/2099030204962275409); "Lets see if i can get the ai to bake down enough indexes/layouts at index time" — [x.com/rikarends/status/2099054563902767134](https://x.com/rikarends/status/2099054563902767134)
- [PRIMARY] "each independent code chunk is indexed so i can also reorder them somehow." — [x.com/rikarends/status/2098782541897494541](https://x.com/rikarends/status/2098782541897494541)

**LOD chain and far-field drawing**
- [PRIMARY] "Its LOD'ed at some point yr not seeing the actual text anymore just a tiny version of it. It goes text->colors of the blocks in the text->lines are blocks->areas are blocks->file is a block" — [x.com/rikarends/status/2099174617009057841](https://x.com/rikarends/status/2099174617009057841)
- [PRIMARY] "it has like 4 lods for the code files" — [x.com/rikarends/status/2097435401929498733](https://x.com/rikarends/status/2097435401929498733); "It is of course heavily LOD'ed" — [x.com/rikarends/status/2098795493677559875](https://x.com/rikarends/status/2098795493677559875); "every single render trick LOD, tilecache (realtime tho)" — [x.com/rikarends/status/2098764777367498927](https://x.com/rikarends/status/2098764777367498927)

**Renderer evolution: tiles, then GPU geometry**
- [PRIMARY] Sep 12, 3.4M-LOC repo: "It shows search results now as a layer on top of the texture tiles. The 2D tile is really fast." — [x.com/rikarends/status/2098765277823373597](https://x.com/rikarends/status/2098765277823373597); "I think its just holding on to too many tilechaches. But without that i think it used 10gb ram" — [x.com/rikarends/status/2098762735286730986](https://x.com/rikarends/status/2098762735286730986)
- [PRIMARY] He declined a prerendered pyramid: "if i pre-render it google maps style into a big image pyramid that would definitely work … you could do this in 2001 if you prerender the tiles" — [x.com/rikarends/status/2098822103948554679](https://x.com/rikarends/status/2098822103948554679), [x.com/rikarends/status/2098822354570743966](https://x.com/rikarends/status/2098822354570743966); "of course i can bake pngs but thats not really a sport" — [x.com/rikarends/status/2098903865764503597](https://x.com/rikarends/status/2098903865764503597); "Its much better to keep rendering dynamic but do the data in a smarter way." — [x.com/rikarends/status/2099038119580282916](https://x.com/rikarends/status/2099038119580282916)
- [PRIMARY] Sep 13: "No need for image tiles. It's all geometry tiles into realtime image tiles." — [x.com/rikarends/status/2099189540040679740](https://x.com/rikarends/status/2099189540040679740)
- [PRIMARY] Sep 13: "previous ram usage was parsing the entire codebase and graph into ram, and on the fly generating gpu data. Now all of that is preprocessed and cached (about 3gb of data for a chromium size codebase)" — [x.com/rikarends/status/2099117618258915399](https://x.com/rikarends/status/2099117618258915399)
- [PRIMARY] Sep 16: "3-8gb for chromium codebase-scale. Most of that i think is all the gpu buffers to keep zooming fast. I now do gpu geometry zoom not tile based" — [x.com/rikarends/status/2100096105107116167](https://x.com/rikarends/status/2100096105107116167)
- [PRIMARY] On density tuning: "Now just need to tweak how much data i chuck on the GPU to get it to be as perfectly pretty as the old one at 120hz. It's trivial to overload it" — [x.com/rikarends/status/2099176800366506009](https://x.com/rikarends/status/2099176800366506009); "is 120hz since thats a fairly specific amount of visible density" — [x.com/rikarends/status/2099176053839179844](https://x.com/rikarends/status/2099176053839179844)

**Disk caches, indices and memory**
- [PRIMARY] "I just had to make a proper disk cache for the LOD and for the search indices." — [x.com/rikarends/status/2099176053839179844](https://x.com/rikarends/status/2099176053839179844); "now that i have a disk db based connection graph and LOD cache" — [x.com/rikarends/status/2099104007088177258](https://x.com/rikarends/status/2099104007088177258)
- [PRIMARY] "it indexes on load / when changed and keeps it on disk sliced per LOD level for continuous reads." — [x.com/rikarends/status/2099752816705204376](https://x.com/rikarends/status/2099752816705204376)
- [PRIMARY] "My guidance: Yea so i heard about logarithms. And eh yea indexes, and streaming LZ4 compression. Yea. eh. yea go fix." (instructions to his AI agents) — [x.com/rikarends/status/2098888446139924855](https://x.com/rikarends/status/2098888446139924855); "Astra is ripping out every single linear datastructure in the code visualiser" — [x.com/rikarends/status/2098923056513024207](https://x.com/rikarends/status/2098923056513024207); "bake/cache/index and memory access optimise everything to ssd" — [x.com/rikarends/status/2099035980653072862](https://x.com/rikarends/status/2099035980653072862)
- [PRIMARY] "I just loaded the entire codebase into ram, parsed and interconnected and then i cached a ton of other things too. All of that sits now neatly on disk as a properly structured cache." — [x.com/rikarends/status/2099111073143984325](https://x.com/rikarends/status/2099111073143984325)

**RAM timeline, all [PRIMARY]**
- 21 GB on the 3.4M-LOC Makepad repo — [2098711123734253878](https://x.com/rikarends/status/2098711123734253878)
- Chromium: "going to eat 80gb" — [2098861849823289840](https://x.com/rikarends/status/2098861849823289840)
- "about 83 GiB RAM" — [2098874480558780838](https://x.com/rikarends/status/2098874480558780838)
- "100gb ram usage" — [2098884287487508877](https://x.com/rikarends/status/2098884287487508877)
- "down to 48gb" — [2098923056513024207](https://x.com/rikarends/status/2098923056513024207)
- 3.5 GB — [2099176053839179844](https://x.com/rikarends/status/2099176053839179844)
- "Full 120hz … at 8gb memory usage on my m3 max" — [2099471743664472315](https://x.com/rikarends/status/2099471743664472315)
- "3-8gb" — [2099424721850728550](https://x.com/rikarends/status/2099424721850728550)
- "120hz, 3gb ram use" — [2100295688881852508](https://x.com/rikarends/status/2100295688881852508)

**Correction and hardware**
- [PRIMARY] On the "2.5M lines" demo: "Its actually 3.4M lines of code." — [x.com/rikarends/status/2098764777367498927](https://x.com/rikarends/status/2098764777367498927)
- [PRIMARY] Hardware: M3 Max — [x.com/rikarends/status/2099471743664472315](https://x.com/rikarends/status/2099471743664472315); a 57" Samsung at 8K — [x.com/rikarends/status/2098778272226066477](https://x.com/rikarends/status/2098778272226066477)
- [SECONDARY] Techtroduce warns that the 21 GB and 3.5 GB figures come from different workloads and are not a before/after pair. — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/)

**Process caveat**
- [PRIMARY] Asked "What is your index architecture?" ([x.com/Carlintveld/status/2097589729231356190](https://x.com/Carlintveld/status/2097589729231356190)), Rik replied: "I honestly have no idea. I just told it to make it fast." — [x.com/rikarends/status/2097590004285411667](https://x.com/rikarends/status/2097590004285411667). "Astra built me an entire in house rust-analyser, git library, viz engine." — [x.com/rikarends/status/2097207537829294465](https://x.com/rikarends/status/2097207537829294465)

### Inferences
- [INFERENCE] **The architecture: precomputed geometry streamed off disk, GPU drawing.** The likely shape is:
  - at index time: tokenize, highlight, analyze, lay out the treemap, and emit per-LOD geometry ("geometry tiles") — coloured rectangles per token, per line, per region and per file — written to disk in LOD-sliced, LZ4-compressed chunks;
  - at runtime: stream the chunks needed by the camera's zoom and footprint into GPU buffers and draw them as instanced quads ("gpu geometry zoom"), with glyphs only at the nearest LOD.
  
  This is an inference from the quoted pieces, not a described design.
- [INFERENCE] **Contrast with glyph3d's Visible mode.** glyph3d keeps raw bytes, a line table and a trie resident on the GPU and lays out only visible lines per frame in a compute shader; far lines are one "wash" quad each. Scope instead lays out once at index time and stores pre-baked per-LOD geometry on disk. Scope's "lines are blocks" level is directly analogous to glyph3d's per-line wash. Scope adds coarser levels (areas, files) and a finer one (per-token colour blocks before glyphs). Scope trades index time and disk (about 3 GB for Chromium) for no per-frame layout. glyph3d trades per-frame GPU layout for near-zero preprocessing (about 170 ms for 97 MB).
- [INFERENCE] **Where the 120 Hz comes from.** Scope's 120 Hz is a density-budget property. Rik repeatedly ties it to "visible density" and how much data goes to the GPU, and he notes that the first 120 Hz on 3.4M LOC was "exactly sized right by luck". It is not a property of fast layout.
- [INFERENCE] **2D or 3D?** The default view is a flat 2D treemap with smooth zoom. The 3D modes (extrusion by log tokens, a tilted perspective plane with lifted diff layers) are optional view modes. Press framing of "code canvases suspended in 3D space" ([dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639)) overstates the 3D-ness relative to the thumbnails. A Russian outlet's description, "2D map zooming into 3D", fits better ([techora.ru](https://techora.ru/news/51-mln-strok-koda-chromium-vizualizirovali-2026-09-18)). Text is laid out on the treemap plane, not in free 3D space.

### Gaps
- The geometry format, the per-LOD switching thresholds, the GPU buffer budget, whether culling is CPU- or GPU-side, and whether compute shaders are used: all unstated.
- The database engine behind the "disk db" (custom or embedded, e.g. sled/redb/SQLite) is unstated.
- I could not view the videos themselves, only thumbnails. The zoom transition into readable text is described in posts but was not examined frame by frame.

---

## 4. Load/index time and what dominates it (the 60 s vs "under 3 min" vs 10 min discrepancy)

### Takeaway
The figures are all Rik's, from different days and pipeline versions:
- **Sep 12:** Chromium "indexed in 79 seconds and built the map in another 46" (about 2 minutes, about 83 GiB RAM).
- **Sep 13–15:** "60 seconds to index/prepare the whole thing" (the 60 s the press quotes).
- **Sep 16:** after adding "15 language backends and static analysis", "10 minutes to index the whole repo … can be pushed down to like 2", then "It's at 2m25 now" the same evening. That is gotburnout's "under 3 minutes".

So the 60 s predates the multi-language static analysis, and the 10 min to 2m25 figures include it. What dominates is the analysis and indexing (parsing, cross-referencing, the graph), not the rendering preparation. Rik says CPU cores were not saturated.

### Cited Findings
- [PRIMARY] Sep 12: "The last full run indexed Chromium in 79 seconds and built the map in another 46, using about 83 GiB RAM. Search worked; reopening the cache and landing on readable source are the remaining blockers." It reads like a pasted agent status line. — [x.com/rikarends/status/2098874480558780838](https://x.com/rikarends/status/2098874480558780838)
- [PRIMARY] Sep 13: "For the chromium codebase it now takes 60 seconds to index/prepare the whole thing on my m3 max." — [x.com/rikarends/status/2099210923529412924](https://x.com/rikarends/status/2099210923529412924)
- [PRIMARY] Sep 15: "it indexes on load / when changed and keeps it on disk sliced per LOD level for continuous reads. Chromium codebase takes 60 seconds and this one like 8 or so." "This one" is the Makepad repo. — [x.com/rikarends/status/2099752816705204376](https://x.com/rikarends/status/2099752816705204376)
- [PRIMARY] Sep 16: "It also now has 15 language backends and static analysis, jump callers, look at diffs." — [x.com/rikarends/status/2100296634894549500](https://x.com/rikarends/status/2100296634894549500); "It did take 10 minutes to index the whole repo btw. But i think that can be pushed down to like 2. My CPU cores weren't saturated" — [x.com/rikarends/status/2100301088351998432](https://x.com/rikarends/status/2100301088351998432); "It's at 2m25 now" — [x.com/rikarends/status/2100352410715644013](https://x.com/rikarends/status/2100352410715644013)
- [PRIMARY] Earlier, on the 3.4M-LOC repo: "I think it parses/caches the entire codebase into ram at startup. That takes a few seconds, like 10 or so on my mac." — [x.com/rikarends/status/2098767677560860856](https://x.com/rikarends/status/2098767677560860856); "It analyses 2.5m lines of code in about 10 seconds" (the custom Rust analyser) — [x.com/rikarends/status/2097591174357799343](https://x.com/rikarends/status/2097591174357799343)
- [PRIMARY] Analyzers: "Had AI build me a C++ analyser for it" (Chromium) — [x.com/rikarends/status/2098789366063624272](https://x.com/rikarends/status/2098789366063624272); "complete ultrafast custom static rust analyser built in" — [x.com/rikarends/status/2097591104921104809](https://x.com/rikarends/status/2097591104921104809); "made it 50x faster and adding 10s of langauges over the last day or two" — [x.com/rikarends/status/2099548069075255334](https://x.com/rikarends/status/2099548069075255334)
- [PRIMARY] The UI inspector shows analysis coverage, which indicates static analysis is part of indexing. On the Makepad repo: "known 620463 relations … unresolved 1782258 paths … entities 721803 · 1336912 edges … unknown method dispatch 607271 sites … 5947 files indexed". — thumbnail of [x.com/rikarends/status/2098710248164868534](https://x.com/rikarends/status/2098710248164868534)
- [SECONDARY] "loaded in roughly 60 seconds" — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/), [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639), [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps); "indexing time reduced to under 3 minutes" — [gotburnout.io](https://gotburnout.io/en/news/developer-loads-entire-chromium-codebase-into-3d-code-viewer-3417)

### Inferences
- [INFERENCE] "Load" in the press really means a full index build (analysis, layout and LOD bake). Reopening an indexed repo from the disk cache is presumably much faster, but no figure has been posted.
- [INFERENCE] **Comparison with glyph3d.** glyph3d's roughly 170 ms covers glyph layout only, with no semantic analysis. Scope's minutes are dominated by building a cross-reference graph over 15 languages plus pre-baked LOD. The two numbers do not measure the same work and should not be compared head to head. For render preparation alone, Scope's "built the map in another 46 [s]" (Sep 12, before optimization) is the closest analogue, and it is still a precompute rather than per-frame work.

### Gaps
- No breakdown of the 2m25 figure into parse, analysis, layout and LOD bake.
- No warm-start (cache reopen) time.
- No numbers on non-Apple hardware. He mentioned "a dual GPU linux situation" ([x.com/rikarends/status/2098555823597158787](https://x.com/rikarends/status/2098555823597158787)) but posted no Scope numbers from it.

---

## 5. Follow-up posts, threads, interviews, community discussion

### Takeaway
Nearly all technical information is in Rik's own X replies, dated 2026-09-08 to 09-24. I found:
- no blog post, talk, podcast, YouTube stream, HN thread or Reddit thread on Scope's internals;
- that the press pieces add nothing beyond his posts (the gotburnout "engineering breakdown" is paywalled);
- in October, a related git-history animation of the Linux kernel ("I'm going to ship this to the world") that is probably Scope's git layer.

### Cited Findings
- [PRIMARY] Timeline of the visualizer posts:
  - Sep 8, "a new kind of AI-IDE … large scale code visualiser + AI worklane system. Code editors only exist as viewers as you zoom in and shatter by type" — [2097207150850273640](https://x.com/rikarends/status/2097207150850273640)
  - Sep 8, "My codebase viz is getting fancier" — [2097393306762191147](https://x.com/rikarends/status/2097393306762191147)
  - Sep 10, "Show me all the 2.5M lines" — [2098148745384702340](https://x.com/rikarends/status/2098148745384702340)
  - Sep 12, the 120 Hz demo — [2098710248164868534](https://x.com/rikarends/status/2098710248164868534)
  - Sep 12, Chromium at 100 GB — [2098884287487508877](https://x.com/rikarends/status/2098884287487508877)
  - Sep 13, 3.5 GB — [2099176053839179844](https://x.com/rikarends/status/2099176053839179844)
  - Sep 14, 120 Hz on Chromium — [2099471743664472315](https://x.com/rikarends/status/2099471743664472315)
  - Sep 14, git diffs — [2099537063628394499](https://x.com/rikarends/status/2099537063628394499)
  - Sep 16, Chromium video — [2100295688881852508](https://x.com/rikarends/status/2100295688881852508)
- [PRIMARY] Intended use: "this is designed to be an AI agent activity monitor" — [x.com/rikarends/status/2098800602855694509](https://x.com/rikarends/status/2098800602855694509); "My plan was to see your agents operate on this in realtime as a visualisation layer, also to see git-diffs at larger scales. i also parse every single code relationship (atleast in Rust)" (to Karpathy) — [x.com/rikarends/status/2098782231971999770](https://x.com/rikarends/status/2098782231971999770)
- [PRIMARY] Oct 2–3: "observing the linux kernel through git mutations is just astounding … Im going to ship this to the world" — [x.com/rikarends/status/2106167805527908705](https://x.com/rikarends/status/2106167805527908705), [x.com/rikarends/status/2106174283902865731](https://x.com/rikarends/status/2106174283902865731)
- [PRIMARY] Beta access was offered through his Discord: "join my discord ill promote you to betatester" — [x.com/rikarends/status/2098488070546874817](https://x.com/rikarends/status/2098488070546874817)
- [SECONDARY] The coverage of record:
  - [Tom's Hardware](https://www.tomshardware.com/tech-industry/developer-builds-viral-3d-source-code-visualizer-that-consumes-21gb-of-ram-flies-around-2-5-million-lines-of-code-at-over-120-frames-per-second) (body not retrievable)
  - [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/) ("moved LOD information into a disk-backed cache"; no verbatim quotes)
  - [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639)
  - [LavX](https://news.lavx.hu/article/developer-builds-3d-source-code-visualizer-that-navigates-2-5-million-lines-at-120-fps) ("streaming compression"; does not mention LOD)
  - [gotburnout.io](https://gotburnout.io/en/news/developer-loads-entire-chromium-codebase-into-3d-code-viewer-3417) (paywalled; "indexing time reduced to under 3 minutes")
  - [techora.ru](https://techora.ru/news/51-mln-strok-koda-chromium-vizualizirovali-2026-09-18)
- [SECONDARY] Searches turned up no HN or Reddit thread specifically about it.

### Inferences
- [INFERENCE] Every press figure traces back to the posts above, and none of the articles carries independent technical reporting. For internals, treat Rik's replies as the only primary source.

### Gaps
- Makepad Discord content was not accessible.
- Rik's replies to other people's threads (outside his own threads) are not in the profile feed and were not exhaustively searched.

---

## 6. Codescape (WIZARDspace/codescape): an "inspired by" project, kept separate from Arends facts

### Takeaway
Codescape is a public Rust + Makepad repo explicitly "inspired by Rik Arends' Makepad source visualizer". The GitHub org is `WIZARDspace`; I could not locate a "flop-labs" GitHub org, but its example repo is called "flop-core". Its design is independent. It is not evidence of how Scope works:
- directories are extruded blocks, files are flat tiles of real text;
- two LODs: a mipmapped "minimap" atlas at distance (one texel = two characters of one line, coloured by syntax class; one quad per file), and SDF-font glyph quads near (capped at 450k glyphs);
- about 0.6 s scan and layout for a ~1M-line repo.

### Cited Findings
- [PRIMARY: codescape] "It is inspired by Rik Arends' Makepad source visualizer. Each directory is a raised block … Each file is a flat tile holding its real text, split into columns so the tile stays roughly square." — [github.com/WIZARDspace/codescape](https://github.com/WIZARDspace/codescape)
- [PRIMARY: codescape] "**Far:** every file is one quad. It samples a mipmapped minimap atlas where each texel covers two characters of one line, coloured by syntax class. The whole repo (about 4.6k files and 1M lines) costs about 4.6k quads. **Near:** files that appear large on screen also get one quad per character, drawn from a signed-distance-field font atlas. Nearest files come first, up to 450k glyphs" — [README](https://github.com/WIZARDspace/codescape)
- [PRIMARY: codescape] "A scan and layout of the example flop-core repository takes about 0.6 s. All text is uploaded once; the camera changes only uniforms and the near-glyph set." It builds against Makepad `dev` pinned at `88639f79` (2026-09-22). The minimap goes up as an explicit mip chain; on Metal the CPU box-filters levels 1–4. — [README](https://github.com/WIZARDspace/codescape)
- [PRIMARY: codescape] The repo was created 2026-10-10 (GitHub API `created_at`). — [api.github.com/repos/WIZARDspace/codescape](https://api.github.com/repos/WIZARDspace/codescape)
- [Other] `dekaruntime/explorer-3d` is also listed as inspired by the same wave, described as a "Google-Maps-style 3D codebase visualizer". I did not examine it. — [github.com/dekaruntime/explorer-3d](https://github.com/dekaruntime/explorer-3d)

### Inferences
- [INFERENCE] Codescape's far LOD (a per-file texture of line × 2-char texels) is an image-pyramid approach of the kind Rik said he could do but chose not to ("bake pngs … not really a sport"). Scope instead moved to GPU geometry. Codescape's near path (SDF glyph quads from Makepad's atlas) is also not what Rik describes ("a bit like Slug"). So codescape reflects its author's reading of the demo, not Scope's design.
- [INFERENCE] For glyph3d, codescape is useful as a second data point:
  - its far-field per-line colour texels are close to glyph3d's per-line wash, at sub-line resolution;
  - its 450k-glyph near cap is a budget glyph3d's Visible mode does not need to impose the same way.

### Gaps
- No performance figures from codescape beyond 0.6 s layout (no fps or RAM numbers).
- The "flop-labs" org name from the assignment could not be matched. Only `WIZARDspace/codescape` and the "flop-core" example repo name were found.
