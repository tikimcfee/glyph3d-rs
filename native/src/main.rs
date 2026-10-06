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
#[cfg(feature = "cubecl")]
use glyph3d_native::cli::CubeclTask;
use glyph3d_native::cli::{
    parse_cli, Cli, CliCommand, FixtureTask, GpuInfoMode, RenderTarget,
};
use glyph3d_native::*;

/// Stage D: drive the Mojo glyph engine in-process over one file.
/// Prints slot count, per-load timing, throughput, and the first records.
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
        CliCommand::Cubecl(task) => {
            #[cfg(feature = "cubecl")]
            {
                let ctx = pollster::block_on(gpu::init(None));
                match task {
                    CubeclTask::Smoke => cubecl_smoke::run(&ctx),
                    CubeclTask::ScanCheck(path) => cubecl_scan::run(&ctx, &path),
                    CubeclTask::ChainCheck(path) => cubecl_chain::run(&ctx, &path),
                    CubeclTask::ChainBench(path) => cubecl_chain::bench(&ctx, &path),
                    CubeclTask::DecodeCheck(path) => cubecl_chain::decode_check(&ctx, &path),
                    CubeclTask::ClusterCheck(path) => cubecl_chain::cluster_check(&ctx, &path),
                    CubeclTask::RepoCheck { dir, color_mode } => {
                        cubecl_chain::repo_check(&ctx, &dir, color_mode);
                    }
                }
            }
            #[cfg(not(feature = "cubecl"))]
            {
                let _ = task;
                eprintln!("error: cubecl options require building with `--features cubecl`");
                std::process::exit(1);
            }
        }
        CliCommand::SpikeVertexYz(dir) => {
            let ctx = pollster::block_on(gpu::init(None));
            let report = spike_vertex_yz::run_spike(&ctx, &dir);
            println!("=== P2 SPIKE REPORT: Vertex-Stage Y/Z Bit-Exactness on Metal ===");
            println!("Total Slots Evaluated: {}", report.total_slots);
            println!(
                "Vertex Stage Y: {} / {} exact matches ({:.4}%) | max ULP diff: {}",
                report.vertex_y_matches,
                report.total_slots,
                (report.vertex_y_matches as f64 / report.total_slots as f64) * 100.0,
                report.max_y_ulp,
            );
            println!(
                "Vertex Stage Z: {} / {} exact matches ({:.4}%) | max ULP diff: {}",
                report.vertex_z_matches,
                report.total_slots,
                (report.vertex_z_matches as f64 / report.total_slots as f64) * 100.0,
                report.max_z_ulp,
            );
            println!(
                "Compute Stage Y: {} / {} exact matches ({:.4}%)",
                report.compute_y_matches,
                report.total_slots,
                (report.compute_y_matches as f64 / report.total_slots as f64) * 100.0,
            );
            println!(
                "Compute Stage Z: {} / {} exact matches ({:.4}%)",
                report.compute_z_matches,
                report.total_slots,
                (report.compute_z_matches as f64 / report.total_slots as f64) * 100.0,
            );
            println!(
                "Vertex vs Compute exact bit match: {} / {} ({:.4}%)",
                report.vertex_matches_compute,
                report.total_slots,
                (report.vertex_matches_compute as f64 / report.total_slots as f64) * 100.0,
            );

            if !report.first_y_mismatches.is_empty() {
                println!("\nFirst Y mismatches (up to 5):");
                for m in report.first_y_mismatches.iter().take(5) {
                    println!(
                        "  slot {}: item {} row {} wrap_seg {} | derived {:.7} ({:#010x}) vs stored {:.7} ({:#010x}) -> ULP {}",
                        m.slot_idx, m.item_idx, m.row, m.wrap_segment, m.derived, m.derived_bits, m.stored, m.stored_bits, m.ulp
                    );
                }
            }
            if !report.first_z_mismatches.is_empty() {
                println!("\nFirst Z mismatches (up to 5):");
                for m in report.first_z_mismatches.iter().take(5) {
                    println!(
                        "  slot {}: item {} row {} wrap_seg {} | derived {:.7} ({:#010x}) vs stored {:.7} ({:#010x}) -> ULP {}",
                        m.slot_idx, m.item_idx, m.row, m.wrap_segment, m.derived, m.derived_bits, m.stored, m.stored_bits, m.ulp
                    );
                }
            }
            std::process::exit(0);
        }
        CliCommand::Fixture(task) => match task {
            FixtureTask::Manifest(paths) => fixture::run_fixture_manifest(&paths),
            FixtureTask::Reference(paths) => fixture::run_fixture_reference(&paths),
            FixtureTask::Trie(paths) => fixture::run_fixture_trie(&paths),
            FixtureTask::Fold(paths) => fixture::run_fixture_fold(&paths),
            FixtureTask::Scan(paths) => scan::run_fixture_scan(&paths),
            FixtureTask::Bake(paths) => bake::run_fixture_bake(&paths),
        },
        CliCommand::RepoScanOnly {
            dir,
            params,
            strategy,
            verify,
        } => {
            #[cfg(not(feature = "cubecl"))]
            if strategy == repo::Strategy::Cubecl {
                eprintln!("error: --repo-engine cubecl was not compiled into this binary (rebuild with `cargo run --features cubecl`)");
                std::process::exit(1);
            }
            let load = repo::load_repo(
                &dir,
                &default_engine_trie(),
                &params,
                strategy,
                verify,
            );
            load.print_stats();
        }
        CliCommand::Render(plan) => {
            #[cfg(not(feature = "cubecl"))]
            if let SceneChoice::Repo { strategy: repo::Strategy::Cubecl, .. } = plan.choice {
                eprintln!("error: --repo-engine cubecl was not compiled into this binary (rebuild with `cargo run --features cubecl`)");
                std::process::exit(1);
            }

            let ctx = pollster::block_on(gpu::init(None));

            #[cfg(feature = "cubecl")]
            if matches!(&plan.choice, SceneChoice::Repo { strategy: repo::Strategy::Cubecl, .. }) {
                let shared_dev = gpu::SharedDevice::from_ctx(&ctx);
                let handle = std::thread::spawn(move || {
                    let t = std::time::Instant::now();
                    cubecl_chain::prewarm(&shared_dev);
                    log::info!("cubecl compute pipeline prewarm finished in {:?}", t.elapsed());
                });
                *ctx.prewarm_handle.lock().unwrap() = Some(handle);
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
                    );
                }
            }
        }
    }
}
