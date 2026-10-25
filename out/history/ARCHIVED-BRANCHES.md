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
