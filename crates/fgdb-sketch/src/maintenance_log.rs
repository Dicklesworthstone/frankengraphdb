//! Canonical sketch-maintenance history.
//!
//! Sketches are derived state, so this stream never authorizes graph data.
//! It records enough identity-bound evidence to reproduce which canonical
//! sketch state was merged or deliberately left unchanged pending a rebuild.

use core::fmt;

use fgdb_types::ObjectId;

/// Canonical record encoding version.
pub const SKETCH_MAINTENANCE_RECORD_VERSION: u16 = 1;

/// Canonical bounded-log encoding version.
pub const SKETCH_MAINTENANCE_LOG_VERSION: u16 = 1;

/// Absolute record-count ceiling for one in-memory maintenance log.
pub const MAX_SKETCH_MAINTENANCE_RECORDS: usize = 1_048_576;

const RECORD_MAGIC: [u8; 8] = *b"FGDBSMR1";
const LOG_MAGIC: [u8; 8] = *b"FGDBSML1";
const RECORD_RESERVED: u32 = 0;
const LOG_RESERVED: u16 = 0;
const RECORD_BYTES: usize = 184;
const LOG_HEADER_BYTES: usize = 20;
const STATE_DIGEST_DOMAIN: &[u8] = b"fgdb:sketch-maintenance:state:v1";
const INPUT_DIGEST_DOMAIN: &[u8] = b"fgdb:sketch-maintenance:input:v1";

/// Unkeyed digest of one canonical sketch state.
///
/// This is drift-detection evidence, not an authoritative content address.
/// In particular, it is deliberately not interchangeable with [`ObjectId`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SketchStateDigest([u8; 32]);

impl SketchStateDigest {
    /// Hashes one complete canonical sketch state.
    #[must_use]
    pub fn from_canonical_state(canonical_state: &[u8]) -> Self {
        Self(domain_separated_digest(
            STATE_DIGEST_DOMAIN,
            canonical_state,
        ))
    }

    /// Exact digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Unkeyed digest of a canonical maintenance input.
///
/// For a merge this names the canonical operand state. For a rebuild request
/// it names the canonical deletion input which could not safely be applied.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SketchInputDigest([u8; 32]);

impl SketchInputDigest {
    /// Hashes one complete canonical maintenance input.
    #[must_use]
    pub fn from_canonical_input(canonical_input: &[u8]) -> Self {
        Self(domain_separated_digest(
            INPUT_DIGEST_DOMAIN,
            canonical_input,
        ))
    }

    /// Exact digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Closed vocabulary of maintained sketch families.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SketchFamily {
    /// Exact small-window quantiles.
    ExactQuantiles = 1,
    /// Exact degree histogram.
    DegreeHistogram = 2,
    /// Count-Min frequency sketch.
    CountMin = 3,
    /// Bottom-k sample.
    BottomK = 4,
    /// Distinct-count sketch.
    Distinct = 5,
    /// Ordered-value zone map.
    ZoneMap = 6,
    /// Sampled two-edge path correlation table.
    PathCorrelation = 7,
    /// Exact label and edge-type counts.
    LabelCounts = 8,
}

impl SketchFamily {
    const fn from_tag(tag: u8) -> Result<Self, SketchMaintenanceCodecError> {
        match tag {
            1 => Ok(Self::ExactQuantiles),
            2 => Ok(Self::DegreeHistogram),
            3 => Ok(Self::CountMin),
            4 => Ok(Self::BottomK),
            5 => Ok(Self::Distinct),
            6 => Ok(Self::ZoneMap),
            7 => Ok(Self::PathCorrelation),
            8 => Ok(Self::LabelCounts),
            _ => Err(SketchMaintenanceCodecError::UnknownSketchFamily { tag }),
        }
    }
}

/// Result of one derived-state maintenance operation.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SketchMaintenanceOutcome {
    /// Two canonical sketch states were merged.
    Merged = 1,
    /// The input could not be applied exactly; state stayed unchanged and a
    /// rebuild is required before the derived summary can be trusted.
    RebuildRequired = 2,
}

impl SketchMaintenanceOutcome {
    const fn from_tag(tag: u8) -> Result<Self, SketchMaintenanceCodecError> {
        match tag {
            1 => Ok(Self::Merged),
            2 => Ok(Self::RebuildRequired),
            _ => Err(SketchMaintenanceCodecError::UnknownMaintenanceOutcome { tag }),
        }
    }
}

/// One identity-bound sketch-maintenance event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SketchMaintenanceRecord {
    /// Gap-free sequence within this log, beginning at zero.
    pub sequence: u64,
    /// Sketch representation maintained by this event.
    pub family: SketchFamily,
    /// Whether the input was applied or instead requires a rebuild.
    pub outcome: SketchMaintenanceOutcome,
    /// Authoritative identity of the complete sketch profile.
    pub sketch_profile_oid: ObjectId,
    /// Authoritative identity of the logical operation which caused the event.
    pub operation_oid: ObjectId,
    /// Canonical state before maintenance.
    pub before_digest: SketchStateDigest,
    /// Canonical merge operand or unapplied deletion input.
    pub input_digest: SketchInputDigest,
    /// Canonical state after maintenance.
    pub after_digest: SketchStateDigest,
}

impl SketchMaintenanceRecord {
    /// Records a successfully merged canonical sketch state.
    #[must_use]
    pub fn merged(
        sequence: u64,
        family: SketchFamily,
        sketch_profile_oid: ObjectId,
        operation_oid: ObjectId,
        before_state: &[u8],
        operand_state: &[u8],
        after_state: &[u8],
    ) -> Self {
        Self {
            sequence,
            family,
            outcome: SketchMaintenanceOutcome::Merged,
            sketch_profile_oid,
            operation_oid,
            before_digest: SketchStateDigest::from_canonical_state(before_state),
            input_digest: SketchInputDigest::from_canonical_input(operand_state),
            after_digest: SketchStateDigest::from_canonical_state(after_state),
        }
    }

    /// Records an unapplied deletion that requires rebuilding derived state.
    ///
    /// The after digest is derived from the same bytes as the before digest,
    /// making the no-mutation guarantee structural rather than caller supplied.
    #[must_use]
    pub fn rebuild_required(
        sequence: u64,
        family: SketchFamily,
        sketch_profile_oid: ObjectId,
        operation_oid: ObjectId,
        unchanged_state: &[u8],
        canonical_deletion: &[u8],
    ) -> Self {
        let unchanged_digest = SketchStateDigest::from_canonical_state(unchanged_state);
        Self {
            sequence,
            family,
            outcome: SketchMaintenanceOutcome::RebuildRequired,
            sketch_profile_oid,
            operation_oid,
            before_digest: unchanged_digest,
            input_digest: SketchInputDigest::from_canonical_input(canonical_deletion),
            after_digest: unchanged_digest,
        }
    }

    /// Encodes this record into its fixed-width canonical form.
    #[must_use]
    pub fn to_canonical_bytes(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0_u8; RECORD_BYTES];
        bytes[..8].copy_from_slice(&RECORD_MAGIC);
        bytes[8..10].copy_from_slice(&SKETCH_MAINTENANCE_RECORD_VERSION.to_le_bytes());
        bytes[10] = self.family as u8;
        bytes[11] = self.outcome as u8;
        bytes[12..16].copy_from_slice(&RECORD_RESERVED.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[24..56].copy_from_slice(self.sketch_profile_oid.as_bytes());
        bytes[56..88].copy_from_slice(self.operation_oid.as_bytes());
        bytes[88..120].copy_from_slice(self.before_digest.as_bytes());
        bytes[120..152].copy_from_slice(self.input_digest.as_bytes());
        bytes[152..184].copy_from_slice(self.after_digest.as_bytes());
        bytes
    }

    /// Decodes one exact canonical record.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, SketchMaintenanceCodecError> {
        if bytes.len() != RECORD_BYTES {
            return Err(SketchMaintenanceCodecError::RecordLength {
                expected: RECORD_BYTES,
                actual: bytes.len(),
            });
        }
        if bytes[..8] != RECORD_MAGIC {
            return Err(SketchMaintenanceCodecError::RecordMagic);
        }

        let version = read_u16(bytes, 8);
        if version != SKETCH_MAINTENANCE_RECORD_VERSION {
            return Err(SketchMaintenanceCodecError::RecordVersion { version });
        }
        let reserved = read_u32(bytes, 12);
        if reserved != RECORD_RESERVED {
            return Err(SketchMaintenanceCodecError::RecordReserved { reserved });
        }

        let record = Self {
            family: SketchFamily::from_tag(bytes[10])?,
            outcome: SketchMaintenanceOutcome::from_tag(bytes[11])?,
            sequence: read_u64(bytes, 16),
            sketch_profile_oid: ObjectId(read_array_32(bytes, 24)),
            operation_oid: ObjectId(read_array_32(bytes, 56)),
            before_digest: SketchStateDigest(read_array_32(bytes, 88)),
            input_digest: SketchInputDigest(read_array_32(bytes, 120)),
            after_digest: SketchStateDigest(read_array_32(bytes, 152)),
        };
        record.validate_outcome()?;
        Ok(record)
    }

    fn validate_outcome(self) -> Result<(), SketchMaintenanceCodecError> {
        if self.outcome == SketchMaintenanceOutcome::RebuildRequired
            && self
                .before_digest
                .as_bytes()
                .cmp(self.after_digest.as_bytes())
                .is_ne()
        {
            return Err(SketchMaintenanceCodecError::RebuildMutatedState);
        }
        Ok(())
    }
}

/// Caller-owned admission bounds for a canonical maintenance log.
///
/// These limits are independent of the encoded profile so untrusted bytes
/// cannot authorize their own allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SketchMaintenanceLogDecodeLimits {
    /// Largest encoded profile or concrete record count accepted.
    pub max_records: usize,
    /// Largest complete canonical log accepted.
    pub max_encoded_bytes: usize,
}

impl SketchMaintenanceLogDecodeLimits {
    /// Creates an explicit decode admission policy.
    #[must_use]
    pub const fn new(max_records: usize, max_encoded_bytes: usize) -> Self {
        Self {
            max_records,
            max_encoded_bytes,
        }
    }
}

/// Bounded, gap-free sketch-maintenance stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SketchMaintenanceLog {
    max_records: usize,
    records: Vec<SketchMaintenanceRecord>,
}

impl SketchMaintenanceLog {
    /// Creates an empty log with a durable maximum-record profile.
    pub fn new(max_records: usize) -> Result<Self, SketchMaintenanceLogError> {
        validate_profile(max_records)?;
        Ok(Self {
            max_records,
            records: Vec::new(),
        })
    }

    /// Durable maximum-record profile.
    #[must_use]
    pub const fn max_records(&self) -> usize {
        self.max_records
    }

    /// Ordered records.
    #[must_use]
    pub fn records(&self) -> &[SketchMaintenanceRecord] {
        &self.records
    }

    /// Appends the next gap-free event.
    pub fn append(
        &mut self,
        record: SketchMaintenanceRecord,
    ) -> Result<(), SketchMaintenanceLogError> {
        if self.records.len() >= self.max_records {
            return Err(SketchMaintenanceLogError::CapacityExceeded {
                max_records: self.max_records,
            });
        }
        let expected = u64::try_from(self.records.len())
            .map_err(|_| SketchMaintenanceLogError::SequenceOverflow)?;
        if record.sequence != expected {
            return Err(SketchMaintenanceLogError::Sequence {
                expected,
                actual: record.sequence,
            });
        }
        record
            .validate_outcome()
            .map_err(SketchMaintenanceLogError::InvalidRecord)?;
        self.records.push(record);
        Ok(())
    }

    /// Encodes the entire bounded stream canonically.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, SketchMaintenanceCodecError> {
        let encoded_len = encoded_log_len(self.records.len())?;
        let encoded_profile = u32::try_from(self.max_records).map_err(|_| {
            SketchMaintenanceCodecError::ProfileTooLarge {
                max_records: self.max_records,
            }
        })?;
        let encoded_count = u32::try_from(self.records.len()).map_err(|_| {
            SketchMaintenanceCodecError::RecordCountTooLarge {
                record_count: self.records.len(),
            }
        })?;

        let mut bytes = Vec::new();
        bytes.try_reserve_exact(encoded_len).map_err(|_| {
            SketchMaintenanceCodecError::AllocationFailed {
                requested_bytes: encoded_len,
            }
        })?;
        bytes.extend_from_slice(&LOG_MAGIC);
        bytes.extend_from_slice(&SKETCH_MAINTENANCE_LOG_VERSION.to_le_bytes());
        bytes.extend_from_slice(&LOG_RESERVED.to_le_bytes());
        bytes.extend_from_slice(&encoded_profile.to_le_bytes());
        bytes.extend_from_slice(&encoded_count.to_le_bytes());
        for record in &self.records {
            bytes.extend_from_slice(&record.to_canonical_bytes());
        }
        debug_assert_eq!(bytes.len(), encoded_len);
        Ok(bytes)
    }

    /// Decodes a canonical stream under caller-owned limits and a trusted
    /// expected durable profile.
    pub fn from_canonical_bytes(
        bytes: &[u8],
        limits: SketchMaintenanceLogDecodeLimits,
        expected_max_records: usize,
    ) -> Result<Self, SketchMaintenanceCodecError> {
        validate_profile(expected_max_records).map_err(|_| {
            SketchMaintenanceCodecError::InvalidProfile {
                max_records: expected_max_records,
            }
        })?;
        if bytes.len() > limits.max_encoded_bytes {
            return Err(SketchMaintenanceCodecError::EncodedBytesLimit {
                actual: bytes.len(),
                max_encoded_bytes: limits.max_encoded_bytes,
            });
        }
        if bytes.len() < LOG_HEADER_BYTES {
            return Err(SketchMaintenanceCodecError::LogTruncated {
                minimum: LOG_HEADER_BYTES,
                actual: bytes.len(),
            });
        }
        if bytes[..8] != LOG_MAGIC {
            return Err(SketchMaintenanceCodecError::LogMagic);
        }

        let version = read_u16(bytes, 8);
        if version != SKETCH_MAINTENANCE_LOG_VERSION {
            return Err(SketchMaintenanceCodecError::LogVersion { version });
        }
        let reserved = read_u16(bytes, 10);
        if reserved != LOG_RESERVED {
            return Err(SketchMaintenanceCodecError::LogReserved { reserved });
        }

        let encoded_profile = read_u32(bytes, 12) as usize;
        let record_count = read_u32(bytes, 16) as usize;
        if encoded_profile != expected_max_records {
            return Err(SketchMaintenanceCodecError::ProfileMismatch {
                expected: expected_max_records,
                actual: encoded_profile,
            });
        }
        if encoded_profile > MAX_SKETCH_MAINTENANCE_RECORDS || encoded_profile > limits.max_records
        {
            return Err(SketchMaintenanceCodecError::ProfileLimit {
                encoded_profile,
                caller_max_records: limits.max_records,
                absolute_max_records: MAX_SKETCH_MAINTENANCE_RECORDS,
            });
        }
        if record_count > encoded_profile || record_count > limits.max_records {
            return Err(SketchMaintenanceCodecError::RecordCountLimit {
                record_count,
                encoded_profile,
                caller_max_records: limits.max_records,
            });
        }

        let expected_len = encoded_log_len(record_count)?;
        if expected_len != bytes.len() {
            return Err(SketchMaintenanceCodecError::LogLength {
                expected: expected_len,
                actual: bytes.len(),
            });
        }

        let mut records = Vec::new();
        records.try_reserve_exact(record_count).map_err(|_| {
            SketchMaintenanceCodecError::AllocationFailed {
                requested_bytes: record_count
                    .saturating_mul(core::mem::size_of::<SketchMaintenanceRecord>()),
            }
        })?;
        let (record_chunks, remainder) = bytes[LOG_HEADER_BYTES..].as_chunks::<RECORD_BYTES>();
        debug_assert!(remainder.is_empty());
        for (index, chunk) in record_chunks.iter().enumerate() {
            let record = SketchMaintenanceRecord::from_canonical_bytes(chunk)?;
            let expected =
                u64::try_from(index).map_err(|_| SketchMaintenanceCodecError::SequenceOverflow)?;
            if record.sequence != expected {
                return Err(SketchMaintenanceCodecError::Sequence {
                    expected,
                    actual: record.sequence,
                });
            }
            records.push(record);
        }

        Ok(Self {
            max_records: encoded_profile,
            records,
        })
    }
}

/// Bounded-log construction or append error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SketchMaintenanceLogError {
    /// The durable capacity profile is invalid.
    InvalidProfile { max_records: usize },
    /// The log has reached its durable capacity.
    CapacityExceeded { max_records: usize },
    /// The event sequence was not the next gap-free value.
    Sequence { expected: u64, actual: u64 },
    /// The platform could not represent the next sequence.
    SequenceOverflow,
    /// A caller-constructed record violated an outcome invariant.
    InvalidRecord(SketchMaintenanceCodecError),
}

impl fmt::Display for SketchMaintenanceLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile { max_records } => write!(
                formatter,
                "sketch maintenance max_records must be in 1..={MAX_SKETCH_MAINTENANCE_RECORDS}, got {max_records}"
            ),
            Self::CapacityExceeded { max_records } => write!(
                formatter,
                "sketch maintenance log reached its {max_records}-record capacity"
            ),
            Self::Sequence { expected, actual } => write!(
                formatter,
                "sketch maintenance sequence must be {expected}, got {actual}"
            ),
            Self::SequenceOverflow => {
                formatter.write_str("sketch maintenance sequence cannot be represented")
            }
            Self::InvalidRecord(error) => write!(formatter, "invalid maintenance record: {error}"),
        }
    }
}

impl std::error::Error for SketchMaintenanceLogError {}

/// Strict canonical codec failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SketchMaintenanceCodecError {
    /// A standalone record did not have its exact fixed width.
    RecordLength { expected: usize, actual: usize },
    /// The record magic was not canonical.
    RecordMagic,
    /// The record version is unsupported.
    RecordVersion { version: u16 },
    /// A reserved record field was nonzero.
    RecordReserved { reserved: u32 },
    /// The sketch-family tag is outside the closed vocabulary.
    UnknownSketchFamily { tag: u8 },
    /// The maintenance-outcome tag is outside the closed vocabulary.
    UnknownMaintenanceOutcome { tag: u8 },
    /// A rebuild-required event falsely claimed a state mutation.
    RebuildMutatedState,
    /// The durable capacity profile is invalid.
    InvalidProfile { max_records: usize },
    /// The complete input exceeded the caller-owned byte budget.
    EncodedBytesLimit {
        actual: usize,
        max_encoded_bytes: usize,
    },
    /// The log ended before its complete header.
    LogTruncated { minimum: usize, actual: usize },
    /// The log magic was not canonical.
    LogMagic,
    /// The log version is unsupported.
    LogVersion { version: u16 },
    /// A reserved log field was nonzero.
    LogReserved { reserved: u16 },
    /// The encoded durable profile disagreed with trusted configuration.
    ProfileMismatch { expected: usize, actual: usize },
    /// The encoded profile exceeded a caller or absolute limit.
    ProfileLimit {
        encoded_profile: usize,
        caller_max_records: usize,
        absolute_max_records: usize,
    },
    /// The concrete record count exceeded its admitted profile.
    RecordCountLimit {
        record_count: usize,
        encoded_profile: usize,
        caller_max_records: usize,
    },
    /// The encoded profile cannot fit the durable field.
    ProfileTooLarge { max_records: usize },
    /// The concrete record count cannot fit the durable field.
    RecordCountTooLarge { record_count: usize },
    /// Length arithmetic overflowed.
    EncodedLengthOverflow,
    /// The exact byte length disagreed with the frame inventory.
    LogLength { expected: usize, actual: usize },
    /// Reserving admitted memory failed.
    AllocationFailed { requested_bytes: usize },
    /// A decoded sequence was not gap-free.
    Sequence { expected: u64, actual: u64 },
    /// The platform could not represent a decoded sequence.
    SequenceOverflow,
}

impl fmt::Display for SketchMaintenanceCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SketchMaintenanceCodecError {}

fn validate_profile(max_records: usize) -> Result<(), SketchMaintenanceLogError> {
    if max_records == 0 || max_records > MAX_SKETCH_MAINTENANCE_RECORDS {
        return Err(SketchMaintenanceLogError::InvalidProfile { max_records });
    }
    Ok(())
}

fn encoded_log_len(record_count: usize) -> Result<usize, SketchMaintenanceCodecError> {
    record_count
        .checked_mul(RECORD_BYTES)
        .and_then(|payload| LOG_HEADER_BYTES.checked_add(payload))
        .ok_or(SketchMaintenanceCodecError::EncodedLengthOverflow)
}

fn domain_separated_digest(domain: &[u8], canonical_bytes: &[u8]) -> [u8; 32] {
    let mut transcript = Vec::with_capacity(
        domain
            .len()
            .saturating_add(core::mem::size_of::<u64>())
            .saturating_add(canonical_bytes.len()),
    );
    transcript.extend_from_slice(domain);
    transcript.extend_from_slice(
        &u64::try_from(canonical_bytes.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    transcript.extend_from_slice(canonical_bytes);
    asupersync::atp::object::compute_hash(&transcript)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut value = [0_u8; 2];
    value.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(value)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}

fn read_array_32(bytes: &[u8], offset: usize) -> [u8; 32] {
    let mut value = [0_u8; 32];
    value.copy_from_slice(&bytes[offset..offset + 32]);
    value
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_HEADER_BYTES, RECORD_BYTES, SketchFamily, SketchMaintenanceCodecError,
        SketchMaintenanceLog, SketchMaintenanceLogDecodeLimits, SketchMaintenanceLogError,
        SketchMaintenanceOutcome, SketchMaintenanceRecord,
    };
    use fgdb_types::ObjectId;

    const PROFILE_OID: ObjectId = ObjectId([0x31; 32]);
    const MERGE_OID: ObjectId = ObjectId([0x41; 32]);
    const DELETE_OID: ObjectId = ObjectId([0x51; 32]);
    const LIMITS: SketchMaintenanceLogDecodeLimits =
        SketchMaintenanceLogDecodeLimits::new(8, 4_096);

    fn merge_record(sequence: u64) -> SketchMaintenanceRecord {
        SketchMaintenanceRecord::merged(
            sequence,
            SketchFamily::CountMin,
            PROFILE_OID,
            MERGE_OID,
            b"canonical-before",
            b"canonical-operand",
            b"canonical-after",
        )
    }

    #[test]
    fn merge_log_round_trips_and_replays_byte_identically() {
        let mut log = SketchMaintenanceLog::new(8).expect("valid profile");
        log.append(merge_record(0)).expect("first record");
        log.append(SketchMaintenanceRecord::merged(
            1,
            SketchFamily::BottomK,
            PROFILE_OID,
            MERGE_OID,
            b"bottom-k-before",
            b"bottom-k-operand",
            b"bottom-k-after",
        ))
        .expect("second record");

        let encoded = log.to_canonical_bytes().expect("encode");
        let replayed =
            SketchMaintenanceLog::from_canonical_bytes(&encoded, LIMITS, 8).expect("strict read");

        assert_eq!(replayed, log);
        assert_eq!(replayed.to_canonical_bytes().expect("re-encode"), encoded);
    }

    #[test]
    fn every_maintained_family_tag_round_trips() {
        // The closed vocabulary must cover every family this crate implements;
        // an unregistered family cannot record its merges at all.
        const FAMILIES: [SketchFamily; 8] = [
            SketchFamily::ExactQuantiles,
            SketchFamily::DegreeHistogram,
            SketchFamily::CountMin,
            SketchFamily::BottomK,
            SketchFamily::Distinct,
            SketchFamily::ZoneMap,
            SketchFamily::PathCorrelation,
            SketchFamily::LabelCounts,
        ];

        for (index, family) in FAMILIES.into_iter().enumerate() {
            let tag = u8::try_from(index + 1).expect("closed vocabulary is small");
            assert_eq!(family as u8, tag);
            assert_eq!(SketchFamily::from_tag(tag), Ok(family));

            let record = SketchMaintenanceRecord::merged(
                index as u64,
                family,
                PROFILE_OID,
                MERGE_OID,
                b"canonical-before",
                b"canonical-operand",
                b"canonical-after",
            );
            assert_eq!(
                SketchMaintenanceRecord::from_canonical_bytes(&record.to_canonical_bytes())
                    .expect("strict record read")
                    .family,
                family
            );
        }

        let unregistered = u8::try_from(FAMILIES.len() + 1).expect("closed vocabulary is small");
        assert_eq!(
            SketchFamily::from_tag(unregistered),
            Err(SketchMaintenanceCodecError::UnknownSketchFamily { tag: unregistered })
        );
    }

    #[test]
    fn rebuild_required_structurally_preserves_state() {
        let record = SketchMaintenanceRecord::rebuild_required(
            0,
            SketchFamily::ZoneMap,
            PROFILE_OID,
            DELETE_OID,
            b"unchanged-zone-map-state",
            b"delete-interior-value:17",
        );

        assert_eq!(record.outcome, SketchMaintenanceOutcome::RebuildRequired);
        assert_eq!(record.before_digest, record.after_digest);
        assert_eq!(
            SketchMaintenanceRecord::from_canonical_bytes(&record.to_canonical_bytes())
                .expect("strict record read"),
            record
        );
    }

    #[test]
    fn decoder_rejects_rebuild_that_claims_mutation() {
        let record = SketchMaintenanceRecord::rebuild_required(
            0,
            SketchFamily::ZoneMap,
            PROFILE_OID,
            DELETE_OID,
            b"unchanged",
            b"deletion",
        );
        let mut encoded = record.to_canonical_bytes();
        encoded[152] ^= 1;

        assert_eq!(
            SketchMaintenanceRecord::from_canonical_bytes(&encoded),
            Err(SketchMaintenanceCodecError::RebuildMutatedState)
        );
    }

    #[test]
    fn log_enforces_gap_free_sequence_and_capacity() {
        let mut log = SketchMaintenanceLog::new(1).expect("valid profile");
        assert_eq!(
            log.append(merge_record(1)),
            Err(SketchMaintenanceLogError::Sequence {
                expected: 0,
                actual: 1,
            })
        );
        log.append(merge_record(0)).expect("in sequence");
        assert_eq!(
            log.append(merge_record(1)),
            Err(SketchMaintenanceLogError::CapacityExceeded { max_records: 1 })
        );
    }

    #[test]
    fn decoder_preflights_profile_count_bytes_and_exact_length() {
        let mut log = SketchMaintenanceLog::new(8).expect("valid profile");
        log.append(merge_record(0)).expect("record");
        let encoded = log.to_canonical_bytes().expect("encode");

        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(&encoded, LIMITS, 7),
            Err(SketchMaintenanceCodecError::ProfileMismatch {
                expected: 7,
                actual: 8,
            })
        );
        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(
                &encoded,
                SketchMaintenanceLogDecodeLimits::new(7, encoded.len()),
                8,
            ),
            Err(SketchMaintenanceCodecError::ProfileLimit {
                encoded_profile: 8,
                caller_max_records: 7,
                absolute_max_records: 1_048_576,
            })
        );
        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(
                &encoded,
                SketchMaintenanceLogDecodeLimits::new(8, encoded.len() - 1),
                8,
            ),
            Err(SketchMaintenanceCodecError::EncodedBytesLimit {
                actual: encoded.len(),
                max_encoded_bytes: encoded.len() - 1,
            })
        );

        let mut oversized_count = encoded.clone();
        oversized_count[16..20].copy_from_slice(&9_u32.to_le_bytes());
        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(&oversized_count, LIMITS, 8),
            Err(SketchMaintenanceCodecError::RecordCountLimit {
                record_count: 9,
                encoded_profile: 8,
                caller_max_records: 8,
            })
        );

        let truncated = &encoded[..encoded.len() - 1];
        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(truncated, LIMITS, 8),
            Err(SketchMaintenanceCodecError::LogLength {
                expected: LOG_HEADER_BYTES + RECORD_BYTES,
                actual: truncated.len(),
            })
        );
    }

    #[test]
    fn decoder_rejects_noncanonical_tags_reserved_bytes_and_sequence() {
        let record = merge_record(0);

        let mut unknown_family = record.to_canonical_bytes();
        unknown_family[10] = 99;
        assert_eq!(
            SketchMaintenanceRecord::from_canonical_bytes(&unknown_family),
            Err(SketchMaintenanceCodecError::UnknownSketchFamily { tag: 99 })
        );

        let mut reserved = record.to_canonical_bytes();
        reserved[12] = 1;
        assert_eq!(
            SketchMaintenanceRecord::from_canonical_bytes(&reserved),
            Err(SketchMaintenanceCodecError::RecordReserved { reserved: 1 })
        );

        let mut log = SketchMaintenanceLog::new(8).expect("valid profile");
        log.append(record).expect("record");
        let mut encoded = log.to_canonical_bytes().expect("encode");
        encoded[LOG_HEADER_BYTES + 16..LOG_HEADER_BYTES + 24].copy_from_slice(&2_u64.to_le_bytes());
        assert_eq!(
            SketchMaintenanceLog::from_canonical_bytes(&encoded, LIMITS, 8),
            Err(SketchMaintenanceCodecError::Sequence {
                expected: 0,
                actual: 2,
            })
        );
    }
}
