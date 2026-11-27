//! The settings window (2026-11-27; `wall/SettingsPanel.jsx` was one folding column).
//!
//! Search across the top, above everything; a list of sections down the
//! left; the chosen section's settings on the right, each a plain label, one
//! line saying what it does, and its control. A search reads every section
//! and says which section each match is in; a click on a section narrows the
//! results to it. Narrower than [`TWO_PANE_MIN_W`] (docked, until its edge is
//! dragged), the sections are a row of chips above the settings.
//!
//! What is shown for a query and a section is `wall_core::settings::settings_view`;
//! this file draws it and sends `wall.config <key> <value>`, `wall.settings
//! section <id>` and the panel verbs (`wall.settings close|left|…`).

use std::collections::{BTreeMap, BTreeSet};

use wall_core::chord::chord_label;
use wall_core::config::{check_config, default_knob_value, WallConfig};
use wall_core::dock::PanelKind;
use wall_core::input::Platform;
use wall_core::settings::{
    settings_view, step_section, KnobDef, KnobKind, KnobValue, SettingRow, SettingsView,
};
use wall_core::store::{PanelSide, SettingsPanelState as SettingsPlacement};

use super::super::resize::{self, ResizeSpec, ResizeState};
use super::{
    dock_buttons_rtl, handle_head_drag, icon_btn, marked_layout_job, panel_viewport_rect,
    resolve_panel_box, settings_panel_bg, ChromeIcon, HeadDragState, ACTIVE_ROW_BG, BORDER, BRIGHT,
    BTN_ON_BG, DESC_TEXT, ERR_RED, FIELD_BG, HEAD_BORDER, HOVER_ROW_BG, KEY_TEXT, LABEL, MUTED,
    OK_GREEN,
};

/// Browser-only or no-op knobs kept in `WallConfig` / `WallStore` so imported
/// browser layouts round-trip without error, but hidden from the native Settings UI.
const NATIVE_HIDDEN_KNOBS: &[&str] = &["renderLoop", "traceInput", "menuHint"];

/// Plain data read by [`show`] (`<Panel>Input` in `P-panels.md`).
#[derive(Clone, Debug)]
pub struct SettingsInput {
    /// Open flag, side (`Left`/`Right`/`Float`), floating coordinates, size,
    /// and the section the window shows.
    pub placement: SettingsPlacement,
    /// Current validated knob values (`wall_core::config::WallConfig`).
    pub config: WallConfig,
    /// Keys of knobs currently dialled away from their defaults (shows `↺` reset button).
    pub dialled: BTreeSet<String>,
    /// Screen-space rectangle assigned by `wall_core::dock` when docked `Left` or `Right`.
    /// When `None`, derived from the egui viewport and `DOCK_WIDTHS.settings` (`380`).
    pub dock_rect: Option<egui::Rect>,
    /// Host platform (`Mac` vs `Linux`/`Other`) for close-chord tooltips and `⌘,` / `Ctrl+Alt+,`.
    pub platform: Platform,
    /// Optional caller-supplied per-knob refusal messages (merged over [`SettingsState::refused`]).
    pub refused: BTreeMap<String, String>,
}

impl Default for SettingsInput {
    fn default() -> Self {
        Self {
            placement: SettingsPlacement::default(),
            config: WallConfig::default(),
            dialled: BTreeSet::new(),
            dock_rect: None,
            platform: Platform::host(),
            refused: BTreeMap::new(),
        }
    }
}

impl SettingsInput {
    /// Build a [`SettingsInput`] from placement and config snapshots.
    pub fn new(placement: SettingsPlacement, config: WallConfig) -> Self {
        Self {
            placement,
            config,
            ..Self::default()
        }
    }
}

/// Alias matching `SettingsPanel` naming if imported alongside `SettingsInput`.
pub type SettingsPanelInput = SettingsInput;

/// Persistent UI view state kept by the caller across frames (`<Panel>State` in `P-panels.md`).
#[derive(Clone, Debug, Default)]
pub struct SettingsState {
    /// Current search query (`SettingsPanel.jsx`'s `query`).
    pub query: String,
    /// Index of the highlighted setting among the ones shown (`SettingsPanel.jsx`'s `active`).
    pub active: usize,
    /// While searching: the one section the results are narrowed to, or
    /// `None` for every section's. A new query un-narrows it.
    pub scope: Option<String>,
    /// A section a click chose that the wall has not answered yet: `(the
    /// store's section when it was chosen, the one chosen)`. The window
    /// shows the choice at once and drops it when the store's moves.
    chosen: Option<(Option<String>, String)>,
    /// The section whose settings were shown on the last pass (`None`: a
    /// search over all of them).
    pub shown_section: Option<String>,
    /// The section list's lines (or chips) as drawn, by section id, and
    /// [`ALL_RESULTS`] for a search's first line.
    pub section_rects: BTreeMap<String, egui::Rect>,
    /// Whether the settings panel wants keyboard input this frame (`true` whenever open).
    pub wants_keyboard: bool,
    /// Floating header drag state (`useHeadDrag` in `Panel.jsx`).
    pub head_drag: HeadDragState,
    /// The edge grips, and a resize drag in progress.
    pub resize: ResizeState,
    /// Per-knob refusal reason from the most recent rejected edit (`SettingsPanel.jsx`'s `refused`).
    pub refused: BTreeMap<String, String>,
    /// Live text drafts for `Text`, `Number`, and `Color` hex fields while focused.
    drafts: BTreeMap<String, String>,
    /// Previous frame's `open` flag so opening puts keyboard focus in the search field.
    was_open: bool,
    /// Requests keyboard focus on the search field on the next frame (`Field`'s `back()`).
    focus_search: bool,
    /// Knob key whose value `Field` should take focus and select-all on this frame (`Enter` in search).
    focus_field: Option<String>,
    /// Last active key scrolled into view via keyboard navigation.
    last_scrolled_key: Option<String>,
    /// True after the user clicks on the wall canvas outside Settings, allowing terminal typing
    /// while Settings stays docked or floating open (`SettingsPanel.jsx` input blur).
    blurred: bool,
    /// Last rendered panel bounding box when open.
    pub last_rect: Option<egui::Rect>,
    /// Floating header drag rectangle when open.
    pub header_drag_rect: Option<egui::Rect>,
    /// Number of settings shown on the last open pass.
    pub visible_count: usize,
    /// Rendered control rectangles keyed by knob identifier (`gridShow`, `gridMinor`, etc.).
    pub control_rects: BTreeMap<String, egui::Rect>,
}

/// Alias matching `SettingsPanel` naming.
pub type SettingsPanelViewState = SettingsState;

impl SettingsState {
    /// Whether the panel wants keyboard events routed to egui instead of the focused terminal.
    pub const fn wants_keyboard(&self) -> bool {
        self.wants_keyboard
    }

    /// The section to show when browsing: the one a click chose until the
    /// wall has stored it, else the stored one.
    fn section_in_force(&mut self, stored: Option<&str>) -> Option<String> {
        if let Some((seen, mine)) = &self.chosen {
            if seen.as_deref() == stored {
                return Some(mine.clone());
            }
            self.chosen = None;
        }
        stored.map(str::to_string)
    }

    /// Show section `id` from now on: at once here, and for the wall to
    /// keep (`wall.settings section <id>`).
    fn choose_section(&mut self, stored: Option<&str>, id: &str, out: &mut Vec<String>) {
        if self.section_in_force(stored).as_deref() == Some(id) {
            return;
        }
        self.chosen = Some((stored.map(str::to_string), id.to_string()));
        out.push(format!("wall.settings section {id}"));
    }

    /// Surrender keyboard capture so the focused terminal receives keys while Settings remains open.
    pub fn blur(&mut self) {
        self.blurred = true;
        self.wants_keyboard = false;
        self.focus_search = false;
        self.focus_field = None;
    }

    /// Record the router's reply to `wall.config <key> …` (`SettingsPanel.jsx` lines 138–140):
    /// stores the stripped refusal text on `ERR: …`, or clears any prior refusal for `key` on `OK`.
    pub fn record_reply(&mut self, key: &str, reply_text: &str) {
        if let Some(rest) = reply_text.strip_prefix("ERR:") {
            self.refused.insert(key.to_string(), rest.trim_start().to_string());
        } else if reply_text.starts_with("ERR") {
            self.refused.insert(key.to_string(), reply_text.to_string());
        } else {
            self.refused.remove(key);
        }
    }
}

fn search_input_id() -> egui::Id {
    egui::Id::new("wall_settings_search_input")
}

fn knob_field_id(key: &str) -> egui::Id {
    egui::Id::new(("wall_settings_field", key))
}

/// Format `<value>` for `wall.config <key> <value>` so `CommandRouter::parse_line` round-trips
/// empty strings (`""`), quotes, backslashes, and leading/trailing spaces without turning an empty
/// edit into a bare `wall.config <key>` read.
fn format_config_arg(raw: &str) -> String {
    let needs_quotes = raw.is_empty()
        || raw.trim() != raw
        || raw.contains(['"', '\'', '\\']);
    if !needs_quotes {
        return raw.to_string();
    }
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('"');
    for ch in raw.chars() {
        if matches!(ch, '"' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}

fn emit_config_edit(
    key: &str,
    raw_value: &str,
    state: &mut SettingsState,
    out: &mut Vec<String>,
) {
    if raw_value == "default" {
        state.refused.remove(key);
        out.push(format!("wall.config {key} default"));
        return;
    }
    match check_config(key, raw_value) {
        Ok(_) => {
            state.refused.remove(key);
        }
        Err(reason) => {
            state.refused.insert(key.to_string(), reason);
        }
    }
    out.push(format!("wall.config {key} {}", format_config_arg(raw_value)));
}

fn is_close_settings_chord(i: &egui::InputState, platform: Platform) -> bool {
    i.events.iter().any(|ev| match ev {
        egui::Event::Key {
            key: egui::Key::Comma,
            pressed: true,
            modifiers,
            ..
        } => match platform {
            Platform::Mac => modifiers.mac_cmd || modifiers.command,
            Platform::Other => modifiers.ctrl && modifiers.alt && !modifiers.mac_cmd,
        },
        _ => false,
    })
}

fn parse_hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let body = hex.trim().strip_prefix('#')?;
    if body.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&body[0..2], 16).ok()?;
    let g = u8::from_str_radix(&body[2..4], 16).ok()?;
    let b = u8::from_str_radix(&body[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Below this width the window has no room for a section list beside the
/// settings: the sections are a row of chips above them instead (a docked
/// settings column, until its edge is dragged wider).
const TWO_PANE_MIN_W: f32 = 560.0;
/// The section list's width in the two-pane layout.
const SECTION_LIST_W: f32 = 178.0;
/// One line of the section list.
const SECTION_ROW_H: f32 = 28.0;
/// The key of the list's `all results` line in [`SettingsState::section_rects`].
pub const ALL_RESULTS: &str = "all";

/// What a click in the section list or on a results heading chose.
enum SectionPick {
    /// One section.
    Section(&'static str),
    /// Every section's matches (a search's `all results` line).
    All,
}

/// Draw the settings window: the search, the section list, and the settings
/// of one section or of the search (`wall_core::settings::settings_view`).
///
/// Returns `true` when the window wants keyboard input this frame (also stored on
/// [`SettingsState::wants_keyboard`]).
pub fn show(
    ctx: &egui::Context,
    state: &mut SettingsState,
    input: &SettingsInput,
    out: &mut Vec<String>,
) -> bool {
    state.control_rects.clear();
    state.section_rects.clear();
    if !input.placement.open {
        state.was_open = false;
        state.wants_keyboard = false;
        state.blurred = false;
        state.head_drag.drag_at = None;
        state.focus_field = None;
        state.focus_search = false;
        state.last_rect = None;
        state.header_drag_rect = None;
        state.visible_count = 0;
        state.shown_section = None;
        state.scope = None;
        state.chosen = None;
        state.resize.reset();
        return false;
    }

    if !state.was_open {
        state.was_open = true;
        state.focus_search = true;
        state.blurred = false;
    }
    if state.focus_search || state.focus_field.is_some() {
        state.blurred = false;
    }
    state.wants_keyboard = !state.blurred;

    // Close chord (`⌘,` on macOS, `Ctrl+Alt+,` on Linux/Windows) closes from anywhere in the window.
    if ctx.input(|i| is_close_settings_chord(i, input.platform)) {
        ctx.input_mut(|i| {
            i.events.retain(|ev| {
                !matches!(
                    ev,
                    egui::Event::Key {
                        key: egui::Key::Comma,
                        pressed: true,
                        ..
                    }
                )
            });
        });
        out.push("wall.settings close".to_string());
        return true;
    }

    let viewport = panel_viewport_rect(ctx);
    let side = input.placement.side;
    let limits = PanelKind::Settings.limits();
    let (mut origin, width, mut max_height) = resolve_panel_box(
        viewport,
        side,
        input.dock_rect,
        input.placement.float,
        &state.head_drag,
        match side {
            PanelSide::Float => resize::float_width(
                &state.resize,
                &input.placement.size,
                limits.float_default_width as f32,
                viewport,
            ),
            PanelSide::Left | PanelSide::Right => limits.dock_default as f32,
        },
    );
    // The height the window is held to: the viewport's when docked; when
    // floating, the one set on it, else its own (a window that kept to its
    // contents would jump as the sections changed).
    let held_height = match side {
        PanelSide::Float => resize::float_height(&state.resize, &input.placement.size, viewport)
            .or(limits.float_default_height.map(|h| h as f32)),
        PanelSide::Left | PanelSide::Right => Some(max_height),
    };
    if side == PanelSide::Float {
        if let Some(h) = held_height {
            max_height = h.min((viewport.height() - 32.0).max(96.0));
        }
        // Until it has been put somewhere it opens in the middle, under the topbar.
        if input.placement.float.is_none() && state.head_drag.drag_at.is_none() {
            origin = egui::pos2(
                (viewport.center().x - width * 0.5).max(viewport.left() + 8.0),
                (viewport.top() + (viewport.height() - max_height) * 0.4)
                    .max(viewport.top() + 46.0),
            );
        }
        if let Some(at) = resize::float_origin(&state.resize) {
            origin = at;
        }
        // Its bottom edge stays in the window, wherever it was put.
        max_height = max_height.min(resize::room_below(origin.y, viewport));
    }
    // A floating panel has a 1 px border, drawn outside what it holds: the
    // size set on it is the box with its border, so what it holds is that
    // much smaller (a drag would otherwise grow it 2 px on every release).
    let rim = if side == PanelSide::Float { resize::FLOAT_RIM_PX } else { 0.0 };
    let width = width - 2.0 * rim;
    let max_height = max_height - 2.0 * rim;

    let panel_frame = match side {
        PanelSide::Float => egui::Frame::NONE
            .fill(settings_panel_bg())
            .corner_radius(egui::CornerRadius::same(7))
            .stroke(egui::Stroke::new(1.0, BORDER))
            .shadow(egui::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: egui::Color32::from_black_alpha(115),
            }),
        PanelSide::Left | PanelSide::Right => egui::Frame::NONE.fill(settings_panel_bg()),
    };
    let close_title = format!("close ({})", chord_label(input.platform, &[","], false));

    egui::Area::new(egui::Id::new("wall_settings_panel"))
        .fixed_pos(origin)
        .order(egui::Order::Foreground)
        .constrain(false)
        // No fade: the wall draws on demand, and a window caught part-way
        // in is a faint one until something else redraws it.
        .fade_in(false)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_settings initial sizing");
            }
            let frame_resp = panel_frame.show(ui, |ui| {
                ui.set_width(width);
                if held_height.is_some() {
                    ui.set_min_height(max_height);
                }
                ui.set_max_height(max_height);
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                let panel_top = ui.cursor().top();

                draw_header(ui, state, side, origin, width, &close_title, out);

                // ── Search, above everything, and the keys it leaves the list ──
                let search_id = search_input_id();
                if state.focus_search {
                    ui.memory_mut(|m| m.request_focus(search_id));
                    state.focus_search = false;
                }
                let any_field_focused = state.focus_field.is_some()
                    || ui.memory(|m| {
                        wall_core::settings::KNOBS
                            .iter()
                            .any(|(k, _)| m.has_focus(knob_field_id(k)))
                    });

                let mut pressed_down = false;
                let mut pressed_up = false;
                let mut pressed_enter = false;
                let mut pressed_escape = false;
                let mut section_step = 0;
                if ui.memory(|m| m.has_focus(search_id)) || any_field_focused {
                    state.blurred = false;
                    state.wants_keyboard = true;
                }
                if !any_field_focused && !state.blurred {
                    // Keep search focused while active when no knob field is being edited.
                    if !ui.memory(|m| m.has_focus(search_id)) {
                        ui.memory_mut(|m| m.request_focus(search_id));
                    }
                    ui.input_mut(|i| {
                        pressed_down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                        pressed_up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                        pressed_enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                        pressed_escape = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
                            section_step = 1;
                        }
                        if i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
                            section_step = -1;
                        }
                    });
                }

                let prev_query = state.query.clone();
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 10,
                        bottom: 10,
                    })
                    .show(ui, |ui| {
                        let field_frame = egui::Frame::NONE
                            .fill(FIELD_BG)
                            .stroke(egui::Stroke::new(1.0, BORDER))
                            .corner_radius(egui::CornerRadius::same(5))
                            .inner_margin(egui::Margin::symmetric(9, 7));
                        field_frame.show(ui, |ui| {
                            ui.set_width((width - 44.0).max(40.0));
                            ui.add(
                                egui::TextEdit::singleline(&mut state.query)
                                    .id(search_id)
                                    .frame(egui::Frame::NONE)
                                    .font(egui::FontId::monospace(13.0))
                                    .text_color(BRIGHT)
                                    .hint_text(
                                        egui::RichText::new("search settings…")
                                            .font(egui::FontId::monospace(13.0))
                                            .color(MUTED),
                                    )
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    });
                let search_bottom = ui.cursor().top();
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    search_bottom,
                    egui::Stroke::new(1.0, HEAD_BORDER),
                );

                // Escape clears a search first, and closes the window only
                // when there is none.
                if pressed_escape {
                    if state.query.is_empty() {
                        out.push("wall.settings close".to_string());
                    } else {
                        state.query.clear();
                        ui.memory_mut(|m| m.request_focus(search_id));
                    }
                }
                let was_searching = !prev_query.trim().is_empty();
                if state.query != prev_query {
                    state.active = 0;
                    // A search that ends narrowed to one section leaves the
                    // window in that section.
                    if state.query.trim().is_empty() && was_searching {
                        if let Some(id) = state.scope.take() {
                            state.choose_section(input.placement.section.as_deref(), &id, out);
                        }
                    }
                    state.scope = None;
                }

                // ── What the window shows ────────────────────────────────────
                let searching = !state.query.trim().is_empty();
                let mut view = current_view(state, input, searching);
                if section_step != 0 {
                    if let Some(id) = step_section(&view, section_step) {
                        if view.shown != Some(id) {
                            pick_section(state, input, searching, SectionPick::Section(id), out);
                            view = current_view(state, input, searching);
                        }
                    }
                }
                state.visible_count = view.rows.len();
                state.shown_section = view.shown.map(str::to_string);

                let mut at = state.active.min(view.rows.len().saturating_sub(1));
                let mut moved_by_keys = false;
                if pressed_down && !view.rows.is_empty() {
                    at = (at + 1).min(view.rows.len() - 1);
                    moved_by_keys = true;
                } else if pressed_up {
                    at = at.saturating_sub(1);
                    moved_by_keys = true;
                }
                state.active = at;
                if pressed_enter {
                    if let Some(row) = view.rows.get(at) {
                        activate_knob(row.key, &row.def, &input.config, state, out);
                    }
                }

                // ── Sections beside (or above) the settings ──────────────────
                let body_h = (panel_top + max_height - ui.cursor().top()).max(60.0);
                let (body_rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, body_h), egui::Sense::hover());
                let mut picked: Option<SectionPick> = None;
                let detail_rect = if width >= TWO_PANE_MIN_W {
                    let list_rect = egui::Rect::from_min_size(
                        body_rect.min,
                        egui::vec2(SECTION_LIST_W, body_rect.height()),
                    );
                    ui.painter().vline(
                        list_rect.right(),
                        body_rect.y_range(),
                        egui::Stroke::new(1.0, HEAD_BORDER),
                    );
                    let mut list_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(list_rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    list_ui.set_clip_rect(list_rect.intersect(ui.clip_rect()));
                    draw_section_list(&mut list_ui, &view, state, &mut picked);
                    egui::Rect::from_min_max(
                        egui::pos2(list_rect.right() + 1.0, body_rect.top()),
                        body_rect.max,
                    )
                } else {
                    let mut chips_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(body_rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    let used = draw_section_chips(&mut chips_ui, &view, state, &mut picked);
                    egui::Rect::from_min_max(
                        egui::pos2(body_rect.left(), (body_rect.top() + used).min(body_rect.bottom())),
                        body_rect.max,
                    )
                };

                let mut detail_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(detail_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                detail_ui.set_clip_rect(detail_rect.intersect(ui.clip_rect()));
                draw_settings(
                    &mut detail_ui,
                    &view,
                    state,
                    input,
                    moved_by_keys,
                    &mut picked,
                    out,
                );

                if let Some(pick) = picked {
                    pick_section(state, input, searching, pick, out);
                }
            });

            // Docked inner border (`borderRight` on left dock, `borderLeft` on right dock).
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

            // The edges that resize it.
            resize::grips(
                ui,
                &mut state.resize,
                &ResizeSpec {
                    verb: "wall.settings",
                    side,
                    rect: r,
                    limits,
                    viewport,
                    centered: false,
                },
                out,
            );
        });

    true
}

/// The view for the query and the section in force: the search's own
/// narrowing while searching, else the section chosen.
fn current_view(
    state: &mut SettingsState,
    input: &SettingsInput,
    searching: bool,
) -> SettingsView<'static> {
    let section = if searching {
        state.scope.clone()
    } else {
        state.section_in_force(input.placement.section.as_deref())
    };
    settings_view(&state.query, section.as_deref(), NATIVE_HIDDEN_KNOBS)
}

/// Act on a choice of section: while searching it narrows the results (the
/// window's own business); browsing, it is the section the window shows,
/// which is the wall's to keep (`wall.settings section <id>`).
fn pick_section(
    state: &mut SettingsState,
    input: &SettingsInput,
    searching: bool,
    pick: SectionPick,
    out: &mut Vec<String>,
) {
    state.active = 0;
    match pick {
        SectionPick::All => state.scope = None,
        SectionPick::Section(id) if searching => state.scope = Some(id.to_string()),
        SectionPick::Section(id) => {
            state.choose_section(input.placement.section.as_deref(), id, out);
        }
    }
}

/// The header: the title (the handle a floating window is moved by) and the
/// dock and close buttons.
fn draw_header(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    side: PanelSide,
    origin: egui::Pos2,
    width: f32,
    close_title: &str,
    out: &mut Vec<String>,
) {
    let head_rect = egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 12,
            right: 6,
            top: 6,
            bottom: 5,
        })
        .show(ui, |ui| {
            let header_w = (width - 18.0).max(40.0);
            ui.allocate_ui_with_layout(
                egui::vec2(header_w, 18.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                    dock_buttons_rtl(ui, "wall.settings", side, close_title, out);

                    let avail = ui.available_width().max(24.0);
                    let (title_rect, title_resp) = ui.allocate_exact_size(
                        egui::vec2(avail, 18.0),
                        if side == PanelSide::Float {
                            egui::Sense::drag()
                        } else {
                            egui::Sense::hover()
                        },
                    );
                    let galley = ui.painter().layout_no_wrap(
                        "settings".to_string(),
                        egui::FontId::monospace(12.0),
                        MUTED,
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
                        "wall.settings",
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
}

/// One line of the section list (or one chip, in the narrow layout).
struct SectionLine {
    /// Its key in [`SettingsState::section_rects`]: the section's id, or
    /// [`ALL_RESULTS`].
    key: &'static str,
    title: String,
    /// The matches in it, while searching.
    count: Option<usize>,
    selected: bool,
    /// What a click on it chooses; `None` for a section a search found
    /// nothing in.
    pick: Option<SectionPick>,
}

/// The lines of the section list: a search's `all results` first, then
/// every section.
fn section_lines(view: &SettingsView<'_>) -> Vec<SectionLine> {
    let mut lines = Vec::with_capacity(view.sections.len() + 1);
    if view.searching {
        lines.push(SectionLine {
            key: ALL_RESULTS,
            title: "All results".to_string(),
            count: Some(view.matched),
            selected: view.shown.is_none(),
            pick: Some(SectionPick::All),
        });
    }
    for entry in &view.sections {
        let id = entry.section.id;
        lines.push(SectionLine {
            key: id,
            title: entry.section.title.to_string(),
            count: view.searching.then_some(entry.count),
            selected: entry.selected,
            pick: (!view.searching || entry.count > 0).then_some(SectionPick::Section(id)),
        });
    }
    lines
}

/// The section list down the window's left: one line a section, the one
/// shown lit. While searching each has the number of matches in it, a
/// section with none is dimmed, and `all results` heads the list.
fn draw_section_list(
    ui: &mut egui::Ui,
    view: &SettingsView<'_>,
    state: &mut SettingsState,
    picked: &mut Option<SectionPick>,
) {
    let list_w = ui.max_rect().width();
    egui::ScrollArea::vertical()
        .id_salt("wall_settings_sections")
        .max_height(ui.max_rect().height())
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(list_w);
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.add_space(6.0);
            for SectionLine { key, title, count, selected, pick } in section_lines(view) {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(list_w, SECTION_ROW_H), egui::Sense::hover());
                state.section_rects.insert(key.to_string(), rect);
                let live = pick.is_some();
                let resp = ui.interact(
                    rect,
                    ui.make_persistent_id(("wall_settings_section", key)),
                    if live { egui::Sense::click() } else { egui::Sense::hover() },
                );
                if ui.is_rect_visible(rect) {
                    if selected {
                        ui.painter()
                            .rect_filled(rect, egui::CornerRadius::ZERO, ACTIVE_ROW_BG);
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
                            egui::CornerRadius::ZERO,
                            OK_GREEN,
                        );
                    } else if live && resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, egui::CornerRadius::ZERO, HOVER_ROW_BG);
                    }
                    let fg = if selected {
                        BRIGHT
                    } else if live {
                        LABEL
                    } else {
                        KEY_TEXT
                    };
                    let count_w = count.map_or(0.0, |n| {
                        let galley = ui.painter().layout_no_wrap(
                            n.to_string(),
                            egui::FontId::monospace(11.0),
                            if live { MUTED } else { KEY_TEXT },
                        );
                        let w = galley.size().x;
                        ui.painter().galley(
                            egui::pos2(
                                rect.right() - 12.0 - w,
                                rect.center().y - galley.size().y * 0.5,
                            ),
                            galley,
                            MUTED,
                        );
                        w + 8.0
                    });
                    let galley = crate::wall::panels::layout_elided(
                        ui.painter(),
                        &title,
                        egui::FontId::monospace(12.5),
                        fg,
                        (list_w - 14.0 - 12.0 - count_w).max(20.0),
                    );
                    ui.painter().galley(
                        egui::pos2(rect.left() + 14.0, rect.center().y - galley.size().y * 0.5),
                        galley,
                        fg,
                    );
                }
                if live && resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    *picked = pick;
                }
            }
            ui.add_space(6.0);
        });
}

/// The narrow layout's section list: a wrapping row of chips above the
/// settings. Returns the height it took.
fn draw_section_chips(
    ui: &mut egui::Ui,
    view: &SettingsView<'_>,
    state: &mut SettingsState,
    picked: &mut Option<SectionPick>,
) -> f32 {
    let top = ui.cursor().top();
    let full = ui.max_rect();
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width((full.width() - 20.0).max(40.0));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                for SectionLine { key, title, count, selected, pick } in section_lines(view) {
                    let live = pick.is_some();
                    let text = match count {
                        Some(n) => format!("{title} {n}"),
                        None => title,
                    };
                    let fg = if selected {
                        BRIGHT
                    } else if live {
                        DESC_TEXT
                    } else {
                        KEY_TEXT
                    };
                    let galley =
                        ui.painter()
                            .layout_no_wrap(text, egui::FontId::monospace(11.5), fg);
                    let (_id, rect) =
                        ui.allocate_space(egui::vec2(galley.size().x + 14.0, 20.0));
                    state.section_rects.insert(key.to_string(), rect);
                    let resp = ui.interact(
                        rect,
                        ui.make_persistent_id(("wall_settings_section", key)),
                        if live { egui::Sense::click() } else { egui::Sense::hover() },
                    );
                    if ui.is_rect_visible(rect) {
                        if selected {
                            ui.painter()
                                .rect_filled(rect, egui::CornerRadius::same(4), BTN_ON_BG);
                        } else if live && resp.hovered() {
                            ui.painter()
                                .rect_filled(rect, egui::CornerRadius::same(4), HOVER_ROW_BG);
                        } else {
                            ui.painter().rect_stroke(
                                rect,
                                egui::CornerRadius::same(4),
                                egui::Stroke::new(1.0, HEAD_BORDER),
                                egui::StrokeKind::Inside,
                            );
                        }
                        ui.painter().galley(
                            egui::pos2(
                                rect.center().x - galley.size().x * 0.5,
                                rect.center().y - galley.size().y * 0.5,
                            ),
                            galley,
                            fg,
                        );
                    }
                    if live && resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        *picked = pick;
                    }
                }
            });
        });
    let bottom = ui.cursor().top();
    ui.painter().hline(
        full.x_range(),
        bottom,
        egui::Stroke::new(1.0, HEAD_BORDER),
    );
    bottom - top + 1.0
}

/// The right-hand side: what is shown (a section's name, or what a search
/// found), then its settings, each a label, its one-line meaning and its
/// control.
fn draw_settings(
    ui: &mut egui::Ui,
    view: &SettingsView<'_>,
    state: &mut SettingsState,
    input: &SettingsInput,
    moved_by_keys: bool,
    picked: &mut Option<SectionPick>,
    out: &mut Vec<String>,
) {
    let pane = ui.max_rect();
    let pane_w = pane.width();
    // Each section keeps its own place in its list.
    let scroll_salt = ("wall_settings_body", view.shown.unwrap_or("search"));
    egui::ScrollArea::vertical()
        .id_salt(scroll_salt)
        .max_height(pane.height())
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(pane_w);
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

            // ── What this is a list of ───────────────────────────────────────
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 18,
                    right: 16,
                    top: 14,
                    bottom: 10,
                })
                .show(ui, |ui| {
                    ui.set_width((pane_w - 34.0).max(40.0));
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    let shown = view
                        .shown
                        .and_then(|id| view.sections.iter().find(|s| s.section.id == id));
                    match (view.searching, shown) {
                        (false, Some(entry)) => {
                            ui.label(
                                egui::RichText::new(entry.section.title)
                                    .font(egui::FontId::monospace(15.0))
                                    .color(BRIGHT),
                            );
                            ui.add_space(3.0);
                            ui.label(
                                egui::RichText::new(entry.section.subtitle)
                                    .font(egui::FontId::monospace(11.5))
                                    .color(MUTED),
                            );
                        }
                        (true, _) if view.matched == 0 => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "no setting matches “{}”",
                                    state.query
                                ))
                                .font(egui::FontId::monospace(12.5))
                                .color(MUTED),
                            );
                        }
                        (true, Some(entry)) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} of {} matches for “{}”, in {}",
                                    view.rows.len(),
                                    view.matched,
                                    state.query,
                                    entry.section.title
                                ))
                                .font(egui::FontId::monospace(12.5))
                                .color(MUTED),
                            );
                        }
                        (true, None) => {
                            let noun = if view.matched == 1 { "setting matches" } else { "settings match" };
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {noun} “{}”",
                                    view.matched, state.query
                                ))
                                .font(egui::FontId::monospace(12.5))
                                .color(MUTED),
                            );
                        }
                        (false, None) => {}
                    }
                });

            for (my_idx, row) in view.rows.iter().enumerate() {
                // A search over every section says which section each run
                // of matches is in; a click on the name narrows to it.
                if view.searching && view.shown.is_none() && row.starts_section {
                    draw_results_heading(ui, pane_w, row, view, picked);
                } else {
                    let (line, _) =
                        ui.allocate_exact_size(egui::vec2(pane_w, 1.0), egui::Sense::hover());
                    ui.painter().hline(
                        (line.left() + 18.0)..=(line.right() - 16.0),
                        line.top(),
                        egui::Stroke::new(1.0, HEAD_BORDER),
                    );
                }
                let is_active = state.active == my_idx;
                let item_rect = draw_setting_row(ui, pane_w, row, is_active, state, input, out);

                if ui.rect_contains_pointer(item_rect)
                    && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO)
                {
                    state.active = my_idx;
                }
                if is_active
                    && (moved_by_keys || state.last_scrolled_key.as_deref() != Some(row.key))
                {
                    if moved_by_keys {
                        ui.scroll_to_rect(item_rect, None);
                    }
                    state.last_scrolled_key = Some(row.key.to_string());
                }
            }
            ui.add_space(10.0);
        });
}

/// The heading over one section's run of matches in a search's results.
fn draw_results_heading(
    ui: &mut egui::Ui,
    pane_w: f32,
    row: &SettingRow<'_>,
    view: &SettingsView<'_>,
    picked: &mut Option<SectionPick>,
) {
    let count = view
        .sections
        .iter()
        .find(|s| s.section.id == row.section.id)
        .map_or(0, |s| s.count);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(pane_w, 30.0), egui::Sense::hover());
    let resp = ui.interact(
        rect,
        ui.make_persistent_id(("wall_settings_results_heading", row.section.id)),
        egui::Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        ui.painter().hline(
            (rect.left() + 18.0)..=(rect.right() - 16.0),
            rect.top(),
            egui::Stroke::new(1.0, HEAD_BORDER),
        );
        let fg = if resp.hovered() { BRIGHT } else { LABEL };
        let title = ui.painter().layout_no_wrap(
            row.section.title.to_string(),
            egui::FontId::monospace(12.0),
            fg,
        );
        let title_w = title.size().x;
        let y = rect.bottom() - 6.0 - title.size().y;
        ui.painter().galley(egui::pos2(rect.left() + 18.0, y), title, fg);
        ui.painter().text(
            egui::pos2(rect.left() + 18.0 + title_w + 8.0, y + 1.0),
            egui::Align2::LEFT_TOP,
            count.to_string(),
            egui::FontId::monospace(11.0),
            MUTED,
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp
        .on_hover_text(format!("show only the matches in {}", row.section.title))
        .clicked()
    {
        *picked = Some(SectionPick::Section(row.section.id));
    }
}

/// One setting: its label and control on a line, its one-line meaning under
/// them, and (only when they are why a search found it) its long
/// description and key. Hovering the label shows all of it.
fn draw_setting_row(
    ui: &mut egui::Ui,
    pane_w: f32,
    row: &SettingRow<'_>,
    is_active: bool,
    state: &mut SettingsState,
    input: &SettingsInput,
    out: &mut Vec<String>,
) -> egui::Rect {
    let key = row.key;
    let is_dialled = input.dialled.contains(key);
    let current_val = input
        .config
        .get(key)
        .or_else(|| default_knob_value(key))
        .expect("every KNOBS entry has a default value");
    let refusal = input
        .refused
        .get(key)
        .or_else(|| state.refused.get(key))
        .cloned();
    let details = format!("{}\n\nwall.config {key}", row.def.description);

    egui::Frame::NONE
        .fill(if is_active {
            ACTIVE_ROW_BG
        } else {
            egui::Color32::TRANSPARENT
        })
        .inner_margin(egui::Margin {
            left: 18,
            right: 16,
            top: 9,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width((pane_w - 34.0).max(40.0));
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);

            // Label on the left, optional `↺` + control on the right.
            ui.horizontal(|ui| {
                ui.set_min_height(22.0);
                ui.label(marked_layout_job(
                    row.def.label,
                    &row.hits.label,
                    egui::FontId::monospace(13.0),
                    BRIGHT,
                ))
                .on_hover_text(&details);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(5.0, 0.0);
                    render_knob_control(ui, key, &row.def, &current_val, state, out);
                    if is_dialled
                        && icon_btn(
                            ui,
                            ("wall_settings_reset", key),
                            ChromeIcon::Reset,
                            false,
                            "back to the default",
                        )
                        .clicked()
                    {
                        state.drafts.remove(key);
                        emit_config_edit(key, "default", state, out);
                    }
                });
            });

            // What it does, in one line.
            ui.label(marked_layout_job(
                row.def.summary,
                &row.hits.summary,
                egui::FontId::monospace(11.5),
                DESC_TEXT,
            ))
            .on_hover_text(&details);

            // The sizes on offer, a click each; the field takes any other.
            if key == "newTerminalSize" {
                render_size_presets(ui, &input.config, &current_val, state, out);
            }

            // Why a search found it, when that is not in the two lines above.
            if !row.hits.description.is_empty() && row.hits.only_in_the_details() {
                ui.label(marked_layout_job(
                    row.def.description,
                    &row.hits.description,
                    egui::FontId::monospace(11.0),
                    MUTED,
                ));
            }
            if !row.hits.key.is_empty() {
                ui.label(marked_layout_job(
                    key,
                    &row.hits.key,
                    egui::FontId::monospace(10.5),
                    MUTED,
                ));
            }

            // Validation refusal, if any.
            if let Some(err_msg) = refusal {
                ui.label(
                    egui::RichText::new(err_msg)
                        .font(egui::FontId::monospace(11.0))
                        .color(ERR_RED),
                );
            }
        })
        .response
        .rect
}

fn activate_knob(
    key: &str,
    def: &KnobDef,
    config: &WallConfig,
    state: &mut SettingsState,
    out: &mut Vec<String>,
) {
    let val = config
        .get(key)
        .or_else(|| default_knob_value(key))
        .expect("every KNOBS entry has a default value");
    match def.kind {
        KnobKind::Bool => {
            let next = !val.as_bool().unwrap_or(false);
            emit_config_edit(key, if next { "true" } else { "false" }, state, out);
        }
        KnobKind::Enum { options } => {
            if options.is_empty() {
                return;
            }
            let cur = val.as_str().unwrap_or("");
            let next_idx = options
                .iter()
                .position(|&o| o == cur)
                .map_or(0, |idx| (idx + 1) % options.len());
            emit_config_edit(key, options[next_idx], state, out);
        }
        KnobKind::Color | KnobKind::Text { .. } | KnobKind::Number { .. } => {
            state.focus_field = Some(key.to_string());
        }
    }
}

/// Render the right-hand control for one knob (`Control` in `SettingsPanel.jsx`).
///
/// Note: called inside a `right_to_left` layout so the control sits flush right and any
/// unit suffix or colour swatch lines up cleanly.
fn render_knob_control(
    ui: &mut egui::Ui,
    key: &str,
    def: &KnobDef,
    value: &KnobValue,
    state: &mut SettingsState,
    out: &mut Vec<String>,
) {
    match def.kind {
        KnobKind::Bool => {
            let on = value.as_bool().unwrap_or(false);
            let (_id, rect) = ui.allocate_space(egui::vec2(30.0, 16.0));
            state.control_rects.insert(key.to_string(), rect);
            let resp = ui
                .interact(
                    rect,
                    ui.make_persistent_id(("wall_settings_bool", key)),
                    egui::Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if ui.is_rect_visible(rect) {
                let (bg, border, dot_col, dot_x) = if on {
                    (
                        egui::Color32::from_rgb(0x1d, 0x3a, 0x2b),
                        egui::Color32::from_rgb(0x2f, 0x6b, 0x4b),
                        OK_GREEN,
                        rect.left() + 21.0,
                    )
                } else {
                    (
                        egui::Color32::from_rgb(0x1a, 0x1f, 0x2a),
                        BORDER,
                        MUTED,
                        rect.left() + 8.0,
                    )
                };
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(8), bg);
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::same(8),
                    egui::Stroke::new(1.0, border),
                    egui::StrokeKind::Inside,
                );
                ui.painter()
                    .circle_filled(egui::pos2(dot_x, rect.center().y), 5.0, dot_col);
            }
            if resp.clicked() {
                emit_config_edit(key, if !on { "true" } else { "false" }, state, out);
            }
        }
        KnobKind::Enum { options } => {
            let cur = value.as_str().unwrap_or("");
            // Reverse iterate because parent layout is right-to-left, keeping options in declaration order left-to-right.
            for &opt in options.iter().rev() {
                let is_on = opt == cur;
                let galley = ui.painter().layout_no_wrap(
                    opt.to_string(),
                    egui::FontId::monospace(11.5),
                    if is_on { BRIGHT } else { DESC_TEXT },
                );
                let btn_w = (galley.size().x + 12.0).max(24.0);
                let (_id, rect) = ui.allocate_space(egui::vec2(btn_w, 18.0));
                let resp = ui
                    .interact(
                        rect,
                        ui.make_persistent_id(("wall_settings_enum", key, opt)),
                        egui::Sense::click(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if ui.is_rect_visible(rect) {
                    if is_on {
                        ui.painter()
                            .rect_filled(rect, egui::CornerRadius::same(4), BTN_ON_BG);
                    } else if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, egui::CornerRadius::same(4), HOVER_ROW_BG);
                    }
                    ui.painter().galley(
                        egui::pos2(
                            rect.center().x - galley.size().x * 0.5,
                            rect.center().y - galley.size().y * 0.5,
                        ),
                        galley,
                        if is_on || resp.hovered() { BRIGHT } else { DESC_TEXT },
                    );
                }
                if resp.clicked() {
                    emit_config_edit(key, opt, state, out);
                }
            }
        }
        KnobKind::Color => {
            let val_str = value.to_string();
            render_commit_field(ui, key, &val_str, 70.0, None, false, state, out);
            let mut rgb = parse_hex_rgb(&val_str).unwrap_or([0x18, 0x1c, 0x26]);
            let swatch_resp = ui.color_edit_button_srgb(&mut rgb);
            if ui.is_rect_visible(swatch_resp.rect) {
                ui.painter().rect_stroke(
                    swatch_resp.rect,
                    egui::CornerRadius::same(3),
                    egui::Stroke::new(1.0, BORDER),
                    egui::StrokeKind::Outside,
                );
            }
            if swatch_resp.changed() {
                let hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
                if !hex.eq_ignore_ascii_case(&val_str) {
                    state.drafts.remove(key);
                    emit_config_edit(key, &hex, state, out);
                }
            }
        }
        KnobKind::Text { .. } => {
            let val_str = value.to_string();
            render_commit_field(ui, key, &val_str, 150.0, Some("(none)"), false, state, out);
        }
        KnobKind::Number { unit, .. } => {
            if let Some(u) = unit {
                ui.label(
                    egui::RichText::new(u)
                        .font(egui::FontId::monospace(10.5))
                        .color(MUTED),
                );
            }
            let val_str = value.to_string();
            render_commit_field(ui, key, &val_str, 64.0, None, true, state, out);
        }
    }
}

/// The `newTerminalSize` knob's presets as a row of chips
/// ([`WallConfig::new_terminal_size_presets`]): the lit one is the size in
/// force, a click on another emits `wall.config newTerminalSize <it>`. Each
/// chip's rectangle is recorded as `newTerminalSize@<size>`.
fn render_size_presets(
    ui: &mut egui::Ui,
    config: &WallConfig,
    value: &KnobValue,
    state: &mut SettingsState,
    out: &mut Vec<String>,
) {
    const KEY: &str = "newTerminalSize";
    let current = value.as_str().unwrap_or("");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
        for size in config.new_terminal_size_presets() {
            let is_on = size == current;
            let galley = ui.painter().layout_no_wrap(
                size.clone(),
                egui::FontId::monospace(11.5),
                if is_on { BRIGHT } else { DESC_TEXT },
            );
            let (_id, rect) =
                ui.allocate_space(egui::vec2((galley.size().x + 12.0).max(24.0), 18.0));
            state.control_rects.insert(format!("{KEY}@{size}"), rect);
            let resp = ui
                .interact(
                    rect,
                    ui.make_persistent_id(("wall_settings_size_preset", size.as_str())),
                    egui::Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if ui.is_rect_visible(rect) {
                if is_on {
                    ui.painter()
                        .rect_filled(rect, egui::CornerRadius::same(4), BTN_ON_BG);
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, egui::CornerRadius::same(4), HOVER_ROW_BG);
                } else {
                    ui.painter().rect_stroke(
                        rect,
                        egui::CornerRadius::same(4),
                        egui::Stroke::new(1.0, BORDER),
                        egui::StrokeKind::Inside,
                    );
                }
                ui.painter().galley(
                    egui::pos2(
                        rect.center().x - galley.size().x * 0.5,
                        rect.center().y - galley.size().y * 0.5,
                    ),
                    galley,
                    if is_on || resp.hovered() { BRIGHT } else { DESC_TEXT },
                );
            }
            if resp.clicked() && !is_on {
                state.drafts.remove(KEY);
                emit_config_edit(KEY, &size, state, out);
            }
        }
    });
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal commit-on-enter/blur text input shared across Color, Text, and Number knobs"
)]
fn render_commit_field(
    ui: &mut egui::Ui,
    key: &str,
    committed_val: &str,
    box_width: f32,
    placeholder: Option<&str>,
    align_right: bool,
    state: &mut SettingsState,
    out: &mut Vec<String>,
) {
    let field_id = knob_field_id(key);
    let select_all_now = state.focus_field.as_deref() == Some(key);
    if select_all_now {
        state.focus_field = None;
        state
            .drafts
            .insert(key.to_string(), committed_val.to_string());
        ui.memory_mut(|m| m.request_focus(field_id));
    }

    let had_focus = ui.memory(|m| m.has_focus(field_id));
    let mut enter_pressed = false;
    let mut escape_pressed = false;
    if had_focus {
        ui.input_mut(|i| {
            enter_pressed = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            escape_pressed = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
    }

    if escape_pressed {
        state.drafts.remove(key);
        state.focus_search = true;
        ui.memory_mut(|m| m.request_focus(search_input_id()));
    }

    let mut buf = state
        .drafts
        .get(key)
        .cloned()
        .unwrap_or_else(|| committed_val.to_string());

    let frame = egui::Frame::NONE
        .fill(FIELD_BG)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(5, 2));

    let inner = frame.show(ui, |ui| {
        ui.set_width((box_width - 12.0).max(24.0));
        let mut edit = egui::TextEdit::singleline(&mut buf)
            .id(field_id)
            .frame(egui::Frame::NONE)
            .font(egui::FontId::monospace(12.0))
            .text_color(BRIGHT)
            .desired_width(f32::INFINITY);
        if align_right {
            edit = edit.horizontal_align(egui::Align::Max);
        }
        if let Some(hint) = placeholder {
            edit = edit.hint_text(
                egui::RichText::new(hint)
                    .font(egui::FontId::monospace(12.0))
                    .color(MUTED),
            );
        }
        edit.show(ui)
    });
    state.control_rects.insert(key.to_string(), inner.response.rect);
    let mut output = inner.inner;

    if select_all_now {
        let char_count = buf.chars().count();
        output.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(char_count),
        )));
        output.state.store(ui.ctx(), output.response.id);
    }

    let is_focused = ui.memory(|m| m.has_focus(field_id));
    if !escape_pressed {
        if enter_pressed {
            state.drafts.remove(key);
            if buf != committed_val {
                emit_config_edit(key, &buf, state, out);
            }
            state.focus_search = true;
            ui.memory_mut(|m| m.request_focus(search_input_id()));
        } else if is_focused {
            state.drafts.insert(key.to_string(), buf);
        } else if output.response.lost_focus() {
            let final_draft = state.drafts.remove(key).unwrap_or(buf);
            if final_draft != committed_val {
                emit_config_edit(key, &final_draft, state, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_input() -> SettingsInput {
        let mut input = SettingsInput::default();
        input.placement.open = true;
        input
    }

    fn run_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        viewport: (f32, f32),
        state: &mut SettingsState,
        input: &SettingsInput,
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
                show(root_ui.ctx(), state, input, &mut out);
            },
        );
        // Brand-new `egui::Area`s run a sizing pass on their first frame; settle it on zero-event inspection frames.
        if had_no_events && input.placement.open {
            full = ctx.run_ui(
                egui::RawInput {
                    screen_rect,
                    ..Default::default()
                },
                |root_ui| {
                    show(root_ui.ctx(), state, input, &mut out);
                },
            );
        }
        let texts = collect_shape_texts(&full.shapes);
        (out, texts)
    }

    fn collect_shape_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(t) => out.push(t.galley.text().to_string()),
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, out);
                    }
                }
                _ => {}
            }
        }
        for cs in shapes {
            walk(&cs.shape, &mut out);
        }
        out
    }

    fn key_press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn settings_panel_lays_out_at_several_sizes_and_modes_without_panicking() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();
        input.dialled.insert("snap".to_string());
        input.dialled.insert("background".to_string());

        for &(w, h) in &[(1600.0, 1000.0), (960.0, 640.0), (360.0, 260.0)] {
            for side in [PanelSide::Float, PanelSide::Left, PanelSide::Right] {
                input.placement.side = side;
                input.dock_rect = match side {
                    PanelSide::Left => Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(380.0_f32.min(w), h),
                    )),
                    PanelSide::Right => Some(egui::Rect::from_min_size(
                        egui::pos2((w - 380.0).max(0.0), 0.0),
                        egui::vec2(380.0_f32.min(w), h),
                    )),
                    PanelSide::Float => None,
                };
                let (out, texts) = run_frame(&ctx, Vec::new(), (w, h), &mut state, &input);
                assert!(out.is_empty());
                assert!(state.wants_keyboard());
                assert!(texts.iter().any(|t| t.contains("settings")));
            }
        }

        // Closed panel renders nothing and releases the keyboard.
        input.placement.open = false;
        let (out, texts) = run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);
        assert!(out.is_empty());
        assert!(!state.wants_keyboard());
        assert!(texts.is_empty());
    }

    #[test]
    fn typing_query_and_pressing_enter_toggles_bool_and_steps_enum() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();

        // Frame 1: open panel so search takes focus.
        run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);

        // Type "snap crossing" (uniquely matching `snap`: "puts its top-left corner on a grid crossing")
        // and press Enter: default `snap` is `true`, so Enter emits `wall.config snap false`.
        let (out, _) = run_frame(
            &ctx,
            vec![
                egui::Event::Text("snap crossing".to_string()),
                key_press(egui::Key::Enter),
            ],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(state.query, "snap crossing");
        assert_eq!(out, vec!["wall.config snap false"]);

        // Clear and search "groupLayout", then press Enter to step from default `tile` to `freeform`.
        state.query.clear();
        let (out_enum, _) = run_frame(
            &ctx,
            vec![
                egui::Event::Text("groupLayout".to_string()),
                key_press(egui::Key::Enter),
            ],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(out_enum, vec!["wall.config groupLayout freeform"]);

        // ArrowDown navigation across matching knobs ("major" matches gridMajorEvery, gridMinPx, gridMajorColor).
        state.query.clear();
        run_frame(
            &ctx,
            vec![egui::Event::Text("major".to_string())],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(state.active, 0);
        run_frame(
            &ctx,
            vec![key_press(egui::Key::ArrowDown)],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(state.active, 1);

        // Flip `input.config.snap = false` and verify Enter emits `wall.config snap true`.
        input.config.snap = false;
        state.query = "snap crossing".to_string();
        state.active = 0;
        let (out_true, _) = run_frame(
            &ctx,
            vec![key_press(egui::Key::Enter)],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(out_true, vec!["wall.config snap true"]);
    }

    #[test]
    fn escape_clears_search_first_then_emits_close_and_empty_match_states_why() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let input = open_input();

        run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);

        // Unmatched query draws explicit message instead of blank space.
        let (out, texts) = run_frame(
            &ctx,
            vec![egui::Event::Text("zebra".to_string())],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert!(out.is_empty());
        assert!(
            texts
                .iter()
                .any(|t| t.contains("no setting matches “zebra”")),
            "expected empty-search message in {texts:?}"
        );

        // First Escape clears the query without closing.
        let (out_clear, _) = run_frame(
            &ctx,
            vec![key_press(egui::Key::Escape)],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert!(out_clear.is_empty());
        assert_eq!(state.query, "");

        // Second Escape on empty query closes the panel.
        let (out_close, _) = run_frame(
            &ctx,
            vec![key_press(egui::Key::Escape)],
            (1280.0, 800.0),
            &mut state,
            &input,
        );
        assert_eq!(out_close, vec!["wall.settings close"]);
    }

    #[test]
    fn simulated_clicks_pick_sections_reset_dialled_knobs_and_dock_panel() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();
        input.placement.side = PanelSide::Left;
        input.dock_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(380.0, 900.0),
        ));
        input.dialled.insert("closeHoldMs".to_string());

        // Two warm-up frames so layout positions are settled.
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);

        // Two-frame primary click (egui registers clicks on press in frame N, release in frame N+1).
        let click_at =
            |ctx: &egui::Context, pos: egui::Pos2, st: &mut SettingsState, inp: &SettingsInput| {
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
                    (1280.0, 900.0),
                    st,
                    inp,
                );
                let (mut release_out, _) = run_frame(
                    ctx,
                    vec![egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    (1280.0, 900.0),
                    st,
                    inp,
                );
                out.append(&mut release_out);
                out
            };

        let close_out = click_at(&ctx, egui::pos2(365.0, 14.0), &mut state, &input);
        assert_eq!(close_out, vec!["wall.settings close"]);

        let right_out = click_at(&ctx, egui::pos2(345.0, 14.0), &mut state, &input);
        assert_eq!(right_out, vec!["wall.settings right"]);

        // At this width the sections are chips above the settings: click `Canvas & grid`.
        let full = ctx.run_ui(
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
        fn find_text_center(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<egui::Pos2> {
            fn walk(shape: &egui::epaint::Shape, needle: &str) -> Option<egui::Pos2> {
                match shape {
                    egui::epaint::Shape::Text(t) if t.galley.text() == needle => {
                        Some(t.pos + t.galley.size() * 0.5)
                    }
                    egui::epaint::Shape::Vec(v) => v.iter().find_map(|s| walk(s, needle)),
                    _ => None,
                }
            }
            shapes.iter().find_map(|cs| walk(&cs.shape, needle))
        }
        let sec_pt = find_text_center(&full.shapes, "Canvas & grid")
            .expect("the Canvas & grid chip must be painted");
        assert_eq!(state.section_rects["canvas"].center(), sec_pt);
        let pick_out = click_at(&ctx, sec_pt, &mut state, &input);
        assert_eq!(pick_out, vec!["wall.settings section canvas"]);
        // It is shown on the next pass, before the wall has stored the choice.
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("canvas"));

        // Focus `closeHoldMs` via Enter, type an invalid value `-5`, press Enter: emits verb and records refusal.
        state.query = "closeHoldMs".to_string();
        state.active = 0;
        run_frame(
            &ctx,
            vec![key_press(egui::Key::Enter)],
            (1280.0, 900.0),
            &mut state,
            &input,
        );
        let (bad_out, _) = run_frame(
            &ctx,
            vec![
                egui::Event::Text("-5".to_string()),
                key_press(egui::Key::Enter),
            ],
            (1280.0, 900.0),
            &mut state,
            &input,
        );
        assert_eq!(bad_out, vec!["wall.config closeHoldMs -5"]);
        assert!(
            state
                .refused
                .get("closeHoldMs")
                .is_some_and(|m| m.contains("closeHoldMs takes a number ≥ 0")),
            "expected refusal for closeHoldMs, got {:?}",
            state.refused
        );
    }

    fn click(
        ctx: &egui::Context,
        pos: egui::Pos2,
        viewport: (f32, f32),
        state: &mut SettingsState,
        input: &SettingsInput,
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
        let (mut released, _) = run_frame(
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
        out.append(&mut released);
        out
    }

    fn knobs_in(section: &str) -> Vec<&'static str> {
        wall_core::settings::KNOBS
            .iter()
            .filter(|(k, d)| d.section == section && !NATIVE_HIDDEN_KNOBS.contains(k))
            .map(|(k, _)| *k)
            .collect()
    }

    #[test]
    fn the_window_is_a_section_list_beside_one_sections_settings() {
        const VIEW: (f32, f32) = (1400.0, 900.0);
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();

        // Floating and never placed: a window of its own size, in the
        // middle, under the topbar.
        let (out, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert!(out.is_empty());
        let rect = state.last_rect.unwrap();
        assert_eq!((rect.width(), rect.height()), (780.0, 560.0));
        assert_eq!(rect.center().x, 700.0);
        assert!(rect.top() >= 46.0 && rect.bottom() <= 900.0, "{rect:?}");

        // The list: every section, top to bottom, the first one shown.
        let listed: Vec<&str> = {
            let mut by_y: Vec<(&String, &egui::Rect)> = state.section_rects.iter().collect();
            by_y.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
            by_y.into_iter().map(|(k, _)| k.as_str()).collect()
        };
        assert_eq!(
            listed,
            ["terminals", "text", "canvas", "groups", "fullscreen", "selection", "input", "agent", "status"]
        );
        // The list is a column down the left; the settings are right of it.
        let list_right = state.section_rects["terminals"].right();
        assert!(state.section_rects.values().all(|r| r.left() == rect.left() + 1.0));
        assert_eq!(state.shown_section.as_deref(), Some("terminals"));
        assert_eq!(state.visible_count, knobs_in("terminals").len());
        for key in knobs_in("terminals") {
            assert!(state.control_rects[key].left() > list_right, "{key}");
        }
        // The search is above both.
        let search_text = texts.iter().any(|t| t == "search settings…");
        assert!(search_text, "{texts:?}");

        // Each setting: a plain label and one line saying what it does. The
        // knob's key and its long description are not on the page.
        for needle in [
            "Terminals",
            "New terminal size",
            "How big a new terminal opens, in columns by rows.",
            "Hold to close",
            "How long you hold the close button before a terminal is closed.",
        ] {
            assert!(texts.iter().any(|t| t == needle), "missing {needle:?} in {texts:?}");
        }
        assert!(!texts.iter().any(|t| t == "closeHoldMs" || t == "newTerminalSize"));
        assert!(!texts.iter().any(|t| t.contains("its ring fills over this time")));

        // A click on a section sends the verb, and the window shows the
        // section on the next pass without waiting for the wall.
        let text_line = state.section_rects["text"].center();
        assert_eq!(
            click(&ctx, text_line, VIEW, &mut state, &input),
            vec!["wall.settings section text"]
        );
        let (_, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("text"));
        assert_eq!(state.visible_count, knobs_in("text").len());
        assert!(texts.iter().any(|t| t == "The font terminals are drawn in."));
        // The same click again asks for nothing.
        assert!(click(&ctx, text_line, VIEW, &mut state, &input).is_empty());

        // The wall's answer is what counts from then on: here it stored
        // the choice, and later something else moved it.
        input.placement.section = Some("text".to_string());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("text"));
        input.placement.section = Some("groups".to_string());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("groups"));

        // Page Down and Page Up step through the list, stopping at its ends.
        let (out, _) = run_frame(&ctx, vec![key_press(egui::Key::PageDown)], VIEW, &mut state, &input);
        assert_eq!(out, vec!["wall.settings section fullscreen"]);
        input.placement.section = Some("terminals".to_string());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        let (out, _) = run_frame(&ctx, vec![key_press(egui::Key::PageUp)], VIEW, &mut state, &input);
        assert!(out.is_empty(), "{out:?}");

        // Dragged narrower than two panes have room for, the sections are
        // chips above the settings, across the window.
        input.placement.size.float_width = Some(480.0);
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        let rect = state.last_rect.unwrap();
        assert_eq!(rect.width(), 480.0);
        let chips_bottom = state
            .section_rects
            .values()
            .map(|r| r.bottom())
            .fold(0.0_f32, f32::max);
        assert_eq!(state.section_rects.len(), 9);
        assert!(state.section_rects.values().any(|r| r.left() > rect.center().x));
        for key in knobs_in("terminals") {
            let control = state.control_rects[key];
            assert!(control.top() > chips_bottom, "{key}");
        }
    }

    #[test]
    fn a_search_reads_every_section_and_says_which_each_match_is_in() {
        const VIEW: (f32, f32) = (1400.0, 900.0);
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();
        input.placement.section = Some("agent".to_string());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("agent"));

        // Typing searches every section, whichever was shown.
        run_frame(&ctx, vec![egui::Event::Text("colour".to_string())], VIEW, &mut state, &input);
        let (_, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section, None);
        assert_eq!(state.visible_count, 6);
        assert!(texts.iter().any(|t| t == "6 settings match “colour”"), "{texts:?}");
        // Each run of matches is under its section's name, and the list
        // counts the matches in each section, `All results` first.
        for heading in ["Text", "Canvas & grid", "Selection & copy", "All results"] {
            assert!(texts.iter().any(|t| t == heading), "missing {heading:?} in {texts:?}");
        }
        let all = state.section_rects[ALL_RESULTS];
        assert!(state
            .section_rects
            .iter()
            .all(|(k, r)| k == ALL_RESULTS || r.top() > all.top()));
        // The match is marked where it is: here in the label.
        for key in ["boldIsBright", "background", "gridMinorColor", "gridMajorColor", "selectionColor", "selectionForeground"] {
            assert!(state.control_rects.contains_key(key), "{key}");
        }

        // A click on a section narrows the results to it: the window's own
        // business, so nothing is sent.
        let canvas = state.section_rects["canvas"].center();
        assert!(click(&ctx, canvas, VIEW, &mut state, &input).is_empty());
        let (_, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("canvas"));
        assert_eq!(state.visible_count, 3);
        assert!(
            texts.iter().any(|t| t == "3 of 6 matches for “colour”, in Canvas & grid"),
            "{texts:?}"
        );
        // A section with no match is listed, dimmed, and a click on it does nothing.
        let agent = state.section_rects["agent"].center();
        assert!(click(&ctx, agent, VIEW, &mut state, &input).is_empty());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("canvas"));
        // `All results` widens it again.
        let all = state.section_rects[ALL_RESULTS].center();
        assert!(click(&ctx, all, VIEW, &mut state, &input).is_empty());
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!((state.shown_section.clone(), state.visible_count), (None, 6));

        // Narrowed again, then the search cleared: the window stays in that
        // section, which is the wall's to keep.
        let canvas = state.section_rects["canvas"].center();
        click(&ctx, canvas, VIEW, &mut state, &input);
        let (out, _) = run_frame(&ctx, vec![key_press(egui::Key::Escape)], VIEW, &mut state, &input);
        assert_eq!(out, vec!["wall.settings section canvas"]);
        assert_eq!(state.query, "");
        run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.shown_section.as_deref(), Some("canvas"));
        assert_eq!(state.visible_count, knobs_in("canvas").len());

        // A match only in a knob's key or long description shows where it
        // was: the key is on the page then, and only then.
        state.query = "closehold".to_string();
        let (_, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.visible_count, 1);
        assert!(texts.iter().any(|t| t == "closeHoldMs"), "{texts:?}");
        state.query = "ring fills".to_string();
        let (_, texts) = run_frame(&ctx, Vec::new(), VIEW, &mut state, &input);
        assert_eq!(state.visible_count, 1);
        assert!(texts.iter().any(|t| t.contains("its ring fills over this time")), "{texts:?}");
    }

    #[test]
    fn new_terminal_size_offers_its_presets_and_a_click_dials_one() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let mut input = open_input();
        input.placement.side = PanelSide::Left;
        input.dock_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(380.0, 900.0),
        ));
        state.query = "newTerminalSize".to_string();
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        assert_eq!(state.visible_count, 1);

        // The default, then the menu's sizes, each a chip; and the field.
        for size in ["100x30", "80x24", "120x36", "160x48"] {
            assert!(
                state.control_rects.contains_key(&format!("newTerminalSize@{size}")),
                "no chip for {size}: {:?}",
                state.control_rects.keys().collect::<Vec<_>>()
            );
        }
        assert!(state.control_rects.contains_key("newTerminalSize"));

        let click = |ctx: &egui::Context,
                     pos: egui::Pos2,
                     st: &mut SettingsState,
                     inp: &SettingsInput| {
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
                (1280.0, 900.0),
                st,
                inp,
            );
            let (mut released, _) = run_frame(
                ctx,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                (1280.0, 900.0),
                st,
                inp,
            );
            out.append(&mut released);
            out
        };

        let chip = state.control_rects["newTerminalSize@120x36"].center();
        assert_eq!(
            click(&ctx, chip, &mut state, &input),
            vec!["wall.config newTerminalSize 120x36"]
        );

        // The size in force is lit, and a click on it asks for nothing.
        input.config.new_terminal_size = "120x36".to_string();
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        let lit = state.control_rects["newTerminalSize@120x36"].center();
        assert!(click(&ctx, lit, &mut state, &input).is_empty());

        // A size that is no preset has no chip of its own: it is the field's.
        input.config.new_terminal_size = "132x43".to_string();
        input.config.menu_sizes = "80x24".to_string();
        run_frame(&ctx, Vec::new(), (1280.0, 900.0), &mut state, &input);
        let mut chips: Vec<&str> = state
            .control_rects
            .keys()
            .filter_map(|k| k.strip_prefix("newTerminalSize@"))
            .collect();
        chips.sort_unstable();
        assert_eq!(chips, vec!["100x30", "80x24"]);
    }

    #[test]
    fn native_settings_hides_browser_only_knobs_and_surfaces_live_renderer_knobs() {
        let ctx = egui::Context::default();
        let mut state = SettingsState::default();
        let input = open_input();

        // Browser-only / dead-live knobs stay in WallConfig for browser import round-trip,
        // but must not appear in the native Settings panel.
        for hidden in ["renderLoop", "traceInput", "menuHint"] {
            assert!(
                wall_core::config::WallConfig::default().get(hidden).is_some(),
                "{hidden} must remain in WallConfig for store/import compatibility"
            );
            state.query = hidden.to_string();
            let (_, texts) = run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);
            assert_eq!(state.visible_count, 0, "{hidden} should be hidden in native Settings");
            assert!(
                texts.iter().any(|t| t.contains("no setting matches")),
                "expected empty match for hidden knob {hidden}, got {texts:?}"
            );
        }

        // Newly promoted live renderer knobs appear and search cleanly.
        for live in ["panelOpacity", "greekPx", "cursorBlinkMs"] {
            state.query = live.to_string();
            let (_, texts) = run_frame(&ctx, Vec::new(), (1280.0, 800.0), &mut state, &input);
            assert!(
                state.visible_count >= 1,
                "{live} should appear in native Settings (texts={texts:?})"
            );
        }
    }
}
