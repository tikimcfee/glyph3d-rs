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

/// The settings window's sections, in the order its list shows them
/// (`wallSettings.SECTIONS`, regrouped 2026-11-27 under names a person would
/// look for: what the browser panel kept under `menu` is with the terminals
/// it resizes, and every notice's timing is under `status`). The ids are the
/// knobs' `section` values and what `wall.settings section` takes.
pub const SECTIONS: &[SettingsSection] = &[
    SettingsSection {
        id: "terminals",
        title: "Terminals",
        subtitle: "size · closing · title bar · cursor",
    },
    SettingsSection {
        id: "text",
        title: "Text",
        subtitle: "font · size · spacing · underlines",
    },
    SettingsSection {
        id: "canvas",
        title: "Canvas & grid",
        subtitle: "background · grid · snapping",
    },
    SettingsSection {
        id: "groups",
        title: "Groups & splits",
        subtitle: "what a new group or split starts with",
    },
    SettingsSection {
        id: "fullscreen",
        title: "Fullscreen",
        subtitle: "text size · margin",
    },
    SettingsSection {
        id: "selection",
        title: "Selection & copy",
        subtitle: "colours · double-click · copying",
    },
    SettingsSection {
        id: "input",
        title: "Keyboard & mouse",
        subtitle: "Option key · zoom · scrolling",
    },
    SettingsSection {
        id: "agent",
        title: "Agent View",
        subtitle: "where it opens · how big",
    },
    SettingsSection {
        id: "status",
        title: "Notices",
        subtitle: "how long they stay · termd status",
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
    /// Trim whitespace and require a non-empty value (e.g. a font family name).
    NonEmptyTrim,
    /// Space-separated modifier names (`ctrl`, `alt`, `meta`, `shift`).
    ZoomKeys,
    /// `#rrggbb` hex colour (lowercased) or `"keep"`.
    SelectionForeground,
    /// Strip all whitespace characters (`s.replace(/\s+/g, '')`).
    SelectionWordChars,
    /// Space-separated `COLSxROWS` pairs (`80x24 120x36`), empty allowed.
    MenuSizes,
    /// Exactly one `COLSxROWS` pair (`100x30`), written back with a plain `x`.
    TerminalSize,
}

impl TextParser {
    /// Parse and normalize `input`, returning the normalized text or a reason string
    /// (without the knob prefix, which [`check_knob`] prepends as `"{key}: {reason}"`).
    pub fn parse(self, input: &str) -> Result<String, String> {
        match self {
            TextParser::Trim => Ok(input.trim().to_string()),
            TextParser::NonEmptyTrim => {
                let s = input.trim();
                if s.is_empty() {
                    Err("cannot be empty".to_string())
                } else {
                    Ok(s.to_string())
                }
            }
            TextParser::ZoomKeys => crate::chord::parse_zoom_keys(input).map(|keys| {
                keys.into_iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            TextParser::SelectionForeground => parse_selection_foreground_knob(input),
            TextParser::SelectionWordChars => {
                Ok(input.chars().filter(|ch| !ch.is_whitespace()).collect())
            }
            TextParser::MenuSizes => {
                crate::menu::parse_sizes(input)?;
                Ok(input.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            TextParser::TerminalSize => {
                let size = crate::menu::parse_size(input)?;
                Ok(format!("{}x{}", size.cols, size.rows))
            }
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

/// Schema definition of one wall knob: its section, label, meaning, and type constraints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnobDef {
    pub section: &'static str,
    pub label: &'static str,
    /// What the setting does, in one plain line: the words under its label
    /// in the settings window (2026-11-27). Never the knob's key.
    pub summary: &'static str,
    /// The whole of it: its values, units and consequences. The window
    /// shows it on hover, and where a search matched it.
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
pub(crate) const GROUP_LAYOUT_OPTIONS: &[&str] = &["tile", "freeform", "split"];
pub(crate) const OPTION_AS_ALT_OPTIONS: &[&str] = &["auto", "true", "false"];
pub(crate) const COPY_ON_SELECT_OPTIONS: &[&str] = &["true", "clipboard", "false"];

/// Every wall knob's definition, in the settings window's order: section by section
/// ([`SECTIONS`]), and within each as its list shows them (`wallSettings.KNOBS`).
pub const KNOBS: &[(&str, KnobDef)] = &[
    (
        "newTerminalSize",
        KnobDef {
            section: "terminals",
            label: "New terminal size",
            summary: "How big a new terminal opens, in columns by rows.",
            kind: KnobKind::Text { parser: TextParser::TerminalSize },
            description: "The size a new terminal opens at, as COLSxROWS (like 100x30): the + button, wall.spawn, a split's new pane and an attached agent's terminal. A terminal that is already open keeps its size.",
        },
    ),
    (
        "menuSizes",
        KnobDef {
            section: "terminals",
            label: "Sizes in the menu",
            summary: "The sizes a terminal's right-click menu offers to resize it to.",
            kind: KnobKind::Text { parser: TextParser::MenuSizes },
            description: "The sizes a terminal's right-click menu offers to resize it to, as COLSxROWS separated by spaces (like 80x24 120x36); empty offers none.",
        },
    ),
    (
        "closeHoldMs",
        KnobDef {
            section: "terminals",
            label: "Hold to close",
            summary: "How long you hold the close button before a terminal is closed.",
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
            label: "Title-bar buttons from",
            summary: "How tall a title bar must be on screen before its buttons show.",
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
        "panelOpacity",
        KnobDef {
            section: "terminals",
            label: "Terminal opacity",
            summary: "How solid a terminal's backing is; lower lets the canvas show through.",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: Some(1.0),
                step: Some(0.05),
                whole: false,
                unit: None,
            },
            description: "Opacity of each terminal's background panel: 1 is solid opaque, lower lets the canvas grid show through, 0 is transparent.",
        },
    ),
    (
        "cursorBlinkMs",
        KnobDef {
            section: "terminals",
            label: "Cursor blink",
            summary: "How fast the cursor blinks; 0 keeps it steady.",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: Some(5000.0),
                step: Some(50.0),
                whole: true,
                unit: Some("ms"),
            },
            description: "Half-period of a blinking cursor in milliseconds; 0 keeps every cursor steady.",
        },
    ),
    (
        "fontFamily",
        KnobDef {
            section: "text",
            label: "Font family",
            summary: "The font terminals are drawn in.",
            kind: KnobKind::Text {
                parser: TextParser::NonEmptyTrim,
            },
            description: "The font every terminal's text is drawn in (bundled: monaspace-neon, monaspace-argon, cousine; or any installed system monospace family such as Menlo or SF Mono). Changing it loads the family live, re-measures the cell, and re-wraps each terminal to keep its panel size.",
        },
    ),
    (
        "fontSize",
        KnobDef {
            section: "text",
            label: "Font size",
            summary: "How big terminal text is.",
            kind: KnobKind::Number {
                min: Some(6.0),
                above: None,
                max: Some(72.0),
                step: Some(0.5),
                whole: false,
                unit: Some("pt"),
            },
            description: "The base font size for every terminal (14 pt default). Changing it scales the cell and re-wraps each terminal's grid to keep its panel size.",
        },
    ),
    (
        "lineHeight",
        KnobDef {
            section: "text",
            label: "Line height",
            summary: "The space between rows of text.",
            kind: KnobKind::Number {
                min: Some(0.8),
                above: None,
                max: Some(2.5),
                step: Some(0.05),
                whole: false,
                unit: Some("×"),
            },
            description: "Vertical cell pitch multiplier (1.0 = natural font height; higher adds leading between rows without stretching glyphs) and re-wraps rows to keep each panel's height.",
        },
    ),
    (
        "boldIsBright",
        KnobDef {
            section: "text",
            label: "Bold is bright",
            summary: "Draw bold text in the brighter version of its colour.",
            kind: KnobKind::Bool,
            description: "Bold text in one of the eight standard colours (red, green, …) is drawn in its bright twin as well as in the bold face. Off, bold changes only the face and the colour stays the one the program named.",
        },
    ),
    (
        "dimAlpha",
        KnobDef {
            section: "text",
            label: "Dim text opacity",
            summary: "How faint dim text is.",
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
        "lineThickness",
        KnobDef {
            section: "text",
            label: "Underline thickness",
            summary: "How thick underlines and strikethroughs are.",
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
            summary: "Move underlines up or down.",
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
            summary: "How tall the wave of a curly underline is.",
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
        "greekPx",
        KnobDef {
            section: "text",
            label: "Simplify text below",
            summary: "Zoomed out, text smaller than this is drawn as plain bars.",
            kind: KnobKind::Number {
                min: Some(0.0),
                above: None,
                max: Some(32.0),
                step: Some(1.0),
                whole: false,
                unit: Some("px"),
            },
            description: "Cell row height on screen below which glyphs simplify into horizontal bars; 0 disables greeking.",
        },
    ),
    (
        "background",
        KnobDef {
            section: "canvas",
            label: "Background colour",
            summary: "The colour behind everything on the canvas.",
            kind: KnobKind::Color,
            description: "The canvas colour behind every terminal and group.",
        },
    ),
    (
        "gridShow",
        KnobDef {
            section: "canvas",
            label: "Show grid",
            summary: "Draw the grid on the canvas.",
            kind: KnobKind::Bool,
            description: "Draw the drafting-paper grid on the canvas. Snapping works with the grid hidden too.",
        },
    ),
    (
        "snap",
        KnobDef {
            section: "canvas",
            label: "Snap to grid",
            summary: "Moves and resizes land on the grid.",
            kind: KnobKind::Bool,
            description: "Moving a terminal puts its top-left corner on a grid crossing; resizing lands the dragged edge on or just inside a line. Hold Ctrl (⌃) while dragging to do the opposite.",
        },
    ),
    (
        "gridMinor",
        KnobDef {
            section: "canvas",
            label: "Grid spacing",
            summary: "How far apart the grid lines are, which is also the snap step.",
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
            summary: "How often a brighter line is drawn.",
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
            summary: "Zoomed out, lines packed tighter than this are left out.",
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
            summary: "How thick the lines are.",
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
            summary: "The colour of the grid lines.",
            kind: KnobKind::Color,
            description: "The colour of the fine grid lines.",
        },
    ),
    (
        "gridMajorColor",
        KnobDef {
            section: "canvas",
            label: "Major line colour",
            summary: "The colour of the brighter major lines.",
            kind: KnobKind::Color,
            description: "The colour of every major grid line.",
        },
    ),
    (
        "renderLoop",
        KnobDef {
            section: "canvas",
            label: "Render loop",
            summary: "Redraw only when something changes, or on every frame.",
            kind: KnobKind::Enum { options: RENDER_LOOP_OPTIONS },
            description: "demand draws the canvas only when something changed (input, output, a move) and idles the GPU otherwise; always draws every display frame — a check when something looks stale.",
        },
    ),
    (
        "groupLayout",
        KnobDef {
            section: "groups",
            label: "New group layout",
            summary: "How a new group arranges its terminals.",
            kind: KnobKind::Enum { options: GROUP_LAYOUT_OPTIONS },
            description: "A new group's layout: tile packs its members in columns; freeform leaves each where it is; split cuts their union into panes, each re-wrapped to fill its own.",
        },
    ),
    (
        "groupColumns",
        KnobDef {
            section: "groups",
            label: "New group columns",
            summary: "How many columns a new tiled group has; 0 picks for you.",
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
            summary: "The space between the terminals of a tiled group.",
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
            summary: "How far a group's backdrop reaches past its terminals.",
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
            summary: "The space between the panes of a split.",
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
            label: "Split title-bar room",
            summary: "The room kept above each pane of a split for its title bar.",
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
            summary: "Give each pane of a new split a title bar.",
            kind: KnobKind::Bool,
            description: "Whether a new split's panes each show a title bar; wall.group.bars (or the group's menu) turns one split's on or off.",
        },
    ),
    (
        "fullscreenMinLinePx",
        KnobDef {
            section: "fullscreen",
            label: "Smallest fullscreen text",
            summary: "The smallest text a terminal starts fullscreen with.",
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
            label: "Fullscreen margin",
            summary: "The space kept around a fullscreen terminal.",
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
            summary: "How much one press of the bigger or smaller text key changes fullscreen text.",
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
        "selectionColor",
        KnobDef {
            section: "selection",
            label: "Selection colour",
            summary: "The highlight colour behind selected text.",
            kind: KnobKind::Color,
            description: "The fill behind selected text in every terminal. Ghostty's default: the terminal's own text colour, so a selection reads as the terminal's colours inverted.",
        },
    ),
    (
        "selectionForeground",
        KnobDef {
            section: "selection",
            label: "Selected text colour",
            summary: "The colour selected text is drawn in, or keep each character's own.",
            kind: KnobKind::Text { parser: TextParser::SelectionForeground },
            description: "The colour selected text is drawn in, as #rrggbb — or keep, and each cell keeps its own colour over the selection colour.",
        },
    ),
    (
        "selectionWordChars",
        KnobDef {
            section: "selection",
            label: "Word boundaries",
            summary: "The characters that end a word when you double-click.",
            kind: KnobKind::Text { parser: TextParser::SelectionWordChars },
            description: "The characters that end a word when you double-click, besides spaces and tabs: a double-click selects the run of other characters around the pointer (or, on one of these, the run of these). Ghostty's default.",
        },
    ),
    (
        "clickRepeatMs",
        KnobDef {
            section: "selection",
            label: "Double-click time",
            summary: "How quickly two clicks must follow each other to count as a double click.",
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
        "copyOnSelect",
        KnobDef {
            section: "selection",
            label: "Copy on select",
            summary: "Copy text as soon as you finish selecting it.",
            kind: KnobKind::Enum { options: COPY_ON_SELECT_OPTIONS },
            description: "What a selection is copied to as you finish it: true — the wall's own selection clipboard, which a middle-click on a terminal pastes (a browser page can't reach the system's primary selection); clipboard — that and the system clipboard; false — neither (the copy chord and the menu still copy).",
        },
    ),
    (
        "selectionClearOnTyping",
        KnobDef {
            section: "selection",
            label: "Typing clears selection",
            summary: "Typing into a terminal drops what was selected in it.",
            kind: KnobKind::Bool,
            description: "A key typed into a terminal drops its selection, as Ghostty does; off, the selection stays until its text changes.",
        },
    ),
    (
        "optionAsAlt",
        KnobDef {
            section: "input",
            label: "Option as Alt",
            summary: "Whether the Option key acts as Alt in terminals.",
            kind: KnobKind::Enum { options: OPTION_AS_ALT_OPTIONS },
            description: "Whether ⌥/Alt is Alt for the terminal: auto sends Option's character when it made one (å), else Alt; true always Alt (Option+A is ESC a); false types characters. Takes the next key.",
        },
    ),
    (
        "zoomKeys",
        KnobDef {
            section: "input",
            label: "Zoom keys",
            summary: "The keys that make the wheel zoom instead of pan.",
            kind: KnobKind::Text { parser: TextParser::ZoomKeys },
            description: "Held with the wheel, any of these zooms the canvas instead of panning it: ctrl, alt, meta, shift, separated by spaces. Keep ctrl for a trackpad pinch (it arrives as Ctrl+wheel); alt is Option and meta is ⌘ from a Mac.",
        },
    ),
    (
        "scrollMultiplier",
        KnobDef {
            section: "input",
            label: "Scroll speed",
            summary: "How fast the wheel scrolls inside a terminal.",
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
            summary: "Log every key, wheel and pointer event the wall receives.",
            kind: KnobKind::Bool,
            description: "Log every wheel, key and pointer press this page receives (its deltas, key, code, modifiers and the element it landed on) to the relay's log store, as [input] lines — to see what a remote desktop or a trackpad actually sends.",
        },
    ),
    (
        "agentViewSide",
        KnobDef {
            section: "agent",
            label: "Default side",
            summary: "Which side of its terminal an Agent View opens on.",
            kind: KnobKind::Enum { options: AGENT_SIDE_OPTIONS },
            description: "Which side of a terminal a newly opened Agent View attaches to (drag its header to move it to any side).",
        },
    ),
    (
        "agentViewWidth",
        KnobDef {
            section: "agent",
            label: "Width at the side",
            summary: "How wide an Agent View is beside its terminal.",
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
            label: "Height above or below",
            summary: "How tall an Agent View is above or below its terminal.",
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
            label: "Gap to the terminal",
            summary: "The space between a terminal and its Agent View.",
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
        "noticeMs",
        KnobDef {
            section: "status",
            label: "Notice time",
            summary: "How long a notice stays at the top of the wall.",
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
        "restartNoticeMs",
        KnobDef {
            section: "status",
            label: "Restart notice after",
            summary: "How long termd must be restarting before a notice appears.",
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
            summary: "How many recent termd errors the status window lists.",
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
        "menuHint",
        KnobDef {
            section: "status",
            label: "Second-click hint",
            summary: "Show the hint at the foot of the right-click menu.",
            kind: KnobKind::Bool,
            description: "Show the quiet line at the foot of the right-click menu saying a second right-click opens the browser's own menu.",
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
    pub summary: Hits,
    pub key: Hits,
    pub description: Hits,
}

impl ItemHits {
    /// Whether the search matched only what the window does not always
    /// show (the key, the long description, or the section's title): the
    /// row then shows where the match was.
    pub fn only_in_the_details(&self) -> bool {
        self.label.is_empty() && self.summary.is_empty()
    }
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
/// the knob's label, one-line meaning, key, description, or section title. Sections with no matching knobs are
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
                def.summary.to_lowercase(),
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
                    summary: hits_in(def.summary, &terms),
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

// ── the settings window ─────────────────────────────────────────────────────

/// One line of the settings window's section list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionEntry {
    pub section: SettingsSection,
    /// The settings it holds; while searching, the ones that match.
    pub count: usize,
    /// Whether its settings are what the right-hand side shows.
    pub selected: bool,
}

/// One setting on the right-hand side of the settings window.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingRow<'a> {
    pub key: &'a str,
    pub def: KnobDef,
    /// The section it is in.
    pub section: SettingsSection,
    /// Whether it is the first row of its section among the rows shown:
    /// where a search's results name the section the rows under it are in.
    pub starts_section: bool,
    pub hits: ItemHits,
}

/// What the settings window shows for a query and a chosen section
/// ([`settings_view`]): the list on its left and the settings on its right.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsView<'a> {
    /// Whether a search is narrowing it (the query has a word in it).
    pub searching: bool,
    /// Every section with a setting to show, in order. While searching a
    /// section nothing matched in stays in the list, with a count of 0.
    pub sections: Vec<SectionEntry>,
    /// The section the right-hand side is showing. `None`: every section's
    /// matches, a search that was not narrowed to one.
    pub shown: Option<&'static str>,
    /// How many settings the search matched, over every section (all of
    /// them when not searching).
    pub matched: usize,
    /// The settings on the right-hand side, top to bottom.
    pub rows: Vec<SettingRow<'a>>,
}

/// Lay the settings window out (2026-11-27): a section list and the
/// settings of one section, or of a search.
///
/// - **Browsing** (an empty `query`): the rows are the settings of
///   `section`, or of the first section when `section` is `None` or names
///   none that has settings.
/// - **Searching**: every setting the query matches ([`search_settings`]),
///   over every section, each run of rows starting with its section
///   ([`SettingRow::starts_section`]). `section` narrows the rows to that
///   section's matches, when it has any; the list's counts are the matches
///   in each.
///
/// `hidden` are knobs the window does not show (the native wall hides the
/// browser-only ones); a section left with none is not listed.
pub fn settings_view(query: &str, section: Option<&str>, hidden: &[&str]) -> SettingsView<'static> {
    let searching = !query.trim().is_empty();
    let shown_of = |groups: Vec<SettingsGroup<'static>>| -> Vec<SettingsGroup<'static>> {
        groups
            .into_iter()
            .filter_map(|mut group| {
                group.items.retain(|it| !hidden.contains(&it.key));
                (!group.items.is_empty()).then_some(group)
            })
            .collect()
    };
    // Every section with a setting to show, whatever the query.
    let listed: Vec<SettingsSection> = shown_of(search_settings(""))
        .into_iter()
        .map(|g| g.section)
        .collect();
    let groups = shown_of(search_settings(query));
    let matched: usize = groups.iter().map(|g| g.items.len()).sum();

    let named = section.filter(|id| groups.iter().any(|g| g.section.id == *id));
    let shown: Option<&'static str> = if searching {
        named.and_then(|id| listed.iter().find(|s| s.id == id).map(|s| s.id))
    } else {
        named
            .and_then(|id| listed.iter().find(|s| s.id == id))
            .or(listed.first())
            .map(|s| s.id)
    };

    let sections = listed
        .iter()
        .map(|&section| SectionEntry {
            section,
            count: groups
                .iter()
                .find(|g| g.section.id == section.id)
                .map_or(0, |g| g.items.len()),
            selected: shown == Some(section.id),
        })
        .collect();

    let mut rows = Vec::new();
    for group in groups {
        if shown.is_some_and(|id| id != group.section.id) {
            continue;
        }
        for (at, item) in group.items.into_iter().enumerate() {
            rows.push(SettingRow {
                key: item.key,
                def: item.def,
                section: group.section,
                starts_section: at == 0,
                hits: item.hits,
            });
        }
    }

    SettingsView {
        searching,
        sections,
        shown,
        matched,
        rows,
    }
}

/// The section after (`step > 0`) or before the one `view` shows, in the
/// list's order and stopping at its ends: what the window's next-section
/// and previous-section keys select. While a search shows every section's
/// matches, the first step goes to the first section that has any.
pub fn step_section(view: &SettingsView<'_>, step: i32) -> Option<&'static str> {
    let with_rows: Vec<&'static str> = view
        .sections
        .iter()
        .filter(|s| s.count > 0)
        .map(|s| s.section.id)
        .collect();
    let at = view
        .shown
        .and_then(|id| with_rows.iter().position(|s| *s == id));
    let next = match at {
        None => 0,
        Some(i) if step > 0 => (i + 1).min(with_rows.len().saturating_sub(1)),
        Some(i) => i.saturating_sub(1),
    };
    with_rows.get(next).copied()
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
            // One plain line: a sentence a person can read, which is not
            // the knob's code name and does not lean on it.
            assert!(
                (12..=80).contains(&def.summary.chars().count()),
                "{key}: a one-line meaning, not {:?}",
                def.summary
            );
            assert!(def.summary.ends_with('.'), "{key}: {:?}", def.summary);
            assert!(!def.summary.contains('\n'), "{key}: one line");
            // (A verb's name has a dot inside a word: `wall.config`.)
            let names_a_verb = def
                .summary
                .trim_end_matches('.')
                .split_whitespace()
                .any(|word| word.contains('.') && word.chars().any(char::is_alphabetic));
            assert!(
                !def.summary.contains(key) && !names_a_verb,
                "{key}: its meaning names the knob or a verb: {:?}",
                def.summary
            );
            assert_ne!(def.summary, def.description, "{key}");
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
        assert_eq!(
            check_config("fontFamily", "  Menlo ").unwrap(),
            KnobValue::Text("Menlo".to_string())
        );
        assert!(check_config("fontFamily", "   ")
            .unwrap_err()
            .contains("fontFamily: cannot be empty"));
        assert_eq!(
            check_config("fontSize", "18").unwrap(),
            KnobValue::Number(18.0)
        );
        assert!(check_config("fontSize", "2").is_err());
        assert_eq!(
            check_config("lineHeight", "1.25").unwrap(),
            KnobValue::Number(1.25)
        );
        assert!(check_config("lineHeight", "0.2").is_err());
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
        assert_eq!(
            keys("font"),
            vec![
                "fontFamily",
                "fontSize",
                "lineHeight",
                "lineThickness",
                "underlineRaise",
            ]
        );
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

    /// The knobs the native window leaves out (`NATIVE_HIDDEN_KNOBS`).
    const HIDDEN: &[&str] = &["renderLoop", "traceInput", "menuHint"];

    fn in_section(id: &str) -> Vec<&'static str> {
        KNOBS
            .iter()
            .filter(|(_, d)| d.section == id)
            .map(|(k, _)| *k)
            .collect()
    }

    #[test]
    fn the_sections_hold_every_knob_once_under_plain_names() {
        let titles: Vec<(&str, &str)> = SECTIONS.iter().map(|s| (s.id, s.title)).collect();
        assert_eq!(
            titles,
            vec![
                ("terminals", "Terminals"),
                ("text", "Text"),
                ("canvas", "Canvas & grid"),
                ("groups", "Groups & splits"),
                ("fullscreen", "Fullscreen"),
                ("selection", "Selection & copy"),
                ("input", "Keyboard & mouse"),
                ("agent", "Agent View"),
                ("status", "Notices"),
            ]
        );

        // Every knob, by where it is now. Nothing is lost by the regrouping:
        // the list is the 53 knobs of `WALL_CONFIG_KEYS`.
        let expected: &[(&str, &[&str])] = &[
            (
                "terminals",
                &[
                    "newTerminalSize",
                    "menuSizes",
                    "closeHoldMs",
                    "barButtonsMinPx",
                    "panelOpacity",
                    "cursorBlinkMs",
                ],
            ),
            (
                "text",
                &[
                    "fontFamily",
                    "fontSize",
                    "lineHeight",
                    "boldIsBright",
                    "dimAlpha",
                    "lineThickness",
                    "underlineRaise",
                    "curlyAmplitude",
                    "greekPx",
                ],
            ),
            (
                "canvas",
                &[
                    "background",
                    "gridShow",
                    "snap",
                    "gridMinor",
                    "gridMajorEvery",
                    "gridMinPx",
                    "gridLinePx",
                    "gridMinorColor",
                    "gridMajorColor",
                    "renderLoop",
                ],
            ),
            (
                "groups",
                &[
                    "groupLayout",
                    "groupColumns",
                    "groupGap",
                    "groupPad",
                    "splitGap",
                    "splitBarRows",
                    "splitBars",
                ],
            ),
            (
                "fullscreen",
                &["fullscreenMinLinePx", "fullscreenPad", "fullscreenTextStep"],
            ),
            (
                "selection",
                &[
                    "selectionColor",
                    "selectionForeground",
                    "selectionWordChars",
                    "clickRepeatMs",
                    "copyOnSelect",
                    "selectionClearOnTyping",
                ],
            ),
            (
                "input",
                &["optionAsAlt", "zoomKeys", "scrollMultiplier", "traceInput"],
            ),
            (
                "agent",
                &[
                    "agentViewSide",
                    "agentViewWidth",
                    "agentViewHeight",
                    "agentViewGap",
                ],
            ),
            (
                "status",
                &["noticeMs", "restartNoticeMs", "errorsShown", "menuHint"],
            ),
        ];
        let mut seen: Vec<&str> = Vec::new();
        for (id, keys) in expected {
            assert_eq!(in_section(id), keys.to_vec(), "section {id}");
            seen.extend(keys.iter());
        }
        // `KNOBS` is in the window's order: section by section.
        let in_order: Vec<&str> = KNOBS.iter().map(|(k, _)| *k).collect();
        assert_eq!(in_order, seen);
        let mut all = seen.clone();
        all.sort_unstable();
        let mut config: Vec<&str> = WALL_CONFIG_KEYS.to_vec();
        config.sort_unstable();
        assert_eq!(all, config);
        assert_eq!(all.len(), 53);
    }

    #[test]
    fn the_window_shows_one_section_or_a_search_over_all_of_them() {
        let keys = |v: &SettingsView<'_>| -> Vec<&'static str> {
            v.rows.iter().map(|r| KNOBS.iter().find(|(k, _)| *k == r.key).unwrap().0).collect()
        };
        let counts = |v: &SettingsView<'_>| -> Vec<(&'static str, usize)> {
            v.sections.iter().map(|s| (s.section.id, s.count)).collect()
        };
        let selected = |v: &SettingsView<'_>| -> Vec<&'static str> {
            v.sections
                .iter()
                .filter(|s| s.selected)
                .map(|s| s.section.id)
                .collect()
        };

        // Browsing: the first section until one is chosen.
        let v = settings_view("", None, HIDDEN);
        assert!(!v.searching);
        assert_eq!(v.shown, Some("terminals"));
        assert_eq!(keys(&v), in_section("terminals"));
        assert_eq!(selected(&v), vec!["terminals"]);
        assert_eq!(v.matched, 50);
        // Each section's count is its settings, less the ones not shown.
        assert_eq!(
            counts(&v),
            vec![
                ("terminals", 6),
                ("text", 9),
                ("canvas", 9),
                ("groups", 7),
                ("fullscreen", 3),
                ("selection", 6),
                ("input", 3),
                ("agent", 4),
                ("status", 3),
            ]
        );
        let starts: Vec<bool> = v.rows.iter().map(|r| r.starts_section).collect();
        assert_eq!(starts, vec![true, false, false, false, false, false]);

        let v = settings_view("", Some("text"), HIDDEN);
        assert_eq!(v.shown, Some("text"));
        assert_eq!(keys(&v), in_section("text"));
        assert_eq!(selected(&v), vec!["text"]);
        // A section that is not one (a name from before the regrouping)
        // shows the first.
        assert_eq!(settings_view("", Some("menu"), HIDDEN).shown, Some("terminals"));
        assert_eq!(settings_view("   ", Some("nope"), HIDDEN).shown, Some("terminals"));

        // Searching: every section's matches, in the list's order, each run
        // starting with its section; the list counts the matches in each.
        let v = settings_view("colour", None, HIDDEN);
        assert!(v.searching);
        assert_eq!(v.shown, None);
        assert_eq!(
            keys(&v),
            vec![
                "boldIsBright",
                "background",
                "gridMinorColor",
                "gridMajorColor",
                "selectionColor",
                "selectionForeground",
            ]
        );
        let tagged: Vec<(&str, bool)> = v
            .rows
            .iter()
            .map(|r| (r.section.title, r.starts_section))
            .collect();
        assert_eq!(
            tagged,
            vec![
                ("Text", true),
                ("Canvas & grid", true),
                ("Canvas & grid", false),
                ("Canvas & grid", false),
                ("Selection & copy", true),
                ("Selection & copy", false),
            ]
        );
        assert_eq!(v.matched, 6);
        assert!(selected(&v).is_empty());
        assert_eq!(
            counts(&v),
            vec![
                ("terminals", 0),
                ("text", 1),
                ("canvas", 3),
                ("groups", 0),
                ("fullscreen", 0),
                ("selection", 2),
                ("input", 0),
                ("agent", 0),
                ("status", 0),
            ]
        );

        // Narrowed to one section's matches; the counts stay the search's.
        let v = settings_view("colour", Some("canvas"), HIDDEN);
        assert_eq!(v.shown, Some("canvas"));
        assert_eq!(keys(&v), vec!["background", "gridMinorColor", "gridMajorColor"]);
        assert_eq!(selected(&v), vec!["canvas"]);
        assert_eq!(v.matched, 6);
        assert_eq!(counts(&v)[2], ("canvas", 3));
        // A section with no match does not narrow it to nothing.
        let v = settings_view("colour", Some("agent"), HIDDEN);
        assert_eq!(v.shown, None);
        assert_eq!(v.rows.len(), 6);

        // The search reads the one-line meaning too, and says when a match
        // is only in what the row does not always show.
        let v = settings_view("how fast", None, HIDDEN);
        assert_eq!(keys(&v), vec!["cursorBlinkMs", "scrollMultiplier"]);
        assert!(!v.rows[0].hits.summary.is_empty());
        assert!(!v.rows[0].hits.only_in_the_details());
        let v = settings_view("closehold", None, HIDDEN);
        assert_eq!(keys(&v), vec!["closeHoldMs"]);
        assert!(v.rows[0].hits.only_in_the_details());
        assert_eq!(v.rows[0].hits.key, vec![[0, 9]]);

        // A knob the window hides never shows; nothing matching is a list
        // of sections at 0 and no rows.
        for query in ["renderLoop", "zebra"] {
            let v = settings_view(query, None, HIDDEN);
            assert!(v.rows.is_empty(), "{query}");
            assert_eq!((v.matched, v.shown), (0, None), "{query}");
            assert_eq!(v.sections.len(), 9);
            assert!(v.sections.iter().all(|s| s.count == 0 && !s.selected));
        }
        // A section with nothing left to show is not listed.
        let agent = in_section("agent");
        let v = settings_view("", Some("agent"), &agent);
        assert!(v.sections.iter().all(|s| s.section.id != "agent"));
        assert_eq!(v.shown, Some("terminals"));
    }

    #[test]
    fn next_and_previous_section_walk_the_list_and_stop_at_its_ends() {
        let browse = |id| settings_view("", Some(id), HIDDEN);
        assert_eq!(step_section(&browse("terminals"), 1), Some("text"));
        assert_eq!(step_section(&browse("text"), -1), Some("terminals"));
        assert_eq!(step_section(&browse("terminals"), -1), Some("terminals"));
        assert_eq!(step_section(&browse("agent"), 1), Some("status"));
        assert_eq!(step_section(&browse("status"), 1), Some("status"));

        // Searching, the sections with no match are stepped over, and the
        // first step from all the results goes to the first with any.
        let all = settings_view("colour", None, HIDDEN);
        assert_eq!(step_section(&all, 1), Some("text"));
        assert_eq!(step_section(&all, -1), Some("text"));
        let found = |id| settings_view("colour", Some(id), HIDDEN);
        assert_eq!(step_section(&found("text"), 1), Some("canvas"));
        assert_eq!(step_section(&found("canvas"), 1), Some("selection"));
        assert_eq!(step_section(&found("selection"), 1), Some("selection"));
        assert_eq!(step_section(&found("canvas"), -1), Some("text"));
        assert_eq!(step_section(&settings_view("zebra", None, HIDDEN), 1), None);
    }
}
