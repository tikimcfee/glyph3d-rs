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
use crate::glyph_scene::{GlyphInstance, GroupRow};
use crate::layout::{
    diff_backends, BackendOutput, GlyphArena, GlyphRecord, InkExtent, ItemParams, LayoutGlyphs,
    LayoutItem, PageExtent, Paint, VerifyLayout,
};
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
}

impl WalkResult {
    /// The in-memory walk: content the CALLER owns (P1-live — the seam's
    /// envelope bytes), not a directory. There are no skip semantics to
    /// report — the caller already decided what exists — so the counters are
    /// zero and `dirs_visited` is 1 (the notional root).
    pub fn from_files(files: Vec<RepoFile>) -> WalkResult {
        let total_bytes = files.iter().map(|f| f.bytes.len()).sum();
        WalkResult {
            files,
            total_bytes,
            skipped_large: 0,
            skipped_non_utf8: 0,
            dirs_visited: 1,
        }
    }
}

impl RepoFile {
    /// An in-memory file: `rel_path` as it would appear under a repo root
    /// (the dir-tint key is its parent, "" at the root — same rule the
    /// walker derives).
    pub fn in_memory(rel_path: impl Into<String>, bytes: Vec<u8>) -> RepoFile {
        let rel_path = rel_path.into();
        let dir = rel_path
            .rfind('/')
            .map(|i| rel_path[..i].to_string())
            .unwrap_or_default();
        RepoFile { rel_path, dir, bytes }
    }
}

/// Recursive walk, deterministic order (files sorted by relative path).
pub fn walk_repo(root: &Path) -> WalkResult {
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
    pub instances: Vec<GlyphInstance>,
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
    let t0 = Instant::now();
    let walk = walk_repo(root);
    let walk_dur = t0.elapsed();
    load_items(walk, walk_dur, root, trie, params, strategy, verify, None)
}

/// The loader proper — everything past the walk. Split so the SAME pipeline
/// serves disk walks (`load_repo`) and caller-owned content
/// (`WalkResult::from_files`, the P1-live envelope path): the fold is a pure
/// function of (bytes, params) either way, and only the bytes' provenance
/// differs. `walk_dur` is the caller's honest walk cost (disk I/O for
/// `load_repo`, ~0 for in-memory) so the phases instrument stays truthful.
/// `root` is the pick-path fallback — envelope-owned scenes override it per
/// file by injecting `PickContext::content`.
///
/// `folds` (P2a): per-rel_path NORMALIZED line ranges (from
/// `seam::normalized_fold_lines`). Folded files' arena slices are rebuilt
/// from their compacted record streams; placements, extents and slot bases
/// are fixed in the same pass, so every consumer downstream of the
/// placement (views, bounds, staging, slot table) sees a consistent field.
/// KNOWN GAP, deliberate v1: the PICK path re-derives UNCOMPACTED records,
/// so a pick on a folded file resolves rows against the unfolded stream
/// until `PickContext` carries the fold set (queued with P2b).
#[allow(clippy::too_many_arguments)]
pub fn load_items(
    walk: WalkResult,
    walk_dur: Duration,
    root: &Path,
    trie: &Path,
    params: &RepoParams,
    strategy: Strategy,
    verify: bool,
    folds: Option<&std::collections::HashMap<String, Vec<std::ops::Range<u32>>>>,
) -> RepoLoad {

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
    let colors: Vec<Vec<u32>> = walk
        .files
        .iter()
        .map(|f| text::colorize_leaders(&f.bytes))
        .collect();
    let mut stage_dur = t.elapsed();

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

    let mut backend = MojoLayout::new(strategy);
    backend
        .load_trie_file(trie)
        .expect("failed to load engine trie");

    let mut arena = GlyphArena::new();
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
    let mut backend_dur = t.elapsed();

    let mut verified = false;
    if verify {
        let t = Instant::now();
        // The counterpart to diff against. Direct is checked against Batched
        // because that is the strategy it replaces; the other two check each
        // other, which is the pairing that existed before it.
        let other = match strategy {
            Strategy::Batched => Strategy::PerItem,
            Strategy::PerItem | Strategy::Direct => Strategy::Batched,
        };
        let mut alt = MojoLayout::new(other);
        alt.load_trie_file(trie)
            .expect("failed to load engine trie");
        let mut alt_arena = GlyphArena::new();
        let (alt_placements, alt_records) = alt
            .layout_items_recording(&items, &mut alt_arena)
            .expect("layout failed");
        let report = diff_backends(
            &BackendOutput {
                name: backend.name(),
                placements: &placements,
                instances: arena.instances(),
                records: &records,
            },
            &BackendOutput {
                name: alt.name(),
                placements: &alt_placements,
                instances: alt_arena.instances(),
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
    let instances = arena.into_instances();
    stage_dur += t.elapsed();

    let t = Instant::now();
    let (groups, bounds_min, bounds_max) = layout(&mut views, params);
    let layout_dur = t.elapsed();

    let stats = LoadStats {
        walk: walk_dur,
        backend: backend_dur,
        phases: backend.phases(),
        stage: stage_dur,
        layout: layout_dur,
        files: walk.files.len(),
        bytes: walk.total_bytes,
        records: total_records,
        instances: instances.len(),
        blanks: total_blanks,
        skipped_large: walk.skipped_large,
        skipped_non_utf8: walk.skipped_non_utf8,
        dirs_visited: walk.dirs_visited,
        strategy,
        verified,
    };
    RepoLoad {
        instances,
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
    let records = rederive_from_bytes(trie, &bytes, item)?;
    Ok((records, bytes))
}

/// The engine re-run without the disk read — the P1-live form. `rederive_records`
/// is this plus `fs::read`; envelope-owned scenes call this directly with the
/// bytes they folded, which is what makes the seam's version join meaningful
/// for live content (same bytes ⇒ same hash ⇒ the update applies).
pub fn rederive_from_bytes(
    trie: &Path,
    bytes: &[u8],
    item: &ItemParams,
) -> std::io::Result<Vec<GlyphRecord>> {
    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("pick: failed to load engine trie");
    eng.load_item(bytes, item).expect("pick: engine re-run failed");
    Ok(eng.read_back().records)
}

thread_local! {
    /// ONE Engine + trie per thread, keyed by trie path, held for the
    /// thread's lifetime — the hot-path re-deriver. `Engine::new` + trie
    /// parse is ~120 ms FIXED; the record walk is microseconds. thread_local
    /// because Engine is !Send AND because the cache must OUTLIVE SCENES:
    /// the live loop (seam.md, P1) rebuilds a scene per edit, so a
    /// scene-lifetime cache never amortizes. The engine handle is built for
    /// exactly this reuse — ffi.mojo resets the arena at every load_item
    /// ("reuse the arena across loads"). Render thread only.
    static REDERIVER: std::cell::RefCell<Option<(PathBuf, Engine)>> =
        const { std::cell::RefCell::new(None) };
}

/// The CACHED re-derivation: same results as [`rederive_from_bytes`], one
/// engine amortized across every call on this thread. A trie-path change
/// (a different checkout/engine build) swaps the cached engine; nothing else
/// can invalidate it — the fold is a pure function of (bytes, params).
pub fn rederive_cached(
    trie: &Path,
    bytes: &[u8],
    item: &ItemParams,
) -> std::io::Result<Vec<GlyphRecord>> {
    REDERIVER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let stale = slot.as_ref().is_some_and(|(t, _)| t != trie);
        if stale {
            *slot = None;
        }
        let (_, eng) = slot.get_or_insert_with(|| {
            let mut eng = Engine::new();
            eng.load_trie_file(trie).expect("pick: failed to load engine trie");
            (trie.to_path_buf(), eng)
        });
        eng.load_item(bytes, item).expect("pick: engine re-run failed");
        Ok(eng.read_back().records)
    })
}

/// The result of folding one file's record stream: the kept records (rows
/// renumbered, y shifted up over every fold's span), what was dropped, and
/// the extents RECOMPUTED from the kept records — the engine's page/ink
/// lanes are documented as max/min over records, so removing records and
/// recomputing is exact, not approximate.
pub struct Folded {
    pub records: Vec<GlyphRecord>,
    pub dropped: usize,
    pub page: PageExtent,
    pub ink: InkExtent,
}

/// P2a — the fold compaction, PURE: drop the records on folded lines,
/// renumber rows, and shift everything below up by each fold's own span.
///
/// The shift is MEASURED from the stream, not computed from a pitch: the
/// vertical pitch a fold occupied is `y(first folded record) − y(first kept
/// record after the fold)`. That handles Down-mode's multi-row wrapped
/// lines without assuming y is row-linear, and needs no ItemParams.
/// LIMITATION (v1, deliberate): a fold whose lines contain NO records at
/// all (only blank lines) frees pitch this cannot see — nothing shifts for
/// it. Folding function bodies never hits this; folding blank runs does.
///
/// `folds` are NORMALIZED, non-overlapping, ascending LINE ranges (the
/// output of [`crate::seam::normalized_fold_lines`]); `lines` is the line
/// index of each record, parallel to `records` (derived from the leader
/// walk by the caller).
pub fn compact_folds(
    records: &[GlyphRecord],
    folds: &[std::ops::Range<u32>],
    lines: &[u32],
) -> Folded {
    debug_assert_eq!(records.len(), lines.len(), "record/line index parallel");
    let folded_line = |line: u32| folds.iter().any(|f| f.contains(&line));

    let mut kept: Vec<GlyphRecord> = Vec::with_capacity(records.len());
    let mut dropped = 0usize;
    // Accumulated shift, established lazily when the stream passes OUT of a
    // fold (the first kept record after it names the fold's own pitch).
    let mut y_shift = 0.0f32;
    let mut rows_hidden = 0u32;
    let mut in_fold = false;
    let mut fold_first_y = 0.0f32;
    let mut fold_first_row = 0u32;
    let mut fold_last_row = 0u32;

    let mut page = PageExtent { right: 0.0, bottom: 0.0, z_min: 0.0, z_max: 0.0 };
    let mut ink_min = [f32::INFINITY; 3];
    let mut ink_max = [f32::NEG_INFINITY; 3];
    let mut first_kept = true;

    for (r, &line) in records.iter().zip(lines) {
        if folded_line(line) {
            dropped += 1;
            if !in_fold {
                in_fold = true;
                fold_first_y = r.y();
                fold_first_row = r.row();
            }
            fold_last_row = r.row();
            continue;
        }
        if in_fold {
            // Leaving the fold: this record's ORIGINAL y is the first kept
            // y below it — the fold's span is what separates the two.
            y_shift += fold_first_y - r.y();
            rows_hidden += fold_last_row - fold_first_row + 1;
            in_fold = false;
        }
        let mut r = *r;
        if y_shift != 0.0 {
            let [x, y, z, adv, h] = r.measures;
            r.measures = [x, y + y_shift, z, adv, h];
        }
        if rows_hidden > 0 {
            r.counts[1] = r.row() - rows_hidden;
        }
        kept.push(r);

        // Extents over the kept stream — the engine's documented formulas.
        page.right = page.right.max(r.x() + r.advance());
        page.bottom = if first_kept { r.y() } else { page.bottom.min(r.y()) };
        page.z_min = if first_kept { r.z() } else { page.z_min.min(r.z()) };
        page.z_max = if first_kept { r.z() } else { page.z_max.max(r.z()) };
        ink_min[0] = ink_min[0].min(r.x());
        ink_max[0] = ink_max[0].max(r.x() + r.advance());
        ink_min[1] = ink_min[1].min(r.y() - r.height() * 0.5);
        ink_max[1] = ink_max[1].max(r.y() + r.height() * 0.5);
        ink_min[2] = ink_min[2].min(r.z());
        ink_max[2] = ink_max[2].max(r.z());
        first_kept = false;
    }
    Folded {
        records: kept,
        dropped,
        page,
        ink: InkExtent { min: ink_min, max: ink_max },
    }
}

/// Byte offsets of every line start (0, then one past each `\n`). The
/// fold-resolving coordinate table — ascending by construction. Pub: the
/// live loop resolves envelope folds against owned content with it.
pub fn line_starts_of(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1),
    );
    starts
}

/// The line index containing `byte` (the LAST start at/before it).
fn line_of_byte(byte: usize, line_starts: &[usize]) -> u32 {
    match line_starts.binary_search(&byte) {
        Ok(i) => i as u32,
        Err(0) => 0,
        Err(i) => (i - 1) as u32,
    }
}

impl RepoLoad {
    /// Convert into the renderer's staged form. `focus` selects one file
    /// (first rel-path containing the substring) for the camera to frame.
    pub fn into_staged(self, focus: Option<&str>, slot_ink: &[Option<[f32; 4]>]) -> StagedText {
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
        let segments: Vec<crate::glyph_scene::SegCull> = self
            .files
            .iter()
            .map(|v| {
                let insts = &self.instances[v.slot_base..v.slot_base + v.slot_count];
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
                    tint: crate::glyph_scene::seg_tint(insts, v.width, v.height, slot_ink),
                }
            })
            .collect();
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
            glyphs_emitted: self.instances.len(),
            codepoints_decoded: self.stats.records,
            missing_or_bitmap: self.stats.blanks,
            segments,
            instances: self.instances,
            groups: self.groups,
            bounds_min: self.bounds_min,
            bounds_max: self.bounds_max,
            focus_bounds,
            pick: Some(crate::glyph_scene::PickContext {
                root: self.root,
                trie: self.trie,
                files: pick_files,
                // Envelope-owned content is the CALLER's to inject (it owns
                // the bytes); disk scenes re-derive from root.
                content: None,
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
             | backend: mojo-cpu/{}{}",
            s.records,
            s.instances,
            s.blanks,
            match s.strategy {
                Strategy::Batched => "batched",
                Strategy::PerItem => "per-item",
                Strategy::Direct => "direct",
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

#[cfg(test)]
mod tests {
    use super::*;

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
