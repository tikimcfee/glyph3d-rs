//! hyper_oracle.rs — HyperLayout held to an oracle-backed reference.
//!
//! `HyperLayout` (`layout_hyper*`) is the production layout engine: every
//! `--load-repo` runs it, whatever `--repo-engine` says. Before this
//! instrument nothing compared it to the JS oracle. The
//! `--fixture-*` instruments hold `fold.rs`, `scan.rs`, `bake.rs` and
//! `text.rs` to the oracle's recorded answers; `--repo-verify` holds
//! HyperLayout to HyperLayout. A defect HyperLayout carries alone was
//! therefore visible only through pixels — and the goldens were re-adopted
//! after HyperLayout landed, so they pin whatever it does.
//!
//! THE REFERENCE is `fold::run_pipeline`: the serial fold, its decode and its
//! sequence pass (`resolve_clusters`), the form `--fixture-fold` holds
//! BIT-EXACT to the oracle over all 26 `.pipe.bin` fixtures (cluster-keycap,
//! the ASCII-led sequence class, among them). Its records are compacted into
//! instances and placements by `layout::compact_records_into`, the seam's one
//! host statement of compaction, and the two outputs are diffed by
//! `layout::diff_backends` plus a per-record tally that names the byte.
//!
//! WHAT THE TWO SIDES SHARE, stated because a cross-form comparison is blind
//! inside anything both forms call (root AGENTS.md, "Earning a green"):
//!
//! - the atlas trie, `atlas::TrieTable` — `lookup` and `fu_to_world` reach
//!   both sides (the fold through `impl ResolveGlyph for TrieTable`). The
//!   fixtures' own tries cannot be used: they carry world-unit values and
//!   HyperLayout resolves in font units. So the trie's correctness is NOT
//!   under test here, and the oracle never saw the atlas trie;
//! - `fold::{rows_for_line, wrap_segment_of, wrap_row_of}`, which HyperLayout
//!   borrows from the fold. Those are the REFERENCE's own functions, held to
//!   the oracle by `--fixture-fold`; a fault in them reddens reference-port
//!   and stays invisible here (`phantom-row` is the measured example).
//!
//! What the sides do NOT share, and therefore what this sees: HyperLayout's
//! decode, its ASCII fast path and sequence resolution
//! (`layout_hyper/char_resolve.rs` — `fast_byte_table`, `resolve_leader`,
//! its own `is_static_zero_cp`, `TrieTable::{starts_a_sequence,
//! sequence_lookup}`; the fold scans the raw table with its own
//! longest-prefix walk), its Pass 1 prepass, its host Pass 2 emission
//! (`pass2_host.rs`, the instances a device-less load produces), its
//! pagination, the recording path's re-derivation (`rederive.rs`, the records
//! tier), and, since 2026-10-09, its DEVICE Pass 2 (`pass2_device.rs`, what
//! every GPU load runs: unified memory on the M2, the staging path on a
//! discrete card) in BOTH slot formats. The device tier runs the production
//! emitter into host memory (`layout_hyper::device_pass2_*_on_host`): every
//! production destination hands that same `EmitInputs::run` a raw address of
//! writable memory, so the only thing swapped is where the bytes land. The
//! 32 B `RenderSlot` is compared byte-for-byte with the reference instance it
//! must equal; the 20 B `DerivedSlot` on every lane it carries (x, row, glyph,
//! wrap segment, colour, group), the wrap segment computed from the
//! reference's column by the fold's own `wrap_segment_of`.
//!
//! WHAT IT CANNOT SEE: the device path past the emitter (the staging copy into
//! VRAM, the buffer chunking), the Derived field's vertex-stage Y/Z (the slot
//! carries none), the background prefetch of Pass 1 (repo-verify holds it to
//! the inline Pass 1 used here), and anything both sides share, above.
//!
//! INPUTS. A `.pipe.bin` fixture contributes its bytes and item params (each
//! item laid out as its own buffer, as a repo file is); a directory is walked
//! as `--load-repo` walks it and given `repo::file_item_params` under the
//! renderer's default shape (wrap back, `--cluster-mode`); any other file is
//! one such item. Paint is flat on both sides — colour is not layout.
//!
//! It refuses to pass over zero records, and under `GLYPH_HYPER_ORACLE_STRICT=1`
//! (the gate's form) also over a corpus in which the reference resolved no
//! sequence, or no ASCII-led one — the class it was built to watch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::atlas::TrieTable;
use crate::fold::{self, ClusterMode, F_LEADER};
use crate::glyph_scene::{GlyphInstance, RenderSlot};
use glyph_field_derived::DerivedSlot;
use crate::layout::{
    compact_records_into, diff_backends, BackendOutput, GlyphArena, GlyphRecord, ItemParams,
    ItemPlacement, LayoutGlyphs, LayoutItem, Paint, DEFAULT_COLOR_PACKED,
};
use crate::layout_hyper::{
    device_pass2_derived_on_host, device_pass2_render_on_host, rederive_item_records, HyperLayout,
};
use crate::text::ResolveGlyph;

/// One item: its own byte buffer, its params, and a name to report it by.
pub struct CorpusItem {
    pub label: String,
    pub bytes: Vec<u8>,
    pub params: ItemParams,
}

/// One input path's worth of items.
pub struct Corpus {
    pub name: String,
    pub items: Vec<CorpusItem>,
}

/// A fixture item's params in the seam's carriers. The five page-geometry
/// integers were truncated by the fixture loader already; this only narrows
/// their carrier.
fn params_of_fixture_item(it: &fold::Item) -> ItemParams {
    ItemParams {
        origin_x: it.origin_x,
        origin_y: it.origin_y,
        origin_z: it.origin_z,
        line_height: it.line_height,
        z_step: it.z_step,
        wrap_width: it.wrap_width as i32,
        wrap_mode: it.wrap_mode,
        cluster_mode: it.cluster_mode,
        has_page: it.has_page,
        page_rows: it.page_rows as i32,
        page_cols: it.page_cols as i32,
        scroll_rows: it.scroll_rows as i32,
        pages_wide: it.pages_wide as i32,
        page_gap_x: it.page_gap_x,
        band_stride_y: it.band_stride_y,
        depth_per_band: it.depth_per_band,
        depth_per_col: it.depth_per_col,
        // NaN on the wire is UNSET, and no layout reads this lane
        // (`fold::tests::paginate_ignores_page_line_height`); the seam's
        // validate refuses a NaN anyway, so it crosses as the repo's 0.0.
        page_line_height: if it.page_line_height.is_nan() { 0.0 } else { it.page_line_height },
    }
}

/// The reference's item: the same params over a buffer of its own.
fn fold_item_of(p: &ItemParams, len: usize) -> fold::Item {
    fold::Item {
        byte_start: 0,
        byte_count: len as i64,
        origin_x: p.origin_x,
        origin_y: p.origin_y,
        origin_z: p.origin_z,
        wrap_width: p.wrap_width as i64,
        wrap_mode: p.wrap_mode,
        cluster_mode: p.cluster_mode,
        z_step: p.z_step,
        line_height: p.line_height,
        has_page: p.has_page,
        page_rows: p.page_rows as i64,
        page_cols: p.page_cols as i64,
        scroll_rows: p.scroll_rows as i64,
        pages_wide: p.pages_wide as i64,
        page_gap_x: p.page_gap_x,
        band_stride_y: p.band_stride_y,
        depth_per_band: p.depth_per_band,
        depth_per_col: p.depth_per_col,
        page_line_height: p.page_line_height,
    }
}

fn repo_params(cluster_mode: ClusterMode) -> crate::repo::RepoParams {
    // The renderer's default repo shape.
    crate::repo::RepoParams {
        wrap_mode: fold::WrapMode::Back,
        cluster_mode,
        ..Default::default()
    }
}

/// Read one input path as a corpus. A directory with no files and a fixture
/// with no items are refused here, not compared.
pub fn load_corpus(path: &Path, cluster_mode: ClusterMode) -> Result<Corpus, String> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if path.is_dir() {
        let walk = crate::repo::walk_repo(path);
        if walk.files.is_empty() {
            return Err(format!("{}: no files walked — refusing an empty corpus", path.display()));
        }
        let rp = repo_params(cluster_mode);
        let items = walk
            .files
            .into_iter()
            .map(|f| {
                let params = crate::repo::file_item_params(&rp, f.bytes.len(), f.newline_count);
                CorpusItem { label: format!("{name}/{}", f.rel_path), bytes: f.bytes, params }
            })
            .collect();
        return Ok(Corpus { name: format!("{name}/ [{cluster_mode:?}]"), items });
    }
    if name.ends_with(".pipe.bin") {
        let fx = crate::fixture::load_pipe_fixture(path)?;
        if fx.items.is_empty() {
            return Err(format!("{}: fixture has no items", path.display()));
        }
        let items = fx
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let s = it.byte_start as usize;
                let e = s + it.byte_count as usize;
                CorpusItem {
                    label: format!("{name} item {i} [{:?}]", it.cluster_mode),
                    bytes: fx.bytes[s..e].to_vec(),
                    params: params_of_fixture_item(it),
                }
            })
            .collect();
        return Ok(Corpus { name, items });
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let params = crate::repo::file_item_params(&repo_params(cluster_mode), bytes.len(), newlines);
    Ok(Corpus {
        name: format!("{name} [{cluster_mode:?}]"),
        items: vec![CorpusItem { label: name, bytes, params }],
    })
}

/// The reference's output for one item: one record per UTF-8 leader, the byte
/// each came from, and how many sequence heads it resolved.
pub struct Reference {
    pub records: Vec<GlyphRecord>,
    pub record_bytes: Vec<usize>,
    /// Leaders whose glyph is a SEQUENCE slot, not the trie's own entry.
    pub heads: usize,
    /// Of those, heads led by an ASCII byte (keycaps) — the C10 class.
    pub ascii_heads: usize,
}

/// Run the oracle-backed fold over one item.
pub fn reference_item(bytes: &[u8], params: &ItemParams, trie: &TrieTable) -> Reference {
    let it = fold_item_of(params, bytes.len());
    let r = fold::run_pipeline(bytes, trie, &[it]);
    let s = &r.slots;
    let mut out = Reference { records: Vec::new(), record_bytes: Vec::new(), heads: 0, ascii_heads: 0 };
    for id in 0..bytes.len() {
        if s.flags(id) & F_LEADER == 0 {
            continue;
        }
        let gi = s.gi[id];
        let cp = fold::decode_codepoint_at(bytes, id, fold::sequence_length(bytes, id));
        if gi != 0 && gi != trie.resolve(cp).glyph_id {
            out.heads += 1;
            if bytes[id] < 0x80 {
                out.ascii_heads += 1;
            }
        }
        out.records.push(GlyphRecord {
            measures: [s.x(id), s.y(id), s.z(id), s.advance(id), s.height(id)],
            counts: [gi, s.row(id) as u32, s.col(id) as u32],
        });
        out.record_bytes.push(id);
    }
    out
}

fn record_bit_eq(a: &GlyphRecord, b: &GlyphRecord) -> bool {
    a.counts == b.counts && a.measures.iter().zip(b.measures.iter()).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn show_record(r: &GlyphRecord) -> String {
    format!(
        "gi {} adv {:e} x {:e} y {:e} z {:e} row {} col {}",
        r.glyph_id(),
        r.advance(),
        r.x(),
        r.y(),
        r.z(),
        r.row(),
        r.col()
    )
}

fn show_instance(i: &GlyphInstance) -> String {
    format!(
        "gi {} adv {:e} x {:e} y {:e} z {:e} row {} col {} rgba {:08x}",
        i.glyph_id, i.advance, i.pos[0], i.pos[1], i.pos[2], i.row, i.col, i.color
    )
}

fn codepoint_at(bytes: &[u8], at: usize) -> String {
    let cp = fold::decode_codepoint_at(bytes, at, fold::sequence_length(bytes, at));
    format!("U+{cp:04X}")
}

/// One item's disagreement, tallied per tier. `first` names the first
/// differing record (or instance) by byte.
#[derive(Default)]
pub struct ItemDiff {
    pub records: usize,
    pub record_bad: usize,
    pub instances: usize,
    pub instance_bad: usize,
    pub placement_bad: bool,
    pub first: Option<String>,
}

impl ItemDiff {
    pub fn clean(&self) -> bool {
        self.record_bad == 0 && self.instance_bad == 0 && !self.placement_bad
    }
}

/// Tally one item's three tiers: records (HyperLayout's recording path vs the
/// fold), instances (HyperLayout's production emission vs the fold's records
/// compacted), placement (bit_eq).
pub fn diff_item(
    bytes: &[u8],
    reference: &Reference,
    hyper_records: &[GlyphRecord],
    ref_instances: &[GlyphInstance],
    hyper_instances: &[GlyphInstance],
    ref_place: &ItemPlacement,
    hyper_place: &ItemPlacement,
) -> ItemDiff {
    let mut d = ItemDiff { records: reference.records.len(), instances: ref_instances.len(), ..Default::default() };
    let n = reference.records.len().min(hyper_records.len());
    for (i, (r, h)) in reference.records.iter().zip(hyper_records.iter()).enumerate() {
        if !record_bit_eq(r, h) {
            d.record_bad += 1;
            if d.first.is_none() {
                let at = reference.record_bytes[i];
                d.first = Some(format!(
                    "record {i} at byte {at} ({}): oracle-backed fold {} | HyperLayout {}",
                    codepoint_at(bytes, at),
                    show_record(r),
                    show_record(h),
                ));
            }
        }
    }
    let extra = reference.records.len().abs_diff(hyper_records.len());
    d.record_bad += extra;
    if extra > 0 && d.first.is_none() {
        d.first = Some(format!(
            "record count: oracle-backed fold {} vs HyperLayout {} (first {n} agree)",
            reference.records.len(),
            hyper_records.len()
        ));
    }

    // Survivor -> byte, for naming an instance: the reference's survivors
    // are its records with a glyph, in record order.
    let survivor_bytes: Vec<usize> = reference
        .records
        .iter()
        .zip(reference.record_bytes.iter())
        .filter(|(r, _)| r.glyph_id() != 0)
        .map(|(_, &b)| b)
        .collect();
    let m = ref_instances.len().min(hyper_instances.len());
    for i in 0..m {
        let a: &[u8] = bytemuck::bytes_of(&ref_instances[i]);
        let b: &[u8] = bytemuck::bytes_of(&hyper_instances[i]);
        if a != b {
            d.instance_bad += 1;
            if d.first.is_none() {
                let at = survivor_bytes.get(i).copied().unwrap_or(0);
                d.first = Some(format!(
                    "instance {i} (reference byte {at}, {}): oracle-backed fold {} | HyperLayout {}",
                    codepoint_at(bytes, at),
                    show_instance(&ref_instances[i]),
                    show_instance(&hyper_instances[i]),
                ));
            }
        }
    }
    d.instance_bad += ref_instances.len().abs_diff(hyper_instances.len());
    d.placement_bad = !ref_place.bit_eq(hyper_place);
    if d.placement_bad && d.first.is_none() {
        d.first = Some(format!("placement: oracle-backed fold {ref_place:?} | HyperLayout {hyper_place:?}"));
    }
    d
}

/// One item's DEVICE tier: the production emitter's slots in both formats
/// against the reference instances, and its placements.
#[derive(Default)]
pub struct DeviceItemDiff {
    pub render_bad: usize,
    pub derived_bad: usize,
    pub placement_bad: usize,
    pub first: Option<String>,
}

/// The Instanced slot a reference instance must be, lane for lane.
fn render_slot_of(i: &GlyphInstance) -> RenderSlot {
    RenderSlot {
        pos: i.pos,
        glyph_id: i.glyph_id,
        color: i.color,
        group_id: i.group_id,
        advance: i.advance,
        height: i.height,
    }
}

/// The Derived slot a reference instance must be: its x, row, glyph, colour
/// and group, and the wrap segment its column falls in (the fold's own
/// `wrap_segment_of`; a survivor is never a newline).
fn derived_slot_of(i: &GlyphInstance, p: &ItemParams) -> DerivedSlot {
    let wrap = fold::wrap_segment_of(i.col as i64, p.wrap_width as i64, false);
    // The row lane carries the column page (glyph_field_derived::derive).
    let x_page = if p.has_page && p.page_cols > 0 { i.col / p.page_cols as u32 } else { 0 };
    DerivedSlot::with_item_and_group(
        i.pos[0],
        glyph_field_derived::pack_row(i.row, x_page),
        (i.glyph_id & 0xFFFF) as u16,
        wrap.clamp(0, u16::MAX as i64) as u16,
        i.color,
        i.group_id,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn diff_device_item(
    bytes: &[u8],
    params: &ItemParams,
    reference: &Reference,
    ref_instances: &[GlyphInstance],
    ref_place: &ItemPlacement,
    render: &[RenderSlot],
    render_place: &ItemPlacement,
    derived: &[DerivedSlot],
    derived_place: &ItemPlacement,
) -> DeviceItemDiff {
    let mut d = DeviceItemDiff::default();
    let survivor_bytes: Vec<usize> = reference
        .records
        .iter()
        .zip(reference.record_bytes.iter())
        .filter(|(r, _)| r.glyph_id() != 0)
        .map(|(_, &b)| b)
        .collect();
    let name = |i: usize| {
        let at = survivor_bytes.get(i).copied().unwrap_or(0);
        format!("reference byte {at}, {}", codepoint_at(bytes, at))
    };
    for (i, r) in ref_instances.iter().enumerate() {
        let want = render_slot_of(r);
        if render.get(i).is_none_or(|got| bytemuck::bytes_of(got) != bytemuck::bytes_of(&want)) {
            d.render_bad += 1;
            if d.first.is_none() {
                let got = render.get(i).map_or("absent".to_string(), |s| {
                    format!(
                        "gi {} adv {:e} x {:e} y {:e} z {:e} rgba {:08x}",
                        s.glyph_id, s.advance, s.pos[0], s.pos[1], s.pos[2], s.color
                    )
                });
                d.first = Some(format!(
                    "device RenderSlot {i} ({}): oracle-backed fold {} | HyperLayout device {got}",
                    name(i),
                    show_instance(r)
                ));
            }
        }
        let want = derived_slot_of(r, params);
        if derived.get(i).is_none_or(|got| bytemuck::bytes_of(got) != bytemuck::bytes_of(&want)) {
            d.derived_bad += 1;
            if d.first.is_none() {
                d.first = Some(format!(
                    "device DerivedSlot {i} ({}): oracle-backed fold {want:?} | HyperLayout device {:?}",
                    name(i),
                    derived.get(i)
                ));
            }
        }
    }
    d.render_bad += render.len().saturating_sub(ref_instances.len());
    d.derived_bad += derived.len().saturating_sub(ref_instances.len());
    for (tier, place) in [("render", render_place), ("derived", derived_place)] {
        if !ref_place.bit_eq(place) {
            d.placement_bad += 1;
            if d.first.is_none() {
                d.first = Some(format!(
                    "device placement ({tier}): oracle-backed fold {ref_place:?} | HyperLayout device {place:?}"
                ));
            }
        }
    }
    d
}

/// The totals one corpus contributes.
pub struct CorpusDiff {
    pub items: usize,
    pub records: usize,
    pub instances: usize,
    pub record_bad: usize,
    pub instance_bad: usize,
    pub placement_bad: usize,
    pub heads: usize,
    pub ascii_heads: usize,
    /// Record disagreements inside leader-mode items (the C13 class: until
    /// 2026-10-09 HyperLayout read no cluster mode), separated from the
    /// cluster-mode one.
    pub leader_mode_record_bad: usize,
    /// The device tier: slots the production device emitter wrote (per
    /// format), and how many of them, and of its placements, differ.
    pub device_slots: usize,
    pub device_render_bad: usize,
    pub device_derived_bad: usize,
    pub device_placement_bad: usize,
    /// The PAINT tier: the device Pass 2 again under `Paint::SyntaxHeuristic`,
    /// every lane (colour included) against the reference instances painted
    /// with `text::colorize_leaders` over the WHOLE item — the colours the
    /// host Pass 2 paints. Slots compared, slots differing (both formats),
    /// and how many reference colours were not the default (anti-vacuity: a
    /// tier that only ever saw default paint compared nothing).
    pub paint_slots: usize,
    pub paint_bad: usize,
    pub paint_colored: usize,
    /// (item label, first divergence) for every differing item, in order.
    pub firsts: Vec<(String, String)>,
    /// diff_backends' verdict over the whole corpus (the seam's own differ).
    pub seam: Result<(), String>,
}

impl Default for CorpusDiff {
    fn default() -> Self {
        Self {
            items: 0,
            records: 0,
            instances: 0,
            record_bad: 0,
            instance_bad: 0,
            placement_bad: 0,
            heads: 0,
            ascii_heads: 0,
            leader_mode_record_bad: 0,
            device_slots: 0,
            device_render_bad: 0,
            device_derived_bad: 0,
            device_placement_bad: 0,
            paint_slots: 0,
            paint_bad: 0,
            paint_colored: 0,
            firsts: Vec::new(),
            seam: Ok(()),
        }
    }
}

/// Lay one corpus out both ways and diff it.
pub fn diff_corpus(corpus: &Corpus, trie: &Arc<TrieTable>) -> Result<CorpusDiff, String> {
    let items: Vec<LayoutItem<'_>> = corpus
        .items
        .iter()
        .enumerate()
        .map(|(i, c)| LayoutItem {
            bytes: &c.bytes,
            params: c.params,
            group_id: i as u32,
            paint: Paint::Flat(DEFAULT_COLOR_PACKED),
        })
        .collect();

    // HyperLayout, production host path (no device): Pass 1 + Pass 2.
    let mut hyper = HyperLayout::with_trie(Arc::clone(trie));
    let mut hyper_arena = GlyphArena::new();
    let hyper_places = hyper.layout_items(&items, &mut hyper_arena).map_err(|e| e.to_string())?;

    // HyperLayout, production DEVICE Pass 2 (what a GPU load runs), both slot
    // formats, written into host memory (see the module header).
    let (render_slots, render_places) = device_pass2_render_on_host(&items, trie);
    let (derived_slots, derived_places) = device_pass2_derived_on_host(&items, trie);

    // The reference: the fold per item, compacted by the seam.
    let mut ref_arena = GlyphArena::new();
    let mut ref_places = Vec::with_capacity(items.len());
    let mut refs = Vec::with_capacity(items.len());
    let mut hyper_recs = Vec::with_capacity(items.len());
    for (i, c) in corpus.items.iter().enumerate() {
        let r = reference_item(&c.bytes, &c.params, trie);
        ref_places.push(compact_records_into(&r.records, Paint::Flat(DEFAULT_COLOR_PACKED), i as u32, &mut ref_arena));
        refs.push(r);
        // The recording path's records, item by item — what
        // `VerifyLayout::layout_items_recording` concatenates.
        hyper_recs.push(rederive_item_records(&c.bytes, &c.params, trie));
    }

    let mut out = CorpusDiff { items: items.len(), ..Default::default() };
    let (ri, hi) = (ref_arena.instances(), hyper_arena.instances());
    for (i, c) in corpus.items.iter().enumerate() {
        let (rp, hp) = (&ref_places[i], &hyper_places[i]);
        let slice = |all: &[GlyphInstance], p: &ItemPlacement| -> Vec<GlyphInstance> {
            let s = (p.slot_base as usize).min(all.len());
            let e = (s + p.slot_count as usize).min(all.len());
            all[s..e].to_vec()
        };
        let ref_inst = slice(ri, rp);
        let d = diff_item(&c.bytes, &refs[i], &hyper_recs[i], &ref_inst, &slice(hi, hp), rp, hp);
        out.records += d.records;
        out.instances += d.instances;
        out.record_bad += d.record_bad;
        out.instance_bad += d.instance_bad;
        out.placement_bad += d.placement_bad as usize;
        out.heads += refs[i].heads;
        out.ascii_heads += refs[i].ascii_heads;
        if c.params.cluster_mode == ClusterMode::Leader {
            out.leader_mode_record_bad += d.record_bad;
        }
        let (rdp, ddp) = (&render_places[i], &derived_places[i]);
        let span = |p: &ItemPlacement, len: usize| {
            let s = (p.slot_base as usize).min(len);
            s..(s + p.slot_count as usize).min(len)
        };
        let dd = diff_device_item(
            &c.bytes,
            &c.params,
            &refs[i],
            &ref_inst,
            rp,
            &render_slots[span(rdp, render_slots.len())],
            rdp,
            &derived_slots[span(ddp, derived_slots.len())],
            ddp,
        );
        out.device_render_bad += dd.render_bad;
        out.device_derived_bad += dd.derived_bad;
        out.device_placement_bad += dd.placement_bad;
        if let Some(f) = d.first.or(dd.first) {
            out.firsts.push((c.label.clone(), f));
        }
    }
    out.device_slots = render_slots.len();

    // The PAINT tier (C17, 2026-10-09). Layout is settled above, so the same
    // emitter under syntax paint must reproduce the reference slots with the
    // whole-item colours: the device Pass 2 colourises line by line, and a
    // line cut into chunks (`layout_hyper/chunk.rs`) is the place it can
    // disagree with a walk of the whole item. Not oracle-backed — the JS
    // oracle has no paint — but the host Pass 2's own colouring.
    let syntax_items: Vec<LayoutItem<'_>> =
        items.iter().map(|it| LayoutItem { paint: Paint::SyntaxHeuristic, ..*it }).collect();
    let (paint_render, paint_render_places) = device_pass2_render_on_host(&syntax_items, trie);
    let (paint_derived, paint_derived_places) = device_pass2_derived_on_host(&syntax_items, trie);
    let mut paint_arena = GlyphArena::new();
    for (i, c) in corpus.items.iter().enumerate() {
        let colors = crate::text::colorize_leaders(&c.bytes);
        let place = compact_records_into(&refs[i].records, Paint::PerRecord(&colors), i as u32, &mut paint_arena);
        let s = (place.slot_base as usize).min(paint_arena.instances().len());
        let e = (s + place.slot_count as usize).min(paint_arena.instances().len());
        let ref_inst = paint_arena.instances()[s..e].to_vec();
        out.paint_colored +=
            ref_inst.iter().filter(|g| g.color != crate::text::palette::C_DEFAULT).count();
        let (rdp, ddp) = (&paint_render_places[i], &paint_derived_places[i]);
        let span = |p: &ItemPlacement, len: usize| {
            let s = (p.slot_base as usize).min(len);
            s..(s + p.slot_count as usize).min(len)
        };
        let dd = diff_device_item(
            &c.bytes,
            &c.params,
            &refs[i],
            &ref_inst,
            &place,
            &paint_render[span(rdp, paint_render.len())],
            rdp,
            &paint_derived[span(ddp, paint_derived.len())],
            ddp,
        );
        out.paint_bad += dd.render_bad + dd.derived_bad;
        if let Some(f) = dd.first {
            out.firsts.push((format!("{} (syntax paint)", c.label), f));
        }
    }
    out.paint_slots = paint_derived.len();

    let ref_all: Vec<GlyphRecord> = refs.iter().flat_map(|r| r.records.iter().copied()).collect();
    let hyper_all: Vec<GlyphRecord> = hyper_recs.into_iter().flatten().collect();
    out.seam = diff_backends(
        &BackendOutput { name: "oracle-backed fold", placements: &ref_places, instances: ri, records: &ref_all },
        &BackendOutput { name: hyper.name(), placements: &hyper_places, instances: hi, records: &hyper_all },
    )
    .map(|_| ());
    Ok(out)
}

/// `--hyper-oracle-check`: every path, both ways, then a verdict. Exits.
pub fn run_hyper_oracle_check(paths: &[PathBuf], cluster_mode: ClusterMode) -> ! {
    let strict = std::env::var("GLYPH_HYPER_ORACLE_STRICT").is_ok_and(|v| v == "1");
    let trie = crate::default_trie();
    let mut total = CorpusDiff::default();
    let mut failed = 0usize;
    let mut first: Option<(String, String)> = None;
    for p in paths {
        let corpus = match load_corpus(p, cluster_mode) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("hyper-oracle FAIL: {e}");
                std::process::exit(1);
            }
        };
        let d = match diff_corpus(&corpus, &trie) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("hyper-oracle FAIL: {}: HyperLayout refused the corpus: {e}", corpus.name);
                std::process::exit(1);
            }
        };
        let device_bad = d.device_render_bad + d.device_derived_bad + d.device_placement_bad;
        let clean = d.record_bad == 0
            && d.instance_bad == 0
            && d.placement_bad == 0
            && device_bad == 0
            && d.paint_bad == 0
            && d.seam.is_ok();
        if clean {
            println!(
                "  PASS {:<34} {} item(s), {} records, {} instances, {} device slots x2 bit-exact ({} sequence heads, {} ASCII-led)",
                corpus.name, d.items, d.records, d.instances, d.device_slots, d.heads, d.ascii_heads
            );
        } else {
            failed += 1;
            println!(
                "FAIL  {:<34} {}/{} records, {}/{} instances, {}/{} placements differ; device {}+{} slots (render+derived), {} placements; paint {}/{} slots ({} sequence heads, {} ASCII-led)",
                corpus.name,
                d.record_bad,
                d.records,
                d.instance_bad,
                d.instances,
                d.placement_bad,
                d.items,
                d.device_render_bad,
                d.device_derived_bad,
                d.device_placement_bad,
                d.paint_bad,
                d.paint_slots * 2,
                d.heads,
                d.ascii_heads
            );
            for (label, f) in d.firsts.iter().take(4) {
                println!("       {label}: {f}");
            }
            if d.firsts.len() > 4 {
                println!("       … and {} more differing item(s)", d.firsts.len() - 4);
            }
            if let Err(e) = &d.seam {
                println!("       diff_backends: {}", e.replace('\n', " "));
            }
            if first.is_none() {
                first = d.firsts.first().cloned();
            }
        }
        total.items += d.items;
        total.records += d.records;
        total.instances += d.instances;
        total.record_bad += d.record_bad;
        total.instance_bad += d.instance_bad;
        total.placement_bad += d.placement_bad;
        total.heads += d.heads;
        total.ascii_heads += d.ascii_heads;
        total.leader_mode_record_bad += d.leader_mode_record_bad;
        total.device_slots += d.device_slots;
        total.device_render_bad += d.device_render_bad;
        total.device_derived_bad += d.device_derived_bad;
        total.device_placement_bad += d.device_placement_bad;
        total.paint_slots += d.paint_slots;
        total.paint_bad += d.paint_bad;
        total.paint_colored += d.paint_colored;
    }

    // Anti-vacuity before the verdict: a differ that compared nothing passes
    // loudest.
    if total.records == 0 {
        eprintln!("hyper-oracle FAIL: compared nothing — {} items, 0 records", total.items);
        std::process::exit(1);
    }
    if total.device_slots == 0 {
        eprintln!("hyper-oracle FAIL: the device tier compared nothing — 0 slots emitted");
        std::process::exit(1);
    }
    if total.paint_colored == 0 {
        eprintln!("hyper-oracle FAIL: the paint tier compared nothing — no reference slot carries a syntax colour");
        std::process::exit(1);
    }
    if strict && (total.heads == 0 || total.ascii_heads == 0) {
        eprintln!(
            "hyper-oracle FAIL (strict): the corpus does not exercise the sequence pass — {} heads, {} ASCII-led",
            total.heads, total.ascii_heads
        );
        std::process::exit(1);
    }
    if failed > 0 {
        if let Some((label, f)) = first {
            println!("first divergence: {label}: {f}");
        }
        eprintln!(
            "hyper-oracle FAIL: {failed}/{} corpora differ — {}/{} records ({} in leader-mode items), {}/{} instances, {}/{} placements; device: {}+{}/{} slots (render+derived), {} placements; paint: {}/{} slots",
            paths.len(),
            total.record_bad,
            total.records,
            total.leader_mode_record_bad,
            total.instance_bad,
            total.instances,
            total.placement_bad,
            total.items,
            total.device_render_bad,
            total.device_derived_bad,
            total.device_slots,
            total.device_placement_bad,
            total.paint_bad,
            total.paint_slots * 2,
        );
        std::process::exit(1);
    }
    println!(
        "hyper-oracle PASS: {} corpora, {} items, {} records, {} instances, {} placements, and the device Pass 2's {} slots in both formats bit-exact vs the oracle-backed fold ({} sequence heads, {} ASCII-led); under syntax paint, {} slots x2 match the whole-item colours ({} not default)",
        paths.len(),
        total.items,
        total.records,
        total.instances,
        total.items,
        total.device_slots,
        total.heads,
        total.ascii_heads,
        total.paint_slots,
        total.paint_colored
    );
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_of(text: &[u8], params: ItemParams) -> Corpus {
        Corpus {
            name: "t".into(),
            items: vec![CorpusItem { label: "t".into(), bytes: text.to_vec(), params }],
        }
    }

    #[test]
    fn plain_ascii_agrees_on_all_three_tiers() {
        // THE COUNTERFACTUAL FOR THE TALLY: a corpus where both sides must
        // agree. Wrapping and paging on, so the fold's positions are not the
        // trivial ones. If this goes red, the instrument (or HyperLayout's
        // ASCII path) is wrong, not the sequence class.
        let rp = repo_params(ClusterMode::Cluster);
        let text = b"fn main() {\n    let x = 1; // a comment that is long enough to wrap twice over\n}\n";
        let params = crate::repo::file_item_params(&rp, text.len(), 3);
        let d = diff_corpus(&corpus_of(text, params), &crate::default_trie()).expect("layout");
        assert!(d.records > 0 && d.instances > 0, "compared nothing");
        assert_eq!(d.device_slots, d.instances, "the device tier emitted a different survivor count");
        assert_eq!((d.record_bad, d.instance_bad, d.placement_bad), (0, 0, 0), "{:?}", d.firsts);
        assert_eq!((d.device_render_bad, d.device_derived_bad, d.device_placement_bad), (0, 0, 0), "{:?}", d.firsts);
        assert!(d.seam.is_ok(), "{:?}", d.seam);
    }

    /// C10 and C13 (2026-10-09): ASCII-led sequences (keycaps) cluster in
    /// cluster mode and only there, and a leader-mode item lays every leader
    /// out as itself, ZWJ and VS16 included. Every tier, host and device,
    /// both modes. The near-misses are the fast path's edges: a keycap base
    /// followed by a non-ASCII byte that starts no sequence, by a lone
    /// continuation byte, by ASCII, and at the very end of the buffer.
    #[test]
    fn keycaps_and_leader_mode_agree_on_every_tier() {
        let text = "1\u{FE0F}\u{20E3} #\u{20E3} *\u{FE0F}\u{20E3}x\n\
                    9\u{E9} 7\u{80} 5 4\u{FE0E}\u{20E3} a\u{200D}b \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\n\
                    0\u{FE0F}\u{FE0F}\u{20E3}2";
        let trie = crate::default_trie();
        for (mode, want_heads) in [(ClusterMode::Cluster, true), (ClusterMode::Leader, false)] {
            let rp = repo_params(mode);
            let params = crate::repo::file_item_params(&rp, text.len(), 2);
            let d = diff_corpus(&corpus_of(text.as_bytes(), params), &trie).expect("layout");
            assert!(d.records > 0 && d.device_slots == d.instances, "{mode:?}: compared nothing");
            assert_eq!((d.record_bad, d.instance_bad, d.placement_bad), (0, 0, 0), "{mode:?}: {:?}", d.firsts);
            assert_eq!(
                (d.device_render_bad, d.device_derived_bad, d.device_placement_bad),
                (0, 0, 0),
                "{mode:?}: {:?}",
                d.firsts
            );
            assert_eq!(d.ascii_heads >= 4, want_heads, "{mode:?}: {} ASCII-led heads", d.ascii_heads);
        }
    }

    /// C14 (2026-10-09): pagination on every tier, host and device, over a
    /// grid of page shapes that reaches each branch of `fold::paginate` —
    /// scroll alone, rows alone, rows + scroll with a conveyor that runs a
    /// row above the page, columns alone (the fold unit is then page_cols),
    /// columns under a narrower and a wider wrap, pages_wide 1 and 3 — in
    /// both wrap modes. The text holds what the emitters special-case:
    /// pure-ASCII lines (the line and burst paths) at, under and over the
    /// fold unit and exact multiples of it, empty lines, non-ASCII lines (the
    /// per-byte path), a line ending exactly on a column page, and an
    /// UNTERMINATED last line that is the widest — the stride is the fold's
    /// widest pre-advance x, which an unterminated line's last glyph sets.
    #[test]
    fn pagination_agrees_on_every_tier() {
        let trie = crate::default_trie();
        let texts: [&str; 3] = [
            "abcdefgh\n\nabcdefghijklmnop\nxy\n\u{E9}t\u{E9} caf\u{E9} \u{4E16}\u{754C}\nabcdefghijkl\n\n12345678901234567890123",
            "line 0\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9 is the widest unterminated",
            "\u{1F680}a\u{1F30D}bcdefgh\n\nabcdefghi\nabcdefgh\n",
        ];
        // (page_rows, page_cols, scroll_rows, pages_wide, wrap_width)
        let shapes: [(i32, i32, i32, i32, i32); 10] = [
            (0, 0, 3, 1, 0),
            (3, 0, 0, 2, 0),
            (3, 0, 2, 3, 0),
            (4, 0, 9, 3, 0),
            (0, 4, 0, 1, 0),
            (0, 4, 0, 1, 6),
            (0, 4, 0, 1, 3),
            (2, 4, 1, 3, 0),
            (2, 5, 1, 2, 7),
            (3, 0, 1, 3, 5),
        ];
        let mut compared = 0usize;
        for mode in [fold::WrapMode::Down, fold::WrapMode::Back] {
            for (rows, cols, scroll, wide, wrap) in shapes {
                let params = ItemParams {
                    origin_x: 0.5,
                    origin_y: -1.0,
                    origin_z: 2.0,
                    line_height: 1.1,
                    z_step: 0.4,
                    wrap_width: wrap,
                    wrap_mode: mode,
                    has_page: true,
                    page_rows: rows,
                    page_cols: cols,
                    scroll_rows: scroll,
                    pages_wide: wide,
                    page_gap_x: 0.8,
                    band_stride_y: 9.5,
                    depth_per_band: -2.5,
                    depth_per_col: 0.75,
                    ..ItemParams::default()
                };
                let corpus = Corpus {
                    name: "paged".into(),
                    items: texts
                        .iter()
                        .enumerate()
                        .map(|(i, t)| CorpusItem { label: format!("text {i}"), bytes: t.as_bytes().to_vec(), params })
                        .collect(),
                };
                let d = diff_corpus(&corpus, &trie).expect("layout");
                let at = format!("{mode:?} rows {rows} cols {cols} scroll {scroll} wide {wide} wrap {wrap}");
                assert!(d.records > 0 && d.device_slots == d.instances, "{at}: compared nothing");
                assert_eq!((d.record_bad, d.instance_bad, d.placement_bad), (0, 0, 0), "{at}: {:?}", d.firsts);
                assert_eq!(
                    (d.device_render_bad, d.device_derived_bad, d.device_placement_bad),
                    (0, 0, 0),
                    "{at}: {:?}",
                    d.firsts
                );
                assert!(d.seam.is_ok(), "{at}: {:?}", d.seam);
                compared += d.instances;
            }
        }
        assert!(compared > 2000, "the grid compared only {compared} instances");
    }

    /// The Derived field's vertex stage, the one lane the oracle cannot see
    /// (a `DerivedSlot` carries no Y or Z), held to the Instanced emitter:
    /// `glyph_field_derived::derive_yz` — the shader's `derive_yz`
    /// transcribed — over every device-emitted Derived slot of the
    /// pagination grid must land on the `RenderSlot`'s own y and z, within
    /// the ulps the shader's f32 fma chain is allowed against the emitter's
    /// f64 arithmetic. Before 2026-10-10 every column-paged shape failed
    /// here: the slot had no column page and z dropped
    /// `x_page × depth_per_col` (0.75 per page in this grid, thousands of
    /// ulps). The shader and its transcription are kept in step by hand;
    /// `derive::tests::wgsl_derive_yz_carries_the_column_page_term` pins
    /// the term in the shader's text.
    #[test]
    fn derived_vertex_stage_agrees_with_instanced() {
        use crate::layout_hyper::{device_pass2_derived_on_host, device_pass2_render_on_host};
        let trie = crate::default_trie();
        let texts: [&str; 3] = [
            "abcdefgh\n\nabcdefghijklmnop\nxy\n\u{E9}t\u{E9} caf\u{E9} \u{4E16}\u{754C}\nabcdefghijkl\n\n12345678901234567890123",
            "line 0\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9 is the widest unterminated",
            "\u{1F680}a\u{1F30D}bcdefgh\n\nabcdefghi\nabcdefgh\n",
        ];
        let shapes: [(i32, i32, i32, i32, i32); 11] = [
            (0, 0, 0, 1, 0),
            (0, 0, 3, 1, 0),
            (3, 0, 0, 2, 0),
            (3, 0, 2, 3, 0),
            (4, 0, 9, 3, 0),
            (0, 4, 0, 1, 0),
            (0, 4, 0, 1, 6),
            (0, 4, 0, 1, 3),
            (2, 4, 1, 3, 0),
            (2, 5, 1, 2, 7),
            (3, 0, 1, 3, 5),
        ];
        let mut compared = 0usize;
        let mut column_paged = 0usize;
        let mut max_err = 0.0f32;
        for mode in [fold::WrapMode::Down, fold::WrapMode::Back] {
            for (rows, cols, scroll, wide, wrap) in shapes {
                let params = ItemParams {
                    origin_x: 0.5,
                    origin_y: -1.0,
                    origin_z: 2.0,
                    line_height: 1.1,
                    z_step: 0.4,
                    wrap_width: wrap,
                    wrap_mode: mode,
                    has_page: rows != 0 || cols != 0 || scroll != 0,
                    page_rows: rows,
                    page_cols: cols,
                    scroll_rows: scroll,
                    pages_wide: wide,
                    page_gap_x: 0.8,
                    band_stride_y: 9.5,
                    depth_per_band: -2.5,
                    depth_per_col: 0.75,
                    ..ItemParams::default()
                };
                let items: Vec<LayoutItem<'_>> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| LayoutItem {
                        bytes: t.as_bytes(),
                        params,
                        group_id: i as u32,
                        paint: Paint::Flat(DEFAULT_COLOR_PACKED),
                    })
                    .collect();
                let (render, _) = device_pass2_render_on_host(&items, &trie);
                let (derived, _) = device_pass2_derived_on_host(&items, &trie);
                assert_eq!(render.len(), derived.len());
                let gpu = glyph_field::ItemParamsGpu::from(&params);
                let at = format!("{mode:?} rows {rows} cols {cols} scroll {scroll} wide {wide} wrap {wrap}");
                for (k, (r, d)) in render.iter().zip(derived.iter()).enumerate() {
                    let [y, z] = glyph_field_derived::derive_yz(d.row, d.wrap_segment() as u32, &gpu);
                    for (name, got, want) in [("y", y, r.pos[1]), ("z", z, r.pos[2])] {
                        // The shader's f32 fma chain against the emitter's f64:
                        // sub-micro-unit noise either way (a cell is ~0.53 wide);
                        // the dropped page term was 0.75 per column page.
                        let err = (got - want).abs();
                        assert!(
                            got.is_finite() && err <= 1e-4,
                            "{at}: slot {k} {name}: shader {got} vs emitter {want} (err {err}; row lane {:#x})",
                            d.row
                        );
                        max_err = max_err.max(err);
                    }
                    if glyph_field_derived::x_page_of(d.row) > 0 {
                        column_paged += 1;
                    }
                    compared += 1;
                }
            }
        }
        assert!(compared > 2000, "the grid compared only {compared} slots");
        assert!(column_paged > 200, "only {column_paged} slots sat on a column page past the first");
        eprintln!("derived vertex stage: {compared} slots, {column_paged} on a later column page, max error {max_err:e}");
    }

    #[test]
    fn the_device_tally_sees_a_planted_slot_difference() {
        let trie = crate::default_trie();
        let text = b"ab\ncd\n";
        let params = ItemParams { line_height: 1.0, ..Default::default() };
        let r = reference_item(text, &params, &trie);
        let mut arena = GlyphArena::new();
        let place = compact_records_into(&r.records, Paint::Flat(DEFAULT_COLOR_PACKED), 0, &mut arena);
        let inst = arena.instances().to_vec();
        let render: Vec<RenderSlot> = inst.iter().map(render_slot_of).collect();
        let mut derived: Vec<DerivedSlot> = inst.iter().map(|i| derived_slot_of(i, &params)).collect();
        let clean = diff_device_item(text, &params, &r, &inst, &place, &render, &place, &derived, &place);
        assert_eq!((clean.render_bad, clean.derived_bad, clean.placement_bad), (0, 0, 0));
        derived[2].row += 1; // 'c', byte 3
        let d = diff_device_item(text, &params, &r, &inst, &place, &render, &place, &derived, &place);
        assert_eq!((d.render_bad, d.derived_bad), (0, 1));
        let first = d.first.expect("a first divergence");
        assert!(first.contains("byte 3") && first.contains("DerivedSlot"), "{first}");
    }

    #[test]
    fn the_tally_sees_a_planted_record_difference_and_names_its_byte() {
        let trie = crate::default_trie();
        let text = b"ab\ncd\n";
        let params = ItemParams { line_height: 1.0, ..Default::default() };
        let r = reference_item(text, &params, &trie);
        let mut hyper = r.records.clone();
        hyper[3].counts[0] += 1; // 'c', byte 3
        let place = ItemPlacement {
            slot_base: 0,
            slot_count: 0,
            record_count: 0,
            page: crate::layout::PageExtent { right: 0.0, bottom: 0.0, z_min: 0.0, z_max: 0.0 },
            ink: crate::layout::InkExtent { min: [0.0; 3], max: [0.0; 3] },
        };
        let d = diff_item(text, &r, &hyper, &[], &[], &place, &place);
        assert_eq!(d.record_bad, 1);
        let first = d.first.expect("a first divergence");
        assert!(first.contains("byte 3") && first.contains("U+0063"), "{first}");
    }

    #[test]
    fn an_empty_directory_is_refused_not_compared() {
        let dir = std::env::temp_dir().join(format!("hyper-oracle-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let got = load_corpus(&dir, ClusterMode::Cluster);
        let _ = std::fs::remove_dir(&dir);
        assert!(got.is_err(), "an empty corpus must be refused");
    }
}
