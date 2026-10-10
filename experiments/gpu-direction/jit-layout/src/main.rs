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
//! Two ways to decide what is visible:
//!   `--view ...`    the synthetic views: a line window or a file sample CHOSEN
//!                   on the CPU — no frustum test; this is what every number
//!                   before the `--camera` work measured.
//!   `--camera ...`  a perspective camera over a shelf placement of every file,
//!                   culled either on the CPU (`--cull cpu`: frustum + LOD over
//!                   the same boxes, list uploaded) or on the GPU (`--cull gpu`:
//!                   four passes, indirect dispatch and draw, no readback).
//!
//! Colour comes from resident tables, not from a load-time pass: byte-range
//! spans per file (`--spans demo`) or a dense per-glyph array for the files
//! that need it (`--dense-*`). See spans.rs.
//!
//! What this does NOT model: emoji/ZWJ sequences, the real atlas trie, Slug
//! coverage in the fragment stage, pagination, the renderer's paint rules
//! beyond a `//` comment flag, depth testing, real LSP spans.
//!
//! Every timing repeats `--repeat` times, interleaved across the measured
//! things, and reports min and median because the GPU and CPU are shared.

mod camera;
mod spans;

use bytemuck::{Pod, Zeroable};
use camera::{Camera, CameraU, ROW_H};
use spans::{Dense, SpanTable};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use wgpu::util::DeviceExt;

const SLOT_BYTES: u64 = 20;
const WRAP_COLS: u32 = 120;
const MAX_ADV: f32 = 2.4;
const MAX_FILE_BYTES: u64 = 10 << 20;
const MAX_BYTE_CHUNKS: usize = 4;
const GLYPH_W: f32 = 0.6;
const COLOR_CODE: u32 = 0xFFD8E2E8;
const COLOR_COMMENT: u32 = 0xFF5AB06A;
const PASS1_THREAD_SET: [usize; 5] = [1, 4, 8, 16, 32];
const PAGE_LINES: usize = 60;
const OVERVIEW_FILES: usize = 2000;
const OVERVIEW_LINES_PER_FILE: usize = 50;
const WORST_LINES: usize = 1000;
const ZOOMOUT_FILES: usize = 20_000;
const SHELF_PITCH: f32 = WRAP_COLS as f32 * 0.9 + 12.0;
const SHELF_GAP_Y: f32 = 6.0;
const CAMERA_PRESETS: [&str; 5] = ["page", "overview", "zoomout", "far", "dense"];

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
/// The line table, 8 B/line: where the line starts, and the glyph ordinal (within its FILE) of
/// its first glyph. Its glyph count is the next line's prefix minus its own (the item's total for
/// the last line); its byte length is `min(next.byte_start, item end) - byte_start - 1`. A
/// sentinel entry closes the last line.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Line { byte_start: u32, glyph_prefix: u32 }
/// The item (file) table, 48 B/item. `byte_len` includes the terminating '\n'. `repr`: 0 heuristic
/// colour, 1 spans, 2 dense colour, 3 dense colour + transform.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Item { first_line: u32, line_count: u32, byte_start: u32, byte_len: u32, max_len: u32, glyph_count: u32, span_base: u32, span_count: u32, repr: u32, dense_base: u32, _pad: [u32; 2] }
/// One visible segment, 32 B, per frame. A whole line has `col_seed = 0, x_seed = 0, state = 0`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct Seg { byte_start: u32, byte_len: u32, line_idx: u32, item: u32, slot_base: u32, col_seed: u32, x_seed: f32, state: u32 }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params { wrap_cols: u32, chunk_shift: u32, chunk_mask: u32, default_color: u32, _pad: [u32; 4] }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CullParams { n_items: u32, segment_bytes: u32, wrap_cols: u32, cap_segs: u32, cap_slots: u32, _pad: [u32; 3] }
/// A file's world box, 32 B, resident: the shelf origin, width (max line x extent, exact from Pass 1),
/// height (rows), and the wrap depth bound. The box top is `oy + ROW_H`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct FileBox { ox: f32, oy: f32, w: f32, h: f32, first_line: u32, line_count: u32, depth: f32, _pad: u32 }
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct SeedDir { line_idx: u32, seed_base: u32, seed_count: u32, _pad: u32 }
/// A continuation's seeds: where it starts in the line, and the fold state there. 16 B, resident.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct SegSeed { byte_off: u32, col: u32, x: f32, state: u32 }
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Query { item: u32, byte: u32 }

/// The counters buffer, 32 B: what the GPU cull counts and the layout/draw consume.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Counts { vis_files: u32, backdrops: u32, cand_lines: u32, vis_lines: u32, segs: u32, slots: u32, dropped: u32, draw_limit: u32 }
impl Counts {
    fn from_words(w: &[u32]) -> Counts { Counts { vis_files: w[0], backdrops: w[1], cand_lines: w[2], vis_lines: w[3], segs: w[4], slots: w[5], dropped: w[6], draw_limit: w[7] } }
    fn words(&self) -> [u32; 8] { [self.vis_files, self.backdrops, self.cand_lines, self.vis_lines, self.segs, self.slots, self.dropped, self.draw_limit] }
}

// ---------------------------------------------------------------- CLI

#[derive(Clone, Copy, PartialEq)]
enum WalkMode { Renderer, All }
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cull { Cpu, Gpu }

struct Args {
    corpus: PathBuf, walk: WalkMode, threads: usize, view: Option<String>, visible_lines: usize, segment_bytes: usize, repeat: usize, screenshot: Option<PathBuf>, draw: bool,
    cull: Vec<Cull>, cameras: Vec<&'static str>, lod_px: f32, target: (u32, u32), max_slots: usize, max_segments: usize,
    spans: bool, dense_files: Vec<String>, dense_sample: usize, dense_in_view: usize, dense_xf: bool, edit_file: String, probes: usize, pass1_scaling: bool,
}

const USAGE: &str = "usage: jit-layout <corpus-dir> [--walk renderer|all] [--threads T] [--view page|overview|worst|random] [--visible-lines N] [--segment-bytes S] [--repeat K] [--screenshot <png>] [--no-draw]\n\
    [--camera page|overview|zoomout|far|dense|all[,..]] [--cull cpu|gpu|both] [--lod-px F] [--target WxH] [--max-slots N] [--max-segments N]\n\
    [--spans none|demo] [--dense-file <rel>]* [--dense-sample N] [--dense-in-view N] [--dense-mode colour|colour-xf] [--edit-file <rel>] [--probes N] [--no-pass1-scaling]";

fn parse_args() -> Args {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let mut a = Args {
        corpus: PathBuf::new(), walk: WalkMode::Renderer, threads, view: None, visible_lines: 100_000, segment_bytes: 2048, repeat: 5, screenshot: None, draw: true,
        cull: vec![Cull::Cpu], cameras: vec![], lod_px: 1.0, target: (800, 500), max_slots: 64 << 20, max_segments: 4 << 20,
        spans: false, dense_files: vec![], dense_sample: 0, dense_in_view: 0, dense_xf: true, edit_file: "kernel/bpf/verifier.c".into(), probes: 1000, pass1_scaling: true,
    };
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
            "--cull" => a.cull = match val("--cull").as_str() { "cpu" => vec![Cull::Cpu], "gpu" => vec![Cull::Gpu], "both" => vec![Cull::Cpu, Cull::Gpu], other => panic!("--cull: {other}") },
            "--camera" => for name in val("--camera").split(',') {
                if name == "all" { a.cameras.extend(CAMERA_PRESETS) } else { a.cameras.push(CAMERA_PRESETS.iter().copied().find(|p| *p == name).unwrap_or_else(|| panic!("--camera: {name}"))) }
            },
            "--lod-px" => a.lod_px = val("--lod-px").parse().expect("--lod-px"),
            "--target" => { let v = val("--target"); let (w, h) = v.split_once('x').expect("--target WxH"); a.target = (w.parse().expect("--target"), h.parse().expect("--target")) }
            "--max-slots" => a.max_slots = num("--max-slots"),
            "--max-segments" => a.max_segments = num("--max-segments"),
            "--spans" => a.spans = match val("--spans").as_str() { "none" => false, "demo" => true, other => panic!("--spans: {other}") },
            "--dense-file" => a.dense_files.push(val("--dense-file")),
            "--dense-sample" => a.dense_sample = num("--dense-sample"),
            "--dense-in-view" => a.dense_in_view = num("--dense-in-view"),
            "--dense-mode" => a.dense_xf = match val("--dense-mode").as_str() { "colour" | "color" => false, "colour-xf" | "color-xf" => true, other => panic!("--dense-mode: {other}") },
            "--edit-file" => a.edit_file = val("--edit-file"),
            "--probes" => a.probes = num("--probes"),
            "--no-pass1-scaling" => a.pass1_scaling = false,
            _ if a.corpus.as_os_str().is_empty() => a.corpus = PathBuf::from(arg),
            _ => panic!("unexpected argument {arg}\n{USAGE}"),
        }
    }
    assert!(!a.corpus.as_os_str().is_empty(), "{USAGE}");
    assert!(a.repeat >= 1 && a.visible_lines >= 1 && a.threads >= 1 && a.segment_bytes >= 16 && a.target.0 >= 16 && a.target.1 >= 16);
    if let Some(v) = &a.view { assert!(["page", "overview", "worst", "random"].contains(&v.as_str()), "--view: {v}") }
    assert!(a.screenshot.is_none() || a.view.is_some() || !a.cameras.is_empty(), "--screenshot needs --view or --camera");
    assert!(a.screenshot.is_none() || a.draw, "--screenshot needs the draw pass");
    a
}

fn loadavg() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") { return s.trim().to_string() }
    // No procfs (macOS): `uptime` prints "... load averages: 1.23 1.45 1.67" (or "load average:").
    std::process::Command::new("uptime").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split("load average").nth(1).map(|t| t.trim_start_matches(|c| c == 's' || c == ':').trim().replace(',', "")))
        .unwrap_or_else(|| "n/a".into())
}

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1e3 }

/// (min, median) of a sample set.
fn stats(v: &[f64]) -> (f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[0], s[s.len() / 2])
}

fn mb(b: u64) -> f64 { b as f64 / 1e6 }

/// Contiguous index ranges over `weights`, `n` of them, balanced by weight (fewer when there are fewer items).
pub fn partition(weights: &[usize], n: usize) -> Vec<std::ops::Range<usize>> {
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
pub struct FileSpan { rel: String, byte_start: usize, byte_len: usize }

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
            let end = last.byte_start + last.byte_len;
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(end - done);
            rest = tail;
            done = end;
            let (files, datas) = (&files[g.clone()], &datas[g]);
            let base = first.byte_start;
            let skip = first.byte_start - (end - mine.len());
            s.spawn(move || {
                for (f, d) in files.iter().zip(datas) {
                    let dst = &mut mine[skip + f.byte_start - base..skip + f.byte_start - base + f.byte_len];
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
    /// The slot for the decoded glyph, then advance — the kernel's loop body (heuristic colour).
    fn emit(&mut self, glyph: u32, a: f32, row: u32, item: u32) -> Slot {
        if self.col % WRAP_COLS == 0 { self.x = 0.0 }
        let s = Slot { x: self.x, row, glyph_and_wrap: glyph | ((self.col / WRAP_COLS) << 16), color: if self.in_comment { COLOR_COMMENT } else { COLOR_CODE }, item };
        self.x += a;
        self.col += 1;
        s
    }
}

/// Lay out one WHOLE line from column 0 — the reference every segmented fold is held to —
/// coloured by the item's representation, exactly as the kernel does it.
fn layout_line(c: &Corpus, li: u32, item: u32, adv: &[f32; 256], out: &mut [Slot], xf_out: &mut Vec<(usize, [u32; 2])>) {
    let it = &c.items[item as usize];
    let (byte_start, byte_len, row) = (c.lines[li as usize].byte_start as usize, c.line_len(li, it) as usize, li - it.first_line);
    let (mut f, mut i, mut k) = (Fold::new(), byte_start, 0usize);
    let (mut si, s_end) = if it.repr == 1 { (c.spans.as_ref().unwrap().line_idx[li as usize] as usize, (it.span_base + it.span_count) as usize) } else { (0, 0) };
    let mut ord = it.dense_base + c.lines[li as usize].glyph_prefix;
    while i < byte_start + byte_len {
        let (g, a, l) = f.decode(&c.bytes, i, adv);
        let mut s = f.emit(g, a, row, item);
        match it.repr {
            0 => {}
            1 => {
                let table = &c.spans.as_ref().unwrap().table;
                let fb = (i - it.byte_start as usize) as u32;
                while si < s_end && table[si].end() <= fb { si += 1 }
                s.color = if si < s_end && table[si].byte_start <= fb { c.palette[table[si].pal() as usize] } else { COLOR_CODE };
            }
            _ => {
                let d = c.dense.as_ref().unwrap();
                s.color = d.color[ord as usize];
                if it.repr == 3 { xf_out.push((k, d.xf[ord as usize])) }
                ord += 1;
            }
        }
        out[k] = s;
        i += l;
        k += 1;
    }
}

/// A short line's glyph count and x extent (the fold's advance sum; exact when it does not wrap,
/// else bounded by a full wrap unit of tabs).
fn measure_line(line: &[u8], adv: &[f32; 256]) -> (u32, f32) {
    let (mut g, mut x) = (0u32, 0f32);
    for &b in line {
        if b & 0xC0 == 0x80 { continue }
        g += 1;
        x += if b < 0x80 { adv[b as usize] } else { 1.0 };
    }
    (g, if g <= WRAP_COLS { x } else { x.min(WRAP_COLS as f32 * MAX_ADV) })
}

/// Pass 1's serial walk of a long line: cut into segments of at least `seg_bytes`,
/// each cut landing only BEFORE AN ASCII BYTE (never inside a codepoint), and
/// snapshot the fold at each cut: the column, the running f32 x (0 when the cut
/// falls on a wrap-unit boundary, as the kernel resets there), the paint flags.
/// Returns the line's glyph count, its x extent (max over wrap units), and the seeds of every segment after the first.
fn seed_long_line(line: &[u8], seg_bytes: usize, adv: &[f32; 256]) -> (u32, f32, Vec<SegSeed>) {
    let (mut f, mut i, mut seg_start, mut seeds, mut max_x) = (Fold::new(), 0usize, 0usize, Vec::new(), 0f32);
    while i < line.len() {
        if i - seg_start >= seg_bytes && line[i] < 0x80 {
            seeds.push(SegSeed { byte_off: i as u32, col: f.col, x: if f.col % WRAP_COLS == 0 { 0.0 } else { f.x }, state: f.state() });
            seg_start = i;
        }
        let (g, a, l) = f.decode(line, i, adv);
        f.emit(g, a, 0, 0);
        max_x = max_x.max(f.x);
        i += l;
    }
    (f.col, max_x, seeds)
}

// ---------------------------------------------------------------- Pass 1

struct Pass1 { lines: Vec<Line>, items: Vec<Item>, widths: Vec<f32>, seed_dir: Vec<SeedDir>, seeds: Vec<SegSeed>, total_glyphs: u64, wall_ms: f64 }

struct Part { lines: Vec<Line>, items: Vec<Item>, widths: Vec<f32>, seeds: Vec<(usize, u32, Vec<SegSeed>)>, glyphs: u64 }

/// The line table, item table, file widths and long-line seeds, built per file on `threads`
/// threads (files are independent) and assembled in parallel.
fn pass1(bytes: &[u8], files: &[FileSpan], threads: usize, seg_bytes: usize, adv: &[f32; 256]) -> Pass1 {
    let t = Instant::now();
    let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
    let parts: Vec<Part> = std::thread::scope(|s| {
        let handles: Vec<_> = groups.iter().map(|g| {
            let files = &files[g.clone()];
            s.spawn(move || {
                let est = files.iter().map(|f| f.byte_len).sum::<usize>() / 24;
                let mut p = Part { lines: Vec::with_capacity(est), items: Vec::with_capacity(files.len()), widths: Vec::with_capacity(files.len()), seeds: Vec::new(), glyphs: 0 };
                for (fi, f) in files.iter().enumerate() {
                    let data = &bytes[f.byte_start..f.byte_start + f.byte_len];
                    let first_line = p.lines.len();
                    let (mut line_start, mut max_len, mut width, mut prefix) = (0usize, 0u32, 0f32, 0u32);
                    for nl in memchr::memchr_iter(b'\n', data) {
                        let line = &data[line_start..nl];
                        let (glyphs, x) = if line.len() > seg_bytes {
                            let (g, x, seeds) = seed_long_line(line, seg_bytes, adv);
                            if !seeds.is_empty() { p.seeds.push((fi, (p.lines.len() - first_line) as u32, seeds)) }
                            (g, x)
                        } else {
                            measure_line(line, adv)
                        };
                        max_len = max_len.max(line.len() as u32);
                        width = width.max(x);
                        p.lines.push(Line { byte_start: (f.byte_start + line_start) as u32, glyph_prefix: prefix });
                        prefix += glyphs;
                        line_start = nl + 1;
                    }
                    p.glyphs += prefix as u64;
                    p.items.push(Item { first_line: first_line as u32, line_count: (p.lines.len() - first_line) as u32, byte_start: f.byte_start as u32, byte_len: f.byte_len as u32, max_len, glyph_count: prefix, span_base: 0, span_count: 0, repr: 0, dense_base: 0, _pad: [0; 2] });
                    p.widths.push(width);
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
    let mut widths = Vec::with_capacity(files.len());
    let (mut seed_dir, mut seeds) = (Vec::new(), Vec::new());
    let mut total_glyphs = 0u64;
    std::thread::scope(|s| {
        let mut rest = &mut lines[..total];
        let mut base = 0usize;
        for p in &parts {
            let item_base = items.len();
            for it in &p.items { items.push(Item { first_line: it.first_line + base as u32, ..*it }) }
            widths.extend_from_slice(&p.widths);
            for (fi, row, sd) in &p.seeds {
                seed_dir.push(SeedDir { line_idx: items[item_base + fi].first_line + row, seed_base: seeds.len() as u32, seed_count: sd.len() as u32, _pad: 0 });
                seeds.extend_from_slice(sd);
            }
            total_glyphs += p.glyphs;
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(p.lines.len());
            rest = tail;
            base += p.lines.len();
            s.spawn(move || mine.copy_from_slice(&p.lines));
        }
    });
    let end = files.last().map(|f| f.byte_start + f.byte_len).unwrap_or(0);
    lines[total] = Line { byte_start: end as u32, glyph_prefix: 0 };
    seed_dir.push(SeedDir { line_idx: u32::MAX, seed_base: 0, seed_count: 0, _pad: 0 });   // sentinel: the directory is never empty
    Pass1 { lines, items, widths, seed_dir, seeds, total_glyphs, wall_ms: ms(t) }
}

// ---------------------------------------------------------------- corpus + visible lists

struct Corpus {
    bytes: Vec<u8>, files: Vec<FileSpan>, lines: Vec<Line>, items: Vec<Item>, widths: Vec<f32>, seed_dir: Vec<SeedDir>, seeds: Vec<SegSeed>,
    total_glyphs: u64, segment_bytes: usize, spans: Option<SpanTable>, dense: Option<Dense>, palette: [u32; 256],
}

impl Corpus {
    fn line_count(&self) -> usize { self.lines.len() - 1 }
    fn item_of(&self, li: u32) -> usize { self.items.partition_point(|it| it.first_line <= li) - 1 }
    fn line_len(&self, li: u32, item: &Item) -> u32 {
        let next = self.lines[li as usize + 1].byte_start.min(item.byte_start + item.byte_len);
        next - self.lines[li as usize].byte_start - 1
    }
    fn line_glyphs(&self, li: u32, item: &Item) -> u32 {
        let next = if li + 1 == item.first_line + item.line_count { item.glyph_count } else { self.lines[li as usize + 1].glyph_prefix };
        next - self.lines[li as usize].glyph_prefix
    }
    /// The seeds of a long line (empty for a line that has none).
    fn seeds_of(&self, li: u32) -> &[SegSeed] {
        let k = self.seed_dir.partition_point(|d| d.line_idx < li);
        let d = &self.seed_dir[k];
        if d.line_idx == li { &self.seeds[d.seed_base as usize..(d.seed_base + d.seed_count) as usize] } else { &[] }
    }
    fn item_by_rel(&self, rel: &str) -> Option<usize> { self.files.iter().position(|f| f.rel == rel) }
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
        let (start, len, glyphs) = (c.lines[li as usize].byte_start, c.line_len(li, it), c.line_glyphs(li, it));
        bytes_read += len as u64;
        // Only a long line can have seeds; the length test keeps the directory search off the common path.
        let seeds = if len as usize > c.segment_bytes { c.seeds_of(li) } else { &[] };
        if seeds.is_empty() {
            segs.push(Seg { byte_start: start, byte_len: len, line_idx: li, item: cur as u32, slot_base: base, col_seed: 0, x_seed: 0.0, state: 0 });
        } else {
            let mut prev = SegSeed { byte_off: 0, col: 0, x: 0.0, state: 0 };
            for sd in seeds.iter().copied().chain(std::iter::once(SegSeed { byte_off: len, col: glyphs, x: 0.0, state: 0 })) {
                segs.push(Seg { byte_start: start + prev.byte_off, byte_len: sd.byte_off - prev.byte_off, line_idx: li, item: cur as u32, slot_base: base + prev.col, col_seed: prev.col, x_seed: prev.x, state: prev.state });
                prev = sd;
            }
        }
        base += glyphs;
    }
    Visible { segs, slots: base as usize, bytes_read }
}

/// The CPU reference: every visible line folded WHOLE, single-threaded, from column 0; plus the
/// dense transforms expected at (slot index, value).
fn cpu_reference(c: &Corpus, lines: &[u32], adv: &[f32; 256]) -> (Vec<Slot>, Vec<(usize, [u32; 2])>) {
    let total: usize = lines.iter().map(|&li| c.line_glyphs(li, &c.items[c.item_of(li)]) as usize).sum();
    let mut out = vec![Slot::zeroed(); total];
    let mut xf = Vec::new();
    let mut k = 0usize;
    for &li in lines {
        let item = c.item_of(li);
        let n = c.line_glyphs(li, &c.items[item]) as usize;
        let mut local = Vec::new();
        layout_line(c, li, item as u32, adv, &mut out[k..k + n], &mut local);
        xf.extend(local.into_iter().map(|(i, v)| (k + i, v)));
        k += n;
    }
    (out, xf)
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

/// The page item: the largest `.c` under `kernel/` (else the largest `.c`, else the largest file).
fn page_item(c: &Corpus) -> usize {
    let pick = |pred: &dyn Fn(&FileSpan) -> bool| c.files.iter().enumerate().filter(|(_, f)| pred(f)).max_by_key(|(_, f)| f.byte_len).map(|(i, _)| i);
    pick(&|f| f.rel.starts_with("kernel/") && f.rel.ends_with(".c")).or_else(|| pick(&|f| f.rel.ends_with(".c"))).or_else(|| pick(&|_| true)).expect("empty corpus")
}

/// The page item's 60 middle rows (first row, count).
fn page_rows(it: &Item) -> (u32, u32) {
    let n = (it.line_count as usize).min(PAGE_LINES) as u32;
    ((it.line_count - n) / 2, n)
}

/// 60 consecutive lines from the middle of the page item.
fn page_view(c: &Corpus) -> View {
    let it = &c.items[page_item(c)];
    let (lo, n) = page_rows(it);
    let start = it.first_line + lo;
    View { name: "page".into(), lines: (start..start + n).collect(), placement: Placement::Stack, tint: false, verify: false }
}

/// The items the overview samples: every k-th file in walk order, 2,000 files.
fn overview_items(c: &Corpus) -> Vec<usize> {
    let k = (c.items.len() / OVERVIEW_FILES).max(1);
    (0..c.items.len()).step_by(k).take(OVERVIEW_FILES).collect()
}

/// The first 50 lines of every sampled file.
fn overview_view(c: &Corpus) -> View {
    let mut lines = Vec::new();
    for i in overview_items(c) {
        let it = &c.items[i];
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
        println!("  {:>8} B  {}:{}  ({} glyphs)", len, c.files[item].rel, li - it.first_line + 1, c.line_glyphs(li, it));
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
        Some("page") => vec![page_view(c)],
        Some("overview") => vec![overview_view(c)],
        Some("worst") => vec![worst_view(c)],
        Some("random") => vec![random_view(c, args.visible_lines, &mut rng, true)],
        None if !args.cameras.is_empty() => vec![],
        _ => {
            let mut v = vec![page_view(c), overview_view(c), worst_view(c)];
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
fn item_origins(c: &Corpus, view: &View, target: (u32, u32)) -> Vec<[f32; 2]> {
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
            let aspect = target.0 as f32 / target.1 as f32;
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
fn fit_camera(slots: &[Slot], origins: &[[f32; 2]], tint: bool, target: (u32, u32)) -> Camera {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for s in slots {
        let o = origins[s.item as usize];
        let (x, y) = (o[0] + s.x, o[1] - s.row as f32 * ROW_H);
        x0 = x0.min(x); x1 = x1.max(x + GLYPH_W); y0 = y0.min(y); y1 = y1.max(y + ROW_H);
    }
    if slots.is_empty() { (x0, x1, y0, y1) = (0.0, 1.0, 0.0, 1.0) }
    Camera::ortho_fit(x0, x1, y0, y1, target, tint, COLOR_CODE)
}

// ---------------------------------------------------------------- shelf placement, camera presets, CPU cull

/// Every file placed once, for good: columns of files in walk order, each column filled to a
/// height that makes the whole tree roughly 16:10. A file's box is its origin, its exact max line
/// extent (Pass 1), its rows, and a wrap-depth bound. Files wider than the column pitch (tab-led
/// lines of 120 columns) overlap the next column; cosmetic, the boxes stay honest.
fn shelf_boxes(c: &Corpus) -> Vec<FileBox> {
    let total_h: f64 = c.items.iter().map(|it| (it.line_count as f32 * ROW_H + SHELF_GAP_Y) as f64).sum();
    let col_h = (total_h * SHELF_PITCH as f64 / 1.6).sqrt() as f32;
    let (mut col, mut y) = (0u32, 0f32);
    let mut boxes = Vec::with_capacity(c.items.len());
    for (i, it) in c.items.iter().enumerate() {
        let h = it.line_count as f32 * ROW_H;
        if y > 0.0 && y + h > col_h { col += 1; y = 0.0 }
        boxes.push(FileBox { ox: col as f32 * SHELF_PITCH, oy: -y - ROW_H, w: c.widths[i], h, first_line: it.first_line, line_count: it.line_count, depth: (it.max_len / WRAP_COLS + 1) as f32 * 0.1, _pad: 0 });
        y += h + SHELF_GAP_Y;
    }
    boxes
}

fn bbox(boxes: &[FileBox], which: impl Iterator<Item = usize>) -> (f32, f32, f32, f32) {
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for i in which {
        let b = &boxes[i];
        x0 = x0.min(b.ox); x1 = x1.max(b.ox + b.w.max(GLYPH_W)); y0 = y0.min(b.oy + ROW_H - b.h); y1 = y1.max(b.oy + ROW_H);
    }
    (x0, x1, y0, y1)
}

/// The camera presets over the shelf. `page` frames 60 rows of the page item; `overview`/`zoomout`
/// frame a box at the tree's centre, at the target's aspect, whose area is 2,000 / 20,000 files' share
/// of the whole shelf (so ~that many files are in frame; the report prints the count); `far` the whole tree;
/// `dense` sits over the page item at the height where a row is 1.5x the LOD threshold — the most
/// lines any frame can lay out.
fn camera_preset(name: &'static str, c: &Corpus, boxes: &[FileBox], target: (u32, u32), lod_px: f32) -> Camera {
    let n = c.items.len();
    let page = page_item(c);
    let area_box = |count: usize| {
        let (x0, x1, y0, y1) = bbox(boxes, 0..n);
        let frac = (count.min(n) as f32 / n as f32).sqrt();
        let aspect = target.0 as f32 / target.1 as f32;
        let side = ((x1 - x0) * (y1 - y0)).sqrt() * frac;
        let (hw, hh) = (side * aspect.sqrt() * 0.5, side / aspect.sqrt() * 0.5);
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        (cx - hw, cx + hw, cy - hh, cy + hh)
    };
    match name {
        "page" => {
            let (b, it) = (&boxes[page], &c.items[page]);
            let (lo, cnt) = page_rows(it);
            let y1 = b.oy - lo as f32 * ROW_H + ROW_H;
            Camera::frame_box(name, b.ox, b.ox + b.w.max(10.0), y1 - cnt as f32 * ROW_H, y1, target, lod_px, false, COLOR_CODE)
        }
        "overview" => { let (x0, x1, y0, y1) = area_box(OVERVIEW_FILES); Camera::frame_box(name, x0, x1, y0, y1, target, lod_px, true, COLOR_CODE) }
        "zoomout" => { let (x0, x1, y0, y1) = area_box(ZOOMOUT_FILES); Camera::frame_box(name, x0, x1, y0, y1, target, lod_px, true, COLOR_CODE) }
        "far" => { let (x0, x1, y0, y1) = bbox(boxes, 0..n); Camera::frame_box(name, x0, x1, y0, y1, target, lod_px, true, COLOR_CODE) }
        "dense" => { let b = &boxes[page]; Camera::at_row_px(name, b.ox + b.w.min(SHELF_PITCH) * 0.5, b.oy + ROW_H - b.h * 0.5, lod_px.max(1.0) * 1.5, target, lod_px, true, COLOR_CODE) }
        other => panic!("camera preset {other}"),
    }
}

struct CpuCull { lines: Vec<u32>, backdrops: Vec<u32>, vis_files: u32, cand_lines: u32 }

/// The CPU twin of cull.wgsl: the same boxes, planes, LOD rule and per-line box, in the same f32 operations.
fn cpu_cull(c: &Corpus, boxes: &[FileBox], cam: &Camera) -> CpuCull {
    let mut out = CpuCull { lines: Vec::new(), backdrops: Vec::new(), vis_files: 0, cand_lines: 0 };
    let row_px = cam.row_px();
    for (i, b) in boxes.iter().enumerate() {
        if b.line_count == 0 { continue }
        let y1 = b.oy + ROW_H;
        if !cam.box_visible([b.ox, y1 - b.h, -b.depth], [b.ox + b.w, y1, 0.0]) { continue }
        if row_px < cam.u.lod_px { out.backdrops.push(i as u32); continue }
        out.vis_files += 1;
        out.cand_lines += b.line_count;
        let it = &c.items[i];
        for row in 0..b.line_count {
            let li = it.first_line + row;
            let glyphs = c.line_glyphs(li, it);
            let w = b.w.min(glyphs.min(WRAP_COLS) as f32 * MAX_ADV);
            let y1 = b.oy - row as f32 * ROW_H + ROW_H;
            if cam.box_visible([b.ox, y1 - ROW_H, -b.depth], [b.ox + w, y1, 0.0]) { out.lines.push(li) }
        }
    }
    out
}

/// (item, file-local byte) -> transient slot, from the line table and the frame's segment list
/// alone: the line by binary search on byte_start, the covering segment, then the glyph-leading
/// bytes between the segment's start and the probe. `segs` is sorted by (line_idx, byte_start).
fn cpu_pick(c: &Corpus, segs: &[Seg], item: u32, byte: u32) -> Option<u32> {
    let it = &c.items[item as usize];
    let b = it.byte_start + byte;
    let lines = &c.lines[it.first_line as usize..(it.first_line + it.line_count) as usize];
    let li = it.first_line + lines.partition_point(|l| l.byte_start <= b) as u32 - 1;
    let k = segs.partition_point(|s| (s.line_idx, s.byte_start) <= (li, b));
    if k == 0 { return None }
    let s = &segs[k - 1];
    if s.line_idx != li || b >= s.byte_start + s.byte_len { return None }
    let leads = c.bytes[s.byte_start as usize..b as usize].iter().filter(|&&x| x & 0xC0 != 0x80).count() as u32;
    Some(s.slot_base + leads)
}

// ---------------------------------------------------------------- GPU

struct Gpu { device: wgpu::Device, queue: wgpu::Queue, ts_period: f32, adapter_info: wgpu::AdapterInfo, adapter_limits: wgpu::Limits }

fn init_gpu() -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("no adapter");
    assert!(adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY), "adapter lacks TIMESTAMP_QUERY");
    let al = adapter.limits();
    let required_limits = wgpu::Limits {
        max_storage_buffer_binding_size: al.max_storage_buffer_binding_size, max_buffer_size: al.max_buffer_size,
        max_storage_buffers_per_shader_stage: al.max_storage_buffers_per_shader_stage.min(64).max(17),
        ..wgpu::Limits::default()
    };
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

    fn buffer(&self, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(4), usage, mapped_at_creation: false })
    }

    fn init_buffer(&self, label: &str, contents: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        if contents.is_empty() { return self.buffer(label, 32, usage) }   // 32 B: a whole element of every stride bound here
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

    /// Copy `size` bytes out of `src` and hand them back (map_async + poll). A DEBUG path: nothing on the frame path calls it.
    fn read_back(&self, src: &wgpu::Buffer, size: u64) -> Vec<u8> {
        if size == 0 { return Vec::new() }
        let dst = self.buffer("readback", size, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(src, 0, &dst, 0, size);
        self.submit_wait(enc.finish());
        self.map_read(&dst, size)
    }

    fn read_back_as<T: Pod>(&self, src: &wgpu::Buffer, count: usize) -> Vec<T> {
        bytemuck::pod_collect_to_vec(&self.read_back(src, (count * std::mem::size_of::<T>()) as u64))
    }

    fn storage_entry(binding: u32, read_only: bool, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }
    fn uniform_entry(binding: u32, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry { binding, visibility: stages, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }
    }

    /// A bind group layout + group from (binding, buffer, kind) triples; kind: 'r' read-only storage, 'w' read-write storage, 'u' uniform.
    fn bind(&self, label: &str, stages: wgpu::ShaderStages, entries: &[(u32, &wgpu::Buffer, char)]) -> (wgpu::BindGroupLayout, wgpu::BindGroup) {
        let layout_entries: Vec<_> = entries.iter().map(|&(b, _, k)| match k { 'u' => Gpu::uniform_entry(b, stages), 'r' => Gpu::storage_entry(b, true, stages), _ => Gpu::storage_entry(b, false, stages) }).collect();
        let bgl = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &layout_entries });
        let group_entries: Vec<_> = entries.iter().map(|&(b, buf, _)| wgpu::BindGroupEntry { binding: b, resource: buf.as_entire_binding() }).collect();
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some(label), layout: &bgl, entries: &group_entries });
        (bgl, bg)
    }

    fn compute_pipeline(&self, module: &wgpu::ShaderModule, bgl: &wgpu::BindGroupLayout, entry: &str) -> wgpu::ComputePipeline {
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(entry), bind_group_layouts: &[Some(bgl)], immediate_size: 0 });
        self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(entry), layout: Some(&layout), module, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
    }
}

/// The resident set: the corpus bytes (in up to four chunks), the line table, the item table, the
/// file boxes, the seed table, the span table + per-line index + palette, the dense arrays.
struct Resident { chunks: Vec<wgpu::Buffer>, lines: wgpu::Buffer, items: wgpu::Buffer, boxes: wgpu::Buffer, seed_dir: wgpu::Buffer, seeds: wgpu::Buffer, spans: wgpu::Buffer, line_span_idx: wgpu::Buffer, palette: wgpu::Buffer, dense_color: wgpu::Buffer, dense_xf: wgpu::Buffer }

struct UploadTimes { base_create_ms: f64, base_submit_ms: f64, extra_create_ms: f64, extra_submit_ms: f64 }

fn upload_resident(gpu: &Gpu, c: &Corpus, boxes: &[FileBox], chunk_size: usize) -> (Resident, UploadTimes) {
    use wgpu::BufferUsages as U;
    let t = Instant::now();
    let chunks = c.bytes.chunks(chunk_size).map(|ch| gpu.init_buffer("bytes", ch, U::STORAGE)).collect();
    let lines = gpu.init_buffer("lines", bytemuck::cast_slice(&c.lines), U::STORAGE);
    let items = gpu.init_buffer("items", bytemuck::cast_slice(&c.items), U::STORAGE | U::COPY_DST);
    let base_create_ms = ms(t);
    let base_submit_ms = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
    let t = Instant::now();
    let boxes = gpu.init_buffer("boxes", bytemuck::cast_slice(boxes), U::STORAGE);
    let seed_dir = gpu.init_buffer("seed-dir", bytemuck::cast_slice(&c.seed_dir), U::STORAGE);
    let seeds = gpu.init_buffer("seeds", bytemuck::cast_slice(&c.seeds), U::STORAGE);
    let (spans, line_span_idx) = match &c.spans {
        Some(s) => (gpu.init_buffer("spans", bytemuck::cast_slice(&s.table), U::STORAGE | U::COPY_DST), gpu.init_buffer("line-span-idx", bytemuck::cast_slice(&s.line_idx), U::STORAGE | U::COPY_DST)),
        None => (gpu.buffer("spans", 32, U::STORAGE | U::COPY_DST), gpu.buffer("line-span-idx", 32, U::STORAGE | U::COPY_DST)),
    };
    let palette = gpu.init_buffer("palette", bytemuck::cast_slice(&c.palette), U::STORAGE);
    let (dense_color, dense_xf) = match &c.dense {
        Some(d) => (gpu.init_buffer("dense-color", bytemuck::cast_slice(&d.color), U::STORAGE | U::COPY_DST), gpu.init_buffer("dense-xf", bytemuck::cast_slice(&d.xf), U::STORAGE | U::COPY_DST)),
        None => (gpu.buffer("dense-color", 32, U::STORAGE | U::COPY_DST), gpu.buffer("dense-xf", 32, U::STORAGE | U::COPY_DST)),
    };
    let extra_create_ms = ms(t);
    let extra_submit_ms = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
    (Resident { chunks, lines, items, boxes, seed_dir, seeds, spans, line_span_idx, palette, dense_color, dense_xf }, UploadTimes { base_create_ms, base_submit_ms, extra_create_ms, extra_submit_ms })
}

/// Everything else: the per-frame buffers, every pipeline, the offscreen target, the query set.
struct Resources {
    segs_buf: wgpu::Buffer, slots_buf: wgpu::Buffer, _slot_xf_buf: wgpu::Buffer, _params_buf: wgpu::Buffer, _cull_params_buf: wgpu::Buffer, cam_buf: wgpu::Buffer, origins_buf: wgpu::Buffer,
    _vis_buf: wgpu::Buffer, backdrops_buf: wgpu::Buffer, counters_buf: wgpu::Buffer, indirect_buf: wgpu::Buffer, queries_buf: wgpu::Buffer, results_buf: wgpu::Buffer,
    layout: wgpu::ComputePipeline, layout_bg: wgpu::BindGroup,
    cull: [wgpu::ComputePipeline; 4], cull_bg: wgpu::BindGroup, indirect_bg: wgpu::BindGroup,
    pick: wgpu::ComputePipeline, pick_bg: wgpu::BindGroup,
    glyphs: wgpu::RenderPipeline, backdrops: wgpu::RenderPipeline, render_bg: wgpu::BindGroup, target: wgpu::Texture, target_view: wgpu::TextureView,
    query_set: wgpu::QuerySet, ts_resolve: wgpu::Buffer, ts_read: wgpu::Buffer,
    cap_slots: usize, cap_segs: usize, n_items: u32, target_size: (u32, u32),
}

fn build_resources(gpu: &Gpu, res: &Resident, c: &Corpus, adv: &[f32; 256], cap_segs: usize, cap_slots: usize, chunk_size: usize, max_probes: usize, target_size: (u32, u32), segment_bytes: usize) -> Resources {
    use wgpu::{BufferUsages as U, ShaderStages as S};
    let d = &gpu.device;
    let n_items = c.items.len() as u32;
    let has_xf = c.items.iter().any(|it| it.repr == 3);
    let adv_buf = gpu.init_buffer("advances", bytemuck::cast_slice(adv), U::STORAGE);
    let segs_buf = gpu.buffer("segments", (cap_segs as u64) * 32, U::STORAGE | U::COPY_DST | U::COPY_SRC);
    let slots_buf = gpu.buffer("slots", (cap_slots as u64) * SLOT_BYTES, U::STORAGE | U::COPY_SRC);
    let slot_xf_buf = gpu.buffer("slot-xf", if has_xf { (cap_slots as u64) * 8 } else { 32 }, U::STORAGE | U::COPY_SRC);
    let params_buf = gpu.init_buffer("params", bytemuck::bytes_of(&Params { wrap_cols: WRAP_COLS, chunk_shift: chunk_size.trailing_zeros(), chunk_mask: (1u32 << chunk_size.trailing_zeros()).wrapping_sub(1), default_color: COLOR_CODE, _pad: [0; 4] }), U::UNIFORM);
    let cull_params_buf = gpu.init_buffer("cull-params", bytemuck::bytes_of(&CullParams { n_items, segment_bytes: segment_bytes as u32, wrap_cols: WRAP_COLS, cap_segs: cap_segs as u32, cap_slots: cap_slots as u32, _pad: [0; 3] }), U::UNIFORM);
    let cam_buf = gpu.buffer("cam", std::mem::size_of::<CameraU>() as u64, U::UNIFORM | U::COPY_DST);
    let origins_buf = gpu.buffer("origins", (n_items as u64) * 8, U::STORAGE | U::COPY_DST);
    let vis_buf = gpu.buffer("vis-files", (n_items as u64) * 16, U::STORAGE | U::COPY_SRC);
    let backdrops_buf = gpu.buffer("backdrops", (n_items as u64) * 4, U::STORAGE | U::COPY_DST | U::COPY_SRC);
    let counters_buf = gpu.buffer("counters", 32, U::STORAGE | U::COPY_DST | U::COPY_SRC);
    let indirect_buf = gpu.buffer("indirect", 64, U::STORAGE | U::INDIRECT | U::COPY_SRC);
    let queries_buf = gpu.buffer("pick-queries", (max_probes as u64) * 8, U::STORAGE | U::COPY_DST);
    let results_buf = gpu.buffer("pick-results", (max_probes as u64) * 4, U::STORAGE | U::COPY_DST | U::COPY_SRC);
    let dummy = gpu.buffer("dummy-chunk", 4, U::STORAGE);
    let chunk = |i: usize| res.chunks.get(i).unwrap_or(&dummy);

    let layout_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("layout.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("layout.wgsl").into()) });
    let (layout_bgl, layout_bg) = gpu.bind("layout", S::COMPUTE, &[
        (0, chunk(0), 'r'), (1, chunk(1), 'r'), (2, chunk(2), 'r'), (3, chunk(3), 'r'), (4, &res.items, 'r'), (5, &segs_buf, 'r'), (6, &adv_buf, 'r'), (7, &slots_buf, 'w'), (8, &params_buf, 'u'),
        (9, &counters_buf, 'w'), (10, &res.lines, 'r'), (11, &res.spans, 'r'), (12, &res.line_span_idx, 'r'), (13, &res.palette, 'r'), (14, &res.dense_color, 'r'), (15, &res.dense_xf, 'r'), (16, &slot_xf_buf, 'w'),
    ]);
    let layout = gpu.compute_pipeline(&layout_module, &layout_bgl, "layout_segments");

    let cull_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cull.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("cull.wgsl").into()) });
    let (cull_bgl, cull_bg) = gpu.bind("cull", S::COMPUTE, &[
        (0, &cam_buf, 'u'), (1, &cull_params_buf, 'u'), (2, &res.boxes, 'r'), (3, &res.items, 'r'), (4, &res.lines, 'r'), (5, &res.seed_dir, 'r'), (6, &res.seeds, 'r'),
        (7, &vis_buf, 'w'), (8, &backdrops_buf, 'w'), (9, &segs_buf, 'w'), (10, &counters_buf, 'w'),
    ]);
    let (indirect_bgl, indirect_bg) = gpu.bind("indirect", S::COMPUTE, &[(0, &indirect_buf, 'w')]);
    let two = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("cull+indirect"), bind_group_layouts: &[Some(&cull_bgl), Some(&indirect_bgl)], immediate_size: 0 });
    let with_indirect = |e: &str| d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some(e), layout: Some(&two), module: &cull_module, entry_point: Some(e), compilation_options: Default::default(), cache: None });
    let cull = [gpu.compute_pipeline(&cull_module, &cull_bgl, "cull_files"), with_indirect("prefix_lines"), gpu.compute_pipeline(&cull_module, &cull_bgl, "cull_lines"), with_indirect("finalize")];

    let pick_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pick.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("pick.wgsl").into()) });
    let (pick_bgl, pick_bg) = gpu.bind("pick", S::COMPUTE, &[
        (0, chunk(0), 'r'), (1, chunk(1), 'r'), (2, chunk(2), 'r'), (3, chunk(3), 'r'), (4, &res.items, 'r'), (5, &res.lines, 'r'), (6, &segs_buf, 'r'), (7, &counters_buf, 'w'), (8, &params_buf, 'u'), (9, &queries_buf, 'r'), (10, &results_buf, 'w'),
    ]);
    let pick = gpu.compute_pipeline(&pick_module, &pick_bgl, "pick");

    let draw_module = d.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("draw.wgsl"), source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()) });
    let (render_bgl, render_bg) = gpu.bind("draw", S::VERTEX, &[(0, &slots_buf, 'r'), (1, &cam_buf, 'u'), (2, &origins_buf, 'r'), (3, &res.boxes, 'r'), (4, &backdrops_buf, 'r'), (5, &res.items, 'r'), (6, &slot_xf_buf, 'r')]);
    let render_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&render_bgl)], immediate_size: 0 });
    let render_pipeline = |entry: &str| d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry), layout: Some(&render_layout),
        vertex: wgpu::VertexState { module: &draw_module, entry_point: Some(entry), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(), depth_stencil: None, multisample: Default::default(),
        fragment: Some(wgpu::FragmentState { module: &draw_module, entry_point: Some("fs_main"), compilation_options: Default::default(), targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())] }),
        multiview_mask: None, cache: None,
    });
    let (glyphs, backdrops) = (render_pipeline("vs_main"), render_pipeline("vs_backdrop"));
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"), size: wgpu::Extent3d { width: target_size.0, height: target_size.1, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());

    let query_set = d.create_query_set(&wgpu::QuerySetDescriptor { label: Some("ts"), ty: wgpu::QueryType::Timestamp, count: 14 });
    let ts_resolve = gpu.buffer("ts-resolve", TS_SLOT * 7, U::QUERY_RESOLVE | U::COPY_SRC);
    let ts_read = gpu.buffer("ts-read", TS_SLOT * 7, U::MAP_READ | U::COPY_DST);
    Resources {
        segs_buf, slots_buf, _slot_xf_buf: slot_xf_buf, _params_buf: params_buf, _cull_params_buf: cull_params_buf, cam_buf, origins_buf, _vis_buf: vis_buf, backdrops_buf, counters_buf, indirect_buf, queries_buf, results_buf,
        layout, layout_bg, cull, cull_bg, indirect_bg, pick, pick_bg, glyphs, backdrops, render_bg, target, target_view, query_set, ts_resolve, ts_read,
        cap_slots, cap_segs, n_items, target_size,
    }
}

/// One frame's measurements. `pass_ms`: cull A, prefix, cull B, finalize, layout, draw; a pass that did not run reads 0.
#[derive(Clone, Copy, Debug, Default)]
struct Frame { cpu_ms: f64, wall_ms: f64, pass_ms: [f64; 6], counts: Counts, bytes_read: u64 }

const TS_SLOT: u64 = 256;

/// Which timestamp pairs were written (query index = 2 * pass).
struct Timing<'a> { set: &'a wgpu::QuerySet, used: [bool; 7] }

fn ts_writes<'a>(set: &'a wgpu::QuerySet, pass: usize) -> wgpu::ComputePassTimestampWrites<'a> {
    wgpu::ComputePassTimestampWrites { query_set: set, beginning_of_pass_write_index: Some(2 * pass as u32), end_of_pass_write_index: Some(2 * pass as u32 + 1) }
}

/// Encode the layout dispatch + the draw (backdrops then glyphs) after whatever produced the segment list.
/// `indirect` = read dispatch and draw arguments from the indirect buffer (GPU cull); else the CPU's counts.
fn encode_layout_and_draw(enc: &mut wgpu::CommandEncoder, r: &Resources, tm: &mut Timing, indirect: bool, segs: u32, slots: u32, backdrops: u32, draw: bool) {
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("layout"), timestamp_writes: Some(ts_writes(tm.set, 4)) });
        pass.set_pipeline(&r.layout);
        pass.set_bind_group(0, &r.layout_bg, &[]);
        if indirect { pass.dispatch_workgroups_indirect(&r.indirect_buf, 12) } else { let g = segs.div_ceil(64); pass.dispatch_workgroups(g.min(65535), g.div_ceil(65535), 1) }
        tm.used[4] = true;
    }
    if draw {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &r.target_view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.08, b: 0.10, a: 1.0 }), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: None,
            timestamp_writes: Some(wgpu::RenderPassTimestampWrites { query_set: tm.set, beginning_of_pass_write_index: Some(10), end_of_pass_write_index: Some(11) }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &r.render_bg, &[]);
        pass.set_pipeline(&r.backdrops);
        if indirect { pass.draw_indirect(&r.indirect_buf, 40) } else if backdrops > 0 { pass.draw(0..6, 0..backdrops) }
        pass.set_pipeline(&r.glyphs);
        if indirect { pass.draw_indirect(&r.indirect_buf, 24) } else { pass.draw(0..6, 0..slots) }
        tm.used[5] = true;
    }
}

/// Finish the encoder: resolve the written timestamp ranges, submit, wait; read the timestamps.
fn finish_frame(gpu: &Gpu, r: &Resources, mut enc: wgpu::CommandEncoder, tm: &Timing, t_cpu: Instant) -> (f64, f64, [f64; 6]) {
    // One 256 B resolve slot per pass (QUERY_RESOLVE_BUFFER_ALIGNMENT); only written pairs are resolved.
    for p in 0..6 { if tm.used[p] { enc.resolve_query_set(tm.set, 2 * p as u32..2 * p as u32 + 2, &r.ts_resolve, TS_SLOT * p as u64) } }
    enc.copy_buffer_to_buffer(&r.ts_resolve, 0, &r.ts_read, 0, TS_SLOT * 7);
    let cb = enc.finish();
    let t = Instant::now();
    gpu.queue.submit([cb]);
    let cpu_ms = ms(t_cpu);
    gpu.wait();
    let wall_ms = ms(t);
    let ts: Vec<u64> = bytemuck::pod_collect_to_vec(&gpu.map_read(&r.ts_read, TS_SLOT * 7));
    let tick = gpu.ts_period as f64 / 1e6;
    let mut pass_ms = [0f64; 6];
    for p in 0..6 { if tm.used[p] { pass_ms[p] = ts[p * 32 + 1].wrapping_sub(ts[p * 32]) as f64 * tick } }
    (cpu_ms, wall_ms, pass_ms)
}

/// The slot just past the whole line a segment belongs to (its line's base + its glyph count).
fn line_end_slot(c: &Corpus, s: &Seg) -> usize {
    (s.slot_base - s.col_seed) as usize + c.line_glyphs(s.line_idx, &c.items[s.item as usize]) as usize
}

/// A frame from a CPU-built line list (the synthetic views, and `--cull cpu`): segments + prefix built
/// here, uploaded with the counters and camera, then layout and draw. `backdrops` is the CPU cull's list.
fn run_frame_list(gpu: &Gpu, r: &Resources, c: &Corpus, lines: &[u32], cam: &CameraU, backdrops: &[u32], draw: bool) -> (Frame, Visible) {
    let t_cpu = Instant::now();
    let mut vis = visible_list(c, lines);
    // The caps the GPU cull applies: a line whose slots or segments would pass a cap is dropped, with every line after it.
    let (demand_slots, demand_segs) = (vis.slots, vis.segs.len());
    let mut dropped = 0u32;
    if vis.slots > r.cap_slots || vis.segs.len() > r.cap_segs {
        let keep = vis.segs.iter().position(|s| s.slot_base as usize >= r.cap_slots).unwrap_or(vis.segs.len()).min(r.cap_segs);
        // Back off to a line boundary whose whole line fits under the slot cap.
        let mut k = keep;
        while k > 0 && (vis.segs[k - 1].col_seed != 0 || line_end_slot(c, &vis.segs[k - 1]) > r.cap_slots) { k -= 1 }
        let kept_lines: HashSet<u32> = vis.segs[..k].iter().map(|s| s.line_idx).collect();
        dropped = (lines.len() - kept_lines.len()) as u32;
        vis.slots = vis.segs[..k].last().map(|s| line_end_slot(c, s)).unwrap_or(0);
        vis.segs.truncate(k);
    }
    let counts = Counts { vis_files: 0, backdrops: backdrops.len() as u32, cand_lines: 0, vis_lines: lines.len() as u32, segs: demand_segs as u32, slots: demand_slots as u32, dropped, draw_limit: vis.slots as u32 };
    let upload = Counts { segs: vis.segs.len() as u32, slots: vis.slots as u32, ..counts };
    gpu.queue.write_buffer(&r.segs_buf, 0, bytemuck::cast_slice(&vis.segs));
    gpu.queue.write_buffer(&r.counters_buf, 0, bytemuck::cast_slice(&upload.words()));
    gpu.queue.write_buffer(&r.cam_buf, 0, bytemuck::bytes_of(cam));
    if !backdrops.is_empty() { gpu.queue.write_buffer(&r.backdrops_buf, 0, bytemuck::cast_slice(backdrops)) }
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    let mut tm = Timing { set: &r.query_set, used: [false; 7] };
    encode_layout_and_draw(&mut enc, r, &mut tm, false, vis.segs.len() as u32, vis.slots as u32, backdrops.len() as u32, draw);
    let (cpu_ms, wall_ms, pass_ms) = finish_frame(gpu, r, enc, &tm, t_cpu);
    (Frame { cpu_ms, wall_ms, pass_ms, counts, bytes_read: vis.bytes_read }, vis)
}

/// A frame under `--cull cpu` with a camera: frustum + LOD on the CPU over the resident boxes, then the list path.
fn run_frame_cpu_cull(gpu: &Gpu, r: &Resources, c: &Corpus, boxes: &[FileBox], cam: &Camera, draw: bool) -> Frame {
    let t = Instant::now();
    let cull = cpu_cull(c, boxes, cam);
    let cull_ms = ms(t);
    let (mut f, _) = run_frame_list(gpu, r, c, &cull.lines, &cam.u, &cull.backdrops, draw);
    f.cpu_ms += cull_ms;
    f.pass_ms[0] = cull_ms;   // reported as the CPU's cull time in the "cull A" column
    f.counts.vis_files = cull.vis_files;
    f.counts.cand_lines = cull.cand_lines;
    f
}

/// A frame under `--cull gpu`: camera + counter reset uploaded, four cull passes, indirect layout, indirect draw.
/// No readback: the counts in the returned frame are zero; `read_counts` fetches them for the report.
fn run_frame_gpu_cull(gpu: &Gpu, r: &Resources, cam: &CameraU, draw: bool) -> Frame {
    let t_cpu = Instant::now();
    gpu.queue.write_buffer(&r.cam_buf, 0, bytemuck::bytes_of(cam));
    gpu.queue.write_buffer(&r.counters_buf, 0, bytemuck::cast_slice(&[0u32, 0, 0, 0, 0, 0, 0, r.cap_slots as u32]));
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    let mut tm = Timing { set: &r.query_set, used: [false; 7] };
    for (p, name) in ["cull A", "prefix", "cull B", "finalize"].iter().enumerate() {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(name), timestamp_writes: Some(ts_writes(tm.set, p)) });
        pass.set_pipeline(&r.cull[p]);
        pass.set_bind_group(0, &r.cull_bg, &[]);
        if p % 2 == 1 { pass.set_bind_group(1, &r.indirect_bg, &[]) }
        match p {
            0 => pass.dispatch_workgroups(r.n_items.div_ceil(64), 1, 1),
            2 => pass.dispatch_workgroups_indirect(&r.indirect_buf, 0),
            _ => pass.dispatch_workgroups(1, 1, 1),
        }
        tm.used[p] = true;
    }
    encode_layout_and_draw(&mut enc, r, &mut tm, true, 0, 0, 0, draw);
    let (cpu_ms, wall_ms, pass_ms) = finish_frame(gpu, r, enc, &tm, t_cpu);
    Frame { cpu_ms, wall_ms, pass_ms, counts: Counts::default(), bytes_read: 0 }
}

/// The debug readback of the counters after a GPU-cull frame (NOT on the frame path); returns (counts, ms).
fn read_counts(gpu: &Gpu, r: &Resources) -> (Counts, f64) {
    let t = Instant::now();
    let w: Vec<u32> = gpu.read_back_as(&r.counters_buf, 8);
    (Counts::from_words(&w), ms(t))
}

/// Place the view's items and fit the camera to what the kernel just emitted (one untimed frame + readback).
fn set_view_camera(gpu: &Gpu, r: &Resources, c: &Corpus, view: &View) -> Camera {
    let origins = item_origins(c, view, r.target_size);
    gpu.queue.write_buffer(&r.origins_buf, 0, bytemuck::cast_slice(&origins));
    let (f, _) = run_frame_list(gpu, r, c, &view.lines, &Camera::ortho_fit(0.0, 1.0, 0.0, 1.0, r.target_size, false, COLOR_CODE).u, &[], false);
    let slots: Vec<Slot> = gpu.read_back_as(&r.slots_buf, f.counts.slots as usize);
    fit_camera(&slots, &origins, view.tint, r.target_size)
}

/// The offscreen target as a PNG (RGB, no text of any kind in the image).
fn screenshot(gpu: &Gpu, r: &Resources, path: &Path) {
    let (w, h) = r.target_size;
    let pitch = (w * 4).next_multiple_of(256);
    let buf = gpu.buffer("shot", (pitch * h) as u64, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
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

/// GPU output of a line list against the single-threaded whole-line CPU fold, every slot, bit-exact
/// (colour by representation included), and the dense transform stream where an item has one.
fn verify_layout(gpu: &Gpu, r: &Resources, c: &Corpus, lines: &[u32], adv: &[f32; 256]) -> String {
    let (f, vis) = run_frame_list(gpu, r, c, lines, &Camera::ortho_fit(0.0, 1.0, 0.0, 1.0, r.target_size, false, COLOR_CODE).u, &[], false);
    let gpu_slots: Vec<Slot> = gpu.read_back_as(&r.slots_buf, f.counts.slots as usize);
    let (cpu_slots, cpu_xf) = cpu_reference(c, lines, adv);
    assert_eq!(gpu_slots.len(), cpu_slots.len());
    let xf_note = if cpu_xf.is_empty() { String::new() } else {
        let gpu_xf: Vec<[u32; 2]> = gpu.read_back_as(&r._slot_xf_buf, f.counts.slots as usize);
        let bad = cpu_xf.iter().filter(|(i, v)| gpu_xf[*i] != *v).count();
        if bad == 0 { format!("; {} dense transforms bit-equal", cpu_xf.len()) } else { return format!("FAIL: {bad} of {} dense transforms differ", cpu_xf.len()) }
    };
    match gpu_slots.iter().zip(&cpu_slots).position(|(a, b)| a != b) {
        None => format!("PASS: {} slots bit-equal GPU (from {} segments) vs whole-line CPU fold, x as f32 bits and colour included{xf_note}", f.counts.slots, vis.segs.len()),
        Some(i) => {
            let seg = vis.segs.iter().rev().find(|s| (s.slot_base as usize) <= i).unwrap();
            format!("FAIL: first mismatch at slot {i} (segment byte_start {} col_seed {} x_seed {}): gpu {:?} cpu {:?}; {} of {} differ", seg.byte_start, seg.col_seed, seg.x_seed, gpu_slots[i], cpu_slots[i], gpu_slots.iter().zip(&cpu_slots).filter(|(a, b)| a != b).count(), f.counts.slots)
        }
    }
}

/// The GPU cull's visible set (segments read back once) against the CPU frustum test over the same boxes, as sets.
fn verify_cull(gpu: &Gpu, r: &Resources, c: &Corpus, boxes: &[FileBox], cam: &Camera) -> String {
    run_frame_gpu_cull(gpu, r, &cam.u, false);
    let (k, _) = read_counts(gpu, r);
    let gpu_segs: Vec<Seg> = gpu.read_back_as(&r.segs_buf, k.segs.min(r.cap_segs as u32) as usize);
    let gpu_back: Vec<u32> = gpu.read_back_as(&r.backdrops_buf, k.backdrops as usize);
    let cpu = cpu_cull(c, boxes, cam);
    if k.dropped > 0 {
        let demand: u64 = cpu.lines.iter().map(|&li| c.line_glyphs(li, &c.items[c.item_of(li)]) as u64).sum();
        let ok = k.vis_lines as usize == cpu.lines.len() && k.vis_files == cpu.vis_files && k.cand_lines == cpu.cand_lines && k.backdrops as usize == cpu.backdrops.len();
        return format!("{}: camera {}: CAP BOUND ({} of {} visible lines dropped; slot demand {} vs cap {}) — which lines survive depends on atomic order, so only the COUNTS are compared: GPU {} files / {} candidate / {} visible lines vs CPU {} / {} / {}; slot demand GPU {} (u32, wraps past 4 G) vs CPU {}",
            if ok { "PASS (counts)" } else { "FAIL" }, cam.name, k.dropped, k.vis_lines, k.slots, r.cap_slots, k.vis_files, k.cand_lines, k.vis_lines, cpu.vis_files, cpu.cand_lines, cpu.lines.len(), k.slots, demand);
    }
    let cpu_vis = visible_list(c, &cpu.lines);
    let key = |s: &Seg| (s.byte_start, s.byte_len, s.line_idx, s.item, s.col_seed, s.x_seed.to_bits(), s.state);
    let gpu_lines: HashSet<u32> = gpu_segs.iter().map(|s| s.line_idx).collect();
    let cpu_lines: HashSet<u32> = cpu.lines.iter().copied().collect();
    let mut gk: Vec<_> = gpu_segs.iter().map(key).collect();
    let mut ck: Vec<_> = cpu_vis.segs.iter().map(key).collect();
    gk.sort();
    ck.sort();
    let gb: HashSet<u32> = gpu_back.iter().copied().collect();
    let cb: HashSet<u32> = cpu.backdrops.iter().copied().collect();
    let ok = gpu_lines == cpu_lines && gk == ck && gb == cb && k.vis_files == cpu.vis_files && k.cand_lines == cpu.cand_lines && k.slots as usize == cpu_vis.slots && k.dropped == 0;
    if ok {
        format!("PASS: camera {}: GPU cull == CPU frustum test over the same boxes — {} visible files, {} backdrops, {} candidate lines, {} visible lines, {} segments (as a set, slot_base excluded: order is atomic), {} slots", cam.name, k.vis_files, k.backdrops, k.cand_lines, cpu_lines.len(), gk.len(), k.slots)
    } else {
        let only_gpu = gpu_lines.difference(&cpu_lines).count();
        let only_cpu = cpu_lines.difference(&gpu_lines).count();
        format!("FAIL: camera {}: GPU {:?} vs CPU files {} cand {} lines {} segs {} slots {} backdrops {}; lines only on GPU {only_gpu}, only on CPU {only_cpu}; segment keys equal {}; backdrop sets equal {}", cam.name, k, cpu.vis_files, cpu.cand_lines, cpu_lines.len(), ck.len(), cpu_vis.slots, cb.len(), gk == ck, gb == cb)
    }
}

/// Picking: `probes` random (file, byte) pairs over the frame's visible segments; CPU pick (line table +
/// segment list, no per-glyph storage) vs the GPU pick micro-kernel vs the slot the readback holds there.
fn verify_pick(gpu: &Gpu, r: &Resources, c: &Corpus, probes: usize, cull_name: &str) -> String {
    let (k, _) = read_counts(gpu, r);
    let mut segs: Vec<Seg> = gpu.read_back_as(&r.segs_buf, k.segs.min(r.cap_segs as u32) as usize);
    segs.retain(|s| s.byte_len > 0);
    if segs.is_empty() { return format!("SKIP: no visible segments under {cull_name}") }
    let slots: Vec<Slot> = gpu.read_back_as(&r.slots_buf, k.slots.min(k.draw_limit) as usize);
    let mut rng = Lcg(0xD1B54A32D192ED03);
    let mut queries = Vec::with_capacity(probes);
    for _ in 0..probes {
        let s = &segs[rng.next(segs.len())];
        let mut b = s.byte_start + rng.next(s.byte_len as usize) as u32;
        while c.bytes[b as usize] & 0xC0 == 0x80 { b -= 1 }
        queries.push(Query { item: s.item, byte: b - c.items[s.item as usize].byte_start });
    }
    segs.sort_by_key(|s| (s.line_idx, s.byte_start));
    let t = Instant::now();
    let cpu: Vec<Option<u32>> = queries.iter().map(|q| cpu_pick(c, &segs, q.item, q.byte)).collect();
    let cpu_ms = ms(t);
    // GPU: all probes in one dispatch (a workgroup each), then one probe alone, both timed.
    let run = |n: usize| -> (Vec<u32>, f64, f64) {
        gpu.queue.write_buffer(&r.queries_buf, 0, bytemuck::cast_slice(&queries[..n]));
        gpu.queue.write_buffer(&r.results_buf, 0, bytemuck::cast_slice(&vec![u32::MAX; n]));
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pick"), timestamp_writes: Some(ts_writes(&r.query_set, 6)) });
            pass.set_pipeline(&r.pick);
            pass.set_bind_group(0, &r.pick_bg, &[]);
            pass.dispatch_workgroups(n as u32, 1, 1);
        }
        enc.resolve_query_set(&r.query_set, 12..14, &r.ts_resolve, TS_SLOT * 6);
        enc.copy_buffer_to_buffer(&r.ts_resolve, TS_SLOT * 6, &r.ts_read, TS_SLOT * 6, 16);
        let wall = gpu.submit_wait(enc.finish());
        let ts: Vec<u64> = bytemuck::pod_collect_to_vec(&gpu.map_read(&r.ts_read, TS_SLOT * 7));
        let gpu_ms = ts[6 * 32 + 1].wrapping_sub(ts[6 * 32]) as f64 * gpu.ts_period as f64 / 1e6;
        (gpu.read_back_as(&r.results_buf, n), gpu_ms, wall)
    };
    let (gpu_all, gpu_all_ms, _) = run(probes);
    let (_, gpu_one_ms, one_wall) = run(1);
    let mut agree = 0usize;
    let mut slot_ok = 0usize;
    let mut first_bad = None;
    for (i, q) in queries.iter().enumerate() {
        let g = gpu_all[i];
        let same = cpu[i] == Some(g);
        agree += usize::from(same);
        let it = &c.items[q.item as usize];
        let abs = (it.byte_start + q.byte) as usize;
        let mut f = Fold::new();
        let (glyph, _, _) = f.decode(&c.bytes, abs, &advance_table());
        let li = it.first_line + c.lines[it.first_line as usize..(it.first_line + it.line_count) as usize].partition_point(|l| l.byte_start as usize <= abs) as u32 - 1;
        let ok = same && (g as usize) < slots.len() && slots[g as usize].item == q.item && slots[g as usize].row == li - it.first_line && slots[g as usize].glyph_and_wrap & 0xFFFF == glyph;
        slot_ok += usize::from(ok);
        if !ok && first_bad.is_none() { first_bad = Some((i, cpu[i], g)) }
    }
    let timing = format!("CPU pick {:.3} ms for {probes} ({:.1} us each); GPU pick kernel {:.3} ms for {probes} probes in one dispatch, {:.3} ms GPU / {:.3} ms submit..poll for one probe over {} segments", cpu_ms, cpu_ms * 1e3 / probes as f64, gpu_all_ms, gpu_one_ms, one_wall, k.segs);
    if slot_ok == probes {
        format!("PASS: {probes} random (file, byte) probes under {cull_name}: CPU pick == GPU pick, and the slot there has the probe's item, row and glyph; {} probes in dense items. {timing}", queries.iter().filter(|q| c.items[q.item as usize].repr >= 2).count())
    } else {
        format!("FAIL: {slot_ok} of {probes} probes resolve ({agree} CPU==GPU); first bad {first_bad:?}. {timing}")
    }
}

/// The edit measurements: replace one file's spans (in place if the new count fits its capacity, else
/// appended at the table's tail and remapped) and, if dense, its dense arrays. Updates the CPU tables too.
fn measure_edits(gpu: &Gpu, res: &Resident, c: &mut Corpus, item: usize) -> Vec<String> {
    let mut out = Vec::new();
    let it = c.items[item];
    let rel = c.files[item].rel.clone();
    if let Some(sp) = c.spans.as_mut() {
        let t = Instant::now();
        let mut new = Vec::new();
        spans::demo_spans(&c.bytes[it.byte_start as usize..(it.byte_start + it.byte_len) as usize], 1, &mut new);
        let gen_ms = ms(t);
        let t = Instant::now();
        let fits = new.len() <= sp.cap[item] as usize;
        let base = if fits { it.span_base as usize } else {
            let b = sp.tail;
            let need = spans::capacity_for(new.len());
            assert!(b + need <= sp.table.len(), "span table headroom exhausted");
            sp.tail += need;
            sp.cap[item] = need as u32;
            b
        };
        sp.table[base..base + new.len()].copy_from_slice(&new);
        let mut idx = vec![0u32; it.line_count as usize];
        spans::line_index(&new, base as u32, &it, &c.lines, &mut idx);
        sp.line_idx[it.first_line as usize..(it.first_line + it.line_count) as usize].copy_from_slice(&idx);
        c.items[item].span_base = base as u32;
        c.items[item].span_count = new.len() as u32;
        let cpu_ms = ms(t);
        let t = Instant::now();
        gpu.queue.write_buffer(&res.spans, base as u64 * 8, bytemuck::cast_slice(&new));
        gpu.queue.write_buffer(&res.line_span_idx, it.first_line as u64 * 4, bytemuck::cast_slice(&idx));
        gpu.queue.write_buffer(&res.items, item as u64 * 48, bytemuck::bytes_of(&c.items[item]));
        let submit_ms = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
        let total_ms = ms(t);
        out.push(format!("span edit {rel}: {} spans -> {} ({}; capacity {}); generate {gen_ms:.3} ms (the analysis, not the edit); CPU rebuild of {} per-line indices + table copy {cpu_ms:.3} ms; write_buffer {} B spans + {} B index + 48 B item, submit+poll {submit_ms:.3} ms ({total_ms:.3} ms total GPU side)",
            it.span_count, new.len(), if fits { "in place" } else { "APPENDED at the tail and remapped; the old range is a hole" }, sp.cap[item], it.line_count, new.len() * 8, idx.len() * 4));
    }
    if it.repr >= 2 {
        let d = c.dense.as_mut().unwrap();
        let t = Instant::now();
        let n = it.glyph_count as usize;
        let base = it.dense_base as usize;
        let color: Vec<u32> = (0..n as u32).map(|o| spans::dense_color_for(o, 0x5EED)).collect();
        let xf: Vec<[u32; 2]> = if it.repr == 3 { (0..n as u32).map(|o| spans::dense_xf_for(o, 0x5EED)).collect() } else { vec![] };
        d.color[base..base + n].copy_from_slice(&color);
        if it.repr == 3 { d.xf[base..base + n].copy_from_slice(&xf) }
        let cpu_ms = ms(t);
        let t = Instant::now();
        gpu.queue.write_buffer(&res.dense_color, base as u64 * 4, bytemuck::cast_slice(&color));
        if it.repr == 3 { gpu.queue.write_buffer(&res.dense_xf, base as u64 * 8, bytemuck::cast_slice(&xf)) }
        let submit_ms = gpu.submit_wait(gpu.device.create_command_encoder(&Default::default()).finish());
        let total_ms = ms(t);
        out.push(format!("dense edit {rel}: {n} glyphs x {} B rewritten in place (fixed per-glyph capacity: no fragmentation, no remap); CPU generate+copy {cpu_ms:.3} ms; write_buffer {} B, submit+poll {submit_ms:.3} ms ({total_ms:.3} ms total GPU side)",
            if it.repr == 3 { 12 } else { 4 }, n * if it.repr == 3 { 12 } else { 4 }));
    }
    out
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
    println!("adapter: {} (backend {:?}, driver {} {}); timestamp period {} ns", gpu.adapter_info.name, gpu.adapter_info.backend, gpu.adapter_info.driver, gpu.adapter_info.driver_info, gpu.ts_period);
    println!("adapter limits: max_buffer_size {} ({:.0} MB), max_storage_buffer_binding_size {} ({:.0} MB), storage buffers per stage {}", al.max_buffer_size, mb(al.max_buffer_size), al.max_storage_buffer_binding_size, mb(al.max_storage_buffer_binding_size as u64), al.max_storage_buffers_per_shader_stage);
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
    let mut corpus = Corpus { bytes: w.bytes, files: w.files, lines: p1.lines, items: p1.items, widths: p1.widths, seed_dir: p1.seed_dir, seeds: p1.seeds, total_glyphs: p1.total_glyphs, segment_bytes: args.segment_bytes, spans: None, dense: None, palette: spans::palette() };
    let source_bytes: u64 = corpus.files.iter().map(|f| f.byte_len as u64).sum();
    println!("walk ({}): {} candidates, {} kept, {} skipped >10 MiB, {} non-UTF-8, {} unreadable; enumerate {:.0} ms, read+validate {:.0} ms ({} threads), concat {:.0} ms; {} B source ({} B chunk padding, {} chunk(s) of {:.0} MB)",
        if args.walk == WalkMode::Renderer { "renderer rules" } else { "all files" }, st.candidates, corpus.files.len(), st.skipped_large, st.skipped_non_utf8, st.read_errors, st.enumerate_ms, st.read_ms, args.threads, st.concat_ms, source_bytes, st.pad_bytes, n_chunks, mb(chunk_size as u64));
    println!("corpus: {} files, {} B, {} lines, {} glyphs; Pass 1 ({} threads) {:.1} ms; {} lines longer than {} B carry {} segment seeds",
        corpus.files.len(), source_bytes, corpus.line_count(), corpus.total_glyphs, args.threads, p1.wall_ms, corpus.seed_dir.len() - 1, args.segment_bytes, corpus.seeds.len());
    assert!(corpus.line_count() > 0, "no lines");
    let page = page_item(&corpus);
    println!("page item: {} ({} B, {} lines, {} glyphs)", corpus.files[page].rel, corpus.items[page].byte_len, corpus.items[page].line_count, corpus.items[page].glyph_count);

    // 2. Colour tables: dense items first (they keep their representation), then the demo spans for the rest.
    let mut dense_items: Vec<usize> = args.dense_files.iter().map(|rel| corpus.item_by_rel(rel).unwrap_or_else(|| panic!("--dense-file {rel}: not in the corpus"))).collect();
    if args.dense_sample > 0 {
        let sampled = overview_items(&corpus);
        let step = (sampled.len() / args.dense_sample).max(1);
        dense_items.extend(sampled.iter().step_by(step).take(args.dense_sample));
    }
    if args.dense_in_view > 0 {
        // N files spread evenly over those whose box the `overview` camera's frustum contains.
        let boxes = shelf_boxes(&corpus);
        let cam = camera_preset("overview", &corpus, &boxes, args.target, 0.0);
        let in_view: Vec<usize> = boxes.iter().enumerate().filter(|(_, b)| b.line_count > 0 && cam.box_visible([b.ox, b.oy + ROW_H - b.h, -b.depth], [b.ox + b.w, b.oy + ROW_H, 0.0])).map(|(i, _)| i).collect();
        let step = (in_view.len() as f64 / args.dense_in_view as f64).max(1.0);
        let picked: Vec<usize> = (0..args.dense_in_view.min(in_view.len())).map(|k| in_view[(k as f64 * step) as usize]).collect();
        println!("dense-in-view: {} of the {} files in the overview camera's frustum made dense", picked.len(), in_view.len());
        dense_items.extend(picked);
    }
    dense_items.sort();
    dense_items.dedup();
    if !dense_items.is_empty() {
        let d = spans::build_dense(&mut corpus.items, &dense_items, args.dense_xf, 0);
        println!("dense: {} items ({} glyphs) carry a per-glyph colour{}; {:.1} ms to materialise; {:.1} MB colour + {:.1} MB transforms resident",
            d.items.len(), d.color.len(), if args.dense_xf { " and transform" } else { "" }, d.build_ms, mb(d.color.len() as u64 * 4), mb(d.xf.len() as u64 * 8));
        corpus.dense = Some(d);
    }
    if args.spans {
        let sp = spans::build_demo(&corpus.bytes, &corpus.files, &mut corpus.items, &corpus.lines, args.threads);
        println!("spans (demo): {} spans over {} files ({:.2} spans per 10 B of source); table {:.1} MB used, {:.1} MB allocated with 12.5%+16 slack per file, {:.1} MB tail headroom; per-line index {:.1} MB (4 B/line); {:.0} ms to generate + index on {} threads",
            sp.used, corpus.items.iter().filter(|it| it.repr == 1).count(), sp.used as f64 * 10.0 / source_bytes as f64, mb(sp.used as u64 * 8), mb(sp.tail as u64 * 8), mb((sp.table.len() - sp.tail) as u64 * 8), mb(sp.line_idx.len() as u64 * 4), sp.build_ms, args.threads);
        assert!((sp.table.len() as u64 * 8) <= al.max_storage_buffer_binding_size as u64, "span table {} B exceeds the binding limit", sp.table.len() * 8);
        corpus.spans = Some(sp);
    }

    // 3. Views and camera presets, and the caps.
    let views = plan_views(&corpus, &args);
    let boxes = shelf_boxes(&corpus);
    let cameras: Vec<Camera> = args.cameras.iter().map(|n| camera_preset(n, &corpus, &boxes, args.target, args.lod_px)).collect();
    let visibles: Vec<Visible> = views.iter().map(|v| visible_list(&corpus, &v.lines)).collect();
    let view_slots = visibles.iter().map(|v| v.slots).max().unwrap_or(0);
    let view_segs = visibles.iter().map(|v| v.segs.len()).max().unwrap_or(0);
    drop(visibles);
    let (cap_slots, cap_segs) = if cameras.is_empty() { (view_slots.max(1), view_segs.max(1)) } else { (view_slots.max(args.max_slots), view_segs.max(args.max_segments)) };
    let lim = gpu.device.limits();
    assert!((cap_slots as u64) * SLOT_BYTES <= lim.max_storage_buffer_binding_size as u64, "slot cap needs {} B, binding limit {}", cap_slots as u64 * SLOT_BYTES, lim.max_storage_buffer_binding_size);
    if !cameras.is_empty() {
        let (x0, x1, y0, y1) = bbox(&boxes, 0..boxes.len());
        println!("shelf: {} files in {} columns of pitch {SHELF_PITCH}; world {:.0} x {:.0} units; widest file {:.1} units; transient caps {} slots ({:.0} MB) / {} segments ({:.0} MB)",
            boxes.len(), (boxes.last().map(|b| b.ox).unwrap_or(0.0) / SHELF_PITCH) as u32 + 1, x1 - x0, y1 - y0, corpus.widths.iter().cloned().fold(0f32, f32::max), cap_slots, mb(cap_slots as u64 * SLOT_BYTES), cap_segs, mb(cap_segs as u64 * 32));
        for cam in &cameras { println!("camera {:<9} eye ({:.0}, {:.0}) height {:.0}; one row = {:.3} px (LOD threshold {} px){}", cam.name, cam.u.eye[0], cam.u.eye[1], cam.dist, cam.row_px(), args.lod_px, if cam.row_px() < args.lod_px { " -> every file is a backdrop" } else { "" }) }
    }

    // 4. Upload the resident set, repeated.
    let mut times: Vec<UploadTimes> = vec![];
    let mut resident = None;
    for _ in 0..args.repeat {
        drop(resident.take());
        gpu.wait();
        let (r, t) = upload_resident(&gpu, &corpus, &boxes, chunk_size);
        times.push(t);
        resident = Some(r);
    }
    let resident = resident.unwrap();
    let res = build_resources(&gpu, &resident, &corpus, &adv, cap_segs, cap_slots, chunk_size, args.probes.max(1), args.target, args.segment_bytes);

    // 5. The interleaved measurement loop: Pass 1 at every thread count, one frame per view, one frame per (camera, cull mode).
    let mut thread_set: Vec<usize> = if args.pass1_scaling { PASS1_THREAD_SET.to_vec() } else { vec![] };
    thread_set.push(args.threads);
    thread_set.sort();
    thread_set.dedup();
    let mut pass1_ms: Vec<Vec<f64>> = vec![vec![]; thread_set.len()];
    let mut frames: Vec<Vec<Frame>> = (0..views.len()).map(|_| Vec::new()).collect();
    let cam_runs: Vec<(usize, Cull)> = cameras.iter().enumerate().flat_map(|(ci, _)| args.cull.iter().map(move |&m| (ci, m))).collect();
    let mut cam_frames: Vec<Vec<Frame>> = (0..cam_runs.len()).map(|_| Vec::new()).collect();
    let shelf_origins: Vec<[f32; 2]> = boxes.iter().map(|b| [b.ox, b.oy]).collect();
    for rep in 0..args.repeat {
        for (ti, &t) in thread_set.iter().enumerate() {
            let p = pass1(&corpus.bytes, &corpus.files, t, args.segment_bytes, &adv);
            assert_eq!(p.lines.len(), corpus.lines.len(), "Pass 1 at {t} threads disagrees on the line count");
            assert_eq!(p.total_glyphs, corpus.total_glyphs);
            pass1_ms[ti].push(p.wall_ms);
        }
        for (vi, view) in views.iter().enumerate() {
            let cam = if args.draw { set_view_camera(&gpu, &res, &corpus, view) } else { Camera::ortho_fit(0.0, 1.0, 0.0, 1.0, args.target, false, COLOR_CODE) };
            frames[vi].push(run_frame_list(&gpu, &res, &corpus, &view.lines, &cam.u, &[], args.draw).0);
            if rep + 1 == args.repeat && cameras.is_empty() {
                if let Some(path) = &args.screenshot { screenshot(&gpu, &res, path) }
            }
        }
        if !cam_runs.is_empty() { gpu.queue.write_buffer(&res.origins_buf, 0, bytemuck::cast_slice(&shelf_origins)) }
        for (ri, &(ci, mode)) in cam_runs.iter().enumerate() {
            let f = match mode { Cull::Cpu => run_frame_cpu_cull(&gpu, &res, &corpus, &boxes, &cameras[ci], args.draw), Cull::Gpu => run_frame_gpu_cull(&gpu, &res, &cameras[ci].u, args.draw) };
            cam_frames[ri].push(f);
            if rep + 1 == args.repeat && ri + 1 == cam_runs.len() {
                if let Some(path) = &args.screenshot { screenshot(&gpu, &res, path) }
            }
        }
    }

    // 6. Debug readbacks for the GPU-cull counts (timed, off the frame path), and the checks.
    let mut cam_counts: Vec<(Counts, f64)> = Vec::new();
    for &(ci, mode) in &cam_runs {
        match mode {
            Cull::Gpu => { run_frame_gpu_cull(&gpu, &res, &cameras[ci].u, false); cam_counts.push(read_counts(&gpu, &res)) }
            Cull::Cpu => cam_counts.push((cam_frames[cam_counts.len()][0].counts, 0.0)),
        }
    }
    let verdicts: Vec<(String, String)> = views.iter().filter(|v| v.verify).map(|v| (v.name.clone(), verify_layout(&gpu, &res, &corpus, &v.lines, &adv))).collect();
    let page_verdict = if corpus.spans.is_some() || corpus.dense.is_some() { Some(verify_layout(&gpu, &res, &corpus, &page_view(&corpus).lines, &adv)) } else { None };
    let cull_verdicts: Vec<String> = cameras.iter().map(|cam| verify_cull(&gpu, &res, &corpus, &boxes, cam)).collect();
    // Picking over the camera frame with the most visible lines (GPU cull), else the page view's list.
    let pick_verdict = if let Some((ci, _)) = cam_runs.iter().zip(&cam_counts).filter(|(r, _)| r.1 == Cull::Gpu).max_by_key(|(_, (k, _))| k.vis_lines).map(|(r, _)| *r) {
        run_frame_gpu_cull(&gpu, &res, &cameras[ci].u, false);
        verify_pick(&gpu, &res, &corpus, args.probes, &format!("--cull gpu, camera {}", cameras[ci].name))
    } else {
        let v = page_view(&corpus);
        run_frame_list(&gpu, &res, &corpus, &v.lines, &Camera::ortho_fit(0.0, 1.0, 0.0, 1.0, args.target, false, COLOR_CODE).u, &[], false);
        verify_pick(&gpu, &res, &corpus, args.probes, "the page view's CPU list")
    };
    // Edits last (they change the tables), then the page lines re-verified against the edited tables.
    let edit_item = corpus.item_by_rel(&args.edit_file).unwrap_or(page);
    let edits = measure_edits(&gpu, &resident, &mut corpus, edit_item);
    let post_edit = if edits.is_empty() { None } else {
        let it = corpus.items[edit_item];
        let (lo, n) = page_rows(&it);
        Some(verify_layout(&gpu, &res, &corpus, &(it.first_line + lo..it.first_line + lo + n).collect::<Vec<_>>(), &adv))
    };

    // ---------------------------------------------------------------- report
    let mm = |v: &[f64]| { let (a, b) = stats(v); format!("{a:9.3} {b:9.3}") };
    println!();
    println!("=== jit-layout: {} files, {} B source, {} lines, {} glyphs; {} repeats; min / median in ms ===", corpus.files.len(), source_bytes, corpus.line_count(), corpus.total_glyphs, args.repeat);
    println!();
    println!("--- Pass 1 scaling (line table 8 B/line + item table 48 B/item + file widths + long-line seeds; wall, this process alone) ---");
    println!("{:>8} {:>9} {:>9} {:>12} {:>12}", "threads", "min", "median", "MB/s@min", "MB/s@median");
    for (ti, &t) in thread_set.iter().enumerate() {
        let (a, b) = stats(&pass1_ms[ti]);
        println!("{:>8} {:9.1} {:9.1} {:12.0} {:12.0}", t, a, b, source_bytes as f64 / a / 1e3, source_bytes as f64 / b / 1e3);
    }
    println!("walk (once): enumerate {:.0} ms, read+validate {:.0} ms ({} threads, {:.0} MB/s), concat {:.0} ms", st.enumerate_ms, st.read_ms, args.threads, source_bytes as f64 / st.read_ms / 1e3, st.concat_ms);
    println!();
    println!("--- resident upload, create_buffer_init / submit+poll ---");
    println!("{:<60} {}", format!("bytes ({} chunk(s)) + line table + item table ({:.0} MB)", n_chunks, mb(corpus.bytes.len() as u64 + corpus.lines.len() as u64 * 8 + corpus.items.len() as u64 * 48)), mm(&times.iter().map(|t| t.base_create_ms).collect::<Vec<_>>()));
    println!("{:<60} {}", "  submit + poll", mm(&times.iter().map(|t| t.base_submit_ms).collect::<Vec<_>>()));
    println!("{:<60} {}", "boxes + seeds + spans + line span index + dense", mm(&times.iter().map(|t| t.extra_create_ms).collect::<Vec<_>>()));
    println!("{:<60} {}", "  submit + poll", mm(&times.iter().map(|t| t.extra_submit_ms).collect::<Vec<_>>()));
    println!();
    for (vi, view) in views.iter().enumerate() {
        let fr = &frames[vi];
        let col = |f: &dyn Fn(&Frame) -> f64| fr.iter().map(f).collect::<Vec<_>>();
        let slots = fr[0].counts.slots as usize;
        println!("--- view {} (CPU-chosen lines, no frustum test): {} lines, {} segments, {} slots, {} visible bytes, transient slots {:.1} MB ---", view.name, view.lines.len(), fr[0].counts.segs, slots, fr[0].bytes_read, mb(slots as u64 * SLOT_BYTES));
        println!("{:<44} {:>9} {:>9}", "measurement", "min", "median");
        let (min_c, med_c) = stats(&col(&|f| f.pass_ms[4]));
        println!("{:<44} {}   {:.2} / {:.2} G glyphs/s", "layout pass GPU", mm(&col(&|f| f.pass_ms[4])), slots as f64 / min_c / 1e6, slots as f64 / med_c / 1e6);
        if args.draw { println!("{:<44} {}", format!("draw pass GPU (flat quads, {}x{})", args.target.0, args.target.1), mm(&col(&|f| f.pass_ms[5]))) }
        println!("{:<44} {}", "CPU visible list + write_buffer + submit", mm(&col(&|f| f.cpu_ms)));
        println!("{:<44} {}", "frame wall (submit..poll)", mm(&col(&|f| f.wall_ms)));
        println!();
    }
    if !cam_runs.is_empty() {
        println!("--- camera presets: {} x {} target, LOD threshold {} px; GPU pass times from timestamp queries; CPU = list/cull + uploads + encode + submit; no readback on the frame path ---", args.target.0, args.target.1, args.lod_px);
        println!("what the view numbers above assumed: the visible list was CHOSEN on the CPU from a view definition (a line window, a file sample) with no frustum test; the camera rows below are culled.");
        println!();
        println!("{:<9} {:<4} {:>8} {:>8} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}", "camera", "cull", "files", "backdrop", "cand ln", "vis ln", "segs", "slots", "dropped", "", "cullA", "prefix", "cullB", "final", "layout", "draw", "CPU", "wall");
        for (ri, &(ci, mode)) in cam_runs.iter().enumerate() {
            let fr = &cam_frames[ri];
            let (k, _) = cam_counts[ri];
            let name = cameras[ci].name;
            let col = |i: usize| stats(&fr.iter().map(|f| f.pass_ms[i]).collect::<Vec<_>>());
            let (cpu, wall) = (stats(&fr.iter().map(|f| f.cpu_ms).collect::<Vec<_>>()), stats(&fr.iter().map(|f| f.wall_ms).collect::<Vec<_>>()));
            let cells: Vec<(f64, f64)> = (0..6).map(col).chain([cpu, wall]).collect();
            let fmt = |sel: &dyn Fn(&(f64, f64)) -> f64| cells.iter().map(|c| format!("{:9.3}", sel(c))).collect::<Vec<_>>().join(" ");
            println!("{:<9} {:<4} {:>8} {:>8} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {}", name, if mode == Cull::Gpu { "gpu" } else { "cpu" }, k.vis_files, k.backdrops, k.cand_lines, k.vis_lines, k.segs, k.slots, k.dropped, "min", fmt(&|c| c.0));
            println!("{:<9} {:<4} {:>8} {:>8} {:>9} {:>9} {:>8} {:>8} {:>8} {:>8} {}", "", "", "", "", "", "", "", "", "", "median", fmt(&|c| c.1));
            if mode == Cull::Cpu { println!("{:<9} {:<4} (cpu: 'cullA' is the CPU frustum+LOD time over {} boxes, included in CPU; prefix/cullB/final do not run)", "", "", boxes.len()) }
            if k.dropped > 0 || k.segs > res.cap_segs as u32 { println!("{:<9} {:<4} CAP BOUND: {} lines dropped, slot demand {} vs cap {}, segment demand {} vs cap {}; drawn slots {}", "", "", k.dropped, k.slots, res.cap_slots, k.segs, res.cap_segs, k.slots.min(k.draw_limit)) }
        }
        let rb: Vec<f64> = cam_counts.iter().filter(|(_, t)| *t > 0.0).map(|(_, t)| *t).collect();
        if !rb.is_empty() { println!("debug readback of the 32 B counters (for this table only; the frame path reads nothing back): {} per frame", mm(&rb)) }
        println!();
    }
    println!("--- resident memory ---");
    let (b_mb, l_mb, i_mb) = (mb(corpus.bytes.len() as u64), mb(corpus.lines.len() as u64 * 8), mb(corpus.items.len() as u64 * 48));
    let box_mb = mb(boxes.len() as u64 * 32);
    let seed_mb = mb(corpus.seed_dir.len() as u64 * 16 + corpus.seeds.len() as u64 * 16);
    let (span_mb, idx_mb) = corpus.spans.as_ref().map(|s| (mb(s.table.len() as u64 * 8), mb(s.line_idx.len() as u64 * 4))).unwrap_or((0.0, 0.0));
    let dense_mb = corpus.dense.as_ref().map(|d| mb(d.color.len() as u64 * 4 + d.xf.len() as u64 * 8)).unwrap_or(0.0);
    println!("bytes {:.1} MB, line table {:.1} MB (8 B/line), item table {:.1} MB (48 B/item), file boxes {:.1} MB (32 B/file), seed table {:.1} MB ({} dir entries x 16 B + {} seeds x 16 B), span table {:.1} MB (8 B/span, allocated incl. slack + headroom), per-line span index {:.1} MB (4 B/line), dense arrays {:.1} MB; total resident {:.1} MB",
        b_mb, l_mb, i_mb, box_mb, seed_mb, corpus.seed_dir.len(), corpus.seeds.len(), span_mb, idx_mb, dense_mb, b_mb + l_mb + i_mb + box_mb + seed_mb + span_mb + idx_mb + dense_mb);
    println!("transient: slots {:.1} MB ({} cap x 20 B){}; segments {:.1} MB ({} cap x 32 B)", mb(cap_slots as u64 * SLOT_BYTES), cap_slots, if corpus.items.iter().any(|it| it.repr == 3) { format!(" + transform stream {:.1} MB (cap x 8 B, written for dense+xf slots only)", mb(cap_slots as u64 * 8)) } else { String::new() }, mb(cap_segs as u64 * 32), cap_segs);
    let vram = vram_total_mb().map(|m| format!("{m} MiB (nvidia-smi)")).unwrap_or_else(|| "n/a".into());
    println!("HyperLayout whole-tree slots for this corpus: {:.1} MB Derived (20 B/glyph), {:.1} MB Instanced (32 B/glyph); adapter max_buffer_size {:.0} MB, VRAM total {}", mb(corpus.total_glyphs * 20), mb(corpus.total_glyphs * 32), mb(al.max_buffer_size), vram);
    println!();
    for (name, v) in &verdicts { println!("readback check ({name}, segment-bytes {}): {v}", args.segment_bytes) }
    if let Some(v) = &page_verdict { println!("readback check (page lines, colour tables on): {v}") }
    for v in &cull_verdicts { println!("cull check: {v}") }
    println!("pick check: {pick_verdict}");
    for e in &edits { println!("edit: {e}") }
    if let Some(v) = &post_edit { println!("readback check (edited file's page rows, after the edit): {v}") }
    println!("not measured: Slug coverage in the fragment stage (identical for both designs; quads only), emoji/ZWJ sequences and the atlas trie, pagination, the renderer's paint beyond a `//` comment flag, real LSP spans (the demo spans are a heuristic; one colour per span, no overlap, no nesting), depth test, a pitched camera (every preset looks straight down).");
    println!("loadavg at end: {}", loadavg());
}
