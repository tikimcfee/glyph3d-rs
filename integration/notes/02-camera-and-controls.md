# 02 — Camera controls: what's adoptable, what's pattern-only

*Research date 2026-09-01, verified against crates.io manifests/releases that day.
Companion: `00-primitives-inventory.md` (§FlyCamera).*

## TL;DR

**There is no maintained, glam-0.30/wgpu-30-ready standalone camera crate.** Your
~110-line `FlyCamera` is not tech debt by ecosystem standards — the ecosystem has no
better drop-in. What the net *does* give you: one **conditional adopt**
(`trackball` if/when glam bumps), a set of **portable patterns** (dolly's driver
decomposition; smooth-bevy-cameras' damping; Bevy 0.18's built-in FreeCamera input
settings), and confirmation your pixel-ray picking needs no external crate.

## The landscape

| crate | version / date | status | stack fit (wgpu 30 / winit 0.30 / glam 0.30) | verdict |
|---|---|---|---|---|
| **trackball** | 0.18.0, 2026-06-22 | active (qu1x) | engine-agnostic orbit math (exp-map); optional `glam ^0.32` + non-optional nalgebra core | **reference now; ADOPT if glam bumps** — the only real standalone orbit-math crate |
| **dolly** | 0.6.0, 2024-07-22 | untouched since 2024, glam `<=0.28` | — | **reference-only** — best *design* for a composable camera rig (drivers: position / yaw-pitch / smoothing / clamp) |
| three-d controls | 0.19.0, 2026-04-17 | active | OpenGL (glow) + cgmath + winit 0.28; **no wgpu backend exists** | reference-only (event-enum decoupling pattern) |
| bevy_flycam | 0.19.0, 2026-07-17 | active | bevy ^0.19-locked | reference-only |
| bevy_spectator | 0.8.0, 2025-05-05 | stale (bevy now 0.19) | bevy-locked | reference-only |
| smooth-bevy-cameras | 0.14.0, 2025-05-03 | stale | bevy-locked | **reference-only but its exp-smoothing is the ecosystem's best "camera feel"** |
| bevy_pancam / bevy_rts_camera / bevy_map_camera / bevy_infinite_grid | 2026 releases | active | bevy-locked | reference-only |
| **Bevy 0.18+ built-in FreeCamera** | 0.18, late 2025 | upstream | bevy-locked | pattern: smoothing/sensitivity/speed-step-on-scroll input settings — closely matches your hand-rolled model |
| cam-geom | 0.17.0, 2026-08-07 (strawlab) | active | nalgebra ^0.35, pure math | reference-only — only pays off for calibrated/distorted lenses (OpenCV models) |
| camera_controllers (Piston) | 0.36.0, 2025-12-06 | Piston input stack | — | reject |
| "cameras" 0.3.2 | 2026-05-16 | **video-capture crate — name trap** | — | reject |
| arcball / bevy_prank / easer / interpolation / keyframe / easing | 2017–2023 | dead/stale | — | reject |
| parry3d | 0.30.2, 2026-08-08 (dimforge) | active | nalgebra, heavy | reject for picking — your inverse-VP ray + ray-AABB is smaller and already oracle-gated |
| tween | 2.2.0, 2026-02-24 | active | optional `glam ^0.32` | conditional adopt if glam bumps (tweened camera moves) |
| one-euro | 0.8.0, 2025-08-05 | fine | nalgebra | overkill for WASD+deltas |

Non-existent (checked, crates.io 404): `bevy_free_cam`, `bevy_look_at`, `target_camera`,
`smoothstep`, `soft-camera`.

## What this means for glyph3d-native

1. **Keep `FlyCamera` as the ownership point.** It already implements the ecosystem norm
   (exponential damping `1 - exp(-k·dt)`, scroll speed multiplier, dt clamp at 0.1).
   If you want better *feel*, port two patterns rather than crates:
   - **dolly's decomposition**: separate the *rig* (eye frame) from *drivers* (input
     velocity, smoothing, look-at lock). Pays off the day you add Orbit/Front-mode
     tweening or a "focus picked file" cinematic move.
   - **Bevy 0.18 FreeCamera settings model**: user-tunable smoothing/sensitivity/speed
     step — you have speed-on-scroll already; smoothing toggle is a small add.
2. **Orbit math**: if you want a proper inertial trackball for the Front/Orbit modes,
   `trackball` is the only maintained standalone option — it flips to adoptable when you
   bump glam 0.30 → 0.32+ (see `03` note: same lever unlocks more crates).
3. **Picking needs nothing.** `pixel_ray` (inverse view-proj) + `ray_aabb` is the
   standard idiom; no crate improves it at your scale. Revisit parry3d only if picking
   evolves into general collision queries.
4. **glam is the strategic lever, again**: 0.33.6 is current (2026-08-28); your 0.30 pin
   is what makes trackball/tween reference-only. Mechanical migration, ~2 minors.

## The Blender-feel grab problem (flagged by the team as critically important)

Current Stage G grab drags the picked file **through its AABB center** in the view
plane. The established 3D-editor techniques to get "natural," in rough order of payoff:

1. **Grab at the hit point, not the center.** Anchor the drag at the exact picked
   world point; per frame, solve the delta that keeps that point under the cursor
   (ray ∋ cursor ∩ grab plane). This alone kills most of the "it jumps to my hand"
   feel. The pick cache already carries the exact hit.
2. **Constraint switching.** Default: view-plane drag. Modifier: ground-plane
   (y-locked), or axis-dominant (project onto the axis whose screen-space projection
   best aligns with mouse travel). Blender/Unity/Godot all converge on this.
3. **Screen-space manipulator handles.** Render axis arrows/planes at the group
   origin (projected to screen space, hit-tested in 2D, mapped back to 3D
   constraints). **transform-gizmo 0.11.0** (2026-08-17, active; egui 0.36 + glam
   ^0.32) implements exactly this and becomes directly adoptable once egui is in AND
   glam is bumped — the egui overlay pass is where the handles draw.
4. **Depth legibility.** A ground-shadow or axis-colored offset line while dragging so
   the eye can judge z; cheap (one extra quad stream, could ride the backdrop
   pipeline's shape).

Note the fly camera's exp damping already matches ecosystem norms; the *manipulation*
layer is where the feel work actually is.

*Sources: crates.io API + dependency manifests (versions/dates as quoted),
github.com/qu1x/trackball, github.com/h3r2tic/dolly,
docs.rs/three-d (renderer::control), bevy.org 0.18 release notes + Free camera example,
docs.rs/cam-geom, docs.rs/parry3d, lib.rs sweeps 2026-09.*
