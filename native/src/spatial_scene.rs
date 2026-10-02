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

pub use alignment::{
    AlignmentAxis, HorizontalAlign, SpatialAlignment, VerticalAlign, WrapConstraint,
};

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

    /// Run transform propagation and update pre-extracted mesh draw cache.
    pub fn update_transforms(&mut self) {
        self.apply_spatial_alignments();
        self.transform_schedule.run(&mut self.world);
        self.cached_mesh_draws = self.extract_mesh_instances();
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
}
