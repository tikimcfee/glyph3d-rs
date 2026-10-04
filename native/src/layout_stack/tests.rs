use super::*;

fn make_test_views() -> Vec<FileView> {
    let dummy_item = crate::repo::file_item_params(&RepoParams::default(), 100, 5);
    vec![
        FileView {
            rel_path: "src/main.rs".to_string(),
            dir: "src".to_string(),
            group_id: 0,
            record_count: 10,
            slot_base: 0,
            slot_count: 10,
            width: 50.0,
            height: 40.0,
            z_min: 0.0,
            z_max: 0.0,
            offset: [0.0; 3],
            item: dummy_item,
        },
        FileView {
            rel_path: "src/lib.rs".to_string(),
            dir: "src".to_string(),
            group_id: 1,
            record_count: 10,
            slot_base: 10,
            slot_count: 10,
            width: 60.0,
            height: 80.0,
            z_min: 0.0,
            z_max: 0.0,
            offset: [0.0; 3],
            item: dummy_item,
        },
        FileView {
            rel_path: "tests/smoke.rs".to_string(),
            dir: "tests".to_string(),
            group_id: 2,
            record_count: 5,
            slot_base: 20,
            slot_count: 5,
            width: 30.0,
            height: 20.0,
            z_min: 0.0,
            z_max: 0.0,
            offset: [0.0; 3],
            item: dummy_item,
        },
    ]
}

#[test]
fn default_shelf_mode_equals_layout_shelf() {
    let mut views1 = make_test_views();
    let mut views2 = make_test_views();
    let params = RepoParams::default();

    let (groups1, bmin1, bmax1) = crate::repo::layout_shelf(&mut views1, &params);
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Shelf, params);
    let (groups2, bmin2, bmax2) = controller.apply(&mut views2);

    assert_eq!(groups1.len(), groups2.len());
    assert_eq!(bmin1, bmin2);
    assert_eq!(bmax1, bmax2);
    for (v1, v2) in views1.iter().zip(views2.iter()) {
        assert_eq!(v1.offset, v2.offset);
    }
}

#[test]
fn dynamic_assignment_moves_file_to_desk() {
    let mut views = make_test_views();
    let params = RepoParams::default();
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

    // Initially in dir:src and dir:tests
    assert_eq!(controller.zone_for_file("src/main.rs", "src"), "dir:src");

    // User/Agent pulls main.rs into an active desk
    controller.assign_file("src/main.rs", "desk:active");
    assert_eq!(controller.zone_for_file("src/main.rs", "src"), "desk:active");

    let (groups, bounds_min, bounds_max) = controller.apply(&mut views);
    assert_eq!(groups.len(), 3);
    assert!(bounds_max[0] > 0.0);
    assert!(bounds_min[1] < 0.0);
}

#[test]
fn test_moving_zone_moves_files_in_gpu_groups() {
    let mut views = make_test_views();
    let params = RepoParams::default();
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

    // Put src/main.rs and src/lib.rs on desk:active
    controller.assign_file("src/main.rs", "desk:active");
    controller.assign_file("src/lib.rs", "desk:active");

    let (mut groups, _, _) = controller.apply(&mut views);
    let orig_main_pos = groups[0].cols[0];
    let orig_lib_pos = groups[1].cols[0];
    let orig_tests_pos = groups[2].cols[0];

    // Bounds are available for the desk zone
    assert!(controller.zone_world_bounds("desk:active").is_some());

    // Now move the active desk by (100, 50, -10)
    let moved = controller.move_zone("desk:active", glam::Vec3::new(100.0, 50.0, -10.0));
    assert!(moved);

    // Synchronize GPU groups
    let updated_gids = controller.sync_gpu_groups(&mut groups);
    assert!(updated_gids.contains(&0));
    assert!(updated_gids.contains(&1));

    // main.rs and lib.rs moved by exactly (100, 50, -10)
    assert_eq!(groups[0].cols[0][0], orig_main_pos[0] + 100.0);
    assert_eq!(groups[0].cols[0][1], orig_main_pos[1] + 50.0);
    assert_eq!(groups[0].cols[0][2], orig_main_pos[2] - 10.0);

    assert_eq!(groups[1].cols[0][0], orig_lib_pos[0] + 100.0);
    assert_eq!(groups[1].cols[0][1], orig_lib_pos[1] + 50.0);
    assert_eq!(groups[1].cols[0][2], orig_lib_pos[2] - 10.0);

    // tests/smoke.rs in the other zone did NOT move
    assert_eq!(groups[2].cols[0], orig_tests_pos);
}

#[test]
fn test_scaling_zone_scales_files_in_gpu_groups() {
    let mut views = make_test_views();
    let params = RepoParams::default();
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

    controller.assign_file("src/main.rs", "desk:scaled");
    let (mut groups, _, _) = controller.apply(&mut views);

    let scaled = controller.scale_zone("desk:scaled", 2.5);
    assert!(scaled);

    let updated_gids = controller.sync_gpu_groups(&mut groups);
    assert!(updated_gids.contains(&0));
    assert_eq!(groups[0].cols[3][0], 2.5);
    assert_eq!(groups[0].cols[3][1], 2.5);
    assert_eq!(groups[0].cols[3][2], 2.5);
}

#[test]
fn test_shelf_hierarchy_populated() {
    let mut views = make_test_views();
    let params = RepoParams::default();
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Shelf, params);

    let (groups, _, _) = controller.apply(&mut views);
    assert_eq!(groups.len(), 3);

    // base:shelf zone must exist
    assert!(controller.zone_world_bounds("base:shelf").is_some());
    // file entities must exist
    assert_eq!(controller.file_entities.len(), 3);
    assert!(controller.file_world_bounds(0).is_some());
}

#[test]
fn test_zone_world_bounds_covers_children() {
    let mut views = make_test_views();
    let params = RepoParams::default();
    let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

    controller.assign_file("src/main.rs", "desk:combo");
    controller.assign_file("src/lib.rs", "desk:combo");

    let _ = controller.apply(&mut views);

    let zone_bounds = controller.zone_world_bounds("desk:combo").expect("zone bounds exist");
    let file0_bounds = controller.file_world_bounds(0).expect("file 0 bounds exist");
    let file1_bounds = controller.file_world_bounds(1).expect("file 1 bounds exist");

    // Zone bounds must envelope both child file bounds
    assert!(zone_bounds.0[0] <= file0_bounds.0[0] && zone_bounds.0[0] <= file1_bounds.0[0]);
    assert!(zone_bounds.1[0] >= file0_bounds.1[0] && zone_bounds.1[0] >= file1_bounds.1[0]);
}
