//! Binary split-pane tree for terminal groups (ports `wall/splitTree.js`).
//!
//! Decides how a split group partitions a world-space rectangle into terminal
//! panes separated by gaps, how panes are inserted, removed, swapped, docked
//! or built from tiled group members, what minimum room a tree requires, where
//! dividers sit and how dragging or minimum-size fitting moves their ratios,
//! which pane sits next to another in each cardinal direction, and whether a
//! tree matches a group's member list.
//!
//! Trees are immutable values: every structural or ratio update returns a new
//! [`SplitNode`]. Coordinates follow [`crate::geom::Rect`]: world units with
//! **y pointing up**, so the `a` half of a [`SplitDir::Col`] branch is its
//! upper half (`max_y` down to `max_y - ah`).

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geom::{Point, Rect};

/// Share of a pane's width or height from an edge within which a pointer drop
/// docks beside the pane rather than swapping with its centre (`DOCK_EDGE`).
pub const DOCK_EDGE: f64 = 0.25;

/// Error returned when a split-tree operation violates an invariant or gets an
/// invalid path, terminal id, or member list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitError {
    message: String,
}

impl SplitError {
    /// Construct an error carrying the message text.
    pub fn new(message: impl Into<String>) -> SplitError {
        SplitError { message: message.into() }
    }

    /// Diagnostic message describing why the operation failed.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SplitError {}

/// A 2D extent in world units (`{ w, h }` in JS; unified with [`crate::layout::Size`]).
pub use crate::layout::Size;

/// How a split branch cuts its rectangle (`'row' | 'col'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    /// Side by side: `a` sits left of `b`.
    Row,
    /// Stacked vertically: `a` sits above `b` (world y points up).
    Col,
}

/// Which half of a cut pane a newly inserted terminal takes (`'before' | 'after'`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum InsertSide {
    /// First half (`a`): left in a [`SplitDir::Row`], upper in a [`SplitDir::Col`].
    Before,
    /// Second half (`b`): right in a [`SplitDir::Row`], lower in a [`SplitDir::Col`].
    #[default]
    After,
}

/// Cardinal direction across panes (`'left' | 'right' | 'up' | 'down'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// Parse a JS direction name (`"left"`, `"right"`, `"up"`, `"down"`).
    pub fn parse_dock(name: &str) -> Result<Direction, SplitError> {
        match name {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            other => Err(SplitError::new(format!(
                "a pane docks left, right, up or down, not '{other}'"
            ))),
        }
    }
}

/// Where a pointer drop over a pane lands (`'left' | 'right' | 'up' | 'down' | 'center'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DropZone {
    Left,
    Right,
    Up,
    Down,
    Center,
}

impl DropZone {
    /// Docking direction when the drop lands near an edge, or `None` for [`DropZone::Center`].
    pub const fn direction(self) -> Option<Direction> {
        match self {
            DropZone::Left => Some(Direction::Left),
            DropZone::Right => Some(Direction::Right),
            DropZone::Up => Some(Direction::Up),
            DropZone::Down => Some(Direction::Down),
            DropZone::Center => None,
        }
    }
}

/// Which child of a branch a path step selects (`'a' | 'b'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BranchSide {
    A,
    B,
}

impl BranchSide {
    const fn as_str(self) -> &'static str {
        match self {
            BranchSide::A => "a",
            BranchSide::B => "b",
        }
    }
}

/// Path from the root of a [`SplitNode`] to a descendant node (`('a'|'b')[]`).
pub type SplitPath = Vec<BranchSide>;

/// Axis along which a divider moves (`'x'` for upright row dividers, `'y'` for level col dividers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    X,
    Y,
}

/// A divider gap between the two halves of a branch node.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub path: SplitPath,
    pub axis: Axis,
    pub band: Rect,
}

/// Binary tree of split panes (`SplitLeaf | SplitBranch`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SplitNode {
    /// One terminal occupying its pane.
    Leaf { id: String },
    /// A rectangle cut in two along `dir`, giving `a` `ratio` of the room after the gap.
    Branch {
        dir: SplitDir,
        ratio: f64,
        a: Box<SplitNode>,
        b: Box<SplitNode>,
    },
}

impl SplitNode {
    /// Construct a leaf pane for terminal `id`.
    pub fn leaf(id: impl Into<String>) -> SplitNode {
        SplitNode::Leaf { id: id.into() }
    }

    /// Construct a branch splitting `a` and `b` along `dir` at `ratio`.
    pub fn branch(dir: SplitDir, ratio: f64, a: SplitNode, b: SplitNode) -> SplitNode {
        SplitNode::Branch {
            dir,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// Whether this node is a single terminal leaf.
    pub const fn is_leaf(&self) -> bool {
        matches!(self, SplitNode::Leaf { .. })
    }
}

/// Pane rectangles keyed by terminal id in insertion (reading) order, matching
/// JavaScript's `Map<string, Rect>` so [`neighbor`] breaks ties deterministically
/// in reading order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneMap {
    entries: Vec<(String, Rect)>,
}

impl PaneMap {
    pub const fn new() -> PaneMap {
        PaneMap { entries: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or overwrite `id`'s rectangle, preserving its original position
    /// when `id` was already present (as JS `Map.prototype.set` does).
    pub fn insert(&mut self, id: impl Into<String>, rect: Rect) {
        let id = id.into();
        if let Some((_, existing)) = self.entries.iter_mut().find(|(key, _)| *key == id) {
            *existing = rect;
        } else {
            self.entries.push((id, rect));
        }
    }

    /// Look up the rectangle assigned to `id`.
    pub fn get(&self, id: &str) -> Option<&Rect> {
        self.entries
            .iter()
            .find_map(|(key, rect)| (key == id).then_some(rect))
    }

    pub fn contains_key(&self, id: &str) -> bool {
        self.entries.iter().any(|(key, _)| key == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Rect)> {
        self.entries.iter().map(|(key, rect)| (key.as_str(), rect))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    pub fn values(&self) -> impl Iterator<Item = &Rect> {
        self.entries.iter().map(|(_, rect)| rect)
    }

    pub fn as_slice(&self) -> &[(String, Rect)] {
        &self.entries
    }
}

impl<'a> IntoIterator for &'a PaneMap {
    type Item = (&'a str, &'a Rect);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (String, Rect)>,
        fn(&'a (String, Rect)) -> (&'a str, &'a Rect),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, rect)| (key.as_str(), rect))
    }
}

impl IntoIterator for PaneMap {
    type Item = (String, Rect);
    type IntoIter = std::vec::IntoIter<(String, Rect)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<S: Into<String>> FromIterator<(S, Rect)> for PaneMap {
    fn from_iter<T: IntoIterator<Item = (S, Rect)>>(iter: T) -> PaneMap {
        let mut map = PaneMap::new();
        for (id, rect) in iter {
            map.insert(id, rect);
        }
        map
    }
}

/// Terminal ids in `tree` in reading order (`a` before `b`).
pub fn leaves(tree: &SplitNode) -> Vec<String> {
    let mut out = Vec::new();
    collect_leaves(tree, &mut out);
    out
}

fn collect_leaves(tree: &SplitNode, out: &mut Vec<String>) {
    match tree {
        SplitNode::Leaf { id } => out.push(id.clone()),
        SplitNode::Branch { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

/// Cut leaf `at` in two along `dir`: `at` keeps one half and `id` takes the
/// other — after `at` (right of it or below it) when `side` is
/// [`InsertSide::After`], or before it when [`InsertSide::Before`].
pub fn insert(
    tree: &SplitNode,
    at: &str,
    id: &str,
    dir: SplitDir,
    side: InsertSide,
) -> SplitNode {
    match tree {
        SplitNode::Leaf { id: leaf_id } => {
            if leaf_id != at {
                return tree.clone();
            }
            let keep = SplitNode::leaf(at);
            let born = SplitNode::leaf(id);
            match side {
                InsertSide::Before => SplitNode::branch(dir, 0.5, born, keep),
                InsertSide::After => SplitNode::branch(dir, 0.5, keep, born),
            }
        }
        SplitNode::Branch {
            dir: branch_dir,
            ratio,
            a,
            b,
        } => SplitNode::branch(
            *branch_dir,
            *ratio,
            insert(a, at, id, dir, side),
            insert(b, at, id, dir, side),
        ),
    }
}

/// Remove leaf `id` from `tree`, promoting its sibling into its parent's place
/// and room. Returns `None` when `tree` was that single leaf.
pub fn remove(tree: &SplitNode, id: &str) -> Option<SplitNode> {
    match tree {
        SplitNode::Leaf { id: leaf_id } => (leaf_id != id).then(|| tree.clone()),
        SplitNode::Branch { dir, ratio, a, b } => {
            let next_a = remove(a, id);
            let next_b = remove(b, id);
            let Some(next_a) = next_a else {
                return next_b;
            };
            let Some(next_b) = next_b else {
                return Some(next_a);
            };
            Some(SplitNode::branch(*dir, *ratio, next_a, next_b))
        }
    }
}

/// Swap leaves `a` and `b` in `tree` (each takes the other's pane while all
/// cuts and ratios stay unchanged).
pub fn swap(tree: &SplitNode, a: &str, b: &str) -> SplitNode {
    match tree {
        SplitNode::Leaf { id } => {
            if id == a {
                SplitNode::leaf(b)
            } else if id == b {
                SplitNode::leaf(a)
            } else {
                tree.clone()
            }
        }
        SplitNode::Branch {
            dir,
            ratio,
            a: left,
            b: right,
        } => SplitNode::branch(*dir, *ratio, swap(left, a, b), swap(right, a, b)),
    }
}

const fn dock_spec(side: Direction) -> (SplitDir, InsertSide) {
    match side {
        Direction::Left => (SplitDir::Row, InsertSide::Before),
        Direction::Right => (SplitDir::Row, InsertSide::After),
        Direction::Up => (SplitDir::Col, InsertSide::Before),
        Direction::Down => (SplitDir::Col, InsertSide::After),
    }
}

/// Remove leaf `id` from its current pane (giving its sibling the room) and
/// dock it beside leaf `at` on `side` by cutting `at`'s pane in two.
pub fn dock(
    tree: &SplitNode,
    id: &str,
    at: &str,
    side: Direction,
) -> Result<SplitNode, SplitError> {
    if id == at {
        return Err(SplitError::new("a pane cannot dock beside itself"));
    }
    let rest = remove(tree, id);
    let Some(rest) = rest else {
        return Err(SplitError::new(format!("no panes '{id}' and '{at}' to dock")));
    };
    if !leaves(&rest).iter().any(|leaf| leaf == at) {
        return Err(SplitError::new(format!("no panes '{id}' and '{at}' to dock")));
    }
    let (dir, insert_side) = dock_spec(side);
    Ok(insert(&rest, at, id, dir, insert_side))
}

/// Where a drop at point `point` over pane rectangle `rect` lands: on the
/// nearest edge when strictly within [`DOCK_EDGE`] of that edge, else
/// [`DropZone::Center`] (swap).
pub fn drop_zone(rect: Rect, point: Point) -> DropZone {
    let fx = (point.x - rect.min_x) / (rect.max_x - rect.min_x).max(1e-9);
    let fy = (rect.max_y - point.y) / (rect.max_y - rect.min_y).max(1e-9);
    let near = [
        (DropZone::Left, fx),
        (DropZone::Right, 1.0 - fx),
        (DropZone::Up, fy),
        (DropZone::Down, 1.0 - fy),
    ];
    let mut best = near[0];
    for candidate in near {
        if candidate.1 < best.1 {
            best = candidate;
        }
    }
    if best.1 < DOCK_EDGE {
        best.0
    } else {
        DropZone::Center
    }
}

/// Build an even split tree from `ids` arranged in rows of `columns` panes,
/// each child taking `1 / n` of its chain (what a tiled group becomes as a split).
pub fn from_members<S: AsRef<str>>(
    ids: &[S],
    columns: usize,
) -> Result<SplitNode, SplitError> {
    if ids.is_empty() {
        return Err(SplitError::new("a split needs at least one terminal"));
    }
    if columns == 0 {
        return Err(SplitError::new("columns must be at least 1"));
    }
    let rows: Vec<SplitNode> = ids
        .chunks(columns)
        .map(|chunk| {
            let row_nodes: Vec<SplitNode> = chunk
                .iter()
                .map(|id| SplitNode::leaf(id.as_ref()))
                .collect();
            even_chain(&row_nodes, SplitDir::Row)
        })
        .collect();
    Ok(even_chain(&rows, SplitDir::Col))
}

/// Even right-leaning chain where the head takes `1 / n` and the tail shares
/// the remainder recursively the same way.
fn even_chain(nodes: &[SplitNode], dir: SplitDir) -> SplitNode {
    if nodes.len() == 1 {
        return nodes[0].clone();
    }
    SplitNode::branch(
        dir,
        1.0 / (nodes.len() as f64),
        nodes[0].clone(),
        even_chain(&nodes[1..], dir),
    )
}

/// Partition `rect` into a branch's two child rectangles separated by `gap`.
fn branch_halves(dir: SplitDir, ratio: f64, rect: Rect, gap: f64) -> (Rect, Rect) {
    match dir {
        SplitDir::Row => {
            let first_width = (rect.max_x - rect.min_x - gap).max(0.0) * ratio;
            (
                Rect {
                    max_x: rect.min_x + first_width,
                    ..rect
                },
                Rect {
                    min_x: rect.min_x + first_width + gap,
                    ..rect
                },
            )
        }
        SplitDir::Col => {
            let first_height = (rect.max_y - rect.min_y - gap).max(0.0) * ratio;
            (
                Rect {
                    min_y: rect.max_y - first_height,
                    ..rect
                },
                Rect {
                    max_y: rect.max_y - first_height - gap,
                    ..rect
                },
            )
        }
    }
}

/// Lay `tree` out over `rect` with `gap` world units between neighbouring
/// halves, returning every leaf pane's rectangle in reading order.
pub fn panes(tree: &SplitNode, rect: Rect, gap: f64) -> PaneMap {
    let mut out = PaneMap::new();
    panes_into(tree, rect, gap, &mut out);
    out
}

fn panes_into(tree: &SplitNode, rect: Rect, gap: f64, out: &mut PaneMap) {
    match tree {
        SplitNode::Leaf { id } => out.insert(id.clone(), rect),
        SplitNode::Branch { dir, ratio, a, b } => {
            let (rect_a, rect_b) = branch_halves(*dir, *ratio, rect, gap);
            panes_into(a, rect_a, gap, out);
            panes_into(b, rect_b, gap, out);
        }
    }
}

/// Minimum world extent `tree` needs so every leaf gets at least `min_pane(id)`
/// with `gap` between cuts.
pub fn min_size<F>(tree: &SplitNode, mut min_pane: F, gap: f64) -> Size
where
    F: FnMut(&str) -> Size,
{
    min_size_inner(tree, &mut min_pane, gap)
}

fn min_size_inner<F>(tree: &SplitNode, min_pane: &mut F, gap: f64) -> Size
where
    F: FnMut(&str) -> Size,
{
    match tree {
        SplitNode::Leaf { id } => min_pane(id),
        SplitNode::Branch { dir, a, b, .. } => {
            let size_a = min_size_inner(a, min_pane, gap);
            let size_b = min_size_inner(b, min_pane, gap);
            match dir {
                SplitDir::Row => Size {
                    w: size_a.w + gap + size_b.w,
                    h: size_a.h.max(size_b.h),
                },
                SplitDir::Col => Size {
                    w: size_a.w.max(size_b.w),
                    h: size_a.h + gap + size_b.h,
                },
            }
        }
    }
}

fn format_path(path: &[BranchSide]) -> String {
    path.iter()
        .map(|step| step.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn node_at<'a>(tree: &'a SplitNode, path: &[BranchSide]) -> Result<&'a SplitNode, SplitError> {
    let mut current = tree;
    for step in path {
        match current {
            SplitNode::Leaf { .. } => {
                return Err(SplitError::new(format!(
                    "no split node at {}",
                    format_path(path)
                )));
            }
            SplitNode::Branch { a, b, .. } => {
                current = match step {
                    BranchSide::A => a,
                    BranchSide::B => b,
                };
            }
        }
    }
    Ok(current)
}

fn rect_at(
    tree: &SplitNode,
    rect: Rect,
    gap: f64,
    path: &[BranchSide],
) -> Result<Rect, SplitError> {
    let mut current = tree;
    let mut current_rect = rect;
    for step in path {
        match current {
            SplitNode::Leaf { .. } => {
                return Err(SplitError::new(format!(
                    "no split node at {}",
                    format_path(path)
                )));
            }
            SplitNode::Branch { dir, ratio, a, b } => {
                let (rect_a, rect_b) = branch_halves(*dir, *ratio, current_rect, gap);
                match step {
                    BranchSide::A => {
                        current_rect = rect_a;
                        current = a;
                    }
                    BranchSide::B => {
                        current_rect = rect_b;
                        current = b;
                    }
                }
            }
        }
    }
    Ok(current_rect)
}

fn collect_dividers(
    tree: &SplitNode,
    rect: Rect,
    gap: f64,
    path: &mut SplitPath,
    out: &mut Vec<Divider>,
) {
    let SplitNode::Branch { dir, ratio, a, b } = tree else {
        return;
    };
    let (rect_a, rect_b) = branch_halves(*dir, *ratio, rect, gap);
    let divider = match dir {
        SplitDir::Row => Divider {
            path: path.clone(),
            axis: Axis::X,
            band: Rect {
                min_x: rect_a.max_x,
                max_x: rect_b.min_x,
                min_y: rect.min_y,
                max_y: rect.max_y,
            },
        },
        SplitDir::Col => Divider {
            path: path.clone(),
            axis: Axis::Y,
            band: Rect {
                min_x: rect.min_x,
                max_x: rect.max_x,
                min_y: rect_b.max_y,
                max_y: rect_a.min_y,
            },
        },
    };
    out.push(divider);

    path.push(BranchSide::A);
    collect_dividers(a, rect_a, gap, path, out);
    path.pop();

    path.push(BranchSide::B);
    collect_dividers(b, rect_b, gap, path, out);
    path.pop();
}

/// Find the divider under `point`, widened across its thin gap by `tol` world
/// units. Checks deepest paths first so a nested divider wins where it meets
/// its parent's.
pub fn divider_at(
    tree: &SplitNode,
    rect: Rect,
    gap: f64,
    point: Point,
    tol: f64,
) -> Option<Divider> {
    let mut all = Vec::new();
    let mut path = Vec::new();
    collect_dividers(tree, rect, gap, &mut path, &mut all);
    all.sort_by_key(|divider| std::cmp::Reverse(divider.path.len()));

    for divider in all {
        let band = divider.band;
        let across = match divider.axis {
            Axis::X => point.x >= band.min_x - tol && point.x <= band.max_x + tol,
            Axis::Y => point.y >= band.min_y - tol && point.y <= band.max_y + tol,
        };
        let along = match divider.axis {
            Axis::X => point.y >= band.min_y && point.y <= band.max_y,
            Axis::Y => point.x >= band.min_x && point.x <= band.max_x,
        };
        if across && along {
            return Some(divider);
        }
    }
    None
}

/// Return `tree` with the branch at `path` given `ratio`.
pub fn set_ratio(
    tree: &SplitNode,
    path: &[BranchSide],
    ratio: f64,
) -> Result<SplitNode, SplitError> {
    let Some((first, rest)) = path.split_first() else {
        return match tree {
            SplitNode::Leaf { .. } => Err(SplitError::new("a pane has no divider")),
            SplitNode::Branch { dir, a, b, .. } => {
                Ok(SplitNode::branch(*dir, ratio, (**a).clone(), (**b).clone()))
            }
        };
    };
    match tree {
        SplitNode::Leaf { .. } => Err(SplitError::new(format!(
            "no split node at {}",
            format_path(path)
        ))),
        SplitNode::Branch {
            dir,
            ratio: current_ratio,
            a,
            b,
        } => match first {
            BranchSide::A => Ok(SplitNode::branch(
                *dir,
                *current_ratio,
                set_ratio(a, rest, ratio)?,
                (**b).clone(),
            )),
            BranchSide::B => Ok(SplitNode::branch(
                *dir,
                *current_ratio,
                (**a).clone(),
                set_ratio(b, rest, ratio)?,
            )),
        },
    }
}

/// Ratio the branch at `path` takes when its divider's centre is dragged to
/// `coord` (`x` for a row, `y` for a col), clamped so both halves retain at
/// least `min_pane`. When the rectangle cannot hold both minimums (`lo > hi`),
/// the branch keeps its existing ratio.
pub fn ratio_for<F>(
    tree: &SplitNode,
    rect: Rect,
    gap: f64,
    path: &[BranchSide],
    coord: f64,
    mut min_pane: F,
) -> Result<f64, SplitError>
where
    F: FnMut(&str) -> Size,
{
    let node = node_at(tree, path)?;
    let SplitNode::Branch { dir, ratio, a, b } = node else {
        return Err(SplitError::new("a pane has no divider"));
    };
    let node_rect = rect_at(tree, rect, gap, path)?;
    let size_a = min_size_inner(a, &mut min_pane, gap);
    let size_b = min_size_inner(b, &mut min_pane, gap);
    let is_row = *dir == SplitDir::Row;
    let room = (if is_row {
        node_rect.max_x - node_rect.min_x - gap
    } else {
        node_rect.max_y - node_rect.min_y - gap
    })
    .max(1e-9);
    let share = if is_row {
        coord - gap / 2.0 - node_rect.min_x
    } else {
        node_rect.max_y - coord - gap / 2.0
    };
    let lo = (if is_row { size_a.w } else { size_a.h }) / room;
    let hi = 1.0 - (if is_row { size_b.w } else { size_b.h }) / room;
    if lo > hi {
        Ok(*ratio)
    } else {
        Ok(hi.min(lo.max(share / room)))
    }
}

/// Return `tree` with every branch's ratio clamped so each side keeps its
/// [`min_size`] within `rect`. A branch whose rectangle cannot satisfy both
/// sides keeps its current ratio.
pub fn fit_ratios<F>(tree: &SplitNode, rect: Rect, gap: f64, mut min_pane: F) -> SplitNode
where
    F: FnMut(&str) -> Size,
{
    fit_ratios_inner(tree, rect, gap, &mut min_pane)
}

fn fit_ratios_inner<F>(tree: &SplitNode, rect: Rect, gap: f64, min_pane: &mut F) -> SplitNode
where
    F: FnMut(&str) -> Size,
{
    let SplitNode::Branch { dir, ratio, a, b } = tree else {
        return tree.clone();
    };
    let is_row = *dir == SplitDir::Row;
    let room = (if is_row {
        rect.max_x - rect.min_x - gap
    } else {
        rect.max_y - rect.min_y - gap
    })
    .max(1e-9);
    let size_a = min_size_inner(a, min_pane, gap);
    let size_b = min_size_inner(b, min_pane, gap);
    let lo = (if is_row { size_a.w } else { size_a.h }) / room;
    let hi = 1.0 - (if is_row { size_b.w } else { size_b.h }) / room;
    let next_ratio = if lo > hi {
        *ratio
    } else {
        hi.min(lo.max(*ratio))
    };
    let (rect_a, rect_b) = branch_halves(*dir, next_ratio, rect, gap);
    let fitted_a = fit_ratios_inner(a, rect_a, gap, min_pane);
    let fitted_b = fit_ratios_inner(b, rect_b, gap, min_pane);
    SplitNode::branch(*dir, next_ratio, fitted_a, fitted_b)
}

/// Find the pane adjacent to `id` in `dir`: the nearest pane beyond that edge
/// whose perpendicular span overlaps `id`'s, breaking gap ties by largest
/// overlap (and equal overlap by first entry in `panes`). Returns `None` at the
/// split's outer boundary or when `id` is absent.
pub fn neighbor(panes: &PaneMap, id: &str, dir: Direction) -> Option<String> {
    let rect = *panes.get(id)?;
    let eps = 1e-6;
    let mut best: Option<&str> = None;
    let mut best_gap = f64::INFINITY;
    let mut best_overlap = -1.0;

    for (other_id, other_rect) in panes {
        if other_id == id {
            continue;
        }
        let (gap, overlap) = match dir {
            Direction::Right => (
                other_rect.min_x - rect.max_x,
                rect.max_y.min(other_rect.max_y) - rect.min_y.max(other_rect.min_y),
            ),
            Direction::Left => (
                rect.min_x - other_rect.max_x,
                rect.max_y.min(other_rect.max_y) - rect.min_y.max(other_rect.min_y),
            ),
            Direction::Down => (
                rect.min_y - other_rect.max_y,
                rect.max_x.min(other_rect.max_x) - rect.min_x.max(other_rect.min_x),
            ),
            Direction::Up => (
                other_rect.min_y - rect.max_y,
                rect.max_x.min(other_rect.max_x) - rect.min_x.max(other_rect.min_x),
            ),
        };
        if gap < -eps || overlap <= eps {
            continue;
        }
        if gap < best_gap - eps || ((gap - best_gap).abs() <= eps && overlap > best_overlap) {
            best = Some(other_id);
            best_gap = gap;
            best_overlap = overlap;
        }
    }
    best.map(str::to_owned)
}

/// Validate `tree` against a group's `members`: every branch ratio must lie
/// strictly between `0` and `1`, no terminal may appear twice, and the tree's
/// leaves must match `members` as a set.
pub fn check_tree<'a, S: AsRef<str>>(
    tree: &'a SplitNode,
    members: &[S],
) -> Result<&'a SplitNode, SplitError> {
    validate_ratios(tree)?;
    let ids = leaves(tree);
    let unique: HashSet<&str> = ids.iter().map(String::as_str).collect();
    if unique.len() != ids.len() {
        return Err(SplitError::new(format!(
            "a split holds a terminal twice: {}",
            ids.join(" ")
        )));
    }
    if ids.len() != members.len()
        || ids
            .iter()
            .any(|id| !members.iter().any(|member| member.as_ref() == id))
    {
        let member_list: Vec<&str> = members.iter().map(AsRef::as_ref).collect();
        return Err(SplitError::new(format!(
            "a split's panes ({}) are not its members ({})",
            ids.join(" "),
            member_list.join(" ")
        )));
    }
    Ok(tree)
}

fn validate_ratios(tree: &SplitNode) -> Result<(), SplitError> {
    match tree {
        SplitNode::Leaf { .. } => Ok(()),
        SplitNode::Branch { ratio, a, b, .. } => {
            if !(*ratio > 0.0 && *ratio < 1.0) {
                return Err(SplitError::new(format!(
                    "a split's ratio is between 0 and 1, not {ratio}"
                )));
            }
            validate_ratios(a)?;
            validate_ratios(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror `R(minX, maxX, minY, maxY)` from `tools/wall-split.test.mjs`.
    fn rect(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Rect {
        Rect { min_x, min_y, max_x, max_y }
    }

    fn assert_close(actual: f64, expected: f64) {
        let diff = (actual - expected).abs();
        assert!(
            diff < 5e-3,
            "expected {actual} to be close to {expected} (diff {diff})"
        );
    }

    #[test]
    fn insert_cuts_pane_remove_promotes_sibling_leaves_read_in_order() {
        let mut tree = SplitNode::leaf("a");
        tree = insert(&tree, "a", "b", SplitDir::Row, InsertSide::After);
        tree = insert(&tree, "b", "c", SplitDir::Col, InsertSide::After);

        assert_eq!(leaves(&tree), vec!["a", "b", "c"]);
        assert_eq!(
            tree,
            SplitNode::branch(
                SplitDir::Row,
                0.5,
                SplitNode::leaf("a"),
                SplitNode::branch(
                    SplitDir::Col,
                    0.5,
                    SplitNode::leaf("b"),
                    SplitNode::leaf("c"),
                ),
            )
        );
        assert_eq!(
            insert(
                &SplitNode::leaf("a"),
                "a",
                "z",
                SplitDir::Row,
                InsertSide::Before
            ),
            SplitNode::branch(
                SplitDir::Row,
                0.5,
                SplitNode::leaf("z"),
                SplitNode::leaf("a"),
            )
        );
        assert_eq!(
            remove(&tree, "b"),
            Some(SplitNode::branch(
                SplitDir::Row,
                0.5,
                SplitNode::leaf("a"),
                SplitNode::leaf("c"),
            ))
        );
        assert_eq!(remove(&SplitNode::leaf("a"), "a"), None);
        assert_eq!(remove(&tree, "nope"), Some(tree));
    }

    #[test]
    fn panes_partition_rect_exactly_with_gaps() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.25,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        let ps = panes(&tree, rect(0.0, 101.0, -61.0, 0.0), 1.0);

        assert_eq!(ps.get("a"), Some(&rect(0.0, 25.0, -61.0, 0.0)));
        assert_eq!(ps.get("b"), Some(&rect(26.0, 101.0, -30.0, 0.0)));
        assert_eq!(ps.get("c"), Some(&rect(26.0, 101.0, -61.0, -31.0)));

        let area: f64 = ps
            .values()
            .map(|r| (r.max_x - r.min_x) * (r.max_y - r.min_y))
            .sum();
        assert_eq!(area, 101.0 * 61.0 - 61.0 * 1.0 - 75.0 * 1.0);
    }

    #[test]
    fn min_size_sums_along_cut_and_takes_larger_across_it() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        assert_eq!(
            min_size(&tree, |_| Size::new(10.0, 4.0), 1.0),
            Size::new(21.0, 9.0)
        );
    }

    #[test]
    fn divider_hit_in_widened_gap_deepest_first_and_drags_within_minimums() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        let bounds = rect(0.0, 101.0, -61.0, 0.0);

        assert_eq!(
            divider_at(&tree, bounds, 1.0, Point::new(50.5, -10.0), 0.0)
                .unwrap()
                .path,
            Vec::<BranchSide>::new()
        );
        assert_eq!(
            divider_at(&tree, bounds, 1.0, Point::new(49.8, -10.0), 0.5)
                .unwrap()
                .path,
            Vec::<BranchSide>::new()
        );
        let nested = divider_at(&tree, bounds, 1.0, Point::new(70.0, -30.5), 0.0).unwrap();
        assert_eq!(nested.path, vec![BranchSide::B]);
        assert_eq!(nested.axis, Axis::Y);
        assert_eq!(
            divider_at(&tree, bounds, 1.0, Point::new(20.0, -20.0), 0.5),
            None
        );

        let min = |_: &str| Size::new(20.0, 10.0);
        assert_close(ratio_for(&tree, bounds, 1.0, &[], 30.5, min).unwrap(), 0.3);
        assert_close(
            ratio_for(&tree, bounds, 1.0, &[], 5.0, min).unwrap(),
            20.0 / 100.0,
        );
        assert_close(
            ratio_for(&tree, bounds, 1.0, &[], 99.0, min).unwrap(),
            80.0 / 100.0,
        );

        let updated = set_ratio(&tree, &[BranchSide::B], 0.25).unwrap();
        let SplitNode::Branch { b, .. } = updated else {
            panic!("expected root branch");
        };
        let SplitNode::Branch { ratio, .. } = *b else {
            panic!("expected child branch b");
        };
        assert_eq!(ratio, 0.25);

        let err = set_ratio(&SplitNode::leaf("a"), &[], 0.3).unwrap_err();
        assert!(err.message().contains("no divider"), "got: {err}");
    }

    #[test]
    fn neighbors_in_four_directions_nearest_overlapping() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        let ps = panes(&tree, rect(0.0, 101.0, -61.0, 0.0), 1.0);

        assert_eq!(
            neighbor(&ps, "a", Direction::Right),
            Some("b".to_owned())
        );
        assert_eq!(
            neighbor(&ps, "b", Direction::Down),
            Some("c".to_owned())
        );
        assert_eq!(
            neighbor(&ps, "c", Direction::Up),
            Some("b".to_owned())
        );
        assert_eq!(
            neighbor(&ps, "c", Direction::Left),
            Some("a".to_owned())
        );
        assert_eq!(neighbor(&ps, "a", Direction::Left), None);
    }

    #[test]
    fn from_members_builds_even_rows_and_check_tree_validates_members() {
        let tree = from_members(&["a", "b", "c", "d", "e"], 2).unwrap();
        assert_eq!(leaves(&tree), vec!["a", "b", "c", "d", "e"]);

        let ps = panes(&tree, rect(0.0, 100.0, -90.0, 0.0), 0.0);
        assert_eq!(ps.get("a"), Some(&rect(0.0, 50.0, -30.0, 0.0)));
        assert_eq!(ps.get("d"), Some(&rect(50.0, 100.0, -60.0, -30.0)));
        assert_eq!(ps.get("e"), Some(&rect(0.0, 100.0, -90.0, -60.0)));

        assert_eq!(check_tree(&tree, &["e", "d", "c", "b", "a"]).unwrap(), &tree);

        let mismatch = check_tree(&tree, &["a", "b"]).unwrap_err();
        assert!(mismatch.message().contains("not its members"), "got: {mismatch}");

        let bad_ratio = check_tree(
            &SplitNode::branch(
                SplitDir::Row,
                1.0,
                SplitNode::leaf("a"),
                SplitNode::leaf("b"),
            ),
            &["a", "b"],
        )
        .unwrap_err();
        assert!(bad_ratio.message().contains("ratio"), "got: {bad_ratio}");
    }

    #[test]
    fn swap_dock_and_drop_zone_rearrange_panes() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.3,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );

        assert_eq!(
            swap(&tree, "a", "c"),
            SplitNode::branch(
                SplitDir::Row,
                0.3,
                SplitNode::leaf("c"),
                SplitNode::branch(
                    SplitDir::Col,
                    0.5,
                    SplitNode::leaf("b"),
                    SplitNode::leaf("a"),
                ),
            )
        );

        assert_eq!(
            dock(&tree, "a", "c", Direction::Down).unwrap(),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::branch(
                    SplitDir::Col,
                    0.5,
                    SplitNode::leaf("c"),
                    SplitNode::leaf("a"),
                ),
            )
        );
        assert_eq!(
            dock(&tree, "c", "a", Direction::Left).unwrap(),
            SplitNode::branch(
                SplitDir::Row,
                0.3,
                SplitNode::branch(
                    SplitDir::Row,
                    0.5,
                    SplitNode::leaf("c"),
                    SplitNode::leaf("a"),
                ),
                SplitNode::leaf("b"),
            )
        );

        let docked_up = dock(&tree, "b", "a", Direction::Up).unwrap();
        let SplitNode::Branch { a: first_child, .. } = docked_up else {
            panic!("expected root branch");
        };
        assert_eq!(
            *first_child,
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("a"),
            )
        );

        let self_err = dock(&tree, "a", "a", Direction::Left).unwrap_err();
        assert!(self_err.message().contains("itself"), "got: {self_err}");

        let missing_err = dock(&tree, "a", "z", Direction::Left).unwrap_err();
        assert!(missing_err.message().contains("no panes"), "got: {missing_err}");

        let pane_rect = rect(0.0, 100.0, -100.0, 0.0);
        assert_eq!(drop_zone(pane_rect, Point::new(50.0, -50.0)), DropZone::Center);
        assert_eq!(drop_zone(pane_rect, Point::new(10.0, -50.0)), DropZone::Left);
        assert_eq!(drop_zone(pane_rect, Point::new(90.0, -40.0)), DropZone::Right);
        assert_eq!(drop_zone(pane_rect, Point::new(40.0, -5.0)), DropZone::Up);
        assert_eq!(drop_zone(pane_rect, Point::new(60.0, -95.0)), DropZone::Down);
    }

    #[test]
    fn fit_ratios_clamps_dividers_to_pane_minimums_and_keeps_ratio_when_unachievable() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        let bounds = rect(0.0, 101.0, -61.0, 0.0);

        // Already fits: unchanged.
        assert_eq!(
            fit_ratios(&tree, bounds, 1.0, |_| Size::new(20.0, 10.0)),
            tree
        );

        // 'a' needs width 70 out of 100 room, 'c' needs height 45 out of 60 room.
        let fitted = fit_ratios(&tree, bounds, 1.0, |id| match id {
            "a" => Size::new(70.0, 10.0),
            "c" => Size::new(10.0, 45.0),
            _ => Size::new(10.0, 10.0),
        });
        assert_eq!(
            fitted,
            SplitNode::branch(
                SplitDir::Row,
                0.7,
                SplitNode::leaf("a"),
                SplitNode::branch(
                    SplitDir::Col,
                    0.25,
                    SplitNode::leaf("b"),
                    SplitNode::leaf("c"),
                ),
            )
        );

        // When a + b exceed room (lo > hi), branch preserves its existing ratio.
        let tight = SplitNode::branch(
            SplitDir::Row,
            0.4,
            SplitNode::leaf("a"),
            SplitNode::leaf("b"),
        );
        assert_eq!(
            fit_ratios(&tight, rect(0.0, 31.0, -20.0, 0.0), 1.0, |_| {
                Size::new(20.0, 10.0)
            }),
            tight
        );
    }

    #[test]
    fn divider_at_prefers_nested_divider_at_junction_and_errors_cover_edge_cases() {
        let tree = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::branch(
                SplitDir::Col,
                0.5,
                SplitNode::leaf("b"),
                SplitNode::leaf("c"),
            ),
        );
        let bounds = rect(0.0, 101.0, -61.0, 0.0);

        // At (51.0, -30.5) the child 'b' divider meets the root divider; deepest wins.
        let hit = divider_at(&tree, bounds, 1.0, Point::new(51.0, -30.5), 0.0).unwrap();
        assert_eq!(hit.path, vec![BranchSide::B]);
        assert_eq!(hit.axis, Axis::Y);

        assert_eq!(DOCK_EDGE, 0.25);
        assert_eq!(
            drop_zone(rect(0.0, 100.0, -100.0, 0.0), Point::new(25.0, -50.0)),
            DropZone::Center
        );
        assert_eq!(
            drop_zone(rect(0.0, 100.0, -100.0, 0.0), Point::new(24.999, -50.0)),
            DropZone::Left
        );

        let dup = SplitNode::branch(
            SplitDir::Row,
            0.5,
            SplitNode::leaf("a"),
            SplitNode::leaf("a"),
        );
        let dup_err = check_tree(&dup, &["a", "a"]).unwrap_err();
        assert!(dup_err.message().contains("twice"), "got: {dup_err}");

        let empty_err = from_members::<&str>(&[], 2).unwrap_err();
        assert!(
            empty_err.message().contains("at least one terminal"),
            "got: {empty_err}"
        );
    }
}
