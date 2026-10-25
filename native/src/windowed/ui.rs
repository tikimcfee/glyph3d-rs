//! Stage K — the egui overlay state and the Debug panel's verb table.
//! Extracted from `windowed.rs` in the 2026-09 code-shape refactor — a pure
//! move; `pub(super)` stands in for the same-module privacy these items had
//! (the window's render drives every field). Since 2026-10-24 the egui
//! plumbing (`EguiUi`) and what the panels remember (`UiState`) are two
//! structs: the panels take `UiState` alone.

/// Stage K: the egui overlay — context, winit event translation, and the
/// wgpu painter. `None` in WindowState under `--no-ui`; the type (and all
/// egui code below) is compiled out entirely without the `egui-ui` feature.
#[cfg(feature = "egui-ui")]
pub(super) struct EguiUi {
    pub(super) ctx: egui::Context,
    pub(super) state: egui_winit::State,
    pub(super) renderer: egui_wgpu::Renderer,
    /// Everything the panels remember between frames.
    pub(super) shell: UiState,
}

/// What the panels remember between frames: which windows are open, what
/// the filters hold, the scanned sessions. All UI memory in one struct, the
/// thing a session file would one day save.
#[cfg(feature = "egui-ui")]
pub(super) struct UiState {
    /// The console window's visibility (F9 toggles; its close button clears
    /// it). The console replaced the K3 scratch field as the K2
    /// typing-isolation trap (typing WASD/h/g/t/x there must not fly the
    /// camera or fire verbs; `app::route_key` ensures it).
    pub(super) console_open: bool,
    /// The console's input line and history.
    pub(super) console: super::panels::console::Console,
    /// Every line the command bus ran, with its replies (the console shows it).
    pub(super) log: super::panels::console::CommandLog,
    /// K4: Debug window visibility (F1 toggles; the window's own close
    /// button clears it).
    pub(super) debug_open: bool,
    /// K5: group-browser substring filter (doubles as a second K2
    /// typing-isolation field).
    pub(super) filter: String,
    /// K5: the browser's selected group (highlight only; click also flies
    /// the camera to the file).
    pub(super) selected_group: Option<u32>,
    /// Agent session browser window visibility (F7 toggles; the window's close button clears it).
    pub(super) session_browser_open: bool,
    /// Agent session browser search query.
    pub(super) session_filter_text: String,
    /// Agent session browser harness filter tab.
    pub(super) session_filter_harness: crate::agent_transcript::discovery::SessionHarnessFilter,
    /// Cached list of discovered agent sessions.
    pub(super) discovered_sessions: Option<Vec<crate::agent_transcript::discovery::DiscoveredSession>>,
    /// The field HUD (F8 toggles): the always-on top-left readout of what
    /// the glyph field is and what it did this frame. Independent of the
    /// Debug window; open by default in `--field-mode visible`.
    pub(super) hud_open: bool,
    /// What the last scan looked at, and why: one line from `glyph-session-dirs`.
    pub(super) session_dirs_report: String,
    /// Text `copy-pose` put on the clipboard, handed to egui inside the next
    /// pass (the executor runs between passes).
    pub(super) pending_clipboard: Option<String>,
}

#[cfg(feature = "egui-ui")]
impl UiState {
    /// The state a window opens with: the Debug window shown, the field HUD
    /// where it has the most to say (`--field-mode visible`).
    pub(super) fn new(hud_open: bool) -> Self {
        Self {
            console_open: false,
            console: Default::default(),
            log: Default::default(),
            debug_open: true,
            filter: String::new(),
            selected_group: None,
            session_browser_open: false,
            session_filter_text: String::new(),
            session_filter_harness: crate::agent_transcript::discovery::SessionHarnessFilter::All,
            discovered_sessions: None,
            hud_open,
            session_dirs_report: String::new(),
            pending_clipboard: None,
        }
    }
}

/// Stage K (K3): Debug-panel verb buttons — each fires its own text as a
/// command-bus line, which the bus parses as `--verb <text>` through the
/// CLI's own definition, so panel ⇔ CLI equivalence is by construction.
/// Zero-arg/default forms only; the
/// parameterized verbs (nudge/scale/move, tint-group rrggbb) stay CLI-only
/// until a phase needs arg entry.
#[cfg(feature = "egui-ui")]
pub(super) const PANEL_VERBS: &[&str] = &[
    "recolor-glyph",
    "recolor-line",
    "tint-cycle",
    "hide-group",
    "show-group",
    "toggle-hidden",
];
