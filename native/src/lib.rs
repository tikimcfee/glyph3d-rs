//! glyph3d-native library — core engine for glyph layout, text parsing, and GPU rendering.

use std::path::{Path, PathBuf};

pub mod atlas;
pub mod bake;
pub mod cli;
pub mod fixture;
pub mod fold;
pub mod glyph_scene;
pub mod glyph_trie;
pub mod gpu;
pub mod layout;
pub mod layout_hyper;
pub mod layout_stack;
pub mod offscreen;
pub mod repo;
pub mod scan;
pub mod scene;
pub mod seam;
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

/// Which scene a run mode builds.
pub enum SceneChoice {
    /// Stage A stress demo (1M colored quads).
    Demo,
    /// Stage C Slug text field: stage `file`, tiled `copies` times.
    Text { file: PathBuf, copies: u32, emoji_sheet: PathBuf, cluster_mode: fold::ClusterMode },
    /// Stage E1: lay `file` out with the engine and render through Slug glyph renderer.
    EngineText { file: PathBuf, trie: PathBuf, emoji_sheet: PathBuf },
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

pub fn build_scene(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull: bool,
) -> Box<dyn SceneLike> {
    build_scene_impl(ctx, color_format, choice, camera_mode, cull, false).0
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
    Box::new(GlyphScene::new(ctx, color_format, atlas, staged, camera_mode, cull))
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
    let mut scene = GlyphScene::new(ctx, color_format, atlas, staged, camera_mode, cull);
    let probe = scene.init_ui_probe();
    (Box::new(scene), Some(probe))
}

pub fn build_scene_probed(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull: bool,
) -> (Box<dyn SceneLike>, Option<glyph_scene::UiProbe>) {
    build_scene_impl(ctx, color_format, choice, camera_mode, cull, true)
}

fn build_scene_impl(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull: bool,
    probe: bool,
) -> (Box<dyn SceneLike>, Option<glyph_scene::UiProbe>) {
    let glyph = |scene: GlyphScene| {
        let mut scene = scene;
        let p = probe.then(|| scene.init_ui_probe());
        (Box::new(scene) as Box<dyn SceneLike>, p)
    };
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
            let mut scene = GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull);
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
            glyph(GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull))
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
                ..Default::default()
            };
            let t_visual_start = std::time::Instant::now();
            let device = &ctx.device;
            let queue = &ctx.queue;
            let (load, atlas, atlas_wall) = std::thread::scope(|s| {
                let atlas_handle = s.spawn(|| {
                    let t = std::time::Instant::now();
                    let a = atlas::Atlas::load_device(device, queue, emoji_sheet);
                    (a, t.elapsed())
                });
                let walk = repo::walk_repo(dir);
                let arena = layout::GlyphArena::new();
                let load = repo::load_repo_from_walk(
                    dir,
                    walk,
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
            let mut scene = GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull);
            scene.set_probe_layout_mode(*layout_mode);
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
