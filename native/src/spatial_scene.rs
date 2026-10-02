//! ECS-driven 3D spatial computing scene graph.
//!
//! Powered by `bevy_ecs` and `bevy_transform`. Manages all scene entities
//! (code file cards, container plates, shelves, carrel zones, terminals, cameras)
//! as first-class ECS entities with true 3D affine transforms, automatic hierarchical
//! forward propagation, OBB raycasting, and decoupled render extraction.

use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use bevy_transform::systems::{
    mark_dirty_trees, propagate_parent_transforms, sync_simple_transforms,
    StaticTransformOptimizations,
};
use crate::glyph_scene::mesh::MeshInstance;

mod query;
mod spawner;
mod sync;
pub mod alignment;
pub mod deck;
pub mod turn_card;
pub mod workdesk;

pub use alignment::{
    AlignmentAxis, HorizontalAlign, SpatialAlignment, VerticalAlign, WrapConstraint,
};
pub use deck::{Deck, DeckItem, DeckMode};
pub use turn_card::{AgentTurnCard, TurnPageKind};
pub use workdesk::{FileActionKind, FileRevisionCard, FileRevisionStack, Workdesk};

/// Type of 3D primitive geometry for a mesh instance.
#[derive(Component, Debug, Clone, PartialEq)]
pub enum SceneMeshKind {
    /// Planar rectangular unit quad scaled by `size` and offset by `origin` (local anchor).
    Quad { size: [f32; 2], origin: [f32; 2] },
    /// 3D box with total extents [w, h, d] centered at origin.
    Box { extents: [f32; 3] },
}

/// Shading material parameters for a scene mesh.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct SceneMeshMaterial {
    /// Linear RGBA color tint.
    pub color: [f32; 4],
    /// Optional material parameters [metallic, roughness, emissive, flags].
    pub params: [f32; 4],
}

impl Default for SceneMeshMaterial {
    fn default() -> Self {
        Self {
            color: [1.0, 1.0, 1.0, 1.0],
            params: [0.0, 0.0, 0.0, 0.0],
        }
    }
}

/// Entity component marking a Slug glyph text canvas bound to a GPU `GroupRow` slot.
#[derive(Component, Debug, Clone)]
pub struct GlyphGroupBinding {
    pub group_id: u32,
    pub tint: [f32; 4],
}

/// Entity component marking a code file representation.
#[derive(Component, Debug, Clone)]
pub struct FileCard {
    pub rel_path: String,
    pub dir: String,
    pub group_id: u32,
}

/// Entity component marking a layout container zone (e.g. shelf or carrel).
#[derive(Component, Debug, Clone)]
pub struct LayoutZoneEntity {
    pub zone_id: String,
    pub title: String,
}

/// Local 3D bounding box for ray hit-testing and collision: [min, max].
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct LocalBounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// Visibility flag. If missing, entity is assumed visible.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visible(pub bool);

/// Extracted scene mesh instances ready for GPU dispatch.
#[derive(Default, Debug, Clone)]
pub struct SceneMeshDraws {
    pub revision: u64,
    pub quads: Vec<MeshInstance>,
    pub cubes: Vec<MeshInstance>,
}

/// Primary 3D Spatial Computing Scene, backed by Bevy ECS.
pub struct SpatialScene {
    pub world: World,
    transform_schedule: Schedule,
    cached_mesh_draws: SceneMeshDraws,
}

impl Default for SpatialScene {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialScene {
    pub fn new() -> Self {
        let mut world = World::new();
        world.init_resource::<StaticTransformOptimizations>();

        let mut transform_schedule = Schedule::default();
        transform_schedule.add_systems(
            (
                mark_dirty_trees,
                propagate_parent_transforms,
                sync_simple_transforms,
            )
                .chain(),
        );

        Self {
            world,
            transform_schedule,
            cached_mesh_draws: SceneMeshDraws::default(),
        }
    }

    /// Run transform propagation with optional delta-time easing for dynamic transitions.
    pub fn update_transforms_animated(&mut self, dt: Option<f32>) {
        self.apply_spatial_alignments();
        self.apply_deck_layouts(dt);
        self.transform_schedule.run(&mut self.world);
        self.cached_mesh_draws = self.extract_mesh_instances();
    }

    /// Run transform propagation and update pre-extracted mesh draw cache (instant snap).
    pub fn update_transforms(&mut self) {
        self.update_transforms_animated(None);
    }
}

#[cfg(test)]
mod tests {
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
        // Card 0 is past, tucked to the left/back
        assert!(t0.translation.x < 0.0);
        assert!(t0.translation.z > 0.0);
        assert_ne!(t0.rotation, Quat::IDENTITY);

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
        assert_eq!(t0.translation.x, 0.0);
        assert_eq!(t0.translation.y, 0.0);
        assert_eq!(t0.translation.z, 8.0); // default splay_lift

        // Item 1 is in same row, displaced across X
        assert!(t1.translation.x > 50.0);
        assert_eq!(t1.translation.y, 0.0);
        assert_eq!(t1.translation.z, 0.0);

        // Item 2 is in next row down
        assert_eq!(t2.translation.x, 0.0);
        assert!(t2.translation.y < -40.0);
    }

    #[test]
    fn test_agent_turn_card_spawning_and_bounds() {
        let mut scene = SpatialScene::new();
        let root = scene.spawn_root("root");
        let card = scene.spawn_agent_turn_card(
            root,
            0,
            [60.0, 40.0],
            4.0,
            "Turn 0: Inspecting repo",
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

        assert_eq!(t0.translation.z, 0.0);
        assert_eq!(t1.translation.z, -12.0);
        assert_eq!(t2.translation.z, -24.0);

        // Stack count and active revision should be 3 and 2
        let desk_comp = scene.world.get::<Workdesk>(desk).unwrap();
        let stack_e = *desk_comp.file_stacks.get("src/main.rs").unwrap();
        let stack_comp = scene.world.get::<FileRevisionStack>(stack_e).unwrap();
        assert_eq!(stack_comp.revision_count, 3);
        assert_eq!(stack_comp.active_revision, 2);
    }
}
