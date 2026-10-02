//! 3D Multi-sheet Deck & Paging container.
//!
//! Provides the physical spatial carrier for multi-page representations:
//! - Rolodex `Deck` mode (active sheet front-and-center at z=0, subsequent sheets
//!   cascading backwards into -Z with crest offsets exposing tabs/headers, past sheets
//!   tucked away with rotation).
//! - Splayed overview grid mode (`Splay`, M x N planar arrangement across X x Y with
//!   the active sheet lifted forward).
//!
//! Pages turn in 3D space while the camera stays fixed in place.

use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use glam::{Quat, Vec3};
use super::{ChildOf, LocalBounds, SpatialScene};

/// Display mode for a 3D Deck container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeckMode {
    /// Focused Rolodex cascade along -Z. Active card is front-and-center (z=0).
    /// Subsequent cards cascade backwards into -Z with crest offsets (+x, +y).
    /// Preceding cards tuck into +Z with a subtle angle rotation.
    #[default]
    Deck,
    /// Unfurled M x N planar grid across X and Y for wide spatial overview.
    Splay,
}

/// 3D Deck container component representing a multi-sheet or turn card carrier.
#[derive(Component, Debug, Clone)]
pub struct Deck {
    pub mode: DeckMode,
    pub active_index: usize,
    /// Distance between successive cards along -Z in Deck mode.
    pub z_pitch: f32,
    /// Crest offset [dx, dy] per card in Deck mode to reveal header tabs.
    pub crest_offset: [f32; 2],
    /// Columns for Splay grid mode (e.g. 3).
    pub splay_columns: usize,
    /// Spacing [dx, dy] between cards in Splay grid mode.
    pub splay_spacing: [f32; 2],
    /// Forward lift (+Z) for the active card in Splay grid mode.
    pub splay_lift: f32,
    /// Subtle rotation angle (in radians) around Y for turned/past cards in Deck mode.
    pub flip_angle_rad: f32,
    /// Whether the deck wraps circularly in Deck mode (Rolodex carousel).
    pub wrap: bool,
}

impl Default for Deck {
    fn default() -> Self {
        Self {
            mode: DeckMode::Deck,
            active_index: 0,
            z_pitch: 16.0,
            crest_offset: [2.0, 4.0],
            splay_columns: 3,
            splay_spacing: [15.0, 20.0],
            splay_lift: 8.0,
            flip_angle_rad: 0.12,
            wrap: true,
        }
    }
}

impl Deck {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_mode(mut self, mode: DeckMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_z_pitch(mut self, z_pitch: f32) -> Self {
        self.z_pitch = z_pitch;
        self
    }

    pub fn with_crest_offset(mut self, dx: f32, dy: f32) -> Self {
        self.crest_offset = [dx, dy];
        self
    }

    pub fn with_splay_columns(mut self, cols: usize) -> Self {
        self.splay_columns = cols.max(1);
        self
    }

    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Advance active page forward in space. Returns true if active page changed.
    pub fn next_page(&mut self, total_items: usize) -> bool {
        if total_items == 0 {
            return false;
        }
        if self.active_index + 1 < total_items {
            self.active_index += 1;
            true
        } else if self.wrap && total_items > 1 {
            self.active_index = 0;
            true
        } else {
            false
        }
    }

    /// Retreat active page backward in space. Returns true if active page changed.
    pub fn prev_page(&mut self, total_items: usize) -> bool {
        if self.active_index > 0 {
            self.active_index -= 1;
            true
        } else if self.wrap && total_items > 1 {
            self.active_index = total_items - 1;
            true
        } else {
            false
        }
    }

    /// Set active page index clamped to bounds.
    pub fn set_active_page(&mut self, index: usize, total_items: usize) {
        if total_items > 0 {
            self.active_index = index.min(total_items - 1);
        } else {
            self.active_index = 0;
        }
    }

    /// Toggle between Deck (Rolodex) and Splay (Overview Grid) modes.
    pub fn toggle_mode(&mut self) -> DeckMode {
        self.mode = match self.mode {
            DeckMode::Deck => DeckMode::Splay,
            DeckMode::Splay => DeckMode::Deck,
        };
        self.mode
    }
}

/// Entity component marking an item's logical index inside a Deck.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckItem {
    pub index: usize,
}

/// Computes local transforms (translation, rotation) and parent bounding box for a deck layout.
pub fn compute_deck_layout(
    deck: &Deck,
    child_bounds: &[([f32; 3], [f32; 3])],
) -> (Vec<(Vec3, Quat)>, Option<LocalBounds>) {
    let n = child_bounds.len();
    if n == 0 {
        return (Vec::new(), None);
    }

    let active = deck.active_index.min(n - 1);
    let mut transforms = Vec::with_capacity(n);
    let mut min_bound = Vec3::splat(f32::INFINITY);
    let mut max_bound = Vec3::splat(f32::NEG_INFINITY);

    match deck.mode {
        DeckMode::Deck => {
            for (i, bounds) in child_bounds.iter().enumerate() {
                let slot = if deck.wrap {
                    ((i as isize - active as isize).rem_euclid(n as isize)) as usize
                } else if i >= active {
                    i - active
                } else {
                    n - 1 - (active - i - 1)
                };

                let delta = slot as f32;
                let trans = Vec3::new(
                    delta * deck.crest_offset[0],
                    delta * deck.crest_offset[1],
                    -delta * deck.z_pitch,
                );
                let rot = Quat::IDENTITY;

                transforms.push((trans, rot));

                // Accumulate enclosing AABB
                update_enclosing_bounds(bounds, trans, rot, &mut min_bound, &mut max_bound);
            }
        }
        DeckMode::Splay => {
            let cols = deck.splay_columns.max(1);

            // Determine max child size to align grid cells neatly
            let max_w = child_bounds
                .iter()
                .map(|b| (b.1[0] - b.0[0]).abs())
                .fold(0.0f32, f32::max)
                .max(1.0);
            let max_h = child_bounds
                .iter()
                .map(|b| (b.1[1] - b.0[1]).abs())
                .fold(0.0f32, f32::max)
                .max(1.0);

            let pitch_x = max_w + deck.splay_spacing[0];
            let pitch_y = max_h + deck.splay_spacing[1];

            for (i, bounds) in child_bounds.iter().enumerate() {
                let col = i % cols;
                let row = i / cols;

                let bias_x = -bounds.0[0];
                let bias_y = -bounds.1[1];

                let target_x = ((col as f32) - (cols - 1) as f32 * 0.5) * pitch_x + bias_x - max_w * 0.5;
                let target_y = -(row as f32 * pitch_y) + bias_y;
                let target_z = if i == active { deck.splay_lift } else { 0.0 };

                let trans = Vec3::new(target_x, target_y, target_z);
                let rot = Quat::IDENTITY;
                transforms.push((trans, rot));

                update_enclosing_bounds(bounds, trans, rot, &mut min_bound, &mut max_bound);
            }
        }
    }

    let container_bounds = if min_bound.x <= max_bound.x {
        Some(LocalBounds {
            min: min_bound.into(),
            max: max_bound.into(),
        })
    } else {
        None
    };

    (transforms, container_bounds)
}

#[inline]
fn update_enclosing_bounds(
    bounds: &([f32; 3], [f32; 3]),
    trans: Vec3,
    rot: Quat,
    min_b: &mut Vec3,
    max_b: &mut Vec3,
) {
    let b_min = Vec3::from(bounds.0);
    let b_max = Vec3::from(bounds.1);

    // Transform 8 corners of child AABB
    let corners = [
        Vec3::new(b_min.x, b_min.y, b_min.z),
        Vec3::new(b_max.x, b_min.y, b_min.z),
        Vec3::new(b_min.x, b_max.y, b_min.z),
        Vec3::new(b_max.x, b_max.y, b_min.z),
        Vec3::new(b_min.x, b_min.y, b_max.z),
        Vec3::new(b_max.x, b_min.y, b_max.z),
        Vec3::new(b_min.x, b_max.y, b_max.z),
        Vec3::new(b_max.x, b_max.y, b_max.z),
    ];

    for c in &corners {
        let world_c = rot * (*c) + trans;
        *min_b = min_b.min(world_c);
        *max_b = max_b.max(world_c);
    }
}

impl SpatialScene {
    /// Spawn a Deck container entity.
    pub fn spawn_deck(&mut self, parent: Entity, name: impl Into<String>, deck: Deck) -> Entity {
        self.world
            .spawn((
                Transform::IDENTITY,
                ChildOf(parent),
                Name::new(name.into()),
                deck,
            ))
            .id()
    }

    /// Apply layout rules for all Deck entities in the scene.
    ///
    /// If `dt` is `Some(delta_seconds)`, transforms smoothly ease toward targets.
    /// If `dt` is `None`, transforms immediately snap to target positions.
    pub fn apply_deck_layouts(&mut self, dt: Option<f32>) {
        let mut decks_to_layout: Vec<(Entity, Deck)> = Vec::new();
        {
            let mut query = self.world.query::<(Entity, &Deck)>();
            for (entity, deck) in query.iter(&self.world) {
                decks_to_layout.push((entity, deck.clone()));
            }
        }

        for (deck_entity, deck) in decks_to_layout {
            let children_entities: Vec<Entity> =
                if let Some(children) = self.world.get::<Children>(deck_entity) {
                    children.to_vec()
                } else {
                    continue;
                };

            if children_entities.is_empty() {
                continue;
            }

            // Order children by DeckItem index if present, preserving declaration order as fallback
            let mut indexed_children: Vec<(usize, Entity)> = children_entities
                .iter()
                .enumerate()
                .map(|(seq, &e)| {
                    let idx = self.world.get::<DeckItem>(e).map(|di| di.index).unwrap_or(seq);
                    (idx, e)
                })
                .collect();
            indexed_children.sort_by_key(|(idx, _)| *idx);

            let sorted_entities: Vec<Entity> =
                indexed_children.into_iter().map(|(_, e)| e).collect();

            let child_bounds: Vec<([f32; 3], [f32; 3])> = sorted_entities
                .iter()
                .map(|&child| {
                    if let Some(bounds) = self.world.get::<LocalBounds>(child) {
                        (bounds.min, bounds.max)
                    } else {
                        ([0.0, 0.0, 0.0], [0.0, 0.0, 0.0])
                    }
                })
                .collect();

            let (target_transforms, container_bounds) =
                compute_deck_layout(&deck, &child_bounds);

            // Apply transforms (with optional easing)
            let easing_alpha = dt.map(|delta| (1.0 - (-14.0 * delta).exp()).clamp(0.0, 1.0));

            for (&child, (target_trans, target_rot)) in
                sorted_entities.iter().zip(target_transforms)
            {
                if let Some(mut transform) = self.world.get_mut::<Transform>(child) {
                    if let Some(alpha) = easing_alpha {
                        transform.translation = transform.translation.lerp(target_trans, alpha);
                        transform.rotation = transform.rotation.slerp(target_rot, alpha);
                    } else {
                        transform.translation = target_trans;
                        transform.rotation = target_rot;
                    }
                }
            }

            // Update parent container bounds
            if let Some(bounds) = container_bounds {
                if let Some(mut lb) = self.world.get_mut::<LocalBounds>(deck_entity) {
                    *lb = bounds;
                } else {
                    self.world.entity_mut(deck_entity).insert(bounds);
                }
            }
        }
    }

    /// Advance active page in a Deck entity.
    pub fn deck_next_page(&mut self, deck_entity: Entity) -> bool {
        let total = self
            .world
            .get::<Children>(deck_entity)
            .map(|c| c.len())
            .unwrap_or(0);
        if let Some(mut deck) = self.world.get_mut::<Deck>(deck_entity) {
            deck.next_page(total)
        } else {
            false
        }
    }

    /// Retreat active page in a Deck entity.
    pub fn deck_prev_page(&mut self, deck_entity: Entity) -> bool {
        let total = self
            .world
            .get::<Children>(deck_entity)
            .map(|c| c.len())
            .unwrap_or(0);
        if let Some(mut deck) = self.world.get_mut::<Deck>(deck_entity) {
            deck.prev_page(total)
        } else {
            false
        }
    }

    /// Toggle DeckMode between Deck and Splay in a Deck entity.
    pub fn deck_toggle_mode(&mut self, deck_entity: Entity) -> Option<DeckMode> {
        let mut deck = self.world.get_mut::<Deck>(deck_entity)?;
        Some(deck.toggle_mode())
    }
}
