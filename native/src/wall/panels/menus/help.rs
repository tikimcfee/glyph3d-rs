//! Port of `wall/HelpPanel.jsx` (+ `wall/Panel.jsx`) onto egui 0.36, backed by
//! [`wall_core::keymap`] and [`wall_core::dock`].
//!
//! Renders the wall's searchable keyboard-shortcut, pointer-gesture, and verb
//! reference sheet, either floating (draggable by its header, persisting its
//! spot via `wall.help float <x> <y>`) or docked to the left or right edge at
//! the rectangle given by [`wall_core::dock::dock_layout`].

use wall_core::chord::chord_label;
use wall_core::dock::{DockLayout, DockSide, PanelKind, PanelSize};
use wall_core::geom::Point;
use wall_core::input::Platform;
use wall_core::keymap::{HelpGroup, search_help};

use crate::wall::panels::resize::{self, ResizeSpec, ResizeState};

// Opaque #0e1118 on linear-blended sRGB targets so bright terminal borders behind
// the panel do not ghost through (`config/defaults.toml` [wall] panel_opacity note).
const PANEL_BG: egui::Color32 = egui::Color32::from_rgb(14, 17, 24);
const BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x30, 0x3c);
const HEAD_BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x24, 0x30);
const MUTED_COLOR: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x73, 0x83);
const SECTION_HEAD_COLOR: egui::Color32 = egui::Color32::from_rgb(0xd4, 0xd9, 0xe2);
const KEY_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8a, 0xb4, 0xf8);
const DOES_COLOR: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa3, 0xb2);
const FIELD_BG: egui::Color32 = egui::Color32::from_rgb(0x0b, 0x0d, 0x13);
const FIELD_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xe9, 0xef);
const BTN_TEXT: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x95, 0xa7);
const BTN_ON_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xe9, 0xef);
const BTN_ON_BG: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x23, 0x30);

const HOW_COLUMN_WIDTH: f32 = 140.0;
const FLOAT_DEFAULT_RIGHT: f32 = 12.0;
const FLOAT_DEFAULT_TOP: f32 = 46.0;

/// Read-only per-frame input to [`show`].
#[derive(Clone, Debug, PartialEq)]
pub struct HelpPanelInput {
    pub open: bool,
    pub side: DockSide,
    /// Saved floating top-left position in logical/CSS pixels (`state.helpPanel.float`);
    /// when `None` in [`DockSide::Float`], defaults to `right: 12, top: 46`.
    pub float_pos: Option<Point>,
    /// Docked rectangle computed from [`wall_core::dock::dock_layout`] when `side` is
    /// [`DockSide::Left`] or [`DockSide::Right`] (see [`Self::dock_rect_for`]).
    pub dock_rect: Option<egui::Rect>,
    pub platform: Platform,
    /// Populated help sections from [`wall_core::keymap::help_sections`].
    pub sections: Vec<HelpGroup>,
    /// The sizes set on the panel (the store's): its floating width and
    /// height are read here; its docked width arrives in `dock_rect`.
    pub size: PanelSize,
}

impl Default for HelpPanelInput {
    fn default() -> Self {
        Self {
            open: false,
            side: DockSide::Float,
            float_pos: None,
            dock_rect: None,
            platform: current_platform(),
            sections: Vec::new(),
            size: PanelSize::default(),
        }
    }
}

impl HelpPanelInput {
    /// Compute the docked panel rectangle for `side` inside `viewport` from
    /// [`wall_core::dock::DockLayout`], or `None` when floating.
    pub fn dock_rect_for(
        side: DockSide,
        layout: &DockLayout,
        viewport: egui::Rect,
    ) -> Option<egui::Rect> {
        let width = (layout.widths.help as f32).min(viewport.width().max(1.0));
        let offset = layout.help as f32;
        match side {
            DockSide::Left => {
                let left = viewport.left() + offset;
                Some(egui::Rect::from_min_max(
                    egui::pos2(left, viewport.top()),
                    egui::pos2((left + width).min(viewport.right()), viewport.bottom()),
                ))
            }
            DockSide::Right => {
                let right = viewport.right() - offset;
                Some(egui::Rect::from_min_max(
                    egui::pos2((right - width).max(viewport.left()), viewport.top()),
                    egui::pos2(right, viewport.bottom()),
                ))
            }
            DockSide::Float => None,
        }
    }
}

const fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else {
        Platform::Other
    }
}

#[derive(Clone, Copy, Debug)]
struct HeadDrag {
    grab_offset: egui::Vec2,
    current_pos: egui::Pos2,
}

/// Persistent view state for the help panel between frames.
#[derive(Clone, Debug, Default)]
pub struct HelpPanelState {
    was_open: bool,
    search_focused: bool,
    last_key_frame: Option<u64>,
    handled_key_frame: Option<u64>,
    last_search_rect: Option<egui::Rect>,
    /// Current search box text (`query` in `wall/HelpPanel.jsx`).
    pub query: String,
    drag: Option<HeadDrag>,
    /// The edge grips, and a resize drag in progress.
    pub resize: ResizeState,
    last_rect: Option<egui::Rect>,
    /// Floating header drag rectangle when open.
    pub header_drag_rect: Option<egui::Rect>,
    /// Total filtered rows displayed on the last open pass.
    pub shown_row_count: usize,
}

impl HelpPanelState {
    /// Last rendered screen rectangle of the help panel, if open.
    pub const fn last_rect(&self) -> Option<egui::Rect> {
        self.last_rect
    }

    /// Surrender keyboard focus from the help search field when the user clicks the canvas.
    pub fn blur(&mut self) {
        self.search_focused = false;
    }
}

/// JavaScript `Math.round` semantics (ties toward `+∞`) as an integer coordinate.
fn js_round_i64(value: f32) -> i64 {
    ((f64::from(value)) + 0.5).floor() as i64
}

fn is_toggle_help_chord(platform: Platform, modifiers: egui::Modifiers) -> bool {
    match platform {
        Platform::Mac => modifiers.mac_cmd || modifiers.command,
        Platform::Other => modifiers.ctrl && modifiers.alt && !modifiers.mac_cmd,
    }
}

/// Draw the help panel into `ctx` and append any triggered `wall.help` verbs to `out`.
///
/// Returns `true` when the help panel claims keyboard input this frame (when
/// opening, while its search field holds focus, or when handling `Escape` /
/// `chord + /`).
pub fn show(
    ctx: &egui::Context,
    state: &mut HelpPanelState,
    input: &HelpPanelInput,
    out: &mut Vec<String>,
) -> bool {
    if !input.open {
        state.was_open = false;
        state.search_focused = false;
        state.last_key_frame = None;
        state.handled_key_frame = None;
        state.last_search_rect = None;
        state.drag = None;
        state.last_rect = None;
        state.header_drag_rect = None;
        state.shown_row_count = 0;
        state.resize.reset();
        return false;
    }

    let search_id = egui::Id::new("wall_help_search_input");
    let frame_nr = ctx.cumulative_frame_nr();
    let just_opened = !state.was_open;
    state.was_open = true;
    if just_opened {
        state.search_focused = true;
    }
    if input.side != DockSide::Float {
        state.drag = None;
    }

    // Read raw frame events once per `cumulative_frame_nr`: egui's `Focus::begin_pass`
    // consumes `Key::Escape` from `i.events` and surrenders widget focus before panel
    // code runs, and a new/resized `Area` may run two layout passes in one frame.
    if state.last_key_frame != Some(frame_nr) {
        state.last_key_frame = Some(frame_nr);
        let mut chord_slash_pressed = false;
        let mut escape_pressed = false;
        let mut clicked_pos: Option<egui::Pos2> = None;

        ctx.input_mut(|i| {
            for ev in &i.raw.events {
                match ev {
                    egui::Event::Key {
                        key,
                        physical_key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let is_slash =
                            *key == egui::Key::Slash || *physical_key == Some(egui::Key::Slash);
                        if is_slash && is_toggle_help_chord(input.platform, *modifiers) {
                            chord_slash_pressed = true;
                        } else if state.search_focused && *key == egui::Key::Escape {
                            escape_pressed = true;
                        }
                    }
                    egui::Event::PointerButton {
                        pos,
                        pressed: true,
                        ..
                    } => {
                        clicked_pos = Some(*pos);
                    }
                    _ => {}
                }
            }
            if chord_slash_pressed || escape_pressed {
                i.events.retain(|ev| !matches!(
                    ev,
                    egui::Event::Key {
                        key: egui::Key::Escape | egui::Key::Slash,
                        pressed: true,
                        ..
                    }
                ));
            }
        });

        if let (Some(pos), Some(srect)) = (clicked_pos, state.last_search_rect) {
            state.search_focused = srect.contains(pos);
        }

        if chord_slash_pressed {
            out.push("wall.help close".to_owned());
            state.handled_key_frame = Some(frame_nr);
        } else if escape_pressed {
            state.handled_key_frame = Some(frame_nr);
            if state.query.is_empty() {
                out.push("wall.help close".to_owned());
                state.search_focused = false;
            } else {
                state.query.clear();
                state.search_focused = true;
            }
        }
    }

    if state.search_focused {
        ctx.memory_mut(|m| m.request_focus(search_id));
    } else {
        ctx.memory_mut(|m| m.surrender_focus(search_id));
    }

    let viewport = ctx.content_rect();
    let (panel_pos, panel_width, fixed_height, max_height) = compute_box_geometry(
        input,
        state.drag.map(|d| d.current_pos),
        &state.resize,
        viewport,
    );

    // Advance or finish an active floating-header drag before drawing.
    if let Some(mut drag) = state.drag {
        let (pointer_pos, primary_down) =
            ctx.input(|i| (i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()), i.pointer.primary_down()));
        if let Some(pos) = pointer_pos {
            drag.current_pos = egui::pos2(
                (pos.x - drag.grab_offset.x).max(0.0),
                (pos.y - drag.grab_offset.y).max(0.0),
            );
        }
        if primary_down {
            state.drag = Some(drag);
        } else {
            let final_pos = drag.current_pos;
            state.drag = None;
            out.push(format!(
                "wall.help float {} {}",
                js_round_i64(final_pos.x),
                js_round_i64(final_pos.y)
            ));
        }
    }

    let live_pos = state.drag.map_or(panel_pos, |d| d.current_pos);
    let shown = search_help(&input.sections, &state.query);
    state.shown_row_count = shown.iter().map(|s| s.rows.len()).sum();
    let is_floating = input.side == DockSide::Float;

    let frame = if is_floating {
        egui::Frame::new()
            .fill(PANEL_BG)
            .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
            .corner_radius(7.0)
            .shadow(egui::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: egui::Color32::from_black_alpha(115),
            })
    } else {
        egui::Frame::new().fill(PANEL_BG).corner_radius(0.0)
    };

    let area_resp = egui::Area::new(egui::Id::new("wall_help_panel"))
        .fixed_pos(live_pos)
        .order(egui::Order::Foreground)
        .constrain(false)
        .interactable(true)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_help initial sizing");
            }
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            let inner = frame.show(ui, |ui| {
                ui.set_min_width(panel_width);
                ui.set_max_width(panel_width);
                if let Some(exact_h) = fixed_height {
                    ui.set_min_height(exact_h);
                    ui.set_max_height(exact_h);
                } else {
                    ui.set_max_height(max_height);
                }

                draw_header(ui, state, input, live_pos, out);
                let search_rect = draw_search_bar(ui, search_id, &mut state.query);
                state.last_search_rect = Some(search_rect);
                draw_sections_body(ui, &shown, &input.sections, &state.query, fixed_height, max_height);
            });

            // Docked panels carry a single 1px border on their canvas-facing edge.
            let rect = inner.response.rect;
            match input.side {
                DockSide::Left => {
                    ui.painter().vline(
                        rect.right(),
                        rect.y_range(),
                        egui::Stroke::new(1.0, BORDER_COLOR),
                    );
                }
                DockSide::Right => {
                    ui.painter().vline(
                        rect.left(),
                        rect.y_range(),
                        egui::Stroke::new(1.0, BORDER_COLOR),
                    );
                }
                DockSide::Float => {}
            }

            // The edges that resize it.
            resize::grips(
                ui,
                &mut state.resize,
                &ResizeSpec {
                    verb: "wall.help",
                    side: input.side,
                    rect,
                    limits: PanelKind::Help.limits(),
                    viewport,
                    centered: false,
                },
                out,
            );
        });

    state.last_rect = Some(area_resp.response.rect);
    state.search_focused || state.handled_key_frame == Some(frame_nr)
}

fn compute_box_geometry(
    input: &HelpPanelInput,
    drag_pos: Option<egui::Pos2>,
    resizing: &ResizeState,
    viewport: egui::Rect,
) -> (egui::Pos2, f32, Option<f32>, f32) {
    match input.side {
        DockSide::Left | DockSide::Right => {
            let rect = input.dock_rect.unwrap_or_else(|| {
                let default_layout = DockLayout::default();
                HelpPanelInput::dock_rect_for(input.side, &default_layout, viewport)
                    .expect("Left/Right always produce a dock rect")
            });
            let width = rect.width().max(120.0);
            let height = rect.height().max(120.0);
            (rect.min, width, Some(height), height)
        }
        DockSide::Float => {
            let width = resize::float_width(
                resizing,
                &input.size,
                PanelKind::Help.limits().float_default_width as f32,
                viewport,
            );
            // Held to a height once one is set (or the bottom is being
            // dragged); else as tall as its rows, up to the viewport's room.
            let held = resize::float_height(resizing, &input.size, viewport);
            let max_h = held.unwrap_or((viewport.height() - 120.0).max(120.0));
            // A drag on an edge pins the panel where the drag has it.
            let pos = resize::float_origin(resizing)
                .or(drag_pos)
                .or_else(|| input.float_pos.map(|p| egui::pos2(p.x as f32, p.y as f32)))
                .unwrap_or_else(|| {
                    egui::pos2(
                        (viewport.right() - FLOAT_DEFAULT_RIGHT - width).max(0.0),
                        viewport.top() + FLOAT_DEFAULT_TOP,
                    )
                });
            // Its bottom edge stays in the window, wherever it was put.
            let max_h = max_h.min(resize::room_below(pos.y, viewport));
            // The floating panel's 1 px border is outside what it holds:
            // the size is the box with it.
            let rim = 2.0 * resize::FLOAT_RIM_PX;
            (pos, width - rim, held.map(|_| max_h - rim), max_h - rim)
        }
    }
}

fn draw_header(
    ui: &mut egui::Ui,
    state: &mut HelpPanelState,
    input: &HelpPanelInput,
    panel_top_left: egui::Pos2,
    out: &mut Vec<String>,
) {
    let mut buttons_left_x = f32::INFINITY;
    let close_tip = format!("close ({})", chord_label(input.platform, &["/"], false));

    let head = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 10,
            right: 6,
            top: 5,
            bottom: 5,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("help")
                            .font(egui::FontId::monospace(12.0))
                            .color(MUTED_COLOR),
                    )
                    .selectable(false),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if dock_button(ui, "wall_help_btn_close", "×", false, &close_tip, &mut buttons_left_x) {
                        out.push("wall.help close".to_owned());
                    }
                    if dock_button(
                        ui,
                        "wall_help_btn_right",
                        "⇥",
                        input.side == DockSide::Right,
                        "stick to the right edge",
                        &mut buttons_left_x,
                    ) {
                        out.push("wall.help right".to_owned());
                    }
                    if dock_button(
                        ui,
                        "wall_help_btn_float",
                        "◫",
                        input.side == DockSide::Float,
                        "float",
                        &mut buttons_left_x,
                    ) {
                        out.push("wall.help float".to_owned());
                    }
                    if dock_button(
                        ui,
                        "wall_help_btn_left",
                        "⇤",
                        input.side == DockSide::Left,
                        "stick to the left edge",
                        &mut buttons_left_x,
                    ) {
                        out.push("wall.help left".to_owned());
                    }
                });
            });
        });

    let head_rect = head.response.rect;
    ui.painter().hline(
        head_rect.x_range(),
        head_rect.bottom(),
        egui::Stroke::new(1.0, HEAD_BORDER_COLOR),
    );

    if input.side == DockSide::Float {
        // Keep the drag region strictly left of the four dock buttons so it never
        // steals hover or click hit-tests from them.
        let drag_right = if buttons_left_x.is_finite() {
            (buttons_left_x - 4.0).max(head_rect.left())
        } else {
            head_rect.right()
        };
        let drag_rect = egui::Rect::from_min_max(
            head_rect.min,
            egui::pos2(drag_right, head_rect.max.y),
        );
        state.header_drag_rect = Some(drag_rect);
        let head_resp = ui.interact(
            drag_rect,
            egui::Id::new("wall_help_head_drag"),
            egui::Sense::drag(),
        );
        if head_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if state.drag.is_none() {
            let started = ui.ctx().input(|i| {
                i.pointer.primary_pressed()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|pos| drag_rect.contains(pos))
            });
            if started {
                if let Some(press_pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                    state.drag = Some(HeadDrag {
                        grab_offset: press_pos - panel_top_left,
                        current_pos: panel_top_left,
                    });
                }
            }
        }
    }
}

/// Non-focusable 18×18 header button (`Btn` in `wall/Panel.jsx`).
fn dock_button(
    ui: &mut egui::Ui,
    id_str: &'static str,
    glyph: &str,
    on: bool,
    tooltip: &str,
    buttons_left_x: &mut f32,
) -> bool {
    let _ = id_str;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    *buttons_left_x = buttons_left_x.min(rect.left());
    let resp = resp.on_hover_text(tooltip);
    let fill = if on {
        BTN_ON_BG
    } else if resp.hovered() {
        egui::Color32::from_rgba_unmultiplied(0x2a, 0x30, 0x3c, 120)
    } else {
        egui::Color32::TRANSPARENT
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 4.0, fill);
    }
    let text_color = if on { BTN_ON_TEXT } else { BTN_TEXT };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::monospace(12.0),
        text_color,
    );
    resp.clicked()
}

fn draw_search_bar(ui: &mut egui::Ui, search_id: egui::Id, query: &mut String) -> egui::Rect {
    let outer = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 8,
            bottom: 4,
        })
        .show(ui, |ui| {
            let box_w = ui.available_width();
            let field_frame = egui::Frame::new()
                .fill(FIELD_BG)
                .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
                .corner_radius(5.0)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(query)
                            .id(search_id)
                            .frame(egui::Frame::NONE)
                            .hint_text("search keys, gestures, verbs…")
                            .font(egui::FontId::monospace(12.5))
                            .text_color(FIELD_TEXT)
                            .desired_width((box_w - 18.0).max(80.0)),
                    );
                });
            field_frame.response.rect
        });
    outer.inner
}

fn draw_sections_body(
    ui: &mut egui::Ui,
    shown: &[HelpGroup],
    all_sections: &[HelpGroup],
    query: &str,
    fixed_height: Option<f32>,
    max_height: f32,
) {
    let chrome_overhead = 78.0;
    let body_max_h = fixed_height
        .unwrap_or(max_height)
        .saturating_sub_f32(chrome_overhead)
        .max(60.0);

    egui::ScrollArea::vertical()
        .id_salt("wall_help_scroll_body")
        .max_height(body_max_h)
        .auto_shrink([false, fixed_height.is_none()])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.add_space(2.0);

            let has_any_source_rows = all_sections.iter().any(|s| !s.rows.is_empty());
            if shown.is_empty() {
                let empty_msg = if query.trim().is_empty() && !has_any_source_rows {
                    "no help entries".to_owned()
                } else {
                    format!("nothing matches “{query}”")
                };
                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 10,
                        top: 6,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(empty_msg)
                                .font(egui::FontId::monospace(12.0))
                                .color(MUTED_COLOR),
                        );
                    });
                return;
            }

            for section in shown {
                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 10,
                        bottom: 4,
                    })
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(section.title)
                                    .font(egui::FontId::monospace(12.0))
                                    .color(SECTION_HEAD_COLOR),
                            )
                            .selectable(false),
                        );
                    });

                let is_verbs = section.id == "verbs";
                for row in &section.rows {
                    if is_verbs {
                        egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: 16,
                                right: 10,
                                top: 3,
                                bottom: 4,
                            })
                            .show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&row.how)
                                                .font(egui::FontId::monospace(12.0))
                                                .color(KEY_COLOR),
                                        )
                                        .wrap(),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&row.does)
                                                .font(egui::FontId::monospace(12.0))
                                                .color(DOES_COLOR),
                                        )
                                        .wrap(),
                                    );
                                });
                            });
                    } else {
                        egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: 16,
                                right: 10,
                                top: 3,
                                bottom: 3,
                            })
                            .show(ui, |ui| {
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(HOW_COLUMN_WIDTH, 16.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_min_width(HOW_COLUMN_WIDTH);
                                            ui.set_max_width(HOW_COLUMN_WIDTH);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&row.how)
                                                        .font(egui::FontId::monospace(12.0))
                                                        .color(KEY_COLOR),
                                                )
                                                .wrap(),
                                            );
                                        },
                                    );
                                    let does_w = (ui.available_width()).max(60.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(does_w, 16.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_max_width(does_w);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&row.does)
                                                        .font(egui::FontId::monospace(12.0))
                                                        .color(DOES_COLOR),
                                                )
                                                .wrap(),
                                            );
                                        },
                                    );
                                });
                            });
                    }
                }
            }
            ui.add_space(10.0);
        });
}

trait SaturatingSubF32 {
    fn saturating_sub_f32(self, rhs: f32) -> f32;
}

impl SaturatingSubF32 for f32 {
    fn saturating_sub_f32(self, rhs: f32) -> f32 {
        (self - rhs).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::dock::{DOCK_WIDTHS, DockPanel, DockState, dock_layout};
    use wall_core::keymap::{HelpVerb, help_sections};

    fn sample_sections() -> Vec<HelpGroup> {
        let verbs = [
            HelpVerb {
                name: "wall.reveal",
                usage: "<id>",
                description: "Show a terminal, focus it and frame it",
            },
            HelpVerb {
                name: "wall.help",
                usage: "[toggle|open|close|left|right|float [x y]]",
                description: "Open, close or place the help",
            },
        ];
        help_sections(Platform::Mac, "ctrl alt meta", &verbs).expect("valid zoom keys")
    }

    fn make_raw_input_at(
        size: (f32, f32),
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0, size.1),
            )),
            time: Some(time),
            events,
            ..Default::default()
        }
    }

    fn make_raw_input(size: (f32, f32), events: Vec<egui::Event>) -> egui::RawInput {
        make_raw_input_at(size, 0.0, events)
    }

    fn run_frame(
        ctx: &egui::Context,
        state: &mut HelpPanelState,
        input: &HelpPanelInput,
        raw: egui::RawInput,
    ) -> (bool, Vec<String>, egui::FullOutput) {
        let mut out = Vec::new();
        let mut wants_keys = false;
        let full = crate::wall::ui::headless_pass(ctx, raw, |root_ui| {
            wants_keys = show(root_ui.ctx(), state, input, &mut out);
        });
        (wants_keys, out, full)
    }

    #[test]
    fn lays_out_floating_and_docked_across_sizes_without_panicking() {
        let sections = sample_sections();
        for (w, h) in [(360.0, 260.0), (900.0, 640.0), (1600.0, 1000.0)] {
            let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h));
            for side in [DockSide::Float, DockSide::Left, DockSide::Right] {
                let dock = dock_layout(&DockState {
                    sidebar: DockPanel::new(true, DockSide::Right),
                    help_panel: Some(DockPanel::new(true, side)),
                    ..Default::default()
                });
                let input = HelpPanelInput {
                    open: true,
                    side,
                    float_pos: None,
                    dock_rect: HelpPanelInput::dock_rect_for(side, &dock, viewport),
                    platform: Platform::Mac,
                    sections: sections.clone(),
    size: Default::default(),
                };
                let ctx = egui::Context::default();
                let mut state = HelpPanelState::default();
                let (wants_keys, out, full) =
                    run_frame(&ctx, &mut state, &input, make_raw_input((w, h), vec![]));
                assert!(wants_keys, "opening help focuses search and claims keys");
                assert!(out.is_empty());
                assert!(!full.shapes.is_empty());
                let rect = state.last_rect().expect("help panel rect recorded");
                if side == DockSide::Right && w >= 800.0 {
                    let expected_right = w - DOCK_WIDTHS.sidebar as f32;
                    assert!(
                        (rect.right() - expected_right).abs() <= 1.0,
                        "docked right help should stack inward of sidebar: got {}, expected {expected_right}",
                        rect.right()
                    );
                }
            }
        }
    }

    #[test]
    fn search_filters_escape_clears_then_closes_and_chord_slash_closes() {
        let input = HelpPanelInput {
            open: true,
            side: DockSide::Float,
            float_pos: Some(Point::new(100.0, 60.0)),
            dock_rect: None,
            platform: Platform::Mac,
            sections: sample_sections(),
    size: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut state = HelpPanelState::default();

        // Open and type a query into the auto-focused search box.
        let (wants, out_type, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((1024.0, 768.0), vec![egui::Event::Text("nonexistentword".to_owned())]),
        );
        assert!(wants);
        assert!(out_type.is_empty());
        assert_eq!(state.query, "nonexistentword");

        // First Escape clears the search query without closing.
        let (wants_esc1, out_esc1, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (1024.0, 768.0),
                vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: Some(egui::Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
        );
        assert!(wants_esc1);
        assert!(out_esc1.is_empty());
        assert!(state.query.is_empty());

        // Second Escape with empty query emits `wall.help close`.
        let (_, out_esc2, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (1024.0, 768.0),
                vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: Some(egui::Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
        );
        assert_eq!(out_esc2, vec!["wall.help close"]);

        // Cmd+/ (`chord + /` on Mac) also emits `wall.help close` even with non-empty query.
        state.query = "split".to_owned();
        let (_, out_chord, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (1024.0, 768.0),
                vec![egui::Event::Key {
                    key: egui::Key::Slash,
                    physical_key: Some(egui::Key::Slash),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        mac_cmd: true,
                        command: true,
                        ..Default::default()
                    },
                }],
            ),
        );
        assert_eq!(out_chord, vec!["wall.help close"]);
    }

    #[test]
    fn dock_buttons_and_floating_header_drag_emit_placement_verbs() {
        let input = HelpPanelInput {
            open: true,
            side: DockSide::Float,
            float_pos: Some(Point::new(100.0, 60.0)),
            dock_rect: None,
            platform: Platform::Mac,
            sections: sample_sections(),
    size: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut state = HelpPanelState::default();
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at((1024.0, 768.0), 0.0, vec![]),
        );
        let rect = state.last_rect().expect("panel rect");

        // Drag the left side of the header (away from the right-aligned dock buttons) by (+45, +25).
        let grab = egui::pos2(rect.left() + 40.0, rect.top() + 12.0);
        let drop_at = egui::pos2(grab.x + 45.0, grab.y + 25.0);
        let (_, out_down, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at(
                (1024.0, 768.0),
                0.1,
                vec![
                    egui::Event::PointerMoved(grab),
                    egui::Event::PointerButton {
                        pos: grab,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
        );
        assert!(out_down.is_empty());

        let (_, out_move, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at((1024.0, 768.0), 0.2, vec![egui::Event::PointerMoved(drop_at)]),
        );
        assert!(out_move.is_empty());

        let (_, out_up, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at(
                (1024.0, 768.0),
                0.3,
                vec![egui::Event::PointerButton {
                    pos: drop_at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
        );
        assert_eq!(out_up, vec!["wall.help float 145 85"]);

        // Settle pointer over the `⇥` right-dock button, then click; repeat on `×` close.
        let close_pos = egui::pos2(rect.right() - 15.0, rect.top() + 14.0);
        let right_pos = egui::pos2(rect.right() - 35.0, rect.top() + 14.0);
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at((1024.0, 768.0), 1.0, vec![egui::Event::PointerMoved(right_pos)]),
        );
        let (_, out_right, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at(
                (1024.0, 768.0),
                1.2,
                vec![
                    egui::Event::PointerButton {
                        pos: right_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: right_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
        );
        assert_eq!(out_right, vec!["wall.help right"]);

        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at((1024.0, 768.0), 2.0, vec![egui::Event::PointerMoved(close_pos)]),
        );
        let (_, out_close, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input_at(
                (1024.0, 768.0),
                2.2,
                vec![
                    egui::Event::PointerButton {
                        pos: close_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: close_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
        );
        assert_eq!(out_close, vec!["wall.help close"]);
    }

    #[test]
    fn empty_sections_input_draws_explicit_empty_state() {
        let input = HelpPanelInput {
            open: true,
            side: DockSide::Float,
            float_pos: None,
            dock_rect: None,
            platform: Platform::Other,
            sections: vec![],
    size: Default::default(),
        };
        let ctx = egui::Context::default();
        let mut state = HelpPanelState::default();
        let (wants, out, full) =
            run_frame(&ctx, &mut state, &input, make_raw_input((640.0, 480.0), vec![]));
        assert!(wants);
        assert!(out.is_empty());
        assert!(!full.shapes.is_empty());
    }
}
