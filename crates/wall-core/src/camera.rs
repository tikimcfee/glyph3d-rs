//! Orthographic camera pose, coordinate conversions, zoom-at-point, wheel-zoom
//! glide, and framing ease (ports the pure model of `wall/WallCamera.js`).
//!
//! Decides how the wall's 2D orthographic camera maps between world space
//! ([`crate::geom::Point`] / [`crate::geom::Rect`], **y up**) and viewport or
//! client pixels (**y down**, `zoom` = pixels per world unit), how direct and
//! wheel-notched zooms hold the world point under the pointer fixed, how
//! framing targets fit a world rectangle inside the viewport, and how framing
//! eases and wheel glides step exponentially in log-zoom and snap cleanly onto
//! their targets.

use crate::geom::{Point, Rect};

/// Exponential ease rate (`1/s`) for framing animations (`ease_to`).
pub const EASE_RATE: f64 = 14.0;

/// Minimum allowed zoom in screen pixels per world unit.
pub const ZOOM_MIN: f64 = 0.05;

/// Maximum allowed zoom in screen pixels per world unit.
pub const ZOOM_MAX: f64 = 200.0;

/// Default viewport fill fraction used when framing a rectangle (`0.94`).
pub const DEFAULT_FIT_MARGIN: f64 = 0.94;

/// Parameters governing eased wheel-notch zooming (`WHEEL_ZOOM` in `WallCamera.js`).
///
/// Every `pixels_per_notch` of wheel delta zooms by `step_factor` about the
/// pointer and eases exponentially in log-zoom at `settle_rate` (`1/s`,
/// reaching ~95% in `3 / settle_rate` seconds) with no overshoot or inertia.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WheelZoomConfig {
    pub step_factor: f64,
    pub pixels_per_notch: f64,
    pub settle_rate: f64,
}

/// Default wheel-zoom parameters (`{ stepFactor: 1.12, pixelsPerNotch: 100, settleRate: 24 }`).
pub const WHEEL_ZOOM: WheelZoomConfig = WheelZoomConfig {
    step_factor: 1.12,
    pixels_per_notch: 100.0,
    settle_rate: 24.0,
};

impl Default for WheelZoomConfig {
    fn default() -> WheelZoomConfig {
        WHEEL_ZOOM
    }
}

/// Orthographic camera pose: world-space centre `(x, y)` (y up) and `zoom` in
/// screen pixels per world unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraPose {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl CameraPose {
    pub const fn new(x: f64, y: f64, zoom: f64) -> CameraPose {
        CameraPose { x, y, zoom }
    }

    /// Convert a viewport-local screen point `(0..view_w, 0..view_h)` (origin
    /// top-left, y down) to world coordinates (y up).
    pub fn to_world(self, screen: Point, view_w: f64, view_h: f64) -> Point {
        Point {
            x: self.x + (screen.x - view_w * 0.5) / self.zoom,
            y: self.y - (screen.y - view_h * 0.5) / self.zoom,
        }
    }

    /// Convert a world point (y up) to viewport-local screen pixels
    /// `(0..view_w, 0..view_h)` (origin top-left, y down).
    pub fn to_screen(self, world: Point, view_w: f64, view_h: f64) -> Point {
        Point {
            x: view_w * 0.5 + (world.x - self.x) * self.zoom,
            y: view_h * 0.5 - (world.y - self.y) * self.zoom,
        }
    }

    /// Project a world rectangle (y up) into a viewport-local [`ScreenBox`]
    /// (origin top-left, y down).
    pub fn to_screen_box(self, rect: Rect, view_w: f64, view_h: f64) -> ScreenBox {
        ScreenBox {
            left: view_w * 0.5 + (rect.min_x - self.x) * self.zoom,
            right: view_w * 0.5 + (rect.max_x - self.x) * self.zoom,
            top: view_h * 0.5 - (rect.max_y - self.y) * self.zoom,
            bottom: view_h * 0.5 - (rect.min_y - self.y) * self.zoom,
        }
    }

    /// World-space rectangle visible across a viewport of `(view_w, view_h)` pixels.
    pub fn visible_rect(self, view_w: f64, view_h: f64) -> Rect {
        let half_w = (view_w * 0.5) / self.zoom;
        let half_h = (view_h * 0.5) / self.zoom;
        Rect {
            min_x: self.x - half_w,
            min_y: self.y - half_h,
            max_x: self.x + half_w,
            max_y: self.y + half_h,
        }
    }
}

impl Default for CameraPose {
    fn default() -> CameraPose {
        CameraPose {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

/// Client-space bounding box of the canvas viewport in pixels (origin top-left,
/// y down), standing in for `HTMLCanvasElement.getBoundingClientRect()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportBox {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl ViewportBox {
    pub const fn new(left: f64, top: f64, width: f64, height: f64) -> ViewportBox {
        ViewportBox {
            left,
            top,
            width,
            height,
        }
    }

    /// Viewport anchored at `(0, 0)` of size `(width, height)` pixels.
    pub const fn from_size(width: f64, height: f64) -> ViewportBox {
        ViewportBox {
            left: 0.0,
            top: 0.0,
            width,
            height,
        }
    }

    /// Convert a client-space point into viewport-local coordinates.
    pub fn to_local(self, client: Point) -> Point {
        Point {
            x: client.x - self.left,
            y: client.y - self.top,
        }
    }

    /// Convert a viewport-local point into client-space coordinates.
    pub fn to_client(self, local: Point) -> Point {
        Point {
            x: local.x + self.left,
            y: local.y + self.top,
        }
    }
}

impl Default for ViewportBox {
    fn default() -> ViewportBox {
        ViewportBox::from_size(1.0, 1.0)
    }
}

/// Axis-aligned rectangle in viewport-relative screen pixels (y down), as
/// returned by [`WallCamera::to_screen_box`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenBox {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl ScreenBox {
    pub fn width(&self) -> f64 {
        self.right - self.left
    }

    pub fn height(&self) -> f64 {
        self.bottom - self.top
    }
}

/// In-flight wheel-zoom glide: target log-zoom and the pointer's client point
/// and world anchor held fixed beneath it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct WheelGlide {
    ln_zoom: f64,
    client: Point,
    world: Point,
}

/// Private copy of `wallLayout.fitRect` so `camera.rs` stays independent of
/// `layout.rs` while parallel work packages land.
fn fit_rect(rect: Rect, view_w: f64, view_h: f64, margin: f64, max_zoom: f64) -> CameraPose {
    let w = (rect.max_x - rect.min_x).max(1e-6);
    let h = (rect.max_y - rect.min_y).max(1e-6);
    CameraPose {
        x: (rect.min_x + rect.max_x) * 0.5,
        y: (rect.min_y + rect.max_y) * 0.5,
        zoom: ((view_w / w).min(view_h / h) * margin).min(max_zoom),
    }
}

/// Pure state machine for the wall's orthographic camera (`WallCamera.js`).
#[derive(Clone, Debug, PartialEq)]
pub struct WallCamera {
    pose: CameraPose,
    bounds: ViewportBox,
    target: Option<CameraPose>,
    wheel_zoom: WheelZoomConfig,
    wheel: Option<WheelGlide>,
}

impl WallCamera {
    /// Create a camera at `pose` over a `(view_w, view_h)` viewport at `(0, 0)`.
    pub fn new(pose: CameraPose, view_w: f64, view_h: f64) -> WallCamera {
        WallCamera {
            pose,
            bounds: ViewportBox::from_size(view_w, view_h),
            target: None,
            wheel_zoom: WHEEL_ZOOM,
            wheel: None,
        }
    }

    /// Create a camera with custom [`WheelZoomConfig`] parameters.
    pub fn with_wheel_zoom(
        pose: CameraPose,
        view_w: f64,
        view_h: f64,
        wheel_zoom: WheelZoomConfig,
    ) -> WallCamera {
        WallCamera {
            wheel_zoom,
            ..WallCamera::new(pose, view_w, view_h)
        }
    }

    /// Current camera pose (world centre `x, y` and `zoom`).
    pub const fn pose(&self) -> CameraPose {
        self.pose
    }

    /// Set the camera pose directly, cancelling any active ease or wheel glide.
    pub fn set_pose(&mut self, pose: CameraPose) {
        self.target = None;
        self.wheel = None;
        self.pose = pose;
    }

    /// Current zoom in screen pixels per world unit.
    pub const fn zoom(&self) -> f64 {
        self.pose.zoom
    }

    /// True while the camera is animating on its own (an ease target or a wheel glide).
    pub const fn moving(&self) -> bool {
        self.target.is_some() || self.wheel.is_some()
    }

    /// Active framing target being eased toward, if any.
    pub const fn target(&self) -> Option<CameraPose> {
        self.target
    }

    /// Current wheel-zoom configuration.
    pub const fn wheel_zoom_config(&self) -> WheelZoomConfig {
        self.wheel_zoom
    }

    /// Update the wheel-zoom configuration.
    pub fn set_wheel_zoom_config(&mut self, wheel_zoom: WheelZoomConfig) {
        self.wheel_zoom = wheel_zoom;
    }

    /// Viewport `(width, height)` in pixels.
    pub const fn view(&self) -> (f64, f64) {
        (self.bounds.width, self.bounds.height)
    }

    /// Client-space viewport box (`left`, `top`, `width`, `height`).
    pub const fn bounds(&self) -> ViewportBox {
        self.bounds
    }

    /// Update viewport `(width, height)` in pixels, keeping `(left, top)` unchanged.
    pub fn set_view(&mut self, width: f64, height: f64) {
        self.bounds.width = width;
        self.bounds.height = height;
    }

    /// Update the full client-space [`ViewportBox`] (standing in for `measure()`).
    pub fn set_bounds(&mut self, bounds: ViewportBox) {
        self.bounds = bounds;
    }

    /// Map a client-space point (or viewport-local point when `bounds.left == 0`
    /// and `bounds.top == 0`) to world space (y up).
    pub fn to_world(&self, client: Point) -> Point {
        let local = self.bounds.to_local(client);
        self.pose
            .to_world(local, self.bounds.width, self.bounds.height)
    }

    /// Map a world point (y up) to viewport-local screen pixels `(0..width, 0..height)`.
    pub fn to_screen(&self, world: Point) -> Point {
        self.pose
            .to_screen(world, self.bounds.width, self.bounds.height)
    }

    /// Map a world point (y up) to client-space pixels (including `bounds.left` and `bounds.top`).
    pub fn to_client(&self, world: Point) -> Point {
        self.bounds.to_client(self.to_screen(world))
    }

    /// Map a world rectangle (y up) to a viewport-relative [`ScreenBox`] (y down).
    pub fn to_screen_box(&self, rect: Rect) -> ScreenBox {
        self.pose
            .to_screen_box(rect, self.bounds.width, self.bounds.height)
    }

    /// World rectangle currently visible inside the viewport.
    pub fn visible_rect(&self) -> Rect {
        self.pose.visible_rect(self.bounds.width, self.bounds.height)
    }

    /// Pan the camera by a screen-space delta `(dx_px, dy_px)` in pixels.
    ///
    /// Cancels any framing ease. If a wheel glide is in flight, its held world
    /// anchor shifts by the same world delta so the glide continues smoothly.
    pub fn pan_by(&mut self, dx_px: f64, dy_px: f64) {
        self.target = None;
        let dx = dx_px / self.pose.zoom;
        let dy = -dy_px / self.pose.zoom;
        self.pose.x += dx;
        self.pose.y += dy;
        if let Some(glide) = &mut self.wheel {
            glide.world.x += dx;
            glide.world.y += dy;
        }
    }

    /// Place the camera centre directly at `(x, y)` in world space, cancelling
    /// any active ease or wheel glide.
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.target = None;
        self.wheel = None;
        self.pose.x = x;
        self.pose.y = y;
    }

    /// Zoom immediately by `factor` about client point `client`, clamping zoom
    /// to `[ZOOM_MIN, ZOOM_MAX]` and shifting the camera centre so the world
    /// point under `client` remains fixed.
    pub fn zoom_at(&mut self, client: Point, factor: f64) {
        self.target = None;
        self.wheel = None;
        let before = self.to_world(client);
        self.pose.zoom = (self.pose.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        let after = self.to_world(client);
        self.pose.x += before.x - after.x;
        self.pose.y += before.y - after.y;
    }

    /// Start or extend an eased wheel-zoom glide about `client`.
    ///
    /// Positive `delta_px` (scrolling away / down) zooms out; negative zooms in.
    /// Notches arriving mid-glide accumulate onto the glide's landing `ln_zoom`
    /// rather than its instantaneous zoom, while refreshing the pointer's anchor.
    pub fn wheel_zoom_at(&mut self, client: Point, delta_px: f64) {
        self.target = None;
        let from = match &self.wheel {
            Some(glide) => glide.ln_zoom,
            None => self.pose.zoom.ln(),
        };
        let step_ln =
            self.wheel_zoom.step_factor.ln() * (delta_px / self.wheel_zoom.pixels_per_notch);
        let ln_zoom = (from - step_ln).clamp(ZOOM_MIN.ln(), ZOOM_MAX.ln());
        let world = self.to_world(client);
        self.wheel = Some(WheelGlide {
            ln_zoom,
            client,
            world,
        });
    }

    /// Compute the [`CameraPose`] that frames `rect` inside the current
    /// viewport with fill fraction `margin` (e.g. [`DEFAULT_FIT_MARGIN`]) and
    /// zoom ceiling `max_zoom` (`f64::INFINITY` for uncapped).
    pub fn fit_target(&self, rect: Rect, margin: f64, max_zoom: f64) -> CameraPose {
        fit_rect(rect, self.bounds.width, self.bounds.height, margin, max_zoom)
    }

    /// Begin easing exponentially toward `target`, cancelling any wheel glide.
    pub fn ease_to(&mut self, target: CameraPose) {
        self.wheel = None;
        self.target = Some(target);
    }

    /// True when the camera already sits within ~4 px and 2% zoom of `target`.
    pub fn is_at(&self, target: CameraPose) -> bool {
        (self.pose.zoom / target.zoom - 1.0).abs() < 0.02
            && (self.pose.x - target.x).hypot(self.pose.y - target.y) * self.pose.zoom < 4.0
    }

    /// Advance any active wheel glide or framing ease by `dt` seconds.
    ///
    /// Both animations interpolate zoom in log space and snap bit-exact onto
    /// their destination once within landing tolerance, clearing the animation.
    pub fn step(&mut self, dt: f64) {
        if self.wheel.is_some() {
            self.step_wheel(dt);
        }
        let Some(target) = self.target else {
            return;
        };
        let k = 1.0 - (-dt * EASE_RATE).exp();
        self.pose.x += (target.x - self.pose.x) * k;
        self.pose.y += (target.y - self.pose.y) * k;
        let ln_current = self.pose.zoom.ln();
        self.pose.zoom = (ln_current + (target.zoom.ln() - ln_current) * k).exp();

        let remaining_px = (target.x - self.pose.x).hypot(target.y - self.pose.y) * self.pose.zoom;
        if remaining_px < 0.5 && (target.zoom / self.pose.zoom - 1.0).abs() < 1e-3 {
            self.pose = target;
            self.target = None;
        }
    }

    fn step_wheel(&mut self, dt: f64) {
        let Some(glide) = self.wheel else {
            return;
        };
        let ln_current = self.pose.zoom.ln();
        let k = 1.0 - (-dt * self.wheel_zoom.settle_rate).exp();
        let landed = (glide.ln_zoom - ln_current).abs() < 1e-3;
        self.pose.zoom = if landed {
            glide.ln_zoom.exp()
        } else {
            (ln_current + (glide.ln_zoom - ln_current) * k).exp()
        };
        let at = self.to_world(glide.client);
        self.pose.x += glide.world.x - at.x;
        self.pose.y += glide.world.y - at.y;
        if landed {
            self.wheel = None;
        }
    }
}

impl Default for WallCamera {
    fn default() -> WallCamera {
        WallCamera::new(CameraPose::default(), 1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_point_close(actual: Point, expected: Point, tol: f64) {
        let dx = (actual.x - expected.x).abs();
        let dy = (actual.y - expected.y).abs();
        assert!(
            dx <= tol && dy <= tol,
            "expected {actual:?} to match {expected:?} within {tol} (dx={dx}, dy={dy})"
        );
    }

    #[test]
    fn fit_target_centers_and_fits_tighter_axis_with_optional_ceiling() {
        let cam = WallCamera::new(CameraPose::default(), 1000.0, 1000.0);
        let small = Rect::from_top_left(0.0, 0.0, 200.0, 100.0);

        assert_eq!(
            cam.fit_target(small, 1.0, f64::INFINITY),
            CameraPose::new(100.0, -50.0, 5.0)
        );
        assert_eq!(
            cam.fit_target(small, 1.0, 3.0),
            CameraPose::new(100.0, -50.0, 3.0)
        );
        let large = Rect::from_top_left(0.0, 0.0, 2000.0, 1000.0);
        assert_eq!(cam.fit_target(large, 1.0, 3.0).zoom, 0.5);
        assert!(
            (cam.fit_target(small, DEFAULT_FIT_MARGIN, f64::INFINITY).zoom - 5.0 * 0.94).abs()
                < 1e-12
        );
    }

    #[test]
    fn world_and_screen_conversions_round_trip_and_invert_y() {
        let mut cam = WallCamera::new(CameraPose::new(42.5, -18.25, 3.5), 1280.0, 800.0);
        let samples = [
            Point::new(0.0, 0.0),
            Point::new(42.5, -18.25),
            Point::new(-250.75, 190.125),
            Point::new(999.0, -432.5),
        ];
        for world in samples {
            let screen = cam.to_screen(world);
            let back = cam.to_world(screen);
            assert_point_close(back, world, 1e-11);
        }

        // Camera centre maps to viewport centre; +1 world y maps -zoom pixels in screen y.
        assert_point_close(
            cam.to_screen(Point::new(42.5, -18.25)),
            Point::new(640.0, 400.0),
            1e-12,
        );
        assert_point_close(
            cam.to_screen(Point::new(43.5, -17.25)),
            Point::new(643.5, 396.5),
            1e-12,
        );

        // ScreenBox corners match projected min/max corners (top uses max_y, bottom uses min_y).
        let world_rect = Rect::new(10.0, -40.0, 90.0, 20.0);
        let sbox = cam.to_screen_box(world_rect);
        let top_left = cam.to_screen(Point::new(world_rect.min_x, world_rect.max_y));
        let bottom_right = cam.to_screen(Point::new(world_rect.max_x, world_rect.min_y));
        assert!((sbox.left - top_left.x).abs() < 1e-12);
        assert!((sbox.top - top_left.y).abs() < 1e-12);
        assert!((sbox.right - bottom_right.x).abs() < 1e-12);
        assert!((sbox.bottom - bottom_right.y).abs() < 1e-12);

        // Non-zero client offset (standing in for getBoundingClientRect).
        cam.set_bounds(ViewportBox::new(120.0, 64.0, 1280.0, 800.0));
        for world in samples {
            let client = cam.to_client(world);
            let back = cam.to_world(client);
            assert_point_close(back, world, 1e-11);
        }
    }

    #[test]
    fn zoom_at_leaves_world_point_under_pointer_fixed_across_factors_and_clamps() {
        let mut cam = WallCamera::new(CameraPose::new(-15.0, 30.0, 1.8), 1440.0, 900.0);
        cam.set_bounds(ViewportBox::new(40.0, 25.0, 1440.0, 900.0));
        let pointer = Point::new(310.0, 685.0);
        let anchor = cam.to_world(pointer);

        for factor in [1.25, 2.7, 0.35, 1e6, 1e-9, 8.0] {
            cam.zoom_at(pointer, factor);
            assert!(cam.zoom() >= ZOOM_MIN && cam.zoom() <= ZOOM_MAX);
            assert_point_close(cam.to_world(pointer), anchor, 1e-10);
        }
    }

    #[test]
    fn wheel_zoom_eases_in_log_space_accumulates_notches_and_holds_pointer_fixed() {
        assert_eq!(WHEEL_ZOOM.step_factor, 1.12);
        assert_eq!(WHEEL_ZOOM.pixels_per_notch, 100.0);
        assert_eq!(WHEEL_ZOOM.settle_rate, 24.0);

        let mut cam = WallCamera::new(CameraPose::new(20.0, -10.0, 2.0), 1200.0, 800.0);
        let pointer = Point::new(275.0, 620.0);
        let anchor = cam.to_world(pointer);

        // One notch in (negative delta_px zooms in by step_factor).
        cam.wheel_zoom_at(pointer, -100.0);
        assert!(cam.moving());

        for _ in 0..3 {
            let prev_zoom = cam.zoom();
            cam.step(1.0 / 60.0);
            assert!(cam.zoom() > prev_zoom && cam.zoom() <= 2.0 * 1.12);
            assert_point_close(cam.to_world(pointer), anchor, 1e-11);
        }

        // Second notch mid-glide adds to the landing target, not the mid-glide zoom.
        assert!(cam.moving());
        cam.wheel_zoom_at(pointer, -100.0);

        let mut steps = 0;
        while cam.moving() && steps < 600 {
            cam.step(1.0 / 60.0);
            assert_point_close(cam.to_world(pointer), anchor, 1e-11);
            steps += 1;
        }
        assert!(!cam.moving(), "wheel glide should settle within 600 frames");
        let expected_zoom = (2.0_f64.ln() + 2.0 * 1.12_f64.ln()).exp();
        assert!((cam.zoom() - expected_zoom).abs() < 1e-12);
        assert_point_close(cam.to_world(pointer), anchor, 1e-11);

        // Panning mid-glide shifts the held world anchor and keeps the glide running.
        cam.wheel_zoom_at(pointer, 100.0);
        let zoom_before_pan = cam.zoom();
        cam.pan_by(50.0, -30.0);
        assert!(cam.moving());
        let shifted_anchor = Point::new(
            anchor.x + 50.0 / zoom_before_pan,
            anchor.y + 30.0 / zoom_before_pan,
        );
        while cam.moving() {
            cam.step(1.0 / 60.0);
            assert_point_close(cam.to_world(pointer), shifted_anchor, 1e-11);
        }
    }

    #[test]
    fn ease_to_lands_exactly_on_target_and_direct_moves_cancel_animations() {
        let mut cam = WallCamera::new(CameraPose::new(0.0, 0.0, 1.0), 1280.0, 800.0);
        let target = CameraPose::new(-320.5, 145.25, 4.25);

        assert!(!cam.is_at(target));
        cam.ease_to(target);
        assert!(cam.moving());
        assert_eq!(cam.target(), Some(target));

        let mut frames = 0;
        while cam.moving() && frames < 600 {
            cam.step(1.0 / 60.0);
            frames += 1;
        }
        assert!(!cam.moving(), "ease_to should land within 600 frames");
        assert_eq!(cam.pose(), target);
        assert!(cam.is_at(target));

        // Direct gestures cancel an in-flight ease.
        cam.ease_to(CameraPose::new(100.0, 100.0, 2.0));
        cam.pan_by(10.0, 5.0);
        assert!(!cam.moving());

        cam.ease_to(CameraPose::new(100.0, 100.0, 2.0));
        cam.move_to(5.0, -5.0);
        assert!(!cam.moving());
        assert_eq!(cam.pose().x, 5.0);
        assert_eq!(cam.pose().y, -5.0);
    }
}
