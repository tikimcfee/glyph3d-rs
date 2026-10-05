//! Spatial layout stack and runtime control object.
//!
//! Provides dynamic, composable spatial layout of repository files and visual
//! items across the 3D canvas. Supports runtime-chosen layout strategies (Shelf,
//! Carrel, Column, Row, Pinned), dynamic carrel/zone creation, and runtime
//! file-to-layout assignments driven by users, agents, or LSP/Zed sidecars.

use std::collections::HashMap;

use crate::glyph_scene::GroupRow;
use crate::repo::{FileView, RepoLayoutMode, RepoParams};
use crate::spatial_scene::SpatialScene;
use bevy_ecs::entity::Entity;
use bevy_transform::components::Transform;

/// Strategy for laying out a group of files within its local bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialLayoutStrategy {
    /// Traditional height-classed shelf packing across the group.
    #[default]
    Shelf,
    /// Neighborhood carrel layout with lead-file prioritization.
    Carrel,
    /// Vertical single-column stack of files.
    Column,
    /// Horizontal side-by-side row of files.
    Row,
}

/// Macro packing mode for arranging multiple zones/carrels relative to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MacroArrangement {
    /// Canvas packing with horizontal/vertical avenues aiming for grid_aspect.
    #[default]
    CanvasAvenues,
    /// Horizontal row of carrels.
    LinearRow,
    /// Vertical column of carrels.
    LinearColumn,
}

/// A spatial zone or carrel in the layout stack.
#[derive(Debug, Clone)]
pub struct LayoutZone {
    /// Unique identifier for this zone (e.g. "dir:native/src", "desk:active", "zone-1").
    pub id: String,
    /// Human-readable title for UI display and spatial labels.
    pub title: String,
    /// Local layout strategy for files inside this zone.
    pub strategy: SpatialLayoutStrategy,
    /// Optional manual pin origin [x, y, z] in world space.
    pub pin_origin: Option<[f32; 3]>,
    /// Optional custom tint overriding default directory tint.
    pub custom_tint: Option<[f32; 3]>,
    /// Computed bounding dimensions of the zone after layout: [width, height, depth].
    pub computed_size: [f32; 3],
    /// Computed origin [x, y, z] in world space after macro layout.
    pub computed_origin: [f32; 3],
    /// Optional bound entity in the SpatialScene.
    pub entity: Option<Entity>,
}

impl LayoutZone {
    pub fn new(id: impl Into<String>, title: impl Into<String>, strategy: SpatialLayoutStrategy) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            strategy,
            pin_origin: None,
            custom_tint: None,
            computed_size: [0.0; 3],
            computed_origin: [0.0; 3],
            entity: None,
        }
    }
}

/// The layout stack: holds custom zones, explicit file assignments,
/// and macro-arrangement configuration.
#[derive(Debug, Clone, Default)]
pub struct LayoutStack {
    /// Fallback strategy for unassigned files.
    pub base_strategy: SpatialLayoutStrategy,
    /// Macro arrangement mode for multiple carrels/zones.
    pub macro_arrangement: MacroArrangement,
    /// User or agent defined layout zones.
    pub zones: Vec<LayoutZone>,
    /// Dynamic map from file `rel_path` to target `zone_id`.
    pub file_to_zone: HashMap<String, String>,
    /// Horizontal avenue spacing between zones.
    pub avenue_gap_x: f32,
    /// Vertical avenue spacing between zones.
    pub avenue_gap_y: f32,
}

/// Runtime control object for spatial layout.
///
/// Encapsulates the active layout mode, the layout stack, and repo parameters.
/// Allows users or agents to dynamically query and modify layout assignments
/// and re-evaluate file placements across the canvas.
pub struct LayoutController {
    pub mode: RepoLayoutMode,
    pub stack: LayoutStack,
    pub params: RepoParams,
    pub scene: SpatialScene,
    pub zone_entities: HashMap<String, Entity>,
    pub file_entities: Vec<Entity>,
    pub active_carrel: Option<Entity>,
    pub session: Option<crate::agent_transcript::AgentSession>,
    pub revision_engine: Option<crate::revision::RevisionEngine>,
}

impl std::fmt::Debug for LayoutController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutController")
            .field("mode", &self.mode)
            .field("stack", &self.stack)
            .field("params", &self.params)
            .finish()
    }
}

pub mod algorithm;

impl LayoutController {
    /// Create a controller with default settings from repo parameters.
    pub fn new(params: RepoParams) -> Self {
        Self::from_mode(params.layout_mode, params)
    }

    /// Create a controller for a specific layout mode.
    pub fn from_mode(mode: RepoLayoutMode, params: RepoParams) -> Self {
        let base_strategy = match mode {
            RepoLayoutMode::Shelf => SpatialLayoutStrategy::Shelf,
            RepoLayoutMode::Carrel => SpatialLayoutStrategy::Carrel,
        };
        let avenue_gap_x = (params.gap_x * 4.0).max(12.0);
        let avenue_gap_y = (params.gap_y * 3.0).max(18.0);
        Self {
            mode,
            stack: LayoutStack {
                base_strategy,
                macro_arrangement: MacroArrangement::CanvasAvenues,
                zones: Vec::new(),
                file_to_zone: HashMap::new(),
                avenue_gap_x,
                avenue_gap_y,
            },
            params,
            scene: SpatialScene::new(),
            zone_entities: HashMap::new(),
            file_entities: Vec::new(),
            active_carrel: None,
            session: None,
            revision_engine: None,
        }
    }

    /// Set the global layout mode (e.g. Shelf vs Carrel).
    pub fn set_mode(&mut self, mode: RepoLayoutMode) {
        self.mode = mode;
        self.stack.base_strategy = match mode {
            RepoLayoutMode::Shelf => SpatialLayoutStrategy::Shelf,
            RepoLayoutMode::Carrel => SpatialLayoutStrategy::Carrel,
        };
    }

    /// Dynamically assign a file to a specific zone / carrel.
    pub fn assign_file(&mut self, rel_path: impl Into<String>, zone_id: impl Into<String>) {
        self.stack.file_to_zone.insert(rel_path.into(), zone_id.into());
    }

    /// Remove a file's custom zone assignment, reverting to default grouping.
    pub fn unassign_file(&mut self, rel_path: &str) {
        self.stack.file_to_zone.remove(rel_path);
    }

    /// Create or return a mutable reference to a layout zone.
    pub fn get_or_create_zone(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        strategy: SpatialLayoutStrategy,
    ) -> &mut LayoutZone {
        let id_str = id.into();
        if let Some(pos) = self.stack.zones.iter().position(|z| z.id == id_str) {
            return &mut self.stack.zones[pos];
        }
        self.stack.zones.push(LayoutZone::new(id_str, title, strategy));
        self.stack.zones.last_mut().expect("just pushed")
    }

    /// Remove a zone by ID and unassign any files pointing to it.
    pub fn remove_zone(&mut self, zone_id: &str) {
        self.stack.zones.retain(|z| z.id != zone_id);
        self.stack.file_to_zone.retain(|_, zid| zid != zone_id);
    }

    /// Query which zone ID a file belongs to.
    pub fn zone_for_file(&self, rel_path: &str, dir: &str) -> String {
        if let Some(zid) = self.stack.file_to_zone.get(rel_path) {
            return zid.clone();
        }
        match self.mode {
            RepoLayoutMode::Shelf => "base:shelf".to_string(),
            RepoLayoutMode::Carrel => format!("dir:{dir}"),
        }
    }

    /// Translate a zone by a delta vector. Automatically propagates down
    /// to all contained files and sheets in that zone.
    pub fn move_zone(&mut self, zone_id: &str, delta: glam::Vec3) -> bool {
        if let Some(&entity) = self.zone_entities.get(zone_id) {
            if let Some(mut transform) = self.scene.world.get_mut::<Transform>(entity) {
                transform.translation += delta;
            }
            self.scene.update_transforms();
            true
        } else {
            false
        }
    }

    /// Scale a zone by a factor. Automatically propagates down to all children.
    pub fn scale_zone(&mut self, zone_id: &str, factor: f32) -> bool {
        if let Some(&entity) = self.zone_entities.get(zone_id) {
            if let Some(mut transform) = self.scene.world.get_mut::<Transform>(entity) {
                transform.scale *= factor;
            }
            self.scene.update_transforms();
            true
        } else {
            false
        }
    }

    /// Retrieve the world-space bounding box for a zone: [min, max].
    pub fn zone_world_bounds(&self, zone_id: &str) -> Option<([f32; 3], [f32; 3])> {
        let entity = *self.zone_entities.get(zone_id)?;
        self.scene.world_bounds(entity)
    }

    /// Retrieve the world-space translation for a zone entity.
    pub fn zone_world_translation(&self, zone_id: &str) -> Option<[f32; 3]> {
        let entity = *self.zone_entities.get(zone_id)?;
        let gtf = self.scene.world.get::<bevy_transform::prelude::GlobalTransform>(entity)?;
        Some(gtf.translation().into())
    }

    /// Retrieve the world-space bounding box for a file view by index: [min, max].
    pub fn file_world_bounds(&self, file_idx: usize) -> Option<([f32; 3], [f32; 3])> {
        let entity = *self.file_entities.get(file_idx)?;
        self.scene.world_bounds(entity)
    }

    /// Synchronize all spatial hierarchy nodes into the GPU GroupRow buffer.
    /// Returns the list of updated group IDs.
    pub fn sync_gpu_groups(&mut self, groups: &mut [GroupRow]) -> Vec<u32> {
        self.scene.sync_to_group_rows(groups)
    }

    /// Populate the SpatialScene for Shelf layout mode, so that file nodes
    /// and the base shelf zone exist for picking, dragging, and bounds queries.
    pub fn build_shelf_hierarchy(&mut self, views: &[FileView]) {
        self.scene = SpatialScene::new();
        self.zone_entities.clear();
        self.file_entities = vec![Entity::PLACEHOLDER; views.len()];

        let stack_root = self.scene.spawn_root("stack_root");
        let shelf_zone_node = self.scene.spawn_zone(
            stack_root,
            "base:shelf",
            "Shelf",
            Transform::IDENTITY,
            [0.0, 0.0],
        );
        self.zone_entities.insert("base:shelf".to_string(), shelf_zone_node);

        for (i, v) in views.iter().enumerate() {
            let file_node = self.scene.spawn_file_card(
                shelf_zone_node,
                v.rel_path.clone(),
                v.dir.clone(),
                i as u32,
                Transform::from_xyz(v.offset[0], v.offset[1], v.offset[2]),
                ([0.0, -v.height, v.z_min], [v.width, 0.0, v.z_max]),
                crate::repo::dir_tint(&v.dir),
            );
            self.file_entities[i] = file_node;
        }
        self.scene.update_transforms();
    }

    /// Check if an active deck exists in the spatial scene. If not, returns None.
    pub fn active_deck_entity(&mut self) -> Option<Entity> {
        let mut query = self.scene.world.query::<(Entity, &crate::spatial_scene::Deck)>();
        query.iter(&self.scene.world).map(|(e, _)| e).next()
    }

    /// Advance active page in the primary deck. Returns a status message if successful.
    pub fn deck_next(&mut self) -> Option<String> {
        let deck_e = self.active_deck_entity()?;
        if self.scene.deck_next_page(deck_e) {
            self.scene.update_transforms();
            let deck = self.scene.world.get::<crate::spatial_scene::Deck>(deck_e)?;
            Some(format!("deck: turned page to {} in mode {:?}", deck.active_index, deck.mode))
        } else {
            None
        }
    }

    /// Retreat active page in the primary deck. Returns a status message if successful.
    pub fn deck_prev(&mut self) -> Option<String> {
        let deck_e = self.active_deck_entity()?;
        if self.scene.deck_prev_page(deck_e) {
            self.scene.update_transforms();
            let deck = self.scene.world.get::<crate::spatial_scene::Deck>(deck_e)?;
            Some(format!("deck: turned page to {} in mode {:?}", deck.active_index, deck.mode))
        } else {
            None
        }
    }

    /// Toggle Deck mode between Rolodex cascade (Deck) and overview grid (Splay).
    pub fn deck_toggle_mode(&mut self) -> Option<String> {
        let deck_e = self.active_deck_entity()?;
        let new_mode = self.scene.deck_toggle_mode(deck_e)?;
        self.scene.update_transforms();
        Some(format!("deck: toggled mode to {:?}", new_mode))
    }

    /// Spawn or focus an Agent Carrel with Turn Deck and Workdesk for interactive spatial experimentation.
    pub fn spawn_agent_carrel_demo(&mut self) -> String {
        use crate::spatial_scene::{Deck, FileActionKind};

        // If the agent carrel demo already exists, do not duplicate nodes!
        // Instead, reset the deck active page to 0 in Rolodex mode.
        if let Some(&_carrel_zone) = self.zone_entities.get("agent:carrel") {
            if let Some(deck_e) = self.active_deck_entity() {
                if let Some(mut deck) = self.scene.world.get_mut::<Deck>(deck_e) {
                    deck.active_index = 0;
                    deck.mode = crate::spatial_scene::DeckMode::Deck;
                }
                self.scene.update_transforms();
                return "Agent Carrel demo already exists — reset to Turn 0 (Rolodex mode). Use [ / ] or n / p to turn pages, v to splay.".to_string();
            }
        }

        let carrel_root = self.scene.spawn_root("agent_carrel_root");
        if let Some(mut tf) = self.scene.world.get_mut::<Transform>(carrel_root) {
            tf.translation = glam::Vec3::new(0.0, 50.0, 10.0);
        }
        let carrel_zone = self.scene.spawn_zone(
            carrel_root,
            "agent:carrel",
            "Agent Study Carrel",
            Transform::IDENTITY,
            [280.0, 160.0],
        );
        self.zone_entities.insert("agent:carrel".to_string(), carrel_zone);

        // 1. Deck with 4 2-page Turn Cards
        let deck = self.scene.spawn_deck(
            carrel_zone,
            "turn_deck",
            Deck::new()
                .with_z_pitch(22.0)
                .with_crest_offset(3.0, 4.0)
                .with_splay_columns(2),
        );

        let titles = [
            "Turn 0: Scan repository & plan spatial refactor",
            "Turn 1: Modularize glyph_scene and spatial_scene",
            "Turn 2: Implement SpatialAlignment and Splay grid",
            "Turn 3: Construct Deck and 2-page Turn Cards",
        ];

        for (i, title) in titles.iter().enumerate() {
            self.scene.spawn_agent_turn_card(
                deck,
                i,
                i,
                i,
                [55.0, 36.0],
                4.0,
                *title,
                None,
            );
        }

        // 2. Workdesk for touched files placed adjacent along X
        let workdesk = self.scene.spawn_workdesk(
            carrel_zone,
            "agent_touched_files",
            [40.0, 40.0],
            14.0,
        );
        if let Some(mut tf) = self.scene.world.get_mut::<Transform>(workdesk) {
            tf.translation.x = 135.0;
        }

        // Revisions for main.rs
        self.scene.workdesk_push_revision(
            workdesk,
            "src/main.rs",
            FileActionKind::Read,
            [35.0, 24.0],
            "inspect cli",
        );
        self.scene.workdesk_push_revision(
            workdesk,
            "src/main.rs",
            FileActionKind::Edit,
            [35.0, 24.0],
            "add deck verbs",
        );

        // Revisions for spatial_scene.rs
        self.scene.workdesk_push_revision(
            workdesk,
            "src/spatial_scene.rs",
            FileActionKind::Read,
            [35.0, 24.0],
            "review ECS hierarchy",
        );
        self.scene.workdesk_push_revision(
            workdesk,
            "src/spatial_scene.rs",
            FileActionKind::Write,
            [35.0, 24.0],
            "integrate Deck & TurnCard",
        );

        self.scene.update_transforms();

        "spawned Agent Carrel demo with 4 Turn Cards and Workdesk (press [ / ] or n / p to turn pages, v to splay grid, c to grab/drag)".to_string()
    }

    /// Spawn an Agent Carrel into the spatial scene with custom options and track it for navigation.
    pub fn spawn_agent_carrel_session_with_options(
        &mut self,
        session: crate::agent_transcript::AgentSession,
        revision_engine: crate::revision::RevisionEngine,
        options: crate::spatial_scene::CarrelLayoutOptions,
    ) -> Entity {
        let carrel_root = self.scene.spawn_root("agent_carrel_root");
        let carrel_zone = self.scene.spawn_zone(
            carrel_root,
            format!("agent:carrel:{}", session.session_id),
            format!("Agent Carrel: {}", session.session_id),
            Transform::IDENTITY,
            [280.0, 160.0],
        );
        self.zone_entities.insert("agent:carrel".to_string(), carrel_zone);

        let carrel_e = self.scene.spawn_agent_carrel_with_options(carrel_zone, &session, &revision_engine, options);
        self.active_carrel = Some(carrel_e);
        self.session = Some(session);
        self.revision_engine = Some(revision_engine);
        self.scene.update_transforms();
        carrel_e
    }

    /// Spawn an Agent Carrel into the spatial scene and track it for navigation.
    pub fn spawn_agent_carrel_session(
        &mut self,
        session: crate::agent_transcript::AgentSession,
        revision_engine: crate::revision::RevisionEngine,
    ) -> Entity {
        self.spawn_agent_carrel_session_with_options(
            session,
            revision_engine,
            crate::spatial_scene::CarrelLayoutOptions::default(),
        )
    }

    /// Advance active atomic beat (or turn) in the agent carrel.
    pub fn carrel_next(&mut self) -> Option<String> {
        let carrel_e = self.active_carrel?;
        let session = self.session.as_ref()?;
        let rev_engine = self.revision_engine.as_ref()?;
        let next_beat = self.scene.carrel_next_beat(carrel_e, session, rev_engine);
        self.scene.update_transforms();
        let events = session.linearize_events(Some(rev_engine));
        let total = events.len().max(session.turn_count());
        let summary = events
            .get(next_beat)
            .map(|e| e.summary())
            .unwrap_or_else(|| {
                session
                    .turns
                    .get(next_beat)
                    .map(|t| t.summary())
                    .unwrap_or_default()
            });
        Some(format!(
            "carrel: beat {}/{} — \"{}\"",
            next_beat + 1,
            total,
            summary
        ))
    }

    /// Retreat active atomic beat (or turn) in the agent carrel.
    pub fn carrel_prev(&mut self) -> Option<String> {
        let carrel_e = self.active_carrel?;
        let session = self.session.as_ref()?;
        let rev_engine = self.revision_engine.as_ref()?;
        let prev_beat = self.scene.carrel_prev_beat(carrel_e, session, rev_engine);
        self.scene.update_transforms();
        let events = session.linearize_events(Some(rev_engine));
        let total = events.len().max(session.turn_count());
        let summary = events
            .get(prev_beat)
            .map(|e| e.summary())
            .unwrap_or_else(|| {
                session
                    .turns
                    .get(prev_beat)
                    .map(|t| t.summary())
                    .unwrap_or_default()
            });
        Some(format!(
            "carrel: beat {}/{} — \"{}\"",
            prev_beat + 1,
            total,
            summary
        ))
    }

    /// Jump to a specific turn in the agent carrel.
    pub fn carrel_set_turn(&mut self, turn_index: usize) -> Option<String> {
        let carrel_e = self.active_carrel?;
        let session = self.session.as_ref()?;
        let rev_engine = self.revision_engine.as_ref()?;
        self.scene.carrel_set_turn(carrel_e, turn_index, session, rev_engine);
        self.scene.update_transforms();
        let total = session.turn_count();
        Some(format!("carrel: jump to turn {}/{}", turn_index + 1, total))
    }
}


#[cfg(test)]
mod tests;
