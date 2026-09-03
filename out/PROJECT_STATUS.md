# PROJECT STATUS — regroup after Stages K + L (2026-09-03)

HEAD: `12437dd` (stage-l report). Eight-gate suite: **ALL GREEN**. Human
tested windowed incl. resize: no breaks detected. This is the shared map of
what happened, what it bought, and what's open — written so the next move is
a choice, not an archaeology dig.

---

## 1. What landed this session

### Stage K — the egui UI (10 code/report commits, `c99631f` → `f19cf8f`)

egui 0.36.1 overlay on the windowed renderer — version stars aligned exactly
(egui 0.36.1 pins wgpu 30.0 / winit 0.30.13, our exact stack). No eframe:
we keep our event loop, surface, and scene pass; egui is a second pass
(`LoadOp::Load`, no depth) on the same encoder.

- **K1** dep stack + plumbing (feature `egui-ui` default-on, `--no-ui` flag)
- **K2** input-consumption matrix (the real feature — typing in a text field
  can't fly the camera or fire h/g/t/x verbs; grab/ungrab interplay)
- **K3** Debug panel: FPS, camera, pick inspector, verb buttons wired through
  the *same* `parse_verb`/`apply_verb` the CLI uses
- **K4** live `LOD_MIN_PX` slider + cull counters (offscreen keeps consts —
  byte-equal PNGs are the proof). `BACKDROP_GAIN` *cut*: no gain uniform
  exists; alpha is baked at staging. Future seam documented.
- **K5** file browser: virtualized flat list, filter, click-to-fly
- **K6** in-window screenshot: `COPY_SRC` readback, F2 / `--screenshot-frame`
  — the composed frame (scene + UI), closing the pixel-verification gap
- **Fixes from the first human runs**: K1's "empty" CentralPanel was an opaque
  full-screen blanket (`ebaaec4`); `utc_stamp` mixed two civil-from-days
  variants (`b2070eb`).

### Stage L — the re_renderer structural borrow (`67e93d3` → `12437dd`)

Per decision record 10: borrow the *shape*, refuse the machinery.

- **L1** `FrameUniform` widening (104 B, `view_proj` pinned at 0..64, reserved
  `deterministic_rendering` flag) — **zero WGSL edits** (bigger buffer binds
  to the unchanged block)
- **L2** `enum Phase { Backdrop, Glyphs }` + per-phase draw lists built in
  `cull_segments` — where selection/labels/multi-view hook in
- **L3** pooled ping-pong view target + composite: `copy_texture_to_texture`
  on the oracle path (bit-exact by construction), new `composite.wgsl`
  fullscreen pass windowed; `--no-composite` proved neutrality, then removed
- **L4** selection mask pass: `Phase::Selection`, selected glyphs/segments
  render into a mask target and composite as a warm tint; click-flash hack
  REMOVED (pick selects, miss clears, persistent until next pick — Stage G's
  sticky-flash gap closed). The K6 pixel seam caught a real bug in minutes
  (`wgpu::Color::BLACK` is (0,0,0,1) — a "clear" that wrote opaque alpha).
  No re-baseline needed: offscreen has no mask path by construction.
- **O1** uncaptured-error dedup (ErrorTracker pattern) — **paid off within
  hours**, catching L3's missing-`COPY_DST` validation error cleanly.
  Owner-ratified semantics + loud `[GPU-ERROR]` marker (`2c60d84`).
- **O2** debug labels everywhere — GPU captures now read like a book
- **Post-L3 fix**: occlusion busy-spin (~100 % CPU while the window is
  occluded) throttled to a 2 Hz retry + parked WaitUntil (`c567d51`);
  live-verified self-recovery when the display woke

### Engine merge (parallel workstream, absorbed)

`a37a847` et al.: JS dependency gone (Python generators, vendored+hash-gated
inputs, `engine/` real directory), parallel driver off the private TaskGroup
API (**1.04× pipeline / 1.09× scan, bit-identical**), check-all grew to eight
gates (generator byte-identity + 16 Mojo conformance suites first). Their
conformance-matrix engagement assertions found 16/48 cells testing nothing.
We absorbed: `pixi install` + `build-engine`, all gates green, zero conflicts
with K/L. Full story: `out/ENGINE_TOOLCHAIN_REPORT.md`.

## 2. Verification posture (why "working" means something)

- Every commit: 8 gates green, four render views **byte-equal** vs baseline.
- Offscreen oracle is deterministic (fixed clock, CPU cull) and was never
  touched by K/L — pixel changes are structurally impossible there.
- Windowed visuals are now agent-verifiable via K6 (`--screenshot-frame`,
  F2) and were human-verified by the owner.
- 33+1 Rust tests: CLI parity, encase layout pins, naga WGSL validation
  (auto-covers `composite.wgsl`), ErrorTracker logic, utc_stamp epochs.

## 3. Open items — the menu

### Ready when you are

| Item | What | Cost |
|---|---|---|
| **BACKDROP_GAIN mini-stage** | store `ink_frac` at staging, apply live gain at backdrop-compaction (zero GPU cost). Needs A/B care — touches staging | mini-stage |
| **K5 follow-up** | exact-match pick variant so browser row-clicks can flash the right file (substring match could hit wrong file) | small |
| ~~Handoff 08 addendum~~ | DONE 2026-09-03 — L1–L4 completion addendum written and synced back to `glyph3d-integration-notes/` | — |

### Blocked / watchlist (do NOT start — triggers recorded in stage reports)

- **puffin_egui**: pinned to egui 0.33 upstream; re-check quarterly (note 05).
- **GPU cull + indirect draws**: waits on wgpu's Metal `first_instance` fix.
- **Descriptor-keyed pipeline pool**: triggers at ~8 pipelines; we're at ~5.
- **`CpuWriteGpuReadBelt`**: triggers if per-frame uploads hit MBs (now KBs).
- **wasm port**: audit says plausible (render path is engine-free); re-runs
  the device-tier refusal if greenlit.

## 4. Decisions — RESOLVED by the owner (2026-09-03)

1. **O1 semantics**: log-once-and-continue KEPT, with the loud greppable
   `[GPU-ERROR]` marker so a first occurrence can't hide in noise.
2. **Root `AGENTS.md` / `README.md`**: another agent's repo-mapping
   work-in-progress — left untracked; refine + commit at a natural point.
3. **Generated schema**: acknowledged — more engine changes coming
   (simplifications + correctness); schema edits via
   `schema/glyph-identity.json` + `gen_schema.py` only.

## 5. Where the frontier is now

Notes 07–10's structural program is **fully executed** (K ✓, L1–L4 ✓,
O1/O2 ✓). The frontier moves to:

- **Labels** (glyphon/cosmic-text, roadmap item 6) — hooks into L2's phase
  lists; the per-renderable text story is the next real design conversation.
- **Editing / resizable arena** (Stage G's named gap; its own stage).
- **BACKDROP_GAIN mini-stage** and the **K5 exact-pick follow-up** as
  smaller ready items.
- Engine-side: the parallel session's simplification/correctness work —
  absorb via the same `pixi install && pixi run build-engine && check-all`
  path as last time.

### Human-pass leftovers (organic testing covers most)

- L4: click→tint appears/persists/moves, click-empty clears, a g-dragged
  group's tint follows, profiler shows the two selection passes only while
  a selection exists.
- Occlusion fix: lid close/open, window fully covered — one log line,
  idle CPU, ≤0.5 s recovery.
- IME preedit with a CJK/dead-key input source (document-only quirks).
