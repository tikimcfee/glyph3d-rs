//! Port of `wall/WallStore.js`: the wall's durable state persisted as a JSON file on disk
//! in place of browser `localStorage`.
//!
//! Stores terminal positions, custom names, attached gomuxai targets, hidden flags, groups
//! (including split trees), panel placements (`sidebar`, `status`, `settings`, `help`),
//! dialled configuration overrides, fullscreen state and per-terminal fullscreen text sizes,
//! and Agent View entries. Each store instance is scoped by a name passed at construction
//! (where the JS app scopes by relay port), pruned against live terminal ids on connect, and
//! written atomically via a sibling temporary file renamed into place.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::{AgentSide, GroupLayout};
use crate::geom::Point;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Backing persistence seam for [`WallStore`], allowing disk directories in production and
/// temporary directories or custom in-memory/failing backends in tests.
pub trait Storage {
    /// Load the raw JSON document for `scope`, returning `Ok(None)` when none exists yet.
    fn load(&self, scope: &str) -> io::Result<Option<Vec<u8>>>;

    /// Persist `bytes` whole for `scope`.
    fn save(&self, scope: &str, bytes: &[u8]) -> io::Result<()>;
}

/// File-backed [`Storage`] writing `<dir>/wall-<scope>.json` atomically via a temporary sibling
/// file followed by `fs::rename`.
#[derive(Clone, Debug)]
pub struct DiskStorage {
    dir: PathBuf,
}

impl DiskStorage {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        DiskStorage { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the JSON file for `scope` inside this directory.
    pub fn path_for(&self, scope: &str) -> PathBuf {
        Self::file_path(&self.dir, scope)
    }

    /// Injective, path-traversal-safe filename for `scope` inside `dir`.
    pub fn file_path(dir: &Path, scope: &str) -> PathBuf {
        let mut safe = String::with_capacity(scope.len());
        if scope.is_empty() {
            safe.push_str("default");
        } else {
            for b in scope.bytes() {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                    safe.push(char::from(b));
                } else {
                    safe.push_str(&format!("%{b:02X}"));
                }
            }
        }
        dir.join(format!("wall-{safe}.json"))
    }
}

impl Storage for DiskStorage {
    fn load(&self, scope: &str) -> io::Result<Option<Vec<u8>>> {
        let path = self.path_for(scope);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn save(&self, scope: &str, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(scope);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("wall-store.json");
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = self
            .dir
            .join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));
        if let Err(err) = fs::write(&temp_path, bytes) {
            let _ = fs::remove_file(&temp_path);
            return Err(err);
        }
        if let Err(err) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(err);
        }
        Ok(())
    }
}

/// Scoped logical key names matching `WallStore.keys` (`glyph3d.wall.<scope>.<what>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreKeys {
    pub positions: String,
    pub names: String,
    pub hidden: String,
    pub groups: String,
    pub sidebar: String,
    pub status: String,
    pub config: String,
    pub settings: String,
    pub help: String,
    pub fullscreen: String,
    pub fullscreen_text: String,
    pub agent_views: String,
    pub attached: String,
}

impl StoreKeys {
    pub fn new(scope: &str) -> Self {
        let k = |what: &str| format!("glyph3d.wall.{scope}.{what}");
        StoreKeys {
            positions: k("positions"),
            names: k("names"),
            hidden: k("hidden"),
            groups: k("groups"),
            sidebar: k("sidebar"),
            status: k("status"),
            config: k("config"),
            settings: k("settings"),
            help: k("help"),
            fullscreen: k("fullscreen"),
            fullscreen_text: k("fullscreenText"),
            agent_views: k("agentViews"),
            attached: k("attached"),
        }
    }
}

/// Serde mirror for [`Point`] so `geom.rs` stays untouched.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct StoredPoint {
    x: f64,
    y: f64,
}

impl From<Point> for StoredPoint {
    fn from(p: Point) -> Self {
        StoredPoint { x: p.x, y: p.y }
    }
}

impl From<StoredPoint> for Point {
    fn from(p: StoredPoint) -> Self {
        Point::new(p.x, p.y)
    }
}

mod serde_point {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::StoredPoint;
    use crate::geom::Point;

    pub fn serialize<S: Serializer>(point: &Point, serializer: S) -> Result<S::Ok, S::Error> {
        StoredPoint::from(*point).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Point, D::Error> {
        StoredPoint::deserialize(deserializer).map(Point::from)
    }
}

mod serde_opt_point {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::StoredPoint;
    use crate::geom::Point;

    pub fn serialize<S: Serializer>(
        point: &Option<Point>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        point.map(StoredPoint::from).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Point>, D::Error> {
        Option::<StoredPoint>::deserialize(deserializer).map(|opt| opt.map(Point::from))
    }
}

/// Axis along which a split branch divides its rectangle (`splitTree.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Row,
    Col,
}

/// Binary split tree stored inside [`WallSplit`] (`splitTree.SplitNode`).
///
/// Defined here so `store.rs` compiles and collapses departed panes independently while
/// `split.rs` is ported on its own branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SplitNode {
    Leaf {
        id: String,
    },
    Branch {
        dir: SplitDir,
        ratio: f64,
        a: Box<SplitNode>,
        b: Box<SplitNode>,
    },
}

/// Remove leaf `target_id` from `node`, handing its room to its sibling (`splitTree.remove`).
fn remove_pane(node: &SplitNode, target_id: &str) -> Option<SplitNode> {
    match node {
        SplitNode::Leaf { id } => {
            if id == target_id {
                None
            } else {
                Some(node.clone())
            }
        }
        SplitNode::Branch { dir, ratio, a, b } => {
            let next_a = remove_pane(a, target_id);
            let next_b = remove_pane(b, target_id);
            match (next_a, next_b) {
                (None, other) => other,
                (other, None) => other,
                (Some(na), Some(nb)) => Some(SplitNode::Branch {
                    dir: *dir,
                    ratio: *ratio,
                    a: Box::new(na),
                    b: Box::new(nb),
                }),
            }
        }
    }
}

/// Persisted split geometry and pane tree of a `split`-layout [`WallGroup`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WallSplit {
    pub w: f64,
    pub h: f64,
    pub bars: bool,
    pub tree: SplitNode,
}

/// A terminal group as persisted in [`WallStore`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WallGroup {
    pub name: String,
    pub layout: GroupLayout,
    pub columns: u32,
    pub gap: f64,
    #[serde(default, with = "serde_opt_point")]
    pub origin: Option<Point>,
    pub members: Vec<String>,
    #[serde(default)]
    pub split: Option<WallSplit>,
}

/// Edge docking or floating mode shared by the wall's panels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelSide {
    Left,
    Right,
    #[default]
    Float,
}

/// Persisted sidebar placement and folded group ids (`WallStore.SIDEBAR_DEFAULT`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidebarState {
    pub open: bool,
    pub side: PanelSide,
    #[serde(with = "serde_point")]
    pub float: Point,
    pub collapsed: Vec<String>,
}

impl Default for SidebarState {
    fn default() -> Self {
        SidebarState {
            open: false,
            side: PanelSide::Right,
            float: Point::new(80.0, 80.0),
            collapsed: Vec::new(),
        }
    }
}

/// Partial update to [`SidebarState`] (`WallStore.setSidebar`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarPatch {
    pub open: Option<bool>,
    pub side: Option<PanelSide>,
    pub float: Option<Point>,
    pub collapsed: Option<Vec<String>>,
}

impl From<SidebarState> for SidebarPatch {
    fn from(s: SidebarState) -> Self {
        SidebarPatch {
            open: Some(s.open),
            side: Some(s.side),
            float: Some(s.float),
            collapsed: Some(s.collapsed),
        }
    }
}

/// Persisted placement of the termd status window (`WallStore.STATUS_WINDOW_DEFAULT`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusWindowState {
    pub open: bool,
    pub side: PanelSide,
    #[serde(default, with = "serde_opt_point")]
    pub float: Option<Point>,
}

impl Default for StatusWindowState {
    fn default() -> Self {
        StatusWindowState {
            open: false,
            side: PanelSide::Float,
            float: None,
        }
    }
}

/// Partial update to [`StatusWindowState`] (`WallStore.setStatusWindow`).
///
/// `float: None` leaves the existing floating position untouched; `float: Some(None)` clears it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusWindowPatch {
    pub open: Option<bool>,
    pub side: Option<PanelSide>,
    pub float: Option<Option<Point>>,
}

impl From<StatusWindowState> for StatusWindowPatch {
    fn from(s: StatusWindowState) -> Self {
        StatusWindowPatch {
            open: Some(s.open),
            side: Some(s.side),
            float: Some(s.float),
        }
    }
}

/// Persisted placement and folded sections of the settings panel (`WallStore.SETTINGS_PANEL_DEFAULT`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsPanelState {
    pub open: bool,
    pub side: PanelSide,
    #[serde(default, with = "serde_opt_point")]
    pub float: Option<Point>,
    pub collapsed: Vec<String>,
}

impl Default for SettingsPanelState {
    fn default() -> Self {
        SettingsPanelState {
            open: false,
            side: PanelSide::Float,
            float: None,
            collapsed: Vec::new(),
        }
    }
}

/// Partial update to [`SettingsPanelState`] (`WallStore.setSettingsPanel`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsPanelPatch {
    pub open: Option<bool>,
    pub side: Option<PanelSide>,
    pub float: Option<Option<Point>>,
    pub collapsed: Option<Vec<String>>,
}

impl From<SettingsPanelState> for SettingsPanelPatch {
    fn from(s: SettingsPanelState) -> Self {
        SettingsPanelPatch {
            open: Some(s.open),
            side: Some(s.side),
            float: Some(s.float),
            collapsed: Some(s.collapsed),
        }
    }
}

/// Persisted placement of the help panel (`WallStore.HELP_PANEL_DEFAULT`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelpPanelState {
    pub open: bool,
    pub side: PanelSide,
    #[serde(default, with = "serde_opt_point")]
    pub float: Option<Point>,
}

impl Default for HelpPanelState {
    fn default() -> Self {
        HelpPanelState {
            open: false,
            side: PanelSide::Float,
            float: None,
        }
    }
}

/// Partial update to [`HelpPanelState`] (`WallStore.setHelpPanel`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HelpPanelPatch {
    pub open: Option<bool>,
    pub side: Option<PanelSide>,
    pub float: Option<Option<Point>>,
}

impl From<HelpPanelState> for HelpPanelPatch {
    fn from(s: HelpPanelState) -> Self {
        HelpPanelPatch {
            open: Some(s.open),
            side: Some(s.side),
            float: Some(s.float),
        }
    }
}

/// Saved pre-fullscreen canvas geometry of one terminal (`WallFullscreen.SavedCanvasPane`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedCanvasPane {
    pub cols: u32,
    pub rows: u32,
    pub x: f64,
    pub y: f64,
}

/// Orthographic camera pose saved when entering fullscreen.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraPose {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

/// Whether fullscreen elevates a single pane or an entire group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenMode {
    Single,
    Group,
}

/// Persisted fullscreen state (`WallFullscreen.FullscreenState`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullscreenState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<FullscreenMode>,
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solo_override: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<SavedCanvasPane>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<BTreeMap<String, SavedCanvasPane>>,
    pub camera: CameraPose,
    pub line_px: f64,
    pub entry_line_px: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_scale: Option<f64>,
}

fn default_agent_session() -> String {
    "auto".to_string()
}

/// Persisted per-terminal Agent View state (`WallStore._agentViews[id]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentViewState {
    pub open: bool,
    pub side: AgentSide,
    #[serde(default = "default_agent_session")]
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<BTreeMap<String, bool>>,
}

/// On-disk JSON document with the exact 13 section keys and shapes of `WallStore.js`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreDocumentRef<'a> {
    positions: &'a BTreeMap<String, StoredPoint>,
    names: &'a BTreeMap<String, String>,
    attached: &'a BTreeMap<String, String>,
    hidden: &'a Vec<String>,
    groups: &'a BTreeMap<String, WallGroup>,
    sidebar: &'a SidebarState,
    status: &'a StatusWindowState,
    config: &'a BTreeMap<String, serde_json::Value>,
    settings: &'a SettingsPanelState,
    help: &'a HelpPanelState,
    fullscreen: &'a Option<FullscreenState>,
    fullscreen_text: &'a BTreeMap<String, f64>,
    agent_views: &'a BTreeMap<String, AgentViewState>,
}

/// Durable wall state scoped by name and persisted atomically through [`Storage`].
pub struct WallStore {
    scope: String,
    keys: StoreKeys,
    file_path: Option<PathBuf>,
    storage: Box<dyn Storage>,
    positions: BTreeMap<String, Point>,
    names: BTreeMap<String, String>,
    attached: BTreeMap<String, String>,
    hidden: BTreeSet<String>,
    groups: BTreeMap<String, WallGroup>,
    sidebar: SidebarState,
    status: StatusWindowState,
    config: BTreeMap<String, serde_json::Value>,
    settings: SettingsPanelState,
    help: HelpPanelState,
    fullscreen: Option<FullscreenState>,
    fullscreen_text: BTreeMap<String, f64>,
    agent_views: BTreeMap<String, AgentViewState>,
}

impl WallStore {
    /// Open a disk-backed store inside `dir`, scoped by `scope` (`<dir>/wall-<scope>.json`).
    pub fn open(dir: impl Into<PathBuf>, scope: impl Into<String>) -> Self {
        let scope_str = scope.into();
        let disk = DiskStorage::new(dir);
        let path = disk.path_for(&scope_str);
        let mut store = Self::with_storage(Box::new(disk), scope_str);
        store.file_path = Some(path);
        store
    }

    /// Build a store over a custom [`Storage`] backend, scoped by `scope`.
    ///
    /// Missing or unreadable storage starts cleanly from defaults, matching `WallStore.js`
    /// when browser storage is blocked.
    pub fn with_storage(storage: Box<dyn Storage>, scope: impl Into<String>) -> Self {
        let scope = scope.into();
        let keys = StoreKeys::new(&scope);
        let root_obj = storage
            .load(&scope)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());

        let field = |name: &str| -> Option<&serde_json::Value> {
            root_obj.as_ref()?.as_object()?.get(name)
        };

        let positions: BTreeMap<String, Point> = field("positions")
            .and_then(|v| serde_json::from_value::<BTreeMap<String, StoredPoint>>(v.clone()).ok())
            .map(|m| m.into_iter().map(|(k, p)| (k, Point::from(p))).collect())
            .unwrap_or_default();

        let names: BTreeMap<String, String> = field("names")
            .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()).ok())
            .map(|m| m.into_iter().filter(|(_, name)| !name.is_empty()).collect())
            .unwrap_or_default();

        let attached: BTreeMap<String, String> = field("attached")
            .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()).ok())
            .map(|m| m.into_iter().filter(|(_, target)| !target.is_empty()).collect())
            .unwrap_or_default();

        let hidden: BTreeSet<String> = field("hidden")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();

        let groups: BTreeMap<String, WallGroup> = field("groups")
            .and_then(|v| serde_json::from_value::<BTreeMap<String, WallGroup>>(v.clone()).ok())
            .unwrap_or_default();

        let sidebar = parse_sidebar(field("sidebar"));
        let status = parse_status_window(field("status"));

        let config: BTreeMap<String, serde_json::Value> = field("config")
            .and_then(|v| {
                serde_json::from_value::<BTreeMap<String, serde_json::Value>>(v.clone()).ok()
            })
            .unwrap_or_default();

        let settings = parse_settings_panel(field("settings"));
        let help = parse_help_panel(field("help"));

        let fullscreen: Option<FullscreenState> = field("fullscreen")
            .and_then(|v| serde_json::from_value::<Option<FullscreenState>>(v.clone()).ok())
            .flatten();

        let fullscreen_text: BTreeMap<String, f64> = field("fullscreenText")
            .and_then(|v| serde_json::from_value::<BTreeMap<String, f64>>(v.clone()).ok())
            .unwrap_or_default();

        let agent_views: BTreeMap<String, AgentViewState> = field("agentViews")
            .and_then(|v| {
                serde_json::from_value::<BTreeMap<String, AgentViewState>>(v.clone()).ok()
            })
            .unwrap_or_default();

        WallStore {
            scope,
            keys,
            file_path: None,
            storage,
            positions,
            names,
            attached,
            hidden,
            groups,
            sidebar,
            status,
            config,
            settings,
            help,
            fullscreen,
            fullscreen_text,
            agent_views,
        }
    }

    /// The scope name passed at construction.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Scoped key names (`glyph3d.wall.<scope>.<what>`).
    pub fn keys(&self) -> &StoreKeys {
        &self.keys
    }

    /// Underlying JSON file path when opened with [`WallStore::open`].
    pub fn path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    fn persist(&self) {
        let positions: BTreeMap<String, StoredPoint> = self
            .positions
            .iter()
            .map(|(k, p)| (k.clone(), StoredPoint::from(*p)))
            .collect();
        let hidden: Vec<String> = self.hidden.iter().cloned().collect();
        let doc = StoreDocumentRef {
            positions: &positions,
            names: &self.names,
            attached: &self.attached,
            hidden: &hidden,
            groups: &self.groups,
            sidebar: &self.sidebar,
            status: &self.status,
            config: &self.config,
            settings: &self.settings,
            help: &self.help,
            fullscreen: &self.fullscreen,
            fullscreen_text: &self.fullscreen_text,
            agent_views: &self.agent_views,
        };
        let mut bytes =
            serde_json::to_vec_pretty(&doc).expect("WallStore document always serializes");
        bytes.push(b'\n');
        // Matching WallStore.js: when storage is blocked or read-only, in-memory state keeps working.
        let _ = self.storage.save(&self.scope, &bytes);
    }

    // ── terminals ────────────────────────────────────────────────────────────

    /// Remembered canvas position of terminal `id`, if any.
    pub fn position(&self, id: &str) -> Option<Point> {
        self.positions.get(id).copied()
    }

    /// Record canvas position `p` for terminal `id`.
    pub fn set_position(&mut self, id: &str, p: Point) {
        self.positions.insert(id.to_string(), p);
        self.persist();
    }

    /// Custom name given to terminal `id`, if any.
    pub fn name(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str).filter(|s| !s.is_empty())
    }

    /// Set custom `name` for terminal `id` (whitespace-only or empty clears it).
    pub fn set_name(&mut self, id: &str, name: &str) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            self.names.remove(id);
        } else {
            self.names.insert(id.to_string(), trimmed.to_string());
        }
        self.persist();
    }

    /// The gomuxai agent (`name` or `name@host`) terminal `id` was opened to attach to, if any.
    pub fn attached(&self, id: &str) -> Option<&str> {
        self.attached
            .get(id)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Record or clear (`None` / empty) the gomuxai agent target attached by terminal `id`.
    pub fn set_attached(&mut self, id: &str, target: Option<&str>) {
        match target.filter(|t| !t.is_empty()) {
            Some(t) => {
                self.attached.insert(id.to_string(), t.to_string());
            }
            None => {
                self.attached.remove(id);
            }
        }
        self.persist();
    }

    /// Every terminal id currently attached to `target`.
    pub fn attached_to(&self, target: &str) -> Vec<String> {
        self.attached
            .iter()
            .filter(|(_, t)| t.as_str() == target)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every `(terminal_id, target)` attachment pair.
    pub fn attached_entries(&self) -> Vec<(String, String)> {
        self.attached
            .iter()
            .map(|(id, target)| (id.clone(), target.clone()))
            .collect()
    }

    /// Whether terminal `id` is marked hidden.
    pub fn hidden(&self, id: &str) -> bool {
        self.hidden.contains(id)
    }

    /// Mark terminal `id` hidden or shown.
    pub fn set_hidden(&mut self, id: &str, on: bool) {
        if on == self.hidden.contains(id) {
            return;
        }
        if on {
            self.hidden.insert(id.to_string());
        } else {
            self.hidden.remove(id);
        }
        self.persist();
    }

    /// Forget terminal `id` completely: its position, name, attachment, hidden mark, group
    /// membership (collapsing split panes and removing emptied groups), fullscreen state/text,
    /// and Agent View entry.
    pub fn forget(&mut self, id: &str) {
        if self.forget_in_memory(id) {
            self.persist();
        }
    }

    fn forget_in_memory(&mut self, id: &str) -> bool {
        let mut changed = false;
        if self.positions.remove(id).is_some() {
            changed = true;
        }
        if self.names.remove(id).is_some() {
            changed = true;
        }
        if self.attached.remove(id).is_some() {
            changed = true;
        }
        if self.hidden.remove(id) {
            changed = true;
        }
        if let Some(gid) = self.group_of(id).map(str::to_string) {
            self.drop_members_in_memory(&gid, &[id]);
            changed = true;
        }
        if self.fullscreen_text.remove(id).is_some() {
            changed = true;
        }
        if let Some(ref fs) = self.fullscreen {
            if fs.id.as_deref() == Some(id) {
                self.fullscreen = None;
                changed = true;
            } else if fs.saved.as_ref().is_some_and(|s| s.contains_key(id)) {
                let mut next_fs = fs.clone();
                if let Some(ref mut saved) = next_fs.saved {
                    saved.remove(id);
                }
                if next_fs
                    .group_id
                    .as_ref()
                    .is_some_and(|gid| !self.groups.contains_key(gid))
                {
                    self.fullscreen = None;
                } else {
                    self.fullscreen = Some(next_fs);
                }
                changed = true;
            } else if fs
                .group_id
                .as_ref()
                .is_some_and(|gid| !self.groups.contains_key(gid))
            {
                self.fullscreen = None;
                changed = true;
            }
        }
        if self.agent_views.remove(id).is_some() {
            changed = true;
        }
        changed
    }

    /// Forget every known terminal not in `live` and delete any groups left with no members,
    /// returning the forgotten terminal ids.
    pub fn prune<I, S>(&mut self, live: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let live_set: BTreeSet<String> =
            live.into_iter().map(|s| s.as_ref().to_string()).collect();
        let mut known: BTreeSet<String> = BTreeSet::new();
        known.extend(self.positions.keys().cloned());
        known.extend(self.names.keys().cloned());
        known.extend(self.attached.keys().cloned());
        known.extend(self.hidden.iter().cloned());
        known.extend(self.fullscreen_text.keys().cloned());
        known.extend(self.agent_views.keys().cloned());
        if let Some(ref fs) = self.fullscreen {
            if let Some(ref id) = fs.id {
                if !id.is_empty() {
                    known.insert(id.clone());
                }
            }
            if let Some(ref saved) = fs.saved {
                known.extend(saved.keys().cloned());
            }
        }
        for group in self.groups.values() {
            known.extend(group.members.iter().cloned());
        }

        let gone: Vec<String> = known
            .into_iter()
            .filter(|id| !live_set.contains(id))
            .collect();
        let mut changed = false;
        for id in &gone {
            if self.forget_in_memory(id) {
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
        gone
    }

    // ── groups ───────────────────────────────────────────────────────────────

    /// Every group by id.
    pub fn groups(&self) -> &BTreeMap<String, WallGroup> {
        &self.groups
    }

    /// Look up one group by id.
    pub fn group(&self, gid: &str) -> Option<&WallGroup> {
        self.groups.get(gid)
    }

    /// The group id containing terminal `id`, if any.
    pub fn group_of(&self, id: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|(_, g)| g.members.iter().any(|m| m == id))
            .map(|(gid, _)| gid.as_str())
    }

    /// Mint the lowest unused group id (`g-1`, `g-2`, …).
    pub fn new_group_id(&self) -> String {
        let mut n: u64 = 1;
        loop {
            let candidate = format!("g-{n}");
            if !self.groups.contains_key(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Write group `gid` whole; its members leave any other group they belonged to, and any
    /// group emptied as a result is removed (and unfolded in the sidebar).
    pub fn set_group(&mut self, gid: &str, group: WallGroup) {
        let other_ids: Vec<String> = self
            .groups
            .keys()
            .filter(|other| other.as_str() != gid)
            .cloned()
            .collect();
        for other in other_ids {
            let taken: Vec<String> = self.groups[&other]
                .members
                .iter()
                .filter(|id| group.members.contains(id))
                .cloned()
                .collect();
            if !taken.is_empty() {
                let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
                self.drop_members_in_memory(&other, &refs);
            }
        }
        self.groups.insert(gid.to_string(), group);
        self.persist();
    }

    /// Delete group `gid` and remove it from `sidebar.collapsed`.
    pub fn delete_group(&mut self, gid: &str) {
        if self.groups.remove(gid).is_none() {
            return;
        }
        self.uncollapse_in_memory(gid);
        self.persist();
    }

    fn drop_members_in_memory(&mut self, gid: &str, ids: &[&str]) {
        let Some(group) = self.groups.get_mut(gid) else {
            return;
        };
        group.members.retain(|m| !ids.contains(&m.as_str()));
        if let Some(ref mut split) = group.split {
            for &id in ids {
                if let Some(next_tree) = remove_pane(&split.tree, id) {
                    split.tree = next_tree;
                }
            }
        }
        if group.members.is_empty() {
            self.groups.remove(gid);
            self.uncollapse_in_memory(gid);
        }
    }

    fn uncollapse_in_memory(&mut self, gid: &str) {
        self.sidebar.collapsed.retain(|c| c != gid);
    }

    // ── the sidebar ──────────────────────────────────────────────────────────

    pub fn sidebar(&self) -> &SidebarState {
        &self.sidebar
    }

    pub fn set_sidebar(&mut self, patch: impl Into<SidebarPatch>) {
        let p = patch.into();
        if let Some(open) = p.open {
            self.sidebar.open = open;
        }
        if let Some(side) = p.side {
            self.sidebar.side = side;
        }
        if let Some(float) = p.float {
            self.sidebar.float = float;
        }
        if let Some(collapsed) = p.collapsed {
            self.sidebar.collapsed = collapsed;
        }
        self.persist();
    }

    // ── the termd status window ──────────────────────────────────────────────

    pub fn status_window(&self) -> &StatusWindowState {
        &self.status
    }

    pub fn set_status_window(&mut self, patch: impl Into<StatusWindowPatch>) {
        let p = patch.into();
        if let Some(open) = p.open {
            self.status.open = open;
        }
        if let Some(side) = p.side {
            self.status.side = side;
        }
        if let Some(float) = p.float {
            self.status.float = float;
        }
        self.persist();
    }

    // ── the knobs and the settings panel ─────────────────────────────────────

    /// Dialled configuration overrides (`key -> raw JSON value`), never pruned.
    pub fn config(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.config
    }

    /// Remember a dialled knob override.
    pub fn set_config(&mut self, key: &str, value: impl Into<serde_json::Value>) {
        self.config.insert(key.to_string(), value.into());
        self.persist();
    }

    /// Forget a dialled knob override so it reverts to the wall's default.
    pub fn clear_config(&mut self, key: &str) {
        if self.config.remove(key).is_none() {
            return;
        }
        self.persist();
    }

    pub fn settings_panel(&self) -> &SettingsPanelState {
        &self.settings
    }

    pub fn set_settings_panel(&mut self, patch: impl Into<SettingsPanelPatch>) {
        let p = patch.into();
        if let Some(open) = p.open {
            self.settings.open = open;
        }
        if let Some(side) = p.side {
            self.settings.side = side;
        }
        if let Some(float) = p.float {
            self.settings.float = float;
        }
        if let Some(collapsed) = p.collapsed {
            self.settings.collapsed = collapsed;
        }
        self.persist();
    }

    // ── the help panel ───────────────────────────────────────────────────────

    pub fn help_panel(&self) -> &HelpPanelState {
        &self.help
    }

    pub fn set_help_panel(&mut self, patch: impl Into<HelpPanelPatch>) {
        let p = patch.into();
        if let Some(open) = p.open {
            self.help.open = open;
        }
        if let Some(side) = p.side {
            self.help.side = side;
        }
        if let Some(float) = p.float {
            self.help.float = float;
        }
        self.persist();
    }

    // ── fullscreen ───────────────────────────────────────────────────────────

    pub fn fullscreen(&self) -> Option<&FullscreenState> {
        self.fullscreen.as_ref()
    }

    pub fn set_fullscreen(&mut self, state: Option<FullscreenState>) {
        self.fullscreen = state;
        self.persist();
    }

    /// Remembered fullscreen text size in CSS px per row for `id`, if set.
    pub fn fullscreen_text(&self, id: &str) -> Option<f64> {
        self.fullscreen_text.get(id).copied()
    }

    /// Set or clear (`None`) a terminal's fullscreen text size.
    pub fn set_fullscreen_text(&mut self, id: &str, px: Option<f64>) {
        match px {
            None => {
                if self.fullscreen_text.remove(id).is_none() {
                    return;
                }
            }
            Some(v) => {
                self.fullscreen_text.insert(id.to_string(), v);
            }
        }
        self.persist();
    }

    // ── agent views ──────────────────────────────────────────────────────────

    pub fn agent_views(&self) -> &BTreeMap<String, AgentViewState> {
        &self.agent_views
    }

    pub fn agent_view(&self, id: &str) -> Option<&AgentViewState> {
        self.agent_views.get(id)
    }

    pub fn set_agent_view(&mut self, id: &str, state: Option<AgentViewState>) {
        match state {
            None => {
                if self.agent_views.remove(id).is_none() {
                    return;
                }
            }
            Some(mut entry) => {
                if entry.session.is_empty() {
                    entry.session = default_agent_session();
                }
                self.agent_views.insert(id.to_string(), entry);
            }
        }
        self.persist();
    }
}

fn parse_sidebar(raw: Option<&serde_json::Value>) -> SidebarState {
    let mut out = SidebarState::default();
    let Some(obj) = raw.and_then(|v| v.as_object()) else {
        return out;
    };
    if let Some(open) = obj.get("open").and_then(|v| v.as_bool()) {
        out.open = open;
    }
    if let Some(side) = obj
        .get("side")
        .and_then(|v| serde_json::from_value::<PanelSide>(v.clone()).ok())
    {
        out.side = side;
    }
    if let Some(float) = obj
        .get("float")
        .and_then(|v| serde_json::from_value::<StoredPoint>(v.clone()).ok())
    {
        out.float = Point::from(float);
    }
    if let Some(collapsed) = obj
        .get("collapsed")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
    {
        out.collapsed = collapsed;
    }
    out
}

fn parse_status_window(raw: Option<&serde_json::Value>) -> StatusWindowState {
    let mut out = StatusWindowState::default();
    let Some(obj) = raw.and_then(|v| v.as_object()) else {
        return out;
    };
    if let Some(open) = obj.get("open").and_then(|v| v.as_bool()) {
        out.open = open;
    }
    if let Some(side) = obj
        .get("side")
        .and_then(|v| serde_json::from_value::<PanelSide>(v.clone()).ok())
    {
        out.side = side;
    }
    if let Some(float_val) = obj.get("float") {
        if let Ok(opt) = serde_json::from_value::<Option<StoredPoint>>(float_val.clone()) {
            out.float = opt.map(Point::from);
        }
    }
    out
}

fn parse_settings_panel(raw: Option<&serde_json::Value>) -> SettingsPanelState {
    let mut out = SettingsPanelState::default();
    let Some(obj) = raw.and_then(|v| v.as_object()) else {
        return out;
    };
    if let Some(open) = obj.get("open").and_then(|v| v.as_bool()) {
        out.open = open;
    }
    if let Some(side) = obj
        .get("side")
        .and_then(|v| serde_json::from_value::<PanelSide>(v.clone()).ok())
    {
        out.side = side;
    }
    if let Some(float_val) = obj.get("float") {
        if let Ok(opt) = serde_json::from_value::<Option<StoredPoint>>(float_val.clone()) {
            out.float = opt.map(Point::from);
        }
    }
    if let Some(collapsed) = obj
        .get("collapsed")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
    {
        out.collapsed = collapsed;
    }
    out
}

fn parse_help_panel(raw: Option<&serde_json::Value>) -> HelpPanelState {
    let mut out = HelpPanelState::default();
    let Some(obj) = raw.and_then(|v| v.as_object()) else {
        return out;
    };
    if let Some(open) = obj.get("open").and_then(|v| v.as_bool()) {
        out.open = open;
    }
    if let Some(side) = obj
        .get("side")
        .and_then(|v| serde_json::from_value::<PanelSide>(v.clone()).ok())
    {
        out.side = side;
    }
    if let Some(float_val) = obj.get("float") {
        if let Ok(opt) = serde_json::from_value::<Option<StoredPoint>>(float_val.clone()) {
            out.float = opt.map(Point::from);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wall-core-store-{label}-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            ScratchDir(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_group(members: &[&str]) -> WallGroup {
        WallGroup {
            name: String::new(),
            layout: GroupLayout::Tile,
            columns: 2,
            gap: 2.0,
            origin: Some(Point::new(0.0, 0.0)),
            members: members.iter().map(|s| (*s).to_string()).collect(),
            split: None,
        }
    }

    #[test]
    fn each_scope_has_its_own_names_and_spots() {
        let dir = ScratchDir::new("scopes");
        let mut a = WallStore::open(&dir.0, "relay-8099");
        let b = WallStore::open(&dir.0, "relay-8093");
        a.set_name("term-1", "build");
        a.set_position("term-1", Point::new(3.0, -4.0));
        assert_eq!(b.name("term-1"), None);
        assert_eq!(b.position("term-1"), None);
        assert_eq!(
            WallStore::open(&dir.0, "relay-8099").name("term-1"),
            Some("build")
        );

        let disk_json: serde_json::Value =
            serde_json::from_slice(&fs::read(a.path().unwrap()).unwrap()).unwrap();
        assert_eq!(
            disk_json["positions"],
            serde_json::json!({ "term-1": { "x": 3.0, "y": -4.0 } })
        );
        assert_ne!(a.keys().names, b.keys().names);
        assert_ne!(a.path(), b.path());

        // Atomic write leaves no `.tmp` sibling behind.
        let entries: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(entries.iter().all(|name| !name.ends_with(".tmp")));
    }

    #[test]
    fn prune_forgets_every_terminal_no_longer_live_and_keeps_live_ones() {
        let dir = ScratchDir::new("prune");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_name("term-1", "build");
        s.set_position("term-1", Point::new(1.0, 1.0));
        s.set_name("term-2", "logs");
        s.set_position("term-3", Point::new(2.0, 2.0));
        let mut pruned = s.prune(["term-2"]);
        pruned.sort();
        assert_eq!(pruned, vec!["term-1", "term-3"]);
        assert_eq!(s.name("term-1"), None);
        assert_eq!(s.position("term-1"), None);
        assert_eq!(s.position("term-3"), None);
        assert_eq!(s.name("term-2"), Some("logs"));

        let again = WallStore::open(&dir.0, "8099");
        assert_eq!(again.name("term-1"), None);
        assert_eq!(again.name("term-2"), Some("logs"));
    }

    #[test]
    fn forget_drops_one_terminal_now() {
        let dir = ScratchDir::new("forget");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_name("term-5", "db");
        s.set_position("term-5", Point::new(0.0, 0.0));
        s.forget("term-5");
        assert_eq!(s.name("term-5"), None);
        assert_eq!(s.position("term-5"), None);
    }

    #[test]
    fn hidden_terminals_groups_and_sidebar_round_trip_per_scope() {
        let dir = ScratchDir::new("round-trip");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_hidden("term-2", true);
        let gid1 = s.new_group_id();
        s.set_group(
            &gid1,
            WallGroup {
                name: "build".to_string(),
                ..make_group(&["term-1", "term-2"])
            },
        );
        let gid2 = s.new_group_id();
        s.set_group(
            &gid2,
            WallGroup {
                layout: GroupLayout::Freeform,
                origin: None,
                ..make_group(&["term-3"])
            },
        );
        s.set_sidebar(SidebarPatch {
            open: Some(true),
            side: Some(PanelSide::Left),
            collapsed: Some(vec!["g-1".to_string()]),
            ..Default::default()
        });

        let back = WallStore::open(&dir.0, "8099");
        assert!(back.hidden("term-2"));
        assert!(!back.hidden("term-1"));
        let mut expected_groups = BTreeMap::new();
        expected_groups.insert(
            "g-1".to_string(),
            WallGroup {
                name: "build".to_string(),
                ..make_group(&["term-1", "term-2"])
            },
        );
        expected_groups.insert(
            "g-2".to_string(),
            WallGroup {
                layout: GroupLayout::Freeform,
                origin: None,
                ..make_group(&["term-3"])
            },
        );
        assert_eq!(back.groups(), &expected_groups);
        assert_eq!(back.group_of("term-3"), Some("g-2"));
        assert_eq!(
            back.sidebar(),
            &SidebarState {
                open: true,
                side: PanelSide::Left,
                float: Point::new(80.0, 80.0),
                collapsed: vec!["g-1".to_string()],
            }
        );
        assert!(WallStore::open(&dir.0, "8093").groups().is_empty());
        assert!(!WallStore::open(&dir.0, "8093").sidebar().open);
        assert_eq!(back.new_group_id(), "g-3");
    }

    #[test]
    fn status_window_keeps_its_own_place_beside_sidebar_per_scope() {
        let dir = ScratchDir::new("status");
        let mut s = WallStore::open(&dir.0, "8099");
        assert_eq!(
            s.status_window(),
            &StatusWindowState {
                open: false,
                side: PanelSide::Float,
                float: None,
            }
        );
        s.set_sidebar(SidebarPatch {
            open: Some(true),
            side: Some(PanelSide::Right),
            ..Default::default()
        });
        s.set_status_window(StatusWindowPatch {
            open: Some(true),
            side: Some(PanelSide::Float),
            float: Some(Some(Point::new(40.0, 60.0))),
        });
        s.set_status_window(StatusWindowPatch {
            side: Some(PanelSide::Right),
            ..Default::default()
        });

        let mut back = WallStore::open(&dir.0, "8099");
        assert_eq!(
            back.status_window(),
            &StatusWindowState {
                open: true,
                side: PanelSide::Right,
                float: Some(Point::new(40.0, 60.0)),
            }
        );
        assert_eq!(
            back.sidebar(),
            &SidebarState {
                open: true,
                side: PanelSide::Right,
                float: Point::new(80.0, 80.0),
                collapsed: Vec::new(),
            }
        );
        assert_ne!(back.keys().status, back.keys().sidebar);
        assert!(!WallStore::open(&dir.0, "8093").status_window().open);
        back.prune([] as [&str; 0]);
        assert!(WallStore::open(&dir.0, "8099").status_window().open);
    }

    #[test]
    fn terminal_is_in_one_group_at_most_and_emptied_group_goes() {
        let dir = ScratchDir::new("one-group");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_group("g-1", make_group(&["term-1", "term-2"]));
        s.set_group("g-2", make_group(&["term-3"]));
        s.set_group("g-2", make_group(&["term-3", "term-2"]));
        assert_eq!(s.group("g-1").unwrap().members, vec!["term-1"]);

        s.set_sidebar(SidebarPatch {
            collapsed: Some(vec!["g-1".to_string()]),
            ..Default::default()
        });
        s.set_group("g-2", make_group(&["term-3", "term-2", "term-1"]));
        assert_eq!(s.group("g-1"), None);
        assert!(s.sidebar().collapsed.is_empty());
        assert_eq!(WallStore::open(&dir.0, "8099").group("g-1"), None);
    }

    #[test]
    fn prune_drops_dead_members_from_groups_emptied_groups_and_dead_hidden_marks() {
        let dir = ScratchDir::new("prune-groups");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_group("g-1", make_group(&["term-1", "term-2"]));
        s.set_group("g-2", make_group(&["term-3"]));
        s.set_hidden("term-2", true);
        s.set_hidden("term-4", true);
        let mut pruned = s.prune(["term-1"]);
        pruned.sort();
        assert_eq!(pruned, vec!["term-2", "term-3", "term-4"]);
        assert_eq!(s.group("g-1").unwrap().members, vec!["term-1"]);
        assert_eq!(s.group("g-2"), None);
        assert!(!s.hidden("term-2"));
        assert!(!s.hidden("term-4"));

        let again = WallStore::open(&dir.0, "8099");
        let group_ids: Vec<&str> = again.groups().keys().map(String::as_str).collect();
        assert_eq!(group_ids, vec!["g-1"]);
        assert!(!again.hidden("term-4"));
    }

    #[test]
    fn forget_takes_terminal_out_of_its_group_and_unhides_it() {
        let dir = ScratchDir::new("forget-group");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_group("g-1", make_group(&["term-1"]));
        s.set_hidden("term-1", true);
        s.forget("term-1");
        assert_eq!(s.group("g-1"), None);
        assert!(!s.hidden("term-1"));
        assert_eq!(s.group_of("term-1"), None);
    }

    #[test]
    fn split_tree_fullscreen_attached_config_and_agent_views_round_trip_and_prune() {
        let dir = ScratchDir::new("extras");
        let mut s = WallStore::open(&dir.0, "rs-wall");

        let tree = SplitNode::Branch {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(SplitNode::Leaf { id: "t1".to_string() }),
            b: Box::new(SplitNode::Leaf { id: "t2".to_string() }),
        };
        s.set_group(
            "g-1",
            WallGroup {
                name: "pair".to_string(),
                layout: GroupLayout::Split,
                columns: 2,
                gap: 0.5,
                origin: Some(Point::new(10.0, 20.0)),
                members: vec!["t1".to_string(), "t2".to_string()],
                split: Some(WallSplit {
                    w: 100.0,
                    h: 50.0,
                    bars: true,
                    tree,
                }),
            },
        );
        s.set_attached("t1", Some("helper@workstation"));
        s.set_config("background", "#203040");
        s.set_settings_panel(SettingsPanelPatch {
            open: Some(true),
            side: Some(PanelSide::Right),
            collapsed: Some(vec!["groups".to_string()]),
            ..Default::default()
        });
        s.set_help_panel(HelpPanelPatch {
            open: Some(true),
            ..Default::default()
        });
        let mut saved = BTreeMap::new();
        saved.insert(
            "t1".to_string(),
            SavedCanvasPane { cols: 80, rows: 24, x: 0.0, y: 0.0 },
        );
        saved.insert(
            "t2".to_string(),
            SavedCanvasPane { cols: 80, rows: 24, x: 50.0, y: 0.0 },
        );
        s.set_fullscreen(Some(FullscreenState {
            mode: Some(FullscreenMode::Group),
            id: None,
            group_id: Some("g-1".to_string()),
            return_group: None,
            solo_override: None,
            canvas: None,
            saved: Some(saved),
            camera: CameraPose { x: 0.0, y: 0.0, zoom: 4.0 },
            line_px: 16.0,
            entry_line_px: 14.0,
            zoom_scale: Some(1.0),
        }));
        s.set_fullscreen_text("t2", Some(18.0));
        s.set_agent_view(
            "t1",
            Some(AgentViewState {
                open: true,
                side: AgentSide::Right,
                session: String::new(),
                filters: None,
            }),
        );

        let mut reloaded = WallStore::open(&dir.0, "rs-wall");
        assert_eq!(reloaded.attached("t1"), Some("helper@workstation"));
        assert_eq!(reloaded.attached_to("helper@workstation"), vec!["t1"]);
        assert_eq!(
            reloaded.agent_view("t1").unwrap().session,
            "auto",
            "empty session normalizes to 'auto'"
        );
        assert_eq!(
            reloaded.config().get("background"),
            Some(&serde_json::json!("#203040"))
        );
        assert!(reloaded.settings_panel().open);
        assert!(reloaded.help_panel().open);

        // Pruning t2 collapses the split tree to Leaf("t1"), removes t2 from fullscreen.saved,
        // and leaves dialled config and chrome untouched.
        assert_eq!(reloaded.prune(["t1"]), vec!["t2"]);
        assert_eq!(
            reloaded.group("g-1").unwrap().split.as_ref().unwrap().tree,
            SplitNode::Leaf { id: "t1".to_string() }
        );
        assert!(!reloaded
            .fullscreen()
            .unwrap()
            .saved
            .as_ref()
            .unwrap()
            .contains_key("t2"));
        assert_eq!(reloaded.fullscreen_text("t2"), None);
        assert_eq!(
            reloaded.config().get("background"),
            Some(&serde_json::json!("#203040"))
        );

        reloaded.clear_config("background");
        assert!(WallStore::open(&dir.0, "rs-wall").config().is_empty());
    }

    #[test]
    fn unavailable_storage_falls_back_in_memory_without_panicking() {
        struct BlockedStorage;
        impl Storage for BlockedStorage {
            fn load(&self, _scope: &str) -> io::Result<Option<Vec<u8>>> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "blocked"))
            }
            fn save(&self, _scope: &str, _bytes: &[u8]) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "blocked"))
            }
        }

        let mut s = WallStore::with_storage(Box::new(BlockedStorage), "blocked");
        s.set_name("term-1", "ephemeral");
        assert_eq!(s.name("term-1"), Some("ephemeral"));
    }
}
