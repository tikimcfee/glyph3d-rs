//! glyph3d-native — the glyph3d renderer: source text, whole repositories of
//! it, laid out in 3D and drawn on the GPU with analytic (Slug) glyph
//! coverage.
//!
//! Run modes (`--help` has every flag; `cargo glyph run` builds first):
//!   (no arguments, on a terminal) / `--launcher`
//!                    the terminal launcher (`launcher/`): pick a scene and
//!                    its options, Enter starts the renderer on them.
//!   `--load-repo DIR`  a repository as a field of files (HyperLayout;
//!                    `--field-mode instanced|derived|visible`).
//!   `--render-file P`  one text file; `--agent-session P` an agent transcript;
//!                    `--demo` the quad-field stress demo.
//!   `--screenshot PATH [--frames N]`
//!                    offscreen: render, write a PNG, exit. Deterministic (a
//!                    fixed virtual clock), which is what the golden views
//!                    rest on.
//!   check instruments  `--fixture-*`, `--hyper-oracle-check`, `--repo-verify`,
//!                    `--gpu-key`, ...: run by the `cargo glyph` gates.
//!
//! This file is the CLI SHELL (clap parsing, run-mode dispatch, help); the
//! renderer lives in the library — `src/lib.rs`.

use clap::CommandFactory;
use glyph3d_native::cli::{
    parse_cli_from, Cli, CliCommand, FixtureTask, GpuInfoMode, RenderTarget,
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
    // The launcher before the config merge: it loads the file itself and
    // shows a bad one on its status line instead of exiting on it.
    let matches = Cli::command().get_matches();
    #[cfg(feature = "launcher")]
    if launcher::wanted(&matches) {
        std::process::exit(launcher::run(matches.get_one::<std::path::PathBuf>("launch_config").cloned()));
    }
    #[cfg(not(feature = "launcher"))]
    if matches.get_flag("launcher") {
        eprintln!("error: --launcher: this build has no launcher (the `launcher` feature is off)");
        std::process::exit(1);
    }
    let cli = parse_cli_from(matches).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

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
