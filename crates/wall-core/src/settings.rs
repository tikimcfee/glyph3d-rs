//! Port of `wall/wallSettings.js`: the wall's knob schema (`SECTIONS`, `KNOBS`),
//! value validation (`check_knob`) that refuses out-of-range or malformed inputs
//! naming the knob rather than clamping them, and full-text search with merged
//! highlight spans (`hits_in`, `search_settings`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// One section of the settings panel, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsSection {
    pub id: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
}

/// The settings panel's sections, in display order (`wallSettings.SECTIONS`).
pub const SECTIONS: &[SettingsSection] = &[
    SettingsSection {
        id: "canvas",
        title: "Canvas",
        subtitle: "background · drafting grid · snapping",
    },
    SettingsSection {
        id: "terminals",
        title: "Terminals",
        subtitle: "closing · title-bar buttons",
    },
    SettingsSection {
        id: "agent",
        title: "Agent view",
        subtitle: "transcript side · dimensions · gap",
    },
    SettingsSection {
        id: "fullscreen",
        title: "Fullscreen",
        subtitle: "text size · room at the edge",
    },
    SettingsSection {
        id: "text",
        title: "Text",
        subtitle: "font family · bold · underline · strikethrough · overline · dim",
    },
    SettingsSection {
        id: "groups",
        title: "Groups",
        subtitle: "what a new group is born with · backdrop",
    },
    SettingsSection {
        id: "status",
        title: "Status",
        subtitle: "the termd status window · notices",
    },
    SettingsSection {
        id: "input",
        title: "Input",
        subtitle: "keyboard · wheel",
    },
    SettingsSection {
        id: "selection",
        title: "Selection",
        subtitle: "selecting text · copy · paste",
    },
    SettingsSection {
        id: "menu",
        title: "Menu",
        subtitle: "the right-click menu",
    },
];

/// The five knob value categories from the settings schema vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnobType {
    Bool,
    Number,
    Color,
    Enum,
    Text,
}

impl KnobType {
    /// Wire/schema name matching JS `KnobDef.type`.
    pub const fn as_str(self) -> &'static str {
        match self {
            KnobType::Bool => "bool",
            KnobType::Number => "number",
            KnobType::Color => "color",
            KnobType::Enum => "enum",
            KnobType::Text => "text",
        }
    }
}

/// Validation and normalization rule for a `text` knob (`KnobDef.parse` in JS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextParser {
    /// Trim leading and trailing whitespace (used when `KnobDef.parse` is absent).
    Trim,
    /// Space-separated modifier names (`ctrl`, `alt`, `meta`, `shift`).
    ZoomKeys,
    /// `#rrggbb` hex colour (lowercased) or `"keep"`.
    SelectionForeground,
    /// Strip all whitespace characters (`s.replace(/\s+/g, '')`).
    SelectionWordChars,
    /// Space-separated `COLSxROWS` pairs (`80x24 120x36`), empty allowed.
    MenuSizes,
}

impl TextParser {
    /// Parse and normalize `input`, returning the normalized text or a reason string
    /// (without the knob prefix, which [`check_knob`] prepends as `"{key}: {reason}"`).
    pub fn parse(self, input: &str) -> Result<String, String> {
        match self {
            TextParser::Trim => Ok(input.trim().to_string()),
            TextParser::ZoomKeys => Ok(parse_zoom_keys_knob(input)?.join(" ")),
            TextParser::SelectionForeground => parse_selection_foreground_knob(input),
            TextParser::SelectionWordChars => {
                Ok(input.chars().filter(|ch| !ch.is_whitespace()).collect())
            }
            TextParser::MenuSizes => parse_menu_sizes_knob(input),
        }
    }
}

/// Type-specific constraints and metadata of a [`KnobDef`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KnobKind {
    /// Boolean switch (`true`/`on`/`1`/`yes` vs `false`/`off`/`0`/`no`).
    Bool,
    /// Numeric value with optional inclusive `min`, exclusive `above`, inclusive `max`,
    /// UI `step`, integer requirement `whole`, and display `unit`.
    Number {
        min: Option<f64>,
        above: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
        whole: bool,
        unit: Option<&'static str>,
    },
    /// `#rrggbb` sRGB hex colour, normalized to lowercase.
    Color,
    /// Fixed set of string options, in UI order.
    Enum { options: &'static [&'static str] },
    /// Free-form or structured text line validated and normalized by `parser`.
    Text { parser: TextParser },
}

impl KnobKind {
    /// The schema type tag (`bool`, `number`, `color`, `enum`, `text`).
    pub const fn knob_type(&self) -> KnobType {
        match self {
            KnobKind::Bool => KnobType::Bool,
            KnobKind::Number { .. } => KnobType::Number,
            KnobKind::Color => KnobType::Color,
            KnobKind::Enum { .. } => KnobType::Enum,
            KnobKind::Text { .. } => KnobType::Text,
        }
    }
}

/// Schema definition of one wall knob: its section, label, description, and type constraints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnobDef {
    pub section: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub kind: KnobKind,
}

impl KnobDef {
    /// The schema type tag (`bool`, `number`, `color`, `enum`, `text`).
    pub const fn knob_type(&self) -> KnobType {
        self.kind.knob_type()
    }
}

pub(crate) const RENDER_LOOP_OPTIONS: &[&str] = &["demand", "always"];
pub(crate) const AGENT_SIDE_OPTIONS: &[&str] = &["right", "left", "top", "bottom"];
pub(crate) const FONT_FAMILY_OPTIONS: &[&str] = &["cousine", "monaspace-neon", "monaspace-argon"];
pub(crate) const GROUP_LAYOUT_OPTIONS: &[&str] = &["tile", "freeform", "split"];
pub(crate) const OPTION_AS_ALT_OPTIONS: &[&str] = &["auto", "true", "false"];
pub(crate) const COPY_ON_SELECT_OPTIONS: &[&str] = &["true", "clipboard", "false"];

/// Every wall knob's definition, in the settings panel's order within each section (`wallSettings.KNOBS`).
pub const KNOBS: &[(&str, KnobDef)] = &[
    (
        "background",
        KnobDef {
            section: "canvas",
            label: "Background",
            kind: KnobKind::Color,
            description: "The canvas colour behind every terminal and group.",
        },
    ),
    (
        "renderLoop",
        KnobDef {
            section: "canvas",
            label: "Render loop",
            kind: KnobKind::Enum { options: RENDER_LOOP_OPTIONS },
            description: "demand draws the canvas only when something changed (input, output, a move) and idles the GPU otherwise; always draws every display frame — a check when something looks stale.",
        },
    ),
    (
        "gridShow",
        KnobDef {
            section: "canvas",
            label: "Show grid",
            kind: KnobKind::Bool,
            description: "Draw the drafting-paper grid on the canvas. Snapping works with the grid hidden too.",
        },
    ),
    (
        "gridMinor",
        KnobDef {
            section: "canvas",
            label: "Grid spacing",
            kind: KnobKind::Number {
                min: None,
                above: Some(0.0),
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("world units"),
            },
            description: "Distance between the fine grid lines — the step moves and resizes snap to.",
        },
    ),
    (
        "gridMajorEvery",
        KnobDef {
            section: "canvas",
            label: "Major line every",
            kind: KnobKind::Number {
                min: Some(1.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: true,
                unit: Some("lines"),
            },
            description: "Every Nth line is a major (brighter) line; 1 makes every line major.",
        },
    ),
    (
        "gridMinPx",
        KnobDef {
            section: "canvas",
            label: "Hide lines closer than",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("px"),
            },
            description: "Zoomed out, a level of lines packed closer than this on screen is not drawn, and snapping steps to the major lines.",
        },
    ),
    (
        "gridLinePx",
        KnobDef {
            section: "canvas",
            label: "Line width",
            kind: KnobKind::Number {
                min: None,
                above: Some(0.0),
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("px"),
            },
            description: "How thick each grid line is on screen.",
        },
    ),
    (
        "gridMinorColor",
        KnobDef {
            section: "canvas",
            label: "Line colour",
            kind: KnobKind::Color,
            description: "The colour of the fine grid lines.",
        },
    ),
    (
        "gridMajorColor",
        KnobDef {
            section: "canvas",
            label: "Major line colour",
            kind: KnobKind::Color,
            description: "The colour of every major grid line.",
        },
    ),
    (
        "snap",
        KnobDef {
            section: "canvas",
            label: "Snap to grid",
            kind: KnobKind::Bool,
            description: "Moving a terminal puts its top-left corner on a grid crossing; resizing lands the dragged edge on or just inside a line. Hold Ctrl (⌃) while dragging to do the opposite.",
        },
    ),
    (
        "closeHoldMs",
        KnobDef {
            section: "terminals",
            label: "Hold to close",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(50.0),
                whole: false,
                unit: Some("ms"),
            },
            description: "How long the close button (or the close chord, X) must be held before the terminal is killed; its ring fills over this time.",
        },
    ),
    (
        "barButtonsMinPx",
        KnobDef {
            section: "terminals",
            label: "Bar buttons from",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("px"),
            },
            description: "A hovered title bar shows its agent, hide and close buttons only when the bar is at least this tall on screen.",
        },
    ),
    (
        "agentViewSide",
        KnobDef {
            section: "agent",
            label: "Default side",
            kind: KnobKind::Enum { options: AGENT_SIDE_OPTIONS },
            description: "Which side of a terminal a newly opened Agent View attaches to (drag its header to move it to any side).",
        },
    ),
    (
        "agentViewWidth",
        KnobDef {
            section: "agent",
            label: "Side width",
            kind: KnobKind::Number {
                min: Some(24.0),
                above: None,
                max: Some(300.0),
                step: Some(2.0),
                whole: false,
                unit: Some("world units"),
            },
            description: "World width of an Agent View when attached to the left or right of its terminal.",
        },
    ),
    (
        "agentViewHeight",
        KnobDef {
            section: "agent",
            label: "Top / bottom height",
            kind: KnobKind::Number {
                min: Some(16.0),
                above: None,
                max: Some(240.0),
                step: Some(2.0),
                whole: false,
                unit: Some("world units"),
            },
            description: "World height of an Agent View when attached above or below its terminal.",
        },
    ),
    (
        "agentViewGap",
        KnobDef {
            section: "agent",
            label: "Attachment gap",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: Some(32.0),
                step: Some(0.5),
                whole: false,
                unit: Some("world units"),
            },
            description: "World space between a terminal and its attached Agent View.",
        },
    ),
    (
        "fullscreenMinLinePx",
        KnobDef {
            section: "fullscreen",
            label: "Smallest entry text",
            kind: KnobKind::Number {
                min: Some(4.0),
                above: None,
                max: Some(200.0),
                step: Some(1.0),
                whole: false,
                unit: Some("px a row"),
            },
            description: "A terminal going fullscreen keeps the text size it had on the canvas, so it shows more text; from far out it starts at least this tall instead.",
        },
    ),
    (
        "fullscreenPad",
        KnobDef {
            section: "fullscreen",
            label: "Edge room",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("px"),
            },
            description: "Space kept between a fullscreen terminal and the edge of the canvas (or a docked panel).",
        },
    ),
    (
        "fullscreenTextStep",
        KnobDef {
            section: "fullscreen",
            label: "Text size step",
            kind: KnobKind::Number {
                min: Some(1.01),
                above: None,
                max: Some(3.0),
                step: Some(0.01),
                whole: false,
                unit: Some("×"),
            },
            description: "How much one bigger or smaller text key (chord + = / chord + -) changes a fullscreen terminal's text; it re-wraps to the new size.",
        },
    ),
    (
        "fontFamily",
        KnobDef {
            section: "text",
            label: "Font family",
            kind: KnobKind::Enum { options: FONT_FAMILY_OPTIONS },
            description: "The font every terminal's text is drawn in, with its regular, bold, italic and bold-italic faces. Changing it redraws every terminal at once: each keeps its columns and rows, and its size on the canvas follows the family's cell. Symbols a family lacks come from the fallback fonts; box drawing, blocks and braille are drawn to the cell in every family.",
        },
    ),
    (
        "boldIsBright",
        KnobDef {
            section: "text",
            label: "Bold is bright",
            kind: KnobKind::Bool,
            description: "Bold text in one of the eight standard colours (red, green, …) is drawn in its bright twin as well as in the bold face. Off, bold changes only the face and the colour stays the one the program named.",
        },
    ),
    (
        "lineThickness",
        KnobDef {
            section: "text",
            label: "Line thickness",
            kind: KnobKind::Number {
                min: None,
                above: Some(0.0),
                max: None,
                step: Some(0.05),
                whole: false,
                unit: Some("× the font's"),
            },
            description: "How thick underlines, strikethroughs and overlines are drawn in every terminal, as a multiple of the font's own underline thickness. A line is never thinner than one pixel.",
        },
    ),
    (
        "underlineRaise",
        KnobDef {
            section: "text",
            label: "Underline raise",
            kind: KnobKind::Number {
                min: Some(-1.0),
                above: None,
                max: Some(1.0),
                step: Some(0.005),
                whole: false,
                unit: Some("line heights"),
            },
            description: "Lifts every underline from where the font family puts it (Cousine draws its own under the descenders, close to the next line): 0.05 is a twentieth of a line up; a negative value lowers it.",
        },
    ),
    (
        "curlyAmplitude",
        KnobDef {
            section: "text",
            label: "Curly underline height",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(0.1),
                whole: false,
                unit: Some("× thickness"),
            },
            description: "How tall the wave of a curly underline — the squiggle under a diagnostic — is, as a multiple of the line's thickness; 0 flattens it to a straight line.",
        },
    ),
    (
        "dimAlpha",
        KnobDef {
            section: "text",
            label: "Dim text opacity",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: Some(1.0),
                step: Some(0.02),
                whole: false,
                unit: None,
            },
            description: "How much of its opacity dim (faint) text keeps: 1 draws it like plain text, lower lets the background show through it, 0 hides it.",
        },
    ),
    (
        "groupLayout",
        KnobDef {
            section: "groups",
            label: "New group layout",
            kind: KnobKind::Enum { options: GROUP_LAYOUT_OPTIONS },
            description: "A new group's layout: tile packs its members in columns; freeform leaves each where it is; split cuts their union into panes, each re-wrapped to fill its own.",
        },
    ),
    (
        "groupColumns",
        KnobDef {
            section: "groups",
            label: "New group columns",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: true,
                unit: None,
            },
            description: "A new tiled group's columns; 0 picks the square root of its size, rounded up.",
        },
    ),
    (
        "groupGap",
        KnobDef {
            section: "groups",
            label: "Tile gap",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("world units"),
            },
            description: "Space between a new tiled group's members (a title bar's room is added above each row).",
        },
    ),
    (
        "groupPad",
        KnobDef {
            section: "groups",
            label: "Backdrop margin",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: false,
                unit: Some("world units"),
            },
            description: "How far a group's backdrop reaches past its members' windows.",
        },
    ),
    (
        "splitGap",
        KnobDef {
            section: "groups",
            label: "Split gap",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(0.1),
                whole: false,
                unit: Some("world units"),
            },
            description: "Space between a split's panes; dividers are dragged in it.",
        },
    ),
    (
        "splitBarRows",
        KnobDef {
            section: "groups",
            label: "Split bar room",
            kind: KnobKind::Number {
                min: Some(0.5),
                above: None,
                max: Some(4.0),
                step: Some(0.05),
                whole: false,
                unit: Some("rows"),
            },
            description: "Room above each split pane for its title bar, in the terminal's rows: the bar is never taller, so stacked panes never overlap. Larger keeps the name readable further out.",
        },
    ),
    (
        "splitBars",
        KnobDef {
            section: "groups",
            label: "Split title bars",
            kind: KnobKind::Bool,
            description: "Whether a new split's panes each show a title bar; wall.group.bars (or the group's menu) turns one split's on or off.",
        },
    ),
    (
        "restartNoticeMs",
        KnobDef {
            section: "status",
            label: "Restart notice after",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(50.0),
                whole: false,
                unit: Some("ms"),
            },
            description: "A termd brain restart shows its amber notice only once it has lasted this long; a quick one shows just the chip.",
        },
    ),
    (
        "errorsShown",
        KnobDef {
            section: "status",
            label: "Errors listed",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(1.0),
                whole: true,
                unit: None,
            },
            description: "How many recent termd errors the status window lists, newest first.",
        },
    ),
    (
        "optionAsAlt",
        KnobDef {
            section: "input",
            label: "Option as Alt",
            kind: KnobKind::Enum { options: OPTION_AS_ALT_OPTIONS },
            description: "Whether ⌥/Alt is Alt for the terminal: auto sends Option's character when it made one (å), else Alt; true always Alt (Option+A is ESC a); false types characters. Takes the next key.",
        },
    ),
    (
        "zoomKeys",
        KnobDef {
            section: "input",
            label: "Zoom keys",
            kind: KnobKind::Text { parser: TextParser::ZoomKeys },
            description: "Held with the wheel, any of these zooms the canvas instead of panning it: ctrl, alt, meta, shift, separated by spaces. Keep ctrl for a trackpad pinch (it arrives as Ctrl+wheel); alt is Option and meta is ⌘ from a Mac.",
        },
    ),
    (
        "scrollMultiplier",
        KnobDef {
            section: "input",
            label: "Scroll speed",
            kind: KnobKind::Number {
                min: None,
                above: Some(0.0),
                max: Some(10.0),
                step: Some(0.1),
                whole: false,
                unit: Some("×"),
            },
            description: "Scales how many lines a wheel or two-finger swipe scrolls inside a focused terminal (1 steps one line per row height of travel; higher scrolls faster).",
        },
    ),
    (
        "traceInput",
        KnobDef {
            section: "input",
            label: "Trace input",
            kind: KnobKind::Bool,
            description: "Log every wheel, key and pointer press this page receives (its deltas, key, code, modifiers and the element it landed on) to the relay's log store, as [input] lines — to see what a remote desktop or a trackpad actually sends.",
        },
    ),
    (
        "selectionColor",
        KnobDef {
            section: "selection",
            label: "Selection colour",
            kind: KnobKind::Color,
            description: "The fill behind selected text in every terminal. Ghostty's default: the terminal's own text colour, so a selection reads as the terminal's colours inverted.",
        },
    ),
    (
        "selectionForeground",
        KnobDef {
            section: "selection",
            label: "Selected text colour",
            kind: KnobKind::Text { parser: TextParser::SelectionForeground },
            description: "The colour selected text is drawn in, as #rrggbb — or keep, and each cell keeps its own colour over the selection colour.",
        },
    ),
    (
        "selectionWordChars",
        KnobDef {
            section: "selection",
            label: "Word boundaries",
            kind: KnobKind::Text { parser: TextParser::SelectionWordChars },
            description: "The characters that end a word when you double-click, besides spaces and tabs: a double-click selects the run of other characters around the pointer (or, on one of these, the run of these). Ghostty's default.",
        },
    ),
    (
        "copyOnSelect",
        KnobDef {
            section: "selection",
            label: "Copy on select",
            kind: KnobKind::Enum { options: COPY_ON_SELECT_OPTIONS },
            description: "What a selection is copied to as you finish it: true — the wall's own selection clipboard, which a middle-click on a terminal pastes (a browser page can't reach the system's primary selection); clipboard — that and the system clipboard; false — neither (the copy chord and the menu still copy).",
        },
    ),
    (
        "clickRepeatMs",
        KnobDef {
            section: "selection",
            label: "Double-click time",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(50.0),
                whole: false,
                unit: Some("ms"),
            },
            description: "Presses on a terminal closer together than this (and within a few pixels) count as a double click (a word) or a triple click (a line).",
        },
    ),
    (
        "selectionClearOnTyping",
        KnobDef {
            section: "selection",
            label: "Typing clears it",
            kind: KnobKind::Bool,
            description: "A key typed into a terminal drops its selection, as Ghostty does; off, the selection stays until its text changes.",
        },
    ),
    (
        "noticeMs",
        KnobDef {
            section: "selection",
            label: "Notice time",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: None,
                step: Some(500.0),
                whole: false,
                unit: Some("ms"),
            },
            description: "How long a notice — a copy or paste the browser refused, and why — stays at the top of the wall.",
        },
    ),
    (
        "menuHint",
        KnobDef {
            section: "menu",
            label: "Second-click hint",
            kind: KnobKind::Bool,
            description: "Show the quiet line at the foot of the right-click menu saying a second right-click opens the browser's own menu.",
        },
    ),
    (
        "menuSizes",
        KnobDef {
            section: "menu",
            label: "Terminal sizes",
            kind: KnobKind::Text { parser: TextParser::MenuSizes },
            description: "The sizes a terminal's right-click menu offers to resize it to, as COLSxROWS separated by spaces (like 80x24 120x36); empty offers none.",
        },
    ),
];

/// Look up a knob's [`KnobDef`] by its camelCase key name.
pub fn knob_def(key: &str) -> Option<&'static KnobDef> {
    KNOBS.iter().find(|(k, _)| *k == key).map(|(_, def)| def)
}

/// A validated knob value returned by [`check_knob`] and `config::check_config`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnobValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl KnobValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            KnobValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            KnobValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            KnobValue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for KnobValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KnobValue::Bool(b) => write!(f, "{b}"),
            KnobValue::Number(n) => f.write_str(&format_js_number(*n)),
            KnobValue::Text(s) => f.write_str(s),
        }
    }
}

/// Raw value passed to [`check_knob`] or `config::check_config`: a typed Rust value,
/// a CLI/verb string, or a JSON value loaded from disk.
#[derive(Clone, Debug, PartialEq)]
pub enum KnobInput {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl KnobInput {
    fn as_js_string(&self) -> String {
        match self {
            KnobInput::Bool(b) => b.to_string(),
            KnobInput::Number(n) => format_js_number(*n),
            KnobInput::Text(s) => s.clone(),
        }
    }
}

impl From<bool> for KnobInput {
    fn from(b: bool) -> Self {
        KnobInput::Bool(b)
    }
}

impl From<f64> for KnobInput {
    fn from(n: f64) -> Self {
        KnobInput::Number(n)
    }
}

impl From<i32> for KnobInput {
    fn from(n: i32) -> Self {
        KnobInput::Number(f64::from(n))
    }
}

impl From<u32> for KnobInput {
    fn from(n: u32) -> Self {
        KnobInput::Number(f64::from(n))
    }
}

impl From<&str> for KnobInput {
    fn from(s: &str) -> Self {
        KnobInput::Text(s.to_string())
    }
}

impl From<String> for KnobInput {
    fn from(s: String) -> Self {
        KnobInput::Text(s)
    }
}

impl From<&String> for KnobInput {
    fn from(s: &String) -> Self {
        KnobInput::Text(s.clone())
    }
}

impl From<KnobValue> for KnobInput {
    fn from(value: KnobValue) -> Self {
        match value {
            KnobValue::Bool(b) => KnobInput::Bool(b),
            KnobValue::Number(n) => KnobInput::Number(n),
            KnobValue::Text(s) => KnobInput::Text(s),
        }
    }
}

impl From<&KnobValue> for KnobInput {
    fn from(value: &KnobValue) -> Self {
        value.clone().into()
    }
}

impl From<&serde_json::Value> for KnobInput {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Bool(b) => KnobInput::Bool(*b),
            serde_json::Value::Number(n) => match n.as_f64() {
                Some(f) => KnobInput::Number(f),
                None => KnobInput::Text(n.to_string()),
            },
            serde_json::Value::String(s) => KnobInput::Text(s.clone()),
            serde_json::Value::Null => KnobInput::Text("null".to_string()),
            other => KnobInput::Text(other.to_string()),
        }
    }
}

impl From<serde_json::Value> for KnobInput {
    fn from(value: serde_json::Value) -> Self {
        KnobInput::from(&value)
    }
}

fn format_js_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n == f64::INFINITY {
        "Infinity".to_string()
    } else if n == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else if n == 0.0 {
        "0".to_string()
    } else {
        n.to_string()
    }
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Private port of `chord.parseZoomKeys` so `settings.rs` stays independent of `chord.rs`
/// while sibling modules are ported in parallel.
fn parse_zoom_keys_knob(s: &str) -> Result<Vec<&str>, String> {
    let keys: Vec<&str> = s.split_whitespace().collect();
    for k in &keys {
        if !matches!(*k, "ctrl" | "alt" | "meta" | "shift") {
            return Err(format!("\"{k}\" is not a modifier (ctrl, alt, meta, shift)"));
        }
    }
    Ok(keys)
}

fn parse_selection_foreground_knob(s: &str) -> Result<String, String> {
    let v = s.trim().to_lowercase();
    if v == "keep" || is_hex_color(&v) {
        Ok(v)
    } else {
        Err(format!(
            "a colour as #rrggbb, or keep (each cell keeps its own colour), not '{s}'"
        ))
    }
}

/// Private port of `contextMenu.parseSizes` + `menuSizes` whitespace normalization so
/// `settings.rs` stays independent of `menu.rs` while sibling modules are ported in parallel.
fn parse_menu_sizes_knob(s: &str) -> Result<String, String> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    for tok in &tokens {
        let parts = tok.split_once('x').or_else(|| tok.split_once('×'));
        let valid = match parts {
            Some((cols, rows))
                if !cols.is_empty()
                    && !rows.is_empty()
                    && cols.bytes().all(|b| b.is_ascii_digit())
                    && rows.bytes().all(|b| b.is_ascii_digit()) =>
            {
                let c = cols.parse::<f64>().unwrap_or(0.0);
                let r = rows.parse::<f64>().unwrap_or(0.0);
                c >= 1.0 && r >= 1.0
            }
            _ => false,
        };
        if !valid {
            return Err(format!(
                "sizes are COLSxROWS, space-separated (like \"80x24 120x36\"), not '{tok}'"
            ));
        }
    }
    Ok(tokens.join(" "))
}

/// Validate and coerce `value` against `def` (`wallSettings.checkKnob`).
///
/// Refuses invalid or out-of-range inputs with an error message naming `key`; never clamps.
pub fn check_knob(
    key: &str,
    def: &KnobDef,
    value: impl Into<KnobInput>,
) -> Result<KnobValue, String> {
    let input = value.into();
    match def.kind {
        KnobKind::Bool => {
            if let KnobInput::Bool(b) = input {
                return Ok(KnobValue::Bool(b));
            }
            let display = input.as_js_string();
            let low = display.to_lowercase();
            match low.as_str() {
                "true" | "on" | "1" | "yes" => Ok(KnobValue::Bool(true)),
                "false" | "off" | "0" | "no" => Ok(KnobValue::Bool(false)),
                _ => Err(format!("{key} is on or off, not '{display}'")),
            }
        }
        KnobKind::Color => {
            let display = input.as_js_string();
            let trimmed = display.trim();
            if !is_hex_color(trimmed) {
                return Err(format!("{key} takes a colour as #rrggbb, not '{display}'"));
            }
            Ok(KnobValue::Text(trimmed.to_ascii_lowercase()))
        }
        KnobKind::Text { parser } => {
            let s = input.as_js_string();
            parser
                .parse(&s)
                .map(KnobValue::Text)
                .map_err(|reason| format!("{key}: {reason}"))
        }
        KnobKind::Enum { options } => {
            let s = input.as_js_string();
            if !options.contains(&s.as_str()) {
                return Err(format!("{key} is {}, not '{s}'", options.join(", ")));
            }
            Ok(KnobValue::Text(s))
        }
        KnobKind::Number { min, above, max, whole, .. } => {
            let display = input.as_js_string();
            let parsed = match input {
                KnobInput::Number(n) => Some(n),
                _ => {
                    let trimmed = display.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        trimmed.parse::<f64>().ok()
                    }
                }
            };
            let Some(raw_n) = parsed.filter(|n| n.is_finite()) else {
                return Err(format!("{key} takes a number, not '{display}'"));
            };
            let n = if raw_n == 0.0 { 0.0 } else { raw_n };
            if let Some(min_v) = min {
                if n < min_v {
                    return Err(format!(
                        "{key} takes a number ≥ {}, not '{display}'",
                        format_js_number(min_v)
                    ));
                }
            }
            if let Some(above_v) = above {
                if n <= above_v {
                    return Err(format!(
                        "{key} takes a number > {}, not '{display}'",
                        format_js_number(above_v)
                    ));
                }
            }
            if let Some(max_v) = max {
                if n > max_v {
                    return Err(format!(
                        "{key} takes a number ≤ {}, not '{display}'",
                        format_js_number(max_v)
                    ));
                }
            }
            if whole && n.fract() != 0.0 {
                return Err(format!("{key} takes a whole number, not '{display}'"));
            }
            Ok(KnobValue::Number(n))
        }
    }
}

// ── search ───────────────────────────────────────────────────────────────────

/// Half-open `[start, end)` UTF-8 byte range into the searched text.
pub type HitRange = [usize; 2];

/// Sorted, non-overlapping `[start, end)` highlight spans.
pub type Hits = Vec<HitRange>;

/// Find where any of `terms` (expected in lowercase) occur in `text` (case-insensitively),
/// returning sorted, merged `[start, end)` byte ranges into `text` (`wallSettings.hitsIn`).
pub fn hits_in<S: AsRef<str>>(text: &str, terms: &[S]) -> Hits {
    if text.is_empty() || terms.is_empty() {
        return Vec::new();
    }

    // Build `low` char-by-char so multi-byte or case-folding length changes map back cleanly
    // to valid UTF-8 byte boundaries in `text`.
    let mut low = String::with_capacity(text.len());
    let mut char_map: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (orig_start, ch) in text.char_indices() {
        let orig_end = orig_start + ch.len_utf8();
        let low_start = low.len();
        for lc in ch.to_lowercase() {
            low.push(lc);
        }
        let low_end = low.len();
        char_map.push((low_start, low_end, orig_start, orig_end));
    }

    let map_start = |low_idx: usize| -> usize {
        char_map
            .iter()
            .find(|(ls, le, _, _)| low_idx >= *ls && low_idx < *le)
            .map(|(_, _, os, _)| *os)
            .unwrap_or(text.len())
    };
    let map_end = |low_idx: usize| -> usize {
        if low_idx == 0 {
            return 0;
        }
        char_map
            .iter()
            .rfind(|(ls, _, _, _)| *ls < low_idx)
            .map(|(_, _, _, oe)| *oe)
            .unwrap_or(text.len())
    };

    let mut raw: Hits = Vec::new();
    for term_ref in terms {
        let t = term_ref.as_ref();
        if t.is_empty() {
            continue;
        }
        let mut search_from = 0;
        while search_from <= low.len() {
            let Some(rel) = low[search_from..].find(t) else {
                break;
            };
            let m_start = search_from + rel;
            let m_end = m_start + t.len();
            raw.push([map_start(m_start), map_end(m_end)]);
            let step = low[m_start..].chars().next().map_or(1, char::len_utf8);
            search_from = m_start + step;
        }
    }

    raw.sort_unstable_by(|a, b| a[0].cmp(&b[0]).then_with(|| a[1].cmp(&b[1])));
    let mut out: Hits = Vec::with_capacity(raw.len());
    for range in raw {
        if let Some(last) = out.last_mut() {
            if range[0] <= last[1] {
                last[1] = last[1].max(range[1]);
                continue;
            }
        }
        out.push(range);
    }
    out
}

/// Highlight spans for one matching knob in [`search_settings`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemHits {
    pub label: Hits,
    pub key: Hits,
    pub description: Hits,
}

/// One matching knob inside a [`SettingsGroup`].
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsItem<'a> {
    pub key: &'a str,
    pub def: KnobDef,
    pub hits: ItemHits,
}

/// One matching section returned by [`search_settings`], carrying its matching knobs in order.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsGroup<'a> {
    pub section: SettingsSection,
    pub title: Hits,
    pub items: Vec<SettingsItem<'a>>,
}

/// Filter [`SECTIONS`] and [`KNOBS`] by `query` (`wallSettings.searchSettings`).
///
/// Every whitespace-delimited word in `query` (case-insensitive) must appear in at least one of
/// the knob's label, key, description, or section title. Sections with no matching knobs are
/// omitted; an empty query returns every section that has knobs, with empty hit lists.
pub fn search_settings(query: &str) -> Vec<SettingsGroup<'static>> {
    search_settings_in(query, SECTIONS, KNOBS)
}

/// Filter an explicit `(sections, knobs)` schema by `query` (matching JS `searchSettings(query, { sections, knobs })`).
pub fn search_settings_in<'a>(
    query: &str,
    sections: &[SettingsSection],
    knobs: &'a [(&'a str, KnobDef)],
) -> Vec<SettingsGroup<'a>> {
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut out = Vec::new();
    for &section in sections {
        let title_low = section.title.to_lowercase();
        let mut items = Vec::new();
        for &(key, def) in knobs {
            if def.section != section.id {
                continue;
            }
            let hay = [
                def.label.to_lowercase(),
                key.to_lowercase(),
                def.description.to_lowercase(),
                title_low.clone(),
            ];
            if !terms.iter().all(|t| hay.iter().any(|h| h.contains(t.as_str()))) {
                continue;
            }
            items.push(SettingsItem {
                key,
                def,
                hits: ItemHits {
                    label: hits_in(def.label, &terms),
                    key: hits_in(key, &terms),
                    description: hits_in(def.description, &terms),
                },
            });
        }
        if !items.is_empty() {
            out.push(SettingsGroup {
                section,
                title: hits_in(section.title, &terms),
                items,
            });
        }
    }
    out
}

/// Assert the compile-time schema invariant from `wallSettings.js`: every entry in [`KNOBS`]
/// names a section present in [`SECTIONS`].
pub fn assert_sections_cover_knobs() {
    for &(key, def) in KNOBS {
        assert!(
            SECTIONS.iter().any(|s| s.id == def.section),
            "[wall] knob '{key}' names section '{}', which SECTIONS lacks",
            def.section
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::{check_config, default_knob_value, WALL_CONFIG_KEYS};

    #[test]
    fn every_knob_is_described_and_its_default_passes_its_own_check() {
        assert_sections_cover_knobs();
        let mut knob_keys: Vec<&str> = KNOBS.iter().map(|(k, _)| *k).collect();
        let mut config_keys: Vec<&str> = WALL_CONFIG_KEYS.to_vec();
        knob_keys.sort_unstable();
        config_keys.sort_unstable();
        assert_eq!(knob_keys, config_keys);

        let sections: BTreeSet<&str> = SECTIONS.iter().map(|s| s.id).collect();
        for &(key, ref def) in KNOBS {
            assert!(sections.contains(def.section), "unknown section for {key}");
            assert!(!def.label.is_empty(), "empty label for {key}");
            assert!(
                def.description.chars().count() > 20,
                "short description for {key}: {}",
                def.description
            );
            assert!(
                ["bool", "number", "color", "enum", "text"].contains(&def.knob_type().as_str()),
                "unexpected type for {key}"
            );
            let default_val = default_knob_value(key).unwrap();
            assert_eq!(check_config(key, &default_val).unwrap(), default_val);
            // As a verb sends it: stringified.
            assert_eq!(check_config(key, default_val.to_string()).unwrap(), default_val);
        }
        for section in SECTIONS {
            assert!(
                KNOBS.iter().any(|(_, d)| d.section == section.id),
                "section {} has no knobs",
                section.id
            );
        }
    }

    #[test]
    fn values_are_checked_by_type_and_refused_naming_the_knob() {
        assert_eq!(check_config("snap", "off").unwrap(), KnobValue::Bool(false));
        assert_eq!(check_config("gridShow", "true").unwrap(), KnobValue::Bool(true));
        assert!(check_config("snap", "sometimes")
            .unwrap_err()
            .contains("snap is on or off"));
        assert_eq!(
            check_config("background", "#A0B1C2").unwrap(),
            KnobValue::Text("#a0b1c2".to_string())
        );
        assert!(check_config("background", "red")
            .unwrap_err()
            .contains("background takes a colour as #rrggbb"));
        assert!(check_config("gridMinor", "0")
            .unwrap_err()
            .contains("gridMinor takes a number > 0"));
        assert_eq!(check_config("gridMinor", "0.5").unwrap(), KnobValue::Number(0.5));
        assert_eq!(check_config("gridMinor", "1e6").unwrap(), KnobValue::Number(1e6));
        assert!(check_config("gridMajorEvery", "2.5")
            .unwrap_err()
            .contains("whole number"));
        assert!(check_config("gridMajorEvery", "0").unwrap_err().contains("≥ 1"));
        assert!(check_config("gridLinePx", "thin")
            .unwrap_err()
            .contains("gridLinePx takes a number"));
        assert!(check_config("groupLayout", "grid")
            .unwrap_err()
            .contains("groupLayout is tile, freeform"));
        assert_eq!(
            check_config("fontFamily", "cousine").unwrap(),
            KnobValue::Text("cousine".to_string())
        );
        assert!(check_config("fontFamily", "helvetica")
            .unwrap_err()
            .contains("fontFamily is cousine, monaspace-neon, monaspace-argon, not 'helvetica'"));
        assert_eq!(check_config("boldIsBright", "on").unwrap(), KnobValue::Bool(true));
        assert!(check_config("nope", "1")
            .unwrap_err()
            .contains("unknown wall knob 'nope'"));
    }

    #[test]
    fn text_knob_parsers_validate_and_normalize() {
        assert_eq!(
            check_config("zoomKeys", "  ctrl   meta ").unwrap(),
            KnobValue::Text("ctrl meta".to_string())
        );
        assert_eq!(
            check_config("zoomKeys", "").unwrap(),
            KnobValue::Text(String::new())
        );
        assert!(check_config("zoomKeys", "ctrl hyper")
            .unwrap_err()
            .contains("zoomKeys: \"hyper\" is not a modifier"));

        assert_eq!(
            check_config("selectionForeground", " KEEP ").unwrap(),
            KnobValue::Text("keep".to_string())
        );
        assert_eq!(
            check_config("selectionForeground", "#AABBCC").unwrap(),
            KnobValue::Text("#aabbcc".to_string())
        );
        assert!(check_config("selectionForeground", "blue")
            .unwrap_err()
            .contains("selectionForeground: a colour as #rrggbb, or keep"));

        assert_eq!(
            check_config("selectionWordChars", " : ; , ").unwrap(),
            KnobValue::Text(":;,".to_string())
        );

        assert_eq!(
            check_config("menuSizes", "  80x24   120×36 ").unwrap(),
            KnobValue::Text("80x24 120×36".to_string())
        );
        assert_eq!(
            check_config("menuSizes", "   ").unwrap(),
            KnobValue::Text(String::new())
        );
        assert!(check_config("menuSizes", "80x0")
            .unwrap_err()
            .contains("menuSizes: sizes are COLSxROWS"));
    }

    #[test]
    fn search_matches_label_key_description_or_section_title_and_marks_hits() {
        let keys = |q: &str| -> Vec<&str> {
            search_settings(q)
                .into_iter()
                .flat_map(|g| g.items.into_iter().map(|it| it.key))
                .collect()
        };
        let sections = |q: &str| -> Vec<&str> {
            search_settings(q).into_iter().map(|g| g.section.id).collect()
        };

        let expected_all: Vec<&str> = KNOBS
            .iter()
            .filter(|(_, d)| SECTIONS.iter().any(|s| s.id == d.section))
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(keys(""), expected_all);
        assert!(keys("snap").contains(&"snap"));
        assert_eq!(sections("snap"), vec!["canvas"]);

        let mut colour_keys = keys("colour");
        colour_keys.sort_unstable();
        assert_eq!(
            colour_keys,
            vec![
                "background",
                "boldIsBright",
                "gridMajorColor",
                "gridMinorColor",
                "selectionColor",
                "selectionForeground",
            ]
        );
        assert_eq!(keys("font"), vec!["fontFamily", "lineThickness", "underlineRaise"]);
        assert_eq!(sections("font family"), vec!["text"]);

        let expected_groups: Vec<&str> = KNOBS
            .iter()
            .filter(|(_, d)| d.section == "groups")
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(keys("GROUPS"), expected_groups);
        assert_eq!(keys("major colour"), vec!["gridMajorColor"]);
        assert_eq!(keys("closehold"), vec!["closeHoldMs"]);
        assert!(search_settings("zebra").is_empty());

        // `wall.config.find major` in wall-settings.test.mjs:
        let major_groups = search_settings("major");
        assert_eq!(major_groups.len(), 1);
        assert_eq!(major_groups[0].section.id, "canvas");
        let major_keys: Vec<&str> = major_groups[0].items.iter().map(|it| it.key).collect();
        assert_eq!(major_keys, vec!["gridMajorEvery", "gridMinPx", "gridMajorColor"]);

        let canvas_group = search_settings("grid line")
            .into_iter()
            .find(|g| g.section.id == "canvas")
            .unwrap();
        let width = canvas_group
            .items
            .iter()
            .find(|it| it.key == "gridLinePx")
            .unwrap();
        assert_eq!(width.hits.label, vec![[0, 4]]);
        assert_eq!(width.hits.key, vec![[0, 8]]);
        assert_eq!(hits_in("aaa", &["a", "aa"]), vec![[0, 3]]);
    }
}
