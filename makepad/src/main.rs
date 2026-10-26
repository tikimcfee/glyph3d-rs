//! glyph3d-makepad: the glyph3d renderer hosted in makepad (2026-10-26).
//! One window: the scene's viewport (`viewport`), and over it the four
//! panels: the field HUD (F8), the Debug panel (F1), the agent carrel's
//! transport, and the agent sessions browser (F7). The engine is the
//! library (`glyph3d-native` without its winit/egui driver); the host owns
//! the scene and rebuilds it (`host`), the panels read one state and emit
//! one action type (`ui_model`), and `ui_adapter` turns those into engine
//! calls. The command line is the engine's own (`--load-repo DIR`,
//! `--render-file P`, `--agent-session S`, `--field-mode` ...), less
//! makepad's flags (`--remote`, ...).
//!
//! Until the panels' widgets land, each panel slot shows its state as text.

pub use makepad_widgets;

use makepad_widgets::*;

mod host;
mod input;
mod sessions;
mod stand_in;
mod ui_adapter;
mod ui_model;
mod viewport;

use host::EngineHost;
use ui_model::{G3UiAction, G3UiState};

app_main!(App);

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    let PanelText = Label{
        draw_text +: {color: #xdfe7ee text_style +: {font_size: 8.5}}
    }
    let Panel = RoundedView{
        width: Fit height: Fit
        padding: Inset{left: 8 right: 8 top: 6 bottom: 6}
        draw_bg +: {color: #x101820d8 border_radius: 4.0}
    }

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.inner_size: vec2(1600, 1000)
                window.title: "glyph3d"
                body +: {
                    stage := View{
                        width: Fill height: Fill
                        flow: Overlay
                        viewport := mod.widgets.Glyph3dViewport{}
                        panels := View{
                            width: Fill height: Fill
                            flow: Down spacing: 8
                            padding: Inset{left: 8 top: 8 right: 8 bottom: 8}
                            hud := Panel{ hud_text := PanelText{text: ""} }
                            debug := Panel{ debug_text := PanelText{text: ""} }
                            carrel := Panel{ carrel_text := PanelText{text: ""} }
                            sessions := Panel{ sessions_text := PanelText{text: ""} }
                        }
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
    /// Built on the first event: the scene load happens once the window
    /// exists, and a command line the engine refuses ends the run there.
    #[rust]
    host: Option<EngineHost>,
}

impl App {
    /// Each placeholder panel shows its state as text until its widget
    /// (`g3_ui.rs`) lands.
    fn show_state(&mut self, cx: &mut Cx, state: &G3UiState) {
        let hud = state.hud_open && !state.hud_lines.is_empty();
        self.ui.view(cx, ids!(hud)).set_visible(cx, hud);
        if hud {
            self.ui.label(cx, ids!(hud_text)).set_text(cx, &state.hud_lines.join("\n"));
        }
        let debug = state.debug_open;
        self.ui.view(cx, ids!(debug)).set_visible(cx, debug);
        if debug {
            let text = match &state.debug {
                Some(d) => {
                    let mut lines = vec![format!("FPS: {:.1}", state.fps), d.camera_line.clone()];
                    lines.extend(d.engine_line.clone());
                    lines.push(d.pick_line.clone());
                    lines.extend(d.zone_line.clone());
                    lines.extend(d.grab_line.clone());
                    lines.push(d.lod.cull_line.clone());
                    lines.push(format!(
                        "text detail {:.2} px | show glyphs {:.2} px | greeking {} ({})",
                        d.lod.text_detail_px,
                        d.lod.show_glyphs_px,
                        d.lod.greeking,
                        if d.lod.greek_pure { "pure" } else { "smooth" }
                    ));
                    if let Some(z) = d.z_wrap_spacing {
                        lines.push(format!("z_wrap_spacing {z:.3}"));
                    }
                    lines.extend(d.z_extent_line.clone());
                    let selected = |c: &[ui_model::G3Choice]| c.iter().find(|c| c.selected).map(|c| c.label.clone());
                    lines.extend(selected(&d.strategies).map(|s| format!("layout engine: {s}")));
                    lines.extend(selected(&d.field_modes).map(|s| format!("glyph field: {s}")));
                    lines.extend(d.cluster_label.clone());
                    lines.extend(d.layout_label.clone());
                    if !d.files.is_empty() {
                        lines.push(format!("files: {}", d.files.len()));
                    }
                    lines.join("\n")
                }
                None => format!("FPS: {:.1}\ncamera/pick: n/a (demo scene has no probe)", state.fps),
            };
            self.ui.label(cx, ids!(debug_text)).set_text(cx, &text);
        }
        self.ui.view(cx, ids!(carrel)).set_visible(cx, state.carrel.is_some());
        if let Some(c) = &state.carrel {
            let text = format!(
                "Agent Carrel: {}\n{} | {} {}",
                c.session_id, c.position_label, c.window_label, c.history_label
            );
            self.ui.label(cx, ids!(carrel_text)).set_text(cx, &text);
        }
        self.ui.view(cx, ids!(sessions)).set_visible(cx, state.sessions_open);
        if state.sessions_open {
            let s = &state.sessions;
            let mut lines = vec![s.scanning_line.clone()];
            lines.extend(s.error.clone());
            lines.extend(s.count_line.clone());
            lines.extend(s.empty_line.clone());
            lines.extend(s.rows.iter().take(12).map(|r| {
                format!("{}{} {} {} {}", if r.is_subagent { "  └ " } else { "" }, r.harness_tag, r.title, r.age, r.size_line)
            }));
            self.ui.label(cx, ids!(sessions_text)).set_text(cx, &lines.join("\n"));
        }
    }

    fn apply(&mut self, action: G3UiAction) {
        if let Some(host) = &mut self.host {
            ui_adapter::apply(host, action);
        }
    }
}

impl MatchEvent for App {
    fn handle_key_down(&mut self, _cx: &mut Cx, ke: &KeyEvent) {
        if let Some(action) = input::host_key(ke.key_code) {
            self.apply(action);
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        crate::viewport::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if self.host.is_none() {
            match host::plan_from_args() {
                Ok((choice, cull_opts, ops)) => self.host = Some(EngineHost::new(choice, cull_opts, &ops)),
                Err(e) => {
                    eprintln!("error: {e}");
                    cx.quit();
                    return;
                }
            }
        }
        self.match_event(cx, event);
        let host = self.host.as_mut().expect("built above");
        self.ui.handle_event(cx, event, &mut Scope::with_data(host));
        if let Event::NextFrame(_) = event {
            let state = ui_adapter::ui_state(self.host.as_ref().expect("built above"));
            self.show_state(cx, &state);
        }
    }
}
