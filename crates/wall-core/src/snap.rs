//! Drafting-grid math for snapping terminal moves and border resizes, ported from
//! `wall/wallSnap.js`.
//!
//! Decides which of the grid's two levels (minor and major) draw at a given camera
//! zoom, what world-unit step a drag snaps to, and where a moved or border-resized
//! terminal lands on the grid.
//!
//! # The snap rule
//!
//! A terminal's body is sized in whole character cells (which are neither square nor
//! multiples of the grid spacing), so all four edges cannot lie on grid lines at once:
//! - **Move** ([`snap_move`]): snaps the body's top-left corner (`min_x`, `max_y`) to
//!   the nearest grid crossing and leaves its size untouched, so left and top edges
//!   align across terminals.
//! - **Resize** ([`snap_resize`]): sends each dragged edge to the grid line nearest the
//!   pointer and picks the largest whole-cell count that keeps that edge on or inside
//!   the line (short by less than one cell), while holding the opposite edge fixed.
//!
//! The snap step is the minor spacing whenever minor lines would draw at the current
//! zoom, and the major spacing otherwise so zoomed-out drags snap to visible lines.

use crate::geom::{Point, Rect};
pub use crate::layout::{Edges, GridSize as CellSize, MinGridSize as MinSize, ResizeResult};

/// Floating-point slack added before flooring a room measurement to whole cells, so an
/// edge that lands squarely on a grid line counts as fitting on that line.
pub const CELL_EPS: f64 = 1e-6;

/// Configuration of the drafting-paper grid (`GridSpec` in `wall/wallSnap.js`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSpec {
    /// World units between adjacent minor grid lines.
    pub grid_minor: f64,
    /// Every Nth minor line is promoted to a major line (`1` makes every line major).
    pub grid_major_every: u32,
    /// Minimum on-screen spacing (in CSS/logical pixels) below which a grid level hides.
    pub grid_min_px: f64,
}

impl GridSpec {
    /// Construct a drafting-grid specification.
    pub const fn new(grid_minor: f64, grid_major_every: u32, grid_min_px: f64) -> Self {
        Self {
            grid_minor,
            grid_major_every,
            grid_min_px,
        }
    }
}

/// Visibility and screen-space line spacing of the minor and major grid levels at a zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridLevels {
    /// Whether minor lines draw at this zoom (`false` when every line is major or too dense).
    pub minor: bool,
    /// Whether major lines draw at this zoom.
    pub major: bool,
    /// Minor line spacing on screen in pixels (`grid_minor * zoom`).
    pub minor_px: f64,
    /// Major line spacing on screen in pixels (`minor_px * grid_major_every`).
    pub major_px: f64,
}

/// Round half-way values toward `+∞`, matching JavaScript's `Math.round`.
fn js_round(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let floored = value.floor();
    if value - floored < 0.5 {
        floored
    } else {
        floored + 1.0
    }
}

/// Decide which grid levels draw at `zoom` (pixels per world unit), returning a hard
/// on/off switch at `grid.grid_min_px` alongside both pixel spacings.
///
/// When `grid.grid_major_every <= 1`, every line is a major line so `minor` is `false`.
pub fn grid_levels(grid: GridSpec, zoom: f64) -> GridLevels {
    let minor_px = grid.grid_minor * zoom;
    let major_px = minor_px * f64::from(grid.grid_major_every);
    GridLevels {
        minor: grid.grid_major_every > 1 && minor_px >= grid.grid_min_px,
        major: major_px >= grid.grid_min_px,
        minor_px,
        major_px,
    }
}

/// World-unit grid spacing that drags snap to at `zoom`: the minor spacing while minor
/// lines meet `grid_min_px` (even if the grid display is hidden), otherwise the major
/// spacing.
pub fn snap_step(grid: GridSpec, zoom: f64) -> f64 {
    if grid.grid_minor * zoom >= grid.grid_min_px {
        grid.grid_minor
    } else {
        grid.grid_minor * f64::from(grid.grid_major_every)
    }
}

/// Nearest multiple of `step` to `value`, using JS `Math.round` tie-breaking and
/// normalizing `-0.0` to `+0.0`.
pub fn snap_to(value: f64, step: f64) -> f64 {
    let snapped = js_round(value / step) * step;
    if snapped == 0.0 {
        0.0
    } else {
        snapped
    }
}

/// World-space delta `(x, y)` that moves `rect`'s top-left corner (`min_x`, `max_y`)
/// onto the nearest grid crossing of spacing `step`, leaving its size unchanged.
pub fn snap_move(rect: Rect, step: f64) -> Point {
    Point {
        x: snap_to(rect.min_x, step) - rect.min_x,
        y: snap_to(rect.max_y, step) - rect.max_y,
    }
}

/// Snap a border-resize drag to the grid of spacing `step`.
///
/// `rect` is the terminal body when the drag began, `start` its cell dimensions then,
/// `stride` the world size of one cell, and `(dx, dy)` the pointer travel in world
/// units (y up). Each dragged edge snaps to its nearest grid line and fits as many
/// whole cells as stay on or inside that line (clamped to `min`), shifting the origin
/// when `left` or `top` is dragged so the opposite edge stays put.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the 8-argument signature of wall/wallSnap.js snapResize"
)]
pub fn snap_resize(
    edges: Edges,
    rect: Rect,
    start: CellSize,
    stride: Point,
    dx: f64,
    dy: f64,
    min: MinSize,
    step: f64,
) -> ResizeResult {
    // Panel padding beyond the cell grid, so a cell count maps back to a body extent.
    let padding_x = (rect.max_x - rect.min_x) - f64::from(start.cols) * stride.x;
    let padding_y = (rect.max_y - rect.min_y) - f64::from(start.rows) * stride.y;
    let fit_cells = |room: f64, padding: f64, cell_span: f64| -> f64 {
        ((room - padding) / cell_span + CELL_EPS).floor()
    };

    let mut cols = f64::from(start.cols);
    let mut rows = f64::from(start.rows);
    if edges.right {
        cols = fit_cells(snap_to(rect.max_x + dx, step) - rect.min_x, padding_x, stride.x);
    }
    if edges.left {
        cols = fit_cells(rect.max_x - snap_to(rect.min_x + dx, step), padding_x, stride.x);
    }
    if edges.top {
        rows = fit_cells(snap_to(rect.max_y + dy, step) - rect.min_y, padding_y, stride.y);
    }
    if edges.bottom {
        rows = fit_cells(rect.max_y - snap_to(rect.min_y + dy, step), padding_y, stride.y);
    }

    let clamped_cols = cols.max(f64::from(min.min_cols)) as u32;
    let clamped_rows = rows.max(f64::from(min.min_rows)) as u32;
    let shift_x = if edges.left {
        (f64::from(start.cols) - f64::from(clamped_cols)) * stride.x
    } else {
        0.0
    };
    let shift_y = if edges.top {
        (f64::from(clamped_rows) - f64::from(start.rows)) * stride.y
    } else {
        0.0
    };

    ResizeResult {
        cols: clamped_cols,
        rows: clamped_rows,
        shift_x,
        shift_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::resize_from_drag;

    const GRID: GridSpec = GridSpec::new(4.0, 5, 6.0);

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // A body: 80 × 24 cells of 1.2 × 2.4 plus 0.6 of panel padding each way,
    // its top-left on a grid crossing.
    const STRIDE: Point = Point::new(1.2, 2.4);
    const START: CellSize = CellSize::new(80, 24);
    const MIN: MinSize = MinSize::new(20, 5);

    fn sample_body() -> Rect {
        Rect {
            min_x: 0.0,
            max_x: 80.0 * 1.2 + 0.6,
            max_y: 0.0,
            min_y: -(24.0 * 2.4 + 0.6),
        }
    }

    fn width_of(cols: u32) -> f64 {
        f64::from(cols) * STRIDE.x + 0.6
    }

    fn height_of(rows: u32) -> f64 {
        f64::from(rows) * STRIDE.y + 0.6
    }

    #[test]
    fn levels_hard_switch_at_grid_min_px() {
        assert_eq!(
            grid_levels(GRID, 4.0),
            GridLevels {
                minor: true,
                major: true,
                minor_px: 16.0,
                major_px: 80.0,
            }
        );
        let at_minor_floor = grid_levels(GRID, 1.5); // 6 px: right on the floor draws
        assert!(at_minor_floor.minor && at_minor_floor.major);

        let below_minor_floor = grid_levels(GRID, 1.49); // just under: minor gone, no fade
        assert!(!below_minor_floor.minor && below_minor_floor.major);

        let at_major_floor = grid_levels(GRID, 0.3); // major at 6 px
        assert!(!at_major_floor.minor && at_major_floor.major);

        let below_major_floor = grid_levels(GRID, 0.29);
        assert!(!below_major_floor.minor && !below_major_floor.major);

        let every_line_major = grid_levels(GridSpec { grid_major_every: 1, ..GRID }, 4.0);
        assert!(!every_line_major.minor && every_line_major.major);

        let zero_floor = grid_levels(GridSpec { grid_min_px: 0.0, ..GRID }, 0.05);
        assert!(zero_floor.minor && zero_floor.major);
    }

    #[test]
    fn snap_step_is_minor_while_minor_draws_else_major() {
        assert_eq!(snap_step(GRID, 4.0), 4.0);
        assert_eq!(snap_step(GRID, 1.5), 4.0);
        assert_eq!(snap_step(GRID, 1.0), 20.0);
        assert_eq!(snap_step(GRID, 0.01), 20.0);
    }

    #[test]
    fn move_puts_body_top_left_on_nearest_crossing() {
        assert_eq!(snap_to(5.9, 4.0), 4.0);
        assert_eq!(snap_to(6.1, 4.0), 8.0);
        // Object.is(snapTo(-1, 4), 0): must be +0.0, never -0.0.
        assert_eq!(snap_to(-1.0, 4.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(snap_to(-2.0, 4.0).to_bits(), 0.0_f64.to_bits());

        let rect = Rect {
            min_x: 13.1,
            max_x: 113.1,
            max_y: -9.7,
            min_y: -59.7,
        };
        let shift = snap_move(rect, 4.0);
        assert!(near(rect.min_x + shift.x, 12.0) && near(rect.max_y + shift.y, -8.0));

        let on_grid = Rect {
            min_x: 8.0,
            max_x: 50.0,
            max_y: -4.0,
            min_y: -30.0,
        };
        assert_eq!(snap_move(on_grid, 4.0), Point::new(0.0, 0.0));
    }

    #[test]
    fn resize_right_edge_lands_on_or_inside_line_in_whole_cells() {
        let body = sample_body();
        let right_only = Edges { right: true, ..Edges::default() };
        for dx in [-30.0, -7.3, -1.0, 0.0, 0.4, 3.3, 9.99, 25.0] {
            let resized = snap_resize(right_only, body, START, STRIDE, dx, 0.0, MIN, 4.0);
            let line = snap_to(body.max_x + dx, 4.0);
            let edge = body.min_x + width_of(resized.cols);
            assert!(edge <= line + 1e-9, "dx={dx}: edge {edge} exceeded line {line}");
            assert!(
                line - edge < STRIDE.x,
                "dx={dx}: gap {} >= cell width {}",
                line - edge,
                STRIDE.x
            );
            assert_eq!(resized.rows, 24);
            assert_eq!(resized.shift_x, 0.0);
            assert_eq!(resized.shift_y, 0.0);
        }
    }

    #[test]
    fn resize_left_top_bottom_edges_keep_opposite_edge_still() {
        let body = sample_body();
        let left_only = Edges { left: true, ..Edges::default() };
        for dx in [-13.0, -4.1, 2.0, 11.0] {
            let resized = snap_resize(left_only, body, START, STRIDE, dx, 0.0, MIN, 4.0);
            let line = snap_to(body.min_x + dx, 4.0);
            let left = body.min_x + resized.shift_x;
            assert!(near(left + width_of(resized.cols), body.max_x));
            assert!(left >= line - 1e-9);
            assert!(left - line < STRIDE.x);
        }

        let top_only = Edges { top: true, ..Edges::default() };
        for dy in [-9.0, 3.0, 14.0] {
            let resized = snap_resize(top_only, body, START, STRIDE, 0.0, dy, MIN, 4.0);
            let line = snap_to(body.max_y + dy, 4.0);
            let top = body.max_y + resized.shift_y;
            assert!(near(top - height_of(resized.rows), body.min_y));
            assert!(top <= line + 1e-9);
            assert!(line - top < STRIDE.y);
        }

        let bottom_only = Edges { bottom: true, ..Edges::default() };
        let bottom = snap_resize(bottom_only, body, START, STRIDE, 0.0, -10.0, MIN, 4.0);
        let bottom_line = snap_to(body.min_y - 10.0, 4.0);
        assert!(body.max_y - height_of(bottom.rows) >= bottom_line - 1e-9);
        assert_eq!(bottom.shift_y, 0.0);
    }

    #[test]
    fn corner_drags_both_edges_and_respects_minimum_size() {
        let body = sample_body();
        let bottom_right = Edges {
            right: true,
            bottom: true,
            ..Edges::default()
        };
        let resized = snap_resize(bottom_right, body, START, STRIDE, 10.0, -10.0, MIN, 4.0);
        assert!(resized.cols > 80);
        assert!(resized.rows > 24);

        let right_only = Edges { right: true, ..Edges::default() };
        assert_eq!(
            snap_resize(right_only, body, START, STRIDE, -500.0, 0.0, MIN, 4.0).cols,
            20
        );
        // Unsnapped drag rounds to the nearest cell, wherever that lands.
        assert_eq!(
            resize_from_drag(right_only, START, STRIDE, 1.3, 0.0, MIN).cols,
            81
        );
    }
}
