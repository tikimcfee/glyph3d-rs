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
    /// Visual plate accent color for the action kind [R, G, B, A]:
    /// `[agent_cards.workdesk]`.
    pub fn accent_color(&self) -> [f32; 4] {
        let w = &crate::config::settings().agent_cards.workdesk;
        match self {
            Self::Read => w.read,
            Self::Edit => w.edit,
            Self::Write => w.write,
            Self::AstAnalysis => w.ast_analysis,
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

        // Spawn new stack container
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

    /// Apply layout rules for all FileRevisionStack entities on workdesks.
    ///
    /// Dynamically shifts cards so that the card matching `active_revision`
    /// sits at the front of the stack (Z = 0), with subsequent and previous
    /// revisions cascading behind into -Z.
    pub fn apply_workdesk_layouts(&mut self, dt: Option<f32>) {
        let mut stacks_to_layout: Vec<(Entity, FileRevisionStack)> = Vec::new();
        {
            let mut query = self.world.query::<(Entity, &FileRevisionStack)>();
            for (entity, stack) in query.iter(&self.world) {
                stacks_to_layout.push((entity, stack.clone()));
            }
        }

        for (stack_entity, stack) in stacks_to_layout {
            let children_entities: Vec<Entity> = if let Some(children) = self.world.get::<Children>(stack_entity) {
                children.to_vec()
            } else {
                continue;
            };

            let n = children_entities.len();
            if n == 0 {
                continue;
            }

            // Find child matching stack.active_revision
            let active_idx = children_entities
                .iter()
                .position(|&child| {
                    self.world
                        .get::<FileRevisionCard>(child)
                        .map(|c| c.revision_index == stack.active_revision)
                        .unwrap_or(false)
                })
                .unwrap_or(0);

            let cascade_x = 0.0f32;
            let cascade_y = 2.0f32;
            let z_pitch = stack.z_pitch;

            let easing_alpha = dt.map(|delta| (1.0 - (-14.0 * delta).exp()).clamp(0.0, 1.0));
            let mut min_bound = glam::Vec3::splat(f32::INFINITY);
            let mut max_bound = glam::Vec3::splat(f32::NEG_INFINITY);

            for (i, &child) in children_entities.iter().enumerate() {
                let delta = if i >= active_idx {
                    i - active_idx
                } else {
                    n - 1 - (active_idx - i - 1)
                } as f32;

                let target_trans = glam::Vec3::new(
                    delta * cascade_x,
                    delta * cascade_y,
                    -delta * z_pitch,
                );

                if let Some(mut transform) = self.world.get_mut::<Transform>(child) {
                    if let Some(alpha) = easing_alpha {
                        transform.translation = transform.translation.lerp(target_trans, alpha);
                    } else {
                        transform.translation = target_trans;
                    }
                }

                if let Some(lb) = self.world.get::<LocalBounds>(child) {
                    let c_min = target_trans + glam::Vec3::from(lb.min);
                    let c_max = target_trans + glam::Vec3::from(lb.max);
                    min_bound = min_bound.min(c_min);
                    max_bound = max_bound.max(c_max);
                }
            }

            if min_bound.x.is_finite() && max_bound.x.is_finite() {
                let container_bounds = LocalBounds {
                    min: min_bound.to_array(),
                    max: max_bound.to_array(),
                };
                if let Some(mut lb) = self.world.get_mut::<LocalBounds>(stack_entity) {
                    *lb = container_bounds;
                } else {
                    self.world.entity_mut(stack_entity).insert(container_bounds);
                }
            }
        }
    }
}
