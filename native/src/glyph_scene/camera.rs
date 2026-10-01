//! The glyph scene's camera cluster: `CameraMode`, the fly camera, and the
//! per-frame camera products. Extracted from `glyph_scene.rs` in the
//! 2026-09 code-shape refactor — a pure move; the `pub(super)` markings
//! stand in for the same-module privacy these items had inside that file
//! (the scene drives the camera's fields directly, e.g. click-to-fly).

use glam::{Mat4, Vec3};

/// Vertical field of view shared by every glyph-scene camera mode.
/// (pub since Stage K (K5): the Debug panel's file browser mirrors the
/// Front-camera framing formula for click-to-fly navigation.)
pub const FOV_Y: f32 = 40f32;

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
        Self {
            eye,
            yaw: 0.0,
            pitch: 0.0,
            speed: fit * 0.4,
            speed_min: fit * 0.005,
            speed_max: fit * 8.0,
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

    pub(super) fn on_key(&mut self, code: winit::keyboard::KeyCode, pressed: bool) {
        use winit::keyboard::KeyCode as K;
        let bit = match code {
            K::KeyW => FLY_FWD,
            K::KeyS => FLY_BACK,
            K::KeyA => FLY_LEFT,
            K::KeyD => FLY_RIGHT,
            K::KeyE | K::KeyR => FLY_UP,
            K::KeyQ | K::KeyF => FLY_DOWN,
            _ => return,
        };
        if pressed {
            self.keys |= bit;
        } else {
            self.keys &= !bit;
        }
    }

    pub(super) fn on_look(&mut self, dx: f32, dy: f32) {
        const SENS: f32 = 0.0022;
        // yaw += : mouse-right rotates the view toward +X (camera right).
        // (was yaw -=, which swung the view left — inverted horizontal look)
        self.yaw += dx * SENS;
        self.pitch = (self.pitch - dy * SENS).clamp(-1.55, 1.55);
    }

    pub(super) fn on_scroll(&mut self, lines: f32) {
        self.speed = (self.speed * 1.15f32.powf(lines)).clamp(self.speed_min, self.speed_max);
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
        // Exponential approach: ~63% of the way to target every 100 ms.
        let k = 1.0 - (-10.0 * dt).exp();
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
