//! Topbar chrome strip across the top of the terminal wall.
//!
//! Ports the top action strip of `wall/main.jsx` (`Chrome`) to egui under the
//! `P-panels.md` contract: displays the wall's name, emits verbs for spawning,
//! fitting, and toggling the palette, sidebar, status window, settings, and
//! help panels, hides completely during fullscreen, respects docked panel
//! insets (`wall_core::dock`), and leaves explicit room for the status panel's
//! `termd` chip and notice.

use wall_core::chord::chord_label;
use wall_core::dock::{EdgeWidths, TOPBAR_EDGE_GAP};
use wall_core::input::Platform;

/// Distance from the top of the viewport to the topbar strip (`styles.topbar.top` in `wall/main.jsx`).
pub const TOPBAR_TOP_PX: f32 = 10.0;
/// Minimum horizontal inset from viewport/docked-panel edges (`wall/dockLayout.js`).
pub const TOPBAR_EDGE_GAP_PX: f32 = TOPBAR_EDGE_GAP as f32;
/// Horizontal spacing between items in the topbar (`styles.topbar.gap` in `wall/main.jsx`).
pub const TOPBAR_ITEM_GAP_PX: f32 = 6.0;
/// Default vertical top coordinate for notices below the topbar (`styles.notice.top` in `wall/main.jsx`).
pub const TOPBAR_NOTICE_TOP_PX: f32 = 44.0;
/// Default verb emitted when the palette (`find`) button is clicked.
pub const DEFAULT_PALETTE_VERB: &str = "wall.palette toggle";

const BTN_PAD_X: f32 = 9.0;
const BTN_PAD_Y: f32 = 4.0;
const NAME_PAD_X: f32 = 8.0;
const NAME_PAD_Y: f32 = 4.0;
const NAME_DOT_SPACE: f32 = 10.0;

const BTN_BG_IDLE: egui::Color32 = egui::Color32::from_rgb(0x14, 0x18, 0x21);
const BTN_BG_HOVER: egui::Color32 = egui::Color32::from_rgb(0x22, 0x29, 0x38);
const BTN_BG_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0x16, 0x40, 0x2c);
const BTN_BORDER_IDLE: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x30, 0x3c);
const BTN_BORDER_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0x6e, 0xe7, 0xa0);
const BTN_FG_IDLE: egui::Color32 = egui::Color32::from_rgb(0xc8, 0xce, 0xd8);
const BTN_FG_HOVER: egui::Color32 = egui::Color32::from_rgb(0xee, 0xf2, 0xf8);
const BTN_FG_ACTIVE: egui::Color32 = egui::Color32::from_rgb(0x8e, 0xf0, 0xb8);

const NAME_BG: egui::Color32 = egui::Color32::from_rgb(0x0f, 0x13, 0x1b);
const NAME_BORDER: egui::Color32 = egui::Color32::from_rgb(0x22, 0x28, 0x34);
const NAME_FG: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa3, 0xb2);
const NAME_DOT: egui::Color32 = egui::Color32::from_rgb(0x6e, 0xe7, 0xa0);
const NAME_EMPTY_FG: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x73, 0x84);

/// Persistent view state for the topbar, kept between frames by the caller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TopbarState {
    /// Last rendered bounding rectangle (`None` before first non-fullscreen frame).
    pub last_bar_rect: Option<egui::Rect>,
    /// Last rendered `termd` chip slot rectangle (`None` in fullscreen or when width is 0).
    pub last_chip_rect: Option<egui::Rect>,
    /// Last rendered button rectangles keyed by short action name (`spawn`, `fit`, `find`, `sidebar`, `status`, `settings`, `help`).
    pub button_rects: std::collections::BTreeMap<&'static str, egui::Rect>,
}

/// Plain-data inputs for [`show`].
#[derive(Clone, Debug)]
pub struct TopbarInput<'a> {
    /// Display name of the wall (for example the namespace or wall title).
    pub wall_name: &'a str,
    /// Platform conventions used to format chord hints (`⌘` vs `Ctrl+Alt`).
    pub platform: Platform,
    /// When true, the topbar hides completely (`!state.fullscreen` in `wall/main.jsx`).
    pub fullscreen: bool,
    /// Left and right horizontal insets in CSS pixels (`dock_layout(state).inset`).
    pub inset: EdgeWidths,
    /// Width in points to leave on the right end of the strip for the status panel's `termd` chip.
    pub termd_chip_width: f32,
    /// Width in points to leave on the strip (and below it) for the status panel's notice.
    pub notice_width: f32,
    /// True while the command/find palette is open.
    pub palette_open: bool,
    /// True while the sidebar panel is open.
    pub sidebar_open: bool,
    /// True while the `termd` status window is open.
    pub status_open: bool,
    /// True while the settings panel is open.
    pub settings_open: bool,
    /// True while the help panel is open.
    pub help_open: bool,
    /// Verb line pushed when the palette (`find`) button is clicked.
    pub palette_verb: &'a str,
}

impl Default for TopbarInput<'static> {
    fn default() -> Self {
        Self {
            wall_name: "wall",
            platform: Platform::host(),
            fullscreen: false,
            inset: EdgeWidths {
                left: TOPBAR_EDGE_GAP,
                right: TOPBAR_EDGE_GAP,
            },
            termd_chip_width: 0.0,
            notice_width: 0.0,
            palette_open: false,
            sidebar_open: false,
            status_open: false,
            settings_open: false,
            help_open: false,
            palette_verb: DEFAULT_PALETTE_VERB,
        }
    }
}

/// Layout geometry and interaction flags returned by [`show`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TopbarOutput {
    /// Bounding screen rectangle of the rendered topbar (`None` in fullscreen).
    pub bar_rect: Option<egui::Rect>,
    /// Reserved slot at the right end of the strip for the status panel's `termd` chip.
    pub termd_chip_rect: Option<egui::Rect>,
    /// Reserved inline slot at the left end of the strip of width `input.notice_width`.
    pub notice_slot_rect: Option<egui::Rect>,
    /// Reserved banner band below the topbar (`top >= 44`, inset between `inset.left` and `inset.right`)
    /// where transient wall or `termd` notices sit without overlapping the topbar.
    pub notice_banner_rect: Option<egui::Rect>,
    /// True when the pointer hovers or presses the topbar this frame.
    pub wants_pointer: bool,
    /// Always `false`: topbar buttons never capture keyboard input away from terminals.
    pub wants_keyboard: bool,
}

/// Draw the topbar strip for one frame and append any triggered verbs to `out`.
pub fn show(
    ctx: &egui::Context,
    state: &mut TopbarState,
    input: &TopbarInput<'_>,
    out: &mut Vec<String>,
) -> TopbarOutput {
    if input.fullscreen {
        state.last_bar_rect = None;
        state.last_chip_rect = None;
        state.button_rects.clear();
        return TopbarOutput::default();
    }

    let viewport = ctx.input(|i| i.viewport_rect());
    let vp_min_x = if viewport.width().is_finite() { viewport.min.x } else { 0.0 };
    let vp_max_x = if viewport.width().is_finite() && viewport.width() > 0.0 {
        viewport.max.x
    } else {
        1280.0
    };

    let inset_left = (input.inset.left as f32).max(TOPBAR_EDGE_GAP_PX);
    let inset_right = (input.inset.right as f32).max(TOPBAR_EDGE_GAP_PX);
    let strip_left = vp_min_x + inset_left;
    let strip_right = (vp_max_x - inset_right).max(strip_left + 1.0);

    let chip_w = if input.termd_chip_width.is_finite() && input.termd_chip_width > 0.0 {
        input.termd_chip_width
    } else {
        0.0
    };
    let notice_w = if input.notice_width.is_finite() && input.notice_width > 0.0 {
        input.notice_width
    } else {
        0.0
    };

    let buttons_right = if chip_w > 0.0 {
        (strip_right - chip_w - TOPBAR_ITEM_GAP_PX).max(strip_left)
    } else {
        strip_right
    };
    let buttons_left_min = if notice_w > 0.0 {
        (strip_left + notice_w + TOPBAR_ITEM_GAP_PX).min(buttons_right)
    } else {
        strip_left
    };
    let max_bar_width = (buttons_right - buttons_left_min).max(48.0);

    let find_chord = chord_label(input.platform, &["K"], false);
    let list_chord = chord_label(input.platform, &["B"], false);
    let status_chord = chord_label(input.platform, &["S"], false);
    let spawn_chord = chord_label(input.platform, &["\\"], false);
    let fit_chord = chord_label(input.platform, &["0"], false);
    let settings_chord = chord_label(input.platform, &[","], false);
    let help_chord = chord_label(input.platform, &["/"], false);

    let find_label = format!("find {find_chord}");
    let list_label = format!("list {list_chord}");
    let status_label = format!("status {status_chord}");
    let palette_verb = if input.palette_verb.trim().is_empty() {
        DEFAULT_PALETTE_VERB
    } else {
        input.palette_verb
    };

    let single_row_w = measure_single_row_width(
        ctx,
        input.wall_name,
        &[
            "+ terminal",
            "fit all",
            &find_label,
            &list_label,
            &status_label,
            "settings",
            "?",
        ],
    );
    let target_w = single_row_w.min(max_bar_width);
    let start_x = (buttons_right - target_w).max(buttons_left_min);

    let area_resp = egui::Area::new(egui::Id::new("wall_topbar"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(start_x, TOPBAR_TOP_PX))
        .default_width(target_w + 2.0)
        .fade_in(false)
        .interactable(true)
        .show(ctx, |ui| {
            if ui.is_sizing_pass() {
                // Complete egui's initial Area measurement within the same `run_ui`
                // frame so an on-demand `WallUi` window gets visible shapes on frame 0.
                ui.ctx().request_discard("wall_topbar initial sizing");
            }
            ui.set_min_width(target_w);
            ui.set_max_width(target_w + 2.0);
            ui.spacing_mut().item_spacing = egui::vec2(TOPBAR_ITEM_GAP_PX, TOPBAR_ITEM_GAP_PX);
            ui.horizontal_wrapped(|ui| {
                draw_wall_name_badge(ui, input.wall_name);

                let (spawn_clicked, spawn_r) = draw_topbar_button_with_rect(
                    ui,
                    "+ terminal",
                    false,
                    &format!("New terminal ({spawn_chord})"),
                );
                state.button_rects.insert("spawn", spawn_r);
                if spawn_clicked {
                    out.push("wall.spawn".to_owned());
                }

                let (fit_clicked, fit_r) = draw_topbar_button_with_rect(
                    ui,
                    "fit all",
                    false,
                    &format!("Fit all ({fit_chord})"),
                );
                state.button_rects.insert("fit", fit_r);
                if fit_clicked {
                    out.push("wall.fit".to_owned());
                }

                let (find_clicked, find_r) = draw_topbar_button_with_rect(
                    ui,
                    &find_label,
                    input.palette_open,
                    &format!("Find by name ({find_chord})"),
                );
                state.button_rects.insert("find", find_r);
                if find_clicked {
                    out.push(palette_verb.to_owned());
                }

                let (sb_clicked, sb_r) = draw_topbar_button_with_rect(
                    ui,
                    &list_label,
                    input.sidebar_open,
                    &format!("Every terminal and group ({list_chord})"),
                );
                state.button_rects.insert("sidebar", sb_r);
                if sb_clicked {
                    out.push("wall.sidebar toggle".to_owned());
                }

                let (st_clicked, st_r) = draw_topbar_button_with_rect(
                    ui,
                    &status_label,
                    input.status_open,
                    &format!("termd's status window ({status_chord})"),
                );
                state.button_rects.insert("status", st_r);
                if st_clicked {
                    out.push("wall.status toggle".to_owned());
                }

                let (set_clicked, set_r) = draw_topbar_button_with_rect(
                    ui,
                    "settings",
                    input.settings_open,
                    &format!("Every knob, searchable ({settings_chord})"),
                );
                state.button_rects.insert("settings", set_r);
                if set_clicked {
                    out.push("wall.settings toggle".to_owned());
                }

                let (help_clicked, help_r) = draw_topbar_button_with_rect(
                    ui,
                    "?",
                    input.help_open,
                    &format!("Keys, gestures and verbs ({help_chord})"),
                );
                state.button_rects.insert("help", help_r);
                if help_clicked {
                    out.push("wall.help toggle".to_owned());
                }
            });
        });

    let bar_rect = area_resp.response.rect;
    state.last_bar_rect = Some(bar_rect);

    let bar_h = bar_rect.height().max(24.0);
    let termd_chip_rect = (chip_w > 0.0).then(|| {
        egui::Rect::from_min_size(
            egui::pos2(strip_right - chip_w, TOPBAR_TOP_PX),
            egui::vec2(chip_w, bar_h),
        )
    });
    state.last_chip_rect = termd_chip_rect;
    let notice_slot_rect = (notice_w > 0.0).then(|| {
        egui::Rect::from_min_size(
            egui::pos2(strip_left, TOPBAR_TOP_PX),
            egui::vec2(notice_w.min((strip_right - strip_left).max(0.0)), bar_h),
        )
    });

    let banner_top = (bar_rect.max.y + 8.0).max(TOPBAR_NOTICE_TOP_PX);
    let banner_avail_w = (strip_right - strip_left).max(0.0);
    let banner_rect = if notice_w > 0.0 && notice_w < banner_avail_w {
        let cx = (strip_left + strip_right) * 0.5;
        egui::Rect::from_min_size(
            egui::pos2(cx - notice_w * 0.5, banner_top),
            egui::vec2(notice_w, 26.0),
        )
    } else {
        egui::Rect::from_min_size(
            egui::pos2(strip_left, banner_top),
            egui::vec2(banner_avail_w, 26.0),
        )
    };

    let pointer_over = ctx.input(|i| {
        i.pointer
            .interact_pos()
            .or_else(|| i.pointer.latest_pos())
            .is_some_and(|p| bar_rect.contains(p))
    });

    TopbarOutput {
        bar_rect: Some(bar_rect),
        termd_chip_rect,
        notice_slot_rect,
        notice_banner_rect: Some(banner_rect),
        wants_pointer: pointer_over || area_resp.response.hovered(),
        wants_keyboard: false,
    }
}

fn measure_single_row_width(ctx: &egui::Context, wall_name: &str, button_labels: &[&str]) -> f32 {
    let font_id = egui::TextStyle::Button.resolve(&ctx.global_style());
    let trimmed = wall_name.trim();
    let (badge_label, dot_w) = if trimmed.is_empty() {
        ("(unnamed)", 0.0)
    } else {
        (trimmed, NAME_DOT_SPACE)
    };

    ctx.fonts_mut(|fonts| {
        let badge_text_w = fonts
            .layout_no_wrap(badge_label.to_owned(), font_id.clone(), NAME_FG)
            .size()
            .x;
        let mut total = badge_text_w + NAME_PAD_X * 2.0 + dot_w;
        for &lbl in button_labels {
            let text_w = fonts
                .layout_no_wrap(lbl.to_owned(), font_id.clone(), BTN_FG_IDLE)
                .size()
                .x;
            total += TOPBAR_ITEM_GAP_PX + text_w + BTN_PAD_X * 2.0;
        }
        total.ceil()
    })
}

fn draw_wall_name_badge(ui: &mut egui::Ui, wall_name: &str) {
    let trimmed = wall_name.trim();
    let (display_text, fg) = if trimmed.is_empty() {
        ("(unnamed)", NAME_EMPTY_FG)
    } else {
        (trimmed, NAME_FG)
    };

    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(display_text.to_owned(), font_id, fg);
    let dot_space = if trimmed.is_empty() { 0.0 } else { NAME_DOT_SPACE };
    let desired = egui::vec2(
        galley.size().x + NAME_PAD_X * 2.0 + dot_space,
        (galley.size().y + NAME_PAD_Y * 2.0).max(24.0),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            egui::CornerRadius::same(5),
            NAME_BG,
            egui::Stroke::new(1.0, NAME_BORDER),
            egui::StrokeKind::Inside,
        );
        let mut text_x = rect.min.x + NAME_PAD_X;
        if !trimmed.is_empty() {
            painter.circle_filled(egui::pos2(text_x + 3.0, rect.center().y), 2.5, NAME_DOT);
            text_x += dot_space;
        }
        let text_pos = egui::pos2(text_x, rect.center().y - galley.size().y * 0.5);
        painter.galley(text_pos, galley, fg);
    }
    resp.on_hover_text(if trimmed.is_empty() {
        "Wall namespace (unnamed)"
    } else {
        "Active wall"
    });
}

fn draw_topbar_button_with_rect(
    ui: &mut egui::Ui,
    label: &str,
    is_open: bool,
    tooltip: &str,
) -> (bool, egui::Rect) {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    let measure_galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font_id.clone(), BTN_FG_IDLE);
    let desired = egui::vec2(
        measure_galley.size().x + BTN_PAD_X * 2.0,
        (measure_galley.size().y + BTN_PAD_Y * 2.0).max(24.0),
    );

    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let hovered = resp.hovered();
    let clicked = resp.clicked();

    if ui.is_rect_visible(rect) {
        let (bg, border, fg) = if is_open {
            (BTN_BG_ACTIVE, BTN_BORDER_ACTIVE, BTN_FG_ACTIVE)
        } else if hovered {
            (BTN_BG_HOVER, BTN_BORDER_ACTIVE, BTN_FG_HOVER)
        } else {
            (BTN_BG_IDLE, BTN_BORDER_IDLE, BTN_FG_IDLE)
        };

        let painter = ui.painter();
        painter.rect(
            rect,
            egui::CornerRadius::same(5),
            bg,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        let galley = painter.layout_no_wrap(label.to_owned(), font_id, fg);
        let text_pos = egui::pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        );
        painter.galley(text_pos, galley, fg);
    }

    resp.on_hover_text(tooltip);
    (clicked, rect)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::wall::ui::configure_wall_context;

    fn make_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);
        ctx
    }

    fn run_headless(
        ctx: &egui::Context,
        raw: egui::RawInput,
        mut build: impl FnMut(&egui::Context),
    ) -> egui::FullOutput {
        let mut full = ctx.run_ui(raw, |ui| build(ui.ctx()));
        // Headless unit tests do not upload font textures to a GPU backend;
        // clear epaint's debug-assertion delta tracker before returning.
        full.textures_delta.clear();
        full
    }

    fn raw_with_viewport(w: f32, h: f32) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(w, h),
            )),
            time: Some(1.0),
            ..Default::default()
        }
    }

    #[test]
    fn fullscreen_topbar_emits_zero_shapes_and_no_repaint() {
        let ctx = make_ctx();
        let mut state = TopbarState::default();
        let input = TopbarInput {
            fullscreen: true,
            ..TopbarInput::default()
        };
        let mut verbs = Vec::new();
        let mut out_meta = TopbarOutput::default();

        let full = run_headless(&ctx, raw_with_viewport(1400.0, 900.0), |ctx| {
            out_meta = show(ctx, &mut state, &input, &mut verbs);
        });

        assert!(full.shapes.is_empty(), "fullscreen topbar must draw nothing");
        assert!(verbs.is_empty());
        assert_eq!(out_meta, TopbarOutput::default());
    }

    #[test]
    fn lays_out_on_frame_zero_at_multiple_sizes_and_leaves_room_for_termd_chip_and_notice() {
        let ctx = make_ctx();
        let mut state = TopbarState::default();
        let input = TopbarInput {
            wall_name: "",
            platform: Platform::Mac,
            inset: EdgeWidths { left: 260.0, right: 24.0 },
            termd_chip_width: 112.0,
            notice_width: 180.0,
            sidebar_open: true,
            ..TopbarInput::default()
        };

        for (step, (w, h)) in [
            (0usize, (1600.0, 900.0)),
            (1, (1024.0, 768.0)),
            (2, (520.0, 480.0)),
            (3, (1600.0, 900.0)),
        ] {
            let mut verbs = Vec::new();
            let mut top_out = TopbarOutput::default();
            let full = run_headless(&ctx, raw_with_viewport(w, h), |ctx| {
                top_out = show(ctx, &mut state, &input, &mut verbs);
            });
            assert!(
                !full.shapes.is_empty(),
                "topbar must emit visible shapes on frame {step} ({w}x{h})"
            );

            if step > 0 {
                let delay = full
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .map(|v| v.repaint_delay)
                    .unwrap_or(Duration::MAX);
                assert!(
                    delay >= Duration::from_secs(3600),
                    "settled static topbar must sleep idle ({w}x{h}), got repaint_delay={delay:?}"
                );
            }

            let bar = top_out.bar_rect.expect("bar_rect present");
            let chip = top_out.termd_chip_rect.expect("chip slot reserved");
            let notice_inline = top_out.notice_slot_rect.expect("inline notice slot reserved");
            let notice_banner = top_out.notice_banner_rect.expect("banner notice slot reserved");

            assert!((chip.width() - 112.0).abs() < 0.01);
            assert!((chip.max.x - (w - 24.0)).abs() < 0.01);

            if w >= 1400.0 {
                assert!(
                    bar.max.x <= chip.min.x - TOPBAR_ITEM_GAP_PX + 2.5,
                    "topbar {bar:?} must not overlap termd chip slot {chip:?}"
                );
                assert!(
                    bar.min.x >= notice_inline.max.x + TOPBAR_ITEM_GAP_PX - 1.0,
                    "topbar {bar:?} must not overlap inline notice slot {notice_inline:?}"
                );
            }
            assert!(
                notice_banner.min.y >= bar.max.y + 7.5,
                "banner notice {notice_banner:?} must sit below topbar {bar:?}"
            );
            assert!(!top_out.wants_keyboard);
            assert!(verbs.is_empty());
        }
    }

    #[test]
    fn clicking_every_topbar_button_emits_expected_verb_and_never_takes_keyboard() {
        let ctx = make_ctx();
        let mut state = TopbarState::default();
        let input = TopbarInput {
            wall_name: "agents-wall",
            platform: Platform::Other,
            ..TopbarInput::default()
        };

        let mut bar_rect = egui::Rect::NOTHING;
        let mut init_verbs = Vec::new();
        let _ = run_headless(&ctx, raw_with_viewport(1600.0, 900.0), |ctx| {
            bar_rect = show(ctx, &mut state, &input, &mut init_verbs)
                .bar_rect
                .expect("bar_rect");
        });

        let mut triggered = Vec::new();
        let y = bar_rect.center().y;
        let mut x = bar_rect.min.x + 2.0;
        while x < bar_rect.max.x - 2.0 {
            let pos = egui::pos2(x, y);
            let mut step_verbs = Vec::new();

            let mut press_raw = raw_with_viewport(1600.0, 900.0);
            press_raw.events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = run_headless(&ctx, press_raw, |ctx| {
                let _ = show(ctx, &mut state, &input, &mut step_verbs);
            });

            let mut release_raw = raw_with_viewport(1600.0, 900.0);
            release_raw.events = vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }];
            let _ = run_headless(&ctx, release_raw, |ctx| {
                let out_meta = show(ctx, &mut state, &input, &mut step_verbs);
                assert!(!out_meta.wants_keyboard);
            });
            assert!(
                !ctx.egui_wants_keyboard_input(),
                "clicking topbar button at x={x} must not capture keyboard focus"
            );

            for v in step_verbs {
                if triggered.last() != Some(&v) {
                    triggered.push(v);
                }
            }
            x += 12.0;
        }

        assert_eq!(
            triggered,
            vec![
                "wall.spawn",
                "wall.fit",
                "wall.palette toggle",
                "wall.sidebar toggle",
                "wall.status toggle",
                "wall.settings toggle",
                "wall.help toggle",
            ]
        );
    }
}
