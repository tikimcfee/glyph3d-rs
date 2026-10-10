//! The window's state: surface/config, the boxed scene, input bookkeeping,
//! FPS accounting, the egui overlay handle and the per-frame render.
//! Extracted from `windowed.rs` in the 2026-09 code-shape refactor — a pure
//! move; `pub(super)` stands in for the same-module privacy these items had
//! (the app handler constructs and drives every field).

use std::sync::Arc;
use std::time::Instant;

use winit::window::{CursorGrabMode, Window};

use crate::gpu::GpuContext;
use crate::scene::{self, SceneLike};

#[cfg(feature = "egui-ui")]
use super::app::RelayoutRequest;
#[cfg(feature = "egui-ui")]
use super::ui::{EguiUi, PANEL_VERBS};

/// A `[ui]` colour setting (sRGB 0-255) as an egui colour.
#[cfg(feature = "egui-ui")]
fn ui_rgb([r, g, b]: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

pub(super) struct WindowState {
    pub(super) window: Arc<Window>,
    pub(super) surface: wgpu::Surface<'static>,
    pub(super) config: wgpu::SurfaceConfiguration,
    pub(super) depth: wgpu::TextureView,
    pub(super) scene: Box<dyn SceneLike>,
    pub(super) start: Instant,
    /// Stage F/G: mouse-look is active only while the pointer is grabbed
    /// (right button held, or backquote toggle).
    pub(super) grabbed: bool,
    /// True once any DeviceEvent::MouseMotion has arrived during the current
    /// grab. Environments where raw device deltas never arrive (some macOS
    /// trackpad / remote-desktop paths) fall back to CursorMoved deltas;
    /// without the flag both paths would fire and look would double-apply.
    pub(super) saw_device_delta: bool,
    /// Last cursor position, physical px (click picks use it).
    pub(super) cursor: (f32, f32),
    pub(super) last_frame: Instant,
    /// Stage K (K6): frames presented since startup (the `--screenshot-frame`
    /// counter — distinct from `frames`, which the FPS line resets every
    /// second).
    pub(super) frames_total: u64,
    /// Stage K (K6): armed CLI capture — capture the frame that reaches N.
    pub(super) shot_at_frame: Option<(u64, std::path::PathBuf)>,
    /// Stage K (K6): capture THIS frame to the path after the final submit
    /// (armed by F2, or by the CLI frame counter).
    pub(super) capture_pending: Option<std::path::PathBuf>,
    /// Post-L3 fix: set when the surface reports Occluded (fully covered /
    /// display asleep). While set, about_to_wait throttles redraw requests
    /// to ~2 Hz instead of spinning at ~100% CPU (measured pre-fix: ~250k
    /// occlusion-skips/s). winit 0.30 gives no reliable wake on
    /// un-occlusion — WindowEvent::Occluded is iOS-only ("Others:
    /// Unsupported"), and RedrawRequested fires only on OS invalidation or
    /// an explicit request_redraw — so a purely event-driven wait could
    /// strand the window blank; the 500 ms retry guarantees recovery.
    pub(super) occluded: bool,
    /// Last time an occluded retry was issued (the 2 Hz throttle).
    pub(super) occluded_retry_at: Instant,
    // FPS accounting
    pub(super) frames: u32,
    pub(super) fps_window_start: Instant,
    /// Stage H: GPU pass timing accumulator for the once-per-second line
    /// (only fed when GLYPH_PROFILE=1 built a profiler).
    pub(super) profile: crate::gpu::ProfileAccumulator,
    /// Stage K: egui overlay state; `None` under `--no-ui`.
    #[cfg(feature = "egui-ui")]
    pub(super) egui: Option<EguiUi>,
    /// Stage K (K3): debug-UI probe — camera/pick snapshot written by the
    /// scene each frame (None for the demo scene and under --no-ui).
    #[cfg(feature = "egui-ui")]
    pub(super) ui_probe: Option<crate::glyph_scene::UiProbe>,
    /// Stage K (K3): the 1 Hz FPS figure, mirrored for the Debug panel (the
    /// println below stays the primary output — offscreen logs read it).
    #[cfg(feature = "egui-ui")]
    pub(super) ui_fps: f32,
    /// Stage K (K4) dev-only verification hook state (GLYPH_K4_SELFTEST=1):
    /// 0 = off/done, 1 = armed (fires at t>3 s), 2 = fired (print the
    /// after-counters next frame). See AGENTS.md debug env vars.
    #[cfg(feature = "egui-ui")]
    pub(super) k4_selftest: u8,
    /// The relayout signal (repo and text scenes): the panel sets Some(...) on
    /// the z_wrap_spacing slider's drag release or the cluster toggle's click;
    /// the RedrawRequested arm consumes it and rebuilds the scene between
    /// frames. None where no layout params exist (demo/engine-text scenes).
    #[cfg(feature = "egui-ui")]
    pub(super) pending_relayout: Option<RelayoutRequest>,
    /// Dev-only verification hook state (GLYPH_ZSPACE_SELFTEST=1): same
    /// 0/1/2/3 arming as cluster_selftest below, driving the layout dial
    /// through the same pending_relayout arm the slider's release uses.
    #[cfg(feature = "egui-ui")]
    pub(super) zspace_selftest: u8,
    /// Dev-only verification hook state (GLYPH_CLUSTER_SELFTEST=1): same
    /// 0/1/2/3 arming as zspace_selftest, toggling the cluster mode through
    /// the same pending_relayout arm the panel's button uses.
    #[cfg(feature = "egui-ui")]
    pub(super) cluster_selftest: u8,
    /// Dev-only verification hook state (GLYPH_FIELDMODE_SELFTEST=1): at
    /// t≈3 s cycles the field mode Instanced → Derived → Visible through the
    /// same pending_relayout arm the panel's selector fires, printing the
    /// HUD line after each rebuild. 0 = off/done; odd states fire, even
    /// states are the quiet frame after a rebuild (see `render`).
    #[cfg(feature = "egui-ui")]
    pub(super) fieldmode_selftest: u8,
    /// Dev-only verification hook state (GLYPH_VISIBLE_VERB_SELFTEST=1, M3):
    /// at t≈3 s picks a glyph through `apply_pick` (the CLI/click entry) and
    /// applies recolor-glyph, nudge-glyph, set-glyph-background, hide-group
    /// and show-group one every few frames, printing each verb's reply and
    /// the HUD line after it. 0 = off/done; 1 = armed; 2.. = the step to run.
    #[cfg(feature = "egui-ui")]
    pub(super) visible_verb_selftest: u8,
}

/// The `--cam-pose` argument that reproduces a Fly pose: eye, then yaw and
/// pitch in DEGREES (the CLI converts back; `cli::ops`). One formatter for
/// the panel's copy button and the F2 `GLYPH_POSE_PRINT` line, so the two
/// cannot disagree about units.
pub(super) fn cam_pose_arg(eye: [f32; 3], yaw: f32, pitch: f32) -> String {
    format!(
        "--cam-pose {:.3} {:.3} {:.3} {:.3} {:.3}",
        eye[0],
        eye[1],
        eye[2],
        yaw.to_degrees(),
        pitch.to_degrees()
    )
}

/// The field HUD's one-line form: what the field is and what it did this
/// frame. Drawn (as several lines) by the F8 HUD and printed verbatim by the
/// field-mode self-test, so the two read the same numbers.
#[cfg(feature = "egui-ui")]
fn hud_line(snap: &crate::glyph_scene::UiProbeState, fps: f32) -> String {
    let mode = snap.field_mode.map(|m| m.to_string()).unwrap_or_else(|| "n/a".to_string());
    let strategy = snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string());
    let mut line = format!(
        "field {mode} | engine {strategy} | segments {} ({} hidden) | backdrops {} | cull {:.2} ms | fps {fps:.1}",
        snap.segments, snap.hidden_segments, snap.cull_backdrops, snap.cull_cpu_ms
    );
    match &snap.visible_stats {
        Some(s) => {
            line += &format!(
                " | items {}/{} visible, {} backdrop | lines {} candidate: {} glyph, {} wash | segments {} | slots {} ({} dropped) | gpu cull {:.2} layout {:.2} draw {:.2} ms",
                s.items_visible,
                s.items_total,
                s.items_backdrop,
                s.lines_candidate,
                s.lines_glyph,
                s.lines_wash,
                s.segments,
                s.slots,
                s.slots_dropped,
                s.cull_ms,
                s.layout_ms,
                s.draw_ms,
            );
        }
        None => {
            line += &format!(" | instances {} in {} draw ranges", snap.cull_instances, snap.cull_ranges);
        }
    }
    // What is selected and what the verbs address, in the field's own key
    // (M3: `item:start..end` and `file byte N` for the Visible field).
    if let Some(sel) = &snap.selection {
        line += &format!(" | selection {sel}");
    }
    if let Some(pick) = &snap.pick_key {
        line += &format!(" | pick {pick}");
    }
    line
}

#[cfg(feature = "egui-ui")]
fn format_relative_time(dur: std::time::Duration) -> String {
    let secs = dur.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

impl WindowState {
    pub(super) fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
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
    pub(super) fn grab(&mut self) {
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

    pub(super) fn ungrab(&mut self) {
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
    pub(super) fn egui_wants_pointer(&self) -> bool {
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
    pub(super) fn capture_to_png(&self, ctx: &GpuContext, frame: &wgpu::SurfaceTexture, path: &std::path::Path) {
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

    pub(super) fn render(&mut self, ctx: &GpuContext) {
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
                            ..Default::default()
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
                            toggle_cluster: true,
                            ..Default::default()
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
        // Dev-only verification hook (GLYPH_FIELDMODE_SELFTEST=1): cycle the
        // field mode Instanced → Derived → Visible through the SAME
        // pending_relayout arm the panel's selector fires, printing the HUD
        // line after each rebuild. Odd states fire a request (consumed after
        // this render), even states are the quiet frame whose render fills
        // the new probe; the print reads that. Until the Visible field's
        // bodies land, the third step panics at `VisibleField::new` — which
        // is the point of running it: the mode reaches the call.
        #[cfg(feature = "egui-ui")]
        {
            let step_mode = |s: u8| match s {
                1 => Some(glyph_field::GlyphFieldMode::Instanced),
                3 => Some(glyph_field::GlyphFieldMode::Derived),
                5 => Some(glyph_field::GlyphFieldMode::Visible),
                _ => None,
            };
            match self.fieldmode_selftest {
                s @ (1 | 3 | 5) if s > 1 || self.time() > 3.0 => {
                    let has_mode = self.ui_probe.as_ref().and_then(|p| p.borrow().field_mode);
                    match (has_mode, step_mode(s)) {
                        (Some(_), Some(mode)) => {
                            if let Some(probe) = &self.ui_probe {
                                println!("FIELDMODE-SELFTEST before step {}: {}", s.div_ceil(2), hud_line(&probe.borrow(), self.ui_fps));
                            }
                            self.pending_relayout = Some(RelayoutRequest {
                                set_field_mode: Some(mode),
                                ..Default::default()
                            });
                            self.fieldmode_selftest = s + 1;
                        }
                        _ => {
                            println!("FIELDMODE-SELFTEST: scene carries no field mode (no probe) — skipping");
                            self.fieldmode_selftest = 0;
                        }
                    }
                }
                s @ (2 | 4 | 6) => {
                    if let Some(probe) = &self.ui_probe {
                        println!("FIELDMODE-SELFTEST after step {}:  {}", s / 2, hud_line(&probe.borrow(), self.ui_fps));
                    }
                    self.fieldmode_selftest = if s == 6 { 0 } else { s + 1 };
                }
                _ => {}
            }
        }
        // Dev-only verification hook (GLYPH_VISIBLE_VERB_SELFTEST=1, M3): the
        // verbs through the SAME entry points the CLI op stream and the panel
        // use (`apply_pick`, `apply_verb` with `parse_verb`'s literals), one
        // step every fourth frame from t≈3 s so the HUD line printed after
        // each step reads a frame that saw the edit (the Visible field's
        // counters lag a frame or two). The value is the pick:
        // `1` = the first file's row 0 col 0; `file[:row[:col]]` otherwise.
        // Meant for `--field-mode visible` (the replies name item:byte); in a
        // stored mode the same steps run through the slot paths.
        #[cfg(feature = "egui-ui")]
        if self.visible_verb_selftest != 0 && self.time() > 3.0 && self.frames_total.is_multiple_of(4) {
            let step = self.visible_verb_selftest;
            let hud = |state: &Self| state.ui_probe.as_ref().map(|p| hud_line(&p.borrow(), state.ui_fps)).unwrap_or_default();
            if step > 1 {
                println!("VISIBLE-VERB-SELFTEST hud after step {}: {}", step - 1, hud(self));
            }
            let verb_step = |state: &mut Self, spec: &str| {
                let verb = crate::parse_verb(spec).expect("selftest verb literal must parse like the CLI");
                let line = state.scene.apply_verb(ctx, &verb).unwrap_or_else(|| "scene does not support verbs".to_string());
                println!("VISIBLE-VERB-SELFTEST step {step} ({spec}): {line}");
            };
            match step {
                1 => {
                    let spec = std::env::var("GLYPH_VISIBLE_VERB_SELFTEST").unwrap_or_default();
                    let mut parts = spec.split(':');
                    let file = parts.next().filter(|f| *f != "1").unwrap_or("").to_string();
                    let row = parts.next().and_then(|r| r.parse().ok()).unwrap_or(0);
                    let col = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                    let cmd = crate::glyph_scene::PickCommand::RowCol { file, row, col };
                    let line = self.scene.apply_pick(ctx, &cmd).unwrap_or_else(|| "scene does not support picks".to_string());
                    println!("VISIBLE-VERB-SELFTEST step 1 (pick {cmd:?}): {line}");
                }
                2 => verb_step(self, "recolor-glyph"),
                3 => verb_step(self, "nudge-glyph 0.5 0 0"),
                4 => verb_step(self, "set-glyph-background 2050c0"),
                5 => verb_step(self, "hide-group"),
                6 => verb_step(self, "show-group"),
                _ => {}
            }
            self.visible_verb_selftest = if step >= 7 { 0 } else { step + 1 };
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
            *selected_group = self.scene.selected_group_id();
            let session_browser_open = &mut egui.session_browser_open;
            let session_filter_text = &mut egui.session_filter_text;
            let session_filter_harness = &mut egui.session_filter_harness;
            let discovered_sessions = &mut egui.discovered_sessions;
            let session_dirs_report = &mut egui.session_dirs_report;
            let hud_open = egui.hud_open;
            // The layout dial's apply signal: the panel's slider sets it on
            // release; the RedrawRequested arm consumes it and rebuilds.
            let pending_relayout = &mut self.pending_relayout;
            let full_output = egui_ctx.run_ui(raw_input, |root_ui| {
                // The field HUD (F8): an always-on readout, separate from the
                // Debug window, anchored top-left — what the field is and
                // what it did this frame, in the same words the self-test
                // prints (`hud_line`).
                if hud_open {
                    if let Some(snap) = &probe_snap {
                        egui::Area::new(egui::Id::new("field_hud"))
                            .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
                            .order(egui::Order::Foreground)
                            .interactable(false)
                            .show(root_ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    let mode = snap.field_mode.map(|m| m.to_string()).unwrap_or_else(|| "n/a".to_string());
                                    let strategy = snap.strategy.map(|s| s.to_string()).unwrap_or_else(|| "n/a".to_string());
                                    ui.monospace(format!("field {mode} | engine {strategy} | {fps:.1} fps"));
                                    ui.monospace(format!(
                                        "segments {} ({} hidden) | backdrops {} | cull {:.2} ms CPU",
                                        snap.segments, snap.hidden_segments, snap.cull_backdrops, snap.cull_cpu_ms
                                    ));
                                    match &snap.visible_stats {
                                        Some(s) => {
                                            ui.monospace(format!(
                                                "items {}/{} visible, {} backdrop",
                                                s.items_visible, s.items_total, s.items_backdrop
                                            ));
                                            ui.monospace(format!(
                                                "lines {} candidate: {} glyph | {} wash",
                                                s.lines_candidate, s.lines_glyph, s.lines_wash
                                            ));
                                            ui.monospace(format!(
                                                "segments {} | slots {} ({} dropped)",
                                                s.segments, s.slots, s.slots_dropped
                                            ));
                                            ui.monospace(format!(
                                                "gpu: cull {:.2} | layout {:.2} | draw {:.2} ms",
                                                s.cull_ms, s.layout_ms, s.draw_ms
                                            ));
                                        }
                                        None => {
                                            ui.monospace(format!(
                                                "instances {} in {} draw ranges",
                                                snap.cull_instances, snap.cull_ranges
                                            ));
                                        }
                                    }
                                    if snap.debug_tint != 0 {
                                        ui.monospace(format!(
                                            "debug tint: {}",
                                            if snap.debug_tint == 1 { "by LOD tier" } else { "by cull state" }
                                        ));
                                    }
                                    // The selection and the pick in the
                                    // field's own key (M3): what a verb
                                    // would address right now.
                                    if snap.selection.is_some() || snap.pick_key.is_some() {
                                        ui.monospace(format!(
                                            "selection {} | pick {}",
                                            snap.selection.as_deref().unwrap_or("none"),
                                            snap.pick_key.as_deref().unwrap_or("none")
                                        ));
                                    }
                                });
                            });
                    }
                }
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
                    .default_pos(egui::pos2(10.0, 10.0))
                    .default_size(egui::vec2(360.0, 520.0))
                    .min_size(egui::vec2(280.0, 180.0))
                    .resizable(true)
                    .collapsible(true)
                    .vscroll(true)
                    .show(root_ui.ctx(), |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
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
                                    "camera: {mode} eye=({:.2},{:.2},{:.2}) yaw={:.3} pitch={:.3} rad ({:.1}°, {:.1}°)",
                                    snap.eye[0], snap.eye[1], snap.eye[2], snap.yaw, snap.pitch,
                                    snap.yaw.to_degrees(), snap.pitch.to_degrees(),
                                ));
                                // The pose as the argument that reproduces
                                // it: a view seen once becomes a golden
                                // view's command line.
                                if ui.button("copy pose (--cam-pose, degrees)").clicked() {
                                    let arg = cam_pose_arg(snap.eye, snap.yaw, snap.pitch);
                                    println!("{arg}");
                                    ui.ctx().copy_text(arg);
                                }
                                if snap.strategy.is_some() || snap.field_mode.is_some() {
                                    let engine_str = snap
                                        .strategy
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| "n/a".to_string());
                                    let field_str = snap
                                        .field_mode
                                        .map(|m| format!("{m:?}"))
                                        .unwrap_or_else(|| "n/a".to_string());
                                    ui.label(format!("engine: {engine_str} | field: {field_str}"));
                                }
                                ui.label(
                                    snap.last_pick
                                        .as_deref()
                                        .unwrap_or("pick: (none yet — left-click a glyph)"),
                                );
                                if let Some(active_zone) = &snap.active_zone {
                                    ui.label(format!("carrel / zone: {active_zone}"));
                                }
                                if snap.grabbed_zone.is_some() || snap.grabbed_group.is_some() {
                                    ui.colored_label(
                                        ui_rgb(crate::config::settings().ui.grab_active),
                                        format!(
                                            "ACTIVE GRAB: {}",
                                            if let Some(z) = &snap.grabbed_zone {
                                                format!("Zone '{z}' (move mouse to drag, wheel to scale, C to release)")
                                            } else if let Some(g) = snap.grabbed_group {
                                                format!("File group {g} (move mouse to drag, wheel to scale, G to release)")
                                            } else {
                                                "".to_string()
                                            }
                                        ),
                                    );
                                }
                                ui.horizontal(|ui| {
                                    if let Some(z) = &snap.grabbed_zone {
                                        if ui.button(format!("Release Carrel '{z}' (C)")).clicked() {
                                            self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyC, true);
                                        }
                                    } else if snap.active_zone.is_some()
                                        && ui.button("Grab Entire Carrel (C)").clicked()
                                    {
                                        self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyC, true);
                                    }

                                    if let Some(g) = snap.grabbed_group {
                                        if ui.button(format!("Release File #{g} (G)")).clicked() {
                                            self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyG, true);
                                        }
                                    } else if snap.last_pick.is_some()
                                        && ui.button("Grab File (G)").clicked()
                                    {
                                        self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyG, true);
                                    }
                                });
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
                        // K4: live cull/LOD tuning — the sliders write the
                        // shared probe cell; GlyphScene::render applies it to
                        // CullState's Cell and the Params uniform before
                        // culling (same frame). The readouts are the sums
                        // GLYPH_CULL_DEBUG prints.
                        //
                        // C26 (2026-10-10): two ideas, two always-visible
                        // sliders in plain words. Every threshold is "px per
                        // text row" — how many on-screen pixels tall a row
                        // appears — fixed, never camera-adapted. The panel
                        // keeps Text detail >= Show glyphs (>= File rectangle
                        // in visible mode): the handle that moved wins and
                        // the others yield (`UiProbeState::keep_lod_order`),
                        // so glyphs appear fuzzed first and sharpen on
                        // approach. CLI/config values are applied as given.
                        if let (Some(snap), Some(cell)) = (&probe_snap, &self.ui_probe) {
                            ui.separator();
                            ui.label("LOD — live, windowed only (offscreen keeps the [lod] settings); px = on-screen pixels per text row:");
                            let visible = snap.field_mode == Some(glyph_field::GlyphFieldMode::Visible);
                            let mut p = cell.borrow_mut();
                            let detail = ui.add(
                                egui::Slider::new(&mut p.greek_onset_px, 2.0..=32.0)
                                    .suffix(" px")
                                    .text("Text detail — rows this tall get full curve detail; below, glyphs fuzz progressively"),
                            );
                            let fuzz = p.greeking;
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut p.greeking, "fuzz below it (greeking, anti-moiré)");
                                ui.add_enabled_ui(fuzz, |ui| {
                                    ui.selectable_value(&mut p.greek_pure, false, "smooth");
                                    ui.selectable_value(&mut p.greek_pure, true, "pure (hard cut, max FPS)");
                                });
                            });
                            // Range brackets the default (1.0) with ~2 octaves
                            // each way; logarithmic because the threshold is
                            // a perceptual scale.
                            let show = ui.add(
                                egui::Slider::new(&mut p.lod_min_px, 0.25..=16.0)
                                    .logarithmic(true)
                                    .suffix(" px")
                                    .text(if visible {
                                        "Show glyphs — rows this tall become glyphs; below, a line is a wash"
                                    } else {
                                        "Show glyphs — rows this tall become glyphs; below, a file is its rectangle"
                                    }),
                            );
                            // The visible field's second handle, under Show
                            // glyphs: the file-rectangle-vs-line-wash tier.
                            let rect = visible.then(|| {
                                ui.indent("lod_file_rectangle", |ui| {
                                    ui.add(
                                        egui::Slider::new(&mut p.lod_backdrop_px, 0.05..=4.0)
                                            .logarithmic(true)
                                            .suffix(" px")
                                            .text("File rectangle (visible mode) — rows this tall get line washes; below, the whole file is one rectangle"),
                                    )
                                })
                                .inner
                            });
                            if detail.changed() {
                                p.keep_lod_order(crate::glyph_scene::LodHandle::TextDetail);
                            } else if show.changed() {
                                p.keep_lod_order(crate::glyph_scene::LodHandle::ShowGlyphs);
                            } else if rect.is_some_and(|r| r.changed()) {
                                p.keep_lod_order(crate::glyph_scene::LodHandle::FileRectangle);
                            }
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut p.file_backgrounds, "File card backgrounds");
                                ui.color_edit_button_rgba_unmultiplied(&mut p.file_bg_color);
                            });
                            ui.label(format!(
                                "cull: {} draw ranges, {} instances | {} backdrops",
                                snap.cull_ranges, snap.cull_instances, snap.cull_backdrops
                            ));
                            // The visible field's debug tint: a diagnostic
                            // the stored modes' shaders never read.
                            if visible {
                                ui.separator();
                                ui.horizontal(|ui| {
                                    ui.label("debug tint:");
                                    for (mode, label) in [(0u32, "off"), (1, "by LOD tier"), (2, "by cull state")] {
                                        if ui.selectable_label(p.debug_tint == mode, label).clicked() {
                                            p.debug_tint = mode;
                                        }
                                    }
                                });
                            }
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
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                        // Layout engine strategy selection — repo scenes only.
                        if let Some(snap) = &probe_snap {
                            if let Some(current_strategy) = snap.strategy {
                                ui.separator();
                                ui.label(
                                    "layout engine (repo) — click switches backend & rebuilds:",
                                );
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .selectable_label(
                                            current_strategy == crate::repo::Strategy::Hyper,
                                            "Hyper (CPU)",
                                        )
                                        .clicked()
                                        && current_strategy != crate::repo::Strategy::Hyper
                                    {
                                        *pending_relayout = Some(RelayoutRequest {
                                            set_strategy: Some(crate::repo::Strategy::Hyper),
                                            ..Default::default()
                                        });
                                    }
                                    if ui
                                        .selectable_label(
                                            current_strategy == crate::repo::Strategy::Direct,
                                            "Direct",
                                        )
                                        .clicked()
                                        && current_strategy != crate::repo::Strategy::Direct
                                    {
                                        *pending_relayout = Some(RelayoutRequest {
                                            set_strategy: Some(crate::repo::Strategy::Direct),
                                            ..Default::default()
                                        });
                                    }
                                    if ui
                                        .selectable_label(
                                            current_strategy == crate::repo::Strategy::Batched,
                                            "Batch",
                                        )
                                        .clicked()
                                        && current_strategy != crate::repo::Strategy::Batched
                                    {
                                        *pending_relayout = Some(RelayoutRequest {
                                            set_strategy: Some(crate::repo::Strategy::Batched),
                                            ..Default::default()
                                        });
                                    }
                                });
                            }
                        }

                        // Glyph field mode selection: the two stored formats
                        // and the Visible field (no slots; experimental, M2).
                        // The highlighted label is the mode actually BUILT
                        // (the probe reads `field.mode()`), so a lane-limit
                        // fallback shows as Instanced here.
                        if let Some(snap) = &probe_snap {
                            if let Some(current_mode) = snap.field_mode {
                                ui.separator();
                                ui.label(
                                    "glyph field mode — click switches the field & rebuilds (visible: experimental, lays out per frame):",
                                );
                                ui.horizontal(|ui| {
                                    for (mode, label) in [
                                        (glyph_field::GlyphFieldMode::Derived, "Derived (20B)"),
                                        (glyph_field::GlyphFieldMode::Instanced, "Instanced (32B)"),
                                        (glyph_field::GlyphFieldMode::Visible, "Visible (no slots)"),
                                    ] {
                                        if ui.selectable_label(current_mode == mode, label).clicked()
                                            && current_mode != mode
                                        {
                                            *pending_relayout = Some(RelayoutRequest {
                                                set_field_mode: Some(mode),
                                                ..Default::default()
                                            });
                                        }
                                    }
                                });
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
                                        toggle_cluster: true,
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                        // Canvas layout toggle (shelf vs carrel) — repo scenes only.
                        if let Some(snap) = &probe_snap {
                            if let Some(mode) = snap.layout_mode {
                                ui.separator();
                                ui.label(
                                    "canvas layout (F6) — click toggles; the scene rebuilds, \
                                     pick/selection state resets:",
                                );
                                let label = match mode {
                                    crate::repo::RepoLayoutMode::Shelf => "layout mode: SHELF (click for carrel)",
                                    crate::repo::RepoLayoutMode::Carrel => "layout mode: CARREL (click for shelf)",
                                };
                                if ui.button(label).clicked() {
                                    *pending_relayout = Some(RelayoutRequest {
                                        toggle_layout: true,
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("agent sessions (F7):");
                            if ui.button("Browse Sessions 📂").clicked() {
                                *session_browser_open = !*session_browser_open;
                            }
                        });
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
                                                    if *selected_group == Some(row.group_id) {
                                                        *selected_group = None;
                                                        self.scene.apply_pick(
                                                            ctx,
                                                            &crate::glyph_scene::PickCommand::Clear,
                                                        );
                                                    } else {
                                                        *selected_group = Some(row.group_id);
                                                        self.scene.apply_pick(
                                                            ctx,
                                                            &crate::glyph_scene::PickCommand::Group(
                                                                row.group_id,
                                                            ),
                                                        );
                                                    // Front-camera framing
                                                    // (camera_eye_target):
                                                    // fit the AABB, text
                                                    // plane faces +Z.
                                                    let half_h_needed = dynst
                                                        .half[1]
                                                        .max(dynst.half[0] / viewport_aspect);
                                                    let dist = crate::glyph_scene::fit_distance(half_h_needed);
                                                    self.scene.set_cam_pose(
                                                        [dynst.center[0], dynst.center[1], dist],
                                                        0.0,
                                                        0.0,
                                                    );
                                                    }
                                                }
                                            });
                                        }
                                    });
                            }
                        }
                    });

                if let Some(carrel) = probe_snap.as_ref().and_then(|s| s.carrel.as_ref()) {
                    egui::Window::new(format!("Agent Carrel: {}", carrel.session_id))
                        .id(egui::Id::new("agent_carrel_hud"))
                        .default_pos(egui::pos2(20.0, 20.0))
                        .default_size(egui::vec2(380.0, 480.0))
                        .min_size(egui::vec2(280.0, 160.0))
                        .resizable(true)
                        .collapsible(true)
                        .vscroll(true)
                        .show(root_ui.ctx(), |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                            ui.horizontal(|ui| {
                                if ui.button("⏮ Oldest (End)").clicked() {
                                    self.scene.on_key(ctx, winit::keyboard::KeyCode::End, true);
                                }

                                let total_beats = carrel.beat_count.max(carrel.turn_count);

                                if ui.button("◀ Prev (P/←/〔)").clicked() && total_beats > 0 {
                                    self.scene.on_key(ctx, winit::keyboard::KeyCode::BracketLeft, true);
                                }

                                if carrel.beat_count > 0 {
                                    ui.label(egui::RichText::new(format!(
                                        "Beat {} / {} (T{})",
                                        carrel.active_beat + 1,
                                        carrel.beat_count,
                                        carrel.active_turn + 1
                                    )).strong());
                                } else {
                                    ui.label(egui::RichText::new(format!("Turn {} / {}", carrel.active_turn + 1, carrel.turn_count)).strong());
                                }

                                if ui.button("Next (N/→/〕) ▶").clicked() && total_beats > 0 {
                                    self.scene.on_key(ctx, winit::keyboard::KeyCode::BracketRight, true);
                                }

                                if ui.button("Latest ⏭ (Home)").clicked() {
                                    self.scene.on_key(ctx, winit::keyboard::KeyCode::Home, true);
                                }

                                ui.separator();
                                if ui.button("Mode (V)").clicked() {
                                    self.scene.on_key(ctx, winit::keyboard::KeyCode::KeyV, true);
                                }
                                ui.separator();
                                if ui.button("Sessions (F7) 📂").clicked() {
                                    *session_browser_open = !*session_browser_open;
                                }
                            });

                            // Timeline & Window Information
                            ui.separator();
                            let total_beats = carrel.beat_count.max(carrel.turn_count);
                            let (w_old, w_new) = carrel.window_item_range;
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(format!(
                                    "Window: [{}..{}] of {}",
                                    w_old, w_new, total_beats
                                )).strong());
                                if carrel.layout_options.deck_scroll_offset > 0 {
                                    ui.colored_label(
                                        ui_rgb(crate::config::settings().ui.history_back),
                                        format!("(-{} back in history)", carrel.layout_options.deck_scroll_offset),
                                    );
                                } else {
                                    ui.colored_label(
                                        ui_rgb(crate::config::settings().ui.history_live),
                                        "(LIVE / LATEST)",
                                    );
                                }
                            });

                            // Collapsible Sliders for limits and offsets
                            let mut current_opts = carrel.layout_options;
                            let mut opts_changed = false;
                            ui.collapsing("Window Limits & Time Travel", |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Deck Window Limit:");
                                    let resp = ui.add(egui::Slider::new(&mut current_opts.deck_window_limit, 5..=100).text("cards"));
                                    if resp.drag_stopped() || resp.lost_focus() {
                                        opts_changed = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Time Scroll (K):");
                                    let max_k = carrel.max_deck_scroll;
                                    let resp = ui.add(egui::Slider::new(&mut current_opts.deck_scroll_offset, 0..=max_k).text("turns back"));
                                    if resp.drag_stopped() || resp.lost_focus() {
                                        opts_changed = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Desk Revision Limit:");
                                    let resp = ui.add(egui::Slider::new(&mut current_opts.desk_revision_limit, 3..=50).text("revs/file"));
                                    if resp.drag_stopped() || resp.lost_focus() {
                                        opts_changed = true;
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Max File Stacks:");
                                    let resp = ui.add(egui::Slider::new(&mut current_opts.max_file_stacks, 5..=50).text("files"));
                                    if resp.drag_stopped() || resp.lost_focus() {
                                        opts_changed = true;
                                    }
                                });
                            });

                            if opts_changed && current_opts != carrel.layout_options {
                                *pending_relayout = Some(RelayoutRequest {
                                    carrel_options: Some(current_opts),
                                    ..Default::default()
                                });
                            }
                            if !carrel.beat_summary.is_empty() {
                                ui.separator();
                                ui.label(egui::RichText::new("Active Beat:").heading());
                                ui.colored_label(ui_rgb(crate::config::settings().ui.beat_summary), &carrel.beat_summary);
                            }
                            if !carrel.prompt_summary.is_empty() {
                                ui.separator();
                                ui.collapsing("Turn Prompt", |ui| {
                                    ui.label(&carrel.prompt_summary);
                                });
                            }
                            if !carrel.touched_files.is_empty() {
                                ui.separator();
                                ui.collapsing(format!("Workdesk Files ({})", carrel.touched_files.len()), |ui| {
                                    for (path, act, total) in &carrel.touched_files {
                                        ui.horizontal(|ui| {
                                            ui.colored_label(ui_rgb(crate::config::settings().ui.workdesk_bullet), "•");
                                            ui.label(format!("{path} (active: R{act}, total: {total})"));
                                        });
                                    }
                                });
                            }
                        });
                }

                if *session_browser_open {
                    if discovered_sessions.is_none() {
                        let dirs = crate::launch_config::LaunchConfig::load_or_default().session_dirs();
                        *session_dirs_report = glyph_session_dirs::describe(&dirs);
                        *discovered_sessions = Some(crate::agent_transcript::discovery::scan_session_dirs(&dirs));
                    }

                    egui::Window::new("Agent Sessions (F7)")
                        .id(egui::Id::new("agent_session_browser"))
                        .open(session_browser_open)
                        .default_pos(egui::pos2(50.0, 50.0))
                        .default_size(egui::vec2(580.0, 480.0))
                        .min_size(egui::vec2(360.0, 240.0))
                        .resizable(true)
                        .collapsible(true)
                        .show(root_ui.ctx(), |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                            // 1. Controls bar
                            ui.horizontal(|ui| {
                                ui.label("Search:");
                                ui.text_edit_singleline(session_filter_text);
                                if !session_filter_text.is_empty() && ui.button("✖").clicked() {
                                    session_filter_text.clear();
                                }
                                if ui.button("⟳ Refresh").clicked() {
                                    let dirs = crate::launch_config::LaunchConfig::load_or_default().session_dirs();
                                    *session_dirs_report = glyph_session_dirs::describe(&dirs);
                                    *discovered_sessions = Some(crate::agent_transcript::discovery::scan_session_dirs(&dirs));
                                }
                            });

                            // 2. Filter tabs
                            ui.horizontal(|ui| {
                                ui.selectable_value(
                                    session_filter_harness,
                                    crate::agent_transcript::discovery::SessionHarnessFilter::All,
                                    "All",
                                );
                                ui.selectable_value(
                                    session_filter_harness,
                                    crate::agent_transcript::discovery::SessionHarnessFilter::ClaudeCode,
                                    "Claude Code",
                                );
                                ui.selectable_value(
                                    session_filter_harness,
                                    crate::agent_transcript::discovery::SessionHarnessFilter::Antigravity,
                                    "Antigravity",
                                );
                                ui.selectable_value(
                                    session_filter_harness,
                                    crate::agent_transcript::discovery::SessionHarnessFilter::KimiCode,
                                    "Kimi",
                                );
                            });
                            ui.separator();
                            ui.small(format!("Scanning: {session_dirs_report}"));

                            let Some(all_sessions) = discovered_sessions.as_ref() else {
                                ui.label("No sessions scanned yet.");
                                return;
                            };
                            if all_sessions.is_empty() {
                                ui.label(
                                    "No sessions found in those directories. Point claude_projects_dir, \
                                     antigravity_brain_dir or kimi_sessions_dir in launch_config.toml \
                                     elsewhere, or set one to \"\" to skip that app.",
                                );
                                return;
                            }

                            let filter_lower = session_filter_text.to_lowercase();
                            let filtered: Vec<&crate::agent_transcript::discovery::DiscoveredSession> = all_sessions
                                .iter()
                                .filter(|s| {
                                    match *session_filter_harness {
                                        crate::agent_transcript::discovery::SessionHarnessFilter::All => true,
                                        crate::agent_transcript::discovery::SessionHarnessFilter::ClaudeCode => {
                                            s.harness == crate::agent_transcript::types::HarnessKind::ClaudeCode
                                        }
                                        crate::agent_transcript::discovery::SessionHarnessFilter::Antigravity => {
                                            s.harness == crate::agent_transcript::types::HarnessKind::Antigravity
                                        }
                                        crate::agent_transcript::discovery::SessionHarnessFilter::KimiCode => {
                                            s.harness == crate::agent_transcript::types::HarnessKind::KimiCode
                                        }
                                    }
                                })
                                .filter(|s| {
                                    if filter_lower.is_empty() {
                                        return true;
                                    }
                                    s.title.to_lowercase().contains(&filter_lower)
                                        || s.id.to_lowercase().contains(&filter_lower)
                                        || s.project_name.as_deref().unwrap_or("").to_lowercase().contains(&filter_lower)
                                        || s.path.to_string_lossy().to_lowercase().contains(&filter_lower)
                                })
                                .collect();

                            ui.label(format!("Showing {} of {} sessions", filtered.len(), all_sessions.len()));
                            ui.separator();

                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    if filtered.is_empty() {
                                        ui.label("No matching sessions found.");
                                        return;
                                    }
                                    for s in filtered {
                                        ui.group(|ui| {
                                            ui.horizontal(|ui| {
                                                match s.harness {
                                                    crate::agent_transcript::types::HarnessKind::ClaudeCode => {
                                                        ui.colored_label(ui_rgb(crate::config::settings().ui.harness_claude), "[Claude]");
                                                    }
                                                    crate::agent_transcript::types::HarnessKind::Antigravity => {
                                                        ui.colored_label(ui_rgb(crate::config::settings().ui.harness_antigravity), "[Antigravity]");
                                                    }
                                                    crate::agent_transcript::types::HarnessKind::KimiCode => {
                                                        ui.colored_label(ui_rgb(crate::config::settings().ui.harness_kimi), "[Kimi]");
                                                    }
                                                    crate::agent_transcript::types::HarnessKind::Generic => {
                                                        ui.colored_label(ui_rgb(crate::config::settings().ui.harness_generic), "[Agent]");
                                                    }
                                                }
                                                if let Some(proj) = &s.project_name {
                                                    ui.colored_label(ui_rgb(crate::config::settings().ui.project_name), format!("📂 {proj}"));
                                                }
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    if ui.button("▶ Load").clicked() {
                                                        *pending_relayout = Some(RelayoutRequest {
                                                            switch_agent_session: Some(s.path.clone()),
                                                            ..Default::default()
                                                        });
                                                    }
                                                    if let Some(m) = s.modified {
                                                        if let Ok(dur) = std::time::SystemTime::now().duration_since(m) {
                                                            ui.label(egui::RichText::new(format_relative_time(dur)).weak().small());
                                                        }
                                                    }
                                                });
                                            });
                                            ui.label(egui::RichText::new(&s.title).strong());
                                            ui.horizontal(|ui| {
                                                let prefix = crate::agent_transcript::types::truncate_chars(&s.id, 16);
                                                ui.label(egui::RichText::new(format!("ID: {prefix}…")).weak().small());
                                                let kb = (s.file_size_bytes as f64) / 1024.0;
                                                ui.label(egui::RichText::new(format!("{kb:.1} KB")).weak().small());
                                            });
                                        });
                                    }
                                });
                        });
                }
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
    pub(super) fn time(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}
