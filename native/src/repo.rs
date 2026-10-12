//! Stage E2 — repository-scale loading.
//!
//! Walks a repository (source-extension whitelist; VCS/build/dependency dirs
//! skipped), lays every file out through the layout seam (`HyperLayout`,
//! whatever `--repo-engine` says), and fills ONE
//! glyph arena. The backend writes instances straight into the arena; only a
//! `--repo-verify` run asks it for the 32 B wire records as well.
//! `--repo-engine` selects; `--repo-verify` diffs against a recording
//! `HyperLayout` reference. Each file is one VIEW (`crate::views`: its
//! item row is the file's index) drawn with its own GROUP row, placed on a
//! 2D grid of code pages; files are views {slot range, engine params} into
//! the shared arena — the web's MegaGlyphField architecture (one arena,
//! files as views). The group row is the file's index too, except under the
//! registry's witness (`group_rows_of_views`); nothing downstream assumes it.
//!
//! Correctness guards:
//! - only valid UTF-8 files reach the layout (valid UTF-8 cannot decode a
//!   codepoint past the trie's 4352-entry block index — the decode assumes
//!   well-formed leads);
//! - the 10 MB per-file cap also keeps every item far under 2^24 bytes, the
//!   point past which a per-item ordinal carried in an f32 would alias
//!   (`schema/glyph-identity.json` states the rule).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::glyph_scene::{GlyphInstance, GroupRow};
use crate::layout::{
    diff_backends, BackendOutput, GlyphArena, ItemParams, LayoutGlyphs,
    LayoutItem, Paint, VerifyLayout,
};
use crate::text::{self, StagedText};

pub use crate::layout_stack::{LayoutController, LayoutStack, LayoutZone, SpatialLayoutStrategy};

/// Layout engine strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Strategy {
    #[default]
    #[value(name = "hyper")]
    Hyper,
    #[value(name = "direct")]
    Direct,
    #[value(name = "batch")]
    Batched,
    #[value(name = "naive")]
    PerItem,
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Strategy::Hyper => write!(f, "hyper"),
            Strategy::Direct => write!(f, "direct"),
            Strategy::Batched => write!(f, "batch"),
            Strategy::PerItem => write!(f, "naive"),
        }
    }
}

impl std::str::FromStr for Strategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hyper" => Ok(Strategy::Hyper),
            "direct" => Ok(Strategy::Direct),
            "batch" => Ok(Strategy::Batched),
            "naive" => Ok(Strategy::PerItem),
            other => Err(format!("unknown strategy {other:?}")),
        }
    }
}

impl Strategy {
    pub fn can_record(&self) -> bool {
        matches!(self, Strategy::Hyper | Strategy::Batched | Strategy::PerItem)
    }

    pub fn materializes_records(&self) -> bool {
        matches!(self, Strategy::Batched | Strategy::PerItem)
    }
}

/// Stage-timing metrics for backend execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct BackendPhases {
    pub fold: Duration,
    pub readback_alloc: Duration,
    pub readback_copy: Duration,
    pub compact: Duration,
}

impl BackendPhases {
    pub fn readback(&self) -> Duration {
        self.readback_alloc + self.readback_copy
    }

    pub fn engine_ranked(&self) -> Vec<(&'static str, Duration)> {
        Vec::new()
    }
}

mod walk;
pub use walk::{walk_repo, RepoFile, WalkResult, MAX_FILE_BYTES, SKIP_DIRS, SOURCE_EXTENSIONS};

/// All layout dials for the repo field in one struct.
#[derive(Clone, Copy, Debug)]
pub struct RepoParams {
    /// Wrap each file at this many glyph columns (engine fold unit).
    pub wrap_cols: i32,
    /// Line pitch in world units.
    pub line_height: f64,
    /// Engine pagination: rows per page — long files fan out as side-by-side
    /// pages of (wrap_cols × page_rows), `max_pages_wide` per band, bands
    /// stacked `page_rows*line_height + band_gap_y` apart. This bounds every
    /// file's footprint regardless of its line count (no page is taller than
    /// page_rows), which keeps the field's ink density uniform.
    pub page_rows: i32,
    pub max_pages_wide: i32,
    /// World-space gap between a file's fanned pages.
    pub page_gap_x: f64,
    pub band_gap_y: f64,
    /// World-space gaps between file footprints on the shelf grid.
    pub gap_x: f32,
    pub gap_y: f32,
    /// Target width/height aspect for the page grid.
    pub grid_aspect: f32,
    /// THE WRAP STAIRCASE. Z step per intra-line wrap segment, as a multiple
    /// of the em cell height — the web's `zWrapSpacing`
    /// (`workers/builders/index.js`, default 0.15), and `ItemParams::z_step`
    /// is this times `CELL_HEIGHT_WORLD`, exactly as `CodeGrid.js:1365`
    /// computes it.
    ///
    /// WHY IT IS A DIAL AND NOT A CONSTANT. This is three-dimensional word
    /// wrap: when a single logical line runs past `wrap_cols`, the fold rolls
    /// it to the next row AND steps it back in Z, so a wrapped line reads as
    /// a staircase receding from the viewer instead of a flat block that
    /// looks like separate lines. The degenerate case is what proves it — a
    /// few hundred thousand characters of minified JSON with no newline at
    /// all becomes a legible wrapped page with visible depth, rather than one
    /// endless thin rectangle.
    ///
    /// The fold has always computed this (`fold.rs`: `wrap_segment = col /
    /// wrap_width`, then `z = origin_z - wrap_segment * z_step`) and the
    /// `.pipe.bin` corpus has always gated it. THIS call site was passing 0,
    /// so every repo render was flat — the algorithm was ported and then
    /// never wired. Setting it to 0 restores that flatness exactly.
    pub z_wrap_spacing: f64,
    /// HOW A WRAP IS SPENT. `WrapDown` advances the visual row, so
    /// a 305,978-character line in a real bundle takes 3,060 rows and pushes
    /// every later line that far into the distance. `WrapBack` (the default
    /// since dde3f82) keeps the row and spends the wrap in depth instead, so
    /// the same line is ONE row and the derangement goes into the axis nothing
    /// else on the shelf is using.
    ///
    /// Measured on `native/fixtures/visual-check/one-long-line.txt`: 440 rows in
    /// WrapDown, 1 in WrapBack. `repo-wide` renders the default mode and
    /// `repo-down` keeps the other covered, so a default here is pinned by a
    /// golden frame either way (root AGENTS.md, "Which view covers what").
    pub wrap_mode: crate::fold::WrapMode,
    /// THE SEQUENCE PASS, per item: whether a codepoint sequence the font draws
    /// as ONE glyph (ZWJ families, RI flags, skin tones, keycaps) resolves to
    /// its sequence slot, with trailing leaders zeroed. Leader is the
    /// params-level default (ItemParams agrees — `text::reference_layout`,
    /// which resolves no sequences, depends on that); the CLI/product default flipped to cluster on
    /// 2026-09-22, and the goldens pin their mode explicitly.
    pub cluster_mode: crate::fold::ClusterMode,
    /// TAB STOPS, per item (`fold::TabWidth`, 2026-10-11): a tab advances to
    /// the next multiple of this many display cells of its line. The default
    /// is the `[layout] tab_width_cells` setting (one cell — the layout before
    /// stops, and every golden's); `--tab-width` sets it per load.
    pub tab_width: crate::fold::TabWidth,
    /// Spatial arrangement mode for the repository files across the canvas.
    pub layout_mode: RepoLayoutMode,
    /// The glyph field the device layout emits for: `Instanced` (32 B
    /// `RenderSlot`) or `Derived` (20 B `DerivedSlot` + line table). The
    /// HyperLayout device path writes the chosen format straight into the
    /// GPU arena — no host transcode between layout and field.
    pub field_mode: glyph_field::GlyphFieldMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum RepoLayoutMode {
    /// Traditional height-classed shelf packing across the whole repo.
    #[default]
    Shelf,
    /// Hierarchical directory-based neighborhood carrels.
    Carrel,
    /// Every file a book contain-fitted onto a uniform page; directories as
    /// pageable volumes, nested one depth step back (`library`).
    Library,
}

impl std::fmt::Display for RepoLayoutMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoLayoutMode::Shelf => write!(f, "shelf"),
            RepoLayoutMode::Carrel => write!(f, "carrel"),
            RepoLayoutMode::Library => write!(f, "library"),
        }
    }
}

impl std::str::FromStr for RepoLayoutMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "shelf" => Ok(RepoLayoutMode::Shelf),
            "carrel" => Ok(RepoLayoutMode::Carrel),
            "library" => Ok(RepoLayoutMode::Library),
            other => Err(format!("unknown layout mode {other:?}")),
        }
    }
}

impl Default for RepoParams {
    fn default() -> Self {
        Self {
            wrap_cols: 100,
            line_height: (text::CELL_HEIGHT_WORLD * text::LINE_HEIGHT_FACTOR) as f64,
            page_rows: 128,
            max_pages_wide: 32,
            page_gap_x: 4.0,
            band_gap_y: 6.0,
            gap_x: 2.5,
            gap_y: 6.0,
            grid_aspect: 1.6,
            z_wrap_spacing: 0.15,
            wrap_mode: crate::fold::WrapMode::Back,
            cluster_mode: crate::fold::ClusterMode::Leader,
            tab_width: crate::config::settings().layout.tab_width(),
            layout_mode: RepoLayoutMode::Shelf,
            field_mode: glyph_field::GlyphFieldMode::default(),
        }
    }
}

/// Per-file engine params: shared wrap/line pitch, plus pagination sized from
/// an estimate of the file's FOLDED row count. Folded rows ≈ max(newlines,
/// bytes/wrap_cols): the newline count alone misses minified single-line
/// files (one \n, megabytes of content — the wrap fold, not the newline,
/// makes their rows). The ESTIMATE only guides pages_wide proportions — the
/// actual footprint is measured from the records afterwards, so an
/// under/over-estimate never breaks the layout.
pub fn file_item_params(p: &RepoParams, byte_len: usize, newline_count: usize) -> ItemParams {
    let page_h = p.page_rows as f64 * p.line_height;
    // ≈ a full 100-col page width + inner gap; only guides the wide choice.
    let page_w = p.wrap_cols as f64 * 0.53 + p.page_gap_x;
    let rows_est = newline_count.max(byte_len / p.wrap_cols.max(1) as usize).max(1);
    let pages = rows_est.div_ceil(p.page_rows as usize);
    let wide = ((pages as f64 * page_h * p.grid_aspect as f64 / page_w)
        .sqrt()
        .ceil() as i32)
        .clamp(1, p.max_pages_wide);
    ItemParams {
        line_height: p.line_height,
        wrap_width: p.wrap_cols,
        wrap_mode: p.wrap_mode,
        cluster_mode: p.cluster_mode,
        tab_width: p.tab_width,
        has_page: true,
        page_rows: p.page_rows,
        page_cols: 0,
        scroll_rows: 0,
        pages_wide: wide,
        page_gap_x: p.page_gap_x,
        band_stride_y: page_h + p.band_gap_y,
        z_step: text::CELL_HEIGHT_WORLD as f64 * p.z_wrap_spacing,
        // depth_per_band / depth_per_col are the PAGE-PLANE depth terms, which
        // the web applies only when the page axis is 'z' (`pageDepth`,
        // CodeGrid.js:1464). This field fans a file's pages across x and y —
        // `pages_wide` columns, `band_stride_y` bands — so its page planes are
        // coplanar by construction and these stay 0. They are a different
        // feature from the wrap staircase above, not a companion to it.
        depth_per_band: 0.0,
        depth_per_col: 0.0,
        page_line_height: 0.0,
        ..Default::default()
    }
}

/// One view's layout record: a walked file's slot range in the shared
/// arena, its extents and params. A repo load makes one per file; a
/// `--render-file --copies N` load N over its one file (2026-10-11), each
/// naming the file it reads in `buffer`. Picking reads the slot-range
/// fields (plus the engine params, which make a deterministic re-layout
/// possible). The engine-record base and row count were part of the
/// original MegaGlyphField-style contract but no reader ever landed —
/// dropped in the Stage J sweep (re-derivable from `item`).
pub struct FileView {
    pub rel_path: String,
    /// Parent directory (relative) — the group-tint key.
    pub dir: String,
    /// The walked file the view reads (its index in the walk): the view
    /// registry's `buffer`.
    pub buffer: u32,
    /// The group row the file draws with (`group_rows_of_views`).
    pub group_id: u32,
    /// The file's length in bytes.
    pub byte_len: usize,
    pub record_count: usize,
    /// Render-arena range (instances; blanks/missing are dropped from it).
    pub slot_base: usize,
    pub slot_count: usize,
    pub ink: crate::layout::InkExtent,
    /// Exact ink size in world units (derived from `ink.max - ink.min`).
    pub width: f32,
    pub height: f32,
    /// Depth extent of the ink.
    pub z_min: f32,
    pub z_max: f32,
    /// Group offset assigned by the grid layout.
    pub offset: [f32; 3],
    /// Stage G: the exact engine params this file was laid out with. A pick
    /// re-runs the engine with THESE params, so the re-derived geometry is
    /// bit-identical to what was staged.
    pub item: ItemParams,
}

pub struct LoadStats {
    pub walk: Duration,
    /// Time inside the layout seam. Since it landed this INCLUDES compaction and
    /// the extent reductions — they moved behind the seam, which is the whole
    /// point — so it is no longer comparable to the pre-seam "engine" number.
    pub backend: Duration,
    /// `backend`, split into the fold, the readback and compaction. The three
    /// have three different fixes, and item 3 of the plan is a decision
    /// between two of them, so the sum alone cannot answer it.
    pub phases: BackendPhases,
    pub stage: Duration,
    pub layout: Duration,
    pub files: usize,
    pub bytes: usize,
    pub records: usize,
    pub instances: usize,
    pub blanks: usize,
    pub skipped_large: usize,
    pub skipped_non_utf8: usize,
    pub dirs_visited: usize,
    pub strategy: Strategy,
    pub verified: bool,
}

pub struct RepoLoad {
    pub arena: GlyphArena,
    pub groups: Vec<GroupRow>,
    pub files: Vec<FileView>,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub stats: LoadStats,
    /// Whether the load painted every glyph one flat colour — every load
    /// since the syntax heuristic's retirement (2026-10-11). A flat load's
    /// far-LOD backdrop takes its file's extension tint; a load that painted
    /// per-glyph colours (spans, at load) would fold them from its slots.
    pub painted_flat: bool,
    /// Stage G: repo root — the pick path re-reads/re-runs individual files
    /// from it.
    pub root: PathBuf,
    /// Hierarchical layout controller and spatial scene graph.
    pub controller: Option<crate::layout_stack::LayoutController>,
}

mod shelf;
pub use shelf::{dir_tints, extension_tint};
pub(crate) use shelf::{dir_tint, layout_shelf};

/// Whole-repo load: walk → paint → the layout seam → grid layout.
///
/// `strategy` selects how `HyperLayout` runs (`Hyper` alone gets the
/// background prefetch) and whether a verify
/// run records. `verify` lays the same items out again with a recording
/// `HyperLayout` reference and diffs the two AT THE SEAM: placements,
/// instances and, when both can produce them, records, all bit-exact.
pub fn load_repo(
    root: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
) -> RepoLoad {
    load_repo_from_walk(
        root,
        walk_repo(root),
        params,
        strategy,
        verify,
        None,
        GlyphArena::new(),
        None,
    )
}

/// The loader proper — everything past the walk. Split so the SAME pipeline
/// serves disk walks (`load_repo`) and caller-owned content
/// (`WalkResult::from_files`, the P1-live envelope path): the fold is a pure
/// function of (bytes, params) either way, and only the bytes' provenance
/// differs.
pub fn load_items(
    walk: WalkResult,
    _walk_dur: Duration,
    root: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    load_repo_from_walk(
        root,
        walk,
        params,
        strategy,
        verify,
        None,
        GlyphArena::new(),
        folds,
    )
}

/// Precomputed repository walk and optional GPU chain tables.
pub struct PrefetchedRepo {
    pub walk: WalkResult,
    pub file_params: Vec<ItemParams>,
    pub(crate) hyper_data: Option<crate::layout_hyper::PrefetchedHyperData>,
}

/// Run repository filesystem walking, file_params calculation and (for
/// `hyper`) the Pass 1 prefetch on a background thread.
pub fn prefetch_repo(
    dir: &Path,
    params: RepoParams,
    strategy: Strategy,
) -> PrefetchedRepo {
    let walk = walk_repo(dir);
    let file_params: Vec<ItemParams> = walk
        .files
        .iter()
        .map(|f| file_item_params(&params, f.bytes.len(), f.newline_count))
        .collect();

    let hyper_data = if strategy == Strategy::Hyper {
        Some(crate::layout_hyper::prefetch_hyper(&walk.files, &file_params, params.field_mode))
    } else {
        None
    };

    PrefetchedRepo {
        walk,
        file_params,
        hyper_data,
    }
}

/// `load_repo` with the walk and the arena already in hand: the caller walks
/// first so it can size the arena to the byte count (the render path's
/// device-mapped arena exists because of this split — leaders ≤ bytes, so
/// `walk.total_bytes` is the slot bound the direct path commits against).
/// `gpu` is the renderer's device when one exists — HyperLayout emits
/// straight into its buffers; `None` is the no-GPU door
/// (`--repo-scan-only`), where layout fills a host arena.
#[expect(clippy::too_many_arguments, reason = "a repo-load entry point: each parameter is a separate input its callers choose (walk, device, arena, folds)")]
pub fn load_repo_from_walk(
    root: &Path,
    walk: WalkResult,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    gpu: Option<&crate::gpu::GpuContext>,
    arena: GlyphArena,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    let file_params: Vec<ItemParams> = walk
        .files
        .iter()
        .map(|f| file_item_params(params, f.bytes.len(), f.newline_count))
        .collect();

    let prefetched = PrefetchedRepo {
        walk,
        file_params,
        hyper_data: None,
    };

    load_repo_from_prefetched(
        root,
        prefetched,
        params,
        strategy,
        verify,
        gpu,
        arena,
        folds,
    )
}

#[expect(clippy::too_many_arguments, reason = "a repo-load entry point: each parameter is a separate input its callers choose (walk, device, arena, folds)")]
pub fn load_repo_from_prefetched(
    root: &Path,
    prefetched: PrefetchedRepo,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    gpu: Option<&crate::gpu::GpuContext>,
    arena: GlyphArena,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    let plan = ViewPlan {
        buffer_of_view: (0..prefetched.walk.files.len() as u32).collect(),
        placement: Placement::Controller,
    };
    load_views(root, prefetched, plan, params, strategy, verify, gpu, arena, folds)
}

/// `--render-file FILE [--copies N]` as a repo load of that one file
/// (2026-10-11; step 2 of the Instanced retirement): one buffer, `copies`
/// views over it, laid out, painted and fielded by the same HyperLayout,
/// colourisers and fields as `--load-repo` — so the Visible field serves it
/// for real, and the glyph verbs and picks work on it. The views are tiled
/// (`tile_views`) rather than shelved, each its own group row so the
/// copies sit side by side. It paints flat like every load (the syntax
/// heuristic it painted until then was retired the same day).
///
/// The bytes are refused where a walk would skip them: not UTF-8 (the
/// decode assumes well-formed leads) or past [`MAX_FILE_BYTES`] (the
/// per-item ordinal guard).
pub fn load_file(path: &Path, copies: u32, params: &RepoParams, gpu: Option<&crate::gpu::GpuContext>) -> RepoLoad {
    let started = Instant::now();
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("failed to read text file {}: {e}", path.display()));
    assert!(std::str::from_utf8(&bytes).is_ok(), "{} is not valid UTF-8; the layout decodes well-formed UTF-8 only", path.display());
    assert!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "{} is {} B, past the {} B per-item cap (the ordinal guard; a repo walk skips such a file)",
        path.display(),
        bytes.len(),
        MAX_FILE_BYTES
    );
    let file_name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    // The pick path re-reads `root/rel_path` from disk, so the root is the
    // file's own directory and the file its one walked entry.
    let root = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut walk = WalkResult::from_files(vec![RepoFile::in_memory(file_name, bytes)]);
    walk.walk_dur = started.elapsed();
    let file_params = walk.files.iter().map(|f| file_item_params(params, f.bytes.len(), f.newline_count)).collect();
    let plan = ViewPlan { buffer_of_view: vec![0; copies.max(1) as usize], placement: Placement::Tiled };
    load_views(
        root,
        PrefetchedRepo { walk, file_params, hyper_data: None },
        plan,
        params,
        Strategy::default(),
        false,
        gpu,
        GlyphArena::new(),
        None,
    )
}

/// Which walked file each view reads, and how the views are placed
/// (2026-10-11). A repo load is one view per file, placed by the layout
/// controller (shelf, carrel or library); `--render-file FILE --copies N`
/// is N views over its one file, tiled — the first producer with several
/// views over one buffer (`crate::views`).
struct ViewPlan {
    /// View `v` reads walked file `buffer_of_view[v]`.
    buffer_of_view: Vec<u32>,
    placement: Placement,
}

enum Placement {
    /// `LayoutController::from_mode(params.layout_mode)`: a repo load.
    Controller,
    /// [`tile_views`]: `--render-file`.
    Tiled,
}

/// The loader proper, over a [`ViewPlan`].
#[expect(clippy::too_many_arguments, reason = "the loader behind the repo-load entry points: the walk, the plan and each input its callers choose (device, arena, folds), passed apart")]
fn load_views(
    root: &Path,
    prefetched: PrefetchedRepo,
    plan: ViewPlan,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    gpu: Option<&crate::gpu::GpuContext>,
    mut arena: GlyphArena,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    let PrefetchedRepo {
        walk,
        file_params,
        hyper_data,
    } = prefetched;
    let view_count = plan.buffer_of_view.len();

    let _load = tracing::info_span!(
        "repo.load",
        files = walk.files.len(),
        bytes = walk.total_bytes,
        ?strategy,
        verify,
    )
    .entered();
    let walk_dur = walk.walk_dur;

    let mut stage_dur = std::time::Duration::ZERO;
    // Every load paints flat (2026-10-11): the lexical syntax heuristic is
    // retired, and colour arrives afterwards as byte spans (`--highlight`,
    // the seam's style runs), never guessed at load.
    let paint_mode = Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED);

    // Each view's group row: its own index, unless the permutation witness
    // asks otherwise (`group_rows_of_views`). The layout writes it into the
    // Instanced slots' group lane; the view registry carries it from here on.
    // One layout item per VIEW: copies of one buffer are laid out once each
    // (every stored slot names its item, so views cannot share slots).
    let group_rows = group_rows_of_views(view_count);
    let items: Vec<LayoutItem<'_>> = plan
        .buffer_of_view
        .iter()
        .enumerate()
        .map(|(view, &buffer)| LayoutItem {
            bytes: &walk.files[buffer as usize].bytes,
            params: file_params[buffer as usize],
            group_id: group_rows.of_view[view],
            paint: paint_mode,
        })
        .collect();

    // Every strategy runs HyperLayout; with a GPU it emits straight into
    // the field's own slot format.
    let mut backend = match gpu {
        Some(ctx) => crate::layout::LayoutEngine::hyper_with_device(
            crate::gpu::SharedDevice::from_ctx(ctx),
            params.field_mode,
        ),
        // Without a device the mode still matters for Visible (no slot is
        // ever emitted, so it needs none); the other modes run as before.
        None => crate::layout::LayoutEngine::hyper_with_mode(params.field_mode),
    };
    if let Some(hd) = hyper_data {
        // The prefetch ran Pass 1 over the walked FILES; it stands for the
        // items only when each file is one view.
        assert!(
            plan.buffer_of_view.iter().enumerate().all(|(view, &buffer)| buffer as usize == view)
                && view_count == walk.files.len(),
            "a Pass 1 prefetch over the files serves one view per file only"
        );
        backend.set_hyper_prefetched(hd);
    }
    let t = Instant::now();
    let sp_backend = tracing::info_span!("repo.backend").entered();
    let (mut placements, records) = if verify && strategy.can_record() {
        backend
            .layout_items_recording(&items, &mut arena)
            .expect("layout failed")
    } else {
        (
            backend
                .layout_items(&items, &mut arena)
                .expect("layout failed"),
            Vec::new(),
        )
    };
    drop(sp_backend);
    let mut backend_dur = t.elapsed();

    let mut verified = false;
    if verify {
        let _sp_verify = tracing::info_span!("repo.verify").entered();
        let t = Instant::now();
        let mut alt = crate::layout_hyper::HyperLayout::new();
        let mut alt_arena = GlyphArena::new();
        let (alt_placements, alt_records) = alt
            .layout_validated_items_recording(&items, &mut alt_arena)
            .expect("layout failed");
        let arena_flat = arena.instances_cow();
        let alt_flat = alt_arena.instances_cow();
        let report = diff_backends(
            &BackendOutput {
                name: backend.name(),
                placements: &placements,
                instances: &arena_flat,
                records: &records,
            },
            &BackendOutput {
                name: "hyper-ref",
                placements: &alt_placements,
                instances: &alt_flat,
                records: &alt_records,
            },
        )
        .unwrap_or_else(|why| panic!("repo-verify FAIL: {why}"));
        if report.items == 0 || report.instances == 0 {
            panic!(
                "repo-verify FAIL: nothing to compare — {} items, {} instances.",
                report.items, report.instances,
            );
        }
        backend_dur += t.elapsed();
        verified = true;
        println!(
            "repo-verify PASS: {} items, {} instances, {} records verified",
            report.items,
            report.instances,
            report.records,
        );
    }

    let t_fold_phase = Instant::now();
    // ── P2a: fold compaction ────────────────────────────────────────────
    // Folded files' slices are rebuilt from their compacted record streams
    // (records re-derived on the cached engine — the fold is a pure function
    // of bytes, so this re-run is bit-identical to what staged the arena);
    // the whole arena is then re-spliced with running slot bases so every
    // item's slice stays contiguous. Unfolded files' slices pass through
    // untouched. Runs ONLY when at least one file carries folds — the
    // golden paths never enter this block.
    if folds.is_some_and(|f| f.values().any(|v| !v.is_empty())) {
        let folds = folds.unwrap();
        let mut old = arena.into_instances();
        let mut rebuilt: Vec<GlyphInstance> = Vec::with_capacity(old.len());
        let mut running: u32 = 0;
        let mut dropped_total = 0usize;
        for (view, &buffer) in plan.buffer_of_view.iter().enumerate() {
            let buffer = buffer as usize;
            let f = &walk.files[buffer];
            let placement = &mut placements[view];
            let base = placement.slot_base as usize;
            let count = placement.slot_count as usize;
            let fold_lines = folds.get(&f.rel_path).filter(|v| !v.is_empty());
            // THE PAGINATION BOUNDARY (found live, 2026-09-26): this repo's
            // layout fans long files into side-by-side PAGES (page_rows 128,
            // bands stacked) — y is not a row ladder across pages, so the
            // renderer-side empirical shift manufactures overlap on
            // paginated files. Renderer compaction is sound ONLY on
            // single-page files; a paginated file keeps its folds SKIPPED
            // and a loud log, until folds become ENGINE-level layout input
            // (queued: the durable design — pagination, wrapping, everything
            // recomputes when the fold feeds the layout).
            let paginated = {
                let item = &file_params[buffer];
                let newlines = f.newline_count;
                let rows_est = newlines
                    .max(f.bytes.len() / item.wrap_width.max(1) as usize)
                    .max(1);
                item.page_rows > 0 && rows_est > item.page_rows as usize
            };
            let fold_lines = if paginated { None } else { fold_lines };
            if paginated && folds.contains_key(&f.rel_path) {
                println!(
                    "fold: {} SKIPPED — paginated file (renderer compaction is \
                     single-page only; engine-level fold input is queued)",
                    f.rel_path
                );
            }
            let slice = match fold_lines {
                None => {
                    let slice = old[base..base + count].to_vec();
                    placement.slot_base = running;
                    running += placement.slot_count;
                    slice
                }
                Some(fold_lines) => {
                    let item = &file_params[buffer];
                    let records =
                        rederive_cached(&f.bytes, item).expect("fold: re-derive failed");
                    let (leaders, _, _, _) =
                        crate::text::fold_leaders(&f.bytes, item.wrap_width, item.wrap_mode);
                    let starts = line_starts_of(&f.bytes);
                    let lines: Vec<u32> =
                        leaders.iter().map(|l| line_of_byte(l.0, &starts)).collect();
                    let folded = compact_folds(&records, fold_lines, &lines);
                    let mut slice = Vec::with_capacity(folded.records.len());
                    for r in &folded.records {
                        if r.glyph_id() == 0 {
                            continue; // blank: no slot, exactly as staging skips them
                        }
                        slice.push(GlyphInstance {
                            pos: [r.x(), r.y(), r.z()],
                            glyph_id: r.glyph_id(),
                            row: r.row(),
                            col: r.col(),
                            color: crate::layout::DEFAULT_COLOR_PACKED,
                            group_id: group_rows.of_view[view],
                            advance: r.advance(),
                            height: r.height(),
                            flags: 0,
                            _pad: 0,
                        });
                    }
                    dropped_total += folded.dropped;
                    placement.slot_base = running;
                    placement.slot_count = slice.len() as u32;
                    placement.record_count = folded.records.len() as u32;
                    placement.page = folded.page;
                    placement.ink = folded.ink;
                    running += placement.slot_count;
                    println!(
                        "fold: {} — {} line range(s), {} records dropped, {} slots remain",
                        f.rel_path,
                        fold_lines.len(),
                        folded.dropped,
                        placement.slot_count
                    );
                    slice
                }
            };
            rebuilt.extend(slice);
        }
        old = rebuilt;
        arena = GlyphArena::new();
        arena.reserve(old.len());
        for inst in old {
            arena.push(inst);
        }
        println!(
            "fold: {} record(s) dropped across the field; arena re-spliced to {} slots in {:?}",
            dropped_total,
            arena.len(),
            t_fold_phase.elapsed()
        );
    }

    let t = Instant::now();
    let sp_views = tracing::info_span!("repo.views").entered();
    let mut total_records = 0usize;
    let mut total_blanks = 0usize;
    let mut file_views: Vec<FileView> = Vec::with_capacity(view_count);
    for (view, &buffer) in plan.buffer_of_view.iter().enumerate() {
        let f = &walk.files[buffer as usize];
        let placed = &placements[view];
        total_records += placed.record_count as usize;
        total_blanks += (placed.record_count - placed.slot_count) as usize;
        let mut ink = placed.ink;
        if placed.slot_count == 0 || ink.min[0] > ink.max[0] {
            ink = crate::layout::InkExtent { min: [0.0; 3], max: [0.0; 3] };
        }
        let width = (ink.max[0] - ink.min[0]).max(0.0);
        let height = (ink.max[1] - ink.min[1]).max(0.0);

        file_views.push(FileView {
            rel_path: f.rel_path.clone(),
            dir: f.dir.clone(),
            buffer,
            group_id: group_rows.of_view[view],
            byte_len: f.bytes.len(),
            record_count: placed.record_count as usize,
            slot_base: placed.slot_base as usize,
            slot_count: placed.slot_count as usize,
            ink,
            width,
            height,
            z_min: ink.min[2],
            z_max: ink.max[2],
            offset: [0.0; 3],
            item: file_params[buffer as usize],
        });
    }
    let instances_len = arena.len();
    drop(sp_views);
    stage_dur += t.elapsed();

    // The visible form keeps the bytes: the walk's own Vecs move into the
    // staging — one allocation per file, no copy of the corpus, however many
    // views read it. Everything the walk still had to say (counts, times, a
    // tiled block's last row) is read before the move.
    let files_len = walk.files.len();
    let (walk_total_bytes, walk_skipped_large, walk_skipped_non_utf8, walk_dirs_visited) =
        (walk.total_bytes, walk.skipped_large, walk.skipped_non_utf8, walk.dirs_visited);
    let ends_in_newline: Vec<bool> =
        plan.buffer_of_view.iter().map(|&buffer| walk.files[buffer as usize].bytes.last() == Some(&b'\n')).collect();
    if let Some(staging) = arena.visible_staging_mut() {
        assert_eq!(staging.seeds.len(), view_count, "one visible seed per view");
        staging.buffer_bytes = walk.files.into_iter().map(|f| f.bytes).collect();
        staging.item_buffers = plan.buffer_of_view.clone();
    }

    let t = Instant::now();
    let sp_grid = tracing::info_span!("repo.layout").entered();
    let (controller, rows_in_view_order, bounds_min, bounds_max) = match plan.placement {
        Placement::Controller => {
            let mut controller = crate::layout_stack::LayoutController::from_mode(params.layout_mode, *params);
            let (rows, bounds_min, bounds_max) = controller.apply(&mut file_views);
            (Some(controller), rows, bounds_min, bounds_max)
        }
        Placement::Tiled => {
            let pages: Vec<crate::layout::PageExtent> = placements.iter().map(|p| p.page).collect();
            let (rows, bounds_min, bounds_max) = tile_views(&mut file_views, &pages, &ends_in_newline, params.line_height as f32);
            (None, rows, bounds_min, bounds_max)
        }
    };
    let groups = group_rows.table(rows_in_view_order);
    drop(sp_grid);
    let layout_dur = t.elapsed();

    let stats = LoadStats {
        walk: walk_dur,
        backend: backend_dur,
        phases: backend.phases(),
        stage: stage_dur,
        layout: layout_dur,
        files: files_len,
        bytes: walk_total_bytes,
        records: total_records,
        instances: instances_len,
        blanks: total_blanks,
        skipped_large: walk_skipped_large,
        skipped_non_utf8: walk_skipped_non_utf8,
        dirs_visited: walk_dirs_visited,
        strategy,
        verified,
    };
    RepoLoad {
        arena,
        groups,
        files: file_views,
        bounds_min,
        bounds_max,
        stats,
        painted_flat: matches!(paint_mode, Paint::Flat(_)),
        root: root.to_path_buf(),
        controller,
    }
}

mod rederive;
pub use rederive::{compact_folds, rederive_cached, Folded};

mod cull_blocks;
pub use cull_blocks::{line_of_byte, line_starts_of};
use cull_blocks::build_file_blocks;

impl RepoLoad {
    /// Convert into the renderer's staged form. `focus` selects one file
    /// (first rel-path containing the substring) for the camera to frame.
    pub fn into_staged(self, focus: Option<&str>, slot_ink: &[Option<[f32; 4]>]) -> StagedText {
        let _sp = tracing::info_span!("repo.staged", files = self.files.len()).entered();
        log::info!(
            "field bounds: x [0, {:.0}], y [{:.0}, {:.1}] — {:.0}x{:.0} world units",
            self.bounds_max[0],
            self.bounds_min[1],
            self.bounds_max[1],
            self.bounds_max[0] - self.bounds_min[0],
            self.bounds_max[1] - self.bounds_min[1],
        );
        // One view per file (`crate::views`), view i being file i — the
        // item row of every per-item table here (the layout's placements,
        // tints and blocks, the item table, the Visible items, the pick
        // records) — and drawn with the file's group row, `v.group_id`.
        // Stage F: one cull segment per file — world AABB (group offset
        // applied, small margins for glyph overhang) + arena slot range +
        // the far-LOD backdrop tint derived from the file's own ink.
        // Stage G: the SAME local AABB goes into the pick table (pre-TRS; the
        // pick path applies the live group TRS itself).
        //
        // The tint fold reads the endpoint's tint STREAM on the Device
        // path (note 23, E2b — no host instances exist there) and the
        // arena chunks on the 48 B paths; same values in the same slot
        // order either way, so the tints are bit-identical.
        let mapped_slots = self
            .arena
            .device_slots()
            .and_then(|d| d.mapped_slots)
            .map(|addr| {
                let len = self.arena.device_slots().unwrap().len;
                // SAFETY: pointer was mapped by create_mapped_render_slots and outlives arena
                unsafe { std::slice::from_raw_parts(addr as *const crate::glyph_scene::RenderSlot, len) }
            });
        let tint_stream: Option<&[u32]> = None;
        let chunks = if self.arena.device_slots().is_none() {
            self.arena.instance_chunks()
        } else {
            Vec::new()
        };
        // The visible form has no slots to fold a tint from: its backdrop
        // takes the flat form's extension tint, and no sub-file blocks — the
        // field culls its own lines.
        let visible = self.arena.is_visible();
        let is_flat = self.painted_flat || visible;
        let file_tints = self.arena.device_slots().map(|d| &d.file_tints);
        let file_blocks = self.arena.device_slots().map(|d| &d.file_blocks);
        let emoji_tint_pairs = self.arena.device_slots().map(|d| &d.emoji_tint_pairs);
        // The file's group scale (1 in every mode but the library, whose fit
        // rides in the group). Applied as `offset + local * s`, so at s = 1
        // every expression below is the unscaled one bit for bit.
        let groups = &self.groups;
        let scale_of = |v: &FileView| -> [f32; 3] {
            groups
                .get(v.group_id as usize)
                .map_or([1.0; 3], |g| [g.cols[3][0], g.cols[3][1], g.cols[3][2]])
        };
        let seg_of = |view: usize, v: &FileView| {
            let s = scale_of(v);
            let tint = if is_flat {
                let area = (v.width as f64 * v.height as f64).max(1e-3);
                let ink_frac = (v.slot_count as f64 * crate::glyph_scene::GLYPH_CELL_AREA as f64 / area).min(1.0);
                let e = (ink_frac * crate::config::settings().lod.backdrop_gain as f64).min(1.0);
                let rgb = extension_tint(&v.rel_path);
                [rgb[0], rgb[1], rgb[2], e as f32]
            } else {
                let fast_tint = file_tints.and_then(|t| t.get(view));
                match fast_tint {
                    Some(acc) if !acc.has_emoji => {
                        let accum = crate::glyph_scene::SegTintAccum::from_parts(acc.sum, acc.cells, slot_ink);
                        accum.finish(v.slot_count, v.width, v.height)
                    }
                    _ => {
                        // The file's slot range folded in arena order: chunk slices
                        // ascend and concatenate exactly, so a range that straddles a
                        // chunk boundary tints bit-identically to the contiguous fold.
                        let mut tint = crate::glyph_scene::SegTintAccum::new(slot_ink);
                        let want = v.slot_base..v.slot_base + v.slot_count;
                        // Derived device emission: no 32 B slots exist to read
                        // back, so Pass 2 captured this item's (glyph, color)
                        // pairs — its whole slot range, in slot order.
                        let emitted_pairs = emoji_tint_pairs
                            .and_then(|e| e.get(view))
                            .filter(|p| !p.is_empty());
                        if let Some(pairs) = emitted_pairs {
                            debug_assert_eq!(pairs.len(), v.slot_count * 2);
                            tint.add_tint(pairs);
                        } else if let Some(slots) = mapped_slots {
                            tint.add_slots(&slots[want]);
                        } else if let Some(tp) = tint_stream {
                            tint.add_tint(&tp[want.start * 2..want.end * 2]);
                        } else {
                            let mut base = 0usize;
                            for chunk in &chunks {
                                let lo = want.start.max(base);
                                let hi = want.end.min(base + chunk.len());
                                if lo < hi {
                                    tint.add(&chunk[lo - base..hi - base]);
                                }
                                base += chunk.len();
                            }
                        }
                        tint.finish(v.slot_count, v.width, v.height)
                    }
                }
            };
            let fast_blocks = file_blocks.and_then(|fb| fb.get(view));
            let blocks = if visible {
                Vec::new()
            } else if let Some(fbs) = fast_blocks {
                fbs.iter()
                    .map(|lb| crate::glyph_scene::BlockCull {
                        min: [
                            v.offset[0] + lb.min[0] * s[0] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[0] * s[0],
                            v.offset[1] + lb.min[1] * s[1] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[1] * s[1],
                            v.offset[2] + lb.min[2] * s[2] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[2] * s[2],
                        ],
                        max: [
                            v.offset[0] + lb.max[0] * s[0] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[0] * s[0],
                            v.offset[1] + lb.max[1] * s[1] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[1] * s[1],
                            v.offset[2] + lb.max[2] * s[2] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[2] * s[2],
                        ],
                        slot_base: (v.slot_base as u32) + lb.slot_base,
                        slot_count: lb.slot_count,
                    })
                    .collect()
            } else {
                build_file_blocks(v, s, mapped_slots, &chunks)
            };
            crate::glyph_scene::SegCull {
                min: [
                    v.offset[0] + v.ink.min[0] * s[0] - crate::glyph_scene::SEG_CULL_PAD_MIN[0] * s[0],
                    v.offset[1] + v.ink.min[1] * s[1] - crate::glyph_scene::SEG_CULL_PAD_MIN[1] * s[1],
                    v.offset[2] + v.ink.min[2] * s[2],
                ],
                max: [
                    v.offset[0] + v.ink.max[0] * s[0] + crate::glyph_scene::SEG_CULL_PAD_MAX[0] * s[0],
                    v.offset[1] + v.ink.max[1] * s[1] + crate::glyph_scene::SEG_CULL_PAD_MAX[1] * s[1],
                    v.offset[2] + v.ink.max[2] * s[2],
                ],
                slot_base: v.slot_base as u32,
                slot_count: v.slot_count as u32,
                tint,
                blocks,
            }
        };
        // seg_tint re-reads the whole arena, one file's slice at a time —
        // ~500 ms serial on the glyph3d-js repo (1,306 files). Shard by file
        // range: per-segment sums are independent and the per-worker results
        // concatenate in file order, so the table is bit-identical. Small
        // repos stay serial (thread spawn would cost more than the pass).
        let sp_segments = tracing::info_span!("repo.segments").entered();
        let segments: Vec<crate::glyph_scene::SegCull> = {
            use rayon::prelude::*;
            self.files.par_iter().enumerate().map(|(view, v)| seg_of(view, v)).collect()
        };
        drop(sp_segments);
        let pick_files: Vec<crate::glyph_scene::PickFileInfo> = self
            .files
            .iter()
            .map(|v| crate::glyph_scene::PickFileInfo {
                rel_path: v.rel_path.clone(),
                item: v.item,
                aabb_min: [
                    v.ink.min[0] - crate::glyph_scene::SEG_CULL_PAD_MIN[0],
                    v.ink.min[1] - crate::glyph_scene::SEG_CULL_PAD_MIN[1],
                    v.ink.min[2] - crate::glyph_scene::PICK_AABB_PAD_Z,
                ],
                aabb_max: [
                    v.ink.max[0] + crate::glyph_scene::SEG_CULL_PAD_MAX[0],
                    v.ink.max[1] + crate::glyph_scene::SEG_CULL_PAD_MAX[1],
                    v.ink.max[2] + crate::glyph_scene::PICK_AABB_PAD_Z,
                ],
            })
            .collect();
        let mut focus_bounds = None;
        if let Some(needle) = focus {
            if let Some(v) = self.files.iter().find(|v| v.rel_path.contains(needle)) {
                let s = scale_of(v);
                let cx = v.offset[0] + (v.ink.min[0] + v.ink.max[0]) * 0.5 * s[0];
                let cy = v.offset[1] + (v.ink.min[1] + v.ink.max[1]) * 0.5 * s[1];
                let hw = ((v.ink.max[0] - v.ink.min[0]) * s[0]).max(1.0) * 0.5;
                let hh = ((v.ink.max[1] - v.ink.min[1]) * s[1]).max(1.0) * 0.5;
                focus_bounds = Some((
                    [cx, cy],
                    [hw, hh],
                ));
                log::info!(
                    "focus: {} ({}x{} records, page {:.3}x{:.3} world units)",
                    v.rel_path,
                    v.record_count,
                    v.slot_count,
                    v.width,
                    v.height,
                );
            } else {
                log::warn!("focus: no file path contains {needle:?} — fitting the whole field");
            }
        }
        let views = crate::views::Views::new(
            self.files
                .iter()
                .enumerate()
                .map(|(view, v)| {
                    let (local_min, local_max) = segment_local_box(v);
                    crate::views::View {
                        buffer: v.buffer,
                        bytes: 0..v.byte_len as u32,
                        recipe: glyph_field::ItemParamsGpu::from(&v.item),
                        group_row: v.group_id,
                        segment: view as u32,
                        slots: v.slot_base as u32..(v.slot_base + v.slot_count) as u32,
                        local_min,
                        local_max,
                    }
                })
                .collect(),
        );
        // The visible field's items: the seam's seeds finished with what only
        // the layout knows — the group row, in the item table's group lane —
        // and the LOCAL box the field's item cull carries through that row
        // every frame: the segment's box before the group's T·R·S (`offset +
        // local * s` is the segment). Local since 2026-10-10, so no group
        // edit re-uploads it.
        let mut arena = self.arena;
        if let Some(staging) = arena.visible_staging_mut() {
            assert_eq!(staging.item_buffers.len(), views.len(), "the loader named every view's buffer");
            let item_table = views.item_table();
            let items: Vec<glyph_field_visible::VisibleItem> = (0..views.len())
                .map(|view| {
                    let (min, max) = views.local_extent(view as u32);
                    staging.item(view, item_table[view], min, max)
                })
                .collect();
            staging.items = items;
        }
        StagedText {
            glyphs_emitted: arena.len(),
            codepoints_decoded: self.stats.records,
            missing_or_bitmap: self.stats.blanks,
            segments,
            instances: arena,
            groups: self.groups,
            bounds_min: self.bounds_min,
            bounds_max: self.bounds_max,
            focus_bounds,
            pick: Some(crate::glyph_scene::PickContext {
                root: self.root,
                files: pick_files,
                // Envelope-owned content is the CALLER's to inject (it owns
                // the bytes); disk scenes re-derive from root. Same for the
                // fold set (P2a) — the loader's compacted field is the
                // caller's to describe back.
                content: None,
                folds: std::collections::HashMap::new(),
            }),
            controller: self.controller,
            views,
        }
    }

    pub fn print_stats(&self) {
        let s = &self.stats;
        let mb = s.bytes as f64 / 1e6;
        println!(
            "repo: {} files, {:.1} MB source ({} dirs walked, {} skipped >{} MiB, {} skipped non-UTF-8)",
            s.files,
            mb,
            s.dirs_visited,
            s.skipped_large,
            MAX_FILE_BYTES >> 20,
            s.skipped_non_utf8,
        );
        println!(
            "repo: {} engine records -> {} glyph instances ({} blank/missing dropped) \
             | backend: {}{}",
            s.records,
            s.instances,
            s.blanks,
            match s.strategy {
                Strategy::Batched => "hyper/batched",
                Strategy::PerItem => "hyper/per-item",
                Strategy::Direct => "hyper/direct",
                Strategy::Hyper => "hyper-rust (parallel direct)",
            },
            if s.verified { " (verified bit-exact vs the other strategy)" } else { "" },
        );
        println!(
            "phases: walk {:.3}s | backend {:.3}s ({:.1} MB/s, {}) \
             | stage {:.3}s | layout {:.3}s | total {:.3}s",
            s.walk.as_secs_f64(),
            s.backend.as_secs_f64(),
            mb / s.backend.as_secs_f64().max(1e-9),
            // Naming what ran, not what usually runs: there is no host
            // compaction on the direct path, and a fixed parenthetical is the
            // same "a name that is not what happened" defect the engine line
            // below was split to fix.
            if s.strategy.materializes_records() {
                "compaction included"
            } else {
                "instances written in place"
            },
            s.stage.as_secs_f64(),
            s.layout.as_secs_f64(),
            (s.walk + s.backend + s.stage + s.layout).as_secs_f64(),
        );
        // The split the plan's item 3 is a decision about. `unattributed` is
        // the part of `backend` these four timers did not claim; it should be
        // near zero, and if it is not, one of them is measuring the wrong span.
        //
        // A STAGE THAT DOES NOT EXIST PRINTS `n/a`, NOT `0.000s`. On the direct
        // path there is no readback and no host compaction at all, and a zero
        // there reads exactly like a stage that ran and was fast — the same
        // ambiguity that made `fold` look like the fold for a day. Only the
        // person who built the path knows which zero is which, and they are not
        // the person who reads this next.
        let p = s.phases;
        let attributed = p.fold + p.readback() + p.compact;
        let absent = !s.strategy.materializes_records();
        let secs = |d: Duration| {
            if absent { "n/a".to_string() } else { format!("{:.3}s", d.as_secs_f64()) }
        };
        println!(
            "  backend: fold {:.3}s | readback {} (alloc {} + copy {}, {}) \
             | compact {} | unattributed {:.3}s",
            p.fold.as_secs_f64(),
            secs(p.readback()),
            secs(p.readback_alloc),
            secs(p.readback_copy),
            if absent {
                "no wire record on this path".to_string()
            } else {
                format!("{:.2} GB", (s.records * 32) as f64 / 1.073_741_824e9)
            },
            secs(p.compact),
            s.backend.saturating_sub(attributed).as_secs_f64(),
        );
        // `fold` above is the whole backend call. This is what the backend
        // says it spent inside it — largest lane first, and `unattributed`
        // here catches the part of the call no engine lane claims.
        //
        // Zero-valued engine lanes are ELIDED rather than printed as 0.000s,
        // for the same reason: a lane absent from this line did not run on
        // this path, so which lanes appear is itself the statement of which
        // route the load took. (No backend reports lanes today —
        // `engine_ranked` is empty — so the line carries only
        // `unattributed`.)
        let ranked = p.engine_ranked();
        let eng_sum: Duration = ranked.iter().map(|(_, d)| *d).sum();
        print!("  engine:");
        for (name, d) in ranked.iter().filter(|(_, d)| !d.is_zero()) {
            print!(" {} {:.3}s", name, d.as_secs_f64());
        }
        println!(
            " | unattributed {:.3}s",
            p.fold.saturating_sub(eng_sum).as_secs_f64()
        );
    }
}

/// Which group row each view draws with. A load files view `i` under row
/// `i`; `GLYPH_PERMUTE_GROUP_ROWS=1` is the step-A witness of the view
/// registry (2026-10-11, `crate::views`): it files them in REVERSE under
/// rows `1..=n` and leaves row 0 a spare identity row, so no view's group
/// row equals its index and the table has a row no view draws with. Every
/// reader that still assumed the two equal draws the wrong file there
/// (`native/tests/view_registry.rs` renders both and compares the pixels).
struct GroupRows {
    of_view: Vec<u32>,
    count: usize,
}

fn group_rows_of_views(view_count: usize) -> GroupRows {
    if std::env::var_os("GLYPH_PERMUTE_GROUP_ROWS").is_some_and(|v| v == "1") {
        log::warn!("GLYPH_PERMUTE_GROUP_ROWS=1: {view_count} views filed under group rows in reverse, row 0 spare");
        GroupRows { of_view: (0..view_count).map(|i| (view_count - i) as u32).collect(), count: view_count + 1 }
    } else {
        GroupRows { of_view: (0..view_count as u32).collect(), count: view_count }
    }
}

/// `--render-file`'s placement (2026-10-11): the views — copies of one
/// file — in a near-square grid, row-major, `ceil(sqrt(n))` columns, with a
/// gap of four primary-font advances, each drawn with an IDENTITY group row
/// at its grid offset (no directory tint: a text view is no repo's file).
/// The rule, the gap and the bounds are the ones the text stager used
/// before `--render-file` became a repo load, so the camera frames a file
/// as it always has.
///
/// A copy's block is its inked width from the pen origin (`ink.max.x`, the
/// width the shelf measures a file by; the page extent's `right` would add
/// the newline record's advance) by its rows down to the pen's row after a
/// final newline (an editor's empty last line): `line_height -
/// page.bottom`, plus a row when the file ends in a newline. The bounds add
/// the one row above row 0 that the stager's did (`max.y = line_height`),
/// and take depth from the pages.
fn tile_views(
    views: &mut [FileView],
    pages: &[crate::layout::PageExtent],
    ends_in_newline: &[bool],
    line_height: f32,
) -> (Vec<GroupRow>, [f32; 3], [f32; 3]) {
    assert_eq!(views.len(), pages.len(), "one page extent per view");
    let (mut block_width, mut block_height) = (0.0f32, 0.0f32);
    let (mut depth_min, mut depth_max) = (0.0f32, 0.0f32);
    for ((view, page), &newline_ended) in views.iter().zip(pages).zip(ends_in_newline) {
        block_width = block_width.max(view.ink.max[0]);
        block_height = block_height.max(line_height - page.bottom + if newline_ended { line_height } else { 0.0 });
        depth_min = depth_min.min(page.z_min);
        depth_max = depth_max.max(page.z_max);
    }
    let trie = crate::default_trie();
    let gap = 4.0 * crate::text::fu_to_world(trie.metrics.advance_fu as i32, trie.metrics.em_height_fu);
    let copies = views.len().max(1) as u32;
    let grid_columns = (copies as f32).sqrt().ceil() as u32;
    let grid_rows = copies.div_ceil(grid_columns);
    let mut rows = Vec::with_capacity(views.len());
    for (copy, view) in views.iter_mut().enumerate() {
        let column = copy as u32 % grid_columns;
        let row = copy as u32 / grid_columns;
        view.offset = [column as f32 * (block_width + gap), -(row as f32 * (block_height + gap)), 0.0];
        rows.push(GroupRow::identity(view.offset));
    }
    let total_width = grid_columns as f32 * (block_width + gap) - gap;
    let total_height = grid_rows as f32 * (block_height + gap) - gap;
    log::info!("layout [tile]: {copies} views, block {block_width:?} x {block_height:?}, field {total_width:?} x {total_height:?}");
    (rows, [0.0, -total_height, depth_min], [total_width, line_height, depth_max])
}

impl GroupRows {
    /// The group table from the layout's rows, which come one per view in
    /// view order (`LayoutController::apply`, `tile_views`): row
    /// `of_view[i]` takes view `i`'s; a row no view draws with stays the
    /// identity.
    fn table(&self, rows_in_view_order: Vec<GroupRow>) -> Vec<GroupRow> {
        assert_eq!(rows_in_view_order.len(), self.of_view.len(), "the layout returns one group row per view");
        let mut table = vec![GroupRow::identity([0.0; 3]); self.count];
        for (row, &group_row) in rows_in_view_order.into_iter().zip(&self.of_view) {
            table[group_row as usize] = row;
        }
        table
    }
}

/// A file's cull box in its own frame: the `SegCull` box `into_staged`
/// builds (ink plus the segment pads, no z pad) before the group's T·R·S,
/// so `offset + local * scale` is that segment. The Visible field's items
/// carry it (their cull applies the live group row).
fn segment_local_box(v: &FileView) -> ([f32; 3], [f32; 3]) {
    use crate::glyph_scene::{SEG_CULL_PAD_MAX, SEG_CULL_PAD_MIN};
    (
        [v.ink.min[0] - SEG_CULL_PAD_MIN[0], v.ink.min[1] - SEG_CULL_PAD_MIN[1], v.ink.min[2]],
        [v.ink.max[0] + SEG_CULL_PAD_MAX[0], v.ink.max[1] + SEG_CULL_PAD_MAX[1], v.ink.max[2]],
    )
}

#[cfg(test)]
#[expect(
    clippy::single_range_in_vec_init,
    reason = "a one-element slice of fold ranges is a real input here (one fold), not a mistyped (a..b).collect()"
)]
mod tests {
    use super::*;
    use crate::layout::GlyphRecord;

    // ── P2a: the fold compaction ─────────────────────────────────────────
    // Synthetic records: one glyph per record, y = -row × 2.0 (the pitch the
    // empirical shift must recover WITHOUT being told), x = col, z = 0.

    fn rec(row: u32, col: u32) -> GlyphRecord {
        GlyphRecord {
            measures: [col as f32, -(row as f32) * 2.0, 0.0, 1.0, 2.0],
            counts: [100 + row + col, row, col],
        }
    }

    fn lines_of(rows: &[u32]) -> Vec<u32> {
        rows.to_vec()
    }

    #[test]
    fn fold_drops_its_records_and_shifts_the_rest_by_its_own_span() {
        // One record per line, lines 0..5. Fold lines 1..=2 (the line range
        // is exclusive-end: 1..3 drops the records on lines 1 AND 2; lines
        // 3,4 shift into the freed span).
        let records: Vec<GlyphRecord> = (0..5).map(|l| rec(l, 0)).collect();
        let folded = compact_folds(&records, &[1..3], &lines_of(&[0, 1, 2, 3, 4]));
        assert_eq!(folded.dropped, 2);
        assert_eq!(folded.records.len(), 3);
        // Shift = y(line 1) − y(line 3) = −2 − (−6) = +4: line 3's y moves
        // from −6 to −2 — into the slot line 1 vacated.
        assert_eq!(folded.records[1].y(), -2.0);
        assert_eq!(folded.records[2].y(), -4.0);
        // Rows renumber: 0,3,4 → 0,1,2.
        assert_eq!(folded.records.iter().map(|r| r.row()).collect::<Vec<_>>(), vec![0, 1, 2]);
        // Extents recomputed from the KEPT stream: bottom = −4 (was −8).
        assert_eq!(folded.page.bottom, -4.0);
        assert_eq!(folded.page.right, 1.0); // x + advance = 0 + 1
        // kept_ix: the join key back to parallel per-record data — the
        // compacted stream's originals, in order.
        assert_eq!(folded.kept_ix, vec![0, 3, 4]);
        // Glyph identity survives untouched — folds move, never rewrite.
        assert_eq!(folded.records[1].glyph_id(), 103);
    }

    #[test]
    fn multiple_folds_accumulate_their_spans() {
        let records: Vec<GlyphRecord> = (0..8).map(|l| rec(l, 0)).collect();
        let lines = lines_of(&(0..8).collect::<Vec<_>>());
        // Fold lines 1..2 and 5..6 (exclusive-end line ranges: 1..3, 5..7).
        let folded = compact_folds(&records, &[1..3, 5..7], &lines);
        assert_eq!(folded.dropped, 4);
        // Kept: 0,3,4,7. Fold 1 spans y(1)−y(3) = 4; fold 2 spans y(5)−y(7)
        // = 4; line 7 accumulates both (shift 8).
        assert_eq!(folded.records[3].y(), -14.0 + 8.0);
        assert_eq!(folded.records[3].row(), 3);
    }

    #[test]
    fn fold_at_eof_drops_but_shifts_nothing() {
        let records: Vec<GlyphRecord> = (0..4).map(|l| rec(l, 0)).collect();
        let folded = compact_folds(&records, &[3..4], &lines_of(&[0, 1, 2, 3]));
        assert_eq!(folded.dropped, 1);
        assert_eq!(folded.records.last().unwrap().y(), -4.0); // untouched
    }

    #[test]
    fn multi_record_lines_fold_and_shift_as_one_span() {
        // Lines 0 (2 records), 1 (3 records), 2 (1 record).
        let records = vec![rec(0, 0), rec(0, 1), rec(1, 0), rec(1, 1), rec(1, 2), rec(2, 0)];
        let lines = lines_of(&[0, 0, 1, 1, 1, 2]);
        let folded = compact_folds(&records, &[1..2], &lines);
        assert_eq!(folded.dropped, 3);
        assert_eq!(folded.records.len(), 3);
        // The span is measured line-to-line: y(1)−y(2) = +2, not per record.
        assert_eq!(folded.records[2].y(), -4.0 + 2.0);
    }

    #[test]
    fn a_records_less_fold_is_the_documented_blind_spot() {
        // No records on line 1 (the stream jumps 0→2): folding it frees
        // pitch the empirical shift cannot see. Nothing drops, nothing
        // moves — the v1 limitation, pinned so it can't silently "improve".
        let records = vec![rec(0, 0), rec(2, 0)];
        let folded = compact_folds(&records, &[1..2], &lines_of(&[0, 2]));
        assert_eq!(folded.dropped, 0);
        assert_eq!(folded.records[1].y(), -4.0);
    }

    /// `--render-file`'s tiling (2026-10-11), the frame the text stager made
    /// before the file became a repo load: four copies of a three-line file
    /// in a 2x2 grid, a block being the inked width by the rows down to the
    /// pen's row after the final newline, four advances apart, each with an
    /// identity group row (no directory tint); the bounds carry the one row
    /// above row 0. A file with no final newline has no pen row below it.
    #[test]
    fn tile_views_frame_copies_as_the_text_stager_did() {
        let line_height = 1.25f32;
        let view = |copy: u32| FileView {
            rel_path: "copy.rs".into(),
            dir: String::new(),
            buffer: 0,
            group_id: copy,
            byte_len: 30,
            record_count: 30,
            slot_base: 27 * copy as usize,
            slot_count: 27,
            ink: crate::layout::InkExtent { min: [0.0, -3.0, 0.0], max: [10.0, 0.5, 0.0] },
            width: 10.0,
            height: 3.5,
            z_min: 0.0,
            z_max: 0.0,
            offset: [0.0; 3],
            item: file_item_params(&RepoParams::default(), 30, 3),
        };
        // Rows 0..2, so the last record's y is -2.5; right counts the
        // newline's advance, which the block must not.
        let page = crate::layout::PageExtent { right: 10.5, bottom: -2.5, z_min: -0.25, z_max: 0.0 };
        let mut views: Vec<FileView> = (0..4).map(view).collect();
        let (rows, bounds_min, bounds_max) = tile_views(&mut views, &[page; 4], &[true; 4], line_height);
        let gap = 4.0 * text::fu_to_world(1229, 2320);
        let (block_width, block_height) = (10.0, 1.25 + 2.5 + 1.25);
        let want = [
            [0.0, 0.0, 0.0],
            [block_width + gap, 0.0, 0.0],
            [0.0, -(block_height + gap), 0.0],
            [block_width + gap, -(block_height + gap), 0.0],
        ];
        for (copy, (v, row)) in views.iter().zip(&rows).enumerate() {
            assert_eq!(v.offset, want[copy], "copy {copy}'s offset");
            assert_eq!(row.cols, GroupRow::identity(want[copy]).cols, "copy {copy}'s group row is the identity at its offset");
        }
        assert_eq!(bounds_min, [0.0, -(2.0 * block_height + gap), -0.25]);
        assert_eq!(bounds_max, [2.0 * block_width + gap, line_height, 0.0]);
        let (_, one_min, one_max) = tile_views(&mut views[..1], &[page], &[false], line_height);
        assert_eq!((one_min, one_max), ([0.0, -(1.25 + 2.5), -0.25], [block_width, line_height, 0.0]), "no final newline, no pen row");
    }

    /// The seam the whole z_wrap_spacing chain hangs on: the CLI flag sets
    /// RepoParams::z_wrap_spacing, and this is the ONE place it becomes the
    /// engine's z_step (× CELL_HEIGHT_WORLD, as CodeGrid.js:1365 computes
    /// it). If this mapping moves, the flag, the field's doc comment, and
    /// the web's semantics all have to move together.
    #[test]
    fn z_wrap_spacing_becomes_z_step_times_cell_height() {
        let p = RepoParams { z_wrap_spacing: 0.6, ..Default::default() };
        let item = file_item_params(&p, 10_000, 100);
        assert_eq!(item.z_step, text::CELL_HEIGHT_WORLD as f64 * 0.6);
        // 0 is the documented flat layout, in domain — not clamped away.
        let p = RepoParams { z_wrap_spacing: 0.0, ..Default::default() };
        assert_eq!(file_item_params(&p, 10_000, 100).z_step, 0.0);
        // The default pins the web's zWrapSpacing (the baselines render it).
        assert_eq!(RepoParams::default().z_wrap_spacing, 0.15);
    }
}
