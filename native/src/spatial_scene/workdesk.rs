//! 3D Spatial Workdesk for touched files, revisions, and diff cards.
//!
//! Provides a dedicated desk surface within an Agent Carrel:
//! - Revisions or edits to the SAME file stack along -Z (`DepthZ`).
//! - Different files stack upwards along +Y or across +X.
//! - Categorical separation by action kind (Read, Edit, Write, Analysis).

use std::collections::HashMap;
use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use super::{
    alignment::SpatialAlignment,
    ChildOf, LocalBounds, SceneMeshKind, SceneMeshMaterial, SpatialScene,
};

use serde::{Deserialize, Serialize};

/// Type of file action or operation represented on the Workdesk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileActionKind {
    Read,
    Edit,
    Write,
    AstAnalysis,
}

impl FileActionKind {
    /// Visual plate accent color for the action kind [R, G, B, A].
    pub fn accent_color(&self) -> [f32; 4] {
        match self {
            Self::Read => [0.15, 0.35, 0.55, 0.90],       // Cyan-slate
            Self::Edit => [0.60, 0.40, 0.15, 0.90],       // Amber
            Self::Write => [0.15, 0.50, 0.30, 0.90],      // Emerald
            Self::AstAnalysis => [0.45, 0.20, 0.55, 0.90], // Purple
        }
    }
}

/// Component marking a Workdesk container entity.
#[derive(Component, Debug, Clone)]
pub struct Workdesk {
    pub name: String,
    pub file_spacing: [f32; 2],
    pub revision_z_pitch: f32,
    pub file_stacks: HashMap<String, Entity>,
}

/// Component marking a Z-stack container for revisions of a specific file.
#[derive(Component, Debug, Clone)]
pub struct FileRevisionStack {
    pub file_path: String,
    pub revision_count: usize,
    pub active_revision: usize,
    pub visible_revisions: Vec<usize>,
    pub z_pitch: f32,
    pub window_limit: usize,
    pub scroll_offset: usize,
}

/// Component marking a specific revision/diff card in a file stack.
#[derive(Component, Debug, Clone)]
pub struct FileRevisionCard {
    pub file_path: String,
    pub revision_index: usize,
    pub action: FileActionKind,
}

impl SpatialScene {
    /// Spawn a Workdesk container on a parent entity.
    pub fn spawn_workdesk(
        &mut self,
        parent: Entity,
        name: impl Into<String>,
        file_spacing: [f32; 2],
        revision_z_pitch: f32,
    ) -> Entity {
        let name_str = name.into();
        self.world
            .spawn((
                Transform::IDENTITY,
                ChildOf(parent),
                Name::new(format!("workdesk:{}", name_str)),
                Workdesk {
                    name: name_str,
                    file_spacing,
                    revision_z_pitch,
                    file_stacks: HashMap::new(),
                },
                // Default arrangement for file stacks: row or splay across X x Y
                SpatialAlignment::splay(file_spacing[0], file_spacing[1], 4),
            ))
            .id()
    }

    /// Retrieve or spawn a `FileRevisionStack` container for `file_path` on the workdesk.
    pub fn workdesk_get_or_create_stack(
        &mut self,
        workdesk_entity: Entity,
        file_path: impl Into<String>,
    ) -> Entity {
        let path_str = file_path.into();

        if let Some(desk) = self.world.get::<Workdesk>(workdesk_entity) {
            if let Some(&stack_e) = desk.file_stacks.get(&path_str) {
                return stack_e;
            }
        }

        let z_pitch = self
            .world
            .get::<Workdesk>(workdesk_entity)
            .map(|d| d.revision_z_pitch)
            .unwrap_or(15.0);

        // Spawn new stack container with DepthZ alignment
        let stack_entity = self
            .world
            .spawn((
                Transform::IDENTITY,
                ChildOf(workdesk_entity),
                Name::new(format!("stack:{}", path_str)),
                FileRevisionStack {
                    file_path: path_str.clone(),
                    revision_count: 0,
                    active_revision: 0,
                    visible_revisions: Vec::new(),
                    z_pitch,
                    window_limit: 20,
                    scroll_offset: 0,
                },
                SpatialAlignment::depth(z_pitch, 0.0, 2.0),
            ))
            .id();

        if let Some(mut desk) = self.world.get_mut::<Workdesk>(workdesk_entity) {
            desk.file_stacks.insert(path_str, stack_entity);
        }

        stack_entity
    }

    /// Push a revision card with an explicit revision index onto the file's revision stack.
    pub fn workdesk_push_revision_card(
        &mut self,
        workdesk_entity: Entity,
        file_path: &str,
        revision_index: usize,
        action: FileActionKind,
        card_size: [f32; 2],
        summary: impl Into<String>,
    ) -> Entity {
        let stack_entity = self.workdesk_get_or_create_stack(workdesk_entity, file_path);

        if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_entity) {
            stack.visible_revisions.push(revision_index);
            if revision_index >= stack.revision_count {
                stack.revision_count = revision_index + 1;
            }
            if stack.visible_revisions.len() == 1 {
                stack.active_revision = revision_index;
            }
        }

        let summary_str = summary.into();
        let card_bounds = LocalBounds {
            min: [0.0, -card_size[1], 0.0],
            max: [card_size[0], 0.0, 0.5],
        };

        self.world
            .spawn((
                Transform::IDENTITY,
                ChildOf(stack_entity),
                Name::new(format!("{file_path}#r{revision_index}: {summary_str}")),
                FileRevisionCard {
                    file_path: file_path.to_string(),
                    revision_index,
                    action,
                },
                card_bounds,
                SceneMeshKind::Quad {
                    size: card_size,
                    origin: [0.0, -card_size[1]],
                },
                SceneMeshMaterial {
                    color: action.accent_color(),
                    params: [0.0, 0.0, 0.0, 0.0],
                },
            ))
            .id()
    }

    /// Push a new revision/diff card onto the file's revision stack with auto-incremented index.
    pub fn workdesk_push_revision(
        &mut self,
        workdesk_entity: Entity,
        file_path: &str,
        action: FileActionKind,
        card_size: [f32; 2],
        summary: impl Into<String>,
    ) -> Entity {
        let stack_entity = self.workdesk_get_or_create_stack(workdesk_entity, file_path);

        let rev_index = if let Some(mut stack) = self.world.get_mut::<FileRevisionStack>(stack_entity) {
            let idx = stack.revision_count;
            stack.revision_count += 1;
            stack.active_revision = idx;
            idx
        } else {
            0
        };

        self.workdesk_push_revision_card(
            workdesk_entity,
            file_path,
            rev_index,
            action,
            card_size,
            summary,
        )
    }
}
