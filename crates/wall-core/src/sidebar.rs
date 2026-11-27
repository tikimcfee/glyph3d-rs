//! The sidebar's pure model, and the pure data rules of `wall/WallBus.js`:
//!
//! - **Bus model (`wall/WallBus.js`)**: [`AgentStatus`] (`BUS_STATUSES` /
//!   `busStatus`), [`bus_target`], [`is_bus_target`], [`local_bus_name`],
//!   [`gomux_screen_id`], [`bus_attachable`], [`bus_hosts`], and
//!   [`resolve_agent_on_wall`].
//! - **Sidebar list**: [`build_sidebar`], turning the wall's terminals,
//!   groups, folded-group set, focused terminal id, gomuxai bus snapshot and
//!   [`SidebarMode`] into ONE list of plain rows ([`SidebarRow`]): a terminal
//!   and the agent it shows are one [`EntryRow`], an agent with no terminal on
//!   the wall is a row of its own, and the list is sectioned by group or by
//!   host. Each row carries the exact wall verb (`Vec<String>`) it or one of
//!   its tool buttons runs.
//!
//! Until 2026-11-27 this was a port of `wall/Sidebar.jsx`: two lists, the
//! terminals under their groups and then the agents under their hosts, an
//! agent already on the wall listed twice (its terminal, and an agent row
//! tagged "on wall").

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::config::GroupLayout;

// ---------------------------------------------------------------------------
// WallBus pure model (`wall/WallBus.js`)
// ---------------------------------------------------------------------------

/// Status of a gomuxai agent (`BUS_STATUSES` / `busStatus` in `wall/WallBus.js`),
/// ordered by urgency rank (`0` most urgent ..= `5` ended).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    /// Blocked on a permission or input prompt (`rank = 0`, `#f2b45a`).
    Waiting,
    /// Errored (`rank = 1`, `#ef6f6c`).
    Error,
    /// Actively running a turn (`rank = 2`, `#7fb0ff`).
    Processing,
    /// Ready and waiting for instructions (`rank = 3`, `#8ef0b8`).
    Idle,
    /// Unrecognized or unreported status (`rank = 4`, `#8b95a7`).
    #[default]
    Unknown,
    /// Finished / exited (`rank = 5`, `#5d6574`).
    Ended,
}

impl AgentStatus {
    /// Parse a raw status word from gomuxai (`busStatus` in `wall/WallBus.js`).
    /// Unknown or empty strings map to [`AgentStatus::Unknown`].
    pub fn parse(status: &str) -> Self {
        match status {
            "waiting" => Self::Waiting,
            "error" => Self::Error,
            "processing" => Self::Processing,
            "idle" => Self::Idle,
            "ended" => Self::Ended,
            _ => Self::Unknown,
        }
    }

    /// Sort priority within a host (`0` = top of list).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Error => 1,
            Self::Processing => 2,
            Self::Idle => 3,
            Self::Unknown => 4,
            Self::Ended => 5,
        }
    }

    /// Badge and status-dot hex colour from `BUS_STATUSES`.
    pub const fn color(self) -> &'static str {
        match self {
            Self::Waiting => "#f2b45a",
            Self::Error => "#ef6f6c",
            Self::Processing => "#7fb0ff",
            Self::Idle => "#8ef0b8",
            Self::Unknown => "#8b95a7",
            Self::Ended => "#5d6574",
        }
    }

    /// Canonical lowercase status label (`word` in `BUS_STATUSES`).
    pub const fn word(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Error => "error",
            Self::Processing => "processing",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
            Self::Ended => "ended",
        }
    }
}

fn is_bus_name_segment(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `target` is a safe `name` or `name@host` token (`isBusTarget` in
/// `wall/WallBus.js`: `^[A-Za-z0-9][A-Za-z0-9_-]*(@[A-Za-z0-9][A-Za-z0-9_-]*)?$`).
pub fn is_bus_target(target: &str) -> bool {
    match target.split_once('@') {
        None => is_bus_name_segment(target),
        Some((name, host)) => {
            !host.contains('@') && is_bus_name_segment(name) && is_bus_name_segment(host)
        }
    }
}

/// Canonical `name` or `name@host` address (`busTarget` in `wall/WallBus.js`).
pub fn bus_target(name: &str, host: &str) -> String {
    if host.is_empty() {
        name.to_string()
    } else {
        format!("{name}@{host}")
    }
}

/// The agent's bare name when `target` refers to this daemon's host (`name`
/// with no `@`, or `name@<snap_host>` when `snap_host` is non-empty), else
/// `None` (`localBusName` in `wall/WallBus.js`, which returns `""` on mismatch).
pub fn local_bus_name<'a>(target: &'a str, snap_host: &str) -> Option<&'a str> {
    if !is_bus_target(target) {
        return None;
    }
    match target.split_once('@') {
        None => Some(target),
        Some((name, host)) => (!snap_host.is_empty() && host == snap_host).then_some(name),
    }
}

/// The `g3d-termd` terminal id `gomuxai`'s `termd` backend spawns an agent in:
/// `"gx-"` followed by `name` sanitized (`[^A-Za-z0-9._-]` -> `'_'`) and
/// truncated to 61 characters (`64` bytes total max; `gomuxScreenId` in
/// `wall/WallBus.js`).
pub fn gomux_screen_id(name: &str) -> String {
    let mut out = String::with_capacity(3 + name.len().min(61));
    out.push_str("gx-");
    for ch in name.chars().take(61) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Whether `agent attach` can open a terminal for an agent (`busAttachable` in
/// `wall/WallBus.js`): `false` only when `attached` is explicitly `Some(false)`
/// (`None` from a remote host that did not report session attachment stays
/// attachable).
pub const fn bus_attachable(attached: Option<bool>) -> bool {
    !matches!(attached, Some(false))
}

/// One raw agent record in a gomuxai fleet snapshot (`snap.agents[i]`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusAgentInput {
    pub name: String,
    pub host: String,
    pub live: bool,
    pub status: String,
    pub end_reason: String,
    /// `Some(true)` if backed by a live session, `Some(false)` if not (e.g. a
    /// subagent without its own PTY), `None` if unknown (e.g. remote host).
    pub attached: Option<bool>,
    pub detail: String,
    pub cwd: String,
}

/// Raw gomuxai fleet snapshot (`wall.busSnapshot()` / `/api/agents`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BusSnapshot {
    /// Local host name reported by the daemon (`""` when unnamed).
    pub host: String,
    /// Diagnostic reason when gomuxai is unavailable (e.g. `"agent CLI not found"`).
    pub reason: String,
    /// Raw agent list (both live and ended).
    pub agents: Vec<BusAgentInput>,
}

/// Resolved live agent entry inside a [`BusHost`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusAgent {
    pub name: String,
    pub host: String,
    /// Canonical `name` or `name@host` address.
    pub target: String,
    /// Parsed status enum.
    pub status: AgentStatus,
    /// Raw status string as received (preserved for palette search matching).
    pub status_raw: String,
    pub end_reason: String,
    pub attached: Option<bool>,
    pub detail: String,
    pub cwd: String,
}

/// Live agents on one host, sorted by urgency rank then name (`busHosts` in
/// `wall/WallBus.js`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusHost {
    pub host: String,
    pub agents: Vec<BusAgent>,
}

/// Group live agents by host, sort each host's agents by status urgency rank
/// then lexicographical name, and order the local host (`""` or `snap.host`)
/// ahead of remote hosts (`busHosts` in `wall/WallBus.js`).
pub fn bus_hosts(snap: &BusSnapshot) -> Vec<BusHost> {
    let mut by_host: HashMap<String, Vec<BusAgent>> = HashMap::new();
    for a in &snap.agents {
        if !a.live || a.name.is_empty() {
            continue;
        }
        let host = a.host.clone();
        let target = bus_target(&a.name, &host);
        by_host.entry(host.clone()).or_default().push(BusAgent {
            name: a.name.clone(),
            host,
            target,
            status: AgentStatus::parse(&a.status),
            status_raw: a.status.clone(),
            end_reason: a.end_reason.clone(),
            attached: a.attached,
            detail: a.detail.clone(),
            cwd: a.cwd.clone(),
        });
    }

    let mut hosts: Vec<BusHost> = by_host
        .into_iter()
        .map(|(host, mut agents)| {
            agents.sort_by(|a, b| {
                a.status
                    .rank()
                    .cmp(&b.status.rank())
                    .then_with(|| a.name.cmp(&b.name))
            });
            BusHost { host, agents }
        })
        .collect();

    hosts.sort_by(|a, b| {
        let a_local = u8::from(!(a.host.is_empty() || a.host == snap.host));
        let b_local = u8::from(!(b.host.is_empty() || b.host == snap.host));
        a_local.cmp(&b_local).then_with(|| a.host.cmp(&b.host))
    });
    hosts
}

/// Resolve which wall terminal (if any) already shows `agent`, matching the
/// exact lookup shared by `Sidebar.jsx` and `Palette.jsx`:
///
/// 1. A terminal whose `attached` target equals `agent.target`.
/// 2. On the first host (`host_index == 0`, the local host when present):
///    a terminal whose `attached` target equals `agent.name`, else a terminal
///    whose id equals `gomux_screen_id(&agent.name)`.
pub fn resolve_agent_on_wall(
    agent: &BusAgent,
    host_index: usize,
    attached_to: &HashMap<&str, &str>,
    terminal_ids: &HashSet<&str>,
) -> Option<String> {
    if let Some(&tid) = attached_to.get(agent.target.as_str()) {
        return Some(tid.to_string());
    }
    if host_index == 0 {
        if let Some(&tid) = attached_to.get(agent.name.as_str()) {
            return Some(tid.to_string());
        }
        let gx = gomux_screen_id(&agent.name);
        if terminal_ids.contains(gx.as_str()) {
            return Some(gx);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Sidebar pure model: one list of terminals and agents (2026-11-27)
// ---------------------------------------------------------------------------

/// How the sidebar sections its one list (`wall.sidebar mode group|host`,
/// persisted with the sidebar's placement).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarMode {
    /// Each group with its members, then a `no group` section.
    #[default]
    Group,
    /// Each host with its rows, sorted by group.
    Host,
}

impl SidebarMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Host => "host",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "group" => Some(Self::Group),
            "host" => Some(Self::Host),
            _ => None,
        }
    }

    pub const fn other(self) -> Self {
        match self {
            Self::Group => Self::Host,
            Self::Host => Self::Group,
        }
    }

    /// The verb that selects this mode (`["wall.sidebar", "mode", <mode>]`).
    pub fn verb(self) -> Vec<String> {
        vec![
            "wall.sidebar".to_string(),
            "mode".to_string(),
            self.as_str().to_string(),
        ]
    }
}

/// Group chip colours. Every one keeps at least 3:1 contrast against both a
/// dark panel (`#0c0e14`) and a light one (`#f4f5f7`), which
/// `group_colors_read_on_dark_and_light_panels` holds them to. Neighbours in
/// this order differ in hue, so groups made one after another look different.
pub const GROUP_COLORS: [&str; 10] = [
    "#4a88ea", // blue
    "#d0701f", // orange
    "#379a55", // green
    "#c964c4", // magenta
    "#1c9690", // teal
    "#e0605c", // red
    "#8f7be8", // violet
    "#b08218", // amber
    "#e0609a", // pink
    "#2a90c0", // cyan
];

/// A group's colour, a function of its id alone so it survives restarts and
/// renames. The canvas draws every group in one style and stores no colour of
/// its own (2026-11-27); if it ever does, that colour belongs here instead.
///
/// The wall names its groups `g-1`, `g-2`, ...: those take the palette in
/// order, so two groups made in a row never share a colour. Any other id is
/// hashed (FNV-1a).
pub fn group_color(group_id: &str) -> &'static str {
    let n = GROUP_COLORS.len();
    let index = match group_id
        .strip_prefix("g-")
        .and_then(|digits| digits.parse::<usize>().ok())
    {
        Some(number) => (number % n + n - 1) % n,
        None => {
            let mut hash: u32 = 0x811c_9dc5;
            for byte in group_id.bytes() {
                hash ^= u32::from(byte);
                hash = hash.wrapping_mul(0x0100_0193);
            }
            hash as usize % n
        }
    };
    GROUP_COLORS[index]
}

/// Terminal snapshot fields read by [`build_sidebar`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarTerminal {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    pub hidden: bool,
    /// Agent target this terminal is attached to (e.g. `"lead@box"`), if any.
    pub attached: Option<String>,
    /// Whether the attached Agent View card is currently open (`!!t.agentView?.open`).
    pub agent_view_open: bool,
}

/// Group snapshot fields read by [`build_sidebar`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarGroup {
    pub id: String,
    pub name: String,
    pub layout: GroupLayout,
    pub columns: usize,
    pub members: Vec<String>,
}

/// Plain-data input to [`build_sidebar`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarInput {
    pub terminals: Vec<SidebarTerminal>,
    pub groups: Vec<SidebarGroup>,
    pub focused_id: Option<String>,
    /// Group ids folded in the sidebar (`state.sidebar.collapsed`).
    pub collapsed_groups: BTreeSet<String>,
    pub bus: BusSnapshot,
    pub mode: SidebarMode,
}

/// How many of a section's rows are on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCount {
    /// Raw host key (`""` for an unnamed local host).
    pub host: String,
    /// `"this host"` when `host` is empty, else `host`.
    pub label: String,
    pub count: usize,
}

/// `"laptop-2 2, workstation 1"`: a section's rows by host, this machine
/// first (the total alone when it has no rows).
fn count_short(total: usize, hosts: &[HostCount]) -> String {
    if hosts.is_empty() {
        return total.to_string();
    }
    let by_host: Vec<String> = hosts
        .iter()
        .map(|h| format!("{} {}", h.label, h.count))
        .collect();
    by_host.join(", ")
}

/// `"3 (laptop-2 2, workstation 1)"`: a section's row count, then
/// [`count_short`].
fn count_label(total: usize, hosts: &[HostCount]) -> String {
    if hosts.is_empty() {
        return total.to_string();
    }
    format!("{total} ({})", count_short(total, hosts))
}

/// A group header in group mode (`GroupRow` in `Sidebar.jsx`), foldable, with
/// the group's tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRow {
    pub id: String,
    pub name: String,
    /// The group's chip colour, `#rrggbb` ([`group_color`]).
    pub color: &'static str,
    /// Members that are terminals on the wall.
    pub member_count: usize,
    /// The members by host, this machine first.
    pub hosts: Vec<HostCount>,
    /// `"3 (laptop-2 2, workstation 1)"`, drawn after the name.
    pub count_label: String,
    /// `"laptop-2 2, workstation 1"`: the same counts without their total,
    /// for a header too narrow for `count_label`.
    pub count_short: String,
    pub collapsed: bool,
    pub layout: GroupLayout,
    pub columns: usize,
    /// Text of the layout toggle button (`"tile"` when tiled, `"free"` otherwise).
    pub layout_badge: &'static str,
    /// Clicking the group row frames the group (`["wall.group.frame", id]`).
    pub click_cmd: Vec<String>,
    /// Fold/unfold chevron (`["wall.sidebar.collapse", id]`).
    pub collapse_cmd: Vec<String>,
    /// `"−"` button when `layout == GroupLayout::Tile` (`["wall.group.columns", id, "-1"]`).
    pub cols_dec_cmd: Option<Vec<String>>,
    /// `"+"` button when `layout == GroupLayout::Tile` (`["wall.group.columns", id, "+1"]`).
    pub cols_inc_cmd: Option<Vec<String>>,
    /// Layout toggle button (`["wall.group.layout", id, "freeform" | "tile"]`).
    pub toggle_layout_cmd: Vec<String>,
    /// Ungroup button (`["wall.ungroup", id]`).
    pub ungroup_cmd: Vec<String>,
}

/// Which plain (unfoldable) section a [`SectionRow`] heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Group mode: the rows in no group, unattached agents included.
    NoGroup,
    /// Host mode: one host's rows.
    Host,
}

/// A plain section header: `no group` in group mode, a host in host mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionRow {
    pub kind: SectionKind,
    /// The host key for [`SectionKind::Host`] (`""` for an unnamed local host).
    pub host: String,
    /// `"no group"`, or the host's label.
    pub label: String,
    pub count: usize,
    /// [`SectionKind::NoGroup`]: the rows by host. Empty for a host section.
    pub hosts: Vec<HostCount>,
    /// `"4 (laptop-2 3, workstation 1)"` for `no group`, `"5"` for a host.
    pub count_label: String,
    /// `"laptop-2 3, workstation 1"` for `no group` (see
    /// [`GroupRow::count_short`]), `"5"` for a host.
    pub count_short: String,
    /// [`SectionKind::Host`]: whether the host is this machine.
    pub local: bool,
}

/// The group a row belongs to, as a coloured chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupChip {
    pub id: String,
    pub name: String,
    /// `#rrggbb` ([`group_color`]).
    pub color: &'static str,
    /// Host mode: the chip carries the group's name, since no header above
    /// the row does. Group mode: the colour alone.
    pub show_name: bool,
}

/// The terminal half of a row: its id and tool verbs (`TermRow` in `Sidebar.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalPart {
    pub id: String,
    pub in_group: bool,
    /// Whether the Agent View toggle button is highlighted (`on`).
    pub agent_view_open: bool,
    /// Agent View toggle button (`["wall.agent", id, "toggle"]`).
    pub agent_toggle_cmd: Vec<String>,
    /// Leave-group button when `in_group` (`["wall.group.remove", id]`).
    pub leave_group_cmd: Option<Vec<String>>,
    /// Show/hide button (`["wall.show", id]` if hidden, else `["wall.hide", id]`).
    pub visibility_cmd: Vec<String>,
    /// Hold-to-close button (`["wall.close", id]`).
    pub close_cmd: Vec<String>,
}

/// The agent half of a row: who it is and how it is doing (`AgentRow` in `Sidebar.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPart {
    pub name: String,
    /// Canonical `name` or `name@host` address.
    pub target: String,
    pub status: AgentStatus,
    /// Raw status attribute (`data-status`: `status_raw` if non-empty, else `"unknown"`).
    pub data_status: String,
    pub attachable: bool,
    /// The terminal on the wall that shows this agent, if any: the row's own
    /// terminal for a merged row. The panel fills the status dot for an agent
    /// on the wall and leaves it hollow for one that is not.
    pub on_wall: Option<String>,
}

/// One row of the list: a terminal, a terminal with the agent it shows merged
/// in, or an agent with no terminal on the wall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRow {
    pub name: String,
    /// `Some` for a terminal on the wall.
    pub terminal: Option<TerminalPart>,
    /// `Some` for an agent, merged into its terminal's row or on a row of its own.
    pub agent: Option<AgentPart>,
    /// The row's host key: this machine for a plain terminal, the agent's
    /// host for an attach terminal or an agent (`""` for an unnamed local host).
    pub host: String,
    /// `"this host"` when `host` is empty, else `host`.
    pub host_label: String,
    /// Whether `host` is this machine.
    pub local: bool,
    /// Group mode: the host's label on a row from another machine (a host
    /// section says it in host mode).
    pub host_tag: Option<String>,
    pub chip: Option<GroupChip>,
    /// Whether the terminal is hidden on the canvas (dims the row, shows `"hidden"`).
    pub hidden: bool,
    /// Whether the row's terminal is `focused_id`.
    pub focused: bool,
    /// Whether a click does something: always for a terminal, and for an
    /// agent when it can be attached (or is on the wall).
    pub interactive: bool,
    /// Multi-line hover text: the agent's target, status, detail and cwd and
    /// where it is on the wall. Empty for a plain terminal.
    pub tooltip: String,
    /// `["wall.reveal", id]` for a terminal, `["wall.attach", target]` for an
    /// agent with none.
    pub click_cmd: Vec<String>,
}

impl EntryRow {
    /// Whether this is an agent with no terminal row of its own.
    pub fn is_agent_only(&self) -> bool {
        self.terminal.is_none()
    }
}

/// One row of the sidebar's list, in drawing order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    /// Group mode: a foldable group header.
    Group(GroupRow),
    /// `no group` (group mode) or a host (host mode).
    Section(SectionRow),
    /// A terminal or an agent under the header before it.
    Entry(EntryRow),
}

impl SidebarRow {
    /// A compact name for the row, for `app.where` and tests: `g:<group id>`,
    /// `s:none`, `h:<host>` (`h:local` for an unnamed local host), `t:<terminal
    /// id>` (`t:<id>+<agent target>` when an agent is merged in) and
    /// `a:<agent target>`.
    pub fn token(&self) -> String {
        match self {
            Self::Group(g) => format!("g:{}", g.id),
            Self::Section(s) => match s.kind {
                SectionKind::NoGroup => "s:none".to_string(),
                SectionKind::Host if s.host.is_empty() => "h:local".to_string(),
                SectionKind::Host => format!("h:{}", s.host),
            },
            Self::Entry(e) => match (&e.terminal, &e.agent) {
                (Some(t), Some(a)) => format!("t:{}+{}", t.id, a.target),
                (Some(t), None) => format!("t:{}", t.id),
                (None, Some(a)) => format!("a:{}", a.target),
                (None, None) => "?".to_string(),
            },
        }
    }
}

/// Empty-state row shown when no live agents are listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsEmptyNotice {
    /// `"no gomuxai on this relay"` when `reason` is present, else `"no live agents"`.
    pub text: &'static str,
    /// Hover tooltip reason, if any.
    pub reason: Option<String>,
}

/// Complete view-model output of [`build_sidebar`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarModel {
    pub mode: SidebarMode,
    /// Every header and row, in drawing order. A folded group's members are
    /// left out.
    pub rows: Vec<SidebarRow>,
    /// True when `input.terminals` is empty (renders the `"no terminals"` placeholder row).
    pub terminals_empty: bool,
    /// Set when the bus lists no live agent.
    pub agents_empty: Option<AgentsEmptyNotice>,
}

impl SidebarModel {
    /// Every row's [`SidebarRow::token`], in order.
    pub fn tokens(&self) -> Vec<String> {
        self.rows.iter().map(SidebarRow::token).collect()
    }
}

fn host_label(host: &str) -> String {
    if host.is_empty() {
        "this host".to_string()
    } else {
        host.to_string()
    }
}

fn build_terminal_part(t: &SidebarTerminal, in_group: bool) -> TerminalPart {
    TerminalPart {
        id: t.id.clone(),
        in_group,
        agent_view_open: t.agent_view_open,
        agent_toggle_cmd: vec![
            "wall.agent".to_string(),
            t.id.clone(),
            "toggle".to_string(),
        ],
        leave_group_cmd: in_group.then(|| vec!["wall.group.remove".to_string(), t.id.clone()]),
        visibility_cmd: vec![
            if t.hidden { "wall.show" } else { "wall.hide" }.to_string(),
            t.id.clone(),
        ],
        close_cmd: vec!["wall.close".to_string(), t.id.clone()],
    }
}

fn build_group_row(g: &SidebarGroup, collapsed: bool, hosts: Vec<HostCount>) -> GroupRow {
    let tiled = g.layout == GroupLayout::Tile;
    let member_count = hosts.iter().map(|h| h.count).sum();
    GroupRow {
        id: g.id.clone(),
        name: g.name.clone(),
        color: group_color(&g.id),
        member_count,
        count_label: count_label(member_count, &hosts),
        count_short: count_short(member_count, &hosts),
        hosts,
        collapsed,
        layout: g.layout,
        columns: g.columns,
        layout_badge: if tiled { "tile" } else { "free" },
        click_cmd: vec!["wall.group.frame".to_string(), g.id.clone()],
        collapse_cmd: vec!["wall.sidebar.collapse".to_string(), g.id.clone()],
        cols_dec_cmd: tiled.then(|| {
            vec![
                "wall.group.columns".to_string(),
                g.id.clone(),
                "-1".to_string(),
            ]
        }),
        cols_inc_cmd: tiled.then(|| {
            vec![
                "wall.group.columns".to_string(),
                g.id.clone(),
                "+1".to_string(),
            ]
        }),
        toggle_layout_cmd: vec![
            "wall.group.layout".to_string(),
            g.id.clone(),
            if tiled { "freeform" } else { "tile" }.to_string(),
        ],
        ungroup_cmd: vec!["wall.ungroup".to_string(), g.id.clone()],
    }
}

/// The agent's hover text; `own_terminal` is the terminal row it is merged into.
fn build_agent_tooltip(
    a: &BusAgent,
    attachable: bool,
    on_wall: Option<&str>,
    own_terminal: bool,
) -> String {
    let status_line = if a.end_reason.is_empty() {
        a.status.word().to_string()
    } else {
        format!("{} ({})", a.status.word(), a.end_reason)
    };
    let footer = match on_wall {
        Some(tid) if own_terminal => format!("on the wall as {tid}"),
        Some(tid) => format!("attached in {tid}"),
        None if attachable => "click to attach".to_string(),
        None => "no live session to attach to".to_string(),
    };
    let mut lines = Vec::with_capacity(5);
    for candidate in [
        a.target.as_str(),
        status_line.as_str(),
        a.detail.as_str(),
        a.cwd.as_str(),
        footer.as_str(),
    ] {
        if !candidate.is_empty() {
            lines.push(candidate);
        }
    }
    lines.join("\n")
}

fn build_agent_part(a: &BusAgent, on_wall: Option<&str>) -> AgentPart {
    AgentPart {
        name: a.name.clone(),
        target: a.target.clone(),
        status: a.status,
        data_status: if a.status_raw.is_empty() {
            "unknown".to_string()
        } else {
            a.status_raw.clone()
        },
        attachable: bus_attachable(a.attached),
        on_wall: on_wall.map(str::to_string),
    }
}

/// A row before sectioning, with what the two modes sort it by.
struct Draft {
    row: EntryRow,
    /// Index into `input.groups`, `NO_GROUP` for a row in none.
    group: usize,
    /// Position in its group's member list (the canvas's tile order).
    member: usize,
    /// Terminals in wall order, then agents in [`bus_hosts`] order.
    seq: usize,
}

const NO_GROUP: usize = usize::MAX;

/// This machine first, then the other hosts by name.
fn host_order(d: &Draft) -> (bool, &str) {
    (!d.row.local, d.row.host.as_str())
}

/// The rows' hosts with their counts, in [`host_order`]. `drafts` is sorted
/// by host already.
fn host_counts(drafts: &[&Draft]) -> Vec<HostCount> {
    let mut out: Vec<HostCount> = Vec::new();
    for d in drafts {
        match out.last_mut() {
            Some(last) if last.host == d.row.host => last.count += 1,
            _ => out.push(HostCount {
                host: d.row.host.clone(),
                label: d.row.host_label.clone(),
                count: 1,
            }),
        }
    }
    out
}

/// Build the sidebar's one list from plain wall state: every terminal on the
/// wall and every live agent, each agent merged into the terminal that shows
/// it ([`resolve_agent_on_wall`]) when there is one, sectioned by
/// `input.mode`.
///
/// - **Group mode**: each group (its header foldable, with the group's
///   tools) and its members sorted by host, then a `no group` section sorted
///   by host, agents with no terminal included. Both headers count their
///   rows by host.
/// - **Host mode**: each host with a count, this machine first, its rows
///   sorted by group (the chip names it), rows in no group last.
///
/// Within one host and group, terminals keep the canvas's order (a group's
/// member order, else the wall's), and agents with no terminal follow them
/// by urgency then name ([`bus_hosts`]).
pub fn build_sidebar(input: &SidebarInput) -> SidebarModel {
    let mut terminal_ids: HashSet<&str> = HashSet::with_capacity(input.terminals.len());
    let mut attached_to: HashMap<&str, &str> = HashMap::new();
    for t in &input.terminals {
        terminal_ids.insert(t.id.as_str());
        if let Some(att) = t.attached.as_deref() {
            if !att.is_empty() {
                attached_to.insert(att, t.id.as_str());
            }
        }
    }

    // An agent host of "" is this machine, as `bus_hosts` sorts it.
    let local_host = input.bus.host.as_str();
    let norm_host = |host: &str| -> String {
        if host.is_empty() {
            local_host.to_string()
        } else {
            host.to_string()
        }
    };

    // Each live agent goes to the terminal that shows it, the first agent to
    // claim a terminal winning it, or onto a row of its own.
    let resolved_hosts = bus_hosts(&input.bus);
    let mut merged: HashMap<String, &BusAgent> = HashMap::new();
    let mut loose: Vec<(&BusAgent, Option<String>)> = Vec::new();
    for (hi, h) in resolved_hosts.iter().enumerate() {
        for a in &h.agents {
            match resolve_agent_on_wall(a, hi, &attached_to, &terminal_ids) {
                Some(tid) if !merged.contains_key(&tid) => {
                    merged.insert(tid, a);
                }
                on_wall => loose.push((a, on_wall)),
            }
        }
    }

    let group_index: HashMap<&str, usize> = input
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.id.as_str(), i))
        .collect();
    let focused = input.focused_id.as_deref();
    let by_group = input.mode == SidebarMode::Group;

    let mut drafts: Vec<Draft> = Vec::with_capacity(input.terminals.len() + loose.len());
    for (seq, t) in input.terminals.iter().enumerate() {
        let group = t
            .group
            .as_deref()
            .and_then(|gid| group_index.get(gid).copied());
        let member = group
            .and_then(|gi| input.groups[gi].members.iter().position(|m| *m == t.id))
            .unwrap_or(usize::MAX);
        let agent = merged.get(t.id.as_str()).copied();
        let host = match (agent, t.attached.as_deref()) {
            (Some(a), _) => norm_host(&a.host),
            (None, Some(target)) => norm_host(target.split_once('@').map_or("", |(_, h)| h)),
            (None, None) => local_host.to_string(),
        };
        let local = host == local_host;
        let host_label = host_label(&host);
        let tooltip = match (agent, t.attached.as_deref()) {
            (Some(a), _) => build_agent_tooltip(a, bus_attachable(a.attached), Some(&t.id), true),
            (None, Some(target)) if !target.is_empty() => format!("attached to {target}"),
            _ => String::new(),
        };
        drafts.push(Draft {
            row: EntryRow {
                name: t.name.clone(),
                terminal: Some(build_terminal_part(t, group.is_some())),
                agent: agent.map(|a| build_agent_part(a, Some(&t.id))),
                host_tag: (by_group && !local).then(|| host_label.clone()),
                host,
                host_label,
                local,
                chip: group.map(|gi| {
                    let g = &input.groups[gi];
                    GroupChip {
                        id: g.id.clone(),
                        name: g.name.clone(),
                        color: group_color(&g.id),
                        show_name: !by_group,
                    }
                }),
                hidden: t.hidden,
                focused: focused == Some(t.id.as_str()),
                interactive: true,
                tooltip,
                click_cmd: vec!["wall.reveal".to_string(), t.id.clone()],
            },
            group: group.unwrap_or(NO_GROUP),
            member,
            seq,
        });
    }
    for (k, (a, on_wall)) in loose.iter().enumerate() {
        let attachable = bus_attachable(a.attached);
        let host = norm_host(&a.host);
        let local = host == local_host;
        let host_label = host_label(&host);
        drafts.push(Draft {
            row: EntryRow {
                name: a.name.clone(),
                terminal: None,
                agent: Some(build_agent_part(a, on_wall.as_deref())),
                host_tag: (by_group && !local).then(|| host_label.clone()),
                host,
                host_label,
                local,
                chip: None,
                hidden: false,
                focused: on_wall.is_some() && on_wall.as_deref() == focused,
                interactive: attachable || on_wall.is_some(),
                tooltip: build_agent_tooltip(a, attachable, on_wall.as_deref(), false),
                click_cmd: vec!["wall.attach".to_string(), a.target.clone()],
            },
            group: NO_GROUP,
            member: usize::MAX,
            seq: input.terminals.len() + k,
        });
    }

    let mut rows = Vec::with_capacity(drafts.len() + input.groups.len() + 1);
    match input.mode {
        SidebarMode::Group => {
            for (gi, g) in input.groups.iter().enumerate() {
                let mut members: Vec<&Draft> = drafts.iter().filter(|d| d.group == gi).collect();
                members.sort_by(|a, b| {
                    host_order(a)
                        .cmp(&host_order(b))
                        .then(a.member.cmp(&b.member))
                        .then(a.seq.cmp(&b.seq))
                });
                let collapsed = input.collapsed_groups.contains(&g.id);
                rows.push(SidebarRow::Group(build_group_row(
                    g,
                    collapsed,
                    host_counts(&members),
                )));
                if !collapsed {
                    rows.extend(members.into_iter().map(|d| SidebarRow::Entry(d.row.clone())));
                }
            }
            let mut rest: Vec<&Draft> = drafts.iter().filter(|d| d.group == NO_GROUP).collect();
            rest.sort_by(|a, b| host_order(a).cmp(&host_order(b)).then(a.seq.cmp(&b.seq)));
            if !rest.is_empty() {
                let hosts = host_counts(&rest);
                rows.push(SidebarRow::Section(SectionRow {
                    kind: SectionKind::NoGroup,
                    host: String::new(),
                    label: "no group".to_string(),
                    count: rest.len(),
                    count_label: count_label(rest.len(), &hosts),
                    count_short: count_short(rest.len(), &hosts),
                    hosts,
                    local: false,
                }));
                rows.extend(rest.into_iter().map(|d| SidebarRow::Entry(d.row.clone())));
            }
        }
        SidebarMode::Host => {
            let mut all: Vec<&Draft> = drafts.iter().collect();
            all.sort_by(|a, b| {
                host_order(a)
                    .cmp(&host_order(b))
                    .then(a.group.cmp(&b.group))
                    .then(a.member.cmp(&b.member))
                    .then(a.seq.cmp(&b.seq))
            });
            for host in host_counts(&all) {
                let local = host.host == local_host;
                rows.push(SidebarRow::Section(SectionRow {
                    kind: SectionKind::Host,
                    host: host.host.clone(),
                    label: host.label.clone(),
                    count: host.count,
                    hosts: Vec::new(),
                    count_label: host.count.to_string(),
                    count_short: host.count.to_string(),
                    local,
                }));
                rows.extend(
                    all.iter()
                        .filter(|d| d.row.host == host.host)
                        .map(|d| SidebarRow::Entry(d.row.clone())),
                );
            }
        }
    }

    let agents_empty = resolved_hosts.is_empty().then(|| AgentsEmptyNotice {
        text: if input.bus.reason.is_empty() {
            "no live agents"
        } else {
            "no gomuxai on this relay"
        },
        reason: (!input.bus.reason.is_empty()).then(|| input.bus.reason.clone()),
    });

    SidebarModel {
        mode: input.mode,
        rows,
        terminals_empty: input.terminals.is_empty(),
        agents_empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, host: &str, status: &str, live: bool) -> BusAgentInput {
        BusAgentInput {
            name: name.to_string(),
            host: host.to_string(),
            live,
            status: status.to_string(),
            ..BusAgentInput::default()
        }
    }

    // Ports `tools/wall-bus.test.mjs`: "WallBus pure helpers: targets, host grouping, status order, screen ids".
    #[test]
    fn wall_bus_pure_helpers_targets_hosts_status_and_screen_ids() {
        assert_eq!(bus_target("lead", ""), "lead");
        assert_eq!(bus_target("lead", "workstation"), "lead@workstation");

        assert!(is_bus_target("wall-lead"));
        assert!(is_bus_target("wall_1@workstation"));
        assert!(!is_bus_target("a@b@c"));
        assert!(!is_bus_target("-flag"));
        assert!(!is_bus_target("a b"));
        assert!(!is_bus_target("a;rm"));
        assert!(!is_bus_target(""));
        assert!(!is_bus_target("a.b"));

        let snap = BusSnapshot {
            host: "box-a".to_string(),
            reason: String::new(),
            agents: vec![
                agent("z-idle", "box-b", "idle", true),
                agent("a-dead", "box-a", "ended", false),
                agent("b-idle", "box-a", "idle", true),
                agent("a-wait", "box-a", "waiting", true),
                agent("a-proc", "box-a", "processing", true),
            ],
        };
        let hosts = bus_hosts(&snap);
        let host_names: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(host_names, vec!["box-a", "box-b"], "local host first");
        let box_a_targets: Vec<&str> = hosts[0].agents.iter().map(|a| a.target.as_str()).collect();
        assert_eq!(
            box_a_targets,
            vec!["a-wait@box-a", "a-proc@box-a", "b-idle@box-a"],
            "dead dropped, waiting -> processing -> idle"
        );
        let box_b_targets: Vec<&str> = hosts[1].agents.iter().map(|a| a.target.as_str()).collect();
        assert_eq!(box_b_targets, vec!["z-idle@box-b"]);

        assert!(bus_attachable(Some(true)));
        assert!(!bus_attachable(Some(false)));
        assert!(
            bus_attachable(None),
            "unknown (remote host) stays attachable"
        );
        assert_eq!(AgentStatus::parse("weird").word(), "unknown");
        assert_eq!(AgentStatus::parse("").word(), "unknown");

        assert_eq!(local_bus_name("lead", ""), Some("lead"));
        assert_eq!(local_bus_name("lead@box-a", "box-a"), Some("lead"));
        assert_eq!(local_bus_name("lead@box-b", "box-a"), None);
        assert_eq!(local_bus_name("lead@box-a", ""), None);
        assert_eq!(local_bus_name("-bad", "box-a"), None);

        assert_eq!(gomux_screen_id("wall-lead"), "gx-wall-lead");
        assert_eq!(gomux_screen_id("a/b c"), "gx-a_b_c");
        assert_eq!(gomux_screen_id(&"x".repeat(80)).len(), 64);
    }

    fn term(id: &str, name: &str, group: Option<&str>, attached: Option<&str>) -> SidebarTerminal {
        SidebarTerminal {
            id: id.to_string(),
            name: name.to_string(),
            group: group.map(str::to_string),
            attached: attached.map(str::to_string),
            ..SidebarTerminal::default()
        }
    }

    fn group(id: &str, name: &str, members: &[&str]) -> SidebarGroup {
        SidebarGroup {
            id: id.to_string(),
            name: name.to_string(),
            layout: GroupLayout::Tile,
            columns: 2,
            members: members.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    /// Two groups over three hosts: plain terminals, a local agent in its
    /// `gx-` terminal, remote agents in attach terminals, an attach terminal
    /// whose agent the bus does not list, and agents with no terminal.
    fn fleet(mode: SidebarMode) -> SidebarInput {
        let mut logs = term("t4", "logs", Some("g-2"), None);
        logs.hidden = true;
        SidebarInput {
            terminals: vec![
                term("t1", "shell", Some("g-1"), None),
                term("gx-lead", "lead", Some("g-1"), None),
                term("t3", "gate", Some("g-1"), Some("linux-gate@workstation")),
                logs,
                term("t5", "bench", Some("g-2"), Some("bench@workstation")),
                term("t6", "scratch", None, None),
                term("t7", "remote shell", None, Some("worker@workstation")),
            ],
            groups: vec![
                // The canvas's order puts the remote member first: the list sorts by host.
                group("g-1", "build", &["t3", "t1", "gx-lead"]),
                group("g-2", "docs", &["t5", "t4"]),
            ],
            focused_id: Some("gx-lead".to_string()),
            collapsed_groups: BTreeSet::new(),
            bus: BusSnapshot {
                host: "laptop-2".to_string(),
                reason: String::new(),
                agents: vec![
                    BusAgentInput {
                        detail: "cargo test".to_string(),
                        cwd: "/repo".to_string(),
                        attached: Some(true),
                        ..agent("lead", "laptop-2", "processing", true)
                    },
                    BusAgentInput {
                        attached: Some(false),
                        ..agent("reviewer", "laptop-2", "idle", true)
                    },
                    BusAgentInput {
                        attached: Some(true),
                        ..agent("probe", "laptop-2", "waiting", true)
                    },
                    agent("linux-gate", "workstation", "idle", true),
                    agent("worker", "workstation", "waiting", true),
                    agent("zeta", "alpha-box", "idle", true),
                    agent("gone", "laptop-2", "ended", false),
                ],
            },
            mode,
        }
    }

    fn entry<'a>(model: &'a SidebarModel, token: &str) -> &'a EntryRow {
        model
            .rows
            .iter()
            .find_map(|row| match row {
                SidebarRow::Entry(e) if row.token() == token => Some(e),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row {token} in {:?}", model.tokens()))
    }

    #[test]
    fn group_mode_is_one_list_with_members_sorted_by_host_and_counted_by_host() {
        let model = build_sidebar(&fleet(SidebarMode::Group));
        assert_eq!(model.mode, SidebarMode::Group);
        assert_eq!(
            model.tokens(),
            vec![
                "g:g-1",
                // This machine first, in the group's own order; then the other host.
                "t:t1",
                "t:gx-lead+lead@laptop-2",
                "t:t3+linux-gate@workstation",
                "g:g-2",
                "t:t4",
                // An attach terminal sits on its target's host, listed by the bus or not.
                "t:t5",
                "s:none",
                // Terminals, then agents with no terminal by urgency; then the other hosts by name.
                "t:t6",
                "a:probe@laptop-2",
                "a:reviewer@laptop-2",
                "a:zeta@alpha-box",
                "t:t7+worker@workstation",
            ],
            "every terminal once, every live agent once, the ended one dropped"
        );

        let SidebarRow::Group(build) = &model.rows[0] else {
            panic!("expected the build group header");
        };
        assert_eq!(build.name, "build");
        assert_eq!(build.member_count, 3);
        assert_eq!(build.count_label, "3 (laptop-2 2, workstation 1)");
        assert_eq!(build.count_short, "laptop-2 2, workstation 1");
        assert_eq!(build.color, group_color("g-1"));
        let SidebarRow::Group(docs) = &model.rows[4] else {
            panic!("expected the docs group header");
        };
        assert_eq!(docs.count_label, "2 (laptop-2 1, workstation 1)");
        assert_ne!(docs.color, build.color);

        let SidebarRow::Section(rest) = &model.rows[7] else {
            panic!("expected the no-group section");
        };
        assert_eq!(rest.kind, SectionKind::NoGroup);
        assert_eq!(rest.label, "no group");
        assert_eq!(rest.count, 5);
        assert_eq!(rest.count_label, "5 (laptop-2 3, alpha-box 1, workstation 1)");
        assert_eq!(rest.count_short, "laptop-2 3, alpha-box 1, workstation 1");

        // A member's chip is its group's colour; the header above names the group.
        let t1 = entry(&model, "t:t1");
        let chip = t1.chip.as_ref().expect("a member carries a chip");
        assert_eq!((chip.id.as_str(), chip.color, chip.show_name), ("g-1", build.color, false));
        assert!(t1.local && t1.host_tag.is_none() && t1.tooltip.is_empty());
        assert!(entry(&model, "t:t6").chip.is_none());
        assert!(entry(&model, "a:probe@laptop-2").chip.is_none());

        // Rows from another machine say so.
        let gate = entry(&model, "t:t3+linux-gate@workstation");
        assert_eq!(gate.host, "workstation");
        assert_eq!(gate.host_tag.as_deref(), Some("workstation"));
        assert!(!gate.local);
        let bench = entry(&model, "t:t5");
        assert_eq!(bench.host, "workstation");
        assert!(bench.agent.is_none());
        assert_eq!(bench.tooltip, "attached to bench@workstation");
        assert_eq!(
            entry(&model, "a:zeta@alpha-box").host_tag.as_deref(),
            Some("alpha-box")
        );

        assert!(entry(&model, "t:t4").hidden);
        assert!(model.agents_empty.is_none());
        assert!(!model.terminals_empty);
    }

    #[test]
    fn host_mode_lists_each_host_with_rows_sorted_by_group_and_ungrouped_last() {
        let model = build_sidebar(&fleet(SidebarMode::Host));
        assert_eq!(model.mode, SidebarMode::Host);
        assert_eq!(
            model.tokens(),
            vec![
                "h:laptop-2",
                // build's members, docs's, then the rows in no group.
                "t:t1",
                "t:gx-lead+lead@laptop-2",
                "t:t4",
                "t:t6",
                "a:probe@laptop-2",
                "a:reviewer@laptop-2",
                "h:alpha-box",
                "a:zeta@alpha-box",
                "h:workstation",
                "t:t3+linux-gate@workstation",
                "t:t5",
                "t:t7+worker@workstation",
            ]
        );
        let sections: Vec<(&str, usize, &str, bool)> = model
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Section(s) => {
                    assert_eq!(s.kind, SectionKind::Host);
                    Some((s.label.as_str(), s.count, s.count_label.as_str(), s.local))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            sections,
            vec![
                ("laptop-2", 6, "6", true),
                ("alpha-box", 1, "1", false),
                ("workstation", 3, "3", false),
            ]
        );

        // No header names the group here, so the chip does.
        let gate = entry(&model, "t:t3+linux-gate@workstation");
        let chip = gate.chip.as_ref().expect("a grouped row carries a chip");
        assert_eq!(
            (chip.name.as_str(), chip.color, chip.show_name),
            ("build", group_color("g-1"), true)
        );
        assert!(gate.host_tag.is_none(), "the host section says the host");
        assert_eq!(
            entry(&model, "t:t4").chip.as_ref().map(|c| c.name.as_str()),
            Some("docs")
        );
        assert!(entry(&model, "t:t7+worker@workstation").chip.is_none());
    }

    #[test]
    fn an_agent_on_the_wall_merges_into_its_terminal_and_one_off_it_keeps_a_row() {
        let model = build_sidebar(&fleet(SidebarMode::Group));

        // Merged: the terminal's name and tools, the agent's status and hover text.
        let lead = entry(&model, "t:gx-lead+lead@laptop-2");
        assert_eq!(lead.name, "lead");
        assert!(lead.focused && lead.interactive && !lead.is_agent_only());
        assert_eq!(lead.click_cmd, vec!["wall.reveal", "gx-lead"]);
        let tools = lead.terminal.as_ref().unwrap();
        assert_eq!(tools.close_cmd, vec!["wall.close", "gx-lead"]);
        assert_eq!(tools.agent_toggle_cmd, vec!["wall.agent", "gx-lead", "toggle"]);
        let badge = lead.agent.as_ref().unwrap();
        assert_eq!(badge.status, AgentStatus::Processing);
        assert_eq!(badge.data_status, "processing");
        assert_eq!(badge.on_wall.as_deref(), Some("gx-lead"));
        assert_eq!(
            lead.tooltip,
            "lead@laptop-2\nprocessing\ncargo test\n/repo\non the wall as gx-lead"
        );

        // An attach terminal keeps its own name; the agent's is in the hover text.
        let worker = entry(&model, "t:t7+worker@workstation");
        assert_eq!(worker.name, "remote shell");
        assert_eq!(worker.agent.as_ref().unwrap().status, AgentStatus::Waiting);
        assert!(worker.tooltip.starts_with("worker@workstation\nwaiting\n"));
        assert!(!worker.focused);

        // No terminal on the wall: a row of its own, and a click attaches.
        let probe = entry(&model, "a:probe@laptop-2");
        assert!(probe.is_agent_only() && probe.interactive && !probe.hidden);
        assert_eq!(probe.name, "probe");
        assert_eq!(probe.click_cmd, vec!["wall.attach", "probe@laptop-2"]);
        assert!(probe.agent.as_ref().unwrap().on_wall.is_none());
        assert!(probe.tooltip.ends_with("click to attach"));

        // No session to attach to: listed, dimmed.
        let reviewer = entry(&model, "a:reviewer@laptop-2");
        assert!(!reviewer.interactive);
        assert!(!reviewer.agent.as_ref().unwrap().attachable);
        assert!(reviewer.tooltip.ends_with("no live session to attach to"));

        // The same agents, merged the same way, whichever mode lists them.
        let by_host = build_sidebar(&fleet(SidebarMode::Host));
        let mut a = model.tokens();
        let mut b = by_host.tokens();
        a.retain(|t| t.starts_with("t:") || t.starts_with("a:"));
        b.retain(|t| t.starts_with("t:") || t.starts_with("a:"));
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn two_agents_resolving_to_one_terminal_both_stay_listed() {
        // `gx-a` is local agent `a`'s terminal by its id, and attached to `b@box-b` too.
        let input = SidebarInput {
            terminals: vec![term("gx-a", "a", None, Some("b@box-b"))],
            bus: BusSnapshot {
                host: "box-a".to_string(),
                reason: String::new(),
                agents: vec![
                    agent("a", "box-a", "idle", true),
                    agent("b", "box-b", "idle", true),
                ],
            },
            ..SidebarInput::default()
        };
        let model = build_sidebar(&input);
        assert_eq!(
            model.tokens(),
            vec!["s:none", "t:gx-a+a@box-a", "a:b@box-b"]
        );
        let b = entry(&model, "a:b@box-b");
        assert_eq!(b.agent.as_ref().unwrap().on_wall.as_deref(), Some("gx-a"));
        assert!(b.interactive);
        assert!(b.tooltip.ends_with("attached in gx-a"));
    }

    #[test]
    fn sidebar_groups_folding_hidden_marks_and_row_commands() {
        let input = SidebarInput {
            terminals: vec![
                SidebarTerminal {
                    id: "t1".to_string(),
                    name: "build".to_string(),
                    group: Some("g1".to_string()),
                    hidden: false,
                    attached: None,
                    agent_view_open: true,
                },
                SidebarTerminal {
                    id: "t2".to_string(),
                    name: "logs".to_string(),
                    group: Some("g1".to_string()),
                    hidden: true,
                    attached: None,
                    agent_view_open: false,
                },
                SidebarTerminal {
                    id: "t3".to_string(),
                    name: "notes".to_string(),
                    group: Some("g2".to_string()),
                    hidden: false,
                    attached: None,
                    agent_view_open: false,
                },
                SidebarTerminal {
                    id: "t4".to_string(),
                    name: "scratch".to_string(),
                    group: None,
                    hidden: true,
                    attached: None,
                    agent_view_open: false,
                },
            ],
            groups: vec![
                SidebarGroup {
                    id: "g1".to_string(),
                    name: "Backend".to_string(),
                    layout: GroupLayout::Tile,
                    columns: 2,
                    members: vec!["t1".to_string(), "missing".to_string(), "t2".to_string()],
                },
                SidebarGroup {
                    id: "g2".to_string(),
                    name: "Docs".to_string(),
                    layout: GroupLayout::Freeform,
                    columns: 1,
                    members: vec!["t3".to_string()],
                },
            ],
            focused_id: Some("t1".to_string()),
            collapsed_groups: BTreeSet::from(["g2".to_string()]),
            bus: BusSnapshot::default(),
            mode: SidebarMode::Group,
        };

        let model = build_sidebar(&input);
        assert!(!model.terminals_empty);
        // g1 header + t1 + t2 + g2 header (folded, so t3 omitted) + no-group header + t4.
        assert_eq!(
            model.tokens(),
            vec!["g:g1", "t:t1", "t:t2", "g:g2", "s:none", "t:t4"]
        );

        let SidebarRow::Group(g1) = &model.rows[0] else {
            panic!("expected g1 group row");
        };
        assert_eq!(g1.name, "Backend");
        assert_eq!(g1.member_count, 2, "a member that is no terminal is not counted");
        assert_eq!(g1.count_label, "2 (this host 2)");
        assert_eq!(g1.count_short, "this host 2");
        assert!(!g1.collapsed);
        assert_eq!(g1.layout_badge, "tile");
        assert_eq!(g1.click_cmd, vec!["wall.group.frame", "g1"]);
        assert_eq!(g1.collapse_cmd, vec!["wall.sidebar.collapse", "g1"]);
        assert_eq!(
            g1.cols_dec_cmd.as_deref(),
            Some(["wall.group.columns".to_string(), "g1".into(), "-1".into()].as_slice())
        );
        assert_eq!(
            g1.cols_inc_cmd.as_deref(),
            Some(["wall.group.columns".to_string(), "g1".into(), "+1".into()].as_slice())
        );
        assert_eq!(
            g1.toggle_layout_cmd,
            vec!["wall.group.layout", "g1", "freeform"]
        );
        assert_eq!(g1.ungroup_cmd, vec!["wall.ungroup", "g1"]);

        let SidebarRow::Entry(t1) = &model.rows[1] else {
            panic!("expected t1 terminal row");
        };
        let tools = t1.terminal.as_ref().expect("t1 is a terminal");
        assert!(tools.in_group);
        assert!(!t1.hidden);
        assert!(t1.focused);
        assert!(tools.agent_view_open);
        assert_eq!(t1.click_cmd, vec!["wall.reveal", "t1"]);
        assert_eq!(tools.agent_toggle_cmd, vec!["wall.agent", "t1", "toggle"]);
        assert_eq!(
            tools.leave_group_cmd.as_deref(),
            Some(["wall.group.remove".to_string(), "t1".into()].as_slice())
        );
        assert_eq!(tools.visibility_cmd, vec!["wall.hide", "t1"]);
        assert_eq!(tools.close_cmd, vec!["wall.close", "t1"]);

        let SidebarRow::Entry(t2) = &model.rows[2] else {
            panic!("expected t2 terminal row");
        };
        assert!(t2.hidden);
        assert!(!t2.focused);
        assert_eq!(
            t2.terminal.as_ref().unwrap().visibility_cmd,
            vec!["wall.show", "t2"]
        );

        let SidebarRow::Group(g2) = &model.rows[3] else {
            panic!("expected g2 group row");
        };
        assert!(g2.collapsed);
        assert_eq!(g2.count_label, "1 (this host 1)", "a folded group still counts");
        assert_eq!(g2.layout_badge, "free");
        assert!(g2.cols_dec_cmd.is_none());
        assert!(g2.cols_inc_cmd.is_none());
        assert_eq!(
            g2.toggle_layout_cmd,
            vec!["wall.group.layout", "g2", "tile"]
        );

        let SidebarRow::Entry(t4) = &model.rows[5] else {
            panic!("expected loose t4 terminal row");
        };
        let tools = t4.terminal.as_ref().unwrap();
        assert!(!tools.in_group);
        assert!(t4.hidden);
        assert!(tools.leave_group_cmd.is_none());
        assert!(t4.chip.is_none());

        // Host mode has no group headers, so nothing folds: t3 is listed.
        let by_host = build_sidebar(&SidebarInput {
            mode: SidebarMode::Host,
            ..input
        });
        assert_eq!(
            by_host.tokens(),
            vec!["h:local", "t:t1", "t:t2", "t:t3", "t:t4"]
        );
    }

    #[test]
    fn empty_states_and_an_unnamed_local_host() {
        let empty_missing_cli = build_sidebar(&SidebarInput {
            bus: BusSnapshot {
                host: String::new(),
                reason: "agent not in PATH".to_string(),
                agents: Vec::new(),
            },
            ..SidebarInput::default()
        });
        assert!(empty_missing_cli.terminals_empty);
        assert!(empty_missing_cli.rows.is_empty());
        assert_eq!(
            empty_missing_cli.agents_empty,
            Some(AgentsEmptyNotice {
                text: "no gomuxai on this relay",
                reason: Some("agent not in PATH".to_string()),
            })
        );
        let empty_fleet = build_sidebar(&SidebarInput::default());
        assert_eq!(
            empty_fleet.agents_empty,
            Some(AgentsEmptyNotice {
                text: "no live agents",
                reason: None,
            })
        );

        // The bus names no host: a plain terminal, a bare attach target and an
        // agent with no host are all this machine.
        let input = SidebarInput {
            terminals: vec![
                term("gx-lead", "lead", None, None),
                term("term-7", "worker-view", None, Some("worker@remote1")),
                term("term-8", "helper-view", None, Some("helper")),
            ],
            focused_id: Some("gx-lead".to_string()),
            bus: BusSnapshot {
                host: String::new(),
                reason: String::new(),
                agents: vec![
                    BusAgentInput {
                        attached: Some(true),
                        ..agent("lead", "", "processing", true)
                    },
                    BusAgentInput {
                        attached: Some(false),
                        ..agent("sub", "", "idle", true)
                    },
                    agent("helper", "", "idle", true),
                    agent("worker", "remote1", "waiting", true),
                ],
            },
            mode: SidebarMode::Host,
            ..SidebarInput::default()
        };
        let model = build_sidebar(&input);
        assert!(model.agents_empty.is_none());
        assert_eq!(
            model.tokens(),
            vec![
                "h:local",
                "t:gx-lead+lead",
                "t:term-8+helper",
                "a:sub",
                "h:remote1",
                "t:term-7+worker@remote1",
            ]
        );
        let SidebarRow::Section(this_host) = &model.rows[0] else {
            panic!("expected the local host section");
        };
        assert_eq!(this_host.label, "this host");
        assert!(this_host.local);
        assert_eq!(this_host.count, 3);
        assert!(entry(&model, "t:gx-lead+lead").focused);

        let by_group = build_sidebar(&SidebarInput {
            mode: SidebarMode::Group,
            ..input
        });
        let SidebarRow::Section(rest) = &by_group.rows[0] else {
            panic!("expected the no-group section");
        };
        assert_eq!(rest.count_label, "4 (this host 3, remote1 1)");
    }

    #[test]
    fn sidebar_mode_words_verbs_and_stored_form() {
        assert_eq!(SidebarMode::default(), SidebarMode::Group);
        for mode in [SidebarMode::Group, SidebarMode::Host] {
            assert_eq!(SidebarMode::parse(mode.as_str()), Some(mode));
            assert_eq!(mode.other().other(), mode);
            assert_ne!(mode.other(), mode);
            assert_eq!(
                serde_json::to_value(mode).unwrap(),
                serde_json::Value::String(mode.as_str().to_string())
            );
        }
        assert_eq!(SidebarMode::parse("hosts"), None);
        assert_eq!(SidebarMode::Host.verb(), vec!["wall.sidebar", "mode", "host"]);
    }

    fn relative_luminance(hex: &str) -> f64 {
        let channel = |i: usize| {
            let c = f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap()) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn group_colors_read_on_dark_and_light_panels() {
        for color in GROUP_COLORS {
            assert!(color.len() == 7 && color.starts_with('#'), "{color}");
            // The sidebar's own fill, and a light panel's.
            for panel in ["#0c0e14", "#f4f5f7"] {
                let ratio = contrast(color, panel);
                assert!(ratio >= 3.0, "{color} on {panel}: {ratio:.2}:1");
            }
        }
        let distinct: BTreeSet<&str> = GROUP_COLORS.into_iter().collect();
        assert_eq!(distinct.len(), GROUP_COLORS.len());
    }

    #[test]
    fn a_groups_color_is_a_function_of_its_id() {
        // The wall's own ids take the palette in order: neighbours differ.
        let numbered: Vec<&str> = (1..=GROUP_COLORS.len())
            .map(|n| group_color(&format!("g-{n}")))
            .collect();
        assert_eq!(numbered, GROUP_COLORS.to_vec());
        assert_eq!(group_color("g-11"), group_color("g-1"));
        assert_eq!(group_color("g-0"), GROUP_COLORS[GROUP_COLORS.len() - 1]);
        // Any other id hashes to one of them, the same one every time.
        for id in ["build", "g-", "g-x", "", "g-99999999999999999999999"] {
            let color = group_color(id);
            assert!(GROUP_COLORS.contains(&color), "{id}");
            assert_eq!(color, group_color(id));
        }
        assert_ne!(group_color("build"), group_color("docs"));
    }
}
