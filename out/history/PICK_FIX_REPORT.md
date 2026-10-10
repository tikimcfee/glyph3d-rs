> **History.** Moved to `out/history/` on 2026-10-10: a dated record, not current state. What is true now: `README.md`, root `AGENTS.md`, `out/MAINTENANCE-NOTES-2026-10-08.md`.

# Pick fix — f32 view-proj inverse in `pixel_ray` (windowed picking)

**Symptom (windowed, glyph3d-js, 95M glyphs):** (1) close-up on a large file:
every click `pick: MISS`; (2) on a large single file, picks work until near
the END of the page, toggling as the camera distance changes; (3) flash
highlight offset from the clicked glyph by a systematic {+dx, −dy}.

## Root cause (measured, not guessed)

`pixel_ray` unprojected the click pixel by **inverting the f32 view-proj**
(`glyph_scene.rs`). The Fly camera uses `near=0.05`,
`far=(fit·50).max(20000)`:

- **Full-field fit** (glyph3d-js field 15098×22682 → fit≈33653 →
  far≈1.68e6): near/far ratio 3.0e-8 is **past f32 epsilon**. The inverted
  matrix unprojects the far-plane point to **w = 0.0 exactly**, so
  `pixel_ray` returned `None` for **every pixel** → every windowed click
  MISSed with "no file under the ray". *(= symptom 1.)*
- **Focused fit** (far=20000): the inverse survives but its angular error
  grows **linearly with distance** — measured end-to-end
  (`tools/repro_pick_oblique.py`, camera aimed at a known record of
  `dictgen-output/co.txt` row 1850, picking the viewport-center pixel):

  | camera | D=20 | D=50 | D=100 | D=500 | D=1000 | D=2000 |
  |---|---|---|---|---|---|---|
  | head-on err (world) | 0.26 | 0.66 | 0.72 | 17.8 | 35.6 | 30.5 |
  | yaw35°/pitch−20° err | 1.08 | 3.09 | miss | miss | 20.6 | miss |

  The 0.8-world-unit acceptance is crossed at **D≈50** head-on and at
  **D≈20** oblique; the error direction is a fixed screen-space offset
  *(= symptoms 2 and 3)*.
- A second, subtler instance of the same class: `Mat4::look_at_rh(eye,
  eye+fwd)` forms `eye−(eye+fwd)` in f32; with eye ≈ 1.2e4 world units the
  cancellation rounds the forward vector by up to ~1e-3 rad — the **render**
  camera itself was rotated away from `fly.forward()`, so any exact pick ray
  would still disagree with what is on screen at long distance.

Scripted head-on picks never saw this: the offscreen Front camera uses
`near=d·0.01, far=d·20` (ratio 2000, well conditioned).

## The fix (no epsilon hacks — the error is eliminated structurally)

- `pixel_ray` now builds the ray **analytically in f64** from the same
  camera basis the view matrix is built from (`right/up/back` +
  `tan(fov/2)` + aspect): there is no matrix inverse and no near/far
  conditioning at all. Ray origin = eye (not the near-plane point).
  `ray_file`/`ray_aabb`/plane intersection/`cursor_moved` drag math moved to
  f64 with it (record scan stays f32 — records are f32).
- Render path: Fly's view matrix uses `Mat4::look_to_rh(eye, fly.forward())`
  (direction, not a target point), removing the big-coordinate f32
  cancellation so render and pick agree. Front/Orbit (all byte-oracle
  screenshots) keep `look_at_rh` — their eye−target is exact.
- `camera_eye_target(t, aspect)` is now the single source both the render
  frame and the pick ray derive from (suspect (d): aspects/poses identical).
- New scripting: `--cam-pose X Y Z YAW PITCH` (degrees) interleaves with
  picks/verbs in the op stream (offscreen + windowed startup), and
  `GLYPH_PICK_DEBUG=1` logs ro/rd/t/q/nearest-cell/dist_world per pick.
  `tools/repro_pick_oblique.py` drives the aim-at-known-record round trip.

## After-numbers

- Focused regime, 21/21 scenarios (yaw {0,35,60}° × D {20…2000}) resolve the
  **exact** record; error ≤ **0.0008 world units** at every distance/angle
  (was: 0.26–46 world units, 1/21).
- Full-field regime (far≈1.68e6): 6/6 resolve (was: 6/6 MISS — ray=None).
- Off-center pixels (786,494), (802,501), grazing yaw 80°, steep pitch −80°,
  close-up D=5: all resolve the exact record (suspects (b)/(d): plane
  intersection and camera-frame inputs verified consistent).
- `tools/check-stage-g.sh`: **ALL PASS** (fixture oracle picks, pixel
  round trips, glyph3d-js oracle picks, fold cross-checks).
- `--repo-verify`: **PASS**, 96,860,762 records bit-exact (untouched path).
- Oracle screenshots **byte-identical**: zoom == `out/e2-file-zoom.png`,
  mid == `out/f-mid-before.png`, field == `out/f-field-after.png`
  (regenerated: `out/g-{zoom,mid,field}-after-fix.png`).
- `cargo build --release`: clean, 0 warnings.

## Windowed path

No unit/coordinate bug found (suspect (c)): `CursorMoved` positions,
`set_viewport`, and the surface config are all physical pixels end-to-end;
aspect sources matched. `windowed.rs` only gained the `Op::CamPose` arm.
The interactive bug was entirely the shared `pixel_ray`/`camera_frame` math
above, which the windowed Fly camera exercised and the offscreen Front
camera did not. LOD backdrop geometry was checked too (suspect (e)):
`sync_segment` re-derives the backdrop rect from the same local AABB the
pick table uses, so they coincide.
