//! Exact label and edge-type cardinality counts.
//!
//! §8.5 lists label/type counts first among `StatsSegment` contents. Unlike the
//! approximate property sketches, this family is exact in both directions: an
//! observation adds a count and a deletion removes exactly that count, so a
//! delete or update workload never forces a rebuild. The price paid for that
//! exactness is a bounded key domain — a schema has finitely many labels and
//! edge types — enforced by the profile rather than assumed.
//!
//! Vertex labels and edge types are separate populations. They share one
//! directory but never one key: the domain is part of the canonical key, so a
//! `Person` label and a hypothetical `Person` edge type cannot alias.
//!
//! Canonical state is the sorted, duplicate-free, zero-free directory of
//! `(domain, key) -> count`. Decrementing a count to zero removes its entry, so
//! two histories that reach the same logical state encode to identical bytes.
//!
//! Merge algebra: profile-identical merges are commutative and associative, and
//! deliberately **not** idempotent. These are exact additive counters, so
//! merging a state with itself doubles every count; that is the correct answer
//! for cardinality accumulation and is asserted by the tests rather than left
//! to the reader.

use core::cmp::Ordering;
use core::fmt;
use std::collections::TryReserveError;

const CANONICAL_MAGIC: [u8; 8] = *b"FGDBLTC1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + (6 * 8);
const CANONICAL_ENTRY_HEADER_BYTES: usize = 1 + 8 + 8;

/// Population addressed by one canonical count key.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LabelCountsDomain {
    /// Count of vertices carrying one label.
    VertexLabel = 1,
    /// Count of edges carrying one type.
    EdgeType = 2,
}

impl LabelCountsDomain {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }

    const fn from_tag(tag: u8) -> Result<Self, LabelCountsCodecError> {
        match tag {
            1 => Ok(Self::VertexLabel),
            2 => Ok(Self::EdgeType),
            _ => Err(LabelCountsCodecError::UnknownDomain { tag }),
        }
    }

    /// Stable lowercase name used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VertexLabel => "vertex-label",
            Self::EdgeType => "edge-type",
        }
    }
}

impl fmt::Display for LabelCountsDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Borrowed canonical identity of one counted label or edge type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LabelCountsKey<'key> {
    domain: LabelCountsDomain,
    name: &'key [u8],
}

impl<'key> LabelCountsKey<'key> {
    /// Creates one borrowed canonical count key.
    #[must_use]
    pub const fn new(domain: LabelCountsDomain, name: &'key [u8]) -> Self {
        Self { domain, name }
    }

    /// Counted population.
    #[must_use]
    pub const fn domain(self) -> LabelCountsDomain {
        self.domain
    }

    /// Canonical label or edge-type name bytes.
    #[must_use]
    pub const fn name(self) -> &'key [u8] {
        self.name
    }
}

/// Complete deterministic behavior and resource profile.
///
/// The directory is bounded by `max_distinct_keys` entries of at most
/// `max_key_bytes` each, so the profile alone fixes the worst-case footprint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LabelCountsProfile {
    /// Maximum number of distinct `(domain, name)` keys retained.
    pub max_distinct_keys: usize,
    /// Maximum bytes in one canonical label or edge-type name.
    pub max_key_bytes: usize,
    /// Maximum aggregate count across every retained key.
    pub max_total_count: u64,
}

impl LabelCountsProfile {
    /// Creates a complete exact-count profile.
    #[must_use]
    pub const fn new(max_distinct_keys: usize, max_key_bytes: usize, max_total_count: u64) -> Self {
        Self {
            max_distinct_keys,
            max_key_bytes,
            max_total_count,
        }
    }

    /// Worst-case retained key payload admitted by this profile.
    pub fn max_key_directory_bytes(self) -> Result<usize, LabelCountsError> {
        self.max_distinct_keys
            .checked_mul(self.max_key_bytes)
            .ok_or(LabelCountsError::ProfileSizeOverflow)
    }
}

/// Caller-owned admission bounds for one canonical label-count value.
///
/// These limits are independent of the encoded profile. Untrusted bytes cannot
/// grant themselves more memory or per-record work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LabelCountsDecodeLimits {
    /// Maximum accepted canonical value length.
    pub max_encoded_bytes: usize,
    /// Maximum accepted profile ceiling and retained entry count.
    pub max_distinct_keys: usize,
    /// Maximum accepted profile ceiling and retained name length.
    pub max_key_bytes: usize,
    /// Maximum accepted profile ceiling and retained aggregate count.
    pub max_total_count: u64,
}

impl LabelCountsDecodeLimits {
    /// Creates explicit decode admission bounds.
    #[must_use]
    pub const fn new(
        max_encoded_bytes: usize,
        max_distinct_keys: usize,
        max_key_bytes: usize,
        max_total_count: u64,
    ) -> Self {
        Self {
            max_encoded_bytes,
            max_distinct_keys,
            max_key_bytes,
            max_total_count,
        }
    }

    /// Bounds sized for a large but finite schema.
    #[must_use]
    pub const fn conservative() -> Self {
        Self::new(8 * 1024 * 1024, 65_536, 1_024, u64::MAX)
    }
}

/// Allocation owned by a failed transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LabelCountsAllocationTarget {
    /// Sorted retained-count directory.
    Directory,
    /// Canonical label or edge-type name bytes.
    Key,
    /// Temporary directory used by an atomic merge.
    MergeDirectory,
}

impl fmt::Display for LabelCountsAllocationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Self::Directory => "count directory",
            Self::Key => "canonical key",
            Self::MergeDirectory => "merge directory",
        };
        formatter.write_str(name)
    }
}

/// Typed construction or state-transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelCountsError {
    /// A zero-key directory has no counting semantics.
    EmptyDistinctKeyLimit,
    /// A canonical name ceiling must be nonzero.
    EmptyKeyByteLimit,
    /// A zero aggregate ceiling admits no observation.
    EmptyTotalCountLimit,
    /// Checked worst-case directory arithmetic overflowed.
    ProfileSizeOverflow,
    /// A canonical key name is empty.
    EmptyKey,
    /// A canonical key name exceeds its profile ceiling.
    KeyTooLarge {
        /// Observed name bytes.
        actual: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// A zero-count observation or deletion has no exact meaning.
    ZeroCount,
    /// Admitting a new key would exceed the distinct-key ceiling.
    DistinctKeyLimitExceeded {
        /// Distinct keys the transition would retain.
        attempted: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// The transition would exceed the aggregate-count ceiling.
    TotalCountLimitExceeded {
        /// Aggregate count the transition would retain.
        attempted: u64,
        /// Profile ceiling.
        maximum: u64,
    },
    /// Checked count arithmetic overflowed.
    CountOverflow,
    /// Checked retained key-byte arithmetic overflowed.
    KeyByteCountOverflow,
    /// A deletion named a key the directory does not retain.
    MissingKey {
        /// Population of the absent key.
        domain: LabelCountsDomain,
    },
    /// A deletion asked for more than the retained count.
    InsufficientCount {
        /// Retained count.
        available: u64,
        /// Requested decrement.
        requested: u64,
    },
    /// Merge operands use different profiles.
    ProfileMismatch,
    /// The allocator rejected a checked reservation.
    AllocationFailed {
        /// Allocation the transition required.
        target: LabelCountsAllocationTarget,
        /// Requested element count.
        requested: usize,
    },
    /// A retained-state invariant did not hold.
    InvariantViolation,
}

impl fmt::Display for LabelCountsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyDistinctKeyLimit => {
                formatter.write_str("label counts require a nonzero distinct-key ceiling")
            }
            Self::EmptyKeyByteLimit => {
                formatter.write_str("label counts require a nonzero key-byte ceiling")
            }
            Self::EmptyTotalCountLimit => {
                formatter.write_str("label counts require a nonzero aggregate-count ceiling")
            }
            Self::ProfileSizeOverflow => {
                formatter.write_str("label-count profile directory size overflowed")
            }
            Self::EmptyKey => formatter.write_str("canonical label-count key is empty"),
            Self::KeyTooLarge { actual, maximum } => write!(
                formatter,
                "canonical label-count key of {actual} bytes exceeds the {maximum}-byte ceiling"
            ),
            Self::ZeroCount => {
                formatter.write_str("label-count transitions require a nonzero count")
            }
            Self::DistinctKeyLimitExceeded { attempted, maximum } => write!(
                formatter,
                "label counts would retain {attempted} distinct keys above the {maximum}-key ceiling"
            ),
            Self::TotalCountLimitExceeded { attempted, maximum } => write!(
                formatter,
                "label counts would total {attempted} above the {maximum}-observation ceiling"
            ),
            Self::CountOverflow => formatter.write_str("label-count arithmetic overflowed"),
            Self::KeyByteCountOverflow => {
                formatter.write_str("retained label-count key bytes overflowed")
            }
            Self::MissingKey { domain } => {
                write!(formatter, "no retained {domain} count for the named key")
            }
            Self::InsufficientCount {
                available,
                requested,
            } => write!(
                formatter,
                "cannot remove {requested} from a retained count of {available}"
            ),
            Self::ProfileMismatch => formatter.write_str("label-count profiles differ"),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "allocator rejected {requested} elements for the {target}"
            ),
            Self::InvariantViolation => {
                formatter.write_str("label-count retained state violated its own invariant")
            }
        }
    }
}

impl std::error::Error for LabelCountsError {}

/// Resource whose caller-owned decode bound was exceeded.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LabelCountsDecodeResource {
    /// Total canonical value length.
    EncodedBytes,
    /// Profile ceiling or retained entry count.
    DistinctKeys,
    /// Profile ceiling or retained name length.
    KeyBytes,
    /// Profile ceiling or retained aggregate count.
    TotalCount,
}

impl fmt::Display for LabelCountsDecodeResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Self::EncodedBytes => "encoded bytes",
            Self::DistinctKeys => "distinct keys",
            Self::KeyBytes => "key bytes",
            Self::TotalCount => "total count",
        };
        formatter.write_str(name)
    }
}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelCountsCodecError {
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
    /// The domain tag is outside the closed vocabulary.
    UnknownDomain {
        /// Tag found in the input.
        tag: u8,
    },
    /// The encoded profile does not equal the trusted expected profile.
    ProfileMismatch {
        /// Trusted profile supplied by the caller.
        expected: LabelCountsProfile,
        /// Profile decoded from the canonical header.
        actual: LabelCountsProfile,
    },
    /// A caller-owned decode bound was exceeded.
    DecodeLimitExceeded {
        /// Bounded resource.
        resource: LabelCountsDecodeResource,
        /// Value found in the input.
        actual: u64,
        /// Caller-owned bound.
        maximum: u64,
    },
    /// The retained entry count exceeds the encoded profile ceiling.
    EntryCountExceedsProfile {
        /// Retained entries.
        actual: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// A canonical entry carried a zero count.
    ZeroCountEntry {
        /// Index of the offending entry.
        index: usize,
    },
    /// Two canonical entries share one `(domain, name)` key.
    DuplicateEntry {
        /// Index of the offending entry.
        index: usize,
    },
    /// Canonical entries are not in strictly ascending key order.
    EntriesOutOfOrder {
        /// Index of the offending entry.
        index: usize,
    },
    /// The declared aggregate count does not equal the sum of the entries.
    TotalCountMismatch {
        /// Aggregate count declared by the header.
        declared: u64,
        /// Aggregate count summed from the entries.
        actual: u64,
    },
    /// The declared retained key bytes do not equal the sum of the entries.
    KeyByteCountMismatch {
        /// Retained key bytes declared by the header.
        declared: usize,
        /// Retained key bytes summed from the entries.
        actual: usize,
    },
    /// Exact encoded-size arithmetic overflowed.
    LengthOverflow,
    /// A canonical scalar does not fit its platform representation.
    IntegerUnrepresentable {
        /// Value found in the input.
        actual: u64,
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
    /// Input continued past the complete canonical value.
    TrailingBytes {
        /// Byte offset of the first unread byte.
        offset: usize,
        /// Unread byte count.
        remaining: usize,
    },
    /// The allocator rejected a checked reservation.
    AllocationFailed {
        /// Requested element count.
        requested: usize,
    },
    /// The decoded state is not a valid retained state.
    InvalidState {
        /// State-transition failure describing the violation.
        source: LabelCountsError,
    },
}

impl fmt::Display for LabelCountsCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::MagicMismatch { actual } => {
                write!(formatter, "unexpected label-count magic {actual:?}")
            }
            Self::UnsupportedVersion { actual } => {
                write!(formatter, "unsupported label-count version {actual}")
            }
            Self::UnknownDomain { tag } => {
                write!(formatter, "unknown label-count domain tag {tag}")
            }
            Self::ProfileMismatch { .. } => {
                formatter.write_str("encoded label-count profile differs from the expected profile")
            }
            Self::DecodeLimitExceeded {
                resource,
                actual,
                maximum,
            } => write!(
                formatter,
                "label-count {resource} of {actual} exceeds the caller bound of {maximum}"
            ),
            Self::EntryCountExceedsProfile { actual, maximum } => write!(
                formatter,
                "{actual} retained label-count entries exceed the {maximum}-key profile ceiling"
            ),
            Self::ZeroCountEntry { index } => {
                write!(formatter, "label-count entry {index} carries a zero count")
            }
            Self::DuplicateEntry { index } => {
                write!(formatter, "label-count entry {index} duplicates its key")
            }
            Self::EntriesOutOfOrder { index } => {
                write!(formatter, "label-count entry {index} is out of order")
            }
            Self::TotalCountMismatch { declared, actual } => write!(
                formatter,
                "declared label-count total {declared} differs from the summed total {actual}"
            ),
            Self::KeyByteCountMismatch { declared, actual } => write!(
                formatter,
                "declared label-count key bytes {declared} differ from the summed {actual}"
            ),
            Self::LengthOverflow => formatter.write_str("label-count length arithmetic overflowed"),
            Self::IntegerUnrepresentable { actual } => {
                write!(formatter, "label-count scalar {actual} is unrepresentable")
            }
            Self::Truncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "label-count input truncated at {offset}: needed {needed}, {remaining} remaining"
            ),
            Self::TrailingBytes { offset, remaining } => write!(
                formatter,
                "label-count input has {remaining} trailing bytes at {offset}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "allocator rejected {requested} label-count elements"
            ),
            Self::InvalidState { source } => {
                write!(formatter, "invalid label-count state: {source}")
            }
        }
    }
}

impl std::error::Error for LabelCountsCodecError {}

impl From<LabelCountsError> for LabelCountsCodecError {
    fn from(source: LabelCountsError) -> Self {
        Self::InvalidState { source }
    }
}

/// One canonical retained count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelCountsEntry {
    domain: LabelCountsDomain,
    name: Vec<u8>,
    count: u64,
}

impl LabelCountsEntry {
    /// Counted population.
    #[must_use]
    pub const fn domain(&self) -> LabelCountsDomain {
        self.domain
    }

    /// Canonical label or edge-type name bytes.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Exact retained count, always nonzero.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Borrowed canonical key.
    #[must_use]
    pub fn key(&self) -> LabelCountsKey<'_> {
        LabelCountsKey::new(self.domain, &self.name)
    }

    fn compare_key(&self, key: LabelCountsKey<'_>) -> Ordering {
        self.domain
            .cmp(&key.domain)
            .then_with(|| self.name.as_slice().cmp(key.name))
    }
}

/// Canonical sorted, duplicate-free, zero-free logical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LabelCountsState<'sketch> {
    /// Complete immutable profile.
    pub profile: LabelCountsProfile,
    /// Aggregate count across every retained key.
    pub total_count: u64,
    /// Aggregate retained canonical key bytes.
    pub key_bytes: usize,
    /// Retained counts in canonical key order.
    pub entries: &'sketch [LabelCountsEntry],
}

/// Bounded exact label and edge-type count directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelCounts {
    profile: LabelCountsProfile,
    entries: Vec<LabelCountsEntry>,
    total_count: u64,
    key_bytes: usize,
}

impl LabelCounts {
    /// Creates an empty directory without allocating its maximum footprint.
    pub fn try_new(profile: LabelCountsProfile) -> Result<Self, LabelCountsError> {
        validate_profile(profile)?;
        Ok(Self {
            profile,
            entries: Vec::new(),
            total_count: 0,
            key_bytes: 0,
        })
    }

    /// Complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> LabelCountsProfile {
        self.profile
    }

    /// Aggregate count across every retained key.
    #[must_use]
    pub const fn total_count(&self) -> u64 {
        self.total_count
    }

    /// Number of retained distinct keys.
    #[must_use]
    pub fn distinct_keys(&self) -> usize {
        self.entries.len()
    }

    /// Whether no key is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Aggregate retained canonical key bytes.
    #[must_use]
    pub const fn key_bytes(&self) -> usize {
        self.key_bytes
    }

    /// Retained counts in canonical key order.
    #[must_use]
    pub fn entries(&self) -> &[LabelCountsEntry] {
        &self.entries
    }

    /// Canonical sorted and duplicate-free logical state.
    #[must_use]
    pub fn canonical_state(&self) -> LabelCountsState<'_> {
        LabelCountsState {
            profile: self.profile,
            total_count: self.total_count,
            key_bytes: self.key_bytes,
            entries: &self.entries,
        }
    }

    /// Exact retained count for one key, zero when the key is unretained.
    ///
    /// This is exact for the observed population: there is no overestimate and
    /// no confidence qualifier, which is precisely why this family is allowed
    /// to serve deletions.
    #[must_use]
    pub fn count(&self, key: LabelCountsKey<'_>) -> u64 {
        match self
            .entries
            .binary_search_by(|entry| entry.compare_key(key))
        {
            Ok(index) => self.entries.get(index).map_or(0, LabelCountsEntry::count),
            Err(_) => 0,
        }
    }

    /// Adds an exact count for one key.
    ///
    /// Every error leaves the logical state unchanged.
    pub fn try_observe(
        &mut self,
        key: LabelCountsKey<'_>,
        count: u64,
    ) -> Result<(), LabelCountsError> {
        validate_key(self.profile, key)?;
        if count == 0 {
            return Err(LabelCountsError::ZeroCount);
        }
        let next_total = self
            .total_count
            .checked_add(count)
            .ok_or(LabelCountsError::CountOverflow)?;
        if next_total > self.profile.max_total_count {
            return Err(LabelCountsError::TotalCountLimitExceeded {
                attempted: next_total,
                maximum: self.profile.max_total_count,
            });
        }

        match self
            .entries
            .binary_search_by(|entry| entry.compare_key(key))
        {
            Ok(index) => {
                let entry = self
                    .entries
                    .get_mut(index)
                    .ok_or(LabelCountsError::InvariantViolation)?;
                let next_count = entry
                    .count
                    .checked_add(count)
                    .ok_or(LabelCountsError::CountOverflow)?;
                entry.count = next_count;
            }
            Err(index) => {
                let next_distinct_keys = self
                    .entries
                    .len()
                    .checked_add(1)
                    .ok_or(LabelCountsError::ProfileSizeOverflow)?;
                if next_distinct_keys > self.profile.max_distinct_keys {
                    return Err(LabelCountsError::DistinctKeyLimitExceeded {
                        attempted: next_distinct_keys,
                        maximum: self.profile.max_distinct_keys,
                    });
                }
                let next_key_bytes = self
                    .key_bytes
                    .checked_add(key.name.len())
                    .ok_or(LabelCountsError::KeyByteCountOverflow)?;
                self.entries.try_reserve(1).map_err(|_: TryReserveError| {
                    LabelCountsError::AllocationFailed {
                        target: LabelCountsAllocationTarget::Directory,
                        requested: next_distinct_keys,
                    }
                })?;
                let name = try_clone_key(key.name, LabelCountsAllocationTarget::Key)?;
                self.entries.insert(
                    index,
                    LabelCountsEntry {
                        domain: key.domain,
                        name,
                        count,
                    },
                );
                self.key_bytes = next_key_bytes;
            }
        }
        self.total_count = next_total;
        Ok(())
    }

    /// Removes an exact count for one key.
    ///
    /// This family supports exact deletion, so a delete workload never demands
    /// a rebuild. Removing the last observation of a key drops its entry, which
    /// is what keeps canonical state a function of logical state alone. Every
    /// error leaves the logical state unchanged.
    pub fn try_remove(
        &mut self,
        key: LabelCountsKey<'_>,
        count: u64,
    ) -> Result<(), LabelCountsError> {
        validate_key(self.profile, key)?;
        if count == 0 {
            return Err(LabelCountsError::ZeroCount);
        }
        let Ok(index) = self
            .entries
            .binary_search_by(|entry| entry.compare_key(key))
        else {
            return Err(LabelCountsError::MissingKey { domain: key.domain });
        };
        let entry = self
            .entries
            .get(index)
            .ok_or(LabelCountsError::InvariantViolation)?;
        let Some(remaining) = entry.count.checked_sub(count) else {
            return Err(LabelCountsError::InsufficientCount {
                available: entry.count,
                requested: count,
            });
        };
        let next_total = self
            .total_count
            .checked_sub(count)
            .ok_or(LabelCountsError::InvariantViolation)?;

        if remaining == 0 {
            let next_key_bytes = self
                .key_bytes
                .checked_sub(entry.name.len())
                .ok_or(LabelCountsError::InvariantViolation)?;
            self.entries.remove(index);
            self.key_bytes = next_key_bytes;
        } else {
            self.entries
                .get_mut(index)
                .ok_or(LabelCountsError::InvariantViolation)?
                .count = remaining;
        }
        self.total_count = next_total;
        Ok(())
    }

    /// Merges the exact key-wise sum of an identical-profile directory.
    ///
    /// Successful profile-identical merges are commutative and associative.
    /// They are not idempotent: these are exact additive counters, so merging a
    /// state with itself doubles every retained count.
    ///
    /// The merged size is computed without allocating, so a merge that would
    /// breach the distinct-key or aggregate-count ceiling fails before either
    /// operand is touched.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), LabelCountsError> {
        if self.profile != other.profile {
            return Err(LabelCountsError::ProfileMismatch);
        }
        let plan = self.plan_merge(other)?;

        let mut merged = Vec::new();
        merged
            .try_reserve_exact(plan.entry_count)
            .map_err(|_: TryReserveError| LabelCountsError::AllocationFailed {
                target: LabelCountsAllocationTarget::MergeDirectory,
                requested: plan.entry_count,
            })?;

        let mut left = 0_usize;
        let mut right = 0_usize;
        while left < self.entries.len() || right < other.entries.len() {
            let (source, count) = match (self.entries.get(left), other.entries.get(right)) {
                (Some(left_entry), Some(right_entry)) => {
                    match left_entry.compare_key(right_entry.key()) {
                        Ordering::Less => {
                            left += 1;
                            (left_entry, left_entry.count)
                        }
                        Ordering::Greater => {
                            right += 1;
                            (right_entry, right_entry.count)
                        }
                        Ordering::Equal => {
                            left += 1;
                            right += 1;
                            let count = left_entry
                                .count
                                .checked_add(right_entry.count)
                                .ok_or(LabelCountsError::CountOverflow)?;
                            (left_entry, count)
                        }
                    }
                }
                (Some(left_entry), None) => {
                    left += 1;
                    (left_entry, left_entry.count)
                }
                (None, Some(right_entry)) => {
                    right += 1;
                    (right_entry, right_entry.count)
                }
                (None, None) => return Err(LabelCountsError::InvariantViolation),
            };
            let name = try_clone_key(&source.name, LabelCountsAllocationTarget::Key)?;
            merged.push(LabelCountsEntry {
                domain: source.domain,
                name,
                count,
            });
        }
        if merged.len() != plan.entry_count {
            return Err(LabelCountsError::InvariantViolation);
        }

        self.entries = merged;
        self.total_count = plan.total_count;
        self.key_bytes = plan.key_bytes;
        Ok(())
    }

    /// Computes the merged shape without allocating or mutating either operand.
    fn plan_merge(&self, other: &Self) -> Result<MergePlan, LabelCountsError> {
        let mut left = 0_usize;
        let mut right = 0_usize;
        let mut entry_count = 0_usize;
        let mut total_count = 0_u64;
        let mut key_bytes = 0_usize;
        while left < self.entries.len() || right < other.entries.len() {
            let (name_bytes, count) = match (self.entries.get(left), other.entries.get(right)) {
                (Some(left_entry), Some(right_entry)) => {
                    match left_entry.compare_key(right_entry.key()) {
                        Ordering::Less => {
                            left += 1;
                            (left_entry.name.len(), left_entry.count)
                        }
                        Ordering::Greater => {
                            right += 1;
                            (right_entry.name.len(), right_entry.count)
                        }
                        Ordering::Equal => {
                            left += 1;
                            right += 1;
                            let count = left_entry
                                .count
                                .checked_add(right_entry.count)
                                .ok_or(LabelCountsError::CountOverflow)?;
                            (left_entry.name.len(), count)
                        }
                    }
                }
                (Some(left_entry), None) => {
                    left += 1;
                    (left_entry.name.len(), left_entry.count)
                }
                (None, Some(right_entry)) => {
                    right += 1;
                    (right_entry.name.len(), right_entry.count)
                }
                (None, None) => return Err(LabelCountsError::InvariantViolation),
            };
            entry_count = entry_count
                .checked_add(1)
                .ok_or(LabelCountsError::ProfileSizeOverflow)?;
            total_count = total_count
                .checked_add(count)
                .ok_or(LabelCountsError::CountOverflow)?;
            key_bytes = key_bytes
                .checked_add(name_bytes)
                .ok_or(LabelCountsError::KeyByteCountOverflow)?;
        }
        if entry_count > self.profile.max_distinct_keys {
            return Err(LabelCountsError::DistinctKeyLimitExceeded {
                attempted: entry_count,
                maximum: self.profile.max_distinct_keys,
            });
        }
        if total_count > self.profile.max_total_count {
            return Err(LabelCountsError::TotalCountLimitExceeded {
                attempted: total_count,
                maximum: self.profile.max_total_count,
            });
        }
        Ok(MergePlan {
            entry_count,
            total_count,
            key_bytes,
        })
    }

    /// Encodes the complete profile and canonical retained state.
    /// Fixed-width integers use the workspace's little-endian durable law.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, LabelCountsCodecError> {
        let layout = validate_canonical_state(
            self.profile,
            self.total_count,
            self.key_bytes,
            &self.entries,
        )?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(layout.encoded_len)
            .map_err(
                |_: TryReserveError| LabelCountsCodecError::AllocationFailed {
                    requested: layout.encoded_len,
                },
            )?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        push_u64(&mut bytes, layout.max_distinct_keys);
        push_u64(&mut bytes, layout.max_key_bytes);
        push_u64(&mut bytes, self.profile.max_total_count);
        push_u64(&mut bytes, self.total_count);
        push_u64(&mut bytes, layout.entry_count);
        push_u64(&mut bytes, layout.key_bytes);
        for entry in &self.entries {
            bytes.push(entry.domain.canonical_tag());
            push_u64(&mut bytes, canonical_usize(entry.name.len())?);
            push_u64(&mut bytes, entry.count);
            bytes.extend_from_slice(&entry.name);
        }
        debug_assert_eq!(bytes.len(), layout.encoded_len);
        Ok(bytes)
    }

    /// Decodes exactly one canonical value under a trusted expected profile and
    /// independent caller-owned resource bounds.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_profile: LabelCountsProfile,
        limits: LabelCountsDecodeLimits,
    ) -> Result<Self, LabelCountsCodecError> {
        let header = preflight_canonical_bytes(bytes, expected_profile, limits)?;
        let mut decoder = LabelCountsDecoder::new(bytes);
        decoder.take(CANONICAL_HEADER_BYTES)?;

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(header.entry_count)
            .map_err(
                |_: TryReserveError| LabelCountsCodecError::AllocationFailed {
                    requested: header.entry_count,
                },
            )?;
        for _ in 0..header.entry_count {
            let domain = LabelCountsDomain::from_tag(decoder.read_u8()?)?;
            let name_bytes = decoded_usize(decoder.read_u64()?)?;
            let count = decoder.read_u64()?;
            let name = decoder.take(name_bytes)?;
            entries.push(LabelCountsEntry {
                domain,
                name: try_clone_key(name, LabelCountsAllocationTarget::Key)?,
                count,
            });
        }
        decoder.finish()?;

        let counts = Self {
            profile: header.profile,
            entries,
            total_count: header.total_count,
            key_bytes: header.key_bytes,
        };
        validate_canonical_state(
            counts.profile,
            counts.total_count,
            counts.key_bytes,
            &counts.entries,
        )?;
        Ok(counts)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MergePlan {
    entry_count: usize,
    total_count: u64,
    key_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalLayout {
    encoded_len: usize,
    max_distinct_keys: u64,
    max_key_bytes: u64,
    entry_count: u64,
    key_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedHeader {
    profile: LabelCountsProfile,
    total_count: u64,
    entry_count: usize,
    key_bytes: usize,
}

fn validate_profile(profile: LabelCountsProfile) -> Result<(), LabelCountsError> {
    if profile.max_distinct_keys == 0 {
        return Err(LabelCountsError::EmptyDistinctKeyLimit);
    }
    if profile.max_key_bytes == 0 {
        return Err(LabelCountsError::EmptyKeyByteLimit);
    }
    if profile.max_total_count == 0 {
        return Err(LabelCountsError::EmptyTotalCountLimit);
    }
    profile.max_key_directory_bytes()?;
    Ok(())
}

fn validate_key(
    profile: LabelCountsProfile,
    key: LabelCountsKey<'_>,
) -> Result<(), LabelCountsError> {
    if key.name.is_empty() {
        return Err(LabelCountsError::EmptyKey);
    }
    if key.name.len() > profile.max_key_bytes {
        return Err(LabelCountsError::KeyTooLarge {
            actual: key.name.len(),
            maximum: profile.max_key_bytes,
        });
    }
    Ok(())
}

fn try_clone_key(
    name: &[u8],
    target: LabelCountsAllocationTarget,
) -> Result<Vec<u8>, LabelCountsError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_: TryReserveError| LabelCountsError::AllocationFailed {
            target,
            requested: name.len(),
        })?;
    owned.extend_from_slice(name);
    Ok(owned)
}

fn validate_canonical_state(
    profile: LabelCountsProfile,
    total_count: u64,
    key_bytes: usize,
    entries: &[LabelCountsEntry],
) -> Result<CanonicalLayout, LabelCountsCodecError> {
    validate_profile(profile)?;
    if entries.len() > profile.max_distinct_keys {
        return Err(LabelCountsCodecError::EntryCountExceedsProfile {
            actual: entries.len(),
            maximum: profile.max_distinct_keys,
        });
    }

    let mut actual_total = 0_u64;
    let mut actual_key_bytes = 0_usize;
    let mut previous: Option<&LabelCountsEntry> = None;
    for (index, entry) in entries.iter().enumerate() {
        validate_key(profile, entry.key())?;
        if entry.count == 0 {
            return Err(LabelCountsCodecError::ZeroCountEntry { index });
        }
        actual_total = actual_total
            .checked_add(entry.count)
            .ok_or(LabelCountsError::CountOverflow)?;
        actual_key_bytes = actual_key_bytes
            .checked_add(entry.name.len())
            .ok_or(LabelCountsError::KeyByteCountOverflow)?;
        if let Some(prior) = previous {
            match prior.compare_key(entry.key()) {
                Ordering::Less => {}
                Ordering::Equal => return Err(LabelCountsCodecError::DuplicateEntry { index }),
                Ordering::Greater => {
                    return Err(LabelCountsCodecError::EntriesOutOfOrder { index });
                }
            }
        }
        previous = Some(entry);
    }
    if actual_total != total_count {
        return Err(LabelCountsCodecError::TotalCountMismatch {
            declared: total_count,
            actual: actual_total,
        });
    }
    if actual_total > profile.max_total_count {
        return Err(LabelCountsError::TotalCountLimitExceeded {
            attempted: actual_total,
            maximum: profile.max_total_count,
        }
        .into());
    }
    if actual_key_bytes != key_bytes {
        return Err(LabelCountsCodecError::KeyByteCountMismatch {
            declared: key_bytes,
            actual: actual_key_bytes,
        });
    }

    Ok(CanonicalLayout {
        encoded_len: expected_canonical_len(entries.len(), key_bytes)?,
        max_distinct_keys: canonical_usize(profile.max_distinct_keys)?,
        max_key_bytes: canonical_usize(profile.max_key_bytes)?,
        entry_count: canonical_usize(entries.len())?,
        key_bytes: canonical_usize(key_bytes)?,
    })
}

fn expected_canonical_len(
    entry_count: usize,
    key_bytes: usize,
) -> Result<usize, LabelCountsCodecError> {
    entry_count
        .checked_mul(CANONICAL_ENTRY_HEADER_BYTES)
        .and_then(|headers| headers.checked_add(key_bytes))
        .and_then(|payload| payload.checked_add(CANONICAL_HEADER_BYTES))
        .ok_or(LabelCountsCodecError::LengthOverflow)
}

fn preflight_canonical_bytes(
    bytes: &[u8],
    expected_profile: LabelCountsProfile,
    limits: LabelCountsDecodeLimits,
) -> Result<DecodedHeader, LabelCountsCodecError> {
    enforce_decode_limit(
        LabelCountsDecodeResource::EncodedBytes,
        canonical_usize(bytes.len())?,
        canonical_usize(limits.max_encoded_bytes)?,
    )?;
    let mut decoder = LabelCountsDecoder::new(bytes);
    let magic = decoder.read_array::<8>()?;
    if magic != CANONICAL_MAGIC {
        return Err(LabelCountsCodecError::MagicMismatch { actual: magic });
    }
    let version = decoder.read_u16()?;
    if version != CANONICAL_VERSION {
        return Err(LabelCountsCodecError::UnsupportedVersion { actual: version });
    }
    let max_distinct_keys = decoded_usize(decoder.read_u64()?)?;
    let max_key_bytes = decoded_usize(decoder.read_u64()?)?;
    let max_total_count = decoder.read_u64()?;
    let total_count = decoder.read_u64()?;
    let entry_count = decoded_usize(decoder.read_u64()?)?;
    let key_bytes = decoded_usize(decoder.read_u64()?)?;

    let limit_distinct_keys = canonical_usize(limits.max_distinct_keys)?;
    let limit_key_bytes = canonical_usize(limits.max_key_bytes)?;
    enforce_decode_limit(
        LabelCountsDecodeResource::DistinctKeys,
        canonical_usize(max_distinct_keys)?,
        limit_distinct_keys,
    )?;
    enforce_decode_limit(
        LabelCountsDecodeResource::DistinctKeys,
        canonical_usize(entry_count)?,
        limit_distinct_keys,
    )?;
    enforce_decode_limit(
        LabelCountsDecodeResource::KeyBytes,
        canonical_usize(max_key_bytes)?,
        limit_key_bytes,
    )?;
    enforce_decode_limit(
        LabelCountsDecodeResource::TotalCount,
        max_total_count,
        limits.max_total_count,
    )?;
    enforce_decode_limit(
        LabelCountsDecodeResource::TotalCount,
        total_count,
        limits.max_total_count,
    )?;

    let profile = LabelCountsProfile {
        max_distinct_keys,
        max_key_bytes,
        max_total_count,
    };
    validate_profile(profile)?;
    if profile != expected_profile {
        return Err(LabelCountsCodecError::ProfileMismatch {
            expected: expected_profile,
            actual: profile,
        });
    }
    if entry_count > max_distinct_keys {
        return Err(LabelCountsCodecError::EntryCountExceedsProfile {
            actual: entry_count,
            maximum: max_distinct_keys,
        });
    }
    let expected_len = expected_canonical_len(entry_count, key_bytes)?;
    if bytes.len() < expected_len {
        return Err(LabelCountsCodecError::Truncated {
            offset: bytes.len(),
            needed: expected_len - bytes.len(),
            remaining: 0,
        });
    }

    let mut actual_total = 0_u64;
    let mut actual_key_bytes = 0_usize;
    let mut previous: Option<(LabelCountsDomain, &[u8])> = None;
    for index in 0..entry_count {
        let domain = LabelCountsDomain::from_tag(decoder.read_u8()?)?;
        let name_bytes = decoded_usize(decoder.read_u64()?)?;
        let count = decoder.read_u64()?;
        enforce_decode_limit(
            LabelCountsDecodeResource::KeyBytes,
            canonical_usize(name_bytes)?,
            limit_key_bytes,
        )?;
        let name = decoder.take(name_bytes)?;
        validate_key(profile, LabelCountsKey::new(domain, name))?;
        if count == 0 {
            return Err(LabelCountsCodecError::ZeroCountEntry { index });
        }
        actual_total = actual_total
            .checked_add(count)
            .ok_or(LabelCountsError::CountOverflow)?;
        enforce_decode_limit(
            LabelCountsDecodeResource::TotalCount,
            actual_total,
            limits.max_total_count,
        )?;
        actual_key_bytes = actual_key_bytes
            .checked_add(name_bytes)
            .ok_or(LabelCountsError::KeyByteCountOverflow)?;
        if let Some((prior_domain, prior_name)) = previous {
            match prior_domain.cmp(&domain).then_with(|| prior_name.cmp(name)) {
                Ordering::Less => {}
                Ordering::Equal => return Err(LabelCountsCodecError::DuplicateEntry { index }),
                Ordering::Greater => {
                    return Err(LabelCountsCodecError::EntriesOutOfOrder { index });
                }
            }
        }
        previous = Some((domain, name));
    }
    if actual_total != total_count {
        return Err(LabelCountsCodecError::TotalCountMismatch {
            declared: total_count,
            actual: actual_total,
        });
    }
    if actual_key_bytes != key_bytes {
        return Err(LabelCountsCodecError::KeyByteCountMismatch {
            declared: key_bytes,
            actual: actual_key_bytes,
        });
    }
    decoder.finish()?;

    Ok(DecodedHeader {
        profile,
        total_count,
        entry_count,
        key_bytes,
    })
}

fn enforce_decode_limit(
    resource: LabelCountsDecodeResource,
    actual: u64,
    maximum: u64,
) -> Result<(), LabelCountsCodecError> {
    if actual > maximum {
        Err(LabelCountsCodecError::DecodeLimitExceeded {
            resource,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn canonical_usize(value: usize) -> Result<u64, LabelCountsCodecError> {
    u64::try_from(value).map_err(|_| LabelCountsCodecError::LengthOverflow)
}

fn decoded_usize(value: u64) -> Result<usize, LabelCountsCodecError> {
    usize::try_from(value)
        .map_err(|_| LabelCountsCodecError::IntegerUnrepresentable { actual: value })
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct LabelCountsDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> LabelCountsDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, needed: usize) -> Result<&'bytes [u8], LabelCountsCodecError> {
        let end = self
            .offset
            .checked_add(needed)
            .ok_or(LabelCountsCodecError::LengthOverflow)?;
        let Some(value) = self.bytes.get(self.offset..end) else {
            return Err(LabelCountsCodecError::Truncated {
                offset: self.offset,
                needed,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        self.offset = end;
        Ok(value)
    }

    fn read_array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], LabelCountsCodecError> {
        let source = self.take(LENGTH)?;
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, LabelCountsCodecError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16, LabelCountsCodecError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, LabelCountsCodecError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), LabelCountsCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(LabelCountsCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        CANONICAL_HEADER_BYTES, CANONICAL_MAGIC, LabelCounts, LabelCountsAllocationTarget,
        LabelCountsCodecError, LabelCountsDecodeLimits, LabelCountsDecodeResource,
        LabelCountsDomain, LabelCountsError, LabelCountsKey, LabelCountsProfile,
    };
    use crate::graph_accuracy_fixtures::named_graph_fixtures;

    const PROFILE: LabelCountsProfile = LabelCountsProfile::new(8, 16, 1_000);

    fn vertex(name: &[u8]) -> LabelCountsKey<'_> {
        LabelCountsKey::new(LabelCountsDomain::VertexLabel, name)
    }

    fn edge(name: &[u8]) -> LabelCountsKey<'_> {
        LabelCountsKey::new(LabelCountsDomain::EdgeType, name)
    }

    fn counts_with(entries: &[(LabelCountsKey<'_>, u64)]) -> LabelCounts {
        let mut counts = LabelCounts::try_new(PROFILE).expect("profile is valid");
        for &(key, count) in entries {
            counts.try_observe(key, count).expect("observation fits");
        }
        counts
    }

    fn limits() -> LabelCountsDecodeLimits {
        LabelCountsDecodeLimits::new(4_096, 8, 16, 1_000)
    }

    #[test]
    fn profile_validation_rejects_degenerate_ceilings() {
        assert_eq!(
            LabelCounts::try_new(LabelCountsProfile::new(0, 16, 10)),
            Err(LabelCountsError::EmptyDistinctKeyLimit)
        );
        assert_eq!(
            LabelCounts::try_new(LabelCountsProfile::new(8, 0, 10)),
            Err(LabelCountsError::EmptyKeyByteLimit)
        );
        assert_eq!(
            LabelCounts::try_new(LabelCountsProfile::new(8, 16, 0)),
            Err(LabelCountsError::EmptyTotalCountLimit)
        );
        assert_eq!(
            LabelCounts::try_new(LabelCountsProfile::new(usize::MAX, 2, 10)),
            Err(LabelCountsError::ProfileSizeOverflow)
        );
    }

    #[test]
    fn observations_are_exact_and_accumulate_per_key() {
        let counts = counts_with(&[
            (vertex(b"Person"), 3),
            (edge(b"KNOWS"), 5),
            (vertex(b"Person"), 4),
        ]);

        assert_eq!(counts.count(vertex(b"Person")), 7);
        assert_eq!(counts.count(edge(b"KNOWS")), 5);
        assert_eq!(counts.count(vertex(b"Missing")), 0);
        assert_eq!(counts.total_count(), 12);
        assert_eq!(counts.distinct_keys(), 2);
        assert_eq!(counts.key_bytes(), b"Person".len() + b"KNOWS".len());
        assert!(!counts.is_empty());
    }

    #[test]
    fn vertex_labels_and_edge_types_never_alias() {
        let counts = counts_with(&[(vertex(b"Person"), 3), (edge(b"Person"), 11)]);

        assert_eq!(counts.count(vertex(b"Person")), 3);
        assert_eq!(counts.count(edge(b"Person")), 11);
        assert_eq!(counts.distinct_keys(), 2);
    }

    #[test]
    fn canonical_order_is_domain_then_name() {
        let counts = counts_with(&[
            (edge(b"KNOWS"), 1),
            (vertex(b"Zebra"), 1),
            (edge(b"AUTHORED"), 1),
            (vertex(b"Person"), 1),
        ]);

        let observed = counts
            .entries()
            .iter()
            .map(|entry| (entry.domain(), entry.name().to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                (LabelCountsDomain::VertexLabel, b"Person".to_vec()),
                (LabelCountsDomain::VertexLabel, b"Zebra".to_vec()),
                (LabelCountsDomain::EdgeType, b"AUTHORED".to_vec()),
                (LabelCountsDomain::EdgeType, b"KNOWS".to_vec()),
            ]
        );
    }

    #[test]
    fn key_validation_rejects_empty_and_oversized_names() {
        let mut counts = LabelCounts::try_new(PROFILE).expect("profile is valid");
        assert_eq!(
            counts.try_observe(vertex(b""), 1),
            Err(LabelCountsError::EmptyKey)
        );
        assert_eq!(
            counts.try_observe(vertex(&[b'x'; 17]), 1),
            Err(LabelCountsError::KeyTooLarge {
                actual: 17,
                maximum: 16,
            })
        );
        assert!(counts.is_empty());
    }

    #[test]
    fn zero_count_transitions_are_rejected_in_both_directions() {
        let mut counts = counts_with(&[(vertex(b"Person"), 2)]);
        assert_eq!(
            counts.try_observe(vertex(b"Person"), 0),
            Err(LabelCountsError::ZeroCount)
        );
        assert_eq!(
            counts.try_remove(vertex(b"Person"), 0),
            Err(LabelCountsError::ZeroCount)
        );
        assert_eq!(counts.count(vertex(b"Person")), 2);
    }

    #[test]
    fn distinct_key_ceiling_rejects_without_mutating() {
        let profile = LabelCountsProfile::new(2, 16, 1_000);
        let mut counts = LabelCounts::try_new(profile).expect("profile is valid");
        counts.try_observe(vertex(b"A"), 1).expect("first key fits");
        counts
            .try_observe(vertex(b"B"), 1)
            .expect("second key fits");

        assert_eq!(
            counts.try_observe(vertex(b"C"), 1),
            Err(LabelCountsError::DistinctKeyLimitExceeded {
                attempted: 3,
                maximum: 2,
            })
        );
        assert_eq!(counts.distinct_keys(), 2);
        assert_eq!(counts.total_count(), 2);
        assert_eq!(counts.count(vertex(b"C")), 0);
    }

    #[test]
    fn total_count_ceiling_rejects_without_mutating() {
        let profile = LabelCountsProfile::new(8, 16, 10);
        let mut counts = LabelCounts::try_new(profile).expect("profile is valid");
        counts.try_observe(vertex(b"A"), 7).expect("first fits");

        assert_eq!(
            counts.try_observe(vertex(b"B"), 4),
            Err(LabelCountsError::TotalCountLimitExceeded {
                attempted: 11,
                maximum: 10,
            })
        );
        assert_eq!(counts.total_count(), 7);
        assert_eq!(counts.distinct_keys(), 1);
    }

    #[test]
    fn deletion_is_exact_and_removing_the_last_observation_drops_the_key() {
        let mut counts = counts_with(&[(vertex(b"Person"), 5), (edge(b"KNOWS"), 2)]);

        counts
            .try_remove(vertex(b"Person"), 3)
            .expect("partial removal is exact");
        assert_eq!(counts.count(vertex(b"Person")), 2);
        assert_eq!(counts.total_count(), 4);
        assert_eq!(counts.distinct_keys(), 2);

        counts
            .try_remove(vertex(b"Person"), 2)
            .expect("final removal is exact");
        assert_eq!(counts.count(vertex(b"Person")), 0);
        assert_eq!(counts.total_count(), 2);
        assert_eq!(counts.distinct_keys(), 1);
        assert_eq!(counts.key_bytes(), b"KNOWS".len());
    }

    #[test]
    fn canonical_state_is_a_function_of_logical_state_alone() {
        let mut observed_then_deleted = counts_with(&[
            (vertex(b"Person"), 9),
            (edge(b"KNOWS"), 4),
            (vertex(b"Ghost"), 3),
        ]);
        observed_then_deleted
            .try_remove(vertex(b"Ghost"), 3)
            .expect("exact deletion");
        observed_then_deleted
            .try_remove(vertex(b"Person"), 2)
            .expect("exact deletion");

        let direct = counts_with(&[(vertex(b"Person"), 7), (edge(b"KNOWS"), 4)]);

        assert_eq!(observed_then_deleted, direct);
        assert_eq!(
            observed_then_deleted
                .try_to_canonical_bytes()
                .expect("state encodes"),
            direct.try_to_canonical_bytes().expect("state encodes")
        );
    }

    #[test]
    fn deletion_failures_are_typed_and_leave_state_unchanged() {
        let mut counts = counts_with(&[(vertex(b"Person"), 2)]);

        assert_eq!(
            counts.try_remove(vertex(b"Absent"), 1),
            Err(LabelCountsError::MissingKey {
                domain: LabelCountsDomain::VertexLabel,
            })
        );
        assert_eq!(
            counts.try_remove(edge(b"Person"), 1),
            Err(LabelCountsError::MissingKey {
                domain: LabelCountsDomain::EdgeType,
            })
        );
        assert_eq!(
            counts.try_remove(vertex(b"Person"), 3),
            Err(LabelCountsError::InsufficientCount {
                available: 2,
                requested: 3,
            })
        );
        assert_eq!(counts.count(vertex(b"Person")), 2);
        assert_eq!(counts.total_count(), 2);
        assert_eq!(counts.distinct_keys(), 1);
    }

    #[test]
    fn merge_is_commutative_and_associative() {
        let first = counts_with(&[(vertex(b"Person"), 3), (edge(b"KNOWS"), 1)]);
        let second = counts_with(&[(vertex(b"Person"), 4), (vertex(b"City"), 2)]);
        let third = counts_with(&[(edge(b"KNOWS"), 5), (edge(b"VISITED"), 6)]);

        let mut left_then_right = first.clone();
        left_then_right.try_merge(&second).expect("merge fits");
        let mut right_then_left = second.clone();
        right_then_left.try_merge(&first).expect("merge fits");
        assert_eq!(left_then_right, right_then_left);
        assert_eq!(left_then_right.count(vertex(b"Person")), 7);
        assert_eq!(left_then_right.count(vertex(b"City")), 2);
        assert_eq!(left_then_right.count(edge(b"KNOWS")), 1);
        assert_eq!(left_then_right.total_count(), 10);

        let mut grouped_left = first.clone();
        grouped_left.try_merge(&second).expect("merge fits");
        grouped_left.try_merge(&third).expect("merge fits");
        let mut grouped_right = second.clone();
        grouped_right.try_merge(&third).expect("merge fits");
        let mut associative = first.clone();
        associative.try_merge(&grouped_right).expect("merge fits");
        assert_eq!(grouped_left, associative);
    }

    #[test]
    fn merge_is_deliberately_not_idempotent() {
        let counts = counts_with(&[(vertex(b"Person"), 3), (edge(b"KNOWS"), 1)]);
        let mut doubled = counts.clone();
        doubled.try_merge(&counts).expect("merge fits");

        assert_ne!(doubled, counts);
        assert_eq!(doubled.count(vertex(b"Person")), 6);
        assert_eq!(doubled.count(edge(b"KNOWS")), 2);
        assert_eq!(doubled.distinct_keys(), counts.distinct_keys());
        assert_eq!(doubled.total_count(), counts.total_count() * 2);
    }

    #[test]
    fn merge_rejects_a_different_profile() {
        let mut counts = counts_with(&[(vertex(b"Person"), 1)]);
        let other_profile = LabelCountsProfile::new(8, 16, 999);
        let other = LabelCounts::try_new(other_profile).expect("profile is valid");

        assert_eq!(
            counts.try_merge(&other),
            Err(LabelCountsError::ProfileMismatch)
        );
        assert_eq!(counts.count(vertex(b"Person")), 1);
    }

    #[test]
    fn merge_ceiling_breaches_leave_both_operands_unchanged() {
        let profile = LabelCountsProfile::new(2, 16, 1_000);
        let mut left = LabelCounts::try_new(profile).expect("profile is valid");
        left.try_observe(vertex(b"A"), 1).expect("fits");
        left.try_observe(vertex(b"B"), 1).expect("fits");
        let mut right = LabelCounts::try_new(profile).expect("profile is valid");
        right.try_observe(vertex(b"C"), 1).expect("fits");

        let before = left.clone();
        assert_eq!(
            left.try_merge(&right),
            Err(LabelCountsError::DistinctKeyLimitExceeded {
                attempted: 3,
                maximum: 2,
            })
        );
        assert_eq!(left, before);
        assert_eq!(right.distinct_keys(), 1);

        let narrow = LabelCountsProfile::new(8, 16, 10);
        let mut small = LabelCounts::try_new(narrow).expect("profile is valid");
        small.try_observe(vertex(b"A"), 6).expect("fits");
        let mut other = LabelCounts::try_new(narrow).expect("profile is valid");
        other.try_observe(vertex(b"A"), 6).expect("fits");
        let before_small = small.clone();
        assert_eq!(
            small.try_merge(&other),
            Err(LabelCountsError::TotalCountLimitExceeded {
                attempted: 12,
                maximum: 10,
            })
        );
        assert_eq!(small, before_small);
    }

    #[test]
    fn canonical_bytes_round_trip_exactly() {
        let counts = counts_with(&[
            (vertex(b"Person"), 3),
            (edge(b"KNOWS"), 5),
            (vertex(b"City"), 2),
        ]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");
        assert_eq!(&encoded[8..10], &1_u16.to_le_bytes());

        let decoded = LabelCounts::try_from_canonical_bytes(&encoded, PROFILE, limits())
            .expect("canonical bytes decode");
        assert_eq!(decoded, counts);
        assert_eq!(
            decoded.try_to_canonical_bytes().expect("state encodes"),
            encoded
        );
    }

    #[test]
    fn empty_state_round_trips_to_the_bare_header() {
        let counts = LabelCounts::try_new(PROFILE).expect("profile is valid");
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");
        assert_eq!(encoded.len(), CANONICAL_HEADER_BYTES);

        let decoded = LabelCounts::try_from_canonical_bytes(&encoded, PROFILE, limits())
            .expect("canonical bytes decode");
        assert_eq!(decoded, counts);
        assert!(decoded.is_empty());
        assert_eq!(decoded.total_count(), 0);
    }

    #[test]
    fn decoding_rejects_a_foreign_or_future_format() {
        let counts = counts_with(&[(vertex(b"Person"), 1)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");

        let mut foreign = encoded.clone();
        foreign[0] = b'X';
        let mut actual = CANONICAL_MAGIC;
        actual[0] = b'X';
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&foreign, PROFILE, limits()),
            Err(LabelCountsCodecError::MagicMismatch { actual })
        );

        let mut future = encoded.clone();
        future[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&future, PROFILE, limits()),
            Err(LabelCountsCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut unknown_domain = encoded.clone();
        unknown_domain[CANONICAL_HEADER_BYTES] = 9;
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&unknown_domain, PROFILE, limits()),
            Err(LabelCountsCodecError::UnknownDomain { tag: 9 })
        );
    }

    #[test]
    fn decoding_rejects_a_profile_the_caller_did_not_expect() {
        let counts = counts_with(&[(vertex(b"Person"), 1)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");
        let unexpected = LabelCountsProfile::new(8, 16, 999);

        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&encoded, unexpected, limits()),
            Err(LabelCountsCodecError::ProfileMismatch {
                expected: unexpected,
                actual: PROFILE,
            })
        );
    }

    #[test]
    fn decoding_rejects_truncated_and_trailing_input() {
        let counts = counts_with(&[(vertex(b"Person"), 1)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");

        let truncated = &encoded[..encoded.len() - 1];
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(truncated, PROFILE, limits()),
            Err(LabelCountsCodecError::Truncated {
                offset: truncated.len(),
                needed: 1,
                remaining: 0,
            })
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&trailing, PROFILE, limits()),
            Err(LabelCountsCodecError::TrailingBytes {
                offset: encoded.len(),
                remaining: 1,
            })
        );
    }

    #[test]
    fn decoding_rejects_noncanonical_entry_sequences() {
        let counts = counts_with(&[(vertex(b"Alpha"), 1), (vertex(b"Bravo"), 1)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");

        // Both names are five bytes and both counts are one, so swapping the
        // payloads reverses key order without disturbing any length, count, or
        // aggregate field: the sequence alone is what the decoder rejects.
        let first_name = CANONICAL_HEADER_BYTES + 17;
        let second_name = first_name + b"Alpha".len() + 17;
        let mut reversed = encoded.clone();
        reversed[first_name..first_name + 5].copy_from_slice(b"Bravo");
        reversed[second_name..second_name + 5].copy_from_slice(b"Alpha");
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&reversed, PROFILE, limits()),
            Err(LabelCountsCodecError::EntriesOutOfOrder { index: 1 })
        );

        let mut duplicated = encoded.clone();
        duplicated[second_name..second_name + 5].copy_from_slice(b"Alpha");
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&duplicated, PROFILE, limits()),
            Err(LabelCountsCodecError::DuplicateEntry { index: 1 })
        );
    }

    #[test]
    fn decoding_rejects_zero_counts_and_disagreeing_aggregates() {
        let counts = counts_with(&[(vertex(b"Person"), 3)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");
        let count_offset = CANONICAL_HEADER_BYTES + 1 + 8;

        let mut zeroed = encoded.clone();
        zeroed[count_offset..count_offset + 8].copy_from_slice(&0_u64.to_le_bytes());
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&zeroed, PROFILE, limits()),
            Err(LabelCountsCodecError::ZeroCountEntry { index: 0 })
        );

        let mut disagreeing = encoded.clone();
        disagreeing[count_offset..count_offset + 8].copy_from_slice(&4_u64.to_le_bytes());
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&disagreeing, PROFILE, limits()),
            Err(LabelCountsCodecError::TotalCountMismatch {
                declared: 3,
                actual: 4,
            })
        );

        let key_bytes_offset = CANONICAL_HEADER_BYTES - 8;
        let mut wrong_key_bytes = encoded.clone();
        wrong_key_bytes[key_bytes_offset..key_bytes_offset + 8]
            .copy_from_slice(&7_u64.to_le_bytes());
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&wrong_key_bytes, PROFILE, limits()),
            Err(LabelCountsCodecError::Truncated {
                offset: encoded.len(),
                needed: 1,
                remaining: 0,
            })
        );
    }

    #[test]
    fn decoding_enforces_caller_owned_bounds_independently_of_the_profile() {
        let counts = counts_with(&[(vertex(b"Person"), 3)]);
        let encoded = counts.try_to_canonical_bytes().expect("state encodes");

        let tiny = LabelCountsDecodeLimits::new(8, 8, 16, 1_000);
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&encoded, PROFILE, tiny),
            Err(LabelCountsCodecError::DecodeLimitExceeded {
                resource: LabelCountsDecodeResource::EncodedBytes,
                actual: u64::try_from(encoded.len()).expect("canonical length fits"),
                maximum: 8,
            })
        );

        let few_keys = LabelCountsDecodeLimits::new(4_096, 4, 16, 1_000);
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&encoded, PROFILE, few_keys),
            Err(LabelCountsCodecError::DecodeLimitExceeded {
                resource: LabelCountsDecodeResource::DistinctKeys,
                actual: 8,
                maximum: 4,
            })
        );

        let small_counts = LabelCountsDecodeLimits::new(4_096, 8, 16, 2);
        assert_eq!(
            LabelCounts::try_from_canonical_bytes(&encoded, PROFILE, small_counts),
            Err(LabelCountsCodecError::DecodeLimitExceeded {
                resource: LabelCountsDecodeResource::TotalCount,
                actual: 1_000,
                maximum: 2,
            })
        );
    }

    #[test]
    fn counts_are_exact_on_the_named_graph_fixtures() {
        for fixture in named_graph_fixtures() {
            let profile = LabelCountsProfile::new(16, 32, u64::from(u32::MAX));
            let mut counts = LabelCounts::try_new(profile).expect("profile is valid");
            let mut expected: BTreeMap<(LabelCountsDomain, Vec<u8>), u64> = BTreeMap::new();

            for node in 0..fixture.node_count {
                // Two label populations with a deterministic, skewed split.
                let name: &[u8] = if node % 3 == 0 { b"Hub" } else { b"Node" };
                counts
                    .try_observe(vertex(name), 1)
                    .expect("vertex label fits");
                *expected
                    .entry((LabelCountsDomain::VertexLabel, name.to_vec()))
                    .or_default() += 1;
            }
            for &(left, right) in &fixture.edges {
                let name: &[u8] = if left < right {
                    b"FORWARD"
                } else {
                    b"BACKWARD"
                };
                counts.try_observe(edge(name), 1).expect("edge type fits");
                *expected
                    .entry((LabelCountsDomain::EdgeType, name.to_vec()))
                    .or_default() += 1;
            }

            let expected_total: u64 = expected.values().sum();
            assert_eq!(counts.total_count(), expected_total, "{}", fixture.name);
            assert_eq!(counts.distinct_keys(), expected.len(), "{}", fixture.name);
            for ((domain, name), count) in &expected {
                assert_eq!(
                    counts.count(LabelCountsKey::new(*domain, name)),
                    *count,
                    "{} {domain} {name:?}",
                    fixture.name
                );
            }

            // Exactness survives deletion: this family never owes a rebuild.
            let removed = expected
                .get(&(LabelCountsDomain::VertexLabel, b"Node".to_vec()))
                .copied()
                .expect("fixture has plain nodes");
            counts
                .try_remove(vertex(b"Node"), removed)
                .expect("exact deletion");
            assert_eq!(counts.count(vertex(b"Node")), 0, "{}", fixture.name);
            assert_eq!(
                counts.total_count(),
                expected_total - removed,
                "{}",
                fixture.name
            );

            let encoded = counts.try_to_canonical_bytes().expect("state encodes");
            let decode_limits = LabelCountsDecodeLimits::new(1 << 20, 16, 32, u64::from(u32::MAX));
            assert_eq!(
                LabelCounts::try_from_canonical_bytes(&encoded, profile, decode_limits)
                    .expect("canonical bytes decode"),
                counts,
                "{}",
                fixture.name
            );
        }
    }

    #[test]
    fn diagnostics_name_the_domain_resource_and_allocation_target() {
        assert_eq!(LabelCountsDomain::VertexLabel.as_str(), "vertex-label");
        assert_eq!(LabelCountsDomain::EdgeType.to_string(), "edge-type");
        assert_eq!(LabelCountsDecodeResource::KeyBytes.to_string(), "key bytes");
        assert_eq!(
            LabelCountsAllocationTarget::MergeDirectory.to_string(),
            "merge directory"
        );
        assert!(
            LabelCountsError::MissingKey {
                domain: LabelCountsDomain::EdgeType,
            }
            .to_string()
            .contains("edge-type")
        );
        assert!(
            LabelCountsCodecError::from(LabelCountsError::ProfileMismatch)
                .to_string()
                .contains("label-count profiles differ")
        );
    }
}
