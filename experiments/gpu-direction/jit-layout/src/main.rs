//! jit-layout — headless measurement of "lay out only the visible lines, on the GPU",
//! at Linux-kernel scale.
//!
//! The status quo lays out a whole repo up front into one slot per glyph
//! (Derived mode: 20 B/slot). The hypothesis measured here: keep the source
//! BYTES (1 B/byte) plus a compact per-line table resident on the GPU, and
//! each frame run a compute pass over just the visible lines (one invocation
//! per SEGMENT, a serial fold over its bytes) into a transient slot buffer,
//! then draw from that. Long lines are cut into seeded segments so no single
//! invocation runs for 64 KiB.
//!
//! What this does NOT model: emoji/ZWJ sequences, the real atlas trie, Slug
//! coverage in the fragment stage, pagination, the renderer's paint rules
//! beyond a `//` comment flag, a real camera (views are synthetic).
//!
//! Every timing repeats `--repeat` times, interleaved across the measured
//! things, and reports min and median because the GPU and CPU are shared.
//!
//!     jit-layout <corpus-dir> [--walk renderer|all] [--threads T]
//!         [--view page|overview|worst|random] [--visible-lines N]
//!         [--segment-bytes S] [--repeat K] [--screenshot <png>] [--no-draw]

use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use wgpu::util::DeviceExt;

const SLOT_BYTES: u64 = 20;
const WRAP_COLS: u32 = 120;
const MAX_FILE_BYTES: u64 = 10 << 20;
const TARGET: (u32, u32) = (800, 500);
const MAX_BYTE_CHUNKS: usize = 4;
const GLYPH_W: f32 = 0.6;
const ROW_H: f32 = 1.2;
const COLOR_CODE: u32 = 0xFFD8E2E8;
const COLOR_COMMENT: u32 = 0xFF5AB06A;
const PASS1_THREAD_SET: [usize; 5] = [1, 4, 8, 16, 32];
const PAGE_LINES: usize = 60;
const OVERVIEW_FILES: usize = 2000;
const OVERVIEW_LINES_PER_FILE: usize = 50;
const WORST_LINES: usize = 1000;

// The renderer's walk rules (native/src/repo/walk.rs), copied so the numbers compare.
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

/// The Derived slot, 20 B.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct Slot { x: f32, row: u32, glyph_and_wrap: u32, color: u32, item: u32 }
/// The line table, 8 B/line: where the line starts and how many glyphs it has
/// (the per-frame slot prefix needs the count). Its length is
/// `min(next.byte_start, item end) - byte_start - 1`; row and item come from the
/// item table. A sentinel entry closes the last line.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct Line { byte_start: u32, glyphs: u32 }
/// The item (file) table, 32 B/item. `byte_len` includes the terminating '\n'.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, longest_line: u32, _pad: [u32; 2] }
/// One visible segment, 32 B, per frame. A whole line has `col_seed = 0, x_seed = 0, state = 0`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params { segment_count: u32, wrap_cols: u32, chunk_shift: u32, chunk_mask: u32 }
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Cam { scale: [f32; 2], offset: [f32; 2], min_px: [f32; 2], tint: u32, _pad: u32 }

// ---------------------------------------------------------------- CLI

#[derive(Clone, Copy, PartialEq)]
enum WalkMode { Renderer, All }

struct Args { corpus: PathBuf, walk: WalkMode, threads: usize, view: Option<String>, visible_lines: usize, segment_bytes: usize, repeat: usize, screenshot: Option<PathBuf>, draw: bool }

const USAGE: &str = "usage: jit-layout <corpus-dir> [--walk renderer|all] [--threads T] [--view page|overview|worst|random] [--visible-lines N] [--segment-bytes S] [--repeat K] [--screenshot <png>] [--no-draw]";

fn parse_args() -> Args {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let mut a = Args { corpus: PathBuf::new(), walk: WalkMode::Renderer, threads, view: None, visible_lines: 100_000, segment_bytes: 2048, repeat: 5, screenshot: None, draw: true };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value"));
        let mut num = |name: &str| val(name).parse::<usize>().unwrap_or_else(|e| panic!("{name}: {e}"));
        match arg.as_str() {
            "--walk" => a.walk = match val("--walk").as_str() { "renderer" => WalkMode::Renderer, "all" => WalkMode::All, other => panic!("--walk: {other}") },
            "--threads" => a.threads = num("--threads"),
            "--view" => a.view = Some(val("--view")),
            "--visible-lines" => a.visible_lines = num("--visible-lines"),
            "--segment-bytes" => a.segment_bytes = num("--segment-bytes"),
            "--repeat" => a.repeat = num("--repeat"),
            "--screenshot" => a.screenshot = Some(PathBuf::from(val("--screenshot"))),
            "--no-draw" => a.draw = false,
            _ if a.corpus.as_os_str().is_empty() => a.corpus = PathBuf::from(arg),
            _ => panic!("unexpected argument {arg}\n{USAGE}"),
        }
    }
    assert!(!a.corpus.as_os_str().is_empty(), "{USAGE}");
    assert!(a.repeat >= 1 && a.visible_lines >= 1 && a.threads >= 1 && a.segment_bytes >= 16);
    if let Some(v) = &a.view { assert!(["page", "overview", "worst", "random"].contains(&v.as_str()), "--view: {v}") }
    assert!(a.screenshot.is_none() || a.view.is_some(), "--screenshot needs --view");
    assert!(a.screenshot.is_none() || a.draw, "--screenshot needs the draw pass");
    a
}

fn loadavg() -> String { std::fs::read_to_string("/proc/loadavg").map(|s| s.trim().to_string()).unwrap_or_else(|_| "n/a".into()) }

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1e3 }

/// (min, median) of a sample set.
fn stats(v: &[f64]) -> (f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2])
}

fn mb(b: u64) -> f64 { b as f64 / 1e6 }

/// Contiguous index ranges over `weights`, `n` of them, balanced by weight (fewer when there are fewer items).
fn partition(weights: &[usize], n: usize) -> Vec<std::ops::Range<usize>> {
    let total: usize = weights.iter().sum();
    let target = total / n.max(1) + 1;
    let (mut out, mut start, mut acc) = (Vec::new(), 0usize, 0usize);
    for (i, w) in weights.iter().enumerate() {
        acc += w;
        if acc >= target && out.len() + 1 < n {
            out.push(start..i + 1);
            start = i + 1;
            acc = 0;
        }
    }
    if start < weights.len() { out.push(start..weights.len()) }
    out
}

// ---------------------------------------------------------------- walk

/// One kept file inside the concatenated byte buffer. `byte_len` includes the terminating '\n'
/// (appended when the file lacks one, so every line is newline-terminated).
struct FileSpan { rel: String, byte_start: usize, byte_len: usize }

struct WalkStats { enumerate_ms: f64, read_ms: f64, concat_ms: f64, candidates: usize, skipped_large: usize, skipped_non_utf8: usize, read_errors: usize, pad_bytes: usize }

struct Walked { bytes: Vec<u8>, files: Vec<FileSpan>, stats: WalkStats }

/// Directory enumeration exactly as the renderer does it: serial, dotfiles skipped
/// (except `.github`), `SKIP_DIRS`, extension filter, sorted by relative path.
fn enumerate(root: &Path, mode: WalkMode) -> Vec<(String, PathBuf)> {
    let mut cands = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let name_os = entry.file_name();
            let Some(name) = name_os.to_str() else { continue };
            if mode == WalkMode::Renderer && name.starts_with('.') && name != ".github" { continue }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if mode == WalkMode::Renderer && SKIP_DIRS.contains(&name) { continue }
                stack.push(entry.path());
            } else if ft.is_file() {
                if mode == WalkMode::Renderer {
                    let Some((_, ext)) = name.rsplit_once('.') else { continue };
                    if !SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) { continue }
                }
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                cands.push((rel, path));
            }
        }
    }
    cands.sort_by(|a, b| a.0.cmp(&b.0));
    cands
}

enum ReadResult { File(Vec<u8>), Large, NonUtf8, Error }

/// Read every candidate on `threads` threads (a shared counter hands out indices).
fn read_all(cands: &[(String, PathBuf)], threads: usize) -> Vec<ReadResult> {
    let next = AtomicUsize::new(0);
    let mut per_thread: Vec<Vec<(usize, ReadResult)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| {
            let mut mine = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= cands.len() { break }
                let path = &cands[i].1;
                let r = match std::fs::metadata(path) {
                    Err(_) => ReadResult::Error,
                    Ok(m) if m.len() > MAX_FILE_BYTES => ReadResult::Large,
                    Ok(_) => match std::fs::read(path) {
                        Err(_) => ReadResult::Error,
                        Ok(b) if std::str::from_utf8(&b).is_err() => ReadResult::NonUtf8,
                        Ok(b) => ReadResult::File(b),
                    },
                };
                mine.push((i, r));
            }
            mine
        })).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut out: Vec<Option<ReadResult>> = (0..cands.len()).map(|_| None).collect();
    for v in per_thread.drain(..) { for (i, r) in v { out[i] = Some(r) } }
    out.into_iter().map(|r| r.unwrap()).collect()
}

/// Walk + read + concatenate. Files are laid out in walk order; when the byte
/// buffer must be split into GPU chunks of `chunk_size`, a file that would
/// straddle a boundary is pushed past it (zero padding), so a segment always
/// lives in one chunk.
fn walk(root: &Path, mode: WalkMode, threads: usize, chunk_size: usize) -> Walked {
    let t = Instant::now();
    let cands = enumerate(root, mode);
    let enumerate_ms = ms(t);
    let t = Instant::now();
    let results = read_all(&cands, threads);
    let read_ms = ms(t);

    let t = Instant::now();
    let (mut skipped_large, mut skipped_non_utf8, mut read_errors, mut pad_bytes) = (0, 0, 0, 0usize);
    let mut files = Vec::new();
    let mut datas: Vec<&[u8]> = Vec::new();
    let mut cur = 0usize;
    for ((rel, _), r) in cands.iter().zip(&results) {
        match r {
            ReadResult::Large => skipped_large += 1,
            ReadResult::NonUtf8 => skipped_non_utf8 += 1,
            ReadResult::Error => read_errors += 1,
            ReadResult::File(b) => {
                let size = b.len() + usize::from(b.last() != Some(&b'\n'));
                let last = cur + size.max(1) - 1;
                if cur / chunk_size != last / chunk_size {
                    let aligned = last / chunk_size * chunk_size;
                    pad_bytes += aligned - cur;
                    cur = aligned;
                }
                files.push(FileSpan { rel: rel.clone(), byte_start: cur, byte_len: size });
                datas.push(b);
                cur += size;
            }
        }
    }
    let total = cur.next_multiple_of(4).max(4);
    let mut bytes = vec![0u8; total];
    let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
    std::thread::scope(|s| {
        let mut rest = &mut bytes[..];
        let mut done = 0usize;
        for g in groups {
            let (first, last) = (&files[g.start], &files[g.end - 1]);
            let (skip, end) = (first.byte_start - done, last.byte_start + last.byte_len);
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(end - done);
            rest = tail;
            done = end;
            let (files, datas) = (&files[g.clone()], &datas[g]);
            let base = first.byte_start;
            s.spawn(move || {
                let _ = skip;
                for (f, d) in files.iter().zip(datas) {
                    let dst = &mut mine[f.byte_start - base..f.byte_start - base + f.byte_len];
                    dst[..d.len()].copy_from_slice(d);
                    if d.len() < f.byte_len { dst[d.len()] = b'\n' }
                }
            });
        }
    });
    let concat_ms = ms(t);
    Walked { bytes, files, stats: WalkStats { enumerate_ms, read_ms, concat_ms, candidates: cands.len(), skipped_large, skipped_non_utf8, read_errors, pad_bytes } }
}

// ---------------------------------------------------------------- the fold (CPU twin of layout.wgsl)

fn advance_table() -> [f32; 256] {
    let mut t = [0.6f32; 256];
    for b in *b" il.,'" { t[b as usize] = 0.3 }
    for b in *b"mwMW" { t[b as usize] = 0.9 }
    t[b'\t' as usize] = 2.4;
    t
}

/// The fold's running state: x since the last wrap-unit boundary, the column, the paint flags.
#[derive(Clone, Copy)]
struct Fold { x: f32, col: u32, in_comment: bool, prev_slash: bool }

impl Fold {
    fn new() -> Fold { Fold { x: 0.0, col: 0, in_comment: false, prev_slash: false } }
    fn state(&self) -> u32 { u32::from(self.in_comment) | u32::from(self.prev_slash) << 1 }
    /// Decode the glyph starting at `bytes[i]`: (glyph id, advance, byte length). Updates the paint flags.
    fn decode(&mut self, bytes: &[u8], i: usize, adv: &[f32; 256]) -> (u32, f32, usize) {
        let b = bytes[i] as u32;
        if b < 0x80 {
            if b == 0x2F { self.in_comment |= self.prev_slash; self.prev_slash = true } else { self.prev_slash = false }
            (b, adv[b as usize], 1)
        } else {
            let (l, mut cp) = if b >= 0xF0 { (4, b & 0x07) } else if b >= 0xE0 { (3, b & 0x0F) } else { (2, b & 0x1F) };
            for j in 1..l { cp = (cp << 6) | (bytes[i + j] as u32 & 0x3F) }
            self.prev_slash = false;
            (cp & 0xFFFF, 1.0, l)
        }
    }
    /// The slot for the decoded glyph, then advance — the kernel's loop body.
    fn emit(&mut self, glyph: u32, a: f32, row: u32, item: u32) -> Slot {
        if self.col % WRAP_COLS == 0 { self.x = 0.0 }
        let s = Slot { x: self.x, row, glyph_and_wrap: glyph | ((self.col / WRAP_COLS) << 16), color: if self.in_comment { COLOR_COMMENT } else { COLOR_CODE }, item };
        self.x += a;
        self.col += 1;
        s
    }
}

/// Lay out one WHOLE line from column 0 — the reference every segmented fold is held to.
fn layout_line(bytes: &[u8], byte_start: usize, byte_len: usize, row: u32, item: u32, adv: &[f32; 256], out: &mut [Slot]) {
    let (mut f, mut i, mut k) = (Fold::new(), byte_start, 0usize);
    while i < byte_start + byte_len {
        let (g, a, l) = f.decode(bytes, i, adv);
        out[k] = f.emit(g, a, row, item);
        i += l;
        k += 1;
    }
}

/// A continuation's seeds: where it starts in the line, and the fold state there.
#[derive(Clone, Copy, Debug)]
struct SegSeed { byte_off: u32, col: u32, x: f32, state: u32 }

/// Pass 1's serial walk of a long line: cut into segments of at least `seg_bytes`,
/// each cut landing only BEFORE AN ASCII BYTE (never inside a codepoint), and
/// snapshot the fold at each cut: the column, the running f32 x (0 when the cut
/// falls on a wrap-unit boundary, as the kernel resets there), the paint flags.
/// Returns the line's glyph count and the seeds of every segment after the first.
fn seed_long_line(line: &[u8], seg_bytes: usize, adv: &[f32; 256]) -> (u32, Vec<SegSeed>) {
    let (mut f, mut i, mut seg_start, mut seeds) = (Fold::new(), 0usize, 0usize, Vec::new());
    while i < line.len() {
        if i - seg_start >= seg_bytes && line[i] < 0x80 {
            seeds.push(SegSeed { byte_off: i as u32, col: f.col, x: if f.col % WRAP_COLS == 0 { 0.0 } else { f.x }, state: f.state() });
            seg_start = i;
        }
        let (g, a, l) = f.decode(line, i, adv);
        f.emit(g, a, 0, 0);
        i += l;
    }
    (f.col, seeds)
}

// ---------------------------------------------------------------- Pass 1

struct Pass1 { lines: Vec<Line>, items: Vec<Item>, seeds: HashMap<u32, Vec<SegSeed>>, total_glyphs: u64, wall_ms: f64 }

struct Part { lines: Vec<Line>, items: Vec<Item>, seeds: Vec<(usize, u32, Vec<SegSeed>)>, glyphs: u64 }

fn continuation_bytes(s: &[u8]) -> usize { s.iter().map(|&b| usize::from((b & 0xC0) == 0x80)).sum() }

/// The line table, item table and long-line seeds, built per file on `threads`
/// threads (files are independent) and assembled in parallel.
fn pass1(bytes: &[u8], files: &[FileSpan], threads: usize, seg_bytes: usize, adv: &[f32; 256]) -> Pass1 {
    let t = Instant::now();
    let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
    let parts: Vec<Part> = std::thread::scope(|s| {
        let handles: Vec<_> = groups.iter().map(|g| {
            let files = &files[g.clone()];
            s.spawn(move || {
                let est = files.iter().map(|f| f.byte_len).sum::<usize>() / 24;
                let mut p = Part { lines: Vec::with_capacity(est), items: Vec::with_capacity(files.len()), seeds: Vec::new(), glyphs: 0 };
                for (fi, f) in files.iter().enumerate() {
                    let data = &bytes[f.byte_start..f.byte_start + f.byte_len];
                    let first_line = p.lines.len();
                    let (mut line_start, mut max_len, mut longest) = (0usize, 0u32, 0u32);
                    for nl in memchr::memchr_iter(b'\n', data) {
                        let line = &data[line_start..nl];
                        let glyphs = if line.len() > seg_bytes {
                            let (g, seeds) = seed_long_line(line, seg_bytes, adv);
                            if !seeds.is_empty() { p.seeds.push((fi, (p.lines.len() - first_line) as u32, seeds)) }
                            g
                        } else {
                            (line.len() - continuation_bytes(line)) as u32
                        };
                        if line.len() as u32 > max_len { max_len = line.len() as u32; longest = (p.lines.len() - first_line) as u32 }
                        p.glyphs += glyphs as u64;
                        p.lines.push(Line { byte_start: (f.byte_start + line_start) as u32, glyphs });
                        line_start = nl + 1;
                    }
                    p.items.push(Item { first_line: first_line as u32, line_count: (p.lines.len() - first_line) as u32, byte_start: f.byte_start as u32, byte_len: f.byte_len as u32, max_len, longest_line: longest, _pad: [0; 2] });
                }
                p
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Assemble: fix up line indices with each part's base, copy the line tables in parallel.
    let total: usize = parts.iter().map(|p| p.lines.len()).sum();
    let mut lines = vec![Line::zeroed(); total + 1];
    let mut items = Vec::with_capacity(files.len());
    let mut seeds = HashMap::new();
    let mut total_glyphs = 0u64;
    std::thread::scope(|s| {
        let mut rest = &mut lines[..total];
        let mut base = 0usize;
        for p in &parts {
            let item_base = items.len();
            for it in &p.items { items.push(Item { first_line: it.first_line + base as u32, longest_line: it.first_line + base as u32 + it.longest_line, ..*it }) }
            for (fi, row, sd) in &p.seeds { seeds.insert(items[item_base + fi].first_line + row, sd.clone()); }
            total_glyphs += p.glyphs;
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(p.lines.len());
            rest = tail;
            base += p.lines.len();
            s.spawn(move || mine.copy_from_slice(&p.lines));
        }
    });
    let end = files.last().map(|f| f.byte_start + f.byte_len).unwrap_or(0);
    lines[total] = Line { byte_start: end as u32, glyphs: 0 };
    Pass1 { lines, items, seeds, total_glyphs, wall_ms: ms(t) }
}

// ---------------------------------------------------------------- corpus + visible lists

struct Corpus { bytes: Vec<u8>, files: Vec<FileSpan>, lines: Vec<Line>, items: Vec<Item>, seeds: HashMap<u32, Vec<SegSeed>>, total_glyphs: u64, segment_bytes: usize }

impl Corpus {
    fn line_count(&self) -> usize { self.lines.len() - 1 }
    fn item_of(&self, li: u32) -> usize { self.items.partition_point(|it| it.first_line <= li) - 1 }
    fn line_len(&self, li: u32, item: &Item) -> u32 {
        let next = self.lines[li as usize + 1].byte_start.min(item.byte_start + item.byte_len);
        next - self.lines[li as usize].byte_start - 1
    }
}

struct Visible { segs: Vec<Seg>, slots: usize, bytes_read: u64 }

/// The per-frame list: one segment per visible line, or several for a long line
/// (from its Pass-1 seeds), with the exclusive slot prefix as each segment's base.
fn visible_list(c: &Corpus, lines: &[u32]) -> Visible {
    let (mut segs, mut base, mut bytes_read) = (Vec::with_capacity(lines.len()), 0u32, 0u64);
    let mut cur = 0usize;
    for &li in lines {
        let it = &c.items[cur];
        if li < it.first_line || li >= it.first_line + it.line_count { cur = c.item_of(li) }
        let it = &c.items[cur];
        let (start, len, glyphs) = (c.lines[li as usize].byte_start, c.line_len(li, it), c.lines[li as usize].glyphs);
        bytes_read += len as u64;
        // Only a long line can have seeds; the length test keeps the hash lookup off the common path.
        let seeds = if len as usize > c.segment_bytes { c.seeds.get(&li) } else { None };
        match seeds {
            Some(seeds) => {
                let mut prev = SegSeed { byte_off: 0, col: 0, x: 0.0, state: 0 };
                for sd in seeds.iter().copied().chain(std::iter::once(SegSeed { byte_off: len, col: glyphs, x: 0.0, state: 0 })) {
                    segs.push(Seg { byte_start: start + prev.byte_off, byte_len: sd.byte_off - prev.byte_off, line_idx: li, item: cur as u32, slot_base: base + prev.col, col_seed: prev.col, x_seed: prev.x, state: prev.state });
                    prev = sd;
                }
            }
            None => segs.push(Seg { byte_start: start, byte_len: len, line_idx: li, item: cur as u32, slot_base: base, col_seed: 0, x_seed: 0.0, state: 0 }),
        }
        base += glyphs;
    }
    Visible { segs, slots: base as usize, bytes_read }
}

/// The CPU reference: every visible line folded WHOLE, single-threaded, from column 0.
fn cpu_reference(c: &Corpus, lines: &[u32], adv: &[f32; 256]) -> Vec<Slot> {
    let total: usize = lines.iter().map(|&li| c.lines[li as usize].glyphs as usize).sum();
    let mut out = vec![Slot::zeroed(); total];
    let mut k = 0usize;
    for &li in lines {
        let item = c.item_of(li);
        let it = &c.items[item];
        let n = c.lines[li as usize].glyphs as usize;
        layout_line(&c.bytes, c.lines[li as usize].byte_start as usize, c.line_len(li, it) as usize, li - it.first_line, item as u32, adv, &mut out[k..k + n]);
        k += n;
    }
    out
}

// ---------------------------------------------------------------- views

#[derive(Clone, Copy, PartialEq)]
enum Placement { Stack, Grid }

struct View { name: String, lines: Vec<u32>, placement: Placement, tint: bool, verify: bool }

/// Deterministic window offsets (an LCG; reproducible across runs, no crate).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: usize) -> usize { self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); ((self.0 >> 33) as usize) % n.max(1) }
}

/// 60 consecutive lines from the middle of the largest `.c` under `kernel/` (else the largest `.c`, else the largest file).
fn page_view(c: &Corpus) -> (View, usize) {
    let pick = |pred: &dyn Fn(&FileSpan) -> bool| c.files.iter().enumerate().filter(|(_, f)| pred(f)).max_by_key(|(_, f)| f.byte_len).map(|(i, _)| i);
    let item = pick(&|f| f.rel.starts_with("kernel/") && f.rel.ends_with(".c")).or_else(|| pick(&|f| f.rel.ends_with(".c"))).or_else(|| pick(&|_| true)).expect("empty corpus");
    let it = &c.items[item];
    let n = (it.line_count as usize).min(PAGE_LINES);
    let start = it.first_line as usize + (it.line_count as usize - n) / 2;
    (View { name: "page".into(), lines: (start as u32..(start + n) as u32).collect(), placement: Placement::Stack, tint: false, verify: false }, item)
}

/// The first 50 lines of every k-th file in walk order, 2,000 files.
fn overview_view(c: &Corpus) -> View {
    let k = (c.items.len() / OVERVIEW_FILES).max(1);
    let mut lines = Vec::new();
    for it in c.items.iter().step_by(k).take(OVERVIEW_FILES) {
        lines.extend(it.first_line..it.first_line + it.line_count.min(OVERVIEW_LINES_PER_FILE as u32));
    }
    View { name: "overview".into(), lines, placement: Placement::Grid, tint: true, verify: false }
}

/// The 1,000 longest lines (by bytes), longest first.
fn worst_view(c: &Corpus) -> View {
    use std::cmp::Reverse;
    let mut heap = std::collections::BinaryHeap::new();
    for (item, it) in c.items.iter().enumerate() {
        for li in it.first_line..it.first_line + it.line_count {
            let len = c.line_len(li, it);
            if heap.len() < WORST_LINES { heap.push(Reverse((len, li, item))) }
            else if let Some(&Reverse((min, ..))) = heap.peek() { if len > min { heap.pop(); heap.push(Reverse((len, li, item))) } }
        }
    }
    let mut top: Vec<(u32, u32, usize)> = heap.into_iter().map(|Reverse(x)| x).collect();
    top.sort_by(|a, b| b.cmp(a));
    println!("worst view: the {} longest lines; top 5:", top.len());
    for &(len, li, item) in top.iter().take(5) {
        let it = &c.items[item];
        println!("  {:>8} B  {}:{}  ({} glyphs)", len, c.files[item].rel, li - it.first_line + 1, c.lines[li as usize].glyphs);
    }
    View { name: "worst".into(), lines: top.iter().map(|&(_, li, _)| li).collect(), placement: Placement::Stack, tint: false, verify: true }
}

fn random_view(c: &Corpus, n: usize, rng: &mut Lcg, verify: bool) -> View {
    let n = n.min(c.line_count());
    let off = rng.next(c.line_count() - n + 1);
    View { name: format!("random N={n}"), lines: (off as u32..(off + n) as u32).collect(), placement: Placement::Stack, tint: true, verify }
}

fn plan_views(c: &Corpus, args: &Args) -> Vec<View> {
    let mut rng = Lcg(0x9E3779B97F4A7C15);
    match args.view.as_deref() {
        Some("page") => vec![page_view(c).0],
        Some("overview") => vec![overview_view(c)],
        Some("worst") => vec![worst_view(c)],
        Some("random") => vec![random_view(c, args.visible_lines, &mut rng, true)],
        _ => {
            let mut v = vec![page_view(c).0, overview_view(c), worst_view(c)];
            let mut sizes: Vec<usize> = [100_000usize, 1_000_000, args.visible_lines].iter().map(|&n| n.min(c.line_count())).collect();
            sizes.sort();
            sizes.dedup();
            for n in sizes { v.push(random_view(c, n, &mut rng, n == args.visible_lines)) }
            v
        }
    }
}

/// World origin per item for the draw: items stacked in a column (reading views)
/// or on a grid (overview). Indexed by GLOBAL item id; items outside the view are left at zero.
fn item_origins(c: &Corpus, view: &View) -> Vec<[f32; 2]> {
    let mut order: Vec<usize> = Vec::new();
    let mut rows: HashMap<usize, (u32, u32)> = HashMap::new();
    for &li in &view.lines {
        let item = c.item_of(li);
        let row = li - c.items[item].first_line;
        rows.entry(item).and_modify(|r| { r.0 = r.0.min(row); r.1 = r.1.max(row) }).or_insert_with(|| { order.push(item); (row, row) });
    }
    let mut origins = vec![[0f32; 2]; c.items.len()];
    match view.placement {
        Placement::Stack => {
            let mut cum = 0u32;
            for item in order {
                let (lo, hi) = rows[&item];
                origins[item] = [0.0, -(cum as f32) * ROW_H + lo as f32 * ROW_H];
                cum += hi - lo + 1;
            }
        }
        Placement::Grid => {
            let cell_h = order.iter().map(|i| rows[i].1 - rows[i].0 + 1).max().unwrap_or(1) as f32 * ROW_H + 6.0;
            let cell_w = WRAP_COLS as f32 * GLYPH_W + 6.0;
            let aspect = TARGET.0 as f32 / TARGET.1 as f32;
            let cols = ((order.len() as f32 * aspect * cell_h / cell_w).sqrt().ceil() as usize).max(1);
            for (k, item) in order.iter().enumerate() {
                let (lo, _) = rows[item];
                origins[*item] = [(k % cols) as f32 * cell_w, -((k / cols) as f32) * cell_h + lo as f32 * ROW_H];
            }
        }
    }
    origins
}

/// An orthographic camera fitted to the slots' world bounds (4% margin), aspect preserved.
fn fit_camera(slots: &[Slot], origins: &[[f32; 2]], tint: bool) -> Cam {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for s in slots {
        let o = origins[s.item as usize];
        let (x, y) = (o[0] + s.x, o[1] - s.row as f32 * ROW_H);
        x0 = x0.min(x); x1 = x1.max(x + GLYPH_W); y0 = y0.min(y); y1 = y1.max(y + ROW_H);
    }
    if slots.is_empty() { (x0, x1, y0, y1) = (0.0, 1.0, 0.0, 1.0) }
    let (w, h) = ((x1 - x0).max(1e-3), (y1 - y0).max(1e-3));
    let px_per_unit = (TARGET.0 as f32 * 0.96 / w).min(TARGET.1 as f32 * 0.96 / h);
    let scale = [px_per_unit * 2.0 / TARGET.0 as f32, px_per_unit * 2.0 / TARGET.1 as f32];
    let offset = [-(x0 + x1) * 0.5 * scale[0], -(y0 + y1) * 0.5 * scale[1]];
    Cam { scale, offset, min_px: [2.0 / TARGET.0 as f32, 2.0 / TARGET.1 as f32], tint: u32::from(tint), _pad: 0 }
}

// ---------------------------------------------------------------- GPU

struct Gpu { device: wgpu::Device, queue: wgpu::Queue, ts_period: f32, adapter_info: wgpu::AdapterInfo, adapter_limits: wgpu::Limits }

fn init_gpu() -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("no Vulkan adapter");
    assert!(adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY), "adapter lacks TIMESTAMP_QUERY");
    let al = adapter.limits();
    let required_limits = wgpu::Limits { max_storage_buffer_binding_size: al.max_storage_buffer_binding_size, max_buffer_size: al.max_buffer_size, ..wgpu::Limits::default() };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("jit-layout"),
        required_features: wgpu::Features::TIMESTAMP_QUERY,
        required_limits,
        ..Default::default()
    })).expect("request_device");
    let ts_period = queue.get_timestamp_period();
    Gpu { device, queue, ts_period, adapter_info: adapter.get_info(), adapter_limits: al }
}

impl Gpu {
    fn wait(&self) { self.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll"); }

    fn submit_wait(&self, cb: wgpu::CommandBuffer) -> f64 {
        let t = Instant::now();
        self.queue.submit([cb]);
        self.wait();
        ms(t)
    }

    fn buffer(&self, label: &str, size: u64, usage: wgpu::BufferUsages, mapped: bool) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(4), usage, mapped_at_creation: mapped })
    }

    fn init_buffer(&self, label: &str, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        if contents.is_empty() { return self.buffer(label, 4, usage, false) }
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage })
    }

    fn map_read(&self, buf: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        buf.slice(..size).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.wait();
        rx.recv().unwrap().expect("map_async");
        let v = buf.slice(..size).get_mapped_range().expect("mapped range").to_vec();
        buf.unmap();
        v
    }

    /// Copy `size` bytes out of `src` and hand them back (map_async + poll).
    fn read_back(&self, src: &wgpu::Buffer, size: u64) -> Vec<u8> {
        let dst = self.buffer("readback", size, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, false);
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &dst, 0, size);
        self.submit_wait(enc.finish());
        self.map_read(&dst, size)
    }

    fn storage_entry(binding: u32, read_only: bool, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
    fn uniform_entry(binding: u32, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
}

/// The resident set: the corpus bytes (in up to four chunks), the line table, the item table.
/// The line table is uploaded and held resident (it is the design's footprint) but this
/// kernel does not read it: the CPU builds the visible list from its own copy, and the
/// segments carry byte range and line index. A GPU-side visibility pass would be its reader.
struct Resident { chunks: Vec<wgpu::Buffer>, _lines: wgpu::Buffer, items: wgpu::Buffer }

fn upload_resident(gpu: &Gpu, c: &Corpus, chunk_size: usize) -> (Resident, f64, f64) {
    use wgpu::BufferUsages as U;
    let t = Instant::now();
    let chunks = c.bytes.chunks(chunk_size).map(|ch| gpu.init_buffer("bytes", ch, U::STORAGE)).collect();
    let lines = gpu.init_buffer("lines", bytemuck::cast_slice(&c.lines), U::STORAGE);
    let items = gpu.init_buffer("items", bytemuck::cast_slice(&c.items), U::STORAGE);
    let create_ms = ms(t);
    let submit_ms = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
    (Resident { chunks, _lines: lines, items }, create_ms, submit_ms)
}

/// Everything else: the per-frame buffers, both pipelines, the offscreen target, the query set.
struct Resources {
    segs_buf: wgpu::Buffer, slots_buf: wgpu::Buffer, params_buf: wgpu::Buffer, cam_buf: wgpu::Buffer, origins_buf: wgpu::Buffer,
    compute: wgpu::ComputePipeline, compute_bg: wgpu::BindGroup,
    render: wgpu::RenderPipeline, render_bg: wgpu::BindGroup, target: wgpu::Texture, target_view: wgpu::TextureView,
    query_set: wgpu::QuerySet, ts_resolve: wgpu::Buffer, ts_read: wgpu::Buffer,
    chunk_shift: u32,
}

fn build_resources(gpu: &Gpu, res: &Resident, adv: &[f32; 256], n_items: usize, max_segments: usize, max_slots: usize, chunk_size: usize) -> Resources {
    use wgpu::{BufferUsages as U, ShaderStages as S};
    let d = &gpu.device;
    let adv_buf = gpu.init_buffer("advances", bytemuck::cast_slice(adv), U::STORAGE);
    let segs_buf = gpu.buffer("segments", (max_segments as u64) * 32, U::STORAGE | U::COPY_DST, false);
    let slots_buf = gpu.buffer("slots", (max_slots as u64) * SLOT_BYTES, U::STORAGE | U::COPY_SRC, false);
    let params_buf = gpu.buffer("params", 16, U::UNIFORM | U::COPY_DST, false);
    let cam_buf = gpu.buffer("cam", 32, U::UNIFORM | U::COPY_DST, false);
    let origins_buf = gpu.buffer("origins", (n_items as u64) * 8, U::STORAGE | U::COPY_DST, false);
    let dummy = gpu.buffer("dummy-chunk", 4, U::STORAGE, false);
    let chunk = |i: usize| res.chunks.get(i).unwrap_or(&dummy).as_entire_binding();

    let compute_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("layout.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("layout.wgsl").into()) });
    let compute_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[
        Gpu::storage_entry(0, true, S::COMPUTE), Gpu::storage_entry(1, true, S::COMPUTE), Gpu::storage_entry(2, true, S::COMPUTE), Gpu::storage_entry(3, true, S::COMPUTE),
        Gpu::storage_entry(4, true, S::COMPUTE), Gpu::storage_entry(5, true, S::COMPUTE), Gpu::storage_entry(6, true, S::COMPUTE),
        Gpu::storage_entry(7, false, S::COMPUTE), Gpu::uniform_entry(8, S::COMPUTE),
    ] });
    let compute_bg = d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &compute_bgl, entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: chunk(0) },
        wgpu::BindGroupEntry { binding: 1, resource: chunk(1) },
        wgpu::BindGroupEntry { binding: 2, resource: chunk(2) },
        wgpu::BindGroupEntry { binding: 3, resource: chunk(3) },
        wgpu::BindGroupEntry { binding: 4, resource: res.items.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 5, resource: segs_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 6, resource: adv_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 7, resource: slots_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 8, resource: params_buf.as_entire_binding() },
    ] });
    let compute_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&compute_bgl)], immediate_size: 0 });
    let compute = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("layout"), layout: Some(&compute_layout), module: &compute_module, entry_point: Some("layout_segments"), compilation_options: Default::default(), cache: None });

    let draw_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("draw.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()) });
    let render_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[Gpu::storage_entry(0, true, S::VERTEX), Gpu::uniform_entry(1, S::VERTEX), Gpu::storage_entry(2, true, S::VERTEX)] });
    let render_bg = d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &render_bgl, entries: &[
        wgpu::BindGroupEntry { binding: 0, resource: slots_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 1, resource: cam_buf.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 2, resource: origins_buf.as_entire_binding() },
    ] });
    let render_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&render_bgl)], immediate_size: 0 });
    let render = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("draw"), layout: Some(&render_layout),
        vertex: wgpu::VertexState { module: &draw_module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(), depth_stencil: None, multisample: Default::default(),
        fragment: Some(wgpu::FragmentState { module: &draw_module, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())] }),
        multiview_mask: None, cache: None,
    });
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"), size: wgpu::Extent3d { width: TARGET.0, height: TARGET.1, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());

    let query_set = d.create_query_set(&wgpu::QuerySetDescriptor { label: Some("ts"), ty: wgpu::QueryType::Timestamp, count: 4 });
    let ts_resolve = gpu.buffer("ts-resolve", 32, U::QUERY_RESOLVE | U::COPY_SRC, false);
    let ts_read = gpu.buffer("ts-read", 32, U::MAP_READ | U::COPY_DST, false);
    Resources { segs_buf, slots_buf, params_buf, cam_buf, origins_buf, compute, compute_bg, render, render_bg, target, target_view, query_set, ts_resolve, ts_read, chunk_shift: chunk_size.trailing_zeros() }
}

struct Frame { cpu_ms: f64, wall_ms: f64, compute_ms: f64, draw_ms: f64, slots: usize, bytes_read: u64, segments: usize }

/// One "frame": visible list (segments + prefix) + upload, compute layout, draw;
/// GPU times from timestamp queries around each pass.
fn run_frame(gpu: &Gpu, r: &Resources, c: &Corpus, lines: &[u32], draw: bool) -> Frame {
    let t_cpu = Instant::now();
    let vis = visible_list(c, lines);
    gpu.queue.write_buffer(&r.segs_buf, 0, bytemuck::cast_slice(&vis.segs));
    gpu.queue.write_buffer(&r.params_buf, 0, bytemuck::bytes_of(&Params { segment_count: vis.segs.len() as u32, wrap_cols: WRAP_COLS, chunk_shift: r.chunk_shift, chunk_mask: (1u32 << r.chunk_shift).wrapping_sub(1) }));
    let cpu_ms = ms(t_cpu);

    let mut enc = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("layout"), timestamp_writes: Some(wgpu::ComputePassTimestampWrites { query_set: &r.query_set, beginning_of_pass_write_index: Some(0), end_of_pass_write_index: Some(1) }) });
        pass.set_pipeline(&r.compute);
        pass.set_bind_group(0, &r.compute_bg, &[]);
        pass.dispatch_workgroups((vis.segs.len() as u32).div_ceil(64), 1, 1);
    }
    if draw {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &r.target_view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.08, b: 0.10, a: 1.0 }), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: None,
            timestamp_writes: Some(wgpu::RenderPassTimestampWrites { query_set: &r.query_set, beginning_of_pass_write_index: Some(2), end_of_pass_write_index: Some(3) }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&r.render);
        pass.set_bind_group(0, &r.render_bg, &[]);
        pass.draw(0..6, 0..vis.slots as u32);
    }
    enc.resolve_query_set(&r.query_set, 0..if draw { 4 } else { 2 }, &r.ts_resolve, 0);
    enc.copy_buffer_to_buffer(&r.ts_resolve, 0, &r.ts_read, 0, 32);
    let wall_ms = gpu.submit_wait(enc.finish());

    let ts: [u64; 4] = *bytemuck::from_bytes(&gpu.map_read(&r.ts_read, 32)[..32]);
    let tick = gpu.ts_period as f64 / 1e6;
    Frame { cpu_ms, wall_ms, compute_ms: ts[1].wrapping_sub(ts[0]) as f64 * tick, draw_ms: if draw { ts[3].wrapping_sub(ts[2]) as f64 * tick } else { 0.0 }, slots: vis.slots, bytes_read: vis.bytes_read, segments: vis.segs.len() }
}

/// Place the view's items and fit the camera to what the kernel just emitted (one untimed frame + readback).
fn set_view_camera(gpu: &Gpu, r: &Resources, c: &Corpus, view: &View) {
    let origins = item_origins(c, view);
    gpu.queue.write_buffer(&r.origins_buf, 0, bytemuck::cast_slice(&origins));
    let f = run_frame(gpu, r, c, &view.lines, false);
    let slots: &[Slot] = &bytemuck::cast_slice::<u8, Slot>(&gpu.read_back(&r.slots_buf, f.slots as u64 * SLOT_BYTES)).to_vec();
    gpu.queue.write_buffer(&r.cam_buf, 0, bytemuck::bytes_of(&fit_camera(slots, &origins, view.tint)));
}

/// The offscreen target as a PNG (RGB, no text of any kind in the image).
fn screenshot(gpu: &Gpu, r: &Resources, path: &Path) {
    let (w, h) = TARGET;
    let pitch = (w * 4).next_multiple_of(256);
    let buf = gpu.buffer("shot", (pitch * h) as u64, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, false);
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(r.target.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(pitch), rows_per_image: None } }, wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 });
    gpu.submit_wait(enc.finish());
    let data = gpu.map_read(&buf, (pitch * h) as u64);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        for px in data[y * pitch as usize..][..(w * 4) as usize].chunks_exact(4) { rgb.extend_from_slice(&px[..3]) }
    }
    let file = std::fs::File::create(path).expect("screenshot path");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    println!("screenshot: {} ({} bytes)", path.display(), std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
}

/// GPU output of a view against the single-threaded whole-line CPU fold, every slot, bit-exact.
fn verify(gpu: &Gpu, r: &Resources, c: &Corpus, view: &View, adv: &[f32; 256]) -> String {
    let f = run_frame(gpu, r, c, &view.lines, false);
    let gpu_slots: Vec<Slot> = bytemuck::cast_slice(&gpu.read_back(&r.slots_buf, f.slots as u64 * SLOT_BYTES)).to_vec();
    let cpu_slots = cpu_reference(c, &view.lines, adv);
    assert_eq!(gpu_slots.len(), cpu_slots.len());
    match gpu_slots.iter().zip(&cpu_slots).position(|(a, b)| a != b) {
        None => format!("PASS: {} slots bit-equal GPU (from {} segments) vs whole-line CPU fold, x as f32 bits included", f.slots, f.segments),
        Some(i) => {
            let vis = visible_list(c, &view.lines);
            let seg = vis.segs.iter().rev().find(|s| (s.slot_base as usize) <= i).unwrap();
            format!("FAIL: first mismatch at slot {i} (segment byte_start {} col_seed {} x_seed {}): gpu {:?} cpu {:?}; {} of {} differ", seg.byte_start, seg.col_seed, seg.x_seed, gpu_slots[i], cpu_slots[i], gpu_slots.iter().zip(&cpu_slots).filter(|(a, b)| a != b).count(), f.slots)
        }
    }
}

fn vram_total_mb() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi").args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

// ---------------------------------------------------------------- main

fn main() {
    let args = parse_args();
    println!("loadavg at start: {}", loadavg());

    let gpu = init_gpu();
    let al = &gpu.adapter_limits;
    println!("adapter: {} ({:?}, driver {} {}); timestamp period {} ns", gpu.adapter_info.name, gpu.adapter_info.backend, gpu.adapter_info.driver, gpu.adapter_info.driver_info, gpu.ts_period);
    println!("adapter limits: max_buffer_size {} ({:.0} MB), max_storage_buffer_binding_size {} ({:.0} MB)", al.max_buffer_size, mb(al.max_buffer_size), al.max_storage_buffer_binding_size, mb(al.max_storage_buffer_binding_size as u64));
    // Byte chunks are a power of two no larger than either limit (and < 4 GiB so a u32 byte index shifts to a chunk).
    let cap = (al.max_storage_buffer_binding_size as u64).min(al.max_buffer_size).min(1 << 31);
    let chunk_size = 1usize << (63 - cap.leading_zeros());

    // 1. The walk (renderer rules), then Pass 1 with the requested thread count: the kept tables.
    let adv = advance_table();
    let w = walk(&args.corpus, args.walk, args.threads, chunk_size);
    let st = &w.stats;
    let n_chunks = w.bytes.len().div_ceil(chunk_size);
    assert!(n_chunks <= MAX_BYTE_CHUNKS, "{} bytes need {} chunks of {}; the kernel binds {}", w.bytes.len(), n_chunks, chunk_size, MAX_BYTE_CHUNKS);
    let p1 = pass1(&w.bytes, &w.files, args.threads, args.segment_bytes, &adv);
    let corpus = Corpus { bytes: w.bytes, files: w.files, lines: p1.lines, items: p1.items, seeds: p1.seeds, total_glyphs: p1.total_glyphs, segment_bytes: args.segment_bytes };
    let source_bytes: u64 = corpus.files.iter().map(|f| f.byte_len as u64).sum();
    println!("walk ({}): {} candidates, {} kept, {} skipped >10 MiB, {} non-UTF-8, {} unreadable; enumerate {:.0} ms, read+validate {:.0} ms ({} threads), concat {:.0} ms; {} B source ({} B chunk padding, {} chunk(s) of {:.0} MB)",
        if args.walk == WalkMode::Renderer { "renderer rules" } else { "all files" }, st.candidates, corpus.files.len(), st.skipped_large, st.skipped_non_utf8, st.read_errors, st.enumerate_ms, st.read_ms, args.threads, st.concat_ms, source_bytes, st.pad_bytes, n_chunks, mb(chunk_size as u64));
    println!("corpus: {} files, {} B, {} lines, {} glyphs; Pass 1 ({} threads) {:.1} ms; {} lines longer than {} B carry {} segment seeds",
        corpus.files.len(), source_bytes, corpus.line_count(), corpus.total_glyphs, args.threads, p1.wall_ms, corpus.seeds.len(), args.segment_bytes, corpus.seeds.values().map(|s| s.len()).sum::<usize>());
    assert!(corpus.line_count() > 0, "no lines");
    let (_, page_item) = page_view(&corpus);
    println!("page view: {} ({} B, {} lines)", corpus.files[page_item].rel, corpus.items[page_item].byte_len, corpus.items[page_item].line_count);

    // 2. Views, and the buffers sized to the largest.
    let views = plan_views(&corpus, &args);
    let visibles: Vec<Visible> = views.iter().map(|v| visible_list(&corpus, &v.lines)).collect();
    let max_slots = visibles.iter().map(|v| v.slots).max().unwrap().max(1);
    let max_segments = visibles.iter().map(|v| v.segs.len()).max().unwrap().max(1);
    drop(visibles);
    let lim = gpu.device.limits();
    assert!((max_slots as u64) * SLOT_BYTES <= lim.max_storage_buffer_binding_size as u64, "largest view needs {} slot bytes, binding limit {}", max_slots as u64 * SLOT_BYTES, lim.max_storage_buffer_binding_size);

    // 3. Upload the resident set, repeated.
    let (mut create_ms, mut submit_ms) = (vec![], vec![]);
    let mut resident = None;
    for _ in 0..args.repeat {
        drop(resident.take());
        gpu.wait();
        let (r, c, s) = upload_resident(&gpu, &corpus, chunk_size);
        create_ms.push(c);
        submit_ms.push(s);
        resident = Some(r);
    }
    let resident = resident.unwrap();
    let res = build_resources(&gpu, &resident, &adv, corpus.items.len(), max_segments, max_slots, chunk_size);

    // 4. The interleaved measurement loop: Pass 1 at every thread count, then one frame per view.
    let mut thread_set: Vec<usize> = PASS1_THREAD_SET.to_vec();
    thread_set.push(args.threads);
    thread_set.sort();
    thread_set.dedup();
    let mut pass1_ms: Vec<Vec<f64>> = vec![vec![]; thread_set.len()];
    let mut frames: Vec<Vec<Frame>> = (0..views.len()).map(|_| Vec::new()).collect();
    for rep in 0..args.repeat {
        for (ti, &t) in thread_set.iter().enumerate() {
            let p = pass1(&corpus.bytes, &corpus.files, t, args.segment_bytes, &adv);
            assert_eq!(p.lines.len(), corpus.lines.len(), "Pass 1 at {t} threads disagrees on the line count");
            assert_eq!(p.total_glyphs, corpus.total_glyphs);
            pass1_ms[ti].push(p.wall_ms);
        }
        for (vi, view) in views.iter().enumerate() {
            if args.draw { set_view_camera(&gpu, &res, &corpus, view) }
            frames[vi].push(run_frame(&gpu, &res, &corpus, &view.lines, args.draw));
            if rep + 1 == args.repeat {
                if let Some(path) = &args.screenshot { screenshot(&gpu, &res, path) }
            }
        }
    }

    // 5. Readback checks: the random window and the worst view (segments) against the whole-line CPU fold.
    let verdicts: Vec<(String, String)> = views.iter().filter(|v| v.verify).map(|v| (v.name.clone(), verify(&gpu, &res, &corpus, v, &adv))).collect();

    // ---------------------------------------------------------------- report
    let mm = |v: &[f64]| { let (a, b) = stats(v); format!("{a:9.3} {b:9.3}") };
    println!();
    println!("=== jit-layout: {} files, {} B source, {} lines, {} glyphs; {} repeats; min / median in ms ===", corpus.files.len(), source_bytes, corpus.line_count(), corpus.total_glyphs, args.repeat);
    println!();
    println!("--- Pass 1 scaling (line table 8 B/line + item table 32 B/item + long-line seeds; wall, this process alone) ---");
    println!("{:>8} {:>9} {:>9} {:>12} {:>12}", "threads", "min", "median", "MB/s@min", "MB/s@median");
    for (ti, &t) in thread_set.iter().enumerate() {
        let (a, b) = stats(&pass1_ms[ti]);
        println!("{:>8} {:9.1} {:9.1} {:12.0} {:12.0}", t, a, b, source_bytes as f64 / a / 1e3, source_bytes as f64 / b / 1e3);
    }
    println!("walk (once): enumerate {:.0} ms, read+validate {:.0} ms ({} threads, {:.0} MB/s), concat {:.0} ms", st.enumerate_ms, st.read_ms, args.threads, source_bytes as f64 / st.read_ms / 1e3, st.concat_ms);
    println!();
    println!("--- resident upload ({} chunk(s) of bytes + line table + item table), create_buffer_init / submit+poll ---", n_chunks);
    println!("{:<44} {}", format!("create_buffer_init ({:.0} MB)", mb(corpus.bytes.len() as u64 + corpus.lines.len() as u64 * 8 + corpus.items.len() as u64 * 32)), mm(&create_ms));
    println!("{:<44} {}", "submit + poll", mm(&submit_ms));
    println!();
    for (vi, view) in views.iter().enumerate() {
        let fr = &frames[vi];
        let col = |f: &dyn Fn(&Frame) -> f64| fr.iter().map(f).collect::<Vec<_>>();
        let slots = fr[0].slots;
        println!("--- view {}: {} lines, {} segments, {} slots, {} visible bytes, transient slots {:.1} MB ---", view.name, view.lines.len(), fr[0].segments, slots, fr[0].bytes_read, mb(slots as u64 * SLOT_BYTES));
        println!("{:<44} {:>9} {:>9}", "measurement", "min", "median");
        let (min_c, med_c) = stats(&col(&|f| f.compute_ms));
        println!("{:<44} {}   {:.2} / {:.2} G glyphs/s", "compute pass GPU", mm(&col(&|f| f.compute_ms)), slots as f64 / min_c / 1e6, slots as f64 / med_c / 1e6);
        if args.draw { println!("{:<44} {}", "draw pass GPU (flat quads, 800x500)", mm(&col(&|f| f.draw_ms))) }
        println!("{:<44} {}", "CPU visible list + write_buffer", mm(&col(&|f| f.cpu_ms)));
        println!("{:<44} {}", "frame wall (submit..poll)", mm(&col(&|f| f.wall_ms)));
        println!();
    }
    println!("--- resident memory ---");
    let (b_mb, l_mb, i_mb) = (mb(corpus.bytes.len() as u64), mb(corpus.lines.len() as u64 * 8), mb(corpus.items.len() as u64 * 32));
    println!("bytes buffer {:.1} MB, line table {:.1} MB ({} B/line), item table {:.1} MB ({} B/item); total resident {:.1} MB; largest transient slot buffer {:.1} MB ({} slots)", b_mb, l_mb, 8, i_mb, 32, b_mb + l_mb + i_mb, mb(max_slots as u64 * SLOT_BYTES), max_slots);
    let vram = vram_total_mb().map(|m| format!("{m} MiB (nvidia-smi)")).unwrap_or_else(|| "n/a".into());
    println!("HyperLayout whole-tree slots for this corpus: {:.1} MB Derived (20 B/glyph), {:.1} MB Instanced (32 B/glyph); adapter max_buffer_size {:.0} MB, VRAM total {}", mb(corpus.total_glyphs * 20), mb(corpus.total_glyphs * 32), mb(al.max_buffer_size), vram);
    println!();
    for (name, v) in &verdicts { println!("readback check ({name}, segment-bytes {}): {v}", args.segment_bytes) }
    println!("not measured: Slug coverage in the fragment stage (identical for both designs; quads only), emoji/ZWJ sequences and the atlas trie, pagination, the renderer's paint beyond a `//` comment flag, a real camera (views are synthetic), depth test.");
    println!("loadavg at end: {}", loadavg());
}
