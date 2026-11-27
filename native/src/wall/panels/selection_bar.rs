//! Floating multi-terminal selection action bar (`wall/SelectionOverlay.js:39-50, 135-155`).
//!
//! Appears beneath the bottom-right corner of the selected terminals' bounding box
//! whenever one or more terminals are selected and no lasso drag is active:
//! - `group (N)` → `wall.group`
//! - `×` → `wall.select none`

use wall_core::chord::chord_label;
use wall_core::input::Platform;

const OUTLINE_GAP_PX: f32 = 3.0;
const BAR_TOP_GAP_PX: f32 = 6.0;
const VIEWPORT_MARGIN_PX: f32 = 4.0;
const BUTTON_GAP_PX: f32 = 2.0;
const BAR_PAD_PX: f32 = 2.0;
const BTN_PAD_X_PX: f32 = 7.0;
const BTN_PAD_Y_PX: f32 = 2.0;

const BAR_BG: egui::Color32 = egui::Color32::from_rgb(0x0e, 0x11, 0x18);
const BAR_BORDER: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x30, 0x3c);
const BTN_FG: egui::Color32 = egui::Color32::from_rgb(0xc8, 0xce, 0xd8);
const BTN_HOVER_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(18, 18, 18, 18);

/// Last-frame hit rectangles recorded by [`show`] (for `app.where` inspection and tests).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionBarState {
    pub count: usize,
    pub last_rect: Option<egui::Rect>,
    pub group_rect: Option<egui::Rect>,
    pub clear_rect: Option<egui::Rect>,
}

/// Per-frame inputs for [`show`].
#[derive(Clone, Copy, Debug)]
pub struct SelectionBarInput {
    /// Number of currently selected shown terminals (`0` hides the bar).
    pub count: usize,
    /// Bounding screen-space rectangle of the selected terminals' window boxes (logical px).
    pub selection_bounds: Option<egui::Rect>,
    /// True while a lasso drag is actively in flight (the bar waits until release).
    pub lassoing: bool,
    /// Host platform for formatting the `⌘G` / `Ctrl+Alt+G` tooltip.
    pub platform: Platform,
}

impl Default for SelectionBarInput {
    fn default() -> Self {
        Self {
            count: 0,
            selection_bounds: None,
            lassoing: false,
            platform: Platform::host(),
        }
    }
}

/// Draw the floating `"group (N)"` + `"×"` selection action bar and append clicked verbs to `out`.
pub fn show(
    ctx: &egui::Context,
    state: &mut SelectionBarState,
    input: &SelectionBarInput,
    out: &mut Vec<String>,
) {
    state.count = 0;
    state.last_rect = None;
    state.group_rect = None;
    state.clear_rect = None;

    let Some(bounds) = input.selection_bounds else {
        return;
    };
    if input.count == 0 || input.lassoing {
        return;
    }

    let font = egui::FontId::monospace(12.0);
    let group_label = format!("group ({})", input.count);
    let (group_galley, clear_galley) = ctx.fonts_mut(|f| {
        (
            f.layout_no_wrap(group_label, font.clone(), BTN_FG),
            f.layout_no_wrap("×".to_string(), font, BTN_FG),
        )
    });

    let btn_h = group_galley
        .size()
        .y
        .max(clear_galley.size().y)
        + 2.0 * BTN_PAD_Y_PX;
    let group_w = group_galley.size().x + 2.0 * BTN_PAD_X_PX;
    let clear_w = clear_galley.size().x + 2.0 * BTN_PAD_X_PX;
    let bar_w = BAR_PAD_PX * 2.0 + group_w + BUTTON_GAP_PX + clear_w + 2.0;
    let bar_h = BAR_PAD_PX * 2.0 + btn_h + 2.0;

    let vp = super::safe_viewport_rect(ctx);
    let right = bounds.max.x + OUTLINE_GAP_PX;
    let bottom = bounds.max.y + OUTLINE_GAP_PX;
    let max_x = (vp.max.x - bar_w - VIEWPORT_MARGIN_PX).max(vp.min.x + VIEWPORT_MARGIN_PX);
    let max_y = (vp.max.y - bar_h - VIEWPORT_MARGIN_PX).max(vp.min.y + VIEWPORT_MARGIN_PX);
    let x = (right - bar_w).clamp(vp.min.x + VIEWPORT_MARGIN_PX, max_x);
    let y = (bottom + BAR_TOP_GAP_PX).clamp(vp.min.y + VIEWPORT_MARGIN_PX, max_y);
    let bar_rect = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(bar_w, bar_h));

    let group_tip = format!(
        "gather into a group ({})",
        super::sanitize_egui_text(&chord_label(input.platform, &["G"], false))
    );

    let mut group_clicked = false;
    let mut clear_clicked = false;
    let mut group_rect = None;
    let mut clear_rect = None;

    egui::Area::new(egui::Id::new("wall_selection_bar"))
        .order(egui::Order::Foreground)
        .fixed_pos(bar_rect.min)
        .default_size(bar_rect.size())
        .fade_in(false)
        .constrain(false)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(BAR_BG)
                .stroke(egui::Stroke::new(1.0, BAR_BORDER))
                .corner_radius(egui::CornerRadius::same(5))
                .inner_margin(egui::Margin::same(BAR_PAD_PX as i8))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(BUTTON_GAP_PX, 0.0);
                    ui.horizontal(|ui| {
                        let (g_rect, g_resp) =
                            ui.allocate_exact_size(egui::vec2(group_w, btn_h), egui::Sense::click());
                        group_rect = Some(g_rect);
                        if g_resp.hovered() {
                            ui.painter().rect_filled(
                                g_rect,
                                egui::CornerRadius::same(3),
                                BTN_HOVER_BG,
                            );
                        }
                        let g_pos = egui::pos2(
                            g_rect.min.x + BTN_PAD_X_PX,
                            g_rect.center().y - group_galley.size().y * 0.5,
                        );
                        ui.painter().galley(g_pos, group_galley, BTN_FG);
                        group_clicked = g_resp
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(group_tip)
                            .clicked();

                        let (c_rect, c_resp) =
                            ui.allocate_exact_size(egui::vec2(clear_w, btn_h), egui::Sense::click());
                        clear_rect = Some(c_rect);
                        if c_resp.hovered() {
                            ui.painter().rect_filled(
                                c_rect,
                                egui::CornerRadius::same(3),
                                BTN_HOVER_BG,
                            );
                        }
                        let c_pos = egui::pos2(
                            c_rect.min.x + BTN_PAD_X_PX,
                            c_rect.center().y - clear_galley.size().y * 0.5,
                        );
                        ui.painter().galley(c_pos, clear_galley, BTN_FG);
                        clear_clicked = c_resp
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("clear the selection (click empty canvas)")
                            .clicked();
                    });
                });
        });

    state.count = input.count;
    state.last_rect = Some(bar_rect);
    state.group_rect = group_rect;
    state.clear_rect = clear_rect;

    if group_clicked {
        out.push("wall.group".to_string());
    }
    if clear_clicked {
        out.push("wall.select none".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wall::ui::configure_wall_context;

    fn make_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        ctx
    }

    fn click_point(
        ctx: &egui::Context,
        pos: egui::Pos2,
        state: &mut SelectionBarState,
        input: &SelectionBarInput,
    ) -> Vec<String> {
        let mut verbs = Vec::new();
        for pressed in [true, false] {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            };
            let _ = ctx.run_ui(raw, |ui| show(ui.ctx(), state, input, &mut verbs));
        }
        verbs
    }

    #[test]
    fn selection_bar_hides_when_empty_or_lassoing_and_emits_group_and_clear_on_click() {
        let ctx = make_ctx();
        let mut state = SelectionBarState::default();
        let mut verbs = Vec::new();

        // Hidden when count == 0.
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| show(ui.ctx(), &mut state, &SelectionBarInput::default(), &mut verbs),
        );
        assert_eq!(state.count, 0);
        assert!(state.last_rect.is_none());

        // Hidden while lassoing.
        let bounds = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(600.0, 420.0));
        let lasso_in = SelectionBarInput {
            count: 2,
            selection_bounds: Some(bounds),
            lassoing: true,
            platform: Platform::Mac,
        };
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| show(ui.ctx(), &mut state, &lasso_in, &mut verbs),
        );
        assert!(state.last_rect.is_none());

        // Shown once lasso settles: sits below bottom-right of selection bounds.
        let shown_in = SelectionBarInput {
            count: 2,
            selection_bounds: Some(bounds),
            lassoing: false,
            platform: Platform::Mac,
        };
        for _ in 0..2 {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| show(ui.ctx(), &mut state, &shown_in, &mut verbs),
            );
        }
        let bar_rect = state.last_rect.expect("selection bar rect");
        assert!(bar_rect.min.y >= bounds.max.y, "bar must sit below selection bounds");
        let group_pt = state.group_rect.expect("group btn rect").center();
        let clear_pt = state.clear_rect.expect("clear btn rect").center();

        assert_eq!(click_point(&ctx, group_pt, &mut state, &shown_in), vec!["wall.group"]);
        assert_eq!(click_point(&ctx, clear_pt, &mut state, &shown_in), vec!["wall.select none"]);
    }
}
