use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use bevy_ecs::prelude::Entity;
use bevy_transform::components::Transform;

use crate::glyph_scene::GroupRow;
use crate::repo::{dir_tint, FileView, RepoLayoutMode, RepoParams};


use super::{LayoutController, LayoutZone, SpatialLayoutStrategy, MacroArrangement};
use crate::spatial_scene::SpatialScene;

impl LayoutController {
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

