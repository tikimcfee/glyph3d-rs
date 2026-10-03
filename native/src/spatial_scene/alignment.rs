//! Spatial alignment and directional relationship layout engine for Bevy ECS.
//!
//! Evaluates sibling relationships within containers and computes relative
//! local transforms AABB-relatively (using `-box.min` / `+box.max`), supporting:
//! - `RowX` (horizontal sequence / shelf)
//! - `ColumnY` (vertical list / table grammar)
//! - `DepthZ` (receding depth cascade / Rolodex)
//! - `WrapConstraint` (`None`, `Count(n)` for Splay grid, `Extent(limit)`)

use bevy_ecs::prelude::*;
use bevy_transform::prelude::*;
use glam::{Quat, Vec3};

use super::{LocalBounds, SceneMeshKind, SpatialScene};

/// Primary axis along which children advance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlignmentAxis {
    /// Horizontal row stacking along +X (shelf/horizontal sequence).
    RowX {
        spacing: f32,
        align_y: VerticalAlign,
    },
    /// Vertical column stacking along Y.
    ColumnY {
        spacing: f32,
        align_x: HorizontalAlign,
        /// If true, advances downward (-Y) like lines of code / document text.
        /// If false, advances upward (+Y) according to Table Grammar.
        downwards: bool,
    },
    /// Depth stacking along -Z (receding depth cascade / Rolodex).
    DepthZ {
        /// Distance between successive items in Z.
        z_step: f32,
        /// Step offset along +X (e.g. cascade to reveal title/tabs).
        cascade_x: f32,
        /// Step offset along +Y (e.g. cascade to reveal header crests).
        cascade_y: f32,
        /// Optional pitch rotation angle around X axis (radians).
        pitch_rad: f32,
    },
}

/// Vertical alignment within a horizontal track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlign {
    #[default]
    Bottom, // flush at min y (y = 0)
    Center, // centered along Y
    Top,    // flush at max y
}

/// Horizontal alignment within a vertical track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HorizontalAlign {
    #[default]
    Left,   // flush at min x (x = 0)
    Center, // centered along X
    Right,  // flush at max x
}

/// Boundary rule that advances layout progression to the next track on the cross axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WrapConstraint {
    /// Pure 1D linear sequence without wrapping.
    None,
    /// Advance to the next track after `count` items on the current track (powers Splay grid).
    Count(usize),
    /// Advance to the next track when accumulated extent along the primary axis exceeds `limit`.
    Extent(f32),
}

/// ECS Component attached to a container entity declaring directional alignment of its children.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct SpatialAlignment {
    pub axis: AlignmentAxis,
    pub wrap: WrapConstraint,
    /// Cross-axis spacing when wrapping occurs (e.g. gap between rows in Splay mode).
    pub track_spacing: f32,
    /// Cross-axis progression direction: if true, rows advance upward (+Y) or columns advance rightward (+X).
    /// If false, rows advance downward (-Y) or columns advance leftward (-X).
    pub cross_axis_positive: bool,
    /// Whether this alignment calculation is currently active.
    pub enabled: bool,
}

impl SpatialAlignment {
    /// Create a horizontal Row (+X) alignment with spacing.
    pub fn row(spacing: f32) -> Self {
        Self {
            axis: AlignmentAxis::RowX {
                spacing,
                align_y: VerticalAlign::Bottom,
            },
            wrap: WrapConstraint::None,
            track_spacing: 0.0,
            cross_axis_positive: false,
            enabled: true,
        }
    }

    /// Create a vertical Column (Y) alignment with spacing.
    pub fn column(spacing: f32, downwards: bool) -> Self {
        Self {
            axis: AlignmentAxis::ColumnY {
                spacing,
                align_x: HorizontalAlign::Left,
                downwards,
            },
            wrap: WrapConstraint::None,
            track_spacing: 0.0,
            cross_axis_positive: true,
            enabled: true,
        }
    }

    /// Create a depth cascade (-Z) alignment.
    pub fn depth(z_step: f32, cascade_x: f32, cascade_y: f32) -> Self {
        Self {
            axis: AlignmentAxis::DepthZ {
                z_step,
                cascade_x,
                cascade_y,
                pitch_rad: 0.0,
            },
            wrap: WrapConstraint::None,
            track_spacing: 0.0,
            cross_axis_positive: false,
            enabled: true,
        }
    }

    /// Create an unfurled Splay grid layout (wrapping every `items_per_row` items).
    pub fn splay(item_spacing_x: f32, row_spacing_y: f32, items_per_row: usize) -> Self {
        Self {
            axis: AlignmentAxis::RowX {
                spacing: item_spacing_x,
                align_y: VerticalAlign::Top,
            },
            wrap: WrapConstraint::Count(items_per_row),
            track_spacing: row_spacing_y,
            cross_axis_positive: false, // rows flow downwards in -Y
            enabled: true,
        }
    }
}

/// Helper to read or derive an entity's local bounding box [min, max].
pub fn get_entity_bounds(world: &World, entity: Entity) -> ([f32; 3], [f32; 3]) {
    if let Some(lb) = world.get::<LocalBounds>(entity) {
        (lb.min, lb.max)
    } else if let Some(mesh) = world.get::<SceneMeshKind>(entity) {
        match mesh {
            SceneMeshKind::Quad { size, origin } => (
                [origin[0], origin[1], 0.0],
                [origin[0] + size[0], origin[1] + size[1], 0.0],
            ),
            SceneMeshKind::Box { extents } => {
                let hx = extents[0] * 0.5;
                let hy = extents[1] * 0.5;
                let hz = extents[2] * 0.5;
                ([-hx, -hy, -hz], [hx, hy, hz])
            }
        }
    } else {
        ([0.0; 3], [0.0; 3])
    }
}

impl SpatialScene {
    /// Evaluate all containers with `SpatialAlignment` and update their children's local `Transform`s.
    /// Also updates the container's `LocalBounds` to tightly enclose its children.
    pub fn apply_spatial_alignments(&mut self) {
        // Collect container entities with SpatialAlignment
        let mut containers = Vec::new();
        {
            let mut query = self.world.query::<(Entity, &SpatialAlignment)>();
            for (entity, alignment) in query.iter(&self.world) {
                if alignment.enabled {
                    containers.push((entity, alignment.clone()));
                }
            }
        }

        for (container, alignment) in containers {
            // Retrieve children of the container
            let children_entities: Vec<Entity> = self
                .world
                .get::<bevy_ecs::hierarchy::Children>(container)
                .map(|c| c.to_vec())
                .unwrap_or_default();

            if children_entities.is_empty() {
                continue;
            }

            // Gather bounds for each child
            let child_bounds: Vec<([f32; 3], [f32; 3])> = children_entities
                .iter()
                .map(|&child| get_entity_bounds(&self.world, child))
                .collect();

            // Calculate target local transforms and enclosing container bounds
            let (target_transforms, container_bounds) =
                compute_alignment_layout(&alignment, &child_bounds);

            // Apply transforms to children
            for (child, (translation, rotation)) in
                children_entities.iter().zip(target_transforms)
            {
                if let Some(mut transform) = self.world.get_mut::<Transform>(*child) {
                    transform.translation = translation;
                    transform.rotation = rotation;
                }
            }

            // Update container's LocalBounds
            if let Some(bounds) = container_bounds {
                if let Some(mut lb) = self.world.get_mut::<LocalBounds>(container) {
                    *lb = bounds;
                } else {
                    self.world.entity_mut(container).insert(bounds);
                }
            }
        }
    }
}

/// Compute target relative positions (translation, rotation) for a sequence of child bounds,
/// and compute the tight local bounding box of the whole container.
pub fn compute_alignment_layout(
    alignment: &SpatialAlignment,
    items: &[([f32; 3], [f32; 3])],
) -> (Vec<(Vec3, Quat)>, Option<LocalBounds>) {
    if items.is_empty() {
        return (Vec::new(), None);
    }

    let mut results = Vec::with_capacity(items.len());
    let mut total_min = Vec3::splat(f32::INFINITY);
    let mut total_max = Vec3::splat(f32::NEG_INFINITY);

    match alignment.axis {
        AlignmentAxis::RowX { spacing, align_y } => {
            let mut tracks: Vec<Vec<usize>> = Vec::new();
            let mut current_track: Vec<usize> = Vec::new();
            let mut track_w = 0.0f32;

            for (idx, (min, max)) in items.iter().enumerate() {
                let w = (max[0] - min[0]).max(0.0);
                let should_wrap = match alignment.wrap {
                    WrapConstraint::None => false,
                    WrapConstraint::Count(k) => !current_track.is_empty() && current_track.len() >= k,
                    WrapConstraint::Extent(limit) => {
                        !current_track.is_empty() && (track_w + w > limit)
                    }
                };

                if should_wrap {
                    tracks.push(std::mem::take(&mut current_track));
                    track_w = 0.0;
                }

                current_track.push(idx);
                track_w += w + spacing;
            }
            if !current_track.is_empty() {
                tracks.push(current_track);
            }

            let mut temp_results = vec![(Vec3::ZERO, Quat::IDENTITY); items.len()];
            let mut current_track_y = 0.0f32;

            for track in tracks {
                let mut track_max_h = 0.0f32;
                for &idx in &track {
                    let (min, max) = items[idx];
                    let h = (max[1] - min[1]).max(0.0);
                    track_max_h = track_max_h.max(h);
                }

                let mut track_x = 0.0f32;
                for &idx in &track {
                    let (min, max) = items[idx];
                    let w = (max[0] - min[0]).max(0.0);
                    let h = (max[1] - min[1]).max(0.0);

                    let x = track_x - min[0];
                    let y = match align_y {
                        VerticalAlign::Bottom => current_track_y - min[1],
                        VerticalAlign::Center => current_track_y - min[1] + (track_max_h - h) * 0.5,
                        VerticalAlign::Top => current_track_y - min[1] + (track_max_h - h),
                    };
                    let z = -min[2];

                    let pos = Vec3::new(x, y, z);
                    temp_results[idx] = (pos, Quat::IDENTITY);

                    let item_min = pos + Vec3::new(min[0], min[1], min[2]);
                    let item_max = pos + Vec3::new(max[0], max[1], max[2]);
                    total_min = total_min.min(item_min);
                    total_max = total_max.max(item_max);

                    track_x += w + spacing;
                }

                if alignment.cross_axis_positive {
                    current_track_y += track_max_h + alignment.track_spacing;
                } else {
                    current_track_y -= track_max_h + alignment.track_spacing;
                }
            }

            results = temp_results;
        }

        AlignmentAxis::ColumnY { spacing, align_x, downwards } => {
            let mut tracks: Vec<Vec<usize>> = Vec::new();
            let mut current_track: Vec<usize> = Vec::new();
            let mut track_h = 0.0f32;

            for (idx, (min, max)) in items.iter().enumerate() {
                let h = (max[1] - min[1]).max(0.0);
                let should_wrap = match alignment.wrap {
                    WrapConstraint::None => false,
                    WrapConstraint::Count(k) => !current_track.is_empty() && current_track.len() >= k,
                    WrapConstraint::Extent(limit) => {
                        !current_track.is_empty() && (track_h + h > limit)
                    }
                };

                if should_wrap {
                    tracks.push(std::mem::take(&mut current_track));
                    track_h = 0.0;
                }

                current_track.push(idx);
                track_h += h + spacing;
            }
            if !current_track.is_empty() {
                tracks.push(current_track);
            }

            let mut temp_results = vec![(Vec3::ZERO, Quat::IDENTITY); items.len()];
            let mut current_track_x = 0.0f32;

            for track in tracks {
                let mut track_max_w = 0.0f32;
                for &idx in &track {
                    let (min, max) = items[idx];
                    let w = (max[0] - min[0]).max(0.0);
                    track_max_w = track_max_w.max(w);
                }

                let mut track_y = 0.0f32;
                for &idx in &track {
                    let (min, max) = items[idx];
                    let w = (max[0] - min[0]).max(0.0);
                    let h = (max[1] - min[1]).max(0.0);

                    let x = match align_x {
                        HorizontalAlign::Left => current_track_x - min[0],
                        HorizontalAlign::Center => current_track_x - min[0] + (track_max_w - w) * 0.5,
                        HorizontalAlign::Right => current_track_x - min[0] + (track_max_w - w),
                    };

                    let y = if downwards {
                        let py = track_y - max[1];
                        track_y -= h + spacing;
                        py
                    } else {
                        let py = track_y - min[1];
                        track_y += h + spacing;
                        py
                    };
                    let z = -min[2];

                    let pos = Vec3::new(x, y, z);
                    temp_results[idx] = (pos, Quat::IDENTITY);

                    let item_min = pos + Vec3::new(min[0], min[1], min[2]);
                    let item_max = pos + Vec3::new(max[0], max[1], max[2]);
                    total_min = total_min.min(item_min);
                    total_max = total_max.max(item_max);
                }

                if alignment.cross_axis_positive {
                    current_track_x += track_max_w + alignment.track_spacing;
                } else {
                    current_track_x -= track_max_w + alignment.track_spacing;
                }
            }

            results = temp_results;
        }

        AlignmentAxis::DepthZ { z_step, cascade_x, cascade_y, pitch_rad } => {
            let rot = if pitch_rad.abs() > 1e-5 {
                Quat::from_rotation_x(pitch_rad)
            } else {
                Quat::IDENTITY
            };

            for (idx, (min, max)) in items.iter().enumerate() {
                let step = idx as f32;
                let x = -min[0] + step * cascade_x;
                let y = -min[1] + step * cascade_y;
                let z = -min[2] - step * z_step;

                let pos = Vec3::new(x, y, z);
                results.push((pos, rot));

                let item_min = pos + Vec3::new(min[0], min[1], min[2]);
                let item_max = pos + Vec3::new(max[0], max[1], max[2]);
                total_min = total_min.min(item_min);
                total_max = total_max.max(item_max);
            }
        }
    }

    let bounds = if total_min.x.is_finite() && total_max.x.is_finite() {
        Some(LocalBounds {
            min: total_min.to_array(),
            max: total_max.to_array(),
        })
    } else {
        None
    };

    (results, bounds)
}
