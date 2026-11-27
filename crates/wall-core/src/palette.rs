//! Port of the pure model inside `wall/Palette.jsx` (the command/find omnibar),
//! with no React or DOM:
//!
//! - [`subsequence_score`]: case-insensitive gap-counting subsequence matcher.
//! - [`direct_token_input`] and [`is_bare_token_command`]: inline `wall.token`
//!   and `#token=…` query shortcuts.
//! - [`palette_items`]: builds the full list of terminals (hidden ones marked),
//!   groups, gomuxai agents by host, and context-sensitive action verbs in tree
//!   order (empty query) or gap-score rank order (non-empty query).
//! - [`choose_item`] and [`Palette`]: mode transitions (`Find`, `Rename`,
//!   `Token`), selection clamping, and what `Enter` runs.

use std::collections::{HashMap, HashSet};

use crate::chord::chord_label;
use crate::config::GroupLayout;
use crate::input::Platform;
use crate::sidebar::{
    bus_attachable, bus_hosts, resolve_agent_on_wall, AgentStatus, BusSnapshot,
};

/// Case-insensitive subsequence match score (`score` in `wall/Palette.jsx`):
/// returns the total number of skipped characters between consecutive matched
/// positions (`0` for an empty query or contiguous substring), or `None` when
/// `query` is not a subsequence of `candidate`. Lower is better.
pub fn subsequence_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let mut qi = 0usize;
    let mut gaps = 0usize;
    let mut last: Option<usize> = None;
    for (i, ch) in candidate.to_lowercase().chars().enumerate() {
        if ch == q[qi] {
            if let Some(prev) = last {
                gaps += i - prev - 1;
            }
            last = Some(i);
            qi += 1;
            if qi == q.len() {
                break;
            }
        }
    }
    (qi == q.len()).then_some(gaps)
}

/// Format a group's layout summary (`layoutText` in `wall/Palette.jsx`).
pub fn group_layout_text(layout: GroupLayout, columns: usize) -> String {
    if layout == GroupLayout::Tile {
        format!(
            "tile · {columns} col{}",
            if columns == 1 { "" } else { "s" }
        )
    } else {
        "freeform".to_string()
    }
}

/// Extract `<val>` when the user types `wall.token <val>` / `wall token <val>`
/// or pastes a `#token=…` URL/fragment in find mode (`directTokenInput` in
/// `wall/Palette.jsx`).
pub fn direct_token_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(after_wall) = trimmed
        .get(..4)
        .and_then(|p| p.eq_ignore_ascii_case("wall").then_some(&trimmed[4..]))
    {
        let after_sep = if let Some(rest) = after_wall.strip_prefix('.') {
            Some(rest)
        } else if after_wall.starts_with(char::is_whitespace) {
            Some(after_wall.trim_start())
        } else {
            None
        };
        if let Some(s) = after_sep {
            if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("token")) {
                let tail = &s[5..];
                if tail.starts_with(char::is_whitespace) {
                    let val = tail.trim();
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }

    for (idx, _) in trimmed.match_indices('#') {
        let frag = &trimmed[idx + 1..];
        let seg_end = frag
            .find(|c: char| c == '#' || c.is_whitespace())
            .unwrap_or(frag.len());
        let seg_lower = frag[..seg_end].to_ascii_lowercase();
        if seg_lower.starts_with("token=")
            || seg_lower.contains("&token=")
            || seg_lower.contains(";token=")
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Whether `raw` (after trimming) is the bare `wall.token` / `wall token`
/// command that switches the palette into [`PaletteMode::Token`].
pub fn is_bare_token_command(raw: &str) -> bool {
    let trimmed = raw.trim();
    let Some(after_wall) = trimmed
        .get(..4)
        .and_then(|p| p.eq_ignore_ascii_case("wall").then_some(&trimmed[4..]))
    else {
        return false;
    };
    let after_sep = if let Some(rest) = after_wall.strip_prefix('.') {
        Some(rest)
    } else if after_wall.starts_with(char::is_whitespace) {
        Some(after_wall.trim_start())
    } else {
        None
    };
    after_sep.is_some_and(|s| s.eq_ignore_ascii_case("token"))
}

/// Terminal snapshot fields read by [`palette_items`] and [`Palette`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteTerminal {
    pub id: String,
    pub name: String,
    /// True when the user explicitly renamed this terminal (`focused.named`).
    pub named: bool,
    pub group: Option<String>,
    pub hidden: bool,
    /// Last non-empty screen line (`t.preview`).
    pub preview: String,
    /// Attached gomuxai agent target, if any (`t.attached`).
    pub attached: Option<String>,
}

/// Group snapshot fields read by [`palette_items`] and [`Palette`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteGroup {
    pub id: String,
    pub name: String,
    /// True when the group has a custom name rather than default `"group N"`.
    pub named: bool,
    pub layout: GroupLayout,
    pub columns: usize,
    pub members: Vec<String>,
}

/// Wall state snapshot read by [`palette_items`] and [`Palette`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteSnapshot {
    pub terminals: Vec<PaletteTerminal>,
    pub groups: Vec<PaletteGroup>,
    pub focused_id: Option<String>,
    pub selection: Vec<String>,
    pub sidebar_open: bool,
    pub settings_open: bool,
    pub snap: bool,
    pub grid_show: bool,
    pub platform: Platform,
    /// Optional HTTP relay port (`Some(port)` adds the `"set relay token…"`
    /// action with detail `":{port}"`; `None` omits that browser-relay action
    /// when connected directly to `g3d-termd` over a Unix socket).
    pub relay_port: Option<u16>,
    pub bus: BusSnapshot,
}

impl Default for PaletteSnapshot {
    fn default() -> Self {
        Self {
            terminals: Vec::new(),
            groups: Vec::new(),
            focused_id: None,
            selection: Vec::new(),
            sidebar_open: false,
            settings_open: false,
            snap: false,
            grid_show: false,
            platform: Platform::Mac,
            relay_port: None,
            bus: BusSnapshot::default(),
        }
    }
}

/// What a rename session in the palette edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenameKind {
    Terminal,
    Group,
}

/// Active mode of the palette (`find`, `rename`, or `token`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PaletteMode {
    #[default]
    Find,
    Rename {
        kind: RenameKind,
        id: String,
    },
    Token,
}

/// What happens when an item is chosen or `Enter` is pressed in the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteEffect {
    /// Dispatch `cmd` to the wall (`wall.run(cmd)`); close the palette iff `close`.
    Run { cmd: Vec<String>, close: bool },
    /// Switch the palette into `Rename` mode seeded with `initial_query`.
    EnterRename {
        kind: RenameKind,
        id: String,
        initial_query: String,
    },
    /// Switch the palette into `Token` mode with an empty query.
    EnterToken,
    /// Nothing to run (e.g. `Enter` on an empty match list).
    Noop,
}

/// Specific payload of a [`PaletteItem`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteItemKind {
    Terminal {
        id: String,
    },
    Group {
        id: String,
    },
    Agent {
        target: String,
        status: AgentStatus,
        attachable: bool,
        on_wall: Option<String>,
    },
    Action {
        id: String,
        effect: PaletteEffect,
    },
}

/// One visible row in the palette's `Find` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    /// Stable row key (`"terminal:<id>"`, `"group:<id>"`, `"agent:<target>"`, `"action:<id>"`).
    pub key: String,
    pub kind: PaletteItemKind,
    /// Raw item label (`t.name`, `g.name`, `a.target`, or action label).
    pub label: String,
    /// Right-hand detail / chord hint column.
    pub detail: String,
    /// Indentation depth (`1` for group members when `query` is empty; `0` otherwise).
    pub depth: u8,
    /// True for hidden terminals (marks the row hidden and renders label in italic).
    pub hidden: bool,
    /// Dimmed colour flag (`!attachable` for agents, `hidden` for terminals).
    pub dim: bool,
    /// Subsequence gap score (`0` when `query` is empty or for actions).
    pub score: usize,
}

impl PaletteItem {
    /// Label text with the `▤ ` (group) or `◆ ` (agent) prefix drawn by `Palette.jsx`.
    pub fn display_label(&self) -> String {
        match &self.kind {
            PaletteItemKind::Group { .. } => format!("▤ {}", self.label),
            PaletteItemKind::Agent { .. } => format!("◆ {}", self.label),
            PaletteItemKind::Terminal { .. } | PaletteItemKind::Action { .. } => self.label.clone(),
        }
    }

    /// Hex text colour chosen by `labelColor` in `Palette.jsx`.
    pub const fn label_color(&self) -> &'static str {
        match &self.kind {
            PaletteItemKind::Agent { status, .. } => {
                if self.dim {
                    "#6b7383"
                } else {
                    status.color()
                }
            }
            PaletteItemKind::Action { .. } => "#8ab4f8",
            PaletteItemKind::Group { .. } => "#aeb7c6",
            PaletteItemKind::Terminal { .. } => {
                if self.hidden {
                    "#6b7383"
                } else {
                    "#e6e9ef"
                }
            }
        }
    }
}

fn terminal_detail(t: &PaletteTerminal, group_prefix: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if let Some(gname) = group_prefix {
        parts.push(format!("in {gname}"));
    }
    if t.hidden {
        parts.push("hidden".to_string());
    }
    if t.name != t.id && !t.id.is_empty() {
        parts.push(t.id.clone());
    }
    if !t.preview.is_empty() {
        parts.push(t.preview.clone());
    }
    parts.join(" · ")
}

fn make_terminal_item(
    t: &PaletteTerminal,
    depth: u8,
    group_prefix: Option<&str>,
    score: usize,
) -> PaletteItem {
    PaletteItem {
        key: format!("terminal:{}", t.id),
        kind: PaletteItemKind::Terminal { id: t.id.clone() },
        label: t.name.clone(),
        detail: terminal_detail(t, group_prefix),
        depth,
        hidden: t.hidden,
        dim: t.hidden,
        score,
    }
}

fn make_group_item(g: &PaletteGroup, score: usize) -> PaletteItem {
    PaletteItem {
        key: format!("group:{}", g.id),
        kind: PaletteItemKind::Group { id: g.id.clone() },
        label: g.name.clone(),
        detail: format!(
            "{} · {}",
            g.members.len(),
            group_layout_text(g.layout, g.columns)
        ),
        depth: 0,
        hidden: false,
        dim: false,
        score,
    }
}

struct RawAction {
    id: String,
    label: String,
    detail: String,
    effect: PaletteEffect,
}

fn run_cmd<I, S>(parts: I, close: bool) -> PaletteEffect
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    PaletteEffect::Run {
        cmd: parts.into_iter().map(Into::into).collect(),
        close,
    }
}

fn build_actions(
    snap: &PaletteSnapshot,
    focused: Option<&PaletteTerminal>,
    fg: Option<&PaletteGroup>,
) -> Vec<RawAction> {
    let p = snap.platform;
    let mut out = vec![
        RawAction {
            id: "spawn".to_string(),
            label: "+ new terminal".to_string(),
            detail: chord_label(p, &["\\"], false),
            effect: run_cmd(["wall.spawn"], true),
        },
        RawAction {
            id: "fit".to_string(),
            label: "fit all".to_string(),
            detail: chord_label(p, &["0"], false),
            effect: run_cmd(["wall.fit"], true),
        },
        RawAction {
            id: "sidebar".to_string(),
            label: if snap.sidebar_open {
                "close the sidebar"
            } else {
                "open the sidebar"
            }
            .to_string(),
            detail: chord_label(p, &["B"], false),
            effect: run_cmd(["wall.sidebar", "toggle"], true),
        },
        RawAction {
            id: "settings".to_string(),
            label: if snap.settings_open {
                "close the settings"
            } else {
                "open the settings"
            }
            .to_string(),
            detail: chord_label(p, &[","], false),
            effect: run_cmd(["wall.settings", "toggle"], true),
        },
        RawAction {
            id: "snap".to_string(),
            label: if snap.snap {
                "stop snapping to the grid"
            } else {
                "snap to the grid"
            }
            .to_string(),
            detail: chord_label(p, &["'"], false),
            effect: run_cmd(["wall.snap", "toggle"], true),
        },
        RawAction {
            id: "grid".to_string(),
            label: if snap.grid_show {
                "hide the grid"
            } else {
                "show the grid"
            }
            .to_string(),
            detail: chord_label(p, &["'"], true),
            effect: run_cmd(["wall.grid", "toggle"], true),
        },
    ];

    if let Some(port) = snap.relay_port {
        out.push(RawAction {
            id: "token".to_string(),
            label: "set relay token…".to_string(),
            detail: format!(":{port}"),
            effect: PaletteEffect::EnterToken,
        });
    }

    if !snap.selection.is_empty() {
        out.push(RawAction {
            id: "group-sel".to_string(),
            label: format!("group the selection ({})", snap.selection.len()),
            detail: chord_label(p, &["G"], false),
            effect: run_cmd(["wall.group"], true),
        });
        for g in &snap.groups {
            let mut cmd = Vec::with_capacity(2 + snap.selection.len());
            cmd.push("wall.group.add".to_string());
            cmd.push(g.id.clone());
            cmd.extend(snap.selection.iter().cloned());
            out.push(RawAction {
                id: format!("add-sel:{}", g.id),
                label: format!("add the selection to \"{}\"", g.name),
                detail: g.id.clone(),
                effect: PaletteEffect::Run { cmd, close: true },
            });
        }
    }

    if let Some(foc) = focused {
        out.push(RawAction {
            id: "rename".to_string(),
            label: format!("rename \"{}\"…", foc.name),
            detail: "focused terminal".to_string(),
            effect: PaletteEffect::EnterRename {
                kind: RenameKind::Terminal,
                id: foc.id.clone(),
                initial_query: if foc.named {
                    foc.name.clone()
                } else {
                    String::new()
                },
            },
        });
        out.push(RawAction {
            id: "hide".to_string(),
            label: format!("hide \"{}\"", foc.name),
            detail: chord_label(p, &["-"], false),
            effect: run_cmd(["wall.hide", foc.id.as_str()], true),
        });
        out.push(RawAction {
            id: "close".to_string(),
            label: format!("close \"{}\" (kills its shell)", foc.name),
            detail: format!("hold {}", chord_label(p, &["X"], false)),
            effect: run_cmd(["wall.close", foc.id.as_str()], true),
        });

        if fg.is_none() && snap.selection.is_empty() {
            out.push(RawAction {
                id: "group-one".to_string(),
                label: format!("group \"{}\"", foc.name),
                detail: chord_label(p, &["G"], false),
                effect: run_cmd(["wall.group", foc.id.as_str()], true),
            });
        }
        if fg.is_none() {
            for g in &snap.groups {
                out.push(RawAction {
                    id: format!("add:{}", g.id),
                    label: format!("add \"{}\" to \"{}\"", foc.name, g.name),
                    detail: g.id.clone(),
                    effect: run_cmd(
                        ["wall.group.add", g.id.as_str(), foc.id.as_str()],
                        true,
                    ),
                });
            }
        }
        if let Some(group) = fg {
            out.push(RawAction {
                id: "leave".to_string(),
                label: format!("take \"{}\" out of \"{}\"", foc.name, group.name),
                detail: group.id.clone(),
                effect: run_cmd(["wall.group.remove", foc.id.as_str()], true),
            });
            out.push(RawAction {
                id: "rename-group".to_string(),
                label: format!("rename group \"{}\"…", group.name),
                detail: group.id.clone(),
                effect: PaletteEffect::EnterRename {
                    kind: RenameKind::Group,
                    id: group.id.clone(),
                    initial_query: if group.named {
                        group.name.clone()
                    } else {
                        String::new()
                    },
                },
            });
            if group.layout == GroupLayout::Tile {
                out.push(RawAction {
                    id: "cols+".to_string(),
                    label: format!(
                        "\"{}\": tile in {} columns",
                        group.name,
                        group.columns + 1
                    ),
                    detail: group.id.clone(),
                    effect: run_cmd(["wall.group.columns", group.id.as_str(), "+1"], false),
                });
                if group.columns > 1 {
                    let dec = group.columns - 1;
                    out.push(RawAction {
                        id: "cols-".to_string(),
                        label: format!(
                            "\"{}\": tile in {dec} column{}",
                            group.name,
                            if dec == 1 { "" } else { "s" }
                        ),
                        detail: group.id.clone(),
                        effect: run_cmd(["wall.group.columns", group.id.as_str(), "-1"], false),
                    });
                }
            }
            let next_layout = if group.layout == GroupLayout::Tile {
                "freeform"
            } else {
                "tile"
            };
            out.push(RawAction {
                id: "layout".to_string(),
                label: format!("\"{}\": {next_layout}", group.name),
                detail: group.id.clone(),
                effect: run_cmd(["wall.group.layout", group.id.as_str(), next_layout], true),
            });
            out.push(RawAction {
                id: "ungroup".to_string(),
                label: format!("ungroup \"{}\"", group.name),
                detail: chord_label(p, &["G"], true),
                effect: run_cmd(["wall.ungroup", group.id.as_str()], true),
            });
        }
    }

    out
}

/// Build the visible palette rows for [`PaletteMode::Find`] (`Palette.jsx`).
///
/// - When `query` is empty, lists groups followed by their members (`depth = 1`),
///   then loose terminals (`depth = 0`), then live agents by host, then all
///   applicable actions.
/// - When `query` is non-empty, scores terminals (against `"name id group preview"`),
///   groups (against `"name"`), and agents (against `"target host status detail cwd"`),
///   stable-sorts matches by ascending gap score, and appends actions whose
///   `label` matches `query`.
pub fn palette_items(snap: &PaletteSnapshot, query: &str) -> Vec<PaletteItem> {
    let mut term_by_id: HashMap<&str, &PaletteTerminal> =
        HashMap::with_capacity(snap.terminals.len());
    let mut terminal_ids: HashSet<&str> = HashSet::with_capacity(snap.terminals.len());
    let mut attached_to: HashMap<&str, &str> = HashMap::new();
    for t in &snap.terminals {
        term_by_id.insert(t.id.as_str(), t);
        terminal_ids.insert(t.id.as_str());
        if let Some(att) = t.attached.as_deref() {
            if !att.is_empty() {
                attached_to.insert(att, t.id.as_str());
            }
        }
    }
    let group_by_id: HashMap<&str, &PaletteGroup> =
        snap.groups.iter().map(|g| (g.id.as_str(), g)).collect();

    let resolved_hosts = bus_hosts(&snap.bus);
    let mut agent_entries: Vec<(PaletteItem, String)> = Vec::new();
    for (hi, h) in resolved_hosts.iter().enumerate() {
        for a in &h.agents {
            let attachable = bus_attachable(a.attached);
            let on_wall = resolve_agent_on_wall(a, hi, &attached_to, &terminal_ids);
            let mut detail_parts = vec![a.status.word().to_string()];
            if let Some(tid) = &on_wall {
                detail_parts.push(format!("on {tid}"));
            } else if !attachable {
                detail_parts.push("no session".to_string());
            }
            if !a.detail.is_empty() {
                detail_parts.push(a.detail.clone());
            }
            let search_text = format!(
                "{} {} {} {} {}",
                a.target, h.host, a.status_raw, a.detail, a.cwd
            );
            let item = PaletteItem {
                key: format!("agent:{}", a.target),
                kind: PaletteItemKind::Agent {
                    target: a.target.clone(),
                    status: a.status,
                    attachable,
                    on_wall,
                },
                label: a.target.clone(),
                detail: detail_parts.join(" · "),
                depth: 0,
                hidden: false,
                dim: !attachable,
                score: 0,
            };
            agent_entries.push((item, search_text));
        }
    }

    let mut listed = Vec::new();
    if query.is_empty() {
        for g in &snap.groups {
            listed.push(make_group_item(g, 0));
            for id in &g.members {
                if let Some(&t) = term_by_id.get(id.as_str()) {
                    listed.push(make_terminal_item(t, 1, None, 0));
                }
            }
        }
        for t in &snap.terminals {
            if t.group.is_none() {
                listed.push(make_terminal_item(t, 0, None, 0));
            }
        }
        for (it, _) in agent_entries {
            listed.push(it);
        }
    } else {
        for t in &snap.terminals {
            let grp = t.group.as_deref().and_then(|gid| group_by_id.get(gid).copied());
            let hay = format!(
                "{} {} {} {}",
                t.name,
                t.id,
                grp.map_or("", |g| g.name.as_str()),
                t.preview
            );
            if let Some(s) = subsequence_score(query, &hay) {
                listed.push(make_terminal_item(
                    t,
                    0,
                    grp.map(|g| g.name.as_str()),
                    s,
                ));
            }
        }
        for g in &snap.groups {
            if let Some(s) = subsequence_score(query, &g.name) {
                listed.push(make_group_item(g, s));
            }
        }
        for (mut it, search_text) in agent_entries {
            if let Some(s) = subsequence_score(query, &search_text) {
                it.score = s;
                listed.push(it);
            }
        }
        listed.sort_by_key(|it| it.score);
    }

    let focused = snap
        .focused_id
        .as_deref()
        .and_then(|fid| term_by_id.get(fid).copied());
    let fg = focused
        .and_then(|f| f.group.as_deref())
        .and_then(|gid| group_by_id.get(gid).copied());

    for action in build_actions(snap, focused, fg) {
        if subsequence_score(query, &action.label).is_some() {
            listed.push(PaletteItem {
                key: format!("action:{}", action.id),
                kind: PaletteItemKind::Action {
                    id: action.id,
                    effect: action.effect,
                },
                label: action.label,
                detail: action.detail,
                depth: 0,
                hidden: false,
                dim: false,
                score: 0,
            });
        }
    }

    listed
}

/// Resolve what choosing `item` (click or `Enter` on a highlighted row) does
/// (`choose` in `wall/Palette.jsx`).
pub fn choose_item(item: &PaletteItem) -> PaletteEffect {
    match &item.kind {
        PaletteItemKind::Terminal { id } => run_cmd(["wall.reveal", id.as_str()], true),
        PaletteItemKind::Group { id } => run_cmd(["wall.group.frame", id.as_str()], true),
        PaletteItemKind::Agent { target, .. } => run_cmd(["wall.attach", target.as_str()], true),
        PaletteItemKind::Action { effect, .. } => effect.clone(),
    }
}

/// Stateful controller for the palette (`mode`, `query`, and `selected` index),
/// implementing `Palette.jsx`'s keyboard navigation, rename target tracking,
/// and `Enter` submission rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Palette {
    pub mode: PaletteMode,
    pub query: String,
    pub selected: usize,
}

impl Palette {
    /// Open the palette in `initial_mode` with an empty query and index `0`.
    pub fn new(initial_mode: PaletteMode) -> Self {
        Self {
            mode: initial_mode,
            query: String::new(),
            selected: 0,
        }
    }

    /// Reconcile `Rename` mode against `snap`: if the terminal or group being
    /// renamed disappeared while the palette was open, resets to `Find` mode
    /// with an empty query (`useEffect` in `Palette.jsx:62-64`), and clamps
    /// `selected` to the current item list length.
    pub fn sync(&mut self, snap: &PaletteSnapshot) {
        if let PaletteMode::Rename { kind, id } = &self.mode {
            let exists = match kind {
                RenameKind::Terminal => snap.terminals.iter().any(|t| &t.id == id),
                RenameKind::Group => snap.groups.iter().any(|g| &g.id == id),
            };
            if !exists {
                self.mode = PaletteMode::Find;
                self.query.clear();
                self.selected = 0;
            }
        }
        if self.mode == PaletteMode::Find {
            let len = palette_items(snap, &self.query).len();
            self.selected = self.selected.min(len.saturating_sub(1));
        }
    }

    /// Update the input box text (`onChange` in `Palette.jsx`: resets `selected` to `0`).
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.selected = 0;
    }

    /// Visible items for the current state (`empty` outside [`PaletteMode::Find`]).
    pub fn items(&self, snap: &PaletteSnapshot) -> Vec<PaletteItem> {
        if self.mode != PaletteMode::Find {
            return Vec::new();
        }
        palette_items(snap, &self.query)
    }

    /// Move the highlighted index down (`ArrowDown`).
    pub fn move_down(&mut self, item_count: usize) {
        if self.mode == PaletteMode::Find && item_count > 0 {
            self.selected = (self.selected + 1).min(item_count - 1);
        }
    }

    /// Move the highlighted index up (`ArrowUp`).
    pub fn move_up(&mut self) {
        if self.mode == PaletteMode::Find {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    /// Header caption above the text input (`None` in `Find` mode).
    pub fn caption(&self, snap: &PaletteSnapshot) -> Option<String> {
        match &self.mode {
            PaletteMode::Find => None,
            PaletteMode::Token => {
                Some("paste relay token or #token=… URL — Enter connects".to_string())
            }
            PaletteMode::Rename { kind, id } => {
                let exists = match kind {
                    RenameKind::Terminal => snap.terminals.iter().any(|t| &t.id == id),
                    RenameKind::Group => snap.groups.iter().any(|g| &g.id == id),
                };
                exists.then(|| {
                    let fallback = match kind {
                        RenameKind::Group => "\"group N\"",
                        RenameKind::Terminal => "the shell's title",
                    };
                    format!("rename {id} — empty uses {fallback}")
                })
            }
        }
    }

    /// Input placeholder string (`Palette.jsx:210`).
    pub fn placeholder(&self, snap: &PaletteSnapshot) -> String {
        match &self.mode {
            PaletteMode::Token => format!(
                "token or http://localhost:{}/#token=…",
                snap.relay_port.unwrap_or(8080)
            ),
            PaletteMode::Rename { kind, id } => {
                let name = match kind {
                    RenameKind::Terminal => snap
                        .terminals
                        .iter()
                        .find(|t| &t.id == id)
                        .map(|t| t.name.as_str()),
                    RenameKind::Group => snap
                        .groups
                        .iter()
                        .find(|g| &g.id == id)
                        .map(|g| g.name.as_str()),
                };
                name.unwrap_or("find a terminal, group or agent…").to_string()
            }
            PaletteMode::Find => "find a terminal, group or agent…".to_string(),
        }
    }

    /// Apply a [`PaletteEffect`] that transitions internal palette mode
    /// (`EnterRename` or `EnterToken`), returning the effect so the caller can
    /// dispatch [`PaletteEffect::Run`] commands and honour `close`.
    fn apply_effect(&mut self, effect: PaletteEffect) -> PaletteEffect {
        match &effect {
            PaletteEffect::EnterRename {
                kind,
                id,
                initial_query,
            } => {
                self.mode = PaletteMode::Rename {
                    kind: *kind,
                    id: id.clone(),
                };
                self.query = initial_query.clone();
                self.selected = 0;
            }
            PaletteEffect::EnterToken => {
                self.mode = PaletteMode::Token;
                self.query.clear();
                self.selected = 0;
            }
            PaletteEffect::Run { .. } | PaletteEffect::Noop => {}
        }
        effect
    }

    /// Choose a specific row index (`onMouseDown` on a list row).
    pub fn choose_at(&mut self, snap: &PaletteSnapshot, index: usize) -> PaletteEffect {
        let items = self.items(snap);
        let Some(item) = items.get(index) else {
            return PaletteEffect::Noop;
        };
        let effect = choose_item(item);
        self.apply_effect(effect)
    }

    /// Handle `Enter` in the current mode (`onKeyDown` in `Palette.jsx`).
    pub fn submit(&mut self, snap: &PaletteSnapshot) -> PaletteEffect {
        self.sync(snap);
        match &self.mode {
            PaletteMode::Token => {
                let val = self.query.trim().to_string();
                run_cmd(["wall.token", val.as_str()], true)
            }
            PaletteMode::Rename { kind, id } => {
                let verb = match kind {
                    RenameKind::Group => "wall.group.rename",
                    RenameKind::Terminal => "wall.rename",
                };
                let trimmed = self.query.trim();
                let mut cmd = vec![verb.to_string(), id.clone()];
                if !trimmed.is_empty() {
                    cmd.push(trimmed.to_string());
                }
                PaletteEffect::Run { cmd, close: true }
            }
            PaletteMode::Find => {
                if let Some(tok_val) = direct_token_input(&self.query) {
                    return run_cmd(["wall.token", tok_val.as_str()], true);
                }
                if is_bare_token_command(&self.query) {
                    return self.apply_effect(PaletteEffect::EnterToken);
                }
                let items = palette_items(snap, &self.query);
                let Some(item) = items.get(self.selected.min(items.len().saturating_sub(1))) else {
                    return PaletteEffect::Noop;
                };
                let effect = choose_item(item);
                self.apply_effect(effect)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::BusAgentInput;

    fn sample_snapshot() -> PaletteSnapshot {
        PaletteSnapshot {
            terminals: vec![
                PaletteTerminal {
                    id: "term-1".to_string(),
                    name: "server".to_string(),
                    named: true,
                    group: Some("g1".to_string()),
                    hidden: false,
                    preview: "Listening on :3000".to_string(),
                    attached: None,
                },
                PaletteTerminal {
                    id: "term-2".to_string(),
                    name: "worker".to_string(),
                    named: false,
                    group: Some("g1".to_string()),
                    hidden: true,
                    preview: "idle".to_string(),
                    attached: None,
                },
                PaletteTerminal {
                    id: "term-3".to_string(),
                    name: "term-3".to_string(),
                    named: false,
                    group: None,
                    hidden: false,
                    preview: "cargo glyph test".to_string(),
                    attached: None,
                },
            ],
            groups: vec![PaletteGroup {
                id: "g1".to_string(),
                name: "Services".to_string(),
                named: true,
                layout: GroupLayout::Tile,
                columns: 2,
                members: vec!["term-1".to_string(), "term-2".to_string()],
            }],
            focused_id: Some("term-1".to_string()),
            selection: Vec::new(),
            sidebar_open: false,
            settings_open: false,
            snap: true,
            grid_show: false,
            platform: Platform::Mac,
            relay_port: None,
            bus: BusSnapshot {
                host: "mac".to_string(),
                reason: String::new(),
                agents: vec![BusAgentInput {
                    name: "wall-lead".to_string(),
                    host: "mac".to_string(),
                    live: true,
                    status: "waiting".to_string(),
                    attached: Some(true),
                    detail: "reviewing".to_string(),
                    cwd: "/work".to_string(),
                    ..BusAgentInput::default()
                }],
            },
        }
    }

    #[test]
    fn subsequence_score_gaps_case_and_non_matches() {
        assert_eq!(subsequence_score("", "anything"), Some(0));
        assert_eq!(subsequence_score("srv", "server"), Some(1));
        assert_eq!(subsequence_score("SER", "my-server"), Some(0));
        assert_eq!(subsequence_score("ac", "abc"), Some(1));
        assert_eq!(subsequence_score("az", "abc"), None);
    }

    #[test]
    fn empty_query_lists_tree_hidden_marks_agents_and_context_actions() {
        let snap = sample_snapshot();
        let items = palette_items(&snap, "");

        // First 5 listed items: group g1 -> term-1 (depth 1) -> term-2 (depth 1, hidden) -> loose term-3 (depth 0) -> agent.
        assert_eq!(items[0].key, "group:g1");
        assert_eq!(items[0].display_label(), "▤ Services");
        assert_eq!(items[0].detail, "2 · tile · 2 cols");
        assert_eq!(items[0].depth, 0);

        assert_eq!(items[1].key, "terminal:term-1");
        assert_eq!(items[1].depth, 1);
        assert!(!items[1].hidden);
        assert_eq!(items[1].detail, "term-1 · Listening on :3000");

        assert_eq!(items[2].key, "terminal:term-2");
        assert_eq!(items[2].depth, 1);
        assert!(items[2].hidden);
        assert!(items[2].dim);
        assert_eq!(items[2].label_color(), "#6b7383");
        assert_eq!(items[2].detail, "hidden · term-2 · idle");

        assert_eq!(items[3].key, "terminal:term-3");
        assert_eq!(items[3].depth, 0);
        // name == id ("term-3"), so id is not repeated in detail.
        assert_eq!(items[3].detail, "cargo glyph test");

        assert_eq!(items[4].key, "agent:wall-lead@mac");
        assert_eq!(items[4].display_label(), "◆ wall-lead@mac");
        assert_eq!(items[4].detail, "waiting · reviewing");

        // Focused terminal is in tiled group g1 (2 columns), so cols+ and cols- actions exist and keep palette open!
        let cols_inc = items
            .iter()
            .find(|i| i.key == "action:cols+")
            .expect("cols+ action");
        assert_eq!(
            choose_item(cols_inc),
            PaletteEffect::Run {
                cmd: vec!["wall.group.columns".into(), "g1".into(), "+1".into()],
                close: false,
            }
        );
        let cols_dec = items
            .iter()
            .find(|i| i.key == "action:cols-")
            .expect("cols- action");
        assert_eq!(cols_dec.label, "\"Services\": tile in 1 column");
        assert_eq!(
            choose_item(cols_dec),
            PaletteEffect::Run {
                cmd: vec!["wall.group.columns".into(), "g1".into(), "-1".into()],
                close: false,
            }
        );
    }

    #[test]
    fn query_filters_and_ranks_by_gap_score_and_prepends_group_name() {
        let mut snap = sample_snapshot();
        // Add a terminal whose name has a gap match ("s...r...v") vs contiguous ("srv") in preview.
        snap.terminals.push(PaletteTerminal {
            id: "term-4".to_string(),
            name: "srv-box".to_string(),
            named: true,
            group: None,
            hidden: false,
            preview: String::new(),
            attached: None,
        });

        let items = palette_items(&snap, "srv");
        // "srv-box" has contiguous "srv" (score 0); "server" ("s-e-r-v", gap 1) and "Services" ("S-e-r-v", gap 1) follow.
        assert_eq!(items[0].key, "terminal:term-4");
        assert_eq!(items[0].score, 0);

        let term1 = items
            .iter()
            .find(|i| i.key == "terminal:term-1")
            .expect("term-1 matched");
        assert_eq!(term1.depth, 0);
        assert!(
            term1.detail.starts_with("in Services · "),
            "grouped terminal detail in search mode starts with 'in <group>': {}",
            term1.detail
        );
    }

    #[test]
    fn enter_runs_verbs_rename_flow_and_reconciles_deleted_target() {
        let mut snap = sample_snapshot();
        let mut pal = Palette::new(PaletteMode::Find);

        // Selecting group row frames it and closes.
        assert_eq!(
            pal.submit(&snap),
            PaletteEffect::Run {
                cmd: vec!["wall.group.frame".into(), "g1".into()],
                close: true,
            }
        );

        // Filter to rename focused terminal ("server", named = true -> initial_query = "server").
        pal.set_query("rename \"server\"");
        let effect = pal.submit(&snap);
        assert_eq!(
            effect,
            PaletteEffect::EnterRename {
                kind: RenameKind::Terminal,
                id: "term-1".to_string(),
                initial_query: "server".to_string(),
            }
        );
        assert_eq!(
            pal.mode,
            PaletteMode::Rename {
                kind: RenameKind::Terminal,
                id: "term-1".to_string(),
            }
        );
        assert_eq!(pal.query, "server");

        // Submitting a new trimmed name runs `wall.rename term-1 api`.
        pal.set_query("  api  ");
        assert_eq!(
            pal.submit(&snap),
            PaletteEffect::Run {
                cmd: vec!["wall.rename".into(), "term-1".into(), "api".into()],
                close: true,
            }
        );

        // Submitting an empty name omits the 3rd arg so the terminal reverts to shell title.
        pal.set_query("   ");
        assert_eq!(
            pal.submit(&snap),
            PaletteEffect::Run {
                cmd: vec!["wall.rename".into(), "term-1".into()],
                close: true,
            }
        );

        // If term-1 disappears while in rename mode, sync drops back to Find mode.
        snap.terminals.retain(|t| t.id != "term-1");
        pal.sync(&snap);
        assert_eq!(pal.mode, PaletteMode::Find);
        assert_eq!(pal.query, "");
    }

    #[test]
    fn token_shortcuts_match_palette_jsx_rules() {
        assert_eq!(
            direct_token_input("  wall.token abc-123  "),
            Some("abc-123".to_string())
        );
        assert_eq!(
            direct_token_input("WALL token secret"),
            Some("secret".to_string())
        );
        assert_eq!(
            direct_token_input("http://localhost:8080/#token=xyz"),
            Some("http://localhost:8080/#token=xyz".to_string())
        );
        assert_eq!(
            direct_token_input("http://localhost:8080/#foo=1&token=xyz"),
            Some("http://localhost:8080/#foo=1&token=xyz".to_string())
        );
        assert_eq!(direct_token_input("wall.token"), None);
        assert_eq!(direct_token_input("server"), None);

        assert!(is_bare_token_command("wall.token"));
        assert!(is_bare_token_command("  wall   TOKEN "));
        assert!(!is_bare_token_command("wall.token abc"));
    }
}
