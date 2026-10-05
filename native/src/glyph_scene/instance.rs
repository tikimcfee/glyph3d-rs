//! The GPU wire types: the per-frame uniform blocks, repr(C) mirrors of the
//! WGSL layouts with the encase-derived assertions pinned against bytemuck's
//! bytes in `layout_tests`. Extracted from `glyph_scene.rs` in the 2026-09
//! code-shape refactor — a pure move; `pub(super)` stands in for the
//! same-module privacy the uniform types had.
//!
//! Since the 2026-10 field-mode split the glyph RECORDS live in their crates
//! and are re-exported here under their old paths, so every producer keeps
//! compiling unchanged: the 48 B `GlyphInstance` and 80 B `GroupRow` in the
//! mode-neutral contract (`glyph-field`), the 32 B `RenderSlot` in the
//! Instanced mode (`glyph-field-instanced`), each with its layout pins.
//!
//! Since E2a (note 23) the SHADER binds the 32 B `RenderSlot`, not the 48 B
//! `GlyphInstance`: row/col, flags and _pad have no live readers (note 22's
//! sweep — the shader never read them, pick/verbs ride the engine cache,
//! seg_tint wants glyph_id+color only). The FFI/engine paths still produce
//! `GlyphInstance`; staging transcodes field-wise, so the vertex math reads
//! the same values and the goldens stay byte-equal by construction.

use bytemuck::{Pod, Zeroable};

pub use glyph_field::{GlyphInstance, GroupRow};
pub use glyph_field_instanced::RenderSlot;

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
    pub(super) greek_mode: u32, // 0 = disabled, 1 = smooth blend (default), 2 = pure hard bypass
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
    pub(super) greek_onset_px: f32,
    pub(super) _pad3: u32,
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

    #[test]
    fn params_size_and_alignment() {
        assert_eq!(std::mem::size_of::<Params>(), 64, "Params must be 64 B (16-byte multiple)");
        assert_eq!(std::mem::align_of::<Params>(), 4);
    }
}
