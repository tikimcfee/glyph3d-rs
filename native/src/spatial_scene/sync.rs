use bevy_ecs::prelude::*;
use glam::{Vec3, Quat, Affine3A};
use crate::glyph_scene::GroupRow;
use super::{SpatialScene, SceneMeshDraws, SceneMeshMaterial, GlobalTransform, SceneMeshKind, Visible, MeshInstance, GlyphGroupBinding};

impl SpatialScene {
    /// Return the pre-extracted scene mesh instances ready for GPU dispatch.
    pub fn collect_mesh_instances(&self) -> SceneMeshDraws {
        self.cached_mesh_draws.clone()
    }

    /// Return a borrowed reference to the pre-extracted scene mesh instances.
    pub fn mesh_draws(&self) -> &SceneMeshDraws {
        &self.cached_mesh_draws
    }

    /// Extract all visible mesh instances (`Quad` and `Box`) into GPU-ready draw buffers.
    pub fn extract_mesh_instances(&mut self) -> SceneMeshDraws {
        let mut mesh_changed = false;
        let mut change_query = self.world.query_filtered::<
            (),
            (
                With<SceneMeshKind>,
                Or<(
                    Changed<GlobalTransform>,
                    Changed<SceneMeshMaterial>,
                    Changed<Visible>,
                )>,
            ),
        >();
        if change_query.iter(&self.world).next().is_some() {
            mesh_changed = true;
        }

        if !mesh_changed {
            return self.cached_mesh_draws.clone();
        }

        let mut draws = SceneMeshDraws {
            revision: self.cached_mesh_draws.revision + 1,
            ..Default::default()
        };

        let mut query = self.world.query::<(
            &GlobalTransform,
            &SceneMeshKind,
            &SceneMeshMaterial,
            Option<&Visible>,
        )>();

        for (gtf, mesh_kind, mat, vis) in query.iter(&self.world) {
            if let Some(v) = vis {
                if !v.0 {
                    continue;
                }
            }

            let parent_affine = gtf.affine();
            let parent_scale_sq = parent_affine.x_axis.length_squared()
                + parent_affine.y_axis.length_squared()
                + parent_affine.z_axis.length_squared();
            if parent_scale_sq < 1e-6 {
                continue;
            }

            match mesh_kind {
                SceneMeshKind::Quad { size, origin } => {
                    let local_affine = Affine3A::from_scale_rotation_translation(
                        Vec3::new(size[0], size[1], 1.0),
                        Quat::IDENTITY,
                        Vec3::new(origin[0] + size[0] * 0.5, origin[1] + size[1] * 0.5, 0.0),
                    );
                    let world_affine = parent_affine * local_affine;
                    draws.quads.push(MeshInstance::from_affine(
                        world_affine,
                        mat.color,
                        mat.params,
                    ));
                }
                SceneMeshKind::Box { extents } => {
                    let local_affine = Affine3A::from_scale(
                        Vec3::new(extents[0], extents[1], extents[2]),
                    );
                    let world_affine = parent_affine * local_affine;
                    draws.cubes.push(MeshInstance::from_affine(
                        world_affine,
                        mat.color,
                        mat.params,
                    ));
                }
            }
        }

        draws
    }

    /// Synchronize all entities with a `GlyphGroupBinding` into the GPU `GroupRow` buffer.
    pub fn sync_to_group_rows(&mut self, groups: &mut [GroupRow]) -> Vec<u32> {
        let mut updated = Vec::new();

        let mut query = self.world.query_filtered::<(
            &GlobalTransform,
            &GlyphGroupBinding,
            Option<&Visible>,
        ), Or<(Changed<GlobalTransform>, Changed<Visible>)>>();

        for (gtf, binding, vis) in query.iter(&self.world) {
            let is_visible = vis.map(|v| v.0).unwrap_or(true);
            let (scale, rotation, translation) = gtf.to_scale_rotation_translation();
            let has_scale = scale.length_squared() > 1e-6;

            let idx = binding.group_id as usize;
            if idx < groups.len() {
                let g = &mut groups[idx];
                if is_visible && has_scale {
                    g.cols[0] = [translation.x, translation.y, translation.z, 0.0];
                    g.cols[1] = [rotation.x, rotation.y, rotation.z, rotation.w];
                    g.cols[2] = binding.tint;
                    g.cols[3] = [scale.x, scale.y, scale.z, 0.0];
                } else {
                    g.cols[0] = [translation.x, translation.y, translation.z, 0.0];
                    g.cols[1] = [rotation.x, rotation.y, rotation.z, rotation.w];
                    g.cols[2] = [0.0, 0.0, 0.0, 0.0];
                    g.cols[3] = [0.0, 0.0, 0.0, 0.0];
                }
                updated.push(binding.group_id);
            }
        }

        self.world.clear_trackers();

        updated
    }

    /// Populate all entities with a `GlyphGroupBinding` into the GPU `GroupRow` buffer unconditionally.
    pub fn sync_all_to_group_rows(&mut self, groups: &mut [GroupRow]) {
        let mut query = self.world.query::<(
            &GlobalTransform,
            &GlyphGroupBinding,
            Option<&Visible>,
        )>();

        for (gtf, binding, vis) in query.iter(&self.world) {
            let is_visible = vis.map(|v| v.0).unwrap_or(true);
            let (scale, rotation, translation) = gtf.to_scale_rotation_translation();
            let has_scale = scale.length_squared() > 1e-6;

            let idx = binding.group_id as usize;
            if idx < groups.len() {
                let g = &mut groups[idx];
                if is_visible && has_scale {
                    g.cols[0] = [translation.x, translation.y, translation.z, 0.0];
                    g.cols[1] = [rotation.x, rotation.y, rotation.z, rotation.w];
                    g.cols[2] = binding.tint;
                    g.cols[3] = [scale.x, scale.y, scale.z, 0.0];
                } else {
                    g.cols[0] = [translation.x, translation.y, translation.z, 0.0];
                    g.cols[1] = [rotation.x, rotation.y, rotation.z, rotation.w];
                    g.cols[2] = [0.0, 0.0, 0.0, 0.0];
                    g.cols[3] = [0.0, 0.0, 0.0, 0.0];
                }
            }
        }
    }
}
