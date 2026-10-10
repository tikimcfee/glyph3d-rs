//! The Slug atlas on the device, built as `native/src/atlas.rs` builds it:
//! curves and glyph map as 1024-wide `Rgba32Uint` textures, the per-slot
//! advance table, the colour-emoji sheet as an `Rgba8UnormSrgb` 2D array with
//! box-filtered mips (decoded through the native crate's own `EmojiSheet`
//! parser and filter, so the texels are the renderer's), and the `Params`
//! uniform with the renderer's defaults (`glyph_scene/setup.rs`).

use std::path::Path;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::gpu::Gpu;
use crate::trie::Trie;

pub const ATLAS_TEX_WIDTH: u32 = 1024;
/// The web's slot count; every appended slot (emoji, sequences) is a bitmap at 2 cells (atlas.rs).
const WEB_SLOTS: usize = 4431;

/// The shader's `Params` block (glyph_field_derived.wgsl), 64 B.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Params {
    pub max_groups: u32,
    pub greek_mode: u32,
    pub _pad1: u32,
    pub _pad2: u32,
    pub dilate_px: f32,
    pub soften: f32,
    pub min_lo: f32,
    pub min_hi: f32,
    pub emoji_cell: [u32; 2],
    pub emoji_cols: u32,
    pub emoji_rows: u32,
    pub emoji_layer: [f32; 2],
    pub greek_onset_px: f32,
    pub _pad3: u32,
}

/// The emoji sheet's geometry the params block carries (G3ES header).
#[derive(Clone, Copy, Debug)]
pub struct SheetGeometry { pub cell: [u32; 2], pub cols: u32, pub rows: u32, pub layer: [f32; 2] }

impl Params {
    /// `glyph_scene/setup.rs`: GLYPH_LOD_DEFAULTS, greek_mode 2 (hard bypass), onset 10 px.
    pub fn renderer_defaults(max_groups: u32, g: SheetGeometry) -> Params {
        Params {
            max_groups, greek_mode: 2, _pad1: 0, _pad2: 0,
            dilate_px: 0.75, soften: 0.45, min_lo: 0.06, min_hi: 0.20,
            emoji_cell: g.cell, emoji_cols: g.cols, emoji_rows: g.rows, emoji_layer: g.layer,
            greek_onset_px: 10.0, _pad3: 0,
        }
    }
}

pub struct EmojiUpload { pub texture: wgpu::Texture, pub geometry: SheetGeometry, pub texture_bytes: u64, pub cells: usize, pub load_ms: f64 }

pub struct Atlas {
    pub curves: wgpu::Texture,
    pub glyphmap: wgpu::Texture,
    pub glyph_advances: wgpu::Buffer,
    pub emoji: Option<EmojiUpload>,
    /// A 1-texel stand-in bound in place of the sheet for the no-sheet timing.
    pub emoji_dummy: wgpu::Texture,
    pub sampler: wgpu::Sampler,
    pub curve_count: u32,
    pub curves_bytes: u64,
    pub glyphmap_bytes: u64,
}

fn read_words(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn upload_uint_texture(gpu: &Gpu, label: &str, width: u32, height: u32, payload: &[u32]) -> wgpu::Texture {
    assert_eq!(payload.len(), (width * height * 4) as usize, "{label}: payload size");
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Uint,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        bytemuck::cast_slice(payload),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * 16), rows_per_image: Some(height) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    texture
}

impl Atlas {
    pub fn load(gpu: &Gpu, dir: &Path, trie: &Trie, emoji_sheet: Option<&Path>) -> Atlas {
        let cv = read_words(&dir.join("curves.bin"));
        assert_eq!(cv[0], u32::from_le_bytes(*b"G3CV"), "curves.bin: bad magic");
        let gm = read_words(&dir.join("glyphmap.bin"));
        assert_eq!(gm[0], u32::from_le_bytes(*b"G3GM"), "glyphmap.bin: bad magic");
        let (cv_w, cv_h, curve_count) = (cv[3], cv[4], cv[5]);
        let (gm_w, gm_h, entry_count) = (gm[3], gm[4], gm[5]);
        assert_eq!(cv_w, ATLAS_TEX_WIDTH);
        assert_eq!(gm_w, ATLAS_TEX_WIDTH);
        assert_eq!(entry_count, trie.slot_count, "glyphmap/glyphs slot count mismatch");
        let curves = upload_uint_texture(gpu, "slug curves", cv_w, cv_h, &cv[8..]);
        let glyphmap = upload_uint_texture(gpu, "slug glyphmap", gm_w, gm_h, &gm[8..]);

        // atlas.rs `load_from_device`: a bitmap slot (a cell, or any appended slot) advances two cells.
        let bitmap_adv = crate::trie::fu_to_world(trie.bitmap_advance_fu as i32, trie.em_height_fu);
        let slot_advances: Vec<f32> = (0..trie.slot_count as usize)
            .map(|s| if trie.emoji_cell[s].is_some() || s >= WEB_SLOTS { bitmap_adv } else { trie.cell_adv })
            .collect();
        let glyph_advances = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph advances"), contents: bytemuck::cast_slice(&slot_advances), usage: wgpu::BufferUsages::STORAGE,
        });

        let emoji = emoji_sheet.map(|p| upload_emoji(gpu, p));
        let emoji_dummy = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("emoji dummy"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("emoji sampler"),
            mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Atlas {
            curves, glyphmap, glyph_advances, emoji, emoji_dummy, sampler, curve_count,
            curves_bytes: (cv_w * cv_h * 16) as u64, glyphmap_bytes: (gm_w * gm_h * 16) as u64,
        }
    }

    /// The geometry the params block needs; a 1×1 single-cell stand-in without a sheet.
    pub fn geometry(&self) -> SheetGeometry {
        self.emoji.as_ref().map(|e| e.geometry).unwrap_or(SheetGeometry { cell: [1, 1], cols: 1, rows: 1, layer: [1.0, 1.0] })
    }

    pub fn emoji_view(&self, real: bool) -> wgpu::TextureView {
        let tex = match (&self.emoji, real) {
            (Some(e), true) => &e.texture,
            _ => &self.emoji_dummy,
        };
        tex.create_view(&wgpu::TextureViewDescriptor { label: Some("emoji view"), dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() })
    }
}

/// `atlas.rs` `EmojiTexture::load_device`, without its on-disk mip cache
/// (which lives beside the sheet in `assets/`; this crate writes nothing there).
/// Layers and their size come from the G3ES header, as the renderer reads
/// them — the generator already keeps a layer within every backend's 2D limit.
fn upload_emoji(gpu: &Gpu, path: &Path) -> EmojiUpload {
    use glyph3d_native::atlas::{box_down_straight, mip_levels_for, EmojiSheet};
    let t = std::time::Instant::now();
    let sheet = EmojiSheet::load(path);
    let mip_levels = mip_levels_for(sheet.cell_w, sheet.cell_h);
    let (level0, _ink) = sheet.decode_layers();
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("emoji sheet"),
        size: wgpu::Extent3d { width: sheet.layer_w, height: sheet.layer_h, depth_or_array_layers: sheet.layers },
        mip_level_count: mip_levels, sample_count: 1, dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut texture_bytes = 0u64;
    for (layer, base) in level0.into_iter().enumerate() {
        let (mut w, mut h) = (sheet.layer_w, sheet.layer_h);
        let mut data = base;
        for level in 0..mip_levels {
            if level > 0 {
                data = box_down_straight(&data, w, h);
                w /= 2;
                h /= 2;
            }
            assert!((w * 4) % 256 == 0, "emoji mip {level}: pitch {} not 256-aligned", w * 4);
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: level, origin: wgpu::Origin3d { x: 0, y: 0, z: layer as u32 }, aspect: wgpu::TextureAspect::All },
                &data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            texture_bytes += (w * h * 4) as u64;
        }
    }
    gpu.queue.submit([]);
    gpu.wait();
    EmojiUpload {
        texture,
        geometry: SheetGeometry { cell: [sheet.cell_w, sheet.cell_h], cols: sheet.cols, rows: sheet.rows_per_layer, layer: [sheet.layer_w as f32, sheet.layer_h as f32] },
        texture_bytes,
        cells: sheet.cells.len(),
        load_ms: t.elapsed().as_secs_f64() * 1e3,
    }
}
