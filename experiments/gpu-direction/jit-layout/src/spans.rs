//! spans.rs — colour that is not computed at load time: per-file BYTE RANGES
//! (what an LSP/AST analysis produces) and, for the files that need it, a DENSE
//! per-glyph attribute array. Both are resident tables the layout kernel reads
//! per segment; nothing here touches a slot.
//!
//! Span table: one global buffer of 8 B spans `{ byte_start (file-local),
//! len | palette << 24 }`, sorted and non-overlapping within a file; the item
//! carries `{ span_base, span_count }`; every file's range is allocated with
//! slack (`SLACK_NUM/SLACK_DEN` + 16) so an edit that grows a little rewrites
//! in place, and the buffer has a tail of headroom for one that grows a lot
//! (append-and-remap: the old range becomes a hole until a compaction nobody
//! runs here). A per-LINE index (4 B/line) names the first span that can cover
//! the line, so a segment starts its walk there.
//!
//! Dense: `color[ord]` (4 B) and `xf[ord]` (4 x f16: dx, dy, dz, scale; 8 B),
//! `ord` the glyph's ordinal within the file (line glyph prefix + column),
//! materialised only for items whose `repr` is 2 or 3 and indexed from the
//! item's `dense_base`.
//!
//! The demo spans are a heuristic, not an analysis: `//` comments to end of
//! line, every identifier-looking word (keyword or not), numbers; the variant
//! used for the edit measurement adds `#` directives and string literals so
//! the replacement has a different count.

use crate::{partition, FileSpan, Item, Line};
use bytemuck::{Pod, Zeroable};
use std::time::Instant;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Span { pub byte_start: u32, pub len_pal: u32 }

impl Span {
    pub fn new(start: u32, len: u32, pal: u32) -> Span { Span { byte_start: start, len_pal: (len & 0xFFFFFF) | (pal << 24) } }
    pub fn end(&self) -> u32 { self.byte_start + (self.len_pal & 0xFFFFFF) }
    pub fn pal(&self) -> u32 { self.len_pal >> 24 }
}

pub const PAL_COMMENT: u32 = 1;
pub const PAL_KEYWORD: u32 = 2;
pub const PAL_IDENT: u32 = 3;
pub const PAL_NUMBER: u32 = 4;
pub const PAL_PREPROC: u32 = 5;
pub const PAL_STRING: u32 = 6;

pub fn palette() -> [u32; 256] {
    let mut p = [0xFFD8E2E8u32; 256];
    p[PAL_COMMENT as usize] = 0xFF5AB06A;
    p[PAL_KEYWORD as usize] = 0xFF3278CC;
    p[PAL_IDENT as usize] = 0xFFC6B7A9;
    p[PAL_NUMBER as usize] = 0xFFBB9768;
    p[PAL_PREPROC as usize] = 0xFF9B7FBD;
    p[PAL_STRING as usize] = 0xFF7A8A6A;
    p
}

const SLACK_NUM: usize = 1;
const SLACK_DEN: usize = 8;
const SLACK_MIN: usize = 16;
/// Per-file capacity: count + 12.5% + 16 spans.
pub fn capacity_for(count: usize) -> usize { count + count * SLACK_NUM / SLACK_DEN + SLACK_MIN }

fn is_ident_start(b: u8) -> bool { b.is_ascii_alphabetic() || b == b'_' }
fn is_ident(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }

fn is_keyword(w: &[u8]) -> bool {
    matches!(w, b"fn" | b"let" | b"mut" | b"pub" | b"use" | b"mod" | b"impl" | b"struct" | b"enum" | b"match" | b"if" | b"else" | b"for" | b"while" | b"loop" | b"return" | b"break" | b"continue" | b"const" | b"static" | b"unsafe" | b"trait" | b"where" | b"as" | b"in" | b"self" | b"Self" | b"true" | b"false"
        | b"int" | b"char" | b"void" | b"unsigned" | b"long" | b"short" | b"signed" | b"float" | b"double" | b"sizeof" | b"typedef" | b"extern" | b"inline" | b"switch" | b"case" | b"default" | b"do" | b"goto" | b"volatile" | b"register" | b"union"
        | b"u8" | b"u16" | b"u32" | b"u64" | b"i8" | b"i16" | b"i32" | b"i64" | b"usize" | b"isize" | b"f32" | b"f64" | b"bool" | b"NULL" | b"def" | b"class" | b"import" | b"from" | b"function" | b"var" | b"type" | b"interface")
}

/// Demo spans of one file (file-local byte offsets). `variant` 0 is the resident set; 1 adds
/// preprocessor directives and string literals.
pub fn demo_spans(data: &[u8], variant: u32, out: &mut Vec<Span>) {
    let n = data.len();
    let mut i = 0usize;
    let mut at_line_start = true;
    while i < n {
        let b = data[i];
        if b == b'/' && i + 1 < n && data[i + 1] == b'/' {
            let end = memchr::memchr(b'\n', &data[i..]).map(|k| i + k).unwrap_or(n);
            out.push(Span::new(i as u32, (end - i) as u32, PAL_COMMENT));
            i = end;
            continue;
        }
        if variant == 1 && b == b'#' && at_line_start {
            let end = memchr::memchr(b'\n', &data[i..]).map(|k| i + k).unwrap_or(n);
            out.push(Span::new(i as u32, (end - i) as u32, PAL_PREPROC));
            i = end;
            continue;
        }
        if variant == 1 && b == b'"' {
            let mut j = i + 1;
            while j < n && data[j] != b'"' && data[j] != b'\n' { j += usize::from(data[j] == b'\\') + 1 }
            let end = (j + 1).min(n);
            out.push(Span::new(i as u32, (end - i) as u32, PAL_STRING));
            i = end;
            at_line_start = false;
            continue;
        }
        if is_ident_start(b) {
            let mut j = i + 1;
            while j < n && is_ident(data[j]) { j += 1 }
            out.push(Span::new(i as u32, (j - i) as u32, if is_keyword(&data[i..j]) { PAL_KEYWORD } else { PAL_IDENT }));
            i = j;
            at_line_start = false;
            continue;
        }
        if b.is_ascii_digit() {
            let mut j = i + 1;
            while j < n && (is_ident(data[j]) || data[j] == b'.') { j += 1 }
            out.push(Span::new(i as u32, (j - i) as u32, PAL_NUMBER));
            i = j;
            at_line_start = false;
            continue;
        }
        at_line_start = b == b'\n' || (at_line_start && (b == b' ' || b == b'\t'));
        i += 1;
    }
}

/// For each line of `item`, the index (absolute) of the first of its spans whose end is past the line's start.
pub fn line_index(spans: &[Span], base: u32, item: &Item, lines: &[Line], out: &mut [u32]) {
    let mut s = 0usize;
    for (k, o) in out.iter_mut().enumerate() {
        let local = lines[item.first_line as usize + k].byte_start - item.byte_start;
        while s < spans.len() && spans[s].end() <= local { s += 1 }
        *o = base + s as u32;
    }
}

pub struct SpanTable {
    /// The resident table as uploaded: every file's spans at its base, slack between files, headroom at the end.
    pub table: Vec<Span>,
    pub line_idx: Vec<u32>,
    pub cap: Vec<u32>,
    pub used: usize,
    pub tail: usize,
    pub build_ms: f64,
}

/// Demo spans for every file, assembled into the resident table with per-file slack, items patched
/// with `span_base/span_count` and `repr = 1` (items already dense keep their representation).
pub fn build_demo(bytes: &[u8], files: &[FileSpan], items: &mut [Item], lines: &[Line], threads: usize) -> SpanTable {
    let t = Instant::now();
    let groups = partition(&files.iter().map(|f| f.byte_len).collect::<Vec<_>>(), threads);
    let parts: Vec<Vec<(Vec<Span>, Vec<u32>)>> = std::thread::scope(|s| {
        let handles: Vec<_> = groups.iter().map(|g| {
            let (files, items) = (&files[g.clone()], &items[g.clone()]);
            s.spawn(move || {
                files.iter().zip(items).map(|(f, it)| {
                    let mut sp = Vec::with_capacity(f.byte_len / 8);
                    demo_spans(&bytes[f.byte_start..f.byte_start + f.byte_len], 0, &mut sp);
                    let mut idx = vec![0u32; it.line_count as usize];
                    line_index(&sp, 0, it, lines, &mut idx);
                    (sp, idx)
                }).collect()
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let per_file: Vec<(Vec<Span>, Vec<u32>)> = parts.into_iter().flatten().collect();
    let used: usize = per_file.iter().map(|(s, _)| s.len()).sum();
    let alloc: usize = per_file.iter().map(|(s, _)| capacity_for(s.len())).sum();
    let headroom = (used / 64).max(1 << 20);
    let mut table = vec![Span::zeroed(); alloc + headroom];
    let mut line_idx = vec![0u32; lines.len()];
    let mut cap = Vec::with_capacity(items.len());
    let mut base = 0usize;
    for (it, (sp, idx)) in items.iter_mut().zip(&per_file) {
        table[base..base + sp.len()].copy_from_slice(sp);
        for (k, v) in idx.iter().enumerate() { line_idx[it.first_line as usize + k] = base as u32 + v }
        it.span_base = base as u32;
        it.span_count = sp.len() as u32;
        if it.repr == 0 { it.repr = 1 }
        let c = capacity_for(sp.len());
        cap.push(c as u32);
        base += c;
    }
    SpanTable { table, line_idx, cap, used, tail: alloc, build_ms: t.elapsed().as_secs_f64() * 1e3 }
}

// ---------------------------------------------------------------- dense per-glyph attributes

/// IEEE half from f32, round to nearest even (normal range; our values are small and finite).
pub fn f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xFF) as i32 - 127 + 15;
    let mant = b & 0x7FFFFF;
    if exp <= 0 {
        if exp < -10 { return sign }
        let m = mant | 0x800000;
        let shift = (14 - exp) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let round = (rem > (1 << (shift - 1))) || (rem == (1 << (shift - 1)) && (half & 1) == 1);
        return sign | (half as u16 + u16::from(round));
    }
    if exp >= 31 { return sign | 0x7C00 }
    let mut half = ((exp as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1FFF;
    if rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1) { half += 1 }
    sign | half as u16
}

pub fn pack_xf(dx: f32, dy: f32, dz: f32, scale: f32) -> [u32; 2] {
    [f16(dx) as u32 | (f16(dy) as u32) << 16, f16(dz) as u32 | (f16(scale) as u32) << 16]
}

/// A heatmap colour by glyph ordinal (the deranged 1:1 case: every glyph its own colour). `seed` varies the edit.
pub fn dense_color_for(ord: u32, seed: u32) -> u32 {
    let h = (ord ^ seed).wrapping_mul(2654435761);
    let t = (ord % 97) as f32 / 97.0;
    let r = (255.0 * t) as u32;
    let g = (255.0 * (1.0 - t)) as u32;
    let b = 64 + (h >> 25);
    0xFF000000 | (b << 16) | (g << 8) | r
}

/// A per-glyph transform by ordinal: a small sideways wiggle, a 10% z lift every 7th glyph, unit scale.
pub fn dense_xf_for(ord: u32, seed: u32) -> [u32; 2] {
    let a = ((ord ^ seed) % 61) as f32 / 61.0 * std::f32::consts::TAU;
    pack_xf(0.05 * a.sin(), 0.05 * a.cos(), if ord % 7 == 0 { 0.1 } else { 0.0 }, 1.0)
}

pub struct Dense { pub color: Vec<u32>, pub xf: Vec<[u32; 2]>, pub items: Vec<usize>, pub build_ms: f64 }

/// Materialise the dense arrays for `which` items (repr 2 = colour, 3 = colour + xf), assigning `dense_base`.
pub fn build_dense(items: &mut [Item], which: &[usize], with_xf: bool, seed: u32) -> Dense {
    let t = Instant::now();
    let total: usize = which.iter().map(|&i| items[i].glyph_count as usize).sum();
    let mut color = Vec::with_capacity(total);
    let mut xf = Vec::with_capacity(if with_xf { total } else { 0 });
    for &i in which {
        let it = &mut items[i];
        it.repr = if with_xf { 3 } else { 2 };
        it.dense_base = color.len() as u32;
        for ord in 0..it.glyph_count {
            color.push(dense_color_for(ord, seed));
            if with_xf { xf.push(dense_xf_for(ord, seed)) }
        }
    }
    Dense { color, xf, items: which.to_vec(), build_ms: t.elapsed().as_secs_f64() * 1e3 }
}
