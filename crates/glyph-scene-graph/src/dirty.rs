//! A two-level dirty set: one bit per row plus one summary bit per 64-row
//! word, so a frame that changed a few rows of a 100k table walks a few
//! words, and the rows come out in ascending order — which is exactly what
//! coalescing them into upload runs needs.
//!
//! The summary/bits split is bevy's (`AtomicSparseBufferVec`,
//! `bevy_render/src/render_resource/sparse_buffer_vec.rs`, bevy 0.16+,
//! MIT OR Apache-2.0). bevy's bits are atomics because many ECS systems mark
//! rows from many threads; ours are written by the one thread that owns the
//! tables, so they are plain words.

const BITS: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct DirtyBits {
    bits: Vec<u64>,
    summary: Vec<u64>,
    count: usize,
}

impl DirtyBits {
    /// Make room for rows `0..len` (never shrinks).
    pub fn grow(&mut self, len: usize) {
        let words = len.div_ceil(BITS);
        if words > self.bits.len() {
            self.bits.resize(words, 0);
            self.summary.resize(words.div_ceil(BITS), 0);
        }
    }

    /// Mark `row`; returns whether it was clean before.
    pub fn set(&mut self, row: u32) -> bool {
        let row = row as usize;
        self.grow(row + 1);
        let (w, b) = (row / BITS, row % BITS);
        let was = self.bits[w] & (1 << b) != 0;
        if !was {
            self.bits[w] |= 1 << b;
            self.summary[w / BITS] |= 1 << (w % BITS);
            self.count += 1;
        }
        !was
    }

    pub fn contains(&self, row: u32) -> bool {
        let row = row as usize;
        self.bits.get(row / BITS).is_some_and(|w| w & (1 << (row % BITS)) != 0)
    }

    /// How many rows are marked.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The marked rows, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.summary.iter().enumerate().flat_map(move |(si, &s)| {
            BitIter(s).flat_map(move |sb| {
                let w = si * BITS + sb as usize;
                BitIter(self.bits[w]).map(move |b| (w * BITS) as u32 + b)
            })
        })
    }

    /// Clear every mark, touching only the words that hold one.
    pub fn clear(&mut self) {
        for (si, s) in self.summary.iter_mut().enumerate() {
            for sb in BitIter(*s) {
                self.bits[si * BITS + sb as usize] = 0;
            }
            *s = 0;
        }
        self.count = 0;
    }
}

/// The set bits of a word, lowest first.
struct BitIter(u64);

impl Iterator for BitIter {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.0 == 0 {
            return None;
        }
        let b = self.0.trailing_zeros();
        self.0 &= self.0 - 1;
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_come_back_ascending_once_each_and_clear_leaves_nothing() {
        let mut d = DirtyBits::default();
        let rows = [70_000u32, 3, 64, 63, 4095, 4096, 3, 0, 70_000];
        let mut fresh = 0;
        for r in rows {
            fresh += d.set(r) as usize;
        }
        assert_eq!(fresh, 7);
        assert_eq!(d.len(), 7);
        assert_eq!(d.iter().collect::<Vec<_>>(), [0, 3, 63, 64, 4095, 4096, 70_000]);
        assert!(d.contains(4096) && !d.contains(4097));
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.iter().count(), 0);
        assert!(!d.contains(70_000));
        assert!(d.set(70_000), "a cleared row is clean again");
    }
}
