//! Flat egui chrome panels drawn over the terminal wall (`out/wall-port/briefs/P-panels.md`).
//!
//! Each panel is a pure `show(ctx, state, input, out) -> bool` function backed by
//! `wall_core` models; every mutation leaves through `out` as a wall verb line.

pub mod context_menu;
pub mod help;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use wall_core::dock::{DockPanel, DockSide, DockState, dock_layout};
    use wall_core::geom::Point;
    use wall_core::input::Platform;
    use wall_core::keymap::{HelpVerb, help_sections};
    use wall_core::menu::{
        GroupLayout, MenuConfig, MenuGroup, MenuState, MenuTarget, MenuTerminal, TerminalPart,
    };

    use crate::atlas::Atlas;
    use crate::wall::camera::WallCamera;
    use crate::wall::renderer::{
        TerminalDraw, WALL_DEPTH_FORMAT, WallRenderer, WallStyle, WallTarget,
    };

    use super::context_menu::{ContextMenuInput, ContextMenuState, OpenContextMenu};
    use super::help::{HelpPanelInput, HelpPanelState};

    /// Renders both `context_menu` (with active hold-to-close ring) and `help`
    /// (docked right) over the wgpu wall test-pattern onto an offscreen texture,
    /// verifies panel pixels, and writes `target/glyph-scratch/wall-panels-preview.png`.
    #[test]
    fn renders_context_menu_and_help_panel_over_wall_to_png() {
        let ctx = crate::gpu::test_device::context();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let atlas = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut wall_renderer =
            WallRenderer::new(&ctx.device, &ctx.queue, atlas, format, WallStyle::default());
        let mut egui_renderer = egui_wgpu::Renderer::new(
            &ctx.device,
            format,
            egui_wgpu::RendererOptions::default(),
        );

        let width = 1280u32;
        let height = 800u32;
        let placed =
            crate::wall::test_pattern::build_overlapping_test_pattern(wall_renderer.metrics());
        let draws: Vec<TerminalDraw<'_>> = placed
            .iter()
            .map(|t| TerminalDraw {
                id: t.id,
                screen: &t.screen,
                position: t.position,
                z: t.z,
                focused: t.focused,
                panel_opacity: None,
                border_color: t.border_color,
            })
            .collect();
        let pad = wall_renderer.style().panel_padding;
        let metrics = wall_renderer.metrics();
        let bounds = placed
            .iter()
            .map(|t| metrics.panel_rect(t.position, t.screen.cols, t.screen.rows, pad))
            .reduce(|a, b| a.union(b))
            .expect("test pattern non-empty");
        let camera = WallCamera::frame_rect(bounds, (width, height), 1.15);

        let wall_state = MenuState {
            focused_id: Some("t2".to_owned()),
            selection: vec!["t2".to_owned()],
            fullscreen: None,
            sidebar_open: false,
            status_window_open: false,
            settings_panel_open: false,
            config: MenuConfig::default(),
            terminals: vec![MenuTerminal {
                id: "t1".to_owned(),
                name: "build-shell".to_owned(),
                named: true,
                cols: 100,
                rows: 30,
                hidden: false,
                group: None,
                selected: true,
                agent_view: None,
            }],
            groups: vec![MenuGroup {
                id: "g1".to_owned(),
                name: "workers".to_owned(),
                named: true,
                layout: GroupLayout::Tile,
                columns: 2,
                bars: true,
                members: vec![],
            }],
        };
        let menu_input = ContextMenuInput {
            open: Some(OpenContextMenu::from_wall(
                MenuTarget::Terminal {
                    id: "t1".to_owned(),
                    part: TerminalPart::Body,
                },
                Point::new(140.0, 90.0),
                &wall_state,
                &wall_state.config,
                Platform::Mac,
            )),
            close_hold_ms: 600.0,
            menu_hint: true,
            platform: Platform::Mac,
            viewport: None,
        };

        let verbs = [
            HelpVerb {
                name: "wall.reveal",
                usage: "<id>",
                description: "Show a terminal (if hidden), focus it and frame it",
            },
            HelpVerb {
                name: "wall.menu",
                usage: "terminal <id> [x y] | group <gid> [x y] | canvas [x y] | close",
                description: "Open the right-click menu or close it",
            },
            HelpVerb {
                name: "wall.help",
                usage: "[toggle|open|close|left|right|float [x y]]",
                description: "Open, close or place the help",
            },
        ];
        let viewport_rect =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width as f32, height as f32));
        let dock = dock_layout(&DockState {
            help_panel: Some(DockPanel::new(true, DockSide::Right)),
            ..Default::default()
        });
        let help_input = HelpPanelInput {
            open: true,
            side: DockSide::Right,
            float_pos: None,
            dock_rect: HelpPanelInput::dock_rect_for(DockSide::Right, &dock, viewport_rect),
            platform: Platform::Mac,
            sections: help_sections(Platform::Mac, "ctrl alt meta", &verbs).expect("valid"),
        };

        let egui_ctx = egui::Context::default();
        let mut menu_state = ContextMenuState::default();
        let mut help_state = HelpPanelState::default();
        let mut verbs_out = Vec::new();

        // Frame 1: select the Hold ("close") row and start holding Enter at t=1.0s.
        let raw1 = egui::RawInput {
            screen_rect: Some(viewport_rect),
            time: Some(1.0),
            events: vec![
                egui::Event::Key {
                    key: egui::Key::End,
                    physical_key: Some(egui::Key::End),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: Some(egui::Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut out1 = egui_ctx.run_ui(raw1, |root_ui| {
            let _ = super::help::show(root_ui.ctx(), &mut help_state, &help_input, &mut verbs_out);
            let _ = super::context_menu::show(
                root_ui.ctx(),
                &mut menu_state,
                &menu_input,
                &mut verbs_out,
            );
        });
        for (id, deltas) in out1.textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(&ctx.device, &ctx.queue, id, &delta);
            }
        }

        // Frame 2: advance to t=1.35s (~58% ring progress) and render to RGBA texture.
        let raw2 = egui::RawInput {
            screen_rect: Some(viewport_rect),
            time: Some(1.35),
            ..Default::default()
        };
        let mut out2 = egui_ctx.run_ui(raw2, |root_ui| {
            let _ = super::help::show(root_ui.ctx(), &mut help_state, &help_input, &mut verbs_out);
            let _ = super::context_menu::show(
                root_ui.ctx(),
                &mut menu_state,
                &menu_input,
                &mut verbs_out,
            );
        });
        assert!(verbs_out.is_empty(), "mid-hold frame should not fire close yet");

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wall panels preview target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = texture.create_view(&Default::default());
        let depth_view = crate::scene::create_depth(&ctx.device, WALL_DEPTH_FORMAT, width, height);

        let unpadded_bpr = width * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wall panels preview readback"),
            size: u64::from(padded_bpr * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("wall panels preview encoder"),
        });
        let target = WallTarget {
            color_view: &color_view,
            depth_view: &depth_view,
            width,
            height,
        };
        wall_renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            &target,
            &camera,
            &draws,
        );

        let clipped = egui_ctx.tessellate(out2.shapes, out2.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: out2.pixels_per_point,
        };
        for (id, deltas) in out2.textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(&ctx.device, &ctx.queue, id, &delta);
            }
        }
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
                label: Some("wall panels egui pass"),
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
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        ctx.queue.submit([encoder.finish()]);
        for id in out1.textures_delta.free.drain().chain(out2.textures_delta.free.drain()) {
            egui_renderer.free_texture(&id);
        }

        let slice = readback.slice(..);
        glyph_field::map_read(
            &ctx.device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "wall panels preview readback",
        );
        let data = slice.get_mapped_range().expect("mapped");
        let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
        for row in 0..height {
            let start = (row * padded_bpr) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded_bpr as usize]);
        }
        drop(data);
        readback.unmap();

        // Verify both panel regions hold visible ink pixels (bright text / ring stroke).
        let menu_rect = menu_state.last_rect().expect("menu rect");
        let help_rect = help_state.last_rect().expect("help rect");
        let count_bright = |rect: egui::Rect| -> usize {
            let x0 = (rect.left().max(0.0) as u32).min(width);
            let x1 = (rect.right().ceil().max(0.0) as u32).min(width);
            let y0 = (rect.top().max(0.0) as u32).min(height);
            let y1 = (rect.bottom().ceil().max(0.0) as u32).min(height);
            let mut bright = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * width + x) * 4) as usize;
                    if pixels[i].max(pixels[i + 1]).max(pixels[i + 2]) > 140 {
                        bright += 1;
                    }
                }
            }
            bright
        };
        assert!(
            count_bright(menu_rect) > 100,
            "context menu region should contain rendered glyph and hold-ring pixels"
        );
        assert!(
            count_bright(help_rect) > 500,
            "help panel region should contain rendered section and shortcut pixels"
        );

        let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/glyph-scratch/wall-panels-preview.png");
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).expect("create preview scratch dir");
        }
        image::save_buffer(&out_path, &pixels, width, height, image::ColorType::Rgba8)
            .expect("write wall-panels-preview.png");
    }
}
