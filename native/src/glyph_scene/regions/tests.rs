//! The bounding regions against the tree cull, a brute-force pick, the
//! instances that reach the GPU and the library's drag (2026-10-10,
//! `regions.rs`).

use glam::DVec3;
use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

use super::{ray_to_local, region_box, region_instance, LibraryRegions, RegionInstance, RegionSpec, RegionTable};
use crate::glyph_scene::cull::{CullState, SegCull};
use crate::glyph_scene::nodes::read_buffer;
use crate::glyph_scene::tree_cull::{Aabb, TreeCull};
use crate::library::plan::LibFile;
use crate::library::{Form, FormCmd, Library, Page};

/// A nested library: chains (`tools/gen`, `a/b/c` compress to one
/// directory each), a four-deep branch, files of every shape and depth.
fn files() -> Vec<LibFile> {
    let dirs = ["", "src/", "src/core/", "src/core/deep/", "src/core/deep/inner/", "src/ui/", "docs/", "docs/guide/", "tools/gen/", "a/b/c/"];
    (0..40)
        .map(|i| {
            let (w, h, d) = (2.0 + (i * 37 % 300) as f32, 1.0 + (i * 53 % 700) as f32, (i * 11 % 40) as f32);
            LibFile { rel_path: format!("{}f{i:02}.rs", dirs[i % dirs.len()]), ink_min: [0.0, -h, -d], ink_max: [w, 0.0, 0.0] }
        })
        .collect()
}

struct Fixture {
    lib: Library,
    file_nodes: Vec<NodeHandle>,
    cull: CullState,
    tree: TreeCull,
    rgb: Vec<[f32; 4]>,
    face: Aabb,
}

/// The library attached to `t`, staged as a load stages it, with the tree
/// cull setup.rs builds for a library with regions (page faces in the
/// sheets' boxes, refits logged).
fn fixture(ctx: &crate::gpu::GpuContext, t: &mut NodeTables) -> Fixture {
    let s = &crate::config::settings().library;
    let files = files();
    let mut lib = Library::new(files.clone(), s);
    let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let file_nodes: Vec<NodeHandle> = (0..files.len())
        .map(|i| {
            let h = t.insert(Some(root), Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
            t.set_group_row(h, Some(i as u32)).unwrap();
            h
        })
        .collect();
    lib.attach(t, Some(root), &file_nodes);
    let tints: Vec<[f32; 3]> = (0..files.len()).map(|i| [0.2 + 0.01 * i as f32, 0.5, 0.8]).collect();
    let groups = crate::library::seated_rows(t, lib.nodes.as_ref().unwrap(), &tints);
    let segments: Vec<SegCull> = files
        .iter()
        .zip(&groups)
        .map(|(f, g)| {
            let (o, sc) = ([g.cols[0][0], g.cols[0][1], g.cols[0][2]], g.cols[3][0]);
            SegCull {
                min: [0, 1, 2].map(|k| o[k] + f.ink_min[k] * sc),
                max: [0, 1, 2].map(|k| o[k] + f.ink_max[k] * sc),
                slot_base: 0,
                slot_count: 1,
                tint: [0.3, 0.4, 0.5, 0.6],
                blocks: Vec::new(),
            }
        })
        .collect();
    let camera_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test camera"),
        size: 256,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    // Segment i is file i's, drawn with group row i.
    let segment_rows: Vec<Option<u32>> = (0..segments.len() as u32).map(Some).collect();
    let cull = CullState::new(ctx, crate::glyph_scene::target::POOL_FORMAT, wgpu::TextureFormat::Depth32Float, &camera_buf, &segments, &groups, &segment_rows);
    let rgb: Vec<[f32; 4]> = groups.iter().map(|g| g.cols[2]).collect();
    let f = lib.face.expect("the default settings draw page faces");
    let face = [[-f.size[0] / 2.0, -f.size[1] / 2.0, -f.gap], [f.size[0] / 2.0, f.size[1] / 2.0, -f.gap]];
    let nodes = lib.nodes.clone().unwrap();
    let mut tree = TreeCull::new(t, nodes.root, &file_nodes, &cull, &rgb).with_own_box(t, &nodes.sheet, face, &cull);
    tree.log_refits();
    Fixture { lib, file_nodes, cull, tree, rgb, face }
}

fn spec() -> RegionSpec {
    RegionSpec::from_settings(&crate::config::settings().library).expect("regions are on by default")
}

/// What the cull does before every frame after a move, and the scene after
/// it: refit, then take the refits into the regions.
fn note(fx: &mut Fixture, table: &mut RegionTable, t: &NodeTables, moved: &[NodeHandle]) {
    for h in moved {
        fx.tree.note_moved(h.index());
    }
    fx.tree.refit(t, &fx.cull);
    table.sync(&mut fx.tree);
}

/// Whether file `f` sits under directory `d` (Plan indices).
fn under(lib: &Library, f: usize, d: usize) -> bool {
    let mut k = Some(lib.plan.files[f].dir);
    while let Some(i) = k {
        if i == d {
            return true;
        }
        k = lib.plan.dirs[i].parent;
    }
    false
}

/// A point `x` of a frame whose world is `w`, in the frame of a node whose
/// world is `p` — f64, so the containment check has margin to spare (the
/// library never rotates).
fn into(p: &Similarity, w: &Similarity, x: [f32; 3]) -> DVec3 {
    let world = DVec3::from(w.translation.map(f64::from)) + DVec3::from(x.map(f64::from)) * f64::from(w.scale);
    (world - DVec3::from(p.translation.map(f64::from))) / f64::from(p.scale)
}

/// Every region against a tree cull built from scratch on the current rows
/// (the boxes an incremental refit must reach), and every file's ink box
/// and page face inside the regions of all its directories.
fn check(fx: &Fixture, table: &RegionTable, t: &mut NodeTables, spec: &RegionSpec, what: &str) {
    let nodes = fx.lib.nodes.clone().unwrap();
    let fresh = TreeCull::new(t, nodes.root, &fx.file_nodes, &fx.cull, &fx.rgb).with_own_box(t, &nodes.sheet, fx.face, &fx.cull);
    assert_eq!(table.len(), fx.lib.plan.dirs.len() - 1, "{what}: one region per directory but the root");
    let mut contained = 0;
    for r in 0..table.len() as u32 {
        let (d, depth) = table.dir_of(r);
        let slot = nodes.dir[d].index();
        let want = region_box(&fresh.bounds_of_slot(slot).unwrap(), depth, spec);
        assert_eq!(table.region_box_of(r), want, "{what}: region of '{}' vs its subtree box, padded", fx.lib.plan.dirs[d].path);
        let b = want.expect("every directory here holds a file");
        let dw = t.world(nodes.dir[d]).unwrap().0;
        for f in (0..fx.file_nodes.len()).filter(|&f| under(&fx.lib, f, d)) {
            let (fw, sw) = (t.world(nodes.file[f]).unwrap().0, t.world(nodes.sheet[f]).unwrap().0);
            let (lo, hi) = (fx.cull.local_min[f], fx.cull.local_max[f]);
            let ink = (0..8).map(|c| (fw, [if c & 1 == 0 { lo[0] } else { hi[0] }, if c & 2 == 0 { lo[1] } else { hi[1] }, if c & 4 == 0 { lo[2] } else { hi[2] }]));
            let face = (0..4).map(|c| (sw, [fx.face[c & 1][0], fx.face[c >> 1][1], fx.face[0][2]]));
            for (w, x) in ink.chain(face) {
                let p = into(&dw, &w, x);
                for k in 0..3 {
                    let eps = 1e-3 * (1.0 + p[k].abs());
                    assert!(
                        p[k] >= f64::from(b[0][k]) - eps && p[k] <= f64::from(b[1][k]) + eps,
                        "{what}: file {f} pokes out of '{}' on axis {k}: {} not in {}..{}",
                        fx.lib.plan.dirs[d].path,
                        p[k],
                        b[0][k],
                        b[1][k]
                    );
                }
            }
            contained += 1;
        }
    }
    assert!(contained > 40, "{what}: the containment check saw {contained} (file, region) pairs");
}

/// The regions ARE the tree cull's subtree boxes, padded (2026-10-10):
/// seated, after a directory drag (its ancestors' boxes refit; its own does
/// not change in its own frame), mid form change and mid page turn, every
/// region equals the padded box of a tree cull built from scratch on the
/// rows of that moment, and holds every page under it. The regions read
/// only the refit log, so a refit the log missed is a stale region.
#[test]
fn region_boxes_are_the_tree_bounds_padded_through_drags_and_eases() {
    let ctx = pollster::block_on(crate::gpu::init(None));
    let spec = spec();
    let mut t = NodeTables::default();
    let mut fx = fixture(&ctx, &mut t);
    let mut table = RegionTable::new(&fx.lib.plan, fx.lib.nodes.as_ref().unwrap(), &fx.tree, spec);
    check(&fx, &table, &mut t, &spec, "seated");
    let h = fx.lib.move_zone(&mut t, "dir:src/core/deep", [60.0, -45.0, 7.0]).unwrap();
    note(&mut fx, &mut table, &t, &[h]);
    check(&fx, &table, &mut t, &spec, "after a drag");
    fx.lib.form(&t, None, FormCmd::Set(Form::Splay));
    for _ in 0..6 {
        let tick = fx.lib.tick(&mut t, 1.0 / 60.0).unwrap();
        note(&mut fx, &mut table, &t, &tick.moved);
    }
    check(&fx, &table, &mut t, &spec, "mid splay");
    fx.lib.form(&t, None, FormCmd::Set(Form::Deck));
    for _ in 0..2000 {
        let Some(tick) = fx.lib.tick(&mut t, 1.0 / 60.0) else { break };
        note(&mut fx, &mut table, &t, &tick.moved);
    }
    fx.lib.page(&t, None, Page::Next);
    for _ in 0..4 {
        let tick = fx.lib.tick(&mut t, 1.0 / 60.0).unwrap();
        note(&mut fx, &mut table, &t, &tick.moved);
    }
    check(&fx, &table, &mut t, &spec, "mid page turn");
}

/// Every region the ray enters, the deepest kept (nearest among equals):
/// the answer the top-down walk must give, by brute force.
fn brute_pick(table: &RegionTable, t: &NodeTables, nodes: &crate::library::LibNodes, ro: DVec3, rd: DVec3) -> Option<(u32, f64)> {
    let mut best: Option<(u32, f64, u32)> = None;
    for r in 0..table.len() as u32 {
        let (d, depth) = table.dir_of(r);
        let Some(b) = table.region_box_of(r) else { continue };
        let (o, v) = ray_to_local(&t.world(nodes.dir[d]).unwrap().0, ro, rd);
        let lo = DVec3::from(b[0].map(f64::from));
        let hi = DVec3::from(b[1].map(f64::from));
        if let Some(tt) = crate::glyph_scene::pick::ray_aabb(o, v, lo, hi) {
            if best.is_none_or(|(bd, bt, br)| depth > bd || (depth == bd && (tt < bt || (tt == bt && r > br)))) {
                best = Some((depth, tt, r));
            }
        }
    }
    best.map(|(_, tt, r)| (r, tt))
}

/// The region pick (2026-10-10) returns the INNERMOST region under the ray
/// — the deepest directory whose region the ray enters — by a walk that
/// descends only into regions it enters: over a grid of rays from a camera
/// in front of the library and one from the side, seated and after a drag,
/// it agrees with a brute-force scan of every region, and a ray through the
/// deepest directory's page picks that depth-4 region. (Min thickness 0, so
/// every child sits strictly inside its parent; a child poking out is the
/// walk's one documented blind spot.)
#[test]
fn the_region_pick_is_the_innermost_region_the_ray_enters() {
    let ctx = pollster::block_on(crate::gpu::init(None));
    let spec = RegionSpec { min_thickness: 0.0, ..spec() };
    let mut t = NodeTables::default();
    let mut fx = fixture(&ctx, &mut t);
    let mut table = RegionTable::new(&fx.lib.plan, fx.lib.nodes.as_ref().unwrap(), &fx.tree, spec);
    let nodes = fx.lib.nodes.clone().unwrap();
    let sweep = |t: &NodeTables, table: &RegionTable, what: &str| {
        let (mut hits, mut deep) = (0, 0);
        for eye in [DVec3::new(0.0, -150.0, 900.0), DVec3::new(700.0, -100.0, 300.0)] {
            for i in 0..60 {
                for j in 0..60 {
                    let target = DVec3::new(-400.0 + 800.0 * f64::from(i) / 59.0, 50.0 - 600.0 * f64::from(j) / 59.0, -60.0);
                    let rd = (target - eye).normalize();
                    let got = table.pick(t, eye, rd);
                    let want = brute_pick(table, t, &nodes, eye, rd);
                    assert_eq!(got.map(|g| g.0), want.map(|w| w.0), "{what}: ray to {target:?} from {eye:?}: walk {got:?} brute {want:?} dirs {:?} {:?}", got.map(|g| table.dir_of(g.0)), want.map(|w| table.dir_of(w.0)));
                    hits += usize::from(got.is_some());
                    deep += usize::from(got.is_some_and(|(r, _)| table.dir_of(r).1 >= 3));
                }
            }
        }
        assert!(hits > 500 && deep > 20, "{what}: the grid must reach regions ({hits} hits, {deep} at depth >= 3)");
    };
    sweep(&t, &table, "seated");
    // Straight through the deepest directory's head page: depth 4.
    let inner = fx.lib.plan.dirs.iter().position(|d| d.path == "src/core/deep/inner").unwrap();
    let f = fx.lib.plan.files.iter().position(|p| p.dir == inner).unwrap();
    let c = t.world(nodes.sheet[f]).unwrap().0.translation.map(f64::from);
    let (r, _) = table.pick(&t, DVec3::new(c[0], c[1], c[2] + 500.0), DVec3::NEG_Z).expect("a ray through a page is inside its regions");
    assert_eq!(table.dir_of(r), (inner, 4), "the ray through src/core/deep/inner's page picks that directory");
    let h = fx.lib.move_zone(&mut t, "dir:src/core", [-150.0, 80.0, 20.0]).unwrap();
    note(&mut fx, &mut table, &t, &[h]);
    sweep(&t, &table, "after a drag");
}

/// What reaches the GPU (2026-10-26, the host composes the regions): after
/// each frame's resolve and `prepare`, the instance buffer read back is
/// `region_instance` of every region's box under its directory's
/// chain-walked world, bit for bit, and each instance's eight corners (as
/// `vs_region` places them) are the box's corners carried through that
/// world in f64 — seated, after one directory is dragged (its ancestors'
/// boxes refit, its subtree's directories resolve) and another scaled —
/// and a settled frame composes and uploads nothing. A region the dirty
/// tracking missed reads back stale.
#[test]
fn the_gpu_draws_the_regions_the_tree_composes() {
    let ctx = pollster::block_on(crate::gpu::init(None));
    let spec = spec();
    let mut t = NodeTables::default();
    let mut fx = fixture(&ctx, &mut t);
    let camera_buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test camera"),
        size: 256,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: false,
    });
    let table = RegionTable::new(&fx.lib.plan, fx.lib.nodes.as_ref().unwrap(), &fx.tree, spec);
    let regions = LibraryRegions::new(&ctx.device, &camera_buf, crate::glyph_scene::target::POOL_FORMAT, table);
    let nodes = fx.lib.nodes.clone().unwrap();
    let frame = |t: &mut NodeTables, fx: &mut Fixture| {
        let plan = t.resolve_frame();
        fx.tree.refit(t, &fx.cull);
        regions.prepare(&ctx.queue, &mut fx.tree, t, Some(&plan))
    };
    let check = |t: &mut NodeTables, fx: &mut Fixture, what: &str| {
        let cost = frame(t, fx);
        assert!(cost.uploaded > 0, "{what}: something moved ({cost:?})");
        let count = regions.table.borrow().len();
        let got: Vec<RegionInstance> = bytemuck::cast_slice(&read_buffer(&ctx, &regions.instances, (count * 64) as u64)).to_vec();
        let table = regions.table.borrow();
        for (r, a) in got.iter().enumerate() {
            let (d, _) = table.dir_of(r as u32);
            let b = table.region_box_of(r as u32).unwrap();
            let w = t.world(nodes.dir[d]).unwrap().0;
            let want = region_instance(&w, b[0], b[1], table.color[r]);
            assert_eq!(bytemuck::bytes_of(a), bytemuck::bytes_of(&want), "{what}: region {r} on the GPU {a:?}, composed {want:?}");
            let q = glam::DQuat::from_xyzw(f64::from(w.rotation[0]), f64::from(w.rotation[1]), f64::from(w.rotation[2]), f64::from(w.rotation[3]));
            let wt = DVec3::new(f64::from(w.translation[0]), f64::from(w.translation[1]), f64::from(w.translation[2]));
            let col = |c: [f32; 4]| DVec3::new(f64::from(c[0]), f64::from(c[1]), f64::from(c[2]));
            let at = DVec3::new(f64::from(a.col0[3]), f64::from(a.col1[3]), f64::from(a.col2[3]));
            for k in 0..8 {
                let c = [k & 1, (k >> 1) & 1, (k >> 2) & 1];
                let corner = DVec3::new(f64::from(b[c[0]][0]), f64::from(b[c[1]][1]), f64::from(b[c[2]][2]));
                let expect = wt + q * (corner * f64::from(w.scale));
                let u = c.map(|i| i as f64 - 0.5);
                let got = col(a.col0) * u[0] + col(a.col1) * u[1] + col(a.col2) * u[2] + at;
                assert!((got - expect).length() <= 1e-3 * (1.0 + expect.length()), "{what}: region {r} corner {k} drawn at {got:?}, the box's is {expect:?}");
            }
        }
    };
    check(&mut t, &mut fx, "seated");
    let settled = frame(&mut t, &mut fx);
    assert_eq!((settled.composed, settled.bytes), (0, 0), "a settled frame composes and uploads nothing: {settled:?}");
    let h = fx.lib.move_zone(&mut t, "dir:src/core", [90.0, -30.0, 4.0]).unwrap();
    let s = fx.lib.zone_node("dir:docs").unwrap();
    let l = t.local(s).unwrap();
    t.set_local(s, Similarity { scale: 1.25, ..l }).unwrap();
    fx.tree.note_moved(h.index());
    fx.tree.note_moved(s.index());
    check(&mut t, &mut fx, "dragged and scaled");
}

/// A region drag (2026-10-10): `g` with a region picked grabs its
/// directory's zone (`grab_target`), and the drag through
/// `Library::move_zone` moves exactly that directory's subtree — every file
/// under it by the drag's world delta, every other file not at all — and
/// the move sticks through a re-plan (held to an undragged twin).
#[test]
fn a_region_drag_moves_its_subtree_and_sticks() {
    use crate::glyph_scene::interaction::grab_target;
    let ctx = pollster::block_on(crate::gpu::init(None));
    let spec = spec();
    let mut t = NodeTables::default();
    let mut fx = fixture(&ctx, &mut t);
    let mut table = RegionTable::new(&fx.lib.plan, fx.lib.nodes.as_ref().unwrap(), &fx.tree, spec);
    let nodes = fx.lib.nodes.clone().unwrap();
    // Pick through a corner of src/core's head page: inside src/core's
    // region, outside its children's (they sit below it).
    let core = fx.lib.plan.dirs.iter().position(|d| d.path == "src/core").unwrap();
    let f = fx.lib.plan.files.iter().position(|p| p.dir == core).unwrap();
    let c = t.world(nodes.sheet[f]).unwrap().0.translation.map(f64::from);
    let (r, _) = table.pick(&t, DVec3::new(c[0] + 25.0, c[1] + 35.0, c[2] + 500.0), DVec3::NEG_Z).unwrap();
    let (d, _) = table.dir_of(r);
    assert_eq!(fx.lib.plan.dirs[d].path, "src/core", "the corner of src/core's head page is in src/core's region and no deeper one");
    let zone = format!("dir:{}", fx.lib.plan.dirs[d].path);
    let grabbed = grab_target(false, Some(zone.clone()), None).expect("g takes a picked region");
    assert_eq!(grabbed, zone);
    assert_eq!(grab_target(true, Some(zone.clone()), Some("dir:docs".into())), Some(zone.clone()), "c takes the picked region over a file's zone");
    assert_eq!(grab_target(false, None, Some("dir:docs".into())), None, "g without a region grabs the file itself");
    let before: Vec<[f32; 3]> = nodes.file.iter().map(|&h| t.world(h).unwrap().0.translation).collect();
    let delta = [37.5f32, -12.25, 3.0];
    let h = fx.lib.move_zone(&mut t, &grabbed, delta).unwrap();
    note(&mut fx, &mut table, &t, &[h]);
    let mut moved = 0;
    for (i, &fh) in nodes.file.iter().enumerate() {
        let now = t.world(fh).unwrap().0.translation;
        if under(&fx.lib, i, d) {
            moved += 1;
            for k in 0..3 {
                let got = now[k] - before[i][k];
                assert!((got - delta[k]).abs() < 1e-3, "file {i} under the region moved {got} on axis {k}, the drag {}", delta[k]);
            }
        } else {
            assert_eq!(now, before[i], "file {i} outside the dragged region did not move");
        }
    }
    assert!(moved >= 8, "the region carries its subtree ({moved} files)");
    // Through a page turn, settled, against a twin that was never dragged.
    let mut tt = NodeTables::default();
    let mut twin = fixture(&ctx, &mut tt).lib;
    for (lib, t) in [(&mut fx.lib, &mut t), (&mut twin, &mut tt)] {
        lib.page(t, None, Page::Next);
        let mut frames = 0;
        while lib.tick(t, 1.0 / 60.0).is_some_and(|k| k.still_animating) {
            frames += 1;
            assert!(frames < 5000, "the ease never settled");
        }
    }
    let a = t.world(nodes.dir[d]).unwrap().0.translation;
    let b = tt.world(twin.nodes.as_ref().unwrap().dir[d]).unwrap().0.translation;
    for k in 0..3 {
        assert!((a[k] - b[k] - delta[k]).abs() < 1e-2, "after a page turn the region sits {} from its twin on axis {k}, dragged {}", a[k] - b[k], delta[k]);
    }
}
