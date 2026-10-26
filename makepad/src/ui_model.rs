//! The panels' model: what each makepad panel reads (`G3UiState`) and the
//! actions it emits (`G3UiAction`). Plain data, std only, no makepad and no
//! engine types, so the widget definitions compile against it alone and the
//! host (`ui_adapter`) is the one place that knows the engine. Selectors
//! carry their options as data (`G3Choice`): a panel draws the labels and
//! emits the chosen index; which engine value an index means is the
//! adapter's business. The egui panels (`native/src/windowed/state.rs`)
//! are the spec: the same reads, the same writes, the same words.
//! Written 2026-10-26 for the makepad host.

use std::rc::Rc;

/// One option of a selector: its label, and whether it is the current one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct G3Choice {
    pub label: String,
    pub selected: bool,
}

/// Everything the four panels read, rebuilt by the host once per frame
/// from the scene's probe, the carrel and the session list.
#[derive(Clone, Debug, Default)]
pub struct G3UiState {
    /// Frames per second over the last second.
    pub fps: f32,
    /// F8, F1 and F7: which panels are open. The carrel panel shows
    /// whenever `carrel` is Some.
    pub hud_open: bool,
    pub debug_open: bool,
    pub sessions_open: bool,
    /// The field HUD (F8), one monospace line each: field and engine, the
    /// segment cull, the field's own counters, the debug tint, the
    /// selection. Empty when the scene has no probe.
    pub hud_lines: Vec<String>,
    /// The Debug panel (F1). None when the scene has no probe (the quad
    /// demo): the panel says so and shows only its session button.
    pub debug: Option<G3DebugState>,
    /// The agent carrel panel; None when no carrel is loaded.
    pub carrel: Option<G3CarrelUiState>,
    /// The agent sessions browser (F7).
    pub sessions: G3SessionsState,
}

/// The Debug panel (F1), top to bottom as the egui window draws it.
#[derive(Clone, Debug, Default)]
pub struct G3DebugState {
    /// `camera: Fly eye=(..) yaw=.. pitch=.. rad (..°, ..°)`.
    pub camera_line: String,
    /// `engine: hyper | field: Derived`; None when neither is known.
    pub engine_line: Option<String>,
    /// The last pick as the stdout pick line, or the hint to make one.
    pub pick_line: String,
    /// `carrel / zone: <id>` when the pick is in one.
    pub zone_line: Option<String>,
    /// `ACTIVE GRAB: ...` while a file or a carrel is grabbed (drawn in
    /// `grab_color`).
    pub grab_line: Option<String>,
    pub grab_color: [u8; 3],
    /// The carrel grab button's label (`Grab Entire Carrel (C)` or
    /// `Release Carrel '<id>' (C)`); None hides it. Emits
    /// `ToggleCarrelGrab`.
    pub carrel_grab_button: Option<String>,
    /// The file grab button's label (`Grab File (G)` or `Release File #<n>
    /// (G)`); None hides it. Emits `ToggleFileGrab`.
    pub file_grab_button: Option<String>,
    /// The verb buttons, each a CLI `--verb` literal: the label IS the
    /// verb. Emits `Verb(label)`.
    pub verbs: Vec<String>,
    /// `Clear Pick` shows while something is picked. Emits `ClearPick`.
    pub can_clear_pick: bool,
    /// The environment toggle's label (`environment: Off (B for ground)`
    /// ...). Emits `ToggleEnvironment` (the scene's B key: Off and Ground
    /// are the only modes).
    pub environment_label: String,
    pub lod: G3LodState,
    /// The layout dial (repo and text scenes): the wrap staircase's pitch,
    /// x em, 0.0..=1.0. None hides the section. Emits `SetZWrapSpacing` on
    /// RELEASE only (a rebuild per tick would make it unusable).
    pub z_wrap_spacing: Option<f64>,
    /// `field depth: z ∈ [lo, hi] world units` under the dial.
    pub z_extent_line: Option<String>,
    /// Layout engine (repo scenes): empty hides it. Emits `SetStrategy(i)`.
    pub strategies: Vec<G3Choice>,
    /// Glyph field mode: empty hides it. Emits `SetFieldMode(i)`. The
    /// selected one is the mode the scene BUILT (a lane-limit fallback
    /// shows here).
    pub field_modes: Vec<G3Choice>,
    /// The sequence pass toggle's label (`cluster mode: ON` / `off`); None
    /// hides it. Emits `ToggleCluster`.
    pub cluster_label: Option<String>,
    /// The canvas layout toggle's label (`layout mode: SHELF (click for
    /// carrel)` ...); None hides it. Emits `ToggleLayout`.
    pub layout_label: Option<String>,
    /// The file browser's rows, static for the scene's life (an `Rc`, so a
    /// per-frame state clone is a refcount). Empty hides the browser.
    pub files: Rc<Vec<G3FileRow>>,
    /// Per-row live state, parallel to `files`; fresh for the rows the
    /// browser reported drawing last frame (`BrowserRows`), stale (but
    /// harmless) for the rest.
    pub file_dyn: Vec<G3FileDyn>,
    /// The selected row's group (highlight).
    pub selected_group: Option<u32>,
}

/// The live LOD and cull controls. Every threshold is px per text row.
#[derive(Clone, Debug, Default)]
pub struct G3LodState {
    /// "Text detail", 2..=32: rows this tall get full curve detail.
    pub text_detail_px: f32,
    /// "Show glyphs", 0.25..=16 (log): rows this tall become glyphs.
    pub show_glyphs_px: f32,
    /// The visible field's "File rectangle", 0.05..=4 (log); None outside
    /// visible mode (hide the slider).
    pub file_rectangle_px: Option<f32>,
    /// Whether "Show glyphs" says "a line is a wash" (visible mode) or "a
    /// file is its rectangle".
    pub visible_mode: bool,
    /// Greeking on, and pure (hard cut) rather than smooth.
    pub greeking: bool,
    pub greek_pure: bool,
    pub file_backgrounds: bool,
    /// Unmultiplied RGBA, 0..1.
    pub file_bg_color: [f32; 4],
    /// The visible field's debug tint (`off`, `by LOD tier`, `by cull
    /// state`); empty outside visible mode. Emits `SetDebugTint(i)`.
    pub debug_tints: Vec<G3Choice>,
    /// `cull: N draw ranges, N instances | N backdrops`.
    pub cull_line: String,
}

/// One file browser row.
#[derive(Clone, Debug, Default)]
pub struct G3FileRow {
    pub rel_path: String,
    /// Directory depth (the egui list indents two spaces per level).
    pub depth: usize,
    pub group_id: u32,
}

impl G3FileRow {
    /// The browser filter: a case-insensitive substring of the path.
    /// `needle_lower` is the filter text, lowercased once by the caller.
    pub fn matches(&self, needle_lower: &str) -> bool {
        needle_lower.is_empty() || self.rel_path.to_lowercase().contains(needle_lower)
    }
}

/// A file row's live state.
#[derive(Clone, Copy, Debug, Default)]
pub struct G3FileDyn {
    pub hidden: bool,
    /// The group tint, sRGB bytes (the swatch).
    pub tint: [u8; 3],
}

/// The agent carrel panel.
#[derive(Clone, Debug, Default)]
pub struct G3CarrelUiState {
    pub session_id: String,
    /// `Beat 3 / 12 (T2)` or `Turn 2 / 5`.
    pub position_label: String,
    /// `Window: [a..b] of n`.
    pub window_label: String,
    /// `(-K back in history)` in `history_back_color`, or `(LIVE /
    /// LATEST)` in `history_live_color`.
    pub history_label: String,
    pub history_color: [u8; 3],
    /// The four dials, each applied on RELEASE as one
    /// `SetCarrelOptions` (with the other three as they are).
    pub options: G3CarrelOptions,
    /// The time scrolls' upper bounds (their lower bound is 0).
    pub max_deck_scroll: usize,
    pub max_desk_scroll: usize,
    /// The active beat's summary (in `beat_color`) and the turn's prompt;
    /// empty hides each.
    pub beat_summary: String,
    pub beat_color: [u8; 3],
    pub prompt_summary: String,
    /// `path (active: R2, total: 5)`, one per workdesk file.
    pub touched_files: Vec<String>,
}

/// The carrel's deck and desk dials. Ranges: deck window 5..=100 cards,
/// deck scroll 0..=`max_deck_scroll` turns back, desk revisions 3..=50 per
/// file, desk scroll 0..=`max_desk_scroll` revisions back, file stacks
/// 5..=50.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct G3CarrelOptions {
    pub deck_window_limit: usize,
    pub deck_scroll_offset: usize,
    pub desk_revision_limit: usize,
    pub desk_scroll_offset: usize,
    pub max_file_stacks: usize,
}

/// The agent sessions browser (F7). The host owns the list, the filter
/// and the harness tab; the rows arrive filtered, parents before their
/// subagents.
#[derive(Clone, Debug, Default)]
pub struct G3SessionsState {
    /// The search text as the host holds it (echo it into the input).
    pub filter: String,
    /// `All`, then one per harness. Emits `SetSessionHarness(i)`.
    pub harness_tabs: Vec<G3Choice>,
    /// `Scanning: <dirs>`, or why gomuxai could not say (`scanning_error`
    /// true: draw it in red).
    pub scanning_line: String,
    pub scanning_error: bool,
    /// Why the list could not be read, or why the last load failed (red).
    pub error: Option<String>,
    /// `Showing N of M sessions`; None when there is no list.
    pub count_line: Option<String>,
    /// What to say when no row shows: no sessions at all (where to point
    /// gomuxai) or none matching.
    pub empty_line: Option<String>,
    pub rows: Vec<G3SessionEntry>,
}

/// One session row.
#[derive(Clone, Debug, Default)]
pub struct G3SessionEntry {
    /// What `LoadSession` takes back.
    pub path: String,
    /// Indent under its parent (`└ subagent`).
    pub is_subagent: bool,
    /// `[Claude]`, `[Antigravity]`, `[Kimi]` or the harness's own name, in
    /// `harness_color`.
    pub harness_tag: String,
    pub harness_color: [u8; 3],
    /// The project (cwd's last element), in `project_color`; may be empty.
    pub project: String,
    pub project_color: [u8; 3],
    pub status: String,
    /// `just now`, `5m ago`, ...; empty when the mtime is unknown.
    pub age: String,
    /// The title, or `(untitled)`.
    pub title: String,
    /// `ID: <first 16 chars>…`.
    pub id_line: String,
    /// `12.3 KB`.
    pub size_line: String,
}

/// Which panel a toggle names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum G3Panel {
    Hud,
    Debug,
    Sessions,
}

/// Everything a panel can ask of the host. Index-carrying actions index the
/// matching `G3Choice` list (or `files`) of the state they were drawn from.
#[derive(Clone, Debug, PartialEq)]
pub enum G3UiAction {
    TogglePanel(G3Panel),
    /// Print the camera as the `--cam-pose` argument that reproduces it,
    /// and put it on the clipboard.
    CopyPose,
    ToggleCarrelGrab,
    ToggleFileGrab,
    /// A verb button: the CLI literal, parsed like `--verb`.
    Verb(String),
    ClearPick,
    ToggleEnvironment,
    // ── live LOD/cull controls, applied next frame ──
    SetTextDetailPx(f32),
    SetShowGlyphsPx(f32),
    SetFileRectanglePx(f32),
    SetGreeking(bool),
    SetGreekPure(bool),
    SetFileBackgrounds(bool),
    SetFileBgColor([f32; 4]),
    SetDebugTint(usize),
    // ── layout: each one rebuilds the scene (pick/selection reset) ──
    SetZWrapSpacing(f64),
    SetStrategy(usize),
    SetFieldMode(usize),
    ToggleCluster,
    ToggleLayout,
    SetCarrelOptions(G3CarrelOptions),
    // ── the file browser ──
    /// The `files` indices the browser drew this frame.
    BrowserRows(Vec<usize>),
    /// A row clicked: select its file and fly to it, or clear the pick if
    /// it was already selected. The index is into `files`.
    PickFile(usize),
    // ── the carrel transport (the scene's own keys). One step: the scene
    // has no separate turn and beat steps (`,` `.` `[` `]` `p` `n` and the
    // arrows all step the same way) ──
    CarrelOldest,
    CarrelPrev,
    CarrelNext,
    CarrelLatest,
    CarrelMode,
    // ── the sessions browser ──
    RefreshSessions,
    SetSessionFilter(String),
    SetSessionHarness(usize),
    LoadSession(String),
}
