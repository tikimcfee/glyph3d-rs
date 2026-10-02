use super::{LayoutController, LayoutZone, SpatialLayoutStrategy};
use crate::repo::{RepoParams, RepoLayoutMode, FileView};
use crate::glyph_scene::GroupRow;

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

    /// Retrieve the world-space translation for a zone entity.
    pub fn zone_world_translation(&self, zone_id: &str) -> Option<[f32; 3]> {
        let entity = *self.zone_entities.get(zone_id)?;
        let gtf = self.scene.world.get::<bevy_transform::prelude::GlobalTransform>(entity)?;
        Some(gtf.translation().into())
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
}
