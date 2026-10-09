//! Slot-buffer copies shaped for the transfer fast path.
//!
//! WHY THIS EXISTS (C16, measured 2026-10-09 on an RTX 5090 / Vulkan,
//! driver 615.78.08): a `copy_buffer_to_buffer` whose SIZE is not a multiple
//! of 16 bytes runs the WHOLE copy at roughly half throughput, not just its
//! tail. wgpu only requires 4 (`COPY_BUFFER_ALIGNMENT`), so nothing complains.
//! A Derived slot is 20 B, so the slot stream's byte length is 16-aligned
//! only when the survivor count is a multiple of 4: an odd count paid ~+10 ms
//! per 1.8 GB copy (a count ≡ 2 mod 4, ~+3 ms). The discrete staging path
//! makes TWO such copies — wgpu-core's own staging → our `mapped_at_creation`
//! buffer at `unmap`, then ours into the VRAM chunk — so the 94 MB tree paid
//! ~+20 ms of backend for one extra glyph. A 32 B `RenderSlot` stream is
//! always 16-aligned and never paid.
//!
//! The fix is shape only, never content: a staging buffer is padded to a
//! multiple of [`FAST_COPY_ALIGN`] (its pad bytes are wgpu's zero-init and are
//! never copied out), and a copy into a destination we do not own the size of
//! is split into a 16-aligned body and a < 16 B tail ([`copy_split`]). Every
//! byte a renderer-visible buffer ends up holding is the byte it held before,
//! and every renderer-visible buffer keeps its size.
//!
//! Not addressed here: a SOURCE OFFSET off 16 also costs (measured: a 4-mod-16
//! offset ~+5 ms over half the stream, an 8-mod-16 one ~+1 ms). The chunked
//! Derived layout puts chunk k at `k × chunk_cap × 20` B, ≡ 8 mod 16 with the
//! 2 GiB binding limit; moving it would move which slot lives in which chunk.

/// The copy size (and staging size) granularity the transfer fast path wants.
pub const FAST_COPY_ALIGN: u64 = 16;

/// A staging buffer's size for `bytes` of payload: rounded up to
/// [`FAST_COPY_ALIGN`], and never zero. Use for buffers nothing but a copy
/// reads — the pad is not part of any slot.
pub fn padded_staging_size(bytes: u64) -> u64 {
    bytes.max(1).next_multiple_of(FAST_COPY_ALIGN)
}

/// `size` split into `(body, tail)`: `body` the largest multiple of
/// [`FAST_COPY_ALIGN`] not above `size`, `tail` the rest (< 16 B, and a
/// multiple of 4 whenever `size` is).
pub fn split_copy_size(size: u64) -> (u64, u64) {
    let body = size - size % FAST_COPY_ALIGN;
    (body, size - body)
}

/// `copy_buffer_to_buffer` of `size` bytes as a 16-aligned body plus, when
/// needed, one small tail copy — the same bytes land at the same addresses.
pub fn copy_split(
    encoder: &mut wgpu::CommandEncoder,
    src: &wgpu::Buffer,
    src_offset: u64,
    dst: &wgpu::Buffer,
    dst_offset: u64,
    size: u64,
) {
    let (body, tail) = split_copy_size(size);
    if body > 0 {
        encoder.copy_buffer_to_buffer(src, src_offset, dst, dst_offset, body);
    }
    if tail > 0 {
        encoder.copy_buffer_to_buffer(src, src_offset + body, dst, dst_offset + body, tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every residue of a 20 B slot count mod 4 (the Derived stream) and of a
    /// 32 B one: the body is fast-path aligned, the tail is a legal wgpu copy,
    /// and nothing is lost or duplicated.
    #[test]
    fn split_body_is_fast_aligned_and_covers_the_size() {
        for slot_bytes in [20u64, 32] {
            for count in [0u64, 1, 2, 3, 4, 5, 91_417_857, 91_417_858, 91_417_859, 107_374_182] {
                let size = count * slot_bytes;
                let (body, tail) = split_copy_size(size);
                assert_eq!(body % FAST_COPY_ALIGN, 0, "{count} x {slot_bytes} B: body {body}");
                assert_eq!(body + tail, size, "{count} x {slot_bytes} B");
                assert!(tail < FAST_COPY_ALIGN, "{count} x {slot_bytes} B: tail {tail}");
                assert_eq!(tail % wgpu::COPY_BUFFER_ALIGNMENT, 0, "{count} x {slot_bytes} B: tail {tail}");
            }
        }
        // The case that was slow: an odd Derived count leaves a 4 or 12 B tail.
        assert_eq!(split_copy_size(91_417_859 * 20), (91_417_859 * 20 - 12, 12));
    }

    /// The staging pad: 16-aligned, never short, never zero, and at most 15 B over.
    #[test]
    fn staging_size_is_padded_not_truncated() {
        for bytes in [0u64, 4, 16, 20, 40, 60, 80, 1_828_357_180] {
            let s = padded_staging_size(bytes);
            assert_eq!(s % FAST_COPY_ALIGN, 0, "{bytes}");
            assert!(s >= bytes.max(1) && s - bytes.max(1) < FAST_COPY_ALIGN, "{bytes} -> {s}");
        }
    }
}
