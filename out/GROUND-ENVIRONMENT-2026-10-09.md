# Ground environment + settings-in-config — 2026-10-09

> **Status (2026-10-10): step 1 merged to main; step 2 (lift the layout onto
> the ground, which moves goldens) open.** The worktree named below is
> historical.

Branch `ground-environment`, built in the sibling worktree `../glyph3d-rs-ground`.
This is step 1 of grounding the GUI: put a ground plane and sky behind the scene to
see how it feels, and get every tunable value out of the code first. **No golden
moved.** Step 2 (below) is the one that moves goldens.

Source proposal: another agent's `grounded_environment_proposal.md` (generic wgpu
advice). Its shader idea held up. These specifics did not, for this renderer:

| Proposal said | This renderer | Done instead |
|---|---|---|
| ground at y = 0 | text hangs **down** from each file's origin (`scan.rs:438`); y = 0 is the top edge, so the corpus would be underground | ground at scene min y − `ground_gap`; lifting the layout onto y = 0 is step 2 |
| `LessEqual`, unproject z = 0 near / 1 far | reverse-Z: clear 0.0, `GreaterEqual`, `perspective(fov, aspect, far, near)` | `GreaterEqual`; NDC z = 1 is near |
| bind the scene camera (`view_proj_inv`, `pos`) | glyph camera block is `view_proj` only, in a fenced shader | own 304 B uniform |
| draw last, blended, no depth write | the glyph pass blends AND writes depth; partial-coverage edge fragments would punch halos into a grid drawn after | drawn **first**, opaque, writes depth; sky writes 0.0 |
| fixed 200 / 2000 spacing, unprojected world rays | 1 text cell = 1.0 world unit; eye reaches ~1e4 (f32 cancellation) | spacings in config (10 / 100); camera-relative rays; grid phase reduced in f64 |
| windowed-only enable | leaves the ground unpinned by any golden | `--environment` flag, works offscreen; a golden view for it is step 2 |

## What landed (first 5 commits)

1. `f335d74` — `launch_config.toml` parsed with the `toml` crate (already in the
   lockfile via cubecl). **Unknown keys are errors** naming the key; a config that
   fails to load stops startup (it used to warn and continue).
2. `8d34b72` — `config/defaults.toml` compiled in; per-key runtime overrides from
   the `[section]`s of `launch_config.toml`; rule §10 in
   `.agents/rules/rust-engineering.md` (settings vs contracts). `launch_config.toml`
   is now **gitignored**, and the committed copy is `launch_config.example.toml`.
3. `5755089` — glyph-scene / LOD / camera tunables into settings. The framing
   formula, which was copy-pasted in four places, is now one function.
4. `913c1f0` — repo tints, agent-card colours, UI label colours into settings.
5. `b89de10` — the ground/sky pass: `--environment ground`, `--ground-y`, and the
   windowed **B** key toggles it.

## How it was proven

The `pixel-ab` gate is **red on untouched main on this host**: the `vulkan-nvidia`
golden set is stale, and `emoji-cluster` / `repo-cluster` have none. So each commit
was proven another way:

- **Golden views byte-equal to main.** main's own sweep renders were snapshotted
  first. After each commit, all 9 golden views were re-rendered and `cmp`'d against
  that snapshot: byte-identical, every time (flag off).
- **Every gate other than pixel-ab green** (`cargo glyph test`), with the test floor
  raised in the same commit as each new test: 223 → 236.
- **The settings reach pixels.** Each mutation was asserted to have landed, then
  restored byte-exact:
  - `glyph_scene.clear_color` changed → `text.png` differs.
  - `fov_y_deg` 40 → 41 → `text.png` differs.
  - all `dir_tints` set to grey → `repo-wide.png` differs.
- **Bit-identity of every migrated literal.** Pinned in
  `config::tests::defaults_match_migrated_literals`. TOML floats go decimal → f64 →
  f32, which can land one ulp away from a decimal → f32 literal.
- **Picking.** The same `--pick-px` resolves identically with the environment on
  and off.
- **The environment itself was looked at** (proof renders below): depth order
  correct, no halos at glyph edges, minor lines fade to the major grid with distance
  (they never re-space), both axes present.

| | |
|---|---|
| `ground-environment/text-ground.png` | front camera over `baseline-view.txt` |
| `ground-environment/repo-oblique-ground.png` | the `repo-back-oblique` pose |
| `ground-environment/repo-high-ground.png` | `--cam-pose 150 400 400 0 -35` |
| `ground-environment/axes-ground.png` | `--cam-pose -150 100 250 -30 -35` (x axis) |

## Follow-ups after the first review (same day)

Ivan's review of the windowed build: the ground renders correctly, the sky
gradient helps the space read, and the fog is too dense on tall content. Four
more commits:

6. `8291387` — **fog is now measured in multiples of the scene's fit distance**
   (`fog_start_fit` 1.5, `fog_end_fit` 12). Fixed world units (200..6000) buried
   tall scenes: `native/src` lays out at 2836 x 3146 units. Fit is constant per
   scene, so nothing moves while you fly. Renders: `tall-{front,oblique}-ground.png`.
   Far-LOD backdrops are translucent, so the grid now shows through them.
   That is accurate.
7. `c8cf832` — selection tint, recolor-verb colours, and the remaining camera
   values (pitch limit, front/orbit depth range, orbit path, `--demo` camera)
   into config. The config guard caught an ordering bug: `parse_verb` runs inside
   clap, before the launch config is read. Verbs now resolve their default
   colour when applied.
8. `63eb453` — **transcript text palette (option A)**. 82 distinct colours
   become 15 neutral roles plus one accent per event kind. Colour means one
   thing, the kind, so a scroll down the deck reads as "a run of edits, then
   thinking". Section headings take the kind's accent. Titles stay white
   (coloured titles were tried: same-hue text on the banner lost contrast).
   Edit moved from amber to flame orange, because it sat 14 apart from
   thinking's gold. A test now keeps every pair of kind accents >= 45 apart.
   Renders: `transcript-deck-{before,after}.png`.

Still open from this round:

- **Mesh lighting** (light direction, 0.8 diffuse, 0.2 ambient) is in the
  fenced `mesh.wgsl`. Moving it to config needs a uniform added to that shader.
- **The greeking dials**, the frame-time cap (0.1 s), and setup's assumed
  1.6 aspect ratio are still compiled in. **Key bindings** are on hold until
  the omnibar / shortcut work.
- ~~Claude Code transcripts may misclassify beats.~~ **Fixed in `3391e82`.**
  - **Cause 1:** typed prompts are STRING content, and the parser only read
    arrays, so a whole session was one turn.
  - **Cause 2:** turns linearized grouped by kind instead of in transcript
    order.
  - Both are fixed, following the `-js` adapter's single file-order stream.
    The rules were cross-checked against claude-code-log, simonw's
    claude-code-transcripts, ccusage and the Agent SDK types.
  - Renders: `transcript-real-{before,after}.png`.
  - Seen in the survey but not handled yet, none of it urgent:
    - `compact_boundary` system records could mark turn breaks.
    - Read results of type `file_unchanged` carry no content.
    - Subagent transcripts now live in `<session>/subagents/` (we skip
      `isSidechain` lines, as the JS did).
    - Token usage repeats on every block line of one API message, so any
      future cost display must deduplicate on `message.id` + `requestId`.

## Step 2 — next, and golden-moving

- **The y = 0 convention**, by lifting the layout. Undecided between:
  - (a) each file stands on the floor (its own origin_y = its height);
  - (b) one whole-scene lift.

  Then the ground goes to y = 0 and `ground_gap` likely goes away.
- **A golden view with `--environment ground`**, plus a mutation proving it reddens
  (e.g. the depth compare flipped). The ground has no pixel coverage today.
- These belong in one re-baseline, adopted by hand, as agreed.

## Open threads

- **Ink colours are not yet settings.**
  - The syntax palette (`text.rs` `palette`) doubles as token-class markers in the
    lexer (`if wc != palette::DEFAULT`, :334/:361): a config that made two colours
    equal would change lexing. Separate class from colour first.
  - `layout::DEFAULT_COLOR_PACKED` lives inside the layout engine and is stamped
    into instance bytes by several strategies. The caller should pass it, rather
    than the engine reading app config.
  - Both are one "ink colours" change.
- **Missed by the sweep:** `target.rs` `SELECTION_TINT` (my colour grep matched
  `0.x` literals only). Also left compiled on purpose, and arguably tunable: the
  front/orbit near-far factors and the orbit path.
- **Colours in config are linear floats** (`#343a45` is `0.034, 0.042, 0.060`). sRGB
  hex strings would be friendlier to edit. Converting changes values, so it fits the
  step-2 re-baseline.
- **Key bindings** (B, G, H, …) are still in code.
- **Settings are read at launch.** Live reload of `launch_config.toml` would need
  `settings()` to stop being a `OnceLock`.
- **Pulling this branch deletes a tracked `launch_config.toml`** from any checkout
  (including the M2's). Copy `launch_config.example.toml` back to
  `launch_config.toml` after pulling.
- `AGENTS.md` still documents gates that `build.toml` does not run (engine-check,
  repo-verify, …). Known from the 2026-10 maintenance pass and not touched here.
- The `vulkan-nvidia` golden set needs re-adopting on this host (stale, and two
  views are missing). That is a human act. It would be the natural moment to do it
  alongside step 2.
