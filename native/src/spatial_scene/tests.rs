use super::*;
use glam::{Quat, Vec3};
use crate::glyph_scene::GroupRow;

#[test]
fn test_ecs_hierarchy_propagation() {
    let mut scene = SpatialScene::new();

    let root = scene.spawn_root("canvas_root");
    let zone = scene.spawn_child(
        root,
        Transform::from_xyz(100.0, 50.0, 0.0),
        "zone_carrel",
    );
    let file = scene.spawn_child(
        zone,
        Transform::from_xyz(10.0, 20.0, 5.0),
        "file_card",
    );

    scene.update_transforms();

    let file_gtf = scene.world.get::<GlobalTransform>(file).unwrap();
    let trans = file_gtf.translation();
    assert!((trans.x - 110.0).abs() < 1e-4);
    assert!((trans.y - 70.0).abs() < 1e-4);
    assert!((trans.z - 5.0).abs() < 1e-4);
}

#[test]
fn test_ecs_mesh_draw_collection() {
    let mut scene = SpatialScene::new();

    let _plate = scene.world.spawn((
        Transform::from_xyz(10.0, 20.0, -1.0),
        SceneMeshKind::Quad {
            size: [50.0, 30.0],
            origin: [0.0, -30.0],
        },
        SceneMeshMaterial {
            color: [0.2, 0.3, 0.4, 1.0],
            params: [0.0, 0.0, 0.0, 0.0],
        },
    )).id();

    scene.update_transforms();
    let draws = scene.collect_mesh_instances();
    assert_eq!(draws.quads.len(), 1);
    assert_eq!(draws.cubes.len(), 0);
    assert_eq!(draws.quads[0].color, [0.2, 0.3, 0.4, 1.0]);
}

#[test]
fn test_ecs_world_bounds_and_raycast() {
    let mut scene = SpatialScene::new();

    let root = scene.spawn_root("root");
    let card = scene.spawn_file_card(
        root,
        "test.rs",
        "src",
        0,
        Transform::from_xyz(10.0, 20.0, 0.0),
        ([0.0, -40.0, 0.0], [50.0, 0.0, 1.0]),
        [1.0, 1.0, 1.0],
    );

    scene.update_transforms();

    let bounds = scene.world_bounds(card).unwrap();
    assert_eq!(bounds.0, [10.0, -20.0, 0.0]);
    assert_eq!(bounds.1, [60.0, 20.0, 1.0]);

    // Raycast straight at center of card
    let ro = glam::DVec3::new(35.0, 0.0, 50.0);
    let rd = glam::DVec3::new(0.0, 0.0, -1.0);
    let hit = scene.raycast_obb(ro, rd);
    assert!(hit.is_some());
    let (hit_entity, hit_t) = hit.unwrap();
    assert_eq!(hit_entity, card);
    assert!((hit_t - 49.0).abs() < 1e-4);
}

#[test]
fn test_parenting_rotates_and_scales_children() {
    let mut scene = SpatialScene::new();
    let parent = scene.world.spawn(
        Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::splat(2.0),
        }
    ).id();
    let child = scene.spawn_child(parent, Transform::from_xyz(5.0, 0.0, 0.0), "child");

    scene.update_transforms();

    let child_gtf = scene.world.get::<GlobalTransform>(child).unwrap();
    assert_eq!(child_gtf.translation().x, 20.0);
    assert_eq!(child_gtf.to_scale_rotation_translation().0, Vec3::splat(2.0));
}

#[test]
fn test_detach_makes_entity_root() {
    let mut scene = SpatialScene::new();
    let parent = scene.spawn_root("parent");
    let child = scene.spawn_child(parent, Transform::from_xyz(5.0, 0.0, 0.0), "child");

    assert_eq!(scene.world.get::<ChildOf>(child).map(|c| c.0), Some(parent));
    scene.detach(child);
    assert_eq!(scene.world.get::<ChildOf>(child), None);
}

#[test]
fn test_sync_to_group_rows_updates_gpu_table() {
    let mut scene = SpatialScene::new();
    let zone = scene.spawn_root("zone");
    scene.world.entity_mut(zone).insert(Transform::from_xyz(50.0, 20.0, 0.0));

    let _file = scene.spawn_file_card(
        zone,
        "test.rs",
        "src",
        3,
        Transform::from_xyz(5.0, -2.0, -1.0),
        ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
        [0.8, 0.2, 0.4],
    );
    scene.update_transforms();

    let mut groups = vec![GroupRow::identity([0.0; 3]); 5];
    let synced = scene.sync_to_group_rows(&mut groups);

    assert_eq!(synced, vec![3]);
    assert_eq!(groups[3].cols[0], [55.0, 18.0, -1.0, 0.0]);
    assert_eq!(groups[3].cols[2], [0.8, 0.2, 0.4, 1.0]);
}

#[test]
fn test_spatial_alignment_row_x() {
    let mut scene = SpatialScene::new();
    let container = scene.spawn_root("row_container");
    scene.world.entity_mut(container).insert(SpatialAlignment::row(10.0));

    let c1 = scene.spawn_child(container, Transform::IDENTITY, "item1");
    scene.world.entity_mut(c1).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [20.0, 30.0, 5.0],
    });

    let c2 = scene.spawn_child(container, Transform::IDENTITY, "item2");
    scene.world.entity_mut(c2).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [40.0, 25.0, 5.0],
    });

    scene.update_transforms();

    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    assert_eq!(t1.translation, Vec3::new(0.0, 0.0, 0.0));
    assert_eq!(t2.translation, Vec3::new(30.0, 0.0, 0.0)); // 20.0 + 10.0 spacing

    let container_bounds = scene.world.get::<LocalBounds>(container).unwrap();
    assert_eq!(container_bounds.min, [0.0, 0.0, 0.0]);
    assert_eq!(container_bounds.max, [70.0, 30.0, 5.0]);
}

#[test]
fn test_spatial_alignment_column_y_upwards() {
    let mut scene = SpatialScene::new();
    let container = scene.spawn_root("column_container");
    scene.world.entity_mut(container).insert(SpatialAlignment::column(5.0, false));

    let c1 = scene.spawn_child(container, Transform::IDENTITY, "item1");
    scene.world.entity_mut(c1).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [50.0, 20.0, 0.0],
    });

    let c2 = scene.spawn_child(container, Transform::IDENTITY, "item2");
    scene.world.entity_mut(c2).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [40.0, 15.0, 0.0],
    });

    scene.update_transforms();

    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    assert_eq!(t1.translation, Vec3::new(0.0, 0.0, 0.0));
    assert_eq!(t2.translation, Vec3::new(0.0, 25.0, 0.0)); // 20.0 + 5.0 spacing
}

#[test]
fn test_spatial_alignment_column_y_downwards() {
    let mut scene = SpatialScene::new();
    let container = scene.spawn_root("doc_column");
    scene.world.entity_mut(container).insert(SpatialAlignment::column(5.0, true));

    let c1 = scene.spawn_child(container, Transform::IDENTITY, "line1");
    scene.world.entity_mut(c1).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [100.0, 10.0, 0.0],
    });

    let c2 = scene.spawn_child(container, Transform::IDENTITY, "line2");
    scene.world.entity_mut(c2).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [80.0, 10.0, 0.0],
    });

    scene.update_transforms();

    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    assert_eq!(t1.translation, Vec3::new(0.0, -10.0, 0.0));
    assert_eq!(t2.translation, Vec3::new(0.0, -25.0, 0.0)); // -10 - 5 - 10
}

#[test]
fn test_spatial_alignment_depth_z_cascade() {
    let mut scene = SpatialScene::new();
    let container = scene.spawn_root("deck_container");
    scene.world.entity_mut(container).insert(SpatialAlignment::depth(15.0, 2.0, 3.0));

    let c0 = scene.spawn_child(container, Transform::IDENTITY, "card0");
    scene.world.entity_mut(c0).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [100.0, 50.0, 1.0],
    });

    let c1 = scene.spawn_child(container, Transform::IDENTITY, "card1");
    scene.world.entity_mut(c1).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [100.0, 50.0, 1.0],
    });

    let c2 = scene.spawn_child(container, Transform::IDENTITY, "card2");
    scene.world.entity_mut(c2).insert(LocalBounds {
        min: [0.0, 0.0, 0.0],
        max: [100.0, 50.0, 1.0],
    });

    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(c0).unwrap();
    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    assert_eq!(t0.translation, Vec3::new(0.0, 0.0, 0.0));
    assert_eq!(t1.translation, Vec3::new(2.0, 3.0, -15.0));
    assert_eq!(t2.translation, Vec3::new(4.0, 6.0, -30.0));
}

#[test]
fn test_spatial_alignment_splay_grid_wrapping() {
    let mut scene = SpatialScene::new();
    let container = scene.spawn_root("splay_grid");
    // 2 items per row, 10px item spacing, 20px row spacing
    scene.world.entity_mut(container).insert(SpatialAlignment::splay(10.0, 20.0, 2));

    let items: Vec<Entity> = (0..4)
        .map(|i| {
            let e = scene.spawn_child(container, Transform::IDENTITY, format!("grid_item_{i}"));
            scene.world.entity_mut(e).insert(LocalBounds {
                min: [0.0, 0.0, 0.0],
                max: [50.0, 30.0, 0.0],
            });
            e
        })
        .collect();

    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(items[0]).unwrap();
    let t1 = scene.world.get::<Transform>(items[1]).unwrap();
    let t2 = scene.world.get::<Transform>(items[2]).unwrap();
    let t3 = scene.world.get::<Transform>(items[3]).unwrap();

    // Row 0 (top track at y=0)
    assert_eq!(t0.translation, Vec3::new(0.0, 0.0, 0.0));
    assert_eq!(t1.translation, Vec3::new(60.0, 0.0, 0.0)); // 50 + 10

    // Row 1 (wrapped: track_y = 0 - 30 - 20 = -50)
    assert_eq!(t2.translation, Vec3::new(0.0, -50.0, 0.0));
    assert_eq!(t3.translation, Vec3::new(60.0, -50.0, 0.0));
}

#[test]
fn test_deck_rolodex_cascade_and_paging() {
    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("carrel_root");
    let deck_entity = scene.spawn_deck(
        root,
        "agent_deck",
        Deck::new()
            .with_z_pitch(20.0)
            .with_crest_offset(3.0, 5.0),
    );

    let c0 = scene.spawn_child(deck_entity, Transform::IDENTITY, "card0");
    scene.world.entity_mut(c0).insert(DeckItem { index: 0 });
    let c1 = scene.spawn_child(deck_entity, Transform::IDENTITY, "card1");
    scene.world.entity_mut(c1).insert(DeckItem { index: 1 });
    let c2 = scene.spawn_child(deck_entity, Transform::IDENTITY, "card2");
    scene.world.entity_mut(c2).insert(DeckItem { index: 2 });

    scene.update_transforms();

    // Initial state: Card 0 is active (at 0,0,0)
    let t0 = scene.world.get::<Transform>(c0).unwrap();
    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    assert_eq!(t0.translation, Vec3::ZERO);
    assert_eq!(t1.translation, Vec3::new(3.0, 5.0, -20.0));
    assert_eq!(t2.translation, Vec3::new(6.0, 10.0, -40.0));

    // Advance to page 1
    assert!(scene.deck_next_page(deck_entity));
    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(c0).unwrap();
    let t1 = scene.world.get::<Transform>(c1).unwrap();
    let t2 = scene.world.get::<Transform>(c2).unwrap();

    // Card 1 is now active at 0,0,0
    assert_eq!(t1.translation, Vec3::ZERO);
    // Card 2 is cascading into -Z
    assert_eq!(t2.translation, Vec3::new(3.0, 5.0, -20.0));
    // Card 0 wraps around to the back of the Rolodex cascade
    assert_eq!(t0.translation, Vec3::new(6.0, 10.0, -40.0));

    // Retreat to page 0
    assert!(scene.deck_prev_page(deck_entity));
    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(c0).unwrap();
    assert_eq!(t0.translation, Vec3::ZERO);
}

#[test]
fn test_deck_splay_mode_unfurl() {
    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("carrel_root");
    let deck_entity = scene.spawn_deck(
        root,
        "agent_deck",
        Deck::new()
            .with_mode(DeckMode::Splay)
            .with_splay_columns(2),
    );

    let items: Vec<Entity> = (0..4)
        .map(|i| {
            let e = scene.spawn_child(deck_entity, Transform::IDENTITY, format!("page_{i}"));
            scene.world.entity_mut(e).insert((
                DeckItem { index: i },
                LocalBounds {
                    min: [0.0, -40.0, 0.0],
                    max: [50.0, 0.0, 0.0],
                },
            ));
            e
        })
        .collect();

    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(items[0]).unwrap();
    let t1 = scene.world.get::<Transform>(items[1]).unwrap();
    let t2 = scene.world.get::<Transform>(items[2]).unwrap();

    // Item 0 is active (lifted along +Z)
    assert_eq!(t0.translation.z, 8.0); // default splay_lift
    assert_eq!(t1.translation.z, 0.0);

    // Item 0 and 1 are in same row across X
    assert_eq!(t0.translation.y, t1.translation.y);
    assert!(t1.translation.x > t0.translation.x);

    // Item 2 is in next row down
    assert!(t2.translation.y < t0.translation.y);
}

#[test]
fn test_agent_turn_card_spawning_and_bounds() {
    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("root");
    let card = scene.spawn_agent_turn_card(
        root,
        0,
        0,
        0,
        [60.0, 40.0],
        4.0,
        "Turn 0: Inspecting repo",
        None,
    );

    scene.update_transforms();

    let turn_comp = scene.world.get::<AgentTurnCard>(card).unwrap();
    assert_eq!(turn_comp.turn_index, 0);
    assert_eq!(turn_comp.page_size, [60.0, 40.0]);
    assert_eq!(turn_comp.spine_gap, 4.0);

    // Check overall bounds enclosing both pages and spine gap:
    // Width: 2 * 60 + 4 = 124. Min X: -62, Max X: 62. Min Y: -40, Max Y: 0.
    let bounds = scene.world.get::<LocalBounds>(card).unwrap();
    assert_eq!(bounds.min, [-62.0, -40.0, 0.0]);
    assert_eq!(bounds.max, [62.0, 0.0, 0.0]);

    // Check Left Page (Mind)
    let left_tf = scene.world.get::<Transform>(turn_comp.left_page).unwrap();
    assert_eq!(left_tf.translation.x, -32.0); // -(30 + 2)
    let left_kind = scene.world.get::<TurnPageKind>(turn_comp.left_page).unwrap();
    assert_eq!(*left_kind, TurnPageKind::Mind);

    // Check Right Page (Impact)
    let right_tf = scene.world.get::<Transform>(turn_comp.right_page).unwrap();
    assert_eq!(right_tf.translation.x, 32.0); // +(30 + 2)
    let right_kind = scene.world.get::<TurnPageKind>(turn_comp.right_page).unwrap();
    assert_eq!(*right_kind, TurnPageKind::Impact);
}

#[test]
fn test_workdesk_file_revisions_z_stack() {
    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("desk_root");
    let desk = scene.spawn_workdesk(
        root,
        "agent_workdesk",
        [30.0, 30.0],
        12.0,
    );

    // Push 3 revisions to file A
    let r0 = scene.workdesk_push_revision(
        desk,
        "src/main.rs",
        FileActionKind::Read,
        [50.0, 30.0],
        "Initial inspection",
    );
    let r1 = scene.workdesk_push_revision(
        desk,
        "src/main.rs",
        FileActionKind::Edit,
        [50.0, 30.0],
        "Modify CLI arguments",
    );
    let r2 = scene.workdesk_push_revision(
        desk,
        "src/main.rs",
        FileActionKind::Write,
        [50.0, 30.0],
        "Save updated main.rs",
    );

    scene.update_transforms();

    // Revisions r0, r1, r2 must cascade along -Z inside the file stack
    let t0 = scene.world.get::<Transform>(r0).unwrap();
    let t1 = scene.world.get::<Transform>(r1).unwrap();
    let t2 = scene.world.get::<Transform>(r2).unwrap();

    // Initial state: Revision 0 is active at front (z = 0)
    assert_eq!(t0.translation.z, 0.0);
    assert_eq!(t1.translation.z, -12.0);
    assert_eq!(t2.translation.z, -24.0);

    let desk_comp = scene.world.get::<Workdesk>(desk).unwrap();
    let stack_e = *desk_comp.file_stacks.get("src/main.rs").unwrap();
    let stack_comp = scene.world.get::<FileRevisionStack>(stack_e).unwrap();
    assert_eq!(stack_comp.revision_count, 3);
    assert_eq!(stack_comp.active_revision, 0);

    // Dynamically shift active revision to revision 2: r2 moves to front (z = 0)
    scene.world.get_mut::<FileRevisionStack>(stack_e).unwrap().active_revision = 2;
    scene.update_transforms();

    let t0 = scene.world.get::<Transform>(r0).unwrap();
    let t1 = scene.world.get::<Transform>(r1).unwrap();
    let t2 = scene.world.get::<Transform>(r2).unwrap();
    assert_eq!(t2.translation.z, 0.0);
    assert_eq!(t0.translation.z, -12.0);
    assert_eq!(t1.translation.z, -24.0);
}

#[test]
fn test_agent_carrel_spawning_and_turn_navigation() {
    use crate::agent_transcript::claude::parse_claude_session;
    use crate::revision::RevisionEngine;
    use serde_json::json;

    let transcript = vec![
        // Turn 0
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Turn 0 prompt" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "foo.rs", "content": "base" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "foo.rs", "content": "base" },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1" }] }
        }).to_string(),
        // Turn 1
        json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": "Turn 1 prompt" }] }
        }).to_string(),
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t2", "name": "Edit", "input": { "file_path": "foo.rs", "old_string": "base", "new_string": "updated" } }
                ]
            }
        }).to_string(),
        json!({
            "type": "user",
            "toolUseResult": { "filePath": "foo.rs", "structuredPatch": [{ "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-base", "+updated"] }] },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t2" }] }
        }).to_string(),
    ].join("\n");

    let session = parse_claude_session(&transcript, "carrel_sess");
    let mut rev_engine = RevisionEngine::new();
    rev_engine.ingest_session(&session);

    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("canvas");
    let carrel = scene.spawn_agent_carrel(root, &session, &rev_engine);

    scene.update_transforms();

    let carrel_comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
    assert_eq!(carrel_comp.turn_count, 2);
    // Initial state: latest turn is at front/slot 0
    assert_eq!(carrel_comp.active_turn, 1);

    let desk = scene.world.get::<Workdesk>(carrel_comp.workdesk_entity).unwrap();
    let stack_e = *desk.file_stacks.get("foo.rs").unwrap();
    let stack = scene.world.get::<FileRevisionStack>(stack_e).unwrap();
    assert_eq!(stack.active_revision, 1);

    // Navigate back to turn 0
    let prev = scene.carrel_prev_turn(carrel, &session, &rev_engine);
    assert_eq!(prev, 0);

    let stack_prev = scene.world.get::<FileRevisionStack>(stack_e).unwrap();
    assert_eq!(stack_prev.active_revision, 0);

    // Advance to turn 1
    let next = scene.carrel_next_turn(carrel, &session, &rev_engine);
    assert_eq!(next, 1);

    let stack_after = scene.world.get::<FileRevisionStack>(stack_e).unwrap();
    assert_eq!(stack_after.active_revision, 1);
}

#[test]
fn test_agent_carrel_sliding_window_and_time_scroll() {
    use crate::agent_transcript::claude::parse_claude_session;
    use crate::revision::RevisionEngine;
    use crate::spatial_scene::agent_carrel::CarrelLayoutOptions;
    use serde_json::json;

    // Build a session with 10 turns
    let mut turns_json = Vec::new();
    for i in 0..10 {
        turns_json.push(json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": format!("Turn {i} prompt") }] }
        }).to_string());
        turns_json.push(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": format!("t{i}"), "name": "Write", "input": { "file_path": "a.rs", "content": format!("v{i}") } }
                ]
            }
        }).to_string());
        turns_json.push(json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "a.rs", "content": format!("v{i}") },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": format!("t{i}") }] }
        }).to_string());
    }

    let session = parse_claude_session(&turns_json.join("\n"), "sliding_sess");
    let mut rev_engine = RevisionEngine::new();
    rev_engine.ingest_session(&session);

    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("canvas");

    // Test with window limit = 4, scroll offset K = 0 (latest)
    let opts = CarrelLayoutOptions {
        deck_window_limit: 4,
        deck_scroll_offset: 0,
        desk_revision_limit: 4,
        desk_scroll_offset: 0,
        max_file_stacks: 10,
        active_beat: None,
    };

    let carrel = scene.spawn_agent_carrel_with_options(root, &session, &rev_engine, opts);
    let carrel_comp = scene.world.get::<AgentCarrel>(carrel).unwrap();

    // 1. Sliding window should contain exactly 4 cards
    assert_eq!(carrel_comp.slot_to_beat.len(), 4);
    // 2. Slot 0 must be the newest beat (beat index 19 out of 20)
    assert_eq!(carrel_comp.slot_to_beat[0], 19);
    assert_eq!(carrel_comp.slot_to_beat[1], 18);
    assert_eq!(carrel_comp.slot_to_beat[2], 17);
    assert_eq!(carrel_comp.slot_to_beat[3], 16);
    assert_eq!(carrel_comp.active_beat, 19);

    // 3. Test scrolling back in time: K = 2 (scroll 2 beats back)
    let opts_scrolled = CarrelLayoutOptions {
        deck_window_limit: 4,
        deck_scroll_offset: 2,
        desk_revision_limit: 4,
        desk_scroll_offset: 0,
        max_file_stacks: 10,
        active_beat: Some(15),
    };
    let carrel_scrolled = scene.spawn_agent_carrel_with_options(root, &session, &rev_engine, opts_scrolled);
    let scrolled_comp = scene.world.get::<AgentCarrel>(carrel_scrolled).unwrap();
    assert_eq!(scrolled_comp.slot_to_beat.len(), 4);
    // Beats 19 and 18 are popped off; newest is now 19 - 2 = 17!
    assert_eq!(scrolled_comp.slot_to_beat[0], 17);
    assert_eq!(scrolled_comp.slot_to_beat[1], 16);
    assert_eq!(scrolled_comp.slot_to_beat[2], 15);
    assert_eq!(scrolled_comp.slot_to_beat[3], 14);
    // Focused active beat is 15 (slot 2)
    assert_eq!(scrolled_comp.active_beat, 15);
}

#[test]
fn test_agent_carrel_page_aware_continuous_navigation() {
    use crate::agent_transcript::claude::parse_claude_session;
    use crate::revision::RevisionEngine;
    use crate::spatial_scene::agent_carrel::CarrelLayoutOptions;
    use serde_json::json;

    let mut turns_json = Vec::new();
    for i in 0..12 {
        turns_json.push(json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": format!("Turn {i}") }] }
        }).to_string());
        turns_json.push(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": format!("t{i}"), "name": "Write", "input": { "file_path": "a.rs", "content": format!("v{i}") } }
                ]
            }
        }).to_string());
        turns_json.push(json!({
            "type": "user",
            "toolUseResult": { "type": "create", "filePath": "a.rs", "content": format!("v{i}") },
            "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": format!("t{i}") }] }
        }).to_string());
    }

    let session = parse_claude_session(&turns_json.join("\n"), "paging_sess");
    let mut rev_engine = RevisionEngine::new();
    rev_engine.ingest_session(&session);

    let mut scene = SpatialScene::new();
    let root = scene.spawn_root("canvas");

    let total_items = session.linearize_events(Some(&rev_engine)).len();
    assert_eq!(total_items, 24);
    let limit = 4usize;
    let initial_beat = total_items - 1; // 23

    // Initial window: beats 23, 22, 21, 20 (k = 0)
    let carrel = scene.spawn_agent_carrel_with_options(
        root,
        &session,
        &rev_engine,
        CarrelLayoutOptions {
            deck_window_limit: limit,
            deck_scroll_offset: 0,
            desk_revision_limit: 4,
            desk_scroll_offset: 0,
            max_file_stacks: 10,
            active_beat: Some(initial_beat),
        },
    );
    let comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
    assert_eq!(comp.slot_to_beat, vec![23, 22, 21, 20]);
    assert_eq!(comp.active_beat, 23);

    // Verify all 24 cards were spawned into ECS
    assert_eq!(comp.all_card_entities.len(), 24);

    // Simulate backward navigation through all beats: 23 -> 0 in O(1) without rebuilding
    let mut visited_slots = Vec::new();
    for _ in 0..total_items - 1 {
        let beat = scene.carrel_step_prev(carrel, &session, &rev_engine);
        let comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
        let slot = comp.slot_to_beat.iter().position(|&b| b == beat).unwrap();
        visited_slots.push((beat, slot));
    }

    // Verify all beats were visited and always occupied a valid slot (0..4)
    assert_eq!(visited_slots.len(), 23);
    for &(beat, slot) in &visited_slots {
        assert!(slot < limit, "beat {beat} mapped to invalid slot {slot}");
    }
    // Final beat should be 0, at slot 3 of the oldest window (beats [3, 2, 1, 0])
    assert_eq!(visited_slots.last().unwrap(), &(0, 3));
    let comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
    assert_eq!(comp.slot_to_beat, vec![3, 2, 1, 0]);

    // Now simulate forward navigation back from 0 -> 23 in O(1)
    let mut forward_slots = Vec::new();
    for _ in 0..total_items - 1 {
        let beat = scene.carrel_step_next(carrel, &session, &rev_engine);
        let comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
        let slot = comp.slot_to_beat.iter().position(|&b| b == beat).unwrap();
        forward_slots.push((beat, slot));
    }
    assert_eq!(forward_slots.len(), 23);
    assert_eq!(forward_slots.last().unwrap(), &(23, 0));
    let comp = scene.world.get::<AgentCarrel>(carrel).unwrap();
    assert_eq!(comp.slot_to_beat, vec![23, 22, 21, 20]);
}
