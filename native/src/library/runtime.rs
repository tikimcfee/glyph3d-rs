//! The library on the spatial scene: the plan's local positions become a
//! bevy_transform hierarchy — library root → directory → volume → sheet →
//! mount (the contain-fit scale) → file card (`GlyphGroupBinding`, identity)
//! — so a file's GroupRow is its flattened world transform, written by the
//! scene's existing `sync_to_group_rows`. Paging, form, stack and sort are
//! RETARGETS: the plan is recomputed, every node's easing starts from where
//! its transform actually is (mid-ease, or wherever a drag left it), and
//! `tick` eases dirs and sheets toward their new slots with the Book's
//! frame-rate-independent `1 − e^(−rate·dt)`. (The JS rebuilt its volume per
//! pass and seeded only the sheets' poses, so its directories jumped; here
//! directories glide too — the structure never changes, only targets.)

use std::collections::HashMap;
use std::time::Instant;

use bevy_ecs::prelude::*;
use bevy_transform::components::{GlobalTransform, Transform};
use glam::Vec3;

use super::book::{ease_k, ease_step};
use super::plan::{build_tree, plan, DirNode, Form, LibFile, Opts, Plan, Sort, Stack, VolumeState};
use crate::spatial_scene::{LayoutZoneEntity, LocalBounds, SpatialScene};

/// A paging request (`pageTo` / `scroll`: clamped at both ends, no wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Next,
    Prev,
    First,
    Last,
    To(usize),
}

/// A form request for the volumes in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormCmd {
    Set(Form),
    Toggle,
}

/// What one animation step cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct LibraryTick {
    /// Dir and sheet nodes whose transform was written this step.
    pub nodes_moved: usize,
    /// The easing loop alone.
    pub ease_ms: f64,
    /// Transform propagation + scene mesh re-extraction (`update_transforms`).
    pub propagate_ms: f64,
    pub still_animating: bool,
}

pub struct Library {
    pub opts: Opts,
    default_form: Form,
    lerp: f64,
    settle: f64,
    files: Vec<LibFile>,
    tree: DirNode,
    pub plan: Plan,
    volumes: HashMap<String, VolumeState>,
    pub root: Entity,
    pub dir_e: Vec<Entity>,
    pub sheet_e: Vec<Entity>,
    pub file_e: Vec<Entity>,
    pub face_count: usize,
    dir_at: Vec<[f64; 3]>,
    sheet_at: Vec<[f64; 3]>,
    animating: bool,
}

fn v3(p: [f64; 3]) -> Vec3 {
    Vec3::new(p[0] as f32, p[1] as f32, p[2] as f32)
}

impl Library {
    /// Spawn the library under a fresh root of `scene`, seated (no easing:
    /// a library is laid down settled, as the JS `seatAll` did). `tints` is
    /// each file's group tint; `dirs[i]` its directory (the zone key).
    pub fn build(
        scene: &mut SpatialScene,
        files: Vec<LibFile>,
        dirs: &[String],
        tints: &[[f32; 3]],
        settings: &crate::config::LibrarySettings,
    ) -> Self {
        let opts = Opts::from_settings(settings);
        let tree = build_tree(&files);
        let volumes = HashMap::new();
        let p = plan(&tree, &files, &opts, &volumes, settings.form);

        let root = scene.spawn_root("library_root");
        let mut dir_e = Vec::with_capacity(p.dirs.len());
        let mut vol_e: Vec<Option<Entity>> = Vec::with_capacity(p.dirs.len());
        for d in &p.dirs {
            let parent = d.parent.map_or(root, |i| dir_e[i]);
            let e = scene.spawn_child(parent, Transform::from_translation(v3(d.pos)), format!("lib:dir:{}", d.path));
            scene.world.entity_mut(e).insert((
                LayoutZoneEntity { zone_id: format!("dir:{}", d.path), title: d.name.clone() },
                LocalBounds {
                    min: [(-d.size[0] / 2.0) as f32, -d.size[1] as f32, -d.size[2] as f32],
                    max: [(d.size[0] / 2.0) as f32, 0.0, 0.0],
                },
            ));
            dir_e.push(e);
            vol_e.push((!d.books.is_empty()).then(|| {
                scene.spawn_child(e, Transform::from_translation(v3(d.volume_pos)), format!("lib:volume:{}", d.path))
            }));
        }
        let (pw, ph) = (opts.page_w as f32, opts.page_h as f32);
        let mut sheet_e = vec![Entity::PLACEHOLDER; files.len()];
        let mut file_e = vec![Entity::PLACEHOLDER; files.len()];
        let mut face_count = 0;
        for (i, (f, fp)) in files.iter().zip(&p.files).enumerate() {
            let vol = vol_e[fp.dir].expect("a directory with a file has a volume");
            let sheet = scene.spawn_child(vol, Transform::from_translation(v3(fp.slot)), format!("lib:sheet:{}", f.rel_path));
            let s = fp.fit.scale as f32;
            if settings.page_faces {
                // The page plane, a hair behind it (the JS face sat on the
                // sheet plane too). Content depth behind the plane — the wrap
                // staircase under depth_align "front", half of it under
                // "center" — is inside the book: the face, which writes
                // depth, hides it from the front.
                scene.spawn_plate(
                    sheet,
                    "lib:face",
                    Transform::from_xyz(0.0, 0.0, -settings.page_face_gap),
                    [pw, ph],
                    [-pw / 2.0, -ph / 2.0],
                    settings.page_face_color,
                );
                face_count += 1;
            }
            let mount = scene.spawn_child(
                sheet,
                Transform { translation: v3(fp.fit.mount), scale: Vec3::splat(s), ..Transform::IDENTITY },
                "lib:mount",
            );
            file_e[i] = scene.spawn_file_card(
                mount,
                f.rel_path.clone(),
                dirs[i].clone(),
                i as u32,
                Transform::IDENTITY,
                (f.ink_min, f.ink_max),
                tints[i],
            );
            sheet_e[i] = sheet;
        }
        let dir_at = p.dirs.iter().map(|d| d.pos).collect();
        let sheet_at = p.files.iter().map(|f| f.slot).collect();
        Library {
            opts,
            default_form: settings.form,
            lerp: f64::from(settings.lerp),
            settle: f64::from(settings.settle),
            files,
            tree,
            plan: p,
            volumes,
            root,
            dir_e,
            sheet_e,
            file_e,
            face_count,
            dir_at,
            sheet_at,
            animating: false,
        }
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// Directories that hold files (one volume each).
    pub fn volume_count(&self) -> usize {
        self.plan.dirs.iter().filter(|d| !d.books.is_empty()).count()
    }

    /// The zone key of every directory (`dir:<path>`) and its entity.
    pub fn zones(&self) -> impl Iterator<Item = (String, Entity)> + '_ {
        self.plan.dirs.iter().zip(&self.dir_e).map(|(d, &e)| (format!("dir:{}", d.path), e))
    }

    pub fn summary(&self) -> String {
        let mut sc: Vec<f64> = self.plan.files.iter().map(|f| f.fit.scale).collect();
        sc.sort_by(f64::total_cmp);
        let capped = sc.iter().filter(|&&s| s >= self.opts.max_upscale).count();
        let fits = if sc.is_empty() {
            String::new()
        } else {
            format!(
                " | fit scale min {:.4} median {:.4} max {:.4} ({} at max_upscale)",
                sc[0],
                sc[sc.len() / 2],
                sc[sc.len() - 1],
                capped
            )
        };
        format!(
            "library: {} files, {} dirs ({} volumes), {} page faces | stack {:?}, sort {:?}{}, page {}x{}{fits}",
            self.files.len(),
            self.plan.dirs.len(),
            self.volume_count(),
            self.face_count,
            self.opts.stack,
            self.opts.sort,
            if self.opts.reverse { " reversed" } else { "" },
            self.opts.page_w,
            self.opts.page_h,
        )
    }

    /// The world rect of file `f`'s page (sheet centre ± half a page), at
    /// the sheet's CURRENT transform.
    pub fn page_world_rect(&self, scene: &SpatialScene, f: usize) -> Option<([f32; 3], [f32; 3])> {
        let g = scene.world.get::<GlobalTransform>(self.sheet_e[f])?;
        let c = g.translation();
        let (hw, hh) = ((self.opts.page_w / 2.0) as f32, (self.opts.page_h / 2.0) as f32);
        Some(([c.x - hw, c.y - hh, c.z], [c.x + hw, c.y + hh, c.z]))
    }

    /// The volumes a verb addresses: the picked file's directory, else
    /// every directory with files.
    fn scope(&self, picked: Option<usize>) -> Vec<usize> {
        match picked.and_then(|f| self.plan.files.get(f)) {
            Some(fp) => vec![fp.dir],
            None => (0..self.plan.dirs.len()).filter(|&d| !self.plan.dirs[d].books.is_empty()).collect(),
        }
    }

    fn state_of(&self, d: usize) -> VolumeState {
        let dp = &self.plan.dirs[d];
        VolumeState { head: dp.head, form: dp.form }
    }

    /// Turn the volumes in scope. Only a `z` stack pages (the shelf and the
    /// pile show every book at once).
    pub fn page(&mut self, scene: &SpatialScene, picked: Option<usize>, cmd: Page) -> String {
        if self.opts.stack != Stack::Z {
            return format!("library: stack is {:?} — only a z stack (volumes) pages", self.opts.stack);
        }
        let scope = self.scope(picked);
        let mut turned = 0;
        for &d in &scope {
            let n = self.plan.dirs[d].books.len();
            let mut st = self.state_of(d);
            let head = match cmd {
                Page::Next => st.head + 1,
                Page::Prev => st.head.saturating_sub(1),
                Page::First => 0,
                Page::Last => n.saturating_sub(1),
                Page::To(i) => i,
            }
            .min(n.saturating_sub(1));
            if head != st.head {
                turned += 1;
            }
            st.head = head;
            self.volumes.insert(self.plan.dirs[d].path.clone(), st);
        }
        self.replan(scene);
        let one = (scope.len() == 1).then(|| {
            let d = &self.plan.dirs[scope[0]];
            format!(" — '{}' page {}/{}", d.path, d.head + 1, d.books.len())
        });
        format!("library: {cmd:?} turned {turned} of {} volume(s){}", scope.len(), one.unwrap_or_default())
    }

    /// Set or toggle the form of the volumes in scope (each toggles its own).
    pub fn form(&mut self, scene: &SpatialScene, picked: Option<usize>, cmd: FormCmd) -> String {
        if self.opts.stack != Stack::Z {
            return format!("library: stack is {:?} — forms are a z stack's (volumes)", self.opts.stack);
        }
        let scope = self.scope(picked);
        for &d in &scope {
            let mut st = self.state_of(d);
            st.form = match cmd {
                FormCmd::Set(f) => f,
                FormCmd::Toggle => match st.form {
                    Form::Deck => Form::Splay,
                    Form::Splay => Form::Deck,
                },
            };
            self.volumes.insert(self.plan.dirs[d].path.clone(), st);
        }
        if picked.is_none() {
            // A whole-library form change also becomes the default for the
            // volumes' next relayout (every volume is in `volumes` now anyway).
            if let FormCmd::Set(f) = cmd {
                self.default_form = f;
            }
        }
        self.replan(scene);
        format!("library: form {cmd:?} on {} volume(s)", scope.len())
    }

    pub fn set_stack(&mut self, scene: &SpatialScene, stack: Stack) -> String {
        self.opts.stack = stack;
        self.replan(scene);
        format!("library: stack {stack:?}")
    }

    pub fn set_sort(&mut self, scene: &SpatialScene, sort: Sort, reverse: bool) -> String {
        self.opts.sort = sort;
        self.opts.reverse = reverse;
        self.replan(scene);
        format!("library: sort {sort:?}{}", if reverse { " reversed" } else { "" })
    }

    /// Recompute every target; each node's ease starts from its live
    /// transform (mid-ease, or dragged), so nothing teleports.
    fn replan(&mut self, scene: &SpatialScene) {
        self.plan = plan(&self.tree, &self.files, &self.opts, &self.volumes, self.default_form);
        let live = |e: Entity| scene.world.get::<Transform>(e).map(|t| t.translation);
        for (at, &e) in self.dir_at.iter_mut().zip(&self.dir_e) {
            if let Some(t) = live(e) {
                *at = [f64::from(t.x), f64::from(t.y), f64::from(t.z)];
            }
        }
        for (at, &e) in self.sheet_at.iter_mut().zip(&self.sheet_e) {
            if let Some(t) = live(e) {
                *at = [f64::from(t.x), f64::from(t.y), f64::from(t.z)];
            }
        }
        self.animating = true;
    }

    /// One animation step of `dt` seconds. None when settled (nothing to
    /// do, nothing written); otherwise the transforms are written and
    /// propagated, and the caller syncs the changed group rows.
    pub fn tick(&mut self, scene: &mut SpatialScene, dt: f32) -> Option<LibraryTick> {
        if !self.animating {
            return None;
        }
        let t0 = Instant::now();
        let k = ease_k(self.lerp, f64::from(dt));
        let mut moved = 0usize;
        let mut pending = false;
        let mut step = |at: &mut [f64; 3], target: [f64; 3], e: Entity, world: &mut World| {
            let (next, did) = ease_step(*at, target, k, self.settle);
            if did {
                *at = next;
                if let Some(mut t) = world.get_mut::<Transform>(e) {
                    t.translation = v3(next);
                }
                moved += 1;
            }
            if next != target {
                pending = true;
            }
        };
        for (d, at) in self.dir_at.iter_mut().enumerate() {
            step(at, self.plan.dirs[d].pos, self.dir_e[d], &mut scene.world);
        }
        for (f, at) in self.sheet_at.iter_mut().enumerate() {
            step(at, self.plan.files[f].slot, self.sheet_e[f], &mut scene.world);
        }
        let ease_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t1 = Instant::now();
        if moved > 0 {
            scene.update_transforms();
        }
        let propagate_ms = t1.elapsed().as_secs_f64() * 1e3;
        self.animating = pending;
        Some(LibraryTick { nodes_moved: moved, ease_ms, propagate_ms, still_animating: pending })
    }
}

impl Opts {
    pub fn from_settings(s: &crate::config::LibrarySettings) -> Self {
        Opts {
            page_w: f64::from(s.page_w),
            page_h: f64::from(s.page_h),
            gap: f64::from(s.gap),
            stack: s.stack,
            sort: s.sort,
            reverse: s.reverse,
            max_upscale: f64::from(s.max_upscale),
            splay_cols: s.splay_cols,
            splay_gap_x: f64::from(s.splay_gap_x),
            splay_gap_y: f64::from(s.splay_gap_y),
            splay_aspect: f64::from(s.splay_aspect),
            splay_lift: f64::from(s.splay_lift),
            dir_gap: f64::from(s.dir_gap),
            depth_z: f64::from(s.depth_z),
            aspect: f64::from(s.aspect),
            depth_front: s.depth_align == super::DepthAlign::Front,
        }
    }
}
