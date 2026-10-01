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

use clap::CommandFactory;
use glyph3d_native::cli::{
    default_text_file, parse_cli, parse_cluster_mode, parse_present_mode, parse_strategy,
    parse_wrap_mode, Cli,
};
use glyph3d_native::*;

/// Stage D: drive the Mojo glyph engine in-process over one file.
/// Prints slot count, per-load timing, throughput, and the first records.
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // The load path's span instrument (note 22). Span-CLOSE lines carry the
    // elapsed time; GLYPH_TRACE (fallback RUST_LOG) names the filter, unset
    // means OFF — a disabled span costs one atomic load. wgpu/cubecl emit
    // their own `tracing` records through the same pipe, so the useful
    // setting is `glyph3d_native=info`, not a bare `info`. set_global_default
    // rather than .init(): the builder's init also installs a `log` bridge
    // (tracing-log is on via the fmt feature) and env_logger above already
    // owns the `log` pipe — the two compose, one facade each.
    let trace_subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GLYPH_TRACE")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();
    tracing::subscriber::set_global_default(trace_subscriber)
        .expect("tracing subscriber installs once, before anything spawns");
    let cli = parse_cli();

    // Stage H: shell completions, then exit (no GPU).
    if let Some(shell) = cli.generate {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "glyph3d-native", &mut std::io::stdout());
        return;
    }

    // The hardware profile: init the GPU exactly as a render would, print,
    // exit. stdout carries only the answer (logs go to stderr), because the
    // build tool reads it to name a baseline directory.
    if cli.gpu_key || cli.gpu_profile {
        let ctx = pollster::block_on(gpu::init(None));
        if cli.gpu_key {
            println!("{}", ctx.profile.key());
        } else {
            print!("{}", ctx.profile.render_text());
        }
        return;
    }

    // Dev-only CubeCL bring-up smoke (note 16, phase 0).
    #[cfg(feature = "cubecl")]
    {
        if cli.cubecl_smoke {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_smoke::run(&ctx);
        }
        if let Some(path) = &cli.cubecl_scan_check {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_scan::run(&ctx, path);
        }
        if let Some(path) = &cli.cubecl_chain_check {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_chain::run(&ctx, path);
        }
        if let Some(path) = &cli.cubecl_chain_bench {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_chain::bench(&ctx, path);
        }
        if let Some(path) = &cli.cubecl_decode_check {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_chain::decode_check(&ctx, path);
        }
        if let Some(path) = &cli.cubecl_cluster_check {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_chain::cluster_check(&ctx, path);
        }
        if let Some(dir) = &cli.cubecl_repo_check {
            let ctx = pollster::block_on(gpu::init(None));
            cubecl_chain::repo_check(&ctx, dir);
        }
    }
    #[cfg(not(feature = "cubecl"))]
    {
        if cli.cubecl_smoke
            || cli.cubecl_scan_check.is_some()
            || cli.cubecl_chain_check.is_some()
            || cli.cubecl_chain_bench.is_some()
            || cli.cubecl_decode_check.is_some()
            || cli.cubecl_cluster_check.is_some()
            || cli.cubecl_repo_check.is_some()
        {
            eprintln!("error: cubecl options require building with `--features cubecl`");
            std::process::exit(1);
        }
    }

    // Fixture parity (reference port): fixture parse manifest / corpus diff — no GPU.
    if !cli.fixture_manifest.is_empty() {
        fixture::run_fixture_manifest(&cli.fixture_manifest);
    }
    if !cli.fixture_reference.is_empty() {
        fixture::run_fixture_reference(&cli.fixture_reference);
    }
    if !cli.fixture_trie.is_empty() {
        fixture::run_fixture_trie(&cli.fixture_trie);
    }
    if !cli.fixture_fold.is_empty() {
        fixture::run_fixture_fold(&cli.fixture_fold);
    }
    if !cli.fixture_scan.is_empty() {
        scan::run_fixture_scan(&cli.fixture_scan);
    }
    if !cli.fixture_bake.is_empty() {
        bake::run_fixture_bake(&cli.fixture_bake);
    }

    // Stage E1: engine check (Mojo engine removed)
    if let Some(_file) = &cli.engine_check {
        log::info!("engine-check: Mojo engine removed; use hyper-rust or unit tests");
    }

    // Stage D: engine file (Mojo engine removed)
    if let Some(_file) = &cli.engine_file {
        log::info!("engine-file: Mojo engine removed; use hyper-rust or unit tests");
        return;
    }

    // Stage E2 scan-only: full load pipeline without a GPU (measurement path).
    if cli.repo_scan_only {
        let dir = cli.load_repo.as_ref().expect("--repo-scan-only needs --load-repo");
        let params = repo::RepoParams {
            wrap_mode: parse_wrap_mode(&cli.wrap_mode),
            z_wrap_spacing: cli.z_wrap_spacing,
            cluster_mode: parse_cluster_mode(&cli.cluster_mode),
            ..Default::default()
        };
        let load = repo::load_repo(
            dir,
            &default_engine_trie(),
            &params,
            parse_strategy(&cli.repo_engine),
            cli.repo_verify,
        );
        load.print_stats();
        return;
    }

    let emoji_sheet = cli.emoji_sheet.clone().unwrap_or_else(default_emoji_sheet);
    let choice = if cli.demo {
        SceneChoice::Demo
    } else if let Some(dir) = &cli.load_repo {
        SceneChoice::Repo {
            dir: dir.clone(),
            strategy: parse_strategy(&cli.repo_engine),
            verify: cli.repo_verify,
            focus: cli.focus_file.clone(),
            wrap_mode: parse_wrap_mode(&cli.wrap_mode),
            z_wrap_spacing: cli.z_wrap_spacing,
            cluster_mode: parse_cluster_mode(&cli.cluster_mode),
            emoji_sheet,
        }
    } else if let Some(file) = &cli.engine_render {
        let trie = cli.engine_trie.unwrap_or_else(default_engine_trie);
        SceneChoice::EngineText {
            file: file.clone(),
            trie,
            emoji_sheet,
        }
    } else {
        SceneChoice::Text {
            file: cli.render_file.unwrap_or_else(default_text_file),
            copies: cli.copies,
            emoji_sheet,
            cluster_mode: parse_cluster_mode(&cli.cluster_mode),
        }
    };

    // Device/queue/adapter init is shared by both modes (gpu::init).
    let ctx = pollster::block_on(gpu::init(None));

    match cli.screenshot {
        Some(path) => offscreen::run(
            &ctx, &choice, &path, cli.frames, cli.zoom, !cli.no_cull, &cli.ops,
        ),
        None => {
            // Stage K (K6): scripted in-window capture (windowed only — clap
            // already rejected the combination with --screenshot).
            let shot = cli.screenshot_frame.zip(cli.screenshot_out.clone());
            windowed::run(
                ctx,
                choice,
                !cli.no_cull,
                &cli.ops,
                !cli.no_ui,
                shot,
                parse_present_mode(&cli.present_mode),
            )
        }
    }
}

