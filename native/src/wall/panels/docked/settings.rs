//! Settings panel (`wall/SettingsPanel.jsx`): every wall knob in sections, searchable,
//! keyboard-first, emitting `wall.config`, `wall.settings`, and `wall.settings.collapse` verbs.

use std::collections::{BTreeMap, BTreeSet};

use wall_core::chord::chord_label;
use wall_core::config::{check_config, default_knob_value, WallConfig};
use wall_core::dock::DOCK_WIDTHS;
use wall_core::input::Platform;
use wall_core::settings::{search_settings, KnobDef, KnobKind, KnobValue, SettingsGroup};
use wall_core::store::{PanelSide, SettingsPanelState as SettingsPlacement};

use super::{
    dock_buttons_rtl, handle_head_drag, icon_btn, marked_layout_job, panel_viewport_rect,
    resolve_panel_box, settings_panel_bg, ChromeIcon, HeadDragState, ACTIVE_ROW_BG, BORDER, BRIGHT,
    BTN_ON_BG, DESC_TEXT, ERR_RED, FIELD_BG, HEAD_BORDER, HOVER_ROW_BG, KEY_TEXT, LABEL, MUTED,
    OK_GREEN,
};

/// Plain data read by [`show`] (`<Panel>Input` in `P-panels.md`).
#[derive(Clone, Debug)]
pub struct SettingsInput {
    /// Open flag, side (`Left`/`Right`/`Float`), floating coordinates, and folded section ids.
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
    /// Index of the highlighted knob within the unfolded matching items (`SettingsPanel.jsx`'s `active`).
    pub active: usize,
    /// Whether the settings panel wants keyboard input this frame (`true` whenever open).
    pub wants_keyboard: bool,
    /// Floating header drag state (`useHeadDrag` in `Panel.jsx`).
    pub head_drag: HeadDragState,
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
    /// Number of visible unfolded matching knobs on the last open pass.
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

/// Draw the settings panel (`wall/SettingsPanel.jsx`).
///
/// Returns `true` when the panel wants keyboard input this frame (also stored on
/// [`SettingsState::wants_keyboard`]).
pub fn show(
    ctx: &egui::Context,
    state: &mut SettingsState,
    input: &SettingsInput,
    out: &mut Vec<String>,
) -> bool {
    state.control_rects.clear();
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

    // Close chord (`⌘,` on macOS, `Ctrl+Alt+,` on Linux/Windows) closes from anywhere in the panel.
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
    let (origin, width, max_height) = resolve_panel_box(
        viewport,
        side,
        input.dock_rect,
        input.placement.float,
        &state.head_drag,
        DOCK_WIDTHS.settings as f32,
    );

    let rounding = match side {
        PanelSide::Float => egui::CornerRadius::same(7),
        PanelSide::Left | PanelSide::Right => egui::CornerRadius::ZERO,
    };
    let frame_stroke = match side {
        PanelSide::Float => egui::Stroke::new(1.0, BORDER),
        PanelSide::Left | PanelSide::Right => egui::Stroke::NONE,
    };

    let close_title = format!("close ({})", chord_label(input.platform, &[","], false));

    egui::Area::new(egui::Id::new("wall_settings_panel"))
        .fixed_pos(origin)
        .order(egui::Order::Foreground)
        .constrain(false)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_settings initial sizing");
            }
            let panel_frame = egui::Frame::NONE
                .fill(settings_panel_bg())
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

                // ── Header (`settings` + `⇤ ◫ ⇥ ×`) ──────────────────────────
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
                                dock_buttons_rtl(ui, "wall.settings", side, &close_title, out);

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

                // ── Search input & keyboard navigation ───────────────────────
                let search_id = search_input_id();
                if state.focus_search {
                    ui.memory_mut(|m| m.request_focus(search_id));
                    state.focus_search = false;
                }

                // Determine whether any per-knob text field currently holds focus.
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
                    });
                }

                let prev_query = state.query.clone();
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 8,
                        bottom: 4,
                    })
                    .show(ui, |ui| {
                        let field_frame = egui::Frame::NONE
                            .fill(FIELD_BG)
                            .stroke(egui::Stroke::new(1.0, BORDER))
                            .corner_radius(egui::CornerRadius::same(5))
                            .inner_margin(egui::Margin::symmetric(8, 6));
                        field_frame.show(ui, |ui| {
                            ui.set_width((width - 38.0).max(40.0));
                            ui.add(
                                egui::TextEdit::singleline(&mut state.query)
                                    .id(search_id)
                                    .frame(egui::Frame::NONE)
                                    .font(egui::FontId::monospace(12.5))
                                    .text_color(BRIGHT)
                                    .hint_text(
                                        egui::RichText::new("search settings…")
                                            .font(egui::FontId::monospace(12.5))
                                            .color(MUTED),
                                    )
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    });

                if state.query != prev_query {
                    state.active = 0;
                }

                if pressed_escape {
                    if !state.query.is_empty() {
                        state.query.clear();
                        state.active = 0;
                        ui.memory_mut(|m| m.request_focus(search_id));
                    } else {
                        out.push("wall.settings close".to_string());
                    }
                }

                let groups: Vec<SettingsGroup<'static>> = search_settings(&state.query);
                let searching = !state.query.trim().is_empty();
                let is_folded = |section_id: &str| -> bool {
                    !searching && input.placement.collapsed.iter().any(|c| c == section_id)
                };
                let visible_keys: Vec<(&'static str, KnobDef)> = groups
                    .iter()
                    .filter(|g| !is_folded(g.section.id))
                    .flat_map(|g| g.items.iter().map(|it| (it.key, it.def)))
                    .collect();
                state.visible_count = visible_keys.len();

                let mut at = state.active.min(visible_keys.len().saturating_sub(1));
                let mut moved_by_keys = false;
                if pressed_down && !visible_keys.is_empty() {
                    at = (at + 1).min(visible_keys.len() - 1);
                    state.active = at;
                    moved_by_keys = true;
                } else if pressed_up {
                    at = at.saturating_sub(1);
                    state.active = at;
                    moved_by_keys = true;
                } else {
                    state.active = at;
                }

                if pressed_enter {
                    if let Some(&(active_key, active_def)) = visible_keys.get(at) {
                        activate_knob(active_key, &active_def, &input.config, state, out);
                    }
                }

                let active_key = visible_keys.get(at).map(|&(k, _)| k);

                // ── Scrollable body ──────────────────────────────────────────
                let body_max_h = (max_height - 76.0).max(48.0);
                egui::ScrollArea::vertical()
                    .id_salt("wall_settings_body")
                    .max_height(body_max_h)
                    .auto_shrink([false, side == PanelSide::Float])
                    .show(ui, |ui| {
                        ui.set_width(width);
                        ui.spacing_mut().item_spacing = egui::vec2(0.0, 2.0);

                        if groups.is_empty() {
                            egui::Frame::NONE
                                .inner_margin(egui::Margin {
                                    left: 26,
                                    right: 10,
                                    top: 8,
                                    bottom: 10,
                                })
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "no setting matches “{}”",
                                            state.query
                                        ))
                                        .font(egui::FontId::monospace(12.0))
                                        .color(MUTED),
                                    );
                                });
                            return;
                        }

                        let mut flat_idx = 0usize;
                        for group in &groups {
                            let sec_id = group.section.id;
                            let folded = is_folded(sec_id);

                            // Section header row
                            let sec_resp = egui::Frame::NONE
                                .inner_margin(egui::Margin {
                                    left: 10,
                                    right: 10,
                                    top: 8,
                                    bottom: 4,
                                })
                                .show(ui, |ui| {
                                    ui.set_width((width - 20.0).max(40.0));
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
                                        let arrow = if searching {
                                            " "
                                        } else if folded {
                                            "▸"
                                        } else {
                                            "▾"
                                        };
                                        ui.label(
                                            egui::RichText::new(arrow)
                                                .font(egui::FontId::monospace(11.0))
                                                .color(MUTED),
                                        );
                                        ui.label(marked_layout_job(
                                            group.section.title,
                                            &group.title,
                                            egui::FontId::monospace(12.0),
                                            LABEL,
                                        ));
                                        ui.label(
                                            egui::RichText::new(group.section.subtitle)
                                                .font(egui::FontId::monospace(11.0))
                                                .color(MUTED),
                                        );
                                        if searching {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            group.items.len().to_string(),
                                                        )
                                                        .font(egui::FontId::monospace(11.0))
                                                        .color(MUTED),
                                                    );
                                                },
                                            );
                                        }
                                    });
                                })
                                .response;

                            let sec_click = ui.interact(
                                sec_resp.rect,
                                ui.make_persistent_id(("wall_settings_sec", sec_id)),
                                egui::Sense::click(),
                            );
                            if !searching {
                                if sec_click.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if sec_click.clicked() {
                                    out.push(format!("wall.settings.collapse {sec_id}"));
                                }
                            }

                            if folded {
                                continue;
                            }

                            for item in &group.items {
                                let my_idx = flat_idx;
                                flat_idx += 1;
                                let is_active = active_key == Some(item.key);
                                let is_dialled = input.dialled.contains(item.key);
                                let current_val = input
                                    .config
                                    .get(item.key)
                                    .or_else(|| default_knob_value(item.key))
                                    .expect("every KNOBS entry has a default value");
                                let refusal = input
                                    .refused
                                    .get(item.key)
                                    .or_else(|| state.refused.get(item.key))
                                    .cloned();

                                let row_bg = if is_active {
                                    ACTIVE_ROW_BG
                                } else {
                                    egui::Color32::TRANSPARENT
                                };

                                let item_resp = egui::Frame::NONE
                                    .fill(row_bg)
                                    .inner_margin(egui::Margin {
                                        left: 26,
                                        right: 10,
                                        top: 5,
                                        bottom: 6,
                                    })
                                    .show(ui, |ui| {
                                        ui.set_width((width - 36.0).max(40.0));
                                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 2.0);

                                        // Top line: label on the left, optional `↺` + control on the right.
                                        ui.horizontal(|ui| {
                                            ui.set_min_height(22.0);
                                            ui.label(marked_layout_job(
                                                item.def.label,
                                                &item.hits.label,
                                                egui::FontId::monospace(12.0),
                                                LABEL,
                                            ));
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.spacing_mut().item_spacing =
                                                        egui::vec2(5.0, 0.0);
                                                    render_knob_control(
                                                        ui,
                                                        item.key,
                                                        &item.def,
                                                        &current_val,
                                                        state,
                                                        out,
                                                    );
                                                    if is_dialled
                                                        && icon_btn(
                                                            ui,
                                                            ("wall_settings_reset", item.key),
                                                            ChromeIcon::Reset,
                                                            false,
                                                            "back to the default",
                                                        )
                                                        .clicked()
                                                    {
                                                        state.drafts.remove(item.key);
                                                        emit_config_edit(
                                                            item.key, "default", state, out,
                                                        );
                                                    }
                                                },
                                            );
                                        });

                                        // Description
                                        ui.label(marked_layout_job(
                                            item.def.description,
                                            &item.hits.description,
                                            egui::FontId::monospace(11.0),
                                            DESC_TEXT,
                                        ));

                                        // Key identifier
                                        ui.label(marked_layout_job(
                                            item.key,
                                            &item.hits.key,
                                            egui::FontId::monospace(10.5),
                                            KEY_TEXT,
                                        ));

                                        // Validation refusal, if any
                                        if let Some(err_msg) = refusal {
                                            ui.label(
                                                egui::RichText::new(err_msg)
                                                    .font(egui::FontId::monospace(11.0))
                                                    .color(ERR_RED),
                                            );
                                        }
                                    })
                                    .response;

                                if ui.rect_contains_pointer(item_resp.rect)
                                    && ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO)
                                {
                                    state.active = my_idx;
                                }

                                if is_active
                                    && (moved_by_keys
                                        || state.last_scrolled_key.as_deref() != Some(item.key))
                                {
                                    if moved_by_keys {
                                        ui.scroll_to_rect(item_resp.rect, None);
                                    }
                                    state.last_scrolled_key = Some(item.key.to_string());
                                }
                            }
                        }
                    });
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
        });

    true
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
    fn simulated_clicks_fold_sections_reset_dialled_knobs_and_dock_panel() {
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

        // Locate the `"Canvas"` section title (the first section in `SECTIONS`) and click it to fold.
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
        let sec_pt = find_text_center(&full.shapes, "Canvas")
            .expect("Canvas section header must be painted");
        let fold_out = click_at(&ctx, sec_pt, &mut state, &input);
        assert_eq!(fold_out, vec!["wall.settings.collapse canvas"]);

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
}
