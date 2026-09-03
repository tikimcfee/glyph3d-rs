# fixture_manifest.mojo — the Mojo half of the stage 0 parse-parity gate.
#
# WHAT THIS ANSWERS: Rust learned to read .pipe.bin in native/src/fixture.rs,
# and there is no second Rust parser for it to disagree with. So the check is
# cross-IMPLEMENTATION: both loaders emit the same canonical line per fixture,
# and tools/check-fixture-parity.sh diffs them.
#
# WHY CHECKSUMS OVER PARSED VALUES, NOT OVER THE FILE. Hashing the bytes on
# disk would agree no matter how wrongly either side parsed them — the file is
# the one thing both are guaranteed to have identical. These hashes are fed the
# bit patterns of the TYPED values AFTER strides, field order, and the carrier
# split have been applied, so getting any of those wrong diverges. FNV-1a is
# order-sensitive, which a sum is not: a section read in the wrong order has to
# show up.
#
# Run: mojo run -I engine --fp-mode contract=off engine/fixture_manifest.mojo \
#          engine/fixtures/*.pipe.bin

from std.sys import argv
from fixture_io import load_pipe_fixture
from glyph_schema import FIXTURE_MEASURE_STRIDE, FIXTURE_COUNT_STRIDE

comptime FNV_OFFSET = UInt64(0xCBF29CE484222325)
comptime FNV_PRIME = UInt64(0x100000001B3)


struct Fnv(Copyable, Movable):
    """FNV-1a 64. The wrapping multiply is the contract — it must agree with
    Rust's `wrapping_mul`, and the parity gate is what proves it does."""

    var h: UInt64

    def __init__(out self):
        self.h = FNV_OFFSET

    def byte(mut self, b: UInt8):
        self.h = (self.h ^ UInt64(b)) * FNV_PRIME

    def u8(mut self, v: UInt8):
        self.byte(v)

    def u32(mut self, v: UInt32):
        for i in range(4):
            self.byte(UInt8((UInt64(v) >> UInt64(i * 8)) & 0xFF))

    def u64(mut self, v: UInt64):
        for i in range(8):
            self.byte(UInt8((v >> UInt64(i * 8)) & 0xFF))

    def i64(mut self, v: Int):
        # Every value hashed through here is non-negative by construction
        # (byte_start/byte_count come from u32, the page params from
        # trunc_nonneg), so the two's-complement question does not arise.
        self.u64(UInt64(v))

    def f32(mut self, v: Float32):
        self.u32(UInt32(v.to_bits()))

    def f64(mut self, v: Float64):
        self.u64(UInt64(v.to_bits()))


def hex16(v: UInt64) -> String:
    """Zero-padded 16 hex digits, built a nibble at a time so the two emitters
    cannot disagree about formatting instead of about content."""
    var digits = String("0123456789abcdef")
    var s = String("")
    for i in range(16):
        var nib = Int((v >> UInt64((15 - i) * 4)) & 0xF)
        s += String(digits[byte = nib : nib + 1])
    return s


def main() raises:
    var args = argv()
    if len(args) < 2:
        print("usage: mojo run -I engine engine/fixture_manifest.mojo <fixture.pipe.bin> ...")
        return

    for a in range(1, len(args)):
        var path = String(args[a])
        var fx = load_pipe_fixture(path)

        var h_bytes = Fnv()
        for i in range(len(fx.bytes)):
            h_bytes.u8(fx.bytes[i])
        var h_tindex = Fnv()
        for i in range(len(fx.trie.block_index)):
            h_tindex.u32(fx.trie.block_index[i])
        var h_tm = Fnv()
        for i in range(len(fx.trie.blocks_m)):
            h_tm.f32(fx.trie.blocks_m[i])
        var h_tc = Fnv()
        for i in range(len(fx.trie.blocks_c)):
            h_tc.u32(fx.trie.blocks_c[i])

        # FIELD ORDER IS THE STRUCT'S, and the Rust side hashes its own struct
        # in the same declaration order. Reordering either silently breaks this
        # gate, which is the intent.
        var h_items = Fnv()
        for i in range(fx.item_count):
            var t = fx.items[i].copy()
            h_items.i64(t.byte_start)
            h_items.i64(t.byte_count)
            h_items.f64(t.origin_x)
            h_items.f64(t.origin_y)
            h_items.f64(t.origin_z)
            h_items.i64(t.wrap_width)
            h_items.f64(t.z_step)
            h_items.f64(t.line_height)
            h_items.u8(UInt8(1) if t.has_page else UInt8(0))
            h_items.i64(t.page_rows)
            h_items.i64(t.page_cols)
            h_items.i64(t.scroll_rows)
            h_items.i64(t.pages_wide)
            h_items.f64(t.page_gap_x)
            h_items.f64(t.band_stride_y)
            h_items.f64(t.depth_per_band)
            h_items.f64(t.depth_per_col)
            h_items.f64(t.page_line_height)

        var h_miss = Fnv()
        for i in range(len(fx.exp_misses)):
            h_miss.u32(fx.exp_misses[i])
        var h_ord = Fnv()
        for i in range(len(fx.exp_ord)):
            h_ord.u32(fx.exp_ord[i])
        var h_meas = Fnv()
        for i in range(len(fx.exp_measures)):
            h_meas.f64(fx.exp_measures[i])
        var h_cnt = Fnv()
        for i in range(len(fx.exp_counts)):
            h_cnt.u32(fx.exp_counts[i])
        var h_bnds = Fnv()
        for i in range(len(fx.exp_item_bounds)):
            h_bnds.u64(fx.exp_item_bounds[i])
        var h_batch = Fnv()
        for i in range(len(fx.exp_batch)):
            h_batch.u64(fx.exp_batch[i])

        var short = path
        var slash = path.rfind("/")
        if slash >= 0:
            short = String(path[byte = slash + 1 :])

        print(
            short,
            "bytes=" + String(fx.byte_len),
            "items=" + String(fx.item_count),
            "tindex=" + String(len(fx.trie.block_index)),
            "tentries=" + String(len(fx.trie.blocks_c) // 2),
            "leaders=" + String(fx.exp_leaders),
            "misses=" + String(len(fx.exp_misses)),
            "h.bytes=" + hex16(h_bytes.h),
            "h.tindex=" + hex16(h_tindex.h),
            "h.tm=" + hex16(h_tm.h),
            "h.tc=" + hex16(h_tc.h),
            "h.items=" + hex16(h_items.h),
            "h.miss=" + hex16(h_miss.h),
            "h.ord=" + hex16(h_ord.h),
            "h.meas=" + hex16(h_meas.h),
            "h.cnt=" + hex16(h_cnt.h),
            "h.bnds=" + hex16(h_bnds.h),
            "h.batch=" + hex16(h_batch.h),
        )
