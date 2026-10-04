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
mod tests;
