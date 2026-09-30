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

use crate::engine::Engine;
use crate::glyph_scene::GroupRow;
use crate::layout::{BackendOutput, GlyphArena, GlyphRecord, ItemParams, ItemPlacement, LayoutError, LayoutGlyphs, LayoutItem, Paint, VerifyLayout, diff_backends};
use crate::layout_mojo::{BackendPhases, MojoLayout, Strategy};
use crate::text::{self, StagedText};

/// Per-file read cap. Doubles as the ordinal-wall guard (2^24 B = 16 MiB).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

const SKIP_DIRS: &[&str] = &[
    ".git", ".svn", ".hg", "node_modules", "target", ".pixi", "dist", "build", "out",
    ".next", ".nuxt", ".cache", "coverage", "__pycache__", ".idea", ".vscode",
    ".turbo", ".vercel", "tmp", "temp",
];

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "py", "mojo", "c", "h", "cpp",
    "hpp", "cc", "hh", "cs", "java", "kt", "swift", "rb", "md", "markdown", "json",
    "jsonc", "toml", "yaml", "yml", "xml", "wgsl", "glsl", "vert", "frag", "metal",
    "css", "scss", "html", "htm", "sh", "bash", "zsh", "fish", "lock", "txt", "sql",
    "lua", "zig", "ex", "exs", "hs", "ml", "clj", "scala", "php", "pl", "r", "jl",
    "nim", "d", "vue", "svelte",
];

/// One walked source file.
pub struct RepoFile {
    pub rel_path: String,
    /// Parent directory (relative, "" at the root) — the group-tint key.
    pub dir: String,
    pub bytes: Vec<u8>,
}

pub struct WalkResult {
    pub files: Vec<RepoFile>,
    pub total_bytes: usize,
    pub skipped_large: usize,
    pub skipped_non_utf8: usize,
    pub dirs_visited: usize,
    /// The walk's own wall time, measured where the walk happens. The load's
    /// `walk` stat reads THIS — the old span inside `load_repo_from_walk`
    /// timed nothing (the walk arrives already done), which is why it printed
    /// 0.000s on corpora that take real milliseconds to read.
    pub walk_dur: std::time::Duration,
}

/// Recursive walk, deterministic order (files sorted by relative path).
pub fn walk_repo(root: &Path) -> WalkResult {
    let _sp = tracing::info_span!("repo.walk", root = %root.display()).entered();
    let t0 = std::time::Instant::now();
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let mut skipped_large = 0usize;
    let mut dirs_visited = 0usize;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        dirs_visited += 1;
        for entry in rd.flatten() {
            let name = match entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(entry.path());
                continue;
            }
            if !ft.is_file() || name.starts_with('.') {
                continue;
            }
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            if !SOURCE_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size > MAX_FILE_BYTES {
                skipped_large += 1;
                continue;
            }
            let rel = match entry.path().strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            candidates.push((rel, entry.path()));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut files = Vec::with_capacity(candidates.len());
    let mut total_bytes = 0usize;
    let mut skipped_non_utf8 = 0usize;
    for (rel, path) in candidates {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // Valid UTF-8 only: the engine's decode assembles codepoints from lead
        // bytes WITHOUT validating continuations; well-formed UTF-8 is the
        // documented precondition for staying inside the trie's block index.
        if std::str::from_utf8(&bytes).is_err() {
            skipped_non_utf8 += 1;
            continue;
        }
        total_bytes += bytes.len();
        let dir = rel
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        files.push(RepoFile {
            rel_path: rel,
            dir,
            bytes,
        });
    }
    WalkResult {
        files,
        total_bytes,
        skipped_large,
        skipped_non_utf8,
        dirs_visited,
        walk_dur: t0.elapsed(),
    }
}

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
    /// Stage G: repo root + engine trie — the pick path re-reads/re-runs
    /// individual files from these.
    pub root: PathBuf,
    pub trie: PathBuf,
}

/// Subtle per-directory tints (multiplied with the syntax colors through the
/// group color, colorBlend 0). Bright pastels — the hue shift groups a
/// directory's pages without drowning the syntax palette. Stage G's
/// `tint-cycle` verb walks this same palette.
pub const DIR_TINTS: &[[f32; 3]] = &[
    [1.00, 1.00, 1.00],
    [1.00, 0.93, 0.87],
    [0.88, 1.00, 0.92],
    [0.88, 0.95, 1.00],
    [1.00, 0.91, 1.00],
    [0.95, 1.00, 0.88],
    [1.00, 0.97, 0.86],
    [0.92, 0.92, 1.00],
    [0.90, 1.00, 1.00],
    [1.00, 0.88, 0.88],
];

fn dir_tint(dir: &str) -> [f32; 3] {
    // FNV-1a 32-bit over the directory path.
    let mut h: u32 = 0x811C9DC5;
    for b in dir.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    DIR_TINTS[(h as usize) % DIR_TINTS.len()]
}

/// Packed SHELF layout, classed by height: files are stably partitioned into
/// height classes (small / medium / large), each class shelf-packed
/// left-to-right in path order (consecutive files — same directory — stay
/// adjacent within a class), classes stacked top to bottom. Shelf packing
/// alone pays each shelf its tallest member's height; with a real repo's
/// skewed heights (1-page files next to 400-page lock files) that wastes
/// 2-3× the field's height. Classing keeps shelf-mates similar in height,
/// so the field's aspect actually approaches `grid_aspect`.
fn layout(
    views: &mut [FileView],
    params: &RepoParams,
) -> (Vec<GroupRow>, [f32; 3], [f32; 3]) {
    let page_h = params.page_rows as f32 * params.line_height as f32;
    // Class bounds in world units: ≤2 pages tall, ≤16 pages tall, monsters.
    let class_of = |h: f32| -> usize {
        if h <= 2.5 * page_h {
            0
        } else if h <= 16.5 * page_h {
            1
        } else {
            2
        }
    };
    let mut order: Vec<usize> = (0..views.len()).collect();
    order.sort_by_key(|&i| class_of(views[i].height)); // stable: path order kept within a class

    let area: f32 = views
        .iter()
        .map(|v| (v.width + params.gap_x) * (v.height + params.gap_y))
        .sum();
    let target_w = (area * params.grid_aspect).sqrt().max(1.0);

    let mut offsets: Vec<[f32; 3]> = vec![[0.0; 3]; views.len()];
    let mut x = 0f32; // pen: left edge of the next page on this shelf
    let mut shelf_top = 0f32; // y of the current shelf's top edge
    let mut shelf_h = 0f32; // tallest page on the current shelf
    let mut max_x = 0f32;
    let mut prev_class = 0usize;
    for &i in &order {
        let v = &views[i];
        let cls = class_of(v.height);
        // Class boundary or full shelf → close the shelf, drop down.
        if (cls != prev_class) || (x > 0.0 && x + v.width > target_w) {
            shelf_top -= shelf_h + params.gap_y;
            x = 0.0;
            shelf_h = 0.0;
            prev_class = cls;
        }
        offsets[i] = [x, shelf_top, 0.0];
        x += v.width + params.gap_x;
        max_x = max_x.max(x - params.gap_x);
        shelf_h = shelf_h.max(v.height);
    }
    let min_y = shelf_top - shelf_h;

    let mut groups = Vec::with_capacity(views.len());
    // Scene depth is the union of the placed files' own depth, not a constant.
    // Seeded at 0 so a wholly planar field still reports a zero-thickness slab
    // rather than an inverted one.
    let (mut min_z, mut max_z) = (0.0f32, 0.0f32);
    for (v, off) in views.iter_mut().zip(offsets.iter()) {
        v.offset = *off;
        min_z = min_z.min(v.offset[2] + v.z_min);
        max_z = max_z.max(v.offset[2] + v.z_max);
        groups.push(GroupRow::tinted(v.offset, dir_tint(&v.dir)));
    }
    log::info!(
        "layout: target_w {:.0}, field {:.0}x{:.0}",
        target_w,
        max_x.max(1.0),
        -min_y,
    );
    (
        groups,
        [0.0, min_y, min_z],
        [max_x.max(1.0), params.line_height as f32, max_z],
    )
}

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

    // Per-file params (pagination sized per file). Newline counts double as
    // the row estimate — one fast byte scan per file.
    let file_params: Vec<ItemParams> = walk
        .files
        .iter()
        .map(|f| {
            let newlines = f.bytes.iter().filter(|&&b| b == b'\n').count();
            file_item_params(params, f.bytes.len(), newlines)
        })
        .collect();

    // Paint is chosen from the SOURCE BYTES and indexed by RECORD, so it is
    // computed here and handed across the seam rather than applied to the
    // instances afterwards: compaction destroys the index that names a byte
    // (the argument is at `layout::Paint`).
    let t = Instant::now();
    let sp_paint = tracing::info_span!("repo.paint").entered();
    let file_bytes: Vec<&[u8]> = walk.files.iter().map(|f| f.bytes.as_slice()).collect();
    let colors = paint_files(&file_bytes);
    let mut stage_dur = t.elapsed();
    drop(sp_paint);

    let items: Vec<LayoutItem<'_>> = walk
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| LayoutItem {
            bytes: &f.bytes,
            params: file_params[index],
            group_id: index as u32,
            paint: Paint::PerRecord(&colors[index]),
        })
        .collect();

    // The seam's one branch point: Cubecl crosses into the device chain,
    // everything else into Mojo. A local enum rather than a second
    // code path per call site — the recording/verify flow below is the
    // backend's OWN contract either way.
    enum Backend {
        Mojo(MojoLayout),
        Cubecl(crate::cubecl_layout::CubeclLayout),
    }
    impl LayoutGlyphs for Backend {
        fn name(&self) -> &'static str {
            match self {
                Backend::Mojo(b) => b.name(),
                Backend::Cubecl(b) => b.name(),
            }
        }
        fn load_trie_file(&mut self, path: &Path) -> Result<(), LayoutError> {
            match self {
                Backend::Mojo(b) => b.load_trie_file(path),
                Backend::Cubecl(b) => b.load_trie_file(path),
            }
        }
        fn layout_validated_items(
            &mut self,
            items: &[LayoutItem<'_>],
            arena: &mut GlyphArena,
        ) -> Result<Vec<ItemPlacement>, LayoutError> {
            match self {
                Backend::Mojo(b) => b.layout_validated_items(items, arena),
                Backend::Cubecl(b) => b.layout_validated_items(items, arena),
            }
        }
    }
    impl Backend {
        /// The Mojo backend's phase meter (load stats). Cubecl reports
        /// nothing here — its spans are a different shape entirely, and
        /// zeros in this struct would read as stages that ran fast.
        fn phases(&self) -> BackendPhases {
            match self {
                Backend::Mojo(b) => b.phases(),
                Backend::Cubecl(_) => BackendPhases::default(),
            }
        }
        /// The cubecl backend's decomposition (rung 5's yardstick).
        fn cubecl_phases(&self) -> Option<crate::cubecl_layout::CubeclPhases> {
            match self {
                Backend::Mojo(_) => None,
                Backend::Cubecl(b) => Some(b.phases()),
            }
        }
    }
    impl VerifyLayout for Backend {
        fn layout_validated_items_recording(
            &mut self,
            items: &[LayoutItem<'_>],
            arena: &mut GlyphArena,
        ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
            match self {
                Backend::Mojo(b) => b.layout_validated_items_recording(items, arena),
                Backend::Cubecl(b) => b.layout_validated_items_recording(items, arena),
            }
        }
    }
    let mut backend = match strategy {
        Strategy::Cubecl => Backend::Cubecl(match gpu {
            Some(ctx) => crate::cubecl_layout::CubeclLayout::with_device(
                crate::cubecl_chain::SharedDevice::from_ctx(ctx),
            ),
            None => crate::cubecl_layout::CubeclLayout::new(),
        }),
        other => Backend::Mojo(MojoLayout::new(other)),
    };
    backend
        .load_trie_file(trie)
        .expect("failed to load engine trie");

    let t = Instant::now();
    // Under --repo-verify the selected backend ALSO records its wire stream,
    // when it has one, so the other can be diffed against it at every
    // granularity. Without it nothing asks for records at all, which is the
    // seam's entire point.
    //
    // `Direct` has no wire stream by construction, so under it the diff is
    // instances and placements only — which is the whole render-visible
    // contract, and the granularity that matters. `diff_backends` is told the
    // records are absent rather than being handed an empty slice to interpret.
    let sp_backend = tracing::info_span!("repo.backend").entered();
    let (placements, records) = if verify && strategy.can_record() {
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
        // The counterpart to diff against. Direct is checked against Batched
        // because that is the strategy it replaces; the other two check each
        // other, which is the pairing that existed before it.
        let other = match strategy {
            Strategy::Batched => Strategy::PerItem,
            Strategy::PerItem | Strategy::Direct | Strategy::Cubecl => Strategy::Batched,
        };
        let mut alt = MojoLayout::new(other);
        alt.load_trie_file(trie)
            .expect("failed to load engine trie");
        let mut alt_arena = GlyphArena::new();
        let (alt_placements, alt_records) = alt
            .layout_items_recording(&items, &mut alt_arena)
            .expect("layout failed");
        // The instances may live in several chunk buffers (the chunked
        // mapped arena); verify paths pay the flatten when so. Free for the
        // host and single-buffer forms.
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
                name: alt.name(),
                placements: &alt_placements,
                instances: &alt_flat,
                records: &alt_records,
            },
        )
        .unwrap_or_else(|why| panic!("repo-verify FAIL: {why}"));
        // ANTI-VACUITY, and it is not hypothetical: before this guard,
        //     --load-repo fixtures/does-not-exist --repo-verify
        // printed "repo-verify PASS: 0 items, 0 instances" and exited 0. The
        // gate runner greens on that substring, so the direct path's ONLY check
        // would have passed having compared nothing at all — if the fixture
        // directory were ever moved, renamed or emptied. A comparison of two
        // empty things is not a verification, and this is the one check in the
        // battery whose failure mode was silence rather than noise.
        if report.items == 0 || report.instances == 0 {
            panic!(
                "repo-verify FAIL: nothing to compare — {} items, {} instances. \
                 A corpus that produces no glyphs cannot verify anything; check \
                 that the corpus path exists and holds files the walker accepts",
                report.items, report.instances,
            );
        }
        backend_dur += t.elapsed(); // honest: verification time is backend time
        verified = true;
        println!(
            "repo-verify PASS: {} items, {} instances, {} records bit-exact between {} and {}",
            report.items,
            report.instances,
            report.records,
            backend.name(),
            alt.name(),
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
            // Paginated footprint: glyph centers run from y=0 down to the page
            // bottom; one line pitch of margin covers the bottom row's
            // descenders.
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
    let (groups, bounds_min, bounds_max) = layout(&mut views, params);
    drop(sp_grid);
    let layout_dur = t.elapsed();

    let stats = LoadStats {
        walk: walk_dur,
        backend: backend_dur,
        phases: backend.phases(),
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
        root: root.to_path_buf(),
        trie: trie.to_path_buf(),
    }
}

/// Stage G — deterministic per-file re-layout for picking: re-read the file
/// from the repo root and re-run the engine with the EXACT ItemParams it was
/// staged with (carried in the PickFileInfo). Same bytes + same params + same
/// engine ⇒ the record stream is bit-identical to what `load_repo` staged, so
/// record positions, ROW/COL, and the blank-drop slot mapping all line up
/// with the arena. Returns the records plus the raw file bytes (the char
/// resolution walk reads them).
pub fn rederive_records(
    root: &Path,
    trie: &Path,
    rel_path: &str,
    item: &ItemParams,
) -> std::io::Result<(Vec<GlyphRecord>, Vec<u8>)> {
    let bytes = std::fs::read(root.join(rel_path))?;
    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("pick: failed to load engine trie");
    eng.load_item(&bytes, item).expect("pick: engine re-run failed");
    Ok((eng.read_back().records, bytes))
}

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
        let tint_stream: Option<&[u32]> =
            self.arena.device_slots().map(|d| d.tint.as_slice());
        let chunks = if tint_stream.is_none() {
            self.arena.instance_chunks()
        } else {
            Vec::new()
        };
        let seg_of = |v: &FileView| {
            // The file's slot range folded in arena order: chunk slices
            // ascend and concatenate exactly, so a range that straddles a
            // chunk boundary tints bit-identically to the contiguous fold.
            let mut tint = crate::glyph_scene::SegTintAccum::new(slot_ink);
            let want = v.slot_base..v.slot_base + v.slot_count;
            match tint_stream {
                Some(tp) => tint.add_tint(&tp[want.start * 2..want.end * 2]),
                None => {
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
            }
            crate::glyph_scene::SegCull {
                min: [
                    v.offset[0] - 0.3,
                    v.offset[1] - v.height - 0.5,
                    v.offset[2] + v.z_min,
                ],
                max: [
                    v.offset[0] + v.width + 0.6,
                    v.offset[1] + 0.75,
                    v.offset[2] + v.z_max,
                ],
                slot_base: v.slot_base as u32,
                slot_count: v.slot_count as u32,
                tint: tint.finish(v.slot_count, v.width, v.height),
            }
        };
        // seg_tint re-reads the whole arena, one file's slice at a time —
        // ~500 ms serial on the glyph3d-js repo (1,306 files). Shard by file
        // range: per-segment sums are independent and the per-worker results
        // concatenate in file order, so the table is bit-identical. Small
        // repos stay serial (thread spawn would cost more than the pass).
        let sp_segments = tracing::info_span!("repo.segments").entered();
        let segments: Vec<crate::glyph_scene::SegCull> = if self.files.len() >= 64 {
            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(8);
            let span = self.files.len().div_ceil(workers);
            let seg_ref = &seg_of;
            std::thread::scope(|s| {
                let handles: Vec<_> = self
                    .files
                    .chunks(span)
                    .map(|range| s.spawn(move || range.iter().map(seg_ref).collect::<Vec<_>>()))
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().expect("segment worker panicked"))
                    .collect()
            })
        } else {
            self.files.iter().map(seg_of).collect()
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
                aabb_min: [-0.3, -v.height - 0.5],
                aabb_max: [v.width + 0.6, 0.75],
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
            }),
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
        } else {
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
fn paint_files(files: &[&[u8]]) -> Vec<Vec<u32>> {
    if files.len() >= 64 {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8);
        let total: usize = files.iter().map(|f| f.len()).sum();
        let target = total.div_ceil(workers).max(1);
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(workers);
        let (mut start, mut acc) = (0usize, 0usize);
        for (i, f) in files.iter().enumerate() {
            acc += f.len();
            if acc >= target {
                ranges.push((start, i + 1));
                start = i + 1;
                acc = 0;
            }
        }
        if start < files.len() {
            ranges.push((start, files.len()));
        }
        std::thread::scope(|s| {
            let handles: Vec<_> = ranges
                .iter()
                .map(|&(a, b)| {
                    s.spawn(move || {
                        files[a..b]
                            .iter()
                            .map(|f| text::colorize_leaders(f))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("paint worker panicked"))
                .collect()
        })
    } else {
        files.iter().map(|f| text::colorize_leaders(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
