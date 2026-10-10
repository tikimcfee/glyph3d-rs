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

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use glyph_field::{FieldResources, FieldTargets, FramePrepare, GlyphField, GroupRow, ItemParamsGpu};
use glyph_field_derived::DerivedSlot;
use glyph_field_visible::test_support::{synthetic_trie, ADVANCE_FU, EM_HEIGHT_FU};
use glyph_field_visible::{
    layout_all_lines, reference_x, tables, ByteSpanGpu, LineEntryGpu, SegmentSeedGpu, TrieUpload, VisibleField, VisibleInputs,
    VisibleItem, VisibleLimits, WRAP_BACK, WRAP_DOWN,
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
    let mut c = Corpus { items: vec![], bytes: vec![], lines: vec![], seeds: vec![], spans: vec![], expected: vec![], item_slots: vec![] };
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
                    glyphs += 1;
                }
                col += 1;
                cells += k;
                if fold_unit > 0 && col % fold_unit == 0 {
                    seg_adv = 0.0;
                } else {
                    seg_adv += k as f32 * cell_adv;
                }
                // Per byte, as the reference walks (a malformed lead's
                // following bytes are classified on their own).
                i += 1;
            }
            c.lines.push(LineEntryGpu { byte_start: line_start as u32, item: idx as u32, base_row: base_row as u32, glyph_count: glyphs });
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

/// One frame: prepare, then a render pass with both draws into an offscreen
/// target (so the pipelines and bind groups are validated), submitted.
fn run_frame(device: &wgpu::Device, queue: &wgpu::Queue, field: &VisibleField, f: &FramePrepare) {
    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("color"),
        size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGETS.color_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGETS.depth_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let cv = color.create_view(&Default::default());
    let dv = depth.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    field.prepare(queue, &mut encoder, f);
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
    //    the twin's, as a set (segment order is atomic).
    run_frame(&device, &queue, &field, &frame(0.0, 0.0, 0.0, 1, 0));
    let k = field.read_counters(&queue);
    assert_eq!(k[tables::counter::ITEMS_VISIBLE], items, "items visible");
    assert_eq!(k[tables::counter::LINES_CANDIDATE], lines, "candidate lines");
    assert_eq!(k[tables::counter::LINES_GLYPH], lines, "glyph-tier lines");
    assert_eq!(k[tables::counter::LINES_WASH], 0);
    assert_eq!(k[tables::counter::SEG_FIT_END], segments, "segments");
    assert_eq!(k[tables::counter::SLOT_FIT_END], total, "slots");
    assert_eq!(k[tables::counter::SLOTS_DROPPED], 0);
    let mut got = field.read_slots(&queue, total);
    let mut want = c.expected.clone();
    got.sort_by_key(sort_key);
    want.sort_by_key(sort_key);
    assert_slots_equal(&got, &want, "frame slots (sorted)");

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
    for s in &fit {
        assert!(c.expected.iter().any(|w| sort_key(w) == sort_key(s) || (w.item_and_group == s.item_and_group && w.row == s.row && w.x.to_bits() == s.x.to_bits())), "a drawn slot is a real one: {}", describe(s));
    }

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
