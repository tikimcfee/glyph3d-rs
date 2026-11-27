//! Native egui overlay panels for the terminal wall (`out/wall-port/briefs/P-panels.md`).
//!
//! Every panel is a stateless drawing function:
//! `show(ctx, state, input, out) -> bool`
//! reading plain data built from `wall_core`, keeping view-only state in `state`,
//! and pushing user-typed verb lines (`wall.config …`, `wall.reveal …`, `wall.status …`)
//! into `out` without touching terminals, the store, or the daemon directly.

use std::path::Path;

use wall_core::dock::DockSide;
use wall_core::geom::Point;
use wall_core::settings::HitRange;
use wall_core::status::StatusTone;
use wall_core::store::PanelSide;

pub mod settings;
pub mod status;

// ── Shared JS wall palette (`SettingsPanel.jsx`, `StatusWindow.jsx`, `Panel.jsx`) ──

pub(crate) const BORDER: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x30, 0x3c);
pub(crate) const HEAD_BORDER: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x24, 0x30);
pub(crate) const TEXT: egui::Color32 = egui::Color32::from_rgb(0xc8, 0xce, 0xd8);
pub(crate) const MUTED: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x73, 0x83);
pub(crate) const LABEL: egui::Color32 = egui::Color32::from_rgb(0xd4, 0xd9, 0xe2);
pub(crate) const BRIGHT: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xe9, 0xef);
pub(crate) const DESC_TEXT: egui::Color32 = egui::Color32::from_rgb(0x8b, 0x95, 0xa7);
pub(crate) const KEY_TEXT: egui::Color32 = egui::Color32::from_rgb(0x4f, 0x57, 0x67);
pub(crate) const OK_GREEN: egui::Color32 = egui::Color32::from_rgb(0x6e, 0xe7, 0xa0);
pub(crate) const AMBER: egui::Color32 = egui::Color32::from_rgb(0xe3, 0xb3, 0x41);
pub(crate) const ERR_RED: egui::Color32 = egui::Color32::from_rgb(0xe5, 0x53, 0x4b);
pub(crate) const ACTIVE_ROW_BG: egui::Color32 = egui::Color32::from_rgb(0x18, 0x20, 0x2d);
pub(crate) const HOVER_ROW_BG: egui::Color32 = egui::Color32::from_rgb(0x1b, 0x21, 0x30);
pub(crate) const FIELD_BG: egui::Color32 = egui::Color32::from_rgb(0x0b, 0x0d, 0x13);
pub(crate) const BTN_ON_BG: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x23, 0x30);
pub(crate) const NOTICE_BG: egui::Color32 = egui::Color32::from_rgb(0x14, 0x18, 0x21);

pub(crate) fn settings_panel_bg() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(14, 17, 24, 247)
}

pub(crate) fn status_panel_bg() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(20, 24, 33, 247)
}

pub(crate) const fn tone_color(tone: StatusTone) -> egui::Color32 {
    match tone {
        StatusTone::Ok => OK_GREEN,
        StatusTone::Muted => MUTED,
        StatusTone::Amber => AMBER,
        StatusTone::Red => ERR_RED,
    }
}

/// Convert [`DockSide`] (`wall_core::dock`) into [`PanelSide`] (`wall_core::store`).
pub const fn dock_side_to_panel_side(side: DockSide) -> PanelSide {
    match side {
        DockSide::Left => PanelSide::Left,
        DockSide::Right => PanelSide::Right,
        DockSide::Float => PanelSide::Float,
    }
}

/// Convert [`PanelSide`] (`wall_core::store`) into [`DockSide`] (`wall_core::dock`).
pub const fn panel_side_to_dock_side(side: PanelSide) -> DockSide {
    match side {
        PanelSide::Left => DockSide::Left,
        PanelSide::Right => DockSide::Right,
        PanelSide::Float => DockSide::Float,
    }
}

/// JavaScript `Math.round` (`+0.5` towards `+∞`) so `float <x> <y>` coordinates match `Panel.jsx`.
pub(crate) fn js_round(x: f32) -> i64 {
    (f64::from(x) + 0.5).floor() as i64
}

/// Read the logical viewport rectangle in both windowed (`egui-winit`) and headless (`RawInput`) runs.
pub(crate) fn panel_viewport_rect(ctx: &egui::Context) -> egui::Rect {
    ctx.input(|i| i.raw.screen_rect.unwrap_or_else(|| i.viewport_rect()))
}

/// Drag state for a floating panel's header bar (`useHeadDrag` in `wall/Panel.jsx`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeadDragState {
    /// Current top-left of the floating panel while a drag is in progress, or `None` when idle.
    pub drag_at: Option<egui::Pos2>,
    drag_offset: egui::Vec2,
}

/// Compute the target bounding rectangle for a panel (`SettingsPanel.jsx` / `StatusWindow.jsx`).
///
/// When `side` is `Left` or `Right`, uses `dock_rect` if provided by the caller, or falls back to
/// flush-left / flush-right of `viewport` with `default_width`. When `side` is `Float`, resolves
/// `drag.drag_at` → `stored_float` → top-right `(right: 12, top: 46)` and caps height at
/// `viewport.height() - 120`.
pub(crate) fn resolve_panel_box(
    viewport: egui::Rect,
    side: PanelSide,
    dock_rect: Option<egui::Rect>,
    stored_float: Option<Point>,
    drag: &HeadDragState,
    default_width: f32,
) -> (egui::Pos2, f32, f32) {
    match side {
        PanelSide::Left | PanelSide::Right => {
            if let Some(rect) = dock_rect {
                return (
                    rect.min,
                    rect.width().max(120.0),
                    rect.height().max(80.0),
                );
            }
            let width = default_width.min(viewport.width().max(120.0));
            let x = match side {
                PanelSide::Left => viewport.left(),
                PanelSide::Right => (viewport.right() - width).max(viewport.left()),
                PanelSide::Float => unreachable!(),
            };
            (egui::pos2(x, viewport.top()), width, viewport.height().max(80.0))
        }
        PanelSide::Float => {
            let max_w = (viewport.width() - 32.0).max(140.0);
            let width = default_width.min(max_w);
            let max_h = (viewport.height() - 120.0).max(96.0);
            let origin = if let Some(at) = drag.drag_at {
                at
            } else if let Some(pt) = stored_float {
                egui::pos2(pt.x as f32, pt.y as f32)
            } else {
                egui::pos2(
                    (viewport.right() - width - 12.0).max(viewport.left() + 8.0),
                    viewport.top() + 46.0,
                )
            };
            (origin, width, max_h)
        }
    }
}

/// Update `drag` from a floating header drag response and push `<verb> float <x> <y>` on release.
pub(crate) fn handle_head_drag(
    ui: &egui::Ui,
    head_resp: &egui::Response,
    side: PanelSide,
    panel_origin: egui::Pos2,
    drag: &mut HeadDragState,
    verb: &str,
    out: &mut Vec<String>,
) {
    if side != PanelSide::Float {
        drag.drag_at = None;
        return;
    }
    if head_resp.hovered() || head_resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
    }
    if head_resp.drag_started_by(egui::PointerButton::Primary) {
        if let Some(ptr) = ui.ctx().input(|i| i.pointer.interact_pos()) {
            drag.drag_offset = ptr - panel_origin;
            drag.drag_at = Some(panel_origin);
        }
    }
    if head_resp.dragged_by(egui::PointerButton::Primary) {
        if let Some(ptr) = ui.ctx().input(|i| i.pointer.interact_pos()) {
            let next = egui::pos2(
                (ptr.x - drag.drag_offset.x).max(0.0),
                (ptr.y - drag.drag_offset.y).max(0.0),
            );
            drag.drag_at = Some(next);
        }
    }
    if head_resp.drag_stopped_by(egui::PointerButton::Primary) {
        if let Some(last) = drag.drag_at.take() {
            out.push(format!(
                "{verb} float {} {}",
                js_round(last.x),
                js_round(last.y)
            ));
        }
    }
}

/// Icon drawn inside a header chrome button (`Panel.jsx`'s `DockButtons` and `SettingsPanel.jsx`'s `↺`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeIcon {
    DockLeft,
    DockFloat,
    DockRight,
    Close,
    Reset,
}

/// Paint an `18×18` non-focus-stealing icon button (`Btn` in `wall/Panel.jsx`).
///
/// Uses `allocate_exact_size(..., Sense::click())` so clicking never moves keyboard focus
/// away from the search box or the focused terminal.
pub(crate) fn icon_btn(
    ui: &mut egui::Ui,
    id_salt: (&str, &str),
    icon: ChromeIcon,
    on: bool,
    tooltip: &str,
) -> egui::Response {
    let (_id, rect) = ui.allocate_space(egui::vec2(18.0, 18.0));
    let resp = ui
        .interact(rect, ui.make_persistent_id(id_salt), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if on {
            painter.rect_filled(rect, egui::CornerRadius::same(4), BTN_ON_BG);
        } else if resp.hovered() {
            painter.rect_filled(rect, egui::CornerRadius::same(4), HOVER_ROW_BG);
        }
        let fg = if on || resp.hovered() { BRIGHT } else { DESC_TEXT };
        let stroke = egui::Stroke::new(1.25, fg);
        let c = rect.center();
        match icon {
            ChromeIcon::DockLeft => {
                // `⇤`: vertical edge bar on the left + left-pointing arrow.
                painter.line_segment(
                    [egui::pos2(c.x - 5.0, c.y - 4.5), egui::pos2(c.x - 5.0, c.y + 4.5)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x + 5.0, c.y), egui::pos2(c.x - 3.0, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x - 0.5, c.y - 3.0), egui::pos2(c.x - 3.5, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x - 0.5, c.y + 3.0), egui::pos2(c.x - 3.5, c.y)],
                    stroke,
                );
            }
            ChromeIcon::DockFloat => {
                // `◫`: floating window outline with top bar.
                let inner = egui::Rect::from_center_size(c, egui::vec2(10.0, 9.0));
                painter.rect_stroke(
                    inner,
                    egui::CornerRadius::same(1),
                    stroke,
                    egui::StrokeKind::Inside,
                );
                painter.line_segment(
                    [
                        egui::pos2(inner.left(), inner.top() + 2.8),
                        egui::pos2(inner.right(), inner.top() + 2.8),
                    ],
                    stroke,
                );
            }
            ChromeIcon::DockRight => {
                // `⇥`: right-pointing arrow + vertical edge bar on the right.
                painter.line_segment(
                    [egui::pos2(c.x + 5.0, c.y - 4.5), egui::pos2(c.x + 5.0, c.y + 4.5)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 3.0, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x + 0.5, c.y - 3.0), egui::pos2(c.x + 3.5, c.y)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x + 0.5, c.y + 3.0), egui::pos2(c.x + 3.5, c.y)],
                    stroke,
                );
            }
            ChromeIcon::Close => {
                // `×`
                painter.line_segment(
                    [egui::pos2(c.x - 3.8, c.y - 3.8), egui::pos2(c.x + 3.8, c.y + 3.8)],
                    stroke,
                );
                painter.line_segment(
                    [egui::pos2(c.x + 3.8, c.y - 3.8), egui::pos2(c.x - 3.8, c.y + 3.8)],
                    stroke,
                );
            }
            ChromeIcon::Reset => {
                // `↺`: counter-clockwise arc + arrowhead at upper-left.
                let r = 4.2_f32;
                let steps = 12usize;
                let start_angle = -std::f32::consts::FRAC_PI_2;
                let end_angle = std::f32::consts::PI * 1.15;
                let mut prev = egui::pos2(c.x + r * start_angle.cos(), c.y + r * start_angle.sin());
                for step in 1..=steps {
                    let t = step as f32 / steps as f32;
                    let a = start_angle + t * (end_angle - start_angle);
                    let pt = egui::pos2(c.x + r * a.cos(), c.y + r * a.sin());
                    painter.line_segment([prev, pt], stroke);
                    prev = pt;
                }
                let tip = egui::pos2(c.x, c.y - r);
                painter.line_segment([tip, egui::pos2(tip.x + 2.6, tip.y - 2.0)], stroke);
                painter.line_segment([tip, egui::pos2(tip.x + 2.6, tip.y + 2.0)], stroke);
            }
        }
    }
    resp
}

/// Render the four panel header buttons (`left`, `float`, `right`, `close`; `DockButtons` in `Panel.jsx`).
///
/// Placed right-to-left (`×`, `⇥`, `◫`, `⇤`) so they stay anchored to the right edge of the header
/// while the remaining left span acts as the title / drag handle.
pub(crate) fn dock_buttons_rtl(
    ui: &mut egui::Ui,
    verb: &str,
    side: PanelSide,
    close_title: &str,
    out: &mut Vec<String>,
) {
    if icon_btn(ui, (verb, "close"), ChromeIcon::Close, false, close_title).clicked() {
        out.push(format!("{verb} close"));
    }
    if icon_btn(
        ui,
        (verb, "right"),
        ChromeIcon::DockRight,
        side == PanelSide::Right,
        "stick to the right edge",
    )
    .clicked()
    {
        out.push(format!("{verb} right"));
    }
    if icon_btn(
        ui,
        (verb, "float"),
        ChromeIcon::DockFloat,
        side == PanelSide::Float,
        "float",
    )
    .clicked()
    {
        out.push(format!("{verb} float"));
    }
    if icon_btn(
        ui,
        (verb, "left"),
        ChromeIcon::DockLeft,
        side == PanelSide::Left,
        "stick to the left edge",
    )
    .clicked()
    {
        out.push(format!("{verb} left"));
    }
}

/// Build an [`egui::text::LayoutJob`] highlighting `hits` in underlined amber (`Marked` in `SettingsPanel.jsx`).
pub(crate) fn marked_layout_job(
    text: &str,
    hits: &[HitRange],
    font_id: egui::FontId,
    base_color: egui::Color32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let plain_fmt = egui::TextFormat {
        font_id: font_id.clone(),
        color: base_color,
        ..Default::default()
    };
    if hits.is_empty() {
        job.append(text, 0.0, plain_fmt);
        return job;
    }
    let hit_fmt = egui::TextFormat {
        font_id,
        color: AMBER,
        underline: egui::Stroke::new(1.0, AMBER),
        ..Default::default()
    };
    let mut at = 0usize;
    for &[raw_a, raw_b] in hits {
        let a = raw_a.min(text.len());
        let b = raw_b.min(text.len());
        if a >= b || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            continue;
        }
        if a > at && text.is_char_boundary(at) {
            job.append(&text[at..a], 0.0, plain_fmt.clone());
        }
        job.append(&text[a..b], 0.0, hit_fmt.clone());
        at = b;
    }
    if at < text.len() && text.is_char_boundary(at) {
        job.append(&text[at..], 0.0, plain_fmt);
    }
    job
}

/// Render an arbitrary egui overlay closure offscreen onto a dark wall backdrop and write it to a PNG.
///
/// Runs two egui passes so `Area` and `ScrollArea` bounds settle before tessellation, then composites
/// through `egui_wgpu::Renderer` on a `Rgba8UnormSrgb` offscreen texture.
pub fn render_panel_to_png(
    gpu: &crate::gpu::GpuContext,
    viewport: (u32, u32),
    pixels_per_point: f32,
    path: &Path,
    mut build_ui: impl FnMut(&egui::Context),
) {
    let (width, height) = (viewport.0.max(1), viewport.1.max(1));
    let device = &gpu.device;
    let queue = &gpu.queue;
    // Use `Rgba8Unorm` so `egui_wgpu` selects its gamma-space framebuffer pipeline and writes
    // exact 0..255 sRGB colour values directly into the readback buffer without linear thinning.
    let format = wgpu::TextureFormat::Rgba8Unorm;

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
    let view = texture.create_view(&Default::default());

    let egui_ctx = egui::Context::default();
    let mut egui_renderer =
        egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default());

    let logical_size = egui::vec2(
        width as f32 / pixels_per_point,
        height as f32 / pixels_per_point,
    );
    let make_input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical_size)),
        viewports: std::iter::once((
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(pixels_per_point),
                inner_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical_size)),
                ..Default::default()
            },
        ))
        .collect(),
        ..Default::default()
    };

    // Warm-up passes size dynamic floating Areas and measured content widths; final pass tessellates.
    let mut deferred_free = Vec::new();
    for _ in 0..2 {
        let warm = egui_ctx.run_ui(make_input(), |root_ui| {
            build_ui(root_ui.ctx());
        });
        for (id, image_deltas) in &warm.textures_delta.set {
            for image_delta in image_deltas {
                egui_renderer.update_texture(device, queue, *id, image_delta);
            }
        }
        deferred_free.extend(warm.textures_delta.free.iter().copied());
    }
    let pass2 = egui_ctx.run_ui(make_input(), |root_ui| {
        build_ui(root_ui.ctx());
    });
    for (id, image_deltas) in &pass2.textures_delta.set {
        for image_delta in image_deltas {
            egui_renderer.update_texture(device, queue, *id, image_delta);
        }
    }
    deferred_free.extend(pass2.textures_delta.free.iter().copied());

    let clipped = egui_ctx.tessellate(pass2.shapes, pass2.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point,
    };

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("wall panel offscreen encoder"),
    });
    let user_cmds = egui_renderer.update_buffers(device, queue, &mut encoder, &clipped, &screen);
    if !user_cmds.is_empty() {
        queue.submit(user_cmds);
    }
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("wall panel egui pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.055,
                        g: 0.063,
                        b: 0.086,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        egui_renderer.render(&mut pass.forget_lifetime(), &clipped, &screen);
    }

    let unpadded_bpr = width * 4;
    let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wall panel readback"),
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

    for id in &deferred_free {
        egui_renderer.free_texture(id);
    }

    let slice = readback.slice(..);
    glyph_field::map_read(
        device,
        &slice,
        crate::config::settings().gpu.wait_limit(),
        "wall panel offscreen readback",
    );
    let data = slice.get_mapped_range().expect("panel readback not mapped");
    let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
    for row in 0..height {
        let start = (row * padded_bpr) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
    }
    drop(data);
    readback.unmap();

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create panel screenshot directory");
        }
    }
    image::save_buffer(path, &pixels, width, height, image::ColorType::Rgba8)
        .expect("failed to write panel PNG");
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wall_core::geom::Point;
    use wall_core::input::Platform;
    use wall_core::status::TermdStatus;
    use wall_core::store::{
        PanelSide, SettingsPanelState as SettingsPlacement, StatusWindowState as StatusPlacement,
    };

    use super::settings::{self, SettingsInput, SettingsState};
    use super::status::{self, StatusInput, StatusState};
    use super::*;

    #[test]
    fn renders_settings_and_status_panels_offscreen_to_png() {
        let probe_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let has_adapter = pollster::block_on(probe_instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            },
        ))
        .is_ok();
        if !has_adapter {
            // Headless sandboxes that block Metal/Vulkan IOKit access cannot open a GPU adapter;
            // real hosts and `cargo glyph test` run with an adapter and exercise full readback.
            return;
        }

        let out_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/glyph-scratch/wall-panels-proof.png");

        let mut settings_state = SettingsState::default();
        settings_state.query = "grid".to_string();
        settings_state.active = 1;
        settings_state.record_reply("gridMinor", "ERR: gridMinor takes a number > 0, not '0'");

        let mut settings_input = SettingsInput {
            placement: SettingsPlacement {
                open: true,
                side: PanelSide::Right,
                float: None,
                collapsed: Vec::new(),
                ..SettingsPlacement::default()
            },
            platform: Platform::Mac,
            dock_rect: Some(egui::Rect::from_min_size(
                egui::pos2(900.0, 0.0),
                egui::vec2(380.0, 820.0),
            )),
            ..SettingsInput::default()
        };
        settings_input.dialled.insert("gridShow".to_string());
        settings_input.dialled.insert("gridMajorColor".to_string());

        let mut termd = TermdStatus::new("rs-wall");
        termd.set_bundled_version("g3d-termd/0.3.0");
        termd.on_welcome("8c1f0e7a9b2d4c55", &["key", "paste", "mouse"], 100);
        termd
            .on_status_json(
                json!({
                    "ev": "status",
                    "at": 1_794_519_127_407_u64,
                    "daemon": {
                        "version": "g3d-termd/0.2.0",
                        "engine": "glyph-vt/0.1.0",
                        "ns": "rs-wall",
                        "started_at": 1_794_518_713_751_u64,
                        "holder": {"pid": 41190, "rss": 3_145_728},
                        "brain": {
                            "pid": 41200, "generation": 2, "rss": 41_943_040,
                            "restarts": {"crash": 1, "watchdog": 0, "upgrade": 0},
                            "last_start": {"reason": "crash", "at": 1_794_519_100_000_u64}
                        },
                        "env": {"warnings": ["G3D_TERMD_DIR overridden"]},
                        "leftovers": [{
                            "id": "t3", "pid": 41260, "since": 1_794_519_000_000_u64,
                            "processes": [{"pid": 41266, "comm": "sleep"}]
                        }]
                    },
                    "terminals": [
                        {
                            "id": "t1", "handle": 7, "pid": 41234, "argv": ["/bin/-zsh"], "login": true,
                            "cwd": "/srv/src", "title": "~/src/app", "cols": 120, "rows": 40,
                            "alt": true, "out_bps": 1_253_000, "flow": "blocked", "input_queued": 2048,
                            "processes": 3,
                            "subscribers": [
                                {"conn": 12, "client": "glyph3d-native/0.1", "role": "own", "mode": "raw", "flow": "block", "lagging": false},
                                {"conn": 15, "client": "g3d/0.2", "role": "watch", "mode": "raw", "flow": "lossy", "lagging": true}
                            ]
                        },
                        {
                            "id": "t2", "handle": 8, "pid": 41299, "argv": ["/usr/bin/cargo"],
                            "cwd": "/srv", "title": "cargo test", "cols": 100, "rows": 32,
                            "out_bps": 0, "last_output_at": 1_794_519_082_407_u64
                        }
                    ],
                    "errors": [{
                        "code": "brain_crash",
                        "message": "brain exited on SIGSEGV",
                        "first_at": 1_794_519_099_900_u64,
                        "at": 1_794_519_099_900_u64,
                        "count": 2,
                        "id": "t1"
                    }]
                }),
                1_794_519_127_407,
            )
            .unwrap();
        // Trigger `restarting` past 400 ms so the amber under-topbar notice renders alongside the stale table!
        termd.on_disconnected(false, 1_794_519_128_000);

        let mut status_state = StatusState::default();
        let mut status_input = StatusInput::new(
            StatusPlacement {
                open: true,
                side: PanelSide::Float,
                float: Some(Point::new(24.0, 96.0)),
                ..StatusPlacement::default()
            },
            termd,
            1_794_519_129_000,
        );
        status_input.platform = Platform::Mac;
        status_input.chrome_inset = (12.0, 392.0);
        status_input
            .wall_names
            .insert("t1".to_string(), "main shell".to_string());

        let gpu = crate::gpu::test_device::context();
        render_panel_to_png(&gpu, (1280, 820), 1.0, &out_path, |ctx| {
            let mut verbs = Vec::new();
            status::show(ctx, &mut status_state, &status_input, &mut verbs);
            settings::show(ctx, &mut settings_state, &settings_input, &mut verbs);
        });

        let meta = std::fs::metadata(&out_path).expect("rendered proof PNG must exist");
        assert!(meta.len() > 4_096, "rendered PNG unexpectedly small: {} B", meta.len());
    }
}
