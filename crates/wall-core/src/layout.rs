//! Pure geometry of the wall, ported from `wall/wallLayout.js`.
//!
//! Decides where a new terminal lands in the flow grid, how items are shoved
//! aside to clear a claimed region, how the camera frames a rectangle, which
//! border edges or window parts a pointer hits in stacking order, focus
//! cycling, group-contiguous Z-ordering and Z-stack depth assignment, reading
//! order across horizontal row bands, cell resize from border drags, group
//! tiling, nearest-tile drop selection, and freeform lasso hit-testing.
//!
//! Coordinates are world units with **y up** ([`Rect::max_y`] is the top edge,
//! [`Rect::min_y`] is the bottom edge), matching `wall/wallLayout.js`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use crate::geom::{Point, Rect};

/// World units between neighbouring terminals in the flow layout — leaves room
/// for a title bar (which caps at 80% of this gap).
pub const FLOW_GAP: f64 = 12.0;

/// Terminals per row before the flow layout wraps to the next row down.
pub const FLOW_COLUMNS: usize = 3;

/// Maximum world height a title bar takes above its terminal body — the room a
/// tiled row reserves above each member's body.
pub const BAR_ROOM: f64 = FLOW_GAP * 0.8;

/// World Z of a fullscreen terminal — well above the canvas stack and inside
/// the orthographic camera's `z = 500` near plane.
pub const FULLSCREEN_Z: f64 = 460.0;

/// Default viewport fill fraction used by `wallLayout.fitRect` (`0.94`).
pub const DEFAULT_FIT_MARGIN: f64 = 0.94;

const Z_BASE: f64 = 2.0;
const Z_MAX_STACK: f64 = 420.0;
const Z_AIR: f64 = 1.5;
const Z_MIN_AIR: f64 = 0.25;
const MAX_FLOW_CANDIDATES: usize = 10_000;
const MAX_SHOVE_STEPS: usize = 10_000;

/// Cardinal direction along which [`shove_aside`] pushes items clear of a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShoveDir {
    Right,
    Left,
    Top,
    Bottom,
}

impl ShoveDir {
    /// Lowercase wire/config name matching the JS union `'right'|'left'|'top'|'bottom'`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ShoveDir::Right => "right",
            ShoveDir::Left => "left",
            ShoveDir::Top => "top",
            ShoveDir::Bottom => "bottom",
        }
    }
}

impl FromStr for ShoveDir {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "right" => Ok(ShoveDir::Right),
            "left" => Ok(ShoveDir::Left),
            "top" => Ok(ShoveDir::Top),
            "bottom" => Ok(ShoveDir::Bottom),
            other => Err(format!(
                "a shove goes right, left, top or bottom, not '{other}'"
            )),
        }
    }
}

/// A keyed canvas item considered by [`shove_aside`].
#[derive(Clone, Debug, PartialEq)]
pub struct ShoveItem {
    pub key: String,
    pub rect: Rect,
}

impl ShoveItem {
    pub fn new(key: impl Into<String>, rect: Rect) -> ShoveItem {
        ShoveItem {
            key: key.into(),
            rect,
        }
    }
}

/// World-space translation applied to a shoved item by [`shove_aside`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShoveDelta {
    pub dx: f64,
    pub dy: f64,
}

impl ShoveDelta {
    pub const fn new(dx: f64, dy: f64) -> ShoveDelta {
        ShoveDelta { dx, dy }
    }
}

/// Camera placement returned by [`fit_rect`]: world centre `(x, y)` and `zoom`
/// in CSS pixels per world unit (re-exported from [`crate::camera::CameraPose`]).
pub use crate::camera::CameraPose;

/// Which border edges of a rectangle a pointer is grabbing. Corners grab two
/// adjacent edges; opposing edges on a tiny rectangle resolve in favour of
/// `left` and `top`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// A terminal window candidate in front-to-back stacking order for [`hit_test`].
#[derive(Clone, Debug, PartialEq)]
pub struct HitTarget {
    pub id: String,
    /// Full window bounds (`body` plus title bar above it).
    pub window: Rect,
    /// Terminal cell grid bounds alone.
    pub body: Rect,
}

impl HitTarget {
    pub fn new(id: impl Into<String>, window: Rect, body: Rect) -> HitTarget {
        HitTarget {
            id: id.into(),
            window,
            body,
        }
    }
}

/// Which part of a terminal window claimed a point in [`hit_test`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HitPart {
    Edge(Edges),
    Title,
    Body,
}

/// Winning terminal hit returned by [`hit_test`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hit {
    pub id: String,
    pub part: HitPart,
}

impl Hit {
    /// Grabbed border edges when `part` is [`HitPart::Edge`], else `None`.
    pub const fn edges(&self) -> Option<Edges> {
        match self.part {
            HitPart::Edge(edges) => Some(edges),
            HitPart::Title | HitPart::Body => None,
        }
    }
}

/// Resize cursor axis for a set of grabbed [`Edges`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResizeCursor {
    NwseResize,
    NeswResize,
    EwResize,
    NsResize,
}

impl ResizeCursor {
    /// CSS cursor keyword returned by `wallLayout.edgeCursor`.
    pub const fn as_css(self) -> &'static str {
        match self {
            ResizeCursor::NwseResize => "nwse-resize",
            ResizeCursor::NeswResize => "nesw-resize",
            ResizeCursor::EwResize => "ew-resize",
            ResizeCursor::NsResize => "ns-resize",
        }
    }
}

/// Callback returning a terminal's Z scale (`TerminalGrid.scale.z`).
pub type ScaleFn<'a> = &'a dyn Fn(&str) -> f64;

/// Callback returning the group id owning a terminal, if any.
pub type GroupFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Optional inputs to [`z_stack_layout`].
#[derive(Default)]
pub struct ZStackOptions<'a> {
    /// Solo fullscreen terminal id (ignored when `fullscreen_ids` is `Some`).
    pub fullscreen_id: Option<&'a str>,
    /// Elevated fullscreen terminal ids (takes precedence over `fullscreen_id`
    /// when `Some`, matching JS `fullscreenIds ?? ...`).
    pub fullscreen_ids: Option<Vec<String>>,
    /// Per-terminal `scale.z` lookup (defaults to `2.0` when `None`).
    pub scale_of: Option<ScaleFn<'a>>,
    /// Group id lookup for each terminal id (`None` when ungrouped).
    pub group_of: Option<GroupFn<'a>>,
}

/// Unified Z-stack assignment for terminals and group frames from [`z_stack_layout`].
#[derive(Clone, Debug, PartialEq)]
pub struct ZStack {
    /// Normalized back-to-front terminal order with group members contiguous.
    pub order: Vec<String>,
    /// World Z coordinate for each terminal id.
    pub terminals: HashMap<String, f64>,
    /// World Z coordinate for each group's backdrop frame.
    pub groups: HashMap<String, f64>,
}

/// An identified rectangle sorted by [`reading_order`].
#[derive(Clone, Debug, PartialEq)]
pub struct ReadingItem {
    pub id: String,
    pub rect: Rect,
}

impl ReadingItem {
    pub fn new(id: impl Into<String>, rect: Rect) -> ReadingItem {
        ReadingItem {
            id: id.into(),
            rect,
        }
    }
}

/// Terminal dimensions in character cells (`cols` × `rows`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GridSize {
    pub cols: u32,
    pub rows: u32,
}

impl GridSize {
    pub const fn new(cols: u32, rows: u32) -> GridSize {
        GridSize { cols, rows }
    }
}

/// Minimum allowed cell dimensions during a border resize drag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MinGridSize {
    pub min_cols: u32,
    pub min_rows: u32,
}

impl MinGridSize {
    pub const fn new(min_cols: u32, min_rows: u32) -> MinGridSize {
        MinGridSize { min_cols, min_rows }
    }
}

/// Resulting cell dimensions and origin shift from [`resize_from_drag`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeResult {
    pub cols: u32,
    pub rows: u32,
    pub shift_x: f64,
    pub shift_y: f64,
}

/// Width and height in world units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

impl Size {
    pub const fn new(w: f64, h: f64) -> Size {
        Size { w, h }
    }
}

/// Parameters controlling [`tile_layout`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileOptions {
    /// Top-left corner of the first window (including its title-bar room).
    pub origin: Point,
    /// Maximum number of columns per row (must be `>= 1`).
    pub columns: usize,
    /// World-unit spacing between adjacent columns and rows (must be `>= 0`).
    pub gap: f64,
    /// Title-bar room reserved above each row's tallest body (defaults to [`BAR_ROOM`]).
    pub bar_room: f64,
}

impl TileOptions {
    /// Construct tiling options using [`BAR_ROOM`] as the row bar height.
    pub const fn new(origin: Point, columns: usize, gap: f64) -> TileOptions {
        TileOptions {
            origin,
            columns,
            gap,
            bar_room: BAR_ROOM,
        }
    }

    /// Validate raw floating-point `columns` and `gap` values with the exact
    /// runtime checks of `wallLayout.tileLayout` (rejecting non-integers, `< 1`,
    /// or negative/NaN gaps).
    pub fn try_from_numbers(
        origin: Point,
        columns: f64,
        gap: f64,
        bar_room: f64,
    ) -> Result<TileOptions, String> {
        if !columns.is_finite() || columns.fract() != 0.0 || columns < 1.0 {
            return Err(format!(
                "[wall] tileLayout: columns must be a positive integer, got {columns}"
            ));
        }
        if gap < 0.0 || gap.is_nan() {
            return Err(format!(
                "[wall] tileLayout: gap must be ≥ 0, got {gap}"
            ));
        }
        Ok(TileOptions {
            origin,
            columns: columns as usize,
            gap,
            bar_room,
        })
    }
}

/// Output of [`tile_layout`]: each member body's top-left corner and the full
/// bounding extent of all tiled windows (including their title-bar room).
#[derive(Clone, Debug, PartialEq)]
pub struct TileLayout {
    pub places: Vec<Point>,
    pub extent: Option<Rect>,
}

/// Whether two rectangles share positive area (touching edges do not count).
pub fn overlaps(a: Rect, b: Rect) -> bool {
    a.overlaps(&b)
}

/// Normalize `-0.0` (and `NaN`, matching JS `expr || 0`) to `+0.0`.
fn normalize_zero_or_nan(value: f64) -> f64 {
    if value == 0.0 || value.is_nan() {
        0.0
    } else {
        value
    }
}

/// First free flow slot for a new terminal of size `(width, height)`.
///
/// Slots form a [`FLOW_COLUMNS`]-wide grid of `(width + FLOW_GAP)` by
/// `(height + FLOW_GAP)` cells starting at `origin` (default `(0, 0)`), scanned
/// row-major downward (`y` decreases per row). A slot is free when it overlaps
/// no rectangle in `taken`, so manually moved terminals stay put and new ones
/// flow around them. Returns the slot's top-left corner.
pub fn free_flow_slot(
    width: f64,
    height: f64,
    taken: &[Rect],
    origin: Option<Point>,
) -> Result<Point, String> {
    let origin = origin.unwrap_or(Point::new(0.0, 0.0));
    for candidate_index in 0..MAX_FLOW_CANDIDATES {
        let col = (candidate_index % FLOW_COLUMNS) as f64;
        let row = (candidate_index / FLOW_COLUMNS) as f64;
        // Match JS `(ox + ...) || 0` so zero coordinates stay +0.0, never -0.0.
        let x = normalize_zero_or_nan(origin.x + col * (width + FLOW_GAP));
        let y = normalize_zero_or_nan(origin.y - row * (height + FLOW_GAP));
        let slot = Rect {
            min_x: x,
            min_y: y - height,
            max_x: x + width,
            max_y: y,
        };
        if !taken.iter().any(|existing| slot.overlaps(existing)) {
            return Ok(Point::new(x, y));
        }
    }
    Err("[wall] no free flow slot in 10k candidates — the taken list is malformed".to_string())
}

/// Make room for `claim` on the canvas by shoving every item it covers along
/// `dir` just far enough to sit `gap` beyond it.
///
/// Items shoved onto others cascade forward along `dir` so nothing ends up
/// underneath any item that moved. Because items only ever advance along `dir`,
/// the cascade settles for well-formed finite rectangles. When `step > 0.0`,
/// each shove distance is rounded up to whole snap steps so grid-aligned items
/// stay on the grid. Items merely touching `claim`'s edge are left alone.
pub fn shove_aside(
    claim: Rect,
    dir: ShoveDir,
    items: &[ShoveItem],
    gap: f64,
    step: f64,
) -> Result<HashMap<String, ShoveDelta>, String> {
    let ahead = |pusher: Rect| -> Rect {
        match dir {
            ShoveDir::Right => Rect {
                max_x: pusher.max_x + gap,
                ..pusher
            },
            ShoveDir::Left => Rect {
                min_x: pusher.min_x - gap,
                ..pusher
            },
            ShoveDir::Top => Rect {
                max_y: pusher.max_y + gap,
                ..pusher
            },
            ShoveDir::Bottom => Rect {
                min_y: pusher.min_y - gap,
                ..pusher
            },
        }
    };
    let needed_distance = |pusher: Rect, target: Rect| -> f64 {
        match dir {
            ShoveDir::Right => pusher.max_x + gap - target.min_x,
            ShoveDir::Left => target.max_x - (pusher.min_x - gap),
            ShoveDir::Top => pusher.max_y + gap - target.min_y,
            ShoveDir::Bottom => target.max_y - (pusher.min_y - gap),
        }
    };
    let sign = match dir {
        ShoveDir::Right | ShoveDir::Top => 1.0,
        ShoveDir::Left | ShoveDir::Bottom => -1.0,
    };
    let horizontal = matches!(dir, ShoveDir::Right | ShoveDir::Left);

    // Preserve JS `new Map(items.map(...))` insertion order and per-push
    // object identity via a monotonically increasing version tag.
    let mut entries: Vec<(String, Rect, u64)> = Vec::with_capacity(items.len());
    let mut key_to_slot: HashMap<String, usize> = HashMap::with_capacity(items.len());
    for item in items {
        if let Some(&slot) = key_to_slot.get(&item.key) {
            entries[slot].1 = item.rect;
        } else {
            let slot = entries.len();
            key_to_slot.insert(item.key.clone(), slot);
            entries.push((item.key.clone(), item.rect, 0));
        }
    }

    struct Pusher {
        slot: Option<(usize, u64)>,
        rect: Rect,
    }

    let mut moved: HashMap<String, ShoveDelta> = HashMap::new();
    let mut pushers: Vec<Pusher> = vec![Pusher {
        slot: None,
        rect: claim,
    }];
    let mut next_version: u64 = 1;
    let mut cursor = 0usize;

    while cursor < pushers.len() {
        if cursor > MAX_SHOVE_STEPS {
            return Err(
                "[wall] shoveAside did not settle in 10k shoves — an item rect is malformed"
                    .to_string(),
            );
        }
        let pusher_slot = pushers[cursor].slot;
        let pusher_rect = pushers[cursor].rect;
        cursor += 1;

        if let Some((slot_idx, version)) = pusher_slot {
            // Shoved again since this entry was queued: its newer rect will push instead.
            if entries[slot_idx].2 != version {
                continue;
            }
        }

        let zone = ahead(pusher_rect);
        for (slot_idx, entry) in entries.iter_mut().enumerate() {
            if pusher_slot.is_some_and(|(by_idx, _)| by_idx == slot_idx) {
                continue;
            }
            let current_rect = entry.1;
            if !current_rect.overlaps(&zone) {
                continue;
            }
            let mut distance = needed_distance(pusher_rect, current_rect);
            if distance <= 0.0 || distance.is_nan() {
                continue;
            }
            if step > 0.0 {
                distance = (distance / step - 1e-9).ceil() * step;
            }
            let dx = if horizontal { sign * distance } else { 0.0 };
            let dy = if horizontal { 0.0 } else { sign * distance };
            let shifted = Rect {
                min_x: current_rect.min_x + dx,
                min_y: current_rect.min_y + dy,
                max_x: current_rect.max_x + dx,
                max_y: current_rect.max_y + dy,
            };
            let version = next_version;
            next_version += 1;
            entry.1 = shifted;
            entry.2 = version;

            let total = moved.entry(entry.0.clone()).or_default();
            total.dx += dx;
            total.dy += dy;
            pushers.push(Pusher {
                slot: Some((slot_idx, version)),
                rect: shifted,
            });
        }
    }

    Ok(moved)
}

/// Camera pose that fits `rect` inside a viewport of `(view_width × view_height)`
/// CSS pixels, filling up to `margin` of the tighter axis and capping zoom at
/// `max_zoom` (pass [`DEFAULT_FIT_MARGIN`] and [`f64::INFINITY`] for JS defaults).
pub fn fit_rect(
    rect: Rect,
    view_width: f64,
    view_height: f64,
    margin: f64,
    max_zoom: f64,
) -> CameraPose {
    let width = (rect.max_x - rect.min_x).max(1e-6);
    let height = (rect.max_y - rect.min_y).max(1e-6);
    CameraPose {
        x: (rect.min_x + rect.max_x) / 2.0,
        y: (rect.min_y + rect.max_y) / 2.0,
        zoom: ((view_width / width).min(view_height / height) * margin).min(max_zoom),
    }
}

/// Smallest axis-aligned rectangle enclosing every rectangle in `rects`, or
/// `None` when `rects` is empty.
pub fn union_rect(rects: &[Rect]) -> Option<Rect> {
    let first = *rects.first()?;
    Some(rects[1..].iter().fold(first, |acc, rect| Rect {
        min_x: acc.min_x.min(rect.min_x),
        min_y: acc.min_y.min(rect.min_y),
        max_x: acc.max_x.max(rect.max_x),
        max_y: acc.max_y.max(rect.max_y),
    }))
}

/// Which border edges of `rect` the world point `(x, y)` is grabbing within
/// `tol` world units on either side of the border, or `None` off the border band.
pub fn edges_at(rect: Rect, x: f64, y: f64, tol: f64) -> Option<Edges> {
    let in_x = x > rect.min_x - tol && x < rect.max_x + tol;
    let in_y = y > rect.min_y - tol && y < rect.max_y + tol;
    if !in_x || !in_y {
        return None;
    }
    let left = (x - rect.min_x).abs() < tol;
    let right = (x - rect.max_x).abs() < tol;
    let top = (y - rect.max_y).abs() < tol;
    let bottom = (y - rect.min_y).abs() < tol;
    if !left && !right && !top && !bottom {
        return None;
    }
    Some(Edges {
        left,
        right: right && !left,
        top,
        bottom: bottom && !top,
    })
}

/// Hit-test world point `(x, y)` against terminals in front-to-back `stack`
/// order (topmost first), with border-grab half-width `tol` in world units.
///
/// Each terminal is tested whole (border band, then title bar, then body)
/// before any terminal beneath it, so a focused terminal on top keeps its body
/// even where a lower terminal's border band reaches underneath it.
pub fn hit_test(stack: &[HitTarget], x: f64, y: f64, tol: f64) -> Option<Hit> {
    for target in stack {
        if let Some(edges) = edges_at(target.window, x, y, tol) {
            return Some(Hit {
                id: target.id.clone(),
                part: HitPart::Edge(edges),
            });
        }
        let win = target.window;
        if x < win.min_x || x > win.max_x || y < win.min_y || y > win.max_y {
            continue;
        }
        let part = if y > target.body.max_y {
            HitPart::Title
        } else {
            HitPart::Body
        };
        return Some(Hit {
            id: target.id.clone(),
            part,
        });
    }
    None
}

/// Resize cursor orientation corresponding to a set of grabbed [`Edges`].
pub fn edge_cursor(edges: Edges) -> ResizeCursor {
    if (edges.left && edges.top) || (edges.right && edges.bottom) {
        ResizeCursor::NwseResize
    } else if (edges.right && edges.top) || (edges.left && edges.bottom) {
        ResizeCursor::NeswResize
    } else if edges.left || edges.right {
        ResizeCursor::EwResize
    } else {
        ResizeCursor::NsResize
    }
}

/// Step through `order` from `current` by `dir` (`+1` / `-1`), wrapping around.
///
/// When `current` is `None` or absent from `order`, starts at the first item
/// when `dir > 0` and at the last item otherwise. Returns `None` when `order`
/// is empty.
pub fn cycle<'a, S: AsRef<str>>(
    order: &'a [S],
    current: Option<&str>,
    dir: isize,
) -> Option<&'a str> {
    if order.is_empty() {
        return None;
    }
    let current_index =
        current.and_then(|cur| order.iter().position(|entry| entry.as_ref() == cur));
    match current_index {
        None => {
            if dir > 0 {
                Some(order[0].as_ref())
            } else {
                Some(order[order.len() - 1].as_ref())
            }
        }
        Some(idx) => {
            let next_idx = (idx as isize + dir).rem_euclid(order.len() as isize) as usize;
            Some(order[next_idx].as_ref())
        }
    }
}

/// Make each group's members contiguous in back-to-front `z_order`, anchored at
/// that group's frontmost member while preserving relative member order within
/// the group, so a group and its `GroupFrame` occupy one coherent Z-slice.
pub fn normalize_z_order<S: AsRef<str>>(
    z_order: &[S],
    group_of: Option<GroupFn<'_>>,
) -> Vec<String> {
    let Some(group_of) = group_of else {
        return z_order.iter().map(|id| id.as_ref().to_string()).collect();
    };
    if z_order.len() <= 1 {
        return z_order.iter().map(|id| id.as_ref().to_string()).collect();
    }

    let mut by_group: HashMap<String, Vec<String>> = HashMap::new();
    for id_ref in z_order {
        let id = id_ref.as_ref();
        if let Some(gid) = group_of(id).filter(|g| !g.is_empty()) {
            by_group.entry(gid).or_default().push(id.to_string());
        }
    }
    if by_group.is_empty() {
        return z_order.iter().map(|id| id.as_ref().to_string()).collect();
    }

    let mut seen_groups: HashSet<String> = HashSet::new();
    let mut reversed: Vec<String> = Vec::with_capacity(z_order.len());
    for id_ref in z_order.iter().rev() {
        let id = id_ref.as_ref();
        match group_of(id).filter(|g| !g.is_empty()) {
            None => reversed.push(id.to_string()),
            Some(gid) => {
                if seen_groups.insert(gid.clone()) {
                    if let Some(members) = by_group.get(&gid) {
                        for member in members.iter().rev() {
                            reversed.push(member.clone());
                        }
                    }
                }
            }
        }
    }
    reversed.reverse();
    reversed
}

/// Unified Z-stack layout for terminals and group frames (`backdrop` at
/// `group_z`, `header` at `group_z + 0.2..0.22`).
///
/// Each group's frame sits immediately behind its backmost member and strictly
/// ahead of any older terminal or group beneath it, while each terminal clears
/// its `-0.5 * scale_z` background and `+0.05 * scale_z` cursor/label sandwich.
/// Fullscreen terminals rise into their own Z-band starting at [`FULLSCREEN_Z`],
/// preserving relative stacking among fullscreen panes.
pub fn z_stack_layout<S: AsRef<str>>(z_order: &[S], options: ZStackOptions<'_>) -> ZStack {
    let order = normalize_z_order(z_order, options.group_of);
    let fullscreen_set: HashSet<String> = match options.fullscreen_ids {
        Some(ids) => ids.into_iter().collect(),
        None => match options.fullscreen_id.filter(|id| !id.is_empty()) {
            Some(id) => HashSet::from([id.to_string()]),
            None => HashSet::new(),
        },
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SliceKind {
        Group,
        Terminal,
    }
    struct Slice {
        kind: SliceKind,
        id: String,
        back: f64,
        front: f64,
    }
    struct FullscreenSlice {
        id: String,
        back: f64,
        front: f64,
    }

    let mut terminals: HashMap<String, f64> = HashMap::new();
    let mut groups: HashMap<String, f64> = HashMap::new();
    let mut slices: Vec<Slice> = Vec::new();
    let mut fullscreen_slices: Vec<FullscreenSlice> = Vec::new();
    let mut seen_groups: HashSet<String> = HashSet::new();

    for id in &order {
        if let Some(group_of) = options.group_of {
            if let Some(gid) = group_of(id).filter(|g| !g.is_empty()) {
                if seen_groups.insert(gid.clone()) {
                    slices.push(Slice {
                        kind: SliceKind::Group,
                        id: gid,
                        back: 0.0,
                        front: 0.25,
                    });
                }
            }
        }
        let raw_scale = options.scale_of.map_or(2.0, |scale_of| scale_of(id)).abs();
        let non_zero_scale = if raw_scale == 0.0 || raw_scale.is_nan() {
            2.0
        } else {
            raw_scale
        };
        let scale_z = non_zero_scale.max(1.0);
        if fullscreen_set.contains(id) {
            fullscreen_slices.push(FullscreenSlice {
                id: id.clone(),
                back: 0.5 * scale_z,
                front: 0.05 * scale_z,
            });
        } else {
            slices.push(Slice {
                kind: SliceKind::Terminal,
                id: id.clone(),
                back: 0.5 * scale_z,
                front: 0.05 * scale_z,
            });
        }
    }

    if fullscreen_slices.len() == 1 {
        terminals.insert(fullscreen_slices[0].id.clone(), FULLSCREEN_Z);
    } else if fullscreen_slices.len() > 1 {
        let mut cursor_z = FULLSCREEN_Z;
        for (idx, slice) in fullscreen_slices.iter().enumerate() {
            let z = if idx == 0 {
                FULLSCREEN_Z
            } else {
                cursor_z + slice.back
            };
            terminals.insert(slice.id.clone(), z);
            cursor_z = z + slice.front + 0.5;
        }
    }

    if slices.is_empty() {
        return ZStack {
            order,
            terminals,
            groups,
        };
    }

    let body_sum = slices
        .iter()
        .fold(0.0, |acc, slice| acc + slice.back + slice.front);
    let gaps = (slices.len().saturating_sub(1)).max(1) as f64;
    let air = if body_sum + gaps * Z_AIR <= Z_MAX_STACK {
        Z_AIR
    } else {
        Z_MIN_AIR.max((Z_MAX_STACK - body_sum) / gaps)
    };

    let mut cursor = Z_BASE;
    for slice in &slices {
        let z = cursor + slice.back;
        match slice.kind {
            SliceKind::Group => {
                groups.insert(slice.id.clone(), z);
            }
            SliceKind::Terminal => {
                terminals.insert(slice.id.clone(), z);
            }
        }
        cursor = z + slice.front + air;
    }

    ZStack {
        order,
        terminals,
        groups,
    }
}

/// World Z for each terminal in back-to-front `z_order`, accounting for each
/// terminal's `scale.z` (default `2.0`) and optional `group_of` frame slices.
pub fn z_positions<S: AsRef<str>>(
    z_order: &[S],
    fullscreen_id: Option<&str>,
    scale_of: Option<ScaleFn<'_>>,
    group_of: Option<GroupFn<'_>>,
) -> HashMap<String, f64> {
    z_stack_layout(
        z_order,
        ZStackOptions {
            fullscreen_id,
            fullscreen_ids: None,
            scale_of,
            group_of,
        },
    )
    .terminals
}

fn cmp_js_number(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Sort items in reading order: horizontal row bands from top to bottom (an
/// item joins the current band while it vertically overlaps at least half of
/// the shorter of the item and the band), and left to right (`min_x` ascending)
/// within each band.
pub fn reading_order(items: &[ReadingItem]) -> Vec<String> {
    if items.len() <= 1 {
        return items.iter().map(|item| item.id.clone()).collect();
    }

    let mut by_top: Vec<&ReadingItem> = items.iter().collect();
    by_top.sort_by(|a, b| {
        cmp_js_number(b.rect.max_y, a.rect.max_y)
            .then_with(|| cmp_js_number(a.rect.min_x, b.rect.min_x))
    });

    struct Band<'a> {
        min_y: f64,
        max_y: f64,
        list: Vec<&'a ReadingItem>,
    }

    let mut bands: Vec<Band<'_>> = Vec::new();
    for item in by_top {
        let rect = item.rect;
        let item_height = (rect.max_y - rect.min_y).max(0.0);
        if let Some(current) = bands.last_mut() {
            let band_height = (current.max_y - current.min_y).max(0.0);
            let overlap = current.max_y.min(rect.max_y) - current.min_y.max(rect.min_y);
            if overlap >= 0.5 * item_height.min(band_height) {
                current.min_y = current.min_y.min(rect.min_y);
                current.max_y = current.max_y.max(rect.max_y);
                current.list.push(item);
                continue;
            }
        }
        bands.push(Band {
            min_y: rect.min_y,
            max_y: rect.max_y,
            list: vec![item],
        });
    }

    let mut ordered = Vec::with_capacity(items.len());
    for mut band in bands {
        band.list.sort_by(|x, y| {
            cmp_js_number(x.rect.min_x, y.rect.min_x)
                .then_with(|| cmp_js_number(y.rect.max_y, x.rect.max_y))
        });
        for item in band.list {
            ordered.push(item.id.clone());
        }
    }
    ordered
}

/// Round matching JavaScript `Math.round` (ties round toward `+Infinity`).
fn js_round(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    if value > 0.0 {
        value.round()
    } else {
        let floored = value.floor();
        if value - floored >= 0.5 {
            floored + 1.0
        } else {
            floored
        }
    }
}

/// Compute new `(cols, rows)` and origin shift `(shift_x, shift_y)` for a
/// border drag.
///
/// Dragging the right or bottom edge grows the terminal away from its top-left
/// origin; dragging the left or top edge grows it toward the pointer and shifts
/// the origin by the added cells so the opposite edge stays fixed.
pub fn resize_from_drag(
    edges: Edges,
    start: GridSize,
    stride: Point,
    dx: f64,
    dy: f64,
    min: MinGridSize,
) -> ResizeResult {
    let mut raw_cols = start.cols as f64;
    let mut raw_rows = start.rows as f64;
    if edges.right {
        raw_cols = start.cols as f64 + dx / stride.x;
    }
    if edges.left {
        raw_cols = start.cols as f64 - dx / stride.x;
    }
    if edges.bottom {
        raw_rows = start.rows as f64 - dy / stride.y;
    }
    if edges.top {
        raw_rows = start.rows as f64 + dy / stride.y;
    }

    let cols = js_round(raw_cols).max(min.min_cols as f64) as u32;
    let rows = js_round(raw_rows).max(min.min_rows as f64) as u32;
    let delta_cols = cols as f64 - start.cols as f64;
    let delta_rows = rows as f64 - start.rows as f64;

    ResizeResult {
        cols,
        rows,
        shift_x: if edges.left {
            normalize_zero_or_nan(-delta_cols * stride.x)
        } else {
            0.0
        },
        shift_y: if edges.top {
            normalize_zero_or_nan(delta_rows * stride.y)
        } else {
            0.0
        },
    }
}

/// Compute a tiled group's body top-left positions and bounding window extent.
///
/// Packs `sizes` row-major across `options.columns` columns separated by
/// `options.gap`. Each column is as wide as its widest member and each row as
/// tall as its tallest window (`body + bar_room`), so mixed-size members align
/// in a table. `options.origin` is the top-left of the first window (top of its
/// title bar).
pub fn tile_layout(sizes: &[Size], options: TileOptions) -> Result<TileLayout, String> {
    if options.columns < 1 {
        return Err(format!(
            "[wall] tileLayout: columns must be a positive integer, got {}",
            options.columns
        ));
    }
    if options.gap < 0.0 || options.gap.is_nan() {
        return Err(format!(
            "[wall] tileLayout: gap must be ≥ 0, got {}",
            options.gap
        ));
    }
    if sizes.is_empty() {
        return Ok(TileLayout {
            places: Vec::new(),
            extent: None,
        });
    }

    let cols = options.columns.min(sizes.len());
    let rows = sizes.len().div_ceil(cols);
    let mut col_widths = vec![0.0_f64; cols];
    let mut row_heights = vec![0.0_f64; rows];
    for (index, size) in sizes.iter().enumerate() {
        let col = index % cols;
        let row = index / cols;
        col_widths[col] = col_widths[col].max(size.w);
        row_heights[row] = row_heights[row].max(size.h + options.bar_room);
    }

    let mut col_xs = Vec::with_capacity(cols);
    col_xs.push(options.origin.x);
    for col in 1..cols {
        col_xs.push(col_xs[col - 1] + col_widths[col - 1] + options.gap);
    }

    let mut row_ys = Vec::with_capacity(rows);
    row_ys.push(options.origin.y);
    for row in 1..rows {
        row_ys.push(row_ys[row - 1] - row_heights[row - 1] - options.gap);
    }

    let places = (0..sizes.len())
        .map(|index| Point::new(col_xs[index % cols], row_ys[index / cols] - options.bar_room))
        .collect();
    let extent = Some(Rect {
        min_x: options.origin.x,
        min_y: row_ys[rows - 1] - row_heights[rows - 1],
        max_x: col_xs[cols - 1] + col_widths[cols - 1],
        max_y: options.origin.y,
    });

    Ok(TileLayout { places, extent })
}

/// Index of the tile whose centre in `centres` is closest to `point`, or `None`
/// when `centres` is empty.
pub fn nearest_place(centres: &[Point], point: Point) -> Option<usize> {
    let mut best_index: Option<usize> = None;
    let mut best_distance_sq = f64::INFINITY;
    for (index, centre) in centres.iter().enumerate() {
        let dx = centre.x - point.x;
        let dy = centre.y - point.y;
        let distance_sq = dx * dx + dy * dy;
        if distance_sq < best_distance_sq {
            best_distance_sq = distance_sq;
            best_index = Some(index);
        }
    }
    best_index
}

/// Even-odd ray-casting test for whether `(x, y)` lies inside `poly`.
pub fn point_in_polygon(poly: &[Point], x: f64, y: f64) -> bool {
    if poly.is_empty() {
        return false;
    }
    let mut inside = false;
    let mut prev = poly.len() - 1;
    for curr in 0..poly.len() {
        let a = poly[curr];
        let b = poly[prev];
        if (a.y > y) != (b.y > y) && x < ((b.x - a.x) * (y - a.y)) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        prev = curr;
    }
    inside
}

/// JavaScript `Math.sign`: returns `0.0` for `±0.0` (unlike `f64::signum`,
/// which returns `±1.0` and would misclassify collinear points).
fn js_sign(value: f64) -> f64 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else if value == 0.0 {
        0.0
    } else {
        f64::NAN
    }
}

/// Whether closed segments `ab` and `cd` cross or touch.
fn segments_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
    let orientation =
        |p: Point, q: Point, r: Point| js_sign((q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x));
    let on_box = |p: Point, q: Point, r: Point| {
        p.x.min(q.x) <= r.x
            && r.x <= p.x.max(q.x)
            && p.y.min(q.y) <= r.y
            && r.y <= p.y.max(q.y)
    };
    let o1 = orientation(a, b, c);
    let o2 = orientation(a, b, d);
    let o3 = orientation(c, d, a);
    let o4 = orientation(c, d, b);
    if o1 != o2 && o3 != o4 {
        return true;
    }
    (o1 == 0.0 && on_box(a, b, c))
        || (o2 == 0.0 && on_box(a, b, d))
        || (o3 == 0.0 && on_box(c, d, a))
        || (o4 == 0.0 && on_box(c, d, b))
}

/// Whether a freeform lasso (closed polygon through `poly`) touches `rect`.
///
/// A lasso touches `rect` when a lasso vertex lies in `rect`, `rect` lies
/// inside `poly`, or `poly`'s closed outline crosses any edge of `rect`. A
/// single-point lasso touches what contains it; an empty lasso touches nothing.
pub fn lasso_touches(poly: &[Point], rect: Rect) -> bool {
    if poly.is_empty() {
        return false;
    }
    if poly.iter().any(|&point| rect.contains(point)) {
        return true;
    }
    if poly.len() < 2 {
        return false;
    }
    let corners = [
        Point::new(rect.min_x, rect.min_y),
        Point::new(rect.max_x, rect.min_y),
        Point::new(rect.max_x, rect.max_y),
        Point::new(rect.min_x, rect.max_y),
    ];
    if poly.len() >= 3 && corners.iter().any(|corner| point_in_polygon(poly, corner.x, corner.y)) {
        return true;
    }
    for index in 0..poly.len() {
        let a = poly[index];
        let b = poly[(index + 1) % poly.len()];
        for corner_idx in 0..4 {
            if segments_cross(a, b, corners[corner_idx], corners[(corner_idx + 1) % 4]) {
                return true;
            }
        }
    }
    false
}

/// Grow `rect` outward by `pad` world units on all four sides.
pub fn pad_rect(rect: Rect, pad: f64) -> Rect {
    Rect {
        min_x: rect.min_x - pad,
        min_y: rect.min_y - pad,
        max_x: rect.max_x + pad,
        max_y: rect.max_y + pad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::from_top_left(x, y, w, h)
    }

    // ── Ported from tools/wall-layout.test.mjs ───────────────────────────────

    #[test]
    fn flow_fills_a_row_left_to_right_then_wraps() {
        let mut taken = Vec::new();
        let mut slots = Vec::new();
        for _ in 0..=FLOW_COLUMNS {
            let slot = free_flow_slot(100.0, 50.0, &taken, None).unwrap();
            slots.push(slot);
            taken.push(rect(slot.x, slot.y, 100.0, 50.0));
        }
        assert_eq!(slots[0], Point::new(0.0, 0.0));
        assert_eq!(slots[1], Point::new(100.0 + FLOW_GAP, 0.0));
        assert_eq!(slots[FLOW_COLUMNS], Point::new(0.0, -(50.0 + FLOW_GAP)));
    }

    #[test]
    fn moved_terminal_is_flowed_around_never_overlapped() {
        let moved = rect(10.0, -5.0, 100.0, 50.0); // sits across slot 0 and slot 1
        let slot = free_flow_slot(100.0, 50.0, &[moved], None).unwrap();
        assert!(!overlaps(rect(slot.x, slot.y, 100.0, 50.0), moved));
    }

    #[test]
    fn free_flow_slot_scans_from_custom_origin_and_keeps_zero_coordinates_positive() {
        let origin = Point::new(800.0, -600.0);
        let mut taken = vec![rect(800.0, -600.0, 100.0, 50.0)];
        let s1 = free_flow_slot(100.0, 50.0, &taken, Some(origin)).unwrap();
        assert_eq!(s1, Point::new(800.0 + 100.0 + FLOW_GAP, -600.0));
        taken.push(rect(s1.x, s1.y, 100.0, 50.0));
        let s2 = free_flow_slot(100.0, 50.0, &taken, Some(origin)).unwrap();
        taken.push(rect(s2.x, s2.y, 100.0, 50.0));
        let s3 = free_flow_slot(100.0, 50.0, &taken, Some(origin)).unwrap();
        assert_eq!(s3, Point::new(800.0, -600.0 - (50.0 + FLOW_GAP)));

        // Row 1 landing exactly at y = 0 produces +0.0, never -0.0 (`Object.is(z, 0)`).
        let zero_slot = free_flow_slot(
            100.0,
            50.0,
            &[rect(0.0, 50.0 + FLOW_GAP, 400.0, 50.0)],
            Some(Point::new(-0.0, 50.0 + FLOW_GAP)),
        )
        .unwrap();
        assert_eq!(zero_slot.x.to_bits(), 0.0_f64.to_bits());
        assert_eq!(zero_slot.y.to_bits(), 0.0_f64.to_bits());

        // Row 0 from (-0.0, -0.0) also normalizes -0.0 - 0.0 to +0.0.
        let origin_neg_zero =
            free_flow_slot(100.0, 50.0, &[], Some(Point::new(-0.0, -0.0))).unwrap();
        assert_eq!(origin_neg_zero.x.to_bits(), 0.0_f64.to_bits());
        assert_eq!(origin_neg_zero.y.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn fit_rect_centers_and_fits_tighter_axis() {
        let fitted = fit_rect(
            rect(0.0, 0.0, 200.0, 100.0),
            1000.0,
            1000.0,
            1.0,
            f64::INFINITY,
        );
        assert_eq!(
            fitted,
            CameraPose {
                x: 100.0,
                y: -50.0,
                zoom: 5.0
            }
        );
    }

    #[test]
    fn fit_rect_with_ceiling_caps_small_rect_and_fits_large_one() {
        assert_eq!(
            fit_rect(rect(0.0, 0.0, 200.0, 100.0), 1000.0, 1000.0, 1.0, 3.0),
            CameraPose {
                x: 100.0,
                y: -50.0,
                zoom: 3.0
            }
        );
        assert_eq!(
            fit_rect(rect(0.0, 0.0, 2000.0, 1000.0), 1000.0, 1000.0, 1.0, 3.0).zoom,
            0.5
        );
    }

    #[test]
    fn union_rect_spans_all_rects_and_empty_is_none() {
        assert_eq!(union_rect(&[]), None);
        assert_eq!(
            union_rect(&[rect(0.0, 0.0, 10.0, 10.0), rect(20.0, 5.0, 10.0, 10.0)]),
            Some(Rect {
                min_x: 0.0,
                min_y: -10.0,
                max_x: 30.0,
                max_y: 5.0
            })
        );
    }

    #[test]
    fn edges_at_grabs_border_band_from_both_sides_and_corners_grab_two_edges() {
        let r = rect(0.0, 0.0, 100.0, 50.0);
        assert_eq!(
            edges_at(r, 101.0, -25.0, 2.0),
            Some(Edges {
                left: false,
                right: true,
                top: false,
                bottom: false
            })
        );
        assert_eq!(edges_at(r, 99.0, -25.0, 2.0).map(|e| e.right), Some(true));
        assert_eq!(edges_at(r, 50.0, -25.0, 2.0), None); // body, not border
        assert_eq!(edges_at(r, 200.0, -25.0, 2.0), None); // far outside
        let corner = edges_at(r, 100.0, -50.0, 2.0).unwrap();
        assert!(corner.right && corner.bottom);
    }

    #[test]
    fn hit_test_top_terminal_keeps_body_over_lower_border_band() {
        let a = HitTarget::new("a", rect(0.0, 4.0, 100.0, 54.0), rect(0.0, 0.0, 100.0, 50.0));
        let b = HitTarget::new(
            "b",
            rect(80.0, -6.0, 100.0, 34.0),
            rect(80.0, -10.0, 100.0, 30.0),
        );

        assert_eq!(
            hit_test(&[b.clone(), a.clone()], 100.0, -20.0, 2.0),
            Some(Hit {
                id: "b".to_string(),
                part: HitPart::Body
            })
        );

        let a_on_top = hit_test(&[a.clone(), b.clone()], 100.0, -20.0, 2.0).unwrap();
        assert_eq!(a_on_top.id, "a");
        assert!(matches!(a_on_top.part, HitPart::Edge(_)));

        let off_b = hit_test(&[b.clone(), a.clone()], 100.0, -45.0, 2.0).unwrap();
        assert_eq!(off_b.id, "a");
        assert!(off_b.edges().unwrap().right);

        assert_eq!(
            hit_test(&[b.clone(), a.clone()], 40.0, 2.0, 1.0),
            Some(Hit {
                id: "a".to_string(),
                part: HitPart::Title
            })
        );

        let top_edge = hit_test(&[b.clone(), a.clone()], 40.0, 4.0, 1.0).unwrap();
        assert_eq!(top_edge.id, "a");
        assert!(top_edge.edges().unwrap().top);

        assert_eq!(hit_test(&[b, a], 500.0, 500.0, 2.0), None);
    }

    #[test]
    fn edge_cursor_maps_corners_and_axes_to_resize_cursors() {
        assert_eq!(
            edge_cursor(Edges {
                left: true,
                top: true,
                ..Edges::default()
            })
            .as_css(),
            "nwse-resize"
        );
        assert_eq!(
            edge_cursor(Edges {
                right: true,
                bottom: true,
                ..Edges::default()
            })
            .as_css(),
            "nwse-resize"
        );
        assert_eq!(
            edge_cursor(Edges {
                right: true,
                top: true,
                ..Edges::default()
            })
            .as_css(),
            "nesw-resize"
        );
        assert_eq!(
            edge_cursor(Edges {
                left: true,
                bottom: true,
                ..Edges::default()
            })
            .as_css(),
            "nesw-resize"
        );
        assert_eq!(
            edge_cursor(Edges {
                left: true,
                ..Edges::default()
            })
            .as_css(),
            "ew-resize"
        );
        assert_eq!(
            edge_cursor(Edges {
                bottom: true,
                ..Edges::default()
            })
            .as_css(),
            "ns-resize"
        );
    }

    #[test]
    fn cycle_wraps_both_ways_and_starts_sensibly_with_no_focus() {
        let order = ["a", "b", "c"];
        assert_eq!(cycle(&order, Some("c"), 1), Some("a"));
        assert_eq!(cycle(&order, Some("a"), -1), Some("c"));
        assert_eq!(cycle(&order, None, 1), Some("a"));
        assert_eq!(cycle(&order, None, -1), Some("c"));
        let empty: [&str; 0] = [];
        assert_eq!(cycle(&empty, None, 1), None);
        for id in order {
            assert_eq!(cycle(&order, cycle(&order, Some(id), 1), -1), Some(id));
        }
    }

    #[test]
    fn resize_from_drag_grows_right_bottom_and_shifts_left_top() {
        let stride = Point::new(1.0, 2.0);
        let start = GridSize::new(80, 24);
        let min = MinGridSize::new(20, 5);

        assert_eq!(
            resize_from_drag(
                Edges {
                    right: true,
                    ..Edges::default()
                },
                start,
                stride,
                10.4,
                0.0,
                min
            ),
            ResizeResult {
                cols: 90,
                rows: 24,
                shift_x: 0.0,
                shift_y: 0.0
            }
        );
        assert_eq!(
            resize_from_drag(
                Edges {
                    bottom: true,
                    ..Edges::default()
                },
                start,
                stride,
                0.0,
                -8.0,
                min
            ),
            ResizeResult {
                cols: 80,
                rows: 28,
                shift_x: 0.0,
                shift_y: 0.0
            }
        );
        assert_eq!(
            resize_from_drag(
                Edges {
                    left: true,
                    ..Edges::default()
                },
                start,
                stride,
                -5.0,
                0.0,
                min
            ),
            ResizeResult {
                cols: 85,
                rows: 24,
                shift_x: -5.0,
                shift_y: 0.0
            }
        );
        assert_eq!(
            resize_from_drag(
                Edges {
                    top: true,
                    ..Edges::default()
                },
                start,
                stride,
                0.0,
                4.0,
                min
            ),
            ResizeResult {
                cols: 80,
                rows: 26,
                shift_x: 0.0,
                shift_y: 4.0
            }
        );
        assert_eq!(
            resize_from_drag(
                Edges {
                    right: true,
                    ..Edges::default()
                },
                start,
                stride,
                -500.0,
                0.0,
                min
            )
            .cols,
            20
        );
    }

    #[test]
    fn tiling_packs_members_row_major_with_bar_room_above_each_row() {
        let sizes = [
            Size::new(100.0, 50.0),
            Size::new(100.0, 50.0),
            Size::new(100.0, 50.0),
        ];
        let TileLayout { places, extent } = tile_layout(
            &sizes,
            TileOptions {
                origin: Point::new(10.0, 20.0),
                columns: 2,
                gap: 2.0,
                bar_room: 8.0,
            },
        )
        .unwrap();

        assert_eq!(
            places,
            vec![
                Point::new(10.0, 12.0),
                Point::new(112.0, 12.0),
                Point::new(10.0, 20.0 - 58.0 - 2.0 - 8.0),
            ]
        );
        assert_eq!(
            extent,
            Some(Rect {
                min_x: 10.0,
                min_y: 20.0 - 2.0 * 58.0 - 2.0,
                max_x: 212.0,
                max_y: 20.0,
            })
        );

        let wins: Vec<Rect> = places
            .iter()
            .enumerate()
            .map(|(idx, place)| Rect {
                min_x: place.x,
                min_y: place.y - sizes[idx].h,
                max_x: place.x + sizes[idx].w,
                max_y: place.y + 8.0,
            })
            .collect();
        for i in 0..wins.len() {
            for j in (i + 1)..wins.len() {
                assert!(!overlaps(wins[i], wins[j]));
            }
        }
    }

    #[test]
    fn tiling_of_mixed_sizes_aligns_columns_and_rows_as_table() {
        let sizes = [
            Size::new(80.0, 40.0),
            Size::new(120.0, 60.0),
            Size::new(100.0, 30.0),
            Size::new(60.0, 20.0),
        ];
        let TileLayout { places, .. } = tile_layout(
            &sizes,
            TileOptions {
                origin: Point::new(0.0, 0.0),
                columns: 2,
                gap: 0.0,
                bar_room: 10.0,
            },
        )
        .unwrap();

        assert_eq!(places[1].x, 100.0);
        assert_eq!(places[3].x, 100.0);
        assert_eq!(places[2].y, -(60.0 + 10.0) - 10.0);
        assert_eq!(places[0].y, places[1].y);
    }

    #[test]
    fn tiling_defaults_bar_room_clamps_columns_and_refuses_nonsense() {
        let TileLayout { places, .. } =
            tile_layout(&[Size::new(10.0, 10.0)], TileOptions::new(Point::new(0.0, 0.0), 4, 1.0))
                .unwrap();
        assert_eq!(places, vec![Point::new(0.0, -BAR_ROOM)]);

        assert_eq!(
            tile_layout(&[], TileOptions::new(Point::new(0.0, 0.0), 2, 0.0)).unwrap(),
            TileLayout {
                places: Vec::new(),
                extent: None,
            }
        );

        let err_zero_cols =
            tile_layout(&[Size::new(1.0, 1.0)], TileOptions::new(Point::new(0.0, 0.0), 0, 0.0))
                .unwrap_err();
        assert!(err_zero_cols.contains("columns"));

        let err_frac_cols =
            TileOptions::try_from_numbers(Point::new(0.0, 0.0), 1.5, 0.0, BAR_ROOM).unwrap_err();
        assert!(err_frac_cols.contains("columns"));

        let err_neg_gap =
            tile_layout(&[Size::new(1.0, 1.0)], TileOptions::new(Point::new(0.0, 0.0), 1, -1.0))
                .unwrap_err();
        assert!(err_neg_gap.contains("gap"));
    }

    #[test]
    fn dropped_tile_goes_to_nearest_place() {
        let centres = [
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(0.0, -100.0),
        ];
        assert_eq!(nearest_place(&centres, Point::new(90.0, -20.0)), Some(1));
        assert_eq!(nearest_place(&centres, Point::new(-5.0, -80.0)), Some(2));
        assert_eq!(nearest_place(&[], Point::new(0.0, 0.0)), None);
    }

    #[test]
    fn lasso_touches_crossed_enclosed_or_entered_rect_and_spares_notch() {
        let r = rect(0.0, 0.0, 10.0, 10.0); // x 0..10, y -10..0
        let around = [
            Point::new(-5.0, 5.0),
            Point::new(15.0, 5.0),
            Point::new(15.0, -15.0),
            Point::new(-5.0, -15.0),
        ];
        assert!(lasso_touches(&around, r));
        assert!(lasso_touches(
            &[
                Point::new(5.0, 20.0),
                Point::new(5.0, -20.0),
                Point::new(6.0, -20.0),
            ],
            r
        ));
        assert!(lasso_touches(
            &[
                Point::new(2.0, -2.0),
                Point::new(30.0, -2.0),
                Point::new(30.0, -8.0),
            ],
            r
        ));
        assert!(!lasso_touches(
            &[
                Point::new(20.0, 5.0),
                Point::new(40.0, 5.0),
                Point::new(40.0, -15.0),
                Point::new(20.0, -15.0),
            ],
            r
        ));

        let u_notch = [
            Point::new(-10.0, 10.0),
            Point::new(-2.0, 10.0),
            Point::new(-2.0, -12.0),
            Point::new(12.0, -12.0),
            Point::new(12.0, 10.0),
            Point::new(20.0, 10.0),
            Point::new(20.0, -20.0),
            Point::new(-10.0, -20.0),
        ];
        assert!(!lasso_touches(&u_notch, r));
        assert!(!lasso_touches(&[], r));
        assert!(lasso_touches(&[Point::new(1.0, -1.0)], r));
        assert!(point_in_polygon(&around, 0.0, 0.0));
        assert_eq!(
            pad_rect(r, 2.0),
            Rect {
                min_x: -2.0,
                min_y: -12.0,
                max_x: 12.0,
                max_y: 2.0
            }
        );
    }

    #[test]
    fn reading_order_walks_row_bands_top_to_bottom_and_left_to_right() {
        let items = [
            ReadingItem::new("d", rect(120.0, -70.0, 100.0, 50.0)),
            ReadingItem::new("b", rect(120.0, 0.0, 100.0, 50.0)),
            ReadingItem::new("c", rect(0.0, -72.0, 100.0, 50.0)),
            ReadingItem::new("a", rect(0.0, -6.0, 100.0, 50.0)),
        ];
        assert_eq!(reading_order(&items), vec!["a", "b", "c", "d"]);
        assert!(reading_order(&[]).is_empty());
        assert_eq!(
            reading_order(&[ReadingItem::new("solo", rect(0.0, 0.0, 10.0, 10.0))]),
            vec!["solo"]
        );
    }

    #[test]
    fn z_positions_clears_scaled_sandwiches_and_lifts_fullscreen() {
        let scale_of = |id: &str| -> f64 {
            match id {
                "c" => 4.0,
                _ => 2.0,
            }
        };
        let z4 = z_positions(&["a", "b", "c", "d"], Some("b"), Some(&scale_of), None);
        assert_eq!(z4["b"], FULLSCREEN_Z);

        let order = ["a", "c", "d"];
        assert!(z4["a"] - 0.5 * scale_of("a") >= 2.0);
        for pair in order.windows(2) {
            let lo = pair[0];
            let hi = pair[1];
            let lo_front = z4[lo] + 0.05 * scale_of(lo);
            let hi_back = z4[hi] - 0.5 * scale_of(hi);
            assert!(hi_back - lo_front >= 1.49);
        }

        let many: Vec<String> = (0..250).map(|idx| format!("t{idx}")).collect();
        let zm = z_positions(&many, None, None, None);
        for pair in many.windows(2) {
            let lo_front = zm[&pair[0]] + 0.05 * 2.0;
            let hi_back = zm[&pair[1]] - 0.5 * 2.0;
            assert!(hi_back - lo_front >= 0.249);
        }
        assert!(zm[many.last().unwrap()] < FULLSCREEN_Z - 10.0);
    }

    #[test]
    fn normalize_z_order_and_z_stack_layout_keep_groups_contiguous_and_sandwiched() {
        let group_of = |id: &str| -> Option<String> {
            match id {
                "a" | "b" => Some("g1".to_string()),
                "d" | "e" => Some("g2".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            normalize_z_order(&["a", "c", "d", "e", "b"], Some(&group_of)),
            vec!["c", "d", "e", "a", "b"]
        );

        let ZStack {
            order,
            terminals,
            groups,
        } = z_stack_layout(
            &["a", "c", "d", "e", "b"],
            ZStackOptions {
                scale_of: Some(&|_| 2.0),
                group_of: Some(&group_of),
                ..ZStackOptions::default()
            },
        );
        assert_eq!(order, vec!["c", "d", "e", "a", "b"]);

        let c_front = terminals["c"] + 0.1;
        let g2_back = groups["g2"];
        let g2_front = g2_back + 0.25;
        let d_back = terminals["d"] - 1.0;
        let e_front = terminals["e"] + 0.1;
        let g1_back = groups["g1"];
        let g1_front = g1_back + 0.25;
        let a_back = terminals["a"] - 1.0;
        let a_front = terminals["a"] + 0.1;
        let b_back = terminals["b"] - 1.0;

        assert!(g2_back > c_front);
        assert!(d_back > g2_front);
        assert!(g1_back > e_front);
        assert!(a_back > g1_front);
        assert!(b_back > a_front);
    }

    #[test]
    fn z_stack_layout_stacks_multiple_fullscreen_panes_above_fullscreen_z() {
        let ZStack { terminals, .. } = z_stack_layout(
            &["x", "y", "z"],
            ZStackOptions {
                fullscreen_ids: Some(vec!["x".to_string(), "z".to_string()]),
                ..ZStackOptions::default()
            },
        );
        assert_eq!(terminals["x"], FULLSCREEN_Z);
        // Second fullscreen slice clears the first's +0.1 front plus 0.5 air plus its own 1.0 back.
        assert!(terminals["z"] - 1.0 > terminals["x"] + 0.1);
        assert!(terminals["y"] < FULLSCREEN_Z);
    }

    #[test]
    fn shove_aside_cascades_snaps_and_leaves_unaffected_items() {
        let claim = rect(100.0, 0.0, 50.0, 40.0); // x 100..150
        let items = [
            ShoveItem::new("hit", rect(140.0, -5.0, 60.0, 30.0)),
            ShoveItem::new("next", rect(205.0, -10.0, 40.0, 20.0)),
            ShoveItem::new("clear", rect(100.0, -100.0, 40.0, 20.0)),
            ShoveItem::new("edge", rect(152.0, -60.0, 10.0, 10.0)),
            ShoveItem::new("behind", rect(20.0, 0.0, 50.0, 40.0)),
        ];
        let moved = shove_aside(claim, ShoveDir::Right, &items, 2.0, 0.0).unwrap();
        let mut keys: Vec<&str> = moved.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["hit", "next"]);
        assert_eq!(moved["hit"], ShoveDelta::new(12.0, 0.0));
        assert_eq!(moved["next"], ShoveDelta::new(9.0, 0.0));

        let after: Vec<Rect> = items
            .iter()
            .map(|item| {
                let delta = moved.get(&item.key).copied().unwrap_or_default();
                Rect {
                    min_x: item.rect.min_x + delta.dx,
                    min_y: item.rect.min_y + delta.dy,
                    max_x: item.rect.max_x + delta.dx,
                    max_y: item.rect.max_y + delta.dy,
                }
            })
            .collect();
        for shifted in &after {
            assert!(!overlaps(*shifted, claim));
        }
        for i in 0..after.len() {
            for j in (i + 1)..after.len() {
                if moved.contains_key(&items[i].key) || moved.contains_key(&items[j].key) {
                    assert!(!overlaps(after[i], after[j]));
                }
            }
        }

        assert_eq!(
            shove_aside(
                claim,
                ShoveDir::Left,
                &[ShoveItem::new("a", rect(80.0, 0.0, 30.0, 10.0))],
                2.0,
                0.0
            )
            .unwrap()["a"],
            ShoveDelta::new(-12.0, 0.0)
        );
        assert_eq!(
            shove_aside(
                claim,
                ShoveDir::Top,
                &[ShoveItem::new("a", rect(110.0, 10.0, 10.0, 20.0))],
                2.0,
                0.0
            )
            .unwrap()["a"],
            ShoveDelta::new(0.0, 12.0)
        );
        assert_eq!(
            shove_aside(
                claim,
                ShoveDir::Bottom,
                &[ShoveItem::new("a", rect(110.0, -30.0, 10.0, 20.0))],
                2.0,
                0.0
            )
            .unwrap()["a"],
            ShoveDelta::new(0.0, -12.0)
        );
        assert_eq!(
            shove_aside(
                claim,
                ShoveDir::Right,
                &[ShoveItem::new("a", rect(140.0, 0.0, 10.0, 10.0))],
                2.0,
                5.0
            )
            .unwrap()["a"],
            ShoveDelta::new(15.0, 0.0)
        );
        let dir_err = ShoveDir::from_str("up").unwrap_err();
        assert!(dir_err.contains("right, left, top or bottom"));
    }

    // ── Pure wallLayout tiling & geometry assertions from tools/wall-groups.test.mjs ──

    #[test]
    fn groups_gathered_origin_and_two_column_tiling_matches_wall_groups_test() {
        // From tools/wall-groups.test.mjs:
        // fakeWall({ a: [100, 50, 300, -40], b: [100, 50, 500, -90], c: [100, 50, 310, -200], d: [100, 50, 50, -60] })
        let bodies = [
            rect(300.0, -40.0, 100.0, 50.0),
            rect(500.0, -90.0, 100.0, 50.0),
            rect(310.0, -200.0, 100.0, 50.0),
            rect(50.0, -60.0, 100.0, 50.0),
        ];
        let tile_boxes: Vec<Rect> = bodies
            .iter()
            .map(|body| Rect {
                max_y: body.max_y + BAR_ROOM,
                ..*body
            })
            .collect();
        let gathered = union_rect(&tile_boxes).unwrap();
        let origin = Point::new(gathered.min_x, gathered.max_y);
        assert_eq!(origin, Point::new(50.0, -40.0 + BAR_ROOM));

        let sizes = [Size::new(100.0, 50.0); 4];
        let TileLayout { places, .. } =
            tile_layout(&sizes, TileOptions::new(origin, 2, 2.0)).unwrap();
        assert_eq!(places[0], Point::new(50.0, -40.0));
        assert_eq!(places[1], Point::new(152.0, -40.0));
        assert_eq!(places[2], Point::new(50.0, -40.0 - (50.0 + BAR_ROOM) - 2.0));
        assert_eq!(
            places[3],
            Point::new(152.0, -40.0 - (50.0 + BAR_ROOM) - 2.0)
        );
    }

    #[test]
    fn groups_retiling_columns_translation_drop_and_top_left_origin_assertions() {
        let sizes_3 = [Size::new(100.0, 50.0); 3];
        let origin = Point::new(0.0, BAR_ROOM);

        // 3 columns: single row; 1 column: single column; 2 shown: second slot reused.
        let row_places = tile_layout(&sizes_3, TileOptions::new(origin, 3, 2.0))
            .unwrap()
            .places;
        assert_eq!(row_places[1].y, row_places[0].y);
        let col_places = tile_layout(&sizes_3, TileOptions::new(origin, 1, 2.0))
            .unwrap()
            .places;
        assert_eq!(col_places[1].x, col_places[0].x);
        let two_shown = tile_layout(&sizes_3[..2], TileOptions::new(origin, 3, 2.0))
            .unwrap()
            .places;
        assert_eq!(two_shown[1], row_places[1]);

        // Translating origin by (30, -20) translates every tiled body by (30, -20).
        let moved_places = tile_layout(
            &sizes_3[..2],
            TileOptions::new(Point::new(origin.x + 30.0, origin.y - 20.0), 2, 2.0),
        )
        .unwrap()
        .places;
        assert_eq!(
            moved_places[0],
            Point::new(row_places[0].x + 30.0, row_places[0].y - 20.0)
        );

        // Dragging member 'a' over the third tile selects index 2 via nearest_place.
        let tile_centres: Vec<Point> = row_places
            .iter()
            .map(|place| rect(place.x, place.y, 100.0, 50.0).centre())
            .collect();
        let dragged_centre = rect(row_places[2].x + 10.0, row_places[2].y - 5.0, 100.0, 50.0).centre();
        assert_eq!(nearest_place(&tile_centres, dragged_centre), Some(2));

        // Switching freeform { a: [100, 50, 0, 0], b: [80, 40, 400, -300] } to 2-column tile.
        let freeform_boxes = [
            Rect {
                max_y: BAR_ROOM,
                ..rect(0.0, 0.0, 100.0, 50.0)
            },
            Rect {
                max_y: -300.0 + BAR_ROOM,
                ..rect(400.0, -300.0, 80.0, 40.0)
            },
        ];
        let u = union_rect(&freeform_boxes).unwrap();
        let tiled = tile_layout(
            &[Size::new(100.0, 50.0), Size::new(80.0, 40.0)],
            TileOptions::new(Point::new(u.min_x, u.max_y), 2, 2.0),
        )
        .unwrap()
        .places;
        assert_eq!(tiled[0], Point::new(0.0, 0.0));
        assert_eq!(tiled[1], Point::new(102.0, 0.0));
    }
}
