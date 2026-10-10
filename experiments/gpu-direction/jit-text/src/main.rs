//! jit-text — headless measurement of the visible-set layout kernel made REAL
//! TEXT: glyph lookup (emoji sequences included) on the GPU from resident
//! atlas tables, and the frame cost of the renderer's own Slug shading.
//!
//! Three questions, one binary:
//!   A. what a frame of real text costs: the Derived-mode Slug shader drawing
//!      the kernel's slots (with and without the emoji sheet bound) against
//!      the same slots as flat quads;
//!   B. where the glyph lookup should live: the codepoints.bin trie as-is
//!      (two dependent loads) against a direct BMP table + hash tables, both
//!      resident, both measured on the three views and on emoji-heavy input;
//!   C. whether the GPU resolver IS HyperLayout's: `--check` holds every slot
//!      (glyph id, advance, x as f32 bits, wrap segment) to the native crate's
//!      HyperLayout over the same bytes.
//!
//! Every timing repeats `--repeat` times, interleaved across the measured
//! things, and reports min and median because the GPU and CPU are shared.
//!
//!     jit-text [<corpus-dir>] [--view page|overview|worst|all] [--repeat K]
//!         [--screenshot <png>] [--atlas-dir <dir>] [--emoji-sheet <path>] [--no-emoji]
//!         [--check <fixture-file-or-dir>]... [--threads T] [--segment-bytes S]

mod atlas;
mod check;
mod corpus;
mod gpu;
mod scene;
mod trie;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use corpus::{Corpus, FileSpan, WalkMode, LINE_HEIGHT, SLOT_BYTES, WRAP_COLS};
use gpu::{loadavg, ms, stats};
use scene::{DrawMode, Placement, Variant, TARGET};
use trie::{GpuTables, Trie};

const PAGE_LINES: usize = 60;
const OVERVIEW_FILES: usize = 2000;
const OVERVIEW_LINES_PER_FILE: usize = 50;
const WORST_LINES: usize = 1000;

static TRIE: std::sync::OnceLock<Arc<Trie>> = std::sync::OnceLock::new();
/// The one trie, for the check's mismatch locator.
pub fn trie_ref() -> &'static Trie { TRIE.get().expect("trie loaded").as_ref() }

// ---------------------------------------------------------------- CLI

struct Args { corpus: Option<PathBuf>, view: Option<String>, repeat: usize, screenshot: Option<PathBuf>, atlas_dir: Option<PathBuf>, emoji_sheet: Option<PathBuf>, emoji: bool, check: Vec<PathBuf>, threads: usize, segment_bytes: usize }

const USAGE: &str = "usage: jit-text [<corpus-dir>] [--view page|overview|worst|all] [--repeat K] [--screenshot <png>] [--atlas-dir <dir>] [--emoji-sheet <path>] [--no-emoji] [--check <fixture-file-or-dir>]... [--threads T] [--segment-bytes S]";

fn parse_args() -> Args {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let mut a = Args { corpus: None, view: None, repeat: 5, screenshot: None, atlas_dir: None, emoji_sheet: None, emoji: true, check: Vec::new(), threads, segment_bytes: 2048 };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--view" => a.view = Some(val("--view")),
            "--repeat" => a.repeat = val("--repeat").parse().expect("--repeat"),
            "--screenshot" => a.screenshot = Some(PathBuf::from(val("--screenshot"))),
            "--atlas-dir" => a.atlas_dir = Some(PathBuf::from(val("--atlas-dir"))),
            "--emoji-sheet" => a.emoji_sheet = Some(PathBuf::from(val("--emoji-sheet"))),
            "--no-emoji" => a.emoji = false,
            "--check" => a.check.push(PathBuf::from(val("--check"))),
            "--threads" => a.threads = val("--threads").parse().expect("--threads"),
            "--segment-bytes" => a.segment_bytes = val("--segment-bytes").parse().expect("--segment-bytes"),
            "-h" | "--help" => { println!("{USAGE}"); std::process::exit(0) }
            _ if a.corpus.is_none() && !arg.starts_with("--") => a.corpus = Some(PathBuf::from(arg)),
            _ => panic!("unexpected argument {arg}\n{USAGE}"),
        }
    }
    assert!(a.corpus.is_some() || !a.check.is_empty(), "{USAGE}");
    assert!(a.repeat >= 1 && a.threads >= 1 && a.segment_bytes >= 16);
    if let Some(v) = &a.view { assert!(["page", "overview", "worst", "all"].contains(&v.as_str()), "--view: {v}") }
    assert!(a.screenshot.is_none() || a.view.is_some(), "--screenshot needs --view");
    a
}

/// `assets/atlas` of the repository this binary lives in: walk up from the
/// executable, then from the working directory, to the first `assets/atlas/codepoints.bin`.
fn default_atlas_dir() -> PathBuf {
    let mut starts = Vec::new();
    if let Ok(exe) = std::env::current_exe() { starts.push(exe) }
    if let Ok(cwd) = std::env::current_dir() { starts.push(cwd) }
    for s in starts {
        let mut p = s.as_path();
        while let Some(parent) = p.parent() {
            let cand = parent.join("assets").join("atlas");
            if cand.join("codepoints.bin").is_file() { return cand }
            p = parent;
        }
    }
    panic!("no assets/atlas above the executable or the working directory; pass --atlas-dir");
}

// ---------------------------------------------------------------- views

#[derive(Clone, Copy, PartialEq)]
enum Layout { Stack, Grid }

struct View { name: String, lines: Vec<u32>, layout: Layout, tint: bool }

/// 60 consecutive lines from the middle of the largest `.c` under `kernel/` (else the largest `.c`, else the largest file).
fn page_view(c: &Corpus) -> (View, usize) {
    let pick = |pred: &dyn Fn(&FileSpan) -> bool| c.files.iter().enumerate().filter(|(_, f)| pred(f)).max_by_key(|(_, f)| f.byte_len).map(|(i, _)| i);
    let item = pick(&|f| f.rel.starts_with("kernel/") && f.rel.ends_with(".c")).or_else(|| pick(&|f| f.rel.ends_with(".c"))).or_else(|| pick(&|_| true)).expect("empty corpus");
    let it = &c.items[item];
    let n = (it.line_count as usize).min(PAGE_LINES);
    let start = it.first_line as usize + (it.line_count as usize - n) / 2;
    (View { name: "page".into(), lines: (start as u32..(start + n) as u32).collect(), layout: Layout::Stack, tint: false }, item)
}

/// The first 50 lines of every k-th file in walk order, 2,000 files.
fn overview_view(c: &Corpus) -> View {
    let k = (c.items.len() / OVERVIEW_FILES).max(1);
    let mut lines = Vec::new();
    for it in c.items.iter().step_by(k).take(OVERVIEW_FILES) {
        lines.extend(it.first_line..it.first_line + it.line_count.min(OVERVIEW_LINES_PER_FILE as u32));
    }
    View { name: "overview".into(), lines, layout: Layout::Grid, tint: true }
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
    println!("worst view: the {} longest lines; top 3:", top.len());
    for &(len, li, item) in top.iter().take(3) {
        let it = &c.items[item];
        println!("  {:>8} B  {}:{}  ({} glyphs)", len, c.files[item].rel, li - it.first_line + 1, c.lines[li as usize].glyphs);
    }
    View { name: "worst".into(), lines: top.iter().map(|&(_, li, _)| li).collect(), layout: Layout::Stack, tint: false }
}

/// Every line of the corpus, stacked — for a small emoji-heavy input.
fn all_view(c: &Corpus) -> View {
    View { name: "all".into(), lines: (0..c.line_count() as u32).collect(), layout: Layout::Stack, tint: false }
}

fn plan_views(c: &Corpus, args: &Args) -> Vec<View> {
    match args.view.as_deref() {
        Some("page") => vec![page_view(c).0],
        Some("overview") => vec![overview_view(c)],
        Some("worst") => vec![worst_view(c)],
        Some("all") => vec![all_view(c)],
        _ => vec![page_view(c).0, overview_view(c), worst_view(c)],
    }
}

fn hue_rgb(h: f32) -> [f32; 3] {
    let k = |o: f32| ((h + o).fract() * 6.0 - 3.0).abs() - 1.0;
    [k(0.0).clamp(0.0, 1.0), k(2.0 / 3.0).clamp(0.0, 1.0), k(1.0 / 3.0).clamp(0.0, 1.0)]
}

/// Place the view: a reading view stacks its lines in order, one group per
/// visible line (so lines picked from anywhere in a file sit consecutively);
/// the overview places one group per item on a grid. Group ids are view-local.
fn place(c: &Corpus, view: &View, cell_adv: f32) -> Placement {
    let tint_of = |item: usize| if view.tint { let h = (item as u32).wrapping_mul(2654435761); let h = h ^ (h >> 15); let c = hue_rgb((h & 0xFFFF) as f32 / 65536.0); [0.55 + 0.45 * c[0], 0.55 + 0.45 * c[1], 0.55 + 0.45 * c[2]] } else { [1.0; 3] };
    let widest = view.lines.iter().map(|&li| c.line_len(li, &c.items[c.item_of(li)]).min(WRAP_COLS)).max().unwrap_or(1);
    let (origins, tints, group_of_line, bounds) = match view.layout {
        Layout::Stack => {
            let mut origins = Vec::with_capacity(view.lines.len());
            let mut tints = Vec::with_capacity(view.lines.len());
            for (k, &li) in view.lines.iter().enumerate() {
                let item = c.item_of(li);
                let row = li - c.items[item].first_line;
                // The slot's y is origin_y - row * line_height; stack position k wants -k * line_height.
                origins.push([0.0, (row as f32 - k as f32) * LINE_HEIGHT, 0.0]);
                tints.push(tint_of(item));
            }
            let n = view.lines.len() as f32;
            let group_of_line: Vec<u32> = (0..view.lines.len() as u32).collect();
            (origins, tints, group_of_line, ([0.0, -n * LINE_HEIGHT + LINE_HEIGHT - 0.5], [widest as f32 * cell_adv, 0.5]))
        }
        Layout::Grid => {
            let mut order: Vec<usize> = Vec::new();
            let mut rows: HashMap<usize, (u32, u32, u32)> = HashMap::new();
            let mut group_of_line = Vec::with_capacity(view.lines.len());
            for &li in &view.lines {
                let item = c.item_of(li);
                let row = li - c.items[item].first_line;
                let e = rows.entry(item).and_modify(|r| { r.0 = r.0.min(row); r.1 = r.1.max(row) }).or_insert_with(|| { order.push(item); (row, row, order.len() as u32 - 1) });
                group_of_line.push(e.2);
            }
            let mut origins = Vec::with_capacity(order.len());
            let tints: Vec<[f32; 3]> = order.iter().map(|&item| tint_of(item)).collect();
            let gap = 4.0f32;
            let cell_h = order.iter().map(|i| rows[i].1 - rows[i].0 + 1).max().unwrap_or(1) as f32 * LINE_HEIGHT + gap;
            let cell_w = WRAP_COLS as f32 * cell_adv + gap;
            let aspect = TARGET.0 as f32 / TARGET.1 as f32;
            let cols = ((order.len() as f32 * aspect * cell_h / cell_w).sqrt().ceil() as usize).max(1);
            for (k, item) in order.iter().enumerate() {
                let (lo, ..) = rows[item];
                origins.push([(k % cols) as f32 * cell_w, -((k / cols) as f32) * cell_h + lo as f32 * LINE_HEIGHT, 0.0]);
            }
            let grid_rows = order.len().div_ceil(cols);
            (origins, tints, group_of_line, ([0.0, -(grid_rows as f32) * cell_h + gap], [cols as f32 * cell_w - gap, 0.5]))
        }
    };
    let px_per_em = scene::px_per_em(bounds);
    Placement { origins, tints, group_of_line, bounds, px_per_em }
}

// ---------------------------------------------------------------- main

struct Measured { compute: HashMap<Variant, Vec<f64>>, draw: HashMap<DrawMode, Vec<f64>>, wall: HashMap<(Variant, DrawMode), Vec<f64>>, cpu: Vec<f64>, slots: usize, segments: usize, bytes_read: u64 }

fn main() {
    let args = parse_args();
    println!("loadavg at start: {}", loadavg());
    let atlas_dir = args.atlas_dir.clone().unwrap_or_else(default_atlas_dir);
    let emoji_sheet = args.emoji.then(|| args.emoji_sheet.clone().unwrap_or_else(|| atlas_dir.join("emoji-sheet.bin")));

    // 1. The tables and the atlas.
    let t = std::time::Instant::now();
    let trie = Arc::new(Trie::load(&atlas_dir));
    TRIE.set(trie.clone()).ok();
    let tables = GpuTables::build(&trie);
    println!("tables: codepoints.bin {} B as read; variant A (trie index + packed blocks + head bitmap + sequence section) {} B; variant B (BMP direct + cp hash + sequence-key hash + sequence section) {} B; ASCII table {} B shared; built + self-checked over every codepoint in {:.0} ms",
        trie.file_bytes, tables.bytes_variant_a, tables.bytes_variant_b, tables.bytes_shared_ascii, ms(t));
    println!("  breakdown A: index {} B, packed blocks {} B, head bitmap {} B; B: BMP table {} B, cp hash {} B ({} slots), seq-key hash {} B ({} slots); sequence section {} B ({} entries x {} words)",
        trie.block_index.len() * 4, trie.blocks.len(), (0x110000 / 32 + 1) * 4, 0x10000 * 4, (tables.cp_hash_mask as usize + 1) * 8, tables.cp_hash_mask + 1, (tables.seq_hash_mask as usize + 1) * 8, tables.seq_hash_mask + 1, trie.sequences.len() * 4, trie.seq_count, tables.seq_stride);

    let gpu = gpu::init_gpu();
    let al = &gpu.adapter_limits;
    println!("adapter: {} ({:?}, driver {} {}); timestamps {}; period {} ns", gpu.adapter_info.name, gpu.adapter_info.backend, gpu.adapter_info.driver, gpu.adapter_info.driver_info, if gpu.timestamps { "yes" } else { "NO (GPU pass times unavailable; wall only)" }, gpu.ts_period);
    println!("adapter limits: max_buffer_size {} ({:.0} MB), max_storage_buffer_binding_size {} ({:.0} MB), storage buffers/stage {}", al.max_buffer_size, corpus::mb(al.max_buffer_size), al.max_storage_buffer_binding_size, corpus::mb(al.max_storage_buffer_binding_size as u64), al.max_storage_buffers_per_shader_stage);
    let cap = (al.max_storage_buffer_binding_size as u64).min(al.max_buffer_size).min(1 << 31);
    let chunk_size = 1usize << (63 - cap.leading_zeros());

    let t = std::time::Instant::now();
    let atlas = atlas::Atlas::load(&gpu, &atlas_dir, &trie, emoji_sheet.as_deref());
    gpu.wait();
    match &atlas.emoji {
        Some(e) => println!("atlas: {} curves ({:.1} MiB) + glyph map ({:.0} KiB) + {} slot advances; emoji sheet {} cells, {:.1} MiB of texture with mips, decoded + uploaded in {:.0} ms; atlas total {:.0} ms", atlas.curve_count, atlas.curves_bytes as f64 / 1048576.0, atlas.glyphmap_bytes as f64 / 1024.0, trie.slot_count, e.cells, e.texture_bytes as f64 / 1048576.0, e.load_ms, ms(t)),
        None => println!("atlas: {} curves ({:.1} MiB) + glyph map ({:.0} KiB) + {} slot advances; NO emoji sheet (--no-emoji); {:.0} ms", atlas.curve_count, atlas.curves_bytes as f64 / 1048576.0, atlas.glyphmap_bytes as f64 / 1024.0, trie.slot_count, ms(t)),
    }
    let pipelines = scene::Pipelines::build(&gpu, &tables);

    // 2. --check: every slot against HyperLayout.
    let mut verdicts = Vec::new();
    if !args.check.is_empty() {
        let t = std::time::Instant::now();
        verdicts = check::run(&gpu, &pipelines, &atlas, &trie, &args.check, args.segment_bytes, chunk_size);
        println!("check: {} comparisons in {:.0} ms", verdicts.len(), ms(t));
    }

    let Some(corpus_dir) = &args.corpus else { report_checks(&verdicts); println!("loadavg at end: {}", loadavg()); return };

    // 3. The walk and Pass 1 (real resolution), the views, the buffers.
    let w = corpus::walk(corpus_dir, WalkMode::Renderer, args.threads, chunk_size);
    let st_enum = (w.stats.enumerate_ms, w.stats.read_ms, w.stats.concat_ms, w.stats.candidates, w.stats.skipped_large, w.stats.skipped_non_utf8, w.stats.read_errors, w.stats.pad_bytes);
    let (corpus, p1) = Corpus::build(&trie, w, args.threads, args.segment_bytes, WRAP_COLS);
    let source_bytes = corpus.source_bytes();
    let n_chunks = corpus.bytes.len().div_ceil(chunk_size);
    println!("walk (renderer rules): {} candidates, {} kept, {} skipped >10 MiB, {} non-UTF-8, {} unreadable; enumerate {:.0} ms, read+validate {:.0} ms ({} threads), concat {:.0} ms; {} B source ({} B chunk padding, {} chunk(s) of {:.0} MB)",
        st_enum.3, corpus.files.len(), st_enum.4, st_enum.5, st_enum.6, st_enum.0, st_enum.1, args.threads, st_enum.2, source_bytes, st_enum.7, n_chunks, corpus::mb(chunk_size as u64));
    println!("corpus: {} files, {} B, {} lines ({} with non-ASCII bytes, resolved on the CPU in Pass 1), {} glyph slots; Pass 1 ({} threads, wrap {} back) {:.1} ms = {:.0} MB/s; {} lines longer than {} B carry {} segment seeds",
        corpus.files.len(), source_bytes, corpus.line_count(), corpus.non_ascii_lines, corpus.total_glyphs, args.threads, WRAP_COLS, p1.wall_ms, source_bytes as f64 / p1.wall_ms / 1e3, corpus.seeds.len(), args.segment_bytes, corpus.seeds.values().map(|s| s.len()).sum::<usize>());
    assert!(corpus.line_count() > 0, "no lines");
    let (_, page_item) = page_view(&corpus);
    println!("page view: {} ({} B, {} lines)", corpus.files[page_item].rel, corpus.items[page_item].byte_len, corpus.items[page_item].line_count);

    let views = plan_views(&corpus, &args);
    let placed: Vec<Placement> = views.iter().map(|v| place(&corpus, v, trie.cell_adv)).collect();
    let visibles: Vec<corpus::Visible> = views.iter().zip(&placed).map(|(v, pl)| corpus::visible_list(&corpus, &v.lines, &pl.group_of_line)).collect();
    let max_slots = visibles.iter().map(|v| v.slots).max().unwrap().max(1);
    let max_segments = visibles.iter().map(|v| v.segs.len()).max().unwrap().max(1);
    let max_groups = placed.iter().map(|p| p.origins.len()).max().unwrap().max(1);
    drop(visibles);
    let buffers = scene::Buffers::new(&gpu, &pipelines, &atlas, &corpus, chunk_size, max_segments, max_slots, max_groups);
    println!("resident upload: {:.1} MB (bytes + line table + item table) in {:.0} ms", corpus::mb(buffers.resident_bytes(&corpus)), buffers.upload_ms);

    // 4. The interleaved measurement loop: per repeat, per view, per lookup variant, per draw mode.
    let modes: Vec<DrawMode> = if atlas.emoji.is_some() { vec![DrawMode::SlugEmoji, DrawMode::SlugNoEmoji, DrawMode::Flat] } else { vec![DrawMode::SlugNoEmoji, DrawMode::Flat] };
    let mut measured: Vec<Measured> = views.iter().map(|_| Measured { compute: HashMap::new(), draw: HashMap::new(), wall: HashMap::new(), cpu: Vec::new(), slots: 0, segments: 0, bytes_read: 0 }).collect();
    for rep in 0..args.repeat {
        for (vi, view) in views.iter().enumerate() {
            let pl = &placed[vi];
            scene::write_placement(&gpu, &buffers, &atlas, pl);
            for &variant in &Variant::ALL {
                for &mode in &modes {
                    let f = scene::run_frame(&gpu, &pipelines, &buffers, &corpus, trie.cell_adv, trie.seq_max, &view.lines, &pl.group_of_line, variant, mode, false);
                    let m = &mut measured[vi];
                    m.compute.entry(variant).or_default().push(f.compute_ms);
                    m.draw.entry(mode).or_default().push(f.draw_ms);
                    m.wall.entry((variant, mode)).or_default().push(f.wall_ms);
                    m.cpu.push(f.cpu_ms);
                    (m.slots, m.segments, m.bytes_read) = (f.slots, f.segments, f.bytes_read);
                    if rep + 1 == args.repeat && variant == Variant::Direct && mode == modes[0] {
                        if let Some(path) = &args.screenshot { scene::screenshot(&gpu, &pipelines, path) }
                    }
                }
            }
        }
    }

    // 5. Readback checks: every view's GPU slots against the whole-line CPU twin, both variants.
    let mut view_verdicts = Vec::new();
    for (vi, view) in views.iter().enumerate() {
        let pl = &placed[vi];
        scene::write_placement(&gpu, &buffers, &atlas, pl);
        let (cpu_slots, cpu_advs) = corpus::cpu_reference(&trie, &corpus, &view.lines, &pl.group_of_line);
        for &variant in &Variant::ALL {
            let f = scene::run_frame(&gpu, &pipelines, &buffers, &corpus, trie.cell_adv, trie.seq_max, &view.lines, &pl.group_of_line, variant, DrawMode::None, true);
            let (gpu_slots, gpu_advs) = scene::read_slots(&gpu, &buffers, f.slots, true);
            let verdict = if gpu_slots.len() != cpu_slots.len() {
                format!("FAIL: {} GPU slots vs {} CPU", gpu_slots.len(), cpu_slots.len())
            } else {
                match gpu_slots.iter().zip(&cpu_slots).zip(gpu_advs.iter().zip(&cpu_advs)).position(|((a, b), (c, d))| a != b || c.to_bits() != d.to_bits()) {
                    None => format!("PASS: {} slots bit-equal GPU (from {} segments) vs whole-line CPU twin, x as f32 bits and advances included", f.slots, f.segments),
                    Some(i) => format!("FAIL: first mismatch at slot {i}: gpu {:?} adv {} cpu {:?} adv {}; {} of {} differ", gpu_slots[i], gpu_advs[i], cpu_slots[i], cpu_advs[i], gpu_slots.iter().zip(&cpu_slots).filter(|(a, b)| a != b).count(), f.slots)
                }
            };
            view_verdicts.push((view.name.clone(), variant, verdict));
        }
    }

    // ---------------------------------------------------------------- report
    let mm = |v: &[f64]| { let (a, b) = stats(v); format!("{a:9.3} {b:9.3}") };
    println!();
    println!("=== jit-text: {} files, {} B source, {} lines, {} glyph slots; {} repeats; min / median in ms ===", corpus.files.len(), source_bytes, corpus.line_count(), corpus.total_glyphs, args.repeat);
    println!("Pass 1 (CPU, {} threads, real glyph resolution): {:.1} ms = {:.0} MB/s", args.threads, p1.wall_ms, source_bytes as f64 / p1.wall_ms / 1e3);
    println!();
    for (vi, view) in views.iter().enumerate() {
        let m = &measured[vi];
        let pl = &placed[vi];
        println!("--- view {}: {} lines, {} groups, {} segments, {} slots drawn, {} visible bytes, transient slots {:.1} MB, {:.2} px/em at {}x{} (one cell {:.2} px wide) ---",
            view.name, view.lines.len(), pl.origins.len(), m.segments, m.slots, m.bytes_read, corpus::mb(m.slots as u64 * SLOT_BYTES), pl.px_per_em, TARGET.0, TARGET.1, pl.px_per_em * trie.cell_adv);
        println!("{:<58} {:>9} {:>9}", "measurement", "min", "median");
        for &variant in &Variant::ALL {
            let v = &m.compute[&variant];
            let (min_c, _) = stats(v);
            println!("{:<58} {}   {:.2} G slots/s at min", format!("layout kernel GPU, lookup {}", variant.name()), mm(v), m.slots as f64 / min_c / 1e6);
        }
        for &mode in &modes { println!("{:<58} {}", format!("draw GPU: {}", mode.name()), mm(&m.draw[&mode])) }
        if modes.contains(&DrawMode::SlugEmoji) {
            let (slug, _) = stats(&m.draw[&DrawMode::SlugEmoji]);
            let (flat, _) = stats(&m.draw[&DrawMode::Flat]);
            println!("{:<58} {:9.3}", "cost of real text: Slug(emoji bound) - flat, at min", slug - flat);
        }
        for &variant in &Variant::ALL { for &mode in &modes { println!("{:<58} {}", format!("frame wall (submit..poll), {} + {}", variant.name(), mode.name()), mm(&m.wall[&(variant, mode)])) } }
        println!("{:<58} {}", "CPU visible list + write_buffer", mm(&m.cpu));
        println!();
    }
    println!("--- resident memory ---");
    let (b_mb, l_mb, i_mb) = (corpus::mb(corpus.bytes.len() as u64), corpus::mb(corpus.lines.len() as u64 * 8), corpus::mb(corpus.items.len() as u64 * 32));
    println!("bytes buffer {:.1} MB, line table {:.1} MB (8 B/line), item table {:.1} MB (32 B/item); lookup tables: variant A {} B, variant B {} B (+{} B ASCII shared); atlas: curves {:.1} MiB, glyph map {:.0} KiB, advances {} B{}; largest transient slot buffer {:.1} MB ({} slots)",
        b_mb, l_mb, i_mb, tables.bytes_variant_a, tables.bytes_variant_b, tables.bytes_shared_ascii, atlas.curves_bytes as f64 / 1048576.0, atlas.glyphmap_bytes as f64 / 1024.0, trie.slot_count * 4,
        atlas.emoji.as_ref().map(|e| format!(", emoji sheet {:.1} MiB", e.texture_bytes as f64 / 1048576.0)).unwrap_or_default(), corpus::mb(max_slots as u64 * SLOT_BYTES), max_slots);
    println!("HyperLayout whole-tree slots for this corpus: {:.1} MB Derived (20 B/glyph)", corpus::mb(corpus.total_glyphs * 20));
    println!();
    for (name, variant, v) in &view_verdicts { println!("readback check (view {name}, lookup {}): {v}", variant.name()) }
    report_checks(&verdicts);
    println!("not modelled: pagination (every item is one flat page; the renderer pages every repo file), syntax paint (flat colour), the far-LOD backdrop, culling, the renderer's camera motion, leader mode (cluster only), the wrap segment above 65,535 (the Derived slot's 16 bits, as in the renderer).");
    println!("loadavg at end: {}", loadavg());
}

fn report_checks(verdicts: &[check::Verdict]) {
    if verdicts.is_empty() { return }
    println!("--- --check: kernel vs HyperLayout (glyph3d-native, cluster mode, flat paint) ---");
    for v in verdicts { println!("{} {:<40} wrap {:>3} {:<22} {}", if v.pass { "PASS" } else { "FAIL" }, v.file, v.wrap, v.variant.name(), v.detail) }
    let (pass, total) = (verdicts.iter().filter(|v| v.pass).count(), verdicts.len());
    println!("check summary: {pass} of {total} PASS");
}
