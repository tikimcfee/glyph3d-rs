# GPU / massively-parallel TEXT LAYOUT — prior art vs glyph3d (as of 2026-10-10)

Scope note. The system being compared is glyph3d's **Visible field mode**: the
raw source bytes, Pass 1's line table (16 B/line `LineEntry`, plus 24 B
`SegmentSeed`s at cuts every 2 KiB of a long line, cut only before an ASCII
byte) and the glyph-lookup trie stay resident on the GPU. Each frame a compute
pass runs one serial fold per visible line (UTF-8 decode, emoji-sequence trie
chain, wrap, paginate) and writes a transient `DerivedSlot` buffer that the
Derived vertex shader draws. Lines too small to read become one quad each.
Sources for the glyph3d side are the repo's own `out/history/VISIBLE-MODE.md` (M1/M2)
and `out/GPU-DIRECTION-2026-10-09.md` §2 (a design argument that the line,
not the byte, is the parallel unit, and that "Nobody ships this: Slug takes
CPU-placed quads; Vello/Parley shape and wrap on the CPU; GPUI lays out on
the CPU and sprites the glyphs").

**What the author's prior-art page already lists.** https://ivanlugo.dev/prior-art/
has an engine-layer volume "05 · Rendering Text at Scale": Slug (public
domain 2026), MSDF, troika-three-text (stalls at ~500 instances), Zed's GPUI,
Makepad, Glamorous Toolkit's Bloc, unnamed instanced-glyph papers, and a 2002
Linux-kernel-in-3D demo. Volumes 01–04 and 06 cover code visualization
(Code Park, CodeCity and others). **None of the layout, parsing or monoid
items below appear on that page.** It is a rendering and visualization
taxonomy, and it has no entries for layout algorithms. —
[ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)

**Method and coverage.** About 35 web searches and fetches, covering
Levien's blog, xi-editor docs, the Zed blog, arXiv, PVLDB, ACM/OOPSLA
abstracts, Google Patents/USPTO, GitHub and Hacker News. Absence of a
finding is a statement about these queries, not proof that something does
not exist. The repo's own rule applies: "no grep match is not does not
exist".

---

## Ranked summary: closeness to "GPU-resident bytes → per-frame visible-line layout → glyph instances"

| rank | item | bytes resident on GPU? | layout on GPU? | visible-only, per frame? | emits glyph instances? | verdict |
|---|---|---|---|---|---|---|
| 1 | **Zutty** (terminal, GLES 3.1 compute) | cell grid (char codes) in an SSBO | no: the CPU VT parser fills a fixed grid; the GPU derives position from the cell index | yes, the grid *is* the screen | no; it writes pixels directly | closest *mechanism* (resident chars + compute per update), but it has no layout problem (fixed-width, no wrap, no shaping) |
| 2 | **GPU-driven text** (J. Joestar, 2021) | strings written by shaders | fixed-width advance in the vertex shader | per frame | compute builds the per-char dispatch list + indirect draw | closest on "compute emits instances + indirect draw"; trivial layout |
| 3 | **Google US8988434B1** (2012, active) | compact 6 B/glyph "glyph sequence texture" | no: "text wrapping, kerning, and layout" on the CPU | n/a | vertex pulling of compact records | prior art for **Derived mode's** vertex pulling, not for Visible |
| 4 | Ghostty / refterm / (Alacritty, kitty: unverified) | cell grid | no | screen grid | instanced cells | grid terminals; layout is the CPU's job |
| 5 | AMD GPUOpen mesh-shader text (2024) | string copied to a GPU buffer | no: positions supplied by the CPU | per change | one mesh workgroup per char | shows "one workgroup per char from a resident string" |
| 6 | GPU lexers/parsers: ParPaRaw, cuJSON, GpJSON, cuDF, Pareas, Co-dfns, HybridSA, bitstream regex | yes, whole input | parsing/lexing only, not layout | no: whole input, batch | no | the algorithmic cousins of glyph3d's fold/scan |
| 7 | Monoid/scan formulations: Hillis & Steele, Mytkowicz, Levien's stack monoid, Panchekha's parallel line breaking, Kmett/finger trees | n/a | theory or CPU | no | no | the theory behind glyph3d's monoid scan form |
| 8 | Virtualized CPU editors: xi (rope science 6), Zed SumTree/DisplayMap, klogg, UwView | no | CPU | **yes**, visible first | CPU-built sprites | the "only what's visible" half, entirely on the CPU |
| 9 | GPU glyph renderers: HarfBuzz 14 `hb-gpu`, Slug, Vello, glyphon | outlines/atlas only | no: shaping/layout on the CPU | n/a | CPU-placed glyphs | rasterization only (already on the prior-art page) |

**Bottom line, stated candidly:** I found **no** published project, paper or
patent that keeps a document's raw bytes resident on the GPU and, per frame,
lays out only the visible lines in a compute shader (variable-width decode,
cluster resolution, wrap, pagination) into a transient glyph-instance buffer.
The pieces exist separately:
- GPU-resident character grids drawn by compute (Zutty).
- Compute-generated per-glyph indirect draws (Joestar).
- Compact vertex-pulled glyph records (Google patent).
- Whole-input GPU lexing and parsing of raw bytes (ParPaRaw, cuJSON, Pareas).
- Visible-first layout on the CPU (xi, Zed, klogg).

Nobody found combines them. The one case where the GPU does the decode-and-place
step from character codes per frame is fixed-width terminal grids, which have
no wrapping or advance problem.

---

## Q1. Raph Levien / Linebender: stack monoid, GPU parsing, piet-gpu/Vello, parley, GPU text layout

### Takeaway
Levien's GPU work gives glyph3d its *parallel primitives*: the stack monoid,
decoupled-lookback prefix sums, and GPGPU JSON/unescaping sketches. His xi
work gives it the *visible-first, per-hard-break* wrapping strategy, but on
the CPU. Neither he nor Linebender has published GPU text *layout*: Parley
shapes and breaks on the CPU, and Vello/HarfBuzz-gpu rasterize CPU-placed
glyphs.

### Cited Findings
- "Towards GPGPU JSON parsing" (2018-05-10) sketches parsing the tree structure of JSON (a Dyck language) with only scan, scatter/gather and sort, avoiding a stack: "We obviate the need for a stack." Estimated cost "3 scans, 4 scatters, and a sort". Not implemented ("I haven't implemented it yet"), no numbers, no mention of text layout. — [raphlinus.github.io](https://raphlinus.github.io/personal/2018/05/10/toward-gpu-json-parsing.html)
- "A sketch of string unescaping on GPGPU" (2018-04-25) is a companion sketch. — [blog index](https://raphlinus.github.io/)
- "The stack monoid" (2020-09-05) parallelizes bracket matching (the parent of each open bracket, the match of each close bracket). The monoid element is `(n_pops, elements)`; push = `(0,[e])`, pop = `(1,[])`. It represents "a function from the state at the beginning of a sequence of tokens to the state at the end", so a scan yields the stack state at every position. It was proposed as a "fundamental building block to represent the parse stack". The GPU design uses a fixed top-of-stack window, a bump allocator on overflow and a decoupled look-back scan. The post had no implementation and gave no numbers. It cites ParPaRaw, a cuDF JSON Lines PR and Aaron Hsu's thesis as related work. — [stack monoid](https://raphlinus.github.io/gpu/2020/09/05/stack-monoid.html)
- "The stack monoid revisited" (2021-05-13): prototype in piet-gpu (PR #90) for the clip stack, measured at "2.4 billion elements per second" on an AMD 5700 XT, "about 10x faster than the Rust scalar code". It names a forward-progress hazard ("can get stuck") and a memory-coherence finding ("volatile alone is indeed not sufficient"). Parsing is still only proposed ("could serve as the basis for a parser for ... JSON"). Text layout is not mentioned. — [revisited](https://raphlinus.github.io/gpu/2021/05/13/stack-monoid-revisited.html)
- He also wrote "Prefix sum on Vulkan" (2020-04-30) and "Prefix sum on portable compute shaders" (2021-11-17), on decoupled-lookback scans in portable GPU APIs. These relate directly to glyph3d's retired CubeCL three-level scan. — [blog index](https://raphlinus.github.io/)
- "Text layout is a loose hierarchy of segmentation" (2020-10-26) covers paragraph → style runs → BiDi → itemization → script runs → shaping clusters, plus UAX #14, greedy and Knuth-Plass breaking. It does **not** discuss parallelism, GPU, monoids or visible-only layout. — [text-layout post](https://raphlinus.github.io/text/2020/10/26/text-layout.html)
- Background: Dec 2020–Oct 2025 at Google Fonts, "research on GPU font rendering". — [levien.com](https://levien.com/)
- xi-editor "Rope science 1": rope nodes cache summaries combined up the tree (example metric: max line width). A reader comment pointed out "this was a monoid, starting my fondness for monoid homomorphisms." Edits recompute O(log n) nodes. — [rope_science_01](https://xi-editor.io/docs/rope_science_01.html)
- xi-editor "Rope science 6: parallel and asynchronous word wrapping":
  - Splits at hard breaks (newlines). Each line wraps independently (Rayon), and results "can just be concatenated together".
  - Proposes rewrapping "what's on the screen first, for immediate update", then out to the nearest hard break, then the rest.
  - Measured ~150 MB/s single-threaded wrap; goal of a 300 MB file in under 1 s.
  - Notes that a huge file with no newlines defeats per-line parallelism.
  - The async part was "not planned for now". — [rope_science_06](https://xi-editor.io/docs/rope_science_06.html)
- Parley is described by Linebender as handling text layout "mostly at the level of line breaking and resolving glyph positions" (CPU). Vello uses GPU compute shaders for 2D rendering, and its glyph caching landed by Sept 2025. — [Linebender Mar 2025](https://linebender.org/blog/tmil-15/), [Linebender Sep 2025](https://linebender.org/blog/tmil-21/), [Linebender Jul 2025](https://linebender.org/blog/tmil-19/)

### Inferences
- glyph3d's GPU-direction §2 conclusion is that the parallel unit is the line, not the byte, because UTF-8, the sequence chain, syntax state and bit-exact f32 x reset at `\n`. That is the same move as xi's rope science 6 (independent per hard break; visible lines first), carried onto the GPU. glyph3d's 2 KiB `SegmentSeed`s are a direct answer to the pathology xi named but did not solve (one giant line).
- The stack monoid is the right tool for nesting (brackets, block comments). glyph3d deliberately avoids needing it, because its syntax heuristic carries no cross-line state.
- No Linebender artifact moves layout onto the GPU. The repo's claim "Vello/Parley shape and wrap on the CPU" holds.

### Gaps
- I did not find Levien's 2025 "I want a good parallel computer" content. Only its title was seen ([index](https://raphlinus.github.io/)), so I can't say whether it discusses text layout.
- I did not search the Linebender Zulip for informal "GPU text layout" threads.

---

## Q2. Academic: parallel prefix-sum parsing and lexing, parallel line breaking, monoid editors

### Takeaway
The FSM-as-function-composition-under-scan idea runs from Hillis & Steele
(1986) through Mytkowicz et al. (2014) to ParPaRaw (GPU, 14.2 GB/s, 2020) and
GPU JSON parsers (cuJSON, ASPLOS '26; GpJSON, PVLDB 2025). Whole GPU
compiler front ends exist (Pareas, Co-dfns). Parallel greedy line breaking
has been designed (Panchekha 2019) but, as far as I found, never shipped on
a GPU. All of these process the *whole input*. None does visible-only work.

### Cited Findings
- **Hillis & Steele, "Data Parallel Algorithms" (CACM 1986).** Applied parallel prefix to DFA membership on the 65,536-processor Connection Machine: O(log n) steps but n processors. A 2026 paper notes that their lexing example treats each character as a function on states and "needs care" (one-character lookahead only). — [CACM PDF](https://ece757.ece.wisc.edu/uw-only/06_cacm86_data_parallel.pdf), [speculative DFA paper summarizing it](https://arxiv.org/pdf/1210.5093), [Nidhögg 2026](https://arxiv.org/pdf/2608.03473)
- **Mytkowicz, Musuvathi, Schulte, "Data-Parallel Finite-State Machines" (ASPLOS 2014).** Enumerates transitions from all states per input symbol to break the dependences. Applications include tokenizing (HTML) and Huffman decoding. Up to 3× on one core (SIMD) and ~21× on 8 cores. — [MSR](https://www.microsoft.com/en-us/research/publication/data-parallel-finite-state-machines/), [PDF](https://microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos302-mytkowicz.pdf)
- **Yang (1996), "Mealy machines are a better model of lexical analyzers".** Output on transitions encodes lookahead. — [NYCU](https://ir.lib.nycu.edu.tw/handle/11536/1379)
- **ParPaRaw (Stehle & Jacobsen, TUM; PVLDB 13, 2020; arXiv 1905.13415).**
  - Massively parallel parsing of delimiter-separated values by parallel FSM simulation.
  - Explicitly "does not require an initial sequential pass over the input to determine a thread's parsing context" (e.g., inside a quoted field or not).
  - Up to 14.2 GB/s on a 3,584-core GPU; 4.8 GB end to end in ~0.44 s including PCIe, using streaming overlap. — [arXiv](https://arxiv.org/pdf/1905.13415), [TUM](https://portal.fis.tum.de/en/publications/parparaw-massively-parallel-parsing-of-delimiter-separated-raw-da/)
- **cuJSON (Gargary, Loaliyan, Zhao; ASPLOS '26).** Offloads "(i) UTF validation, (ii) JSON tokenization, and (iii) nesting structure recognition" to the GPU. Claims to beat simdjson/Pison (CPU) and cuDF/GPJSON (GPU). The repo README has no numbers. — [GitHub](https://github.com/AutomataLab/cuJSON)
- **GpJSON (PVLDB vol. 18, 2025).** Structural indexes built on the GPU, queried in situ. ≥2.9× over parallel JSON parsers and query engines, 6–8× over RAPIDS on an A100. — [PVLDB PDF](https://www.vldb.org/pvldb/vol18/p3216-bonetta.pdf)
- **RAPIDS cuDF** GPU JSON reader (nested → struct/list columns). — [NVIDIA blog](https://developer.nvidia.com/blog/gpu-accelerated-json-data-processing-with-rapids/), [cuDF docs](https://docs.rapids.ai/api/cudf/nightly/cudf/io/read-json/)
- **Pareas (Leiden, 2021).** A compiler whose major stages all run on the GPU. Voetter's thesis does lexing, parsing and semantic analysis on Futhark data-parallel primitives. It "scales well for large source input, but suffers from the overhead of scheduling and distribution of work for smaller input." Huijben's thesis is the GPU code-generation backend. — [Voetter thesis](https://theses.liacs.nl/pdf/2020-2021-VoetterRobin.pdf), [Huijben thesis](https://theses.liacs.nl/pdf/2020-2021-HuijbenMarcel.pdf)
- **Co-dfns (Aaron Hsu).** APL-subset compiler designed to self-host on the GPU, with trees represented as vectors. Dissertation: "A Data Parallel Compiler Hosted on the GPU". — [APL Wiki](https://aplwiki.com/wiki/Co-dfns), [IU ScholarWorks](https://scholarworks.iu.edu/dspace/items/3ab772c9-92c9-4f59-bd95-40aff99e8c7a), [Dyalog '23](https://www.dyalog.com/uploads/conference/dyalog23/materials/D15_CodfnsUpdate2023.pdf)
- **Nidhögg, "Certified Split Points for Parallel Lexing" (arXiv 2608.03473, Aug–Sep 2026).** Splits input at certified delimiter bytes so chunks lex without recovering entry states (exact, or modulo discarded tokens). It groups prior work as simulation, speculation, prescanning and overlap. CPU only: 92.6–95.3% parallel efficiency at 8 threads, 3.46–3.94× at 4 threads. — [arXiv](https://arxiv.org/abs/2608.03473)
- **Panchekha, parallel greedy line breaking (blog, 2019-12-01).** Four phases: prefix sum of word widths, exponential search for each word's next-line start, pointer doubling into a skip list, one thread per output line. O(log N) time, O(N log N) space. "I have no idea how practical this algorithm is." GPU named as the natural target but not implemented. — [pavpanchekha.com](https://pavpanchekha.com/blog/line-breaking.html)
- **Meyerovich & Bodík, "Fast and parallel webpage layout" (WWW 2010).** Layout solving as attribute grammars (O(log) and reflow-free), parallel selector matching and font rendering, up to 80× speedups. Later thesis "Parallel Layout Engines" (2013) synthesizes parallel tree traversals. The GPU is mentioned only as a general target in the snippets I saw. — [WWW'10 PDF](https://archives.iw3c2.org/www2010/_lmeyerov/projects/pbrowser/pubfiles/playout.pdf), [EECS-2013-242](https://www2.eecs.berkeley.edu/Pubs/TechRpts/2013/EECS-2013-242.html)
- **Hinze & Paterson finger trees (JFP 2006).** A sequence annotated by a `Measured v a` whose `v` is a `Monoid`; `split` takes a predicate on the measure. This is the canonical "measure monoid" text-buffer structure. — [Hackage fingertree](https://hackage-origin.haskell.org/package/fingertree/docs/Data-FingerTree.html)
- **Yi** (Haskell editor) has an incremental-parser module and parser-based keybindings. — [Hackage yi](https://hackage.haskell.org/package/yi-0.6.6.0/docs/yi.txt), [Yi structure notes](https://archives.haskell.org/code.haskell.org/yi/doc/blog/01-Structure.text)

### Inferences
- ParPaRaw's "no sequential pre-pass" approach (simulate every possible start state) is the alternative glyph3d did *not* take. glyph3d keeps a CPU Pass 1 that produces seeds (line table + `SegmentSeed`s), so each GPU thread starts from a known state. For glyph3d the fold state is not a small FSM state: it includes a running f32 x that must be bit-exact to the oracle's summation order, which state enumeration can't provide. That is a principled reason for the pre-pass.
- glyph3d's cut rule ("a cut lands only before an ASCII byte; the atlas asserts no sequence has an ASCII member after its first") does for emoji-sequence resolution what Nidhögg's 2026 "certified split points" do for DFA lexing: choose split bytes at which the entry state is known. Nidhögg is the closest *academic* analog to glyph3d's chunk/segment-cut design, though it is CPU-only and targets lexing, not layout.
- glyph3d's monoid scan form (row, col, x as a prefix sum over bytes) sits in the Hillis–Steele / Mytkowicz lineage. glyph3d's own measurements (GPU-direction §2) reach the same practical conclusion as Pareas: GPU kernels run at memory speed, while overhead (upload, compile, launch, small inputs) decides the outcome.
- Panchekha's algorithm targets *optimal-independent greedy* breaking of one paragraph. glyph3d's wrap is a fixed-column fold-unit rule (`rows_for_line`), so it needs none of that machinery. A per-line serial fold is enough.

### Gaps
- I couldn't confirm the details of Hillis & Steele's own lexing example (the paper is behind a UW-only link; known only secondhand).
- I found no Kmett "monoidal parsing" primary source (the search returned only Yi). Flag it as unverified.
- No GPU Knuth-Plass implementation was found.

---

## Q3. GPU font shaping, GPU regex/grep, GPU UTF-8 decode/validation

### Takeaway
No one ships GPU *shaping*. HarfBuzz's 2026 GPU library is a Slug-based
*rasterizer* fed by CPU shaping. GPU regex is mature (HybridSA, bitstream
engines). GPU UTF-8 validation exists inside GPU parsers (cuJSON's first
phase), while standalone fast UTF-8 work (simdutf) is CPU SIMD.

### Cited Findings
- **HarfBuzz 14.0 (2026-04-01)** added `libharfbuzz-gpu`: "GPU-based text rasterization via the Slug algorithm", with GLSL/WGSL/MSL/HLSL shaders and a WebGPU/WebGL demo. 14.2.0 added a COLRv0/v1 paint encoder (`hb_gpu_paint_t`). The component "encodes glyph outlines for GPU rasterization". Shaping remains on the CPU. — [Phoronix](https://www.phoronix.com/news/HarfBuzz-14.0-Released), [hb-gpu.h](https://flutter.googlesource.com/third_party/harfbuzz/+/HEAD/src/hb-gpu.h), [14.2.0 changelog](https://patchwork.ipfire.org/project/ipfire/patch/20260426114418.122889-14-adolf.belka@ipfire.org/)
- **Slug** patent dedicated to the public domain on 2026-03-17 (Lengyel). — [search summary citing alphapixeldev / HN](https://alphapixeldev.com/sdf-vs-msdf-vs-slug-vs-rive-gpu-text-rendering/) (secondary). The prior-art page also lists it.
- **HybridSA (OOPSLA 2024).** CPU–GPU multi-pattern regex via bit-parallel shift-and. 4–60× over the best CPU engine and 4–233× over the best GPU engine. — [SPLASH 2024](https://2024.splashcon.org/details/splash-2024-oopsla/114/HybridSA-GPU-Acceleration-of-Multi-Pattern-Regex-Matching-using-Bit-Parallelism), [NSF PAR](https://par.nsf.gov/biblio/10618058-hybridsa-gpu-acceleration-multi-pattern-regex-matching-using-bit-parallelism)
- **Interleaved Bitstream Execution for multi-pattern regex on GPUs (Ge, Chu, Liu, Oct 2025).** 19.5× geomean over the best GPU regex engine. — [Stevens](https://researchwith.stevens.edu/en/publications/interleaved-bitstream-execution-for-multi-pattern-regex-matching-/)
- **Cameron et al., "Bitwise Data Parallelism in Regular Expression Matching" (PACT 2014)** is the foundational parallel-bitstream grep paper, with GPU results. — [PDF](https://www2.cs.sfu.ca/~ashriram/papers/2014_PACT_GREP.pdf)
- **CPU SIMD Unicode (simdutf lineage):** Lemire et al., "Unicode at Gigabytes per Second" (UTF-8 validation/transcoding); AVX-512 transcoding (2022). — [arXiv 2111.08692](https://arxiv.org/pdf/2111.08692), [arXiv 2212.05098](https://arxiv.org/pdf/2212.05098)
- **GPU tokenization (LLM BPE):** BlockBPE (ICML 2025 ES-FoMo workshop) runs one CUDA block per string, drops regex pre-tokenization, and reaches up to 2× tiktoken. GPUTOK (arXiv 2603.02597, 2026) keeps token-level equality with a CPU reference. — [BlockBPE](https://arxiv.org/abs/2507.11941v1), [GPUTOK](https://arxiv.org/html/2603.02597v1)
- Dawn (WebGPU) has a GPU-style UTF-8 decoder overlong check (code-level). — [dawn](https://dawn.googlesource.com/dawn/+/refs/heads/chromium/6945)

### Inferences
- glyph3d's GPU kernel decodes UTF-8 and resolves emoji ZWJ/flag/keycap sequences through a resident 380 KB trie on the GPU, per visible line. I found no other GPU code doing grapheme/emoji-cluster resolution. GPU tokenizers (BlockBPE/GPUTOK) are the nearest analog: byte-sequence → token-id resolution against a GPU-resident table, one block per string.
- GPUTOK's "token-level equality with a CPU reference" mirrors glyph3d's hyper-oracle discipline (the GPU kernel held slot-for-slot to HyperLayout).

### Gaps
- No HarfBuzz-on-GPU *shaping* attempt was found.
- No standalone GPU UTF-8 validation paper was found (it exists only as a phase inside cuJSON).

---

## Q4. Huge-file viewers/editors that virtualize layout; any with bytes on the GPU?

### Takeaway
Every large-file viewer and editor found virtualizes on the **CPU**: a
sparse line index plus rendering only the visible lines (klogg, UwView), or
monoid-summary trees mapping offsets to rows (Zed SumTree, xi rope). None
keeps the bytes on the GPU or lays out there per frame.

### Cited Findings
- **Zed:** the rope is built on a `SumTree` (copy-on-write B+ tree whose nodes carry summaries). `TextSummary` holds len, len_utf16, `lines` (Point), first/last line chars, `longest_row`. Summaries add up the tree, and `offset_to_point` seeks by one dimension while accumulating another. The `DisplayMap` stacks inlay, fold, tab, wrap and block maps, all SumTrees. The article doesn't say where layout runs, but GPUI is CPU layout with sprited glyphs (see repo note). — [Zed blog](https://zed.dev/blog/zed-decoded-rope-sumtree), [Simon Willison summary](https://simonwillison.net/2024/Apr/28/zed-decoded-rope-sumtree), [early wrap_map commit](https://git.secluded.site/zed/commit/b513df3844cc4ab9f24d5e89642136366c2673b1)
- **xi:** visible-first rewrap and per-hard-break parallel wrap (see Q1). — [rope_science_06](https://xi-editor.io/docs/rope_science_06.html)
- **klogg:** reads files from disk without loading them into memory, handles files over 10 GB and more than 2^31 lines, uses multi-threading and SIMD (secondary sources). — [telecomhall](https://www.telecomhall.net/t/klogg-a-fast-advanced-log-explorer-based-on-glogg-project/11824), [korben.info](https://korben.info/en/klogg-log-viewer-tool.html)
- **UwView:** memory-maps the file, renders only visible lines, resolves line positions via a small sparse index. — [alternativeto](https://alternativeto.net/software/uwview/about/)
- **Makepad:** SDF text in GPU atlases, HarfBuzz shaping (community docs). Its creator reports "1-3 millisecond per update/edit/fold cpu time". Layout is on the CPU. — [futureofcoding Q&A](https://linen.futureofcoding.org/t/42752/hi-people-i-made-makepad-i-can-answer-questions), [makepad-font skill page](https://skills.cat/skills/zhanghandong/makepad-skills/makepad-font) (non-official)
- **glyphon (wgpu):** shaping, layout and rasterization via cosmic-text on the CPU, atlas via etagere. gpui-wgpu also shapes with cosmic-text and draws instanced quads. — [glyphon](https://github.com/grovesNL/glyphon), [gpui-wgpu deepwiki](https://deepwiki.com/mdeand/gpui-wgpu/2.5-text-rendering)
- A 2021 W3C public-gpu thread (Apple's Myles Maxfield): no architectural reason a WebGPU text editor must beat Canvas2D, and custom text is unlikely to match native across scripts. — [W3C](https://lists.w3.org/Archives/Public/public-gpu/2021Jul/0006.html)

### Inferences
- glyph3d's line table (`LineEntry`: byte_start, item, base_row, glyph_count) is functionally a flattened, GPU-resident version of the offset↔row summary that Zed's SumTree and xi's rope compute on the CPU. The difference is that glyph3d's is a flat array built by a parallel Pass 1 (42.6 MB for 2.66 M lines in 72 ms on 32 threads, per VISIBLE-MODE.md M1), and the GPU reads it directly. CPU editors walk theirs in a tree.
- The "lines too small to read are one quad each" LOD has no analog in these editors, which never zoom out to a whole repo. The nearest analog is minimap rendering (unsearched; see Gaps).

### Gaps
- Not checked with sources: VS Code/Monaco large-file mode and minimap, Sublime's large-file handling, glogg internals, Hex Fiend. None is believed to be GPU-resident, but that is unverified.
- Use.GPU (Steven Wittens) has `@use-gpu/layout` Text/Glyphs components. I couldn't determine whether its text layout runs on the GPU. — [usegpu.live](https://usegpu.live/docs/reference-components-@use-gpu-layout)

---

## Q5. Patents

### Takeaway
I found no patent claiming GPU-side layout (advances, wrapping) from a
character buffer, or per-frame visible-line layout in a shader. The closest,
Google's US8988434B1, explicitly keeps layout on the CPU and claims vertex
pulling from a compact per-glyph texture. That is prior art for glyph3d's
*Derived* mode, not its Visible mode.

### Cited Findings
- **US8988434B1, "Text rendering for browsers and mobile based applications".**
  - Google; inventor Ian Roth; priority/filed 2012-04-12; granted 2015-03-24; status active, adjusted expiry 2032-11-28.
  - The CPU does "font management, text wrapping, kerning, and layout" and builds a per-sequence **glyph sequence texture** (16-bit glyph index + 4-byte screen location, about 6 B/glyph versus about 96 B/glyph for a conventional VBO).
  - A static **glyph position texture** holds atlas coordinates and size.
  - A generic index VBO drives a vertex shader that looks up both textures and computes the 6 vertices per glyph.
  - The GPU does not derive advances or line breaks.
  - Claim 1 begins "generating, by the central processor, passive graphics processor text resources prior to real-time text rendering". — [Google Patents](https://patents.google.com/patent/US8988434B1/en)
- **US8842127B1, "Text rendering with improved glyph cache management".** The CPU lays out lines using advances, then sends textured rectangles to the GPU. — [Google Patents](https://patents.google.com/patent/US8842127B1/en)
- **US20120053928A1, "dynamically applying line breaks in text":** no GPU component in the snippet. **US10019415 (cross-platform text layout)** names Knuth-Plass; its output is the glyph sequence plus coordinates (CPU). **US6647533 ("Incremental algorithms for optimal linebreaking")** and **US6510441 ("Optimal line break determination")** are CPU line breaking. — [US20120053928](https://patents.google.com/patent/US20120053928), [US10019415 PDF](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/10019415), [US6647533 PDF](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/6647533), [US6510441 PDF](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/6510441)
- Others surfaced but not relevant to layout: US8139075 (packing glyph textures in a GPU buffer), US9129441 (lookup tables for text rendering), US10186237 (glyph-mask render buffer), US12242807 (tokenizing alphanumeric text with FSMs). — [US8139075](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/8139075), [US9129441](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9129441), [US10186237](https://patents.google.com/patent/US10186237), [US12242807](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/12242807)

### Inferences
- The patent landscape found is CPU-layout, GPU-draw. US8988434's compact-record vertex pulling (6 B/glyph) is an older relative of glyph3d's 20 B `DerivedSlot` with vertex-stage Y/Z derivation. glyph3d additionally derives Y/Z from row and page on the GPU, which the patent leaves to the CPU.
- No patent found covers "compute shader lays out visible lines from resident bytes". That is not a freedom-to-operate opinion: the search was keyword-level, and claims were not read in full.

### Gaps
- No assignee-targeted searches (Apple, Microsoft DirectWrite, Adobe, NVIDIA, JetBrains) and no CPC-class browsing (G06T 11/60, G06F 40/103). A proper patent search would need those.

---

## Q6. Rendering-adjacent: GPU terminals with cell layout in shaders; Shadertoy-style text-from-buffer

### Takeaway
Zutty is confirmed: it renders the terminal grid with an OpenGL ES 3.1
compute shader, one invocation per cell, from a GPU-resident cell array. It
is the nearest shipped "characters resident on GPU, compute places glyphs per
update" system. But the CPU has already done all the layout (the VT parser
fills a fixed grid), and there is no variable width, wrapping or shaping on
the GPU. Grid terminals (Ghostty, refterm and others) position cells in the
vertex shader from grid coordinates.

### Cited Findings
- **Zutty:**
  - "One independent rendering process per cell", launched by "calling DispatchCompute with the dimensions of our terminal grid".
  - `cells[]` lives in an SSBO; a 256×256 texture maps char codes to atlas positions; bold and italic are atlas layers.
  - Drawing is "a trivial pixel-for-pixel copy operation".
  - The host's job "ends with maintaining the right content in this array of cells".
  - One glyph per equal-sized cell is called a first approximation; no wrapping or shaping on the GPU.
  - Project origin: "a concept to prove the feasibility of using a GLES Compute Shader to render a fixed-width text grid entirely in graphics hardware". Version 0.16. — [How Zutty works](https://tomscii.sig7.se/2020/11/How-Zutty-works), [zutty-crt fork README](https://github.com/jengelh/zutty-crt), [OpenBSD port](https://openports.pl/path/x11/zutty)
- **refterm (Casey Muratori):** "refterm.hlsl – shader for doing tile-based rendering", with a CPU glyph cache "mapping Unicode runs to glyphs". Supports line wrapping, reflow on resize and large scrollback. The README doesn't say where wrapping runs. Presented as a minimum speed bar, not a maximum. — [GitHub](https://github.com/cmuratori/refterm)
- **Ghostty:** the Metal/GLSL cell shader takes per-cell grid position (x < columns, y < rows), cell width (wide chars), colours, and the glyph's atlas position/size/bearing. The vertex shader builds the quad, and one pass draws the whole grid. — [cell.metal mirror](https://git.uoc.run.place/Applied-Software/ghostty/src/commit/ced8776120d2dbd7ebeab6d538a9958e44fb44ff/src/renderer/shaders/cell.metal), [cell.v.glsl mirror](https://git.uoc.run.place/Applied-Software/ghostty/raw/commit/19692f297e650580c7e749fd82d5d5c71da40460/shaders/cell.v.glsl)
- **GPU-driven text (Gabriel/"JorenJoestar", 2021-10-26):**
  - Strings are written into a GPU buffer by shaders.
  - A compute shader builds the per-character dispatch list and sets indirect-draw args (`instance_count = global_index`).
  - The vertex shader places each char at `entry.x + char_index * sprite_size.x` (fixed `CHAR_SPACING = (8,12)`). No newline handling.
  - The post's dispatch loop appears to read `entries[0]` for every entry, which is a possible bug. — [blog](https://jorenjoestar.github.io/post/gpu_driven_text/gpu_driven_text/)
- **AMD GPUOpen, "Font- and vector-art rendering with mesh shaders" (Meyer, Kuth, Oberberger, 2024-03-13).** "Every time the string changes, we copy `textToRender` to a GPU-buffer", then "dispatch as many thread-groups, as we have characters". Character positions are supplied by the CPU (no advances or line breaks on the GPU). — [GPUOpen](https://gpuopen.com/learn/mesh_shaders/mesh_shaders-font_and_vector_art_rendering_with_mesh_shaders/)
- **SitePoint, "WebGPU compute shaders for high-throughput syntax highlighting".** Per search snippets: per-character tokenization in WebGPU compute over a UTF-32 buffer, timings for a 1 MB file, no line layout or visible-line selection. The page returned 403 to fetch, so I couldn't verify it or rule out low-quality or AI-generated content. — [sitepoint](https://www.sitepoint.com/webgpu-compute-shaders-for-high-throughput-syntax-highlighting/)
- Osor.io "Rendering Crispy Text On The GPU" keeps glyphs resident in an atlas (rendering only). — [osor.io](https://osor.io/text)
- Will Dobbie, GPU text rendering with vector textures (per-pixel Bézier evaluation; rendering only). — [wdobbie.com](https://wdobbie.com/post/gpu-text-rendering-with-vector-textures/)

### Inferences
- The terminal family proves "chars resident on GPU + shader places them" at screen scale, but only because a fixed grid turns layout into `position = cell_index × cell_size`. That is the "clean-run trick" GPU-direction §2a already identifies for pure-ASCII lines. glyph3d's contribution beyond this family is everything a grid avoids:
  - variable-width UTF-8 and 0/1/2-cell advances;
  - greedy emoji-sequence chains;
  - fold-unit wrapping and pagination;
  - bit-exact f32 running x;
  - seeded mid-line segments;
  - zoom-dependent LOD (line → wash quad);
  - all over a 93 MB, 2.66 M-line corpus rather than an 80×24 to 300×100 screen.
- Joestar plus GPUOpen together show "compute produces the glyph list and the indirect draw args", which is glyph3d's M2 shape (cull → prefix over blocks → compact → per-line layout → indirect draw). Neither does real layout.

### Gaps
- kitty and Alacritty shader internals were not verified (search returned only secondary sources and par-term). — [par-term-render](https://docs.rs/crate/par-term-render/0.9.1/source/README.md)
- I didn't find a Shadertoy example that lays out text from a byte buffer with wrapping. Shadertoy text is typically fixed-width font-texture printing (not sourced here).
- Windows Terminal's AtlasEngine and Warp were not examined.

---

## Cross-cutting inferences for the report writer

1. **Has it been done?** As far as this search reaches, no. The combination
   in glyph3d's Visible mode is not documented anywhere I found:
   - raw bytes resident on the GPU;
   - a CPU-built, oracle-checked line table with mid-line seeds;
   - a per-frame compute fold over visible lines only (variable width, emoji-cluster trie, wrap, paginate);
   - a transient instance buffer drawn by a vertex-pulling shader;
   - a wash-quad LOD.

   The closest shipped system is Zutty (2020), and its "layout" is a fixed grid filled by the CPU.
2. **Why nobody does it** (consistent with the repo's own analysis and the
   sources):
   - Real text layout (BiDi, shaping, font fallback) is CPU-library territory: Parley, HarfBuzz, cosmic-text. HarfBuzz's 2026 GPU push went to rasterization, not shaping.
   - Editors never need more than one screen of layout per frame, so CPU virtualization (xi, Zed, klogg) is sufficient.
   - glyph3d's case is unusual: a monospace-cell code renderer, an atlas with 0/1/2-cell advances, no BiDi, and a whole-repo zoom. Under those conditions the per-line fold is simple enough for a GPU thread and the visible set can be huge (zoomed-out repo).
3. **Algorithmic lineage glyph3d belongs to:**
   - fold/scan as monoid: Hillis & Steele → Mytkowicz → Levien's stack monoid and prefix sums → ParPaRaw/cuJSON;
   - visible-first, per-hard-break parallel wrap: xi rope science 6;
   - offset↔row summaries: finger trees, xi rope, Zed SumTree;
   - certified split points for chunked state-free continuation: Nidhögg 2026, analogous to glyph3d's ASCII-only cut rule;
   - compact-record vertex pulling: Google US8988434, analogous to Derived mode;
   - compute-built indirect glyph draws: Joestar 2021, GPUOpen 2024.
4. **What to add to the prior-art page** (currently absent): Zutty, Levien's stack monoid, the prefix-sum posts and GPGPU JSON sketch, xi rope science 1/6, Zed SumTree/DisplayMap, ParPaRaw, cuJSON/GpJSON, Pareas, Co-dfns, Panchekha's parallel line breaking, Nidhögg 2026, US8988434B1, Joestar's GPU-driven text, GPUOpen mesh-shader text, and HarfBuzz 14 `hb-gpu` (rasterization only).
