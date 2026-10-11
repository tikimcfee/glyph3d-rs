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
    /// note the drag; `apply_drag` moves it once per frame (from `animate`).
    /// `GLYPH_DRAG_PER_EVENT=1` applies every move as it arrives, as before
    /// 2026-10-10 — the A/B for the drag instrument.
    pub fn cursor_moved(&mut self, ctx: &GpuContext, x: f32, y: f32) {
        let prev = self.cursor;
        self.cursor = (x, y);
        if (x - prev.0).abs() + (y - prev.1).abs() < 1e-3 {
            return;
        }
        if self.grabbed_zone.is_none() && self.grabbed_group.is_none() {
            return;
        }
        self.drag_pending = Some(match self.drag_pending {
            Some((from, n)) => (from, n + 1),
            None => (prev, 1),
        });
        if std::env::var_os("GLYPH_DRAG_PER_EVENT").is_some() {
            self.apply_drag(ctx);
        }
    }

    /// Apply the pending grab drag: the cursor's whole path since the last
    /// frame as ONE move in the view plane through the grabbed thing's
    /// centre (two rays' hits on one plane: the sum of the per-event
    /// deltas, since every delta lies in that plane). `GLYPH_DRAG_TIMING=1`
    /// prints a `DRAGTIME` line per applied drag.
    pub(super) fn apply_drag(&mut self, ctx: &GpuContext) {
        let Some((prev, events)) = self.drag_pending.take() else { return };
        let to = self.cursor;
        let t0 = std::time::Instant::now();
        let mut stages = DragStages::default();
        let kind = if self.grabbed_zone.is_some() { "zone" } else { "file" };
        self.drag_from_to(ctx, prev, to, &mut stages);
        if std::env::var_os("GLYPH_DRAG_TIMING").is_some() {
            println!(
                "DRAGTIME kind={kind} events={events} groups={} move_ms={:.4} (propagate {:.4} extract {:.4}) sync_ms={:.4} upload_ms={:.4} seg_ms={:.4} total_ms={:.4}",
                stages.groups,
                stages.move_ms,
                stages.propagate_ms,
                stages.extract_ms,
                stages.sync_ms,
                stages.upload_ms,
                stages.seg_ms,
                t0.elapsed().as_secs_f64() * 1e3,
            );
        }
    }

    /// The controller's moved groups to the GPU: rows, then segments.
    fn sync_controller_groups(&mut self, ctx: &GpuContext, stages: &mut DragStages) {
        let Some(ctrl) = &mut self.controller else { return };
        let t = std::time::Instant::now();
        let updated_gids = ctrl.sync_gpu_groups(&mut self.groups_cpu);
        stages.sync_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        self.write_group_rows(ctx, &updated_gids);
        stages.upload_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        for &gid in &updated_gids {
            self.sync_segment(ctx, gid);
        }
        stages.seg_ms = t.elapsed().as_secs_f64() * 1e3;
        stages.groups = updated_gids.len();
        if let Some(ctrl) = &self.controller {
            stages.propagate_ms = ctrl.scene.last_update.propagate_ms;
            stages.extract_ms = ctrl.scene.last_update.extract_ms;
        }
    }

    fn drag_from_to(&mut self, ctx: &GpuContext, prev: (f32, f32), (x, y): (f32, f32), stages: &mut DragStages) {
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
            let t = std::time::Instant::now();
            let moved = self.controller.as_mut().is_some_and(|ctrl| ctrl.move_zone(&zid_str, d_vec));
            stages.move_ms = t.elapsed().as_secs_f64() * 1e3;
            if moved {
                self.sync_controller_groups(ctx, stages);
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
                let t = std::time::Instant::now();
                let local = card_local_delta(&ctrl.scene, entity, d_vec);
                if let Some(mut transform) = ctrl.scene.world.get_mut::<bevy_transform::components::Transform>(entity) {
                    transform.translation += local;
                }
                ctrl.scene.update_transforms();
                stages.move_ms = t.elapsed().as_secs_f64() * 1e3;
                self.sync_controller_groups(ctx, stages);
                return;
            }
        }
        // No controller entity for the group: the drag writes its node's
        // local row (the world delta in its parent's frame).
        if self.nodes.translate(gid, d_vec.to_array()) {
            self.group_node_edited(ctx, gid);
        }
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
                        self.sync_segment(ctx, gid);
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
                        self.sync_segment(ctx, g);
                    }
                    if let Some(g) = self.groups_cpu.get(gid as usize) {
                        s = g.cols[3][0];
                    }
                    println!("grab: group {gid} scale -> {s:.3}");
                    return;
                }
            }
            // No controller entity: the wheel scales the group's node.
            if let Some(ns) = self.nodes.scale_by(gid, f) {
                s = ns;
                self.group_node_edited(ctx, gid);
            }
            println!("grab: group {gid} scale -> {s:.3}");
        } else if matches!(self.camera_mode, CameraMode::Fly) {
            self.fly.on_scroll(lines);
        }
    }

    /// Windowed verb keys: h highlight line, g grab/release file, c grab/release carrel,
    /// t cycle tint, x toggle hidden, b toggle the ground/sky environment; in
    /// a library load ] [ (n p . , arrows) turn pages, Home/End jump, v
    /// toggles deck/splay.
    pub(super) fn verb_key(&mut self, ctx: &GpuContext, key: winit::keyboard::KeyCode) {
        use winit::keyboard::KeyCode as K;
        // A library load takes the deck keys for its volumes (the picked
        // file's, else every volume): the same verbs `--verb` scripts.
        if self.has_library() {
            use crate::library::{FormCmd, LibraryVerb, Page};
            let verb = match key {
                K::BracketRight | K::KeyN | K::ArrowRight | K::Period => Some(LibraryVerb::Page(Page::Next)),
                K::BracketLeft | K::KeyP | K::ArrowLeft | K::Comma => Some(LibraryVerb::Page(Page::Prev)),
                K::Home => Some(LibraryVerb::Page(Page::First)),
                K::End => Some(LibraryVerb::Page(Page::Last)),
                K::KeyV => Some(LibraryVerb::Form(FormCmd::Toggle)),
                _ => None,
            };
            if let Some(v) = verb {
                println!("{}", self.apply_library_verb(&v));
                return;
            }
        }
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
                            self.sync_segment(ctx, g);
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
                            self.sync_segment(ctx, g);
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
                                self.sync_segment(ctx, g);
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
                            self.sync_segment(ctx, g);
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
                            self.sync_segment(ctx, g);
                        }
                        println!("{msg}");
                    }
                }
            }
            _ => {}
        }
    }
}

/// What one applied drag cost (`GLYPH_DRAG_TIMING`).
#[derive(Default)]
struct DragStages {
    groups: usize,
    /// The controller's move: the transform write and `update_transforms`
    /// (layout systems, bevy propagation, scene-mesh re-extraction).
    move_ms: f64,
    /// `move_ms`'s bevy propagation and scene-mesh re-extraction.
    propagate_ms: f64,
    extract_ms: f64,
    sync_ms: f64,
    upload_ms: f64,
    seg_ms: f64,
}

/// A world-space drag delta in a file card's LOCAL frame: the inverse of its
/// parent's world transform applied to the vector (E10, 2026-10-10). A card
/// under a scaled parent — every library file sits under its page's mount,
/// which carries the contain-fit scale — moved `s` times the cursor when the
/// world delta was added to its local translation as is. A parent at the
/// identity (the shelf, the carrel) gets the delta back unchanged.
pub(crate) fn card_local_delta(scene: &crate::spatial_scene::SpatialScene, card: bevy_ecs::entity::Entity, world: Vec3) -> Vec3 {
    let parent = scene.world.get::<bevy_ecs::hierarchy::ChildOf>(card).map(|c| c.parent());
    match parent.and_then(|p| scene.world.get::<bevy_transform::components::GlobalTransform>(p)) {
        Some(g) => g.affine().inverse().transform_vector3(world),
        None => world,
    }
}

#[cfg(test)]
mod tests {
    use super::card_local_delta;
    use crate::spatial_scene::SpatialScene;
    use bevy_transform::components::{GlobalTransform, Transform};
    use glam::Vec3;

    /// E10 (2026-10-10): a `g` drag moves the file by the cursor's world
    /// delta whatever its parent's scale. A library file sits under its
    /// page's mount, which carries the contain-fit scale; adding the world
    /// delta to the card's LOCAL translation moved it `s` times the cursor.
    #[test]
    fn a_drag_under_a_scaled_parent_moves_the_card_by_the_world_delta() {
        let mut scene = SpatialScene::new();
        let root = scene.spawn_root("root");
        let mount = scene.spawn_child(
            root,
            Transform { translation: Vec3::new(10.0, -5.0, 2.0), scale: Vec3::splat(0.25), ..Transform::IDENTITY },
            "mount",
        );
        let card = scene.spawn_file_card(mount, "a.rs", "", 0, Transform::IDENTITY, ([0.0; 3], [1.0; 3]), [1.0; 3]);
        scene.update_transforms();
        let before = scene.world.get::<GlobalTransform>(card).unwrap().translation();
        let world = Vec3::new(3.0, -2.0, 0.5);
        let local = card_local_delta(&scene, card, world);
        scene.world.get_mut::<Transform>(card).unwrap().translation += local;
        scene.update_transforms();
        let moved = scene.world.get::<GlobalTransform>(card).unwrap().translation() - before;
        assert!((moved - world).length() < 1e-5, "the card moved {moved:?} for a world delta of {world:?} under a 0.25 mount");
        // Under an identity parent the delta is the world delta, unchanged.
        let flat = scene.spawn_file_card(root, "b.rs", "", 1, Transform::IDENTITY, ([0.0; 3], [1.0; 3]), [1.0; 3]);
        scene.update_transforms();
        assert_eq!(card_local_delta(&scene, flat, world), world);
    }
}
