//! Type-safe scalar identity-column compression.
//!
//! This module is deliberately below durable framing and codec registration.
//! It assigns no tags or codec IDs and defines no wire envelope. The
//! [`IdentityColumn`] chooser compares only the scalar payload represented
//! here: raw 16-byte identities versus an 11-byte sorted prefix dictionary,
//! fixed-width prefix indexes, and 6-byte slots.
//!
//! Row order is always retained. Binary search is exposed only by
//! [`SortedIdentityColumn`], whose constructor validates monotone identity
//! order before wrapping a column.

#![forbid(unsafe_code)]

use core::{fmt, iter::FusedIterator, marker::PhantomData};

use fgdb_types::{EId, VId};

/// Number of bits in the partition component of a vertex or edge identity.
pub const PARTITION_BITS: u32 = 20;
/// Number of bits in the monotone slot component of a vertex or edge identity.
pub const SLOT_BITS: u32 = 44;
/// Largest partition identifier representable by an identity.
pub const MAX_PARTITION: u32 = (1_u32 << PARTITION_BITS) - 1;
/// Largest monotone slot representable by an identity.
pub const MAX_SLOT: u64 = (1_u64 << SLOT_BITS) - 1;

const RAW_ID_BYTES: usize = 16;
const PREFIX_BYTES: usize = 11;
const SLOT_BYTES: usize = 6;

/// Checked decomposition of a `VId` or `EId`.
///
/// The packed order is `(allocation_epoch, partition, monotone_slot)`.
/// Consequently tuple order, packed-`u128` order, and canonical big-endian
/// byte order are identical.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IdentityParts {
    allocation_epoch: u64,
    partition: u32,
    monotone_slot: u64,
}

impl IdentityParts {
    /// Validates and constructs one identity decomposition.
    pub const fn try_new(
        allocation_epoch: u64,
        partition: u32,
        monotone_slot: u64,
    ) -> Result<Self, IdentityPartsError> {
        if partition > MAX_PARTITION {
            return Err(IdentityPartsError::PartitionOutOfRange {
                actual: partition,
                maximum: MAX_PARTITION,
            });
        }
        if monotone_slot > MAX_SLOT {
            return Err(IdentityPartsError::SlotOutOfRange {
                actual: monotone_slot,
                maximum: MAX_SLOT,
            });
        }
        Ok(Self {
            allocation_epoch,
            partition,
            monotone_slot,
        })
    }

    /// Decomposes every possible 128-bit identity.
    #[must_use]
    pub const fn unpack(bits: u128) -> Self {
        Self {
            allocation_epoch: (bits >> 64) as u64,
            partition: ((bits >> SLOT_BITS) as u32) & MAX_PARTITION,
            monotone_slot: (bits as u64) & MAX_SLOT,
        }
    }

    /// Returns the allocation epoch.
    #[must_use]
    pub const fn allocation_epoch(self) -> u64 {
        self.allocation_epoch
    }

    /// Returns the 20-bit partition identifier.
    #[must_use]
    pub const fn partition(self) -> u32 {
        self.partition
    }

    /// Returns the 44-bit monotone slot.
    #[must_use]
    pub const fn monotone_slot(self) -> u64 {
        self.monotone_slot
    }

    /// Packs this decomposition into the identity's canonical `u128`.
    #[must_use]
    pub const fn pack(self) -> u128 {
        (self.allocation_epoch as u128) << 64
            | (self.partition as u128) << SLOT_BITS
            | self.monotone_slot as u128
    }

    /// Returns the order-preserving canonical big-endian key.
    #[must_use]
    pub const fn canonical_be_key(self) -> [u8; RAW_ID_BYTES] {
        self.pack().to_be_bytes()
    }
}

/// Rejected identity component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityPartsError {
    /// A partition needs more than 20 bits.
    PartitionOutOfRange {
        /// Rejected partition.
        actual: u32,
        /// Largest accepted partition.
        maximum: u32,
    },
    /// A monotone slot needs more than 44 bits.
    SlotOutOfRange {
        /// Rejected slot.
        actual: u64,
        /// Largest accepted slot.
        maximum: u64,
    },
}

impl fmt::Display for IdentityPartsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::PartitionOutOfRange { actual, maximum } => {
                write!(formatter, "identity partition {actual} exceeds {maximum}")
            }
            Self::SlotOutOfRange { actual, maximum } => {
                write!(formatter, "identity slot {actual} exceeds {maximum}")
            }
        }
    }
}

impl std::error::Error for IdentityPartsError {}

mod sealed {
    pub trait Sealed {}
}

/// An element identity accepted by [`IdentityColumn`].
///
/// This trait is sealed: only [`VId`] and [`EId`] implement it. In particular,
/// `u128`, `GraphId`, and `BranchId` cannot instantiate an identity column.
///
/// ```compile_fail
/// use fgdb_codec::identity::{IdentityColumn, IdentityColumnLimits};
/// use fgdb_types::GraphId;
///
/// let values = [GraphId(1)];
/// let _ = IdentityColumn::try_new(
///     &values,
///     IdentityColumnLimits::new(1, 1, 16),
/// );
/// ```
pub trait ElementIdentity: sealed::Sealed + Copy + Eq + Ord {
    #[doc(hidden)]
    fn from_identity_bits(bits: u128) -> Self;

    #[doc(hidden)]
    fn identity_bits(self) -> u128;
}

impl sealed::Sealed for VId {}

impl ElementIdentity for VId {
    fn from_identity_bits(bits: u128) -> Self {
        Self(bits)
    }

    fn identity_bits(self) -> u128 {
        self.0
    }
}

impl sealed::Sealed for EId {}

impl ElementIdentity for EId {
    fn from_identity_bits(bits: u128) -> Self {
        Self(bits)
    }

    fn identity_bits(self) -> u128 {
        self.0
    }
}

/// Construction ceilings for one identity column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityColumnLimits {
    max_rows: usize,
    max_prefixes: usize,
    max_payload_bytes: usize,
}

impl IdentityColumnLimits {
    /// Creates exact row, shared-prefix-dictionary, and scalar-payload limits.
    #[must_use]
    pub const fn new(max_rows: usize, max_prefixes: usize, max_payload_bytes: usize) -> Self {
        Self {
            max_rows,
            max_prefixes,
            max_payload_bytes,
        }
    }

    /// Returns the row ceiling.
    #[must_use]
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Returns the largest shared prefix dictionary the caller permits.
    ///
    /// Exceeding this value makes the shared representation unavailable; it
    /// does not reject a column that fits the raw representation.
    #[must_use]
    pub const fn max_prefixes(self) -> usize {
        self.max_prefixes
    }

    /// Returns the scalar-payload byte ceiling.
    #[must_use]
    pub const fn max_payload_bytes(self) -> usize {
        self.max_payload_bytes
    }
}

/// Selected scalar identity representation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IdentityRepresentation {
    /// Sixteen bytes per row.
    Raw128,
    /// Sorted 11-byte prefix dictionary, fixed-width indexes, and 6-byte slots.
    SharedPrefixFixed,
}

/// Internal allocation named by an identity-column failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationTarget {
    /// Raw 128-bit rows.
    RawRows,
    /// Candidate sorted unique prefix dictionary.
    PrefixDictionary,
    /// Fixed-width per-row prefix indexes.
    PrefixIndexes,
    /// Six-byte per-row monotone slots.
    Slots,
    /// Complete registry-independent scalar payload bytes.
    EncodedPayload,
}

/// Checked representation-size calculation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeCalculation {
    /// Sixteen raw bytes per row.
    RawPayload,
    /// Eleven bytes per unique prefix.
    PrefixPayload,
    /// Fixed-width prefix indexes.
    PrefixIndexPayload,
    /// Six bytes per row for slots.
    SlotPayload,
    /// Sum of all shared-prefix components.
    SharedPayload,
    /// Incrementing the distinct-prefix count.
    PrefixCount,
}

/// Constructor invariant whose violation is returned instead of panicking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityConstructionInvariant {
    /// A row prefix was absent from the dictionary built from those rows.
    PrefixDictionaryMembership,
    /// Prefix-index storage was requested with a width outside `0/1/2/4`.
    PrefixIndexWidth,
    /// A dictionary index did not fit the preflight-selected storage width.
    PrefixIndexRange,
    /// Materialized payload bytes disagreed with checked chooser accounting.
    EncodedPayloadLength,
}

/// Checked identity-column construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityColumnError {
    /// The input contains more rows than authorized.
    RowLimitExceeded {
        /// Number of input rows.
        rows: usize,
        /// Caller-selected ceiling.
        limit: usize,
    },
    /// The selected representation exceeds the scalar-payload ceiling.
    PayloadLimitExceeded {
        /// Representation selected by the deterministic chooser.
        representation: IdentityRepresentation,
        /// Exact required scalar-payload bytes.
        required: usize,
        /// Caller-selected ceiling.
        limit: usize,
    },
    /// Neither raw rows nor the theoretical one-prefix shared minimum can fit.
    NoRepresentationFits {
        /// Exact raw scalar payload bytes.
        raw_required: usize,
        /// Smallest possible shared payload for this row count.
        minimum_shared_required: usize,
        /// Caller-selected scalar payload ceiling.
        limit: usize,
    },
    /// Representation-size arithmetic overflowed.
    SizeOverflow {
        /// Stable calculation name.
        calculation: SizeCalculation,
    },
    /// Reserving one bounded representation component failed.
    AllocationFailed {
        /// Component being allocated.
        target: AllocationTarget,
        /// Requested rows, entries, or bytes, according to `target`.
        requested: usize,
    },
    /// A private constructor invariant was violated.
    ConstructionInvariantViolation {
        /// Stable invariant category.
        invariant: IdentityConstructionInvariant,
    },
    /// Input to [`SortedIdentityColumn`] decreased at `index`.
    NotSorted {
        /// First decreasing row.
        index: usize,
        /// Canonical key immediately before `index`.
        previous: [u8; RAW_ID_BYTES],
        /// Canonical key at `index`.
        current: [u8; RAW_ID_BYTES],
    },
}

impl fmt::Display for IdentityColumnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::RowLimitExceeded { rows, limit } => {
                write!(
                    formatter,
                    "identity column has {rows} rows, limit is {limit}"
                )
            }
            Self::PayloadLimitExceeded {
                representation,
                required,
                limit,
            } => write!(
                formatter,
                "{representation:?} identity payload needs {required} bytes, limit is {limit}"
            ),
            Self::NoRepresentationFits {
                raw_required,
                minimum_shared_required,
                limit,
            } => write!(
                formatter,
                "identity payload limit {limit} is below raw size {raw_required} and minimum shared size {minimum_shared_required}"
            ),
            Self::SizeOverflow { calculation } => {
                write!(formatter, "identity {calculation:?} arithmetic overflowed")
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} entries for identity {target:?}"
            ),
            Self::ConstructionInvariantViolation { invariant } => {
                write!(formatter, "identity constructor violated {invariant:?}")
            }
            Self::NotSorted {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "identity column decreases at index {index}: {previous:02x?} then {current:02x?}"
            ),
        }
    }
}

impl std::error::Error for IdentityColumnError {}

/// One canonical `(allocation_epoch, partition)` dictionary key.
///
/// The compact byte array is an in-memory scalar key, not a durable record.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IdentityPrefix {
    bytes: [u8; PREFIX_BYTES],
}

impl IdentityPrefix {
    fn from_parts(parts: IdentityParts) -> Self {
        let epoch = parts.allocation_epoch().to_be_bytes();
        let partition = parts.partition();
        Self {
            bytes: [
                epoch[0],
                epoch[1],
                epoch[2],
                epoch[3],
                epoch[4],
                epoch[5],
                epoch[6],
                epoch[7],
                ((partition >> 16) & 0x0f) as u8,
                (partition >> 8) as u8,
                partition as u8,
            ],
        }
    }

    /// Returns the prefix's allocation epoch.
    #[must_use]
    pub fn allocation_epoch(self) -> u64 {
        u64::from_be_bytes([
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
            self.bytes[5],
            self.bytes[6],
            self.bytes[7],
        ])
    }

    /// Returns the prefix's 20-bit partition.
    #[must_use]
    pub const fn partition(self) -> u32 {
        (self.bytes[8] as u32) << 16 | (self.bytes[9] as u32) << 8 | self.bytes[10] as u32
    }

    fn with_slot(self, slot: u64) -> u128 {
        (self.allocation_epoch() as u128) << 64
            | (self.partition() as u128) << SLOT_BITS
            | slot as u128
    }
}

impl fmt::Debug for IdentityPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityPrefix")
            .field("allocation_epoch", &self.allocation_epoch())
            .field("partition", &self.partition())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PrefixIndexes {
    Zero,
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
}

impl PrefixIndexes {
    const fn width(&self) -> usize {
        match self {
            Self::Zero => 0,
            Self::U8(_) => 1,
            Self::U16(_) => 2,
            Self::U32(_) => 4,
        }
    }

    fn get(&self, row: usize) -> Option<usize> {
        match self {
            Self::Zero => Some(0),
            Self::U8(indexes) => indexes.get(row).map(|index| usize::from(*index)),
            Self::U16(indexes) => indexes.get(row).map(|index| usize::from(*index)),
            Self::U32(indexes) => indexes
                .get(row)
                .and_then(|index| usize::try_from(*index).ok()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum IdentityStorage {
    Raw128(Vec<u128>),
    SharedPrefixFixed {
        prefixes: Vec<IdentityPrefix>,
        indexes: PrefixIndexes,
        slots: Vec<[u8; SLOT_BYTES]>,
    },
}

/// Bounded in-memory identity column retaining arbitrary row order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityColumn<T: ElementIdentity> {
    storage: IdentityStorage,
    encoded_payload_len: usize,
    identity_type: PhantomData<fn() -> T>,
}

impl<T: ElementIdentity> IdentityColumn<T> {
    /// Selects the strictly smaller scalar representation.
    ///
    /// Exact size ties, a caller prefix ceiling, or a dictionary outside the
    /// fixed 0/1/2/4-byte index domain pin the chooser to [`Raw128`].
    /// [`Raw128`]: IdentityRepresentation::Raw128
    pub fn try_new(
        values: &[T],
        limits: IdentityColumnLimits,
    ) -> Result<Self, IdentityColumnError> {
        validate_row_limit(values.len(), limits)?;

        let raw_payload_len = raw_payload_len(values.len())?;
        if values.is_empty() || limits.max_prefixes() == 0 {
            return Self::try_raw(values, raw_payload_len, limits);
        }

        let minimum_shared_payload =
            shared_payload_len(values.len(), 1)?.ok_or(IdentityColumnError::SizeOverflow {
                calculation: SizeCalculation::SharedPayload,
            })?;
        if raw_payload_len > limits.max_payload_bytes()
            && minimum_shared_payload > limits.max_payload_bytes()
        {
            return Err(IdentityColumnError::NoRepresentationFits {
                raw_required: raw_payload_len,
                minimum_shared_required: minimum_shared_payload,
                limit: limits.max_payload_bytes(),
            });
        }

        // Collect once, then sort/deduplicate. Incremental sorted insertion is
        // quadratic for adversarial prefix diversity and would let a bounded
        // row count amplify CPU work before the raw fallback is selected.
        let mut prefixes = Vec::new();
        prefixes.try_reserve_exact(values.len()).map_err(|_| {
            IdentityColumnError::AllocationFailed {
                target: AllocationTarget::PrefixDictionary,
                requested: values.len(),
            }
        })?;
        prefixes.extend(
            values.iter().map(|value| {
                IdentityPrefix::from_parts(IdentityParts::unpack(value.identity_bits()))
            }),
        );
        prefixes.sort_unstable();
        prefixes.dedup();

        if prefixes.len() > limits.max_prefixes() {
            return Self::try_raw(values, raw_payload_len, limits);
        }

        let Some(shared_payload_len) = shared_payload_len(values.len(), prefixes.len())? else {
            return Self::try_raw(values, raw_payload_len, limits);
        };
        if shared_payload_len >= raw_payload_len {
            return Self::try_raw(values, raw_payload_len, limits);
        }
        if shared_payload_len > limits.max_payload_bytes() {
            return Err(IdentityColumnError::PayloadLimitExceeded {
                representation: IdentityRepresentation::SharedPrefixFixed,
                required: shared_payload_len,
                limit: limits.max_payload_bytes(),
            });
        }

        let Some(index_width) = prefix_index_width(prefixes.len()) else {
            return Self::try_raw(values, raw_payload_len, limits);
        };
        let mut indexes = allocate_indexes(index_width, values.len())?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(values.len()).map_err(|_| {
            IdentityColumnError::AllocationFailed {
                target: AllocationTarget::Slots,
                requested: values.len(),
            }
        })?;

        for value in values {
            let parts = IdentityParts::unpack(value.identity_bits());
            let prefix = IdentityPrefix::from_parts(parts);
            let index = prefixes.binary_search(&prefix).map_err(|_| {
                IdentityColumnError::ConstructionInvariantViolation {
                    invariant: IdentityConstructionInvariant::PrefixDictionaryMembership,
                }
            })?;
            push_index(&mut indexes, index)?;
            slots.push(slot_be_bytes(parts.monotone_slot()));
        }

        Ok(Self {
            storage: IdentityStorage::SharedPrefixFixed {
                prefixes,
                indexes,
                slots,
            },
            encoded_payload_len: shared_payload_len,
            identity_type: PhantomData,
        })
    }

    fn try_raw(
        values: &[T],
        raw_payload_len: usize,
        limits: IdentityColumnLimits,
    ) -> Result<Self, IdentityColumnError> {
        if raw_payload_len > limits.max_payload_bytes() {
            return Err(IdentityColumnError::PayloadLimitExceeded {
                representation: IdentityRepresentation::Raw128,
                required: raw_payload_len,
                limit: limits.max_payload_bytes(),
            });
        }

        let mut rows = Vec::new();
        rows.try_reserve_exact(values.len()).map_err(|_| {
            IdentityColumnError::AllocationFailed {
                target: AllocationTarget::RawRows,
                requested: values.len(),
            }
        })?;
        rows.extend(values.iter().map(|value| value.identity_bits()));
        Ok(Self {
            storage: IdentityStorage::Raw128(rows),
            encoded_payload_len: raw_payload_len,
            identity_type: PhantomData,
        })
    }

    /// Returns the number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.storage {
            IdentityStorage::Raw128(rows) => rows.len(),
            IdentityStorage::SharedPrefixFixed { slots, .. } => slots.len(),
        }
    }

    /// Returns whether the column contains no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the selected scalar representation.
    #[must_use]
    pub const fn representation(&self) -> IdentityRepresentation {
        match self.storage {
            IdentityStorage::Raw128(_) => IdentityRepresentation::Raw128,
            IdentityStorage::SharedPrefixFixed { .. } => IdentityRepresentation::SharedPrefixFixed,
        }
    }

    /// Returns the exact scalar-payload size used by the chooser.
    ///
    /// No durable tag, length, checksum, or envelope is included.
    #[must_use]
    pub const fn encoded_payload_len(&self) -> usize {
        self.encoded_payload_len
    }

    /// Materializes the exact registry-independent scalar payload.
    ///
    /// Raw identities and prefix bytes preserve canonical big-endian identity
    /// order. Multi-byte dictionary indexes use little-endian fixed integers,
    /// matching the durable-format baseline. This payload deliberately omits
    /// a representation tag, row/dictionary counts, version, checksum, and
    /// envelope; callers must not treat it as a registered durable encoding.
    pub fn try_scalar_payload(
        &self,
        max_output_bytes: usize,
    ) -> Result<Vec<u8>, IdentityColumnError> {
        if self.encoded_payload_len > max_output_bytes {
            return Err(IdentityColumnError::PayloadLimitExceeded {
                representation: self.representation(),
                required: self.encoded_payload_len,
                limit: max_output_bytes,
            });
        }

        let mut output = Vec::new();
        output
            .try_reserve_exact(self.encoded_payload_len)
            .map_err(|_| IdentityColumnError::AllocationFailed {
                target: AllocationTarget::EncodedPayload,
                requested: self.encoded_payload_len,
            })?;
        match &self.storage {
            IdentityStorage::Raw128(rows) => {
                for row in rows {
                    output.extend_from_slice(&row.to_be_bytes());
                }
            }
            IdentityStorage::SharedPrefixFixed {
                prefixes,
                indexes,
                slots,
            } => {
                for prefix in prefixes {
                    output.extend_from_slice(&prefix.bytes);
                }
                match indexes {
                    PrefixIndexes::Zero => {}
                    PrefixIndexes::U8(values) => output.extend_from_slice(values),
                    PrefixIndexes::U16(values) => {
                        for value in values {
                            output.extend_from_slice(&value.to_le_bytes());
                        }
                    }
                    PrefixIndexes::U32(values) => {
                        for value in values {
                            output.extend_from_slice(&value.to_le_bytes());
                        }
                    }
                }
                for slot in slots {
                    output.extend_from_slice(slot);
                }
            }
        }
        if output.len() != self.encoded_payload_len {
            return Err(IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::EncodedPayloadLength,
            });
        }
        Ok(output)
    }

    /// Returns the sorted unique shared dictionary, or `None` for raw rows.
    #[must_use]
    pub fn prefix_dictionary(&self) -> Option<&[IdentityPrefix]> {
        match &self.storage {
            IdentityStorage::Raw128(_) => None,
            IdentityStorage::SharedPrefixFixed { prefixes, .. } => Some(prefixes),
        }
    }

    /// Returns the fixed prefix-index width, or zero for raw rows.
    #[must_use]
    pub const fn prefix_index_width(&self) -> usize {
        match &self.storage {
            IdentityStorage::Raw128(_) => 0,
            IdentityStorage::SharedPrefixFixed { indexes, .. } => indexes.width(),
        }
    }

    /// Reconstructs one typed identity.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<T> {
        let bits = match &self.storage {
            IdentityStorage::Raw128(rows) => *rows.get(row)?,
            IdentityStorage::SharedPrefixFixed {
                prefixes,
                indexes,
                slots,
            } => {
                let prefix = *prefixes.get(indexes.get(row)?)?;
                let slot = slot_from_be_bytes(*slots.get(row)?);
                prefix.with_slot(slot)
            }
        };
        Some(T::from_identity_bits(bits))
    }

    /// Returns one order-preserving canonical big-endian key.
    #[must_use]
    pub fn canonical_key_at(&self, row: usize) -> Option<[u8; RAW_ID_BYTES]> {
        self.get(row)
            .map(|identity| identity.identity_bits().to_be_bytes())
    }

    fn compare_row_to_parts(
        &self,
        row: usize,
        probe: IdentityParts,
    ) -> Option<core::cmp::Ordering> {
        match &self.storage {
            IdentityStorage::Raw128(rows) => {
                Some(IdentityParts::unpack(*rows.get(row)?).cmp(&probe))
            }
            IdentityStorage::SharedPrefixFixed {
                prefixes,
                indexes,
                slots,
            } => {
                let row_prefix = *prefixes.get(indexes.get(row)?)?;
                let probe_prefix = IdentityPrefix::from_parts(probe);
                let row_slot = slot_from_be_bytes(*slots.get(row)?);
                Some(
                    row_prefix
                        .cmp(&probe_prefix)
                        .then_with(|| row_slot.cmp(&probe.monotone_slot())),
                )
            }
        }
    }

    /// Iterates typed identities in original row order.
    #[must_use]
    pub fn iter(&self) -> IdentityIter<'_, T> {
        IdentityIter {
            column: self,
            row: 0,
        }
    }
}

/// Allocation-free row-order iterator over an [`IdentityColumn`].
#[derive(Clone, Debug)]
pub struct IdentityIter<'a, T: ElementIdentity> {
    column: &'a IdentityColumn<T>,
    row: usize,
}

impl<T: ElementIdentity> Iterator for IdentityIter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let value = self.column.get(self.row)?;
        self.row += 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.column.len() - self.row;
        (remaining, Some(remaining))
    }
}

impl<T: ElementIdentity> ExactSizeIterator for IdentityIter<'_, T> {}
impl<T: ElementIdentity> FusedIterator for IdentityIter<'_, T> {}

/// Identity column whose monotone row order was validated at construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortedIdentityColumn<T: ElementIdentity> {
    column: IdentityColumn<T>,
}

impl<T: ElementIdentity> SortedIdentityColumn<T> {
    /// Validates monotone order, then constructs a bounded identity column.
    pub fn try_new(
        values: &[T],
        limits: IdentityColumnLimits,
    ) -> Result<Self, IdentityColumnError> {
        validate_row_limit(values.len(), limits)?;
        for (offset, pair) in values.windows(2).enumerate() {
            if pair[0] > pair[1] {
                return Err(IdentityColumnError::NotSorted {
                    index: offset + 1,
                    previous: pair[0].identity_bits().to_be_bytes(),
                    current: pair[1].identity_bits().to_be_bytes(),
                });
            }
        }
        Ok(Self {
            column: IdentityColumn::try_new(values, limits)?,
        })
    }

    /// Returns the first row whose identity is not less than `probe`.
    #[must_use]
    pub fn lower_bound(&self, probe: T) -> usize {
        let probe = IdentityParts::unpack(probe.identity_bits());
        let mut low = 0;
        let mut high = self.column.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let ordering = self.column.compare_row_to_parts(middle, probe);
            debug_assert!(ordering.is_some(), "validated column lost an in-range row");
            if ordering == Some(core::cmp::Ordering::Less) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// Returns the validated column.
    #[must_use]
    pub const fn as_column(&self) -> &IdentityColumn<T> {
        &self.column
    }

    /// Returns the number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.column.len()
    }

    /// Returns whether the column contains no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.column.is_empty()
    }

    /// Reconstructs one typed identity.
    #[must_use]
    pub fn get(&self, row: usize) -> Option<T> {
        self.column.get(row)
    }

    /// Iterates typed identities in validated sorted order.
    #[must_use]
    pub fn iter(&self) -> IdentityIter<'_, T> {
        self.column.iter()
    }

    /// Consumes the sorted proof wrapper.
    #[must_use]
    pub fn into_inner(self) -> IdentityColumn<T> {
        self.column
    }
}

fn validate_row_limit(
    rows: usize,
    limits: IdentityColumnLimits,
) -> Result<(), IdentityColumnError> {
    if rows > limits.max_rows() {
        return Err(IdentityColumnError::RowLimitExceeded {
            rows,
            limit: limits.max_rows(),
        });
    }
    Ok(())
}

fn raw_payload_len(rows: usize) -> Result<usize, IdentityColumnError> {
    rows.checked_mul(RAW_ID_BYTES)
        .ok_or(IdentityColumnError::SizeOverflow {
            calculation: SizeCalculation::RawPayload,
        })
}

fn shared_payload_len(rows: usize, prefixes: usize) -> Result<Option<usize>, IdentityColumnError> {
    let Some(index_width) = prefix_index_width(prefixes) else {
        return Ok(None);
    };
    let prefix_bytes =
        prefixes
            .checked_mul(PREFIX_BYTES)
            .ok_or(IdentityColumnError::SizeOverflow {
                calculation: SizeCalculation::PrefixPayload,
            })?;
    let index_bytes = rows
        .checked_mul(index_width)
        .ok_or(IdentityColumnError::SizeOverflow {
            calculation: SizeCalculation::PrefixIndexPayload,
        })?;
    let slot_bytes = rows
        .checked_mul(SLOT_BYTES)
        .ok_or(IdentityColumnError::SizeOverflow {
            calculation: SizeCalculation::SlotPayload,
        })?;
    let payload = prefix_bytes
        .checked_add(index_bytes)
        .and_then(|partial| partial.checked_add(slot_bytes))
        .ok_or(IdentityColumnError::SizeOverflow {
            calculation: SizeCalculation::SharedPayload,
        })?;
    Ok(Some(payload))
}

const fn prefix_index_width(prefixes: usize) -> Option<usize> {
    match prefixes {
        0 | 1 => Some(0),
        2..=256 => Some(1),
        257..=65_536 => Some(2),
        _ => {
            let maximum_index = prefixes - 1;
            if maximum_index <= u32::MAX as usize {
                Some(4)
            } else {
                None
            }
        }
    }
}

fn allocate_indexes(width: usize, rows: usize) -> Result<PrefixIndexes, IdentityColumnError> {
    match width {
        0 => Ok(PrefixIndexes::Zero),
        1 => {
            let mut indexes = Vec::new();
            reserve_indexes(&mut indexes, rows)?;
            Ok(PrefixIndexes::U8(indexes))
        }
        2 => {
            let mut indexes = Vec::new();
            reserve_indexes(&mut indexes, rows)?;
            Ok(PrefixIndexes::U16(indexes))
        }
        4 => {
            let mut indexes = Vec::new();
            reserve_indexes(&mut indexes, rows)?;
            Ok(PrefixIndexes::U32(indexes))
        }
        _ => Err(IdentityColumnError::ConstructionInvariantViolation {
            invariant: IdentityConstructionInvariant::PrefixIndexWidth,
        }),
    }
}

fn reserve_indexes<I>(indexes: &mut Vec<I>, rows: usize) -> Result<(), IdentityColumnError> {
    indexes
        .try_reserve_exact(rows)
        .map_err(|_| IdentityColumnError::AllocationFailed {
            target: AllocationTarget::PrefixIndexes,
            requested: rows,
        })
}

fn push_index(indexes: &mut PrefixIndexes, index: usize) -> Result<(), IdentityColumnError> {
    match indexes {
        PrefixIndexes::Zero if index == 0 => {}
        PrefixIndexes::Zero => {
            return Err(IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            });
        }
        PrefixIndexes::U8(values) => values.push(u8::try_from(index).map_err(|_| {
            IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            }
        })?),
        PrefixIndexes::U16(values) => values.push(u16::try_from(index).map_err(|_| {
            IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            }
        })?),
        PrefixIndexes::U32(values) => values.push(u32::try_from(index).map_err(|_| {
            IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            }
        })?),
    }
    Ok(())
}

const fn slot_be_bytes(slot: u64) -> [u8; SLOT_BYTES] {
    [
        (slot >> 40) as u8,
        (slot >> 32) as u8,
        (slot >> 24) as u8,
        (slot >> 16) as u8,
        (slot >> 8) as u8,
        slot as u8,
    ]
}

const fn slot_from_be_bytes(bytes: [u8; SLOT_BYTES]) -> u64 {
    (bytes[0] as u64) << 40
        | (bytes[1] as u64) << 32
        | (bytes[2] as u64) << 24
        | (bytes[3] as u64) << 16
        | (bytes[4] as u64) << 8
        | bytes[5] as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(epoch: u64, partition: u32, slot: u64) -> IdentityParts {
        IdentityParts::try_new(epoch, partition, slot).unwrap()
    }

    fn vid(epoch: u64, partition: u32, slot: u64) -> VId {
        VId(parts(epoch, partition, slot).pack())
    }

    fn eid(epoch: u64, partition: u32, slot: u64) -> EId {
        EId(parts(epoch, partition, slot).pack())
    }

    fn limits(rows: usize, prefixes: usize) -> IdentityColumnLimits {
        IdentityColumnLimits::new(rows, prefixes, usize::MAX)
    }

    fn vids_with_prefix_count(rows: usize, prefix_count: usize) -> Vec<VId> {
        assert!(prefix_count > 0);
        assert!(prefix_count <= rows);
        (0..rows)
            .map(|row| {
                let prefix = row % prefix_count;
                vid(
                    u64::try_from(prefix).unwrap(),
                    u32::try_from(prefix & MAX_PARTITION as usize).unwrap(),
                    u64::try_from(row / prefix_count).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn parts_pin_bit_boundaries_and_reject_wider_components() {
        let minimum = parts(0, 0, 0);
        assert_eq!(minimum.pack(), 0);
        assert_eq!(IdentityParts::unpack(0), minimum);

        let maximum = parts(u64::MAX, MAX_PARTITION, MAX_SLOT);
        assert_eq!(maximum.pack(), u128::MAX);
        assert_eq!(IdentityParts::unpack(u128::MAX), maximum);
        assert_eq!(maximum.canonical_be_key(), [u8::MAX; 16]);

        assert_eq!(
            IdentityParts::try_new(0, MAX_PARTITION + 1, 0),
            Err(IdentityPartsError::PartitionOutOfRange {
                actual: MAX_PARTITION + 1,
                maximum: MAX_PARTITION,
            })
        );
        assert_eq!(
            IdentityParts::try_new(0, 0, MAX_SLOT + 1),
            Err(IdentityPartsError::SlotOutOfRange {
                actual: MAX_SLOT + 1,
                maximum: MAX_SLOT,
            })
        );
    }

    #[test]
    fn boundary_tuple_packed_and_big_endian_orders_are_equivalent() {
        let epochs = [0, 1, u64::MAX];
        let partitions = [0, 1, MAX_PARTITION];
        let slots = [0, 1, MAX_SLOT];
        let mut all = Vec::new();
        for epoch in epochs {
            for partition in partitions {
                for slot in slots {
                    all.push(parts(epoch, partition, slot));
                }
            }
        }

        for left in &all {
            for right in &all {
                let tuple_order = (
                    left.allocation_epoch(),
                    left.partition(),
                    left.monotone_slot(),
                )
                    .cmp(&(
                        right.allocation_epoch(),
                        right.partition(),
                        right.monotone_slot(),
                    ));
                assert_eq!(left.pack().cmp(&right.pack()), tuple_order);
                assert_eq!(
                    left.canonical_be_key().cmp(&right.canonical_be_key()),
                    tuple_order
                );
                assert_eq!(IdentityParts::unpack(left.pack()), *left);
            }
        }
    }

    #[test]
    fn contiguous_small_domain_exhaustively_preserves_total_order() {
        let mut identities = Vec::new();
        for epoch in 0..4 {
            for partition in 0..8 {
                for slot in 0..16 {
                    identities.push(parts(epoch, partition, slot));
                }
            }
        }

        for (left_index, left) in identities.iter().enumerate() {
            for (right_index, right) in identities.iter().enumerate() {
                assert_eq!(
                    left.cmp(right),
                    left.pack().cmp(&right.pack()),
                    "packed order differs at ({left_index}, {right_index})"
                );
                assert_eq!(
                    left.cmp(right),
                    left.canonical_be_key().cmp(&right.canonical_be_key()),
                    "big-endian order differs at ({left_index}, {right_index})"
                );
            }
        }
    }

    #[test]
    fn seeded_large_mixed_prefix_order_and_lower_bound_match_slice_oracle() {
        let prefixes = (0_u64..32)
            .map(|prefix| (prefix.wrapping_mul(0x9e37_79b9), prefix as u32 * 17))
            .collect::<Vec<_>>();
        let mut state = 0xd1b5_4a32_d192_ed03_u64;
        let mut values = Vec::new();
        for row in 0..4_096 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let (epoch, partition) = prefixes[row % prefixes.len()];
            values.push(vid(epoch, partition, state & MAX_SLOT));
        }
        values.sort_unstable();

        let sorted = SortedIdentityColumn::try_new(
            &values,
            IdentityColumnLimits::new(values.len(), prefixes.len(), usize::MAX),
        )
        .unwrap();
        assert_eq!(
            sorted.as_column().representation(),
            IdentityRepresentation::SharedPrefixFixed
        );
        assert_eq!(sorted.iter().collect::<Vec<_>>(), values);
        assert!(
            values
                .windows(2)
                .all(|pair| pair[0].0.to_be_bytes() <= pair[1].0.to_be_bytes())
        );

        for probe_index in 0..1_024 {
            state = state
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            let (epoch, partition) = prefixes[probe_index % prefixes.len()];
            let probe = vid(epoch, partition, state & MAX_SLOT);
            assert_eq!(
                sorted.lower_bound(probe),
                values.partition_point(|value| *value < probe)
            );
        }
    }

    #[test]
    fn typed_columns_round_trip_arbitrary_order_and_duplicates() {
        let vertices = [
            vid(7, 3, 9),
            vid(1, 2, 8),
            vid(7, 3, 9),
            vid(7, 3, 1),
            vid(1, 2, 8),
            vid(9, 4, 0),
            vid(7, 3, 2),
            vid(1, 2, 7),
            vid(9, 4, 1),
            vid(7, 3, 3),
            vid(1, 2, 6),
            vid(9, 4, 2),
            vid(7, 3, 4),
            vid(1, 2, 5),
            vid(9, 4, 3),
            vid(7, 3, 5),
        ];
        let vertex_column = IdentityColumn::try_new(&vertices, limits(vertices.len(), 3)).unwrap();
        assert_eq!(vertex_column.iter().collect::<Vec<_>>(), vertices.to_vec());
        for (row, value) in vertices.iter().enumerate() {
            assert_eq!(vertex_column.get(row), Some(*value));
            assert_eq!(
                vertex_column.canonical_key_at(row),
                Some(value.0.to_be_bytes())
            );
        }
        assert_eq!(vertex_column.get(vertices.len()), None);

        let edges = [eid(11, 5, 8), eid(11, 5, 1), eid(11, 5, 8), eid(11, 5, 2)];
        let edge_column = IdentityColumn::try_new(&edges, limits(edges.len(), 1)).unwrap();
        assert_eq!(edge_column.iter().collect::<Vec<_>>(), edges);
    }

    #[test]
    fn shared_dictionary_is_sorted_unique_and_indexes_reconstruct_rows() {
        let values = [
            vid(9, 7, 3),
            vid(1, 5, 4),
            vid(9, 7, 2),
            vid(4, 2, 8),
            vid(1, 5, 1),
            vid(4, 2, 9),
            vid(9, 7, 0),
            vid(1, 5, 2),
            vid(4, 2, 7),
            vid(9, 7, 1),
            vid(1, 5, 3),
            vid(4, 2, 6),
            vid(9, 7, 4),
            vid(1, 5, 0),
            vid(4, 2, 5),
            vid(9, 7, 5),
        ];
        let column = IdentityColumn::try_new(&values, limits(values.len(), 3)).unwrap();
        assert_eq!(
            column.representation(),
            IdentityRepresentation::SharedPrefixFixed
        );
        let dictionary = column.prefix_dictionary().unwrap();
        assert_eq!(dictionary.len(), 3);
        assert!(dictionary.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            dictionary
                .iter()
                .map(|prefix| (prefix.allocation_epoch(), prefix.partition()))
                .collect::<Vec<_>>(),
            vec![(1, 5), (4, 2), (9, 7)]
        );
        assert_eq!(column.prefix_index_width(), 1);
        for (row, value) in values.iter().enumerate() {
            assert_eq!(column.get(row), Some(*value));
            assert_eq!(column.canonical_key_at(row), Some(value.0.to_be_bytes()));
        }
    }

    #[test]
    fn chooser_pins_exact_boundaries_and_raw_fallbacks() {
        let n1_d1 = vids_with_prefix_count(1, 1);
        let column = IdentityColumn::try_new(&n1_d1, limits(1, 1)).unwrap();
        assert_eq!(column.representation(), IdentityRepresentation::Raw128);
        assert_eq!(column.encoded_payload_len(), 16);

        let n256_d1 = vids_with_prefix_count(256, 1);
        let column = IdentityColumn::try_new(&n256_d1, limits(256, 1)).unwrap();
        assert_eq!(
            column.representation(),
            IdentityRepresentation::SharedPrefixFixed
        );
        assert_eq!(column.encoded_payload_len(), 1_547);
        assert_eq!(column.prefix_index_width(), 0);

        let n256_d209 = vids_with_prefix_count(256, 209);
        let column = IdentityColumn::try_new(&n256_d209, limits(256, 209)).unwrap();
        assert_eq!(
            column.representation(),
            IdentityRepresentation::SharedPrefixFixed
        );
        assert_eq!(column.encoded_payload_len(), 4_091);
        assert_eq!(column.prefix_index_width(), 1);

        let n256_d210 = vids_with_prefix_count(256, 210);
        let column = IdentityColumn::try_new(&n256_d210, limits(256, 210)).unwrap();
        assert_eq!(column.representation(), IdentityRepresentation::Raw128);
        assert_eq!(column.encoded_payload_len(), 4_096);

        let n11_d9 = vids_with_prefix_count(11, 9);
        assert_eq!(shared_payload_len(11, 9).unwrap(), Some(176));
        let column = IdentityColumn::try_new(&n11_d9, limits(11, 9)).unwrap();
        assert_eq!(column.representation(), IdentityRepresentation::Raw128);
        assert_eq!(column.encoded_payload_len(), 176);

        let all_distinct = vids_with_prefix_count(256, 256);
        let column = IdentityColumn::try_new(&all_distinct, limits(256, 256)).unwrap();
        assert_eq!(column.representation(), IdentityRepresentation::Raw128);

        let prefix_limited = IdentityColumn::try_new(&n256_d1, limits(256, 0)).unwrap();
        assert_eq!(
            prefix_limited.representation(),
            IdentityRepresentation::Raw128
        );
    }

    #[test]
    fn index_width_accounting_uses_only_zero_one_two_or_four_bytes() {
        assert_eq!(prefix_index_width(0), Some(0));
        assert_eq!(prefix_index_width(1), Some(0));
        assert_eq!(prefix_index_width(2), Some(1));
        assert_eq!(prefix_index_width(256), Some(1));
        assert_eq!(prefix_index_width(257), Some(2));
        assert_eq!(prefix_index_width(65_536), Some(2));
        assert_eq!(prefix_index_width(65_537), Some(4));
        assert_eq!(
            shared_payload_len(1_024, 257).unwrap(),
            Some(257 * 11 + 1_024 * 2 + 1_024 * 6)
        );
        assert_eq!(
            allocate_indexes(3, 0),
            Err(IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexWidth,
            })
        );

        let mut zero_width = PrefixIndexes::Zero;
        assert_eq!(
            push_index(&mut zero_width, 1),
            Err(IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            })
        );
        let mut one_byte = PrefixIndexes::U8(Vec::new());
        assert_eq!(
            push_index(&mut one_byte, 256),
            Err(IdentityColumnError::ConstructionInvariantViolation {
                invariant: IdentityConstructionInvariant::PrefixIndexRange,
            })
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn dictionary_outside_u32_index_domain_is_unavailable() {
        let first_unavailable = u32::MAX as usize + 2;
        assert_eq!(prefix_index_width(first_unavailable), None);
        assert_eq!(shared_payload_len(1, first_unavailable).unwrap(), None);
    }

    #[test]
    fn diversity_curve_reaches_an_honest_raw_plateau() {
        let mut payloads = Vec::new();
        for prefixes in 1..=256 {
            let values = vids_with_prefix_count(256, prefixes);
            let column = IdentityColumn::try_new(&values, limits(values.len(), prefixes)).unwrap();
            payloads.push(column.encoded_payload_len());
        }
        assert_eq!(payloads[208], 4_091);
        assert!(payloads[..209].windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(payloads[209..].iter().all(|payload| *payload == 4_096));
    }

    #[test]
    fn sorted_wrapper_lower_bound_matches_slice_oracle_with_duplicates() {
        let values = vec![
            vid(5, 7, 1),
            vid(5, 7, 1),
            vid(5, 7, 3),
            vid(5, 7, 8),
            vid(5, 7, 8),
            vid(5, 7, 13),
            vid(5, 7, 21),
            vid(5, 7, 21),
            vid(5, 7, 34),
            vid(5, 7, 55),
            vid(5, 7, 89),
            vid(5, 7, 144),
        ];
        let sorted = SortedIdentityColumn::try_new(&values, limits(values.len(), 1)).unwrap();
        for probe_slot in 0..=145 {
            let probe = vid(5, 7, probe_slot);
            let expected = values.partition_point(|value| *value < probe);
            assert_eq!(sorted.lower_bound(probe), expected);
        }
        assert_eq!(sorted.iter().collect::<Vec<_>>(), values);

        let unsorted = [vid(1, 0, 2), vid(1, 0, 1)];
        assert!(matches!(
            SortedIdentityColumn::try_new(&unsorted, limits(2, 1)),
            Err(IdentityColumnError::NotSorted { index: 1, .. })
        ));
        assert_eq!(
            SortedIdentityColumn::try_new(&unsorted, limits(1, 1)),
            Err(IdentityColumnError::RowLimitExceeded { rows: 2, limit: 1 }),
            "resource ceiling must reject before scanning order"
        );
    }

    #[test]
    fn limits_and_checked_size_fail_before_representation_publication() {
        let values = vids_with_prefix_count(256, 1);
        assert_eq!(
            IdentityColumn::try_new(&values, IdentityColumnLimits::new(255, 1, usize::MAX)),
            Err(IdentityColumnError::RowLimitExceeded {
                rows: 256,
                limit: 255,
            })
        );
        assert_eq!(
            IdentityColumn::try_new(&values, IdentityColumnLimits::new(256, 1, 1_546)),
            Err(IdentityColumnError::NoRepresentationFits {
                raw_required: 4_096,
                minimum_shared_required: 1_547,
                limit: 1_546,
            })
        );

        let raw = vids_with_prefix_count(1, 1);
        assert_eq!(
            IdentityColumn::try_new(&raw, IdentityColumnLimits::new(1, 1, 15)),
            Err(IdentityColumnError::NoRepresentationFits {
                raw_required: 16,
                minimum_shared_required: 17,
                limit: 15,
            })
        );

        let many = vids_with_prefix_count(4_096, 1);
        assert_eq!(
            IdentityColumn::try_new(&many, IdentityColumnLimits::new(many.len(), 1, 0)),
            Err(IdentityColumnError::NoRepresentationFits {
                raw_required: many.len() * RAW_ID_BYTES,
                minimum_shared_required: PREFIX_BYTES + many.len() * SLOT_BYTES,
                limit: 0,
            }),
            "payload impossibility must fail before prefix scratch allocation"
        );
        assert_eq!(
            raw_payload_len(usize::MAX),
            Err(IdentityColumnError::SizeOverflow {
                calculation: SizeCalculation::RawPayload,
            })
        );
        assert_eq!(
            shared_payload_len(usize::MAX / SLOT_BYTES + 1, 1),
            Err(IdentityColumnError::SizeOverflow {
                calculation: SizeCalculation::SlotPayload,
            })
        );
    }

    #[test]
    fn scalar_payload_materializes_exact_accounted_bytes_under_a_bound() {
        let raw_values = [vid(9, 7, 3), vid(1, 5, 4)];
        let raw = IdentityColumn::try_new(
            &raw_values,
            IdentityColumnLimits::new(raw_values.len(), 0, usize::MAX),
        )
        .unwrap();
        let mut expected_raw = Vec::new();
        for value in raw_values {
            expected_raw.extend_from_slice(&value.0.to_be_bytes());
        }
        assert_eq!(
            raw.try_scalar_payload(expected_raw.len()),
            Ok(expected_raw.clone())
        );
        assert_eq!(
            raw.try_scalar_payload(expected_raw.len() - 1),
            Err(IdentityColumnError::PayloadLimitExceeded {
                representation: IdentityRepresentation::Raw128,
                required: expected_raw.len(),
                limit: expected_raw.len() - 1,
            })
        );

        let shared_values = vids_with_prefix_count(256, 3);
        let shared =
            IdentityColumn::try_new(&shared_values, limits(shared_values.len(), 3)).unwrap();
        let first = shared
            .try_scalar_payload(shared.encoded_payload_len())
            .unwrap();
        let second = shared
            .try_scalar_payload(shared.encoded_payload_len())
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), shared.encoded_payload_len());
        assert_eq!(
            &first[..PREFIX_BYTES],
            &shared.prefix_dictionary().unwrap()[0].bytes
        );
    }

    #[test]
    fn representative_and_adversarial_edge_accounting_is_honest() {
        let representative = vids_with_prefix_count(256, 1);
        let sources =
            IdentityColumn::try_new(&representative, limits(representative.len(), 1)).unwrap();
        let destinations =
            IdentityColumn::try_new(&representative, limits(representative.len(), 1)).unwrap();
        let representative_bytes =
            sources.encoded_payload_len() + destinations.encoded_payload_len();
        assert_eq!(representative_bytes, 3_094);
        assert_eq!((representative_bytes, 256), (3_094, 256));

        let adversarial = vids_with_prefix_count(256, 256);
        let sources =
            IdentityColumn::try_new(&adversarial, limits(adversarial.len(), 256)).unwrap();
        let destinations =
            IdentityColumn::try_new(&adversarial, limits(adversarial.len(), 256)).unwrap();
        let adversarial_bytes = sources.encoded_payload_len() + destinations.encoded_payload_len();
        assert_eq!(adversarial_bytes, 8_192);
        assert_eq!(adversarial_bytes / 256, 32);
    }
}
