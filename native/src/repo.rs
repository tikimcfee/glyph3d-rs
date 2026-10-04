//! Stage E2 — repository-scale loading.
//!
//! Walks a repository (source-extension whitelist; VCS/build/dependency dirs
//! skipped), runs the Mojo engine over every file, and fills ONE glyph arena.
//! HOW it crosses into Mojo is `layout_mojo::Strategy`'s business, not this
//! file's — the record strategies stage a 32 B wire stream and compact it here,
//! while `Direct` has the engine write instances into the arena and stages
//! nothing. `--repo-engine` selects; `--repo-verify` diffs two. Each file is a GROUP
//! (group_id == file index) placed on a 2D grid of code pages; files are
//! views {slot_base/count, engine params} into the shared arena — the
//! web's MegaGlyphField architecture (one arena, files as views).
//!
//! Correctness guards:
//! - only valid UTF-8 files reach the engine (valid UTF-8 cannot decode a
//!   codepoint past the trie's 4352-entry block index — the engine's decode
//!   assumes well-formed leads, see Stage E1 report);
//! - the 10 MB per-file cap also keeps every item far under the engine's
//!   per-item 2^24-byte ordinal wall (engine README, ordinal_invariant).

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Strategy {
    #[default]
    Hyper,
    Direct,
    Batched,
    PerItem,
    Cubecl,
}

impl Strategy {
    pub fn can_record(&self) -> bool {
        matches!(self, Strategy::Hyper | Strategy::Batched | Strategy::PerItem | Strategy::Cubecl)
    }

    pub fn materializes_records(&self) -> bool {
        matches!(self, Strategy::Batched | Strategy::PerItem | Strategy::Cubecl)
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
    /// params-level default (ItemParams agrees — engine-check's reference path
    /// depends on that); the CLI/product default flipped to cluster on
    /// 2026-09-22, and the goldens pin their mode explicitly.
    pub cluster_mode: crate::fold::ClusterMode,
    /// Spatial arrangement mode for the repository files across the canvas.
    pub layout_mode: RepoLayoutMode,
    /// Colorization strategy during load: `Syntax` (eager CPU lexer) or `Flat` (fast geometric load).
    pub color_mode: ColorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoLayoutMode {
    /// Traditional height-classed shelf packing across the whole repo.
    #[default]
    Shelf,
    /// Hierarchical directory-based neighborhood carrels.
    Carrel,
}

/// Colorization strategy during repo load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Eager CPU syntax coloring during load via colorize_leaders (default, preserves all goldens).
    #[default]
    Syntax,
    /// Fast geometric ingestion: uniform base color for glyphs, file-extension map for LOD backdrop tint.
    /// Defers per-glyph syntax coloring to external/asynchronous flows.
    Flat,
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
            layout_mode: RepoLayoutMode::Shelf,
            color_mode: ColorMode::Flat,
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

/// One file's view into the shared arena. Stage G picking reads the
/// slot-range fields (plus the per-file engine params, which make a
/// deterministic re-layout possible). The engine-record base and row count
/// were part of the original MegaGlyphField-style contract but no reader
/// ever landed — dropped in the Stage J sweep (re-derivable from `item`).
pub struct FileView {
    pub rel_path: String,
    /// Parent directory (relative) — the group-tint key.
    pub dir: String,
    pub group_id: u32,
    pub record_count: usize,
    /// Render-arena range (instances; blanks/missing are dropped from it).
    pub slot_base: usize,
    pub slot_count: usize,
    /// Page size in world units (before the group offset).
    pub width: f32,
    pub height: f32,
    /// Depth extent of the laid-out page, before the group offset.
    ///
    /// From `page`, not `ink`. In Z the two measure the same thing — a glyph
    /// quad has no thickness, so depth is a point per record — and page runs
    /// over ALL records while ink runs over survivors, making page a superset.
    /// A box containing everything drawn only costs a draw; one that does not
    /// can drop something visible.
    ///
    /// This does NOT generalise to Y: `page.bottom` tracks baselines and ink
    /// hangs half a glyph-height below them, which is why the xy below carries
    /// hand-tuned margins instead of using the page directly.
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
    /// The cubecl backend's own decomposition (rung 5's yardstick); None on
    /// the Mojo strategies, whose spans live in `phases`.
    #[cfg(feature = "cubecl")]
    pub cubecl: Option<crate::cubecl_layout::CubeclPhases>,
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
    pub color_mode: ColorMode,
    /// Stage G: repo root + engine trie — the pick path re-reads/re-runs
    /// individual files from these.
    pub root: PathBuf,
    pub trie: PathBuf,
    /// Hierarchical layout controller and spatial scene graph.
    pub controller: Option<crate::layout_stack::LayoutController>,
}

mod shelf;
pub use shelf::{extension_tint, DIR_TINTS};
pub(crate) use shelf::{dir_tint, layout_shelf};


/// Whole-repo load: walk → paint → the layout seam → grid layout.
///
/// `strategy` selects the Mojo backend's FFI strategy — a backend-internal
/// choice since the layout seam, threaded through only because the CLI still
/// exposes it. `verify` runs ANOTHER strategy as a second backend and diffs the
/// two AT THE SEAM: placements, instances and, when both can produce them,
/// records, all bit-exact. That is the standing check, and with the Rust
/// backend the same call diffs Mojo against Rust with nothing new written.
///
/// It was `batch: bool` until `Direct` landed. A boolean cannot name three
/// strategies, and the honest fix is the enum the backend already had rather
/// than a second flag beside the first.
pub fn load_repo(
    root: &Path,
    trie: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
) -> RepoLoad {
    load_repo_from_walk(
        root,
        walk_repo(root),
        trie,
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
#[allow(clippy::too_many_arguments)]
pub fn load_items(
    walk: WalkResult,
    _walk_dur: Duration,
    root: &Path,
    trie: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    load_repo_from_walk(
        root,
        walk,
        trie,
        params,
        strategy,
        verify,
        None,
        GlyphArena::new(),
        folds,
    )
}

/// `load_repo` with the walk and the arena already in hand: the caller walks
/// first so it can size the arena to the byte count (the render path's
/// device-mapped arena exists because of this split — leaders ≤ bytes, so
/// `walk.total_bytes` is the slot bound the direct path commits against).
/// `gpu` is the renderer's device when one exists — the cubecl backend
/// shares it (rung 5a) instead of constructing a second; `None` is the
/// no-GPU door (`--repo-scan-only`), where the chain makes its own.
#[allow(clippy::too_many_arguments)]
pub fn load_repo_from_walk(
    root: &Path,
    walk: WalkResult,
    trie: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    gpu: Option<&crate::gpu::GpuContext>,
    mut arena: GlyphArena,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {
    let _load = tracing::info_span!(
        "repo.load",
        files = walk.files.len(),
        bytes = walk.total_bytes,
        ?strategy,
        verify,
    )
    .entered();
    let walk_dur = walk.walk_dur;

    use rayon::prelude::*;

    // Per-file params (pagination sized per file). Newline counts double as
    // the row estimate — one fast byte scan per file.
    let file_params: Vec<ItemParams> = walk
        .files
        .par_iter()
        .map(|f| {
            let newlines = memchr::memchr_iter(b'\n', &f.bytes).count();
            file_item_params(params, f.bytes.len(), newlines)
        })
        .collect();

    // Paint is chosen from the SOURCE BYTES and indexed by RECORD, so it is
    // computed here and handed across the seam rather than applied to the
    // instances afterwards: compaction destroys the index that names a byte
    // (the argument is at `layout::Paint`).
    let mut stage_dur = std::time::Duration::ZERO;
    let paint_mode = match params.color_mode {
        ColorMode::Syntax => Paint::SyntaxHeuristic,
        ColorMode::Flat => Paint::Flat(crate::layout::DEFAULT_COLOR_PACKED),
    };

    let items: Vec<LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| LayoutItem {
            bytes: &f.bytes,
            params: file_params[index],
            group_id: index as u32,
            paint: paint_mode,
        })
        .collect();

    let mut backend = match strategy {
        #[cfg(feature = "cubecl")]
        Strategy::Cubecl => match gpu {
            Some(ctx) => crate::layout::LayoutEngine::cubecl_with_device(
                crate::cubecl_chain::SharedDevice::from_ctx(ctx),
            ),
            None => crate::layout::LayoutEngine::cubecl(),
        },
        _ => match gpu {
            Some(ctx) => crate::layout::LayoutEngine::hyper_with_device(
                crate::gpu::SharedDevice::from_ctx(ctx),
            ),
            None => crate::layout::LayoutEngine::hyper(),
        },
    };
    backend
        .load_trie_file(trie)
        .expect("failed to load engine trie");

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
        alt.load_trie_file(trie).expect("failed to load trie");
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
        for (index, f) in walk.files.iter().enumerate() {
            let placement = &mut placements[index];
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
                let item = &file_params[index];
                let newlines = f.bytes.iter().filter(|&&b| b == b'\n').count();
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
                    let item = &file_params[index];
                    let records =
                        rederive_cached(trie, &f.bytes, item).expect("fold: re-derive failed");
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
                            group_id: index as u32,
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
    let mut views: Vec<FileView> = Vec::with_capacity(walk.files.len());
    for (index, f) in walk.files.iter().enumerate() {
        let placed = &placements[index];
        total_records += placed.record_count as usize;
        total_blanks += (placed.record_count - placed.slot_count) as usize;
        views.push(FileView {
            rel_path: f.rel_path.clone(),
            dir: f.dir.clone(),
            group_id: index as u32,
            record_count: placed.record_count as usize,
            slot_base: placed.slot_base as usize,
            slot_count: placed.slot_count as usize,
            width: placed.page.right,
            height: -placed.page.bottom + params.line_height as f32,
            z_min: placed.page.z_min,
            z_max: placed.page.z_max,
            offset: [0.0; 3],
            item: file_params[index],
        });
    }
    let instances_len = arena.len();
    drop(sp_views);
    stage_dur += t.elapsed();

    let t = Instant::now();
    let sp_grid = tracing::info_span!("repo.layout").entered();
    let mut controller = crate::layout_stack::LayoutController::from_mode(params.layout_mode, *params);
    let (groups, bounds_min, bounds_max) = controller.apply(&mut views);
    drop(sp_grid);
    let layout_dur = t.elapsed();

    let stats = LoadStats {
        walk: walk_dur,
        backend: backend_dur,
        phases: backend.phases(),
        #[cfg(feature = "cubecl")]
        cubecl: backend.cubecl_phases(),
        stage: stage_dur,
        layout: layout_dur,
        files: walk.files.len(),
        bytes: walk.total_bytes,
        records: total_records,
        instances: instances_len,
        blanks: total_blanks,
        skipped_large: walk.skipped_large,
        skipped_non_utf8: walk.skipped_non_utf8,
        dirs_visited: walk.dirs_visited,
        strategy,
        verified,
    };
    RepoLoad {
        arena,
        groups,
        files: views,
        bounds_min,
        bounds_max,
        stats,
        color_mode: params.color_mode,
        root: root.to_path_buf(),
        trie: trie.to_path_buf(),
        controller: Some(controller),
    }
}

mod rederive;
pub use rederive::{compact_folds, rederive_cached, rederive_from_bytes, rederive_records, Folded};


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
        #[cfg(feature = "cubecl")]
        let tint_stream: Option<&[u32]> = if mapped_slots.is_none() {
            self.arena.device_slots().map(|d| d.tint.as_slice())
        } else {
            None
        };
        #[cfg(not(feature = "cubecl"))]
        let tint_stream: Option<&[u32]> = None;
        let chunks = if mapped_slots.is_none() && tint_stream.is_none() {
            self.arena.instance_chunks()
        } else {
            Vec::new()
        };
        let is_flat = self.color_mode == ColorMode::Flat;
        let file_tints = self.arena.device_slots().map(|d| &d.file_tints);
        let file_blocks = self.arena.device_slots().map(|d| &d.file_blocks);
        let seg_of = |v: &FileView| {
            let tint = if is_flat {
                let area = (v.width as f64 * v.height as f64).max(1e-3);
                let ink_frac = (v.slot_count as f64 * crate::glyph_scene::GLYPH_CELL_AREA as f64 / area).min(1.0);
                let e = (ink_frac * crate::glyph_scene::BACKDROP_GAIN as f64).min(1.0);
                let rgb = extension_tint(&v.rel_path);
                [rgb[0], rgb[1], rgb[2], e as f32]
            } else {
                let fast_tint = file_tints.and_then(|t| t.get(v.group_id as usize));
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
                        if let Some(slots) = mapped_slots {
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
            let fast_blocks = file_blocks.and_then(|fb| fb.get(v.group_id as usize));
            let blocks = if let Some(fbs) = fast_blocks {
                fbs.iter()
                    .map(|lb| crate::glyph_scene::BlockCull {
                        min: [
                            v.offset[0] + lb.min[0] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[0],
                            v.offset[1] + lb.min[1] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[1],
                            v.offset[2] + lb.min[2] - crate::glyph_scene::BLOCK_CULL_PAD_MIN[2],
                        ],
                        max: [
                            v.offset[0] + lb.max[0] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[0],
                            v.offset[1] + lb.max[1] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[1],
                            v.offset[2] + lb.max[2] + crate::glyph_scene::BLOCK_CULL_PAD_MAX[2],
                        ],
                        slot_base: (v.slot_base as u32) + lb.slot_base,
                        slot_count: lb.slot_count,
                    })
                    .collect()
            } else {
                build_file_blocks(v, mapped_slots, &chunks)
            };
            crate::glyph_scene::SegCull {
                min: [
                    v.offset[0] - crate::glyph_scene::SEG_CULL_PAD_MIN[0],
                    v.offset[1] - v.height - crate::glyph_scene::SEG_CULL_PAD_MIN[1],
                    v.offset[2] + v.z_min,
                ],
                max: [
                    v.offset[0] + v.width + crate::glyph_scene::SEG_CULL_PAD_MAX[0],
                    v.offset[1] + crate::glyph_scene::SEG_CULL_PAD_MAX[1],
                    v.offset[2] + v.z_max,
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
            self.files.par_iter().map(seg_of).collect()
        };
        drop(sp_segments);
        let pick_files: Vec<crate::glyph_scene::PickFileInfo> = self
            .files
            .iter()
            .map(|v| crate::glyph_scene::PickFileInfo {
                rel_path: v.rel_path.clone(),
                group_id: v.group_id,
                slot_base: v.slot_base as u32,
                slot_count: v.slot_count as u32,
                item: v.item,
                aabb_min: [
                    -crate::glyph_scene::SEG_CULL_PAD_MIN[0],
                    -v.height - crate::glyph_scene::SEG_CULL_PAD_MIN[1],
                    v.z_min - crate::glyph_scene::PICK_AABB_PAD_Z,
                ],
                aabb_max: [
                    v.width + crate::glyph_scene::SEG_CULL_PAD_MAX[0],
                    crate::glyph_scene::SEG_CULL_PAD_MAX[1],
                    v.z_max + crate::glyph_scene::PICK_AABB_PAD_Z,
                ],
            })
            .collect();
        let mut focus_bounds = None;
        if let Some(needle) = focus {
            if let Some(v) = self.files.iter().find(|v| v.rel_path.contains(needle)) {
                let cx = v.offset[0] + v.width * 0.5;
                let cy = v.offset[1] - v.height * 0.5;
                focus_bounds = Some((
                    [cx, cy],
                    [v.width.max(1.0) * 0.5, v.height.max(1.0) * 0.5],
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
        StagedText {
            glyphs_emitted: self.arena.len(),
            codepoints_decoded: self.stats.records,
            missing_or_bitmap: self.stats.blanks,
            segments,
            instances: self.arena,
            groups: self.groups,
            bounds_min: self.bounds_min,
            bounds_max: self.bounds_max,
            focus_bounds,
            pick: Some(crate::glyph_scene::PickContext {
                root: self.root,
                trie: self.trie,
                files: pick_files,
                // Envelope-owned content is the CALLER's to inject (it owns
                // the bytes); disk scenes re-derive from root. Same for the
                // fold set (P2a) — the loader's compacted field is the
                // caller's to describe back.
                content: None,
                folds: std::collections::HashMap::new(),
            }),
            controller: self.controller,
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
                Strategy::Batched => "mojo-cpu/batched",
                Strategy::PerItem => "mojo-cpu/per-item",
                Strategy::Direct => "mojo-cpu/direct",
                Strategy::Cubecl => "device/cubecl (endpoint)",
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
        #[cfg(feature = "cubecl")]
        let has_cubecl = s.cubecl.is_some();
        #[cfg(not(feature = "cubecl"))]
        let has_cubecl = false;

        #[cfg(feature = "cubecl")]
        if let Some(cp) = s.cubecl {
            // The device chain's stages are a different shape from the
            // record/direct split below — printing the Mojo block for it
            // would show zeros for stages that RAN, the exact ambiguity the
            // n/a rule exists against. Cubecl reports its own spans.
            let ch = &cp.chain;
            let accounted = cp.marshal
                + ch.prep
                + ch.tables
                + ch.init
                + ch.upload
                + ch.dispatch
                + cp.emit_readback
                + cp.convert
                + cp.compact;
            println!(
                "  cubecl chain: prep {:.3}s | tables {:.3}s | init {:.3}s | pack+upload {:.3}s \
                 | dispatch {:.3}s | emit+readback {:.3}s ({:.2} GB records)",
                ch.prep.as_secs_f64(),
                ch.tables.as_secs_f64(),
                ch.init.as_secs_f64(),
                ch.upload.as_secs_f64(),
                ch.dispatch.as_secs_f64(),
                cp.emit_readback.as_secs_f64(),
                (s.records * 32) as f64 / 1.073_741_824e9,
            );
            println!(
                "  cubecl host: marshal {:.3}s | convert {:.3}s | compact {:.3}s \
                 | unattributed {:.3}s",
                cp.marshal.as_secs_f64(),
                cp.convert.as_secs_f64(),
                cp.compact.as_secs_f64(),
                s.backend.saturating_sub(accounted).as_secs_f64(),
            );
        }
        if !has_cubecl {
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
            // `fold` above is the whole FFI call. This is what the engine says it
            // spent inside it — largest lane first, and `unattributed` here catches
            // the part of the call that is neither run_pipeline nor the two stages
            // the FFI entry owns (marshalling, arena reuse, the return trip).
            //
            // Zero-valued engine lanes are ELIDED rather than printed as 0.000s,
            // for the same reason: a lane absent from this line did not run on this
            // path. `eg_compact`/`eg_counts` belong to the record path and
            // `eg_direct` to the direct one, so which lanes appear is itself the
            // statement of which route the load took.
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
}

/// The repo paint pass: per-file `colorize_leaders`, sharded by BYTE-balanced
/// contiguous file ranges once the corpus is big enough to pay for threads
/// (the seg_tint precedent, but byte-bound: the flagship's files span
/// 100 B..2 MB, so a count-based split strands a worker on a bundle).
/// Per-file colorize state is independent by construction and the ranges
/// concatenate in file order — bit-identical to the serial map (the
/// `sharded_paint_matches_serial` test fences exactly that; the golden views
/// only ever run the serial arm). 1.0s -> ~0.24s at the flagship, 2026-09-30.
#[cfg(test)]
fn paint_files(files: &[&[u8]]) -> Vec<Vec<u32>> {
    use rayon::prelude::*;
    files.par_iter().map(|f| text::colorize_leaders(f)).collect()
}

#[cfg(test)]
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

    /// The sharded paint path (>= 64 files) must reproduce the serial map
    /// bit-identically — the golden views run the serial path (5 files), so
    /// without this test nothing exercises the shard cut/order logic.
    #[test]
    fn sharded_paint_matches_serial() {
        let files: Vec<Vec<u8>> = (0..100usize)
            .map(|i| {
                format!(
                    "// comment {i}\nlet s{i} = \"str {i}\" + '🦀'; // 🚀\nfn w{i}_ord(ord) {{ }}\n"
                )
                .into_bytes()
            })
            .collect();
        let refs: Vec<&[u8]> = files.iter().map(|f| f.as_slice()).collect();
        let serial: Vec<Vec<u32>> = refs.iter().map(|b| text::colorize_leaders(b)).collect();
        assert_eq!(serial, paint_files(&refs));
        // And below the shard threshold the serial arm answers directly.
        let few: Vec<&[u8]> = refs[..3].to_vec();
        assert_eq!(paint_files(&few), serial[..3].to_vec());
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
