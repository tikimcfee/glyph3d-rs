//! The `GlyphField` trait: everything the scene does to the per-glyph
//! storage, phrased without its format.

use std::ops::Range;

use crate::{GlyphFieldMode, GlyphPlacement};

/// A built glyph field: per-glyph slot storage in chunks, the pipeline that
/// draws it, and the verbs that edit single glyphs.
///
/// Slots are addressed by a global index `0..glyph_count()`. Storage is split
/// into chunks of `chunk_capacity()` slots (a repo-scale field exceeds one
/// storage binding), so a draw names `(chunk, chunk-local range)` and slot `s`
/// lives at `(s / chunk_capacity, s % chunk_capacity)`. A mode never reorders
/// slots — culling, picking and styling all key on the slot index.
pub trait GlyphField {
    /// Which implementation this is.
    fn mode(&self) -> GlyphFieldMode;

    /// Total glyph slots.
    fn glyph_count(&self) -> u32;

    /// Slots per chunk (uniform; the last chunk may hold fewer).
    fn chunk_capacity(&self) -> u32;

    /// Bytes of slot storage per glyph (the mode's record size; diagnostics
    /// and upload accounting).
    fn slot_bytes(&self) -> u32;

    /// Live slots in each chunk, in chunk order.
    fn chunk_glyph_counts(&self) -> &[u32];

    /// Number of chunks.
    fn chunk_count(&self) -> u32 {
        self.chunk_glyph_counts().len() as u32
    }

    /// The main glyph pipeline (the pooled color target, depth-tested and
    /// depth-writing).
    fn glyph_pipeline(&self) -> &wgpu::RenderPipeline;

    /// A selection-mask pipeline over the same shader and bind groups: renders
    /// glyph coverage into `mask_format`, no blend, no depth.
    fn create_mask_pipeline(
        &self,
        device: &wgpu::Device,
        mask_format: wgpu::TextureFormat,
        sample_count: u32,
    ) -> wgpu::RenderPipeline;

    /// Record draws for `(chunk, chunk-local slot range)` pairs into a pass
    /// whose pipeline the CALLER has set (the glyph pipeline or a mask
    /// pipeline from [`GlyphField::create_mask_pipeline`]). Sets the field's
    /// own index/bind state; re-binds a chunk only when it changes, so a
    /// chunk-major list binds each chunk once.
    fn record_draws(&self, pass: &mut wgpu::RenderPass<'_>, draws: &[(u32, Range<u32>)]);

    // ── single-glyph verbs ─────────────────────────────────────────────

    /// Set one glyph's foreground color (packed RGBA8, sRGB display values).
    fn write_color(&self, queue: &wgpu::Queue, slot: u32, rgba: u32);

    /// Move one glyph (glyph-local position; the group TRS applies on top).
    fn write_position(&self, queue: &wgpu::Queue, slot: u32, position: [f32; 3]);

    /// Resize one glyph's cell.
    fn write_extent(&self, queue: &wgpu::Queue, slot: u32, advance: f32, height: f32);

    /// Change the group ID for one glyph.
    fn write_group_id(&self, queue: &wgpu::Queue, slot: u32, group_id: u32);

    // ── bulk verbs ─────────────────────────────────────────────────────

    /// Rewrite the full placement of the contiguous slots
    /// `first_slot .. first_slot + placements.len()`. The field splits the run
    /// at chunk boundaries itself.
    fn write_placements(&self, queue: &wgpu::Queue, first_slot: u32, placements: &[GlyphPlacement]);

    /// Set the colors of the contiguous slots starting at `first_slot`
    /// (clamped to the field). May bypass the queue when storage is mapped.
    fn write_colors(&self, queue: &wgpu::Queue, first_slot: u32, colors: &[u32]);

    // ── diagnostics ────────────────────────────────────────────────────

    /// Read back `out.len()` raw 32-bit words of slot storage starting at
    /// `slot`'s first byte, in the mode's own format (GLYPH_G_DUMP).
    fn read_slot_words(&self, device: &wgpu::Device, queue: &wgpu::Queue, slot: u32, out: &mut [u32]);
}

/// Split the global slot run `first_slot .. first_slot + count` into
/// per-chunk pieces: `(chunk, chunk-local slot range, offset of the piece
/// within the run)`. Pieces come out in ascending slot order, one per chunk
/// the run touches.
pub fn split_at_chunks(
    first_slot: u32,
    count: u32,
    chunk_capacity: u32,
) -> impl Iterator<Item = (u32, Range<u32>, u32)> {
    let end = first_slot + count;
    let mut cursor = first_slot;
    std::iter::from_fn(move || {
        if cursor >= end {
            return None;
        }
        let chunk = cursor / chunk_capacity;
        let chunk_start = chunk * chunk_capacity;
        let piece_end = end.min(chunk_start + chunk_capacity);
        let piece = (chunk, (cursor - chunk_start)..(piece_end - chunk_start), cursor - first_slot);
        cursor = piece_end;
        Some(piece)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_inside_one_chunk() {
        let pieces: Vec<_> = split_at_chunks(5, 3, 10).collect();
        assert_eq!(pieces, [(0, 5..8, 0)]);
    }

    #[test]
    fn split_straddles_chunks() {
        let pieces: Vec<_> = split_at_chunks(8, 15, 10).collect();
        assert_eq!(pieces, [(0, 8..10, 0), (1, 0..10, 2), (2, 0..3, 12)]);
    }

    #[test]
    fn split_empty_run() {
        assert_eq!(split_at_chunks(4, 0, 10).count(), 0);
    }
}
