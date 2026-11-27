//! Port of `wall/ContextMenu.jsx` (+ `wall/holdPress.js`) onto egui 0.36,
//! backed by [`wall_core::menu`].
//!
//! Renders the wall's right-click / menu-key popup: a quiet target caption,
//! verb-backed rows (`Action`, `Hold` close with circular progress ring over
//! `close_hold_ms`, inline `Rename` field, `Choice` chips, and inline
//! `cols × rows` custom terminal size fields validated by
//! [`wall_core::menu::check_size`]), optional `"right-click again: browser menu"`
//! footer, viewport-edge flipping via [`wall_core::menu::place_menu`], and full
//! keyboard navigation (`↑`/`↓`/`Home`/`End`, `←`/`→` on choices, `Enter`/`Space`
//! including held close, `Tab` between size fields, and `Escape`).

use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Duration;

use wall_core::geom::Point;
use wall_core::input::Platform;
use wall_core::menu::{
    ChoiceOption, MenuConfig, MenuDimensions, MenuItem, MenuState, MenuTarget, SizeEdit,
    check_size, menu_items, place_menu,
};

/// Default hold-to-close duration in milliseconds (`closeHoldMs` in `wall/wallConfig.js`).
pub const DEFAULT_CLOSE_HOLD_MS: f64 = 650.0;

const MENU_MIN_WIDTH: f32 = 196.0;
const MENU_MAX_WIDTH: f32 = 360.0;
const ROW_MIN_HEIGHT: f32 = 22.0;
const MARK_BOX_SIZE: f32 = 14.0;

const BOX_BG: egui::Color32 = egui::Color32::from_rgb(0x0c, 0x0e, 0x14);
const BORDER_COLOR: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x30, 0x3c);
const SEP_COLOR: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x24, 0x30);
const TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(0xd4, 0xd9, 0xe2);
const CAPTION_COLOR: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x73, 0x83);
const ACTIVE_ROW_BG: egui::Color32 = egui::Color32::from_rgb(0x1b, 0x21, 0x30);
const MARK_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x95, 0xa7);
const DETAIL_COLOR: egui::Color32 = egui::Color32::from_rgb(0x5d, 0x65, 0x74);
const CHIP_TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x95, 0xa7);
const CHIP_ON_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xe9, 0xef);
const CHIP_ON_BG: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x23, 0x30);
const CHIP_ACTIVE_BORDER: egui::Color32 = egui::Color32::from_rgb(0x3b, 0x43, 0x52);
const FIELD_BG: egui::Color32 = egui::Color32::from_rgb(0x0b, 0x0d, 0x13);
const FIELD_FOCUS_BORDER: egui::Color32 = egui::Color32::from_rgb(0x4b, 0x56, 0x6b);
const REFUSED_COLOR: egui::Color32 = egui::Color32::from_rgb(0xe5, 0x53, 0x4b);

/// Compute the menu's header caption for `target` (`captionOf` in `wall/ContextMenu.jsx`).
pub fn caption_of(target: &MenuTarget, state: &MenuState) -> String {
    match target {
        MenuTarget::Terminal { id, .. } => state
            .terminals
            .iter()
            .find(|t| &t.id == id)
            .map_or_else(|| id.clone(), |t| t.name.clone()),
        MenuTarget::Group { id } => state
            .groups
            .iter()
            .find(|g| &g.id == id)
            .map_or_else(|| id.clone(), |g| g.name.clone()),
        MenuTarget::Canvas { .. } => "canvas".to_owned(),
    }
}

/// Format a tokenized wall verb invocation into a single router command line,
/// quoting arguments that hold whitespace, quotes, or backslashes so
/// `wall_core::router::CommandRouter` tokenizes them back losslessly.
pub fn format_verb(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| quote_verb_token(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_verb_token(token: &str) -> String {
    let needs_quotes = token.is_empty()
        || token
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'));
    if !needs_quotes {
        return token.to_owned();
    }
    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');
    for ch in token.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Data for an open context menu instance.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenContextMenu {
    pub target: MenuTarget,
    /// Anchor position in logical/CSS canvas pixels (`menu.at` in JS).
    pub at: Point,
    /// Quiet top caption naming what the menu acts on (`captionOf(target, state)`).
    pub caption: String,
    /// Verb-backed rows built by [`wall_core::menu::menu_items`].
    pub items: Vec<MenuItem>,
    /// Optional host-reported refusal from the last `terminal.resize` attempt.
    pub size_error: Option<String>,
}

impl OpenContextMenu {
    /// Build an [`OpenContextMenu`] directly from [`wall_core::menu`] snapshot models.
    pub fn from_wall(
        target: MenuTarget,
        at: Point,
        state: &MenuState,
        config: &MenuConfig,
        platform: Platform,
    ) -> Self {
        let caption = caption_of(&target, state);
        let items = menu_items(&target, state, config, platform);
        Self {
            target,
            at,
            caption,
            items,
            size_error: None,
        }
    }
}

/// Read-only per-frame input to [`show`].
#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuInput {
    /// `Some` while the wall's context menu is open, `None` when closed.
    pub open: Option<OpenContextMenu>,
    /// Optional explicit canvas viewport dimensions for [`place_menu`]; falls back
    /// to egui's viewport rect when `None`.
    pub viewport: Option<MenuDimensions>,
    /// Hold time in milliseconds required to trigger a [`MenuItem::Hold`] close row.
    pub close_hold_ms: f64,
    /// Whether to display the `"right-click again: browser menu"` footer (`menuHint`).
    pub menu_hint: bool,
    /// Platform modifier conventions (for secondary-press detection on outside clicks).
    pub platform: Platform,
}

impl Default for ContextMenuInput {
    fn default() -> Self {
        Self {
            open: None,
            viewport: None,
            close_hold_ms: DEFAULT_CLOSE_HOLD_MS,
            menu_hint: false,
            platform: current_platform(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeField {
    Cols,
    Rows,
}

impl SizeField {
    const fn other(self) -> Self {
        match self {
            Self::Cols => Self::Rows,
            Self::Rows => Self::Cols,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenameEdit {
    item_id: String,
    text: String,
    request_focus: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SizingEdit {
    item_id: String,
    cols: String,
    rows: String,
    error: String,
    focused_field: SizeField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoldSource {
    Pointer,
    Key(egui::Key),
}

#[derive(Clone, Debug, PartialEq)]
struct HoldProgress {
    item_id: String,
    started_at_secs: f64,
    source: HoldSource,
}

#[derive(Clone, Debug, PartialEq)]
struct MenuInstanceKey {
    target: MenuTarget,
    at_bits: (u64, u64),
}

impl MenuInstanceKey {
    fn of(menu: &OpenContextMenu) -> Self {
        Self {
            target: menu.target.clone(),
            at_bits: (menu.at.x.to_bits(), menu.at.y.to_bits()),
        }
    }
}

/// Persistent view state for the right-click context menu between frames.
#[derive(Clone, Debug, Default)]
pub struct ContextMenuState {
    instance: Option<MenuInstanceKey>,
    /// Index into the non-separator rows (`None` until pointer movement or arrow key).
    active_row: Option<usize>,
    /// Highlighted option index when `active_row` is a [`MenuItem::Choice`].
    active_option: usize,
    renaming: Option<RenameEdit>,
    sizing: Option<SizingEdit>,
    hold: Option<HoldProgress>,
    last_inline_key_frame: Option<u64>,
    measured_size: Option<egui::Vec2>,
    last_rect: Option<egui::Rect>,
    last_pointer_pos: Option<egui::Pos2>,
    closed_empty: bool,
    /// Last rendered item row rectangles keyed by menu item id (`split-right`, `close`, `spawn`, etc.).
    pub item_rects: std::collections::BTreeMap<String, egui::Rect>,
}

impl ContextMenuState {
    /// Reset all transient menu view state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Last rendered screen rectangle of the menu popup, if open (`[data-wall-menu]`).
    pub const fn last_rect(&self) -> Option<egui::Rect> {
        self.last_rect
    }

    /// Whether the inline `cols × rows` size fields are currently active.
    pub const fn is_sizing(&self) -> bool {
        self.sizing.is_some()
    }

    /// Whether the inline rename text box is currently active.
    pub const fn is_renaming(&self) -> bool {
        self.renaming.is_some()
    }
}

fn emit_close(out: &mut Vec<String>) {
    let close_line = "wall.menu close";
    if out.last().map(String::as_str) != Some(close_line) {
        out.push(close_line.to_owned());
    }
}

fn emit_verb(out: &mut Vec<String>, parts: &[String], stay_open: bool) {
    if !parts.is_empty() {
        out.push(format_verb(parts));
    }
    if !stay_open {
        emit_close(out);
    }
}

fn default_option_index(item: &MenuItem) -> usize {
    match item {
        MenuItem::Choice { options, .. } => options.iter().position(|o| o.on).unwrap_or(0),
        _ => 0,
    }
}

fn set_active_row(state: &mut ContextMenuState, non_sep_items: &[&MenuItem], row_idx: usize) {
    let changed = state.active_row != Some(row_idx);
    state.active_row = Some(row_idx);
    if changed {
        if let Some(item) = non_sep_items.get(row_idx) {
            state.active_option = default_option_index(item);
        }
    }
}

fn estimate_menu_size(menu: &OpenContextMenu, _menu_hint: bool) -> egui::Vec2 {
    let row_h = menu
        .items
        .iter()
        .map(|it| if it.is_sep() { 9.0 } else { ROW_MIN_HEIGHT + 2.0 })
        .sum::<f32>()
        .max(ROW_MIN_HEIGHT);
    egui::vec2(236.0, 24.0 + row_h)
}

/// Draw the context menu into `ctx` and append any triggered wall verb lines to `out`.
///
/// Returns `true` while the menu is open so the caller routes keyboard events to
/// egui rather than to the focused terminal.
pub fn show(
    ctx: &egui::Context,
    state: &mut ContextMenuState,
    input: &ContextMenuInput,
    out: &mut Vec<String>,
) -> bool {
    let Some(menu) = &input.open else {
        if state.instance.is_some() || state.last_rect.is_some() {
            state.reset();
        }
        return false;
    };

    let key = MenuInstanceKey::of(menu);
    let current_hover = ctx.input(|i| i.pointer.hover_pos().or_else(|| i.pointer.interact_pos()));
    let just_opened = state.instance.as_ref() != Some(&key);
    if just_opened {
        *state = ContextMenuState {
            instance: Some(key),
            last_pointer_pos: current_hover,
            ..ContextMenuState::default()
        };
    }
    state.item_rects.clear();

    let pointer_moved = match (current_hover, state.last_pointer_pos) {
        (Some(now), Some(prev)) => (now - prev).length_sq() > 0.01,
        (Some(_), None) => ctx.input(|i| i.pointer.delta().length_sq() > 0.0),
        _ => false,
    };
    if current_hover.is_some() {
        state.last_pointer_pos = current_hover;
    }

    let screen_rect = ctx.content_rect();
    let view_dims = input.viewport.unwrap_or_else(|| {
        MenuDimensions::new(
            f64::from(screen_rect.width().max(1.0)),
            f64::from(screen_rect.height().max(1.0)),
        )
    });
    let box_size = state
        .measured_size
        .unwrap_or_else(|| estimate_menu_size(menu, input.menu_hint));
    let placed = place_menu(
        menu.at,
        MenuDimensions::new(f64::from(box_size.x), f64::from(box_size.y)),
        view_dims,
    );
    let top_left = egui::pos2(placed.x as f32, placed.y as f32);

    // Empty menu items: display a quiet empty row instead of nothing AND close cleanly.
    if menu.items.is_empty() {
        let area_resp = menu_area(ctx, top_left).show(ctx, |ui| {
            menu_frame().show(ui, |ui| {
                ui.set_min_width(MENU_MIN_WIDTH);
                ui.set_max_width(MENU_MAX_WIDTH);
                draw_caption(ui, &menu.caption);
                draw_empty_message(ui, "no actions");
            });
        });
        state.last_rect = Some(area_resp.response.rect);
        state.measured_size = Some(area_resp.response.rect.size());
        if !state.closed_empty {
            state.closed_empty = true;
            emit_close(out);
        }
        return true;
    }

    let non_sep: Vec<&MenuItem> = menu.items.iter().filter(|it| !it.is_sep()).collect();
    if let Some(idx) = state.active_row {
        if idx >= non_sep.len() {
            state.active_row = Some(non_sep.len() - 1);
        }
    }
    // Drop rename/sizing state if a snapshot update removed the backing item.
    if let Some(ren) = &state.renaming {
        if !non_sep
            .iter()
            .any(|it| matches!(it, MenuItem::Rename { id, .. } if id == &ren.item_id))
        {
            state.renaming = None;
        }
    }
    if let Some(sz) = &state.sizing {
        if !non_sep.iter().any(
            |it| matches!(it, MenuItem::Choice { id, edit: Some(_), .. } if id == &sz.item_id),
        ) {
            state.sizing = None;
        }
    }

    let now_secs = ctx.input(|i| i.time);
    let hold_secs = (input.close_hold_ms.max(0.0)) / 1000.0;

    // Intercept special keys once per frame from `raw.events` so `Focus::begin_pass`
    // consuming `Escape`/`Tab` on an already-focused `TextEdit` cannot drop them.
    let mut inline_enter = false;
    let mut inline_escape = false;
    let mut inline_tab = false;
    if state.renaming.is_some() || state.sizing.is_some() {
        let frame_nr = ctx.cumulative_frame_nr();
        if state.last_inline_key_frame != Some(frame_nr) {
            state.last_inline_key_frame = Some(frame_nr);
            ctx.input_mut(|i| {
                inline_escape = has_raw_key_press(i, egui::Key::Escape);
                inline_enter = has_raw_key_press(i, egui::Key::Enter);
                if state.sizing.is_some() {
                    inline_tab = has_raw_key_press(i, egui::Key::Tab);
                }
                let _ = take_key_press(i, egui::Key::Escape);
                let _ = take_key_press(i, egui::Key::Enter);
                if state.sizing.is_some() {
                    let _ = take_key_press(i, egui::Key::Tab);
                }
            });
        }
    } else {
        handle_menu_navigation_keys(ctx, state, &non_sep, now_secs, hold_secs, out);
    }

    // Update any keyboard-driven hold cancellation on KeyUp.
    if let Some(hold) = &state.hold {
        if let HoldSource::Key(held_key) = hold.source {
            let released = ctx.input(|i| {
                !i.key_down(held_key)
                    || i.events.iter().any(|ev| {
                        matches!(
                            ev,
                            egui::Event::Key {
                                key,
                                pressed: false,
                                ..
                            } if *key == held_key
                        )
                    })
            });
            let active_matches = state
                .active_row
                .and_then(|idx| non_sep.get(idx))
                .is_some_and(|it| matches!(it, MenuItem::Hold { id, .. } if id == &hold.item_id));
            if released || !active_matches {
                state.hold = None;
            }
        }
    }

    let area_resp = menu_area(ctx, top_left).show(ctx, |ui| {
        if ui.is_sizing_pass() {
            ui.ctx().request_discard("wall_context_menu initial sizing");
        }
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        menu_frame().show(ui, |ui| {
            ui.set_min_width(MENU_MIN_WIDTH);
            ui.set_max_width(MENU_MAX_WIDTH);
            draw_caption(ui, &menu.caption);

            let mut row_cursor = 0usize;
            for item in &menu.items {
                match item {
                    MenuItem::Sep { .. } => draw_separator(ui),
                    MenuItem::Action {
                        id,
                        label,
                        verb,
                        detail,
                    } => {
                        let idx = row_cursor;
                        row_cursor += 1;
                        let is_active = state.active_row == Some(idx);
                        let resp = draw_plain_row(ui, label, detail.as_deref(), None, is_active);
                        state.item_rects.insert(id.clone(), resp.rect);
                        if pointer_moved && resp.hovered() {
                            set_active_row(state, &non_sep, idx);
                        }
                        if resp.clicked() {
                            set_active_row(state, &non_sep, idx);
                            emit_verb(out, verb, false);
                        }
                    }
                    MenuItem::Rename {
                        id,
                        label,
                        verb,
                        value,
                        placeholder,
                    } => {
                        let idx = row_cursor;
                        row_cursor += 1;
                        let is_editing = state
                            .renaming
                            .as_ref()
                            .is_some_and(|ren| &ren.item_id == id);
                        if is_editing {
                            draw_rename_editor_row(
                                ui,
                                state,
                                id,
                                verb,
                                placeholder,
                                inline_enter,
                                inline_escape,
                                out,
                            );
                        } else {
                            let is_active = state.active_row == Some(idx);
                            let resp = draw_plain_row(ui, label, None, None, is_active);
                            if pointer_moved && resp.hovered() {
                                set_active_row(state, &non_sep, idx);
                            }
                            if resp.clicked() {
                                set_active_row(state, &non_sep, idx);
                                state.renaming = Some(RenameEdit {
                                    item_id: id.clone(),
                                    text: value.clone(),
                                    request_focus: true,
                                });
                            }
                        }
                    }
                    MenuItem::Choice {
                        id,
                        label,
                        detail,
                        options,
                        edit,
                    } => {
                        let idx = row_cursor;
                        row_cursor += 1;
                        let is_sizing = state
                            .sizing
                            .as_ref()
                            .is_some_and(|sz| &sz.item_id == id)
                            && edit.is_some();
                        if let (true, Some(size_edit)) = (is_sizing, edit.as_ref()) {
                            draw_sizing_editor_row(
                                ui,
                                state,
                                id,
                                label,
                                size_edit,
                                menu.size_error.as_deref(),
                                inline_tab,
                                inline_enter,
                                inline_escape,
                                out,
                            );
                        } else {
                            draw_choice_row(
                                ui,
                                state,
                                &non_sep,
                                idx,
                                id,
                                label,
                                detail.as_deref(),
                                options,
                                edit.as_ref(),
                                pointer_moved,
                                out,
                            );
                        }
                    }
                    MenuItem::Hold {
                        id,
                        label,
                        verb,
                        detail,
                    } => {
                        let idx = row_cursor;
                        row_cursor += 1;
                        draw_hold_row(
                            ui,
                            state,
                            &non_sep,
                            idx,
                            id,
                            label,
                            detail.as_deref(),
                            verb,
                            pointer_moved,
                            now_secs,
                            hold_secs,
                            out,
                        );
                    }
                }
            }
        });
    });

    let rect = area_resp.response.rect;
    let measured = rect.size();
    if state
        .measured_size
        .is_none_or(|prev| (prev - measured).length_sq() > 0.25)
    {
        state.measured_size = Some(measured);
        ctx.request_repaint();
    }
    state.last_rect = Some(rect);

    // Dismiss on any press outside the menu box (ignoring the opening secondary press itself).
    let clicked_outside = !just_opened
        && ctx.input(|i| {
            i.events.iter().any(|ev| {
                matches!(
                    ev,
                    egui::Event::PointerButton {
                        pos,
                        pressed: true,
                        ..
                } if !rect.contains(*pos)
            )
        })
    });
    if clicked_outside {
        emit_close(out);
    }

    true
}

fn has_raw_key_press(input: &egui::InputState, target: egui::Key) -> bool {
    input.raw.events.iter().any(|ev| {
        matches!(
            ev,
            egui::Event::Key {
                key,
                pressed: true,
                ..
            } if *key == target
        )
    })
}

fn take_key_press(input: &mut egui::InputState, target: egui::Key) -> bool {
    let mut matched = false;
    input.events.retain(|ev| match ev {
        egui::Event::Key {
            key,
            pressed: true,
            ..
        } if *key == target => {
            matched = true;
            false
        }
        _ => true,
    });
    matched
}

fn handle_menu_navigation_keys(
    ctx: &egui::Context,
    state: &mut ContextMenuState,
    non_sep: &[&MenuItem],
    now_secs: f64,
    hold_secs: f64,
    out: &mut Vec<String>,
) {
    let count = non_sep.len();
    if count == 0 {
        return;
    }

    #[derive(Clone, Copy)]
    enum NavCmd {
        Escape,
        Down,
        Up,
        Home,
        End,
        Left,
        Right,
        Activate { key: egui::Key, repeat: bool },
        Tab,
    }

    let mut commands = Vec::new();
    ctx.input_mut(|i| {
        i.events.retain(|ev| {
            let egui::Event::Key {
                key,
                pressed: true,
                repeat,
                ..
            } = ev
            else {
                return true;
            };
            let nav = match key {
                egui::Key::Escape => Some(NavCmd::Escape),
                egui::Key::ArrowDown => Some(NavCmd::Down),
                egui::Key::ArrowUp => Some(NavCmd::Up),
                egui::Key::Home => Some(NavCmd::Home),
                egui::Key::End => Some(NavCmd::End),
                egui::Key::ArrowLeft => Some(NavCmd::Left),
                egui::Key::ArrowRight => Some(NavCmd::Right),
                egui::Key::Enter | egui::Key::Space => Some(NavCmd::Activate {
                    key: *key,
                    repeat: *repeat,
                }),
                egui::Key::Tab => Some(NavCmd::Tab),
                _ => None,
            };
            if let Some(cmd) = nav {
                commands.push(cmd);
                false
            } else {
                true
            }
        });
    });

    for cmd in commands {
        match cmd {
            NavCmd::Escape => {
                state.hold = None;
                emit_close(out);
            }
            NavCmd::Down => {
                let next = match state.active_row {
                    None => 0,
                    Some(cur) => (cur + 1) % count,
                };
                set_active_row(state, non_sep, next);
            }
            NavCmd::Up => {
                let prev = match state.active_row {
                    None => count - 1,
                    Some(cur) => (cur + count - 1) % count,
                };
                set_active_row(state, non_sep, prev);
            }
            NavCmd::Home => set_active_row(state, non_sep, 0),
            NavCmd::End => set_active_row(state, non_sep, count - 1),
            NavCmd::Left | NavCmd::Right => {
                if let Some(MenuItem::Choice { options, .. }) =
                    state.active_row.and_then(|idx| non_sep.get(idx)).copied()
                {
                    let opt_len = options.len();
                    if opt_len > 0 {
                        let cur = state.active_option.min(opt_len - 1);
                        state.active_option = match cmd {
                            NavCmd::Right => (cur + 1) % opt_len,
                            _ => (cur + opt_len - 1) % opt_len,
                        };
                    }
                }
            }
            NavCmd::Activate { key, repeat } => {
                let Some(active_idx) = state.active_row else {
                    continue;
                };
                let Some(&item) = non_sep.get(active_idx) else {
                    continue;
                };
                match item {
                    MenuItem::Hold { id, verb, .. } => {
                        if !repeat && state.hold.is_none() {
                            if hold_secs <= 0.0 {
                                emit_verb(out, verb, false);
                            } else {
                                state.hold = Some(HoldProgress {
                                    item_id: id.clone(),
                                    started_at_secs: now_secs,
                                    source: HoldSource::Key(key),
                                });
                            }
                        }
                    }
                    MenuItem::Action { verb, .. } => emit_verb(out, verb, false),
                    MenuItem::Rename { id, value, .. } => {
                        state.renaming = Some(RenameEdit {
                            item_id: id.clone(),
                            text: value.clone(),
                            request_focus: true,
                        });
                    }
                    MenuItem::Choice {
                        id, options, edit, ..
                    } => {
                        if let Some(chosen) =
                            options.get(state.active_option.min(options.len().saturating_sub(1)))
                        {
                            pick_choice_option(state, active_idx, id, chosen, edit.as_ref(), out);
                        }
                    }
                    MenuItem::Sep { .. } => {}
                }
            }
            NavCmd::Tab => {}
        }
    }
}

fn pick_choice_option(
    state: &mut ContextMenuState,
    row_idx: usize,
    item_id: &str,
    option: &ChoiceOption,
    edit: Option<&SizeEdit>,
    out: &mut Vec<String>,
) {
    if option.custom {
        if let Some(size_edit) = edit {
            state.active_row = Some(row_idx);
            state.sizing = Some(SizingEdit {
                item_id: item_id.to_owned(),
                cols: size_edit.cols.to_string(),
                rows: size_edit.rows.to_string(),
                error: String::new(),
                focused_field: SizeField::Cols,
            });
        }
    } else if let Some(verb) = &option.verb {
        emit_verb(out, verb, item_id == "columns");
    }
}

fn menu_area(ctx: &egui::Context, top_left: egui::Pos2) -> egui::Area {
    let _ = ctx;
    egui::Area::new(egui::Id::new("wall_context_menu"))
        .fixed_pos(top_left)
        .order(egui::Order::Tooltip)
        .constrain(false)
        .interactable(true)
}

fn menu_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BOX_BG)
        .stroke(egui::Stroke::new(1.0, BORDER_COLOR))
        .corner_radius(7.0)
        .inner_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 4,
            bottom: 4,
        })
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: egui::Color32::from_black_alpha(115),
        })
}

fn fit_font_size(text: &str, max_px: f32, min_px: f32, max_width: f32) -> f32 {
    let char_count = text.chars().count().max(1) as f32;
    let approx_em = 0.62 * char_count;
    (max_width / approx_em).clamp(min_px, max_px)
}

fn draw_caption(ui: &mut egui::Ui, caption: &str) {
    let font_px = fit_font_size(caption, 11.0, 8.5, MENU_MAX_WIDTH - 36.0);
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 26,
            right: 10,
            top: 2,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_min_width(MENU_MIN_WIDTH - 36.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(caption)
                        .font(egui::FontId::monospace(font_px))
                        .color(CAPTION_COLOR),
                )
                .truncate(),
            );
        });
}

fn draw_empty_message(ui: &mut egui::Ui, message: &str) {
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 26,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(message)
                    .font(egui::FontId::monospace(11.5))
                    .color(DETAIL_COLOR),
            );
        });
}

fn draw_separator(ui: &mut egui::Ui) {
    let width = ui.available_width().max(MENU_MIN_WIDTH);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 9.0), egui::Sense::hover());
    let y = rect.center().y;
    ui.painter()
        .hline(rect.x_range(), y, egui::Stroke::new(1.0, SEP_COLOR));
}

fn draw_plain_row(
    ui: &mut egui::Ui,
    label: &str,
    detail: Option<&str>,
    mark: Option<(&str, egui::Color32)>,
    is_active: bool,
) -> egui::Response {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 6,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width().max(MENU_MIN_WIDTH - 16.0));
            ui.set_min_height(ROW_MIN_HEIGHT - 6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (mark_rect, _) = ui.allocate_exact_size(
                    egui::vec2(MARK_BOX_SIZE, MARK_BOX_SIZE),
                    egui::Sense::hover(),
                );
                if let Some((glyph, color)) = mark {
                    ui.painter().text(
                        mark_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        glyph,
                        egui::FontId::monospace(12.0),
                        color,
                    );
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .font(egui::FontId::monospace(12.5))
                            .color(TEXT_COLOR),
                    )
                    .selectable(false),
                );
                if let Some(detail_text) = detail {
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(detail_text)
                                    .font(egui::FontId::monospace(10.5))
                                    .color(DETAIL_COLOR),
                            )
                            .selectable(false),
                        );
                    });
                }
            });
        });

    let full_rect = egui::Rect::from_min_max(
        egui::pos2(ui.min_rect().left(), inner.response.rect.top()),
        egui::pos2(
            inner.response.rect.right().max(ui.min_rect().left() + MENU_MIN_WIDTH),
            inner.response.rect.bottom(),
        ),
    );
    if is_active {
        ui.painter()
            .set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, ACTIVE_ROW_BG));
    }
    ui.interact(full_rect, ui.next_auto_id(), egui::Sense::click())
}

#[expect(
    clippy::too_many_arguments,
    reason = "inline rename row needs state, target item metadata, intercepted key flags, and output sink"
)]
fn draw_rename_editor_row(
    ui: &mut egui::Ui,
    state: &mut ContextMenuState,
    item_id: &str,
    verb: &[String],
    placeholder: &str,
    inline_enter: bool,
    inline_escape: bool,
    out: &mut Vec<String>,
) {
    let field_id = egui::Id::new(("wall_menu_rename", item_id));
    if let Some(ren) = &mut state.renaming {
        if ren.request_focus {
            ui.ctx().memory_mut(|m| m.request_focus(field_id));
            ren.request_focus = false;
        }
    }

    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 26,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(MENU_MIN_WIDTH - 36.0);
            if let Some(ren) = &mut state.renaming {
                egui::Frame::new()
                    .fill(FIELD_BG)
                    .stroke(egui::Stroke::new(1.0, FIELD_FOCUS_BORDER))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(6, 2))
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ren.text)
                                .id(field_id)
                                .frame(egui::Frame::NONE)
                                .hint_text(placeholder)
                                .font(egui::FontId::monospace(12.5))
                                .text_color(CHIP_ON_TEXT)
                                .desired_width((ui.available_width()).max(140.0)),
                        );
                    });
            }
        });
    ui.painter().set(
        bg_idx,
        egui::Shape::rect_filled(inner.response.rect, 0.0, ACTIVE_ROW_BG),
    );

    if inline_escape {
        state.renaming = None;
        emit_close(out);
    } else if inline_enter {
        if let Some(ren) = state.renaming.take() {
            let trimmed = ren.text.trim();
            let mut full_verb = verb.to_vec();
            if !trimmed.is_empty() {
                full_verb.push(trimmed.to_owned());
            }
            emit_verb(out, &full_verb, false);
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "choice row renders label, optional detail, option chips, hover tracking, and custom-size entry"
)]
fn draw_choice_row(
    ui: &mut egui::Ui,
    state: &mut ContextMenuState,
    non_sep: &[&MenuItem],
    row_idx: usize,
    item_id: &str,
    label: &str,
    detail: Option<&str>,
    options: &[ChoiceOption],
    edit: Option<&SizeEdit>,
    pointer_moved: bool,
    out: &mut Vec<String>,
) {
    let is_active = state.active_row == Some(row_idx);
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let mut clicked_chip = false;

    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 6,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width().max(MENU_MIN_WIDTH - 16.0));
            ui.set_min_height(ROW_MIN_HEIGHT - 6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.allocate_exact_size(
                    egui::vec2(MARK_BOX_SIZE, MARK_BOX_SIZE),
                    egui::Sense::hover(),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .font(egui::FontId::monospace(12.5))
                            .color(TEXT_COLOR),
                    )
                    .selectable(false),
                );
                if let Some(detail_text) = detail {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(detail_text)
                                .font(egui::FontId::monospace(10.5))
                                .color(DETAIL_COLOR),
                        )
                        .selectable(false),
                    );
                }
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (opt_idx, opt) in options.iter().enumerate().rev() {
                        let opt_highlighted =
                            is_active && state.active_option.min(options.len().saturating_sub(1)) == opt_idx;
                        let font_px = if item_id == "add-to" {
                            fit_font_size(&opt.label, 12.0, 9.0, 108.0)
                        } else {
                            12.0
                        };
                        let chip_fill = if opt.on {
                            CHIP_ON_BG
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let chip_stroke = if opt_highlighted {
                            egui::Stroke::new(1.0, CHIP_ACTIVE_BORDER)
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::TRANSPARENT)
                        };
                        let chip_text_color = if opt.on {
                            CHIP_ON_TEXT
                        } else {
                            CHIP_TEXT_COLOR
                        };
                        let chip_inner = egui::Frame::new()
                            .fill(chip_fill)
                            .stroke(chip_stroke)
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(6, 1))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&opt.label)
                                            .font(egui::FontId::monospace(font_px))
                                            .color(chip_text_color),
                                    )
                                    .selectable(false),
                                )
                            });
                        let chip_resp = ui.interact(
                            chip_inner.response.rect,
                            egui::Id::new(("wall_menu_chip", item_id, &opt.id)),
                            egui::Sense::click(),
                        );
                        if pointer_moved && chip_resp.hovered() {
                            set_active_row(state, non_sep, row_idx);
                            state.active_option = opt_idx;
                        }
                        if chip_resp.clicked() {
                            clicked_chip = true;
                            set_active_row(state, non_sep, row_idx);
                            state.active_option = opt_idx;
                            pick_choice_option(state, row_idx, item_id, opt, edit, out);
                        }
                    }
                });
            });
        });

    let row_rect = inner.response.rect;
    if state.active_row == Some(row_idx) {
        ui.painter()
            .set(bg_idx, egui::Shape::rect_filled(row_rect, 0.0, ACTIVE_ROW_BG));
    }
    let row_resp = ui.interact(
        row_rect,
        egui::Id::new(("wall_menu_choice_row", item_id)),
        egui::Sense::click(),
    );
    if pointer_moved && row_resp.hovered() {
        set_active_row(state, non_sep, row_idx);
    }
    if row_resp.clicked() && !clicked_chip && !options.is_empty() {
        set_active_row(state, non_sep, row_idx);
        let opt_idx = state.active_option.min(options.len() - 1);
        pick_choice_option(state, row_idx, item_id, &options[opt_idx], edit, out);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "custom cols x rows editor renders two fields, Tab/Enter/Esc routing, and check_size error text"
)]
fn draw_sizing_editor_row(
    ui: &mut egui::Ui,
    state: &mut ContextMenuState,
    item_id: &str,
    label: &str,
    size_edit: &SizeEdit,
    external_error: Option<&str>,
    inline_tab: bool,
    inline_enter: bool,
    inline_escape: bool,
    out: &mut Vec<String>,
) {
    let cols_id = egui::Id::new(("wall_menu_size_cols", item_id));
    let rows_id = egui::Id::new(("wall_menu_size_rows", item_id));

    if let Some(sz) = &mut state.sizing {
        if inline_tab {
            sz.focused_field = sz.focused_field.other();
        }
        let target_id = match sz.focused_field {
            SizeField::Cols => cols_id,
            SizeField::Rows => rows_id,
        };
        ui.ctx().memory_mut(|m| m.request_focus(target_id));
    }

    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 6,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width().max(MENU_MIN_WIDTH - 16.0));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.allocate_exact_size(
                    egui::vec2(MARK_BOX_SIZE, MARK_BOX_SIZE),
                    egui::Sense::hover(),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .font(egui::FontId::monospace(12.5))
                            .color(TEXT_COLOR),
                    )
                    .selectable(false),
                );
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if let Some(sz) = &mut state.sizing {
                        let rows_focused = sz.focused_field == SizeField::Rows;
                        let rows_resp = draw_numeric_size_box(ui, rows_id, &mut sz.rows, rows_focused);
                        ui.label(
                            egui::RichText::new("×")
                                .font(egui::FontId::monospace(12.0))
                                .color(DETAIL_COLOR),
                        );
                        let cols_focused = sz.focused_field == SizeField::Cols;
                        let cols_resp = draw_numeric_size_box(ui, cols_id, &mut sz.cols, cols_focused);

                        if !inline_tab {
                            if rows_resp.clicked() {
                                sz.focused_field = SizeField::Rows;
                                ui.ctx().memory_mut(|m| m.request_focus(rows_id));
                            } else if cols_resp.clicked() {
                                sz.focused_field = SizeField::Cols;
                                ui.ctx().memory_mut(|m| m.request_focus(cols_id));
                            }
                        }
                        if cols_resp.changed() || rows_resp.changed() {
                            sz.error.clear();
                        }
                    }
                });
            });
        });

    ui.painter().set(
        bg_idx,
        egui::Shape::rect_filled(inner.response.rect, 0.0, ACTIVE_ROW_BG),
    );

    if inline_escape {
        state.sizing = None;
        return;
    }

    if inline_enter {
        if let Some(sz) = &mut state.sizing {
            match check_size(&sz.cols, &sz.rows) {
                Ok(valid) => {
                    let mut full_verb = size_edit.verb.clone();
                    full_verb.push(valid.cols.to_string());
                    full_verb.push(valid.rows.to_string());
                    state.sizing = None;
                    emit_verb(out, &full_verb, false);
                    return;
                }
                Err(reason) => {
                    sz.error = reason;
                }
            }
        }
    }

    let refusal = state
        .sizing
        .as_ref()
        .and_then(|sz| (!sz.error.is_empty()).then_some(sz.error.as_str()))
        .or(external_error);
    if let Some(err_text) = refusal {
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: 26,
                right: 10,
                top: 0,
                bottom: 3,
            })
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(err_text)
                        .font(egui::FontId::monospace(11.0))
                        .color(REFUSED_COLOR),
                );
            });
    }
}

fn draw_numeric_size_box(
    ui: &mut egui::Ui,
    field_id: egui::Id,
    text: &mut String,
    focused: bool,
) -> egui::Response {
    let border = if focused {
        FIELD_FOCUS_BORDER
    } else {
        BORDER_COLOR
    };
    let inner = egui::Frame::new()
        .fill(FIELD_BG)
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(text)
                    .id(field_id)
                    .frame(egui::Frame::NONE)
                    .horizontal_align(egui::Align::Max)
                    .font(egui::FontId::monospace(12.0))
                    .text_color(CHIP_ON_TEXT)
                    .desired_width(42.0),
            )
        });
    inner.inner
}

#[expect(
    clippy::too_many_arguments,
    reason = "hold-to-close row tracks both pointer and keyboard hold progress and paints its ring"
)]
fn draw_hold_row(
    ui: &mut egui::Ui,
    state: &mut ContextMenuState,
    non_sep: &[&MenuItem],
    row_idx: usize,
    item_id: &str,
    label: &str,
    detail: Option<&str>,
    verb: &[String],
    pointer_moved: bool,
    now_secs: f64,
    hold_secs: f64,
    out: &mut Vec<String>,
) {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let mut mark_rect = egui::Rect::NOTHING;

    let inner = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 6,
            right: 10,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width().max(MENU_MIN_WIDTH - 16.0));
            ui.set_min_height(ROW_MIN_HEIGHT - 6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (mrect, _) = ui.allocate_exact_size(
                    egui::vec2(MARK_BOX_SIZE, MARK_BOX_SIZE),
                    egui::Sense::hover(),
                );
                mark_rect = mrect;
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .font(egui::FontId::monospace(12.5))
                            .color(TEXT_COLOR),
                    )
                    .selectable(false),
                );
                if let Some(detail_text) = detail {
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(detail_text)
                                    .font(egui::FontId::monospace(10.5))
                                    .color(DETAIL_COLOR),
                            )
                            .selectable(false),
                        );
                    });
                }
            });
        });

    let row_rect = inner.response.rect;
    state.item_rects.insert(item_id.to_owned(), row_rect);
    let resp = ui.interact(
        row_rect,
        egui::Id::new(("wall_menu_hold", item_id)),
        egui::Sense::click_and_drag(),
    );
    if pointer_moved && resp.hovered() {
        set_active_row(state, non_sep, row_idx);
    }

    let pointer_pressed_inside = ui.ctx().input(|i| {
        i.pointer.primary_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|pos| row_rect.contains(pos))
    });
    let pointer_still_holding = ui.ctx().input(|i| {
        i.pointer.primary_down()
            && i.pointer
                .interact_pos()
                .or_else(|| i.pointer.hover_pos())
                .is_some_and(|pos| row_rect.contains(pos))
    });

    if pointer_pressed_inside {
        set_active_row(state, non_sep, row_idx);
        if hold_secs <= 0.0 {
            state.hold = None;
            emit_verb(out, verb, false);
        } else if state.hold.is_none() {
            state.hold = Some(HoldProgress {
                item_id: item_id.to_owned(),
                started_at_secs: now_secs,
                source: HoldSource::Pointer,
            });
        }
    }

    // Cancel a pointer-initiated hold if released or pointer slid off the row.
    if state.hold.as_ref().is_some_and(|h| {
        h.item_id == item_id && h.source == HoldSource::Pointer && !pointer_still_holding
    }) {
        state.hold = None;
    }

    let mut progress: f32 = 0.0;
    if let Some(hold) = state.hold.as_ref().filter(|h| h.item_id == item_id) {
        let elapsed = (now_secs - hold.started_at_secs).max(0.0);
        if hold_secs <= 0.0 || elapsed >= hold_secs {
            state.hold = None;
            emit_verb(out, verb, false);
        } else {
            progress = (elapsed / hold_secs).clamp(0.0, 1.0) as f32;
            ui.ctx()
                .request_repaint_after(Duration::from_millis(16));
        }
    }

    if state.active_row == Some(row_idx) {
        ui.painter()
            .set(bg_idx, egui::Shape::rect_filled(row_rect, 0.0, ACTIVE_ROW_BG));
    }

    let mark_color = if progress > 0.0 {
        REFUSED_COLOR
    } else {
        MARK_COLOR
    };
    ui.painter().text(
        mark_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(12.0),
        mark_color,
    );
    if progress > 0.0 {
        paint_hold_ring(ui.painter(), mark_rect.center(), 7.5, progress);
    }
}

fn paint_hold_ring(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    progress: f32,
) {
    let track_color = egui::Color32::from_rgba_unmultiplied(0xe5, 0x53, 0x4b, 48);
    painter.circle_stroke(center, radius, egui::Stroke::new(1.4, track_color));

    let clamped = progress.clamp(0.0, 1.0);
    if clamped <= 0.0 {
        return;
    }
    let steps = ((clamped * 32.0).ceil() as usize).max(2);
    let mut points = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let t = (step as f32 / steps as f32) * clamped;
        let angle = -FRAC_PI_2 + t * TAU;
        let (sin_a, cos_a) = angle.sin_cos();
        points.push(egui::pos2(center.x + radius * cos_a, center.y + radius * sin_a));
    }
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.6, REFUSED_COLOR),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::menu::{GroupLayout, MenuGroup, MenuTerminal, TerminalPart};

    fn sample_wall_state() -> MenuState {
        MenuState {
            focused_id: Some("t2".to_owned()),
            selection: vec!["t2".to_owned()],
            fullscreen: None,
            sidebar_open: false,
            status_window_open: false,
            settings_panel_open: false,
            config: MenuConfig::default(),
            terminals: vec![
                MenuTerminal {
                    id: "t1".to_owned(),
                    name: "build-shell".to_owned(),
                    named: true,
                    cols: 100,
                    rows: 30,
                    hidden: false,
                    group: None,
                    selected: true,
                    agent_view: None,
                },
                MenuTerminal {
                    id: "t2".to_owned(),
                    name: "logs".to_owned(),
                    named: false,
                    cols: 80,
                    rows: 24,
                    hidden: false,
                    group: Some("g1".to_owned()),
                    selected: false,
                    agent_view: None,
                },
            ],
            groups: vec![MenuGroup {
                id: "g1".to_owned(),
                name: "workers".to_owned(),
                named: true,
                layout: GroupLayout::Tile,
                columns: 2,
                bars: true,
                members: vec!["t2".to_owned()],
            }],
        }
    }

    fn make_raw_input(size: (f32, f32), time: f64, events: Vec<egui::Event>) -> egui::RawInput {
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

    fn key_down(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn key_up(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn run_frame(
        ctx: &egui::Context,
        state: &mut ContextMenuState,
        input: &ContextMenuInput,
        raw: egui::RawInput,
    ) -> (bool, Vec<String>, egui::FullOutput) {
        let mut out = Vec::new();
        let mut wants_keys = false;
        let full = ctx.run_ui(raw, |root_ui| {
            wants_keys = show(root_ui.ctx(), state, input, &mut out);
        });
        (wants_keys, out, full)
    }

    #[test]
    fn lays_out_across_viewport_sizes_and_flips_inside_bounds() {
        let wall_state = sample_wall_state();
        let open = OpenContextMenu::from_wall(
            MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            Point::new(780.0, 560.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let input = ContextMenuInput {
            open: Some(open),
            menu_hint: true,
            ..Default::default()
        };

        for (w, h) in [(320.0, 240.0), (800.0, 600.0), (1600.0, 1000.0)] {
            let ctx = egui::Context::default();
            let mut state = ContextMenuState::default();
            // Run two frames so measured_size settles.
            let (wants1, out1, _) =
                run_frame(&ctx, &mut state, &input, make_raw_input((w, h), 0.0, vec![]));
            let (wants2, out2, _) =
                run_frame(&ctx, &mut state, &input, make_raw_input((w, h), 0.016, vec![]));
            assert!(wants1 && wants2);
            assert!(out1.is_empty() && out2.is_empty());
            let rect = state.last_rect().expect("menu rect recorded");
            assert!(rect.width() >= MENU_MIN_WIDTH - 1.0);
            if w >= rect.width() + 8.0 {
                assert!(
                    rect.right() <= w - 3.0,
                    "menu right {} should stay inside viewport width {w}",
                    rect.right()
                );
            }
        }
    }

    #[test]
    fn keyboard_navigation_runs_actions_choices_and_escape() {
        let wall_state = sample_wall_state();
        let open = OpenContextMenu::from_wall(
            MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            Point::new(40.0, 40.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let input = ContextMenuInput {
            open: Some(open),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut state = ContextMenuState::default();

        // Down once highlights row 0 ("copy"), Enter dispatches wall.copy t1 + wall.menu close.
        let (_, out, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.0, vec![key_down(egui::Key::ArrowDown), key_down(egui::Key::Enter)]),
        );
        assert_eq!(out, vec!["wall.copy t1", "wall.menu close"]);

        // Group tiled columns choice: stepping columns keeps the menu open (`stay = true`).
        let group_open = OpenContextMenu::from_wall(
            MenuTarget::Group {
                id: "g1".to_owned(),
            },
            Point::new(50.0, 50.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let group_input = ContextMenuInput {
            open: Some(group_open),
            ..Default::default()
        };
        // Non-sep rows on tiled group g1: 0=frame, 1=rename…, 2=layout, 3=columns, ...
        let (_, out_cols, _) = run_frame(
            &ctx,
            &mut state,
            &group_input,
            make_raw_input(
                (800.0, 600.0),
                0.1,
                vec![
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowRight),
                    key_down(egui::Key::Enter),
                ],
            ),
        );
        assert_eq!(out_cols, vec!["wall.group.columns g1 +1"]);

        // Escape closes the menu.
        let (_, out_esc, _) = run_frame(
            &ctx,
            &mut state,
            &group_input,
            make_raw_input((800.0, 600.0), 0.2, vec![key_down(egui::Key::Escape)]),
        );
        assert_eq!(out_esc, vec!["wall.menu close"]);
    }

    #[test]
    fn rename_inline_field_submits_trimmed_name_and_empty_restores_default() {
        let wall_state = sample_wall_state();
        let open = OpenContextMenu::from_wall(
            MenuTarget::Group {
                id: "g1".to_owned(),
            },
            Point::new(40.0, 40.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let input = ContextMenuInput {
            open: Some(open),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut state = ContextMenuState::default();

        // Navigate to row 1 ("rename…") and press Enter to open inline field.
        let (_, out0, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                0.0,
                vec![
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::Enter),
                ],
            ),
        );
        assert!(out0.is_empty());
        assert!(state.is_renaming());

        // Replace text with a multi-word name and press Enter.
        state.renaming.as_mut().unwrap().text = "  my workers  ".to_owned();
        let (_, out1, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.05, vec![key_down(egui::Key::Enter)]),
        );
        assert_eq!(out1, vec!["wall.group.rename g1 \"my workers\"", "wall.menu close"]);

        // Re-enter rename, clear text, press Enter -> bare verb without trailing empty arg.
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.1, vec![key_down(egui::Key::Enter)]),
        );
        state.renaming.as_mut().unwrap().text = "   ".to_owned();
        let (_, out2, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.15, vec![key_down(egui::Key::Enter)]),
        );
        assert_eq!(out2, vec!["wall.group.rename g1", "wall.menu close"]);
    }

    #[test]
    fn custom_size_validates_tabs_between_fields_and_esc_returns_to_chips() {
        let wall_state = sample_wall_state();
        let open = OpenContextMenu::from_wall(
            MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            Point::new(40.0, 40.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let size_only = OpenContextMenu {
            items: open
                .items
                .into_iter()
                .filter(|it| it.id() == "size")
                .collect(),
            ..open
        };
        let input = ContextMenuInput {
            open: Some(size_only),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut state = ContextMenuState::default();

        // Highlight size row, step Left to wrap onto `custom…`, press Enter.
        let (_, out0, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                0.0,
                vec![
                    key_down(egui::Key::ArrowDown),
                    key_down(egui::Key::ArrowLeft),
                    key_down(egui::Key::Enter),
                ],
            ),
        );
        assert!(out0.is_empty());
        assert!(state.is_sizing());

        // Invalid cols ("0") refuses without closing.
        state.sizing.as_mut().unwrap().cols = "0".to_owned();
        let (_, out_bad, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.05, vec![key_down(egui::Key::Enter)]),
        );
        assert!(out_bad.is_empty());
        assert_eq!(
            state.sizing.as_ref().unwrap().error,
            "cols: at least 1"
        );

        // Escape exits sizing back to chips without closing; a second Escape closes.
        let (_, out_back, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.1, vec![key_down(egui::Key::Escape)]),
        );
        assert!(out_back.is_empty());
        assert!(!state.is_sizing());

        // Re-open custom size, clear both fields, type into cols, Tab to rows, type into rows, Enter!
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 0.15, vec![key_down(egui::Key::Enter)]),
        );
        state.sizing.as_mut().unwrap().cols.clear();
        state.sizing.as_mut().unwrap().rows.clear();
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                0.2,
                vec![egui::Event::Text("132".to_owned())],
            ),
        );
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                0.25,
                vec![key_down(egui::Key::Tab)],
            ),
        );
        let (_, out_ok, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                0.3,
                vec![
                    egui::Event::Text("43".to_owned()),
                    key_down(egui::Key::Enter),
                ],
            ),
        );
        assert_eq!(out_ok, vec!["terminal.resize t1 132 43", "wall.menu close"]);
    }

    #[test]
    fn hold_close_cancels_on_early_release_and_fires_after_hold_duration() {
        let wall_state = sample_wall_state();
        let open = OpenContextMenu::from_wall(
            MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            Point::new(40.0, 40.0),
            &wall_state,
            &wall_state.config,
            Platform::Mac,
        );
        let input = ContextMenuInput {
            open: Some(open),
            close_hold_ms: 500.0,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut state = ContextMenuState::default();

        // Jump to End ("close" hold row) and start holding Enter at t=1.0s.
        let (_, out_start, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input(
                (800.0, 600.0),
                1.0,
                vec![key_down(egui::Key::End), key_down(egui::Key::Enter)],
            ),
        );
        assert!(out_start.is_empty());

        // Mid-hold at t=1.25s: still holding, not yet complete.
        let (_, out_mid, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 1.25, vec![]),
        );
        assert!(out_mid.is_empty());

        // Release early at t=1.30s -> cancels! Even at t=2.0s nothing fires.
        let (_, out_rel, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 1.30, vec![key_up(egui::Key::Enter)]),
        );
        let (_, out_after, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 2.0, vec![]),
        );
        assert!(out_rel.is_empty() && out_after.is_empty());

        // Hold again at t=3.0s through t=3.55s -> fires wall.close t1 and closes!
        let _ = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 3.0, vec![key_down(egui::Key::Enter)]),
        );
        let (_, out_done, _) = run_frame(
            &ctx,
            &mut state,
            &input,
            make_raw_input((800.0, 600.0), 3.55, vec![]),
        );
        assert_eq!(out_done, vec!["wall.close t1", "wall.menu close"]);
    }

    #[test]
    fn simulated_click_on_action_row_and_empty_input_emit_expected_verbs() {
        let ctx = egui::Context::default();
        let mut state = ContextMenuState::default();

        // Empty items: draws "no actions" box (non-empty shapes) AND emits wall.menu close.
        let empty_input = ContextMenuInput {
            open: Some(OpenContextMenu {
                target: MenuTarget::Canvas { world: None },
                at: Point::new(20.0, 20.0),
                caption: "canvas".to_owned(),
                items: vec![],
                size_error: None,
            }),
            ..Default::default()
        };
        let (_, empty_out, empty_full) = run_frame(
            &ctx,
            &mut state,
            &empty_input,
            make_raw_input((640.0, 480.0), 0.0, vec![]),
        );
        assert_eq!(empty_out, vec!["wall.menu close"]);
        assert!(!empty_full.shapes.is_empty());

        // Single action row clicked by pointer.
        let one_item_input = ContextMenuInput {
            open: Some(OpenContextMenu {
                target: MenuTarget::Terminal {
                    id: "t3".to_owned(),
                    part: TerminalPart::Title,
                },
                at: Point::new(30.0, 30.0),
                caption: "t3".to_owned(),
                items: vec![MenuItem::Action {
                    id: "frame".to_owned(),
                    label: "frame".to_owned(),
                    verb: vec!["wall.reveal".to_owned(), "t3".to_owned()],
                    detail: None,
                }],
                size_error: None,
            }),
            ..Default::default()
        };
        let _ = run_frame(
            &ctx,
            &mut state,
            &one_item_input,
            make_raw_input((640.0, 480.0), 0.1, vec![]),
        );
        let box_rect = state.last_rect().expect("menu box laid out");
        let click_pos = egui::pos2(box_rect.center().x, box_rect.bottom() - 12.0);
        let (_, click_out, _) = run_frame(
            &ctx,
            &mut state,
            &one_item_input,
            make_raw_input(
                (640.0, 480.0),
                0.2,
                vec![
                    egui::Event::PointerMoved(click_pos),
                    egui::Event::PointerButton {
                        pos: click_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: click_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            ),
        );
        assert_eq!(click_out, vec!["wall.reveal t3", "wall.menu close"]);
    }
}
