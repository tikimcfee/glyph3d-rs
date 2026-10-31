//! `Glyph3dViewport`: the widget the scene draws into, and the scene's
//! input. Pointer and keys go to the engine's hooks the way the winit
//! driver routes them (`native/src/windowed/app.rs`): a left press picks
//! (`on_click`), the cursor feeds `on_cursor` (a grabbed file follows it),
//! the wheel feeds `on_scroll`, and the right button held, or backquote
//! toggled, locks the pointer for mouse-look (`on_mouse_look`) until it is
//! released, toggled off, or Escape. Keys reach the scene while the
//! viewport holds key focus (it takes it on a press, and at start), so
//! typing in a panel's text input never flies the camera. Positions are
//! physical px from the viewport's own corner, as the scene's ray picks
//! expect.
//!
//! What it draws: each frame the scene advances (`EngineHost::advance`) on
//! the next-frame event, and on draw the scene prepares its frame
//! (`SceneLike::prepare_frame`, the CPU half: camera, cull, environment,
//! changed rows, the probe sync) which the makepad passes draw
//! (`g3_passes::G3Renderer`, fed by `frame_feed::FrameFeed`).

use makepad_widgets::*;

use crate::frame_feed::FrameFeed;
use crate::g3_passes::{G3Draws, G3Renderer};
use crate::host::EngineHost;
use crate::input::scene_key;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.Glyph3dViewportBase = #(Glyph3dViewport::register_widget(vm))
    mod.widgets.Glyph3dViewport = set_type_default() do mod.widgets.Glyph3dViewportBase{
        width: Fill
        height: Fill
        draws: mod.draw.G3Draws{}
    }
}

/// Physical px a wheel line is worth (the winit driver's figure for a pixel
/// delta).
const PX_PER_LINE: f64 = 53.0;

#[derive(Script, ScriptHook, WidgetRef, WidgetRegister)]
pub struct Glyph3dViewport {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[live]
    draws: G3Draws,
    #[rust]
    renderer: Option<G3Renderer>,
    #[rust]
    feed: FrameFeed,
    #[rust]
    area: Area,
    #[rust]
    next_frame: NextFrame,
    #[rust]
    focused_once: bool,
    /// The last draw's DPI factor: logical points to physical px.
    #[rust(1.0)]
    dpi: f64,
}

impl Glyph3dViewport {
    /// A logical position as physical px from the viewport's corner.
    fn physical(&self, cx: &Cx, abs: DVec2) -> (f32, f32) {
        let rel = abs - self.area.rect(cx).pos;
        ((rel.x * self.dpi) as f32, (rel.y * self.dpi) as f32)
    }

    fn set_looking(&mut self, cx: &mut Cx, host: &mut EngineHost, on: bool) {
        if host.looking != on {
            host.looking = on;
            cx.lock_mouse_pointer(on);
        }
    }

}

impl WidgetNode for Glyph3dViewport {
    fn widget_uid(&self) -> WidgetUid {
        self.uid
    }

    fn walk(&mut self, _cx: &mut Cx) -> Walk {
        self.walk
    }

    fn area(&self) -> Area {
        self.area
    }

    fn redraw(&mut self, cx: &mut Cx) {
        self.area.redraw(cx);
    }
}

impl Widget for Glyph3dViewport {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if matches!(event, Event::GpuReset) {
            if let Some(renderer) = &mut self.renderer {
                renderer.reset_gpu();
            }
            self.area.redraw(cx);
        }
        let Some(host) = scope.data.get_mut::<EngineHost>() else { return };

        if self.next_frame.is_event(event).is_some() {
            host.advance();
            self.next_frame = cx.new_next_frame();
            self.area.redraw(cx);
        }

        // Mouse-look: while the pointer is locked its motion turns the camera,
        // wherever it would have been, and the right button's release (if
        // it locked it) ends the look.
        if host.looking {
            match event {
                Event::MouseMove(me) => {
                    let (dx, dy) = (me.lock_delta.x as f32, me.lock_delta.y as f32);
                    if dx != 0.0 || dy != 0.0 {
                        host.scene.on_mouse_look(dx, dy);
                    }
                    cx.repin_mouse_pointer();
                    return;
                }
                Event::MouseUp(me) if me.button.is_secondary() => {
                    self.set_looking(cx, host, false);
                    return;
                }
                _ => {}
            }
        }

        match event.hits(cx, self.area) {
            Hit::FingerDown(fe) => {
                cx.set_key_focus(self.area);
                match fe.device.mouse_button() {
                    Some(b) if b.is_secondary() => self.set_looking(cx, host, true),
                    Some(b) if b.is_primary() && !host.looking => {
                        let (x, y) = self.physical(cx, fe.abs);
                        host.scene.on_click(x, y);
                    }
                    _ => {}
                }
            }
            Hit::FingerMove(fe) => {
                let (x, y) = self.physical(cx, fe.abs);
                host.scene.on_cursor(x, y);
            }
            Hit::FingerHoverIn(fh) | Hit::FingerHoverOver(fh) => {
                let (x, y) = self.physical(cx, fh.abs);
                host.scene.on_cursor(x, y);
            }
            Hit::FingerScroll(fs) => {
                host.scene.on_scroll((-fs.scroll.y / PX_PER_LINE) as f32);
            }
            // Backquote toggles mouse-look (a hand without a usable right
            // button), Escape ends it; neither is the scene's.
            Hit::KeyDown(ke) if ke.key_code == KeyCode::Backtick => {
                let on = !host.looking;
                self.set_looking(cx, host, on);
            }
            Hit::KeyDown(ke) if ke.key_code == KeyCode::Escape => self.set_looking(cx, host, false),
            Hit::KeyDown(ke) => {
                if let Some(key) = scene_key(ke.key_code) {
                    if !ke.is_repeat {
                        host.key(key, true);
                    }
                }
            }
            Hit::KeyUp(ke) => {
                if let Some(key) = scene_key(ke.key_code) {
                    host.key(key, false);
                }
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let rect = cx.walk_turtle_with_area(&mut self.area, walk);
        if rect.size.x <= 1.0 || rect.size.y <= 1.0 {
            return DrawStep::done();
        }
        self.dpi = cx.current_dpi_factor();
        if let Some(host) = scope.data.get_mut::<EngineHost>() {
            host.set_viewport((rect.size.x * self.dpi).round() as u32, (rect.size.y * self.dpi).round() as u32);
            let renderer = self.renderer.get_or_insert_with(|| G3Renderer::new(cx.cx, host.glyph_envelope));
            let drew = match host.scene.prepare_frame(host.viewport(), host.time()) {
                Some(frame) => {
                    self.feed.draw(cx, renderer, &mut self.draws, rect, host.scene_generation(), &frame);
                    true
                }
                None => false,
            };
            if drew {
                host.count_frame();
            }
        }
        if !self.focused_once {
            self.focused_once = true;
            cx.set_key_focus(self.area);
            self.next_frame = cx.new_next_frame();
        }
        DrawStep::done()
    }
}
