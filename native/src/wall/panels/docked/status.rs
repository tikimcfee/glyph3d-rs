//! `termd` status window, topbar status chip, and under-topbar outage notice (`wall/StatusWindow.jsx`),
//! driven by [`wall_core::status::TermdStatus`] and emitting `wall.status`, `wall.status.dismiss`,
//! and `wall.reveal <id>` verbs.

use std::collections::BTreeMap;

use wall_core::chord::chord_label;
use wall_core::dock::DOCK_WIDTHS;
use wall_core::input::Platform;
use wall_core::status::{
    chip_look, empty_daemon_message, format_bytes, format_name_version, format_span, notice_for,
    terminal_rows, DaemonStatus, LeftoverStatus, StatusError, StatusNotice, StatusTerminalRow,
    TermdState, TermdStatus, DEFAULT_RESTART_NOTICE_MS,
};
use wall_core::store::{PanelSide, StatusWindowState as StatusPlacement};

use super::{
    dock_buttons_rtl, handle_head_drag, panel_viewport_rect, resolve_panel_box, status_panel_bg,
    tone_color, HeadDragState, AMBER, BORDER, BRIGHT, HEAD_BORDER, HOVER_ROW_BG, MUTED, NOTICE_BG,
    TEXT,
};

/// Plain data read by [`show`] (`<Panel>Input` in `P-panels.md`).
#[derive(Clone, Debug)]
pub struct StatusInput {
    /// Open flag, side (`Left`/`Right`/`Float`), and floating coordinates of the status window.
    pub placement: StatusPlacement,
    /// Current `g3d-termd` status document and connection state (`wall_core::status::TermdStatus`).
    pub status: TermdStatus,
    /// Optional pre-computed terminal rows; when `None`, built from `status` and `wall_names`.
    pub rows: Option<Vec<StatusTerminalRow>>,
    /// Display names of terminals currently on the wall (`id -> display_name`).
    /// Clicking a terminal present in `wall_names` emits `wall.reveal <id>`.
    pub wall_names: BTreeMap<String, String>,
    /// Current wall-clock timestamp in epoch milliseconds, used for uptime, idle, and notice spans.
    pub now_ms: u64,
    /// Grace in milliseconds before `Restarting` shows an amber notice (`WallConfig::restart_notice_ms`, `400`).
    pub restart_notice_ms: u64,
    /// Maximum number of recent daemon errors shown (`WallConfig::errors_shown`, `3`).
    pub errors_shown: usize,
    /// Screen-space rectangle assigned by `wall_core::dock` when docked `Left` or `Right`.
    pub dock_rect: Option<egui::Rect>,
    /// Standalone topbar chip position (`Some(pos)` draws the `termd` chip at `pos`;
    /// set `None` if a shared topbar embeds [`show_chip_ui`] inline).
    pub chip_pos: Option<egui::Pos2>,
    /// Horizontal `(left, right)` chrome insets from `wall_core::dock::dock_layout` used to center
    /// the under-topbar notice between docked panels.
    pub chrome_inset: (f32, f32),
    /// Whether `show` should render its own `egui` outage notice banner (`true` by default;
    /// callers whose canvas chrome renders `WallFrame::status_notice` set `false` to avoid
    /// drawing two stacked banners).
    pub show_notice: bool,
    /// Host platform (`Mac` vs `Linux`/`Other`) for `⌘S` / `Ctrl+Alt+S` tooltips.
    pub platform: Platform,
}

impl Default for StatusInput {
    fn default() -> Self {
        Self {
            placement: StatusPlacement::default(),
            status: TermdStatus::default(),
            rows: None,
            wall_names: BTreeMap::new(),
            now_ms: 0,
            restart_notice_ms: DEFAULT_RESTART_NOTICE_MS,
            errors_shown: 3,
            dock_rect: None,
            chip_pos: Some(egui::pos2(12.0, 10.0)),
            chrome_inset: (12.0, 12.0),
            show_notice: true,
            platform: Platform::host(),
        }
    }
}

impl StatusInput {
    /// Build a [`StatusInput`] from placement and `TermdStatus` snapshots.
    pub fn new(placement: StatusPlacement, status: TermdStatus, now_ms: u64) -> Self {
        Self {
            placement,
            status,
            now_ms,
            ..Self::default()
        }
    }
}

/// Alias matching `StatusWindow` / `StatusPanel` naming.
pub type StatusPanelInput = StatusInput;

/// Persistent UI view state kept by the caller across frames (`<Panel>State` in `P-panels.md`).
#[derive(Clone, Debug, Default)]
pub struct StatusState {
    /// Whether the status UI wants keyboard input this frame (`false`: status widgets never steal focus).
    pub wants_keyboard: bool,
    /// Floating header drag state (`useHeadDrag` in `Panel.jsx`).
    pub head_drag: HeadDragState,
    /// Latched `now_ms` when brain trouble began if `status.since_ms()` was zero (`useTroubleSince`).
    pub trouble_since_ms: Option<u64>,
    /// Measured content width for floating placement so the window hugs its table instead of spanning `100vw`.
    float_width: Option<f32>,
    /// Last rendered window bounding box when open.
    pub last_rect: Option<egui::Rect>,
    /// Floating header drag rectangle when open.
    pub header_drag_rect: Option<egui::Rect>,
    /// Terminal ids rendered in the status table on the last open pass.
    pub shown_row_ids: Vec<String>,
}

/// Alias matching `StatusPanel` naming.
pub type StatusPanelState = StatusState;

impl StatusState {
    /// Whether the status UI wants keyboard input routed to egui instead of a terminal (`false`).
    pub const fn wants_keyboard(&self) -> bool {
        self.wants_keyboard
    }

    /// Effective floating width in points (`StatusWindow` hugs its measured table width).
    pub fn effective_float_width(&self, max_float_w: f32) -> f32 {
        self.float_width
            .unwrap_or(580.0)
            .clamp(300.0_f32.min(max_float_w), max_float_w.min(760.0))
    }
}

/// Format an epoch-millisecond timestamp as `HH:MM` UTC (`clock(ms)` in `StatusWindow.jsx`).
fn format_clock_utc(epoch_ms: u64) -> String {
    let total_secs = epoch_ms / 1000;
    let hours = (total_secs / 3600) % 24;
    let mins = (total_secs / 60) % 60;
    format!("{hours:02}:{mins:02}")
}

/// Draw the `termd` status chip, under-topbar outage notice, and status window (`wall/StatusWindow.jsx`).
///
/// Returns `false` (`state.wants_keyboard = false`) because status buttons never claim keyboard focus.
pub fn show(
    ctx: &egui::Context,
    state: &mut StatusState,
    input: &StatusInput,
    out: &mut Vec<String>,
) -> bool {
    state.wants_keyboard = false;

    if !input.status.state().is_trouble() {
        state.trouble_since_ms = None;
    } else if state.trouble_since_ms.is_none() {
        state.trouble_since_ms = Some(input.now_ms);
    }

    if let Some(chip_pos) = input.chip_pos {
        egui::Area::new(egui::Id::new("wall_termd_chip"))
            .fixed_pos(chip_pos)
            .order(egui::Order::Foreground)
            .constrain(false)
            .show(ctx, |ui| {
                show_chip_ui(ui, &input.status, input.platform, out);
            });
    }

    let since_ms = if input.status.since_ms() > 0 {
        input.status.since_ms()
    } else {
        state.trouble_since_ms.unwrap_or(input.now_ms)
    };
    if input.show_notice {
        if let Some(notice) =
            notice_for(&input.status, since_ms, input.now_ms, input.restart_notice_ms)
        {
            show_notice(ctx, &notice, input.chrome_inset, out);
        }
    }

    if input.placement.open {
        show_window(ctx, state, input, out);
    } else {
        state.head_drag.drag_at = None;
        state.last_rect = None;
        state.header_drag_rect = None;
        state.shown_row_ids.clear();
    }

    false
}

/// Render the `TermdChip` inline inside any `egui::Ui` (e.g. a shared topbar or standalone `Area`).
///
/// Clicking emits `wall.status toggle` without claiming keyboard focus.
pub fn show_chip_ui(
    ui: &mut egui::Ui,
    status: &TermdStatus,
    platform: Platform,
    out: &mut Vec<String>,
) -> egui::Response {
    let look = chip_look(status);
    let mark_color = tone_color(look.tone);
    let text_color = if status.state() == TermdState::Ok {
        TEXT
    } else {
        mark_color
    };

    let mut job = egui::text::LayoutJob::default();
    job.append(
        look.mark,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: mark_color,
            ..Default::default()
        },
    );
    job.append(
        " ",
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: text_color,
            ..Default::default()
        },
    );
    job.append(
        &look.text,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: text_color,
            ..Default::default()
        },
    );

    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let size = egui::vec2(galley.size().x + 12.0, (galley.size().y + 8.0).max(22.0));
    let (_id, rect) = ui.allocate_space(size);
    let tooltip = format!("termd's status ({})", chord_label(platform, &["S"], false));
    let resp = ui
        .interact(
            rect,
            ui.make_persistent_id("wall_termd_chip_btn"),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip);

    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), HOVER_ROW_BG);
        }
        ui.painter().galley(
            egui::pos2(
                rect.left() + 6.0,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            text_color,
        );
    }

    if resp.clicked() {
        out.push("wall.status toggle".to_string());
    }
    resp
}

/// Render the under-topbar `TermdNotice` banner (`TermdNotice` in `StatusWindow.jsx`).
///
/// Clicking `×` on a dismissible (`lost`) notice emits `wall.status.dismiss`.
pub fn show_notice(
    ctx: &egui::Context,
    notice: &StatusNotice,
    chrome_inset: (f32, f32),
    out: &mut Vec<String>,
) {
    let viewport = panel_viewport_rect(ctx);
    let span_left = viewport.left() + chrome_inset.0;
    let span_right = (viewport.right() - chrome_inset.1).max(span_left + 80.0);
    let center_x = (span_left + span_right) * 0.5;
    let max_w = (span_right - span_left).max(80.0).min((viewport.width() - 32.0).max(80.0));
    let color = tone_color(notice.tone);

    egui::Area::new(egui::Id::new("wall_termd_notice"))
        .anchor(
            egui::Align2::CENTER_TOP,
            egui::vec2(center_x - viewport.center().x, 46.0),
        )
        .order(egui::Order::Tooltip)
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(NOTICE_BG)
                .stroke(egui::Stroke::new(1.0, BORDER))
                .corner_radius(egui::CornerRadius::same(5))
                .inner_margin(egui::Margin::symmetric(12, 5))
                .show(ui, |ui| {
                    ui.set_max_width(max_w);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                        ui.label(
                            egui::RichText::new(&notice.text)
                                .font(egui::FontId::monospace(12.0))
                                .color(color),
                        );
                        if notice.dismiss {
                            let (_id, rect) = ui.allocate_space(egui::vec2(16.0, 16.0));
                            let resp = ui
                                .interact(
                                    rect,
                                    ui.make_persistent_id("wall_termd_notice_dismiss"),
                                    egui::Sense::click(),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text("dismiss");
                            if ui.is_rect_visible(rect) {
                                let fg = if resp.hovered() { BRIGHT } else { MUTED };
                                let c = rect.center();
                                let stroke = egui::Stroke::new(1.25, fg);
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(c.x - 3.5, c.y - 3.5),
                                        egui::pos2(c.x + 3.5, c.y + 3.5),
                                    ],
                                    stroke,
                                );
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(c.x + 3.5, c.y - 3.5),
                                        egui::pos2(c.x - 3.5, c.y + 3.5),
                                    ],
                                    stroke,
                                );
                            }
                            if resp.clicked() {
                                out.push("wall.status.dismiss".to_string());
                            }
                        }
                    });
                });
        });
}

/// Render the `StatusWindow` panel (`StatusWindow` in `StatusWindow.jsx`).
pub fn show_window(
    ctx: &egui::Context,
    state: &mut StatusState,
    input: &StatusInput,
    out: &mut Vec<String>,
) {
    let viewport = panel_viewport_rect(ctx);
    let side = input.placement.side;
    let max_float_w = (viewport.width() - 32.0).max(160.0);
    let target_default_w = match side {
        PanelSide::Float => state
            .float_width
            .unwrap_or(580.0)
            .clamp(300.0_f32.min(max_float_w), max_float_w.min(760.0)),
        PanelSide::Left | PanelSide::Right => DOCK_WIDTHS.status as f32,
    };
    let (origin, width, max_height) = resolve_panel_box(
        viewport,
        side,
        input.dock_rect,
        input.placement.float,
        &state.head_drag,
        target_default_w,
    );

    let rounding = match side {
        PanelSide::Float => egui::CornerRadius::same(7),
        PanelSide::Left | PanelSide::Right => egui::CornerRadius::ZERO,
    };
    let frame_stroke = match side {
        PanelSide::Float => egui::Stroke::new(1.0, BORDER),
        PanelSide::Left | PanelSide::Right => egui::Stroke::NONE,
    };

    let look = chip_look(&input.status);
    let close_title = format!("close ({})", chord_label(input.platform, &["S"], false));
    let stale = input.status.is_stale();
    let computed_rows;
    let rows: &[StatusTerminalRow] = match &input.rows {
        Some(prebuilt) => prebuilt.as_slice(),
        None => {
            computed_rows = terminal_rows(&input.status, input.now_ms, |id| {
                input.wall_names.get(id).cloned()
            });
            computed_rows.as_slice()
        }
    };

    state.shown_row_ids = rows.iter().map(|r| r.id.clone()).collect();

    egui::Area::new(egui::Id::new("wall_status_window"))
        .fixed_pos(origin)
        .order(egui::Order::Foreground)
        .constrain(false)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_status initial sizing");
            }
            let panel_frame = egui::Frame::NONE
                .fill(status_panel_bg())
                .corner_radius(rounding)
                .stroke(frame_stroke);

            let frame_resp = panel_frame.show(ui, |ui| {
                ui.set_width(width);
                if side == PanelSide::Float {
                    ui.set_max_height(max_height);
                } else {
                    ui.set_min_height(max_height);
                    ui.set_max_height(max_height);
                }
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                // ── Header (`● termd [· state]` + `⇤ ◫ ⇥ ×`) ─────────────────
                let head_rect = egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 6,
                        top: 5,
                        bottom: 5,
                    })
                    .show(ui, |ui| {
                        let header_w = (width - 16.0).max(40.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(header_w, 18.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                                dock_buttons_rtl(ui, "wall.status", side, &close_title, out);

                                let mut title_job = egui::text::LayoutJob::default();
                                let look_col = tone_color(look.tone);
                                title_job.append(
                                    look.mark,
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::monospace(12.0),
                                        color: look_col,
                                        ..Default::default()
                                    },
                                );
                                title_job.append(
                                    " termd",
                                    0.0,
                                    egui::TextFormat {
                                        font_id: egui::FontId::monospace(12.0),
                                        color: MUTED,
                                        ..Default::default()
                                    },
                                );
                                if input.status.state() != TermdState::Ok {
                                    let state_word = if input.status.state() == TermdState::Absent {
                                        "idle"
                                    } else {
                                        input.status.state().as_str()
                                    };
                                    title_job.append(
                                        &format!(" · {state_word}"),
                                        0.0,
                                        egui::TextFormat {
                                            font_id: egui::FontId::monospace(12.0),
                                            color: look_col,
                                            ..Default::default()
                                        },
                                    );
                                }
                                let galley = ui.fonts_mut(|f| f.layout_job(title_job));
                                let avail = ui.available_width().max(galley.size().x + 8.0);
                                let (title_rect, title_resp) = ui.allocate_exact_size(
                                    egui::vec2(avail, 18.0),
                                    if side == PanelSide::Float {
                                        egui::Sense::drag()
                                    } else {
                                        egui::Sense::hover()
                                    },
                                );
                                ui.painter().galley(
                                    egui::pos2(
                                        title_rect.left(),
                                        title_rect.center().y - galley.size().y * 0.5,
                                    ),
                                    galley,
                                    MUTED,
                                );
                                state.header_drag_rect = Some(title_rect);
                                handle_head_drag(
                                    ui,
                                    &title_resp,
                                    side,
                                    origin,
                                    &mut state.head_drag,
                                    "wall.status",
                                    out,
                                );
                            },
                        );
                    })
                    .response
                    .rect;
                ui.painter().line_segment(
                    [
                        egui::pos2(head_rect.left(), head_rect.bottom()),
                        egui::pos2(head_rect.right(), head_rect.bottom()),
                    ],
                    egui::Stroke::new(1.0, HEAD_BORDER),
                );

                // ── Body (`Daemon`, `Terminals`, `Leftovers`, `Errors`) ──────
                let body_max_h = (max_height - 34.0).max(48.0);
                let inner_target_w = (width - 20.0).max(40.0);
                let mut measured_content_w = 0.0_f32;
                egui::ScrollArea::both()
                    .id_salt("wall_status_body")
                    .max_height(body_max_h)
                    .auto_shrink([false, side == PanelSide::Float])
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin {
                                left: 10,
                                right: 10,
                                top: 7,
                                bottom: 9,
                            })
                            .show(ui, |ui| {
                                if side != PanelSide::Float {
                                    ui.set_min_width(inner_target_w);
                                }
                                ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);

                                match input.status.doc() {
                                    None => {
                                        let r = ui.label(
                                            egui::RichText::new(empty_daemon_message(&input.status))
                                                .font(egui::FontId::monospace(12.0))
                                                .color(MUTED),
                                        );
                                        measured_content_w = r.rect.width();
                                    }
                                    Some(doc) => {
                                        let daemon_w = render_daemon_section(
                                            ui,
                                            &doc.daemon,
                                            input.status.upgrade_available(),
                                            input.now_ms,
                                            inner_target_w,
                                        );
                                        let terms_w =
                                            render_terminals_section(ui, rows, stale, out);
                                        let leftovers_w =
                                            render_leftovers_section(ui, &doc.daemon.leftovers);
                                        let errors_w = render_errors_section(
                                            ui,
                                            &doc.errors,
                                            input.errors_shown,
                                        );
                                        measured_content_w = daemon_w
                                            .max(terms_w)
                                            .max(leftovers_w)
                                            .max(errors_w);
                                    }
                                }
                            });
                    });
                if side == PanelSide::Float && measured_content_w > 0.0 {
                    state.float_width = Some(
                        (measured_content_w + 22.0)
                            .clamp(300.0_f32.min(max_float_w), max_float_w.min(760.0)),
                    );
                }
            });

            let r = frame_resp.response.rect;
            state.last_rect = Some(r);
            match side {
                PanelSide::Left => {
                    ui.painter().line_segment(
                        [egui::pos2(r.right(), r.top()), egui::pos2(r.right(), r.bottom())],
                        egui::Stroke::new(1.0, BORDER),
                    );
                }
                PanelSide::Right => {
                    ui.painter().line_segment(
                        [egui::pos2(r.left(), r.top()), egui::pos2(r.left(), r.bottom())],
                        egui::Stroke::new(1.0, BORDER),
                    );
                }
                PanelSide::Float => {}
            }
        });
}

fn render_daemon_section(
    ui: &mut egui::Ui,
    d: &DaemonStatus,
    upgrade_available: Option<&str>,
    now_ms: u64,
    row_width: f32,
) -> f32 {
    // Row 1: `<version> · <engine> · <ns X | socket>` on the left, `up <span>` flush right within `row_width`.
    let mut left_job = egui::text::LayoutJob::default();
    left_job.append(
        &format_name_version(&d.version),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: TEXT,
            ..Default::default()
        },
    );
    let where_label = match d.ns.as_deref().filter(|s| !s.is_empty()) {
        Some(ns) => format!("ns {ns}"),
        None => d.socket.clone(),
    };
    left_job.append(
        &format!(" · {} · {where_label}", format_name_version(&d.engine)),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            color: MUTED,
            ..Default::default()
        },
    );
    let left_galley = ui.fonts_mut(|f| f.layout_job(left_job));
    let uptime_str = format!("up {}", format_span(now_ms.saturating_sub(d.started_at)));
    let right_galley = ui.painter().layout_no_wrap(
        uptime_str,
        egui::FontId::monospace(12.0),
        MUTED,
    );
    let natural_row1_w = left_galley.size().x + 24.0 + right_galley.size().x;
    let alloc_w = row_width.max(natural_row1_w);
    let row1_h = left_galley.size().y.max(right_galley.size().y).max(16.0);
    let (row1_rect, _) = ui.allocate_exact_size(egui::vec2(alloc_w, row1_h), egui::Sense::hover());
    if ui.is_rect_visible(row1_rect) {
        ui.painter().galley(
            egui::pos2(
                row1_rect.left(),
                row1_rect.center().y - left_galley.size().y * 0.5,
            ),
            left_galley,
            TEXT,
        );
        ui.painter().galley(
            egui::pos2(
                row1_rect.right() - right_galley.size().x,
                row1_rect.center().y - right_galley.size().y * 0.5,
            ),
            right_galley,
            MUTED,
        );
    }

    // Row 2: holder, brain, optional restarts summary.
    let holder_rss = d
        .holder
        .rss
        .map_or_else(String::new, |r| format!(" · {}", format_bytes(r)));
    let b = &d.brain;
    let brain_rss = b
        .rss
        .map_or_else(String::new, |r| format!(" · {}", format_bytes(r)));
    let restarts: Vec<String> = [
        ("crash", b.restarts.crash),
        ("watchdog", b.restarts.watchdog),
        ("upgrade", b.restarts.upgrade),
    ]
    .into_iter()
    .filter(|&(_, n)| n > 0)
    .map(|(w, n)| format!("{n} {w}"))
    .collect();

    let mut natural_w = natural_row1_w;
    let row2_resp = ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(20.0, 2.0);
        ui.label(
            egui::RichText::new(format!("holder {}{holder_rss}", d.holder.pid))
                .font(egui::FontId::monospace(12.0))
                .color(MUTED),
        );
        ui.label(
            egui::RichText::new(format!("brain {} · gen {}{brain_rss}", b.pid, b.generation))
                .font(egui::FontId::monospace(12.0))
                .color(MUTED),
        );
        if !restarts.is_empty() {
            ui.label(
                egui::RichText::new(format!(
                    "restarts: {} · last {} {}",
                    restarts.join(", "),
                    b.last_start.reason,
                    format_clock_utc(b.last_start.at),
                ))
                .font(egui::FontId::monospace(12.0))
                .color(MUTED),
            );
        }
    });
    natural_w = natural_w.max(row2_resp.response.rect.width());

    for warning in &d.env.warnings {
        let r = ui.label(
            egui::RichText::new(format!("env: {warning}"))
                .font(egui::FontId::monospace(12.0))
                .color(AMBER),
        );
        natural_w = natural_w.max(r.rect.width());
    }

    if let Some(newer) = upgrade_available {
        let r = ui.label(
            egui::RichText::new(format!(
                "the relay ships {} · g3d upgrade moves termd to it, every shell kept",
                format_name_version(newer)
            ))
            .font(egui::FontId::monospace(12.0))
            .color(AMBER),
        );
        natural_w = natural_w.max(r.rect.width());
    }
    natural_w
}

fn render_terminals_section(
    ui: &mut egui::Ui,
    rows: &[StatusTerminalRow],
    stale: bool,
    out: &mut Vec<String>,
) -> f32 {
    ui.add_space(4.0);
    if rows.is_empty() {
        let r = ui.label(
            egui::RichText::new("no terminals")
                .font(egui::FontId::monospace(12.0))
                .color(MUTED),
        );
        return r.rect.width();
    }

    let dim = if stale { 0.55_f32 } else { 1.0_f32 };
    let apply_dim = |c: egui::Color32| -> egui::Color32 {
        if stale {
            c.gamma_multiply(dim)
        } else {
            c
        }
    };

    let grid_resp = egui::Grid::new("wall_status_terminals_grid")
        .spacing(egui::vec2(14.0, 3.0))
        .striped(false)
        .show(ui, |ui| {
            for row in rows {
                let on_wall = row.click_cmd.is_some();
                let sense = if on_wall {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                };

                let c1 = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.name)
                            .font(egui::FontId::monospace(12.0))
                            .color(apply_dim(BRIGHT)),
                    )
                    .extend()
                    .sense(sense),
                );
                let c2 = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.program)
                            .font(egui::FontId::monospace(12.0))
                            .color(apply_dim(MUTED)),
                    )
                    .extend()
                    .sense(sense),
                );
                let c3 = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.size)
                            .font(egui::FontId::monospace(12.0))
                            .color(apply_dim(MUTED)),
                    )
                    .extend()
                    .sense(sense),
                );
                let c4 = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.subscribers)
                            .font(egui::FontId::monospace(12.0))
                            .color(apply_dim(MUTED)),
                    )
                    .extend()
                    .sense(sense),
                );
                let act_col = if row.activity_live { TEXT } else { MUTED };
                let c5 = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.activity)
                            .font(egui::FontId::monospace(12.0))
                            .color(apply_dim(act_col)),
                    )
                    .extend()
                    .sense(sense),
                );

                let mut notes_job = egui::text::LayoutJob::default();
                if !row.notes.loud.is_empty() {
                    notes_job.append(
                        &row.notes.loud.join(" · "),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(12.0),
                            color: apply_dim(AMBER),
                            ..Default::default()
                        },
                    );
                }
                if !row.notes.loud.is_empty() && !row.notes.quiet.is_empty() {
                    notes_job.append(
                        " ",
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(12.0),
                            color: apply_dim(MUTED),
                            ..Default::default()
                        },
                    );
                }
                if !row.notes.quiet.is_empty() {
                    notes_job.append(
                        &row.notes.quiet.join(" · "),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(12.0),
                            color: apply_dim(MUTED),
                            ..Default::default()
                        },
                    );
                }
                let c6 = ui.add(egui::Label::new(notes_job).extend().sense(sense));
                ui.end_row();

                let cells = [c1, c2, c3, c4, c5, c6];
                let row_hovered = cells.iter().any(egui::Response::hovered);
                let row_clicked = cells.iter().any(egui::Response::clicked);

                if row_hovered && on_wall {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                for cell_resp in cells {
                    cell_resp.on_hover_text(&row.tooltip);
                }
                if row_clicked && on_wall {
                    out.push(format!("wall.reveal {}", row.id));
                }
            }
        });
    grid_resp.response.rect.width()
}

fn render_leftovers_section(ui: &mut egui::Ui, leftovers: &[LeftoverStatus]) -> f32 {
    if leftovers.is_empty() {
        return 0.0;
    }
    ui.add_space(6.0);
    let mut max_w = 0.0_f32;
    for l in leftovers {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            "left running",
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: AMBER,
                ..Default::default()
            },
        );
        job.append(
            &format!(" {} · ended {} ", l.id, format_clock_utc(l.since)),
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: MUTED,
                ..Default::default()
            },
        );
        let comms = l
            .processes
            .iter()
            .map(|p| p.comm.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        job.append(
            &comms,
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: TEXT,
                ..Default::default()
            },
        );
        let tip = l
            .processes
            .iter()
            .map(|p| format!("{} {}", p.pid, p.comm))
            .collect::<Vec<_>>()
            .join("\n");
        let resp = ui.add(egui::Label::new(job).wrap().sense(egui::Sense::hover()));
        max_w = max_w.max(resp.rect.width());
        if !tip.is_empty() {
            resp.on_hover_text(tip);
        }
    }
    max_w
}

fn render_errors_section(ui: &mut egui::Ui, errors: &[StatusError], shown: usize) -> f32 {
    let slice_len = errors.len().min(shown);
    if slice_len == 0 {
        return 0.0;
    }
    ui.add_space(6.0);
    let mut max_w = 0.0_f32;
    for e in &errors[..slice_len] {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &format!("{} ", format_clock_utc(e.at)),
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: MUTED,
                ..Default::default()
            },
        );
        job.append(
            &e.code,
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: AMBER,
                ..Default::default()
            },
        );
        if e.count > 1 {
            job.append(
                &format!(" ×{}", e.count),
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::monospace(12.0),
                    color: MUTED,
                    ..Default::default()
                },
            );
        }
        if let Some(id) = e.id.as_deref().filter(|s| !s.is_empty()) {
            job.append(
                &format!(" {id}"),
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::monospace(12.0),
                    color: MUTED,
                    ..Default::default()
                },
            );
        }
        job.append(
            &format!(" {}", e.message),
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(12.0),
                color: TEXT,
                ..Default::default()
            },
        );
        let tip = format!(
            "{}\nfirst {} UTC · {}×",
            e.message,
            format_clock_utc(e.first_at),
            e.count
        );
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        ui.set_max_width(640.0);
        let resp = ui.add(egui::Label::new(job).wrap().sense(egui::Sense::hover()));
        max_w = max_w.max(resp.rect.width());
        resp.on_hover_text(tip);
    }
    max_w
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_live_status() -> TermdStatus {
        let mut status = TermdStatus::new("rs-wall");
        status.set_bundled_version("g3d-termd/0.3.0");
        status.on_welcome("8c1f0e7a9b2d4c55", &["key", "paste", "mouse"], 100);
        status
            .on_status_json(
                json!({
                    "ev": "status",
                    "at": 1_794_519_127_407_u64,
                    "daemon": {
                        "version": "g3d-termd/0.2.0",
                        "engine": "libghostty-vt/1.0.1",
                        "proto": [1, 5],
                        "ns": "rs-wall",
                        "socket": "/tmp/g3d-501/termd-rs-wall.sock",
                        "started_at": 1_794_518_713_751_u64,
                        "holder": {"pid": 41190, "rss": 3_145_728},
                        "brain": {
                            "pid": 41200, "generation": 2, "rss": 41_943_040,
                            "restarts": {"crash": 1, "watchdog": 0, "upgrade": 0},
                            "last_start": {"reason": "crash", "at": 1_794_519_100_000_u64}
                        },
                        "env": {"warnings": ["PATH fell back to default"]},
                        "leftovers": [{
                            "id": "t3", "pid": 41260, "since": 1_794_519_000_000_u64,
                            "processes": [{"pid": 41266, "comm": "sleep"}]
                        }]
                    },
                    "terminals": [
                        {
                            "id": "t1", "handle": 7, "pid": 41234, "argv": ["/bin/-zsh"], "login": true,
                            "cwd": "/srv/src", "pwd": "/srv/src/app", "title": "~/src/app", "cols": 120, "rows": 40,
                            "alt": true, "out_bps": 1_253_000, "last_output_at": 1_794_519_127_390_u64,
                            "flow": "blocked", "input_queued": 2048, "processes": 3,
                            "subscribers": [{"conn": 12, "client": "glyph3d-native/0.1", "role": "own", "mode": "raw", "flow": "block", "lagging": false}]
                        },
                        {
                            "id": "t2", "handle": 8, "pid": 41299, "argv": ["/usr/bin/htop"],
                            "cwd": "/srv", "title": "htop", "cols": 80, "rows": 24,
                            "out_bps": 0, "last_output_at": 1_794_519_082_407_u64
                        }
                    ],
                    "errors": [{
                        "code": "brain_crash",
                        "message": "brain exited on SIGSEGV",
                        "first_at": 1_794_519_099_900_u64,
                        "at": 1_794_519_099_900_u64,
                        "count": 2,
                        "id": "t1"
                    }]
                }),
                1_794_519_127_407,
            )
            .unwrap();
        status
    }

    fn run_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        viewport: (f32, f32),
        state: &mut StatusState,
        input: &StatusInput,
    ) -> (Vec<String>, Vec<String>) {
        let mut out = Vec::new();
        let screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(viewport.0, viewport.1),
        ));
        let had_no_events = events.is_empty();
        let mut full = ctx.run_ui(
            egui::RawInput {
                screen_rect,
                events,
                ..Default::default()
            },
            |root_ui| {
                let wants_kb = show(root_ui.ctx(), state, input, &mut out);
                assert!(!wants_kb, "status panel must never claim keyboard focus");
            },
        );
        // Settle newly created `egui::Area` sizing passes on zero-event inspection frames.
        if had_no_events {
            full = ctx.run_ui(
                egui::RawInput {
                    screen_rect,
                    ..Default::default()
                },
                |root_ui| {
                    let wants_kb = show(root_ui.ctx(), state, input, &mut out);
                    assert!(!wants_kb, "status panel must never claim keyboard focus");
                },
            );
        }
        let mut texts = Vec::new();
        fn walk(shape: &egui::epaint::Shape, texts: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(t) => texts.push(t.galley.text().to_string()),
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, texts);
                    }
                }
                _ => {}
            }
        }
        for cs in &full.shapes {
            walk(&cs.shape, &mut texts);
        }
        (out, texts)
    }

    fn click_at(
        ctx: &egui::Context,
        pos: egui::Pos2,
        viewport: (f32, f32),
        state: &mut StatusState,
        input: &StatusInput,
    ) -> Vec<String> {
        let (mut out, _) = run_frame(
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            viewport,
            state,
            input,
        );
        let (mut release_out, _) = run_frame(
            ctx,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            viewport,
            state,
            input,
        );
        out.append(&mut release_out);
        out
    }

    #[test]
    fn status_panel_lays_out_across_sizes_and_renders_empty_states_explicitly() {
        let ctx = egui::Context::default();
        let mut state = StatusState::default();

        // 1. Absent daemon (no doc): chip says `termd idle` and open window explains why.
        let mut absent_status = TermdStatus::new("rs-wall");
        absent_status.on_socket_absent(1_000);
        let mut input = StatusInput::new(
            StatusPlacement {
                open: true,
                side: PanelSide::Float,
                float: None,
            },
            absent_status,
            2_000,
        );
        for &(w, h) in &[(1600.0, 1000.0), (900.0, 600.0), (340.0, 260.0)] {
            let (_, texts) = run_frame(&ctx, Vec::new(), (w, h), &mut state, &input);
            assert!(
                texts
                    .iter()
                    .any(|t| t.contains("no termd running in this namespace")),
                "missing absent message in {texts:?}"
            );
            assert!(texts.iter().any(|t| t.contains("termd idle")));
        }

        // 2. Live daemon with zero terminals: window explicitly says `no terminals`.
        let mut empty_doc_status = TermdStatus::new("rs-wall");
        empty_doc_status.on_welcome("inst", &["key", "paste"], 1_000);
        empty_doc_status
            .on_status_json(
                json!({
                    "daemon": {"version": "g3d-termd/0.2.0", "engine": "glyph-vt/0.1", "ns": "rs-wall", "started_at": 500},
                    "terminals": []
                }),
                1_500,
            )
            .unwrap();
        input.status = empty_doc_status;
        let (_, texts) = run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);
        assert!(
            texts.iter().any(|t| t.contains("no terminals")),
            "expected 'no terminals' in {texts:?}"
        );
    }

    #[test]
    fn chip_click_toggles_window_row_click_reveals_wall_terminal_and_lost_notice_dismisses() {
        let ctx = egui::Context::default();
        let mut state = StatusState::default();
        let mut input = StatusInput::new(
            StatusPlacement {
                open: true,
                side: PanelSide::Left,
                float: None,
            },
            sample_live_status(),
            1_794_519_127_407,
        );
        input.dock_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 40.0),
            egui::vec2(440.0, 860.0),
        ));
        input.chip_pos = Some(egui::pos2(600.0, 10.0));
        // Only t1 is on the wall; t2 is an external session in the same namespace.
        input.wall_names.insert("t1".to_string(), "main shell".to_string());

        // Warm-up layout passes.
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        let (_, texts) = run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        assert!(texts.iter().any(|t| t.contains("termd 2")));
        assert!(texts.iter().any(|t| t.contains("main shell")));
        assert!(texts.iter().any(|t| t.contains("left running")));
        assert!(texts.iter().any(|t| t.contains("brain_crash")));

        // Clicking the topbar chip at `(600, 10)` emits `wall.status toggle`.
        let chip_out = click_at(
            &ctx,
            egui::pos2(620.0, 20.0),
            (1280.0, 900.0),
            &mut state,
            &input,
        );
        assert_eq!(chip_out, vec!["wall.status toggle"]);

        // Find `"main shell"` text position in the rendered shapes to click `t1` accurately.
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 900.0),
            )),
            ..Default::default()
        };
        let full = ctx.run_ui(raw, |root_ui| {
            let mut unused = Vec::new();
            show(root_ui.ctx(), &mut state, &input, &mut unused);
        });
        fn find_text_pos(
            shapes: &[egui::epaint::ClippedShape],
            mut pred: impl FnMut(&str) -> bool,
        ) -> Option<(egui::Pos2, egui::Vec2)> {
            fn walk(
                shape: &egui::epaint::Shape,
                pred: &mut dyn FnMut(&str) -> bool,
            ) -> Option<(egui::Pos2, egui::Vec2)> {
                match shape {
                    egui::epaint::Shape::Text(t) if pred(t.galley.text()) => {
                        Some((t.pos, t.galley.size()))
                    }
                    egui::epaint::Shape::Vec(v) => v.iter().find_map(|s| walk(s, pred)),
                    _ => None,
                }
            }
            shapes.iter().find_map(|cs| walk(&cs.shape, &mut pred))
        }
        let (ms_pos, ms_size) =
            find_text_pos(&full.shapes, |s| s == "main shell").expect("main shell row must be painted");
        let row_pt = ms_pos + ms_size * 0.5;
        let row_out = click_at(&ctx, row_pt, (1280.0, 900.0), &mut state, &input);
        assert_eq!(row_out, vec!["wall.reveal t1"]);

        // Simulate a holder crash (`lost`) so the dismissible red notice appears under the topbar.
        input.placement.open = false;
        input.status.on_socket_absent(1_794_519_130_000);
        assert_eq!(input.status.state(), TermdState::Lost);
        let (_, notice_texts) = run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        assert!(
            notice_texts
                .iter()
                .any(|t| t.contains("termd lost: the shells in namespace \"rs-wall\" are gone")),
            "expected lost notice in {notice_texts:?}"
        );

        // Locate the notice's text rect so we click its `×` dismiss button just to the right of the text.
        let full_notice = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 900.0),
                )),
                ..Default::default()
            },
            |root_ui| {
                let mut unused = Vec::new();
                show(root_ui.ctx(), &mut state, &input, &mut unused);
            },
        );
        let (nt_pos, nt_size) = find_text_pos(&full_notice.shapes, |s| s.starts_with("termd lost:"))
            .expect("lost notice text must be painted");
        let dismiss_pt = egui::pos2(nt_pos.x + nt_size.x + 18.0, nt_pos.y + nt_size.y * 0.5);
        let dismiss_out = click_at(&ctx, dismiss_pt, (1280.0, 900.0), &mut state, &input);
        assert_eq!(dismiss_out, vec!["wall.status.dismiss"]);
    }

    #[test]
    fn floating_width_stabilizes_without_creeping() {
        let ctx = egui::Context::default();
        let mut state = StatusState::default();
        let mut input = StatusInput::new(
            StatusPlacement {
                open: true,
                side: PanelSide::Float,
                float: None,
            },
            sample_live_status(),
            1_794_519_127_407,
        );
        input
            .wall_names
            .insert("t1".to_string(), "main shell".to_string());

        run_frame(&ctx, Vec::new(), (1600.0, 1000.0), &mut state, &input);
        run_frame(&ctx, Vec::new(), (1600.0, 1000.0), &mut state, &input);
        let settled = state.float_width.expect("floating status sets float_width");
        for _ in 0..20 {
            run_frame(&ctx, Vec::new(), (1600.0, 1000.0), &mut state, &input);
        }
        let after = state.float_width.expect("floating status keeps float_width");
        assert!(
            (after - settled).abs() < 0.01,
            "float_width crept from {settled} to {after}"
        );
    }
}
