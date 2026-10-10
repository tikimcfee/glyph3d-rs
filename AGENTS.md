# AGENTS.md — glyph3d-native (root)

Orientation for anyone, human or agent, working in this tree. The renderer's
output is the contract: **every refactor must be provably output-neutral**,
proven by running the checks, not by argument.

**This file is canonical for anything repo-wide** — what the checks do, what is
fenced, what the vocabulary means. `native/AGENTS.md` is canonical for the Rust
crate (style, module contracts, debug env vars), and the module headers in
`native/src` for pipeline internals. When they disagree about a repo-wide fact,
this file wins and the other is stale — say so in your commit rather than
patching a correction on top of the stale text, which is how this file rotted
the last time. (It rotted again: until 2026-10-09 the check section below
described sixteen gates when `build.toml` declared nine. It was rewritten
against `build.toml` that day; `cargo glyph gates` is the live list.)

## Layout

- `native/` — the pure-Rust renderer and layout engine binary (`glyph3d-native`).
  Features sub-200ms repo loading (`HyperLayout`) and modularized Slug WGSL rendering.
- `glyph/` — the build/verify tool: gate runner, mutation prover. (The launcher
  TUI moved into the renderer on 2026-10-10, C27: `native/src/launcher/`.)
- `crates/` — the glyph field, split by render mode (2026-10): `glyph-field` is the
  mode-neutral contract (`GlyphField` trait, `GlyphFieldMode`, the shared records and
  binding map); `glyph-field-instanced` is the Instanced mode (32 B `RenderSlot`, its
  upload path, pipelines and `glyph_field.wgsl`); `glyph-field-derived` is the Derived mode
  (20 B `DerivedSlot`, GPU vertex-stage Y/Z derivation, and `glyph_field_derived.wgsl`);
  `glyph-field-visible` (2026-10-10; `out/VISIBLE-MODE.md`) is the
  Visible mode — no slot per glyph: the source bytes, Pass 1's line table and the atlas
  trie are resident, the lines in view are laid out per frame on the GPU into a
  transient `DerivedSlot` buffer the Derived shader draws, and lines too small for
  glyphs are washes (one quad each). The renderer's side of it is
  `native/src/layout_hyper/visible.rs` (the arena's third form) and the `Visible`
  arms of `glyph_scene/{setup,render}.rs`. Chosen at load with
  `--field-mode instanced|derived|visible`, switchable live from the Debug panel.
  M3 (2026-10-10) re-keyed selection, the glyph verbs and styling there by
  (item, byte) in place of the slot (`out/VISIBLE-MODE.md` § M3); the witness
  is `native/tests/visible_verbs.rs`, a pixel comparison of the same verb in
  Derived and Visible mode.
  `crates/glyph-session-dirs` (2026-10-09, std only) is the one table of where agent
  apps keep session transcripts, shared by the renderer's F7 browser and the
  renderer's launcher; `launch_config.example.toml` documents the overrides.
- `engine/` — `fixtures/`, the conformance corpus recorded from the JS oracle
  (26 `.pipe.bin` + 8 `.bake.bin`, their generators and vendored inputs), and
  `glyph_schema.mjs`, generated from the schema.
- `tools/` — check scripts, generators, `bench_hyper.py` performance harness, and repro helpers.
- `.agents/` — agent house rules (`rules/rust-engineering.md`) and operational testing skill
  (`skills/glyph-engine-testing/SKILL.md`).
- `assets/atlas/` — prebaked glyph-geometry binaries (+ `FORMAT.md`).
- `schema/glyph-identity.json` — layout source of truth (vendored, hash-pinned).
- `out/` — historical reports, proof PNGs, `tooling-ab/baseline/` (the pixel oracle).
- `integration/` — vendored egui 0.36.1 source. A **grep reference only**: the egui
  that actually compiles comes from crates.io via `Cargo.toml`. Patching this copy
  changes nothing.
- `research/` — background surveys and GPU architecture studies.
- `experiments/` — its own workspace, outside every gate: the Zed-integration
  spikes and `gpu-direction/` (the GPU-layout research behind visible mode;
  some of its scripts still name the retired CubeCL engine).
- `config/defaults.toml` — every named, tunable value (colours, spacings, LOD
  thresholds), compiled in; `launch_config.toml` `[section]`s override it.
- `.claude/worktrees/` — untracked.

## Build

```sh
cargo build --release -p glyph3d-native   # the renderer
cargo test --workspace                    # every test binary + WGSL validation
cargo run -p glyph -- validate            # build.toml against its schema
```

The layout engine is `HyperLayout` (`native/src/layout_hyper.rs`): a parallel,
cache-blocked CPU layout written with Rayon, writing into unified-memory mapped
buffers where the GPU allows it and staging otherwise.

**Performance characteristics:**
- Flagship corpus: the retired JS repo, glyph3d-js (1,306 files, 97.0 MB source,
  95.2 million glyph instances) loaded and laid out in **~168 ms** backend (576.9 MB/s) /
  **~177 ms** total visual init in `--color-mode flat`, and **~215 ms** backend (450.9 MB/s) /
  **~226 ms** total visual init in `--color-mode syntax` on Apple Silicon Metal.
  These figures are from late 2026-09, before C22's windowed staging and
  before visible mode (which runs no Pass 2 at all); they have not been
  re-measured since. `tools/bench_hyper.py` reproduces them; re-measure
  before quoting.
  Benchmark tools read its location from `GLYPH_FLAGSHIP_REPO`; nothing in the
  tree hardcodes where a checkout lives.
- CubeCL, the GPU compute-layout engine (`--repo-engine cubecl`), was RETIRED on
  2026-10-09. Its kernels ran at memory speed, but its upload, shader compile and
  launch overhead never beat HyperLayout, and HyperLayout's host staging recovers
  its one discrete-GPU advantage. The measurements and the case are in
  `out/GPU-DIRECTION-2026-10-09.md`; the GPU's next role (laying out only the
  visible lines from resident bytes) shipped as the visible field mode.
- ByteSpan token painting: `ByteSpan` and `Paint::ByteSpans` provide byte-range semantic
  token coloring directly from AST/LSP analyses into mapped unified memory.
- Modularized renderer: `glyph_scene.rs` is factored into `setup.rs`, `pipelines.rs`
  (composite) and `render.rs`; the glyph field itself — slot storage, upload, glyph
  pipeline, WGSL — lives behind the `GlyphField` trait in `crates/` (one crate per
  render mode, chosen with `--field-mode`; the scene never touches slot bytes).

**The dependency graph is declared in `build.toml`** (artifact, input globs,
build command, class) and executed by `glyph` (`glyph/src/`: `manifest.rs` the
types, `gates.rs`, `golden.rs`, `prove.rs`, `products.rs`, `main.rs` the CLI). The baseline PNGs are class **golden**: verified,
never built — the runner refuses. Red on any compiler warning, test failure, gate mismatch,
or mutation survival.

## The tool

```sh
cargo glyph build          bring the renderer up to date
cargo glyph test           run everything; nonzero if anything is wrong
cargo glyph test rust      only what you changed: rust | render | corpus
cargo glyph test --frozen  assert currency instead of building it
cargo glyph run --demo     build if stale, then launch the renderer; arguments
                           pass through, and none opens its terminal launcher
```

`run` executes in YOUR directory, so a file argument means what it says relative
to where you typed it. The checks `cd` to `native/` because they pass
native-relative fixture paths deliberately — that is their business, not yours.
The `cargo glyph` alias is repo-scoped (it lives in `.cargo/config.toml`), so
from outside the workspace call the binary directly:
`<repo>/target/release/glyph3d-native --load-repo .`

**The alias works in nested worktrees too** (`.claude/worktrees/<name>`,
since 2026-10-09). Cargo merges `.cargo/config.toml` from every ancestor
directory and concatenates alias ARRAYS, so there the tool receives
`run --quiet --release -p glyph --` before its verb; it strips every leading
copy (`strip_doubled_alias`, pinned to the config by a test). Keep the alias
an array: a string is not merged, but a checkout on one form and a worktree on
the other make cargo refuse to load its config at all (measured).

**Use it rather than the pieces.** Do not hand-run `cargo build`, the `tools/`
scripts, or the binary's own check flags in place of a gate: the ordering
between them is exactly what the tool exists to hold for you, and getting it
wrong is how a stale binary once made a check test the previous build for a
day. `tools/check-all.sh` still works; it is a thin door onto `cargo glyph test`.

- **Scope is an argument, not a verb.** The scopes answer "I changed X, what
  should I run": `rust` (native/, crates/, glyph/), `render` (layout, shaders,
  anything that moves a pixel), `corpus` (fixtures, generators, vendored
  inputs). Every verdict line says how many gates ran ("5 of 15 gates ran,
  scope rust"), and a selection of ZERO gates is refused as `CHECK-ALL:
  NOTHING RAN`, never green: a scope once outlived its last gate and printed
  ALL GATES GREEN over nothing until it was removed (2026-10-09).
- **`test` builds; `--frozen` refuses to.** Building is the iterating intent.
  `--frozen` is the validating intent: if something is stale, that IS the
  finding, and a check that silently rebuilds could never report it.
- **`cargo glyph prove`** applies each mutation declared in `build.toml`,
  requires the named check to redden for the named reason, and restores
  byte-exact. It reports COVERAGE — which checks have no mutation and are
  therefore unproven — not a pass count. `cargo glyph prove` prints the live
  figure, and a new check should arrive with the mutation that proves it.
  Scoped forms for iteration (2026-09-29):
  `--mutation <name>` (repeatable) proves exactly the named mutations, and
  `--changed` proves only mutations whose target file differs from HEAD —
  "prove what you touched" (a mutation you MOVED keeps its name and is
  selected by its file). A scoped run's verdict names its scope ("a scoped
  run proves its scope, not the manifest"); the unscoped run remains the
  landing bar. A mutation whose gate is ALREADY red cannot prove anything and
  the prover says so — on a host whose pixel set is stale, the pixel-ab
  mutations are unprovable until the set is re-adopted.
  A mutation's gate can BUILD a product from the mutated source (`cargo test
  --release` links the renderer): after restoring, the prover drops the stamp
  of every product the file feeds and rebuilds it, and a mutation of the
  runner declares `rebuild`, because every later gate is spawned from
  `target/release/glyph` (2026-10-09: before this, a prove left the mutant
  behind a stamp reading current).
- **`cargo glyph gates`** prints what each check compares and cannot see;
  **`graph`** the artifact graph; **`validate`** the manifest against its schema.

**Run it in a worktree if anyone else is working in this repo.** It reads the
WORKING TREE, not HEAD, so another thread's uncommitted edits fail your checks
and tell you nothing about your own change. This has happened. It is a
property of the runner, so it applies just as much to tooling work —
`native/AGENTS.md` has the worktree setup commands.

Everything the tool does is declared in `build.toml` and typed in
`glyph/src/manifest.rs`. A key the code does not know is a parse error; a field the
code does not read is a `dead_code` warning against a zero-warning gate. That is
deliberate: this repo shipped a manifest whose `needs` edges were declared and
read by nothing at all.

## What the checks actually do

Fifteen gates (2026-10-09), in `build.toml` order. For each: what it compares, what makes it
red, and **what it cannot see**. The last is the part worth reading. A check is
a claim about a counterfactual, and a check whose blind spot you don't know is
a green you can't price. (Gates were once numbered positions in one shell
script, `N/9`, renumbered twice; old reports use the numbers — map by name.)

**Products — `glyph build`, and the first thing `test` does.** Not a check, and
it cannot catch anything; it is here because everything below is a statement
about an artifact, and a stale one makes every statement false. The renderer
is a *product*: nothing to compare against, it only has to be CURRENT.
Currency is a content hash of the declared inputs — `native/src`, `crates/`,
`native/Cargo.toml`, `glyph/Cargo.toml`, and the root `Cargo.toml`,
`Cargo.lock` and `.cargo/config.toml` — stamped at build time, rebuilt only
when that hash moved. It is built at WORKSPACE scope, the scope cargo-build
and cargo-test rebuild it at: a package-scope build resolves features for the
renderer alone and links different bytes, so until 2026-10-09 the stamp
vouched for one binary and every gate after cargo-build ran another (which
is also why `glyph/Cargo.toml`, whose dependencies' features reach the
renderer, is an input). Red only on a compile error, which is FATAL (the battery stops): a
stale binary makes every gate below a statement about the wrong build. Blind
to an input the list does not name: until 2026-10-09 it named a lockfile that
no longer existed and omitted the real one, so a dependency bump left the
renderer "current" (measured). `manifest` now refuses an input that matches no
file.

**manifest.** `build.toml` against itself: every `needs` resolves and orders,
every gate and mutation names a real target, every golden output is keyed by
`{gpu}`, and every product input — glob or literal — matches at least one
file. Blind to whether the declared inputs are the RIGHT ones: it proves a
pattern matches something, never that it matches everything that matters.

**committed-artifacts.** One mechanism per generator, all driven from
build.toml: generator-native check modes for the schema (`gen_schema.py
--check`, which also runs the schema's own invariant validation), the emoji
sheet (`gen_emoji_sheet.py --check`), the cluster class table
(`gen_cluster_table.py --check`) and the emoji demo corpus
(`gen_emoji_corpus.py --check`); the four atlas bins are rebuilt by
`export-atlas.mjs` into a scratch dir and `cmp`'d. (The schema was ungated from the engine's retirement
until 2026-10-09: its generator still emitted a half for the retired engine,
so `--check` was red and no artifact declared it.) Red when a generated
artifact is hand-edited, or a generator changes behaviour. Blind to
whether the *inputs* are right: it proves each artifact is what its generator
makes, not that what it makes is correct. For the atlas's lookup tables,
atlas-tables (next) checks part of that.

**atlas-tables** (`tools/check_atlas.py`, 2026-10-09). What `codepoints.bin`
and `glyphs.bin` MEAN, where it can be checked: the two agree (magic, version,
shape, upem, missing-block metrics, the sequence slots ending at glyphs.bin's
slot count), pinned lookups resolve where they must ('A' 34, ' ' 1, RAT 3839,
ROCKET 4759, TAG SPACE missing, the ZWJ family 6819), every entry is one em
tall and block 0 is the missing block. These checks lived in the generator of
`engine-trie.bin`, a re-containering of the same tables for the retired engine
that no Rust code read; the blob was retired (D9) and its checks kept. Its
domain sweep — the font-unit → world conversion is the NEAREST f32 to the
exact quotient, ties to even, over every u16 — now runs in Rust against the
conversion that ships (`text::tests::fu_to_world_is_the_nearest_f32`).
`atlas-pin-moved` proves this gate, `fu-to-world-wrong-denominator` the
sweep. Blind to every codepoint and sequence outside the pins.

The same gate rebuilds the **fixture corpus** in a **scratch copy** of
`engine/fixtures` (generators + vendored inputs + the `../glyph_schema.mjs`
edge) and byte-compares against the committed 34 — an older form deleted the
committed fixtures in place and restored them with `git checkout`, which
needed the restore to be exactly right. The expected counts (26 pipe + 8 bake)
are **declared in build.toml**, never derived from the tree under test: the
old gate `ls`-counted the tree it was checking, so a deleted fixture lowered
both sides of the comparison and stayed green (measured 2026-09-06: eleven of
twelve gates green on a shrunken corpus; only the fixture.rs pin caught it).
The Rust-side pins stay — declared count in build.toml and hard pin in the
test suite are two independent witnesses, not duplicate coverage. Blind to
whether the oracle is *correct* — it proves reproducibility, not truth. And
it only proves the corpus is what it was: whether the Rust layout still
AGREES with the corpus is the job of the reference-port gate (below).

**vendor-hashes.** The hashes of the vendored + derived files
(`vendor-manifest.py --check`; the two `hb.*` files are additionally re-derived
from their ref sources and byte-compared — a semantic pin, not just a hash;
third-party files are also held to their recorded fetch hash). Blind to
upstream drift **by design** (a difference there is information, not a
failure), and blind to a vendored file that matches its own recorded hash while
being the wrong revision for the fixtures that depend on it — which has
happened here, and is caught today only by the fixture rebuild above.

**cargo-build.** `cargo build --release`, zero warnings. Red on any
warning rustc emits; a build ERROR is fatal (the battery stops). Blind to
anything silenced with `#[allow(...)]`.

**cargo-check-no-ui** (2026-10-09). `cargo check` of the renderer with
`--no-default-features` (no egui overlay), all targets, zero warnings, in its
own `target/no-ui` so the changed feature set never invalidates the main build.
It exists because that build is supported and nothing compiled it: an `egui`
use outside the `egui-ui` gate broke it unnoticed (C24, found by the GPU
research agent). `egui-use-ungated` proves it. Blind to whether the no-UI build
RUNS correctly — it is checked, not executed.

**cargo-clippy.** `cargo clippy --release`, zero warnings. Same, for lints.
It runs with `--all-targets` (since 2026-10-09), so test code is linted too:
until then 14 test-only lints had piled up unseen, one of them deny-level.
Blind to anything silenced with `#[allow(...)]`.

**cargo-doc.** `cargo doc --no-deps`, zero warnings. Red when a doc comment names
a symbol that no longer exists, or leaves an HTML tag open. It exists because
renames are constant here and this was the one class the battery could not see:
two links to `Engine::records` survived its rename to `read_back` through a full
green run. Blind to whether the prose is TRUE — it checks that the symbols named
still exist, not that the sentence around them is current.

**cargo-test.** Every test binary in the workspace: naga WGSL validation (the
renderer's and each field-mode crate's), CLI parity, encase lane layout,
`ItemParams` validation, the layout-seam suites, the fixture loader and its
refusals, the fold/scan/bake unit suites (monoid domain, wrap and paginate
rules), `text::reference_layout` bit-exact against every in-domain fixture's
recorded answers, the transcript parsers, the launcher, and the runner's own
verdict logic. The count is deliberately not written here — `cargo glyph test`
prints it next to the floor on every run, and a number in this paragraph would
be one more thing to forget. Red when a test fails, when a whole test binary
stops reporting, or when **fewer than `test_floor` tests actually run** — the
floor lives in `build.toml [settings]`. That floor is a ratchet, not an
equality: adding tests never reddens it, and when the real count rises above
it every green run prints a NOTE naming the number to raise it to — so it
cannot decay into a figure far below reality without saying so. Raise it in
the same commit that adds the tests.

The floor exists because the previous form could not fail. It counted
`test result: ok` summary lines and required two; at the time there were
exactly two binaries, so the threshold was met by the tree's SHAPE rather than
by anything running. Verified 2026-09-06: marking three tests `#[ignore]` left
the old check printing `PASS tests green` and the new one printing
`FAIL — 82 tests ran, floor is 85`. The realistic loss was never deletion — it
is a dropped `mod` declaration or an `#[ignore]` that outlives its reason,
neither of which rustc says a word about. **This matters more than it looks**,
because the pins that keep the fixture corpus from silently shrinking
(`native/src/fixture.rs`, 26 pipe; `native/src/bake.rs`, 8 bake — both worded
"update deliberately") live inside this check.

**pick-oracle** (`tools/check-pick-oracle.sh`). Scripted picks and pixel-ray
round trips from the native binary against an independent Python fold oracle.
Red on any pick resolving to the wrong record. Since 2026-09-10 it also probes
`native/fixtures/emoji-view.txt`: a row/col pick never sees a glyph's advance
(col is a leader count on both sides), so the emoji probes are pixel-ray
round trips on a double-advance cell and on the cell two leaders AFTER it —
the place a mis-sized rect would put the ray in the wrong glyph. The oracle
itself knows nothing of advances, which is why it is a witness here. One
mechanical caution: under `set -euo pipefail` an oracle that exits nonzero
inside a command substitution aborts the script mid-run. The wrapper still
reports FAIL, but every check after the abort point silently did not run.
Blind to pick paths outside the scripted set.

**reference-port** (`tools/check-fixture-parity.sh`; re-gated 2026-10-09).
The Rust layout forms against the JS oracle's recorded answers, five halves,
with the volumes they clear — quote these when you change it, because a count
that quietly drops is how this check would go vacuous without going red:
`--fixture-trie` (26 fixtures, 23,552 trie entries rebuilt from bytes),
`--fixture-fold` (the serial fold, 155,222 leaders, 1,874,328 per-byte lanes
bit-exact), `--fixture-scan` (208 cases across 8 tunings; 1,188,024
leader-lanes bit-exact, 53,752 within 1e-4), `--fixture-bake` (8 fixtures,
27,315 leaders, 167 checkpoints, 530 seed-protocol queries) and
`--fixture-reference` (text.rs over its domain: 4 fixtures, 5,332 records).
The script declares the corpus size (26 + 8) and refuses any other; the bake
fails if no query ran, the reference if nothing was in domain. Red on any lane
that leaves its tier. `phantom-row` and `advance-zeroed` prove it reddens
through the fold, `fixture-deleted` that a shrunken corpus is refused. Blind
to the engines that SHIP — these are `fold.rs`/`scan.rs`/`bake.rs`/`text.rs`,
not HyperLayout — to the atlas trie (each fixture carries its own
synthetic one), and silently to the 22 fixtures outside text.rs's domain. The
parse is checked only through the lanes computed from it: the second loader
it was once diffed against (Mojo) is retired.

**hyper-oracle** (`--hyper-oracle-check`, `native/src/hyper_oracle.rs`; new
2026-10-09; **green** since the C10/C13 fix the same day, and over the paged
fixtures too since the C14 fix, dd6fe01). HyperLayout —
the production layout engine — against an oracle-backed reference:
`fold::run_pipeline` (decode, sequence pass, fold, paginate — the form
reference-port holds bit-exact to the oracle), its records compacted by
`layout::compact_records_into` and diffed through `layout::diff_backends`
plus a per-record tally that names the byte and both glyph ids. Four tiers:
records (HyperLayout's recording path), host instances and placements (its
host Pass 2, what a device-less load runs), and the DEVICE Pass 2
(`pass2_device.rs`, what every GPU load runs) in both slot formats plus its
placements. The device tier runs the production emitter into host memory:
the unified, discrete and host-staging paths all hand the same
`EmitInputs::run` a raw address of writable memory, so only where the bytes
land is swapped. Corpus: the 26 pipe fixtures' bytes and item params (each item
its own buffer, as a repo file is, in its own cluster mode; `paged-rows`,
`paged-cols` and `scroll-only` carry the `page_rows`/`page_cols`/`scroll_rows`
classes), the IMMUTABLE
`cubecl-fork` corpus, `g-cluster-repo`, `overflow-leads.txt` (the
out-of-range decode input no check read after 2026-09-30), `g-pick-repo`
(every repo golden's corpus; `wide.txt` is cut inside its long line) and the
IMMUTABLE `chunk-cut.txt` and `chunk-cut-paint.txt` (built for the
intra-line cut classes, below) in cluster mode, then those six again in leader
mode. STRICT refuses zero records, zero
resolved sequences, and zero ASCII-led sequences; both runs refuse zero device
slots. **History**: red on arrival, 6 of 29 corpora. The keycap class
(HyperLayout's ASCII fast path never asked whether `1` starts a sequence, so
`1️⃣` laid out as `1` plus a stray mark) and the cluster MODE (ignored:
leader-mode ZWJ zeroed in `cluster-zwj`; 29,826 records of `cubecl-fork` +
`g-cluster-repo` under `--cluster-mode leader`) were fixed together; the
device tier, added with the fix, then found the device Pass 2's page extent
dropping the newline's own record (fixed too; page extents feed no pixel).
The three paged fixtures were split out to a red `hyper-oracle-paged` gate
until C14 was fixed (both Pass 2 paths paginated them unlike the fold:
`row + scroll_rows`, `y_page` from the unscrolled row, `x_page` dropped
without `page_rows`; the device line path never turned a column page; Pass 1's
stride took an unterminated line's x after its last advance). Records agreed
throughout — the recording path had its own, correct paginate. The emitters
now share one transcription of `fold::paginate`, `layout_hyper/page.rs`.
Note that every REPO load is paged (`file_item_params` sets `has_page` and
`page_rows`; only `scroll_rows` and `page_cols` are unset), so this code is
on the common path. What the two sides SHARE, and this therefore cannot see: the atlas trie
(`TrieTable::lookup`, `fu_to_world` — the fixtures' own tries are world-unit
and HyperLayout resolves font units, so the oracle has never seen the atlas
trie), and `fold::{rows_for_line, wrap_segment_of, wrap_row_of}`, which
HyperLayout borrows from the reference (reference-port fences those:
`phantom-row` reddens it and leaves this gate's count unmoved). Also blind to
the device path past the emitter (the staging copy, buffer chunking), the
Derived field's vertex-stage Y/Z, and the Pass 1 prefetch. Two paging
classes no gate corpus reaches — a column page that turns inside a fold unit
(`page_cols` under a different `wrap_width`) and an unterminated widest last
line in a multi-page item — are fenced by the unit grid
`hyper_oracle::tests::pagination_agrees_on_every_tier` instead (cargo-test).
Mutations: `hyper-head-advance`, `hyper-keycap-lookahead-dropped`,
`hyper-leader-mode-ignored`, `hyper-page-scroll-sign`, and three only the
device tier can see, `hyper-device-leader-mode-ignored`,
`hyper-device-newline-page` and `hyper-device-x-page-dropped`; on cargo-test,
`hyper-device-page-cols-run` and `hyper-pass1-stride-after-advance` (the two
classes above).
**Intra-line chunk cuts (C15, fixed 2026-10-09).** A line with no newline
within 64 KiB of the chunk threshold is cut INSIDE the line, and the chunk
after the cut inherits the line's column and advances. Two defects lived
there, both found and re-derived on this gate's device tier: the inherited
segment advance was a product (`(col % fold_unit) * ascii_adv`), not the
fold's running f32 sum (65 slots of `g-pick-repo/wide.txt` an ulp off), and
the cut fell on any codepoint boundary, so a sequence across it was resolved
in two halves by the chunked passes (the device emitted the pieces; Pass 1's
survivor count outran the host emission). Now a cut lands only before an
ASCII byte (the atlas asserts no sequence has an ASCII member after its
first), the seed is the running sum, and Pass 1 measures a continued line's
width with its true seed wherever the page stride reaches an output.
`chunk-cut.txt` carries a sequence across each of its three 64 KiB targets,
non-ASCII advances before each cut, and continuations wider than any ASCII
segment past the first page; mutations `hyper-chunk-seed-product`,
`hyper-chunk-cut-splits-sequences` and `hyper-continued-line-unmeasured`.
**The paint tier** (C17, 2026-10-09). The four tiers above run flat paint.
A fifth runs the device Pass 2 again under `Paint::SyntaxHeuristic` and holds
every lane to the reference instances painted by `text::colorize_leaders`
over the WHOLE item, which is what the host Pass 2 paints. It is not
oracle-backed (the JS oracle has no paint): it proves the device's
line-by-line colouring agrees with the whole-item colouriser, not that the
heuristic is right. Red on arrival, it found four disagreements, each fixed:
the device restarted its colouring at every intra-line cut (578 of
`wide.txt`'s glyphs; the only one that moved pixels — `repo-down` 281 px,
`repo-back-oblique` 747, `repo-wide` 20 vs main@164af5c on this host, all of
it measured to this one change); the generic colourisers judged a word that
runs into a string (`return"$"`) up to its last word byte and filled any
colour back, where the ASCII fast path judges it up to its end and fills only
a keyword (`real-minified`); the whole-item colouriser never flushed an
unterminated last word, and the per-line one panicked on a lead truncated at
a line's end (`malformed`); and the device's all-comment-line shortcut painted
indentation comment (`real-kernels`; no ink, no pixel). `chunk-cut-paint.txt`
puts a `return` across every intra-line cut, so the tail half has a witness
(`wide.txt` reaches only the head half). Mutations: `hyper-cut-head-own-share`,
`hyper-cut-tail-own-share`, `hyper-comment-line-indent-painted`, and on the
colourisers' unit tests `colorizer-fills-any-word-colour` and
`colorizer-last-word-unflushed`. Shared and unjudged: the heuristic's own
rules, and `--render-file`'s tokenizer (`stage_file`), a third copy no tier
reads. Measured but ungated (2026-10-09): a 94 MB tree of crates.io sources
is bit-exact on every tier in both modes (99 M records).

**repo-verify** and **repo-verify-direct** (`--repo-verify`, re-gated
2026-10-09). `--repo-engine hyper` and `direct` over `g-pick-repo`, both wrap
modes, diffed against a fresh recording HyperLayout at the seam: placements,
instances, and records where both sides have them (`direct` has none and its
PASS line says `0 records`). Each refuses a verify over zero items — before
2026-09-07 a missing corpus printed `PASS: 0 items` and exited 0. Read the
pair honestly: **every `--repo-engine` constructs the same
HyperLayout**, so this is HyperLayout against HyperLayout. Under
`--repo-scan-only` (no GPU) the sides differ only by `hyper`'s background
prefetch of Pass 1, and for `direct` not at all, so these catch
nondeterminism in the parallel passes and prefetch drift — truth is
hyper-oracle's job. `batch`/`naive` are not gated: they run the reference's
own code path verbatim. Both gates are UNCOVERED in prove, and that is the
finding, not an omission: since C15 the prefetch and the inline Pass 1 are
one driver (`pass1_over_chunks`) and differ only in how the chunk list is
built (before it, only in a segment-advance table g-pick-repo never read:
doubling it stayed green, measured 2026-10-09), and nothing separates
`direct` from its reference without a device.

**pixel-ab.** The golden views re-rendered and byte-compared against
`out/tooling-ab/baseline/<key>/` — `cargo glyph graph` lists them, and the count
is deliberately not repeated here because it has changed. This is the **only**
check that sees pixels. In build.toml those PNGs are class **golden**: verified,
with NO build path — the runner refuses to regenerate them, because re-baselining
is a human act. Red on any change to camera, shading, layout, shaping or culling
that reaches one of those frames. Blind to everything outside them, and it cannot
distinguish a regression from an intentional change, which is deliberate. A
commit that moves a pixel on purpose re-baselines in the same commit and says
why; one that does not leaves every later change unprovable on that platform
(2026-10-07/08: a camera recentre and a palette change landed without
re-baselining, and both platforms' sets went red until 2026-10-09, when both
were re-adopted — `21f9aef` names the commit behind every pixel that had
moved, and the walk found two regressions the red gate had hidden: a perf
commit that culled `long.md`'s far lines (`048d403`, repaired by `9c96ad1`)
and per-chunk syntax colouring (`8b28f1f`, C17). A red gate stops seeing the
NEXT change too).

**One golden set per rasterizer, since 2026-09-07.** `<key>` is what the
renderer prints from `--gpu-key`: `<backend>-<vendor>` off the adapter wgpu
actually picked (`metal-apple`, `vulkan-nvidia`), resolved by the runner from
the `{gpu}` token in build.toml. The first Linux run showed why: against the
Metal set, NVIDIA's Vulkan rasterizer differs by ~1 level over 1-4% of pixels
plus a few dozen ISOLATED single-pixel coverage flips at quad edges, while every
numeric check — fold, scan, bake, picks — is bit-exact. Pixels
are a property of the rasterizer; the layout is not. Making vendors agree is
not a goal and nothing here tries. The key is deliberately coarser than the
hardware (a 5090 and a 4090 share a set until a diff proves otherwise); each set
carries an `ADAPTER.txt` from `--gpu-profile` naming the exact device and
driver that made it, and the gate prints a NOTE when the live adapter disagrees
with the record, so a driver update that moves a pixel explains itself.
Escalating the key to device level is a change to `GpuProfile::key` alone.

The gate has three states, not two: byte-equal, DIVERGES (the renderer
changed on this hardware — fix or re-baseline by hand), and **no set for this
host's key** — red, with the adoption commands printed. Adoption is still by
hand: look at the frames, run `cargo glyph drift`, copy, record the adapter,
say why in the commit. `drift` is an INSTRUMENT, not a gate: this host's fresh
renders against every OTHER set, reporting differing pixels, max delta, and
whether the high-delta pixels are isolated (edge flips) or clustered (something
has a shape). The day that line stops saying "edge noise" is the day to look at
the shader; until then a cross-vendor difference is expected and uninteresting.

**One frame once said "clustered", and it was looked at (2026-09-07).**
`repo-back-oblique` drifted metal-apple vs vulkan-nvidia by 8.93% of pixels with
236 clustered high-delta — every other view isolated edge flips, 0 clustered.
(At the 2026-10-09 re-adoption it drifts 9.87% with 0 clustered; what removed the cluster was not
traced. The reasoning below still holds for the band.)
Both frames were inspected side by side: identical structure, identical text in
front of the receding column, difference confined to the dense far band where
thousands of coplanar quads overlap and the two rasterizers reject depth in a
different order. That band is the whole point of this view, so it is the frame
where vendors are LEAST likely to agree, and every numeric check is bit-exact
across both. Expected, not a defect — but the instrument was right to make
someone look, and this note exists so the next person does not look twice.
What a set proves is the renderer ON THE HARDWARE THAT MADE IT — a green here on
Linux says nothing about Metal, and `validate` refuses a golden output that is
not keyed.

**Golden equivalents: the Derived field, without a second set (2026-10-09).**
`build.toml [settings] golden_equivalents` lists argument lists under which
every golden view must render its baseline's exact bytes; the gate renders
each view again with each list appended and compares against the SAME PNG
(`PASS  text.png BYTE-EQUAL under --field-mode derived`). It holds one entry,
`--field-mode derived`: the Derived field renders every golden frame
identically to Instanced on both sets (measured when it was added), so its
upload, storage, vertex-stage Y/Z and edit paths are covered with nothing new
to adopt. Before it, no golden rendered Derived at all, and a Derived edit
defect that emptied a frame (C19, below) was invisible. A divergence under an
equivalent with the plain frame equal is the VARIANT breaking — never a reason
to re-baseline. `drift` compares the plain frames only.

**The Visible field as an equivalent, with named exemptions (M4, 2026-10-10).**
The second entry is `--field-mode visible`. Measured on both sets before it
was added: `demo` (no field), `text`, `emoji` and `emoji-cluster` (a
`--render-file` scene has no line table and falls back to Instanced, which
the load says) and `repo-highlight` (visible mode for real: the sidecar's
spans over alpha.rs) render their baselines byte for byte; the five
syntax-painted repo views do not, and the difference is exactly the
load-time syntax paint, which visible mode does not apply by decision —
under `--field-mode visible` each is the stored modes' `--color-mode flat`
render byte for byte (47,154 / 222,277 / 43,715 / 256,744 / 29,181 px of
token colour short of the baseline for repo-wide / -down / -zoom /
-back-oblique / -cluster on vulkan-nvidia; within ±101 px of those on
metal-apple, the rasterizers' usual edge noise). Those five are EXEMPT from
that one equivalent,
each with its why on the view (`[[golden_view.exempt]]`); an exempt frame is
still rendered and its distance printed as a NOTE with the pixel count, an
exempt frame that turns out byte-equal is flagged as a possibly stale
exemption, `validate` refuses an exemption naming no declared equivalent or
no why, and `cargo glyph gates` lists every exemption under pixel-ab — a
named blind spot, priced like the rest, never a quiet drop. So the visible
equivalent holds visible mode to pixels on ONE frame (repo-highlight;
`visible-row-off-by-one` proves it reddens); flat twins of the five would
hold it on their cameras too, and candidate frames for those live in
`out/tooling-ab/sweep/candidates/visible-flat/` (byte-identical to the
visible renders and to Instanced's flat render) for Ivan to adopt or not.

**Which view covers what, because the answer is not uniform.** `repo-wide`
renders the default wrap mode (`back` — a wrap costs DEPTH); `repo-down` exists
to keep the non-default row-per-wrap geometry covered, and was added when the
default moved, because otherwise making one mode default silently retires pixel
coverage of the other. `repo-zoom` covers **neither** meaningfully: `alpha.rs`'s
longest line is 24 columns and never wraps in either mode, so that view is
pinning a camera angle, not a layout. `repo-back-oblique` (2026-09-07) is the
camera pitched down over `wide.txt` so its receding column converges up into
`long.md`'s pages: the only frame in which a glyph at one depth overlaps a
glyph at another, and therefore the only one that can see z-ORDER. It was
added when the glyph pass was found to test depth without writing it, so a
later-drawn file painted over a nearer one — and `repo-wide` had carried that
wrong picture in every baseline since `back` became the default, byte-equal
throughout, because a golden cannot tell a wrong picture from a right one.
The `depth-write-off` mutation proves the new frame reddens on that class.
`emoji` (2026-09-10) is the only frame that samples the colour-emoji sheet —
web-era slots re-pointed at it, appended slots, slots the font cannot draw,
and emoji-in-the-font-that-stay-text, all through the CPU staging path — so
it is the only frame that can see a cell placement, inset, flip or alpha
error, none of which any numeric gate can see because the layout is
untouched. `emoji-uv-flip` proves it reddens. Expect MORE cross-vendor drift
on it than on text: filtered sampling of a mipmapped sRGB texture is not
analytic coverage, and two rasterizers' filters need not agree to the bit.
The alpha contract those pixels rest on is stated once, in the shader header
of `glyph_field.wgsl`, so a platform whose emoji edges differ while its text
does not has a checklist.
`emoji-cluster` and `repo-cluster` (2026-09-20) pin the SEQUENCE PASS — one
frame per path: the former through `--render-file` (the CPU staging twin),
the latter through `--load-repo` (the repo load end to end). One line per
sequence class the trie resolves (families, flags, skin tones, keycaps, tag
flags) plus the fallbacks (unlisted chains stay pieces, ZWJ/VS16 zero-width);
the `cluster-static-zero-off` mutation proves they redden. The `emoji` view
pins LEADER mode by hand — its fixture is immutable, and the default flipped
to cluster on 2026-09-22, so its command carries `--cluster-mode leader`
explicitly and the pairing now reads one level up: the default gets its pixel
coverage from the unpinned repo views (g-pick-repo carries no sequences — the
flip moves no pixel there), and leader stays pinned here.
Known cost of the depth-write fix, measured: in the dense far region of
`repo-down`, ~1,400 of 1.6M pixels lose a little ink where coplanar quads
overlap and the later fragment's interpolated depth lands an ulp behind — the
price of a blended pass writing depth, accepted over draw-order visibility.

`repo-highlight` (2026-10-09) is the only frame through the highlight
sidecar (`--highlight`), the byte-range styling path the AST/LSP colouring
will arrive by: `alpha.rs` whole, six spans recoloured over syntax paint.
Under the Derived equivalent it is the witness for C19 — Derived's
`write_placements` rewrote whole slots with the row and wrap zeroed, so every
edited glyph left the frame (`derived-placement-row-zeroed`). Its sidecar is
deliberately out of byte order, which found a second defect when the frame
was made: `style_file` walks runs as byte-ascending and the sidecar parser
never sorted, so an out-of-order span was silently skipped
(`sidecar-runs-unsorted`).

It is also less all-seeing than it looks. The page-extent origin seed was
renderer-affecting and every view stayed byte-equal, because the seed only binds
for an item with zero records and no fixture had an empty file. It can see that
class today only because `native/fixtures/g-pick-repo/empty.rs` was added for
it. **Do not tidy that file away.** Ask what else these frames cannot see —
`--wrap-mode back` on a repo whose files never wrap is the current example,
and a second, proven one: **the far-LOD backdrop tint never fires in any
golden view** — the cameras keep all five `g-pick-repo` files near enough that
no segment substitutes its backdrop quad, so the seg_tint lane is
pixel-invisible. Its byte-level fence lived in the cubecl-fork gate, retired
with CubeCL (2026-10-09), so that lane is currently watched by nothing.

### Retired 2026-09-30, re-gated 2026-10-09

`5e94de8` ("decouple CubeCL … and clean build.toml") removed seven gates and
26 mutations. Three served only the retired engine (engine-suites,
engine-check, and its Mojo halves). Four checked pure-Rust paths whose
instruments kept shipping and kept passing, run by nothing for nine days;
they came back as reference-port, repo-verify(-direct) and
cubecl-chain/-fork (the last two retired again on 2026-10-09 with the CubeCL
engine itself; their corpus, `native/fixtures/cubecl-fork/`, stays, read by
hyper-oracle), and the check that never existed — HyperLayout against
the oracle — arrived with them, red. In those nine days HyperLayout became the
only engine, and nothing compared it to anything but itself.

The lessons these gates taught stay true and are the pattern for any new one:
**a check must refuse to pass having compared nothing** (the bake fails if no
query ran, the reference fails if nothing was in domain, repo-verify refuses
zero items, STRICT refuses an unexercised bucket, the parity script refuses a
corpus of the wrong size), and a count that quietly drops is how a check goes
vacuous without going red — quote the volumes. And a gate that compares an
engine to itself is a determinism check: say so in its `blind_to`.

### What the whole battery cannot see

Worth holding in one place, because each check's blind spot is defensible alone
and the union is not:

- **The device path past the emitter.** Since 2026-10-09 hyper-oracle runs
  the device Pass 2 (`layout_hyper/pass2_device.rs`) into host memory and
  holds it to the oracle; what happens to those bytes afterwards (the
  staging copy into VRAM, the buffer chunking, the Derived vertex stage's
  Y/Z) is seen by pixels alone.
- **The atlas trie's VALUES.** Every oracle comparison runs on the fixtures'
  synthetic tries; hyper-oracle runs both sides on the atlas trie, so a wrong
  advance in `codepoints.bin` moves both sides together.
- **Nothing executes the benches.** `tools/bench_hyper.py` is run by hand.
- **`tools/verify_atlas.py`, `preview_glyphs.py`, `repro_pick_oblique.py`** are
  manual tools, run by **zero** checks (atlas-tables covers the lookup tables'
  consistency and pins, nothing of the curves or glyph map). So the atlas bins' structural and semantic
  correctness, and the oblique-pick repro, are exercised by nothing in the battery
  — the atlas is only ever checked for being byte-identical to what it was, which
  says nothing about whether what it was is right.
- **PROVENANCE.md's prose is generated but only partially validated.** The
  fixture count is READ FROM build.toml at generation time — the generator
  refuses to write an unverifiable number — but the rest of the prose is
  hand-written inside `tools/vendor-manifest.py` and nothing re-validates it;
  the vendor-hashes gate only ever compares hashes and re-derivations.

### Earning a green

A pass is a claim about a counterfactual, so test the counterfactual: break what
a check watches and confirm it reddens. Some of that is now mechanical —
`cargo glyph prove` applies each mutation declared in `build.toml`,
requires the named gate to go red for the named reason, and restores byte-exact.
It reports COVERAGE rather than a pass count, so a gate nobody has proven is
listed as uncovered instead of being counted as working — run it and read that
line. This paragraph deliberately states no figure: the previous version did,
went stale, was corrected, gained a caveat saying it had gone stale, and went
stale again the same week. A number in prose that describes the tree is a
number that rots; the tool prints the live one. Three times this repo shipped a check
that could not fail — a gate asserting on float noise, a ceiling constant no test
protected, and a fixture checksum comparing bytes guaranteed identical before the
command ran. Every one was caught by execution; not one by inspection.

And **a mutation that produced no failure proves nothing until you know it
landed.** "I broke it and nothing failed" and "I failed to break it" print
identically. Assert the edit applied, then read the result. Equally: a source
scan is worth exactly what its match set is worth — "no grep match" is not "does
not exist," which has produced a wrong conclusion here as recently as
2026-09-06. A measurement can be undone before it is read, too: `cargo run -p
glyph` re-serializes `Cargo.lock` before the runner hashes it, so a lockfile
perturbation "proves" nothing that way (2026-10-09) — run `target/release/glyph`.

There is a THIRD outcome, beyond "landed" and "failed to land": **landed in a
region nothing reads.** Measured 2026-09-06 — resolving an out-of-range
codepoint to a real trie block instead of the shared missing block left every
check green but one, because no fixture in the corpus carries an F5–F7 lead
byte; only `fixtures/overflow-leads.txt` reaches that branch. So a mutation's
`why` names the CONSUMER it perturbs, not just the defect it stands for, and
"the gate stayed green" is a claim about the corpus until you have shown the
mutated line is on a path the corpus walks.

A cross-form comparison is blind to a fault inside a function both forms
share. Measured 2026-09-06: the wrap rule counting the terminating newline's
own row (`phantom-row`) reddened the oracle-backed fixture comparison and left
the serial-fold-versus-scan comparison green, because both forms call
`rows_for_line`. Only the oracle can see a shared defect.

## Fences — generated, vendored, or immutable

| Path | Status | Why it is fenced |
|---|---|---|
| `assets/atlas/{curves,glyphmap,glyphs,codepoints}.bin` | generated | `tools/export-atlas.mjs` from `tools/vendor/ref` AND `emoji-sheet.bin` (the emoji slots after the web's 4,431); hand-edits are reverted by the next rebuild-and-compare |
| `assets/atlas/emoji-sheet.bin` | generated | `tools/gen_emoji_sheet.py` from the vendored Noto Color Emoji; regenerate it BEFORE the atlas bins, which read it |
| `assets/atlas/cluster-classes.bin` | generated | `tools/gen_cluster_table.py` from the vendored UCD — the class table every cluster-mode implementation reads; regenerate BEFORE the atlas bins, which carry it verbatim |
| `native/fixtures/emoji-corpus-{small,large}.txt` | generated | `tools/gen_emoji_corpus.py` from `codepoints.bin`'s v2 sequence section — the cluster demo corpus; a demo asset, not a golden input |
| `engine/glyph_schema.mjs` | generated | `tools/gen_schema.py` from `schema/glyph-identity.json`; the fixture generators read it, so editing the schema invalidates the corpus |
| `tools/vendor/` | vendored, hash-pinned | `vendor-manifest.py --check`; upstream drift is information, not failure |
| `schema/glyph-identity.json` | vendored verbatim | drift means an upstream refresh, not a local edit |
| `native/src/shaders/*.wgsl`, `crates/*/shaders/*.wgsl` | fenced | the naga tests (`native/tests/wgsl.rs`, and each field-mode crate's own `tests/wgsl.rs`) pin the shader *set* — that it compiles and exists, not what it draws. The only thing that sees a pixel change is the golden-view A/B, whose blind spots are above. That gap is why edits here need their own re-baselined change rather than an ordinary commit. `glyph_field.wgsl` moved byte-identically (git mv) into `crates/glyph-field-instanced/shaders/` on 2026-10-05 when the glyph field split into render modes; a move is not an edit, and the goldens are the proof |
| `native/fixtures/baseline-view.txt` | IMMUTABLE | it is the input to `text.png`; editing it re-baselines that check silently |
| `native/fixtures/emoji-view.txt` | IMMUTABLE | the input to `emoji.png`, one line per class of bitmap slot the trie carries; same reason |
| `native/fixtures/g-pick-repo/empty.rs` | IMMUTABLE, zero bytes | the only input that reaches the page-extent origin seed; deleting it removes a check's ability to see its subject without removing the check |
| `native/fixtures/chunk-cut.txt` | IMMUTABLE | hyper-oracle's only input built for the intra-line chunk-cut classes (C15): a sequence across each 64 KiB cut target, non-ASCII advances before each cut, a continuation wider than every ASCII row past the first page. Its cut positions are planned against the 64 KiB threshold and the cut rule, so changing either (or the file) can move a target off its sequence and silently weaken the gate — re-check that each mutation in its `why` still reddens |
| `native/fixtures/highlight-alpha.tsv` | IMMUTABLE | the input to `repo-highlight.png`; DELIBERATELY out of byte order (the `1` span after the string span) — sorting it removes the frame's only witness for `sidecar-runs-unsorted` |
| `native/fixtures/chunk-cut-paint.txt` | IMMUTABLE | hyper-oracle's paint-tier input (C17): a `return` across every intra-line cut, one pure-ASCII line and one not, so a chunk that coloured only its own share of a line is seen at the chunk's END; the only witness `hyper-cut-tail-own-share` has |
| `native/fixtures/cubecl-fork/` | IMMUTABLE | named for the retired CubeCL fork check, now read by hyper-oracle (both runs) — the only committed input exercising paginate's m >= 3 / segment >= 3 classes, the cluster classes at wrap/page boundaries, and the empty-item placement class (`clusters.txt` + `empty.txt`). Editing it hollows hyper-oracle's coverage of those classes silently |
| `out/tooling-ab/baseline/<key>/` | tracked pixel oracle, one set per rasterizer; **golden** in build.toml | changes only on purpose, with a note saying why; the runner refuses to regenerate it. A new host adopts its own set by hand (the gate prints how); it never edits another's |
| `integration/egui/` | vendored reference | never compiled; the real dependency is from crates.io |

Hand-editing a generated file buys a failure on the next run. Regenerate instead
(`python3 tools/gen_schema.py`,
`node tools/export-atlas.mjs`; the justfile has `gen-schema`).

**Which language a thing is written in is a correctness decision, not taste.**
Code that produces or checks an ANSWER stays in its own language, deliberately:
`tools/g_pick_oracle.py` computes a fold independently and adjudicates the Rust
against it, and the vendored JS produces the corpus's expected values. Port
either to Rust and it becomes a second Rust implementation that can share a
fault with the thing it validates — the check would stay green and stop meaning
anything. Machinery that merely RUNS things has no such claim on its language:
the mutation harness edits a file, runs a check and matches a string, so it
moved from Python into `glyph` without argument. Ask which one you are holding
before you rewrite it.

**The JS repo this engine was ported from (glyph3d-js) is read-only history.** It is
the JS oracle this engine was ported from, now retired: `tools/vendor/ref` and
`engine/fixtures/inputs/` are revision-pinned snapshots of it, and the two have
deliberately forked. Edits there are invisible to every check here, so they
cannot be verified and cannot be trusted.

Dependency pins (wgpu 30, winit 0.30, glam 0.33, egui 0.36):
no bump without its own pass. The past bumps were done as multi-part
work and their reports (`out/STAGE_H_REPORT.md`, `STAGE_I_REPORT.md`) are worth
reading — but they agree on less than they look like they do, each having
reinvented its own structure, so take the invariant and not the format: **one
dependency per commit, the full battery green before each commit lands** (not
after, in bulk), the resolved version checked against the published manifest
rather than against a plan, a call-site sweep, and the golden views `cmp`'d into a
named scratch dir. Record what moved and why.

## Vocabulary — and which numbers are alive

Three numbering schemes exist in this tree and two of them are dead. This is the
single most common way to misread the repo, so:

- **Lettered stages (A–L)** are **history, not structure.** `out/STAGE_*_REPORT.md`
  are records of landed work and keep their names on purpose. There has never
  been a canonical index, and there cannot be one now: some letters have no
  report and survive only as retrospective mentions inside later ones. The
  convention is **retired** — see "Where work lands". Outside `out/`, a stage
  letter is archaeology. When you touch a comment carrying one, prefer the
  substance ("since the layout seam", "since the carrier split") over the letter.
- **`Stage 0`–`4` is ambiguous and binds to two different lists.** The live one is
  the reference port: 0 = fixture parity, 1 = trie, 2 = fold, 3 = scan,
  4 = bake — the `--fixture-*` instruments carry those names, and the
  `port: stage N` commits record them. The dead one numbered the layout-seam
  work. Some source comments still reference the dead scheme unqualified.
  Name the thing, not the number.
- **Check numbers (0–9, 1b, 8b)** were positions in one shell script and were
  renumbered twice before the gates got names (2026-09-06). The live
  identifiers are the `[[gate]] name =` strings in `build.toml`
  (manifest … pixel-ab); `cargo glyph gates` lists them. Old reports and
  comments still use the numbers — map by name.

## Where work lands

One logical change per commit, with what you ran in the message. Proof PNGs cited
by a report are committed; scratch renders are not. The battery writes to
`out/tooling-ab/sweep/` (untracked).

**The lettered-stage report convention is retired.** It ran C through L and
stopped on 2026-09-03. What stopped is the `out/STAGE_<LETTER>_REPORT.md`
artifact and its template; the word "stage" is still in use for the reference
port's live numbering. Do not start a new letter; `out/STAGE_*.md` stays as
history.

Some older handoff notes have not caught up and will tell you otherwise —
`integration/notes/08-view-structure-handoff.md:71` instructs you to file a report
"(house cadence, per AGENTS.md)", and `06` and `09` say the same for their stages.
**Those stages already executed and those instructions are superseded by this
file.** `integration/notes/README.md` still lists 08 and 09 as "PLANNED, not
executed", which is also no longer true.

Multi-part work that moves the pixel baseline or a dependency still deserves a
written note in `out/` saying what changed and what you ran; it just is not a
"stage" any more and does not need that template.

## Read next

- `native/AGENTS.md` — Rust crate rules, module contracts, debug env vars.
- `native/src/*.rs` module headers — the real per-module contracts.
- `assets/atlas/FORMAT.md`; `schema/glyph-identity.json` when touching layout.
- `TOOLING-PLAN.md` — why the tool is shaped the way it is.

`out/*_REPORT.md` are **records, not current state**, and some state facts that
later stopped being true. Read them for why something was done, not for how
things are.
