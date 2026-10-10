//! `--check`: every slot the kernel emits for a fixture — glyph id, advance,
//! x as f32 bits (and the wrap segment) — against HyperLayout itself, the
//! native crate's production layout engine linked as a library, over the same
//! bytes with the same item parameters (cluster mode, flat paint). Run twice:
//! wrap 0 (the single-file form `engine_layout` uses) and the views' wrap
//! (100 columns, a wrap steps back in depth), and in both lookup variants.

use std::path::Path;

use glyph3d_native::fold::{ClusterMode, WrapMode};
use glyph3d_native::layout::{GlyphArena, ItemParams, LayoutEngine, LayoutGlyphs, LayoutItem, Paint};

use crate::atlas::Atlas;
use crate::corpus::{self, Corpus, WalkMode, FLAT_COLOR, LINE_HEIGHT, WRAP_COLS};
use crate::gpu::Gpu;
use crate::scene::{self, Buffers, DrawMode, Pipelines, Variant};
use crate::trie::Trie;

pub struct Verdict { pub file: String, pub wrap: u32, pub variant: Variant, pub pass: bool, pub detail: String }

/// HyperLayout over `bytes` as one item: (glyph_id, x, advance, col) per slot, in slot order.
fn reference(bytes: &[u8], wrap: u32) -> Vec<(u32, f32, f32, u32)> {
    let params = ItemParams {
        line_height: LINE_HEIGHT as f64,
        wrap_width: wrap as i32,
        wrap_mode: WrapMode::Back,
        cluster_mode: ClusterMode::Cluster,
        z_step: corpus::Z_STEP as f64,
        ..Default::default()
    };
    let item = LayoutItem { bytes, params, group_id: 0, paint: Paint::Flat(FLAT_COLOR) };
    let mut engine = LayoutEngine::hyper();
    let mut arena = GlyphArena::new();
    let placements = engine.layout_items(&[item], &mut arena).expect("HyperLayout refused the item");
    assert_eq!(placements.len(), 1);
    arena.instances().iter().map(|g| (g.glyph_id, g.pos[0], g.advance, g.col)).collect()
}

/// Lay the file out with the kernel and compare. The corpus is the one file,
/// so every line is visible and the kernel's segments are the whole item.
pub fn check_file(gpu: &Gpu, p: &Pipelines, atlas: &Atlas, trie: &Trie, path: &Path, rel: &str, wrap: u32, segment_bytes: usize, chunk_size: usize) -> Vec<Verdict> {
    let w = corpus::walk(path, WalkMode::All, 1, chunk_size);
    assert_eq!(w.files.len(), 1, "{rel}: expected one file");
    let (c, _) = Corpus::build(trie, w, 1, segment_bytes, wrap);
    let lines: Vec<u32> = (0..c.line_count() as u32).collect();
    let groups = vec![0u32; lines.len()];
    let vis = corpus::visible_list(&c, &lines, &groups);
    let b = Buffers::new(gpu, p, atlas, &c, chunk_size, vis.segs.len(), vis.slots, 1);
    let want = reference(&c.bytes[..c.files[0].real_len], wrap);
    Variant::ALL.iter().map(|&variant| {
        let f = scene::run_frame(gpu, p, &b, &c, trie.cell_adv, trie.seq_max, &lines, &groups, variant, DrawMode::None, true);
        let (slots, advs) = scene::read_slots(gpu, &b, f.slots, true);
        let detail = compare(&c, &slots, &advs, &want, wrap);
        Verdict { file: rel.to_string(), wrap, variant, pass: detail.starts_with("PASS"), detail }
    }).collect()
}

fn compare(c: &Corpus, slots: &[corpus::Slot], advs: &[f32], want: &[(u32, f32, f32, u32)], wrap: u32) -> String {
    if slots.len() != want.len() {
        return format!("FAIL: {} slots from the kernel, HyperLayout has {} (first {} compared below){}", slots.len(), want.len(), slots.len().min(want.len()),
            first_mismatch(c, slots, advs, want, wrap).map(|m| format!("; {m}")).unwrap_or_default());
    }
    match first_mismatch(c, slots, advs, want, wrap) {
        None => format!("PASS: {} slots bit-equal to HyperLayout (glyph id, advance, x as f32 bits, wrap segment)", slots.len()),
        Some(m) => format!("FAIL: {m}"),
    }
}

/// The first slot that differs, named by its byte offset in the file (HyperLayout's
/// `col` is the record index within its line; the byte offset is recovered by
/// re-walking the line with the CPU twin).
fn first_mismatch(c: &Corpus, slots: &[corpus::Slot], advs: &[f32], want: &[(u32, f32, f32, u32)], wrap: u32) -> Option<String> {
    let n = slots.len().min(want.len());
    let differs = |i: usize| {
        let (g, x, a, col) = want[i];
        let s = slots[i];
        let seg_want = if wrap > 0 { col / wrap } else { 0 };
        (s.glyph_and_wrap & 0xFFFF) != g || s.x.to_bits() != x.to_bits() || advs[i].to_bits() != a.to_bits() || (s.glyph_and_wrap >> 16) != seg_want
    };
    let i = (0..n).find(|&i| differs(i))?;
    let (g, x, a, col) = want[i];
    let s = slots[i];
    let count = (0..n).filter(|&i| differs(i)).count();
    let byte = byte_of_slot(c, i);
    Some(format!("slot {i} (row {}, byte offset {}): expected glyph {g} adv {a} x {x} ({:#010x}) seg {}; got glyph {} adv {} x {} ({:#010x}) seg {}; {count} of {n} differ",
        s.row, byte.map(|b| b.to_string()).unwrap_or_else(|| "?".into()), x.to_bits(), if wrap > 0 { col / wrap } else { 0 },
        s.glyph_and_wrap & 0xFFFF, advs[i], s.x, s.x.to_bits(), s.glyph_and_wrap >> 16))
}

fn byte_of_slot(c: &Corpus, slot: usize) -> Option<usize> {
    let trie = crate::trie_ref();
    let mut k = 0usize;
    for li in 0..c.line_count() as u32 {
        let n = c.lines[li as usize].glyphs as usize;
        if slot >= k + n { k += n; continue }
        let it = &c.items[c.item_of(li)];
        let start = c.lines[li as usize].byte_start as usize;
        let end = start + c.line_len(li, it) as usize;
        let lim = it.byte_start as usize + it.real_len as usize;
        let mut f = corpus::Fold::new();
        let mut i = start;
        while i < end {
            let (r, len) = f.step(trie, &c.bytes, i, end, lim, c.wrap);
            if let Some((glyph, ..)) = r {
                if glyph != 0 { if k == slot { return Some(i - it.byte_start as usize) } k += 1 }
            }
            i += len;
        }
        return None;
    }
    None
}

/// Every file under `path` (a file, or a directory walked with every file kept), sorted.
pub fn files_of(path: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut v = corpus::enumerate(path, WalkMode::All);
    if path.is_dir() {
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for (rel, _) in &mut v { *rel = format!("{base}/{rel}") }
    }
    v
}

pub fn run(gpu: &Gpu, p: &Pipelines, atlas: &Atlas, trie: &Trie, paths: &[std::path::PathBuf], segment_bytes: usize, chunk_size: usize) -> Vec<Verdict> {
    let mut out = Vec::new();
    for path in paths {
        for (rel, file) in files_of(path) {
            for wrap in [0u32, WRAP_COLS] {
                out.extend(check_file(gpu, p, atlas, trie, &file, &rel, wrap, segment_bytes, chunk_size));
            }
        }
    }
    out
}
