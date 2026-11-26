//! End-to-end scenario tests driving `wall_core::wall::Wall` through its plain-data
//! input methods against a fake daemon/window backend that applies `WallEffect`s.
//!
//! Follows the non-pixel scenarios of the JS gate (`tools/wall-e2e.mjs`) plus the
//! full interaction contract for groups, splits, fullscreen, text selection, program
//! mouse reporting, and restart persistence through `WallStore`.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use term_cells::Screen;
use wall_core::dock::DockSide;
use wall_core::geom::{Point, Rect};
use wall_core::input::{Button, KeyEvent, Mods, Platform};
use wall_core::keymap::{KeyDecision, TerminalKey};
use wall_core::model::{CellGeometry, WallEffect};
use wall_core::pointer::{Buttons, CursorIcon, PointerEvent, TerminalMouseEvent, WheelEvent};
use wall_core::scroll::DeltaMode;
use wall_core::snap::{snap_step, snap_to, GridSpec};
use wall_core::store::{Storage, WallStore};
use wall_core::wall::{TerminalAppeared, Wall, WallFrame, WallOptions};

const MODS_NONE: Mods = Mods {
    shift: false,
    ctrl: false,
    alt: false,
    meta: false,
};
const MODS_CTRL: Mods = Mods {
    ctrl: true,
    ..MODS_NONE
};
const MODS_META: Mods = Mods {
    meta: true,
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
