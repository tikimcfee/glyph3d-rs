//! glyph3d-native library — core engine for glyph layout, text parsing, and GPU rendering.

use std::path::{Path, PathBuf};

pub mod agent_transcript;
pub mod atlas;
pub mod bake;
pub mod cli;
pub mod config;
pub mod fixture;
pub mod fold;
pub mod glyph_scene;
pub mod glyph_trie;
pub mod hyper_oracle;
pub mod gpu;
pub mod launch_config;
pub mod layout;
pub mod layout_hyper;
pub mod layout_stack;
pub mod offscreen;
pub mod repo;
pub mod revision;
pub mod scan;
pub mod scene;
pub mod seam;
pub mod spatial_scene;
pub mod spike_vertex_yz;
pub mod text;
pub mod windowed;

#[cfg(feature = "cubecl")]
pub mod cubecl_smoke;
#[cfg(feature = "cubecl")]
pub mod cubecl_scan;
#[cfg(feature = "cubecl")]
pub mod cubecl_chain;
#[cfg(feature = "cubecl")]
pub mod cubecl_layout;

pub use atlas::default_trie;
pub use cli::{Op, parse_verb};
pub use glyph_scene::{CameraMode, GlyphScene};
pub use gpu::GpuContext;
pub use layout::{LayoutEngine, LayoutGlyphs, VerifyLayout};
pub use scene::{Scene, SceneLike};

pub const OFFSCREEN_WIDTH: u32 = 1600;
pub const OFFSCREEN_HEIGHT: u32 = 1000;

pub type CachedAgentSession = std::sync::Arc<
    std::sync::RwLock<Option<std::sync::Arc<(agent_transcript::AgentSession, revision::RevisionEngine)>>>,
>;

/// Which scene a run mode builds.
#[derive(Clone, Debug)]
pub enum SceneChoice {
    /// Stage A stress demo (1M colored quads).
    Demo,
    /// Stage C Slug text field: stage `file`, tiled `copies` times.
    Text { file: PathBuf, copies: u32, emoji_sheet: PathBuf, cluster_mode: fold::ClusterMode },
    /// Stage E1: lay `file` out with the engine and render through Slug glyph renderer.
    EngineText { file: PathBuf, trie: PathBuf, emoji_sheet: PathBuf },
    /// Agent Session: 3D Agent Carrel with Turn Deck and Workdesk.
    AgentSession {
        session_path: PathBuf,
        emoji_sheet: PathBuf,
        layout_options: spatial_scene::CarrelLayoutOptions,
        cached_session: CachedAgentSession,
    },
    /// Stage E2: load a whole repository as a field of code pages — one group
    /// per file, one shared glyph arena, grid layout.
    Repo {
        dir: PathBuf,
        strategy: repo::Strategy,
        verify: bool,
        focus: Option<String>,
        wrap_mode: fold::WrapMode,
        z_wrap_spacing: f64,
        cluster_mode: fold::ClusterMode,
        layout_mode: repo::RepoLayoutMode,
        color_mode: repo::ColorMode,
        emoji_sheet: PathBuf,
    },
}

pub fn atlas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/atlas")
}

pub fn default_engine_trie() -> PathBuf {
    atlas_dir().join("engine-trie.bin")
}

pub fn default_emoji_sheet() -> PathBuf {
    atlas_dir().join("emoji-sheet.bin")
}

pub fn engine_item_params_at(origin: [f64; 3]) -> layout::ItemParams {
    layout::ItemParams {
        line_height: (text::CELL_HEIGHT_WORLD * text::LINE_HEIGHT_FACTOR) as f64,
        origin_x: origin[0],
        origin_y: origin[1],
        origin_z: origin[2],
        ..Default::default()
    }
}

pub fn engine_item(bytes: &[u8]) -> layout::LayoutItem<'_> {
    engine_item_at(bytes, [0.0, 0.0, 0.0])
}

pub fn engine_item_at(bytes: &[u8], origin: [f64; 3]) -> layout::LayoutItem<'_> {
    layout::LayoutItem {
        bytes,
        params: engine_item_params_at(origin),
        group_id: 0,
        paint: layout::Paint::Flat(layout::DEFAULT_COLOR_PACKED),
    }
}

pub fn engine_layout(file: &Path, trie: &Path) -> (layout::GlyphArena, layout::ItemPlacement) {
    let bytes = std::fs::read(file).expect("failed to read engine input file");
    let mut backend = layout::LayoutEngine::hyper();
    backend.load_trie_file(trie).expect("failed to load trie");
    let mut arena = layout::GlyphArena::new();
    let placements = backend
        .layout_items(&[engine_item(&bytes)], &mut arena)
        .expect("engine layout failed");
    (arena, placements[0])
}


#[derive(Clone, Copy, Debug)]
pub struct SceneCullOptions {
    pub cull: bool,
    pub file_backgrounds: bool,
    pub file_bg_color: [f32; 4],
    pub lod_min_px: Option<f32>,
    pub greeking: bool,
    pub greek_pure: bool,
    pub greek_onset_px: Option<f32>,
    /// Which glyph-field implementation the scene builds (`--field-mode`).
    pub field_mode: glyph_scene::GlyphFieldMode,
    /// The ground/sky environment (`--environment`, `[environment] mode`).
    pub environment: config::EnvironmentMode,
    /// Explicit ground height (`--ground-y`); None = below the scene.
    pub ground_y: Option<f32>,
}

impl Default for SceneCullOptions {
    fn default() -> Self {
        Self {
            cull: true,
            file_backgrounds: false,
            file_bg_color: config::settings().glyph_scene.file_bg_color,
            lod_min_px: None,
            greeking: true,
            greek_pure: true,
            greek_onset_px: None,
            field_mode: glyph_scene::GlyphFieldMode::Instanced,
            environment: config::settings().environment.mode,
            ground_y: None,
        }
    }
}

pub fn build_scene(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull: bool,
) -> Box<dyn SceneLike> {
    build_scene_impl(
        ctx,
        color_format,
        choice,
        camera_mode,
        SceneCullOptions { cull, ..Default::default() },
        false,
    ).0
}

pub fn build_scene_with_options(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull_opts: SceneCullOptions,
) -> Box<dyn SceneLike> {
    build_scene_impl(ctx, color_format, choice, camera_mode, cull_opts, false).0
}

/// Build a GlyphScene from ALREADY-STAGED content — the P1-live entry for
/// callers that OWN their content (the seam's envelope path: bytes through
/// `repo::load_items` + `into_staged`, `PickContext::content` injected by
/// the caller). No probe: this is the offscreen/linked-embedder shape.
pub fn build_scene_from_staged(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    atlas: &atlas::Atlas,
    staged: text::StagedText,
    camera_mode: CameraMode,
    cull: bool,
) -> Box<dyn SceneLike> {
    Box::new(GlyphScene::new(
        ctx, color_format, atlas, staged, camera_mode, cull, glyph_scene::GlyphFieldMode::Instanced,
    ))
}

/// The PROBED twin of [`build_scene_from_staged`] — installs the Debug
/// panel's read-back channel before type erasure, so a live rebuilt scene
/// keeps its panel (the windowed live loop's shape: rebuild + restyle while
/// the viewer watches).
pub fn build_scene_from_staged_probed(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    atlas: &atlas::Atlas,
    staged: text::StagedText,
    camera_mode: CameraMode,
    cull: bool,
) -> (Box<dyn SceneLike>, Option<glyph_scene::UiProbe>) {
    let mut scene = GlyphScene::new(
        ctx, color_format, atlas, staged, camera_mode, cull, glyph_scene::GlyphFieldMode::Instanced,
    );
    let probe = scene.init_ui_probe();
    (Box::new(scene), Some(probe))
}

pub fn build_scene_probed(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull_opts: SceneCullOptions,
) -> (Box<dyn SceneLike>, Option<glyph_scene::UiProbe>) {
    build_scene_impl(ctx, color_format, choice, camera_mode, cull_opts, true)
}

fn build_scene_impl(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull_opts: SceneCullOptions,
    probe: bool,
) -> (Box<dyn SceneLike>, Option<glyph_scene::UiProbe>) {
    let glyph = |scene: GlyphScene| {
        let mut scene = scene;
        scene.set_file_backgrounds(cull_opts.file_backgrounds);
        scene.set_file_bg_color(cull_opts.file_bg_color);
        if let Some(lod) = cull_opts.lod_min_px {
            scene.set_lod_min_px(lod);
        }
        let mode = if !cull_opts.greeking {
            0
        } else if cull_opts.greek_pure {
            2
        } else {
            1
        };
        scene.set_greek_mode(&ctx.queue, mode);
        if let Some(onset) = cull_opts.greek_onset_px {
            scene.set_greek_onset_px(&ctx.queue, onset);
        }
        scene.set_environment(cull_opts.environment, cull_opts.ground_y);
        let p = probe.then(|| scene.init_ui_probe());
        (Box::new(scene) as Box<dyn SceneLike>, p)
    };
    let cull = cull_opts.cull;
    let field_mode = cull_opts.field_mode;
    match choice {
        SceneChoice::Demo => (Box::new(Scene::new(ctx, color_format)), None),
        SceneChoice::Text { file, copies, emoji_sheet, cluster_mode } => {
            let atlas = atlas::Atlas::load(ctx, emoji_sheet);
            let staged = text::stage_file(&atlas, file, *copies, *cluster_mode);
            log::info!(
                "staged {}: {} codepoints → {} glyph instances ({} copies, {} missing/bitmap)",
                file.display(),
                staged.codepoints_decoded,
                staged.glyphs_emitted,
                copies,
                staged.missing_or_bitmap,
            );
            let mut scene = GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull, field_mode);
            scene.set_probe_cluster_mode(matches!(cluster_mode, fold::ClusterMode::Cluster));
            glyph(scene)
        }
        SceneChoice::EngineText { file, trie, emoji_sheet } => {
            let atlas = atlas::Atlas::load(ctx, emoji_sheet);
            let (arena, placement) = engine_layout(file, trie);
            log::info!(
                "engine-staged {}: {} records ({} blank/missing slots dropped)",
                file.display(),
                placement.record_count,
                placement.record_count - placement.slot_count,
            );
            let staged = text::stage_records(arena, &placement, &atlas.slot_ink);
            glyph(GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull, field_mode))
        }
        SceneChoice::AgentSession { session_path, emoji_sheet, layout_options, cached_session } => {
            let cached_opt = cached_session.read().ok().and_then(|g| g.clone());
            let (session, rev_engine) = if let Some(cached) = cached_opt {
                let (s, r) = (*cached).clone();
                (s, r)
            } else {
                let session = match agent_transcript::load_session_from_path(session_path) {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("Failed to load agent session from {}: {}", session_path.display(), e);
                        let session_id = session_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("empty");
                        agent_transcript::AgentSession::new(agent_transcript::HarnessKind::ClaudeCode, session_id)
                    }
                };
                let cwd_opt = session.cwd.clone();
                let cur_dir = std::env::current_dir().ok();

                let mut rev_engine = revision::RevisionEngine::new().with_disk_resolver(move |rel_path: &str| {
                    let clean = rel_path.trim().trim_matches('"').trim_matches('\'');
                    let path_str = clean.strip_prefix("file://").unwrap_or(clean);
                    let p = std::path::Path::new(path_str);
                    if p.is_absolute() && p.is_file() {
                        if let Ok(content) = std::fs::read_to_string(p) {
                            return Some(content);
                        }
                    }
                    if let Some(ref cwd) = cwd_opt {
                        let full = std::path::Path::new(cwd).join(path_str);
                        if full.is_file() {
                            if let Ok(content) = std::fs::read_to_string(&full) {
                                return Some(content);
                            }
                        }
                    }
                    if let Some(ref cur) = cur_dir {
                        let full = cur.join(path_str);
                        if full.is_file() {
                            if let Ok(content) = std::fs::read_to_string(&full) {
                                return Some(content);
                            }
                        }
                    }
                    None
                });
                rev_engine.ingest_session(&session);
                if let Ok(mut g) = cached_session.write() {
                    *g = Some(std::sync::Arc::new((session.clone(), rev_engine.clone())));
                }
                (session, rev_engine)
            };

            let atlas = atlas::Atlas::load(ctx, emoji_sheet);
            let staged = agent_transcript::stage_agent_session_with_options(
                Some(&atlas),
                &atlas.slot_ink,
                session,
                rev_engine,
                *layout_options,
            );
            let scene = GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull, field_mode);
            glyph(scene)
        }
        SceneChoice::Repo {
            dir,
            strategy,
            verify,
            focus,
            wrap_mode,
            z_wrap_spacing,
            cluster_mode,
            layout_mode,
            color_mode,
            emoji_sheet,
        } => {
            let params = repo::RepoParams {
                wrap_mode: *wrap_mode,
                z_wrap_spacing: *z_wrap_spacing,
                cluster_mode: *cluster_mode,
                layout_mode: *layout_mode,
                color_mode: *color_mode,
                field_mode,
                ..Default::default()
            };
            let t_visual_start = std::time::Instant::now();
            let device = &ctx.device;
            let queue = &ctx.queue;
            #[cfg(feature = "cubecl")]
            let shared_dev = gpu::SharedDevice::from_ctx(ctx);
            let prefetched_atlas_handle = ctx.prefetched_atlas.lock().unwrap_or_else(|e| e.into_inner()).take();
            let (load, atlas, atlas_wall) = std::thread::scope(|s| {
                let atlas_handle = s.spawn(move || {
                    let t = std::time::Instant::now();
                    let a = if let Some(h) = prefetched_atlas_handle {
                        let res = h.join().expect("prefetched atlas thread panicked");
                        log::info!("joined prefetched atlas in {:?}", t.elapsed());
                        res
                    } else {
                        atlas::Atlas::load_device(device, queue, emoji_sheet)
                    };
                    (a, t.elapsed())
                });
                #[cfg(feature = "cubecl")]
                if *strategy == repo::Strategy::Cubecl {
                    let mut guard = ctx.prewarm_handle.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.is_none() {
                        let dev = shared_dev.clone();
                        let is_derived = params.field_mode == glyph_field::GlyphFieldMode::Derived;
                        *guard = Some(std::thread::spawn(move || {
                            let t = std::time::Instant::now();
                            cubecl_chain::prewarm(&dev, Some(is_derived));
                            log::info!("cubecl compute pipeline prewarm finished in {:?}", t.elapsed());
                        }));
                    }
                }
                let prefetched = if let Some(h) = ctx.prefetched_walk.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let t_wait = std::time::Instant::now();
                    let res = h.join().unwrap_or_else(|_| repo::prefetch_repo(dir, params, *strategy));
                    log::info!("joined prefetched repo in {:?}", t_wait.elapsed());
                    res
                } else {
                    repo::prefetch_repo(dir, params, *strategy)
                };
                let arena = layout::GlyphArena::new();
                let load = repo::load_repo_from_prefetched(
                    dir,
                    prefetched,
                    &default_engine_trie(),
                    &params,
                    *strategy,
                    *verify,
                    Some(ctx),
                    arena,
                    None,
                );
                let (a, dur) = atlas_handle.join().expect("atlas load thread panicked");
                (load, a, dur)
            });
            load.print_stats();

            let t_staged = std::time::Instant::now();
            let staged = load.into_staged(focus.as_deref(), &atlas.slot_ink);
            let staged_dur = t_staged.elapsed();

            let t_scene = std::time::Instant::now();
            let mut scene = GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull, field_mode);
            scene.set_probe_layout_mode(*layout_mode);
            scene.set_probe_strategy(*strategy);
            let scene_dur = t_scene.elapsed();
            let visual_total = t_visual_start.elapsed();

            println!(
                "visual: atlas {:.3}s (concurrent) | staged {:.3}s | scene {:.3}s | total visual init {:.3}s",
                atlas_wall.as_secs_f64(),
                staged_dur.as_secs_f64(),
                scene_dur.as_secs_f64(),
                visual_total.as_secs_f64(),
            );
            glyph(scene)
        }
    }
}
