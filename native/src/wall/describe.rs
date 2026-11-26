//! Translates a `wall_core::wall::WallFrame` description into [`Shape`]s,
//! [`Label`]s, and [`PaperStyle`] by delegating every element to [`super::chrome`].

use std::collections::HashMap;

use glam::Vec2;
use wall_core::geom::{Point, Rect};
use wall_core::labels::{Candidate, Placement};
use wall_core::layout::BAR_ROOM;
use wall_core::status::StatusTone;
use wall_core::wall::{TerminalFrame, WallFrame as DescribedFrame};

use super::camera::WallCamera;
use super::chrome::{
    ChromeFonts, ChromeStyle, ChromeView, DropPreviewDesc, FullscreenVeilDesc, GroupFrameDesc,
    LassoDesc, MapLabelDesc, MapLabelGeometry, NoticeDesc, NoticeKind, ScrollbackIndicatorDesc,
    SelectionOutlineDesc, TitleBarDesc, push_drop_preview, push_fullscreen_veil, push_group_frame,
    push_lasso, push_map_label, push_notice, push_scrollback_indicator, push_selection_outline,
    push_title_bar,
};

/// Plain-data scrollback view position supplied per terminal by the host session layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalScrollback {
    pub offset: u64,
    pub max: u64,
    pub viewing: bool,
}
use super::labels::Label;
use super::metrics::WorldRect;
use super::paper::PaperStyle;
use super::shapes::Shape;
use super::srgb_hex;

/// Depth for multi-terminal selection outlines: above the canvas Z-stack
/// (`Z_MAX_STACK = 420`), below the fullscreen veil (`FULLSCREEN_BACKDROP_Z = 459`).
const Z_SELECTION: f32 = 430.0;

/// Depth for screen-space overlays (lasso, drop preview, map labels, notice):
/// above fullscreen terminals (`FULLSCREEN_Z = 460`) and inside `WallCamera::DEFAULT_Z_MAX` (`1000`).
const Z_OVERLAY: f32 = 900.0;

/// Parse `#rrggbb` or `#rgb` into normalized sRGB RGBA (`0.0..=1.0`), falling
/// back to loud magenta on invalid input.
pub fn css_color(s: &str, alpha: f32) -> [f32; 4] {
    let h = s.trim().trim_start_matches('#');
    let full = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => String::new(),
    };
    match u32::from_str_radix(&full, 16) {
        Ok(v) if full.len() == 6 => srgb_hex(v, alpha),
        _ => [1.0, 0.0, 1.0, alpha],
    }
}

#[inline]
fn to_world_rect(r: Rect) -> WorldRect {
    WorldRect::new(
        Vec2::new(r.min_x as f32, r.min_y as f32),
        Vec2::new(r.max_x as f32, r.max_y as f32),
    )
}

#[inline]
fn to_vec2(p: Point) -> Vec2 {
    Vec2::new(p.x as f32, p.y as f32)
}

/// Append all in-scene chrome (`groups`, `title_bars`, `selection_outlines`,
/// `lasso`, `drop_preview`, `fullscreen_backdrop`, `map_labels`, `status_notice`,
/// and `notice`) from `described` into `shapes` and `labels`, and return the
/// frame's [`PaperStyle`].
///
/// `camera.zoom` is in physical screen pixels per world unit (`described.camera.zoom * scale_factor`),
/// `viewport` is physical `(width, height)`, and world units follow `wall_core`
/// (`WORLD_CELL_HEIGHT = 2.4`, `BAR_ROOM = 9.6`).
pub fn describe_frame_chrome(
    described: &DescribedFrame,
    camera: &WallCamera,
    viewport: (u32, u32),
    scale_factor: f32,
    fonts: &mut ChromeFonts<'_>,
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
) -> PaperStyle {
    describe_frame_chrome_with_scrollback(
        described,
        camera,
        viewport,
        scale_factor,
        fonts,
        shapes,
        labels,
        |_| None,
    )
}

/// Same as [`describe_frame_chrome`], plus per-terminal scrollback indicators
/// whenever `scrollback(id)` reports a scrolled-back terminal (`viewing || offset > 0`).
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors describe_frame_chrome with one callback for session scrollback state"
)]
pub fn describe_frame_chrome_with_scrollback(
    described: &DescribedFrame,
    camera: &WallCamera,
    viewport: (u32, u32),
    scale_factor: f32,
    fonts: &mut ChromeFonts<'_>,
    shapes: &mut Vec<Shape>,
    labels: &mut Vec<Label<'static>>,
    scrollback: impl Fn(&str) -> Option<TerminalScrollback>,
) -> PaperStyle {
    let view = ChromeView::new(camera.zoom, scale_factor);
    let mut style = ChromeStyle {
        bar_max_world: BAR_ROOM as f32,
        // `WallNames` in `wall-core` has already made the bar-vs-map-label
        // decision (with hysteresis) and exposed it in `bar.text_visible`.
        min_text_px: 0.0,
        ..ChromeStyle::default()
    };
    if let Some(veil) = &described.fullscreen_backdrop {
        style.veil_fill = css_color(&veil.background, veil.opacity as f32);
    }

    let term_by_id: HashMap<&str, &TerminalFrame> = described
        .terminals
        .iter()
        .map(|t| (t.id.as_str(), t))
        .collect();

    // 1. Group frames & headers (behind member terminals).
    for group in &described.groups {
        push_group_frame(
            shapes,
            labels,
            fonts,
            &style,
            view,
            &GroupFrameDesc {
                backdrop_rect: to_world_rect(group.frame_rect),
                z: group.z as f32,
                name: &group.name,
                focused: group.focused,
                hovered: group.hovered,
            },
        );
    }

    // 2. Terminal title bars (always reserving `BAR_BUTTON_ROOM` on the right, matching `WallNames.js`).
    for bar in &described.title_bars {
        let strip_world = to_world_rect(bar.rect);

        let (body_rect, selected) = match term_by_id.get(bar.terminal_id.as_str()) {
            Some(tf) => (to_world_rect(tf.body_rect), tf.selected),
            None => (
                WorldRect::new(
                    Vec2::new(strip_world.min.x, strip_world.min.y - 1.0),
                    Vec2::new(strip_world.max.x, strip_world.min.y),
                ),
                false,
            ),
        };
        let mut desc = TitleBarDesc::new(body_rect, bar.z as f32, &bar.name);
        desc.agent_status = bar.agent_status;
        desc.focused = bar.focused;
        desc.hovered = bar.hovered;
        desc.selected = selected;
        desc.name_visible = bar.text_visible;
        desc.max_world_height = Some(bar.rect.height() as f32);
        desc.reserve_button_room = true;
        push_title_bar(shapes, labels, fonts, &style, view, &desc);
    }

    // 2b. Per-terminal scrolled-back indicators (silent on live terminals).
    for term in &described.terminals {
        if let Some(sb) = scrollback(&term.id) {
            if sb.viewing || sb.offset > 0 {
                push_scrollback_indicator(
                    shapes,
                    labels,
                    fonts,
                    view,
                    &ScrollbackIndicatorDesc {
                        body_rect: to_world_rect(term.body_rect),
                        z: term.z as f32,
                        offset: sb.offset,
                        max: sb.max,
                        visible_rows: term.rows.min(u32::from(u16::MAX)) as u16,
                        viewing: sb.viewing,
                        focused: term.focused,
                    },
                );
            }
        }
    }

    // 3. Multi-terminal selection outlines (`3px` outside each selected window).
    for (idx, sel) in described.selection_outlines.iter().enumerate() {
        push_selection_outline(
            shapes,
            &style,
            view,
            SelectionOutlineDesc {
                window_rect: to_world_rect(sel.rect),
                z: Z_SELECTION + idx as f32 * 0.01,
            },
        );
    }

    // 4. Shift-drag lasso polygon.
    if let Some(lasso) = &described.lasso {
        if lasso.len() >= 2 {
            let pts: Vec<Vec2> = lasso.iter().copied().map(to_vec2).collect();
            push_lasso(
                shapes,
                &style,
                view,
                LassoDesc {
                    points: &pts,
                    z: Z_OVERLAY,
                },
            );
        }
    }

    // 5. Tile / split drop preview (solid blue when accepted, dashed red when refused).
    if let Some(drop) = &described.drop_preview {
        push_drop_preview(
            shapes,
            &style,
            view,
            DropPreviewDesc {
                rect: to_world_rect(drop.rect),
                z: Z_OVERLAY + 1.0,
                accepted: drop.ok,
            },
        );
    }

    // 6. Fullscreen dimming veil with disjoint cutout strips around the active pane(s).
    if let Some(veil) = &described.fullscreen_backdrop {
        let holes: Vec<WorldRect> = veil.cutouts.iter().copied().map(to_world_rect).collect();
        push_fullscreen_veil(
            shapes,
            &style,
            &FullscreenVeilDesc {
                bounds: camera.visible_world_rect(viewport),
                holes: &holes,
                z: veil.z as f32,
            },
        );
    }

    // 7. Screen-placed map labels & leader lines for zoomed-out / narrow terminals.
    for (idx, ml) in described.map_labels.iter().enumerate() {
        let placement = Placement {
            label_box: ml.label_box,
            cand: Candidate::Above,
            leader: ml.leader,
        };
        push_map_label(
            shapes,
            labels,
            fonts,
            &style,
            view,
            &MapLabelDesc {
                text: &ml.text,
                focused: ml.focused,
                hovered: ml.hovered,
                z: Z_OVERLAY + 2.0 + idx as f32 * 0.02,
                geometry: MapLabelGeometry::Placed {
                    placement,
                    camera: *camera,
                    viewport,
                },
            },
        );
    }

    // 8. Top-of-wall daemon status notice and/or transient wall notice, centered
    //    between any docked side panels.
    let mut next_notice_top: Option<f32> = None;
    if let Some(sn) = described
        .status_notice
        .as_ref()
        .filter(|n| !n.text.is_empty())
    {
        let kind = match sn.tone {
            StatusTone::Red => NoticeKind::Red,
            StatusTone::Amber | StatusTone::Ok | StatusTone::Muted => NoticeKind::Amber,
        };
        let mut notice = NoticeDesc::new(&sn.text, kind, *camera, viewport, Z_OVERLAY + 10.0);
        notice.left_inset_css_px = described.panels.dock.inset.left as f32;
        notice.right_inset_css_px = described.panels.dock.inset.right as f32;
        push_notice(shapes, labels, fonts, &style, view, &notice);
        next_notice_top =
            Some(style.notice_top_px + style.notice_font_px + 2.0 * style.notice_pad_y_px + 6.0);
    }
    if let Some(text) = described.notice.as_deref().filter(|s| !s.is_empty()) {
        let mut notice = NoticeDesc::new(text, NoticeKind::Amber, *camera, viewport, Z_OVERLAY + 10.5);
        notice.left_inset_css_px = described.panels.dock.inset.left as f32;
        notice.right_inset_css_px = described.panels.dock.inset.right as f32;
        notice.top_css_px = next_notice_top;
        push_notice(shapes, labels, fonts, &style, view, &notice);
    }

    PaperStyle {
        show: described.paper.grid_show,
        background: css_color(&described.paper.background, 1.0),
        minor_color: css_color(&described.paper.minor_color, 1.0),
        major_color: css_color(&described.paper.major_color, 1.0),
        minor_spacing: described.paper.grid.grid_minor as f32,
        major_every: described.paper.grid.grid_major_every,
        line_px: described.paper.line_px as f32 * view.safe_scale(),
        min_px: described.paper.grid.grid_min_px as f32 * view.safe_scale(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyph_font::{FamilyId, FontAtlas};
    use wall_core::layout::Size;
    use wall_core::model::CellGeometry;
    use wall_core::wall::{TerminalAppeared, Wall, WallOptions};

    use crate::wall::metrics::CellMetrics;

    fn load_test_fonts() -> (CellMetrics, FontAtlas, FamilyId) {
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

    fn make_wall(metrics: CellMetrics) -> Wall {
        let cell_scale = 2.4 / metrics.cell_height;
        let opts = WallOptions {
            geometry: CellGeometry::with_padding(
                f64::from(metrics.cell_width * cell_scale),
                2.4,
                0.3,
                0.3,
            ),
            viewport: Size::new(1600.0, 1000.0),
            scale_factor: 2.0,
            ..WallOptions::default()
        };
        Wall::in_memory(opts)
    }

    #[test]
    fn describe_maps_groups_bars_selection_map_labels_and_notice_on_retina() {
        let (metrics, mut atlas, family) = load_test_fonts();
        let mut wall = make_wall(metrics);
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("build-runner"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("log-watcher"));
        let _ = wall.run("wall.group t1 t2");
        let _ = wall.run("wall.select t1");
        let _ = wall.run("wall.fit");
        let _ = wall.tick(1.0, 1000.0);

        let described = wall.frame();
        let sf = 2.0_f32;
        let vp = (3200, 2000);
        let cam = WallCamera::new(
            Vec2::new(described.camera.x as f32, described.camera.y as f32),
            described.camera.zoom as f32 * sf,
        );

        let mut shapes = Vec::new();
        let mut labels = Vec::new();
        let paper = {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(&described, &cam, vp, sf, &mut fonts, &mut shapes, &mut labels)
        };
        assert!(paper.show);
        // 1 group (2 rects) + 2 title bars (2 rects) + 1 selection outline (1 rect) = 5 shapes.
        assert_eq!(described.groups.len(), 1);
        assert_eq!(described.title_bars.len(), 2);
        assert_eq!(described.selection_outlines.len(), 1);
        assert_eq!(shapes.len(), 5);
        assert_eq!(labels.len(), 3, "1 group header + 2 terminal title bars");

        // Zoom far out so terminals hand off from title bars to map labels, and
        // trigger a refused-attach notice to exercise dock-inset notice rendering.
        wall.pinch(800.0, 500.0, 0.08);
        let _ = wall.run("wall.attach");
        let _ = wall.run("wall.sidebar left");
        let far = wall.frame();
        assert!(!far.map_labels.is_empty(), "far zoom should emit map labels");
        let far_cam = WallCamera::new(
            Vec2::new(far.camera.x as f32, far.camera.y as f32),
            far.camera.zoom as f32 * sf,
        );
        shapes.clear();
        labels.clear();
        {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(&far, &far_cam, vp, sf, &mut fonts, &mut shapes, &mut labels);
        }
        assert!(
            labels
                .iter()
                .any(|l| matches!(l.height, crate::wall::labels::LabelHeight::ScreenPx(_))),
            "map labels must render in constant ScreenPx height"
        );
    }

    #[test]
    fn describe_cuts_disjoint_holes_in_fullscreen_veil_for_single_and_group() {
        let (metrics, mut atlas, family) = load_test_fonts();
        let mut wall = make_wall(metrics);
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("pane-left"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("pane-right"));
        let _ = wall.run("wall.group t1 t2");
        let gid = wall.groups().ids().into_iter().next().expect("created group");
        let _ = wall.run(format!("wall.group.layout {gid} split"));

        // Enter group fullscreen on the 2-pane split, applying ResizeTerminal effects
        // and stepping camera easing just as `app.rs` does!
        let _ = wall.run("wall.sidebar left");
        let reply = wall.run(format!("wall.fullscreen on {gid}"));
        assert!(reply.text.starts_with("OK:"), "fullscreen failed: {}", reply.text);
        for effect in wall.drain_effects() {
            if let wall_core::model::WallEffect::ResizeTerminal { id, cols, rows } = effect {
                wall.terminal_resized(&id, cols, rows);
            }
        }
        for step in 1..=30 {
            let _ = wall.tick(1.0 / 30.0, f64::from(step) * 33.3);
        }
        let fs_frame = wall.frame();
        assert!(fs_frame.fullscreen_backdrop.is_some());

        let sf = 2.0_f32;
        let vp = (3200, 2000);
        let cam = WallCamera::new(
            Vec2::new(fs_frame.camera.x as f32, fs_frame.camera.y as f32),
            fs_frame.camera.zoom as f32 * sf,
        );
        let mut shapes = Vec::new();
        let mut labels = Vec::new();
        {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(&fs_frame, &cam, vp, sf, &mut fonts, &mut shapes, &mut labels);
        }

        let veil_z = fs_frame.fullscreen_backdrop.as_ref().unwrap().z as f32;
        let veil_rects: Vec<WorldRect> = shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Rect(r) if (r.z - veil_z).abs() < 1e-3 => Some(r.rect),
                _ => None,
            })
            .collect();
        assert!(
            veil_rects.len() >= 2,
            "veil with interior fullscreen cutout should decompose into border strips, got {}",
            veil_rects.len()
        );
        for t in fs_frame.terminals.iter().filter(|t| t.fullscreen) {
            let center = to_world_rect(t.body_rect).center();
            for vr in &veil_rects {
                assert!(
                    !vr.contains(center),
                    "veil strip {vr:?} must not cover fullscreen pane {} centre {center:?}",
                    t.id
                );
            }
        }
    }

    #[test]
    fn describe_wires_group_hover_agent_three_span_map_hover_and_status_notice() {
        use wall_core::bus::{GomuxAgent, GomuxSnapshot};
        use wall_core::input::Mods;
        use wall_core::pointer::{Buttons, PointerEvent};
        use wall_core::wall::NoScreens;

        let (metrics, mut atlas, family) = load_test_fonts();
        let mut wall = make_wall(metrics);
        wall.terminal_appeared(TerminalAppeared::new("t1", 80, 24).with_title("one"));
        wall.terminal_appeared(TerminalAppeared::new("t2", 80, 24).with_title("two"));
        let _ = wall.run("wall.group t1 t2");
        wall.set_bus_snapshot(
            Some(GomuxSnapshot {
                gen: 1,
                host: Some("local".to_owned()),
                agents: vec![GomuxAgent {
                    name: "helper".to_owned(),
                    host: Some("local".to_owned()),
                    live: true,
                    status: Some("waiting".to_owned()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            "",
        );
        let _ = wall.run("wall.attach helper");
        wall.terminal_appeared(TerminalAppeared::new("t3", 80, 24));
        let _ = wall.run("wall.fit");
        let _ = wall.tick(2.0, 2000.0);

        // Hover group header.
        let f0 = wall.frame();
        let hdr = f0.groups[0].header_rect;
        let hdr_pt = f0.camera.to_screen(
            Point::new(0.5 * (hdr.min_x + hdr.max_x), 0.5 * (hdr.min_y + hdr.max_y)),
            f0.viewport.w,
            f0.viewport.h,
        );
        wall.pointer_move(
            &PointerEvent {
                pointer_id: 1,
                x: hdr_pt.x,
                y: hdr_pt.y,
                button: None,
                buttons: Buttons::NONE,
                mods: Mods::default(),
                time_ms: 2100.0,
            },
            &NoScreens,
        );
        // Trigger a Red daemon status notice alongside the group hover and agent-attached title bar.
        wall.on_termd_brain_failed(2100);
        let f1 = wall.frame();
        assert!(f1.groups[0].hovered);
        assert!(f1.status_notice.is_some());

        let sf = 2.0_f32;
        let vp = (3200, 2000);
        let cam = WallCamera::new(
            Vec2::new(f1.camera.x as f32, f1.camera.y as f32),
            f1.camera.zoom as f32 * sf,
        );
        let mut shapes = Vec::new();
        let mut labels = Vec::new();
        {
            let mut fonts = ChromeFonts::new(metrics, &mut atlas, family);
            describe_frame_chrome(&f1, &cam, vp, sf, &mut fonts, &mut shapes, &mut labels);
        }
        // Three-span agent title emits separate `helper@local`, ` · `, and `waiting` labels.
        assert!(labels.iter().any(|l| l.text == "helper@local"));
        assert!(labels.iter().any(|l| l.text == " · "));
        assert!(labels.iter().any(|l| l.text == "waiting"));
        // Red status notice emits a label with the daemon outage text and red notice fill shape.
        assert!(labels.iter().any(|l| l.text.contains("termd's engine is down")));
        let style = ChromeStyle::default();
        assert!(shapes.iter().any(|s| matches!(
            s,
            Shape::Rect(r) if r.fill == Some(style.group_header_fill_hovered)
        )));
        assert!(shapes.iter().any(|s| matches!(
            s,
            Shape::Rect(r) if r.fill == Some(style.notice_red_fill)
        )));
    }
}
