//! camera.rs — the camera as the GPU sees it: a view-projection, its six
//! frustum planes (Gribb–Hartmann, from the matrix rows), and the pixel scale
//! the LOD test needs; plus the CPU twin of the box test used by `--cull cpu`
//! and by the set comparison that checks the GPU cull.
//!
//! Two camera kinds share one uniform: a perspective camera looking straight
//! down -z at the text plane (the `--camera` presets), and the orthographic
//! fit the synthetic `--view`s have always used. `flags & 1` says which, so the
//! LOD test can divide by distance for one and not the other.

use bytemuck::{Pod, Zeroable};

pub const ROW_H: f32 = 1.2;
const FOV_Y: f32 = 45.0f32 * std::f32::consts::PI / 180.0;

/// Column-major 4x4, `m[col][row]`, as `mat4x4<f32>` is laid out in WGSL.
#[derive(Clone, Copy, Debug)]
pub struct Mat4(pub [[f32; 4]; 4]);

impl Mat4 {
    pub fn identity() -> Mat4 {
        Mat4([[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]])
    }
    /// `self * o`.
    pub fn mul(&self, o: &Mat4) -> Mat4 {
        let mut r = [[0f32; 4]; 4];
        for c in 0..4 { for row in 0..4 { r[c][row] = (0..4).map(|k| self.0[k][row] * o.0[c][k]).sum() } }
        Mat4(r)
    }
    /// Right-handed, looking down -z, depth 0..1 (wgpu's convention; glam's `perspective_rh`).
    pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
        let f = 1.0 / (fov_y * 0.5).tan();
        let mut m = [[0f32; 4]; 4];
        m[0][0] = f / aspect;
        m[1][1] = f;
        m[2][2] = far / (near - far);
        m[3][2] = near * far / (near - far);
        m[2][3] = -1.0;
        Mat4(m)
    }
    /// Orthographic, depth 0..1 (glam's `orthographic_rh`).
    pub fn ortho(l: f32, r: f32, b: f32, t: f32, near: f32, far: f32) -> Mat4 {
        let mut m = Mat4::identity().0;
        m[0][0] = 2.0 / (r - l);
        m[1][1] = 2.0 / (t - b);
        m[2][2] = 1.0 / (near - far);
        m[3][0] = -(r + l) / (r - l);
        m[3][1] = -(t + b) / (t - b);
        m[3][2] = near / (near - far);
        Mat4(m)
    }
    pub fn translation(x: f32, y: f32, z: f32) -> Mat4 {
        let mut m = Mat4::identity().0;
        m[3] = [x, y, z, 1.0];
        Mat4(m)
    }
    fn row(&self, r: usize) -> [f32; 4] { [self.0[0][r], self.0[1][r], self.0[2][r], self.0[3][r]] }
}

/// The uniform both cull kernels and the draw read. 208 B.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct CameraU {
    pub vp: [[f32; 4]; 4],
    pub planes: [[f32; 4]; 6],
    pub viewport: [f32; 2],
    pub lod_px: f32,
    /// Projected height in pixels of one world unit at distance 1 (perspective) or flat (ortho).
    pub px_scale: f32,
    pub eye: [f32; 3],
    pub tint: u32,
    pub default_color: u32,
    /// bit 0: orthographic.
    pub flags: u32,
    pub _pad: [u32; 2],
}

#[derive(Clone, Copy, Debug)]
pub struct Camera { pub u: CameraU, pub name: &'static str, pub dist: f32 }

fn planes_of(vp: &Mat4) -> [[f32; 4]; 6] {
    let (r0, r1, r2, r3) = (vp.row(0), vp.row(1), vp.row(2), vp.row(3));
    let add = |a: [f32; 4], b: [f32; 4], s: f32| [a[0] + s * b[0], a[1] + s * b[1], a[2] + s * b[2], a[3] + s * b[3]];
    let raw = [add(r3, r0, 1.0), add(r3, r0, -1.0), add(r3, r1, 1.0), add(r3, r1, -1.0), r2, add(r3, r2, -1.0)];
    let mut out = [[0f32; 4]; 6];
    for (o, p) in out.iter_mut().zip(raw) {
        let n = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-20);
        *o = [p[0] / n, p[1] / n, p[2] / n, p[3] / n];
    }
    out
}

impl Camera {
    /// Perspective camera above `(cx, cy)` at height `dist`, looking down -z.
    pub fn perspective(name: &'static str, cx: f32, cy: f32, dist: f32, target: (u32, u32), lod_px: f32, tint: bool, default_color: u32) -> Camera {
        let aspect = target.0 as f32 / target.1 as f32;
        // Wrap depth can reach thousands of units below the plane (a 10 MiB line); keep it in the frustum.
        let (near, far) = (dist * 0.01, dist * 4.0 + 20_000.0);
        let p = Mat4::perspective(FOV_Y, aspect, near, far);
        let vp = p.mul(&Mat4::translation(-cx, -cy, -dist));
        let px_scale = p.0[1][1] * target.1 as f32 * 0.5;
        Camera { u: CameraU { vp: vp.0, planes: planes_of(&vp), viewport: [target.0 as f32, target.1 as f32], lod_px, px_scale, eye: [cx, cy, dist], tint: u32::from(tint), default_color, flags: 0, _pad: [0; 2] }, name, dist }
    }

    /// The perspective camera that frames a world box with a 4% margin.
    pub fn frame_box(name: &'static str, x0: f32, x1: f32, y0: f32, y1: f32, target: (u32, u32), lod_px: f32, tint: bool, default_color: u32) -> Camera {
        let aspect = target.0 as f32 / target.1 as f32;
        let t = (FOV_Y * 0.5).tan();
        let (hw, hh) = (((x1 - x0) * 0.5).max(1e-3), ((y1 - y0) * 0.5).max(1e-3));
        let dist = (hh / t).max(hw / (t * aspect)) / 0.96;
        Camera::perspective(name, (x0 + x1) * 0.5, (y0 + y1) * 0.5, dist, target, lod_px, tint, default_color)
    }

    /// The perspective camera over `(cx, cy)` at which one row is `row_px` pixels tall.
    pub fn at_row_px(name: &'static str, cx: f32, cy: f32, row_px: f32, target: (u32, u32), lod_px: f32, tint: bool, default_color: u32) -> Camera {
        let f = 1.0 / (FOV_Y * 0.5).tan();
        let px_scale = f * target.1 as f32 * 0.5;
        Camera::perspective(name, cx, cy, ROW_H * px_scale / row_px, target, lod_px, tint, default_color)
    }

    /// The orthographic fit the synthetic views use: a world box with a 4% margin, aspect preserved.
    pub fn ortho_fit(x0: f32, x1: f32, y0: f32, y1: f32, target: (u32, u32), tint: bool, default_color: u32) -> Camera {
        let (w, h) = ((x1 - x0).max(1e-3), (y1 - y0).max(1e-3));
        let px_per_unit = (target.0 as f32 * 0.96 / w).min(target.1 as f32 * 0.96 / h);
        let (hw, hh) = (target.0 as f32 / px_per_unit * 0.5, target.1 as f32 / px_per_unit * 0.5);
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let vp = Mat4::ortho(cx - hw, cx + hw, cy - hh, cy + hh, -1.0, 100_000.0);
        Camera { u: CameraU { vp: vp.0, planes: planes_of(&vp), viewport: [target.0 as f32, target.1 as f32], lod_px: 0.0, px_scale: px_per_unit, eye: [cx, cy, 1.0], tint: u32::from(tint), default_color, flags: 1, _pad: [0; 2] }, name: "ortho", dist: 1.0 }
    }

    /// Projected height of one row in pixels for a box whose nearest face is at z = 0 — the LOD test.
    pub fn row_px(&self) -> f32 {
        if self.u.flags & 1 != 0 { ROW_H * self.u.px_scale } else { ROW_H * self.u.px_scale / self.u.eye[2].max(1e-3) }
    }

    /// The positive-vertex test: the box is outside when some plane has its nearest corner behind it.
    pub fn box_visible(&self, mn: [f32; 3], mx: [f32; 3]) -> bool {
        for p in &self.u.planes {
            let v = [if p[0] >= 0.0 { mx[0] } else { mn[0] }, if p[1] >= 0.0 { mx[1] } else { mn[1] }, if p[2] >= 0.0 { mx[2] } else { mn[2] }];
            if p[0] * v[0] + p[1] * v[1] + p[2] * v[2] + p[3] < 0.0 { return false }
        }
        true
    }
}
