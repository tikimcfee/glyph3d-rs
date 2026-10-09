//! The glyph scene's camera cluster: `CameraMode`, the fly camera, and the
//! per-frame camera products. Extracted from `glyph_scene.rs` in the
//! 2026-09 code-shape refactor — a pure move; the `pub(super)` markings
//! stand in for the same-module privacy these items had inside that file
//! (the scene drives the camera's fields directly, e.g. click-to-fly).

use glam::{Mat4, Vec3};

/// Vertical field of view shared by every glyph-scene camera mode, degrees
/// (`[camera] fov_y_deg`).
pub fn fov_y_deg() -> f32 {
    crate::config::settings().camera.fov_y_deg
}

/// Front-camera fit distance for a vertical half-extent: the distance at
/// which it fills the view, with the configured margin and pad. The single
/// form of the framing formula (the Debug panel's click-to-fly mirrors it).
pub fn fit_distance(half_h_needed: f32) -> f32 {
    let c = &crate::config::settings().camera;
    half_h_needed / (c.fov_y_deg.to_radians() * 0.5).tan() * c.frame_margin + c.frame_pad
}

/// Camera behavior. `Front` faces the text plane dead-on at a fit distance
/// (offscreen verification); `Orbit` slowly circles the block (legacy
/// windowed demo); `Fly` is the Stage F free camera driven by windowed input.
/// `zoom` multiplies magnification (2.0 = twice as close).
#[derive(Clone, Copy)]
pub enum CameraMode {
    Front { zoom: f32 },
    Orbit,
    Fly,
}

/// Stage F — fly camera state: WASD strafe/forward, E|R up, Q|F down,
/// mouse-look (yaw/pitch), scroll = persistent speed multiplier, exponential
/// velocity damping. yaw = 0 looks down −Z (the text plane faces +Z).
#[derive(Clone, Copy)]
pub struct FlyCamera {
    pub eye: Vec3,
    pub(super) yaw: f32,
    pub(super) pitch: f32,
    speed: f32,
    speed_min: f32,
    speed_max: f32,
    pub(super) vel: Vec3,
    pub(super) keys: u8, // FWD|BACK|LEFT|RIGHT|UP|DOWN
}

const FLY_FWD: u8 = 1;
const FLY_BACK: u8 = 2;
const FLY_LEFT: u8 = 4;
const FLY_RIGHT: u8 = 8;
const FLY_UP: u8 = 16;
const FLY_DOWN: u8 = 32;

impl FlyCamera {
    pub(super) fn new(eye: Vec3, fit: f32) -> Self {
        let c = &crate::config::settings().camera;
        Self {
            eye,
            yaw: 0.0,
            pitch: 0.0,
            speed: fit * c.fly_speed,
            speed_min: fit * c.fly_speed_min,
            speed_max: fit * c.fly_speed_max,
            vel: Vec3::ZERO,
            keys: 0,
        }
    }

    /// View direction from yaw/pitch: yaw 0 = −Z, right-handed, Y up.
    pub(super) fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(sy * cp, sp, -cy * cp)
    }

    pub(super) fn on_key(&mut self, code: winit::keyboard::KeyCode, pressed: bool) -> bool {
        use winit::keyboard::KeyCode as K;
        let bit = match code {
            K::KeyW => FLY_FWD,
            K::KeyS => FLY_BACK,
            K::KeyA => FLY_LEFT,
            K::KeyD => FLY_RIGHT,
            K::KeyE | K::KeyR => FLY_UP,
            K::KeyQ | K::KeyF => FLY_DOWN,
            _ => return false,
        };
        if pressed {
            self.keys |= bit;
        } else {
            self.keys &= !bit;
        }
        true
    }

    pub(super) fn on_look(&mut self, dx: f32, dy: f32) {
        let sens = crate::config::settings().camera.look_sensitivity;
        // yaw += : mouse-right rotates the view toward +X (camera right).
        // (was yaw -=, which swung the view left — inverted horizontal look)
        self.yaw += dx * sens;
        self.pitch = (self.pitch - dy * sens).clamp(-1.55, 1.55);
    }

    pub(super) fn on_scroll(&mut self, lines: f32) {
        let step = crate::config::settings().camera.fly_scroll_step;
        self.speed = (self.speed * step.powf(lines)).clamp(self.speed_min, self.speed_max);
    }

    pub(super) fn tick(&mut self, dt: f32) {
        let fwd = self.forward();
        let right = Vec3::new(self.yaw.cos(), 0.0, self.yaw.sin());
        let mut dir = Vec3::ZERO;
        if self.keys & FLY_FWD != 0 {
            dir += fwd;
        }
        if self.keys & FLY_BACK != 0 {
            dir -= fwd;
        }
        if self.keys & FLY_RIGHT != 0 {
            dir += right;
        }
        if self.keys & FLY_LEFT != 0 {
            dir -= right;
        }
        if self.keys & FLY_UP != 0 {
            dir += Vec3::Y;
        }
        if self.keys & FLY_DOWN != 0 {
            dir -= Vec3::Y;
        }
        let target = if dir.length_squared() > 0.0 {
            dir.normalize() * self.speed
        } else {
            Vec3::ZERO
        };
        // Exponential approach: ~63% of the way to target every 1/rate s.
        let k = 1.0 - (-crate::config::settings().camera.fly_damping * dt).exp();
        self.vel += (target - self.vel) * k;
        self.eye += self.vel * dt;
    }
}

/// One frame's camera products: the view-proj (written to the camera uniform)
/// plus the eye position (consumed by the cull pass for the LOD metric).
pub(super) struct CamFrame {
    pub(super) view_proj: Mat4,
    pub(super) eye: Vec3,
}
