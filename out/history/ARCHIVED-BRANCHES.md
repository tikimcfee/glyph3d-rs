# Archived branches

> History. Unmerged branches whose direction `main` did not take, kept as
> annotated tags so their commits and reports stay readable. Read one with
> `git show <tag>:<path>` or `git log <tag>`; nothing here is live code.

## `archive/multi-draw-indirect` (archived 2026-10-24)

Stage K as first run (2026-09-01), five commits on `main` of that day, plus
one archive commit carrying the worktree's untracked measurement outputs
(`out/tooling-ab/mdi/`, `out/tooling-ab/mdi-bench/`: the logs and PNGs behind
the report's numbers). Not the Stage K on `main`, which is the egui overlay
(`out/history/STAGE_K_REPORT.md`): two runs took the same letter.

**What it did.** A compute pass culled segments on the GPU and wrote draw
arguments for `multi_draw_indirect` (`--cull gpu`), or packed the visible
slots into one `draw_indirect` per chunk (`--cull gpu-packed`), with the CPU
cull kept as the reference twin (`--cull cpu`). Its report is
`out/STAGE_K_REPORT.md` at the tag. Measured then on the M2 (900-frame
medians of three, byte-identical renders): wide 314 fps vs CPU 226, mid 162
vs 159, zoom 229 vs 216; packed 361 on the all-backdrop view and ~12% slower
on the glyph-heavy one. It also found a hazard at arena chunks of 4 GiB and
more.

**Why it was not merged.** `main` kept the CPU cull with direct range draws
(about 1,300 AABB tests, microseconds), and the Visible field mode has since
moved per-frame layout onto the GPU by another route (`out/VISIBLE-MODE.md`).

**A finding that outlived it, unverified on today's code.** `main`'s header in
`native/src/glyph_scene.rs` (and the comment in `glyph_scene/setup.rs` on
`multi_draw_indirect`) says wgpu 30's Metal backend draws nothing for an
indirect draw with `first_instance != 0`. The branch's `mdi-probe` traced the
same symptom to the device never requesting
`Features::INDIRECT_FIRST_INSTANCE`. In that case wgpu's own indirect-validation
pass zeroes the offending draws in its copy of the arguments, so readbacks of
ours look correct and nothing draws. That would make the comment on `main` a
misdiagnosis, not a Metal defect. Board item C37 holds it until someone checks
it against wgpu 30 as pinned today.

| | |
|---|---|
| Tag | `archive/multi-draw-indirect` → `362094e` (base `9299916`) |
| Branch | `worktree-multi-draw-indirect`, deleted 2026-10-24 |
| Report | `git show archive/multi-draw-indirect:out/STAGE_K_REPORT.md` |

## `archive/egui-interface-shell` and `archive/egui-interface-m1` (archived 2026-11-01)

The egui window's interface shell, in two forms, both left in the second
clone (`viz-native/glyph3d-native`) when its `main` was merged here
(`8df9447`).

**`archive/egui-interface-shell`** → `6d0594b` (base `5d9a2f6`), branch
`worktree-agent-ac8b5f3d63d95903e`, 2026-10-24/25: nine commits that made
every action in the egui window a command LINE on one bus (`windowed/bus.rs`,
`exec.rs`), parsed by the CLI's own parser, with the panels moved into
`windowed/panels/`, a console (F9) and a Cmd-K palette. Its design note is
`out/INTERFACE-SHELL-2026-10-24.md` at the tag. The old clone's tag
`pre-rebase-interface` (`aa0d923`) is the same nine commits before their
rebase onto `5d9a2f6`. `git range-diff` shows only the `test_floor` numbers
and the z-dial ceiling change that the rebase absorbed.

**`archive/egui-interface-m1`** → `cf0f1d2` (base `3bf99f2`), branch
`worktree-interface`, 2026-09-02: "stage-m", the first form (a `ui::Command`
type, docked panels, inverse formatters). The shell above supersedes it by
its own account. Its notes are `integration/notes/11-interface-design.md`
and `out/STAGE_M_REPORT.md` at the tag.

**Why it was not merged.** Two days after the shell was written the
direction changed: makepad panels are to replace the winit window, the egui
overlay and the launcher TUI (`out/TEST-DIET-PLAN-2026-10-26.md`, premise 4).
The window's code was reshaped under the shell too (`SceneKey`, the relayout
rebuild moved into the engine, a scene that holds no device). Ivan's
decision (2026-11-01): archive it, and rebuild it in the makepad host, which
already has most of the panels. The spec for that rebuild is
`out/INTERFACE-SHELL-SPEC-2026-11-01.md`. The parts that do not belong to
the window were ported on 2026-11-01 (`worktree-stranded`):
`cli::apply_op`, the one op applier, now shared by offscreen, the window and
the makepad host; `cli::VERB_HEADS`; `--max-file-stacks`; and the window's
key-routing fix (a fly key released inside a focused field no longer
latches, and Cmd/Ctrl chords no longer fly).

| | |
|---|---|
| Tags | `archive/egui-interface-shell` → `6d0594b`; `archive/egui-interface-m1` → `cf0f1d2` |
| Branches | `worktree-agent-ac8b5f3d63d95903e` and `worktree-interface`, and the tag `pre-rebase-interface`, in the old clone |
| Design | `git show archive/egui-interface-shell:out/INTERFACE-SHELL-2026-10-24.md` |
| Rebuild spec | `out/INTERFACE-SHELL-SPEC-2026-11-01.md` |

## The second clone, retired (2026-11-01)

`~/localdev/viz-native/glyph3d-native` was a second checkout of this repo.
Unpushed work stranded there twice: C35/C36 (merged on 2026-10-31) and the
interface shell (above). Ivan retired it on 2026-11-01. Every ref it held
that no remote had is tagged here. Its gitignored local files and the notes
around it moved to `~/localdev/archive/viz-native-2026-11-01/`.

| Tag | Was | What |
|---|---|---|
| `archive/segment-entry-chain` → `7f0bc3e` | branch `exp/segment-entry-chain` | the retired CubeCL engine: a segment-entry chain for `apply_and_emit`, bit-exact, no measurable speedup; "kept for reference; not merged" |
| `archive/stash-2026-10-07-cubecl-prewarm` → `415144a` | stash on `perf/cubecl-prewarm-sram-sub100ms` | CubeCL prewarm WIP (engine retired 2026-10-09) |
| `archive/stash-2026-10-02-agent-books-handoff` → `ae072ee` | stash on `worktree-workspace-random-experiments` | "handoff for agent books" |
| `archive/stash-2026-10-02-random-experiments` → `502842c` | stash on `worktree-workspace-random-experiments` | WIP after the per-frame mesh re-upload fix |
| `archive/stash-2026-10-01-removed-mojo` → `f449f58` | stash on `main` | "removed mojo" |

A stash tag points at the stash's merge commit: `git show <tag>` shows the
working-tree diff, and `git show <tag>^2` the index.
