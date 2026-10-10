//! The resident corpus: the walk (the renderer's rules, copied from
//! jit-layout so the numbers compare), Pass 1 (the line and item tables plus
//! the seeds of every long line, now with the REAL glyph resolution: a
//! pure-ASCII line is counted from a table, any other line is walked by the
//! CPU twin of the kernel), the per-frame visible list, and the CPU
//! reference fold the GPU output is held bit-equal to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use crate::gpu::ms;
use crate::trie::{pk_cells, pk_glyph, Trie};

pub const SLOT_BYTES: u64 = 20;
pub const MAX_FILE_BYTES: u64 = 10 << 20;
/// The renderer's repo defaults: 100 columns, a wrap steps BACK in depth (repo.rs RepoParams).
pub const WRAP_COLS: u32 = 100;
pub const LINE_HEIGHT: f32 = 1.25;
pub const Z_STEP: f32 = 0.15;
/// `layout::DEFAULT_COLOR_PACKED`, the flat paint.
pub const FLAT_COLOR: u32 = 0xFF_D4D4D4;

// The renderer's walk rules (native/src/repo/walk.rs).
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

/// The Derived slot, 20 B (glyph_field_derived.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Slot { pub x: f32, pub row: u32, pub glyph_and_wrap: u32, pub color: u32, pub group: u32 }
/// The line table, 8 B/line; a sentinel closes the last line.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Line { pub byte_start: u32, pub glyphs: u32 }
/// The item table, 32 B/item, resident (the design's footprint; the kernel
/// reads it through the segments). `byte_len` includes the terminating '\n'
/// (appended when the file lacks one); `real_len` is the file's own length.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Item { pub first_line: u32, pub line_count: u32, pub byte_start: u32, pub byte_len: u32, pub real_len: u32, pub max_len: u32, pub longest_line: u32, pub _pad: u32 }
/// One visible segment, 36 B, per frame: its bytes, the row and view-local
/// group its slots carry, the item's true end (`lim`, past which the kernel
/// reads 0 as the reference does), its slot base and the fold seeds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Seg { pub byte_start: u32, pub byte_len: u32, pub row: u32, pub group: u32, pub lim: u32, pub slot_base: u32, pub col_seed: u32, pub cells_seed: u32, pub x_seed: f32 }
pub const SEG_BYTES: u64 = 36;

#[derive(Clone, Copy, PartialEq)]
pub enum WalkMode { Renderer, All }

pub fn mb(b: u64) -> f64 { b as f64 / 1e6 }

/// Contiguous index ranges over `weights`, `n` of them, balanced by weight.
fn partition(weights: &[usize], n: usize) -> Vec<std::ops::Range<usize>> {
    let total: usize = weights.iter().sum();
    let target = total / n.max(1) + 1;
    let (mut out, mut start, mut acc) = (Vec::new(), 0usize, 0usize);
    for (i, w) in weights.iter().enumerate() {
        acc += w;
        if acc >= target && out.len() + 1 < n { out.push(start..i + 1); start = i + 1; acc = 0 }
    }
    if start < weights.len() { out.push(start..weights.len()) }
    out
}

// ---------------------------------------------------------------- walk

pub struct FileSpan { pub rel: String, pub byte_start: usize, pub byte_len: usize, pub real_len: usize }

pub struct WalkStats { pub enumerate_ms: f64, pub read_ms: f64, pub concat_ms: f64, pub candidates: usize, pub skipped_large: usize, pub skipped_non_utf8: usize, pub read_errors: usize, pub pad_bytes: usize }

pub struct Walked { pub bytes: Vec<u8>, pub files: Vec<FileSpan>, pub stats: WalkStats }

pub fn enumerate(root: &Path, mode: WalkMode) -> Vec<(String, PathBuf)> {
    let mut cands = Vec::new();
    if root.is_file() {
        cands.push((root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), root.to_path_buf()));
        return cands;
    }
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

fn read_all(cands: &[(String, PathBuf)], threads: usize, utf8_only: bool) -> Vec<ReadResult> {
    let next = AtomicUsize::new(0);
    let mut per_thread: Vec<Vec<(usize, ReadResult)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| {
            let mut mine = Vec::new();
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= cands.len() { break }
                let r = match std::fs::metadata(&cands[i].1) {
                    Err(_) => ReadResult::Error,
                    Ok(m) if m.len() > MAX_FILE_BYTES => ReadResult::Large,
                    Ok(_) => match std::fs::read(&cands[i].1) {
                        Err(_) => ReadResult::Error,
                        Ok(b) if utf8_only && std::str::from_utf8(&b).is_err() => ReadResult::NonUtf8,
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

/// Walk + read + concatenate; a file never straddles a GPU chunk boundary.
/// `WalkMode::All` (the check corpora) keeps non-UTF-8 files too: the kernel
/// and HyperLayout both take any bytes.
pub fn walk(root: &Path, mode: WalkMode, threads: usize, chunk_size: usize) -> Walked {
    let t = Instant::now();
    let cands = enumerate(root, mode);
    let enumerate_ms = ms(t);
    let t = Instant::now();
    let results = read_all(&cands, threads, mode == WalkMode::Renderer);
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
                let last = cur + size - 1;
                if cur / chunk_size != last / chunk_size {
                    let aligned = last / chunk_size * chunk_size;
                    pad_bytes += aligned - cur;
                    cur = aligned;
                }
                files.push(FileSpan { rel: rel.clone(), byte_start: cur, byte_len: size, real_len: b.len() });
                datas.push(b);
                cur += size;
            }
        }
    }
    let total = cur.next_multiple_of(4).max(4);
    let mut bytes = vec![0u8; total];
    if !files.is_empty() {
        let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
        std::thread::scope(|s| {
            let mut rest = &mut bytes[..];
            let mut done = 0usize;
            for g in groups {
                let (first, last) = (&files[g.start], &files[g.end - 1]);
                let end = last.byte_start + last.byte_len;
                let (mine, tail) = std::mem::take(&mut rest).split_at_mut(end - done);
                rest = tail;
                let base = done;
                done = end;
                let (files, datas) = (&files[g.clone()], &datas[g]);
                let _ = first;
                s.spawn(move || {
                    for (f, d) in files.iter().zip(datas) {
                        let dst = &mut mine[f.byte_start - base..f.byte_start - base + f.byte_len];
                        dst[..d.len()].copy_from_slice(d);
                        if d.len() < f.byte_len { dst[d.len()] = b'\n' }
                    }
                });
            }
        });
    }
    let concat_ms = ms(t);
    Walked { bytes, files, stats: WalkStats { enumerate_ms, read_ms, concat_ms, candidates: cands.len(), skipped_large, skipped_non_utf8, read_errors, pad_bytes } }
}

// ---------------------------------------------------------------- the fold (CPU twin of layout.wgsl)

/// A continuation's seeds: where it starts in the line and the fold state there.
#[derive(Clone, Copy, Debug)]
pub struct SegSeed { pub byte_off: u32, pub col: u32, pub cells: u32, pub x: f32, /// Slots the line drew before this cut (the segment's slot base inside the line).
    pub slots: u32 }

/// The fold's running state, step for step the kernel's.
#[derive(Clone, Copy)]
pub struct Fold { pub col: u32, pub cells: u32, pub x: f32, pub trailer_until: usize }

impl Fold {
    pub fn new() -> Fold { Fold { col: 0, cells: 0, x: 0.0, trailer_until: 0 } }
    pub fn seed(&self, byte_off: u32, slots: u32) -> SegSeed { SegSeed { byte_off, col: self.col, cells: self.cells, x: self.x, slots } }

    /// Resolve the leader at `i` and advance: returns `(glyph, x, wrap_segment, advance, byte length)`,
    /// or `None` with a 1-byte step for a non-leader. `trie.cell_adv` is one cell.
    #[inline(always)]
    pub fn step(&mut self, trie: &Trie, bytes: &[u8], i: usize, end: usize, lim: usize, wrap: u32) -> (Option<(u32, f32, u32, f32)>, usize) {
        let Some((r, len)) = trie.resolve(bytes, i, end, lim, &mut self.trailer_until) else { return (None, 1) };
        let (glyph, k) = (pk_glyph(r), pk_cells(r));
        let adv = k as f32 * trie.cell_adv;
        let (xs, seg) = if wrap > 0 {
            if self.col % wrap == 0 { self.x = 0.0 }
            (self.x, self.col / wrap)
        } else {
            (self.cells as f32 * trie.cell_adv, 0)
        };
        self.col += 1;
        self.cells += k;
        self.x += adv;
        (Some((glyph, xs, seg, adv)), len)
    }
}

/// Lay out `bytes[start..end)` from `fold`'s state, emitting one slot per glyph.
pub fn fold_segment(trie: &Trie, bytes: &[u8], start: usize, end: usize, lim: usize, wrap: u32, fold: &mut Fold, row: u32, group: u32, mut emit: impl FnMut(Slot, f32)) {
    let mut i = start;
    while i < end {
        let (r, len) = fold.step(trie, bytes, i, end, lim, wrap);
        if let Some((glyph, xs, seg, adv)) = r {
            if glyph != 0 { emit(Slot { x: xs, row, glyph_and_wrap: glyph | (seg << 16), color: FLAT_COLOR, group }, adv) }
        }
        i += len;
    }
}

fn is_ascii(line: &[u8]) -> bool {
    let (chunks, rest) = line.as_chunks::<8>();
    chunks.iter().fold(0u64, |acc, c| acc | u64::from_le_bytes(*c)) & 0x8080_8080_8080_8080 == 0 && rest.iter().all(|&b| b < 0x80)
}

/// Per ASCII byte: 1 when it draws a slot (every printable byte; controls and DEL do not).
pub struct AsciiCounts { pub has_slot: [u8; 128], pub run_x: Vec<f32> }

impl AsciiCounts {
    pub fn new(trie: &Trie, wrap: u32) -> AsciiCounts {
        let mut has_slot = [0u8; 128];
        for b in 0..128usize {
            assert_eq!(pk_cells(trie.ascii[b]), 1, "every ASCII byte is one cell");
            has_slot[b] = u8::from(pk_glyph(trie.ascii[b]) != 0);
        }
        // The f32 running sum of n cells, n < wrap, in the fold's own order.
        let mut run_x = Vec::with_capacity(wrap.max(1) as usize);
        let mut x = 0f32;
        for _ in 0..wrap.max(1) { run_x.push(x); x += trie.cell_adv }
        AsciiCounts { has_slot, run_x }
    }
    fn slots(&self, line: &[u8]) -> u32 { line.iter().map(|&b| self.has_slot[b as usize] as u32).sum() }
    fn seed_at(&self, i: usize, wrap: u32, slots: u32) -> SegSeed {
        SegSeed { byte_off: i as u32, col: i as u32, cells: i as u32, x: if wrap > 0 { self.run_x[i % wrap as usize] } else { 0.0 }, slots }
    }
}

/// Pass 1's walk of one line: its slot count and, when it is longer than
/// `seg_bytes`, the seeds of every segment after the first (a cut lands only
/// before an ASCII byte, so no codepoint, sequence or trailer span is split).
fn line_slots_and_seeds(trie: &Trie, ac: &AsciiCounts, data: &[u8], start: usize, end: usize, lim: usize, seg_bytes: usize, wrap: u32) -> (u32, Vec<SegSeed>) {
    let line = &data[start..end];
    let mut seeds = Vec::new();
    if is_ascii(line) {
        if line.len() > seg_bytes {
            let (mut cut, mut slots) = (seg_bytes, 0u32);
            while cut < line.len() {
                slots += ac.slots(&line[cut - seg_bytes..cut]);
                seeds.push(ac.seed_at(cut, wrap, slots));
                cut += seg_bytes;
            }
        }
        return (ac.slots(line), seeds);
    }
    let (mut f, mut i, mut seg_start, mut slots) = (Fold::new(), start, start, 0u32);
    while i < end {
        if i - seg_start >= seg_bytes && data[i] < 0x80 {
            seeds.push(f.seed((i - start) as u32, slots));
            seg_start = i;
        }
        let (r, len) = f.step(trie, data, i, end, lim, wrap);
        if let Some((glyph, ..)) = r { slots += u32::from(glyph != 0) }
        i += len;
    }
    (slots, seeds)
}

// ---------------------------------------------------------------- Pass 1

pub struct Pass1 { pub lines: Vec<Line>, pub items: Vec<Item>, pub seeds: HashMap<u32, Vec<SegSeed>>, pub total_glyphs: u64, pub non_ascii_lines: u64, pub wall_ms: f64 }

struct Part { lines: Vec<Line>, items: Vec<Item>, seeds: Vec<(usize, u32, Vec<SegSeed>)>, glyphs: u64, non_ascii: u64 }

pub fn pass1(trie: &Trie, bytes: &[u8], files: &[FileSpan], threads: usize, seg_bytes: usize, wrap: u32) -> Pass1 {
    let t = Instant::now();
    let ac = AsciiCounts::new(trie, wrap);
    let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
    let parts: Vec<Part> = std::thread::scope(|s| {
        let handles: Vec<_> = groups.iter().map(|g| {
            let files = &files[g.clone()];
            let ac = &ac;
            s.spawn(move || {
                let est = files.iter().map(|f| f.byte_len).sum::<usize>() / 24;
                let mut p = Part { lines: Vec::with_capacity(est), items: Vec::with_capacity(files.len()), seeds: Vec::new(), glyphs: 0, non_ascii: 0 };
                for (fi, f) in files.iter().enumerate() {
                    let data = &bytes[f.byte_start..f.byte_start + f.byte_len];
                    let first_line = p.lines.len();
                    let (mut line_start, mut max_len, mut longest) = (0usize, 0u32, 0u32);
                    for nl in memchr::memchr_iter(b'\n', data) {
                        let (glyphs, seeds) = line_slots_and_seeds(trie, ac, data, line_start, nl, f.real_len, seg_bytes, wrap);
                        if !is_ascii(&data[line_start..nl]) { p.non_ascii += 1 }
                        if !seeds.is_empty() { p.seeds.push((fi, (p.lines.len() - first_line) as u32, seeds)) }
                        let len = (nl - line_start) as u32;
                        if len > max_len { max_len = len; longest = (p.lines.len() - first_line) as u32 }
                        p.glyphs += glyphs as u64;
                        p.lines.push(Line { byte_start: (f.byte_start + line_start) as u32, glyphs });
                        line_start = nl + 1;
                    }
                    p.items.push(Item { first_line: first_line as u32, line_count: (p.lines.len() - first_line) as u32, byte_start: f.byte_start as u32, byte_len: f.byte_len as u32, real_len: f.real_len as u32, max_len, longest_line: longest, _pad: 0 });
                }
                p
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total: usize = parts.iter().map(|p| p.lines.len()).sum();
    let mut lines = vec![Line::zeroed(); total + 1];
    let mut items = Vec::with_capacity(files.len());
    let mut seeds = HashMap::new();
    let (mut total_glyphs, mut non_ascii_lines) = (0u64, 0u64);
    std::thread::scope(|s| {
        let mut rest = &mut lines[..total];
        let mut base = 0usize;
        for p in &parts {
            let item_base = items.len();
            for it in &p.items { items.push(Item { first_line: it.first_line + base as u32, longest_line: it.first_line + base as u32 + it.longest_line, ..*it }) }
            for (fi, row, sd) in &p.seeds { seeds.insert(items[item_base + fi].first_line + row, sd.clone()); }
            total_glyphs += p.glyphs;
            non_ascii_lines += p.non_ascii;
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut(p.lines.len());
            rest = tail;
            base += p.lines.len();
            s.spawn(move || mine.copy_from_slice(&p.lines));
        }
    });
    let end = files.last().map(|f| f.byte_start + f.byte_len).unwrap_or(0);
    lines[total] = Line { byte_start: end as u32, glyphs: 0 };
    Pass1 { lines, items, seeds, total_glyphs, non_ascii_lines, wall_ms: ms(t) }
}

// ---------------------------------------------------------------- corpus + visible lists

pub struct Corpus { pub bytes: Vec<u8>, pub files: Vec<FileSpan>, pub lines: Vec<Line>, pub items: Vec<Item>, pub seeds: HashMap<u32, Vec<SegSeed>>, pub total_glyphs: u64, pub non_ascii_lines: u64, pub segment_bytes: usize, pub wrap: u32 }

impl Corpus {
    pub fn build(trie: &Trie, w: Walked, threads: usize, seg_bytes: usize, wrap: u32) -> (Corpus, Pass1Stats) {
        let p1 = pass1(trie, &w.bytes, &w.files, threads, seg_bytes, wrap);
        let stats = Pass1Stats { wall_ms: p1.wall_ms };
        let c = Corpus { bytes: w.bytes, files: w.files, lines: p1.lines, items: p1.items, seeds: p1.seeds, total_glyphs: p1.total_glyphs, non_ascii_lines: p1.non_ascii_lines, segment_bytes: seg_bytes, wrap };
        // Every seeded line is a long line of the table the visible list reads, and vice versa.
        for (&li, sd) in &c.seeds {
            let len = c.line_len(li, &c.items[c.item_of(li)]) as usize;
            assert!(len > seg_bytes, "line {li} carries {} seeds but is {len} B", sd.len());
            assert!(sd.last().is_some_and(|s| (s.byte_off as usize) < len));
        }
        let long_lines = (0..c.line_count() as u32).filter(|&li| c.line_len(li, &c.items[c.item_of(li)]) as usize > seg_bytes).count();
        assert_eq!(long_lines, c.seeds.len(), "long lines without seeds");
        (c, stats)
    }
    pub fn line_count(&self) -> usize { self.lines.len() - 1 }
    pub fn item_of(&self, li: u32) -> usize { self.items.partition_point(|it| it.first_line <= li) - 1 }
    pub fn line_len(&self, li: u32, item: &Item) -> u32 {
        let next = self.lines[li as usize + 1].byte_start.min(item.byte_start + item.byte_len);
        next - self.lines[li as usize].byte_start - 1
    }
    pub fn source_bytes(&self) -> u64 { self.files.iter().map(|f| f.byte_len as u64).sum() }
}

pub struct Pass1Stats { pub wall_ms: f64 }

pub struct Visible { pub segs: Vec<Seg>, pub slots: usize, pub bytes_read: u64 }

/// The per-frame list: one segment per visible line, or several for a long
/// line (from its Pass-1 seeds), with the exclusive slot prefix as each
/// segment's base. `group_of_line[k]` is the view-local group of `lines[k]`.
pub fn visible_list(c: &Corpus, lines: &[u32], group_of_line: &[u32]) -> Visible {
    let (mut segs, mut base, mut bytes_read) = (Vec::with_capacity(lines.len()), 0u32, 0u64);
    let mut cur = 0usize;
    for (k, &li) in lines.iter().enumerate() {
        let it = &c.items[cur];
        if li < it.first_line || li >= it.first_line + it.line_count { cur = c.item_of(li) }
        let it = &c.items[cur];
        let (start, len, glyphs) = (c.lines[li as usize].byte_start, c.line_len(li, it), c.lines[li as usize].glyphs);
        let (row, group, lim) = (li - it.first_line, group_of_line[k], it.byte_start + it.real_len);
        bytes_read += len as u64;
        let seeds = if len as usize > c.segment_bytes { c.seeds.get(&li) } else { None };
        match seeds {
            Some(seeds) => {
                let mut prev = SegSeed { byte_off: 0, col: 0, cells: 0, x: 0.0, slots: 0 };
                for sd in seeds.iter().copied().chain(std::iter::once(SegSeed { byte_off: len, col: 0, cells: 0, x: 0.0, slots: glyphs })) {
                    segs.push(Seg { byte_start: start + prev.byte_off, byte_len: sd.byte_off - prev.byte_off, row, group, lim, slot_base: base + prev.slots, col_seed: prev.col, cells_seed: prev.cells, x_seed: prev.x });
                    prev = sd;
                }
            }
            None => segs.push(Seg { byte_start: start, byte_len: len, row, group, lim, slot_base: base, col_seed: 0, cells_seed: 0, x_seed: 0.0 }),
        }
        base += glyphs;
    }
    Visible { segs, slots: base as usize, bytes_read }
}

/// The CPU reference: every visible line folded WHOLE, single-threaded, from
/// column 0, with the view's group ids. Returns the slots and their advances.
pub fn cpu_reference(trie: &Trie, c: &Corpus, lines: &[u32], group_of_line: &[u32]) -> (Vec<Slot>, Vec<f32>) {
    let total: usize = lines.iter().map(|&li| c.lines[li as usize].glyphs as usize).sum();
    let (mut out, mut advs) = (Vec::with_capacity(total), Vec::with_capacity(total));
    for (k, &li) in lines.iter().enumerate() {
        let item = c.item_of(li);
        let it = &c.items[item];
        let start = c.lines[li as usize].byte_start as usize;
        let end = start + c.line_len(li, it) as usize;
        let lim = it.byte_start as usize + it.real_len as usize;
        let mut f = Fold::new();
        fold_segment(trie, &c.bytes, start, end, lim, c.wrap, &mut f, li - it.first_line, group_of_line[k], |s, a| { out.push(s); advs.push(a) });
    }
    assert_eq!(out.len(), total, "Pass 1's slot count disagrees with the whole-line fold");
    (out, advs)
}
