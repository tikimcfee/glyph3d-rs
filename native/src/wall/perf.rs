//! Window-thread and upload performance probe (`GLYPH_WALL_PERF=1`).
//!
//! Activated by `GLYPH_WALL_PERF=1` (or `GLYPH_WALL_PERF_IDLE_MS=<ms>` /
//! `GLYPH_WALL_PERF_MAX_MS=<ms>`). Collects per-frame CPU durations on the
//! window's thread split into [`super::app`] `App::pump` and `App::draw`
//! (excluding swapchain VSync wait inside `get_current_texture`), summed
//! [`UploadStats`] from [`super::renderer::WallRenderer`], process CPU time,
//! and wall-clock FPS, then prints one `wall-perf:` summary line at exit.
//!
//! When `GLYPH_WALL_PERF_MAX_MS` is set and an unbounded flood wedges the
//! window thread inside `Sessions::pump` past the cap, a lightweight sleeping
//! watchdog records the in-flight `App::pump` stall duration, prints the
//! `wall-perf:` summary with `stalled=1`, and exits cleanly.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::event_loop::{ControlFlow, EventLoopProxy};

use crate::gpu::GpuContext;

use super::app::Wake;
use super::renderer::UploadStats;

/// Default settle window before starting an untouched idle measurement so the
/// opening `wall.fit` camera ease (`EASE_RATE = 14/s`) has snapped home.
const DEFAULT_IDLE_SETTLE_MS: u64 = 1200;
const WATCHDOG_GRACE_MS: u64 = 250;
const MAX_LIVE_SAMPLES: usize = 16_384;

#[derive(Clone, Copy, Debug, Default)]
struct WorstFrame {
    idx: u64,
    total_ns: u64,
    acq_ns: u64,
    render_ns: u64,
    submit_ns: u64,
    pump_ns: u64,
    rows: usize,
}

#[derive(Clone, Copy, Debug)]
enum IdleProbe {
    Off,
    PendingOpen {
        settle_ms: u64,
        idle_ms: u64,
    },
    Settling {
        settle_until: Instant,
        max_settle_until: Instant,
        idle_ms: u64,
    },
    Measuring {
        measure_start: Instant,
        measure_until: Instant,
        start_frames: u64,
        start_cpu_ns: Option<u64>,
    },
    Completed {
        idle_wall: Duration,
        idle_frames: u64,
        idle_cpu_ns: Option<u64>,
    },
}

struct PerfInner {
    opened_at: Option<Instant>,
    opened_cpu_ns: Option<u64>,
    frames_recorded: u64,
    pending_pump_ns: u64,
    total_pump_ns: u64,
    pump_wakes: u64,
    frame_pump_ns: Vec<u64>,
    frame_draw_ns: Vec<u64>,
    frame_wait_ns: Vec<u64>,
    worst: WorstFrame,
    uploads: UploadStats,
    idle: IdleProbe,
}

struct PerfShared {
    epoch: Instant,
    in_pump_marker_ns: AtomicU64,
    pump_calls_started: AtomicU64,
    occluded_skips: AtomicU64,
    cap_reached: AtomicBool,
    printed_summary: AtomicBool,
    inner: Mutex<PerfInner>,
}

impl PerfShared {
    fn rel_ns_plus_one(&self, at: Instant) -> u64 {
        let rel = at
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u128::from(u64::MAX - 1)) as u64;
        rel + 1
    }

    fn inflight_pump_ns(&self) -> Option<u64> {
        let marker = self.in_pump_marker_ns.load(Ordering::Acquire);
        if marker == 0 {
            return None;
        }
        let now_marker = self.rel_ns_plus_one(Instant::now());
        Some(now_marker.saturating_sub(marker))
    }
}

struct OccludedBacking {
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    texture: wgpu::Texture,
}

/// Optional performance accumulator driven from [`super::app`].
pub struct WallPerf {
    enabled: bool,
    max_wall: Option<Duration>,
    occluded_backing: Option<OccludedBacking>,
    shared: Arc<PerfShared>,
}

/// RAII timer over a single `App::pump` call.
pub struct PumpScope {
    shared: Option<Arc<PerfShared>>,
    start: Instant,
}

impl Drop for PumpScope {
    fn drop(&mut self) {
        let Some(shared) = &self.shared else {
            return;
        };
        let elapsed_ns = self
            .start
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        shared.in_pump_marker_ns.store(0, Ordering::Release);
        let mut inner = shared.inner.lock().expect("perf mutex poisoned");
        inner.pending_pump_ns = inner.pending_pump_ns.saturating_add(elapsed_ns);
        inner.total_pump_ns = inner.total_pump_ns.saturating_add(elapsed_ns);
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok())
}

/// Total user + system CPU time consumed by this process across all threads, in nanoseconds.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_cpu_ns() -> Option<u64> {
    #[repr(C)]
    struct Timespec {
        tv_sec: std::os::raw::c_long,
        tv_nsec: std::os::raw::c_long,
    }
    #[cfg(target_os = "macos")]
    const CLOCK_PROCESS_CPUTIME_ID: std::os::raw::c_int = 12;
    #[cfg(target_os = "linux")]
    const CLOCK_PROCESS_CPUTIME_ID: std::os::raw::c_int = 2;

    extern "C" {
        fn clock_gettime(clk_id: std::os::raw::c_int, tp: *mut Timespec) -> std::os::raw::c_int;
    }

    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `CLOCK_PROCESS_CPUTIME_ID` is a valid POSIX process CPU clock id on macOS and
    // Linux, and `&mut ts` is a valid, aligned writable `timespec` pointer for the call.
    let rc = unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    if rc == 0 && ts.tv_sec >= 0 && ts.tv_nsec >= 0 {
        Some(
            (ts.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as u64),
        )
    } else {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cpu_ns() -> Option<u64> {
    None
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Nearest-rank quantile (`q` in `0.0..=1.0`) in milliseconds from a sorted nanosecond slice.
fn sorted_quantile_ms(sorted_ns: &[u64], q: f64) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ns.len() as f64 * q).ceil() as usize)
        .saturating_sub(1)
        .min(sorted_ns.len() - 1);
    ns_to_ms(sorted_ns[idx])
}

fn distribution_ms(samples_ns: &[u64]) -> (f64, f64, f64) {
    if samples_ns.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sorted = samples_ns.to_vec();
    sorted.sort_unstable();
    let p50 = sorted_quantile_ms(&sorted, 0.50);
    let p95 = sorted_quantile_ms(&sorted, 0.95);
    let max = ns_to_ms(*sorted.last().expect("non-empty"));
    (p50, p95, max)
}

fn merge_deadline(a: Option<Instant>, b: Option<Instant>) -> ControlFlow {
    match (a, b) {
        (Some(x), Some(y)) => ControlFlow::WaitUntil(x.min(y)),
        (Some(x), None) | (None, Some(x)) => ControlFlow::WaitUntil(x),
        (None, None) => ControlFlow::Wait,
    }
}

fn format_summary_from_inner(
    inner: &PerfInner,
    frames: u64,
    wall: Duration,
    proc_cpu_ns: Option<u64>,
    inflight_pump_ns: Option<u64>,
    pump_calls_started: u64,
    occluded_skips: u64,
) -> String {
    let stalled = u32::from(inflight_pump_ns.is_some());
    let extra_pump_ns = inflight_pump_ns.unwrap_or(0);
    let mut pump_samples = inner.frame_pump_ns.clone();
    let trailing = inner.pending_pump_ns.saturating_add(extra_pump_ns);
    if trailing > 0 && (stalled != 0 || pump_samples.is_empty()) {
        pump_samples.push(trailing);
    }

    let wall_s = wall.as_secs_f64();
    let wall_ms = wall_s * 1000.0;
    let fps = if wall_s > 0.0 {
        frames as f64 / wall_s
    } else {
        0.0
    };
    let (pump_p50, pump_p95, pump_max) = distribution_ms(&pump_samples);
    let (draw_p50, draw_p95, draw_max) = distribution_ms(&inner.frame_draw_ns);
    let (wait_p50, wait_p95, wait_max) = distribution_ms(&inner.frame_wait_ns);
    let pump_sum_ms = ns_to_ms(inner.total_pump_ns.saturating_add(extra_pump_ns));
    let draw_sum_ms = ns_to_ms(inner.frame_draw_ns.iter().copied().sum());
    let wait_sum_ms = ns_to_ms(inner.frame_wait_ns.iter().copied().sum());
    let _ = pump_calls_started;
    let wakes = inner.pump_wakes;
    let wake_hz = if wall_s > 0.0 {
        wakes as f64 / wall_s
    } else {
        0.0
    };
    let cpu_field = proc_cpu_ns.map_or_else(|| "na".to_owned(), |ns| format!("{:.3}", ns_to_ms(ns)));
    let w = inner.worst;

    let mut line = format!(
        "wall-perf: frames={frames} wall_ms={wall_ms:.2} fps={fps:.2} \
         pump_ms=p50:{pump_p50:.3},p95:{pump_p95:.3},max:{pump_max:.3},sum:{pump_sum_ms:.2},wakes:{wakes},wake_hz:{wake_hz:.1} \
         draw_ms=p50:{draw_p50:.3},p95:{draw_p95:.3},max:{draw_max:.3},sum:{draw_sum_ms:.2} \
         wait_ms=p50:{wait_p50:.3},p95:{wait_p95:.3},max:{wait_max:.3},sum:{wait_sum_ms:.2} \
         worst_ms=total:{:.3},idx:{},acq:{:.3},render:{:.3},submit:{:.3},pump:{:.3},rows:{} \
         proc_cpu_ms={cpu_field} \
         uploads=drawn:{},uploaded:{},rows:{},writes:{} \
         stalled={stalled} occluded={occluded_skips}",
        ns_to_ms(w.total_ns),
        w.idx,
        ns_to_ms(w.acq_ns),
        ns_to_ms(w.render_ns),
        ns_to_ms(w.submit_ns),
        ns_to_ms(w.pump_ns),
        w.rows,
        inner.uploads.terminals_drawn,
        inner.uploads.terminals_uploaded,
        inner.uploads.rows_uploaded,
        inner.uploads.write_calls,
    );

    if let IdleProbe::Completed {
        idle_wall,
        idle_frames,
        idle_cpu_ns,
    } = inner.idle
    {
        let idle_ms = idle_wall.as_secs_f64() * 1000.0;
        let idle_cpu =
            idle_cpu_ns.map_or_else(|| "na".to_owned(), |ns| format!("{:.3}", ns_to_ms(ns)));
        line.push_str(&format!(
            " idle_ms={idle_ms:.2} idle_frames={idle_frames} idle_cpu_ms={idle_cpu}"
        ));
    }

    line
}

impl WallPerf {
    /// Configure from `GLYPH_WALL_PERF`, `GLYPH_WALL_PERF_MAX_MS`,
    /// `GLYPH_WALL_PERF_IDLE_MS`, and `GLYPH_WALL_PERF_SETTLE_MS`.
    pub fn from_env() -> Self {
        let idle_ms = env_u64("GLYPH_WALL_PERF_IDLE_MS").filter(|&ms| ms > 0);
        let settle_ms = env_u64("GLYPH_WALL_PERF_SETTLE_MS").unwrap_or(DEFAULT_IDLE_SETTLE_MS);
        let max_wall = env_u64("GLYPH_WALL_PERF_MAX_MS")
            .filter(|&ms| ms > 0)
            .map(Duration::from_millis);
        let enabled = env_truthy("GLYPH_WALL_PERF") || idle_ms.is_some() || max_wall.is_some();
        let idle = match idle_ms {
            Some(ms) => IdleProbe::PendingOpen {
                settle_ms,
                idle_ms: ms,
            },
            None => IdleProbe::Off,
        };
        Self {
            enabled,
            max_wall,
            occluded_backing: None,
            shared: Arc::new(PerfShared {
                epoch: Instant::now(),
                in_pump_marker_ns: AtomicU64::new(0),
                pump_calls_started: AtomicU64::new(0),
                occluded_skips: AtomicU64::new(0),
                cap_reached: AtomicBool::new(false),
                printed_summary: AtomicBool::new(false),
                inner: Mutex::new(PerfInner {
                    opened_at: None,
                    opened_cpu_ns: None,
                    frames_recorded: 0,
                    pending_pump_ns: 0,
                    total_pump_ns: 0,
                    pump_wakes: 0,
                    frame_pump_ns: Vec::new(),
                    frame_draw_ns: Vec::new(),
                    frame_wait_ns: Vec::new(),
                    worst: WorstFrame::default(),
                    uploads: UploadStats::default(),
                    idle,
                }),
            }),
        }
    }

    /// Whether `GLYPH_WALL_PERF` collection is active for this process.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Drop any offscreen GPU texture before the window's surface/device tears down.
    pub fn clear_gpu(&mut self) {
        self.occluded_backing = None;
    }

    /// Record when the window, renderer, and initial terminals have opened.
    pub fn mark_opened(&mut self, proxy: &EventLoopProxy<Wake>) {
        let now = Instant::now();
        {
            let mut inner = self.shared.inner.lock().expect("perf mutex poisoned");
            inner.opened_at = Some(now);
            inner.opened_cpu_ns = process_cpu_ns();
            if let IdleProbe::PendingOpen { settle_ms, idle_ms } = inner.idle {
                let settle_until = now + Duration::from_millis(settle_ms);
                inner.idle = IdleProbe::Settling {
                    settle_until,
                    max_settle_until: settle_until + Duration::from_millis(2000),
                    idle_ms,
                };
            }
        }
        if !self.enabled {
            return;
        }

        if let Some(cap) = self.max_wall {
            let shared = Arc::clone(&self.shared);
            let proxy = proxy.clone();
            std::thread::Builder::new()
                .name("wall-perf-cap".to_owned())
                .spawn(move || {
                    std::thread::sleep(cap);
                    if shared.printed_summary.load(Ordering::Acquire) {
                        return;
                    }
                    shared.cap_reached.store(true, Ordering::Release);
                    let _ = proxy.send_event(Wake);
                    std::thread::sleep(Duration::from_millis(WATCHDOG_GRACE_MS));
                    if shared.printed_summary.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    let inflight = shared.inflight_pump_ns();
                    let started = shared.pump_calls_started.load(Ordering::Acquire);
                    let occluded = shared.occluded_skips.load(Ordering::Acquire);
                    let inner = shared.inner.lock().expect("perf mutex poisoned");
                    let frames = inner.frames_recorded;
                    let wall = inner.opened_at.map_or(cap, |t| t.elapsed());
                    let proc_cpu = process_cpu_ns()
                        .zip(inner.opened_cpu_ns)
                        .map(|(end, start)| end.saturating_sub(start));
                    let summary = format_summary_from_inner(
                        &inner, frames, wall, proc_cpu, inflight, started, occluded,
                    );
                    drop(inner);
                    println!("the wall drew {frames} frames");
                    println!("{summary}");
                    let _ = std::io::stdout().flush();
                    std::process::exit(0);
                })
                .expect("spawn wall-perf-cap thread");
        }
    }

    /// True when the loop is already exiting or `GLYPH_WALL_PERF_MAX_MS` has elapsed.
    pub fn should_exit_on_wake(&self, already_exiting: bool) -> bool {
        if !self.enabled {
            return false;
        }
        already_exiting || self.shared.cap_reached.load(Ordering::Acquire)
    }

    /// When `GLYPH_WALL_PERF` is active and the window surface returns
    /// `Occluded | Timeout` (e.g. macOS lock screen or background Space),
    /// return a surface-matching offscreen `TextureView` so `App::draw` and
    /// `WallRenderer::render` still execute and are measured.
    pub fn occluded_view(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Option<wgpu::TextureView> {
        if !self.enabled {
            return None;
        }
        self.shared.occluded_skips.fetch_add(1, Ordering::Relaxed);
        let w = width.max(1);
        let h = height.max(1);
        let needs_alloc = self.occluded_backing.as_ref().is_none_or(|b| {
            b.format != format || b.width != w || b.height != h
        });
        if needs_alloc {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("wall-perf occluded target"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            self.occluded_backing = Some(OccludedBacking {
                format,
                width: w,
                height: h,
                texture,
            });
        }
        Some(
            self.occluded_backing
                .as_ref()
                .expect("occluded backing initialized")
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
        )
    }

    /// Record a single `EventLoop` `Wake` event delivered to `user_event`.
    pub fn record_wake(&self) {
        let mut inner = self.shared.inner.lock().expect("perf mutex poisoned");
        inner.pump_wakes += 1;
    }

    /// Begin timing a single `App::pump` call on the window's thread.
    pub fn pump_scope(&self) -> PumpScope {
        let start = Instant::now();
        if self.enabled {
            let marker = self.shared.rel_ns_plus_one(start);
            self.shared
                .in_pump_marker_ns
                .store(marker, Ordering::Release);
        }
        self.shared
            .pump_calls_started
            .fetch_add(1, Ordering::Relaxed);
        PumpScope {
            shared: Some(Arc::clone(&self.shared)),
            start,
        }
    }

    /// Submit `encoder`, present `frame` (or wait on GPU completion when rendering
    /// to the occluded fallback target), and record `App::draw` CPU + wait timings.
    pub fn submit_and_record(
        &mut self,
        ctx: &GpuContext,
        encoder: wgpu::CommandEncoder,
        frame: Option<wgpu::SurfaceTexture>,
        timings: (Instant, Instant, Instant),
        upload: UploadStats,
    ) {
        let t_before_submit = Instant::now();
        let sub = ctx.queue.submit([encoder.finish()]);
        let (t_draw_start, t_acquire, t_post_acquire) = timings;
        let (t_cpu_done, extra_wait_ns) = if let Some(f) = frame {
            ctx.queue.present(f);
            (Instant::now(), 0u64)
        } else if self.enabled {
            let cpu_done = Instant::now();
            glyph_field::wait_for(
                &ctx.device,
                Some(sub),
                crate::config::settings().gpu.wait_limit(),
                "wall-perf occluded frame",
            );
            let gpu_wait = cpu_done
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64;
            (cpu_done, gpu_wait)
        } else {
            (Instant::now(), 0u64)
        };

        let pre_ns = t_acquire
            .saturating_duration_since(t_draw_start)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let acq_wait_ns = t_post_acquire
            .saturating_duration_since(t_acquire)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let post_render_ns = t_before_submit
            .saturating_duration_since(t_post_acquire)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let submit_ns = t_cpu_done
            .saturating_duration_since(t_before_submit)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let render_ns = pre_ns.saturating_add(post_render_ns);
        let draw_ns = render_ns.saturating_add(submit_ns);
        let wait_ns = acq_wait_ns.saturating_add(extra_wait_ns);

        let mut inner = self.shared.inner.lock().expect("perf mutex poisoned");
        if !self.enabled && inner.frame_draw_ns.len() >= MAX_LIVE_SAMPLES {
            let half = MAX_LIVE_SAMPLES / 2;
            inner.frame_pump_ns.drain(..half);
            inner.frame_draw_ns.drain(..half);
            inner.frame_wait_ns.drain(..half);
        }
        let pump_ns = inner.pending_pump_ns;
        inner.pending_pump_ns = 0;
        let frame_idx = inner.frames_recorded;
        inner.frame_pump_ns.push(pump_ns);
        inner.frame_draw_ns.push(draw_ns);
        inner.frame_wait_ns.push(wait_ns);
        inner.frames_recorded += 1;

        let total_ns = pump_ns.saturating_add(draw_ns).saturating_add(wait_ns);
        if total_ns >= inner.worst.total_ns {
            inner.worst = WorstFrame {
                idx: frame_idx,
                total_ns,
                acq_ns: wait_ns,
                render_ns,
                submit_ns,
                pump_ns,
                rows: upload.rows_uploaded,
            };
        }

        inner.uploads.terminals_drawn += upload.terminals_drawn;
        inner.uploads.terminals_uploaded += upload.terminals_uploaded;
        inner.uploads.rows_uploaded += upload.rows_uploaded;
        inner.uploads.write_calls += upload.write_calls;
    }

    /// Answer `wallctl app.perf [reset]` over the current accumulated window.
    pub fn control_summary(&self, reset: bool) -> String {
        let started = self.shared.pump_calls_started.load(Ordering::Acquire);
        let occluded = if reset {
            self.shared.occluded_skips.swap(0, Ordering::AcqRel)
        } else {
            self.shared.occluded_skips.load(Ordering::Acquire)
        };
        let mut inner = self.shared.inner.lock().expect("perf mutex poisoned");
        let now = Instant::now();
        let wall = inner.opened_at.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        let now_cpu = process_cpu_ns();
        let proc_cpu_ns = now_cpu
            .zip(inner.opened_cpu_ns)
            .map(|(end, start)| end.saturating_sub(start));
        let frames = inner.frames_recorded;
        let out = format_summary_from_inner(&inner, frames, wall, proc_cpu_ns, None, started, occluded);
        if reset {
            inner.opened_at = Some(now);
            inner.opened_cpu_ns = now_cpu;
            inner.frames_recorded = 0;
            inner.pending_pump_ns = 0;
            inner.total_pump_ns = 0;
            inner.pump_wakes = 0;
            inner.frame_pump_ns.clear();
            inner.frame_draw_ns.clear();
            inner.frame_wait_ns.clear();
            inner.worst = WorstFrame::default();
            inner.uploads = UploadStats::default();
        }
        out
    }

    /// Advance optional wall-cap / idle-window timers and merge with `ui_wake_at`.
    pub fn control_flow(
        &mut self,
        current_frames: u64,
        animating: bool,
        ui_wake_at: Option<Instant>,
    ) -> (bool, ControlFlow) {
        if !self.enabled {
            return (false, merge_deadline(ui_wake_at, None));
        }
        if self.shared.cap_reached.load(Ordering::Acquire) {
            return (true, ControlFlow::Wait);
        }
        let mut inner = self.shared.inner.lock().expect("perf mutex poisoned");
        if let (Some(cap), Some(t0)) = (self.max_wall, inner.opened_at) {
            if t0.elapsed() >= cap {
                self.shared.cap_reached.store(true, Ordering::Release);
                return (true, ControlFlow::Wait);
            }
        }
        let perf_wake_at = match inner.idle {
            IdleProbe::Off | IdleProbe::PendingOpen { .. } => None,
            IdleProbe::Settling {
                settle_until,
                max_settle_until,
                idle_ms,
            } => {
                let now = Instant::now();
                if now < settle_until {
                    Some(settle_until)
                } else if animating && now < max_settle_until {
                    Some(max_settle_until.min(now + Duration::from_millis(50)))
                } else {
                    let measure_until = now + Duration::from_millis(idle_ms);
                    inner.idle = IdleProbe::Measuring {
                        measure_start: now,
                        measure_until,
                        start_frames: current_frames,
                        start_cpu_ns: process_cpu_ns(),
                    };
                    Some(measure_until)
                }
            }
            IdleProbe::Measuring {
                measure_start,
                measure_until,
                start_frames,
                start_cpu_ns,
            } => {
                let now = Instant::now();
                if now < measure_until {
                    Some(measure_until)
                } else {
                    let idle_wall = now.saturating_duration_since(measure_start);
                    let idle_frames = current_frames.saturating_sub(start_frames);
                    let idle_cpu_ns = process_cpu_ns()
                        .zip(start_cpu_ns)
                        .map(|(end, start)| end.saturating_sub(start));
                    inner.idle = IdleProbe::Completed {
                        idle_wall,
                        idle_frames,
                        idle_cpu_ns,
                    };
                    return (true, ControlFlow::Wait);
                }
            }
            IdleProbe::Completed { .. } => return (true, ControlFlow::Wait),
        };
        let cap_wake_at = match (self.max_wall, inner.opened_at) {
            (Some(cap), Some(t0)) => Some(t0 + cap),
            _ => None,
        };
        let combined_perf = match (perf_wake_at, cap_wake_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        (false, merge_deadline(ui_wake_at, combined_perf))
    }

    /// Print the single summary line if `GLYPH_WALL_PERF` was enabled.
    pub fn print_summary(&self, frames: u64) {
        if !self.enabled {
            return;
        }
        if self.shared.printed_summary.swap(true, Ordering::AcqRel) {
            return;
        }
        let started = self.shared.pump_calls_started.load(Ordering::Acquire);
        let occluded = self.shared.occluded_skips.load(Ordering::Acquire);
        let inner = self.shared.inner.lock().expect("perf mutex poisoned");
        let wall = inner.opened_at.map_or(Duration::ZERO, |t| t.elapsed());
        let proc_cpu_ns = process_cpu_ns()
            .zip(inner.opened_cpu_ns)
            .map(|(end, start)| end.saturating_sub(start));
        println!(
            "{}",
            format_summary_from_inner(&inner, frames, wall, proc_cpu_ns, None, started, occluded)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_summary_format_round_trip() {
        let mut inner = PerfInner {
            opened_at: None,
            opened_cpu_ns: None,
            frames_recorded: 20,
            pending_pump_ns: 0,
            total_pump_ns: 0,
            pump_wakes: 0,
            frame_pump_ns: Vec::new(),
            frame_draw_ns: Vec::new(),
            frame_wait_ns: Vec::new(),
            worst: WorstFrame {
                idx: 19,
                total_ns: 18_000_000,
                acq_ns: 15_000_000,
                render_ns: 800_000,
                submit_ns: 200_000,
                pump_ns: 2_000_000,
                rows: 48,
            },
            uploads: UploadStats::default(),
            idle: IdleProbe::Completed {
                idle_wall: Duration::from_millis(5000),
                idle_frames: 0,
                idle_cpu_ns: Some(80_000),
            },
        };

        for step in 1u64..=20 {
            let pump_ns = step * 100_000;
            inner.total_pump_ns += pump_ns;
            inner.pump_wakes += 1;
            inner.frame_pump_ns.push(pump_ns);
            inner.frame_draw_ns.push(step * 50_000);
            inner.frame_wait_ns.push(15_000_000);
            inner.uploads.terminals_drawn += 4;
            inner.uploads.terminals_uploaded += 2;
            inner.uploads.rows_uploaded += 48;
            inner.uploads.write_calls += 2;
        }

        let summary = format_summary_from_inner(
            &inner,
            20,
            Duration::from_secs(1),
            Some(42_500_000),
            None,
            20,
            0,
        );
        assert!(summary.starts_with("wall-perf: frames=20 wall_ms=1000.00 fps=20.00 "));
        assert!(summary.contains("pump_ms=p50:1.000,p95:1.900,max:2.000,sum:21.00,wakes:20,wake_hz:20.0"));
        assert!(summary.contains("draw_ms=p50:0.500,p95:0.950,max:1.000,sum:10.50"));
        assert!(summary.contains("wait_ms=p50:15.000,p95:15.000,max:15.000,sum:300.00"));
        assert!(summary.contains("worst_ms=total:18.000,idx:19,acq:15.000,render:0.800,submit:0.200,pump:2.000,rows:48"));
        assert!(summary.contains("proc_cpu_ms=42.500"));
        assert!(summary.contains("uploads=drawn:80,uploaded:40,rows:960,writes:40"));
        assert!(summary.contains("stalled=0 occluded=0"));
        assert!(summary.contains("idle_ms=5000.00 idle_frames=0 idle_cpu_ms=0.080"));

        let stalled = format_summary_from_inner(
            &inner,
            1,
            Duration::from_secs(4),
            Some(4_100_000_000),
            Some(3_990_000_000),
            21,
            3,
        );
        assert!(stalled.contains("stalled=1 occluded=3"));
        assert!(stalled.contains("max:3990.000"));
    }

    #[test]
    fn process_cpu_clock_is_monotonic_on_supported_unix() {
        if let Some(t0) = process_cpu_ns() {
            let mut acc = 0u64;
            for i in 0..10_000u64 {
                acc = acc.wrapping_add(i.wrapping_mul(31));
            }
            std::hint::black_box(acc);
            let t1 = process_cpu_ns().expect("second clock_gettime should succeed");
            assert!(t1 >= t0);
        }
    }
}
