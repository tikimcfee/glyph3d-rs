//! The windowed dev-only verification hooks (`GLYPH_*_SELFTEST`,
//! native/AGENTS.md § Debug env vars): each drives one panel or input path
//! through the window's own entry points at t≈3 s, with no human at the
//! mouse, and prints what it saw. Moved out of `WindowState::render`
//! (2026-10-24) — a pure move; `render` calls `run_selftests` where the
//! hooks used to sit, before the frame is acquired.

use crate::gpu::GpuContext;

use super::bus::lines;
use super::state::{hud_line, WindowState};

impl WindowState {
    pub(super) fn run_selftests(&mut self, ctx: &GpuContext) {
        // Stage K (K4) dev-only verification hook (GLYPH_K4_SELFTEST=1):
        // move the LOD slider programmatically once (t≈3 s) and log the cull
        // counters before/after — exercises the full panel → probe →
        // CullState → cull_segments write path without a human at the mouse.
        match self.k4_selftest {
            1 if self.time() > 3.0 => {
                if let Some(probe) = &self.ui_probe {
                    let mut p = probe.borrow_mut();
                    println!(
                        "K4SELFTEST before: lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                    // The panel slider's range max: at 16 px/em every visible
                    // fixture segment drops to its backdrop quad.
                    p.lod_min_px = 16.0;
                }
                self.k4_selftest = 2;
            }
            2 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "K4SELFTEST after:  lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                }
                self.k4_selftest = 0;
            }
            _ => {}
        }
        // Dev-only verification hook (GLYPH_ZSPACE_SELFTEST=1): drive the
        // layout dial programmatically — set the probe's z_wrap_spacing to
        // 2× its seed and push the SAME line the slider's drag-release
        // pushes (`--z-wrap-spacing X`, run after this render by the command
        // bus's drain in window_event's RedrawRequested arm). Logs the instance count (must NOT change —
        // z_step moves no slot counts) and the field's z extent (must
        // ~double) before/after the rebuild. States: 1 = fire at t>3 s;
        // 2 = one quiet frame (the rebuild happened after last frame's
        // render; THIS frame's scene render refreshes the new probe's
        // z_extent); 3 = print the after-readout, done. Skips when there is
        // no dial.
        match self.zspace_selftest {
            1 if self.time() > 3.0 => {
                let seed = self.ui_probe.as_ref().and_then(|p| p.borrow().z_wrap_spacing);
                match seed {
                    Some(seed) => {
                        let before_extent = self.ui_probe.as_ref().and_then(|p| p.borrow().z_extent);
                        println!(
                            "ZSPACE-SELFTEST before: z_wrap_spacing={seed:.2} instances={} z_extent={before_extent:?}",
                            self.scene.instance_count()
                        );
                        let new = (seed * 2.0).min(1.0);
                        if let Some(probe) = &self.ui_probe {
                            probe.borrow_mut().z_wrap_spacing = Some(new);
                        }
                        self.push_line(&lines::z_wrap_spacing(new));
                        self.zspace_selftest = 2;
                    }
                    None => {
                        println!("ZSPACE-SELFTEST: no layout dial (non-repo scene or --no-ui) — skipping");
                        self.zspace_selftest = 0;
                    }
                }
            }
            2 => self.zspace_selftest = 3,
            3 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "ZSPACE-SELFTEST after:  z_wrap_spacing={:?} instances={} z_extent={:?}",
                        p.z_wrap_spacing,
                        self.scene.instance_count(),
                        p.z_extent
                    );
                }
                self.zspace_selftest = 0;
            }
            _ => {}
        }
        // Dev-only verification hook (GLYPH_CLUSTER_SELFTEST=1): toggle the
        // cluster mode through the SAME command-bus line the panel's button
        // pushes (`--cluster-mode X`, the other mode). Meaningful only on cluster-bearing content (the
        // g-cluster-repo fixture, or a text scene like emoji-corpus-small.txt):
        // instance count must DROP as trailers leave the arena. States:
        // 1 = fire at t>3 s; 2 = one quiet frame; 3 = print the
        // after-readout, done.
        match self.cluster_selftest {
            1 if self.time() > 3.0 => {
                let has = self.ui_probe.as_ref().and_then(|p| p.borrow().cluster_mode);
                match has {
                    Some(on) => {
                        println!(
                            "CLUSTER-SELFTEST before: cluster_mode={on} instances={}",
                            self.scene.instance_count()
                        );
                        let current = if on {
                            crate::fold::ClusterMode::Cluster
                        } else {
                            crate::fold::ClusterMode::Leader
                        };
                        self.push_line(&lines::flag("cluster-mode", super::app::other_cluster_mode(current)));
                        self.cluster_selftest = 2;
                    }
                    None => {
                        println!("CLUSTER-SELFTEST: scene carries no cluster mode (no toggle) — skipping");
                        self.cluster_selftest = 0;
                    }
                }
            }
            2 => self.cluster_selftest = 3,
            3 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "CLUSTER-SELFTEST after:  cluster_mode={:?} instances={}",
                        p.cluster_mode,
                        self.scene.instance_count()
                    );
                }
                self.cluster_selftest = 0;
            }
            _ => {}
        }
        // Dev-only verification hook (GLYPH_FIELDMODE_SELFTEST=1): cycle the
        // field mode Instanced → Derived → Visible through the SAME
        // command-bus line the panel's selector pushes (`--field-mode X`),
        // printing the HUD line after each rebuild. Odd states push the line
        // (run after this render), even states are the quiet frame whose render fills
        // the new probe; the print reads that. Until the Visible field's
        // bodies land, the third step panics at `VisibleField::new` — which
        // is the point of running it: the mode reaches the call.
        {
            let step_mode = |s: u8| match s {
                1 => Some(glyph_field::GlyphFieldMode::Instanced),
                3 => Some(glyph_field::GlyphFieldMode::Derived),
                5 => Some(glyph_field::GlyphFieldMode::Visible),
                _ => None,
            };
            match self.fieldmode_selftest {
                s @ (1 | 3 | 5) if s > 1 || self.time() > 3.0 => {
                    let has_mode = self.ui_probe.as_ref().and_then(|p| p.borrow().field_mode);
                    match (has_mode, step_mode(s)) {
                        (Some(_), Some(mode)) => {
                            if let Some(probe) = &self.ui_probe {
                                println!("FIELDMODE-SELFTEST before step {}: {}", s.div_ceil(2), hud_line(&probe.borrow(), self.ui_fps));
                            }
                            self.push_line(&lines::flag("field-mode", mode));
                            self.fieldmode_selftest = s + 1;
                        }
                        _ => {
                            println!("FIELDMODE-SELFTEST: scene carries no field mode (no probe) — skipping");
                            self.fieldmode_selftest = 0;
                        }
                    }
                }
                s @ (2 | 4 | 6) => {
                    if let Some(probe) = &self.ui_probe {
                        println!("FIELDMODE-SELFTEST after step {}:  {}", s / 2, hud_line(&probe.borrow(), self.ui_fps));
                    }
                    self.fieldmode_selftest = if s == 6 { 0 } else { s + 1 };
                }
                _ => {}
            }
        }
        // Dev-only verification hook (GLYPH_VISIBLE_VERB_SELFTEST=1, M3): the
        // verbs through the SAME entry points the CLI op stream and the panel
        // use (`apply_pick`, `apply_verb` with `parse_verb`'s literals), one
        // step every fourth frame from t≈3 s so the HUD line printed after
        // each step reads a frame that saw the edit (the Visible field's
        // counters lag a frame or two). The value is the pick:
        // `1` = the first file's row 0 col 0; `file[:row[:col]]` otherwise.
        // Meant for `--field-mode visible` (the replies name item:byte); in a
        // stored mode the same steps run through the slot paths.
        if self.visible_verb_selftest != 0 && self.time() > 3.0 && self.frames_total.is_multiple_of(4) {
            let step = self.visible_verb_selftest;
            let hud = |state: &Self| state.ui_probe.as_ref().map(|p| hud_line(&p.borrow(), state.ui_fps)).unwrap_or_default();
            if step > 1 {
                println!("VISIBLE-VERB-SELFTEST hud after step {}: {}", step - 1, hud(self));
            }
            let verb_step = |state: &mut Self, spec: &str| {
                let verb = crate::parse_verb(spec).expect("selftest verb literal must parse like the CLI");
                let line = state.scene.apply_verb(ctx, &verb).unwrap_or_else(|| "scene does not support verbs".to_string());
                println!("VISIBLE-VERB-SELFTEST step {step} ({spec}): {line}");
            };
            match step {
                1 => {
                    let spec = std::env::var("GLYPH_VISIBLE_VERB_SELFTEST").unwrap_or_default();
                    let mut parts = spec.split(':');
                    let file = parts.next().filter(|f| *f != "1").unwrap_or("").to_string();
                    let row = parts.next().and_then(|r| r.parse().ok()).unwrap_or(0);
                    let col = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                    let cmd = crate::glyph_scene::PickCommand::RowCol { file, row, col };
                    let line = self.scene.apply_pick(ctx, &cmd).unwrap_or_else(|| "scene does not support picks".to_string());
                    println!("VISIBLE-VERB-SELFTEST step 1 (pick {cmd:?}): {line}");
                }
                2 => verb_step(self, "recolor-glyph"),
                3 => verb_step(self, "nudge-glyph 0.5 0 0"),
                4 => verb_step(self, "set-glyph-background 2050c0"),
                5 => verb_step(self, "hide-group"),
                6 => verb_step(self, "show-group"),
                _ => {}
            }
            self.visible_verb_selftest = if step >= 7 { 0 } else { step + 1 };
        }
        // Dev-only verification hook (GLYPH_GRAB_SELFTEST=file[:dx:dy], C29,
        // 2026-10-10): the windowed GRAB path, through the SAME entry points
        // the window uses — `apply_pick` on `file`, `on_cursor` to park the
        // cursor at the centre, `on_key(G)` to grab, ten `on_cursor` steps
        // along (dx, dy) px (default 0, 400: straight down), `on_key(G)` to
        // release — one step every fourth frame from t≈3 s, the HUD line
        // (items and lines per tier) printed after each, and the group's
        // offset at the end (`move-group 0 0 0`). Meant for `--field-mode
        // visible` with a `--cam-pose` that puts the drag where you want:
        // an item still in view after the drag must stay counted visible.
        if self.grab_selftest != 0 && self.time() > 3.0 && self.frames_total.is_multiple_of(4) {
            let step = self.grab_selftest;
            let hud = |state: &Self| state.ui_probe.as_ref().map(|p| hud_line(&p.borrow(), state.ui_fps)).unwrap_or_default();
            let spec = std::env::var("GLYPH_GRAB_SELFTEST").unwrap_or_default();
            let mut parts = spec.split(':');
            let file = parts.next().filter(|f| *f != "1").unwrap_or("").to_string();
            let dx: f32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let dy: f32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(400.0);
            let (cx, cy) = (self.config.width as f32 * 0.5, self.config.height as f32 * 0.5);
            const DRAG_STEPS: u8 = 10;
            match step {
                1 => {
                    let cmd = crate::glyph_scene::PickCommand::RowCol { file, row: 0, col: 0 };
                    let line = self.scene.apply_pick(ctx, &cmd).unwrap_or_else(|| "scene does not support picks".to_string());
                    println!("GRAB-SELFTEST step 1 (pick {cmd:?}): {line}");
                    // Park the cursor before grabbing: a drag's delta is
                    // measured from wherever the cursor last was.
                    self.scene.on_cursor(ctx, cx, cy);
                }
                2 => {
                    self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyG, true);
                    println!("GRAB-SELFTEST step 2 (g, grabbed): hud {}", hud(self));
                }
                s if s < 3 + DRAG_STEPS => {
                    let k = f32::from(s - 2) / f32::from(DRAG_STEPS);
                    self.scene.on_cursor(ctx, cx + dx * k, cy + dy * k);
                    println!("GRAB-SELFTEST step {s} (drag to {:+.0},{:+.0} px): hud {}", dx * k, dy * k, hud(self));
                }
                s if s == 3 + DRAG_STEPS => {
                    self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyG, true);
                    let verb = crate::parse_verb("move-group 0 0 0").expect("selftest verb literal must parse like the CLI");
                    let line = self.scene.apply_verb(ctx, &verb).unwrap_or_default();
                    println!("GRAB-SELFTEST step {s} (g, released): {line}");
                }
                _ => {
                    println!("GRAB-SELFTEST hud after release: {}", hud(self));
                }
            }
            self.grab_selftest = if step >= 4 + DRAG_STEPS { 0 } else { step + 1 };
        }
    }
}
