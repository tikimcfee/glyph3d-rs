//! Native `egui` sidebar panel (`wall/Panel.jsx` + `wall/holdPress.js`),
//! backed by [`wall_core::sidebar`] and [`wall_core::dock`].
//!
//! Contract (`out/wall-port/briefs/P-panels.md`):
//! `pub fn show(ctx: &egui::Context, state: &mut SidebarState, input: &SidebarInput, out: &mut Vec<String>)`
//!
//! - Draws ONE list (2026-11-27; `wall/Sidebar.jsx` drew the terminals and
//!   then the agents): every terminal on the wall and every live agent, an
//!   agent that a terminal shows drawn in that terminal's row. A row carries
//!   its group's chip, a status dot when it has an agent (filled when the
//!   agent is on the wall, a ring when it is not: a click attaches it), the
//!   agent's status, and on hover the terminal's tools and the agent's
//!   details. Hidden terminals are dimmed and marked `"hidden"`.
//! - The header's `group | host` switch (`wall.sidebar mode group|host`)
//!   sections the list: by group (each foldable, with column `−`/`+`,
//!   `tile`/`free` toggle and ungroup tools, then `no group`), or by host.
//!   Every rule about what is listed where is [`wall_core::sidebar::build_sidebar`]'s;
//!   this file draws its rows.
//! - Docks on the left or right viewport edge (using `input.dock_rect` from
//!   [`wall_core::dock::dock_layout`]) or floats freely with draggable header
//!   persistence (`wall.sidebar float x y`).
//! - Is as wide as its edge was dragged ([`super::resize`]): `wall.sidebar
//!   width` docked, `wall.sidebar size` floating; a name is cut only when the
//!   panel is too narrow for it.
//! - A right-click on a row opens its menu (2026-11-27): a terminal's is the
//!   terminal's own, as a right-click on it gives on the canvas (and what
//!   still applies, for a hidden one); an agent's with no terminal offers to
//!   attach it. The row sends `wall.menu row <id> <x> <y>` or `wall.menu
//!   agent <name> <x> <y>`; the menu is the wall's ([`super::context_menu`]).
//! - Never steals keyboard focus from the focused terminal (`wants_keyboard` is
//!   always `false`).

use std::collections::HashMap;
use std::f32::consts::TAU;

use wall_core::chord::chord_label;
use wall_core::dock::{DockLayout, DockSide, PanelKind, PanelSize, DOCK_WIDTHS};
use wall_core::input::Platform;
use wall_core::sidebar::{
    build_sidebar, EntryRow, GroupRow, SectionKind, SectionRow, SidebarInput as CoreSidebarInput,
    SidebarMode, SidebarModel, SidebarRow,
};

use super::resize::{self, ResizeSpec, ResizeState};
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
    let w = layout.widths.sidebar as f32;
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
    /// The list's rows and mode, from [`wall_core::sidebar::build_sidebar`].
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
    /// The sizes set on the panel (the store's): its floating width and
    /// height are read here; its docked width arrives in `dock_rect`.
    pub size: PanelSize,
    /// The edge grips, and a resize drag in progress.
    pub resize: ResizeState,
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
    /// Last rendered panel bounding box when open.
    pub last_rect: Option<egui::Rect>,
    /// Header drag hit rectangle when open.
    pub header_drag_rect: Option<egui::Rect>,
    /// Header dock button rectangles `[left, float, right, close]`.
    pub dock_btn_rects: [Option<egui::Rect>; 4],
    /// Group fold chevron rectangles keyed by group id.
    pub group_fold_rects: HashMap<String, egui::Rect>,
    /// Terminal row click rectangles keyed by terminal id.
    pub term_row_rects: HashMap<String, egui::Rect>,
    /// Click rectangles of the agents with no terminal on the wall, keyed by target.
    pub agent_row_rects: HashMap<String, egui::Rect>,
    /// The header's mode switch halves `[group, host]`.
    pub mode_btn_rects: [Option<egui::Rect>; 2],
    /// Whether the header had room for its title beside the switch.
    pub title_shown: bool,
    /// Every row drawn last frame, top to bottom, as `app.where` prints it:
    /// `g:<group>[<host>=<n>,...]`, `s:none[...]`, `h:<host>[<n>]`,
    /// `t:<terminal>`, `t:<terminal>+<agent>=<status>`, `a:<agent>=<status>`.
    pub rows_drawn: Vec<String>,
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
            size: PanelSize::default(),
            resize: ResizeState::default(),
            wants_keyboard: false,
            fold_state: HashMap::new(),
            drag_offset: None,
            hold_close: None,
            last_rect: None,
            header_drag_rect: None,
            dock_btn_rects: [None; 4],
            group_fold_rects: HashMap::new(),
            term_row_rects: HashMap::new(),
            agent_row_rects: HashMap::new(),
            mode_btn_rects: [None; 2],
            title_shown: false,
            rows_drawn: Vec::new(),
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

const ROW_H: f32 = 22.0;
/// Air above every header but the list's first row.
const SECTION_GAP: f32 = 4.0;
/// Where an entry's name starts: past the group chip and the status marker.
const NAME_X: f32 = 31.0;
/// The name keeps this much (or all of it, when shorter) before a status
/// word, group pill or host tag is allowed beside it.
const NAME_MIN_W: f32 = 80.0;

const FG_TEXT: egui::Color32 = egui::Color32::from_rgb(0xd4, 0xd9, 0xe2);
const FG_HEAD: egui::Color32 = egui::Color32::from_rgb(0xae, 0xb7, 0xc6);
const FG_FOCUS: egui::Color32 = egui::Color32::from_rgb(0x8e, 0xf0, 0xb8);
const FG_DIM: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x73, 0x83);
const FG_FAINT: egui::Color32 = egui::Color32::from_rgb(0x5d, 0x65, 0x74);
const BG_HOVER: egui::Color32 = egui::Color32::from_rgb(0x15, 0x1a, 0x24);
const BG_FOCUS: egui::Color32 = egui::Color32::from_rgb(0x16, 0x25, 0x1e);

fn pointer_in(ui: &egui::Ui, rect: egui::Rect) -> bool {
    ui.ctx()
        .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        .is_some_and(|p| rect.contains(p))
}

/// A group's chip: a short bar in its colour, on its header and on every
/// member row.
fn paint_group_chip(painter: &egui::Painter, x: f32, center_y: f32, color: egui::Color32) {
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(x, center_y - 6.0),
            egui::pos2(x + 3.0, center_y + 6.0),
        ),
        egui::CornerRadius::same(1),
        color,
    );
}

/// What `app.where` prints for a drawn row: [`SidebarRow::token`], then a
/// header's count by host (`g:g-1[laptop-2=2,workstation=1]`, `h:workstation[3]`)
/// or an agent's status (`t:gx-lead+lead@laptop-2=processing`).
fn report_token(row: &SidebarRow) -> String {
    let by_host = |hosts: &[wall_core::sidebar::HostCount]| -> String {
        hosts
            .iter()
            .map(|h| {
                let host = if h.host.is_empty() { "local" } else { h.host.as_str() };
                format!("{host}={}", h.count)
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let token = match row {
        SidebarRow::Group(g) => format!("{}[{}]", row.token(), by_host(&g.hosts)),
        SidebarRow::Section(s) => match s.kind {
            SectionKind::NoGroup => format!("{}[{}]", row.token(), by_host(&s.hosts)),
            SectionKind::Host => format!("{}[{}]", row.token(), s.count),
        },
        SidebarRow::Entry(e) => match &e.agent {
            Some(a) => format!("{}={}", row.token(), a.status.word()),
            None => row.token(),
        },
    };
    token
        .chars()
        .map(|c| if c.is_whitespace() || c == ';' { '_' } else { c })
        .collect()
}

/// A header's label: the name, then its count by host, dimmer and smaller.
/// `counts` are the forms of the count, longest first
/// (`"3 (laptop-2 2, workstation 1)"`, then `"laptop-2 2, workstation 1"`).
/// The first of these that fits beside the name is drawn: each form after a
/// `·`, then the last with no `·`; when none fits, that is cut with `…` (the
/// header's hover text has all of it). 248 px does not hold the long form
/// for two hosts with nine-letter names, and holds the short one for a group
/// name of up to seven letters.
fn paint_header_label(
    painter: &egui::Painter,
    left: f32,
    center_y: f32,
    max_w: f32,
    name: &str,
    name_color: egui::Color32,
    counts: &[&str],
) {
    let name_galley = layout_elided(
        painter,
        name,
        egui::FontId::monospace(12.5),
        name_color,
        max_w.max(16.0),
    );
    let name_w = name_galley.size().x;
    painter.galley(
        egui::pos2(left, center_y - name_galley.size().y * 0.5),
        name_galley,
        name_color,
    );
    let rest = max_w - name_w - 5.0;
    if rest >= 24.0 {
        let font = egui::FontId::monospace(10.0);
        let shortest = counts.last().copied().unwrap_or_default();
        let forms = counts
            .iter()
            .map(|count| format!("· {count}"))
            .chain(std::iter::once(shortest.to_string()));
        let mut whole = None;
        for form in forms {
            let galley = painter.layout_no_wrap(form, font.clone(), FG_FAINT);
            if galley.size().x <= rest {
                whole = Some(galley);
                break;
            }
        }
        let count_galley =
            whole.unwrap_or_else(|| layout_elided(painter, shortest, font, FG_FAINT, rest));
        painter.galley(
            egui::pos2(
                left + name_w + 5.0,
                center_y - count_galley.size().y * 0.5 + 0.5,
            ),
            count_galley,
            FG_FAINT,
        );
    }
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
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(panel_w, ROW_H), egui::Sense::hover());
    let pointer_in_row = pointer_in(ui, row_rect);

    if ui.is_rect_visible(row_rect) && pointer_in_row {
        ui.painter()
            .rect_filled(row_rect, egui::CornerRadius::ZERO, BG_HOVER);
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
    state.group_fold_rects.insert(g.id.clone(), fold_resp.rect);
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
    if ui.is_rect_visible(row_rect) {
        let chip_x = row_ui.cursor().min.x + 2.0;
        let color = parse_hex_color(g.color).unwrap_or(FG_FAINT);
        paint_group_chip(row_ui.painter(), chip_x, row_rect.center().y, color);
        let name_x = chip_x + 8.0;
        paint_header_label(
            row_ui.painter(),
            name_x,
            row_rect.center().y,
            row_rect.right() - visual_tools_w - name_x,
            &g.name,
            FG_HEAD,
            &[&g.count_label, &g.count_short],
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
    // The label is cut to the row; the hover text is all of it.
    let body_hit = body_hit.on_hover_text(sanitize_egui_text(&format!(
        "{} · {}",
        g.name, g.count_label
    )));
    if body_hit.clicked() && !tool_interacted {
        push_cmd(out, &g.click_cmd);
    }
}

/// A plain header: `no group` in group mode, a host in host mode.
fn draw_section_row(ui: &mut egui::Ui, panel_w: f32, s: &SectionRow) {
    let (row_rect, resp) =
        ui.allocate_exact_size(egui::vec2(panel_w, ROW_H), egui::Sense::hover());
    if ui.is_rect_visible(row_rect) {
        let name_color = match s.kind {
            SectionKind::Host => FG_HEAD,
            SectionKind::NoGroup => egui::Color32::from_rgb(0x7c, 0x85, 0x96),
        };
        let left = row_rect.left() + 10.0;
        paint_header_label(
            ui.painter(),
            left,
            row_rect.center().y,
            row_rect.right() - 6.0 - left,
            &s.label,
            name_color,
            &[&s.count_label, &s.count_short],
        );
    }
    let tip = match s.kind {
        SectionKind::Host if s.local => format!("{} · {} · this machine", s.label, s.count_label),
        _ => format!("{} · {}", s.label, s.count_label),
    };
    let _ = resp.on_hover_text(sanitize_egui_text(&tip));
}

/// The mark before a row's name. An agent's is a dot in its status colour,
/// filled when a terminal on the wall shows it and a ring when none does (the
/// old "on wall" tag); a terminal with no agent gets a faint prompt.
fn paint_row_marker(painter: &egui::Painter, center: egui::Pos2, e: &EntryRow, fade: f32) {
    match &e.agent {
        Some(a) => {
            let color = parse_hex_color(a.status.color())
                .unwrap_or(egui::Color32::from_rgb(0x8b, 0x95, 0xa7))
                .gamma_multiply(fade);
            if a.on_wall.is_some() {
                painter.circle_filled(center, 3.5, color);
            } else {
                painter.circle_stroke(center, 3.0, egui::Stroke::new(1.3, color));
            }
        }
        None => {
            let stroke = egui::Stroke::new(1.3, FG_FAINT.gamma_multiply(fade));
            let tip = egui::pos2(center.x + 1.8, center.y);
            painter.line_segment([egui::pos2(center.x - 1.8, center.y - 3.0), tip], stroke);
            painter.line_segment([egui::pos2(center.x - 1.8, center.y + 3.0), tip], stroke);
        }
    }
}

/// One row of the list: a terminal (with its tools on hover, and its agent's
/// status when it shows one) or an agent with no terminal on the wall.
fn draw_entry_row(
    ui: &mut egui::Ui,
    panel_w: f32,
    e: &EntryRow,
    platform: Platform,
    close_hold_ms: u64,
    state: &mut SidebarState,
    out: &mut Vec<String>,
) {
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(panel_w, ROW_H), egui::Sense::hover());
    let pointer_in_row = pointer_in(ui, row_rect);
    let center_y = row_rect.center().y;

    let tools_hit_w = match &e.terminal {
        Some(t) if t.leave_group_cmd.is_some() => 4.0 * 19.0 + 8.0,
        Some(_) => 3.0 * 19.0 + 8.0,
        None => 0.0,
    };
    let tools_shown = e.terminal.is_some() && pointer_in_row;

    if ui.is_rect_visible(row_rect) {
        if e.focused {
            ui.painter()
                .rect_filled(row_rect, egui::CornerRadius::ZERO, BG_FOCUS);
        } else if pointer_in_row {
            ui.painter()
                .rect_filled(row_rect, egui::CornerRadius::ZERO, BG_HOVER);
        }

        // A hidden terminal's row is dimmed whole.
        let fade = if e.hidden { 0.5 } else { 1.0 };
        if let Some(chip) = &e.chip {
            let color = parse_hex_color(chip.color).unwrap_or(FG_FAINT);
            paint_group_chip(
                ui.painter(),
                row_rect.left() + 10.0,
                center_y,
                color.gamma_multiply(fade),
            );
        }
        paint_row_marker(
            ui.painter(),
            egui::pos2(row_rect.left() + 22.0, center_y),
            e,
            fade,
        );

        let name_x = row_rect.left() + NAME_X;
        let name_fg = if e.hidden || !e.interactive {
            FG_DIM
        } else if e.focused {
            FG_FOCUS
        } else {
            FG_TEXT
        };
        let small = egui::FontId::monospace(10.5);
        let name_full =
            ui.painter()
                .layout_no_wrap(sanitize_egui_text(&e.name), egui::FontId::monospace(12.5), name_fg);
        let name_need = name_full.size().x.min(NAME_MIN_W);

        // What sits right of the name, by what it is worth when the row is
        // too narrow for all of it: `hidden`, the agent's status, then the
        // group's name (host mode) or the host's (group mode). The row's
        // tools take the place of the last two while the pointer is on it.
        let right_edge = row_rect.right() - if tools_shown { tools_hit_w } else { 8.0 };
        let mut room = right_edge - name_x;
        let mut take = |galley: &std::sync::Arc<egui::Galley>, pad: f32| -> bool {
            let w = galley.size().x + pad + 6.0;
            if room - w >= name_need {
                room -= w;
                true
            } else {
                false
            }
        };

        let hidden_galley = e.hidden.then(|| {
            ui.painter()
                .layout_no_wrap("hidden".to_string(), small.clone(), FG_FAINT)
        });
        let hidden_galley = hidden_galley.filter(|g| take(g, 0.0));

        let status_galley = match (&e.agent, tools_shown) {
            (Some(a), false) => {
                let color = parse_hex_color(a.status.color())
                    .unwrap_or(egui::Color32::from_rgb(0x8b, 0x95, 0xa7))
                    .gamma_multiply(fade);
                let galley =
                    ui.painter()
                        .layout_no_wrap(a.status.word().to_string(), small.clone(), color);
                take(&galley, 0.0).then_some((galley, color))
            }
            _ => None,
        };

        // (galley, colour, drawn as a pill)
        let side_galley = if tools_shown {
            None
        } else if let Some(chip) = e.chip.as_ref().filter(|c| c.show_name) {
            let color = parse_hex_color(chip.color)
                .unwrap_or(FG_FAINT)
                .gamma_multiply(fade);
            let galley = layout_elided(ui.painter(), &chip.name, small.clone(), color, 72.0);
            take(&galley, 8.0).then_some((galley, color, true))
        } else if let Some(host) = &e.host_tag {
            let galley = layout_elided(
                ui.painter(),
                &format!("@{host}"),
                small.clone(),
                FG_FAINT,
                84.0,
            );
            take(&galley, 0.0).then_some((galley, FG_FAINT, false))
        } else {
            None
        };

        let mut x = right_edge;
        if let Some((galley, color)) = status_galley {
            x -= galley.size().x;
            ui.painter().galley(
                egui::pos2(x, center_y - galley.size().y * 0.5),
                galley,
                color,
            );
            x -= 6.0;
        }
        if let Some(galley) = hidden_galley {
            x -= galley.size().x;
            ui.painter().galley(
                egui::pos2(x, center_y - galley.size().y * 0.5),
                galley,
                FG_FAINT,
            );
            x -= 6.0;
        }
        if let Some((galley, color, pill)) = side_galley {
            let pad = if pill { 4.0 } else { 0.0 };
            x -= galley.size().x + pad;
            if pill {
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x - pad, center_y - 7.5),
                        egui::pos2(x + galley.size().x + pad, center_y + 7.5),
                    ),
                    egui::CornerRadius::same(4),
                    color.gamma_multiply(0.16),
                );
            }
            ui.painter().galley(
                egui::pos2(x, center_y - galley.size().y * 0.5),
                galley,
                color,
            );
        }

        let name_galley = layout_elided(
            ui.painter(),
            &e.name,
            egui::FontId::monospace(12.5),
            name_fg,
            room.max(20.0),
        );
        ui.painter().galley(
            egui::pos2(name_x, center_y - name_galley.size().y * 0.5),
            name_galley,
            name_fg,
        );
    }

    let mut tool_interacted = false;
    if let Some(t) = &e.terminal {
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

        let (vis_icon, vis_tip) = if e.hidden {
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
    }

    // The row outside its tools: a terminal's reveals it, an agent's attaches.
    let click_right = (row_rect.right() - tools_hit_w).max(row_rect.left() + 20.0);
    let row_id = match (&e.terminal, &e.agent) {
        (Some(t), _) => ui.make_persistent_id(("wall_sb_term_row", &t.id)),
        (None, Some(a)) => ui.make_persistent_id(("wall_sb_agent_row", &a.target)),
        (None, None) => ui.make_persistent_id(("wall_sb_row", &e.name)),
    };
    let row_hit = ui.interact(
        egui::Rect::from_min_max(row_rect.min, egui::pos2(click_right, row_rect.bottom())),
        row_id,
        egui::Sense::click(),
    );
    match (&e.terminal, &e.agent) {
        (Some(t), _) => {
            state.term_row_rects.insert(t.id.clone(), row_hit.rect);
        }
        (None, Some(a)) => {
            state.agent_row_rects.insert(a.target.clone(), row_hit.rect);
        }
        (None, None) => {}
    }
    if e.interactive && row_hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let row_hit = if e.tooltip.is_empty() {
        row_hit
    } else {
        row_hit.on_hover_text(sanitize_egui_text(&e.tooltip))
    };
    // A right-click anywhere on the row (its tools too) opens its menu
    // where the pointer is: a terminal's own menu, as on the canvas, or an
    // agent's. On a Mac a Ctrl+click is a right-click here as it is there.
    let ctrl_click = platform == Platform::Mac
        && row_hit.clicked()
        && ui.input(|i| i.modifiers.ctrl);
    let right_click = ui.rect_contains_pointer(row_rect)
        && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary));
    if (ctrl_click || right_click) && !e.menu_cmd.is_empty() {
        if let Some(at) = ui.input(|i| i.pointer.interact_pos()) {
            let mut cmd = e.menu_cmd.clone();
            cmd.push((at.x.round() as i64).to_string());
            cmd.push((at.y.round() as i64).to_string());
            push_cmd(out, &cmd);
        }
    } else if row_hit.clicked() && !tool_interacted {
        push_cmd(out, &e.click_cmd);
    }
}

/// The header's `group | host` switch: the lit half is how the list is
/// sectioned, and a click on the other runs `wall.sidebar mode <it>`.
fn draw_mode_toggle(
    ui: &mut egui::Ui,
    right: f32,
    center_y: f32,
    mode: SidebarMode,
    state: &mut SidebarState,
    out: &mut Vec<String>,
) -> f32 {
    // Small enough that the title beside it is drawn whole at the docked
    // width: 248 px holds the title, this switch and four dock buttons.
    let font = egui::FontId::monospace(10.5);
    let halves = [SidebarMode::Group, SidebarMode::Host];
    let widths = halves.map(|m| {
        ui.painter()
            .layout_no_wrap(m.as_str().to_string(), font.clone(), FG_TEXT)
            .size()
            .x
            + 8.0
    });
    let left = right - widths[0] - widths[1];
    let mut x = left;
    for (k, half) in halves.into_iter().enumerate() {
        let rect = egui::Rect::from_min_max(
            egui::pos2(x, center_y - 9.0),
            egui::pos2(x + widths[k], center_y + 9.0),
        );
        x += widths[k];
        state.mode_btn_rects[k] = Some(rect);
        let resp = ui.interact(
            rect,
            ui.make_persistent_id(("wall_sidebar_mode", half.as_str())),
            egui::Sense::click(),
        );
        let active = half == mode;
        if ui.is_rect_visible(rect) {
            if active || resp.hovered() {
                let bg = if active {
                    egui::Color32::from_rgb(0x1d, 0x23, 0x30)
                } else {
                    egui::Color32::from_rgb(0x23, 0x2a, 0x3a)
                };
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(4), bg);
            }
            let fg = if active || resp.hovered() {
                egui::Color32::from_rgb(0xe6, 0xe9, 0xef)
            } else {
                egui::Color32::from_rgb(0x8b, 0x95, 0xa7)
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                half.as_str(),
                font.clone(),
                fg,
            );
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let tip = match half {
            SidebarMode::Group => "list by group: each group's members, then the rest",
            SidebarMode::Host => "list by host: each machine's terminals and agents",
        };
        if resp.on_hover_text(tip).clicked() && !active {
            push_cmd(out, &half.verb());
        }
    }
    left
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
    state.group_fold_rects.clear();
    state.term_row_rects.clear();
    state.agent_row_rects.clear();
    state.rows_drawn.clear();
    state.mode_btn_rects = [None; 2];
    if !state.open {
        state.drag_offset = None;
        state.hold_close = None;
        state.last_rect = None;
        state.header_drag_rect = None;
        state.dock_btn_rects = [None; 4];
        state.resize.reset();
        return;
    }

    let viewport = safe_viewport_rect(ctx);
    let limits = PanelKind::Sidebar.limits();
    let is_float = state.side == DockSide::Float;

    let (pos, panel_w, fixed_h, max_h) = if is_float {
        let panel_w = resize::float_width(
            &state.resize,
            &state.size,
            limits.float_default_width as f32,
            viewport,
        );
        // A drag on an edge pins the panel where the drag has it.
        if let Some(origin) = resize::float_origin(&state.resize) {
            state.float_pos = origin;
        }
        let held_h = resize::float_height(&state.resize, &state.size, viewport);
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
        // The floating panel's 1 px border is outside what it holds: the
        // size is the box with it.
        let rim = 2.0 * resize::FLOAT_RIM_PX;
        // Its bottom edge stays in the window, wherever it was put.
        let room = resize::room_below(clamped.y, viewport);
        let held_h = held_h.map(|h| h.min(room));
        (
            clamped,
            panel_w - rim,
            held_h.map(|h| h - rim),
            held_h.map_or(((viewport.height() - 120.0).max(96.0)).min(room), |h| h - rim),
        )
    } else {
        let r = input
            .dock_rect
            .unwrap_or_else(|| fallback_dock_rect(viewport, state.side));
        (
            r.min,
            r.width().max(120.0_f32.min(viewport.width())),
            Some(r.height().max(48.0)),
            r.height().max(48.0),
        )
    };

    egui::Area::new(egui::Id::new(SIDEBAR_AREA_ID))
        .order(egui::Order::Middle)
        .fixed_pos(pos)
        .fade_in(false)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_sidebar initial sizing");
            }
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
                    state.header_drag_rect = Some(drag_rect);
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

                    // Right to left: the dock buttons, the mode switch, and
                    // the title in what is left.
                    let strip_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            head_rect.right() - 6.0 - (4.0 * 18.0 + 6.0),
                            head_rect.center().y - 9.0,
                        ),
                        egui::vec2(4.0 * 18.0 + 6.0, 18.0),
                    );
                    for idx in 0..4 {
                        let x0 = strip_rect.left() + idx as f32 * 20.0;
                        state.dock_btn_rects[idx] = Some(egui::Rect::from_min_size(
                            egui::pos2(x0, strip_rect.top()),
                            egui::vec2(18.0, 18.0),
                        ));
                    }
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

                    let toggle_left = draw_mode_toggle(
                        ui,
                        strip_rect.left() - 6.0,
                        head_rect.center().y,
                        input.model.mode,
                        state,
                        out,
                    );
                    // The title is drawn whole or not at all: the switch
                    // and the dock buttons are what the header is for.
                    let title_left = head_rect.left() + 10.0;
                    let title_color = egui::Color32::from_rgb(0x7c, 0x85, 0x96);
                    let title = ui.painter().layout_no_wrap(
                        "terminals".to_string(),
                        egui::FontId::monospace(12.5),
                        title_color,
                    );
                    state.title_shown = title_left + title.size().x + 4.0 <= toggle_left;
                    if state.title_shown {
                        ui.painter().galley(
                            egui::pos2(title_left, head_rect.center().y - title.size().y * 0.5),
                            title,
                            title_color,
                        );
                    }

                    // Header bottom divider (`1px solid #1e2430`).
                    let (div_rect, _) =
                        ui.allocate_exact_size(egui::vec2(panel_w, 1.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        div_rect,
                        egui::CornerRadius::ZERO,
                        egui::Color32::from_rgb(0x1e, 0x24, 0x30),
                    );

                    // Scrollable body: the one list.
                    let body_max_h = (max_h - head_h - 1.0).max(40.0);
                    egui::ScrollArea::vertical()
                        .id_salt("wall_sidebar_body")
                        .max_height(body_max_h)
                        .auto_shrink([false, fixed_h.is_none()])
                        .show(ui, |ui| {
                            ui.set_width(panel_w);
                            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                            ui.add_space(4.0);

                            if input.model.terminals_empty {
                                let (empty_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(panel_w, ROW_H),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    egui::pos2(empty_rect.left() + 10.0, empty_rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "no terminals",
                                    egui::FontId::monospace(12.5),
                                    FG_DIM,
                                );
                            }

                            // A folded group's members are skipped here as
                            // well as left out by the model, so a fold shows
                            // on the click's own frame.
                            let mut in_folded_group = false;
                            let mut first = !input.model.terminals_empty;
                            for row in &input.model.rows {
                                match row {
                                    SidebarRow::Group(g) => {
                                        if !first {
                                            ui.add_space(SECTION_GAP);
                                        }
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
                                        in_folded_group =
                                            state.is_group_collapsed(&g.id, g.collapsed);
                                    }
                                    SidebarRow::Section(s) => {
                                        if !first {
                                            ui.add_space(SECTION_GAP);
                                        }
                                        draw_section_row(ui, panel_w, s);
                                        in_folded_group = false;
                                    }
                                    SidebarRow::Entry(e) => {
                                        if in_folded_group {
                                            continue;
                                        }
                                        draw_entry_row(
                                            ui,
                                            panel_w,
                                            e,
                                            input.platform,
                                            input.close_hold_ms,
                                            state,
                                            out,
                                        );
                                    }
                                }
                                first = false;
                                state.rows_drawn.push(report_token(row));
                            }

                            if let Some(empty) = &input.model.agents_empty {
                                let (erect, eresp) = ui.allocate_exact_size(
                                    egui::vec2(panel_w, ROW_H),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    egui::pos2(erect.left() + 10.0, erect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    empty.text,
                                    egui::FontId::monospace(12.0),
                                    FG_DIM,
                                );
                                if let Some(reason) = &empty.reason {
                                    let _ = eresp.on_hover_text(reason);
                                }
                            }
                            ui.add_space(4.0);
                        });
                });

            // Inner vertical edge border when docked (`Sidebar.jsx:134`: `1px solid #222834`).
            state.last_rect = Some(frame_resp.response.rect);
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

            // The edges that resize it, over the rows and the scroll bar.
            resize::grips(
                ui,
                &mut state.resize,
                &ResizeSpec {
                    verb: "wall.sidebar",
                    side: state.side,
                    rect: frame_resp.response.rect,
                    limits,
                    viewport,
                    centered: false,
                },
                out,
            );
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

    /// One group (`g1`: `t1`, focused), a hidden loose terminal, a local
    /// agent in its `gx-` terminal, and one with no terminal on the wall.
    fn sample_sidebar_input(viewport: egui::Rect, mode: SidebarMode) -> SidebarInput {
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
                SidebarTerminal {
                    id: "gx-coder".to_string(),
                    name: "coder".to_string(),
                    group: None,
                    hidden: false,
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
                agents: vec![
                    BusAgentInput {
                        name: "lead".to_string(),
                        host: "mac".to_string(),
                        live: true,
                        status: "idle".to_string(),
                        attached: Some(true),
                        ..BusAgentInput::default()
                    },
                    BusAgentInput {
                        name: "coder".to_string(),
                        host: "mac".to_string(),
                        live: true,
                        status: "processing".to_string(),
                        attached: Some(true),
                        ..BusAgentInput::default()
                    },
                    BusAgentInput {
                        name: "gate".to_string(),
                        host: "linux".to_string(),
                        live: true,
                        status: "waiting".to_string(),
                        attached: None,
                        ..BusAgentInput::default()
                    },
                ],
            },
            mode,
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

    /// Two passes with no input: the first sizes the `Area`, the second
    /// records every rect at the left dock (x: 0..248).
    fn settle(
        ctx: &egui::Context,
        vp: egui::Rect,
        time: f64,
        state: &mut SidebarState,
        input: &SidebarInput,
        out: &mut Vec<String>,
    ) {
        for k in 0..2 {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(vp),
                    time: Some(time + f64::from(k) * 0.016),
                    ..egui::RawInput::default()
                },
                |ui| show(ui.ctx(), state, input, out),
            );
        }
    }

    /// A press and release of `button` at `pos` in a 1280 x 800 window, as
    /// two passes; what was sent.
    fn press_at(
        ctx: &egui::Context,
        pos: egui::Pos2,
        button: egui::PointerButton,
        modifiers: egui::Modifiers,
        time: f64,
        state: &mut SidebarState,
        input: &SidebarInput,
    ) -> Vec<String> {
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let mut out = Vec::new();
        for (k, pressed) in [true, false].into_iter().enumerate() {
            // (The modifiers held arrive as an event of their own.)
            let mut events = vec![egui::Event::ModifiersChanged(modifiers)];
            if pressed {
                events.push(egui::Event::PointerMoved(pos));
            }
            events.push(egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            });
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(vp),
                    time: Some(time + k as f64 * 0.016),
                    events,
                    ..egui::RawInput::default()
                },
                |ui| show(ui.ctx(), state, input, &mut out),
            );
        }
        out
    }

    #[test]
    fn a_right_click_on_a_row_asks_for_its_menu_where_the_pointer_is() {
        let ctx = egui::Context::default();
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let mut state = SidebarState::new(true, DockSide::Left);
        let input = sample_sidebar_input(vp, SidebarMode::Group);
        let mut out = Vec::new();
        settle(&ctx, vp, 1.0, &mut state, &input, &mut out);
        assert!(out.is_empty());
        let right = egui::PointerButton::Secondary;
        let plain = egui::Modifiers::NONE;

        // A terminal's row: its own menu, by its id, at the pointer.
        let t1 = state.term_row_rects["t1"].center();
        assert_eq!(
            press_at(&ctx, t1, right, plain, 2.0, &mut state, &input),
            vec![format!("wall.menu row t1 {} {}", t1.x.round(), t1.y.round())]
        );
        // A hidden terminal's row has one too: it is the only place it has.
        let t2 = state.term_row_rects["t2"].center();
        assert_eq!(
            press_at(&ctx, t2, right, plain, 3.0, &mut state, &input),
            vec![format!("wall.menu row t2 {} {}", t2.x.round(), t2.y.round())]
        );
        // Anywhere on the row counts, its tools at the right end too.
        let over_tools = egui::pos2(240.0, t1.y);
        assert!(!state.term_row_rects["t1"].contains(over_tools));
        assert_eq!(
            press_at(&ctx, over_tools, right, plain, 4.0, &mut state, &input),
            vec![format!("wall.menu row t1 240 {}", t1.y.round())]
        );

        // An agent with no terminal on the wall: the agent's menu, by its name.
        let (target, rect) = state
            .agent_row_rects
            .iter()
            .find(|(k, _)| k.starts_with("lead"))
            .map(|(k, r)| (k.clone(), *r))
            .expect("lead has a row of its own");
        let at = rect.center();
        assert_eq!(
            press_at(&ctx, at, right, plain, 5.0, &mut state, &input),
            vec![format!("wall.menu agent {target} {} {}", at.x.round(), at.y.round())]
        );

        // A plain click is still the row's own: reveal, or attach.
        let left = egui::PointerButton::Primary;
        assert_eq!(
            press_at(&ctx, t1, left, plain, 6.0, &mut state, &input),
            vec!["wall.reveal t1"]
        );
        assert_eq!(
            press_at(&ctx, at, left, plain, 7.0, &mut state, &input),
            vec![format!("wall.attach {target}")]
        );
        // On a Mac, Ctrl+click is the right-click it is on the canvas.
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..egui::Modifiers::NONE
        };
        assert_eq!(
            press_at(&ctx, t1, left, ctrl, 8.0, &mut state, &input),
            vec![format!("wall.menu row t1 {} {}", t1.x.round(), t1.y.round())]
        );
        let other = SidebarInput {
            platform: Platform::Other,
            ..input.clone()
        };
        assert_eq!(
            press_at(&ctx, t1, left, ctrl, 9.0, &mut state, &other),
            vec!["wall.reveal t1"]
        );

        // A right-click on a header, or beside the panel, asks for nothing.
        let header = state.group_fold_rects["g1"].center() + egui::vec2(60.0, 0.0);
        assert!(press_at(&ctx, header, right, plain, 10.0, &mut state, &input).is_empty());
        let outside = egui::pos2(600.0, t1.y);
        assert!(press_at(&ctx, outside, right, plain, 11.0, &mut state, &input).is_empty());
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
        let input = sample_sidebar_input(vp, SidebarMode::Group);
        let mut state = SidebarState::new(true, DockSide::Left);
        let mut out = Vec::new();
        settle(&ctx, vp, 0.0, &mut state, &input, &mut out);

        // One list: the group and its member, then the rest by host. The
        // agent whose terminal is on the wall is that terminal's row.
        assert_eq!(
            state.rows_drawn,
            vec![
                "g:g1[mac=1]",
                "t:t1",
                "s:none[mac=3,linux=1]",
                "t:t2",
                "t:gx-coder+coder@mac=processing",
                "a:lead@mac=idle",
                "a:gate@linux=waiting",
            ]
        );
        // The header is 30 + a 1 px divider + 4 px of air, and a row is 22:
        // g1 is 35..57 and t1 57..79. The next header sits 4 px lower.
        let t1 = state.term_row_rects["t1"];
        assert_eq!((t1.top(), t1.bottom()), (57.0, 79.0));
        let t2 = state.term_row_rects["t2"];
        assert_eq!((t2.top(), t2.bottom()), (105.0, 127.0));
        assert!(!state.term_row_rects.contains_key("lead@mac"));
        assert_eq!(state.agent_row_rects.len(), 2);

        // 1) A terminal's row reveals it, with or without an agent in it.
        click_at(&ctx, vp_size, t1.center(), 0.1, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.reveal t1"]);
        out.clear();
        let coder = state.term_row_rects["gx-coder"].center();
        click_at(&ctx, vp_size, coder, 0.15, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.reveal gx-coder"]);

        // 2) An agent with no terminal on the wall: a click attaches it.
        out.clear();
        let lead = state.agent_row_rects["lead@mac"].center();
        click_at(&ctx, vp_size, lead, 0.2, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.attach lead@mac"]);

        // 3) The fold chevron folds g1 at once: t1's row goes, the rest move up.
        out.clear();
        let fold = state.group_fold_rects["g1"].center();
        click_at(&ctx, vp_size, fold, 0.3, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.sidebar.collapse g1"]);
        assert!(state.is_group_collapsed("g1", false));
        out.clear();
        settle(&ctx, vp, 0.4, &mut state, &input, &mut out);
        assert!(!state.term_row_rects.contains_key("t1"));
        assert_eq!(state.rows_drawn[..3], ["g:g1[mac=1]", "s:none[mac=3,linux=1]", "t:t2"]);
        let t2 = state.term_row_rects["t2"];
        assert_eq!((t2.top(), t2.bottom()), (83.0, 105.0));

        // 4) The hidden terminal's show button, second tool from the right
        //    (close is 224..242, show 205..223) -> `wall.show t2`.
        out.clear();
        let t2_y = t2.center().y;
        click_at(&ctx, vp_size, egui::pos2(213.0, t2_y), 0.5, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.show t2"]);

        // 5) Hold-to-close on t2's `×` (x = 232): pressed at t = 1.0 s it must
        //    NOT fire at 1.2 s (200 ms < 400 ms) and must at 1.5 s.
        out.clear();
        let close_pos = egui::pos2(232.0, t2_y);
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

        // 6) An agent's row has no tools: where a terminal's `×` would be,
        //    a click still attaches.
        out.clear();
        let lead_y = state.agent_row_rects["lead@mac"].center().y;
        click_at(&ctx, vp_size, egui::pos2(232.0, lead_y), 1.7, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.attach lead@mac"]);

        // 7) Header close button (`×`, rightmost in 4-button strip at x = 231, y = 15) -> `wall.sidebar close` and sets `open = false`.
        out.clear();
        click_at(&ctx, vp_size, egui::pos2(231.0, 15.0), 1.8, &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.sidebar close"]);
        assert!(!state.open);
    }

    #[test]
    fn header_switch_emits_the_mode_verb_and_host_mode_draws_host_sections() {
        let ctx = egui::Context::default();
        let vp_size = egui::vec2(1000.0, 700.0);
        let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, vp_size);
        let by_group = sample_sidebar_input(vp, SidebarMode::Group);
        let mut state = SidebarState::new(true, DockSide::Left);
        let mut out = Vec::new();
        settle(&ctx, vp, 0.0, &mut state, &by_group, &mut out);

        // Both halves sit in the header, left of the dock buttons and clear
        // of the title.
        let [Some(group_btn), Some(host_btn)] = state.mode_btn_rects else {
            panic!("the header draws both halves of the switch");
        };
        let dock_left = state.dock_btn_rects[0].expect("dock buttons drawn");
        assert!(group_btn.right() <= host_btn.left() + 0.01);
        assert!(host_btn.right() < dock_left.left());
        assert!(state.title_shown, "room for the title: {group_btn:?}");
        assert!(group_btn.top() >= 0.0 && group_btn.bottom() <= 30.0);

        // The lit half does nothing; the other asks for its mode.
        click_at(&ctx, vp_size, group_btn.center(), 0.1, &mut state, &by_group, &mut out);
        assert!(out.is_empty(), "already by group: {out:?}");
        click_at(&ctx, vp_size, host_btn.center(), 0.2, &mut state, &by_group, &mut out);
        assert_eq!(out, vec!["wall.sidebar mode host"]);

        // The wall answers with a host-mode model: each host, its rows by group.
        out.clear();
        let by_host = sample_sidebar_input(vp, SidebarMode::Host);
        settle(&ctx, vp, 0.3, &mut state, &by_host, &mut out);
        assert_eq!(
            state.rows_drawn,
            vec![
                "h:mac[4]",
                "t:t1",
                "t:t2",
                "t:gx-coder+coder@mac=processing",
                "a:lead@mac=idle",
                "h:linux[1]",
                "a:gate@linux=waiting",
            ]
        );
        assert!(state.group_fold_rects.is_empty(), "no group headers by host");
        let gate = state.agent_row_rects["gate@linux"].center();
        click_at(&ctx, vp_size, gate, 0.4, &mut state, &by_host, &mut out);
        assert_eq!(out, vec!["wall.attach gate@linux"]);
        out.clear();
        let group_btn = state.mode_btn_rects[0].unwrap().center();
        click_at(&ctx, vp_size, group_btn, 0.5, &mut state, &by_host, &mut out);
        assert_eq!(out, vec!["wall.sidebar mode group"]);
    }
}
