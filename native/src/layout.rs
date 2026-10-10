//! layout.rs — THE LAYOUT SEAM: one contract, every backend behind it.
//!
//! Everything that lays glyphs out passes through the types here, and the
//! shape of these types is the point: each backend is an *implementation* of
//! this contract, so getting it wrong means writing every backend twice.
//!
//! WHY THE SEAM IS SHAPED THIS WAY. The obvious contract between a layout
//! backend and the renderer is `-> Vec<GlyphRecord>`: a CPU-owned array of
//! every glyph's position, carried home in full. A device backend under that
//! contract measures readback bandwidth, not its kernels — 36 B of output per
//! source byte (~864 MB for a 24 MB repo) crosses back to the host every load,
//! and a GPU layout benchmarked that way read ~100 MB/s flat from 4 MB to
//! 24 MB for exactly that reason.
//!
//! Because everything the CPU actually did with those bytes was:
//!
//!   1. drop the blanks             — a stream compaction
//!   2. repack 32 B → 48 B          — a copy that inserts padding and paint
//!   3. min/max the positions       — a reduction
//!
//! ...and then hand the result straight to a GPU instance buffer. A filter, a
//! memcpy and a reduction, all three of which the device can do without ever
//! telling the host a single position. What the host genuinely needs back is
//! WHERE THINGS ARE — slot ranges, counts, extents — not what they are:
//! picking sends a coordinate and gets one id, a highlight is a range and a
//! write, a group move is one 80 B row. Metadata, measured in bytes.
//!
//! So the seam is stated as **fill this arena, return counts and ranges**:
//!
//! ```text
//!   caller  ──[ LayoutItem: bytes + params + paint ]──▶  backend
//!   caller  ◀─[ ItemPlacement: slot range + extents ]──  backend
//!   caller  ────────[ &mut GlyphArena (destination) ]──▶  backend
//! ```
//!
//! The arena is passed IN and owned by the caller, so a backend never decides
//! where the glyphs live. On the host it is a `Vec<GlyphInstance>`; the point
//! of naming it a destination rather than a return value is that the
//! device-resident path replaces its interior with a device buffer without
//! moving a single call site.
//!
//! WHO IMPLEMENTS IT ([`LayoutEngine`]):
//!
//! | backend | module | `--repo-engine` | target |
//! |---|---|---|---|
//! | `HyperLayout` (parallel CPU, Rayon) | `layout_hyper.rs` | `hyper` (default), `direct`, `batch`, `naive` | host arena or mapped device slots |
//!
//! Beside them, outside the seam, sit the corpus instruments: the serial
//! `fold`, the `scan` form and the `bake`, each held bit-exact (or at a stated
//! eps tier) to the JS oracle's recorded answers in `engine/fixtures` by the
//! `--fixture-*` instruments. The JS oracle itself (`viz-web/glyph3d-js`) is
//! frozen and not executed here.
//!
//! WHAT IS DELIBERATELY NOT HERE:
//!
//! - **A position in the return value.** `LayoutGlyphs` cannot produce records
//!   at all; a gate that needs them asks [`VerifyLayout`], a separate trait.
//!   That is what lets a device-resident backend keep glyphs on the device.
//!   IT IS NOT THE SAME AS DELETING THE READBACK: `VerifyLayout` gates the
//!   API, not the copy, and a recording run still pays it. What deletes the
//!   copy is the direct path — `HyperLayout::layout_items` writes instances
//!   into the caller's arena and no 32 B wire record exists at all. The
//!   recording path is the verification form. Which strategies exist, and
//!   what each costs, comes from `--repo-scan-only`, not from this comment.
//! - **`text::reference_layout`.** It is a second, independently-derived
//!   realization of the same layout and its whole value is that it shares no
//!   lineage with the fold. It stays where it is.
//! - **The `--repo-engine` strategy names.** `direct`/`batch`/`naive` are CLI
//!   choices (`repo::Strategy`) that select how a load records and reports,
//!   not separate contracts; all three construct the same `HyperLayout`.

use crate::glyph_scene::GlyphInstance;

/// One render-read record, exactly the layout's wire format
/// (`schema/glyph-identity.json`): 32 B, f32 lanes crossing as raw bits.
///
///   f32 X Y Z ADVANCE HEIGHT | u32 GLYPH_ID ROW COL
///
/// This is the SHARED record shape, not one backend's detail — every backend
/// and every corpus instrument produces the same 32 B from its own fold, which
/// is what makes them diffable by bits. It lives on the seam for that reason.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphRecord {
    /// X, Y, Z, ADVANCE, HEIGHT (render-read measures).
    pub measures: [f32; 5],
    /// GLYPH_ID, ROW, COL.
    pub counts: [u32; 3],
}

impl GlyphRecord {
    pub fn x(self) -> f32 {
        self.measures[0]
    }
    pub fn y(self) -> f32 {
        self.measures[1]
    }
    pub fn z(self) -> f32 {
        self.measures[2]
    }
    pub fn advance(self) -> f32 {
        self.measures[3]
    }
    pub fn height(self) -> f32 {
        self.measures[4]
    }
    pub fn glyph_id(self) -> u32 {
        self.counts[0]
    }
    pub fn row(self) -> u32 {
        self.counts[1]
    }
    pub fn col(self) -> u32 {
        self.counts[2]
    }
}

const _: () = assert!(std::mem::size_of::<GlyphRecord>() == 32);

/// Packed RGBA8 of the default text color (`212,212,212`, alpha 255) — the one
/// definition, shared by the flat paint and the per-record fallback. It used to
/// exist twice (a literal in `repo.rs`, a `pack_rgba8` call in `text.rs`) with
/// nothing asserting they agreed; they did, and now they cannot disagree.
pub const DEFAULT_COLOR_PACKED: u32 = 0xFF_D4D4D4;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A backend's refusal. `status` is the backend's own code where it has one
/// (an integer, so a refusal stays comparable across backends); `backend`
/// names who refused, because with several of them an unattributed error is
/// unactionable.
#[derive(Debug)]
pub struct LayoutError {
    pub backend: &'static str,
    pub status: i32,
    pub what: String,
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} failed with status {}", self.backend, self.what, self.status)
    }
}
impl std::error::Error for LayoutError {}

/// Refusal raised on the HOST, before any backend is entered — deliberately
/// outside every backend's status range so it can never be confused with one.
pub const LAYOUT_BAD_PARAMS: i32 = -1;

// ---------------------------------------------------------------------------
// What goes in
// ---------------------------------------------------------------------------

/// Layout params for one item (one text file). Mirrors [`crate::fold::Item`].
/// f64 fields keep the oracle's float discipline; page geometry is integer.
///
/// These are the CONTRACT's params, not any one backend's: every backend takes
/// them, and a backend's own wire form of them (the `.pipe.bin` item record,
/// the device item table `glyph_field::ItemParamsGpu`) is a serialization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemParams {
    pub origin_x: f64,
    pub origin_y: f64,
    pub origin_z: f64,
    pub line_height: f64,
    pub z_step: f64,
    /// Fold unit in COLUMNS; 0 = no wrap.
    pub wrap_width: i32,
    /// How a wrap spends itself: down a row or back in depth (the default,
    /// here and in `RepoParams`).
    /// ITEM-LEVEL, exactly like `wrap_width` — see [`crate::fold::WrapMode`].
    pub wrap_mode: crate::fold::WrapMode,
    /// Whether the sequence pass resolves codepoint clusters to single glyphs.
    /// ITEM-LEVEL, exactly like `wrap_mode` — see [`crate::fold::ClusterMode`].
    pub cluster_mode: crate::fold::ClusterMode,
    pub has_page: bool,
    pub page_rows: i32,
    pub page_cols: i32,
    pub scroll_rows: i32,
    pub pages_wide: i32,
    pub page_gap_x: f64,
    pub band_stride_y: f64,
    pub depth_per_band: f64,
    pub depth_per_col: f64,
    pub page_line_height: f64,
}

impl Default for ItemParams {
    /// Plain text field: origin at 0, unit line height, no wrap, no pages.
    fn default() -> Self {
        Self {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            line_height: 1.0,
            z_step: 0.0,
            wrap_width: 0,
            wrap_mode: crate::fold::WrapMode::Back,
            cluster_mode: crate::fold::ClusterMode::default(),
            has_page: false,
            page_rows: 0,
            page_cols: 0,
            scroll_rows: 0,
            pages_wide: 0,
            page_gap_x: 0.0,
            band_stride_y: 0.0,
            depth_per_band: 0.0,
            depth_per_col: 0.0,
            page_line_height: 0.0,
        }
    }
}

impl From<&ItemParams> for glyph_field::ItemParamsGpu {
    fn from(p: &ItemParams) -> Self {
        let z_step_lo = (p.z_step - p.z_step as f32 as f64) as f32;
        let line_height_lo = (p.line_height - p.line_height as f32 as f64) as f32;
        Self {
            line_height: p.line_height as f32,
            origin_y: p.origin_y as f32,
            origin_z: p.origin_z as f32,
            z_step: p.z_step as f32,
            z_step_lo,
            band_stride_y: p.band_stride_y as f32,
            depth_per_band: p.depth_per_band as f32,
            depth_per_col: p.depth_per_col as f32,
            page_rows: p.page_rows,
            pages_wide: p.pages_wide,
            page_cols: p.page_cols,
            scroll_rows: p.scroll_rows,
            has_page: if p.has_page { 1 } else { 0 },
            line_height_lo,
            group: 0,
            _pad2: 0,
        }
    }
}

impl ItemParams {
    /// Refuse a layout a backend would silently turn into NaN.
    ///
    /// WHY THIS EXISTS. The fold performs NO input validation: `line_height`
    /// is a raw `f64`, and an unset line_height is NaN and propagates through
    /// every lane. Every check in this tree then compares layouts BY BITS —
    /// and two NaNs compare bit-equal. So a NaN pitch produces a NaN layout
    /// that every bit-exact comparison and the byte-equal render A/B would
    /// pass. The JS oracle carries an `assertLineHeight` for exactly this;
    /// this is the native side's.
    ///
    /// NaN specifically is not "some invalid float": it is the `.pipe.bin`
    /// wire encoding for UNSET. Reaching this function means an unset pitch
    /// travelled all the way to the layout call without anyone resolving it,
    /// which is a different bug from a corrupt one and says so.
    ///
    /// ZERO IS LEGAL, and that is the subtle half. A zero pitch collapses
    /// every row onto one baseline — a degenerate layout, but a CHOICE, and
    /// not the same thing as an omission. The idiomatic Rust reflex
    /// (`if lh == 0.0 { default }`, or `Option::unwrap_or`) quietly conflates
    /// the two; this does not.
    ///
    /// It lives on the SEAM rather than in one backend because it guards the
    /// contract, not an implementation: the CPU and GPU backends inherit it by
    /// construction instead of each having to remember.
    pub fn validate(&self, item: usize) -> Result<(), LayoutError> {
        let bad = |what: &str, why: &str| {
            Err(LayoutError {
                backend: "seam",
                status: LAYOUT_BAD_PARAMS,
                what: format!("item {item}: {what} — {why}"),
            })
        };
        if self.line_height.is_nan() {
            return bad(
                "line_height is NaN",
                "NaN is the wire encoding for UNSET, so an unresolved pitch reached \
                 the layout call. Every gate here compares by bits and two NaNs are \
                 bit-equal, so nothing downstream would notice",
            );
        }
        for (name, v) in [
            ("line_height", self.line_height),
            ("origin_x", self.origin_x),
            ("origin_y", self.origin_y),
            ("origin_z", self.origin_z),
            ("z_step", self.z_step),
            ("page_gap_x", self.page_gap_x),
            ("band_stride_y", self.band_stride_y),
            ("depth_per_band", self.depth_per_band),
            ("depth_per_col", self.depth_per_col),
            ("page_line_height", self.page_line_height),
        ] {
            if !v.is_finite() {
                return bad(
                    &format!("{name} is {v}"),
                    "a non-finite measure propagates into every position it touches",
                );
            }
        }
        for (name, v) in [
            ("wrap_width", self.wrap_width),
            ("page_rows", self.page_rows),
            ("page_cols", self.page_cols),
            ("scroll_rows", self.scroll_rows),
            ("pages_wide", self.pages_wide),
        ] {
            if v < 0 {
                return bad(&format!("{name} is {v}"), "page geometry counts are non-negative");
            }
        }
        Ok(())
    }
}

/// How a backend paints the glyphs it emits.
///
/// THE INDEX IS BY RECORD, NOT BY INSTANCE, and that ordering constraint is
/// the reason paint crosses the seam at all rather than being applied
/// afterwards: paint is chosen from the SOURCE BYTES, and compaction destroys
/// the index that names a byte. Once the blanks are gone, `instances[i]` no
/// longer says which record — or which byte — it came from. So a backend
/// paints during compaction or not at all, and that holds whether the
/// compaction runs in a `for` loop or in a stream-compaction kernel.
/// A byte range `[start..end)` painted with a packed RGBA8 color.
/// Designed for AST (e.g. Tree-sitter) and LSP semantic tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSpan {
    pub start: u32,
    pub end: u32,
    pub color: u32,
}

#[derive(Clone, Copy)]
pub enum Paint<'a> {
    /// Every glyph the same packed RGBA8.
    Flat(u32),
    /// One packed RGBA8 per RECORD, in record order — legacy format.
    /// Must be exactly as long as the item's record stream.
    PerRecord(&'a [u32]),
    /// Non-overlapping byte ranges in ascending order — AST/LSP format.
    ByteSpans(&'a [ByteSpan]),
    /// Compute heuristic syntax colors on-the-fly per item during layout.
    SyntaxHeuristic,
}

/// One item to lay out: the bytes, how to lay them out, how to paint them, and
/// which group row the resulting instances answer to.
pub struct LayoutItem<'a> {
    pub bytes: &'a [u8],
    pub params: ItemParams,
    pub group_id: u32,
    pub paint: Paint<'a>,
}

// ---------------------------------------------------------------------------
// What comes back — ranges and extents, never positions
// ---------------------------------------------------------------------------

/// The page footprint of one item, measured over EVERY record — blanks
/// included, because a blank still occupies its advance and a line of trailing
/// spaces still widens the page even though it emits no ink.
///
/// Both scalars are clamped to include the item's own origin: an empty file is
/// a zero-size page, not an inverted one. That is not a defensive tweak, it is
/// the seed the reduction has always run with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageExtent {
    /// Rightmost `x + advance` over all records, at least 0.
    pub right: f32,
    /// Lowest `y` over all records, at most 0.
    pub bottom: f32,
    /// Deepest and shallowest `z` over all records, both at least/at most 0.
    ///
    /// Two lanes rather than one because the sign of depth is a layout choice,
    /// not a fact: `z_step` is a parameter and WrapBack may push either way. A
    /// single "how deep" lane would silently assume one direction.
    pub z_min: f32,
    pub z_max: f32,
}

/// The inked bounds of one item: min/max over the QUADS of the surviving
/// instances (`x .. x + advance` horizontally, `y ± height/2` vertically) —
/// what a camera has to frame to see the glyphs, as distinct from the page
/// rectangle they were laid out on.
///
/// Empty when the item emitted no instances (min stays +inf, max -inf); the
/// caller decides what an empty view should frame, because that is a policy
/// question and the seam does not have a policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InkExtent {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// Where one item's glyphs went. This is the entire render-side return value:
/// a range, three counts and two rectangles. No positions cross it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemPlacement {
    /// First arena slot this item wrote.
    pub slot_base: u32,
    /// How many arena slots it wrote (records minus blanks).
    pub slot_count: u32,
    /// How many records the fold produced, blanks included. The pick path
    /// re-derives records and indexes them by this count, so it is part of the
    /// contract and not a statistic.
    pub record_count: u32,
    pub page: PageExtent,
    pub ink: InkExtent,
}

impl ItemPlacement {
    /// Bit-exact comparison — the relation every gate in this tree uses.
    ///
    /// `PartialEq` on floats is a DIFFERENT relation: it calls `-0.0` equal to
    /// `0.0` and `NaN` unequal to itself. Both differences matter here. A page
    /// whose bottom is `-0.0` came from a different arithmetic path than one
    /// whose bottom is `0.0`, and a NaN extent is the exact failure the params
    /// guard exists to catch — compared by value, two NaN layouts would report
    /// as "differing" and two `±0.0` layouts as "identical", which is wrong in
    /// both directions at once.
    pub fn bit_eq(&self, other: &Self) -> bool {
        let same_f32 = |a: f32, b: f32| a.to_bits() == b.to_bits();
        self.slot_base == other.slot_base
            && self.slot_count == other.slot_count
            && self.record_count == other.record_count
            && same_f32(self.page.right, other.page.right)
            && same_f32(self.page.bottom, other.page.bottom)
            && self
                .ink
                .min
                .iter()
                .chain(self.ink.max.iter())
                .zip(other.ink.min.iter().chain(other.ink.max.iter()))
                .all(|(a, b)| same_f32(*a, *b))
    }
}

/// Where laid-out glyphs land. The CALLER owns it and passes it in; a backend
/// appends and never reads back.
///
/// Two forms: a host `Vec<GlyphInstance>`; and the endpoint form (`Device`,
/// note 23's E2b) — the 32 B slots on device, bound by the renderer directly,
/// no host copy anywhere.
#[derive(Default)]
pub struct GlyphArena {
    instances: Vec<GlyphInstance>,
    /// Present on the ENDPOINT path (note 23, E2b): the chain's 32 B slots
    /// on device, bound directly — no host copy exists. Mutually exclusive
    /// with the host form; every slice-returning accessor panics on it.
    device: Option<DeviceSlots>,
    /// The third form (`--field-mode visible`, 2026-10-10): NO slots at all.
    /// Pass 1's line table and the items' own bytes, which the field lays out
    /// per frame on the GPU (`layout_hyper::visible`). `instances` stays
    /// empty, so the slice accessors return nothing rather than panicking;
    /// `len()` reports the glyphs the items WOULD produce.
    visible: Option<crate::layout_hyper::VisibleStaging>,
}

/// The endpoint's arena form (note 23, E2b): the chain's 32 B RenderSlots
/// on device, bound directly by the renderer — no host copy exists. The
/// per-slot tint lanes ride host-side (seg_tint's fold input — the slots
/// themselves are device-only).
/// Precomputed per-item tint statistics accumulated during layout Pass 2.
/// Avoids reading back instance buffers from mapped GPU memory.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileTintAccum {
    pub sum: [f64; 3],
    pub cells: usize,
    pub has_emoji: bool,
}

pub struct DeviceSlots {
    /// One entry per chunk buffer. `offset` is slot 0's byte address in
    /// the buffer — 0 for the exclusive-page big forms, nonzero where the
    /// allocator sliced.
    pub chunks: Vec<DeviceSlotChunk>,
    /// Slots per chunk (uniform; the last chunk may hold fewer).
    pub chunk_slots: usize,
    pub len: usize,
    /// When mapped in host-visible memory, base pointer to the RenderSlot slice as usize.
    /// RenderSlot ONLY — None whenever `format` is Derived (see `derived`).
    pub mapped_slots: Option<usize>,
    /// Precomputed in-flight tint accumulators per item, avoiding 3 GB mapped memory readback.
    pub file_tints: Vec<FileTintAccum>,
    /// Precomputed in-flight local block bounds per item, avoiding 3 GB mapped memory readback in build_file_blocks.
    pub file_blocks: Vec<Vec<crate::glyph_scene::BlockCull>>,
    /// Which field's slot format the chunks hold. A field binds only its own
    /// format; setup refuses a mismatch rather than drawing garbage.
    pub format: glyph_field::GlyphFieldMode,
    /// The Derived format's companions (None for Instanced).
    pub derived: Option<DerivedDeviceSlots>,
    /// Per item: `(glyph_id, color)` pairs captured at emission for items
    /// whose fast tint is not final (`has_emoji`), empty otherwise — so the
    /// tint fold never reads device slots back. Empty when not produced.
    pub emoji_tint_pairs: Vec<Vec<u32>>,
}

/// What a Derived-format device arena carries beside its 20 B slots.
pub struct DerivedDeviceSlots {
    /// Host address of the mapped `DerivedSlot` slice (unified memory), for
    /// in-place bulk color writes. None on staged (discrete) paths.
    pub mapped_base: Option<usize>,
}

/// One chunk of device slots — the field contract's [`glyph_field::SlotChunk`]
/// (buffer, slot-0 byte offset, live slots), named for its producer role here.
pub use glyph_field::SlotChunk as DeviceSlotChunk;

impl GlyphArena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a host Vec (the text/engine-text scenes stage from their own fold).
    pub fn from_vec(instances: Vec<GlyphInstance>) -> Self {
        Self { instances, device: None, visible: None }
    }

    /// The endpoint form (note 23, E2b): the chain's slots on device.
    pub fn from_device(device: DeviceSlots) -> Self {
        assert!(device.len > 0, "a device arena with zero slots is the host form's job");
        Self { instances: Vec::new(), device: Some(device), visible: None }
    }

    /// The visible form: no slots, the line table and (once the loader moves
    /// them in) the bytes.
    pub fn from_visible(staging: crate::layout_hyper::VisibleStaging) -> Self {
        Self { instances: Vec::new(), device: None, visible: Some(staging) }
    }

    /// True on the endpoint form — 32 B slots on device, bound directly.
    pub fn is_device(&self) -> bool {
        self.device.is_some()
    }

    /// True on the visible form — no slots; the field lays out per frame.
    pub fn is_visible(&self) -> bool {
        self.visible.is_some()
    }

    /// The visible form's staging. None on the other two forms.
    pub fn visible_staging(&self) -> Option<&crate::layout_hyper::VisibleStaging> {
        self.visible.as_ref()
    }

    /// The visible form's staging, to finish (the loader moves the bytes in,
    /// `into_staged` builds the items).
    pub fn visible_staging_mut(&mut self) -> Option<&mut crate::layout_hyper::VisibleStaging> {
        self.visible.as_mut()
    }

    /// The device slots (buffer + offset per chunk) for the renderer's
    /// direct bind. None on the host form.
    pub fn device_slots(&self) -> Option<&DeviceSlots> {
        self.device.as_ref()
    }

    /// Slots written so far — the next item's `slot_base`. On the visible
    /// form, the glyphs the items would produce (nothing is written).
    pub fn len(&self) -> usize {
        if let Some(d) = &self.device {
            d.len
        } else if let Some(v) = &self.visible {
            usize::try_from(v.glyph_count).expect("glyph count exceeds usize")
        } else {
            self.instances.len()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every committed instance as per-chunk slices, in chunk order.
    pub fn instance_chunks(&self) -> Vec<&[GlyphInstance]> {
        assert!(
            self.device.is_none(),
            "instance_chunks on a device arena: the slots never exist on host — \
             the renderer binds the buffers, seg_tint reads the tint lanes"
        );
        vec![&self.instances]
    }

    /// The committed instances as one slice.
    pub fn instances(&self) -> &[GlyphInstance] {
        assert!(
            self.device.is_none(),
            "instances() on a device arena: the slots never exist on host"
        );
        &self.instances
    }

    /// The committed instances, contiguous.
    pub fn instances_cow(&self) -> std::borrow::Cow<'_, [GlyphInstance]> {
        std::borrow::Cow::Borrowed(self.instances())
    }

    pub fn into_instances(self) -> Vec<GlyphInstance> {
        assert!(
            self.device.is_none(),
            "into_instances() on a device arena: the slots never exist on host"
        );
        self.instances
    }

    /// Update slot colors in-place for host or direct-mapped device slots.
    /// Returns the number of slots updated.
    pub fn update_slot_colors(&mut self, slot_base: usize, colors: &[u32]) -> usize {
        if colors.is_empty() {
            return 0;
        }
        if let Some(d) = &self.device {
            if let Some(addr) = d.mapped_slots {
                let count = colors.len().min(d.len.saturating_sub(slot_base));
                let ptr = addr as *mut crate::glyph_scene::RenderSlot;
                unsafe {
                    for (i, &c) in colors.iter().take(count).enumerate() {
                        (*ptr.add(slot_base + i)).color = c;
                    }
                }
                count
            } else {
                0
            }
        } else {
            let count = colors.len().min(self.instances.len().saturating_sub(slot_base));
            for (i, &c) in colors.iter().take(count).enumerate() {
                self.instances[slot_base + i].color = c;
            }
            count
        }
    }

    /// Hint the upper bound on slots still to come (records, before blanks are
    /// dropped). A hint only: the real count is lower and the arena grows.
    pub fn reserve(&mut self, records: usize) {
        self.instances.reserve(records);
    }

    /// Append one instance. `pub(crate)` on purpose: filling the arena is a
    /// BACKEND's job, and a caller that pushes its own instances is a caller
    /// that has smuggled a fourth layout implementation into the tree.
    pub(crate) fn push(&mut self, instance: GlyphInstance) {
        assert!(
            self.device.is_none(),
            "push into a device arena: the chain's scatter fills it on device"
        );
        self.instances.push(instance);
    }

    /// Hand a backend the arena's UNINITIALIZED tail so it can write instances
    /// where they will live, instead of building them somewhere else and
    /// copying. Returns the write pointer and how many slots are available.
    pub(crate) fn uninit_tail(&mut self, want: usize) -> (*mut GlyphInstance, usize) {
        self.instances.reserve(want);
        let len = self.instances.len();
        // SAFETY: `reserve` guarantees capacity for `want` past `len`, and the
        // pointer is only valid until the next mutation — which `commit` is,
        // and which nothing else can perform on the tail.
        let ptr = unsafe { self.instances.as_mut_ptr().add(len) };
        (ptr, want)
    }

    /// Make `written` slots of the tail visible.
    ///
    /// # Safety
    /// `written` slots starting at the pointer from the matching
    /// [`GlyphArena::uninit_tail`] must have been fully initialized, and
    /// `written` must not exceed the capacity that call returned.
    pub(crate) unsafe fn commit(&mut self, written: usize) {
        let len = self.instances.len();
        assert!(
            written <= self.instances.capacity() - len,
            "commit({written}) exceeds the {} uncommitted slots reserved",
            self.instances.capacity() - len,
        );
        unsafe { self.instances.set_len(len + written) };
    }
}

// ---------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------

/// A layout backend. Bytes and params in, instances into the arena, placements
/// out. Note what is absent: no method returns a position.
pub trait LayoutGlyphs {
    /// Which backend this is, for error attribution and for the verify report.
    fn name(&self) -> &'static str;

    /// IMPLEMENT THIS. Lay out every item, appending to `arena`, and report
    /// where each landed. The returned vector is parallel to `items`, and
    /// every item's params have already been validated when it is called.
    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError>;

    /// CALL THIS. Validates every item — naming the one that failed, because
    /// with a whole corpus in one arena an unnamed refusal is unactionable —
    /// and then delegates.
    ///
    /// It is a provided method rather than a rule each backend follows so that
    /// a backend CANNOT forget: three implementations of one fold is already
    /// three chances to omit a guard, and this removes all three.
    fn layout_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        for (index, item) in items.iter().enumerate() {
            item.params.validate(index)?;
        }
        self.layout_validated_items(items, arena)
    }
}

/// A backend that can also hand back the raw wire records — VERIFICATION ONLY.
///
/// This is the `Vec<GlyphRecord>` contract the seam exists to remove, kept as a
/// SEPARATE trait so that holding a `LayoutGlyphs` makes the readback
/// unreachable rather than merely discouraged. Gates may pay 36 B per source
/// byte to compare two backends lane by lane; frames may not.
///
/// It is a distinct call rather than a getter because the records are not a
/// by-product lying around after `layout_items`: the direct path never
/// materializes a record, and a device-resident backend keeps none on the
/// host at all. Asking for them is asking for work.
pub trait VerifyLayout: LayoutGlyphs {
    /// IMPLEMENT THIS — params already validated, as above.
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError>;

    /// CALL THIS.
    fn layout_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
        for (index, item) in items.iter().enumerate() {
            item.params.validate(index)?;
        }
        self.layout_validated_items_recording(items, arena)
    }
}

// ---------------------------------------------------------------------------
// The unified layout container
// ---------------------------------------------------------------------------

/// A unified container for layout engines (today one: the pure-Rust HyperLayout).
/// Allows versioned or alternative layout engines to be integrated cleanly behind a common interface.
pub enum LayoutEngine {
    Hyper(crate::layout_hyper::HyperLayout),
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::hyper()
    }
}

impl LayoutEngine {
    /// Constructs a HyperLayout engine (parallel Rayon CPU layout).
    pub fn hyper() -> Self {
        Self::Hyper(crate::layout_hyper::HyperLayout::new())
    }

    /// Constructs a HyperLayout engine sharing a GPU context device. The
    /// device path emits `field_mode`'s own slot format directly (32 B
    /// `RenderSlot` for Instanced, 20 B `DerivedSlot` + line table for
    /// Derived) — the renderer then binds it with no transcode.
    pub fn hyper_with_device(
        device: crate::gpu::SharedDevice,
        field_mode: glyph_field::GlyphFieldMode,
    ) -> Self {
        Self::Hyper(crate::layout_hyper::HyperLayout::with_device(device, field_mode))
    }

    /// A device-less HyperLayout that still knows the field mode: for
    /// Instanced and Derived it is `hyper()` (no device, host records); for
    /// Visible it stages the line table and bytes, which needs no device —
    /// the `--repo-scan-only` and live (`load_items`) doors to that mode.
    pub fn hyper_with_mode(field_mode: glyph_field::GlyphFieldMode) -> Self {
        Self::Hyper(crate::layout_hyper::HyperLayout::with_field_mode(field_mode))
    }

    /// Sets prefetched inputs from the background prefetch thread for HyperLayout.
    pub(crate) fn set_hyper_prefetched(&mut self, data: crate::layout_hyper::PrefetchedHyperData) {
        let Self::Hyper(h) = self;
        h.set_prefetched(data);
    }

    /// Retrieve generic backend execution phases.
    pub fn phases(&self) -> crate::repo::BackendPhases {
        crate::repo::BackendPhases::default()
    }

}

impl LayoutGlyphs for LayoutEngine {
    fn name(&self) -> &'static str {
        match self {
            Self::Hyper(b) => b.name(),
        }
    }

    fn layout_validated_items(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<Vec<ItemPlacement>, LayoutError> {
        match self {
            Self::Hyper(b) => b.layout_validated_items(items, arena),
        }
    }
}

impl VerifyLayout for LayoutEngine {
    fn layout_validated_items_recording(
        &mut self,
        items: &[LayoutItem<'_>],
        arena: &mut GlyphArena,
    ) -> Result<(Vec<ItemPlacement>, Vec<GlyphRecord>), LayoutError> {
        match self {
            Self::Hyper(b) => b.layout_validated_items_recording(items, arena),
        }
    }
}

// ---------------------------------------------------------------------------
// The host realization of compaction
// ---------------------------------------------------------------------------

/// Compact one item's records into the arena: drop the blanks, repack 32 B →
/// 48 B with paint and group, and reduce the two extents in the same pass.
///
/// NO SHIPPING BACKEND CALLS IT. `HyperLayout` emits instances directly. Its one
/// non-test caller is the `--hyper-oracle-check` instrument
/// (`hyper_oracle.rs`), which compacts the oracle-backed fold's records with
/// it. It stays as the one host statement of what compaction means
/// — blanks, paint indexing, extents — which the seam's tests pin, so a
/// backend can differ from it about the fold (the thing the corpus actually
/// checks) and is held to it for the rest. A second hand-written copy of this
/// loop would be a second place for the paint index to slip.
///
/// A backend that compacts on its own is held to the same output by
/// `--repo-verify`, which diffs arenas and placements, not just records.
///
/// Blank records (`glyph_id == 0` — missing or whitespace) emit no instance;
/// their advance is already baked into the surviving records' X by the fold,
/// so dropping them moves nothing.
pub(crate) fn compact_records_into(
    records: &[GlyphRecord],
    paint: Paint<'_>,
    group_id: u32,
    arena: &mut GlyphArena,
) -> ItemPlacement {
    if let Paint::PerRecord(colors) = paint {
        // Loud, at the seam. The two are equal by construction (one color per
        // UTF-8 leader, one record per UTF-8 leader) — which is exactly why a
        // silent `unwrap_or(DEFAULT)` fallback here could paint a whole file
        // the wrong color for a decade without anyone seeing a symptom.
        assert_eq!(
            colors.len(),
            records.len(),
            "paint is indexed by record: {} colors for {} records",
            colors.len(),
            records.len(),
        );
    }

    let slot_base = arena.len();
    arena.reserve(records.len());

    // Page: seeded at the origin, over ALL records.
    let mut page_right: f32 = 0.0;
    let mut page_bottom: f32 = 0.0;
    let mut page_z_min: f32 = 0.0;
    let mut page_z_max: f32 = 0.0;
    // Ink: seeded empty, over SURVIVORS only.
    let mut ink_min = [f32::INFINITY; 3];
    let mut ink_max = [f32::NEG_INFINITY; 3];

    for (index, record) in records.iter().enumerate() {
        let right = record.x() + record.advance();
        if right > page_right {
            page_right = right;
        }
        if record.y() < page_bottom {
            page_bottom = record.y();
        }
        if record.z() < page_z_min {
            page_z_min = record.z();
        }
        if record.z() > page_z_max {
            page_z_max = record.z();
        }
        if record.glyph_id() == 0 {
            continue;
        }
        let half_height = record.height() * 0.5;
        ink_min[0] = ink_min[0].min(record.x());
        ink_min[1] = ink_min[1].min(record.y() - half_height);
        ink_max[0] = ink_max[0].max(right);
        ink_max[1] = ink_max[1].max(record.y() + half_height);
        // Depth is a point, not a span: a glyph quad has no thickness. Both
        // lanes take the same z so the extent stays a real AABB.
        ink_min[2] = ink_min[2].min(record.z());
        ink_max[2] = ink_max[2].max(record.z());

        arena.push(GlyphInstance {
            pos: [record.x(), record.y(), record.z()],
            glyph_id: record.glyph_id(),
            row: record.row(),
            col: record.col(),
            color: match paint {
                Paint::Flat(rgba) => rgba,
                Paint::PerRecord(colors) => colors[index],
                Paint::ByteSpans(_) | Paint::SyntaxHeuristic => DEFAULT_COLOR_PACKED,
            },
            group_id,
            advance: record.advance(),
            height: record.height(),
            flags: 0, // the wire record carries no flags; the shader reads mode from the glyphmap
            _pad: 0,
        });
    }

    ItemPlacement {
        slot_base: slot_base as u32,
        slot_count: (arena.len() - slot_base) as u32,
        record_count: records.len() as u32,
        page: PageExtent {
            right: page_right,
            bottom: page_bottom,
            z_min: page_z_min,
            z_max: page_z_max,
        },
        ink: InkExtent { min: ink_min, max: ink_max },
    }
}

// ---------------------------------------------------------------------------
// Backend-vs-backend verification
// ---------------------------------------------------------------------------

/// One backend's complete output over one corpus.
pub struct BackendOutput<'a> {
    pub name: &'static str,
    pub placements: &'a [ItemPlacement],
    pub instances: &'a [GlyphInstance],
    /// The wire records, from [`VerifyLayout`]. May be empty when a backend
    /// was not asked to record them — the diff then simply has less to say,
    /// and says so.
    pub records: &'a [GlyphRecord],
}

/// What a passing verification actually covered. Returned rather than printed
/// so the caller can state it: a PASS line that does not say what it compared
/// is a claim without a scope.
#[derive(Debug)]
pub struct VerifyReport {
    pub items: usize,
    pub instances: usize,
    pub records: usize,
}

/// Diff two backends at the seam, bit-exact, and say where they first differ.
///
/// THIS IS WHAT A NEW BACKEND IS SCORED BY, which is why it compares three
/// things rather than one. The old `--repo-verify` compared RECORDS
/// only; records alone cannot see a difference in compaction, in paint
/// indexing, or in either extent — every one of which now lives behind the
/// seam and every one of which a new backend has to get right. Instances and
/// placements close that, and records stay because they are the finest
/// granularity available and they see blanks, which instances by construction
/// cannot.
pub fn diff_backends(
    a: &BackendOutput<'_>,
    b: &BackendOutput<'_>,
) -> Result<VerifyReport, String> {
    let (an, bn) = (a.name, b.name);

    if a.placements.len() != b.placements.len() {
        return Err(format!(
            "item count: {an} {} vs {bn} {}",
            a.placements.len(),
            b.placements.len()
        ));
    }
    for (index, (pa, pb)) in a.placements.iter().zip(b.placements.iter()).enumerate() {
        if !pa.bit_eq(pb) {
            return Err(format!(
                "item {index} placement differs:\n  {an}: {pa:?}\n  {bn}: {pb:?}"
            ));
        }
    }

    if a.instances.len() != b.instances.len() {
        return Err(format!(
            "instance count: {an} {} vs {bn} {}",
            a.instances.len(),
            b.instances.len()
        ));
    }
    let a_bytes: &[u8] = bytemuck::cast_slice(a.instances);
    let b_bytes: &[u8] = bytemuck::cast_slice(b.instances);
    if a_bytes != b_bytes {
        let stride = std::mem::size_of::<GlyphInstance>();
        let at = a_bytes
            .iter()
            .zip(b_bytes.iter())
            .position(|(x, y)| x != y)
            .expect("slices differ but no byte does");
        return Err(format!(
            "instance {} differs at byte {} of {stride}:\n  {an}: {:02x}\n  {bn}: {:02x}",
            at / stride,
            at % stride,
            a_bytes[at],
            b_bytes[at],
        ));
    }

    // A backend with no wire stream — `Strategy::Direct` materializes none by
    // design — contributes no record tier, and the diff says so by reporting
    // zero records rather than failing on the asymmetry. This is deliberately
    // NOT `zip`-and-shrug: an EMPTY side means "cannot produce", and the
    // caller must state the reduced scope in its PASS line, because a pass
    // that does not say what it compared is a claim without a scope.
    //
    // The tiers above are not weakened by this. Instances and placements are
    // the entire render-visible contract, and they were compared byte for byte
    // before reaching here.
    let compare_records = !a.records.is_empty() && !b.records.is_empty();
    if compare_records && a.records.len() != b.records.len() {
        return Err(format!(
            "record count: {an} {} vs {bn} {}",
            a.records.len(),
            b.records.len()
        ));
    }
    let (ra_all, rb_all): (&[GlyphRecord], &[GlyphRecord]) =
        if compare_records { (a.records, b.records) } else { (&[], &[]) };
    for (index, (ra, rb)) in ra_all.iter().zip(rb_all.iter()).enumerate() {
        let same = ra.counts == rb.counts
            && ra
                .measures
                .iter()
                .zip(rb.measures.iter())
                .all(|(x, y)| x.to_bits() == y.to_bits());
        if !same {
            return Err(format!(
                "record {index} differs:\n  {an}: {ra:?}\n  {bn}: {rb:?}"
            ));
        }
    }

    Ok(VerifyReport {
        items: a.placements.len(),
        instances: a.instances.len(),
        records: ra_all.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_params() -> ItemParams {
        ItemParams { line_height: 1.2, ..Default::default() }
    }

    #[test]
    fn a_stated_pitch_still_runs() {
        // THE COUNTERFACTUAL FOR THE GUARD. Without this, `return Err(..)` at
        // the top of validate() would pass every negative test below.
        assert!(ok_params().validate(0).is_ok());
    }

    #[test]
    fn zero_is_legal_a_degenerate_pitch_is_a_choice() {
        // Every row on one baseline is a layout, not an omission. The reflex
        // `if lh == 0.0 { default }` conflates them; we must not.
        let p = ItemParams { line_height: 0.0, ..Default::default() };
        assert!(p.validate(0).is_ok(), "zero pitch must be accepted, not defaulted");
    }

    #[test]
    fn nan_pitch_is_refused_and_named_as_the_unset_encoding() {
        let p = ItemParams { line_height: f64::NAN, ..Default::default() };
        let e = p.validate(3).expect_err("NaN pitch must be refused");
        assert_eq!(e.status, LAYOUT_BAD_PARAMS);
        assert_eq!(e.backend, "seam", "the refusal is the host's, before any backend");
        assert!(e.what.contains("item 3"), "must name the item: {}", e.what);
        assert!(e.what.contains("line_height"), "must name the field: {}", e.what);
        assert!(e.what.contains("UNSET"), "must say NaN means unset: {}", e.what);
    }

    #[test]
    fn infinite_pitch_is_refused() {
        for lh in [f64::INFINITY, f64::NEG_INFINITY] {
            let p = ItemParams { line_height: lh, ..Default::default() };
            assert!(p.validate(0).is_err(), "{lh} must be refused");
        }
    }

    #[test]
    fn every_measure_is_checked_not_just_the_pitch() {
        // A non-finite anywhere propagates into positions. Walk them all so a
        // newly added measure that skips validate() shows up as a gap here.
        let mut n = 0;
        for mutate in [
            (|p: &mut ItemParams| p.origin_x = f64::NAN) as fn(&mut ItemParams),
            |p: &mut ItemParams| p.origin_y = f64::INFINITY,
            |p: &mut ItemParams| p.origin_z = f64::NAN,
            |p: &mut ItemParams| p.z_step = f64::NAN,
            |p: &mut ItemParams| p.page_gap_x = f64::NAN,
            |p: &mut ItemParams| p.band_stride_y = f64::NAN,
            |p: &mut ItemParams| p.depth_per_band = f64::NAN,
            |p: &mut ItemParams| p.depth_per_col = f64::NAN,
            |p: &mut ItemParams| p.page_line_height = f64::NAN,
        ] {
            let mut p = ok_params();
            mutate(&mut p);
            assert!(p.validate(0).is_err(), "a non-finite measure slipped through");
            n += 1;
        }
        assert_eq!(n, 9, "the sweep must cover every f64 measure but the pitch");
    }

    #[test]
    fn negative_page_geometry_is_refused() {
        let mut n = 0;
        for mutate in [
            (|p: &mut ItemParams| p.wrap_width = -1) as fn(&mut ItemParams),
            |p: &mut ItemParams| p.page_rows = -1,
            |p: &mut ItemParams| p.page_cols = -1,
            |p: &mut ItemParams| p.scroll_rows = -1,
            |p: &mut ItemParams| p.pages_wide = -1,
        ] {
            let mut p = ok_params();
            mutate(&mut p);
            assert!(p.validate(0).is_err(), "a negative count slipped through");
            n += 1;
        }
        assert_eq!(n, 5, "the sweep must cover every integer count");
    }

    #[test]
    fn the_default_params_are_themselves_valid() {
        // repo.rs and main.rs both build on ..Default::default(); if the default
        // were invalid every caller would fail at the seam.
        assert!(ItemParams::default().validate(0).is_ok());
    }

    /// A hand-built record stream with a known answer: three inked glyphs and
    /// two blanks, so compaction, paint indexing and both extents are each
    /// distinguishable from every plausible off-by-one.
    fn sample_records() -> Vec<GlyphRecord> {
        //          X     Y     Z   ADV    H  |  GID  ROW  COL
        let rows: [([f32; 5], [u32; 3]); 5] = [
            ([0.0, 0.0, 0.0, 2.0, 4.0], [7, 0, 0]),   // ink
            ([2.0, 0.0, 0.0, 3.0, 4.0], [0, 0, 1]),   // BLANK, widest right edge
            ([5.0, -1.0, 0.0, 1.0, 4.0], [9, 1, 0]),  // ink, lowest inked y
            ([6.0, -9.0, 0.0, 0.5, 4.0], [0, 2, 0]),  // BLANK, lowest y overall
            ([1.0, -1.0, 0.0, 1.0, 8.0], [3, 1, 1]),  // ink, tallest
        ];
        rows.iter().map(|(m, c)| GlyphRecord { measures: *m, counts: *c }).collect()
    }

    /// Depth reaches the extents, from the records, without a constant.
    ///
    /// This is the guard against fixing the cull arithmetic and calling it
    /// done: `cull_segments` can be made to respect depth while every segment
    /// handed to it still claims the old flat slab, in which case the unit
    /// tests pass, the four baselines stay byte-equal, and production is
    /// exactly as blind as before. The reduction below is the only place a
    /// record's z becomes an extent, so this is where that shortcut would have
    /// to hide.
    #[test]
    fn extents_carry_depth_from_the_records() {
        // Two inked glyphs at different depths, and a blank deeper than both.
        let records: Vec<GlyphRecord> = [
            ([0.0, 0.0, -2.0, 1.0, 4.0], [7u32, 0, 0]),  // ink, shallow
            ([1.0, 0.0, -9.5, 1.0, 4.0], [8, 0, 1]),     // ink, deep
            ([2.0, 0.0, -30.0, 1.0, 4.0], [0, 0, 2]),    // BLANK, deepest
        ]
        .iter()
        .map(|(m, c)| GlyphRecord { measures: *m, counts: *c })
        .collect();

        let mut arena = GlyphArena::new();
        let p = compact_records_into(&records, Paint::Flat(0), 0, &mut arena);

        // Page is over ALL records, blanks included — it is the conservative
        // box, and a cull built from it must not clip a blank's position away.
        assert_eq!(p.page.z_min, -30.0, "page depth must include blanks");
        assert_eq!(p.page.z_max, 0.0, "page depth is seeded at the origin");

        // Ink is over survivors only, so the blank's -30 must NOT widen it.
        assert_eq!(p.ink.min[2], -9.5, "ink depth must span the inked glyphs");
        assert_eq!(p.ink.max[2], -2.0);

        // And the thing that would betray a constant: none of these is ±1.
        assert!(
            p.page.z_min < -1.0 && p.ink.min[2] < -1.0,
            "depth looks like the old ±1 slab rather than the records"
        );

        // PAGE CONTAINS INK IN Z, and that is what makes it safe to build the
        // cull's depth from: a box containing everything drawn can only cost a
        // draw, while one that does not can drop something visible.
        assert!(p.page.z_min <= p.ink.min[2] && p.page.z_max >= p.ink.max[2]);

        // It does NOT contain ink in Y, and this assertion is here because I
        // assumed it did and was wrong. `page.bottom` tracks each record's
        // BASELINE; `ink` tracks the glyph QUAD, which hangs half a height
        // below that baseline. So ink reaches outside the page rectangle
        // vertically — which is why the repo cull's xy carries hand-tuned
        // margins (-0.5 below, +0.75 above in `repo.rs`) rather than using the
        // page directly.
        //
        // Depth has no such problem because a glyph quad has no thickness: the
        // z extent is a point per record, so page and ink measure the same
        // thing in that axis and containment is exact.
        assert!(p.page.right >= p.ink.max[0], "page is a superset horizontally");
        assert!(
            p.page.bottom > p.ink.min[1],
            "page.bottom tracks baselines, so ink must hang below it"
        );
    }

    /// How this reduction relates to the ENGINE's per-item box, which is a
    /// different computation and deliberately not the same number.
    ///
    /// `fold::bounds_range` (verified against the corpus by `--fixture-fold`)
    /// seeds at ±infinity and measures
    /// only what is there. `page` seeds at the ORIGIN, so it always contains
    /// (0,0,0) whether or not a glyph does. They therefore disagree by the seed
    /// for any item whose content does not straddle the origin — which is most
    /// of them — and that disagreement is correct rather than a defect: the two
    /// answer different questions. `page` is "what rectangle was this laid out
    /// on", the engine box is "where did the glyphs actually land".
    ///
    /// The consequence worth pinning: `page` CONTAINS the engine box, so a cull
    /// built from it is conservative with respect to the engine's own answer.
    #[test]
    fn page_contains_the_engine_box_and_differs_by_the_seed() {
        let records: Vec<GlyphRecord> = [
            ([3.0, -4.0, -8.0, 1.0, 2.0], [7u32, 0, 0]),
            ([4.0, -6.0, -5.0, 1.0, 2.0], [8, 1, 0]),
        ]
        .iter()
        .map(|(m, c)| GlyphRecord { measures: *m, counts: *c })
        .collect();

        let mut arena = GlyphArena::new();
        let p = compact_records_into(&records, Paint::Flat(0), 0, &mut arena);

        // The engine box over the same records: seeded empty, not at origin.
        let (mut ez_lo, mut ez_hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for r in &records {
            ez_lo = ez_lo.min(r.z());
            ez_hi = ez_hi.max(r.z());
        }
        assert_eq!((ez_lo, ez_hi), (-8.0, -5.0));

        // Contained...
        assert!(p.page.z_min <= ez_lo && p.page.z_max >= ez_hi);
        // ...and NOT equal, because of the origin seed. If this ever starts
        // holding, the seeding changed and the cull's conservatism changed
        // with it.
        assert_ne!(p.page.z_max, ez_hi, "page must keep its origin seed");
    }

    #[test]
    fn compaction_drops_blanks_and_reports_the_range() {
        let records = sample_records();
        let mut arena = GlyphArena::new();
        let placement = compact_records_into(&records, Paint::Flat(0xDEAD_BEEF), 4, &mut arena);

        assert_eq!(placement.record_count, 5, "records include blanks");
        assert_eq!(placement.slot_count, 3, "instances do not");
        assert_eq!(placement.slot_base, 0);
        assert_eq!(arena.len(), 3);
        let ids: Vec<u32> = arena.instances().iter().map(|g| g.glyph_id).collect();
        assert_eq!(ids, vec![7, 9, 3], "survivors keep record order");
        assert!(arena.instances().iter().all(|g| g.group_id == 4 && g.color == 0xDEAD_BEEF));
    }

    #[test]
    fn a_second_item_appends_and_its_slot_base_says_where() {
        let records = sample_records();
        let mut arena = GlyphArena::new();
        let first = compact_records_into(&records, Paint::Flat(1), 0, &mut arena);
        let second = compact_records_into(&records, Paint::Flat(2), 1, &mut arena);
        assert_eq!(first.slot_base, 0);
        assert_eq!(second.slot_base, first.slot_count, "the arena is one shared range");
        assert_eq!(arena.len() as u32, first.slot_count + second.slot_count);
    }

    #[test]
    fn paint_is_indexed_by_record_so_blanks_consume_an_entry() {
        // THE FAILURE THIS PINS. Index the colors by INSTANCE instead and the
        // survivors get 10, 20, 30 — a mistake that looks right (every glyph is
        // painted, the count matches) and is wrong from the first blank on.
        let records = sample_records();
        let colors = [10u32, 20, 30, 40, 50];
        let mut arena = GlyphArena::new();
        compact_records_into(&records, Paint::PerRecord(&colors), 0, &mut arena);
        let got: Vec<u32> = arena.instances().iter().map(|g| g.color).collect();
        assert_eq!(got, vec![10, 30, 50], "record indices 0, 2, 4 survived");
    }

    #[test]
    #[should_panic(expected = "paint is indexed by record")]
    fn a_paint_array_that_is_not_record_length_is_refused_loudly() {
        let records = sample_records();
        let colors = [10u32, 20, 30]; // one per SURVIVOR — the tempting mistake
        let mut arena = GlyphArena::new();
        compact_records_into(&records, Paint::PerRecord(&colors), 0, &mut arena);
    }

    #[test]
    fn the_page_extent_counts_blanks_and_holds_the_origin() {
        let records = sample_records();
        let mut arena = GlyphArena::new();
        let placement = compact_records_into(&records, Paint::Flat(0), 0, &mut arena);
        // BOTH extremes of the page belong to BLANK records here, on purpose:
        // the widest right edge is record 3 (x=6, advance=0.5) and the lowest
        // y is record 3 as well. Measure the page over survivors only and this
        // test reports 6.0 and -1.0 instead. A page is what the fold laid out,
        // not what happened to be inked.
        assert_eq!(placement.page.right, 6.5, "a blank's advance still widens the page");
        assert_eq!(placement.page.bottom, -9.0, "a blank's row still deepens the page");

        // The origin is always inside the page: an item laid out entirely above
        // and right of it reports the seeds, not an inverted rectangle.
        let above = [GlyphRecord { measures: [3.0, 5.0, 0.0, 1.0, 1.0], counts: [1, 0, 0] }];
        let mut elsewhere = GlyphArena::new();
        let clamped = compact_records_into(&above, Paint::Flat(0), 0, &mut elsewhere);
        assert_eq!(clamped.page.bottom, 0.0, "bottom is clamped at the origin");
        assert_eq!(clamped.page.right, 4.0, "right is the real edge when it exceeds the origin");
    }

    #[test]
    fn the_ink_extent_measures_quads_of_survivors_only() {
        let records = sample_records();
        let mut arena = GlyphArena::new();
        let placement = compact_records_into(&records, Paint::Flat(0), 0, &mut arena);
        let ink = placement.ink;
        assert_eq!(ink.min[0], 0.0, "leftmost inked x");
        assert_eq!(ink.max[0], 6.0, "rightmost inked x+advance (record 2)");
        // Vertical: record 4 is tallest (h=8 at y=-1) → -1-4 = -5 .. -1+4 = 3.
        assert_eq!(ink.min[1], -5.0);
        assert_eq!(ink.max[1], 3.0);
        // The blank at y=-9 lowered the PAGE but must not lower the INK.
        assert_eq!(placement.page.bottom, -9.0);
        assert!(ink.min[1] > placement.page.bottom, "ink is not the page");
    }

    /// `bit_eq` exists because `PartialEq` is a different relation on floats.
    /// If it ever silently becomes `==`, this is what notices.
    #[test]
    fn bit_eq_and_partial_eq_disagree_exactly_where_they_should() {
        let base = ItemPlacement {
            slot_base: 0,
            slot_count: 1,
            record_count: 1,
            page: PageExtent { right: 1.0, bottom: 0.0, z_min: 0.0, z_max: 0.0 },
            ink: InkExtent { min: [0.0, 0.0, 0.0], max: [1.0, 1.0, 0.0] },
        };
        let mut negative_zero = base;
        negative_zero.page.bottom = -0.0;
        assert_eq!(negative_zero, base, "PartialEq calls -0.0 and 0.0 equal");
        assert!(!negative_zero.bit_eq(&base), "bit_eq must not");

        let mut nan = base;
        nan.page.right = f32::NAN;
        let mut same_nan = base;
        same_nan.page.right = f32::NAN;
        assert_ne!(nan, same_nan, "PartialEq calls NaN unequal to itself");
        assert!(nan.bit_eq(&same_nan), "bit_eq must call identical bits identical");
    }

    /// WHAT A NEW BACKEND IS SCORED BY. A differ that cannot report a difference
    /// is worse than no differ, so break the output in each of the three
    /// places it looks and confirm all three are seen — and confirm the
    /// unbroken pair passes, or the three reds prove nothing.
    #[test]
    fn diff_backends_sees_a_difference_in_each_of_the_three_things_it_compares() {
        let records = sample_records();
        let mut arena = GlyphArena::new();
        let places = vec![compact_records_into(&records, Paint::Flat(7), 0, &mut arena)];
        let instances = arena.instances().to_vec();
        let out = |name, p: &[ItemPlacement], i: &[GlyphInstance], r: &[GlyphRecord]| BackendOutput {
            name,
            placements: p.to_vec().leak(),
            instances: i.to_vec().leak(),
            records: r.to_vec().leak(),
        };

        // THE COUNTERFACTUAL: identical output must pass, or every red below
        // is just "the differ always complains".
        let report = diff_backends(
            &out("a", &places, &instances, &records),
            &out("b", &places, &instances, &records),
        )
        .expect("identical output must verify");
        assert_eq!(report.items, 1);
        assert_eq!(report.instances, instances.len());
        assert_eq!(report.records, records.len());

        // 1. placement — one float, one ulp.
        let mut bent = places.clone();
        bent[0].page.right = f32::from_bits(places[0].page.right.to_bits() + 1);
        let why = diff_backends(
            &out("a", &places, &instances, &records),
            &out("b", &bent, &instances, &records),
        )
        .expect_err("a one-ulp placement difference must be seen");
        assert!(why.contains("item 0 placement"), "{why}");

        // 2. instance — one byte of one slot.
        let mut bent = instances.clone();
        bent[1].col ^= 1;
        let why = diff_backends(
            &out("a", &places, &instances, &records),
            &out("b", &places, &bent, &records),
        )
        .expect_err("a one-bit instance difference must be seen");
        assert!(why.contains("instance 1 differs"), "{why}");

        // 3. record — a lane that compaction DROPS, so only this half can see
        //    it. Record 3 is a blank: no instance carries its position, and
        //    its z is not extremal, so neither extent moves either.
        let mut bent = records.clone();
        bent[3].measures[2] = 1.0;
        let why = diff_backends(
            &out("a", &places, &instances, &records),
            &out("b", &places, &instances, &bent),
        )
        .expect_err("a blank record's lane must still be seen");
        assert!(why.contains("record 3 differs"), "{why}");
    }

    #[test]
    fn an_item_with_no_records_reports_an_empty_ink_extent() {
        let mut arena = GlyphArena::new();
        let placement = compact_records_into(&[], Paint::Flat(0), 0, &mut arena);
        assert_eq!(placement.slot_count, 0);
        assert_eq!(placement.record_count, 0);
        assert_eq!(
            placement.page,
            PageExtent { right: 0.0, bottom: 0.0, z_min: 0.0, z_max: 0.0 }
        );
        assert!(placement.ink.min[0].is_infinite() && placement.ink.min[0] > 0.0);
        assert!(placement.ink.max[0].is_infinite() && placement.ink.max[0] < 0.0);
    }
}
