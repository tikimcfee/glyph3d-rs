//! Offscreen terminal byte-stream replay (`--wall-replay FILE --screenshot OUT.png`).
//!
//! Feeds a recorded stream of raw terminal bytes (such as a `g3d-termd` subscriber
//! capture containing `REPAINT` + `OUTPUT` frames) through [`glyph_vt::Vt`] at a fixed
//! `(cols, rows)` grid size and renders the resulting [`term_cells::Screen`] through
//! `super::renderer::render_wall_to_png`.

#[cfg(feature = "wgpu")]
use std::path::Path;

#[cfg(feature = "wgpu")]
use glam::Vec2;
use glyph_vt::{Theme, Vt};
use term_cells::Screen;

#[cfg(feature = "wgpu")]
use crate::gpu::GpuContext;

#[cfg(feature = "wgpu")]
use super::camera::WallCamera;
#[cfg(feature = "wgpu")]
use super::renderer::{TerminalDraw, WallRenderer, WallStyle, render_wall_to_png};

/// Feed `bytes` through a passive [`Vt`] emulator of `cols` × `rows` cells under the
/// wall's default [`Theme`] and return the resolved [`Screen`].
pub fn replay_screen(bytes: &[u8], cols: u16, rows: u16) -> Screen {
    let theme = Theme::default();
    let mut vt = Vt::new(cols, rows, &theme);
    vt.feed(bytes);
    let mut screen = Screen::new(cols, rows);
    vt.fill(&mut screen);
    screen
}

/// CLI driver for `--wall-replay FILE [--wall-size COLSxROWS] --screenshot OUT.png`.
#[cfg(feature = "wgpu")]
#[expect(
    clippy::too_many_arguments,
    reason = "CLI entry point forwarding decoded replay parameters directly to the wall renderer"
)]
pub fn render_replay_offscreen(
    ctx: &GpuContext,
    bytes: &[u8],
    cols: u16,
    rows: u16,
    emoji_sheet: &Path,
    path: &Path,
    frames: u32,
    zoom_multiplier: f32,
) {
    let screen = replay_screen(bytes, cols, rows);
    let mut renderer = WallRenderer::new(
        &ctx.device,
        &ctx.queue,
        emoji_sheet,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        WallStyle::default(),
    );

    let position = Vec2::ZERO;
    let draws = [TerminalDraw {
        id: 1,
        screen: &screen,
        position,
        z: 2.0,
        scale: 1.0,
        focused: true,
        panel_opacity: None,
        border_color: None,
        selection: None,
    }];

    let pad = renderer.style().panel_padding;
    let bounds = renderer
        .metrics()
        .panel_rect(position, screen.cols, screen.rows, pad);
    let mut camera = WallCamera::frame_rect(
        bounds,
        (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT),
        1.06,
    );
    if zoom_multiplier > 0.0 && zoom_multiplier.is_finite() {
        camera.zoom *= zoom_multiplier;
    }

    render_wall_to_png(
        ctx,
        &mut renderer,
        &camera,
        &super::renderer::WallFrame { terminals: &draws, ..Default::default() },
        (crate::OFFSCREEN_WIDTH, crate::OFFSCREEN_HEIGHT),
        frames,
        path,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_cells::rgb;

    #[test]
    fn replay_screen_decodes_ansi_colours_and_cursor_position() {
        let stream = b"\x1b[H\x1b[2J\x1b[38;2;120;200;80mhello\x1b[0m world";
        let screen = replay_screen(stream, 80, 24);
        assert_eq!((screen.cols, screen.rows), (80, 24));
        assert_eq!(screen.row(0)[0].content, u32::from(b'h'));
        assert_eq!(screen.row(0)[0].fg, rgb(120, 200, 80));
        assert_eq!(screen.row(0)[6].content, u32::from(b'w'));
        assert_eq!(screen.cursor.col, 11);
        assert_eq!(screen.cursor.row, 0);
    }
}
