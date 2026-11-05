//! Allocation-free bucket mechanics for a stable incoming-position transpose.
//!
//! The caller owns source admission, allocation, checkpoints, and the two source
//! passes. It counts each destination, prefixes all buckets, then supplies
//! monotonically increasing source positions. No identity or source authority
//! is created here; positions continue to refer to the admitted outgoing image.

#![forbid(unsafe_code)]

use core::ops::Range;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransposeError {
    SizeOverflow,
    InvalidPosition,
}

/// Exactly three machine words per destination. Together with one u64 per
/// incidence this fits the former 32-byte incidence-record scratch allowance
/// whenever the admitted dictionary has no more entries than incidences.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Bucket {
    start: usize,
    next: usize,
    end: usize,
}

impl Bucket {
    /// Count before prefixing; the end field temporarily holds the degree.
    pub(crate) fn count_one(&mut self) -> Result<(), TransposeError> {
        self.end = self
            .end
            .checked_add(1)
            .ok_or(TransposeError::SizeOverflow)?;
        Ok(())
    }

    /// Turn the completed count into a disjoint output interval. Failure leaves
    /// the count untouched, and successful prefixing starts its write cursor.
    pub(crate) fn prefix(&mut self, start: usize) -> Result<usize, TransposeError> {
        let end = start
            .checked_add(self.end)
            .ok_or(TransposeError::SizeOverflow)?;
        self.start = start;
        self.next = start;
        self.end = end;
        Ok(end)
    }

    /// Append the next source position without crossing the assigned interval.
    /// The caller's increasing source traversal makes every bucket monotone.
    pub(crate) fn scatter(
        &mut self,
        positions: &mut [u64],
        position: u64,
    ) -> Result<(), TransposeError> {
        if self.next >= self.end {
            return Err(TransposeError::InvalidPosition);
        }
        let output = positions
            .get_mut(self.next)
            .ok_or(TransposeError::InvalidPosition)?;
        *output = position;
        self.next += 1;
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.next == self.end
    }

    pub(crate) fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(ranks: &[usize], destinations: usize) {
        let mut buckets = vec![Bucket::default(); destinations];
        let mut positions = Vec::with_capacity(ranks.len());
        for &rank in ranks {
            buckets[rank].count_one().unwrap();
            positions.push(0);
        }
        let mut end = 0;
        for bucket in &mut buckets {
            end = bucket.prefix(end).unwrap();
        }
        assert_eq!(end, ranks.len());
        for (position, &rank) in ranks.iter().enumerate() {
            buckets[rank]
                .scatter(&mut positions, position as u64)
                .unwrap();
        }
        let mut expected = ranks
            .iter()
            .enumerate()
            .map(|(position, &rank)| (rank, position as u64))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        let mut actual = Vec::new();
        for (rank, bucket) in buckets.iter().enumerate() {
            assert!(bucket.is_complete());
            let row = &positions[bucket.range()];
            assert!(row.windows(2).all(|pair| pair[0] < pair[1]));
            actual.extend(row.iter().map(|&position| (rank, position)));
        }
        assert_eq!(actual, expected);
    }

    fn enumerate(ranks: &mut Vec<usize>, destinations: usize, remaining: usize) {
        check(ranks, destinations);
        if remaining == 0 {
            return;
        }
        for rank in 0..destinations {
            ranks.push(rank);
            enumerate(ranks, destinations, remaining - 1);
            assert_eq!(ranks.pop(), Some(rank));
        }
    }

    #[test]
    fn every_small_rank_sequence_has_the_comparator_permutation() {
        check(&[], 0);
        for destinations in 1..=4 {
            enumerate(&mut Vec::new(), destinations, 7);
        }
    }

    #[test]
    fn sparse_hub_duplicate_and_reversed_inputs_keep_stable_positions() {
        let mut seed = 0xdead_beef_1234_5678u64;
        for count in [1usize, 8, 63, 64, 65, 255, 256, 257, 65_536] {
            let destinations = count.min(8_192);
            for shape in 0..4 {
                let ranks = (0..count)
                    .map(|position| match shape {
                        0 => position % destinations,
                        1 => (count - position - 1) % destinations,
                        2 => 0,
                        _ => {
                            seed ^= seed << 13;
                            seed ^= seed >> 7;
                            seed ^= seed << 17;
                            seed as usize % destinations
                        }
                    })
                    .collect::<Vec<_>>();
                check(&ranks, destinations);
            }
        }
    }

    #[test]
    fn overflow_and_invalid_scatter_leave_the_refusing_step_untouched() {
        let mut count = Bucket {
            end: usize::MAX,
            ..Bucket::default()
        };
        let before = count;
        assert_eq!(count.count_one(), Err(TransposeError::SizeOverflow));
        assert_eq!(count, before);
        assert_eq!(count.prefix(1), Err(TransposeError::SizeOverflow));
        assert_eq!(count, before);

        let mut bucket = Bucket::default();
        bucket.count_one().unwrap();
        bucket.count_one().unwrap();
        assert_eq!(bucket.prefix(0), Ok(2));
        let mut too_short = [17];
        bucket.scatter(&mut too_short, 3).unwrap();
        let before = bucket;
        assert_eq!(
            bucket.scatter(&mut too_short, 5),
            Err(TransposeError::InvalidPosition)
        );
        assert_eq!(bucket, before);
        assert_eq!(too_short, [3]);
        let mut exact = [3, 17];
        bucket.scatter(&mut exact, u64::MAX).unwrap();
        assert_eq!(exact, [3, u64::MAX]);
        assert!(bucket.is_complete());
        let before = bucket;
        assert_eq!(
            bucket.scatter(&mut exact, 7),
            Err(TransposeError::InvalidPosition)
        );
        assert_eq!(bucket, before);
        assert_eq!(exact, [3, u64::MAX]);
    }

    #[test]
    fn bucket_backing_fits_the_existing_incidence_scratch_bound() {
        assert_eq!(size_of::<Bucket>(), 3 * size_of::<usize>());
        assert!(size_of::<Bucket>() + size_of::<u64>() <= 32);
        for incidences in [0usize, 1, 8, 257, 65_536] {
            for destinations in [0, incidences / 2, incidences] {
                assert!(
                    destinations * size_of::<Bucket>() + incidences * size_of::<u64>()
                        <= incidences * 32
                );
            }
        }
    }
}
