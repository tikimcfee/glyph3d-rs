//! glyph3d's windowed panels as native Makepad widgets: the field HUD (F8),
//! the Agent Carrel panel, the Debug panel with its virtualized file browser
//! (F1), and the agent sessions browser (F7). The port of the egui overlay in
//! `native/src/windowed/state.rs`.
//!
//! The panels read the host's model (`ui_model.rs`: [`G3UiState`], rebuilt
//! once a frame) and answer with its [`G3UiAction`]s; they know nothing of
//! the engine. The host's crate root declares `mod ui_model;` and
//! `pub use makepad_widgets;`, then:
//!
//! ```ignore
//! // AppMain::script_mod, before the app's own module:
//! crate::makepad_widgets::script_mod(vm);
//! crate::g3_ui::script_mod(vm);
//! self::script_mod(vm)
//!
//! // Splash, over the viewport (flow: Overlay):
//! g3 := mod.widgets.G3UiLayer{}
//!
//! // Each frame:
//! if let Some(mut layer) = self.ui.widget(cx, ids!(g3)).borrow_mut::<G3UiLayer>() {
//!     layer.set_state(cx, Rc::new(state));
//! }
//!
//! // MatchEvent::handle_actions:
//! for action in g3_ui_actions(actions) { /* the adapter applies it */ }
//! ```
//!
//! The host owns F1/F7/F8 (the `*_open` flags); the panels emit
//! `TogglePanel` only from their own buttons. While a panel's text field
//! holds the keyboard ([`G3UiLayer::text_focused`]), the host flies no camera
//! and fires no verb key (K2 typing isolation).

use crate::makepad_widgets::*;
use crate::ui_model::*;
use std::rc::Rc;

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // A section caption inside a panel.
    let G3Caption = Label{
        width: Fill
        margin: Inset{top: 6}
        draw_text +: {color: #x8fa3b8 text_style: theme.font_bold{font_size: 8}}
    }
    // A plain wrapping readout line.
    let G3Line = Label{
        width: Fill
        draw_text +: {color: #xd8dee6 text_style: theme.font_regular{font_size: 8.5}}
    }
    // A monospace readout block (several lines in one label).
    let G3Mono = Label{
        width: Fit
        draw_text +: {color: #xd8dee6 text_style: theme.font_code{font_size: 8}}
    }
    // A row of up to eight choices (a model `Vec<G3Choice>` or the verbs);
    // the active one reads `[label]`, unused slots hide. Wraps when narrow.
    let G3Choices = View{
        width: Fill
        height: Fit
        flow: Flow.Right{wrap: true}
        spacing: 4
        c0 := ButtonFlat{text: ""}
        c1 := ButtonFlat{text: ""}
        c2 := ButtonFlat{text: ""}
        c3 := ButtonFlat{text: ""}
        c4 := ButtonFlat{text: ""}
        c5 := ButtonFlat{text: ""}
        c6 := ButtonFlat{text: ""}
        c7 := ButtonFlat{text: ""}
    }
    let G3Panel = RoundedView{
        width: 380
        height: Fit
        flow: Down
        spacing: 4
        padding: 10
        draw_bg.color: #x15191fee
        draw_bg.border_radius: 6.0
    }
    let G3Title = Label{
        width: Fill
        draw_text +: {color: #xf0f3f6 text_style: theme.font_bold{font_size: 10}}
    }

    // ── 1. The field HUD (F8).
    mod.widgets.G3HudOverlayBase = #(G3HudOverlay::register_widget(vm))
    mod.widgets.G3HudOverlay = set_type_default() do mod.widgets.G3HudOverlayBase{
        width: Fit
        height: Fit
        RoundedView{
            width: Fit
            height: Fit
            padding: Inset{left: 8 right: 8 top: 6 bottom: 6}
            draw_bg.color: #x0b0d10b8
            draw_bg.border_radius: 4.0
            hud_text := G3Mono{text: ""}
        }
    }

    // ── 2. The Agent Carrel panel (shown while a carrel is loaded).
    mod.widgets.G3CarrelPanelBase = #(G3CarrelPanel::register_widget(vm))
    mod.widgets.G3CarrelPanel = set_type_default() do mod.widgets.G3CarrelPanelBase{
        width: Fit
        height: Fit
        G3Panel{
            carrel_title := G3Title{text: "Agent Carrel"}
            View{
                width: Fill
                height: Fit
                flow: Flow.Right{wrap: true}
                spacing: 4
                align: Align{y: 0.5}
                carrel_oldest := ButtonFlat{text: "|< Oldest (End)"}
                carrel_prev := ButtonFlat{text: "< Prev"}
                carrel_position := Label{
                    text: ""
                    draw_text +: {color: #xf0f3f6 text_style: theme.font_bold{font_size: 9}}
                }
                carrel_next := ButtonFlat{text: "Next >"}
                carrel_latest := ButtonFlat{text: "Latest (Home) >|"}
                carrel_mode := ButtonFlat{text: "Mode (V)"}
                carrel_sessions := ButtonFlat{text: "Sessions (F7)"}
            }
            View{
                width: Fill
                height: Fit
                flow: Flow.Right{wrap: true}
                spacing: 6
                carrel_window := Label{
                    text: ""
                    draw_text +: {color: #xf0f3f6 text_style: theme.font_bold{font_size: 8.5}}
                }
                carrel_history := Label{text: ""}
            }
            G3Caption{text: "WINDOW LIMITS & TIME TRAVEL (apply on release)"}
            deck_window_limit := Slider{text: "Deck window (cards)" min: 5 max: 100 step: 1 precision: 0 default: 20}
            deck_scroll_offset := Slider{text: "Time scroll K (turns back)" min: 0 max: 1 step: 1 precision: 0 default: 0}
            desk_revision_limit := Slider{text: "Desk revisions (per file)" min: 3 max: 50 step: 1 precision: 0 default: 20}
            desk_scroll_offset := Slider{text: "Desk scroll (revisions back)" min: 0 max: 1 step: 1 precision: 0 default: 0}
            max_file_stacks := Slider{text: "Max file stacks" min: 5 max: 50 step: 1 precision: 0 default: 20}
            beat_caption := G3Caption{text: "ACTIVE BEAT"}
            beat_summary := G3Line{text: ""}
            prompt_caption := G3Caption{text: "TURN PROMPT"}
            prompt_summary := G3Line{text: ""}
            files_caption := G3Caption{text: "WORKDESK FILES"}
            touched_files := G3Line{text: ""}
        }
    }

    // ── 3. The Debug panel (F1) and its virtualized file browser.
    mod.widgets.G3DebugPanelBase = #(G3DebugPanel::register_widget(vm))
    mod.widgets.G3DebugPanel = set_type_default() do mod.widgets.G3DebugPanelBase{
        width: Fit
        height: Fill
        G3Panel{
            height: Fill
            G3Title{text: "Debug (F1)"}
            ScrollYView{
                width: Fill
                height: Fill
                flow: Down
                spacing: 4
                fps_text := G3Line{text: ""}
                no_probe := G3Line{text: "camera/pick: n/a (demo scene has no probe)"}
                probe_section := View{
                    width: Fill
                    height: Fit
                    flow: Down
                    spacing: 4
                    camera_text := G3Line{text: ""}
                    copy_pose := ButtonFlat{text: "copy pose (--cam-pose, degrees)"}
                    engine_text := G3Line{text: ""}
                    pick_text := G3Line{text: ""}
                    zone_text := G3Line{text: ""}
                    grab_text := G3Line{text: ""}
                    View{
                        width: Fill
                        height: Fit
                        flow: Flow.Right{wrap: true}
                        spacing: 4
                        carrel_grab := ButtonFlat{text: ""}
                        file_grab := ButtonFlat{text: ""}
                    }

                    G3Caption{text: "VERBS (same strings --verb parses)"}
                    verbs := G3Choices{}
                    clear_pick := ButtonFlat{text: "Clear Pick"}
                    environment_toggle := ButtonFlat{text: ""}

                    G3Caption{text: "LOD: live, windowed only; px = on-screen pixels per text row"}
                    text_detail := Slider{text: "Text detail" min: 2.0 max: 32.0 precision: 1 default: 10.0 unit: "px"}
                    G3Line{text: "rows this tall get full curve detail; below, glyphs fuzz progressively"}
                    View{
                        width: Fill
                        height: Fit
                        flow: Flow.Right{wrap: true}
                        spacing: 4
                        align: Align{y: 0.5}
                        greeking := CheckBox{text: "fuzz below it (greeking, anti-moiré)"}
                        greek_smooth := ButtonFlat{text: "smooth"}
                        greek_pure := ButtonFlat{text: "pure (hard cut, max FPS)"}
                    }
                    show_glyphs := Slider{text: "Show glyphs" min: 0.25 max: 16.0 precision: 2 default: 1.0 unit: "px" taper: Log}
                    show_glyphs_note := G3Line{text: ""}
                    file_backgrounds := CheckBox{text: "File card backgrounds"}
                    View{
                        width: Fill
                        height: Fit
                        flow: Right
                        spacing: 6
                        align: Align{y: 0.5}
                        bg_swatch := SolidView{width: 18 height: 18 draw_bg.color: #x333333}
                        View{
                            width: Fill
                            height: Fit
                            flow: Down
                            bg_r := Slider{text: "R" min: 0.0 max: 1.0 precision: 2}
                            bg_g := Slider{text: "G" min: 0.0 max: 1.0 precision: 2}
                            bg_b := Slider{text: "B" min: 0.0 max: 1.0 precision: 2}
                            bg_a := Slider{text: "A" min: 0.0 max: 1.0 precision: 2 default: 1.0}
                        }
                    }
                    cull_text := G3Line{text: ""}

                    z_section := View{
                        width: Fill
                        height: Fit
                        flow: Down
                        spacing: 4
                        G3Caption{text: "LAYOUT (repo): applies on release; the scene rebuilds, pick/selection state resets"}
                        z_wrap_spacing := Slider{text: "z_wrap_spacing x em (0 = flat, default 0.15)" min: 0.0 max: 1.0 step: 0.01 precision: 2 default: 0.15}
                        z_extent := G3Line{text: ""}
                    }
                    strategy_section := View{
                        width: Fill
                        height: Fit
                        flow: Down
                        spacing: 4
                        G3Caption{text: "LAYOUT ENGINE (repo): click switches backend & rebuilds"}
                        strategies := G3Choices{}
                    }
                    field_section := View{
                        width: Fill
                        height: Fit
                        flow: Down
                        spacing: 4
                        G3Caption{text: "GLYPH FIELD MODE: click switches the field & rebuilds"}
                        field_modes := G3Choices{}
                    }
                    cluster_section := View{
                        width: Fill
                        height: Fit
                        flow: Down
                        spacing: 4
                        G3Caption{text: "SEQUENCE PASS: click toggles; the scene rebuilds, pick/selection state resets"}
                        cluster_toggle := ButtonFlat{text: ""}
                    }
                    layout_section := View{
                        width: Fill
                        height: Fit
                        flow: Down
                        spacing: 4
                        G3Caption{text: "CANVAS LAYOUT (F6): click toggles; the scene rebuilds, pick/selection state resets"}
                        layout_toggle := ButtonFlat{text: ""}
                    }
                }
                View{
                    width: Fill
                    height: Fit
                    flow: Right
                    spacing: 6
                    align: Align{y: 0.5}
                    G3Line{width: Fit text: "agent sessions (F7):"}
                    open_sessions := ButtonFlat{text: "Browse Sessions"}
                }

                browser_section := View{
                    width: Fill
                    height: Fit
                    flow: Down
                    spacing: 4
                    files_count := G3Caption{text: "FILES"}
                    file_filter := TextInput{width: Fill empty_text: "filter"}
                    file_list := PortalList{
                        width: Fill
                        height: 280
                        Row := View{
                            width: Fill
                            height: 22
                            flow: Right
                            spacing: 4
                            align: Align{y: 0.5}
                            swatch := SolidView{width: 10 height: 10 draw_bg.color: #x888888}
                            name := ButtonFlat{
                                width: Fill
                                align: Align{x: 0.0 y: 0.5}
                                text: ""
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 4. The agent sessions browser (F7).
    mod.widgets.G3SessionsModalBase = #(G3SessionsModal::register_widget(vm))
    mod.widgets.G3SessionsModal = set_type_default() do mod.widgets.G3SessionsModalBase{
        width: Fit
        height: Fit
        G3Panel{
            width: 580
            height: 480
            View{
                width: Fill
                height: Fit
                flow: Right
                align: Align{y: 0.5}
                G3Title{text: "Agent Sessions (F7)"}
                close := ButtonFlat{text: "Close"}
            }
            View{
                width: Fill
                height: Fit
                flow: Right
                spacing: 4
                align: Align{y: 0.5}
                Label{text: "Search:"}
                session_filter := TextInput{width: Fill empty_text: ""}
                clear_filter := ButtonFlat{text: "x"}
                refresh := ButtonFlat{text: "Refresh"}
            }
            harness_tabs := G3Choices{}
            scanning_text := G3Line{text: "" draw_text +: {text_style: theme.font_regular{font_size: 7.5}}}
            error_text := G3Line{text: "" draw_text +: {color: #xff8080}}
            count_text := G3Line{text: ""}
            empty_text := G3Line{text: ""}
            session_list := PortalList{
                width: Fill
                height: Fill
                Row := RoundedView{
                    width: Fill
                    height: Fit
                    flow: Down
                    spacing: 2
                    padding: 6
                    margin: Inset{bottom: 4}
                    draw_bg.color: #x1e242c
                    draw_bg.border_radius: 4.0
                    View{
                        width: Fill
                        height: Fit
                        flow: Right
                        spacing: 6
                        align: Align{y: 0.5}
                        sub := Label{text: "  └ subagent" draw_text +: {color: #x8fa3b8}}
                        tag := Label{text: ""}
                        project := Label{text: ""}
                        status := Label{text: "" draw_text +: {color: #x8fa3b8 text_style: theme.font_regular{font_size: 7.5}}}
                        Filler{height: 1}
                        age := Label{text: "" draw_text +: {color: #x8fa3b8 text_style: theme.font_regular{font_size: 7.5}}}
                        load := ButtonFlat{text: "Load"}
                    }
                    title := Label{
                        width: Fill
                        text: ""
                        draw_text +: {color: #xf0f3f6 text_style: theme.font_bold{font_size: 9}}
                    }
                    meta := Label{text: "" draw_text +: {color: #x8fa3b8 text_style: theme.font_code{font_size: 7.5}}}
                }
            }
        }
    }

    // ── All four over the viewport.
    mod.widgets.G3UiLayerBase = #(G3UiLayer::register_widget(vm))
    mod.widgets.G3UiLayer = set_type_default() do mod.widgets.G3UiLayerBase{
        width: Fill
        height: Fill
        flow: Overlay
        View{
            width: Fit
            height: Fill
            flow: Down
            spacing: 8
            padding: 8
            hud := mod.widgets.G3HudOverlay{visible: false}
            debug_panel := mod.widgets.G3DebugPanel{visible: false}
        }
        View{
            width: Fill
            height: Fill
            padding: 8
            align: Align{x: 1.0 y: 0.0}
            carrel_panel := mod.widgets.G3CarrelPanel{visible: false}
        }
        View{
            width: Fill
            height: Fill
            align: Align{x: 0.5 y: 0.5}
            sessions := mod.widgets.G3SessionsModal{visible: false}
        }
    }
}

/// Every panel action in `actions`, in order.
pub fn g3_ui_actions(actions: &Actions) -> Vec<G3UiAction> {
    actions
        .iter()
        .filter_map(|a| a.as_widget_action())
        .filter_map(|wa| wa.action.downcast_ref::<G3UiAction>())
        .cloned()
        .collect()
}

// ─────────────────────────── Shared helpers ───────────────────────────

const CHOICE_SLOTS: usize = 8;

fn choice_text(label: &str, selected: bool) -> String {
    if selected { format!("[{label}]") } else { label.to_string() }
}

fn choice_slot(i: usize) -> LiveId {
    LiveId::from_str(&format!("c{i}"))
}

/// Fill a `G3Choices` row from `labels` (label, selected); slots past the
/// end hide. More than eight options show the first eight.
fn sync_choices<'a>(cx: &mut Cx, row: &WidgetRef, labels: impl Iterator<Item = (&'a str, bool)>) {
    let mut labels = labels;
    for i in 0..CHOICE_SLOTS {
        let button = row.button(cx, &[choice_slot(i)]);
        match labels.next() {
            Some((label, selected)) => {
                button.set_text(cx, &choice_text(label, selected));
                button.set_visible(cx, true);
            }
            None => button.set_visible(cx, false),
        }
    }
}

/// The slot of a `G3Choices` row that was clicked.
fn clicked_choice(cx: &mut Cx, row: &WidgetRef, actions: &Actions) -> Option<usize> {
    (0..CHOICE_SLOTS).find(|&i| row.button(cx, &[choice_slot(i)]).clicked(actions))
}

fn rgb(c: [u8; 3]) -> Vec4f {
    vec4(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, 1.0)
}

fn set_text_color(cx: &mut Cx, widget: &WidgetRef, c: [u8; 3]) {
    let color = rgb(c);
    let mut widget = widget.clone();
    script_apply_eval!(cx, widget, { draw_text +: { color: #(color) } });
}

fn set_bg_color(cx: &mut Cx, widget: &WidgetRef, color: Vec4f) {
    let mut widget = widget.clone();
    script_apply_eval!(cx, widget, { draw_bg +: { color: #(color) } });
}

/// Show `text` in a label, or hide it when there is none.
fn show_line(cx: &mut Cx, view: &View, id: &[LiveId], text: Option<&str>) {
    view.widget(cx, id).set_visible(cx, text.is_some());
    if let Some(text) = text {
        view.label(cx, id).set_text(cx, text);
    }
}

/// Sliders between a press and its release, so a per-frame `set_state`
/// does not pull the handle from under the pointer.
#[derive(Default)]
struct HeldSliders(Vec<LiveId>);

impl HeldSliders {
    fn track(&mut self, slider: &SliderRef, id: LiveId, actions: &Actions) {
        if slider.start_slide(actions) && !self.0.contains(&id) {
            self.0.push(id);
        }
        if slider.end_slide(actions).is_some() {
            self.0.retain(|h| *h != id);
        }
    }
}

/// Write `v` into a slider unless the person holds it or is typing in it.
fn sync_slider(cx: &mut Cx, view: &View, id: LiveId, held: &HeldSliders, v: f64) {
    let slider = view.slider(cx, &[id]);
    if held.0.contains(&id) || slider.has_text_focus(cx) {
        return;
    }
    if slider.value().is_some_and(|cur| (cur - v).abs() > 1e-6) {
        slider.set_value(cx, v);
    }
}

fn sync_check(cx: &mut Cx, view: &View, id: &[LiveId], v: bool) {
    let check = view.check_box(cx, id);
    if check.active(cx) != v {
        check.set_active(cx, v, Animate::No);
    }
}

// ───────────────────────────── 1. The HUD ─────────────────────────────

#[derive(Script, ScriptHook, Widget)]
pub struct G3HudOverlay {
    #[deref]
    view: View,
}

impl G3HudOverlay {
    pub fn set_state(&mut self, cx: &mut Cx, state: &G3UiState) {
        self.view.label(cx, ids!(hud_text)).set_text(cx, &state.hud_lines.join("\n"));
    }
}

impl Widget for G3HudOverlay {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

// ───────────────────────── 2. The carrel panel ─────────────────────────

#[derive(Script, ScriptHook, Widget)]
pub struct G3CarrelPanel {
    #[deref]
    view: View,
    #[rust]
    options: Option<G3CarrelOptions>,
    #[rust]
    held: HeldSliders,
    /// The time scrolls' upper bounds as last applied to their sliders.
    #[rust]
    scroll_max: Option<(usize, usize)>,
}

const CARREL_SLIDERS: [LiveId; 5] = [
    live_id!(deck_window_limit),
    live_id!(deck_scroll_offset),
    live_id!(desk_revision_limit),
    live_id!(desk_scroll_offset),
    live_id!(max_file_stacks),
];

impl G3CarrelPanel {
    pub fn set_state(&mut self, cx: &mut Cx, state: &G3UiState) {
        let Some(c) = &state.carrel else {
            self.options = None;
            return;
        };
        let v = &self.view;
        v.label(cx, ids!(carrel_title)).set_text(cx, &format!("Agent Carrel: {}", c.session_id));
        v.label(cx, ids!(carrel_position)).set_text(cx, &c.position_label);
        v.label(cx, ids!(carrel_window)).set_text(cx, &c.window_label);
        v.label(cx, ids!(carrel_history)).set_text(cx, &c.history_label);
        set_text_color(cx, &v.widget(cx, ids!(carrel_history)), c.history_color);

        // The time scrolls' ranges follow the session; a slider's max is a
        // DSL property, so it is applied, not set.
        let max = (c.max_deck_scroll.max(1), c.max_desk_scroll.max(1));
        if self.scroll_max != Some(max) {
            self.scroll_max = Some(max);
            let (deck_max, desk_max) = (max.0 as f64, max.1 as f64);
            let mut deck = v.widget(cx, ids!(deck_scroll_offset));
            script_apply_eval!(cx, deck, { max: #(deck_max) });
            let mut desk = v.widget(cx, ids!(desk_scroll_offset));
            script_apply_eval!(cx, desk, { max: #(desk_max) });
        }
        let o = c.options;
        let values = [
            o.deck_window_limit,
            o.deck_scroll_offset,
            o.desk_revision_limit,
            o.desk_scroll_offset,
            o.max_file_stacks,
        ];
        for (id, value) in CARREL_SLIDERS.iter().zip(values) {
            sync_slider(cx, v, *id, &self.held, value as f64);
        }

        let beat = (!c.beat_summary.is_empty()).then_some(c.beat_summary.as_str());
        v.widget(cx, ids!(beat_caption)).set_visible(cx, beat.is_some());
        show_line(cx, v, ids!(beat_summary), beat);
        set_text_color(cx, &v.widget(cx, ids!(beat_summary)), c.beat_color);
        let prompt = (!c.prompt_summary.is_empty()).then_some(c.prompt_summary.as_str());
        v.widget(cx, ids!(prompt_caption)).set_visible(cx, prompt.is_some());
        show_line(cx, v, ids!(prompt_summary), prompt);
        let files: Vec<String> = c.touched_files.iter().map(|f| format!("• {f}")).collect();
        let files = files.join("\n");
        v.widget(cx, ids!(files_caption)).set_visible(cx, !files.is_empty());
        v.label(cx, ids!(files_caption))
            .set_text(cx, &format!("WORKDESK FILES ({})", c.touched_files.len()));
        show_line(cx, v, ids!(touched_files), (!files.is_empty()).then_some(files.as_str()));
        self.options = Some(o);
    }
}

impl Widget for G3CarrelPanel {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        self.widget_match_event(cx, event, scope);
    }
}

impl WidgetMatchEvent for G3CarrelPanel {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, _scope: &mut Scope) {
        let Some(options) = self.options else { return };
        let uid = self.widget_uid();
        let v = &self.view;
        for (id, action) in [
            (ids!(carrel_oldest), G3UiAction::CarrelOldest),
            (ids!(carrel_prev), G3UiAction::CarrelPrev),
            (ids!(carrel_next), G3UiAction::CarrelNext),
            (ids!(carrel_latest), G3UiAction::CarrelLatest),
            (ids!(carrel_mode), G3UiAction::CarrelMode),
            (ids!(carrel_sessions), G3UiAction::TogglePanel(G3Panel::Sessions)),
        ] {
            if v.button(cx, id).clicked(actions) {
                cx.widget_action(uid, action);
            }
        }
        // The dials rebuild the scene: on release (or a typed value), never
        // per tick, as one SetCarrelOptions with the others as they are.
        let mut opts = options;
        for id in CARREL_SLIDERS {
            let slider = v.slider(cx, &[id]);
            self.held.track(&slider, id, actions);
            if let Some(value) = slider.end_slide(actions) {
                let value = value.round().max(0.0) as usize;
                match id {
                    live_id!(deck_window_limit) => opts.deck_window_limit = value,
                    live_id!(deck_scroll_offset) => opts.deck_scroll_offset = value,
                    live_id!(desk_revision_limit) => opts.desk_revision_limit = value,
                    live_id!(desk_scroll_offset) => opts.desk_scroll_offset = value,
                    _ => opts.max_file_stacks = value,
                }
            }
        }
        if opts != options {
            cx.widget_action(uid, G3UiAction::SetCarrelOptions(opts));
        }
    }
}

// ───────────────────────── 3. The Debug panel ─────────────────────────

#[derive(Script, ScriptHook, Widget)]
pub struct G3DebugPanel {
    #[deref]
    view: View,
    #[rust]
    state: Rc<G3UiState>,
    #[rust]
    held: HeldSliders,
    #[rust]
    filter: String,
    /// `files` indices that pass the filter, and the `files` and filter
    /// they were computed from.
    #[rust]
    filtered: Vec<usize>,
    #[rust]
    filtered_for: Option<(*const Vec<G3FileRow>, String)>,
    /// The rows drawn this frame, and the set last reported as BrowserRows.
    #[rust]
    drawn: Vec<usize>,
    #[rust]
    reported: Option<Vec<usize>>,
}

const LOD_SLIDERS: [LiveId; 6] = [
    live_id!(text_detail),
    live_id!(show_glyphs),
    live_id!(bg_r),
    live_id!(bg_g),
    live_id!(bg_b),
    live_id!(bg_a),
];

impl G3DebugPanel {
    fn debug(&self) -> Option<&G3DebugState> {
        self.state.debug.as_ref()
    }

    pub fn text_focused(&self, cx: &Cx) -> bool {
        cx.has_key_focus(self.view.text_input(cx, ids!(file_filter)).area())
            || LOD_SLIDERS
                .iter()
                .chain(&[live_id!(z_wrap_spacing)])
                .any(|id| self.view.slider(cx, &[*id]).has_text_focus(cx))
    }

    fn refilter(&mut self, cx: &mut Cx) {
        let files = self.debug().map(|d| d.files.clone()).unwrap_or_default();
        let key = (Rc::as_ptr(&files), self.filter.clone());
        if self.filtered_for.as_ref() == Some(&key) {
            return;
        }
        let needle = self.filter.to_lowercase();
        self.filtered = files.iter().enumerate().filter(|(_, f)| f.matches(&needle)).map(|(i, _)| i).collect();
        self.filtered_for = Some(key);
        let count = if needle.is_empty() {
            format!("FILES: {} (click = select + fly to file)", files.len())
        } else {
            format!("FILES: {} of {} (click = select + fly to file)", self.filtered.len(), files.len())
        };
        self.view.label(cx, ids!(files_count)).set_text(cx, &count);
        self.view.redraw(cx);
    }

    pub fn set_state(&mut self, cx: &mut Cx, state: Rc<G3UiState>) {
        let v = &self.view;
        v.label(cx, ids!(fps_text)).set_text(cx, &format!("FPS: {:.1}", state.fps));
        v.widget(cx, ids!(no_probe)).set_visible(cx, state.debug.is_none());
        v.widget(cx, ids!(probe_section)).set_visible(cx, state.debug.is_some());
        let files_empty = state.debug.as_ref().is_none_or(|d| d.files.is_empty());
        v.widget(cx, ids!(browser_section)).set_visible(cx, !files_empty);
        if let Some(d) = &state.debug {
            v.label(cx, ids!(camera_text)).set_text(cx, &d.camera_line);
            show_line(cx, v, ids!(engine_text), d.engine_line.as_deref());
            v.label(cx, ids!(pick_text)).set_text(cx, &d.pick_line);
            show_line(cx, v, ids!(zone_text), d.zone_line.as_deref());
            show_line(cx, v, ids!(grab_text), d.grab_line.as_deref());
            set_text_color(cx, &v.widget(cx, ids!(grab_text)), d.grab_color);
            for (id, label) in [(ids!(carrel_grab), &d.carrel_grab_button), (ids!(file_grab), &d.file_grab_button)] {
                let button = v.button(cx, id);
                button.set_visible(cx, label.is_some());
                if let Some(label) = label {
                    button.set_text(cx, label);
                }
            }
            sync_choices(cx, &v.widget(cx, ids!(verbs)), d.verbs.iter().map(|s| (s.as_str(), false)));
            v.widget(cx, ids!(clear_pick)).set_visible(cx, d.can_clear_pick);
            v.button(cx, ids!(environment_toggle)).set_text(cx, &d.environment_label);

            let l = &d.lod;
            let [r, g, b, a] = l.file_bg_color;
            let values = [l.text_detail_px, l.show_glyphs_px, r, g, b, a];
            for (id, value) in LOD_SLIDERS.iter().zip(values) {
                sync_slider(cx, v, *id, &self.held, value as f64);
            }
            v.label(cx, ids!(show_glyphs_note)).set_text(cx, "rows this tall become glyphs; below, a file is its rectangle");
            sync_check(cx, v, ids!(greeking), l.greeking);
            sync_check(cx, v, ids!(file_backgrounds), l.file_backgrounds);
            v.button(cx, ids!(greek_smooth)).set_text(cx, &choice_text("smooth", !l.greek_pure));
            v.button(cx, ids!(greek_pure)).set_text(cx, &choice_text("pure (hard cut, max FPS)", l.greek_pure));
            set_bg_color(cx, &v.widget(cx, ids!(bg_swatch)), vec4(r, g, b, 1.0));
            v.label(cx, ids!(cull_text)).set_text(cx, &l.cull_line);

            v.widget(cx, ids!(z_section)).set_visible(cx, d.z_wrap_spacing.is_some());
            if let Some(z) = d.z_wrap_spacing {
                sync_slider(cx, v, live_id!(z_wrap_spacing), &self.held, z);
            }
            show_line(cx, v, ids!(z_extent), d.z_extent_line.as_deref());
            v.widget(cx, ids!(strategy_section)).set_visible(cx, !d.strategies.is_empty());
            sync_choices(cx, &v.widget(cx, ids!(strategies)), d.strategies.iter().map(|c| (c.label.as_str(), c.selected)));
            v.widget(cx, ids!(field_section)).set_visible(cx, !d.field_modes.is_empty());
            sync_choices(cx, &v.widget(cx, ids!(field_modes)), d.field_modes.iter().map(|c| (c.label.as_str(), c.selected)));
            v.widget(cx, ids!(cluster_section)).set_visible(cx, d.cluster_label.is_some());
            v.button(cx, ids!(cluster_toggle)).set_text(cx, d.cluster_label.as_deref().unwrap_or(""));
            v.widget(cx, ids!(layout_section)).set_visible(cx, d.layout_label.is_some());
            v.button(cx, ids!(layout_toggle)).set_text(cx, d.layout_label.as_deref().unwrap_or(""));
        }
        self.state = state;
        self.refilter(cx);
        self.view.redraw(cx);
    }
}

impl Widget for G3DebugPanel {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.drawn.clear();
        let state = self.state.clone();
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = item.borrow_mut::<PortalList>() else { continue };
            let Some(d) = state.debug.as_ref() else { continue };
            list.set_item_range(cx, 0, self.filtered.len());
            while let Some(pos) = list.next_visible_item(cx) {
                let Some(&index) = self.filtered.get(pos) else { continue };
                let Some(file) = d.files.get(index) else { continue };
                self.drawn.push(index);
                let dynst = d.file_dyn.get(index).copied().unwrap_or_default();
                let row = list.item(cx, pos, id!(Row));
                let marker = if d.selected_group == Some(file.group_id) { "> " } else { "" };
                let hidden = if dynst.hidden { "  (hidden)" } else { "" };
                row.button(cx, ids!(name))
                    .set_text(cx, &format!("{marker}{}{}{hidden}", "  ".repeat(file.depth), file.rel_path));
                let swatch = row.widget(cx, ids!(swatch));
                set_bg_color(cx, &swatch, rgb(dynst.tint));
                row.draw_all(cx, &mut Scope::empty());
            }
        }
        // The scene refreshes these rows' live state next frame. Reported
        // only when the set changes, so a still list wakes no event.
        if self.reported.as_ref() != Some(&self.drawn) {
            self.reported = Some(self.drawn.clone());
            cx.widget_action(self.widget_uid(), G3UiAction::BrowserRows(self.drawn.clone()));
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        self.widget_match_event(cx, event, scope);
    }
}

impl G3DebugPanel {
    /// A closed browser draws no rows: report that once, and report the
    /// rows again when it reopens.
    fn closed(&mut self, cx: &mut Cx) {
        if self.reported.as_ref().is_some_and(|r| r.is_empty()) {
            return;
        }
        self.reported = Some(Vec::new());
        cx.widget_action(self.widget_uid(), G3UiAction::BrowserRows(Vec::new()));
    }
}

impl WidgetMatchEvent for G3DebugPanel {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, _scope: &mut Scope) {
        let uid = self.widget_uid();
        let state = self.state.clone();
        let emit = |cx: &mut Cx, action: G3UiAction| cx.widget_action(uid, action);
        let v = &self.view;

        if v.button(cx, ids!(open_sessions)).clicked(actions) {
            emit(cx, G3UiAction::TogglePanel(G3Panel::Sessions));
        }
        let Some(d) = state.debug.as_ref() else { return };

        if v.button(cx, ids!(copy_pose)).clicked(actions) {
            emit(cx, G3UiAction::CopyPose);
        }
        if v.button(cx, ids!(carrel_grab)).clicked(actions) {
            emit(cx, G3UiAction::ToggleCarrelGrab);
        }
        if v.button(cx, ids!(file_grab)).clicked(actions) {
            emit(cx, G3UiAction::ToggleFileGrab);
        }
        if let Some(verb) = clicked_choice(cx, &v.widget(cx, ids!(verbs)), actions).and_then(|i| d.verbs.get(i)) {
            emit(cx, G3UiAction::Verb(verb.clone()));
        }
        if v.button(cx, ids!(clear_pick)).clicked(actions) {
            emit(cx, G3UiAction::ClearPick);
        }
        if v.button(cx, ids!(environment_toggle)).clicked(actions) {
            emit(cx, G3UiAction::ToggleEnvironment);
        }

        // Live controls, every tick.
        for id in LOD_SLIDERS {
            let slider = v.slider(cx, &[id]);
            self.held.track(&slider, id, actions);
            let Some(value) = slider.slided(actions) else { continue };
            let value = value as f32;
            let mut color = d.lod.file_bg_color;
            let action = match id {
                live_id!(text_detail) => G3UiAction::SetTextDetailPx(value),
                live_id!(show_glyphs) => G3UiAction::SetShowGlyphsPx(value),
                id => {
                    let lane = [live_id!(bg_r), live_id!(bg_g), live_id!(bg_b), live_id!(bg_a)]
                        .iter()
                        .position(|l| *l == id)
                        .unwrap_or(3);
                    color[lane] = value;
                    G3UiAction::SetFileBgColor(color)
                }
            };
            emit(cx, action);
        }
        if let Some(on) = v.check_box(cx, ids!(greeking)).changed(actions) {
            emit(cx, G3UiAction::SetGreeking(on));
        }
        if let Some(on) = v.check_box(cx, ids!(file_backgrounds)).changed(actions) {
            emit(cx, G3UiAction::SetFileBackgrounds(on));
        }
        for (id, pure) in [(ids!(greek_smooth), false), (ids!(greek_pure), true)] {
            if v.button(cx, id).clicked(actions) && d.lod.greek_pure != pure {
                emit(cx, G3UiAction::SetGreekPure(pure));
            }
        }

        // Layout: each rebuilds the scene, so a click on the current choice
        // sends nothing, and the dial fires on release only.
        let z_id = live_id!(z_wrap_spacing);
        let z_slider = v.slider(cx, &[z_id]);
        self.held.track(&z_slider, z_id, actions);
        if let Some(z) = z_slider.end_slide(actions) {
            if d.z_wrap_spacing.is_some_and(|cur| cur.to_bits() != z.to_bits()) {
                emit(cx, G3UiAction::SetZWrapSpacing(z));
            }
        }
        if let Some(i) = clicked_choice(cx, &v.widget(cx, ids!(strategies)), actions) {
            if d.strategies.get(i).is_some_and(|c| !c.selected) {
                emit(cx, G3UiAction::SetStrategy(i));
            }
        }
        if let Some(i) = clicked_choice(cx, &v.widget(cx, ids!(field_modes)), actions) {
            if d.field_modes.get(i).is_some_and(|c| !c.selected) {
                emit(cx, G3UiAction::SetFieldMode(i));
            }
        }
        if v.button(cx, ids!(cluster_toggle)).clicked(actions) {
            emit(cx, G3UiAction::ToggleCluster);
        }
        if v.button(cx, ids!(layout_toggle)).clicked(actions) {
            emit(cx, G3UiAction::ToggleLayout);
        }

        // The browser.
        let list = v.portal_list(cx, ids!(file_list));
        for (pos, row) in list.items_with_actions(actions) {
            if row.button(cx, ids!(name)).clicked(actions) {
                if let Some(&index) = self.filtered.get(pos) {
                    emit(cx, G3UiAction::PickFile(index));
                }
            }
        }
        if let Some(text) = v.text_input(cx, ids!(file_filter)).changed(actions) {
            self.filter = text;
            self.refilter(cx);
        }
    }
}

// ─────────────────────── 4. The sessions browser ───────────────────────

#[derive(Script, ScriptHook, Widget)]
pub struct G3SessionsModal {
    #[deref]
    view: View,
    #[rust]
    state: Rc<G3UiState>,
}

impl G3SessionsModal {
    pub fn text_focused(&self, cx: &Cx) -> bool {
        cx.has_key_focus(self.view.text_input(cx, ids!(session_filter)).area())
    }

    pub fn set_state(&mut self, cx: &mut Cx, state: Rc<G3UiState>) {
        let s = &state.sessions;
        let v = &self.view;
        // The host holds the search text: echo it, but never under the
        // cursor of someone typing.
        let input = v.text_input(cx, ids!(session_filter));
        if !cx.has_key_focus(input.area()) && input.text() != s.filter {
            input.set_text(cx, &s.filter);
        }
        sync_choices(cx, &v.widget(cx, ids!(harness_tabs)), s.harness_tabs.iter().map(|c| (c.label.as_str(), c.selected)));
        v.label(cx, ids!(scanning_text)).set_text(cx, &s.scanning_line);
        set_text_color(cx, &v.widget(cx, ids!(scanning_text)), if s.scanning_error { [255, 128, 128] } else { [143, 163, 184] });
        show_line(cx, v, ids!(error_text), s.error.as_deref());
        show_line(cx, v, ids!(count_text), s.count_line.as_deref());
        show_line(cx, v, ids!(empty_text), s.empty_line.as_deref());
        self.state = state;
        self.view.redraw(cx);
    }
}

impl Widget for G3SessionsModal {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let state = self.state.clone();
        let rows = &state.sessions.rows;
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            let Some(mut list) = item.borrow_mut::<PortalList>() else { continue };
            list.set_item_range(cx, 0, rows.len());
            while let Some(pos) = list.next_visible_item(cx) {
                let Some(s) = rows.get(pos) else { continue };
                let row = list.item(cx, pos, id!(Row));
                row.widget(cx, ids!(sub)).set_visible(cx, s.is_subagent);
                row.label(cx, ids!(tag)).set_text(cx, &s.harness_tag);
                let tag = row.widget(cx, ids!(tag));
                set_text_color(cx, &tag, s.harness_color);
                let project = if s.project.is_empty() { String::new() } else { format!("project: {}", s.project) };
                row.label(cx, ids!(project)).set_text(cx, &project);
                let project = row.widget(cx, ids!(project));
                set_text_color(cx, &project, s.project_color);
                row.label(cx, ids!(status)).set_text(cx, &s.status);
                row.label(cx, ids!(age)).set_text(cx, &s.age);
                row.label(cx, ids!(title)).set_text(cx, &s.title);
                row.label(cx, ids!(meta)).set_text(cx, &format!("{}  {}", s.id_line, s.size_line));
                row.draw_all(cx, &mut Scope::empty());
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        self.widget_match_event(cx, event, scope);
    }
}

impl WidgetMatchEvent for G3SessionsModal {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, _scope: &mut Scope) {
        let uid = self.widget_uid();
        let v = &self.view;
        if v.button(cx, ids!(close)).clicked(actions) {
            cx.widget_action(uid, G3UiAction::TogglePanel(G3Panel::Sessions));
        }
        if v.button(cx, ids!(refresh)).clicked(actions) {
            cx.widget_action(uid, G3UiAction::RefreshSessions);
        }
        let input = v.text_input(cx, ids!(session_filter));
        if let Some(text) = input.changed(actions) {
            cx.widget_action(uid, G3UiAction::SetSessionFilter(text));
        }
        if v.button(cx, ids!(clear_filter)).clicked(actions) {
            input.set_text(cx, "");
            cx.widget_action(uid, G3UiAction::SetSessionFilter(String::new()));
        }
        if let Some(i) = clicked_choice(cx, &v.widget(cx, ids!(harness_tabs)), actions) {
            if self.state.sessions.harness_tabs.get(i).is_some_and(|c| !c.selected) {
                cx.widget_action(uid, G3UiAction::SetSessionHarness(i));
            }
        }
        let list = v.portal_list(cx, ids!(session_list));
        for (pos, row) in list.items_with_actions(actions) {
            if row.button(cx, ids!(load)).clicked(actions) {
                if let Some(s) = self.state.sessions.rows.get(pos) {
                    cx.widget_action(uid, G3UiAction::LoadSession(s.path.clone()));
                }
            }
        }
    }
}

// ───────────────────────────── The layer ─────────────────────────────

#[derive(Script, ScriptHook, Widget)]
pub struct G3UiLayer {
    #[deref]
    view: View,
}

impl G3UiLayer {
    /// The model for this frame. Call it every frame: a call from
    /// `handle_startup` reaches the labels but not yet the sliders' values,
    /// and the next call puts them right.
    pub fn set_state(&mut self, cx: &mut Cx, state: Rc<G3UiState>) {
        let mut changed = false;
        for (id, open) in [
            (ids!(hud), state.hud_open),
            (ids!(debug_panel), state.debug_open),
            (ids!(carrel_panel), state.carrel.is_some()),
            (ids!(sessions), state.sessions_open),
        ] {
            let panel = self.view.widget(cx, id);
            if panel.visible() != open {
                panel.set_visible(cx, open);
                changed = true;
            }
        }
        if changed {
            self.view.redraw(cx);
        }
        let v = &self.view;
        if !state.debug_open {
            if let Some(mut debug) = v.widget(cx, ids!(debug_panel)).borrow_mut::<G3DebugPanel>() {
                debug.closed(cx);
            }
        }
        if let Some(mut hud) = v.widget(cx, ids!(hud)).borrow_mut::<G3HudOverlay>() {
            hud.set_state(cx, &state);
        }
        if let Some(mut carrel) = v.widget(cx, ids!(carrel_panel)).borrow_mut::<G3CarrelPanel>() {
            carrel.set_state(cx, &state);
        }
        if let Some(mut sessions) = v.widget(cx, ids!(sessions)).borrow_mut::<G3SessionsModal>() {
            sessions.set_state(cx, state.clone());
        }
        if let Some(mut debug) = v.widget(cx, ids!(debug_panel)).borrow_mut::<G3DebugPanel>() {
            debug.set_state(cx, state);
        }
    }

    /// A panel's text field holds the keyboard: the host must not fly the
    /// camera or fire verb keys (K2 typing isolation).
    pub fn text_focused(&self, cx: &Cx) -> bool {
        let debug = self.view.widget(cx, ids!(debug_panel));
        let sessions = self.view.widget(cx, ids!(sessions));
        (debug.visible() && debug.borrow::<G3DebugPanel>().is_some_and(|d| d.text_focused(cx)))
            || (sessions.visible() && sessions.borrow::<G3SessionsModal>().is_some_and(|m| m.text_focused(cx)))
    }
}

impl Widget for G3UiLayer {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selected_choice_reads_bracketed() {
        assert_eq!(choice_text("Derived", true), "[Derived]");
        assert_eq!(choice_text("Derived", false), "Derived");
    }

    #[test]
    fn choice_slots_are_c0_to_c7() {
        assert_eq!(choice_slot(0), live_id!(c0));
        assert_eq!(choice_slot(CHOICE_SLOTS - 1), live_id!(c7));
    }
}
