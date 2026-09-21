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

mod atlas;
mod bake;
mod engine;
mod fixture;
mod fold;
mod glyph_trie;
mod layout;
mod layout_mojo;
mod gpu;
mod glyph_scene;
mod offscreen;
mod repo;
mod scan;
mod scene;
mod text;
mod windowed;

use std::path::{Path, PathBuf};

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use glyph_scene::{CameraMode, GlyphScene, PickCommand, Verb};
use gpu::GpuContext;
// The seam is used by trait, not by concrete backend: swapping `MojoLayout`
// for the Rust one changes the constructor and nothing else here.
use layout::{LayoutGlyphs, VerifyLayout};
use scene::{Scene, SceneLike};

pub const OFFSCREEN_WIDTH: u32 = 1600;
pub const OFFSCREEN_HEIGHT: u32 = 1000;

/// Which scene a run mode builds.
pub enum SceneChoice {
    /// Stage A stress demo (1M colored quads).
    Demo,
    /// Stage C Slug text field: stage `file`, tiled `copies` times.
    Text { file: PathBuf, copies: u32, emoji_sheet: PathBuf, cluster_mode: fold::ClusterMode },
    /// Stage E1: lay `file` out with the Mojo engine (real atlas trie) and
    /// render the engine's records through the same Slug glyph renderer.
    EngineText { file: PathBuf, trie: PathBuf, emoji_sheet: PathBuf },
    /// Stage E2: load a whole repository as a field of code pages — one group
    /// per file, one shared glyph arena, grid layout.
    Repo {
        dir: PathBuf,
        strategy: layout_mojo::Strategy,
        verify: bool,
        focus: Option<String>,
        /// How a wrap is spent. `Back` is the default: a wrapped line costs
        /// DEPTH rather than a row, which is the layout this renderer is for.
        /// `repo-wide` and `repo-zoom` are gated on it; `repo-down` keeps the
        /// other mode covered so making one default cannot silently retire the
        /// other.
        wrap_mode: fold::WrapMode,
        /// The wrap staircase's pitch (`--z-wrap-spacing`, default 0.15 —
        /// the web's `zWrapSpacing`). Carried alongside wrap_mode for the
        /// same reason: the baselines depend on the default, so the CLI
        /// layer pins it rather than letting RepoParams::default speak alone.
        z_wrap_spacing: f64,
        /// The sequence pass on a repo load (`--cluster-mode`, default
        /// leader). Same standing as wrap_mode: the baselines pin the default.
        cluster_mode: fold::ClusterMode,
        /// The colour-emoji sheet (`--emoji-sheet`; default the committed
        /// one). Every glyph scene loads it — the same handle the engine
        /// trie has, so swapping a sheet is a command-line act.
        emoji_sheet: PathBuf,
    },
}

/// `--wrap-mode` -> the layout parameter. clap's `value_parser` has already
/// refused anything that is not one of the two spellings, so an unknown value
/// here is a bug in this function rather than in the caller's command line —
/// which is why it panics instead of falling back to the default. A silent
/// fallback would render mode A while the operator believed they asked for B.
fn parse_wrap_mode(s: &str) -> fold::WrapMode {
    match s {
        "down" => fold::WrapMode::Down,
        "back" => fold::WrapMode::Back,
        other => panic!("--wrap-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--z-wrap-spacing` validation, run by clap at parse time. NaN must be
/// refused HERE, not left to `ItemParams::validate` at the layout seam:
/// clap's error names the flag and the value; the seam's panic would name
/// neither. Negative means the staircase steps FORWARD through the page
/// plane, which no baseline has ever rendered; refuse it rather than gate
/// nothing on a geometry nobody has looked at. 0 is legitimate — the
/// documented flat layout (`RepoParams::z_wrap_spacing`).
fn parse_z_wrap_spacing(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("--z-wrap-spacing: {s:?} is not a number"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!(
            "--z-wrap-spacing: {v} is out of domain (need a finite value >= 0)"
        ));
    }
    Ok(v)
}

/// Panics on an unknown strategy for the same reason `parse_wrap_mode` does:
/// clap has already refused anything else, so reaching here means the parser
/// and this match disagree, and silently loading with the wrong strategy would
/// make a verification run compare something other than what was asked for.
fn parse_strategy(s: &str) -> layout_mojo::Strategy {
    use layout_mojo::Strategy;
    match s {
        "naive" => Strategy::PerItem,
        "batch" => Strategy::Batched,
        "direct" => Strategy::Direct,
        other => panic!("--repo-engine: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--cluster-mode` -> the layout parameter. Same shape as `parse_wrap_mode`:
/// clap has already refused anything that is not one of the two spellings, so
/// an unknown value here is a bug in this function rather than in the caller's
/// command line — a silent fallback would render mode A while the operator
/// believed they asked for B.
fn parse_cluster_mode(s: &str) -> fold::ClusterMode {
    match s {
        "leader" => fold::ClusterMode::Leader,
        "cluster" => fold::ClusterMode::Cluster,
        other => panic!("--cluster-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// The atlas directory shared by every mode (`<crate>/../assets/atlas`).
pub fn atlas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/atlas")
}

/// Stage E1: the engine's default trie — the REAL atlas mapping, generated by
/// `python3 tools/gen_real_trie.py` (not a conformance fixture).
pub fn default_engine_trie() -> PathBuf {
    atlas_dir().join("engine-trie.bin")
}

/// The colour-emoji sheet the renderer loads unless `--emoji-sheet` says
/// otherwise — `assets/atlas/emoji-sheet.bin`, baked by tools/gen_emoji_sheet.py.
pub fn default_emoji_sheet() -> PathBuf {
    atlas_dir().join("emoji-sheet.bin")
}

/// The engine layout params the renderer/cross-check use: unit cell height
/// (text::CELL_HEIGHT_WORLD) and the production line pitch. No wrap, no pages.
/// The same params at a chosen origin.
///
/// The origin exists as a parameter because a zero one is a BLIND SPOT. A Mojo
/// nightly was found miscompiling `ffi.mojo`'s descriptor read when built into
/// a test executable — an uninitialised read — while the same source built as
/// the shipping dylib was bit-exact. `ffi_selftest` catches that class, and for
/// a while it was the ONLY thing that did, because this cross-check laid every
/// item out at (0,0,0) and so never exercised the `origin_x` read at all: a
/// garbage value added to zero and compared against zero-plus-the-same-garbage
/// agrees with itself. One instrument covering a whole failure class is a
/// single point of failure, so `--engine-check` now runs a non-zero origin too.
pub fn engine_item_params_at(origin: [f64; 3]) -> layout::ItemParams {
    layout::ItemParams {
        line_height: (text::CELL_HEIGHT_WORLD * text::LINE_HEIGHT_FACTOR) as f64,
        origin_x: origin[0],
        origin_y: origin[1],
        origin_z: origin[2],
        ..Default::default()
    }
}

/// The one item `--engine-render` and `--engine-check` each lay out: the whole
/// file, production line pitch, flat default paint, group 0.
fn engine_item(bytes: &[u8]) -> layout::LayoutItem<'_> {
    engine_item_at(bytes, [0.0, 0.0, 0.0])
}

fn engine_item_at(bytes: &[u8], origin: [f64; 3]) -> layout::LayoutItem<'_> {
    layout::LayoutItem {
        bytes,
        params: engine_item_params_at(origin),
        group_id: 0,
        paint: layout::Paint::Flat(layout::DEFAULT_COLOR_PACKED),
    }
}

fn engine_backend(trie: &Path) -> layout_mojo::MojoLayout {
    let mut backend = layout_mojo::MojoLayout::new(layout_mojo::Strategy::Batched);
    backend
        .load_trie_file(trie)
        .expect("failed to load engine trie");
    backend
}

/// Lay one file out through the seam FOR RENDERING: instances in an arena plus
/// its placement. No records, no readback — this is the path a frame takes.
fn engine_layout(file: &Path, trie: &Path) -> (layout::GlyphArena, layout::ItemPlacement) {
    let bytes = std::fs::read(file).expect("failed to read engine input file");
    let mut backend = engine_backend(trie);
    let mut arena = layout::GlyphArena::new();
    let placements = backend
        .layout_items(&[engine_item(&bytes)], &mut arena)
        .expect("engine layout failed");
    (arena, placements[0])
}

/// Lay one file out through the seam FOR VERIFICATION: the wire records, which
/// `--engine-check` diffs lane by lane against the independent CPU reference.
/// This is the 36 B-per-source-byte readback the render path above does not
/// pay, asked for explicitly through `VerifyLayout` — see `layout.rs`.
fn engine_layout_records_at(
    file: &Path,
    trie: &Path,
    origin: [f64; 3],
) -> Vec<layout::GlyphRecord> {
    let bytes = std::fs::read(file).expect("failed to read engine input file");
    let mut backend = engine_backend(trie);
    let mut arena = layout::GlyphArena::new();
    let (placements, records) = backend
        .layout_items_recording(&[engine_item_at(&bytes, origin)], &mut arena)
        .expect("engine layout failed");
    assert_eq!(
        records.len() as u32, placements[0].record_count,
        "record copy count mismatch"
    );
    records
}

/// Build the chosen scene for a given color target format.
pub fn build_scene(
    ctx: &GpuContext,
    color_format: wgpu::TextureFormat,
    choice: &SceneChoice,
    camera_mode: CameraMode,
    cull: bool,
) -> Box<dyn SceneLike> {
    // Offscreen path: never installs a UI probe (probe = false), so the
    // scene behaves exactly as pre-K.
    build_scene_impl(ctx, color_format, choice, camera_mode, cull, false).0
}

/// Stage K (K3): windowed scene construction. Same scenes as build_scene,
/// plus a debug-UI probe handle installed on the concrete GlyphScene BEFORE
/// type erasure — the egui Debug panel's read-back channel (windowed.rs
/// holds the scene as `Box<dyn SceneLike>`; fence 4 forbids SceneLike
/// changes). The demo scene has no probe (None).
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
    // Same construction order for both modes; only the probe install differs.
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
            // No pick context on this path, so the panel's cluster toggle
            // seeds from the choice directly — hand it the mode the scene
            // was staged with.
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
            emoji_sheet,
        } => {
            let params = repo::RepoParams {
                wrap_mode: *wrap_mode,
                z_wrap_spacing: *z_wrap_spacing,
                cluster_mode: *cluster_mode,
                ..Default::default()
            };
            let load = repo::load_repo(dir, &default_engine_trie(), &params, *strategy, *verify);
            load.print_stats();
            let atlas = atlas::Atlas::load(ctx, emoji_sheet);
            let staged = load.into_staged(focus.as_deref(), &atlas.slot_ink);
            glyph(GlyphScene::new(ctx, color_format, &atlas, staged, camera_mode, cull))
        }
    }
}

/// Long-form help tail: the mode summary + verb reference + windowed keys from
/// the hand-rolled parser's --help (nothing user-facing was dropped).
const AFTER_LONG_HELP: &str = "\
MODES:
  no args                windowed text field (default file: this crate's main.rs)
  --demo                 windowed: Stage A quad-field demo
  --screenshot PATH      offscreen: render N frames, write PNG, print timing,
                         exit 0. Deterministic (fixed virtual clock).

VERBS (--verb \"V [ARGS]\", repeatable; applies to the most recent pick):
  recolor-glyph [rrggbb] | recolor-line [rrggbb]
  nudge-glyph dx dy [dz] | scale-glyph f
  move-group dx dy dz | scale-group s
  tint-group rrggbb | tint-cycle | hide-group | show-group | toggle-hidden

WINDOWED MODE:
  fly camera — WASD move, E|R up, Q|F down, RIGHT-drag look, scroll = speed,
  Esc releases | interact: LEFT click = pick glyph, h highlight line,
  g grab file (mouse drags, scroll scales), t tint, x hide |
  F1 toggle Debug panel, F2 save screenshot (out/windowed-shot-*.png) |
  --screenshot-frame N --screenshot-out PATH: scripted capture, keeps running";

/// Stage H: clap-derive CLI. Semantics (flags, defaults, op-stream order) are
/// preserved from the hand-rolled parser it replaced — parity is pinned by the
/// tests at the bottom of this file. `args_override_self` keeps the old
/// last-wins behavior for repeated scalar flags.
#[derive(Parser)]
#[command(
    name = "glyph3d-native",
    about = "native GPU port of the glyph3d-js code-visualization system (dev/verification CLI)",
    after_long_help = AFTER_LONG_HELP,
    args_override_self = true,
)]
struct Cli {
    /// Offscreen: render to PATH (PNG), print timing, exit 0
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,
    /// Frames to render offscreen
    #[arg(long, value_name = "N", default_value_t = 1)]
    frames: u32,
    /// Stage A quad-field demo instead of the text field
    #[arg(long)]
    demo: bool,
    /// Text file to stage (UTF-8); default: this crate's main.rs
    #[arg(long, value_name = "PATH")]
    render_file: Option<PathBuf>,
    /// Tile the file N times (stress)
    #[arg(long, value_name = "N", default_value_t = 1)]
    copies: u32,
    /// Camera magnification for offscreen
    #[arg(long, value_name = "F", default_value_t = 1.0, allow_negative_numbers = true)]
    zoom: f32,
    /// Stage D: run the Mojo glyph engine in-process on PATH, print records, exit
    #[arg(long, value_name = "PATH")]
    engine_file: Option<PathBuf>,
    /// The colour-emoji sheet every glyph scene loads (default:
    /// assets/atlas/emoji-sheet.bin, baked from the vendored Noto Color Emoji).
    /// Point it at another G3ES file to swap the sheet without a rebuild.
    #[arg(long, value_name = "PATH")]
    emoji_sheet: Option<PathBuf>,
    /// Trie for engine modes (default: assets/atlas/engine-trie.bin — the real
    /// atlas mapping; pass a .pipe.bin fixture for the toy one)
    #[arg(long, value_name = "PATH")]
    engine_trie: Option<PathBuf>,
    /// Repeat the --engine-file load N times (leak/stability loop)
    #[arg(long, value_name = "N", default_value_t = 1)]
    engine_loop: u32,
    /// Stage E1: cross-check engine output vs text.rs CPU reference (bit-exact), exit
    #[arg(long, value_name = "PATH")]
    engine_check: Option<PathBuf>,
    /// Stage E1: render engine records through the Slug renderer
    #[arg(long, value_name = "PATH")]
    engine_render: Option<PathBuf>,
    /// Fixture parity (reference port): print the canonical parse manifest for each
    /// .pipe.bin fixture and exit. tools/check-fixture-parity.sh diffs these
    /// lines against the ones engine/fixture_manifest.mojo emits from the Mojo
    /// loader — two independent parsers agreeing on checksums over their PARSED
    /// values, not on the file's bytes.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_manifest: Vec<PathBuf>,
    /// Fixture parity: lay each .pipe.bin with the CPU reference fold and diff
    /// BIT-EXACT against the oracle's own expected lanes, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_reference: Vec<PathBuf>,
    /// Trie rebuild: rebuild each .pipe.bin's trie from its own bytes with the
    /// ported GlyphTrie and diff against the trie the oracle stored, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_trie: Vec<PathBuf>,
    /// Full fold: run the ported serial fold over each .pipe.bin and compare
    /// EVERY lane of EVERY byte plus boxes and the batch union, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_fold: Vec<PathBuf>,
    /// Scan form: run the ported scan form over each .pipe.bin at a SWEEP of
    /// chunk/group/shard tunings and compare under the tiered contract, then
    /// exit. Invariance across the tunings is associativity in situ.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_scan: Vec<PathBuf>,
    /// Bake: replay each .bake.bin through the ported bake and diff the
    /// record AND every seed-protocol query bit-exact, then exit.
    #[arg(long, value_name = "PATH", num_args = 1..)]
    fixture_bake: Vec<PathBuf>,
    /// Stage E2: load a whole repository as a field of code pages
    #[arg(long, value_name = "DIR")]
    load_repo: Option<PathBuf>,
    /// Which FFI strategy the Mojo backend uses. `direct` is the DEFAULT: the
    /// engine writes render instances straight into the arena, materializing no
    /// wire record on either side of the FFI — one pass where `naive` and
    /// `batch` take three, and it folds in chunks so lane memory follows the
    /// chunk rather than the corpus. The record strategies remain because they
    /// are the verification form: `VerifyLayout` needs a wire stream, and
    /// `--repo-verify` diffs whichever pair you name.
    #[arg(long, value_name = "MODE", default_value = "direct", value_parser = ["naive", "batch", "direct"])]
    repo_engine: String,
    /// Diff the chosen strategy against a counterpart, bit-exact over the
    /// whole repo: placements and instances always, wire records when both
    /// paths have them (`direct` has none, and the PASS line says so).
    #[arg(long)]
    repo_verify: bool,
    /// How a wrap is spent on a repo load: `back` (the default, and what the
    /// `repo-wide`/`repo-zoom` byte-equal screenshot baselines are taken
    /// under; `repo-down` covers the other) keeps the row and steps the
    /// segment back in depth instead — one row per source line however long
    /// it is. `down` advances the visual row per wrap.
    #[arg(long, value_name = "MODE", default_value = "back", value_parser = ["down", "back"])]
    wrap_mode: String,
    /// The wrap staircase's pitch: z step per intra-line wrap segment, as a
    /// multiple of the em cell height (`RepoParams::z_wrap_spacing` — the
    /// web's `zWrapSpacing`). 0 restores the flat layout exactly
    /// (repo.rs). The default is what the repo screenshot baselines are
    /// taken under — same standing as --wrap-mode.
    // allow_negative_numbers so a negative REACHES the parser and is refused
    // with the flag named — otherwise clap eats "-0.1" as an unknown flag.
    #[arg(long, value_name = "F", default_value_t = 0.15, allow_negative_numbers = true, value_parser = parse_z_wrap_spacing)]
    z_wrap_spacing: f64,
    /// Whether the sequence pass resolves codepoint clusters to single glyphs
    /// on a repo load (and on --render-file): `leader` (the default) is one
    /// glyph per UTF-8 leader; `cluster` resolves the font's sequences
    /// (ZWJ families, RI flags, skin tones, keycaps) to single slots with
    /// trailing leaders zeroed. The emoji baselines render `leader`.
    #[arg(long, value_name = "MODE", default_value = "leader", value_parser = ["leader", "cluster"])]
    cluster_mode: String,
    /// Stage E2: frame the first file whose path contains SUBSTR
    #[arg(long, value_name = "SUBSTR")]
    focus_file: Option<String>,
    /// Stage E2: walk + engine + stage + stats, then exit (no GPU)
    #[arg(long)]
    repo_scan_only: bool,
    /// Stage F: disable the cull/LOD pass (legacy per-chunk draws; debug/A-B)
    #[arg(long)]
    no_cull: bool,
    /// Stage K: windowed without the egui UI overlay (exact pre-K behavior)
    #[arg(long)]
    no_ui: bool,
    /// Stage K (K6): windowed only — capture the frame after N frames have
    /// rendered (requires --screenshot-out; the app KEEPS RUNNING afterward —
    /// unlike --screenshot it never exits)
    #[arg(long, value_name = "N", requires = "screenshot_out", conflicts_with = "screenshot")]
    screenshot_frame: Option<u64>,
    /// Stage K (K6): windowed only — PNG path for --screenshot-frame
    #[arg(long, value_name = "PATH", requires = "screenshot_frame", conflicts_with = "screenshot")]
    screenshot_out: Option<PathBuf>,
    /// Generate shell completions for SHELL and exit
    #[arg(long, value_name = "SHELL")]
    generate: Option<clap_complete::Shell>,
    /// Print the golden-set key for the adapter wgpu picks (`backend-vendor`,
    /// e.g. `vulkan-nvidia`) and exit. The build tool asks this to choose
    /// which baseline directory the pixel gate compares against.
    #[arg(long)]
    gpu_key: bool,
    /// Print the full hardware profile the renderer resolved and exit — the
    /// provenance record committed beside a golden set as ADAPTER.txt.
    #[arg(long)]
    gpu_profile: bool,
    /// Windowed only: how frames reach the display. `fifo` (the default) is
    /// vsync, so the FPS line reads the monitor's refresh; `mailbox` and
    /// `immediate` uncap it where the surface supports them (else fifo, and
    /// the log says so). Offscreen renders never present and ignore this.
    #[arg(long, value_name = "MODE", default_value = "fifo", value_parser = ["fifo", "mailbox", "immediate"])]
    present_mode: String,
    /// Stage G op-stream flags, captured per-flag by clap and re-interleaved
    /// into `ops` by build_ops().
    #[command(flatten)]
    raw_ops: RawOps,
    /// Stage G: the interleaved pick/verb script, in CLI order. Verbs apply
    /// to the most recent pick.
    #[arg(skip)]
    ops: Vec<Op>,
}

/// The op-stream flags exactly as clap captures them (per-flag vectors).
/// `build_ops` restores the true CLI interleaving via occurrence indices.
#[derive(clap::Args, Default)]
struct RawOps {
    /// Stage G: pick the first file whose path contains SUBSTR
    #[arg(long, value_name = "SUBSTR", action = ArgAction::Append)]
    pick_file: Vec<String>,
    /// With --pick-file: deterministic glyph pick (folded row)
    #[arg(long, value_name = "N", action = ArgAction::Append)]
    pick_row: Vec<u32>,
    /// With --pick-file: deterministic glyph pick (folded col)
    #[arg(long, value_name = "M", action = ArgAction::Append)]
    pick_col: Vec<u32>,
    /// Ray pick through physical pixel (X,Y) of the viewport
    #[arg(long, value_names = ["X", "Y"], num_args = 2, action = ArgAction::Append, allow_negative_numbers = true)]
    pick_px: Vec<f32>,
    /// Scripted Fly-camera pose: eye + yaw/pitch in DEGREES (interleaves with
    /// picks/verbs like --pick-px)
    #[arg(long, value_names = ["X", "Y", "Z", "YAW", "PITCH"], num_args = 5, action = ArgAction::Append, allow_negative_numbers = true)]
    cam_pose: Vec<f32>,
    /// Manipulation verb on the most recent pick (repeatable — see VERBS below)
    #[arg(long, value_name = "V [ARGS]", action = ArgAction::Append, value_parser = parse_verb)]
    verb: Vec<Verb>,
}

/// Stage G: one scripted operation (picks and verbs interleave in CLI order).
pub enum Op {
    Pick(PickCommand),
    Verb(Verb),
    /// Scripted Fly-camera pose: eye + yaw/pitch (RADIANS) — repro of
    /// oblique windowed camera states for --pick-px.
    CamPose([f32; 3], f32, f32),
}

/// --pick-row/--pick-col upgrade the most recent --pick-file pick into a
/// deterministic RowCol pick (defaults: row 0 / col 0 for the unset half).
fn set_pick_row_col(ops: &mut Vec<Op>, row: Option<u32>, col: Option<u32>) {
    match ops.last_mut() {
        Some(Op::Pick(PickCommand::File(f))) => {
            let f = f.clone();
            ops.pop();
            ops.push(Op::Pick(PickCommand::RowCol {
                file: f,
                row: row.unwrap_or(0),
                col: col.unwrap_or(0),
            }));
        }
        Some(Op::Pick(PickCommand::RowCol { row: r, col: c, .. })) => {
            if let Some(row) = row {
                *r = row;
            }
            if let Some(col) = col {
                *c = col;
            }
        }
        _ => panic!("--pick-row/--pick-col must follow --pick-file"),
    }
}

/// Parse a `--verb` string into a Verb (clap `value_parser`). Forms:
///   recolor-glyph `[rrggbb]`      recolor-line `[rrggbb]`
///   nudge-glyph dx dy `[dz]`      scale-glyph f
///   move-group dx dy dz         scale-group s
///   tint-group rrggbb           tint-cycle
///   hide-group | show-group | toggle-hidden
fn parse_verb(s: &str) -> Result<Verb, String> {
    let t: Vec<&str> = s.split_whitespace().collect();
    let usage = format!(
        "unknown/malformed --verb {s:?} — expected recolor-glyph|recolor-line|\
         nudge-glyph|scale-glyph|move-group|scale-group|tint-group|tint-cycle|\
         hide-group|show-group|toggle-hidden"
    );
    let f = |i: usize| -> Result<f32, String> {
        t.get(i)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("--verb {s:?}: bad/missing float at position {i}"))
    };
    let hex = |i: usize| -> Result<[u8; 3], String> {
        let h = t
            .get(i)
            .ok_or_else(|| format!("--verb {s:?}: missing rrggbb at position {i}"))?
            .trim_start_matches('#');
        let v = u32::from_str_radix(h, 16)
            .map_err(|_| format!("--verb {s:?}: bad hex color {h:?}"))?;
        Ok([((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8])
    };
    Ok(match t.first().copied().unwrap_or("") {
        "recolor-glyph" => Verb::RecolorGlyph(if t.len() > 1 { hex(1)? } else { [255, 80, 80] }),
        "recolor-line" => Verb::RecolorLine(if t.len() > 1 { hex(1)? } else { [255, 213, 79] }),
        "nudge-glyph" => Verb::NudgeGlyph([f(1)?, f(2)?, if t.len() > 3 { f(3)? } else { 0.0 }]),
        "scale-glyph" => Verb::ScaleGlyph(f(1)?),
        "move-group" => Verb::MoveGroup([f(1)?, f(2)?, f(3)?]),
        "scale-group" => Verb::ScaleGroup(f(1)?),
        "tint-group" => {
            let [r, g, b] = hex(1)?;
            Verb::TintGroup([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
        }
        "tint-cycle" => Verb::TintCycle,
        "hide-group" => Verb::SetHidden(true),
        "show-group" => Verb::SetHidden(false),
        "toggle-hidden" => Verb::ToggleHidden,
        _ => return Err(usage),
    })
}

/// Re-interleave the op stream in true CLI order. clap stores each flag's
/// values separately; `indices_of` yields one argv index PER VALUE (verified
/// against clap_builder 4.6.6: `push_arg_values` pushes an index per value),
/// so for multi-value flags (--pick-px, --cam-pose) both values and indices
/// are chunked by the flag's arity and zipped occurrence-by-occurrence.
fn build_ops(matches: &clap::ArgMatches, raw: &RawOps) -> Vec<Op> {
    enum Keyed {
        PickFile(String),
        Row(u32),
        Col(u32),
        Px(f32, f32),
        CamPose([f32; 3], f32, f32),
        Verb(Verb),
    }
    let indices = |id: &str| -> Vec<usize> {
        matches.indices_of(id).map(Iterator::collect).unwrap_or_default()
    };
    let mut keyed: Vec<(usize, Keyed)> = Vec::new();
    for (i, f) in indices("pick_file").into_iter().zip(raw.pick_file.iter()) {
        keyed.push((i, Keyed::PickFile(f.clone())));
    }
    for (i, r) in indices("pick_row").into_iter().zip(raw.pick_row.iter()) {
        keyed.push((i, Keyed::Row(*r)));
    }
    for (i, c) in indices("pick_col").into_iter().zip(raw.pick_col.iter()) {
        keyed.push((i, Keyed::Col(*c)));
    }
    for (ic, xy) in indices("pick_px").chunks(2).zip(raw.pick_px.as_chunks::<2>().0) {
        keyed.push((ic[0], Keyed::Px(xy[0], xy[1])));
    }
    for (ic, v) in indices("cam_pose").chunks(5).zip(raw.cam_pose.as_chunks::<5>().0) {
        // Degrees on the CLI, radians in the op stream (unchanged semantics).
        keyed.push((
            ic[0],
            Keyed::CamPose([v[0], v[1], v[2]], v[3].to_radians(), v[4].to_radians()),
        ));
    }
    for (i, v) in indices("verb").into_iter().zip(raw.verb.iter()) {
        keyed.push((i, Keyed::Verb(v.clone())));
    }
    keyed.sort_by_key(|(i, _)| *i);

    let mut ops = Vec::new();
    for (_, k) in keyed {
        match k {
            Keyed::PickFile(f) => ops.push(Op::Pick(PickCommand::File(f))),
            Keyed::Row(r) => set_pick_row_col(&mut ops, Some(r), None),
            Keyed::Col(c) => set_pick_row_col(&mut ops, None, Some(c)),
            Keyed::Px(x, y) => ops.push(Op::Pick(PickCommand::Pixel { x, y })),
            Keyed::CamPose(p, yaw, pitch) => ops.push(Op::CamPose(p, yaw, pitch)),
            Keyed::Verb(v) => ops.push(Op::Verb(v)),
        }
    }
    ops
}

/// Parse argv (including `argv[0]`) into a Cli, reconstructing the op stream.
fn parse_cli_from(matches: clap::ArgMatches) -> Cli {
    let mut cli = Cli::from_arg_matches(&matches).expect("clap derive round-trip");
    cli.ops = build_ops(&matches, &cli.raw_ops);
    cli
}

fn parse_cli() -> Cli {
    parse_cli_from(Cli::command().get_matches())
}

/// clap has already refused anything outside the three spellings.
fn parse_present_mode(s: &str) -> wgpu::PresentMode {
    match s {
        "mailbox" => wgpu::PresentMode::Mailbox,
        "immediate" => wgpu::PresentMode::Immediate,
        _ => wgpu::PresentMode::Fifo,
    }
}

fn default_text_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")
}

/// Stage D: drive the Mojo glyph engine in-process over one file.
/// Prints slot count, per-load timing, throughput, and the first records.
fn run_engine_smoke(file: &Path, trie: Option<&Path>, loops: u32) {
    let default_trie = default_engine_trie();
    let trie = trie.unwrap_or(&default_trie);
    let bytes = std::fs::read(file).expect("failed to read --engine-file");

    let mut eng = engine::Engine::new();
    eng.load_trie_file(trie).expect("failed to load engine trie");

    let params = layout::ItemParams::default();
    let mut last_count = 0u64;
    let t0 = std::time::Instant::now();
    for _ in 0..loops {
        last_count = eng.load_item(&bytes, &params).expect("engine load_item failed");
    }
    let dt = t0.elapsed();

    let records = eng.read_back().records;
    assert_eq!(records.len() as u64, last_count, "record copy count mismatch");

    let total_mb = bytes.len() as f64 * loops as f64 / 1e6;
    println!(
        "engine: {} ({} B) x {} loads -> {} records in {:.3} s ({:.1} MB/s pipeline)",
        file.display(),
        bytes.len(),
        loops,
        records.len(),
        dt.as_secs_f64(),
        total_mb / dt.as_secs_f64(),
    );
    for (i, r) in records.iter().take(8).enumerate() {
        println!(
            "  rec[{}]: X={} Y={} Z={} ADVANCE={} HEIGHT={} GLYPH_ID={} ROW={} COL={}",
            i,
            r.x(),
            r.y(),
            r.z(),
            r.advance(),
            r.height(),
            r.glyph_id(),
            r.row(),
            r.col(),
        );
    }
}

/// Stage E1 cross-validation: run the engine through the FFI on `file`, then
/// independently compute the expected records with text.rs's CPU reference
/// (same atlas trie, engine fold conventions) and diff BIT-EXACT — counts and
/// measure bit patterns alike, no tolerance. Exit 1 on any divergence.
fn run_engine_check(file: &Path, trie_path: Option<&Path>) -> ! {
    let default_trie = default_engine_trie();
    let trie_path = trie_path.unwrap_or(&default_trie);
    let bytes = std::fs::read(file).expect("failed to read --engine-check file");
    let trie = atlas::TrieTable::load(&atlas_dir());

    // TWO origins, and the non-zero one is the point. At (0,0,0) an
    // uninitialised origin read is invisible: garbage added to zero on both
    // sides of the comparison agrees with itself. A Mojo nightly was caught
    // doing exactly that (see engine_item_params_at), and for a while
    // ffi_selftest was the only instrument that could see it.
    for origin in [[0.0, 0.0, 0.0], [-3.5, 11.25, 2.75]] {
        let records = engine_layout_records_at(file, trie_path, origin);
        let p = engine_item_params_at(origin);
        let expected = text::reference_layout(
            &trie,
            &bytes,
            [p.origin_x, p.origin_y, p.origin_z],
            p.line_height,
        );
        if let Err(report) = text::diff_records(&records, &expected) {
            eprintln!(
                "engine-check FAIL: {} at origin {origin:?} (trie: {})\n{report}",
                file.display(),
                trie_path.display(),
            );
            std::process::exit(1);
        }
        println!(
            "engine-check PASS: {} ({} B) at origin {origin:?} — {} records bit-exact vs the \
             CPU reference [fp contract=off verified at the dylib] (trie: {})",
            file.display(),
            bytes.len(),
            records.len(),
            trie_path.display(),
        );
    }
    std::process::exit(0);
}

/// Fixture parity: emit the canonical parse manifest, one line per fixture.
fn run_fixture_manifest(paths: &[PathBuf]) -> ! {
    for p in paths {
        match fixture::load_pipe_fixture(p) {
            Ok(fx) => println!("{}", fx.manifest()),
            Err(e) => {
                eprintln!("fixture-manifest FAIL: {e}");
                std::process::exit(1);
            }
        }
    }
    std::process::exit(0);
}

/// Bake: the bake and its seed protocol against the .bake.bin corpus.
fn run_fixture_bake(paths: &[PathBuf]) -> ! {
    let mut leaders = 0usize;
    let mut checkpoints = 0usize;
    let mut queries = 0usize;
    let mut failed = 0usize;
    for p in paths {
        let fx = match bake::load_bake_fixture(p) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("fixture-bake FAIL: {e}");
                std::process::exit(1);
            }
        };
        let d = bake::diff_bake(&fx);
        if d.bad.is_empty() {
            println!(
                "  PASS {:<28} {} leaders / {} checkpoints / {} prefix + {} wrap queries",
                fx.name, d.leaders, d.checkpoints, d.prefix_queries, d.wrap_queries
            );
            leaders += d.leaders;
            checkpoints += d.checkpoints;
            queries += d.prefix_queries + d.wrap_queries;
        } else {
            failed += 1;
            println!("  FAIL {:<28} {} disagreement(s)", fx.name, d.bad.len());
            for line in d.bad.iter().take(8) {
                println!("       {line}");
            }
        }
    }
    if failed > 0 {
        eprintln!("fixture-bake FAIL: {failed}/{} fixtures differ", paths.len());
        std::process::exit(1);
    }
    // ANTI-VACUITY. The QUERY half is what distinguishes this from a second
    // whole-file record comparison: a bake with subtly wrong checkpoints answers
    // every total correctly and every random-access question wrongly. A run with
    // no queries would be reporting only the half that cannot see that.
    if queries == 0 {
        eprintln!("fixture-bake FAIL: no seed-protocol query was exercised");
        std::process::exit(1);
    }
    println!(
        "fixture-bake PASS: {} fixture(s), {leaders} leaders, {checkpoints} checkpoints, \
         {queries} seed-protocol queries bit-exact",
        paths.len()
    );
    std::process::exit(0);
}

/// The tunings the scan form sweeps: (chunk_size, group_size, shards).
///
/// The Mojo suite runs two — the default and one awkward pair. Being serial
/// makes more of them cheap, and each one is a different GROUPING of the same
/// monoid, so the sweep is the associativity test. The degenerate ones matter
/// most: chunk 1 makes every byte its own interval, group 1 removes the spine's
/// grouping entirely, and a large chunk makes the whole buffer one interval so
/// nothing is combined at all. `shards` is the dial on resolve_x's segment walk
/// — with one shard per item the walk NEVER fires, because the first leader of
/// an item always starts a segment.
const SCAN_TUNINGS: &[(usize, usize, usize)] = &[
    // The GPU's own defaults come from scan.rs so the sweep cannot drift from
    // the tuning the shipped path would use.
    (scan::DEFAULT_CHUNK_SIZE, scan::DEFAULT_GROUP_SIZE, 1),
    (scan::DEFAULT_CHUNK_SIZE, scan::DEFAULT_GROUP_SIZE, 4), // resolve_x shards mid-segment
    (7, 3, 3),      // seams inside multi-byte sequences AND fold units
    (1, 1, 8),      // every byte its own interval, no grouping
    (3, 1, 2),
    (1, 64, 5),
    (4096, 8, 7),   // one interval: nothing combines, everything shards
    (13, 5, 11),
];

/// Scan form: the scan form against the corpus, swept across tunings.
fn run_fixture_scan(paths: &[PathBuf]) -> ! {
    let mut failed = 0usize;
    let mut strict = 0usize;
    let mut tiered = 0usize;
    let mut cases = 0usize;
    for p in paths {
        let fx = match fixture::load_pipe_fixture(p) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("fixture-scan FAIL: {e}");
                std::process::exit(1);
            }
        };
        let mut worst: Option<fixture::ScanDiff> = None;
        let mut ok = true;
        for &(chunk, group, shards) in SCAN_TUNINGS {
            let d = fixture::diff_scan(&fx, chunk, group, shards);
            cases += 1;
            strict += d.bit_exact_leaders;
            tiered += d.tiered_leaders;
            if !d.bad.is_empty() {
                ok = false;
                if worst.is_none() {
                    worst = Some(d);
                }
            }
        }
        if ok {
            println!("  PASS {:<26} {} tunings within the tiered contract", fx.name, SCAN_TUNINGS.len());
        } else {
            failed += 1;
            let d = worst.unwrap();
            println!(
                "  FAIL {:<26} K={}/G={}/S={} — {} mismatch(es)",
                fx.name, d.chunk_size, d.group_size, d.shards, d.bad.len()
            );
            for line in d.bad.iter().take(8) {
                println!("       {line}");
            }
        }
    }
    if failed > 0 {
        eprintln!("fixture-scan FAIL: {failed}/{} fixtures differ", paths.len());
        std::process::exit(1);
    }
    // ANTI-VACUITY. The strict tier is the load-bearing one — it is where the
    // scan claims BIT equality with the serial fold — so a run that held no
    // leader to it would be reporting the tolerant tier's green as the whole
    // result.
    if strict == 0 {
        eprintln!("fixture-scan FAIL: no leader was held to the BIT-exact tier");
        std::process::exit(1);
    }
    println!(
        "fixture-scan PASS: {} fixture(s) x {} tunings = {cases} cases; \
         {strict} leader-lanes BIT-exact, {tiered} within 1e-4 relative",
        paths.len(),
        SCAN_TUNINGS.len(),
    );
    std::process::exit(0);
}

/// Full fold: the ported fold against the whole corpus, every lane of every byte.
fn run_fixture_fold(paths: &[PathBuf]) -> ! {
    let mut lanes = 0usize;
    let mut leaders = 0usize;
    let mut failed = 0usize;
    for p in paths {
        let fx = match fixture::load_pipe_fixture(p) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("fixture-fold FAIL: {e}");
                std::process::exit(1);
            }
        };
        let d = fixture::diff_full_fold(&fx);
        if d.bad.is_empty() {
            println!(
                "  PASS {:<26} {} bytes / {} leaders / {} lanes bit-exact",
                fx.name, d.bytes, d.leaders, d.lanes
            );
            lanes += d.lanes;
            leaders += d.leaders;
        } else {
            failed += 1;
            println!("  FAIL {:<26} {} disagreement(s)", fx.name, d.bad.len());
            for line in d.bad.iter().take(8) {
                println!("       {line}");
            }
        }
    }
    if failed > 0 {
        eprintln!("fixture-fold FAIL: {failed}/{} fixtures differ", paths.len());
        std::process::exit(1);
    }
    println!(
        "fixture-fold PASS: {} fixture(s), {leaders} leaders, {lanes} per-byte lanes \
         bit-exact vs the JS oracle",
        paths.len()
    );
    std::process::exit(0);
}

/// Trie rebuild: rebuild every fixture's trie from its bytes and diff it against the
/// one the oracle stored.
///
/// NOT a round trip: the input is the fixture's raw BYTES plus gen.mjs's pure
/// metrics function, and nothing about the stored trie's structure is handed
/// back to the builder. So the block layout, the content dedup and — the part
/// that matters — the INSERTION ORDER are all under test.
fn run_fixture_trie(paths: &[PathBuf]) -> ! {
    let mut entries = 0usize;
    let mut failed = 0usize;
    for p in paths {
        let fx = match fixture::load_pipe_fixture(p) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("fixture-trie FAIL: {e}");
                std::process::exit(1);
            }
        };
        let r = fixture::rebuild_trie_and_diff(&fx);
        if r.bad.is_empty() {
            println!(
                "  PASS {:<26} {} entries / {} blocks / {} mapped cps — value-identical",
                fx.name, r.entries, r.block_count, r.mapped
            );
            entries += r.entries;
        } else {
            failed += 1;
            println!("  FAIL {:<26} {} disagreement(s)", fx.name, r.bad.len());
            for line in r.bad.iter().take(8) {
                println!("       {line}");
            }
        }
    }
    if failed > 0 {
        eprintln!("fixture-trie FAIL: {failed}/{} fixtures differ", paths.len());
        std::process::exit(1);
    }
    println!(
        "fixture-trie PASS: {} fixture(s), {entries} trie entries rebuilt from bytes",
        paths.len()
    );
    std::process::exit(0);
}

/// Fixture parity: hold text.rs's CPU fold to the fixture corpus, bit-exact.
///
/// Out-of-domain fixtures are SKIPPED WITH A REASON rather than silently
/// dropped, and a run in which nothing was in domain FAILS. Both halves matter:
/// this gate's whole job is comparing, and a comparison that compared nothing
/// is the loudest-passing thing there is.
fn run_fixture_reference(paths: &[PathBuf]) -> ! {
    let mut compared = 0usize;
    let mut records = 0usize;
    let mut lanes = 0usize;
    let mut failed = 0usize;
    for p in paths {
        let fx = match fixture::load_pipe_fixture(p) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("fixture-reference FAIL: {e}");
                std::process::exit(1);
            }
        };
        let outcome = fixture::diff_against_reference_layout(&fx);
        if let Some(why) = outcome.skipped {
            println!("  SKIP {:<26} out of reference_layout domain: {why}", fx.name);
            continue;
        }
        compared += 1;
        records += outcome.records;
        lanes += outcome.compared_lanes;
        if outcome.bad.is_empty() {
            println!(
                "  PASS {:<26} {} records, {} lanes bit-exact",
                fx.name, outcome.records, outcome.compared_lanes
            );
        } else {
            failed += 1;
            print!("  FAIL {}", fixture::report(&fx, &outcome.bad, 10));
        }
    }
    if compared == 0 {
        eprintln!("fixture-reference FAIL: no fixture was in domain — nothing was compared");
        std::process::exit(1);
    }
    if failed > 0 {
        eprintln!("fixture-reference FAIL: {failed}/{compared} fixtures differ");
        std::process::exit(1);
    }
    println!(
        "fixture-reference PASS: {compared} fixture(s), {records} records, {lanes} lanes bit-exact \
         vs the oracle's expected values"
    );
    std::process::exit(0);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
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

    // Fixture parity (reference port): fixture parse manifest / corpus diff — no GPU.
    if !cli.fixture_manifest.is_empty() {
        run_fixture_manifest(&cli.fixture_manifest);
    }
    if !cli.fixture_reference.is_empty() {
        run_fixture_reference(&cli.fixture_reference);
    }
    if !cli.fixture_trie.is_empty() {
        run_fixture_trie(&cli.fixture_trie);
    }
    if !cli.fixture_fold.is_empty() {
        run_fixture_fold(&cli.fixture_fold);
    }
    if !cli.fixture_scan.is_empty() {
        run_fixture_scan(&cli.fixture_scan);
    }
    if !cli.fixture_bake.is_empty() {
        run_fixture_bake(&cli.fixture_bake);
    }

    // Stage E1: engine ↔ CPU-reference cross-check — no GPU involved.
    if let Some(file) = &cli.engine_check {
        run_engine_check(file, cli.engine_trie.as_deref());
    }

    // Stage D: Mojo engine in-process smoke test — no GPU involved.
    if let Some(file) = &cli.engine_file {
        run_engine_smoke(file, cli.engine_trie.as_deref(), cli.engine_loop);
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

// ── Stage H: CLI parity tests ────────────────────────────────────────────────
// Pin the clap migration against the hand-rolled parser's semantics: same
// flags, same defaults, same op-stream interleaving, same pick-row/col upgrade
// rules (including the error path), same last-wins scalar repeats.
#[cfg(test)]
mod cli_tests {
    use super::*;

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let argv = std::iter::once("glyph3d-native").chain(args.iter().copied());
        let matches = Cli::command().try_get_matches_from(argv)?;
        Ok(parse_cli_from(matches))
    }

    fn parse(args: &[&str]) -> Cli {
        try_parse(args).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    #[test]
    fn defaults_match_old_parser() {
        let cli = parse(&[]);
        assert!(cli.screenshot.is_none());
        assert_eq!(cli.frames, 1);
        assert!(!cli.demo);
        assert!(cli.render_file.is_none());
        assert_eq!(cli.copies, 1);
        assert_eq!(cli.zoom, 1.0);
        assert!(cli.engine_file.is_none());
        assert!(cli.engine_trie.is_none());
        assert!(cli.emoji_sheet.is_none());
        assert_eq!(cli.engine_loop, 1);
        assert!(cli.engine_check.is_none());
        assert!(cli.engine_render.is_none());
        assert!(cli.load_repo.is_none());
        // The DEFAULT is `direct` since 2026-09-07. This assertion is not
        // decoration: three golden views and, until it was pinned, the
        // repo-verify gate all inherit this value, so a change here silently
        // changes what they exercise.
        assert_eq!(cli.repo_engine, "direct");
        // THE DEFAULT THE SCREENSHOT BASELINES DEPEND ON. A change here moves
        // repo-wide.png and repo-zoom.png, so it is pinned in the CLI layer too
        // and not only in RepoParams::default.
        assert_eq!(cli.wrap_mode, "back");
        assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
        // ...and the non-default is still reachable and still spelled the same.
        assert_eq!(parse_wrap_mode("down"), fold::WrapMode::Down);
        // Same standing as wrap_mode: the wrap staircase's pitch moves the
        // same baselines (repo-wide renders the staircase), so the CLI layer
        // pins the default too, not only RepoParams::default (repo.rs).
        assert_eq!(cli.z_wrap_spacing, 0.15);
        // The sequence pass: leader is the default every baseline renders;
        // cluster is the reachable other spelling.
        assert_eq!(cli.cluster_mode, "leader");
        assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Leader);
        assert_eq!(parse_cluster_mode("cluster"), fold::ClusterMode::Cluster);
        assert!(!cli.repo_verify);
        assert!(cli.focus_file.is_none());
        assert!(!cli.repo_scan_only);
        assert!(!cli.no_cull);
        assert!(!cli.no_ui);
        assert!(cli.screenshot_frame.is_none());
        assert!(cli.screenshot_out.is_none());
        assert!(cli.ops.is_empty());
        assert!(!cli.gpu_key);
        assert!(!cli.gpu_profile);
        // Fifo is the default because it is what every FPS figure before
        // 2026-09-07 was measured under; changing it would make old numbers
        // incomparable without saying so.
        assert_eq!(cli.present_mode, "fifo");
        assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Fifo);
    }

    #[test]
    fn scalar_flags_parse() {
        let cli = parse(&[
            "--screenshot", "out.png", "--frames", "2", "--demo", "--copies", "3", "--zoom",
            "2.5", "--no-cull", "--no-ui", "--engine-loop", "4", "--load-repo", "fixtures/g-pick-repo",
            "--repo-engine", "batch", "--repo-verify", "--focus-file", "alpha",
            "--wrap-mode", "back", "--z-wrap-spacing", "0.6", "--cluster-mode", "cluster",
            "--render-file", "src/main.rs", "--engine-file", "a.rs", "--engine-trie", "t.bin",
            "--engine-check", "b.rs", "--engine-render", "c.rs",
            "--present-mode", "mailbox", "--gpu-key", "--gpu-profile",
            "--emoji-sheet", "sheets/other.bin",
        ]);
        assert_eq!(cli.emoji_sheet, Some(PathBuf::from("sheets/other.bin")));
        assert_eq!(cli.present_mode, "mailbox");
        assert_eq!(parse_present_mode(&cli.present_mode), wgpu::PresentMode::Mailbox);
        assert!(cli.gpu_key);
        assert!(cli.gpu_profile);
        assert_eq!(cli.screenshot, Some(PathBuf::from("out.png")));
        assert_eq!(cli.frames, 2);
        assert!(cli.demo);
        assert_eq!(cli.copies, 3);
        assert_eq!(cli.zoom, 2.5);
        assert!(cli.no_cull);
        assert!(cli.no_ui);
        assert_eq!(cli.engine_loop, 4);
        assert_eq!(cli.load_repo, Some(PathBuf::from("fixtures/g-pick-repo")));
        assert_eq!(cli.repo_engine, "batch");
        assert_eq!(cli.wrap_mode, "back");
        assert_eq!(parse_wrap_mode(&cli.wrap_mode), fold::WrapMode::Back);
        assert_eq!(cli.z_wrap_spacing, 0.6);
        assert_eq!(cli.cluster_mode, "cluster");
        assert_eq!(parse_cluster_mode(&cli.cluster_mode), fold::ClusterMode::Cluster);
        assert!(cli.repo_verify);
        assert_eq!(cli.focus_file.as_deref(), Some("alpha"));
        assert_eq!(cli.render_file, Some(PathBuf::from("src/main.rs")));
        assert_eq!(cli.engine_file, Some(PathBuf::from("a.rs")));
        assert_eq!(cli.engine_trie, Some(PathBuf::from("t.bin")));
        assert_eq!(cli.engine_check, Some(PathBuf::from("b.rs")));
        assert_eq!(cli.engine_render, Some(PathBuf::from("c.rs")));
    }

    /// An unknown mode is REFUSED at the boundary, not folded to the default.
    /// A silent fallback would lay out mode A while the operator asked for B,
    /// and nothing downstream could tell them apart from a correct mode-A run.
    #[test]
    fn an_unknown_wrap_mode_is_refused() {
        let text = match try_parse(&["--wrap-mode", "sideways"]) {
            Ok(_) => panic!("clap must refuse an unknown wrap mode"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("sideways"), "the error must name the bad value: {text}");
    }

    /// Same rule as the wrap mode's: an unknown cluster mode is REFUSED at the
    /// boundary, not folded to the default — a silent fallback would render
    /// mode A while the operator believed they asked for B.
    #[test]
    fn an_unknown_cluster_mode_is_refused() {
        let text = match try_parse(&["--cluster-mode", "grapheme"]) {
            Ok(_) => panic!("clap must refuse an unknown cluster mode"),
            Err(e) => e.to_string(),
        };
        assert!(text.contains("grapheme"), "the error must name the bad value: {text}");
    }

    /// Out-of-domain spacing is REFUSED at the boundary with the flag and the
    /// value named — not passed down to ItemParams::validate, whose panic
    /// would say "z_step" and nothing about the command line. NaN is the
    /// dangerous one (two NaN layouts compare bit-equal — see
    /// ItemParams::validate); negative is a forward staircase no baseline
    /// has ever rendered.
    #[test]
    fn an_out_of_domain_z_wrap_spacing_is_refused() {
        for bad in ["-0.1", "NaN", "inf", "abc"] {
            let text = match try_parse(&["--z-wrap-spacing", bad]) {
                Ok(_) => panic!("clap must refuse --z-wrap-spacing {bad}"),
                Err(e) => e.to_string(),
            };
            assert!(text.contains("--z-wrap-spacing"), "error must name the flag: {text}");
        }
        // The boundary itself is in domain: 0 is the documented flat layout.
        let cli = parse(&["--z-wrap-spacing", "0"]);
        assert_eq!(cli.z_wrap_spacing, 0.0);
    }

    #[test]
    fn repeated_scalar_flag_is_last_wins() {
        // The hand-rolled parser silently took the last occurrence.
        let cli = parse(&["--zoom", "2", "--zoom", "3"]);
        assert_eq!(cli.zoom, 3.0);
    }

    #[test]
    fn op_stream_preserves_cli_order() {
        let cli = parse(&[
            "--pick-file", "alpha", "--pick-row", "4", "--pick-col", "4",
            "--verb", "recolor-line ff0000",
            "--cam-pose", "1", "2", "3", "30", "-10",
            "--pick-px", "100", "200",
            "--verb", "tint-cycle",
            "--pick-px", "300", "400",
            "--pick-file", "beta",
            "--verb", "hide-group",
        ]);
        assert_eq!(cli.ops.len(), 8);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "alpha");
                assert_eq!(*row, 4);
                assert_eq!(*col, 4);
            }
            _ => panic!("op[0] should be the upgraded RowCol pick"),
        }
        assert!(matches!(cli.ops[1], Op::Verb(Verb::RecolorLine([255, 0, 0]))));
        match &cli.ops[2] {
            Op::CamPose(p, yaw, pitch) => {
                assert_eq!(*p, [1.0, 2.0, 3.0]);
                assert!((yaw - 30f32.to_radians()).abs() < 1e-6);
                assert!((pitch - (-10f32).to_radians()).abs() < 1e-6);
            }
            _ => panic!("op[2] should be CamPose"),
        }
        assert!(matches!(
            cli.ops[3],
            Op::Pick(PickCommand::Pixel { x: 100.0, y: 200.0 })
        ));
        assert!(matches!(cli.ops[4], Op::Verb(Verb::TintCycle)));
        assert!(matches!(
            cli.ops[5],
            Op::Pick(PickCommand::Pixel { x: 300.0, y: 400.0 })
        ));
        assert!(matches!(&cli.ops[6], Op::Pick(PickCommand::File(f)) if f == "beta"));
        assert!(matches!(&cli.ops[7], Op::Verb(Verb::SetHidden(true))));
    }

    #[test]
    fn pick_file_beta_is_in_stream() {
        let cli = parse(&["--pick-px", "1", "2", "--pick-file", "beta", "--pick-row", "7"]);
        assert_eq!(cli.ops.len(), 2);
        assert!(matches!(
            cli.ops[0],
            Op::Pick(PickCommand::Pixel { x: 1.0, y: 2.0 })
        ));
        match &cli.ops[1] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "beta");
                assert_eq!(*row, 7);
                assert_eq!(*col, 0);
            }
            _ => panic!("op[1] should be RowCol beta 7 0"),
        }
    }

    #[test]
    fn pick_row_col_upgrade_semantics() {
        // Repeated upgrades mutate the same pick (row/col defaults 0).
        let cli = parse(&["--pick-file", "a", "--pick-row", "1", "--pick-row", "2"]);
        assert_eq!(cli.ops.len(), 1);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { file, row, col }) => {
                assert_eq!(file, "a");
                assert_eq!(*row, 2);
                assert_eq!(*col, 0);
            }
            _ => panic!("expected RowCol"),
        }
        let cli = parse(&["--pick-file", "a", "--pick-col", "5", "--pick-row", "3"]);
        match &cli.ops[0] {
            Op::Pick(PickCommand::RowCol { row, col, .. }) => {
                assert_eq!(*row, 3);
                assert_eq!(*col, 5);
            }
            _ => panic!("expected RowCol"),
        }
        // No row/col: plain File pick survives.
        let cli = parse(&["--pick-file", "a"]);
        assert!(matches!(&cli.ops[0], Op::Pick(PickCommand::File(f)) if f == "a"));
    }

    #[test]
    #[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
    fn pick_row_without_pick_file_panics() {
        parse(&["--pick-row", "1"]);
    }

    #[test]
    #[should_panic(expected = "--pick-row/--pick-col must follow --pick-file")]
    fn pick_row_after_verb_panics() {
        parse(&["--pick-file", "a", "--verb", "tint-cycle", "--pick-row", "1"]);
    }

    #[test]
    fn unknown_flag_errors() {
        let err = try_parse(&["--bogus"]).err().expect("--bogus must fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn help_is_display_help() {
        // -h/--help surface as DisplayHelp via try_get_matches_from; the real
        // binary exits 0 through clap's default error exit path.
        let err = Cli::command()
            .try_get_matches_from(["glyph3d-native", "--help"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn repo_engine_rejects_bad_value() {
        assert!(try_parse(&["--repo-engine", "turbo"]).is_err());
    }

    #[test]
    fn verb_error_messages_survive() {
        let err = parse_verb("frobnicate 1 2").err().expect("bad verb must fail");
        assert!(err.contains("unknown/malformed --verb"), "{err}");
        let err = parse_verb("nudge-glyph 1").err().expect("bad verb must fail");
        assert!(err.contains("bad/missing float at position 2"), "{err}");
        let err = parse_verb("tint-group zzzz").err().expect("bad verb must fail");
        assert!(err.contains("bad hex color"), "{err}");
        let err = try_parse(&["--verb", "frobnicate"]).err().expect("bad verb must fail");
        assert!(err.to_string().contains("unknown/malformed --verb"), "{err}");
    }

    #[test]
    fn verb_defaults_and_forms() {
        assert!(matches!(
            parse_verb("recolor-glyph").unwrap(),
            Verb::RecolorGlyph([255, 80, 80])
        ));
        assert!(matches!(
            parse_verb("recolor-line").unwrap(),
            Verb::RecolorLine([255, 213, 79])
        ));
        assert!(matches!(
            parse_verb("nudge-glyph 1 2").unwrap(),
            Verb::NudgeGlyph(v) if v == [1.0, 2.0, 0.0]
        ));
        assert!(matches!(
            parse_verb("move-group 1 2 3").unwrap(),
            Verb::MoveGroup(v) if v == [1.0, 2.0, 3.0]
        ));
        // Every channel is asserted, and the literal is chosen so that no
        // channel can be right by accident. `ff0080` could not do this job:
        // 0xFF survives `& -> |`, 0x00 survives `>> -> <<`, and blue was never
        // checked at all — seven distinct mutations of the hex decoder hid
        // behind one badly-chosen colour, all of them invisible to the full
        // twelve-gate battery. 0x12/0x34/0x56 are distinct, and none is 0 or
        // 255, so a wrong mask, a wrong shift or a wrong divisor all move a
        // value this test reads.
        let Verb::TintGroup(v) = parse_verb("tint-group 123456").unwrap() else {
            panic!("tint-group did not parse to TintGroup");
        };
        assert!((v[0] - 18.0 / 255.0).abs() < 1e-6, "red channel: {v:?}");
        assert!((v[1] - 52.0 / 255.0).abs() < 1e-6, "green channel: {v:?}");
        assert!((v[2] - 86.0 / 255.0).abs() < 1e-6, "blue channel: {v:?}");

        // The explicit-colour form, which is what makes the `t.len() > 1`
        // arity checks falsifiable: with only the bare verb tested, `>` and `<`
        // both fall through to the default and the check is unobservable.
        assert!(matches!(
            parse_verb("recolor-glyph aabbcc").unwrap(),
            Verb::RecolorGlyph([0xAA, 0xBB, 0xCC])
        ));
        assert!(matches!(
            parse_verb("recolor-line 0a141e").unwrap(),
            Verb::RecolorLine([0x0A, 0x14, 0x1E])
        ));
        assert!(matches!(parse_verb("show-group").unwrap(), Verb::SetHidden(false)));
        assert!(matches!(parse_verb("toggle-hidden").unwrap(), Verb::ToggleHidden));
        assert!(matches!(
            parse_verb("scale-glyph 2.5").unwrap(),
            Verb::ScaleGlyph(s) if s == 2.5
        ));
        assert!(matches!(
            parse_verb("scale-group 0.5").unwrap(),
            Verb::ScaleGroup(s) if s == 0.5
        ));
    }

    #[test]
    fn screenshot_frame_flags_parse() {
        // Stage K (K6): the windowed capture pair parses together.
        let cli = parse(&["--screenshot-frame", "90", "--screenshot-out", "/tmp/shot.png"]);
        assert_eq!(cli.screenshot_frame, Some(90));
        assert_eq!(cli.screenshot_out, Some(PathBuf::from("/tmp/shot.png")));
        // They require each other.
        assert!(try_parse(&["--screenshot-frame", "90"]).is_err());
        assert!(try_parse(&["--screenshot-out", "/tmp/shot.png"]).is_err());
        // Windowed-only: clap rejects combining them with offscreen --screenshot.
        assert!(try_parse(&[
            "--screenshot", "out.png", "--screenshot-frame", "90", "--screenshot-out", "/tmp/s.png",
        ])
        .is_err());
    }

    #[test]
    fn generate_flag_parses() {
        let cli = parse(&["--generate", "bash"]);
        assert_eq!(cli.generate, Some(clap_complete::Shell::Bash));
        assert!(try_parse(&["--generate", "tcsh"]).is_err());
    }

    #[test]
    fn negative_numbers_in_cam_pose_and_pick_px() {
        // The old parser took the next raw tokens unconditionally; clap needs
        // allow_negative_numbers to match that for oblique camera repros.
        let cli = parse(&["--cam-pose", "-1", "-2", "-3", "-180", "-45"]);
        match &cli.ops[0] {
            Op::CamPose(p, yaw, pitch) => {
                assert_eq!(*p, [-1.0, -2.0, -3.0]);
                assert!((yaw - (-180f32).to_radians()).abs() < 1e-6);
                assert!((pitch - (-45f32).to_radians()).abs() < 1e-6);
            }
            _ => panic!("expected CamPose"),
        }
    }
}
