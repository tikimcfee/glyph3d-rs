//! End-to-end scenario tests driving `wall_core::wall::Wall` through its plain-data
//! input methods against a fake daemon/window backend that applies `WallEffect`s.
//!
//! Follows the non-pixel scenarios of the JS gate (`tools/wall-e2e.mjs`) plus the
//! full interaction contract for groups, splits, fullscreen, text selection, program
//! mouse reporting, and restart persistence through `WallStore`.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use term_cells::{Modes, MouseTracking as TermMouseTracking, Screen};
use wall_core::config::GroupLayout;
use wall_core::dock::DockSide;
use wall_core::geom::{Point, Rect};
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::keymap::{KeyDecision, TerminalKey};
use wall_core::layout::FULLSCREEN_Z;
use wall_core::menu::Verdict;
use wall_core::model::{CellGeometry, WallEffect};
use wall_core::pointer::{
    Buttons, CursorIcon, MouseAction, PointerEvent, ProgramMouseButton, TerminalMouseEvent,
    WheelEvent,
};
use wall_core::scroll::DeltaMode;
use wall_core::selection::screen_from_lines;
use wall_core::snap::{snap_step, snap_to, GridSpec};
use wall_core::store::{FullscreenMode, Storage, WallStore};
use wall_core::wall::{TerminalAppeared, Wall, WallFrame, WallOptions};

const MODS_NONE: Mods = Mods {
    shift: false,
    ctrl: false,
    alt: false,
    meta: false,
};
const MODS_SHIFT: Mods = Mods {
    shift: true,
    ..MODS_NONE
};
const MODS_CTRL: Mods = Mods {
    ctrl: true,
    ..MODS_NONE
};
const MODS_META: Mods = Mods {
    meta: true,
    ..MODS_NONE
};
const MODS_META_SHIFT: Mods = Mods {
    meta: true,
    shift: true,
    ..MODS_NONE
};

/// Shared in-memory [`Storage`] so restart tests can drop a [`Wall`] and build a
/// fresh one over the exact JSON persisted by the first instance.
#[derive(Clone, Default)]
struct SharedStorage(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl Storage for SharedStorage {
    fn load(&self, scope: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().expect("storage lock").get(scope).cloned())
    }

    fn save(&self, scope: &str, bytes: &[u8]) -> io::Result<()> {
        self.0
            .lock()
            .expect("storage lock")
            .insert(scope.to_owned(), bytes.to_vec());
        Ok(())
    }
}

/// Fake daemon + window + OS clipboard harness applying every `WallEffect` back
/// into `Wall`.
struct Harness {
    wall: Wall,
    screens: HashMap<String, Screen>,
    next_id: usize,
    now_ms: f64,
    clipboard: String,
    keys_sent: Vec<(String, TerminalKey)>,
    pasted: Vec<(String, String)>,
    mouse_events: Vec<(String, TerminalMouseEvent)>,
    scrolls: Vec<(String, i32)>,
    resizes: Vec<(String, u32, u32)>,
    kills: Vec<String>,
    cursors: Vec<CursorIcon>,
    knobs_changed: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        Self::with_platform(Platform::Mac)
    }

    fn with_platform(platform: Platform) -> Self {
        let opts = WallOptions {
            platform,
            geometry: CellGeometry {
                stride: Point::new(0.6, 1.2),
                padding: Point::new(0.4, 0.4),
                scale_z: 1.0,
            },
            ..Default::default()
        };
        Self::with_storage(SharedStorage::default(), "test", opts)
    }

    fn with_storage(storage: SharedStorage, scope: &str, opts: WallOptions) -> Self {
        let store = WallStore::with_storage(Box::new(storage), scope);
        Self {
            wall: Wall::new(store, opts),
            screens: HashMap::new(),
            next_id: 1,
            now_ms: 1_000.0,
            clipboard: String::new(),
            keys_sent: Vec::new(),
            pasted: Vec::new(),
            mouse_events: Vec::new(),
            scrolls: Vec::new(),
            resizes: Vec::new(),
            kills: Vec::new(),
            cursors: Vec::new(),
            knobs_changed: Vec::new(),
        }
    }

    /// Apply every pending `WallEffect` to the fake backend until the outbox settles.
    fn pump(&mut self) {
        for _ in 0..16 {
            let effects = self.wall.drain_effects();
            if effects.is_empty() {
                break;
            }
            for eff in effects {
                match eff {
                    WallEffect::SpawnTerminal { cols, rows, .. } => {
                        let id = format!("t{}", self.next_id);
                        self.next_id += 1;
                        self.screens
                            .insert(id.clone(), Screen::new(cols as u16, rows as u16));
                        self.wall
                            .terminal_appeared(TerminalAppeared::new(id, cols, rows));
                    }
                    WallEffect::ResizeTerminal { id, cols, rows } => {
                        self.resizes.push((id.clone(), cols, rows));
                        if let Some(screen) = self.screens.get_mut(&id) {
                            screen.resize(cols as u16, rows as u16);
                        }
                        self.wall.terminal_resized(&id, cols, rows);
                    }
                    WallEffect::KillTerminal { id } => {
                        self.kills.push(id.clone());
                        self.screens.remove(&id);
                        self.wall.terminal_ended(&id);
                    }
                    WallEffect::SendKey { id, key } => {
                        self.keys_sent.push((id, key));
                    }
                    WallEffect::PasteTerminal { id, text } => {
                        self.pasted.push((id, text));
                    }
                    WallEffect::SendMouse { id, event } => {
                        self.mouse_events.push((id, event));
                    }
                    WallEffect::ScrollTerminal { id, lines, .. } => {
                        self.scrolls.push((id, lines));
                    }
                    WallEffect::WriteClipboard { text } => {
                        self.clipboard = text;
                    }
                    WallEffect::ReadClipboard { target } => {
                        let text = self.clipboard.clone();
                        self.wall.deliver_clipboard(&target, &text);
                    }
                    WallEffect::SetCursor { cursor } => {
                        self.cursors.push(cursor);
                    }
                    WallEffect::ConfigKnobChanged { key, .. } => {
                        self.knobs_changed.push(key);
                    }
                    WallEffect::RefreshTerminal { .. } | WallEffect::Redraw => {}
                }
            }
        }
    }

    fn run_ok(&mut self, cmd: &str) {
        let reply = self.wall.run(cmd);
        assert!(!reply.is_err(), "{cmd:?} failed: {}", reply.text);
        self.pump();
    }

    /// Spawn one terminal via `wall.spawn`, settle its camera animation, and return its id.
    fn spawn(&mut self) -> String {
        let before = self.next_id;
        self.run_ok("wall.spawn");
        self.settle();
        format!("t{before}")
    }

    /// Step the clock in 16 ms increments until all camera/scroll/fullscreen animations finish.
    fn settle(&mut self) {
        for _ in 0..240 {
            self.now_ms += 16.0;
            let animating = self.wall.tick(0.016, self.now_ms);
            self.pump();
            if !animating {
                break;
            }
        }
    }

    fn frame(&mut self) -> WallFrame {
        self.wall.frame()
    }

    fn screen_pt(&self, world: Point) -> Point {
        self.wall.camera().to_screen(world)
    }

    fn ptr(
        &mut self,
        screen: Point,
        button: Option<Button>,
        buttons: Buttons,
        mods: Mods,
    ) -> PointerEvent {
        self.now_ms += 10.0;
        PointerEvent {
            x: screen.x,
            y: screen.y,
            button,
            buttons,
            mods,
            pointer_id: 1,
            time_ms: self.now_ms,
        }
    }

    fn click_world(&mut self, world: Point, mods: Mods) {
        let s = self.screen_pt(world);
        let down = self.ptr(s, Some(Button::Left), Buttons::LEFT, mods);
        self.wall.pointer_down(&down, &self.screens);
        let up = self.ptr(s, Some(Button::Left), Buttons::NONE, mods);
        self.wall.pointer_up(&up, &self.screens);
        self.pump();
    }

    fn drag_world_with_move_mods(&mut self, from: Point, to: Point, down_mods: Mods, move_mods: Mods) {
        let s0 = self.screen_pt(from);
        let s1 = self.screen_pt(to);
        let mid = Point::new((s0.x + s1.x) * 0.5, (s0.y + s1.y) * 0.5);
        let down = self.ptr(s0, Some(Button::Left), Buttons::LEFT, down_mods);
        self.wall.pointer_down(&down, &self.screens);
        let mv1 = self.ptr(mid, None, Buttons::LEFT, move_mods);
        self.wall.pointer_move(&mv1, &self.screens);
        let mv2 = self.ptr(s1, None, Buttons::LEFT, move_mods);
        self.wall.pointer_move(&mv2, &self.screens);
        let up = self.ptr(s1, Some(Button::Left), Buttons::NONE, move_mods);
        self.wall.pointer_up(&up, &self.screens);
        self.pump();
    }

    fn drag_world(&mut self, from: Point, to: Point, mods: Mods) {
        self.drag_world_with_move_mods(from, to, mods, mods);
    }

    fn key_press(&mut self, key: &str, code: &str, mods: Mods) -> KeyDecision {
        let text = if key.chars().count() == 1 && !mods.ctrl && !mods.meta && !mods.alt {
            Some(key.to_owned())
        } else {
            None
        };
        let ev = KeyEvent {
            code: code.to_owned(),
            key: key.to_owned(),
            text,
            mods,
            repeat: false,
            pressed: true,
        };
        let dec = self.wall.key(&ev, &self.screens);
        self.pump();
        dec
    }

    fn key_release(&mut self, key: &str, code: &str, mods: Mods) -> KeyDecision {
        let ev = KeyEvent {
            code: code.to_owned(),
            key: key.to_owned(),
            text: None,
            mods,
            repeat: false,
            pressed: false,
        };
        let dec = self.wall.key(&ev, &self.screens);
        self.pump();
        dec
    }

    fn set_screen_lines(&mut self, id: &str, lines: &[&str]) {
        let (cols, rows) = {
            let entry = self.wall.terminal(id).expect("terminal exists");
            (entry.cols as u16, entry.rows as usize)
        };
        let mut padded: Vec<&str> = lines.to_vec();
        padded.resize(rows, "");
        let screen = screen_from_lines(&padded, cols, &[], |_| false);
        self.screens.insert(id.to_owned(), screen);
    }

    /// World point at fractional cell coordinates `(col_f, row_f)` inside terminal `id`.
    fn cell_point(&self, id: &str, col_f: f64, row_f: f64) -> Point {
        let entry = self.wall.terminal(id).expect("terminal exists");
        let geom = self.wall.model().geometry();
        let origin = entry.cell_origin(geom);
        let sx = geom.stride.x * entry.scale;
        let sy = geom.stride.y * entry.scale;
        Point::new(origin.x + col_f * sx, origin.y - row_f * sy)
    }

    /// World center of cell `(col, row)` inside terminal `id`.
    fn cell_world(&self, id: &str, col: usize, row: usize) -> Point {
        self.cell_point(id, col as f64 + 0.5, row as f64 + 0.5)
    }

    fn grid_spec(&self) -> GridSpec {
        let cfg = self.wall.config();
        GridSpec::new(
            cfg.grid_minor,
            cfg.grid_major_every.max(1.0) as u32,
            cfg.grid_min_px,
        )
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.min_x < b.max_x - 1e-6
        && a.max_x > b.min_x + 1e-6
        && a.min_y < b.max_y - 1e-6
        && a.max_y > b.min_y + 1e-6
}

fn rect_center(r: Rect) -> Point {
    Point::new((r.min_x + r.max_x) * 0.5, (r.min_y + r.max_y) * 0.5)
}

// ── Spine scenarios ──────────────────────────────────────────────────────────

#[test]
fn spawn_three_terminals_flow_into_place() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    let t3 = h.spawn();

    assert_eq!(h.wall.focused_id(), Some(t3.as_str()));

    let frame = h.frame();
    assert_eq!(frame.terminals.len(), 3);
    assert_eq!(frame.title_bars.len(), 3);
    assert!(frame.paper.grid_show);
    assert!(frame.paper.levels.minor || frame.paper.levels.major);

    // Every pair of spawned terminals flows into non-overlapping rectangles.
    let rects: Vec<Rect> = [t1, t2, t3]
        .iter()
        .map(|id| h.wall.terminal_rect(id).expect("rect"))
        .collect();
    for i in 0..rects.len() {
        for j in (i + 1)..rects.len() {
            assert!(
                !rects_overlap(rects[i], rects[j]),
                "terminals {i} and {j} overlapped: {:?} vs {:?}",
                rects[i],
                rects[j]
            );
        }
    }
    // Second terminal flows to the right of the first.
    assert!(rects[1].min_x > rects[0].max_x);
}

#[test]
fn click_focuses_and_raises() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    let t3 = h.spawn();
    assert_eq!(h.wall.focused_id(), Some(t3.as_str()));

    // Fit all so t1, t2, t3 are comfortably inside the viewport.
    h.run_ok("wall.fit");
    h.settle();

    let r1 = h.wall.terminal_rect(&t1).expect("t1 rect");
    h.click_world(rect_center(r1), MODS_NONE);

    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    let frame = h.frame();
    let f1 = frame.terminals.iter().find(|t| t.id == t1).expect("f1");
    let f2 = frame.terminals.iter().find(|t| t.id == t2).expect("f2");
    let f3 = frame.terminals.iter().find(|t| t.id == t3).expect("f3");
    assert!(f1.focused && f1.border.focused);
    assert!(!f2.focused && !f2.border.focused);
    assert!(!f3.focused && !f3.border.focused);
    assert!(
        f1.z > f2.z && f1.z > f3.z,
        "clicked terminal should rise above siblings: z1={} z2={} z3={}",
        f1.z,
        f2.z,
        f3.z
    );
    // Terminals are returned sorted back-to-front by Z, so t1 is drawn last (on top).
    assert_eq!(
        frame.terminals.last().map(|t| t.id.as_str()),
        Some(t1.as_str())
    );
}

#[test]
fn title_bar_drag_moves_with_snapping_and_ctrl_inverts() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.run_ok("wall.fit");
    h.settle();

    // Ensure snap is on and inspect the snap step at current zoom.
    assert!(h.wall.config().snap);
    let zoom = h.wall.camera().zoom();
    let step = snap_step(h.grid_spec(), zoom);
    assert!(step > 0.0);

    // Drag the title bar by an intentionally off-grid vector.
    let frame0 = h.frame();
    let bar0 = frame0
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("title bar");
    let drag_from = rect_center(bar0.rect);
    let raw_delta = Point::new(step * 1.37, -step * 0.63);
    h.drag_world(
        drag_from,
        Point::new(drag_from.x + raw_delta.x, drag_from.y + raw_delta.y),
        MODS_NONE,
    );

    let after_snap = h.wall.terminal(&t1).expect("t1").position;
    assert!(
        (after_snap.x - snap_to(after_snap.x, step)).abs() < 1e-6,
        "snapped x={} should lie on grid step={step}",
        after_snap.x
    );
    assert!(
        (after_snap.y - snap_to(after_snap.y, step)).abs() < 1e-6,
        "snapped y={} should lie on grid step={step}",
        after_snap.y
    );

    // Drag with Ctrl held during the drag: inverts snap, moving by exact travel off-grid
    // (on macOS Ctrl at pointer-down is secondary click, so Ctrl is held once dragging).
    let frame1 = h.frame();
    let bar1 = frame1
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("title bar");
    let drag_from2 = rect_center(bar1.rect);
    let free_delta = Point::new(step * 0.37, -step * 0.29);
    h.drag_world_with_move_mods(
        drag_from2,
        Point::new(drag_from2.x + free_delta.x, drag_from2.y + free_delta.y),
        MODS_NONE,
        MODS_CTRL,
    );
    let after_ctrl = h.wall.terminal(&t1).expect("t1").position;
    assert!((after_ctrl.x - (after_snap.x + free_delta.x)).abs() < 1e-4);
    assert!((after_ctrl.y - (after_snap.y + free_delta.y)).abs() < 1e-4);
    assert!((after_ctrl.x - snap_to(after_ctrl.x, step)).abs() > 0.05 * step);

    // `wall.snap off` makes a plain drag free; `wall.snap on` snaps again.
    h.run_ok("wall.snap off");
    assert!(!h.wall.config().snap);
    let frame2 = h.frame();
    let bar2 = frame2
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("title bar");
    let drag_from3 = rect_center(bar2.rect);
    h.drag_world(
        drag_from3,
        Point::new(drag_from3.x + step * 0.23, drag_from3.y),
        MODS_NONE,
    );
    let after_free = h.wall.terminal(&t1).expect("t1").position;
    assert!((after_free.x - (after_ctrl.x + step * 0.23)).abs() < 1e-4);

    h.run_ok("wall.snap on");
    assert!(h.wall.config().snap);
}

#[test]
fn border_drag_resizes_in_whole_cells_and_requests_resize() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.run_ok("wall.snap off");
    h.run_ok("wall.fit");
    h.settle();

    let before = h.wall.terminal_rect(&t1).expect("rect before");
    let before_cols = h.wall.terminal(&t1).expect("entry").cols;
    let before_rows = h.wall.terminal(&t1).expect("entry").rows;
    let stride = h.wall.model().geometry().stride;

    h.resizes.clear();
    // Drag right border outward by +10 cells.
    let right_mid = Point::new(before.max_x, (before.min_y + before.max_y) * 0.5);
    h.drag_world(
        right_mid,
        Point::new(right_mid.x + stride.x * 10.0, right_mid.y),
        MODS_NONE,
    );

    assert!(
        !h.resizes.is_empty(),
        "border drag must emit WallEffect::ResizeTerminal"
    );
    let after_right = h.wall.terminal(&t1).expect("entry after right drag");
    assert_eq!(after_right.cols, before_cols + 10);
    assert_eq!(after_right.rows, before_rows);
    let rect_after_right = h.wall.terminal_rect(&t1).expect("rect after right");
    // Opposite (left) edge stayed anchored.
    assert!((rect_after_right.min_x - before.min_x).abs() < 1e-6);

    // Drag bottom border downward (-Y in world) by 6 rows.
    let bottom_mid = Point::new(
        (rect_after_right.min_x + rect_after_right.max_x) * 0.5,
        rect_after_right.min_y,
    );
    h.drag_world(
        bottom_mid,
        Point::new(bottom_mid.x, bottom_mid.y - stride.y * 6.0),
        MODS_NONE,
    );
    let after_bottom = h.wall.terminal(&t1).expect("entry after bottom drag");
    assert_eq!(after_bottom.rows, before_rows + 6);
    let rect_after_bottom = h.wall.terminal_rect(&t1).expect("rect after bottom");
    // Opposite (top) edge stayed anchored.
    assert!((rect_after_bottom.max_y - rect_after_right.max_y).abs() < 1e-6);
}

#[test]
fn wheel_pans_zooms_with_zoom_key_and_scrolls_focused_terminal() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.run_ok("wall.fit");
    h.settle();

    // Pick a screen point on empty canvas outside t1.
    let r1 = h.wall.terminal_rect(&t1).expect("t1 rect");
    let empty_world = Point::new(r1.min_x - 30.0, r1.max_y + 20.0);
    let empty_screen = h.screen_pt(empty_world);

    // 1. Plain wheel on canvas pans camera (zoom unchanged).
    let pose0 = h.wall.camera().pose();
    h.now_ms += 200.0;
    h.wall.wheel(&WheelEvent {
        x: empty_screen.x,
        y: empty_screen.y,
        delta_x: 120.0,
        delta_y: 80.0,
        delta_mode: DeltaMode::Pixel,
        mods: MODS_NONE,
        time_ms: h.now_ms,
    });
    h.pump();
    let pose_pan = h.wall.camera().pose();
    assert!((pose_pan.zoom - pose0.zoom).abs() < 1e-9);
    assert!(
        (pose_pan.x - pose0.x).abs() > 1.0 && (pose_pan.y - pose0.y).abs() > 1.0,
        "canvas wheel should pan camera: before={pose0:?} after={pose_pan:?}"
    );

    // 2. Wheel with zoom key (Ctrl) zooms about pointer holding anchor world point fixed.
    let anchor_screen = Point::new(420.0, 310.0);
    let anchor_world_before = h.wall.camera().to_world(anchor_screen);
    let zoom_before = h.wall.camera().zoom();
    h.now_ms += 400.0;
    h.wall.wheel(&WheelEvent {
        x: anchor_screen.x,
        y: anchor_screen.y,
        delta_x: 0.0,
        delta_y: -180.0,
        delta_mode: DeltaMode::Pixel,
        mods: MODS_CTRL,
        time_ms: h.now_ms,
    });
    h.settle();
    let zoom_after = h.wall.camera().zoom();
    assert!(
        zoom_after > zoom_before * 1.1,
        "Ctrl+wheel up should zoom in: {zoom_before} -> {zoom_after}"
    );
    let anchor_world_after = h.wall.camera().to_world(anchor_screen);
    assert!(
        (anchor_world_after.x - anchor_world_before.x).abs() < 0.05
            && (anchor_world_after.y - anchor_world_before.y).abs() < 0.05,
        "zoom must hold world anchor fixed: before={anchor_world_before:?} after={anchor_world_after:?}"
    );

    // 3. Pinch zooms about pointer holding anchor fixed.
    let pinch_screen = Point::new(600.0, 400.0);
    let pinch_world_before = h.wall.camera().to_world(pinch_screen);
    let pinch_zoom_before = h.wall.camera().zoom();
    h.wall.pinch(pinch_screen.x, pinch_screen.y, 1.25);
    h.pump();
    assert!((h.wall.camera().zoom() - pinch_zoom_before * 1.25).abs() < 1e-6);
    let pinch_world_after = h.wall.camera().to_world(pinch_screen);
    assert!((pinch_world_after.x - pinch_world_before.x).abs() < 1e-6);
    assert!((pinch_world_after.y - pinch_world_before.y).abs() < 1e-6);

    // 4. Plain wheel over the focused terminal scrolls the terminal while camera holds still.
    h.run_ok("wall.fit");
    h.settle();
    let term_screen = h.screen_pt(rect_center(h.wall.terminal_rect(&t1).expect("t1")));
    let cam_before_scroll = h.wall.camera().pose();
    h.scrolls.clear();
    h.now_ms += 400.0;
    h.wall.wheel(&WheelEvent {
        x: term_screen.x,
        y: term_screen.y,
        delta_x: 0.0,
        delta_y: -120.0,
        delta_mode: DeltaMode::Pixel,
        mods: MODS_NONE,
        time_ms: h.now_ms,
    });
    h.settle();
    assert_eq!(h.wall.camera().pose(), cam_before_scroll);
    assert!(
        !h.scrolls.is_empty() && h.scrolls.iter().all(|(id, _)| id == &t1),
        "wheel over focused terminal must emit ScrollTerminal for {t1}, got {:?}",
        h.scrolls
    );
}

#[test]
fn keys_and_verbs_drive_wall_and_forward_to_focused_terminal() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // Plain typing reaches focused terminal through SendKey.
    h.keys_sent.clear();
    let dec = h.key_press("a", "KeyA", MODS_NONE);
    assert!(matches!(dec, KeyDecision::TerminalKey { .. }));
    assert_eq!(h.keys_sent.len(), 1);
    assert_eq!(h.keys_sent[0].0, t2);
    assert_eq!(h.keys_sent[0].1.key, "a");

    // Cmd+K toggles command palette; Escape closes it.
    assert!(!h.frame().palette_open);
    h.key_press("k", "KeyK", MODS_META);
    assert!(h.frame().palette_open);
    h.key_press("Escape", "Escape", MODS_NONE);
    assert!(!h.frame().palette_open);

    // Cmd+B toggles sidebar panel in WallFrame.
    let open_before = h.frame().panels.sidebar.open;
    h.key_press("b", "KeyB", MODS_META);
    assert_eq!(h.frame().panels.sidebar.open, !open_before);

    // Cmd+] cycles focus between t2 and t1 without sending keys to any terminal.
    let keys_before = h.keys_sent.len();
    h.key_press("]", "BracketRight", MODS_META);
    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    assert_eq!(h.keys_sent.len(), keys_before);

    // Verbs: rename, move, config knob, grid toggle, docked panels.
    h.run_ok(&format!("wall.rename {t1} build-shell"));
    assert_eq!(h.wall.name_of(&t1), "build-shell");

    h.run_ok(&format!("terminal.move {t1} 12.5 -7.5"));
    assert_eq!(
        h.wall.terminal(&t1).expect("t1").position,
        Point::new(12.5, -7.5)
    );

    h.knobs_changed.clear();
    h.run_ok("wall.grid off");
    assert!(!h.frame().paper.grid_show);
    assert!(h.knobs_changed.iter().any(|k| k == "gridShow"));

    h.run_ok("wall.sidebar left");
    h.run_ok("wall.status left");
    let frame = h.frame();
    assert!(frame.panels.sidebar.open && frame.panels.sidebar.side == DockSide::Left);
    assert!(frame.panels.status_window.open && frame.panels.status_window.side == DockSide::Left);
    assert!(frame.panels.dock.inset.left > 0.0);
    assert!(
        frame
            .title_bars
            .iter()
            .any(|b| b.terminal_id == t1 && b.text == "build-shell"),
        "renamed title bar must appear in WallFrame"
    );
}

#[test]
fn zoom_out_hands_off_to_map_labels_and_press_label_frames() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // Zoom way out so title-bar text drops below the handoff threshold.
    h.run_ok("wall.fit");
    h.settle();
    h.wall.pinch(700.0, 450.0, 0.05);
    h.pump();

    let frame = h.frame();
    assert_eq!(
        frame.map_labels.len(),
        2,
        "zoomed-out terminals should hand off to screen-space map labels"
    );
    assert!(
        frame.title_bars.iter().all(|b| !b.text_visible),
        "title bar text should be hidden once handed off to map labels"
    );

    // Pressing t1's map label focuses t1 and eases the camera to frame it.
    let lbl = frame
        .map_labels
        .iter()
        .find(|m| m.terminal_id == t1)
        .expect("t1 map label");
    let click_pt = Point::new(
        (lbl.label_box.left + lbl.label_box.right) * 0.5,
        (lbl.label_box.top + lbl.label_box.bottom) * 0.5,
    );
    let down = h.ptr(click_pt, Some(Button::Left), Buttons::LEFT, MODS_NONE);
    h.wall.press_label(&t1, &down);
    let up = h.ptr(click_pt, Some(Button::Left), Buttons::NONE, MODS_NONE);
    h.wall.pointer_up(&up, &h.screens);
    h.settle();

    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    assert!(
        h.wall.camera().zoom() > 5.0,
        "clicking map label should zoom back in to frame {t1}, zoom={}",
        h.wall.camera().zoom()
    );
}

// ── Structure: lasso, groups, splits, dividers, pane rearrange ───────────────

#[test]
fn lasso_selects_and_shift_click_toggles() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    let t3 = h.spawn();
    h.run_ok("wall.fit");
    h.settle();

    let r1 = h.wall.window_rect(&t1).expect("r1");
    let r2 = h.wall.window_rect(&t2).expect("r2");
    let r3 = h.wall.window_rect(&t3).expect("r3");

    // Shift+drag around t1 and t2, leaving t3 outside.
    let pad = 4.0;
    let corners = [
        Point::new(r1.min_x - pad, r1.max_y + pad),
        Point::new(r2.max_x + pad, r2.max_y + pad),
        Point::new(r2.max_x + pad, r2.min_y.min(r1.min_y) - pad),
        Point::new(r1.min_x - pad, r2.min_y.min(r1.min_y) - pad),
    ];
    let s0 = h.screen_pt(corners[0]);
    let down = h.ptr(s0, Some(Button::Left), Buttons::LEFT, MODS_SHIFT);
    h.wall.pointer_down(&down, &h.screens);
    for c in &corners[1..] {
        let sc = h.screen_pt(*c);
        let mv = h.ptr(sc, None, Buttons::LEFT, MODS_SHIFT);
        h.wall.pointer_move(&mv, &h.screens);
    }

    // While Shift-dragging, the lasso polygon is live in world units on WallFrame.
    let mid_frame = h.frame();
    assert!(
        mid_frame.lasso.as_ref().is_some_and(|pts| pts.len() >= 3),
        "WallFrame.lasso must expose the live polygon while Shift-dragging"
    );

    let s_end = h.screen_pt(corners[3]);
    let up = h.ptr(s_end, Some(Button::Left), Buttons::NONE, MODS_SHIFT);
    h.wall.pointer_up(&up, &h.screens);
    h.pump();

    let after = h.frame();
    assert!(after.lasso.is_none());
    let mut selected: Vec<String> = after
        .selection_outlines
        .iter()
        .map(|o| o.terminal_id.clone())
        .collect();
    selected.sort();
    assert_eq!(selected, vec![t1.clone(), t2.clone()]);

    // Shift+click toggles t1 out, then t3 in.
    h.click_world(rect_center(r1), MODS_SHIFT);
    assert_eq!(h.wall.selection(), std::slice::from_ref(&t2));

    h.click_world(rect_center(r3), MODS_SHIFT);
    let mut sel2 = h.wall.selection().to_vec();
    sel2.sort();
    assert_eq!(sel2, vec![t2.clone(), t3.clone()]);

    // Plain click on empty canvas clears selection.
    h.click_world(Point::new(r1.min_x - 30.0, r1.max_y + 30.0), MODS_NONE);
    assert!(h.wall.selection().is_empty());
    assert!(h.frame().selection_outlines.is_empty());
}

#[test]
fn chord_g_groups_tiled_and_header_drag_moves_group() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    let t3 = h.spawn();
    h.run_ok("wall.snap off");
    h.run_ok("wall.fit");
    h.settle();

    // Select all three terminals and group them via Cmd+G.
    h.run_ok(&format!("wall.select {t1} {t2} {t3}"));
    h.key_press("g", "KeyG", MODS_META);
    h.settle();

    let frame0 = h.frame();
    assert_eq!(frame0.groups.len(), 1, "Cmd+G must create one group");
    let grp = &frame0.groups[0];
    assert_eq!(grp.layout, GroupLayout::Tile);
    let gid = grp.id.clone();

    // Tiled members sit without overlap inside the group's frame.
    let rects: Vec<Rect> = [&t1, &t2, &t3]
        .iter()
        .map(|id| h.wall.terminal_rect(id).expect("rect"))
        .collect();
    for i in 0..rects.len() {
        for j in (i + 1)..rects.len() {
            assert!(!rects_overlap(rects[i], rects[j]));
        }
    }

    // Drag the group header: every member shifts by the same delta.
    let before_pos: Vec<Point> = [&t1, &t2, &t3]
        .iter()
        .map(|id| h.wall.terminal(id).expect("t").position)
        .collect();
    let header_mid = rect_center(grp.header_rect);
    let shift = Point::new(18.0, -11.0);
    h.drag_world(
        header_mid,
        Point::new(header_mid.x + shift.x, header_mid.y + shift.y),
        MODS_NONE,
    );

    for (i, id) in [&t1, &t2, &t3].iter().enumerate() {
        let after = h.wall.terminal(id).expect("t").position;
        assert!(
            (after.x - (before_pos[i].x + shift.x)).abs() < 1e-4
                && (after.y - (before_pos[i].y + shift.y)).abs() < 1e-4,
            "member {id} did not move with group header: before={:?} after={:?}",
            before_pos[i],
            after
        );
    }

    // Switch layout to freeform via verb: dragging one member's title bar moves only that member.
    h.run_ok(&format!("wall.group.layout {gid} freeform"));
    let frame1 = h.frame();
    assert_eq!(frame1.groups[0].layout, GroupLayout::Freeform);
    let bar1 = frame1
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("t1 bar");
    let t1_before = h.wall.terminal(&t1).expect("t1").position;
    let t2_before = h.wall.terminal(&t2).expect("t2").position;
    let from = rect_center(bar1.rect);
    h.drag_world(from, Point::new(from.x + 12.0, from.y - 6.0), MODS_NONE);
    let t1_after = h.wall.terminal(&t1).expect("t1").position;
    let t2_after = h.wall.terminal(&t2).expect("t2").position;
    assert!((t1_after.x - (t1_before.x + 12.0)).abs() < 1e-4);
    assert!((t2_after.x - t2_before.x).abs() < 1e-6);
}

#[test]
fn chord_d_splits_and_new_terminal_lands_as_pane() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let orig_cols = h.wall.terminal(&t1).expect("t1").cols;
    let orig_rows = h.wall.terminal(&t1).expect("t1").rows;

    // Chord+D splits right: emits SpawnTerminal, fake backend spawns t2, t2 lands as pane.
    h.resizes.clear();
    h.key_press("d", "KeyD", MODS_META);
    h.settle();

    let t2 = "t2".to_owned();
    assert!(h.wall.terminal(&t2).is_some(), "Cmd+D must spawn t2");
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    let frame = h.frame();
    assert_eq!(frame.groups.len(), 1);
    assert_eq!(frame.groups[0].layout, GroupLayout::Split);
    assert!(
        !h.resizes.is_empty(),
        "splitting must emit ResizeTerminal for the split panes"
    );

    let r1 = h.wall.terminal_rect(&t1).expect("r1");
    let r2 = h.wall.terminal_rect(&t2).expect("r2");
    assert!(
        r2.min_x > r1.max_x,
        "right split should place t2 to the right of t1: r1={r1:?} r2={r2:?}"
    );
    assert!(
        h.wall.terminal(&t1).expect("t1").cols < orig_cols
            && h.wall.terminal(&t2).expect("t2").cols < orig_cols,
        "each horizontal pane should shrink below original width {orig_cols}"
    );

    // Split t2 downward via verb: t3 lands below t2 inside the same split group.
    h.run_ok("wall.split down");
    h.settle();
    let t3 = "t3".to_owned();
    assert!(h.wall.terminal(&t3).is_some());
    assert_eq!(h.frame().groups.len(), 1);
    let r2_after = h.wall.terminal_rect(&t2).expect("r2_after");
    let r3_after = h.wall.terminal_rect(&t3).expect("r3_after");
    assert!(
        r3_after.max_y < r2_after.min_y,
        "down split should place t3 below t2: r2={r2_after:?} r3={r3_after:?}"
    );
    assert!(h.wall.terminal(&t3).expect("t3").rows < orig_rows);
}

#[test]
fn divider_drag_resizes_split_panes() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.run_ok("wall.snap off");
    h.key_press("d", "KeyD", MODS_META);
    let t2 = "t2".to_owned();
    h.run_ok("wall.fit");
    h.settle();

    let r1_before = h.wall.terminal_rect(&t1).expect("r1");
    let r2_before = h.wall.terminal_rect(&t2).expect("r2");
    let cols1_before = h.wall.terminal(&t1).expect("t1").cols;
    let cols2_before = h.wall.terminal(&t2).expect("t2").cols;

    // Drag the vertical divider band between t1 and t2 to the right.
    let divider_pt = Point::new(
        (r1_before.max_x + r2_before.min_x) * 0.5,
        (r1_before.min_y + r1_before.max_y) * 0.5,
    );
    let delta_x = (r1_before.max_x - r1_before.min_x) * 0.25;
    h.resizes.clear();
    h.drag_world(
        divider_pt,
        Point::new(divider_pt.x + delta_x, divider_pt.y),
        MODS_NONE,
    );

    assert!(
        !h.resizes.is_empty(),
        "divider drag must emit ResizeTerminal"
    );
    let cols1_after = h.wall.terminal(&t1).expect("t1").cols;
    let cols2_after = h.wall.terminal(&t2).expect("t2").cols;
    assert!(
        cols1_after > cols1_before && cols2_after < cols2_before,
        "dragging divider right must widen t1 ({cols1_before}->{cols1_after}) and narrow t2 ({cols2_before}->{cols2_after})"
    );
}

#[test]
fn pane_swap_and_dock_by_title_bar_drag() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.key_press("d", "KeyD", MODS_META);
    let t2 = "t2".to_owned();
    h.run_ok("wall.fit");
    h.settle();

    let r1_init = h.wall.terminal_rect(&t1).expect("r1");
    let r2_init = h.wall.terminal_rect(&t2).expect("r2");
    assert!(r1_init.min_x < r2_init.min_x);

    // Drag t1's title bar over t2's center: shows drop preview, and releasing swaps panes.
    let frame0 = h.frame();
    let bar1 = frame0
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("t1 bar");
    let s_from = h.screen_pt(rect_center(bar1.rect));
    let s_center2 = h.screen_pt(rect_center(r2_init));
    let down = h.ptr(s_from, Some(Button::Left), Buttons::LEFT, MODS_NONE);
    h.wall.pointer_down(&down, &h.screens);
    let mv = h.ptr(s_center2, None, Buttons::LEFT, MODS_NONE);
    h.wall.pointer_move(&mv, &h.screens);

    let mid_frame = h.frame();
    assert!(
        mid_frame.drop_preview.is_some_and(|p| p.ok),
        "dragging pane over sibling center must produce an ok drop_preview"
    );

    let up = h.ptr(s_center2, Some(Button::Left), Buttons::NONE, MODS_NONE);
    h.wall.pointer_up(&up, &h.screens);
    h.pump();

    let r1_swapped = h.wall.terminal_rect(&t1).expect("r1 swapped");
    let r2_swapped = h.wall.terminal_rect(&t2).expect("r2 swapped");
    assert!(
        r2_swapped.min_x < r1_swapped.min_x,
        "drop onto sibling center must swap panes: r2={r2_swapped:?} r1={r1_swapped:?}"
    );
    assert!(h.frame().drop_preview.is_none());

    // Drag t1's title bar onto t2's bottom edge: docks t1 below t2.
    let frame1 = h.frame();
    let bar1_b = frame1
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("t1 bar");
    let bottom_zone = Point::new(
        (r2_swapped.min_x + r2_swapped.max_x) * 0.5,
        r2_swapped.min_y + (r2_swapped.max_y - r2_swapped.min_y) * 0.1,
    );
    h.drag_world(rect_center(bar1_b.rect), bottom_zone, MODS_NONE);

    let r1_docked = h.wall.terminal_rect(&t1).expect("r1 docked");
    let r2_docked = h.wall.terminal_rect(&t2).expect("r2 docked");
    assert!(
        r1_docked.max_y < r2_docked.min_y,
        "drop onto bottom edge must dock t1 below t2: r1={r1_docked:?} r2={r2_docked:?}"
    );

    // `wall.pane.move` verb swaps them vertically too.
    h.run_ok(&format!("wall.pane.move up {t1}"));
    let r1_up = h.wall.terminal_rect(&t1).expect("r1 up");
    let r2_up = h.wall.terminal_rect(&t2).expect("r2 up");
    assert!(r1_up.min_y > r2_up.max_y);
}

// ── Fullscreen (solo, group, pane zoom, text scale, refit, exit) ─────────────

#[test]
fn fullscreen_single_group_pane_zoom_text_scale_and_exit() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.run_ok("wall.fit");
    h.settle();

    let orig_cols = h.wall.terminal(&t1).expect("t1").cols;
    let orig_rows = h.wall.terminal(&t1).expect("t1").rows;
    let orig_pos = h.wall.terminal(&t1).expect("t1").position;
    let orig_cam = h.wall.camera().pose();

    // 1. Solo fullscreen via Cmd+Enter: elevates to FULLSCREEN_Z, shows backdrop, resizes to fill.
    h.key_press("Enter", "Enter", MODS_META);
    h.settle();

    assert!(h.wall.fullscreen().active());
    let fs_frame = h.frame();
    let backdrop = fs_frame
        .fullscreen_backdrop
        .as_ref()
        .expect("fullscreen backdrop");
    assert_eq!(backdrop.mode, FullscreenMode::Single);
    assert_eq!(backdrop.target, t1);
    let tf1 = fs_frame.terminals.iter().find(|t| t.id == t1).expect("tf1");
    assert!(tf1.fullscreen);
    assert!((tf1.z - FULLSCREEN_Z).abs() < 1e-6);
    assert!(
        fs_frame.title_bars.iter().all(|b| b.terminal_id != t1),
        "solo fullscreen hides its own title bar"
    );
    let fs_cols = tf1.cols;
    assert_ne!(fs_cols, orig_cols);

    // Text scale bigger/smaller/reset steps fullscreen cols & rows.
    h.run_ok("wall.fullscreen.text bigger");
    h.settle();
    let bigger_cols = h.wall.terminal(&t1).expect("t1").cols;
    assert!(
        bigger_cols < fs_cols,
        "bigger text should yield fewer columns: {fs_cols} -> {bigger_cols}"
    );
    h.run_ok("wall.fullscreen.text reset");
    h.settle();
    assert_eq!(h.wall.terminal(&t1).expect("t1").cols, fs_cols);

    // Docking a sidebar while in fullscreen refits the fullscreen area narrower.
    h.run_ok("wall.sidebar left");
    h.settle();
    let docked_cols = h.wall.terminal(&t1).expect("t1").cols;
    assert!(
        docked_cols < fs_cols,
        "docking sidebar during fullscreen should narrow terminal: {fs_cols} -> {docked_cols}"
    );
    h.run_ok("wall.sidebar close");
    h.settle();

    // Exit solo fullscreen via Cmd+Enter: restores original cols, rows, position, and camera.
    h.key_press("Enter", "Enter", MODS_META);
    h.settle();
    assert!(!h.wall.fullscreen().active());
    assert!(h.frame().fullscreen_backdrop.is_none());
    let restored = h.wall.terminal(&t1).expect("t1");
    assert_eq!(restored.cols, orig_cols);
    assert_eq!(restored.rows, orig_rows);
    assert!((restored.position.x - orig_pos.x).abs() < 1e-5);
    assert!((restored.position.y - orig_pos.y).abs() < 1e-5);
    assert!((h.wall.camera().pose().zoom - orig_cam.zoom).abs() < 1e-3);

    // 2. Group fullscreen on a 2-pane split, then Cmd+Shift+Enter zooms into one pane and back.
    h.key_press("d", "KeyD", MODS_META);
    let t2 = "t2".to_owned();
    h.settle();
    let gid = h.wall.groups().group_of(&t2).expect("split group");

    h.key_press("Enter", "Enter", MODS_META);
    h.settle();
    let gfs = h.frame();
    assert_eq!(
        gfs.fullscreen_backdrop.as_ref().map(|b| b.mode),
        Some(FullscreenMode::Group)
    );
    assert_eq!(
        gfs.fullscreen_backdrop.as_ref().map(|b| b.target.as_str()),
        Some(gid.as_str())
    );
    assert!(gfs.terminals.iter().all(|t| t.fullscreen));

    // Zoom into focused pane (t2) alone with Cmd+Shift+Enter.
    h.key_press("Enter", "Enter", MODS_META_SHIFT);
    h.settle();
    let pfs = h.frame();
    assert_eq!(
        pfs.fullscreen_backdrop.as_ref().map(|b| b.mode),
        Some(FullscreenMode::Single)
    );
    assert_eq!(
        pfs.fullscreen_backdrop.as_ref().map(|b| b.target.as_str()),
        Some(t2.as_str())
    );

    // Cmd+Shift+Enter again returns to the group fullscreen.
    h.key_press("Enter", "Enter", MODS_META_SHIFT);
    h.settle();
    assert_eq!(
        h.frame().fullscreen_backdrop.as_ref().map(|b| b.mode),
        Some(FullscreenMode::Group)
    );

    // Cmd+Enter exits fullscreen completely back to the canvas.
    h.key_press("Enter", "Enter", MODS_META);
    h.settle();
    assert!(!h.wall.fullscreen().active());
}

// ── Hide, show, close, and hold-to-close chord ───────────────────────────────

#[test]
fn hide_show_close_and_hold_close_chord() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // Hide t2: leaves WallFrame.terminals, focus falls back to t1.
    h.run_ok(&format!("wall.hide {t2}"));
    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    assert_eq!(h.frame().terminals.len(), 1);
    assert_eq!(h.frame().terminals[0].id, t1);

    // Show t2: returns to WallFrame.terminals.
    h.run_ok(&format!("wall.show {t2}"));
    assert_eq!(h.frame().terminals.len(), 2);

    // Short Cmd+X press cancelled before closeHoldMs keeps t1 alive.
    let hold_ms = h.wall.config().close_hold_ms;
    assert!(hold_ms > 50.0);
    h.kills.clear();
    h.key_press("x", "KeyX", MODS_META);
    h.now_ms += hold_ms * 0.4;
    let animating = h.wall.tick(0.016, h.now_ms);
    assert!(animating);
    let mid = h.frame();
    let progress = mid
        .terminals
        .iter()
        .find(|t| t.id == t1)
        .and_then(|t| t.close_hold_progress)
        .expect("close_hold_progress must be present while holding Cmd+X");
    assert!(progress > 0.1 && progress < 0.9);
    h.key_release("x", "KeyX", MODS_META);
    assert!(h.kills.is_empty());
    assert!(h.wall.terminal(&t1).is_some());
    assert!(
        h.frame()
            .terminals
            .iter()
            .find(|t| t.id == t1)
            .expect("t1")
            .close_hold_progress
            .is_none()
    );

    // Holding Cmd+X past closeHoldMs emits KillTerminal and removes t1.
    h.key_press("x", "KeyX", MODS_META);
    h.now_ms += hold_ms + 50.0;
    h.wall.tick(0.016, h.now_ms);
    h.pump();
    assert_eq!(h.kills, vec![t1.clone()]);
    assert!(h.wall.terminal(&t1).is_none());
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // `wall.close` closes t2 directly.
    h.run_ok(&format!("wall.close {t2}"));
    assert!(h.wall.terminal(&t2).is_none());
    assert!(h.frame().terminals.is_empty());
}

// ── Text selection, double/triple-click, copy & paste ────────────────────────

#[test]
fn text_selection_drag_double_triple_click_and_copy_paste() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.set_screen_lines(&t1, &["hello world foo bar", "second line here"]);
    h.run_ok("wall.fit");
    h.settle();

    // 1. Drag across "world" (row 0, cols 6..11).
    let start_w = h.cell_point(&t1, 6.1, 0.5);
    let end_w = h.cell_point(&t1, 10.9, 0.5);
    h.drag_world(start_w, end_w, MODS_NONE);

    assert_eq!(h.wall.selection_text(&t1, &h.screens), "world");
    let frame = h.frame();
    let tf = frame.terminals.iter().find(|t| t.id == t1).expect("tf");
    let sel = tf.text_selection.as_ref().expect("text_selection in frame");
    assert_eq!((sel.r0, sel.c0, sel.r1), (0, 6, 0));

    // copyOnSelect filled the selection clipboard: middle-click pastes it.
    h.pasted.clear();
    let mid_screen = h.screen_pt(h.cell_world(&t1, 2, 0));
    let mdown = h.ptr(mid_screen, Some(Button::Middle), Buttons::MIDDLE, MODS_NONE);
    h.wall.pointer_down(&mdown, &h.screens);
    let mup = h.ptr(mid_screen, Some(Button::Middle), Buttons::NONE, MODS_NONE);
    h.wall.pointer_up(&mup, &h.screens);
    h.pump();
    assert_eq!(h.pasted, vec![(t1.clone(), "world".to_owned())]);

    // 2. Double-click on "foo" (col 13, row 0) selects the word "foo".
    h.now_ms += 1_000.0; // reset click counter beyond clickRepeatMs
    let foo_pt = h.cell_world(&t1, 13, 0);
    h.click_world(foo_pt, MODS_NONE);
    h.click_world(foo_pt, MODS_NONE);
    assert_eq!(h.wall.selection_text(&t1, &h.screens), "foo");

    // 3. Triple-click on row 1 selects the full line "second line here".
    h.now_ms += 1_000.0;
    let line1_pt = h.cell_world(&t1, 3, 1);
    h.click_world(line1_pt, MODS_NONE);
    h.click_world(line1_pt, MODS_NONE);
    h.click_world(line1_pt, MODS_NONE);
    assert_eq!(h.wall.selection_text(&t1, &h.screens), "second line here");

    // 4. Copy chord (Cmd+C on Mac) emits WriteClipboard with exact selected text.
    h.clipboard.clear();
    h.key_press("c", "KeyC", MODS_META);
    assert_eq!(h.clipboard, "second line here");

    // 4b. `run_with_screens("wall.copy")` reads live updated screen text without caching Screen.
    h.set_screen_lines(&t1, &["hello world foo bar", "updated line now"]);
    h.clipboard.clear();
    let copy_reply = h.wall.run_with_screens("wall.copy", &h.screens);
    assert!(!copy_reply.is_err(), "wall.copy failed: {}", copy_reply.text);
    h.pump();
    assert_eq!(h.clipboard, "updated line now");

    // 5. `wall.paste` emits ReadClipboard -> fake backend delivers clipboard -> PasteTerminal.
    h.pasted.clear();
    h.run_ok("wall.paste");
    assert_eq!(
        h.pasted,
        vec![(t1.clone(), "updated line now".to_owned())]
    );

    // 6. Typing clears the text selection (selectionClearOnTyping default is true) and sends key.
    h.keys_sent.clear();
    h.key_press("z", "KeyZ", MODS_NONE);
    assert!(h.wall.selection_text(&t1, &h.screens).is_empty());
    assert!(
        h.frame()
            .terminals
            .iter()
            .find(|t| t.id == t1)
            .expect("t1")
            .text_selection
            .is_none()
    );
    assert_eq!(h.keys_sent.len(), 1);
    assert_eq!(h.keys_sent[0].1.key, "z");

    // 7. Ctrl+C with empty selection reaches the terminal (never hijacked by copy).
    h.keys_sent.clear();
    let dec = h.key_press("c", "KeyC", MODS_CTRL);
    assert!(matches!(dec, KeyDecision::TerminalKey { .. }));
    assert_eq!(h.keys_sent.len(), 1);
    assert!(h.keys_sent[0].1.mods.contains(&"Control"));
}

// ── Program mouse reporting & Shift bypass ───────────────────────────────────

#[test]
fn mouse_reporting_program_receives_press_drag_release_while_shift_selects() {
    let mut h = Harness::new();
    let t1 = h.spawn();
    let t2 = h.spawn();
    h.set_screen_lines(&t1, &["alpha beta gamma"]);
    h.run_ok("wall.fit");
    h.settle();

    // Enable button-event mouse tracking on t1 while t2 is focused.
    let modes = Modes {
        mouse: TermMouseTracking::Button,
        ..Default::default()
    };
    h.wall.terminal_modes(&t1, modes);
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // 1. Right-click on unfocused mouse-reporting t1 opens the wall's own menu.
    let s_t1 = h.screen_pt(h.cell_world(&t1, 2, 0));
    let dec_unfocused = h.wall.context_menu(s_t1.x, s_t1.y, MODS_NONE);
    assert_eq!(dec_unfocused.verdict, Verdict::Ours);
    assert!(h.frame().menu.is_some());

    // 2. Clicking unfocused t1 focuses it WITHOUT sending a mouse press to the program.
    h.mouse_events.clear();
    h.click_world(h.cell_world(&t1, 2, 0), MODS_NONE);
    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    assert!(
        h.mouse_events.is_empty(),
        "focusing click on unfocused mouse-reporting terminal must not forward mouse"
    );

    // 3. Right-click on now-focused mouse-reporting t1 body belongs to the program;
    //    Shift+right-click bypasses program capture and opens native/wall menu.
    let dec_focused = h.wall.context_menu(s_t1.x, s_t1.y, MODS_NONE);
    assert_eq!(dec_focused.verdict, Verdict::Program);

    // 4. Plain press + drag + release on focused t1 forwards Press, Move, Release to program.
    h.mouse_events.clear();
    let c0 = h.cell_world(&t1, 1, 0);
    let c1 = h.cell_world(&t1, 5, 0);
    h.drag_world(c0, c1, MODS_NONE);
    assert!(
        h.mouse_events.len() >= 3,
        "expected press + drag moves + release, got {:?}",
        h.mouse_events
    );
    assert_eq!(h.mouse_events[0].0, t1);
    assert_eq!(h.mouse_events[0].1.action, MouseAction::Press);
    assert_eq!(h.mouse_events[0].1.button, ProgramMouseButton::Left);
    assert!(
        h.mouse_events
            .iter()
            .any(|(_, ev)| ev.action == MouseAction::Move),
        "expected drag motion event in {:?}",
        h.mouse_events
    );
    assert_eq!(
        h.mouse_events.last().map(|(_, ev)| ev.action),
        Some(MouseAction::Release)
    );
    assert!(
        h.wall.selection_text(&t1, &h.screens).is_empty(),
        "program mouse drag must not select text"
    );

    // 5. Shift+drag bypasses program mouse and selects "beta" (cols 6..10).
    h.mouse_events.clear();
    h.drag_world(h.cell_point(&t1, 6.1, 0.5), h.cell_point(&t1, 9.9, 0.5), MODS_SHIFT);
    assert!(
        h.mouse_events.is_empty(),
        "Shift+drag must bypass program mouse, got {:?}",
        h.mouse_events
    );
    assert_eq!(h.wall.selection_text(&t1, &h.screens), "beta");
}

// ── Restart persistence through WallStore ────────────────────────────────────

#[test]
fn state_survives_restart_through_store() {
    let storage = SharedStorage::default();
    let opts = WallOptions {
        platform: Platform::Mac,
        geometry: CellGeometry {
            stride: Point::new(0.6, 1.2),
            padding: Point::new(0.4, 0.4),
            scale_z: 1.0,
        },
        ..Default::default()
    };

    let (t3_pos, gid, t1_size, t2_size, t3_size) = {
        let mut h = Harness::with_storage(storage.clone(), "restart-scope", opts.clone());
        let t1 = h.spawn();
        let t2 = h.spawn();
        let t3 = h.spawn();
        let t_dead = h.spawn();

        // Rename t1, move & hide t3, dial config knobs, dock sidebar left, split-group t1+t2.
        h.run_ok(&format!("wall.rename {t1} editor"));
        h.run_ok(&format!("terminal.move {t3} 250.0 -120.0"));
        h.run_ok(&format!("wall.hide {t3}"));
        h.run_ok("wall.grid off");
        h.run_ok("wall.snap off");
        h.run_ok("wall.sidebar left");
        h.run_ok(&format!("wall.group {t1} {t2}"));
        let gid = h.wall.groups().group_of(&t1).expect("group for t1");
        h.run_ok(&format!("wall.group.layout {gid} split"));
        h.run_ok(&format!("wall.group.rename {gid} work"));
        h.settle();

        // Focus t1 and enter group fullscreen so fullscreen persists across restart too.
        h.run_ok(&format!("terminal.focus {t1}"));
        h.key_press("Enter", "Enter", MODS_META);
        h.settle();
        assert!(h.wall.fullscreen().active());

        // Rename t_dead so it is definitely persisted in the store before disappearing.
        h.run_ok(&format!("wall.rename {t_dead} ephemeral"));

        let t3_pos = h.wall.terminal(&t3).expect("t3").position;
        let s1 = (
            h.wall.terminal(&t1).expect("t1").cols,
            h.wall.terminal(&t1).expect("t1").rows,
        );
        let s2 = (
            h.wall.terminal(&t2).expect("t2").cols,
            h.wall.terminal(&t2).expect("t2").rows,
        );
        let s3 = (
            h.wall.terminal(&t3).expect("t3").cols,
            h.wall.terminal(&t3).expect("t3").rows,
        );
        (t3_pos, gid, s1, s2, s3)
    };

    // Fresh Wall constructed over the SAME persisted storage.
    let mut h2 = Harness::with_storage(storage, "restart-scope", opts);

    // Reconcile daemon's live list: t4 (t_dead) is gone and must be pruned.
    let pruned = h2.wall.set_live_terminals(&["t1", "t2", "t3"]);
    assert!(
        pruned.iter().any(|id| id == "t4"),
        "stale terminal t4 should be pruned on connect, pruned={pruned:?}"
    );

    // Adopt each live terminal from the daemon.
    for (id, (cols, rows)) in [("t1", t1_size), ("t2", t2_size), ("t3", t3_size)] {
        h2.screens
            .insert(id.to_owned(), Screen::new(cols as u16, rows as u16));
        h2.wall
            .terminal_appeared(TerminalAppeared::adopted(id, cols, rows));
        h2.pump();
    }
    h2.settle();

    // Every persisted attribute came back intact.
    assert_eq!(h2.wall.name_of("t1"), "editor");
    assert!(!h2.wall.config().grid_show);
    assert!(!h2.wall.config().snap);
    assert!(h2.wall.store().hidden("t3"));
    assert!((h2.wall.terminal("t3").expect("t3").position.x - t3_pos.x).abs() < 1e-5);
    assert!((h2.wall.terminal("t3").expect("t3").position.y - t3_pos.y).abs() < 1e-5);

    let frame = h2.frame();
    assert!(frame.panels.sidebar.open && frame.panels.sidebar.side == DockSide::Left);
    assert_eq!(h2.wall.groups().group_of("t1").as_deref(), Some(gid.as_str()));
    assert_eq!(h2.wall.groups().group_of("t2").as_deref(), Some(gid.as_str()));
    assert_eq!(
        h2.wall.groups().group(&gid).map(|g| g.layout),
        Some(GroupLayout::Split)
    );
    assert_eq!(
        h2.wall.groups().name_of(&gid).ok().as_deref(),
        Some("work")
    );
    assert_eq!(
        frame.fullscreen_backdrop.as_ref().map(|b| b.mode),
        Some(FullscreenMode::Group),
        "saved group fullscreen must be adopted on restart"
    );
    assert_eq!(
        frame.fullscreen_backdrop.as_ref().map(|b| b.target.as_str()),
        Some(gid.as_str())
    );
    assert!(
        frame.terminals.iter().all(|t| t.id != "t3"),
        "hidden terminal t3 must remain hidden after restart"
    );

    // Exiting adopted group fullscreen returns cleanly to the canvas.
    h2.key_press("Enter", "Enter", MODS_META);
    h2.settle();
    assert!(!h2.wall.fullscreen().active());
    assert!(h2.frame().fullscreen_backdrop.is_none());
}

#[test]
fn double_click_frames_pane_focus_and_other_platform_chords() {
    // 1. Double-clicking a title bar frames that terminal; double-clicking empty canvas fits all;
    //    `wall.pane.focus` walks split neighbors.
    let mut h = Harness::new();
    let t1 = h.spawn();
    h.key_press("d", "KeyD", MODS_META);
    let t2 = "t2".to_owned();
    h.run_ok("wall.fit");
    h.settle();
    let fit_zoom = h.wall.camera().zoom();

    let frame0 = h.frame();
    let bar1 = frame0
        .title_bars
        .iter()
        .find(|b| b.terminal_id == t1)
        .expect("t1 bar");
    let bar_s = h.screen_pt(rect_center(bar1.rect));
    let dbl = h.ptr(bar_s, Some(Button::Left), Buttons::NONE, MODS_NONE);
    h.wall.pointer_double_click(&dbl);
    h.settle();
    assert!(
        h.wall.camera().zoom() > fit_zoom,
        "double-clicking title bar should zoom in to frame {t1}"
    );

    // `wall.pane right` moves focus from t1 to t2.
    h.run_ok(&format!("terminal.focus {t1}"));
    assert_eq!(h.wall.focused_id(), Some(t1.as_str()));
    h.run_ok("wall.pane right");
    assert_eq!(h.wall.focused_id(), Some(t2.as_str()));

    // Double-clicking empty canvas fits all again.
    let r1 = h.wall.terminal_rect(&t1).expect("r1");
    let empty_s = h.screen_pt(Point::new(r1.min_x - 40.0, r1.max_y + 40.0));
    let dbl_empty = h.ptr(empty_s, Some(Button::Left), Buttons::NONE, MODS_NONE);
    h.wall.pointer_double_click(&dbl_empty);
    h.settle();
    assert!((h.wall.camera().zoom() - fit_zoom).abs() < 0.2);

    // 2. Non-macOS platform (Ctrl+Alt wall chord, Ctrl+Shift+C/V copy & paste).
    let mut linux = Harness::with_platform(Platform::Other);
    let l1 = linux.spawn();
    linux.run_ok("wall.fit");
    linux.settle();

    let mods_ctrl_alt = Mods {
        ctrl: true,
        alt: true,
        ..MODS_NONE
    };
    let mods_ctrl_shift = Mods {
        ctrl: true,
        shift: true,
        ..MODS_NONE
    };

    // Ctrl+Alt+D splits on non-Mac.
    linux.key_press("d", "KeyD", mods_ctrl_alt);
    linux.settle();
    assert!(linux.wall.terminal("t2").is_some());
    assert_eq!(linux.frame().groups.len(), 1);

    // Repaint l1's resized screen, double-click "linux", Ctrl+Shift+C copies, Ctrl+Shift+V pastes.
    linux.set_screen_lines(&l1, &["linux clipboard test"]);
    linux.run_ok(&format!("terminal.focus {l1}"));
    linux.now_ms += 1_000.0;
    let word_pt = linux.cell_world(&l1, 2, 0);
    linux.click_world(word_pt, MODS_NONE);
    linux.click_world(word_pt, MODS_NONE);
    linux.clipboard.clear();
    linux.key_press("C", "KeyC", mods_ctrl_shift);
    assert_eq!(linux.clipboard, "linux");

    linux.pasted.clear();
    linux.key_press("V", "KeyV", mods_ctrl_shift);
    assert_eq!(linux.pasted, vec![(l1, "linux".to_owned())]);
}

#[test]
fn disk_backed_wall_open_persists_across_instances() {
    let dir = std::env::temp_dir().join(format!("wall-scenarios-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let opts = WallOptions {
        platform: Platform::Mac,
        ..Default::default()
    };

    {
        let mut wall = Wall::open(&dir, "disk-test", opts.clone());
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24));
        let reply = wall.run("wall.rename t1 saved-on-disk");
        assert!(!reply.is_err());
        let reply2 = wall.run("wall.grid off");
        assert!(!reply2.is_err());
    }

    {
        let mut wall2 = Wall::open(&dir, "disk-test", opts);
        wall2.set_live_terminals(&["t1"]);
        wall2.terminal_appeared(TerminalAppeared::adopted("t1", 80, 24));
        assert_eq!(wall2.name_of("t1"), "saved-on-disk");
        assert!(!wall2.config().grid_show);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

