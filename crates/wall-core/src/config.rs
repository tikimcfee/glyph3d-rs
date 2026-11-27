//! Port of `wall/wallConfig.js`: every configurable wall knob with its default
//! ([`WallConfig`]), the glyph shader line-style knob list ([`LINE_STYLE_KNOBS`]),
//! and [`check_config`], which validates a knob key and value against [`crate::settings::KNOBS`].

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::settings::{check_knob, knob_def, KnobInput, KnobValue, KNOBS};

/// Which side of a terminal an attached Agent View sits on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSide {
    #[default]
    Right,
    Left,
    Top,
    Bottom,
}

impl AgentSide {
    pub const ALL: &[AgentSide] = &[
        AgentSide::Right,
        AgentSide::Left,
        AgentSide::Top,
        AgentSide::Bottom,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AgentSide::Right => "right",
            AgentSide::Left => "left",
            AgentSide::Top => "top",
            AgentSide::Bottom => "bottom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "right" => Some(AgentSide::Right),
            "left" => Some(AgentSide::Left),
            "top" => Some(AgentSide::Top),
            "bottom" => Some(AgentSide::Bottom),
            _ => None,
        }
    }
}

impl fmt::Display for AgentSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a group arranges its member terminals on the canvas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupLayout {
    #[default]
    Tile,
    Freeform,
    Split,
}

impl GroupLayout {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroupLayout::Tile => "tile",
            GroupLayout::Freeform => "freeform",
            GroupLayout::Split => "split",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tile" => Some(GroupLayout::Tile),
            "freeform" => Some(GroupLayout::Freeform),
            "split" => Some(GroupLayout::Split),
            _ => None,
        }
    }
}

impl fmt::Display for GroupLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for GroupLayout {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        GroupLayout::parse(raw).ok_or_else(|| {
            format!("a group's layout is 'tile', 'freeform' or 'split', not '{raw}'")
        })
    }
}

/// Whether macOS Option acts as Alt when encoding terminal keys (`keyEncoding.OPTION_AS_ALT`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OptionAsAlt {
    #[default]
    Auto,
    True,
    False,
}

impl OptionAsAlt {
    pub const OPTIONS: &[&str] = &["auto", "true", "false"];

    pub const fn as_str(self) -> &'static str {
        match self {
            OptionAsAlt::Auto => "auto",
            OptionAsAlt::True => "true",
            OptionAsAlt::False => "false",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(OptionAsAlt::Auto),
            "true" => Ok(OptionAsAlt::True),
            "false" => Ok(OptionAsAlt::False),
            _ => Err(format!("optionAsAlt is auto, true, false, not '{s}'")),
        }
    }
}

impl fmt::Display for OptionAsAlt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Destination for text copied automatically when a terminal selection finishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyOnSelect {
    #[default]
    True,
    Clipboard,
    False,
}

impl CopyOnSelect {
    pub const fn as_str(self) -> &'static str {
        match self {
            CopyOnSelect::True => "true",
            CopyOnSelect::Clipboard => "clipboard",
            CopyOnSelect::False => "false",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "true" => Some(CopyOnSelect::True),
            "clipboard" => Some(CopyOnSelect::Clipboard),
            "false" => Some(CopyOnSelect::False),
            _ => None,
        }
    }
}

impl fmt::Display for CopyOnSelect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// When the canvas renders frames (`demand` vs `always`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderLoop {
    #[default]
    Demand,
    Always,
}

impl RenderLoop {
    pub const fn as_str(self) -> &'static str {
        match self {
            RenderLoop::Demand => "demand",
            RenderLoop::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "demand" => Some(RenderLoop::Demand),
            "always" => Some(RenderLoop::Always),
            _ => None,
        }
    }
}

impl fmt::Display for RenderLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Font family identifier from the font chain (`fonts/families.js`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FontFamily {
    #[serde(rename = "cousine")]
    Cousine,
    #[default]
    #[serde(rename = "monaspace-neon")]
    MonaspaceNeon,
    #[serde(rename = "monaspace-argon")]
    MonaspaceArgon,
}

impl FontFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            FontFamily::Cousine => "cousine",
            FontFamily::MonaspaceNeon => "monaspace-neon",
            FontFamily::MonaspaceArgon => "monaspace-argon",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cousine" => Some(FontFamily::Cousine),
            "monaspace-neon" => Some(FontFamily::MonaspaceNeon),
            "monaspace-argon" => Some(FontFamily::MonaspaceArgon),
            _ => None,
        }
    }
}

impl fmt::Display for FontFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default selection fill colour (`TERMINAL_SELECTION_DEFAULTS.background`).
pub const SELECTION_COLOR_DEFAULT: &str = "#cccccc";
/// Default selection foreground colour (`TERMINAL_SELECTION_DEFAULTS.foreground`).
pub const SELECTION_FOREGROUND_DEFAULT: &str = "#0a0a1e";
/// Default word-boundary characters besides whitespace (`TERMINAL_SELECTION_DEFAULTS.wordChars`).
pub const SELECTION_WORD_CHARS_DEFAULT: &str = "'\"│`|:;,()[]{}<>$";
/// Default `fontFamily` (`"monaspace-neon"`).
pub const FONT_FAMILY_DEFAULT: &str = "monaspace-neon";
/// Default `fontSize` in points (`14.0`).
pub const FONT_SIZE_DEFAULT: f64 = 14.0;
/// Default `lineHeight` multiplier (`1.0`).
pub const LINE_HEIGHT_DEFAULT: f64 = 1.0;
/// Default `boldIsBright` value (`BOLD_IS_BRIGHT_DEFAULT`).
pub const BOLD_IS_BRIGHT_DEFAULT: bool = false;
/// Default `lineThickness` (`GLYPH_LINE_DEFAULTS.lineThickness`).
pub const LINE_THICKNESS_DEFAULT: f64 = 1.0;
/// Default `underlineRaise` (`GLYPH_LINE_DEFAULTS.underlineRaise`).
pub const UNDERLINE_RAISE_DEFAULT: f64 = 0.0;
/// Default `curlyAmplitude` (`GLYPH_LINE_DEFAULTS.curlyAmplitude`).
pub const CURLY_AMPLITUDE_DEFAULT: f64 = 1.5;
/// Default `dimAlpha` (`GLYPH_LINE_DEFAULTS.dimAlpha`).
pub const DIM_ALPHA_DEFAULT: f64 = 0.5;
/// Default `panelOpacity` (`config/defaults.toml` `[wall] panel_opacity`).
pub const PANEL_OPACITY_DEFAULT: f64 = 1.0;
/// Default `cursorBlinkMs` (`config/defaults.toml` `[wall] cursor_blink_ms`).
pub const CURSOR_BLINK_MS_DEFAULT: f64 = 530.0;
/// Default `greekPx` (`config/defaults.toml` `[wall] greek_px`).
pub const GREEK_PX_DEFAULT: f64 = 8.0;

/// Knob keys that drive the glyph shader's cell line style (`wallConfig.LINE_STYLE_KNOBS`).
pub const LINE_STYLE_KNOBS: &[&str] = &[
    "lineThickness",
    "underlineRaise",
    "curlyAmplitude",
    "dimAlpha",
];

/// Every knob key of [`WallConfig`], in `wallConfig.WALL_CONFIG`'s declaration order.
pub const WALL_CONFIG_KEYS: &[&str] = &[
    "closeHoldMs",
    "barButtonsMinPx",
    "panelOpacity",
    "cursorBlinkMs",
    "agentViewSide",
    "agentViewWidth",
    "agentViewHeight",
    "agentViewGap",
    "groupLayout",
    "groupColumns",
    "groupGap",
    "groupPad",
    "splitGap",
    "splitBarRows",
    "splitBars",
    "restartNoticeMs",
    "errorsShown",
    "optionAsAlt",
    "traceInput",
    "zoomKeys",
    "scrollMultiplier",
    "fullscreenMinLinePx",
    "fullscreenPad",
    "fullscreenTextStep",
    "selectionColor",
    "selectionForeground",
    "selectionWordChars",
    "copyOnSelect",
    "clickRepeatMs",
    "selectionClearOnTyping",
    "noticeMs",
    "background",
    "renderLoop",
    "gridShow",
    "gridMinor",
    "gridMajorEvery",
    "gridMinPx",
    "gridLinePx",
    "gridMinorColor",
    "gridMajorColor",
    "snap",
    "menuHint",
    "menuSizes",
    "fontFamily",
    "fontSize",
    "lineHeight",
    "boldIsBright",
    "lineThickness",
    "underlineRaise",
    "curlyAmplitude",
    "dimAlpha",
    "greekPx",
];

/// All configurable knobs of the wall (`wallConfig.WALL_CONFIG`), initialized to their defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WallConfig {
    /// Hold duration in ms before the close button or close chord kills a terminal.
    pub close_hold_ms: f64,
    /// Minimum title-bar height in CSS px before its button cluster is shown.
    pub bar_buttons_min_px: f64,
    /// Opacity of each terminal's background panel (`0..=1`, default `1.0`).
    pub panel_opacity: f64,
    /// Half-period of a blinking cursor in milliseconds (`0` = steady, default `530`).
    pub cursor_blink_ms: f64,
    /// Default side of a terminal a newly opened Agent View attaches to.
    pub agent_view_side: AgentSide,
    /// World width of an Agent View on the left or right of a terminal.
    pub agent_view_width: f64,
    /// World height of an Agent View above or below a terminal.
    pub agent_view_height: f64,
    /// World gap between a terminal and its attached Agent View.
    pub agent_view_gap: f64,
    /// Layout mode a newly created group starts with.
    pub group_layout: GroupLayout,
    /// Column count for a new tiled group (`0` picks `ceil(sqrt(len))`).
    pub group_columns: f64,
    /// World gap between members of a new tiled group.
    pub group_gap: f64,
    /// World margin a group's backdrop extends past its members' windows.
    pub group_pad: f64,
    /// World gap between panes of a split group.
    pub split_gap: f64,
    /// Room above each split pane reserved for its title bar, in terminal rows.
    pub split_bar_rows: f64,
    /// Whether panes of a newly created split group show title bars.
    pub split_bars: bool,
    /// Delay in ms before a termd brain restart displays its amber notice.
    pub restart_notice_ms: f64,
    /// Number of recent termd errors shown in the status window.
    pub errors_shown: f64,
    /// How macOS Option is treated relative to Alt for terminal key events.
    pub option_as_alt: OptionAsAlt,
    /// Whether wheel, key, and pointer presses are logged as `[input]` trace lines.
    pub trace_input: bool,
    /// Space-separated modifier names (`ctrl`, `alt`, `meta`, `shift`) that make wheel events zoom.
    pub zoom_keys: String,
    /// Multiplier for wheel/trackpad scroll speed inside a focused terminal.
    pub scroll_multiplier: f64,
    /// Minimum row height in CSS px when entering fullscreen from a zoomed-out canvas.
    pub fullscreen_min_line_px: f64,
    /// Padding in CSS px kept inside the canvas edge around a fullscreen terminal.
    pub fullscreen_pad: f64,
    /// Factor by which fullscreen text size steps up or down per chord press.
    pub fullscreen_text_step: f64,
    /// Fill colour (`#rrggbb`) behind selected cells.
    pub selection_color: String,
    /// Text colour (`#rrggbb` or `"keep"`) of selected cells.
    pub selection_foreground: String,
    /// Characters besides whitespace that bound a word on double-click.
    pub selection_word_chars: String,
    /// Which clipboards receive a finished selection automatically.
    pub copy_on_select: CopyOnSelect,
    /// Maximum interval in ms between clicks counted as double/triple click.
    pub click_repeat_ms: f64,
    /// Whether typing into a terminal clears its active text selection.
    pub selection_clear_on_typing: bool,
    /// Duration in ms that a wall notice stays visible.
    pub notice_ms: f64,
    /// Canvas background colour (`#rrggbb`).
    pub background: String,
    /// Frame scheduling mode (`demand` vs `always`).
    pub render_loop: RenderLoop,
    /// Whether the drafting-paper grid is drawn on the canvas.
    pub grid_show: bool,
    /// World spacing between minor grid lines (also the base snap step).
    pub grid_minor: f64,
    /// Number of minor intervals per major grid line.
    pub grid_major_every: f64,
    /// Minimum screen spacing in CSS px below which a grid level is hidden.
    pub grid_min_px: f64,
    /// Grid line thickness on screen in CSS px.
    pub grid_line_px: f64,
    /// Minor grid line colour (`#rrggbb`).
    pub grid_minor_color: String,
    /// Major grid line colour (`#rrggbb`).
    pub grid_major_color: String,
    /// Whether moves and resizes snap to the grid by default.
    pub snap: bool,
    /// Whether the right-click menu shows the second-right-click native-menu hint.
    pub menu_hint: bool,
    /// Space-separated `COLSxROWS` size presets offered in a terminal's right-click menu.
    pub menu_sizes: String,
    /// Active font family drawn across terminals (bundled id or system family name).
    pub font_family: String,
    /// Base font size in points (`14.0` default).
    pub font_size: f64,
    /// Vertical cell pitch multiplier (`1.0` default).
    pub line_height: f64,
    /// Whether bold text in standard SGR colours 30–37 shifts to bright 90–97.
    pub bold_is_bright: bool,
    /// Thickness multiplier for underline, strikethrough, and overline relative to the font.
    pub line_thickness: f64,
    /// Vertical offset added to underline position, in line heights.
    pub underline_raise: f64,
    /// Wave height of curly underlines, as a multiple of line thickness.
    pub curly_amplitude: f64,
    /// Opacity multiplier retained by dim (`SGR 2`) text (`0..=1`).
    pub dim_alpha: f64,
    /// Cell row height in CSS px below which glyphs simplify to horizontal bars (`0` = off).
    pub greek_px: f64,
}

impl Default for WallConfig {
    fn default() -> Self {
        WallConfig {
            close_hold_ms: 650.0,
            bar_buttons_min_px: 14.0,
            panel_opacity: PANEL_OPACITY_DEFAULT,
            cursor_blink_ms: CURSOR_BLINK_MS_DEFAULT,
            agent_view_side: AgentSide::Right,
            agent_view_width: 76.0,
            agent_view_height: 44.0,
            agent_view_gap: 1.5,
            group_layout: GroupLayout::Tile,
            group_columns: 0.0,
            group_gap: 2.0,
            group_pad: 3.0,
            split_gap: 0.5,
            split_bar_rows: 1.25,
            split_bars: true,
            restart_notice_ms: 400.0,
            errors_shown: 3.0,
            option_as_alt: OptionAsAlt::Auto,
            trace_input: false,
            zoom_keys: "ctrl alt meta".to_string(),
            scroll_multiplier: 1.0,
            fullscreen_min_line_px: 14.0,
            fullscreen_pad: 6.0,
            fullscreen_text_step: 1.1,
            selection_color: SELECTION_COLOR_DEFAULT.to_string(),
            selection_foreground: SELECTION_FOREGROUND_DEFAULT.to_string(),
            selection_word_chars: SELECTION_WORD_CHARS_DEFAULT.to_string(),
            copy_on_select: CopyOnSelect::True,
            click_repeat_ms: 500.0,
            selection_clear_on_typing: true,
            notice_ms: 5000.0,
            background: "#07080b".to_string(),
            render_loop: RenderLoop::Demand,
            grid_show: true,
            grid_minor: 4.0,
            grid_major_every: 5.0,
            grid_min_px: 6.0,
            grid_line_px: 1.0,
            grid_minor_color: "#0f1219".to_string(),
            grid_major_color: "#181c26".to_string(),
            snap: true,
            menu_hint: true,
            menu_sizes: "80x24 120x36 160x48".to_string(),
            font_family: FONT_FAMILY_DEFAULT.to_string(),
            font_size: FONT_SIZE_DEFAULT,
            line_height: LINE_HEIGHT_DEFAULT,
            bold_is_bright: BOLD_IS_BRIGHT_DEFAULT,
            line_thickness: LINE_THICKNESS_DEFAULT,
            underline_raise: UNDERLINE_RAISE_DEFAULT,
            curly_amplitude: CURLY_AMPLITUDE_DEFAULT,
            dim_alpha: DIM_ALPHA_DEFAULT,
            greek_px: GREEK_PX_DEFAULT,
        }
    }
}

impl WallConfig {
    /// Read a knob's current value by its camelCase JS key name.
    pub fn get(&self, key: &str) -> Option<KnobValue> {
        Some(match key {
            "closeHoldMs" => KnobValue::Number(self.close_hold_ms),
            "barButtonsMinPx" => KnobValue::Number(self.bar_buttons_min_px),
            "panelOpacity" => KnobValue::Number(self.panel_opacity),
            "cursorBlinkMs" => KnobValue::Number(self.cursor_blink_ms),
            "agentViewSide" => KnobValue::Text(self.agent_view_side.as_str().to_string()),
            "agentViewWidth" => KnobValue::Number(self.agent_view_width),
            "agentViewHeight" => KnobValue::Number(self.agent_view_height),
            "agentViewGap" => KnobValue::Number(self.agent_view_gap),
            "groupLayout" => KnobValue::Text(self.group_layout.as_str().to_string()),
            "groupColumns" => KnobValue::Number(self.group_columns),
            "groupGap" => KnobValue::Number(self.group_gap),
            "groupPad" => KnobValue::Number(self.group_pad),
            "splitGap" => KnobValue::Number(self.split_gap),
            "splitBarRows" => KnobValue::Number(self.split_bar_rows),
            "splitBars" => KnobValue::Bool(self.split_bars),
            "restartNoticeMs" => KnobValue::Number(self.restart_notice_ms),
            "errorsShown" => KnobValue::Number(self.errors_shown),
            "optionAsAlt" => KnobValue::Text(self.option_as_alt.as_str().to_string()),
            "traceInput" => KnobValue::Bool(self.trace_input),
            "zoomKeys" => KnobValue::Text(self.zoom_keys.clone()),
            "scrollMultiplier" => KnobValue::Number(self.scroll_multiplier),
            "fullscreenMinLinePx" => KnobValue::Number(self.fullscreen_min_line_px),
            "fullscreenPad" => KnobValue::Number(self.fullscreen_pad),
            "fullscreenTextStep" => KnobValue::Number(self.fullscreen_text_step),
            "selectionColor" => KnobValue::Text(self.selection_color.clone()),
            "selectionForeground" => KnobValue::Text(self.selection_foreground.clone()),
            "selectionWordChars" => KnobValue::Text(self.selection_word_chars.clone()),
            "copyOnSelect" => KnobValue::Text(self.copy_on_select.as_str().to_string()),
            "clickRepeatMs" => KnobValue::Number(self.click_repeat_ms),
            "selectionClearOnTyping" => KnobValue::Bool(self.selection_clear_on_typing),
            "noticeMs" => KnobValue::Number(self.notice_ms),
            "background" => KnobValue::Text(self.background.clone()),
            "renderLoop" => KnobValue::Text(self.render_loop.as_str().to_string()),
            "gridShow" => KnobValue::Bool(self.grid_show),
            "gridMinor" => KnobValue::Number(self.grid_minor),
            "gridMajorEvery" => KnobValue::Number(self.grid_major_every),
            "gridMinPx" => KnobValue::Number(self.grid_min_px),
            "gridLinePx" => KnobValue::Number(self.grid_line_px),
            "gridMinorColor" => KnobValue::Text(self.grid_minor_color.clone()),
            "gridMajorColor" => KnobValue::Text(self.grid_major_color.clone()),
            "snap" => KnobValue::Bool(self.snap),
            "menuHint" => KnobValue::Bool(self.menu_hint),
            "menuSizes" => KnobValue::Text(self.menu_sizes.clone()),
            "fontFamily" => KnobValue::Text(self.font_family.clone()),
            "fontSize" => KnobValue::Number(self.font_size),
            "lineHeight" => KnobValue::Number(self.line_height),
            "boldIsBright" => KnobValue::Bool(self.bold_is_bright),
            "lineThickness" => KnobValue::Number(self.line_thickness),
            "underlineRaise" => KnobValue::Number(self.underline_raise),
            "curlyAmplitude" => KnobValue::Number(self.curly_amplitude),
            "dimAlpha" => KnobValue::Number(self.dim_alpha),
            "greekPx" => KnobValue::Number(self.greek_px),
            _ => return None,
        })
    }

    /// Validate `value` via [`check_config`] and update `key` on `self`, returning the coerced [`KnobValue`].
    pub fn set(&mut self, key: &str, value: impl Into<KnobInput>) -> Result<KnobValue, String> {
        let checked = check_config(key, value)?;
        self.apply_checked(key, &checked);
        Ok(checked)
    }

    /// Reset one knob on `self` back to `born`'s value (`WallConfig::default()` if none was customized),
    /// returning the restored [`KnobValue`].
    pub fn reset_from(&mut self, key: &str, born: &WallConfig) -> Result<KnobValue, String> {
        let Some(val) = born.get(key) else {
            return Err(format!(
                "unknown wall knob '{key}' (knobs: {})",
                WALL_CONFIG_KEYS.join(", ")
            ));
        };
        self.apply_checked(key, &val);
        Ok(val)
    }

    /// Apply persisted dialled entries (e.g. from [`crate::store::WallStore::config`]), returning the
    /// keys and reasons of any invalid entries so the caller can log and drop them (`TerminalWall._dialled`).
    pub fn apply_dialled(
        &mut self,
        stored: &BTreeMap<String, serde_json::Value>,
    ) -> Vec<(String, String)> {
        let mut rejected = Vec::new();
        for (key, raw) in stored {
            if let Err(err) = self.set(key, raw) {
                rejected.push((key.clone(), err));
            }
        }
        rejected
    }

    fn apply_checked(&mut self, key: &str, checked: &KnobValue) {
        fn num(v: &KnobValue) -> f64 {
            v.as_f64().expect("checked number knob holds f64")
        }
        fn flag(v: &KnobValue) -> bool {
            v.as_bool().expect("checked bool knob holds bool")
        }
        fn text(v: &KnobValue) -> &str {
            v.as_str().expect("checked text/enum/color knob holds str")
        }

        match key {
            "closeHoldMs" => self.close_hold_ms = num(checked),
            "barButtonsMinPx" => self.bar_buttons_min_px = num(checked),
            "panelOpacity" => self.panel_opacity = num(checked),
            "cursorBlinkMs" => self.cursor_blink_ms = num(checked),
            "agentViewSide" => {
                self.agent_view_side =
                    AgentSide::parse(text(checked)).expect("checked agentViewSide option")
            }
            "agentViewWidth" => self.agent_view_width = num(checked),
            "agentViewHeight" => self.agent_view_height = num(checked),
            "agentViewGap" => self.agent_view_gap = num(checked),
            "groupLayout" => {
                self.group_layout =
                    GroupLayout::parse(text(checked)).expect("checked groupLayout option")
            }
            "groupColumns" => self.group_columns = num(checked),
            "groupGap" => self.group_gap = num(checked),
            "groupPad" => self.group_pad = num(checked),
            "splitGap" => self.split_gap = num(checked),
            "splitBarRows" => self.split_bar_rows = num(checked),
            "splitBars" => self.split_bars = flag(checked),
            "restartNoticeMs" => self.restart_notice_ms = num(checked),
            "errorsShown" => self.errors_shown = num(checked),
            "optionAsAlt" => {
                self.option_as_alt =
                    OptionAsAlt::parse(text(checked)).expect("checked optionAsAlt option")
            }
            "traceInput" => self.trace_input = flag(checked),
            "zoomKeys" => self.zoom_keys = text(checked).to_string(),
            "scrollMultiplier" => self.scroll_multiplier = num(checked),
            "fullscreenMinLinePx" => self.fullscreen_min_line_px = num(checked),
            "fullscreenPad" => self.fullscreen_pad = num(checked),
            "fullscreenTextStep" => self.fullscreen_text_step = num(checked),
            "selectionColor" => self.selection_color = text(checked).to_string(),
            "selectionForeground" => self.selection_foreground = text(checked).to_string(),
            "selectionWordChars" => self.selection_word_chars = text(checked).to_string(),
            "copyOnSelect" => {
                self.copy_on_select =
                    CopyOnSelect::parse(text(checked)).expect("checked copyOnSelect option")
            }
            "clickRepeatMs" => self.click_repeat_ms = num(checked),
            "selectionClearOnTyping" => self.selection_clear_on_typing = flag(checked),
            "noticeMs" => self.notice_ms = num(checked),
            "background" => self.background = text(checked).to_string(),
            "renderLoop" => {
                self.render_loop =
                    RenderLoop::parse(text(checked)).expect("checked renderLoop option")
            }
            "gridShow" => self.grid_show = flag(checked),
            "gridMinor" => self.grid_minor = num(checked),
            "gridMajorEvery" => self.grid_major_every = num(checked),
            "gridMinPx" => self.grid_min_px = num(checked),
            "gridLinePx" => self.grid_line_px = num(checked),
            "gridMinorColor" => self.grid_minor_color = text(checked).to_string(),
            "gridMajorColor" => self.grid_major_color = text(checked).to_string(),
            "snap" => self.snap = flag(checked),
            "menuHint" => self.menu_hint = flag(checked),
            "menuSizes" => self.menu_sizes = text(checked).to_string(),
            "fontFamily" => self.font_family = text(checked).to_string(),
            "fontSize" => self.font_size = num(checked),
            "lineHeight" => self.line_height = num(checked),
            "boldIsBright" => self.bold_is_bright = flag(checked),
            "lineThickness" => self.line_thickness = num(checked),
            "underlineRaise" => self.underline_raise = num(checked),
            "curlyAmplitude" => self.curly_amplitude = num(checked),
            "dimAlpha" => self.dim_alpha = num(checked),
            "greekPx" => self.greek_px = num(checked),
            other => panic!("check_config accepted unknown wall knob '{other}'"),
        }
    }
}

/// Default [`KnobValue`] of `key` in [`WallConfig::default`], or `None` if `key` is unknown.
pub fn default_knob_value(key: &str) -> Option<KnobValue> {
    WallConfig::default().get(key)
}

/// Assert the load-time bijection checked by `wallConfig.js`: every key in [`WALL_CONFIG_KEYS`]
/// has a definition in [`KNOBS`], and every definition in [`KNOBS`] exists in [`WallConfig`].
pub fn assert_config_matches_knobs() {
    let defaults = WallConfig::default();
    for &key in WALL_CONFIG_KEYS {
        assert!(
            knob_def(key).is_some(),
            "[wall] knob '{key}' has no definition in settings::KNOBS"
        );
        assert!(
            defaults.get(key).is_some(),
            "[wall] WALL_CONFIG_KEYS lists '{key}', which WallConfig::get lacks"
        );
    }
    for &(key, _) in KNOBS {
        assert!(
            WALL_CONFIG_KEYS.contains(&key),
            "[wall] settings::KNOBS defines '{key}', which WALL_CONFIG lacks"
        );
    }
}

/// Validate one wall knob's value (`wallConfig.checkConfig`), coercing strings sent by CLI verbs.
///
/// Returns `Err` naming the knob when `key` is unknown or `value` fails its [`KNOBS`] definition.
pub fn check_config(key: &str, value: impl Into<KnobInput>) -> Result<KnobValue, String> {
    let Some(def) = knob_def(key) else {
        return Err(format!(
            "unknown wall knob '{key}' (knobs: {})",
            WALL_CONFIG_KEYS.join(", ")
        ));
    };
    check_knob(key, def, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_config_and_knobs_bijection_and_line_style_knobs() {
        assert_config_matches_knobs();
        assert_eq!(WALL_CONFIG_KEYS.len(), 52);
        assert_eq!(KNOBS.len(), 52);
        for &k in LINE_STYLE_KNOBS {
            assert!(WALL_CONFIG_KEYS.contains(&k), "missing line style knob {k}");
        }
    }

    #[test]
    fn wall_config_set_reset_and_apply_dialled() {
        let born = WallConfig::default();
        let mut cfg = born.clone();
        assert_eq!(
            cfg.set("background", "#203040").unwrap(),
            KnobValue::Text("#203040".to_string())
        );
        assert_eq!(cfg.background, "#203040");
        assert_eq!(cfg.set("snap", "off").unwrap(), KnobValue::Bool(false));
        assert!(!cfg.snap);
        assert!(cfg.set("gridMinor", "-2").is_err());
        assert_eq!(cfg.grid_minor, born.grid_minor);

        assert_eq!(
            cfg.reset_from("background", &born).unwrap(),
            KnobValue::Text(born.background.clone())
        );
        assert_eq!(cfg.background, born.background);

        let mut stored = BTreeMap::new();
        stored.insert("gridMinor".to_string(), serde_json::json!(0));
        stored.insert("snap".to_string(), serde_json::json!(false));
        stored.insert("gone".to_string(), serde_json::json!(1));
        let mut reloaded = born.clone();
        let rejected = reloaded.apply_dialled(&stored);
        assert_eq!(reloaded.grid_minor, born.grid_minor);
        assert!(!reloaded.snap);
        let rejected_keys: Vec<&str> = rejected.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(rejected_keys, vec!["gone", "gridMinor"]);
    }
}
