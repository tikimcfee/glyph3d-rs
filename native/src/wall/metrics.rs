//! World-space cell metrics and grid-to-rectangle arithmetic.
//!
//! Cell dimensions are snapped to 10 significant bits (`snap_cell_dimension`),
//! matching `glyph3d-js`'s `core/cellMetrics.js`: `k * cell_width + cell_width`
//! equals `(k + 1) * cell_width` bit-identically in `f32` for every `k < 16384`,
//! so neighbouring cells share edges with zero cracks and zero overlap.

use glam::Vec2;

use crate::atlas::PrimaryMetrics;
use crate::text::CELL_HEIGHT_WORLD;

/// Axis-aligned rectangle in world coordinates (+X right, +Y up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldRect {
    pub min: Vec2,
    pub max: Vec2,
}

impl WorldRect {
    pub const fn new(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    pub fn center(&self) -> Vec2 {
        (self.min + self.max) * 0.5
    }

    pub fn contains(&self, point: Vec2) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }

    pub fn intersects(&self, other: Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }

    pub fn union(&self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    /// Expand this rectangle outward by `margin` on all four sides (negative shrinks).
    pub fn expand(&self, margin: f32) -> Self {
        self.expand_xy(margin, margin)
    }

    /// Expand this rectangle outward by `dx` horizontally and `dy` vertically.
    pub fn expand_xy(&self, dx: f32, dy: f32) -> Self {
        Self {
            min: Vec2::new(self.min.x - dx, self.min.y - dy),
            max: Vec2::new(self.max.x + dx, self.max.y + dy),
        }
    }
}

/// Sanitize a per-terminal scale factor (`<= 0` or non-finite defaults to `1.0`).
pub fn sanitize_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Round a positive world-space cell dimension to 10 significant bits.
///
/// Shifts the value by less than 0.1% while guaranteeing that integer multiples
/// `k * w` and `(k + 1) * w` are exact in `f32` for every `k < 16384` (10 + 14
/// = 24 bits of IEEE 754 float32 mantissa), matching `snapCellDimension` in
/// `glyph3d-js/packages/glyph3d-core/src/core/cellMetrics.js`.
pub fn snap_cell_dimension(value: f32) -> f32 {
    if !value.is_finite() || value <= 0.0 {
        return value;
    }
    let value_f64 = f64::from(value);
    let quantum = 2.0_f64.powi(value_f64.log2().floor() as i32 - 9);
    ((value_f64 / quantum).round() * quantum) as f32
}

/// Snapped world-space cell size derived from the atlas's primary font metrics.
///
/// A terminal's world `origin` is the top-left corner of cell `(0, 0)`.
/// Columns advance along `+X`; rows advance downward along `-Y` (row 0 on top).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
}

impl CellMetrics {
    /// Derive snapped cell metrics from the atlas's primary font metrics.
    pub fn from_primary(primary: PrimaryMetrics) -> Self {
        let raw_width = (f64::from(primary.advance_fu) * f64::from(CELL_HEIGHT_WORLD)
            / f64::from(primary.em_height_fu)) as f32;
        Self {
            cell_width: snap_cell_dimension(raw_width),
            cell_height: snap_cell_dimension(CELL_HEIGHT_WORLD),
        }
    }

    /// Construct snapped cell metrics from raw world width and height.
    pub fn snapped(raw_width: f32, raw_height: f32) -> Self {
        Self {
            cell_width: snap_cell_dimension(raw_width),
            cell_height: snap_cell_dimension(raw_height),
        }
    }

    /// World-space rectangle of cell `(col, row)` spanning `span_cols` columns.
    pub fn cell_rect(&self, origin: Vec2, col: u16, row: u16, span_cols: u16) -> WorldRect {
        let left = origin.x + f32::from(col) * self.cell_width;
        let right = origin.x + f32::from(col.saturating_add(span_cols)) * self.cell_width;
        let top = origin.y - f32::from(row) * self.cell_height;
        let bottom = origin.y - f32::from(row.saturating_add(1)) * self.cell_height;
        WorldRect::new(Vec2::new(left, bottom), Vec2::new(right, top))
    }

    /// World-space extent of the `cols × rows` cell grid alone (without panel padding).
    pub fn grid_rect(&self, origin: Vec2, cols: u16, rows: u16) -> WorldRect {
        let width = f32::from(cols) * self.cell_width;
        let height = f32::from(rows) * self.cell_height;
        WorldRect::new(
            Vec2::new(origin.x, origin.y - height),
            Vec2::new(origin.x + width, origin.y),
        )
    }

    /// World-space extent of the terminal's background panel (cell grid + `padding` on all sides).
    pub fn panel_rect(&self, origin: Vec2, cols: u16, rows: u16, padding: f32) -> WorldRect {
        let grid = self.grid_rect(origin, cols, rows);
        WorldRect::new(
            Vec2::new(grid.min.x - padding, grid.min.y - padding),
            Vec2::new(grid.max.x + padding, grid.max.y + padding),
        )
    }

    /// Return cell metrics scaled uniformly by `scale` (matching per-terminal `TerminalDraw::scale`).
    pub fn scaled(self, scale: f32) -> Self {
        let s = sanitize_scale(scale);
        Self {
            cell_width: self.cell_width * s,
            cell_height: self.cell_height * s,
        }
    }

    /// World-space extent of a terminal's background panel when drawn at `scale`.
    pub fn terminal_panel_rect(
        &self,
        origin: Vec2,
        cols: u16,
        rows: u16,
        padding: f32,
        scale: f32,
    ) -> WorldRect {
        let s = sanitize_scale(scale);
        self.scaled(s).panel_rect(origin, cols, rows, padding * s)
    }

    /// Resolve a world point to its `(col, row)` cell if inside the cell grid.
    pub fn cell_at_world(
        &self,
        origin: Vec2,
        cols: u16,
        rows: u16,
        world: Vec2,
    ) -> Option<(u16, u16)> {
        if cols == 0 || rows == 0 {
            return None;
        }
        let col_f = (world.x - origin.x) / self.cell_width;
        let row_f = (origin.y - world.y) / self.cell_height;
        if col_f < 0.0 || row_f < 0.0 || col_f >= f32::from(cols) || row_f >= f32::from(rows) {
            return None;
        }
        let col = (col_f.floor() as u16).min(cols - 1);
        let row = (row_f.floor() as u16).min(rows - 1);
        Some((col, row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapped_dimensions_stay_within_tenth_of_a_percent_and_tile_exactly() {
        let primary = PrimaryMetrics {
            upem: 2048,
            advance_fu: 1229,
            em_height_fu: 2320,
        };
        let raw_width = 1229.0f32 / 2320.0;
        let metrics = CellMetrics::from_primary(primary);
        assert_eq!(metrics.cell_height, 1.0);
        let rel_err = (metrics.cell_width - raw_width).abs() / raw_width;
        assert!(rel_err < 0.001, "snap moved cell_width by {rel_err:.6}");

        // Every adjacent pair of columns and rows up to 16383 must share their edge bit-exact.
        for k in 0u32..16383 {
            let right = (k as f32) * metrics.cell_width + metrics.cell_width;
            let next_left = ((k + 1) as f32) * metrics.cell_width;
            assert_eq!(
                right.to_bits(),
                next_left.to_bits(),
                "column edge mismatch at k={k}"
            );
        }
    }

    #[test]
    fn cell_rect_and_point_lookup_round_trip() {
        let metrics = CellMetrics::snapped(0.53, 1.0);
        let origin = Vec2::new(10.0, 20.0);
        let r0 = metrics.cell_rect(origin, 3, 2, 1);
        let r1 = metrics.cell_rect(origin, 4, 2, 1);
        assert_eq!(r0.max.x.to_bits(), r1.min.x.to_bits(), "adjacent cells share exact edge");

        let wide = metrics.cell_rect(origin, 3, 2, 2);
        assert_eq!(wide.min, r0.min);
        assert_eq!(wide.max, r1.max);

        assert_eq!(metrics.cell_at_world(origin, 80, 24, r0.center()), Some((3, 2)));
        assert_eq!(metrics.cell_at_world(origin, 80, 24, origin + Vec2::new(-0.01, -0.5)), None);
        assert_eq!(metrics.cell_at_world(origin, 80, 24, origin + Vec2::new(0.1, 0.01)), None);

        let panel = metrics.panel_rect(origin, 80, 24, 0.3);
        let grid = metrics.grid_rect(origin, 80, 24);
        assert!((panel.width() - (grid.width() + 0.6)).abs() < 1e-4);
        assert!((panel.height() - (grid.height() + 0.6)).abs() < 1e-4);

        // Half-scale terminal: both grid and padding halve around origin.
        let half_panel = metrics.terminal_panel_rect(origin, 80, 24, 0.3, 0.5);
        assert!((half_panel.width() - panel.width() * 0.5).abs() < 1e-4);
        assert!((half_panel.height() - panel.height() * 0.5).abs() < 1e-4);
        let half_cell = metrics.scaled(0.5).cell_rect(origin, 3, 2, 1);
        assert_eq!(metrics.scaled(0.5).cell_at_world(origin, 80, 24, half_cell.center()), Some((3, 2)));
        assert_eq!(sanitize_scale(0.0), 1.0);
        assert_eq!(sanitize_scale(-2.0), 1.0);
        assert_eq!(sanitize_scale(f32::NAN), 1.0);
    }
}
