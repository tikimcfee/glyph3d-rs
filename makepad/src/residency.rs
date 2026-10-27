//! Which glyph blocks are resident on the device (2026-10-26): the policy
//! `G3Renderer` applies, with no makepad context, so it is tested alone.
//!
//! Each frame names the blocks it needs (the cull draws them, or the
//! selection covers them). The held ones stay; the missing ones are
//! published in arena order, at most `upload_per_frame` bytes a frame (the
//! native frame builder uploads a new publication in the frame that first
//! draws it, with no budget of its own); room under the envelope is made by
//! evicting held blocks the frame does not need, least recently needed
//! first. A needed block that finds no room is left out and counted.

/// What the host lets the glyph blocks hold of the device: `envelope`
/// bytes resident at once, `upload_per_frame` bytes published a frame.
#[derive(Clone, Copy, Debug)]
pub struct G3Residency {
    pub envelope: usize,
    pub upload_per_frame: usize,
}

impl Default for G3Residency {
    /// No envelope: every block the frame needs is published, at most
    /// 256 MiB of them a frame.
    fn default() -> Self {
        Self { envelope: usize::MAX, upload_per_frame: 256 << 20 }
    }
}

/// One frame's decision: the blocks to evict (in order) and then to
/// publish, and how many needed blocks found no room.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResidencyPlan {
    pub evict: Vec<usize>,
    pub publish: Vec<usize>,
    pub unfit: usize,
}

/// The resident set: per block index, its bytes and the last frame that
/// needed it, while it is resident.
#[derive(Debug, Default)]
pub struct BlockResidency {
    pub residency: G3Residency,
    held: Vec<Option<(usize, u64)>>,
    bytes: usize,
    frame: u64,
}

impl BlockResidency {
    /// Resident blocks and their bytes.
    pub fn resident(&self) -> (usize, usize) {
        (self.held.iter().flatten().count(), self.bytes)
    }

    /// Plan a frame over `block_count` blocks of `block_bytes(k)` bytes
    /// that needs `needed` (ascending), and take the plan as done. Blocks
    /// past `block_count` (the scene shrank) are evicted first.
    pub fn plan(&mut self, block_count: usize, block_bytes: impl Fn(usize) -> usize, needed: &[usize]) -> ResidencyPlan {
        self.frame += 1;
        let frame = self.frame;
        let mut plan = ResidencyPlan::default();
        for (k, gone) in self.held.iter().enumerate().skip(block_count) {
            if let Some((bytes, _)) = gone {
                self.bytes -= bytes;
                plan.evict.push(k);
            }
        }
        self.held.resize(block_count, None);
        // The held blocks first: all of them are this frame's before any
        // eviction looks for one that is not.
        let mut missing = Vec::new();
        for &k in needed {
            match &mut self.held[k] {
                Some((_, last)) => *last = frame,
                None => missing.push(k),
            }
        }
        let mut uploaded = 0;
        for k in missing {
            let bytes = block_bytes(k);
            if uploaded > 0 && uploaded + bytes > self.residency.upload_per_frame {
                continue;
            }
            while self.bytes + bytes > self.residency.envelope {
                let victim = self
                    .held
                    .iter()
                    .enumerate()
                    .filter_map(|(j, h)| h.filter(|&(_, last)| last != frame).map(|(_, last)| (last, j)))
                    .min();
                let Some((_, j)) = victim else { break };
                let (gone, _) = self.held[j].take().expect("the victim is resident");
                self.bytes -= gone;
                plan.evict.push(j);
            }
            if self.bytes + bytes > self.residency.envelope {
                plan.unfit += 1;
                continue;
            }
            self.held[k] = Some((bytes, frame));
            self.bytes += bytes;
            uploaded += bytes;
            plan.publish.push(k);
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 100;

    fn residency(envelope_blocks: usize, per_frame_blocks: usize) -> BlockResidency {
        BlockResidency {
            residency: G3Residency { envelope: envelope_blocks * BLOCK, upload_per_frame: per_frame_blocks * BLOCK },
            ..BlockResidency::default()
        }
    }

    #[test]
    fn needed_blocks_publish_in_order_within_the_frame_budget() {
        let mut r = residency(100, 2);
        let plan = r.plan(10, |_| BLOCK, &[1, 3, 4, 7]);
        assert_eq!(plan, ResidencyPlan { evict: vec![], publish: vec![1, 3], unfit: 0 });
        let plan = r.plan(10, |_| BLOCK, &[1, 3, 4, 7]);
        assert_eq!(plan.publish, [4, 7], "the rest next frame; the held ones are not published again");
        assert_eq!(r.plan(10, |_| BLOCK, &[1, 3, 4, 7]), ResidencyPlan::default(), "a frame with nothing new does nothing");
        assert_eq!(r.resident(), (4, 4 * BLOCK));
        let mut first = residency(100, 1);
        assert_eq!(first.plan(2, |_| 5 * BLOCK, &[0, 1]).publish, [0], "one block past the budget still goes up alone");
    }

    #[test]
    fn room_is_made_from_the_least_recently_needed_and_never_from_this_frame() {
        let mut r = residency(3, 10);
        r.plan(10, |_| BLOCK, &[0]);
        r.plan(10, |_| BLOCK, &[1]);
        r.plan(10, |_| BLOCK, &[2]);
        r.plan(10, |_| BLOCK, &[0]); // 0 is now more recent than 1 and 2
        let plan = r.plan(10, |_| BLOCK, &[5, 6]);
        assert_eq!(plan.evict, [1, 2], "least recently needed first: 0 was needed after them");
        assert_eq!(plan.publish, [5, 6]);
        // A held block needed this frame is never the victim, even when it
        // comes after the missing ones in arena order.
        let plan = r.plan(10, |_| BLOCK, &[3, 6]);
        assert_eq!((plan.evict, plan.publish), (vec![0], vec![3]));
        let resident: Vec<usize> = (0..10).filter(|&k| r.held[k].is_some()).collect();
        assert_eq!(resident, [3, 5, 6]);
    }

    #[test]
    fn a_view_past_the_envelope_draws_what_fits_and_counts_the_rest() {
        let mut r = residency(2, 10);
        let plan = r.plan(10, |_| BLOCK, &[0, 1, 2, 3]);
        assert_eq!(plan, ResidencyPlan { evict: vec![], publish: vec![0, 1], unfit: 2 });
        assert_eq!(r.plan(10, |_| BLOCK, &[0, 1, 2, 3]).unfit, 2, "nothing held is evictable while the frame needs it");
    }

    #[test]
    fn a_shrunk_scene_evicts_the_blocks_past_its_end() {
        let mut r = residency(10, 10);
        r.plan(6, |_| BLOCK, &[1, 4, 5]);
        let plan = r.plan(4, |_| BLOCK, &[1]);
        assert_eq!(plan.evict, [4, 5]);
        assert_eq!(r.resident(), (1, BLOCK));
    }
}
