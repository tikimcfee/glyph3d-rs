//! Native `egui` command/find palette (`wall/Palette.jsx`), backed by
//! [`wall_core::palette`].
//!
//! Contract (`out/wall-port/briefs/P-panels.md`):
//! `pub fn show(ctx: &egui::Context, state: &mut PaletteState, input: &PaletteInput, out: &mut Vec<String>)`
//!
//! - While `state.open` is true, [`PaletteState::wants_keyboard`] is `true` so
//!   the host sends keyboard events to `egui` rather than to the focused
//!   terminal.
//! - Typing filters items via [`wall_core::palette::palette_items`], `↑`/`↓`
//!   move the highlighted row, `Enter` (or clicking a row) appends the chosen
//!   `wall.*` verb line to `out` (or transitions into `Rename`/`Token` mode),
//!   and `Escape` clears a non-empty search query in `Find` mode before closing
//!   the palette on a second `Escape` (or immediately when the query is empty
//!   or in `Rename`/`Token` mode). Clicking the backdrop scrim also closes it.

use wall_core::palette::{
    Palette, PaletteEffect, PaletteItem, PaletteItemKind, PaletteMode, PaletteSnapshot,
};

use wall_core::dock::{DockSide, PanelSize, PALETTE_LIMITS};

use super::resize::{self, ResizeSpec, ResizeState};
use super::{format_verb_line, layout_elided, parse_hex_color, safe_viewport_rect};

const INPUT_ID_SALT: &str = "wall_palette_query_input";
const SCRIM_ID_SALT: &str = "wall_palette_scrim";
const MODAL_ID_SALT: &str = "wall_palette_modal";

/// Plain-data input read by [`show`].
#[derive(Clone, Debug, Default)]
pub struct PaletteInput {
    pub snapshot: PaletteSnapshot,
    /// The size set on the palette (`wall.palette size`): its width, and
    /// the height its box is held to. Unset, it is 560 px wide and as tall
    /// as its list, up to most of the window.
    pub size: PanelSize,
}

impl From<PaletteSnapshot> for PaletteInput {
    fn from(snapshot: PaletteSnapshot) -> Self {
        Self {
            snapshot,
            size: PanelSize::default(),
        }
    }
}

/// Persistent view state for the palette held across frames by the caller.
#[derive(Clone, Debug, Default)]
pub struct PaletteState {
    /// Whether the palette modal is currently visible.
    pub open: bool,
    /// Pure mode, query, and selection model from `wall_core::palette`.
    pub palette: Palette,
    /// Whether the palette claims keyboard input this frame (`true` whenever
    /// `open` is `true` during [`show`]).
    pub wants_keyboard: bool,
    /// Last pointer position observed inside the list, so keyboard `↑`/`↓`
    /// navigation is not overridden by a stationary cursor hovering a row.
    last_pointer_pos: Option<egui::Pos2>,
    /// Last rendered modal bounding box when open.
    pub last_rect: Option<egui::Rect>,
    /// Number of filtered items shown in `Find` mode on the last open pass.
    pub item_count: usize,
    /// The edge grips, and a resize drag in progress.
    pub resize: ResizeState,
}

impl PaletteState {
    /// Open the palette in `Find` mode with an empty query and index `0`.
    pub fn open_find(&mut self) {
        self.open_mode(PaletteMode::Find);
    }

    /// Open the palette in `mode` with an empty query and index `0`.
    pub fn open_mode(&mut self, mode: PaletteMode) {
        self.open = true;
        self.palette = Palette::new(mode);
        self.wants_keyboard = true;
        self.last_pointer_pos = None;
    }

    /// Toggle the palette open (`Find` mode) or closed.
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open_find();
        }
    }

    /// Close the palette and reset its transient query/mode.
    pub fn close(&mut self) {
        self.open = false;
        self.wants_keyboard = false;
        self.palette = Palette::new(PaletteMode::Find);
        self.last_pointer_pos = None;
        self.last_rect = None;
        self.item_count = 0;
    }

    /// Whether the palette wants keyboard events routed to `egui` rather than
    /// to a terminal.
    pub const fn wants_keyboard(&self) -> bool {
        self.wants_keyboard
    }
}

fn dispatch_effect(state: &mut PaletteState, effect: PaletteEffect, out: &mut Vec<String>) {
    match effect {
        PaletteEffect::Run { cmd, close } => {
            if !cmd.is_empty() {
                out.push(format_verb_line(&cmd));
            }
            if close {
                state.close();
            }
        }
        PaletteEffect::EnterRename { .. } | PaletteEffect::EnterToken | PaletteEffect::Noop => {}
    }
}

fn paint_item_prefix(painter: &egui::Painter, rect: egui::Rect, item: &PaletteItem, color: egui::Color32) {
    let c = rect.center();
    match &item.kind {
        PaletteItemKind::Group { .. } => {
            // `▤` group icon: crisp stroked box with horizontal rules.
            let box_rect = egui::Rect::from_center_size(c, egui::vec2(9.0, 9.0));
            let stroke = egui::Stroke::new(1.15, color);
            painter.rect_stroke(
                box_rect,
                egui::CornerRadius::same(1),
                stroke,
                egui::StrokeKind::Inside,
            );
            for dy in [-1.6_f32, 1.6] {
                painter.line_segment(
                    [
                        egui::pos2(box_rect.left() + 1.5, c.y + dy),
                        egui::pos2(box_rect.right() - 1.5, c.y + dy),
                    ],
                    stroke,
                );
            }
        }
        PaletteItemKind::Agent { .. } => {
            // `◆` agent diamond in the agent's status colour.
            let r = 4.2;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x, c.y - r),
                    egui::pos2(c.x + r, c.y),
                    egui::pos2(c.x, c.y + r),
                    egui::pos2(c.x - r, c.y),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        PaletteItemKind::Terminal { .. } | PaletteItemKind::Action { .. } => {}
    }
}

/// Draw the palette over the canvas if `state.open` is true, appending any
/// triggered `wall.*` verb lines into `out`.
pub fn show(
    ctx: &egui::Context,
    state: &mut PaletteState,
    input: &PaletteInput,
    out: &mut Vec<String>,
) {
    if !state.open {
        state.wants_keyboard = false;
        state.last_rect = None;
        state.item_count = 0;
        state.resize.reset();
        return;
    }
    state.wants_keyboard = true;
    state.palette.sync(&input.snapshot);

    // Intercept Escape, Enter, and Up/Down before `TextEdit` sees them so
    // `TextEdit` never drops focus on Enter/Escape while still processing text
    // insertion, Backspace, and cursor edits in this frame's `RawInput`.
    let (esc_pressed, enter_pressed, down_steps, up_steps) = ctx.input_mut(|i| {
        let esc = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        let enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        let mut down = 0usize;
        while i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            down += 1;
        }
        let mut up = 0usize;
        while i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            up += 1;
        }
        (esc, enter, down, up)
    });

    let viewport = safe_viewport_rect(ctx);
    let box_width = resize::float_width(
        &state.resize,
        &input.size,
        PALETTE_LIMITS.float_default_width as f32,
        viewport,
    )
    .max(140.0);
    let box_left = viewport.left() + ((viewport.width() - box_width) * 0.5).max(0.0);
    // The box's 1 px border is outside what it holds: the size is the box
    // with it.
    let box_width = box_width - 2.0 * resize::FLOAT_RIM_PX;
    let box_top = viewport.top() + (viewport.height() * 0.14).clamp(6.0, viewport.height() * 0.5);
    let list_max_h = (viewport.height() * 0.60).max(56.0);
    // The height the box is held to once one is set (or its bottom is being
    // dragged): the list then fills what the input leaves of it.
    let held_height = resize::float_height(&state.resize, &input.size, viewport)
        .map(|h| h.min((viewport.bottom() - box_top - 8.0).max(96.0)));

    // Full-viewport scrim (`Palette.jsx:239`).
    let mut scrim_clicked = false;
    egui::Area::new(egui::Id::new(SCRIM_ID_SALT))
        .order(egui::Order::Middle)
        .fixed_pos(viewport.min)
        .fade_in(false)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(viewport.size(), egui::Sense::click());
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::ZERO,
                egui::Color32::from_rgba_unmultiplied(4, 6, 10, 120),
            );
            if resp.clicked() {
                scrim_clicked = true;
            }
        });

    let input_id = egui::Id::new(INPUT_ID_SALT);
    if !ctx.memory(|m| m.has_focus(input_id)) {
        ctx.memory_mut(|m| m.request_focus(input_id));
    }

    let pointer_now = ctx.input(|i| i.pointer.hover_pos());
    let pointer_moved = pointer_now.is_some() && pointer_now != state.last_pointer_pos;
    state.last_pointer_pos = pointer_now;

    let mut chosen_row: Option<usize> = None;
    let mut modal_rect = egui::Rect::NOTHING;

    let area_resp = egui::Area::new(egui::Id::new(MODAL_ID_SALT))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(box_left, box_top))
        .fade_in(false)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                ui.ctx().request_discard("wall_palette initial sizing");
            }
            let frame = egui::Frame::NONE
                .fill(egui::Color32::from_rgb(0x0e, 0x11, 0x18))
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(0x2a, 0x30, 0x3c),
                ))
                .corner_radius(egui::CornerRadius::same(8))
                .shadow(egui::Shadow {
                    offset: [0, 12],
                    blur: 32,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(150),
                });

            let framed = frame.show(ui, |ui| {
                ui.set_width(box_width);
                ui.set_max_width(box_width);
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                // Optional mode caption (`Palette.jsx:203-204`).
                if let Some(caption) = state.palette.caption(&input.snapshot) {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: 12,
                            right: 12,
                            top: 8,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(caption)
                                    .monospace()
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(0x6b, 0x73, 0x83)),
                            );
                        });
                }

                // Query input box (`Palette.jsx:205-214`).
                let placeholder = state.palette.placeholder(&input.snapshot);
                let mut query_buf = state.palette.query.clone();
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(0x0b, 0x0d, 0x13))
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 10,
                        bottom: 10,
                    })
                    .show(ui, |ui| {
                        ui.set_width(box_width - 24.0);
                        let edit = egui::TextEdit::singleline(&mut query_buf)
                            .id(input_id)
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .font(egui::FontId::monospace(14.0))
                            .text_color(egui::Color32::from_rgb(0xe6, 0xe9, 0xef))
                            .hint_text(
                                egui::RichText::new(placeholder)
                                    .monospace()
                                    .size(14.0)
                                    .color(egui::Color32::from_rgb(0x6b, 0x73, 0x83)),
                            );
                        ui.add(edit);
                    });
                if query_buf != state.palette.query {
                    state.palette.set_query(query_buf);
                }

                // Bottom border under input (`1px solid #2a303c`).
                let (sep_rect, _) =
                    ui.allocate_exact_size(egui::vec2(box_width, 1.0), egui::Sense::hover());
                ui.painter().rect_filled(
                    sep_rect,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_rgb(0x2a, 0x30, 0x3c),
                );

                // Apply keyboard navigation after `TextEdit` updated `query`,
                // so the rendered row highlight reflects this frame's arrows.
                if state.palette.mode == PaletteMode::Find {
                    let count_now = state.palette.items(&input.snapshot).len();
                    for _ in 0..down_steps {
                        state.palette.move_down(count_now);
                    }
                    for _ in 0..up_steps {
                        state.palette.move_up();
                    }
                }

                if state.palette.mode == PaletteMode::Find {
                    let items = state.palette.items(&input.snapshot);
                    state.item_count = items.len();
                    state.palette.selected = state
                        .palette
                        .selected
                        .min(items.len().saturating_sub(1));

                    let list_h = held_height.map(|h| {
                        (box_top + h - resize::FLOAT_RIM_PX - ui.cursor().top()).max(40.0)
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("wall_palette_list")
                        .max_height(list_h.unwrap_or(list_max_h))
                        .auto_shrink([false, list_h.is_none()])
                        .show(ui, |ui| {
                            ui.set_width(box_width);
                            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                            if items.is_empty() {
                                let (row_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(box_width, 28.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().text(
                                    egui::pos2(row_rect.left() + 12.0, row_rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "no match",
                                    egui::FontId::monospace(12.5),
                                    egui::Color32::from_rgb(0x6b, 0x73, 0x83),
                                );
                                return;
                            }

                            for (idx, item) in items.iter().enumerate() {
                                let row_h = 28.0;
                                let (row_rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(box_width, row_h),
                                    egui::Sense::click(),
                                );
                                if pointer_moved && resp.hovered() {
                                    state.palette.selected = idx;
                                }
                                if resp.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if resp.clicked() {
                                    chosen_row = Some(idx);
                                }

                                if ui.is_rect_visible(row_rect) {
                                    if idx == state.palette.selected {
                                        ui.painter().rect_filled(
                                            row_rect,
                                            egui::CornerRadius::ZERO,
                                            egui::Color32::from_rgb(0x1b, 0x21, 0x30),
                                        );
                                    }

                                    let pad_left = 12.0 + f32::from(item.depth) * 14.0;
                                    let fg = parse_hex_color(item.label_color())
                                        .unwrap_or(egui::Color32::from_rgb(0xe6, 0xe9, 0xef));
                                    let has_prefix = matches!(
                                        item.kind,
                                        PaletteItemKind::Group { .. }
                                            | PaletteItemKind::Agent { .. }
                                    );
                                    let mut label_x = row_rect.left() + pad_left;
                                    if has_prefix {
                                        let icon_rect = egui::Rect::from_min_size(
                                            egui::pos2(label_x, row_rect.center().y - 6.0),
                                            egui::vec2(12.0, 12.0),
                                        );
                                        paint_item_prefix(ui.painter(), icon_rect, item, fg);
                                        label_x += 15.0;
                                    }

                                    // Measure detail on the right (`maxWidth: 55%`) with `…` truncation.
                                    let max_detail_w = box_width * 0.52;
                                    let mut detail_left = row_rect.right() - 12.0;
                                    if !item.detail.is_empty() {
                                        let galley = layout_elided(
                                            ui.painter(),
                                            &item.detail,
                                            egui::FontId::monospace(11.5),
                                            egui::Color32::from_rgb(0x6b, 0x73, 0x83),
                                            max_detail_w,
                                        );
                                        detail_left = row_rect.right() - 12.0 - galley.size().x;
                                        ui.painter().galley(
                                            egui::pos2(
                                                detail_left,
                                                row_rect.center().y - galley.size().y * 0.5,
                                            ),
                                            galley,
                                            egui::Color32::from_rgb(0x6b, 0x73, 0x83),
                                        );
                                    }

                                    let label_max_w =
                                        (detail_left - 12.0 - label_x).max(24.0);
                                    let label_galley = layout_elided(
                                        ui.painter(),
                                        &item.label,
                                        egui::FontId::monospace(12.5),
                                        fg,
                                        label_max_w,
                                    );
                                    ui.painter().galley(
                                        egui::pos2(
                                            label_x,
                                            row_rect.center().y - label_galley.size().y * 0.5,
                                        ),
                                        label_galley,
                                        fg,
                                    );
                                }
                            }
                        });
                }
            });

            // The edges that resize it: the sides its width (it stays
            // centred), the bottom its height.
            resize::grips(
                ui,
                &mut state.resize,
                &ResizeSpec {
                    verb: "wall.palette",
                    side: DockSide::Float,
                    rect: framed.response.rect,
                    limits: PALETTE_LIMITS,
                    viewport,
                    centered: true,
                },
                out,
            );
        });
    modal_rect = modal_rect.union(area_resp.response.rect);
    state.last_rect = Some(modal_rect);

    // Handle Escape (`P-panels.md`: Escape clears the search then closes).
    if esc_pressed {
        if state.palette.mode == PaletteMode::Find && !state.palette.query.is_empty() {
            state.palette.set_query("");
        } else {
            state.close();
        }
        return;
    }

    if let Some(idx) = chosen_row {
        let effect = state.palette.choose_at(&input.snapshot, idx);
        dispatch_effect(state, effect, out);
        return;
    }

    if enter_pressed {
        let effect = state.palette.submit(&input.snapshot);
        dispatch_effect(state, effect, out);
        return;
    }

    if scrim_clicked {
        let click_pos = ctx.input(|i| i.pointer.interact_pos());
        if !click_pos.is_some_and(|p| modal_rect.contains(p)) {
            state.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::config::GroupLayout;
    use wall_core::input::Platform;
    use wall_core::palette::{PaletteGroup, PaletteTerminal};
    use wall_core::sidebar::{BusAgentInput, BusSnapshot};

    fn sample_input() -> PaletteInput {
        PaletteInput::from(PaletteSnapshot {
            terminals: vec![
                PaletteTerminal {
                    id: "t1".to_string(),
                    name: "api-server".to_string(),
                    named: true,
                    group: Some("g1".to_string()),
                    hidden: false,
                    preview: "Listening on 3000".to_string(),
                    attached: None,
                },
                PaletteTerminal {
                    id: "t2".to_string(),
                    name: "bg-worker".to_string(),
                    named: false,
                    group: None,
                    hidden: true,
                    preview: "sleeping".to_string(),
                    attached: None,
                },
            ],
            groups: vec![PaletteGroup {
                id: "g1".to_string(),
                name: "Backend".to_string(),
                named: true,
                layout: GroupLayout::Tile,
                columns: 2,
                members: vec!["t1".to_string()],
            }],
            focused_id: Some("t1".to_string()),
            selection: Vec::new(),
            sidebar_open: false,
            settings_open: false,
            snap: true,
            grid_show: false,
            platform: Platform::Mac,
            relay_port: None,
            bus: BusSnapshot {
                host: "mac".to_string(),
                reason: String::new(),
                agents: vec![BusAgentInput {
                    name: "lead".to_string(),
                    host: "mac".to_string(),
                    live: true,
                    status: "waiting".to_string(),
                    attached: Some(true),
                    detail: "idle".to_string(),
                    ..BusAgentInput::default()
                }],
            },
        })
    }

    fn raw_with_events(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..egui::RawInput::default()
        }
    }

    fn key_press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn palette_lays_out_across_viewport_sizes_including_empty_and_no_match() {
        let ctx = egui::Context::default();
        let mut state = PaletteState::default();
        state.open_find();
        let empty_input = PaletteInput::default();
        let mut out = Vec::new();

        for size in [
            egui::vec2(220.0, 160.0),
            egui::vec2(800.0, 600.0),
            egui::vec2(1920.0, 1080.0),
        ] {
            let full = crate::wall::ui::headless_pass(&ctx, raw_with_events(size, Vec::new()), |ui| {
                show(ui.ctx(), &mut state, &empty_input, &mut out);
            });
            assert!(state.wants_keyboard());
            assert!(
                !full.shapes.is_empty(),
                "open palette must draw shapes at {size:?}"
            );
        }

        // Unmatched query renders the "no match" row rather than nothing.
        state.palette.set_query("zzzz-nonexistent-query");
        let full = crate::wall::ui::headless_pass(&ctx, raw_with_events(egui::vec2(800.0, 600.0), Vec::new()), |ui| {
            show(ui.ctx(), &mut state, &empty_input, &mut out);
        });
        assert!(!full.shapes.is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn typing_query_arrows_and_enter_emit_expected_verb_and_escape_clears_then_closes() {
        let ctx = egui::Context::default();
        let input = sample_input();
        let mut state = PaletteState::default();
        state.open_find();
        let mut out = Vec::new();

        // Type "bg-worker" and press Enter in a single frame -> reveals hidden terminal t2 and closes!
        let _ = crate::wall::ui::headless_pass(&ctx, 
            raw_with_events(
                egui::vec2(1024.0, 768.0),
                vec![
                    egui::Event::Text("bg-worker".to_string()),
                    key_press(egui::Key::Enter),
                ],
            ),
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert_eq!(out, vec!["wall.reveal t2"]);
        assert!(!state.open);

        // Re-open: ArrowDown from index 0 ("Backend" group) to index 1 ("t1"), then Enter -> reveals t1.
        out.clear();
        state.open_find();
        let _ = crate::wall::ui::headless_pass(&ctx, 
            raw_with_events(
                egui::vec2(1024.0, 768.0),
                vec![key_press(egui::Key::ArrowDown), key_press(egui::Key::Enter)],
            ),
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert_eq!(out, vec!["wall.reveal t1"]);
        assert!(!state.open);

        // Re-open, type a filter, press Escape -> clears filter first, keeps palette open;
        // second Escape closes the palette!
        out.clear();
        state.open_find();
        let _ = crate::wall::ui::headless_pass(&ctx, 
            raw_with_events(
                egui::vec2(1024.0, 768.0),
                vec![egui::Event::Text("lead".to_string())],
            ),
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert_eq!(state.palette.query, "lead");
        assert!(state.open);

        let _ = crate::wall::ui::headless_pass(&ctx, 
            raw_with_events(egui::vec2(1024.0, 768.0), vec![key_press(egui::Key::Escape)]),
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert_eq!(state.palette.query, "");
        assert!(state.open, "first Escape clears non-empty search");

        let _ = crate::wall::ui::headless_pass(&ctx, 
            raw_with_events(egui::vec2(1024.0, 768.0), vec![key_press(egui::Key::Escape)]),
            |ui| show(ui.ctx(), &mut state, &input, &mut out),
        );
        assert!(!state.open, "second Escape closes palette");
        assert!(out.is_empty());
    }

    fn click_at(
        ctx: &egui::Context,
        vp: egui::Vec2,
        pos: egui::Pos2,
        state: &mut PaletteState,
        input: &PaletteInput,
        out: &mut Vec<String>,
    ) {
        let _ = crate::wall::ui::headless_pass(ctx, 
            raw_with_events(
                vp,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
            |ui| show(ui.ctx(), state, input, out),
        );
        let _ = crate::wall::ui::headless_pass(ctx, 
            raw_with_events(
                vp,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            ),
            |ui| show(ui.ctx(), state, input, out),
        );
    }

    #[test]
    fn simulated_pointer_click_on_row_emits_verb_and_scrim_click_closes() {
        let ctx = egui::Context::default();
        let input = sample_input();
        let mut state = PaletteState::default();
        state.open_find();
        let mut out = Vec::new();
        let vp = egui::vec2(1000.0, 700.0);

        // Two warm passes settle Area's first-frame sizing pass and populate WidgetRects.
        for _ in 0..2 {
            let _ = crate::wall::ui::headless_pass(&ctx, raw_with_events(vp, Vec::new()), |ui| {
                show(ui.ctx(), &mut state, &input, &mut out);
            });
        }

        // Box top = 700 * 0.14 = 98; input height = ~40 + 1px border => row 0 sits around y = 152, x = 500.
        click_at(&ctx, vp, egui::pos2(500.0, 152.0), &mut state, &input, &mut out);
        assert_eq!(out, vec!["wall.group.frame g1"]);
        assert!(!state.open);

        // Re-open and click outside the modal on the scrim (top-left 10, 10) -> closes with no verb.
        out.clear();
        state.open_find();
        for _ in 0..2 {
            let _ = crate::wall::ui::headless_pass(&ctx, raw_with_events(vp, Vec::new()), |ui| {
                show(ui.ctx(), &mut state, &input, &mut out);
            });
        }
        click_at(&ctx, vp, egui::pos2(10.0, 10.0), &mut state, &input, &mut out);
        assert!(!state.open);
        assert!(out.is_empty());
    }
}
