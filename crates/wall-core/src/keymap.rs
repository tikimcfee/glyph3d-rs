//! The wall's chord table, plain keys, pointer gestures, help sections, and
//! pure keyboard dispatch decision.
//!
//! Ports `wall/wallKeymap.js` (`KEYMAP`, `chordFor`, `chordText`, `PLAIN_KEYS`,
//! `gestures`, `HELP_SECTIONS`), the pure section/search builders from
//! `wall/HelpPanel.jsx` (`helpSections`, `searchHelp`), and the keyboard
//! routing decision in `wall/WallKeys.js` (together with `keyEventFor` and
//! `OPTION_AS_ALT` from `packages/glyph3d-core/src/services/interaction/keyEncoding.js`),
//! without any window, DOM, or terminal grid.

use std::collections::HashMap;

use crate::chord::{
    alt_label, block_select_label, chord_label, copy_label, focus_history_dir,
    focus_history_label, group_cycle_dir, group_cycle_label, is_chord, is_copy_chord,
    is_paste_chord, snap_invert_label, zoom_wheel_label, zoom_wheel_label_keys, CycleDir,
    ZoomKey,
};
use crate::input::{KeyEvent, Platform};

/// Whether a terminal or group currently fills the screen (`fullscreen`) or the
/// open canvas is active (`canvas`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KeyMode {
    #[default]
    Canvas,
    Fullscreen,
}

impl KeyMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyMode::Canvas => "canvas",
            KeyMode::Fullscreen => "fullscreen",
        }
    }
}

/// Section identifier shared by [`KEYMAP`], [`plain_keys`], [`gestures`], and [`HELP_SECTIONS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionId {
    Terminals,
    Fullscreen,
    Text,
    Splits,
    Groups,
    Canvas,
    Panels,
}

impl SectionId {
    pub const fn as_str(self) -> &'static str {
        match self {
            SectionId::Terminals => "terminals",
            SectionId::Fullscreen => "fullscreen",
            SectionId::Text => "text",
            SectionId::Splits => "splits",
            SectionId::Groups => "groups",
            SectionId::Canvas => "canvas",
            SectionId::Panels => "panels",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            SectionId::Terminals => "Terminals",
            SectionId::Fullscreen => "Fullscreen",
            SectionId::Text => "Text",
            SectionId::Splits => "Splits",
            SectionId::Groups => "Groups",
            SectionId::Canvas => "Canvas",
            SectionId::Panels => "Panels",
        }
    }
}

/// Cardinal direction for focusing or moving panes (`left`, `right`, `up`, `down`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneDir {
    Left,
    Right,
    Up,
    Down,
}

impl PaneDir {
    pub const fn as_str(self) -> &'static str {
        match self {
            PaneDir::Left => "left",
            PaneDir::Right => "right",
            PaneDir::Up => "up",
            PaneDir::Down => "down",
        }
    }
}

/// Direction in which `wall.split` cuts the focused terminal (`right` or `down`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SplitDir {
    Right,
    Down,
}

impl SplitDir {
    pub const fn as_str(self) -> &'static str {
        match self {
            SplitDir::Right => "right",
            SplitDir::Down => "down",
        }
    }
}

/// Text-size adjustment inside fullscreen mode (`wall.fullscreen.text`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FullscreenTextStep {
    Bigger,
    Smaller,
    Reset,
}

impl FullscreenTextStep {
    pub const fn as_str(self) -> &'static str {
        match self {
            FullscreenTextStep::Bigger => "bigger",
            FullscreenTextStep::Smaller => "smaller",
            FullscreenTextStep::Reset => "reset",
        }
    }
}

/// Declarative action attached to an entry in [`KEYMAP`] (`WallChord.run` in `wallKeymap.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChordAction {
    /// Focus the next (`+1`) or previous (`-1`) pane in reading order.
    CycleFocus(CycleDir),
    /// Toggle fullscreen on the focused group/terminal (or exit active fullscreen);
    /// when neither fullscreen nor focused, fit every terminal in view.
    ToggleFullscreenOrFit,
    /// Toggle single-pane fullscreen when fullscreen is active or a terminal is focused.
    TogglePaneFullscreen,
    /// Adjust text size of the fullscreen terminal or group.
    FullscreenText(FullscreenTextStep),
    /// Spawn a new terminal.
    Spawn,
    /// Toggle the focused terminal's Agent View.
    ToggleAgentView,
    /// Hide the focused terminal.
    HideFocused,
    /// Hold to close the focused terminal (starts the hold ring on non-repeat press).
    HoldCloseFocused,
    /// Split the focused terminal right or down.
    Split(SplitDir),
    /// Focus the pane (or canvas window) in `PaneDir`.
    FocusPane(PaneDir),
    /// Move the focused pane or tiled member a step in `PaneDir` within its group.
    MovePane(PaneDir),
    /// Group the wall selection, or the focused terminal when nothing is selected.
    GroupSelectionOrFocused,
    /// Ungroup the focused terminal's group.
    UngroupFocused,
    /// Fit every shown terminal in view.
    FitAll,
    /// Toggle grid snapping (`wall.snap toggle`).
    ToggleSnap,
    /// Toggle grid visibility (`wall.grid toggle`).
    ToggleGrid,
    /// Toggle the sidebar (`wall.sidebar toggle`).
    ToggleSidebar,
    /// Toggle the termd status window (`wall.status toggle`).
    ToggleStatus,
    /// Toggle the settings panel (`wall.settings toggle`).
    ToggleSettings,
    /// Toggle the help panel (`wall.help toggle`).
    ToggleHelp,
}

/// One chord in [`KEYMAP`] (`WallChord` in `wallKeymap.js`).
///
/// Chords match on physical `code` so keyboard layouts never rewrite them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallChord {
    pub section: SectionId,
    /// Physical key (`KeyboardEvent.code`), e.g. `"BracketRight"`, `"Enter"`, `"KeyD"`.
    pub code: &'static str,
    /// `Some(true)` if Shift must be held, `Some(false)` if Shift must not be held,
    /// or `None` if Shift does not matter.
    pub shift: Option<bool>,
    /// `Some(mode)` if the chord applies only in that mode, or `None` if it applies in both.
    pub mode: Option<KeyMode>,
    /// Key glyph shown in the help panel (`"]"`, `"↵"`, `"0"`, `"K"`).
    pub key: &'static str,
    /// Description shown in the help panel.
    pub does: &'static str,
    /// Handler action for `WallKeys`, or `None` when another UI layer owns the chord (`KeyK`: palette).
    pub action: Option<ChordAction>,
}

/// The wall's complete chord table (`KEYMAP` in `wallKeymap.js`), in help-panel order.
pub const KEYMAP: &[WallChord] = &[
    WallChord {
        section: SectionId::Terminals,
        code: "BracketRight",
        shift: Some(false),
        mode: None,
        key: "]",
        does: "focus the next pane (reading order)",
        action: Some(ChordAction::CycleFocus(CycleDir::Next)),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "BracketLeft",
        shift: Some(false),
        mode: None,
        key: "[",
        does: "focus the previous pane (reading order)",
        action: Some(ChordAction::CycleFocus(CycleDir::Prev)),
    },
    WallChord {
        section: SectionId::Fullscreen,
        code: "Enter",
        shift: Some(false),
        mode: None,
        key: "↵",
        does: "fullscreen the focused group (or terminal); again: back to the canvas",
        action: Some(ChordAction::ToggleFullscreenOrFit),
    },
    WallChord {
        section: SectionId::Fullscreen,
        code: "Enter",
        shift: Some(true),
        mode: None,
        key: "↵",
        does: "fullscreen the focused pane alone (again: back to its group, or the canvas)",
        action: Some(ChordAction::TogglePaneFullscreen),
    },
    WallChord {
        section: SectionId::Fullscreen,
        code: "Equal",
        shift: None,
        mode: Some(KeyMode::Fullscreen),
        key: "=",
        does: "bigger text (re-wraps; remembered for this terminal or group)",
        action: Some(ChordAction::FullscreenText(FullscreenTextStep::Bigger)),
    },
    WallChord {
        section: SectionId::Fullscreen,
        code: "Minus",
        shift: None,
        mode: Some(KeyMode::Fullscreen),
        key: "-",
        does: "smaller text",
        action: Some(ChordAction::FullscreenText(FullscreenTextStep::Smaller)),
    },
    WallChord {
        section: SectionId::Fullscreen,
        code: "Digit0",
        shift: None,
        mode: Some(KeyMode::Fullscreen),
        key: "0",
        does: "text back to the size it had on the canvas",
        action: Some(ChordAction::FullscreenText(FullscreenTextStep::Reset)),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "Backslash",
        shift: Some(false),
        mode: None,
        key: "\\",
        does: "open a new terminal",
        action: Some(ChordAction::Spawn),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "KeyA",
        shift: None,
        mode: Some(KeyMode::Canvas),
        key: "A",
        does: "toggle the focused terminal's Agent View (live transcript events)",
        action: Some(ChordAction::ToggleAgentView),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "Minus",
        shift: None,
        mode: Some(KeyMode::Canvas),
        key: "-",
        does: "hide the focused terminal (it keeps running)",
        action: Some(ChordAction::HideFocused),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "KeyX",
        shift: None,
        mode: None,
        key: "X",
        does: "hold: close the focused terminal (ends it)",
        action: Some(ChordAction::HoldCloseFocused),
    },
    WallChord {
        section: SectionId::Terminals,
        code: "KeyK",
        shift: None,
        mode: None,
        key: "K",
        does: "find a terminal by name, or run a verb",
        action: None,
    },
    WallChord {
        section: SectionId::Splits,
        code: "KeyD",
        shift: Some(false),
        mode: None,
        key: "D",
        does: "split the focused terminal: a new one to its right",
        action: Some(ChordAction::Split(SplitDir::Right)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "KeyD",
        shift: Some(true),
        mode: None,
        key: "D",
        does: "split the focused terminal: a new one below it",
        action: Some(ChordAction::Split(SplitDir::Down)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowLeft",
        shift: Some(false),
        mode: None,
        key: "←",
        does: "focus the pane (or window) to the left",
        action: Some(ChordAction::FocusPane(PaneDir::Left)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowRight",
        shift: Some(false),
        mode: None,
        key: "→",
        does: "focus the pane (or window) to the right",
        action: Some(ChordAction::FocusPane(PaneDir::Right)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowUp",
        shift: Some(false),
        mode: None,
        key: "↑",
        does: "focus the pane (or window) above",
        action: Some(ChordAction::FocusPane(PaneDir::Up)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowDown",
        shift: Some(false),
        mode: None,
        key: "↓",
        does: "focus the pane (or window) below",
        action: Some(ChordAction::FocusPane(PaneDir::Down)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowLeft",
        shift: Some(true),
        mode: None,
        key: "←",
        does: "move the focused pane (or tile) a place left in its group",
        action: Some(ChordAction::MovePane(PaneDir::Left)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowRight",
        shift: Some(true),
        mode: None,
        key: "→",
        does: "move the focused pane (or tile) a place right",
        action: Some(ChordAction::MovePane(PaneDir::Right)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowUp",
        shift: Some(true),
        mode: None,
        key: "↑",
        does: "move the focused pane (or tile) a place up",
        action: Some(ChordAction::MovePane(PaneDir::Up)),
    },
    WallChord {
        section: SectionId::Splits,
        code: "ArrowDown",
        shift: Some(true),
        mode: None,
        key: "↓",
        does: "move the focused pane (or tile) a place down",
        action: Some(ChordAction::MovePane(PaneDir::Down)),
    },
    WallChord {
        section: SectionId::Groups,
        code: "KeyG",
        shift: Some(false),
        mode: None,
        key: "G",
        does: "group the selection (or the focused terminal)",
        action: Some(ChordAction::GroupSelectionOrFocused),
    },
    WallChord {
        section: SectionId::Groups,
        code: "KeyG",
        shift: Some(true),
        mode: None,
        key: "G",
        does: "ungroup the focused terminal's group",
        action: Some(ChordAction::UngroupFocused),
    },
    WallChord {
        section: SectionId::Canvas,
        code: "Digit0",
        shift: None,
        mode: Some(KeyMode::Canvas),
        key: "0",
        does: "fit every terminal in view",
        action: Some(ChordAction::FitAll),
    },
    WallChord {
        section: SectionId::Canvas,
        code: "Quote",
        shift: Some(false),
        mode: None,
        key: "'",
        does: "snap moves and resizes to the grid, or not",
        action: Some(ChordAction::ToggleSnap),
    },
    WallChord {
        section: SectionId::Canvas,
        code: "Quote",
        shift: Some(true),
        mode: None,
        key: "'",
        does: "show or hide the drafting grid",
        action: Some(ChordAction::ToggleGrid),
    },
    WallChord {
        section: SectionId::Panels,
        code: "KeyB",
        shift: None,
        mode: None,
        key: "B",
        does: "the sidebar: every terminal and group",
        action: Some(ChordAction::ToggleSidebar),
    },
    WallChord {
        section: SectionId::Panels,
        code: "KeyS",
        shift: None,
        mode: None,
        key: "S",
        does: "termd's status window",
        action: Some(ChordAction::ToggleStatus),
    },
    WallChord {
        section: SectionId::Panels,
        code: "Comma",
        shift: None,
        mode: None,
        key: ",",
        does: "settings: every knob, searchable",
        action: Some(ChordAction::ToggleSettings),
    },
    WallChord {
        section: SectionId::Panels,
        code: "Slash",
        shift: None,
        mode: None,
        key: "/",
        does: "this help",
        action: Some(ChordAction::ToggleHelp),
    },
];

/// Look up the [`WallChord`] matching `code`, `shift`, and `mode` (assuming the caller
/// has already verified the event carries the platform's chord modifier).
pub fn chord_for_code(code: &str, shift: bool, mode: KeyMode) -> Option<&'static WallChord> {
    KEYMAP.iter().find(|c| {
        c.code == code
            && c.shift.is_none_or(|want| want == shift)
            && c.mode.is_none_or(|want| want == mode)
    })
}

/// Look up the [`WallChord`] that `event` names in `mode` (`chordFor` in `wallKeymap.js`).
///
/// Assumes the caller has already checked [`is_chord`].
pub fn chord_for(event: &KeyEvent, mode: KeyMode) -> Option<&'static WallChord> {
    chord_for_code(&event.code, event.mods.shift, mode)
}

/// Format `chord` as the help panel shows it (`chordText` in `wallKeymap.js`).
pub fn chord_text(platform: Platform, chord: &WallChord) -> String {
    chord_label(platform, &[chord.key], chord.shift == Some(true))
}

/// A non-chord keyboard shortcut handled by the wall (`PLAIN_KEYS` in `wallKeymap.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlainKey {
    pub section: SectionId,
    pub key: &'static str,
    pub does: &'static str,
}

const PLAIN_KEYS_MAC: &[PlainKey] = &[
    PlainKey {
        section: SectionId::Text,
        key: copy_label(Platform::Mac),
        does: "copy the focused terminal's selected text (Ctrl+C still reaches the program)",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: focus_history_label(Platform::Mac),
        does: "back / forward through focused terminals",
    },
    PlainKey {
        section: SectionId::Groups,
        key: group_cycle_label(Platform::Mac),
        does: "focus the previous / next group or window",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: "Esc",
        does: "clear the selection (when no terminal has the keys)",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: "Menu · ⇧F10",
        does: "open the right-click menu from the keyboard",
    },
];

const PLAIN_KEYS_OTHER: &[PlainKey] = &[
    PlainKey {
        section: SectionId::Text,
        key: copy_label(Platform::Other),
        does: "copy the focused terminal's selected text (Ctrl+C still reaches the program)",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: focus_history_label(Platform::Other),
        does: "back / forward through focused terminals",
    },
    PlainKey {
        section: SectionId::Groups,
        key: group_cycle_label(Platform::Other),
        does: "focus the previous / next group or window",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: "Esc",
        does: "clear the selection (when no terminal has the keys)",
    },
    PlainKey {
        section: SectionId::Terminals,
        key: "Menu · ⇧F10",
        does: "open the right-click menu from the keyboard",
    },
];

/// Keys without the wall chord that `WallKeys` answers, labelled for `platform`
/// (`PLAIN_KEYS` in `wallKeymap.js`).
pub const fn plain_keys(platform: Platform) -> &'static [PlainKey] {
    match platform {
        Platform::Mac => PLAIN_KEYS_MAC,
        Platform::Other => PLAIN_KEYS_OTHER,
    }
}

/// One pointer gesture listed in the help panel (`gestures()` in `wallKeymap.js`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gesture {
    pub section: SectionId,
    pub how: String,
    pub does: String,
}

fn gestures_with_zoom_label(platform: Platform, zoom_label: &str) -> Vec<Gesture> {
    let alt = alt_label(platform);
    let block = block_select_label(platform);
    let snap = snap_invert_label(platform);
    let g = |section: SectionId, how: String, does: String| Gesture { section, how, does };
    vec![
        g(SectionId::Terminals, "click".into(), "give a terminal the keys".into()),
        g(
            SectionId::Text,
            "drag a terminal's body".into(),
            "select its text (twice: a word, three times: a line)".into(),
        ),
        g(SectionId::Text, format!("{block}+drag"), "select a block of columns".into()),
        g(
            SectionId::Text,
            "⇧-drag".into(),
            "select text over a program that hears the mouse".into(),
        ),
        g(
            SectionId::Text,
            "middle-click".into(),
            "paste the wall's selection clipboard into a terminal".into(),
        ),
        g(
            SectionId::Terminals,
            "drag a title bar".into(),
            format!("move a terminal (or {alt}-drag anywhere on it)"),
        ),
        g(SectionId::Terminals, "drag a border".into(), "resize a terminal".into()),
        g(
            SectionId::Terminals,
            "drag an agent view's header".into(),
            "move it to right, left, top or bottom of its terminal".into(),
        ),
        g(
            SectionId::Terminals,
            "right-click".into(),
            "the menu for a terminal, a group or the canvas".into(),
        ),
        g(
            SectionId::Groups,
            "⇧-drag empty canvas".into(),
            "lasso terminals into the selection (⇧-click toggles one)".into(),
        ),
        g(SectionId::Groups, "drag a group header".into(), "move every member".into()),
        g(
            SectionId::Splits,
            "drag the gap between panes".into(),
            "move the cut; both sides re-wrap".into(),
        ),
        g(
            SectionId::Splits,
            "drag a split's outer border".into(),
            "resize the whole split; every pane re-wraps".into(),
        ),
        g(
            SectionId::Splits,
            "drag a pane's title bar".into(),
            "rearrange: drop it on a pane to trade places, near its edge to dock beside it".into(),
        ),
        g(SectionId::Splits, format!("{alt}-drag a pane"), "move the whole split".into()),
        g(
            SectionId::Groups,
            "drag a tiled member's title bar".into(),
            "move it to another tile (the others shift along)".into(),
        ),
        g(
            SectionId::Canvas,
            "drag empty canvas · wheel".into(),
            "pan (a wheel over the focused terminal scrolls its history)".into(),
        ),
        g(
            SectionId::Canvas,
            format!("{zoom_label}+wheel · pinch"),
            "zoom about the pointer".into(),
        ),
        g(
            SectionId::Canvas,
            format!("hold {snap} mid-drag"),
            "move or resize free of the grid (or onto it, with snap off)".into(),
        ),
    ]
}

/// Pointer gestures formatted for `platform` and already-parsed [`ZoomKey`]s.
pub fn gestures_for_keys(platform: Platform, zoom_keys: &[ZoomKey]) -> Vec<Gesture> {
    let label = zoom_wheel_label_keys(platform, zoom_keys);
    gestures_with_zoom_label(platform, &label)
}

/// Pointer gestures formatted for `platform` and the `zoomKeys` setting string
/// (`gestures` in `wallKeymap.js`).
pub fn gestures(platform: Platform, zoom_keys: &str) -> Result<Vec<Gesture>, String> {
    let label = zoom_wheel_label(platform, zoom_keys)?;
    Ok(gestures_with_zoom_label(platform, &label))
}

/// Section header in the help panel (`HELP_SECTIONS` in `wallKeymap.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelpSection {
    pub id: SectionId,
    pub title: &'static str,
}

/// The help panel's sections, in display order (`HELP_SECTIONS` in `wallKeymap.js`).
pub const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection { id: SectionId::Terminals, title: "Terminals" },
    HelpSection { id: SectionId::Fullscreen, title: "Fullscreen" },
    HelpSection { id: SectionId::Text, title: "Text" },
    HelpSection { id: SectionId::Splits, title: "Splits" },
    HelpSection { id: SectionId::Groups, title: "Groups" },
    HelpSection { id: SectionId::Canvas, title: "Canvas" },
    HelpSection { id: SectionId::Panels, title: "Panels" },
];

/// Verb metadata supplied to [`help_sections`] (`HelpPanel.helpSections` in `HelpPanel.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpVerb<'a> {
    pub name: &'a str,
    pub usage: &'a str,
    pub description: &'a str,
}

/// A single row (`how` -> `does`) in a rendered help section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpRow {
    pub how: String,
    pub does: String,
}

/// A populated section of the help panel, including the trailing `"verbs"` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpGroup {
    pub id: &'static str,
    pub title: &'static str,
    pub rows: Vec<HelpRow>,
}

/// Assemble every section and row shown in the help panel (`helpSections` in `HelpPanel.jsx`):
/// chords from [`KEYMAP`], [`plain_keys`], [`gestures`], and registered `verbs`.
pub fn help_sections(
    platform: Platform,
    zoom_keys: &str,
    verbs: &[HelpVerb<'_>],
) -> Result<Vec<HelpGroup>, String> {
    let gesture_list = gestures(platform, zoom_keys)?;
    let plain_list = plain_keys(platform);
    let mut out = Vec::with_capacity(HELP_SECTIONS.len() + 1);
    for section in HELP_SECTIONS {
        let mut rows = Vec::new();
        for c in KEYMAP.iter().filter(|c| c.section == section.id) {
            rows.push(HelpRow { how: chord_text(platform, c), does: c.does.to_string() });
        }
        for k in plain_list.iter().filter(|k| k.section == section.id) {
            rows.push(HelpRow { how: k.key.to_string(), does: k.does.to_string() });
        }
        for g in gesture_list.iter().filter(|g| g.section == section.id) {
            rows.push(HelpRow { how: g.how.clone(), does: g.does.clone() });
        }
        out.push(HelpGroup { id: section.id.as_str(), title: section.title, rows });
    }
    let verb_rows = verbs
        .iter()
        .map(|v| HelpRow {
            how: if v.usage.is_empty() {
                v.name.to_string()
            } else {
                format!("{} {}", v.name, v.usage)
            },
            does: v.description.to_string(),
        })
        .collect();
    out.push(HelpGroup {
        id: "verbs",
        title: "Verbs (palette or glyph3d-cli)",
        rows: verb_rows,
    });
    Ok(out)
}

/// Filter help sections to rows containing every whitespace-separated word of `query`
/// (case-insensitive across `how`, `does`, and the section's `title`), dropping empty
/// sections (`searchHelp` in `HelpPanel.jsx`).
pub fn search_help(sections: &[HelpGroup], query: &str) -> Vec<HelpGroup> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return sections.to_vec();
    }
    sections
        .iter()
        .filter_map(|section| {
            let rows: Vec<HelpRow> = section
                .rows
                .iter()
                .filter(|row| {
                    let haystack =
                        format!("{} {} {}", row.how, row.does, section.title).to_lowercase();
                    words.iter().all(|w| haystack.contains(w.as_str()))
                })
                .cloned()
                .collect();
            if rows.is_empty() {
                None
            } else {
                Some(HelpGroup { id: section.id, title: section.title, rows })
            }
        })
        .collect()
}

// ── Pure keyboard dispatch (`WallKeys.js` + `keyEncoding.js`) ────────────────

/// Values of the `optionAsAlt` knob (`OPTION_AS_ALT` in `keyEncoding.js`;
/// unified with [`crate::config::OptionAsAlt`]).
pub use crate::config::OptionAsAlt;

/// Phase of a key event sent to `termd`'s `key` op (`press`, `repeat`, `release`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalKeyAction {
    Press,
    Repeat,
    Release,
}

impl TerminalKeyAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            TerminalKeyAction::Press => "press",
            TerminalKeyAction::Repeat => "repeat",
            TerminalKeyAction::Release => "release",
        }
    }
}

/// A key event mapped for `termd`'s `key` op (`TerminalKeyEvent` in `keyEncoding.js`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalKey {
    /// Physical key (`KeyboardEvent.code`), e.g. `"KeyA"`, `"Enter"`.
    pub code: String,
    /// Logical key (`KeyboardEvent.key`), e.g. `"a"`, `"å"`, `"Enter"`.
    pub key: String,
    /// Active W3C modifier names in `keyEncoding.js` order (`"Shift"`, `"Control"`, `"Alt"`, `"Meta"`).
    pub mods: Vec<&'static str>,
    /// Text typed by the press (single Unicode scalar from `key` when `event.text` is `None`,
    /// empty for named keys such as `"Enter"` or `"ArrowLeft"`).
    pub text: String,
    pub action: TerminalKeyAction,
    pub option_as_alt: OptionAsAlt,
}

/// Keys that are modifiers themselves or an uncommitted dead-key/IME state (`NOTHING_YET` in
/// `keyEncoding.js`): they produce no terminal key event.
fn is_nothing_yet_key(key: &str) -> bool {
    matches!(
        key,
        "Shift"
            | "Control"
            | "Alt"
            | "Meta"
            | "AltGraph"
            | "CapsLock"
            | "NumLock"
            | "ScrollLock"
            | "Fn"
            | "FnLock"
            | "Hyper"
            | "Super"
            | "Symbol"
            | "SymbolLock"
            | "Dead"
            | "Process"
            | "Unidentified"
    )
}

/// Map `event` to a [`TerminalKey`] for `termd`, or `None` when the key should be left alone
/// (`keyEventFor` in `keyEncoding.js`: modifier-only keys, paste chords, and uncaptured `Escape`).
pub fn terminal_key_for(
    platform: Platform,
    event: &KeyEvent,
    capture_escape: bool,
    option_as_alt: OptionAsAlt,
) -> Option<TerminalKey> {
    let k = event.key.as_str();
    if k.is_empty() || is_nothing_yet_key(k) {
        return None;
    }
    if is_paste_chord(platform, event) {
        return None;
    }
    if k == "Escape" && !capture_escape {
        return None;
    }

    let mut mods = Vec::with_capacity(4);
    if event.mods.shift {
        mods.push("Shift");
    }
    if event.mods.ctrl {
        mods.push("Control");
    }
    if event.mods.alt {
        mods.push("Alt");
    }
    if event.mods.meta {
        mods.push("Meta");
    }

    let text = match &event.text {
        Some(explicit) => explicit.clone(),
        None if k.chars().count() == 1 => k.to_string(),
        None => String::new(),
    };
    let action = if !event.pressed {
        TerminalKeyAction::Release
    } else if event.repeat {
        TerminalKeyAction::Repeat
    } else {
        TerminalKeyAction::Press
    };

    Some(TerminalKey {
        code: event.code.clone(),
        key: event.key.clone(),
        mods,
        text,
        action,
        option_as_alt,
    })
}

/// Whether keyboard focus currently sits inside a UI text control rather than on the wall canvas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FieldFocus {
    /// No text control has focus; keys belong to the wall and its focused terminal.
    #[default]
    None,
    /// A text field (such as the palette or an inline rename input) has focus: global bracket
    /// navigation (`⌘[`/`⌘]` and `Ctrl+Shift+[`/`]`) still intercepts, without blurring.
    Input,
    /// A search/select control inside the Help or Settings panel has focus: global bracket
    /// navigation intercepts and blurs the control (`[data-help], [data-settings]` in `WallKeys.js`).
    PanelControl,
}

/// Snapshot of wall state read by [`decide_key`] to classify a [`KeyEvent`] without a window or grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyContext<'a> {
    /// True while the right-click context menu is open (`wall.menu != null`), in which case
    /// the menu owns keyboard input.
    pub menu_open: bool,
    /// Whether a UI text control currently holds focus.
    pub field_focus: FieldFocus,
    /// True while a terminal or group is in fullscreen (`wall.fullscreen.active || !!wall.fullscreen.id`).
    pub fullscreen: bool,
    /// Currently focused terminal id (`wall.focusedId`), if any.
    pub focused_id: Option<&'a str>,
    /// True when the focused terminal currently has a non-empty text selection.
    pub focused_has_selection: bool,
    /// Group id containing the focused terminal (`wall.groups.groupOf(focusedId)`), if any.
    pub focused_group: Option<&'a str>,
    /// True when the focused terminal belongs to a `split` or `tile` group
    /// (`wall.groups.splits(id) || wall.groups.tiles(id)`).
    pub focused_in_split_or_tile: bool,
    /// True when the wall's multi-terminal selection set is non-empty (`wall.selection.size > 0`).
    pub has_wall_selection: bool,
    /// Current value of the `optionAsAlt` knob, read per key event.
    pub option_as_alt: OptionAsAlt,
    /// Current value of the `selectionClearOnTyping` knob (`true` by default in `WALL_CONFIG`).
    pub selection_clear_on_typing: bool,
    /// For a key release (`!event.pressed`), the terminal id that received the matching press
    /// of `event.code` (`WallKeys._down.get(e.code)`).
    pub down_terminal: Option<&'a str>,
}

impl<'a> KeyContext<'a> {
    /// Active [`KeyMode`] derived from [`Self::fullscreen`].
    pub const fn mode(&self) -> KeyMode {
        if self.fullscreen {
            KeyMode::Fullscreen
        } else {
            KeyMode::Canvas
        }
    }
}

/// Concrete wall operation triggered by a claimed wall chord or plain wall shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WallCommand {
    /// Execute a `wall.*` verb argv on the command router (e.g. `["wall.settings", "toggle"]`).
    Verb(Vec<String>),
    /// Cycle focus to the next (`+1`) or previous (`-1`) pane in reading order (`wall.cycleFocus`).
    CycleFocus(CycleDir),
    /// Cycle focus to the next (`+1`) or previous (`-1`) group or window (`wall.cycleGroupFocus`).
    CycleGroupFocus(CycleDir),
    /// Step backward (`-1`) or forward (`+1`) in focus history (`wall.stepFocusHistory`).
    StepFocusHistory(CycleDir),
    /// Spawn a new terminal (`wall.spawn()`).
    Spawn,
    /// Ease the camera to fit every shown terminal (`wall.fitAll()`).
    FitAll,
    /// Start holding the close ring on the named terminal (`wall.barButtons.holdKey(id)`).
    HoldClose(String),
    /// Open the right-click context menu from the keyboard (`Menu` / `⇧F10`).
    OpenMenu,
}

impl WallCommand {
    /// Equivalent CLI verb argv where one exists on `wallCommands.js` (`None` for gestures
    /// that only exist as interactive wall methods, such as hold-to-close or pane/group cycling).
    pub fn verb_argv(&self) -> Option<Vec<String>> {
        match self {
            WallCommand::Verb(argv) => Some(argv.clone()),
            WallCommand::StepFocusHistory(dir) => Some(vec![
                "wall.focus.history".into(),
                match dir {
                    CycleDir::Prev => "back".into(),
                    CycleDir::Next => "forward".into(),
                },
            ]),
            WallCommand::Spawn => Some(vec!["wall.spawn".into()]),
            WallCommand::FitAll => Some(vec!["wall.fit".into()]),
            WallCommand::CycleFocus(_)
            | WallCommand::CycleGroupFocus(_)
            | WallCommand::HoldClose(_)
            | WallCommand::OpenMenu => None,
        }
    }
}

/// Pure result of [`decide_key`]: what `WallKeys` decides to do with a single [`KeyEvent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyDecision {
    /// A wall chord or plain wall shortcut claimed the event.
    ///
    /// `command` is `None` when the shortcut claims the event without running a command
    /// (for instance `⌘-` on an unfocused canvas, a repeated `⌘X`, or the keyup of a claimed
    /// shortcut). `blur_field` is `true` when global bracket navigation intercepted a key
    /// inside a Help/Settings panel control.
    Chord {
        command: Option<WallCommand>,
        blur_field: bool,
        release_close_hold: bool,
    },
    /// The platform's copy chord matched while the focused terminal has a selection.
    ///
    /// On an initial press `terminal` is `Some(focused_id)` (`["wall.copy", id]`); on key
    /// repeat `terminal` is `None` (still claimed, without copying a second time).
    Copy {
        terminal: Option<String>,
    },
    /// The event matched `KeyK` in [`KEYMAP`] (`action: None`), which `WallKeys` leaves
    /// unclaimed so the command/search palette can handle it.
    Palette,
    /// Send `key` to `terminal` (`focused_id` on press/repeat, `down_terminal` on release).
    ///
    /// On press/repeat, `clear_selection` is `true` when `selection_clear_on_typing` is enabled.
    TerminalKey {
        terminal: String,
        key: TerminalKey,
        clear_selection: bool,
        release_close_hold: bool,
    },
    /// Leave the event unclaimed (menu open, ordinary input inside a text field, unbound `⌘`
    /// shortcut on macOS, paste chord, modifier-only key, or key press with nothing focused).
    Pass {
        release_close_hold: bool,
    },
}

impl KeyDecision {
    /// Whether `WallKeys` claims (`preventDefault` / consumes) this key event.
    pub const fn claimed(&self) -> bool {
        matches!(
            self,
            KeyDecision::Chord { .. } | KeyDecision::Copy { .. } | KeyDecision::TerminalKey { .. }
        )
    }

    /// Whether a key release carrying this decision ends any active `KeyX` close-hold ring
    /// (`barButtons.releaseKey()` in `WallKeys._keyUp`).
    pub const fn releases_close_hold(&self) -> bool {
        match self {
            KeyDecision::Chord { release_close_hold, .. }
            | KeyDecision::TerminalKey { release_close_hold, .. }
            | KeyDecision::Pass { release_close_hold } => *release_close_hold,
            KeyDecision::Copy { .. } | KeyDecision::Palette => false,
        }
    }
}

/// Whether `event` matches an active wall shortcut that `WallKeys` claims across phases
/// (`_claimsShortcut` in `WallKeys.js`): focus-history, group-cycle, or a [`KEYMAP`] entry
/// in `mode` with `action.is_some()`.
pub fn claims_shortcut(
    platform: Platform,
    event: &KeyEvent,
    mode: KeyMode,
    menu_open: bool,
) -> bool {
    if menu_open {
        return false;
    }
    if focus_history_dir(event).is_some() || group_cycle_dir(event).is_some() {
        return true;
    }
    if !is_chord(platform, event) {
        return false;
    }
    chord_for(event, mode).is_some_and(|c| c.action.is_some())
}

fn verb<const N: usize>(parts: [&str; N]) -> WallCommand {
    WallCommand::Verb(parts.into_iter().map(str::to_string).collect())
}

/// Resolve a [`ChordAction`] against `ctx` and `repeat` into the [`WallCommand`] it runs,
/// or `None` when its guard declines to act (while still claiming the key).
pub fn resolve_chord_action(
    action: ChordAction,
    ctx: &KeyContext<'_>,
    repeat: bool,
) -> Option<WallCommand> {
    match action {
        ChordAction::CycleFocus(dir) => Some(WallCommand::CycleFocus(dir)),
        ChordAction::ToggleFullscreenOrFit => {
            if ctx.fullscreen || ctx.focused_id.is_some() {
                Some(verb(["wall.fullscreen", "toggle"]))
            } else {
                Some(WallCommand::FitAll)
            }
        }
        ChordAction::TogglePaneFullscreen => {
            if ctx.fullscreen || ctx.focused_id.is_some() {
                Some(verb(["wall.fullscreen.pane", "toggle"]))
            } else {
                None
            }
        }
        ChordAction::FullscreenText(step) => {
            Some(verb(["wall.fullscreen.text", step.as_str()]))
        }
        ChordAction::Spawn => Some(WallCommand::Spawn),
        ChordAction::ToggleAgentView => {
            ctx.focused_id.map(|id| verb(["wall.agent", id, "toggle"]))
        }
        ChordAction::HideFocused => ctx.focused_id.map(|id| verb(["wall.hide", id])),
        ChordAction::HoldCloseFocused => {
            if !repeat {
                ctx.focused_id.map(|id| WallCommand::HoldClose(id.to_string()))
            } else {
                None
            }
        }
        ChordAction::Split(dir) => Some(verb(["wall.split", dir.as_str()])),
        ChordAction::FocusPane(dir) => Some(verb(["wall.pane", dir.as_str()])),
        ChordAction::MovePane(dir) => {
            if ctx.focused_id.is_some() && ctx.focused_in_split_or_tile {
                Some(verb(["wall.pane.move", dir.as_str()]))
            } else {
                None
            }
        }
        ChordAction::GroupSelectionOrFocused => match (ctx.has_wall_selection, ctx.focused_id) {
            (true, _) | (false, None) => Some(verb(["wall.group"])),
            (false, Some(id)) => Some(verb(["wall.group", id])),
        },
        ChordAction::UngroupFocused => {
            if ctx.focused_id.is_some() {
                ctx.focused_group.map(|gid| verb(["wall.ungroup", gid]))
            } else {
                None
            }
        }
        ChordAction::FitAll => Some(WallCommand::FitAll),
        ChordAction::ToggleSnap => Some(verb(["wall.snap", "toggle"])),
        ChordAction::ToggleGrid => Some(verb(["wall.grid", "toggle"])),
        ChordAction::ToggleSidebar => Some(verb(["wall.sidebar", "toggle"])),
        ChordAction::ToggleStatus => Some(verb(["wall.status", "toggle"])),
        ChordAction::ToggleSettings => Some(verb(["wall.settings", "toggle"])),
        ChordAction::ToggleHelp => Some(verb(["wall.help", "toggle"])),
    }
}

fn is_global_nav_code(code: &str) -> bool {
    matches!(code, "BracketLeft" | "BracketRight")
}

fn is_context_menu_key(platform: Platform, event: &KeyEvent) -> bool {
    if is_chord(platform, event) {
        return false;
    }
    if event.key == "ContextMenu" {
        return true;
    }
    event.key == "F10"
        && event.mods.shift
        && !event.mods.ctrl
        && !event.mods.alt
        && !event.mods.meta
}

/// Pure keyboard routing decision from `WallKeys._key` (when `event.pressed` is `true`)
/// and `WallKeys._keyUp` (when `event.pressed` is `false`).
pub fn decide_key(platform: Platform, event: &KeyEvent, ctx: &KeyContext<'_>) -> KeyDecision {
    let mode = ctx.mode();

    if !event.pressed {
        let release_close_hold = event.code == "KeyX" || !is_chord(platform, event);
        if claims_shortcut(platform, event, mode, ctx.menu_open) {
            return KeyDecision::Chord {
                command: None,
                blur_field: false,
                release_close_hold,
            };
        }
        if let Some(id) = ctx.down_terminal {
            if let Some(key) = terminal_key_for(platform, event, true, ctx.option_as_alt) {
                return KeyDecision::TerminalKey {
                    terminal: id.to_string(),
                    key,
                    clear_selection: false,
                    release_close_hold,
                };
            }
        }
        return KeyDecision::Pass { release_close_hold };
    }

    if ctx.menu_open {
        return KeyDecision::Pass { release_close_hold: false };
    }

    if ctx.field_focus != FieldFocus::None {
        let blur_field = ctx.field_focus == FieldFocus::PanelControl;
        if let Some(dir) = group_cycle_dir(event) {
            return KeyDecision::Chord {
                command: Some(WallCommand::CycleGroupFocus(dir)),
                blur_field,
                release_close_hold: false,
            };
        }
        if is_chord(platform, event) && is_global_nav_code(&event.code) {
            if let Some(chord) = chord_for(event, mode) {
                if let Some(action) = chord.action {
                    return KeyDecision::Chord {
                        command: resolve_chord_action(action, ctx, event.repeat),
                        blur_field,
                        release_close_hold: false,
                    };
                }
            }
        }
        return KeyDecision::Pass { release_close_hold: false };
    }

    if let Some(dir) = focus_history_dir(event) {
        return KeyDecision::Chord {
            command: Some(WallCommand::StepFocusHistory(dir)),
            blur_field: false,
            release_close_hold: false,
        };
    }

    if let Some(dir) = group_cycle_dir(event) {
        return KeyDecision::Chord {
            command: Some(WallCommand::CycleGroupFocus(dir)),
            blur_field: false,
            release_close_hold: false,
        };
    }

    if is_copy_chord(platform, event) && ctx.focused_has_selection {
        if let Some(id) = ctx.focused_id {
            return KeyDecision::Copy {
                terminal: if event.repeat { None } else { Some(id.to_string()) },
            };
        }
    }

    if is_context_menu_key(platform, event) {
        return KeyDecision::Chord {
            command: Some(WallCommand::OpenMenu),
            blur_field: false,
            release_close_hold: false,
        };
    }

    if is_chord(platform, event) {
        if let Some(chord) = chord_for(event, mode) {
            return match chord.action {
                Some(action) => KeyDecision::Chord {
                    command: resolve_chord_action(action, ctx, event.repeat),
                    blur_field: false,
                    release_close_hold: false,
                },
                None => KeyDecision::Palette,
            };
        }
        if matches!(platform, Platform::Mac) {
            return KeyDecision::Pass { release_close_hold: false };
        }
    }

    let Some(id) = ctx.focused_id else {
        if event.key == "Escape" && ctx.has_wall_selection {
            return KeyDecision::Chord {
                command: Some(verb(["wall.select", "none"])),
                blur_field: false,
                release_close_hold: false,
            };
        }
        return KeyDecision::Pass { release_close_hold: false };
    };

    let Some(key) = terminal_key_for(platform, event, true, ctx.option_as_alt) else {
        return KeyDecision::Pass { release_close_hold: false };
    };

    KeyDecision::TerminalKey {
        terminal: id.to_string(),
        key,
        clear_selection: ctx.selection_clear_on_typing,
        release_close_hold: false,
    }
}

/// Tracks physical keys (`code -> terminal_id`) whose press went to a terminal so the
/// matching release reaches that same terminal (`WallKeys._down` in `WallKeys.js`).
#[derive(Clone, Debug, Default)]
pub struct PressedKeys {
    down: HashMap<String, String>,
}

impl PressedKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Terminal id that received the press of `code`, if still held.
    pub fn get(&self, code: &str) -> Option<&str> {
        self.down.get(code).map(String::as_str)
    }

    /// Clear all tracked presses (e.g. on window blur).
    pub fn clear(&mut self) {
        self.down.clear();
    }

    /// Run [`decide_key`] while maintaining the press-to-release terminal map, mirroring
    /// `WallKeys._key` and `WallKeys._keyUp` (assuming `sendKey` accepts the press).
    pub fn decide(
        &mut self,
        platform: Platform,
        event: &KeyEvent,
        ctx: &KeyContext<'_>,
    ) -> KeyDecision {
        if !event.pressed {
            let mode = ctx.mode();
            if claims_shortcut(platform, event, mode, ctx.menu_open) {
                return decide_key(platform, event, ctx);
            }
            let down = self.down.remove(&event.code);
            let release_ctx = KeyContext {
                down_terminal: down.as_deref().or(ctx.down_terminal),
                ..ctx.clone()
            };
            return decide_key(platform, event, &release_ctx);
        }

        let decision = decide_key(platform, event, ctx);
        if let KeyDecision::TerminalKey { terminal, key, .. } = &decision {
            if !key.code.is_empty() {
                self.down.insert(key.code.clone(), terminal.clone());
            }
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::chord_mods;
    use crate::input::Mods;

    const DEFAULT_ZOOM_KEYS: &str = "ctrl alt meta";

    fn chord_key(platform: Platform, code: &str, shift: bool) -> KeyEvent {
        let mut mods = chord_mods(platform);
        mods.shift = shift;
        KeyEvent {
            code: code.to_string(),
            key: String::new(),
            text: None,
            mods,
            repeat: false,
            pressed: true,
        }
    }

    fn ctrl_key(code: &str, shift: bool) -> KeyEvent {
        KeyEvent {
            code: code.to_string(),
            key: String::new(),
            text: None,
            mods: Mods { ctrl: true, shift, alt: false, meta: false },
            repeat: false,
            pressed: true,
        }
    }

    fn down(key: &str, code: &str, mods: Mods) -> KeyEvent {
        KeyEvent {
            code: code.to_string(),
            key: key.to_string(),
            text: None,
            mods,
            repeat: false,
            pressed: true,
        }
    }

    fn copy_chord_ev(platform: Platform) -> KeyEvent {
        match platform {
            Platform::Mac => down("c", "KeyC", Mods { meta: true, ..Mods::default() }),
            Platform::Other => {
                down("C", "KeyC", Mods { ctrl: true, shift: true, ..Mods::default() })
            }
        }
    }

    fn option_a(pressed: bool) -> KeyEvent {
        KeyEvent {
            code: "KeyA".to_string(),
            key: "å".to_string(),
            text: None,
            mods: Mods { alt: true, ..Mods::default() },
            repeat: false,
            pressed,
        }
    }

    fn overlap(a: &WallChord, b: &WallChord) -> bool {
        a.code == b.code
            && (a.shift.is_none() || b.shift.is_none() || a.shift == b.shift)
            && (a.mode.is_none() || b.mode.is_none() || a.mode == b.mode)
    }

    // ── Port of tools/wall-keymap.test.mjs ───────────────────────────────────

    #[test]
    fn no_two_chords_can_both_answer_one_key() {
        for (i, a) in KEYMAP.iter().enumerate() {
            for b in &KEYMAP[i + 1..] {
                assert!(
                    !overlap(a, b),
                    "chords overlap on code={}: {:?} vs {:?}",
                    a.code,
                    a.does,
                    b.does
                );
            }
        }
        let section_ids: Vec<SectionId> = HELP_SECTIONS.iter().map(|s| s.id).collect();
        for c in KEYMAP {
            assert!(section_ids.contains(&c.section));
        }
    }

    #[test]
    fn chord_for_matches_shift_and_mode_as_each_entry_says() {
        let at = |code: &str, mode: KeyMode, shift: bool| chord_for_code(code, shift, mode);

        assert!(at("Enter", KeyMode::Canvas, false)
            .unwrap()
            .does
            .contains("fullscreen the focused group"));
        assert_eq!(
            at("Enter", KeyMode::Fullscreen, false),
            at("Enter", KeyMode::Canvas, false)
        );
        assert!(at("Enter", KeyMode::Canvas, true)
            .unwrap()
            .does
            .contains("focused pane alone"));
        assert_eq!(
            at("Enter", KeyMode::Fullscreen, true),
            at("Enter", KeyMode::Canvas, true)
        );
        assert!(at("Backslash", KeyMode::Canvas, false)
            .unwrap()
            .does
            .contains("new terminal"));
        assert!(at("BracketRight", KeyMode::Canvas, false)
            .unwrap()
            .does
            .contains("next pane"));
        assert_eq!(at("BracketRight", KeyMode::Canvas, true), None);
        assert!(at("BracketLeft", KeyMode::Canvas, false)
            .unwrap()
            .does
            .contains("previous pane"));
        assert_eq!(at("BracketLeft", KeyMode::Canvas, true), None);
        assert!(at("ArrowLeft", KeyMode::Canvas, false)
            .unwrap()
            .does
            .contains("focus the pane"));
        assert!(at("ArrowLeft", KeyMode::Canvas, true)
            .unwrap()
            .does
            .contains("move the focused pane"));
        assert_eq!(
            at("ArrowDown", KeyMode::Fullscreen, true),
            at("ArrowDown", KeyMode::Canvas, true)
        );
        assert!(at("Minus", KeyMode::Canvas, false).unwrap().does.contains("hide"));
        assert!(at("Minus", KeyMode::Fullscreen, false)
            .unwrap()
            .does
            .contains("smaller text"));
        assert!(at("Digit0", KeyMode::Canvas, false).unwrap().does.contains("fit every"));
        assert!(at("Digit0", KeyMode::Fullscreen, false)
            .unwrap()
            .does
            .contains("back to the size"));
        assert_eq!(at("Equal", KeyMode::Canvas, false), None);
        assert_eq!(at("KeyQ", KeyMode::Canvas, false), None);
    }

    #[test]
    fn wall_keys_dispatches_from_the_table_across_platforms() {
        for platform in [Platform::Mac, Platform::Other] {
            let mut ran: Vec<WallCommand> = Vec::new();
            let mut ctx = KeyContext {
                selection_clear_on_typing: true,
                ..KeyContext::default()
            };
            let apply =
                |ev: &KeyEvent, ctx: &KeyContext<'_>, ran: &mut Vec<WallCommand>| -> KeyDecision {
                    let d = decide_key(platform, ev, ctx);
                    if let KeyDecision::Chord { command: Some(cmd), .. } = &d {
                        ran.push(cmd.clone());
                    }
                    d
                };

            let next = apply(&chord_key(platform, "BracketRight", false), &ctx, &mut ran);
            let prev_pane = apply(&chord_key(platform, "BracketLeft", false), &ctx, &mut ran);
            let browser_tab_next =
                apply(&chord_key(platform, "BracketRight", true), &ctx, &mut ran);
            let browser_tab_prev =
                apply(&chord_key(platform, "BracketLeft", true), &ctx, &mut ran);

            let back = apply(&ctrl_key("Comma", false), &ctx, &mut ran);
            let fwd = apply(&ctrl_key("Period", true), &ctx, &mut ran);
            let ctrl_prev_group = apply(&ctrl_key("BracketLeft", true), &ctx, &mut ran);
            let ctrl_next_group = apply(&ctrl_key("BracketRight", true), &ctx, &mut ran);

            apply(&chord_key(platform, "Comma", false), &ctx, &mut ran);
            apply(&chord_key(platform, "Backslash", false), &ctx, &mut ran);
            apply(&chord_key(platform, "Slash", false), &ctx, &mut ran);
            apply(&chord_key(platform, "Enter", false), &ctx, &mut ran);
            let canvas_minus = apply(&chord_key(platform, "Minus", false), &ctx, &mut ran);
            assert!(canvas_minus.claimed());

            ctx.fullscreen = true;
            apply(&chord_key(platform, "Enter", true), &ctx, &mut ran);
            apply(&chord_key(platform, "Minus", false), &ctx, &mut ran);
            apply(&chord_key(platform, "Enter", false), &ctx, &mut ran);

            let k = apply(&chord_key(platform, "KeyK", false), &ctx, &mut ran);
            assert_eq!(k, KeyDecision::Palette);

            assert_eq!(
                ran,
                vec![
                    WallCommand::CycleFocus(CycleDir::Next),
                    WallCommand::CycleFocus(CycleDir::Prev),
                    WallCommand::StepFocusHistory(CycleDir::Prev),
                    WallCommand::StepFocusHistory(CycleDir::Next),
                    WallCommand::CycleGroupFocus(CycleDir::Prev),
                    WallCommand::CycleGroupFocus(CycleDir::Next),
                    verb(["wall.settings", "toggle"]),
                    WallCommand::Spawn,
                    verb(["wall.help", "toggle"]),
                    WallCommand::FitAll,
                    verb(["wall.fullscreen.pane", "toggle"]),
                    verb(["wall.fullscreen.text", "smaller"]),
                    verb(["wall.fullscreen", "toggle"]),
                ]
            );
            assert!(next.claimed());
            assert!(prev_pane.claimed());
            assert!(!browser_tab_next.claimed());
            assert!(!browser_tab_prev.claimed());
            assert!(back.claimed());
            assert!(fwd.claimed());
            assert!(ctrl_prev_group.claimed());
            assert!(ctrl_next_group.claimed());
            assert!(!k.claimed());

            // Shortcut claiming on keyup and inside a focused Help/Settings panel search input.
            let bracket_right = chord_key(platform, "BracketRight", false);
            assert!(claims_shortcut(platform, &bracket_right, KeyMode::Fullscreen, false));
            let mut ku = bracket_right.clone();
            ku.pressed = false;
            let ku_dec = apply(&ku, &ctx, &mut ran);
            assert!(ku_dec.claimed());
            assert_eq!(ran.len(), 13);

            let panel_ctx = KeyContext {
                field_focus: FieldFocus::PanelControl,
                ..ctx.clone()
            };
            let kd_in_panel = apply(&bracket_right, &panel_ctx, &mut ran);
            assert_eq!(
                kd_in_panel,
                KeyDecision::Chord {
                    command: Some(WallCommand::CycleFocus(CycleDir::Next)),
                    blur_field: true,
                    release_close_hold: false,
                }
            );
            assert_eq!(ran.last(), Some(&WallCommand::CycleFocus(CycleDir::Next)));

            let ctrl_in_panel = apply(&ctrl_key("BracketLeft", true), &panel_ctx, &mut ran);
            assert_eq!(
                ctrl_in_panel,
                KeyDecision::Chord {
                    command: Some(WallCommand::CycleGroupFocus(CycleDir::Prev)),
                    blur_field: true,
                    release_close_hold: false,
                }
            );
            assert_eq!(ran.last(), Some(&WallCommand::CycleGroupFocus(CycleDir::Prev)));

            // Ordinary chords and keys inside a focused panel input stay with the input.
            let comma_in_panel = decide_key(
                platform,
                &chord_key(platform, "Comma", false),
                &panel_ctx,
            );
            assert!(!comma_in_panel.claimed());
        }
    }

    #[test]
    fn help_lists_every_chord_plain_key_gesture_and_verb_and_search_keeps_every_word() {
        for platform in [Platform::Mac, Platform::Other] {
            let verbs = [
                HelpVerb { name: "call", usage: "<base64-json>", description: "Call JSON" },
                HelpVerb { name: "help", usage: "[namespace|verb]", description: "List verbs" },
                HelpVerb {
                    name: "wall.hide",
                    usage: "<id…>",
                    description: "Take terminals off the canvas",
                },
            ];
            let sections = help_sections(platform, DEFAULT_ZOOM_KEYS, &verbs).unwrap();
            let rows: Vec<String> = sections
                .iter()
                .flat_map(|s| s.rows.iter().map(|r| format!("{} | {}", r.how, r.does)))
                .collect();

            for c in KEYMAP {
                assert!(rows.contains(&format!("{} | {}", chord_text(platform, c), c.does)));
            }
            for k in plain_keys(platform) {
                assert!(rows.contains(&format!("{} | {}", k.key, k.does)));
            }
            for g in gestures(platform, DEFAULT_ZOOM_KEYS).unwrap() {
                assert!(rows.contains(&format!("{} | {}", g.how, g.does)));
            }

            let verb_hows: Vec<&str> = sections
                .iter()
                .find(|s| s.id == "verbs")
                .unwrap()
                .rows
                .iter()
                .map(|r| r.how.as_str())
                .collect();
            assert_eq!(
                verb_hows,
                vec!["call <base64-json>", "help [namespace|verb]", "wall.hide <id…>"]
            );

            let found = search_help(&sections, "hide terminal");
            let found_hows: Vec<&str> =
                found.iter().flat_map(|s| s.rows.iter().map(|r| r.how.as_str())).collect();
            assert!(found_hows.contains(&"wall.hide <id…>"));
            assert!(found.iter().all(|s| {
                s.rows.iter().all(|r| {
                    format!("{} {} {}", r.how, r.does, s.title)
                        .to_lowercase()
                        .contains("hide")
                })
            }));
            assert_eq!(search_help(&sections, "zzqq"), Vec::<HelpGroup>::new());
        }
    }

    // ── Port of tools/wall-keys.test.mjs ─────────────────────────────────────

    #[test]
    fn option_as_alt_is_born_auto_and_checked() {
        assert_eq!(OptionAsAlt::default(), OptionAsAlt::Auto);
        assert_eq!(OptionAsAlt::default().as_str(), "auto");
        for v in ["auto", "true", "false"] {
            assert_eq!(OptionAsAlt::parse(v).unwrap().as_str(), v);
        }
        let err = OptionAsAlt::parse("left").unwrap_err();
        assert!(err.contains("optionAsAlt is auto, true, false"));
    }

    #[test]
    fn every_key_carries_option_as_alt_as_it_is_when_sent() {
        for platform in [Platform::Mac, Platform::Other] {
            let mut tracker = PressedKeys::new();
            let mut sent: Vec<TerminalKey> = Vec::new();
            let mut ctx = KeyContext {
                focused_id: Some("t1"),
                option_as_alt: OptionAsAlt::Auto,
                selection_clear_on_typing: true,
                ..KeyContext::default()
            };

            for ev in [option_a(true), option_a(false)] {
                if let KeyDecision::TerminalKey { key, .. } =
                    tracker.decide(platform, &ev, &ctx)
                {
                    sent.push(key);
                }
            }
            let pairs: Vec<(TerminalKeyAction, OptionAsAlt)> =
                sent.iter().map(|k| (k.action, k.option_as_alt)).collect();
            assert_eq!(
                pairs,
                vec![
                    (TerminalKeyAction::Press, OptionAsAlt::Auto),
                    (TerminalKeyAction::Release, OptionAsAlt::Auto),
                ]
            );

            ctx.option_as_alt = OptionAsAlt::parse("false").unwrap();
            if let KeyDecision::TerminalKey { key, .. } =
                tracker.decide(platform, &option_a(true), &ctx)
            {
                sent.push(key);
            }
            let last = sent.last().unwrap();
            assert_eq!(last.code, "KeyA");
            assert_eq!(last.text, "å");
            assert_eq!(last.mods, vec!["Alt"]);
            assert_eq!(last.option_as_alt, OptionAsAlt::False);

            ctx.option_as_alt = OptionAsAlt::True;
            if let KeyDecision::TerminalKey { key, .. } =
                tracker.decide(platform, &option_a(false), &ctx)
            {
                sent.push(key);
            }
            let last = sent.last().unwrap();
            assert_eq!(last.action, TerminalKeyAction::Release);
            assert_eq!(last.option_as_alt, OptionAsAlt::True);
        }
    }

    #[test]
    fn copy_chord_copies_a_selection_with_none_is_program_or_browser_key_and_ctrl_c_always_is() {
        for platform in [Platform::Mac, Platform::Other] {
            let mut sent: Vec<TerminalKey> = Vec::new();
            let mut ran: Vec<Vec<String>> = Vec::new();
            let mut has_selection = false;

            let step = |ev: &KeyEvent,
                        has_selection: &mut bool,
                        sent: &mut Vec<TerminalKey>,
                        ran: &mut Vec<Vec<String>>|
             -> KeyDecision {
                let ctx = KeyContext {
                    focused_id: Some("t1"),
                    focused_has_selection: *has_selection,
                    selection_clear_on_typing: true,
                    ..KeyContext::default()
                };
                let dec = decide_key(platform, ev, &ctx);
                match &dec {
                    KeyDecision::Copy { terminal: Some(id) } => {
                        ran.push(vec!["wall.copy".into(), id.clone()]);
                    }
                    KeyDecision::TerminalKey { key, clear_selection, .. } => {
                        sent.push(key.clone());
                        if *clear_selection {
                            *has_selection = false;
                        }
                    }
                    _ => {}
                }
                dec
            };

            // Nothing selected: nothing is copied. Off macOS Ctrl+Shift+C reaches the program;
            // on macOS ⌘C is left unclaimed for the browser/OS.
            let bare = step(&copy_chord_ev(platform), &mut has_selection, &mut sent, &mut ran);
            assert!(ran.is_empty());
            match platform {
                Platform::Mac => {
                    assert!(sent.is_empty());
                    assert!(!bare.claimed());
                }
                Platform::Other => {
                    let last = sent.last().unwrap();
                    assert_eq!(last.code, "KeyC");
                    assert_eq!(last.mods, vec!["Shift", "Control"]);
                }
            }

            // A selection: the chord copies it, claims the key, keeps the selection, and sends nothing.
            has_selection = true;
            let n = sent.len();
            let chord = step(&copy_chord_ev(platform), &mut has_selection, &mut sent, &mut ran);
            assert_eq!(ran, vec![vec!["wall.copy".to_string(), "t1".to_string()]]);
            assert_eq!(sent.len(), n);
            assert!(chord.claimed());
            assert!(has_selection);

            // Ctrl+C is SIGINT: reaches the program whether selected or not, and typing drops selection.
            step(
                &down("c", "KeyC", Mods { ctrl: true, ..Mods::default() }),
                &mut has_selection,
                &mut sent,
                &mut ran,
            );
            let last = sent.last().unwrap();
            assert_eq!(last.code, "KeyC");
            assert_eq!(last.key, "c");
            assert_eq!(last.mods, vec!["Control"]);
            assert_eq!(ran.len(), 1);
            assert!(!has_selection);
        }
    }

    #[test]
    fn selection_clear_on_typing_off_leaves_selection_and_modifiers_alone_type_nothing() {
        for platform in [Platform::Mac, Platform::Other] {
            let mut has_selection = true;
            let send = |ev: &KeyEvent, clear_on_typing: bool, has_selection: &mut bool| {
                let ctx = KeyContext {
                    focused_id: Some("t1"),
                    focused_has_selection: *has_selection,
                    selection_clear_on_typing: clear_on_typing,
                    ..KeyContext::default()
                };
                if let KeyDecision::TerminalKey { clear_selection, .. } =
                    decide_key(platform, ev, &ctx)
                {
                    if clear_selection {
                        *has_selection = false;
                    }
                }
            };

            send(&down("a", "KeyA", Mods::default()), false, &mut has_selection);
            assert!(has_selection);

            let shift_only = down("Shift", "ShiftLeft", Mods { shift: true, ..Mods::default() });
            send(&shift_only, false, &mut has_selection);
            send(&shift_only, true, &mut has_selection);
            assert!(has_selection);

            send(&down("a", "KeyA", Mods::default()), true, &mut has_selection);
            assert!(!has_selection);
        }
    }

    #[test]
    fn unshifted_ctrl_brackets_reach_terminal_while_ctrl_shift_brackets_cycle_groups() {
        for platform in [Platform::Mac, Platform::Other] {
            let ctx = KeyContext {
                focused_id: Some("t1"),
                selection_clear_on_typing: true,
                ..KeyContext::default()
            };
            let mut groups: Vec<i32> = Vec::new();
            let mut sent: Vec<(String, Vec<&'static str>)> = Vec::new();

            for ev in [
                down("[", "BracketLeft", Mods { ctrl: true, ..Mods::default() }),
                down("]", "BracketRight", Mods { ctrl: true, ..Mods::default() }),
                down(
                    "{",
                    "BracketLeft",
                    Mods { ctrl: true, shift: true, ..Mods::default() },
                ),
                down(
                    "}",
                    "BracketRight",
                    Mods { ctrl: true, shift: true, ..Mods::default() },
                ),
            ] {
                match decide_key(platform, &ev, &ctx) {
                    KeyDecision::Chord {
                        command: Some(WallCommand::CycleGroupFocus(dir)),
                        ..
                    } => groups.push(dir.step()),
                    KeyDecision::TerminalKey { key, .. } => {
                        sent.push((key.code, key.mods));
                    }
                    other => panic!("unexpected decision: {other:?}"),
                }
            }

            assert_eq!(
                sent,
                vec![
                    ("BracketLeft".to_string(), vec!["Control"]),
                    ("BracketRight".to_string(), vec!["Control"]),
                ]
            );
            assert_eq!(groups, vec![-1, 1]);
        }
    }

    #[test]
    fn guards_escape_menu_and_close_hold_release_behave_like_wall_keys() {
        for platform in [Platform::Mac, Platform::Other] {
            // Escape with no focused terminal clears a non-empty wall selection, and passes otherwise.
            let esc = down("Escape", "Escape", Mods::default());
            let empty_ctx = KeyContext::default();
            assert!(!decide_key(platform, &esc, &empty_ctx).claimed());

            let sel_ctx = KeyContext { has_wall_selection: true, ..KeyContext::default() };
            assert_eq!(
                decide_key(platform, &esc, &sel_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.select", "none"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );

            // ContextMenu and Shift+F10 open the menu; while menu_open is true, keys pass.
            let menu_ev = down("ContextMenu", "ContextMenu", Mods::default());
            let shift_f10 = down("F10", "F10", Mods { shift: true, ..Mods::default() });
            assert_eq!(
                decide_key(platform, &menu_ev, &empty_ctx),
                KeyDecision::Chord {
                    command: Some(WallCommand::OpenMenu),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &shift_f10, &empty_ctx),
                KeyDecision::Chord {
                    command: Some(WallCommand::OpenMenu),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            let menu_open_ctx = KeyContext { menu_open: true, ..KeyContext::default() };
            assert!(!decide_key(platform, &menu_ev, &menu_open_ctx).claimed());

            // KeyX hold-to-close ignores key repeat, and releasing KeyX or dropping the chord releases hold.
            let focused_ctx = KeyContext { focused_id: Some("t1"), ..KeyContext::default() };
            let hold_x = chord_key(platform, "KeyX", false);
            assert_eq!(
                decide_key(platform, &hold_x, &focused_ctx),
                KeyDecision::Chord {
                    command: Some(WallCommand::HoldClose("t1".into())),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            let mut repeat_x = hold_x.clone();
            repeat_x.repeat = true;
            assert_eq!(
                decide_key(platform, &repeat_x, &focused_ctx),
                KeyDecision::Chord {
                    command: None,
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            let mut up_x = hold_x;
            up_x.pressed = false;
            let up_dec = decide_key(platform, &up_x, &focused_ctx);
            assert!(up_dec.claimed());
            assert!(up_dec.releases_close_hold());

            // Group / ungroup, split / pane move, agent view, snap / grid / panels, and paste chord.
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyG", false), &empty_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.group"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyG", false), &focused_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.group", "t1"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            let grouped_ctx = KeyContext {
                focused_id: Some("t1"),
                focused_group: Some("g-1"),
                focused_in_split_or_tile: true,
                has_wall_selection: true,
                ..KeyContext::default()
            };
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyG", false), &grouped_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.group"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyG", true), &focused_ctx),
                KeyDecision::Chord {
                    command: None,
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyG", true), &grouped_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.ungroup", "g-1"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "ArrowLeft", true), &focused_ctx),
                KeyDecision::Chord {
                    command: None,
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "ArrowLeft", true), &grouped_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.pane.move", "left"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "KeyA", false), &focused_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.agent", "t1", "toggle"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );
            assert_eq!(
                decide_key(platform, &chord_key(platform, "Minus", false), &focused_ctx),
                KeyDecision::Chord {
                    command: Some(verb(["wall.hide", "t1"])),
                    blur_field: false,
                    release_close_hold: false,
                }
            );

            // Paste chord is never sent to the terminal, while Ctrl+V still reaches the program.
            let paste = match platform {
                Platform::Mac => down("v", "KeyV", Mods { meta: true, ..Mods::default() }),
                Platform::Other => {
                    down("V", "KeyV", Mods { ctrl: true, shift: true, ..Mods::default() })
                }
            };
            assert!(!decide_key(platform, &paste, &focused_ctx).claimed());
            let ctrl_v = down("v", "KeyV", Mods { ctrl: true, ..Mods::default() });
            assert!(matches!(
                decide_key(platform, &ctrl_v, &focused_ctx),
                KeyDecision::TerminalKey { .. }
            ));
        }
    }
}
