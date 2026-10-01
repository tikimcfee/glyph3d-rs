//! The GPU wire types: the 48 B glyph instance, the 80 B group row, and the
//! per-frame uniform blocks, each repr(C) mirrors of the WGSL layouts with
//! the encase-derived assertions pinned against bytemuck's bytes in
//! `layout_tests`. Extracted from `glyph_scene.rs` in the 2026-09 code-shape
//! refactor — a pure move; `pub(super)` stands in for the same-module
//! privacy the uniform types had.
//!
//! Since E2a (note 23) the SHADER binds the 32 B `RenderSlot`, not the 48 B
//! `GlyphInstance`: row/col, flags and _pad have no live readers (note 22's
//! sweep — the shader never read them, pick/verbs ride the engine cache,
//! seg_tint wants glyph_id+color only). The FFI/engine paths still produce
//! `GlyphInstance`; staging transcodes field-wise, so the vertex math reads
//! the same values and the goldens stay byte-equal by construction.

use bytemuck::{Pod, Zeroable};

/// Per-instance glyph slot — 48 B, mirrors `InstanceSlot` in glyph_field.wgsl.
/// (Layout rationale is documented in the shader header.)
/// Stage H: encase ShaderType derive — generated WGSL-layout size/offsets,
/// asserted against the bytemuck wire format in `layout_tests` below (the
/// Stage G strided-color bug class, caught at compile/test time).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
pub struct GlyphInstance {
    pub pos: [f32; 3],
    pub glyph_id: u32,
    pub row: u32,
    pub col: u32,
    pub color: u32, // packed RGBA8 (sRGB display values)
    pub group_id: u32,
    pub advance: f32,
    pub height: f32,
    pub flags: u32,
    pub _pad: u32,
}

/// The render-bound slot — 32 B, what the shader's `InstanceSlot` has been
/// since E2a. The CubeCL chain's scatter produces this form on device (the
/// endpoint, note 23); the 48 B engine paths transcode at staging. Field
/// order is the shader's read order minus the dead lanes — every value the
/// vertex math reads is bit-identical to the 48 B form's.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
pub struct RenderSlot {
    pub pos: [f32; 3],
    pub glyph_id: u32,
    pub color: u32, // packed RGBA8 (sRGB display values)
    pub group_id: u32,
    pub advance: f32,
    pub height: f32,
}

impl From<&GlyphInstance> for RenderSlot {
    #[inline(always)]
    fn from(g: &GlyphInstance) -> Self {
        Self {
            pos: g.pos,
            glyph_id: g.glyph_id,
            color: g.color,
            group_id: g.group_id,
            advance: g.advance,
            height: g.height,
        }
    }
}

/// Group table row — 5 vec4s, 80 B, the web's GROUP_STRIDE=5 schema
/// (glyphVertex.js): offset / quat / color+alpha / scale+colorBlend / clip.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
pub struct GroupRow {
    pub cols: [[f32; 4]; 5],
}

impl GroupRow {
    /// Identity pose at `offset`: unit quat, white opaque color, unit scale,
    /// colorBlend 0 (multiply), clip disabled.
    pub fn identity(offset: [f32; 3]) -> Self {
        Self {
            cols: [
                [offset[0], offset[1], offset[2], 0.0],
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 0.0],
            ],
        }
    }

    /// Stage E2: identity pose with a color tint (multiplied with per-instance
    /// colors; colorBlend 0 = multiply, alpha 1).
    pub fn tinted(offset: [f32; 3], rgb: [f32; 3]) -> Self {
        let mut g = Self::identity(offset);
        g.cols[2] = [rgb[0], rgb[1], rgb[2], 1.0];
        g
    }
}

/// Stage L (L1): the frame uniform — was CameraUniform (just view_proj).
/// re_renderer's FrameUniformBuffer borrow: everything a frame needs behind
/// the one reserved binding. `view_proj` stays at offset 0, byte-for-byte
/// the same 64 B; the appended lanes bind to the UNCHANGED WGSL uniform
/// block (glyph_field.wgsl `struct Camera` = 64 B minimum binding size ≤
/// this 104 B buffer — no shader bytes move; the extra lanes are consumed by
/// a future stage that changes the shader anyway). `flags` bit 0 is reserved
/// `deterministic_rendering` (re_renderer's RenderMode::Deterministic idea);
/// 0 everywhere today — nothing consumes it yet.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
pub(super) struct FrameUniform {
    pub(super) view_proj: [f32; 16],
    pub(super) eye: [f32; 3],
    pub(super) _pad0: f32,
    pub(super) viewport: [f32; 2],
    pub(super) px_scale: f32,
    pub(super) time: f32,
    pub(super) flags: u32,
    pub(super) _pad1: u32,
}

/// GlyphField.js GLYPH_LOD_DEFAULTS + group count, one uniform block.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct Params {
    pub(super) max_groups: u32,
    pub(super) _pad0: u32,
    pub(super) _pad1: u32,
    pub(super) _pad2: u32,
    pub(super) dilate_px: f32,
    pub(super) soften: f32,
    pub(super) min_lo: f32,
    pub(super) min_hi: f32,
    /// Emoji sheet geometry, mirrored from the G3ES header (atlas.rs) so the
    /// vertex stage can place a cell from its index alone. WGSL `vec2<u32>` /
    /// `vec2<f32>` are 8-byte aligned; this 32-byte tail keeps the struct at a
    /// 16-byte multiple (64 B).
    pub(super) emoji_cell: [u32; 2],
    pub(super) emoji_cols: u32,
    pub(super) emoji_rows: u32,
    pub(super) emoji_layer: [f32; 2],
    pub(super) _pad3: [u32; 2],
}


// vec4s) are mirrored by hand in the repr(C) structs above — the Stage G
// strided-color bug came from exactly this mirroring. encase's derive computes
// WGSL-layout size/offsets independently; these tests pin the two
// representations against each other AND against bytemuck's raw bytes, so a
// layout edit that disagrees with the shaders fails `cargo test` at compile
// time instead of corrupting a render.
#[cfg(test)]
mod layout_tests {
    use super::*;
    use encase::{ShaderSize, ShaderType};

    #[test]
    fn glyph_instance_size_and_offsets() {
        assert_eq!(<GlyphInstance as ShaderSize>::SHADER_SIZE.get(), 48, "WGSL lane map is 12 x 4 B");
        assert_eq!(std::mem::size_of::<GlyphInstance>(), 48);
        // Lane map from the glyph_field.wgsl header (offset in bytes).
        // (Metadata is by-value; METADATA is a const, so re-instantiate it.)
        let expected = [
            ("pos", 0),
            ("glyph_id", 12),
            ("row", 16),
            ("col", 20),
            ("color", 24),
            ("group_id", 28),
            ("advance", 32),
            ("height", 36),
            ("flags", 40),
            ("_pad", 44),
        ];
        for (i, (name, off)) in expected.iter().enumerate() {
            assert_eq!(GlyphInstance::METADATA.offset(i), *off as u64, "field {name} offset");
        }
    }

    /// The render-bound slot (E2a): 32 B / 8 lanes, mirrors the shader's
    /// `InstanceSlot`. vec3's 16-alignment does not pad the following u32
    /// (roundUp(4, 12) = 12) and the array stride rounds 32 up to 16's
    /// multiple — 32 — so Rust, encase and WGSL agree exactly.
    #[test]
    fn render_slot_size_and_offsets() {
        assert_eq!(<RenderSlot as ShaderSize>::SHADER_SIZE.get(), 32, "WGSL lane map is 8 x 4 B");
        assert_eq!(std::mem::size_of::<RenderSlot>(), 32);
        let expected = [
            ("pos", 0),
            ("glyph_id", 12),
            ("color", 16),
            ("group_id", 20),
            ("advance", 24),
            ("height", 28),
        ];
        for (i, (name, off)) in expected.iter().enumerate() {
            assert_eq!(RenderSlot::METADATA.offset(i), *off as u64, "field {name} offset");
        }
    }

    #[test]
    fn group_row_size_and_offsets() {
        assert_eq!(<GroupRow as ShaderSize>::SHADER_SIZE.get(), 80, "GROUP_STRIDE=5 vec4s");
        assert_eq!(std::mem::size_of::<GroupRow>(), 80);
        assert_eq!(GroupRow::METADATA.offset(0), 0, "cols offset");
    }

    /// Stage L (L1): the widened frame uniform. `view_proj` is pinned at
    /// 0..64 — the unchanged WGSL `Camera` block binds those first 64 B, and
    /// the minimum-binding-size rule lets the larger buffer carry the
    /// appended lanes without a shader edit. NOTE: repr(C) arrays are
    /// align-4 in Rust (no GPU vec alignment), so there is NO tail padding —
    /// Rust and encase agree at 104 B.
    #[test]
    fn frame_uniform_size_and_offsets() {
        assert_eq!(std::mem::size_of::<FrameUniform>(), 104, "repr(C) size (align-4 arrays: no tail pad)");
        let expected = [
            ("view_proj", 0),
            ("eye", 64),
            ("_pad0", 76),
            ("viewport", 80),
            ("px_scale", 88),
            ("time", 92),
            ("flags", 96),
            ("_pad1", 100),
        ];
        for (i, (name, off)) in expected.iter().enumerate() {
            assert_eq!(FrameUniform::METADATA.offset(i), *off as u64, "field {name} offset");
        }
        // Encase uniform-space size agrees with repr(C); both are ≥ the WGSL
        // block's 64 B minimum binding size.
        assert_eq!(<FrameUniform as ShaderSize>::SHADER_SIZE.get(), 104);
    }

    /// Decisive check: encase's serialization must be byte-identical to the
    /// bytemuck wire format for distinctive bit patterns — if this holds, the
    /// write paths can never diverge silently.
    #[test]
    fn encase_bytes_match_bytemuck() {
        let inst = GlyphInstance {
            pos: [1.5, -2.25, 3.75],
            glyph_id: 0xAABBCCDD,
            row: 0x11223344,
            col: 0x55667788,
            color: 0xDEADBEEF,
            group_id: 7,
            advance: 0.529_741_4,
            height: 1.0,
            flags: 0x80000001,
            _pad: 0x42424242,
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&inst).unwrap();
        assert_eq!(buf.len(), 48);
        assert_eq!(&buf[..], bytemuck::bytes_of(&inst), "GlyphInstance bytes");

        let slot = RenderSlot {
            pos: [1.5, -2.25, 3.75],
            glyph_id: 0xAABBCCDD,
            color: 0xDEADBEEF,
            group_id: 7,
            advance: 0.529_741_4,
            height: 1.0,
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&slot).unwrap();
        assert_eq!(buf.len(), 32);
        assert_eq!(&buf[..], bytemuck::bytes_of(&slot), "RenderSlot bytes");

        let row = GroupRow {
            cols: [
                [1.0, 2.0, 3.0, 4.0],
                [0.0, 0.0, 0.0, 1.0],
                [0.25, 0.5, 0.75, 1.0],
                [2.0, 2.0, 2.0, 0.0],
                [-1.0, -2.0, 1e10, f32::MIN_POSITIVE],
            ],
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&row).unwrap();
        assert_eq!(buf.len(), 80);
        assert_eq!(&buf[..], bytemuck::bytes_of(&row), "GroupRow bytes");
    }
}
