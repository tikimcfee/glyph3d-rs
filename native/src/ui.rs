//! Stage M (M1): the interface shell — command bus, docked panels, palette.
//!
//! The constitution (integration/notes/11-interface-design.md, inherited from the
//! web app's second-pass GUI): every user intent is a `Command`; the executor in
//! windowed.rs dispatches through the EXACT SceneLike APIs the CLI op-stream uses
//! (`apply_pick` / `apply_verb` / `set_cam_pose`). Panels are one-way
//! state→view — they read a UiProbeState snapshot plus the command log and
//! return commands; they never touch the scene, a buffer, or the engine. The
//! one sanctioned direct write is the K4 live-dial seam (`lod_min_px` on the
//! shared probe cell), which offscreen never installs — continuous slider
//! tuning is not an intent and must not spam the bus.
//!
//! This module is pure egui + data: `draw_ui` builds one frame from
//! (input, state) and returns the commands that frame enqueued; execution
//! happens in windowed.rs after the egui pass closes. That split keeps the
//! scene/window borrows out of the UI build and makes the bus the single place
//! where intents become effects — the console log records exactly that stream,
//! and every palette row's subtitle IS the line the console would accept (the
//! palette-as-tutor law).
//!
//! Layout: HUD (top strip), Files (left dock), Inspector (right dock), Console
//! (bottom dock), palette popup (⌘K / Ctrl-K, toggled app-level in windowed.rs).
//! The field stays full-bleed — panels float over the 3D surface, matching the
//! web app's overlay chrome mode; hosting the canvas INSIDE a panel is post-L3
//! (note 08's sRGB wrinkle is recorded there). F1 toggles the whole shell (the
//! K3 Debug window's content lives on, redistributed into HUD/Inspector/Files).

use crate::glyph_scene::{UiProbe, UiProbeState, Verb};
// egui 0.36: one unified Panel type with left/right/top/bottom constructors,
// shown into a &mut Ui (not the Context — verified against the vendored
// source; Window is still Context-based).
use egui::containers::panel::Panel;

// ── The bus ─────────────────────────────────────────────────────────────────

/// One user intent, as data. Panel buttons, console lines, and palette picks
/// all produce these; windowed.rs's executor is the only consumer. Scene-bound
/// variants map 1:1 onto CLI op-stream operations (the log line the executor
/// records is the same one the CLI op would print).
#[derive(Clone)]
pub enum Command {
    /// A scene verb, parsed from the exact CLI `--verb` grammar.
    Verb(Verb),
    /// Deterministic pick (the `--pick-file/--pick-row/--pick-col` op). Used
    /// directly and as the targeting half of composed panel actions (e.g.
    /// "hide this file's group" = PickRowCol + Verb — the op-stream's own
    /// targeting composition).
    PickRowCol { file: String, row: u32, col: u32 },
    /// Fly the camera to frame the first file whose path contains SUBSTR
    /// (resolved against the live probe's group table by the executor).
    FlyFile(String),
    /// Fly the camera to an explicit pose (palette file rows carry the AABB
    /// already; the executor applies the K5 framing formula).
    FlyFileBox { center: [f32; 2], half: [f32; 2] },
    /// Capture the next presented frame (the F2 path).
    Screenshot,
    /// Show/hide one docked panel.
    TogglePanel(PanelId),
    /// Log the console/palette grammar (the palette-as-tutor law in text form).
    Help,
    /// Wipe the command log.
    ClearLog,
}

/// Docked panels toggleable at runtime (the HUD is the shell's chrome — always
/// on while the shell is; it carries the toggles themselves).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelId {
    Files,
    Inspector,
    Console,
}

impl PanelId {
    pub const ALL: [PanelId; 3] = [PanelId::Files, PanelId::Inspector, PanelId::Console];
    pub fn name(self) -> &'static str {
        match self {
            PanelId::Files => "files",
            PanelId::Inspector => "inspector",
            PanelId::Console => "console",
        }
    }
}

/// The command echo shown in the log / palette subtitle. Kept stable: it IS
/// the grammar lesson (a row's subtitle is the line the console would accept).
pub fn format_command(cmd: &Command) -> String {
    match cmd {
        Command::Verb(v) => format!("verb {}", verb_line(v)),
        Command::PickRowCol { file, row, col } => format!(
            "pick {file}:{row}:{col} (--pick-file {file} --pick-row {row} --pick-col {col})"
        ),
        Command::FlyFile(f) => format!("fly {f}"),
        Command::FlyFileBox { .. } => "fly (palette file box)".to_string(),
        Command::Screenshot => "shot".to_string(),
        Command::TogglePanel(p) => format!("panel {}", p.name()),
        Command::Help => "help".to_string(),
        Command::ClearLog => "clear".to_string(),
    }
}

/// Inverse of parse_verb for display — used for the log echo only, never for
/// execution (execution always goes through crate::parse_verb on the composed
/// string, keeping panel ⇔ CLI equivalence by construction).
fn verb_line(v: &Verb) -> String {
    match v {
        Verb::RecolorGlyph(c) => format!("recolor-glyph {:02x}{:02x}{:02x}", c[0], c[1], c[2]),
        Verb::RecolorLine(c) => format!("recolor-line {:02x}{:02x}{:02x}", c[0], c[1], c[2]),
        Verb::NudgeGlyph(d) => format!("nudge-glyph {} {} {}", d[0], d[1], d[2]),
        Verb::ScaleGlyph(s) => format!("scale-glyph {s}"),
        Verb::MoveGroup(d) => format!("move-group {} {} {}", d[0], d[1], d[2]),
        Verb::ScaleGroup(s) => format!("scale-group {s}"),
        Verb::TintGroup(c) => {
            // Stored normalized; round-trip through the hex the CLI uses.
            format!(
                "tint-group {:02x}{:02x}{:02x}",
                (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                (c[2].clamp(0.0, 1.0) * 255.0) as u8
            )
        }
        Verb::TintCycle => "tint-cycle".to_string(),
        Verb::SetHidden(true) => "hide-group".to_string(),
        Verb::SetHidden(false) => "show-group".to_string(),
        Verb::ToggleHidden => "toggle-hidden".to_string(),
    }
}

/// The console/palette grammar: the full `--verb` vocabulary plus the app
/// commands. Pure — the executor applies the effects.
pub fn parse_console_line(line: &str) -> Result<Vec<Command>, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(Vec::new());
    }
    let head = line.split_whitespace().next().expect("non-empty after trim");
    match head {
        "help" | "?" => return Ok(vec![Command::Help]),
        "clear" => return Ok(vec![Command::ClearLog]),
        "shot" | "screenshot" => return Ok(vec![Command::Screenshot]),
        "panel" => {
            let arg = line.split_whitespace().nth(1).unwrap_or("");
            return match PanelId::ALL.iter().find(|p| p.name() == arg) {
                Some(p) => Ok(vec![Command::TogglePanel(*p)]),
                None => Err(format!(
                    "panel: expected one of {} (got {arg:?})",
                    PanelId::ALL.iter().map(|p| p.name()).collect::<Vec<_>>().join("|")
                )),
            };
        }
        "fly" => {
            return match line.split_whitespace().nth(1) {
                Some(f) => Ok(vec![Command::FlyFile(f.to_string())]),
                None => Err("fly: expected a path substring (fly alpha.rs)".to_string()),
            };
        }
        _ => {}
    }
    if VERB_WORDS.contains(&head) {
        let verb = crate::parse_verb(line).map_err(|e| e.replace("--verb", "verb"))?;
        Ok(vec![Command::Verb(verb)])
    } else {
        Err(format!(
            "unknown command {head:?} — verbs ({}), or help|clear|shot|panel <id>|fly <substr>",
            VERB_WORDS.join("|")
        ))
    }
}

/// First tokens parse_verb accepts — the shared vocabulary (kept in lockstep
/// with crate::parse_verb's match arms; its parse error is the backstop).
const VERB_WORDS: &[&str] = &[
    "recolor-glyph",
    "recolor-line",
    "nudge-glyph",
    "scale-glyph",
    "move-group",
    "scale-group",
    "tint-group",
    "tint-cycle",
    "hide-group",
    "show-group",
    "toggle-hidden",
];

/// The console `help` text (logged by the executor; mirrors the grammar above).
pub fn help_text() -> &'static str {
    "verbs: recolor-glyph|recolor-line [rrggbb] · nudge-glyph dx dy [dz] · scale-glyph f · \
     move-group dx dy dz · scale-group s · tint-group rrggbb · tint-cycle · hide-group · \
     show-group · toggle-hidden\n\
     app: help · clear · shot · panel files|inspector|console · fly <substr> · \
     verbs apply to the most recent pick (left-click a glyph first)"
}

// ── State ────────────────────────────────────────────────────────────────────

/// One command-log line. `Result` lines carry the scene's own log string (the
/// same line the CLI op prints); `Error` lines are grammar/executor failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    Cmd,
    Result,
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub kind: LogKind,
    pub text: String,
}

/// Cap on the command log ring (the console shows the tail; full history is
/// not load-bearing state, and 500 lines is a few seconds of dense use).
const LOG_CAP: usize = 500;

/// All UI memory in one struct — the session-persistence serialization point
/// when M6 grows layout saving (notes/11).
pub struct UiState {
    /// F1: the whole shell (HUD + docks). False = bare field + hotkeys only.
    pub shell_open: bool,
    pub files_open: bool,
    pub inspector_open: bool,
    pub console_open: bool,
    /// Command log ring (shown by the console panel).
    pub log: Vec<LogLine>,
    pub console_input: String,
    pub console_history: Vec<String>,
    /// Index into history for ↑/↓ recall (None = live input).
    history_pos: Option<usize>,
    /// Files-panel substring filter (the K5 field, promoted).
    pub files_filter: String,
    /// The browser's selected group (highlight; click also flies).
    pub selected_group: Option<u32>,
    /// ⌘K palette state.
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_selected: usize,
    /// Inspector parameterized-verb scratch (applied via composed verb lines,
    /// never written to the scene directly).
    pub tint_rgb: [u8; 3],
    pub scale_group_v: f32,
    pub scale_glyph_v: f32,
    pub nudge_step: f32,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            shell_open: true,
            files_open: true,
            inspector_open: true,
            console_open: true,
            log: Vec::new(),
            console_input: String::new(),
            console_history: Vec::new(),
            history_pos: None,
            files_filter: String::new(),
            selected_group: None,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            tint_rgb: [120, 180, 255],
            scale_group_v: 1.25,
            scale_glyph_v: 1.5,
            nudge_step: 0.5,
        }
    }
}

impl UiState {
    pub fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        if self.log.len() == LOG_CAP {
            self.log.remove(0);
        }
        self.log.push(LogLine { kind, text: text.into() });
    }
}

/// Read-only per-frame input for the UI build. `probe` is the snapshot the
/// panels reflect; `probe_cell` is the shared cell itself (K4's live-dial seam
/// — the LOD slider writes it directly, per the continuous-tuning carve-out).
/// Both `None` for the demo scene. (Fly framing happens in the executor with
/// the live viewport aspect, so the UI never needs it.)
pub struct UiInput {
    pub probe: Option<UiProbeState>,
    pub probe_cell: Option<UiProbe>,
    pub fps: f32,
    pub instances: u32,
}

// ── Fly framing (the K5 formula, hoisted for the palette; the files panel
//    delegates to FlyFile/FlyFileBox so the executor frames with live data) ──

/// Front-camera framing for a file group's world AABB: fit the box, margin
/// 1.08 + 2.0 world units, text plane faces +Z (yaw = pitch = 0). Returns
/// (eye, yaw, pitch) for SceneLike::set_cam_pose.
pub fn fly_to_framing(center: [f32; 2], half: [f32; 2], viewport_aspect: f32) -> ([f32; 3], f32, f32) {
    let half_h_needed = half[1].max(half[0] / viewport_aspect);
    let dist = half_h_needed / (crate::glyph_scene::FOV_Y.to_radians() * 0.5).tan() * 1.08 + 2.0;
    ([center[0], center[1], dist], 0.0, 0.0)
}

// ── Palette matching ─────────────────────────────────────────────────────────

/// Subsequence match with a simple preference score (contiguous runs and word
/// starts rank higher). Case-insensitive; the query must consume entirely.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    // Both sides lowercased — "MAIN" must match "main" (the query side was
    // the miss; caught by the case-insensitivity test).
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = target.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut prev_match = false;
    for (ti, tc) in t.iter().enumerate() {
        if qi < q.len() && *tc == q[qi] {
            let word_start =
                ti == 0 || matches!(t[ti - 1], '/' | '-' | '_' | '.' | ' ');
            score += if prev_match {
                3
            } else if word_start {
                2
            } else {
                1
            };
            prev_match = true;
            qi += 1;
        } else {
            prev_match = false;
        }
    }
    (qi == q.len()).then_some(score - (target.len() as i32) / 16)
}

/// One palette row. `Template` fills the console input instead of firing —
/// parameterized verbs teach their grammar first, then take arguments.
enum PaletteEntry {
    Cmd { title: String, subtitle: String, cmd: Command },
    Template { title: String, subtitle: String, fill: String },
    File { title: String, center: [f32; 2], half: [f32; 2] },
}

impl PaletteEntry {
    fn title(&self) -> &str {
        match self {
            PaletteEntry::Cmd { title, .. }
            | PaletteEntry::Template { title, .. }
            | PaletteEntry::File { title, .. } => title,
        }
    }
    fn subtitle(&self) -> &str {
        match self {
            PaletteEntry::Cmd { subtitle, .. } | PaletteEntry::Template { subtitle, .. } => subtitle,
            PaletteEntry::File { .. } => "camera: frame this file's group",
        }
    }
}

/// The palette catalog: verbs + app commands (+ per-frame file entries from
/// the probe, added in `palette_entries`). Every subtitle is either the exact
/// console line the entry fires or the grammar it fills — the tutor law.
fn palette_catalog() -> Vec<PaletteEntry> {
    let mut e = Vec::new();
    let cmd = |title: &str, subtitle: String, cmd: Command| PaletteEntry::Cmd {
        title: title.to_string(),
        subtitle,
        cmd,
    };
    // Zero-arg verbs execute directly.
    for spec in ["tint-cycle", "hide-group", "show-group", "toggle-hidden"] {
        e.push(cmd(
            spec,
            format!("verb: {spec}"),
            Command::Verb(crate::parse_verb(spec).expect("palette verb literal")),
        ));
    }
    for (spec, what) in [
        ("recolor-glyph", "verb: recolor the picked glyph [rrggbb], default red"),
        ("recolor-line", "verb: recolor the picked line [rrggbb], default amber"),
    ] {
        e.push(cmd(
            spec,
            format!("{what} — e.g. `{spec} ff8040`"),
            Command::Verb(crate::parse_verb(spec).expect("palette verb literal")),
        ));
    }
    // Parameterized verbs become console templates (Enter/Tab fills).
    for (title, head) in [
        ("nudge-glyph dx dy [dz]", "nudge-glyph"),
        ("scale-glyph f", "scale-glyph"),
        ("move-group dx dy dz", "move-group"),
        ("scale-group s", "scale-group"),
        ("tint-group rrggbb", "tint-group"),
    ] {
        e.push(PaletteEntry::Template {
            title: title.to_string(),
            subtitle: format!("verb: {title} — fills the console for arguments"),
            fill: format!("{head} "),
        });
    }
    e.push(PaletteEntry::Template {
        title: "fly <substr>".to_string(),
        subtitle: "app: fly the camera to the first matching file — fills console".to_string(),
        fill: "fly ".to_string(),
    });
    for p in PanelId::ALL {
        e.push(cmd(
            &format!("panel {}", p.name()),
            format!("app: toggle the {} panel", p.name()),
            Command::TogglePanel(p),
        ));
    }
    e.push(cmd("shot", "app: capture the next presented frame (F2)".to_string(), Command::Screenshot));
    e.push(cmd("help", "app: log the console grammar".to_string(), Command::Help));
    e.push(cmd("clear", "app: wipe the command log".to_string(), Command::ClearLog));
    e
}

fn palette_entries(probe: Option<&UiProbeState>) -> Vec<PaletteEntry> {
    let mut entries = palette_catalog();
    if let Some(probe) = probe {
        for (i, row) in probe.files.iter().enumerate() {
            let dynst = probe.file_dyn.get(i).copied().unwrap_or_default();
            entries.push(PaletteEntry::File {
                title: format!("fly: {}", row.rel_path),
                center: dynst.center,
                half: dynst.half,
            });
        }
    }
    entries
}

// ── The frame build ──────────────────────────────────────────────────────────

/// Build one UI frame. Panels first (they claim dock edges), the palette
/// window last (it floats above). Returns the commands this frame enqueued —
/// the executor in windowed.rs runs them after the egui pass closes.
/// `root` is run_ui's root Ui: egui 0.36 panels show into a Ui.
pub fn draw_ui(root: &mut egui::Ui, input: &UiInput, state: &mut UiState) -> Vec<Command> {
    let mut out = Vec::new();
    if state.palette_open {
        // The palette is callable even with the shell hidden (⌘K re-opens the
        // shell — see windowed.rs), so draw it first regardless.
        palette_window(root.ctx(), input, state, &mut out);
        if !state.shell_open {
            return out;
        }
    }
    if !state.shell_open {
        return out;
    }
    hud(root, input, state, &mut out);
    if state.console_open {
        console_panel(root, state, &mut out);
    }
    if state.files_open {
        files_panel(root, input, state, &mut out);
    }
    if state.inspector_open {
        inspector_panel(root, input, state, &mut out);
    }
    out
}

/// Top strip: the always-reflected vitals + the panel toggles. Thin by design
/// — anything deep lives in a dock.
fn hud(root: &mut egui::Ui, input: &UiInput, state: &mut UiState, out: &mut Vec<Command>) {
    Panel::top("hud").show(root, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.strong("glyph3d-native");
            ui.separator();
            ui.label(format!("FPS {:.0}", input.fps));
            ui.label(format!("{} instances", input.instances));
            match input.probe.as_ref() {
                Some(p) => {
                    ui.label(format!("eye ({:.0},{:.0},{:.0})", p.eye[0], p.eye[1], p.eye[2]));
                    if p.cull_ranges > 0 || p.cull_backdrops > 0 {
                        ui.label(format!(
                            "cull {} rng / {} inst / {} bkdr",
                            p.cull_ranges, p.cull_instances, p.cull_backdrops
                        ));
                    }
                    if let Some(pick) = &p.last_pick {
                        ui.separator();
                        // Truncated — the full line lives in the Inspector.
                        let short: String = pick.chars().take(80).collect();
                        let short = if pick.chars().count() > 80 {
                            format!("{short}…")
                        } else {
                            short
                        };
                        ui.label(short);
                    }
                }
                None => {
                    ui.label("demo scene (no probe)");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label("⌘K palette");
                ui.menu_button("panels", |ui| {
                    for p in PanelId::ALL {
                        // The checkbox shows a local copy; the real flag flips
                        // exactly once, in the executor, via the command.
                        let mut open = panel_flag(state, p);
                        if ui.checkbox(&mut open, p.name()).changed() {
                            out.push(Command::TogglePanel(p));
                        }
                    }
                    ui.separator();
                    if ui.button("hide shell (F1)").clicked() {
                        state.shell_open = false;
                        state.palette_open = false;
                    }
                });
            });
        });
    });
}

fn panel_flag(state: &UiState, p: PanelId) -> bool {
    match p {
        PanelId::Files => state.files_open,
        PanelId::Inspector => state.inspector_open,
        PanelId::Console => state.console_open,
    }
}

/// Left dock: the K5 group browser as its own panel — filter, virtualized
/// rows, select + fly on click, hide/show through bus composition
/// (PickRowCol + Verb, exactly the CLI op-stream's targeting).
fn files_panel(root: &mut egui::Ui, input: &UiInput, state: &mut UiState, out: &mut Vec<Command>) {
    Panel::left("files").default_size(240.0).show(root, |ui| {
        ui.heading("files");
        match input.probe.as_ref() {
            None => {
                ui.label("no probe (demo scene)");
            }
            Some(probe) if probe.files.is_empty() => {
                ui.label("single-file scene — load a repo for the browser");
            }
            Some(probe) => {
                ui.horizontal(|ui| {
                    ui.label("filter:");
                    ui.text_edit_singleline(&mut state.files_filter);
                });
                let needle = state.files_filter.to_lowercase();
                let idxs: Vec<usize> = probe
                    .files
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        needle.is_empty() || r.rel_path.to_lowercase().contains(&needle)
                    })
                    .map(|(i, _)| i)
                    .collect();
                ui.label(format!("{} files (click = fly)", idxs.len()));
                let row_h = ui.text_style_height(&egui::TextStyle::Body);
                egui::ScrollArea::vertical().show_rows(ui, row_h, idxs.len(), |ui, range| {
                    for &i in &idxs[range] {
                        let row = &probe.files[i];
                        let dynst = probe.file_dyn.get(i).copied().unwrap_or_default();
                        let depth = row.rel_path.matches('/').count();
                        ui.horizontal(|ui| {
                            // Tint swatch drawn, not typeset (K5 precedent).
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 10.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                egui::Color32::from_rgb(dynst.tint[0], dynst.tint[1], dynst.tint[2]),
                            );
                            let name = row.rel_path.rsplit('/').next().unwrap_or(&row.rel_path);
                            let label = format!(
                                "{}{}{}",
                                "  ".repeat(depth),
                                name,
                                if dynst.hidden { "  (hidden)" } else { "" },
                            );
                            let resp = ui
                                .selectable_label(state.selected_group == Some(row.group_id), label)
                                .on_hover_text(&row.rel_path);
                            if resp.clicked() {
                                state.selected_group = Some(row.group_id);
                                out.push(Command::FlyFile(row.rel_path.clone()));
                            }
                            // Hide/show via the bus: pick the file's (0,0),
                            // then the group verb — the CLI's own targeting.
                            let toggle = if dynst.hidden { "show" } else { "hide" };
                            if ui.small_button(toggle).clicked() {
                                state.selected_group = Some(row.group_id);
                                out.push(Command::PickRowCol {
                                    file: row.rel_path.clone(),
                                    row: 0,
                                    col: 0,
                                });
                                out.push(Command::Verb(
                                    crate::parse_verb(if dynst.hidden {
                                        "show-group"
                                    } else {
                                        "hide-group"
                                    })
                                    .expect("panel verb literal"),
                                ));
                            }
                        });
                    }
                });
            }
        }
    });
}

/// Right dock: the pick inspector + the verb toolbox + the K4 live dial.
fn inspector_panel(root: &mut egui::Ui, input: &UiInput, state: &mut UiState, out: &mut Vec<Command>) {
    Panel::right("inspector").default_size(270.0).show(root, |ui| {
        ui.heading("inspector");
        let Some(probe) = input.probe.as_ref() else {
            ui.label("no probe (demo scene)");
            return;
        };
        ui.label(
            probe
                .last_pick
                .as_deref()
                .unwrap_or("pick: (none yet — left-click a glyph)"),
        );
        ui.separator();
        ui.label("verbs — same strings --verb parses:");
        ui.horizontal_wrapped(|ui| {
            for spec in ["tint-cycle", "hide-group", "show-group", "toggle-hidden"] {
                if ui.button(spec).clicked() {
                    out.push(Command::Verb(
                        crate::parse_verb(spec).expect("panel verb literal"),
                    ));
                }
            }
        });
        // Parameterized forms: compose the CLI line, parse it through the
        // same parser — equivalence by construction, and the log echo shows
        // the exact line (the grammar lesson again).
        ui.separator();
        ui.label("tint group:");
        ui.horizontal(|ui| {
            ui.color_edit_button_srgb(&mut state.tint_rgb);
            if ui.button("apply").clicked() {
                push_verb(
                    out,
                    state,
                    format!(
                        "tint-group {:02x}{:02x}{:02x}",
                        state.tint_rgb[0], state.tint_rgb[1], state.tint_rgb[2]
                    ),
                );
            }
        });
        ui.label("scale group:");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut state.scale_group_v)
                    .speed(0.05)
                    .range(0.1..=8.0),
            );
            if ui.button("apply").clicked() {
                push_verb(out, state, format!("scale-group {}", state.scale_group_v));
            }
        });
        ui.label("scale glyph:");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut state.scale_glyph_v)
                    .speed(0.05)
                    .range(0.1..=8.0),
            );
            if ui.button("apply").clicked() {
                push_verb(out, state, format!("scale-glyph {}", state.scale_glyph_v));
            }
        });
        ui.label("nudge glyph:");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut state.nudge_step)
                    .speed(0.05)
                    .range(0.05..=8.0),
            );
            for (label, d) in [
                ("←", [-state.nudge_step, 0.0, 0.0]),
                ("→", [state.nudge_step, 0.0, 0.0]),
                ("↓", [0.0, -state.nudge_step, 0.0]),
                ("↑", [0.0, state.nudge_step, 0.0]),
            ] {
                if ui.small_button(label).clicked() {
                    push_verb(
                        out,
                        state,
                        format!("nudge-glyph {} {} {}", d[0], d[1], d[2]),
                    );
                }
            }
        });
        // K4's live dial, verbatim: the slider writes the shared probe cell
        // the scene applies before culling; offscreen keeps the compile-time
        // consts (no probe installed there). Continuous tuning, not a command.
        if let Some(cell) = &input.probe_cell {
            ui.separator();
            ui.label("cull/LOD — live, windowed only:");
            ui.add(
                egui::Slider::new(&mut cell.borrow_mut().lod_min_px, 0.25..=16.0)
                    .logarithmic(true)
                    .text("LOD_MIN_PX px/em (const 1.0)"),
            );
        }
        ui.label(format!(
            "cull: {} draw ranges, {} instances | {} backdrops",
            probe.cull_ranges, probe.cull_instances, probe.cull_backdrops
        ));
    });
}

/// Compose a verb line, parse it through the CLI parser, enqueue or log the
/// parse error (which for these forms is a programming slip, not user input —
/// but the grammar stays the single source of truth either way).
fn push_verb(out: &mut Vec<Command>, state: &mut UiState, spec: String) {
    match crate::parse_verb(&spec) {
        Ok(v) => out.push(Command::Verb(v)),
        Err(e) => state.log(LogKind::Error, e),
    }
}

// ── Console ──────────────────────────────────────────────────────────────────

fn console_panel(root: &mut egui::Ui, state: &mut UiState, out: &mut Vec<Command>) {
    Panel::bottom("console")
        .default_size(150.0)
        .min_size(64.0)
        .show(root, |ui| {
            ui.label("console — verbs + help|clear|shot|panel|fly (↑/↓ history)");
            let row_h = ui.text_style_height(&egui::TextStyle::Body);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height((ui.available_height() - row_h * 1.8).max(40.0))
                .show(ui, |ui| {
                    for line in &state.log {
                        let color = match line.kind {
                            LogKind::Cmd => egui::Color32::from_rgb(150, 200, 255),
                            LogKind::Result => egui::Color32::from_gray(220),
                            LogKind::Info => egui::Color32::from_gray(185),
                            LogKind::Error => egui::Color32::from_rgb(255, 120, 120),
                        };
                        ui.colored_label(color, &line.text);
                    }
                });
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.console_input).hint_text("> verb / app command"),
            );
            // History recall. Arrows also move the caret inside the field —
            // recall wins because a single-line field has nowhere for the
            // caret to go; the tradeoff is noted and accepted.
            let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
            let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
            if up && !state.console_history.is_empty() {
                let pos =
                    state.history_pos.map_or(state.console_history.len(), |p| p.saturating_sub(1));
                state.console_input = state.console_history[pos].clone();
                state.history_pos = Some(pos);
            } else if down {
                if let Some(p) = state.history_pos {
                    if p + 1 < state.console_history.len() {
                        state.console_input = state.console_history[p + 1].clone();
                        state.history_pos = Some(p + 1);
                    } else {
                        state.console_input.clear();
                        state.history_pos = None;
                    }
                }
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let line = std::mem::take(&mut state.console_input);
                state.history_pos = None;
                if !line.trim().is_empty() {
                    state.console_history.push(line.clone());
                    match parse_console_line(&line) {
                        Ok(cmds) => out.extend(cmds),
                        Err(e) => state.log(LogKind::Error, e),
                    }
                }
                resp.request_focus();
            }
        });
}

// ── Palette ──────────────────────────────────────────────────────────────────

fn palette_window(ctx: &egui::Context, input: &UiInput, state: &mut UiState, out: &mut Vec<Command>) {
    let mut open = state.palette_open;
    egui::Window::new("palette")
        .id(egui::Id::new("stage_m_palette"))
        .open(&mut open)
        .default_pos(egui::pos2(420.0, 180.0))
        .default_width(520.0)
        .show(ctx, |ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.palette_query)
                    .hint_text("verbs, files (fly:), app commands…"),
            );
            if !resp.has_focus() {
                resp.request_focus();
            }
            let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
            let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
            let tab = ui.input(|i| i.key_pressed(egui::Key::Tab));
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));

            let entries = palette_entries(input.probe.as_ref());
            let mut matched: Vec<(i32, &PaletteEntry)> = entries
                .iter()
                .filter_map(|e| fuzzy_score(&state.palette_query, e.title()).map(|s| (s, e)))
                .collect();
            matched.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            state.palette_selected = state.palette_selected.min(matched.len().saturating_sub(1));
            if up {
                state.palette_selected = state.palette_selected.saturating_sub(1);
            }
            if down && state.palette_selected + 1 < matched.len() {
                state.palette_selected += 1;
            }

            let row_h = ui.text_style_height(&egui::TextStyle::Body);
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show_rows(ui, row_h * 2.0, matched.len(), |ui, range| {
                    let base = range.start;
                    for (vi, (_, entry)) in matched[range].iter().enumerate() {
                        let gi = base + vi;
                        let selected = gi == state.palette_selected;
                        let title = if selected {
                            format!("▶ {}", entry.title())
                        } else {
                            entry.title().to_string()
                        };
                        let resp = ui.selectable_label(selected, title);
                        // RichText carries the small+gray styling (egui 0.36:
                        // Label has no text_color; RichText does).
                        ui.add(egui::Label::new(
                            egui::RichText::new(entry.subtitle())
                                .small()
                                .color(egui::Color32::from_gray(140)),
                        ));
                        let fired = resp.clicked() || (enter && selected);
                        if tab && selected {
                            // Complete into the console: the palette is the
                            // tutor; the console finishes the thought.
                            if let PaletteEntry::Template { fill, .. } = entry {
                                state.console_input = fill.clone();
                                state.palette_open = false;
                                state.palette_query.clear();
                                state.palette_selected = 0;
                            }
                        } else if fired {
                            match entry {
                                PaletteEntry::Cmd { cmd, .. } => out.push(cmd.clone()),
                                PaletteEntry::Template { fill, .. } => {
                                    state.console_input = fill.clone();
                                }
                                PaletteEntry::File { center, half, .. } => {
                                    out.push(Command::FlyFileBox {
                                        center: *center,
                                        half: *half,
                                    });
                                }
                            }
                            state.palette_open = false;
                            state.palette_query.clear();
                            state.palette_selected = 0;
                        }
                    }
                });
        });
    if !open {
        state.palette_open = false;
    }
}

// ── Tests (pure logic only — no GPU, no egui context needed) ─────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_parses_verbs_through_the_cli_grammar() {
        let cmds = parse_console_line("tint-cycle").expect("verb line parses");
        assert!(matches!(&cmds[..], [Command::Verb(Verb::TintCycle)]));
        let cmds = parse_console_line("nudge-glyph 1 2 0.5").expect("verb line parses");
        assert!(matches!(
            &cmds[..],
            [Command::Verb(Verb::NudgeGlyph(d))] if *d == [1.0, 2.0, 0.5]
        ));
    }

    #[test]
    fn console_parses_app_commands() {
        assert!(matches!(&parse_console_line("help").unwrap()[..], [Command::Help]));
        assert!(matches!(&parse_console_line("clear").unwrap()[..], [Command::ClearLog]));
        assert!(matches!(&parse_console_line("shot").unwrap()[..], [Command::Screenshot]));
        assert!(matches!(
            &parse_console_line("panel files").unwrap()[..],
            [Command::TogglePanel(PanelId::Files)]
        ));
        let cmds = parse_console_line("fly alpha.rs").expect("fly parses");
        match &cmds[..] {
            [Command::FlyFile(f)] => assert_eq!(f, "alpha.rs"),
            other => panic!("expected FlyFile, got {} entries", other.len()),
        }
        // Blank is a no-op, not an error.
        assert!(parse_console_line("   ").unwrap().is_empty());
    }

    #[test]
    fn console_rejects_unknown_and_malformed() {
        // Verb carries no Debug (house style), so .err() rather than
        // expect_err (which would need it on the Ok type).
        let e = parse_console_line("frobnicate").err().expect("unknown must fail");
        assert!(e.contains("unknown command"), "{e}");
        let e = parse_console_line("panel nonsense").err().expect("bad panel must fail");
        assert!(e.contains("files"), "{e}");
        let e = parse_console_line("nudge-glyph 1").err().expect("bad verb args must fail");
        assert!(e.contains("float"), "{e}");
        let e = parse_console_line("fly").err().expect("fly needs a substr");
        assert!(e.contains("substring"), "{e}");
    }

    #[test]
    fn fuzzy_matches_rank_and_reject() {
        // Subsequence matches; contiguous + word-start outranks scattered.
        let a = fuzzy_score("fly", "fly: src/main.rs").expect("prefix matches");
        let b = fuzzy_score("fl", "files").expect("scattered subsequence matches");
        assert!(a > b, "contiguous should outrank scattered ({a} vs {b})");
        assert!(fuzzy_score("zzz", "src/main.rs").is_none(), "no subsequence = no match");
        assert_eq!(fuzzy_score("", "anything").unwrap(), 0, "empty query matches all");
        // Case-insensitive.
        assert!(fuzzy_score("MAIN", "src/main.rs").is_some());
    }

    #[test]
    fn fly_framing_fits_the_box() {
        // Mirror of the K5 formula (moved here unchanged): a 6x4 box (half
        // 3x2) in a 1.6-aspect viewport fits vertically — 3/1.6 < 2.
        let (eye, yaw, pitch) = fly_to_framing([0.0, 0.0], [3.0, 2.0], 1.6);
        let expected = 2.0f32 / (crate::glyph_scene::FOV_Y.to_radians() * 0.5).tan() * 1.08 + 2.0;
        assert_eq!(eye, [0.0, 0.0, expected]);
        assert_eq!((yaw, pitch), (0.0, 0.0));
        // A wide box in a square viewport fits by width instead: half 8 > 1.
        let (eye2, _, _) = fly_to_framing([0.0, 0.0], [8.0, 1.0], 1.0);
        let expected2 = 8.0f32 / (crate::glyph_scene::FOV_Y.to_radians() * 0.5).tan() * 1.08 + 2.0;
        assert_eq!(eye2, [0.0, 0.0, expected2]);
    }

    #[test]
    fn format_command_echoes_the_console_grammar() {
        assert_eq!(
            format_command(&Command::Verb(crate::parse_verb("tint-cycle").expect("literal"))),
            "verb tint-cycle"
        );
        assert_eq!(format_command(&Command::FlyFile("a.rs".into())), "fly a.rs");
        assert_eq!(
            format_command(&Command::PickRowCol { file: "a.rs".into(), row: 3, col: 7 }),
            "pick a.rs:3:7 (--pick-file a.rs --pick-row 3 --pick-col 7)"
        );
    }

    #[test]
    fn palette_catalog_verb_literals_parse() {
        // The zero-arg verbs the catalog/panels fire bare must parse bare…
        for spec in ["tint-cycle", "hide-group", "show-group", "toggle-hidden"] {
            crate::parse_verb(spec).unwrap_or_else(|e| panic!("verb literal {spec}: {e}"));
        }
        // …and the parameterized ones parse with their template's arg shape
        // (a bare word is correctly rejected — that is the grammar).
        assert!(crate::parse_verb("nudge-glyph").is_err());
        for spec in ["nudge-glyph 0 0", "scale-glyph 1", "move-group 0 0 0", "scale-group 1", "tint-group ff8040"] {
            crate::parse_verb(spec).unwrap_or_else(|e| panic!("verb literal {spec}: {e}"));
        }
    }

    #[test]
    fn log_ring_caps_at_500() {
        let mut s = UiState::default();
        for i in 0..600 {
            s.log(LogKind::Info, format!("line {i}"));
        }
        assert_eq!(s.log.len(), LOG_CAP);
        assert_eq!(s.log.last().expect("non-empty").text, "line 599");
        assert_eq!(s.log.first().expect("non-empty").text, "line 100");
    }
}
