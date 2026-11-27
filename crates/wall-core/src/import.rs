//! Importer from a browser-wall (`glyph3d-js/wall/WallStore.js`) export into a
//! native [`WallStore`].
//!
//! ## How the browser wall persists state
//! `WallStore.js` writes exclusively to browser `window.localStorage` under 13 keys
//! scoped by relay port (`glyph3d.wall.relay-<port>.<section>`):
//! `positions`, `names`, `hidden`, `groups`, `sidebar`, `status`, `config`,
//! `settings`, `help`, `fullscreen`, `fullscreenText`, `agentViews`, and `attached`.
//! Each `localStorage` entry holds a JSON-encoded string, and multiple `relay-<port>`
//! scopes may coexist in the same browser origin.
//!
//! Native [`WallStore`] stores a single JSON document `<store_dir>/wall-<ns>.json`
//! using the same world coordinate system (`WORLD_CELL_HEIGHT = 2.4`, `PANEL_PAD = 0.3`,
//! `y` up) and the same `g3d-termd` terminal identifiers (`t1`, `t2`, `gx-<agent>`).
//!
//! This module accepts either:
//! 1. A browser `localStorage` JSON dump (`{"glyph3d.wall.relay-8765.positions": "{...}", ...}`,
//!    with stringified or already-parsed JSON values), or
//! 2. A direct 13-key `WallStore` JSON object (`{"positions": {...}, "groups": {...}, ...}`),
//!    or an object wrapping either under `"localStorage"` or `"store"`.
//!
//! Every field that maps cleanly is written to the target [`WallStore`]; anything
//! malformed, browser-only, or unknown is skipped (or noted) in [`ImportReport::unmapped`]
//! so nothing is silently lost or half-applied.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::camera::CameraPose;
use crate::config::{check_config, AgentSide, GroupLayout, WallConfig};
use crate::geom::Point;
use crate::settings::{KnobInput, KnobValue, SECTIONS};
use crate::split::{self, SplitNode};
use crate::store::{
    AgentViewState, DiskStorage, FullscreenMode, FullscreenState, HelpPanelPatch, PanelSide,
    SavedCanvasPane, SettingsPanelPatch, SidebarPatch, StatusWindowPatch, Storage, WallGroup,
    WallSplit, WallStore,
};

const WALL_PREFIX: &str = "glyph3d.wall.";

const KNOWN_SECTIONS: &[&str] = &[
    "positions",
    "names",
    "attached",
    "hidden",
    "groups",
    "sidebar",
    "status",
    "config",
    "settings",
    "help",
    "fullscreen",
    "fullscreenText",
    "agentViews",
];

/// Where the imported sections came from inside the input JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportSourceKind {
    /// Browser `localStorage` entries (`glyph3d.wall.<scope>.<section>`).
    LocalStorage {
        scope: String,
        available_scopes: Vec<String>,
    },
    /// Direct 13-key `WallStore` JSON document.
    StoreDocument,
}

/// One key, field, or entry from the browser export that does not map (or does
/// not have an active native counterpart), together with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedItem {
    pub key: String,
    pub reason: String,
}

/// Counts of mapped items written into the native [`WallStore`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub positions: usize,
    pub names: usize,
    pub attached: usize,
    pub hidden: usize,
    pub groups: usize,
    pub split_groups: usize,
    pub config_knobs: usize,
    pub fullscreen: bool,
    pub fullscreen_text: usize,
    pub agent_views: usize,
    pub panels: usize,
}

/// Summary of a completed browser-wall import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub source: ImportSourceKind,
    pub target_scope: String,
    pub target_path: Option<PathBuf>,
    pub counts: ImportCounts,
    pub unmapped: Vec<UnmappedItem>,
    pub notes: Vec<String>,
}

impl ImportReport {
    /// Human-readable multi-line report suitable for `wallctl` and control replies.
    pub fn summary_text(&self) -> String {
        let src_desc = match &self.source {
            ImportSourceKind::LocalStorage { scope, .. } => format!("browser localStorage ({scope})"),
            ImportSourceKind::StoreDocument => "WallStore JSON document".to_string(),
        };
        let dst_desc = match &self.target_path {
            Some(p) => format!("namespace '{}' ({})", self.target_scope, p.display()),
            None => format!("namespace '{}'", self.target_scope),
        };
        let c = &self.counts;
        let mut out = format!(
            "OK: imported {src_desc} -> {dst_desc}\n  mapped: {} position(s), {} name(s), {} group(s) ({} split), {} hidden, {} attached, {} config knob(s), {} agent view(s), {} panel state(s), fullscreen={}, {} fullscreen text size(s)",
            c.positions,
            c.names,
            c.groups,
            c.split_groups,
            c.hidden,
            c.attached,
            c.config_knobs,
            c.agent_views,
            c.panels,
            if c.fullscreen { "on" } else { "off" },
            c.fullscreen_text,
        );
        if self.unmapped.is_empty() {
            out.push_str("\n  unmapped: none");
        } else {
            out.push_str(&format!("\n  unmapped ({}):", self.unmapped.len()));
            for item in &self.unmapped {
                out.push_str(&format!("\n    - {}: {}", item.key, item.reason));
            }
        }
        if !self.notes.is_empty() {
            out.push_str("\n  notes:");
            for note in &self.notes {
                out.push_str(&format!("\n    - {note}"));
            }
        }
        out
    }
}

/// Read `export_path`, map its browser-wall state into `<store_dir>/wall-<target_scope>.json`,
/// and return the [`ImportReport`].
pub fn import_browser_file(
    export_path: &Path,
    source_scope: Option<&str>,
    store_dir: &Path,
    target_scope: &str,
) -> Result<ImportReport, String> {
    let raw = fs::read_to_string(export_path)
        .map_err(|e| format!("could not read '{}': {e}", export_path.display()))?;
    let disk = DiskStorage::new(store_dir);
    let target_path = disk.path_for(target_scope);
    let (_store, mut report) =
        import_browser_json_into_storage(&raw, source_scope, Box::new(disk), target_scope)?;
    report.target_path = Some(target_path);
    Ok(report)
}

/// Parse `raw_json` (a browser `localStorage` dump or a 13-key `WallStore` object),
/// populate a fresh [`WallStore`] backed by `storage` under `target_scope`, persist
/// it immediately via [`WallStore::save_checked`], and return both store and report.
pub fn import_browser_json_into_storage(
    raw_json: &str,
    source_scope: Option<&str>,
    storage: Box<dyn Storage>,
    target_scope: impl Into<String>,
) -> Result<(WallStore, ImportReport), String> {
    let target_scope = target_scope.into();
    if target_scope.trim().is_empty() {
        return Err("target namespace cannot be empty".to_string());
    }
    let root: Value =
        serde_json::from_str(raw_json).map_err(|e| format!("invalid JSON in export: {e}"))?;
    let root_obj = root
        .as_object()
        .ok_or_else(|| "export JSON root must be an object".to_string())?;

    let mut unmapped: Vec<UnmappedItem> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let (source, sections) = extract_sections(root_obj, source_scope, &mut unmapped, &mut notes)?;

    let mut store = WallStore::with_storage(storage, target_scope.clone());
    store.reset_in_memory();

    let mut counts = ImportCounts::default();

    for (section, val) in &sections {
        if !KNOWN_SECTIONS.contains(&section.as_str()) {
            unmapped.push(UnmappedItem {
                key: section.clone(),
                reason: "unknown browser WallStore section".to_string(),
            });
        } else if val.is_null() && section != "fullscreen" {
            // Explicit null on a map/list section leaves its empty default.
        }
    }

    if let Some(val) = sections.get("positions") {
        counts.positions = import_positions(val, &mut store, &mut unmapped);
    }
    if let Some(val) = sections.get("names") {
        counts.names = import_string_map(val, "names", &mut unmapped, |id, name| {
            store.set_name(id, name);
        });
    }
    if let Some(val) = sections.get("attached") {
        counts.attached = import_string_map(val, "attached", &mut unmapped, |id, target| {
            store.set_attached(id, Some(target));
        });
    }
    if let Some(val) = sections.get("hidden") {
        counts.hidden = import_hidden(val, &mut store, &mut unmapped);
    }
    if let Some(val) = sections.get("groups") {
        let (g_count, s_count) = import_groups(val, &mut store, &mut unmapped);
        counts.groups = g_count;
        counts.split_groups = s_count;
    }
    if let Some(val) = sections.get("sidebar") {
        if import_sidebar(val, &mut store, &mut unmapped) {
            counts.panels += 1;
        }
    }
    if let Some(val) = sections.get("status") {
        if import_status(val, &mut store, &mut unmapped) {
            counts.panels += 1;
        }
    }
    if let Some(val) = sections.get("settings") {
        if import_settings_panel(val, &mut store, &mut unmapped) {
            counts.panels += 1;
        }
    }
    if let Some(val) = sections.get("help") {
        if import_help_panel(val, &mut store, &mut unmapped) {
            counts.panels += 1;
        }
    }
    if let Some(val) = sections.get("config") {
        counts.config_knobs = import_config(val, &mut store, &mut unmapped);
    }
    if let Some(val) = sections.get("fullscreen") {
        counts.fullscreen = import_fullscreen(val, &mut store, &mut unmapped);
    }
    if let Some(val) = sections.get("fullscreenText") {
        counts.fullscreen_text = import_fullscreen_text(val, &mut store, &mut unmapped);
    }
    if let Some(val) = sections.get("agentViews") {
        counts.agent_views = import_agent_views(val, &mut store, &mut unmapped);
    }

    notes.push(
        "live terminal cols x rows and scrollback live in g3d-termd (adopted from the daemon on connect; split pane trees and pre-fullscreen saved sizes are carried in groups/fullscreen)"
            .to_string(),
    );

    store
        .save_checked()
        .map_err(|e| format!("could not write native WallStore for '{target_scope}': {e}"))?;

    let report = ImportReport {
        source,
        target_scope,
        target_path: None,
        counts,
        unmapped,
        notes,
    };
    Ok((store, report))
}

fn extract_sections(
    root_obj: &Map<String, Value>,
    requested_scope: Option<&str>,
    unmapped: &mut Vec<UnmappedItem>,
    notes: &mut Vec<String>,
) -> Result<(ImportSourceKind, BTreeMap<String, Value>), String> {
    // Allow optional wrapper `{ "localStorage": { ... } }` or `{ "store": { ... } }`.
    let effective_obj = if let Some(ls) = root_obj.get("localStorage").and_then(Value::as_object) {
        ls
    } else if root_obj.len() <= 2
        && root_obj.contains_key("store")
        && root_obj.get("store").is_some_and(Value::is_object)
    {
        root_obj["store"].as_object().expect("checked is_object")
    } else {
        root_obj
    };

    let has_ls_keys = effective_obj.keys().any(|k| k.starts_with(WALL_PREFIX));
    if has_ls_keys {
        return extract_local_storage_sections(effective_obj, requested_scope, unmapped, notes);
    }

    if let Some(scope) = requested_scope.filter(|s| !s.is_empty()) {
        notes.push(format!(
            "export is already an unscoped WallStore JSON object; source scope '{scope}' was not needed"
        ));
    }

    let any_known = effective_obj
        .keys()
        .any(|k| KNOWN_SECTIONS.contains(&k.as_str()));
    if !any_known && !effective_obj.is_empty() {
        return Err(
            "export JSON contains neither 'glyph3d.wall.<scope>.*' localStorage keys nor WallStore sections (positions, groups, names, config, ...)"
                .to_string(),
        );
    }

    let sections: BTreeMap<String, Value> = effective_obj
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok((ImportSourceKind::StoreDocument, sections))
}

fn extract_local_storage_sections(
    obj: &Map<String, Value>,
    requested_scope: Option<&str>,
    unmapped: &mut Vec<UnmappedItem>,
    notes: &mut Vec<String>,
) -> Result<(ImportSourceKind, BTreeMap<String, Value>), String> {
    let mut by_scope: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut non_wall_other = 0usize;

    for (full_key, raw_val) in obj {
        let Some(rest) = full_key.strip_prefix(WALL_PREFIX) else {
            if full_key.starts_with("glyph3d.") {
                unmapped.push(UnmappedItem {
                    key: full_key.clone(),
                    reason: "browser WebSocket/IDE localStorage key is not part of the native wall (native wall connects directly to g3d-termd's Unix socket)".to_string(),
                });
            } else {
                non_wall_other += 1;
            }
            continue;
        };
        let Some((scope, section)) = rest.rsplit_once('.') else {
            unmapped.push(UnmappedItem {
                key: full_key.clone(),
                reason: "malformed glyph3d.wall.* key (expected glyph3d.wall.<scope>.<section>)"
                    .to_string(),
            });
            continue;
        };
        if scope.is_empty() || section.is_empty() {
            unmapped.push(UnmappedItem {
                key: full_key.clone(),
                reason: "empty scope or section in glyph3d.wall.* key".to_string(),
            });
            continue;
        }
        let decoded = match raw_val {
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(parsed) => parsed,
                Err(e) => {
                    unmapped.push(UnmappedItem {
                        key: full_key.clone(),
                        reason: format!("corrupt JSON string in localStorage value: {e}"),
                    });
                    continue;
                }
            },
            other => other.clone(),
        };
        by_scope
            .entry(scope.to_string())
            .or_default()
            .insert(section.to_string(), decoded);
    }

    if non_wall_other > 0 {
        notes.push(format!(
            "ignored {non_wall_other} unrelated non-glyph3d localStorage key(s) from the browser origin"
        ));
    }

    if by_scope.is_empty() {
        return Err("no valid 'glyph3d.wall.<scope>.<section>' entries found in export".to_string());
    }

    let available_scopes: Vec<String> = by_scope.keys().cloned().collect();
    let chosen_scope = if let Some(req) = requested_scope.map(str::trim).filter(|s| !s.is_empty()) {
        if by_scope.contains_key(req) {
            req.to_string()
        } else {
            let prefixed = format!("relay-{req}");
            if by_scope.contains_key(&prefixed) {
                prefixed
            } else {
                return Err(format!(
                    "browser scope '{req}' not found in export (available scopes: {})",
                    available_scopes.join(", ")
                ));
            }
        }
    } else if available_scopes.len() == 1 {
        available_scopes[0].clone()
    } else {
        let non_empty: Vec<(&String, usize)> = by_scope
            .iter()
            .map(|(sc, secs)| (sc, scope_weight(secs)))
            .filter(|(_, w)| *w > 0)
            .collect();
        match non_empty.as_slice() {
            [(only, _)] => {
                let skipped: Vec<&str> = available_scopes
                    .iter()
                    .map(String::as_str)
                    .filter(|s| *s != only.as_str())
                    .collect();
                notes.push(format!(
                    "selected only non-empty browser scope '{only}' (skipped empty scope(s): {})",
                    skipped.join(", ")
                ));
                (*only).clone()
            }
            [] => {
                let first = available_scopes[0].clone();
                notes.push(format!(
                    "multiple empty browser scopes found ({}); selected '{first}'",
                    available_scopes.join(", ")
                ));
                first
            }
            many => {
                let summary: Vec<String> = many
                    .iter()
                    .map(|(sc, w)| format!("{sc} ({w} item(s))"))
                    .collect();
                return Err(format!(
                    "export holds multiple non-empty browser scopes: {}; pass the scope to import (e.g. wall.import <file> {})",
                    summary.join(", "),
                    many[0].0
                ));
            }
        }
    };

    let sections = by_scope.remove(&chosen_scope).unwrap_or_default();
    Ok((
        ImportSourceKind::LocalStorage {
            scope: chosen_scope,
            available_scopes,
        },
        sections,
    ))
}

fn scope_weight(sections: &BTreeMap<String, Value>) -> usize {
    let map_len = |k: &str| sections.get(k).and_then(Value::as_object).map_or(0, Map::len);
    let arr_len = |k: &str| sections.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    map_len("positions")
        + map_len("groups")
        + map_len("names")
        + map_len("attached")
        + arr_len("hidden")
        + map_len("config")
        + map_len("agentViews")
        + map_len("fullscreenText")
        + usize::from(sections.get("fullscreen").is_some_and(|v| !v.is_null()))
}

fn parse_point(val: &Value) -> Option<Point> {
    let obj = val.as_object()?;
    let x = obj.get("x")?.as_f64()?;
    let y = obj.get("y")?.as_f64()?;
    if x.is_finite() && y.is_finite() {
        Some(Point::new(x, y))
    } else {
        None
    }
}

fn import_positions(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> usize {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "positions".to_string(),
            reason: "expected JSON object mapping terminal id -> {x, y}".to_string(),
        });
        return 0;
    };
    let mut count = 0;
    for (id, pt_val) in obj {
        if id.trim().is_empty() {
            continue;
        }
        match parse_point(pt_val) {
            Some(pt) => {
                store.set_position(id, pt);
                count += 1;
            }
            None => unmapped.push(UnmappedItem {
                key: format!("positions.{id}"),
                reason: "expected finite {x, y} coordinates".to_string(),
            }),
        }
    }
    count
}

fn import_string_map(
    val: &Value,
    section: &str,
    unmapped: &mut Vec<UnmappedItem>,
    mut apply: impl FnMut(&str, &str),
) -> usize {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: section.to_string(),
            reason: "expected JSON object mapping terminal id -> string".to_string(),
        });
        return 0;
    };
    let mut count = 0;
    for (id, v) in obj {
        if id.trim().is_empty() {
            continue;
        }
        match v.as_str() {
            Some(s) if !s.trim().is_empty() => {
                apply(id, s.trim());
                count += 1;
            }
            Some(_) => {}
            None => unmapped.push(UnmappedItem {
                key: format!("{section}.{id}"),
                reason: "expected string value".to_string(),
            }),
        }
    }
    count
}

fn import_hidden(val: &Value, store: &mut WallStore, unmapped: &mut Vec<UnmappedItem>) -> usize {
    let Some(arr) = val.as_array() else {
        unmapped.push(UnmappedItem {
            key: "hidden".to_string(),
            reason: "expected JSON array of terminal ids".to_string(),
        });
        return 0;
    };
    let mut seen = BTreeSet::new();
    for (idx, item) in arr.iter().enumerate() {
        match item.as_str() {
            Some(id) if !id.trim().is_empty() => {
                if seen.insert(id.to_string()) {
                    store.set_hidden(id, true);
                }
            }
            Some(_) => {}
            None => unmapped.push(UnmappedItem {
                key: format!("hidden[{idx}]"),
                reason: "expected terminal id string".to_string(),
            }),
        }
    }
    seen.len()
}

fn import_groups(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> (usize, usize) {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "groups".to_string(),
            reason: "expected JSON object mapping group id -> group".to_string(),
        });
        return (0, 0);
    };
    let defaults = WallConfig::default();
    let mut group_count = 0;
    let mut split_count = 0;

    for (gid, g_val) in obj {
        let Some(g_obj) = g_val.as_object() else {
            unmapped.push(UnmappedItem {
                key: format!("groups.{gid}"),
                reason: "expected group object".to_string(),
            });
            continue;
        };
        let Some(raw_members) = g_obj.get("members").and_then(Value::as_array) else {
            unmapped.push(UnmappedItem {
                key: format!("groups.{gid}.members"),
                reason: "missing members array".to_string(),
            });
            continue;
        };
        let mut members: Vec<String> = Vec::new();
        for m in raw_members {
            if let Some(id) = m.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                if !members.iter().any(|existing| existing == id) {
                    members.push(id.to_string());
                }
            }
        }
        if members.is_empty() {
            unmapped.push(UnmappedItem {
                key: format!("groups.{gid}"),
                reason: "group has no members".to_string(),
            });
            continue;
        }

        let name = g_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        let mut layout = match g_obj.get("layout").and_then(Value::as_str) {
            Some(raw) => match GroupLayout::parse(raw) {
                Some(l) => l,
                None => {
                    unmapped.push(UnmappedItem {
                        key: format!("groups.{gid}.layout"),
                        reason: format!("unknown layout '{raw}'; fell back to 'tile'"),
                    });
                    GroupLayout::Tile
                }
            },
            None => GroupLayout::Tile,
        };

        let columns = match g_obj.get("columns").and_then(Value::as_f64) {
            Some(n) if n.is_finite() && n >= 0.0 => n.round() as usize,
            _ => members.len().isqrt().max(1),
        };

        let gap = g_obj
            .get("gap")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(defaults.group_gap);

        let origin = match g_obj.get("origin") {
            None | Some(Value::Null) => None,
            Some(o_val) => match parse_point(o_val) {
                Some(p) => Some(p),
                None => {
                    unmapped.push(UnmappedItem {
                        key: format!("groups.{gid}.origin"),
                        reason: "invalid origin point; cleared".to_string(),
                    });
                    None
                }
            },
        };

        let mut split = None;
        if layout == GroupLayout::Split {
            match g_obj.get("split") {
                Some(s_val) if !s_val.is_null() => {
                    match parse_wall_split(s_val, &members) {
                        Ok(ws) => {
                            split = Some(ws);
                            split_count += 1;
                        }
                        Err(reason) => {
                            unmapped.push(UnmappedItem {
                                key: format!("groups.{gid}.split"),
                                reason: format!("{reason}; downgraded group layout to 'tile'"),
                            });
                            layout = GroupLayout::Tile;
                        }
                    }
                }
                _ => {
                    unmapped.push(UnmappedItem {
                        key: format!("groups.{gid}.split"),
                        reason: "split group had no split state; downgraded layout to 'tile'"
                            .to_string(),
                    });
                    layout = GroupLayout::Tile;
                }
            }
        }

        store.set_group(
            gid,
            WallGroup {
                name,
                layout,
                columns: if columns == 0 {
                    (members.len() as f64).sqrt().ceil() as usize
                } else {
                    columns
                },
                gap,
                origin,
                members,
                split,
            },
        );
        group_count += 1;
    }
    (group_count, split_count)
}

fn parse_wall_split(val: &Value, members: &[String]) -> Result<WallSplit, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| "expected split object {w, h, bars, tree}".to_string())?;
    let w = obj
        .get("w")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| "split.w must be a positive number".to_string())?;
    let h = obj
        .get("h")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| "split.h must be a positive number".to_string())?;
    let bars = obj.get("bars").and_then(Value::as_bool).unwrap_or(true);
    let tree_val = obj
        .get("tree")
        .ok_or_else(|| "missing split.tree".to_string())?;
    let tree: SplitNode = serde_json::from_value(tree_val.clone())
        .map_err(|e| format!("invalid split.tree: {e}"))?;
    split::check_tree(&tree, members).map_err(|e| e.to_string())?;
    Ok(WallSplit { w, h, bars, tree })
}

fn parse_panel_side(val: Option<&Value>, path: &str, unmapped: &mut Vec<UnmappedItem>) -> Option<PanelSide> {
    let v = val?;
    match serde_json::from_value::<PanelSide>(v.clone()) {
        Ok(side) => Some(side),
        Err(_) => {
            unmapped.push(UnmappedItem {
                key: path.to_string(),
                reason: format!("expected 'left', 'right', or 'float', got {v}"),
            });
            None
        }
    }
}

fn parse_opt_float_point(
    val: Option<&Value>,
    path: &str,
    unmapped: &mut Vec<UnmappedItem>,
) -> Option<Option<Point>> {
    let v = val?;
    if v.is_null() {
        return Some(None);
    }
    match parse_point(v) {
        Some(p) => Some(Some(p)),
        None => {
            unmapped.push(UnmappedItem {
                key: path.to_string(),
                reason: "expected {x, y} or null".to_string(),
            });
            None
        }
    }
}

fn import_sidebar(val: &Value, store: &mut WallStore, unmapped: &mut Vec<UnmappedItem>) -> bool {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "sidebar".to_string(),
            reason: "expected sidebar state object".to_string(),
        });
        return false;
    };
    let open = obj.get("open").and_then(Value::as_bool);
    let side = parse_panel_side(obj.get("side"), "sidebar.side", unmapped);
    let float = match obj.get("float") {
        None | Some(Value::Null) => None,
        Some(v) => match parse_point(v) {
            Some(p) => Some(p),
            None => {
                unmapped.push(UnmappedItem {
                    key: "sidebar.float".to_string(),
                    reason: "expected finite {x, y}".to_string(),
                });
                None
            }
        },
    };
    let collapsed = obj.get("collapsed").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
    });
    // The JS wall's sidebar had one layout, so an export names no mode.
    store.set_sidebar(SidebarPatch {
        open,
        side,
        float,
        collapsed,
        mode: None,
    });
    true
}

fn import_status(val: &Value, store: &mut WallStore, unmapped: &mut Vec<UnmappedItem>) -> bool {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "status".to_string(),
            reason: "expected status window state object".to_string(),
        });
        return false;
    };
    let open = obj.get("open").and_then(Value::as_bool);
    let side = parse_panel_side(obj.get("side"), "status.side", unmapped);
    let float = parse_opt_float_point(obj.get("float"), "status.float", unmapped);
    store.set_status_window(StatusWindowPatch { open, side, float });
    true
}

fn import_settings_panel(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> bool {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "settings".to_string(),
            reason: "expected settings panel state object".to_string(),
        });
        return false;
    };
    let open = obj.get("open").and_then(Value::as_bool);
    let side = parse_panel_side(obj.get("side"), "settings.side", unmapped);
    let float = parse_opt_float_point(obj.get("float"), "settings.float", unmapped);
    let collapsed = obj.get("collapsed").and_then(|v| {
        v.as_array().map(|arr| {
            let mut valid = Vec::new();
            for item in arr {
                if let Some(sec) = item.as_str() {
                    if SECTIONS.iter().any(|s| s.id == sec) {
                        valid.push(sec.to_string());
                    } else {
                        unmapped.push(UnmappedItem {
                            key: format!("settings.collapsed.{sec}"),
                            reason: format!("unknown settings section '{sec}'"),
                        });
                    }
                }
            }
            valid
        })
    });
    store.set_settings_panel(SettingsPanelPatch {
        open,
        side,
        float,
        collapsed,
    });
    true
}

fn import_help_panel(val: &Value, store: &mut WallStore, unmapped: &mut Vec<UnmappedItem>) -> bool {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "help".to_string(),
            reason: "expected help panel state object".to_string(),
        });
        return false;
    };
    let open = obj.get("open").and_then(Value::as_bool);
    let side = parse_panel_side(obj.get("side"), "help.side", unmapped);
    let float = parse_opt_float_point(obj.get("float"), "help.float", unmapped);
    store.set_help_panel(HelpPanelPatch { open, side, float });
    true
}

fn knob_value_to_json(val: KnobValue) -> Value {
    match val {
        KnobValue::Bool(b) => Value::Bool(b),
        KnobValue::Number(n) => serde_json::Number::from_f64(n)
            .map_or(Value::Null, Value::Number),
        KnobValue::Text(s) => Value::String(s),
    }
}

fn import_config(val: &Value, store: &mut WallStore, unmapped: &mut Vec<UnmappedItem>) -> usize {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "config".to_string(),
            reason: "expected JSON object mapping knob -> value".to_string(),
        });
        return 0;
    };
    let mut count = 0;
    for (key, raw_val) in obj {
        match check_config(key, KnobInput::from(raw_val)) {
            Ok(checked) => {
                if key == "renderLoop" && checked == KnobValue::Text("always".to_string()) {
                    unmapped.push(UnmappedItem {
                        key: "config.renderLoop".to_string(),
                        reason: "native wall always renders on demand (winit ControlFlow::Wait); 'always' has no continuous-spin mode natively".to_string(),
                    });
                    continue;
                }
                if key == "traceInput" && checked == KnobValue::Bool(true) {
                    unmapped.push(UnmappedItem {
                        key: "config.traceInput".to_string(),
                        reason: "browser relay log-store input tracing has no Go relay in the native wall".to_string(),
                    });
                    continue;
                }
                store.set_config(key, knob_value_to_json(checked));
                count += 1;
            }
            Err(reason) => {
                unmapped.push(UnmappedItem {
                    key: format!("config.{key}"),
                    reason,
                });
            }
        }
    }
    count
}

fn parse_saved_pane(val: &Value) -> Option<SavedCanvasPane> {
    let obj = val.as_object()?;
    let cols = obj.get("cols")?.as_f64()?;
    let rows = obj.get("rows")?.as_f64()?;
    let x = obj.get("x")?.as_f64()?;
    let y = obj.get("y")?.as_f64()?;
    if cols.is_finite() && cols >= 1.0 && rows.is_finite() && rows >= 1.0 && x.is_finite() && y.is_finite() {
        Some(SavedCanvasPane {
            cols: cols.round() as u32,
            rows: rows.round() as u32,
            x,
            y,
        })
    } else {
        None
    }
}

fn import_fullscreen(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> bool {
    if val.is_null() {
        store.set_fullscreen(None);
        return false;
    }
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "fullscreen".to_string(),
            reason: "expected fullscreen state object or null".to_string(),
        });
        return false;
    };
    let Some(cam_obj) = obj.get("camera").and_then(Value::as_object) else {
        unmapped.push(UnmappedItem {
            key: "fullscreen.camera".to_string(),
            reason: "missing camera {x, y, zoom}".to_string(),
        });
        return false;
    };
    let (Some(cx), Some(cy), Some(czoom)) = (
        cam_obj.get("x").and_then(Value::as_f64).filter(|n| n.is_finite()),
        cam_obj.get("y").and_then(Value::as_f64).filter(|n| n.is_finite()),
        cam_obj.get("zoom").and_then(Value::as_f64).filter(|n| n.is_finite() && *n > 0.0),
    ) else {
        unmapped.push(UnmappedItem {
            key: "fullscreen.camera".to_string(),
            reason: "invalid camera pose in fullscreen state".to_string(),
        });
        return false;
    };
    let Some(line_px) = obj
        .get("linePx")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
    else {
        unmapped.push(UnmappedItem {
            key: "fullscreen.linePx".to_string(),
            reason: "missing or invalid linePx".to_string(),
        });
        return false;
    };
    let entry_line_px = obj
        .get("entryLinePx")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(line_px);

    let mode = match obj.get("mode").and_then(Value::as_str) {
        Some("single") => Some(FullscreenMode::Single),
        Some("group") => Some(FullscreenMode::Group),
        None => None,
        Some(other) => {
            unmapped.push(UnmappedItem {
                key: "fullscreen.mode".to_string(),
                reason: format!("unknown fullscreen mode '{other}'"),
            });
            return false;
        }
    };
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let group_id = obj
        .get("groupId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if id.is_none() && group_id.is_none() {
        unmapped.push(UnmappedItem {
            key: "fullscreen".to_string(),
            reason: "fullscreen state names neither terminal id nor groupId".to_string(),
        });
        return false;
    }

    let return_group = obj
        .get("returnGroup")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let solo_override = obj.get("soloOverride").and_then(Value::as_bool);
    let canvas = obj.get("canvas").and_then(parse_saved_pane);
    let saved = obj.get("saved").and_then(Value::as_object).map(|m| {
        m.iter()
            .filter_map(|(tid, sv)| parse_saved_pane(sv).map(|p| (tid.clone(), p)))
            .collect::<BTreeMap<_, _>>()
    });
    let zoom_scale = obj
        .get("zoomScale")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0);

    store.set_fullscreen(Some(FullscreenState {
        mode,
        id,
        group_id,
        return_group,
        solo_override,
        canvas,
        saved,
        camera: CameraPose {
            x: cx,
            y: cy,
            zoom: czoom,
        },
        line_px,
        entry_line_px,
        zoom_scale,
    }));
    true
}

fn import_fullscreen_text(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> usize {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "fullscreenText".to_string(),
            reason: "expected JSON object mapping terminal id -> linePx".to_string(),
        });
        return 0;
    };
    let mut count = 0;
    for (id, px_val) in obj {
        if id.trim().is_empty() {
            continue;
        }
        match px_val.as_f64() {
            Some(px) if px.is_finite() && px > 0.0 => {
                store.set_fullscreen_text(id, Some(px));
                count += 1;
            }
            _ => unmapped.push(UnmappedItem {
                key: format!("fullscreenText.{id}"),
                reason: "expected positive number".to_string(),
            }),
        }
    }
    count
}

fn import_agent_views(
    val: &Value,
    store: &mut WallStore,
    unmapped: &mut Vec<UnmappedItem>,
) -> usize {
    let Some(obj) = val.as_object() else {
        unmapped.push(UnmappedItem {
            key: "agentViews".to_string(),
            reason: "expected JSON object mapping terminal id -> agent view state".to_string(),
        });
        return 0;
    };
    let mut count = 0;
    for (id, av_val) in obj {
        if id.trim().is_empty() {
            continue;
        }
        let Some(av_obj) = av_val.as_object() else {
            unmapped.push(UnmappedItem {
                key: format!("agentViews.{id}"),
                reason: "expected agent view object".to_string(),
            });
            continue;
        };
        let open = av_obj.get("open").and_then(Value::as_bool).unwrap_or(false);
        let side = match av_obj.get("side").and_then(Value::as_str) {
            Some(raw) => match AgentSide::parse(raw) {
                Some(s) => s,
                None => {
                    unmapped.push(UnmappedItem {
                        key: format!("agentViews.{id}.side"),
                        reason: format!("unknown side '{raw}'; defaulted to 'right'"),
                    });
                    AgentSide::Right
                }
            },
            None => AgentSide::Right,
        };
        let session = av_obj
            .get("session")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
            .to_string();
        let filters = av_obj.get("filters").and_then(Value::as_object).map(|f_obj| {
            f_obj
                .iter()
                .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
                .collect::<BTreeMap<_, _>>()
        });
        store.set_agent_view(
            id,
            Some(AgentViewState {
                open,
                side,
                session,
                filters,
            }),
        );
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MemoryStorage;
    use serde_json::json;

    #[test]
    fn imports_browser_local_storage_export_and_lists_unmapped_items() {
        let browser_dump = json!({
            "glyph3d.wall.relay-8765.positions": serde_json::to_string(&json!({
                "t1": { "x": -48.0, "y": 24.0 },
                "t2": { "x": 96.0, "y": 24.0 },
                "t3": { "x": 96.0, "y": -60.0 },
                "gx-planner": { "x": 260.0, "y": 24.0 }
            })).unwrap(),
            "glyph3d.wall.relay-8765.names": serde_json::to_string(&json!({
                "t1": "server",
                "t2": "tests"
            })).unwrap(),
            "glyph3d.wall.relay-8765.attached": serde_json::to_string(&json!({
                "gx-planner": "planner@workstation"
            })).unwrap(),
            "glyph3d.wall.relay-8765.hidden": serde_json::to_string(&json!(["t3"])).unwrap(),
            "glyph3d.wall.relay-8765.groups": serde_json::to_string(&json!({
                "g-1": {
                    "name": "work",
                    "layout": "split",
                    "columns": 2,
                    "gap": 2,
                    "origin": { "x": -48.0, "y": 24.0 },
                    "members": ["t1", "t2"],
                    "split": {
                        "w": 244.0,
                        "h": 76.0,
                        "bars": true,
                        "tree": {
                            "dir": "row",
                            "ratio": 0.4,
                            "a": { "id": "t1" },
                            "b": { "id": "t2" }
                        }
                    }
                }
            })).unwrap(),
            "glyph3d.wall.relay-8765.sidebar": serde_json::to_string(&json!({
                "open": true,
                "side": "left",
                "float": { "x": 80, "y": 80 },
                "collapsed": ["g-1"]
            })).unwrap(),
            "glyph3d.wall.relay-8765.config": serde_json::to_string(&json!({
                "fontFamily": "monaspace-argon",
                "snap": false,
                "gridMinor": 8,
                "renderLoop": "always",
                "traceInput": true,
                "browserOnlyKnob": 42
            })).unwrap(),
            "glyph3d.wall.relay-8765.fullscreenText": serde_json::to_string(&json!({
                "t1": 18.5
            })).unwrap(),
            "glyph3d.wall.relay-8765.agentViews": serde_json::to_string(&json!({
                "gx-planner": {
                    "open": true,
                    "side": "right",
                    "session": "auto",
                    "filters": { "read": false }
                }
            })).unwrap(),
            "glyph3d.relay.8765.token": "secret-token",
            "vite:hmr": "1"
        });

        let mem = MemoryStorage::default();
        let (store, report) = import_browser_json_into_storage(
            &browser_dump.to_string(),
            None,
            Box::new(mem.clone()),
            "wall",
        )
        .expect("import succeeds");

        assert_eq!(
            report.source,
            ImportSourceKind::LocalStorage {
                scope: "relay-8765".to_string(),
                available_scopes: vec!["relay-8765".to_string()],
            }
        );
        assert_eq!(report.counts.positions, 4);
        assert_eq!(report.counts.names, 2);
        assert_eq!(report.counts.attached, 1);
        assert_eq!(report.counts.hidden, 1);
        assert_eq!(report.counts.groups, 1);
        assert_eq!(report.counts.split_groups, 1);
        assert_eq!(report.counts.config_knobs, 3);
        assert_eq!(report.counts.fullscreen_text, 1);
        assert_eq!(report.counts.agent_views, 1);
        assert_eq!(report.counts.panels, 1);

        assert_eq!(store.position("t1"), Some(Point::new(-48.0, 24.0)));
        assert_eq!(store.name("t1"), Some("server"));
        assert_eq!(store.attached("gx-planner"), Some("planner@workstation"));
        assert!(store.hidden("t3"));
        let g1 = store.group("g-1").expect("g-1 imported");
        assert_eq!(g1.layout, GroupLayout::Split);
        assert!(g1.split.is_some());
        assert_eq!(store.sidebar().side, PanelSide::Left);
        assert!(store.sidebar().open);
        assert_eq!(
            store.config().get("fontFamily"),
            Some(&json!("monaspace-argon"))
        );
        assert_eq!(store.config().get("snap"), Some(&json!(false)));
        assert!(!store.config().contains_key("renderLoop"));
        assert!(!store.config().contains_key("browserOnlyKnob"));
        assert_eq!(store.fullscreen_text("t1"), Some(18.5));
        assert!(store.agent_view("gx-planner").is_some_and(|av| av.open));

        let unmapped_keys: Vec<&str> = report.unmapped.iter().map(|u| u.key.as_str()).collect();
        assert!(unmapped_keys.contains(&"config.renderLoop"));
        assert!(unmapped_keys.contains(&"config.traceInput"));
        assert!(unmapped_keys.contains(&"config.browserOnlyKnob"));
        assert!(unmapped_keys.contains(&"glyph3d.relay.8765.token"));

        // Re-opening from the persisted bytes round-trips identically.
        let reloaded = WallStore::with_storage(Box::new(mem), "wall");
        assert_eq!(reloaded.position("t1"), Some(Point::new(-48.0, 24.0)));
        assert_eq!(reloaded.group("g-1"), store.group("g-1"));
    }

    #[test]
    fn multiple_browser_scopes_pick_non_empty_or_require_explicit_scope() {
        let one_active = json!({
            "glyph3d.wall.relay-8765.positions": "{\"t1\":{\"x\":10,\"y\":20}}",
            "glyph3d.wall.relay-9999.positions": "{}"
        });
        let (_, report) = import_browser_json_into_storage(
            &one_active.to_string(),
            None,
            Box::new(MemoryStorage::default()),
            "probe",
        )
        .expect("picks only non-empty scope");
        assert_eq!(
            report.source,
            ImportSourceKind::LocalStorage {
                scope: "relay-8765".to_string(),
                available_scopes: vec!["relay-8765".to_string(), "relay-9999".to_string()],
            }
        );

        let two_active = json!({
            "glyph3d.wall.relay-8765.positions": "{\"t1\":{\"x\":10,\"y\":20}}",
            "glyph3d.wall.relay-8766.positions": "{\"t2\":{\"x\":30,\"y\":40}}"
        });
        let err = import_browser_json_into_storage(
            &two_active.to_string(),
            None,
            Box::new(MemoryStorage::default()),
            "probe",
        )
        .err()
        .expect("ambiguous scopes must fail");
        assert!(err.contains("multiple non-empty browser scopes"), "{err}");

        // Passing either `relay-8766` or bare port `8766` resolves cleanly.
        let (store, _) = import_browser_json_into_storage(
            &two_active.to_string(),
            Some("8766"),
            Box::new(MemoryStorage::default()),
            "probe",
        )
        .expect("explicit port selects relay-8766");
        assert_eq!(store.position("t2"), Some(Point::new(30.0, 40.0)));
        assert_eq!(store.position("t1"), None);
    }

    #[test]
    fn malformed_split_tree_downgrades_to_tile_and_reports_reason() {
        let doc = json!({
            "groups": {
                "g-1": {
                    "name": "broken",
                    "layout": "split",
                    "columns": 2,
                    "gap": 2.0,
                    "origin": { "x": 0.0, "y": 0.0 },
                    "members": ["t1", "t2"],
                    "split": {
                        "w": 120.0,
                        "h": 60.0,
                        "bars": true,
                        "tree": { "id": "t1" }
                    }
                }
            }
        });
        let (store, report) = import_browser_json_into_storage(
            &doc.to_string(),
            None,
            Box::new(MemoryStorage::default()),
            "probe",
        )
        .expect("import succeeds with downgraded split");
        let g = store.group("g-1").expect("group kept");
        assert_eq!(g.layout, GroupLayout::Tile);
        assert!(g.split.is_none());
        assert!(report
            .unmapped
            .iter()
            .any(|u| u.key == "groups.g-1.split"));
    }
}
