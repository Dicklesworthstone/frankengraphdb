//! Mergeable logarithmic histograms for authorized graph degrees.
//!
//! The 65 buckets are fixed and canonical: bucket zero contains degree zero;
//! bucket `b > 0` contains values whose unsigned bit length is `b`. Unlike an
//! insertion-only frequency sketch, this summary supports exact deletion at
//! its declared bucket resolution.

use core::fmt;

/// Number of canonical degree buckets.
pub const DEGREE_BUCKETS: usize = u64::BITS as usize + 1;

const CANONICAL_MAGIC: [u8; 8] = *b"FGDBDGH1";
const CANONICAL_VERSION: u16 = 1;
/// Exact byte length of one canonical degree histogram.
pub const DEGREE_HISTOGRAM_CANONICAL_BYTES: usize = 8 + 2 + 2 + 8 + 8 + (DEGREE_BUCKETS * 8);

/// Inclusive value interval represented by one bucket.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DegreeBucket {
    /// Canonical bucket index in `0..65`.
    pub index: u8,
    /// Smallest degree in this bucket.
    pub lower: u64,
    /// Largest degree in this bucket.
    pub upper: u64,
    /// Number of observations currently in this bucket.
    pub count: u64,
}

/// Typed histogram transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DegreeHistogramError {
    /// The configured total-observation ceiling would be exceeded.
    ObservationLimitExceeded {
        /// Configured maximum observation count.
        maximum: u64,
    },
    /// A bucket or total-count addition overflowed.
    CountOverflow,
    /// A deletion named a bucket with no observations.
    MissingObservation {
        /// Degree whose bucket is empty.
        degree: u64,
        /// Canonical empty bucket.
        bucket: u8,
    },
    /// Merge operands use different observation ceilings.
    ProfileMismatch {
        /// Receiver ceiling.
        left_maximum: u64,
        /// Other ceiling.
        right_maximum: u64,
    },
}

impl fmt::Display for DegreeHistogramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ObservationLimitExceeded { maximum } => write!(
                formatter,
                "degree histogram reached its {maximum}-observation ceiling"
            ),
            Self::CountOverflow => formatter.write_str("degree histogram count overflow"),
            Self::MissingObservation { degree, bucket } => write!(
                formatter,
                "cannot remove degree {degree}: canonical bucket {bucket} is empty"
            ),
            Self::ProfileMismatch {
                left_maximum,
                right_maximum,
            } => write!(
                formatter,
                "degree histogram ceilings differ: {left_maximum} versus {right_maximum}"
            ),
        }
    }
}

impl std::error::Error for DegreeHistogramError {}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DegreeHistogramCodecError {
    /// Input length is not the one fixed canonical histogram length.
    EncodedLengthMismatch {
        /// Required fixed canonical length.
        expected: usize,
        /// Actual input length.
        actual: usize,
    },
    /// The eight-byte format discriminator did not match.
    MagicMismatch {
        /// Bytes found at the format discriminator.
        actual: [u8; 8],
    },
    /// The encoded version is unsupported.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// The encoded bucket inventory is not the fixed canonical inventory.
    BucketCountMismatch {
        /// Bucket count found in the input.
        actual: u16,
    },
    /// The encoded observation ceiling does not equal the trusted expected ceiling.
    ProfileMismatch {
        /// Trusted observation ceiling supplied by the caller.
        expected_maximum: u64,
        /// Observation ceiling decoded from the canonical header.
        actual_maximum: u64,
    },
    /// Input ended before a complete field could be read.
    Truncated {
        /// Byte offset of the field.
        offset: usize,
        /// Bytes needed for the field.
        needed: usize,
        /// Bytes remaining at the offset.
        remaining: usize,
    },
    /// Input contains bytes after the one canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
    /// The encoded observation count exceeds its profile ceiling.
    ObservationLimitExceeded {
        /// Encoded observation count.
        actual: u64,
        /// Encoded ceiling.
        maximum: u64,
    },
    /// Adding canonical bucket counts overflowed.
    CountOverflow,
    /// Canonical bucket counts do not sum to the encoded total.
    TotalMismatch {
        /// Encoded total.
        expected: u64,
        /// Sum of bucket counts.
        actual: u64,
    },
}

impl fmt::Display for DegreeHistogramCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for DegreeHistogramCodecError {}

/// Exact-at-bucket-resolution histogram with checked merge and deletion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DegreeHistogram {
    counts: [u64; DEGREE_BUCKETS],
    total: u64,
    max_observations: u64,
}

impl DegreeHistogram {
    /// Creates an empty histogram with an explicit observation ceiling.
    #[must_use]
    pub const fn new(max_observations: u64) -> Self {
        Self {
            counts: [0; DEGREE_BUCKETS],
            total: 0,
            max_observations,
        }
    }

    /// Configured total-observation ceiling.
    #[must_use]
    pub const fn max_observations(&self) -> u64 {
        self.max_observations
    }

    /// Number of observations.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.total
    }

    /// Whether no degree is represented.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Canonical bucket counts in increasing degree order.
    #[must_use]
    pub const fn canonical_counts(&self) -> &[u64; DEGREE_BUCKETS] {
        &self.counts
    }

    /// Encodes the complete profile and buckets into fixed canonical bytes.
    /// Fixed-width integers use the workspace's little-endian durable law.
    pub fn to_canonical_bytes(
        &self,
    ) -> Result<[u8; DEGREE_HISTOGRAM_CANONICAL_BYTES], DegreeHistogramCodecError> {
        validate_canonical_counts(self.max_observations, self.total, &self.counts)?;
        let mut bytes = [0_u8; DEGREE_HISTOGRAM_CANONICAL_BYTES];
        let mut offset = 0;
        copy_field(&mut bytes, &mut offset, &CANONICAL_MAGIC);
        copy_field(&mut bytes, &mut offset, &CANONICAL_VERSION.to_le_bytes());
        copy_field(
            &mut bytes,
            &mut offset,
            &(DEGREE_BUCKETS as u16).to_le_bytes(),
        );
        copy_field(
            &mut bytes,
            &mut offset,
            &self.max_observations.to_le_bytes(),
        );
        copy_field(&mut bytes, &mut offset, &self.total.to_le_bytes());
        for count in self.counts {
            copy_field(&mut bytes, &mut offset, &count.to_le_bytes());
        }
        debug_assert_eq!(offset, DEGREE_HISTOGRAM_CANONICAL_BYTES);
        Ok(bytes)
    }

    /// Decodes exactly one fixed canonical histogram value.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_max_observations: u64,
    ) -> Result<Self, DegreeHistogramCodecError> {
        if bytes.len() != DEGREE_HISTOGRAM_CANONICAL_BYTES {
            return Err(DegreeHistogramCodecError::EncodedLengthMismatch {
                expected: DEGREE_HISTOGRAM_CANONICAL_BYTES,
                actual: bytes.len(),
            });
        }
        let mut decoder = DegreeHistogramDecoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != CANONICAL_MAGIC {
            return Err(DegreeHistogramCodecError::MagicMismatch { actual: magic });
        }
        let version = decoder.read_u16()?;
        if version != CANONICAL_VERSION {
            return Err(DegreeHistogramCodecError::UnsupportedVersion { actual: version });
        }
        let bucket_count = decoder.read_u16()?;
        if usize::from(bucket_count) != DEGREE_BUCKETS {
            return Err(DegreeHistogramCodecError::BucketCountMismatch {
                actual: bucket_count,
            });
        }
        let max_observations = decoder.read_u64()?;
        if max_observations != expected_max_observations {
            return Err(DegreeHistogramCodecError::ProfileMismatch {
                expected_maximum: expected_max_observations,
                actual_maximum: max_observations,
            });
        }
        let total = decoder.read_u64()?;
        let mut counts = [0_u64; DEGREE_BUCKETS];
        for count in &mut counts {
            *count = decoder.read_u64()?;
        }
        decoder.finish()?;
        validate_canonical_counts(max_observations, total, &counts)?;
        Ok(Self {
            counts,
            total,
            max_observations,
        })
    }

    /// Adds one degree observation.
    pub fn try_observe(&mut self, degree: u64) -> Result<(), DegreeHistogramError> {
        let next_total = self
            .total
            .checked_add(1)
            .ok_or(DegreeHistogramError::CountOverflow)?;
        if next_total > self.max_observations {
            return Err(DegreeHistogramError::ObservationLimitExceeded {
                maximum: self.max_observations,
            });
        }
        let index = bucket_index(degree);
        let next_bucket = self.counts[index]
            .checked_add(1)
            .ok_or(DegreeHistogramError::CountOverflow)?;
        self.counts[index] = next_bucket;
        self.total = next_total;
        Ok(())
    }

    /// Removes one degree observation at the declared bucket resolution.
    pub fn try_remove(&mut self, degree: u64) -> Result<(), DegreeHistogramError> {
        let index = bucket_index(degree);
        if self.counts[index] == 0 {
            return Err(DegreeHistogramError::MissingObservation {
                degree,
                bucket: index as u8,
            });
        }
        self.counts[index] -= 1;
        self.total -= 1;
        Ok(())
    }

    /// Merges another histogram after checking the complete profile and result.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), DegreeHistogramError> {
        if self.max_observations != other.max_observations {
            return Err(DegreeHistogramError::ProfileMismatch {
                left_maximum: self.max_observations,
                right_maximum: other.max_observations,
            });
        }
        let next_total = self
            .total
            .checked_add(other.total)
            .ok_or(DegreeHistogramError::CountOverflow)?;
        if next_total > self.max_observations {
            return Err(DegreeHistogramError::ObservationLimitExceeded {
                maximum: self.max_observations,
            });
        }
        for (&left, &right) in self.counts.iter().zip(&other.counts) {
            left.checked_add(right)
                .ok_or(DegreeHistogramError::CountOverflow)?;
        }
        for (left, &right) in self.counts.iter_mut().zip(&other.counts) {
            *left += right;
        }
        self.total = next_total;
        Ok(())
    }

    /// Returns the bucket containing the zero-based ordered observation.
    ///
    /// The result is an interval because values inside one logarithmic bucket
    /// are intentionally indistinguishable.
    #[must_use]
    pub fn select_bucket(&self, ordinal: u64) -> Option<DegreeBucket> {
        if ordinal >= self.total {
            return None;
        }
        let mut preceding = 0_u64;
        for (index, &count) in self.counts.iter().enumerate() {
            let after = preceding + count;
            if ordinal < after {
                let (lower, upper) = bucket_bounds(index);
                return Some(DegreeBucket {
                    index: index as u8,
                    lower,
                    upper,
                    count,
                });
            }
            preceding = after;
        }
        None
    }

    /// Returns one bucket's canonical interval and count.
    #[must_use]
    pub fn bucket(&self, index: u8) -> Option<DegreeBucket> {
        let index = usize::from(index);
        let &count = self.counts.get(index)?;
        let (lower, upper) = bucket_bounds(index);
        Some(DegreeBucket {
            index: index as u8,
            lower,
            upper,
            count,
        })
    }
}

fn bucket_index(degree: u64) -> usize {
    if degree == 0 {
        0
    } else {
        (u64::BITS - degree.leading_zeros()) as usize
    }
}

fn bucket_bounds(index: usize) -> (u64, u64) {
    debug_assert!(index < DEGREE_BUCKETS);
    match index {
        0 => (0, 0),
        64 => (1_u64 << 63, u64::MAX),
        _ => {
            let lower = 1_u64 << (index - 1);
            (lower, (lower << 1) - 1)
        }
    }
}

fn validate_canonical_counts(
    max_observations: u64,
    total: u64,
    counts: &[u64; DEGREE_BUCKETS],
) -> Result<(), DegreeHistogramCodecError> {
    if total > max_observations {
        return Err(DegreeHistogramCodecError::ObservationLimitExceeded {
            actual: total,
            maximum: max_observations,
        });
    }
    let mut sum = 0_u64;
    for count in counts {
        sum = sum
            .checked_add(*count)
            .ok_or(DegreeHistogramCodecError::CountOverflow)?;
    }
    if sum != total {
        return Err(DegreeHistogramCodecError::TotalMismatch {
            expected: total,
            actual: sum,
        });
    }
    Ok(())
}

fn copy_field<const LENGTH: usize>(
    destination: &mut [u8; DEGREE_HISTOGRAM_CANONICAL_BYTES],
    offset: &mut usize,
    value: &[u8; LENGTH],
) {
    let end = *offset + LENGTH;
    destination[*offset..end].copy_from_slice(value);
    *offset = end;
}

struct DegreeHistogramDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> DegreeHistogramDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const LENGTH: usize>(
        &mut self,
    ) -> Result<[u8; LENGTH], DegreeHistogramCodecError> {
        let Some(end) = self.offset.checked_add(LENGTH) else {
            return Err(DegreeHistogramCodecError::Truncated {
                offset: self.offset,
                needed: LENGTH,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        let Some(source) = self.bytes.get(self.offset..end) else {
            return Err(DegreeHistogramCodecError::Truncated {
                offset: self.offset,
                needed: LENGTH,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        self.offset = end;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, DegreeHistogramCodecError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, DegreeHistogramCodecError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), DegreeHistogramCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(DegreeHistogramCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_accuracy_fixtures::{FixtureSource, named_graph_fixtures, node_degrees};

    #[test]
    fn every_power_boundary_maps_to_its_canonical_interval() {
        let fixtures = [
            (0, 0, 0, 0),
            (1, 1, 1, 1),
            (2, 2, 2, 3),
            (3, 2, 2, 3),
            (4, 3, 4, 7),
            (7, 3, 4, 7),
            (8, 4, 8, 15),
            (u64::MAX, 64, 1_u64 << 63, u64::MAX),
        ];
        for (degree, index, lower, upper) in fixtures {
            assert_eq!(bucket_index(degree), index);
            assert_eq!(bucket_bounds(index), (lower, upper));
        }
    }

    #[test]
    fn observe_remove_and_selection_preserve_bucket_semantics() {
        let mut histogram = DegreeHistogram::new(20);
        for degree in [0, 1, 2, 3, 3, 8, u64::MAX] {
            histogram.try_observe(degree).expect("within ceiling");
        }
        assert_eq!(histogram.len(), 7);
        assert_eq!(
            histogram.select_bucket(0),
            Some(DegreeBucket {
                index: 0,
                lower: 0,
                upper: 0,
                count: 1,
            })
        );
        assert_eq!(
            histogram.select_bucket(3).map(|bucket| bucket.index),
            Some(2)
        );
        assert_eq!(
            histogram.select_bucket(6).map(|bucket| bucket.index),
            Some(64)
        );
        assert_eq!(histogram.select_bucket(7), None);

        histogram.try_remove(2).expect("bucket has observations");
        assert_eq!(histogram.bucket(2).map(|bucket| bucket.count), Some(2));
        histogram.try_remove(3).expect("bucket has observations");
        histogram.try_remove(3).expect("bucket has observations");
        assert_eq!(
            histogram.try_remove(2),
            Err(DegreeHistogramError::MissingObservation {
                degree: 2,
                bucket: 2,
            })
        );
    }

    #[test]
    fn canonical_codec_round_trips_and_is_observation_order_independent() {
        let observations = [0, 1, 2, 3, 3, 8, u64::MAX];
        let mut forward = DegreeHistogram::new(20);
        let mut reverse = DegreeHistogram::new(20);
        for degree in observations {
            forward.try_observe(degree).expect("within ceiling");
        }
        for degree in observations.into_iter().rev() {
            reverse.try_observe(degree).expect("within ceiling");
        }

        let forward_bytes = forward.to_canonical_bytes().expect("valid histogram");
        let reverse_bytes = reverse.to_canonical_bytes().expect("valid histogram");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBDGH1");
        assert_eq!(&forward_bytes[8..10], &1_u16.to_le_bytes());

        let decoded = DegreeHistogram::try_from_canonical_bytes(&forward_bytes, 20)
            .expect("canonical histogram");
        assert_eq!(decoded, forward);
        assert_eq!(
            DegreeHistogram::try_from_canonical_bytes(&forward_bytes, 19),
            Err(DegreeHistogramCodecError::ProfileMismatch {
                expected_maximum: 19,
                actual_maximum: 20,
            })
        );
        assert_eq!(
            decoded.to_canonical_bytes().expect("decoded histogram"),
            forward_bytes
        );
    }

    #[test]
    fn canonical_decoder_rejects_noncanonical_or_incomplete_values() {
        let mut histogram = DegreeHistogram::new(20);
        for degree in [0, 1, 2, 3] {
            histogram.try_observe(degree).expect("within ceiling");
        }
        let encoded = histogram.to_canonical_bytes().expect("valid histogram");

        let mut wrong_magic = encoded;
        wrong_magic[0] ^= 1;
        assert!(matches!(
            DegreeHistogram::try_from_canonical_bytes(&wrong_magic, 20),
            Err(DegreeHistogramCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded;
        wrong_version[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            DegreeHistogram::try_from_canonical_bytes(&wrong_version, 20),
            Err(DegreeHistogramCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut wrong_bucket_count = encoded;
        wrong_bucket_count[10..12].copy_from_slice(&64_u16.to_le_bytes());
        assert_eq!(
            DegreeHistogram::try_from_canonical_bytes(&wrong_bucket_count, 20),
            Err(DegreeHistogramCodecError::BucketCountMismatch { actual: 64 })
        );

        let mut wrong_total = encoded;
        wrong_total[27] ^= 1;
        assert!(matches!(
            DegreeHistogram::try_from_canonical_bytes(&wrong_total, 20),
            Err(DegreeHistogramCodecError::TotalMismatch { .. })
                | Err(DegreeHistogramCodecError::ObservationLimitExceeded { .. })
        ));

        assert_eq!(
            DegreeHistogram::try_from_canonical_bytes(&encoded[..encoded.len() - 1], 20),
            Err(DegreeHistogramCodecError::EncodedLengthMismatch {
                expected: DEGREE_HISTOGRAM_CANONICAL_BYTES,
                actual: DEGREE_HISTOGRAM_CANONICAL_BYTES - 1,
            })
        );

        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert_eq!(
            DegreeHistogram::try_from_canonical_bytes(&trailing, 20),
            Err(DegreeHistogramCodecError::EncodedLengthMismatch {
                expected: DEGREE_HISTOGRAM_CANONICAL_BYTES,
                actual: DEGREE_HISTOGRAM_CANONICAL_BYTES + 1,
            })
        );
    }

    #[test]
    fn merge_is_additive_and_not_idempotent() {
        // `merge_is_checked_commutative_and_associative` cannot constrain this
        // kernel: max is ALSO commutative and associative, so replacing counter
        // addition with max leaves that test green. Verified by mutation --
        // the whole crate suite passed under exactly that substitution.
        //
        // Two relations separate the operators. Both are asserted here because
        // either alone leaves a gap.
        fn part(values: &[u64]) -> DegreeHistogram {
            let mut histogram = DegreeHistogram::new(100);
            for &value in values {
                histogram.try_observe(value).expect("within ceiling");
            }
            histogram
        }

        // 1. NON-IDEMPOTENCE. Merging a histogram with itself must DOUBLE its
        // counts; under max it would be a no-op. This is the discriminator
        // label_counts already relies on, and it is the cheapest one that
        // distinguishes addition from any absorbing operator.
        let single = part(&[1, 1, 5]);
        let mut doubled = single.clone();
        doubled.try_merge(&single).expect("matching profile");
        assert_ne!(
            doubled, single,
            "merging a histogram with itself must not be a no-op: addition is \
             not idempotent, and a merge that absorbs would be indistinguishable \
             from max"
        );
        assert_eq!(doubled.len(), single.len() * 2);
        for (bucket, count) in doubled.canonical_counts().iter().enumerate() {
            assert_eq!(
                *count,
                single.canonical_counts()[bucket] * 2,
                "bucket {bucket} must double under self-merge"
            );
        }

        // 2. MASS CONSERVATION over a disjoint split. Partitioning a stream and
        // merging must equal accumulating it whole -- the property distributed
        // StatsSegment accumulation rests on. Asserted on canonical bytes, not
        // just totals, so a merge that moved mass between buckets while keeping
        // the sum right would still fail.
        let left = [0_u64, 1, 3, 1];
        let right = [2_u64, 8, 9, 1];
        let mut whole = DegreeHistogram::new(100);
        for &value in left.iter().chain(right.iter()) {
            whole.try_observe(value).expect("within ceiling");
        }
        let mut split = part(&left);
        split.try_merge(&part(&right)).expect("matching profile");
        assert_eq!(
            split.to_canonical_bytes(),
            whole.to_canonical_bytes(),
            "splitting the stream and merging must equal accumulating it whole"
        );
        assert_eq!(
            split.len(),
            u64::try_from(left.len() + right.len()).expect("fixture length fits u64")
        );
    }

    #[test]
    fn merge_is_checked_commutative_and_associative() {
        fn part(values: &[u64]) -> DegreeHistogram {
            let mut histogram = DegreeHistogram::new(100);
            for &value in values {
                histogram.try_observe(value).expect("within ceiling");
            }
            histogram
        }
        let a = part(&[0, 1, 3]);
        let b = part(&[2, 8, 9]);
        let c = part(&[u64::MAX, 16]);

        let mut left = a.clone();
        left.try_merge(&b).expect("matching profile");
        let mut right = b.clone();
        right.try_merge(&a).expect("matching profile");
        assert_eq!(left, right);

        let mut ab_c = left;
        ab_c.try_merge(&c).expect("matching profile");
        let mut bc = b;
        bc.try_merge(&c).expect("matching profile");
        let mut a_bc = a;
        a_bc.try_merge(&bc).expect("matching profile");
        assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn profile_and_count_failures_are_atomic() {
        let mut histogram = DegreeHistogram::new(1);
        histogram.try_observe(7).expect("first observation");
        let before = histogram.clone();
        assert_eq!(
            histogram.try_observe(9),
            Err(DegreeHistogramError::ObservationLimitExceeded { maximum: 1 })
        );
        assert_eq!(histogram, before);

        let other = DegreeHistogram::new(2);
        assert_eq!(
            histogram.try_merge(&other),
            Err(DegreeHistogramError::ProfileMismatch {
                left_maximum: 1,
                right_maximum: 2,
            })
        );
        assert_eq!(histogram, before);
    }

    #[test]
    fn named_fnx_degree_histograms_are_exact_and_partition_merge_equivalent() {
        for fixture in named_graph_fixtures() {
            assert_eq!(fixture.source, FixtureSource::FnxGenerators);
            let degrees = node_degrees(&fixture);
            let ceiling = u64::try_from(degrees.len()).expect("fixture size fits u64");
            let build = |values: &[u64]| {
                let mut histogram = DegreeHistogram::new(ceiling);
                for &degree in values {
                    histogram
                        .try_observe(degree)
                        .expect("named fixture fits its exact ceiling");
                }
                histogram
            };

            let whole = build(&degrees);
            let even = degrees.iter().step_by(2).copied().collect::<Vec<_>>();
            let odd = degrees
                .iter()
                .skip(1)
                .step_by(2)
                .copied()
                .collect::<Vec<_>>();
            let mut partitioned = build(&even);
            partitioned
                .try_merge(&build(&odd))
                .expect("partitioned named fixture fits the whole ceiling");
            assert_eq!(
                partitioned.to_canonical_bytes(),
                whole.to_canonical_bytes(),
                "{} partition merge must equal whole accumulation",
                fixture.name
            );

            let expected_nonzero = match fixture.name {
                "fnx_path_graph_n1024" => vec![(1_usize, 2_u64), (2, 1_022)],
                "fnx_star_graph_spokes1023" => vec![(1_usize, 1_023_u64), (10, 1)],
                "fnx_cycle_graph_n1024" => vec![(2_usize, 1_024_u64)],
                "fnx_complete_bipartite_graph_48_64" => {
                    vec![(6_usize, 64_u64), (7, 48)]
                }
                _ => Vec::new(),
            };
            assert!(
                !expected_nonzero.is_empty(),
                "unregistered named graph fixture {}",
                fixture.name
            );
            let actual_nonzero = whole
                .canonical_counts()
                .iter()
                .copied()
                .enumerate()
                .filter(|&(_, count)| count != 0)
                .collect::<Vec<_>>();
            assert_eq!(actual_nonzero, expected_nonzero, "{}", fixture.name);
            assert_eq!(whole.len(), ceiling);

            let removed_degree = degrees[degrees.len() / 2];
            let mut removed = whole.clone();
            removed
                .try_remove(removed_degree)
                .expect("the independently derived degree is present");
            let mut retained = degrees.clone();
            retained.remove(degrees.len() / 2);
            assert_eq!(
                removed.to_canonical_bytes(),
                build(&retained).to_canonical_bytes(),
                "{} exact delete must equal rebuilding the retained population",
                fixture.name
            );
        }
    }
}
