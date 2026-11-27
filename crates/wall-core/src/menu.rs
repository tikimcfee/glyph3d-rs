//! Port of `wall/contextMenu.js`: decides who owns a right-click or context-menu
//! event (`context_policy`), what counts as a secondary press (`is_secondary_press`),
//! custom and configured terminal size parsing (`MAX_CELLS`, `check_size`,
//! `parse_sizes`), which verb-backed rows appear on a terminal, group header or
//! canvas menu (`menu_items`), and where the menu box sits inside the viewport
//! (`place_menu`).
//!
//! Every menu row dispatches a wall verb; the menu holds no imperative
//! behaviour of its own. The event policy preserves native context menus on
//! editable fields, DOM/UI chrome, Shift+right-click, and a second right-click
//! while the wall's own menu is open, while handing right-clicks on the focused
//! mouse-reporting terminal's body to that program unless Shift bypasses capture.

use crate::chord::{chord_label, copy_label, paste_label};
use crate::geom::Point;
use crate::input::{Button, Mods, Platform};

/// Who a context-menu event belongs to (`Verdict` in `wall/contextMenu.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Open the wall's own context menu.
    Ours,
    /// Leave the event to the native/OS/UI-chrome menu (and close ours if open).
    Native,
    /// Suppress menus because the focused terminal's mouse-reporting program owns the click.
    Program,
}

/// Which part of a terminal window was targeted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalPart {
    Body,
    Title,
    Edge,
}

/// What sits under the pointer as the menu reads it (`MenuTarget` in `wall/contextMenu.js`).
#[derive(Clone, Debug, PartialEq)]
pub enum MenuTarget {
    Terminal { id: String, part: TerminalPart },
    Group { id: String },
    Canvas { world: Option<Point> },
}

/// Event facts inspected by [`context_policy`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextEvent {
    pub shift_key: bool,
    /// Target is a text input or editable field.
    pub editable: bool,
    /// Target is UI chrome outside the wall's canvas, map labels, and bar buttons.
    pub chrome: bool,
    /// Whether the wall's own menu is already open.
    pub menu_open: bool,
    /// Raised by the keyboard ContextMenu / Shift+F10 shortcut.
    pub keyboard: bool,
    pub target: Option<MenuTarget>,
    /// Whether the targeted terminal's program enabled DECSET mouse reporting.
    pub mouse_reporting: bool,
    /// Whether the targeted terminal is currently focused (`focusedId`).
    pub focused: bool,
}

/// Outcome of [`context_policy`]: who gets the event and a short diagnostic reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextDecision {
    pub verdict: Verdict,
    pub why: &'static str,
}

/// Decide who owns a context-menu event (`contextPolicy` in `wall/contextMenu.js`).
pub fn context_policy(event: &ContextEvent) -> ContextDecision {
    if event.keyboard {
        return ContextDecision {
            verdict: Verdict::Ours,
            why: "the menu key opened ours",
        };
    }
    if event.editable {
        return ContextDecision {
            verdict: Verdict::Native,
            why: "a text field keeps its own menu",
        };
    }
    if event.menu_open {
        return ContextDecision {
            verdict: Verdict::Native,
            why: "right-click again: the browser menu",
        };
    }
    let Some(target) = &event.target else {
        return ContextDecision {
            verdict: Verdict::Native,
            why: "DOM chrome keeps its own menu",
        };
    };
    if event.chrome {
        return ContextDecision {
            verdict: Verdict::Native,
            why: "DOM chrome keeps its own menu",
        };
    }

    let reported = matches!(
        target,
        MenuTarget::Terminal {
            part: TerminalPart::Body,
            ..
        }
    ) && event.mouse_reporting
        && event.focused;
    if reported {
        return if event.shift_key {
            ContextDecision {
                verdict: Verdict::Ours,
                why: "Shift bypasses the program's mouse capture",
            }
        } else {
            ContextDecision {
                verdict: Verdict::Program,
                why: "the program reports the mouse",
            }
        };
    }

    if event.shift_key {
        return ContextDecision {
            verdict: Verdict::Native,
            why: "Shift+right-click is the browser menu",
        };
    }

    let why = match target {
        MenuTarget::Terminal { .. } => "over terminal",
        MenuTarget::Group { .. } => "over group",
        MenuTarget::Canvas { .. } => "over canvas",
    };
    ContextDecision {
        verdict: Verdict::Ours,
        why,
    }
}

/// Whether a pointer press is a secondary click: right button anywhere, or
/// Ctrl+left-click on macOS (`isSecondaryPress` in `wall/contextMenu.js`).
///
/// A secondary press does not dismiss an already-open wall menu on `pointerdown`
/// so the `contextmenu` event that follows can act as "right-click again".
pub fn is_secondary_press(button: Button, mods: Mods, platform: Platform) -> bool {
    button == Button::Right
        || (platform == Platform::Mac && button == Button::Left && mods.ctrl)
}

/// Maximum columns or rows a terminal can have (`termd` carries dimensions as `u16`).
pub const MAX_CELLS: u32 = 65535;

/// Terminal dimensions in character cells (`cols` × `rows`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellSize {
    pub cols: u32,
    pub rows: u32,
}

impl CellSize {
    pub const fn new(cols: u32, rows: u32) -> Self {
        Self { cols, rows }
    }
}

/// Validate a custom terminal size typed into the menu's `cols` × `rows` fields:
/// two whole numbers in `1..=MAX_CELLS`, returning a short human-facing refusal
/// reason otherwise (`checkSize` in `wall/contextMenu.js`).
pub fn check_size(cols: &str, rows: &str) -> Result<CellSize, String> {
    fn check_one(raw: &str, what: &str) -> Result<u32, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("{what}: a whole number, please"));
        }
        if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("{what}: '{trimmed}' isn't a whole number"));
        }
        if trimmed.bytes().all(|b| b == b'0') {
            return Err(format!("{what}: at least 1"));
        }
        match trimmed.parse::<u32>() {
            Ok(value) if value <= MAX_CELLS => Ok(value),
            _ => Err(format!("{what}: at most {MAX_CELLS} (termd's limit)")),
        }
    }

    Ok(CellSize {
        cols: check_one(cols, "cols")?,
        rows: check_one(rows, "rows")?,
    })
}

/// Parse one terminal size written `COLSxROWS` (or `COLS×ROWS`), each side a
/// whole number in `1..=MAX_CELLS`: the `newTerminalSize` knob's form
/// (2026-11-27). The refusal names what was wrong with it.
pub fn parse_size(input: &str) -> Result<CellSize, String> {
    let trimmed = input.trim();
    let Some((cols, rows)) = trimmed.split_once(['x', 'X', '×']) else {
        return Err(format!(
            "a size is COLSxROWS (like \"100x30\"), not '{trimmed}'"
        ));
    };
    check_size(cols, rows)
}

/// Parse the `menuSizes` knob: space-separated `COLSxROWS` (or `COLS×ROWS`)
/// entries (`parseSizes` in `wall/contextMenu.js`).
pub fn parse_sizes(input: &str) -> Result<Vec<CellSize>, String> {
    let mut out = Vec::new();
    for tok in input.split_whitespace() {
        let parsed = tok
            .split_once(['x', '×'])
            .and_then(|(c_str, r_str)| {
                if c_str.is_empty()
                    || r_str.is_empty()
                    || !c_str.bytes().all(|b| b.is_ascii_digit())
                    || !r_str.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let cols = c_str.parse::<u32>().ok()?;
                let rows = r_str.parse::<u32>().ok()?;
                if cols < 1 || rows < 1 {
                    return None;
                }
                Some(CellSize { cols, rows })
            });
        match parsed {
            Some(size) => out.push(size),
            None => {
                return Err(format!(
                    "sizes are COLSxROWS, space-separated (like \"80x24 120x36\"), not '{tok}'"
                ));
            }
        }
    }
    Ok(out)
}

/// Values accepted by `wall.config optionAsAlt`, unified with [`crate::config::OptionAsAlt`].
pub use crate::config::OptionAsAlt;

const OPTION_AS_ALT_CHOICES: [(OptionAsAlt, &str); 3] = [
    (OptionAsAlt::Auto, "auto"),
    (OptionAsAlt::True, "on"),
    (OptionAsAlt::False, "off"),
];

/// Side of a terminal an Agent View attaches to (unified with [`crate::config::AgentSide`]).
pub use crate::config::AgentSide;

/// Offered Agent View sides in menu order.
pub const AGENT_SIDES: [AgentSide; 4] = [
    AgentSide::Right,
    AgentSide::Left,
    AgentSide::Top,
    AgentSide::Bottom,
];

/// Group arrangement mode (unified with [`crate::config::GroupLayout`]).
pub use crate::config::GroupLayout;

/// A terminal's attached Agent View state as exposed in `snapshot()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalAgentView {
    pub open: bool,
    pub side: AgentSide,
}

/// Per-terminal snapshot fields read by [`menu_items`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuTerminal {
    pub id: String,
    pub name: String,
    pub named: bool,
    pub cols: u32,
    pub rows: u32,
    pub hidden: bool,
    pub group: Option<String>,
    /// Whether the terminal currently has a text selection (`copy` row).
    pub selected: bool,
    pub agent_view: Option<TerminalAgentView>,
}

/// Per-group snapshot fields read by [`menu_items`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuGroup {
    pub id: String,
    pub name: String,
    pub named: bool,
    pub layout: GroupLayout,
    pub columns: u32,
    pub bars: bool,
    pub members: Vec<String>,
}

/// Configuration knobs read when building context menus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuConfig {
    pub option_as_alt: OptionAsAlt,
    pub menu_sizes: String,
    pub snap: bool,
    pub grid_show: bool,
}

impl Default for MenuConfig {
    fn default() -> Self {
        Self {
            option_as_alt: OptionAsAlt::Auto,
            menu_sizes: "80x24 120x36 160x48".to_string(),
            snap: true,
            grid_show: true,
        }
    }
}

/// Wall snapshot state read by [`menu_items`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuState {
    pub focused_id: Option<String>,
    pub selection: Vec<String>,
    /// Currently fullscreen terminal id or group id, if any.
    pub fullscreen: Option<String>,
    pub sidebar_open: bool,
    pub status_window_open: bool,
    pub settings_panel_open: bool,
    pub config: MenuConfig,
    pub terminals: Vec<MenuTerminal>,
    pub groups: Vec<MenuGroup>,
}

/// One option chip inside a [`MenuItem::Choice`] row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
    pub verb: Option<Vec<String>>,
    pub on: bool,
    pub custom: bool,
}

/// Inline size editor specification attached to the terminal `size` choice row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeEdit {
    pub verb: Vec<String>,
    pub cols: u32,
    pub rows: u32,
}

/// One row of the context menu (`MenuItem` in `wall/contextMenu.js`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Action {
        id: String,
        label: String,
        verb: Vec<String>,
        detail: Option<String>,
    },
    Hold {
        id: String,
        label: String,
        verb: Vec<String>,
        detail: Option<String>,
    },
    Rename {
        id: String,
        label: String,
        verb: Vec<String>,
        value: String,
        placeholder: String,
    },
    Choice {
        id: String,
        label: String,
        detail: Option<String>,
        options: Vec<ChoiceOption>,
        edit: Option<SizeEdit>,
    },
    Sep {
        id: String,
    },
}

impl MenuItem {
    /// Stable item identifier (`"paste"`, `"size"`, `"s0"`, ...).
    pub fn id(&self) -> &str {
        match self {
            Self::Action { id, .. }
            | Self::Hold { id, .. }
            | Self::Rename { id, .. }
            | Self::Choice { id, .. }
            | Self::Sep { id } => id,
        }
    }

    /// Whether this item is a visual section separator.
    pub const fn is_sep(&self) -> bool {
        matches!(self, Self::Sep { .. })
    }
}

fn curly_quote(text: &str) -> String {
    format!("“{text}”")
}

fn verb<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.into_iter().map(str::to_owned).collect()
}

fn action(id: &str, label: impl Into<String>, cmd: Vec<String>, detail: Option<String>) -> MenuItem {
    MenuItem::Action {
        id: id.to_owned(),
        label: label.into(),
        verb: cmd,
        detail,
    }
}

fn sep(id: &str) -> MenuItem {
    MenuItem::Sep { id: id.to_owned() }
}

/// Build the menu rows for `target` from `state` and `config` (`menuItems` in `wall/contextMenu.js`).
pub fn menu_items(
    target: &MenuTarget,
    state: &MenuState,
    config: &MenuConfig,
    platform: Platform,
) -> Vec<MenuItem> {
    match target {
        MenuTarget::Terminal { id, .. } => terminal_items(id, state, config, platform),
        MenuTarget::Group { id } => group_items(id, state, platform),
        MenuTarget::Canvas { world } => canvas_items(world.unwrap_or(Point::new(0.0, 0.0)), state),
    }
}

fn terminal_items(
    id: &str,
    state: &MenuState,
    config: &MenuConfig,
    platform: Platform,
) -> Vec<MenuItem> {
    let Some(term) = state.terminals.iter().find(|t| t.id == id) else {
        return Vec::new();
    };
    if term.hidden {
        return Vec::new();
    }

    let group = term
        .group
        .as_deref()
        .and_then(|gid| state.groups.iter().find(|g| g.id == gid));
    let in_selection = state.selection.iter().any(|sel| sel == id);
    let others: Vec<&str> = state
        .selection
        .iter()
        .map(String::as_str)
        .filter(|&sel| sel != id)
        .collect();
    let active_agent_side = term
        .agent_view
        .filter(|av| av.open)
        .map(|av| av.side);

    let is_solo_fs = state.fullscreen.as_deref() == Some(id);
    let is_group_fs = group.is_some_and(|g| state.fullscreen.as_deref() == Some(g.id.as_str()));
    let is_fs = is_solo_fs || is_group_fs;

    let mut items = Vec::new();
    if term.selected {
        items.push(action(
            "copy",
            "copy",
            verb(["wall.copy", id]),
            Some(copy_label(platform).to_owned()),
        ));
    }
    items.push(action(
        "paste",
        "paste",
        verb(["wall.paste", id]),
        Some(paste_label(platform).to_owned()),
    ));
    items.push(sep("s0"));

    if is_fs {
        items.push(action(
            "fullscreen-off",
            "exit fullscreen",
            verb(["wall.fullscreen", "off"]),
            Some(chord_label(platform, &["↵"], false)),
        ));
    } else {
        items.push(action("frame", "frame", verb(["wall.reveal", id]), None));
    }
    if is_group_fs {
        items.push(action(
            "fullscreen-pane",
            "fullscreen pane",
            verb(["wall.fullscreen.pane", "on", id]),
            Some(chord_label(platform, &["↵"], true)),
        ));
    }
    if state.focused_id.as_deref() != Some(id) {
        items.push(action("focus", "focus", verb(["terminal.focus", id]), None));
    }

    items.push(MenuItem::Rename {
        id: "rename".to_owned(),
        label: "rename…".to_owned(),
        verb: verb(["wall.rename", id]),
        value: if term.named {
            term.name.clone()
        } else {
            String::new()
        },
        placeholder: term.name.clone(),
    });

    let mut agent_options = Vec::with_capacity(1 + AGENT_SIDES.len());
    agent_options.push(ChoiceOption {
        id: "off".to_owned(),
        label: "off".to_owned(),
        verb: Some(verb(["wall.agent", id, "close"])),
        on: active_agent_side.is_none(),
        custom: false,
    });
    for side in AGENT_SIDES {
        let side_str = side.as_str();
        agent_options.push(ChoiceOption {
            id: side_str.to_owned(),
            label: side_str.to_owned(),
            verb: Some(verb(["wall.agent.side", id, side_str])),
            on: active_agent_side == Some(side),
            custom: false,
        });
    }
    items.push(MenuItem::Choice {
        id: "agent-view".to_owned(),
        label: "agent view".to_owned(),
        detail: None,
        options: agent_options,
        edit: None,
    });

    items.push(sep("s1"));
    items.push(action(
        "select",
        if in_selection {
            "take out of the selection"
        } else {
            "add to the selection"
        },
        verb(["wall.select.toggle", id]),
        None,
    ));

    if let Some(grp) = group {
        items.push(action(
            "leave",
            format!("take out of {}", curly_quote(&grp.name)),
            verb(["wall.group.remove", id]),
            None,
        ));
    } else {
        if !others.is_empty() {
            let mut group_verb = vec!["wall.group".to_owned()];
            for other in &others {
                if !group_verb[1..].iter().any(|seen| seen == other) {
                    group_verb.push((*other).to_owned());
                }
            }
            group_verb.push(id.to_owned());
            items.push(action(
                "group",
                format!("group with the selection ({})", others.len() + 1),
                group_verb,
                None,
            ));
        } else {
            items.push(action("group", "new group", verb(["wall.group", id]), None));
        }
        if !state.groups.is_empty() {
            let options = state
                .groups
                .iter()
                .map(|g| ChoiceOption {
                    id: g.id.clone(),
                    label: g.name.clone(),
                    verb: Some(verb(["wall.group.add", &g.id, id])),
                    on: false,
                    custom: false,
                })
                .collect();
            items.push(MenuItem::Choice {
                id: "add-to".to_owned(),
                label: "add to".to_owned(),
                detail: None,
                options,
                edit: None,
            });
        }
    }

    items.push(sep("s2"));
    items.push(action(
        "split-right",
        "split right",
        verb(["wall.split", "right", id]),
        Some(chord_label(platform, &["D"], false)),
    ));
    items.push(action(
        "split-down",
        "split down",
        verb(["wall.split", "down", id]),
        Some(chord_label(platform, &["D"], true)),
    ));

    let size_owned_elsewhere = group.is_some_and(|g| g.layout == GroupLayout::Split)
        || is_solo_fs
        || (is_group_fs && group.is_some_and(|g| g.layout != GroupLayout::Freeform));
    if size_owned_elsewhere {
        append_tail(&mut items, id, config);
        return items;
    }

    let parsed_sizes = parse_sizes(&config.menu_sizes)
        .expect("MenuConfig.menu_sizes must hold valid COLSxROWS entries");
    let mut size_options: Vec<ChoiceOption> = parsed_sizes
        .into_iter()
        .map(|z| ChoiceOption {
            id: format!("{}x{}", z.cols, z.rows),
            label: format!("{}×{}", z.cols, z.rows),
            verb: Some(vec![
                "terminal.resize".to_owned(),
                id.to_owned(),
                z.cols.to_string(),
                z.rows.to_string(),
            ]),
            on: z.cols == term.cols && z.rows == term.rows,
            custom: false,
        })
        .collect();
    let matches_offer = size_options.iter().any(|opt| opt.on);
    size_options.push(ChoiceOption {
        id: "custom".to_owned(),
        label: "custom…".to_owned(),
        verb: None,
        on: false,
        custom: true,
    });
    items.push(MenuItem::Choice {
        id: "size".to_owned(),
        label: "size".to_owned(),
        detail: if matches_offer {
            None
        } else {
            Some(format!("{}×{}", term.cols, term.rows))
        },
        options: size_options,
        edit: Some(SizeEdit {
            verb: verb(["terminal.resize", id]),
            cols: term.cols,
            rows: term.rows,
        }),
    });

    append_tail(&mut items, id, config);
    items
}

fn append_tail(items: &mut Vec<MenuItem>, id: &str, config: &MenuConfig) {
    items.push(action(
        "redraw",
        "redraw",
        verb(["terminal.refresh", id]),
        None,
    ));
    let alt_options = OPTION_AS_ALT_CHOICES
        .into_iter()
        .map(|(val, label)| {
            let raw = val.as_str();
            ChoiceOption {
                id: raw.to_owned(),
                label: label.to_owned(),
                verb: Some(verb(["wall.config", "optionAsAlt", raw])),
                on: config.option_as_alt == val,
                custom: false,
            }
        })
        .collect();
    items.push(MenuItem::Choice {
        id: "option-as-alt".to_owned(),
        label: "Option as Alt".to_owned(),
        detail: Some("every terminal".to_owned()),
        options: alt_options,
        edit: None,
    });
    items.push(sep("s3"));
    items.push(action(
        "hide",
        "hide",
        verb(["wall.hide", id]),
        Some("keeps running".to_owned()),
    ));
    items.push(MenuItem::Hold {
        id: "close".to_owned(),
        label: "close".to_owned(),
        verb: verb(["wall.close", id]),
        detail: Some("hold · kills the shell".to_owned()),
    });
}

fn group_items(gid: &str, state: &MenuState, platform: Platform) -> Vec<MenuItem> {
    let Some(group) = state.groups.iter().find(|g| g.id == gid) else {
        return Vec::new();
    };
    let shown: Vec<String> = group
        .members
        .iter()
        .filter(|mid| {
            state
                .terminals
                .iter()
                .any(|t| t.id == **mid && !t.hidden)
        })
        .cloned()
        .collect();
    let is_tile = group.layout == GroupLayout::Tile;
    let is_split = group.layout == GroupLayout::Split;
    let is_fs = state.fullscreen.as_deref() == Some(gid);

    let mut items = Vec::new();
    if is_fs {
        items.push(action(
            "fullscreen-off",
            "exit fullscreen",
            verb(["wall.fullscreen", "off"]),
            Some(chord_label(platform, &["↵"], false)),
        ));
    } else {
        items.push(action(
            "frame",
            "frame",
            verb(["wall.group.frame", gid]),
            None,
        ));
    }

    items.push(MenuItem::Rename {
        id: "rename".to_owned(),
        label: "rename…".to_owned(),
        verb: verb(["wall.group.rename", gid]),
        value: if group.named {
            group.name.clone()
        } else {
            String::new()
        },
        placeholder: group.name.clone(),
    });
    items.push(sep("s1"));

    items.push(MenuItem::Choice {
        id: "layout".to_owned(),
        label: "layout".to_owned(),
        detail: None,
        options: vec![
            ChoiceOption {
                id: "tile".to_owned(),
                label: "tile".to_owned(),
                verb: Some(verb(["wall.group.layout", gid, "tile"])),
                on: is_tile,
                custom: false,
            },
            ChoiceOption {
                id: "freeform".to_owned(),
                label: "freeform".to_owned(),
                verb: Some(verb(["wall.group.layout", gid, "freeform"])),
                on: group.layout == GroupLayout::Freeform,
                custom: false,
            },
            ChoiceOption {
                id: "split".to_owned(),
                label: "split".to_owned(),
                verb: Some(verb(["wall.group.layout", gid, "split"])),
                on: is_split,
                custom: false,
            },
        ],
        edit: None,
    });

    if is_split {
        items.push(MenuItem::Choice {
            id: "bars".to_owned(),
            label: "title bars".to_owned(),
            detail: None,
            options: vec![
                ChoiceOption {
                    id: "on".to_owned(),
                    label: "shown".to_owned(),
                    verb: Some(verb(["wall.group.bars", gid, "on"])),
                    on: group.bars,
                    custom: false,
                },
                ChoiceOption {
                    id: "off".to_owned(),
                    label: "hidden".to_owned(),
                    verb: Some(verb(["wall.group.bars", gid, "off"])),
                    on: !group.bars,
                    custom: false,
                },
            ],
            edit: None,
        });
    }

    if is_tile {
        items.push(MenuItem::Choice {
            id: "columns".to_owned(),
            label: "columns".to_owned(),
            detail: Some(group.columns.to_string()),
            options: vec![
                ChoiceOption {
                    id: "fewer".to_owned(),
                    label: "−".to_owned(),
                    verb: Some(verb(["wall.group.columns", gid, "-1"])),
                    on: false,
                    custom: false,
                },
                ChoiceOption {
                    id: "more".to_owned(),
                    label: "+".to_owned(),
                    verb: Some(verb(["wall.group.columns", gid, "+1"])),
                    on: false,
                    custom: false,
                },
            ],
            edit: None,
        });
    }

    items.push(sep("s2"));
    if !shown.is_empty() {
        let mut select_verb = vec!["wall.select".to_owned()];
        select_verb.extend(shown.iter().cloned());
        items.push(action("select", "select its terminals", select_verb, None));

        let mut hide_verb = vec!["wall.hide".to_owned()];
        hide_verb.extend(shown);
        items.push(action(
            "hide-all",
            "hide all",
            hide_verb,
            Some("they keep running".to_owned()),
        ));
    }
    items.push(action(
        "ungroup",
        "ungroup",
        verb(["wall.ungroup", gid]),
        Some("members stay put".to_owned()),
    ));

    items
}

fn canvas_items(world: Point, state: &MenuState) -> Vec<MenuItem> {
    let any_shown = state.terminals.iter().any(|t| !t.hidden);
    let mut items = Vec::new();
    items.push(action(
        "spawn",
        "new terminal here",
        vec![
            "wall.spawn".to_owned(),
            format_hundredths(world.x),
            format_hundredths(world.y),
        ],
        None,
    ));
    if any_shown {
        items.push(action("fit", "fit all", verb(["wall.fit"]), None));
        items.push(action(
            "select-all",
            "select all",
            verb(["wall.select", "all"]),
            None,
        ));
    }
    items.push(sep("s1"));
    items.push(MenuItem::Choice {
        id: "snap".to_owned(),
        label: "snap to grid".to_owned(),
        detail: None,
        options: vec![
            ChoiceOption {
                id: "on".to_owned(),
                label: "on".to_owned(),
                verb: Some(verb(["wall.snap", "on"])),
                on: state.config.snap,
                custom: false,
            },
            ChoiceOption {
                id: "off".to_owned(),
                label: "off".to_owned(),
                verb: Some(verb(["wall.snap", "off"])),
                on: !state.config.snap,
                custom: false,
            },
        ],
        edit: None,
    });
    items.push(MenuItem::Choice {
        id: "grid".to_owned(),
        label: "grid".to_owned(),
        detail: None,
        options: vec![
            ChoiceOption {
                id: "show".to_owned(),
                label: "show".to_owned(),
                verb: Some(verb(["wall.grid", "on"])),
                on: state.config.grid_show,
                custom: false,
            },
            ChoiceOption {
                id: "hide".to_owned(),
                label: "hide".to_owned(),
                verb: Some(verb(["wall.grid", "off"])),
                on: !state.config.grid_show,
                custom: false,
            },
        ],
        edit: None,
    });
    items.push(sep("s2"));
    items.push(action(
        "sidebar",
        if state.sidebar_open {
            "close the sidebar"
        } else {
            "open the sidebar"
        },
        verb([
            "wall.sidebar",
            if state.sidebar_open { "close" } else { "open" },
        ]),
        None,
    ));
    items.push(action(
        "status",
        if state.status_window_open {
            "close the status window"
        } else {
            "open the status window"
        },
        verb([
            "wall.status",
            if state.status_window_open {
                "close"
            } else {
                "open"
            },
        ]),
        None,
    ));
    items.push(action(
        "settings",
        if state.settings_panel_open {
            "close the settings"
        } else {
            "settings…"
        },
        verb([
            "wall.settings",
            if state.settings_panel_open {
                "close"
            } else {
                "open"
            },
        ]),
        None,
    ));

    items
}

/// Round to hundredths with JS `Math.round` tie-toward-+infinity semantics and
/// format without `-0`.
fn format_hundredths(value: f64) -> String {
    let scaled = value * 100.0;
    let rounded_int = if !scaled.is_finite() || scaled == 0.0 {
        scaled
    } else if scaled > 0.0 {
        let floor = scaled.floor();
        if scaled - floor >= 0.5 {
            floor + 1.0
        } else {
            floor
        }
    } else {
        let ceil = scaled.ceil();
        if ceil - scaled > 0.5 {
            ceil - 1.0
        } else {
            ceil
        }
    };
    let hundredths = rounded_int / 100.0;
    let clean = if hundredths == 0.0 { 0.0 } else { hundredths };
    clean.to_string()
}

/// Width and height of a menu box or viewport in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MenuDimensions {
    pub w: f64,
    pub h: f64,
}

impl MenuDimensions {
    pub const fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }
}

/// Default viewport edge margin in CSS pixels for [`place_menu`].
pub const DEFAULT_MENU_MARGIN: f64 = 4.0;

/// Place the menu's top-left corner at `at`, flipping before `at` when it would
/// cross the right or bottom edge, and clamping inside [`DEFAULT_MENU_MARGIN`]
/// when neither side fits (`placeMenu` in `wall/contextMenu.js`).
pub fn place_menu(at: Point, size: MenuDimensions, view: MenuDimensions) -> Point {
    place_menu_with_margin(at, size, view, DEFAULT_MENU_MARGIN)
}

/// Like [`place_menu`], with an explicit viewport margin in CSS pixels.
pub fn place_menu_with_margin(
    at: Point,
    size: MenuDimensions,
    view: MenuDimensions,
    margin: f64,
) -> Point {
    fn place_axis(point: f64, extent: f64, viewport: f64, margin: f64) -> f64 {
        if point + extent <= viewport - margin {
            return point;
        }
        if point - extent >= margin {
            return point - extent;
        }
        margin.max(viewport - margin - extent)
    }

    Point {
        x: place_axis(at.x, size.w, view.w, margin),
        y: place_axis(at.y, size.h, view.h, margin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn term_target(part: TerminalPart) -> MenuTarget {
        MenuTarget::Terminal {
            id: "t1".to_owned(),
            part,
        }
    }

    fn snap_fixture() -> MenuState {
        MenuState {
            focused_id: None,
            selection: vec!["t2".to_owned()],
            fullscreen: None,
            sidebar_open: false,
            status_window_open: true,
            settings_panel_open: false,
            config: MenuConfig {
                snap: true,
                grid_show: false,
                ..MenuConfig::default()
            },
            terminals: vec![
                MenuTerminal {
                    id: "t1".to_owned(),
                    name: "zsh".to_owned(),
                    named: false,
                    cols: 80,
                    rows: 24,
                    hidden: false,
                    group: None,
                    selected: false,
                    agent_view: None,
                },
                MenuTerminal {
                    id: "t2".to_owned(),
                    name: "logs".to_owned(),
                    named: true,
                    cols: 120,
                    rows: 36,
                    hidden: false,
                    group: None,
                    selected: true,
                    agent_view: None,
                },
                MenuTerminal {
                    id: "t3".to_owned(),
                    name: "build".to_owned(),
                    named: false,
                    cols: 80,
                    rows: 24,
                    hidden: false,
                    group: Some("g-1".to_owned()),
                    selected: false,
                    agent_view: None,
                },
                MenuTerminal {
                    id: "t4".to_owned(),
                    name: "old".to_owned(),
                    named: false,
                    cols: 80,
                    rows: 24,
                    hidden: true,
                    group: Some("g-1".to_owned()),
                    selected: false,
                    agent_view: None,
                },
            ],
            groups: vec![MenuGroup {
                id: "g-1".to_owned(),
                name: "group 1".to_owned(),
                named: false,
                layout: GroupLayout::Tile,
                columns: 2,
                bars: false,
                members: vec!["t3".to_owned(), "t4".to_owned()],
            }],
        }
    }

    fn non_sep_ids(items: &[MenuItem]) -> Vec<&str> {
        items
            .iter()
            .filter(|i| !i.is_sep())
            .map(MenuItem::id)
            .collect()
    }

    fn by_id(items: &[MenuItem]) -> HashMap<&str, &MenuItem> {
        items
            .iter()
            .filter(|i| !i.is_sep())
            .map(|i| (i.id(), i))
            .collect()
    }

    fn strs<const N: usize>(arr: [&str; N]) -> Vec<String> {
        arr.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn event_policy_first_right_click_ours_again_native_shift_native() {
        let base = ContextEvent::default();
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                ..base.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Title)),
                ..base.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(MenuTarget::Group {
                    id: "g-1".to_owned()
                }),
                ..base.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(MenuTarget::Canvas { world: None }),
                ..base.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                menu_open: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(MenuTarget::Canvas { world: None }),
                menu_open: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                shift_key: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(MenuTarget::Canvas { world: None }),
                shift_key: true,
                ..base
            })
            .verdict,
            Verdict::Native
        );
    }

    #[test]
    fn event_policy_focused_reporting_body_is_programs_shift_ours_unfocused_ours() {
        let rep = ContextEvent {
            mouse_reporting: true,
            focused: true,
            ..ContextEvent::default()
        };
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                ..rep.clone()
            })
            .verdict,
            Verdict::Program
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                shift_key: true,
                ..rep.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Title)),
                ..rep.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Edge)),
                shift_key: true,
                ..rep.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                focused: false,
                target: Some(term_target(TerminalPart::Body)),
                ..rep.clone()
            })
            .verdict,
            Verdict::Ours
        );
        assert_eq!(
            context_policy(&ContextEvent {
                focused: false,
                target: Some(term_target(TerminalPart::Body)),
                shift_key: true,
                ..rep.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                target: Some(term_target(TerminalPart::Body)),
                menu_open: true,
                ..rep
            })
            .verdict,
            Verdict::Native
        );
    }

    #[test]
    fn event_policy_inputs_chrome_native_and_menu_key_ours() {
        let base = ContextEvent::default();
        assert_eq!(
            context_policy(&ContextEvent {
                editable: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                chrome: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(context_policy(&base).verdict, Verdict::Native);
        assert_eq!(
            context_policy(&ContextEvent {
                editable: true,
                menu_open: true,
                ..base.clone()
            })
            .verdict,
            Verdict::Native
        );
        assert_eq!(
            context_policy(&ContextEvent {
                keyboard: true,
                shift_key: true,
                menu_open: true,
                ..base
            })
            .verdict,
            Verdict::Ours
        );
    }

    #[test]
    fn secondary_press_right_button_or_ctrl_click_on_macos_only() {
        let no_mods = Mods::default();
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert!(is_secondary_press(Button::Right, no_mods, Platform::Other));
        assert!(is_secondary_press(Button::Left, ctrl, Platform::Mac));
        assert!(!is_secondary_press(Button::Left, ctrl, Platform::Other));
        assert!(!is_secondary_press(Button::Left, no_mods, Platform::Mac));
    }

    #[test]
    fn terminal_menu_lists_expected_items_and_verbs() {
        let snap = snap_fixture();
        let cfg = MenuConfig::default();
        let raw = menu_items(
            &MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            &snap,
            &cfg,
            Platform::Mac,
        );
        assert_eq!(
            non_sep_ids(&raw),
            vec![
                "paste",
                "frame",
                "focus",
                "rename",
                "agent-view",
                "select",
                "group",
                "add-to",
                "split-right",
                "split-down",
                "size",
                "redraw",
                "option-as-alt",
                "hide",
                "close",
            ]
        );
        let items = by_id(&raw);

        let MenuItem::Choice {
            options: agent_opts,
            ..
        } = items["agent-view"]
        else {
            panic!("agent-view must be Choice");
        };
        let agent_pairs: Vec<(&str, bool)> =
            agent_opts.iter().map(|o| (o.id.as_str(), o.on)).collect();
        assert_eq!(
            agent_pairs,
            vec![
                ("off", true),
                ("right", false),
                ("left", false),
                ("top", false),
                ("bottom", false),
            ]
        );
        assert_eq!(
            agent_opts[1].verb,
            Some(strs(["wall.agent.side", "t1", "right"]))
        );

        let MenuItem::Action {
            verb: paste_verb, ..
        } = items["paste"]
        else {
            panic!("paste must be Action");
        };
        assert_eq!(paste_verb, &strs(["wall.paste", "t1"]));

        let raw_t2 = menu_items(
            &MenuTarget::Terminal {
                id: "t2".to_owned(),
                part: TerminalPart::Body,
            },
            &snap,
            &cfg,
            Platform::Mac,
        );
        assert_eq!(&non_sep_ids(&raw_t2)[..2], &["copy", "paste"]);
        let t2_items = by_id(&raw_t2);
        let MenuItem::Action {
            verb: copy_verb, ..
        } = t2_items["copy"]
        else {
            panic!("copy must be Action");
        };
        assert_eq!(copy_verb, &strs(["wall.copy", "t2"]));

        let MenuItem::Action {
            verb: frame_verb, ..
        } = items["frame"]
        else {
            panic!("frame must be Action");
        };
        assert_eq!(frame_verb, &strs(["wall.reveal", "t1"]));

        let MenuItem::Action {
            verb: focus_verb, ..
        } = items["focus"]
        else {
            panic!("focus must be Action");
        };
        assert_eq!(focus_verb, &strs(["terminal.focus", "t1"]));

        assert_eq!(
            items["rename"],
            &MenuItem::Rename {
                id: "rename".to_owned(),
                label: "rename…".to_owned(),
                verb: strs(["wall.rename", "t1"]),
                value: String::new(),
                placeholder: "zsh".to_owned(),
            }
        );

        let MenuItem::Action {
            verb: select_verb, ..
        } = items["select"]
        else {
            panic!("select must be Action");
        };
        assert_eq!(select_verb, &strs(["wall.select.toggle", "t1"]));

        let MenuItem::Action {
            verb: group_verb, ..
        } = items["group"]
        else {
            panic!("group must be Action");
        };
        assert_eq!(group_verb, &strs(["wall.group", "t2", "t1"]));

        let MenuItem::Choice {
            options: add_to_opts,
            ..
        } = items["add-to"]
        else {
            panic!("add-to must be Choice");
        };
        assert_eq!(
            add_to_opts[0].verb,
            Some(strs(["wall.group.add", "g-1", "t1"]))
        );

        let MenuItem::Choice {
            detail: size_detail,
            options: size_opts,
            edit: size_edit,
            ..
        } = items["size"]
        else {
            panic!("size must be Choice");
        };
        let size_labels: Vec<(&str, bool)> =
            size_opts.iter().map(|o| (o.label.as_str(), o.on)).collect();
        assert_eq!(
            size_labels,
            vec![
                ("80×24", true),
                ("120×36", false),
                ("160×48", false),
                ("custom…", false),
            ]
        );
        let last_size = size_opts.last().unwrap();
        assert_eq!(last_size.id, "custom");
        assert!(last_size.custom);
        assert_eq!(
            size_edit,
            &Some(SizeEdit {
                verb: strs(["terminal.resize", "t1"]),
                cols: 80,
                rows: 24,
            })
        );
        assert_eq!(
            size_opts[1].verb,
            Some(strs(["terminal.resize", "t1", "120", "36"]))
        );
        assert_eq!(size_detail, &None);

        let mut odd = snap.clone();
        odd.terminals[0].cols = 100;
        odd.terminals[0].rows = 30;
        let odd_raw = menu_items(
            &MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            &odd,
            &cfg,
            Platform::Mac,
        );
        let MenuItem::Choice {
            detail: odd_detail, ..
        } = by_id(&odd_raw)["size"]
        else {
            panic!("size must be Choice");
        };
        assert_eq!(odd_detail.as_deref(), Some("100×30"));

        let MenuItem::Action {
            verb: redraw_verb, ..
        } = items["redraw"]
        else {
            panic!("redraw must be Action");
        };
        assert_eq!(redraw_verb, &strs(["terminal.refresh", "t1"]));

        let MenuItem::Choice {
            options: opt_alt_opts,
            ..
        } = items["option-as-alt"]
        else {
            panic!("option-as-alt must be Choice");
        };
        let alt_pairs: Vec<(&str, bool)> = opt_alt_opts
            .iter()
            .map(|o| (o.verb.as_ref().unwrap().last().unwrap().as_str(), o.on))
            .collect();
        assert_eq!(
            alt_pairs,
            vec![("auto", true), ("true", false), ("false", false)]
        );

        let MenuItem::Action {
            verb: hide_verb, ..
        } = items["hide"]
        else {
            panic!("hide must be Action");
        };
        assert_eq!(hide_verb, &strs(["wall.hide", "t1"]));

        assert_eq!(
            items["close"],
            &MenuItem::Hold {
                id: "close".to_owned(),
                label: "close".to_owned(),
                verb: strs(["wall.close", "t1"]),
                detail: Some("hold · kills the shell".to_owned()),
            }
        );

        let mut grouped_state = snap.clone();
        grouped_state.focused_id = Some("t3".to_owned());
        grouped_state.selection.clear();
        let grouped_raw = menu_items(
            &MenuTarget::Terminal {
                id: "t3".to_owned(),
                part: TerminalPart::Title,
            },
            &grouped_state,
            &cfg,
            Platform::Mac,
        );
        let grouped = by_id(&grouped_raw);
        assert!(!grouped.contains_key("focus"));
        let MenuItem::Action {
            verb: leave_verb,
            label: leave_label,
            ..
        } = grouped["leave"]
        else {
            panic!("leave must be Action");
        };
        assert_eq!(leave_verb, &strs(["wall.group.remove", "t3"]));
        assert_eq!(leave_label, "take out of “group 1”");
        assert!(!grouped.contains_key("group"));
        assert!(!grouped.contains_key("add-to"));

        assert!(
            menu_items(
                &MenuTarget::Terminal {
                    id: "t4".to_owned(),
                    part: TerminalPart::Body,
                },
                &snap,
                &cfg,
                Platform::Mac,
            )
            .is_empty()
        );
        assert!(
            menu_items(
                &MenuTarget::Terminal {
                    id: "nope".to_owned(),
                    part: TerminalPart::Body,
                },
                &snap,
                &cfg,
                Platform::Mac,
            )
            .is_empty()
        );

        let empty_sizes_cfg = MenuConfig {
            menu_sizes: String::new(),
            ..cfg
        };
        let no_offers_raw = menu_items(
            &MenuTarget::Terminal {
                id: "t1".to_owned(),
                part: TerminalPart::Body,
            },
            &snap,
            &empty_sizes_cfg,
            Platform::Mac,
        );
        let MenuItem::Choice {
            detail: none_detail,
            options: none_opts,
            ..
        } = by_id(&no_offers_raw)["size"]
        else {
            panic!("size must be Choice");
        };
        let none_ids: Vec<&str> = none_opts.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(none_ids, vec!["custom"]);
        assert_eq!(none_detail.as_deref(), Some("80×24"));
    }

    #[test]
    fn group_header_menu_lists_expected_items_and_verbs() {
        let snap = snap_fixture();
        let cfg = MenuConfig::default();
        let raw = menu_items(
            &MenuTarget::Group {
                id: "g-1".to_owned(),
            },
            &snap,
            &cfg,
            Platform::Mac,
        );
        assert_eq!(
            non_sep_ids(&raw),
            vec![
                "frame", "rename", "layout", "columns", "select", "hide-all", "ungroup"
            ]
        );
        let items = by_id(&raw);

        let MenuItem::Choice {
            options: layout_opts,
            ..
        } = items["layout"]
        else {
            panic!("layout must be Choice");
        };
        let layout_pairs: Vec<(Vec<String>, bool)> = layout_opts
            .iter()
            .map(|o| (o.verb.clone().unwrap(), o.on))
            .collect();
        assert_eq!(
            layout_pairs,
            vec![
                (strs(["wall.group.layout", "g-1", "tile"]), true),
                (strs(["wall.group.layout", "g-1", "freeform"]), false),
                (strs(["wall.group.layout", "g-1", "split"]), false),
            ]
        );

        let MenuItem::Choice {
            options: col_opts, ..
        } = items["columns"]
        else {
            panic!("columns must be Choice");
        };
        let col_verbs: Vec<Vec<String>> = col_opts.iter().map(|o| o.verb.clone().unwrap()).collect();
        assert_eq!(
            col_verbs,
            vec![
                strs(["wall.group.columns", "g-1", "-1"]),
                strs(["wall.group.columns", "g-1", "+1"]),
            ]
        );

        let MenuItem::Action {
            verb: hide_all_verb,
            ..
        } = items["hide-all"]
        else {
            panic!("hide-all must be Action");
        };
        assert_eq!(hide_all_verb, &strs(["wall.hide", "t3"]));

        let MenuItem::Action {
            verb: ungroup_verb, ..
        } = items["ungroup"]
        else {
            panic!("ungroup must be Action");
        };
        assert_eq!(ungroup_verb, &strs(["wall.ungroup", "g-1"]));

        let mut free_state = snap.clone();
        free_state.groups[0].layout = GroupLayout::Freeform;
        let free_raw = menu_items(
            &MenuTarget::Group {
                id: "g-1".to_owned(),
            },
            &free_state,
            &cfg,
            Platform::Mac,
        );
        assert!(!by_id(&free_raw).contains_key("columns"));

        assert!(
            menu_items(
                &MenuTarget::Group {
                    id: "g-9".to_owned(),
                },
                &snap,
                &cfg,
                Platform::Mac,
            )
            .is_empty()
        );
    }

    #[test]
    fn canvas_menu_lists_spawn_fit_select_all_snap_grid_and_panels() {
        let snap = snap_fixture();
        let cfg = MenuConfig::default();
        let raw = menu_items(
            &MenuTarget::Canvas {
                world: Some(Point::new(12.3456, -40.0)),
            },
            &snap,
            &cfg,
            Platform::Mac,
        );
        assert_eq!(
            non_sep_ids(&raw),
            vec![
                "spawn",
                "fit",
                "select-all",
                "snap",
                "grid",
                "sidebar",
                "status",
                "settings"
            ]
        );
        let items = by_id(&raw);

        let MenuItem::Choice {
            options: snap_opts, ..
        } = items["snap"]
        else {
            panic!("snap must be Choice");
        };
        let snap_pairs: Vec<(Vec<String>, bool)> = snap_opts
            .iter()
            .map(|o| (o.verb.clone().unwrap(), o.on))
            .collect();
        assert_eq!(
            snap_pairs,
            vec![
                (strs(["wall.snap", "on"]), true),
                (strs(["wall.snap", "off"]), false),
            ]
        );

        let MenuItem::Choice {
            options: grid_opts, ..
        } = items["grid"]
        else {
            panic!("grid must be Choice");
        };
        let grid_triples: Vec<(&str, Vec<String>, bool)> = grid_opts
            .iter()
            .map(|o| (o.label.as_str(), o.verb.clone().unwrap(), o.on))
            .collect();
        assert_eq!(
            grid_triples,
            vec![
                ("show", strs(["wall.grid", "on"]), false),
                ("hide", strs(["wall.grid", "off"]), true),
            ]
        );

        let MenuItem::Action {
            label: settings_label,
            verb: settings_verb,
            ..
        } = items["settings"]
        else {
            panic!("settings must be Action");
        };
        assert_eq!(settings_label, "settings…");
        assert_eq!(settings_verb, &strs(["wall.settings", "open"]));

        let MenuItem::Action {
            verb: spawn_verb, ..
        } = items["spawn"]
        else {
            panic!("spawn must be Action");
        };
        assert_eq!(spawn_verb, &strs(["wall.spawn", "12.35", "-40"]));

        let MenuItem::Action { verb: fit_verb, .. } = items["fit"] else {
            panic!("fit must be Action");
        };
        assert_eq!(fit_verb, &strs(["wall.fit"]));

        let MenuItem::Action {
            verb: select_all_verb,
            ..
        } = items["select-all"]
        else {
            panic!("select-all must be Action");
        };
        assert_eq!(select_all_verb, &strs(["wall.select", "all"]));

        let MenuItem::Action {
            verb: sidebar_verb, ..
        } = items["sidebar"]
        else {
            panic!("sidebar must be Action");
        };
        assert_eq!(sidebar_verb, &strs(["wall.sidebar", "open"]));

        let MenuItem::Action {
            label: status_label,
            verb: status_verb,
            ..
        } = items["status"]
        else {
            panic!("status must be Action");
        };
        assert_eq!(status_label, "close the status window");
        assert_eq!(status_verb, &strs(["wall.status", "close"]));

        let mut empty_wall = snap;
        empty_wall.terminals.clear();
        let empty_raw = menu_items(
            &MenuTarget::Canvas { world: None },
            &empty_wall,
            &cfg,
            Platform::Mac,
        );
        assert_eq!(
            non_sep_ids(&empty_raw),
            vec!["spawn", "snap", "grid", "sidebar", "status", "settings"]
        );
    }

    #[test]
    fn custom_size_checks_whole_cols_and_rows_up_to_u16() {
        assert_eq!(check_size("100", " 7 "), Ok(CellSize::new(100, 7)));
        assert_eq!(
            check_size("1", &MAX_CELLS.to_string()),
            Ok(CellSize::new(1, MAX_CELLS))
        );
        assert_eq!(
            check_size("0", "24"),
            Err("cols: at least 1".to_owned())
        );
        assert!(
            check_size("80", "")
                .unwrap_err()
                .starts_with("rows: a whole number")
        );
        assert_eq!(
            check_size("8x", "24"),
            Err("cols: '8x' isn't a whole number".to_owned())
        );
        assert_eq!(
            check_size("80", "2.5"),
            Err("rows: '2.5' isn't a whole number".to_owned())
        );
        assert!(
            check_size("-3", "24")
                .unwrap_err()
                .contains("isn't a whole number")
        );
        assert!(
            check_size("80", &(MAX_CELLS + 1).to_string())
                .unwrap_err()
                .starts_with("rows: at most 65535")
        );
    }

    #[test]
    fn parse_sizes_accepts_ascii_and_unicode_cross_and_rejects_bad_tokens() {
        assert_eq!(
            parse_sizes("80x24 100×30"),
            Ok(vec![CellSize::new(80, 24), CellSize::new(100, 30)])
        );
        assert_eq!(parse_sizes(""), Ok(Vec::new()));
        let bad_word = parse_sizes("80x24 big").unwrap_err();
        assert!(bad_word.starts_with("sizes are COLSxROWS"));
        assert!(bad_word.ends_with("not 'big'"));
        assert!(parse_sizes("0x24").unwrap_err().contains("COLSxROWS"));
    }

    #[test]
    fn place_menu_flips_at_right_and_bottom_edges_and_stays_in_view() {
        let view = MenuDimensions::new(800.0, 600.0);
        let size = MenuDimensions::new(200.0, 300.0);
        assert_eq!(
            place_menu(Point::new(100.0, 100.0), size, view),
            Point::new(100.0, 100.0)
        );
        assert_eq!(
            place_menu(Point::new(700.0, 100.0), size, view),
            Point::new(500.0, 100.0)
        );
        assert_eq!(
            place_menu(Point::new(100.0, 500.0), size, view),
            Point::new(100.0, 200.0)
        );
        assert_eq!(
            place_menu(
                Point::new(150.0, 200.0),
                size,
                MenuDimensions::new(300.0, 400.0)
            ),
            Point::new(96.0, 96.0)
        );
        assert_eq!(
            place_menu(
                Point::new(10.0, 10.0),
                MenuDimensions::new(900.0, 100.0),
                view
            ),
            Point::new(4.0, 10.0)
        );
    }

    #[test]
    fn fullscreen_split_bars_and_non_mac_chord_details_match_js_rules() {
        let mut state = snap_fixture();
        state.fullscreen = Some("g-1".to_owned());
        state.terminals[2].selected = true;
        state.terminals[2].agent_view = Some(TerminalAgentView {
            open: true,
            side: AgentSide::Left,
        });
        let cfg = MenuConfig::default();

        // Tile group in fullscreen: exit fullscreen + fullscreen pane, no size row, Linux/Win chord details.
        let raw_term = menu_items(
            &MenuTarget::Terminal {
                id: "t3".to_owned(),
                part: TerminalPart::Body,
            },
            &state,
            &cfg,
            Platform::Other,
        );
        let items = by_id(&raw_term);
        assert!(!items.contains_key("frame"));
        assert!(!items.contains_key("size"));
        let MenuItem::Action {
            detail: copy_detail,
            ..
        } = items["copy"]
        else {
            panic!("copy must be Action");
        };
        assert_eq!(copy_detail.as_deref(), Some("Ctrl+Shift+C"));
        let MenuItem::Action {
            detail: fs_off_detail,
            verb: fs_off_verb,
            ..
        } = items["fullscreen-off"]
        else {
            panic!("fullscreen-off must be Action");
        };
        assert_eq!(fs_off_detail.as_deref(), Some("Ctrl+Alt+↵"));
        assert_eq!(fs_off_verb, &strs(["wall.fullscreen", "off"]));
        let MenuItem::Action {
            detail: fs_pane_detail,
            verb: fs_pane_verb,
            ..
        } = items["fullscreen-pane"]
        else {
            panic!("fullscreen-pane must be Action");
        };
        assert_eq!(fs_pane_detail.as_deref(), Some("Ctrl+Alt+Shift+↵"));
        assert_eq!(fs_pane_verb, &strs(["wall.fullscreen.pane", "on", "t3"]));

        // Freeform group in fullscreen keeps its size row.
        state.groups[0].layout = GroupLayout::Freeform;
        let free_fs = menu_items(
            &MenuTarget::Terminal {
                id: "t3".to_owned(),
                part: TerminalPart::Body,
            },
            &state,
            &cfg,
            Platform::Mac,
        );
        assert!(by_id(&free_fs).contains_key("size"));

        // Split group header offers title bars toggle and fullscreen-off when fullscreen.
        state.groups[0].layout = GroupLayout::Split;
        state.groups[0].bars = true;
        let split_grp = menu_items(
            &MenuTarget::Group {
                id: "g-1".to_owned(),
            },
            &state,
            &cfg,
            Platform::Mac,
        );
        let grp_items = by_id(&split_grp);
        assert!(grp_items.contains_key("fullscreen-off"));
        let MenuItem::Choice {
            options: bar_opts, ..
        } = grp_items["bars"]
        else {
            panic!("bars must be Choice");
        };
        let bar_pairs: Vec<(&str, bool)> =
            bar_opts.iter().map(|o| (o.id.as_str(), o.on)).collect();
        assert_eq!(bar_pairs, vec![("on", true), ("off", false)]);
    }
}
