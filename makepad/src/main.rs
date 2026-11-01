//! glyph3d-makepad: the glyph3d renderer hosted in makepad (2026-10-26).
//! One window: the scene's viewport (`viewport`), and over it the panels
//! (`g3_ui`'s `G3UiLayer`): the field HUD (F8), the Debug panel (F1), the
//! agent carrel's transport, and the agent sessions browser (F7). The engine
//! is the library (`glyph3d-native` without its winit/egui driver); the host
//! owns the scene and rebuilds it (`host`), the panels read one state and
//! emit one action type (`ui_model`), and `ui_adapter` turns those into
//! engine calls. The command line is the engine's own (`--load-repo DIR`,
//! `--render-file P`, `--agent-session S`, `--field-mode` ...), less
//! makepad's flags (`--remote`, ...).

pub use makepad_widgets;

use std::rc::Rc;

use makepad_widgets::*;

mod definition;
mod frame_feed;
mod g3_passes;
mod g3_ui;
mod host;
mod input;
mod residency;
mod sessions;
mod ui_adapter;
mod ui_model;
mod viewport;

use g3_ui::G3UiLayer;
use host::EngineHost;
use ui_model::{G3Panel, G3UiAction};

app_main!(App, configure: |_cx: &mut Cx| host::parse_plan_or_exit());

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                depth_buffer: false
                window.inner_size: vec2(1600, 1000)
                window.title: "glyph3d"
                body +: {
                    stage := View{
                        width: Fill height: Fill
                        flow: Overlay
                        viewport := mod.widgets.Glyph3dViewport{}
                        g3 := mod.widgets.G3UiLayer{}
                    }
                }
            }
        }
    }
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    /// Built on the first event from the plan parsed at startup (a command
    /// line the engine refuses never gets this far).
    #[rust]
    host: Option<EngineHost>,
    #[rust]
    file_rows: ui_adapter::FileRows,
}

impl App {
    fn apply(&mut self, action: G3UiAction) {
        if let Some(host) = &mut self.host {
            ui_adapter::apply(host, action);
        }
    }

    /// The panels' state, once a frame.
    fn publish_state(&mut self, cx: &mut Cx) {
        let Some(host) = &self.host else { return };
        let state = Rc::new(ui_adapter::ui_state(host, &mut self.file_rows));
        if let Some(mut layer) = self.ui.widget(cx, ids!(g3)).borrow_mut::<G3UiLayer>() {
            layer.set_state(cx, state);
        }
    }

    /// A panel's text field holds the keyboard.
    fn text_focused(&self, cx: &Cx) -> bool {
        self.ui.widget(cx, ids!(g3)).borrow::<G3UiLayer>().is_some_and(|l| l.text_focused(cx))
    }
}

impl MatchEvent for App {
    fn handle_actions(&mut self, _cx: &mut Cx, actions: &Actions) {
        for action in g3_ui::g3_ui_actions(actions) {
            self.apply(action);
        }
    }

    /// The host's keys. F8 works anywhere; F1, F6 and F7 yield to a focused
    /// text field, as the egui window's did.
    fn handle_key_down(&mut self, cx: &mut Cx, ke: &KeyEvent) {
        if let Some(action) = input::host_key(ke.key_code) {
            if action == G3UiAction::TogglePanel(G3Panel::Hud) || !self.text_focused(cx) {
                self.apply(action);
            }
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        crate::g3_passes::script_mod(vm);
        crate::g3_ui::script_mod(vm);
        crate::viewport::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if self.host.is_none() {
            // The glyph blocks are resident within makepad's publication
            // envelope, which its HAL sets from the device's memory budget
            // when the session starts (before the first event). A backend
            // that reports no budget leaves it 0, and the host refuses to
            // start there rather than publish without a bound.
            let envelope = cx.publication_accounting().envelope;
            if envelope == 0 {
                eprintln!(
                    "error: makepad's {:?} backend reports no device memory budget (its publication envelope is 0), so the glyph blocks would have no bound on the device: glyph3d-makepad runs on a backend that reports one (Vulkan with VK_EXT_memory_budget)",
                    cx.gpu_backend()
                );
                std::process::exit(1);
            }
            // A scene that cannot be built (a load refused past the
            // machine's memory, a session that cannot be read) panics with
            // its reason, which the panic hook has printed: the app exits
            // on it rather than run on, its HAL containing the panic, with
            // no scene.
            let (choice, cull_opts, ops) = host::take_plan();
            let definition = host::take_definition();
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| EngineHost::new(choice, cull_opts, &ops, envelope, definition)));
            self.host = Some(built.unwrap_or_else(|_| std::process::exit(1)));
        }
        self.match_event(cx, event);
        let host = self.host.as_mut().expect("built above");
        self.ui.handle_event(cx, event, &mut Scope::with_data(host));
        if let Event::NextFrame(_) = event {
            self.publish_state(cx);
        }
    }
}
