//! 2-Page Agent Turn Card representation.
//!
//! Embodies the fundamental interaction unit of an autonomous agent:
//! - Left Page: Mind (Prompt, reasoning / chain-of-thought, tool invocations).
//! - Right Page: Material Impact (Output responses, tool results, diffs, touched files).
//! - Spine Gap: Physical separation and visual hinge between the two pages.

use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use glam::Vec3;
use super::{
    deck::DeckItem, ChildOf, LocalBounds, SceneMeshKind, SceneMeshMaterial, SpatialScene,
};

/// Role or cognitive category of a page within an Agent Turn Card.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPageKind {
    /// Left Page: Mind (Prompt, reasoning/thinking, tool calls invoked).
    Mind,
    /// Right Page: Material Impact (Output text, tool responses, diffs, touched file previews).
    Impact,
}

/// Component marking a 2-page Agent Turn Card unit.
#[derive(Component, Debug, Clone)]
pub struct AgentTurnCard {
    pub deck_slot: usize,
    pub event_index: usize,
    pub turn_index: usize,
    pub page_size: [f32; 2],
    pub spine_gap: f32,
    pub left_page: Entity,
    pub right_page: Entity,
}

impl SpatialScene {
    /// Spawn a 2-page Agent Turn/Beat Card container with Left (Spec) and Right (Artifact) pages.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_agent_turn_card(
        &mut self,
        parent: Entity,
        deck_slot: usize,
        event_index: usize,
        turn_index: usize,
        page_size: [f32; 2],
        spine_gap: f32,
        title: impl Into<String>,
        banner_colors: Option<([f32; 4], [f32; 4])>,
    ) -> Entity {
        let title_str = title.into();
        let half_w = page_size[0] * 0.5;
        let half_gap = spine_gap * 0.5;

        let (left_banner_col, right_banner_col) = banner_colors.unwrap_or((
            [0.20, 0.36, 0.60, 0.95],
            [0.18, 0.52, 0.35, 0.95],
        ));

        // Container bounds enclose both pages and the spine gap
        let card_bounds = LocalBounds {
            min: [-page_size[0] - half_gap, -page_size[1], 0.0],
            max: [page_size[0] + half_gap, 0.0, 0.0],
        };

        // Root turn card entity
        let card_entity = self
            .world
            .spawn((
                Transform::IDENTITY,
                ChildOf(parent),
                Name::new(format!("beat_{event_index}: {title_str}")),
                card_bounds,
                DeckItem { index: deck_slot },
            ))
            .id();

        // 1. Left Page (Spec / Mind)
        let left_trans = Transform::from_translation(Vec3::new(-half_w - half_gap, 0.0, 0.0));
        let left_bounds = LocalBounds {
            min: [-half_w, -page_size[1], 0.0],
            max: [half_w, 0.0, 0.0],
        };
        let left_page = self
            .world
            .spawn((
                left_trans,
                ChildOf(card_entity),
                Name::new("page_left_mind"),
                TurnPageKind::Mind,
                left_bounds,
                SceneMeshKind::Quad {
                    size: page_size,
                    origin: [-half_w, -page_size[1]],
                },
                SceneMeshMaterial {
                    color: [0.08, 0.10, 0.14, 0.90],
                    params: [0.0, 0.0, 0.0, 0.0],
                },
            ))
            .id();

        // Left Page Content (Mind): Header band
        self.world.spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.1)),
            ChildOf(left_page),
            Name::new("mind_header_banner"),
            SceneMeshKind::Quad {
                size: [page_size[0] - 6.0, 3.5],
                origin: [-half_w + 3.0, -5.5],
            },
            SceneMeshMaterial {
                color: left_banner_col,
                params: [0.0, 0.0, 0.0, 0.0],
            },
        ));

        // 2. Right Page (Artifact / Material Impact)
        let right_trans = Transform::from_translation(Vec3::new(half_w + half_gap, 0.0, 0.0));
        let right_bounds = LocalBounds {
            min: [-half_w, -page_size[1], 0.0],
            max: [half_w, 0.0, 0.0],
        };
        let right_page = self
            .world
            .spawn((
                right_trans,
                ChildOf(card_entity),
                Name::new("page_right_impact"),
                TurnPageKind::Impact,
                right_bounds,
                SceneMeshKind::Quad {
                    size: page_size,
                    origin: [-half_w, -page_size[1]],
                },
                SceneMeshMaterial {
                    color: [0.10, 0.12, 0.17, 0.90],
                    params: [0.0, 0.0, 0.0, 0.0],
                },
            ))
            .id();

        // Right Page Content (Material Impact): Header band
        self.world.spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.1)),
            ChildOf(right_page),
            Name::new("impact_header_banner"),
            SceneMeshKind::Quad {
                size: [page_size[0] - 6.0, 3.5],
                origin: [-half_w + 3.0, -5.5],
            },
            SceneMeshMaterial {
                color: right_banner_col,
                params: [0.0, 0.0, 0.0, 0.0],
            },
        ));

        // 3. Spine separator plate (visual hinge)
        if spine_gap > 0.0 {
            let spine_w = (spine_gap * 0.4).max(1.0);
            self.world.spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, -0.2)),
                ChildOf(card_entity),
                Name::new("spine_hinge"),
                SceneMeshKind::Quad {
                    size: [spine_w, page_size[1]],
                    origin: [-spine_w * 0.5, -page_size[1]],
                },
                SceneMeshMaterial {
                    color: [0.18, 0.22, 0.30, 0.75],
                    params: [0.0, 0.0, 0.0, 0.0],
                },
                LocalBounds {
                    min: [-spine_w * 0.5, -page_size[1], -0.2],
                    max: [spine_w * 0.5, 0.0, 0.0],
                },
            ));
        }

        self.world.entity_mut(card_entity).insert(AgentTurnCard {
            deck_slot,
            event_index,
            turn_index,
            page_size,
            spine_gap,
            left_page,
            right_page,
        });

        card_entity
    }
}
