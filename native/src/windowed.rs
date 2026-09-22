//! Windowed mode: winit window + wgpu surface, uncapped continuous render loop,
//! FPS printed to stdout once per second. Renders whichever scene the CLI chose.
//!
//! Stage F: the glyph scenes run a FLY camera — WASD strafe/forward, E|R up,
//! Q|F down, scroll wheel = speed multiplier.
//! Stage G: interaction is split between the two mouse buttons so picking and
//! the fly camera coexist:
//!   - LEFT click (ungrabbed pointer) = PICK the glyph under the cursor
//!     (prints file:row:col:char; Stage L/L4: selection-tints it);
//!   - RIGHT press-and-drag = mouse-look (pointer confined + hidden while
//!     held; raw DeviceEvent deltas); Esc also releases;
//!   - verb keys act on the last pick: `h` highlight line, `g` grab/release
//!     the picked file (mouse drags it in the view plane, scroll scales it),
//!     `t` cycle the tint palette, `x` toggle hidden.
//!
//! Stage K: egui 0.36 overlay (feature `egui-ui`, default ON; `--no-ui` at
//! runtime gives exact pre-K behavior). egui sees every window event FIRST;
//! scene input routing only handles events egui did not consume. Per frame,
//! after the scene pass on the SAME encoder, an egui pass (LoadOp::Load, no
//! depth attachment) paints the tessellated UI onto the surface. K1 shipped
//! the plumbing with an EMPTY CentralPanel. K2 input-consumption rules:
//!   - keys route only when egui hasn't consumed them (a focused egui widget
//!     swallows keys — typing WASD/h/g/t/x in a text field must not fly the
//!     camera or fire verbs; Tab is always egui's);
//!   - right-press grab and left-click pick route only when egui doesn't
//!     want the pointer (hovering a panel counts); right-RELEASE always
//!     ungrabs (consumed releases must not latch the grab);
//!   - cursor bookkeeping always updates; scene cursor effects (look
//!     fallback, grabbed-group drags) skip while egui wants the pointer;
//!   - the moment egui wants ANY input, the look-grab is released (checked
//!     per frame in render()); it re-acquires once the pointer leaves egui.
//!
//! K3 adds the Debug window (FPS/camera/pick readouts, verb buttons calling
//! the exact CLI op API, K2 scratch text field). K4 makes LOD_MIN_PX live
//! via a slider (probe-cell → CullState Cell seam; offscreen keeps the
//! const) and adds live cull counters; F1 toggles the window.
//!
//! K6: in-window screenshot. The surface is configured with COPY_SRC (an
//! assert at configure checks the adapter supports it); on demand (F2 — an
//! app-level hotkey that fires regardless of egui focus — or the
//! `--screenshot-frame N --screenshot-out PATH` CLI pair) the just-encoded
//! surface texture is copied to a MAP_READ buffer AFTER the final
//! queue.submit and BEFORE present, blocking-mapped, swizzled BGRA→RGBA
//! (the windowed surface is Bgra8UnormSrgb — offscreen is Rgba and needs no
//! swizzle), and written as PNG. The readback happens after BOTH the scene
//! pass and the egui pass, so the PNG is the COMPOSED frame — 3D scene AND
//! the Debug window; that inclusion is the point (pixel-verification seam
//! for "invisible by construction" UI claims, per the stage erratum).
//!
//! Layout dial (repo scenes): the Debug panel's z_wrap_spacing slider
//! applies on drag RELEASE by rebuilding the scene — re-running
//! repo::load_repo with the new pitch and swapping the GlyphScene in place
//! (the pending_relayout arm in window_event). Layout is not a per-frame
//! input like K4's LOD threshold, and the JS system's `grid.layout` was
//! likewise a discrete refold command, not a live drag. The camera pose
//! survives the swap (restored from the old probe); pick/selection/grab
//! state resets with the scene.

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::glyph_scene::CameraMode;
use crate::gpu::GpuContext;
use crate::scene::{self, SceneLike};
use crate::{build_scene, Op, SceneChoice};

/// Stage K: the egui overlay — context, winit event translation, and the
/// wgpu painter. `None` in WindowState under `--no-ui`; the type (and all
/// egui code below) is compiled out entirely without the `egui-ui` feature.
#[cfg(feature = "egui-ui")]
struct EguiUi {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// K3: scratch text field in the Debug window — doubles as the K2
    /// typing-isolation trap test (typing WASD/h/g/t/x here must not fly the
    /// camera or fire verbs; the gating matrix in window_event ensures it).
    scratch: String,
    /// K4: Debug window visibility (F1 toggles; the window's own close
    /// button clears it).
    debug_open: bool,
    /// K5: group-browser substring filter (doubles as a second K2
    /// typing-isolation field).
    filter: String,
    /// K5: the browser's selected group (highlight only; click also flies
    /// the camera to the file).
    selected_group: Option<u32>,
}

/// Stage K (K3): Debug-panel verb buttons — CLI `--verb` literals parsed
/// through the same crate::parse_verb the CLI uses, so panel ⇔ CLI
/// equivalence is by construction. Zero-arg/default forms only; the
/// parameterized verbs (nudge/scale/move, tint-group rrggbb) stay CLI-only
/// until a phase needs arg entry.
#[cfg(feature = "egui-ui")]
const PANEL_VERBS: &[&str] = &[
    "recolor-glyph",
    "recolor-line",
    "tint-cycle",
    "hide-group",
    "show-group",
    "toggle-hidden",
];

struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    scene: Box<dyn SceneLike>,
    start: Instant,
    /// Stage F/G: mouse-look is active only while the pointer is grabbed
    /// (right button held, or backquote toggle).
    grabbed: bool,
    /// True once any DeviceEvent::MouseMotion has arrived during the current
    /// grab. Environments where raw device deltas never arrive (some macOS
    /// trackpad / remote-desktop paths) fall back to CursorMoved deltas;
    /// without the flag both paths would fire and look would double-apply.
    saw_device_delta: bool,
    /// Last cursor position, physical px (click picks use it).
    cursor: (f32, f32),
    last_frame: Instant,
    /// Stage K (K6): frames presented since startup (the `--screenshot-frame`
    /// counter — distinct from `frames`, which the FPS line resets every
    /// second).
    frames_total: u64,
    /// Stage K (K6): armed CLI capture — capture the frame that reaches N.
    shot_at_frame: Option<(u64, std::path::PathBuf)>,
    /// Stage K (K6): capture THIS frame to the path after the final submit
    /// (armed by F2, or by the CLI frame counter).
    capture_pending: Option<std::path::PathBuf>,
    /// Post-L3 fix: set when the surface reports Occluded (fully covered /
    /// display asleep). While set, about_to_wait throttles redraw requests
    /// to ~2 Hz instead of spinning at ~100% CPU (measured pre-fix: ~250k
    /// occlusion-skips/s). winit 0.30 gives no reliable wake on
    /// un-occlusion — WindowEvent::Occluded is iOS-only ("Others:
    /// Unsupported"), and RedrawRequested fires only on OS invalidation or
    /// an explicit request_redraw — so a purely event-driven wait could
    /// strand the window blank; the 500 ms retry guarantees recovery.
    occluded: bool,
    /// Last time an occluded retry was issued (the 2 Hz throttle).
    occluded_retry_at: Instant,
    // FPS accounting
    frames: u32,
    fps_window_start: Instant,
    /// Stage H: GPU pass timing accumulator for the once-per-second line
    /// (only fed when GLYPH_PROFILE=1 built a profiler).
    profile: crate::gpu::ProfileAccumulator,
    /// Stage K: egui overlay state; `None` under `--no-ui`.
    #[cfg(feature = "egui-ui")]
    egui: Option<EguiUi>,
    /// Stage K (K3): debug-UI probe — camera/pick snapshot written by the
    /// scene each frame (None for the demo scene and under --no-ui).
    #[cfg(feature = "egui-ui")]
    ui_probe: Option<crate::glyph_scene::UiProbe>,
    /// Stage K (K3): the 1 Hz FPS figure, mirrored for the Debug panel (the
    /// println below stays the primary output — offscreen logs read it).
    #[cfg(feature = "egui-ui")]
    ui_fps: f32,
    /// Stage K (K4) dev-only verification hook state (GLYPH_K4_SELFTEST=1):
    /// 0 = off/done, 1 = armed (fires at t>3 s), 2 = fired (print the
    /// after-counters next frame). See AGENTS.md debug env vars.
    #[cfg(feature = "egui-ui")]
    k4_selftest: u8,
    /// The relayout signal (repo and text scenes): the panel sets Some(...) on
    /// the z_wrap_spacing slider's drag release or the cluster toggle's click;
    /// the RedrawRequested arm consumes it and rebuilds the scene between
    /// frames. None where no layout params exist (demo/engine-text scenes).
    #[cfg(feature = "egui-ui")]
    pending_relayout: Option<RelayoutRequest>,
    /// Dev-only verification hook state (GLYPH_ZSPACE_SELFTEST=1): same
    /// 0/1/2/3 arming as cluster_selftest below, driving the layout dial
    /// through the same pending_relayout arm the slider's release uses.
    #[cfg(feature = "egui-ui")]
    zspace_selftest: u8,
    /// Dev-only verification hook state (GLYPH_CLUSTER_SELFTEST=1): same
    /// 0/1/2/3 arming as zspace_selftest, toggling the cluster mode through
    /// the same pending_relayout arm the panel's button uses.
    #[cfg(feature = "egui-ui")]
    cluster_selftest: u8,
}

impl WindowState {
    fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimized
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&ctx.device, &self.config);
        self.depth = scene::create_depth(&ctx.device, self.scene.depth_format(), width, height);
    }

    /// Stage F: grab the pointer for mouse-look (confined to the window —
    /// Locked is unsupported on macOS — and hidden). DeviceEvent deltas keep
    /// arriving at the window edges, which is what look control needs.
    ///
    /// Robustness fix: a failed cursor grab must NOT silently kill mouse-look
    /// (that was the "right-drag does nothing" bug — no log, grabbed stayed
    /// false). We now try Confined then Locked, log the outcome, and stay
    /// grabbed even if both fail: the CursorMoved fallback still gives look
    /// control, just without edge confinement.
    fn grab(&mut self) {
        if self.grabbed {
            return;
        }
        self.saw_device_delta = false;
        if self.window.set_cursor_grab(CursorGrabMode::Confined).is_ok() {
            self.window.set_cursor_visible(false);
            log::info!("mouse-look: pointer grabbed (confined)");
        } else if self.window.set_cursor_grab(CursorGrabMode::Locked).is_ok() {
            self.window.set_cursor_visible(false);
            log::info!("mouse-look: pointer grabbed (locked)");
        } else {
            log::warn!(
                "mouse-look: cursor grab unsupported here — look still works, \
                 but the pointer is not confined to the window"
            );
        }
        self.grabbed = true;
    }

    fn ungrab(&mut self) {
        if !self.grabbed {
            return;
        }
        let _ = self.window.set_cursor_grab(CursorGrabMode::None);
        self.window.set_cursor_visible(true);
        self.grabbed = false;
    }

    /// Stage K (K2): does egui currently want the pointer (hovering a panel,
    /// or mid-drag of a widget)? Feature off / `--no-ui`: never.
    /// `EventResponse.consumed` for CursorMoved only covers the mid-drag
    /// case (`egui_is_using_pointer`); this query additionally covers hover,
    /// so scene cursor effects (grabbed-group drags) don't track the pointer
    /// across egui windows.
    fn egui_wants_pointer(&self) -> bool {
        #[cfg(feature = "egui-ui")]
        {
            self.egui
                .as_ref()
                .is_some_and(|e| e.ctx.egui_wants_pointer_input())
        }
        #[cfg(not(feature = "egui-ui"))]
        {
            false
        }
    }

    /// Stage K (K6): read back the just-encoded surface texture (the
    /// COMPOSED frame — scene pass + egui pass) and write it to `path` as
    /// PNG. Mirrors offscreen.rs's readback (256-byte-aligned rows,
    /// blocking map) with one difference: the windowed surface is
    /// Bgra8UnormSrgb, so every pixel is swizzled BGRA→RGBA (offscreen's
    /// target is Rgba and skips this). Called after the final queue.submit
    /// and before present; the blocking wait stalls the loop for one frame,
    /// which is fine for an on-demand capture.
    fn capture_to_png(&self, ctx: &GpuContext, frame: &wgpu::SurfaceTexture, path: &std::path::Path) {
        let (w, h) = (self.config.width, self.config.height);
        let unpadded_bpr = w * 4;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windowed shot readback"),
            size: (padded_bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windowed shot copy"), // Stage L (O2)
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &frame.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        ctx.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        ctx.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .expect("device poll failed during windowed-shot readback");
        rx.recv()
            .expect("windowed-shot map_async callback dropped")
            .expect("windowed-shot buffer map failed");

        let data = slice.get_mapped_range().expect("windowed-shot range not mapped");
        let mut pixels = Vec::with_capacity((unpadded_bpr * h) as usize);
        for row in 0..h {
            let start = (row * padded_bpr) as usize;
            for px in data[start..start + unpadded_bpr as usize].as_chunks::<4>().0 {
                // BGRA → RGBA swizzle.
                pixels.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        drop(data);
        readback.unmap();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create windowed-shot directory");
        }
        image::save_buffer(path, &pixels, w, h, image::ColorType::Rgba8)
            .expect("failed to write windowed-shot PNG");
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        println!("windowed shot: wrote {}", abs.display());
    }

    fn render(&mut self, ctx: &GpuContext) {
        // Stage K (K4) dev-only verification hook (GLYPH_K4_SELFTEST=1):
        // move the LOD slider programmatically once (t≈3 s) and log the cull
        // counters before/after — exercises the full panel → probe →
        // CullState → cull_segments write path without a human at the mouse.
        #[cfg(feature = "egui-ui")]
        match self.k4_selftest {
            1 if self.time() > 3.0 => {
                if let Some(probe) = &self.ui_probe {
                    let mut p = probe.borrow_mut();
                    println!(
                        "K4SELFTEST before: lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                    // The panel slider's range max: at 16 px/em every visible
                    // fixture segment drops to its backdrop quad.
                    p.lod_min_px = 16.0;
                }
                self.k4_selftest = 2;
            }
            2 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "K4SELFTEST after:  lod_min_px={:.2} -> ranges={} instances={} backdrops={}",
                        p.lod_min_px, p.cull_ranges, p.cull_instances, p.cull_backdrops
                    );
                }
                self.k4_selftest = 0;
            }
            _ => {}
        }
        // Dev-only verification hook (GLYPH_ZSPACE_SELFTEST=1): drive the
        // layout dial programmatically — set the probe's z_wrap_spacing to
        // 2× its seed and fire the SAME pending_relayout arm the slider's
        // drag-release sets (consumed after this render, in window_event's
        // RedrawRequested arm). Logs the instance count (must NOT change —
        // z_step moves no slot counts) and the field's z extent (must
        // ~double) before/after the rebuild. States: 1 = fire at t>3 s;
        // 2 = one quiet frame (the rebuild happened after last frame's
        // render; THIS frame's scene render refreshes the new probe's
        // z_extent); 3 = print the after-readout, done. Skips when there is
        // no dial.
        #[cfg(feature = "egui-ui")]
        match self.zspace_selftest {
            1 if self.time() > 3.0 => {
                let seed = self.ui_probe.as_ref().and_then(|p| p.borrow().z_wrap_spacing);
                match seed {
                    Some(seed) => {
                        let before_extent = self.ui_probe.as_ref().and_then(|p| p.borrow().z_extent);
                        println!(
                            "ZSPACE-SELFTEST before: z_wrap_spacing={seed:.2} instances={} z_extent={before_extent:?}",
                            self.scene.instance_count()
                        );
                        let new = (seed * 2.0).min(1.0);
                        if let Some(probe) = &self.ui_probe {
                            probe.borrow_mut().z_wrap_spacing = Some(new);
                        }
                        self.pending_relayout = Some(RelayoutRequest {
                            z_wrap_spacing: Some(new),
                            toggle_cluster: false,
                        });
                        self.zspace_selftest = 2;
                    }
                    None => {
                        println!("ZSPACE-SELFTEST: no layout dial (non-repo scene or --no-ui) — skipping");
                        self.zspace_selftest = 0;
                    }
                }
            }
            2 => self.zspace_selftest = 3,
            3 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "ZSPACE-SELFTEST after:  z_wrap_spacing={:?} instances={} z_extent={:?}",
                        p.z_wrap_spacing,
                        self.scene.instance_count(),
                        p.z_extent
                    );
                }
                self.zspace_selftest = 0;
            }
            _ => {}
        }
        // Dev-only verification hook (GLYPH_CLUSTER_SELFTEST=1): toggle the
        // cluster mode through the SAME pending_relayout arm the panel's
        // button fires. Meaningful only on cluster-bearing content (the
        // g-cluster-repo fixture, or a text scene like emoji-corpus-small.txt):
        // instance count must DROP as trailers leave the arena. States:
        // 1 = fire at t>3 s; 2 = one quiet frame; 3 = print the
        // after-readout, done.
        #[cfg(feature = "egui-ui")]
        match self.cluster_selftest {
            1 if self.time() > 3.0 => {
                let has = self.ui_probe.as_ref().and_then(|p| p.borrow().cluster_mode);
                match has {
                    Some(on) => {
                        println!(
                            "CLUSTER-SELFTEST before: cluster_mode={on} instances={}",
                            self.scene.instance_count()
                        );
                        self.pending_relayout = Some(RelayoutRequest {
                            z_wrap_spacing: None,
                            toggle_cluster: true,
                        });
                        self.cluster_selftest = 2;
                    }
                    None => {
                        println!("CLUSTER-SELFTEST: scene carries no cluster mode (no toggle) — skipping");
                        self.cluster_selftest = 0;
                    }
                }
            }
            2 => self.cluster_selftest = 3,
            3 => {
                if let Some(probe) = &self.ui_probe {
                    let p = probe.borrow();
                    println!(
                        "CLUSTER-SELFTEST after:  cluster_mode={:?} instances={}",
                        p.cluster_mode,
                        self.scene.instance_count()
                    );
                }
                self.cluster_selftest = 0;
            }
            _ => {}
        }
        // wgpu 30: get_current_texture returns a status enum instead of Result.
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Lost/outdated surfaces happen on resize; reconfigure and skip.
            Cst::Lost | Cst::Outdated => {
                self.surface.configure(&ctx.device, &self.config);
                return;
            }
            // Post-L3 fix: occlusion is sticky (fully covered window /
            // display asleep) — mark it so about_to_wait throttles instead
            // of spinning. Timeout is transient acquire contention: skip the
            // frame and keep today's cadence (unchanged).
            Cst::Occluded => {
                if !self.occluded {
                    log::info!("surface occluded — throttling redraws to ~2 Hz until visible");
                }
                self.occluded = true;
                return;
            }
            Cst::Timeout => return,
            Cst::Validation => {
                log::error!("surface validation error on acquire");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windowed frame"), // Stage L (O2)
        });
        self.scene.render(
            ctx,
            &mut encoder,
            &crate::scene::FrameTarget {
                color_view: &view,
                // Stage L (L3): the composite's copy-vs-shader split keys on
                // the format; the copy path needs the texture handle.
                color_texture: &frame.texture,
                color_format: self.config.format,
                depth_view: &self.depth,
                width: self.config.width,
                height: self.config.height,
            },
            self.time(),
        );

        // Stage K (K1): egui overlay, painted as a second pass on the SAME
        // encoder after the scene pass. Exact 0.36 lifecycle (verified against
        // the vendored source): take_egui_input → run_ui →
        // handle_platform_output → tessellate → update_texture per
        // textures_delta.set (drained: TexturesDelta debug-panics if dropped
        // with unapplied deltas) → update_buffers (mandatory — render()
        // panics otherwise) → render pass with LoadOp::Load and NO depth
        // attachment (egui paints OVER the scene) → submit → free_texture per
        // textures_delta.free AFTER queue.submit (the freed textures may be
        // referenced by the in-flight submission). `forget_lifetime` turns
        // encoder-aliasing mistakes into runtime errors, so the pass is
        // scoped and dropped before we touch the encoder again.
        #[cfg(feature = "egui-ui")]
        let mut egui_textures_delta: Option<egui::TexturesDelta> = None;
        // Stage K (K2): set inside the egui block (post-run_ui, so egui's
        // focus/hover state is fresh for THIS frame); acted on after the
        // borrow ends.
        #[cfg(feature = "egui-ui")]
        let mut egui_wants_input = false;
        #[cfg(feature = "egui-ui")]
        if let Some(egui) = self.egui.as_mut() {
            let raw_input = egui.state.take_egui_input(&self.window);
            // Clone the Context (an Arc bump) so the closure below can borrow
            // the other EguiUi / WindowState fields disjointly.
            let egui_ctx = egui.ctx.clone();
            // K3: snapshot the probe before the closure (RefCell stays
            // unborrowed across the UI build).
            let probe_snap = self.ui_probe.as_ref().map(|p| p.borrow().clone());
            let fps = self.ui_fps;
            // K4: hoist the panel-state borrows so the Window builder and the
            // inner closure capture disjoint fields.
            let debug_open = &mut egui.debug_open;
            let scratch = &mut egui.scratch;
            let filter = &mut egui.filter;
            let selected_group = &mut egui.selected_group;
            // The layout dial's apply signal: the panel's slider sets it on
            // release; the RedrawRequested arm consumes it and rebuilds.
            let pending_relayout = &mut self.pending_relayout;
            let full_output = egui_ctx.run_ui(raw_input, |root_ui| {
                // K1 leftover REMOVED (stage-k fix): the empty
                // `CentralPanel::default()` paints an OPAQUE full-viewport
                // panel_fill rect that blanketed the 3D scene — "background
                // layer" only affects input order, not fill. With real
                // windows present it is vestigial; no CentralPanel is needed
                // (egui::Window shows fine without one).
                // K3: the Debug window. Every scene-facing call below is the
                // exact API the CLI ops use — verb buttons parse CLI-literal
                // strings through crate::parse_verb and call
                // SceneLike::apply_verb; the determinism chain never learns
                // egui exists.
                egui::Window::new("Debug")
                    .id(egui::Id::new("stage_k_debug_panel"))
                    .open(debug_open)
                    .show(root_ui.ctx(), |ui| {
                        ui.label(format!("FPS: {fps:.1}"));
                        match &probe_snap {
                            Some(snap) => {
                                let mode = match snap.camera_mode {
                                    Some(crate::glyph_scene::CameraMode::Front { zoom }) => {
                                        format!("Front(zoom {zoom:.2})")
                                    }
                                    Some(crate::glyph_scene::CameraMode::Orbit) => {
                                        "Orbit".to_string()
                                    }
                                    Some(crate::glyph_scene::CameraMode::Fly) => "Fly".to_string(),
                                    None => "—".to_string(),
                                };
                                ui.label(format!(
                                    "camera: {mode} eye=({:.2},{:.2},{:.2}) yaw={:.3} pitch={:.3}",
                                    snap.eye[0], snap.eye[1], snap.eye[2], snap.yaw, snap.pitch,
                                ));
                                ui.label(
                                    snap.last_pick
                                        .as_deref()
                                        .unwrap_or("pick: (none yet — left-click a glyph)"),
                                );
                            }
                            None => {
                                ui.label("camera/pick: n/a (demo scene has no probe)");
                            }
                        }
                        ui.separator();
                        ui.label("verbs (same strings --verb parses):");
                        ui.horizontal_wrapped(|ui| {
                            for spec in PANEL_VERBS {
                                if ui.button(*spec).clicked() {
                                    let verb = crate::parse_verb(spec)
                                        .expect("panel verb literal must parse like the CLI");
                                    if let Some(line) = self.scene.apply_verb(ctx, &verb) {
                                        println!("{line}");
                                    }
                                }
                            }
                        });
                        // K4: live cull/LOD tuning — the slider writes the
                        // shared probe cell; GlyphScene::render applies it to
                        // CullState's Cell before culling (same frame). The
                        // readouts are the sums GLYPH_CULL_DEBUG prints.
                        if let (Some(snap), Some(cell)) = (&probe_snap, &self.ui_probe) {
                            ui.separator();
                            ui.label("cull/LOD — live, windowed only (offscreen keeps consts):");
                            // Range brackets the const default (1.0 px/em) with
                            // ~2 octaves each way; logarithmic because the
                            // threshold is a perceptual scale.
                            ui.add(
                                egui::Slider::new(&mut cell.borrow_mut().lod_min_px, 0.25..=16.0)
                                    .logarithmic(true)
                                    .text("LOD_MIN_PX px/em (const 1.0)"),
                            );
                            ui.label(format!(
                                "cull: {} draw ranges, {} instances | {} backdrops",
                                snap.cull_ranges, snap.cull_instances, snap.cull_backdrops
                            ));
                        }
                        // The layout dial (repo scenes only): the wrap
                        // staircase's pitch. Unlike K4's cull input this IS
                        // layout — applying re-runs load_repo and rebuilds
                        // the scene — so it fires on drag release (or a typed
                        // commit), never per tick: the JS system's
                        // `grid.layout` was likewise a discrete refold
                        // command, not a live drag.
                        if let (Some(snap), Some(cell)) = (&probe_snap, &self.ui_probe) {
                            let mut p = cell.borrow_mut();
                            if let Some(spacing) = &mut p.z_wrap_spacing {
                                ui.separator();
                                ui.label(
                                    "layout (repo) — applies on release; the scene rebuilds, \
                                     pick/selection state resets:",
                                );
                                let resp = ui.add(
                                    egui::Slider::new(spacing, 0.0..=1.0)
                                        .text("z_wrap_spacing × em (0 = flat, default 0.15)"),
                                );
                                if let Some([lo, hi]) = snap.z_extent {
                                    ui.label(format!(
                                        "field depth: z ∈ [{lo:.1}, {hi:.1}] world units"
                                    ));
                                }
                                if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                                    *pending_relayout = Some(RelayoutRequest {
                                        z_wrap_spacing: Some(*spacing),
                                        toggle_cluster: false,
                                    });
                                }
                            }
                        }
                        // The sequence pass toggle — repo AND text scenes
                        // (text scenes seed the probe from the staging
                        // choice, no pick context needed). The same rebuild
                        // arm as the dial: a discrete refold per click,
                        // never a per-tick write.
                        if let Some(snap) = &probe_snap {
                            if let Some(on) = snap.cluster_mode {
                                ui.separator();
                                ui.label(
                                    "sequence pass — click toggles; the scene rebuilds, \
                                     pick/selection state resets:",
                                );
                                if ui
                                    .button(if on { "cluster mode: ON" } else { "cluster mode: off" })
                                    .clicked()
                                {
                                    *pending_relayout = Some(RelayoutRequest {
                                        z_wrap_spacing: None,
                                        toggle_cluster: true,
                                    });
                                }
                            }
                        }
                        ui.separator();
                        ui.label("K2 typing test — WASD/h/g/t/x here must not move the scene:");
                        ui.text_edit_singleline(scratch);
                        // K5: group browser — FLAT virtualized list
                        // (ScrollArea::show_rows builds only the visible
                        // rows; a CollapsingHeader-per-directory tree is NOT
                        // virtualized and would be the classic egui
                        // large-list trap at ~1.3k files). Vanilla egui — no
                        // new deps (fence 2). Row click = select + fly-to
                        // via the exact --cam-pose API (set_cam_pose);
                        // framing mirrors camera_eye_target's Front formula.
                        if let Some(snap) = &probe_snap {
                            if !snap.files.is_empty() {
                                ui.separator();
                                ui.label(format!(
                                    "files: {} (click = select + fly to file)",
                                    snap.files.len()
                                ));
                                ui.horizontal(|ui| {
                                    ui.label("filter:");
                                    ui.text_edit_singleline(filter);
                                });
                                let needle = filter.to_lowercase();
                                let idxs: Vec<usize> = snap
                                    .files
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, r)| {
                                        needle.is_empty()
                                            || r.rel_path.to_lowercase().contains(&needle)
                                    })
                                    .map(|(i, _)| i)
                                    .collect();
                                let row_h = ui.text_style_height(&egui::TextStyle::Body);
                                let viewport_aspect =
                                    self.config.width as f32 / self.config.height.max(1) as f32;
                                egui::ScrollArea::vertical()
                                    .max_height(280.0)
                                    .show_rows(ui, row_h, idxs.len(), |ui, range| {
                                        for &i in &idxs[range] {
                                            let row = &snap.files[i];
                                            let dynst =
                                                snap.file_dyn.get(i).copied().unwrap_or_default();
                                            let depth = row.rel_path.matches('/').count();
                                            ui.horizontal(|ui| {
                                                // Tint swatch drawn, not
                                                // typeset (no font-glyph
                                                // dependency).
                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(10.0, 10.0),
                                                    egui::Sense::hover(),
                                                );
                                                ui.painter().rect_filled(
                                                    rect,
                                                    2.0,
                                                    egui::Color32::from_rgb(
                                                        dynst.tint[0],
                                                        dynst.tint[1],
                                                        dynst.tint[2],
                                                    ),
                                                );
                                                let label = format!(
                                                    "{}{}{}",
                                                    "  ".repeat(depth),
                                                    row.rel_path,
                                                    if dynst.hidden { "  (hidden)" } else { "" },
                                                );
                                                let resp = ui.selectable_label(
                                                    *selected_group == Some(row.group_id),
                                                    label,
                                                );
                                                if resp.clicked() {
                                                    *selected_group = Some(row.group_id);
                                                    // Front-camera framing
                                                    // (camera_eye_target):
                                                    // fit the AABB, margin
                                                    // 1.08 + 2.0, text plane
                                                    // faces +Z.
                                                    let half_h_needed = dynst
                                                        .half[1]
                                                        .max(dynst.half[0] / viewport_aspect);
                                                    let dist = half_h_needed
                                                        / (crate::glyph_scene::FOV_Y
                                                            .to_radians()
                                                            * 0.5)
                                                            .tan()
                                                        * 1.08
                                                        + 2.0;
                                                    self.scene.set_cam_pose(
                                                        [dynst.center[0], dynst.center[1], dist],
                                                        0.0,
                                                        0.0,
                                                    );
                                                }
                                            });
                                        }
                                    });
                            }
                        }
                    });
            });
            let egui::FullOutput {
                platform_output,
                mut textures_delta,
                shapes,
                pixels_per_point,
                viewport_output: _,
            } = full_output;
            egui.state
                .handle_platform_output(&self.window, platform_output);
            let clipped = egui.ctx.tessellate(shapes, pixels_per_point);
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.config.width, self.config.height],
                pixels_per_point,
            };
            for (id, image_deltas) in textures_delta.set.drain() {
                for image_delta in image_deltas {
                    egui.renderer
                        .update_texture(&ctx.device, &ctx.queue, id, &image_delta);
                }
            }
            let user_cmds = egui.renderer.update_buffers(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                &clipped,
                &screen,
            );
            // Callback command buffers (from paint callbacks) must be
            // submitted before the main buffer. K1 registers no callbacks, so
            // this is always empty — the branch keeps the contract explicit.
            if !user_cmds.is_empty() {
                ctx.queue.submit(user_cmds);
            }
            // Stage H scheme, mirrored: pass-boundary timestamp query.
            let pass_query = ctx
                .profiler
                .as_ref()
                .map(|p| p.borrow().begin_pass_query("egui pass", &mut encoder));
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui pass"),
                    timestamp_writes: pass_query
                        .as_ref()
                        .and_then(|q| q.render_pass_timestamp_writes()),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });
                egui.renderer
                    .render(&mut pass.forget_lifetime(), &clipped, &screen);
            }
            if let (Some(p), Some(q)) = (&ctx.profiler, pass_query) {
                p.borrow().end_query(&mut encoder, q);
            }
            egui_textures_delta = Some(textures_delta);
            egui_wants_input =
                egui.ctx.egui_wants_pointer_input() || egui.ctx.egui_wants_keyboard_input();
        }

        // Stage K (K2): UI focus and the mouse-look grab cannot coexist —
        // while grabbed, raw DeviceEvent deltas drive look and bypass egui
        // entirely, and a hidden/confined pointer must never leave egui
        // thinking it is being hovered. The moment egui wants input (pointer
        // over a panel, or a focused widget), release the grab. Grab
        // re-acquires normally once the pointer leaves egui's area
        // (`egui_wants_pointer_input` is false again, so right-press routes
        // to the scene). Mid-look-drags are NOT interrupted: while a button
        // is down and the drag started outside egui, egui reports wanting
        // nothing (see `egui_wants_pointer_input`'s `any_down` clause).
        #[cfg(feature = "egui-ui")]
        if egui_wants_input && self.grabbed {
            self.ungrab();
        }

        // Stage H: resolve profiler queries before submit (see offscreen.rs).
        if let Some(p) = &ctx.profiler {
            p.borrow_mut().resolve_queries(&mut encoder);
        }
        ctx.queue.submit([encoder.finish()]);
        // Stage K: free egui textures only AFTER the submit that referenced
        // them is handed to the queue.
        #[cfg(feature = "egui-ui")]
        if let (Some(egui), Some(delta)) = (self.egui.as_mut(), egui_textures_delta.as_mut()) {
            for id in delta.free.drain() {
                egui.renderer.free_texture(&id);
            }
        }
        // Stage K (K6): scripted CLI capture — arm once this frame reaches N
        // (the current frame is the (frames_total+1)-th).
        if let Some((n, path)) = &self.shot_at_frame {
            if self.frames_total + 1 >= *n {
                self.capture_pending = Some(path.clone());
                self.shot_at_frame = None;
            }
        }
        // Stage K (K6): capture BEFORE present — the acquired texture is the
        // composed frame (scene + egui), still ours to copy from.
        if let Some(path) = self.capture_pending.take() {
            self.capture_to_png(ctx, &frame, &path);
        }
        // wgpu 30: presentation goes through the queue, not the texture.
        ctx.queue.present(frame);
        self.frames_total += 1;
        // Post-L3 fix: a successful present clears occlusion (the 2 Hz retry
        // found the window visible again).
        self.occluded = false;

        // Stage H: close the profiler frame and fold any GPU-completed frame
        // into the running means (non-blocking pump for the query maps).
        if let Some(p) = &ctx.profiler {
            if let Err(e) = p.borrow_mut().end_frame() {
                log::warn!("profiler end_frame: {e}");
            }
            let _ = ctx.device.poll(wgpu::PollType::Poll);
            let period = ctx.queue.get_timestamp_period();
            if let Some(results) = p.borrow_mut().process_finished_frame(period) {
                self.profile.add_frame(&results);
            }
        }

        // FPS: print a line every second.
        self.frames += 1;
        let elapsed = self.fps_window_start.elapsed();
        if elapsed.as_secs_f32() >= 1.0 {
            let profile_suffix = if ctx.profiler.is_some() {
                let cpu = crate::gpu::take_cpu_scope_summary(ctx);
                format!(
                    " | profile: GPU {} | CPU {}",
                    if self.profile.scopes.is_empty() {
                        "—".to_string()
                    } else {
                        self.profile.summary()
                    },
                    if cpu.is_empty() { "—".to_string() } else { cpu },
                )
            } else {
                String::new()
            };
            // The present mode rides on the line so a figure is never read
            // without its cap: under Fifo this IS the display's refresh.
            println!(
                "FPS: {:.1} ({} frames in {:.2?}, {} instances, present={:?}){profile_suffix}",
                self.frames as f32 / elapsed.as_secs_f32(),
                self.frames,
                elapsed,
                self.scene.instance_count(),
                self.config.present_mode,
            );
            // Stage K (K3): mirror the same figure for the Debug panel.
            #[cfg(feature = "egui-ui")]
            {
                self.ui_fps = self.frames as f32 / elapsed.as_secs_f32();
            }
            self.frames = 0;
            self.fps_window_start = Instant::now();
            self.profile = crate::gpu::ProfileAccumulator::default();
        }
    }

    /// Wall-clock animation time (windowed mode is not determinism-bound).
    fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}

/// The relayout arm: mutate the scene choice's layout params, then swap the
/// scene in place — rebuilt through the same builder startup used, so it is
/// correct by construction (the one true layout path rebuilds everything the
/// params touch: cull AABBs, per-file pick params, bounds) at the cost of a
/// full reload per apply, which is why the panel fires on release/click, not
/// per tick. The camera pose survives via the old probe's last frame;
/// pick/selection/grab state resets with the scene (a fresh load has none —
/// same as the JS `grid.layout` refold). Repo scenes carry the z dial and
/// the cluster toggle; text scenes carry the toggle only; anything else has
/// no layout params and a stale signal is a no-op.
/// A free function, not an App method: the caller already holds
/// `state: &mut WindowState` borrowed from `self.state`, so `&mut self`
/// would double-borrow.
#[cfg(feature = "egui-ui")]
fn apply_relayout(
    ctx: &GpuContext,
    choice: &mut SceneChoice,
    cull: bool,
    ui: bool,
    state: &mut WindowState,
    req: RelayoutRequest,
) {
    let mut changed = false;
    let mut note = String::new();
    // The z dial exists on repo scenes only.
    if let SceneChoice::Repo { z_wrap_spacing, .. } = choice {
        if let Some(new_z) = req.z_wrap_spacing {
            if z_wrap_spacing.to_bits() != new_z.to_bits() {
                *z_wrap_spacing = new_z;
                changed = true;
                note += &format!("z_wrap_spacing -> {new_z} ");
            }
        }
    }
    // The cluster toggle rides any scene whose choice carries the mode —
    // repo and text alike (SceneChoice::Text has carried it since the pass
    // landed; the panel seed for text scenes is GlyphScene's
    // probe_cluster_mode).
    if req.toggle_cluster {
        let mode = match choice {
            SceneChoice::Repo { cluster_mode, .. } | SceneChoice::Text { cluster_mode, .. } => {
                Some(cluster_mode)
            }
            _ => None,
        };
        if let Some(mode) = mode {
            *mode = match *mode {
                crate::fold::ClusterMode::Leader => crate::fold::ClusterMode::Cluster,
                crate::fold::ClusterMode::Cluster => crate::fold::ClusterMode::Leader,
            };
            changed = true;
            note += &format!("cluster_mode -> {mode:?} ");
        }
    }
    if !changed {
        return; // a release without a move (a click, a typed repeat) rebuilds nothing
    }
    // Snapshot the camera BEFORE the swap: the probe holds last frame's
    // actual eye/yaw/pitch, and a rebuild that teleports the viewer would
    // make the dial unusable.
    let pose = state.ui_probe.as_ref().map(|p| {
        let p = p.borrow();
        (p.eye, p.yaw, p.pitch)
    });
    let t = Instant::now();
    let (mut scene, probe) = if ui {
        crate::build_scene_probed(ctx, state.config.format, choice, CameraMode::Fly, cull)
    } else {
        (build_scene(ctx, state.config.format, choice, CameraMode::Fly, cull), None)
    };
    scene.set_viewport(state.config.width, state.config.height);
    if let Some((eye, yaw, pitch)) = pose {
        scene.set_cam_pose(eye, yaw, pitch);
    }
    state.scene = scene;
    state.ui_probe = probe;
    println!(
        "relayout: {note}(scene rebuilt in {:?}; pick/selection/grab state reset)",
        t.elapsed()
    );
}

/// What the panel asked the rebuild arm to change. Either field alone may be
/// set; the arm applies both and rebuilds only if one actually moved.
#[cfg(feature = "egui-ui")]
#[derive(Clone, Copy, Default)]
struct RelayoutRequest {
    z_wrap_spacing: Option<f64>,
    toggle_cluster: bool,
}

struct App<'a> {
    ctx: GpuContext,
    /// Owned, not borrowed: the relayout arm mutates the choice's params in
    /// place (Repo's z_wrap_spacing, Repo/Text cluster_mode) before rebuilding
    /// the scene.
    choice: SceneChoice,
    cull: bool,
    /// Stage G: scripted picks/verbs applied once at startup (smoke testing
    /// the same code path the windowed verbs use).
    ops: &'a [Op],
    start: Instant,
    state: Option<WindowState>,
    /// Stage K: build the egui overlay (false under `--no-ui`).
    #[cfg(feature = "egui-ui")]
    ui: bool,
    /// Stage K (K6): scripted in-window capture (`--screenshot-frame N`
    /// `--screenshot-out PATH`).
    shot: Option<(u64, std::path::PathBuf)>,
    /// `--present-mode`. Fifo is vsync and caps the FPS line at the display's
    /// refresh (75 on the first Linux box, 2026-09-07 — a number that says
    /// nothing about the renderer). Applied only if the surface offers it.
    present_mode: wgpu::PresentMode,
}

impl ApplicationHandler for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("glyph3d-native — Stage F (fly camera + GPU cull/LOD)")
                        .with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0)),
                )
                .expect("window creation failed"),
        );

        // Arc<Window> gives the surface a 'static lifetime handle.
        let surface = self
            .ctx
            .instance
            .create_surface(window.clone())
            .expect("surface creation failed");

        let caps = surface.get_capabilities(&self.ctx.adapter);
        // Prefer an sRGB format to match the offscreen oracle's output space.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Stage K (K6): COPY_SRC enables the in-window screenshot readback
        // (copy_texture_to_buffer off the acquired surface texture). Metal
        // supports it; assert once at configure so an exotic adapter fails
        // loud at startup, not at capture time.
        assert!(
            caps.usages.contains(wgpu::TextureUsages::COPY_SRC),
            "K6: surface/adapter does not support COPY_SRC — in-window screenshot impossible \
             (caps.usages = {:?})",
            caps.usages
        );
        let size = window.inner_size();
        let present_mode = if caps.present_modes.contains(&self.present_mode) {
            self.present_mode
        } else {
            log::warn!(
                "present mode {:?} not offered by this surface (offers {:?}); using Fifo",
                self.present_mode,
                caps.present_modes
            );
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            // Stage K (K6): COPY_SRC = in-window screenshot readback.
            // Stage L (L3): COPY_DST = the composite's copy path when the
            // surface format happens to match the pool (non-Metal adapters);
            // windowed normally composites through the shader (BGRA).
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            format,
            // Auto == sRGB for 8-bit formats; required field in wgpu 30.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width,
            height: size.height,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&self.ctx.device, &config);

        // Stage K (K1): egui overlay state, constructed after the surface is
        // configured. The renderer gets the ACTUAL configured surface format
        // (never hardcode it): our format is sRGB, which egui accepts via its
        // linear-framebuffer shader path — it logs a once-per-pipeline warning
        // (accepted: switching the surface to gamma-space would perturb the
        // glyph pass's output encoding for zero UI gain).
        #[cfg(feature = "egui-ui")]
        let egui = if self.ui {
            let egui_ctx = egui::Context::default();
            let state = egui_winit::State::new(
                egui_ctx.clone(),
                egui::ViewportId::ROOT,
                &*window,
                Some(window.scale_factor() as f32),
                None,
                Some(self.ctx.device.limits().max_texture_dimension_2d as usize),
            );
            let renderer = egui_wgpu::Renderer::new(
                &self.ctx.device,
                format,
                egui_wgpu::RendererOptions::default(),
            );
            Some(EguiUi {
                ctx: egui_ctx,
                state,
                renderer,
                scratch: String::new(),
                debug_open: true,
                filter: String::new(),
                selected_group: None,
            })
        } else {
            None
        };

        // Stage F: windowed glyph scenes get the fly camera (the Stage A demo
        // scene keeps its internal orbit; it ignores camera_mode).
        // Stage K (K3): with the UI on, install the debug-UI probe on the
        // concrete GlyphScene BEFORE type erasure (see build_scene_probed).
        #[cfg(feature = "egui-ui")]
        let (mut scene, ui_probe) = if self.ui {
            crate::build_scene_probed(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull)
        } else {
            (build_scene(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull), None)
        };
        #[cfg(not(feature = "egui-ui"))]
        let mut scene = build_scene(&self.ctx, format, &self.choice, CameraMode::Fly, self.cull);
        let depth = scene::create_depth(&self.ctx.device, scene.depth_format(), config.width, config.height);
        log::info!(
            "surface: {}x{} {:?} present={:?}",
            config.width,
            config.height,
            format,
            config.present_mode
        );
        println!(
            "fly camera: WASD move | E|R up, Q|F down | RIGHT-drag or ` (backquote) = look | scroll = speed | Esc releases\n\
             \x20 interact: LEFT click = pick glyph | h highlight line | g grab file \
             (mouse drags, scroll scales) | t cycle tint | x hide/show"
        );
        #[cfg(feature = "egui-ui")]
        if self.ui {
            println!("debug panel: F1 toggles the egui Debug window (sliders tune LOD_MIN_PX live)");
        }
        println!("screenshot: F2 saves the next presented frame to out/windowed-shot-<timestamp>.png");

        // Stage G: scripted startup picks/verbs (same entry points the
        // windowed event handlers use).
        scene.set_viewport(config.width, config.height);
        for op in self.ops {
            let line = match op {
                Op::Pick(p) => scene.apply_pick(&self.ctx, p),
                Op::Verb(v) => scene.apply_verb(&self.ctx, v),
                Op::CamPose(eye, yaw, pitch) => {
                    scene.set_cam_pose(*eye, *yaw, *pitch);
                    None
                }
            };
            if let Some(line) = line {
                println!("{line}");
            }
        }

        self.state = Some(WindowState {
            window,
            surface,
            config,
            depth,
            scene,
            start: self.start,
            grabbed: false,
            saw_device_delta: false,
            cursor: (0.0, 0.0),
            last_frame: Instant::now(),
            frames_total: 0,
            shot_at_frame: self.shot.clone(),
            capture_pending: None,
            occluded: false,
            occluded_retry_at: Instant::now(),
            frames: 0,
            fps_window_start: Instant::now(),
            profile: crate::gpu::ProfileAccumulator::default(),
            #[cfg(feature = "egui-ui")]
            egui,
            #[cfg(feature = "egui-ui")]
            ui_probe,
            #[cfg(feature = "egui-ui")]
            ui_fps: 0.0,
            #[cfg(feature = "egui-ui")]
            k4_selftest: u8::from(std::env::var_os("GLYPH_K4_SELFTEST").is_some()),
            #[cfg(feature = "egui-ui")]
            pending_relayout: None,
            #[cfg(feature = "egui-ui")]
            zspace_selftest: u8::from(std::env::var_os("GLYPH_ZSPACE_SELFTEST").is_some()),
            #[cfg(feature = "egui-ui")]
            cluster_selftest: u8::from(std::env::var_os("GLYPH_CLUSTER_SELFTEST").is_some()),
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        // Stage K: egui sees every window event FIRST. Scene routing below only
        // handles events egui did not consume (0.36 semantics: pointer/wheel
        // consumed when egui wants pointer input; CursorMoved consumed while
        // egui is using the pointer; keys consumed when a widget has focus,
        // and Tab always). CloseRequested/Resized/RedrawRequested are never
        // consumed, so the lifecycle arms stay ungated — and are matched
        // before the consumption arm defensively anyway.
        #[cfg(feature = "egui-ui")]
        let egui_consumed = match state.egui.as_mut() {
            Some(egui) => egui.state.on_window_event(&state.window, &event).consumed,
            None => false,
        };
        #[cfg(not(feature = "egui-ui"))]
        let egui_consumed = false;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                // Post-L3 fix: a resize means the window is being shown —
                // clear occlusion so the next about_to_wait resumes the
                // continuous redraw immediately.
                state.occluded = false;
                state.resize(&self.ctx, size.width, size.height);
                state.scene.set_viewport(size.width, size.height);
            }
            WindowEvent::RedrawRequested => {
                state.render(&self.ctx);
                // The layout controls apply BETWEEN frames: the panel set
                // pending_relayout on slider release / toggle click during
                // this render; rebuild now so the next render presents the
                // new params.
                #[cfg(feature = "egui-ui")]
                if let Some(req) = state.pending_relayout.take() {
                    apply_relayout(&self.ctx, &mut self.choice, self.cull, self.ui, state, req);
                }
            }
            // Stage K (K6): F2 = capture the next presented frame to PNG.
            // App-level hotkey that must work REGARDLESS of egui focus (e.g.
            // while typing in the scratch field), so it matches ABOVE the
            // egui-consumed arm — unlike F1, which deliberately yields to a
            // focused field. egui still saw the event first (the feed at the
            // top); it ignores F2.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(event.physical_key, PhysicalKey::Code(KeyCode::F2)) =>
            {
                // Anchor at the repo root's out/ (CARGO_MANIFEST_DIR is
                // native/), not the process cwd — running from native/ used
                // to scatter shots into native/out/.
                state.capture_pending = Some(std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../out"
                )).join(format!(
                    "windowed-shot-{}.png",
                    utc_stamp(std::time::SystemTime::now())
                )));
            }
            // Stage K (K2): right-RELEASE always ungrabs, even when egui
            // consumed the event (e.g. the release landed on a panel).
            // Otherwise a grab started outside a panel would latch on
            // forever (the K1 ordering swallowed consumed releases). ungrab()
            // is a no-op when not grabbed, so panel releases stay harmless.
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Right, .. } => {
                state.ungrab();
            }
            // Consumed by egui (e.g. click on a panel, typing in a focused
            // widget): never routed to the scene.
            _ if egui_consumed => {}
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    // Stage K (K4): F1 toggles the Debug window — never scene
                    // input. (With the scratch field focused, egui consumes
                    // F1 first; release focus with Esc, then F1.)
                    #[cfg(feature = "egui-ui")]
                    if code == KeyCode::F1 && pressed {
                        if let Some(egui) = state.egui.as_mut() {
                            egui.debug_open = !egui.debug_open;
                        }
                        return;
                    }
                    // Esc releases the pointer grab (and is not camera input).
                    if code == KeyCode::Escape && pressed {
                        state.ungrab();
                    } else if code == KeyCode::Backquote && pressed {
                        // Backquote toggles mouse-look grab — an always-available
                        // alternative to holding the right button (trackpads,
                        // mice without a usable right button, accessibility).
                        if state.grabbed {
                            state.ungrab();
                        } else {
                            state.grab();
                        }
                    } else {
                        state.scene.on_key(&self.ctx, code, pressed);
                    }
                }
            }
            // Stage G: LEFT click picks (only while the pointer is NOT
            // grabbed); RIGHT press grabs for mouse-look. (The release arm
            // lives above the egui-consumed arm — see the K2 note there.)
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                state.grab();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if !state.grabbed {
                    let (x, y) = state.cursor;
                    state.scene.on_click(&self.ctx, x, y);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (px, py) = (position.x as f32, position.y as f32);
                // Stage K (K2): cursor POSITION bookkeeping always updates
                // (click picks read it), but scene cursor effects route only
                // while egui doesn't want the pointer — hovering an egui
                // window included (EventResponse.consumed for CursorMoved
                // covers only mid-widget-drag, not hover). This stops a
                // g-grabbed group from tracking the pointer across panels.
                let route_to_scene = !state.egui_wants_pointer();
                // Fallback look path: while grabbed, if no raw DeviceEvent
                // deltas have ever arrived this grab (dead device-delta
                // environments), drive look from cursor movement instead.
                // Mid-look-drag egui wants nothing (drag started outside
                // it), so this still fires while grabbed over a panel.
                if route_to_scene && state.grabbed && !state.saw_device_delta {
                    let (dx, dy) = (px - state.cursor.0, py - state.cursor.1);
                    if dx != 0.0 || dy != 0.0 {
                        state.scene.on_mouse_look(&self.ctx, dx, dy);
                    }
                }
                state.cursor = (px, py);
                if route_to_scene {
                    state.scene.on_cursor(&self.ctx, px, py);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Pixel deltas (macOS trackpads): ~53 px per notch.
                    MouseScrollDelta::PixelDelta(p) => (p.y / 53.0) as f32,
                };
                state.scene.on_scroll(&self.ctx, lines);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        // Raw mouse deltas drive look while grabbed (unaffected by the
        // confined pointer hitting the window edge).
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            if let Some(state) = self.state.as_mut() {
                if state.grabbed {
                    state.saw_device_delta = true;
                    state.scene.on_mouse_look(&self.ctx, dx as f32, dy as f32);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            // Post-L3 fix: while the surface reports Occluded, do NOT spin
            // request_redraw (measured pre-fix: ~250k skipped acquires/s,
            // ~100% CPU). winit 0.30 gives no reliable wake when a fully
            // occluded macOS window becomes visible again
            // (WindowEvent::Occluded is iOS-only; RedrawRequested fires on
            // OS invalidation or an explicit request_redraw), so a purely
            // event-driven wait could strand the window blank — instead
            // retry at ~2 Hz via a WaitUntil deadline; recovery takes at
            // most half a second. Not occluded: restore the default Wait
            // (the continuous redraw below keeps the loop hot as today).
            if state.occluded {
                const RETRY: std::time::Duration = std::time::Duration::from_millis(500);
                let elapsed = state.occluded_retry_at.elapsed();
                if elapsed < RETRY {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + (RETRY - elapsed)));
                    return;
                }
                state.occluded_retry_at = Instant::now();
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            // Fixed-cap dt so a stalled frame (resize, HUD hiccup) doesn't
            // launch the camera.
            let dt = state.last_frame.elapsed().as_secs_f32().min(0.1);
            state.last_frame = Instant::now();
            state.scene.tick(dt);
            state.window.request_redraw(); // continuous render loop
        }
    }
}

/// Stage K (K6): `yyyymmdd-hhmmss` UTC stamp for shot filenames (no chrono
/// dep; Howard Hinnant's civil-from-days algorithm).
fn utc_stamp(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the unix epoch")
        .as_secs();
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    // Canonical Hinnant: (5*doy + 2)/153. (An earlier edit used the +456
    // variant's constant with the canonical d/m formulas — the variants are
    // not mixable; produced e.g. month=12 day=89 for 2026-09-02.)
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", tod / 3600, tod % 3600 / 60, tod % 60)
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn utc_stamp_known_epochs() {
        // Values pinned against `date -u` (macOS): epoch 0, and the K6 fix
        // date 2026-09-02 18:21:12 UTC (the bad stamp that exposed the bug
        // read "202612-89-182112").
        let at = |s: u64| utc_stamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "19700101-000000");
        assert_eq!(at(1788373272), "20260902-182112");
        assert_eq!(at(951782400), "20000229-000000"); // leap day, era boundary math
        assert_eq!(at(4102444800), "21000101-000000"); // non-leap century year
    }
}

pub fn run(
    ctx: GpuContext,
    // Owned: the relayout arm mutates the choice's params (Repo's
    // z_wrap_spacing, Repo/Text cluster_mode) before rebuilding the scene
    // (windowed.rs's pending_relayout arm). Offscreen keeps borrowing its own.
    choice: SceneChoice,
    cull: bool,
    ops: &[Op],
    ui: bool,
    shot: Option<(u64, std::path::PathBuf)>,
    present_mode: wgpu::PresentMode,
) {
    // Without the `egui-ui` feature the overlay is compiled out entirely;
    // the flag is accepted (and ignored) so the CLI is identical either way.
    #[cfg(not(feature = "egui-ui"))]
    let _ = ui;
    let event_loop = EventLoop::new().expect("event loop creation failed");
    let mut app = App {
        ctx,
        choice,
        cull,
        ops,
        start: Instant::now(),
        state: None,
        #[cfg(feature = "egui-ui")]
        ui,
        shot,
        present_mode,
    };
    event_loop.run_app(&mut app).expect("event loop error");
}
