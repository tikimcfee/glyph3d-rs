//! Port of the pure model inside `wall/Sidebar.jsx` and the pure data rules of
//! `wall/WallBus.js`:
//!
//! - **Bus model (`wall/WallBus.js`)**: [`AgentStatus`] (`BUS_STATUSES` /
//!   `busStatus`), [`bus_target`], [`is_bus_target`], [`local_bus_name`],
//!   [`gomux_screen_id`], [`bus_attachable`], [`bus_hosts`], and
//!   [`resolve_agent_on_wall`].
//! - **Sidebar tree (`wall/Sidebar.jsx`)**: [`build_sidebar`], turning the
//!   wall's terminals, groups, folded-group set, focused terminal id, and
//!   gomuxai bus snapshot into plain rows ([`SidebarRow::Group`],
//!   [`SidebarRow::Terminal`]) plus the per-host [`AgentHostSection`] list and
//!   the exact wall verb (`Vec<String>`) each row or row tool button runs.

use std::collections::{BTreeSet, HashMap, HashSet};

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
// Sidebar pure model (`wall/Sidebar.jsx`)
// ---------------------------------------------------------------------------

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
}

/// A top-level group header row in the sidebar (`GroupRow` in `Sidebar.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRow {
    pub id: String,
    pub name: String,
    /// Count badge (`g.members.length`).
    pub member_count: usize,
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

/// A terminal row in the sidebar (`TermRow` in `Sidebar.jsx`), either indented
/// under an expanded group (`depth == 1`) or loose (`depth == 0`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRow {
    pub id: String,
    pub name: String,
    /// Indentation depth (`1` inside an unfolded group, `0` when ungrouped).
    pub depth: u8,
    pub in_group: bool,
    /// Whether the terminal is hidden on the canvas (dims label, shows `"hidden"` tag).
    pub hidden: bool,
    /// Whether this terminal is `focused_id`.
    pub focused: bool,
    /// Whether the Agent View toggle button is highlighted (`on`).
    pub agent_view_open: bool,
    /// Clicking the row reveals the terminal (`["wall.reveal", id]`).
    pub click_cmd: Vec<String>,
    /// Agent View toggle button (`["wall.agent", id, "toggle"]`).
    pub agent_toggle_cmd: Vec<String>,
    /// Leave-group button when `in_group` (`["wall.group.remove", id]`).
    pub leave_group_cmd: Option<Vec<String>>,
    /// Show/hide button (`["wall.show", id]` if hidden, else `["wall.hide", id]`).
    pub visibility_cmd: Vec<String>,
    /// Hold-to-close button (`["wall.close", id]`).
    pub close_cmd: Vec<String>,
}

/// One row in the upper terminals-and-groups tree of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Group(GroupRow),
    Terminal(TerminalRow),
}

/// One gomuxai agent row under a host header (`AgentRow` in `Sidebar.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub host: String,
    pub target: String,
    pub status: AgentStatus,
    /// Raw status attribute (`data-status`: `status_raw` if non-empty, else `"unknown"`).
    pub data_status: String,
    pub attachable: bool,
    /// Terminal id on the wall attached to this agent, if any (renders `"on wall"` tag).
    pub on_wall: Option<String>,
    /// Highlighted when `on_wall` matches `focused_id`.
    pub focused: bool,
    /// Pointer cursor / normal brightness (`attachable || on_wall.is_some()`).
    pub interactive: bool,
    /// Multi-line hover title (`target`, `status (endReason)`, `detail`, `cwd`, attachment note).
    pub tooltip: String,
    /// Clicking the row runs `["wall.attach", target]`.
    pub click_cmd: Vec<String>,
}

/// One host subsection in the sidebar's `agents` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHostSection {
    /// Raw host key (`""` for unnamed local host).
    pub host: String,
    /// Display header (`"this host"` when `host` is empty, else `host`).
    pub label: String,
    /// Agent count badge (`agents.len()`).
    pub count: usize,
    pub agents: Vec<AgentRow>,
}

/// Empty-state row shown in the `agents` section when no live agents are listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsEmptyNotice {
    /// `"no gomuxai on this relay"` when `reason` is present, else `"no live agents"`.
    pub text: &'static str,
    /// Hover tooltip reason, if any.
    pub reason: Option<String>,
}

/// The `agents` section at the bottom of the sidebar (`AgentsSection` in `Sidebar.jsx`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarAgents {
    pub hosts: Vec<AgentHostSection>,
    pub empty: Option<AgentsEmptyNotice>,
}

/// Complete view-model output of [`build_sidebar`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarModel {
    pub rows: Vec<SidebarRow>,
    /// True when `input.terminals` is empty (renders the `"no terminals"` placeholder row).
    pub terminals_empty: bool,
    pub agents: SidebarAgents,
}

fn build_terminal_row(t: &SidebarTerminal, depth: u8, focused_id: Option<&str>) -> TerminalRow {
    let in_group = t.group.is_some();
    TerminalRow {
        id: t.id.clone(),
        name: t.name.clone(),
        depth,
        in_group,
        hidden: t.hidden,
        focused: focused_id == Some(t.id.as_str()),
        agent_view_open: t.agent_view_open,
        click_cmd: vec!["wall.reveal".to_string(), t.id.clone()],
        agent_toggle_cmd: vec![
            "wall.agent".to_string(),
            t.id.clone(),
            "toggle".to_string(),
        ],
        leave_group_cmd: in_group
            .then(|| vec!["wall.group.remove".to_string(), t.id.clone()]),
        visibility_cmd: vec![
            if t.hidden { "wall.show" } else { "wall.hide" }.to_string(),
            t.id.clone(),
        ],
        close_cmd: vec!["wall.close".to_string(), t.id.clone()],
    }
}

fn build_group_row(g: &SidebarGroup, collapsed: bool) -> GroupRow {
    let tiled = g.layout == GroupLayout::Tile;
    GroupRow {
        id: g.id.clone(),
        name: g.name.clone(),
        member_count: g.members.len(),
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

fn build_agent_tooltip(a: &BusAgent, attachable: bool, on_wall: Option<&str>) -> String {
    let status_line = if a.end_reason.is_empty() {
        a.status.word().to_string()
    } else {
        format!("{} ({})", a.status.word(), a.end_reason)
    };
    let footer = if let Some(tid) = on_wall {
        format!("attached in {tid}")
    } else if attachable {
        "click to attach".to_string()
    } else {
        "no live session to attach to".to_string()
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

/// Build the pure sidebar row list and agents section from plain wall state
/// (`Sidebar.jsx`).
pub fn build_sidebar(input: &SidebarInput) -> SidebarModel {
    let mut by_id: HashMap<&str, &SidebarTerminal> = HashMap::with_capacity(input.terminals.len());
    let mut terminal_ids: HashSet<&str> = HashSet::with_capacity(input.terminals.len());
    let mut attached_to: HashMap<&str, &str> = HashMap::new();
    for t in &input.terminals {
        by_id.insert(t.id.as_str(), t);
        terminal_ids.insert(t.id.as_str());
        if let Some(att) = t.attached.as_deref() {
            if !att.is_empty() {
                attached_to.insert(att, t.id.as_str());
            }
        }
    }

    let focused = input.focused_id.as_deref();
    let mut rows = Vec::new();
    for g in &input.groups {
        let collapsed = input.collapsed_groups.contains(&g.id);
        rows.push(SidebarRow::Group(build_group_row(g, collapsed)));
        if !collapsed {
            for member_id in &g.members {
                if let Some(&t) = by_id.get(member_id.as_str()) {
                    rows.push(SidebarRow::Terminal(build_terminal_row(t, 1, focused)));
                }
            }
        }
    }
    for t in &input.terminals {
        if t.group.is_none() {
            rows.push(SidebarRow::Terminal(build_terminal_row(t, 0, focused)));
        }
    }

    let resolved_hosts = bus_hosts(&input.bus);
    let mut host_sections = Vec::with_capacity(resolved_hosts.len());
    for (hi, h) in resolved_hosts.iter().enumerate() {
        let mut agent_rows = Vec::with_capacity(h.agents.len());
        for a in &h.agents {
            let attachable = bus_attachable(a.attached);
            let on_wall = resolve_agent_on_wall(a, hi, &attached_to, &terminal_ids);
            let is_focused = on_wall.is_some() && on_wall.as_deref() == focused;
            let interactive = attachable || on_wall.is_some();
            let tooltip = build_agent_tooltip(a, attachable, on_wall.as_deref());
            agent_rows.push(AgentRow {
                name: a.name.clone(),
                host: a.host.clone(),
                target: a.target.clone(),
                status: a.status,
                data_status: if a.status_raw.is_empty() {
                    "unknown".to_string()
                } else {
                    a.status_raw.clone()
                },
                attachable,
                on_wall,
                focused: is_focused,
                interactive,
                tooltip,
                click_cmd: vec!["wall.attach".to_string(), a.target.clone()],
            });
        }
        host_sections.push(AgentHostSection {
            label: if h.host.is_empty() {
                "this host".to_string()
            } else {
                h.host.clone()
            },
            host: h.host.clone(),
            count: agent_rows.len(),
            agents: agent_rows,
        });
    }

    let empty = host_sections.is_empty().then(|| AgentsEmptyNotice {
        text: if input.bus.reason.is_empty() {
            "no live agents"
        } else {
            "no gomuxai on this relay"
        },
        reason: (!input.bus.reason.is_empty()).then(|| input.bus.reason.clone()),
    });

    SidebarModel {
        rows,
        terminals_empty: input.terminals.is_empty(),
        agents: SidebarAgents {
            hosts: host_sections,
            empty,
        },
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
        };

        let model = build_sidebar(&input);
        assert!(!model.terminals_empty);
        // g1 header + t1 + t2 + g2 header (collapsed, so t3 omitted) + loose t4 = 5 rows.
        assert_eq!(model.rows.len(), 5);

        let SidebarRow::Group(g1) = &model.rows[0] else {
            panic!("expected g1 group row");
        };
        assert_eq!(g1.name, "Backend");
        assert_eq!(g1.member_count, 3);
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

        let SidebarRow::Terminal(t1) = &model.rows[1] else {
            panic!("expected t1 terminal row");
        };
        assert_eq!(t1.depth, 1);
        assert!(t1.in_group);
        assert!(!t1.hidden);
        assert!(t1.focused);
        assert!(t1.agent_view_open);
        assert_eq!(t1.click_cmd, vec!["wall.reveal", "t1"]);
        assert_eq!(t1.agent_toggle_cmd, vec!["wall.agent", "t1", "toggle"]);
        assert_eq!(
            t1.leave_group_cmd.as_deref(),
            Some(["wall.group.remove".to_string(), "t1".into()].as_slice())
        );
        assert_eq!(t1.visibility_cmd, vec!["wall.hide", "t1"]);
        assert_eq!(t1.close_cmd, vec!["wall.close", "t1"]);

        let SidebarRow::Terminal(t2) = &model.rows[2] else {
            panic!("expected t2 terminal row");
        };
        assert!(t2.hidden);
        assert!(!t2.focused);
        assert_eq!(t2.visibility_cmd, vec!["wall.show", "t2"]);

        let SidebarRow::Group(g2) = &model.rows[3] else {
            panic!("expected g2 group row");
        };
        assert!(g2.collapsed);
        assert_eq!(g2.layout_badge, "free");
        assert!(g2.cols_dec_cmd.is_none());
        assert!(g2.cols_inc_cmd.is_none());
        assert_eq!(
            g2.toggle_layout_cmd,
            vec!["wall.group.layout", "g2", "tile"]
        );

        let SidebarRow::Terminal(t4) = &model.rows[4] else {
            panic!("expected loose t4 terminal row");
        };
        assert_eq!(t4.depth, 0);
        assert!(!t4.in_group);
        assert!(t4.hidden);
        assert!(t4.leave_group_cmd.is_none());
    }

    #[test]
    fn sidebar_agents_section_resolves_wall_terminals_and_empty_reasons() {
        let empty_missing_cli = build_sidebar(&SidebarInput {
            bus: BusSnapshot {
                host: String::new(),
                reason: "agent not in PATH".to_string(),
                agents: Vec::new(),
            },
            ..SidebarInput::default()
        });
        assert!(empty_missing_cli.terminals_empty);
        assert_eq!(
            empty_missing_cli.agents.empty,
            Some(AgentsEmptyNotice {
                text: "no gomuxai on this relay",
                reason: Some("agent not in PATH".to_string()),
            })
        );

        let input = SidebarInput {
            terminals: vec![
                SidebarTerminal {
                    id: "gx-lead".to_string(),
                    name: "lead".to_string(),
                    ..SidebarTerminal::default()
                },
                SidebarTerminal {
                    id: "term-7".to_string(),
                    name: "worker-view".to_string(),
                    attached: Some("worker@remote1".to_string()),
                    ..SidebarTerminal::default()
                },
            ],
            focused_id: Some("gx-lead".to_string()),
            bus: BusSnapshot {
                host: String::new(),
                reason: String::new(),
                agents: vec![
                    BusAgentInput {
                        name: "lead".to_string(),
                        host: String::new(),
                        live: true,
                        status: "processing".to_string(),
                        attached: Some(true),
                        detail: "cargo test".to_string(),
                        cwd: "/repo".to_string(),
                        ..BusAgentInput::default()
                    },
                    BusAgentInput {
                        name: "sub".to_string(),
                        host: String::new(),
                        live: true,
                        status: "idle".to_string(),
                        attached: Some(false),
                        ..BusAgentInput::default()
                    },
                    BusAgentInput {
                        name: "worker".to_string(),
                        host: "remote1".to_string(),
                        live: true,
                        status: "waiting".to_string(),
                        attached: None,
                        ..BusAgentInput::default()
                    },
                ],
            },
            ..SidebarInput::default()
        };

        let model = build_sidebar(&input);
        assert!(model.agents.empty.is_none());
        assert_eq!(model.agents.hosts.len(), 2);
        assert_eq!(model.agents.hosts[0].label, "this host");
        assert_eq!(model.agents.hosts[0].count, 2);

        let lead = &model.agents.hosts[0].agents[0];
        assert_eq!(lead.name, "lead");
        assert_eq!(lead.on_wall.as_deref(), Some("gx-lead"));
        assert!(lead.focused);
        assert!(lead.interactive);
        assert!(lead.tooltip.contains("attached in gx-lead"));
        assert_eq!(lead.click_cmd, vec!["wall.attach", "lead"]);

        let sub = &model.agents.hosts[0].agents[1];
        assert!(!sub.attachable);
        assert!(sub.on_wall.is_none());
        assert!(!sub.interactive);
        assert!(sub.tooltip.contains("no live session to attach to"));

        assert_eq!(model.agents.hosts[1].label, "remote1");
        let remote = &model.agents.hosts[1].agents[0];
        assert_eq!(remote.on_wall.as_deref(), Some("term-7"));
        assert!(!remote.focused);
        assert!(remote.interactive);
    }
}
