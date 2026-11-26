//! All `wall.*` command-bus verbs, their argument validation, reply formatting,
//! and the single [`WallApi`] trait they drive.
//!
//! Ports `wall/wallCommands.js` onto [`crate::router::CommandRouter`], together
//! with the small pure helpers it imports from `WebSocketBridge.js`
//! (`normalizeRelayHost`, `parseRelayEndpoint`, `parseRelayTokenInput`) and
//! `WallBus.js` (`busStatus`, `busAttachable`, `gomuxScreenId`), plus a private
//! knob-schema helper standing in while `config.rs` and `settings.rs` are still
//! unbuilt stubs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::geom::Point;
use crate::router::{is_token_chars, CommandReply, CommandRouter, RegisterMeta};

/// Validated settings panel section ids (`SECTIONS` in `wall/wallSettings.js`).
pub const SETTINGS_SECTION_IDS: &[&str] = &[
    "canvas",
    "terminals",
    "agent",
    "fullscreen",
    "text",
    "groups",
    "status",
    "input",
    "selection",
    "menu",
];

const AGENT_SIDES: &[&str] = &["right", "left", "top", "bottom"];
const AGENT_MODES: &[&str] = &[
    "toggle", "open", "close", "on", "off", "right", "left", "top", "bottom",
];

fn fmt_num(n: f64) -> String {
    if n == 0.0 {
        "0".to_string()
    } else {
        n.to_string()
    }
}

/// JavaScript `Math.round`: ties round toward `+∞`, and `-0.0` normalizes to `0.0`.
fn js_round(x: f64) -> f64 {
    let r = (x + 0.5).floor();
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

fn int_arg(raw: Option<&String>, what: &str) -> Result<i64, String> {
    let disp = raw.map_or("undefined", String::as_str);
    let trimmed = disp.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let n: f64 = trimmed
        .parse()
        .map_err(|_| format!("{what} takes a whole number, not '{disp}'"))?;
    if !n.is_finite() || n.fract() != 0.0 {
        return Err(format!("{what} takes a whole number, not '{disp}'"));
    }
    Ok(n as i64)
}

fn num_arg(raw: Option<&String>, what: &str) -> Result<f64, String> {
    let Some(s) = raw else {
        return Err(format!("{what} takes a number, not 'undefined'"));
    };
    let trimmed = s.trim();
    let n: f64 = if trimmed.is_empty() {
        0.0
    } else {
        trimmed
            .parse()
            .map_err(|_| format!("{what} takes a number, not '{s}'"))?
    };
    if !n.is_finite() {
        return Err(format!("{what} takes a number, not '{s}'"));
    }
    Ok(n)
}

// ── Relay URL / `#token=` helpers (`WebSocketBridge.js`) ─────────────────────

/// Normalize a relay hostname (`normalizeRelayHost` in `WebSocketBridge.js`):
/// trim, lowercase, strip one pair of outer `[` `]` brackets, defaulting to `"localhost"`.
pub fn normalize_relay_host(host: Option<&str>) -> String {
    let mut h = host.unwrap_or("").trim().to_lowercase();
    if h.starts_with('[') && h.ends_with(']') && h.len() >= 2 {
        h = h[1..h.len() - 1].trim().to_string();
    }
    if h.is_empty() {
        "localhost".to_string()
    } else {
        h
    }
}

fn percent_decode_lossy(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.replace('+', " "))
}

fn has_query_token_param(raw: &str) -> bool {
    let before_hash = raw.split('#').next().unwrap_or(raw);
    let Some(q_idx) = before_hash.find('?') else {
        return false;
    };
    let query = &before_hash[q_idx + 1..];
    for part in query.split(['&', ';']) {
        if part.is_empty() {
            continue;
        }
        let raw_key = part.split('=').next().unwrap_or(part);
        let decoded = percent_decode_lossy(raw_key);
        if decoded.trim().eq_ignore_ascii_case("token") {
            return true;
        }
    }
    false
}

fn search_params_get(query: &str, want_key: &str) -> Option<String> {
    let q = query.strip_prefix('?').unwrap_or(query);
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        if percent_decode_lossy(k) == want_key {
            return Some(percent_decode_lossy(v));
        }
    }
    None
}

fn search_params_has(query: &str, want_key: &str) -> bool {
    search_params_get(query, want_key).is_some()
}

/// Extract a capability token from a raw RFC 7230 token or `#token=<tok>` fragment/URL,
/// rejecting `?token=` query leaks and tokenless URLs (`parseRelayTokenInput`).
pub fn parse_relay_token_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || has_query_token_param(trimmed) {
        return None;
    }
    let candidate = if let Some((_, frag)) = trimmed.split_once('#') {
        search_params_get(frag, "token").unwrap_or_default()
    } else if trimmed.starts_with("token=") {
        search_params_get(trimmed, "token").unwrap_or_default()
    } else if trimmed.contains("://") {
        return None;
    } else {
        trimmed.to_string()
    };
    let tok = candidate.trim();
    if is_token_chars(tok) {
        Some(tok.to_string())
    } else {
        None
    }
}

/// Canonicalized relay endpoint returned by [`parse_relay_endpoint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRelayEndpoint {
    pub host: String,
    pub port: u16,
    pub ws_url: String,
    pub token: Option<String>,
    pub had_hash_token: bool,
}

/// Parse a bare port or `ws`/`wss`/`http`/`https` relay URL (`parseRelayEndpoint` in
/// `WebSocketBridge.js`), never echoing secrets in error strings.
pub fn parse_relay_endpoint(
    input: &str,
    default_host: Option<&str>,
    default_https: bool,
) -> Result<ParsedRelayEndpoint, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("empty relay endpoint".into());
    }

    if raw.bytes().all(|b| b.is_ascii_digit()) {
        let port: u16 = raw
            .parse::<u16>()
            .ok()
            .filter(|&p| p >= 1)
            .ok_or_else(|| "invalid relay port (must be 1..65535)".to_string())?;
        let host = normalize_relay_host(default_host);
        let proto = if default_https { "wss:" } else { "ws:" };
        let url_host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
        return Ok(ParsedRelayEndpoint {
            host,
            port,
            ws_url: format!("{proto}//{url_host}:{port}"),
            token: None,
            had_hash_token: false,
        });
    }

    let (before_hash, frag_opt) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };
    let Some((scheme_raw, after_scheme)) = before_hash.split_once("://") else {
        return Err("invalid relay endpoint (expected port or ws/wss/http/https URL)".into());
    };
    let scheme = scheme_raw.to_lowercase();
    if !matches!(scheme.as_str(), "ws" | "wss" | "http" | "https") {
        return Err("unsupported relay URL scheme (expected ws, wss, http, or https)".into());
    }

    let (authority_and_path, query) = match after_scheme.split_once('?') {
        Some((ap, q)) => (ap, q),
        None => (after_scheme, ""),
    };
    let (authority, path) = match authority_and_path.find('/') {
        Some(idx) => (&authority_and_path[..idx], &authority_and_path[idx..]),
        None => (authority_and_path, "/"),
    };
    if authority.is_empty() {
        return Err("missing host in relay URL".into());
    }

    let (raw_host, explicit_port): (&str, Option<&str>) = if let Some(rest) =
        authority.strip_prefix('[')
    {
        let Some(close_idx) = rest.find(']') else {
            return Err("invalid relay endpoint (expected port or ws/wss/http/https URL)".into());
        };
        let ipv6 = &authority[..=close_idx + 1];
        let after_bracket = &rest[close_idx + 1..];
        if after_bracket.is_empty() {
            (ipv6, None)
        } else if let Some(p) = after_bracket.strip_prefix(':') {
            (ipv6, Some(p))
        } else {
            return Err("invalid relay endpoint (expected port or ws/wss/http/https URL)".into());
        }
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        if h.contains(':') {
            return Err("invalid relay endpoint (expected port or ws/wss/http/https URL)".into());
        }
        (h, Some(p))
    } else {
        (authority, None)
    };

    if raw_host.is_empty() {
        return Err("missing host in relay URL".into());
    }
    let parsed_explicit_port: Option<u16> = match explicit_port {
        Some("") | None => None,
        Some(p) => {
            if !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid port in relay URL".into());
            }
            let n = p
                .parse::<u16>()
                .ok()
                .filter(|&n| n >= 1)
                .ok_or_else(|| "invalid port in relay URL".to_string())?;
            Some(n)
        }
    };

    if has_query_token_param(raw) {
        return Err("relay token must not be passed in URL query string".into());
    }

    let mut token = None;
    let mut had_hash_token = false;
    if let Some(frag) = frag_opt {
        if search_params_has(frag, "token") {
            had_hash_token = true;
            let tok = search_params_get(frag, "token").unwrap_or_default();
            let tok_trimmed = tok.trim();
            if !is_token_chars(tok_trimmed) {
                return Err("invalid #token= in URL".into());
            }
            token = Some(tok_trimmed.to_string());
        }
    }

    let secure = scheme == "wss" || scheme == "https";
    let host = normalize_relay_host(Some(raw_host));
    let url_host = if host.contains(':') { format!("[{host}]") } else { host.clone() };

    if (scheme == "http" || scheme == "https") && search_params_has(query, "relay") {
        let relay_raw = search_params_get(query, "relay").unwrap_or_default();
        let trimmed_relay = relay_raw.trim();
        let port = if !trimmed_relay.is_empty() && trimmed_relay.bytes().all(|b| b.is_ascii_digit())
        {
            trimmed_relay.parse::<u16>().ok().filter(|&p| p >= 1)
        } else {
            None
        }
        .ok_or_else(|| "invalid ?relay= port in URL".to_string())?;
        return Ok(ParsedRelayEndpoint {
            host,
            port,
            ws_url: format!("{}//{url_host}:{port}", if secure { "wss:" } else { "ws:" }),
            token,
            had_hash_token,
        });
    }

    let port = parsed_explicit_port.unwrap_or(if secure { 443 } else { 80 });
    let ws_path = if (scheme == "ws" || scheme == "wss") && !path.is_empty() && path != "/" {
        path
    } else {
        ""
    };
    let port_part = match parsed_explicit_port {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let ws_url = format!(
        "{}//{url_host}{port_part}{ws_path}",
        if secure { "wss:" } else { "ws:" }
    );
    Ok(ParsedRelayEndpoint { host, port, ws_url, token, had_hash_token })
}

// ── Pure `WallBus.js` helpers used by `wall.bus` ─────────────────────────────

fn bus_status_word(status: Option<&str>) -> &'static str {
    match status {
        Some("waiting") => "waiting",
        Some("error") => "error",
        Some("processing") => "processing",
        Some("idle") => "idle",
        Some("ended") => "ended",
        _ => "unknown",
    }
}

/// Whether a listed gomuxai agent has a live session to attach to (`busAttachable` in `WallBus.js`).
pub fn bus_attachable(attached: Option<bool>) -> bool {
    attached != Some(false)
}

/// Display terminal id for a local gomuxai agent's direct `termd` subscription
/// (`gomuxScreenId` in `WallBus.js`).
pub fn gomux_screen_id(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(61)
        .collect();
    format!("gx-{cleaned}")
}

// ── Knob schema helpers backed by `config.rs` and `settings.rs` ──────────────

fn knob_value_to_json(val: crate::settings::KnobValue) -> Value {
    match val {
        crate::settings::KnobValue::Bool(b) => Value::Bool(b),
        crate::settings::KnobValue::Number(n) => json!(n),
        crate::settings::KnobValue::Text(s) => Value::String(s),
    }
}

/// Validate a knob value against the full wall knob schema (`config::check_config`).
pub fn check_fallback_knob(key: &str, raw: &str) -> Result<Value, String> {
    crate::config::check_config(key, raw).map(knob_value_to_json)
}

/// Default value of a knob from the wall configuration schema (`config::default_knob_value`).
pub fn default_fallback_knob(key: &str) -> Result<Value, String> {
    crate::config::default_knob_value(key)
        .map(knob_value_to_json)
        .ok_or_else(|| format!("unknown wall knob '{key}'"))
}

/// Search the wall knob schema (`settings::search_settings` for `wall.config.find`).
pub fn search_fallback_knobs(query: &str) -> Vec<ConfigSearchGroup> {
    crate::settings::search_settings(query)
        .into_iter()
        .map(|g| ConfigSearchGroup {
            section_id: g.section.id.to_string(),
            section_title: g.section.title.to_string(),
            items: g
                .items
                .into_iter()
                .map(|it| ConfigSearchItem {
                    key: it.key.to_string(),
                    label: it.def.label.to_string(),
                })
                .collect(),
        })
        .collect()
}

// ── Shared data records for `WallApi` ────────────────────────────────────────

/// Terminal summary row returned in [`WallSnapshot`] (`wall.list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSummary {
    pub id: String,
    pub name: String,
    pub hidden: bool,
    pub group: Option<String>,
}

/// Group summary row returned in [`WallSnapshot`] and group verbs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupSummary {
    pub id: String,
    pub name: String,
    /// `"tile"`, `"freeform"`, or `"split"`.
    pub layout: String,
    pub columns: usize,
    pub gap: f64,
    pub bars: Option<bool>,
    pub members: Vec<String>,
}

/// Snapshot of terminals, groups, and selection for `wall.list`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WallSnapshot {
    pub terminals: Vec<TerminalSummary>,
    pub groups: Vec<GroupSummary>,
    pub selection: Vec<String>,
}

/// Current fullscreen status read by `wall.fullscreen` and `wall.fullscreen.pane`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FullscreenStatus {
    pub active: bool,
    /// `"single"`, `"group"`, or `""` when inactive.
    pub mode: String,
    /// Active target (`id` or `group_id`) when `active` is `true`.
    pub target: Option<String>,
    /// Solo fullscreen terminal id, if in `"single"` mode.
    pub id: Option<String>,
    /// Group fullscreen id, if in `"group"` mode.
    pub group_id: Option<String>,
    /// Remembered group to return to when unzooming single-pane fullscreen.
    pub return_group: Option<String>,
    pub line_px: f64,
    /// Raw state payload included in `data.fullscreen`.
    pub state: Option<Value>,
}

/// Dockable/floatable panel state (`sidebar`, `statusWindow`, `settingsPanel`, `helpPanel`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelPlacement {
    pub open: bool,
    /// `"left"`, `"right"`, or `"float"`.
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub float: Option<PointPayload>,
    #[serde(default)]
    pub collapsed: Vec<String>,
}

impl Default for PanelPlacement {
    fn default() -> Self {
        Self {
            open: false,
            side: "float".to_string(),
            float: None,
            collapsed: Vec::new(),
        }
    }
}

/// JSON `{ x, y }` coordinate payload.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointPayload {
    pub x: f64,
    pub y: f64,
}

impl From<Point> for PointPayload {
    fn from(p: Point) -> Self {
        Self { x: p.x, y: p.y }
    }
}

/// Patch applied by `placePanel` (`wall.sidebar`, `wall.status`, `wall.settings`, `wall.help`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelPatch {
    pub open: Option<bool>,
    pub side: Option<String>,
    pub float: Option<Point>,
    pub collapsed: Option<Vec<String>>,
}

/// Which panel a panel-placement verb operates on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelKind {
    Sidebar,
    Status,
    Settings,
    Help,
}

/// Screen-space bounding box (CSS px) returned for positioning `wall.menu`
/// (unified with [`crate::camera::ScreenBox`]).
pub use crate::camera::ScreenBox;

/// Target passed to [`WallApi::open_menu`] (`MenuTarget` in `contextMenu.js`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MenuTarget {
    Terminal { id: String, part: String },
    Group { id: String },
    Canvas,
}

/// Matched knob entry returned in [`ConfigSearchGroup`] (`wall.config.find`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigSearchItem {
    pub key: String,
    pub label: String,
}

/// Matched section and its knobs for `wall.config.find`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigSearchGroup {
    pub section_id: String,
    pub section_title: String,
    pub items: Vec<ConfigSearchItem>,
}

/// One gomuxai agent entry in [`BusHostGroup`] (`wall.bus`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusAgentEntry {
    pub name: String,
    pub target: String,
    pub status: Option<String>,
    pub end_reason: Option<String>,
    pub detail: Option<String>,
    pub attached: Option<bool>,
}

/// Host group of live gomuxai agents (`WallBus.hosts()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusHostGroup {
    pub host: String,
    pub agents: Vec<BusAgentEntry>,
}

/// Bus state read by `wall.bus`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusView {
    /// `None` when no snapshot has arrived (`reason` explains why).
    pub snapshot_host: Option<String>,
    pub reason: String,
    pub hosts: Vec<BusHostGroup>,
}

/// Result of [`WallApi::attach_agent`] (`wall.attach`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachOutcome {
    pub id: String,
    pub existing: bool,
}

/// Result of [`WallApi::agent_set_open`] (`wall.agent`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOpenResult {
    pub open: bool,
    pub side: String,
    pub session: Option<String>,
    pub missing: Option<String>,
}

/// Result of [`WallApi::agent_set_side`] (`wall.agent.side`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSideResult {
    pub side: String,
}

/// Result of [`WallApi::agent_set_bulk_expanded`] (`wall.agent.expand` / `collapse`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBulkExpandResult {
    pub changed: usize,
}

/// Result of [`WallApi::agent_toggle_event`] (`wall.agent.show`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentToggleEventResult {
    #[serde(rename = "eventId")]
    pub event_id: String,
    pub expanded: bool,
}

/// Result of [`WallApi::agent_jump`] (`wall.agent.jump`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJumpResult {
    pub live: bool,
    #[serde(rename = "eventId")]
    pub event_id: String,
    pub offset: usize,
}

/// Result of [`WallApi::agent_load_earlier`] (`wall.agent.earlier`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEarlierResult {
    pub loaded: usize,
}

fn value_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

// ── The single host trait (`WallApi`) ────────────────────────────────────────

/// The wall's imperative surface as `wall/wallCommands.js` calls it.
///
/// Implementing this single trait on the real wall (or a headless test double)
/// wires all 54 `wall.*` verbs with identical argument validation, error refusal
/// text, and structured replies.
pub trait WallApi {
    // ── Terminals, visibility, focus & framing ───────────────────────────
    fn snapshot(&self) -> WallSnapshot;
    fn has_terminal(&self, id: &str) -> bool;
    fn is_shown(&self, id: &str) -> bool;
    fn shown_order(&self) -> Vec<String>;
    fn focused_id(&self) -> Option<String>;

    fn hide(&mut self, id: &str) -> Result<(), String>;
    fn show(&mut self, id: &str) -> Result<(), String>;
    fn focus_and_frame(&mut self, id: &str) -> Result<(), String>;
    fn spawn(&mut self, at: Option<Point>) -> Result<String, String>;
    fn fit_all(&mut self) -> Result<(), String>;
    fn close_terminal(&mut self, id: &str) -> Result<CommandReply, String>;
    fn rename_terminal(&mut self, id: &str, name: &str) -> Result<(), String>;
    fn terminal_name(&self, id: &str) -> String;

    // ── Wall selection & terminal text clipboard ─────────────────────────
    fn selection(&self) -> Vec<String>;
    fn select(&mut self, ids: &[String]) -> Result<(), String>;

    fn terminal_selection_text(&self, id: &str) -> String;
    fn selection_clipboard(&self) -> String;
    fn write_clipboard(&mut self, text: &str) -> Result<(), String>;
    fn read_clipboard(&mut self) -> Result<String, String>;
    /// Read the system clipboard synchronously (`Ok(Some(text))`), or emit an async
    /// read-clipboard request for `id` (`Ok(None)`). Defaults to [`Self::read_clipboard`].
    fn request_clipboard_paste(&mut self, _id: &str) -> Result<Option<String>, String> {
        self.read_clipboard().map(Some)
    }
    fn paste_into_terminal(&mut self, id: &str, text: &str) -> Result<bool, String>;
    fn notify(&mut self, message: &str);

    // ── Fullscreen ───────────────────────────────────────────────────────
    fn fullscreen_info(&self) -> FullscreenStatus;
    fn fullscreen_exit(&mut self) -> Result<(), String>;
    fn fullscreen_enter_default(&mut self, id: &str) -> Result<bool, String>;
    fn fullscreen_enter(
        &mut self,
        id: &str,
        return_group: Option<&str>,
    ) -> Result<bool, String>;
    fn fullscreen_enter_group(&mut self, gid: &str) -> Result<bool, String>;
    fn fullscreen_toggle_pane(&mut self, id: Option<&str>) -> Result<(), String>;
    fn fullscreen_text(&mut self, dir: i32) -> Result<Option<f64>, String>;

    // ── Groups, splits, panes & focus history ────────────────────────────
    fn has_group(&self, gid: &str) -> bool;
    fn group_of(&self, id: &str) -> Option<String>;
    fn group_info(&self, gid: &str) -> Result<GroupSummary, String>;
    fn group_name(&self, gid: &str) -> Result<String, String>;
    fn group_create(&mut self, ids: &[String]) -> Result<String, String>;
    fn group_add(&mut self, gid: &str, ids: &[String]) -> Result<(), String>;
    fn group_remove(&mut self, ids: &[String]) -> Result<(), String>;
    fn group_ungroup(&mut self, gid: &str) -> Result<(), String>;
    fn group_set_layout(
        &mut self,
        gid: &str,
        layout: &str,
        columns: Option<i64>,
        gap: Option<f64>,
    ) -> Result<(), String>;
    fn group_set_columns(&mut self, gid: &str, columns: usize) -> Result<(), String>;
    fn group_set_bars(&mut self, gid: &str, on: bool) -> Result<(), String>;
    fn group_set_gap(&mut self, gid: &str, gap: f64) -> Result<(), String>;
    fn group_rename(&mut self, gid: &str, name: &str) -> Result<(), String>;
    fn group_move(&mut self, gid: &str, dx: f64, dy: f64) -> Result<(), String>;
    fn frame_group(&mut self, gid: &str) -> Result<bool, String>;

    fn split_terminal(&mut self, dir: &str, id: Option<&str>) -> Result<String, String>;
    fn focus_pane(&mut self, dir: &str) -> Result<Option<String>, String>;
    fn group_move_pane(&mut self, id: &str, dir: &str) -> Result<Option<String>, String>;
    fn step_focus_history(&mut self, dir: i32) -> Result<Option<String>, String>;

    // ── Panels, status & context menu ────────────────────────────────────
    fn panel(&self, kind: PanelKind) -> PanelPlacement;
    fn set_panel(&mut self, kind: PanelKind, patch: PanelPatch);
    fn termd_state(&self) -> String;
    fn termd_dismiss_lost(&mut self) -> bool;

    fn view_size(&self) -> (f64, f64);
    fn terminal_screen_box(&self, id: &str) -> Result<ScreenBox, String>;
    fn group_header_screen_box(&self, gid: &str) -> Result<Option<ScreenBox>, String>;
    fn open_menu(&mut self, target: MenuTarget, at: Point) -> Result<Value, String>;
    fn close_menu(&mut self);

    // ── Config knobs ─────────────────────────────────────────────────────
    fn config_entries(&self) -> Vec<(String, Value)>;
    fn config_get(&self, key: &str) -> Result<Value, String>;
    fn config_check(&self, key: &str, raw: &str) -> Result<Value, String> {
        check_fallback_knob(key, raw)
    }
    fn config_set(&mut self, key: &str, value: Value) -> Result<Value, String>;
    fn config_reset(&mut self, key: &str) -> Result<Value, String>;
    fn config_search(&self, query: &str) -> Vec<ConfigSearchGroup> {
        search_fallback_knobs(query)
    }

    // ── gomuxai bus, Agent View & relay token ────────────────────────────
    fn bus_view(&self) -> BusView;
    fn terminals_attached_to(&self, target: &str) -> Vec<String>;
    fn attach_agent(&mut self, target: &str) -> Result<AttachOutcome, String>;

    fn agent_set_open(&mut self, id: &str, how: &str) -> Result<AgentOpenResult, String>;
    fn agent_set_side(&mut self, id: &str, side: &str) -> Result<AgentSideResult, String>;
    fn agent_set_filter(
        &mut self,
        id: &str,
        mode: &str,
        categories: &[String],
    ) -> Result<Value, String>;
    fn agent_set_bulk_expanded(
        &mut self,
        id: &str,
        expanded: bool,
        categories: &[String],
    ) -> Result<AgentBulkExpandResult, String>;
    fn agent_toggle_event(
        &mut self,
        id: &str,
        event_id: &str,
        how: &str,
    ) -> Result<AgentToggleEventResult, String>;
    fn agent_show_full_body(&mut self, id: &str, event_id: &str) -> Result<(), String>;
    fn agent_jump(&mut self, id: &str, target: &str) -> Result<AgentJumpResult, String>;
    fn agent_load_earlier(&mut self, id: &str) -> Result<AgentEarlierResult, String>;

    /// Active `(host, port)` of the WebSocket relay bridge, or `None` when not running.
    fn relay_endpoint(&self) -> Option<(String, u16)>;
    fn set_relay_token(&mut self, token: &str) -> Result<(), String>;
}

// ── Shared internal helpers matching `wallCommands.js` ───────────────────────

fn terminal_ids<W: WallApi>(wall: &W, args: &[String], min: usize) -> Result<Vec<String>, String> {
    if args.len() < min {
        return Err("name a terminal".into());
    }
    for id in args {
        if !wall.has_terminal(id) {
            return Err(format!("no terminal '{id}'"));
        }
    }
    Ok(args.to_vec())
}

fn text_target<W: WallApi>(wall: &W, args: &[String]) -> Result<String, String> {
    let id = match args.first() {
        Some(named) => Some(named.clone()),
        None => wall.focused_id(),
    };
    let Some(id) = id else {
        return Err("no terminal named, and none is focused".into());
    };
    if !wall.has_terminal(&id) {
        return Err(format!("no terminal '{id}'"));
    }
    Ok(id)
}

fn place_panel<W: WallApi>(
    wall: &mut W,
    kind: PanelKind,
    name: &str,
    args: &[String],
) -> Result<(), String> {
    let what = args.first().map_or("toggle", String::as_str);
    match what {
        "toggle" => {
            let cur = wall.panel(kind);
            wall.set_panel(
                kind,
                PanelPatch { open: Some(!cur.open), ..PanelPatch::default() },
            );
            Ok(())
        }
        "open" | "close" => {
            wall.set_panel(
                kind,
                PanelPatch { open: Some(what == "open"), ..PanelPatch::default() },
            );
            Ok(())
        }
        "left" | "right" => {
            wall.set_panel(
                kind,
                PanelPatch {
                    open: Some(true),
                    side: Some(what.to_string()),
                    ..PanelPatch::default()
                },
            );
            Ok(())
        }
        "float" => {
            let float = if args.get(1).is_some() {
                Some(Point::new(
                    num_arg(args.get(1), "x")?,
                    num_arg(args.get(2), "y")?,
                ))
            } else {
                None
            };
            wall.set_panel(
                kind,
                PanelPatch {
                    open: Some(true),
                    side: Some("float".to_string()),
                    float,
                    collapsed: None,
                },
            );
            Ok(())
        }
        _ => Err(format!(
            "the {name} takes toggle|open|close|left|right|float, not '{what}'"
        )),
    }
}

fn on_off_knob<W: WallApi>(wall: &mut W, key: &str, how: Option<&String>) -> Result<bool, String> {
    let mode = how.map_or("toggle", String::as_str);
    if !matches!(mode, "toggle" | "on" | "off") {
        return Err(format!("takes toggle|on|off, not '{mode}'"));
    }
    let next = if mode == "toggle" {
        !wall.config_get(key)?.as_bool().unwrap_or(false)
    } else {
        mode == "on"
    };
    wall.config_set(key, Value::Bool(next))?;
    Ok(next)
}

fn fullscreen_reply(fs: FullscreenStatus) -> CommandReply {
    if fs.active {
        let target = fs.target.as_deref().unwrap_or("");
        CommandReply::ok(
            format!("fullscreen {target}: {:.1} px a row", fs.line_px),
            Some(json!({ "fullscreen": fs.state })),
        )
    } else {
        CommandReply::ok("fullscreen off", Some(json!({ "fullscreen": Value::Null })))
    }
}

fn reg<W, F>(
    router: &mut CommandRouter<W>,
    name: &str,
    usage: &str,
    description: &str,
    mut f: F,
) where
    W: WallApi + 'static,
    F: FnMut(&[String], &mut W) -> Result<CommandReply, String> + 'static,
{
    router.register(
        name,
        RegisterMeta { description, usage, returns: "" },
        move |args, wall, _meta| match f(args, wall) {
            Ok(reply) => Ok(reply),
            Err(msg) => Ok(CommandReply::err(crate::router::redact_relay_secrets(&msg))),
        },
    );
}

/// Register all 54 `wall.*` verbs from `wall/wallCommands.js` on `router`.
pub fn register_wall_commands<W: WallApi + 'static>(router: &mut CommandRouter<W>) {
    reg(
        router,
        "wall.list",
        "",
        "Every terminal and group on the wall: shown, hidden, grouped",
        |_args, wall| {
            let s = wall.snapshot();
            let fmt_line = |t: &TerminalSummary| {
                let name_json = serde_json::to_string(&t.name).unwrap_or_default();
                format!(
                    "  {}{}",
                    t.id,
                    if t.hidden {
                        format!(" (hidden) {name_json}")
                    } else {
                        format!(" {name_json}")
                    }
                )
            };
            let mut out = Vec::new();
            for g in &s.groups {
                let name_json = serde_json::to_string(&g.name).unwrap_or_default();
                let desc = if g.layout == "tile" {
                    format!("tile {} cols, gap {}", g.columns, fmt_num(g.gap))
                } else {
                    "freeform".to_string()
                };
                out.push(format!("{} {name_json} {desc}", g.id));
                for id in &g.members {
                    if let Some(t) = s.terminals.iter().find(|x| &x.id == id) {
                        out.push(fmt_line(t));
                    }
                }
            }
            let loose: Vec<&TerminalSummary> =
                s.terminals.iter().filter(|t| t.group.is_none()).collect();
            if !loose.is_empty() {
                out.push("ungrouped".to_string());
                for t in loose {
                    out.push(fmt_line(t));
                }
            }
            let text = if out.is_empty() {
                "(no terminals)".to_string()
            } else {
                out.join("\n")
            };
            Ok(CommandReply {
                text,
                data: Some(json!({
                    "terminals": s.terminals,
                    "groups": s.groups,
                    "selection": s.selection,
                })),
            })
        },
    );

    reg(
        router,
        "wall.hide",
        "<id…>",
        "Take terminals off the canvas; they keep running",
        |args, wall| {
            let ids = terminal_ids(wall, args, 1)?;
            for id in &ids {
                wall.hide(id)?;
            }
            Ok(CommandReply::ok(
                format!("hid {}", args.join(", ")),
                Some(json!({ "ids": args })),
            ))
        },
    );

    reg(
        router,
        "wall.show",
        "<id…>",
        "Bring hidden terminals back, at their own spots",
        |args, wall| {
            let ids = terminal_ids(wall, args, 1)?;
            for id in &ids {
                wall.show(id)?;
            }
            Ok(CommandReply::ok(
                format!("showed {}", args.join(", ")),
                Some(json!({ "ids": args })),
            ))
        },
    );

    reg(
        router,
        "wall.reveal",
        "<id>",
        "Show a terminal (if hidden), focus it and frame it",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            wall.show(id)?;
            wall.focus_and_frame(id)?;
            Ok(CommandReply::ok(format!("revealed {id}"), Some(json!({ "id": id }))))
        },
    );

    reg(
        router,
        "wall.spawn",
        "[x y]",
        "A new terminal, focused and framed; at x y (world) its window's top-left corner",
        |args, wall| {
            let at = if !args.is_empty() {
                Some(Point::new(
                    num_arg(args.first(), "x")?,
                    num_arg(args.get(1), "y")?,
                ))
            } else {
                None
            };
            let id = wall.spawn(at).map_err(|r| {
                if r.is_empty() {
                    "the spawn did not land".to_string()
                } else {
                    r
                }
            })?;
            let (suffix, at_val) = match at {
                Some(p) => (
                    format!(" at ({}, {})", fmt_num(p.x), fmt_num(p.y)),
                    json!({ "x": p.x, "y": p.y }),
                ),
                None => (String::new(), Value::Null),
            };
            Ok(CommandReply::ok(
                format!("spawned {id}{suffix}"),
                Some(json!({ "id": id, "at": at_val })),
            ))
        },
    );

    reg(
        router,
        "wall.fullscreen",
        "[toggle|on|off] [id|gid]",
        "Put a group (when focused in one with 2+ panes) or terminal fullscreen, or back on the canvas",
        |args, wall| {
            let what = args.first().map_or("toggle", String::as_str);
            let named = args.get(1).map(String::as_str);
            if what == "off" {
                wall.fullscreen_exit()?;
            } else if what == "on" || what == "toggle" {
                match named {
                    None => {
                        if what == "toggle" && wall.fullscreen_info().active {
                            wall.fullscreen_exit()?;
                        } else {
                            let Some(focused) = wall.focused_id() else {
                                return Err(
                                    "nothing focused to put fullscreen: focus one, or name a terminal or group"
                                        .into(),
                                );
                            };
                            if !wall.fullscreen_enter_default(&focused)? {
                                return Err(format!("'{focused}' is hidden"));
                            }
                        }
                    }
                    Some(name) if wall.has_group(name) => {
                        if what == "toggle"
                            && wall.fullscreen_info().group_id.as_deref() == Some(name)
                        {
                            wall.fullscreen_exit()?;
                        } else if !wall.fullscreen_enter_group(name)? {
                            return Err(format!(
                                "group '{name}' has no shown member to put fullscreen"
                            ));
                        }
                    }
                    Some(name) => {
                        let ids = terminal_ids(wall, &[name.to_string()], 1)?;
                        let id = &ids[0];
                        if what == "toggle" && wall.fullscreen_info().id.as_deref() == Some(id) {
                            wall.fullscreen_exit()?;
                        } else if !wall.fullscreen_enter(id, None)? {
                            return Err(format!("terminal '{id}' is hidden"));
                        }
                    }
                }
            } else {
                return Err(format!("fullscreen takes toggle|on|off, not '{what}'"));
            }
            Ok(fullscreen_reply(wall.fullscreen_info()))
        },
    );

    reg(
        router,
        "wall.fullscreen.pane",
        "[toggle|on|off] [id]",
        "Fullscreen a single pane alone (zooming out of Group Fullscreen and back on toggle)",
        |args, wall| {
            let what = args.first().map_or("toggle", String::as_str);
            let id = match args.get(1) {
                Some(named) => Some(terminal_ids(wall, std::slice::from_ref(named), 1)?.remove(0)),
                None => None,
            };
            let fs = wall.fullscreen_info();
            match what {
                "off" => {
                    if fs.mode == "single"
                        && fs
                            .return_group
                            .as_deref()
                            .is_some_and(|rg| wall.has_group(rg))
                    {
                        wall.fullscreen_toggle_pane(None)?;
                    } else {
                        wall.fullscreen_exit()?;
                    }
                }
                "toggle" => {
                    let focused = wall.focused_id();
                    if !fs.active && id.is_none() && focused.is_none() {
                        return Err("no terminal to put fullscreen: focus one, or name it".into());
                    }
                    if !fs.active {
                        let target = id.as_deref().or(focused.as_deref()).expect("checked above");
                        if !wall.fullscreen_enter(target, None)? {
                            return Err(format!("terminal '{target}' is hidden"));
                        }
                    } else {
                        wall.fullscreen_toggle_pane(id.as_deref())?;
                    }
                }
                "on" => {
                    let focused = wall.focused_id();
                    let Some(target) = id.as_deref().or(focused.as_deref()) else {
                        return Err("no terminal to put fullscreen: focus one, or name it".into());
                    };
                    let rg_candidate = if fs.mode == "group" {
                        fs.group_id.clone()
                    } else {
                        fs.return_group.clone()
                    };
                    let return_group = rg_candidate.filter(|rg| {
                        wall.group_of(target).as_deref() == Some(rg.as_str())
                    });
                    if !wall.fullscreen_enter(target, return_group.as_deref())? {
                        return Err(format!("terminal '{target}' is hidden"));
                    }
                }
                _ => return Err(format!("fullscreen.pane takes toggle|on|off, not '{what}'")),
            }
            Ok(fullscreen_reply(wall.fullscreen_info()))
        },
    );

    reg(
        router,
        "wall.fullscreen.text",
        "<bigger|smaller|reset>",
        "The fullscreen terminal's text size: it re-wraps, and the size is remembered for it (reset: the canvas's size)",
        |args, wall| {
            let arg0 = args.first().map_or("", String::as_str);
            let dir = match arg0 {
                "bigger" => 1,
                "smaller" => -1,
                "reset" => 0,
                _ => {
                    return Err(format!(
                        "fullscreen text takes bigger|smaller|reset, not '{arg0}'"
                    ))
                }
            };
            let Some(px) = wall.fullscreen_text(dir)? else {
                return Err("no terminal is fullscreen".into());
            };
            Ok(CommandReply::ok(
                format!("fullscreen text {px:.1} px a row"),
                Some(json!({ "linePx": px })),
            ))
        },
    );

    reg(
        router,
        "wall.fit",
        "",
        "Ease the camera to fit every shown terminal",
        |_args, wall| {
            if wall.shown_order().is_empty() {
                return Err("no shown terminal to fit".into());
            }
            wall.fit_all()?;
            let n = wall.shown_order().len();
            Ok(CommandReply::ok(format!("fitting {n} terminal(s)"), None))
        },
    );

    reg(
        router,
        "wall.close",
        "<id>",
        "Kill a terminal — its program in termd — and forget it",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            wall.close_terminal(&ids[0])
        },
    );

    reg(
        router,
        "wall.rename",
        "<id> [name…]",
        "Name a terminal; empty gives back its shell's title",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let name = args.get(1..).unwrap_or(&[]).join(" ");
            wall.rename_terminal(id, &name)?;
            let now = wall.terminal_name(id);
            Ok(CommandReply::ok(
                format!("{id} is \"{now}\""),
                Some(json!({ "id": id, "name": now })),
            ))
        },
    );

    reg(
        router,
        "wall.select",
        "[<id…>|all|none]",
        "Select exactly these terminals (all: every shown one; none clears)",
        |args, wall| {
            let ids = match args {
                [one] if one == "none" => Vec::new(),
                [one] if one == "all" => wall.shown_order(),
                _ => terminal_ids(wall, args, 0)?,
            };
            wall.select(&ids)?;
            let sel = wall.selection();
            Ok(CommandReply::ok(
                format!("selected {}", ids.len()),
                Some(json!({ "selection": sel })),
            ))
        },
    );

    reg(
        router,
        "wall.select.add",
        "<id…>",
        "Add terminals to the selection",
        |args, wall| {
            let add_ids = terminal_ids(wall, args, 1)?;
            let mut next = wall.selection();
            for id in add_ids {
                if !next.contains(&id) {
                    next.push(id);
                }
            }
            wall.select(&next)?;
            let sel = wall.selection();
            Ok(CommandReply::ok(
                format!("selected {}", sel.len()),
                Some(json!({ "selection": sel })),
            ))
        },
    );

    reg(
        router,
        "wall.select.toggle",
        "<id>",
        "Add a terminal to the selection, or take it out",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let mut next = wall.selection();
            if let Some(pos) = next.iter().position(|x| x == id) {
                next.remove(pos);
            } else {
                next.push(id.clone());
            }
            wall.select(&next)?;
            let sel = wall.selection();
            Ok(CommandReply::ok(
                format!("selected {}", sel.len()),
                Some(json!({ "selection": sel })),
            ))
        },
    );

    reg(
        router,
        "wall.copy",
        "[id]",
        "Copy a terminal's selected text (default: the focused terminal's) to the system clipboard",
        |args, wall| {
            let id = text_target(wall, args)?;
            let text = wall.terminal_selection_text(&id);
            if text.is_empty() {
                return Err(format!("nothing is selected in {id}"));
            }
            if let Err(e) = wall.write_clipboard(&text) {
                wall.notify(&format!("copy: {e}"));
                return Err(e);
            }
            let chars = text.chars().count();
            Ok(CommandReply::ok(
                format!("copied {chars} characters from {id}"),
                Some(json!({ "id": id, "text": text })),
            ))
        },
    );

    reg(
        router,
        "wall.paste",
        "[id] [selection]",
        "Paste into a terminal (default: the focused one) as the paste chord does: the system clipboard, or with `selection` the wall's selection clipboard (what a middle-click pastes)",
        |args, wall| {
            let from_selection = args.last().is_some_and(|s| s == "selection");
            let target_args = if from_selection {
                &args[..args.len() - 1]
            } else {
                args
            };
            let id = text_target(wall, target_args)?;
            let text = if from_selection {
                wall.selection_clipboard()
            } else {
                match wall.request_clipboard_paste(&id) {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        return Ok(CommandReply::ok(
                            format!("requested clipboard paste into {id}"),
                            Some(json!({ "id": id, "pending": true })),
                        ));
                    }
                    Err(e) => {
                        wall.notify(&format!("paste: {e}"));
                        return Err(e);
                    }
                }
            };
            if text.is_empty() {
                return Err(if from_selection {
                    "the selection clipboard is empty (select some text first)".into()
                } else {
                    "the clipboard holds no text".into()
                });
            }
            if !wall.paste_into_terminal(&id, &text)? {
                return Err(format!("{id} has no input route (no adapter owns it)"));
            }
            let chars = text.chars().count();
            let utf16_len = text.encode_utf16().count();
            let suffix = if from_selection {
                " from the selection clipboard"
            } else {
                ""
            };
            Ok(CommandReply::ok(
                format!("pasted {chars} characters into {id}{suffix}"),
                Some(json!({ "id": id, "length": utf16_len })),
            ))
        },
    );

    reg(
        router,
        "wall.group",
        "[<id…>]",
        "Gather terminals (default: the selection) into a new group",
        |args, wall| {
            let ids = if !args.is_empty() {
                terminal_ids(wall, args, 1)?
            } else {
                wall.selection()
            };
            if ids.is_empty() {
                return Err(
                    "nothing selected — lasso some terminals (Shift+drag) or name them".into(),
                );
            }
            let gid = wall.group_create(&ids)?;
            wall.select(&[])?;
            let name = wall.group_name(&gid)?;
            Ok(CommandReply::ok(
                format!("{gid} \"{name}\" gathers {}", ids.join(", ")),
                Some(json!({ "gid": gid, "members": ids })),
            ))
        },
    );

    reg(
        router,
        "wall.group.add",
        "<gid> <id…>",
        "Add terminals to a group",
        |args, wall| {
            let gid = args.first().map_or("", String::as_str);
            let rest = args.get(1..).unwrap_or(&[]);
            let ids = terminal_ids(wall, rest, 1)?;
            wall.group_add(gid, &ids)?;
            let g = wall.group_info(gid)?;
            Ok(CommandReply::ok(
                format!("{gid} has {}", g.members.len()),
                Some(json!({ "gid": gid })),
            ))
        },
    );

    reg(
        router,
        "wall.group.remove",
        "<id…>",
        "Take terminals out of their group; they stay where they are",
        |args, wall| {
            let ids = terminal_ids(wall, args, 1)?;
            wall.group_remove(&ids)?;
            Ok(CommandReply::ok(
                format!("took {} out", args.join(", ")),
                Some(json!({ "ids": args })),
            ))
        },
    );

    reg(
        router,
        "wall.ungroup",
        "<gid>",
        "Dissolve a group; its terminals stay where they are",
        |args, wall| {
            let Some(gid) = args.first().filter(|s| !s.is_empty()) else {
                return Err("name a group".into());
            };
            wall.group_ungroup(gid)?;
            Ok(CommandReply::ok(
                format!("dissolved {gid}"),
                Some(json!({ "gid": gid })),
            ))
        },
    );

    reg(
        router,
        "wall.group.layout",
        "<gid> tile [columns] [gap] | freeform | split [columns]",
        "Set a group's layout (split: one rect cut into panes, rows of columns)",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let layout = args.get(1).map_or("undefined", String::as_str);
            if !matches!(layout, "tile" | "freeform" | "split") {
                return Err(format!(
                    "a layout is 'tile', 'freeform' or 'split', not '{layout}'"
                ));
            }
            let cols = match args.get(2) {
                Some(c) => Some(int_arg(Some(c), "columns")?),
                None => None,
            };
            let gap = match args.get(3) {
                Some(g) => Some(num_arg(Some(g), "gap")?),
                None => None,
            };
            wall.group_set_layout(gid, layout, cols, gap)?;
            let g = wall.group_info(gid)?;
            Ok(CommandReply::ok(
                format!("{gid} is {layout}"),
                Some(json!({ "gid": gid, "group": g })),
            ))
        },
    );

    reg(
        router,
        "wall.group.columns",
        "<gid> <n|+n|-n>",
        "Set a tiled group's columns (+n / -n steps)",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let g = wall.group_info(gid)?;
            let raw_n = args.get(1);
            let parsed = int_arg(raw_n, "columns")?;
            let is_delta = raw_n.is_some_and(|s| s.starts_with('+') || s.starts_with('-'));
            let cols = if is_delta {
                g.columns as i64 + parsed
            } else {
                parsed
            };
            let max_cols = g.members.len().max(1) as i64;
            let clamped = cols.clamp(1, max_cols) as usize;
            wall.group_set_columns(gid, clamped)?;
            let now = wall.group_info(gid)?.columns;
            Ok(CommandReply::ok(
                format!("{gid} tiles in {now}"),
                Some(json!({ "gid": gid, "columns": now })),
            ))
        },
    );

    reg(
        router,
        "wall.split",
        "<right|down> [id]",
        "Split a terminal (default: the focused one): a new terminal takes the half to its right or below",
        |args, wall| {
            let dir = args.first().map_or("", String::as_str);
            if dir != "right" && dir != "down" {
                return Err(format!("a split goes right or down, not '{dir}'"));
            }
            let id = match args.get(1) {
                Some(named) => Some(terminal_ids(wall, std::slice::from_ref(named), 1)?.remove(0)),
                None => wall.focused_id(),
            };
            let born = wall.split_terminal(dir, id.as_deref())?;
            let gid = wall.group_of(&born);
            let id_disp = id.as_deref().unwrap_or("null");
            Ok(CommandReply::ok(
                format!("split {id_disp} {dir}: {born}"),
                Some(json!({ "id": born, "gid": gid })),
            ))
        },
    );

    reg(
        router,
        "wall.pane",
        "<left|right|up|down>",
        "Focus the pane (or the nearest window) next to the focused terminal that way",
        |args, wall| {
            let dir = args.first().map_or("", String::as_str);
            if !matches!(dir, "left" | "right" | "up" | "down") {
                return Err(format!("a pane is left, right, up or down, not '{dir}'"));
            }
            let from = wall.focused_id();
            let to = wall.focus_pane(dir)?;
            let msg = match &to {
                Some(t) => format!("focused {t}"),
                None => format!("nothing {dir} of {}", from.as_deref().unwrap_or("nothing")),
            };
            Ok(CommandReply::ok(msg, Some(json!({ "id": to }))))
        },
    );

    reg(
        router,
        "wall.pane.move",
        "<left|right|up|down> [id]",
        "Move a terminal (default: the focused one) a step within its group: a split pane trades places with the pane that way, a tile with the next tile",
        |args, wall| {
            let dir = args.first().map_or("", String::as_str);
            if !matches!(dir, "left" | "right" | "up" | "down") {
                return Err(format!("a pane moves left, right, up or down, not '{dir}'"));
            }
            let id = match args.get(1) {
                Some(named) => Some(terminal_ids(wall, std::slice::from_ref(named), 1)?.remove(0)),
                None => wall.focused_id(),
            };
            let Some(id) = id else {
                return Err("no terminal to move: focus one, or name it".into());
            };
            let other = wall.group_move_pane(&id, dir)?;
            let msg = match &other {
                Some(o) => format!("{id} traded places with {o}"),
                None => format!("nothing {dir} of {id} in its group"),
            };
            Ok(CommandReply::ok(msg, Some(json!({ "id": id, "with": other }))))
        },
    );

    reg(
        router,
        "wall.focus.history",
        "<back|forward>",
        "Step back or forward through previously focused terminals",
        |args, wall| {
            let arg0 = args.first().map_or("", String::as_str);
            let dir = match arg0 {
                "back" => -1,
                "forward" => 1,
                _ => return Err(format!("focus history takes back or forward, not '{arg0}'")),
            };
            let to = wall.step_focus_history(dir)?;
            let msg = match &to {
                Some(t) => format!("focused {t}"),
                None => format!("no {arg0} terminal in focus history"),
            };
            Ok(CommandReply::ok(msg, Some(json!({ "id": to }))))
        },
    );

    reg(
        router,
        "wall.group.bars",
        "<gid> [toggle|on|off]",
        "Show or hide a split's pane title bars",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let how = args.get(1).map_or("toggle", String::as_str);
            if !matches!(how, "toggle" | "on" | "off") {
                return Err(format!("bars take toggle|on|off, not '{how}'"));
            }
            let g = wall.group_info(gid)?;
            let on = if how == "toggle" {
                !g.bars.unwrap_or(false)
            } else {
                how == "on"
            };
            wall.group_set_bars(gid, on)?;
            Ok(CommandReply::ok(
                format!("{gid} bars {}", if on { "shown" } else { "hidden" }),
                Some(json!({ "gid": gid, "bars": on })),
            ))
        },
    );

    reg(
        router,
        "wall.group.gap",
        "<gid> <n>",
        "Set a tiled group's gap (world units)",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let gap = num_arg(args.get(1), "gap")?;
            wall.group_set_gap(gid, gap)?;
            Ok(CommandReply::ok(
                format!("{gid} gap {}", args[1]),
                Some(json!({ "gid": gid })),
            ))
        },
    );

    reg(
        router,
        "wall.group.rename",
        "<gid> [name…]",
        "Name a group; empty gives back \"group N\"",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let name = args.get(1..).unwrap_or(&[]).join(" ");
            wall.group_rename(gid, &name)?;
            let now = wall.group_name(gid)?;
            Ok(CommandReply::ok(
                format!("{gid} is \"{now}\""),
                Some(json!({ "gid": gid, "name": now })),
            ))
        },
    );

    reg(
        router,
        "wall.group.move",
        "<gid> <dx> <dy>",
        "Move a whole group by (dx, dy) world units",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            let dx = num_arg(args.get(1), "dx")?;
            let dy = num_arg(args.get(2), "dy")?;
            wall.group_move(gid, dx, dy)?;
            Ok(CommandReply::ok(format!("moved {gid}"), Some(json!({ "gid": gid }))))
        },
    );

    reg(
        router,
        "wall.group.frame",
        "<gid>",
        "Ease the camera to fit a group's shown members",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            if !wall.frame_group(gid)? {
                return Err(format!("group '{gid}' has no shown member to frame"));
            }
            Ok(CommandReply::ok(format!("framed {gid}"), Some(json!({ "gid": gid }))))
        },
    );

    reg(
        router,
        "wall.sidebar",
        "[toggle|open|close|left|right|float [x y]]",
        "Open, close or place the sidebar",
        |args, wall| {
            place_panel(wall, PanelKind::Sidebar, "sidebar", args)?;
            let now = wall.panel(PanelKind::Sidebar);
            let state = if now.open {
                format!("open, {}", now.side)
            } else {
                "closed".to_string()
            };
            Ok(CommandReply::ok(
                format!("sidebar {state}"),
                Some(json!({ "sidebar": now })),
            ))
        },
    );

    reg(
        router,
        "wall.sidebar.collapse",
        "<gid> [toggle|on|off]",
        "Fold or unfold a group's members in the sidebar",
        |args, wall| {
            let gid = args.first().map_or("undefined", String::as_str);
            wall.group_info(gid)?;
            let how = args.get(1).map_or("toggle", String::as_str);
            let mut cur = wall.panel(PanelKind::Sidebar).collapsed;
            let has = cur.iter().any(|x| x == gid);
            let on = match how {
                "toggle" => !has,
                "on" => true,
                "off" => false,
                _ => return Err(format!("collapse takes toggle|on|off, not '{how}'")),
            };
            if on && !has {
                cur.push(gid.to_string());
            } else if !on {
                cur.retain(|x| x != gid);
            }
            wall.set_panel(
                PanelKind::Sidebar,
                PanelPatch { collapsed: Some(cur), ..PanelPatch::default() },
            );
            Ok(CommandReply::ok(
                format!("{gid} {}", if on { "folded" } else { "unfolded" }),
                Some(json!({ "gid": gid, "collapsed": on })),
            ))
        },
    );

    reg(
        router,
        "wall.status",
        "[toggle|open|close|left|right|float [x y]]",
        "Open, close or place the termd status window",
        |args, wall| {
            place_panel(wall, PanelKind::Status, "status window", args)?;
            let now = wall.panel(PanelKind::Status);
            let termd = wall.termd_state();
            let state = if now.open {
                format!("open, {}", now.side)
            } else {
                "closed".to_string()
            };
            Ok(CommandReply::ok(
                format!("status window {state} · termd {termd}"),
                Some(json!({ "window": now, "termd": termd })),
            ))
        },
    );

    reg(
        router,
        "wall.status.dismiss",
        "",
        "Put away the termd-lost notice",
        |_args, wall| {
            let msg = if wall.termd_dismiss_lost() {
                "dismissed the termd-lost notice"
            } else {
                "no termd-lost notice to dismiss"
            };
            Ok(CommandReply::ok(msg, None))
        },
    );

    reg(
        router,
        "wall.menu",
        "terminal <id> [x y] | group <gid> [x y] | canvas [x y] | close",
        "Open the right-click menu on a terminal, a group or the canvas (at canvas px), or close it",
        |args, wall| {
            let kind = args.first().map_or("undefined", String::as_str);
            if kind == "close" {
                wall.close_menu();
                return Ok(CommandReply::ok("menu closed", None));
            }
            if !matches!(kind, "terminal" | "group" | "canvas") {
                return Err(format!(
                    "the menu opens on terminal|group|canvas (or close), not '{kind}'"
                ));
            }
            let (id_opt, rest): (Option<&str>, &[String]) = if kind == "canvas" {
                (None, args.get(1..).unwrap_or(&[]))
            } else {
                let id = args.get(1).map_or("", String::as_str);
                if id.is_empty() {
                    return Err(format!("name a {kind}"));
                }
                (Some(id), args.get(2..).unwrap_or(&[]))
            };

            let at = if !rest.is_empty() {
                Point::new(num_arg(rest.first(), "x")?, num_arg(rest.get(1), "y")?)
            } else if kind == "terminal" {
                let id = id_opt.expect("checked non-canvas");
                if !wall.is_shown(id) {
                    return Err(if wall.has_terminal(id) {
                        format!("terminal '{id}' is hidden")
                    } else {
                        format!("no terminal '{id}'")
                    });
                }
                let b = wall.terminal_screen_box(id)?;
                Point::new(js_round(b.left + 12.0), js_round(b.top + 12.0))
            } else if kind == "group" {
                let gid = id_opt.expect("checked non-canvas");
                let Some(b) = wall.group_header_screen_box(gid)? else {
                    return Err(format!("group '{gid}' has no header on the canvas"));
                };
                Point::new(js_round(b.left), js_round(b.bottom))
            } else {
                let (vw, vh) = wall.view_size();
                Point::new(js_round(vw / 2.0), js_round(vh / 2.0))
            };

            let target = match kind {
                "terminal" => MenuTarget::Terminal {
                    id: id_opt.expect("terminal id").to_string(),
                    part: "body".to_string(),
                },
                "group" => MenuTarget::Group { id: id_opt.expect("group id").to_string() },
                _ => MenuTarget::Canvas,
            };
            let menu_val = wall.open_menu(target, at)?;
            let id_part = id_opt.map_or(String::new(), |id| format!(" {id}"));
            Ok(CommandReply::ok(
                format!(
                    "menu on {kind}{id_part} at ({}, {})",
                    fmt_num(at.x),
                    fmt_num(at.y)
                ),
                Some(json!({ "menu": menu_val })),
            ))
        },
    );

    reg(
        router,
        "wall.config",
        "[<knob> [value…|default]]",
        "Read or set the wall's knobs",
        |args, wall| {
            let Some(key) = args.first() else {
                let entries = wall.config_entries();
                let text = entries
                    .iter()
                    .map(|(k, v)| format!("{k} {}", value_display(v)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let map: serde_json::Map<String, Value> = entries.into_iter().collect();
                return Ok(CommandReply { text, data: Some(Value::Object(map)) });
            };
            let rest = args.get(1..).unwrap_or(&[]);
            let value_opt = if rest.is_empty() {
                None
            } else {
                Some(rest.join(" "))
            };
            let v = match value_opt.as_deref() {
                None => {
                    let cur = wall.config_get(key)?;
                    wall.config_check(key, &value_display(&cur))?;
                    cur
                }
                Some("default") => {
                    let cur = wall.config_get(key)?;
                    wall.config_check(key, &value_display(&cur))?;
                    wall.config_reset(key)?
                }
                Some(raw) => {
                    let checked = wall.config_check(key, raw)?;
                    wall.config_set(key, checked)?
                }
            };
            Ok(CommandReply::ok(
                format!("{key} {}", value_display(&v)),
                Some(json!({ key: v })),
            ))
        },
    );

    reg(
        router,
        "wall.snap",
        "[toggle|on|off]",
        "Snap moves and resizes to the grid, or not",
        |args, wall| {
            let on = on_off_knob(wall, "snap", args.first())?;
            Ok(CommandReply::ok(
                format!("snap {}", if on { "on" } else { "off" }),
                Some(json!({ "snap": on })),
            ))
        },
    );

    reg(
        router,
        "wall.grid",
        "[toggle|on|off]",
        "Draw the drafting grid on the canvas, or not",
        |args, wall| {
            let on = on_off_knob(wall, "gridShow", args.first())?;
            Ok(CommandReply::ok(
                format!("grid {}", if on { "shown" } else { "hidden" }),
                Some(json!({ "gridShow": on })),
            ))
        },
    );

    reg(
        router,
        "wall.config.find",
        "<words…>",
        "The knobs whose label, key, description or section hold every word",
        |args, wall| {
            let query = args.join(" ");
            let found = wall.config_search(&query);
            let mut out = Vec::new();
            let mut sections_data = Vec::new();
            for g in &found {
                out.push(g.section_title.clone());
                let mut keys = Vec::new();
                for it in &g.items {
                    let cur = wall
                        .config_get(&it.key)
                        .map(|v| value_display(&v))
                        .unwrap_or_default();
                    out.push(format!("  {} {cur}  — {}", it.key, it.label));
                    keys.push(it.key.clone());
                }
                sections_data.push(json!({ "id": g.section_id, "keys": keys }));
            }
            let text = if out.is_empty() {
                format!("(no knob matches '{query}')")
            } else {
                out.join("\n")
            };
            Ok(CommandReply {
                text,
                data: Some(json!({ "sections": sections_data })),
            })
        },
    );

    reg(
        router,
        "wall.settings",
        "[toggle|open|close|left|right|float [x y]]",
        "Open, close or place the settings panel",
        |args, wall| {
            place_panel(wall, PanelKind::Settings, "settings panel", args)?;
            let now = wall.panel(PanelKind::Settings);
            let state = if now.open {
                format!("open, {}", now.side)
            } else {
                "closed".to_string()
            };
            Ok(CommandReply::ok(
                format!("settings {state}"),
                Some(json!({ "panel": now })),
            ))
        },
    );

    reg(
        router,
        "wall.help",
        "[toggle|open|close|left|right|float [x y]]",
        "Open, close or place the help: the wall's keys, gestures and verbs",
        |args, wall| {
            place_panel(wall, PanelKind::Help, "help", args)?;
            let now = wall.panel(PanelKind::Help);
            let state = if now.open {
                format!("open, {}", now.side)
            } else {
                "closed".to_string()
            };
            Ok(CommandReply::ok(
                format!("help {state}"),
                Some(json!({ "panel": now })),
            ))
        },
    );

    reg(
        router,
        "wall.settings.collapse",
        "<section> [toggle|on|off]",
        "Fold or unfold a section of the settings panel",
        |args, wall| {
            let id = args.first().map_or("undefined", String::as_str);
            if !SETTINGS_SECTION_IDS.contains(&id) {
                return Err(format!(
                    "no section '{id}' (sections: {})",
                    SETTINGS_SECTION_IDS.join(", ")
                ));
            }
            let how = args.get(1).map_or("toggle", String::as_str);
            let mut cur = wall.panel(PanelKind::Settings).collapsed;
            let has = cur.iter().any(|x| x == id);
            let on = match how {
                "toggle" => !has,
                "on" => true,
                "off" => false,
                _ => return Err(format!("collapse takes toggle|on|off, not '{how}'")),
            };
            if on && !has {
                cur.push(id.to_string());
            } else if !on {
                cur.retain(|x| x != id);
            }
            wall.set_panel(
                PanelKind::Settings,
                PanelPatch { collapsed: Some(cur), ..PanelPatch::default() },
            );
            Ok(CommandReply::ok(
                format!("{id} {}", if on { "folded" } else { "unfolded" }),
                Some(json!({ "section": id, "collapsed": on })),
            ))
        },
    );

    reg(
        router,
        "wall.bus",
        "",
        "The gomuxai agents the relay sees, by host, with their status and the terminal attached to each",
        |_args, wall| {
            let bus = wall.bus_view();
            let Some(local_host) = bus.snapshot_host else {
                return Err(format!("no gomuxai agents: {}", bus.reason));
            };
            let attached_terms = |a: &BusAgentEntry, is_local: bool| -> Vec<String> {
                let mut seen = BTreeSet::new();
                let mut out = Vec::new();
                for id in wall.terminals_attached_to(&a.target) {
                    if wall.has_terminal(&id) && seen.insert(id.clone()) {
                        out.push(id);
                    }
                }
                if is_local {
                    for id in wall.terminals_attached_to(&a.name) {
                        if wall.has_terminal(&id) && seen.insert(id.clone()) {
                            out.push(id);
                        }
                    }
                    let gx = gomux_screen_id(&a.name);
                    if wall.has_terminal(&gx) && seen.insert(gx.clone()) {
                        out.push(gx);
                    }
                }
                out
            };

            let mut out = Vec::new();
            let mut hosts_json = Vec::new();
            for h in &bus.hosts {
                let is_local = h.host.is_empty() || h.host == local_host;
                out.push(if h.host.is_empty() {
                    "this host".to_string()
                } else {
                    h.host.clone()
                });
                let mut agents_json = Vec::new();
                for a in &h.agents {
                    let on = attached_terms(a, is_local);
                    let word = bus_status_word(a.status.as_deref());
                    let status_str = match &a.end_reason {
                        Some(r) if !r.is_empty() => format!("{word} ({r})"),
                        _ => word.to_string(),
                    };
                    let mut notes = Vec::new();
                    if let Some(d) = &a.detail {
                        if !d.is_empty() {
                            notes.push(d.clone());
                        }
                    }
                    if !bus_attachable(a.attached) {
                        notes.push("no live session".to_string());
                    }
                    if !on.is_empty() {
                        notes.push(format!("on {}", on.join(", ")));
                    }
                    let notes_part = if notes.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", notes.join(" · "))
                    };
                    out.push(format!("  {} {status_str}{notes_part}", a.target));
                    agents_json.push(json!({
                        "target": a.target,
                        "status": a.status,
                        "attachable": bus_attachable(a.attached),
                        "terminals": on,
                    }));
                }
                hosts_json.push(json!({ "host": h.host, "agents": agents_json }));
            }
            let text = if out.is_empty() {
                "(no live agents)".to_string()
            } else {
                out.join("\n")
            };
            Ok(CommandReply { text, data: Some(json!({ "hosts": hosts_json })) })
        },
    );

    reg(
        router,
        "wall.attach",
        "<name[@host]>",
        "Attach to a gomuxai agent: reveal its terminal, else attach directly to its local termd session (or ssh on another host)",
        |args, wall| {
            let Some(target) = args.first().filter(|s| !s.is_empty()) else {
                return Err(
                    "name an agent: name, or name@host for one on another host".into(),
                );
            };
            let outcome = match wall.attach_agent(target) {
                Ok(o) => o,
                Err(e) => {
                    wall.notify(&format!("attach: {e}"));
                    return Err(e);
                }
            };
            let msg = if outcome.existing {
                format!("revealed {}, attached to {target}", outcome.id)
            } else {
                format!("{} attaches to {target}", outcome.id)
            };
            Ok(CommandReply::ok(
                msg,
                Some(json!({
                    "id": outcome.id,
                    "target": target,
                    "existing": outcome.existing,
                })),
            ))
        },
    );

    reg(
        router,
        "wall.agent",
        "[id] [toggle|open|close|right|left|top|bottom]",
        "Open, close or place a terminal's Agent View transcript companion (default: the focused terminal)",
        |args, wall| {
            let first = args.first().map(String::as_str);
            let second = args.get(1).map(String::as_str);
            let (id_opt, how) = match first {
                None | Some("") => (wall.focused_id(), "toggle"),
                Some(f) if AGENT_MODES.contains(&f) && !wall.has_terminal(f) => {
                    (wall.focused_id(), f)
                }
                Some(f) => (Some(f.to_string()), second.unwrap_or("toggle")),
            };
            let Some(id) = id_opt else {
                return Err("no terminal named, and none is focused".into());
            };
            let r = wall.agent_set_open(&id, how)?;
            let pair = if let Some(sess) = &r.session {
                format!(" · session {sess}")
            } else if let Some(miss) = &r.missing {
                format!(" · no agent found: {miss}")
            } else {
                String::new()
            };
            let state = if r.open {
                format!("open ({}){pair}", r.side)
            } else {
                "closed".to_string()
            };
            Ok(CommandReply::ok(
                format!("agent view on {id}: {state}"),
                Some(json!({
                    "id": id,
                    "open": r.open,
                    "side": r.side,
                    "session": r.session,
                    "missing": r.missing,
                })),
            ))
        },
    );

    reg(
        router,
        "wall.agent.side",
        "<id> <right|left|top|bottom>",
        "Attach a terminal's Agent View to right, left, top or bottom",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let side = args.get(1).map_or("", String::as_str);
            if !AGENT_SIDES.contains(&side) {
                return Err(format!(
                    "side takes right, left, top or bottom, not '{side}'"
                ));
            }
            let r = wall.agent_set_side(id, side)?;
            Ok(CommandReply::ok(
                format!("agent view on {id}: {}", r.side),
                Some(json!({ "id": id, "side": r.side })),
            ))
        },
    );

    reg(
        router,
        "wall.agent.filter",
        "<id> <reset|all|only|on|off|toggle> [category…]",
        "Filter visible event categories in a terminal's Agent View (e.g. only tools, only read image, reset)",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let raw_mode = args
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("reset")
                .to_lowercase();
            let mode = if raw_mode == "all" {
                "reset"
            } else {
                raw_mode.as_str()
            };
            let cats = args.get(2..).unwrap_or(&[]);
            let r = wall.agent_set_filter(id, mode, cats)?;
            Ok(CommandReply::ok(
                format!("agent view filter on {id} updated"),
                Some(r),
            ))
        },
    );

    reg(
        router,
        "wall.agent.expand",
        "<id> [all|<category…>]",
        "Expand all visible events or a category subset (e.g. tools, reads, read, image, edit, run, think, say)",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let cats = args.get(1..).unwrap_or(&[]);
            let r = wall.agent_set_bulk_expanded(id, true, cats)?;
            Ok(CommandReply::ok(
                format!("expanded {} event(s) on {id}", r.changed),
                Some(json!(r)),
            ))
        },
    );

    reg(
        router,
        "wall.agent.collapse",
        "<id> [all|<category…>]",
        "Collapse all expanded events or a category subset",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let cats = args.get(1..).unwrap_or(&[]);
            let r = wall.agent_set_bulk_expanded(id, false, cats)?;
            Ok(CommandReply::ok(
                format!("collapsed {} event(s) on {id}", r.changed),
                Some(json!(r)),
            ))
        },
    );

    reg(
        router,
        "wall.agent.show",
        "<id> <eventId> [toggle|on|off|full]",
        "Expand or collapse a single event row (`full` shows uncapped file/output lines)",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let event_id = args.get(1).map_or("", String::as_str);
            let how = args.get(2).map_or("toggle", String::as_str);
            if how == "full" {
                wall.agent_toggle_event(id, event_id, "on")?;
                wall.agent_show_full_body(id, event_id)?;
                return Ok(CommandReply::ok(
                    format!("expanded full body of {event_id} on {id}"),
                    Some(json!({
                        "id": id,
                        "eventId": event_id,
                        "expanded": true,
                        "full": true,
                    })),
                ));
            }
            if !matches!(how, "toggle" | "on" | "off") {
                return Err(format!("show takes toggle|on|off|full, not '{how}'"));
            }
            let r = wall.agent_toggle_event(id, event_id, how)?;
            let word = if r.expanded { "expanded" } else { "collapsed" };
            Ok(CommandReply::ok(
                format!("{word} {event_id} on {id}"),
                Some(json!(r)),
            ))
        },
    );

    reg(
        router,
        "wall.agent.jump",
        "<id> [<eventId>|live]",
        "Jump the attached terminal's scrollback view to where an event occurred (or `live` for offset 0)",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let target = args.get(1).map_or("live", String::as_str);
            let r = wall.agent_jump(id, target)?;
            let msg = if r.live {
                format!("{id} scrolled to live")
            } else {
                format!("{id} jumped to {} (offset {})", r.event_id, r.offset)
            };
            Ok(CommandReply::ok(msg, Some(json!(r))))
        },
    );

    reg(
        router,
        "wall.agent.earlier",
        "<id>",
        "Load an earlier window of transcript events into a terminal's Agent View",
        |args, wall| {
            let first_slice = &args[..args.len().min(1)];
            let ids = terminal_ids(wall, first_slice, 1)?;
            let id = &ids[0];
            let r = wall.agent_load_earlier(id)?;
            Ok(CommandReply::ok(
                format!("loaded {} earlier event(s) on {id}", r.loaded),
                Some(json!(r)),
            ))
        },
    );

    reg(
        router,
        "wall.token",
        "<token | url#token=…>",
        "Set the relay capability token and reconnect without reloading the page",
        |args, wall| {
            let Some((bridge_host, bridge_port)) = wall.relay_endpoint() else {
                return Err("the wall is not running".into());
            };
            let raw = args.first().map_or("", |s| s.trim());
            if raw.is_empty() {
                return Err("wall.token takes a token or a URL carrying #token=…".into());
            }
            let tok = if raw.contains("://") {
                let Ok(ep) = parse_relay_endpoint(raw, Some(&bridge_host), false) else {
                    return Err(
                        "wall.token takes a token or a valid URL carrying #token=…".into(),
                    );
                };
                let Some(tok) = ep.token else {
                    return Err(
                        "wall.token takes a token or a valid URL carrying #token=…".into(),
                    );
                };
                let norm_bridge_host = normalize_relay_host(Some(&bridge_host));
                if ep.host != norm_bridge_host || ep.port != bridge_port {
                    return Err(format!(
                        "URL targets {}:{}, but relay is {}:{bridge_port}",
                        ep.host, ep.port, norm_bridge_host
                    ));
                }
                tok
            } else {
                let Some(tok) = parse_relay_token_input(raw) else {
                    return Err("wall.token takes a token or a URL carrying #token=…".into());
                };
                tok
            };
            wall.set_relay_token(&tok)?;
            Ok(CommandReply::ok(
                "relay token updated; reconnecting",
                Some(json!({ "hasToken": true })),
            ))
        },
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::input::Platform;
    use crate::keymap::{help_sections, search_help, HelpVerb};

    #[derive(Clone, Debug, Default)]
    struct FakeTerm {
        name: String,
        hidden: bool,
        selected_text: String,
        pasted: Vec<String>,
        paste_ok: bool,
        alt_screen: bool,
    }

    #[derive(Clone, Debug)]
    struct OpenMenuRecord {
        target: MenuTarget,
        at: Point,
    }

    struct FakeWall {
        order: Vec<String>,
        terms: BTreeMap<String, FakeTerm>,
        groups: BTreeMap<String, GroupSummary>,
        selection: Vec<String>,
        focused_id: Option<String>,
        focus_history: Vec<String>,
        focus_index: isize,
        fits: usize,
        spawn_counter: usize,
        clipboard: String,
        clipboard_err: Option<String>,
        selection_clipboard: String,
        notices: Vec<String>,
        fullscreen: FullscreenStatus,
        panels: BTreeMap<String, PanelPlacement>,
        termd_state: String,
        termd_lost: bool,
        view: (f64, f64),
        menu: Option<OpenMenuRecord>,
        config: BTreeMap<String, Value>,
        bus: BusView,
        attached_map: BTreeMap<String, String>,
        agent_open: BTreeMap<String, (bool, String, Option<String>)>,
        relay: Option<(String, u16)>,
        relay_token: Option<String>,
    }

    impl Default for FakeWall {
        fn default() -> Self {
            let mut config = BTreeMap::new();
            for &key in crate::config::WALL_CONFIG_KEYS {
                config.insert(
                    key.to_string(),
                    default_fallback_knob(key).unwrap(),
                );
            }
            Self {
                order: Vec::new(),
                terms: BTreeMap::new(),
                groups: BTreeMap::new(),
                selection: Vec::new(),
                focused_id: None,
                focus_history: Vec::new(),
                focus_index: -1,
                fits: 0,
                spawn_counter: 0,
                clipboard: String::new(),
                clipboard_err: None,
                selection_clipboard: String::new(),
                notices: Vec::new(),
                fullscreen: FullscreenStatus::default(),
                panels: BTreeMap::new(),
                termd_state: "ok".to_string(),
                termd_lost: false,
                view: (800.0, 600.0),
                menu: None,
                config,
                bus: BusView {
                    snapshot_host: None,
                    reason: "not connected to the relay".to_string(),
                    hosts: Vec::new(),
                },
                attached_map: BTreeMap::new(),
                agent_open: BTreeMap::new(),
                relay: Some(("localhost".to_string(), 8080)),
                relay_token: None,
            }
        }
    }

    impl FakeWall {
        fn add_term(&mut self, id: &str, hidden: bool) {
            self.order.push(id.to_string());
            self.terms.insert(
                id.to_string(),
                FakeTerm {
                    name: id.to_string(),
                    hidden,
                    selected_text: String::new(),
                    pasted: Vec::new(),
                    paste_ok: true,
                    alt_screen: false,
                },
            );
        }

        fn panel_key(kind: PanelKind) -> &'static str {
            match kind {
                PanelKind::Sidebar => "sidebar",
                PanelKind::Status => "status",
                PanelKind::Settings => "settings",
                PanelKind::Help => "help",
            }
        }
    }

    impl WallApi for FakeWall {
        fn snapshot(&self) -> WallSnapshot {
            let terminals = self
                .order
                .iter()
                .filter_map(|id| {
                    let t = self.terms.get(id)?;
                    Some(TerminalSummary {
                        id: id.clone(),
                        name: t.name.clone(),
                        hidden: t.hidden,
                        group: self.group_of(id),
                    })
                })
                .collect();
            let groups = self.groups.values().cloned().collect();
            WallSnapshot { terminals, groups, selection: self.selection.clone() }
        }

        fn has_terminal(&self, id: &str) -> bool {
            self.terms.contains_key(id)
        }

        fn is_shown(&self, id: &str) -> bool {
            self.terms.get(id).is_some_and(|t| !t.hidden)
        }

        fn shown_order(&self) -> Vec<String> {
            self.order
                .iter()
                .filter(|id| self.is_shown(id))
                .cloned()
                .collect()
        }

        fn focused_id(&self) -> Option<String> {
            self.focused_id.clone()
        }

        fn hide(&mut self, id: &str) -> Result<(), String> {
            if let Some(t) = self.terms.get_mut(id) {
                t.hidden = true;
            }
            self.selection.retain(|x| x != id);
            Ok(())
        }

        fn show(&mut self, id: &str) -> Result<(), String> {
            if let Some(t) = self.terms.get_mut(id) {
                t.hidden = false;
            }
            Ok(())
        }

        fn focus_and_frame(&mut self, id: &str) -> Result<(), String> {
            self.focused_id = Some(id.to_string());
            self.fits += 1;
            Ok(())
        }

        fn spawn(&mut self, _at: Option<Point>) -> Result<String, String> {
            self.spawn_counter += 1;
            let id = format!("spawn-{}", self.spawn_counter);
            self.add_term(&id, false);
            self.focused_id = Some(id.clone());
            Ok(id)
        }

        fn fit_all(&mut self) -> Result<(), String> {
            self.fits += 1;
            Ok(())
        }

        fn close_terminal(&mut self, id: &str) -> Result<CommandReply, String> {
            self.terms.remove(id);
            self.order.retain(|x| x != id);
            Ok(CommandReply::ok(format!("closed {id}"), Some(json!({ "id": id }))))
        }

        fn rename_terminal(&mut self, id: &str, name: &str) -> Result<(), String> {
            if let Some(t) = self.terms.get_mut(id) {
                t.name = if name.is_empty() {
                    id.to_string()
                } else {
                    name.to_string()
                };
            }
            Ok(())
        }

        fn terminal_name(&self, id: &str) -> String {
            self.terms
                .get(id)
                .map_or_else(|| id.to_string(), |t| t.name.clone())
        }

        fn selection(&self) -> Vec<String> {
            self.selection.clone()
        }

        fn select(&mut self, ids: &[String]) -> Result<(), String> {
            self.selection = ids
                .iter()
                .filter(|id| self.is_shown(id))
                .cloned()
                .collect();
            Ok(())
        }

        fn terminal_selection_text(&self, id: &str) -> String {
            self.terms
                .get(id)
                .map_or_else(String::new, |t| t.selected_text.clone())
        }

        fn selection_clipboard(&self) -> String {
            self.selection_clipboard.clone()
        }

        fn write_clipboard(&mut self, text: &str) -> Result<(), String> {
            if let Some(err) = &self.clipboard_err {
                return Err(err.clone());
            }
            self.clipboard = text.to_string();
            Ok(())
        }

        fn read_clipboard(&mut self) -> Result<String, String> {
            if let Some(err) = &self.clipboard_err {
                return Err(err.clone());
            }
            Ok(self.clipboard.clone())
        }

        fn paste_into_terminal(&mut self, id: &str, text: &str) -> Result<bool, String> {
            let Some(t) = self.terms.get_mut(id) else {
                return Ok(false);
            };
            if !t.paste_ok {
                return Ok(false);
            }
            t.pasted.push(text.to_string());
            Ok(true)
        }

        fn notify(&mut self, message: &str) {
            self.notices.push(message.to_string());
        }

        fn fullscreen_info(&self) -> FullscreenStatus {
            self.fullscreen.clone()
        }

        fn fullscreen_exit(&mut self) -> Result<(), String> {
            self.fullscreen = FullscreenStatus::default();
            Ok(())
        }

        fn fullscreen_enter_default(&mut self, id: &str) -> Result<bool, String> {
            if !self.is_shown(id) {
                return Ok(false);
            }
            if let Some(gid) = self.group_of(id) {
                let shown_members = self.groups[&gid]
                    .members
                    .iter()
                    .filter(|m| self.is_shown(m))
                    .count();
                if shown_members >= 2 {
                    return self.fullscreen_enter_group(&gid);
                }
            }
            self.fullscreen_enter(id, None)
        }

        fn fullscreen_enter(
            &mut self,
            id: &str,
            return_group: Option<&str>,
        ) -> Result<bool, String> {
            if !self.is_shown(id) {
                return Ok(false);
            }
            self.fullscreen = FullscreenStatus {
                active: true,
                mode: "single".to_string(),
                target: Some(id.to_string()),
                id: Some(id.to_string()),
                group_id: None,
                return_group: return_group.map(str::to_string),
                line_px: 18.0,
                state: Some(json!({ "id": id, "linePx": 18.0 })),
            };
            Ok(true)
        }

        fn fullscreen_enter_group(&mut self, gid: &str) -> Result<bool, String> {
            let Some(g) = self.groups.get(gid) else {
                return Ok(false);
            };
            if !g.members.iter().any(|m| self.is_shown(m)) {
                return Ok(false);
            }
            self.fullscreen = FullscreenStatus {
                active: true,
                mode: "group".to_string(),
                target: Some(gid.to_string()),
                id: None,
                group_id: Some(gid.to_string()),
                return_group: None,
                line_px: 18.0,
                state: Some(json!({ "groupId": gid, "linePx": 18.0 })),
            };
            Ok(true)
        }

        fn fullscreen_toggle_pane(&mut self, id: Option<&str>) -> Result<(), String> {
            if self.fullscreen.mode == "group" {
                let rg = self.fullscreen.group_id.clone();
                let target = id
                    .map(str::to_string)
                    .or_else(|| self.focused_id.clone())
                    .ok_or_else(|| "no terminal to put fullscreen".to_string())?;
                self.fullscreen_enter(&target, rg.as_deref())?;
            } else if let Some(rg) = self.fullscreen.return_group.clone() {
                self.fullscreen_enter_group(&rg)?;
            } else {
                self.fullscreen_exit()?;
            }
            Ok(())
        }

        fn fullscreen_text(&mut self, dir: i32) -> Result<Option<f64>, String> {
            if !self.fullscreen.active {
                return Ok(None);
            }
            self.fullscreen.line_px = match dir {
                1 => self.fullscreen.line_px * 1.1,
                -1 => self.fullscreen.line_px / 1.1,
                _ => 18.0,
            };
            Ok(Some(self.fullscreen.line_px))
        }

        fn has_group(&self, gid: &str) -> bool {
            self.groups.contains_key(gid)
        }

        fn group_of(&self, id: &str) -> Option<String> {
            self.groups
                .iter()
                .find(|(_, g)| g.members.iter().any(|m| m == id))
                .map(|(gid, _)| gid.clone())
        }

        fn group_info(&self, gid: &str) -> Result<GroupSummary, String> {
            self.groups
                .get(gid)
                .cloned()
                .ok_or_else(|| format!("no group '{gid}'"))
        }

        fn group_name(&self, gid: &str) -> Result<String, String> {
            Ok(self.group_info(gid)?.name)
        }

        fn group_create(&mut self, ids: &[String]) -> Result<String, String> {
            let n = self.groups.len() + 1;
            let gid = format!("g-{n}");
            self.groups.insert(
                gid.clone(),
                GroupSummary {
                    id: gid.clone(),
                    name: format!("group {n}"),
                    layout: "tile".to_string(),
                    columns: 2,
                    gap: 2.0,
                    bars: None,
                    members: ids.to_vec(),
                },
            );
            Ok(gid)
        }

        fn group_add(&mut self, gid: &str, ids: &[String]) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            for id in ids {
                if !g.members.contains(id) {
                    g.members.push(id.clone());
                }
            }
            Ok(())
        }

        fn group_remove(&mut self, ids: &[String]) -> Result<(), String> {
            for g in self.groups.values_mut() {
                g.members.retain(|m| !ids.contains(m));
            }
            Ok(())
        }

        fn group_ungroup(&mut self, gid: &str) -> Result<(), String> {
            self.groups
                .remove(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            Ok(())
        }

        fn group_set_layout(
            &mut self,
            gid: &str,
            layout: &str,
            columns: Option<i64>,
            gap: Option<f64>,
        ) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            g.layout = layout.to_string();
            if let Some(c) = columns {
                g.columns = c.max(1) as usize;
            }
            if let Some(gv) = gap {
                g.gap = gv;
            }
            if layout == "split" && g.bars.is_none() {
                g.bars = Some(true);
            }
            Ok(())
        }

        fn group_set_columns(&mut self, gid: &str, columns: usize) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            g.columns = columns;
            Ok(())
        }

        fn group_set_bars(&mut self, gid: &str, on: bool) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            g.bars = Some(on);
            Ok(())
        }

        fn group_set_gap(&mut self, gid: &str, gap: f64) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            g.gap = gap;
            Ok(())
        }

        fn group_rename(&mut self, gid: &str, name: &str) -> Result<(), String> {
            let g = self
                .groups
                .get_mut(gid)
                .ok_or_else(|| format!("no group '{gid}'"))?;
            g.name = if name.is_empty() {
                format!("group {}", gid.trim_start_matches("g-"))
            } else {
                name.to_string()
            };
            Ok(())
        }

        fn group_move(&mut self, gid: &str, _dx: f64, _dy: f64) -> Result<(), String> {
            self.group_info(gid)?;
            Ok(())
        }

        fn frame_group(&mut self, gid: &str) -> Result<bool, String> {
            let g = self.group_info(gid)?;
            let any_shown = g.members.iter().any(|m| self.is_shown(m));
            if any_shown {
                self.fits += 1;
            }
            Ok(any_shown)
        }

        fn split_terminal(&mut self, _dir: &str, id: Option<&str>) -> Result<String, String> {
            let Some(base) = id else {
                return Err("no terminal to split".into());
            };
            let born = self.spawn(None)?;
            if let Some(gid) = self.group_of(base) {
                self.group_add(&gid, std::slice::from_ref(&born))?;
            } else {
                let gid = self.group_create(&[base.to_string(), born.clone()])?;
                self.group_set_layout(&gid, "split", None, None)?;
            }
            Ok(born)
        }

        fn focus_pane(&mut self, dir: &str) -> Result<Option<String>, String> {
            if dir == "right" {
                if let Some(second) = self.shown_order().get(1).cloned() {
                    self.focused_id = Some(second.clone());
                    return Ok(Some(second));
                }
            }
            Ok(None)
        }

        fn group_move_pane(&mut self, id: &str, dir: &str) -> Result<Option<String>, String> {
            let Some(gid) = self.group_of(id) else {
                return Err(format!("'{id}' is in no group"));
            };
            let g = self.groups.get_mut(&gid).unwrap();
            if dir == "right" && g.members.len() >= 2 && g.members[0] == id {
                g.members.swap(0, 1);
                return Ok(Some(g.members[0].clone()));
            }
            Ok(None)
        }

        fn step_focus_history(&mut self, dir: i32) -> Result<Option<String>, String> {
            let next_idx = self.focus_index + dir as isize;
            if next_idx < 0 || next_idx as usize >= self.focus_history.len() {
                return Ok(None);
            }
            self.focus_index = next_idx;
            let id = self.focus_history[next_idx as usize].clone();
            self.focused_id = Some(id.clone());
            Ok(Some(id))
        }

        fn panel(&self, kind: PanelKind) -> PanelPlacement {
            self.panels
                .get(Self::panel_key(kind))
                .cloned()
                .unwrap_or_default()
        }

        fn set_panel(&mut self, kind: PanelKind, patch: PanelPatch) {
            let mut cur = self.panel(kind);
            if let Some(open) = patch.open {
                cur.open = open;
            }
            if let Some(side) = patch.side {
                cur.side = side;
            }
            if let Some(fl) = patch.float {
                cur.float = Some(fl.into());
            }
            if let Some(col) = patch.collapsed {
                cur.collapsed = col;
            }
            self.panels.insert(Self::panel_key(kind).to_string(), cur);
        }

        fn termd_state(&self) -> String {
            self.termd_state.clone()
        }

        fn termd_dismiss_lost(&mut self) -> bool {
            let was = self.termd_lost;
            self.termd_lost = false;
            was
        }

        fn view_size(&self) -> (f64, f64) {
            self.view
        }

        fn terminal_screen_box(&self, _id: &str) -> Result<ScreenBox, String> {
            Ok(ScreenBox { left: 100.0, top: 50.0, right: 200.0, bottom: 70.0 })
        }

        fn group_header_screen_box(&self, gid: &str) -> Result<Option<ScreenBox>, String> {
            if self.groups.contains_key(gid) {
                Ok(Some(ScreenBox { left: 100.0, top: 50.0, right: 200.0, bottom: 70.0 }))
            } else {
                Ok(None)
            }
        }

        fn open_menu(&mut self, target: MenuTarget, at: Point) -> Result<Value, String> {
            self.menu = Some(OpenMenuRecord { target: target.clone(), at });
            Ok(json!({ "target": target, "at": { "x": at.x, "y": at.y } }))
        }

        fn close_menu(&mut self) {
            self.menu = None;
        }

        fn config_entries(&self) -> Vec<(String, Value)> {
            self.config
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }

        fn config_get(&self, key: &str) -> Result<Value, String> {
            self.config.get(key).cloned().ok_or_else(|| {
                let names = crate::config::WALL_CONFIG_KEYS.join(", ");
                format!("unknown wall knob '{key}' (knobs: {names})")
            })
        }

        fn config_set(&mut self, key: &str, value: Value) -> Result<Value, String> {
            self.config.insert(key.to_string(), value.clone());
            Ok(value)
        }

        fn config_reset(&mut self, key: &str) -> Result<Value, String> {
            let v = default_fallback_knob(key)?;
            self.config.insert(key.to_string(), v.clone());
            Ok(v)
        }

        fn bus_view(&self) -> BusView {
            self.bus.clone()
        }

        fn terminals_attached_to(&self, target: &str) -> Vec<String> {
            self.attached_map
                .iter()
                .filter(|(_, t)| t.as_str() == target)
                .map(|(id, _)| id.clone())
                .collect()
        }

        fn attach_agent(&mut self, target: &str) -> Result<AttachOutcome, String> {
            if target.starts_with("dead-") {
                return Err(format!("'{target}' has no live session to attach to"));
            }
            if let Some((existing_id, _)) =
                self.attached_map.iter().find(|(_, t)| t.as_str() == target)
            {
                return Ok(AttachOutcome { id: existing_id.clone(), existing: true });
            }
            let id = if target.contains('@') {
                "term-remote-1".to_string()
            } else {
                gomux_screen_id(target)
            };
            self.add_term(&id, false);
            self.attached_map.insert(id.clone(), target.to_string());
            Ok(AttachOutcome { id, existing: false })
        }

        fn agent_set_open(&mut self, id: &str, how: &str) -> Result<AgentOpenResult, String> {
            if !self.has_terminal(id) {
                return Err(format!("no terminal '{id}'"));
            }
            let entry = self.agent_open.entry(id.to_string()).or_insert((
                false,
                "right".to_string(),
                Some("sess-1".to_string()),
            ));
            match how {
                "close" | "off" => entry.0 = false,
                "open" | "on" => entry.0 = true,
                "toggle" => entry.0 = !entry.0,
                side if AGENT_SIDES.contains(&side) => {
                    entry.0 = true;
                    entry.1 = side.to_string();
                }
                other => return Err(format!("unknown agent view mode '{other}'")),
            }
            Ok(AgentOpenResult {
                open: entry.0,
                side: entry.1.clone(),
                session: entry.2.clone(),
                missing: None,
            })
        }

        fn agent_set_side(&mut self, id: &str, side: &str) -> Result<AgentSideResult, String> {
            let entry = self.agent_open.entry(id.to_string()).or_insert((
                true,
                side.to_string(),
                Some("sess-1".to_string()),
            ));
            entry.1 = side.to_string();
            Ok(AgentSideResult { side: side.to_string() })
        }

        fn agent_set_filter(
            &mut self,
            _id: &str,
            mode: &str,
            categories: &[String],
        ) -> Result<Value, String> {
            Ok(json!({ "mode": mode, "categories": categories }))
        }

        fn agent_set_bulk_expanded(
            &mut self,
            _id: &str,
            expanded: bool,
            _categories: &[String],
        ) -> Result<AgentBulkExpandResult, String> {
            Ok(AgentBulkExpandResult { changed: usize::from(expanded) })
        }

        fn agent_toggle_event(
            &mut self,
            _id: &str,
            event_id: &str,
            how: &str,
        ) -> Result<AgentToggleEventResult, String> {
            Ok(AgentToggleEventResult {
                event_id: event_id.to_string(),
                expanded: how != "off",
            })
        }

        fn agent_show_full_body(&mut self, _id: &str, _event_id: &str) -> Result<(), String> {
            Ok(())
        }

        fn agent_jump(&mut self, id: &str, target: &str) -> Result<AgentJumpResult, String> {
            if self.terms.get(id).is_some_and(|t| t.alt_screen) {
                let msg = format!("{id} is on the alternate screen");
                self.notify(&msg);
                return Err(msg);
            }
            if target == "live" {
                Ok(AgentJumpResult {
                    live: true,
                    event_id: "live".to_string(),
                    offset: 0,
                })
            } else {
                Ok(AgentJumpResult {
                    live: false,
                    event_id: target.to_string(),
                    offset: 56,
                })
            }
        }

        fn agent_load_earlier(&mut self, _id: &str) -> Result<AgentEarlierResult, String> {
            Ok(AgentEarlierResult { loaded: 1 })
        }

        fn relay_endpoint(&self) -> Option<(String, u16)> {
            self.relay.clone()
        }

        fn set_relay_token(&mut self, token: &str) -> Result<(), String> {
            self.relay_token = Some(token.to_string());
            Ok(())
        }
    }

    fn wall_router() -> CommandRouter<FakeWall> {
        let mut r = CommandRouter::new(FakeWall::default());
        register_wall_commands(&mut r);
        r
    }

    #[test]
    fn registers_all_54_wall_verbs_plus_call_and_help_and_feeds_help_sections() {
        let r = wall_router();
        let cmds = r.list_commands();
        assert_eq!(cmds.len(), 56);

        let help_verbs: Vec<HelpVerb<'_>> = cmds
            .iter()
            .map(|c| HelpVerb {
                name: &c.name,
                usage: &c.usage,
                description: &c.description,
            })
            .collect();
        let sections = help_sections(Platform::Mac, "ctrl alt meta", &help_verbs).unwrap();
        let found = search_help(&sections, "hide terminal");
        assert!(found
            .iter()
            .flat_map(|s| s.rows.iter().map(|row| row.how.as_str()))
            .any(|h| h == "wall.hide <id…>"));
    }

    // ── Port of verb assertions in tools/wall-settings.test.mjs ──────────────

    #[test]
    fn snap_grid_config_config_find_and_settings_panel_verbs_match_js() {
        let mut r = wall_router();
        assert_eq!(r.execute(["wall.snap"]).text, "OK: snap off");
        assert_eq!(r.execute(["wall.snap", "on"]).text, "OK: snap on");
        assert_eq!(r.execute(["wall.grid", "off"]).text, "OK: grid hidden");
        assert!(r
            .execute(["wall.grid", "maybe"])
            .text
            .starts_with("ERR: takes toggle|on|off"));
        assert_eq!(
            r.execute(["wall.config", "background", "#203040"]).text,
            "OK: background #203040"
        );
        assert!(r
            .execute(["wall.config", "gridMinor", "-2"])
            .text
            .starts_with("ERR: gridMinor"));
        assert_eq!(
            r.execute(["wall.config", "background", "default"]).text,
            "OK: background #07080b"
        );

        let found = r.execute(["wall.config.find", "major"]);
        assert_eq!(
            found.data.unwrap()["sections"],
            json!([{ "id": "canvas", "keys": ["gridMajorEvery", "gridMinPx", "gridMajorColor"] }])
        );
        assert!(r
            .execute(["wall.config.find", "zebra"])
            .text
            .contains("no knob matches"));

        assert_eq!(r.execute(["wall.settings"]).text, "OK: settings open, float");
        assert_eq!(
            r.execute(["wall.settings", "right"]).text,
            "OK: settings open, right"
        );
        assert_eq!(
            r.execute(["wall.settings.collapse", "groups"]).text,
            "OK: groups folded"
        );
        assert!(r
            .execute(["wall.settings.collapse", "nope"])
            .text
            .starts_with("ERR: no section 'nope'"));
        assert_eq!(r.execute(["wall.settings", "close"]).text, "OK: settings closed");
    }

    // ── Port of verb assertions in tools/wall-menu.test.mjs ──────────────────

    #[test]
    fn menu_select_all_fit_and_spawn_verbs_match_js() {
        let mut r = wall_router();
        r.context.add_term("t1", false);
        r.context.add_term("t2", true);
        let gid = r.context.group_create(&["t1".into()]).unwrap();
        assert_eq!(gid, "g-1");

        let m1 = r.execute(["wall.menu", "terminal", "t1"]);
        assert!(m1.text.starts_with("OK"));
        let menu = r.context.menu.as_ref().unwrap();
        assert_eq!(
            menu.target,
            MenuTarget::Terminal { id: "t1".into(), part: "body".into() }
        );
        assert_eq!(menu.at, Point::new(112.0, 62.0));

        r.execute(["wall.menu", "canvas"]);
        let menu = r.context.menu.as_ref().unwrap();
        assert_eq!(menu.target, MenuTarget::Canvas);
        assert_eq!(menu.at, Point::new(400.0, 300.0));

        r.execute(["wall.menu", "group", "g-1", "5", "6"]);
        assert_eq!(r.context.menu.as_ref().unwrap().at, Point::new(5.0, 6.0));

        assert!(r
            .execute(["wall.menu", "terminal", "t2"])
            .text
            .contains("ERR: terminal 't2' is hidden"));
        assert!(r
            .execute(["wall.menu", "terminal", "zz"])
            .text
            .contains("ERR: no terminal 'zz'"));
        assert!(r
            .execute(["wall.menu", "nowhere"])
            .text
            .contains("ERR: the menu opens on terminal|group|canvas"));
        assert!(r.execute(["wall.menu", "close"]).text.starts_with("OK"));
        assert!(r.context.menu.is_none());

        r.execute(["wall.select", "all"]);
        assert_eq!(r.context.selection, vec!["t1".to_string()]);
        assert!(r.execute(["wall.fit"]).text.starts_with("OK"));
        assert_eq!(r.context.fits, 1);
        assert!(r
            .execute(["wall.spawn", "x"])
            .text
            .contains("ERR: x takes a number"));
    }

    // ── Port of copy / paste verb assertions in tools/wall-select.test.mjs ───

    #[test]
    fn copy_and_paste_verbs_and_clipboard_refusals_match_js() {
        let mut r = wall_router();
        r.context.add_term("t", false);
        r.context.add_term("u", false);
        r.context.terms.get_mut("t").unwrap().selected_text = "make test".into();
        r.context.focused_id = Some("t".into());

        let cp = r.execute(["wall.copy"]);
        assert_eq!(cp.text, "OK: copied 9 characters from t");
        assert_eq!(r.context.clipboard, "make test");

        r.context.clipboard = "git status".into();
        let ps = r.execute(["wall.paste", "t"]);
        assert!(ps.text.starts_with("OK: pasted 10 characters into t"));

        r.context.selection_clipboard = "echo sel".into();
        let ps_sel = r.execute(["wall.paste", "selection"]);
        assert!(ps_sel
            .text
            .starts_with("OK: pasted 8 characters into t from the selection clipboard"));
        assert_eq!(
            r.context.terms["t"].pasted,
            vec!["git status".to_string(), "echo sel".to_string()]
        );

        assert_eq!(
            r.execute(["wall.copy", "u"]).text,
            "ERR: nothing is selected in u"
        );
        assert_eq!(
            r.execute(["wall.copy", "nope"]).text,
            "ERR: no terminal 'nope'"
        );

        r.context.clipboard_err =
            Some("the browser refused the clipboard (NotAllowedError: Write permission denied.)".into());
        let refused_copy = r.execute(["wall.copy", "t"]);
        assert!(refused_copy
            .text
            .starts_with("ERR: the browser refused the clipboard (NotAllowedError:"));
        assert!(r
            .context
            .notices
            .last()
            .unwrap()
            .starts_with("copy: the browser refused the clipboard"));

        r.context.clipboard_err =
            Some("the browser refused to read the clipboard".into());
        let refused_paste = r.execute(["wall.paste", "t"]);
        assert!(refused_paste
            .text
            .starts_with("ERR: the browser refused to read the clipboard"));
        assert!(r.context.notices.last().unwrap().starts_with("paste:"));
    }

    // ── Port of fullscreen & focus-history verb assertions (wall-fullscreen.test.mjs) ──

    #[test]
    fn fullscreen_pane_text_and_focus_history_verbs_match_js() {
        let mut r = wall_router();
        r.context.add_term("a", false);
        r.context.add_term("b", false);
        let gid = r
            .context
            .group_create(&["a".into(), "b".into()])
            .unwrap();
        r.context
            .group_set_layout(&gid, "split", Some(2), None)
            .unwrap();
        r.context.focused_id = Some("a".into());
        r.context.focus_history = vec!["a".into(), "c".into(), "d".into()];
        r.context.focus_index = 2;

        assert_eq!(
            r.execute(["wall.focus.history", "back"]).data.unwrap()["id"],
            json!("c")
        );
        assert_eq!(
            r.execute(["wall.focus.history", "forward"]).data.unwrap()["id"],
            json!("d")
        );

        // Focused 'a' in 2-pane split: wall.fullscreen toggle enters group fullscreen.
        r.context.focused_id = Some("a".into());
        let enter_grp = r.execute(["wall.fullscreen", "toggle"]);
        assert_eq!(enter_grp.text, format!("OK: fullscreen {gid}: 18.0 px a row"));
        assert_eq!(r.context.fullscreen.mode, "group");

        // wall.fullscreen.pane toggle zooms 'a' solo with returnGroup = gid, then back.
        let zoom_pane = r.execute(["wall.fullscreen.pane", "toggle"]);
        assert_eq!(zoom_pane.text, "OK: fullscreen a: 18.0 px a row");
        assert_eq!(r.context.fullscreen.return_group.as_deref(), Some(gid.as_str()));

        let text_up = r.execute(["wall.fullscreen.text", "bigger"]);
        assert_eq!(text_up.text, "OK: fullscreen text 19.8 px a row");

        let unzoom_pane = r.execute(["wall.fullscreen.pane", "toggle"]);
        assert_eq!(unzoom_pane.text, format!("OK: fullscreen {gid}: 18.0 px a row"));

        let exit_fs = r.execute(["wall.fullscreen", "toggle"]);
        assert_eq!(exit_fs.text, "OK: fullscreen off");
        assert!(!r.context.fullscreen.active);
    }

    // ── Port of bus, attach, agent-view & token verb assertions ──────────────

    #[test]
    fn bus_attach_agent_view_and_token_verbs_match_js() {
        let mut r = wall_router();
        r.context.add_term("t1", false);
        r.context.focused_id = Some("t1".into());

        // wall.bus before snapshot refuses with reason, then lists hosts and agents once up.
        assert_eq!(
            r.execute(["wall.bus"]).text,
            "ERR: no gomuxai agents: not connected to the relay"
        );
        r.context.bus = BusView {
            snapshot_host: Some("laptop-2".into()),
            reason: String::new(),
            hosts: vec![BusHostGroup {
                host: "laptop-2".into(),
                agents: vec![
                    BusAgentEntry {
                        name: "local-agy".into(),
                        target: "local-agy@laptop-2".into(),
                        status: Some("waiting".into()),
                        end_reason: None,
                        detail: Some("prompt".into()),
                        attached: Some(true),
                    },
                    BusAgentEntry {
                        name: "dead-agy".into(),
                        target: "dead-agy@laptop-2".into(),
                        status: Some("ended".into()),
                        end_reason: Some("gone".into()),
                        detail: None,
                        attached: Some(false),
                    },
                ],
            }],
        };

        let attach_local = r.execute(["wall.attach", "local-agy"]);
        assert_eq!(attach_local.text, "OK: gx-local-agy attaches to local-agy");
        let attach_again = r.execute(["wall.attach", "local-agy"]);
        assert_eq!(
            attach_again.text,
            "OK: revealed gx-local-agy, attached to local-agy"
        );
        let attach_dead = r.execute(["wall.attach", "dead-agy@laptop-2"]);
        assert!(attach_dead
            .text
            .contains("ERR: 'dead-agy@laptop-2' has no live session to attach to"));
        assert!(r
            .context
            .notices
            .last()
            .unwrap()
            .starts_with("attach: 'dead-agy@laptop-2'"));

        let bus_list = r.execute(["wall.bus"]);
        assert!(bus_list.text.contains("local-agy@laptop-2 waiting · prompt · on gx-local-agy"));
        assert!(bus_list.text.contains("dead-agy@laptop-2 ended (gone) · no live session"));

        // wall.agent* verbs (wall-agent-view.test.mjs)
        r.context.focused_id = Some("t1".into());
        assert_eq!(
            r.execute(["wall.agent"]).text,
            "OK: agent view on t1: open (right) · session sess-1"
        );
        assert_eq!(
            r.execute(["wall.agent.jump", "t1", "t-read"]).text,
            "OK: t1 jumped to t-read (offset 56)"
        );
        assert_eq!(
            r.execute(["wall.agent.jump", "t1", "live"]).text,
            "OK: t1 scrolled to live"
        );
        r.context.terms.get_mut("t1").unwrap().alt_screen = true;
        assert!(r
            .execute(["wall.agent.jump", "t1", "t-read"])
            .text
            .contains("alternate screen"));
        r.context.terms.get_mut("t1").unwrap().alt_screen = false;

        assert_eq!(
            r.execute(["wall.agent.show", "t1", "t-read", "on"]).text,
            "OK: expanded t-read on t1"
        );
        assert_eq!(
            r.execute(["wall.agent.show", "t1", "t-read", "full"]).text,
            "OK: expanded full body of t-read on t1"
        );
        assert_eq!(
            r.execute(["wall.agent.collapse", "t1", "all"]).text,
            "OK: collapsed 0 event(s) on t1"
        );
        assert_eq!(
            r.execute(["wall.agent.expand", "t1", "tools"]).text,
            "OK: expanded 1 event(s) on t1"
        );
        assert_eq!(
            r.execute(["wall.agent.earlier", "t1"]).text,
            "OK: loaded 1 earlier event(s) on t1"
        );
        assert_eq!(
            r.execute(["wall.agent.side", "t1", "left"]).text,
            "OK: agent view on t1: left"
        );
        assert_eq!(
            r.execute(["wall.agent.filter", "t1", "only", "tools"]).text,
            "OK: agent view filter on t1 updated"
        );
        assert_eq!(
            r.execute(["wall.agent", "t1", "close"]).text,
            "OK: agent view on t1: closed"
        );

        // wall.token + relay-auth.test.mjs endpoint / cross-port / cross-host / secret checks
        let ok_tok = r.execute(["wall.token", "http://localhost:5180/?relay=8080#token=good_tok_1"]);
        assert_eq!(ok_tok.text, "OK: relay token updated; reconnecting");
        assert_eq!(r.context.relay_token.as_deref(), Some("good_tok_1"));

        let wrong_port = r.execute([
            "wall.token",
            "http://localhost:8182/#token=wrong_port_tok_666",
        ]);
        assert!(wrong_port.is_err());
        assert!(!wrong_port.text.contains("wrong_port_tok_666"));
        assert_eq!(r.context.relay_token.as_deref(), Some("good_tok_1"));

        let wrong_host = r.execute([
            "wall.token",
            "http://other.example:8080/#token=wrong_host_tok_777",
        ]);
        assert!(wrong_host.is_err());
        assert!(!wrong_host.text.contains("wrong_host_tok_777"));

        let query_leak = r.execute([
            "wall.token",
            "http://localhost:8080/?token=leak_secret_999",
        ]);
        assert!(query_leak.is_err());
        assert!(!query_leak.text.contains("leak_secret_999"));
    }

    #[test]
    fn group_list_sidebar_and_status_verbs_match_js() {
        let mut r = wall_router();
        r.context.add_term("a", false);
        r.context.add_term("b", false);
        r.context.add_term("c", true);
        r.context.selection = vec!["a".into(), "b".into()];

        let g = r.execute(["wall.group"]);
        assert_eq!(g.text, "OK: g-1 \"group 1\" gathers a, b");
        assert!(r.context.selection.is_empty());

        assert_eq!(
            r.execute(["wall.group.columns", "g-1", "-1"]).text,
            "OK: g-1 tiles in 1"
        );
        assert_eq!(
            r.execute(["wall.group.columns", "g-1", "+1"]).text,
            "OK: g-1 tiles in 2"
        );
        assert_eq!(
            r.execute(["wall.group.layout", "g-1", "split"]).text,
            "OK: g-1 is split"
        );
        assert_eq!(
            r.execute(["wall.group.bars", "g-1"]).text,
            "OK: g-1 bars hidden"
        );
        assert_eq!(
            r.execute(["wall.pane.move", "right", "a"]).text,
            "OK: a traded places with b"
        );
        assert_eq!(
            r.execute(["wall.sidebar.collapse", "g-1"]).text,
            "OK: g-1 folded"
        );
        assert_eq!(
            r.execute(["wall.status", "float", "10", "20"]).text,
            "OK: status window open, float · termd ok"
        );
        assert_eq!(
            r.execute(["wall.status.dismiss"]).text,
            "OK: no termd-lost notice to dismiss"
        );

        let list = r.execute(["wall.list"]);
        assert!(list.text.contains("g-1 \"group 1\""));
        assert!(list.text.contains("ungrouped\n  c (hidden) \"c\""));
    }
}
