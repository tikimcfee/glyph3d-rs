//! The library on the renderer's transform tree (`glyph-scene-graph`; since
//! 2026-10-10 — before, a bevy_transform hierarchy flattened on the CPU):
//! library root → directory → volume → sheet → mount (the contain-fit
//! scale) → file. The file node IS the file's group node: it routes its
//! resolved world into its group row, so the GPU resolve writes the row the
//! draw shaders read, and a move writes only the LOCAL rows of the nodes
//! that moved — a page turn its sheets, a directory drag one row — never a
//! row per descendant. Paging, form, stack and sort are RETARGETS: the plan
//! is recomputed, every node's easing starts from where its local row
//! actually is (mid-ease, or wherever a drag left it), and `tick` eases dirs
//! and sheets toward their new slots with the Book's frame-rate-independent
//! `1 − e^(−rate·dt)`. (The JS rebuilt its volume per pass and seeded only
//! the sheets' poses, so its directories jumped; here directories glide too
//! — the structure never changes, only targets.)
//!
//! The rows come out bit for bit as bevy's did: the resolve composes
//! top-down, summing each translation root first exactly as bevy's
//! propagation does, and every step of the chain is a product with 1 or a
//! sum (`tests::the_tree_resolves_the_rows_bevy_composed`).

use std::collections::HashMap;
use std::time::Instant;

use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

use super::book::{ease_k, ease_step};
use super::plan::{build_tree, plan, DirNode, Form, LibFile, Opts, Plan, Sort, Stack, VolumeState};

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

/// What one animation step did and cost.
#[derive(Clone, Debug, Default)]
pub struct LibraryTick {
    /// Dir and sheet nodes whose local row was written this step.
    pub nodes_moved: usize,
    /// Those nodes (their subtrees are what moved).
    pub moved: Vec<NodeHandle>,
    /// The easing loop alone (local-row writes included).
    pub ease_ms: f64,
    pub still_animating: bool,
}

/// The library's nodes in a [`NodeTables`] (`Library::insert_nodes`).
#[derive(Clone, Debug)]
pub struct LibNodes {
    pub root: NodeHandle,
    /// Per `Plan::dirs` entry.
    pub dir: Vec<NodeHandle>,
    /// Per `Plan::dirs` entry: None for a directory without files.
    pub volume: Vec<Option<NodeHandle>>,
    /// Per file (load order): its sheet, its mount, and the file node.
    pub sheet: Vec<NodeHandle>,
    pub mount: Vec<NodeHandle>,
    pub file: Vec<NodeHandle>,
}

/// The page face behind every sheet: a quad `size` wide centred on the
/// sheet, `gap` behind its plane (`[library] page_faces`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceSpec {
    pub size: [f32; 2],
    pub gap: f32,
    pub color: [f32; 4],
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
    /// `dir:<path>` → index into `plan.dirs` (the zone a `c` grab names).
    zone_dirs: HashMap<String, usize>,
    /// The page face, when `[library] page_faces` is on.
    pub face: Option<FaceSpec>,
    /// The nodes in the scene's tables, once attached (`attach`).
    pub nodes: Option<LibNodes>,
    dir_at: Vec<[f64; 3]>,
    sheet_at: Vec<[f64; 3]>,
    animating: bool,
}

fn v3(p: [f64; 3]) -> [f32; 3] {
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

impl Library {
    /// Plan the library, seated (no easing: a library is laid down settled,
    /// as the JS `seatAll` did). Nothing exists in any node table until
    /// [`Library::attach`] or [`Library::insert_nodes`].
    pub fn new(files: Vec<LibFile>, settings: &crate::config::LibrarySettings) -> Self {
        let opts = Opts::from_settings(settings);
        let tree = build_tree(&files);
        let volumes = HashMap::new();
        let p = plan(&tree, &files, &opts, &volumes, settings.form);
        let zone_dirs = p.dirs.iter().enumerate().map(|(i, d)| (format!("dir:{}", d.path), i)).collect();
        let face = settings.page_faces.then_some(FaceSpec {
            size: [opts.page_w as f32, opts.page_h as f32],
            gap: settings.page_face_gap,
            color: settings.page_face_color,
        });
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
            zone_dirs,
            face,
            nodes: None,
            dir_at,
            sheet_at,
            animating: false,
        }
    }

    /// Insert the library's hierarchy into `t` under `parent` (a root when
    /// None) at the current positions: root → directory → volume → sheet →
    /// mount (translation + the contain-fit scale) → file (identity). With
    /// `files`, those nodes become the file nodes — reparented under their
    /// mounts, their local rows reset to the identity, their group rows
    /// kept; without, fresh file nodes are made, routing to group row i.
    /// Interior nodes route to no group row.
    pub fn insert_nodes(&self, t: &mut NodeTables, parent: Option<NodeHandle>, files: Option<&[NodeHandle]>) -> LibNodes {
        let id = Appearance::IDENTITY;
        let root = t.insert(parent, Similarity::IDENTITY, id).expect("the parent is live");
        let mut dir: Vec<NodeHandle> = Vec::with_capacity(self.plan.dirs.len());
        let mut volume = Vec::with_capacity(self.plan.dirs.len());
        for (d, at) in self.plan.dirs.iter().zip(&self.dir_at) {
            let p = d.parent.map_or(root, |i| dir[i]);
            let h = t.insert(Some(p), Similarity::from_translation(v3(*at)), id).expect("live");
            dir.push(h);
            volume.push(
                (!d.books.is_empty())
                    .then(|| t.insert(Some(h), Similarity::from_translation(v3(d.volume_pos)), id).expect("live")),
            );
        }
        let n = self.files.len();
        let (mut sheet, mut mount, mut file) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for (i, (fp, at)) in self.plan.files.iter().zip(&self.sheet_at).enumerate() {
            let vol = volume[fp.dir].expect("a directory with a file has a volume");
            let sh = t.insert(Some(vol), Similarity::from_translation(v3(*at)), id).expect("live");
            let m = Similarity { scale: fp.fit.scale as f32, ..Similarity::from_translation(v3(fp.fit.mount)) };
            let mt = t.insert(Some(sh), m, id).expect("live");
            let f = match files {
                Some(fs) => {
                    let f = fs[i];
                    t.reparent(f, Some(mt)).expect("a file node is live");
                    t.set_local(f, Similarity::IDENTITY).expect("live");
                    if t.post_scale(f).expect("live").xyz != [1.0; 3] {
                        t.set_post_scale(f, [1.0; 3]).expect("live");
                    }
                    f
                }
                None => {
                    let f = t.insert(Some(mt), Similarity::IDENTITY, id).expect("live");
                    t.set_group_row(f, Some(i as u32)).expect("live");
                    f
                }
            };
            sheet.push(sh);
            mount.push(mt);
            file.push(f);
        }
        LibNodes { root, dir, volume, sheet, mount, file }
    }

    /// Move the library onto the scene's tables: `files[i]` (file i's group
    /// node) is reparented under its mount, and the library keeps the
    /// handles it eases.
    pub fn attach(&mut self, t: &mut NodeTables, parent: Option<NodeHandle>, files: &[NodeHandle]) {
        assert_eq!(files.len(), self.files.len(), "one group node per library file");
        self.nodes = Some(self.insert_nodes(t, parent, Some(files)));
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// Files in the library (one group each: group id = load index).
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Directories that hold files (one volume each).
    pub fn volume_count(&self) -> usize {
        self.plan.dirs.iter().filter(|d| !d.books.is_empty()).count()
    }

    /// The directory node a zone id (`dir:<path>`) names, once attached.
    pub fn zone_node(&self, zone_id: &str) -> Option<NodeHandle> {
        let d = *self.zone_dirs.get(zone_id)?;
        Some(self.nodes.as_ref()?.dir[d])
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
            if self.face.is_some() { self.files.len() } else { 0 },
            self.opts.stack,
            self.opts.sort,
            if self.opts.reverse { " reversed" } else { "" },
            self.opts.page_w,
            self.opts.page_h,
        )
    }

    /// The world rect of a page (its sheet's world centre ± half a page),
    /// from the sheet node's CURRENT world (O(depth)).
    pub fn page_world_rect(&self, t: &NodeTables, sheet: NodeHandle) -> Option<([f32; 3], [f32; 3])> {
        let c = t.world(sheet).ok()?.0.translation;
        let (hw, hh) = ((self.opts.page_w / 2.0) as f32, (self.opts.page_h / 2.0) as f32);
        Some(([c[0] - hw, c[1] - hh, c[2]], [c[0] + hw, c[1] + hh, c[2]]))
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
    pub fn page(&mut self, t: &NodeTables, picked: Option<usize>, cmd: Page) -> String {
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
        self.replan(t);
        let one = (scope.len() == 1).then(|| {
            let d = &self.plan.dirs[scope[0]];
            format!(" — '{}' page {}/{}", d.path, d.head + 1, d.books.len())
        });
        format!("library: {cmd:?} turned {turned} of {} volume(s){}", scope.len(), one.unwrap_or_default())
    }

    /// Set or toggle the form of the volumes in scope (each toggles its own).
    pub fn form(&mut self, t: &NodeTables, picked: Option<usize>, cmd: FormCmd) -> String {
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
        self.replan(t);
        format!("library: form {cmd:?} on {} volume(s)", scope.len())
    }

    pub fn set_stack(&mut self, t: &NodeTables, stack: Stack) -> String {
        self.opts.stack = stack;
        self.replan(t);
        format!("library: stack {stack:?}")
    }

    pub fn set_sort(&mut self, t: &NodeTables, sort: Sort, reverse: bool) -> String {
        self.opts.sort = sort;
        self.opts.reverse = reverse;
        self.replan(t);
        format!("library: sort {sort:?}{}", if reverse { " reversed" } else { "" })
    }

    /// Recompute every target; each node's ease starts from its live local
    /// row (mid-ease, or dragged), so nothing teleports.
    fn replan(&mut self, t: &NodeTables) {
        self.plan = plan(&self.tree, &self.files, &self.opts, &self.volumes, self.default_form);
        if let Some(n) = &self.nodes {
            let live = |h: NodeHandle| t.local(h).ok().map(|l| l.translation.map(f64::from));
            for (at, &h) in self.dir_at.iter_mut().zip(&n.dir) {
                if let Some(p) = live(h) {
                    *at = p;
                }
            }
            for (at, &h) in self.sheet_at.iter_mut().zip(&n.sheet) {
                if let Some(p) = live(h) {
                    *at = p;
                }
            }
        }
        self.animating = true;
    }

    /// One animation step of `dt` seconds. None when settled (nothing to
    /// do, nothing written); otherwise the moved nodes' local rows are
    /// written and returned, and the caller mirrors what they carry.
    pub fn tick(&mut self, t: &mut NodeTables, dt: f32) -> Option<LibraryTick> {
        if !self.animating {
            return None;
        }
        let nodes = self.nodes.as_ref()?;
        let t0 = Instant::now();
        let k = ease_k(self.lerp, f64::from(dt));
        let mut moved = Vec::new();
        let mut pending = false;
        let settle = self.settle;
        let mut step = |at: &mut [f64; 3], target: [f64; 3], h: NodeHandle, t: &mut NodeTables| {
            let (next, did) = ease_step(*at, target, k, settle);
            if did {
                *at = next;
                let l = t.local(h).expect("library nodes stay live");
                t.set_local(h, Similarity { translation: v3(next), ..l }).expect("live");
                moved.push(h);
            }
            if next != target {
                pending = true;
            }
        };
        for (d, at) in self.dir_at.iter_mut().enumerate() {
            step(at, self.plan.dirs[d].pos, nodes.dir[d], t);
        }
        for (f, at) in self.sheet_at.iter_mut().enumerate() {
            step(at, self.plan.files[f].slot, nodes.sheet[f], t);
        }
        let ease_ms = t0.elapsed().as_secs_f64() * 1e3;
        self.animating = pending;
        Some(LibraryTick { nodes_moved: moved.len(), moved, ease_ms, still_animating: pending })
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
