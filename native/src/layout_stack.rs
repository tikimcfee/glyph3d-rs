//! Spatial layout stack and runtime control object.
//!
//! Provides dynamic, composable spatial layout of repository files and visual
//! items across the 3D canvas. Supports runtime-chosen layout strategies (Shelf,
//! Carrel, Column, Row, Pinned), dynamic carrel/zone creation, and runtime
//! file-to-layout assignments driven by users, agents, or LSP/Zed sidecars.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use crate::glyph_scene::GroupRow;
use crate::repo::{dir_tint, FileView, RepoLayoutMode, RepoParams};
use crate::spatial_scene::SpatialScene;
use bevy_ecs::entity::Entity;
use bevy_transform::components::Transform;

/// Strategy for laying out a group of files within its local bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialLayoutStrategy {
    /// Traditional height-classed shelf packing across the group.
    #[default]
    Shelf,
    /// Neighborhood carrel layout with lead-file prioritization.
    Carrel,
    /// Vertical single-column stack of files.
    Column,
    /// Horizontal side-by-side row of files.
    Row,
}

/// Macro packing mode for arranging multiple zones/carrels relative to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MacroArrangement {
    /// Canvas packing with horizontal/vertical avenues aiming for grid_aspect.
    #[default]
    CanvasAvenues,
    /// Horizontal row of carrels.
    LinearRow,
    /// Vertical column of carrels.
    LinearColumn,
}

/// A spatial zone or carrel in the layout stack.
#[derive(Debug, Clone)]
pub struct LayoutZone {
    /// Unique identifier for this zone (e.g. "dir:native/src", "desk:active", "zone-1").
    pub id: String,
    /// Human-readable title for UI display and spatial labels.
    pub title: String,
    /// Local layout strategy for files inside this zone.
    pub strategy: SpatialLayoutStrategy,
    /// Optional manual pin origin [x, y, z] in world space.
    pub pin_origin: Option<[f32; 3]>,
    /// Optional custom tint overriding default directory tint.
    pub custom_tint: Option<[f32; 3]>,
    /// Computed bounding dimensions of the zone after layout: [width, height, depth].
    pub computed_size: [f32; 3],
    /// Computed origin [x, y, z] in world space after macro layout.
    pub computed_origin: [f32; 3],
    /// Optional bound entity in the SpatialScene.
    pub entity: Option<Entity>,
}

impl LayoutZone {
    pub fn new(id: impl Into<String>, title: impl Into<String>, strategy: SpatialLayoutStrategy) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            strategy,
            pin_origin: None,
            custom_tint: None,
            computed_size: [0.0; 3],
            computed_origin: [0.0; 3],
            entity: None,
        }
    }
}

/// The layout stack: holds custom zones, explicit file assignments,
/// and macro-arrangement configuration.
#[derive(Debug, Clone, Default)]
pub struct LayoutStack {
    /// Fallback strategy for unassigned files.
    pub base_strategy: SpatialLayoutStrategy,
    /// Macro arrangement mode for multiple carrels/zones.
    pub macro_arrangement: MacroArrangement,
    /// User or agent defined layout zones.
    pub zones: Vec<LayoutZone>,
    /// Dynamic map from file `rel_path` to target `zone_id`.
    pub file_to_zone: HashMap<String, String>,
    /// Horizontal avenue spacing between zones.
    pub avenue_gap_x: f32,
    /// Vertical avenue spacing between zones.
    pub avenue_gap_y: f32,
}

/// Runtime control object for spatial layout.
///
/// Encapsulates the active layout mode, the layout stack, and repo parameters.
/// Allows users or agents to dynamically query and modify layout assignments
/// and re-evaluate file placements across the canvas.
pub struct LayoutController {
    pub mode: RepoLayoutMode,
    pub stack: LayoutStack,
    pub params: RepoParams,
    pub scene: SpatialScene,
    pub zone_entities: HashMap<String, Entity>,
    pub file_entities: Vec<Entity>,
}

impl std::fmt::Debug for LayoutController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutController")
            .field("mode", &self.mode)
            .field("stack", &self.stack)
            .field("params", &self.params)
            .finish()
    }
}

impl LayoutController {
    /// Create a controller with default settings from repo parameters.
    pub fn new(params: RepoParams) -> Self {
        Self::from_mode(params.layout_mode, params)
    }

    /// Create a controller for a specific layout mode.
    pub fn from_mode(mode: RepoLayoutMode, params: RepoParams) -> Self {
        let base_strategy = match mode {
            RepoLayoutMode::Shelf => SpatialLayoutStrategy::Shelf,
            RepoLayoutMode::Carrel => SpatialLayoutStrategy::Carrel,
        };
        let avenue_gap_x = (params.gap_x * 4.0).max(12.0);
        let avenue_gap_y = (params.gap_y * 3.0).max(18.0);
        Self {
            mode,
            stack: LayoutStack {
                base_strategy,
                macro_arrangement: MacroArrangement::CanvasAvenues,
                zones: Vec::new(),
                file_to_zone: HashMap::new(),
                avenue_gap_x,
                avenue_gap_y,
            },
            params,
            scene: SpatialScene::new(),
            zone_entities: HashMap::new(),
            file_entities: Vec::new(),
        }
    }

    /// Set the global layout mode (e.g. Shelf vs Carrel).
    pub fn set_mode(&mut self, mode: RepoLayoutMode) {
        self.mode = mode;
        self.stack.base_strategy = match mode {
            RepoLayoutMode::Shelf => SpatialLayoutStrategy::Shelf,
            RepoLayoutMode::Carrel => SpatialLayoutStrategy::Carrel,
        };
    }

    /// Dynamically assign a file to a specific zone / carrel.
    pub fn assign_file(&mut self, rel_path: impl Into<String>, zone_id: impl Into<String>) {
        self.stack.file_to_zone.insert(rel_path.into(), zone_id.into());
    }

    /// Remove a file's custom zone assignment, reverting to default grouping.
    pub fn unassign_file(&mut self, rel_path: &str) {
        self.stack.file_to_zone.remove(rel_path);
    }

    /// Create or return a mutable reference to a layout zone.
    pub fn get_or_create_zone(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        strategy: SpatialLayoutStrategy,
    ) -> &mut LayoutZone {
        let id_str = id.into();
        if let Some(pos) = self.stack.zones.iter().position(|z| z.id == id_str) {
            return &mut self.stack.zones[pos];
        }
        self.stack.zones.push(LayoutZone::new(id_str, title, strategy));
        self.stack.zones.last_mut().expect("just pushed")
    }

    /// Remove a zone by ID and unassign any files pointing to it.
    pub fn remove_zone(&mut self, zone_id: &str) {
        self.stack.zones.retain(|z| z.id != zone_id);
        self.stack.file_to_zone.retain(|_, zid| zid != zone_id);
    }

    /// Query which zone ID a file belongs to.
    pub fn zone_for_file(&self, rel_path: &str, dir: &str) -> String {
        if let Some(zid) = self.stack.file_to_zone.get(rel_path) {
            return zid.clone();
        }
        match self.mode {
            RepoLayoutMode::Shelf => "base:shelf".to_string(),
            RepoLayoutMode::Carrel => format!("dir:{dir}"),
        }
    }

    /// Translate a zone by a delta vector. Automatically propagates down
    /// to all contained files and sheets in that zone.
    pub fn move_zone(&mut self, zone_id: &str, delta: glam::Vec3) -> bool {
        if let Some(&entity) = self.zone_entities.get(zone_id) {
            if let Some(mut transform) = self.scene.world.get_mut::<Transform>(entity) {
                transform.translation += delta;
            }
            self.scene.update_transforms();
            true
        } else {
            false
        }
    }

    /// Scale a zone by a factor. Automatically propagates down to all children.
    pub fn scale_zone(&mut self, zone_id: &str, factor: f32) -> bool {
        if let Some(&entity) = self.zone_entities.get(zone_id) {
            if let Some(mut transform) = self.scene.world.get_mut::<Transform>(entity) {
                transform.scale *= factor;
            }
            self.scene.update_transforms();
            true
        } else {
            false
        }
    }

    /// Retrieve the world-space bounding box for a zone: [min, max].
    pub fn zone_world_bounds(&self, zone_id: &str) -> Option<([f32; 3], [f32; 3])> {
        let entity = *self.zone_entities.get(zone_id)?;
        self.scene.world_bounds(entity)
    }

    /// Retrieve the world-space bounding box for a file view by index: [min, max].
    pub fn file_world_bounds(&self, file_idx: usize) -> Option<([f32; 3], [f32; 3])> {
        let entity = *self.file_entities.get(file_idx)?;
        self.scene.world_bounds(entity)
    }

    /// Synchronize all spatial hierarchy nodes into the GPU GroupRow buffer.
    /// Returns the list of updated group IDs.
    pub fn sync_gpu_groups(&mut self, groups: &mut [GroupRow]) -> Vec<u32> {
        self.scene.sync_to_group_rows(groups)
    }

    /// Populate the SpatialScene for Shelf layout mode, so that file nodes
    /// and the base shelf zone exist for picking, dragging, and bounds queries.
    pub fn build_shelf_hierarchy(&mut self, views: &[FileView]) {
        self.scene = SpatialScene::new();
        self.zone_entities.clear();
        self.file_entities = vec![Entity::PLACEHOLDER; views.len()];

        let stack_root = self.scene.spawn_root("stack_root");
        let shelf_zone_node = self.scene.spawn_zone(
            stack_root,
            "base:shelf",
            "Shelf",
            Transform::IDENTITY,
            [0.0, 0.0],
        );
        self.zone_entities.insert("base:shelf".to_string(), shelf_zone_node);

        for (i, v) in views.iter().enumerate() {
            let file_node = self.scene.spawn_file_card(
                shelf_zone_node,
                v.rel_path.clone(),
                v.dir.clone(),
                i as u32,
                Transform::from_xyz(v.offset[0], v.offset[1], v.offset[2]),
                ([0.0, -v.height, v.z_min], [v.width, 0.0, v.z_max]),
                crate::repo::dir_tint(&v.dir),
            );
            self.file_entities[i] = file_node;
        }
        self.scene.update_transforms();
    }

    /// Apply the layout stack and control object to place all files on the canvas.
    ///
    /// When in pure Shelf mode without custom zones or file overrides, delegates
    /// directly to the canonical height-classed shelf packing (`layout_shelf`) to
    /// guarantee 100% byte-identical output with baseline golden gates.
    pub fn apply(
        &mut self,
        views: &mut [FileView],
    ) -> (Vec<GroupRow>, [f32; 3], [f32; 3]) {
        // Fast-path: pure unmodified shelf layout
        if self.mode == RepoLayoutMode::Shelf
            && self.stack.zones.is_empty()
            && self.stack.file_to_zone.is_empty()
        {
            let res = crate::repo::layout_shelf(views, &self.params);
            self.build_shelf_hierarchy(views);
            return res;
        }

        // Dynamic multi-zone layout (handles Carrel and custom zones)
        self.apply_dynamic_stack(views)
    }

    fn apply_dynamic_stack(
        &mut self,
        views: &mut [FileView],
    ) -> (Vec<GroupRow>, [f32; 3], [f32; 3]) {
        // 1. Group files by zone
        let mut zone_membership: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, v) in views.iter().enumerate() {
            let zid = self.zone_for_file(&v.rel_path, &v.dir);
            zone_membership.entry(zid).or_default().push(i);
        }

        struct ZoneEval {
            zone: LayoutZone,
            files: Vec<usize>,
            local_offsets: Vec<[f32; 2]>,
        }

        let mut evaluated_zones: Vec<ZoneEval> = Vec::with_capacity(zone_membership.len());

        // 2. Evaluate each zone locally
        for (zid, files) in zone_membership {
            let (title, strategy, custom_tint, pin_origin) = if let Some(existing) = self.stack.zones.iter().find(|z| z.id == zid) {
                (existing.title.clone(), existing.strategy, existing.custom_tint, existing.pin_origin)
            } else if let Some(dir) = zid.strip_prefix("dir:") {
                (dir.to_string(), self.stack.base_strategy, None, None)
            } else {
                (zid.clone(), self.stack.base_strategy, None, None)
            };

            let (local_offsets, w, h) = Self::layout_local_zone(views, &files, strategy, &self.params);

            let mut zone = LayoutZone::new(zid, title, strategy);
            zone.custom_tint = custom_tint;
            zone.pin_origin = pin_origin;
            zone.computed_size = [w, h, 0.0];

            evaluated_zones.push(ZoneEval {
                zone,
                files,
                local_offsets,
            });
        }

        // 3. Macro packing of zones across the repository canvas
        let avenue_gap_x = self.stack.avenue_gap_x;
        let avenue_gap_y = self.stack.avenue_gap_y;

        let total_macro_area: f32 = evaluated_zones
            .iter()
            .map(|z| (z.zone.computed_size[0] + avenue_gap_x) * (z.zone.computed_size[1] + avenue_gap_y))
            .sum();
        let target_macro_w = (total_macro_area * self.params.grid_aspect).sqrt().max(1.0);

        let mut macro_x = 0.0f32;
        let mut macro_top = 0.0f32;
        let mut macro_shelf_h = 0.0f32;
        let mut max_macro_x = 0.0f32;

        // Rebuild spatial scene
        self.scene = SpatialScene::new();
        self.zone_entities.clear();
        self.file_entities = vec![Entity::PLACEHOLDER; views.len()];
        let stack_root = self.scene.spawn_root("stack_root");

        for z in &mut evaluated_zones {
            let zone_w = z.zone.computed_size[0];
            let zone_h = z.zone.computed_size[1];

            let origin = if let Some(pin) = z.zone.pin_origin {
                pin
            } else {
                match self.stack.macro_arrangement {
                    MacroArrangement::CanvasAvenues => {
                        if macro_x > 0.0 && (macro_x + zone_w > target_macro_w) {
                            macro_top -= macro_shelf_h + avenue_gap_y;
                            macro_x = 0.0;
                            macro_shelf_h = 0.0;
                        }
                        let orig = [macro_x, macro_top, 0.0];
                        macro_x += zone_w + avenue_gap_x;
                        max_macro_x = max_macro_x.max(macro_x - avenue_gap_x);
                        macro_shelf_h = macro_shelf_h.max(zone_h);
                        orig
                    }
                    MacroArrangement::LinearRow => {
                        let orig = [macro_x, 0.0, 0.0];
                        macro_x += zone_w + avenue_gap_x;
                        max_macro_x = max_macro_x.max(macro_x - avenue_gap_x);
                        macro_shelf_h = macro_shelf_h.max(zone_h);
                        orig
                    }
                    MacroArrangement::LinearColumn => {
                        let orig = [0.0, macro_top, 0.0];
                        macro_top -= zone_h + avenue_gap_y;
                        max_macro_x = max_macro_x.max(zone_w);
                        macro_shelf_h = macro_shelf_h.max(zone_h);
                        orig
                    }
                }
            };

            z.zone.computed_origin = origin;

            let zone_node = self.scene.spawn_zone(
                stack_root,
                z.zone.id.clone(),
                z.zone.title.clone(),
                Transform::from_xyz(origin[0], origin[1], origin[2]),
                [zone_w, zone_h],
            );
            z.zone.entity = Some(zone_node);
            self.zone_entities.insert(z.zone.id.clone(), zone_node);

            // Spawn the zone's container plate as a child entity of zone_node
            let pad = 2.0f32;
            let custom_tint = self.stack.zones.iter().find(|zk| zk.id == z.zone.id).and_then(|zk| zk.custom_tint);
            let dir_for_zone = z.files.first().map(|&idx| views[idx].dir.as_str()).unwrap_or("");
            let tint = custom_tint.unwrap_or_else(|| dir_tint(dir_for_zone));
            let plate_color = [tint[0] * 0.15, tint[1] * 0.15, tint[2] * 0.18, 0.95];

            self.scene.spawn_plate(
                zone_node,
                format!("plate:{}", z.zone.id),
                Transform::from_xyz(-pad, pad, -0.05),
                [zone_w + pad * 2.0, zone_h + pad * 2.0],
                [0.0, -(zone_h + pad * 2.0)],
                plate_color,
            );

            for (file_in_zone, &file_view_idx) in z.files.iter().enumerate() {
                let [rel_x, rel_y] = z.local_offsets[file_in_zone];
                views[file_view_idx].offset = [
                    origin[0] + rel_x,
                    origin[1] + rel_y,
                    origin[2],
                ];

                let v = &views[file_view_idx];
                let tint = z.zone.custom_tint.unwrap_or_else(|| dir_tint(&v.dir));
                let file_node = self.scene.spawn_file_card(
                    zone_node,
                    views[file_view_idx].rel_path.clone(),
                    v.dir.clone(),
                    file_view_idx as u32,
                    Transform::from_xyz(rel_x, rel_y, 0.0),
                    ([0.0, -v.height, v.z_min], [v.width, 0.0, v.z_max]),
                    tint,
                );
                self.file_entities[file_view_idx] = file_node;
            }
        }
        self.scene.update_transforms();

        let min_macro_y = match self.stack.macro_arrangement {
            MacroArrangement::CanvasAvenues | MacroArrangement::LinearColumn => macro_top - macro_shelf_h,
            MacroArrangement::LinearRow => -macro_shelf_h,
        };

        // 4. Update group rows and bounds in file-index order
        let mut groups = Vec::with_capacity(views.len());
        let (mut min_z, mut max_z) = (0.0f32, 0.0f32);
        for v in views.iter() {
            min_z = min_z.min(v.offset[2] + v.z_min);
            max_z = max_z.max(v.offset[2] + v.z_max);
            let zid = self.zone_for_file(&v.rel_path, &v.dir);
            let custom_tint = self.stack.zones.iter().find(|z| z.id == zid).and_then(|z| z.custom_tint);
            let tint = custom_tint.unwrap_or_else(|| dir_tint(&v.dir));
            groups.push(GroupRow::tinted(v.offset, tint));
        }

        log::info!(
            "layout [dynamic stack]: {} zones, target_w {:.0}, field {:.0}x{:.0}",
            evaluated_zones.len(),
            target_macro_w,
            max_macro_x.max(1.0),
            -min_macro_y,
        );

        (
            groups,
            [0.0, min_macro_y, min_z],
            [max_macro_x.max(1.0), self.params.line_height as f32, max_z],
        )
    }

    /// Lay out a slice of files locally according to a spatial strategy.
    fn layout_local_zone(
        views: &[FileView],
        files: &[usize],
        strategy: SpatialLayoutStrategy,
        params: &RepoParams,
    ) -> (Vec<[f32; 2]>, f32, f32) {
        match strategy {
            SpatialLayoutStrategy::Carrel => {
                let entry_score = |path: &str| -> usize {
                    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
                    match name {
                        "lib.rs" | "main.rs" | "index.ts" | "index.js" => 0,
                        "mod.rs" => 1,
                        "README.md" => 2,
                        "Cargo.toml" | "package.json" => 3,
                        _ => 4,
                    }
                };
                let mut sorted_files = files.to_vec();
                sorted_files.sort_by(|&a, &b| {
                    let sa = entry_score(&views[a].rel_path);
                    let sb = entry_score(&views[b].rel_path);
                    sa.cmp(&sb).then_with(|| views[a].rel_path.cmp(&views[b].rel_path))
                });

                let carrel_area: f32 = sorted_files
                    .iter()
                    .map(|&i| (views[i].width + params.gap_x) * (views[i].height + params.gap_y))
                    .sum();
                let max_file_w: f32 = sorted_files
                    .iter()
                    .map(|&i| views[i].width)
                    .fold(0.0f32, f32::max);

                let target_w = (carrel_area * 1.33).sqrt().max(max_file_w).max(1.0);

                let mut local_map: HashMap<usize, [f32; 2]> = HashMap::with_capacity(files.len());
                let mut x = 0.0f32;
                let mut shelf_top = 0.0f32;
                let mut shelf_h = 0.0f32;
                let mut max_local_x = 0.0f32;

                for &i in &sorted_files {
                    let v = &views[i];
                    if x > 0.0 && (x + v.width > target_w) {
                        shelf_top -= shelf_h + params.gap_y;
                        x = 0.0;
                        shelf_h = 0.0;
                    }
                    local_map.insert(i, [x, shelf_top]);
                    x += v.width + params.gap_x;
                    max_local_x = max_local_x.max(x - params.gap_x);
                    shelf_h = shelf_h.max(v.height);
                }

                let carrel_w = max_local_x.max(1.0);
                let carrel_h = (-shelf_top + shelf_h).max(params.line_height as f32);

                let ordered_offsets = files.iter().map(|idx| local_map[idx]).collect();
                (ordered_offsets, carrel_w, carrel_h)
            }
            SpatialLayoutStrategy::Shelf => {
                let page_h = params.page_rows as f32 * params.line_height as f32;
                let is_small = |h: f32| h <= page_h * 2.0;
                let is_monster = |h: f32| h > page_h * 16.0;

                let mut small = Vec::new();
                let mut med = Vec::new();
                let mut monster = Vec::new();
                for &i in files {
                    let h = views[i].height;
                    if is_small(h) {
                        small.push(i);
                    } else if is_monster(h) {
                        monster.push(i);
                    } else {
                        med.push(i);
                    }
                }

                let total_area: f32 = files
                    .iter()
                    .map(|&i| (views[i].width + params.gap_x) * (views[i].height + params.gap_y))
                    .sum();
                let target_w = (total_area * params.grid_aspect).sqrt().max(1.0);

                let mut local_map: HashMap<usize, [f32; 2]> = HashMap::with_capacity(files.len());
                let mut shelf_top = 0.0f32;
                let mut max_x = 0.0f32;

                for class_files in [&small, &med, &monster] {
                    if class_files.is_empty() {
                        continue;
                    }
                    let mut x = 0.0f32;
                    let mut shelf_h = 0.0f32;
                    for &i in class_files.iter() {
                        let v = &views[i];
                        if x > 0.0 && (x + v.width > target_w) {
                            shelf_top -= shelf_h + params.gap_y;
                            x = 0.0;
                            shelf_h = 0.0;
                        }
                        local_map.insert(i, [x, shelf_top]);
                        x += v.width + params.gap_x;
                        max_x = max_x.max(x - params.gap_x);
                        shelf_h = shelf_h.max(v.height);
                    }
                    shelf_top -= shelf_h + params.gap_y;
                }

                let w = max_x.max(1.0);
                let h = -shelf_top;
                let ordered_offsets = files.iter().map(|idx| local_map[idx]).collect();
                (ordered_offsets, w, h)
            }
            SpatialLayoutStrategy::Column => {
                let mut y = 0.0f32;
                let mut max_w = 0.0f32;
                let mut offsets = Vec::with_capacity(files.len());
                for &i in files {
                    let v = &views[i];
                    offsets.push([0.0, y]);
                    max_w = max_w.max(v.width);
                    y -= v.height + params.gap_y;
                }
                (offsets, max_w.max(1.0), -y)
            }
            SpatialLayoutStrategy::Row => {
                let mut x = 0.0f32;
                let mut max_h = 0.0f32;
                let mut offsets = Vec::with_capacity(files.len());
                for &i in files {
                    let v = &views[i];
                    offsets.push([x, 0.0]);
                    max_h = max_h.max(v.height);
                    x += v.width + params.gap_x;
                }
                (offsets, x.max(1.0), max_h)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_views() -> Vec<FileView> {
        let dummy_item = crate::repo::file_item_params(&RepoParams::default(), 100, 5);
        vec![
            FileView {
                rel_path: "src/main.rs".to_string(),
                dir: "src".to_string(),
                group_id: 0,
                record_count: 10,
                slot_base: 0,
                slot_count: 10,
                width: 50.0,
                height: 40.0,
                z_min: 0.0,
                z_max: 0.0,
                offset: [0.0; 3],
                item: dummy_item,
            },
            FileView {
                rel_path: "src/lib.rs".to_string(),
                dir: "src".to_string(),
                group_id: 1,
                record_count: 10,
                slot_base: 10,
                slot_count: 10,
                width: 60.0,
                height: 80.0,
                z_min: 0.0,
                z_max: 0.0,
                offset: [0.0; 3],
                item: dummy_item,
            },
            FileView {
                rel_path: "tests/smoke.rs".to_string(),
                dir: "tests".to_string(),
                group_id: 2,
                record_count: 5,
                slot_base: 20,
                slot_count: 5,
                width: 30.0,
                height: 20.0,
                z_min: 0.0,
                z_max: 0.0,
                offset: [0.0; 3],
                item: dummy_item,
            },
        ]
    }

    #[test]
    fn default_shelf_mode_equals_layout_shelf() {
        let mut views1 = make_test_views();
        let mut views2 = make_test_views();
        let params = RepoParams::default();

        let (groups1, bmin1, bmax1) = crate::repo::layout_shelf(&mut views1, &params);
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Shelf, params);
        let (groups2, bmin2, bmax2) = controller.apply(&mut views2);

        assert_eq!(groups1.len(), groups2.len());
        assert_eq!(bmin1, bmin2);
        assert_eq!(bmax1, bmax2);
        for (v1, v2) in views1.iter().zip(views2.iter()) {
            assert_eq!(v1.offset, v2.offset);
        }
    }

    #[test]
    fn dynamic_assignment_moves_file_to_desk() {
        let mut views = make_test_views();
        let params = RepoParams::default();
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

        // Initially in dir:src and dir:tests
        assert_eq!(controller.zone_for_file("src/main.rs", "src"), "dir:src");

        // User/Agent pulls main.rs into an active desk
        controller.assign_file("src/main.rs", "desk:active");
        assert_eq!(controller.zone_for_file("src/main.rs", "src"), "desk:active");

        let (groups, bounds_min, bounds_max) = controller.apply(&mut views);
        assert_eq!(groups.len(), 3);
        assert!(bounds_max[0] > 0.0);
        assert!(bounds_min[1] < 0.0);
    }

    #[test]
    fn test_moving_zone_moves_files_in_gpu_groups() {
        let mut views = make_test_views();
        let params = RepoParams::default();
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

        // Put src/main.rs and src/lib.rs on desk:active
        controller.assign_file("src/main.rs", "desk:active");
        controller.assign_file("src/lib.rs", "desk:active");

        let (mut groups, _, _) = controller.apply(&mut views);
        let orig_main_pos = groups[0].cols[0];
        let orig_lib_pos = groups[1].cols[0];
        let orig_tests_pos = groups[2].cols[0];

        // Bounds are available for the desk zone
        assert!(controller.zone_world_bounds("desk:active").is_some());

        // Now move the active desk by (100, 50, -10)
        let moved = controller.move_zone("desk:active", glam::Vec3::new(100.0, 50.0, -10.0));
        assert!(moved);

        // Synchronize GPU groups
        let updated_gids = controller.sync_gpu_groups(&mut groups);
        assert!(updated_gids.contains(&0));
        assert!(updated_gids.contains(&1));

        // main.rs and lib.rs moved by exactly (100, 50, -10)
        assert_eq!(groups[0].cols[0][0], orig_main_pos[0] + 100.0);
        assert_eq!(groups[0].cols[0][1], orig_main_pos[1] + 50.0);
        assert_eq!(groups[0].cols[0][2], orig_main_pos[2] - 10.0);

        assert_eq!(groups[1].cols[0][0], orig_lib_pos[0] + 100.0);
        assert_eq!(groups[1].cols[0][1], orig_lib_pos[1] + 50.0);
        assert_eq!(groups[1].cols[0][2], orig_lib_pos[2] - 10.0);

        // tests/smoke.rs in the other zone did NOT move
        assert_eq!(groups[2].cols[0], orig_tests_pos);
    }

    #[test]
    fn test_scaling_zone_scales_files_in_gpu_groups() {
        let mut views = make_test_views();
        let params = RepoParams::default();
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

        controller.assign_file("src/main.rs", "desk:scaled");
        let (mut groups, _, _) = controller.apply(&mut views);

        let scaled = controller.scale_zone("desk:scaled", 2.5);
        assert!(scaled);

        let updated_gids = controller.sync_gpu_groups(&mut groups);
        assert!(updated_gids.contains(&0));
        assert_eq!(groups[0].cols[3][0], 2.5);
        assert_eq!(groups[0].cols[3][1], 2.5);
        assert_eq!(groups[0].cols[3][2], 2.5);
    }

    #[test]
    fn test_shelf_hierarchy_populated() {
        let mut views = make_test_views();
        let params = RepoParams::default();
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Shelf, params);

        let (groups, _, _) = controller.apply(&mut views);
        assert_eq!(groups.len(), 3);

        // base:shelf zone must exist
        assert!(controller.zone_world_bounds("base:shelf").is_some());
        // file entities must exist
        assert_eq!(controller.file_entities.len(), 3);
        assert!(controller.file_world_bounds(0).is_some());
    }

    #[test]
    fn test_zone_world_bounds_covers_children() {
        let mut views = make_test_views();
        let params = RepoParams::default();
        let mut controller = LayoutController::from_mode(RepoLayoutMode::Carrel, params);

        controller.assign_file("src/main.rs", "desk:combo");
        controller.assign_file("src/lib.rs", "desk:combo");

        let _ = controller.apply(&mut views);

        let zone_bounds = controller.zone_world_bounds("desk:combo").expect("zone bounds exist");
        let file0_bounds = controller.file_world_bounds(0).expect("file 0 bounds exist");
        let file1_bounds = controller.file_world_bounds(1).expect("file 1 bounds exist");

        // Zone bounds must envelope both child file bounds
        assert!(zone_bounds.0[0] <= file0_bounds.0[0] && zone_bounds.0[0] <= file1_bounds.0[0]);
        assert!(zone_bounds.1[0] >= file0_bounds.1[0] && zone_bounds.1[0] >= file1_bounds.1[0]);
    }
}
