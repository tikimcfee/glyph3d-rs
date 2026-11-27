//! egui host for the terminal wall's flat panels (`native/src/wall/panels/`).
//!
//! Modelled on `native/src/windowed/{ui,app,state}.rs` and built for an
//! on-demand window: when no panel draws anything, [`WallUi`] skips the GPU
//! pass, drops repaint requests, and short-circuits window events at O(1) cost.
//!
//! ## Three-call lifecycle
//! 1. [`WallUi::on_window_event`] — feed winit events to egui first and learn
//!    whether egui consumed the event, wants the keyboard or pointer, or wants
//!    a redraw.
//! 2. [`WallUi::run`] — run one egui UI pass over a closure taking `&egui::Context`
//!    (matching `out/wall-port/briefs/P-panels.md`'s panel contract) and get back
//!    a [`UiFrame`] with optional [`PaintedUi`], focus flags, and `repaint_after`.
//! 3. [`WallUi::paint`] — paint a [`PaintedUi`] over the wall's scene into the
//!    same surface `TextureView` (`LoadOp::Load`, no depth attachment).
//!
//! ## Colour space on sRGB targets
//! The wall's surface (and offscreen screenshot target) is sRGB
//! (`Bgra8UnormSrgb` / `Rgba8UnormSrgb`). [`egui_wgpu::Renderer`] is created
//! with that exact surface format, which selects egui-wgpu's
//! `fs_main_linear_framebuffer` fragment shader: `egui::Color32` sRGB inputs
//! are decoded to linear in the shader and hardware-encoded back to sRGB on
//! store, preserving exact dark panel hues without touching the wall's colour
//! pipeline. Alpha blending therefore occurs in linear space (the same accepted
//! trade-off as `native/src/windowed/app.rs`), so panel fills stay nearly opaque.

use std::path::PathBuf;
use std::time::Duration;

use glam::Vec2;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::config::WallSettings;

use super::renderer::UploadStats;

/// Threshold above which egui's `ViewportOutput::repaint_delay` means "idle, do not repaint".
const IDLE_REPAINT_THRESHOLD: Duration = Duration::from_secs(60 * 60 * 24);

fn extract_repaint_after(
    has_shapes: bool,
    viewport_output: &std::collections::BTreeMap<egui::ViewportId, egui::ViewportOutput>,
) -> Option<Duration> {
    if !has_shapes {
        return None;
    }
    viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|v| v.repaint_delay)
        .filter(|d| *d < IDLE_REPAINT_THRESHOLD)
}

/// Outcome of feeding one winit [`WindowEvent`] to [`WallUi::on_window_event`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiEventOutcome {
    /// True when egui consumed this event (do not forward to the wall canvas or terminals).
    pub consumed: bool,
    /// True when egui wants keyboard input this frame (focused input or modal panel).
    pub wants_keyboard: bool,
    /// True when egui wants pointer input this frame (hovering a panel or mid-drag).
    pub wants_pointer: bool,
    /// True when egui needs a redraw in response to this event.
    pub repaint: bool,
}

/// Prepared tessellated UI ready for [`WallUi::paint`].
///
/// Always pass back to [`WallUi::paint`] (or drop only after draining
/// `textures_delta`); `WallUi::paint` drains `textures_delta` on the first call
/// so the same `PaintedUi` may safely be painted a second time (e.g. into a
/// `GLYPH_WALL_SHOT` capture target).
pub struct PaintedUi {
    clipped: Vec<egui::ClippedPrimitive>,
    textures_delta: egui::TexturesDelta,
    screen: egui_wgpu::ScreenDescriptor,
}

impl PaintedUi {
    /// Number of clipped draw primitives in this frame.
    pub fn primitive_count(&self) -> usize {
        self.clipped.len()
    }
}

/// Output of one UI frame from [`WallUi::run`].
pub struct UiFrame {
    /// Prepared GPU payload for [`WallUi::paint`], or `None` when nothing drew
    /// and no texture updates are pending (zero GPU pass).
    pub painted: Option<PaintedUi>,
    /// True when egui wants keyboard input after laying out this frame.
    pub wants_keyboard: bool,
    /// True when egui wants pointer input after laying out this frame.
    pub wants_pointer: bool,
    /// Requested delay until the next repaint (`Some(Duration::ZERO)` = next
    /// frame immediately; `Some(d)` = wake after `d`; `None` = sleep until input).
    pub repaint_after: Option<Duration>,
}

/// egui host for the wall window: context, winit event bridge, and wgpu painter.
pub struct WallUi {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Whether the most recent `run` drew any shapes (when false, window events
    /// short-circuit with zero repaint requests and zero consumption).
    active: bool,
    /// Texture ids scheduled for deletion after the previous frame's `queue.submit`.
    pending_free: Vec<egui::TextureId>,
}

impl WallUi {
    /// Construct a styled [`WallUi`] targeting `surface_format` (typically sRGB).
    pub fn new(
        device: &wgpu::Device,
        window: &Window,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);

        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        // Pass the surface's actual sRGB format so egui-wgpu picks its
        // linear-framebuffer fragment shader and paints directly onto the
        // wall's surface view without changing the wall's colour encoding.
        let renderer = egui_wgpu::Renderer::new(
            device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );
        Self {
            ctx,
            state,
            renderer,
            active: false,
            pending_free: Vec::new(),
        }
    }

    /// True if the previous UI frame drew any panels (so callers can skip
    /// [`Self::run`] altogether once all panels are closed and cleared).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Access the underlying egui context (e.g. for tests or external styling).
    pub fn context(&self) -> &egui::Context {
        &self.ctx
    }

    /// Surrender focus from any currently focused egui widget (e.g. when the user
    /// clicks on the wall canvas or a global navigation chord blurs panel search).
    pub fn surrender_focus(&self) {
        if let Some(id) = self.ctx.memory(|m| m.focused()) {
            self.ctx.memory_mut(|m| m.surrender_focus(id));
        }
    }

    /// The system clipboard's text, through the window's own clipboard
    /// (`smithay-clipboard` on Wayland, where a fresh `arboard` speaks only
    /// X11 and fails; `arboard` elsewhere).
    pub fn clipboard_text(&mut self) -> Option<String> {
        self.state.clipboard_text()
    }

    /// Put text on the system clipboard, through the same door.
    pub fn set_clipboard_text(&mut self, text: String) {
        self.state.set_clipboard_text(text);
    }

    /// **Call 1 of 3:** feed a winit [`WindowEvent`] to egui first.
    ///
    /// When no panel was open on the last frame (`!self.is_active()`), this
    /// short-circuits without pushing pointer/key events or requesting repaints
    /// (forwarding only `ScaleFactorChanged` so DPI changes are never lost).
    pub fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> UiEventOutcome {
        if !self.active {
            if matches!(event, WindowEvent::ScaleFactorChanged { .. }) {
                let _ = self.state.on_window_event(window, event);
            }
            return UiEventOutcome::default();
        }
        let resp = self.state.on_window_event(window, event);
        UiEventOutcome {
            consumed: resp.consumed,
            wants_keyboard: self.ctx.egui_wants_keyboard_input(),
            wants_pointer: self.ctx.egui_wants_pointer_input(),
            repaint: resp.repaint,
        }
    }

    /// Feed a synthetic [`egui::Event`] (from `wallctl` `app.move` / `app.click`)
    /// into the next UI frame when any panel is active.
    pub fn push_egui_event(&mut self, event: egui::Event) -> bool {
        if !self.active {
            return false;
        }
        self.state.egui_input_mut().events.push(event);
        true
    }

    /// **Call 2 of 3:** run one frame of UI given a closure over `&egui::Context`.
    ///
    /// Returns `painted: None` and `repaint_after: None` when the closure emits
    /// no shapes and no textures changed, so the caller pays no GPU pass and
    /// can sleep in `ControlFlow::Wait`.
    pub fn run(
        &mut self,
        window: &Window,
        viewport: (u32, u32),
        mut build_ui: impl FnMut(&egui::Context),
    ) -> UiFrame {
        let raw_input = self.state.take_egui_input(window);
        let ctx = self.ctx.clone();
        let full_output = ctx.run_ui(raw_input, |root_ui| {
            build_ui(root_ui.ctx());
        });

        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        self.state.handle_platform_output(window, platform_output);
        if !self.state.allow_ime() {
            // Keep window IME enabled for terminals even after an egui `TextEdit`
            // surrenders focus and `egui-winit` toggles `set_ime_allowed(false)`.
            window.set_ime_allowed(true);
        }

        let has_shapes = !shapes.is_empty();
        self.active = has_shapes;

        let repaint_after = extract_repaint_after(has_shapes, &viewport_output);

        let wants_keyboard = has_shapes && self.ctx.egui_wants_keyboard_input();
        let wants_pointer = has_shapes && self.ctx.egui_wants_pointer_input();

        let clipped = if has_shapes {
            self.ctx.tessellate(shapes, pixels_per_point)
        } else {
            Vec::new()
        };

        let needs_gpu = !clipped.is_empty()
            || !textures_delta.set.is_empty()
            || !textures_delta.free.is_empty()
            || !self.pending_free.is_empty();

        let painted = if needs_gpu {
            Some(PaintedUi {
                clipped,
                textures_delta,
                screen: egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [viewport.0.max(1), viewport.1.max(1)],
                    pixels_per_point,
                },
            })
        } else {
            // `textures_delta` is empty here, so dropping it cannot assert.
            None
        };

        UiFrame {
            painted,
            wants_keyboard,
            wants_pointer,
            repaint_after,
        }
    }

    /// **Call 3 of 3:** paint a prepared [`PaintedUi`] over the wall's scene into
    /// `color_view` (`LoadOp::Load`, no depth attachment).
    ///
    /// Skips the render pass completely when `painted.clipped` is empty (still
    /// applying/deferring any texture deltas cleanly). Safe to call a second time
    /// on the same `PaintedUi` (e.g. into a `GLYPH_WALL_SHOT` offscreen target).
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        painted: &mut PaintedUi,
    ) {
        // Free textures deferred from the previous submitted frame.
        for id in self.pending_free.drain(..) {
            self.renderer.free_texture(&id);
        }

        for (id, image_deltas) in painted.textures_delta.set.drain() {
            for image_delta in image_deltas {
                self.renderer.update_texture(device, queue, id, &image_delta);
            }
        }
        self.pending_free.extend(painted.textures_delta.free.drain());

        if painted.clipped.is_empty() {
            return;
        }

        let user_cmds = self.renderer.update_buffers(
            device,
            queue,
            encoder,
            &painted.clipped,
            &painted.screen,
        );
        if !user_cmds.is_empty() {
            queue.submit(user_cmds);
        }

        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("wall egui pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
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
        self.renderer
            .render(&mut pass.forget_lifetime(), &painted.clipped, &painted.screen);
    }
}

/// Convert normalized sRGB `[r, g, b, a]` (`0.0..=1.0`, from `[wall]` config) into [`egui::Color32`].
pub fn color32_from_srgb(rgba: [f32; 4]) -> egui::Color32 {
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    egui::Color32::from_rgba_unmultiplied(ch(rgba[0]), ch(rgba[1]), ch(rgba[2]), ch(rgba[3]))
}

/// Directory holding the bundled UI and fallback fonts (`Contents/Resources/fonts`
/// inside a macOS `.app` bundle, `<prefix>/share/glyph3d-wall/fonts` on Linux, else
/// `<repo>/assets/fonts`).
fn wall_fonts_dir() -> PathBuf {
    super::glyphs::default_fonts_dir()
}

/// Path to the bundled monospace UI font (`fonts/Cousine-Regular.ttf`), resolved
/// through the same bundle -> repo lookup order as [`super::glyphs::default_fonts_dir`].
pub fn wall_ui_font_path() -> PathBuf {
    wall_fonts_dir().join("Cousine-Regular.ttf")
}

/// Apply the wall's dark visual theme (sourced from `config/defaults.toml [wall]`)
/// and install the monospace UI font (`assets/fonts/Cousine-Regular.ttf`) plus
/// the symbol-complete fallback faces (`MesloLGS-NF-Mono.ttf`, `DejaVuSans.ttf`)
/// from `glyph_font::default_font_chain` once onto `ctx`.
/// Also usable in headless panel tests (`egui::Context::default()`).
pub fn configure_wall_context(ctx: &egui::Context, wall: &WallSettings) {
    install_wall_fonts(ctx);
    apply_wall_style(ctx, wall);
}

fn install_wall_fonts(ctx: &egui::Context) {
    let dir = wall_fonts_dir();
    // Mirrors `glyph_font::default_font_chain`: Cousine primary for crisp ASCII
    // metrics, backed by MesloLGS NF Mono and DejaVu Sans so keycap/modifier
    // glyphs (`⌘`, `⌥`, `⌃`, `⇧`, `↵`, `⌫`, `⇤`, `⇥`, `◫`, `▾`, `▸`) never draw tofu.
    let chain = [
        ("wall_mono", dir.join("Cousine-Regular.ttf")),
        ("wall_meslo", dir.join("MesloLGS-NF-Mono.ttf")),
        ("wall_dejavu", dir.join("DejaVuSans.ttf")),
    ];
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = Vec::new();
    for (name, path) in chain {
        match std::fs::read(&path) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
                loaded.push(name.to_owned());
            }
            Err(e) => {
                log::warn!(
                    "wall ui: could not read font {}: {e}; skipping",
                    path.display()
                );
            }
        }
    }
    if loaded.is_empty() {
        return;
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for (idx, name) in loaded.iter().enumerate() {
            list.insert(idx, name.clone());
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_wall_style(ctx: &egui::Context, wall: &WallSettings) {
    let bg_deep = color32_from_srgb([
        wall.paper_background[0],
        wall.paper_background[1],
        wall.paper_background[2],
        1.0,
    ]);
    // Opaque panel fill: on the wall's sRGB surface, hardware alpha blending runs
    // in linear space, where even 4% alpha lets bright terminal ink visibly ghost
    // through a dark panel (same reason `[wall] panel_opacity` defaults to 1.00).
    let bg_panel = color32_from_srgb([
        wall.clear_color[0],
        wall.clear_color[1],
        wall.clear_color[2],
        1.0,
    ]);
    let bg_faint = color32_from_srgb([
        wall.paper_minor_color[0],
        wall.paper_minor_color[1],
        wall.paper_minor_color[2],
        1.0,
    ]);
    let bg_widget = color32_from_srgb([
        wall.paper_major_color[0],
        wall.paper_major_color[1],
        wall.paper_major_color[2],
        1.0,
    ]);
    let accent = color32_from_srgb([
        wall.border_focused_color[0],
        wall.border_focused_color[1],
        wall.border_focused_color[2],
        1.0,
    ]);
    let border_idle = color32_from_srgb([
        wall.border_unfocused_color[0],
        wall.border_unfocused_color[1],
        wall.border_unfocused_color[2],
        1.0,
    ]);
    let accent_fill = egui::Color32::from_rgb(0x16, 0x40, 0x2c);
    let hover_fill = egui::Color32::from_rgb(0x22, 0x29, 0x38);
    let text_primary = egui::Color32::from_rgb(0xd8, 0xde, 0xe9);
    let text_bright = egui::Color32::from_rgb(0xee, 0xf2, 0xf8);

    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(text_primary);
    visuals.panel_fill = bg_panel;
    visuals.window_fill = bg_panel;
    visuals.window_stroke = egui::Stroke::new(1.0, border_idle);
    visuals.window_corner_radius = egui::CornerRadius::same(4);
    visuals.menu_corner_radius = egui::CornerRadius::same(4);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 10,
        spread: 0,
        color: egui::Color32::from_black_alpha(90),
    };
    visuals.faint_bg_color = bg_faint;
    visuals.extreme_bg_color = bg_deep;
    visuals.code_bg_color = bg_widget;
    visuals.hyperlink_color = accent;
    visuals.selection.bg_fill = accent_fill;
    visuals.selection.stroke = egui::Stroke::new(1.0, accent);

    visuals.widgets.noninteractive.bg_fill = bg_panel;
    visuals.widgets.noninteractive.weak_bg_fill = bg_panel;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, border_idle);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_primary);

    visuals.widgets.inactive.bg_fill = bg_widget;
    visuals.widgets.inactive.weak_bg_fill = bg_faint;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, border_idle);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text_primary);

    visuals.widgets.hovered.bg_fill = hover_fill;
    visuals.widgets.hovered.weak_bg_fill = hover_fill;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text_bright);

    visuals.widgets.active.bg_fill = accent_fill;
    visuals.widgets.active.weak_bg_fill = accent_fill;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, accent);

    visuals.widgets.open.bg_fill = hover_fill;
    visuals.widgets.open.weak_bg_fill = hover_fill;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, text_bright);

    ctx.set_visuals(visuals);
}

/// Snapshot of one placed terminal for [`show_debug_hud`].
#[derive(Clone, Debug)]
pub struct HudTerminalInfo<'a> {
    pub id: u64,
    pub title: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub scale: f32,
    pub focused: bool,
}

/// Plain-data snapshot rendered by the built-in debug HUD ([`show_debug_hud`]).
#[derive(Clone, Debug)]
pub struct DebugHudInput<'a> {
    pub camera_zoom: f32,
    pub camera_center: Vec2,
    pub viewport: (u32, u32),
    pub stats: UploadStats,
    pub terminals: &'a [HudTerminalInfo<'a>],
}

/// Built-in wall debug HUD panel (`F1` toggles; `GLYPH_WALL_HUD=1` opens at startup).
///
/// Lists camera zoom/viewport, this frame's [`UploadStats`] from [`super::WallRenderer`],
/// and every placed terminal's id, size, scale, and title.
pub fn show_debug_hud(ctx: &egui::Context, open: &mut bool, input: &DebugHudInput<'_>) {
    if !*open {
        return;
    }
    let wall = &crate::config::settings().wall;
    let accent = color32_from_srgb([
        wall.border_focused_color[0],
        wall.border_focused_color[1],
        wall.border_focused_color[2],
        1.0,
    ]);
    let muted = egui::Color32::from_rgb(0x8b, 0x95, 0xa7);

    egui::Window::new("Wall Debug HUD")
        .id(egui::Id::new("wall_debug_hud"))
        .open(open)
        .default_pos([16.0, 16.0])
        .resizable(false)
        .collapsible(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("zoom").color(muted));
                ui.label(
                    egui::RichText::new(format!("{:.2} px/u", input.camera_zoom))
                        .color(accent)
                        .strong(),
                );
                ui.separator();
                ui.label(egui::RichText::new("center").color(muted));
                ui.label(format!(
                    "({:.1}, {:.1})",
                    input.camera_center.x, input.camera_center.y
                ));
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("{}x{}", input.viewport.0, input.viewport.1))
                        .color(muted),
                );
            });

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("uploads").color(muted));
                ui.label(format!(
                    "drawn {} | uploaded {} | rows {} | writes {}",
                    input.stats.terminals_drawn,
                    input.stats.terminals_uploaded,
                    input.stats.rows_uploaded,
                    input.stats.write_calls,
                ));
            });

            ui.separator();

            if input.terminals.is_empty() {
                ui.label(egui::RichText::new("(no terminals)").color(muted));
            } else {
                egui::Grid::new("wall_debug_hud_terminals")
                    .num_columns(4)
                    .spacing([14.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("id").color(muted));
                        ui.label(egui::RichText::new("size").color(muted));
                        ui.label(egui::RichText::new("scale").color(muted));
                        ui.label(egui::RichText::new("title").color(muted));
                        ui.end_row();

                        for t in input.terminals {
                            let id_label = if t.focused {
                                egui::RichText::new(format!("*{}", t.id))
                                    .color(accent)
                                    .strong()
                            } else {
                                egui::RichText::new(format!(" {}", t.id))
                            };
                            ui.label(id_label);
                            ui.label(format!("{}x{}", t.cols, t.rows));
                            ui.label(format!("{:.2}x", t.scale));
                            let title = if t.title.is_empty() {
                                egui::RichText::new("(untitled)").color(muted)
                            } else if t.focused {
                                egui::RichText::new(t.title).color(accent)
                            } else {
                                egui::RichText::new(t.title)
                            };
                            ui.label(title);
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("F1: toggle HUD")
                    .color(muted)
                    .small(),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_debug_hud_lays_out_and_produces_zero_shapes_when_closed() {
        let ctx = egui::Context::default();
        configure_wall_context(&ctx, &crate::config::settings().wall);

        let terms = [
            HudTerminalInfo {
                id: 1,
                title: "shell-1",
                cols: 80,
                rows: 24,
                scale: 1.0,
                focused: true,
            },
            HudTerminalInfo {
                id: 2,
                title: "",
                cols: 60,
                rows: 18,
                scale: 0.5,
                focused: false,
            },
        ];
        let input = DebugHudInput {
            camera_zoom: 24.5,
            camera_center: Vec2::new(10.0, -5.0),
            viewport: (1600, 1000),
            stats: UploadStats {
                terminals_drawn: 2,
                terminals_uploaded: 1,
                rows_uploaded: 3,
                write_calls: 1,
            },
            terminals: &terms,
        };

        // Closed HUD emits zero shapes and surfaces no repaint request.
        let mut closed = false;
        let out_closed = ctx.run_ui(egui::RawInput::default(), |ui| {
            show_debug_hud(ui.ctx(), &mut closed, &input);
        });
        assert!(out_closed.shapes.is_empty(), "closed HUD must emit zero shapes");
        assert_eq!(
            extract_repaint_after(!out_closed.shapes.is_empty(), &out_closed.viewport_output),
            None,
            "closed HUD must never request a repaint"
        );

        // Open HUD emits shapes, finishes its open animation, settles to idle, and keeps `open == true`.
        let mut open = true;
        let make_raw = |time_secs: f64| egui::RawInput {
            time: Some(time_secs),
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1600.0, 1000.0),
            )),
            ..Default::default()
        };
        let mut last_out = None;
        for step in 0..4 {
            last_out = Some(ctx.run_ui(make_raw(f64::from(step) * 0.5), |ui| {
                show_debug_hud(ui.ctx(), &mut open, &input);
            }));
        }
        let settled = last_out.expect("ran at least once");
        assert!(open);
        assert!(!settled.shapes.is_empty(), "open HUD must emit UI shapes");
        assert_eq!(
            extract_repaint_after(true, &settled.viewport_output),
            None,
            "settled static HUD must sleep idle without repainting"
        );

        // A panel requesting a timed repaint (e.g. blinking caret) surfaces `Some(delay)`.
        let out_blink = ctx.run_ui(make_raw(2.5), |ui| {
            show_debug_hud(ui.ctx(), &mut open, &input);
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        });
        let blink_delay = extract_repaint_after(!out_blink.shapes.is_empty(), &out_blink.viewport_output)
            .expect("timed repaint must surface");
        assert!(blink_delay <= Duration::from_millis(250), "delay={blink_delay:?}");
    }

    #[test]
    fn wall_font_file_exists_and_color_conversion_is_exact() {
        assert!(
            wall_ui_font_path().is_file(),
            "expected monospace UI font at {}",
            wall_ui_font_path().display()
        );
        let c = color32_from_srgb([0.431, 0.906, 0.627, 1.0]);
        assert_eq!(c, egui::Color32::from_rgb(110, 231, 160));
    }

    /// Verifies egui-wgpu renders an opaque dark `egui::Color32` accurately onto
    /// the wall's sRGB target (`Rgba8UnormSrgb`) without double-gamma washout.
    #[test]
    fn gpu_egui_srgb_target_preserves_dark_panel_color() {
        let ctx = crate::gpu::test_device::context();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut egui_renderer = egui_wgpu::Renderer::new(
            &ctx.device,
            format,
            egui_wgpu::RendererOptions::default(),
        );
        let egui_ctx = egui::Context::default();
        configure_wall_context(&egui_ctx, &crate::config::settings().wall);

        let w = 128u32;
        let h = 128u32;
        let target_col = egui::Color32::from_rgb(0x16, 0x40, 0x2c);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(w as f32, h as f32),
            )),
            ..Default::default()
        };
        let mut full = egui_ctx.run_ui(raw, |ui| {
            let rect = egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(w as f32, h as f32),
            );
            ui.ctx()
                .layer_painter(egui::LayerId::background())
                .rect_filled(rect, 0.0, target_col);
        });
        let clipped = egui_ctx.tessellate(full.shapes, full.pixels_per_point);
        assert!(!clipped.is_empty(), "sRGB probe must emit at least one clipped primitive");
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [w, h],
            pixels_per_point: full.pixels_per_point,
        };

        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wall ui srgb test target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let unpadded_bpr = w * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall ui srgb readback"),
            size: u64::from(padded_bpr * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        for (id, deltas) in full.textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(&ctx.device, &ctx.queue, id, &delta);
            }
        }
        let mut encoder = ctx.device.create_command_encoder(&Default::default());
        let user_cmds = egui_renderer.update_buffers(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            &clipped,
            &screen,
        );
        if !user_cmds.is_empty() {
            ctx.queue.submit(user_cmds);
        }
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wall ui srgb test pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            egui_renderer.render(&mut pass.forget_lifetime(), &clipped, &screen);
        }
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
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        ctx.queue.submit([encoder.finish()]);
        for id in full.textures_delta.free.drain() {
            egui_renderer.free_texture(&id);
        }

        let slice = readback.slice(..);
        glyph_field::map_read(
            &ctx.device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "wall ui srgb readback",
        );
        let data = slice.get_mapped_range().expect("mapped");
        let idx = ((h / 2) * padded_bpr + (w / 2) * 4) as usize;
        let got = [data[idx], data[idx + 1], data[idx + 2]];
        drop(data);
        readback.unmap();

        assert!(
            got[0].abs_diff(0x16) <= 1 && got[1].abs_diff(0x40) <= 1 && got[2].abs_diff(0x2c) <= 1,
            "expected sRGB #16402c (22, 64, 44) on sRGB target, got {got:?}"
        );
    }
}
