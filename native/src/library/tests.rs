//! The library against the JS scheme's own numbers (the retired renderer's
//! `tools/contenttree.test.mjs` § library scheme, tests 35-41, and the Book
//! test arithmetic), plus the scene binding: the tree draws what bevy drew,
//! and what a page turn moves.

use std::collections::HashMap;

use glyph_scene_graph::{Appearance, NodeHandle, NodeTables, Similarity};

use crate::glyph_scene::mesh::MeshInstance;
use crate::glyph_scene::GroupRow;
use crate::spatial_scene::SpatialScene;

use super::book::{contain_fit, deck_slot, ease_k, ease_step, splay_grid, splay_slot, SplayDims};
use super::plan::{build_tree, flow_boxes, ordered_pack, plan, slot_for, sort_books, stack_extent, Form, LibFile, Opts, Sort, Stack};
use super::{FormCmd, Library, LibraryVerb, Page};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn dims(cols: u32, aspect: f64, page_w: f64, page_h: f64, gap_x: f64, gap_y: f64) -> SplayDims {
    SplayDims { cols, aspect, page_w, page_h, gap_x, gap_y }
}

/// The JS test's small page: 20 x 30, splay gaps 4/6, two columns, lift 8.
fn opts() -> Opts {
    Opts {
        page_w: 20.0,
        page_h: 30.0,
        gap: 10.0,
        stack: Stack::Z,
        sort: Sort::Name,
        reverse: false,
        max_upscale: 4.0,
        splay_cols: 2,
        splay_gap_x: 4.0,
        splay_gap_y: 6.0,
        splay_aspect: 1.5,
        splay_lift: 8.0,
        dir_gap: 80.0,
        depth_z: 500.0,
        aspect: 1.5,
        depth_front: false,
    }
}

/// A mock file whose content box is `w` x `h` from its origin, as a repo
/// file's ink runs right and down from (0, 0).
fn file(path: &str, w: f32, h: f32) -> LibFile {
    LibFile { rel_path: path.to_string(), ink_min: [0.0, -h, 0.0], ink_max: [w, 0.0, 0.0] }
}

#[test]
fn splay_grid_matches_the_book_arithmetic() {
    // Fixed cols: rows cover, extent exact.
    let g = splay_grid(6, &dims(3, 1.5, 20.0, 30.0, 4.0, 6.0));
    assert_eq!((g.cols, g.rows), (3, 2));
    assert!(close(g.w, 3.0 * 24.0 - 4.0) && close(g.h, 2.0 * 36.0 - 6.0));
    // Auto cols from the aspect target: nine squares at aspect 1 are 3 x 3.
    let g = splay_grid(9, &dims(0, 1.0, 10.0, 10.0, 0.0, 0.0));
    assert_eq!((g.cols, g.rows), (3, 3));
    // One page is a 1 x 1 grid (default aspect 1.5 rounds sqrt(1.5) to 1).
    let g = splay_grid(1, &dims(0, 1.5, 10.0, 10.0, 0.0, 0.0));
    assert_eq!((g.cols, g.rows, g.w, g.h), (1, 1, 10.0, 10.0));
    // Fixed cols never exceed the page count; no pages, no rows.
    assert_eq!(splay_grid(2, &dims(5, 1.5, 10.0, 10.0, 1.0, 1.0)).cols, 2);
    let g = splay_grid(0, &dims(0, 1.5, 10.0, 10.0, 2.0, 2.0));
    assert_eq!((g.cols, g.rows, g.h), (1, 0, 0.0));
}

#[test]
fn splay_slots_are_page_order_with_the_head_lifted() {
    let d = dims(2, 1.5, 20.0, 30.0, 4.0, 6.0);
    // 2 cols → a(0,0) b(1,0) c(0,1); columns centred on x, rows descending.
    assert_eq!(splay_slot(0, 0, 3, &d, 8.0), [-12.0, 0.0, 8.0]);
    assert_eq!(splay_slot(1, 0, 3, &d, 8.0), [12.0, 0.0, 0.0]);
    assert_eq!(splay_slot(2, 0, 3, &d, 8.0), [-12.0, -36.0, 0.0]);
    // Paging moves only the lift: the bookmark crosses the grid.
    assert_eq!(splay_slot(0, 2, 3, &d, 8.0)[2], 0.0);
    assert_eq!(splay_slot(2, 2, 3, &d, 8.0)[2], 8.0);
}

#[test]
fn deck_slot_law_reads_page_order_or_recency() {
    // A library volume (order +1): page 1, 2, 3 recede one gap each...
    let z = |i, head| deck_slot(i, head, 3, 1, 10.0)[2];
    assert_eq!([z(0, 0), z(1, 0), z(2, 0)], [0.0, -10.0, -20.0]);
    // ...and a turned page wraps to the back in turn order.
    assert_eq!([z(0, 1), z(1, 1), z(2, 1)], [-20.0, 0.0, -10.0]);
    // An agent book (order -1): the newest fronts, older recede.
    let r = |i| deck_slot(i, 2, 3, -1, 10.0)[2];
    assert_eq!([r(2), r(1), r(0)], [0.0, -10.0, -20.0]);
    // A head past the end is clamped; an empty deck rests at the origin.
    assert_eq!(deck_slot(0, 9, 3, 1, 10.0)[2], -10.0);
    assert_eq!(deck_slot(0, 0, 0, 1, 10.0), [0.0; 3]);
}

#[test]
fn contain_fit_is_uniform_capped_and_centred() {
    // The width term binds: min(20/10, 30/5, 10) = 2, content centred.
    let f = contain_fit([0.0, -5.0, 0.0], [10.0, 0.0, 0.0], 20.0, 30.0, 10.0, false);
    assert_eq!(f.scale, 2.0);
    assert_eq!(f.mount, [-10.0, 5.0, -0.0]);
    // The height term binds.
    assert_eq!(contain_fit([0.0, -60.0, 0.0], [10.0, 0.0, 0.0], 20.0, 30.0, 10.0, false).scale, 0.5);
    // A one-liner is capped at max_upscale: no giant.
    assert_eq!(contain_fit([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], 20.0, 30.0, 4.0, false).scale, 4.0);
    // An empty file seats neutrally (scale 1 at the page centre), never NaN.
    let e = contain_fit([0.0; 3], [0.0; 3], 20.0, 30.0, 4.0, false);
    assert_eq!((e.scale, e.mount), (1.0, [0.0; 3]));
    // Depth: centred (the JS) or the front on the page plane.
    let deep_min = [0.0, -5.0, -40.0];
    let deep_max = [10.0, 0.0, 0.0];
    assert_eq!(contain_fit(deep_min, deep_max, 20.0, 30.0, 10.0, false).mount[2], 40.0);
    assert_eq!(contain_fit(deep_min, deep_max, 20.0, 30.0, 10.0, true).mount[2], -0.0);
}

#[test]
fn stack_extents_and_shelf_pile_slots_match_the_js_footprints() {
    let o = Opts { gap: 5.0, ..opts() };
    // Deck: one page, (n − 1)·gap deep.
    assert_eq!(stack_extent(3, &o, Form::Deck), [20.0, 30.0, 10.0]);
    // Splay: the grid (2 cols, 3 pages → 2 rows), lift deep.
    assert_eq!(stack_extent(3, &o, Form::Splay), [2.0 * 24.0 - 4.0, 2.0 * 36.0 - 6.0, 8.0]);
    let shelf = Opts { stack: Stack::X, ..o };
    assert_eq!(stack_extent(3, &shelf, Form::Deck), [70.0, 30.0, 0.0]);
    // The JS shelf books sit at x = −25, 0, 25 (the volume adds nothing on x).
    let xs: Vec<f64> = (0..3).map(|i| slot_for(i, 3, 0, Form::Deck, &shelf)[0]).collect();
    assert_eq!(xs, [-25.0, 0.0, 25.0]);
    let pile = Opts { stack: Stack::Y, ..o };
    assert_eq!(stack_extent(2, &pile, Form::Deck), [20.0, 65.0, 0.0]);
    // The JS pile books sit at y = −15, −50: the volume's −pageH/2 + the slot.
    let ys: Vec<f64> = (0..2).map(|i| -o.page_h / 2.0 + slot_for(i, 2, 0, Form::Deck, &pile)[1]).collect();
    assert_eq!(ys, [-15.0, -50.0]);
    assert_eq!(stack_extent(0, &o, Form::Deck), [0.0; 3]);
}

#[test]
fn flow_boxes_snake_and_ordered_pack_wraps_by_area() {
    let sizes = [[10.0, 5.0], [10.0, 5.0], [10.0, 5.0]];
    let (slots, w, h) = flow_boxes(&sizes, 2.0, 25.0, true);
    assert_eq!((slots[0].x, slots[0].y), (0.0, 0.0));
    assert_eq!((slots[1].x, slots[1].y), (12.0, 0.0));
    // Row 1 runs right to left: the third box sits under the second.
    assert_eq!((slots[2].x, slots[2].y, slots[2].row), (12.0, -7.0, 1));
    assert_eq!((w, h), (22.0, 12.0));
    // One box never wraps; the wrap width otherwise comes from the area.
    let (one, w1, _) = ordered_pack(&[[100.0, 1.0]], 5.0, 1.5);
    assert_eq!((one[0].x, w1), (0.0, 100.0));
    let (four, _, _) = ordered_pack(&[[10.0, 10.0]; 4], 0.0, 1.0);
    assert_eq!(four.iter().map(|s| s.row).collect::<Vec<_>>(), [0, 0, 1, 1]);
}

#[test]
fn sorts_ask_their_questions() {
    // name: aa < b.css < zzz; width: zzz > b.css > aa.
    let files = vec![file("lib/aa.js", 2.0, 1.0), file("lib/zzzzzzzz.js", 8.0, 1.0), file("lib/b.css", 5.0, 1.0)];
    let order = |sort, reverse| {
        let mut b = vec![0, 1, 2];
        sort_books(&mut b, &files, sort, reverse);
        b
    };
    assert_eq!(order(Sort::Name, false), [0, 2, 1]);
    assert_eq!(order(Sort::Size, false)[0], 1, "size fronts the biggest book");
    assert_eq!(order(Sort::Ext, false)[0], 2, "ext shelves css before js");
    assert_eq!(order(Sort::Name, true)[0], 1, "reverse flips the name deck");
}

#[test]
fn the_tree_sorts_siblings_and_compresses_chains() {
    let files = vec![file("a/b/c/y.rs", 1.0, 1.0), file("a/b/c/X.rs", 1.0, 1.0), file("Top.md", 1.0, 1.0), file("d/e.rs", 1.0, 1.0)];
    let t = build_tree(&files);
    assert_eq!(t.files, [2]);
    let names: Vec<&str> = t.dirs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["a/b/c", "d"], "the single-child chain a → b → c lays out as its tail");
    assert_eq!(t.dirs[0].path, "a/b/c");
    assert_eq!(t.dirs[0].files, [1, 0], "names sort case-insensitively");
}

/// World translation of a dir by walking `parent` (the plan is relative).
fn dir_world(p: &super::plan::Plan, mut d: usize) -> [f64; 3] {
    let mut w = [0.0; 3];
    loop {
        for (wa, pa) in w.iter_mut().zip(p.dirs[d].pos) {
            *wa += pa;
        }
        match p.dirs[d].parent {
            Some(up) => d = up,
            None => return w,
        }
    }
}

#[test]
fn child_collections_hang_below_one_depth_step_back() {
    let o = Opts { depth_z: 100.0, dir_gap: 8.0, ..opts() };
    let files = vec![file("top.js", 1.0, 1.0), file("sub/inner.js", 1.0, 1.0)];
    let p = plan(&build_tree(&files), &files, &o, &HashMap::new(), Form::Deck);
    let world_z = |f: usize| dir_world(&p, p.files[f].dir)[2] + p.dirs[p.files[f].dir].volume_pos[2] + p.files[f].slot[2];
    assert_eq!(world_z(0), 0.0, "the root book fronts at z = 0");
    assert_eq!(world_z(1), -100.0, "the child collection sits exactly one depth step back");
    let sub = p.dirs.iter().position(|d| d.path == "sub").unwrap();
    assert!(p.dirs[sub].pos[1] <= -(30.0 + 8.0) + 1e-9, "and hangs below the parent's stack");
    // The root's footprint: its page, the dir gap, the child tier.
    assert_eq!(p.dirs[0].size[1], 30.0 + 8.0 + 30.0);
    assert_eq!(p.dirs[0].size[2], 100.0);
}

#[test]
fn the_plan_is_the_same_whatever_the_load_order() {
    let a = vec![file("src/b.rs", 3.0, 4.0), file("src/a.rs", 5.0, 2.0), file("x/y/z.md", 9.0, 9.0), file("r.txt", 1.0, 7.0)];
    let mut b = a.clone();
    b.reverse();
    let o = opts();
    let pa = plan(&build_tree(&a), &a, &o, &HashMap::new(), Form::Splay);
    let pb = plan(&build_tree(&b), &b, &o, &HashMap::new(), Form::Splay);
    for (i, f) in a.iter().enumerate() {
        let j = b.iter().position(|g| g.rel_path == f.rel_path).unwrap();
        let (fa, fb) = (&pa.files[i], &pb.files[j]);
        assert_eq!(fa.slot, fb.slot, "{}", f.rel_path);
        assert_eq!(dir_world(&pa, fa.dir), dir_world(&pb, fb.dir), "{}", f.rel_path);
        assert_eq!(fa.fit, fb.fit);
    }
}

#[test]
fn easing_is_frame_rate_independent_and_settles() {
    assert!(close(ease_k(9.0, 1.0 / 60.0), 1.0 - (-0.15f64).exp()));
    // A stalled frame does not launch the pages: dt is clamped to 0.1 s.
    assert_eq!(ease_k(9.0, 5.0), ease_k(9.0, 0.1));
    assert_eq!(ease_k(0.0, 1.0 / 60.0), 1.0, "rate 0 snaps");
    // Two half-steps land where one whole step does (to rounding).
    let one = ease_step([0.0; 3], [10.0, 0.0, 0.0], 1.0 - (-0.2f64).exp(), 1e-6).0;
    let h = 1.0 - (-0.1f64).exp();
    let two = ease_step(ease_step([0.0; 3], [10.0, 0.0, 0.0], h, 1e-6).0, [10.0, 0.0, 0.0], h, 1e-6).0;
    assert!((one[0] - two[0]).abs() < 1e-12);
    // The JS test's 40 updates of dt = 1 settle a page onto its slot.
    let mut at = [0.0, 0.0, -10.0];
    for _ in 0..40 {
        at = ease_step(at, [0.0; 3], ease_k(9.0, 1.0), 0.05).0;
    }
    assert_eq!(at, [0.0; 3]);
    // Within settle: snaps (and reports the move); exactly there: still.
    assert_eq!(ease_step([0.0, 0.0, 0.01], [0.0; 3], 0.1, 0.05), ([0.0; 3], true));
    assert_eq!(ease_step([0.0; 3], [0.0; 3], 0.1, 0.05), ([0.0; 3], false));
}

#[test]
fn library_verbs_parse_and_refuse() {
    assert_eq!(LibraryVerb::parse("page-to", &["3"]), Ok(LibraryVerb::Page(Page::To(2))));
    assert!(LibraryVerb::parse("page-to", &["0"]).is_err(), "pages count from 1");
    assert_eq!(LibraryVerb::parse("form", &["toggle"]), Ok(LibraryVerb::Form(FormCmd::Toggle)));
    assert_eq!(LibraryVerb::parse("library-stack", &["y"]), Ok(LibraryVerb::Stack(Stack::Y)));
    assert_eq!(LibraryVerb::parse("library-sort", &["ext", "reverse"]), Ok(LibraryVerb::Sort(Sort::Ext, true)));
    assert!(LibraryVerb::parse("form", &["open"]).is_err());
    assert!(LibraryVerb::parse("library-sort", &["size", "up"]).is_err());
    assert!(crate::cli::parse_verb("page-next").is_ok(), "--verb reaches the library parser");
}

/// A library on node tables the way the scene holds it: one flat group node
/// per file under an identity root (`GroupNodes`), then `attach` moves the
/// files under their mounts.
fn attached(files: &[LibFile], s: &crate::config::LibrarySettings) -> (Library, NodeTables, NodeHandle) {
    let mut lib = Library::new(files.to_vec(), s);
    let mut t = NodeTables::default();
    let root = t.insert(None, Similarity::IDENTITY, Appearance::IDENTITY).unwrap();
    let file_nodes: Vec<NodeHandle> = (0..files.len())
        .map(|i| {
            // Whatever the load row was: attach resets it to the identity.
            let h = t.insert(Some(root), Similarity::from_translation([i as f32 * 7.0, -3.0, 1.0]), Appearance::IDENTITY).unwrap();
            t.set_group_row(h, Some(i as u32)).unwrap();
            h
        })
        .collect();
    lib.attach(&mut t, Some(root), &file_nodes);
    (lib, t, root)
}

/// The old binding, rebuilt in bevy for the comparison: the hierarchy as
/// `Library::build` spawned it until 2026-10-10 (root → dir → volume →
/// sheet → face plate / mount → file card), every node's Transform taken
/// from the tree's LOCAL row, then bevy's propagation, `sync_all_to_group_
/// rows` and its mesh extraction — what the renderer drew from.
fn bevy_reference(lib: &Library, t: &NodeTables, tints: &[[f32; 3]]) -> (Vec<GroupRow>, Vec<MeshInstance>) {
    use bevy_transform::components::Transform;
    use glam::{Quat, Vec3};
    let n = lib.nodes.as_ref().unwrap();
    let tf = |h: NodeHandle| {
        let l = t.local(h).unwrap();
        let q = l.rotation;
        Transform { translation: Vec3::from(l.translation), rotation: Quat::from_xyzw(q[0], q[1], q[2], q[3]), scale: Vec3::splat(l.scale) }
    };
    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("library_root");
    let mut dir_e = Vec::new();
    let mut vol_e = Vec::new();
    for (d, dp) in lib.plan.dirs.iter().enumerate() {
        let parent = dp.parent.map_or(root, |i| dir_e[i]);
        let e = scene.spawn_child(parent, tf(n.dir[d]), "dir");
        dir_e.push(e);
        vol_e.push(n.volume[d].map(|v| scene.spawn_child(e, tf(v), "volume")));
    }
    for (i, fp) in lib.plan.files.iter().enumerate() {
        let sheet = scene.spawn_child(vol_e[fp.dir].unwrap(), tf(n.sheet[i]), "sheet");
        if let Some(face) = &lib.face {
            let (pw, ph) = (face.size[0], face.size[1]);
            scene.spawn_plate(sheet, "lib:face", Transform::from_xyz(0.0, 0.0, -face.gap), [pw, ph], [-pw / 2.0, -ph / 2.0], face.color);
        }
        let mount = scene.spawn_child(sheet, tf(n.mount[i]), "mount");
        scene.spawn_file_card(mount, format!("f{i}"), "", i as u32, tf(n.file[i]), ([0.0; 3], [1.0; 3]), tints[i]);
    }
    scene.update_transforms();
    let mut rows: Vec<GroupRow> = tints.iter().map(|t| GroupRow::tinted([0.0; 3], *t)).collect();
    scene.sync_all_to_group_rows(&mut rows);
    (rows, scene.mesh_draws().quads.clone())
}

/// The rows and faces the tree resolves, against bevy's, bit for bit.
fn assert_tree_matches_bevy(lib: &Library, t: &NodeTables, tints: &[[f32; 3]], what: &str) {
    let n = lib.nodes.as_ref().unwrap();
    let (want_rows, want_faces) = bevy_reference(lib, t, tints);
    let got_rows = crate::library::seated_rows(t, n, tints);
    for (i, (g, w)) in got_rows.iter().zip(&want_rows).enumerate() {
        let (gb, wb): (&[u32; 24], &[u32; 24]) = (bytemuck::cast_ref(&g.cols), bytemuck::cast_ref(&w.cols));
        assert_eq!(gb[..16], wb[..16], "{what}: file {i}'s group row (cols 0-3): tree {:?} bevy {:?}", &g.cols[..4], &w.cols[..4]);
    }
    let spec = lib.face.as_ref().expect("the default settings draw faces");
    let got_faces: Vec<MeshInstance> = n.sheet.iter().map(|&s| crate::glyph_scene::faces::face_instance(&t.world(s).unwrap().0, spec)).collect();
    assert_eq!(got_faces.len(), want_faces.len(), "{what}: one face per sheet, in file order");
    for (i, (g, w)) in got_faces.iter().zip(&want_faces).enumerate() {
        assert_eq!(bytemuck::bytes_of(g), bytemuck::bytes_of(w), "{what}: face {i}: tree {g:?} bevy {w:?}");
    }
}

/// The library on the transform tree (2026-10-10) draws what the bevy
/// hierarchy drew: every file's group row and every page face, bit for
/// bit, seated, mid-ease, settled after a page turn and a form change, and
/// after a directory drag and a file drag. Both sum each translation root
/// first; every other step is a product with 1 or a sum with 0 — so this
/// holds exactly, and the GPU resolve (same order) draws the same frame
/// (`translation_chains_resolve_root_first_and_bit_exact`, and the witness
/// frames in the findings log).
#[test]
fn the_tree_resolves_the_rows_bevy_composed() {
    let s = &crate::config::settings().library;
    let files = vec![
        file("a.rs", 41.3, 17.9),
        file("src/b.rs", 512.7, 1033.1),
        file("src/c.rs", 3.1, 1.25),
        file("src/deep/x/y.py", 97.0, 211.6),
        file("src/deep/z.md", 13.37, 900.0),
        file("docs/readme.md", 77.7, 66.6),
        file("docs/n.txt", 0.0, 0.0),
    ];
    let tints: Vec<[f32; 3]> = (0..files.len()).map(|i| [0.1 * i as f32, 0.5, 0.9]).collect();
    let (mut lib, mut t, _root) = attached(&files, s);
    assert_tree_matches_bevy(&lib, &t, &tints, "seated");
    // A whole-library form change: every dir and sheet eases.
    lib.form(&t, None, FormCmd::Set(Form::Splay));
    for frame in 0..9 {
        let tick = lib.tick(&mut t, 1.0 / 60.0).expect("animating");
        assert!(tick.nodes_moved > 0);
        if frame % 4 == 0 {
            assert_tree_matches_bevy(&lib, &t, &tints, &format!("splay frame {frame}"));
        }
    }
    while lib.tick(&mut t, 1.0 / 60.0).is_some_and(|k| k.still_animating) {}
    assert_tree_matches_bevy(&lib, &t, &tints, "splay settled");
    // A directory drag: one local row, the world delta in its parent's frame.
    let zone = lib.zone_node("dir:src/deep").expect("a zone per directory");
    let d = t.parent_world(zone).unwrap().world_delta_to_local([13.25, -7.5, 0.125]);
    let l = t.local(zone).unwrap();
    t.set_local(zone, Similarity { translation: [l.translation[0] + d[0], l.translation[1] + d[1], l.translation[2] + d[2]], ..l }).unwrap();
    // A file drag under its scaled mount (E10, through the tree).
    let f = lib.nodes.as_ref().unwrap().file[1];
    let d = t.parent_world(f).unwrap().world_delta_to_local([2.5, 1.0, 0.0]);
    t.set_local(f, Similarity::from_translation(d)).unwrap();
    assert_tree_matches_bevy(&lib, &t, &tints, "after the drags");
    lib.page(&t, None, Page::Next);
    for _ in 0..5 {
        lib.tick(&mut t, 1.0 / 60.0);
    }
    assert_tree_matches_bevy(&lib, &t, &tints, "mid page turn, dragged");
}

/// The scene binding, without a GPU: a file's world is the composition of
/// the plan (dir → volume → sheet slot → mount fit), a page turn writes only
/// its own volume's sheets (the local rows the GPU uploads), and the ease
/// lands every sheet on its new slot.
#[test]
fn a_page_turn_moves_one_volume_and_settles_on_the_slot_law() {
    let s = &crate::config::settings().library;
    let files = vec![file("d/a.rs", 50.0, 70.0), file("d/b.rs", 10.0, 10.0), file("d/c.rs", 30.0, 5.0), file("e/x.rs", 40.0, 40.0)];
    let (mut lib, mut t, _root) = attached(&files, s);

    let expect = |lib: &Library, f: usize| -> ([f64; 3], f64) {
        let p = &lib.plan;
        let fp = &p.files[f];
        let d = dir_world(p, fp.dir);
        let v = p.dirs[fp.dir].volume_pos;
        let t = [0, 1, 2].map(|a| d[a] + v[a] + fp.slot[a] + fp.fit.mount[a]);
        (t, fp.fit.scale)
    };
    let world = |t: &NodeTables, lib: &Library, f: usize| t.world(lib.nodes.as_ref().unwrap().file[f]).unwrap().0;
    for f in 0..files.len() {
        let (want, sc) = expect(&lib, f);
        let w = world(&t, &lib, f);
        for (a, ta) in want.iter().enumerate() {
            assert!((f64::from(w.translation[a]) - ta).abs() < 1e-3, "file {f} axis {a}: {} vs {ta}", w.translation[a]);
        }
        assert!((f64::from(w.scale) - sc).abs() < 1e-5, "file {f} scale");
    }

    // Turn the volume holding d/b.rs (file 1); e/ stays put.
    let reply = lib.page(&t, Some(1), Page::Next);
    assert!(reply.contains("turned 1 of 1"), "{reply}");
    let sheets: Vec<NodeHandle> = lib.nodes.as_ref().unwrap().sheet.clone();
    let mut moved = std::collections::BTreeSet::new();
    let mut frames = 0;
    while let Some(tick) = lib.tick(&mut t, 1.0 / 60.0) {
        frames += 1;
        for h in &tick.moved {
            moved.insert(sheets.iter().position(|s| s == h).expect("a page turn moves sheets only"));
        }
        if !tick.still_animating {
            break;
        }
        assert!(frames < 2000, "the ease never settled");
    }
    assert!(!lib.is_animating());
    assert_eq!(moved.into_iter().collect::<Vec<_>>(), [0, 1, 2], "only the turned volume's sheets moved");
    for f in 0..files.len() {
        let (want, _) = expect(&lib, f);
        let w = world(&t, &lib, f);
        for (a, ta) in want.iter().enumerate() {
            assert!((f64::from(w.translation[a]) - ta).abs() < 1e-2, "settled file {f} axis {a}");
        }
    }
    // Page order: page 2 (b.rs) now fronts at z = 0 in its volume.
    assert_eq!(lib.plan.files[1].slot, [0.0; 3]);
    // A settled library does nothing at all.
    assert!(lib.tick(&mut t, 1.0 / 60.0).is_none());
    // A form change retargets every volume when nothing is picked.
    assert!(lib.form(&t, None, FormCmd::Set(Form::Splay)).contains("2 volume(s)"));
    assert!(lib.is_animating());
}
