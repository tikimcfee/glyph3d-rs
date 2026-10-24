# Prior art: massive-scale zoomable / spatial / LOD text outside code tools

Compiled 2026-10-10. Scope: systems that show very large text corpora as one continuous navigable space with semantic zoom or level of detail, outside code tools. Each entry gives year, authors, scale, technique, and how it differs from glyph3d. glyph3d, for reference: about 10^8 analytic vector glyphs from about 100 MB of text, laid out on the GPU per frame from bytes resident on the GPU, a per-line "wash" quad at distance, flown through in 3D.

**What the author's prior-art page already covers** ([ivanlugo.dev/prior-art](https://ivanlugo.dev/prior-art/)). It is code-centric throughout:
- Vol 01, rendered code in space: Code Bubbles, Code Canvas, Code Park, CodeHouse, IDEvelopAR, Primrose, VRIDE, Haystack, Padioleau's Codemap.
- Vol 02, code cities: CodeCity, ExplorViz, IslandViz and others.
- Vol 03, VR IDEs and commercial code maps.
- Vol 04, web toys: Gource, code_swarm, anvaka's galaxies and others.
- Vol 05, the text engine layer: Slug, MSDF, troika-three-text, GPUI, Makepad, Bloc, a 2002 Linux-kernel-in-3D demo.

**What the page does not list, and these notes add:**
- genome browsers;
- ZUI and focus+context research (Pad++, space-scale diagrams, SDAZ, Information Mural, Document Lens, Perspective Wall);
- text-visualization art (TextArc, Valence, Preservation of Favoured Traces, Writing Without Words, Spineless Classics);
- DTP greeking;
- large-scale GPU label and scatter systems (deck.gl TextLayer, Deepscatter/Nomic Atlas, Embedding Atlas, Paperscape);
- trace viewers (Perfetto);
- Deep Zoom / Seadragon;
- VR text-reading studies;
- Library of Babel 3D projects.

Seesoft appears here as the root of the line-silhouette idea. It may be known to the author, but it is not on the page.

## Closeness ranking

The criteria are glyph3d's combination: (a) literal readable text, (b) about 10^8 glyphs in one space, (c) continuous zoom from overview to glyph, (d) interactive frame rate, (e) 3D.

1. **Genome browsers** (JBrowse 2, IGV/igv.js, UCSC, GenomeSpy, HiGlass): meet a, c and d. Their corpus is about 3×10^9 characters, which beats b on corpus size. They miss b on glyphs on screen, and they miss e. Close analysis below.
2. **Pad++** (1994): meets a, c, d (about 10 fps), and the scale was 600k objects. Has semantic zoom, labels that appear when legible, and a "do not draw tiny objects" LOD. 2D.
3. **Seadragon / Deep Zoom** (2007): meets a and c. Shows whole books zoomable as image pyramids. Pre-rasterized, so no layout. 2D.
4. **Seesoft** (1992) and **Information Mural** (1995): the line-silhouette and pixel-compression overview ancestors of the wash LOD. No continuous zoom to glyphs.
5. **Perfetto, deck.gl TextLayer, Deepscatter/Atlas, Embedding Atlas, Paperscape**: GPU or Canvas systems at 10^6 to 10^9 items with zoom-dependent labels. The text is labels, not a readable continuous corpus.
6. **Art**: TextArc, Valence (3D), Preservation of Favoured Traces, Posavec, Spineless Classics. Whole books at a glance. Mostly static or small scale.
7. **VR reading spaces / Immersive Space to Think**: document-scale 3D layouts with tens of documents. No LOD at corpus scale.

---

## 1. Genome browsers: "zoomable text at 3 billion characters" in another field?

### Takeaway
Genome browsers are the nearest existing analogue for zoom from the whole corpus down to readable characters over a 10^9-character single string. They load only the visible window from indexed files. But they are 1D strip views that show at most screen-width × a few rows of letters. They switch from letters to coloured cells to nothing, or to derived summary tracks; there is no silhouette of the text itself. Most still draw letters with Canvas2D `fillText`/`strokeText`. GenomeSpy is the exception: it draws sequence letters as MSDF text in WebGL. None is 3D, and none puts 10^8 letters on screen at once.

### Cited Findings

**igv.js** `sequenceTrack.js` has three hard LOD tiers keyed on bp per pixel ([source](https://raw.githubusercontent.com/igvteam/igv.js/master/js/sequenceTrack.js)):
- Above `bppSequenceThreshold = 10` bp/px: no sequence is fetched or drawn.
- Above 0.1 and up to 10 bp/px: each base is a `fillRect`, coloured by nucleotide, `1/bpPerPixel` px wide.
- At or below `BP_PER_PIXEL_THRESHOLD = 1/10` bp/px (a base at least 10 px wide): letters are drawn with Canvas2D `strokeText`, centred with `measureText`.
- Translated amino-acid frames follow the same letter threshold.

**JBrowse 2** (GMOD/jbrowse-components, main branch as of Oct 2026; latest release v4.3.0, 2026-05-21):
- `showsLetters(bpPerPx)` returns `1/bpPerPx >= 12`, so a base must be at least 12 px wide to get a letter and border ([sequenceGeometry.ts](https://github.com/GMOD/jbrowse-components/blob/main/plugins/sequence/src/LinearReferenceSequenceDisplay/components/sequenceGeometry.ts)).
- Above `ZOOMED_OUT_BP_PER_PX = 10` the track shows a message instead of a canvas ([model.ts](https://github.com/GMOD/jbrowse-components/blob/main/plugins/sequence/src/LinearReferenceSequenceDisplay/model.ts)).
- Letters are drawn per base with Canvas2D `ctx.fillText`. The font is `min(rowHeight-2, 14)` px sans-serif, and only the visible part of each block is drawn ([drawSequenceLetters.ts](https://github.com/GMOD/jbrowse-components/blob/main/plugins/sequence/src/LinearReferenceSequenceDisplay/components/drawSequenceLetters.ts)).
- Since 2026-10-01 the coloured cells beneath the letters have a GPU instanced-quad path: a Slang shader compiled to WGSL and GLSL, six vertices per instance, with high-precision bp-to-clip-space conversion. The commit message is "Sequence cells: hold the three backends to one picture" ([sequenceCell.slang](https://github.com/GMOD/jbrowse-components/blob/main/plugins/sequence/src/LinearReferenceSequenceDisplay/components/shaders/sequenceCell.slang); commit history via the GitHub API).
- The January 2026 v4.0.3 release replaced the SVG feature renderer with an HTML5 canvas renderer ([v4.0.3 release](https://www.jbrowse.org/jb2/blog/2026/01/10/v4.0.3-release/)).
- The older JBrowse sequence track showed colour-coded blocks without letters at first zoom. The switch to letters depends on window size and screen resolution ([RGD JBrowse help](https://rgd.mcw.edu/wg/help3/tools/rgd-genome-browsers/the-rat-jbrowse-genome-browser/more-about-jbrowse-tracks/reference-sequence-track)).

**UCSC Genome Browser** (server-rendered images):
- The Base Position track shows the sequence letters once zoomed to base level, and a "base" button zooms the window to fit the bases ([ruler help](https://genome.ucsc.edu/goldenPath/help/ruler.html)).
- For alignment base colouring, labels need zoom within 3× of base level. Further out, "the browser will choose one color to represent many bases" ([hgBaseLabel help](https://genome.ucsc.edu/goldenPath/help/hgBaseLabel.html)).

**GenomeSpy** (Kärkkäinen et al., GigaScience 2024):
- A grammar-based WebGL toolkit. Sequence is loaded lazily from indexed FASTA as the user navigates, split into nucleotides, and drawn as a `text` mark over `rect` backgrounds. All rendering is on the GPU "enabling a high frame rate and smoothly animated interactions" ([bioRxiv](https://www.biorxiv.org/content/10.1101/2023.10.06.561159.full.pdf); [GigaScience record](https://researchexchange.ocrahope.org/papers/gigascience-giae040)).
- Its text fragment shader is MSDF: `median(r,g,b)`, a supersampled distance averaged over an n×n grid, and an `uSdfNumerator` antialias slope ([text.fragment.glsl](https://github.com/genome-spy/genome-spy/blob/master/packages/core/src/rendering/webgl/marks/text.fragment.glsl); [text.vertex.glsl](https://github.com/genome-spy/genome-spy/blob/master/packages/core/src/rendering/webgl/marks/text.vertex.glsl)).
- Ranged text is either "squeezed" (proportions kept) or hidden when it does not fit its segment. `logoLetters` stretches glyph ink to fill a cell ([text mark docs](https://github.com/genome-spy/genome-spy/blob/master/docs/grammar/mark/text.md)).

**HiGlass** (Kerpedjiev et al., Genome Biology 2018): web-based multiscale navigation of 2D genome interaction maps alongside 1D tracks ([Harvard VCG](https://vcg.seas.harvard.edu/publications/20180824-higlass); [bioRxiv](https://www.biorxiv.org/content/10.1101/121889v3)).

### Inferences
**Where genome browsers match glyph3d:**
- Overview-to-character zoom over a corpus far larger than glyph3d's (about 3×10^9 vs 10^8 characters).
- Lazy, view-driven fetch and render of only what is in view, structurally like glyph3d's visible mode. The browsers fetch from server-side indexes (FASTA .fai, 2bit). glyph3d reads bytes resident on the GPU.
- Hard LOD thresholds keyed on screen size per character (IGV 10 px/base, JBrowse 12 px/base). This is the same idea as glyph3d's switch from glyph to wash.

**Where they differ from glyph3d (candidly):**
- (i) 1D: one string per track, one or a few rows. Letters on screen are bounded by viewport width, roughly 10^2 to 10^3.
- (ii) Monospaced fixed cells: no shaping, no lines, no wrap, no pages.
- (iii) Between "letters" and "nothing" they show coloured cells per base. Further out the sequence vanishes and other tracks (coverage, genes) carry the overview. There is no picture of the text's shape like SeeSoft's line silhouettes.
- (iv) Mostly Canvas2D text, except GenomeSpy's MSDF.
- (v) 2D only.

So "this already exists in another field" holds for the zoom-to-character navigation idea at billions of characters. It does not hold for 10^8 glyphs laid out and visible together in a 3D field.

### Gaps
- No source measures how many letters JBrowse or IGV draw per frame, or their frame rates.
- IGV desktop (Java) thresholds were not checked; only igv.js was.
- Whether GenomeSpy's text tier has a zoom-out LOD beyond squeeze/hide and viewport-edge fade was not established.
- HiGlass tiling and WebGL details are not in the abstract-level sources.

---

## 2. Greeking and text LOD: DTP, ZUIs, deep zoom, whole-book images

### Takeaway
Greeking (placeholder bars below a size threshold) is decades-old DTP practice. Pad++ (1994) already combined spatial indexing, do-not-draw-tiny-objects, labels-appear-when-legible and refine-when-still at 600k objects. Seadragon/Deep Zoom (2007) zoomed into whole books as image pyramids. None does per-frame layout of 10^8 live glyphs in 3D.

### Cited Findings

**InDesign "Greek Type Below"**:
- Type at or below the set size is drawn on screen as placeholder bars. It is set per display-performance view ([Adobe help](https://helpx.adobe.com/lv/indesign/desktop/get-started/settings-and-preferences/adjust-text-display-quality.html)).
- The default is 7 pt ([Noble Desktop](https://blog.nobledesktop.com/learn/indesign/type-formatting-indesign-video-tutorial)).
- Forum experts say the threshold takes zoom into account ([Adobe community](https://community.adobe.com/t5/InDesign/Why-so-greeking/m-p/8848249)).

**Pad++** (Bederson & Hollan, UIST '94, pp. 17–26):
- Implemented in C++ on SGI GL and X, with a Tcl/Tk front end.
- "Successfully loaded over 600,000 objects and maintained interactive rates", about 10 fps on a directory tree.
- Efficiency methods: spatial indexing (bounding-box hierarchy), restructuring, "Spatial Level-Of-Detail: Render only the detail needed", clipping, "Refinement: Render fast with low resolution while navigating and refine the image when still", and adaptive render scheduling.
- Semantic zooming through procedural objects that render differently by size.
- Filenames appear as labels "when the user is sufficiently close to be able to read them". Zooming into a file loads its text.
- "Very small objects are not drawn and larger ones are drawn with reduced resolution."
- Source for all of the above: [paper PDF](https://www.lri.fr/~mbl/Teach/SituatedComputing/2017/papers/ZUI/pad++-uis94-p17-bederson.pdf). JVLC 1996 version: [PDF](https://mrl.cs.nyu.edu/publications/jvlc-96-pad/jvlc-96-pad.pdf).

**Space-Scale Diagrams** (Furnas & Bederson, CHI '95): an analytic framework that stacks copies of a 2D world at each magnification into an inverted pyramid, so zoom and pan trajectories can be reasoned about ([CHI '95 page](https://www.chi1995.chistatic.hosting.acm.org/proceedings/papers/gwf_bdy.htm); [PDF](https://www.cs.umd.edu/projects/hcil/jazz/learn/papers/chi-95-spacescale.pdf)). It cites Furnas's Generalized Fisheye Views (CHI '86) (same PDF's references, via [Pad++ paper](https://www.lri.fr/~mbl/Teach/SituatedComputing/2017/papers/ZUI/pad++-uis94-p17-bederson.pdf)).

**Speed-Dependent Automatic Zooming** (Igarashi & Hinckley, UIST 2000, pp. 139–148):
- Scroll speed drives zoom-out so visual flow stays constant, with semantic zooming to show a document overview during fast scrolling.
- Prototypes included a web browser and a dictionary viewer ([PDF](https://www-ui.is.s.u-tokyo.ac.jp/~takeo/papers/uist2000.pdf); [project page](https://www-ui.is.s.u-tokyo.ac.jp/~takeo/research/autozoom/autozoom.htm)).
- Microsoft patent US6747680, now expired ([Google Patents](https://patents.google.com/patent/US6747680)).

**Seadragon / Deep Zoom** (Microsoft Live Labs, TED 2007, Blaise Agüera y Arcas):
- Loads only the region in view and only at the resolution displayed ([Wikipedia: Deep Zoom](https://en.wikipedia.org/wiki/Deep_Zoom)).
- The TED demo showed a 300-megapixel image and zooming into a complete book ([historyofinformation.com](https://historyofinformation.com/detail.php?id=1896)), plus a zoomable Guardian edition ([exacteditions blog](https://blog.exacteditions.com/scaleable-print-and-collective-seeing/)).
- The book's identity (often recalled as Bleak House) could not be confirmed.

**Spineless Classics** (UK, Carl Pappenheim; posters since around 2012):
- The complete text of a novel on one sheet at about 4 pt, arranged into an image. B1/B2/A0/A4 sizes. War and Peace spans four sheets ([Modern Retail](https://modernretail.co.uk/spineless-classics/); [Londonist](https://londonist.com/2014/02/spineless-classics-famous-london-stories-on-posters); [The National](https://thenationalnews.com/lifestyle/home/wish-list-spineless-classics-poster-1.420949)).
- Over 100 titles ([Shortlist](https://shortlist.com/news/single-page-book-posters)).

### Inferences
- glyph3d's wash quad is greeking taken to its extreme: one quad per line rather than bars per word or line, in 3D, with the cutover driven by screen size.
- Pad++'s list (spatial index, skip tiny objects, refine when still) is the classic ZUI recipe. glyph3d replaces "refine when still" with full analytic quality every frame, which is a meaningful difference.
- Seadragon-style pyramids give overview-to-letter zoom for any text, at the cost of pre-rasterizing. Text cannot be restyled, re-laid-out, picked or selected per glyph, and gigapixel scale holds only on the order of 10^6 to 10^7 legible characters.

### Gaps
- No confirmed example of a whole large corpus (Wikipedia, a legal code) published as a single deep-zoom pyramid with every word legible.
- "The Whole Bible on one page", "Every Word" and "Words in Space" could not be pinned to specific sourced projects.
- "Text LOD" in game engines (for example TextMeshPro SDF) was not researched here; it is partly covered by the author's Volume 05 (MSDF).

---

## 3. Text-visualization research: overviews of whole texts

### Takeaway
The research line runs as follows:
- Seesoft (1992): each line is a pixel row.
- Information Mural (1995): anti-aliased compression of a whole space into one window.
- Document Lens and Perspective Wall (early 90s, PARC): 3D perspective of pages.
- TextArc (2002): a whole novel in one ellipse.

These establish the overview half of glyph3d's idea. None goes from that overview continuously to readable glyphs at 10^8 scale.

### Cited Findings

**Seesoft** (Eick, Steffen & Sumner, IEEE TSE 18(11):957–968, Nov 1992):
- Each line of text becomes a thin row coloured by a statistic, and the row mirrors the line's indentation and length.
- Up to about 50,000 lines on a high-resolution monitor ([GMU lecture](https://people.cs.gmu.edu/~tlatoza/teaching/swe795f19/Lecture%207%20-%20Software%20Visualization.pdf); [Bern SCG lecture](https://scg.unibe.ch/download/lectures/sma/SMA-06-SoftwareVisualization.pdf)).

**Information Mural** (Jerding & Stasko, InfoVis '95 pp. 43–50; TVCG 4(3) 1998):
- A reduced 2D picture of an entire information space fitting one window, built with grayscale, intensity, colour, pixel size and anti-aliased compression.
- Prototypes covered software, scientific data, text documents and geographic data ([Georgia Tech page](https://sites.cc.gatech.edu/gvu/ii/softvis/infoviz/information_mural.html); [GT repository](https://repository.gatech.edu/entities/publication/38ee4578-3426-4143-9ce6-7d7d72848e42); [UIST '95 demo](https://uist.acm.org/uist1995/abstracts/Jerding.html)).

**Perspective Wall** (Mackinlay, Robertson & Card, 1991) and **Document Lens** (Robertson & Mackinlay):
- The Perspective Wall presents detail on a seemingly 3D wall ([HumanIT review](https://humanit.hb.se/article/download/242/567)).
- A later patent says perspective "greatly reduces the legibility of a page" in Document-Lens-style views ([US5943679](https://patents.google.com/patent/US5943679)).
- Primary-paper details were not retrieved.

**TextArc** (W. Bradford Paley, April 2002):
- A whole book on one screen: every line in tiny type around a large ellipse, plus each word placed by spring forces toward its occurrences, sized by frequency.
- Paley calls it "just a visual index" ([CIRCA wiki](https://circa.cs.ualberta.ca/index.php/CIRCA:TextArc); [SIGGRAPH Digital Art Archive](https://digitalartarchive.siggraph.org/?p=16520); [Japan Media Arts Festival](https://www.bunka.go.jp/j-mediaarts-festival/en/award/single/textarc-print-alices-adventure-in-wonderland/index-2.html)).

### Inferences
- glyph3d's far-field wash is Seesoft's line silhouette (row length and indent) carried into a 3D field. Unlike Seesoft, it degrades continuously into the actual glyphs.
- TextArc's tiny-type ring is the closest art-world precedent for a whole corpus as readable but microscopic text. Its scale is one book of about 10^5 words.
- WordTree, Document Cards and Compus were not researched in this pass. They are text-analytic summaries, not whole-corpus spatial renderings, so they are farther from glyph3d.

### Gaps
- The Document Lens and Perspective Wall primary papers were not fetched, so year and scale details rest on secondary sources.
- No sourced Seesoft-style log visualizer working at 10^8 lines was found.

---

## 4. GPU data-viz systems rendering text or labels at million scale

### Takeaway
Web GPU systems routinely handle 10^6 to 10^9 points with zoom-dependent label LOD (quadtree tiles, density, auto-labels). Their text is sparse labels: a few hundred to a few thousand visible at once, chosen by importance. It is not a continuous readable corpus. deck.gl's TextLayer is an instanced-quad SDF atlas, the same primitive family as glyph3d's instanced mode, aimed at labels.

### Cited Findings

**deck.gl TextLayer**:
- A CompositeLayer wrapping IconLayer. It auto-builds a font atlas from `characterSet` and font settings, with SDF available through `fontSettings: {sdf: true}` ([deck.gl docs](https://deck.gl/docs/api-reference/layers/text-layer); [PointLabelLayer](https://deck.gl/docs/api-reference/carto/point-label-layer)).
- Lonboard's binding defaults to ASCII 32–128, or generates the set from the data ([Lonboard](https://developmentseed.org/lonboard/latest/api/layers/text-layer/)).
- No million-label benchmark was found.

**Deepscatter / Nomic Atlas** (Ben Schmidt et al.):
- WebGL through REGL. "Zoomable, animated scatterplots in the browser that scales over a billion points".
- Arrow data in a quadtree tile format loaded only as needed on zoom ([deepscatter README](https://cdn.jsdelivr.net/npm/deepscatter@2.15.2/README.md); [Nomic blog](https://home.nomic.ai/blog/posts/why-are-web-browsers-the-best-data-browsers)).
- An Atlas map of 5.5M tweets is a public example (same Nomic blog).

**Apple Embedding Atlas** (open-sourced around May 2025; arXiv 2505.06386):
- WebGPU with a WebGL2 fallback, millions of points, automatic clustering and labeling, KDE density contours, order-independent transparency ([InfoQ](https://www.infoq.com/news/2025/11/embedding-atlas); [paper summary](https://www.emergentmind.com/papers/2505.06386)).
- The practical ceiling is reported inconsistently ("millions" vs "a few million").

**Paperscape** (Damien George & Rob Knegjens, 2013):
- A zoomable tiled map of arXiv: about 1M papers in 2013, about 2.5M later.
- Zooming in reveals more labels and frequent-word labels per region ([Physics World](https://physicsworld.com/a/welcome-to-the-arxiv-galaxy/); [IAIFI](https://research.iaifi.org/paperscape); [ICA MapCarte](https://mapdesign.icaci.org/tag/multiscale/)).

**Perfetto UI** (Google trace viewer):
- Built for multi-GB traces.
- Small slices are quantized to 1 px blocks rather than skipped, and "when zoomed enough all slices will be displayed" ([commit](https://flutter.googlesource.com/third_party/perfetto/+/7c8d0847dca8a2be90da1be4c05c1889fd1d96d2)).
- Per-slice styling is skipped unless zoomed in, because of `fillRect` overhead ([commit](https://flutter.googlesource.com/third_party/perfetto/+/bee15bb3172197dc48e65aa67f41037bc140c6d1%5E)).
- Canvas2D.

### Inferences
- The label-map family solves "which text to show" by importance sampling. glyph3d solves "show all text" with geometric LOD.
- Perfetto's 1 px quantization of sub-pixel slices is the timeline equivalent of glyph3d's wash quad: never drop, collapse to a minimal mark.

### Gaps
- No public benchmark of deck.gl TextLayer or Atlas at 10^7 or more glyphs.
- Whether Atlas labels use SDF text was not established.
- Datashader-for-text, Kepler.gl text and regl text projects were not researched.

---

## 5. Art and installations

### Takeaway
Artists have rendered whole books as single images or 3D structures since the late 1990s. Fry's Valence (1999) is the earliest found to place a book's words in 3D space dynamically. These are book-scale (10^5 words), often with text abstracted. None is a navigable 10^8-glyph field.

### Cited Findings

**Valence** (Ben Fry, 1999; MIT master's thesis "Organic Information Design"):
- Reads Twain's *The Innocents Abroad* linearly, placing each unique word as a node in 3D space with branches between adjacent words. Frequent words drift outward.
- Later versions: two German books at Ars Electronica Center 2001; Genome Valence at the 2002 Whitney Biennial ([benfry.com/valence](https://benfry.com/valence/); [Ars Electronica 2001](https://webarchive.ars.electronica.art/festival2001/texte/valence_e.html)).

**On the Origin of Species: The Preservation of Favoured Traces** (Ben Fry, Sept 2009, Processing; text from John van Wyhe's Darwin Online):
- All six editions shown: the first edition, about 150k words, in grey columns by chapter, with later editions' changes colour-coded. Rolling over blocks reveals the text.
- Also a 6×9 in. book and a poster held by Cooper Hewitt ([FlowingData](https://flowingdata.com/2009/09/07/ben-fry-visualizes-the-evolution-of-darwins-ideas); [Cooper Hewitt](https://www-6.collection.cooperhewitt.org/objects/1108751253); [OSU DH guide](https://guides.osu.edu/DH/text)).

**Writing Without Words** (Stefanie Posavec, MA project, around 2006–2009): "sentence drawings" of Kerouac's *On the Road*. Each sentence is a line segment whose length is its word count, turning at each sentence end and coloured by theme. A 48×36 in. poster covers the whole novel ([stefanieposavec.com](https://www.stefanieposavec.com/archive/writing-without-words); [Mas Context](https://www.mascontext.com/issues/15-visibility-fall-12/writing-without-words/); [The Marginalian](https://www.themarginalian.org/2009/05/07/writing-without-words/)).

**Library of Babel in 3D**:
- Keiwan Donyagard's iOS/Unity app: hexagonal rooms ([App Store](https://apps.apple.com/app/id1180198611)).
- Ancient Pixel's browser "Library of Babel 3D", Phaser, a work in progress where only random generation is implemented ([itch.io](https://ancientpixel.itch.io/library-of-babel-3d)).
- Jamie Zawinski's 2016 SketchUp render ([Hyperallergic](https://hyperallergic.com/an-attempt-to-3d-model-jorge-luis-borgess-library-of-babel)).
- Bridges 2015 paper on gallery geometry ([Bridges](https://archive.bridgesmathart.org/2015/bridges2015-359.html)).

### Inferences
- Fry's Favoured Traces is the closest art precedent for "corpus as greyed blocks, roll over to read". That is greeking with hover reveal, at about 1M words across six editions.
- Library of Babel 3D projects are the thematic precedent (walkable rooms of text). They generate text on demand rather than displaying a real corpus at scale.

### Gaps
- No sourced "Matrix rain at huge counts" or demoscene 3D typography fly-through at 10^6+ glyphs was found.
- Brendan Dawes's text works were not researched.
- "Deconstructulator" could not be verified.

---

## 6. VR/AR reading spaces

### Takeaway
VR text research is about document-scale sensemaking (tens of documents arranged in space) and reading comprehension in headsets. No large-corpus LOD text system was found in this literature.

### Cited Findings
- **Immersive Space to Think** (North et al., IEEE VR 2021): 17 novice analysts arranged historical documents in VR. Three layout strategies were observed ([NSF PAR](https://par.nsf.gov/biblio/10312400-sensemaking-strategies-immersive-space-think); [VT](https://sanghani.cs.vt.edu/research/publications/2021/sensemaking-strategies-with-immersive-space-to-think.html)).
- **Rau, Zheng & Guo (2021)**: 64 participants retained 82% (VR) and 88% (AR) of information relative to an LCD baseline ([Emerald, ILS](https://emerald.com/insight/content/doi/10.1108/ILS-11-2020-0236/full/html)).
- **Blum, Cetin & Stuerzlinger**: wall display vs VR headset sensemaking. No significant quantitative difference, more strategies on the wall ([ZCU](https://dspace.zcu.cz/bitstream/11025/35636/1/Blum.pdf)).
- **Curved virtual walls** (arXiv 2501.10338, Jan 2025): smaller errors on curved than flat walls for visualization reading ([arXiv](https://export.arxiv.org/pdf/2501.10338)).

### Inferences
- Legibility cost in headsets is a measured issue: about 82–88% retention vs LCD. This bears on any VR port of glyph3d.

### Gaps
- No Vision Pro or Quest app rendering corpus-scale (10^7+ glyph) text was found. Spatial code apps fall under the author's Volume 01/03.

---

## Cross-cutting summary for the report writer

- **Nothing found combines all five criteria.** The nearest per axis:
  - Scale of corpus with zoom to character: genome browsers (10^9 characters, 1D, about 10^3 letters visible).
  - ZUI architecture: Pad++ (600k objects, 1994).
  - Overview silhouette: Seesoft and Information Mural.
  - 3D text: Valence (one book), Document Lens / Perspective Wall (pages).
  - GPU label scale: Deepscatter, Embedding Atlas, deck.gl (labels, not corpora).
- **The genome-browser analogy is real but partial.** The fair framing: genome browsers already zoom over billions of characters to readable letters by fetching only the window. glyph3d's distinguishing combination is 2D text lines in a 3D field, 10^8 glyphs simultaneously present (wash or glyph), and per-frame GPU layout from resident bytes. Genome browsers draw a few hundred letters per row with Canvas2D (GenomeSpy: WebGL MSDF), and above about 10 bp/px they drop the sequence entirely rather than draw a silhouette of it.
