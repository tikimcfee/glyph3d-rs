use glam::{DVec3, Vec3};
use super::{CameraMode, GlyphScene, pick::{PickCommand, Verb}};
use crate::gpu::GpuContext;

impl GlyphScene {
    pub fn set_cam_pose(&mut self, eye: [f32; 3], yaw: f32, pitch: f32) {
        self.camera_mode = CameraMode::Fly;
        self.fly.eye = Vec3::new(eye[0], eye[1], eye[2]);
        self.fly.yaw = yaw;
        self.fly.pitch = pitch;
        self.fly.vel = Vec3::ZERO;
        self.fly.keys = 0;
        log::info!(
            "cam pose: eye=({:.2},{:.2},{:.2}) yaw={yaw:.4} pitch={pitch:.4} (Fly)",
            eye[0],
            eye[1],
            eye[2]
        );
    }

    /// Windowed click: pick at the pixel and return the log line. Stage L
    /// (L4): the click-flash hack is gone — `apply_pick` now drives the
    /// selection mask instead (no instance-byte writes, nothing to restore).
    pub fn click_pick(&mut self, ctx: &GpuContext, x: f32, y: f32) -> Option<String> {
        self.apply_pick(ctx, &PickCommand::Pixel { x, y })
    }

    /// Windowed cursor move: while a group or carrel is grabbed (`g` or `c`),
    /// drag it in the view plane through its center.
    pub fn cursor_moved(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        let prev = self.cursor;
        self.cursor = (x, y);
        if (x - prev.0).abs() + (y - prev.1).abs() < 1e-3 {
            return;
        }

        // Branch 1: Dragging an entire Carrel / LayoutZone (`KeyC`)
        if let Some(ref zid) = self.grabbed_zone {
            let (Some((o0, d0)), Some((o1, d1))) =
                (self.pixel_ray(prev.0, prev.1), self.pixel_ray(x, y))
            else {
                return;
            };
            let (w, h) = self.viewport.get();
            let Some((_, fwd)) = self.pixel_ray(w as f32 * 0.5, h as f32 * 0.5) else {
                return;
            };

            let zone_center = self
                .controller
                .as_ref()
                .and_then(|c| c.zone_world_translation(zid))
                .map(|t| DVec3::new(t[0] as f64, t[1] as f64, t[2] as f64));

            let Some(c) = zone_center else {
                self.grabbed_zone = None;
                return;
            };

            let hit_plane = |o: DVec3, d: DVec3| -> Option<DVec3> {
                let denom = d.dot(fwd);
                if denom.abs() < 1e-12 {
                    None
                } else {
                    Some(o + d * ((c - o).dot(fwd) / denom))
                }
            };
            let (Some(p0), Some(p1)) = (hit_plane(o0, d0), hit_plane(o1, d1)) else {
                return;
            };
            let delta = p1 - p0;
            let d_vec = glam::Vec3::new(delta.x as f32, delta.y as f32, delta.z as f32);

            let zid_str = zid.clone();
            if let Some(ctrl) = &mut self.controller {
                if ctrl.move_zone(&zid_str, d_vec) {
                    let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                    self.write_group_rows(ctx, &updated_gids);
                    for &gid in &updated_gids {
                        self.sync_segment(gid);
                    }
                }
            }
            return;
        }

        // Branch 2: Dragging an individual file group (`KeyG`)
        let Some(gid) = self.grabbed_group else { return };
        let (Some((o0, d0)), Some((o1, d1))) =
            (self.pixel_ray(prev.0, prev.1), self.pixel_ray(x, y))
        else {
            return;
        };
        let (w, h) = self.viewport.get();
        let Some((_, fwd)) = self.pixel_ray(w as f32 * 0.5, h as f32 * 0.5) else {
            return;
        };
        let Some((off, sc, _, _)) = self.group_trs(gid) else {
            return;
        };
        let center_local = self
            .pick
            .as_ref()
            .and_then(|p| p.files.iter().find(|f| f.group_id == gid))
            .map(|i| {
                [
                    (i.aabb_min[0] + i.aabb_max[0]) * 0.5,
                    (i.aabb_min[1] + i.aabb_max[1]) * 0.5,
                    (i.aabb_min[2] + i.aabb_max[2]) * 0.5,
                ]
            });
        let Some(cl) = center_local else {
            self.grabbed_group = None;
            return;
        };
        let c = DVec3::new(
            cl[0] as f64 * sc.x as f64 + off.x as f64,
            cl[1] as f64 * sc.y as f64 + off.y as f64,
            cl[2] as f64 * sc.z as f64 + off.z as f64,
        );
        let hit_plane = |o: DVec3, d: DVec3| -> Option<DVec3> {
            let denom = d.dot(fwd);
            if denom.abs() < 1e-12 {
                None
            } else {
                Some(o + d * ((c - o).dot(fwd) / denom))
            }
        };
        let (Some(p0), Some(p1)) = (hit_plane(o0, d0), hit_plane(o1, d1)) else {
            return;
        };
        let delta = p1 - p0;
        let d_vec = glam::Vec3::new(delta.x as f32, delta.y as f32, delta.z as f32);

        if let Some(ctrl) = &mut self.controller {
            if let Some(&entity) = ctrl.file_entities.get(gid as usize) {
                if let Some(mut transform) = ctrl.scene.world.get_mut::<bevy_transform::components::Transform>(entity) {
                    transform.translation += d_vec;
                }
                ctrl.scene.update_transforms();
                let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                self.write_group_rows(ctx, &updated_gids);
                for &g in &updated_gids {
                    self.sync_segment(g);
                }
                return;
            } else if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
                g.cols[0][0] += delta.x as f32;
                g.cols[0][1] += delta.y as f32;
                g.cols[0][2] += delta.z as f32;
            }
        } else if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
            g.cols[0][0] += delta.x as f32;
            g.cols[0][1] += delta.y as f32;
            g.cols[0][2] += delta.z as f32;
        }

        self.write_group_row(ctx, gid);
        self.sync_segment(gid);
    }

    /// Windowed scroll: scales the grabbed carrel or file; otherwise camera speed.
    pub(super) fn scroll_or_scale(&mut self, ctx: &GpuContext, lines: f32) {
        if let Some(ref zid) = self.grabbed_zone {
            let f = 1.1f32.powf(lines);
            let zid_str = zid.clone();
            if let Some(ctrl) = &mut self.controller {
                if ctrl.scale_zone(&zid_str, f) {
                    let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                    self.write_group_rows(ctx, &updated_gids);
                    for &gid in &updated_gids {
                        self.sync_segment(gid);
                    }
                    println!("grab carrel: zone '{zid_str}' scaled by factor {f:.3}");
                }
            }
        } else if let Some(gid) = self.grabbed_group {
            let f = 1.1f32.powf(lines);
            let mut s = 0.0;
            if let Some(ctrl) = &mut self.controller {
                if let Some(&entity) = ctrl.file_entities.get(gid as usize) {
                    if let Some(mut transform) = ctrl.scene.world.get_mut::<bevy_transform::components::Transform>(entity) {
                        transform.scale *= f;
                    }
                    ctrl.scene.update_transforms();
                    let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                    self.write_group_rows(ctx, &updated_gids);
                    for &g in &updated_gids {
                        self.sync_segment(g);
                    }
                    if let Some(g) = self.groups_cpu.get(gid as usize) {
                        s = g.cols[3][0];
                    }
                    println!("grab: group {gid} scale -> {s:.3}");
                    return;
                } else if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
                    for c in 0..3 {
                        g.cols[3][c] = (g.cols[3][c] * f).clamp(0.001, 100.0);
                    }
                    s = g.cols[3][0];
                }
            } else if let Some(g) = self.groups_cpu.get_mut(gid as usize) {
                for c in 0..3 {
                    g.cols[3][c] = (g.cols[3][c] * f).clamp(0.001, 100.0);
                }
                s = g.cols[3][0];
            }
            self.write_group_row(ctx, gid);
            self.sync_segment(gid);
            println!("grab: group {gid} scale -> {s:.3}");
        } else if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_scroll(lines);
        }
    }

    /// Windowed verb keys: h highlight line, g grab/release file, c grab/release carrel,
    /// t cycle tint, x toggle hidden, b toggle the ground/sky environment.
    pub(super) fn verb_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode) {
        use winit::keyboard::KeyCode as K;
        match key {
            K::KeyH => {
                let line = self.apply_verb(ctx, &Verb::RecolorLine(None));
                println!("{line}");
            }
            K::KeyT => {
                let line = self.apply_verb(ctx, &Verb::TintCycle);
                println!("{line}");
            }
            K::KeyX => {
                let line = self.apply_verb(ctx, &Verb::ToggleHidden);
                println!("{line}");
            }
            K::KeyB => {
                let mode = self.environment.toggle();
                println!("environment: {mode:?}");
            }
            K::KeyG => {
                self.grabbed_zone = None;
                match self.grabbed_group {
                    Some(gid) => {
                        self.grabbed_group = None;
                        println!("grab: released group {gid}");
                    }
                    None => match &self.picked {
                        Some(h) => {
                            self.grabbed_group = Some(h.group_id);
                            println!(
                                "grab: {} (group {}) — mouse drags it in the view plane, scroll scales, g releases",
                                h.rel_path, h.group_id
                            );
                        }
                        None => println!("grab: nothing picked (click a file first)"),
                    },
                }
            }
            K::KeyC => {
                self.grabbed_group = None;
                match &self.grabbed_zone {
                    Some(zid) => {
                        let zid_clone = zid.clone();
                        self.grabbed_zone = None;
                        println!("grab carrel: released zone '{zid_clone}'");
                    }
                    None => match &self.picked {
                        Some(h) => {
                            let zid = if let Some(ctrl) = &self.controller {
                                let dir = h.rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                                ctrl.zone_for_file(&h.rel_path, dir)
                            } else {
                                format!("group:{}", h.group_id)
                            };
                            println!(
                                "grab carrel: zone '{zid}' (via {}) — mouse drags the entire carrel in 3D space, scroll scales, c releases",
                                h.rel_path
                            );
                            self.grabbed_zone = Some(zid);
                        }
                        None => println!("grab carrel: nothing picked (click a file first)"),
                    },
                }
            }
            K::BracketRight | K::KeyN | K::ArrowRight | K::Period => {
                if let Some(ctrl) = &mut self.controller {
                    if let Some(msg) = ctrl.carrel_next().or_else(|| ctrl.deck_next()) {
                        let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                        self.write_group_rows(ctx, &updated_gids);
                        for &g in &updated_gids {
                            self.sync_segment(g);
                        }
                        println!("{msg}");
                    } else {
                        println!("deck: reached end of deck (or no active deck)");
                    }
                }
            }
            K::BracketLeft | K::KeyP | K::ArrowLeft | K::Comma => {
                if let Some(ctrl) = &mut self.controller {
                    if let Some(msg) = ctrl.carrel_prev().or_else(|| ctrl.deck_prev()) {
                        let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                        self.write_group_rows(ctx, &updated_gids);
                        for &g in &updated_gids {
                            self.sync_segment(g);
                        }
                        println!("{msg}");
                    } else {
                        println!("deck: reached beginning of deck (or no active deck)");
                    }
                }
            }
            K::Home => {
                if let Some(ctrl) = &mut self.controller {
                    let total = ctrl
                        .session
                        .as_ref()
                        .map(|s| s.linearize_events(ctrl.revision_engine.as_ref()).len().max(s.turn_count()))
                        .unwrap_or(0);
                    if total > 0 {
                        if let Some(msg) = ctrl.carrel_set_beat(total - 1) {
                            let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                            self.write_group_rows(ctx, &updated_gids);
                            for &g in &updated_gids {
                                self.sync_segment(g);
                            }
                            println!("{msg}");
                        }
                    }
                }
            }
            K::End => {
                if let Some(ctrl) = &mut self.controller {
                    if let Some(msg) = ctrl.carrel_set_beat(0) {
                        let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                        self.write_group_rows(ctx, &updated_gids);
                        for &g in &updated_gids {
                            self.sync_segment(g);
                        }
                        println!("{msg}");
                    }
                }
            }
            K::KeyV => {
                if let Some(ctrl) = &mut self.controller {
                    if let Some(msg) = ctrl.deck_toggle_mode() {
                        let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
                        self.write_group_rows(ctx, &updated_gids);
                        for &g in &updated_gids {
                            self.sync_segment(g);
                        }
                        println!("{msg}");
                    }
                }
            }
            _ => {}
        }
    }
}
