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
use glam::{Affine3A, Quat, Vec3};

use crate::glyph_scene::mesh::MeshInstance;
use crate::glyph_scene::GroupRow;

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
        self.transform_schedule.run(&mut self.world);
        self.cached_mesh_draws = self.extract_mesh_instances();
    }

    /// Spawn a root organizational transform entity.
    pub fn spawn_root(&mut self, name: impl Into<String>) -> Entity {
        self.world
            .spawn((
                Transform::IDENTITY,
                Name::new(name.into()),
            ))
            .id()
    }

    /// Spawn a child entity parented to `parent`.
    pub fn spawn_child(
        &mut self,
        parent: Entity,
        transform: Transform,
        name: impl Into<String>,
    ) -> Entity {
        self.world
            .spawn((
                transform,
                ChildOf(parent),
                Name::new(name.into()),
            ))
            .id()
    }

    /// Spawn a layout container zone entity.
    pub fn spawn_zone(
        &mut self,
        parent: Entity,
        zone_id: impl Into<String>,
        title: impl Into<String>,
        transform: Transform,
        size: [f32; 2],
    ) -> Entity {
        let zid = zone_id.into();
        self.world
            .spawn((
                transform,
                ChildOf(parent),
                Name::new(format!("zone:{}", zid)),
                LayoutZoneEntity {
                    zone_id: zid,
                    title: title.into(),
                },
                LocalBounds {
                    min: [0.0, -size[1], 0.0],
                    max: [size[0], 0.0, 0.0],
                },
            ))
            .id()
    }

    /// Spawn a background container plate mesh entity.
    pub fn spawn_plate(
        &mut self,
        parent: Entity,
        name: impl Into<String>,
        transform: Transform,
        size: [f32; 2],
        origin: [f32; 2],
        color: [f32; 4],
    ) -> Entity {
        self.world
            .spawn((
                transform,
                ChildOf(parent),
                Name::new(name.into()),
                SceneMeshKind::Quad { size, origin },
                SceneMeshMaterial {
                    color,
                    params: [0.0, 0.0, 0.0, 0.0],
                },
                LocalBounds {
                    min: [origin[0], origin[1], 0.0],
                    max: [origin[0] + size[0], origin[1] + size[1], 0.0],
                },
            ))
            .id()
    }

    /// Spawn a code file card entity with glyph group binding.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_file_card(
        &mut self,
        parent: Entity,
        rel_path: impl Into<String>,
        dir: impl Into<String>,
        group_id: u32,
        transform: Transform,
        local_bounds: ([f32; 3], [f32; 3]),
        tint: [f32; 3],
    ) -> Entity {
        let path_str = rel_path.into();
        self.world
            .spawn((
                transform,
                ChildOf(parent),
                Name::new(path_str.clone()),
                FileCard {
                    rel_path: path_str,
                    dir: dir.into(),
                    group_id,
                },
                GlyphGroupBinding {
                    group_id,
                    tint: [tint[0], tint[1], tint[2], 1.0],
                },
                LocalBounds {
                    min: local_bounds.0,
                    max: local_bounds.1,
                },
            ))
            .id()
    }

    /// Attach an entity as a child of another entity.
    pub fn attach_child(&mut self, parent: Entity, child: Entity) {
        if parent == child {
            log::warn!("spatial_scene: cannot parent entity {:?} to itself", child);
            return;
        }
        self.world.entity_mut(child).insert(ChildOf(parent));
    }

    /// Detach an entity from its parent.
    pub fn detach(&mut self, entity: Entity) {
        self.world.entity_mut(entity).remove::<ChildOf>();
    }

    /// Compute world-space axis-aligned bounding box (AABB) for an entity and its subtree.
    pub fn world_bounds(&self, entity: Entity) -> Option<([f32; 3], [f32; 3])> {
        let gtf = self.world.get::<GlobalTransform>(entity)?;
        let mut world_min = Vec3::splat(f32::INFINITY);
        let mut world_max = Vec3::splat(f32::NEG_INFINITY);
        let mut has_bounds = false;

        let local_bounds = self.world.get::<LocalBounds>(entity).copied().or_else(|| {
            self.world.get::<SceneMeshKind>(entity).map(|mesh| match mesh {
                SceneMeshKind::Quad { size, origin } => LocalBounds {
                    min: [origin[0], origin[1], 0.0],
                    max: [origin[0] + size[0], origin[1] + size[1], 0.0],
                },
                SceneMeshKind::Box { extents } => {
                    let hx = extents[0] * 0.5;
                    let hy = extents[1] * 0.5;
                    let hz = extents[2] * 0.5;
                    LocalBounds {
                        min: [-hx, -hy, -hz],
                        max: [hx, hy, hz],
                    }
                }
            })
        });

        if let Some(b) = local_bounds {
            let corners = [
                Vec3::new(b.min[0], b.min[1], b.min[2]),
                Vec3::new(b.max[0], b.min[1], b.min[2]),
                Vec3::new(b.min[0], b.max[1], b.min[2]),
                Vec3::new(b.max[0], b.max[1], b.min[2]),
                Vec3::new(b.min[0], b.min[1], b.max[2]),
                Vec3::new(b.max[0], b.min[1], b.max[2]),
                Vec3::new(b.min[0], b.max[1], b.max[2]),
                Vec3::new(b.max[0], b.max[1], b.max[2]),
            ];
            for c in corners {
                let wp = gtf.transform_point(c);
                world_min = world_min.min(wp);
                world_max = world_max.max(wp);
            }
            has_bounds = true;
        }

        if let Some(children) = self.world.get::<bevy_ecs::hierarchy::Children>(entity) {
            for child in children.iter() {
                if let Some((c_min, c_max)) = self.world_bounds(child) {
                    world_min = world_min.min(Vec3::from_array(c_min));
                    world_max = world_max.max(Vec3::from_array(c_max));
                    has_bounds = true;
                }
            }
        }

        if has_bounds {
            Some((world_min.to_array(), world_max.to_array()))
        } else {
            None
        }
    }

    /// Return the pre-extracted scene mesh instances ready for GPU dispatch.
    pub fn collect_mesh_instances(&self) -> SceneMeshDraws {
        self.cached_mesh_draws.clone()
    }

    /// Return a borrowed reference to the pre-extracted scene mesh instances.
    pub fn mesh_draws(&self) -> &SceneMeshDraws {
        &self.cached_mesh_draws
    }

    /// Extract all visible mesh instances (`Quad` and `Box`) into GPU-ready draw buffers.
    pub fn extract_mesh_instances(&mut self) -> SceneMeshDraws {
        let mut mesh_changed = false;
        let mut change_query = self.world.query_filtered::<
            (),
            (
                With<SceneMeshKind>,
                Or<(
                    Changed<GlobalTransform>,
                    Changed<SceneMeshMaterial>,
                    Changed<Visible>,
                )>,
            ),
        >();
        if change_query.iter(&self.world).next().is_some() {
            mesh_changed = true;
        }

        if !mesh_changed {
            return self.cached_mesh_draws.clone();
        }

        let mut draws = SceneMeshDraws::default();

        let mut query = self.world.query::<(
            &GlobalTransform,
            &SceneMeshKind,
            &SceneMeshMaterial,
            Option<&Visible>,
        )>();

        for (gtf, mesh_kind, mat, vis) in query.iter(&self.world) {
            if let Some(v) = vis {
                if !v.0 {
                    continue;
                }
            }

            let parent_affine = gtf.affine();

            match mesh_kind {
                SceneMeshKind::Quad { size, origin } => {
                    let local_affine = Affine3A::from_scale_rotation_translation(
                        Vec3::new(size[0], size[1], 1.0),
                        Quat::IDENTITY,
                        Vec3::new(origin[0] + size[0] * 0.5, origin[1] + size[1] * 0.5, 0.0),
                    );
                    let world_affine = parent_affine * local_affine;
                    draws.quads.push(MeshInstance::from_affine(
                        world_affine,
                        mat.color,
                        mat.params,
                    ));
                }
                SceneMeshKind::Box { extents } => {
                    let local_affine = Affine3A::from_scale(
                        Vec3::new(extents[0], extents[1], extents[2]),
                    );
                    let world_affine = parent_affine * local_affine;
                    draws.cubes.push(MeshInstance::from_affine(
                        world_affine,
                        mat.color,
                        mat.params,
                    ));
                }
            }
        }

        draws
    }

    /// Synchronize all entities with a `GlyphGroupBinding` into the GPU `GroupRow` buffer.
    pub fn sync_to_group_rows(&mut self, groups: &mut [GroupRow]) -> Vec<u32> {
        let mut updated = Vec::new();

        let mut query = self.world.query_filtered::<(
            &GlobalTransform,
            &GlyphGroupBinding,
            Option<&Visible>,
        ), Changed<GlobalTransform>>();

        for (gtf, binding, vis) in query.iter(&self.world) {
            if let Some(v) = vis {
                if !v.0 {
                    continue;
                }
            }

            let idx = binding.group_id as usize;
            if idx < groups.len() {
                let (scale, rotation, translation) = gtf.to_scale_rotation_translation();
                let g = &mut groups[idx];
                g.cols[0] = [translation.x, translation.y, translation.z, 0.0];
                g.cols[1] = [rotation.x, rotation.y, rotation.z, rotation.w];
                g.cols[2] = binding.tint;
                g.cols[3] = [scale.x, scale.y, scale.z, 0.0];
                updated.push(binding.group_id);
            }
        }

        self.world.clear_trackers();

        updated
    }

    /// Raycast against all entities with `LocalBounds` using true Oriented Bounding Box (OBB)
    /// unprojection: unprojects the world ray into entity local space via `world_from_local.inverse()`.
    /// Returns `Some((Entity, world_hit_t))` for the nearest hit entity in front of the ray.
    pub fn raycast_obb(
        &mut self,
        ro: glam::DVec3,
        rd: glam::DVec3,
    ) -> Option<(Entity, f64)> {
        let mut best: Option<(Entity, f64)> = None;

        let mut query = self.world.query::<(
            Entity,
            &GlobalTransform,
            &LocalBounds,
            Option<&Visible>,
        )>();

        for (entity, gtf, bounds, vis) in query.iter(&self.world) {
            if let Some(v) = vis {
                if !v.0 {
                    continue;
                }
            }

            let affine = gtf.affine();
            let inv_affine = affine.inverse();

            // Transform world ray into local space
            let ro_f32 = Vec3::new(ro.x as f32, ro.y as f32, ro.z as f32);
            let rd_f32 = Vec3::new(rd.x as f32, rd.y as f32, rd.z as f32);

            let ro_local = inv_affine.transform_point3(ro_f32);
            let rd_local = inv_affine.transform_vector3(rd_f32);

            let ro_loc_d = glam::DVec3::new(ro_local.x as f64, ro_local.y as f64, ro_local.z as f64);
            let rd_loc_d = glam::DVec3::new(rd_local.x as f64, rd_local.y as f64, rd_local.z as f64);

            let b_min = glam::DVec3::new(bounds.min[0] as f64, bounds.min[1] as f64, bounds.min[2] as f64);
            let b_max = glam::DVec3::new(bounds.max[0] as f64, bounds.max[1] as f64, bounds.max[2] as f64);

            let min = b_min.min(b_max);
            let max = b_min.max(b_max);

            if let Some(t) = ray_aabb_d(ro_loc_d, rd_loc_d, min, max) {
                if best.is_none_or(|(_, bt)| t < bt) {
                    best = Some((entity, t));
                }
            }
        }

        best
    }
}

/// Ray-AABB intersection in double precision.
#[inline]
fn ray_aabb_d(
    ro: glam::DVec3,
    rd: glam::DVec3,
    min: glam::DVec3,
    max: glam::DVec3,
) -> Option<f64> {
    let mut tmin = f64::NEG_INFINITY;
    let mut tmax = f64::INFINITY;

    for i in 0..3 {
        let (origin, dir, bmin, bmax) = match i {
            0 => (ro.x, rd.x, min.x, max.x),
            1 => (ro.y, rd.y, min.y, max.y),
            _ => (ro.z, rd.z, min.z, max.z),
        };

        if dir.abs() < 1e-12 {
            if origin < bmin || origin > bmax {
                return None;
            }
        } else {
            let inv_d = 1.0 / dir;
            let mut t1 = (bmin - origin) * inv_d;
            let mut t2 = (bmax - origin) * inv_d;
            if t1 > t2 {
                core::mem::swap(&mut t1, &mut t2);
            }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmin > tmax {
                return None;
            }
        }
    }

    if tmax < 0.0 {
        return None;
    }

    Some(if tmin >= 0.0 { tmin } else { tmax })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
