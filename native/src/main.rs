//! glyph3d-native — native GPU port of the glyph3d-js code-visualization system.
//!
//! Stage A: windowed/offscreen shell + 1M-instance quad-field stress demo.
//! Stage C: Slug analytic-coverage glyph renderer (atlas loader, WGSL Slug
//!          pipeline, text staging) behind `--render-file` (the default scene).
//! Stage F: fly camera (windowed) + per-file CPU frustum/LOD culling with
//!          far-LOD backdrop quads — the repo field is interactive.
//!          `--no-cull` keeps the legacy full-field draws for A/B.
//! Stage G: CPU picking + live manipulation. `--pick-file/--pick-row/
//!          --pick-col/--pick-px` resolve file → row/col → char; `--verb`
//!          edits instances (recolor/nudge/scale glyph, recolor line) and
//!          groups (move/scale/tint/hide) with partial buffer uploads.
//!          Windowed: left click picks, h/g/t/x verbs, right-drag look.
//! Stage H: clap CLI (parity-tested), naga WGSL validation in `cargo test`,
//!          opt-in wgpu-profiler pass timings (GLYPH_PROFILE=1), encase
//!          layout assertions for the hand-mirrored WGSL lane maps.
//! Stage I: glam 0.30 → 0.33 (byte-identical under the full A/B suite);
//!          baseline views moved onto the immutable fixtures/baseline-view.txt.
//! Stage K: egui 0.36 overlay on the windowed renderer (K1: deps + plumbing
//!          with an empty UI; `--no-ui` gives exact pre-K windowed behavior).
//!
//! Run modes:
//!   (default) `[--render-file <path>] [--copies N]`
//!                                    windowed: text field, orbiting camera, FPS log.
//!   --demo                           windowed: Stage A quad-field demo.
//!   --screenshot <path.png> [--frames N] [--zoom F] [--render-file P] [--copies N] [--demo]
//!                                    offscreen: render N frames, write PNG, print
//!                                    timing, exit 0. Deterministic (fixed virtual clock).
//!
//! This file is the CLI SHELL (clap parsing, run-mode dispatch, help); the
//! renderer lives in the library — `src/lib.rs`.

use clap::CommandFactory;
use glyph3d_native::cli::{
    parse_cli, Cli, CliCommand, FixtureTask, GpuInfoMode, RenderTarget,
};
use glyph3d_native::*;

/// Parse the CLI and dispatch one run mode: a render (windowed or offscreen),
/// a repo scan, or one of the exit-driver instruments (fixture, GPU info).
fn main() {
    // Unified logging & tracing substrate.
    // tracing-log automatically captures all log::* records and routes them into tracing.
    // Filtering precedence:
    // 1. GLYPH_TRACE (tracing-specific filter, e.g. "glyph3d_native=info,chain=debug")
    // 2. RUST_LOG (standard environment logging filter)
    // 3. Default: "glyph3d_native=info,wgpu=info,naga=warn"
    // (wgpu info is retained for hardware initialization diagnostics across machines).
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("GLYPH_TRACE")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_env("RUST_LOG"))
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("glyph3d_native=info,wgpu=info,naga=warn")
        });

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            Some(*s)
        } else {
            info.payload().downcast_ref::<String>().map(|s| s.as_str())
        };
        if let Some(msg) = payload {
            if msg.contains("Broken pipe") || msg.contains("os error 32") {
                std::process::exit(0);
            }
        }
        default_hook(info);
    }));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();
    let cli = parse_cli();

    match cli.action() {
        CliCommand::GenerateCompletion { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "glyph3d-native", &mut std::io::stdout());
        }
        CliCommand::GpuInfo(mode) => {
            let ctx = pollster::block_on(gpu::init(None));
            match mode {
                GpuInfoMode::Key => println!("{}", ctx.profile.key()),
                GpuInfoMode::Profile => print!("{}", ctx.profile.render_text()),
            }
        }
        CliCommand::Fixture(task) => match task {
            FixtureTask::Reference(paths) => fixture::run_fixture_reference(&paths),
            FixtureTask::Trie(paths) => fixture::run_fixture_trie(&paths),
            FixtureTask::Fold(paths) => fixture::run_fixture_fold(&paths),
            FixtureTask::Scan(paths) => scan::run_fixture_scan(&paths),
            FixtureTask::Bake(paths) => bake::run_fixture_bake(&paths),
            FixtureTask::HyperOracle(paths, mode) => hyper_oracle::run_hyper_oracle_check(&paths, mode),
            FixtureTask::LineTableStats(paths, mode) => {
                glyph3d_native::layout_hyper::line_table::run_line_table_stats(&paths, mode)
            }
        },
        CliCommand::RepoScanOnly {
            dir,
            params,
            strategy,
            verify,
        } => {
            let load = repo::load_repo(
                &dir,
                &params,
                strategy,
                verify,
            );
            load.print_stats();
        }
        CliCommand::Render(plan) => {
            let prefetched_walk = if let SceneChoice::Repo {
                dir,
                strategy,
                wrap_mode,
                z_wrap_spacing,
                cluster_mode,
                layout_mode,
                color_mode,
                ..
            } = &plan.choice {
                let dir = dir.clone();
                let strategy = *strategy;
                let params = repo::RepoParams {
                    wrap_mode: *wrap_mode,
                    z_wrap_spacing: *z_wrap_spacing,
                    cluster_mode: *cluster_mode,
                    layout_mode: *layout_mode,
                    color_mode: *color_mode,
                    field_mode: plan.cull_opts.field_mode,
                    ..Default::default()
                };
                Some(std::thread::spawn(move || repo::prefetch_repo(&dir, params, strategy)))
            } else {
                None
            };

            let ctx = pollster::block_on(gpu::init(None));
            *ctx.prefetched_walk.lock().unwrap_or_else(|e| e.into_inner()) = prefetched_walk;

            let emoji_sheet_path = match &plan.choice {
                SceneChoice::Repo { emoji_sheet, .. } => Some(emoji_sheet.clone()),
                SceneChoice::Text { emoji_sheet, .. } => Some(emoji_sheet.clone()),
                SceneChoice::EngineText { emoji_sheet, .. } => Some(emoji_sheet.clone()),
                SceneChoice::AgentSession { emoji_sheet, .. } => Some(emoji_sheet.clone()),
                _ => None,
            };
            if let Some(emoji_sheet) = emoji_sheet_path {
                let dev = ctx.device.clone();
                let q = ctx.queue.clone();
                let atlas_handle = std::thread::spawn(move || {
                    let t = std::time::Instant::now();
                    let a = atlas::Atlas::load_device(&dev, &q, &emoji_sheet);
                    log::info!("prefetched atlas loaded in {:?}", t.elapsed());
                    a
                });
                *ctx.prefetched_atlas.lock().unwrap_or_else(|e| e.into_inner()) = Some(atlas_handle);
            }

            match plan.target {
                RenderTarget::Offscreen { path, frames, zoom } => {
                    offscreen::run(
                        &ctx,
                        &plan.choice,
                        &path,
                        frames,
                        zoom,
                        plan.cull_opts,
                        &plan.ops,
                    );
                }
                RenderTarget::Windowed {
                    no_ui,
                    shot,
                    present_mode,
                    frames,
                } => {
                    windowed::run(
                        ctx,
                        plan.choice,
                        plan.cull_opts,
                        &plan.ops,
                        !no_ui,
                        shot,
                        present_mode.into(),
                        None,
                        frames,
                    );
                }
            }
        }
    }
}
