//! Terminal group model — tiling, freeform and split layouts — ported from
//! `wall/WallGroups.js` (model half, without Three.js `GroupFrame` meshes or
//! relay transport).
//!
//! A group gathers terminals under an optional name, an ordered member list,
//! and one of three layouts:
//! - [`GroupLayout::Tile`]: packs shown members row-major into `columns`
//!   columns separated by `gap` world units starting at `origin`
//!   ([`crate::layout::tile_layout`]).
//! - [`GroupLayout::Freeform`]: leaves each member at its own canvas spot while
//!   moving them together when the group's header is dragged.
//! - [`GroupLayout::Split`]: partitions one world-space rectangle into binary
//!   panes ([`crate::split`]), re-wrapping each shown member in whole character
//!   cells inside its pane below optional title-bar room.
//!
//! Host queries and side effects (terminal bounds, cell counts, cell stride,
//! visibility, resizing, placing a body, remembering positions, title-bar room)
//! flow through [`WallHost`].

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use crate::geom::{Point, Rect};
use crate::layout::{
    self, Edges, Size as LayoutSize, TileOptions, BAR_ROOM,
};
use crate::split::{
    self, Axis, BranchSide, Direction, Divider, DropZone, InsertSide, PaneMap, Size, SplitDir,
    SplitNode, SplitPath,
};

/// Smallest cell grid a split pane allows (`MIN_PANE.cols` in `WallGroups.js`).
pub const MIN_PANE_COLS: u32 = 20;

/// Smallest row count a split pane allows (`MIN_PANE.rows` in `WallGroups.js`).
pub const MIN_PANE_ROWS: u32 = 5;

/// Fallback world Z for a group's backdrop frame before Z-stack assignment
/// (`DEFAULT_GROUP_Z` in `GroupFrame.js`).
pub const DEFAULT_GROUP_Z: f64 = -4.0;

/// Tolerance in world units for boundary/minimum comparisons (`EDGE_EPS`).
const EDGE_EPS: f64 = 1e-6;

/// Error returned when a group operation is refused (unknown group or terminal,
/// invalid column count or gap, split under `20×5`, or invalid layout move).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupError {
    message: String,
}

impl GroupError {
    pub fn new(message: impl Into<String>) -> GroupError {
        GroupError {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GroupError {}

/// How a group arranges its members (`'tile' | 'freeform' | 'split'`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GroupLayout {
    /// Pack shown members row-major into `columns` columns separated by `gap`.
    #[default]
    Tile,
    /// Leave each member at its own canvas coordinates; move together by header.
    Freeform,
    /// Partition one rectangle into binary panes re-wrapped to whole cells.
    Split,
}

impl GroupLayout {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroupLayout::Tile => "tile",
            GroupLayout::Freeform => "freeform",
            GroupLayout::Split => "split",
        }
    }
}

impl FromStr for GroupLayout {
    type Err = GroupError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "tile" => Ok(GroupLayout::Tile),
            "freeform" => Ok(GroupLayout::Freeform),
            "split" => Ok(GroupLayout::Split),
            other => Err(GroupError::new(format!(
                "a group's layout is 'tile', 'freeform' or 'split', not '{other}'"
            ))),
        }
    }
}

/// Direction along which [`Groups::can_split`] and [`Groups::insert_pane`] cut
/// an existing pane (`'right' | 'down'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SplitInsertDir {
    Right,
    Down,
}

impl SplitInsertDir {
    pub const fn as_str(self) -> &'static str {
        match self {
            SplitInsertDir::Right => "right",
            SplitInsertDir::Down => "down",
        }
    }

    const fn split_dir(self) -> SplitDir {
        match self {
            SplitInsertDir::Right => SplitDir::Row,
            SplitInsertDir::Down => SplitDir::Col,
        }
    }
}

impl FromStr for SplitInsertDir {
    type Err = GroupError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "right" => Ok(SplitInsertDir::Right),
            "down" => Ok(SplitInsertDir::Down),
            other => Err(GroupError::new(format!(
                "a split goes right or down, not '{other}'"
            ))),
        }
    }
}

/// Split-specific state stored on a [`Group`] (`WallSplit` in `WallStore.js`).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSplit {
    /// Width of the split's rectangle in world units.
    pub w: f64,
    /// Height of the split's rectangle in world units (including title-bar room).
    pub h: f64,
    /// Whether panes reserve and display title bars (`splitBarRows` high).
    pub bars: bool,
    /// Binary tree of terminal leaves partitioning the split rectangle.
    pub tree: SplitNode,
}

/// Durable data shape of a terminal group (`WallGroup` in `WallStore.js`).
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// User-assigned group name (`""` falls back to `"group N"` in [`Groups::name_of`]).
    pub name: String,
    /// Active arrangement mode (`tile`, `freeform`, or `split`).
    pub layout: GroupLayout,
    /// Column count for `tile` layout (and row width when converting to `split`).
    pub columns: usize,
    /// World-unit spacing between tiled columns and rows.
    pub gap: f64,
    /// Top-left corner of the first tiled window or of the split's rectangle.
    pub origin: Option<Point>,
    /// Member terminal ids in tiling / reading order.
    pub members: Vec<String>,
    /// Split geometry and pane tree when `layout == GroupLayout::Split`, else `None`.
    pub split: Option<GroupSplit>,
}

/// Group-related wall configuration knobs (`WALL_CONFIG` subset read by `WallGroups.js`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupConfig {
    /// Default layout for newly created groups (`groupLayout`: `'tile'`).
    pub group_layout: GroupLayout,
    /// Default column count for new tiled groups (`0` = `ceil(sqrt(n))`).
    pub group_columns: usize,
    /// Default world-unit gap between tiled members (`groupGap`: `2.0`).
    pub group_gap: f64,
    /// World-unit padding around shown member footprints for the group backdrop (`groupPad`: `3.0`).
    pub group_pad: f64,
    /// World-unit gap between neighbouring panes in a split (`splitGap`: `0.5`).
    pub split_gap: f64,
    /// Title-bar room reserved above each split pane in terminal rows (`splitBarRows`: `1.25`).
    pub split_bar_rows: f64,
    /// Whether newly created splits show pane title bars (`splitBars`: `true`).
    pub split_bars: bool,
}

impl Default for GroupConfig {
    fn default() -> GroupConfig {
        GroupConfig {
            group_layout: GroupLayout::Tile,
            group_columns: 0,
            group_gap: 2.0,
            group_pad: 3.0,
            split_gap: 0.5,
            split_bar_rows: 1.25,
            split_bars: true,
        }
    }
}

/// Optional overrides passed to [`Groups::create`].
#[derive(Clone, Debug, Default)]
pub struct CreateGroupOptions {
    pub name: Option<String>,
    pub layout: Option<GroupLayout>,
    pub columns: Option<usize>,
    pub gap: Option<f64>,
}

/// Optional overrides passed to [`Groups::set_layout`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SetLayoutOptions {
    pub columns: Option<usize>,
    pub gap: Option<f64>,
}

/// Per-edge world-unit room reserved inside a split pane beside its terminal
/// (used when an attached Agent View is open on that terminal).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneInset {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

/// Tiling footprint metrics of one member (`_metricsOf` in `WallGroups.js`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileMetrics {
    pub w: f64,
    pub h: f64,
    pub shift_x: f64,
    pub shift_y: f64,
    pub tile_box: Rect,
}

/// Snapshot of a terminal's live geometry on the wall.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalGeometry {
    /// Body top-left position (`grid.position` in JS).
    pub position: Point,
    /// World bounds of the terminal's body/panel (`wall.terminalRect(id)`).
    pub rect: Rect,
    /// Current character column count.
    pub cols: u32,
    /// Current character row count.
    pub rows: u32,
    /// World units per character cell (`grid.cellStride`).
    pub cell_stride: Point,
}

/// Host interface through which [`Groups`] queries and updates terminals on the wall.
pub trait WallHost {
    /// Group and split configuration knobs currently active on the wall.
    fn group_config(&self) -> GroupConfig {
        GroupConfig::default()
    }

    /// Live geometry of terminal `id`, or `None` if no such terminal exists on the wall.
    fn terminal(&self, id: &str) -> Option<TerminalGeometry>;

    /// Ordered list of terminal ids currently registered on the wall (`wall.order`).
    fn terminal_ids(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether terminal `id` is on the wall and not hidden (`wall.isShown(id)`).
    fn is_shown(&self, id: &str) -> bool {
        self.terminal(id).is_some()
    }

    /// Whether terminal `id` is currently drawn on the canvas (`wall.onCanvas(id)`).
    fn on_canvas(&self, id: &str) -> bool {
        self.is_shown(id)
    }

    /// Move terminal `id`'s body top-left to `pos` (`wall.placeBody(id, pos)`).
    fn place_body(&mut self, id: &str, pos: Point);

    /// Persist terminal `id`'s current position in durable state (`wall.rememberPosition(id)`).
    fn remember_position(&mut self, _id: &str) {}

    /// Resize terminal `id` to `cols × rows` character cells (`terminal.resize`).
    fn resize_terminal(&mut self, _id: &str, _cols: u32, _rows: u32) {}

    /// Set or clear the capped title-bar world height on terminal `id`
    /// (`wall.names?.setBarRoom(id, room)`).
    fn set_bar_room(&mut self, _id: &str, _room: Option<f64>) {}

    /// Solo fullscreen terminal id (`wall.fullscreen?.id`), if single-pane fullscreen is active.
    fn fullscreen_terminal_id(&self) -> Option<&str> {
        None
    }

    /// Fullscreen group id (`wall.fullscreen?.groupId`), if group fullscreen is active.
    fn fullscreen_group_id(&self) -> Option<&str> {
        None
    }

    /// Active fullscreen split rectangle override (`wall.fullscreen?.splitRectForGroup(g)`).
    fn fullscreen_split_rect(&self, _group: &Group) -> Option<Rect> {
        None
    }

    /// Re-fit active fullscreen view when its fullscreen group changes (`wall.fullscreen.refit()`).
    fn refit_fullscreen(&mut self) {}

    /// Per-edge inset reserved inside `id`'s split pane for an open Agent View.
    fn agent_inset(&self, _id: &str) -> PaneInset {
        PaneInset::default()
    }

    /// Custom tile footprint metrics when an Agent View is attached to `id`.
    fn agent_tile_metrics(&self, _id: &str) -> Option<TileMetrics> {
        None
    }

    /// Window rectangle (body + title bar) of `id` on the canvas at `zoom` for `frame_rect`.
    fn canvas_window_rect(&self, id: &str, _zoom: f64) -> Option<Rect> {
        let term = self.terminal(id)?;
        Some(Rect {
            max_y: term.rect.max_y + BAR_ROOM,
            ..term.rect
        })
    }

    /// Full footprint rectangle (window + open Agent View) of `id` on the canvas at `zoom`.
    fn canvas_footprint_rect(&self, id: &str, zoom: f64) -> Option<Rect> {
        self.canvas_window_rect(id, zoom)
    }

    /// World height of group `gid`'s header bar at `zoom` (defaults to [`BAR_ROOM`]).
    fn group_header_height(&self, _gid: &str, _zoom: f64) -> f64 {
        BAR_ROOM
    }
}

/// Captured state at the start of dragging a group by its header (`dragStart`).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupDrag {
    pub gid: String,
    pub members: Vec<(String, Point)>,
    pub origin: Option<Point>,
    pub delta: Option<Point>,
}

/// Preview and target index for a dragged tiled member (`tileDrop`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileDrop {
    pub to: usize,
    pub rect: Rect,
}

/// Divider hit returned by [`Groups::divider_at`].
#[derive(Clone, Debug, PartialEq)]
pub struct GroupDivider {
    pub gid: String,
    pub divider: Divider,
}

/// Inner divider reference returned by [`Groups::split_edge`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DividerTarget {
    pub path: SplitPath,
    pub axis: Axis,
}

/// Classification of a border press on a split pane (`splitEdge`): either an
/// outer boundary edge of the whole split or an inner divider next to the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitEdgeTarget {
    pub gid: String,
    pub outer: Option<Edges>,
    pub divider: Option<DividerTarget>,
}

/// Preview and resulting tree when dropping split pane `id` at a world point (`paneDrop`).
#[derive(Clone, Debug, PartialEq)]
pub struct PaneDrop {
    pub gid: String,
    pub at: String,
    pub zone: DropZone,
    pub tree: SplitNode,
    pub rect: Rect,
    pub fits: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct SplitShape {
    tree: SplitNode,
    w: f64,
    h: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct FitMemo {
    before: SplitShape,
    after: SplitShape,
}

struct SplitPlan {
    gid: String,
    next: Group,
    touched: Vec<String>,
}

/// Natural numeric suffix of a group id (`"g-3"` -> `3`, else `0`).
fn group_numeral(gid: &str) -> u64 {
    gid.chars()
        .skip(2)
        .collect::<String>()
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

/// Collection of terminal groups and the operations that create, arrange, resize,
/// and rearrange them on a [`WallHost`].
#[derive(Clone, Debug, Default)]
pub struct Groups {
    groups: HashMap<String, Group>,
    group_z: HashMap<String, f64>,
    fit_memo: HashMap<String, FitMemo>,
}

impl Groups {
    /// Create an empty group model.
    pub fn new() -> Groups {
        Groups::default()
    }

    /// Hydrate `Groups` from persisted `WallStore` group entries, normalizing
    /// any legacy non-split entry's `split` field to `None`.
    pub fn from_map(mut groups: HashMap<String, Group>) -> Groups {
        for group in groups.values_mut() {
            if group.layout != GroupLayout::Split {
                group.split = None;
            }
        }
        Groups {
            groups,
            group_z: HashMap::new(),
            fit_memo: HashMap::new(),
        }
    }

    /// Borrow all stored groups by group id (for persistence / inspection).
    pub fn as_map(&self) -> &HashMap<String, Group> {
        &self.groups
    }

    /// Consume `Groups` and return its underlying group map.
    pub fn into_map(self) -> HashMap<String, Group> {
        self.groups
    }

    /// Parse a cardinal movement direction (`"left"`, `"right"`, `"up"`, `"down"`)
    /// with the exact error message of `WallGroups.movePane`.
    pub fn parse_move_dir(raw: &str) -> Result<Direction, GroupError> {
        match raw {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            other => Err(GroupError::new(format!(
                "a pane moves left, right, up or down, not '{other}'"
            ))),
        }
    }

    // ── reading ──────────────────────────────────────────────────────────────

    /// Every group id in creation/numeral order (`g-1`, `g-2`, ...).
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.groups.keys().cloned().collect();
        ids.sort_by(|a, b| {
            group_numeral(a)
                .cmp(&group_numeral(b))
                .then_with(|| a.cmp(b))
        });
        ids
    }

    /// Non-failing group lookup (`WallStore.group(gid)`).
    pub fn group(&self, gid: &str) -> Option<&Group> {
        self.groups.get(gid)
    }

    /// Lookup group `gid`, returning an error when it does not exist (`WallGroups.get(gid)`).
    pub fn get(&self, gid: &str) -> Result<&Group, GroupError> {
        self.groups
            .get(gid)
            .ok_or_else(|| GroupError::new(format!("no group '{gid}'")))
    }

    /// Group id containing terminal `id`, if any (`WallStore.groupOf(id)`).
    pub fn group_of(&self, id: &str) -> Option<String> {
        for gid in self.ids() {
            if let Some(group) = self.groups.get(&gid) {
                if group.members.iter().any(|member| member == id) {
                    return Some(gid);
                }
            }
        }
        None
    }

    /// Display name of group `gid`: its custom name when non-empty, else `"group N"`.
    pub fn name_of(&self, gid: &str) -> Result<String, GroupError> {
        let group = self.get(gid)?;
        if group.name.is_empty() {
            Ok(format!("group {}", group_numeral(gid)))
        } else {
            Ok(group.name.clone())
        }
    }

    /// Whether terminal `id`'s spot is owned by a tiled group.
    pub fn tiles(&self, id: &str) -> bool {
        self.group_of(id)
            .and_then(|gid| self.groups.get(&gid))
            .is_some_and(|group| group.layout == GroupLayout::Tile)
    }

    /// Whether terminal `id` is a pane of a split group.
    pub fn splits(&self, id: &str) -> bool {
        self.group_of(id)
            .and_then(|gid| self.groups.get(&gid))
            .is_some_and(|group| group.layout == GroupLayout::Split)
    }

    /// Members of group `gid` that are currently on the wall and shown, in tiling order.
    pub fn shown_members<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
    ) -> Result<Vec<String>, GroupError> {
        let group = self.get(gid)?;
        Ok(group
            .members
            .iter()
            .filter(|id| wall.is_shown(id))
            .cloned()
            .collect())
    }

    /// Forget a terminal completely (called when `WallStore.forget` / `prune`
    /// drops a dead terminal): removes it from any group, collapses any split
    /// tree so its sibling takes the room, and deletes any group left empty.
    pub fn forget_terminal(&mut self, id: &str) {
        if let Some(gid) = self.group_of(id) {
            self.drop_members_from_store(&gid, &[id.to_string()]);
        }
    }

    // ── changing ─────────────────────────────────────────────────────────────

    /// Gather terminals `ids` into a new group (moving them out of any existing
    /// groups) and lay them out. Returns the new group's id (`"g-1"`, ...).
    pub fn create<W: WallHost, S: AsRef<str>>(
        &mut self,
        wall: &mut W,
        ids: &[S],
        options: CreateGroupOptions,
    ) -> Result<String, GroupError> {
        let members = self.validate_terminals(wall, ids)?;
        if members.is_empty() {
            return Err(GroupError::new("a group needs at least one terminal"));
        }
        let cfg = wall.group_config();
        let gid = self.new_group_id();
        let columns = options.columns.unwrap_or_else(|| {
            if cfg.group_columns > 0 {
                cfg.group_columns
            } else {
                ((members.len() as f64).sqrt().ceil()) as usize
            }
        });
        let layout = options.layout.unwrap_or(cfg.group_layout);
        let name = options.name.unwrap_or_default().trim().to_string();
        let gap = options.gap.unwrap_or(cfg.group_gap);

        let mut group = Group {
            name,
            layout,
            columns,
            gap,
            origin: None,
            members: members.clone(),
            split: None,
        };
        if layout == GroupLayout::Split {
            let probe = Group {
                layout: GroupLayout::Tile,
                ..group
            };
            group = self.as_split(wall, probe)?;
        } else {
            Self::check_group(&group)?;
            group.origin = self.origin_of(wall, &members);
        }

        let touched = self.groups_of(&members);
        self.commit(wall, &gid, group, &touched)?;
        Ok(gid)
    }

    /// Append terminals `ids` to group `gid` (removing them from any other
    /// group they belonged to).
    pub fn add<W: WallHost, S: AsRef<str>>(
        &mut self,
        wall: &mut W,
        gid: &str,
        ids: &[S],
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let validated = self.validate_terminals(wall, ids)?;
        let more: Vec<String> = validated
            .into_iter()
            .filter(|id| !group.members.contains(id))
            .collect();

        if group.layout == GroupLayout::Split {
            let split = group
                .split
                .as_ref()
                .ok_or_else(|| GroupError::new("a split needs its rect"))?;
            let mut tree = split.tree.clone();
            for id in &more {
                let current_leaves = split::leaves(&tree);
                let Some(last) = current_leaves.last() else {
                    return Err(GroupError::new("a split needs at least one terminal"));
                };
                let probe_group = Group {
                    split: Some(GroupSplit {
                        tree: tree.clone(),
                        ..split.clone()
                    }),
                    ..group.clone()
                };
                let panes = self.pane_rects_of(wall, &probe_group);
                let dir = match panes.get(last) {
                    Some(rect) if rect.width() >= rect.height() => SplitDir::Row,
                    _ => SplitDir::Col,
                };
                tree = split::insert(&tree, last, id, dir, InsertSide::After);
            }
            let next_members = split::leaves(&tree);
            let next = Group {
                members: next_members,
                split: Some(GroupSplit {
                    tree,
                    ..split.clone()
                }),
                ..group
            };
            self.ensure_fits(
                wall,
                &next,
                &format!("no room in group '{gid}' for {} more pane(s)", more.len()),
            )?;
            let touched = self.groups_of(&more);
            self.commit(wall, gid, next, &touched)?;
            return Ok(());
        }

        let mut members = group.members.clone();
        members.extend(more.iter().cloned());
        let touched = self.groups_of(&more);
        self.commit(wall, gid, Group { members, ..group }, &touched)
    }

    /// Remove terminals `ids` from whichever groups they belong to, leaving
    /// their positions untouched and deleting any group left with no members.
    pub fn remove<W: WallHost, S: AsRef<str>>(
        &mut self,
        wall: &mut W,
        ids: &[S],
    ) -> Result<(), GroupError> {
        let mut by_group: Vec<(String, Vec<String>)> = Vec::new();
        for id_ref in ids {
            let id = id_ref.as_ref();
            let Some(gid) = self.group_of(id) else {
                return Err(GroupError::new(format!("'{id}' is in no group")));
            };
            if let Some((_, list)) = by_group.iter_mut().find(|(g, _)| *g == gid) {
                list.push(id.to_string());
            } else {
                by_group.push((gid, vec![id.to_string()]));
            }
        }

        for (gid, out) in by_group {
            let group = self.get(&gid)?.clone();
            let members: Vec<String> = group
                .members
                .iter()
                .filter(|id| !out.contains(id))
                .cloned()
                .collect();
            if members.is_empty() {
                self.delete_group_from_store(&gid);
                continue;
            }
            let next_split = group.split.map(|split| {
                let next_tree = out.iter().fold(split.tree, |tree, id| {
                    split::remove(&tree, id).unwrap_or(tree)
                });
                GroupSplit {
                    tree: next_tree,
                    ..split
                }
            });
            self.commit(
                wall,
                &gid,
                Group {
                    members,
                    split: next_split,
                    ..group
                },
                &[],
            )?;
        }
        self.sync_bars(wall);
        Ok(())
    }

    /// Dissolve group `gid` while leaving all its member terminals where they sit.
    pub fn ungroup<W: WallHost>(&mut self, wall: &mut W, gid: &str) -> Result<(), GroupError> {
        let _ = self.get(gid)?;
        self.delete_group_from_store(gid);
        self.sync_bars(wall);
        Ok(())
    }

    /// Switch group `gid`'s layout (`tile`, `freeform`, or `split`), optionally
    /// updating `columns` and `gap`.
    pub fn set_layout<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        layout: GroupLayout,
        options: SetLayoutOptions,
    ) -> Result<(), GroupError> {
        let current = self.get(gid)?.clone();
        let mut next = Group {
            layout,
            columns: options.columns.unwrap_or(current.columns),
            gap: options.gap.unwrap_or(current.gap),
            split: if layout == GroupLayout::Split {
                current.split.clone()
            } else {
                None
            },
            ..current.clone()
        };
        if layout == GroupLayout::Split && current.layout != GroupLayout::Split {
            next = self.as_split(
                wall,
                Group {
                    layout: GroupLayout::Tile,
                    ..next
                },
            )?;
        }
        Self::check_group(&next)?;
        if layout == GroupLayout::Tile && current.layout != GroupLayout::Tile {
            next.origin = self.origin_of(wall, &current.members);
        }
        self.commit(wall, gid, next, &[])?;
        self.sync_bars(wall);
        Ok(())
    }

    // ── splits ───────────────────────────────────────────────────────────────

    /// Verify that terminal `at` can be split along `dir` while keeping every
    /// resulting pane at or above `20×5` cells. Mutates nothing.
    pub fn can_split<W: WallHost>(
        &self,
        wall: &W,
        at: &str,
        dir: SplitInsertDir,
    ) -> Result<(), GroupError> {
        self.split_plan(wall, at, "\0new", dir).map(|_| ())
    }

    /// Cut pane `at` in two along `dir` and place terminal `id` into the new
    /// right or lower half. Creates a single-terminal split around `at`'s
    /// window first if `at` is not already in a split group.
    pub fn insert_pane<W: WallHost>(
        &mut self,
        wall: &mut W,
        at: &str,
        id: &str,
        dir: SplitInsertDir,
    ) -> Result<String, GroupError> {
        if wall.terminal(id).is_none() {
            return Err(GroupError::new(format!("no terminal '{id}'")));
        }
        let plan = self.split_plan(wall, at, id, dir)?;
        self.commit(wall, &plan.gid, plan.next, &plan.touched)?;
        Ok(plan.gid)
    }

    /// Divider under `point` inside split group `gid` (within `tol` world units
    /// of the divider gap), or `None` when `gid` is not a visible split.
    pub fn group_divider_at<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
        point: Point,
        tol: f64,
    ) -> Option<Divider> {
        let group = self.groups.get(gid)?;
        if group.layout != GroupLayout::Split {
            return None;
        }
        let split = group.split.as_ref()?;
        let is_fs_group = wall.fullscreen_group_id() == Some(gid);
        let any_visible = group.members.iter().any(|id| {
            if is_fs_group {
                wall.is_shown(id)
            } else {
                wall.on_canvas(id)
            }
        });
        if !any_visible {
            return None;
        }
        let rect = self.split_rect_of(wall, group)?;
        split::divider_at(&split.tree, rect, wall.group_config().split_gap, point, tol)
    }

    /// Frontmost split divider under `point` across all split groups, widened
    /// by `tol` world units.
    pub fn divider_at<W: WallHost>(
        &self,
        wall: &W,
        point: Point,
        tol: f64,
    ) -> Option<GroupDivider> {
        let mut ordered = self.ids();
        ordered.sort_by(|a, b| {
            self.depth_of(b)
                .partial_cmp(&self.depth_of(a))
                .unwrap_or(Ordering::Equal)
        });
        for gid in ordered {
            if let Some(divider) = self.group_divider_at(wall, &gid, point, tol) {
                return Some(GroupDivider { gid, divider });
            }
        }
        None
    }

    /// Drag the split divider at `path` inside group `gid` to world coordinate
    /// `coord` (`x` for an upright row divider, `y` for a level col divider),
    /// clamped so every pane retains at least `20×5` cells.
    pub fn drag_divider<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        path: &[BranchSide],
        coord: f64,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let split = group
            .split
            .as_ref()
            .ok_or_else(|| GroupError::new(format!("group '{gid}' is not a split")))?;
        let rect = self
            .split_rect_of(wall, &group)
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        let split_gap = wall.group_config().split_gap;
        let ratio = split::ratio_for(&split.tree, rect, split_gap, path, coord, |id| {
            self.min_pane_size(wall, &group, id)
        })
        .map_err(|err| GroupError::new(err.to_string()))?;
        let current_ratio = Self::node_ratio(&split.tree, path)?;
        if (ratio - current_ratio).abs() < 1e-9 {
            return Ok(());
        }
        let next_tree = split::set_ratio(&split.tree, path, ratio)
            .map_err(|err| GroupError::new(err.to_string()))?;
        self.commit(
            wall,
            gid,
            Group {
                split: Some(GroupSplit {
                    tree: next_tree,
                    ..split.clone()
                }),
                ..group
            },
            &[],
        )
    }

    /// Determine what grabbing `edges` on split pane `id` drags: outer boundary
    /// edges resize the whole split, while an inner edge drags the adjacent divider.
    pub fn split_edge<W: WallHost>(
        &self,
        wall: &W,
        id: &str,
        edges: Edges,
    ) -> Result<SplitEdgeTarget, GroupError> {
        let gid = self
            .group_of(id)
            .ok_or_else(|| GroupError::new(format!("'{id}' is in no group")))?;
        let group = self.get(&gid)?;
        let split = group
            .split
            .as_ref()
            .ok_or_else(|| GroupError::new(format!("group '{gid}' is not a split")))?;
        let pane_rects = self.pane_rects_of(wall, group);
        let pane_rect = *pane_rects
            .get(id)
            .ok_or_else(|| GroupError::new(format!("no pane '{id}' in group '{gid}'")))?;
        let split_rect = self
            .split_rect_of(wall, group)
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;

        let on_outer = Edges {
            left: pane_rect.min_x <= split_rect.min_x + EDGE_EPS,
            right: pane_rect.max_x >= split_rect.max_x - EDGE_EPS,
            top: pane_rect.max_y >= split_rect.max_y - EDGE_EPS,
            bottom: pane_rect.min_y <= split_rect.min_y + EDGE_EPS,
        };
        let outer = Edges {
            left: edges.left && on_outer.left,
            right: edges.right && on_outer.right,
            top: edges.top && on_outer.top,
            bottom: edges.bottom && on_outer.bottom,
        };
        if outer.left || outer.right || outer.top || outer.bottom {
            return Ok(SplitEdgeTarget {
                gid,
                outer: Some(outer),
                divider: None,
            });
        }

        let gap = wall.group_config().split_gap;
        let tol = gap + self.cell_stride_of(wall, group).x;
        let mid_x = (pane_rect.min_x + pane_rect.max_x) / 2.0;
        let mid_y = (pane_rect.min_y + pane_rect.max_y) / 2.0;
        let probe = if edges.right {
            Point::new(pane_rect.max_x + gap / 2.0, mid_y)
        } else if edges.left {
            Point::new(pane_rect.min_x - gap / 2.0, mid_y)
        } else if edges.bottom {
            Point::new(mid_x, pane_rect.min_y - gap / 2.0)
        } else {
            Point::new(mid_x, pane_rect.max_y + gap / 2.0)
        };
        let divider =
            split::divider_at(&split.tree, split_rect, gap, probe, tol).map(|d| DividerTarget {
                path: d.path,
                axis: d.axis,
            });
        Ok(SplitEdgeTarget {
            gid,
            outer: None,
            divider,
        })
    }

    /// Preview dropping split pane `id` at world point `point`: returns the
    /// target pane, drop zone (`Center` swaps, an edge docks), resulting tree,
    /// preview rectangle for `id`, and whether every pane still fits `20×5`.
    pub fn pane_drop<W: WallHost>(&self, wall: &W, id: &str, point: Point) -> Option<PaneDrop> {
        let gid = self.group_of(id)?;
        let group = self.groups.get(&gid)?;
        if group.layout != GroupLayout::Split {
            return None;
        }
        let split = group.split.as_ref()?;
        for (at, &pane_rect) in &self.pane_rects_of(wall, group) {
            if at == id
                || point.x < pane_rect.min_x
                || point.x > pane_rect.max_x
                || point.y < pane_rect.min_y
                || point.y > pane_rect.max_y
            {
                continue;
            }
            let zone = split::drop_zone(pane_rect, point);
            let tree = match zone.direction() {
                None => split::swap(&split.tree, id, at),
                Some(side) => split::dock(&split.tree, id, at, side).ok()?,
            };
            let next = Group {
                members: split::leaves(&tree),
                split: Some(GroupSplit {
                    tree: tree.clone(),
                    ..split.clone()
                }),
                ..group.clone()
            };
            let fits = self.ensure_fits(wall, &next, "").is_ok();
            let rect = *self.pane_rects_of(wall, &next).get(id)?;
            return Some(PaneDrop {
                gid,
                at: at.to_string(),
                zone,
                tree,
                rect,
                fits,
            });
        }
        None
    }

    /// Execute a title-bar drop of split pane `id` at world point `point`. When
    /// dropped outside other panes or into a layout where panes would fall
    /// below `20×5`, snaps `id` back to its existing pane and returns `Ok(None)`.
    pub fn drop_pane<W: WallHost>(
        &mut self,
        wall: &mut W,
        id: &str,
        point: Point,
    ) -> Result<Option<PaneDrop>, GroupError> {
        let Some(drop) = self.pane_drop(wall, id, point) else {
            self.relayout_of(wall, id)?;
            return Ok(None);
        };
        if !drop.fits {
            self.relayout_of(wall, id)?;
            return Ok(None);
        }
        let group = self.get(&drop.gid)?.clone();
        let split = group
            .split
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        let next = Group {
            members: split::leaves(&drop.tree),
            split: Some(GroupSplit {
                tree: drop.tree.clone(),
                ..split
            }),
            ..group
        };
        self.commit(wall, &drop.gid, next, &[])?;
        Ok(Some(drop))
    }

    /// Move member `id` one step in `dir` within its group: swaps with the
    /// neighbouring pane in a split, or with the adjacent shown tile in a tiled
    /// group. Returns the id of the member it traded places with, or `Ok(None)`
    /// at the group's boundary.
    pub fn move_pane<W: WallHost>(
        &mut self,
        wall: &mut W,
        id: &str,
        dir: Direction,
    ) -> Result<Option<String>, GroupError> {
        let gid = self
            .group_of(id)
            .ok_or_else(|| GroupError::new(format!("'{id}' is in no group")))?;
        let group = self.get(&gid)?.clone();

        if group.layout == GroupLayout::Split {
            let split = group
                .split
                .as_ref()
                .ok_or_else(|| GroupError::new("a split needs its rect"))?;
            let Some(other) = split::neighbor(&self.pane_rects_of(wall, &group), id, dir) else {
                return Ok(None);
            };
            let tree = split::swap(&split.tree, id, &other);
            let next = Group {
                members: split::leaves(&tree),
                split: Some(GroupSplit {
                    tree,
                    ..split.clone()
                }),
                ..group
            };
            self.commit(wall, &gid, next, &[])?;
            return Ok(Some(other));
        }

        if group.layout != GroupLayout::Tile {
            return Err(GroupError::new(format!(
                "group '{gid}' is {}: its members have no places to trade",
                group.layout.as_str()
            )));
        }

        let shown = self.shown_members(wall, &gid)?;
        let Some(index) = shown.iter().position(|member| member == id) else {
            return Err(GroupError::new(format!("'{id}' is hidden")));
        };
        let cols = group.columns.min(shown.len());
        let target_index = match dir {
            Direction::Left => (index % cols != 0).then(|| index - 1),
            Direction::Right => {
                ((index + 1) % cols != 0 && index + 1 < shown.len()).then_some(index + 1)
            }
            Direction::Up => index.checked_sub(cols),
            Direction::Down => (index + cols < shown.len()).then_some(index + cols),
        };
        let Some(other_index) = target_index else {
            return Ok(None);
        };
        let other_id = shown[other_index].clone();
        let mut order = shown;
        order.swap(index, other_index);
        let members = self.with_hidden(wall, &group, &order);
        self.commit(wall, &gid, Group { members, ..group }, &[])?;
        Ok(Some(other_id))
    }

    /// World-space rectangle of split group `gid`.
    pub fn split_rect<W: WallHost>(&self, wall: &W, gid: &str) -> Result<Rect, GroupError> {
        let group = self.get(gid)?;
        self.split_rect_of(wall, group)
            .ok_or_else(|| GroupError::new(format!("group '{gid}' is not a split")))
    }

    /// Resize split group `gid`'s outer rectangle to `rect`, clamping width and
    /// height so every pane retains its `20×5` minimum and anchoring any
    /// clamped edge against its opposite stationary edge.
    pub fn resize_split<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        rect: Rect,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let split = group
            .split
            .as_ref()
            .ok_or_else(|| GroupError::new(format!("group '{gid}' is not a split")))?;
        let origin = group
            .origin
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        let min = split::min_size(&split.tree, |id| self.min_pane_size(wall, &group, id), wall.group_config().split_gap);
        let w = min.w.max(rect.max_x - rect.min_x);
        let h = min.h.max(rect.max_y - rect.min_y);
        let old = self
            .split_rect_of(wall, &group)
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;

        let x = if rect.min_x != old.min_x {
            rect.max_x - w
        } else {
            rect.min_x
        };
        let y = if rect.max_y != old.max_y {
            rect.min_y + h
        } else {
            rect.max_y
        };
        if x == origin.x && y == origin.y && w == split.w && h == split.h {
            return Ok(());
        }
        self.commit(
            wall,
            gid,
            Group {
                origin: Some(Point::new(x, y)),
                split: Some(GroupSplit {
                    w,
                    h,
                    ..split.clone()
                }),
                ..group
            },
            &[],
        )
    }

    /// Show or hide pane title bars inside split group `gid`.
    pub fn set_bars<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        on: bool,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        if group.layout != GroupLayout::Split {
            return Err(GroupError::new(format!("group '{gid}' is not a split")));
        }
        let split = group
            .split
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        self.commit(
            wall,
            gid,
            Group {
                split: Some(GroupSplit { bars: on, ..split }),
                ..group
            },
            &[],
        )
    }

    /// World-space rectangle of each pane in split group `gid`, in reading order.
    pub fn pane_rects<W: WallHost>(&self, wall: &W, gid: &str) -> Result<PaneMap, GroupError> {
        let group = self.get(gid)?;
        if group.layout != GroupLayout::Split {
            return Err(GroupError::new(format!("group '{gid}' is not a split")));
        }
        Ok(self.pane_rects_of(wall, group))
    }

    /// Update group `gid`'s column count (`>= 1`) and re-layout.
    pub fn set_columns<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        columns: usize,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let next = Group { columns, ..group };
        Self::check_group(&next)?;
        self.commit(wall, gid, next, &[])
    }

    /// Update group `gid`'s gap (`>= 0`, finite) and re-layout.
    pub fn set_gap<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        gap: f64,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let next = Group { gap, ..group };
        Self::check_group(&next)?;
        self.commit(wall, gid, next, &[])
    }

    /// Rename group `gid` (an empty/whitespace name restores its default `"group N"` label).
    pub fn rename<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        name: &str,
    ) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        let next = Group {
            name: name.trim().to_string(),
            ..group
        };
        self.commit(wall, gid, next, &[])
    }

    /// Translate an entire group — all members (shown or hidden) and its origin —
    /// by `(dx, dy)` world units (`WallGroups.move`).
    pub fn move_group<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        dx: f64,
        dy: f64,
    ) -> Result<(), GroupError> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(GroupError::new(format!(
                "a move takes two numbers, not {dx} {dy}"
            )));
        }
        let mut drag = self.drag_start(wall, gid)?;
        self.drag_to(wall, &mut drag, dx, dy);
        self.drag_end(wall, &drag);
        Ok(())
    }

    /// Begin a header drag on group `gid`, recording every member's starting
    /// position and the group's origin.
    pub fn drag_start<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
    ) -> Result<GroupDrag, GroupError> {
        let group = self.get(gid)?;
        let mut members = Vec::with_capacity(group.members.len());
        for id in &group.members {
            if let Some(term) = wall.terminal(id) {
                members.push((id.clone(), term.position));
            }
        }
        Ok(GroupDrag {
            gid: gid.to_string(),
            members,
            origin: group.origin,
            delta: None,
        })
    }

    /// Pose the dragged group at `(dx, dy)` from where `drag` started.
    pub fn drag_to<W: WallHost>(&self, wall: &mut W, drag: &mut GroupDrag, dx: f64, dy: f64) {
        for (id, start_pos) in &drag.members {
            if wall.terminal(id).is_some() {
                wall.place_body(id, Point::new(start_pos.x + dx, start_pos.y + dy));
            }
        }
        drag.delta = Some(Point::new(dx, dy));
    }

    /// Commit a completed header drag: remember member positions and shift the
    /// group's stored `origin` (no-op if the group disappeared mid-drag).
    pub fn drag_end<W: WallHost>(&mut self, wall: &mut W, drag: &GroupDrag) {
        let Some(group) = self.groups.get(&drag.gid).cloned() else {
            return;
        };
        for (id, _) in &drag.members {
            if wall.terminal(id).is_some() {
                wall.remember_position(id);
            }
        }
        let origin = match (drag.origin, drag.delta) {
            (Some(orig), Some(delta)) => Some(Point::new(orig.x + delta.x, orig.y + delta.y)),
            _ => group.origin,
        };
        self.set_group_in_store(&drag.gid, Group { origin, ..group });
    }

    /// Where tiled member `id` dragged by its title bar would drop right now:
    /// index in `shown_members` of the nearest tile centre and that tile's rect.
    pub fn tile_drop<W: WallHost>(&self, wall: &W, id: &str) -> Option<TileDrop> {
        let gid = self.group_of(id)?;
        let group = self.groups.get(&gid)?;
        if group.layout != GroupLayout::Tile {
            return None;
        }
        let shown = self.shown_members(wall, &gid).ok()?;
        let (places, metrics) = self.tiling(wall, group, &shown)?;
        let mut rects = Vec::with_capacity(shown.len());
        for (idx, member_id) in shown.iter().enumerate() {
            let term = wall.terminal(member_id)?;
            let x = places[idx].x + metrics[idx].shift_x;
            let y = places[idx].y - metrics[idx].shift_y;
            rects.push(Rect::from_top_left(
                x,
                y,
                term.rect.width(),
                term.rect.height(),
            ));
        }
        let dragged_rect = wall.terminal(id)?.rect;
        let centres: Vec<Point> = rects.iter().map(Rect::centre).collect();
        let to = layout::nearest_place(&centres, dragged_rect.centre())?;
        Some(TileDrop {
            to,
            rect: rects[to],
        })
    }

    /// Finish dragging member `id` by its title bar: in a tiled group, inserts
    /// `id` into the nearest tile and re-tiles; in a freeform group, remembers
    /// `id`'s new spot.
    pub fn drop_member<W: WallHost>(
        &mut self,
        wall: &mut W,
        id: &str,
    ) -> Result<(), GroupError> {
        let Some(drop) = self.tile_drop(wall, id) else {
            wall.remember_position(id);
            return Ok(());
        };
        let gid = self
            .group_of(id)
            .ok_or_else(|| GroupError::new(format!("'{id}' is in no group")))?;
        let group = self.get(&gid)?.clone();
        let shown = self.shown_members(wall, &gid)?;
        let mut order: Vec<String> = shown.into_iter().filter(|m| m != id).collect();
        order.insert(drop.to, id.to_string());
        let members = self.with_hidden(wall, &group, &order);
        self.commit(wall, &gid, Group { members, ..group }, &[])
    }

    /// Re-layout group `gid` over its currently shown members (`tile` or `split`).
    pub fn relayout<W: WallHost>(&mut self, wall: &mut W, gid: &str) -> Result<(), GroupError> {
        let group = self.get(gid)?.clone();
        if wall.fullscreen_group_id() == Some(gid) {
            wall.refit_fullscreen();
            return Ok(());
        }
        if group.layout == GroupLayout::Split {
            let fitted = self.fitted_split(wall, gid, group);
            self.layout_split(wall, &fitted);
            return Ok(());
        }
        if group.layout != GroupLayout::Tile {
            return Ok(());
        }
        if let Some(fs_id) = wall.fullscreen_terminal_id() {
            if group.members.iter().any(|m| m == fs_id) {
                return Ok(());
            }
        }
        let shown = self.shown_members(wall, gid)?;
        if shown.is_empty() {
            return Ok(());
        }
        let origin = group.origin.or_else(|| self.origin_of(wall, &shown));
        let group_with_origin = Group {
            origin,
            ..group.clone()
        };
        if let Some((places, metrics)) = self.tiling(wall, &group_with_origin, &shown) {
            for (idx, id) in shown.iter().enumerate() {
                wall.place_body(
                    id,
                    Point::new(
                        places[idx].x + metrics[idx].shift_x,
                        places[idx].y - metrics[idx].shift_y,
                    ),
                );
                wall.remember_position(id);
            }
        }
        if group.origin.is_none() {
            self.set_group_in_store(gid, group_with_origin);
        }
        Ok(())
    }

    /// Re-layout whichever group terminal `id` belongs to, if any.
    pub fn relayout_of<W: WallHost>(&mut self, wall: &mut W, id: &str) -> Result<(), GroupError> {
        if let Some(gid) = self.group_of(id) {
            self.relayout(wall, &gid)?;
        }
        Ok(())
    }

    /// Re-layout every group in creation order (e.g. after pruning dead sessions).
    pub fn relayout_all<W: WallHost>(&mut self, wall: &mut W) -> Result<(), GroupError> {
        for gid in self.ids() {
            self.relayout(wall, &gid)?;
        }
        Ok(())
    }

    // ── frame, header & Z-depth geometry ─────────────────────────────────────

    /// Group `gid`'s backdrop rectangle at `zoom` (padded union of shown member
    /// footprints, or padded split rectangle), or `None` when no member is shown.
    /// `bare` names a member whose open Agent View is excluded.
    pub fn frame_rect<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
        zoom: f64,
        bare: Option<&str>,
    ) -> Result<Option<Rect>, GroupError> {
        let shown = self.shown_members(wall, gid)?;
        if shown.is_empty() {
            return Ok(None);
        }
        let group = self.get(gid)?;
        let pad = wall.group_config().group_pad;
        if group.layout == GroupLayout::Split {
            let (Some(origin), Some(split)) = (group.origin, group.split.as_ref()) else {
                return Ok(None);
            };
            let rect = Rect::from_top_left(origin.x, origin.y, split.w, split.h);
            return Ok(Some(layout::pad_rect(rect, pad)));
        }
        let rects: Vec<Rect> = shown
            .iter()
            .filter_map(|id| {
                if bare == Some(id.as_str()) {
                    wall.canvas_window_rect(id, zoom)
                } else {
                    wall.canvas_footprint_rect(id, zoom)
                }
            })
            .collect();
        Ok(layout::union_rect(&rects).map(|u| layout::pad_rect(u, pad)))
    }

    /// Header bar rectangle above group `gid`'s backdrop at `zoom`, or `None`
    /// when no member is shown.
    pub fn header_rect<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
        zoom: f64,
        bare: Option<&str>,
    ) -> Result<Option<Rect>, GroupError> {
        let Some(frame) = self.frame_rect(wall, gid, zoom, bare)? else {
            return Ok(None);
        };
        let header_h = wall.group_header_height(gid, zoom);
        Ok(Some(Rect {
            min_x: frame.min_x,
            min_y: frame.max_y,
            max_x: frame.max_x,
            max_y: frame.max_y + header_h,
        }))
    }

    /// Combined canvas bounds (backdrop frame + header) of group `gid` at `zoom`.
    pub fn box_rect<W: WallHost>(
        &self,
        wall: &W,
        gid: &str,
        zoom: f64,
        bare: Option<&str>,
    ) -> Result<Option<Rect>, GroupError> {
        let frame = self.frame_rect(wall, gid, zoom, bare)?;
        let header = self.header_rect(wall, gid, zoom, bare)?;
        Ok(match (frame, header) {
            (Some(f), Some(h)) => layout::union_rect(&[f, h]),
            (f, _) => f,
        })
    }

    /// Record world Z depths assigned to each group's backdrop frame
    /// ([`crate::layout::z_stack_layout`]).
    pub fn set_depths(&mut self, depths: HashMap<String, f64>) {
        self.group_z = depths;
    }

    /// World Z depth of group `gid`'s backdrop frame (defaults to [`DEFAULT_GROUP_Z`]).
    pub fn depth_of(&self, gid: &str) -> f64 {
        self.group_z.get(gid).copied().unwrap_or(DEFAULT_GROUP_Z)
    }

    /// Frontmost group whose header strip contains world `point` at `zoom`.
    pub fn header_at<W: WallHost>(&self, wall: &W, point: Point, zoom: f64) -> Option<String> {
        let mut ordered = self.ids();
        ordered.sort_by(|a, b| {
            self.depth_of(b)
                .partial_cmp(&self.depth_of(a))
                .unwrap_or(Ordering::Equal)
        });
        for gid in ordered {
            if let Ok(Some(header)) = self.header_rect(wall, &gid, zoom, None) {
                if header.contains(point) {
                    return Some(gid);
                }
            }
        }
        None
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn new_group_id(&self) -> String {
        let mut n = 1u64;
        while self.groups.contains_key(&format!("g-{n}")) {
            n += 1;
        }
        format!("g-{n}")
    }

    fn set_group_in_store(&mut self, gid: &str, group: Group) {
        let other_ids: Vec<String> = self
            .groups
            .keys()
            .filter(|other| other.as_str() != gid)
            .cloned()
            .collect();
        for other in other_ids {
            let taken: Vec<String> = self.groups[&other]
                .members
                .iter()
                .filter(|id| group.members.contains(id))
                .cloned()
                .collect();
            if !taken.is_empty() {
                self.drop_members_from_store(&other, &taken);
            }
        }
        self.groups.insert(gid.to_string(), group);
    }

    fn delete_group_from_store(&mut self, gid: &str) {
        self.groups.remove(gid);
        self.fit_memo.remove(gid);
        self.group_z.remove(gid);
    }

    fn drop_members_from_store(&mut self, gid: &str, drop_ids: &[String]) {
        let Some(group) = self.groups.get_mut(gid) else {
            return;
        };
        group.members.retain(|id| !drop_ids.contains(id));
        if let Some(split) = group.split.as_mut() {
            for id in drop_ids {
                if let Some(next_tree) = split::remove(&split.tree, id) {
                    split.tree = next_tree;
                }
            }
        }
        if group.members.is_empty() {
            self.delete_group_from_store(gid);
        }
    }

    fn validate_terminals<W: WallHost, S: AsRef<str>>(
        &self,
        wall: &W,
        ids: &[S],
    ) -> Result<Vec<String>, GroupError> {
        let mut out = Vec::with_capacity(ids.len());
        for id_ref in ids {
            let id = id_ref.as_ref();
            if wall.terminal(id).is_none() {
                return Err(GroupError::new(format!("no terminal '{id}'")));
            }
            if !out.iter().any(|existing| existing == id) {
                out.push(id.to_string());
            }
        }
        Ok(out)
    }

    fn groups_of(&self, ids: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if let Some(gid) = self.group_of(id) {
                if seen.insert(gid.clone()) {
                    out.push(gid);
                }
            }
        }
        out
    }

    fn with_hidden<W: WallHost>(
        &self,
        wall: &W,
        group: &Group,
        shown_order: &[String],
    ) -> Vec<String> {
        let mut shown_iter = shown_order.iter();
        group
            .members
            .iter()
            .map(|member| {
                if wall.is_shown(member) {
                    shown_iter
                        .next()
                        .cloned()
                        .unwrap_or_else(|| member.clone())
                } else {
                    member.clone()
                }
            })
            .collect()
    }

    fn check_group(group: &Group) -> Result<(), GroupError> {
        if group.layout == GroupLayout::Split {
            let Some(split) = group.split.as_ref() else {
                return Err(GroupError::new("a split needs its rect"));
            };
            if split.w <= 0.0
                || split.w.is_nan()
                || split.h <= 0.0
                || split.h.is_nan()
                || group.origin.is_none()
            {
                return Err(GroupError::new("a split needs its rect"));
            }
            split::check_tree(&split.tree, &group.members)
                .map_err(|err| GroupError::new(err.to_string()))?;
        }
        if group.columns < 1 {
            return Err(GroupError::new(format!(
                "columns must be a whole number ≥ 1, not '{}'",
                group.columns
            )));
        }
        if !group.gap.is_finite() || group.gap < 0.0 {
            return Err(GroupError::new(format!(
                "gap must be a number ≥ 0, not '{}'",
                group.gap
            )));
        }
        Ok(())
    }

    fn metrics_of<W: WallHost>(&self, wall: &W, id: &str) -> Option<TileMetrics> {
        if let Some(metrics) = wall.agent_tile_metrics(id) {
            return Some(metrics);
        }
        let rect = wall.terminal(id)?.rect;
        Some(TileMetrics {
            w: rect.width(),
            h: rect.height(),
            shift_x: 0.0,
            shift_y: 0.0,
            tile_box: Rect {
                max_y: rect.max_y + BAR_ROOM,
                ..rect
            },
        })
    }

    fn origin_of<W: WallHost>(&self, wall: &W, ids: &[String]) -> Option<Point> {
        let boxes: Vec<Rect> = ids
            .iter()
            .filter_map(|id| self.metrics_of(wall, id).map(|m| m.tile_box))
            .collect();
        let u = layout::union_rect(&boxes)?;
        Some(Point::new(u.min_x, u.max_y))
    }

    fn tiling<W: WallHost>(
        &self,
        wall: &W,
        group: &Group,
        shown: &[String],
    ) -> Option<(Vec<Point>, Vec<TileMetrics>)> {
        let mut metrics = Vec::with_capacity(shown.len());
        let mut sizes = Vec::with_capacity(shown.len());
        for id in shown {
            let m = self.metrics_of(wall, id)?;
            sizes.push(LayoutSize::new(m.w, m.h));
            metrics.push(m);
        }
        let origin = group.origin.or_else(|| self.origin_of(wall, shown))?;
        let laid = layout::tile_layout(
            &sizes,
            TileOptions {
                origin,
                columns: group.columns,
                gap: group.gap,
                bar_room: BAR_ROOM,
            },
        )
        .ok()?;
        Some((laid.places, metrics))
    }

    fn commit<W: WallHost>(
        &mut self,
        wall: &mut W,
        gid: &str,
        group: Group,
        touched: &[String],
    ) -> Result<(), GroupError> {
        self.set_group_in_store(gid, group);
        self.relayout(wall, gid)?;
        for other in touched {
            if other != gid && self.groups.contains_key(other) {
                self.relayout(wall, other)?;
            }
        }
        self.sync_bars(wall);
        Ok(())
    }

    // ── split internals ──────────────────────────────────────────────────────

    fn split_rect_of<W: WallHost>(&self, wall: &W, group: &Group) -> Option<Rect> {
        if let Some(fs_rect) = wall.fullscreen_split_rect(group) {
            return Some(fs_rect);
        }
        let origin = group.origin?;
        let split = group.split.as_ref()?;
        Some(Rect::from_top_left(origin.x, origin.y, split.w, split.h))
    }

    fn pane_rects_of<W: WallHost>(&self, wall: &W, group: &Group) -> PaneMap {
        let (Some(split), Some(rect)) = (group.split.as_ref(), self.split_rect_of(wall, group))
        else {
            return PaneMap::new();
        };
        split::panes(&split.tree, rect, wall.group_config().split_gap)
    }

    fn cell_stride_of<W: WallHost>(&self, wall: &W, group: &Group) -> Point {
        group
            .members
            .iter()
            .find_map(|id| wall.terminal(id).map(|t| t.cell_stride))
            .unwrap_or(Point::new(1.0, 1.0))
    }

    fn bar_room_of<W: WallHost>(&self, wall: &W, group: &Group) -> f64 {
        let Some(split) = group.split.as_ref() else {
            return 0.0;
        };
        if split.bars {
            wall.group_config().split_bar_rows * self.cell_stride_of(wall, group).y
        } else {
            0.0
        }
    }

    fn frame_padding_of<W: WallHost>(&self, wall: &W, id: &str) -> Point {
        let Some(term) = wall.terminal(id) else {
            return Point::new(0.0, 0.0);
        };
        Point::new(
            term.rect.width() - (term.cols as f64) * term.cell_stride.x,
            term.rect.height() - (term.rows as f64) * term.cell_stride.y,
        )
    }

    fn min_pane_size<W: WallHost>(&self, wall: &W, group: &Group, id: &str) -> Size {
        let stride = self.cell_stride_of(wall, group);
        let room = self.bar_room_of(wall, group);
        let frame = group
            .members
            .iter()
            .find(|member| wall.terminal(member).is_some())
            .map(|member| self.frame_padding_of(wall, member))
            .unwrap_or(Point::new(0.0, 0.0));
        let inset = wall.agent_inset(id);
        Size::new(
            (MIN_PANE_COLS as f64) * stride.x + frame.x + inset.left + inset.right,
            (MIN_PANE_ROWS as f64) * stride.y + frame.y + room + inset.top + inset.bottom,
        )
    }

    fn fitted_split<W: WallHost>(&mut self, wall: &W, gid: &str, group: Group) -> Group {
        let Some(split) = group.split.as_ref().cloned() else {
            return group;
        };
        let split_gap = wall.group_config().split_gap;
        let current_shape = SplitShape {
            tree: split.tree.clone(),
            w: split.w,
            h: split.h,
        };

        if let Some(memo) = self.fit_memo.get(gid).cloned() {
            if memo.after == current_shape {
                let old_group = Group {
                    split: Some(GroupSplit {
                        tree: memo.before.tree.clone(),
                        w: memo.before.w,
                        h: memo.before.h,
                        bars: split.bars,
                    }),
                    ..group.clone()
                };
                let min_old = split::min_size(
                    &memo.before.tree,
                    |id| self.min_pane_size(wall, &old_group, id),
                    split_gap,
                );
                if let Some(old_rect) = self.split_rect_of(wall, &old_group) {
                    let refitted = split::fit_ratios(
                        &memo.before.tree,
                        old_rect,
                        split_gap,
                        |id| self.min_pane_size(wall, &old_group, id),
                    );
                    if min_old.w <= memo.before.w + EDGE_EPS
                        && min_old.h <= memo.before.h + EDGE_EPS
                        && refitted == memo.before.tree
                    {
                        self.fit_memo.remove(gid);
                        self.set_group_in_store(gid, old_group.clone());
                        return old_group;
                    }
                }
            } else {
                self.fit_memo.remove(gid);
            }
        }

        let min = split::min_size(
            &split.tree,
            |id| self.min_pane_size(wall, &group, id),
            split_gap,
        );
        let mut next = group.clone();
        if min.w > split.w + EDGE_EPS || min.h > split.h + EDGE_EPS {
            next.split = Some(GroupSplit {
                w: split.w.max(min.w),
                h: split.h.max(min.h),
                ..split.clone()
            });
        }
        if let (Some(next_split), Some(next_rect)) =
            (next.split.clone(), self.split_rect_of(wall, &next))
        {
            let fitted_tree = split::fit_ratios(
                &next_split.tree,
                next_rect,
                split_gap,
                |id| self.min_pane_size(wall, &next, id),
            );
            if fitted_tree != next_split.tree {
                next.split = Some(GroupSplit {
                    tree: fitted_tree,
                    ..next_split
                });
            }
        }

        if next != group {
            if let Some(next_split) = next.split.as_ref() {
                let kept = self
                    .fit_memo
                    .get(gid)
                    .map(|m| m.before.clone())
                    .unwrap_or(current_shape);
                self.fit_memo.insert(
                    gid.to_string(),
                    FitMemo {
                        before: kept,
                        after: SplitShape {
                            tree: next_split.tree.clone(),
                            w: next_split.w,
                            h: next_split.h,
                        },
                    },
                );
            }
            self.set_group_in_store(gid, next.clone());
        }
        next
    }

    fn ensure_fits<W: WallHost>(
        &self,
        wall: &W,
        group: &Group,
        why: &str,
    ) -> Result<(), GroupError> {
        let split = group
            .split
            .as_ref()
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        let min = split::min_size(
            &split.tree,
            |id| self.min_pane_size(wall, group, id),
            wall.group_config().split_gap,
        );
        if min.w > split.w + EDGE_EPS || min.h > split.h + EDGE_EPS {
            return Err(GroupError::new(format!(
                "{why}: each pane needs at least {MIN_PANE_COLS}×{MIN_PANE_ROWS}"
            )));
        }
        Ok(())
    }

    fn node_ratio(tree: &SplitNode, path: &[BranchSide]) -> Result<f64, GroupError> {
        let mut node = tree;
        for step in path {
            match node {
                SplitNode::Leaf { .. } => {
                    return Err(GroupError::new("a pane has no divider"));
                }
                SplitNode::Branch { a, b, .. } => {
                    node = match step {
                        BranchSide::A => a,
                        BranchSide::B => b,
                    };
                }
            }
        }
        match node {
            SplitNode::Leaf { .. } => Err(GroupError::new("a pane has no divider")),
            SplitNode::Branch { ratio, .. } => Ok(*ratio),
        }
    }

    fn split_plan<W: WallHost>(
        &self,
        wall: &W,
        at: &str,
        id: &str,
        dir: SplitInsertDir,
    ) -> Result<SplitPlan, GroupError> {
        if wall.terminal(at).is_none() {
            return Err(GroupError::new(format!("no terminal '{at}'")));
        }
        let existing_gid = self.group_of(at);
        let existing_group = existing_gid
            .as_deref()
            .and_then(|gid| self.groups.get(gid))
            .cloned();
        let mut touched = Vec::new();
        let (gid, base_group) = match (existing_gid, existing_group) {
            (Some(gid), Some(group)) if group.layout == GroupLayout::Split => (gid, group),
            (maybe_gid, _) => {
                if let Some(old_gid) = maybe_gid {
                    touched.push(old_gid);
                }
                let new_gid = self.new_group_id();
                let seed = Group {
                    name: String::new(),
                    layout: GroupLayout::Tile,
                    columns: 1,
                    gap: wall.group_config().group_gap,
                    origin: None,
                    members: vec![at.to_string()],
                    split: None,
                };
                (new_gid, self.as_split(wall, seed)?)
            }
        };
        if let Some(id_gid) = self.group_of(id) {
            if id_gid != gid && !touched.contains(&id_gid) {
                touched.push(id_gid);
            }
        }

        let split = base_group
            .split
            .as_ref()
            .ok_or_else(|| GroupError::new("a split needs its rect"))?;
        let tree = split::insert(&split.tree, at, id, dir.split_dir(), InsertSide::After);
        let next = Group {
            members: split::leaves(&tree),
            split: Some(GroupSplit {
                tree,
                ..split.clone()
            }),
            ..base_group
        };
        self.ensure_fits(
            wall,
            &next,
            &format!("no room to split '{at}' {}", dir.as_str()),
        )?;
        Ok(SplitPlan { gid, next, touched })
    }

    fn as_split<W: WallHost>(&self, wall: &W, group: Group) -> Result<Group, GroupError> {
        if group.members.is_empty() {
            return Err(GroupError::new("a split needs at least one terminal"));
        }
        let bars = wall.group_config().split_bars;
        let probe = Group {
            split: Some(GroupSplit {
                w: 1.0,
                h: 1.0,
                bars,
                tree: SplitNode::leaf(group.members[0].clone()),
            }),
            ..group.clone()
        };
        let room = self.bar_room_of(wall, &probe);
        let rects: Vec<Rect> = group
            .members
            .iter()
            .filter_map(|id| wall.terminal(id).map(|t| t.rect))
            .collect();
        let u = layout::union_rect(&rects)
            .ok_or_else(|| GroupError::new("a split needs its terminals on the wall"))?;
        let columns = group.columns.min(group.members.len()).max(1);
        let tree = split::from_members(&group.members, columns)
            .map_err(|err| GroupError::new(err.to_string()))?;
        Ok(Group {
            layout: GroupLayout::Split,
            origin: Some(Point::new(u.min_x, u.max_y + room)),
            split: Some(GroupSplit {
                w: u.width(),
                h: u.height() + room,
                bars,
                tree,
            }),
            ..group
        })
    }

    fn layout_split<W: WallHost>(&self, wall: &mut W, group: &Group) {
        if let Some(fs_id) = wall.fullscreen_terminal_id() {
            if group.members.iter().any(|m| m == fs_id) {
                return;
            }
        }
        let room = self.bar_room_of(wall, group);
        for (id, &pane_rect) in &self.pane_rects_of(wall, group) {
            let Some(term) = wall.terminal(id) else {
                continue;
            };
            let stride = term.cell_stride;
            let frame = self.frame_padding_of(wall, id);
            let inset = wall.agent_inset(id);
            let raw_cols =
                ((pane_rect.width() - inset.left - inset.right - frame.x) / stride.x + EDGE_EPS)
                    .floor() as i64;
            let raw_rows = ((pane_rect.height() - room - inset.top - inset.bottom - frame.y)
                / stride.y
                + EDGE_EPS)
                .floor() as i64;
            let cols = raw_cols.max(MIN_PANE_COLS as i64) as u32;
            let rows = raw_rows.max(MIN_PANE_ROWS as i64) as u32;
            if cols != term.cols || rows != term.rows {
                wall.resize_terminal(id, cols, rows);
            }
            wall.place_body(
                id,
                Point::new(pane_rect.min_x + inset.left, pane_rect.max_y - room - inset.top),
            );
            wall.remember_position(id);
            wall.set_bar_room(id, Some(room));
        }
    }

    fn sync_bars<W: WallHost>(&self, wall: &mut W) {
        for id in wall.terminal_ids() {
            if !self.splits(&id) {
                wall.set_bar_room(&id, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Stand-in wall for tools/wall-groups.test.mjs (`w × h` body rects) ────

    #[derive(Clone, Debug)]
    struct RectTerm {
        position: Point,
        w: f64,
        h: f64,
    }

    struct FakeTileWall {
        terminals: HashMap<String, RectTerm>,
        order: Vec<String>,
        hidden: HashSet<String>,
        remembered: HashMap<String, Point>,
        config: GroupConfig,
    }

    impl FakeTileWall {
        fn new(specs: &[(&str, f64, f64, f64, f64)]) -> (FakeTileWall, Groups) {
            let mut terminals = HashMap::new();
            let mut order = Vec::new();
            for &(id, w, h, x, y) in specs {
                terminals.insert(
                    id.to_string(),
                    RectTerm {
                        position: Point::new(x, y),
                        w,
                        h,
                    },
                );
                order.push(id.to_string());
            }
            (
                FakeTileWall {
                    terminals,
                    order,
                    hidden: HashSet::new(),
                    remembered: HashMap::new(),
                    config: GroupConfig {
                        group_gap: 2.0,
                        ..GroupConfig::default()
                    },
                },
                Groups::new(),
            )
        }

        fn at(&self, id: &str) -> Point {
            self.terminals[id].position
        }

        fn set_hidden(&mut self, id: &str, on: bool) {
            if on {
                self.hidden.insert(id.to_string());
            } else {
                self.hidden.remove(id);
            }
        }
    }

    impl WallHost for FakeTileWall {
        fn group_config(&self) -> GroupConfig {
            self.config
        }

        fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
            let t = self.terminals.get(id)?;
            Some(TerminalGeometry {
                position: t.position,
                rect: Rect::from_top_left(t.position.x, t.position.y, t.w, t.h),
                cols: 80,
                rows: 24,
                cell_stride: Point::new(1.0, 1.0),
            })
        }

        fn terminal_ids(&self) -> Vec<String> {
            self.order.clone()
        }

        fn is_shown(&self, id: &str) -> bool {
            self.terminals.contains_key(id) && !self.hidden.contains(id)
        }

        fn place_body(&mut self, id: &str, pos: Point) {
            if let Some(t) = self.terminals.get_mut(id) {
                t.position = pos;
            }
        }

        fn remember_position(&mut self, id: &str) {
            if let Some(t) = self.terminals.get(id) {
                self.remembered.insert(id.to_string(), t.position);
            }
        }
    }

    // ── Stand-in wall for tools/wall-split.test.mjs (cell grids + PAD) ───────

    const PAD: f64 = 0.3;
    const SX: f64 = 1.0;
    const SY: f64 = 2.0;

    #[derive(Clone, Debug)]
    struct GridTerm {
        cols: u32,
        rows: u32,
        position: Point,
    }

    struct FakeSplitWall {
        terminals: HashMap<String, GridTerm>,
        order: Vec<String>,
        hidden: HashSet<String>,
        remembered: HashMap<String, Point>,
        bars: HashMap<String, f64>,
        insets: HashMap<String, PaneInset>,
        config: GroupConfig,
    }

    impl FakeSplitWall {
        fn new(specs: &[(&str, u32, u32, f64, f64)]) -> (FakeSplitWall, Groups) {
            let mut terminals = HashMap::new();
            let mut order = Vec::new();
            for &(id, cols, rows, x, y) in specs {
                terminals.insert(
                    id.to_string(),
                    GridTerm {
                        cols,
                        rows,
                        position: Point::new(x, y),
                    },
                );
                order.push(id.to_string());
            }
            (
                FakeSplitWall {
                    terminals,
                    order,
                    hidden: HashSet::new(),
                    remembered: HashMap::new(),
                    bars: HashMap::new(),
                    insets: HashMap::new(),
                    config: GroupConfig {
                        split_gap: 0.5,
                        split_bar_rows: 1.5,
                        split_bars: true,
                        ..GroupConfig::default()
                    },
                },
                Groups::new(),
            )
        }

        fn add(&mut self, id: &str, cols: u32, rows: u32) {
            self.terminals.insert(
                id.to_string(),
                GridTerm {
                    cols,
                    rows,
                    position: Point::new(999.0, 999.0),
                },
            );
            self.order.push(id.to_string());
        }

        fn terminal_rect(&self, id: &str) -> Rect {
            let g = &self.terminals[id];
            Rect {
                min_x: g.position.x,
                max_x: g.position.x + (g.cols as f64) * SX + 2.0 * PAD,
                min_y: g.position.y - (g.rows as f64) * SY - 2.0 * PAD,
                max_y: g.position.y,
            }
        }

        fn forget(&mut self, groups: &mut Groups, id: &str) {
            self.remembered.remove(id);
            self.hidden.remove(id);
            groups.forget_terminal(id);
        }
    }

    impl WallHost for FakeSplitWall {
        fn group_config(&self) -> GroupConfig {
            self.config
        }

        fn terminal(&self, id: &str) -> Option<TerminalGeometry> {
            let g = self.terminals.get(id)?;
            Some(TerminalGeometry {
                position: g.position,
                rect: self.terminal_rect(id),
                cols: g.cols,
                rows: g.rows,
                cell_stride: Point::new(SX, SY),
            })
        }

        fn terminal_ids(&self) -> Vec<String> {
            self.order.clone()
        }

        fn is_shown(&self, id: &str) -> bool {
            self.terminals.contains_key(id) && !self.hidden.contains(id)
        }

        fn place_body(&mut self, id: &str, pos: Point) {
            if let Some(g) = self.terminals.get_mut(id) {
                g.position = pos;
            }
        }

        fn remember_position(&mut self, id: &str) {
            if let Some(g) = self.terminals.get(id) {
                self.remembered.insert(id.to_string(), g.position);
            }
        }

        fn resize_terminal(&mut self, id: &str, cols: u32, rows: u32) {
            if let Some(g) = self.terminals.get_mut(id) {
                g.cols = cols;
                g.rows = rows;
            }
        }

        fn set_bar_room(&mut self, id: &str, room: Option<f64>) {
            match room {
                None => {
                    self.bars.remove(id);
                }
                Some(r) => {
                    self.bars.insert(id.to_string(), r);
                }
            }
        }

        fn agent_inset(&self, id: &str) -> PaneInset {
            self.insets.get(id).copied().unwrap_or_default()
        }
    }

    /// Port of `expectFilled(wall, gid)` in `tools/wall-split.test.mjs`.
    fn assert_filled(wall: &FakeSplitWall, groups: &Groups, gid: &str) {
        let g = groups.get(gid).unwrap();
        let split = g.split.as_ref().unwrap();
        let room = if split.bars {
            wall.config.split_bar_rows * SY
        } else {
            0.0
        };
        for (id, &pr) in &groups.pane_rects(wall, gid).unwrap() {
            let r = wall.terminal_rect(id);
            assert_eq!(
                (id, r.min_x, r.max_y),
                (id, pr.min_x, pr.max_y - room),
                "pane '{id}' top-left mismatch"
            );
            assert!(r.max_x <= pr.max_x + 1e-9);
            assert!(r.min_y >= pr.min_y - 1e-9);
            assert!(pr.max_x - r.max_x < SX);
            assert!(r.min_y - pr.min_y < SY);
        }
    }

    // ── All 8 test blocks from tools/wall-groups.test.mjs ────────────────────

    #[test]
    fn new_group_tiles_members_in_ceil_sqrt_n_columns_and_remembers_spots() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 300.0, -40.0),
            ("b", 100.0, 50.0, 500.0, -90.0),
            ("c", 100.0, 50.0, 310.0, -200.0),
            ("d", 100.0, 50.0, 50.0, -60.0),
        ]);
        let gid = groups
            .create(&mut wall, &["a", "b", "c", "d"], CreateGroupOptions::default())
            .unwrap();
        assert_eq!(gid, "g-1");

        let g = groups.get(&gid).unwrap().clone();
        assert_eq!(g.columns, 2);
        assert_eq!(g.origin, Some(Point::new(50.0, -40.0 + BAR_ROOM)));

        let places = layout::tile_layout(
            &[LayoutSize::new(100.0, 50.0); 4],
            TileOptions::new(g.origin.unwrap(), 2, 2.0),
        )
        .unwrap()
        .places;
        let actual: Vec<Point> = ["a", "b", "c", "d"].iter().map(|id| wall.at(id)).collect();
        assert_eq!(actual, places);
        assert_eq!(wall.remembered.get("d").copied(), Some(places[3]));
        assert_eq!(groups.name_of(&gid).unwrap(), "group 1");
        assert!(groups.tiles("a"));
    }

    #[test]
    fn columns_hiding_and_showing_retile_and_restored_member_reclaims_tile() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 0.0, 0.0),
            ("c", 100.0, 50.0, 0.0, 0.0),
        ]);
        let gid = groups
            .create(
                &mut wall,
                &["a", "b", "c"],
                CreateGroupOptions {
                    columns: Some(3),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        let before: Vec<Point> = ["a", "b", "c"].iter().map(|id| wall.at(id)).collect();
        assert_eq!(before[1].y, before[0].y); // one row

        groups.set_columns(&mut wall, &gid, 1).unwrap();
        assert_eq!(wall.at("b").x, wall.at("a").x); // one column

        groups.set_columns(&mut wall, &gid, 3).unwrap();
        wall.set_hidden("b", true);
        groups.relayout_of(&mut wall, "b").unwrap();
        assert_eq!(wall.at("c"), before[1]); // c closes the gap

        wall.set_hidden("b", false);
        groups.relayout_of(&mut wall, "b").unwrap();
        let restored: Vec<Point> = ["a", "b", "c"].iter().map(|id| wall.at(id)).collect();
        assert_eq!(restored, before);
    }

    #[test]
    fn group_moves_whole_members_and_tiling_and_keeps_where_left() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 0.0, 0.0),
        ]);
        let gid = groups
            .create(&mut wall, &["a", "b"], CreateGroupOptions::default())
            .unwrap();
        let a0 = wall.at("a");
        let o0 = groups.get(&gid).unwrap().origin.unwrap();

        groups.move_group(&mut wall, &gid, 30.0, -20.0).unwrap();
        assert_eq!(wall.at("a"), Point::new(a0.x + 30.0, a0.y - 20.0));
        assert_eq!(
            groups.get(&gid).unwrap().origin,
            Some(Point::new(o0.x + 30.0, o0.y - 20.0))
        );
        assert_eq!(wall.remembered.get("a").copied(), Some(wall.at("a")));

        groups.relayout(&mut wall, &gid).unwrap();
        assert_eq!(wall.at("a"), Point::new(a0.x + 30.0, a0.y - 20.0));
    }

    #[test]
    fn tiled_member_dropped_elsewhere_takes_nearest_tile_and_others_shift() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 0.0, 0.0),
            ("c", 100.0, 50.0, 0.0, 0.0),
        ]);
        let gid = groups
            .create(
                &mut wall,
                &["a", "b", "c"],
                CreateGroupOptions {
                    columns: Some(3),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        let places: Vec<Point> = ["a", "b", "c"].iter().map(|id| wall.at(id)).collect();

        // Dragged over the third tile.
        wall.place_body("a", Point::new(places[2].x + 10.0, places[2].y - 5.0));
        assert_eq!(
            groups.tile_drop(&wall, "a"),
            Some(TileDrop {
                to: 2,
                rect: Rect {
                    min_x: places[2].x,
                    max_x: places[2].x + 100.0,
                    min_y: places[2].y - 50.0,
                    max_y: places[2].y,
                }
            })
        );

        groups.drop_member(&mut wall, "a").unwrap();
        assert_eq!(groups.get(&gid).unwrap().members, vec!["b", "c", "a"]);
        let after: Vec<Point> = ["b", "c", "a"].iter().map(|id| wall.at(id)).collect();
        assert_eq!(after, places);
    }

    #[test]
    fn tiled_member_steps_across_row_or_column_and_hidden_members_keep_places() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 0.0, 0.0),
            ("c", 100.0, 50.0, 0.0, 0.0),
            ("d", 100.0, 50.0, 0.0, 0.0),
            ("e", 100.0, 50.0, 0.0, 0.0),
        ]);
        let gid = groups
            .create(
                &mut wall,
                &["a", "b", "c", "d", "e"],
                CreateGroupOptions {
                    columns: Some(2),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        let places: Vec<Point> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|id| wall.at(id))
            .collect();

        assert_eq!(groups.move_pane(&mut wall, "a", Direction::Left).unwrap(), None);
        assert_eq!(groups.move_pane(&mut wall, "b", Direction::Right).unwrap(), None);
        assert_eq!(groups.move_pane(&mut wall, "e", Direction::Right).unwrap(), None);
        assert_eq!(
            groups.move_pane(&mut wall, "d", Direction::Up).unwrap(),
            Some("b".to_string())
        );
        assert_eq!(
            groups.get(&gid).unwrap().members,
            vec!["a", "d", "c", "b", "e"]
        );
        assert_eq!(wall.at("d"), places[1]);

        assert_eq!(
            groups.move_pane(&mut wall, "c", Direction::Down).unwrap(),
            Some("e".to_string())
        );
        assert_eq!(
            groups.move_pane(&mut wall, "a", Direction::Right).unwrap(),
            Some("d".to_string())
        );
        assert_eq!(
            groups.get(&gid).unwrap().members,
            vec!["d", "a", "e", "b", "c"]
        );

        wall.set_hidden("e", true);
        assert_eq!(
            groups.move_pane(&mut wall, "c", Direction::Left).unwrap(),
            Some("b".to_string())
        );
        assert_eq!(
            groups.get(&gid).unwrap().members,
            vec!["d", "a", "e", "c", "b"]
        );

        let freeform_gid = groups
            .create(
                &mut wall,
                &["a"],
                CreateGroupOptions {
                    layout: Some(GroupLayout::Freeform),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        let err = groups.move_pane(&mut wall, "a", Direction::Up).unwrap_err();
        assert!(err.message().contains("freeform"));
        assert!(!freeform_gid.is_empty());
    }

    #[test]
    fn removing_ungrouping_and_regrouping_leaves_spots_and_drops_empty_group() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 100.0, 50.0, 0.0, 0.0),
            ("c", 100.0, 50.0, 0.0, 0.0),
        ]);
        let g1 = groups
            .create(
                &mut wall,
                &["a", "b", "c"],
                CreateGroupOptions {
                    columns: Some(3),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        let c_at = wall.at("c");
        groups.remove(&mut wall, &["c"]).unwrap();
        assert_eq!(groups.get(&g1).unwrap().members, vec!["a", "b"]);
        assert_eq!(wall.at("c"), c_at);

        let g2 = groups
            .create(
                &mut wall,
                &["a", "b"],
                CreateGroupOptions {
                    layout: Some(GroupLayout::Freeform),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        assert_eq!(groups.group(&g1), None);
        assert_eq!(groups.group_of("a"), Some(g2.clone()));

        let a_at = wall.at("a");
        groups.ungroup(&mut wall, &g2).unwrap();
        assert_eq!(groups.group_of("a"), None);
        assert_eq!(wall.at("a"), a_at);
    }

    #[test]
    fn freeform_leaves_members_in_place_and_switching_to_tile_starts_at_top_left() {
        let (mut wall, mut groups) = FakeTileWall::new(&[
            ("a", 100.0, 50.0, 0.0, 0.0),
            ("b", 80.0, 40.0, 400.0, -300.0),
        ]);
        let gid = groups
            .create(
                &mut wall,
                &["a", "b"],
                CreateGroupOptions {
                    layout: Some(GroupLayout::Freeform),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();
        assert_eq!(wall.at("b"), Point::new(400.0, -300.0));
        assert!(!groups.tiles("b"));

        groups
            .set_layout(
                &mut wall,
                &gid,
                GroupLayout::Tile,
                SetLayoutOptions {
                    columns: Some(2),
                    gap: None,
                },
            )
            .unwrap();
        assert_eq!(wall.at("a"), Point::new(0.0, 0.0));
        assert_eq!(wall.at("b"), Point::new(102.0, 0.0));

        groups
            .set_layout(&mut wall, &gid, GroupLayout::Freeform, SetLayoutOptions::default())
            .unwrap();
        assert_eq!(wall.at("b"), Point::new(102.0, 0.0));
    }

    #[test]
    fn groups_refuses_invalid_requests_with_diagnostic_messages() {
        let (mut wall, mut groups) = FakeTileWall::new(&[("a", 100.0, 50.0, 0.0, 0.0)]);
        let empty: [&str; 0] = [];
        assert!(groups
            .create(&mut wall, &empty, CreateGroupOptions::default())
            .unwrap_err()
            .message()
            .contains("at least one"));
        assert!(groups
            .create(&mut wall, &["nope"], CreateGroupOptions::default())
            .unwrap_err()
            .message()
            .contains("no terminal 'nope'"));

        let gid = groups
            .create(&mut wall, &["a"], CreateGroupOptions::default())
            .unwrap();
        assert!(groups
            .set_columns(&mut wall, &gid, 0)
            .unwrap_err()
            .message()
            .contains("columns"));
        assert!(GroupLayout::from_str("grid")
            .unwrap_err()
            .message()
            .contains("'tile', 'freeform' or 'split'"));
        assert!(groups
            .get("g-9")
            .unwrap_err()
            .message()
            .contains("no group 'g-9'"));
        assert!(groups.remove(&mut wall, &["a", "a"]).is_ok());
        assert!(groups
            .remove(&mut wall, &["a"])
            .unwrap_err()
            .message()
            .contains("in no group"));
    }

    // ── All 11 non-Agent-View WallGroups tests from tools/wall-split.test.mjs ─

    #[test]
    fn splitting_terminal_right_makes_split_round_window_and_rewraps_both_halves() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 80, 24, 10.0, -5.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        let g = groups.get(&gid).unwrap().clone();
        assert_eq!(g.layout, GroupLayout::Split);
        assert_eq!(g.members, vec!["a", "b"]);
        assert_eq!(g.origin, Some(Point::new(10.0, -5.0 + 1.5 * SY)));
        assert!((g.split.as_ref().unwrap().w - (80.0 * SX + 2.0 * PAD)).abs() < 1e-9);
        assert_filled(&wall, &groups, &gid);
        assert_eq!(wall.terminals["a"].cols, wall.terminals["b"].cols);
        let expected_cols = (((80.0 + 2.0 * PAD - 0.5) / 2.0 - 2.0 * PAD) / SX).floor() as u32;
        assert_eq!(wall.terminals["a"].cols, expected_cols);
        assert_eq!(wall.bars.get("a").copied(), Some(1.5 * SY));

        wall.add("c", 80, 24);
        groups
            .insert_pane(&mut wall, "b", "c", SplitInsertDir::Down)
            .unwrap();
        assert_eq!(groups.get(&gid).unwrap().members, vec!["a", "b", "c"]);
        assert_filled(&wall, &groups, &gid);
        assert_eq!(
            split::neighbor(&groups.pane_rects(&wall, &gid).unwrap(), "b", Direction::Down),
            Some("c".to_string())
        );
    }

    #[test]
    fn split_leaving_half_under_20x5_is_refused_without_changing_anything() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 30, 24, 0.0, 0.0)]);
        wall.add("b", 30, 24);
        assert!(groups
            .can_split(&wall, "a", SplitInsertDir::Right)
            .unwrap_err()
            .message()
            .contains("no room to split 'a' right"));
        assert!(groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap_err()
            .message()
            .contains("at least 20×5"));
        assert!(groups.as_map().is_empty());
        assert!(groups.can_split(&wall, "a", SplitInsertDir::Down).is_ok());
    }

    #[test]
    fn pane_that_leaves_gives_room_to_sibling_and_split_rewraps() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 80, 24, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        groups.remove(&mut wall, &["b"]).unwrap();
        assert_eq!(groups.get(&gid).unwrap().members, vec!["a"]);
        assert_eq!(
            groups.get(&gid).unwrap().split.as_ref().unwrap().tree,
            SplitNode::leaf("a")
        );
        assert_filled(&wall, &groups, &gid);
        assert_eq!(wall.terminals["a"].cols, 80);

        wall.add("c", 80, 24);
        groups
            .insert_pane(&mut wall, "a", "c", SplitInsertDir::Down)
            .unwrap();
        wall.forget(&mut groups, "a");
        assert_eq!(
            groups.group(&gid).unwrap().split.as_ref().unwrap().tree,
            SplitNode::leaf("c")
        );
    }

    #[test]
    fn divider_drag_moves_cut_within_minimums_and_outer_resize_grows_panes() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 100, 24, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        let sr = groups.split_rect(&wall, &gid).unwrap();

        groups.drag_divider(&mut wall, &gid, &[], sr.min_x + 30.0).unwrap();
        assert_filled(&wall, &groups, &gid);
        assert!(wall.terminals["a"].cols < wall.terminals["b"].cols);

        groups.drag_divider(&mut wall, &gid, &[], sr.min_x + 1.0).unwrap();
        assert_eq!(wall.terminals["a"].cols, 20);

        let before = wall.terminals["b"].cols;
        groups
            .resize_split(
                &mut wall,
                &gid,
                Rect {
                    max_x: sr.max_x + 10.0 * SX,
                    ..sr
                },
            )
            .unwrap();
        assert!(wall.terminals["b"].cols > before);
        assert_filled(&wall, &groups, &gid);

        groups
            .resize_split(
                &mut wall,
                &gid,
                Rect {
                    max_x: sr.min_x + 1.0,
                    ..sr
                },
            )
            .unwrap();
        let min = split::min_size(
            &groups.get(&gid).unwrap().split.as_ref().unwrap().tree,
            |_| Size::new(20.0 * SX + 2.0 * PAD, 5.0 * SY + 2.0 * PAD + 1.5 * SY),
            0.5,
        );
        assert!((groups.get(&gid).unwrap().split.as_ref().unwrap().w - min.w).abs() < 1e-9);
    }

    #[test]
    fn inner_border_drags_adjacent_divider_and_outer_border_resizes_split() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 80, 24, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();

        assert_eq!(
            groups
                .split_edge(
                    &wall,
                    "a",
                    Edges {
                        right: true,
                        ..Edges::default()
                    }
                )
                .unwrap(),
            SplitEdgeTarget {
                gid: gid.clone(),
                outer: None,
                divider: Some(DividerTarget {
                    path: Vec::new(),
                    axis: Axis::X,
                }),
            }
        );
        assert_eq!(
            groups
                .split_edge(
                    &wall,
                    "b",
                    Edges {
                        left: true,
                        ..Edges::default()
                    }
                )
                .unwrap()
                .divider,
            Some(DividerTarget {
                path: Vec::new(),
                axis: Axis::X,
            })
        );
        assert_eq!(
            groups
                .split_edge(
                    &wall,
                    "a",
                    Edges {
                        left: true,
                        top: true,
                        ..Edges::default()
                    }
                )
                .unwrap(),
            SplitEdgeTarget {
                gid,
                outer: Some(Edges {
                    left: true,
                    right: false,
                    top: true,
                    bottom: false,
                }),
                divider: None,
            }
        );
    }

    #[test]
    fn bars_toggle_frees_bar_room_for_extra_row_and_restores_it() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 80, 24, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        let rows = wall.terminals["a"].rows;

        groups.set_bars(&mut wall, &gid, false).unwrap();
        assert_eq!(wall.bars.get("a").copied(), Some(0.0));
        assert_eq!(wall.terminals["a"].rows, rows + 1);
        assert_filled(&wall, &groups, &gid);

        groups.set_bars(&mut wall, &gid, true).unwrap();
        assert_eq!(wall.terminals["a"].rows, rows);
    }

    #[test]
    fn tiled_group_converts_to_even_split_and_back_restoring_title_bars() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[
            ("a", 80, 24, 0.0, 0.0),
            ("b", 80, 24, 90.0, 0.0),
            ("c", 80, 24, 0.0, -60.0),
            ("d", 80, 24, 90.0, -60.0),
        ]);
        let gid = groups
            .create(
                &mut wall,
                &["a", "b", "c", "d"],
                CreateGroupOptions {
                    layout: Some(GroupLayout::Tile),
                    columns: Some(2),
                    ..CreateGroupOptions::default()
                },
            )
            .unwrap();

        groups
            .set_layout(&mut wall, &gid, GroupLayout::Split, SetLayoutOptions::default())
            .unwrap();
        let g = groups.get(&gid).unwrap();
        assert_eq!(
            split::leaves(&g.split.as_ref().unwrap().tree),
            vec!["a", "b", "c", "d"]
        );
        assert_filled(&wall, &groups, &gid);
        let pr = groups.pane_rects(&wall, &gid).unwrap();
        assert_eq!(split::neighbor(&pr, "a", Direction::Right), Some("b".to_string()));
        assert_eq!(split::neighbor(&pr, "a", Direction::Down), Some("c".to_string()));

        let size = (wall.terminals["a"].cols, wall.terminals["a"].rows);
        groups
            .set_layout(&mut wall, &gid, GroupLayout::Tile, SetLayoutOptions::default())
            .unwrap();
        assert_eq!(groups.get(&gid).unwrap().split, None);
        assert_eq!((wall.terminals["a"].cols, wall.terminals["a"].rows), size);
        assert!(!wall.bars.contains_key("a"));
    }

    #[test]
    fn group_map_round_trips_split_whole_across_reload() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 80, 24, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Down)
            .unwrap();
        let reloaded = Groups::from_map(groups.as_map().clone());
        assert_eq!(reloaded.group(&gid), groups.group(&gid));
        assert_eq!(
            reloaded.group(&gid).unwrap().split.as_ref().unwrap().tree,
            SplitNode::branch(SplitDir::Col, 0.5, SplitNode::leaf("a"), SplitNode::leaf("b"))
        );
    }

    #[test]
    fn pane_dropped_on_another_swaps_or_docks_and_rewraps_both() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 120, 40, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        wall.add("c", 80, 24);
        groups
            .insert_pane(&mut wall, "b", "c", SplitInsertDir::Down)
            .unwrap();

        let ps = groups.pane_rects(&wall, &gid).unwrap();
        let a_rect = *ps.get("a").unwrap();
        let swap_plan = groups.pane_drop(&wall, "c", a_rect.centre()).unwrap();
        assert_eq!(swap_plan.gid, gid);
        assert_eq!(swap_plan.at, "a");
        assert_eq!(swap_plan.zone, DropZone::Center);
        assert!(swap_plan.fits);
        assert_eq!(swap_plan.rect, a_rect);
        assert_eq!(
            groups.group(&gid).unwrap().split.as_ref().unwrap().tree,
            split::insert(
                &split::insert(&SplitNode::leaf("a"), "a", "b", SplitDir::Row, InsertSide::After),
                "b",
                "c",
                SplitDir::Col,
                InsertSide::After,
            )
        );

        groups.drop_pane(&mut wall, "c", a_rect.centre()).unwrap();
        assert_eq!(groups.get(&gid).unwrap().members, vec!["c", "b", "a"]);
        assert_eq!(
            groups.pane_rects(&wall, &gid).unwrap().get("c").copied(),
            Some(a_rect)
        );
        assert_filled(&wall, &groups, &gid);
        assert!(wall.terminals["c"].rows > wall.terminals["a"].rows);

        // Drop 'a' near 'b's top edge: docks 'a' above 'b'.
        let pb = *groups.pane_rects(&wall, &gid).unwrap().get("b").unwrap();
        let near_top = Point::new(
            (pb.min_x + pb.max_x) / 2.0,
            pb.max_y - (pb.max_y - pb.min_y) * 0.1,
        );
        let dock_plan = groups.pane_drop(&wall, "a", near_top).unwrap();
        assert_eq!(dock_plan.at, "b");
        assert_eq!(dock_plan.zone, DropZone::Up);

        groups.drop_pane(&mut wall, "a", near_top).unwrap();
        assert_eq!(
            groups.get(&gid).unwrap().split.as_ref().unwrap().tree,
            SplitNode::branch(
                SplitDir::Row,
                0.5,
                SplitNode::leaf("c"),
                SplitNode::branch(SplitDir::Col, 0.5, SplitNode::leaf("a"), SplitNode::leaf("b")),
            )
        );
        assert_eq!(groups.get(&gid).unwrap().members, vec!["c", "a", "b"]);
        let final_rects = groups.pane_rects(&wall, &gid).unwrap();
        assert_eq!(
            split::neighbor(&final_rects, "a", Direction::Down),
            Some("b".to_string())
        );
        assert_eq!(
            split::neighbor(&final_rects, "a", Direction::Left),
            Some("c".to_string())
        );
        assert_filled(&wall, &groups, &gid);
    }

    #[test]
    fn drop_off_other_panes_or_squeezing_under_20x5_sends_pane_back() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 60, 30, 0.0, 0.0)]);
        wall.add("b", 60, 30);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Down)
            .unwrap();
        let before_tree = groups
            .group(&gid)
            .unwrap()
            .split
            .as_ref()
            .unwrap()
            .tree
            .clone();
        let pa = *groups.pane_rects(&wall, &gid).unwrap().get("a").unwrap();

        wall.terminals.get_mut("b").unwrap().position.x += 500.0;
        assert_eq!(groups.pane_drop(&wall, "b", Point::new(999.0, 999.0)), None);
        assert_eq!(
            groups
                .drop_pane(&mut wall, "b", Point::new(999.0, 999.0))
                .unwrap(),
            None
        );
        assert_filled(&wall, &groups, &gid);

        let sr = groups.split_rect(&wall, &gid).unwrap();
        groups
            .resize_split(
                &mut wall,
                &gid,
                Rect {
                    max_x: sr.min_x + 30.0 * SX,
                    ..sr
                },
            )
            .unwrap();
        let drop_pt = Point::new(pa.min_x + 1.0, (pa.min_y + pa.max_y) / 2.0);
        let plan = groups.pane_drop(&wall, "b", drop_pt).unwrap();
        assert_eq!(plan.at, "a");
        assert_eq!(plan.zone, DropZone::Left);
        assert!(!plan.fits);
        assert_eq!(groups.drop_pane(&mut wall, "b", drop_pt).unwrap(), None);
        assert_eq!(
            groups.group(&gid).unwrap().split.as_ref().unwrap().tree,
            before_tree
        );
        assert_filled(&wall, &groups, &gid);
    }

    #[test]
    fn split_pane_moves_stepwise_to_neighbour_and_stops_at_edge() {
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 120, 40, 0.0, 0.0)]);
        wall.add("b", 80, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        wall.add("c", 80, 24);
        groups
            .insert_pane(&mut wall, "b", "c", SplitInsertDir::Down)
            .unwrap();

        assert_eq!(groups.move_pane(&mut wall, "a", Direction::Left).unwrap(), None);
        assert_eq!(
            groups.move_pane(&mut wall, "c", Direction::Up).unwrap(),
            Some("b".to_string())
        );
        assert_eq!(groups.get(&gid).unwrap().members, vec!["a", "c", "b"]);
        assert_eq!(
            groups.move_pane(&mut wall, "c", Direction::Left).unwrap(),
            Some("a".to_string())
        );
        assert_eq!(groups.get(&gid).unwrap().members, vec!["c", "a", "b"]);
        assert_filled(&wall, &groups, &gid);

        assert!(Groups::parse_move_dir("sideways")
            .unwrap_err()
            .message()
            .contains("left, right, up or down"));
    }

    #[test]
    fn pane_inset_grows_split_to_fit_minimum_and_restores_when_cleared() {
        // Exercises `_fitted` / `fit_memo` directly via `PaneInset` without needing `wallAgentView`.
        let (mut wall, mut groups) = FakeSplitWall::new(&[("a", 44, 24, 0.0, 0.0)]);
        wall.add("b", 44, 24);
        let gid = groups
            .insert_pane(&mut wall, "a", "b", SplitInsertDir::Right)
            .unwrap();
        let w0 = groups.get(&gid).unwrap().split.as_ref().unwrap().w;

        wall.insets.insert(
            "a".to_string(),
            PaneInset {
                left: 30.5,
                ..PaneInset::default()
            },
        );
        groups.relayout(&mut wall, &gid).unwrap();
        assert!(groups.get(&gid).unwrap().split.as_ref().unwrap().w > w0);
        let pa = *groups.pane_rects(&wall, &gid).unwrap().get("a").unwrap();
        let body = wall.terminal_rect("a");
        assert!((body.min_x - pa.min_x - 30.5).abs() < 1e-9);
        assert!(wall.terminals["a"].cols >= 20);
        assert!(body.max_x <= pa.max_x + 1e-9);

        wall.insets.clear();
        groups.relayout(&mut wall, &gid).unwrap();
        assert_eq!(groups.get(&gid).unwrap().split.as_ref().unwrap().w, w0);
        if let SplitNode::Branch { ratio, .. } =
            &groups.get(&gid).unwrap().split.as_ref().unwrap().tree
        {
            assert_eq!(*ratio, 0.5);
        } else {
            panic!("expected branch");
        }
        assert_eq!(wall.terminals["a"].cols, wall.terminals["b"].cols);
        assert_filled(&wall, &groups, &gid);
    }
}
