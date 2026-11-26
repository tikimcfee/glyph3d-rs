//! Title-bar action buttons for a hovered terminal (`wall/BarButtons.js` + `wall/holdPress.js`).
//!
//! Draws three compact buttons at the right end of a hovered terminal's title bar:
//! 1. **Agent View** (`≡`): toggles the terminal's Agent View (`wall.agent <id> toggle`).
//! 2. **Hide** (`–`): hides the terminal on click (`wall.hide <id>`).
//! 3. **Close** (`×`): kills the terminal only when **held** for `close_hold_ms`
//!    (`wall.close <id>`), filling a clockwise `#e5534b` progress ring around the
//!    button while held (`wall/holdPress.js`).
//!
//! Shrinks with the title bar and hides completely below `min_bar_px` (or when no
//! terminal is hovered and no hold is running). Repaints are requested **only**
//! while a hold is actively in progress and cease on the exact frame the hold
//! completes or cancels.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Duration;

/// Default hold duration before `×` closes a terminal (`WallConfig::close_hold_ms`).
pub const DEFAULT_CLOSE_HOLD_MS: f64 = 650.0;
/// Default minimum title-bar height in points below which buttons hide (`WallConfig::bar_buttons_min_px`).
pub const DEFAULT_BAR_BUTTONS_MIN_PX: f32 = 14.0;

const BUTTON_GAP_PX: f32 = 2.0;
const RIGHT_INSET_PX: f32 = 3.0;
const HOLD_REPAINT_STEP: Duration = Duration::from_millis(16);

const FG_IDLE: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa3, 0xb2);
const FG_HOVER: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xe9, 0xef);
const FG_AGENT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0x8e, 0xf0, 0xb8);

const BG_HOVER: egui::Color32 = egui::Color32::from_rgb(0x26, 0x2d, 0x3d);
const BG_AGENT_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0x16, 0x40, 0x2c);
const HOLD_RING_COLOR: egui::Color32 = egui::Color32::from_rgb(0xe5, 0x53, 0x4b);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoldSource {
    Pointer,
    Key,
}

#[derive(Clone, Debug, PartialEq)]
struct ActiveHold {
    target_id: String,
    started_at_secs: f64,
    duration_secs: f64,
    source: HoldSource,
}

/// Persistent close-hold state kept across frames by the caller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BarButtonsState {
    hold: Option<ActiveHold>,
    /// Latched when a pointer hold finishes so keeping the mouse button depressed
    /// past completion does not immediately start a second close hold.
    awaiting_pointer_release: bool,
}

impl BarButtonsState {
    /// Terminal id whose close hold is currently running, if any (`BarButtons.holdTarget` in JS).
    ///
    /// Callers can pass this terminal's `bar_rect` while a hold is active so a
    /// keyboard hold (`Chord+X`) keeps painting its ring on that terminal's bar.
    pub fn hold_target(&self) -> Option<&str> {
        self.hold.as_ref().map(|h| h.target_id.as_str())
    }

    /// True while a pointer or keyboard close hold is actively running.
    pub fn is_holding(&self) -> bool {
        self.hold.is_some()
    }

    /// Current `[0.0, 1.0]` ring progress of an active hold at `now_secs`, or `None` when idle.
    pub fn hold_progress(&self, now_secs: f64) -> Option<f32> {
        let hold = self.hold.as_ref()?;
        if hold.duration_secs <= 0.0 {
            return Some(1.0);
        }
        let elapsed = (now_secs - hold.started_at_secs).max(0.0);
        Some((elapsed / hold.duration_secs).clamp(0.0, 1.0) as f32)
    }

    /// Start a keyboard-driven close hold on `id` (`BarButtons.holdKey` / `Chord+X`).
    pub fn hold_key(&mut self, id: impl Into<String>, now_secs: f64, close_hold_ms: f64) {
        let id = id.into();
        if id.is_empty() {
            return;
        }
        self.hold = Some(ActiveHold {
            target_id: id,
            started_at_secs: now_secs,
            duration_secs: (close_hold_ms / 1000.0).max(0.0),
            source: HoldSource::Key,
        });
    }

    /// Release an active keyboard-driven close hold (`BarButtons.releaseKey`).
    pub fn release_key(&mut self) {
        if self
            .hold
            .as_ref()
            .is_some_and(|h| h.source == HoldSource::Key)
        {
            self.hold = None;
        }
    }

    /// Cancel any active close hold immediately.
    pub fn cancel(&mut self) {
        self.hold = None;
    }
}

/// Per-frame inputs for [`show`].
#[derive(Clone, Copy, Debug)]
pub struct BarButtonsInput<'a> {
    /// Hovered terminal id (or `state.hold_target()` while a hold is pinned).
    pub terminal_id: Option<&'a str>,
    /// Screen-space rectangle of `terminal_id`'s title bar this frame.
    pub bar_rect: Option<egui::Rect>,
    /// True if `terminal_id` currently has an Agent View open (`≡` highlighted).
    pub is_agent_open: bool,
    /// Required hold duration in milliseconds (`WallConfig::close_hold_ms`).
    pub close_hold_ms: f64,
    /// Minimum bar height in points required to show the buttons (`WallConfig::bar_buttons_min_px`).
    pub min_bar_px: f32,
    /// Monotonic frame timestamp in seconds.
    pub now_secs: f64,
}

impl Default for BarButtonsInput<'static> {
    fn default() -> Self {
        Self {
            terminal_id: None,
            bar_rect: None,
            is_agent_open: false,
            close_hold_ms: DEFAULT_CLOSE_HOLD_MS,
            min_bar_px: DEFAULT_BAR_BUTTONS_MIN_PX,
            now_secs: 0.0,
        }
    }
}

/// Geometry, repaint demand, and pointer-capture flags returned by [`show`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BarButtonsOutput {
    /// Bounding screen rectangle of the 3-button cluster (`None` when hidden).
    pub cluster_rect: Option<egui::Rect>,
    /// True **only** while a close hold is actively animating (`0 <= elapsed < close_hold_ms`).
    /// Becomes `false` on the exact frame the hold completes or cancels.
    pub wants_repaint: bool,
    /// Delay until the next hold animation frame when `wants_repaint` is `true`, else `None`.
    pub repaint_after: Option<Duration>,
    /// True when the pointer is over the visible button cluster this frame.
    pub wants_pointer: bool,
    /// Always `false`: bar buttons never take keyboard input.
    pub wants_keyboard: bool,
}

/// Compute the `[agent, hide, close]` button rectangles inside `bar_rect` (`BarButtons.update` in JS),
/// returning `None` when `bar_rect.height() < min_bar_px`.
pub fn button_cluster_rects(
    bar_rect: egui::Rect,
    min_bar_px: f32,
) -> Option<(egui::Rect, [egui::Rect; 3], f32)> {
    let bar_px = bar_rect.height();
    if !bar_px.is_finite() || bar_px < min_bar_px || bar_rect.width() <= 0.0 {
        return None;
    }
    let size = (bar_px - 4.0).clamp(8.0, 18.0);
    let cluster_w = 3.0 * size + 2.0 * BUTTON_GAP_PX;
    let x0 = bar_rect.max.x - RIGHT_INSET_PX - cluster_w;
    let y0 = bar_rect.min.y + (bar_px - size) * 0.5;

    let r0 = egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(size, size));
    let r1 = egui::Rect::from_min_size(
        egui::pos2(x0 + size + BUTTON_GAP_PX, y0),
        egui::vec2(size, size),
    );
    let r2 = egui::Rect::from_min_size(
        egui::pos2(x0 + 2.0 * (size + BUTTON_GAP_PX), y0),
        egui::vec2(size, size),
    );
    let cluster = egui::Rect::from_min_max(r0.min, r2.max);
    Some((cluster, [r0, r1, r2], size))
}

/// Draw the hovered terminal's bar buttons for one frame and append any triggered verbs to `out`.
pub fn show(
    ctx: &egui::Context,
    state: &mut BarButtonsState,
    input: &BarButtonsInput<'_>,
    out: &mut Vec<String>,
) -> BarButtonsOutput {
    let (primary_pressed, primary_down, pointer_pos) = ctx.input(|i| {
        (
            i.pointer.button_pressed(egui::PointerButton::Primary),
            i.pointer.primary_down(),
            i.pointer.interact_pos().or_else(|| i.pointer.latest_pos()),
        )
    });
    if !primary_down {
        state.awaiting_pointer_release = false;
    }

    let target_id = input.terminal_id.filter(|id| !id.is_empty());
    let layout = target_id
        .zip(input.bar_rect)
        .and_then(|(id, rect)| button_cluster_rects(rect, input.min_bar_px).map(|g| (id, g)));

    // If the bar is hidden or no terminal is hovered, cancel any pointer hold immediately,
    // while still advancing an active keyboard hold (`Chord+X` works in fullscreen or zoomed out).
    let Some((id, (cluster_rect, [agent_rect, hide_rect, close_rect], size))) = layout else {
        if state
            .hold
            .as_ref()
            .is_some_and(|h| h.source == HoldSource::Pointer)
        {
            state.hold = None;
        }
        let (wants_repaint, repaint_after) = tick_active_hold(ctx, state, input.now_secs, out);
        return BarButtonsOutput {
            cluster_rect: None,
            wants_repaint,
            repaint_after,
            wants_pointer: false,
            wants_keyboard: false,
        };
    };

    // Cancel a pointer hold if the target terminal changed, the button was released,
    // or the pointer slid off the `×` button (`pointerleave` / `pointerup` in `holdPress.js`).
    if let Some(hold) = &state.hold {
        if hold.source == HoldSource::Pointer {
            let still_on_close = hold.target_id == id
                && primary_down
                && pointer_pos.is_some_and(|p| close_rect.contains(p));
            if !still_on_close {
                state.hold = None;
            }
        }
    }

    // Start a pointer hold on primary press inside `close_rect`.
    if state.hold.is_none()
        && !state.awaiting_pointer_release
        && primary_pressed
        && pointer_pos.is_some_and(|p| close_rect.contains(p))
    {
        state.hold = Some(ActiveHold {
            target_id: id.to_owned(),
            started_at_secs: input.now_secs,
            duration_secs: (input.close_hold_ms / 1000.0).max(0.0),
            source: HoldSource::Pointer,
        });
    }

    let mut agent_clicked = false;
    let mut hide_clicked = false;
    let mut is_sizing_pass = false;

    egui::Area::new(egui::Id::new("wall_bar_buttons"))
        .order(egui::Order::Foreground)
        .fixed_pos(cluster_rect.min)
        .default_size(cluster_rect.size())
        .fade_in(false)
        .interactable(true)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                is_sizing_pass = true;
                ui.ctx().request_discard("wall_bar_buttons initial sizing");
            }
            ui.spacing_mut().item_spacing = egui::vec2(BUTTON_GAP_PX, 0.0);
            ui.horizontal(|ui| {
                let (_, agent_resp) =
                    ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
                agent_clicked = agent_resp.clicked();
                paint_bar_button(
                    ui.painter(),
                    agent_rect,
                    BarIcon::Agent,
                    agent_resp.hovered(),
                    input.is_agent_open,
                    None,
                );
                agent_resp
                    .on_hover_text("agent view (live transcript events alongside this terminal)");

                let (_, hide_resp) =
                    ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
                hide_clicked = hide_resp.clicked();
                paint_bar_button(
                    ui.painter(),
                    hide_rect,
                    BarIcon::Hide,
                    hide_resp.hovered(),
                    false,
                    None,
                );
                hide_resp.on_hover_text(
                    "hide (it keeps running; the sidebar and the palette bring it back)",
                );

                let (_, close_resp) =
                    ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
                let close_hovered =
                    close_resp.hovered() || pointer_pos.is_some_and(|p| close_rect.contains(p));
                let ring_progress = state
                    .hold
                    .as_ref()
                    .filter(|h| {
                        h.target_id == id
                            && h.duration_secs > 0.0
                            && (input.now_secs - h.started_at_secs).max(0.0) < h.duration_secs
                    })
                    .and_then(|_| state.hold_progress(input.now_secs));
                paint_bar_button(
                    ui.painter(),
                    close_rect,
                    BarIcon::Close,
                    close_hovered,
                    false,
                    ring_progress,
                );
                close_resp.on_hover_text("hold: close (ends the terminal)");
            });
        });

    if is_sizing_pass {
        return BarButtonsOutput {
            cluster_rect: Some(cluster_rect),
            wants_repaint: false,
            repaint_after: None,
            wants_pointer: pointer_pos.is_some_and(|p| cluster_rect.contains(p)),
            wants_keyboard: false,
        };
    }

    let was_pointer_hold = state
        .hold
        .as_ref()
        .is_some_and(|h| h.source == HoldSource::Pointer);
    let (wants_repaint, repaint_after) = tick_active_hold(ctx, state, input.now_secs, out);
    if was_pointer_hold && state.hold.is_none() && primary_down {
        state.awaiting_pointer_release = true;
    }

    if agent_clicked {
        out.push(format!("wall.agent {id} toggle"));
    }
    if hide_clicked {
        out.push(format!("wall.hide {id}"));
    }

    let wants_pointer = pointer_pos.is_some_and(|p| cluster_rect.contains(p));

    BarButtonsOutput {
        cluster_rect: Some(cluster_rect),
        wants_repaint,
        repaint_after,
        wants_pointer,
        wants_keyboard: false,
    }
}

fn tick_active_hold(
    ctx: &egui::Context,
    state: &mut BarButtonsState,
    now_secs: f64,
    out: &mut Vec<String>,
) -> (bool, Option<Duration>) {
    let Some(hold) = &state.hold else {
        return (false, None);
    };
    let elapsed = (now_secs - hold.started_at_secs).max(0.0);
    if hold.duration_secs <= 0.0 || elapsed >= hold.duration_secs {
        let target_id = hold.target_id.clone();
        state.hold = None;
        out.push(format!("wall.close {target_id}"));
        return (false, None);
    }

    let remaining = Duration::from_secs_f64((hold.duration_secs - elapsed).max(0.001));
    let step = remaining.min(HOLD_REPAINT_STEP);
    ctx.request_repaint_after(step);
    (true, Some(step))
}

#[derive(Clone, Copy)]
enum BarIcon {
    Agent,
    Hide,
    Close,
}

fn paint_bar_button(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: BarIcon,
    hovered: bool,
    active: bool,
    ring_progress: Option<f32>,
) {
    let size = rect.width().min(rect.height());
    let rounding = egui::CornerRadius::same(3);

    if active {
        painter.rect_filled(rect, rounding, BG_AGENT_ACTIVE);
    } else if hovered {
        painter.rect_filled(rect, rounding, BG_HOVER);
    }

    let fg = if active {
        FG_AGENT_ACTIVE
    } else if hovered || ring_progress.is_some() {
        FG_HOVER
    } else {
        FG_IDLE
    };

    let c = rect.center();
    let stroke_w = (size * 0.095).clamp(1.1, 1.6);
    let stroke = egui::Stroke::new(stroke_w, fg);

    match icon {
        BarIcon::Agent => {
            let half_w = (size * 0.25).max(2.0);
            let dy = (size * 0.19).max(1.6);
            for offset_y in [-dy, 0.0, dy] {
                painter.line_segment(
                    [
                        egui::pos2(c.x - half_w, c.y + offset_y),
                        egui::pos2(c.x + half_w, c.y + offset_y),
                    ],
                    stroke,
                );
            }
        }
        BarIcon::Hide => {
            let half_w = (size * 0.24).max(2.0);
            painter.line_segment(
                [egui::pos2(c.x - half_w, c.y), egui::pos2(c.x + half_w, c.y)],
                stroke,
            );
        }
        BarIcon::Close => {
            let half_d = (size * 0.19).max(1.6);
            painter.line_segment(
                [
                    egui::pos2(c.x - half_d, c.y - half_d),
                    egui::pos2(c.x + half_d, c.y + half_d),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x + half_d, c.y - half_d),
                    egui::pos2(c.x - half_d, c.y + half_d),
                ],
                stroke,
            );
        }
    }

    if let Some(progress) = ring_progress {
        let clamped = progress.clamp(0.0, 1.0);
        if clamped > 0.005 {
            let radius = (8.5 / 20.0) * size;
            let ring_w = (1.6 * (size / 18.0)).clamp(1.1, 1.7);
            let segments = ((clamped * 32.0).ceil() as usize).max(2);
            let mut pts = Vec::with_capacity(segments + 1);
            for idx in 0..=segments {
                let t = idx as f32 / segments as f32;
                let angle = -FRAC_PI_2 + clamped * TAU * t;
                pts.push(egui::pos2(
                    c.x + radius * angle.cos(),
                    c.y + radius * angle.sin(),
                ));
            }
            painter.add(egui::Shape::line(
                pts,
                egui::Stroke::new(ring_w, HOLD_RING_COLOR),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wall::ui::configure_wall_context;

    fn make_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        ctx
    }

    fn run_headless(
        ctx: &egui::Context,
        raw: egui::RawInput,
        mut build: impl FnMut(&egui::Context),
    ) -> egui::FullOutput {
        let mut full = ctx.run_ui(raw, |ui| build(ui.ctx()));
        full.textures_delta.clear();
        full
    }

    fn raw_at(now_secs: f64, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(now_secs),
            events,
            ..Default::default()
        }
    }

    fn root_repaint_delay(full: &egui::FullOutput) -> Duration {
        full.viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX)
    }

    #[test]
    fn hides_without_shapes_or_repaints_when_unhovered_or_below_min_bar_px() {
        let ctx = make_ctx();
        let mut state = BarButtonsState::default();
        let mut verbs = Vec::new();

        // No hovered terminal -> zero shapes, zero repaints.
        let mut out_none = BarButtonsOutput::default();
        let full_none = run_headless(&ctx, raw_at(0.0, Vec::new()), |ctx| {
            out_none = show(ctx, &mut state, &BarButtonsInput::default(), &mut verbs);
        });
        assert!(full_none.shapes.is_empty());
        assert_eq!(out_none, BarButtonsOutput::default());

        // Hovered terminal with bar height 11px (< default 14px) -> shrinks away completely.
        let tiny_bar = egui::Rect::from_min_size(egui::pos2(100.0, 80.0), egui::vec2(300.0, 11.0));
        let mut out_tiny = BarButtonsOutput::default();
        let full_tiny = run_headless(&ctx, raw_at(0.1, Vec::new()), |ctx| {
            out_tiny = show(
                ctx,
                &mut state,
                &BarButtonsInput {
                    terminal_id: Some("t1"),
                    bar_rect: Some(tiny_bar),
                    now_secs: 0.1,
                    ..BarButtonsInput::default()
                },
                &mut verbs,
            );
        });
        assert!(full_tiny.shapes.is_empty(), "bar below min_bar_px must draw nothing");
        assert_eq!(out_tiny.cluster_rect, None);
        assert!(!out_tiny.wants_repaint);
        assert!(verbs.is_empty());
    }

    #[test]
    fn scales_with_bar_height_and_emits_agent_and_hide_verbs_on_click() {
        let ctx = make_ctx();
        let mut state = BarButtonsState::default();

        // Check BarButtons.js sizing math at 14px (-> 10px buttons) and 24px (-> clamped 18px buttons).
        let bar_14 = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 14.0));
        let (_, [a14, _, c14], sz14) = button_cluster_rects(bar_14, 14.0).expect("visible at 14px");
        assert!((sz14 - 10.0).abs() < 1e-4);
        assert!((c14.max.x - (bar_14.max.x - 3.0)).abs() < 1e-4);
        assert!((a14.min.x - (bar_14.max.x - 3.0 - 30.0 - 4.0)).abs() < 1e-4);

        let bar = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 22.0));
        let (_, [agent_rect, hide_rect, _close_rect], sz22) =
            button_cluster_rects(bar, 14.0).expect("visible at 22px");
        assert!((sz22 - 18.0).abs() < 1e-4);

        // Frame 0 draws visible shapes immediately; frame 1+ sleeps with zero repaint requests.
        for (idx, t) in [0.0, 0.05].into_iter().enumerate() {
            let mut verbs = Vec::new();
            let mut out = BarButtonsOutput::default();
            let full = run_headless(&ctx, raw_at(t, Vec::new()), |ctx| {
                out = show(
                    ctx,
                    &mut state,
                    &BarButtonsInput {
                        terminal_id: Some("t7"),
                        bar_rect: Some(bar),
                        is_agent_open: true,
                        now_secs: t,
                        ..BarButtonsInput::default()
                    },
                    &mut verbs,
                );
            });
            assert!(!full.shapes.is_empty(), "bar buttons must draw on frame {idx}");
            assert!(!out.wants_repaint);
            assert_eq!(out.repaint_after, None);
            if idx > 0 {
                assert!(
                    root_repaint_delay(&full) >= Duration::from_secs(3600),
                    "settled idle bar buttons must never ask for repaints"
                );
            }
        }

        // Click `≡` -> `wall.agent t7 toggle`
        let mut verbs = Vec::new();
        let agent_pos = agent_rect.center();
        let _ = run_headless(
            &ctx,
            raw_at(
                1.0,
                vec![
                    egui::Event::PointerMoved(agent_pos),
                    egui::Event::PointerButton {
                        pos: agent_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
            |ctx| {
                let _ = show(
                    ctx,
                    &mut state,
                    &BarButtonsInput {
                        terminal_id: Some("t7"),
                        bar_rect: Some(bar),
                        now_secs: 1.0,
                        ..BarButtonsInput::default()
                    },
                    &mut verbs,
                );
            },
        );
        let _ = run_headless(
            &ctx,
            raw_at(
                1.02,
                vec![egui::Event::PointerButton {
                    pos: agent_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
            |ctx| {
                let _ = show(
                    ctx,
                    &mut state,
                    &BarButtonsInput {
                        terminal_id: Some("t7"),
                        bar_rect: Some(bar),
                        now_secs: 1.02,
                        ..BarButtonsInput::default()
                    },
                    &mut verbs,
                );
            },
        );
        assert_eq!(verbs, vec!["wall.agent t7 toggle"]);

        // Click `–` -> `wall.hide t7`
        verbs.clear();
        let hide_pos = hide_rect.center();
        let _ = run_headless(
            &ctx,
            raw_at(
                2.0,
                vec![
                    egui::Event::PointerMoved(hide_pos),
                    egui::Event::PointerButton {
                        pos: hide_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
            |ctx| {
                let _ = show(
                    ctx,
                    &mut state,
                    &BarButtonsInput {
                        terminal_id: Some("t7"),
                        bar_rect: Some(bar),
                        now_secs: 2.0,
                        ..BarButtonsInput::default()
                    },
                    &mut verbs,
                );
            },
        );
        let _ = run_headless(
            &ctx,
            raw_at(
                2.02,
                vec![egui::Event::PointerButton {
                    pos: hide_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
            |ctx| {
                let _ = show(
                    ctx,
                    &mut state,
                    &BarButtonsInput {
                        terminal_id: Some("t7"),
                        bar_rect: Some(bar),
                        now_secs: 2.02,
                        ..BarButtonsInput::default()
                    },
                    &mut verbs,
                );
            },
        );
        assert_eq!(verbs, vec!["wall.hide t7"]);
    }

    #[test]
    fn close_hold_animates_repaints_while_held_and_stops_immediately_on_cancel_or_completion() {
        let ctx = make_ctx();
        let mut state = BarButtonsState::default();
        let bar = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 22.0));
        let (_, [_, _, close_rect], _) = button_cluster_rects(bar, 14.0).expect("visible");
        let close_pos = close_rect.center();

        let run_frame = |state: &mut BarButtonsState,
                         now_secs: f64,
                         events: Vec<egui::Event>,
                         verbs: &mut Vec<String>|
         -> (BarButtonsOutput, Duration) {
            let mut out = BarButtonsOutput::default();
            let full = run_headless(&ctx, raw_at(now_secs, events), |ctx| {
                out = show(
                    ctx,
                    state,
                    &BarButtonsInput {
                        terminal_id: Some("t3"),
                        bar_rect: Some(bar),
                        close_hold_ms: 500.0,
                        now_secs,
                        ..BarButtonsInput::default()
                    },
                    verbs,
                );
            });
            (out, root_repaint_delay(&full))
        };

        // Settle Area registration on frame 0 so `root_repaint_delay` measures purely our hold timer.
        let mut verbs = Vec::new();
        let _ = run_frame(&mut state, 0.0, Vec::new(), &mut verbs);

        // 1. Short click/release (200 ms < 500 ms): requests repaints while held, cancels on
        //    release without emitting `wall.close`, and stops asking for repaints immediately.
        let (out_press, delay_press) = run_frame(
            &mut state,
            1.0,
            vec![
                egui::Event::PointerMoved(close_pos),
                egui::Event::PointerButton {
                    pos: close_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut verbs,
        );
        assert!(out_press.wants_repaint);
        assert!(delay_press <= HOLD_REPAINT_STEP);
        assert_eq!(state.hold_target(), Some("t3"));
        assert!(verbs.is_empty());

        let (out_mid, delay_mid) = run_frame(&mut state, 1.2, Vec::new(), &mut verbs);
        assert!(out_mid.wants_repaint);
        assert!(delay_mid <= HOLD_REPAINT_STEP);
        assert!(verbs.is_empty());

        let (out_cancel, _) = run_frame(
            &mut state,
            1.25,
            vec![egui::Event::PointerButton {
                pos: close_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut verbs,
        );
        assert!(!out_cancel.wants_repaint, "cancelled hold must stop repainting at once");
        assert_eq!(out_cancel.repaint_after, None);
        assert!(verbs.is_empty(), "early release must not close terminal");
        // Once egui's built-in hover-tooltip fade (~83ms) settles, context sleeps completely idle.
        let _ = run_frame(&mut state, 1.40, Vec::new(), &mut verbs);
        let (out_cancel_settled, delay_cancel) = run_frame(&mut state, 1.60, Vec::new(), &mut verbs);
        assert!(!out_cancel_settled.wants_repaint);
        assert!(
            delay_cancel >= Duration::from_secs(3600),
            "egui context must sleep idle after hold cancel, got {delay_cancel:?}"
        );

        // 2. Pointer sliding off `×` (`pointerleave`) mid-hold cancels and stops repaints.
        let _ = run_frame(
            &mut state,
            2.0,
            vec![
                egui::Event::PointerMoved(close_pos),
                egui::Event::PointerButton {
                    pos: close_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut verbs,
        );
        assert!(state.is_holding());
        let outside = egui::pos2(bar.min.x + 10.0, close_pos.y);
        let (out_leave, _) = run_frame(
            &mut state,
            2.2,
            vec![egui::Event::PointerMoved(outside)],
            &mut verbs,
        );
        assert!(!out_leave.wants_repaint);
        assert_eq!(out_leave.repaint_after, None);
        assert!(verbs.is_empty());
        let _ = run_frame(
            &mut state,
            2.25,
            vec![egui::Event::PointerButton {
                pos: outside,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut verbs,
        );
        let _ = run_frame(&mut state, 2.45, Vec::new(), &mut verbs);
        let (out_leave_settled, delay_leave) = run_frame(&mut state, 2.65, Vec::new(), &mut verbs);
        assert!(!out_leave_settled.wants_repaint);
        assert!(delay_leave >= Duration::from_secs(3600));

        // 3. Full pointer hold >= 500 ms: emits `wall.close t3` once and stops asking for
        //    repaints on the completion frame itself (even while mouse button stays down).
        let (out_start, _) = run_frame(
            &mut state,
            3.0,
            vec![
                egui::Event::PointerMoved(close_pos),
                egui::Event::PointerButton {
                    pos: close_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut verbs,
        );
        assert!(out_start.wants_repaint);

        let (out_almost, _) = run_frame(&mut state, 3.49, Vec::new(), &mut verbs);
        assert!(out_almost.wants_repaint);
        assert!(verbs.is_empty());

        let (out_done, _) = run_frame(&mut state, 3.50, Vec::new(), &mut verbs);
        assert_eq!(verbs, vec!["wall.close t3"]);
        assert!(
            !out_done.wants_repaint,
            "completed hold must stop requesting repaints on the completion frame"
        );
        assert_eq!(out_done.repaint_after, None);

        // Holding mouse down another frame must NOT start a second hold until released,
        // and once egui's hover-tooltip fade settles, context sleeps completely idle.
        verbs.clear();
        let _ = run_frame(&mut state, 3.65, Vec::new(), &mut verbs);
        let _ = run_frame(&mut state, 3.85, Vec::new(), &mut verbs);
        let (out_after, delay_after) = run_frame(&mut state, 4.05, Vec::new(), &mut verbs);
        assert!(!out_after.wants_repaint);
        assert!(
            delay_after >= Duration::from_secs(3600),
            "egui context must sleep once hold completes, got {delay_after:?}"
        );
        assert!(verbs.is_empty());
        // Move pointer off and release before testing keyboard hold in isolation.
        let _ = run_frame(
            &mut state,
            4.10,
            vec![
                egui::Event::PointerMoved(outside),
                egui::Event::PointerButton {
                    pos: outside,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut verbs,
        );
        let _ = run_frame(&mut state, 4.30, Vec::new(), &mut verbs);
        let _ = run_frame(&mut state, 4.50, Vec::new(), &mut verbs);

        // 4. Keyboard hold (`Chord+X`): animates, cancels on `release_key`, fires on completion.
        state.hold_key("t3", 5.0, 400.0);
        let (out_k1, _) = run_frame(&mut state, 5.2, Vec::new(), &mut verbs);
        assert!(out_k1.wants_repaint);
        state.release_key();
        let (out_k_rel, delay_k_rel) = run_frame(&mut state, 5.25, Vec::new(), &mut verbs);
        assert!(!out_k_rel.wants_repaint);
        assert!(delay_k_rel >= Duration::from_secs(3600));
        assert!(verbs.is_empty());

        state.hold_key("t3", 6.0, 400.0);
        let (out_k_done, delay_k_done) = run_frame(&mut state, 6.4, Vec::new(), &mut verbs);
        assert_eq!(verbs, vec!["wall.close t3"]);
        assert!(!out_k_done.wants_repaint);
        assert!(delay_k_done >= Duration::from_secs(3600));
    }

    #[cfg(feature = "wgpu")]
    #[test]
    fn gpu_renders_topbar_and_bar_buttons_preview_to_png() {
        use std::path::PathBuf;
        use std::sync::Arc;

        use glam::Vec2;
        use term_cells::{Cell, Screen, rgb};

        use crate::atlas::Atlas;
        use crate::wall::panels::topbar::{self, TopbarInput, TopbarState};
        use crate::wall::{
            TerminalDraw, WallCamera, WallFrame, WallRenderer, WallStyle,
            render_wall_to_png_with_overlay,
        };

        let gpu = crate::gpu::test_device::context();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut wall_renderer = WallRenderer::new(
            &gpu.device,
            &gpu.queue,
            atlas,
            format,
            WallStyle::default(),
        );
        let mut egui_renderer =
            egui_wgpu::Renderer::new(&gpu.device, format, egui_wgpu::RendererOptions::default());

        let mut screen = Screen::new(56, 14);
        screen.default_bg = rgb(0x0d, 0x10, 0x17);
        let line = "$ wallctl wall.list  # hovered title bar + topbar";
        for (col, ch) in line.chars().enumerate() {
            if col < usize::from(screen.cols) {
                let idx = usize::from(screen.cols) + col;
                screen.cells[idx] = Cell {
                    content: ch as u32,
                    fg: rgb(0xd8, 0xde, 0xe9),
                    ..Cell::BLANK
                };
            }
        }
        screen.generation += 1;
        screen.row_gen[1] = screen.generation;

        let vp = (960u32, 540u32);
        let cam = WallCamera::new(Vec2::new(14.0, -6.0), 22.0);
        let egui_ctx = make_ctx();
        let mut top_state = TopbarState::default();
        let mut bar_state = BarButtonsState::default();
        bar_state.hold_key("t1", 1.0, 650.0);

        let bar_rect = egui::Rect::from_min_size(egui::pos2(190.0, 128.0), egui::vec2(580.0, 22.0));
        let top_input = TopbarInput {
            wall_name: "wall",
            termd_chip_width: 84.0,
            sidebar_open: true,
            ..TopbarInput::default()
        };
        let bar_input = BarButtonsInput {
            terminal_id: Some("t1"),
            bar_rect: Some(bar_rect),
            is_agent_open: true,
            close_hold_ms: 650.0,
            min_bar_px: 14.0,
            now_secs: 1.40, // ~62% hold ring filled
        };

        let mut verbs = Vec::new();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(vp.0 as f32, vp.1 as f32),
            )),
            time: Some(1.40),
            ..Default::default()
        };
        let mut full = egui_ctx.run_ui(raw, |ui| {
            ui.ctx()
                .layer_painter(egui::LayerId::background())
                .rect_filled(
                    bar_rect,
                    egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 },
                    egui::Color32::from_rgb(0x16, 0x1b, 0x26),
                );
            ui.ctx().layer_painter(egui::LayerId::background()).text(
                egui::pos2(bar_rect.min.x + 8.0, bar_rect.center().y),
                egui::Align2::LEFT_CENTER,
                "t1 · zsh (80×24)",
                egui::FontId::monospace(11.5),
                egui::Color32::from_rgb(0xc8, 0xce, 0xd8),
            );
            let _ = topbar::show(ui.ctx(), &mut top_state, &top_input, &mut verbs);
            let _ = show(ui.ctx(), &mut bar_state, &bar_input, &mut verbs);
        });

        let clipped = egui_ctx.tessellate(full.shapes, full.pixels_per_point);
        assert!(!clipped.is_empty());
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [vp.0, vp.1],
            pixels_per_point: full.pixels_per_point,
        };
        for (id, deltas) in full.textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(&gpu.device, &gpu.queue, id, &delta);
            }
        }

        let out_path = std::env::var_os("GLYPH_PANELS_PREVIEW_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("glyph_wall_panels_preview.png"));
        let draws = [TerminalDraw {
            id: 1,
            screen: &screen,
            position: Vec2::new(2.0, -3.5),
            z: 0.0,
            scale: 1.0,
            focused: true,
            panel_opacity: Some(1.0),
            border_color: None,
        }];
        let frame = WallFrame {
            terminals: &draws,
            ..Default::default()
        };

        render_wall_to_png_with_overlay(
            &gpu,
            &mut wall_renderer,
            &cam,
            &frame,
            vp,
            1,
            &out_path,
            |encoder, color_view| {
                let cmds = egui_renderer.update_buffers(
                    &gpu.device,
                    &gpu.queue,
                    encoder,
                    &clipped,
                    &screen_desc,
                );
                if !cmds.is_empty() {
                    gpu.queue.submit(cmds);
                }
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("panels preview egui pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: color_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                egui_renderer.render(&mut pass.forget_lifetime(), &clipped, &screen_desc);
            },
        );
        for id in full.textures_delta.free.drain() {
            egui_renderer.free_texture(&id);
        }
        assert!(
            out_path.is_file(),
            "expected preview PNG at {}",
            out_path.display()
        );
    }
}
