# The Zed integration seam — contract, decisions, and ladder

Status: design frozen for P1 (in-process, live buffers) as of 2026-09-25, after
the S1–S3 spikes landed (`fc6a197` headless chunks + One Dark HTML, `fd1731f`
`--highlight` op, `8bcb1d8` sidecar emission + demo). This doc is the surface
the spatial-workspace grammar will be built on; it graduates out of this
directory only when the integration stops being an experiment.

Everything here is *records, not arguments*: each claim cites the commit that
demonstrated it or names the rung that will.

## What the seam is

The boundary between Zed's editor machinery (buffers, tree-sitter, themes,
extensions — everything below their `editor`/`workspace`/`ui` crates) and our
glyph field (fold → records → instances → pixels). Proven consumable as pure
data with no window: S1 pulled 17,003 styled runs from Zed's own `language.rs`
through `BufferSnapshot::chunks(range, LanguageAwareStyling)`; S3 pushed those
runs through to 76,129 GPU instance colors via `--highlight`.

## Decisions (settled 2026-09-25, Ivan + ZCode)

1. **Link, not IPC.** The Zed crates compile into our process (proven: the
   whole graph builds as path deps on our toolchain — see the patch/lock notes
   in `experiments/zedspike/Cargo.toml`). No IPC pipeline: it would confound
   loading and update handling on top of buffer/byte-source oddities that are
   already hard enough.
2. **Edit-space keys are in the contract; joins are version-stamped, never
   ambiently remapped.** No runtime offset-bridge cache. The provider emits
   `(version, runs)` where the runs are over that version's byte space; the
   renderer folds that version's bytes; the join rule is version EQUALITY,
   checked — mismatch is a visible drop-and-request, not silent misalignment.
   Anchors: `crates/text` (where `Anchor` lives) has no gpui dependency in
   normal builds, so anchor types MAY cross the seam at the round-trip
   boundary (P4) without dragging the runtime in. Byte-range + version is the
   v1 vocabulary.
3. **One envelope, four planes — stages populate fields, never fork paths.**
   The anti-sprawl law (the compositional wall is the failure mode this
   design exists to prevent): style-only v1 ships `structure: []` and
   `decorations: []` — empty, not absent. Folds/inlays/blocks arrive as new
   `StructureDelta` variants through the SAME apply path.
4. **The field owns wrapping.** One wrap owner. Zed's WrapMap shapes through
   the platform text system; ours is cell-grid arithmetic. Zed soft-wrap
   settings translate to a column count at the seam. What we consume from
   their DisplayMap: folds (row ranges), inlays (styled text at anchors),
   blocks (degrade to plain rows, v1 does none of this).

## The envelope

```rust
struct BufferVersion(u64);        // monotonic, provider-assigned per edit

/// Everything known about one buffer at one version. One apply path in the
/// renderer walks this; stage scope = which fields are populated.
struct SurfaceUpdate {
    file: FileKey,                // v1: rel_path; later: Zed ProjectPath
    version: BufferVersion,
    content: Option<Vec<u8>>,     // None = tombstone (file gone; keep records)
    style: Vec<StyleRun>,         // (byte range, fg, weight, italic, …) at `version`
    structure: Vec<StructureDelta>, // FoldRows/UnfoldRows/Inlay/… — later stages
    decorations: Vec<Decoration>, // selections, carets, search hits — P4
    complete: bool,               // parse-in-flight: apply partial, honestly
}
```

Join invariant (the load-bearing rule): style/structure offsets are valid
against `content` **at `version`** and nothing else. The renderer records
which version it folded; a `SurfaceUpdate` whose version ≠ the folded version
is dropped with a log line and a re-derive request. No translation layer
exists to drift.

## Planes and ownership

| Plane | Ours | Zed's | Join |
|---|---|---|---|
| Content (bytes→records→instances) | fold, records, instances — unchanged, oracle-checked | CRDT buffer snapshots | snapshot bytes → fold input, per version |
| Style (ranges→colors) | instance color writes (`--highlight` mechanism) | chunks() + SyntaxTheme | byte ranges at a version |
| Structure (wrap/folds/inlays/blocks) | wrap (cells), rows, z-layout | DisplayMap layers we consume | fold row ranges; inlays at anchors |
| Round-trip (P4) | pick → (file, byte, version) | byte→Anchor at that version | pure function of a version |

## Runtime shape

- **Provider thread owns one headless gpui `App`** — the `remote_server` /
  `HeadlessProject` pattern, NOT `TestAppContext` (spike-flavored). The App
  never touches the window; the render loop never blocks on parsing.
- Channels between provider and render threads; backpressure by coalescing —
  at most one envelope per buffer per coalescing window; intermediate edits
  collapse into the surviving final version.
- Failure semantics: partial parse → `complete: false`, apply what's there.
  Language change → new provider binding, wholesale style replacement at the
  same version. Binary/unparseable → content-only envelope (today's raw-fold
  behavior). Deletion → tombstone.

## Zed-side facts this design rests on

- Everything below `editor`/`workspace`/`ui` runs windowless; gpui imports in
  `project`/`language`/`text` are the entity/async runtime, never rendering.
- `chunks()` output `HighlightId`s are theme-space indexes (built per language
  by `Language::set_theme` + `build_highlight_map`) — a backend consumes
  (text, color, weight, italic) directly, no name resolution (S1).
- Extension host is WASM, narrow data contracts, cannot draw UI — extensions
  become field-drivers through this seam without ever touching pixels.
- Path-dep integration costs are pinned in `experiments/zedspike/Cargo.toml`:
  the `[patch.crates-io]` table must be re-declared per workspace; the
  `async-task` patch rev is upstream-dead (crates.io instead); `Cargo.lock`
  is SEEDED from Zed's (fresh resolves drift — rust-embed grew a required
  trait item their fs_embed arm doesn't implement); `util/debug-embed` is
  required or asset lookup walks the EXE's ancestors for a `.git`.

## Gate coverage (whose half is pinned by whom)

- **Ours:** a checked-in micro-sidecar over a fixture file + a golden view
  that applies it — deterministic, no Zed dep in the gate, pixel-pins the
  join (offsets → records → instance colors). The `--highlight` op is
  otherwise opt-in-only; battery proven byte-neutral without it (`fd1731f`).
- **Theirs:** tree-sitter/theme correctness is Zed's test suite's job. Same
  oracle-boundary principle as the rest of this repo: trust the other side's
  witnesses; don't pretend to cover their defects.

## Ladder

- **P0 (landed):** sidecar file, static, whole-file, offscreen op. `fd1731f`,
  `8bcb1d8`.
- **P1:** provider module behind a trait (still file-driven) → provider
  thread + channel envelope (sidecar file dies) → live buffer events →
  re-derive + re-fold per version (measure whole-file re-fold per keystroke
  BEFORE building record-splicing; the scan monoid is the eventual shape).
- **P2:** structure plane (folds first), micro-sidecar golden view.
- **P3:** provider-shape generalization — themes, grammars, extensions as
  interchangeable run-producers.
- **P4:** round-trip (picks as anchors), decorations, editing.

## Open (not blocking P1)

- `FileKey` graduation path: rel_path → ProjectPath/EntityId when the
  workspace grammar lands (that doc follows this one).
- `experiments/zedspike` should eventually `git mv` under
  `zed_integration_experiments/` so code and docs live together — one move,
  done once, not now.
- Whether the micro-sidecar golden becomes a build.toml `golden_view` (needs
  a stable fixture + key) — decide when P2 lands.
