//! Registry-independent logical neighbor/run digest validation.
//!
//! Physical neighbor encodings are deliberately absent from the transcript.
//! An encoded destination scalar is first decoded through [`EncodedNeighbors`]
//! and resolved to its stable [`VId`]. The transcript then contains only
//! ordered `(source VId, destination VId, EId)` incidences, explicit row
//! boundaries, and bounded counts. Elias-Fano, StreamVByte, DenseIntervals,
//! and future physical representations can therefore be compared without
//! making their bytes part of logical identity.
//!
//! [`LogicalNeighborRunDigest`] is a validation digest, not an
//! [`fgdb_types::ObjectId`], registered durable format, codec identifier, or
//! substitute for the database-keyed object identity recipe. A durable owner
//! must still register its framing, limits, and authority-bound identity
//! transcript. Equality proves only the supplied stable neighbor-row tuples;
//! it does not prove visibility, authorization, property, resolver-authority,
//! or query-answer equivalence.

#![forbid(unsafe_code)]

use core::fmt;

use fgdb_types::{EId, VId};

use crate::neighbor::{EncodedNeighbors, NeighborCodec, NeighborError};

/// Domain/version separator for the registry-independent validation
/// transcript.
///
/// The terminal NUL makes the separator self-terminating before the first
/// fixed-width count. This constant does not register a durable format.
pub const LOGICAL_NEIGHBOR_RUN_DOMAIN_V1: &[u8] = b"fgdb:codec:logical-neighbor-run:v1\0";

const ROW_TAG: u8 = 0x52;
const INCIDENCE_TAG: u8 = 0x49;
const COUNT_BYTES: usize = core::mem::size_of::<u64>();
const ID_BYTES: usize = core::mem::size_of::<u128>();
const RUN_HEADER_BYTES: usize = COUNT_BYTES * 2;
const ROW_BYTES: usize = 1 + ID_BYTES + COUNT_BYTES;
const INCIDENCE_BYTES: usize = 1 + ID_BYTES + ID_BYTES;

/// Explicit resource ceilings for one logical neighbor-run transcript.
///
/// Every digest operation requires this value. There is intentionally no
/// unbounded or implicit default.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LogicalDigestLimits {
    max_rows: usize,
    max_entries_per_row: usize,
    max_total_entries: usize,
    max_transcript_bytes: usize,
}

impl LogicalDigestLimits {
    /// Creates exact ceilings for row count, per-row incidence count, total
    /// incidence count, and materialized canonical transcript bytes.
    #[must_use]
    pub const fn new(
        max_rows: usize,
        max_entries_per_row: usize,
        max_total_entries: usize,
        max_transcript_bytes: usize,
    ) -> Self {
        Self {
            max_rows,
            max_entries_per_row,
            max_total_entries,
            max_transcript_bytes,
        }
    }

    /// Maximum number of source rows.
    #[must_use]
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Maximum number of incidences in any one row.
    #[must_use]
    pub const fn max_entries_per_row(self) -> usize {
        self.max_entries_per_row
    }

    /// Maximum total incidences across all rows.
    #[must_use]
    pub const fn max_total_entries(self) -> usize {
        self.max_total_entries
    }

    /// Maximum canonical transcript bytes materialized before hashing.
    #[must_use]
    pub const fn max_transcript_bytes(self) -> usize {
        self.max_transcript_bytes
    }
}

/// Stable logical edge incidence within one source row.
///
/// Repeated values are retained. In particular, two equal incidences produce
/// two transcript entries rather than being silently deduplicated.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableNeighbor {
    destination: VId,
    edge: EId,
}

impl StableNeighbor {
    /// Creates one stable destination/edge incidence.
    #[must_use]
    pub const fn new(destination: VId, edge: EId) -> Self {
        Self { destination, edge }
    }

    /// Stable destination vertex identity.
    #[must_use]
    pub const fn destination(self) -> VId {
        self.destination
    }

    /// Stable edge identity.
    #[must_use]
    pub const fn edge(self) -> EId {
        self.edge
    }
}

/// Borrowed stable logical row used when no physical neighbor decoding is
/// needed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StableNeighborRow<'a> {
    source: VId,
    incidences: &'a [StableNeighbor],
}

impl<'a> StableNeighborRow<'a> {
    /// Creates one source row. Slice order and duplicates are preserved.
    #[must_use]
    pub const fn new(source: VId, incidences: &'a [StableNeighbor]) -> Self {
        Self { source, incidences }
    }

    /// Stable source vertex identity.
    #[must_use]
    pub const fn source(self) -> VId {
        self.source
    }

    /// Ordered stable incidences.
    #[must_use]
    pub const fn incidences(self) -> &'a [StableNeighbor] {
        self.incidences
    }
}

/// Borrowed physical-neighbor row plus its aligned stable EId column.
///
/// The physical neighbor bytes and arm tag never enter the logical transcript.
/// The decoded scalar at each position is resolved through
/// [`DestinationIdResolver`], while the EId at the same position preserves
/// parallel-edge identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedNeighborRow<'a> {
    source: VId,
    neighbors: &'a EncodedNeighbors,
    edge_ids: &'a [EId],
}

impl<'a> EncodedNeighborRow<'a> {
    /// Creates an encoded source row and its position-aligned stable edge IDs.
    #[must_use]
    pub const fn new(source: VId, neighbors: &'a EncodedNeighbors, edge_ids: &'a [EId]) -> Self {
        Self {
            source,
            neighbors,
            edge_ids,
        }
    }

    /// Stable source vertex identity.
    #[must_use]
    pub const fn source(self) -> VId {
        self.source
    }

    /// Explicit physical neighbor representation.
    #[must_use]
    pub const fn neighbors(self) -> &'a EncodedNeighbors {
        self.neighbors
    }

    /// Stable edge IDs aligned one-for-one with decoded neighbors.
    #[must_use]
    pub const fn edge_ids(self) -> &'a [EId] {
        self.edge_ids
    }
}

/// Resolves a decoded physical neighbor scalar to a stable vertex identity.
///
/// `row_index` permits one resolver to consult the distinct ordinal-map
/// authority carried by each run row. Returning `None` rejects the transcript;
/// unresolved ordinals are never hashed directly.
pub trait DestinationIdResolver {
    /// Returns the stable destination for one decoded physical scalar.
    fn resolve_destination(&self, row_index: usize, encoded: u64) -> Option<VId>;
}

impl<F> DestinationIdResolver for F
where
    F: Fn(usize, u64) -> Option<VId>,
{
    fn resolve_destination(&self, row_index: usize, encoded: u64) -> Option<VId> {
        self(row_index, encoded)
    }
}

/// Digest of the canonical registry-independent logical validation transcript.
///
/// This type is intentionally distinct from every durable identity type.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LogicalNeighborRunDigest([u8; 32]);

impl LogicalNeighborRunDigest {
    /// Foundation-owned digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for LogicalNeighborRunDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LogicalNeighborRunDigest(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// Checked transcript-size calculation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LogicalSizeCalculation {
    /// Fixed domain plus run header.
    RunHeader,
    /// Adding one source-row transcript.
    Row,
    /// Multiplying a row's incidence count by its fixed transcript width.
    Incidences,
    /// Summing incidence counts across rows.
    TotalEntries,
    /// Converting a platform row count to the canonical `u64` count.
    RowCount,
    /// Converting a platform incidence count to the canonical `u64` count.
    EntryCount,
}

/// Failure while deriving a stable logical neighbor-run digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalDigestError {
    /// The input has more rows than the caller-authorized ceiling.
    RowLimitExceeded {
        /// Supplied row count.
        rows: usize,
        /// Caller-authorized ceiling.
        limit: usize,
    },
    /// One row has more incidences than its caller-authorized ceiling.
    RowEntryLimitExceeded {
        /// Zero-based source-row position.
        row_index: usize,
        /// Supplied incidence count.
        entries: usize,
        /// Caller-authorized ceiling.
        limit: usize,
    },
    /// All rows together have more incidences than the authorized ceiling.
    TotalEntryLimitExceeded {
        /// Supplied total incidence count.
        entries: usize,
        /// Caller-authorized ceiling.
        limit: usize,
    },
    /// The canonical transcript exceeds its caller-authorized byte ceiling.
    TranscriptLimitExceeded {
        /// Exact canonical transcript bytes required.
        bytes: usize,
        /// Caller-authorized ceiling.
        limit: usize,
    },
    /// A checked count or transcript-size operation overflowed.
    SizeOverflow {
        /// Stable calculation name.
        calculation: LogicalSizeCalculation,
    },
    /// Transcript allocation failed after all counts and sizes were checked.
    AllocationFailed {
        /// Exact canonical transcript bytes requested.
        requested: usize,
    },
    /// A physical row's EId column does not align with its decoded neighbors.
    EdgeCountMismatch {
        /// Zero-based source-row position.
        row_index: usize,
        /// Number of decoded-neighbor positions declared by the representation.
        neighbors: usize,
        /// Number of supplied stable edge identities.
        edge_ids: usize,
    },
    /// One physical representation failed during bounded sequential decode.
    NeighborDecode {
        /// Zero-based source-row position.
        row_index: usize,
        /// Zero-based incidence position being decoded.
        entry_index: usize,
        /// Explicit physical representation arm.
        codec: NeighborCodec,
        /// Typed scalar neighbor failure.
        source: NeighborError,
    },
    /// A decoded physical scalar had no stable-ID mapping.
    UnresolvedDestination {
        /// Zero-based source-row position.
        row_index: usize,
        /// Zero-based incidence position.
        entry_index: usize,
        /// Decoded representation value that could not be resolved.
        encoded: u64,
    },
    /// A physical cursor ended before its declared logical length.
    DecodedLengthMismatch {
        /// Zero-based source-row position.
        row_index: usize,
        /// Length declared by the physical representation.
        expected: usize,
        /// Number of values yielded before exhaustion.
        actual: usize,
    },
    /// A physical cursor yielded a value after its declared logical length.
    UnexpectedDecodedValue {
        /// Zero-based source-row position.
        row_index: usize,
        /// First position beyond the declared logical length.
        entry_index: usize,
        /// Unexpected decoded value.
        encoded: u64,
    },
}

impl fmt::Display for LogicalDigestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::RowLimitExceeded { rows, limit } => {
                write!(formatter, "logical run has {rows} rows, limit is {limit}")
            }
            Self::RowEntryLimitExceeded {
                row_index,
                entries,
                limit,
            } => write!(
                formatter,
                "logical run row {row_index} has {entries} entries, limit is {limit}"
            ),
            Self::TotalEntryLimitExceeded { entries, limit } => write!(
                formatter,
                "logical run has {entries} total entries, limit is {limit}"
            ),
            Self::TranscriptLimitExceeded { bytes, limit } => write!(
                formatter,
                "logical run transcript needs {bytes} bytes, limit is {limit}"
            ),
            Self::SizeOverflow { calculation } => {
                write!(
                    formatter,
                    "logical run {calculation:?} arithmetic overflowed"
                )
            }
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} bytes for logical run transcript"
            ),
            Self::EdgeCountMismatch {
                row_index,
                neighbors,
                edge_ids,
            } => write!(
                formatter,
                "logical run row {row_index} has {neighbors} neighbors but {edge_ids} edge IDs"
            ),
            Self::NeighborDecode {
                row_index,
                entry_index,
                codec,
                source,
            } => write!(
                formatter,
                "logical run row {row_index} {codec:?} decode failed at entry \
                 {entry_index}: {source}"
            ),
            Self::UnresolvedDestination {
                row_index,
                entry_index,
                encoded,
            } => write!(
                formatter,
                "logical run row {row_index} entry {entry_index} has unresolved \
                 destination scalar {encoded}"
            ),
            Self::DecodedLengthMismatch {
                row_index,
                expected,
                actual,
            } => write!(
                formatter,
                "logical run row {row_index} decoded {actual} neighbors, expected {expected}"
            ),
            Self::UnexpectedDecodedValue {
                row_index,
                entry_index,
                encoded,
            } => write!(
                formatter,
                "logical run row {row_index} decoded unexpected value {encoded} at \
                 entry {entry_index}"
            ),
        }
    }
}

impl std::error::Error for LogicalDigestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NeighborDecode { source, .. } => Some(source),
            Self::RowLimitExceeded { .. }
            | Self::RowEntryLimitExceeded { .. }
            | Self::TotalEntryLimitExceeded { .. }
            | Self::TranscriptLimitExceeded { .. }
            | Self::SizeOverflow { .. }
            | Self::AllocationFailed { .. }
            | Self::EdgeCountMismatch { .. }
            | Self::UnresolvedDestination { .. }
            | Self::DecodedLengthMismatch { .. }
            | Self::UnexpectedDecodedValue { .. } => None,
        }
    }
}

/// Side of a typed codec-substitution validation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CodecSubstitutionSide {
    /// Existing physical representation.
    Existing,
    /// Proposed replacement representation.
    Replacement,
}

/// Failure while proving a physical codec substitution preserves logical
/// neighbor rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecSubstitutionError {
    /// One side could not derive a bounded logical validation digest.
    Derivation {
        /// Representation side that failed.
        side: CodecSubstitutionSide,
        /// Typed derivation failure.
        source: LogicalDigestError,
    },
    /// Both sides were valid but represented different stable graph tuples or
    /// row boundaries.
    LogicalMismatch {
        /// Existing representation's logical digest.
        existing: LogicalNeighborRunDigest,
        /// Replacement representation's logical digest.
        replacement: LogicalNeighborRunDigest,
    },
}

impl fmt::Display for CodecSubstitutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Derivation { side, source } => {
                write!(formatter, "{side:?} codec derivation failed: {source}")
            }
            Self::LogicalMismatch {
                existing,
                replacement,
            } => write!(
                formatter,
                "codec substitution changes logical run: existing={existing:?}, \
                 replacement={replacement:?}"
            ),
        }
    }
}

impl std::error::Error for CodecSubstitutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Derivation { source, .. } => Some(source),
            Self::LogicalMismatch { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TranscriptPlan {
    row_count: u64,
    total_entries: u64,
    transcript_bytes: usize,
}

/// Derives the validation digest from already-stable logical rows.
///
/// Row order, incidence order, multiplicity, stable sources, stable
/// destinations, stable edge IDs, and empty/nonempty row boundaries all
/// participate in the transcript.
pub fn digest_stable_neighbor_run(
    rows: &[StableNeighborRow<'_>],
    limits: LogicalDigestLimits,
) -> Result<LogicalNeighborRunDigest, LogicalDigestError> {
    let plan = preflight(
        rows.len(),
        |row_index| rows[row_index].incidences.len(),
        limits,
    )?;
    let mut transcript = allocate_transcript(plan.transcript_bytes)?;
    write_run_header(&mut transcript, plan);

    for row in rows {
        write_row_header(&mut transcript, row.source, row.incidences.len())?;
        for incidence in row.incidences {
            write_incidence(&mut transcript, *incidence);
        }
    }

    finish_digest(transcript, plan.transcript_bytes)
}

/// Derives the validation digest through bounded sequential decode of physical
/// neighbor representations.
///
/// Each decoded scalar is resolved to a stable VId before it enters the
/// transcript. Neither private encoded bytes nor [`NeighborCodec`] arm tags are
/// hashed.
pub fn digest_encoded_neighbor_run<R>(
    rows: &[EncodedNeighborRow<'_>],
    resolver: &R,
    limits: LogicalDigestLimits,
) -> Result<LogicalNeighborRunDigest, LogicalDigestError>
where
    R: DestinationIdResolver + ?Sized,
{
    let plan = preflight(
        rows.len(),
        |row_index| rows[row_index].neighbors.len(),
        limits,
    )?;
    for (row_index, row) in rows.iter().enumerate() {
        if row.neighbors.len() != row.edge_ids.len() {
            return Err(LogicalDigestError::EdgeCountMismatch {
                row_index,
                neighbors: row.neighbors.len(),
                edge_ids: row.edge_ids.len(),
            });
        }
    }
    let mut transcript = allocate_transcript(plan.transcript_bytes)?;
    write_run_header(&mut transcript, plan);

    for (row_index, row) in rows.iter().enumerate() {
        write_row_header(&mut transcript, row.source, row.neighbors.len())?;
        let mut cursor = row.neighbors.cursor();
        for (entry_index, edge) in row.edge_ids.iter().copied().enumerate() {
            let encoded = cursor
                .try_next()
                .map_err(|source| LogicalDigestError::NeighborDecode {
                    row_index,
                    entry_index,
                    codec: row.neighbors.codec(),
                    source,
                })?
                .ok_or(LogicalDigestError::DecodedLengthMismatch {
                    row_index,
                    expected: row.neighbors.len(),
                    actual: entry_index,
                })?;
            let destination = resolver.resolve_destination(row_index, encoded).ok_or(
                LogicalDigestError::UnresolvedDestination {
                    row_index,
                    entry_index,
                    encoded,
                },
            )?;
            write_incidence(&mut transcript, StableNeighbor::new(destination, edge));
        }
        let trailing_index = row.edge_ids.len();
        match cursor.try_next() {
            Ok(None) => {}
            Ok(Some(encoded)) => {
                return Err(LogicalDigestError::UnexpectedDecodedValue {
                    row_index,
                    entry_index: trailing_index,
                    encoded,
                });
            }
            Err(source) => {
                return Err(LogicalDigestError::NeighborDecode {
                    row_index,
                    entry_index: trailing_index,
                    codec: row.neighbors.codec(),
                    source,
                });
            }
        }
    }

    finish_digest(transcript, plan.transcript_bytes)
}

/// Validates that a proposed physical codec substitution preserves stable
/// logical neighbor rows.
///
/// The two sides may use different codec arms and different physical ordinal
/// maps. Equality is decided solely from their resolved stable tuples and row
/// boundaries. This does not validate visibility, authorization, properties,
/// resolver authority, or a query answer contract.
pub fn validate_codec_substitution<ExistingResolver, ReplacementResolver>(
    existing_rows: &[EncodedNeighborRow<'_>],
    existing_resolver: &ExistingResolver,
    replacement_rows: &[EncodedNeighborRow<'_>],
    replacement_resolver: &ReplacementResolver,
    limits: LogicalDigestLimits,
) -> Result<LogicalNeighborRunDigest, CodecSubstitutionError>
where
    ExistingResolver: DestinationIdResolver + ?Sized,
    ReplacementResolver: DestinationIdResolver + ?Sized,
{
    let existing = digest_encoded_neighbor_run(existing_rows, existing_resolver, limits).map_err(
        |source| CodecSubstitutionError::Derivation {
            side: CodecSubstitutionSide::Existing,
            source,
        },
    )?;
    let replacement = digest_encoded_neighbor_run(replacement_rows, replacement_resolver, limits)
        .map_err(|source| CodecSubstitutionError::Derivation {
        side: CodecSubstitutionSide::Replacement,
        source,
    })?;
    if existing != replacement {
        return Err(CodecSubstitutionError::LogicalMismatch {
            existing,
            replacement,
        });
    }
    Ok(existing)
}

fn preflight(
    row_count: usize,
    mut entries_in_row: impl FnMut(usize) -> usize,
    limits: LogicalDigestLimits,
) -> Result<TranscriptPlan, LogicalDigestError> {
    if row_count > limits.max_rows {
        return Err(LogicalDigestError::RowLimitExceeded {
            rows: row_count,
            limit: limits.max_rows,
        });
    }
    let row_count_u64 = u64::try_from(row_count).map_err(|_| LogicalDigestError::SizeOverflow {
        calculation: LogicalSizeCalculation::RowCount,
    })?;
    let mut transcript_bytes = LOGICAL_NEIGHBOR_RUN_DOMAIN_V1
        .len()
        .checked_add(RUN_HEADER_BYTES)
        .ok_or(LogicalDigestError::SizeOverflow {
            calculation: LogicalSizeCalculation::RunHeader,
        })?;
    let mut total_entries = 0_usize;

    for row_index in 0..row_count {
        let entries = entries_in_row(row_index);
        if entries > limits.max_entries_per_row {
            return Err(LogicalDigestError::RowEntryLimitExceeded {
                row_index,
                entries,
                limit: limits.max_entries_per_row,
            });
        }
        let _ = u64::try_from(entries).map_err(|_| LogicalDigestError::SizeOverflow {
            calculation: LogicalSizeCalculation::EntryCount,
        })?;
        total_entries =
            total_entries
                .checked_add(entries)
                .ok_or(LogicalDigestError::SizeOverflow {
                    calculation: LogicalSizeCalculation::TotalEntries,
                })?;
        if total_entries > limits.max_total_entries {
            return Err(LogicalDigestError::TotalEntryLimitExceeded {
                entries: total_entries,
                limit: limits.max_total_entries,
            });
        }
        transcript_bytes = transcript_len_after_row(transcript_bytes, entries).ok_or(
            LogicalDigestError::SizeOverflow {
                calculation: LogicalSizeCalculation::Incidences,
            },
        )?;
    }

    if transcript_bytes > limits.max_transcript_bytes {
        return Err(LogicalDigestError::TranscriptLimitExceeded {
            bytes: transcript_bytes,
            limit: limits.max_transcript_bytes,
        });
    }
    let total_entries_u64 =
        u64::try_from(total_entries).map_err(|_| LogicalDigestError::SizeOverflow {
            calculation: LogicalSizeCalculation::EntryCount,
        })?;
    Ok(TranscriptPlan {
        row_count: row_count_u64,
        total_entries: total_entries_u64,
        transcript_bytes,
    })
}

fn transcript_len_after_row(current: usize, entries: usize) -> Option<usize> {
    let incidence_bytes = entries.checked_mul(INCIDENCE_BYTES)?;
    current.checked_add(ROW_BYTES)?.checked_add(incidence_bytes)
}

fn allocate_transcript(bytes: usize) -> Result<Vec<u8>, LogicalDigestError> {
    let mut transcript = Vec::new();
    transcript
        .try_reserve_exact(bytes)
        .map_err(|_| LogicalDigestError::AllocationFailed { requested: bytes })?;
    Ok(transcript)
}

fn write_run_header(transcript: &mut Vec<u8>, plan: TranscriptPlan) {
    transcript.extend_from_slice(LOGICAL_NEIGHBOR_RUN_DOMAIN_V1);
    transcript.extend_from_slice(&plan.row_count.to_be_bytes());
    transcript.extend_from_slice(&plan.total_entries.to_be_bytes());
}

fn write_row_header(
    transcript: &mut Vec<u8>,
    source: VId,
    entries: usize,
) -> Result<(), LogicalDigestError> {
    let entry_count = u64::try_from(entries).map_err(|_| LogicalDigestError::SizeOverflow {
        calculation: LogicalSizeCalculation::EntryCount,
    })?;
    transcript.push(ROW_TAG);
    transcript.extend_from_slice(&source.0.to_be_bytes());
    transcript.extend_from_slice(&entry_count.to_be_bytes());
    Ok(())
}

fn write_incidence(transcript: &mut Vec<u8>, incidence: StableNeighbor) {
    transcript.push(INCIDENCE_TAG);
    transcript.extend_from_slice(&incidence.destination.0.to_be_bytes());
    transcript.extend_from_slice(&incidence.edge.0.to_be_bytes());
}

fn finish_digest(
    transcript: Vec<u8>,
    expected_bytes: usize,
) -> Result<LogicalNeighborRunDigest, LogicalDigestError> {
    if transcript.len() != expected_bytes {
        return Err(LogicalDigestError::SizeOverflow {
            calculation: LogicalSizeCalculation::Incidences,
        });
    }
    Ok(LogicalNeighborRunDigest(
        asupersync::atp::object::compute_hash(&transcript),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elias_fano::EntryLimit;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const GENEROUS: LogicalDigestLimits = LogicalDigestLimits::new(8, 32, 64, 16 * 1024);

    fn encoded(codec: NeighborCodec, values: &[u64]) -> Result<EncodedNeighbors, NeighborError> {
        let limit = EntryLimit::new(values.len());
        match codec {
            NeighborCodec::EliasFano => EncodedNeighbors::try_elias_fano(values, limit),
            NeighborCodec::StreamVByte => EncodedNeighbors::try_stream_vbyte(values, limit),
            NeighborCodec::DenseIntervals => EncodedNeighbors::try_dense_intervals(values, limit),
        }
    }

    fn stable_digest(
        source: VId,
        incidences: &[StableNeighbor],
    ) -> Result<LogicalNeighborRunDigest, LogicalDigestError> {
        digest_stable_neighbor_run(&[StableNeighborRow::new(source, incidences)], GENEROUS)
    }

    #[test]
    fn exact_v1_vector_pins_transcript_and_digest() -> TestResult {
        let incidences = [
            StableNeighbor::new(VId(2), EId(3)),
            StableNeighbor::new(VId(4), EId(5)),
        ];
        let rows = [StableNeighborRow::new(VId(1), &incidences)];
        let plan = preflight(rows.len(), |index| rows[index].incidences.len(), GENEROUS)?;
        let mut transcript = allocate_transcript(plan.transcript_bytes)?;
        write_run_header(&mut transcript, plan);
        write_row_header(&mut transcript, rows[0].source, incidences.len())?;
        for incidence in incidences {
            write_incidence(&mut transcript, incidence);
        }

        let mut expected = Vec::new();
        expected.extend_from_slice(LOGICAL_NEIGHBOR_RUN_DOMAIN_V1);
        expected.extend_from_slice(&1_u64.to_be_bytes());
        expected.extend_from_slice(&2_u64.to_be_bytes());
        expected.push(ROW_TAG);
        expected.extend_from_slice(&1_u128.to_be_bytes());
        expected.extend_from_slice(&2_u64.to_be_bytes());
        expected.push(INCIDENCE_TAG);
        expected.extend_from_slice(&2_u128.to_be_bytes());
        expected.extend_from_slice(&3_u128.to_be_bytes());
        expected.push(INCIDENCE_TAG);
        expected.extend_from_slice(&4_u128.to_be_bytes());
        expected.extend_from_slice(&5_u128.to_be_bytes());
        assert_eq!(transcript, expected);
        assert_eq!(transcript.len(), 142);

        let digest = digest_stable_neighbor_run(&rows, GENEROUS)?;
        assert_eq!(
            digest.as_bytes(),
            &[
                0xf0, 0x63, 0x76, 0xb6, 0x11, 0x2c, 0x71, 0xb6, 0xe3, 0x0e, 0x7b, 0x65, 0x1b, 0x1a,
                0xd8, 0x4b, 0xd3, 0x34, 0xb7, 0x1e, 0xb6, 0x2d, 0x40, 0x16, 0x53, 0xc7, 0xe1, 0x2b,
                0xbd, 0xc3, 0x27, 0x84,
            ]
        );
        Ok(())
    }

    #[test]
    fn all_physical_arms_resolve_to_one_logical_digest() -> TestResult {
        let values = [1, 2, 3, 10, 11, 12];
        let edge_ids = [EId(101), EId(102), EId(103), EId(104), EId(105), EId(106)];
        let resolver = |_: usize, ordinal: u64| Some(VId(1_000 + u128::from(ordinal)));
        let mut digests = Vec::new();

        for codec in [
            NeighborCodec::EliasFano,
            NeighborCodec::StreamVByte,
            NeighborCodec::DenseIntervals,
        ] {
            let neighbors = encoded(codec, &values)?;
            let rows = [EncodedNeighborRow::new(VId(99), &neighbors, &edge_ids)];
            digests.push(digest_encoded_neighbor_run(&rows, &resolver, GENEROUS)?);
        }

        assert_eq!(digests[0], digests[1]);
        assert_eq!(digests[1], digests[2]);
        let stable: Vec<_> = values
            .iter()
            .zip(edge_ids)
            .map(|(&ordinal, edge)| StableNeighbor::new(VId(1_000 + u128::from(ordinal)), edge))
            .collect();
        assert_eq!(digests[0], stable_digest(VId(99), &stable)?);
        Ok(())
    }

    #[test]
    fn substitution_accepts_different_arms_and_ordinal_spaces() -> TestResult {
        let existing_values = [1, 2, 5, 6];
        let replacement_values = [101, 102, 105, 106];
        let edge_ids = [EId(7), EId(8), EId(9), EId(10)];
        let existing = encoded(NeighborCodec::EliasFano, &existing_values)?;
        let replacement = encoded(NeighborCodec::DenseIntervals, &replacement_values)?;
        let existing_rows = [EncodedNeighborRow::new(VId(50), &existing, &edge_ids)];
        let replacement_rows = [EncodedNeighborRow::new(VId(50), &replacement, &edge_ids)];
        let existing_resolver = |_: usize, ordinal: u64| Some(VId(10_000 + u128::from(ordinal)));
        let replacement_resolver = |_: usize, ordinal: u64| Some(VId(9_900 + u128::from(ordinal)));

        let digest = validate_codec_substitution(
            &existing_rows,
            &existing_resolver,
            &replacement_rows,
            &replacement_resolver,
            GENEROUS,
        )?;
        assert_eq!(
            digest,
            digest_encoded_neighbor_run(&existing_rows, &existing_resolver, GENEROUS)?
        );
        Ok(())
    }

    #[test]
    fn transcript_is_sensitive_to_order_multiplicity_and_identity() -> TestResult {
        let a = StableNeighbor::new(VId(10), EId(100));
        let b = StableNeighbor::new(VId(20), EId(200));
        let canonical = stable_digest(VId(1), &[a, b])?;

        assert_ne!(canonical, stable_digest(VId(1), &[b, a])?);
        assert_ne!(canonical, stable_digest(VId(1), &[a, b, b])?);
        assert_ne!(canonical, stable_digest(VId(2), &[a, b])?);
        assert_ne!(
            canonical,
            stable_digest(
                VId(1),
                &[
                    StableNeighbor::new(VId(10), EId(100)),
                    StableNeighbor::new(VId(20), EId(201)),
                ],
            )?
        );
        assert_ne!(
            canonical,
            stable_digest(
                VId(1),
                &[
                    StableNeighbor::new(VId(11), EId(100)),
                    StableNeighbor::new(VId(20), EId(200)),
                ],
            )?
        );
        Ok(())
    }

    #[test]
    fn row_boundaries_and_empty_rows_are_logical() -> TestResult {
        let a = StableNeighbor::new(VId(10), EId(100));
        let b = StableNeighbor::new(VId(20), EId(200));
        let together_incidences = [a, b];
        let split_first = [a];
        let split_second = [b];
        let together = [StableNeighborRow::new(VId(1), &together_incidences)];
        let split = [
            StableNeighborRow::new(VId(1), &split_first),
            StableNeighborRow::new(VId(1), &split_second),
        ];
        let with_empty = [
            StableNeighborRow::new(VId(1), &together_incidences),
            StableNeighborRow::new(VId(2), &[]),
        ];

        let together_digest = digest_stable_neighbor_run(&together, GENEROUS)?;
        assert_ne!(
            together_digest,
            digest_stable_neighbor_run(&split, GENEROUS)?
        );
        assert_ne!(
            together_digest,
            digest_stable_neighbor_run(&with_empty, GENEROUS)?
        );
        Ok(())
    }

    #[test]
    fn malformed_encoded_rows_fail_before_hashing() -> TestResult {
        let neighbors = encoded(NeighborCodec::StreamVByte, &[1, 2])?;
        let mismatched = [EncodedNeighborRow::new(VId(1), &neighbors, &[EId(7)])];
        assert_eq!(
            digest_encoded_neighbor_run(
                &mismatched,
                &|_, value| Some(VId(u128::from(value))),
                GENEROUS,
            ),
            Err(LogicalDigestError::EdgeCountMismatch {
                row_index: 0,
                neighbors: 2,
                edge_ids: 1,
            })
        );

        let edge_ids = [EId(7), EId(8)];
        let unresolved = [EncodedNeighborRow::new(VId(1), &neighbors, &edge_ids)];
        assert_eq!(
            digest_encoded_neighbor_run(
                &unresolved,
                &|_, value| (value != 2).then_some(VId(u128::from(value))),
                GENEROUS,
            ),
            Err(LogicalDigestError::UnresolvedDestination {
                row_index: 0,
                entry_index: 1,
                encoded: 2,
            })
        );
        Ok(())
    }

    #[test]
    fn every_resource_ceiling_is_enforced_before_allocation() {
        let incidence = [StableNeighbor::new(VId(2), EId(3))];
        let rows = [
            StableNeighborRow::new(VId(1), &incidence),
            StableNeighborRow::new(VId(2), &incidence),
        ];
        assert_eq!(
            digest_stable_neighbor_run(&rows, LogicalDigestLimits::new(1, 8, 8, 4096)),
            Err(LogicalDigestError::RowLimitExceeded { rows: 2, limit: 1 })
        );
        assert_eq!(
            digest_stable_neighbor_run(&rows[..1], LogicalDigestLimits::new(1, 0, 8, 4096)),
            Err(LogicalDigestError::RowEntryLimitExceeded {
                row_index: 0,
                entries: 1,
                limit: 0,
            })
        );
        assert_eq!(
            digest_stable_neighbor_run(&rows, LogicalDigestLimits::new(2, 1, 1, 4096)),
            Err(LogicalDigestError::TotalEntryLimitExceeded {
                entries: 2,
                limit: 1,
            })
        );

        let required =
            LOGICAL_NEIGHBOR_RUN_DOMAIN_V1.len() + RUN_HEADER_BYTES + ROW_BYTES + INCIDENCE_BYTES;
        assert_eq!(
            digest_stable_neighbor_run(&rows[..1], LogicalDigestLimits::new(1, 1, 1, required - 1),),
            Err(LogicalDigestError::TranscriptLimitExceeded {
                bytes: required,
                limit: required - 1,
            })
        );
        assert_eq!(
            transcript_len_after_row(usize::MAX - ROW_BYTES + 1, 0),
            None
        );
        assert_eq!(transcript_len_after_row(0, usize::MAX), None);
    }

    #[test]
    fn substitution_errors_name_the_failing_side_and_mismatch() -> TestResult {
        let existing = encoded(NeighborCodec::EliasFano, &[1])?;
        let replacement = encoded(NeighborCodec::StreamVByte, &[2])?;
        let existing_edges = [EId(9)];
        let replacement_edges = [EId(10)];
        let existing_rows = [EncodedNeighborRow::new(VId(1), &existing, &existing_edges)];
        let replacement_rows = [EncodedNeighborRow::new(
            VId(1),
            &replacement,
            &replacement_edges,
        )];
        let identity = |_: usize, value: u64| Some(VId(u128::from(value)));

        assert!(matches!(
            validate_codec_substitution(
                &existing_rows,
                &identity,
                &replacement_rows,
                &identity,
                GENEROUS,
            ),
            Err(CodecSubstitutionError::LogicalMismatch { .. })
        ));

        let unresolved = |_: usize, _: u64| None;
        assert!(matches!(
            validate_codec_substitution(
                &existing_rows,
                &identity,
                &replacement_rows,
                &unresolved,
                GENEROUS,
            ),
            Err(CodecSubstitutionError::Derivation {
                side: CodecSubstitutionSide::Replacement,
                source: LogicalDigestError::UnresolvedDestination { .. },
            })
        ));
        Ok(())
    }
}
