//! A panel's edges are its resize handles (2026-11-27; the JS panels were one
//! size each).
//!
//! Every panel draws itself, then calls [`grips`] inside its own `Area`: a
//! thin strip along each edge that can move takes a drag. While the drag
//! lasts the panel is drawn at the dragged size ([`ResizeState::live`]); on
//! release the size goes to the wall as a verb line, which is what stores it
//! (`wall.<panel> width <px>` docked, `wall.<panel> size <w> [h]` floating),
//! and a double click puts the default back (`width default`, `size
//! default`). Nothing here touches the store.
//!
//! - **Docked** to an edge, a panel has one grip: the edge that faces the
//!   canvas. Its width is the store's `dockWidth`.
//! - **Floating**, it has its left, right and bottom edges and the two
//!   bottom corners (the top is the header: it moves the panel). A drag pins
//!   where the panel is (`wall.<panel> float x y`), since an unplaced panel
//!   is anchored by its right edge and would grow away from the pointer.
//!   A side edge sets the width alone; the bottom and the corners set the
//!   height too, and from then on the panel is that tall rather than as tall
//!   as what it holds.
//! - **Centred** (the palette), a side edge moves both sides, and no place
//!   is written.
//!
//! The strips are inside the panel, registered after its contents, so they
//! take the pointer over a row or a scroll bar under them.

use wall_core::dock::{DockSide, PanelLimits, PanelSize};

/// How far into the panel an edge's strip reaches.
const GRIP_PX: f32 = 6.0;
/// The side of the square a bottom corner takes.
const CORNER_PX: f32 = 14.0;
/// The canvas a docked panel always leaves: it cannot be dragged wider than
/// the viewport less this.
const CANVAS_KEPT_PX: f32 = 120.0;
/// The margin a floating panel keeps from the viewport's far edges.
const FLOAT_MARGIN_PX: f32 = 16.0;
/// The border a floating panel draws around what it holds. A panel's size
/// is its box with the border, so each takes this off both ways for what is
/// inside.
pub const FLOAT_RIM_PX: f32 = 1.0;
/// The line drawn on the edge under the pointer.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x8a, 0xb4, 0xf8);

/// One edge or corner of a panel that takes a resize drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Grip {
    Left,
    Right,
    Bottom,
    BottomLeft,
    BottomRight,
}

impl Grip {
    /// The name `app.where` prints the grip under.
    pub const fn name(self) -> &'static str {
        match self {
            Grip::Left => "left",
            Grip::Right => "right",
            Grip::Bottom => "bottom",
            Grip::BottomLeft => "bottom_left",
            Grip::BottomRight => "bottom_right",
        }
    }

    const fn moves_left(self) -> bool {
        matches!(self, Grip::Left | Grip::BottomLeft)
    }

    const fn moves_right(self) -> bool {
        matches!(self, Grip::Right | Grip::BottomRight)
    }

    const fn moves_bottom(self) -> bool {
        matches!(self, Grip::Bottom | Grip::BottomLeft | Grip::BottomRight)
    }

    const fn cursor(self) -> egui::CursorIcon {
        match self {
            Grip::Left | Grip::Right => egui::CursorIcon::ResizeHorizontal,
            Grip::Bottom => egui::CursorIcon::ResizeVertical,
            Grip::BottomLeft => egui::CursorIcon::ResizeNeSw,
            Grip::BottomRight => egui::CursorIcon::ResizeNwSe,
        }
    }

    /// The strip of `panel` this grip takes the pointer on.
    fn hit_rect(self, panel: egui::Rect) -> egui::Rect {
        let corner = CORNER_PX.min(panel.width() * 0.25).min(panel.height() * 0.25);
        match self {
            Grip::Left => egui::Rect::from_min_max(
                panel.min,
                egui::pos2(panel.left() + GRIP_PX, panel.bottom()),
            ),
            Grip::Right => egui::Rect::from_min_max(
                egui::pos2(panel.right() - GRIP_PX, panel.top()),
                panel.max,
            ),
            Grip::Bottom => egui::Rect::from_min_max(
                egui::pos2(panel.left(), panel.bottom() - GRIP_PX),
                panel.max,
            ),
            Grip::BottomLeft => egui::Rect::from_min_max(
                egui::pos2(panel.left(), panel.bottom() - corner),
                egui::pos2(panel.left() + corner, panel.bottom()),
            ),
            Grip::BottomRight => egui::Rect::from_min_max(
                egui::pos2(panel.right() - corner, panel.bottom() - corner),
                panel.max,
            ),
        }
    }
}

/// The grips of a panel on `side`, the corners last so they are on top of
/// the edges they share pixels with.
const fn grips_of(side: DockSide, centered: bool) -> &'static [Grip] {
    match (side, centered) {
        // Docked left, the panel's right edge faces the canvas; and the
        // other way round.
        (DockSide::Left, _) => &[Grip::Right],
        (DockSide::Right, _) => &[Grip::Left],
        (DockSide::Float, true) => &[Grip::Left, Grip::Right, Grip::Bottom],
        (DockSide::Float, false) => &[
            Grip::Left,
            Grip::Right,
            Grip::Bottom,
            Grip::BottomLeft,
            Grip::BottomRight,
        ],
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    grip: Grip,
    /// Where the pointer went down.
    from: egui::Pos2,
    /// The panel when it did.
    start: egui::Rect,
    /// The panel as dragged so far.
    now: egui::Rect,
}

/// The box a drag in progress gives its panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveBox {
    pub rect: egui::Rect,
    /// Whether the drag sets the height (the bottom edge or a corner); a
    /// side edge leaves the height to the panel.
    pub sets_height: bool,
}

/// One panel's resize state, kept across frames beside its other view state.
#[derive(Clone, Debug, Default)]
pub struct ResizeState {
    drag: Option<Drag>,
    /// The strips offered on the last pass, for `app.where` and the tests.
    pub grips: Vec<(Grip, egui::Rect)>,
}

impl ResizeState {
    /// While an edge is being dragged, the box the panel is dragged to.
    pub fn live(&self) -> Option<LiveBox> {
        self.drag.map(|d| LiveBox {
            rect: d.now,
            sets_height: d.grip.moves_bottom(),
        })
    }

    /// The dragged width, for a panel whose width is all a drag can change
    /// (a docked one).
    pub fn live_width(&self) -> Option<f32> {
        self.drag.map(|d| d.now.width())
    }

    /// Whether an edge is being dragged.
    pub const fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Forget a drag and the strips: the panel closed.
    pub fn reset(&mut self) {
        self.drag = None;
        self.grips.clear();
    }

    /// The strip of one grip on the last pass.
    pub fn grip_rect(&self, grip: Grip) -> Option<egui::Rect> {
        self.grips.iter().find(|(g, _)| *g == grip).map(|(_, r)| *r)
    }

    /// What `app.where` prints: `left:l,t,r,b;bottom:...`, or `none`.
    pub fn report(&self) -> String {
        if self.grips.is_empty() {
            return "none".to_string();
        }
        self.grips
            .iter()
            .map(|(g, r)| {
                format!(
                    "{}:{:.2},{:.2},{:.2},{:.2}",
                    g.name(),
                    r.min.x,
                    r.min.y,
                    r.max.x,
                    r.max.y
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// The width a floating panel is drawn at: the drag's while an edge is
/// dragged, else the one set on it, else `fallback`; never wider than the
/// viewport leaves room for.
pub fn float_width(
    state: &ResizeState,
    size: &PanelSize,
    fallback: f32,
    viewport: egui::Rect,
) -> f32 {
    let room = (viewport.width() - 2.0 * FLOAT_MARGIN_PX).max(140.0);
    state
        .live()
        .map(|l| l.rect.width())
        .or(size.float_width.map(|w| w as f32))
        .unwrap_or(fallback)
        .min(room)
}

/// The height a floating panel is held to, if it has one: the drag's while
/// the bottom is dragged, else the one set on it. `None` leaves the panel
/// as tall as what it holds.
pub fn float_height(state: &ResizeState, size: &PanelSize, viewport: egui::Rect) -> Option<f32> {
    let room = (viewport.height() - 2.0 * FLOAT_MARGIN_PX).max(96.0);
    state
        .live()
        .filter(|l| l.sets_height)
        .map(|l| l.rect.height())
        .or(size.float_height.map(|h| h as f32))
        .map(|h| h.min(room))
}

/// The most a floating panel whose top is at `top` may be tall for its
/// bottom edge to stay in the viewport: a panel moved down the window used
/// to run off its foot, and its bottom edge is a grip now.
pub fn room_below(top: f32, viewport: egui::Rect) -> f32 {
    (viewport.bottom() - top - 0.5 * FLOAT_MARGIN_PX).max(96.0)
}

/// Where a floating panel's top-left is while one of its edges is dragged:
/// the drag pins it, so the edge under the pointer is the one that moves.
pub fn float_origin(state: &ResizeState) -> Option<egui::Pos2> {
    state.live().map(|l| l.rect.min)
}

/// What [`grips`] needs to know about the panel it is called for.
#[derive(Clone, Copy, Debug)]
pub struct ResizeSpec<'a> {
    /// The panel's verb (`"wall.sidebar"`): sizes are sent as `<verb> width
    /// …`, `<verb> size …` and `<verb> float x y`.
    pub verb: &'a str,
    pub side: DockSide,
    /// The panel as drawn on this pass.
    pub rect: egui::Rect,
    pub limits: PanelLimits,
    pub viewport: egui::Rect,
    /// A panel centred in the viewport (the palette): a side edge moves
    /// both sides, and no place is written.
    pub centered: bool,
}

/// JavaScript `Math.round`, as the panels' other coordinates are written.
fn px(value: f32) -> i64 {
    (f64::from(value) + 0.5).floor() as i64
}

fn dragged_rect(drag: &Drag, pointer: egui::Pos2, spec: &ResizeSpec<'_>) -> egui::Rect {
    let delta = pointer - drag.from;
    let start = drag.start;
    let limits = &spec.limits;
    let viewport = spec.viewport;
    match spec.side {
        DockSide::Left | DockSide::Right => {
            let min_w = limits.dock_min as f32;
            let max_w = (limits.dock_max as f32)
                .min(viewport.width() - CANVAS_KEPT_PX)
                .max(min_w);
            if spec.side == DockSide::Left {
                let w = (start.width() + delta.x).clamp(min_w, max_w);
                egui::Rect::from_min_size(start.min, egui::vec2(w, start.height()))
            } else {
                let w = (start.width() - delta.x).clamp(min_w, max_w);
                egui::Rect::from_min_max(egui::pos2(start.right() - w, start.top()), start.max)
            }
        }
        DockSide::Float => {
            let min_w = limits.float_min_width as f32;
            let max_w = (limits.float_max_width as f32)
                .min(viewport.width() - 2.0 * FLOAT_MARGIN_PX)
                .max(min_w);
            let min_h = limits.float_min_height as f32;
            let max_h = (limits.float_max_height as f32)
                .min(viewport.height() - 2.0 * FLOAT_MARGIN_PX)
                .max(min_h);
            let mut rect = start;
            if spec.centered {
                let grow = match drag.grip {
                    Grip::Left => -2.0 * delta.x,
                    Grip::Right => 2.0 * delta.x,
                    _ => 0.0,
                };
                let w = (start.width() + grow).clamp(min_w, max_w);
                rect = egui::Rect::from_center_size(
                    start.center(),
                    egui::vec2(w, start.height()),
                );
                rect.min.y = start.min.y;
                rect.max.y = start.max.y;
            } else if drag.grip.moves_right() {
                rect.max.x = start.left() + (start.width() + delta.x).clamp(min_w, max_w);
            } else if drag.grip.moves_left() {
                // The left edge stops at the viewport's: the panel's place
                // is never written off screen.
                let room = (start.right() - viewport.left()).max(min_w);
                let w = (start.width() - delta.x).clamp(min_w, max_w.min(room));
                rect.min.x = start.right() - w;
            }
            if drag.grip.moves_bottom() {
                rect.max.y = start.top() + (start.height() + delta.y).clamp(min_h, max_h);
            }
            rect
        }
    }
}

/// The verb lines a finished drag sends.
fn emit(spec: &ResizeSpec<'_>, drag: &Drag, out: &mut Vec<String>) {
    if drag.now == drag.start {
        return;
    }
    let verb = spec.verb;
    match spec.side {
        DockSide::Left | DockSide::Right => {
            out.push(format!("{verb} width {}", px(drag.now.width())));
        }
        DockSide::Float => {
            if !spec.centered {
                out.push(format!(
                    "{verb} float {} {}",
                    px(drag.now.left()),
                    px(drag.now.top())
                ));
            }
            if drag.grip.moves_bottom() {
                out.push(format!(
                    "{verb} size {} {}",
                    px(drag.now.width()),
                    px(drag.now.height())
                ));
            } else {
                out.push(format!("{verb} size {}", px(drag.now.width())));
            }
        }
    }
}

/// Offer and run one panel's resize grips. Call it inside the panel's own
/// `Area`, after its contents, with the rectangle the panel was drawn in.
pub fn grips(
    ui: &mut egui::Ui,
    state: &mut ResizeState,
    spec: &ResizeSpec<'_>,
    out: &mut Vec<String>,
) {
    state.grips.clear();
    // A drag on a grip the panel no longer has (it was docked or floated
    // mid-drag) is dropped.
    let offered = grips_of(spec.side, spec.centered);
    if state.drag.is_some_and(|d| !offered.contains(&d.grip)) {
        state.drag = None;
    }

    for &grip in offered {
        let hit = grip.hit_rect(spec.rect);
        state.grips.push((grip, hit));
        let resp = ui.interact(
            hit,
            egui::Id::new(("wall_panel_resize", spec.verb, grip.name())),
            egui::Sense::click_and_drag(),
        );

        if resp.drag_started_by(egui::PointerButton::Primary) {
            let from = ui
                .input(|i| i.pointer.press_origin().or_else(|| i.pointer.interact_pos()))
                .unwrap_or_else(|| hit.center());
            state.drag = Some(Drag {
                grip,
                from,
                start: spec.rect,
                now: spec.rect,
            });
        }

        let mine = state.drag.is_some_and(|d| d.grip == grip);
        if mine && resp.dragged_by(egui::PointerButton::Primary) {
            if let (Some(pointer), Some(drag)) =
                (ui.input(|i| i.pointer.interact_pos()), state.drag.as_mut())
            {
                drag.now = dragged_rect(drag, pointer, spec);
            }
        }
        if mine && resp.drag_stopped() {
            if let Some(drag) = state.drag.take() {
                emit(spec, &drag, out);
            }
        }

        if resp.double_clicked() {
            state.drag = None;
            out.push(match spec.side {
                DockSide::Left | DockSide::Right => format!("{} width default", spec.verb),
                DockSide::Float => format!("{} size default", spec.verb),
            });
        }

        if resp.hovered() || mine {
            ui.ctx().set_cursor_icon(grip.cursor());
            let stroke = egui::Stroke::new(2.0, ACCENT);
            let r = spec.rect;
            let painter = ui.painter();
            if grip.moves_left() {
                painter.vline(r.left() + 1.0, r.y_range(), stroke);
            }
            if grip.moves_right() {
                painter.vline(r.right() - 1.0, r.y_range(), stroke);
            }
            if grip.moves_bottom() {
                painter.hline(r.x_range(), r.bottom() - 1.0, stroke);
            }
        }
        let _ = resp.on_hover_text("drag to resize · double-click for the default size");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::dock::PanelKind;

    const VIEW: egui::Vec2 = egui::vec2(1400.0, 900.0);

    /// A stand-in panel: a filled box at `rect`, with a click-sensing row
    /// across it (as a sidebar row is), then its grips.
    struct Probe {
        ctx: egui::Context,
        state: ResizeState,
        side: DockSide,
        centered: bool,
        /// The panel's box when no drag is live: what the store would give.
        stored: egui::Rect,
        time: f64,
    }

    impl Probe {
        fn new(side: DockSide, stored: egui::Rect) -> Self {
            Probe {
                ctx: egui::Context::default(),
                state: ResizeState::default(),
                side,
                centered: false,
                stored,
                time: 0.0,
            }
        }

        /// The box the panel is drawn in on this pass: the drag's, as each
        /// panel reads it.
        fn drawn(&self) -> egui::Rect {
            self.state.live().map_or(self.stored, |l| l.rect)
        }

        fn pass(&mut self, events: Vec<egui::Event>) -> Vec<String> {
            self.time += 0.05;
            let rect = self.drawn();
            let spec = ResizeSpec {
                verb: "wall.probe",
                side: self.side,
                rect,
                limits: PanelKind::Sidebar.limits(),
                viewport: egui::Rect::from_min_size(egui::Pos2::ZERO, VIEW),
                centered: self.centered,
            };
            let mut out = Vec::new();
            let state = &mut self.state;
            let _ = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, VIEW)),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |root| {
                    egui::Area::new(egui::Id::new("resize_probe"))
                        .fixed_pos(rect.min)
                        .constrain(false)
                        .show(root.ctx(), |ui| {
                            let (panel, _) =
                                ui.allocate_exact_size(rect.size(), egui::Sense::hover());
                            // A row under the grips: they must win over it.
                            let _ = ui.interact(
                                panel,
                                egui::Id::new("resize_probe_row"),
                                egui::Sense::click(),
                            );
                            grips(ui, state, &spec, &mut out);
                        });
                },
            );
            out
        }

        fn press(&mut self, pos: egui::Pos2) -> Vec<String> {
            let mut out = self.pass(vec![egui::Event::PointerMoved(pos)]);
            out.extend(self.pass(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]));
            out
        }

        fn release(&mut self, pos: egui::Pos2) -> Vec<String> {
            self.pass(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }])
        }

        /// Press on `grip`, move by `by` in three steps, release.
        fn drag(&mut self, grip: Grip, by: egui::Vec2) -> Vec<String> {
            self.pass(Vec::new());
            let from = self.state.grip_rect(grip).expect("grip offered").center();
            let mut out = self.press(from);
            for step in 1..=3 {
                let at = from + by * (step as f32 / 3.0);
                out.extend(self.pass(vec![egui::Event::PointerMoved(at)]));
            }
            out.extend(self.release(from + by));
            out
        }
    }

    fn docked(side: DockSide, width: f32) -> egui::Rect {
        match side {
            DockSide::Left => egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, VIEW.y)),
            _ => egui::Rect::from_min_size(
                egui::pos2(VIEW.x - width, 0.0),
                egui::vec2(width, VIEW.y),
            ),
        }
    }

    #[test]
    fn a_docked_panel_has_the_one_edge_that_faces_the_canvas() {
        let mut left = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        left.pass(Vec::new());
        assert_eq!(left.state.grips.len(), 1);
        let strip = left.state.grip_rect(Grip::Right).unwrap();
        assert_eq!(strip.right(), 248.0);
        assert_eq!(strip.height(), VIEW.y);

        let mut right = Probe::new(DockSide::Right, docked(DockSide::Right, 248.0));
        right.pass(Vec::new());
        assert_eq!(right.state.grips.len(), 1);
        assert_eq!(right.state.grip_rect(Grip::Left).unwrap().left(), VIEW.x - 248.0);

        let mut float = Probe::new(
            DockSide::Float,
            egui::Rect::from_min_size(egui::pos2(300.0, 100.0), egui::vec2(248.0, 400.0)),
        );
        float.pass(Vec::new());
        let names: Vec<&str> = float.state.grips.iter().map(|(g, _)| g.name()).collect();
        assert_eq!(names, ["left", "right", "bottom", "bottom_left", "bottom_right"]);
        assert!(float.state.report().starts_with("left:300.00,100.00,306.00,500.00;right:"));
    }

    #[test]
    fn dragging_a_docked_edge_sends_the_width_on_release_and_follows_meanwhile() {
        // Docked left: the right edge, dragged right, widens.
        let mut p = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Right).unwrap().center();
        assert!(p.press(from).is_empty());
        assert!(p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(40.0, 0.0))]).is_empty());
        assert!(p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(72.0, 3.0))]).is_empty());
        // Nothing is sent while the drag lasts; the panel is drawn at the
        // dragged width.
        assert!(p.state.dragging());
        assert_eq!(p.state.live_width(), Some(320.0));
        assert_eq!(p.drawn().width(), 320.0);
        assert_eq!(p.drawn().left(), 0.0);
        assert_eq!(p.release(from + egui::vec2(72.0, 3.0)), ["wall.probe width 320"]);
        assert!(!p.state.dragging());

        // Docked right: the left edge, dragged left, widens, and the panel
        // stays against the right edge.
        let mut p = Probe::new(DockSide::Right, docked(DockSide::Right, 248.0));
        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Left).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from - egui::vec2(100.0, 0.0))]);
        assert_eq!(p.drawn().right(), VIEW.x);
        assert_eq!(p.drawn().width(), 348.0);
        assert_eq!(p.release(from - egui::vec2(100.0, 0.0)), ["wall.probe width 348"]);

        // And dragged right, it narrows.
        let mut p = Probe::new(DockSide::Right, docked(DockSide::Right, 348.0));
        assert_eq!(p.drag(Grip::Left, egui::vec2(48.0, 0.0)), ["wall.probe width 300"]);
    }

    #[test]
    fn a_drag_stops_at_the_panels_limits_and_keeps_some_canvas() {
        // Narrower than the sidebar's 180 px is not offered.
        let mut p = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        assert_eq!(p.drag(Grip::Right, egui::vec2(-400.0, 0.0)), ["wall.probe width 180"]);
        // Nor wider than the viewport less the canvas it keeps.
        let mut p = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        assert_eq!(
            p.drag(Grip::Right, egui::vec2(5000.0, 0.0)),
            [format!("wall.probe width {}", VIEW.x - CANVAS_KEPT_PX)]
        );
        // A press and release that moved nothing sends nothing.
        let mut p = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        assert!(p.drag(Grip::Right, egui::vec2(0.0, 0.0)).is_empty());
    }

    #[test]
    fn a_floating_panel_resizes_from_three_edges_and_two_corners() {
        let at = egui::Rect::from_min_size(egui::pos2(300.0, 100.0), egui::vec2(260.0, 400.0));

        // The right edge: the width alone, and the panel's place is pinned.
        let mut p = Probe::new(DockSide::Float, at);
        assert_eq!(
            p.drag(Grip::Right, egui::vec2(90.0, 50.0)),
            ["wall.probe float 300 100", "wall.probe size 350"]
        );
        // The bottom edge: the height too.
        let mut p = Probe::new(DockSide::Float, at);
        assert_eq!(
            p.drag(Grip::Bottom, egui::vec2(30.0, 120.0)),
            ["wall.probe float 300 100", "wall.probe size 260 520"]
        );
        // The bottom-right corner: both.
        let mut p = Probe::new(DockSide::Float, at);
        assert_eq!(
            p.drag(Grip::BottomRight, egui::vec2(40.0, -100.0)),
            ["wall.probe float 300 100", "wall.probe size 300 300"]
        );
        // The left edge moves the panel's place with it; its right edge stays.
        let mut p = Probe::new(DockSide::Float, at);
        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Left).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from - egui::vec2(60.0, 0.0))]);
        assert_eq!(p.drawn().right(), at.right());
        assert_eq!(float_origin(&p.state), Some(egui::pos2(240.0, 100.0)));
        assert_eq!(
            p.release(from - egui::vec2(60.0, 0.0)),
            ["wall.probe float 240 100", "wall.probe size 320"]
        );
        // The bottom-left corner: place, width and height.
        let mut p = Probe::new(DockSide::Float, at);
        assert_eq!(
            p.drag(Grip::BottomLeft, egui::vec2(-40.0, 60.0)),
            ["wall.probe float 260 100", "wall.probe size 300 460"]
        );
        // The left edge stops at the viewport's.
        let mut p = Probe::new(DockSide::Float, at);
        assert_eq!(
            p.drag(Grip::Left, egui::vec2(-900.0, 0.0)),
            ["wall.probe float 0 100", "wall.probe size 560"]
        );
    }

    #[test]
    fn a_side_drag_leaves_the_height_to_the_panel_and_a_bottom_drag_holds_it() {
        let at = egui::Rect::from_min_size(egui::pos2(300.0, 100.0), egui::vec2(260.0, 400.0));
        let view = egui::Rect::from_min_size(egui::Pos2::ZERO, VIEW);
        let unset = PanelSize::default();
        let set = PanelSize {
            dock_width: None,
            float_width: Some(500.0),
            float_height: Some(333.0),
        };

        let mut p = Probe::new(DockSide::Float, at);
        // At rest: what was set, else the fallback and no held height.
        assert_eq!(float_width(&p.state, &unset, 248.0, view), 248.0);
        assert_eq!(float_height(&p.state, &unset, view), None);
        assert_eq!(float_width(&p.state, &set, 248.0, view), 500.0);
        assert_eq!(float_height(&p.state, &set, view), Some(333.0));
        assert_eq!(float_origin(&p.state), None);
        // Never wider or taller than the window has room for.
        let small = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        assert_eq!(float_width(&p.state, &set, 248.0, small), 368.0);
        assert_eq!(float_height(&p.state, &set, small), Some(268.0));
        // A panel lower down the window has less room below it, and never none.
        assert_eq!(room_below(100.0, view), 792.0);
        assert_eq!(room_below(700.0, view), 192.0);
        assert_eq!(room_below(880.0, view), 96.0);

        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Right).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(40.0, 0.0))]);
        assert_eq!(float_width(&p.state, &unset, 248.0, view), 300.0);
        assert_eq!(float_height(&p.state, &unset, view), None, "a side edge sets no height");
        assert_eq!(float_height(&p.state, &set, view), Some(333.0));
        p.release(from + egui::vec2(40.0, 0.0));

        let mut p = Probe::new(DockSide::Float, at);
        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Bottom).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(0.0, 50.0))]);
        assert_eq!(float_height(&p.state, &unset, view), Some(450.0));
    }

    #[test]
    fn a_centred_panel_grows_both_ways_and_writes_no_place() {
        let at = egui::Rect::from_center_size(egui::pos2(700.0, 300.0), egui::vec2(560.0, 300.0));
        let mut p = Probe::new(DockSide::Float, at);
        p.centered = true;
        p.pass(Vec::new());
        let names: Vec<&str> = p.state.grips.iter().map(|(g, _)| g.name()).collect();
        assert_eq!(names, ["left", "right", "bottom"]);

        let from = p.state.grip_rect(Grip::Right).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(50.0, 0.0))]);
        assert_eq!(p.drawn().center().x, 700.0);
        assert_eq!(p.drawn().width(), 660.0);
        assert_eq!(p.release(from + egui::vec2(50.0, 0.0)), ["wall.probe size 660"]);

        let mut p = Probe::new(DockSide::Float, at);
        p.centered = true;
        assert_eq!(p.drag(Grip::Left, egui::vec2(30.0, 0.0)), ["wall.probe size 500"]);
        let mut p = Probe::new(DockSide::Float, at);
        p.centered = true;
        assert_eq!(p.drag(Grip::Bottom, egui::vec2(0.0, 80.0)), ["wall.probe size 560 380"]);
    }

    #[test]
    fn a_double_click_on_an_edge_asks_for_the_default_size() {
        let mut p = Probe::new(DockSide::Right, docked(DockSide::Right, 400.0));
        p.pass(Vec::new());
        let at = p.state.grip_rect(Grip::Left).unwrap().center();
        let mut out = p.press(at);
        out.extend(p.release(at));
        assert!(out.is_empty(), "one click asks for nothing: {out:?}");
        out.extend(p.press(at));
        out.extend(p.release(at));
        assert_eq!(out, ["wall.probe width default"]);

        let mut p = Probe::new(
            DockSide::Float,
            egui::Rect::from_min_size(egui::pos2(300.0, 100.0), egui::vec2(260.0, 400.0)),
        );
        p.pass(Vec::new());
        let at = p.state.grip_rect(Grip::Bottom).unwrap().center();
        let mut out = p.press(at);
        out.extend(p.release(at));
        out.extend(p.press(at));
        out.extend(p.release(at));
        assert_eq!(out, ["wall.probe size default"]);
    }

    #[test]
    fn a_drag_is_dropped_when_the_panel_changes_sides_under_it() {
        let mut p = Probe::new(DockSide::Left, docked(DockSide::Left, 248.0));
        p.pass(Vec::new());
        let from = p.state.grip_rect(Grip::Right).unwrap().center();
        p.press(from);
        p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(40.0, 0.0))]);
        assert!(p.state.dragging());
        // Docked right now: it has no right grip.
        p.side = DockSide::Right;
        p.stored = docked(DockSide::Right, 248.0);
        let out = p.pass(vec![egui::Event::PointerMoved(from + egui::vec2(60.0, 0.0))]);
        assert!(out.is_empty());
        assert!(!p.state.dragging());
    }
}
