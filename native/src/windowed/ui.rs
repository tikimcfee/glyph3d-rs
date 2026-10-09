//! Stage K — the egui overlay state and the Debug panel's verb table.
//! Extracted from `windowed.rs` in the 2026-09 code-shape refactor — a pure
//! move; `pub(super)` stands in for the same-module privacy these items had
//! (the window's render drives every field).

/// Stage K: the egui overlay — context, winit event translation, and the
/// wgpu painter. `None` in WindowState under `--no-ui`; the type (and all
/// egui code below) is compiled out entirely without the `egui-ui` feature.
#[cfg(feature = "egui-ui")]
pub(super) struct EguiUi {
    pub(super) ctx: egui::Context,
    pub(super) state: egui_winit::State,
    pub(super) renderer: egui_wgpu::Renderer,
    /// K3: scratch text field in the Debug window — doubles as the K2
    /// typing-isolation trap test (typing WASD/h/g/t/x here must not fly the
    /// camera or fire verbs; the gating matrix in window_event ensures it).
    pub(super) scratch: String,
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
    /// What the last scan looked at, and why: one line from `glyph-session-dirs`.
    pub(super) session_dirs_report: String,
}

/// Stage K (K3): Debug-panel verb buttons — CLI `--verb` literals parsed
/// through the same crate::parse_verb the CLI uses, so panel ⇔ CLI
/// equivalence is by construction. Zero-arg/default forms only; the
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
