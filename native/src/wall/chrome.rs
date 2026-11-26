//! Pure canvas-chrome builders turning plain descriptions into [`Shape`]s and
//! [`Label`]s (`wall/TitleBar.js`, `wall/GroupFrame.js`, `wall/SelectionOverlay.js`,
//! `wall/MapLabels.js`, `wall/WallFullscreen.js`, `wall/StatusWindow.jsx`, and
//! `wall/main.jsx`).
//!
//! Every pixel size in [`ChromeStyle`] is specified in logical CSS pixels and
//! multiplied by [`ChromeView::scale_factor`] (`1.0` standard, `2.0` Retina)
//! when converted to physical screen pixels (`stroke_px`, `corner_radius_px`,
//! `DashStyle`, `LabelHeight::ScreenPx`) or to world units (`px * scale / zoom`).

use std::collections::HashMap;

use glam::Vec2;
use glyph_font::{FamilyId, FontAtlas};
use term_cells::{Cursor, CursorShape, Screen, attrs, rgb};
use wall_core::labels::{Candidate, LabelItem, LabelView, Leader, Placement, ScreenBox, place_labels};
use wall_core::sidebar::AgentStatus;

use super::camera::WallCamera;
use super::labels::{Label, LabelHeight, measure_label_width};
use super::metrics::{CellMetrics, WorldRect};
use super::paper::PaperStyle;
use super::shapes::{DashStyle, LeaderShape, PolygonShape, RectShape, Shape};
use super::srgb_hex;
use super::test_pattern::{RunStyle, TestPatternChrome, TestPatternTerminal, write_run};

/// Visual sizes (in logical CSS pixels unless noted as world units or fractions)
/// and sRGB RGBA colours for all in-scene wall chrome, defaulting to the exact
/// values from the JS wall.
#[derive(Clone, Debug, PartialEq)]
pub struct ChromeStyle {
    // ── Title bar (`wall/TitleBar.js` + `wall/WallNames.js`) ──────────────────
    /// Logical on-screen height of a title bar or group header (`TITLE_BAR_PX = 22`).
    pub title_bar_px: f32,
    /// Maximum world height of a standard title bar (`BAR_ROOM = 1.6` cell rows).
    pub bar_max_world: f32,
    /// Smallest logical pixel height a terminal bar's text may shrink to before
    /// handing off to a map label (`MIN_TEXT_PX = 8`).
    pub min_text_px: f32,
    /// Hysteresis multiplier when returning from map label to bar (`FIT_HYSTERESIS = 1.1`).
    pub fit_hysteresis: f32,
    /// Natural text height as a fraction of bar height (`TEXT_FRACTION = 0.6`).
    pub title_text_fraction: f32,
    /// Horizontal text inset on each end in multiples of bar height (`TEXT_INSET = 0.45`).
    pub title_text_inset: f32,
    /// Extra right-hand room reserved for hover bar buttons, in bar heights (`BAR_BUTTON_ROOM = 3.15`).
    pub title_hover_reserve_bars: f32,
    pub bar_fill_idle: [f32; 4],
    pub bar_fill_focused: [f32; 4],
    pub bar_fill_hovered: [f32; 4],
    pub bar_fill_focused_hovered: [f32; 4],
    pub bar_text_idle: [f32; 4],
    pub bar_text_focused: [f32; 4],
    pub bar_text_hovered: [f32; 4],
    pub bar_hover_stroke: [f32; 4],
    pub bar_agent_sep_color: [f32; 4],

    // ── Group frame & header (`wall/GroupFrame.js`) ──────────────────────────
    /// World Z offset of a group's header strip above its backdrop (`HEADER_Z_OFFSET = 0.2`).
    pub group_header_z_offset: f32,
    pub group_backdrop_fill: [f32; 4],
    pub group_border_color: [f32; 4],
    pub group_border_px: f32,
    pub group_header_fill_idle: [f32; 4],
    pub group_header_fill_focused: [f32; 4],
    pub group_header_fill_hovered: [f32; 4],
    pub group_header_text_idle: [f32; 4],
    pub group_header_text_focused: [f32; 4],
    pub group_header_text_hovered: [f32; 4],

    // ── Selection outline, lasso & drop preview (`wall/SelectionOverlay.js`) ─
    /// Logical gap outside a selected window (`OUTLINE_GAP = 3`).
    pub selection_gap_px: f32,
    pub selection_stroke_px: f32,
    pub selection_corner_radius_px: f32,
    pub selection_color: [f32; 4],

    pub lasso_fill: [f32; 4],
    pub lasso_stroke: [f32; 4],
    pub lasso_stroke_px: f32,
    pub lasso_dash_px: f32,
    pub lasso_gap_px: f32,

    pub drop_stroke_px: f32,
    pub drop_corner_radius_px: f32,
    pub drop_ok_fill: [f32; 4],
    pub drop_ok_stroke: [f32; 4],
    pub drop_refused_fill: [f32; 4],
    pub drop_refused_stroke: [f32; 4],
    pub drop_refused_dash_px: f32,
    pub drop_refused_gap_px: f32,

    // ── Map labels (`wall/MapLabels.js`) ─────────────────────────────────────
    pub map_label_font_px: f32,
    pub map_label_height_px: f32,
    pub map_label_pad_x_px: f32,
    pub map_label_border_px: f32,
    pub map_label_corner_radius_px: f32,
    pub leader_width_px: f32,
    pub leader_dot_radius_px: f32,
    pub map_idle_fill: [f32; 4],
    pub map_idle_border: [f32; 4],
    pub map_idle_text: [f32; 4],
    pub map_idle_leader: [f32; 4],
    pub map_hovered_fill: [f32; 4],
    pub map_hovered_border: [f32; 4],
    pub map_hovered_text: [f32; 4],
    pub map_hovered_leader: [f32; 4],
    pub map_focused_fill: [f32; 4],
    pub map_focused_border: [f32; 4],
    pub map_focused_text: [f32; 4],
    pub map_focused_leader: [f32; 4],

    // ── Notice banner (`wall/main.jsx` + `wall/StatusWindow.jsx`) ────────────
    pub notice_top_px: f32,
    pub notice_edge_margin_px: f32,
    pub notice_max_width_fraction: f32,
    pub notice_font_px: f32,
    pub notice_pad_x_px: f32,
    pub notice_pad_y_px: f32,
    pub notice_border_px: f32,
    pub notice_corner_radius_px: f32,
    pub notice_amber_fill: [f32; 4],
    pub notice_amber_border: [f32; 4],
    pub notice_amber_text: [f32; 4],
    pub notice_red_fill: [f32; 4],
    pub notice_red_border: [f32; 4],
    pub notice_red_text: [f32; 4],

    // ── Fullscreen veil (`wall/WallFullscreen.js` + `wall/index.html`) ───────
    /// Veil RGBA colour (`#07080b` at `BACKDROP_OPACITY = 0.90`).
    pub veil_fill: [f32; 4],
}

impl Default for ChromeStyle {
    fn default() -> Self {
        Self {
            title_bar_px: 22.0,
            bar_max_world: 1.6,
            min_text_px: 8.0,
            fit_hysteresis: 1.1,
            title_text_fraction: 0.60,
            title_text_inset: 0.45,
            title_hover_reserve_bars: 3.15,
            bar_fill_idle: srgb_hex(0x1f2633, 1.0),
            bar_fill_focused: srgb_hex(0x16402c, 1.0),
            bar_fill_hovered: srgb_hex(0x273042, 1.0),
            bar_fill_focused_hovered: srgb_hex(0x1c4f37, 1.0),
            bar_text_idle: srgb_hex(0xc4ccd8, 1.0),
            bar_text_focused: srgb_hex(0x8ef0b8, 1.0),
            bar_text_hovered: srgb_hex(0xe6e9ef, 1.0),
            bar_hover_stroke: srgb_hex(0x4e596f, 0.95),
            bar_agent_sep_color: srgb_hex(0x6b7383, 1.0),

            group_header_z_offset: 0.2,
            group_backdrop_fill: srgb_hex(0x0d1017, 1.0),
            group_border_color: srgb_hex(0x262d3a, 1.0),
            group_border_px: 1.0,
            group_header_fill_idle: srgb_hex(0x121722, 1.0),
            group_header_fill_focused: srgb_hex(0x12261d, 1.0),
            group_header_fill_hovered: srgb_hex(0x181f2e, 1.0),
            group_header_text_idle: srgb_hex(0x8b95a7, 1.0),
            group_header_text_focused: srgb_hex(0x8ef0b8, 1.0),
            group_header_text_hovered: srgb_hex(0xc4ccd8, 1.0),

            selection_gap_px: 3.0,
            selection_stroke_px: 1.25,
            selection_corner_radius_px: 3.0,
            selection_color: srgb_hex(0x8ab4f8, 0.85),

            lasso_fill: srgb_hex(0x8ab4f8, 0.06),
            lasso_stroke: srgb_hex(0x8ab4f8, 0.85),
            lasso_stroke_px: 1.0,
            lasso_dash_px: 4.0,
            lasso_gap_px: 3.0,

            drop_stroke_px: 2.0,
            drop_corner_radius_px: 3.0,
            drop_ok_fill: srgb_hex(0x8ab4f8, 0.14),
            drop_ok_stroke: srgb_hex(0x8ab4f8, 0.85),
            drop_refused_fill: srgb_hex(0xf08070, 0.08),
            drop_refused_stroke: srgb_hex(0xf08070, 0.85),
            drop_refused_dash_px: 6.0,
            drop_refused_gap_px: 4.0,

            map_label_font_px: 12.0,
            map_label_height_px: 18.0,
            map_label_pad_x_px: 6.0,
            map_label_border_px: 1.0,
            map_label_corner_radius_px: 4.0,
            leader_width_px: 1.0,
            leader_dot_radius_px: 2.0,
            map_idle_fill: srgb_hex(0x181d27, 0.94),
            map_idle_border: srgb_hex(0x323b4c, 1.0),
            map_idle_text: srgb_hex(0xc4ccd8, 1.0),
            map_idle_leader: srgb_hex(0x8c9bb4, 0.65),
            map_hovered_fill: srgb_hex(0x1f2634, 0.96),
            map_hovered_border: srgb_hex(0x4e596f, 1.0),
            map_hovered_text: srgb_hex(0xe6e9ef, 1.0),
            map_hovered_leader: srgb_hex(0xb4c3dc, 0.80),
            map_focused_fill: srgb_hex(0x102c1f, 0.95),
            map_focused_border: srgb_hex(0x2f7a52, 1.0),
            map_focused_text: srgb_hex(0x8ef0b8, 1.0),
            map_focused_leader: srgb_hex(0x6ee7a0, 0.80),

            notice_top_px: 44.0,
            notice_edge_margin_px: 12.0,
            notice_max_width_fraction: 0.80,
            notice_font_px: 12.0,
            notice_pad_x_px: 10.0,
            notice_pad_y_px: 5.0,
            notice_border_px: 1.0,
            notice_corner_radius_px: 5.0,
            notice_amber_fill: srgb_hex(0x2a1a12, 0.96),
            notice_amber_border: srgb_hex(0x5a3422, 1.0),
            notice_amber_text: srgb_hex(0xf0b089, 1.0),
            notice_red_fill: srgb_hex(0x2a1416, 0.96),
            notice_red_border: srgb_hex(0x6e2b32, 1.0),
            notice_red_text: srgb_hex(0xf28b82, 1.0),

            veil_fill: srgb_hex(0x07080b, 0.90),
        }
    }
}

/// Camera zoom and display scale factor shared across chrome builders.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChromeView {
    /// Physical screen pixels per world unit (`WallCamera::zoom`).
    pub zoom: f32,
    /// Display backing scale factor (`1.0` standard, `2.0` Retina).
    pub scale_factor: f32,
}

impl ChromeView {
    pub const fn new(zoom: f32, scale_factor: f32) -> Self {
        Self { zoom, scale_factor }
    }

    #[inline]
    pub fn safe_zoom(self) -> f32 {
        if self.zoom.is_finite() && self.zoom > 0.0 {
            self.zoom
        } else {
            24.0
        }
    }

    #[inline]
    pub fn safe_scale(self) -> f32 {
        if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        }
    }

    /// Convert a logical CSS-pixel measurement to physical screen pixels.
    #[inline]
    pub fn phys_px(self, css_px: f32) -> f32 {
        css_px * self.safe_scale()
    }

    /// Convert a logical CSS-pixel measurement to world units at `zoom`.
    #[inline]
    pub fn css_to_world(self, css_px: f32) -> f32 {
        self.phys_px(css_px) / self.safe_zoom()
    }
}

impl Default for ChromeView {
    fn default() -> Self {
        Self {
            zoom: 24.0,
            scale_factor: 1.0,
        }
    }
}

/// Font metrics, mutable [`FontAtlas`], and active [`FamilyId`] bundled for
/// chrome builders that measure or emit text [`Label`]s.
pub struct ChromeFonts<'a> {
    pub metrics: CellMetrics,
    pub atlas: &'a mut FontAtlas,
    pub family: FamilyId,
}

impl<'a> ChromeFonts<'a> {
    pub fn new(metrics: CellMetrics, atlas: &'a mut FontAtlas, family: FamilyId) -> Self {
        Self {
            metrics,
            atlas,
            family,
        }
    }
}

fn agent_status_srgb(status: AgentStatus) -> [f32; 4] {
    match status {
        AgentStatus::Waiting => srgb_hex(0xf2b45a, 1.0),
        AgentStatus::Error => srgb_hex(0xef6f6c, 1.0),
        AgentStatus::Processing => srgb_hex(0x7fb0ff, 1.0),
        AgentStatus::Idle => srgb_hex(0x8ef0b8, 1.0),
        AgentStatus::Unknown => srgb_hex(0x8b95a7, 1.0),
        AgentStatus::Ended => srgb_hex(0x5d6574, 1.0),
    }
}

/// Unproject a physical-pixel screen rectangle (`left <= right`, `top <= bottom`,
/// origin top-left, `+Y` down) into a [`WorldRect`] (`+Y` up).
pub fn phys_screen_rect_to_world(
    camera: &WallCamera,
    viewport: (u32, u32),
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
) -> WorldRect {
    let tl = camera.screen_to_world(Vec2::new(left, top), viewport);
    let br = camera.screen_to_world(Vec2::new(right, bottom), viewport);
    WorldRect::new(Vec2::new(tl.x, br.y), Vec2::new(br.x, tl.y))
}

/// Unproject a logical CSS-pixel [`ScreenBox`] (from `wall_core::labels` or
/// `wall_core::camera`) into a [`WorldRect`], multiplying by `scale_factor`.
pub fn screen_box_to_world_rect(
    box_css: ScreenBox,
    camera: &WallCamera,
    viewport: (u32, u32),
    scale_factor: f32,
) -> WorldRect {
    let s = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    phys_screen_rect_to_world(
        camera,
        viewport,
        box_css.left as f32 * s,
        box_css.top as f32 * s,
        box_css.right as f32 * s,
        box_css.bottom as f32 * s,
    )
}

/// Project a [`WorldRect`] (`+Y` up) into a logical CSS-pixel [`ScreenBox`]
/// (`+Y` down) for [`wall_core::labels::place_labels`], dividing physical
/// screen coordinates by `scale_factor`.
pub fn world_rect_to_screen_box(
    rect: WorldRect,
    camera: &WallCamera,
    viewport: (u32, u32),
    scale_factor: f32,
) -> ScreenBox {
    let inv_s = if scale_factor.is_finite() && scale_factor > 0.0 {
        1.0 / f64::from(scale_factor)
    } else {
        1.0
    };
    let tl = camera.world_to_screen(Vec2::new(rect.min.x, rect.max.y), viewport);
    let br = camera.world_to_screen(Vec2::new(rect.max.x, rect.min.y), viewport);
    ScreenBox::new(
        f64::from(tl.x) * inv_s,
        f64::from(tl.y) * inv_s,
        f64::from(br.x) * inv_s,
        f64::from(br.y) * inv_s,
    )
}

// ── 1. Title Bar (`wall/TitleBar.js` + `wall/WallNames.js` + `TerminalWall.js`) ──

/// Format an agent-attached terminal's combined title (`<name> · <status>`,
/// matching `TerminalWall.titleOf` in `wall/TerminalWall.js`).
pub fn format_agent_title(name: &str, status: AgentStatus) -> String {
    let suffix = format!(" · {}", status.word());
    if name.ends_with(&suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

/// Plain-data description of a terminal's title bar.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleBarDesc<'a> {
    /// World rectangle of the terminal's body panel (`strip` sits immediately above `body_rect.max.y`).
    pub body_rect: WorldRect,
    /// World depth of the terminal.
    pub z: f32,
    /// Base title string (or pre-formatted title).
    pub title: &'a str,
    /// Optional gomuxai agent status when attached (`Some(status)` renders `<name> · <status>` with coloured status word).
    pub agent_status: Option<AgentStatus>,
    /// Whether this terminal holds keyboard focus.
    pub focused: bool,
    /// Whether the pointer currently hovers this terminal.
    pub hovered: bool,
    /// Whether this terminal belongs to the multi-terminal selection.
    pub selected: bool,
    /// Whether the bar should draw its text (`false` when a map label carries the name instead).
    pub name_visible: bool,
    /// Previous frame's fit state for `FIT_HYSTERESIS` (`true` on first frame).
    pub prev_fits: bool,
    /// Optional per-terminal world height cap (e.g. split `bar_room`; `Some(0.0)` suppresses the bar; `None` uses `style.bar_max_world`).
    pub max_world_height: Option<f32>,
    /// Reserve right-hand space for hover bar buttons (`true` reserves `style.title_hover_reserve_bars * bar_h`; automatically enabled when `hovered`).
    pub reserve_button_room: bool,
}

impl<'a> TitleBarDesc<'a> {
    pub fn new(body_rect: WorldRect, z: f32, title: &'a str) -> Self {
        Self {
            body_rect,
            z,
            title,
            agent_status: None,
            focused: false,
            hovered: false,
            selected: false,
            name_visible: true,
            prev_fits: true,
            max_world_height: None,
            reserve_button_room: false,
        }
    }
}

/// Computed geometry and fit verdict returned by [`push_title_bar`] / [`build_title_bar`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TitleBarLayoutOut {
    /// World rectangle occupied by the title bar strip (`height == 0` when hidden by `max_world_height == Some(0.0)`).
    pub strip_rect: WorldRect,
    /// Combined window rectangle (`body_rect` union `strip_rect`).
    pub window_rect: WorldRect,
    /// Resolved world line height of the title text (`0.0` if empty or zero room).
    pub text_world_height: f32,
    /// Whether the full title fits at or above `min_text_px` (with `fit_hysteresis`).
    pub name_fits: bool,
}

/// Compute a title bar's world height (`min(title_bar_px * scale_factor / zoom, max_world)`).
pub fn title_bar_world_height(
    style: &ChromeStyle,
    view: ChromeView,
    max_world_override: Option<f32>,
) -> f32 {
    let cap = max_world_override.unwrap_or(style.bar_max_world);
    if !cap.is_finite() || cap <= 0.0 {
        return 0.0;
    }
    view.css_to_world(style.title_bar_px).min(cap)
}

/// Append a terminal title bar's [`Shape`] and [`Label`]s into `shapes` and `labels`.
pub fn push_title_bar(
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &TitleBarDesc<'_>,
) -> TitleBarLayoutOut {
    let bar_h = title_bar_world_height(style, view, desc.max_world_height);
    if bar_h <= 0.0 || desc.body_rect.width() <= 0.0 {
        let collapsed = WorldRect::new(
            Vec2::new(desc.body_rect.min.x, desc.body_rect.max.y),
            Vec2::new(desc.body_rect.max.x, desc.body_rect.max.y),
        );
        return TitleBarLayoutOut {
            strip_rect: collapsed,
            window_rect: desc.body_rect,
            text_world_height: 0.0,
            name_fits: false,
        };
    }

    let strip = WorldRect::new(
        Vec2::new(desc.body_rect.min.x, desc.body_rect.max.y),
        Vec2::new(desc.body_rect.max.x, desc.body_rect.max.y + bar_h),
    );
    let fill = match (desc.focused, desc.hovered) {
        (true, true) => style.bar_fill_focused_hovered,
        (true, false) => style.bar_fill_focused,
        (false, true) => style.bar_fill_hovered,
        (false, false) => style.bar_fill_idle,
    };
    let text_color = if desc.focused {
        style.bar_text_focused
    } else if desc.hovered {
        style.bar_text_hovered
    } else {
        style.bar_text_idle
    };
    let stroke = if desc.selected {
        Some(style.selection_color)
    } else if desc.hovered {
        Some(style.bar_hover_stroke)
    } else {
        None
    };

    shapes.push(Shape::Rect(RectShape {
        rect: strip,
        z: desc.z,
        fill: Some(fill),
        stroke,
        stroke_px: if stroke.is_some() {
            view.phys_px(1.0)
        } else {
            0.0
        },
        dash: DashStyle::SOLID,
        corner_radius_px: 0.0,
    }));

    let full_title = match desc.agent_status {
        Some(st) => format_agent_title(desc.title, st),
        None => desc.title.to_string(),
    };
    let inset = bar_h * style.title_text_inset;
    let reserve = if desc.hovered || desc.reserve_button_room {
        style.title_hover_reserve_bars * bar_h
    } else {
        0.0
    };
    let room = (strip.width() - 2.0 * inset - reserve).max(0.0);
    let natural_h = bar_h * style.title_text_fraction;
    let full_w = measure_label_width(
        fonts.metrics,
        fonts.atlas,
        fonts.family,
        &full_title,
        natural_h,
    );
    let text_h = if room <= 0.0 {
        0.0
    } else if full_w > room && full_w > 1e-6 {
        natural_h * (room / full_w)
    } else {
        natural_h
    };

    let phys_line_px = text_h * view.safe_zoom();
    let floor_phys_px = view.phys_px(style.min_text_px)
        * if desc.prev_fits {
            1.0
        } else {
            style.fit_hysteresis
        };
    let name_fits = full_title.is_empty() || (room > 0.0 && phys_line_px >= floor_phys_px);

    if desc.name_visible && name_fits && text_h > 1e-5 && !full_title.is_empty() {
        let x0 = strip.min.x + inset;
        let top_y = strip.center().y + 0.5 * text_h;
        let clip_rect = WorldRect::new(
            Vec2::new(strip.min.x, strip.min.y),
            Vec2::new((strip.max.x - reserve).max(strip.min.x), strip.max.y),
        );
        if let Some(st) = desc.agent_status {
            let suffix = format!(" · {}", st.word());
            let base_name = full_title.strip_suffix(&suffix).unwrap_or(desc.title);
            let w_name =
                measure_label_width(fonts.metrics, fonts.atlas, fonts.family, base_name, text_h);
            let sep = " · ";
            let w_sep = measure_label_width(fonts.metrics, fonts.atlas, fonts.family, sep, text_h);
            if !base_name.is_empty() {
                labels.push(
                    Label::new(
                        base_name.to_string(),
                        Vec2::new(x0, top_y),
                        LabelHeight::World(text_h),
                        text_color,
                        desc.z,
                    )
                    .with_clip(clip_rect),
                );
            }
            labels.push(
                Label::new(
                    sep,
                    Vec2::new(x0 + w_name, top_y),
                    LabelHeight::World(text_h),
                    style.bar_agent_sep_color,
                    desc.z,
                )
                .with_clip(clip_rect),
            );
            labels.push(
                Label::new(
                    st.word(),
                    Vec2::new(x0 + w_name + w_sep, top_y),
                    LabelHeight::World(text_h),
                    agent_status_srgb(st),
                    desc.z,
                )
                .with_clip(clip_rect),
            );
        } else {
            labels.push(
                Label::new(
                    full_title,
                    Vec2::new(x0, top_y),
                    LabelHeight::World(text_h),
                    text_color,
                    desc.z,
                )
                .with_clip(clip_rect),
            );
        }
    }

    TitleBarLayoutOut {
        strip_rect: strip,
        window_rect: desc.body_rect.union(strip),
        text_world_height: text_h,
        name_fits,
    }
}

/// Standalone title-bar builder returning `(TitleBarLayoutOut, Vec<Shape>, Vec<Label<'static>>)`.
pub fn build_title_bar(
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &TitleBarDesc<'_>,
) -> (TitleBarLayoutOut, Vec<Shape>, Vec<Label<'static>>) {
    let mut shapes = Vec::with_capacity(1);
    let mut labels = Vec::with_capacity(3);
    let layout = push_title_bar(&mut shapes, &mut labels, fonts, style, view, desc);
    (layout, shapes, labels)
}

// ── 2. Group Frame & Header (`wall/GroupFrame.js`) ────────────────────────────

/// Plain-data description of a group's backdrop frame and header strip.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupFrameDesc<'a> {
    /// Padded world rectangle enclosing the group's shown member windows (`backdrop` rect).
    pub backdrop_rect: WorldRect,
    /// World depth of the backdrop (`header` sits at `z + style.group_header_z_offset`).
    pub z: f32,
    /// Group header title string.
    pub name: &'a str,
    /// True when one of the group's members holds focus.
    pub focused: bool,
    /// True when the pointer hovers the group's header strip.
    pub hovered: bool,
}

/// Geometry returned by [`push_group_frame`] / [`build_group_frame`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupFrameLayoutOut {
    pub backdrop_rect: WorldRect,
    pub header_rect: WorldRect,
    pub full_rect: WorldRect,
}

/// Append a group's backdrop rectangle, header strip, and shrunk-to-fit header
/// [`Label`] into `shapes` and `labels`.
pub fn push_group_frame(
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &GroupFrameDesc<'_>,
) -> GroupFrameLayoutOut {
    let border_px = view.phys_px(style.group_border_px);
    shapes.push(Shape::Rect(RectShape {
        rect: desc.backdrop_rect,
        z: desc.z,
        fill: Some(style.group_backdrop_fill),
        stroke: Some(style.group_border_color),
        stroke_px: border_px,
        dash: DashStyle::SOLID,
        corner_radius_px: 0.0,
    }));

    let bar_h = title_bar_world_height(style, view, None);
    let header_rect = WorldRect::new(
        Vec2::new(desc.backdrop_rect.min.x, desc.backdrop_rect.max.y),
        Vec2::new(desc.backdrop_rect.max.x, desc.backdrop_rect.max.y + bar_h),
    );
    let header_z = desc.z + style.group_header_z_offset;
    let header_fill = if desc.focused {
        style.group_header_fill_focused
    } else if desc.hovered {
        style.group_header_fill_hovered
    } else {
        style.group_header_fill_idle
    };
    let header_text = if desc.focused {
        style.group_header_text_focused
    } else if desc.hovered {
        style.group_header_text_hovered
    } else {
        style.group_header_text_idle
    };
    let header_stroke = if desc.hovered {
        style.bar_hover_stroke
    } else {
        style.group_border_color
    };

    if bar_h > 0.0 && header_rect.width() > 0.0 {
        shapes.push(Shape::Rect(RectShape {
            rect: header_rect,
            z: header_z,
            fill: Some(header_fill),
            stroke: Some(header_stroke),
            stroke_px: border_px,
            dash: DashStyle::SOLID,
            corner_radius_px: 0.0,
        }));

        // `GroupFrame.js` passes `minTextPx: 0`, so a group header always shrinks
        // its title to fit rather than handing off to a map label.
        let inset = bar_h * style.title_text_inset;
        let room = (header_rect.width() - 2.0 * inset).max(0.0);
        let natural_h = bar_h * style.title_text_fraction;
        let full_w =
            measure_label_width(fonts.metrics, fonts.atlas, fonts.family, desc.name, natural_h);
        let text_h = if room <= 0.0 {
            0.0
        } else if full_w > room && full_w > 1e-6 {
            natural_h * (room / full_w)
        } else {
            natural_h
        };
        if text_h > 1e-5 && !desc.name.is_empty() {
            labels.push(
                Label::new(
                    desc.name.to_string(),
                    Vec2::new(header_rect.min.x + inset, header_rect.center().y + 0.5 * text_h),
                    LabelHeight::World(text_h),
                    header_text,
                    header_z,
                )
                .with_clip(header_rect),
            );
        }
    }

    GroupFrameLayoutOut {
        backdrop_rect: desc.backdrop_rect,
        header_rect,
        full_rect: desc.backdrop_rect.union(header_rect),
    }
}

/// Standalone group-frame builder returning `(GroupFrameLayoutOut, Vec<Shape>, Vec<Label<'static>>)`.
pub fn build_group_frame(
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &GroupFrameDesc<'_>,
) -> (GroupFrameLayoutOut, Vec<Shape>, Vec<Label<'static>>) {
    let mut shapes = Vec::with_capacity(2);
    let mut labels = Vec::with_capacity(1);
    let layout = push_group_frame(&mut shapes, &mut labels, fonts, style, view, desc);
    (layout, shapes, labels)
}

// ── 3. Selection Outline (`wall/SelectionOverlay.js`) ─────────────────────────

/// Plain-data description of a selection outline around one selected window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionOutlineDesc {
    /// World rectangle of the selected window (typically `body_rect.union(strip_rect)`).
    pub window_rect: WorldRect,
    /// World depth of the outline.
    pub z: f32,
}

/// Append a selection outline (`selection_gap_px` outside `window_rect`) to `shapes`,
/// returning the expanded outline world rectangle.
pub fn push_selection_outline(
    shapes: &mut Vec<Shape>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: SelectionOutlineDesc,
) -> WorldRect {
    let gap_world = view.css_to_world(style.selection_gap_px);
    let rect = desc.window_rect.expand(gap_world);
    shapes.push(Shape::Rect(RectShape {
        rect,
        z: desc.z,
        fill: None,
        stroke: Some(style.selection_color),
        stroke_px: view.phys_px(style.selection_stroke_px),
        dash: DashStyle::SOLID,
        corner_radius_px: view.phys_px(style.selection_corner_radius_px),
    }));
    rect
}

/// Standalone selection-outline builder returning `Vec<Shape>`.
pub fn build_selection_outline(
    style: &ChromeStyle,
    view: ChromeView,
    desc: SelectionOutlineDesc,
) -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(1);
    push_selection_outline(&mut shapes, style, view, desc);
    shapes
}

// ── 4. Lasso (`wall/SelectionOverlay.js`) ─────────────────────────────────────

/// Plain-data description of an active selection lasso polygon in world space.
#[derive(Clone, Debug, PartialEq)]
pub struct LassoDesc<'a> {
    pub points: &'a [Vec2],
    pub z: f32,
}

/// Append a translucent dashed selection lasso polygon to `shapes`.
pub fn push_lasso(
    shapes: &mut Vec<Shape>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: LassoDesc<'_>,
) {
    if desc.points.len() < 2 {
        return;
    }
    shapes.push(Shape::Polygon(PolygonShape {
        points: desc.points.to_vec(),
        z: desc.z,
        fill: Some(style.lasso_fill),
        stroke: style.lasso_stroke,
        stroke_px: view.phys_px(style.lasso_stroke_px),
        dash: DashStyle::new(
            view.phys_px(style.lasso_dash_px),
            view.phys_px(style.lasso_gap_px),
        ),
    }));
}

/// Standalone lasso builder returning `Vec<Shape>`.
pub fn build_lasso(style: &ChromeStyle, view: ChromeView, desc: LassoDesc<'_>) -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(1);
    push_lasso(&mut shapes, style, view, desc);
    shapes
}

// ── 5. Drop Preview (`wall/SelectionOverlay.js`) ──────────────────────────────

/// Plain-data description of a tile/split drop-target preview rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropPreviewDesc {
    pub rect: WorldRect,
    pub z: f32,
    /// `true` = accepted (solid blue border + blue tint); `false` = refused (dashed red border + coral tint).
    pub accepted: bool,
}

/// Append an accepted or refused drop-preview rectangle to `shapes`.
pub fn push_drop_preview(
    shapes: &mut Vec<Shape>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: DropPreviewDesc,
) {
    if desc.rect.width() <= 0.0 || desc.rect.height() <= 0.0 {
        return;
    }
    let (fill, stroke, dash) = if desc.accepted {
        (style.drop_ok_fill, style.drop_ok_stroke, DashStyle::SOLID)
    } else {
        (
            style.drop_refused_fill,
            style.drop_refused_stroke,
            DashStyle::new(
                view.phys_px(style.drop_refused_dash_px),
                view.phys_px(style.drop_refused_gap_px),
            ),
        )
    };
    shapes.push(Shape::Rect(RectShape {
        rect: desc.rect,
        z: desc.z,
        fill: Some(fill),
        stroke: Some(stroke),
        stroke_px: view.phys_px(style.drop_stroke_px),
        dash,
        corner_radius_px: view.phys_px(style.drop_corner_radius_px),
    }));
}

/// Standalone drop-preview builder returning `Vec<Shape>`.
pub fn build_drop_preview(
    style: &ChromeStyle,
    view: ChromeView,
    desc: DropPreviewDesc,
) -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(1);
    push_drop_preview(&mut shapes, style, view, desc);
    shapes
}

// ── 6. Map Label with Pill and Leader (`wall/MapLabels.js` + `wall_core::labels`) ──

/// Measure the logical CSS-pixel `(width, height)` of a map-label pill for `text`,
/// ready to pass into [`wall_core::labels::LabelItem`].
pub fn measure_map_label_css(
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    text: &str,
) -> (f64, f64) {
    let text_w_css = measure_label_width(
        fonts.metrics,
        fonts.atlas,
        fonts.family,
        text,
        style.map_label_font_px,
    );
    let w = (f64::from(text_w_css).ceil()) + 2.0 * f64::from(style.map_label_pad_x_px) + 2.0;
    (w, f64::from(style.map_label_height_px))
}

/// How a map label's pill and optional leader line are positioned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MapLabelGeometry {
    /// Already resolved in world space (`pill_min` is the bottom-left corner of
    /// the pill in world units, or pass `pill_rect` via [`MapLabelGeometry::WorldPill`];
    /// `leader` is `(from_on_pill, to_on_target)`).
    WorldPill {
        pill_rect: WorldRect,
        leader: Option<(Vec2, Vec2)>,
    },
    /// World bottom-left corner of the pill, auto-sizing width/height from the
    /// text and `ChromeStyle` at `view`.
    WorldAutoSize {
        pill_min: Vec2,
        leader_to: Option<Vec2>,
    },
    /// Logical CSS-pixel [`Placement`] produced by `wall_core::labels::place_labels`,
    /// unprojected through `camera` and `viewport` (physical px).
    Placed {
        placement: Placement,
        camera: WallCamera,
        viewport: (u32, u32),
    },
}

/// Plain-data description of one map label.
#[derive(Clone, Debug, PartialEq)]
pub struct MapLabelDesc<'a> {
    pub text: &'a str,
    pub focused: bool,
    pub hovered: bool,
    /// World depth of the pill (`leader` draws slightly behind at `z - 0.1`).
    pub z: f32,
    pub geometry: MapLabelGeometry,
}

/// Append a map-label pill, its `ScreenPx` [`Label`], and its optional [`LeaderShape`]
/// (segment + anchor dot) into `shapes` and `labels`, returning the pill's [`WorldRect`].
pub fn push_map_label(
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &MapLabelDesc<'_>,
) -> WorldRect {
    let font_phys_px = view.phys_px(style.map_label_font_px);
    let text_h_world = font_phys_px / view.safe_zoom();
    let pad_x_world = view.css_to_world(style.map_label_pad_x_px);

    let (pill_rect, leader_pts): (WorldRect, Option<(Vec2, Vec2)>) = match desc.geometry {
        MapLabelGeometry::WorldPill { pill_rect, leader } => (pill_rect, leader),
        MapLabelGeometry::WorldAutoSize {
            pill_min,
            leader_to,
        } => {
            let text_w_world = measure_label_width(
                fonts.metrics,
                fonts.atlas,
                fonts.family,
                desc.text,
                text_h_world,
            );
            let pill_w = text_w_world + 2.0 * pad_x_world;
            let pill_h = view.css_to_world(style.map_label_height_px);
            let pill = WorldRect::new(pill_min, pill_min + Vec2::new(pill_w, pill_h));
            let leader = leader_to.map(|target_pt| {
                // Connect from nearest point on pill border to target_pt.
                let from = Vec2::new(
                    target_pt.x.clamp(pill.min.x, pill.max.x),
                    target_pt.y.clamp(pill.min.y, pill.max.y),
                );
                (from, target_pt)
            });
            (pill, leader)
        }
        MapLabelGeometry::Placed {
            placement,
            camera,
            viewport,
        } => {
            let s = view.safe_scale();
            let pill = screen_box_to_world_rect(placement.label_box, &camera, viewport, s);
            let leader = placement.leader.map(|Leader { x1, y1, x2, y2 }| {
                let from =
                    camera.screen_to_world(Vec2::new(x1 as f32 * s, y1 as f32 * s), viewport);
                let to = camera.screen_to_world(Vec2::new(x2 as f32 * s, y2 as f32 * s), viewport);
                (from, to)
            });
            (pill, leader)
        }
    };

    let (fill, border, text_col, leader_col) = if desc.focused {
        (
            style.map_focused_fill,
            style.map_focused_border,
            style.map_focused_text,
            style.map_focused_leader,
        )
    } else if desc.hovered {
        (
            style.map_hovered_fill,
            style.map_hovered_border,
            style.map_hovered_text,
            style.map_hovered_leader,
        )
    } else {
        (
            style.map_idle_fill,
            style.map_idle_border,
            style.map_idle_text,
            style.map_idle_leader,
        )
    };

    if let Some((from, to)) = leader_pts {
        shapes.push(Shape::Leader(LeaderShape {
            from,
            to,
            z: desc.z - 0.1,
            color: leader_col,
            width_px: view.phys_px(style.leader_width_px),
            dot_radius_px: view.phys_px(style.leader_dot_radius_px),
            dash: DashStyle::SOLID,
        }));
    }

    shapes.push(Shape::Rect(RectShape {
        rect: pill_rect,
        z: desc.z,
        fill: Some(fill),
        stroke: Some(border),
        stroke_px: view.phys_px(style.map_label_border_px),
        dash: DashStyle::SOLID,
        corner_radius_px: view.phys_px(style.map_label_corner_radius_px),
    }));

    if !desc.text.is_empty() && pill_rect.width() > 0.0 && pill_rect.height() > 0.0 {
        let text_w_world = measure_label_width(
            fonts.metrics,
            fonts.atlas,
            fonts.family,
            desc.text,
            text_h_world,
        );
        let text_x =
            (pill_rect.center().x - 0.5 * text_w_world).max(pill_rect.min.x + pad_x_world * 0.5);
        let text_top_y = pill_rect.center().y + 0.5 * text_h_world;
        labels.push(
            Label::new(
                desc.text.to_string(),
                Vec2::new(text_x, text_top_y),
                LabelHeight::ScreenPx(font_phys_px),
                text_col,
                desc.z,
            )
            .with_clip(pill_rect),
        );
    }

    pill_rect
}

/// Standalone map-label builder returning `(WorldRect, Vec<Shape>, Vec<Label<'static>>)`.
pub fn build_map_label(
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &MapLabelDesc<'_>,
) -> (WorldRect, Vec<Shape>, Vec<Label<'static>>) {
    let mut shapes = Vec::with_capacity(2);
    let mut labels = Vec::with_capacity(1);
    let rect = push_map_label(&mut shapes, &mut labels, fonts, style, view, desc);
    (rect, shapes, labels)
}

// ── 7. Notice Banner (`wall/main.jsx` + `wall/StatusWindow.jsx`) ──────────────

/// Visual severity of a top-of-wall notice banner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    /// Warm amber notice (fullscreen adoption reminder, restarting daemon, etc.).
    #[default]
    Amber,
    /// Red error/refusal notice (refused attach, refused clipboard paste, lost daemon).
    Red,
}

/// Plain-data description of a top-of-viewport notice banner.
#[derive(Clone, Debug, PartialEq)]
pub struct NoticeDesc<'a> {
    pub text: &'a str,
    pub kind: NoticeKind,
    /// Camera used to unproject the screen-pinned banner into world space.
    pub camera: WallCamera,
    /// Physical pixel viewport `(width, height)`.
    pub viewport: (u32, u32),
    /// World depth of the banner (above canvas terminals and fullscreen veil).
    pub z: f32,
    /// Optional logical CSS-pixel left inset from docked left panels (`0.0` if none).
    pub left_inset_css_px: f32,
    /// Optional logical CSS-pixel right inset from docked right panels (`0.0` if none).
    pub right_inset_css_px: f32,
    /// Optional override for logical CSS-pixel top offset (`None` = `style.notice_top_px`).
    pub top_css_px: Option<f32>,
}

impl<'a> NoticeDesc<'a> {
    pub fn new(
        text: &'a str,
        kind: NoticeKind,
        camera: WallCamera,
        viewport: (u32, u32),
        z: f32,
    ) -> Self {
        Self {
            text,
            kind,
            camera,
            viewport,
            z,
            left_inset_css_px: 0.0,
            right_inset_css_px: 0.0,
            top_css_px: None,
        }
    }
}

/// Append a screen-pinned top notice banner (`Shape::Rect` + clipped [`Label`])
/// into `shapes` and `labels`, returning the banner's [`WorldRect`].
pub fn push_notice(
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &NoticeDesc<'_>,
) -> Option<WorldRect> {
    if desc.text.is_empty() || desc.viewport.0 == 0 || desc.viewport.1 == 0 {
        return None;
    }
    let vp_w = desc.viewport.0 as f32;
    let edge_px = view.phys_px(style.notice_edge_margin_px);
    let avail_left = (view.phys_px(desc.left_inset_css_px) + edge_px).min(vp_w);
    let avail_right = (vp_w - view.phys_px(desc.right_inset_css_px) - edge_px).max(avail_left + 32.0);
    let avail_w = (avail_right - avail_left).max(32.0);
    let max_pill_w = (avail_w * style.notice_max_width_fraction).max(32.0);

    let natural_font_px = view.phys_px(style.notice_font_px);
    let pad_x_px = view.phys_px(style.notice_pad_x_px);
    let pad_y_px = view.phys_px(style.notice_pad_y_px);

    // Measure text in physical pixels (`height_world = natural_font_px` gives width in physical px!).
    let natural_text_w_px = measure_label_width(
        fonts.metrics,
        fonts.atlas,
        fonts.family,
        desc.text,
        natural_font_px,
    );
    let max_text_w_px = (max_pill_w - 2.0 * pad_x_px).max(16.0);
    // Shrink font slightly if the notice exceeds 80% of viewport width so the whole message stays readable.
    let font_px = if natural_text_w_px > max_text_w_px && natural_text_w_px > 1e-4 {
        (natural_font_px * (max_text_w_px / natural_text_w_px)).max(view.phys_px(8.0))
    } else {
        natural_font_px
    };
    let text_w_px =
        measure_label_width(fonts.metrics, fonts.atlas, fonts.family, desc.text, font_px)
            .min(max_text_w_px);
    let pill_w_px = (text_w_px + 2.0 * pad_x_px).min(max_pill_w);
    let pill_h_px = natural_font_px + 2.0 * pad_y_px;

    let center_x_px = 0.5 * (avail_left + avail_right);
    let left_px = center_x_px - 0.5 * pill_w_px;
    let right_px = center_x_px + 0.5 * pill_w_px;
    let top_px = view.phys_px(desc.top_css_px.unwrap_or(style.notice_top_px));
    let bottom_px = top_px + pill_h_px;

    let pill_world = phys_screen_rect_to_world(
        &desc.camera,
        desc.viewport,
        left_px,
        top_px,
        right_px,
        bottom_px,
    );

    let (fill, border, text_col) = match desc.kind {
        NoticeKind::Amber => (
            style.notice_amber_fill,
            style.notice_amber_border,
            style.notice_amber_text,
        ),
        NoticeKind::Red => (
            style.notice_red_fill,
            style.notice_red_border,
            style.notice_red_text,
        ),
    };

    shapes.push(Shape::Rect(RectShape {
        rect: pill_world,
        z: desc.z,
        fill: Some(fill),
        stroke: Some(border),
        stroke_px: view.phys_px(style.notice_border_px),
        dash: DashStyle::SOLID,
        corner_radius_px: view.phys_px(style.notice_corner_radius_px),
    }));

    let text_h_world = font_px / desc.camera.zoom.max(1e-4);
    let pad_x_world = pad_x_px / desc.camera.zoom.max(1e-4);
    labels.push(
        Label::new(
            desc.text.to_string(),
            Vec2::new(
                pill_world.min.x + pad_x_world,
                pill_world.center().y + 0.5 * text_h_world,
            ),
            LabelHeight::ScreenPx(font_px),
            text_col,
            desc.z,
        )
        .with_clip(pill_world),
    );

    Some(pill_world)
}

/// Standalone notice builder returning `(Option<WorldRect>, Vec<Shape>, Vec<Label<'static>>)`.
pub fn build_notice(
    fonts: &mut ChromeFonts<'_>,
    style: &ChromeStyle,
    view: ChromeView,
    desc: &NoticeDesc<'_>,
) -> (Option<WorldRect>, Vec<Shape>, Vec<Label<'static>>) {
    let mut shapes = Vec::with_capacity(1);
    let mut labels = Vec::with_capacity(1);
    let rect = push_notice(&mut shapes, &mut labels, fonts, style, view, desc);
    (rect, shapes, labels)
}

// ── 8. Fullscreen Veil (`wall/WallFullscreen.js` `backdropClipPath`) ──────────

/// Plain-data description of a fullscreen dimming veil over `bounds` with
/// rectangular `holes` left completely uncovered.
#[derive(Clone, Debug, PartialEq)]
pub struct FullscreenVeilDesc<'a> {
    /// Outer world rectangle covered by the veil (typically `camera.visible_world_rect(viewport)`).
    pub bounds: WorldRect,
    /// Uncovered cutout rectangles in world space (single fullscreen pane, split union, or group member windows).
    pub holes: &'a [WorldRect],
    /// World depth of the veil (above canvas windows, below elevated fullscreen windows).
    pub z: f32,
}

/// Decompose `bounds \ union(holes)` into mutually disjoint, edge-sharing
/// [`WorldRect`]s so translucent alpha (`0.90`) blends strictly once everywhere
/// outside `holes` with zero seam overlap, even when `holes` overlap each other.
pub fn fullscreen_veil_rects(bounds: WorldRect, holes: &[WorldRect]) -> Vec<WorldRect> {
    const EPS: f32 = 1e-5;
    if bounds.width() <= EPS || bounds.height() <= EPS {
        return Vec::new();
    }

    let mut clamped: Vec<WorldRect> = Vec::with_capacity(holes.len());
    let mut ys: Vec<f32> = Vec::with_capacity(2 + holes.len() * 2);
    ys.push(bounds.min.y);
    ys.push(bounds.max.y);

    for h in holes {
        let min_x = h.min.x.clamp(bounds.min.x, bounds.max.x);
        let max_x = h.max.x.clamp(bounds.min.x, bounds.max.x);
        let min_y = h.min.y.clamp(bounds.min.y, bounds.max.y);
        let max_y = h.max.y.clamp(bounds.min.y, bounds.max.y);
        if max_x - min_x > EPS && max_y - min_y > EPS {
            clamped.push(WorldRect::new(Vec2::new(min_x, min_y), Vec2::new(max_x, max_y)));
            ys.push(min_y);
            ys.push(max_y);
        }
    }

    if clamped.is_empty() {
        return vec![bounds];
    }

    ys.sort_by(f32::total_cmp);
    ys.dedup_by(|a, b| (*a - *b).abs() <= EPS);

    let mut strips: Vec<WorldRect> = Vec::new();
    let mut x_spans: Vec<(f32, f32)> = Vec::new();

    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        if y1 - y0 <= EPS {
            continue;
        }
        let y_mid = 0.5 * (y0 + y1);
        x_spans.clear();
        for h in &clamped {
            if h.min.y <= y_mid && h.max.y >= y_mid {
                x_spans.push((h.min.x, h.max.x));
            }
        }
        x_spans.sort_by(|a, b| a.0.total_cmp(&b.0));

        // Merge overlapping or touching X hole intervals in this horizontal band.
        let mut merged: Vec<(f32, f32)> = Vec::with_capacity(x_spans.len());
        for (l, r) in x_spans.iter().copied() {
            if let Some(last) = merged.last_mut() {
                if l <= last.1 + EPS {
                    last.1 = last.1.max(r);
                    continue;
                }
            }
            merged.push((l, r));
        }

        // Emit complement intervals in `[bounds.min.x, bounds.max.x]`, coalescing
        // vertically with any strip from the previous band that has identical X bounds.
        let mut cursor_x = bounds.min.x;
        for (hl, hr) in merged {
            if hl - cursor_x > EPS {
                push_or_coalesce_strip(&mut strips, cursor_x, hl, y0, y1, EPS);
            }
            cursor_x = cursor_x.max(hr);
        }
        if bounds.max.x - cursor_x > EPS {
            push_or_coalesce_strip(&mut strips, cursor_x, bounds.max.x, y0, y1, EPS);
        }
    }

    strips
}

fn push_or_coalesce_strip(
    strips: &mut Vec<WorldRect>,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    eps: f32,
) {
    if let Some(prev) = strips.iter_mut().find(|s| {
        (s.max.y - y0).abs() <= eps && (s.min.x - x0).abs() <= eps && (s.max.x - x1).abs() <= eps
    }) {
        prev.max.y = y1;
    } else {
        strips.push(WorldRect::new(Vec2::new(x0, y0), Vec2::new(x1, y1)));
    }
}

/// Append fullscreen veil rectangles covering `desc.bounds` except `desc.holes`
/// into `shapes`, returning the number of `RectShape`s emitted.
pub fn push_fullscreen_veil(
    shapes: &mut Vec<Shape>,
    style: &ChromeStyle,
    desc: &FullscreenVeilDesc<'_>,
) -> usize {
    let rects = fullscreen_veil_rects(desc.bounds, desc.holes);
    let count = rects.len();
    for rect in rects {
        shapes.push(Shape::Rect(RectShape {
            rect,
            z: desc.z,
            fill: Some(style.veil_fill),
            stroke: None,
            stroke_px: 0.0,
            dash: DashStyle::SOLID,
            corner_radius_px: 0.0,
        }));
    }
    count
}

/// Standalone fullscreen-veil builder returning `Vec<Shape>`.
pub fn build_fullscreen_veil(style: &ChromeStyle, desc: &FullscreenVeilDesc<'_>) -> Vec<Shape> {
    let mut shapes = Vec::new();
    push_fullscreen_veil(&mut shapes, style, desc);
    shapes
}

// ── Showcase Pattern (`--wall-chrome-pattern --screenshot OUT.png`) ───────────

fn make_card_screen(cols: u16, rows: u16, title: &str, bg: u32, lines: &[(&str, u32)]) -> Screen {
    let mut s = Screen::new(cols, rows);
    s.default_fg = rgb(0xd4, 0xd9, 0xe2);
    s.default_bg = bg;
    s.title = title.to_string();
    for (idx, &(text, fg)) in lines.iter().enumerate() {
        let r = (idx as u16) * 2 + 1;
        if r >= rows {
            break;
        }
        let st = if idx == 0 {
            RunStyle::fg(fg).with_attrs(attrs::BOLD)
        } else {
            RunStyle::fg(fg)
        };
        write_run(&mut s, r, 1, text, st);
    }
    s.cursor = Cursor {
        col: 1,
        row: (rows.saturating_sub(2)).max(1),
        visible: true,
        shape: CursorShape::Block,
        blink: false,
    };
    s
}

/// Build the terminals for `--wall-chrome-pattern`, exercising every chrome
/// element and state across a clean two-zone layout:
///
/// - **Left/Centre Canvas Zone**:
///   - `id = 1`: Focused terminal inside Focused Group A (`core-checks`).
///   - `id = 2`: Agent-attached (`waiting`) + Selected terminal inside Focused Group A.
///   - `id = 3`: Hovered terminal with long shrunk title inside Idle Group B (`bg-workers`).
///   - `id = 4`: Agent-attached (`processing`) terminal inside Idle Group B.
///   - `id = 5`: Loose Idle + Selected terminal (inside Hovered Group C `staging`).
///   - `id = 6`: Small `0.35x` unfocused terminal carrying a detached **Idle Map Label** + leader.
///   - `id = 7`: Small `0.35x` focused-style terminal carrying a detached **Focused Map Label** + leader.
///   - `id = 8`: Small `0.35x` terminal carrying an **Adjacent (touching, no-leader) Map Label**.
/// - **Right Fullscreen Veil Zone**:
///   - `id = 9`: Dimmed background terminal (`z = 1.0`) underneath the `90%` fullscreen veil (`z = 15.0`), partially visible outside and crisply un-dimmed inside a secondary hole!
///   - `id = 10`: Elevated fullscreen terminal (`z = 25.0`) sitting inside its exact cutout hole in the veil.
pub fn build_chrome_pattern_terminals(metrics: CellMetrics) -> Vec<TestPatternTerminal> {
    let cw = metrics.cell_width;
    let ch = metrics.cell_height;

    let mint = rgb(0x8e, 0xf0, 0xb8);
    let amber = rgb(0xf2, 0xb4, 0x5a);
    let blue = rgb(0x7f, 0xb0, 0xff);
    let slate = rgb(0xc4, 0xcc, 0xd8);
    let coral = rgb(0xf0, 0x80, 0x70);

    let s1 = make_card_screen(
        34,
        9,
        "cargo test -p wall-core",
        rgb(0x13, 0x18, 0x22),
        &[
            ("T1: FOCUSED BAR", mint),
            ("in focused group A", slate),
            ("158 passed; 0 failed", mint),
        ],
    );
    let s2 = make_card_screen(
        34,
        9,
        "wall-layout",
        rgb(0x16, 0x19, 0x24),
        &[
            ("T2: AGENT WAITING + SELECTED", amber),
            ("bar: wall-layout · waiting", amber),
            ("3px selection outline", blue),
        ],
    );
    let s3 = make_card_screen(
        34,
        9,
        "hovered-worker — long-shrunk-title-in-bar-reserving-buttons",
        rgb(0x17, 0x1c, 0x28),
        &[
            ("T3: HOVERED BAR (SHRUNK)", rgb(0xe6, 0xe9, 0xef)),
            ("reserves 3.15x bar button room", slate),
            ("in idle group B", slate),
        ],
    );
    let s4 = make_card_screen(
        34,
        9,
        "wall-lead",
        rgb(0x14, 0x18, 0x24),
        &[
            ("T4: AGENT PROCESSING", blue),
            ("bar: wall-lead · processing", blue),
            ("idle bar + blue status word", slate),
        ],
    );
    let s5 = make_card_screen(
        34,
        8,
        "zsh — ~/glyph3d-rs [8x34]",
        rgb(0x15, 0x19, 0x23),
        &[
            ("T5: IDLE + SELECTED", slate),
            ("in hovered group C", blue),
            ("drop previews to the right ->", coral),
        ],
    );

    let s6 = make_card_screen(
        26,
        8,
        "tiny-idle-watcher",
        rgb(0x18, 0x1d, 0x27),
        &[
            ("T6 (0.35x)", slate),
            ("idle map label", slate),
            ("+ leader + dot", amber),
        ],
    );
    let s7 = make_card_screen(
        26,
        8,
        "tiny-focused-probe",
        rgb(0x12, 0x22, 0x1a),
        &[
            ("T7 (0.35x)", mint),
            ("focused map label", mint),
            ("+ green leader", mint),
        ],
    );
    let s8 = make_card_screen(
        26,
        8,
        "touching-hovered",
        rgb(0x1a, 0x1e, 0x2a),
        &[
            ("T8 (0.35x)", blue),
            ("touching pill", slate),
            ("(no leader line)", slate),
        ],
    );

    let s9 = make_card_screen(
        40,
        18,
        "background-under-veil",
        rgb(0x1d, 0x24, 0x36),
        &[
            ("T9: CANVAS TERMINAL UNDER VEIL", coral),
            ("90% dimmed outside holes;", slate),
            ("crisp inside cutout window below:", amber),
            ("CUTOUT-WINDOW-CRISP-CHECK-LINE-1", mint),
            ("CUTOUT-WINDOW-CRISP-CHECK-LINE-2", blue),
            ("DIMMED-OUTSIDE-THE-TWO-HOLES-3", coral),
            ("DIMMED-OUTSIDE-THE-TWO-HOLES-4", amber),
        ],
    );
    let s10 = make_card_screen(
        34,
        11,
        "fullscreen-pane",
        rgb(0x14, 0x22, 0x1c),
        &[
            ("T10: ELEVATED FULLSCREEN HOLE", mint),
            ("z = 25 above z = 15 veil", slate),
            ("veil cut cleanly around this pane", mint),
            ("no dim over cells or border!", blue),
        ],
    );

    vec![
        // Top-left: Focused Group A (T1 + T2)
        TestPatternTerminal {
            id: 1,
            title: s1.title.clone(),
            screen: s1,
            position: Vec2::new(0.0, 18.0 * ch),
            z: 2.0,
            scale: 1.0,
            focused: true,
            border_color: None,
            selection: None,
        },
        TestPatternTerminal {
            id: 2,
            title: s2.title.clone(),
            screen: s2,
            position: Vec2::new(38.0 * cw, 18.0 * ch),
            z: 2.2,
            scale: 1.0,
            focused: false,
            border_color: None,
            selection: None,
        },
        // Middle-left: Idle Group B (T3 + T4)
        TestPatternTerminal {
            id: 3,
            title: s3.title.clone(),
            screen: s3,
            position: Vec2::new(0.0, 4.0 * ch),
            z: 2.0,
            scale: 1.0,
            focused: false,
            border_color: Some(rgb(0x4e, 0x59, 0x6f)),
            selection: None,
        },
        TestPatternTerminal {
            id: 4,
            title: s4.title.clone(),
            screen: s4,
            position: Vec2::new(38.0 * cw, 4.0 * ch),
            z: 2.1,
            scale: 1.0,
            focused: false,
            border_color: None,
            selection: None,
        },
        // Bottom-left: Hovered Group C (T5)
        TestPatternTerminal {
            id: 5,
            title: s5.title.clone(),
            screen: s5,
            position: Vec2::new(0.0, -9.5 * ch),
            z: 2.0,
            scale: 1.0,
            focused: false,
            border_color: None,
            selection: None,
        },
        // Bottom-centre: 3 small 0.35x terminals with Map Labels (T6, T7, T8)
        TestPatternTerminal {
            id: 6,
            title: s6.title.clone(),
            screen: s6,
            position: Vec2::new(40.0 * cw, -14.5 * ch),
            z: 2.0,
            scale: 0.35,
            focused: false,
            border_color: None,
            selection: None,
        },
        TestPatternTerminal {
            id: 7,
            title: s7.title.clone(),
            screen: s7,
            position: Vec2::new(54.0 * cw, -14.5 * ch),
            z: 2.2,
            scale: 0.35,
            focused: false,
            border_color: Some(mint),
            selection: None,
        },
        TestPatternTerminal {
            id: 8,
            title: s8.title.clone(),
            screen: s8,
            position: Vec2::new(68.0 * cw, -14.5 * ch),
            z: 2.0,
            scale: 0.35,
            focused: false,
            border_color: None,
            selection: None,
        },
        // Right zone: Fullscreen Veil demonstration (T9 behind veil at z=1, T10 in hole at z=25)
        TestPatternTerminal {
            id: 9,
            title: s9.title.clone(),
            screen: s9,
            position: Vec2::new(88.0 * cw, 16.0 * ch),
            z: 1.0,
            scale: 1.0,
            focused: false,
            border_color: None,
            selection: None,
        },
        TestPatternTerminal {
            id: 10,
            title: s10.title.clone(),
            screen: s10,
            position: Vec2::new(92.0 * cw, 3.5 * ch),
            z: 25.0,
            scale: 1.0,
            focused: false,
            border_color: Some(mint),
            selection: None,
        },
    ]
}

/// Build every piece of chrome for `--wall-chrome-pattern` using the functions
/// in this module (`push_title_bar`, `push_group_frame`, `push_selection_outline`,
/// `push_lasso`, `push_drop_preview`, `push_map_label` driven by
/// `wall_core::labels::place_labels`, `push_notice`, and `push_fullscreen_veil`).
pub fn build_chrome_pattern_scene(
    fonts: &mut ChromeFonts<'_>,
    panel_padding: f32,
    camera: WallCamera,
    viewport: (u32, u32),
    scale_factor: f32,
    terminals: &[TestPatternTerminal],
) -> TestPatternChrome {
    let style = ChromeStyle::default();
    let view = ChromeView::new(camera.zoom, scale_factor);
    let paper = PaperStyle::default();

    let mut shapes = Vec::new();
    let mut labels = Vec::new();
    let mut bounds: Option<WorldRect> = None;
    let mut include = |r: WorldRect| {
        bounds = Some(match bounds {
            Some(acc) => acc.union(r),
            None => r,
        });
    };

    let mut group_a: Option<WorldRect> = None;
    let mut group_b: Option<WorldRect> = None;
    let mut group_c: Option<WorldRect> = None;
    let mut t5_win: Option<WorldRect> = None;
    let mut t9_win: Option<WorldRect> = None;
    let mut t10_panel: Option<WorldRect> = None;
    let mut obstacle_world_rects: Vec<WorldRect> = Vec::new();

    struct MapCandidate {
        id: u64,
        text: String,
        window_rect: WorldRect,
        focused: bool,
        hovered: bool,
    }
    let mut map_candidates: Vec<MapCandidate> = Vec::new();

    for t in terminals {
        let panel = fonts.metrics.terminal_panel_rect(
            t.position,
            t.screen.cols,
            t.screen.rows,
            panel_padding,
            t.scale,
        );
        include(panel);

        // T10 is a solo fullscreen pane (no title bar in solo fullscreen).
        if t.id == 10 {
            t10_panel = Some(panel);
            obstacle_world_rects.push(panel);
            continue;
        }

        let mut bar_desc = TitleBarDesc::new(panel, t.z, &t.title);
        match t.id {
            1 => {
                bar_desc.focused = true;
            }
            2 => {
                bar_desc.agent_status = Some(AgentStatus::Waiting);
                bar_desc.selected = true;
            }
            3 => {
                bar_desc.hovered = true;
            }
            4 => {
                bar_desc.agent_status = Some(AgentStatus::Processing);
            }
            5 => {
                bar_desc.selected = true;
            }
            7 => {
                bar_desc.focused = true;
            }
            8 => {
                bar_desc.hovered = true;
            }
            _ => {}
        }

        let out = push_title_bar(&mut shapes, &mut labels, fonts, &style, view, &bar_desc);
        include(out.window_rect);
        obstacle_world_rects.push(out.window_rect);

        // Any canvas terminal (`id <= 8`) whose bar text fell below `min_text_px`
        // (T6..T8 at 1x; all of T1..T8 at 0.14x!) hands its name to `place_labels`.
        if t.id <= 8 && !out.name_fits {
            let text = match bar_desc.agent_status {
                Some(st) => format_agent_title(&t.title, st),
                None => t.title.clone(),
            };
            map_candidates.push(MapCandidate {
                id: t.id,
                text,
                window_rect: out.window_rect,
                focused: bar_desc.focused,
                hovered: bar_desc.hovered,
            });
        }

        if matches!(t.id, 2 | 5) {
            let sel = push_selection_outline(
                &mut shapes,
                &style,
                view,
                SelectionOutlineDesc {
                    window_rect: out.window_rect,
                    z: 8.0,
                },
            );
            include(sel);
        }

        match t.id {
            1 | 2 => {
                group_a = Some(match group_a {
                    Some(u) => u.union(out.window_rect),
                    None => out.window_rect,
                });
            }
            3 | 4 => {
                group_b = Some(match group_b {
                    Some(u) => u.union(out.window_rect),
                    None => out.window_rect,
                });
            }
            5 => {
                group_c = Some(out.window_rect);
                t5_win = Some(out.window_rect);
            }
            9 => t9_win = Some(out.window_rect),
            _ => {}
        }
    }

    // Three group frames: Focused (A), Idle (B), and Hovered (C).
    if let Some(ga) = group_a {
        let g = push_group_frame(
            &mut shapes,
            &mut labels,
            fonts,
            &style,
            view,
            &GroupFrameDesc {
                backdrop_rect: ga.expand_xy(1.1, 0.9),
                z: 0.2,
                name: "group: core-checks (focused)",
                focused: true,
                hovered: false,
            },
        );
        include(g.full_rect);
        obstacle_world_rects.push(g.header_rect);
    }
    if let Some(gb) = group_b {
        let g = push_group_frame(
            &mut shapes,
            &mut labels,
            fonts,
            &style,
            view,
            &GroupFrameDesc {
                backdrop_rect: gb.expand_xy(1.1, 0.9),
                z: 0.2,
                name: "group: background-jobs (idle)",
                focused: false,
                hovered: false,
            },
        );
        include(g.full_rect);
        obstacle_world_rects.push(g.header_rect);
    }
    if let Some(gc) = group_c {
        let g = push_group_frame(
            &mut shapes,
            &mut labels,
            fonts,
            &style,
            view,
            &GroupFrameDesc {
                backdrop_rect: gc.expand_xy(1.1, 0.9),
                z: 0.2,
                name: "group: staging (hovered)",
                focused: false,
                hovered: true,
            },
        );
        include(g.full_rect);
        obstacle_world_rects.push(g.header_rect);
    }

    // Accepted + Refused drop previews + Lasso below/beside Group C.
    if let Some(t5) = t5_win {
        let cw = fonts.metrics.cell_width;
        let ch = fonts.metrics.cell_height;
        let ok_rect = WorldRect::new(
            Vec2::new(t5.max.x + 3.0 * cw, t5.min.y + 4.5 * ch),
            Vec2::new(t5.max.x + 18.0 * cw, t5.max.y),
        );
        push_drop_preview(
            &mut shapes,
            &style,
            view,
            DropPreviewDesc {
                rect: ok_rect,
                z: 9.0,
                accepted: true,
            },
        );
        include(ok_rect);
        obstacle_world_rects.push(ok_rect);

        let refused_rect = WorldRect::new(
            Vec2::new(t5.max.x + 20.0 * cw, t5.min.y + 4.5 * ch),
            Vec2::new(t5.max.x + 35.0 * cw, t5.max.y),
        );
        push_drop_preview(
            &mut shapes,
            &style,
            view,
            DropPreviewDesc {
                rect: refused_rect,
                z: 9.0,
                accepted: false,
            },
        );
        include(refused_rect);
        obstacle_world_rects.push(refused_rect);

        let lasso_pts = vec![
            Vec2::new(t5.min.x - 1.5 * cw, t5.min.y - 1.5 * ch),
            Vec2::new(t5.min.x + 14.0 * cw, t5.min.y - 0.6 * ch),
            Vec2::new(t5.min.x + 22.0 * cw, t5.min.y - 3.6 * ch),
            Vec2::new(t5.min.x + 16.0 * cw, t5.min.y - 6.0 * ch),
            Vec2::new(t5.min.x + 2.0 * cw, t5.min.y - 5.2 * ch),
        ];
        for &pt in &lasso_pts {
            include(WorldRect::new(pt, pt));
        }
        push_lasso(
            &mut shapes,
            &style,
            view,
            LassoDesc {
                points: &lasso_pts,
                z: 9.5,
            },
        );
    }

    // Fullscreen veil over the right-hand zone (`z = 15.0`) with TWO holes:
    // 1) around elevated T10 (`z = 25.0`), and
    // 2) a crisp inspection window over rows 3..5 of T9 (`z = 1.0`) so multi-hole
    //    strip decomposition and 90% dimming are both directly visible!
    if let (Some(t9), Some(t10)) = (t9_win, t10_panel) {
        let veil_bounds = t9.union(t10).expand_xy(2.2, 2.0);
        let inspect_hole = WorldRect::new(
            Vec2::new(t9.min.x + 1.2, t9.max.y - 10.2),
            Vec2::new(t9.max.x - 1.2, t9.max.y - 6.8),
        );
        push_fullscreen_veil(
            &mut shapes,
            &style,
            &FullscreenVeilDesc {
                bounds: veil_bounds,
                holes: &[t10, inspect_hole],
                z: 15.0,
            },
        );
        // Subtle hairline around veil zone so its extent is obvious on dark paper.
        shapes.push(Shape::Rect(RectShape {
            rect: veil_bounds,
            z: 15.1,
            fill: None,
            stroke: Some(srgb_hex(0x2a303c, 0.7)),
            stroke_px: view.phys_px(1.0),
            dash: DashStyle::new(view.phys_px(4.0), view.phys_px(4.0)),
            corner_radius_px: 0.0,
        }));
        include(veil_bounds);
        obstacle_world_rects.push(veil_bounds);
    }

    // Place map labels via `wall_core::labels::place_labels` (`MapLabels.js` priority:
    // focused first, hovered second, then id). Seed `prev` so at 1x T6 & T7 exercise
    // detached `Below2` leader lines + anchor dots while T8 exercises an adjacent
    // `Above` touching pill without a leader!
    if !map_candidates.is_empty() && viewport.0 > 0 && viewport.1 > 0 {
        map_candidates.sort_by_key(|c| (!c.focused, !c.hovered, c.id));
        let inv_s = 1.0 / f64::from(view.safe_scale());
        let label_view = LabelView::new(
            f64::from(viewport.0) * inv_s,
            f64::from(viewport.1) * inv_s,
        );
        let obstacles: Vec<ScreenBox> = obstacle_world_rects
            .iter()
            .map(|&r| world_rect_to_screen_box(r, &camera, viewport, view.safe_scale()))
            .collect();
        let mut prev: HashMap<String, Candidate> = HashMap::new();
        prev.insert("6".to_string(), Candidate::Below2);
        prev.insert("7".to_string(), Candidate::Below2);
        prev.insert("8".to_string(), Candidate::Above);

        let items: Vec<LabelItem> = map_candidates
            .iter()
            .map(|c| {
                let (w, h) = measure_map_label_css(fonts, &style, &c.text);
                LabelItem {
                    id: c.id.to_string(),
                    target: world_rect_to_screen_box(
                        c.window_rect,
                        &camera,
                        viewport,
                        view.safe_scale(),
                    ),
                    w,
                    h,
                }
            })
            .collect();
        let placements = place_labels(&items, label_view, &prev, Some(&obstacles));
        for (idx, c) in map_candidates.iter().enumerate() {
            let Some(Some(placement)) = placements.get(&c.id.to_string()).copied() else {
                continue;
            };
            let pill_world = push_map_label(
                &mut shapes,
                &mut labels,
                fonts,
                &style,
                view,
                &MapLabelDesc {
                    text: &c.text,
                    focused: c.focused,
                    hovered: c.hovered,
                    z: 30.0 + (map_candidates.len() - idx) as f32 * 0.05,
                    geometry: MapLabelGeometry::Placed {
                        placement,
                        camera,
                        viewport,
                    },
                },
            );
            // Include map label bounds during 1x camera framing (`zoom >= 10`) so
            // T6 & T7's bottom leader pills stay comfortably inside the 1x frame.
            if camera.zoom >= 10.0 {
                include(pill_world);
            }
        }
    }

    // Screen-pinned top Notices (both Amber and Red!) — not added to `bounds`
    // because notices follow the viewport top at any camera zoom!
    let mut amber_notice = NoticeDesc::new(
        "fullscreen: core-checks — Cmd+Enter shows the whole wall again",
        NoticeKind::Amber,
        camera,
        viewport,
        40.0,
    );
    amber_notice.top_css_px = Some(14.0);
    push_notice(
        &mut shapes,
        &mut labels,
        fonts,
        &style,
        view,
        &amber_notice,
    );

    let mut red_notice = NoticeDesc::new(
        "refused attach: agent has no live session · clipboard paste refused",
        NoticeKind::Red,
        camera,
        viewport,
        40.0,
    );
    red_notice.top_css_px = Some(44.0);
    push_notice(&mut shapes, &mut labels, fonts, &style, view, &red_notice);

    // Leave extra headroom at the top of `chrome_bounds` so the two top notices sit
    // cleanly above the canvas groups at 1x framing.
    let chrome_bounds = bounds
        .map(|b| {
            WorldRect::new(
                Vec2::new(b.min.x, b.min.y - 1.0),
                Vec2::new(b.max.x, b.max.y + 5.0),
            )
        })
        .unwrap_or_else(|| WorldRect::new(Vec2::ZERO, Vec2::ONE));

    TestPatternChrome {
        paper,
        shapes,
        labels,
        chrome_bounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_test_atlas() -> (CellMetrics, FontAtlas, FamilyId) {
        let specs = glyph_font::default_font_chain(&crate::wall::glyphs::default_fonts_dir());
        let atlas = FontAtlas::load(&specs).expect("load default font chain");
        let family = atlas
            .family_id(glyph_font::MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());
        let fm = atlas.family_metrics(family);
        let raw_w = (f64::from(fm.advance_fu) * f64::from(crate::text::CELL_HEIGHT_WORLD)
            / fm.extents.em_height()) as f32;
        let metrics = CellMetrics::snapped(raw_w, crate::text::CELL_HEIGHT_WORLD);
        (metrics, atlas, family)
    }

    #[test]
    fn title_bar_scales_with_retina_shrinks_text_and_splits_agent_status() {
        let (metrics, mut atlas, family) = load_test_atlas();
        let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
        let style = ChromeStyle::default();
        let body = WorldRect::new(Vec2::new(0.0, 0.0), Vec2::new(20.0, 10.0));

        // 1x vs 2x Retina at zoom=44: 1x bar is 22/44 = 0.5 world units; 2x Retina is 44/44 = 1.0 world unit!
        let (out_1x, _, _) = build_title_bar(
            &mut fonts,
            &style,
            ChromeView::new(44.0, 1.0),
            &TitleBarDesc::new(body, 2.0, "zsh"),
        );
        let (out_2x, _, _) = build_title_bar(
            &mut fonts,
            &style,
            ChromeView::new(44.0, 2.0),
            &TitleBarDesc::new(body, 2.0, "zsh"),
        );
        assert!((out_1x.strip_rect.height() - 0.5).abs() < 1e-5);
        assert!((out_2x.strip_rect.height() - 1.0).abs() < 1e-5);

        // Agent-attached `<name> · <status>` emits 3 aligned label spans (name, ` · `, status word).
        let mut agent_desc = TitleBarDesc::new(body, 2.0, "wall-layout");
        agent_desc.agent_status = Some(AgentStatus::Waiting);
        let (agent_out, shapes, labels) = build_title_bar(
            &mut fonts,
            &style,
            ChromeView::new(24.0, 1.0),
            &agent_desc,
        );
        assert!(agent_out.name_fits);
        assert_eq!(shapes.len(), 1);
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0].text, "wall-layout");
        assert_eq!(labels[1].text, " · ");
        assert_eq!(labels[2].text, "waiting");
        assert_eq!(labels[2].color, srgb_hex(0xf2b45a, 1.0));

        // Zero split bar_room suppresses the bar cleanly.
        let mut hidden_bar = TitleBarDesc::new(body, 2.0, "pane");
        hidden_bar.max_world_height = Some(0.0);
        let (zero_out, z_shapes, z_labels) = build_title_bar(
            &mut fonts,
            &style,
            ChromeView::new(24.0, 1.0),
            &hidden_bar,
        );
        assert_eq!(zero_out.strip_rect.height(), 0.0);
        assert!(z_shapes.is_empty() && z_labels.is_empty());
    }

    #[test]
    fn group_selection_lasso_drop_map_label_and_notice_emit_expected_primitives() {
        let (metrics, mut atlas, family) = load_test_atlas();
        let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
        let style = ChromeStyle::default();
        let view = ChromeView::new(20.0, 2.0); // Retina 2x

        // Group frame: backdrop + elevated header + 1 label.
        let (gf, gf_shapes, gf_labels) = build_group_frame(
            &mut fonts,
            &style,
            view,
            &GroupFrameDesc {
                backdrop_rect: WorldRect::new(Vec2::ZERO, Vec2::new(30.0, 15.0)),
                z: -4.0,
                name: "fleet",
                focused: true,
                hovered: false,
            },
        );
        assert!(gf.header_rect.min.y >= gf.backdrop_rect.max.y - 1e-5);
        assert_eq!(gf_shapes.len(), 2);
        assert_eq!(gf_labels.len(), 1);
        assert!(gf_shapes[1].z() > gf_shapes[0].z());

        // Selection outline: expands by `3 * 2 / 20 = 0.3` world units on Retina 2x.
        let sel = build_selection_outline(
            &style,
            view,
            SelectionOutlineDesc {
                window_rect: WorldRect::new(Vec2::new(1.0, 2.0), Vec2::new(11.0, 8.0)),
                z: 10.0,
            },
        );
        let Shape::Rect(sel_r) = &sel[0] else {
            panic!("expected rect");
        };
        assert!((sel_r.rect.min.x - 0.7).abs() < 1e-5);
        assert!((sel_r.corner_radius_px - 6.0).abs() < 1e-5);

        // Drop preview: accepted is solid blue, refused is dashed red.
        let ok = build_drop_preview(
            &style,
            view,
            DropPreviewDesc {
                rect: WorldRect::new(Vec2::ZERO, Vec2::new(5.0, 4.0)),
                z: 9.0,
                accepted: true,
            },
        );
        let no = build_drop_preview(
            &style,
            view,
            DropPreviewDesc {
                rect: WorldRect::new(Vec2::ZERO, Vec2::new(5.0, 4.0)),
                z: 9.0,
                accepted: false,
            },
        );
        let (Shape::Rect(ok_r), Shape::Rect(no_r)) = (&ok[0], &no[0]) else {
            panic!("expected drop rects");
        };
        assert!(!ok_r.dash.is_dashed());
        assert!(no_r.dash.is_dashed());
        assert_ne!(ok_r.stroke, no_r.stroke);

        // Map label from `wall_core::labels::Placement` with leader + round-trip ScreenBox helper.
        let cam = WallCamera::new(Vec2::ZERO, 20.0);
        let vp = (1280, 800);
        let orig_world = WorldRect::new(Vec2::new(-5.0, -3.0), Vec2::new(7.0, 4.0));
        let sbox = world_rect_to_screen_box(orig_world, &cam, vp, 2.0);
        let round_trip = screen_box_to_world_rect(sbox, &cam, vp, 2.0);
        assert!((round_trip.min - orig_world.min).length() < 1e-4);
        assert!((round_trip.max - orig_world.max).length() < 1e-4);

        let placement = Placement {
            cand: Candidate::Right,
            label_box: ScreenBox::from_top_left(300.0, 200.0, 96.0, 18.0),
            leader: Some(Leader {
                x1: 300.0,
                y1: 209.0,
                x2: 280.0,
                y2: 209.0,
            }),
        };
        let (_, ml_shapes, ml_labels) = build_map_label(
            &mut fonts,
            &style,
            ChromeView::new(cam.zoom, 1.0),
            &MapLabelDesc {
                text: "worker-2",
                focused: false,
                hovered: false,
                z: 12.0,
                geometry: MapLabelGeometry::Placed {
                    placement,
                    camera: cam,
                    viewport: vp,
                },
            },
        );
        assert_eq!(ml_shapes.len(), 2, "leader + pill");
        assert!(matches!(ml_shapes[0], Shape::Leader(_)));
        assert_eq!(ml_labels.len(), 1);

        // Notice: Amber vs Red.
        let (amber_rect, n_shapes, n_labels) = build_notice(
            &mut fonts,
            &style,
            ChromeView::new(cam.zoom, 1.0),
            &NoticeDesc::new("paste refused by clipboard", NoticeKind::Red, cam, vp, 50.0),
        );
        assert!(amber_rect.is_some());
        assert_eq!(n_shapes.len(), 1);
        assert_eq!(n_labels.len(), 1);
    }

    #[test]
    fn fullscreen_veil_covers_complement_of_disjoint_and_overlapping_holes_without_overlap() {
        let bounds = WorldRect::new(Vec2::new(0.0, 0.0), Vec2::new(100.0, 80.0));
        let total_area = bounds.width() * bounds.height();

        // Single interior hole (20..60, 10..50) -> area 40*40 = 1600; 4 coalesced strips!
        let h1 = WorldRect::new(Vec2::new(20.0, 10.0), Vec2::new(60.0, 50.0));
        let single = fullscreen_veil_rects(bounds, &[h1]);
        assert_eq!(single.len(), 4, "single interior hole should coalesce into 4 strips");
        let veil_area: f32 = single.iter().map(|r| r.width() * r.height()).sum();
        assert!(
            (veil_area - (total_area - 1600.0)).abs() < 1e-2,
            "veil area {veil_area} != {}",
            total_area - 1600.0
        );
        for (i, a) in single.iter().enumerate() {
            assert!(!a.contains(h1.center()), "veil strip must not cover hole");
            for b in &single[i + 1..] {
                let ix = (a.max.x.min(b.max.x) - a.min.x.max(b.min.x)).max(0.0);
                let iy = (a.max.y.min(b.max.y) - a.min.y.max(b.min.y)).max(0.0);
                assert!(ix * iy < 1e-4, "veil strips {a:?} and {b:?} overlap!");
            }
        }

        // Two overlapping holes: union area = (40*40) + (40*40) - (20*20) = 2800.
        let h2 = WorldRect::new(Vec2::new(40.0, 30.0), Vec2::new(80.0, 70.0));
        let overlap = fullscreen_veil_rects(bounds, &[h1, h2]);
        let overlap_area: f32 = overlap.iter().map(|r| r.width() * r.height()).sum();
        assert!(
            (overlap_area - (total_area - 2800.0)).abs() < 1e-2,
            "overlapping-holes veil area {overlap_area} != {}",
            total_area - 2800.0
        );
        for (i, a) in overlap.iter().enumerate() {
            for b in &overlap[i + 1..] {
                let ix = (a.max.x.min(b.max.x) - a.min.x.max(b.min.x)).max(0.0);
                let iy = (a.max.y.min(b.max.y) - a.min.y.max(b.min.y)).max(0.0);
                assert!(ix * iy < 1e-4, "veil strips {a:?} and {b:?} overlap!");
            }
        }
    }
}
