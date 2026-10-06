//! The records every field mode shares: the engine's neutral 48 B glyph
//! record, the 80 B group-table row, and the mode-neutral edit record.
//!
//! Moved here from `native/src/glyph_scene/instance.rs` when the field split
//! into modes (2026-10); native re-exports both types under their old paths,
//! so every producer keeps compiling unchanged. The encase-vs-bytemuck layout
//! pins moved with them.

use bytemuck::{Pod, Zeroable};

/// The engine's glyph record — 48 B, the neutral form every CPU producer
/// (HyperLayout's host path, `text::stage_file`, the FFI-era record paths)
/// emits. No mode binds it directly: each mode converts it into its own slot
/// format at upload (Instanced: field-wise into the 32 B `RenderSlot`, so the
/// vertex math reads the same values).
///
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

/// Group table row — 6 vec4s, 96 B, extended from the web's GROUP_STRIDE=5 schema
/// (glyphVertex.js): offset / quat / color+alpha / scale+colorBlend / clip / bg_color.
/// Every mode's shader binds the same table (binding 2), so a group verb is
/// mode-neutral by construction.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, encase::ShaderType)]
pub struct GroupRow {
    pub cols: [[f32; 4]; 6],
}

impl GroupRow {
    /// Identity pose at `offset`: unit quat, white opaque color, unit scale,
    /// colorBlend 0 (multiply), clip disabled, transparent background.
    pub fn identity(offset: [f32; 3]) -> Self {
        Self {
            cols: [
                [offset[0], offset[1], offset[2], 0.0],
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 0.0, 0.0], // bg_color
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

/// One glyph's full placement, mode-neutral — what an edit verb writes.
///
/// Every mode must accept it for any slot (the addressability invariant): a
/// mode that stores less than this per glyph is responsible for making the
/// write take effect anyway (e.g. by promoting the glyph to an override).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphPlacement {
    /// Glyph-local position (the group's TRS applies on top).
    pub position: [f32; 3],
    pub glyph_id: u32,
    /// Packed RGBA8 (sRGB display values).
    pub color: u32,
    pub group_id: u32,
    pub advance: f32,
    pub height: f32,
}

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

    #[test]
    fn group_row_size_and_offsets() {
        assert_eq!(<GroupRow as ShaderSize>::SHADER_SIZE.get(), 96, "GROUP_STRIDE=6 vec4s");
        assert_eq!(std::mem::size_of::<GroupRow>(), 96);
        assert_eq!(GroupRow::METADATA.offset(0), 0, "cols offset");
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

        let row = GroupRow {
            cols: [
                [1.0, 2.0, 3.0, 4.0],
                [0.0, 0.0, 0.0, 1.0],
                [0.25, 0.5, 0.75, 1.0],
                [2.0, 2.0, 2.0, 0.0],
                [-1.0, -2.0, 1e10, f32::MIN_POSITIVE],
                [0.1, 0.2, 0.3, 0.4],
            ],
        };
        let mut buf = Vec::<u8>::new();
        encase::StorageBuffer::new(&mut buf).write(&row).unwrap();
        assert_eq!(buf.len(), 96);
        assert_eq!(&buf[..], bytemuck::bytes_of(&row), "GroupRow bytes");
    }
}
