//! World-anchored Agent View companion cards for the terminal wall (`wall/AgentViewOverlay.jsx`).
//!
//! Unlike docked chrome panels, each open Agent View is placed at the integer-snapped screen
//! rectangle computed for it by [`wall_core::agent_views::AgentViews`] beside its terminal, moving
//! and scaling continuously with the canvas camera:
//!
//! - **Small-scale handling in egui**:
//!   - Below [`wall_core::agent_views::BADGE_FLOOR_PX`] (`placement.badge_mode`, `11.0 * scale < 7.2`,
//!     i.e. `scale < ~0.655`), the full card collapses to a crisp far-zoom badge (`>= 28×18` pt)
//!     showing the session status dot, total event count (`≡ N`), and unread count (`+K`); clicking
//!     it emits `wall.reveal <id>`.
//!   - At and above the floor (`scale >= ~0.655`), we avoid `TSTransform` bitmap scaling (which
//!     blurs glyphs when zoomed in and washes out 1 px dividers when zoomed out). Instead, each
//!     card builds a per-card `egui::Style` and metric table scaled by `s = placement.scale`:
//!     `FontId::monospace(px * s)` rasterizes glyphs at their exact target pixel size in egui's
//!     atlas, `spacing.interact_size` is zeroed so egui's default `40×18` pt button floor cannot
//!     overflow a `0.66×` card, paddings/gaps/row heights/scrollbars scale linearly with `s` so
//!     nothing reflows across zoom levels, the outer frame stays a crisp 1 pt border, and internal
//!     dividers use `(placement.hairline_px as f32 * s).max(1.0)` (`>= 1` pt).
//! - **Data in, verb lines out**: emits only validated `wall.*` verbs (`wall.reveal`,
//!   `wall.agent <id> close`, `wall.agent.side`, `wall.agent.filter`, `wall.agent.expand`,
//!   `wall.agent.collapse`, `wall.agent.show`, `wall.agent.jump`, `wall.agent.earlier`).

use std::collections::{BTreeMap, BTreeSet};

use wall_core::agent_view::{
    category_by_id, category_hue, expand_category_tokens, group_selection_state,
    is_default_filter, is_exact_filter, pick_agent_side, CategoryGroupNode, CategorySubgroup,
    GroupSelection, CATEGORY_GROUPS_TREE, CATEGORY_IDS,
};
use wall_core::agent_views::{
    AgentEventRow, AgentViewDescriptor, AgentViews, AgentViewsHost, ScreenPlacement,
    VisibleScreenPlacement,
};
use wall_core::camera::WallCamera as CoreWallCamera;
use wall_core::config::AgentSide;
use wall_core::geom::{Point as CorePoint, Rect as CoreRect};

/// Coordinate space of [`VisibleScreenPlacement`] values supplied in [`AgentViewPanelInput`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementUnits {
    /// Coordinates and `scale` are already in egui logical points (matching browser CSS pixels).
    #[default]
    Points,
    /// Coordinates and `scale` were computed in physical framebuffer pixels and are divided by
    /// `ctx.pixels_per_point()` before placing egui areas.
    PhysicalPixels,
}

/// One open Agent View card to draw this frame.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentViewCardInput {
    /// Snapshot descriptor from [`AgentViews::describe`].
    pub view: AgentViewDescriptor,
    /// Per-frame screen placement from [`AgentViews::screen_placement_of`].
    pub placement: VisibleScreenPlacement,
    /// Optional screen rectangle of the attached terminal window (body + title bar, in the same
    /// [`PlacementUnits`] as `placement`), enabling header-drag side picking and drop-ghost
    /// preview in addition to the header's `L`/`R`/`T`/`B` side buttons.
    pub terminal_screen_rect: Option<egui::Rect>,
}

/// Plain-data frame input for [`show`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentViewPanelInput {
    /// Visible Agent View cards in canvas draw order.
    pub cards: Vec<AgentViewCardInput>,
    /// When `true` (single-pane or group fullscreen active), no Agent View cards are drawn.
    pub fullscreen: bool,
    /// Coordinate units of `placement` and `terminal_screen_rect`.
    pub units: PlacementUnits,
}

/// Alias matching the uniform `<Panel>Input` naming across `native/src/wall/panels/`.
pub type Input = AgentViewPanelInput;

impl AgentViewPanelInput {
    /// Build [`AgentViewPanelInput`] directly from [`wall_core::agent_views::AgentViews`] and a
    /// [`wall_core::camera::WallCamera`] (logical points).
    pub fn from_views<S: AsRef<str>>(
        host: &dyn AgentViewsHost,
        views: &AgentViews,
        camera: &CoreWallCamera,
        order: &[S],
    ) -> Self {
        if host.fullscreen_active() {
            return Self {
                cards: Vec::new(),
                fullscreen: true,
                units: PlacementUnits::Points,
            };
        }
        let (vw, vh) = camera.view();
        let pose = camera.pose();
        let mut cards = Vec::new();
        for id_ref in order {
            let id = id_ref.as_ref();
            let ScreenPlacement::Visible(placement) =
                views.screen_placement_at(host, pose, vw, vh, id)
            else {
                continue;
            };
            let Some(view) = views.describe(host, id) else {
                continue;
            };
            let terminal_screen_rect = host.terminal(id).map(|term| {
                let bar = host.bar_height(id, pose.zoom).max(0.0);
                let win_world = CoreRect {
                    max_y: term.body_rect.max_y + bar,
                    ..term.body_rect
                };
                let sb = pose.to_screen_box(win_world, vw, vh);
                egui::Rect::from_min_max(
                    egui::pos2(sb.left as f32, sb.top as f32),
                    egui::pos2(sb.right as f32, sb.bottom as f32),
                )
            });
            cards.push(AgentViewCardInput {
                view,
                placement,
                terminal_screen_rect,
            });
        }
        Self {
            cards,
            fullscreen: false,
            units: PlacementUnits::Points,
        }
    }
}

/// Per-card view state retained across frames by the caller.
#[derive(Clone, Debug)]
pub struct CardUiState {
    /// Whether the collapsible grouped category filter drawer is open.
    pub menu_open: bool,
    /// Group/subgroup IDs folded inside the category filter drawer.
    pub folded_groups: BTreeSet<String>,
    /// Whether the event scroll area is pinned to its live bottom tail.
    pub stick_bottom: bool,
    /// Unread live rows accumulated locally while `!stick_bottom`.
    pub local_unread: usize,
    last_host_stick_bottom: Option<bool>,
    last_session_id: Option<String>,
    last_row_count: usize,
    last_total_count: usize,
    last_tail_id: Option<String>,
    last_content_h: f32,
    scroll_to_tail_next: bool,
}

impl Default for CardUiState {
    fn default() -> Self {
        Self {
            menu_open: false,
            folded_groups: BTreeSet::new(),
            stick_bottom: true,
            local_unread: 0,
            last_host_stick_bottom: None,
            last_session_id: None,
            last_row_count: 0,
            last_total_count: 0,
            last_tail_id: None,
            last_content_h: 0.0,
            scroll_to_tail_next: false,
        }
    }
}

#[derive(Clone, Debug)]
struct HeaderDragState {
    id: String,
    start_pos: egui::Pos2,
    dragging: bool,
    current_side: AgentSide,
}

/// Persistent UI state across frames for all Agent View cards on the wall.
#[derive(Clone, Debug, Default)]
pub struct AgentViewPanelState {
    cards: BTreeMap<String, CardUiState>,
    drag: Option<HeaderDragState>,
    /// True when an open category menu drawer should capture keyboard input (`Escape` to close)
    /// rather than forwarding keys to the focused terminal.
    pub wants_keyboard: bool,
    /// True when the pointer is over any visible Agent View card/badge or dragging a card header.
    pub wants_pointer: bool,
    /// Set when the user clicks an unfocused Agent View card so the host can focus its terminal
    /// without camera-framing (`wall.focus(id)` in JS; `commands.rs` only has `wall.reveal`).
    pub clicked_focus_id: Option<String>,
    /// `(terminal_id, stick_bottom)` transitions observed from scroll/follow interactions so the
    /// host can optionally sync [`AgentViews::set_stick_bottom`].
    pub stick_bottom_changes: Vec<(String, bool)>,
    /// Per-frame interactive hit rectangles recorded by semantic key (`"<term_id>:<widget_key>"`),
    /// enabling deterministic headless click tests without brittle pixel coordinates.
    hit_rects: BTreeMap<String, egui::Rect>,
}

/// Alias matching the uniform `<Panel>State` naming across `native/src/wall/panels/`.
pub type State = AgentViewPanelState;

impl AgentViewPanelState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow a terminal's [`CardUiState`], if present.
    pub fn card(&self, id: &str) -> Option<&CardUiState> {
        self.cards.get(id)
    }

    /// Mutable access to a terminal's [`CardUiState`].
    pub fn card_mut(&mut self, id: &str) -> &mut CardUiState {
        self.cards.entry(id.to_string()).or_default()
    }

    /// Drain `(terminal_id, stick_bottom)` changes recorded during [`show`].
    pub fn take_stick_bottom_changes(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.stick_bottom_changes)
    }

    /// Take the unfocused terminal ID clicked during [`show`], if any.
    pub fn take_clicked_focus_id(&mut self) -> Option<String> {
        self.clicked_focus_id.take()
    }

    /// Look up the last rendered screen rectangle (in egui points) for `"<term_id>:<action>"`.
    pub fn hit_rect(&self, id: &str, action: &str) -> Option<egui::Rect> {
        self.hit_rects.get(&format!("{id}:{action}")).copied()
    }
}

#[derive(Clone, Copy)]
struct StatusDot {
    color: egui::Color32,
    glyph: &'static str,
    label: &'static str,
}

fn status_dot(view: &AgentViewDescriptor) -> StatusDot {
    if view.session_id.is_none() {
        return StatusDot {
            color: rgb_hex(0xf0b089),
            glyph: "⚠",
            label: "no session",
        };
    }
    let raw = view
        .session
        .as_ref()
        .map(|s| s.status.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(if view.following { "idle" } else { "unknown" });
    match raw {
        "processing" => StatusDot {
            color: rgb_hex(0x6ee7a0),
            glyph: "●",
            label: "processing",
        },
        "waiting" => StatusDot {
            color: rgb_hex(0xfbbf24),
            glyph: "●",
            label: "waiting for you",
        },
        "idle" => StatusDot {
            color: rgb_hex(0x60a5fa),
            glyph: "●",
            label: "idle",
        },
        "error" => StatusDot {
            color: rgb_hex(0xf87171),
            glyph: "●",
            label: "error",
        },
        "ended" => StatusDot {
            color: rgb_hex(0x6b7383),
            glyph: "○",
            label: "ended",
        },
        _ => StatusDot {
            color: rgb_hex(0x7c8596),
            glyph: "◌",
            label: "unknown",
        },
    }
}

const fn rgb_hex(hex: u32) -> egui::Color32 {
    egui::Color32::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

fn parse_css_hex(s: &str) -> Option<egui::Color32> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let val = u32::from_str_radix(hex, 16).ok()?;
    Some(rgb_hex(val))
}

fn with_alpha(c: egui::Color32, alpha: f32) -> egui::Color32 {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    let next_a = ((f32::from(a) * alpha.clamp(0.0, 1.0)).round() as u8).max(1);
    egui::Color32::from_rgba_unmultiplied(r, g, b, next_a)
}

#[derive(Clone, Copy)]
struct CardMetrics {
    s: f32,
    hairline: f32,
    header_h: f32,
    toolbar_h: f32,
    chips_h: f32,
    row_h: f32,
    follow_h: f32,
    font_header: f32,
    font_body: f32,
    font_code: f32,
    font_small: f32,
    font_tiny: f32,
}

impl CardMetrics {
    fn for_scale(scale: f32, hairline_px: u32) -> Self {
        let s = scale.clamp(0.5, 4.0);
        let hairline = ((hairline_px as f32) * s).max(1.0);
        Self {
            s,
            hairline,
            header_h: (22.0 * s).max(14.0),
            toolbar_h: (24.0 * s).max(15.0),
            chips_h: (22.0 * s).max(14.0),
            row_h: (19.0 * s).max(13.0),
            follow_h: (19.0 * s).max(13.0),
            font_header: (10.5 * s).max(6.8),
            font_body: (10.5 * s).max(6.8),
            font_code: (10.0 * s).max(6.5),
            font_small: (9.5 * s).max(6.2),
            font_tiny: (8.5 * s).max(6.0),
        }
    }
}

fn apply_scaled_card_style(ui: &mut egui::Ui, m: CardMetrics) {
    let s = m.s;
    let style = ui.style_mut();
    style.spacing.interact_size = egui::Vec2::ZERO;
    style.spacing.button_padding = egui::vec2((4.0 * s).max(2.0), (1.0 * s).max(0.5));
    style.spacing.item_spacing = egui::vec2((4.0 * s).max(1.5), (2.0 * s).max(1.0));
    style.spacing.window_margin = egui::Margin::ZERO;
    style.spacing.scroll.bar_width = (5.0 * s).clamp(3.0, 8.0);
    style.spacing.scroll.floating = true;
    style.visuals.override_text_color = Some(rgb_hex(0xd4d9e4));
    style.visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(m.hairline, rgb_hex(0x263149));
}

#[derive(Clone, Copy)]
struct BtnColors {
    bg: egui::Color32,
    fg: egui::Color32,
    border: egui::Color32,
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal compact button helper keeps visual styling, tooltip, and test-hit registration together"
)]
fn mini_button(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    term_id: &str,
    hit_key: &str,
    label: &str,
    font_px: f32,
    pad: egui::Vec2,
    colors: BtnColors,
    hairline: f32,
    enabled: bool,
    tooltip: &str,
) -> egui::Response {
    let font = egui::FontId::monospace(font_px);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, colors.fg);
    let desired = galley.size() + pad * 2.0;
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (_, rect) = ui.allocate_space(desired);
    let resp = ui.interact(rect, egui::Id::new(("av_btn", term_id, hit_key)), sense);
    hits.insert(format!("{term_id}:{hit_key}"), rect);

    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let fill = if hovered {
            brighten(colors.bg, 18)
        } else {
            colors.bg
        };
        let stroke_col = if hovered {
            brighten(colors.border, 24)
        } else {
            colors.border
        };
        ui.painter().rect_filled(rect, 0.0, fill);
        if colors.border != egui::Color32::TRANSPARENT && hairline > 0.0 {
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(hairline, stroke_col),
                egui::StrokeKind::Inside,
            );
        }
        let text_pos = egui::pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(
            text_pos,
            galley,
            if enabled {
                colors.fg
            } else {
                with_alpha(colors.fg, 0.5)
            },
        );
    }

    if !tooltip.is_empty() {
        resp.on_hover_text(tooltip)
    } else {
        resp
    }
}

fn brighten(c: egui::Color32, delta: u8) -> egui::Color32 {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    egui::Color32::from_rgba_unmultiplied(
        r.saturating_add(delta),
        g.saturating_add(delta),
        b.saturating_add(delta),
        a,
    )
}

fn truncate_to_width(painter: &egui::Painter, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if text.is_empty() || max_w <= 4.0 {
        return String::new();
    }
    let full = painter.layout_no_wrap(text.to_string(), font.clone(), egui::Color32::WHITE);
    if full.size().x <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        let g = painter.layout_no_wrap(candidate, font.clone(), egui::Color32::WHITE);
        if g.size().x <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        "…".to_string()
    } else {
        chars[..lo].iter().collect::<String>() + "…"
    }
}

/// Convert a screen-space pointer position (origin top-left, +Y down) relative to a terminal's
/// screen-space window rectangle into an [`AgentSide`] via [`pick_agent_side`] (which operates in
/// world space where +Y is up).
pub fn pick_side_screen_space(term_screen_rect: egui::Rect, pointer: egui::Pos2) -> AgentSide {
    // Flip Y around 0 (`world_y = -screen_y`) so `top` (`pointer.y < rect.top()`) maps to `max_y`.
    let world_rect = CoreRect {
        min_x: f64::from(term_screen_rect.min.x),
        max_x: f64::from(term_screen_rect.max.x),
        min_y: -f64::from(term_screen_rect.max.y),
        max_y: -f64::from(term_screen_rect.min.y),
    };
    let world_pt = CorePoint::new(f64::from(pointer.x), -f64::from(pointer.y));
    pick_agent_side(world_rect, world_pt)
}

fn preview_ghost_screen_rect(
    term_rect: egui::Rect,
    current_card_rect: egui::Rect,
    side: AgentSide,
    s: f32,
) -> egui::Rect {
    let gap = (6.0 * s).max(2.0);
    let w = current_card_rect.width().max(48.0 * s);
    let h = current_card_rect.height().max(48.0 * s);
    match side {
        AgentSide::Left => egui::Rect::from_min_max(
            egui::pos2(term_rect.min.x - gap - w, term_rect.min.y),
            egui::pos2(term_rect.min.x - gap, term_rect.max.y),
        ),
        AgentSide::Right => egui::Rect::from_min_max(
            egui::pos2(term_rect.max.x + gap, term_rect.min.y),
            egui::pos2(term_rect.max.x + gap + w, term_rect.max.y),
        ),
        AgentSide::Top => egui::Rect::from_min_max(
            egui::pos2(term_rect.min.x, term_rect.min.y - gap - h),
            egui::pos2(term_rect.max.x, term_rect.min.y - gap),
        ),
        AgentSide::Bottom => egui::Rect::from_min_max(
            egui::pos2(term_rect.min.x, term_rect.max.y + gap),
            egui::pos2(term_rect.max.x, term_rect.max.y + gap + h),
        ),
    }
}

fn draw_dashed_rect(painter: &egui::Painter, rect: egui::Rect, stroke: egui::Stroke, dash: f32, gap: f32) {
    let corners = [
        (rect.left_top(), rect.right_top()),
        (rect.right_top(), rect.right_bottom()),
        (rect.right_bottom(), rect.left_bottom()),
        (rect.left_bottom(), rect.left_top()),
    ];
    let step = (dash + gap).max(2.0);
    for (a, b) in corners {
        let dir = b - a;
        let len = dir.length();
        if len <= 1e-3 {
            continue;
        }
        let unit = dir / len;
        let mut t = 0.0;
        while t < len {
            let t_end = (t + dash).min(len);
            painter.line_segment([a + unit * t, a + unit * t_end], stroke);
            t += step;
        }
    }
}

fn placement_in_points(
    p: &VisibleScreenPlacement,
    units: PlacementUnits,
    ppp: f32,
) -> (egui::Rect, f32, bool) {
    let inv = match units {
        PlacementUnits::Points => 1.0,
        PlacementUnits::PhysicalPixels => 1.0 / ppp.max(0.5),
    };
    let left = (p.left as f32) * inv;
    let top = (p.top as f32) * inv;
    let width = ((p.width as f32) * inv).max(if p.badge_mode { 28.0 } else { 8.0 });
    let height = ((p.height as f32) * inv).max(if p.badge_mode { 18.0 } else { 8.0 });
    let scale = (p.scale as f32) * inv;
    let badge_mode = p.badge_mode || (11.0 * scale < 7.2);
    (
        egui::Rect::from_min_size(egui::pos2(left, top), egui::vec2(width, height)),
        scale,
        badge_mode,
    )
}

/// Render all open world-anchored Agent View cards (and far-zoom badges) onto `ctx`.
///
/// Appends `wall.*` command-bus verb lines to `out` and returns `true` when an open category
/// filter drawer wants keyboard input this frame (`Escape` dismisses the drawer without sending
/// `Escape` to the focused terminal).
pub fn show(
    ctx: &egui::Context,
    state: &mut AgentViewPanelState,
    input: &AgentViewPanelInput,
    out: &mut Vec<String>,
) -> bool {
    state.hit_rects.clear();
    state.wants_keyboard = false;
    state.wants_pointer = false;
    state.clicked_focus_id = None;

    if input.fullscreen || input.cards.is_empty() {
        state.drag = None;
        return false;
    }

    let ppp = ctx.pixels_per_point();

    // Escape closes any open category menu drawer (`AgentViewOverlay.jsx:514-519`).
    let any_menu_was_open = input
        .cards
        .iter()
        .any(|c| state.cards.get(&c.view.id).is_some_and(|st| st.menu_open));
    if any_menu_was_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        for card in &input.cards {
            if let Some(st) = state.cards.get_mut(&card.view.id) {
                st.menu_open = false;
            }
        }
        state.wants_keyboard = true;
    }

    // Outside click dismisses open category menu drawers (`AgentViewOverlay.jsx:509-513`).
    let pointer_pressed_pos = ctx.input(|i| {
        i.pointer
            .button_pressed(egui::PointerButton::Primary)
            .then(|| i.pointer.interact_pos())
            .flatten()
    });

    // Sort cards by z_index (`3` unfocused, `4` focused) while preserving input order ties.
    let mut order_indices: Vec<usize> = (0..input.cards.len()).collect();
    order_indices.sort_by_key(|&idx| input.cards[idx].placement.z_index);

    let pointer_hover_pos = ctx.input(|i| i.pointer.hover_pos().or_else(|| i.pointer.interact_pos()));

    for idx in order_indices {
        let card = &input.cards[idx];
        let view = &card.view;
        let (outer_rect, scale, badge_mode) =
            placement_in_points(&card.placement, input.units, ppp);

        if let Some(p) = pointer_hover_pos {
            if outer_rect.contains(p) {
                state.wants_pointer = true;
            }
        }
        if let Some(press_pos) = pointer_pressed_pos {
            if outer_rect.contains(press_pos) && !card.placement.focused {
                state.clicked_focus_id = Some(view.id.clone());
            }
        }

        let area_order = if card.placement.focused {
            egui::Order::Foreground
        } else {
            egui::Order::Middle
        };
        let area_id = egui::Id::new(("wall_agent_view_card", &view.id));
        let layer_id = egui::LayerId::new(area_order, area_id);

        // Ensure `egui::Area` completes its sizing pass inside `Context::run_ui`'s built-in
        // multi-pass loop on the first frame (and on `Middle` <-> `Foreground` focus switches)
        // so cards render immediately in an on-demand `Wait` event loop or single-frame capture
        // while keeping `layer_id` registered in `Memory::areas()` for pointer hit-testing.
        if !ctx.memory(|mem| mem.areas().visible_last_frame(&layer_id)) {
            ctx.request_discard("agent_view_open");
        }

        egui::Area::new(area_id)
            .fixed_pos(outer_rect.min)
            .default_size(outer_rect.size())
            .order(area_order)
            .constrain(false)
            .fade_in(false)
            .show(ctx, |ui| {
                ui.set_clip_rect(outer_rect);
                ui.set_min_size(outer_rect.size());
                ui.set_max_size(outer_rect.size());

                if badge_mode {
                    render_badge(
                        ui,
                        &mut state.hit_rects,
                        view,
                        outer_rect,
                        card.placement.focused,
                        out,
                    );
                } else {
                    render_full_card(
                        ctx,
                        ui,
                        state,
                        card,
                        outer_rect,
                        scale,
                        pointer_pressed_pos,
                        out,
                    );
                }
            });
    }

    // Handle active header drag preview & drop-ghost outline (`DropSideGhost`).
    if let Some(drag) = state.drag.clone() {
        state.wants_pointer = true;
        let card_opt = input.cards.iter().find(|c| c.view.id == drag.id);
        let (pointer_pos, primary_down) = ctx.input(|i| {
            (
                i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()),
                i.pointer.button_down(egui::PointerButton::Primary),
            )
        });

        if let Some(card) = card_opt {
            let term_rect_pts = card.terminal_screen_rect.map(|r| match input.units {
                PlacementUnits::Points => r,
                PlacementUnits::PhysicalPixels => {
                    let inv = 1.0 / ppp.max(0.5);
                    egui::Rect::from_min_max(r.min * inv, r.max * inv)
                }
            });

            if let (Some(pos), Some(term_rect)) = (pointer_pos, term_rect_pts) {
                let mut next_drag = drag.clone();
                if !next_drag.dragging && (pos - next_drag.start_pos).length() >= 5.0 {
                    next_drag.dragging = true;
                }
                if next_drag.dragging {
                    next_drag.current_side = pick_side_screen_space(term_rect, pos);
                    let (card_rect, scale, _) =
                        placement_in_points(&card.placement, input.units, ppp);
                    let ghost = preview_ghost_screen_rect(
                        term_rect,
                        card_rect,
                        next_drag.current_side,
                        scale.max(0.65),
                    );
                    let painter = ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Tooltip,
                        egui::Id::new("wall_agent_drop_ghost"),
                    ));
                    painter.rect_filled(
                        ghost,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(0x6e, 0xe7, 0xa0, 22),
                    );
                    draw_dashed_rect(
                        &painter,
                        ghost,
                        egui::Stroke::new(1.5, rgb_hex(0x6ee7a0)),
                        6.0,
                        4.0,
                    );
                }
                if primary_down {
                    state.drag = Some(next_drag);
                } else {
                    if next_drag.dragging && next_drag.current_side != card.view.side {
                        out.push(format!(
                            "wall.agent.side {} {}",
                            card.view.id,
                            next_drag.current_side.as_str()
                        ));
                    }
                    state.drag = None;
                }
            } else if !primary_down {
                state.drag = None;
            }
        } else {
            state.drag = None;
        }
    }

    if input
        .cards
        .iter()
        .any(|c| state.cards.get(&c.view.id).is_some_and(|st| st.menu_open))
    {
        state.wants_keyboard = true;
    }

    state.wants_keyboard
}

fn render_badge(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    outer_rect: egui::Rect,
    focused: bool,
    out: &mut Vec<String>,
) {
    let dot = status_dot(view);
    let bg = if focused {
        rgb_hex(0x11261c)
    } else {
        rgb_hex(0x121622)
    };
    let border = if focused {
        rgb_hex(0x4ade80)
    } else {
        rgb_hex(0x4a5568)
    };

    let (_, rect) = ui.allocate_space(outer_rect.size());
    let resp = ui.interact(
        rect,
        egui::Id::new(("av_badge", &view.id)),
        egui::Sense::click(),
    );
    hits.insert(format!("{}:badge", view.id), outer_rect);

    let painter = ui.painter_at(outer_rect);
    let hovered = resp.hovered();
    painter.rect_filled(outer_rect, 0.0, if hovered { brighten(bg, 14) } else { bg });
    painter.rect_stroke(
        outer_rect,
        0.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let font_px = (outer_rect.height() * 0.52).clamp(8.5, 11.0);
    let font = egui::FontId::monospace(font_px);
    let inner = outer_rect.shrink2(egui::vec2(4.0, 1.0));

    let dot_galley = painter.layout_no_wrap(dot.glyph.to_string(), font.clone(), dot.color);
    let count_str = if inner.width() >= 58.0 && view.unread_count > 0 {
        format!("≡ {} +{}", view.total_count, view.unread_count)
    } else if inner.width() >= 34.0 {
        format!("≡ {}", view.total_count)
    } else {
        format!("{}", view.total_count)
    };
    let text_col = if view.unread_count > 0 {
        rgb_hex(0x8ef0b8)
    } else {
        rgb_hex(0xc4ccd8)
    };
    let count_trimmed = truncate_to_width(
        &painter,
        &count_str,
        &font,
        (inner.width() - dot_galley.size().x - 4.0).max(0.0),
    );
    let count_galley = painter.layout_no_wrap(count_trimmed, font, text_col);

    let total_w = dot_galley.size().x
        + if count_galley.size().x > 0.0 {
            4.0 + count_galley.size().x
        } else {
            0.0
        };
    let start_x = (outer_rect.center().x - total_w * 0.5).max(inner.min.x);
    painter.galley(
        egui::pos2(start_x, outer_rect.center().y - dot_galley.size().y * 0.5),
        dot_galley.clone(),
        dot.color,
    );
    if count_galley.size().x > 0.0 {
        painter.galley(
            egui::pos2(
                start_x + dot_galley.size().x + 4.0,
                outer_rect.center().y - count_galley.size().y * 0.5,
            ),
            count_galley,
            text_col,
        );
    }

    let tip = format!(
        "{} ({} events{}) — click to frame",
        view.session_label,
        view.visible_count,
        if view.unread_count > 0 {
            format!(", {} unread", view.unread_count)
        } else {
            String::new()
        }
    );
    if resp.on_hover_text(tip).clicked() {
        out.push(format!("wall.reveal {}", view.id));
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "card renderer threads panel state, card descriptor, scaled outer rect, and verb outbox together"
)]
fn render_full_card(
    _ctx: &egui::Context,
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    card: &AgentViewCardInput,
    outer_rect: egui::Rect,
    scale: f32,
    pointer_pressed_pos: Option<egui::Pos2>,
    out: &mut Vec<String>,
) {
    let view = &card.view;
    let focused = card.placement.focused;
    let m = CardMetrics::for_scale(scale, card.placement.hairline_px);
    apply_scaled_card_style(ui, m);

    // Sync per-card tail/unread state with incoming descriptor.
    let (menu_open, stick_bottom, unread_count, force_scroll_tail) = {
        let cs = state.cards.entry(view.id.clone()).or_default();
        let tail_id = view.rows.last().map(|r| r.summary.id.clone());
        let session_changed = cs.last_session_id != view.session_id;
        if session_changed {
            cs.stick_bottom = view.stick_bottom;
            cs.last_host_stick_bottom = Some(view.stick_bottom);
            cs.local_unread = 0;
            if cs.stick_bottom {
                cs.scroll_to_tail_next = true;
            }
        } else if cs.last_host_stick_bottom != Some(view.stick_bottom) {
            cs.stick_bottom = view.stick_bottom;
            cs.last_host_stick_bottom = Some(view.stick_bottom);
            if cs.stick_bottom {
                cs.local_unread = 0;
                cs.scroll_to_tail_next = true;
            }
        }
        let new_tail = tail_id != cs.last_tail_id && view.total_count > cs.last_total_count;
        if !session_changed && new_tail {
            if cs.stick_bottom {
                cs.scroll_to_tail_next = true;
            } else {
                cs.local_unread += view.total_count - cs.last_total_count;
            }
        }
        cs.last_session_id = view.session_id.clone();
        cs.last_row_count = view.rows.len();
        cs.last_total_count = view.total_count;
        cs.last_tail_id = tail_id;
        let force = std::mem::replace(&mut cs.scroll_to_tail_next, false);
        let unread = if cs.stick_bottom {
            0
        } else {
            view.unread_count.max(cs.local_unread)
        };
        (cs.menu_open, cs.stick_bottom, unread, force)
    };

    // Outer 1pt frame & card background (`#0a0a1e`).
    let outer_border = if focused {
        rgb_hex(0x4ade80)
    } else {
        rgb_hex(0x4a5568)
    };
    ui.painter().rect_filled(outer_rect, 0.0, rgb_hex(0x0a0a1e));
    ui.painter().rect_stroke(
        outer_rect,
        0.0,
        egui::Stroke::new(1.0, outer_border),
        egui::StrokeKind::Inside,
    );

    let inner_rect = outer_rect.shrink(1.0);
    if inner_rect.width() <= 2.0 || inner_rect.height() <= 2.0 {
        return;
    }

    let mut cursor_y = inner_rect.min.y;

    // ── 1. Header (`22 * s`) ─────────────────────────────────────────────────
    let header_h = m.header_h.min(inner_rect.height());
    let header_rect = egui::Rect::from_min_size(
        egui::pos2(inner_rect.min.x, cursor_y),
        egui::vec2(inner_rect.width(), header_h),
    );
    cursor_y += header_h;

    render_header(
        ui,
        state,
        card,
        header_rect,
        m,
        focused,
        unread_count,
        out,
    );

    if cursor_y >= inner_rect.max.y - 2.0 {
        return;
    }

    // ── 2. Toolbar (`24 * s`) ────────────────────────────────────────────────
    let toolbar_h = m.toolbar_h.min(inner_rect.max.y - cursor_y);
    let toolbar_rect = egui::Rect::from_min_size(
        egui::pos2(inner_rect.min.x, cursor_y),
        egui::vec2(inner_rect.width(), toolbar_h),
    );
    cursor_y += toolbar_h;

    let was_menu_open = menu_open;
    let filter_trigger_rect = render_toolbar(
        ui,
        state,
        view,
        toolbar_rect,
        m,
        menu_open,
        out,
    );
    let mut menu_open = state.cards.get(&view.id).is_some_and(|cs| cs.menu_open);

    // ── 3. Quick Category Filter Chips (`20 * s` when session is paired & height permits) ──
    if view.session_id.is_some() && (inner_rect.max.y - cursor_y) >= m.chips_h + 28.0 * m.s {
        let chips_rect = egui::Rect::from_min_size(
            egui::pos2(inner_rect.min.x, cursor_y),
            egui::vec2(inner_rect.width(), m.chips_h),
        );
        cursor_y += m.chips_h;
        render_category_chips(ui, &mut state.hit_rects, view, chips_rect, m, out);
    }

    let body_rect = egui::Rect::from_min_max(
        egui::pos2(inner_rect.min.x, cursor_y),
        inner_rect.max,
    );
    if body_rect.height() <= 2.0 {
        return;
    }

    // Dismiss open menu drawer if primary click landed outside both the drawer and trigger button.
    if menu_open {
        let drawer_max_h = (body_rect.height() - 6.0 * m.s).max(20.0);
        let drawer_rect = egui::Rect::from_min_size(
            body_rect.min,
            egui::vec2(body_rect.width(), drawer_max_h),
        );
        if was_menu_open {
            if let Some(press_pos) = pointer_pressed_pos {
                if !drawer_rect.contains(press_pos) && !filter_trigger_rect.contains(press_pos) {
                    if let Some(cs) = state.cards.get_mut(&view.id) {
                        cs.menu_open = false;
                    }
                    menu_open = false;
                }
            }
        }
        if menu_open {
            render_category_menu_drawer(ui, state, view, drawer_rect, m, out);
            return;
        }
    }

    // Reserve bottom jump-to-latest / unread bar when `!stick_bottom && !view.rows.is_empty()`.
    let show_follow_bar = !stick_bottom && !view.rows.is_empty() && body_rect.height() > m.follow_h * 1.8;
    let (list_rect, follow_rect_opt) = if show_follow_bar {
        let split_y = body_rect.max.y - m.follow_h;
        (
            egui::Rect::from_min_max(body_rect.min, egui::pos2(body_rect.max.x, split_y)),
            Some(egui::Rect::from_min_max(
                egui::pos2(body_rect.min.x, split_y),
                body_rect.max,
            )),
        )
    } else {
        (body_rect, None)
    };

    // ── 4. Event Stream Scroll List ──────────────────────────────────────────
    let mut list_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_list_ui", &view.id))
            .max_rect(list_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    list_ui.set_clip_rect(list_rect);

    let scroll_out = egui::ScrollArea::vertical()
        .id_salt(("wall_agent_stream", &view.id))
        .auto_shrink([false, false])
        .stick_to_bottom(stick_bottom)
        .max_height(list_rect.height())
        .show(&mut list_ui, |ui| {
            ui.set_min_width(list_rect.width());

            if view.has_earlier {
                render_load_earlier_btn(ui, &mut state.hit_rects, view, list_rect.width(), m, out);
            }

            render_empty_or_status_states(ui, &mut state.hit_rects, view, m, out);

            for row in &view.rows {
                render_event_row(ui, &mut state.hit_rects, &view.id, row, list_rect.width(), m, out);
            }

            if force_scroll_tail {
                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            }
        });

    // Update `stick_bottom` from user scroll position when content overflows viewport
    // (ignoring frames where expanding rows grew `content_h` without a user scroll).
    let content_h = scroll_out.content_size.y;
    let view_h = scroll_out.inner_rect.height();
    if let Some(cs) = state.cards.get_mut(&view.id) {
        let content_grew = content_h > cs.last_content_h + 1.0;
        cs.last_content_h = content_h;
        if content_h > view_h + 2.0 {
            let dist_from_bottom = content_h - (scroll_out.state.offset.y + view_h);
            let now_at_bottom = dist_from_bottom <= (24.0 * m.s).max(12.0);
            let allow_transition = now_at_bottom || !content_grew;
            if now_at_bottom != cs.stick_bottom && !force_scroll_tail && allow_transition {
                cs.stick_bottom = now_at_bottom;
                cs.last_host_stick_bottom = Some(now_at_bottom);
                if now_at_bottom {
                    cs.local_unread = 0;
                }
                state.stick_bottom_changes.push((view.id.clone(), now_at_bottom));
            }
        }
    }

    // ── 5. Bottom Unread / Jump-to-Latest Bar (`AgentViewOverlay.jsx:797-801`) ──
    if let Some(follow_rect) = follow_rect_opt {
        render_follow_tail_bar(ui, state, view, follow_rect, unread_count, m);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "header lays out status, session title, scrollback live chip, unread badge, L/R/T/B side buttons, and close button"
)]
fn render_header(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    card: &AgentViewCardInput,
    rect: egui::Rect,
    m: CardMetrics,
    focused: bool,
    unread_count: usize,
    out: &mut Vec<String>,
) {
    let view = &card.view;
    let s = m.s;
    let bg = if focused {
        rgb_hex(0x16402c)
    } else {
        rgb_hex(0x1e2536)
    };
    let border_col = if focused {
        rgb_hex(0x265c40)
    } else {
        rgb_hex(0x313b52)
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - m.hairline * 0.5),
            egui::pos2(rect.max.x, rect.max.y - m.hairline * 0.5),
        ],
        egui::Stroke::new(m.hairline, border_col),
    );

    let pad_x = (5.0 * s).max(3.0);
    let inner = rect.shrink2(egui::vec2(pad_x, 1.0));

    // Render right-pinned buttons first (`right_to_left`) so we know exact remaining width for the title.
    let mut hdr_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_hdr_rhs", &view.id))
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    hdr_ui.spacing_mut().item_spacing.x = (2.0 * s).max(1.0);

    // Close button `×`
    let close_fg = if focused {
        rgb_hex(0x8ef0b8)
    } else {
        rgb_hex(0xcbd3e1)
    };
    if mini_button(
        &mut hdr_ui,
        &mut state.hit_rects,
        &view.id,
        "close",
        "×",
        (12.5 * s).max(8.0),
        egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
        BtnColors {
            bg: egui::Color32::TRANSPARENT,
            fg: close_fg,
            border: egui::Color32::TRANSPARENT,
        },
        0.0,
        true,
        "Close Agent View (terminal keeps running)",
    )
    .clicked()
    {
        out.push(format!("wall.agent {} close", view.id));
    }

    // Side buttons (`L`, `R`, `T`, `B`) in reverse order because layout is right_to_left -> displays `L R T B`!
    if inner.width() >= 96.0 * s {
        for side in [
            AgentSide::Left,
            AgentSide::Right,
            AgentSide::Top,
            AgentSide::Bottom,
        ]
        .into_iter()
        .rev()
        {
            let active = view.side == side;
            let letter = match side {
                AgentSide::Left => "L",
                AgentSide::Right => "R",
                AgentSide::Top => "T",
                AgentSide::Bottom => "B",
            };
            let hit_key = format!("side-{}", side.as_str());
            let tip = format!("Attach Agent View on {}", side.as_str());
            if mini_button(
                &mut hdr_ui,
                &mut state.hit_rects,
                &view.id,
                &hit_key,
                letter,
                m.font_tiny,
                egui::vec2((2.5 * s).max(1.5), (0.8 * s).max(0.5)),
                if active {
                    BtnColors {
                        bg: rgb_hex(0x203c32),
                        fg: rgb_hex(0x95f3be),
                        border: rgb_hex(0x3d855f),
                    }
                } else {
                    BtnColors {
                        bg: rgb_hex(0x161d2e),
                        fg: rgb_hex(0x858fa3),
                        border: rgb_hex(0x2d384f),
                    }
                },
                m.hairline,
                true,
                &tip,
            )
            .clicked()
                && !active
            {
                out.push(format!("wall.agent.side {} {}", view.id, side.as_str()));
            }
        }
    }

    // Terminal scrollback live chip (`↑{view_offset} · live`)
    if view.view_offset > 0 && inner.width() >= 120.0 * s {
        let label = format!("↑{}·live", view.view_offset);
        if mini_button(
            &mut hdr_ui,
            &mut state.hit_rects,
            &view.id,
            "jump-live",
            &label,
            m.font_small,
            egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
            BtnColors {
                bg: rgb_hex(0x1a3026),
                fg: rgb_hex(0x8ef0b8),
                border: rgb_hex(0x356b4f),
            },
            m.hairline,
            true,
            "Return attached terminal from scrollback to live output",
        )
        .clicked()
        {
            out.push(format!("wall.agent.jump {} live", view.id));
        }
    }

    // Unread count pill (`+{unread}`) when scrolled up in the event stream
    if unread_count > 0 && inner.width() >= 110.0 * s {
        let label = format!("+{unread_count}");
        if mini_button(
            &mut hdr_ui,
            &mut state.hit_rects,
            &view.id,
            "unread-pill",
            &label,
            m.font_tiny,
            egui::vec2((3.0 * s).max(2.0), (0.8 * s).max(0.5)),
            BtnColors {
                bg: rgb_hex(0x173a29),
                fg: rgb_hex(0x93f2bc),
                border: rgb_hex(0x3d855f),
            },
            m.hairline,
            true,
            "Unread live events — click to jump to latest",
        )
        .clicked()
        {
            scroll_card_to_tail(state, &view.id);
        }
    }

    // Remaining left span of the header holds the status dot + session label (and drag handle).
    let right_used_min_x = hdr_ui.min_rect().min.x;
    let left_max_x = if hdr_ui.min_rect().width() > 0.0 {
        (right_used_min_x - 4.0 * s).max(inner.min.x)
    } else {
        inner.max.x
    };
    let title_rect = egui::Rect::from_min_max(
        egui::pos2(inner.min.x, rect.min.y),
        egui::pos2(left_max_x, rect.max.y),
    );

    if title_rect.width() > 4.0 {
        let sense = if card.terminal_screen_rect.is_some() {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::hover()
        };
        let drag_resp = hdr_ui.interact(title_rect, egui::Id::new(("av_hdr_drag", &view.id)), sense);
        state
            .hit_rects
            .insert(format!("{}:header", view.id), title_rect);

        if card.terminal_screen_rect.is_some()
            && state.drag.is_none()
            && (drag_resp.drag_started() || drag_resp.is_pointer_button_down_on())
        {
            let origin = hdr_ui
                .ctx()
                .input(|i| i.pointer.press_origin().or_else(|| i.pointer.interact_pos()));
            if let Some(pos) = origin.or_else(|| drag_resp.interact_pointer_pos()) {
                state.drag = Some(HeaderDragState {
                    id: view.id.clone(),
                    start_pos: pos,
                    dragging: false,
                    current_side: view.side,
                });
            }
        }

        let dot = status_dot(view);
        let font = egui::FontId::monospace(m.font_header);
        let dot_galley = painter.layout_no_wrap(dot.glyph.to_string(), font.clone(), dot.color);
        painter.galley(
            egui::pos2(title_rect.min.x, rect.center().y - dot_galley.size().y * 0.5),
            dot_galley.clone(),
            dot.color,
        );

        let (label_text, label_color, hover_tip) = if let Some(sid) = &view.session_id {
            let via_note = if view.session_via == Some(wall_core::agent_view::ResolveVia::Directory) {
                " (the only live agent in this terminal's directory)"
            } else {
                ""
            };
            (
                view.session_label.as_str(),
                if focused {
                    rgb_hex(0xb8f7d2)
                } else {
                    rgb_hex(0xd4d9e4)
                },
                format!("{} · {sid}{via_note} (status: {})", view.session_label, dot.label),
            )
        } else {
            (
                "no agent found",
                rgb_hex(0xf0b089),
                view.missing
                    .clone()
                    .unwrap_or_else(|| "no agent session for this terminal".to_string()),
            )
        };

        let text_start_x = title_rect.min.x + dot_galley.size().x + (4.0 * s).max(2.0);
        let avail_w = (title_rect.max.x - text_start_x).max(0.0);
        let truncated = truncate_to_width(&painter, label_text, &font, avail_w);
        if !truncated.is_empty() {
            let tg = painter.layout_no_wrap(truncated, font, label_color);
            painter.galley(
                egui::pos2(text_start_x, rect.center().y - tg.size().y * 0.5),
                tg,
                label_color,
            );
        }
        drag_resp.on_hover_text(hover_tip);
    }
}

fn render_toolbar(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    rect: egui::Rect,
    m: CardMetrics,
    menu_open: bool,
    out: &mut Vec<String>,
) -> egui::Rect {
    let s = m.s;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, rgb_hex(0x0e1224));
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - m.hairline * 0.5),
            egui::pos2(rect.max.x, rect.max.y - m.hairline * 0.5),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x263149)),
    );

    let pad_x = (5.0 * s).max(3.0);
    let inner = rect.shrink2(egui::vec2(pad_x, 1.0));
    let compact = inner.width() < 175.0 * s;

    // Right-side expand/collapse buttons first so they never clip.
    let mut right_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_toolbar_rhs", &view.id))
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    right_ui.spacing_mut().item_spacing.x = (3.0 * s).max(1.5);

    let filtered = !is_default_filter(&view.filters);
    let collapse_label = if compact { "−" } else { "− collapse" };
    if mini_button(
        &mut right_ui,
        &mut state.hit_rects,
        &view.id,
        "collapse-all",
        collapse_label,
        m.font_small,
        egui::vec2((4.5 * s).max(2.0), (1.5 * s).max(1.0)),
        if view.expanded_count > 0 {
            BtnColors {
                bg: rgb_hex(0x1b2640),
                fg: rgb_hex(0xdce5f5),
                border: rgb_hex(0x4a5f88),
            }
        } else {
            BtnColors {
                bg: rgb_hex(0x141b30),
                fg: rgb_hex(0xb8c2d4),
                border: rgb_hex(0x34415c),
            }
        },
        m.hairline,
        true,
        "Collapse all expanded events",
    )
    .clicked()
    {
        out.push(format!("wall.agent.collapse {} all", view.id));
    }

    let expand_label = if compact { "+" } else { "+ expand" };
    let expand_tip = if filtered {
        format!("Expand all visible ({}) events", view.filter_label)
    } else {
        "Expand all visible events".to_string()
    };
    if mini_button(
        &mut right_ui,
        &mut state.hit_rects,
        &view.id,
        "expand-all",
        expand_label,
        m.font_small,
        egui::vec2((4.5 * s).max(2.0), (1.5 * s).max(1.0)),
        BtnColors {
            bg: rgb_hex(0x141b30),
            fg: rgb_hex(0xb8c2d4),
            border: rgb_hex(0x34415c),
        },
        m.hairline,
        true,
        &expand_tip,
    )
    .clicked()
    {
        out.push(format!("wall.agent.expand {} all", view.id));
    }

    let left_max_x = (right_ui.min_rect().min.x - 4.0 * s).max(inner.min.x);
    let left_rect = egui::Rect::from_min_max(
        egui::pos2(inner.min.x, inner.min.y),
        egui::pos2(left_max_x, inner.max.y),
    );
    let mut left_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_toolbar_lhs", &view.id))
            .max_rect(left_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    left_ui.spacing_mut().item_spacing.x = (4.0 * s).max(2.0);

    let noise_on = view.filters.get("noise").copied().unwrap_or(false);
    let denom = if noise_on {
        view.total_count
    } else {
        view.standard_count
    };
    let count_text = if filtered && denom != view.visible_count {
        format!("{}/{}", view.visible_count, denom)
    } else {
        format!("{}", view.visible_count)
    };
    let arrow = if menu_open { "▴" } else { "▾" };
    let filter_name = if view.filter_label.is_empty() {
        "all actions"
    } else {
        view.filter_label.as_str()
    };
    let raw_trigger = format!("{arrow} {filter_name} {count_text}");
    let trigger_font = egui::FontId::monospace(m.font_small);
    let max_trigger_w = (left_rect.width() - if view.following { 38.0 * s } else { 2.0 }).max(24.0);
    let trigger_label = truncate_to_width(
        &painter,
        &raw_trigger,
        &trigger_font,
        (max_trigger_w - 10.0 * s).max(16.0),
    );

    let trigger_colors = if menu_open {
        BtnColors {
            bg: rgb_hex(0x1d2b49),
            fg: rgb_hex(0xe6f0ff),
            border: rgb_hex(0x6992d6),
        }
    } else if filtered {
        BtnColors {
            bg: rgb_hex(0x172540),
            fg: rgb_hex(0xe6f0ff),
            border: rgb_hex(0x4c74b4),
        }
    } else {
        BtnColors {
            bg: rgb_hex(0x151c33),
            fg: rgb_hex(0xd8dfec),
            border: rgb_hex(0x384562),
        }
    };

    let trigger_resp = mini_button(
        &mut left_ui,
        &mut state.hit_rects,
        &view.id,
        "toggle-filter-menu",
        &trigger_label,
        m.font_small,
        egui::vec2((5.0 * s).max(2.5), (1.5 * s).max(1.0)),
        trigger_colors,
        m.hairline,
        true,
        "Filter or expand/collapse specific action groups & categories",
    );
    let trigger_rect = trigger_resp.rect;
    if trigger_resp.clicked() {
        if let Some(cs) = state.cards.get_mut(&view.id) {
            cs.menu_open = !cs.menu_open;
        }
    }

    if view.following && left_ui.available_width() >= 30.0 * s {
        left_ui.label(
            egui::RichText::new("● live")
                .font(egui::FontId::monospace(m.font_tiny))
                .color(rgb_hex(0x6ee7a0)),
        );
    }

    trigger_rect
}

fn render_category_chips(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    rect: egui::Rect,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let s = m.s;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, rgb_hex(0x0b0f1f));
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - m.hairline * 0.5),
            egui::pos2(rect.max.x, rect.max.y - m.hairline * 0.5),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x1d263b)),
    );

    let inner = rect.shrink2(egui::vec2((4.0 * s).max(2.0), 1.0));
    let mut chip_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_chip_ui", &view.id))
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    chip_ui.set_clip_rect(inner);

    egui::ScrollArea::horizontal()
        .id_salt(("wall_agent_chips", &view.id))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(&mut chip_ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = (3.0 * s).max(1.5);
                let filtered = !is_default_filter(&view.filters);
                if filtered
                    && mini_button(
                        ui,
                        hits,
                        &view.id,
                        "chip-reset",
                        "all",
                        m.font_tiny,
                        egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
                        BtnColors {
                            bg: rgb_hex(0x193529),
                            fg: rgb_hex(0x95f3be),
                            border: rgb_hex(0x3a7a5c),
                        },
                        m.hairline,
                        true,
                        "Reset filter to show all standard categories",
                    )
                    .clicked()
                {
                    out.push(format!("wall.agent.filter {} reset", view.id));
                }

                for &cid in CATEGORY_IDS {
                    let count = view.category_counts.get(cid).copied().unwrap_or(0);
                    let enabled = view.filters.get(cid).copied().unwrap_or(false);
                    let is_non_default = if cid == "noise" { enabled } else { !enabled };
                    // Show categories present in this session, plus any category actively toggled away from default
                    // (and when empty session, show the core categories so chips are still inspectable).
                    let show_chip = count > 0
                        || is_non_default
                        || (view.total_count == 0 && matches!(cid, "prompt" | "say" | "read" | "edit" | "run"));
                    if !show_chip {
                        continue;
                    }

                    let hue = parse_css_hex(&category_hue(cid)).unwrap_or(rgb_hex(0x8b95a7));
                    let cat_meta = category_by_id(cid);
                    let label_word = cat_meta.map_or(cid, |c| c.label);
                    let text = if count > 0 {
                        format!("● {label_word} {count}")
                    } else {
                        format!("● {label_word}")
                    };
                    let solo = is_exact_filter(&view.filters, &[cid]);
                    let colors = if solo {
                        BtnColors {
                            bg: rgb_hex(0x1e3558),
                            fg: brighten(hue, 35),
                            border: hue,
                        }
                    } else if enabled {
                        BtnColors {
                            bg: rgb_hex(0x131a2e),
                            fg: hue,
                            border: with_alpha(hue, 0.55),
                        }
                    } else {
                        BtnColors {
                            bg: rgb_hex(0x0d1120),
                            fg: rgb_hex(0x5d667a),
                            border: rgb_hex(0x232c42),
                        }
                    };
                    let tip = format!(
                        "{} — click to toggle, right-click to solo",
                        cat_meta.map_or(cid, |c| c.title)
                    );
                    let resp = mini_button(
                        ui,
                        hits,
                        &view.id,
                        &format!("chip-{cid}"),
                        &text,
                        m.font_tiny,
                        egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
                        colors,
                        m.hairline,
                        true,
                        &tip,
                    );
                    if resp.clicked() {
                        out.push(format!("wall.agent.filter {} toggle {cid}", view.id));
                    } else if resp.secondary_clicked() {
                        if solo {
                            out.push(format!("wall.agent.filter {} reset", view.id));
                        } else {
                            out.push(format!("wall.agent.filter {} only {cid}", view.id));
                        }
                    }
                }
            });
        });
}

fn sum_counts(counts: &BTreeMap<String, usize>, ids: &[&str]) -> usize {
    ids.iter()
        .map(|cid| counts.get(*cid).copied().unwrap_or(0))
        .sum()
}

fn render_category_menu_drawer(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    rect: egui::Rect,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let s = m.s;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, rgb_hex(0x0d1223));
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(m.hairline, rgb_hex(0x415378)),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink(m.hairline);
    let top_h = (22.0 * s).max(15.0).min(inner.height());
    let top_rect = egui::Rect::from_min_size(inner.min, egui::vec2(inner.width(), top_h));
    painter.rect_filled(top_rect, 0.0, rgb_hex(0x141c34));
    painter.line_segment(
        [
            egui::pos2(top_rect.min.x, top_rect.max.y),
            egui::pos2(top_rect.max.x, top_rect.max.y),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x283450)),
    );

    let top_inner = top_rect.shrink2(egui::vec2((5.0 * s).max(3.0), 1.0));
    let mut top_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_drawer_top", &view.id))
            .max_rect(top_inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    top_ui.spacing_mut().item_spacing.x = (3.0 * s).max(1.5);

    if mini_button(
        &mut top_ui,
        &mut state.hit_rects,
        &view.id,
        "close-filter-menu",
        "done ▲",
        m.font_tiny,
        egui::vec2((4.0 * s).max(2.0), (1.0 * s).max(0.5)),
        BtnColors {
            bg: rgb_hex(0x151d33),
            fg: rgb_hex(0x9fb0ca),
            border: rgb_hex(0x313e59),
        },
        m.hairline,
        true,
        "Close menu",
    )
    .clicked()
    {
        if let Some(cs) = state.cards.get_mut(&view.id) {
            cs.menu_open = false;
        }
    }

    let default_active = is_default_filter(&view.filters);
    if mini_button(
        &mut top_ui,
        &mut state.hit_rects,
        &view.id,
        "menu-show-all",
        "show all",
        m.font_tiny,
        egui::vec2((4.0 * s).max(2.0), (1.0 * s).max(0.5)),
        if !default_active {
            BtnColors {
                bg: rgb_hex(0x193529),
                fg: rgb_hex(0x95f3be),
                border: rgb_hex(0x3a7a5c),
            }
        } else {
            BtnColors {
                bg: rgb_hex(0x151d33),
                fg: rgb_hex(0x9fb0ca),
                border: rgb_hex(0x313e59),
            }
        },
        m.hairline,
        true,
        "Reset filter to show all standard categories",
    )
    .clicked()
    {
        out.push(format!("wall.agent.filter {} reset", view.id));
    }

    let title_max_x = (top_ui.min_rect().min.x - 4.0 * s).max(top_inner.min.x);
    let title_font = egui::FontId::monospace(m.font_small);
    let title_str = truncate_to_width(
        &painter,
        "Action groups & subsets",
        &title_font,
        title_max_x - top_inner.min.x,
    );
    if !title_str.is_empty() {
        let g = painter.layout_no_wrap(title_str, title_font, rgb_hex(0x93a3bf));
        painter.galley(
            egui::pos2(top_inner.min.x, top_rect.center().y - g.size().y * 0.5),
            g,
            rgb_hex(0x93a3bf),
        );
    }

    let scroll_rect = egui::Rect::from_min_max(
        egui::pos2(inner.min.x, top_rect.max.y + m.hairline),
        inner.max,
    );
    if scroll_rect.height() <= 4.0 {
        return;
    }

    let mut drawer_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_drawer_body", &view.id))
            .max_rect(scroll_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    drawer_ui.set_clip_rect(scroll_rect);

    egui::ScrollArea::vertical()
        .id_salt(("wall_agent_drawer", &view.id))
        .auto_shrink([false, false])
        .max_height(scroll_rect.height())
        .show(&mut drawer_ui, |ui| {
            for group in CATEGORY_GROUPS_TREE {
                render_menu_top_group(ui, state, view, group, scroll_rect.width(), m, out);
            }
        });
}

fn render_menu_top_group(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    group: &CategoryGroupNode,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let is_folded = state
        .cards
        .get(&view.id)
        .is_some_and(|cs| cs.folded_groups.contains(group.id));
    render_menu_group_row(
        ui,
        state,
        view,
        group.id,
        group.label,
        group.token,
        group.categories,
        0,
        is_folded,
        width,
        m,
        out,
    );
    if is_folded {
        return;
    }
    for sub in group.subgroups {
        render_menu_subgroup(ui, state, view, sub, width, m, out);
    }
    for &cid in group.direct_categories {
        render_menu_category_row(ui, &mut state.hit_rects, view, cid, 1, width, m, out);
    }
}

fn render_menu_subgroup(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    sub: &CategorySubgroup,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let is_folded = state
        .cards
        .get(&view.id)
        .is_some_and(|cs| cs.folded_groups.contains(sub.id));
    render_menu_group_row(
        ui,
        state,
        view,
        sub.id,
        sub.label,
        sub.token,
        sub.categories,
        1,
        is_folded,
        width,
        m,
        out,
    );
    if !is_folded {
        for &cid in sub.categories {
            render_menu_category_row(ui, &mut state.hit_rects, view, cid, 2, width, m, out);
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "menu group row binds hierarchy depth, fold state, tri-state checkbox, and subset only/exp/col buttons"
)]
fn render_menu_group_row(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    group_id: &str,
    label: &str,
    token: &str,
    categories: &[&str],
    depth: usize,
    is_folded: bool,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let s = m.s;
    let row_h = (20.0 * s).max(14.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
    let bg = if depth == 0 {
        rgb_hex(0x12182d)
    } else {
        rgb_hex(0x0f1527)
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - m.hairline * 0.5),
            egui::pos2(rect.max.x, rect.max.y - m.hairline * 0.5),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x1a2236)),
    );

    let indent = (4.0 + depth as f32 * 10.0) * s;
    let inner = egui::Rect::from_min_max(
        egui::pos2(rect.min.x + indent, rect.min.y),
        egui::pos2(rect.max.x - 4.0 * s, rect.max.y),
    );

    // Right-side `only` / `exp` / `col` buttons
    let mut right_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_mgrp_rhs", &view.id, group_id))
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    right_ui.spacing_mut().item_spacing.x = (2.0 * s).max(1.0);
    render_subset_action_btns(
        &mut right_ui,
        &mut state.hit_rects,
        view,
        token,
        categories,
        m,
        out,
    );

    let left_max_x = (right_ui.min_rect().min.x - 4.0 * s).max(inner.min.x);
    let left_rect = egui::Rect::from_min_max(inner.min, egui::pos2(left_max_x, inner.max.y));
    let mut left_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_mgrp_lhs", &view.id, group_id))
            .max_rect(left_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    left_ui.spacing_mut().item_spacing.x = (4.0 * s).max(2.0);

    let fold_glyph = if is_folded { "▸" } else { "▾" };
    if mini_button(
        &mut left_ui,
        &mut state.hit_rects,
        &view.id,
        &format!("fold-{group_id}"),
        fold_glyph,
        m.font_small,
        egui::vec2((2.0 * s).max(1.0), (1.0 * s).max(0.5)),
        BtnColors {
            bg: egui::Color32::TRANSPARENT,
            fg: rgb_hex(0x8b97ae),
            border: egui::Color32::TRANSPARENT,
        },
        0.0,
        true,
        &format!(
            "{} {label}",
            if is_folded { "Unfold" } else { "Fold" }
        ),
    )
    .clicked()
    {
        if let Some(cs) = state.cards.get_mut(&view.id) {
            if !cs.folded_groups.remove(group_id) {
                cs.folded_groups.insert(group_id.to_string());
            }
        }
    }

    let sel = group_selection_state(&view.filters, categories);
    let count = sum_counts(&view.category_counts, categories);
    let hit_rect = egui::Rect::from_min_max(
        egui::pos2(left_ui.cursor().min.x, rect.min.y),
        egui::pos2(left_max_x, rect.max.y),
    );
    if hit_rect.width() > 4.0 {
        let hit_resp = left_ui.interact(
            hit_rect,
            egui::Id::new(("av_grp_toggle", &view.id, group_id)),
            egui::Sense::click(),
        );
        state
            .hit_rects
            .insert(format!("{}:group-{token}", view.id), hit_rect);
        draw_checkbox_and_label(
            &painter,
            hit_rect,
            sel,
            None,
            label,
            count,
            if sel == GroupSelection::None {
                rgb_hex(0x8893a8)
            } else {
                rgb_hex(0xe6ebf5)
            },
            m,
        );
        if hit_resp
            .on_hover_text(format!("Toggle all categories in {label}"))
            .clicked()
        {
            let mode = if sel == GroupSelection::All { "off" } else { "on" };
            out.push(format!("wall.agent.filter {} {mode} {token}", view.id));
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "menu category row binds indentation depth, checkbox state, hue swatch, count, and subset buttons"
)]
fn render_menu_category_row(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    cat_id: &str,
    depth: usize,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let Some(cat) = category_by_id(cat_id) else {
        return;
    };
    let s = m.s;
    let row_h = (19.0 * s).max(13.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let indent = (8.0 + depth as f32 * 10.0) * s;
    let inner = egui::Rect::from_min_max(
        egui::pos2(rect.min.x + indent, rect.min.y),
        egui::pos2(rect.max.x - 4.0 * s, rect.max.y),
    );

    let mut right_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_mcat_rhs", &view.id, cat_id))
            .max_rect(inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    right_ui.spacing_mut().item_spacing.x = (2.0 * s).max(1.0);
    render_subset_action_btns(&mut right_ui, hits, view, cat_id, &[cat_id], m, out);

    let left_max_x = (right_ui.min_rect().min.x - 4.0 * s).max(inner.min.x);
    let hit_rect = egui::Rect::from_min_max(inner.min, egui::pos2(left_max_x, inner.max.y));
    if hit_rect.width() > 4.0 {
        let hit_resp = ui.interact(
            hit_rect,
            egui::Id::new(("av_cat_toggle", &view.id, cat_id)),
            egui::Sense::click(),
        );
        hits.insert(format!("{}:menu-cat-{cat_id}", view.id), hit_rect);
        let checked = view.filters.get(cat_id).copied().unwrap_or(false);
        let hue = parse_css_hex(&category_hue(cat_id)).unwrap_or(rgb_hex(0x8b95a7));
        let count = view.category_counts.get(cat_id).copied().unwrap_or(0);
        draw_checkbox_and_label(
            &painter,
            hit_rect,
            if checked {
                GroupSelection::All
            } else {
                GroupSelection::None
            },
            Some(hue),
            cat.short,
            count,
            if checked {
                rgb_hex(0xdce2ee)
            } else {
                rgb_hex(0x7f8a9f)
            },
            m,
        );
        if hit_resp
            .on_hover_text(format!("{} — click to show or hide", cat.title))
            .clicked()
        {
            out.push(format!("wall.agent.filter {} toggle {cat_id}", view.id));
        }
    }
}

fn render_subset_action_btns(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    token: &str,
    category_ids: &[&str],
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let s = m.s;
    let pad = egui::vec2((3.0 * s).max(1.5), (0.8 * s).max(0.5));
    let base = BtnColors {
        bg: rgb_hex(0x151d33),
        fg: rgb_hex(0x9fb0ca),
        border: rgb_hex(0x313e59),
    };

    if mini_button(
        ui,
        hits,
        &view.id,
        &format!("col-{token}"),
        "col",
        m.font_tiny,
        pad,
        base,
        m.hairline,
        true,
        &format!("Collapse {token}"),
    )
    .clicked()
    {
        out.push(format!("wall.agent.collapse {} {token}", view.id));
    }

    if mini_button(
        ui,
        hits,
        &view.id,
        &format!("exp-{token}"),
        "exp",
        m.font_tiny,
        pad,
        base,
        m.hairline,
        true,
        &format!("Expand all {token}"),
    )
    .clicked()
    {
        if let Ok(expanded_ids) = expand_category_tokens(&[token]) {
            let any_on = expanded_ids
                .iter()
                .any(|cid| view.filters.get(*cid).copied().unwrap_or(false));
            if !any_on {
                out.push(format!("wall.agent.filter {} on {token}", view.id));
            }
        }
        out.push(format!("wall.agent.expand {} {token}", view.id));
    }

    let solo = is_exact_filter(&view.filters, category_ids);
    if mini_button(
        ui,
        hits,
        &view.id,
        &format!("only-{token}"),
        "only",
        m.font_tiny,
        pad,
        if solo {
            BtnColors {
                bg: rgb_hex(0x1e3558),
                fg: rgb_hex(0xe6f0ff),
                border: rgb_hex(0x5c8be0),
            }
        } else {
            base
        },
        m.hairline,
        true,
        if solo {
            "Showing only this subset — click to reset"
        } else {
            "Show only this subset"
        },
    )
    .clicked()
    {
        if solo {
            out.push(format!("wall.agent.filter {} reset", view.id));
        } else {
            out.push(format!("wall.agent.filter {} only {token}", view.id));
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "paints tri-state checkbox, optional category hue swatch, truncated label, and right-aligned count in one pass"
)]
fn draw_checkbox_and_label(
    painter: &egui::Painter,
    rect: egui::Rect,
    sel: GroupSelection,
    swatch: Option<egui::Color32>,
    label: &str,
    count: usize,
    label_color: egui::Color32,
    m: CardMetrics,
) {
    let s = m.s;
    let box_sz = (11.0 * s).max(8.0);
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, rect.center().y - box_sz * 0.5),
        egui::vec2(box_sz, box_sz),
    );
    let (bg, border, mark, mark_col) = match sel {
        GroupSelection::All => (rgb_hex(0x1e3a5f), rgb_hex(0x5b8bd4), "✓", rgb_hex(0x93c5fd)),
        GroupSelection::Some => (rgb_hex(0x192a44), rgb_hex(0x5b8bd4), "–", rgb_hex(0x7fb3ee)),
        GroupSelection::None => (rgb_hex(0x0b0f1e), rgb_hex(0x43506a), "", rgb_hex(0x7fb3ee)),
    };
    painter.rect_filled(box_rect, 0.0, bg);
    painter.rect_stroke(
        box_rect,
        0.0,
        egui::Stroke::new(m.hairline, border),
        egui::StrokeKind::Inside,
    );
    if !mark.is_empty() {
        let mg = painter.layout_no_wrap(
            mark.to_string(),
            egui::FontId::monospace((8.5 * s).max(6.0)),
            mark_col,
        );
        painter.galley(
            egui::pos2(
                box_rect.center().x - mg.size().x * 0.5,
                box_rect.center().y - mg.size().y * 0.5,
            ),
            mg,
            mark_col,
        );
    }

    let mut x = box_rect.max.x + (4.0 * s).max(2.0);
    if let Some(dot_col) = swatch {
        let d = (7.0 * s).max(4.5);
        let d_rect = egui::Rect::from_min_size(
            egui::pos2(x, rect.center().y - d * 0.5),
            egui::vec2(d, d),
        );
        painter.rect_filled(d_rect, 0.0, dot_col);
        x = d_rect.max.x + (4.0 * s).max(2.0);
    }

    let count_font = egui::FontId::monospace(m.font_tiny);
    let count_galley = painter.layout_no_wrap(format!("{count}"), count_font, rgb_hex(0x6e798f));
    let count_x = (rect.max.x - count_galley.size().x - 2.0 * s).max(x);
    if count_x > x + 6.0 {
        painter.galley(
            egui::pos2(count_x, rect.center().y - count_galley.size().y * 0.5),
            count_galley,
            rgb_hex(0x6e798f),
        );
    }

    let label_font = egui::FontId::monospace(m.font_small);
    let truncated = truncate_to_width(painter, label, &label_font, (count_x - x - 3.0 * s).max(0.0));
    if !truncated.is_empty() {
        let lg = painter.layout_no_wrap(truncated, label_font, label_color);
        painter.galley(
            egui::pos2(x, rect.center().y - lg.size().y * 0.5),
            lg,
            label_color,
        );
    }
}

fn render_load_earlier_btn(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let label = if view.loading_earlier {
        "loading earlier…"
    } else {
        "↑ load earlier events"
    };
    let h = (20.0 * m.s).max(14.0);
    let sense = if view.loading_earlier {
        egui::Sense::hover()
    } else {
        egui::Sense::click()
    };
    let (_, rect) = ui.allocate_space(egui::vec2(width, h));
    let resp = ui.interact(rect, egui::Id::new(("av_load_earlier", &view.id)), sense);
    hits.insert(format!("{}:load-earlier", view.id), rect);

    if ui.is_rect_visible(rect) {
        let bg = if !view.loading_earlier && resp.hovered() {
            brighten(rgb_hex(0x11172c), 16)
        } else {
            rgb_hex(0x11172c)
        };
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);
        painter.line_segment(
            [
                egui::pos2(rect.min.x, rect.max.y - m.hairline * 0.5),
                egui::pos2(rect.max.x, rect.max.y - m.hairline * 0.5),
            ],
            egui::Stroke::new(m.hairline, rgb_hex(0x242f47)),
        );
        let g = painter.layout_no_wrap(
            label.to_string(),
            egui::FontId::monospace(m.font_small),
            rgb_hex(0x96a2b8),
        );
        painter.galley(
            egui::pos2(
                rect.center().x - g.size().x * 0.5,
                rect.center().y - g.size().y * 0.5,
            ),
            g,
            rgb_hex(0x96a2b8),
        );
    }

    if !view.loading_earlier && resp.clicked() {
        out.push(format!("wall.agent.earlier {}", view.id));
    }
}

fn render_empty_or_status_states(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    view: &AgentViewDescriptor,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let pad = (10.0 * m.s).max(5.0);

    if let Some(err) = &view.error {
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(pad as i8))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("agent stream unavailable")
                        .font(egui::FontId::monospace(m.font_body))
                        .color(rgb_hex(0xf0b089)),
                );
                ui.label(
                    egui::RichText::new(err)
                        .font(egui::FontId::monospace(m.font_small))
                        .color(rgb_hex(0x7c8596)),
                );
            });
        return;
    }

    if view.session_id.is_none() {
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(pad as i8))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("no agent found for this terminal")
                        .font(egui::FontId::monospace(m.font_body))
                        .color(rgb_hex(0xf0b089)),
                );
                let reason = view
                    .missing
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("waiting for the agent session list…");
                ui.label(
                    egui::RichText::new(reason)
                        .font(egui::FontId::monospace(m.font_small))
                        .color(rgb_hex(0x7c8596)),
                );
            });
        return;
    }

    if !view.rows.is_empty() {
        return;
    }

    egui::Frame::NONE
        .inner_margin(egui::Margin::same(pad as i8))
        .show(ui, |ui| {
            if view.loading {
                ui.label(
                    egui::RichText::new("reading transcript…")
                        .font(egui::FontId::monospace(m.font_body))
                        .color(rgb_hex(0x7c8596)),
                );
            } else if view.total_count > 0 {
                ui.label(
                    egui::RichText::new(format!("no events match \"{}\"", view.filter_label))
                        .font(egui::FontId::monospace(m.font_body))
                        .color(rgb_hex(0x7c8596)),
                );
                if mini_button(
                    ui,
                    hits,
                    &view.id,
                    "reset-empty-filter",
                    "show all actions",
                    m.font_small,
                    egui::vec2((5.0 * m.s).max(2.5), (2.0 * m.s).max(1.0)),
                    BtnColors {
                        bg: rgb_hex(0x151e36),
                        fg: rgb_hex(0x93c5fd),
                        border: rgb_hex(0x334770),
                    },
                    m.hairline,
                    true,
                    "Reset filter to show all standard categories",
                )
                .clicked()
                {
                    out.push(format!("wall.agent.filter {} reset", view.id));
                }
            } else {
                ui.label(
                    egui::RichText::new("waiting for first event…")
                        .font(egui::FontId::monospace(m.font_body))
                        .color(rgb_hex(0x7c8596)),
                );
            }
        });
}

fn render_event_row(
    ui: &mut egui::Ui,
    hits: &mut BTreeMap<String, egui::Rect>,
    term_id: &str,
    row: &AgentEventRow,
    width: f32,
    m: CardMetrics,
    out: &mut Vec<String>,
) {
    let s = m.s;
    let sum = &row.summary;
    let alpha = if sum.abandoned { 0.48 } else { 1.0 };
    let hue = with_alpha(
        parse_css_hex(&sum.hue).unwrap_or(rgb_hex(0x8b95a7)),
        alpha,
    );

    let (line_rect, _) = ui.allocate_exact_size(egui::vec2(width, m.row_h), egui::Sense::hover());
    let row_bg = if row.jumped {
        egui::Color32::from_rgba_unmultiplied(0x6e, 0xe7, 0xa0, 24)
    } else if row.expanded {
        rgb_hex(0x0f1328)
    } else {
        egui::Color32::TRANSPARENT
    };
    let painter = ui.painter_at(line_rect);
    if row_bg != egui::Color32::TRANSPARENT {
        painter.rect_filled(line_rect, 0.0, row_bg);
    }

    // Left 3pt hue bar
    let bar_w = (3.0 * s).max(2.0);
    painter.rect_filled(
        egui::Rect::from_min_max(
            line_rect.min,
            egui::pos2(line_rect.min.x + bar_w, line_rect.max.y),
        ),
        0.0,
        hue,
    );

    // Right-aligned `show`/`hide` + `jump` buttons
    let actions_inner = egui::Rect::from_min_max(
        egui::pos2(line_rect.min.x + bar_w + 3.0 * s, line_rect.min.y),
        egui::pos2(line_rect.max.x - (4.0 * s).max(2.0), line_rect.max.y),
    );
    let mut act_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("av_row_rhs", term_id, &sum.id))
            .max_rect(actions_inner)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    act_ui.spacing_mut().item_spacing.x = (2.0 * s).max(1.0);

    let jump_tip = if row.exact_anchor {
        "Jump attached terminal to this event"
    } else {
        "Jump attached terminal near this event"
    };
    if mini_button(
        &mut act_ui,
        hits,
        term_id,
        &format!("jump-{}", sum.id),
        "jump",
        m.font_small,
        egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
        if row.jumped {
            BtnColors {
                bg: rgb_hex(0x173a29),
                fg: rgb_hex(0x93f2bc),
                border: rgb_hex(0x3d855f),
            }
        } else {
            BtnColors {
                bg: rgb_hex(0x141b30),
                fg: rgb_hex(0xa8b4c9),
                border: rgb_hex(0x323f59),
            }
        },
        m.hairline,
        true,
        jump_tip,
    )
    .clicked()
    {
        out.push(format!("wall.agent.jump {term_id} {}", sum.id));
    }

    let show_label = if row.expanded { "hide" } else { "show" };
    let show_clicked = mini_button(
        &mut act_ui,
        hits,
        term_id,
        &format!("show-{}", sum.id),
        show_label,
        m.font_small,
        egui::vec2((3.5 * s).max(2.0), (1.8 * s).max(1.0)),
        if row.expanded {
            BtnColors {
                bg: rgb_hex(0x202e4c),
                fg: rgb_hex(0xe6edf8),
                border: rgb_hex(0x5672a8),
            }
        } else {
            BtnColors {
                bg: rgb_hex(0x141b30),
                fg: rgb_hex(0xa8b4c9),
                border: rgb_hex(0x323f59),
            }
        },
        m.hairline,
        true,
        if row.expanded {
            "Collapse event details"
        } else {
            "Expand event details"
        },
    )
    .clicked();

    // Optional `stat` badge immediately left of the buttons
    let mut text_right_x = (act_ui.min_rect().min.x - 4.0 * s).max(actions_inner.min.x);
    if !sum.stat.is_empty() && (text_right_x - actions_inner.min.x) > 72.0 * s {
        let stat_col = with_alpha(
            if sum.status == "error" {
                rgb_hex(0xf87171)
            } else {
                rgb_hex(0x858fa3)
            },
            alpha,
        );
        let sg = painter.layout_no_wrap(
            sum.stat.clone(),
            egui::FontId::monospace(m.font_small),
            stat_col,
        );
        if text_right_x - sg.size().x > actions_inner.min.x + 42.0 * s {
            let sx = text_right_x - sg.size().x;
            painter.galley(
                egui::pos2(sx, line_rect.center().y - sg.size().y * 0.5),
                sg,
                stat_col,
            );
            text_right_x = sx - 4.0 * s;
        }
    }

    // Clickable row body (`tag` + `summary`)
    let body_hit = egui::Rect::from_min_max(
        egui::pos2(line_rect.min.x, line_rect.min.y),
        egui::pos2(text_right_x, line_rect.max.y),
    );
    let row_resp = ui.interact(
        body_hit,
        egui::Id::new(("av_row_hit", term_id, &sum.id)),
        egui::Sense::click(),
    );
    hits.insert(format!("{term_id}:row-{}", sum.id), body_hit);

    if row_resp.hovered() && row_bg == egui::Color32::TRANSPARENT {
        painter.rect_filled(
            body_hit,
            0.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 11),
        );
    }

    let tag_x = line_rect.min.x + bar_w + (4.0 * s).max(2.0);
    let tag_max_w = (58.0 * s).min((text_right_x - tag_x) * 0.45).max(18.0);
    let tag_font = egui::FontId::monospace((10.0 * s).max(6.5));
    let tag_str = truncate_to_width(&painter, &sum.tag, &tag_font, tag_max_w);
    let tag_galley = painter.layout_no_wrap(tag_str, tag_font, hue);
    let tag_col_w = tag_galley.size().x.max((34.0 * s).min(tag_max_w));
    painter.galley(
        egui::pos2(tag_x, line_rect.center().y - tag_galley.size().y * 0.5),
        tag_galley,
        hue,
    );

    let sum_x = tag_x + tag_col_w + (4.0 * s).max(2.0);
    let sum_avail_w = (text_right_x - sum_x).max(0.0);
    if sum_avail_w > 4.0 {
        let sum_col = with_alpha(
            if sum.status == "error" {
                rgb_hex(0xf87171)
            } else if sum.event_kind == "think" {
                rgb_hex(0x9aa3b8)
            } else {
                rgb_hex(0xd4d9e4)
            },
            alpha,
        );
        let sum_font = egui::FontId::monospace(m.font_body);
        let sum_str = truncate_to_width(&painter, &sum.summary, &sum_font, sum_avail_w);
        let sg = painter.layout_no_wrap(sum_str, sum_font, sum_col);
        let sy = line_rect.center().y - sg.size().y * 0.5;
        let sw = sg.size().x;
        let sh = sg.size().y;
        painter.galley(egui::pos2(sum_x, sy), sg, sum_col);
        if sum.abandoned && sw > 1.0 {
            painter.line_segment(
                [
                    egui::pos2(sum_x, sy + sh * 0.52),
                    egui::pos2(sum_x + sw, sy + sh * 0.52),
                ],
                egui::Stroke::new(1.0, sum_col),
            );
        }
    }

    if show_clicked
        || row_resp
            .on_hover_text(format!("{}: {}", sum.tag, sum.summary))
            .clicked()
    {
        out.push(format!("wall.agent.show {term_id} {} toggle", sum.id));
    }

    // Expanded detail drawer (`AgentViewOverlay.jsx:433-456`)
    if row.expanded {
        if let Some(detail) = &row.detail {
            egui::Frame::NONE
                .fill(rgb_hex(0x080b19))
                .inner_margin(egui::Margin {
                    left: (9.0 * s).max(4.0) as i8,
                    right: (6.0 * s).max(3.0) as i8,
                    top: (4.0 * s).max(2.0) as i8,
                    bottom: (5.0 * s).max(2.0) as i8,
                })
                .show(ui, |ui| {
                    ui.set_min_width((width - 15.0 * s).max(10.0));

                    if !detail.target.is_empty() || !detail.facts.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            if !detail.target.is_empty() {
                                ui.label(
                                    egui::RichText::new(&detail.target)
                                        .font(egui::FontId::monospace(m.font_small))
                                        .color(rgb_hex(0x93c5fd)),
                                );
                            }
                            if !detail.facts.is_empty() {
                                ui.label(
                                    egui::RichText::new(detail.facts.join(" · "))
                                        .font(egui::FontId::monospace(m.font_small))
                                        .color(rgb_hex(0x788296)),
                                );
                            }
                        });
                    }

                    if !detail.detail.is_empty() && detail.detail != sum.summary {
                        egui::Frame::NONE
                            .fill(rgb_hex(0x10162b))
                            .stroke(egui::Stroke::new(m.hairline, rgb_hex(0x25304c)))
                            .inner_margin(egui::Margin::symmetric(
                                (4.0 * s).max(2.0) as i8,
                                (3.0 * s).max(1.0) as i8,
                            ))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&detail.detail)
                                            .font(egui::FontId::monospace(m.font_code))
                                            .color(rgb_hex(0xc4ccd8)),
                                    )
                                    .wrap(),
                                );
                            });
                    }

                    if !detail.body.is_empty() {
                        egui::Frame::NONE
                            .fill(rgb_hex(0x0b0f1f))
                            .stroke(egui::Stroke::new(m.hairline, rgb_hex(0x202a44)))
                            .inner_margin(egui::Margin::symmetric(
                                (4.0 * s).max(2.0) as i8,
                                (3.0 * s).max(1.0) as i8,
                            ))
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt(("av_detail_body", term_id, &sum.id))
                                    .max_height((220.0 * s).max(60.0))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&detail.body)
                                                    .font(egui::FontId::monospace(m.font_code))
                                                    .color(rgb_hex(0xcfd6e3)),
                                            )
                                            .wrap(),
                                        );
                                    });
                            });
                    } else if detail.detail.is_empty() {
                        ui.label(
                            egui::RichText::new("(no output text)")
                                .italics()
                                .font(egui::FontId::monospace(m.font_code))
                                .color(rgb_hex(0x5d6574)),
                        );
                    }

                    if detail.truncated {
                        let more_label = format!("show all {} lines", detail.total_lines);
                        if mini_button(
                            ui,
                            hits,
                            term_id,
                            &format!("show-full-{}", sum.id),
                            &more_label,
                            m.font_small,
                            egui::vec2((5.0 * s).max(2.5), (1.5 * s).max(1.0)),
                            BtnColors {
                                bg: rgb_hex(0x151e36),
                                fg: rgb_hex(0x93c5fd),
                                border: rgb_hex(0x334770),
                            },
                            m.hairline,
                            true,
                            "Load full uncapped output",
                        )
                        .clicked()
                        {
                            out.push(format!("wall.agent.show {term_id} {} full", sum.id));
                        }
                    }
                });
        }
    }

    // Bottom divider under the event block (`#192138`)
    let end_y = ui.min_rect().max.y;
    ui.painter().line_segment(
        [
            egui::pos2(line_rect.min.x, end_y),
            egui::pos2(line_rect.max.x, end_y),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x192138)),
    );
}

fn scroll_card_to_tail(state: &mut AgentViewPanelState, id: &str) {
    let cs = state.cards.entry(id.to_string()).or_default();
    let changed = !cs.stick_bottom || cs.local_unread > 0;
    cs.stick_bottom = true;
    cs.last_host_stick_bottom = Some(true);
    cs.local_unread = 0;
    cs.scroll_to_tail_next = true;
    if changed {
        state.stick_bottom_changes.push((id.to_string(), true));
    }
}

fn render_follow_tail_bar(
    ui: &mut egui::Ui,
    state: &mut AgentViewPanelState,
    view: &AgentViewDescriptor,
    rect: egui::Rect,
    unread_count: usize,
    m: CardMetrics,
) {
    let bar_resp = ui.interact(
        rect,
        egui::Id::new(("av_follow_tail_bar", &view.id)),
        egui::Sense::click(),
    );
    state
        .hit_rects
        .insert(format!("{}:follow-tail", view.id), rect);

    let hovered = bar_resp.hovered();
    let bg = if hovered {
        brighten(rgb_hex(0x132b20), 16)
    } else {
        rgb_hex(0x132b20)
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.min.y + m.hairline * 0.5),
            egui::pos2(rect.max.x, rect.min.y + m.hairline * 0.5),
        ],
        egui::Stroke::new(m.hairline, rgb_hex(0x2d664c)),
    );

    let text = if unread_count > 0 {
        format!("↓ follow live stream · {unread_count} new")
    } else {
        "↓ follow live stream".to_string()
    };
    let font = egui::FontId::monospace(m.font_small);
    let trimmed = truncate_to_width(&painter, &text, &font, (rect.width() - 8.0 * m.s).max(8.0));
    let g = painter.layout_no_wrap(trimmed, font, rgb_hex(0x8ef0b8));
    painter.galley(
        egui::pos2(
            rect.center().x - g.size().x * 0.5,
            rect.center().y - g.size().y * 0.5,
        ),
        g,
        rgb_hex(0x8ef0b8),
    );

    if bar_resp.clicked() {
        scroll_card_to_tail(state, &view.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wall_core::agent_view::{
        default_filters, describe_active_filter, format_expanded_event, summarize_event_row,
        AgentSession, EventRecord, ExpandOptions, FileSnapshot, ResolveVia,
    };
    use wall_core::commands::{
        register_wall_commands, AgentBulkExpandResult, AgentEarlierResult, AgentJumpResult,
        AgentOpenResult, AgentSideResult, AgentToggleEventResult, AttachOutcome, BusView,
        FullscreenStatus, GroupSummary, MenuTarget, PanelKind, PanelPatch, PanelPlacement,
        ScreenBox, WallApi, WallSnapshot,
    };
    use wall_core::router::{CommandReply, CommandRouter};

    fn sample_descriptor(id: &str) -> AgentViewDescriptor {
        let records = vec![
            EventRecord {
                id: "e-prompt".to_string(),
                event_kind: "prompt".to_string(),
                at: 100,
                action: "prompt".to_string(),
                tag: Some("you".to_string()),
                origin: Some("human".to_string()),
                result: "Build the world-anchored Agent View card".to_string(),
                ..EventRecord::default()
            },
            EventRecord {
                id: "e-think".to_string(),
                event_kind: "think".to_string(),
                at: 200,
                action: "think".to_string(),
                result: "Checking small-scale typography and hairline strokes\nLine 2".to_string(),
                ..EventRecord::default()
            },
            EventRecord {
                id: "e-read".to_string(),
                event_kind: "tool".to_string(),
                at: 300,
                action: "read".to_string(),
                name: Some("Read".to_string()),
                target: "crates/wall-core/src/agent_views.rs".to_string(),
                status: Some("ok".to_string()),
                file: Some(FileSnapshot {
                    text: "pub struct AgentViewDescriptor {\n    pub id: String,\n}".to_string(),
                    start: 755,
                    total: Some(4359),
                }),
                ..EventRecord::default()
            },
            EventRecord {
                id: "e-edit".to_string(),
                event_kind: "tool".to_string(),
                at: 400,
                action: "edit".to_string(),
                name: Some("Edit".to_string()),
                target: "native/src/wall/panels/agent_view.rs".to_string(),
                detail: "updated card header & scaled metrics".to_string(),
                status: Some("ok".to_string()),
                meta: Some(serde_json::json!({ "added": 48, "removed": 4 })),
                file: Some(FileSnapshot {
                    text: (1..=200)
                        .map(|n| format!("pub fn line_{n}() {{}}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    start: 1,
                    total: Some(200),
                }),
                ..EventRecord::default()
            },
            EventRecord {
                id: "e-run-err".to_string(),
                event_kind: "tool".to_string(),
                at: 500,
                action: "bash".to_string(),
                name: Some("Bash".to_string()),
                detail: "cargo test -p agent-feed".to_string(),
                result: "xcode-select: sandbox link denied".to_string(),
                status: Some("error".to_string()),
                ..EventRecord::default()
            },
            EventRecord {
                id: "e-say".to_string(),
                event_kind: "say".to_string(),
                at: 600,
                action: "say".to_string(),
                final_reply: true,
                result: "All 16 stub-agent unit tests pass.".to_string(),
                ..EventRecord::default()
            },
        ];

        let mut category_counts: BTreeMap<String, usize> = CATEGORY_IDS
            .iter()
            .map(|&cid| (cid.to_string(), 0usize))
            .collect();
        for rec in &records {
            for cid in wall_core::agent_view::categories_of_record(rec) {
                *category_counts.get_mut(cid).unwrap() += 1;
            }
        }

        let rows: Vec<AgentEventRow> = records
            .iter()
            .map(|rec| {
                let expanded = rec.id == "e-edit";
                AgentEventRow {
                    summary: summarize_event_row(rec, false),
                    expanded,
                    jumped: rec.id == "e-read",
                    exact_anchor: rec.id == "e-read",
                    detail: expanded.then(|| {
                        format_expanded_event(
                            rec,
                            ExpandOptions {
                                max_lines: 6,
                                full: false,
                            },
                        )
                    }),
                }
            })
            .collect();

        let filters = default_filters();
        AgentViewDescriptor {
            id: id.to_string(),
            open: true,
            side: AgentSide::Right,
            session_binding: wall_core::agent_views::SessionBinding::Auto,
            session_id: Some("9054f040-e0c8-4060".to_string()),
            session_via: Some(ResolveVia::Workgroup),
            missing: None,
            session: Some(AgentSession {
                session_id: "9054f040-e0c8-4060".to_string(),
                harness: "agy".to_string(),
                agent_name: Some("wall-config".to_string()),
                status: "processing".to_string(),
                ..AgentSession::default()
            }),
            session_label: "wall-config · agy".to_string(),
            filter_label: describe_active_filter(&filters),
            filters,
            category_counts,
            expanded_count: 1,
            total_count: records.len(),
            standard_count: records.len(),
            visible_count: rows.len(),
            unread_count: 3,
            stick_bottom: false,
            loading: false,
            loading_earlier: false,
            has_earlier: true,
            following: true,
            error: None,
            view_offset: 14,
            alt_screen: false,
            rows,
        }
    }

    fn placement_at(
        left: i64,
        top: i64,
        width: u32,
        height: u32,
        scale: f64,
        focused: bool,
    ) -> VisibleScreenPlacement {
        let badge_mode = 11.0 * scale < 7.2;
        let hairline_px = if badge_mode {
            1
        } else {
            (1.0 / scale).ceil().max(1.0) as u32
        };
        VisibleScreenPlacement {
            world_rect: CoreRect::new(0.0, 0.0, 56.0, 34.0),
            left,
            top,
            width,
            height,
            scale,
            badge_mode,
            focused,
            z_index: if focused { 4 } else { 3 },
            logical_width: if badge_mode {
                0.0
            } else {
                f64::from(width.saturating_sub(2).max(1)) / scale
            },
            logical_height: if badge_mode {
                0.0
            } else {
                f64::from(height.saturating_sub(2).max(1)) / scale
            },
            hairline_px,
        }
    }

    fn run_frame(
        ctx: &egui::Context,
        viewport: egui::Vec2,
        events: Vec<egui::Event>,
        state: &mut AgentViewPanelState,
        input: &AgentViewPanelInput,
    ) -> (Vec<String>, bool) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
            events,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut wants_kb = false;
        let mut full = ctx.run_ui(raw, |ui| {
            wants_kb = show(ui.ctx(), state, input, &mut out);
        });
        // epaint 0.36 asserts that `TexturesDelta` was handled before drop.
        full.textures_delta.clear();
        (out, wants_kb)
    }

    fn click_events(pos: egui::Pos2, button: egui::PointerButton) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    /// Minimal `WallApi` test double to verify that every verb emitted by the panel is accepted
    /// by `wall_core::commands::register_wall_commands`.
    struct RouterStubWall;

    impl WallApi for RouterStubWall {
        fn snapshot(&self) -> WallSnapshot {
            WallSnapshot {
                terminals: Vec::new(),
                groups: Vec::new(),
                selection: Vec::new(),
            }
        }
        fn has_terminal(&self, id: &str) -> bool {
            id == "t1"
        }
        fn is_shown(&self, id: &str) -> bool {
            id == "t1"
        }
        fn shown_order(&self) -> Vec<String> {
            vec!["t1".to_string()]
        }
        fn focused_id(&self) -> Option<String> {
            Some("t1".to_string())
        }
        fn hide(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn show(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn focus_and_frame(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn spawn(&mut self, _: Option<CorePoint>) -> Result<String, String> {
            Ok("t2".to_string())
        }
        fn fit_all(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close_terminal(&mut self, _: &str) -> Result<CommandReply, String> {
            Ok(CommandReply::ok("closed", None))
        }
        fn rename_terminal(&mut self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn terminal_name(&self, id: &str) -> String {
            id.to_string()
        }
        fn selection(&self) -> Vec<String> {
            Vec::new()
        }
        fn select(&mut self, _: &[String]) -> Result<(), String> {
            Ok(())
        }
        fn terminal_selection_text(&self, _: &str) -> String {
            String::new()
        }
        fn selection_clipboard(&self) -> String {
            String::new()
        }
        fn write_clipboard(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn read_clipboard(&mut self) -> Result<String, String> {
            Ok(String::new())
        }
        fn paste_into_terminal(&mut self, _: &str, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn notify(&mut self, _: &str) {}
        fn fullscreen_info(&self) -> FullscreenStatus {
            FullscreenStatus::default()
        }
        fn fullscreen_exit(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn fullscreen_enter_default(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn fullscreen_enter(&mut self, _: &str, _: Option<&str>) -> Result<bool, String> {
            Ok(true)
        }
        fn fullscreen_enter_group(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn fullscreen_toggle_pane(&mut self, _: Option<&str>) -> Result<(), String> {
            Ok(())
        }
        fn fullscreen_text(&mut self, _: i32) -> Result<Option<f64>, String> {
            Ok(Some(14.0))
        }
        fn has_group(&self, _: &str) -> bool {
            false
        }
        fn group_of(&self, _: &str) -> Option<String> {
            None
        }
        fn group_info(&self, gid: &str) -> Result<GroupSummary, String> {
            Err(format!("no group '{gid}'"))
        }
        fn group_name(&self, gid: &str) -> Result<String, String> {
            Err(format!("no group '{gid}'"))
        }
        fn group_create(&mut self, _: &[String]) -> Result<String, String> {
            Ok("g1".to_string())
        }
        fn group_add(&mut self, _: &str, _: &[String]) -> Result<(), String> {
            Ok(())
        }
        fn group_remove(&mut self, _: &[String]) -> Result<(), String> {
            Ok(())
        }
        fn group_ungroup(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn group_set_layout(
            &mut self,
            _: &str,
            _: &str,
            _: Option<i64>,
            _: Option<f64>,
        ) -> Result<(), String> {
            Ok(())
        }
        fn group_set_columns(&mut self, _: &str, _: usize) -> Result<(), String> {
            Ok(())
        }
        fn group_set_bars(&mut self, _: &str, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn group_set_gap(&mut self, _: &str, _: f64) -> Result<(), String> {
            Ok(())
        }
        fn group_rename(&mut self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn group_move(&mut self, _: &str, _: f64, _: f64) -> Result<(), String> {
            Ok(())
        }
        fn frame_group(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn split_terminal(&mut self, _: &str, _: Option<&str>) -> Result<String, String> {
            Ok("t2".to_string())
        }
        fn focus_pane(&mut self, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn group_move_pane(&mut self, _: &str, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn step_focus_history(&mut self, _: i32) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn panel(&self, _: PanelKind) -> PanelPlacement {
            PanelPlacement {
                open: false,
                side: "right".to_string(),
                float: None,
                collapsed: Vec::new(),
                mode: None,
            }
        }
        fn set_panel(&mut self, _: PanelKind, _: PanelPatch) {}
        fn termd_state(&self) -> String {
            "ok".to_string()
        }
        fn termd_dismiss_lost(&mut self) -> bool {
            false
        }
        fn view_size(&self) -> (f64, f64) {
            (1280.0, 800.0)
        }
        fn terminal_screen_box(&self, _: &str) -> Result<ScreenBox, String> {
            Ok(ScreenBox {
                left: 0.0,
                top: 0.0,
                right: 300.0,
                bottom: 200.0,
            })
        }
        fn group_header_screen_box(&self, _: &str) -> Result<Option<ScreenBox>, String> {
            Ok(None)
        }
        fn open_menu(&mut self, _: MenuTarget, _: CorePoint) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn close_menu(&mut self) {}
        fn config_entries(&self) -> Vec<(String, serde_json::Value)> {
            Vec::new()
        }
        fn config_get(&self, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Bool(true))
        }
        fn config_set(&mut self, _: &str, v: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(v)
        }
        fn config_reset(&mut self, _: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Bool(true))
        }
        fn bus_view(&self) -> BusView {
            BusView {
                snapshot_host: Some("local".to_string()),
                reason: String::new(),
                hosts: Vec::new(),
            }
        }
        fn terminals_attached_to(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn attach_agent(&mut self, _: &str) -> Result<AttachOutcome, String> {
            Ok(AttachOutcome {
                id: "t1".to_string(),
                existing: true,
            })
        }
        fn agent_set_open(&mut self, _: &str, how: &str) -> Result<AgentOpenResult, String> {
            Ok(AgentOpenResult {
                open: how != "close",
                side: "right".to_string(),
                session: Some("s1".to_string()),
                missing: None,
            })
        }
        fn agent_set_side(&mut self, _: &str, side: &str) -> Result<AgentSideResult, String> {
            Ok(AgentSideResult {
                side: side.to_string(),
            })
        }
        fn agent_set_filter(
            &mut self,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "ok": true }))
        }
        fn agent_set_bulk_expanded(
            &mut self,
            _: &str,
            _: bool,
            _: &[String],
        ) -> Result<AgentBulkExpandResult, String> {
            Ok(AgentBulkExpandResult { changed: 2 })
        }
        fn agent_toggle_event(
            &mut self,
            _: &str,
            event_id: &str,
            how: &str,
        ) -> Result<AgentToggleEventResult, String> {
            Ok(AgentToggleEventResult {
                event_id: event_id.to_string(),
                expanded: how != "off",
            })
        }
        fn agent_show_full_body(&mut self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn agent_jump(&mut self, _: &str, target: &str) -> Result<AgentJumpResult, String> {
            Ok(AgentJumpResult {
                live: target == "live",
                event_id: if target == "live" {
                    String::new()
                } else {
                    target.to_string()
                },
                offset: if target == "live" { 0 } else { 14 },
            })
        }
        fn agent_load_earlier(&mut self, _: &str) -> Result<AgentEarlierResult, String> {
            Ok(AgentEarlierResult { loaded: 5 })
        }
        fn relay_endpoint(&self) -> Option<(String, u16)> {
            None
        }
        fn set_relay_token(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn lays_out_across_scales_badge_floor_and_empty_states_without_panicking() {
        let ctx = egui::Context::default();
        let mut state = AgentViewPanelState::new();
        let vp = egui::vec2(1280.0, 800.0);

        // Empty and fullscreen inputs emit nothing and want no keyboard.
        let (out, kb) = run_frame(&ctx, vp, Vec::new(), &mut state, &AgentViewPanelInput::default());
        assert!(out.is_empty() && !kb);

        let fs_input = AgentViewPanelInput {
            cards: vec![AgentViewCardInput {
                view: sample_descriptor("t1"),
                placement: placement_at(40, 40, 260, 360, 1.0, true),
                terminal_screen_rect: None,
            }],
            fullscreen: true,
            units: PlacementUnits::Points,
        };
        let (out, kb) = run_frame(&ctx, vp, Vec::new(), &mut state, &fs_input);
        assert!(out.is_empty() && !kb);

        // Multi-scale sweep: zoomed-in (1.75x), base (1.0x), small-scale above badge floor (0.66x),
        // badge mode (0.40x), and clamped tiny minimum rect (28x18 and 8x8).
        for (w, h, scale) in [
            (420, 560, 1.75),
            (260, 380, 1.0),
            (156, 230, 0.66),
            (72, 44, 0.40),
            (28, 18, 0.25),
            (12, 12, 0.80),
        ] {
            let input = AgentViewPanelInput {
                cards: vec![AgentViewCardInput {
                    view: sample_descriptor("t1"),
                    placement: placement_at(20, 20, w, h, scale, true),
                    terminal_screen_rect: None,
                }],
                fullscreen: false,
                units: PlacementUnits::Points,
            };
            let _ = run_frame(&ctx, vp, Vec::new(), &mut state, &input);
        }
    }

    #[test]
    fn simulated_clicks_and_escape_emit_router_valid_wall_verbs() {
        let ctx = egui::Context::default();
        let mut state = AgentViewPanelState::new();
        let vp = egui::vec2(1280.0, 900.0);

        let card_input = AgentViewPanelInput {
            cards: vec![AgentViewCardInput {
                view: sample_descriptor("t1"),
                placement: placement_at(40, 40, 320, 560, 1.0, false),
                terminal_screen_rect: None,
            }],
            fullscreen: false,
            units: PlacementUnits::Points,
        };

        // Prime layout pass to populate hit rects.
        let _ = run_frame(&ctx, vp, Vec::new(), &mut state, &card_input);

        let mut all_emitted: Vec<String> = Vec::new();
        let mut click_key = |state: &mut AgentViewPanelState,
                             input: &AgentViewPanelInput,
                             key: &str,
                             btn: egui::PointerButton|
         -> Vec<String> {
            let rect = state
                .hit_rect("t1", key)
                .unwrap_or_else(|| panic!("missing hit rect for t1:{key}"));
            let (out, _) = run_frame(&ctx, vp, click_events(rect.center(), btn), state, input);
            all_emitted.extend(out.clone());
            out
        };

        // Clicking an unfocused card records `clicked_focus_id` and fires the button's verb.
        assert_eq!(
            click_key(&mut state, &card_input, "side-left", egui::PointerButton::Primary),
            vec!["wall.agent.side t1 left"]
        );
        assert_eq!(state.take_clicked_focus_id().as_deref(), Some("t1"));
        assert_eq!(
            click_key(&mut state, &card_input, "side-top", egui::PointerButton::Primary),
            vec!["wall.agent.side t1 top"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "side-bottom", egui::PointerButton::Primary),
            vec!["wall.agent.side t1 bottom"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "jump-live", egui::PointerButton::Primary),
            vec!["wall.agent.jump t1 live"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "expand-all", egui::PointerButton::Primary),
            vec!["wall.agent.expand t1 all"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "collapse-all", egui::PointerButton::Primary),
            vec!["wall.agent.collapse t1 all"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "chip-read", egui::PointerButton::Primary),
            vec!["wall.agent.filter t1 toggle read"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "chip-read", egui::PointerButton::Secondary),
            vec!["wall.agent.filter t1 only read"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "load-earlier", egui::PointerButton::Primary),
            vec!["wall.agent.earlier t1"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "show-e-prompt", egui::PointerButton::Primary),
            vec!["wall.agent.show t1 e-prompt toggle"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "jump-e-prompt", egui::PointerButton::Primary),
            vec!["wall.agent.jump t1 e-prompt"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "show-full-e-edit", egui::PointerButton::Primary),
            vec!["wall.agent.show t1 e-edit full"]
        );

        // Clicking `follow-tail` clears unread state and records `stick_bottom = true`.
        let follow_out =
            click_key(&mut state, &card_input, "follow-tail", egui::PointerButton::Primary);
        assert!(follow_out.is_empty());
        assert_eq!(state.take_stick_bottom_changes(), vec![("t1".to_string(), true)]);

        // Open category menu drawer -> wants_keyboard becomes true; subset buttons emit verbs;
        // Escape closes the drawer and clears wants_keyboard.
        let trigger_pos = state.hit_rect("t1", "toggle-filter-menu").unwrap().center();
        let (_, kb_open) = run_frame(
            &ctx,
            vp,
            click_events(trigger_pos, egui::PointerButton::Primary),
            &mut state,
            &card_input,
        );
        assert!(kb_open && state.card("t1").unwrap().menu_open);

        assert_eq!(
            click_key(&mut state, &card_input, "only-tools", egui::PointerButton::Primary),
            vec!["wall.agent.filter t1 only tools"]
        );
        assert_eq!(
            click_key(&mut state, &card_input, "exp-noise", egui::PointerButton::Primary),
            vec![
                "wall.agent.filter t1 on noise",
                "wall.agent.expand t1 noise"
            ],
            "expanding a currently disabled category enables its filter first"
        );
        assert_eq!(
            click_key(&mut state, &card_input, "col-tools", egui::PointerButton::Primary),
            vec!["wall.agent.collapse t1 tools"]
        );

        let (_, _) = run_frame(
            &ctx,
            vp,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut state,
            &card_input,
        );
        let (_, kb_after_esc) = run_frame(&ctx, vp, Vec::new(), &mut state, &card_input);
        assert!(!kb_after_esc && !state.card("t1").unwrap().menu_open);

        // Close button (`×`)
        assert_eq!(
            click_key(&mut state, &card_input, "close", egui::PointerButton::Primary),
            vec!["wall.agent t1 close"]
        );

        // Far-zoom badge click -> `wall.reveal t1`
        let badge_input = AgentViewPanelInput {
            cards: vec![AgentViewCardInput {
                view: sample_descriptor("t1"),
                placement: placement_at(40, 40, 84, 26, 0.35, true),
                terminal_screen_rect: None,
            }],
            fullscreen: false,
            units: PlacementUnits::Points,
        };
        let _ = run_frame(&ctx, vp, Vec::new(), &mut state, &badge_input);
        assert_eq!(
            click_key(&mut state, &badge_input, "badge", egui::PointerButton::Primary),
            vec!["wall.reveal t1"]
        );

        // Validate EVERY emitted verb string through the real `wall_core` CommandRouter!
        let mut router = CommandRouter::new(RouterStubWall);
        register_wall_commands(&mut router);
        assert!(!all_emitted.is_empty());
        for line in &all_emitted {
            let reply = router.execute(line.as_str());
            assert!(
                !reply.is_err(),
                "verb `{line}` returned error reply: {}",
                reply.text
            );
        }
    }

    #[test]
    fn header_drag_picks_top_bottom_left_right_with_screen_y_down() {
        let term_rect = egui::Rect::from_min_max(egui::pos2(300.0, 200.0), egui::pos2(600.0, 400.0));
        assert_eq!(
            pick_side_screen_space(term_rect, egui::pos2(450.0, 120.0)),
            AgentSide::Top,
            "pointer above terminal (smaller screen y) picks Top"
        );
        assert_eq!(
            pick_side_screen_space(term_rect, egui::pos2(450.0, 480.0)),
            AgentSide::Bottom,
            "pointer below terminal (larger screen y) picks Bottom"
        );
        assert_eq!(
            pick_side_screen_space(term_rect, egui::pos2(210.0, 300.0)),
            AgentSide::Left
        );
        assert_eq!(
            pick_side_screen_space(term_rect, egui::pos2(690.0, 300.0)),
            AgentSide::Right
        );

        // End-to-end drag of header above terminal emits `wall.agent.side t1 top`.
        let ctx = egui::Context::default();
        let mut state = AgentViewPanelState::new();
        let vp = egui::vec2(1000.0, 700.0);
        let input = AgentViewPanelInput {
            cards: vec![AgentViewCardInput {
                view: sample_descriptor("t1"),
                placement: placement_at(610, 200, 240, 220, 1.0, true),
                terminal_screen_rect: Some(term_rect),
            }],
            fullscreen: false,
            units: PlacementUnits::Points,
        };
        let _ = run_frame(&ctx, vp, Vec::new(), &mut state, &input);
        let hdr = state.hit_rect("t1", "header").unwrap().center();
        let target = egui::pos2(450.0, 110.0);

        let _ = run_frame(
            &ctx,
            vp,
            vec![
                egui::Event::PointerMoved(hdr),
                egui::Event::PointerButton {
                    pos: hdr,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            &mut state,
            &input,
        );
        let _ = run_frame(
            &ctx,
            vp,
            vec![egui::Event::PointerMoved(target)],
            &mut state,
            &input,
        );
        let (out, _) = run_frame(
            &ctx,
            vp,
            vec![egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut state,
            &input,
        );
        assert_eq!(out, vec!["wall.agent.side t1 top"]);
    }

    #[test]
    fn empty_missing_and_unavailable_states_render_and_reset_filter() {
        let ctx = egui::Context::default();
        let mut state = AgentViewPanelState::new();
        let vp = egui::vec2(900.0, 600.0);

        // 1. Filtered-to-empty state exposes `reset-empty-filter` button.
        let mut filtered_empty = sample_descriptor("t1");
        filtered_empty.rows.clear();
        filtered_empty.visible_count = 0;
        filtered_empty.filter_label = "image reads".to_string();

        // 2. No-session state (`session_id = None`, `missing = Some(...)`).
        let mut no_sess = sample_descriptor("t2");
        no_sess.session_id = None;
        no_sess.session = None;
        no_sess.rows.clear();
        no_sess.total_count = 0;
        no_sess.visible_count = 0;
        no_sess.missing = Some("no live Claude/Agy session in /Users/user/scratch".to_string());

        // 3. Stream error / unavailable state.
        let mut unavailable = sample_descriptor("t3");
        unavailable.error = Some("gomuxai `agent` binary not found".to_string());
        unavailable.rows.clear();

        let input = AgentViewPanelInput {
            cards: vec![
                AgentViewCardInput {
                    view: filtered_empty,
                    placement: placement_at(20, 20, 260, 220, 1.0, true),
                    terminal_screen_rect: None,
                },
                AgentViewCardInput {
                    view: no_sess,
                    placement: placement_at(300, 20, 260, 220, 1.0, false),
                    terminal_screen_rect: None,
                },
                AgentViewCardInput {
                    view: unavailable,
                    placement: placement_at(580, 20, 260, 220, 1.0, false),
                    terminal_screen_rect: None,
                },
            ],
            fullscreen: false,
            units: PlacementUnits::Points,
        };

        let _ = run_frame(&ctx, vp, Vec::new(), &mut state, &input);
        let reset_pos = state
            .hit_rect("t1", "reset-empty-filter")
            .expect("empty filtered state renders reset button")
            .center();
        let (out, _) = run_frame(
            &ctx,
            vp,
            click_events(reset_pos, egui::PointerButton::Primary),
            &mut state,
            &input,
        );
        assert_eq!(out, vec!["wall.agent.filter t1 reset"]);
    }

    #[test]
    fn renders_agent_view_cards_and_badge_over_wall_offscreen_to_png() {
        let gpu = crate::gpu::test_device::context();

        let (width, height) = (1360u32, 820u32);
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut wall_renderer = crate::wall::WallRenderer::new(
            &gpu.device,
            &gpu.queue,
            &crate::default_emoji_sheet(),
            format,
            crate::wall::WallStyle::default(),
        );

        let placed = crate::wall::test_pattern::build_overlapping_test_pattern(wall_renderer.metrics());
        let draws: Vec<crate::wall::TerminalDraw<'_>> = placed
            .iter()
            .take(1)
            .map(|t| crate::wall::TerminalDraw {
                id: t.id,
                screen: &t.screen,
                position: glam::Vec2::new(-52.0, 12.0),
                z: 1.0,
                scale: 1.0,
                focused: true,
                panel_opacity: None,
                border_color: None,
                selection: None,
                hover_link: None,
            })
            .collect();
        let paper = crate::wall::PaperStyle::default();
        let wall_frame = crate::wall::WallFrame {
            paper: Some(&paper),
            terminals: &draws,
            shapes: &[],
            labels: &[],
        };
        let camera = crate::wall::WallCamera::new(glam::Vec2::new(14.0, 0.0), 11.0);

        // Build 4 Agent View placements showcasing:
        // (1) Full 1.0x card with expanded diff + unread follow bar,
        // (2) Small-scale 0.70x card just above the 0.655x badge floor,
        // (3) Far-zoom 0.35x compact badge,
        // (4) Missing/no-agent card.
        let mut no_sess = sample_descriptor("t-missing");
        no_sess.session_id = None;
        no_sess.session = None;
        no_sess.rows.clear();
        no_sess.total_count = 0;
        no_sess.standard_count = 0;
        no_sess.visible_count = 0;
        no_sess.expanded_count = 0;
        no_sess.unread_count = 0;
        no_sess.view_offset = 0;
        no_sess.following = false;
        no_sess.has_earlier = false;
        no_sess.missing = Some("waiting for `agent sessions` in ~/repo…".to_string());

        let panel_input = AgentViewPanelInput {
            cards: vec![
                AgentViewCardInput {
                    view: sample_descriptor("t-main"),
                    placement: placement_at(510, 28, 360, 760, 1.0, true),
                    terminal_screen_rect: None,
                },
                AgentViewCardInput {
                    view: sample_descriptor("t-small"),
                    placement: placement_at(890, 28, 250, 440, 0.70, false),
                    terminal_screen_rect: None,
                },
                AgentViewCardInput {
                    view: sample_descriptor("t-badge"),
                    placement: placement_at(1160, 28, 112, 28, 0.35, false),
                    terminal_screen_rect: None,
                },
                AgentViewCardInput {
                    view: no_sess,
                    placement: placement_at(890, 490, 382, 200, 1.0, false),
                    terminal_screen_rect: None,
                },
            ],
            fullscreen: false,
            units: PlacementUnits::Points,
        };

        let egui_ctx = egui::Context::default();
        let mut panel_state = AgentViewPanelState::new();
        let mut egui_renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            format,
            egui_wgpu::RendererOptions::default(),
        );

        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("agent view offscreen preview"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = texture.create_view(&Default::default());
        let depth_view =
            crate::scene::create_depth(&gpu.device, crate::wall::WALL_DEPTH_FORMAT, width, height);

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("agent view preview encoder"),
            });
        let target = crate::wall::WallTarget {
            color_view: &color_view,
            depth_view: &depth_view,
            width,
            height,
        };
        wall_renderer.render(&gpu.device, &gpu.queue, &mut encoder, &target, &camera, &wall_frame);

        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32, height as f32),
            )),
            ..Default::default()
        };
        let mut out_verbs = Vec::new();
        let mut full_output = egui_ctx.run_ui(raw_input, |ui| {
            show(ui.ctx(), &mut panel_state, &panel_input, &mut out_verbs);
        });
        for (id, image_deltas) in full_output.textures_delta.set.drain() {
            for image_delta in image_deltas {
                egui_renderer.update_texture(&gpu.device, &gpu.queue, id, &image_delta);
            }
        }
        let clipped = egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
        assert!(
            !clipped.is_empty(),
            "egui must emit clipped primitives for the 4 cards"
        );
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: full_output.pixels_per_point,
        };
        let user_cmds = egui_renderer.update_buffers(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &clipped,
            &screen_desc,
        );
        if !user_cmds.is_empty() {
            gpu.queue.submit(user_cmds);
        }
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("agent view egui pass"),
                timestamp_writes: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
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
        }

        let unpadded_bpr = width * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("agent view preview readback"),
            size: u64::from(padded_bpr * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        for id in full_output.textures_delta.free.drain() {
            egui_renderer.free_texture(&id);
        }

        let slice = readback.slice(..);
        glyph_field::map_read(
            &gpu.device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "agent view preview readback",
        );
        let data = slice.get_mapped_range().expect("preview mapped");
        let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
        for row in 0..height {
            let start = (row * padded_bpr) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
        }
        drop(data);
        readback.unmap();

        // Counterfactual check: verify each of the 4 card rects rendered distinct non-background pixels!
        for (label, rx, ry, rw, rh) in [
            ("1.0x card", 510usize, 28usize, 360usize, 760usize),
            ("0.70x card", 890usize, 28usize, 250usize, 440usize),
            ("badge", 1160usize, 28usize, 112usize, 28usize),
            ("no-session card", 890usize, 490usize, 382usize, 200usize),
        ] {
            let mut distinct_bright = 0usize;
            for y in ry..(ry + rh) {
                for x in rx..(rx + rw) {
                    let i = (y * width as usize + x) * 4;
                    let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
                    if r > 60 || g > 90 || b > 110 {
                        distinct_bright += 1;
                    }
                }
            }
            assert!(
                distinct_bright > 40,
                "{label} rendered no visible foreground glyphs/strokes (bright px = {distinct_bright})"
            );
        }

        let out_png = std::env::var_os("GLYPH_AGENT_VIEW_PREVIEW_PNG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/glyph-scratch/agent-view-preview.png"));
        if let Some(parent) = out_png.parent() {
            std::fs::create_dir_all(parent).expect("create preview dir");
        }
        image::save_buffer(&out_png, &pixels, width, height, image::ColorType::Rgba8)
            .expect("write agent-view-preview.png");
    }
}
