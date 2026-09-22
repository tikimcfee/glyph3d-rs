# Cluster mode — the sequence pass

A UTF-8 leader has always produced one glyph. Under cluster mode, a codepoint
SEQUENCE the font draws as one glyph resolves to its single slot — the thing
`out/EMOJI.md` carried the data for since 2026-09-10 ("the sequence table is
carried so a future shaping pass finds its cells already placed").

    ClusterMode::Leader   (0, the default)  one glyph per UTF-8 leader.
    ClusterMode::Cluster  (1)               the sequence pass: resolve per item,
                                            between decode and the fold.

## The rule

Per item, one serial walk over the leaders (the engine runs items in parallel —
`engine/glyph_cluster.mojo`; the oracle is `resolveClusters` in
`engine/fixtures/inputs/glyphPipelineReference.js`; the Rust twin is
`fold::resolve_clusters`; the CPU staging path's is in `text::stage_file`):

- The invisible-by-design characters never occupy a cell: ZWJ, the variation
  selectors, the tag characters.
- A codepoint some sequence starts with (the table's own first members — the
  candidacy set can never drift from the table) probes the sequence table for
  the LONGEST prefix match. A match gives the head the sequence slot at the
  trie's bitmap advance; the span's trailing leaders become trailers: glyph 0,
  advance 0, `F_CLUSTER_TRAILER` (bit 16).
- FE0F is normalized out of the probe key — the font's GSUB strips VS16 — while
  the VS16 leaders ride the trailer span. VS15 breaks a probe (text presentation
  asked, text presentation honored). A newline or the item end ends a candidate
  (GB4/GB5).
- RI pairs need no parity state: the serial skip-past pairs them greedily from
  the left, which is exactly GB12/GB13. A pair the table lacks stays two single
  letter-glyphs (the fallback is pinned, not assumed).

## What does NOT change

Records stay per leader: ROW/COL count leaders, the witness lanes, the
paint-by-record contract, the pick cross-check, the ordinal machinery all stand
untouched. The fold reads the resolved static lanes exactly as it always did —
a zero advance is an exact no-op in its f32 sums. The scan monoid reads the same
arrays and never learns what a cluster is (no new lanes, no junction-term
widening — the wrap-mode note's precondition is undisturbed).

## The data is generated, never hand-written

UCD 17.0.0 vendored under `tools/vendor/third-party/unicode-ucd/`;
`tools/gen_cluster_table.py` bakes `assets/atlas/cluster-classes.bin` from it
(full GCB space + the emoji-data properties — the general UAX #29 phase rides
the same table later). A hand-rolled state machine in the planning experiment
broke keycaps and was caught only because its test expectation was also wrong
in the other direction — that failure is why the generator exists.

## What the work caught in flight

`run_streaming` (`engine/glyph_record.mojo`) hand-copied Item fields and never
carried the new one, so the scratch-pool path laid out unclustered where the
whole-corpus lay resolved — found by `conformance_record`'s stream-vs-whole
diff on cluster-zwj before anything rendered wrong. The field-copy class of
defect, named at the fix.

## Verified

- 25 pipe fixtures regenerate byte-identical under v5 (rule unreachable in
  leader mode); the eight cluster fixtures are bit-exact against the oracle on
  the engine, both scan tunings.
- The golden pair: `emoji-cluster` (the CPU staging path) and `repo-cluster`
  (the engine path, end to end), adopted by hand after a look; the prove
  mutations `cluster-static-zero-off` and `cluster-trailer-advance-one` redden
  them for their named reasons.
- The Debug panel's cluster toggle rides the scene-rebuild arm;
  `GLYPH_CLUSTER_SELFTEST=1` drives it headlessly (instance count drops as
  trailers leave the arena: 359 → 345 on the fixture repo, 893 → 801 on
  `fixtures/emoji-corpus-small.txt`). Repo scenes at landing; text scenes
  joined when the demo corpus wanted the toggle there — the probe seeds from
  the staging choice when the scene has no pick context.
- The `z_wrap_spacing` Debug-panel dial and its `GLYPH_ZSPACE_SELFTEST=1` hook
  landed in the same sitting (0.15 → 0.30 doubles the field's z extent,
  instance count unchanged at 407,133).

## Deferred, deliberately

General UAX #29 text clustering (combining marks, Hangul, Indic) — the class
table already carries their classes; the segmentation FSM for it is proven
exact under regrouping (the transition monoid closes at 16 elements; a summary
is one u64). The GPU cluster kernel (the device suites skip cluster fixtures,
printed). 2026-09-22: its first step landed — the rule decomposed into a
per-position probe + a commit chain (cluster_split.mojo), swappable via
run_pipeline's comptime `split` param and proven bit-exact against the serial
rule by conformance_split (cluster-overlap.pipe.bin carries the phantom
case); the monoid design stays shelved, the chain's carry is one integer.
Bake v4 (the tail fold re-derives trie advances today; `conformance_resume`
skips cluster items, printed). Shaping — Turing-complete, CPU-side everywhere
in the industry, and this engine's measured-placement model does not need it
yet.
