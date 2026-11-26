//! Integration tests for `wall_core::pointer::WallPointer`, porting every headless
//! `WallPointer` test from:
//! - `tools/wall-snap.test.mjs` (the 5 stand-in pointer snap tests / 14 assertions)
//! - `tools/wall-scroll.test.mjs` (the 2 stand-in wheel & quench tests / 11 assertions)
//! - `tools/wall-select.test.mjs` (all 7 body selection, click-counting, lasso, block,
//!   middle-paste, and double-click tests / 37 assertions)
//! - `tools/wall-mouse.test.mjs` (`cellAtPoint`, `programGets` / `heldButton` / `modsOf`,
//!   and all 7 program-mouse reporting tests / 52 assertions)
//!
//! Also exercises split-divider, split-resize, split-pane rearrange, and map-label gestures.

use std::collections::{HashMap, HashSet};

use term_cells::Screen;
use wall_core::geom::{Point, Rect};
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::layout::{
    edges_at, hit_test, resize_from_drag, Edges, GridSize, HitPart, HitTarget, MinGridSize,
};
use wall_core::pointer::{
    cell_at_point, held_button, mods_of, program_gets, Buttons, CanvasHit, CellPoint, CursorIcon,
    DropPreview, FullscreenState, GestureKind, MouseAction, MouseTracking, PointerConfig,
    PointerEvent, PointerHost, ProgramMouseButton, SelectionMode, SplitEdge, TerminalMouseEvent,
    WallPointer, WheelEvent, MIN_SIZE,
};
use wall_core::scroll::{DeltaMode, STREAM_IDLE_MS};
use wall_core::selection::{screen_from_lines, TerminalSelection};
use wall_core::split::{Axis, BranchSide};

// ── Unified stand-in wall & terminal test double ────────────────────────────

#[derive(Clone, Debug)]
struct MockTerminal {
    cols: u32,
    rows: u32,
    position: Point,
    stride: Point,
    /// Panel padding added onto `(cols * stride.x, rows * stride.y)` when `custom_body` is `None`.
    panel_pad: f64,
    /// Explicit body rect override (when `Some`, translated by `position` delta from `(0, 0)`).
    custom_body: Option<Rect>,
    title_height: f64,
    edge_tol: f64,
    tracking: MouseTracking,
    screen: Screen,
    selection: TerminalSelection,
    shown: bool,
}

impl MockTerminal {
    fn new(cols: u32, rows: u32, stride: Point) -> Self {
        Self {
            cols,
            rows,
            position: Point::new(0.0, 0.0),
            stride,
            panel_pad: 0.0,
            custom_body: None,
            title_height: 1.0,
            edge_tol: 0.0,
            tracking: MouseTracking::None,
            screen: Screen::new(cols as u16, rows as u16),
            selection: TerminalSelection::new(),
            shown: true,
        }
    }

    fn set_lines(&mut self, raw: &str) {
        let mut lines: Vec<&str> = raw.split("\r\n").collect();
        lines.resize(self.rows as usize, "");
        self.screen = screen_from_lines(&lines, self.cols as u16, &[], |_| false);
    }

    fn body_rect(&self) -> Rect {
        if let Some(base) = self.custom_body {
            Rect {
                min_x: base.min_x + self.position.x,
                max_x: base.max_x + self.position.x,
                min_y: base.min_y + self.position.y,
                max_y: base.max_y + self.position.y,
            }
        } else {
            let width = f64::from(self.cols) * self.stride.x + self.panel_pad;
            let height = f64::from(self.rows) * self.stride.y + self.panel_pad;
            Rect {
                min_x: self.position.x,
                max_x: self.position.x + width,
                max_y: self.position.y,
                min_y: self.position.y - height,
            }
        }
    }

    fn window_rect(&self) -> Rect {
        let body = self.body_rect();
        Rect {
            max_y: body.max_y + self.title_height,
            ..body
        }
    }
}

struct StandInWall {
    config: PointerConfig,
    camera_pos: Point,
    zoom: f64,
    viewport_h: f64,
    camera_target: Option<(f64, f64, f64)>,
    focused_id: Option<String>,
    hovered_id: Option<String>,
    fullscreen: Option<FullscreenState>,
    cursor: CursorIcon,
    captured: HashSet<i32>,
    terminals: HashMap<String, MockTerminal>,
    order: Vec<String>,
    groups: HashMap<String, Vec<String>>,
    group_headers: HashMap<String, Rect>,
    tiled_members: HashSet<String>,
    split_members: HashMap<String, String>,
    split_rects: HashMap<String, Rect>,
    split_edges: HashMap<String, SplitEdge>,
    forced_hit: Option<CanvasHit>,
    selection_clipboard: String,
    // Recorded side effects for assertions:
    commands: Vec<Vec<String>>,
    focus_calls: Vec<String>,
    selections_made: Vec<(String, String)>,
    sent_mouse: Vec<TerminalMouseEvent>,
    panned: Vec<(f64, f64)>,
    zoomed: Vec<f64>,
    group_moves: Vec<Point>,
    divider_drags: Vec<(String, Vec<BranchSide>, f64)>,
    split_resizes: Vec<(String, Rect)>,
    pane_drops: Vec<(String, Point)>,
    remembered_positions: Vec<String>,
    fit_all_count: usize,
}

impl StandInWall {
    fn new(zoom: f64) -> Self {
        Self {
            config: PointerConfig::default(),
            camera_pos: Point::new(0.0, 0.0),
            zoom,
            viewport_h: 600.0,
            camera_target: None,
            focused_id: None,
            hovered_id: None,
            fullscreen: None,
            cursor: CursorIcon::Default,
            captured: HashSet::new(),
            terminals: HashMap::new(),
            order: Vec::new(),
            groups: HashMap::new(),
            group_headers: HashMap::new(),
            tiled_members: HashSet::new(),
            split_members: HashMap::new(),
            split_rects: HashMap::new(),
            split_edges: HashMap::new(),
            forced_hit: None,
            selection_clipboard: String::new(),
            commands: Vec::new(),
            focus_calls: Vec::new(),
            selections_made: Vec::new(),
            sent_mouse: Vec::new(),
            panned: Vec::new(),
            zoomed: Vec::new(),
            group_moves: Vec::new(),
            divider_drags: Vec::new(),
            split_resizes: Vec::new(),
            pane_drops: Vec::new(),
            remembered_positions: Vec::new(),
            fit_all_count: 0,
        }
    }

    fn add_terminal(&mut self, id: &str, term: MockTerminal) {
        self.order.push(id.to_owned());
        self.terminals.insert(id.to_owned(), term);
    }

    fn ease_to(&mut self, x: f64, y: f64, zoom: f64) {
        self.camera_target = Some((x, y, zoom));
    }

    /// Screen coordinates inside cell `(col, row)` at fractional offset `(fx, fy)` for a
    /// terminal at `(0, 0)` under `self.zoom`.
    fn screen_at_cell(&self, id: &str, col: u32, row: u32, fx: f64, fy: f64) -> Point {
        let term = &self.terminals[id];
        let sx = term.stride.x;
        let sy = term.stride.y;
        Point::new(
            self.zoom * (f64::from(col) + fx) * sx,
            -self.zoom * (sy * 0.5 - (f64::from(row) + fy) * sy),
        )
    }

    fn selection_text(&self, id: &str) -> String {
        let term = &self.terminals[id];
        term.selection.text(&term.screen)
    }
}

impl PointerHost for StandInWall {
    fn config(&self) -> PointerConfig {
        self.config
    }

    fn camera_pos(&self) -> Point {
        self.camera_pos
    }

    fn camera_zoom(&self) -> f64 {
        self.zoom
    }

    fn viewport_height(&self) -> f64 {
        self.viewport_h
    }

    fn to_world(&self, screen_x: f64, screen_y: f64) -> Point {
        Point::new(screen_x / self.zoom, -screen_y / self.zoom)
    }

    fn focused_id(&self) -> Option<&str> {
        self.focused_id.as_deref()
    }

    fn fullscreen(&self) -> Option<FullscreenState> {
        self.fullscreen.clone()
    }

    fn hit(&self, world: Point) -> Option<CanvasHit> {
        if let Some(hit) = &self.forced_hit {
            return Some(hit.clone());
        }
        if let Some(FullscreenState::Single { id }) = &self.fullscreen {
            let term = self.terminals.get(id)?;
            return term.body_rect().contains(world).then(|| CanvasHit::Terminal {
                id: id.clone(),
                part: HitPart::Body,
            });
        }
        for id in self.order.iter().rev() {
            let Some(term) = self.terminals.get(id) else {
                continue;
            };
            if !term.shown {
                continue;
            }
            let win = term.window_rect();
            let body = term.body_rect();
            if term.edge_tol > 0.0 {
                if let Some(hit) =
                    hit_test(&[HitTarget::new(id.clone(), win, body)], world.x, world.y, term.edge_tol)
                {
                    return Some(CanvasHit::from(hit));
                }
            } else if win.contains(world) {
                let part = if world.y > body.max_y {
                    HitPart::Title
                } else {
                    HitPart::Body
                };
                return Some(CanvasHit::Terminal {
                    id: id.clone(),
                    part,
                });
            }
        }
        for (gid, rect) in &self.group_headers {
            if rect.contains(world) {
                return Some(CanvasHit::GroupHeader { group: gid.clone() });
            }
        }
        None
    }

    fn has_terminal(&self, id: &str) -> bool {
        self.terminals.contains_key(id)
    }

    fn is_shown(&self, id: &str) -> bool {
        self.terminals.get(id).is_some_and(|t| t.shown)
    }

    fn terminal_position(&self, id: &str) -> Option<Point> {
        self.terminals.get(id).map(|t| t.position)
    }

    fn terminal_size(&self, id: &str) -> Option<GridSize> {
        self.terminals.get(id).map(|t| GridSize::new(t.cols, t.rows))
    }

    fn terminal_stride(&self, id: &str) -> Option<Point> {
        self.terminals.get(id).map(|t| t.stride)
    }

    fn terminal_rect(&self, id: &str) -> Option<Rect> {
        self.terminals.get(id).map(MockTerminal::body_rect)
    }

    fn window_rect(&self, id: &str) -> Option<Rect> {
        self.terminals.get(id).map(MockTerminal::window_rect)
    }

    fn canvas_order(&self) -> Vec<String> {
        if self.fullscreen.is_some() {
            return Vec::new();
        }
        self.order
            .iter()
            .filter(|id| self.is_shown(id))
            .cloned()
            .collect()
    }

    fn cell_at(&self, id: &str, screen_x: f64, screen_y: f64) -> Option<CellPoint> {
        let term = self.terminals.get(id)?;
        let world = self.to_world(screen_x, screen_y);
        Some(cell_at_point(
            term.cols,
            term.rows,
            term.stride,
            world.x - term.position.x,
            world.y - term.position.y,
        ))
    }

    fn mouse_tracking(&self, id: &str) -> MouseTracking {
        self.terminals
            .get(id)
            .map_or(MouseTracking::None, |t| t.tracking)
    }

    fn has_selection(&self, id: &str) -> bool {
        self.terminals
            .get(id)
            .is_some_and(|t| !t.selection.is_empty())
    }

    fn has_selection_clipboard(&self) -> bool {
        !self.selection_clipboard.is_empty()
    }

    fn has_group(&self, group_id: &str) -> bool {
        self.groups.contains_key(group_id)
    }

    fn group_of(&self, id: &str) -> Option<String> {
        if let Some(gid) = self.split_members.get(id) {
            return Some(gid.clone());
        }
        self.groups
            .iter()
            .find_map(|(gid, members)| members.iter().any(|m| m == id).then(|| gid.clone()))
    }

    fn is_tiled_member(&self, id: &str) -> bool {
        self.tiled_members.contains(id)
    }

    fn is_split_member(&self, id: &str) -> bool {
        self.split_members.contains_key(id)
    }

    fn shown_group_members(&self, group_id: &str) -> Vec<String> {
        self.groups
            .get(group_id)
            .into_iter()
            .flatten()
            .filter(|id| self.is_shown(id))
            .cloned()
            .collect()
    }

    fn split_edge(&self, id: &str, edges: Edges) -> Option<SplitEdge> {
        if let Some(explicit) = self.split_edges.get(id) {
            return Some(explicit.clone());
        }
        let gid = self.split_members.get(id)?;
        Some(SplitEdge::Outer {
            group: gid.clone(),
            edges,
        })
    }

    fn split_rect(&self, group_id: &str) -> Option<Rect> {
        self.split_rects.get(group_id).copied()
    }

    fn set_pointer_capture(&mut self, pointer_id: i32) {
        self.captured.insert(pointer_id);
    }

    fn release_pointer_capture(&mut self, pointer_id: i32) {
        self.captured.remove(&pointer_id);
    }

    fn cancel_camera_ease(&mut self) {
        self.camera_target = None;
    }

    fn camera_move_to(&mut self, x: f64, y: f64) {
        self.camera_pos = Point::new(x, y);
    }

    fn camera_pan_by(&mut self, dx_px: f64, dy_px: f64) {
        self.camera_target = None;
        self.panned.push((dx_px, dy_px));
    }

    fn camera_zoom_at(&mut self, _screen_x: f64, _screen_y: f64, factor: f64) {
        self.camera_target = None;
        self.zoomed.push(factor);
    }

    fn camera_wheel_zoom_at(&mut self, _screen_x: f64, _screen_y: f64, dy_px: f64) {
        self.camera_target = None;
        self.zoomed.push(dy_px);
    }

    fn fit_all(&mut self) {
        self.fit_all_count += 1;
    }

    fn focus(&mut self, id: Option<&str>, frame: bool) {
        let label = match (id, frame) {
            (Some(id), true) => format!("{id} framed"),
            (Some(id), false) => id.to_owned(),
            (None, _) => "<none>".to_owned(),
        };
        self.focus_calls.push(label);
        self.focused_id = id.map(str::to_owned);
    }

    fn set_hovered(&mut self, id: Option<&str>) {
        self.hovered_id = id.map(str::to_owned);
    }

    fn set_cursor(&mut self, cursor: CursorIcon) {
        self.cursor = cursor;
    }

    fn set_terminal_position(&mut self, id: &str, pos: Point) {
        if let Some(term) = self.terminals.get_mut(id) {
            term.position = pos;
        }
    }

    fn remember_position(&mut self, id: &str) {
        self.remembered_positions.push(id.to_owned());
    }

    fn clear_selection(&mut self, id: &str) {
        if let Some(term) = self.terminals.get_mut(id) {
            term.selection.clear();
        }
    }

    fn select_start(
        &mut self,
        id: &str,
        cell_x: f64,
        cell_y: f64,
        mode: SelectionMode,
        block: bool,
    ) {
        if let Some(term) = self.terminals.get_mut(id) {
            term.selection
                .start(&term.screen, cell_x, cell_y, mode, block);
        }
    }

    fn select_extend(&mut self, id: &str, cell_x: f64, cell_y: f64) {
        if let Some(term) = self.terminals.get_mut(id) {
            term.selection.extend(&term.screen, cell_x, cell_y);
        }
    }

    fn selection_made(&mut self, id: &str) {
        let text = self.selection_text(id);
        self.selections_made.push((id.to_owned(), text));
    }

    fn send_mouse(&mut self, _id: &str, event: TerminalMouseEvent) {
        self.sent_mouse.push(event);
    }

    fn run_command(&mut self, args: &[String]) {
        self.commands.push(args.to_vec());
    }

    fn group_drag_to(&mut self, _group_id: &str, dx: f64, dy: f64) {
        self.group_moves.push(Point::new(dx, dy));
    }

    fn pane_drop(&mut self, _id: &str, world: Point) -> Option<DropPreview> {
        Some(DropPreview {
            rect: Rect::new(world.x, world.y - 10.0, world.x + 20.0, world.y),
            ok: true,
        })
    }

    fn drop_pane(&mut self, id: &str, world: Point) {
        self.pane_drops.push((id.to_owned(), world));
    }

    fn drag_divider(&mut self, group_id: &str, path: &[BranchSide], coord: f64) {
        self.divider_drags
            .push((group_id.to_owned(), path.to_vec(), coord));
    }

    fn resize_split(&mut self, group_id: &str, rect: Rect) {
        self.split_resizes.push((group_id.to_owned(), rect));
    }
}

// ── 1. Ported stand-in WallPointer tests from tools/wall-snap.test.mjs ──────

fn snap_stand_in(snap: bool, tiled: bool) -> (StandInWall, WallPointer) {
    let mut wall = StandInWall::new(4.0);
    wall.config.snap = snap;
    let mut term = MockTerminal::new(80, 24, Point::new(1.2, 2.4));
    term.position = Point::new(1.0, -1.0);
    term.panel_pad = 0.6;
    term.title_height = 2.0;
    wall.add_terminal("t", term);
    wall.groups.insert("g-1".to_owned(), vec!["t".to_owned()]);
    if tiled {
        wall.tiled_members.insert("t".to_owned());
    }
    (wall, WallPointer::new())
}

fn snap_width_of(cols: u32) -> f64 {
    f64::from(cols) * 1.2 + 0.6
}

#[test]
fn snap_title_bar_drag_snaps_top_left_and_ctrl_inverts_live() {
    let (mut wall, mut pointer) = snap_stand_in(true, false);
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "t".to_owned(),
        part: HitPart::Title,
    });
    pointer.on_down(&mut wall, &PointerEvent::default());

    // 41 px at zoom 4 = 10.25 world units right: corner (1, -1) -> (12, 0).
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(12.0, 0.0));

    // Ctrl held: free (unsnapped).
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(11.25, -1.0));

    // Ctrl released with pointer stationary: snapped again.
    pointer.on_key(
        &mut wall,
        &KeyEvent {
            key: "Control".to_owned(),
            mods: Mods {
                ctrl: false,
                ..Mods::default()
            },
            pressed: false,
            ..KeyEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(12.0, 0.0));

    // Ctrl pressed again with pointer stationary: free again.
    pointer.on_key(
        &mut wall,
        &KeyEvent {
            key: "Control".to_owned(),
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            pressed: true,
            ..KeyEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(11.25, -1.0));
}

#[test]
fn snap_off_does_not_snap_and_ctrl_held_snaps() {
    let (mut wall, mut pointer) = snap_stand_in(false, false);
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "t".to_owned(),
        part: HitPart::Title,
    });
    pointer.on_down(&mut wall, &PointerEvent::default());

    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(11.25, -1.0));

    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(12.0, 0.0));
}

#[test]
fn tiled_member_drag_never_snaps() {
    let (mut wall, mut pointer) = snap_stand_in(true, true);
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "t".to_owned(),
        part: HitPart::Title,
    });
    pointer.on_down(&mut wall, &PointerEvent::default());
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.terminals["t"].position, Point::new(11.25, -1.0));
}

#[test]
fn border_drag_resizes_to_snapped_edge_and_wobble_inside_slop_resizes_nothing() {
    let (mut wall, mut pointer) = snap_stand_in(true, false);
    let right_edge = Edges {
        right: true,
        ..Edges::default()
    };
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "t".to_owned(),
        part: HitPart::Edge(right_edge),
    });
    pointer.on_down(&mut wall, &PointerEvent::default());

    // 2 px wobble (<= DRAG_SLOP_PX = 4): resizes nothing.
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 2.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert!(wall.commands.is_empty());

    // 30 px = 7.5 world units right: edge (97.6 -> 105.1) snaps to line at 104.
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 30.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    let last_cmd = wall.commands.last().expect("expected terminal.resize");
    assert_eq!(&last_cmd[0..2], &["terminal.resize", "t"]);
    let cols: u32 = last_cmd[2].parse().expect("valid u32 cols");
    let edge = 1.0 + snap_width_of(cols);
    assert!(edge <= 104.0 + 1e-9);
    assert!(104.0 - edge < 1.2);

    // Ctrl held: plain cell rounding.
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 30.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            ..PointerEvent::default()
        },
    );
    let unsnapped_cols: u32 = wall
        .commands
        .last()
        .expect("expected unsnapped resize")[2]
        .parse()
        .expect("valid u32 cols");
    assert_eq!(
        unsnapped_cols,
        resize_from_drag(
            right_edge,
            GridSize::new(80, 24),
            Point::new(1.2, 2.4),
            7.5,
            0.0,
            MIN_SIZE
        )
        .cols
    );
}

#[test]
fn group_header_drag_lands_group_top_left_body_corner_on_grid() {
    let (mut wall, mut pointer) = snap_stand_in(true, false);
    wall.forced_hit = Some(CanvasHit::GroupHeader {
        group: "g-1".to_owned(),
    });
    pointer.on_down(&mut wall, &PointerEvent::default());

    // (41, -9) px at zoom 4 = (10.25, 2.25) world: corner (1, -1) -> (12, 0), delta (11, 1).
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: -9.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.group_moves.last().copied(), Some(Point::new(11.0, 1.0)));

    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 41.0,
            y: -9.0,
            buttons: Buttons::LEFT,
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.group_moves.last().copied(),
        Some(Point::new(10.25, 2.25))
    );
}

// ── 2. Ported stand-in WallPointer tests from tools/wall-scroll.test.mjs ────

#[test]
fn scroll_horizontal_swipe_emits_nothing_and_cancel_scroll_halts_wheel_momentum() {
    let mut wall = StandInWall::new(10.0);
    wall.focused_id = Some("t1".to_owned());
    let mut term = MockTerminal::new(80, 24, Point::new(1.0, 2.0));
    term.custom_body = Some(Rect::new(0.0, -48.0, 80.0, 0.0));
    wall.add_terminal("t1", term);
    let mut pointer = WallPointer::new();

    // Pure horizontal wheel over focused terminal (delta_y = 0) never scrolls it.
    let at_5_6 = wall.screen_at_cell("t1", 5, 6, 0.0, 0.5);
    pointer.on_wheel(
        &mut wall,
        &WheelEvent {
            x: at_5_6.x,
            y: at_5_6.y,
            delta_x: 25.0,
            delta_y: 0.0,
            delta_mode: DeltaMode::Pixel,
            mods: Mods::default(),
            time_ms: 100.0,
        },
    );
    assert!(wall.commands.is_empty());

    // Discrete notch starts gliding (+1 immediate, +4 pending at row_px = 2 * 10 = 20);
    // cancelling scroll on keypress halts the rest.
    pointer.on_wheel(
        &mut wall,
        &WheelEvent {
            x: at_5_6.x,
            y: at_5_6.y,
            delta_x: 0.0,
            delta_y: -100.0,
            delta_mode: DeltaMode::Pixel,
            mods: Mods::default(),
            time_ms: 110.0,
        },
    );
    assert_eq!(
        wall.commands,
        vec![vec![
            "terminal.scroll".to_owned(),
            "t1".to_owned(),
            "1".to_owned(),
            "5".to_owned(),
            "6.5".to_owned(),
        ]]
    );
    assert!(pointer.gliding());

    pointer.cancel_scroll();
    for _ in 0..10 {
        pointer.step(&mut wall, 1.0 / 60.0);
    }
    assert_eq!(wall.commands.len(), 1);
}

#[test]
fn fullscreen_transition_mid_flick_quenches_momentum_and_guards_mid_stream_modifiers() {
    let mut wall = StandInWall::new(10.0);
    wall.focused_id = Some("t1".to_owned());
    wall.fullscreen = Some(FullscreenState::Single {
        id: "t1".to_owned(),
    });
    let mut term = MockTerminal::new(80, 24, Point::new(1.0, 2.0));
    term.custom_body = Some(Rect::new(0.0, -48.0, 80.0, 0.0));
    wall.add_terminal("t1", term);
    let mut pointer = WallPointer::new();

    let wheel = |wall: &mut StandInWall,
                 pointer: &mut WallPointer,
                 dx: f64,
                 dy: f64,
                 now: f64,
                 mods: Mods| {
        pointer.on_wheel(
            wall,
            &WheelEvent {
                x: 900.0,
                y: 400.0,
                delta_x: dx,
                delta_y: dy,
                delta_mode: DeltaMode::Pixel,
                mods,
                time_ms: now,
            },
        );
    };

    // (Opt + fling in fullscreen) -> (Cmd + Enter exit while fling crests & decays).
    let mut t = 1000.0;
    let alt_only = Mods {
        alt: true,
        ..Mods::default()
    };
    t += 8.0;
    wheel(&mut wall, &mut pointer, 0.0, -14.0, t, alt_only);
    t += 8.0;
    wheel(&mut wall, &mut pointer, 0.0, -28.0, t, alt_only);
    assert!(wall.zoomed.is_empty());

    wall.fullscreen = None;
    wall.ease_to(0.0, 0.0, 10.0);
    pointer.quench_wheel(t);

    let alt_meta = Mods {
        alt: true,
        meta: true,
        ..Mods::default()
    };
    let meta_only = Mods {
        meta: true,
        ..Mods::default()
    };
    for (dy, mods) in [
        (-75.0, alt_meta),
        (-90.0, meta_only),
        (-64.0, meta_only),
        (-42.0, alt_only),
        (-24.0, Mods::default()),
        (-10.0, Mods::default()),
        (-3.0, Mods::default()),
    ] {
        t += 8.0;
        wheel(&mut wall, &mut pointer, 0.0, dy, t, mods);
    }
    assert!(wall.zoomed.is_empty());
    assert!(wall.panned.is_empty());
    assert!(wall.commands.is_empty());
    assert_eq!(wall.camera_target, Some((0.0, 0.0, 10.0)));

    // After > 120 ms, a deliberate fresh flick in the opposite direction (+40) releases quench.
    t += 130.0;
    wheel(&mut wall, &mut pointer, 0.0, 40.0, t, Mods::default());
    assert_eq!(wall.panned, vec![(0.0, 40.0)]);

    // Plain fling on canvas, then pressing ⌘ mid-momentum: no zoomies, while Ctrl pinch zooms.
    wall.panned.clear();
    wall.zoomed.clear();
    t += STREAM_IDLE_MS + 50.0;
    t += 8.0;
    wheel(&mut wall, &mut pointer, 0.0, 30.0, t, Mods::default());
    t += 8.0;
    wheel(&mut wall, &mut pointer, 0.0, 24.0, t, meta_only);
    assert!(wall.zoomed.is_empty());
    assert_eq!(wall.panned.len(), 2);

    t += 8.0;
    wheel(
        &mut wall,
        &mut pointer,
        0.0,
        -20.0,
        t,
        Mods {
            ctrl: true,
            ..Mods::default()
        },
    );
    assert_eq!(wall.zoomed.len(), 1);

    // After STREAM_IDLE_MS of quiet, normal scrolling in fullscreen works right away.
    wall.fullscreen = Some(FullscreenState::Single {
        id: "t1".to_owned(),
    });
    wall.commands.clear();
    t += STREAM_IDLE_MS + 20.0;
    wheel(&mut wall, &mut pointer, 0.0, -40.0, t, Mods::default());
    assert_eq!(wall.commands.len(), 1);
}

// ── 3. Ported WallPointer tests from tools/wall-select.test.mjs ─────────────

const SELECT_ZOOM: f64 = 100.0;

fn select_stand_in(focused: Option<&str>) -> (StandInWall, WallPointer) {
    let mut wall = StandInWall::new(SELECT_ZOOM);
    wall.focused_id = focused.map(str::to_owned);
    // 30 × 6 grid at zoom 100, cell stride 0.2 × 0.4 (~20 px wide, 40 px tall per cell).
    let mut term = MockTerminal::new(30, 6, Point::new(0.2, 0.4));
    term.custom_body = Some(Rect::new(0.0, 0.2 - 6.0 * 0.4, 30.0 * 0.2, 0.2));
    term.title_height = 1.0;
    term.set_lines("hello world foo\r\nsecond (line) here\r\n  indented line");
    wall.add_terminal("t", term);
    (wall, WallPointer::new())
}

struct SelectClock(f64);

impl SelectClock {
    fn new() -> Self {
        Self(1000.0)
    }

    fn press(
        &mut self,
        wall: &mut StandInWall,
        pointer: &mut WallPointer,
        pos: Point,
        mods: Mods,
        button: Button,
        dt: f64,
    ) {
        self.0 += dt;
        pointer.on_down(
            wall,
            &PointerEvent {
                pointer_id: 1,
                x: pos.x,
                y: pos.y,
                button: Some(button),
                buttons: Buttons::from_button(button),
                mods,
                time_ms: self.0,
            },
        );
    }

    fn move_to(&self, wall: &mut StandInWall, pointer: &mut WallPointer, pos: Point, mods: Mods, buttons: Buttons) {
        pointer.on_move(
            wall,
            &PointerEvent {
                pointer_id: 1,
                x: pos.x,
                y: pos.y,
                button: Some(Button::Left),
                buttons,
                mods,
                time_ms: self.0,
            },
        );
    }

    fn up(&self, wall: &mut StandInWall, pointer: &mut WallPointer, pos: Point, mods: Mods, button: Button) {
        pointer.on_up(
            wall,
            &PointerEvent {
                pointer_id: 1,
                x: pos.x,
                y: pos.y,
                button: Some(button),
                buttons: Buttons::NONE,
                mods,
                time_ms: self.0,
            },
        );
    }

    fn drag(&mut self, wall: &mut StandInWall, pointer: &mut WallPointer, from: Point, to: Point, mods: Mods) {
        self.press(wall, pointer, from, mods, Button::Left, 2000.0);
        self.move_to(wall, pointer, to, mods, Buttons::LEFT);
        self.up(wall, pointer, to, mods, Button::Left);
    }
}

#[test]
fn body_drag_selects_from_press_to_pointer_focuses_and_click_clears() {
    let (mut wall, mut pointer) = select_stand_in(None);
    let mut clock = SelectClock::new();

    let p0 = wall.screen_at_cell("t", 0, 0, 0.2, 0.5);
    clock.press(&mut wall, &mut pointer, p0, Mods::default(), Button::Left, 2000.0);
    assert_eq!(wall.focus_calls, vec!["t"]);
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Text));

    let p1 = wall.screen_at_cell("t", 4, 0, 0.7, 0.5);
    clock.move_to(&mut wall, &mut pointer, p1, Mods::default(), Buttons::LEFT);
    assert_eq!(wall.selection_text("t"), "hello");

    let p2 = wall.screen_at_cell("t", 5, 1, 0.7, 0.5);
    clock.move_to(&mut wall, &mut pointer, p2, Mods::default(), Buttons::LEFT);
    assert_eq!(wall.selection_text("t"), "hello world foo\nsecond");

    clock.up(&mut wall, &mut pointer, p2, Mods::default(), Button::Left);
    assert_eq!(
        wall.selections_made,
        vec![("t".to_owned(), "hello world foo\nsecond".to_owned())]
    );

    // A click with no travel clears it, and selects nothing.
    let c22 = wall.screen_at_cell("t", 2, 2, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c22, Mods::default(), Button::Left, 2000.0);
    assert!(!wall.has_selection("t"));
    clock.up(&mut wall, &mut pointer, c22, Mods::default(), Button::Left);
    assert_eq!(wall.selections_made.len(), 1);

    // A wobble inside DRAG_SLOP_PX is still a click.
    let w0 = wall.screen_at_cell("t", 2, 2, 0.2, 0.5);
    let w1 = wall.screen_at_cell("t", 2, 2, 0.25, 0.5);
    clock.press(&mut wall, &mut pointer, w0, Mods::default(), Button::Left, 2000.0);
    clock.move_to(&mut wall, &mut pointer, w1, Mods::default(), Buttons::LEFT);
    clock.up(&mut wall, &mut pointer, w1, Mods::default(), Button::Left);
    assert!(!wall.has_selection("t"));
}

#[test]
fn presses_within_click_repeat_ms_select_word_then_line_then_single_again() {
    let (mut wall, mut pointer) = select_stand_in(Some("t"));
    let mut clock = SelectClock::new();

    let c70 = wall.screen_at_cell("t", 7, 0, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c70, Mods::default(), Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, c70, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c70, Mods::default(), Button::Left, 100.0);
    clock.up(&mut wall, &mut pointer, c70, Mods::default(), Button::Left);
    assert_eq!(wall.selection_text("t"), "world");
    assert_eq!(
        wall.selections_made.last(),
        Some(&("t".to_owned(), "world".to_owned()))
    );

    clock.press(&mut wall, &mut pointer, c70, Mods::default(), Button::Left, 100.0);
    assert_eq!(wall.selection_text("t"), "hello world foo");
    clock.up(&mut wall, &mut pointer, c70, Mods::default(), Button::Left);

    // Fourth click wraps back to single cell click and clears.
    clock.press(&mut wall, &mut pointer, c70, Mods::default(), Button::Left, 100.0);
    assert!(!wall.has_selection("t"));
    clock.up(&mut wall, &mut pointer, c70, Mods::default(), Button::Left);

    // A double-click's drag grows by words.
    let c81 = wall.screen_at_cell("t", 8, 1, 0.5, 0.5);
    let c151 = wall.screen_at_cell("t", 15, 1, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c81, Mods::default(), Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, c81, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c81, Mods::default(), Button::Left, 100.0);
    assert_eq!(wall.selection_text("t"), "line");
    clock.move_to(&mut wall, &mut pointer, c151, Mods::default(), Buttons::LEFT);
    clock.up(&mut wall, &mut pointer, c151, Mods::default(), Button::Left);
    assert_eq!(wall.selection_text("t"), "line) here");

    // Presses further apart in time or place stay single clicks.
    let c22 = wall.screen_at_cell("t", 2, 2, 0.5, 0.5);
    let c92 = wall.screen_at_cell("t", 9, 2, 0.5, 0.5);
    let slow_gap = wall.config.click_repeat_ms + 1.0;
    clock.press(&mut wall, &mut pointer, c22, Mods::default(), Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, c22, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c22, Mods::default(), Button::Left, slow_gap);
    clock.up(&mut wall, &mut pointer, c22, Mods::default(), Button::Left);
    assert!(!wall.has_selection("t"));

    clock.press(&mut wall, &mut pointer, c22, Mods::default(), Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, c22, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c92, Mods::default(), Button::Left, 50.0);
    clock.up(&mut wall, &mut pointer, c92, Mods::default(), Button::Left);
    assert!(!wall.has_selection("t"));

    // Line mode trims leading blanks.
    let c52 = wall.screen_at_cell("t", 5, 2, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c52, Mods::default(), Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, c52, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c52, Mods::default(), Button::Left, 50.0);
    clock.up(&mut wall, &mut pointer, c52, Mods::default(), Button::Left);
    clock.press(&mut wall, &mut pointer, c52, Mods::default(), Button::Left, 50.0);
    clock.up(&mut wall, &mut pointer, c52, Mods::default(), Button::Left);
    assert_eq!(wall.selection_text("t"), "indented line");
}

#[test]
fn shift_drag_on_body_selects_text_shift_click_toggles_and_shift_off_body_lassoes() {
    let (mut wall, mut pointer) = select_stand_in(None);
    let mut clock = SelectClock::new();
    let shift = Mods {
        shift: true,
        ..Mods::default()
    };

    let from = wall.screen_at_cell("t", 0, 1, 0.1, 0.5);
    let to = wall.screen_at_cell("t", 6, 1, 0.1, 0.5);
    clock.drag(&mut wall, &mut pointer, from, to, shift);
    assert_eq!(wall.selection_text("t"), "second");
    assert_eq!(wall.focus_calls, vec!["t"]);
    assert!(wall.commands.is_empty());

    let mid = wall.screen_at_cell("t", 3, 1, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, mid, shift, Button::Left, 2000.0);
    clock.up(&mut wall, &mut pointer, mid, shift, Button::Left);
    assert_eq!(
        wall.commands,
        vec![vec!["wall.select.toggle".to_owned(), "t".to_owned()]]
    );
    assert_eq!(wall.selection_text("t"), "second");

    // Shift-drag starting on the title bar turns into a lasso.
    let body_max_y = wall.terminals["t"].body_rect().max_y;
    let title_pt = Point::new(
        wall.screen_at_cell("t", 3, 0, 0.5, 0.5).x,
        -SELECT_ZOOM * (body_max_y + 0.5),
    );
    let lasso_end = Point::new(title_pt.x + 80.0, title_pt.y + 80.0);
    clock.press(&mut wall, &mut pointer, title_pt, shift, Button::Left, 2000.0);
    clock.move_to(&mut wall, &mut pointer, lasso_end, shift, Buttons::LEFT);
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Lasso));
    clock.up(&mut wall, &mut pointer, lasso_end, shift, Button::Left);
    assert_eq!(
        wall.commands.last(),
        Some(&vec!["wall.select.add".to_owned(), "t".to_owned()])
    );
}

#[test]
fn reporting_program_gets_plain_drag_while_shift_drag_selects_anyway() {
    let (mut wall, mut pointer) = select_stand_in(Some("t"));
    wall.terminals.get_mut("t").unwrap().tracking = MouseTracking::Button;
    let mut clock = SelectClock::new();

    let p00 = wall.screen_at_cell("t", 0, 0, 0.5, 0.5);
    let p40 = wall.screen_at_cell("t", 4, 0, 0.5, 0.5);
    clock.drag(&mut wall, &mut pointer, p00, p40, Mods::default());
    let actions: Vec<&str> = wall.sent_mouse.iter().map(|m| m.action.as_str()).collect();
    assert_eq!(actions, vec!["press", "move", "release"]);
    assert!(!wall.has_selection("t"));

    wall.sent_mouse.clear();
    let s0 = wall.screen_at_cell("t", 0, 0, 0.1, 0.5);
    let s5 = wall.screen_at_cell("t", 5, 0, 0.1, 0.5);
    clock.drag(
        &mut wall,
        &mut pointer,
        s0,
        s5,
        Mods {
            shift: true,
            ..Mods::default()
        },
    );
    assert!(wall.sent_mouse.is_empty());
    assert_eq!(wall.selection_text("t"), "hello");
}

#[test]
fn block_modifiers_select_rectangular_block_and_alt_drag_still_moves_terminal() {
    for (platform, block_mods) in [
        (
            Platform::Mac,
            Mods {
                meta: true,
                alt: true,
                ..Mods::default()
            },
        ),
        (
            Platform::Other,
            Mods {
                ctrl: true,
                alt: true,
                ..Mods::default()
            },
        ),
    ] {
        let (mut wall, mut pointer) = select_stand_in(None);
        wall.config.platform = platform;
        let mut clock = SelectClock::new();

        let from = wall.screen_at_cell("t", 1, 0, 0.1, 0.5);
        let to = wall.screen_at_cell("t", 5, 1, 0.1, 0.5);
        clock.drag(&mut wall, &mut pointer, from, to, block_mods);
        let sel = &wall.terminals["t"].selection;
        assert!(sel.block);
        assert_eq!((sel.c0, sel.r0), (1, 0));
        assert_eq!((sel.c1, sel.r1), (5, 1));
        assert_eq!(wall.selection_text("t"), "ello\necon");

        let c10 = wall.screen_at_cell("t", 1, 0, 0.5, 0.5);
        let alt_only = Mods {
            alt: true,
            ..Mods::default()
        };
        clock.press(&mut wall, &mut pointer, c10, alt_only, Button::Left, 2000.0);
        assert_eq!(pointer.active_gesture(), Some(GestureKind::Press));
        clock.up(&mut wall, &mut pointer, c10, alt_only, Button::Left);
    }
}

#[test]
fn middle_click_on_body_pastes_selection_clipboard_and_middle_drag_pans() {
    let (mut wall, mut pointer) = select_stand_in(None);
    let mut clock = SelectClock::new();

    let c33 = wall.screen_at_cell("t", 3, 3, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c33, Mods::default(), Button::Middle, 2000.0);
    clock.up(&mut wall, &mut pointer, c33, Mods::default(), Button::Middle);
    assert!(wall.commands.is_empty());

    wall.selection_clipboard = "echo hi".to_owned();
    clock.press(&mut wall, &mut pointer, c33, Mods::default(), Button::Middle, 2000.0);
    clock.up(&mut wall, &mut pointer, c33, Mods::default(), Button::Middle);
    assert_eq!(
        wall.commands,
        vec![vec![
            "wall.paste".to_owned(),
            "t".to_owned(),
            "selection".to_owned(),
        ]]
    );

    let c123 = wall.screen_at_cell("t", 12, 3, 0.5, 0.5);
    clock.press(&mut wall, &mut pointer, c33, Mods::default(), Button::Middle, 2000.0);
    clock.move_to(&mut wall, &mut pointer, c123, Mods::default(), Buttons::MIDDLE);
    clock.up(&mut wall, &mut pointer, c123, Mods::default(), Button::Middle);
    assert_eq!(wall.commands.len(), 1);
    assert!(wall.focus_calls.is_empty());
}

#[test]
fn double_click_on_body_frames_nothing_while_title_bar_frames() {
    let (mut wall, mut pointer) = select_stand_in(None);
    let body_pt = wall.screen_at_cell("t", 3, 1, 0.5, 0.5);
    pointer.on_double_click(
        &mut wall,
        &PointerEvent {
            x: body_pt.x,
            y: body_pt.y,
            ..PointerEvent::default()
        },
    );
    assert!(wall.focus_calls.is_empty());

    let body_max_y = wall.terminals["t"].body_rect().max_y;
    let title_pt = Point::new(
        wall.screen_at_cell("t", 3, 0, 0.5, 0.5).x,
        -SELECT_ZOOM * (body_max_y + 0.5),
    );
    pointer.on_double_click(
        &mut wall,
        &PointerEvent {
            x: title_pt.x,
            y: title_pt.y,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.focus_calls, vec!["t framed"]);
}

// ── 4. Ported tests from tools/wall-mouse.test.mjs ──────────────────────────

#[test]
fn cell_at_point_matches_slot_left_edge_and_vertical_centre_geometry() {
    let stride = Point::new(0.7, 1.5);
    for (c, r) in [(0_u32, 0_u32), (3, 2), (9, 3)] {
        let slot_x = f64::from(c) * stride.x;
        let slot_y = -f64::from(r) * stride.y;
        let p1 = cell_at_point(10, 4, stride, slot_x + 0.01, slot_y);
        assert_eq!((p1.col, p1.row), (c, r));
        let p2 = cell_at_point(
            10,
            4,
            stride,
            slot_x + stride.x * 0.99,
            slot_y + stride.y * 0.49,
        );
        assert_eq!((p2.col, p2.row), (c, r));
        let p3 = cell_at_point(10, 4, stride, slot_x + 0.01, slot_y - stride.y * 0.51);
        assert_eq!(p3.row, (r + 1).min(3));
    }
    let mid = cell_at_point(
        10,
        4,
        stride,
        3.5 * stride.x,
        stride.y * 0.5 - 2.25 * stride.y,
    );
    assert!((mid.x - 3.5).abs() < 1e-9);
    assert!((mid.y - 2.25).abs() < 1e-9);

    assert_eq!(
        cell_at_point(10, 4, stride, -5.0, 50.0),
        CellPoint {
            x: 0.0,
            y: 0.0,
            col: 0,
            row: 0,
        }
    );
    assert_eq!(
        cell_at_point(10, 4, stride, 500.0, -500.0),
        CellPoint {
            x: 10.0,
            y: 4.0,
            col: 9,
            row: 3,
        }
    );
}

#[test]
fn program_gets_and_button_modifier_helpers_match_wall_mouse_rules() {
    let plain = Mods::default();
    let shift = Mods {
        shift: true,
        ..Mods::default()
    };
    let alt = Mods {
        alt: true,
        ..Mods::default()
    };

    assert!(program_gets(
        Some(Button::Left),
        plain,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(program_gets(
        Some(Button::Middle),
        plain,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(program_gets(
        Some(Button::Right),
        plain,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(program_gets(
        Some(Button::Back),
        plain,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Left),
        shift,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Right),
        shift,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Left),
        alt,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(program_gets(
        Some(Button::Right),
        alt,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Left),
        plain,
        HitPart::Body,
        false,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Left),
        plain,
        HitPart::Body,
        true,
        MouseTracking::None
    ));
    assert!(!program_gets(
        Some(Button::Left),
        plain,
        HitPart::Title,
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        Some(Button::Left),
        plain,
        HitPart::Edge(Edges::default()),
        true,
        MouseTracking::Normal
    ));
    assert!(!program_gets(
        None,
        plain,
        HitPart::Body,
        true,
        MouseTracking::Normal
    ));

    let mapped: Vec<ProgramMouseButton> = [0_u8, 1, 2, 4, 3, 8, 16]
        .into_iter()
        .map(|bits| held_button(Buttons(bits)))
        .collect();
    assert_eq!(
        mapped,
        vec![
            ProgramMouseButton::None,
            ProgramMouseButton::Left,
            ProgramMouseButton::Right,
            ProgramMouseButton::Middle,
            ProgramMouseButton::Left,
            ProgramMouseButton::Back,
            ProgramMouseButton::Forward,
        ]
    );
    assert_eq!(
        mods_of(Mods {
            shift: true,
            ctrl: true,
            alt: false,
            meta: true,
        }),
        vec!["Shift", "Control", "Meta"]
    );
}

fn mouse_stand_in(tracking: MouseTracking, focused: Option<&str>) -> (StandInWall, WallPointer) {
    let mut wall = StandInWall::new(10.0);
    wall.focused_id = focused.map(str::to_owned);
    let mut term = MockTerminal::new(80, 24, Point::new(1.0, 2.0));
    term.custom_body = Some(Rect::new(0.0, -47.0, 80.0, 1.0));
    term.title_height = 3.0;
    term.tracking = tracking;
    wall.add_terminal("t", term);
    (wall, WallPointer::new())
}

fn mouse_at(col: u32, row: u32, fx: f64, fy: f64) -> Point {
    Point::new(
        10.0 * (f64::from(col) + fx),
        20.0 * (f64::from(row) + fy) - 10.0,
    )
}

fn button_label(btn: ProgramMouseButton) -> &'static str {
    match btn {
        ProgramMouseButton::Left => "left",
        ProgramMouseButton::Middle => "middle",
        ProgramMouseButton::Right => "right",
        ProgramMouseButton::Back => "8",
        ProgramMouseButton::Forward => "9",
        ProgramMouseButton::None => "none",
    }
}

fn summary_cell(ev: &TerminalMouseEvent) -> String {
    format!(
        "{} {} {},{}",
        ev.action.as_str(),
        button_label(ev.button),
        ev.col.floor() as i32,
        ev.row.floor() as i32
    )
}

#[test]
fn mouse_click_on_focused_body_sends_press_and_release_with_fractional_cell() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Normal, Some("t"));
    let pt = mouse_at(3, 5, 0.5, 0.25);

    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: pt.x,
            y: pt.y,
            button: Some(Button::Left),
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Program));
    assert!(wall.captured.contains(&1));

    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: pt.x,
            y: pt.y,
            button: Some(Button::Left),
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.sent_mouse,
        vec![
            TerminalMouseEvent {
                action: MouseAction::Press,
                button: ProgramMouseButton::Left,
                col: 3.5,
                row: 5.25,
                mods: Mods::default(),
            },
            TerminalMouseEvent {
                action: MouseAction::Release,
                button: ProgramMouseButton::Left,
                col: 3.5,
                row: 5.25,
                mods: Mods::default(),
            },
        ]
    );
    assert!(wall.focus_calls.is_empty());
    assert!(!wall.captured.contains(&1));

    // Control rides along on a right-button press and release too.
    wall.sent_mouse.clear();
    let origin = mouse_at(0, 0, 0.5, 0.5);
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: origin.x,
            y: origin.y,
            button: Some(Button::Right),
            buttons: Buttons::RIGHT,
            mods: ctrl,
            ..PointerEvent::default()
        },
    );
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: origin.x,
            y: origin.y,
            button: Some(Button::Right),
            buttons: Buttons::NONE,
            mods: ctrl,
            ..PointerEvent::default()
        },
    );
    let right_summary: Vec<String> = wall
        .sent_mouse
        .iter()
        .map(|ev| {
            format!(
                "{} {} {}",
                ev.action.as_str(),
                button_label(ev.button),
                ev.mod_names().join(",")
            )
        })
        .collect();
    assert_eq!(
        right_summary,
        vec!["press right Control", "release right Control"]
    );
}

#[test]
fn mouse_drag_reports_once_per_cell_clamped_off_edge_until_release() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Button, Some("t"));
    let p11 = mouse_at(1, 1, 0.5, 0.5);
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    let p11_far = mouse_at(1, 1, 0.9, 0.9);
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p11_far.x,
            y: p11_far.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    let p21_near = mouse_at(2, 1, 0.1, 0.5);
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p21_near.x,
            y: p21_near.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    let p21_far = mouse_at(2, 1, 0.8, 0.5);
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p21_far.x,
            y: p21_far.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 2000.0,
            y: -500.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: 2000.0,
            y: -500.0,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );

    let got: Vec<String> = wall.sent_mouse.iter().map(summary_cell).collect();
    assert_eq!(
        got,
        vec![
            "press left 1,1",
            "move left 2,1",
            "move left 80,0",
            "release left 80,0",
        ]
    );
    assert_eq!((wall.sent_mouse[2].col, wall.sent_mouse[2].row), (80.0, 0.0));
}

#[test]
fn normal_tracking_ignores_drag_motion_and_reports_chorded_buttons_mid_gesture() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Normal, Some("t"));
    let p11 = mouse_at(1, 1, 0.5, 0.5);
    let p42 = mouse_at(4, 2, 0.5, 0.5);

    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p42.x,
            y: p42.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.sent_mouse
            .iter()
            .map(|e| e.action.as_str())
            .collect::<Vec<_>>(),
        vec!["press"]
    );

    // Right button joins (buttons = 3) and releases (buttons = 1) inside the gesture.
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p42.x,
            y: p42.y,
            button: Some(Button::Right),
            buttons: Buttons(3),
            ..PointerEvent::default()
        },
    );
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p42.x,
            y: p42.y,
            button: Some(Button::Right),
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: p42.x,
            y: p42.y,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );

    let got: Vec<String> = wall.sent_mouse.iter().map(summary_cell).collect();
    assert_eq!(
        got,
        vec![
            "press left 1,1",
            "press right 4,2",
            "release right 4,2",
            "release left 4,2",
        ]
    );
}

#[test]
fn cancelled_program_gesture_releases_held_buttons_where_last_heard() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Any, Some("t"));
    let p67 = mouse_at(6, 7, 0.5, 0.5);
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p67.x,
            y: p67.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_cancel(
        &mut wall,
        &PointerEvent {
            x: p67.x,
            y: p67.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    let got: Vec<String> = wall.sent_mouse.iter().map(summary_cell).collect();
    assert_eq!(got, vec!["press left 6,7", "release left 6,7"]);
    assert_eq!(pointer.active_gesture(), None);
}

#[test]
fn any_event_tracking_hears_hover_once_per_cell_while_button_tracking_does_not() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Any, Some("t"));
    let hover = |wall: &mut StandInWall, pointer: &mut WallPointer, pt: Point, mods: Mods| {
        pointer.on_move(
            wall,
            &PointerEvent {
                x: pt.x,
                y: pt.y,
                button: None,
                buttons: Buttons::NONE,
                mods,
                ..PointerEvent::default()
            },
        );
    };

    hover(&mut wall, &mut pointer, mouse_at(2, 2, 0.5, 0.5), Mods::default());
    hover(&mut wall, &mut pointer, mouse_at(2, 2, 0.9, 0.5), Mods::default());
    hover(&mut wall, &mut pointer, mouse_at(3, 2, 0.5, 0.5), Mods::default());
    hover(&mut wall, &mut pointer, Point::new(2000.0, 2000.0), Mods::default());
    hover(&mut wall, &mut pointer, mouse_at(3, 2, 0.5, 0.5), Mods::default());
    hover(
        &mut wall,
        &mut pointer,
        mouse_at(4, 2, 0.5, 0.5),
        Mods {
            shift: true,
            ..Mods::default()
        },
    );

    let got: Vec<String> = wall.sent_mouse.iter().map(summary_cell).collect();
    assert_eq!(
        got,
        vec!["move none 2,2", "move none 3,2", "move none 3,2"]
    );
    assert_ne!(wall.cursor, CursorIcon::Default);

    hover(&mut wall, &mut pointer, mouse_at(5, 2, 0.5, 0.5), Mods::default());
    assert_eq!(wall.cursor, CursorIcon::Default);

    let (mut btn_wall, mut btn_pointer) = mouse_stand_in(MouseTracking::Button, Some("t"));
    hover(
        &mut btn_wall,
        &mut btn_pointer,
        mouse_at(2, 2, 0.5, 0.5),
        Mods::default(),
    );
    assert!(btn_wall.sent_mouse.is_empty());
}

#[test]
fn unfocused_click_focuses_and_sends_nothing_while_shift_alt_and_title_stay_walls() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Any, None);
    let p11 = mouse_at(1, 1, 0.5, 0.5);
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );
    assert_eq!(wall.focus_calls, vec!["t"]);

    // Now focused: hover is reported.
    let p99 = mouse_at(9, 9, 0.5, 0.5);
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: p99.x,
            y: p99.y,
            button: None,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.sent_mouse
            .iter()
            .map(|e| e.action.as_str())
            .collect::<Vec<_>>(),
        vec!["move"]
    );
    wall.sent_mouse.clear();

    let shift = Mods {
        shift: true,
        ..Mods::default()
    };
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::LEFT,
            mods: shift,
            ..PointerEvent::default()
        },
    );
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Shift));
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::NONE,
            mods: shift,
            ..PointerEvent::default()
        },
    );

    let alt = Mods {
        alt: true,
        ..Mods::default()
    };
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::LEFT,
            mods: alt,
            ..PointerEvent::default()
        },
    );
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Press));
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: p11.x,
            y: p11.y,
            buttons: Buttons::NONE,
            mods: alt,
            ..PointerEvent::default()
        },
    );

    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: 50.0,
            y: -20.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Press));
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: 50.0,
            y: -20.0,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );
    assert!(wall.sent_mouse.is_empty());
}

#[test]
fn double_click_on_program_body_belongs_to_program_and_wheel_scroll_carries_cell() {
    let (mut wall, mut pointer) = mouse_stand_in(MouseTracking::Normal, Some("t"));
    let p22 = mouse_at(2, 2, 0.5, 0.5);
    pointer.on_double_click(
        &mut wall,
        &PointerEvent {
            x: p22.x,
            y: p22.y,
            ..PointerEvent::default()
        },
    );
    assert!(wall.focus_calls.is_empty());

    // Disable glide so -60 px at row_px = 2 * 10 = 20 emits all +3? Wait: in mouse_stand_in
    // wall-mouse.test.mjs had no cellStride on `grid`, so `rowPx` was `null` -> 30 px/line -> +2!
    // Verify fractional cell coordinates (7.5, 3.5) with `glide_scroll = false`.
    wall.config.glide_scroll = false;
    let p73 = mouse_at(7, 3, 0.5, 0.5);
    pointer.on_wheel(
        &mut wall,
        &WheelEvent {
            x: p73.x,
            y: p73.y,
            delta_x: 0.0,
            delta_y: -40.0,
            delta_mode: DeltaMode::Pixel,
            mods: Mods::default(),
            time_ms: 100.0,
        },
    );
    assert_eq!(
        wall.commands,
        vec![vec![
            "terminal.scroll".to_owned(),
            "t".to_owned(),
            "2".to_owned(),
            "7.5".to_owned(),
            "3.5".to_owned(),
        ]]
    );
}

// ── 5. Split divider, outer split resize, split pane rearrange & map label ──

#[test]
fn split_divider_outer_border_pane_rearrange_and_map_label_gestures() {
    let mut wall = StandInWall::new(4.0);
    let mut term = MockTerminal::new(40, 20, Point::new(1.0, 2.0));
    term.edge_tol = 0.5;
    wall.add_terminal("p1", term);
    wall.groups
        .insert("split-1".to_owned(), vec!["p1".to_owned()]);
    wall.split_members
        .insert("p1".to_owned(), "split-1".to_owned());
    wall.split_rects
        .insert("split-1".to_owned(), Rect::new(0.0, -40.0, 80.0, 0.0));
    let mut pointer = WallPointer::new();

    // 1) Divider drag moves along its axis once past DRAG_SLOP_PX.
    wall.forced_hit = Some(CanvasHit::Divider {
        group: "split-1".to_owned(),
        path: vec![BranchSide::A],
        axis: Axis::X,
    });
    pointer.on_down(&mut wall, &PointerEvent::default());
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 2.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert!(wall.divider_drags.is_empty());
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 40.0,
            y: 0.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.divider_drags,
        vec![("split-1".to_owned(), vec![BranchSide::A], 10.0)]
    );
    pointer.on_up(&mut wall, &PointerEvent::default());

    // 2) Outer split border drag snaps dragged edge to grid (and Ctrl inverts).
    let right_edge = edges_at(Rect::new(0.0, -40.0, 80.0, 0.0), 80.0, -10.0, 0.5).unwrap();
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "p1".to_owned(),
        part: HitPart::Edge(right_edge),
    });
    pointer.on_down(
        &mut wall,
        &PointerEvent {
            x: 320.0,
            y: 40.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(pointer.active_gesture(), Some(GestureKind::SplitResize));
    // +13 px at zoom 4 = +3.25 world -> 83.25 snaps to 84.0.
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 333.0,
            y: 40.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.split_resizes.last(),
        Some(&("split-1".to_owned(), Rect::new(0.0, -40.0, 84.0, 0.0)))
    );
    pointer.on_up(&mut wall, &PointerEvent::default());

    // 3) Split pane title-bar drag updates drop_preview and commits drop_pane on release.
    wall.forced_hit = Some(CanvasHit::Terminal {
        id: "p1".to_owned(),
        part: HitPart::Title,
    });
    pointer.on_down(&mut wall, &PointerEvent::default());
    assert_eq!(pointer.active_gesture(), Some(GestureKind::Pane));
    pointer.on_move(
        &mut wall,
        &PointerEvent {
            x: 20.0,
            y: -12.0,
            buttons: Buttons::LEFT,
            ..PointerEvent::default()
        },
    );
    assert!(pointer.drop_preview().is_some());
    pointer.on_up(
        &mut wall,
        &PointerEvent {
            x: 20.0,
            y: -12.0,
            buttons: Buttons::NONE,
            ..PointerEvent::default()
        },
    );
    assert_eq!(
        wall.pane_drops,
        vec![("p1".to_owned(), Point::new(5.0, 3.0))]
    );

    // 4) Map label click focuses AND frames; map label drag moves the terminal.
    wall.forced_hit = None;
    wall.focus_calls.clear();
    pointer.press_label(&mut wall, "p1", &PointerEvent::default());
    pointer.on_up(&mut wall, &PointerEvent::default());
    assert_eq!(wall.focus_calls, vec!["p1 framed"]);

    let _min: MinGridSize = MIN_SIZE;
}
