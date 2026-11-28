//! Chrome panels drawn with `egui` over the terminal wall (`out/wall-port/briefs/P-panels.md`).
//!
//! Each panel is a pure `show(ctx, state, input, out)` function backed by its
//! model in `wall_core`. Panels read plain data from `input`, hold only their
//! own view state in `state`, and emit CLI verb lines into `out` (such as
//! `"wall.reveal t3"` or `"wall.sidebar float 24 40"`) without touching
//! terminals, the store, or the daemon.

// The settings panel and the status window arrived with a shared kit of
// their own (docked.rs); they are re-exported here under their plain names.
pub mod docked;
// The right-click menu and the help panel, with their own kit (menus.rs).
pub mod menus;
// The Agent View companion card, anchored to its terminal in the world.
pub mod agent_view;
// The buttons on a terminal's title bar, and the bar across the top of the window.
pub mod bar_buttons;
pub mod palette;
// The grips on a panel's edges: drag to resize, double-click for the default.
pub mod resize;
pub mod selection_bar;
pub mod sidebar;
pub mod topbar;
pub use docked::{settings, status};
pub use menus::{context_menu, help};

use std::path::Path;

use wall_core::dock::DockSide;

/// Format a `wall.*` command argv slice into a single verb line parseable by
/// [`wall_core::router::CommandRouter::parse_line`].
///
/// Plain tokens (`"wall.reveal"`, `"t3"`, `"-1"`) are joined with single spaces
/// verbatim; tokens containing spaces, quotes, backslashes, newlines, or tabs
/// (such as a multi-word terminal rename) are quoted and escaped so
/// `CommandRouter::parse_line` round-trips them losslessly.
pub fn format_verb_line(argv: &[String]) -> String {
    let mut parts = Vec::with_capacity(argv.len());
    for token in argv {
        let needs_quotes = token.is_empty()
            || token
                .chars()
                .any(|c| matches!(c, ' ' | '"' | '\\' | '\n' | '\t'));
        if !needs_quotes {
            parts.push(token.clone());
        } else {
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
            parts.push(quoted);
        }
    }
    parts.join(" ")
}

/// Replace macOS modifier glyphs absent from egui's bundled `Hack` font (`⇧`,
/// `⌥`, `⌃`, which otherwise draw dotted missing-glyph boxes; `⌘` IS present in
/// `Hack`) with readable ASCII equivalents.
pub(crate) fn sanitize_egui_text(text: &str) -> String {
    if !text.contains(['⇧', '⌥', '⌃']) {
        return text.to_string();
    }
    text.replace('⇧', "Shift+")
        .replace('⌥', "Opt+")
        .replace('⌃', "Ctrl+")
}

/// Lay out `text` on a single line, truncating with `…` if it exceeds `max_width`.
pub(crate) fn layout_elided(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let clean = sanitize_egui_text(text);
    let full = painter.layout_no_wrap(clean.clone(), font.clone(), color);
    if full.size().x <= max_width || max_width <= 8.0 {
        return full;
    }
    let chars: Vec<char> = clean.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        let g = painter.layout_no_wrap(candidate, font.clone(), color);
        if g.size().x <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let truncated = if lo == 0 {
        "…".to_string()
    } else {
        chars[..lo].iter().collect::<String>() + "…"
    };
    painter.layout_no_wrap(truncated, font, color)
}

/// Parse a `#rrggbb` hex colour string into [`egui::Color32`].
pub(crate) fn parse_hex_color(hex: &str) -> Option<egui::Color32> {
    let raw = hex.strip_prefix('#')?;
    if raw.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

/// Finite viewport rectangle in logical points, falling back to `1280x800` when
/// headless [`egui::RawInput`] leaves `screen_rect` unbounded.
pub(crate) fn safe_viewport_rect(ctx: &egui::Context) -> egui::Rect {
    let raw = ctx.input(|i| i.viewport_rect());
    let min_x = if raw.min.x.is_finite() { raw.min.x } else { 0.0 };
    let min_y = if raw.min.y.is_finite() { raw.min.y } else { 0.0 };
    let w = if raw.width().is_finite() && raw.width() > 0.0 && raw.width() < 32_768.0 {
        raw.width()
    } else {
        1280.0
    };
    let h = if raw.height().is_finite() && raw.height() > 0.0 && raw.height() < 32_768.0 {
        raw.height()
    } else {
        800.0
    };
    egui::Rect::from_min_size(egui::pos2(min_x, min_y), egui::vec2(w, h))
}

/// Vector chrome icons shared by side panels (`Panel.jsx` / `Sidebar.jsx`),
/// drawn directly with [`egui::Painter`] so they never depend on system font
/// glyph coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeIcon {
    DockLeft,
    DockFloat,
    DockRight,
    Close,
    ChevronRight,
    ChevronDown,
    AgentView,
    LeaveGroup,
    ShowCircle,
    HideDash,
    Minus,
    Plus,
    Ungroup,
}

pub(crate) fn paint_chrome_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: ChromeIcon,
    color: egui::Color32,
) {
    let c = rect.center();
    let stroke = egui::Stroke::new(1.35, color);
    match icon {
        ChromeIcon::DockLeft => {
            let x_bar = c.x - 4.5;
            painter.line_segment([egui::pos2(x_bar, c.y - 4.5), egui::pos2(x_bar, c.y + 4.5)], stroke);
            painter.line_segment([egui::pos2(c.x + 4.5, c.y), egui::pos2(x_bar + 1.5, c.y)], stroke);
            painter.line_segment(
                [egui::pos2(x_bar + 4.0, c.y - 2.6), egui::pos2(x_bar + 1.4, c.y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(x_bar + 4.0, c.y + 2.6), egui::pos2(x_bar + 1.4, c.y)],
                stroke,
            );
        }
        ChromeIcon::DockFloat => {
            let r = egui::Rect::from_center_size(c, egui::vec2(9.5, 8.0));
            painter.rect_stroke(r, egui::CornerRadius::same(1), stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [egui::pos2(r.left(), r.top() + 2.5), egui::pos2(r.right(), r.top() + 2.5)],
                stroke,
            );
        }
        ChromeIcon::DockRight => {
            let x_bar = c.x + 4.5;
            painter.line_segment([egui::pos2(x_bar, c.y - 4.5), egui::pos2(x_bar, c.y + 4.5)], stroke);
            painter.line_segment([egui::pos2(c.x - 4.5, c.y), egui::pos2(x_bar - 1.5, c.y)], stroke);
            painter.line_segment(
                [egui::pos2(x_bar - 4.0, c.y - 2.6), egui::pos2(x_bar - 1.4, c.y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(x_bar - 4.0, c.y + 2.6), egui::pos2(x_bar - 1.4, c.y)],
                stroke,
            );
        }
        ChromeIcon::Close => {
            let d = 3.6;
            painter.line_segment([egui::pos2(c.x - d, c.y - d), egui::pos2(c.x + d, c.y + d)], stroke);
            painter.line_segment([egui::pos2(c.x + d, c.y - d), egui::pos2(c.x - d, c.y + d)], stroke);
        }
        ChromeIcon::ChevronRight => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - 2.2, c.y - 3.6),
                    egui::pos2(c.x + 2.8, c.y),
                    egui::pos2(c.x - 2.2, c.y + 3.6),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        ChromeIcon::ChevronDown => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - 3.6, c.y - 2.0),
                    egui::pos2(c.x + 3.6, c.y - 2.0),
                    egui::pos2(c.x, c.y + 2.8),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        ChromeIcon::AgentView => {
            for dy in [-3.2_f32, 0.0, 3.2] {
                painter.line_segment([egui::pos2(c.x - 4.0, c.y + dy), egui::pos2(c.x + 4.0, c.y + dy)], stroke);
            }
        }
        ChromeIcon::LeaveGroup => {
            // Arrow pointing up-left out of a corner (`⇱`).
            let tl = egui::pos2(c.x - 3.6, c.y - 3.6);
            let br = egui::pos2(c.x + 3.4, c.y + 3.4);
            painter.line_segment([br, tl], stroke);
            painter.line_segment([tl, egui::pos2(tl.x + 4.2, tl.y)], stroke);
            painter.line_segment([tl, egui::pos2(tl.x, tl.y + 4.2)], stroke);
        }
        ChromeIcon::ShowCircle => {
            painter.circle_stroke(c, 3.8, stroke);
        }
        ChromeIcon::HideDash => {
            painter.line_segment([egui::pos2(c.x - 3.8, c.y), egui::pos2(c.x + 3.8, c.y)], stroke);
        }
        ChromeIcon::Minus => {
            painter.line_segment([egui::pos2(c.x - 3.5, c.y), egui::pos2(c.x + 3.5, c.y)], stroke);
        }
        ChromeIcon::Plus => {
            painter.line_segment([egui::pos2(c.x - 3.5, c.y), egui::pos2(c.x + 3.5, c.y)], stroke);
            painter.line_segment([egui::pos2(c.x, c.y - 3.5), egui::pos2(c.x, c.y + 3.5)], stroke);
        }
        ChromeIcon::Ungroup => {
            painter.circle_stroke(c, 4.0, stroke);
            painter.line_segment(
                [egui::pos2(c.x - 2.8, c.y + 2.8), egui::pos2(c.x + 2.8, c.y - 2.8)],
                stroke,
            );
        }
    }
}

/// Non-focusable `18x18` chrome icon button (`Btn` in `wall/Panel.jsx`).
pub(crate) fn chrome_icon_btn(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    tooltip: &str,
    active: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if active || hovered {
            let bg = if active {
                egui::Color32::from_rgb(0x1d, 0x23, 0x30)
            } else {
                egui::Color32::from_rgb(0x23, 0x2a, 0x3a)
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), bg);
        }
        let fg = if active || hovered {
            egui::Color32::from_rgb(0xe6, 0xe9, 0xef)
        } else {
            egui::Color32::from_rgb(0x8b, 0x95, 0xa7)
        };
        paint_chrome_icon(ui.painter(), rect, icon, fg);
    }
    if hovered_for_pointer(&resp) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(sanitize_egui_text(tooltip))
}

fn hovered_for_pointer(resp: &egui::Response) -> bool {
    resp.hovered()
}

/// Header dock-side and close strip (`DockButtons` in `wall/Panel.jsx`).
pub(crate) fn dock_buttons(
    ui: &mut egui::Ui,
    verb: &str,
    side: &mut DockSide,
    open: &mut bool,
    close_tooltip: &str,
    out: &mut Vec<String>,
) {
    if chrome_icon_btn(
        ui,
        ChromeIcon::DockLeft,
        "stick to the left edge",
        *side == DockSide::Left,
    )
    .clicked()
    {
        *side = DockSide::Left;
        out.push(format!("{verb} left"));
    }
    if chrome_icon_btn(
        ui,
        ChromeIcon::DockFloat,
        "float",
        *side == DockSide::Float,
    )
    .clicked()
    {
        *side = DockSide::Float;
        out.push(format!("{verb} float"));
    }
    if chrome_icon_btn(
        ui,
        ChromeIcon::DockRight,
        "stick to the right edge",
        *side == DockSide::Right,
    )
    .clicked()
    {
        *side = DockSide::Right;
        out.push(format!("{verb} right"));
    }
    if chrome_icon_btn(ui, ChromeIcon::Close, close_tooltip, false).clicked() {
        *open = false;
        out.push(format!("{verb} close"));
    }
}

/// Offscreen helper: run `draw` in a fresh [`egui::Context`] over a solid
/// `clear` background and write the rendered RGBA8 image to `path` using
/// `egui_wgpu::Renderer`.
pub fn render_egui_offscreen_to_png(
    ctx: &crate::gpu::GpuContext,
    width: u32,
    height: u32,
    pixels_per_point: f32,
    clear: wgpu::Color,
    mut draw: impl FnMut(&egui::Context),
    path: &Path,
) {
    let device = &ctx.device;
    let queue = &ctx.queue;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut egui_renderer =
        egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default());
    let egui_ctx = egui::Context::default();

    let logical_w = (width as f32) / pixels_per_point;
    let logical_h = (height as f32) / pixels_per_point;
    let screen_rect =
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(logical_w, logical_h));

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("wall panel offscreen target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Two passes let egui settle any first-frame font/area sizing.
    for pass_idx in 0..2 {
        let mut raw = egui::RawInput {
            screen_rect: Some(screen_rect),
            time: Some(f64::from(pass_idx) * (1.0 / 60.0)),
            ..egui::RawInput::default()
        };
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(pixels_per_point);

        let egui::FullOutput {
            mut textures_delta,
            shapes,
            pixels_per_point,
            ..
        } = egui_ctx.run_ui(raw, |root_ui| {
            draw(root_ui.ctx());
        });
        let clipped = egui_ctx.tessellate(shapes, pixels_per_point);
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point,
        };
        for (id, deltas) in textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(device, queue, id, &delta);
            }
        }

        if pass_idx == 1 {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("wall panel offscreen pass"),
            });
            let user_cmds =
                egui_renderer.update_buffers(device, queue, &mut encoder, &clipped, &screen_desc);
            if !user_cmds.is_empty() {
                queue.submit(user_cmds);
            }
            {
                let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("wall panel egui pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..wgpu::RenderPassDescriptor::default()
                });
                egui_renderer.render(&mut rpass.forget_lifetime(), &clipped, &screen_desc);
            }

            let unpadded_bpr = width * 4;
            let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("wall panel offscreen readback"),
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
                size,
            );
            queue.submit([encoder.finish()]);

            let slice = readback.slice(..);
            glyph_field::map_read(
                device,
                &slice,
                crate::config::settings().gpu.wait_limit(),
                "wall panel offscreen readback",
            );
            let mapped = slice
                .get_mapped_range()
                .expect("wall panel offscreen readback mapped");
            let mut rgba = Vec::with_capacity((unpadded_bpr * height) as usize);
            for row in 0..height {
                let start = (row * padded_bpr) as usize;
                rgba.extend_from_slice(&mapped[start..start + unpadded_bpr as usize]);
            }
            drop(mapped);
            readback.unmap();

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .expect("failed to create parent directory for panel PNG");
            }
            image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)
                .expect("failed to save panel PNG");
        }

        for id in textures_delta.free.drain() {
            egui_renderer.free_texture(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use wall_core::config::GroupLayout;
    use wall_core::dock::{dock_layout, DockPanel, DockState};
    use wall_core::input::Platform;
    use wall_core::palette::{PaletteGroup, PaletteSnapshot, PaletteTerminal};
    use wall_core::router::CommandRouter;
    use wall_core::sidebar::{
        build_sidebar, BusAgentInput, BusSnapshot, SidebarGroup, SidebarInput as CoreSidebarInput,
        SidebarMode, SidebarTerminal,
    };

    #[test]
    fn format_verb_line_round_trips_through_command_router_parse_line() {
        let plain = vec!["wall.reveal".to_string(), "term-3".to_string()];
        assert_eq!(format_verb_line(&plain), "wall.reveal term-3");
        assert_eq!(CommandRouter::<()>::parse_line(&format_verb_line(&plain)), plain);

        let spaced = vec![
            "wall.rename".to_string(),
            "term-1".to_string(),
            "api \"prod\" \\ worker".to_string(),
        ];
        let line = format_verb_line(&spaced);
        assert_eq!(CommandRouter::<()>::parse_line(&line), spaced);
    }

    #[test]
    fn gpu_renders_palette_and_sidebar_panels_to_png() {
        let gpu = crate::gpu::test_device::context();
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));

        let bus = BusSnapshot {
            host: "laptop-2".to_string(),
            reason: String::new(),
            agents: vec![
                BusAgentInput {
                    name: "wall-lead".to_string(),
                    host: "laptop-2".to_string(),
                    live: true,
                    status: "processing".to_string(),
                    attached: Some(true),
                    detail: "merging panels".to_string(),
                    cwd: "/Users/user/localdev/glyph3d-rs".to_string(),
                    ..BusAgentInput::default()
                },
                BusAgentInput {
                    name: "wall-layout".to_string(),
                    host: "laptop-2".to_string(),
                    live: true,
                    status: "waiting".to_string(),
                    attached: Some(true),
                    detail: "reviewing PNG".to_string(),
                    ..BusAgentInput::default()
                },
                BusAgentInput {
                    name: "bench-runner".to_string(),
                    host: "workstation".to_string(),
                    live: true,
                    status: "idle".to_string(),
                    attached: None,
                    detail: "ready".to_string(),
                    ..BusAgentInput::default()
                },
                BusAgentInput {
                    name: "linux-gate".to_string(),
                    host: "workstation".to_string(),
                    live: true,
                    status: "error".to_string(),
                    attached: None,
                    ..BusAgentInput::default()
                },
            ],
            stale: None,
        };

        // The sidebar's one list: a group over two hosts (a local agent in
        // its `gx-` terminal, a hidden shell, another host's agent in an
        // attach terminal), a folded group, and agents with no terminal.
        let sb_terminal = |id: &str, name: &str, group: Option<&str>, attached: Option<&str>| {
            SidebarTerminal {
                id: id.to_string(),
                name: name.to_string(),
                group: group.map(str::to_string),
                hidden: false,
                attached: attached.map(str::to_string),
                agent_view_open: false,
            }
        };
        let raw_sb = CoreSidebarInput {
            terminals: vec![
                SidebarTerminal {
                    agent_view_open: true,
                    ..sb_terminal(
                        "gx-wall-lead",
                        "wall-lead",
                        Some("g-1"),
                        Some("wall-lead@laptop-2"),
                    )
                },
                SidebarTerminal {
                    hidden: true,
                    ..sb_terminal("term-2", "termd logs", Some("g-1"), None)
                },
                sb_terminal(
                    "term-4",
                    "bench",
                    Some("g-1"),
                    Some("bench-runner@workstation"),
                ),
                sb_terminal("term-5", "notes", Some("g-2"), None),
                sb_terminal("term-3", "zsh — glyph3d-rs", None, None),
            ],
            groups: vec![
                SidebarGroup {
                    id: "g-1".to_string(),
                    name: "fleet".to_string(),
                    layout: GroupLayout::Tile,
                    columns: 2,
                    members: vec![
                        "gx-wall-lead".to_string(),
                        "term-2".to_string(),
                        "term-4".to_string(),
                    ],
                },
                SidebarGroup {
                    id: "g-2".to_string(),
                    name: "folded docs".to_string(),
                    layout: GroupLayout::Freeform,
                    columns: 1,
                    members: vec!["term-5".to_string()],
                },
            ],
            focused_id: Some("gx-wall-lead".to_string()),
            collapsed_groups: BTreeSet::from(["g-2".to_string()]),
            bus: bus.clone(),
            mode: SidebarMode::Group,
        };
        let dock = dock_layout(&DockState {
            sidebar: DockPanel::new(true, DockSide::Left),
            ..DockState::default()
        });
        let sb_input = sidebar::SidebarInput {
            model: build_sidebar(&raw_sb),
            dock_rect: sidebar::sidebar_dock_rect(viewport, DockSide::Left, &dock),
            platform: Platform::Mac,
            close_hold_ms: 450,
        };
        let mut sb_state = sidebar::SidebarState::new(true, DockSide::Left);

        let pal_input = palette::PaletteInput::from(PaletteSnapshot {
            terminals: vec![
                PaletteTerminal {
                    id: "gx-wall-lead".to_string(),
                    name: "wall-lead".to_string(),
                    named: true,
                    group: Some("g1".to_string()),
                    hidden: false,
                    preview: "cargo build --release -p glyph3d-native".to_string(),
                    attached: Some("wall-lead@laptop-2".to_string()),
                },
                PaletteTerminal {
                    id: "term-2".to_string(),
                    name: "termd logs".to_string(),
                    named: true,
                    group: Some("g1".to_string()),
                    hidden: true,
                    preview: "listening on rs-wall.sock".to_string(),
                    attached: None,
                },
                PaletteTerminal {
                    id: "term-3".to_string(),
                    name: "zsh — glyph3d-rs".to_string(),
                    named: false,
                    group: None,
                    hidden: false,
                    preview: "git status -sb".to_string(),
                    attached: None,
                },
            ],
            groups: vec![PaletteGroup {
                id: "g1".to_string(),
                name: "Core Fleet".to_string(),
                named: true,
                layout: GroupLayout::Tile,
                columns: 2,
                members: vec!["gx-wall-lead".to_string(), "term-2".to_string()],
            }],
            focused_id: Some("gx-wall-lead".to_string()),
            selection: Vec::new(),
            sidebar_open: true,
            settings_open: false,
            snap: true,
            grid_show: true,
            platform: Platform::Mac,
            relay_port: None,
            bus,
        });
        let mut pal_state = palette::PaletteState::default();
        pal_state.open_find();
        pal_state.palette.selected = 1;

        let out_path = std::env::var_os("GLYPH_PANEL_PROOF_PNG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("glyph3d_wall_panels_proof.png"));
        let mut out_verbs = Vec::new();
        render_egui_offscreen_to_png(
            &gpu,
            1280,
            800,
            1.0,
            wgpu::Color {
                r: 0.012,
                g: 0.014,
                b: 0.022,
                a: 1.0,
            },
            |ctx| {
                sidebar::show(ctx, &mut sb_state, &sb_input, &mut out_verbs);
                palette::show(ctx, &mut pal_state, &pal_input, &mut out_verbs);
            },
            &out_path,
        );
        assert!(out_path.exists(), "expected panel proof PNG at {out_path:?}");
        let img = image::open(&out_path).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (1280, 800));
        // Left docked sidebar (x=100, y=100) and centered palette (x=640, y=200) must differ from clear background.
        let clear_px = *img.get_pixel(1200, 760);
        let sb_px = *img.get_pixel(100, 100);
        let pal_px = *img.get_pixel(640, 200);
        assert_ne!(sb_px, clear_px, "sidebar region should be drawn");
        assert_ne!(pal_px, clear_px, "palette region should be drawn");

        // A member's row carries its group's chip, and each group's is its
        // own colour: g-1 the palette's blue, g-2 its orange. A row in no
        // group has none. (This helper's target darkens what egui draws, so
        // the pixels are judged by hue, not held to the hex.)
        let px = |img: &image::RgbaImage, x: f32, y: f32| *img.get_pixel(x as u32, y as u32);
        let member = sb_state.term_row_rects["term-4"];
        let chip_px = px(&img, member.left() + 11.0, member.center().y);
        let beside_px = px(&img, member.left() + 16.0, member.center().y);
        assert_ne!(chip_px, beside_px, "a member row draws its chip");
        assert!(
            chip_px[2] > chip_px[0].saturating_add(60),
            "g-1's chip is blue: {chip_px:?}"
        );
        let folded = sb_state.group_fold_rects["g-2"];
        let head_px = px(&img, folded.right() + 5.0, folded.center().y);
        assert!(
            head_px[0] > head_px[2].saturating_add(60),
            "g-2's chip is orange: {head_px:?}"
        );
        // (The target dithers: the panel's fill varies by a level.)
        let loose = sb_state.term_row_rects["term-3"];
        let (bare_px, bare_beside) = (
            px(&img, loose.left() + 11.0, loose.center().y),
            px(&img, loose.left() + 16.0, loose.center().y),
        );
        assert!(
            (0..3).all(|c| bare_px[c].abs_diff(bare_beside[c]) <= 3),
            "a row in no group has no chip: {bare_px:?} beside {bare_beside:?}"
        );

        // The same wall listed by host, on a PNG beside the first.
        let by_host_path = out_path.with_file_name(format!(
            "{}-by-host.png",
            out_path.file_stem().unwrap().to_string_lossy()
        ));
        let sb_by_host = sidebar::SidebarInput {
            model: build_sidebar(&CoreSidebarInput {
                mode: SidebarMode::Host,
                ..raw_sb
            }),
            ..sb_input
        };
        let mut sb_state = sidebar::SidebarState::new(true, DockSide::Left);
        render_egui_offscreen_to_png(
            &gpu,
            1280,
            800,
            1.0,
            wgpu::Color {
                r: 0.012,
                g: 0.014,
                b: 0.022,
                a: 1.0,
            },
            |ctx| sidebar::show(ctx, &mut sb_state, &sb_by_host, &mut out_verbs),
            &by_host_path,
        );
        let by_host = image::open(&by_host_path).unwrap().to_rgba8();
        assert_eq!(
            sb_state.rows_drawn,
            vec![
                "h:laptop-2[5]",
                "t:gx-wall-lead+wall-lead@laptop-2=processing",
                "t:term-2",
                "t:term-5",
                "t:term-3",
                "a:wall-layout@laptop-2=waiting",
                "h:workstation[2]",
                "t:term-4+bench-runner@workstation=idle",
                "a:linux-gate@workstation=error",
            ]
        );
        // Each grouped row keeps its chip under its host.
        let member = sb_state.term_row_rects["term-4"];
        let chip_px = px(&by_host, member.left() + 11.0, member.center().y);
        assert!(
            chip_px[2] > chip_px[0].saturating_add(60),
            "g-1's chip is blue by host too: {chip_px:?}"
        );
    }
}
