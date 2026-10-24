# Prior art: the whole codebase as literal text in navigable space

Researched 2026-10-10. Scope: systems that render the actual source text (every glyph, or a line-faithful silhouette of it) in 2D/3D space at whole-repository scale, compared against glyph3d (Rust/wgpu; flagship 1,306 files / 97 MB / ~95 M glyph instances laid out in ~170 ms cold; pages/columns in world space; backward-in-depth wrapping; one "wash" quad per far line, analytic glyphs near; glyph picking, syntax colour, emoji, agent-transcript browser).

## What the author's prior-art page already covers, and what it is missing

### Takeaway
The author's survey (ivanlugo.dev/prior-art) already covers the academic rendered-text lineage, code cities, VR IDEs, web toys and GPU text. It does NOT list the two closest items found here: **Rik Arends' Makepad 3D source visualizer (September 2026)** and its open-source follow-on **codescape (flop-labs, September 2026)**. It also does not list **Wideboard** (Austin Appleby, 2013, revived 2023), which renders all of Linux's source text in a browser tab.

### Cited Findings
- The page is organised into six volumes. Vol 01, "Rendered-Text Lineage": Code Bubbles, Code Canvas, Code Park ("2017 UCF master's thesis, the closest published 3D precedent, never released"), CodeHouse, IDEvelopAR, Primrose, VRIDE, Haystack, Padioleau's Codemap, plus empirical 3D code-reading studies — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Vol 02, "City Tradition": CodeCity, ExplorViz, IslandViz, CodeMetropolis, EvoStreets/SoftVis3D, CityVR, VR City, GoCity/JSCity/PHPCity, CodeCharta, SecCityVR — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Vol 03, "VR IDEs & Commercial Code Maps": Primitive.io, CodeSee, Sourcetrail, CodeScene, CAST Imaging, Structure101, Glamorous Toolkit, and the web IDEs (Codespaces, vscode.dev, Gitpod) — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Vol 04, "Web Toys & Generative Art": Gource, code_swarm, Codeology, CodeFlower, GitHub repo-visualizer, anvaka's galaxies, contribution-graph cities, GitDiagram, CodeBoarding, Tornhill's crime-scene tools — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Vol 05, "Rendering Text at Scale": Slug (described as public domain in 2026), MSDF, troika-three-text ("stalls at about 500 instances"), Zed's GPUI, Makepad, GT's Bloc, instanced-glyph papers, and a "2002 Linux-kernel-in-3D demo" — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Makepad appears on the page only as a GPU UI framework (Vol 05). Its creator's 2026 code visualizer does not appear — [ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)
- Wideboard's 2023 HN thread includes a commenter linking **LookAtThat** (github.com/tikimcfee/LookAtThat), "a 3D, GPU-based text visualizer and tracer for Swift" on macOS. That repo belongs to the glyph3d author's own GitHub account (tikimcfee), so it is the author's own predecessor and not external prior art — [HN 34758336](https://news.ycombinator.com/item?id=34758336); [author GitHub linked from prior-art page](https://ivanlugo.dev/prior-art/)
- glyph3d-js (tikimcfee) shows up in search as a three.js/WebGPU renderer of whole source files as GPU-instanced text, one InstancedBufferGeometry draw per text grid — [github.com/tikimcfee/glyph3d-js](https://github.com/tikimcfee/glyph3d-js)

### Inferences
- The page's claim that Code Park is "the closest published 3D precedent" still holds for *peer-reviewed* work. It no longer holds for prior art overall: Arends' visualizer (Sept 2026) is closer, and codescape is a public open-source implementation of the same idea.

### Gaps
- I did not open the individual volume pages, only the index, so per-item claims on those pages were not checked.

## Ranked inventory: closest to glyph3d first

### Takeaway
**At least one system does essentially what glyph3d does: Rik Arends' Makepad visualizer**, demoed on X on 2026-09-12. It flies through whole repositories rendered as real, readable source text in 3D, with distance LOD, at 120 fps over 2.5 M lines, and it loaded Chromium (~51 M lines). It is unreleased (private beta, planned paid product). **codescape** (flop-labs, built on Makepad, credits Arends) is an open-source version of the same idea with a two-tier LOD that closely resembles glyph3d's wash/glyph split. **Wideboard** (2013/2023) is the earliest found whole-kernel literal-text renderer, but it is 2D and bitmap-font. Everything else either shows real text at small scale (Code Park, Code Canvas, Code Bubbles, Codemap, Code Thumbnails, minimaps) or replaces text with an abstraction (SeeSoft lines, cities, graphs).

### Cited Findings

**1. Rik Arends' Makepad 3D source-code visualizer (2026). CLOSEST, essentially the same concept.**
- Who and when: Rik Arends, creator of Makepad (a Rust app-development environment he founded in 2019). Posted on X 2026-09-12 (x.com/rikarends/status/2098710248164868534, with video). Press coverage followed 2026-09-16/17 — [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639); [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/)
- Literal text: yes. Code is shown "on large canvases that hang in 3D space", and the user flies closer to read individual fragments — [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639). It "can transition from a large-scale view of a repository down to the underlying source text" — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/)
- Scale and performance: 2.5 M-line codebase at over 120 fps (the X video is capped at 60). Chromium, ~51 M lines, loaded in roughly 60 s at 120 Hz on an M3 Max — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/); [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639). A second source says indexing time came down "to under 3 minutes" for Chromium, which does not match the ~60 s figure — [gotburnout/AI Radar](https://gotburnout.io/en/news/developer-loads-entire-chromium-codebase-into-3d-code-viewer-3417)
- Memory: about 21 GB RAM for the first 2.5 M-line version. Chromium used about 8 GB at first, then about 3.5 GB after level-of-detail data and search indexes moved to a disk-backed cache with streaming — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/); [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639)
- LOD: distant code is drawn in less detail, and LOD data is cached on disk. No glyph-rendering method is published — [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639); [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/)
- Status: experimental and unreleased. Arends plans a product for "a small fee", and one report says it is in private beta. Asked why he built it, he said "because I could", and he concedes its usefulness may be limited — [Techtroduce](https://www.techtroduce.com/rik-arends-3d-source-code-visualizer/); [dev.ua](https://dev.ua/en/news/3d-vizualizator-kodu-1789554639)
- Reception: Tom's Hardware headline "Developer builds viral 3D source code visualizer that consumes 21GB of RAM — flies around 2.5 million lines of code at over 120 frames per second". It reached HN 2026-09-17, where it got 2 points — [Tom's Hardware](https://www.tomshardware.com/tech-industry/developer-builds-viral-3d-source-code-visualizer-that-consumes-21gb-of-ram-flies-around-2-5-million-lines-of-code-at-over-120-frames-per-second); [HN Algolia 49737800](https://hn.algolia.com/api/v1/search?query=source%20code%20visualizer%203D&tags=story)
- How it differs from glyph3d, as far as is public: (a) load time is ~60 s for Chromium, against glyph3d's ~170 ms for a 97 MB / 1,306-file tree. Volumes differ, and Chromium's byte size is not given, so the per-byte rates cannot be compared from public data. (b) Its memory footprint (21 GB, then 3.5 GB) drove disk-backed LOD caches and streaming. (c) Files sit on canvases (panels) in 3D, rather than glyph3d's pages/columns with backward-in-depth wrap. (d) It is closed and unreleased, with no published glyph technique, picking, syntax colouring, emoji or transcript features.

**2. codescape (flop-labs, with a WIZARDspace fork), September 2026. Open source; its LOD design is the nearest match to glyph3d's wash/glyph split.**
- What: "Fly through any Git repository as an interactive 3D map of its source code, built with Rust and Makepad." The README says it is "inspired by Rik Arends' Makepad source visualizer" — [github.com/flop-labs/codescape](https://github.com/flop-labs/codescape); [WIZARDspace fork](https://github.com/WIZARDspace/codescape)
- Layout: directories are raised blocks stacked on their parents. Each file is a flat tile holding "its real text, split into columns so the tile stays roughly square" — [WIZARDspace/codescape](https://github.com/WIZARDspace/codescape)
- Rendering: far view is one quad per file sampling a mipmapped minimap atlas, each texel covering two characters of a line, coloured by syntax class. Near view is one quad per character from an SDF font atlas for files large on screen, capped at about 450k glyphs drawn nearest-first. Text is uploaded once, and the camera changes only uniforms and the near-glyph set — [WIZARDspace/codescape](https://github.com/WIZARDspace/codescape); [flop-labs/codescape](https://github.com/flop-labs/codescape)
- Scale: example repo about 4.6k files / 1 M lines, with about 0.6 s scan and layout. A synthetic 8x copy (36.8k files, 8.3 M lines) keeps about 200 MB of minimap texture. Self-reported 1.9k–2.6k fps on an NVIDIA GB10 and 115–128 fps (vsync) on an M3 Max — [flop-labs/codescape](https://github.com/flop-labs/codescape)
- Extras: git diff/PR overlays, Quint counterexample trace playback, headless PNG/video recording — [WIZARDspace/codescape](https://github.com/WIZARDspace/codescape)
- Status: 9 commits, 0 stars. Makepad is pinned to a 2026-09-22 dev commit. No license visible — [WIZARDspace/codescape](https://github.com/WIZARDspace/codescape); [flop-labs/codescape](https://github.com/flop-labs/codescape)
- Differs from glyph3d: (a) its far LOD is a per-file minimap texture (2 chars per texel), against glyph3d's per-line wash quad (geometry, not texture). (b) Near glyphs are SDF and capped at 450k, against glyph3d's analytic glyphs and 95 M laid-out instances. (c) It is roughly 1/10 to 1/100 glyph3d's flagship volume. (d) Files are flat tiles on a directory relief, with no depth-wrap. (e) No picking or emoji.

**3. Wideboard (Austin Appleby), proof of concept 2013, revived 2023. Whole Linux kernel as literal text, 2D.**
- Renders "_all_ of Linux's source code in a browser tab via WebGL". Show HN 2023-02-12 by aappleby, who wrote the original in 2013. Live demo at wideboard.stjo.dev — [HN 34758336](https://news.ycombinator.com/item?id=34758336)
- Technique: WebGL 1 with custom shaders. A 4096×4096 texture holds raw ASCII source. A second texture holds one texel per line (24-bit pointer into the text plus 8-bit length). A third holds a bitmap font atlas (Terminus). More textures are allocated as needed, up to GPU memory — [github.com/aappleby/Wideboard](https://github.com/aappleby/Wideboard)
- Scale: about 56,000 kernel files, about 31 M lines, about 1.2 GB of text, at 60 fps on a 3080 Ti with single-digit GPU utilisation. The 2013 version did about 800 MB at 60 fps on a 2012 MacBook Pro iGPU. Chrome showed about 9 GB GPU memory, which the author attributes to Chrome/ANGLE — [Wideboard repo](https://github.com/aappleby/Wideboard); [HN 34758336](https://news.ycombinator.com/item?id=34758336)
- Limits: no syntax highlighting, kerning or vector fonts. Map-style drag/zoom ("ala Google Maps"). No LOD described. 205 stars — [HN 34758336](https://news.ycombinator.com/item?id=34758336); [Wideboard repo](https://github.com/aappleby/Wideboard)
- Differs from glyph3d: 2D only (pan/zoom, no 3D camera or depth layout), a bitmap monospace font sampled by a fragment shader rather than per-glyph geometry, no syntax colour, no picking, ASCII only. It is still the strongest earlier evidence that "the whole kernel as literal text at 60 fps" was feasible a decade earlier, at about 10x glyph3d's flagship byte volume.

**4. Codemap / pfff_visual (Yoann Padioleau, Facebook, ~2010–2013). Already listed by the author.**
- A treemap of the whole repository in which each rectangle is a file sized by file size, with source-code thumbnails, semantic highlighting, search and navigation, and emacs/vim hooks. Padioleau calls it a map view, "not an editor", and says its navigation is "less smooth than Google Maps" — [Padioleau OCaml 2013 slides](https://ocaml.org/meetings/ocaml/2013/slides/padioleau.pdf); [pfff_visual listing](https://de.osdn.net/projects/freshmeat_pfff_visual/releases/)
- Differs from glyph3d: 2D treemap, software rendering (Cairo/GTK era), aspect-ratio-distorted rectangles rather than readable pages.

**5. Code Park (Khaloo, Maghoumi, Taranta, Bettner, LaViola; UCF; VISSOFT 2017). Already listed.**
- Each class is a 3D room, and its members are laid out on the walls as syntax-aware "wallpaper" of real code. Overview and first-person modes. Two user studies found it easier than an IDE for code understanding — [arXiv 1708.02174](https://arxiv.org/pdf/1708.02174); [thesis](https://www.eecs.ucf.edu/isuelab/publications/pubs/Pooya-Master-Thesis_Final.pdf)
- Differs: class-granular and aimed at novices and small codebases; no whole-repo glyph field and no reported scale numbers.

**6. Code Canvas (DeLine & Rowan, MSR, ICSE NIER 2010) and Debugger Canvas (Bragdon, Rowan, Jacobsen, DeLine, Reiss; ICSE 2012).**
- Code Canvas is "an infinite zoomable surface for software development" in Visual Studio 2010, with semantic zoom built on a WPF ZoomableCanvas. It presents a project's complete set of definitions as a map — [MSR Code Canvas](https://microsoft.com/en-us/research/project/code-canvas); [Rowan blog](https://learn.microsoft.com/de-at/archive/blogs/kaelr/code-canvas); [Debugger Canvas paper](https://www.microsoft.com/en-us/research/publication/debugger-canvas-industrial-experience-with-the-code-bubbles-paradigm/)
- Debugger Canvas is the industrial version of Code Bubbles: bubbles of code on a 2D pan-and-zoom surface, showing the working set rather than the whole project — [MSR](https://www.microsoft.com/en-us/research/publication/debugger-canvas-industrial-experience-with-the-code-bubbles-paradigm/); [Brown Code Bubbles](https://cs.brown.edu/~spr/codebubbles/about.html)
- Differs: 2D, WPF/GDI text, and they target editing at working-set scale; neither claims whole-repo-in-view scale.

**7. Code Thumbnails (DeLine, Czerwinski, Meyers, Venolia, Drucker, Robertson; MSR; VL/HCC 2006).**
- A scrollbar thumbnail of the file and a "desktop" of file thumbnails, for navigating by spatial memory; formative study with 11 developers — [MSR publication](https://www.microsoft.com/en-us/research/publication/code-thumbnails-using-spatial-memory-to-navigate-source-code/); [PDF](https://microsoft.com/en-us/research/uploads/prod/2006/09/Using-Spatial-Memory-to-Navigate-Source-Code.pdf). A 2023 paper credits this idea as the ancestor of the VS Code minimap — [arXiv 2304.09301](https://arxiv.org/pdf/2304.09301)
- Relevance: this is the direct academic ancestor of glyph3d's far-line wash, a shrunk silhouette of the real text used as a spatial landmark.

**8. Editor minimaps (Sublime Text, VS Code).**
- The VS Code minimap draws characters by default and coloured blocks with `editor.minimap.renderCharacters: false`. A literal-text LOD, confined to one file — [@IT tutorial](https://atmarkit.itmedia.co.jp/ait/spv/1811/09/news033.html). A mirror of VS Code source shows a prebaked, downsampled per-scale character renderer (minimapCharRendererFactory). The mirror was corrupted, so this is unverified — [mirror](https://v48tuyih0eytvyiwyd84kwv0.core.boriel.com/00c8e5c6-d0f2-4a8c-9be1-4f8e7424e4bc?page=8)

**9. SeeSoft (Eick, Steffen, Sumner; AT&T Bell Labs; IEEE TSE 18(11), Nov 1992).**
- Each line of code is a thin row, indentation can follow the source, and colour encodes statistics such as age, author or modification count. The example shows 15,255 LOC, with capacity "up to 50,000 LOC" (secondary source) — [GMU lecture slides](https://people.cs.gmu.edu/~tlatoza/teaching/swe795f19/Lecture%207%20-%20Software%20Visualization.pdf); [Maletic SoftVis tools list](https://www.cs.kent.edu/~jmaletic/softvis/SoftVisTools.html)
- Relevance: glyph3d's wash quad is SeeSoft's line row placed in 3D world space beside the real glyphs. SeeSoft never showed the glyphs themselves.

**10. Abstractions: the "fly through the code" items that do NOT render text.**
- kernel3d (2003): a 3D animation of Linux source development in which files are shapes and dependencies are coloured lines, plus MPEG "guided tour" flights through Linux 2.4.5 file structure. This is most likely the "2002 Linux-kernel-in-3D demo" on the author's page (unconfirmed) — [OSNews 3094](https://www.osnews.com/story/3094/visualizing-source-code-in-3d/); [Slashdot 2003](https://developers.slashdot.org/story/03/03/23/1342234/3d-visualization-of-linux-kernel-development); [interconnected.org](https://interconnected.org/home/2003/03/09/these_3d_animations)
- Ripary (2009): a visualisation of Linux 2.6.31 posted to LKML — [LKML](https://lkml.iu.edu/hypermail/linux/kernel/0910.0/01613.html)
- ExplorViz visualising the Linux kernel as a city (2025) — [Zenodo](https://zenodo.org/records/16910769)
- dep-tree, a 3D force-directed dependency graph (Show HN 2024, 180 points) — [github.com/gabotechs/dep-tree](https://github.com/gabotechs/dep-tree)
- code-universe, a repo as a 3D call-graph galaxy (TS Compiler API, Barnes-Hut, three.js) — [github](https://github.com/spilak0203-debug/code-universe)
- dart_code_3d, a 3D map of a Dart codebase — [github](https://github.com/hawkbee1/dart_code_3d)
- An instanced 3D-glyph LOD paper in three.js (ACM Web3D 2024): "glyphs" there are data glyphs for modules in a 2.5D landscape, not typographic glyphs — [ACM DL](https://dl.acm.org/doi/10.1145/3665318.3677171)
- CATE: an infinite-canvas desktop IDE with floating terminals, editors and browsers. Spatial windows, not a repo field — [peerpush](https://peerpush.com/p/cate)

**11. Readability evidence for code in 3D/VR.**
- Dominic et al. (ICPC 2020 ERA, extended 2022), 26 graduate students: reading code in VR raised physical demand, effort and task load, with no significant difference in measured or perceived productivity versus desktop — [ICPC 2020](https://conf.researchr.org/details/icpc-2020/icpc-2020-era/6/-Program-Comprehension-in-Virtual-Reality); [PMC9486787](https://pmc.ncbi.nlm.nih.gov/articles/PMC9486787/)
- Oberhauser & Lecon (2017): VR software visualisation was as efficient and effective as mouse and screen, but 30% of subjects reported VR sickness — [ACHI 2017](https://opus-htw-aalen.bsz-bw.de/files/643/achi_2017_8_40_20201.pdf)
- Kojić et al. (QoMEX 2020) on text distance, size and contrast for VR reading — [arXiv 2004.01545](https://arxiv.org/pdf/2004.01545)

### Inferences
- **The closest 3–5, with concrete differences:**
  1. **Arends / Makepad visualizer**: the same idea, same month, same language family (Rust). The public differentiators for glyph3d are cold-load speed (170 ms vs ~60 s, at different volumes), memory (no disk-backed LOD cache needed at 97 MB), the open artifact (glyph3d vs a closed beta), and features (picking, emoji, depth-wrap, agent transcripts). On the core claim "whole repo, real text, fly-through, LOD, 120 fps" there is no meaningful novelty gap. Treat this as a direct contemporary.
  2. **codescape**: open source, with a far/near LOD architecturally very like glyph3d's wash/glyph split (texture-per-file vs quad-per-line). It is about 1–2 orders of magnitude below glyph3d's flagship volume and uses SDF glyphs capped at 450k.
  3. **Wideboard**: whole-kernel literal text at 60 fps in 2013. 2D, bitmap font, no syntax colour. It sets the bar that "all text resident on the GPU, drawn by shader" was achievable long ago, so glyph3d's novelty is 3D layout, per-glyph geometry, analytic glyphs, emoji and picking, not resident whole-repo text as such.
  4. **Codemap (pfff)**: whole-repo treemap with real code thumbnails; 2D, older.
  5. **Code Park**: the closest *published academic* 3D precedent with real text, but class-scale.
- Wideboard's per-line pointer texture (line offset plus length) is structurally similar to glyph3d's visible mode, where the source bytes and line table stay resident and the GPU lays out lines in view per frame. This is worth noting as convergent design.
- The SeeSoft → Code Thumbnails → minimap → codescape-minimap-atlas lineage is the prior art for glyph3d's "wash" LOD. glyph3d's variant (one quad per line, in 3D, beside analytic glyphs) is a distinct implementation of an old idea.
- The Arends demo drew Makepad-ecosystem copycats within two weeks (codescape credits him). More are likely to appear through late 2026, and a search in a month may find more.

### Gaps
- No primary technical writeup from Arends: the X post (status 2098710248164868534) was not fetchable, and the Tom's Hardware body would not render. His glyph technique, file layout and LOD scheme are unknown, and the Chromium load-time figures conflict (~60 s vs "under 3 minutes").
- Chromium's byte volume in Arends' test is not reported, so a throughput comparison with glyph3d's 576.9 MB/s cannot be made from public data.
- Wideboard's LOD behaviour at full zoom-out (whether text is minified by texture sampling or by an explicit LOD) is not documented in the pages read.
- The commenter-mentioned spatial code editor "Aerial" (App Store) could not be found.
- "Thorium", "Melange", "Natto", "Muse" and tldraw-based code tools, from the brief: not verified in this pass. None turned up as whole-repo literal-text renderers in my searches. Haystack is listed by the author.
- Pad++ (Bederson & Hollan, 1994), Jazz/Piccolo and Xerox PARC's Document Lens (3D perspective view of document pages, 1993) are the canonical ZUI and 3D-text ancestors, but I did not fetch primary sources for them in this pass, so they are listed without citation.
- sv3D (Marcus, Feng, Maletic, 2003; SeeSoft extended to 3D) is believed to exist but was not confirmed by search.
- No hits on Reddit (r/GraphicsProgramming, r/rust, r/vrdev) or YouTube for whole-repo literal-text fly-throughs beyond the items above. Search coverage of those sites through the available tools is weak, so absence is not evidence of absence.
- No reliable source found for a "whole codebase printed as a poster in tiny type" project; Commits.io (2016) turns repos into framed art without an every-line claim — [TNW](https://thenextweb.com/news/commits-io)
