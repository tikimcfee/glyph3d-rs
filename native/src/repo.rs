//! Stage E2 — repository-scale loading.
//!
//! Walks a repository (source-extension whitelist; VCS/build/dependency dirs
//! skipped), runs the Mojo engine over every file — either one `load_item`
//! call per file (naive) or one batched `load_items` call over a concatenated
//! blob — and stages the records into ONE glyph arena. Each file is a GROUP
//! (group_id == file index) placed on a 2D grid of code pages; files are
//! views {record_base/count, slot_base/count} into the shared arena — the
//! web's MegaGlyphField architecture (one arena, files as views).
//!
//! Correctness guards:
//! - only valid UTF-8 files reach the engine (valid UTF-8 cannot decode a
//!   codepoint past the trie's 4352-entry block index — the engine's decode
//!   assumes well-formed leads, see Stage E1 report);
//! - the 10 MB per-file cap also keeps every item far under the engine's
//!   per-item 2^24-byte ordinal wall (engine-local README, ordinal_invariant).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engine::{Engine, GlyphRecord, ItemParams};
use crate::glyph_scene::{GlyphInstance, GroupRow};
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

/// Packed RGBA8 of palette::DEFAULT ([212,212,212], alpha 255) — the fallback
/// when a record has no color entry (should not happen; belts-and-braces).
const DEFAULT_COLOR_PACKED: u32 = 0xFF_D4D4D4;

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
        has_page: true,
        page_rows: p.page_rows,
        page_cols: 0,
        scroll_rows: 0,
        pages_wide: wide,
        page_gap_x: p.page_gap_x,
        band_stride_y: page_h + p.band_gap_y,
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
    /// Group offset assigned by the grid layout.
    pub offset: [f32; 3],
    /// Stage G: the exact engine params this file was laid out with. A pick
    /// re-runs the engine with THESE params, so the re-derived geometry is
    /// bit-identical to what was staged.
    pub item: ItemParams,
}

pub struct LoadStats {
    pub walk: Duration,
    pub engine: Duration,
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
    pub batch: bool,
    pub verified: bool,
}

pub struct RepoLoad {
    pub instances: Vec<GlyphInstance>,
    pub groups: Vec<GroupRow>,
    pub files: Vec<FileView>,
    pub bounds_min: [f32; 2],
    pub bounds_max: [f32; 2],
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

/// Shared context threaded through every `stage_one` call: layout params,
/// the arena being appended to, and the dropped-blank counter.
struct StageCtx<'a> {
    params: &'a RepoParams,
    instances: &'a mut Vec<GlyphInstance>,
    blanks: &'a mut usize,
}

/// Stage one file's engine records into the arena (blanks/missing dropped —
/// their advance is baked into the survivors' X), returning its FileView.
fn stage_one(
    f: &RepoFile,
    records: &[GlyphRecord],
    group_id: u32,
    item: ItemParams,
    cx: &mut StageCtx<'_>,
) -> FileView {
    let params = cx.params;
    let colors = text::colorize_leaders(&f.bytes);
    debug_assert_eq!(
        colors.len(),
        records.len(),
        "color/record count mismatch on {}",
        f.rel_path
    );
    let slot_base = cx.instances.len();
    let mut max_x: f32 = 0.0;
    let mut min_y: f32 = 0.0;
    let mut blank = 0usize;
    for (i, r) in records.iter().enumerate() {
        let right = r.x() + r.advance();
        if right > max_x {
            max_x = right;
        }
        if r.y() < min_y {
            min_y = r.y();
        }
        if r.glyph_id() == 0 {
            blank += 1;
            continue;
        }
        cx.instances.push(GlyphInstance {
            pos: [r.x(), r.y(), r.z()],
            glyph_id: r.glyph_id(),
            row: r.row(),
            col: r.col(),
            color: colors.get(i).copied().unwrap_or(DEFAULT_COLOR_PACKED),
            group_id,
            advance: r.advance(),
            height: r.height(),
            flags: 0, // the wire record carries no flags; the shader reads mode from the glyphmap
            _pad: 0,
        });
    }
    *cx.blanks += blank;
    FileView {
        rel_path: f.rel_path.clone(),
        dir: f.dir.clone(),
        group_id,
        record_count: records.len(),
        slot_base,
        slot_count: cx.instances.len() - slot_base,
        width: max_x,
        // Paginated footprint: glyph centers run from y=0 down to min_y;
        // one line pitch of margin covers the bottom row's descenders.
        height: -min_y + params.line_height as f32,
        offset: [0.0; 3],
        item,
    }
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
) -> (Vec<GroupRow>, [f32; 2], [f32; 2]) {
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
    for (v, off) in views.iter_mut().zip(offsets.iter()) {
        v.offset = *off;
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
        [0.0, min_y],
        [max_x.max(1.0), params.line_height as f32],
    )
}

/// Whole-repo load: walk → engine → stage → grid layout. `batch` selects the
/// batched FFI (one engine call for the whole corpus) over the per-file loop;
/// `verify` runs BOTH and diffs every record bit-exact (the standing gate).
pub fn load_repo(
    root: &Path,
    trie: &Path,
    params: &RepoParams,
    batch: bool,
    verify: bool,
) -> RepoLoad {
    let t0 = Instant::now();
    let walk = walk_repo(root);
    let walk_dur = t0.elapsed();

    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("failed to load engine trie");
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

    let mut instances: Vec<GlyphInstance> = Vec::new();
    let mut views: Vec<FileView> = Vec::with_capacity(walk.files.len());
    let mut engine_dur = Duration::ZERO;
    let mut stage_dur = Duration::ZERO;
    let mut total_records = 0usize;
    let mut total_blanks = 0usize;
    // With --repo-verify the selected path's full record stream is kept so the
    // OTHER path can be diffed against it bit-exact afterwards.
    let mut kept_stream: Option<Vec<GlyphRecord>> = verify.then(Vec::new);

    if batch {
        // One concatenated blob; item boundaries are prefix sums (contiguous,
        // ascending — the pipeline's documented requirement).
        let t = Instant::now();
        let mut blob = Vec::with_capacity(walk.total_bytes);
        let mut descs = Vec::with_capacity(walk.files.len());
        for (f, fp) in walk.files.iter().zip(file_params.iter()) {
            descs.push((blob.len() as u64, f.bytes.len() as u64, *fp));
            blob.extend_from_slice(&f.bytes);
        }
        let counts = eng
            .load_items(&blob, &descs)
            .expect("engine load_items failed");
        let all = eng.records();
        engine_dur = t.elapsed();
        assert_eq!(
            counts.iter().sum::<u64>() as usize,
            all.len(),
            "batch per-item counts do not sum to the record count"
        );
        let t = Instant::now();
        let mut cx = StageCtx {
            params,
            instances: &mut instances,
            blanks: &mut total_blanks,
        };
        let mut rec_base = 0usize;
        for (fi, f) in walk.files.iter().enumerate() {
            let n = counts[fi] as usize;
            let records = &all[rec_base..rec_base + n];
            views.push(stage_one(f, records, fi as u32, file_params[fi], &mut cx));
            rec_base += n;
            total_records += n;
        }
        stage_dur = t.elapsed();
        if let Some(s) = &mut kept_stream {
            *s = all;
        }
    } else {
        let mut cx = StageCtx {
            params,
            instances: &mut instances,
            blanks: &mut total_blanks,
        };
        for (fi, f) in walk.files.iter().enumerate() {
            let t = Instant::now();
            eng.load_item(&f.bytes, &file_params[fi])
                .expect("engine load_item failed");
            let records = eng.records();
            engine_dur += t.elapsed();
            let t = Instant::now();
            views.push(stage_one(f, &records, fi as u32, file_params[fi], &mut cx));
            stage_dur += t.elapsed();
            total_records += records.len();
            if let Some(s) = &mut kept_stream {
                s.extend_from_slice(&records);
            }
        }
    }

    let mut verified = false;
    if verify {
        // Cross-check: run the OTHER engine path and diff every record
        // bit-exact (same discipline as --engine-check).
        let t = Instant::now();
        let alt = run_engine_path(&walk, &file_params, trie, !batch);
        let cur = kept_stream.take().expect("verify kept the selected stream");
        let mut bad = 0usize;
        let mut first_diff = String::new();
        if alt.len() != cur.len() {
            first_diff = format!("record count: {} vs {}", cur.len(), alt.len());
            bad = 1;
        } else {
            for (i, (a, b)) in cur.iter().zip(alt.iter()).enumerate() {
                let same = a.counts == b.counts
                    && a
                        .measures
                        .iter()
                        .zip(b.measures.iter())
                        .all(|(x, y)| x.to_bits() == y.to_bits());
                if !same {
                    bad += 1;
                    if bad <= 5 {
                        first_diff.push_str(&format!(
                            "\n  rec[{i}]: selected {:?} vs other {:?}",
                            a, b
                        ));
                    }
                }
            }
        }
        if bad != 0 {
            panic!("repo-verify FAIL: {bad} record(s) differ{first_diff}");
        }
        engine_dur += t.elapsed(); // honest: verification time is engine time
        verified = true;
        println!(
            "repo-verify PASS: {} records bit-exact between {} and {} paths",
            cur.len(),
            if batch { "batch" } else { "naive" },
            if batch { "naive" } else { "batch" },
        );
    }

    let t = Instant::now();
    let (groups, bounds_min, bounds_max) = layout(&mut views, params);
    let layout_dur = t.elapsed();

    let stats = LoadStats {
        walk: walk_dur,
        engine: engine_dur,
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
        batch,
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
    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("pick: failed to load engine trie");
    eng.load_item(&bytes, item).expect("pick: engine re-run failed");
    Ok((eng.records(), bytes))
}

/// Run one engine path over the whole corpus and return the full record
/// stream (used by --repo-verify to diff batch vs naive).
fn run_engine_path(
    walk: &WalkResult,
    file_params: &[ItemParams],
    trie: &Path,
    batch: bool,
) -> Vec<GlyphRecord> {
    let mut eng = Engine::new();
    eng.load_trie_file(trie).expect("failed to load engine trie");
    if batch {
        let mut blob = Vec::with_capacity(walk.total_bytes);
        let mut descs = Vec::with_capacity(walk.files.len());
        for (f, fp) in walk.files.iter().zip(file_params.iter()) {
            descs.push((blob.len() as u64, f.bytes.len() as u64, *fp));
            blob.extend_from_slice(&f.bytes);
        }
        eng.load_items(&blob, &descs)
            .expect("engine load_items failed");
        eng.records()
    } else {
        let mut out = Vec::new();
        for (f, fp) in walk.files.iter().zip(file_params.iter()) {
            eng.load_item(&f.bytes, fp)
                .expect("engine load_item failed");
            out.extend_from_slice(&eng.records());
        }
        out
    }
}

impl RepoLoad {
    /// Convert into the renderer's staged form. `focus` selects one file
    /// (first rel-path containing the substring) for the camera to frame.
    pub fn into_staged(self, focus: Option<&str>) -> StagedText {
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
                    min: [v.offset[0] - 0.3, v.offset[1] - v.height - 0.5],
                    max: [v.offset[0] + v.width + 0.6, v.offset[1] + 0.75],
                    slot_base: v.slot_base as u32,
                    slot_count: v.slot_count as u32,
                    _pad: [0.0; 2],
                    tint: crate::glyph_scene::seg_tint(insts, v.width, v.height),
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
             | engine path: {}{}",
            s.records,
            s.instances,
            s.blanks,
            if s.batch { "batch" } else { "naive per-file" },
            if s.verified { " (verified bit-exact vs the other path)" } else { "" },
        );
        println!(
            "phases: walk {:.3}s | engine {:.3}s ({:.1} MB/s) | stage {:.3}s | layout {:.3}s \
             | total {:.3}s",
            s.walk.as_secs_f64(),
            s.engine.as_secs_f64(),
            mb / s.engine.as_secs_f64().max(1e-9),
            s.stage.as_secs_f64(),
            s.layout.as_secs_f64(),
            (s.walk + s.engine + s.stage + s.layout).as_secs_f64(),
        );
    }
}
