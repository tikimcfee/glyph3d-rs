//! Port of `wall/wallAgentView.js` (plus the pure record/hue helpers it consumes from
//! `collections/agentEvents.js` and `collections/toolRegistry.js`): world-space geometry,
//! side picking, tiling/split footprint metrics, action categorization, subset filtering,
//! row & expanded-detail formatting, terminal-to-session pairing, and scrollback jump math
//! for a terminal's attached Agent View.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::config::AgentSide;
use crate::geom::{Point, Rect};
use crate::layout::BAR_ROOM;

/// Which side of a terminal its Agent View sits on, in schema/UI order (`wallAgentView.AGENT_SIDES`).
pub const AGENT_SIDES: &[AgentSide] = AgentSide::ALL;

/// Metadata for one filterable/expandable action category (`wallAgentView.CATEGORIES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategoryInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub short: &'static str,
    pub title: &'static str,
}

/// Filterable / expandable action categories in display order (`wallAgentView.CATEGORIES`).
pub const CATEGORIES: &[CategoryInfo] = &[
    CategoryInfo {
        id: "prompt",
        label: "prompt",
        short: "Prompts",
        title: "Prompts & messages into the agent",
    },
    CategoryInfo {
        id: "think",
        label: "think",
        short: "Thinking",
        title: "Interior reasoning / thinking steps",
    },
    CategoryInfo {
        id: "say",
        label: "say",
        short: "Replies",
        title: "Agent replies",
    },
    CategoryInfo {
        id: "read",
        label: "read",
        short: "File reads",
        title: "Text file reads",
    },
    CategoryInfo {
        id: "image",
        label: "image",
        short: "Image reads",
        title: "Image file reads",
    },
    CategoryInfo {
        id: "edit",
        label: "edit",
        short: "Edits & writes",
        title: "File edits & writes",
    },
    CategoryInfo {
        id: "run",
        label: "run",
        short: "Commands & tasks",
        title: "Shell commands & background tasks",
    },
    CategoryInfo {
        id: "search",
        label: "search",
        short: "Search & web",
        title: "Grep, glob, web search & fetch",
    },
    CategoryInfo {
        id: "ask",
        label: "ask",
        short: "Ask & skills",
        title: "Questions, peer messages, skills & other tools",
    },
    CategoryInfo {
        id: "system",
        label: "system",
        short: "System",
        title: "Harness system records & model requests",
    },
    CategoryInfo {
        id: "noise",
        label: "noise",
        short: "Bookkeeping",
        title: "Bookkeeping calls (TodoWrite, ToolSearch, …)",
    },
];

/// Look up a [`CategoryInfo`] by category id (`wallAgentView.CATEGORY_BY_ID`).
pub fn category_by_id(id: &str) -> Option<&'static CategoryInfo> {
    CATEGORIES.iter().find(|c| c.id == id)
}

/// Every category id in display order (`wallAgentView.CATEGORY_IDS`).
pub const CATEGORY_IDS: &[&str] = &[
    "prompt", "think", "say", "read", "image", "edit", "run", "search", "ask", "system", "noise",
];

/// Content categories shown by default; `noise` stays off until requested (`STANDARD_CATEGORY_IDS`).
pub const STANDARD_CATEGORY_IDS: &[&str] = &[
    "prompt", "think", "say", "read", "image", "edit", "run", "search", "ask", "system",
];

const GROUP_CONVERSATION: &[&str] = &["prompt", "think", "say"];
const GROUP_MESSAGES: &[&str] = &["prompt", "say"];
const GROUP_TOOLS: &[&str] = &["read", "image", "edit", "run", "search", "ask"];
const GROUP_READS: &[&str] = &["read", "image"];
const GROUP_FILES: &[&str] = &["read", "image", "edit"];
const GROUP_SYSTEM_ALL: &[&str] = &["system", "noise"];
const DIRECT_TOOLS: &[&str] = &["edit", "run", "search", "ask"];

/// Named action subsets accepted by filter, expand, and collapse commands (`CATEGORY_GROUPS`).
pub const CATEGORY_GROUPS: &[(&str, &[&str])] = &[
    ("all", CATEGORY_IDS),
    ("standard", STANDARD_CATEGORY_IDS),
    ("conversation", GROUP_CONVERSATION),
    ("messages", GROUP_MESSAGES),
    ("msgs", GROUP_MESSAGES),
    ("tools", GROUP_TOOLS),
    ("tool", GROUP_TOOLS),
    ("reads", GROUP_READS),
    ("files", GROUP_FILES),
    ("system_all", GROUP_SYSTEM_ALL),
];

/// Look up a named category subset in [`CATEGORY_GROUPS`].
pub fn category_group(token: &str) -> Option<&'static [&'static str]> {
    CATEGORY_GROUPS
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, ids)| *ids)
}

/// Nested subgroup inside [`CategoryGroupNode`] (`wallAgentView.CATEGORY_GROUPS_TREE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategorySubgroup {
    pub id: &'static str,
    pub label: &'static str,
    pub token: &'static str,
    pub categories: &'static [&'static str],
}

/// Top-level group in [`CATEGORY_GROUPS_TREE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CategoryGroupNode {
    pub id: &'static str,
    pub label: &'static str,
    pub token: &'static str,
    pub categories: &'static [&'static str],
    pub subgroups: &'static [CategorySubgroup],
    pub direct_categories: &'static [&'static str],
}

const TOOLS_SUBGROUPS: &[CategorySubgroup] = &[CategorySubgroup {
    id: "reads",
    label: "File & image reads",
    token: "reads",
    categories: GROUP_READS,
}];

/// Explicit hierarchical grouping of action categories for collapsible filter & expand menus.
pub const CATEGORY_GROUPS_TREE: &[CategoryGroupNode] = &[
    CategoryGroupNode {
        id: "conversation",
        label: "Conversation",
        token: "conversation",
        categories: GROUP_CONVERSATION,
        subgroups: &[],
        direct_categories: GROUP_CONVERSATION,
    },
    CategoryGroupNode {
        id: "tools",
        label: "Tool calls",
        token: "tools",
        categories: GROUP_TOOLS,
        subgroups: TOOLS_SUBGROUPS,
        direct_categories: DIRECT_TOOLS,
    },
    CategoryGroupNode {
        id: "system_all",
        label: "System & bookkeeping",
        token: "system_all",
        categories: GROUP_SYSTEM_ALL,
        subgroups: &[],
        direct_categories: GROUP_SYSTEM_ALL,
    },
];

/// Active category visibility map (`category_id -> visible`).
pub type FilterMap = BTreeMap<String, bool>;

/// Default category filter map: every standard category enabled, `noise` disabled (`DEFAULT_FILTERS`).
pub fn default_filters() -> FilterMap {
    CATEGORY_IDS
        .iter()
        .map(|&id| (id.to_string(), id != "noise"))
        .collect()
}

/// RGB hue triple in `0.0..=1.0` (`toolRegistry.ACTION_HUES`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RgbHue {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

const IMAGE_HUE: RgbHue = RgbHue { r: 0.32, g: 0.80, b: 0.94 };

/// Identity bucket an action colours/sorts under (`toolRegistry.kindForAction`).
pub fn kind_for_action(action: &str) -> &'static str {
    match action.to_ascii_lowercase().as_str() {
        "read" => "read",
        "grep" | "glob" | "search" => "search",
        "edit" => "edit",
        "write" => "write",
        "bash" | "task" => "run",
        "ask" => "ask",
        "fetch" => "fetch",
        "say" | "send" => "say",
        "think" => "think",
        "prompt" => "prompt",
        "system" => "system",
        "noise" => "noise",
        _ => "other",
    }
}

/// Default per-kind identity hue (`toolRegistry.ACTION_HUES`).
pub fn action_hue(kind: &str) -> RgbHue {
    match kind {
        "read" => RgbHue { r: 0.35, g: 0.66, b: 0.92 },
        "search" => RgbHue { r: 0.70, g: 0.50, b: 0.85 },
        "edit" | "write" => RgbHue { r: 0.90, g: 0.66, b: 0.36 },
        "run" => RgbHue { r: 0.44, g: 0.76, b: 0.46 },
        "ask" => RgbHue { r: 0.95, g: 0.58, b: 0.55 },
        "fetch" => RgbHue { r: 0.45, g: 0.82, b: 0.78 },
        "say" => RgbHue { r: 0.92, g: 0.94, b: 0.98 },
        "think" => RgbHue { r: 0.58, g: 0.52, b: 0.78 },
        "prompt" => RgbHue { r: 0.96, g: 0.42, b: 0.78 },
        "system" => RgbHue { r: 0.80, g: 0.74, b: 0.52 },
        "noise" => RgbHue { r: 0.34, g: 0.36, b: 0.40 },
        _ => RgbHue { r: 0.62, g: 0.64, b: 0.68 },
    }
}

fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// Format an [`RgbHue`] as a `#rrggbb` CSS hex string (`toolRegistry.cssHue`).
pub fn css_hue(hue: RgbHue) -> String {
    let ch = |n: f64| js_round(n.clamp(0.0, 1.0) * 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", ch(hue.r), ch(hue.g), ch(hue.b))
}

/// Identity colour (`#rrggbb`) for a category id (`wallAgentView.categoryHue`).
pub fn category_hue(category_id: &str) -> String {
    if category_id == "image" {
        css_hue(IMAGE_HUE)
    } else {
        css_hue(action_hue(category_id))
    }
}

// ── event records (`collections/agentEvents.js` seam) ────────────────────────

/// Turn metadata attached to an [`EventRecord`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInfo {
    pub id: String,
    pub index: i64,
    #[serde(default)]
    pub derived: bool,
}

/// File content snapshot carried on a tool `read`, `edit`, or `write` record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub text: String,
    pub start: usize,
    pub total: Option<usize>,
}

/// Normalized event record produced from a gomuxai `EVENTS.md` frame (`agentEvents.eventRecord`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord {
    pub id: String,
    pub event_kind: String,
    pub at: i64,
    pub ts: Option<String>,
    pub turn: Option<TurnInfo>,
    pub agent: Option<String>,
    #[serde(default)]
    pub abandoned: bool,
    pub action: String,
    pub tag: Option<String>,
    pub name: Option<String>,
    pub origin: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub images: usize,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub final_reply: bool,
    #[serde(default)]
    pub provisional: bool,
    pub subtype: Option<String>,
    pub target: String,
    pub detail: String,
    pub result: String,
    pub meta: Option<serde_json::Value>,
    pub file: Option<FileSnapshot>,
    pub status: Option<String>,
    #[serde(default)]
    pub sub_sessions: Vec<String>,
}

fn prompt_origin_tag(origin: &str) -> String {
    match origin {
        "human" => "you".to_string(),
        "queued" => "you · queued".to_string(),
        "delivered" => "delivered".to_string(),
        "task-notification" => "notification".to_string(),
        "peer" => "peer".to_string(),
        "command" => "command".to_string(),
        "system" => "harness".to_string(),
        "" => "prompt".to_string(),
        other => other.to_string(),
    }
}

/// Convert a raw `EVENTS.md` event JSON object into an [`EventRecord`] (`agentEvents.eventRecord`).
///
/// When `show_noise` is `false`, `tool` events with `action == "noise"` return `Ok(None)`.
pub fn event_record(
    ev: &serde_json::Value,
    at_override: Option<i64>,
    show_noise: bool,
) -> Result<Option<EventRecord>, String> {
    let obj = ev
        .as_object()
        .ok_or_else(|| "[agentEvents] frame: not a JSON object".to_string())?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let kind = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("[agentEvents] event {id}: missing kind"))?;
    let body = obj
        .get(kind)
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("[agentEvents] event {id} ({kind}) has no {kind} body"))?;

    let at = at_override
        .or_else(|| obj.get("at").and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let ts = obj
        .get("ts")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let turn = obj.get("turn").and_then(|t| {
        let tobj = t.as_object()?;
        Some(TurnInfo {
            id: tobj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            index: tobj.get("index").and_then(|v| v.as_i64()).unwrap_or(0),
            derived: tobj.get("derived").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    });
    let agent = obj
        .get("agent")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let abandoned = obj
        .get("abandoned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut rec = EventRecord {
        id: id.clone(),
        event_kind: kind.to_string(),
        at,
        ts,
        turn,
        agent,
        abandoned,
        ..EventRecord::default()
    };

    match kind {
        "prompt" => {
            let origin = body.get("origin").and_then(|v| v.as_str());
            rec.action = "prompt".to_string();
            rec.tag = Some(prompt_origin_tag(origin.unwrap_or("")));
            rec.origin = origin.map(str::to_string);
            rec.command = body
                .get("command")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            rec.images = body
                .get("images")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            rec.result = body
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        "think" => {
            let redacted = body
                .get("redacted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            rec.action = "think".to_string();
            rec.redacted = redacted;
            rec.detail = if redacted {
                "redacted".to_string()
            } else {
                String::new()
            };
            rec.result = body
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        "say" => {
            rec.action = "say".to_string();
            rec.final_reply = body.get("final").and_then(|v| v.as_bool()).unwrap_or(false);
            rec.provisional = body
                .get("provisional")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            rec.result = body
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        "tool" => {
            let raw_action = body.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if raw_action == "noise" && !show_noise {
                return Ok(None);
            }
            let raw_name = body.get("name").and_then(|v| v.as_str());
            let action = if !raw_action.is_empty() {
                raw_action
            } else {
                raw_name.filter(|s| !s.is_empty()).unwrap_or("tool")
            }
            .to_lowercase();
            let status = body
                .get("result")
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("pending")
                .to_string();
            let target = body
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let out = body
                .get("result")
                .and_then(|r| r.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            rec.action = action;
            rec.name = raw_name.map(str::to_string);
            if raw_action == "noise" {
                if let Some(n) = raw_name {
                    rec.tag = Some(n.to_string());
                }
            }
            rec.target = target.clone();
            rec.detail = body
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            rec.result = if !target.is_empty() && status != "error" {
                String::new()
            } else {
                out
            };
            rec.meta = body
                .get("meta")
                .filter(|m| m.is_object())
                .cloned();
            rec.file = body.get("file").and_then(|f| {
                let fobj = f.as_object()?;
                let text = fobj.get("text")?.as_str()?.to_string();
                let start = fobj
                    .get("start")
                    .and_then(|v| v.as_f64())
                    .map(|n| n.floor() as isize)
                    .unwrap_or(1)
                    .max(1) as usize;
                let total = fobj
                    .get("total")
                    .and_then(|v| v.as_u64())
                    .filter(|&t| t > 0)
                    .map(|t| t as usize);
                Some(FileSnapshot { text, start, total })
            });
            rec.status = Some(status);
            rec.sub_sessions = body
                .get("sub_sessions")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }
        "system" => {
            let subtype = body
                .get("subtype")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            rec.action = "system".to_string();
            rec.detail = subtype.clone().unwrap_or_default();
            rec.subtype = subtype;
            rec.result = if let Some(text) = body.get("text").and_then(|v| v.as_str()) {
                text.to_string()
            } else if let Some(data) = body.get("data").filter(|v| !v.is_null()) {
                serde_json::to_string_pretty(data).unwrap_or_default()
            } else {
                String::new()
            };
        }
        "request" => {
            rec.action = "request".to_string();
            rec.detail = body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        other => {
            return Err(format!(
                "[agentEvents] event {id}: unknown kind {other:?}"
            ));
        }
    }
    Ok(Some(rec))
}

/// Info rows describing an [`EventRecord`]'s own facts (`agentEvents.eventFacts`).
pub fn event_facts(rec: &EventRecord) -> Vec<String> {
    if rec.event_kind.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    if let Some(ref turn) = rec.turn {
        let suffix = if turn.derived { " (derived)" } else { "" };
        rows.push(format!("turn {}{suffix}", turn.index));
    }
    if rec.abandoned {
        rows.push("abandoned (its prompt was edited and sent again)".to_string());
    }
    if rec.event_kind == "prompt" && rec.images > 0 {
        let plural = if rec.images == 1 { "" } else { "s" };
        rows.push(format!("{} image{plural}", rec.images));
    }
    if rec.event_kind == "say" && rec.final_reply {
        rows.push(if rec.provisional {
            "final reply · provisional".to_string()
        } else {
            "final reply".to_string()
        });
    } else if rec.event_kind == "say" && rec.provisional {
        rows.push("provisional".to_string());
    }
    if rec.event_kind == "think" && rec.redacted {
        rows.push("redacted (signature only)".to_string());
    }
    if rec.event_kind == "tool" {
        if let Some(status_row) = match rec.status.as_deref() {
            Some("pending") => Some("pending"),
            Some("background") => Some("running in background"),
            Some("error") => Some("error"),
            Some("interrupted") => Some("interrupted"),
            _ => None,
        } {
            rows.push(status_row.to_string());
        }
        if !rec.sub_sessions.is_empty() {
            rows.push(format!("started {}", rec.sub_sessions.join(", ")));
        }
    }
    rows
}

// ── categorization & subset matching ─────────────────────────────────────────

fn meta_bytes(rec: &EventRecord) -> Option<f64> {
    rec.meta
        .as_ref()?
        .get("bytes")?
        .as_f64()
        .filter(|&b| b.is_finite() && b > 0.0)
}

/// Whether a tool record is an image read (`Read` carrying positive `meta.bytes`).
pub fn is_image_tool_read(rec: &EventRecord) -> bool {
    rec.event_kind == "tool" && rec.action == "read" && meta_bytes(rec).is_some()
}

/// Categories an [`EventRecord`] belongs to (`wallAgentView.categoriesOfRecord`).
pub fn categories_of_record(rec: &EventRecord) -> Vec<&'static str> {
    match rec.event_kind.as_str() {
        "prompt" => vec!["prompt"],
        "think" => vec!["think"],
        "say" => vec!["say"],
        "system" | "request" => vec!["system"],
        "tool" => match rec.action.to_ascii_lowercase().as_str() {
            "noise" => vec!["noise"],
            "read" => {
                if is_image_tool_read(rec) {
                    vec!["image"]
                } else {
                    vec!["read"]
                }
            }
            "edit" | "write" => vec!["edit"],
            "bash" | "task" => vec!["run"],
            "grep" | "glob" | "search" | "fetch" => vec!["search"],
            _ => vec!["ask"],
        },
        _ => vec!["system"],
    }
}

/// Expand one or more category/group tokens into validated category ids (`expandCategoryTokens`).
pub fn expand_category_tokens<S: AsRef<str>>(tokens: &[S]) -> Result<Vec<&'static str>, String> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut push_unique = |id: &'static str| {
        if !out.contains(&id) {
            out.push(id);
        }
    };
    for raw in tokens {
        for tok in raw
            .as_ref()
            .to_lowercase()
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
        {
            if let Some(group_ids) = category_group(tok) {
                for &id in group_ids {
                    push_unique(id);
                }
            } else if let Some(&id) = CATEGORY_IDS.iter().find(|&&cid| cid == tok) {
                push_unique(id);
            } else {
                return Err(format!(
                    "unknown action category '{tok}' (categories: {}; groups: all, conversation, tools, reads, files, messages, system_all)",
                    CATEGORY_IDS.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

/// Normalize a partial or persisted filter map against [`default_filters`] (`normalizeFilters`).
pub fn normalize_filters(raw: Option<&FilterMap>) -> FilterMap {
    let mut out = default_filters();
    if let Some(map) = raw {
        for &id in CATEGORY_IDS {
            if let Some(&val) = map.get(id) {
                out.insert(id.to_string(), val);
            }
        }
    }
    out
}

/// Whether `filters` currently enables every standard category (`isAllStandardSelected`).
pub fn is_all_standard_selected(filters: &FilterMap) -> bool {
    STANDARD_CATEGORY_IDS
        .iter()
        .all(|&id| filters.get(id).copied().unwrap_or(false))
}

/// Whether `filters` equals [`default_filters`] (`isDefaultFilter`).
pub fn is_default_filter(filters: &FilterMap) -> bool {
    is_all_standard_selected(filters) && !filters.get("noise").copied().unwrap_or(false)
}

/// Tri-state selection state (`all` | `some` | `none`) for a category group (`groupSelectionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSelection {
    All,
    Some,
    None,
}

impl GroupSelection {
    pub const fn as_str(self) -> &'static str {
        match self {
            GroupSelection::All => "all",
            GroupSelection::Some => "some",
            GroupSelection::None => "none",
        }
    }
}

/// Compute [`GroupSelection`] of `category_ids` within `filters` (`groupSelectionState`).
pub fn group_selection_state<S: AsRef<str>>(
    filters: &FilterMap,
    category_ids: &[S],
) -> GroupSelection {
    let mut on_count = 0usize;
    for id in category_ids {
        if filters.get(id.as_ref()).copied().unwrap_or(false) {
            on_count += 1;
        }
    }
    if on_count == 0 {
        GroupSelection::None
    } else if on_count == category_ids.len() {
        GroupSelection::All
    } else {
        GroupSelection::Some
    }
}

/// Whether `filters` has ONLY `target_ids` enabled across [`CATEGORY_IDS`] (`isExactFilter`).
pub fn is_exact_filter<S: AsRef<str>>(filters: &FilterMap, target_ids: &[S]) -> bool {
    CATEGORY_IDS.iter().all(|&id| {
        let enabled = filters.get(id).copied().unwrap_or(false);
        let in_target = target_ids.iter().any(|t| t.as_ref() == id);
        enabled == in_target
    })
}

/// Human-readable summary of `filters` for the collapsed dropdown trigger (`describeActiveFilter`).
pub fn describe_active_filter(filters: &FilterMap) -> String {
    if is_all_standard_selected(filters) {
        return if filters.get("noise").copied().unwrap_or(false) {
            "all + noise".to_string()
        } else {
            "all actions".to_string()
        };
    }
    let enabled: Vec<&str> = CATEGORY_IDS
        .iter()
        .copied()
        .filter(|&id| filters.get(id).copied().unwrap_or(false))
        .collect();
    if enabled.is_empty() {
        return "none".to_string();
    }
    if is_exact_filter(filters, GROUP_CONVERSATION) {
        return "conversation".to_string();
    }
    if is_exact_filter(filters, GROUP_MESSAGES) {
        return "messages".to_string();
    }
    if is_exact_filter(filters, GROUP_TOOLS) {
        return "tool calls".to_string();
    }
    if is_exact_filter(filters, GROUP_READS) {
        return "file & img reads".to_string();
    }
    if is_exact_filter(filters, GROUP_FILES) {
        return "file ops".to_string();
    }
    if is_exact_filter(filters, GROUP_SYSTEM_ALL) {
        return "system & noise".to_string();
    }
    if enabled.len() == 1 {
        return category_by_id(enabled[0])
            .map(|c| c.short.to_lowercase())
            .unwrap_or_else(|| enabled[0].to_string());
    }
    if enabled.len() == 2 {
        return format!("{} + {}", enabled[0], enabled[1]);
    }
    format!("{} categories", enabled.len())
}

/// Mutation mode accepted by [`apply_filter_change`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    Reset,
    Only,
    On,
    Off,
    Toggle,
}

impl FilterMode {
    pub fn parse(mode: &str) -> Result<Self, String> {
        match mode {
            "reset" => Ok(FilterMode::Reset),
            "only" => Ok(FilterMode::Only),
            "on" => Ok(FilterMode::On),
            "off" => Ok(FilterMode::Off),
            "toggle" => Ok(FilterMode::Toggle),
            other => Err(format!(
                "filter mode takes reset|only|on|off|toggle, not '{other}'"
            )),
        }
    }
}

/// Apply a filter mutation (`reset` | `only` | `on` | `off` | `toggle`) and return a fresh map
/// (`wallAgentView.applyFilterChange`).
pub fn apply_filter_change<S: AsRef<str>>(
    current: Option<&FilterMap>,
    mode: FilterMode,
    tokens: &[S],
) -> Result<FilterMap, String> {
    let cur = normalize_filters(current);
    if mode == FilterMode::Reset {
        return Ok(default_filters());
    }
    let cats = expand_category_tokens(tokens)?;
    if cats.is_empty() {
        return Err(
            "name at least one category or group (e.g. tools, reads, read, image, edit, run, think, say, prompt, noise)"
                .to_string(),
        );
    }
    if mode == FilterMode::Only {
        let has_all = tokens
            .iter()
            .any(|t| t.as_ref().eq_ignore_ascii_case("all"));
        if has_all {
            return Ok(CATEGORY_IDS
                .iter()
                .map(|&id| (id.to_string(), true))
                .collect());
        }
        let mut next: FilterMap = CATEGORY_IDS
            .iter()
            .map(|&id| (id.to_string(), cats.contains(&id)))
            .collect();
        if !cats.contains(&"noise") {
            next.insert("noise".to_string(), false);
        }
        return Ok(next);
    }
    let mut next = cur;
    for id in cats {
        match mode {
            FilterMode::On => {
                next.insert(id.to_string(), true);
            }
            FilterMode::Off => {
                next.insert(id.to_string(), false);
            }
            FilterMode::Toggle => {
                let prev = next.get(id).copied().unwrap_or(false);
                next.insert(id.to_string(), !prev);
            }
            FilterMode::Reset | FilterMode::Only => unreachable!(),
        }
    }
    Ok(next)
}

/// Whether `rec` passes the active `filters` map (`wallAgentView.matchesFilter`).
pub fn matches_filter(rec: &EventRecord, filters: &FilterMap) -> bool {
    categories_of_record(rec)
        .into_iter()
        .any(|c| filters.get(c).copied().unwrap_or(false))
}

/// Whether `rec` matches a subset token list (`wallAgentView.matchesSubset`).
pub fn matches_subset<S: AsRef<str>>(rec: &EventRecord, tokens: &[S]) -> Result<bool, String> {
    if tokens.is_empty()
        || tokens
            .iter()
            .any(|t| t.as_ref().eq_ignore_ascii_case("all"))
    {
        return Ok(true);
    }
    let expanded = expand_category_tokens(tokens)?;
    Ok(categories_of_record(rec)
        .into_iter()
        .any(|c| expanded.contains(&c)))
}

// ── row & expanded content formatting ────────────────────────────────────────

/// Human-readable byte size (`formatBytes` in `wallAgentView.js`).
pub fn format_bytes(bytes: f64) -> String {
    if !bytes.is_finite() || bytes < 0.0 {
        return String::new();
    }
    if bytes < 1024.0 {
        return format!("{}B", js_round(bytes) as u64);
    }
    if bytes < 1024.0 * 1024.0 {
        let kb = bytes / 1024.0;
        if kb >= 10.0 {
            return format!("{}KB", js_round(kb) as u64);
        }
        return format!("{kb:.1}KB");
    }
    format!("{:.1}MB", bytes / (1024.0 * 1024.0))
}

fn one_line(text: &str, max_chars: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    for raw in text.split('\n') {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        let char_count = collapsed.chars().count();
        if char_count > max_chars {
            let prefix: String = collapsed
                .chars()
                .take(max_chars.saturating_sub(1))
                .collect();
            return format!("{prefix}…");
        }
        return collapsed;
    }
    String::new()
}

fn status_badge(status: Option<&str>) -> &'static str {
    match status {
        Some("pending") => "…",
        Some("background") => "bg",
        Some("error") => "err",
        Some("interrupted") => "int",
        _ => "",
    }
}

fn tool_metric(rec: &EventRecord, img_read: bool) -> String {
    if img_read {
        return meta_bytes(rec).map_or_else(String::new, format_bytes);
    }
    if rec.action == "read" {
        if let Some(range) = rec
            .meta
            .as_ref()
            .and_then(|m| m.get("range"))
            .and_then(|v| v.as_array())
        {
            if range.len() >= 2 {
                if let (Some(a), Some(b)) = (range[0].as_i64(), range[1].as_i64()) {
                    return format!("L{a}–{b}");
                }
            }
        }
        if let Some(total) = rec.file.as_ref().and_then(|f| f.total).filter(|&t| t > 0) {
            return format!("{total}L");
        }
    }
    if rec.action == "edit" {
        if let Some(meta) = rec.meta.as_ref().and_then(|m| m.as_object()) {
            let added = meta.get("added").filter(|v| !v.is_null());
            let removed = meta.get("removed").filter(|v| !v.is_null());
            if added.is_some() || removed.is_some() {
                let a = added.and_then(|v| v.as_i64()).unwrap_or(0);
                let r = removed.and_then(|v| v.as_i64()).unwrap_or(0);
                return format!("+{a} −{r}");
            }
        }
    }
    if rec.action == "write" {
        if let Some(total) = rec.file.as_ref().and_then(|f| f.total).filter(|&t| t > 0) {
            return format!("{total}L");
        }
    }
    String::new()
}

/// Compact 1-line row descriptor returned by [`summarize_event_row`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRowSummary {
    pub id: String,
    pub event_kind: String,
    pub action: String,
    pub categories: Vec<&'static str>,
    pub hue: String,
    pub tag: String,
    pub summary: String,
    pub stat: String,
    pub status: String,
    pub abandoned: bool,
    pub turn_index: Option<i64>,
    pub agent: Option<String>,
    pub sub_sessions: Vec<String>,
}

/// Format an [`EventRecord`] into a compact 1-line row descriptor (`summarizeEventRow`).
pub fn summarize_event_row(rec: &EventRecord, abandoned: bool) -> EventRowSummary {
    let categories = categories_of_record(rec);
    let img_read = is_image_tool_read(rec);
    let hue_obj = if img_read {
        IMAGE_HUE
    } else {
        action_hue(kind_for_action(&rec.action))
    };
    let hue = css_hue(hue_obj);

    let mut tag = if !rec.action.is_empty() {
        rec.action.clone()
    } else if !rec.event_kind.is_empty() {
        rec.event_kind.clone()
    } else {
        "event".to_string()
    };
    let summary: String;
    let mut stat = String::new();

    match rec.event_kind.as_str() {
        "prompt" => {
            if let Some(cmd) = rec.command.as_deref().filter(|s| !s.is_empty()) {
                tag = if cmd == "!" {
                    "!cmd".to_string()
                } else {
                    cmd.to_string()
                };
            } else {
                let raw_tag = rec.tag.as_deref().filter(|s| !s.is_empty()).unwrap_or("you");
                tag = raw_tag
                    .split('·')
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join("·");
            }
            let line = one_line(&rec.result, 140);
            summary = if !line.is_empty() {
                line
            } else if rec.images > 0 {
                let plural = if rec.images == 1 { "" } else { "s" };
                format!("({} attached image{plural})", rec.images)
            } else {
                "(empty prompt)".to_string()
            };
            if rec.images > 0 {
                stat = format!("{} img", rec.images);
            }
        }
        "think" => {
            tag = "think".to_string();
            let line = one_line(&rec.result, 140);
            summary = if rec.redacted && rec.result.is_empty() {
                "(redacted thinking)".to_string()
            } else if !line.is_empty() {
                line
            } else {
                "(thinking…)".to_string()
            };
            if rec.redacted {
                stat = "redacted".to_string();
            } else if !rec.result.is_empty() {
                let n = rec.result.split('\n').count();
                if n > 1 {
                    stat = format!("{n}L");
                }
            }
        }
        "say" => {
            tag = if rec.provisional {
                "say…".to_string()
            } else {
                "say".to_string()
            };
            let line = one_line(&rec.result, 140);
            summary = if line.is_empty() {
                "(empty reply)".to_string()
            } else {
                line
            };
            if rec.final_reply {
                stat = "final".to_string();
            }
        }
        "tool" => {
            let a = if rec.action.is_empty() {
                "tool".to_string()
            } else {
                rec.action.to_lowercase()
            };
            if a == "noise" {
                tag = rec
                    .tag
                    .as_deref()
                    .or(rec.name.as_deref())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("noise")
                    .to_string();
            } else if img_read {
                tag = "image".to_string();
            } else {
                tag = a.clone();
            }

            if a == "grep" || a == "search" {
                let d = one_line(&rec.detail, 70);
                let mut parts = Vec::new();
                if !d.is_empty() {
                    parts.push(d);
                }
                if !rec.target.is_empty() {
                    parts.push(rec.target.clone());
                }
                summary = parts.join("  ");
            } else if !rec.target.is_empty() {
                summary = rec.target.clone();
            } else {
                let d = one_line(&rec.detail, 140);
                let r = one_line(&rec.result, 140);
                summary = if !d.is_empty() {
                    d
                } else if !r.is_empty() {
                    r
                } else if let Some(name) = rec.name.as_deref().filter(|s| !s.is_empty()) {
                    name.to_string()
                } else {
                    a
                };
            }

            let metric = tool_metric(rec, img_read);
            let sb = status_badge(rec.status.as_deref());
            stat = match (!metric.is_empty(), !sb.is_empty()) {
                (true, true) => format!("{metric} · {sb}"),
                (true, false) => metric,
                (false, true) => sb.to_string(),
                (false, false) => String::new(),
            };
        }
        "system" => {
            tag = rec
                .subtype
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("system")
                .to_string();
            let r = one_line(&rec.result, 140);
            summary = if !r.is_empty() {
                r
            } else if !rec.detail.is_empty() {
                rec.detail.clone()
            } else {
                "system event".to_string()
            };
        }
        "request" => {
            tag = "model".to_string();
            summary = if rec.detail.is_empty() {
                "request".to_string()
            } else {
                rec.detail.clone()
            };
        }
        _ => {
            let r = one_line(&rec.result, 140);
            summary = if !r.is_empty() {
                r
            } else if !rec.detail.is_empty() {
                rec.detail.clone()
            } else if !rec.target.is_empty() {
                rec.target.clone()
            } else {
                tag.clone()
            };
        }
    }

    EventRowSummary {
        id: rec.id.clone(),
        event_kind: rec.event_kind.clone(),
        action: rec.action.clone(),
        categories,
        hue,
        tag,
        summary,
        stat,
        status: rec.status.clone().unwrap_or_else(|| "ok".to_string()),
        abandoned,
        turn_index: rec.turn.as_ref().map(|t| t.index),
        agent: rec.agent.clone(),
        sub_sessions: rec.sub_sessions.clone(),
    }
}

/// Options controlling [`format_expanded_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpandOptions {
    pub max_lines: usize,
    pub full: bool,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        ExpandOptions {
            max_lines: 160,
            full: false,
        }
    }
}

/// Expanded detail body for an event row (`formatExpandedEvent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedEvent {
    pub facts: Vec<String>,
    pub target: String,
    pub detail: String,
    pub body: String,
    pub total_lines: usize,
    pub truncated: bool,
}

/// Format the expanded detail view for `rec` (`wallAgentView.formatExpandedEvent`).
pub fn format_expanded_event(rec: &EventRecord, opts: ExpandOptions) -> ExpandedEvent {
    let mut facts = event_facts(rec);
    if let Some(agent) = rec.agent.as_deref().filter(|s| !s.is_empty()) {
        facts.insert(0, format!("agent {agent}"));
    }
    if is_image_tool_read(rec) {
        if let Some(bytes) = meta_bytes(rec) {
            facts.push(format_bytes(bytes));
        }
    }

    let target = rec.target.clone();
    let detail = if !rec.detail.is_empty() && Some(rec.detail.as_str()) != rec.subtype.as_deref() {
        rec.detail.clone()
    } else {
        String::new()
    };

    if let Some(ref file) = rec.file {
        let start = file.start.max(1);
        let mut raw_lines: Vec<&str> = file.text.split('\n').collect();
        if raw_lines.len() > 1 && raw_lines.last() == Some(&"") {
            raw_lines.pop();
        }
        let width = (start + raw_lines.len().saturating_sub(1))
            .to_string()
            .len();
        let limit = if opts.full {
            raw_lines.len()
        } else {
            raw_lines.len().min(opts.max_lines)
        };
        let mut numbered = Vec::with_capacity(limit);
        for (i, line) in raw_lines.iter().take(limit).enumerate() {
            numbered.push(format!("{:>width$} │ {line}", start + i));
        }
        return ExpandedEvent {
            facts,
            target,
            detail,
            body: numbered.join("\n"),
            total_lines: raw_lines.len(),
            truncated: raw_lines.len() > limit,
        };
    }

    if rec.result.is_empty() {
        return ExpandedEvent {
            facts,
            target,
            detail,
            body: String::new(),
            total_lines: 0,
            truncated: false,
        };
    }
    let lines: Vec<&str> = rec.result.split('\n').collect();
    let limit = if opts.full {
        lines.len()
    } else {
        lines.len().min(opts.max_lines)
    };
    ExpandedEvent {
        facts,
        target,
        detail,
        body: lines[..limit].join("\n"),
        total_lines: lines.len(),
        truncated: lines.len() > limit,
    }
}

// ── world geometry, drag side picking & tiling/split footprints ──────────────

/// Configured Agent View world dimensions (`agentViewWidth`, `agentViewHeight`, `agentViewGap`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentViewDims {
    pub width: f64,
    pub height: f64,
    pub gap: f64,
}

impl AgentViewDims {
    pub const fn new(width: f64, height: f64, gap: f64) -> Self {
        AgentViewDims { width, height, gap }
    }
}

/// World-space rectangle of an Agent View attached to `side` of a terminal (`agentViewRect`).
///
/// On [`AgentSide::Right`] and [`AgentSide::Left`], the pane runs flush from `body_rect.min_y`
/// up to `body_rect.max_y + bar_height.max(0.0)`. On [`AgentSide::Top`] and [`AgentSide::Bottom`],
/// it matches the terminal's width (`body_rect.min_x .. body_rect.max_x`).
pub fn agent_view_rect(
    body_rect: Rect,
    bar_height: f64,
    side: AgentSide,
    dims: AgentViewDims,
) -> Rect {
    let top_y = body_rect.max_y + bar_height.max(0.0);
    match side {
        AgentSide::Left => Rect {
            min_x: body_rect.min_x - dims.gap - dims.width,
            max_x: body_rect.min_x - dims.gap,
            min_y: body_rect.min_y,
            max_y: top_y,
        },
        AgentSide::Top => Rect {
            min_x: body_rect.min_x,
            max_x: body_rect.max_x,
            min_y: top_y + dims.gap,
            max_y: top_y + dims.gap + dims.height,
        },
        AgentSide::Bottom => Rect {
            min_x: body_rect.min_x,
            max_x: body_rect.max_x,
            min_y: body_rect.min_y - dims.gap - dims.height,
            max_y: body_rect.min_y - dims.gap,
        },
        AgentSide::Right => Rect {
            min_x: body_rect.max_x + dims.gap,
            max_x: body_rect.max_x + dims.gap + dims.width,
            min_y: body_rect.min_y,
            max_y: top_y,
        },
    }
}

/// Zoom-invariant tile footprint returned by [`footprint_tile_metrics`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootprintTileMetrics {
    pub w: f64,
    pub h: f64,
    pub shift_x: f64,
    pub shift_y: f64,
    pub tile_box: Rect,
}

/// Zoom-invariant tile footprint metrics for tiled groups (`footprintTileMetrics`).
///
/// Placing the terminal's body at `(place.x + shift_x, place.y - shift_y)` keeps the terminal,
/// its title bar (up to [`BAR_ROOM`]), and its attached Agent View strictly inside `tile_box`.
pub fn footprint_tile_metrics(
    body_rect: Rect,
    side: Option<AgentSide>,
    dims: AgentViewDims,
) -> FootprintTileMetrics {
    let body_w = body_rect.max_x - body_rect.min_x;
    let body_h = body_rect.max_y - body_rect.min_y;
    let mut w = body_w;
    let mut h = body_h;
    let mut shift_x = 0.0;
    let mut shift_y = 0.0;
    match side {
        Some(AgentSide::Right) => {
            w = body_w + dims.gap + dims.width;
        }
        Some(AgentSide::Left) => {
            w = dims.width + dims.gap + body_w;
            shift_x = dims.width + dims.gap;
        }
        Some(AgentSide::Bottom) => {
            h = body_h + dims.gap + dims.height;
        }
        Some(AgentSide::Top) => {
            h = dims.height + dims.gap + body_h;
            shift_y = dims.height + dims.gap;
        }
        None => {}
    }
    let min_x = body_rect.min_x - shift_x;
    let max_y = body_rect.max_y + BAR_ROOM + shift_y;
    FootprintTileMetrics {
        w,
        h,
        shift_x,
        shift_y,
        tile_box: Rect {
            min_x,
            max_x: min_x + w,
            min_y: max_y - (h + BAR_ROOM),
            max_y,
        },
    }
}

/// Per-edge world inset reserved inside a split pane for an attached Agent View (`agentInset`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentInset {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

/// World-unit room an Agent View on `side` claims inside a split pane (`agentInset`).
pub fn agent_inset(side: Option<AgentSide>, dims: AgentViewDims) -> AgentInset {
    AgentInset {
        left: if side == Some(AgentSide::Left) {
            dims.width + dims.gap
        } else {
            0.0
        },
        right: if side == Some(AgentSide::Right) {
            dims.width + dims.gap
        } else {
            0.0
        },
        top: if side == Some(AgentSide::Top) {
            dims.height + dims.gap
        } else {
            0.0
        },
        bottom: if side == Some(AgentSide::Bottom) {
            dims.height + dims.gap
        } else {
            0.0
        },
    }
}

/// Pick which side of `window_rect` a dragged world point `pt` targets, using aspect-normalized
/// quadrants around the window's centre (`pickAgentSide`).
pub fn pick_agent_side(window_rect: Rect, pt: Point) -> AgentSide {
    let cx = (window_rect.min_x + window_rect.max_x) / 2.0;
    let cy = (window_rect.min_y + window_rect.max_y) / 2.0;
    let half_w = ((window_rect.max_x - window_rect.min_x) / 2.0).max(1e-6);
    let half_h = ((window_rect.max_y - window_rect.min_y) / 2.0).max(1e-6);
    let nx = (pt.x - cx) / half_w;
    let ny = (pt.y - cy) / half_h;
    if nx.abs() >= ny.abs() {
        if nx >= 0.0 {
            AgentSide::Right
        } else {
            AgentSide::Left
        }
    } else if ny >= 0.0 {
        AgentSide::Top
    } else {
        AgentSide::Bottom
    }
}

// ── session discovery & pairing ──────────────────────────────────────────────

fn percent_decode_utf8(input: &str) -> Result<String, std::string::FromUtf8Error> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                char::from(bytes[i + 1]).to_digit(16),
                char::from(bytes[i + 2]).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out)
}

/// Normalize a filesystem path for comparison (`normPath`): decodes OSC 7 `file://host/path`
/// URLs and strips trailing slashes (preserving root `/`).
pub fn norm_path(path: &str) -> String {
    let mut trimmed = path.trim().to_string();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        if let Some(slash_idx) = rest.find('/') {
            let url_path = &rest[slash_idx..];
            trimmed = percent_decode_utf8(url_path).unwrap_or_else(|_| url_path.to_string());
        }
    }
    if trimmed == "/" {
        return "/".to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m_adj = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an RFC3339 / ISO-8601 timestamp into epoch milliseconds (`0` when absent or malformed).
fn parse_rfc3339_ms(raw: &str) -> i64 {
    let s = raw.trim();
    if s.len() < 19 {
        return 0;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return 0;
    }
    let Ok(year) = s[0..4].parse::<i64>() else { return 0 };
    let Ok(month) = s[5..7].parse::<i64>() else { return 0 };
    let Ok(day) = s[8..10].parse::<i64>() else { return 0 };
    let Ok(hour) = s[11..13].parse::<i64>() else { return 0 };
    let Ok(minute) = s[14..16].parse::<i64>() else { return 0 };
    let Ok(second) = s[17..19].parse::<i64>() else { return 0 };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return 0;
    }

    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if frac_len == 0 {
            return 0;
        }
        let digits = &after_dot[..frac_len];
        let mut ms_acc = 0i64;
        for idx in 0..3 {
            let d = digits
                .as_bytes()
                .get(idx)
                .map_or(0, |b| i64::from(b - b'0'));
            ms_acc = ms_acc * 10 + d;
        }
        millis = ms_acc;
        rest = &after_dot[frac_len..];
    }

    let tz_offset_ms = if rest.is_empty() || rest.eq_ignore_ascii_case("Z") {
        0i64
    } else if let Some(sign) = match rest.as_bytes().first() {
        Some(b'+') => Some(1i64),
        Some(b'-') => Some(-1i64),
        _ => None,
    } {
        let tz = &rest[1..];
        let (tz_h, tz_m) = if let Some((h_str, m_str)) = tz.split_once(':') {
            let Ok(h) = h_str.parse::<i64>() else { return 0 };
            let Ok(m) = m_str.parse::<i64>() else { return 0 };
            (h, m)
        } else if tz.len() == 4 {
            let Ok(h) = tz[0..2].parse::<i64>() else { return 0 };
            let Ok(m) = tz[2..4].parse::<i64>() else { return 0 };
            (h, m)
        } else {
            return 0;
        };
        sign * (tz_h * 3600 + tz_m * 60) * 1000
    } else {
        return 0;
    };

    let days = days_from_civil(year, month, day);
    ((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000 + millis) - tz_offset_ms
}

/// Workgroup metadata on an [`AgentSession`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkgroupMeta {
    pub name: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub store: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

/// Parent session reference set on a subagent's [`AgentSession`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentSessionMeta {
    pub session_id: String,
    #[serde(default)]
    pub tool_id: Option<String>,
}

/// Agent session descriptor from `agent sessions --json` (`AgentSessionProvider.AgentSession`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub workgroup: Option<WorkgroupMeta>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub end_reason: Option<String>,
    #[serde(default)]
    pub status_at: Option<String>,
    #[serde(default)]
    pub parent_session: Option<ParentSessionMeta>,
    #[serde(default)]
    pub continued_in: Option<String>,
}

/// Terminal metadata inspected by [`resolve_terminal_session`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalSessionMeta {
    pub id: Option<String>,
    pub attached: Option<String>,
    pub session_id: Option<String>,
    pub pwd: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// How [`resolve_terminal_session`] matched a terminal to its [`AgentSession`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveVia {
    Session,
    Workgroup,
    Directory,
}

impl ResolveVia {
    pub const fn as_str(self) -> &'static str {
        match self {
            ResolveVia::Session => "session",
            ResolveVia::Workgroup => "workgroup",
            ResolveVia::Directory => "directory",
        }
    }
}

/// Result of [`resolve_terminal_session`]: the paired session and match path, or why none matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSession {
    pub session: Option<AgentSession>,
    pub via: Option<ResolveVia>,
    pub missing: Option<String>,
}

fn gx_agent_name(label: &str) -> Option<&str> {
    let rest = label.trim().strip_prefix("gx-")?;
    if rest.is_empty() {
        return None;
    }
    if rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        Some(rest)
    } else {
        None
    }
}

/// Pair a terminal (`term_meta`) with its own [`AgentSession`], or explain why none can be
/// chosen (`wallAgentView.resolveTerminalSession`).
pub fn resolve_terminal_session(
    term_meta: &TerminalSessionMeta,
    sessions: &[AgentSession],
) -> ResolvedSession {
    let found = |session: &AgentSession, via: ResolveVia| ResolvedSession {
        session: Some(session.clone()),
        via: Some(via),
        missing: None,
    };
    let none = |missing: String| ResolvedSession {
        session: None,
        via: None,
        missing: Some(missing),
    };

    // Live first, newest first.
    let mut candidates: Vec<&AgentSession> = sessions.iter().collect();
    candidates.sort_by(|a, b| {
        let a_live = i32::from(a.status != "ended");
        let b_live = i32::from(b.status != "ended");
        b_live
            .cmp(&a_live)
            .then_with(|| parse_rfc3339_ms(&b.mtime).cmp(&parse_rfc3339_ms(&a.mtime)))
    });

    if let Some(sid) = term_meta.session_id.as_deref().filter(|s| !s.is_empty()) {
        if let Some(by_sid) = candidates.iter().find(|s| s.session_id == sid) {
            return found(by_sid, ResolveVia::Session);
        }
    }

    let mut agent_name = String::new();
    if let Some(raw) = term_meta
        .attached
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        agent_name = raw
            .split_once('@')
            .map_or(raw, |(name, _)| name)
            .to_string();
    } else {
        for label in [term_meta.id.as_deref(), term_meta.title.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Some(matched) = gx_agent_name(label) {
                agent_name = matched.to_string();
                break;
            }
        }
    }

    if !agent_name.is_empty() {
        if let Some(wg) = candidates.iter().find(|s| {
            s.parent_session.is_none()
                && s.workgroup
                    .as_ref()
                    .is_some_and(|w| w.name == agent_name)
        }) {
            return found(wg, ResolveVia::Workgroup);
        }
        return none(format!(
            "attached to agent '{agent_name}', but no session of it is listed"
        ));
    }

    let pwd_norm = term_meta.pwd.as_deref().map_or_else(String::new, norm_path);
    let dir = if !pwd_norm.is_empty() {
        pwd_norm
    } else {
        term_meta.cwd.as_deref().map_or_else(String::new, norm_path)
    };
    if dir.is_empty() {
        return none("this terminal hasn't reported its directory".to_string());
    }

    let here: Vec<&&AgentSession> = candidates
        .iter()
        .filter(|s| {
            s.parent_session.is_none()
                && s.status != "ended"
                && s.cwd.as_deref().map_or_else(String::new, norm_path) == dir
        })
        .collect();
    if here.len() == 1 {
        return found(here[0], ResolveVia::Directory);
    }
    if here.len() > 1 {
        return none(format!(
            "{} live agents in {dir}; can't tell which runs here",
            here.len()
        ));
    }
    none(format!("no live agent in {dir}"))
}

fn short_dir(cwd: &str) -> String {
    let n = norm_path(cwd);
    if n.is_empty() {
        return String::new();
    }
    n.rsplit('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(&n)
        .to_string()
}

/// Concise human-readable label for a session in the Agent View header (`formatSessionLabel`).
pub fn format_session_label(session: Option<&AgentSession>) -> String {
    let Some(s) = session else {
        return "no session".to_string();
    };
    let who = s
        .workgroup
        .as_ref()
        .map(|w| w.name.as_str())
        .filter(|n| !n.is_empty())
        .or_else(|| s.agent_name.as_deref().filter(|n| !n.is_empty()))
        .or_else(|| (!s.harness.is_empty()).then_some(s.harness.as_str()))
        .unwrap_or("agent");
    let short_id: String = s.session_id.chars().take(8).collect();
    let title = if let Some(t) = s.title.as_deref().filter(|t| !t.is_empty()) {
        one_line(t, 34)
    } else if let Some(proj) = s.project.as_deref().filter(|p| !p.is_empty()) {
        proj.to_string()
    } else {
        let dir = s.cwd.as_deref().map_or_else(String::new, short_dir);
        if !dir.is_empty() {
            dir
        } else {
            short_id.clone()
        }
    };
    let sub = if s.parent_session.is_some() {
        "sub · "
    } else {
        ""
    };
    let shown_title = if title.is_empty() { &short_id } else { &title };
    format!("{sub}{who}: {shown_title}")
}

// ── terminal scrollback line anchors & jump math ─────────────────────────────

/// Read a protocol-supplied terminal line number from a raw `EVENTS.md` JSON frame
/// (`extractProtocolTerminalLine`).
pub fn extract_protocol_terminal_line(ev: &serde_json::Value) -> Option<u64> {
    let obj = ev.as_object()?;
    let kind = obj.get("kind").and_then(|v| v.as_str());
    let candidates = [
        obj.get("terminal_line"),
        obj.get("turn").and_then(|t| t.get("terminal_line")),
        kind.and_then(|k| obj.get(k))
            .and_then(|b| b.get("terminal_line")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(n) = candidate.as_f64() {
            if n.is_finite() && n >= 0.0 {
                return Some(n.floor() as u64);
            }
        }
    }
    None
}

/// Stored terminal line anchor of an event (`wallAgentView.LineAnchor`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LineAnchor {
    pub terminal_line: Option<f64>,
    pub scrollback_rows: Option<f64>,
    pub cursor_row: Option<f64>,
    pub exact: Option<bool>,
}

/// Default context rows kept above the jumped-to line in [`compute_jump_offset`].
pub const DEFAULT_JUMP_CONTEXT_ROWS: f64 = 2.0;

/// Convert a stored [`LineAnchor`] into the `termd` `view` offset (`0` = live screen; `k > 0` =
/// `k` rows back from live) given the terminal's current `now_scrollback` row count
/// (`wallAgentView.computeJumpOffset`).
pub fn compute_jump_offset(
    anchor: Option<&LineAnchor>,
    now_scrollback: f64,
    context_rows: f64,
) -> u64 {
    let max_off = if now_scrollback.is_finite() && now_scrollback > 0.0 {
        now_scrollback.floor()
    } else {
        0.0
    };
    let Some(anchor) = anchor else {
        return 0;
    };
    if max_off == 0.0 {
        return 0;
    }
    let abs_row = match anchor.terminal_line.filter(|n| n.is_finite()) {
        Some(tl) => tl,
        None => {
            let sb = anchor
                .scrollback_rows
                .filter(|n| n.is_finite())
                .unwrap_or(0.0)
                .max(0.0);
            let cur = anchor
                .cursor_row
                .filter(|n| n.is_finite())
                .unwrap_or(0.0)
                .max(0.0);
            sb + cur
        }
    };
    let ctx = if context_rows.is_finite() {
        context_rows.max(0.0)
    } else {
        0.0
    };
    let desired_top = (abs_row - ctx).max(0.0);
    let raw_offset = max_off - desired_top;
    js_round(raw_offset).clamp(0.0, max_off) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WallConfig;
    use crate::layout::{overlaps, tile_layout, union_rect, Size, TileOptions};

    fn make_event(
        id: &str,
        at: i64,
        kind: &str,
        body: serde_json::Value,
        extra: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), serde_json::json!("event"));
        obj.insert("op".into(), serde_json::json!("add"));
        obj.insert("id".into(), serde_json::json!(id));
        obj.insert("seq".into(), serde_json::json!(1));
        obj.insert("at".into(), serde_json::json!(at));
        obj.insert("line".into(), serde_json::json!(1));
        obj.insert("ts".into(), serde_json::json!("2026-11-14T22:00:00Z"));
        obj.insert(
            "turn".into(),
            serde_json::json!({ "id": "turn-1", "index": 1 }),
        );
        obj.insert("kind".into(), serde_json::json!(kind));
        obj.insert(kind.into(), body);
        if let Some(serde_json::Value::Object(ex)) = extra {
            for (k, v) in ex {
                obj.insert(k, v);
            }
        }
        serde_json::Value::Object(obj)
    }

    #[test]
    fn categorizes_every_events_kind_and_distinguishes_text_reads_image_reads_and_noise() {
        let rec_of = |ev: serde_json::Value| event_record(&ev, None, true).unwrap().unwrap();

        let prompt_plain = rec_of(make_event(
            "e1",
            10,
            "prompt",
            serde_json::json!({ "origin": "human", "text": "check wall layout" }),
            None,
        ));
        let prompt_with_img = rec_of(make_event(
            "e2",
            20,
            "prompt",
            serde_json::json!({
                "origin": "human",
                "text": "see screenshot",
                "images": [{ "media_type": "image/png" }]
            }),
            None,
        ));
        let think_rec = rec_of(make_event(
            "e3",
            30,
            "think",
            serde_json::json!({ "text": "checking invariants\nline 2", "redacted": false }),
            None,
        ));
        let say_rec = rec_of(make_event(
            "e4",
            40,
            "say",
            serde_json::json!({ "text": "All good.", "final": true }),
            None,
        ));
        let file_read = rec_of(make_event(
            "e5",
            50,
            "tool",
            serde_json::json!({
                "name": "Read",
                "action": "read",
                "target": "wall/TerminalWall.js",
                "meta": { "range": [10, 40] },
                "file": { "text": "line ten\nline eleven\n", "start": 10, "total": 900 },
                "result": { "status": "ok", "output": "" }
            }),
            None,
        ));
        let img_read = rec_of(make_event(
            "e6",
            60,
            "tool",
            serde_json::json!({
                "name": "Read",
                "action": "read",
                "target": "docs/diagram.png",
                "meta": { "bytes": 43008 },
                "result": { "status": "ok", "output": "" }
            }),
            None,
        ));
        let edit_rec = rec_of(make_event(
            "e7",
            70,
            "tool",
            serde_json::json!({
                "name": "Edit",
                "action": "edit",
                "target": "wall/WallGroups.js",
                "meta": { "added": 14, "removed": 3 },
                "result": { "status": "ok", "output": "" }
            }),
            None,
        ));
        let bash_rec = rec_of(make_event(
            "e8",
            80,
            "tool",
            serde_json::json!({
                "name": "Bash",
                "action": "bash",
                "detail": "bun test tools/wall-*.test.mjs",
                "result": { "status": "ok", "output": "111 pass\n0 fail" }
            }),
            None,
        ));
        let grep_rec = rec_of(make_event(
            "e9",
            90,
            "tool",
            serde_json::json!({
                "name": "Grep",
                "action": "grep",
                "target": "wall/",
                "detail": "windowRect",
                "result": { "status": "ok", "output": "wall/TerminalWall.js:393" }
            }),
            None,
        ));
        let noise_rec = rec_of(make_event(
            "e10",
            100,
            "tool",
            serde_json::json!({
                "name": "TodoWrite",
                "action": "noise",
                "detail": "3 items",
                "result": { "status": "ok", "output": "" }
            }),
            None,
        ));

        assert_eq!(categories_of_record(&prompt_plain), vec!["prompt"]);
        assert_eq!(categories_of_record(&prompt_with_img), vec!["prompt"]);
        assert_eq!(categories_of_record(&think_rec), vec!["think"]);
        assert_eq!(categories_of_record(&say_rec), vec!["say"]);
        assert_eq!(categories_of_record(&file_read), vec!["read"]);
        assert_eq!(categories_of_record(&img_read), vec!["image"]);
        assert_eq!(categories_of_record(&edit_rec), vec!["edit"]);
        assert_eq!(categories_of_record(&bash_rec), vec!["run"]);
        assert_eq!(categories_of_record(&grep_rec), vec!["search"]);
        assert_eq!(categories_of_record(&noise_rec), vec!["noise"]);

        let defaults = default_filters();
        let all = [
            &prompt_plain,
            &prompt_with_img,
            &think_rec,
            &say_rec,
            &file_read,
            &img_read,
            &edit_rec,
            &bash_rec,
            &grep_rec,
            &noise_rec,
        ];
        let matched_default: Vec<&str> = all
            .iter()
            .filter(|r| matches_filter(r, &defaults))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(
            matched_default,
            vec!["e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9"]
        );

        let only_tools =
            apply_filter_change(Some(&defaults), FilterMode::Only, &["tools"]).unwrap();
        let matched_tools: Vec<&str> = all
            .iter()
            .filter(|r| matches_filter(r, &only_tools))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(matched_tools, vec!["e5", "e6", "e7", "e8", "e9"]);

        let only_images =
            apply_filter_change(Some(&defaults), FilterMode::Only, &["image"]).unwrap();
        let matched_images: Vec<&str> = all
            .iter()
            .filter(|r| matches_filter(r, &only_images))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(matched_images, vec!["e6"]);

        let only_reads =
            apply_filter_change(Some(&defaults), FilterMode::Only, &["reads"]).unwrap();
        let matched_reads: Vec<&str> = all
            .iter()
            .filter(|r| matches_filter(r, &only_reads))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(matched_reads, vec!["e5", "e6"]);

        assert!(expand_category_tokens(&["bogus"])
            .unwrap_err()
            .contains("unknown action category 'bogus'"));

        let row_read = summarize_event_row(&file_read, false);
        assert_eq!(row_read.tag, "read");
        assert_eq!(row_read.summary, "wall/TerminalWall.js");
        assert_eq!(row_read.stat, "L10–40");

        let row_img = summarize_event_row(&img_read, false);
        assert_eq!(row_img.tag, "image");
        assert_eq!(row_img.summary, "docs/diagram.png");
        assert_eq!(row_img.stat, "42KB");

        let row_edit = summarize_event_row(&edit_rec, false);
        assert_eq!(row_edit.tag, "edit");
        assert_eq!(row_edit.summary, "wall/WallGroups.js");
        assert_eq!(row_edit.stat, "+14 −3");

        let row_say = summarize_event_row(&say_rec, true);
        assert_eq!(row_say.tag, "say");
        assert_eq!(row_say.stat, "final");
        assert!(row_say.abandoned);

        let expanded = format_expanded_event(&file_read, ExpandOptions::default());
        assert_eq!(expanded.body, "10 │ line ten\n11 │ line eleven");
        assert!(matches_subset(&img_read, &["reads"]).unwrap());
        assert!(!matches_subset(&bash_rec, &["reads"]).unwrap());

        let mut seen_in_tree: Vec<&str> = Vec::new();
        for g in CATEGORY_GROUPS_TREE {
            for sub in g.subgroups {
                seen_in_tree.extend_from_slice(sub.categories);
            }
            seen_in_tree.extend_from_slice(g.direct_categories);
        }
        assert_eq!(seen_in_tree, CATEGORY_IDS);
        for &cid in CATEGORY_IDS {
            let hue = category_hue(cid);
            assert_eq!(hue.len(), 7);
            assert!(hue.starts_with('#'));
            assert!(hue[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        }

        assert_eq!(describe_active_filter(&defaults), "all actions");
        let with_noise = apply_filter_change(Some(&defaults), FilterMode::On, &["noise"]).unwrap();
        assert_eq!(describe_active_filter(&with_noise), "all + noise");
        assert_eq!(describe_active_filter(&only_tools), "tool calls");
        assert_eq!(describe_active_filter(&only_reads), "file & img reads");
        assert_eq!(describe_active_filter(&only_images), "image reads");
        let only_conv =
            apply_filter_change(Some(&defaults), FilterMode::Only, &["conversation"]).unwrap();
        assert_eq!(describe_active_filter(&only_conv), "conversation");
        let edit_and_run =
            apply_filter_change(Some(&defaults), FilterMode::Only, &["edit", "run"]).unwrap();
        assert_eq!(describe_active_filter(&edit_and_run), "edit + run");
        assert_eq!(
            group_selection_state(&defaults, &["prompt", "think", "say"]),
            GroupSelection::All
        );
        assert_eq!(
            group_selection_state(&defaults, &["system", "noise"]),
            GroupSelection::Some
        );
        assert_eq!(
            group_selection_state(&only_reads, &["prompt", "think", "say"]),
            GroupSelection::None
        );
    }

    #[test]
    fn agent_view_rect_pick_agent_side_and_tile_footprints_across_all_4_sides() {
        let body = Rect::new(100.0, -58.0, 216.0, 0.0);
        let dims = AgentViewDims::new(76.0, 44.0, 1.5);

        for &side in AGENT_SIDES {
            let ar = agent_view_rect(body, BAR_ROOM, side, dims);
            let win = Rect::new(body.min_x, body.min_y, body.max_x, body.max_y + BAR_ROOM);
            assert!(!overlaps(ar, win));

            let m = footprint_tile_metrics(body, Some(side), dims);
            let full = union_rect(&[win, ar]).unwrap();
            assert!((full.min_x - m.tile_box.min_x).abs() < 1e-6);
            assert!((full.max_x - m.tile_box.max_x).abs() < 1e-6);
            assert!((full.min_y - m.tile_box.min_y).abs() < 1e-6);
            assert!((full.max_y - m.tile_box.max_y).abs() < 1e-6);
        }

        let win = Rect::new(0.0, -60.0, 120.0, 10.0); // centre = (60, -25)
        assert_eq!(pick_agent_side(win, Point::new(140.0, -25.0)), AgentSide::Right);
        assert_eq!(pick_agent_side(win, Point::new(-20.0, -25.0)), AgentSide::Left);
        assert_eq!(pick_agent_side(win, Point::new(60.0, 30.0)), AgentSide::Top);
        assert_eq!(pick_agent_side(win, Point::new(60.0, -80.0)), AgentSide::Bottom);

        // Pure tiling verification for 4 terminals wearing Agent Views on all 4 distinct sides:
        let sides = [
            AgentSide::Right,
            AgentSide::Left,
            AgentSide::Top,
            AgentSide::Bottom,
        ];
        let base_body = Rect::from_top_left(0.0, 0.0, 100.0, 50.0);
        let metrics: Vec<FootprintTileMetrics> = sides
            .iter()
            .map(|&s| footprint_tile_metrics(base_body, Some(s), dims))
            .collect();
        let sizes: Vec<Size> = metrics.iter().map(|m| Size::new(m.w, m.h)).collect();
        let tiled = tile_layout(&sizes, TileOptions::new(Point::new(0.0, 0.0), 2, 4.0)).unwrap();
        let footprints: Vec<Rect> = tiled
            .places
            .iter()
            .zip(metrics.iter())
            .zip(sides.iter())
            .map(|((place, m), &side)| {
                let placed_body =
                    Rect::from_top_left(place.x + m.shift_x, place.y - m.shift_y, 100.0, 50.0);
                let placed_win = Rect::new(
                    placed_body.min_x,
                    placed_body.min_y,
                    placed_body.max_x,
                    placed_body.max_y + BAR_ROOM,
                );
                let placed_av = agent_view_rect(placed_body, BAR_ROOM, side, dims);
                union_rect(&[placed_win, placed_av]).unwrap()
            })
            .collect();
        for i in 0..footprints.len() {
            for j in (i + 1)..footprints.len() {
                assert!(!overlaps(footprints[i], footprints[j]));
            }
        }
        let cfg = WallConfig::default();
        let all_union = union_rect(&footprints).unwrap();
        let extent = tiled.extent.unwrap();
        assert!((extent.min_x - all_union.min_x).abs() < 1e-6);
        assert!((extent.max_x - all_union.max_x).abs() < 1e-6);
        assert!((extent.min_y - all_union.min_y).abs() < 1e-6);
        assert!((extent.max_y - all_union.max_y).abs() < 1e-6);
        assert!(cfg.group_pad > 0.0);

        assert_eq!(
            agent_inset(Some(AgentSide::Left), dims),
            AgentInset { left: 77.5, right: 0.0, top: 0.0, bottom: 0.0 }
        );
        assert_eq!(agent_inset(None, dims), AgentInset::default());
    }

    #[test]
    fn resolve_terminal_session_pairs_exact_session_workgroup_or_directory() {
        let sess = |id: &str, cwd: &str, status: &str, mtime: &str| AgentSession {
            session_id: id.to_string(),
            harness: "claude".to_string(),
            cwd: Some(cwd.to_string()),
            status: status.to_string(),
            mtime: mtime.to_string(),
            ..AgentSession::default()
        };
        let sessions = vec![
            sess("ended-1", "/repo/app", "ended", "2026-11-14T22:50:00Z"),
            sess("live-older", "/repo/app", "idle", "2026-11-14T21:00:00Z"),
            sess(
                "live-newer",
                "/repo/app",
                "processing",
                "2026-11-14T22:10:00Z",
            ),
            AgentSession {
                parent_session: Some(ParentSessionMeta {
                    session_id: "live-newer".to_string(),
                    tool_id: None,
                }),
                ..sess(
                    "sub-child",
                    "/repo/solo",
                    "processing",
                    "2026-11-14T22:55:00Z",
                )
            },
            sess("solo", "/repo/solo", "idle", "2026-11-14T22:00:00Z"),
            sess("only-ended", "/repo/old", "ended", "2026-11-14T20:00:00Z"),
            AgentSession {
                workgroup: Some(WorkgroupMeta {
                    name: "alpha".to_string(),
                    ..WorkgroupMeta::default()
                }),
                ..sess(
                    "wg-old",
                    "/workgroups/alpha",
                    "ended",
                    "2026-11-14T19:00:00Z",
                )
            },
            AgentSession {
                workgroup: Some(WorkgroupMeta {
                    name: "alpha".to_string(),
                    ..WorkgroupMeta::default()
                }),
                ..sess(
                    "wg-sess",
                    "/workgroups/alpha",
                    "idle",
                    "2026-11-14T20:00:00Z",
                )
            },
        ];

        let sid = |meta: TerminalSessionMeta| -> Option<String> {
            resolve_terminal_session(&meta, &sessions)
                .session
                .map(|s| s.session_id)
        };

        assert_eq!(
            resolve_terminal_session(
                &TerminalSessionMeta {
                    id: Some("gx-alpha".into()),
                    session_id: Some("live-older".into()),
                    cwd: Some("/repo/app".into()),
                    ..Default::default()
                },
                &sessions
            ),
            ResolvedSession {
                session: Some(sessions[1].clone()),
                via: Some(ResolveVia::Session),
                missing: None,
            }
        );

        assert_eq!(
            sid(TerminalSessionMeta {
                id: Some("term-9".into()),
                attached: Some("alpha@far-host".into()),
                cwd: Some("/repo/app".into()),
                ..Default::default()
            }),
            Some("wg-sess".into())
        );
        assert_eq!(
            sid(TerminalSessionMeta {
                id: Some("gx-alpha".into()),
                cwd: Some("/tmp".into()),
                ..Default::default()
            }),
            Some("wg-sess".into())
        );

        let gone = resolve_terminal_session(
            &TerminalSessionMeta {
                id: Some("gx-beta".into()),
                pwd: Some("/repo/solo".into()),
                ..Default::default()
            },
            &sessions,
        );
        assert_eq!(gone.session, None);
        assert!(gone.missing.unwrap().contains("'beta'"));

        let by_dir = resolve_terminal_session(
            &TerminalSessionMeta {
                id: Some("t1".into()),
                pwd: Some("/repo/solo/".into()),
                cwd: Some("/home/me".into()),
                ..Default::default()
            },
            &sessions,
        );
        assert_eq!(by_dir.via, Some(ResolveVia::Directory));
        assert_eq!(by_dir.session.unwrap().session_id, "solo");

        assert_eq!(
            sid(TerminalSessionMeta {
                id: Some("t1".into()),
                pwd: Some("file://some-host/repo/so%6Co".into()),
                cwd: Some("/home/me".into()),
                ..Default::default()
            }),
            Some("solo".into())
        );

        let two = resolve_terminal_session(
            &TerminalSessionMeta {
                id: Some("t1".into()),
                pwd: Some("/repo/app".into()),
                ..Default::default()
            },
            &sessions,
        );
        assert_eq!(two.session, None);
        assert!(two.missing.unwrap().contains("2 live agents"));

        assert_eq!(
            sid(TerminalSessionMeta {
                id: Some("t1".into()),
                pwd: Some("/repo/old".into()),
                ..Default::default()
            }),
            None
        );
        assert!(resolve_terminal_session(
            &TerminalSessionMeta {
                id: Some("t1".into()),
                ..Default::default()
            },
            &sessions
        )
        .missing
        .unwrap()
        .contains("directory"));
        assert!(resolve_terminal_session(
            &TerminalSessionMeta {
                id: Some("t1".into()),
                pwd: Some("/repo/solo".into()),
                ..Default::default()
            },
            &[]
        )
        .missing
        .unwrap()
        .contains("no live agent"));

        assert_eq!(format_session_label(None), "no session");
        assert_eq!(format_session_label(Some(&sessions[7])), "alpha: alpha");
    }

    #[test]
    fn jump_offset_and_protocol_terminal_line_extraction() {
        let anchor = LineAnchor {
            scrollback_rows: Some(120.0),
            cursor_row: Some(6.0),
            ..Default::default()
        };
        assert_eq!(compute_jump_offset(Some(&anchor), 180.0, 2.0), 56);

        let ev = make_event(
            "s-reply",
            170,
            "say",
            serde_json::json!({ "text": "Done reading!", "final": true }),
            Some(serde_json::json!({ "terminal_line": 150 })),
        );
        let proto_line = extract_protocol_terminal_line(&ev);
        assert_eq!(proto_line, Some(150));
        let proto_anchor = LineAnchor {
            terminal_line: proto_line.map(|n| n as f64),
            ..Default::default()
        };
        assert_eq!(compute_jump_offset(Some(&proto_anchor), 180.0, 2.0), 32);
        assert_eq!(compute_jump_offset(None, 180.0, 2.0), 0);
        assert_eq!(compute_jump_offset(Some(&anchor), 0.0, 2.0), 0);
    }
}
