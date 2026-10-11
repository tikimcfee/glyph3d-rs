//! The Visible field on a real adapter: a tiny synthetic corpus (ASCII
//! lines, a line longer than `segment_bytes` with seeds from a serial walk,
//! multi-byte UTF-8 and emoji sequences in both cluster modes, a paged
//! WrapDown item, an empty item) laid out headless through
//! `layout_all_lines` and compared slot for slot — x bits, row lane,
//! glyph|wrap, colour, item lane — against a CPU twin written here from
//! `char_resolve.rs` and `pass2_device.rs`'s rules; then the full frame path
//! (`prepare` with the cull, the indirect dispatches, the draws into an
//! offscreen target) against the same twin as a set, with a hidden item, a
//! wash-only frame, an empty frustum, and the stats ring.
//!
//! The twin computes x in the reference's f64 form (`reference_x`), so it is
//! independent of the kernel's `fma` — a device whose fma is not fused would
//! fail here on the paged and foldless items.
//!
//! M3 (2026-10-10): the twin also records each slot's (item, byte) key, so
//! the byte-keyed paths are judged against it — per-glyph overrides in the
//! headless layout (colour, an x nudge as the host's f32 add, group rows
//! through the Derived override lane), the selection mask holding exactly
//! the slots whose byte is in range, `set_item_span_range` merging on the
//! GPU path, and `locate` against a readback search over random probes.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use glyph_field::{FieldResources, FieldTargets, FramePrepare, GlyphField, GroupRow, ItemParamsGpu};
use glyph_field_derived::DerivedSlot;
use glyph_field_visible::test_support::{synthetic_trie, ADVANCE_FU, EM_HEIGHT_FU};
use glyph_field_visible::{
    layout_all_lines, layout_all_lines_with_overrides, reference_x, tables, ByteSpanGpu, GlyphOverride, LineEntryGpu, SegmentSeedGpu,
    TrieUpload, VisibleField, VisibleInputs, VisibleItem, VisibleLimits, NO_GROUP, WRAP_BACK, WRAP_DOWN,
};

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

fn device() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .expect("an adapter (this test needs a GPU)");
    let mut features = wgpu::Features::empty();
    if adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
        features |= wgpu::Features::TIMESTAMP_QUERY;
    }
    block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("glyph-field-visible test device"),
        required_features: features,
        required_limits: adapter.limits(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device")
}

// ── the CPU twin ─────────────────────────────────────────────────────────────

const DEFAULT_COLOR: u32 = 0xFF_D4D4D4;

struct Twin<'a> {
    trie: &'a TrieUpload,
}

impl Twin<'_> {
    fn entry(&self, cp: u32) -> (u32, u32) {
        let t = self.trie;
        let block = if cp <= 0x10FFFF { t.block_index[(cp >> t.block_shift) as usize] } else { 0 };
        let e = (((block << t.block_shift) | (cp & 0xFF)) * 4) as usize;
        (t.blocks[e], t.blocks[e + 1] / ADVANCE_FU)
    }
    fn starts_seq(&self, cp: u32) -> bool {
        let stride = 2 + self.trie.seq_max as usize;
        self.trie.sequences.chunks_exact(stride).any(|e| e[2] == cp)
    }
    fn seq_lookup(&self, key: &[u32]) -> Option<u32> {
        let stride = 2 + self.trie.seq_max as usize;
        self.trie.sequences.chunks_exact(stride).find(|e| &e[2..2 + e[1] as usize] == key).map(|e| e[0])
    }
}

fn seq_len(b: u8) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else if b & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn decode(bytes: &[u8], i: usize, len: usize) -> u32 {
    let at = |k: usize| bytes.get(i + k).copied().unwrap_or(0) as u32;
    match len {
        1 => at(0),
        2 => ((at(0) & 0x1F) << 6) | (at(1) & 0x3F),
        3 => ((at(0) & 0x0F) << 12) | ((at(1) & 0x3F) << 6) | (at(2) & 0x3F),
        _ => ((at(0) & 0x07) << 18) | ((at(1) & 0x3F) << 12) | ((at(2) & 0x3F) << 6) | (at(3) & 0x3F),
    }
}

fn is_static_zero(cp: u32) -> bool {
    cp == 0x200D || (0xFE00..=0xFE0F).contains(&cp) || (0xE0020..=0xE007F).contains(&cp)
}

/// `char_resolve.rs::resolve_byte_char_cluster` over the item's bytes:
/// `(glyph, cells, byte length)` for a leader, None for a non-leader.
fn resolve(tw: &Twin, bytes: &[u8], i: usize, cluster: bool, trailer_until: &mut usize) -> Option<(u32, u32, usize)> {
    let b = bytes[i];
    let len = seq_len(b);
    if len == 0 {
        return None;
    }
    let cp = decode(bytes, i, len);
    let (glyph, cells) = tw.entry(cp);
    if !cluster {
        return Some((glyph, cells, len));
    }
    if i < *trailer_until || is_static_zero(cp) {
        return Some((0, 0, len));
    }
    if tw.starts_seq(cp) {
        let mut key = vec![cp];
        let mut key_end = vec![i + len];
        let mut p = i + len;
        while p < bytes.len() && key.len() < tw.trie.seq_max as usize {
            let n2 = seq_len(bytes[p]);
            if n2 == 0 {
                break;
            }
            let cp2 = decode(bytes, p, n2);
            if cp2 == 0x0A || cp2 == 0xFE0E {
                break;
            }
            if cp2 != 0xFE0F {
                key.push(cp2);
                key_end.push(p + n2);
            }
            p += n2;
        }
        for try_len in (2..=key.len()).rev() {
            if let Some(slot) = tw.seq_lookup(&key[..try_len]) {
                *trailer_until = key_end[try_len - 1];
                return Some((slot, 2, len));
            }
        }
    }
    Some((glyph, cells, len))
}

struct Corpus {
    items: Vec<VisibleItem>,
    bytes: Vec<Vec<u8>>,
    lines: Vec<LineEntryGpu>,
    seeds: Vec<SegmentSeedGpu>,
    spans: Vec<ByteSpanGpu>,
    expected: Vec<DerivedSlot>,
    /// Per item, its slot range in `expected`.
    item_slots: Vec<std::ops::Range<usize>>,
    /// Per expected slot, its (item, item-relative leader byte).
    expected_keys: Vec<(u32, u32)>,
    /// Per expected slot, its advance in cells (the wash extent witness).
    expected_cells: Vec<u32>,
    /// Per line, the cells advanced by leaders that produce NO slot (a
    /// missing glyph, a malformed lead): the fold's cursor moves, the
    /// glyphs do not, so a wash may reach that far past the last glyph.
    line_blank_cells: Vec<u32>,
}

impl Corpus {
    /// The expected slot of the glyph whose leader is `byte` of `item`, if
    /// that byte is a surviving leader.
    fn slot_of(&self, item: u32, byte: u32) -> Option<usize> {
        self.expected_keys.iter().position(|&k| k == (item, byte))
    }
}

struct ItemSpec {
    text: Vec<u8>,
    params: ItemParamsGpu,
    origin_x: f64,
    stride_x: f64,
    wrap_width: i32,
    wrap_mode: u32,
    cluster: bool,
    spans: Vec<ByteSpanGpu>,
    group: u32,
}

fn rows_for_line(len: i64, wrap: i64, down: bool) -> i64 {
    if !down || wrap <= 0 || len <= 0 {
        1
    } else {
        (len - 1) / wrap + 1
    }
}

/// Lay the specs out serially (whole lines, no cuts) and record the tables
/// the field is built from, cutting seeds every `segment_bytes` before an
/// ASCII byte as `line_table.rs` does.
fn build(trie: &TrieUpload, specs: &[ItemSpec], segment_bytes: usize) -> Corpus {
    let tw = Twin { trie };
    let cell_adv = tables::fu_to_world(ADVANCE_FU as i32, EM_HEIGHT_FU);
    let mut c = Corpus { items: vec![], bytes: vec![], lines: vec![], seeds: vec![], spans: vec![], expected: vec![], item_slots: vec![], expected_keys: vec![], expected_cells: vec![], line_blank_cells: vec![] };
    let mut byte_base = 0u64;
    for (idx, s) in specs.iter().enumerate() {
        let bytes = &s.text;
        let item_slot_start = c.expected.len();
        let first_line = c.lines.len() as u32;
        let span_base = c.spans.len() as u32;
        c.spans.extend_from_slice(&s.spans);
        let p = &s.params;
        let fold_unit = if s.wrap_width > 0 { s.wrap_width as i64 } else if p.has_page != 0 { p.page_cols as i64 } else { 0 };
        let down = s.wrap_mode == WRAP_DOWN;
        let page_active = p.has_page != 0 && (p.page_rows != 0 || p.page_cols != 0 || p.scroll_rows != 0);
        let mut base_row = 0i64;
        let mut line_start = 0usize;
        while line_start < bytes.len() {
            let line_end = bytes[line_start..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |o| line_start + o);
            let line_idx = c.lines.len() as u32;
            let (mut col, mut cells, mut seg_adv, mut glyphs) = (0i64, 0u32, 0f32, 0u32);
            // The line's widest fold unit in cells (`LineEntryGpu::width_cells`),
            // and the cells its slot-less leaders advanced.
            let (mut unit_cells, mut width, mut blank_cells) = (0u32, 0u32, 0u32);
            let mut trailer_until = 0usize;
            let mut seg_start = line_start;
            let mut i = line_start;
            let mut si = s.spans.iter().position(|sp| sp.end as usize > line_start).unwrap_or(s.spans.len());
            while i < line_end {
                if i - seg_start >= segment_bytes && bytes[i] < 0x80 {
                    c.seeds.push(SegmentSeedGpu { line: line_idx, byte_offset: (i - line_start) as u32, col: col as u32, seg_adv, cells, _pad: 0 });
                    seg_start = i;
                }
                let Some((glyph, k, _len)) = resolve(&tw, bytes, i, s.cluster, &mut trailer_until) else {
                    i += 1;
                    continue;
                };
                if glyph != 0 {
                    let wrap = s.wrap_width as i64;
                    let wrap_segment = if wrap > 0 { col / wrap } else { 0 };
                    let row = base_row + if down { wrap_segment } else { 0 };
                    let x_page = if p.has_page != 0 && p.page_cols > 0 { col / p.page_cols as i64 } else { 0 };
                    let m = if page_active {
                        let screen_row = row - p.scroll_rows as i64;
                        let y_page = if p.page_rows > 0 && screen_row >= p.page_rows as i64 { screen_row / p.page_rows as i64 } else { 0 };
                        let pages_wide = if p.pages_wide > 1 { p.pages_wide as i64 } else { 1 };
                        Some((y_page % pages_wide) as u32)
                    } else {
                        None
                    };
                    let x = reference_x(fold_unit > 0, seg_adv, cells, cell_adv, s.origin_x, m, s.stride_x);
                    while si < s.spans.len() && i as u32 >= s.spans[si].end {
                        si += 1;
                    }
                    let color = if si < s.spans.len() && i as u32 >= s.spans[si].start { s.spans[si].color } else { DEFAULT_COLOR };
                    c.expected.push(DerivedSlot::with_item_and_group(
                        x,
                        glyph_field_derived::pack_row(row as u32, x_page as u32),
                        glyph as u16,
                        wrap_segment as u16,
                        color,
                        glyph_field_derived::item_lane(idx as u32),
                    ));
                    c.expected_keys.push((idx as u32, i as u32));
                    c.expected_cells.push(k);
                    glyphs += 1;
                } else {
                    blank_cells += k;
                }
                col += 1;
                cells += k;
                unit_cells += k;
                if fold_unit > 0 && col % fold_unit == 0 {
                    width = width.max(unit_cells);
                    unit_cells = 0;
                    seg_adv = 0.0;
                } else {
                    seg_adv += k as f32 * cell_adv;
                }
                // Per byte, as the reference walks (a malformed lead's
                // following bytes are classified on their own).
                i += 1;
            }
            c.lines.push(LineEntryGpu {
                byte_start: line_start as u32,
                item: idx as u32,
                base_row: base_row as u32,
                glyph_count: glyphs,
                cols: col as u32,
                width_cells: width.max(unit_cells),
            });
            c.line_blank_cells.push(blank_cells);
            base_row += rows_for_line(col, s.wrap_width as i64, down);
            line_start = line_end + 1;
        }
        c.items.push(VisibleItem {
            params: s.params,
            origin_x: s.origin_x,
            stride_x: s.stride_x,
            wrap_width: s.wrap_width,
            wrap_mode: s.wrap_mode,
            cluster: u32::from(s.cluster),
            byte_base,
            byte_len: bytes.len() as u32,
            first_line,
            line_count: c.lines.len() as u32 - first_line,
            span_base,
            span_count: s.spans.len() as u32,
            bbox_min: [-50.0 + idx as f32 * 100.0, -200.0, -50.0],
            bbox_max: [50.0 + idx as f32 * 100.0, 10.0, 50.0],
            group_id: s.group,
        });
        byte_base += bytes.len() as u64;
        c.bytes.push(bytes.clone());
        c.item_slots.push(item_slot_start..c.expected.len());
    }
    c
}

fn corpus() -> (TrieUpload, Corpus) {
    let trie = synthetic_trie();
    let plain = ItemParamsGpu { line_height: 1.25, group: 0, ..Default::default() };
    let paged = ItemParamsGpu {
        line_height: 1.25,
        page_rows: 4,
        pages_wide: 2,
        has_page: 1,
        band_stride_y: 30.0,
        depth_per_band: 0.5,
        z_step: 0.15,
        group: 1,
        ..Default::default()
    };
    // A long line mixing ASCII, two- and three-byte codepoints and every
    // sequence class: keycaps (ASCII heads, one with VS16 inside), a ZWJ
    // family, a lone ZWJ, a VS16 after a non-head, an unlisted chain.
    let mut long = Vec::new();
    for k in 0..6 {
        long.extend_from_slice(format!("seg{k} caf\u{e9} \u{4e2d} #\u{20e3} 1\u{fe0f}\u{20e3} \u{1f468}\u{200d}\u{1f469} x\u{200d}y a\u{fe0f}b \u{1f468}\u{1f469} ").as_bytes());
    }
    let mut text1 = long.clone();
    text1.extend_from_slice(b"\nshort\n\nanother line here\n");
    for k in 0..6 {
        text1.extend_from_slice(format!("line {k} on a later page\n").as_bytes());
    }
    let specs = vec![
        ItemSpec {
            // The fourth line is malformed UTF-8: a three-byte lead followed
            // by ASCII, a stray continuation byte, a two-byte lead before the
            // newline and a four-byte lead cut by the item's end.
            text: b"hello world\nfoo\tbar \x7f!\n\n  indented line\nbad \xe2AB \x80 x\xc3\nno newline at end\xf0".to_vec(),
            params: plain,
            origin_x: 3.5 + 2f64.powi(-33),
            stride_x: 0.0,
            wrap_width: 0,
            wrap_mode: WRAP_BACK,
            cluster: true,
            spans: vec![ByteSpanGpu { start: 0, end: 5, color: 0xFF2020FF }, ByteSpanGpu { start: 6, end: 11, color: 0xFF20FF20 }, ByteSpanGpu { start: 24, end: 40, color: 0xFFFF2020 }],
            group: 0,
        },
        ItemSpec {
            text: text1,
            params: paged,
            // Neither is f32-representable: the kernel's (hi, lo) halves and
            // single rounding are what the oracle tier found wanting.
            origin_x: -2.25 + 2f64.powi(-30),
            stride_x: 7.5 + 3.0 * 2f64.powi(-27),
            wrap_width: 10,
            wrap_mode: WRAP_DOWN,
            cluster: true,
            spans: vec![ByteSpanGpu { start: 0, end: 3, color: 0xFF0000FF }, ByteSpanGpu { start: 70, end: 200, color: 0xFF00FF00 }],
            group: 1,
        },
        ItemSpec { text: long, params: plain, origin_x: 0.0, stride_x: 0.0, wrap_width: 0, wrap_mode: WRAP_BACK, cluster: false, spans: vec![], group: 0 },
        ItemSpec { text: Vec::new(), params: plain, origin_x: 0.0, stride_x: 0.0, wrap_width: 0, wrap_mode: WRAP_BACK, cluster: true, spans: vec![], group: 0 },
        ItemSpec {
            text: b"back wrapped line that folds into depth several times over\n".to_vec(),
            params: ItemParamsGpu { line_height: 1.25, z_step: 0.15, ..Default::default() },
            origin_x: 1.0,
            stride_x: 0.0,
            wrap_width: 8,
            wrap_mode: WRAP_BACK,
            cluster: true,
            spans: vec![],
            group: 0,
        },
    ];
    let c = build(&trie, &specs, 64);
    (trie, c)
}

fn inputs<'a>(trie: &'a TrieUpload, c: &'a Corpus, byte_refs: &'a [&'a [u8]]) -> VisibleInputs<'a> {
    VisibleInputs {
        item_bytes: byte_refs,
        items: &c.items,
        lines: &c.lines,
        seeds: &c.seeds,
        segment_bytes: 64,
        trie,
        spans: &c.spans,
        default_color: DEFAULT_COLOR,
    }
}

fn describe(s: &DerivedSlot) -> String {
    format!(
        "x={} ({:#010x}) row={} x_page={} glyph={} wrap={} color={:#010x} item={}",
        s.x,
        s.x.to_bits(),
        glyph_field_derived::row_of(s.row),
        glyph_field_derived::x_page_of(s.row),
        s.glyph_id(),
        s.wrap_segment(),
        s.color,
        glyph_field_derived::item_of(s.item_and_group)
    )
}

fn assert_slots_equal(got: &[DerivedSlot], want: &[DerivedSlot], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: slot count");
    let mut bad = 0;
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        if g.x.to_bits() != w.x.to_bits() || g.row != w.row || g.glyph_and_wrap != w.glyph_and_wrap || g.color != w.color || g.item_and_group != w.item_and_group {
            if bad < 12 {
                eprintln!("{what}: slot {k}\n   got  {}\n   want {}", describe(g), describe(w));
            }
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "{what}: {bad} of {} slots differ", want.len());
}

fn sort_key(s: &DerivedSlot) -> (u32, u32, u32, u32, u32) {
    (s.item_and_group, s.row, s.x.to_bits(), s.glyph_and_wrap, s.color)
}

#[test]
fn layout_all_lines_matches_the_cpu_twin() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    // The corpus exercises what it claims to.
    assert!(c.seeds.len() >= 10, "the long line must carry seeds ({})", c.seeds.len());
    assert!(c.expected.iter().any(|s| s.glyph_id() == 9000), "a keycap resolved");
    assert!(c.expected.iter().any(|s| s.glyph_id() == 9001), "a keycap with VS16 inside resolved");
    assert!(c.expected.iter().any(|s| s.glyph_id() == 9002), "a ZWJ family resolved");
    assert!(c.expected.iter().any(|s| s.wrap_segment() > 0), "a wrapped segment");
    assert!(c.expected.iter().any(|s| glyph_field_derived::row_of(s.row) >= 8), "rows past the second page");
    assert!(c.expected.iter().any(|s| s.color == 0xFF00FF00), "a span colour inside a seeded segment");
    let got = layout_all_lines(&device, &queue, &inp);
    assert_slots_equal(&got, &c.expected, "layout_all_lines");
    assert!(got.len() > 400, "{} slots", got.len());
}

struct Dummy {
    frame_uniform: wgpu::Buffer,
    group_table: wgpu::Buffer,
    params: wgpu::Buffer,
    glyph_advances: wgpu::Buffer,
    glyph_map: wgpu::TextureView,
    curves: wgpu::TextureView,
    emoji: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

impl Dummy {
    fn new(device: &wgpu::Device) -> Self {
        use wgpu::util::DeviceExt;
        let uniform = |label: &str, size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
        };
        let mut groups = vec![GroupRow::identity([0.0; 3]), GroupRow::identity([100.0, 0.0, 0.0])];
        groups[1].cols[3] = [2.0, 2.0, 2.0, 0.0]; // group 1 scaled x2
        let group_table = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("groups"),
            contents: bytemuck::cast_slice(&groups),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let advances = vec![0.53f32; 65536];
        let glyph_advances = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("advances"),
            contents: bytemuck::cast_slice(&advances),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let tex = |label: &str, format: wgpu::TextureFormat, layers: u32| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: layers },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(if layers > 1 { wgpu::TextureViewDimension::D2Array } else { wgpu::TextureViewDimension::D2 }),
                    ..Default::default()
                })
        };
        Self {
            frame_uniform: uniform("frame", 256),
            group_table,
            params: uniform("params", 128),
            glyph_advances,
            glyph_map: tex("glyphmap", wgpu::TextureFormat::Rgba32Uint, 1),
            curves: tex("curves", wgpu::TextureFormat::Rgba32Uint, 1),
            emoji: tex("emoji", wgpu::TextureFormat::Rgba8UnormSrgb, 2),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor::default()),
        }
    }
    fn resources<'a>(&'a self, item_params: &'a [ItemParamsGpu]) -> FieldResources<'a> {
        FieldResources {
            frame_uniform: &self.frame_uniform,
            group_table: &self.group_table,
            glyph_map: &self.glyph_map,
            curves: &self.curves,
            params: &self.params,
            emoji_sheet: &self.emoji,
            emoji_sampler: &self.sampler,
            glyph_advances: &self.glyph_advances,
            item_params,
        }
    }
}

const TARGETS: FieldTargets = FieldTargets { color_format: wgpu::TextureFormat::Rgba8UnormSrgb, depth_format: wgpu::TextureFormat::Depth32Float, sample_count: 1 };

/// An orthographic camera over x, y in +-1000 and z in [-100, 100] (wgpu
/// clip z in [0, 1]), the eye above the plane; `shift_x` moves the world
/// off screen.
fn frame(shift_x: f32, lod_glyph_px: f32, lod_backdrop_px: f32, greek_mode: u32, debug_tint: u32) -> FramePrepare {
    FramePrepare {
        view_proj: [[1.0 / 1000.0, 0.0, 0.0, 0.0], [0.0, 1.0 / 1000.0, 0.0, 0.0], [0.0, 0.0, 1.0 / 200.0, 0.0], [shift_x, 0.0, 0.5, 1.0]],
        eye: [0.0, 0.0, 60.0],
        viewport: [640.0, 480.0],
        px_scale: 1000.0,
        lod_glyph_px,
        lod_backdrop_px,
        greek_mode,
        debug_tint,
        time: 0.0,
    }
}

/// The scene's mask target format (`glyph_scene/target.rs::MASK_FORMAT`).
const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn target(device: &wgpu::Device, label: &str, format: wgpu::TextureFormat) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// One frame: prepare, then a render pass with both draws into an offscreen
/// target (so the pipelines and bind groups are validated), submitted.
fn run_frame(device: &wgpu::Device, queue: &wgpu::Queue, field: &VisibleField, f: &FramePrepare) {
    run_frame_with_mask(device, queue, field, f, None);
}

/// The same frame with the scene's selection pass: `prepare_mask` after
/// `prepare` in the frame's encoder (when a selection is given), the glyph
/// pass, then the mask pass — the mask pipeline built from the FIELD's core
/// as the scene builds it, over the mask core's bind group, into a mask
/// target with no depth. `selection` is `(item, start, end)`.
fn run_frame_with_mask(device: &wgpu::Device, queue: &wgpu::Queue, field: &VisibleField, f: &FramePrepare, selection: Option<(&wgpu::RenderPipeline, u32, u32, u32)>) {
    let cv = target(device, "color", TARGETS.color_format);
    let dv = target(device, "depth", TARGETS.depth_format);
    let mv = target(device, "mask", MASK_FORMAT);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    field.prepare(queue, &mut encoder, f);
    if let Some((_, item, start, end)) = selection {
        field.prepare_mask(queue, &mut encoder, item, start, end);
    }
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glyphs"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &cv,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &dv,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0.0), store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(field.glyph_pipeline());
        field.record_draws(&mut pass, &[]);
        field.record_wash_draw(&mut pass);
    }
    if let Some((mask_pipeline, ..)) = selection {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("selection mask"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &mv,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(mask_pipeline);
        field.record_mask_draw(&mut pass);
    }
    queue.submit([encoder.finish()]);
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
}

#[test]
fn the_frame_path_culls_lays_out_and_draws() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let params: Vec<ItemParamsGpu> = c.items.iter().map(|i| i.params).collect();
    let dummy = Dummy::new(&device);
    let field = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 1 << 12 });
    let total = c.expected.len() as u32;
    let items = c.items.len() as u32;
    let lines = c.lines.len() as u32;
    let segments = lines + c.seeds.len() as u32;

    // 1. Everything in view, every line glyph tier: the transient slots are
    //    the twin's, in the twin's order — arena order, (item, line, byte):
    //    the cull places lines by scans since C30, by atomics before.
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::ITEMS_VISIBLE], items, "items visible");
    assert_eq!(k[tables::counter::LINES_CANDIDATE], lines, "candidate lines");
    assert_eq!(k[tables::counter::LINES_GLYPH], lines, "glyph-tier lines");
    assert_eq!(k[tables::counter::LINES_WASH], 0);
    assert_eq!(k[tables::counter::SEG_FIT_END], segments, "segments");
    assert_eq!(k[tables::counter::SLOT_FIT_END], total, "slots");
    assert_eq!(k[tables::counter::SLOTS_DROPPED], 0);
    let got = field.read_slots(&queue, total);
    assert_slots_equal(&got, &c.expected, "frame slots (in arena order)");

    // 2. A hidden item: neither laid out nor counted.
    field.set_item_hidden(&queue, 1, true);
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::ITEMS_HIDDEN], 1);
    assert_eq!(k[tables::counter::ITEMS_VISIBLE], items - 1);
    assert_eq!(k[tables::counter::SLOT_FIT_END], total - c.item_slots[1].len() as u32, "slots without item 1");
    field.set_item_hidden(&queue, 1, false);

    // 3. Wash only (threshold above every row): one quad per line, no slots.
    run_frame(&device, &queue, &field, &frame(0.0, 1e9, 0.0, 2, 1));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::LINES_WASH], lines);
    assert_eq!(k[tables::counter::LINES_GLYPH], 0);
    assert_eq!(k[tables::counter::WASH], lines);
    assert_eq!(k[tables::counter::SLOT_FIT_END], 0);
    // greek_mode 0: wash-tier lines produce nothing.
    run_frame(&device, &queue, &field, &frame(0.0, 1e9, 0.0, 0, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::WASH], 0);
    assert_eq!(k[tables::counter::LINES_WASH], lines);

    // 4. Backdrop threshold above every row: items are backdrops, no lines.
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 1e9, 1, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::ITEMS_BACKDROP], items);
    assert_eq!(k[tables::counter::LINES_CANDIDATE], 0);

    // 5. The world off screen: nothing visible.
    run_frame(&device, &queue, &field, &frame(5.0, 0.0, 0.0, 1, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::ITEMS_VISIBLE], 0);
    assert_eq!(k[tables::counter::ITEMS_CULLED], items);

    // 6. A slot cap that binds: fitting lines are whole, the rest counted.
    let small = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 40, max_segments: 64, max_wash: 64 });
    run_frame(&device, &queue, &small, &frame(0.0, 0.0, 0.0, 1, 2));
    let k = small.read_counters(&queue);
    assert!(k[tables::counter::SLOT_FIT_END] <= 40);
    assert!(k[tables::counter::SLOTS_DROPPED] > 0);
    assert_eq!(k[tables::counter::SLOTS_DROPPED] + k[tables::counter::SLOT_FIT_END], total, "every slot is drawn or counted dropped");
    assert_eq!(k[tables::counter::LINES_GLYPH] + k[tables::counter::LINES_DROPPED], lines);
    let fit = small.read_slots(&queue, k[tables::counter::SLOT_FIT_END]);
    // The lines that fit are the FIRST in (item, line) order and the dropped
    // ones the last, the same lines every frame (C30): the drawn slots are
    // the arena's prefix, and a second frame draws the same bytes.
    // (Colour aside: the cull-state tint paints every drawn glyph.)
    let place = |s: &DerivedSlot| (s.x.to_bits(), s.row, s.glyph_and_wrap, s.item_and_group);
    let bad = fit.iter().zip(&c.expected).filter(|(g, w)| place(g) != place(w)).count();
    assert_eq!(bad, 0, "capped frame: {bad} of {} drawn slots are not the arena-order prefix", fit.len());
    run_frame(&device, &queue, &small, &frame(0.0, 0.0, 0.0, 1, 2));
    let k2 = small.read_counters(&queue);
    assert_eq!(k2, k, "a capped frame repeats its counters");
    assert_slots_equal(&small.read_slots(&queue, k2[tables::counter::SLOT_FIT_END]), &fit, "a capped frame repeats its slots");

    // 6b. A wash cap that binds: the boxes past it draw nothing, and the
    //     stats say how many (2026-10-10: they said "0 dropped" before).
    let few = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 4 });
    for _ in 0..4 {
        run_frame(&device, &queue, &few, &frame(0.0, 1e9, 0.0, 2, 0));
    }
    assert_eq!(few.read_counters(&queue)[tables::counter::WASH], lines, "every line reserves its box");
    assert_eq!(few.stats().wash_dropped, lines - 4, "the stats count the wash boxes past the cap");
    assert_eq!(field.stats().wash_dropped, 0);

    // 7. The stats ring: after a few frames the readback has landed.
    for _ in 0..4 {
        run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    }
    let st = field.stats();
    assert_eq!(st.items_total, items);
    assert_eq!(st.items_visible, items, "stats lag by two frames; four identical frames settle them");
    assert_eq!(st.slots, total);
    assert_eq!(st.lines_glyph, lines);
    if device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
        assert!(st.cull_ms > 0.0 && st.layout_ms > 0.0, "timestamps: cull {} ms layout {} ms", st.cull_ms, st.layout_ms);
    }

    // 8. A span edit: in place, then a remap, each visible in the next frame.
    field.set_item_spans(&queue, 0, &[ByteSpanGpu { start: 0, end: 11, color: 0xFF112233 }]);
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let got = field.read_slots(&queue, total);
    let first_line_slots = got.iter().filter(|s| glyph_field_derived::item_of(s.item_and_group) == 0 && glyph_field_derived::row_of(s.row) == 0).count();
    assert_eq!(first_line_slots, 11, "'hello world' draws eleven glyphs (the space is a slot)");
    assert!(got.iter().filter(|s| glyph_field_derived::item_of(s.item_and_group) == 0 && glyph_field_derived::row_of(s.row) == 0).all(|s| s.color == 0xFF112233), "the whole first line took the new span");
    let many: Vec<ByteSpanGpu> = (0..40u32).map(|k| ByteSpanGpu { start: k, end: k + 1, color: 0xFF000000 | k }).collect();
    field.set_item_spans(&queue, 0, &many);
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let got = field.read_slots(&queue, total);
    let h = got.iter().find(|s| glyph_field_derived::item_of(s.item_and_group) == 0 && glyph_field_derived::row_of(s.row) == 0 && s.glyph_id() == b'h' as u16).expect("the 'h' of hello");
    assert_eq!(h.color, 0xFF000000, "byte 0 took span 0 after the remap");
    let w = got.iter().find(|s| glyph_field_derived::item_of(s.item_and_group) == 0 && glyph_field_derived::row_of(s.row) == 0 && s.glyph_id() == b'w' as u16).expect("the 'w' of world");
    assert_eq!(w.color, 0xFF000006, "byte 6 took span 6 after the remap");
}

/// C28 (2026-10-10): the wash tier is a BOX per line that spans exactly what
/// the line's glyphs span — x from the item's origin over the line's widest
/// fold unit, the rows a WrapDown line stacks, the depth segments a WrapBack
/// line recedes through — held here to the glyph slots of the same line
/// from the twin: every slot inside the box, the box no wider than the
/// glyphs reach, as many depth segments as the deepest slot's fold, as many
/// rows as the lowest slot's. Before it the wash was a flat quad at segment
/// 0's depth as wide as the cull's 2x-fold-unit byte bound, and Ivan saw
/// wide.txt's back-wrapped lines run off the right instead of receding;
/// nothing held the wash tier to anything.
///
/// Rows past a page boundary are not judged for x: the wash stacks a
/// WrapDown line's rows straight down while the glyphs move to the next
/// page column — a known coarseness of the wash, outside C28.
#[test]
fn the_wash_box_spans_exactly_what_the_glyphs_span() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let params: Vec<ItemParamsGpu> = c.items.iter().map(|i| i.params).collect();
    let dummy = Dummy::new(&device);
    let field = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 1 << 12 });
    let cell_adv = tables::fu_to_world(ADVANCE_FU as i32, EM_HEIGHT_FU);
    // Every line a wash (the glyph threshold above every row), hard greeking.
    run_frame(&device, &queue, &field, &frame(0.0, 1e9, 0.0, 2, 0));
    let k = field.read_counters(&queue);
    let n = k[tables::counter::WASH];
    assert_eq!(n, c.lines.len() as u32, "one wash box per line");
    let washes = field.read_wash(&queue, n);
    let eps = 1e-3f32;
    let mut checked = 0usize;
    let mut deep = 0usize;
    let mut stacked = 0usize;
    for w in &washes {
        let item = w.item as usize;
        let row0 = glyph_field_derived::row_of(w.row_lane);
        let li = c
            .lines
            .iter()
            .position(|l| l.item == w.item && l.base_row == row0)
            .unwrap_or_else(|| panic!("a wash names a line: item {item} row {row0}"));
        let line = c.lines[li];
        let line_end = c.lines.get(li + 1).filter(|next| next.item == w.item).map_or(c.bytes[item].len() as u32, |next| next.byte_start - 1);
        let slots: Vec<(usize, &DerivedSlot)> = c
            .expected_keys
            .iter()
            .enumerate()
            .filter(|(_, &(it, b))| it == w.item && b >= line.byte_start && b < line_end)
            .map(|(k, _)| (k, &c.expected[k]))
            .collect();
        if slots.is_empty() {
            assert_eq!(w.width, 0.0, "line {li}: an empty line's wash has no width");
            continue;
        }
        let p = c.items[item].params;
        let page_of = |row: u32| -> i32 {
            let screen_row = row as i32 - p.scroll_rows;
            if p.has_page != 0 && p.page_rows > 0 && screen_row >= p.page_rows {
                screen_row / p.page_rows
            } else {
                0
            }
        };
        let first_page = page_of(row0);
        let x_lo = slots.iter().filter(|(_, s)| page_of(glyph_field_derived::row_of(s.row)) == first_page).map(|(_, s)| s.x).fold(f32::INFINITY, f32::min);
        let x_hi = slots
            .iter()
            .filter(|(_, s)| page_of(glyph_field_derived::row_of(s.row)) == first_page)
            .map(|(k, s)| s.x + c.expected_cells[*k] as f32 * cell_adv)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_seg = slots.iter().map(|(_, s)| u32::from(s.wrap_segment())).max().unwrap();
        let max_row = slots.iter().map(|(_, s)| glyph_field_derived::row_of(s.row)).max().unwrap();
        assert!(
            w.x0 <= x_lo + eps && x_hi <= w.x0 + w.width + eps,
            "line {li} (item {item} row {row0}): glyphs x [{x_lo}, {x_hi}] escape the wash [{}, {}]",
            w.x0,
            w.x0 + w.width
        );
        // Tight on the right, up to the advance of leaders with no glyph
        // (the malformed line ends in a truncated lead: the fold's cursor
        // moves one cell past the last drawn glyph, and so may the wash).
        let slack = c.line_blank_cells[li] as f32 * cell_adv;
        assert!(
            x_hi >= w.x0 + w.width - slack - eps,
            "line {li}: the wash [{}, {}] is wider than its glyphs reach ({x_hi}; {slack} of slot-less advance allowed)",
            w.x0,
            w.x0 + w.width
        );
        assert_eq!(w.nseg, max_seg + 1, "line {li}: the box spans the line's depth segments");
        assert_eq!(w.rows, max_row - row0 + 1, "line {li}: the box stacks the line's rows");
        checked += 1;
        deep += usize::from(w.nseg > 1);
        stacked += usize::from(w.rows > 1);
    }
    assert!(checked >= 10, "the corpus gave {checked} non-empty washed lines to check");
    assert!(deep >= 1 && stacked >= 1, "the corpus must wash a back-wrapped line ({deep}) and a down-wrapped one ({stacked})");
}

// ── M3: edits and selection keyed by (item, byte) ───────────────────────────

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The per-glyph overrides through the headless kernel, held lane for lane
/// to the twin with the same edits applied on the host: a colour beats the
/// span, the nudge is `x + nudge` in f32 AFTER the narrowing, a group puts
/// the field's own row index in the Derived lane. Bytes that are not
/// surviving leaders (a tab, a continuation byte, a glyph-0 lead) take an
/// override that changes nothing; a cleared or replaced override leaves the
/// last state; an override in a seeded segment is found by the segment's
/// binary search.
#[test]
fn overrides_apply_in_the_headless_layout_as_the_twin_predicts() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let ov = |item: u32, byte: u32, color: u32, x_nudge: f32, group: u32| GlyphOverride { item, byte, color, x_nudge, group };
    assert_eq!(c.bytes[0][15], b'\t');
    assert_eq!(c.bytes[0][48], 0x80, "a continuation byte");
    assert_eq!(c.bytes[0][44], 0xE2, "a lead followed by ASCII: a glyph-0 leader");
    assert_eq!(c.bytes[1][66], b'c', "the second segment of the long line");
    assert!(c.seeds.iter().any(|s| s.line == c.items[1].first_line && s.byte_offset == 64), "the long line is cut at 64");
    let overrides = vec![
        ov(0, 1, 0xFF0000AA, 0.0, NO_GROUP),                 // colour only
        ov(1, 66, 0, 0.375, NO_GROUP),                       // nudge only, in a seeded segment
        ov(4, 5, 0, 0.0, 7),                                 // group only
        ov(4, 6, 0, 0.0, 7),                                 // the same group: shares its row
        ov(4, 7, 0, 0.0, 9),                                 // a second group: a second row
        ov(0, 0, 0xFF00AA00, -0.125, 7),                     // all three on one glyph
        ov(0, 15, 0xFFFFFFFF, 1.0, 9),                       // a tab: glyph 0, no slot — nothing to apply to
        ov(0, 48, 0xFFFFFFFF, 1.0, 9),                       // a continuation byte: not a leader
        ov(0, 44, 0xFFFFFFFF, 1.0, 9),                       // a glyph-0 lead
        ov(2, 4, 0xFF123456, 0.0, NO_GROUP),                 // set…
        ov(2, 4, 0, 0.0, NO_GROUP),                          // …then cleared: no effect
        ov(4, 10, 0xFF111111, 0.0, NO_GROUP),                // set…
        ov(4, 10, 0xFF222222, 0.0, NO_GROUP),                // …then replaced: the last wins
    ];
    let got = layout_all_lines_with_overrides(&device, &queue, &inp, &overrides);
    assert_eq!(got.group_overrides, [0, 7, 9], "one row per distinct group, in order of first use");
    let k_of = |group: u32| got.group_overrides.iter().position(|&g| g == group).expect("group has a row") as u32;

    let mut want = c.expected.clone();
    let at = |item: u32, byte: u32| c.slot_of(item, byte).unwrap_or_else(|| panic!("({item}, {byte}) is a surviving leader"));
    want[at(0, 1)].color = 0xFF0000AA;
    want[at(1, 66)].x += 0.375;
    want[at(4, 5)].item_and_group = glyph_field_derived::override_lane(4, k_of(7));
    want[at(4, 6)].item_and_group = glyph_field_derived::override_lane(4, k_of(7));
    want[at(4, 7)].item_and_group = glyph_field_derived::override_lane(4, k_of(9));
    want[at(0, 0)].color = 0xFF00AA00;
    want[at(0, 0)].x += -0.125;
    want[at(0, 0)].item_and_group = glyph_field_derived::override_lane(0, k_of(7));
    want[at(4, 10)].color = 0xFF222222;
    for (item, byte) in [(0, 15), (0, 48), (0, 44)] {
        assert!(c.slot_of(item, byte).is_none(), "({item}, {byte}) must not be a surviving leader");
    }
    assert_slots_equal(&got.slots, &want, "headless layout with overrides");
    // The nudge is one f32 add after the narrowing, not folded into it: the
    // twin's x is the reference's single rounding plus the nudge.
    let base = c.expected[at(1, 66)].x;
    assert_ne!(got.slots[at(1, 66)].x.to_bits(), base.to_bits());
    assert_eq!(got.slots[at(1, 66)].x.to_bits(), (base + 0.375f32).to_bits());
}

/// A frame with a selection: the mask buffer holds exactly the slots whose
/// leader byte is in the range (as a set — atomic order), each bit-equal to
/// the main buffer's slot of the same glyph; a range across three segments
/// of a long line; a whole item; an empty item; an empty range; no
/// selection; a frame whose lines are washes (no segments: nothing); and
/// after a frame without `prepare_mask` the draw is back to zero.
#[test]
fn the_selection_mask_holds_exactly_the_slots_in_range() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let params: Vec<ItemParamsGpu> = c.items.iter().map(|i| i.params).collect();
    let dummy = Dummy::new(&device);
    let field = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 1 << 12 });
    assert_eq!(field.mask_capacity(), 1 << 16, "the mask cap follows the slot cap under the 1 M ceiling");
    let mask_pipeline = field.create_mask_pipeline(&device, MASK_FORMAT, 1);
    let total = c.expected.len() as u32;
    let all = frame(0.0, 0.0, 0.0, 1, 0);

    let expect_mask = |item: u32, start: u32, end: u32, what: &str| {
        run_frame_with_mask(&device, &queue, &field, &all, Some((&mask_pipeline, item, start, end)));
        let n = field.read_mask_count(&queue);
        let mut want: Vec<DerivedSlot> = c.expected_keys.iter().zip(&c.expected).filter(|(&(i, b), _)| i == item && b >= start && b < end).map(|(_, s)| *s).collect();
        let mut got = field.read_mask_slots(&queue, n);
        got.sort_by_key(sort_key);
        want.sort_by_key(sort_key);
        assert_slots_equal(&got, &want, what);
        // Each is a slot the main buffer drew this frame too.
        let main = field.read_slots(&queue, total);
        for s in &got {
            assert!(main.iter().any(|m| sort_key(m) == sort_key(s)), "{what}: a mask slot is in the main buffer: {}", describe(s));
        }
        n
    };

    // Three segments of the long line (cuts at 64 and 128).
    let n = expect_mask(1, 50, 140, "mask over item 1 [50, 140)");
    assert!(n >= 40, "the range holds many glyphs ({n})");
    assert!(c.expected_keys.iter().any(|&(i, b)| i == 1 && (50..64).contains(&b)) && c.expected_keys.iter().any(|&(i, b)| i == 1 && (128..140).contains(&b)), "the range reaches the first and third segments");
    // A glyph alone; a whole item (the end clamped); an item with no bytes.
    assert_eq!(expect_mask(0, 6, 7, "one glyph"), 1);
    assert_eq!(expect_mask(4, 0, u32::MAX, "all of item 4"), c.item_slots[4].len() as u32);
    assert_eq!(expect_mask(3, 0, 100, "the empty item"), 0);
    // An empty range, and no selection at all: zero instances.
    assert_eq!(expect_mask(1, 10, 10, "an empty range"), 0);
    run_frame_with_mask(&device, &queue, &field, &all, Some((&mask_pipeline, 1, 50, 140)));
    assert!(field.read_mask_count(&queue) > 0);
    run_frame(&device, &queue, &field, &all);
    assert_eq!(field.read_mask_count(&queue), 0, "a frame without prepare_mask resets the draw");
    // The wash tier has no segments: a selection there draws nothing.
    run_frame_with_mask(&device, &queue, &field, &frame(0.0, 1e9, 0.0, 2, 0), Some((&mask_pipeline, 1, 50, 140)));
    assert_eq!(field.read_mask_count(&queue), 0, "washed lines have no glyph slots to mask");
    // Overrides ride into the mask too (the same kernel): a recoloured
    // glyph's mask slot carries the colour (the mask pipeline ignores it,
    // but the record is the main buffer's).
    field.set_glyph_override(&queue, GlyphOverride { item: 0, byte: 6, color: 0xFF0000AA, x_nudge: 0.0, group: NO_GROUP });
    run_frame_with_mask(&device, &queue, &field, &all, Some((&mask_pipeline, 0, 6, 7)));
    let m = field.read_mask_slots(&queue, 1);
    assert_eq!(m[0].color, 0xFF0000AA);
    assert_eq!(m[0].glyph_id(), b'w' as u16);
}

/// `set_item_span_range` on the GPU path: colours checked per byte through
/// `locate` (which this ties to the span edit), on a range inside the
/// loaded spans, a clear, and a range across lines.
#[test]
fn set_item_span_range_merges_into_the_item_spans_on_the_gpu() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let params: Vec<ItemParamsGpu> = c.items.iter().map(|i| i.params).collect();
    let dummy = Dummy::new(&device);
    let field = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 1 << 12 });
    let total = c.expected.len() as u32;
    let all = frame(0.0, 0.0, 0.0, 1, 0);
    let color_at = |byte: u32| -> u32 {
        let slots = field.read_slots(&queue, total);
        let k = field.locate(&queue, 0, byte).unwrap_or_else(|| panic!("byte {byte} of item 0 is a drawn glyph"));
        slots[k as usize].color
    };
    const BLUE: u32 = 0xFF2020FF;
    const GREEN: u32 = 0xFF20FF20;
    const RED: u32 = 0xFFFF2020;

    // As loaded: [0,5) blue, [6,11) green, [24,40) red; byte 5 default.
    run_frame(&device, &queue, &field, &all);
    assert_eq!((color_at(0), color_at(5), color_at(6), color_at(24)), (BLUE, DEFAULT_COLOR, GREEN, RED));

    // Inside: [2, 8) → blue | new | green, with byte 5 (the gap) coloured.
    field.set_item_span_range(&queue, 0, 2, 8, 0xFF112233);
    run_frame(&device, &queue, &field, &all);
    let line0: Vec<u32> = (0..11).map(color_at).collect();
    assert_eq!(line0, [BLUE, BLUE, 0xFF112233, 0xFF112233, 0xFF112233, 0xFF112233, 0xFF112233, 0xFF112233, GREEN, GREEN, GREEN]);

    // A clear: the whole first line back to the default; the later span stays.
    field.set_item_span_range(&queue, 0, 0, 11, 0);
    run_frame(&device, &queue, &field, &all);
    assert!((0..11).all(|b| color_at(b) == DEFAULT_COLOR), "cleared");
    assert_eq!(color_at(24), RED);

    // Across lines: [8, 30) covers the end of line 0, line 1, and clips the
    // red span at 30.
    field.set_item_span_range(&queue, 0, 8, 30, 0xFF445566);
    run_frame(&device, &queue, &field, &all);
    assert_eq!((color_at(7), color_at(8), color_at(10)), (DEFAULT_COLOR, 0xFF445566, 0xFF445566));
    assert_eq!((color_at(12), color_at(21)), (0xFF445566, 0xFF445566), "line 1 ('foo\\tbar \\x7f!')");
    assert_eq!((color_at(24), color_at(29), color_at(30), color_at(38)), (0xFF445566, 0xFF445566, RED, RED));
    // The host copy the merge ran on is what the device has: the kernel's
    // colouring above is the proof; the span list itself is two runs.
    let mut host = c.spans[c.items[0].span_base as usize..(c.items[0].span_base + c.items[0].span_count) as usize].to_vec();
    glyph_field_visible::merge_span_range(&mut host, 2, 8, 0xFF112233);
    glyph_field_visible::merge_span_range(&mut host, 0, 11, 0);
    glyph_field_visible::merge_span_range(&mut host, 8, 30, 0xFF445566);
    assert_eq!(host, [ByteSpanGpu { start: 8, end: 30, color: 0xFF445566 }, ByteSpanGpu { start: 30, end: 40, color: RED }]);
}

/// `locate` against a readback search: for random (item, byte) probes —
/// surviving leaders, continuation bytes, glyph-0 cells (tabs, DEL, the
/// newline, sequence trailers, a lead followed by ASCII), bytes past the
/// item, an item past the field — the slot it names holds the twin's record
/// for that glyph, or it says `None` exactly when the twin has no slot.
#[test]
fn locate_names_the_transient_slot_of_item_byte_or_none() {
    let (device, queue) = device();
    let (trie, c) = corpus();
    let refs: Vec<&[u8]> = c.bytes.iter().map(|b| b.as_slice()).collect();
    let inp = inputs(&trie, &c, &refs);
    let params: Vec<ItemParamsGpu> = c.items.iter().map(|i| i.params).collect();
    let dummy = Dummy::new(&device);
    let field = VisibleField::new(&device, &queue, &inp, &dummy.resources(&params), TARGETS, VisibleLimits { max_slots: 1 << 16, max_segments: 1 << 12, max_wash: 1 << 12 });
    let total = c.expected.len() as u32;
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let slots = field.read_slots(&queue, total);

    let mut probes: Vec<(u32, u32)> = vec![(0, 15), (0, 20), (0, 11), (0, 48), (0, 44), (0, 70), (0, 71), (0, 500), (3, 0), (9, 0), (1, 64), (1, 66), (1, 127), (1, 128)];
    // The long line's sequence bytes: every byte of the first piece.
    probes.extend((0..61u32).map(|b| (1, b)));
    // The same bytes in leader mode (item 2 is the long line, cluster off).
    probes.extend((0..61u32).map(|b| (2, b)));
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    while probes.len() < 400 {
        let r = xorshift(&mut state);
        let item = (r % 5) as u32;
        let len = c.bytes[item as usize].len() as u32;
        probes.push((item, ((r >> 8) % (len + 4) as u64) as u32));
    }
    let (mut some, mut none_past, mut none_continuation, mut none_glyph0) = (0, 0, 0, 0);
    for &(item, byte) in &probes {
        let want = c.slot_of(item, byte).map(|k| c.expected[k]);
        let got = field.locate(&queue, item, byte);
        match (want, got) {
            (Some(w), Some(k)) => {
                assert!(k < total, "({item}, {byte}): slot {k} of {total}");
                assert_slots_equal(&[slots[k as usize]], &[w], &format!("locate({item}, {byte}) = slot {k}"));
                some += 1;
            }
            (None, None) => {
                let bytes = c.bytes.get(item as usize);
                match bytes.and_then(|b| b.get(byte as usize)) {
                    None => none_past += 1,
                    Some(b) if (0x80..0xC0).contains(b) => none_continuation += 1,
                    Some(_) => none_glyph0 += 1,
                }
            }
            (w, g) => panic!("locate({item}, {byte}): twin {w:?}, field {g:?}"),
        }
    }
    // 400 probes: about half land on surviving leaders (193 on 2026-10-10),
    // the rest spread over the None classes, each of which must be reached.
    assert!(some >= 150, "{some} located of {}", probes.len());
    assert!(none_past >= 5 && none_continuation >= 20 && none_glyph0 >= 20, "None classes: past {none_past}, continuation {none_continuation}, glyph-0 {none_glyph0}");
    // GLYPH_G_DUMP's door: the trait's word readback over a located slot.
    let k = field.locate(&queue, 0, 6).expect("'w' of world");
    let mut words = [0u32; 5];
    field.read_slot_words(&device, &queue, k, &mut words);
    assert_eq!(words[2] & 0xFFFF, b'w' as u32);
    assert_eq!(words[4], glyph_field_derived::item_lane(0));
    // A frame with the item hidden: its glyphs have no slot.
    field.set_item_hidden(&queue, 0, true);
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    assert_eq!(field.locate(&queue, 0, 6), None, "a hidden item's glyph is nowhere");
    assert!(field.locate(&queue, 4, 0).is_some());
}
