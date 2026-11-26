//! Native `egui` sidebar panel (`wall/Sidebar.jsx` + `wall/Panel.jsx` +
//! `wall/holdPress.js`), backed by [`wall_core::sidebar`] and
//! [`wall_core::dock`].
//!
//! Contract (`out/wall-port/briefs/P-panels.md`):
//! `pub fn show(ctx: &egui::Context, state: &mut SidebarState, input: &SidebarInput, out: &mut Vec<String>)`
//!
//! - Lists groups first (foldable, with column `−`/`+`, `tile`/`free` toggle,
//!   and ungroup tools), their members indented beneath them, then ungrouped
//!   terminals, with hidden terminals dimmed and marked `"hidden"`.
//! - Below the terminals tree, renders the `agents` section grouped by host,
//!   showing status dots, `"on wall"` badges, hover details, and `wall.attach`
//!   clicks (or the empty-state explanation when no live agents exist).
//! - Docks on the left or right viewport edge (using `input.dock_rect` from
//!   [`wall_core::dock::dock_layout`]) or floats freely with draggable header
//!   persistence (`wall.sidebar float x y`).
//! - Never steals keyboard focus from the focused terminal (`wants_keyboard` is
//!   always `false`).

use std::collections::HashMap;
use std::f32::consts::TAU;

use wall_core::chord::chord_label;
use wall_core::dock::{DockLayout, DockSide, DOCK_WIDTHS};
use wall_core::input::Platform;
use wall_core::sidebar::{
    build_sidebar, AgentRow, GroupRow, SidebarInput as CoreSidebarInput, SidebarModel, SidebarRow,
    TerminalRow,
};

use super::{
    chrome_icon_btn, dock_buttons, format_verb_line, layout_elided, paint_chrome_icon,
    parse_hex_color, safe_viewport_rect, sanitize_egui_text, ChromeIcon,
};

const SIDEBAR_AREA_ID: &str = "wall_sidebar_panel";
const DEFAULT_CLOSE_HOLD_MS: u64 = 450;

/// Compute the docked [`egui::Rect`] for the sidebar on `viewport` from
/// [`wall_core::dock::DockLayout`] (`None` when `side == DockSide::Float`).
pub fn sidebar_dock_rect(
    viewport: egui::Rect,
    side: DockSide,
    layout: &DockLayout,
) -> Option<egui::Rect> {
    let w = DOCK_WIDTHS.sidebar as f32;
    let offset = layout.sidebar as f32;
    match side {
        DockSide::Left => {
            let left = viewport.left() + offset;
            Some(egui::Rect::from_min_max(
                egui::pos2(left, viewport.top()),
                egui::pos2((left + w).min(viewport.right()), viewport.bottom()),
            ))
        }
        DockSide::Right => {
            let right = viewport.right() - offset;
            Some(egui::Rect::from_min_max(
                egui::pos2((right - w).max(viewport.left()), viewport.top()),
                egui::pos2(right, viewport.bottom()),
            ))
        }
        DockSide::Float => None,
    }
}

fn fallback_dock_rect(viewport: egui::Rect, side: DockSide) -> egui::Rect {
    let w = (DOCK_WIDTHS.sidebar as f32).min(viewport.width());
    match side {
        DockSide::Left => egui::Rect::from_min_max(
            viewport.min,
            egui::pos2(viewport.left() + w, viewport.bottom()),
        ),
        DockSide::Right => egui::Rect::from_min_max(
            egui::pos2(viewport.right() - w, viewport.top()),
            viewport.max,
        ),
        DockSide::Float => viewport,
    }
}

/// Plain-data input read by [`show`].
#[derive(Clone, Debug)]
pub struct SidebarInput {
    /// Rows and agent host sections produced by [`wall_core::sidebar::build_sidebar`].
    pub model: SidebarModel,
    /// Docked rectangle from [`sidebar_dock_rect`] when `side` is `Left` or `Right`.
    /// When `None` while docked, falls back to the outer `Left`/`Right` viewport edge.
    pub dock_rect: Option<egui::Rect>,
    /// Platform used to format chord hints in button tooltips (`⌘B` vs `Ctrl+Alt+B`).
    pub platform: Platform,
    /// Hold duration in milliseconds for the terminal `×` hold-to-close ring (`closeHoldMs`).
    pub close_hold_ms: u64,
}

impl SidebarInput {
    /// Wrap an already-built [`SidebarModel`] with default Mac tooltips and 450 ms close hold.
    pub fn new(model: SidebarModel) -> Self {
        Self {
            model,
            dock_rect: None,
            platform: Platform::Mac,
            close_hold_ms: DEFAULT_CLOSE_HOLD_MS,
        }
    }

    /// Build directly from [`wall_core::sidebar::SidebarInput`].
    pub fn from_core(raw: &CoreSidebarInput) -> Self {
        Self::new(build_sidebar(raw))
    }
}

impl Default for SidebarInput {
    fn default() -> Self {
        Self::from_core(&CoreSidebarInput::default())
    }
}

/// Persistent view state for the sidebar held across frames by the caller.
#[derive(Clone, Debug)]
pub struct SidebarState {
    /// Whether the sidebar panel is open.
    pub open: bool,
    /// Placement mode (`Left`, `Right`, or `Float`).
    pub side: DockSide,
    /// Top-left position in viewport points when `side == DockSide::Float`.
    pub float_pos: egui::Pos2,
    /// Always `false`: sidebar controls never steal keyboard focus from terminals.
    pub wants_keyboard: bool,
    /// Tracks `(last_input_collapsed, effective_collapsed)` per group id so
    /// folding works immediately on click AND stays synchronized when the
    /// caller updates `SidebarModel` via `wall.sidebar.collapse`.
    fold_state: HashMap<String, (bool, bool)>,
    /// Active floating header drag offset (`pointer_pos - float_pos`).
    drag_offset: Option<egui::Vec2>,
    /// Active hold-to-close press `(terminal_id, start_time_secs)`.
    hold_close: Option<(String, f64)>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new(false, DockSide::Right)
    }
}

impl SidebarState {
    /// Construct a sidebar state with explicit `open` and `side` (`float_pos` defaults to `(16, 48)`).
    pub fn new(open: bool, side: DockSide) -> Self {
        Self {
            open,
            side,
            float_pos: egui::pos2(16.0, 48.0),
            wants_keyboard: false,
            fold_state: HashMap::new(),
            drag_offset: None,
            hold_close: None,
        }
    }

    /// Whether the sidebar wants keyboard input (`false`).
    pub const fn wants_keyboard(&self) -> bool {
        self.wants_keyboard
    }

    /// Whether group `gid` is currently folded in the view.
    pub fn is_group_collapsed(&mut self, gid: &str, model_collapsed: bool) -> bool {
        let entry = self
            .fold_state
            .entry(gid.to_string())
            .or_insert((model_collapsed, model_collapsed));
        if entry.0 != model_collapsed {
            *entry = (model_collapsed, model_collapsed);
        }
        entry.1
    }

    fn toggle_group_collapsed(&mut self, gid: &str, model_collapsed: bool) {
        let cur = self.is_group_collapsed(gid, model_collapsed);
        self.fold_state
            .insert(gid.to_string(), (model_collapsed, !cur));
    }
}

fn push_cmd(out: &mut Vec<String>, cmd: &[String]) {
    if !cmd.is_empty() {
        out.push(format_verb_line(cmd));
    }
}

/// Non-focusable `18x18` row tool button that is always allocated for hit-testing
/// (`visibility: hidden` until hovered, so trackpad tap-to-click hits on the first
/// frame without shifting row text width).
fn row_tool_btn(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    tooltip: &str,
    active: bool,
    visible: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    if visible && ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if active || hovered {
            let bg = if active {
                egui::Color32::from_rgb(0x1d, 0x23, 0x30)
            } else {
                egui::Color32::from_rgb(0x23, 0x2a, 0x3a)
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), bg);
        }
        let fg = if active || hovered {
            egui::Color32::from_rgb(0xe6, 0xe9, 0xef)
        } else {
            egui::Color32::from_rgb(0x8b, 0x95, 0xa7)
        };
        paint_chrome_icon(ui.painter(), rect, icon, fg);
    }
    if visible && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if visible {
        resp.on_hover_text(sanitize_egui_text(tooltip))
    } else {
        resp
    }
}

/// Paint a `HoldClose` `18x18` button (`Sidebar.jsx:10-17` + `holdPress.js`):
/// holding primary press over `hold_ms` fills a `#e5534b` progress arc around `×`
/// and returns `true` once completed.
fn hold_close_btn(
    ui: &mut egui::Ui,
    term_id: &str,
    hold_ms: u64,
    visible: bool,
    hold_slot: &mut Option<(String, f64)>,
) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click_and_drag());
    let now = ui.ctx().input(|i| i.time);
    let primary_down = ui.ctx().input(|i| i.pointer.primary_down());
    let pointer_over = ui
        .ctx()
        .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        .is_some_and(|p| rect.contains(p));

    let mut interacted = false;
    let mut fired = false;

    if (resp.is_pointer_button_down_on() || (primary_down && pointer_over && resp.hovered()))
        && pointer_over
    {
        interacted = true;
        let start = match hold_slot {
            Some((id, t0)) if id == term_id => *t0,
            _ => {
                *hold_slot = Some((term_id.to_string(), now));
                now
            }
        };
        let need_secs = (hold_ms as f64) / 1000.0;
        let progress = if need_secs <= 0.0 {
            1.0
        } else {
            ((now - start) / need_secs).clamp(0.0, 1.0)
        };

        if ui.is_rect_visible(rect) {
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(4),
                egui::Color32::from_rgb(0x23, 0x2a, 0x3a),
            );
            paint_chrome_icon(
                ui.painter(),
                rect,
                ChromeIcon::Close,
                egui::Color32::from_rgb(0xe6, 0xe9, 0xef),
            );
            if progress > 0.0 {
                let steps = ((progress * 28.0).ceil() as usize).max(2);
                let center = rect.center();
                let radius = 7.6_f32;
                let pts: Vec<egui::Pos2> = (0..=steps)
                    .map(|k| {
                        let frac = (k as f32) / (steps as f32);
                        let angle = -std::f32::consts::FRAC_PI_2 + (progress as f32) * TAU * frac;
                        egui::pos2(center.x + radius * angle.cos(), center.y + radius * angle.sin())
                    })
                    .collect();
                ui.painter().add(egui::Shape::line(
                    pts,
                    egui::Stroke::new(1.6, egui::Color32::from_rgb(0xe5, 0x53, 0x4b)),
                ));
            }
        }

        if progress >= 1.0 {
            *hold_slot = None;
            fired = true;
        } else {
            ui.ctx().request_repaint();
        }
    } else {
        if hold_slot.as_ref().is_some_and(|(id, _)| id == term_id) {
            *hold_slot = None;
        }
        if resp.clicked() && hold_ms == 0 {
            interacted = true;
            fired = true;
        }
        if visible && ui.is_rect_visible(rect) {
            if resp.hovered() {
                ui.painter().rect_filled(
                    rect,
                    egui::CornerRadius::same(4),
                    egui::Color32::from_rgb(0x23, 0x2a, 0x3a),
                );
            }
            let fg = if resp.hovered() {
                egui::Color32::from_rgb(0xe6, 0xe9, 0xef)
            } else {
                egui::Color32::from_rgb(0x8b, 0x95, 0xa7)
            };
            paint_chrome_icon(ui.painter(), rect, ChromeIcon::Close, fg);
        }
    }

    if visible && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if visible {
        let _ = resp.on_hover_text("hold to close (kills the shell)");
    }
    (interacted, fired)
}

fn draw_group_row(
    ui: &mut egui::Ui,
    panel_w: f32,
    g: &GroupRow,
    collapsed: bool,
    platform: Platform,
    state: &mut SidebarState,
    out: &mut Vec<String>,
) {
    let row_h = 22.0;
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(panel_w, row_h), egui::Sense::hover());
    let pointer_in_row = ui
        .ctx()
        .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        .is_some_and(|p| row_rect.contains(p));

    if ui.is_rect_visible(row_rect) && pointer_in_row {
        ui.painter().rect_filled(
            row_rect,
            egui::CornerRadius::ZERO,
            egui::Color32::from_rgb(0x15, 0x1a, 0x24),
        );
    }

    let mut tool_interacted = false;
    let mut row_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(row_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row_ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
    row_ui.add_space(4.0);

    // Fold/unfold button (`▸` / `▾`).
    let chevron = if collapsed {
        ChromeIcon::ChevronRight
    } else {
        ChromeIcon::ChevronDown
    };
    let fold_resp = chrome_icon_btn(
        &mut row_ui,
        chevron,
        if collapsed { "unfold" } else { "fold" },
        false,
    );
    if fold_resp.clicked() {
        tool_interacted = true;
        state.toggle_group_collapsed(&g.id, g.collapsed);
        push_cmd(out, &g.collapse_cmd);
    }

    let tiled = g.cols_dec_cmd.is_some() && g.cols_inc_cmd.is_some();
    let tools_hit_w = if tiled {
        18.0 + 14.0 + 18.0 + 34.0 + 18.0 + 12.0
    } else {
        34.0 + 18.0 + 8.0
    };
    let visual_tools_w = if pointer_in_row { tools_hit_w } else { 6.0 };
    let count_text = g.member_count.to_string();
    let count_w = 18.0;
    let avail_name_w =
        (row_rect.right() - visual_tools_w - count_w - row_ui.cursor().min.x - 4.0).max(24.0);
    if ui.is_rect_visible(row_rect) {
        let name_x = row_ui.cursor().min.x;
        let galley = layout_elided(
            row_ui.painter(),
            &g.name,
            egui::FontId::monospace(12.5),
            egui::Color32::from_rgb(0xae, 0xb7, 0xc6),
            (avail_name_w - 2.0).max(16.0),
        );
        row_ui.painter().galley(
            egui::pos2(name_x + 2.0, row_rect.center().y - galley.size().y * 0.5),
            galley,
            egui::Color32::from_rgb(0xae, 0xb7, 0xc6),
        );
        let count_right = row_rect.right() - visual_tools_w - 2.0;
        row_ui.painter().text(
            egui::pos2(count_right, row_rect.center().y),
            egui::Align2::RIGHT_CENTER,
            count_text,
            egui::FontId::monospace(11.0),
            egui::Color32::from_rgb(0x5d, 0x65, 0x74),
        );
    }

    row_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add_space(6.0);
        let ungroup_tip = format!(
            "ungroup (members stay where they are) — {}",
            chord_label(platform, &["G"], true)
        );
        if row_tool_btn(ui, ChromeIcon::Ungroup, &ungroup_tip, false, pointer_in_row).clicked() {
            tool_interacted = true;
            push_cmd(out, &g.ungroup_cmd);
        }

        let layout_tip = if tiled {
            "freeform: members keep their own spots"
        } else {
            "tile: pack members in columns"
        };
        let (badge_rect, badge_resp) =
            ui.allocate_exact_size(egui::vec2(32.0, 18.0), egui::Sense::click());
        if pointer_in_row && ui.is_rect_visible(badge_rect) {
            if badge_resp.hovered() {
                ui.painter().rect_filled(
                    badge_rect,
                    egui::CornerRadius::same(4),
                    egui::Color32::from_rgb(0x23, 0x2a, 0x3a),
                );
            }
            let fg = if badge_resp.hovered() {
                egui::Color32::from_rgb(0xe6, 0xe9, 0xef)
            } else {
                egui::Color32::from_rgb(0x8b, 0x95, 0xa7)
            };
            ui.painter().text(
                badge_rect.center(),
                egui::Align2::CENTER_CENTER,
                g.layout_badge,
                egui::FontId::monospace(11.0),
                fg,
            );
        }
        if pointer_in_row && badge_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let badge_resp = if pointer_in_row {
            badge_resp.on_hover_text(layout_tip)
        } else {
            badge_resp
        };
        if badge_resp.clicked() {
            tool_interacted = true;
            push_cmd(out, &g.toggle_layout_cmd);
        }

        if let (Some(dec_cmd), Some(inc_cmd)) = (&g.cols_dec_cmd, &g.cols_inc_cmd) {
            if row_tool_btn(ui, ChromeIcon::Plus, "more columns", false, pointer_in_row).clicked() {
                tool_interacted = true;
                push_cmd(out, inc_cmd);
            }
            let (cols_rect, cols_resp) =
                ui.allocate_exact_size(egui::vec2(12.0, 18.0), egui::Sense::hover());
            if pointer_in_row && ui.is_rect_visible(cols_rect) {
                ui.painter().text(
                    cols_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    g.columns.to_string(),
                    egui::FontId::monospace(11.0),
                    egui::Color32::from_rgb(0x8b, 0x95, 0xa7),
                );
                let _ = cols_resp.on_hover_text("columns");
            }
            if row_tool_btn(ui, ChromeIcon::Minus, "fewer columns", false, pointer_in_row).clicked()
            {
                tool_interacted = true;
                push_cmd(out, dec_cmd);
            }
        }
    });

    // Row click (outside fold chevron and tool buttons) frames the group.
    let body_hit = ui.interact(
        egui::Rect::from_min_max(
            egui::pos2(row_rect.left() + 24.0, row_rect.top()),
            egui::pos2(
                (row_rect.right() - tools_hit_w).max(row_rect.left() + 28.0),
                row_rect.bottom(),
            ),
        ),
        ui.make_persistent_id(("wall_sb_group_row", &g.id)),
        egui::Sense::click(),
    );
    if body_hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if body_hit.clicked() && !tool_interacted {
        push_cmd(out, &g.click_cmd);
    }
}

fn draw_terminal_row(
    ui: &mut egui::Ui,
    panel_w: f32,
    t: &TerminalRow,
    platform: Platform,
    close_hold_ms: u64,
    state: &mut SidebarState,
    out: &mut Vec<String>,
) {
    let row_h = 22.0;
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(panel_w, row_h), egui::Sense::hover());
    let pointer_in_row = ui
        .ctx()
        .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        .is_some_and(|p| row_rect.contains(p));

    if ui.is_rect_visible(row_rect) {
        if t.focused {
            ui.painter().rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(0x16, 0x25, 0x1e),
            );
        } else if pointer_in_row {
            ui.painter().rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(0x15, 0x1a, 0x24),
            );
        }
    }

    let pad_left = 10.0 + f32::from(t.depth) * 16.0;
    let tools_count = if t.leave_group_cmd.is_some() { 4.0 } else { 3.0 };
    let tools_hit_w = tools_count * 19.0 + 8.0;
    let visual_tools_w = if pointer_in_row { tools_hit_w } else { 6.0 };
    let hidden_tag_w = if t.hidden { 42.0 } else { 0.0 };

    if ui.is_rect_visible(row_rect) {
        let name_x = row_rect.left() + pad_left;
        let name_max_w =
            (row_rect.right() - visual_tools_w - hidden_tag_w - 4.0 - name_x).max(20.0);
        let fg = if t.hidden {
            egui::Color32::from_rgb(0x6b, 0x73, 0x83)
        } else if t.focused {
            egui::Color32::from_rgb(0x8e, 0xf0, 0xb8)
        } else {
            egui::Color32::from_rgb(0xd4, 0xd9, 0xe2)
        };
        let galley = layout_elided(
            ui.painter(),
            &t.name,
            egui::FontId::monospace(12.5),
            fg,
            name_max_w,
        );
        ui.painter().galley(
            egui::pos2(name_x, row_rect.center().y - galley.size().y * 0.5),
            galley,
            fg,
        );

        if t.hidden {
            let tag_right = row_rect.right() - visual_tools_w - 2.0;
            ui.painter().text(
                egui::pos2(tag_right - hidden_tag_w * 0.5, row_rect.center().y),
                egui::Align2::CENTER_CENTER,
                "hidden",
                egui::FontId::monospace(10.5),
                egui::Color32::from_rgb(0x5d, 0x65, 0x74),
            );
        }
    }

    let mut tool_interacted = false;
    let mut row_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(row_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    row_ui.spacing_mut().item_spacing = egui::vec2(1.0, 0.0);
    row_ui.add_space(6.0);

    let (held_active, close_fired) = hold_close_btn(
        &mut row_ui,
        &t.id,
        close_hold_ms,
        pointer_in_row,
        &mut state.hold_close,
    );
    if held_active {
        tool_interacted = true;
    }
    if close_fired {
        push_cmd(out, &t.close_cmd);
    }

    let (vis_icon, vis_tip) = if t.hidden {
        (
            ChromeIcon::ShowCircle,
            "show it, at its own spot".to_string(),
        )
    } else {
        (
            ChromeIcon::HideDash,
            format!(
                "hide it (it keeps running) — {}",
                chord_label(platform, &["-"], false)
            ),
        )
    };
    if row_tool_btn(&mut row_ui, vis_icon, &vis_tip, false, pointer_in_row).clicked() {
        tool_interacted = true;
        push_cmd(out, &t.visibility_cmd);
    }

    if let Some(leave_cmd) = &t.leave_group_cmd {
        if row_tool_btn(
            &mut row_ui,
            ChromeIcon::LeaveGroup,
            "take it out of its group",
            false,
            pointer_in_row,
        )
        .clicked()
        {
            tool_interacted = true;
            push_cmd(out, leave_cmd);
        }
    }

    let agent_tip = format!("agent view ({})", chord_label(platform, &["A"], false));
    if row_tool_btn(
        &mut row_ui,
        ChromeIcon::AgentView,
        &agent_tip,
        t.agent_view_open,
        pointer_in_row,
    )
    .clicked()
    {
        tool_interacted = true;
        push_cmd(out, &t.agent_toggle_cmd);
    }

    let click_right = (row_rect.right() - tools_hit_w).max(row_rect.left() + 20.0);
    let row_hit = ui.interact(
        egui::Rect::from_min_max(row_rect.min, egui::pos2(click_right, row_rect.bottom())),
        ui.make_persistent_id(("wall_sb_term_row", &t.id)),
        egui::Sense::click(),
    );
    if row_hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if row_hit.clicked() && !tool_interacted {
        push_cmd(out, &t.click_cmd);
    }
}

fn draw_agent_row(ui: &mut egui::Ui, panel_w: f32, a: &AgentRow, out: &mut Vec<String>) {
    let row_h = 22.0;
    let (row_rect, resp) =
        ui.allocate_exact_size(egui::vec2(panel_w, row_h), egui::Sense::click());
    if a.interactive && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let resp = resp.on_hover_text(&a.tooltip);
    let clicked = resp.clicked();

    if ui.is_rect_visible(row_rect) {
        if a.focused {
            ui.painter().rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(0x16, 0x25, 0x1e),
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(
                row_rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgb(0x15, 0x1a, 0x24),
            );
        }

        let status_color = parse_hex_color(a.status.color())
            .unwrap_or(egui::Color32::from_rgb(0x8b, 0x95, 0xa7));
        let dot_center = egui::pos2(row_rect.left() + 19.5, row_rect.center().y);
        ui.painter().circle_filled(dot_center, 3.5, status_color);

        // Right-aligned status word and optional "on wall" badge.
        let status_galley = ui.painter().layout_no_wrap(
            a.status.word().to_string(),
            egui::FontId::monospace(10.5),
            status_color,
        );
        let status_x = row_rect.right() - 8.0 - status_galley.size().x;
        ui.painter().galley(
            egui::pos2(status_x, row_rect.center().y - status_galley.size().y * 0.5),
            status_galley,
            status_color,
        );

        let mut right_bound = status_x - 6.0;
        if a.on_wall.is_some() {
            let on_galley = ui.painter().layout_no_wrap(
                "on wall".to_string(),
                egui::FontId::monospace(10.5),
                egui::Color32::from_rgb(0x5d, 0x65, 0x74),
            );
            let on_x = right_bound - on_galley.size().x;
            ui.painter().galley(
                egui::pos2(on_x, row_rect.center().y - on_galley.size().y * 0.5),
                on_galley,
                egui::Color32::from_rgb(0x5d, 0x65, 0x74),
            );
            right_bound = on_x - 6.0;
        }

        let name_x = row_rect.left() + 28.0;
        let name_fg = if !a.interactive {
            egui::Color32::from_rgb(0x6b, 0x73, 0x83)
        } else if a.focused {
            egui::Color32::from_rgb(0x8e, 0xf0, 0xb8)
        } else {
            egui::Color32::from_rgb(0xd4, 0xd9, 0xe2)
        };
        let clip = egui::Rect::from_min_max(
            egui::pos2(name_x, row_rect.top()),
            egui::pos2(right_bound.max(name_x + 16.0), row_rect.bottom()),
        );
        let name_galley =
            ui.painter()
                .layout_no_wrap(a.name.clone(), egui::FontId::monospace(12.5), name_fg);
        ui.painter().with_clip_rect(clip).galley(
            egui::pos2(name_x, row_rect.center().y - name_galley.size().y * 0.5),
            name_galley,
            name_fg,
        );
    }

    if clicked {
        push_cmd(out, &a.click_cmd);
    }
}

/// Draw the sidebar panel if `state.open` is true, appending any triggered
/// `wall.*` verb lines into `out`.
pub fn show(
    ctx: &egui::Context,
    state: &mut SidebarState,
    input: &SidebarInput,
    out: &mut Vec<String>,
) {
    state.wants_keyboard = false;
    if !state.open {
        state.drag_offset = None;
        state.hold_close = None;
        return;
    }

    let viewport = safe_viewport_rect(ctx);
    let panel_w = (DOCK_WIDTHS.sidebar as f32).min(viewport.width().max(120.0));
    let is_float = state.side == DockSide::Float;

    let (pos, fixed_h, max_h) = if is_float {
        let clamped = egui::pos2(
            state
                .float_pos
                .x
                .clamp(viewport.left(), (viewport.right() - panel_w).max(viewport.left())),
            state
                .float_pos
                .y
                .clamp(viewport.top(), (viewport.bottom() - 48.0).max(viewport.top())),
        );
        state.float_pos = clamped;
        (clamped, None, (viewport.height() - 120.0).max(96.0))
    } else {
        let r = input
            .dock_rect
            .unwrap_or_else(|| fallback_dock_rect(viewport, state.side));
        (r.min, Some(r.height().max(48.0)), r.height().max(48.0))
    };

    egui::Area::new(egui::Id::new(SIDEBAR_AREA_ID))
        .order(egui::Order::Middle)
        .fixed_pos(pos)
        .fade_in(false)
        .show(ctx, |ui| {
            let corner = if is_float {
                egui::CornerRadius::same(7)
            } else {
                egui::CornerRadius::ZERO
            };
            let stroke = if is_float {
                egui::Stroke::new(1.0, egui::Color32::from_rgb(0x2a, 0x30, 0x3c))
            } else {
                egui::Stroke::NONE
            };
            let shadow = if is_float {
                egui::Shadow {
                    offset: [0, 8],
                    blur: 28,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(115),
                }
            } else {
                egui::Shadow::NONE
            };

            let frame_resp = egui::Frame::NONE
                .fill(egui::Color32::from_rgba_unmultiplied(12, 14, 20, 245))
                .stroke(stroke)
                .corner_radius(corner)
                .shadow(shadow)
                .show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_max_width(panel_w);
                    if let Some(h) = fixed_h {
                        ui.set_min_height(h);
                        ui.set_max_height(h);
                    } else {
                        ui.set_max_height(max_h);
                    }
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                    // Header bar (`Sidebar.jsx:141-144` + `useHeadDrag` in `Panel.jsx:48-68`).
                    let head_h = 30.0;
                    let (head_rect, _) =
                        ui.allocate_exact_size(egui::vec2(panel_w, head_h), egui::Sense::hover());
                    let dock_btns_w = 4.0 * 19.0 + 6.0;
                    let drag_rect = egui::Rect::from_min_max(
                        head_rect.min,
                        egui::pos2(
                            (head_rect.right() - dock_btns_w).max(head_rect.left() + 16.0),
                            head_rect.bottom(),
                        ),
                    );
                    let drag_resp = ui.interact(
                        drag_rect,
                        ui.make_persistent_id("wall_sidebar_head_drag"),
                        if is_float {
                            egui::Sense::drag()
                        } else {
                            egui::Sense::hover()
                        },
                    );
                    if is_float {
                        if drag_resp.hovered() || drag_resp.dragged() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                        }
                        if drag_resp.drag_started() {
                            if let Some(ptr) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                                state.drag_offset = Some(ptr - state.float_pos);
                            }
                        }
                        if drag_resp.dragged() {
                            if let (Some(ptr), Some(off)) =
                                (ui.ctx().input(|i| i.pointer.interact_pos()), state.drag_offset)
                            {
                                let next = ptr - off;
                                state.float_pos = egui::pos2(next.x.max(0.0), next.y.max(0.0));
                            }
                        }
                        if drag_resp.drag_stopped() {
                            state.drag_offset = None;
                            out.push(format!(
                                "wall.sidebar float {} {}",
                                state.float_pos.x.round() as i64,
                                state.float_pos.y.round() as i64
                            ));
                        }
                    } else {
                        state.drag_offset = None;
                    }

                    let mut head_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(head_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    head_ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                    head_ui.add_space(10.0);
                    head_ui.label(
                        egui::RichText::new("terminals")
                            .monospace()
                            .size(12.5)
                            .color(egui::Color32::from_rgb(0x7c, 0x85, 0x96)),
                    );
                    head_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        // Render dock buttons left-to-right inside the right-aligned box.
                        let (strip_rect, _) = ui.allocate_exact_size(
                            egui::vec2(4.0 * 18.0 + 6.0, 18.0),
                            egui::Sense::hover(),
                        );
                        let mut strip_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(strip_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        strip_ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                        let close_tip =
                            format!("close ({})", chord_label(input.platform, &["B"], false));
                        dock_buttons(
                            &mut strip_ui,
                            "wall.sidebar",
                            &mut state.side,
                            &mut state.open,
                            &close_tip,
                            out,
                        );
                    });

                    // Header bottom divider (`1px solid #1e2430`).
                    let (div_rect, _) =
                        ui.allocate_exact_size(egui::vec2(panel_w, 1.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        div_rect,
                        egui::CornerRadius::ZERO,
                        egui::Color32::from_rgb(0x1e, 0x24, 0x30),
                    );

                    // Scrollable body (`Sidebar.jsx:145-160`).
                    let body_max_h = (max_h - head_h - 1.0).max(40.0);
                    egui::ScrollArea::vertical()
                        .id_salt("wall_sidebar_body")
                        .max_height(body_max_h)
                        .auto_shrink([false, is_float])
                        .show(ui, |ui| {
                            ui.set_width(panel_w);
                            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                            ui.add_space(4.0);

                            let mut skip_grouped_members = false;
                            for row in &input.model.rows {
                                match row {
                                    SidebarRow::Group(g) => {
                                        let collapsed =
                                            state.is_group_collapsed(&g.id, g.collapsed);
                                        draw_group_row(
                                            ui,
                                            panel_w,
                                            g,
                                            collapsed,
                                            input.platform,
                                            state,
                                            out,
                                        );
                                        skip_grouped_members =
                                            state.is_group_collapsed(&g.id, g.collapsed);
                                    }
                                    SidebarRow::Terminal(t) => {
                                        if t.depth > 0 && skip_grouped_members {
                                            continue;
                                        }
                                        if t.depth == 0 {
                                            skip_grouped_members = false;
                                        }
                                        draw_terminal_row(
                                            ui,
                                            panel_w,
                                            t,
                                            input.platform,
                                            input.close_hold_ms,
                                            state,
                                            out,
                                        );
                                    }
                                }
                            }

                            if input.model.terminals_empty {
                                let (empty_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(panel_w, 22.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    egui::pos2(empty_rect.left() + 10.0, empty_rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "no terminals",
                                    egui::FontId::monospace(12.5),
                                    egui::Color32::from_rgb(0x6b, 0x73, 0x83),
                                );
                            }

                            // Agents section (`AgentsSection` in `Sidebar.jsx:86-113`).
                            ui.add_space(6.0);
                            let (sec_line, _) = ui.allocate_exact_size(
                                egui::vec2(panel_w, 1.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                sec_line,
                                egui::CornerRadius::ZERO,
                                egui::Color32::from_rgb(0x1e, 0x24, 0x30),
                            );
                            let (sec_head, _) = ui.allocate_exact_size(
                                egui::vec2(panel_w, 24.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().text(
                                egui::pos2(sec_head.left() + 10.0, sec_head.center().y + 2.0),
                                egui::Align2::LEFT_CENTER,
                                "agents",
                                egui::FontId::monospace(12.5),
                                egui::Color32::from_rgb(0x7c, 0x85, 0x96),
                            );

                            for host in &input.model.agents.hosts {
                                let (h_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(panel_w, 20.0),
                                    egui::Sense::hover(),
                                );
                                if ui.is_rect_visible(h_rect) {
                                    ui.painter().text(
                                        egui::pos2(h_rect.left() + 8.0, h_rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        &host.label,
                                        egui::FontId::monospace(12.0),
                                        egui::Color32::from_rgb(0xae, 0xb7, 0xc6),
                                    );
                                    ui.painter().text(
                                        egui::pos2(h_rect.right() - 8.0, h_rect.center().y),
                                        egui::Align2::RIGHT_CENTER,
                                        host.count.to_string(),
                                        egui::FontId::monospace(11.0),
                                        egui::Color32::from_rgb(0x5d, 0x65, 0x74),
                                    );
                                }
                                for agent in &host.agents {
                                    draw_agent_row(ui, panel_w, agent, out);
                                }
                            }

                            if let Some(empty) = &input.model.agents.empty {
                                let (erect, eresp) = ui.allocate_exact_size(
                                    egui::vec2(panel_w, 22.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    egui::pos2(erect.left() + 10.0, erect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    empty.text,
                                    egui::FontId::monospace(12.0),
                                    egui::Color32::from_rgb(0x6b, 0x73, 0x83),
                                );
                                if let Some(reason) = &empty.reason {
                                    let _ = eresp.on_hover_text(reason);
                                }
                            }
                            ui.add_space(4.0);
                        });
                });

            // Inner vertical edge border when docked (`Sidebar.jsx:134`: `1px solid #222834`).
            if !is_float {
                let r = frame_resp.response.rect;
                let border_x = match state.side {
                    DockSide::Left => r.right() - 0.5,
                    DockSide::Right => r.left() + 0.5,
                    DockSide::Float => r.left(),
                };
                ui.painter().line_segment(
                    [egui::pos2(border_x, r.top()), egui::pos2(border_x, r.bottom())],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(0x22, 0x28, 0x34)),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use wall_core::config::GroupLayout;
    use wall_core::dock::{dock_layout, DockPanel, DockState};
    use wall_core::sidebar::{BusAgentInput, BusSnapshot, SidebarGroup, SidebarTerminal};

    fn click_at(
        ctx: &egui::Context,
        size: egui::Vec2,
        pos: egui::Pos2,
        time: f64,
        state: &mut SidebarState,
        input: &SidebarInput,
        out: &mut Vec<String>,
    ) {
        let rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size));
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: rect,
                time: Some(time),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), state, input, out),
        );
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: rect,
                time: Some(time + 0.016),
                events: vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), state, input, out),
        );
    }

    fn sample_sidebar_input(viewport: egui::Rect) -> SidebarInput {
        let raw = CoreSidebarInput {
            terminals: vec![
                SidebarTerminal {
                    id: "t1".to_string(),
                    name: "server".to_string(),
                    group: Some("g1".to_string()),
                    hidden: false,
                    attached: None,
                    agent_view_open: false,
                },
                SidebarTerminal {
                    id: "t2".to_string(),
                    name: "hidden-log".to_string(),
                    group: None,
                    hidden: true,
                    attached: None,
                    agent_view_open: false,
                },
            ],
            groups: vec![SidebarGroup {
                id: "g1".to_string(),
                name: "Backend".to_string(),
                layout: GroupLayout::Tile,
                columns: 2,
                members: vec!["t1".to_string()],
            }],
            focused_id: Some("t1".to_string()),
            collapsed_groups: BTreeSet::new(),
            bus: BusSnapshot {
                host: "mac".to_string(),
                reason: String::new(),
                agents: vec![BusAgentInput {
                    name: "lead".to_string(),
                    host: "mac".to_string(),
                    live: true,
                    status: "idle".to_string(),
                    attached: Some(true),
                    ..BusAgentInput::default()
                }],
            },
        };
        let dock = dock_layout(&DockState {
            sidebar: DockPanel::new(true, DockSide::Left),
            ..DockState::default()
        });
        SidebarInput {
            model: build_sidebar(&raw),
            dock_rect: sidebar_dock_rect(viewport, DockSide::Left, &dock),
            platform: Platform::Mac,
            close_hold_ms: 400,
        }
    }

    #[test]
    fn sidebar_lays_out_at_multiple_sizes_and_draws_empty_placeholders() {
        let ctx = egui::Context::default();
        let mut state = SidebarState::new(true, DockSide::Left);
        let empty = SidebarInput::default();
        let mut out = Vec::new();

        for size in [
            egui::vec2(200.0, 140.0),
            egui::vec2(800.0, 600.0),
            egui::vec2(1600.0, 1000.0),
        ] {
            let full = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..egui::RawInput::default()
                },
                |ui| show(ui.ctx(), &mut state, &empty, &mut out),
            );
            assert!(!state.wants_keyboard(), "sidebar must never capture keyboard");
            assert!(
                !full.shapes.is_empty(),
                "empty sidebar must draw 'no terminals' / 'no live agents' at {size:?}"
            );
        }
    }

    #[test]
    fn sidebar_row_clicks_fold_dock_buttons_and_hold_close_emit_verbs() {
        let ctx = egui::Context::default();
        let vp_size = egui::vec2(1000.0, 700.0);
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, vp_size);
        let input = sample_sidebar_input(vp);
        let mut state = SidebarState::new(true, DockSide::Left);
        let mut out = Vec::new();

        // Two warm passes settle Area's sizing pass and populate WidgetRects at left dock (x: 0..248).
        for k in 0..2 {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(vp),
                    time: Some(f64::from(k) * 0.016),
                    ..egui::RawInput::default()
                },
                |ui| show(ui.ctx(), &mut state, &input, &mut out),
            );
        }

        // Header height = 30 + 1 divider + 4 top pad = 35..57 is GroupRow g1;
        // 57..79 is TerminalRow t1; 79..101 is loose TerminalRow t2.
        // 1) Click t1 row body -> `wall.reveal t1`.
        click_at(&ctx, vp_size, egui::pos2(80.0, 68.0), 0.1, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.reveal t1"]);

        // 2) Click g1 fold chevron (x = 4..22, y = 46) -> `wall.sidebar.collapse g1` and folds t1!
        out.clear();
        click_at(&ctx, vp_size, egui::pos2(13.0, 46.0), 0.2, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.sidebar.collapse g1"]);
        assert!(state.is_group_collapsed("g1", false));

        // Now t1 is folded away, so y = 57..79 is loose hidden terminal t2!
        // Clicking t2's show button (second tool from right: close is 223..241, show is 204..222) -> `wall.show t2`.
        out.clear();
        click_at(&ctx, vp_size, egui::pos2(213.0, 68.0), 0.3, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.show t2"]);

        // 3) Hold-to-close on t2's `×` button (x = 232, y = 68):
        // Press at t = 1.0 s -> should NOT fire at t = 1.2 s (200 ms < 400 ms), SHOULD fire at t = 1.5 s (500 ms >= 400 ms)!
        out.clear();
        let close_pos = egui::pos2(232.0, 68.0);
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(vp),
                time: Some(1.0),
                events: vec![
                    egui::Event::PointerMoved(close_pos),
                    egui::Event::PointerButton {
                        pos: close_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert!(out.is_empty(), "hold-close must not fire immediately on press");

        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(vp),
                time: Some(1.2),
                events: vec![egui::Event::PointerMoved(close_pos)],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert!(out.is_empty(), "hold-close must wait full close_hold_ms");

        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(vp),
                time: Some(1.5),
                events: vec![egui::Event::PointerMoved(close_pos)],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert_eq!(out, vec!["wall.close t2"]);

        // Release mouse button cleanly.
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(vp),
                time: Some(1.6),
                events: vec![egui::Event::PointerButton {
                    pos: close_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..egui::RawInput::default()
            },
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );

        // 4) Agent row click: with g1 folded, rows are g1 (35..57), t2 (57..79), space(6)+line(1)+agents_head(24) = 79..110, host_head(20) = 110..130, agent row = 130..152.
        out.clear();
        click_at(&ctx, vp_size, egui::pos2(80.0, 141.0), 1.7, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.attach lead@mac"]);

        // 5) Header close button (`×`, rightmost in 4-button strip at x = 231, y = 15) -> `wall.sidebar close` and sets `open = false`.
        out.clear();
        click_at(&ctx, vp_size, egui::pos2(231.0, 15.0), 1.8, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.sidebar close"]);
        assert!(!state.open);
    }
}
