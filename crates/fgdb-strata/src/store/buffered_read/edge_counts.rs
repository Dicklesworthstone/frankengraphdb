//! A bounded counting Bloom filter for a second pass over the same authenticated
//! blocks. A counter of one proves a recorded EId occurs once. A collision can
//! only send another EId through full history validation.

use super::{BufferedReadCx, BufferedReadError, EId, MemoryError, MemoryPool};
use crate::tiered::memory::TrackedBytes;

const LANES: usize = 4;
pub(super) const MIN_BYTES: usize = LANES * 8;

pub(super) struct EdgeCounts {
    counters: TrackedBytes,
    lane_bytes: usize,
}

impl EdgeCounts {
    /// Four equal, power-of-two lanes, with a minimum of eight bytes each.
    /// Round down so the caller's byte allowance remains a hard ceiling.
    pub(super) fn new(
        cx: &impl BufferedReadCx,
        pool: &MemoryPool,
        bytes: usize,
    ) -> Result<Self, BufferedReadError> {
        if bytes < MIN_BYTES {
            return Err(MemoryError::InvalidLimits.into());
        }
        let table_bytes = 1usize << bytes.ilog2();
        let counters = pool.allocate_zeroed(cx, table_bytes)?;
        if counters.charged_bytes() > bytes {
            return Err(BufferedReadError::Limit {
                resource: "edge history counter capacity",
                requested: counters.charged_bytes(),
                limit: bytes,
            });
        }
        Ok(Self {
            counters,
            lane_bytes: table_bytes / LANES,
        })
    }

    fn locations(&self, eid: EId) -> [(usize, u32); LANES] {
        // Mix both halves before masking. Separate salts, odd multipliers and
        // rotations keep sequential ordinals and repeated high prefixes from
        // selecting the same bucket across all four lanes.
        const SALTS: [u64; LANES] = [
            0x243f_6a88_85a3_08d3,
            0x1319_8a2e_0370_7344,
            0xa409_3822_299f_31d0,
            0x082e_fa98_ec4e_6c89,
        ];
        const MULTIPLIERS: [u64; LANES] = [
            0x9e37_79b9_7f4a_7c15,
            0xd6e8_feb8_6659_fd93,
            0xa24b_aed4_963e_e407,
            0x9fb2_1c65_1e98_df25,
        ];
        const ROTATIONS: [u32; LANES] = [11, 27, 43, 59];
        let low = eid.0 as u64;
        let high = (eid.0 >> 64) as u64;
        std::array::from_fn(|lane| {
            let key = low
                .wrapping_add(SALTS[lane])
                .wrapping_mul(MULTIPLIERS[lane])
                ^ high.rotate_left(ROTATIONS[lane]);
            // The SplitMix64 finalizer diffuses both halves into the low bits
            // used by a power-of-two lane.
            let key = (key ^ (key >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            let key = (key ^ (key >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            let hash = key ^ (key >> 31);
            let byte = lane * self.lane_bytes + (((hash >> 2) as usize) & (self.lane_bytes - 1));
            (byte, (hash as u32 & 3) * 2)
        })
    }

    pub(super) fn note(&mut self, eid: EId) {
        for (byte, shift) in self.locations(eid) {
            let packed = &mut self.counters.as_mut()[byte];
            if (*packed >> shift) & 3 < 3 {
                // No carry can reach an adjacent counter.
                *packed += 1 << shift;
            }
        }
    }

    pub(super) fn needs_history(&self, eid: EId) -> bool {
        let mut singleton = false;
        for (byte, shift) in self.locations(eid) {
            match (self.counters.as_ref()[byte] >> shift) & 3 {
                0 => return true, // Never certify an identity absent from this pass.
                1 => singleton = true,
                _ => {}
            }
        }
        !singleton
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;

    #[test]
    fn rounds_down_and_refunds_the_admitted_table() {
        let (_, report) = run_async_under_lab(71, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let pool = MemoryPool::new(64, 0).unwrap();
            for budget in [0, 1, 31] {
                assert!(EdgeCounts::new(&cx, &pool, budget).is_err());
                assert_eq!(pool.used(), 0);
            }
            for (budget, charge) in [(32, 32), (63, 32), (64, 64), (127, 64)] {
                let _table = EdgeCounts::new(&cx, &pool, budget).unwrap();
                assert_eq!(pool.used(), charge);
            }
            assert!(matches!(
                EdgeCounts::new(&cx, &pool, usize::MAX),
                Err(BufferedReadError::Memory(
                    MemoryError::ResourceExhausted { .. }
                ))
            ));
            assert_eq!(pool.used(), 0);
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }

    #[test]
    fn collisions_and_saturation_never_hide_a_repeat() {
        let (_, report) = run_async_under_lab(72, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let pool = MemoryPool::new(32, 0).unwrap();
            let mut table = EdgeCounts::new(&cx, &pool, 32).unwrap();
            let first = EId(u128::MAX);
            assert!(table.needs_history(first));
            table.note(first);
            assert!(!table.needs_history(first));
            for _ in 0..512 {
                table.note(first);
            }
            assert!(table.needs_history(first));
            // Saturate the 32 counters in each lane, including all packed
            // neighbors. Every one of these identities is now repeated.
            for i in 0..4096u128 {
                table.note(EId(i));
                table.note(EId(i));
            }
            assert!(table.counters.as_ref().iter().all(|&byte| byte == u8::MAX));
            assert!((0..4096u128).all(|i| table.needs_history(EId(i))));
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }

    #[test]
    fn sparse_ordinals_in_either_half_preserve_singleton_admission() {
        let (_, report) = run_async_under_lab(73, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let pool = MemoryPool::new(65_536, 0).unwrap();
            let layouts: [fn(u64) -> u128; 4] = [
                |i| u128::from(i),
                |i| u128::from(i) << 64,
                |i| (u128::from(i) << 64) | u128::from(i),
                |i| (u128::from(!i) << 64) | u128::from(i.reverse_bits()),
            ];
            for layout in layouts {
                let mut table = EdgeCounts::new(&cx, &pool, 65_536).unwrap();
                for i in 0..16_000 {
                    let id = EId(layout(i));
                    table.note(id);
                    if i % 16 == 0 {
                        table.note(id);
                    }
                }
                let mut extra = 0;
                for i in 0..16_000 {
                    let track = table.needs_history(EId(layout(i)));
                    if i % 16 == 0 {
                        assert!(track);
                    } else if track {
                        extra += 1;
                    }
                }
                assert!(
                    extra < 300,
                    "{extra} of 15,000 singletons collided in every lane"
                );
            }
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }
}
