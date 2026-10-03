use bevy_ecs::prelude::*;

use super::{SpatialScene, ChildOf, LocalBounds, Transform, SceneMeshMaterial, GlyphGroupBinding, SceneMeshKind, LayoutZoneEntity, FileCard};

impl SpatialScene {
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
}
