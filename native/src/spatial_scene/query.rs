use bevy_ecs::prelude::*;
use glam::Vec3;
use super::{SpatialScene, LocalBounds, GlobalTransform, SceneMeshKind, Visible};

impl SpatialScene {
    /// Compute world-space axis-aligned bounding box (AABB) for an entity and its subtree.
    pub fn world_bounds(&self, entity: Entity) -> Option<([f32; 3], [f32; 3])> {
        let gtf = self.world.get::<GlobalTransform>(entity)?;
        let mut world_min = Vec3::splat(f32::INFINITY);
        let mut world_max = Vec3::splat(f32::NEG_INFINITY);
        let mut has_bounds = false;

        let local_bounds = self.world.get::<LocalBounds>(entity).copied().or_else(|| {
            self.world.get::<SceneMeshKind>(entity).map(|mesh| match mesh {
                SceneMeshKind::Quad { size, origin } => LocalBounds {
                    min: [origin[0], origin[1], 0.0],
                    max: [origin[0] + size[0], origin[1] + size[1], 0.0],
                },
                SceneMeshKind::Box { extents } => {
                    let hx = extents[0] * 0.5;
                    let hy = extents[1] * 0.5;
                    let hz = extents[2] * 0.5;
                    LocalBounds {
                        min: [-hx, -hy, -hz],
                        max: [hx, hy, hz],
                    }
                }
            })
        });

        if let Some(b) = local_bounds {
            let corners = [
                Vec3::new(b.min[0], b.min[1], b.min[2]),
                Vec3::new(b.max[0], b.min[1], b.min[2]),
                Vec3::new(b.min[0], b.max[1], b.min[2]),
                Vec3::new(b.max[0], b.max[1], b.min[2]),
                Vec3::new(b.min[0], b.min[1], b.max[2]),
                Vec3::new(b.max[0], b.min[1], b.max[2]),
                Vec3::new(b.min[0], b.max[1], b.max[2]),
                Vec3::new(b.max[0], b.max[1], b.max[2]),
            ];
            for c in corners {
                let wp = gtf.transform_point(c);
                world_min = world_min.min(wp);
                world_max = world_max.max(wp);
            }
            has_bounds = true;
        }

        if let Some(children) = self.world.get::<bevy_ecs::hierarchy::Children>(entity) {
            for child in children.iter() {
                if let Some((c_min, c_max)) = self.world_bounds(child) {
                    world_min = world_min.min(Vec3::from_array(c_min));
                    world_max = world_max.max(Vec3::from_array(c_max));
                    has_bounds = true;
                }
            }
        }

        if has_bounds {
            Some((world_min.to_array(), world_max.to_array()))
        } else {
            None
        }
    }
    /// Raycast against all entities with `LocalBounds` using true Oriented Bounding Box (OBB)
    /// unprojection: unprojects the world ray into entity local space via `world_from_local.inverse()`.
    /// Returns `Some((Entity, world_hit_t))` for the nearest hit entity in front of the ray.
    pub fn raycast_obb(
        &mut self,
        ro: glam::DVec3,
        rd: glam::DVec3,
    ) -> Option<(Entity, f64)> {
        let mut best: Option<(Entity, f64)> = None;

        let mut query = self.world.query::<(
            Entity,
            &GlobalTransform,
            &LocalBounds,
            Option<&Visible>,
        )>();

        for (entity, gtf, bounds, vis) in query.iter(&self.world) {
            if let Some(v) = vis {
                if !v.0 {
                    continue;
                }
            }

            let affine = gtf.affine();
            let inv_affine = affine.inverse();

            // Transform world ray into local space
            let ro_f32 = Vec3::new(ro.x as f32, ro.y as f32, ro.z as f32);
            let rd_f32 = Vec3::new(rd.x as f32, rd.y as f32, rd.z as f32);

            let ro_local = inv_affine.transform_point3(ro_f32);
            let rd_local = inv_affine.transform_vector3(rd_f32);

            let ro_loc_d = glam::DVec3::new(ro_local.x as f64, ro_local.y as f64, ro_local.z as f64);
            let rd_loc_d = glam::DVec3::new(rd_local.x as f64, rd_local.y as f64, rd_local.z as f64);

            let b_min = glam::DVec3::new(bounds.min[0] as f64, bounds.min[1] as f64, bounds.min[2] as f64);
            let b_max = glam::DVec3::new(bounds.max[0] as f64, bounds.max[1] as f64, bounds.max[2] as f64);

            let min = b_min.min(b_max);
            let max = b_min.max(b_max);

            if let Some(t) = ray_aabb_d(ro_loc_d, rd_loc_d, min, max) {
                if best.is_none_or(|(_, bt)| t < bt) {
                    best = Some((entity, t));
                }
            }
        }

        best
    }
}

/// Ray-AABB intersection in double precision.
#[inline]
fn ray_aabb_d(
    ro: glam::DVec3,
    rd: glam::DVec3,
    min: glam::DVec3,
    max: glam::DVec3,
) -> Option<f64> {
    let mut tmin = f64::NEG_INFINITY;
    let mut tmax = f64::INFINITY;

    for i in 0..3 {
        let (origin, dir, bmin, bmax) = match i {
            0 => (ro.x, rd.x, min.x, max.x),
            1 => (ro.y, rd.y, min.y, max.y),
            _ => (ro.z, rd.z, min.z, max.z),
        };

        if dir.abs() < 1e-12 {
            if origin < bmin || origin > bmax {
                return None;
            }
        } else {
            let inv_d = 1.0 / dir;
            let mut t1 = (bmin - origin) * inv_d;
            let mut t2 = (bmax - origin) * inv_d;
            if t1 > t2 {
                core::mem::swap(&mut t1, &mut t2);
            }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmin > tmax {
                return None;
            }
        }
    }

    if tmax < 0.0 {
        return None;
    }

    Some(if tmin >= 0.0 { tmin } else { tmax })
}
