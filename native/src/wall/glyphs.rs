//! Runtime glyph source for the terminal wall cell renderer (`GlyphSource`).
//!
//! Bridges `crates/glyph-font` into `native/src/wall/`:
//! - Adapts the engine's [`Atlas`] emoji sheet + sequence table into
//!   [`glyph_font::EmojiSource`] without duplicating PNGs or re-reading `emoji-sheet.bin`.
//! - Under the `wgpu` feature, `GlyphSource` owns a [`glyph_font::FontAtlas`] loaded from
//!   `assets/fonts/` (default family Monaspace Neon, matching the JS wall; Cousine and
//!   Monaspace Argon selectable), creates and owns the two `Rgba32Uint` wgpu textures
//!   (`glyphmap` and `curves`) in the exact layout `wall_cells.wgsl` bindings 1 and 2 sample,
//!   uploads only dirty texel rectangles each frame and recreates a texture when it grows,
//!   answers cell resolution (`resolve_cell`), and converts the active family's cell and line
//!   metrics into `CellMetrics` and `WallStyle`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glyph_font::EmojiSource;
#[cfg(feature = "wgpu")]
use glyph_font::{
    default_font_chain, FamilyId, FontAtlas, FontError, LineMetrics, ResolvedCell, TextureBacking,
    TextureDirty, CURVE_INITIAL_ROWS, MAP_INITIAL_ROWS, MONASPACE_NEON_FAMILY, TEXTURE_WIDTH,
    U32S_PER_TEXEL,
};
#[cfg(feature = "wgpu")]
use term_cells::{Cell, Screen};

use crate::atlas::Atlas;
#[cfg(feature = "wgpu")]
use crate::text::CELL_HEIGHT_WORLD;

#[cfg(feature = "wgpu")]
use super::metrics::CellMetrics;
#[cfg(feature = "wgpu")]
use super::renderer::WallStyle;

/// Bytes per `Rgba32Uint` texel (`4 * sizeof(u32)`).
#[cfg(feature = "wgpu")]
const BYTES_PER_TEXEL: u32 = U32S_PER_TEXEL * 4;

/// Default directory holding the wall's runtime font files (`<repo>/assets/fonts`).
pub fn default_fonts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts")
}

/// Adapts the engine's loaded [`Atlas`] (its `EmojiSheet` codepoint/cell tables and its
/// v2 `TrieTable` sequence table) to [`glyph_font::EmojiSource`].
///
/// Single-codepoint lookups query `atlas.emoji.sheet` directly so BMP emoji that also
/// have monochrome outlines (such as `❤` U+2764) still resolve their colour cell when
/// requested with VS16 (`"❤️"`). Multi-codepoint sequences (ZWJ families, flags, skin
/// tones, keycaps) query `atlas.trie.sequence_lookup` with `U+FE0F` stripped, matching
/// `native/src/wall/cell.rs`.
pub struct AtlasEmojiSource {
    atlas: Arc<Atlas>,
}

impl AtlasEmojiSource {
    pub fn new(atlas: Arc<Atlas>) -> Self {
        Self { atlas }
    }

    fn cell_for_single(&self, cp: u32) -> Option<u32> {
        let sheet = &self.atlas.emoji.sheet;
        let idx = sheet
            .codepoints
            .binary_search_by_key(&cp, |&(k, _)| k)
            .ok()?;
        let glyph_id = sheet.codepoints[idx].1;
        sheet
            .cells
            .binary_search_by_key(&glyph_id, |c| c.glyph_id)
            .ok()
            .map(|cell_idx| cell_idx as u32)
    }

    fn cell_for_sequence(&self, cps: &[u32]) -> Option<u32> {
        let trie = &self.atlas.trie;
        if cps.len() < 2 || !trie.starts_a_sequence(cps[0]) {
            return None;
        }
        let max_len = cps.len().min(trie.seq_max as usize);
        for probe_len in (2..=max_len).rev() {
            if let Some(slot) = trie.sequence_lookup(&cps[..probe_len]) {
                if let Some(cell) = trie.emoji_cell.get(slot as usize).copied().flatten() {
                    return Some(cell);
                }
            }
        }
        None
    }
}

impl EmojiSource for AtlasEmojiSource {
    fn cell_for(&self, text: &str) -> Option<u32> {
        let mut key = [0u32; 16];
        let mut len = 0usize;
        for ch in text.chars() {
            let cp = ch as u32;
            if cp == 0xFE0F {
                continue;
            }
            if len == key.len() {
                return None;
            }
            key[len] = cp;
            len += 1;
        }
        if len == 0 {
            return None;
        }
        if len == 1 {
            return self.cell_for_single(key[0]);
        }
        self.cell_for_sequence(&key[..len])
    }
}

/// Per-frame GPU upload report returned by [`GlyphSource::sync`].
#[cfg(feature = "wgpu")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphSyncStats {
    /// True when either the `curves` or `glyphmap` texture was recreated after growing.
    /// Callers holding a `BindGroup` over [`GlyphSource::glyphmap_view`] /
    /// [`GlyphSource::curves_view`] must rebuild that bind group when this is true.
    pub rebound: bool,
    /// Number of `queue.write_texture` calls issued across both Slug textures.
    pub write_calls: usize,
    /// Total `Rgba32Uint` texels uploaded across both Slug textures.
    pub texels_uploaded: u64,
    /// Total bytes uploaded across both Slug textures.
    pub bytes_uploaded: u64,
}

/// Runtime Slug glyph source owning a [`FontAtlas`] and its live `wgpu` `glyphmap` and
/// `curves` textures (`Rgba32Uint`, 1024 texels wide — a drop-in for `wall_cells.wgsl`
/// group 0 bindings 1 and 2).
#[cfg(feature = "wgpu")]
pub struct GlyphSource {
    atlas: FontAtlas,
    active_family: FamilyId,
    curves_tex: wgpu::Texture,
    curves_view: wgpu::TextureView,
    glyphmap_tex: wgpu::Texture,
    glyphmap_view: wgpu::TextureView,
}

#[cfg(feature = "wgpu")]
impl GlyphSource {
    /// Load the wall's default font chain from `assets/fonts/` (selecting Monaspace Neon
    /// as the active family, with Cousine and Monaspace Argon available), wire `atlas`'s
    /// emoji sheet as the colour-emoji source, and allocate the initial GPU textures.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, atlas: Arc<Atlas>) -> Self {
        Self::load(device, queue, &default_fonts_dir(), atlas)
            .expect("failed to load default wall font chain from assets/fonts")
    }

    /// Load the default font chain from `fonts_dir` and wire `atlas`'s emoji sheet.
    pub fn load(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fonts_dir: &Path,
        atlas: Arc<Atlas>,
    ) -> Result<Self, FontError> {
        let emoji: Arc<dyn EmojiSource> = Arc::new(AtlasEmojiSource::new(atlas));
        Self::load_with_initial_rows(
            device,
            queue,
            fonts_dir,
            Some(emoji),
            CURVE_INITIAL_ROWS,
            MAP_INITIAL_ROWS,
        )
    }

    /// Load the default font chain with explicit emoji source and initial texture row capacities.
    pub fn load_with_initial_rows(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fonts_dir: &Path,
        emoji_source: Option<Arc<dyn EmojiSource>>,
        curve_rows: u32,
        map_rows: u32,
    ) -> Result<Self, FontError> {
        let specs = default_font_chain(fonts_dir);
        let mut atlas = FontAtlas::load_with_initial_rows(&specs, curve_rows, map_rows)?;
        atlas.set_emoji_source(emoji_source);

        // JS wall default: Monaspace Neon; fall back to chain default if absent.
        let active_family = atlas
            .family_id(MONASPACE_NEON_FAMILY)
            .unwrap_or_else(|| atlas.default_family());

        // Drain any initial clean state and upload the zero-initialised initial textures so
        // every texel on the device starts defined (slot 0 = [0, 0, 0, 0]).
        let _ = atlas.take_dirty();
        let (curves_tex, curves_view) =
            create_and_upload_full(device, queue, "wall slug curves", atlas.curve_texture());
        let (glyphmap_tex, glyphmap_view) = create_and_upload_full(
            device,
            queue,
            "wall slug glyphmap",
            atlas.glyphmap_texture(),
        );

        Ok(Self {
            atlas,
            active_family,
            curves_tex,
            curves_view,
            glyphmap_tex,
            glyphmap_view,
        })
    }

    /// Underlying [`FontAtlas`] (for diagnostics, family listing, or readback verification).
    pub fn atlas(&self) -> &FontAtlas {
        &self.atlas
    }

    /// Currently selected font family (default: Monaspace Neon).
    #[inline]
    pub fn family(&self) -> FamilyId {
        self.active_family
    }

    /// Look up a family handle by id (`"monaspace-neon"`, `"cousine"`, `"monaspace-argon"`).
    pub fn family_id(&self, name: &str) -> Option<FamilyId> {
        self.atlas.family_id(name)
    }

    /// Switch the active font family by id (`"monaspace-neon"`, `"cousine"`, `"monaspace-argon"`).
    /// Returns the selected [`FamilyId`] or `None` if `name` is not registered.
    pub fn set_family(&mut self, name: &str) -> Option<FamilyId> {
        let id = self.atlas.family_id(name)?;
        self.active_family = id;
        Some(id)
    }

    /// Switch the active font family directly by [`FamilyId`].
    pub fn set_family_id(&mut self, family: FamilyId) {
        assert!(
            family.0 < self.atlas.family_count(),
            "family id {} out of range ({} families)",
            family.0,
            self.atlas.family_count()
        );
        self.active_family = family;
    }

    /// Resolve `(slot, span_cols)` for `cell` in the active font family.
    ///
    /// Covers ASCII/Unicode scalars, bold/italic faces from `cell.attrs`, cell sprites,
    /// combining-mark clusters, colour emoji, spaces, blanks, and trailing wide cells.
    pub fn resolve_cell(&mut self, screen: &Screen, cell: &Cell) -> ResolvedCell {
        self.atlas.resolve_cell(self.active_family, screen, cell)
    }

    /// Resolve `(slot, span_cols)` for `cell` in an explicit font `family`.
    pub fn resolve_cell_in(
        &mut self,
        family: FamilyId,
        screen: &Screen,
        cell: &Cell,
    ) -> ResolvedCell {
        self.atlas.resolve_cell(family, screen, cell)
    }

    /// Snapped world-space [`CellMetrics`] for the active family (`CELL_HEIGHT_WORLD = 1.0`).
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell_metrics_for(self.active_family)
    }

    /// Snapped world-space [`CellMetrics`] for `family`.
    pub fn cell_metrics_for(&self, family: FamilyId) -> CellMetrics {
        let m = self.atlas.family_metrics(family);
        let raw_width = (f64::from(m.advance_fu) * f64::from(CELL_HEIGHT_WORLD)
            / m.extents.em_height()) as f32;
        CellMetrics::snapped(raw_width, CELL_HEIGHT_WORLD)
    }

    /// Normalised `[0, 1]` cell-space underline/strikethrough metrics for the active family.
    pub fn line_metrics(&self) -> Option<LineMetrics> {
        self.line_metrics_for(self.active_family)
    }

    /// Normalised `[0, 1]` cell-space underline/strikethrough metrics for `family`.
    pub fn line_metrics_for(&self, family: FamilyId) -> Option<LineMetrics> {
        self.atlas.family_metrics(family).lines
    }

    /// Copy the active family's underline/strikethrough position and thickness into `style`.
    pub fn apply_line_metrics(&self, style: &mut WallStyle) {
        self.apply_line_metrics_for(self.active_family, style);
    }

    /// Copy `family`'s underline/strikethrough position and thickness into `style`.
    pub fn apply_line_metrics_for(&self, family: FamilyId, style: &mut WallStyle) {
        if let Some(lines) = self.line_metrics_for(family) {
            style.underline_y = lines.underline_y;
            style.strike_y = lines.strike_y;
            style.line_thickness = lines.thickness;
        }
    }

    /// GPU `glyphmap` texture view (drop-in for `wall_cells.wgsl` group 0 binding 1).
    pub fn glyphmap_view(&self) -> &wgpu::TextureView {
        &self.glyphmap_view
    }

    /// GPU `curves` texture view (drop-in for `wall_cells.wgsl` group 0 binding 2).
    pub fn curves_view(&self) -> &wgpu::TextureView {
        &self.curves_view
    }

    /// Underlying GPU `glyphmap` texture (`Rgba32Uint`, 1024 texels wide).
    pub fn glyphmap_texture(&self) -> &wgpu::Texture {
        &self.glyphmap_tex
    }

    /// Underlying GPU `curves` texture (`Rgba32Uint`, 1024 texels wide).
    pub fn curves_texture(&self) -> &wgpu::Texture {
        &self.curves_tex
    }

    /// Upload any texels written since the last call to `sync`, recreating either GPU texture
    /// if its backing array grew. On an unchanged frame, issues zero GPU writes and returns
    /// [`GlyphSyncStats::default()`].
    pub fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> GlyphSyncStats {
        let dirty = self.atlas.take_dirty();
        let mut stats = GlyphSyncStats::default();

        if dirty.curves.is_dirty() {
            sync_one_texture(
                device,
                queue,
                "wall slug curves",
                &dirty.curves,
                self.atlas.curve_texture(),
                &mut self.curves_tex,
                &mut self.curves_view,
                &mut stats,
            );
        }
        if dirty.glyphmap.is_dirty() {
            sync_one_texture(
                device,
                queue,
                "wall slug glyphmap",
                &dirty.glyphmap,
                self.atlas.glyphmap_texture(),
                &mut self.glyphmap_tex,
                &mut self.glyphmap_view,
                &mut stats,
            );
        }

        stats
    }
}

#[cfg(feature = "wgpu")]
fn create_uint_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    (tex, view)
}

#[cfg(feature = "wgpu")]
fn create_and_upload_full(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    backing: TextureBacking<'_>,
) -> (wgpu::Texture, wgpu::TextureView) {
    let (tex, view) = create_uint_texture(device, label, backing.width, backing.height);
    upload_full_texture(queue, &tex, backing);
    (tex, view)
}

#[cfg(feature = "wgpu")]
fn upload_full_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    backing: TextureBacking<'_>,
) {
    assert_eq!(
        backing.data.len(),
        (backing.width as usize) * (backing.height as usize) * (U32S_PER_TEXEL as usize),
        "backing buffer length mismatch"
    );
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(backing.data),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(backing.width * BYTES_PER_TEXEL),
            rows_per_image: Some(backing.height),
        },
        wgpu::Extent3d {
            width: backing.width,
            height: backing.height,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(feature = "wgpu")]
#[expect(
    clippy::too_many_arguments,
    reason = "internal helper syncing one Rgba32Uint texture from FontAtlas dirty state"
)]
fn sync_one_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    dirty: &TextureDirty,
    backing: TextureBacking<'_>,
    texture: &mut wgpu::Texture,
    view: &mut wgpu::TextureView,
    stats: &mut GlyphSyncStats,
) {
    if dirty.grew {
        let (new_tex, new_view) = create_and_upload_full(device, queue, label, backing);
        *texture = new_tex;
        *view = new_view;
        let texels = u64::from(backing.width) * u64::from(backing.height);
        stats.rebound = true;
        stats.write_calls += 1;
        stats.texels_uploaded += texels;
        stats.bytes_uploaded += texels * u64::from(BYTES_PER_TEXEL);
        return;
    }

    for rect in dirty.rects() {
        // texel_range_to_rects yields height == 1 for partial rows and full width for multi-row spans,
        // so every rect maps to one contiguous slice of `backing.data`.
        assert!(
            rect.height == 1 || (rect.x == 0 && rect.width == TEXTURE_WIDTH),
            "multi-row upload rect must be full-width"
        );
        let start_texel = (rect.y as usize) * (backing.width as usize) + (rect.x as usize);
        let texel_count = (rect.width as usize) * (rect.height as usize);
        let start_word = start_texel * (U32S_PER_TEXEL as usize);
        let end_word = start_word + texel_count * (U32S_PER_TEXEL as usize);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&backing.data[start_word..end_word]),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rect.width * BYTES_PER_TEXEL),
                rows_per_image: Some(rect.height),
            },
            wgpu::Extent3d {
                width: rect.width,
                height: rect.height,
                depth_or_array_layers: 1,
            },
        );
        stats.write_calls += 1;
        stats.texels_uploaded += texel_count as u64;
        stats.bytes_uploaded += (texel_count as u64) * u64::from(BYTES_PER_TEXEL);
    }
}

#[cfg(all(test, feature = "wgpu"))]
mod tests {
    use super::*;
    use glyph_font::{
        BLANK_SLOT, DEFAULT_FONT_FAMILY, GLYPH_MODE_BITMAP, GLYPH_MODE_OUTLINE, GLYPH_MODE_SPRITE,
        MONASPACE_ARGON_FAMILY,
    };
    use term_cells::{attrs, rgb, CLUSTER_BASE};

    /// Read back an entire `Rgba32Uint` 2D texture from `device` into a flat `Vec<u32>`
    /// matching `TextureBacking::data` layout (`width * height * 4`).
    fn readback_uint_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Vec<u32> {
        let size = texture.size();
        let width = size.width;
        let height = size.height;
        let unpadded_bpr = width * BYTES_PER_TEXEL;
        let padded_bpr = unpadded_bpr.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyphs test uint readback"),
            size: u64::from(padded_bpr * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("glyphs test uint copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        glyph_field::map_read(
            device,
            &slice,
            crate::config::settings().gpu.wait_limit(),
            "glyphs test uint readback",
        );
        let mapped = slice.get_mapped_range().expect("uint readback mapped");
        let mut out = Vec::with_capacity((width * height * U32S_PER_TEXEL) as usize);
        for row in 0..height {
            let start = (row * padded_bpr) as usize;
            let row_bytes = &mapped[start..start + unpadded_bpr as usize];
            out.extend_from_slice(bytemuck::cast_slice::<u8, u32>(row_bytes));
        }
        drop(mapped);
        readback.unmap();
        out
    }

    fn assert_gpu_matches_atlas(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        glyphs: &GlyphSource,
    ) {
        let gpu_map = readback_uint_texture(device, queue, glyphs.glyphmap_texture());
        let cpu_map = glyphs.atlas().glyphmap_texture();
        assert_eq!(
            gpu_map.len(),
            cpu_map.data.len(),
            "glyphmap GPU length != CPU length"
        );
        assert_eq!(gpu_map, cpu_map.data, "glyphmap GPU texels != FontAtlas");

        let gpu_curves = readback_uint_texture(device, queue, glyphs.curves_texture());
        let cpu_curves = glyphs.atlas().curve_texture();
        assert_eq!(
            gpu_curves.len(),
            cpu_curves.data.len(),
            "curves GPU length != CPU length"
        );
        assert_eq!(gpu_curves, cpu_curves.data, "curves GPU texels != FontAtlas");
    }

    #[test]
    fn gpu_screenful_upload_matches_font_atlas_and_idle_frame_uploads_nothing() {
        let ctx = crate::gpu::test_device::context();
        let baked = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let mut glyphs = GlyphSource::new(&ctx.device, &ctx.queue, Arc::clone(&baked));

        // Default family is Monaspace Neon; Cousine and Argon are selectable with distinct metrics.
        let neon_id = glyphs.family_id(MONASPACE_NEON_FAMILY).expect("neon");
        let cousine_id = glyphs.family_id(DEFAULT_FONT_FAMILY).expect("cousine");
        let argon_id = glyphs.family_id(MONASPACE_ARGON_FAMILY).expect("argon");
        assert_eq!(glyphs.family(), neon_id);
        let neon_metrics = glyphs.cell_metrics();
        let cousine_metrics = glyphs.cell_metrics_for(cousine_id);
        assert_ne!(neon_metrics.cell_width, cousine_metrics.cell_width);
        assert_eq!(cousine_metrics, CellMetrics::from_primary(baked.metrics));

        let mut style = WallStyle::default();
        glyphs.apply_line_metrics(&mut style);
        let neon_lines = glyphs.line_metrics().expect("neon line metrics");
        assert_eq!(style.underline_y, neon_lines.underline_y);
        assert_eq!(style.strike_y, neon_lines.strike_y);
        assert_eq!(style.line_thickness, neon_lines.thickness);

        // Fresh source starts clean (0 uploads).
        assert_eq!(
            glyphs.sync(&ctx.device, &ctx.queue),
            GlyphSyncStats::default()
        );

        // Build a screenful of mixed cells: ASCII, bold, italic, bold-italic, box drawing,
        // accented NFC + composite clusters, single emoji, ZWJ emoji sequence, VS16/VS15, blank.
        let mut screen = Screen::new(16, 3);
        let make = |content: u32, cell_attrs: u16, width: u8| Cell {
            content,
            fg: rgb(0xe0, 0xe0, 0xe0),
            bg: rgb(0x10, 0x14, 0x1c),
            ul: 0,
            attrs: cell_attrs,
            width,
            _pad: 0,
        };
        screen.clusters.push("e\u{0301}".to_owned()); // CLUSTER_BASE + 0 (NFC é)
        screen.clusters.push("x\u{0301}".to_owned()); // CLUSTER_BASE + 1 (composite x́)
        screen.clusters.push("q\u{0308}\u{0304}".to_owned()); // CLUSTER_BASE + 2 (stacked q̈̄)
        screen.clusters.push("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_owned()); // +3 ZWJ
        screen.clusters.push("\u{2764}\u{FE0F}".to_owned()); // +4 VS16 colour heart
        screen.clusters.push("\u{2764}\u{FE0E}".to_owned()); // +5 VS15 text heart

        for (i, ch) in "Hello, World!09@".chars().enumerate() {
            screen.row_mut(0)[i] = make(ch as u32, 0, 1);
        }
        screen.row_mut(1)[0] = make('B' as u32, attrs::BOLD, 1);
        screen.row_mut(1)[1] = make('I' as u32, attrs::ITALIC, 1);
        screen.row_mut(1)[2] = make('Z' as u32, attrs::BOLD | attrs::ITALIC, 1);
        screen.row_mut(1)[3] = make('╭' as u32, 0, 1);
        screen.row_mut(1)[4] = make('─' as u32, attrs::BOLD, 1);
        screen.row_mut(1)[5] = make('╬' as u32, 0, 1);
        screen.row_mut(1)[6] = make('▒' as u32, 0, 1);
        screen.row_mut(1)[7] = make(CLUSTER_BASE, 0, 1);
        screen.row_mut(1)[8] = make(CLUSTER_BASE + 1, attrs::BOLD, 1);
        screen.row_mut(1)[9] = make(CLUSTER_BASE + 2, 0, 1);
        screen.row_mut(1)[10] = make('🚀' as u32, 0, 2);
        screen.row_mut(1)[11] = make(0, 0, 0);
        screen.row_mut(1)[12] = make(CLUSTER_BASE + 3, 0, 2);
        screen.row_mut(1)[13] = make(0, 0, 0);
        screen.row_mut(1)[14] = make(CLUSTER_BASE + 4, 0, 2);
        screen.row_mut(1)[15] = make(0, 0, 0);
        screen.row_mut(2)[0] = make(CLUSTER_BASE + 5, 0, 1);
        screen.row_mut(2)[1] = make(' ' as u32, 0, 1);
        screen.row_mut(2)[2] = Cell::BLANK;

        let mut resolved = Vec::new();
        for r in 0..screen.rows {
            for cell in screen.row(r) {
                resolved.push(glyphs.resolve_cell(&screen, cell));
            }
        }

        // Spot-check resolved modes and spans.
        let r1 = |col: usize| resolved[16 + col];
        assert_eq!(r1(0).span_cols, 1);
        assert_eq!(glyphs.atlas().map_entry(r1(0).slot)[2], GLYPH_MODE_OUTLINE);
        assert_eq!(glyphs.atlas().map_entry(r1(3).slot)[2], GLYPH_MODE_SPRITE);
        assert_eq!(glyphs.atlas().map_entry(r1(6).slot)[2], GLYPH_MODE_SPRITE);
        assert_eq!(glyphs.atlas().map_entry(r1(6).slot)[3], 128);
        assert!(glyphs.atlas().is_composite_slot(r1(8).slot));
        assert_eq!(r1(10).span_cols, 2);
        assert_eq!(glyphs.atlas().map_entry(r1(10).slot), [0, 0, GLYPH_MODE_BITMAP, 958]);
        assert_eq!(r1(11), ResolvedCell { slot: BLANK_SLOT, span_cols: 0 });
        assert_eq!(r1(12).span_cols, 2);
        assert_eq!(glyphs.atlas().map_entry(r1(12).slot)[2], GLYPH_MODE_BITMAP);
        assert_eq!(
            glyphs.atlas().map_entry(r1(12).slot)[3],
            baked.trie.emoji_cell[6819].expect("baked ZWJ family cell")
        );
        assert_eq!(glyphs.atlas().map_entry(r1(14).slot)[2], GLYPH_MODE_BITMAP);
        assert_eq!(glyphs.atlas().map_entry(resolved[32].slot)[2], GLYPH_MODE_OUTLINE);
        assert_eq!(resolved[33], ResolvedCell { slot: BLANK_SLOT, span_cols: 1 });
        assert_eq!(resolved[34], ResolvedCell { slot: BLANK_SLOT, span_cols: 1 });

        // First sync: incremental dirty upload (no growth), GPU readback bit-identical to FontAtlas.
        let s1 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(!s1.rebound);
        assert!(s1.write_calls >= 2);
        assert!(s1.texels_uploaded > 0 && s1.bytes_uploaded == s1.texels_uploaded * 16);
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);

        // Unchanged second frame: uploads nothing.
        let s2 = glyphs.sync(&ctx.device, &ctx.queue);
        assert_eq!(s2, GlyphSyncStats::default());

        // Switch family to Argon, resolve one new glyph, verify incremental upload + readback.
        assert_eq!(glyphs.set_family(MONASPACE_ARGON_FAMILY), Some(argon_id));
        let omega = make('Ω' as u32, attrs::BOLD, 1);
        let r_omega = glyphs.resolve_cell(&screen, &omega);
        assert_ne!(r_omega.slot, BLANK_SLOT);
        let s3 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(!s3.rebound);
        assert!(s3.write_calls >= 2 && s3.texels_uploaded < 512);
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);
    }

    #[test]
    fn gpu_texture_growth_recreates_textures_and_matches_font_atlas() {
        let ctx = crate::gpu::test_device::context();
        let baked = Arc::new(Atlas::load(&crate::default_emoji_sheet()));
        let emoji: Arc<dyn EmojiSource> = Arc::new(AtlasEmojiSource::new(baked));

        // Start at 1 curve row (512 curves) and 1 map row (1024 slots) so both double on GPU.
        let mut glyphs = GlyphSource::load_with_initial_rows(
            &ctx.device,
            &ctx.queue,
            &default_fonts_dir(),
            Some(emoji),
            1,
            1,
        )
        .expect("load small-capacity GlyphSource");
        assert_eq!(glyphs.curves_texture().height(), 1);
        assert_eq!(glyphs.glyphmap_texture().height(), 1);

        let screen = Screen::new(1, 1);
        let cell_of = |cp: u32, cell_attrs: u16| Cell {
            content: cp,
            fg: rgb(0xff, 0xff, 0xff),
            bg: 0,
            ul: 0,
            attrs: cell_attrs,
            width: 1,
            _pad: 0,
        };

        // Initial small batch: no growth yet; verify GPU readback.
        for ch in "AtlasGrowth!".chars() {
            glyphs.resolve_cell(&screen, &cell_of(ch as u32, 0));
        }
        let s1 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(!s1.rebound);
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);

        // Push > 1,100 distinct slots across all 3 families and all 4 faces so BOTH
        // curves and glyphmap textures double past their initial 1-row allocation.
        let families = [
            glyphs.family_id(DEFAULT_FONT_FAMILY).unwrap(),
            glyphs.family_id(MONASPACE_NEON_FAMILY).unwrap(),
            glyphs.family_id(MONASPACE_ARGON_FAMILY).unwrap(),
        ];
        let attr_faces = [0, attrs::BOLD, attrs::ITALIC, attrs::BOLD | attrs::ITALIC];
        for fam in families {
            for &a in &attr_faces {
                for cp in 0x21u32..=0x7Eu32 {
                    glyphs.resolve_cell_in(fam, &screen, &cell_of(cp, a));
                }
                for cp in 0x00A1u32..=0x0120u32 {
                    glyphs.resolve_cell_in(fam, &screen, &cell_of(cp, a));
                }
            }
        }

        let s2 = glyphs.sync(&ctx.device, &ctx.queue);
        assert!(s2.rebound, "growth must flag rebound so caller rebuilds shared_bg");
        assert!(
            glyphs.curves_texture().height() > 1,
            "curves texture must have grown past 1 row (got {})",
            glyphs.curves_texture().height()
        );
        assert!(
            glyphs.glyphmap_texture().height() > 1,
            "glyphmap texture must have grown past 1 row (got {})",
            glyphs.glyphmap_texture().height()
        );
        assert_gpu_matches_atlas(&ctx.device, &ctx.queue, &glyphs);

        // Subsequent unchanged frame uploads nothing.
        assert_eq!(
            glyphs.sync(&ctx.device, &ctx.queue),
            GlyphSyncStats::default()
        );
    }
}
