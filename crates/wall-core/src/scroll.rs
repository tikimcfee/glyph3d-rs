//! Terminal wheel scrolling and continuous wheel-stream gating, ported from
//! `wall/terminalScroll.js`.
//!
//! Decides three things with no window or DOM dependency:
//! - [`wheel_delta_lines`]: converts raw vertical wheel deltas (pixel, line, or page
//!   units) into signed fractional terminal lines (`+` = up into scrollback history,
//!   `-` = down toward live output), clamping pixel row heights to `[12, 36]` px so
//!   zoomed-out micro-terminals do not rocket and huge ones stay responsive.
//! - [`TerminalScroller`]: accumulates sub-line fractions 1:1 on trackpads and gentle
//!   OS momentum tails, while giving discrete mouse wheel notches an immediate lead
//!   line followed by an exponentially decaying frame-by-frame glide (`step(dt)`) that
//!   conserves whole lines and cancels on direction reversal, idle timeout, or reset.
//! - [`WheelStreamGate`]: quenches residual OS inertial wheel tails across fullscreen
//!   or camera-framing transitions, prevents pressing `⌘` or `⌥` mid-pan from turning
//!   a coasting scroll into camera zoom (while honouring `ctrl_key` trackpad pinches),
//!   and latches stationary-pointer gestures to their starting target (`Terminal` vs
//!   `Pan`).

use crate::geom::Point;

/// Fallback CSS/logical pixels per terminal line when no rendered `row_px` is supplied.
const DEFAULT_WHEEL_PX_PER_LINE: f64 = 30.0;

/// Lower clamp on screen-space row height (pixels) so micro-zoomed terminals do not rocket.
const MIN_ROW_PX: f64 = 12.0;

/// Upper clamp on screen-space row height (pixels) so deeply zoomed terminals stay responsive.
const MAX_ROW_PX: f64 = 36.0;

/// Idle gap in milliseconds after which a finished wheel gesture drops any leftover
/// sub-line remainder or trackpad classification (`STREAM_IDLE_MS` in `terminalScroll.js`).
pub const STREAM_IDLE_MS: f64 = 180.0;

/// Quiet gap in milliseconds with no wheel events before an inertial stream is considered over.
const GATE_QUIET_MS: f64 = 140.0;

/// Pixel delta threshold at gesture start separating a trackpad from a discrete mouse notch.
const NOTCH_MIN_PX: f64 = 80.0;

/// Exponential settle rate (`1/s`) for gliding multi-line discrete wheel notches across frames.
const GLIDE_SETTLE_RATE: f64 = 22.0;

/// Pointer travel in client pixels that breaks a stationary canvas wheel-stream target latch.
const LATCH_SLOP_PX: f64 = 24.0;

/// Tolerance for whole-line extraction and glide completion.
const EPS: f64 = 1e-6;

/// Unit of a wheel event's delta (`WheelEvent.deltaMode` in the browser).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeltaMode {
    /// CSS/logical pixels (`deltaMode === 0`).
    #[default]
    Pixel = 0,
    /// Lines (`deltaMode === 1`).
    Line = 1,
    /// Pages of `max(1, rows - 2)` lines (`deltaMode === 2`).
    Page = 2,
}

impl DeltaMode {
    /// Convert a DOM-style numeric `deltaMode` (`0`, `1`, `2`; anything else falls back to
    /// [`DeltaMode::Pixel`], matching `terminalScroll.js`).
    pub const fn from_u32(mode: u32) -> Self {
        match mode {
            1 => Self::Line,
            2 => Self::Page,
            _ => Self::Pixel,
        }
    }
}

/// Scaling parameters for [`wheel_delta_lines`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelDeltaOptions {
    /// Rendered screen-space height of one terminal row in pixels (`cell_stride.y * zoom`).
    /// When `None`, non-finite, or `<= 0`, falls back to 30 px per line.
    pub row_px: Option<f64>,
    /// Terminal height in rows, used to size page scrolls (`DeltaMode::Page`).
    pub rows: u32,
    /// Scroll speed multiplier (`scrollMultiplier`); non-finite or `<= 0` falls back to `1.0`.
    pub multiplier: f64,
}

impl Default for WheelDeltaOptions {
    fn default() -> Self {
        Self {
            row_px: None,
            rows: 24,
            multiplier: 1.0,
        }
    }
}

/// Options for [`TerminalScroller::wheel`] and [`TerminalScroller::wheel_with`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelOptions {
    /// Rendered screen-space height of one terminal row in pixels (`cell_stride.y * zoom`).
    pub row_px: Option<f64>,
    /// Terminal height in rows, used to size page scrolls (`DeltaMode::Page`).
    pub rows: u32,
    /// Scroll speed multiplier (`scrollMultiplier`); non-finite or `<= 0` falls back to `1.0`.
    pub multiplier: f64,
    /// Whether multi-line discrete mouse wheel notches should glide across `step(dt)` frames.
    pub glide: bool,
}

impl Default for WheelOptions {
    fn default() -> Self {
        Self {
            row_px: None,
            rows: 24,
            multiplier: 1.0,
            glide: false,
        }
    }
}

impl WheelOptions {
    fn delta_options(self) -> WheelDeltaOptions {
        WheelDeltaOptions {
            row_px: self.row_px,
            rows: self.rows,
            multiplier: self.multiplier,
        }
    }
}

/// Sign function matching JavaScript's `Math.sign` (`0.0` stays `0.0` instead of `±1.0`).
fn js_sign(value: f64) -> f64 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        value
    }
}

/// Convert a raw vertical wheel delta `dy` into signed fractional terminal lines
/// (`+` = scroll up into history, `-` = scroll down toward live output).
pub fn wheel_delta_lines(dy: f64, delta_mode: DeltaMode, opts: WheelDeltaOptions) -> f64 {
    if dy == 0.0 || !dy.is_finite() {
        return 0.0;
    }
    let multiplier = if opts.multiplier.is_finite() && opts.multiplier > 0.0 {
        opts.multiplier
    } else {
        1.0
    };
    match delta_mode {
        DeltaMode::Line => -dy * multiplier,
        DeltaMode::Page => {
            let page_rows = (f64::from(opts.rows) - 2.0).max(1.0);
            -dy * page_rows * multiplier
        }
        DeltaMode::Pixel => {
            let px_per_line = match opts.row_px {
                Some(row_px) if row_px.is_finite() && row_px > 0.0 => {
                    row_px.clamp(MIN_ROW_PX, MAX_ROW_PX)
                }
                _ => DEFAULT_WHEEL_PX_PER_LINE,
            };
            -(dy / px_per_line) * multiplier
        }
    }
}

/// Sub-line wheel accumulator and discrete-notch momentum stepper for terminal scrollback
/// (`TerminalScroller` in `wall/terminalScroll.js`).
#[derive(Clone, Debug)]
pub struct TerminalScroller {
    id: Option<String>,
    remainder: f64,
    pending: f64,
    trackpad: bool,
    last_at_ms: f64,
    cell: Point,
}

impl Default for TerminalScroller {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalScroller {
    /// Create an idle scroller with no buffered remainder or momentum glide.
    pub fn new() -> Self {
        Self {
            id: None,
            remainder: 0.0,
            pending: 0.0,
            trackpad: false,
            last_at_ms: f64::NEG_INFINITY,
            cell: Point::new(0.0, 0.0),
        }
    }

    /// Terminal ID currently holding sub-line remainder or momentum glide state, if any.
    pub fn active_id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Pointer cell coordinates recorded on the most recent wheel event.
    pub fn active_cell(&self) -> Point {
        self.cell
    }

    /// Whether discrete wheel-notch lines are still gliding across animation frames.
    pub fn gliding(&self) -> bool {
        self.pending.abs() >= EPS
    }

    /// Drop any buffered sub-line remainder and cancel in-flight momentum glide.
    pub fn reset(&mut self) {
        self.id = None;
        self.remainder = 0.0;
        self.pending = 0.0;
        self.trackpad = false;
        self.last_at_ms = f64::NEG_INFINITY;
    }

    /// Feed a vertical wheel event on terminal `id` at pointer `cell` and timestamp
    /// `now_ms`, returning the signed whole lines to emit synchronously (`0` if none).
    pub fn wheel(
        &mut self,
        id: &str,
        dy: f64,
        delta_mode: DeltaMode,
        cell: Point,
        now_ms: f64,
        opts: WheelOptions,
    ) -> i32 {
        if id.is_empty() || dy == 0.0 || !dy.is_finite() {
            return 0;
        }
        if self.id.as_deref() != Some(id) {
            self.reset();
            self.id = Some(id.to_owned());
        } else if now_ms - self.last_at_ms > STREAM_IDLE_MS && !self.gliding() {
            self.remainder = 0.0;
            self.trackpad = false;
        }

        let delta = wheel_delta_lines(dy, delta_mode, opts.delta_options());
        if delta == 0.0 {
            return 0;
        }

        let total = self.remainder + self.pending;
        if total != 0.0 && js_sign(delta) != js_sign(total) {
            self.remainder = 0.0;
            self.pending = 0.0;
        }

        let sub_notch_pixel =
            delta_mode == DeltaMode::Pixel && (dy.abs() < NOTCH_MIN_PX || dy.fract() != 0.0);
        if sub_notch_pixel {
            self.trackpad = true;
            if self.pending != 0.0 {
                self.remainder += self.pending;
                self.pending = 0.0;
            }
        } else if delta_mode != DeltaMode::Pixel {
            self.trackpad = false;
        }

        self.last_at_ms = now_ms;
        self.cell = cell;

        if self.trackpad || !opts.glide || delta.abs() <= 1.25 {
            self.remainder += delta;
            return self.take_whole();
        }

        let was_idle = self.pending.abs() < 0.5;
        self.pending += delta;
        if was_idle && (self.pending + self.remainder).abs() >= 1.0 - EPS {
            let lead = if delta > 0.0 { 1 } else { -1 };
            self.pending -= f64::from(lead);
            return lead;
        }
        0
    }

    /// Feed a vertical wheel event like [`Self::wheel`], invoking `on_scroll(id, lines, cell)`
    /// synchronously whenever non-zero whole lines are emitted.
    #[expect(
        clippy::too_many_arguments,
        reason = "matches wheel() plus the explicit on_scroll sink"
    )]
    pub fn wheel_with(
        &mut self,
        id: &str,
        dy: f64,
        delta_mode: DeltaMode,
        cell: Point,
        now_ms: f64,
        opts: WheelOptions,
        mut on_scroll: impl FnMut(&str, i32, Point),
    ) -> i32 {
        let whole = self.wheel(id, dy, delta_mode, cell, now_ms, opts);
        if whole != 0 {
            on_scroll(id, whole, self.cell);
        }
        whole
    }

    /// Advance any in-flight discrete-notch glide by `dt` seconds, returning the signed
    /// whole lines emitted on this frame (`0` if none).
    pub fn step(&mut self, dt: f64) -> i32 {
        if self.id.is_none() || self.pending.abs() < EPS {
            return 0;
        }
        if !dt.is_finite() || dt <= 0.0 {
            return 0;
        }
        let settle_factor = 1.0 - (-dt * GLIDE_SETTLE_RATE).exp();
        let done = self.pending.abs() < 0.15;
        let take = if done {
            self.pending
        } else {
            self.pending * settle_factor
        };
        self.pending = if done { 0.0 } else { self.pending - take };
        self.remainder += take;
        self.take_whole()
    }

    /// Advance any in-flight glide like [`Self::step`], invoking `on_scroll(id, lines, cell)`
    /// whenever non-zero whole lines are emitted on this frame.
    pub fn step_with(&mut self, dt: f64, mut on_scroll: impl FnMut(&str, i32, Point)) -> i32 {
        let whole = self.step(dt);
        if whole != 0 {
            if let Some(id) = self.id.as_deref() {
                on_scroll(id, whole, self.cell);
            }
        }
        whole
    }

    fn take_whole(&mut self) -> i32 {
        let biased = self.remainder + js_sign(self.remainder) * EPS;
        let whole_float = biased.trunc();
        if whole_float == 0.0 {
            return 0;
        }
        self.remainder -= whole_float;
        if self.remainder.abs() < EPS {
            self.remainder = 0.0;
        }
        whole_float as i32
    }
}

/// Modifier flags inspected by [`WheelStreamGate::accept`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AcceptOptions {
    /// Whether the event's modifiers ask for camera zoom (`chord::zooms_wheel`).
    pub zoom: bool,
    /// Whether `Ctrl` is set on the wheel event (how OS trackpad pinches arrive).
    pub ctrl_key: bool,
}

/// Decision returned by [`WheelStreamGate::accept`] for an incoming wheel event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateDecision {
    /// `false` when the event belongs to a quenched pre-transition inertial tail.
    pub ok: bool,
    /// Effective zoom flag for this wheel stream after mid-stream modifier guarding.
    pub zoom: bool,
}

/// Latched target for a continuous plain-wheel gesture on the canvas.
#[derive(Clone, Debug, PartialEq)]
pub enum CanvasWheelTarget {
    /// Route the wheel stream to terminal `id`, latched at client coordinates `(x, y)`.
    Terminal { id: String, x: f64, y: f64 },
    /// Route the wheel stream to canvas panning, latched at client coordinates `(x, y)`.
    Pan { x: f64, y: f64 },
}

#[derive(Clone, Copy, Debug)]
struct QuenchState {
    started_at_ms: f64,
    min_magnitude: f64,
    velocity_x: f64,
    velocity_y: f64,
}

/// Tracks continuous OS wheel and trackpad streams across fullscreen/framing transitions,
/// mid-stream modifier presses, and sliding canvas targets (`WheelStreamGate` in
/// `wall/terminalScroll.js`).
#[derive(Clone, Debug)]
pub struct WheelStreamGate {
    last_at_ms: f64,
    last_dx: f64,
    last_dy: f64,
    last_magnitude: f64,
    stream_zoom: Option<bool>,
    quench: Option<QuenchState>,
    latch: Option<CanvasWheelTarget>,
}

impl Default for WheelStreamGate {
    fn default() -> Self {
        Self::new()
    }
}

impl WheelStreamGate {
    /// Create a fresh stream gate with no active stream, quench, or target latch.
    pub fn new() -> Self {
        Self {
            last_at_ms: f64::NEG_INFINITY,
            last_dx: 0.0,
            last_dy: 0.0,
            last_magnitude: 0.0,
            stream_zoom: None,
            quench: None,
            latch: None,
        }
    }

    /// Clear all stream history, active quench state, and target latching.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Quench any wheel stream active within `GATE_QUIET_MS` of `now_ms` (called when
    /// entering or leaving fullscreen or framing the camera) so its inertial tail is
    /// absorbed instead of zooming or panning the newly transitioned view.
    pub fn quench(&mut self, now_ms: f64) {
        self.latch = None;
        if now_ms - self.last_at_ms <= GATE_QUIET_MS {
            self.quench = Some(QuenchState {
                started_at_ms: now_ms,
                min_magnitude: if self.last_magnitude > 0.0 {
                    self.last_magnitude
                } else {
                    f64::INFINITY
                },
                velocity_x: self.last_dx,
                velocity_y: self.last_dy,
            });
        } else {
            self.quench = None;
        }
    }

    /// Inspect a pixel-scaled `(dx, dy)` wheel event at `now_ms`.
    ///
    /// Returns `ok: false` while absorbing a quenched pre-transition inertial tail;
    /// otherwise returns `ok: true` with the effective `zoom` flag (blocking `⌘` or `⌥`
    /// pressed mid-pan from promoting a coasting scroll into camera zoom, while always
    /// allowing `ctrl_key` trackpad pinches).
    pub fn accept(
        &mut self,
        dx: f64,
        dy: f64,
        now_ms: f64,
        opts: AcceptOptions,
    ) -> GateDecision {
        let dt = now_ms - self.last_at_ms;
        let magnitude = dx.hypot(dy);
        if let Some(mut quench) = self.quench {
            if dt > GATE_QUIET_MS {
                self.quench = None;
                self.stream_zoom = None;
            } else {
                let age = now_ms - quench.started_at_ms;
                let reversed =
                    age > 120.0 && (dx * quench.velocity_x + dy * quench.velocity_y) < -16.0;
                let regripped = age > 250.0 && quench.min_magnitude < 4.0 && magnitude > 36.0;
                if reversed || regripped {
                    self.quench = None;
                    self.stream_zoom = None;
                } else {
                    if magnitude > 0.0 {
                        quench.min_magnitude = quench.min_magnitude.min(magnitude);
                        quench.velocity_x = dx;
                        quench.velocity_y = dy;
                    }
                    self.quench = Some(quench);
                    self.last_at_ms = now_ms;
                    self.last_dx = dx;
                    self.last_dy = dy;
                    self.last_magnitude = magnitude;
                    return GateDecision {
                        ok: false,
                        zoom: false,
                    };
                }
            }
        }

        let mut effective_zoom = opts.zoom;
        if dt > GATE_QUIET_MS || self.stream_zoom.is_none() {
            self.latch = None;
            self.stream_zoom = Some(effective_zoom);
        } else if opts.ctrl_key {
            // Trackpad pinch arrives as ctrl_key + wheel and may begin mid-pan: always honour it.
            self.stream_zoom = Some(effective_zoom);
        } else if self.stream_zoom == Some(false) && effective_zoom {
            // Pressing ⌘ or ⌥ mid-momentum (e.g. preparing ⌘↵) must never turn a coasting scroll into zoom.
            effective_zoom = false;
        }

        self.last_at_ms = now_ms;
        self.last_dx = dx;
        self.last_dy = dy;
        self.last_magnitude = magnitude;
        GateDecision {
            ok: true,
            zoom: effective_zoom,
        }
    }

    /// Resolve or keep the latched target for a continuous plain-wheel gesture at client
    /// coordinates `(client_x, client_y)`.
    ///
    /// While the pointer stays within 24 px of where the latch was set, the initial
    /// target (`Terminal` vs `Pan`) holds even if content slides under the stationary
    /// cursor or focus changes.
    pub fn canvas_target(
        &mut self,
        client_x: f64,
        client_y: f64,
        hit_focused_id: Option<&str>,
    ) -> CanvasWheelTarget {
        if let Some(existing) = &self.latch {
            let (latched_x, latched_y) = match existing {
                CanvasWheelTarget::Terminal { x, y, .. } | CanvasWheelTarget::Pan { x, y } => {
                    (*x, *y)
                }
            };
            if (client_x - latched_x).hypot(client_y - latched_y) <= LATCH_SLOP_PX {
                return existing.clone();
            }
        }
        let target = match hit_focused_id.filter(|id| !id.is_empty()) {
            Some(id) => CanvasWheelTarget::Terminal {
                id: id.to_owned(),
                x: client_x,
                y: client_y,
            },
            None => CanvasWheelTarget::Pan {
                x: client_x,
                y: client_y,
            },
        };
        self.latch = Some(target.clone());
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[derive(Clone, Debug, PartialEq)]
    struct RecordedScroll {
        id: String,
        lines: i32,
        x: f64,
        y: f64,
    }

    #[test]
    fn wheel_delta_lines_scales_modes_and_clamps_extreme_row_px() {
        let row_18 = WheelDeltaOptions {
            row_px: Some(18.0),
            ..Default::default()
        };
        assert_eq!(wheel_delta_lines(0.0, DeltaMode::Pixel, row_18), 0.0);
        assert_eq!(wheel_delta_lines(-18.0, DeltaMode::Pixel, row_18), 1.0);
        assert!(near(wheel_delta_lines(9.0, DeltaMode::Pixel, row_18), -0.5));
        assert_eq!(
            wheel_delta_lines(
                -18.0,
                DeltaMode::Pixel,
                WheelDeltaOptions {
                    row_px: Some(18.0),
                    multiplier: 2.0,
                    ..Default::default()
                }
            ),
            2.0
        );

        // Clamped to [12, 36] px/line so zoomed-out micro-terminals do not rocket.
        assert_eq!(
            wheel_delta_lines(
                -12.0,
                DeltaMode::Pixel,
                WheelDeltaOptions {
                    row_px: Some(2.0),
                    ..Default::default()
                }
            ),
            1.0
        );
        assert_eq!(
            wheel_delta_lines(
                -36.0,
                DeltaMode::Pixel,
                WheelDeltaOptions {
                    row_px: Some(90.0),
                    ..Default::default()
                }
            ),
            1.0
        );

        // DeltaMode::Line (1) and DeltaMode::Page (2, pages of rows - 2).
        assert!(near(
            wheel_delta_lines(
                -3.0,
                DeltaMode::Line,
                WheelDeltaOptions {
                    multiplier: 1.5,
                    ..Default::default()
                }
            ),
            4.5
        ));
        assert_eq!(
            wheel_delta_lines(
                1.0,
                DeltaMode::Page,
                WheelDeltaOptions {
                    rows: 24,
                    ..Default::default()
                }
            ),
            -22.0
        );
    }

    #[test]
    fn trackpad_streams_accumulate_sub_line_fractions_and_taper_tails_cleanly() {
        let mut events: Vec<RecordedScroll> = Vec::new();
        let mut scroller = TerminalScroller::new();
        let cell = Point::new(4.0, 5.0);
        let opts = WheelOptions {
            row_px: Some(18.0),
            glide: true,
            ..Default::default()
        };
        let mut now = 1000.0;

        // Slow 3 px nudges at row_px = 18: 5 nudges (15 px) emit nothing; 6th (18 px) steps +1 line.
        for _ in 0..5 {
            now += 8.0;
            let emitted =
                scroller.wheel_with("t1", -3.0, DeltaMode::Pixel, cell, now, opts, |id, lines, at| {
                    events.push(RecordedScroll {
                        id: id.to_owned(),
                        lines,
                        x: at.x,
                        y: at.y,
                    });
                });
            assert_eq!(emitted, 0);
        }
        assert!(events.is_empty());

        now += 8.0;
        let sixth =
            scroller.wheel_with("t1", -3.0, DeltaMode::Pixel, cell, now, opts, |id, lines, at| {
                events.push(RecordedScroll {
                    id: id.to_owned(),
                    lines,
                    x: at.x,
                    y: at.y,
                });
            });
        assert_eq!(sixth, 1);
        assert_eq!(
            events,
            vec![RecordedScroll {
                id: "t1".to_owned(),
                lines: 1,
                x: 4.0,
                y: 5.0,
            }]
        );

        // Fast trackpad flick + OS momentum tail (including peak > 80 px mid-stream): stays direct,
        // zero synthetic lag, and tapers smoothly at the tail instead of firing 1 line per 1 px event.
        events.clear();
        let flick = [-20.0, -96.0, -54.0, -18.0, -8.0, -5.0, -3.0, -2.0]; // -206 px -> 11.444 lines -> 11 whole lines
        for dy in flick {
            now += 8.0;
            scroller.wheel_with("t1", dy, DeltaMode::Pixel, cell, now, opts, |id, lines, at| {
                events.push(RecordedScroll {
                    id: id.to_owned(),
                    lines,
                    x: at.x,
                    y: at.y,
                });
            });
        }
        assert!(!scroller.gliding());
        let total: i32 = events.iter().map(|e| e.lines).sum();
        assert_eq!(total, 11);
        assert!(events.iter().all(|e| e.lines > 0));

        // Idle gap > STREAM_IDLE_MS clears leftover 0.444 remainder so a fresh 12 px nudge (< 18 px)
        // does not jump early.
        events.clear();
        let after_idle = scroller.wheel_with(
            "t1",
            -12.0,
            DeltaMode::Pixel,
            cell,
            now + STREAM_IDLE_MS + 20.0,
            opts,
            |id, lines, at| {
                events.push(RecordedScroll {
                    id: id.to_owned(),
                    lines,
                    x: at.x,
                    y: at.y,
                });
            },
        );
        assert_eq!(after_idle, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn discrete_wheel_notches_step_lead_line_glide_with_conservation_and_cancel_on_reversal() {
        let mut events: Vec<(String, i32)> = Vec::new();
        let mut scroller = TerminalScroller::new();
        let cell = Point::new(2.0, 3.0);

        // A -108 px mouse notch at row_px = 18 asks for +6 lines: +1 immediately, +5 paced across step(dt).
        let lead = scroller.wheel_with(
            "t1",
            -108.0,
            DeltaMode::Pixel,
            cell,
            100.0,
            WheelOptions {
                row_px: Some(18.0),
                glide: true,
                ..Default::default()
            },
            |id, lines, _| events.push((id.to_owned(), lines)),
        );
        assert_eq!(lead, 1);
        assert!(scroller.gliding());
        assert_eq!(events.iter().map(|(_, l)| *l).collect::<Vec<_>>(), vec![1]);

        let mut safety = 0;
        while scroller.gliding() && safety < 60 {
            safety += 1;
            scroller.step_with(1.0 / 60.0, |id, lines, _| events.push((id.to_owned(), lines)));
        }
        assert!(!scroller.gliding());
        assert!(events.len() > 2); // paced across multiple frames, not dumped at once
        assert_eq!(events.iter().map(|(_, l)| *l).sum::<i32>(), 6);

        // Stacking two rapid notches (+4 lines each in DeltaMode::Line) lands at exact +8 lines.
        events.clear();
        let line_glide = WheelOptions {
            glide: true,
            ..Default::default()
        };
        scroller.wheel_with("t1", -4.0, DeltaMode::Line, cell, 500.0, line_glide, |id, l, _| {
            events.push((id.to_owned(), l));
        });
        scroller.step_with(1.0 / 60.0, |id, l, _| events.push((id.to_owned(), l)));
        scroller.wheel_with("t1", -4.0, DeltaMode::Line, cell, 520.0, line_glide, |id, l, _| {
            events.push((id.to_owned(), l));
        });
        safety = 0;
        while scroller.gliding() && safety < 60 {
            safety += 1;
            scroller.step_with(1.0 / 60.0, |id, l, _| events.push((id.to_owned(), l)));
        }
        assert_eq!(events.iter().map(|(_, l)| *l).sum::<i32>(), 8);

        // Reversing direction mid-glide drops pending upward momentum immediately and steps downward.
        events.clear();
        scroller.wheel_with("t1", -6.0, DeltaMode::Line, cell, 900.0, line_glide, |id, l, _| {
            events.push((id.to_owned(), l));
        });
        assert!(scroller.gliding());
        let rev =
            scroller.wheel_with("t1", 4.0, DeltaMode::Line, cell, 910.0, line_glide, |id, l, _| {
                events.push((id.to_owned(), l));
            });
        assert_eq!(rev, -1);
        while scroller.gliding() {
            scroller.step_with(1.0 / 60.0, |id, l, _| events.push((id.to_owned(), l)));
        }
        assert_eq!(events[1..].iter().map(|(_, l)| *l).sum::<i32>(), -4);
    }

    #[test]
    fn zero_vertical_delta_emits_nothing_and_reset_halts_momentum() {
        let mut scroller = TerminalScroller::new();
        let cell = Point::new(5.0, 6.0);
        let opts = WheelOptions {
            row_px: Some(20.0),
            glide: true,
            ..Default::default()
        };

        assert_eq!(scroller.wheel("t1", 0.0, DeltaMode::Pixel, cell, 100.0, opts), 0);
        assert_eq!(scroller.wheel("t1", -100.0, DeltaMode::Pixel, cell, 110.0, opts), 1);
        assert!(scroller.gliding());

        // Resetting (e.g. when typing a key or changing focus) cancels remaining momentum.
        scroller.reset();
        assert!(!scroller.gliding());
        for _ in 0..10 {
            assert_eq!(scroller.step(1.0 / 60.0), 0);
        }
    }

    #[test]
    fn wheel_stream_gate_quenches_transition_tail_and_guards_mid_stream_modifiers() {
        let mut gate = WheelStreamGate::new();
        let mut t = 1000.0;

        // Opt+fling starts accelerating in fullscreen before transition.
        t += 8.0;
        assert_eq!(
            gate.accept(
                0.0,
                -14.0,
                t,
                AcceptOptions {
                    zoom: true,
                    ctrl_key: false,
                }
            ),
            GateDecision {
                ok: true,
                zoom: true,
            }
        );
        t += 8.0;
        assert_eq!(
            gate.accept(
                0.0,
                -28.0,
                t,
                AcceptOptions {
                    zoom: true,
                    ctrl_key: false,
                }
            ),
            GateDecision {
                ok: true,
                zoom: true,
            }
        );

        // Exiting fullscreen quenches the active stream even as the fling crests and decays
        // with meta/alt modifiers still riding the inertial events.
        gate.quench(t);
        for (dy, zoom) in [
            (-75.0, true),
            (-90.0, true),
            (-64.0, true),
            (-42.0, true),
            (-24.0, false),
            (-10.0, false),
            (-3.0, false),
        ] {
            t += 8.0;
            assert_eq!(
                gate.accept(
                    0.0,
                    dy,
                    t,
                    AcceptOptions {
                        zoom,
                        ctrl_key: false,
                    }
                ),
                GateDecision {
                    ok: false,
                    zoom: false,
                }
            );
        }

        // After > 120 ms, a deliberate fresh flick in the opposite direction (+40) releases quench.
        t += 130.0;
        assert_eq!(
            gate.accept(0.0, 40.0, t, AcceptOptions::default()),
            GateDecision {
                ok: true,
                zoom: false,
            }
        );

        // Plain fling on canvas, then pressing ⌘ mid-momentum: does NOT promote to zoom,
        // whereas a genuine Ctrl+wheel trackpad pinch DOES zoom.
        t += STREAM_IDLE_MS + 50.0;
        t += 8.0;
        assert_eq!(
            gate.accept(0.0, 30.0, t, AcceptOptions::default()),
            GateDecision {
                ok: true,
                zoom: false,
            }
        );
        t += 8.0;
        assert_eq!(
            gate.accept(
                0.0,
                24.0,
                t,
                AcceptOptions {
                    zoom: true,
                    ctrl_key: false,
                }
            ),
            GateDecision {
                ok: true,
                zoom: false,
            }
        );
        t += 8.0;
        assert_eq!(
            gate.accept(
                0.0,
                -20.0,
                t,
                AcceptOptions {
                    zoom: true,
                    ctrl_key: true,
                }
            ),
            GateDecision {
                ok: true,
                zoom: true,
            }
        );

        // After STREAM_IDLE_MS of quiet, normal plain scrolling works right away.
        t += STREAM_IDLE_MS + 20.0;
        assert_eq!(
            gate.accept(0.0, -40.0, t, AcceptOptions::default()),
            GateDecision {
                ok: true,
                zoom: false,
            }
        );
    }

    #[test]
    fn canvas_wheel_target_latching_holds_within_slop_and_rebinds_past_slop() {
        let mut gate = WheelStreamGate::new();
        let mut t = 500.0;

        // Start a pan on empty canvas at (300, 300).
        t += 8.0;
        assert!(gate.accept(0.0, 30.0, t, AcceptOptions::default()).ok);
        assert_eq!(
            gate.canvas_target(300.0, 300.0, None),
            CanvasWheelTarget::Pan { x: 300.0, y: 300.0 }
        );

        // Focused terminal "t1" slides underneath stationary cursor (300, 300) mid-pan: stays Pan.
        t += 8.0;
        assert!(gate.accept(0.0, 24.0, t, AcceptOptions::default()).ok);
        assert_eq!(
            gate.canvas_target(300.0, 300.0, Some("t1")),
            CanvasWheelTarget::Pan { x: 300.0, y: 300.0 }
        );

        // Physically moving the mouse pointer > 24 px onto "t1" re-evaluates immediately.
        t += 8.0;
        assert!(gate.accept(0.0, 20.0, t, AcceptOptions::default()).ok);
        assert_eq!(
            gate.canvas_target(340.0, 300.0, Some("t1")),
            CanvasWheelTarget::Terminal {
                id: "t1".to_owned(),
                x: 340.0,
                y: 300.0,
            }
        );
    }

    #[test]
    fn regrip_releases_quench_and_switching_terminals_resets_scroller() {
        // Default 30 px/line fallback when row_px is None.
        assert_eq!(
            wheel_delta_lines(-30.0, DeltaMode::Pixel, WheelDeltaOptions::default()),
            1.0
        );
        assert_eq!(
            wheel_delta_lines(f64::NAN, DeltaMode::Pixel, WheelDeltaOptions::default()),
            0.0
        );

        // Switching terminal ID drops leftover sub-line remainder from the previous terminal.
        let mut scroller = TerminalScroller::new();
        let cell = Point::new(1.0, 1.0);
        let opts = WheelOptions {
            row_px: Some(18.0),
            glide: true,
            ..Default::default()
        };
        assert_eq!(scroller.wheel("t1", -15.0, DeltaMode::Pixel, cell, 10.0, opts), 0);
        assert_eq!(scroller.active_id(), Some("t1"));
        assert_eq!(scroller.wheel("t2", -6.0, DeltaMode::Pixel, cell, 18.0, opts), 0);
        assert_eq!(scroller.active_id(), Some("t2"));

        // Regripping after > 250 ms once the quenched stream decayed near rest (< 4 px)
        // releases the quench on a strong new flick (> 36 px) in the same direction.
        let mut gate = WheelStreamGate::new();
        assert!(gate.accept(0.0, -30.0, 100.0, AcceptOptions::default()).ok);
        gate.quench(105.0);
        assert!(!gate.accept(0.0, -2.0, 200.0, AcceptOptions::default()).ok);
        assert!(!gate.accept(0.0, -1.0, 320.0, AcceptOptions::default()).ok);
        assert_eq!(
            gate.accept(0.0, -45.0, 370.0, AcceptOptions::default()),
            GateDecision {
                ok: true,
                zoom: false,
            }
        );
    }
}
