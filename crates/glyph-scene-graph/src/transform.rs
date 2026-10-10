//! The rows the tables hold, and the one composition rule.
//!
//! A node's LOCAL transform is a similarity: translation, rotation
//! (quaternion) and one uniform scale, 32 B. Similarities are closed under
//! composition, so a world transform is the same 32 B shape and inherits
//! cleanly down any depth. Per-axis scale is NOT inherited: it is a separate
//! leaf factor ([`PostScale`]) applied in the node's own frame before its
//! similarity — Unity Entities' `LocalTransform` (uniform) beside
//! `PostTransformMatrix` (anything else), for the same reason: a non-uniform
//! scale under a rotation is a shear, and a shear does not compose into a
//! similarity.
//!
//! The arithmetic here is written in the SAME order as `shaders/resolve.wgsl`
//! (the rotation is the cross-product sandwich `glyph_field.wgsl` already
//! uses), so the CPU reference and the GPU pass differ only by whatever the
//! GPU fuses into an fma. Under an identity parent both are exact: every
//! product with 1 and every sum with 0 returns its operand, so the transitional
//! group rows (every group a child of an identity root) come out bit for bit
//! as the rows they were (the one exception is the sign of a zero, `-0 + 0 =
//! +0`, which no golden frame can see).

use bytemuck::{Pod, Zeroable};

/// Translation, uniform scale and rotation: 32 B, `struct Similarity
/// { t_s: vec4<f32>, q: vec4<f32> }` in WGSL (scale rides in `t_s.w`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Similarity {
    pub translation: [f32; 3],
    pub scale: f32,
    /// x, y, z, w. Never normalised here: a caller's quaternion is composed
    /// as given, exactly as the group row's was drawn.
    pub rotation: [f32; 4],
}

impl Similarity {
    pub const IDENTITY: Self = Self { translation: [0.0; 3], scale: 1.0, rotation: [0.0, 0.0, 0.0, 1.0] };

    pub fn from_translation(t: [f32; 3]) -> Self {
        Self { translation: t, ..Self::IDENTITY }
    }

    /// `parent ∘ child`: the child's frame expressed in the parent's parent.
    pub fn compose(parent: &Self, child: &Self) -> Self {
        let st = scale3(child.translation, parent.scale);
        let rt = rotate(parent.rotation, st);
        Self {
            translation: add3(parent.translation, rt),
            scale: parent.scale * child.scale,
            rotation: quat_mul(parent.rotation, child.rotation),
        }
    }

    /// A world-space displacement as the displacement of a child of `self`:
    /// undo the uniform scale, then the rotation (the conjugate; `self` is
    /// assumed a unit quaternion here, as every rotation a verb writes is).
    pub fn world_delta_to_local(&self, d: [f32; 3]) -> [f32; 3] {
        let q = self.rotation;
        let conj = [-q[0], -q[1], -q[2], q[3]];
        let unscaled = [d[0] / self.scale, d[1] / self.scale, d[2] / self.scale];
        rotate(conj, unscaled)
    }
}

impl Default for Similarity {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The non-inherited per-axis scale, 16 B (`w` unused). Identity is 1, 1, 1.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct PostScale {
    pub xyz: [f32; 3],
    pub _pad: f32,
}

impl PostScale {
    pub const IDENTITY: Self = Self { xyz: [1.0; 3], _pad: 0.0 };
}

impl Default for PostScale {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// How a node looks, written by verbs and styling, never by a transform
/// writer: 32 B. `tint.w` is the node's alpha; alpha MULTIPLIES down the
/// tree (a hidden directory hides what is inside it), tint and blend are the
/// node's own. In WGSL `struct Appearance { tint: vec4<f32>, blend: f32,
/// flags: u32, _p0: u32, _p1: u32 }`. Kept in f32 rather than RGBA8 so the
/// transitional group rows (f32 columns) round-trip exactly.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Appearance {
    pub tint: [f32; 4],
    /// The group row's colourBlend: 0 multiplies the glyph colour by the
    /// tint, 1 replaces it.
    pub blend: f32,
    /// Reserved (visibility layers, LOD hints); written through, read by no
    /// shader yet.
    pub flags: u32,
    pub _pad: [u32; 2],
}

impl Appearance {
    pub const IDENTITY: Self = Self { tint: [1.0; 4], blend: 0.0, flags: 0, _pad: [0; 2] };
}

impl Default for Appearance {
    fn default() -> Self {
        Self::IDENTITY
    }
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale3(v: [f32; 3], s: f32) -> [f32; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// `v + 2·q.xyz × (q.xyz × v + q.w·v)`: the form `glyph_field.wgsl` rotates
/// with, so the resolve and the draw agree on what a quaternion means.
pub fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let c = cross(u, v);
    let qc = [c[0] + v[0] * q[3], c[1] + v[1] * q[3], c[2] + v[2] * q[3]];
    let c2 = cross(u, qc);
    [v[0] + 2.0 * c2[0], v[1] + 2.0 * c2[1], v[2] + 2.0 * c2[2]]
}

/// Hamilton product `a·b` (apply b, then a), WGSL's term order.
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let (av, bv) = ([a[0], a[1], a[2]], [b[0], b[1], b[2]]);
    let c = cross(av, bv);
    let dot = av[0] * bv[0] + av[1] * bv[1] + av[2] * bv[2];
    [
        a[3] * bv[0] + b[3] * av[0] + c[0],
        a[3] * bv[1] + b[3] * av[1] + c[1],
        a[3] * bv[2] + b[3] * av[2] + c[2],
        a[3] * b[3] - dot,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(s: &Similarity) -> [u32; 8] {
        bytemuck::cast(*s)
    }

    #[test]
    fn an_identity_parent_returns_the_child_bit_for_bit() {
        let child = Similarity {
            translation: [12.5, -167.8, 3.0e-7],
            scale: 0.0086,
            rotation: [0.1, -0.2, 0.3, 0.927_361_85],
        };
        assert_eq!(bits(&Similarity::compose(&Similarity::IDENTITY, &child)), bits(&child));
    }

    #[test]
    fn composition_is_associative_to_rounding() {
        let a = Similarity { translation: [1.0, 2.0, 3.0], scale: 2.0, rotation: [0.0, 0.0, 0.382_683_43, 0.923_879_5] };
        let b = Similarity { translation: [-4.0, 0.5, 1.0], scale: 0.5, rotation: [0.258_819, 0.0, 0.0, 0.965_925_8] };
        let c = Similarity { translation: [7.0, -1.0, 0.25], scale: 3.0, rotation: [0.0, 0.130_526_2, 0.0, 0.991_444_9] };
        let l = Similarity::compose(&Similarity::compose(&a, &b), &c);
        let r = Similarity::compose(&a, &Similarity::compose(&b, &c));
        for k in 0..3 {
            assert!((l.translation[k] - r.translation[k]).abs() < 1e-5, "t{k}: {l:?} vs {r:?}");
        }
        for k in 0..4 {
            assert!((l.rotation[k] - r.rotation[k]).abs() < 1e-6, "q{k}");
        }
        assert_eq!(l.scale, r.scale);
    }

    #[test]
    fn a_world_delta_lands_where_it_was_aimed() {
        // Under a scaled, rotated parent, moving a child by the converted
        // delta moves its world translation by the world delta.
        let parent = Similarity { translation: [5.0, 0.0, 0.0], scale: 4.0, rotation: [0.0, 0.0, 0.707_106_77, 0.707_106_77] };
        let child = Similarity::from_translation([1.0, 1.0, 0.0]);
        let d = [3.0, -2.0, 0.5];
        let local = parent.world_delta_to_local(d);
        let moved = Similarity { translation: add3(child.translation, local), ..child };
        let before = Similarity::compose(&parent, &child).translation;
        let after = Similarity::compose(&parent, &moved).translation;
        for k in 0..3 {
            assert!((after[k] - before[k] - d[k]).abs() < 1e-5, "axis {k}: {before:?} -> {after:?}");
        }
    }
}
