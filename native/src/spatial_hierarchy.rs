//! Spatial hierarchy & transform parenting system.
//!
//! Provides a fast, arena-based scene graph where entities (`LayoutZone`, `Book`,
//! `File`, `Sheet`) can be parented, moved, rotated, and scaled.
//!
//! Forward propagation computes world-space transforms from local parent-child
//! compositions, and syncs leaf nodes with GPU `GroupRow` slots (80 bytes)
//! for instantaneous Slug WGSL rendering without shader changes.

use glam::{Mat4, Quat, Vec3};
use crate::glyph_scene::GroupRow;

/// Strongly typed node handle within a `SpatialHierarchy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub usize);

/// 3D TRS (translation, rotation, scale) transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialTransform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for SpatialTransform {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl SpatialTransform {
    pub fn from_translation(t: Vec3) -> Self {
        Self {
            translation: t,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }

    pub fn from_xyz(x: f32, y: f32, z: f32) -> Self {
        Self::from_translation(Vec3::new(x, y, z))
    }

    /// Compose two transforms: `self * child` (child relative to self).
    #[inline]
    pub fn mul_transform(&self, child: &SpatialTransform) -> SpatialTransform {
        SpatialTransform {
            translation: self.translation + self.rotation * (self.scale * child.translation),
            rotation: (self.rotation * child.rotation).normalize(),
            scale: self.scale * child.scale,
        }
    }

    /// Transform a local point into the parent space of this transform.
    #[inline]
    pub fn transform_point(&self, point: Vec3) -> Vec3 {
        self.translation + self.rotation * (self.scale * point)
    }

    /// Convert to a 4x4 affine matrix.
    #[inline]
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

/// Geometric mesh representation of an entity in 3D space.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum MeshGeometry {
    /// Planar rectangular quad: size [width, height], origin offset [ox, oy].
    Quad { size: [f32; 2], origin: [f32; 2] },
    /// 3D box: extents [width, height, depth].
    Box { extents: [f32; 3] },
    /// Slug glyph vector field bound to a group_id in GPU storage.
    Glyphs { group_id: u32 },
    /// Non-visual / organizational transform node.
    #[default]
    None,
}

/// Shading material for an entity.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Material {
    /// Unlit flat or translucent solid color (linear RGBA).
    Flat { color: [f32; 4] },
    /// Analytic vector glyph shader (Slug WGSL).
    Slug,
    /// No material.
    #[default]
    None,
}

/// An entity node in the spatial scene graph.
#[derive(Debug, Clone)]
pub struct SpatialNode {
    pub id: NodeId,
    pub name: String,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub local: SpatialTransform,
    pub world: SpatialTransform,
    /// Visual mesh geometry (Quad, Box, Glyphs, None).
    pub mesh: MeshGeometry,
    /// Shading material (Flat, Slug, None).
    pub material: Material,
    /// Visibility flag.
    pub visible: bool,
    /// Optional binding to a GPU `GroupRow` table slot (if this node draws glyphs).
    pub group_id: Option<u32>,
    /// Tint color for this node's GroupRow (RGBA)
    pub tint: [f32; 4],
    /// Local bounding box: [min_x, min_y, min_z] .. [max_x, max_y, max_z].
    /// Serves as the authoritative bounds for page backgrounds and collision.
    pub local_bounds: Option<([f32; 3], [f32; 3])>,
    /// Dirty flag indicating world transform or children need recomputing.
    pub dirty: bool,
}

impl SpatialNode {
    pub fn new(id: NodeId, name: String) -> Self {
        Self {
            id,
            name,
            parent: None,
            children: Vec::new(),
            local: SpatialTransform::default(),
            world: SpatialTransform::default(),
            mesh: MeshGeometry::None,
            material: Material::None,
            visible: true,
            group_id: None,
            tint: [1.0, 1.0, 1.0, 1.0],
            local_bounds: None,
            dirty: true,
        }
    }
}

/// Arena-based hierarchical scene graph for spatial layout and node parenting.
#[derive(Debug, Clone, Default)]
pub struct SpatialHierarchy {
    nodes: Vec<Option<SpatialNode>>,
    free_list: Vec<usize>,
    root_nodes: Vec<NodeId>,
}

impl SpatialHierarchy {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            free_list: Vec::new(),
            root_nodes: Vec::new(),
        }
    }

    /// Allocate a new node in the hierarchy.
    pub fn create_node(&mut self, name: impl Into<String>) -> NodeId {
        let name = name.into();
        let id_raw = if let Some(idx) = self.free_list.pop() {
            let id = NodeId(idx);
            self.nodes[idx] = Some(SpatialNode::new(id, name));
            idx
        } else {
            let idx = self.nodes.len();
            let id = NodeId(idx);
            self.nodes.push(Some(SpatialNode::new(id, name)));
            idx
        };
        let id = NodeId(id_raw);
        self.root_nodes.push(id);
        id
    }

    /// Retrieve an immutable reference to a node.
    pub fn get(&self, id: NodeId) -> Option<&SpatialNode> {
        self.nodes.get(id.0).and_then(|opt| opt.as_ref())
    }

    /// Retrieve a mutable reference to a node.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut SpatialNode> {
        self.nodes.get_mut(id.0).and_then(|opt| opt.as_mut())
    }

    /// Attach a child node to a parent node.
    /// If the child already had a parent, it is detached from its previous parent first.
    pub fn attach_child(&mut self, parent_id: NodeId, child_id: NodeId) {
        if parent_id == child_id {
            log::warn!("spatial_hierarchy: cannot parent node {:?} to itself", child_id);
            return;
        }

        // Prevent cyclic parenting: parent cannot be a descendant of child
        if self.is_descendant_of(parent_id, child_id) {
            log::warn!("spatial_hierarchy: cyclic parenting detected: {:?} is already descendant of {:?}", parent_id, child_id);
            return;
        }

        self.detach(child_id);

        if let Some(parent) = self.get_mut(parent_id) {
            if !parent.children.contains(&child_id) {
                parent.children.push(child_id);
            }
        }
        if let Some(child) = self.get_mut(child_id) {
            child.parent = Some(parent_id);
            child.dirty = true;
        }

        // Remove from root_nodes
        self.root_nodes.retain(|&r| r != child_id);
        self.mark_dirty(child_id);
    }

    /// Detach a node from its parent, making it a root node.
    pub fn detach(&mut self, id: NodeId) {
        let prev_parent = self.get(id).and_then(|n| n.parent);
        if let Some(parent_id) = prev_parent {
            if let Some(parent) = self.get_mut(parent_id) {
                parent.children.retain(|&c| c != id);
            }
            if let Some(node) = self.get_mut(id) {
                node.parent = None;
                node.dirty = true;
            }
            if !self.root_nodes.contains(&id) {
                self.root_nodes.push(id);
            }
            self.mark_dirty(id);
        }
    }

    /// Remove a node and detach all its children.
    pub fn remove_node(&mut self, id: NodeId) -> Option<SpatialNode> {
        let children = self.get(id).map(|n| n.children.clone()).unwrap_or_default();
        for child in children {
            self.detach(child);
        }
        self.detach(id);
        self.root_nodes.retain(|&r| r != id);

        if id.0 < self.nodes.len() {
            let removed = self.nodes[id.0].take();
            self.free_list.push(id.0);
            removed
        } else {
            None
        }
    }

    /// Set local transform of a node and mark its subtree dirty.
    pub fn set_local_transform(&mut self, id: NodeId, transform: SpatialTransform) {
        if let Some(node) = self.get_mut(id) {
            node.local = transform;
            node.dirty = true;
        }
        self.mark_dirty(id);
    }

    /// Translate a node relative to its current local position.
    pub fn translate(&mut self, id: NodeId, delta: Vec3) {
        if let Some(node) = self.get_mut(id) {
            node.local.translation += delta;
            node.dirty = true;
        }
        self.mark_dirty(id);
    }

    /// Uniformly scale a node relative to its current local scale.
    pub fn scale(&mut self, id: NodeId, factor: f32) {
        if let Some(node) = self.get_mut(id) {
            node.local.scale *= factor;
            node.dirty = true;
        }
        self.mark_dirty(id);
    }

    /// Spawn a child entity attached to a parent node with specified transform, mesh, and material.
    pub fn spawn_child(
        &mut self,
        parent: NodeId,
        name: impl Into<String>,
        local: SpatialTransform,
        mesh: MeshGeometry,
        material: Material,
    ) -> NodeId {
        let child = self.create_node(name);
        self.set_local_transform(child, local);
        self.set_mesh(child, mesh);
        self.set_material(child, material);
        self.attach_child(parent, child);
        child
    }

    /// Set mesh geometry for a node.
    pub fn set_mesh(&mut self, id: NodeId, mesh: MeshGeometry) {
        if let Some(node) = self.get_mut(id) {
            node.mesh = mesh;
        }
    }

    /// Set shading material for a node.
    pub fn set_material(&mut self, id: NodeId, material: Material) {
        if let Some(node) = self.get_mut(id) {
            node.material = material;
        }
    }

    /// Set visibility for a node.
    pub fn set_visible(&mut self, id: NodeId, visible: bool) {
        if let Some(node) = self.get_mut(id) {
            node.visible = visible;
        }
    }

    /// Set local bounding box for a node.
    pub fn set_local_bounds(&mut self, id: NodeId, min: [f32; 3], max: [f32; 3]) {
        if let Some(node) = self.get_mut(id) {
            node.local_bounds = Some((min, max));
        }
    }

    /// Set GPU GroupRow binding for this node.
    pub fn set_group_id(&mut self, id: NodeId, group_id: u32) {
        if let Some(node) = self.get_mut(id) {
            node.group_id = Some(group_id);
        }
    }

    /// Set tint color for this node (RGB / RGBA).
    pub fn set_tint(&mut self, id: NodeId, rgb: [f32; 3]) {
        if let Some(node) = self.get_mut(id) {
            node.tint = [rgb[0], rgb[1], rgb[2], 1.0];
        }
    }

    /// Check if `node` is a descendant of `ancestor`.
    pub fn is_descendant_of(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut curr = self.get(node).and_then(|n| n.parent);
        while let Some(p) = curr {
            if p == ancestor {
                return true;
            }
            curr = self.get(p).and_then(|n| n.parent);
        }
        false
    }

    /// Mark a node and all of its descendants as dirty.
    pub fn mark_dirty(&mut self, id: NodeId) {
        let mut stack = vec![id];
        while let Some(curr) = stack.pop() {
            if let Some(node) = self.get_mut(curr) {
                node.dirty = true;
                stack.extend(node.children.iter().copied());
            }
        }
    }

    /// Propagate world transforms down the tree for all dirty nodes.
    /// Returns the number of nodes updated.
    pub fn update_world_transforms(&mut self) -> usize {
        let mut updated_count = 0;
        let roots = self.root_nodes.clone();
        for root in roots {
            updated_count += self.propagate_node(root, SpatialTransform::default(), false);
        }
        updated_count
    }

    fn propagate_node(&mut self, id: NodeId, parent_world: SpatialTransform, parent_dirty: bool) -> usize {
        let Some(node) = self.get(id) else { return 0 };
        let is_dirty = node.dirty || parent_dirty;
        let children = node.children.clone();

        let world_transform = if is_dirty {
            parent_world.mul_transform(&node.local)
        } else {
            node.world
        };

        let mut updated = 0;
        if let Some(node_mut) = self.get_mut(id) {
            if is_dirty {
                node_mut.world = world_transform;
                node_mut.dirty = false;
                updated += 1;
            }
        }

        for child in children {
            updated += self.propagate_node(child, world_transform, is_dirty);
        }

        updated
    }

    /// Compute world-space axis-aligned bounding box (AABB) for a node and its subtree.
    pub fn world_bounds(&self, id: NodeId) -> Option<([f32; 3], [f32; 3])> {
        let node = self.get(id)?;
        let mut world_min = Vec3::splat(f32::INFINITY);
        let mut world_max = Vec3::splat(f32::NEG_INFINITY);
        let mut has_bounds = false;

        let local_box = if let Some((min, max)) = node.local_bounds {
            Some((min, max))
        } else {
            match &node.mesh {
                MeshGeometry::Quad { size, origin } => {
                    Some(([origin[0], origin[1], 0.0], [origin[0] + size[0], origin[1] + size[1], 0.0]))
                }
                MeshGeometry::Box { extents } => {
                    let hx = extents[0] * 0.5;
                    let hy = extents[1] * 0.5;
                    let hz = extents[2] * 0.5;
                    Some(([-hx, -hy, -hz], [hx, hy, hz]))
                }
                _ => None,
            }
        };

        if let Some((min, max)) = local_box {
            let w = &node.world;
            let corners = [
                Vec3::new(min[0], min[1], min[2]),
                Vec3::new(max[0], min[1], min[2]),
                Vec3::new(min[0], max[1], min[2]),
                Vec3::new(max[0], max[1], min[2]),
                Vec3::new(min[0], min[1], max[2]),
                Vec3::new(max[0], min[1], max[2]),
                Vec3::new(min[0], max[1], max[2]),
                Vec3::new(max[0], max[1], max[2]),
            ];

            for c in corners {
                let wp = w.transform_point(c);
                world_min = world_min.min(wp);
                world_max = world_max.max(wp);
            }
            has_bounds = true;
        }

        for &child in &node.children {
            if let Some((c_min, c_max)) = self.world_bounds(child) {
                world_min = world_min.min(Vec3::from_array(c_min));
                world_max = world_max.max(Vec3::from_array(c_max));
                has_bounds = true;
            }
        }

        if has_bounds {
            Some((world_min.to_array(), world_max.to_array()))
        } else {
            None
        }
    }

    /// Collect GPU render instances for all visible primitive meshes in the hierarchy.
    pub fn collect_primitive_instances(&self) -> Vec<crate::glyph_scene::BackdropInst> {
        let mut instances = Vec::new();
        for node_opt in &self.nodes {
            let Some(node) = node_opt else { continue };
            if !node.visible {
                continue;
            }

            match (&node.mesh, &node.material) {
                (MeshGeometry::Quad { size, origin }, Material::Flat { color }) => {
                    let min_p = Vec3::new(origin[0], origin[1], 0.0);
                    let max_p = Vec3::new(origin[0] + size[0], origin[1] + size[1], 0.0);
                    let corners = [
                        Vec3::new(min_p.x, min_p.y, 0.0),
                        Vec3::new(max_p.x, min_p.y, 0.0),
                        Vec3::new(min_p.x, max_p.y, 0.0),
                        Vec3::new(max_p.x, max_p.y, 0.0),
                    ];
                    let mut w_min = Vec3::splat(f32::INFINITY);
                    let mut w_max = Vec3::splat(f32::NEG_INFINITY);
                    for c in corners {
                        let wp = node.world.transform_point(c);
                        w_min = w_min.min(wp);
                        w_max = w_max.max(wp);
                    }
                    instances.push(crate::glyph_scene::BackdropInst {
                        min: [w_min.x, w_min.y],
                        max: [w_max.x, w_max.y],
                        rgba: *color,
                        depth: [w_min.z, 0.0, 0.0, 0.0],
                    });
                }
                (MeshGeometry::Box { extents }, Material::Flat { color }) => {
                    let hx = extents[0] * 0.5;
                    let hy = extents[1] * 0.5;
                    let hz = extents[2] * 0.5;
                    let corners = [
                        Vec3::new(-hx, -hy, -hz),
                        Vec3::new(hx, -hy, -hz),
                        Vec3::new(-hx, hy, -hz),
                        Vec3::new(hx, hy, -hz),
                        Vec3::new(-hx, -hy, hz),
                        Vec3::new(hx, -hy, hz),
                        Vec3::new(-hx, hy, hz),
                        Vec3::new(hx, hy, hz),
                    ];
                    let mut w_min = Vec3::splat(f32::INFINITY);
                    let mut w_max = Vec3::splat(f32::NEG_INFINITY);
                    for c in corners {
                        let wp = node.world.transform_point(c);
                        w_min = w_min.min(wp);
                        w_max = w_max.max(wp);
                    }
                    instances.push(crate::glyph_scene::BackdropInst {
                        min: [w_min.x, w_min.y],
                        max: [w_max.x, w_max.y],
                        rgba: *color,
                        depth: [w_min.z, 0.0, 0.0, 0.0],
                    });
                }
                _ => {}
            }
        }
        instances
    }

    /// Synchronize all nodes bound to a `group_id` into the given `GroupRow` buffer.
    /// Returns the list of `group_id`s that were updated.
    pub fn sync_to_group_rows(&self, groups: &mut [GroupRow]) -> Vec<u32> {
        let mut updated = Vec::new();
        for node in self.nodes.iter().flatten() {
            if let Some(gid) = node.group_id {
                let idx = gid as usize;
                if idx < groups.len() {
                    let w = &node.world;
                    let g = &mut groups[idx];
                    g.cols[0] = [w.translation.x, w.translation.y, w.translation.z, 0.0];
                    g.cols[1] = [w.rotation.x, w.rotation.y, w.rotation.z, w.rotation.w];
                    g.cols[2] = node.tint;
                    g.cols[3] = [w.scale.x, w.scale.y, w.scale.z, 0.0];
                    updated.push(gid);
                }
            }
        }
        updated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parenting_translates_children() {
        let mut h = SpatialHierarchy::new();
        let carrel = h.create_node("carrel_src");
        let book = h.create_node("agent_book");
        let page = h.create_node("turn_page_1");

        h.attach_child(carrel, book);
        h.attach_child(book, page);

        // Position carrel at (100, 50, 0), book at (10, 0, -5), page at (2, 3, 0)
        h.set_local_transform(carrel, SpatialTransform::from_xyz(100.0, 50.0, 0.0));
        h.set_local_transform(book, SpatialTransform::from_xyz(10.0, 0.0, -5.0));
        h.set_local_transform(page, SpatialTransform::from_xyz(2.0, 3.0, 0.0));

        h.update_world_transforms();

        let page_world = h.get(page).unwrap().world;
        assert_eq!(page_world.translation, Vec3::new(112.0, 53.0, -5.0));

        // Now move the carrel by (20, -10, 5)
        h.translate(carrel, Vec3::new(20.0, -10.0, 5.0));
        h.update_world_transforms();

        let page_world_after = h.get(page).unwrap().world;
        assert_eq!(page_world_after.translation, Vec3::new(132.0, 43.0, 0.0));
    }

    #[test]
    fn parenting_rotates_and_scales_children() {
        let mut h = SpatialHierarchy::new();
        let parent = h.create_node("parent");
        let child = h.create_node("child");
        h.attach_child(parent, child);

        // Parent scaled by 2, child at local (5, 0, 0)
        h.set_local_transform(
            parent,
            SpatialTransform {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::splat(2.0),
            },
        );
        h.set_local_transform(child, SpatialTransform::from_xyz(5.0, 0.0, 0.0));

        h.update_world_transforms();

        // In world space, child should be at 10 + 2 * 5 = 20
        let child_world = h.get(child).unwrap().world;
        assert_eq!(child_world.translation.x, 20.0);
        assert_eq!(child_world.scale, Vec3::splat(2.0));
    }

    #[test]
    fn detach_makes_node_root() {
        let mut h = SpatialHierarchy::new();
        let parent = h.create_node("parent");
        let child = h.create_node("child");
        h.attach_child(parent, child);

        assert_eq!(h.get(child).unwrap().parent, Some(parent));
        h.detach(child);
        assert_eq!(h.get(child).unwrap().parent, None);
        assert!(h.root_nodes.contains(&child));
    }

    #[test]
    fn cyclic_parenting_is_refused() {
        let mut h = SpatialHierarchy::new();
        let a = h.create_node("a");
        let b = h.create_node("b");
        let c = h.create_node("c");

        h.attach_child(a, b);
        h.attach_child(b, c);

        // Trying to parent a to c must be refused
        h.attach_child(c, a);
        assert_eq!(h.get(a).unwrap().parent, None);
    }

    #[test]
    fn sync_to_group_rows_updates_gpu_table() {
        let mut h = SpatialHierarchy::new();
        let zone = h.create_node("zone");
        let file = h.create_node("file");
        h.attach_child(zone, file);

        h.set_group_id(file, 3);
        h.set_tint(file, [0.8, 0.2, 0.4]);
        h.set_local_transform(zone, SpatialTransform::from_xyz(50.0, 20.0, 0.0));
        h.set_local_transform(file, SpatialTransform::from_xyz(5.0, -2.0, -1.0));
        h.update_world_transforms();

        let mut groups = vec![GroupRow::identity([0.0; 3]); 5];
        let synced = h.sync_to_group_rows(&mut groups);

        assert_eq!(synced, vec![3]);
        assert_eq!(groups[3].cols[0], [55.0, 18.0, -1.0, 0.0]);
        assert_eq!(groups[3].cols[2], [0.8, 0.2, 0.4, 1.0]);
    }

    #[test]
    fn spawn_child_and_collect_primitive_instances() {
        let mut h = SpatialHierarchy::new();
        let carrel = h.create_node("carrel:dir:core");
        h.set_local_transform(carrel, SpatialTransform::from_xyz(100.0, 50.0, -10.0));

        // Spawn a background plate quad as a child entity
        let plate = h.spawn_child(
            carrel,
            "plate:dir:core",
            SpatialTransform::from_xyz(-2.0, 2.0, -0.05),
            MeshGeometry::Quad {
                size: [44.0, 34.0],
                origin: [0.0, -34.0],
            },
            Material::Flat {
                color: [0.1, 0.2, 0.3, 0.9],
            },
        );

        h.update_world_transforms();

        let insts = h.collect_primitive_instances();
        assert_eq!(insts.len(), 1);
        assert_eq!(insts[0].min[0], 98.0);
        assert_eq!(insts[0].max[0], 142.0);
        assert_eq!(insts[0].min[1], 18.0);
        assert_eq!(insts[0].max[1], 52.0);
        assert_eq!(insts[0].depth[0], -10.05);
        assert_eq!(insts[0].rgba, [0.1, 0.2, 0.3, 0.9]);

        // Hide plate
        h.set_visible(plate, false);
        assert!(h.collect_primitive_instances().is_empty());
    }
}
