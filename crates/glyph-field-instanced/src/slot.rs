//! The Instanced mode's slot record.

use bytemuck::{Pod, Zeroable};
use glyph_field::{GlyphInstance, GlyphPlacement};

/// The render-bound slot — 32 B, what the shader's `InstanceSlot` has been
/// since E2a. HyperLayout's device Pass 2 emits this form directly; the
/// 48 B engine paths transcode at staging. Field
/// order is the shader's read order minus the dead lanes — every value the
/// vertex math reads is bit-identical to the 48 B form's.
///
/// Since E2a (note 23) the SHADER binds this, not the 48 B `GlyphInstance`:
/// row/col, flags and _pad have no live readers (note 22's sweep — the
/// shader never read them, pick/verbs ride the engine cache, seg_tint wants
/// glyph_id+color only).
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

/// Byte stride of one slot.
pub const SLOT_BYTES: u64 = std::mem::size_of::<RenderSlot>() as u64;
/// Byte offset of `pos` (12 B) within a slot.
pub const POSITION_OFFSET: u64 = 0;
/// Byte offset of `color` (4 B) within a slot.
pub const COLOR_OFFSET: u64 = 16;
/// Byte offset of `group_id` (4 B) within a slot.
pub const GROUP_ID_OFFSET: u64 = 20;
/// Byte offset of `advance` (4 B, with `height` immediately after) within a slot.
pub const EXTENT_OFFSET: u64 = 24;

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

impl From<&GlyphPlacement> for RenderSlot {
    #[inline(always)]
    fn from(p: &GlyphPlacement) -> Self {
        Self {
            pos: p.position,
            glyph_id: p.glyph_id,
            color: p.color,
            group_id: p.group_id,
            advance: p.advance,
            height: p.height,
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use encase::{ShaderSize, ShaderType};

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
        // The verb offsets are the lane map's, not hand-kept copies of it.
        assert_eq!(RenderSlot::METADATA.offset(0), POSITION_OFFSET);
        assert_eq!(RenderSlot::METADATA.offset(2), COLOR_OFFSET);
        assert_eq!(RenderSlot::METADATA.offset(4), EXTENT_OFFSET);
        assert_eq!(RenderSlot::METADATA.offset(5), EXTENT_OFFSET + 4, "height follows advance");
    }

    /// encase's serialization must be byte-identical to the bytemuck wire
    /// format for distinctive bit patterns.
    #[test]
    fn encase_bytes_match_bytemuck() {
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
    }

    /// The 48 B → 32 B transcode keeps every lane the vertex stage reads.
    #[test]
    fn from_instance_keeps_the_read_lanes() {
        let instance = GlyphInstance {
            pos: [1.0, 2.0, 3.0],
            glyph_id: 9,
            row: 4,
            col: 5,
            color: 0x1122_3344,
            group_id: 6,
            advance: 0.5,
            height: 1.0,
            flags: 7,
            _pad: 8,
        };
        let slot = RenderSlot::from(&instance);
        assert_eq!(slot.pos, instance.pos);
        assert_eq!(slot.glyph_id, instance.glyph_id);
        assert_eq!(slot.color, instance.color);
        assert_eq!(slot.group_id, instance.group_id);
        assert_eq!(slot.advance.to_bits(), instance.advance.to_bits());
        assert_eq!(slot.height.to_bits(), instance.height.to_bits());
    }
}
