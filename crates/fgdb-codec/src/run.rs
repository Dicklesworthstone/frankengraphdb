//! Bounded registry-independent sealed neighbor runs.
//!
//! This module composes the scalar codec pieces into one immutable lifecycle:
//! seal canonical source rows, locate a row through a sorted identity column,
//! scan or intersect its explicit [`NeighborCodec`] representation, and
//! validate a physical substitution through the stable logical digest.
//!
//! The representation deliberately stops below `AdjRunSegment` framing. It
//! assigns no durable codec IDs, ordinal-map authority, checksum, object
//! identity, visibility metadata, property locators, or security semantics.
//! A Strata owner must supply those registered fields around this validated
//! scalar payload.

#![forbid(unsafe_code)]

use core::fmt;

use fgdb_types::{EId, VId};

use crate::{
    elias_fano::{EliasFano, EliasFanoError, EntryLimit as EliasFanoEntryLimit},
    evidence::{CodecRunRow, EvidenceError},
    identity::{
        IdentityColumn, IdentityColumnDescriptor, IdentityColumnError, IdentityColumnLimits,
        OriginBirthOrder, SortedIdentityColumn,
    },
    kernel::{IdentityColumnKernel, NeighborKernel, ScalarKernels},
    logical::{
        CodecSubstitutionError, DestinationIdResolver, EncodedNeighborRow, LogicalDigestError,
        LogicalDigestLimits, LogicalNeighborRunDigest, digest_encoded_neighbor_run,
        validate_codec_substitution,
    },
    neighbor::{
        EncodedNeighbors, EntryLimit as NeighborEntryLimit, NeighborCodec, NeighborCursor,
        NeighborError,
    },
};

/// Explicit resource ceilings for one registry-independent sealed run.
///
/// Every allocation or materialization performed by this module is governed by
/// one of these fields or by the nested identity/logical limits. There is no
/// implicit default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedRunLimits {
    max_rows: usize,
    max_entries_per_row: usize,
    max_total_entries: usize,
    max_offset_storage_words: usize,
    source_identity: IdentityColumnLimits,
    edge_identity: IdentityColumnLimits,
    logical_digest: LogicalDigestLimits,
}

impl SealedRunLimits {
    /// Creates exact structural, identity-column, and logical-digest ceilings.
    #[must_use]
    pub const fn new(
        max_rows: usize,
        max_entries_per_row: usize,
        max_total_entries: usize,
        max_offset_storage_words: usize,
        source_identity: IdentityColumnLimits,
        edge_identity: IdentityColumnLimits,
        logical_digest: LogicalDigestLimits,
    ) -> Self {
        Self {
            max_rows,
            max_entries_per_row,
            max_total_entries,
            max_offset_storage_words,
            source_identity,
            edge_identity,
            logical_digest,
        }
    }

    /// Maximum number of source rows.
    #[must_use]
    pub const fn max_rows(self) -> usize {
        self.max_rows
    }

    /// Maximum incidences in any source row.
    #[must_use]
    pub const fn max_entries_per_row(self) -> usize {
        self.max_entries_per_row
    }

    /// Maximum incidences across the complete run.
    #[must_use]
    pub const fn max_total_entries(self) -> usize {
        self.max_total_entries
    }

    /// Maximum logical `u64` words retained by the Elias–Fano offset index.
    #[must_use]
    pub const fn max_offset_storage_words(self) -> usize {
        self.max_offset_storage_words
    }

    /// Bounds for the sorted source identity column.
    #[must_use]
    pub const fn source_identity(self) -> IdentityColumnLimits {
        self.source_identity
    }

    /// Bounds for the position-aligned stable EID column.
    #[must_use]
    pub const fn edge_identity(self) -> IdentityColumnLimits {
        self.edge_identity
    }

    /// Bounds for canonical stable-tuple digest validation.
    #[must_use]
    pub const fn logical_digest(self) -> LogicalDigestLimits {
        self.logical_digest
    }
}

/// One borrowed source row supplied to [`SealedNeighborRun::try_seal`].
///
/// `encoded_destinations` are physical scalars interpreted by the caller's
/// [`DestinationIdResolver`]. They must be strictly increasing because every
/// current neighbor-codec arm represents a set-ordered scalar sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedNeighborRowInput<'a> {
    source: VId,
    encoded_destinations: &'a [u64],
    stable_edge_ids: &'a [EId],
    origin_birth_orders: &'a [OriginBirthOrder<EId>],
    codec: NeighborCodec,
}

impl<'a> SealedNeighborRowInput<'a> {
    /// Creates one aligned source-row input.
    #[must_use]
    pub const fn new(
        source: VId,
        encoded_destinations: &'a [u64],
        stable_edge_ids: &'a [EId],
        origin_birth_orders: &'a [OriginBirthOrder<EId>],
        codec: NeighborCodec,
    ) -> Self {
        Self {
            source,
            encoded_destinations,
            stable_edge_ids,
            origin_birth_orders,
            codec,
        }
    }

    /// Stable source identity.
    #[must_use]
    pub const fn source(self) -> VId {
        self.source
    }

    /// Strictly increasing physical destination scalars.
    #[must_use]
    pub const fn encoded_destinations(self) -> &'a [u64] {
        self.encoded_destinations
    }

    /// Stable EIDs aligned with physical destination positions.
    #[must_use]
    pub const fn stable_edge_ids(self) -> &'a [EId] {
        self.stable_edge_ids
    }

    /// Immutable origin-order tuples aligned with the EID column.
    #[must_use]
    pub const fn origin_birth_orders(self) -> &'a [OriginBirthOrder<EId>] {
        self.origin_birth_orders
    }

    /// Explicit scalar neighbor representation.
    #[must_use]
    pub const fn codec(self) -> NeighborCodec {
        self.codec
    }
}

/// Scalar codec-arm counts retained as deterministic run evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NeighborCodecCounts {
    elias_fano: usize,
    stream_vbyte: usize,
    dense_intervals: usize,
}

impl NeighborCodecCounts {
    fn increment(&mut self, codec: NeighborCodec) -> Result<(), SealedRunError> {
        let target = match codec {
            NeighborCodec::EliasFano => &mut self.elias_fano,
            NeighborCodec::StreamVByte => &mut self.stream_vbyte,
            NeighborCodec::DenseIntervals => &mut self.dense_intervals,
        };
        *target = target.checked_add(1).ok_or(SealedRunError::SizeOverflow {
            calculation: SealedRunSizeCalculation::CodecCount,
        })?;
        Ok(())
    }

    /// Number of lists using `codec`.
    #[must_use]
    pub const fn count(self, codec: NeighborCodec) -> usize {
        match codec {
            NeighborCodec::EliasFano => self.elias_fano,
            NeighborCodec::StreamVByte => self.stream_vbyte,
            NeighborCodec::DenseIntervals => self.dense_intervals,
        }
    }
}

/// Deterministic structural evidence attached to a sealed run.
///
/// This is diagnostic evidence, not a registered durable record. In
/// particular, the codec arms are in-memory capability tags and
/// `logical_digest` is not an `ObjectId`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedRunEvidence {
    row_count: usize,
    edge_count: usize,
    offset_storage_words: usize,
    codec_counts: NeighborCodecCounts,
    source_identity: IdentityColumnDescriptor,
    stable_eids: IdentityColumnDescriptor,
    logical_digest: LogicalNeighborRunDigest,
}

impl SealedRunEvidence {
    /// Number of source rows.
    #[must_use]
    pub const fn row_count(self) -> usize {
        self.row_count
    }

    /// Number of edge incidences.
    #[must_use]
    pub const fn edge_count(self) -> usize {
        self.edge_count
    }

    /// Logical storage words retained by the offset index.
    #[must_use]
    pub const fn offset_storage_words(self) -> usize {
        self.offset_storage_words
    }

    /// Per-arm list counts.
    #[must_use]
    pub const fn codec_counts(self) -> NeighborCodecCounts {
        self.codec_counts
    }

    /// Exact scalar descriptor for the source identity column.
    #[must_use]
    pub const fn source_identity(self) -> IdentityColumnDescriptor {
        self.source_identity
    }

    /// Exact scalar descriptor for the stable EID column.
    #[must_use]
    pub const fn stable_eids(self) -> IdentityColumnDescriptor {
        self.stable_eids
    }

    /// Canonical stable-tuple validation digest.
    #[must_use]
    pub const fn logical_digest(self) -> LogicalNeighborRunDigest {
        self.logical_digest
    }
}

/// Existing structured codec rows for both identity-column payloads.
///
/// The symbolic IDs are explicitly non-durable and the rows account only for
/// registry-independent scalar payload bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunIdentityEvidence {
    source_ids: CodecRunRow,
    stable_eids: CodecRunRow,
}

impl RunIdentityEvidence {
    /// Encoded-output evidence for the source identity column.
    #[must_use]
    pub const fn source_ids(&self) -> &CodecRunRow {
        &self.source_ids
    }

    /// Encoded-output evidence for the stable EID column.
    #[must_use]
    pub const fn stable_eids(&self) -> &CodecRunRow {
        &self.stable_eids
    }
}

/// One immutable registry-independent sealed neighbor run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedNeighborRun {
    source_ids: SortedIdentityColumn<VId>,
    offsets: EliasFano,
    neighbor_lists: Vec<EncodedNeighbors>,
    stable_eids: IdentityColumn<EId>,
    origin_birth_orders: Vec<OriginBirthOrder<EId>>,
    logical_digest: LogicalNeighborRunDigest,
    evidence: SealedRunEvidence,
}

impl SealedNeighborRun {
    /// Seals canonical aligned rows under explicit resource limits.
    ///
    /// Source IDs must be strictly increasing, each destination-scalar list
    /// must be strictly increasing, all three position columns must agree in
    /// length, EIDs must be unique across the run, and each origin-order tuple
    /// must name its aligned EID. The supplied resolver is used only to derive
    /// the stable logical digest; its authority remains the caller's concern.
    pub fn try_seal<R>(
        rows: &[SealedNeighborRowInput<'_>],
        resolver: &R,
        limits: SealedRunLimits,
    ) -> Result<Self, SealedRunError>
    where
        R: DestinationIdResolver + ?Sized,
    {
        let plan = preflight_rows(rows, limits)?;
        let offset_count = rows
            .len()
            .checked_add(1)
            .ok_or(SealedRunError::SizeOverflow {
                calculation: SealedRunSizeCalculation::OffsetCount,
            })?;

        let mut source_values = allocate_vec(rows.len(), SealedRunAllocation::SourceIds)?;
        let mut offsets = allocate_vec(offset_count, SealedRunAllocation::Offsets)?;
        let mut neighbor_lists = allocate_vec(rows.len(), SealedRunAllocation::NeighborLists)?;
        let mut edge_values = allocate_vec(plan.total_entries, SealedRunAllocation::StableEdgeIds)?;
        let mut origin_birth_orders =
            allocate_vec(plan.total_entries, SealedRunAllocation::OriginBirthOrders)?;
        let mut codec_counts = NeighborCodecCounts::default();

        offsets.push(0_u64);
        let kernels = ScalarKernels;
        let neighbor_limit = NeighborEntryLimit::new(limits.max_entries_per_row);
        let mut running_entries = 0_usize;

        for (row_index, row) in rows.iter().copied().enumerate() {
            source_values.push(row.source);
            let encoded = kernels
                .build_neighbors(row.codec, row.encoded_destinations, neighbor_limit)
                .map_err(|source| SealedRunError::NeighborConstruction { row_index, source })?;
            codec_counts.increment(row.codec)?;
            neighbor_lists.push(encoded);
            edge_values.extend_from_slice(row.stable_edge_ids);
            origin_birth_orders.extend_from_slice(row.origin_birth_orders);
            running_entries = running_entries
                .checked_add(row.stable_edge_ids.len())
                .ok_or(SealedRunError::SizeOverflow {
                    calculation: SealedRunSizeCalculation::TotalEntries,
                })?;
            offsets.push(u64::try_from(running_entries).map_err(|_| {
                SealedRunError::SizeOverflow {
                    calculation: SealedRunSizeCalculation::OffsetValue,
                }
            })?);
        }

        validate_unique_edges(&edge_values)?;
        let source_ids = SortedIdentityColumn::try_new_with_delta_for_slots(
            &source_values,
            limits.source_identity,
        )
        .map_err(|source| SealedRunError::IdentityColumn {
            column: SealedRunIdentityColumn::SourceIds,
            source,
        })?;
        let stable_eids =
            IdentityColumn::try_new(&edge_values, limits.edge_identity).map_err(|source| {
                SealedRunError::IdentityColumn {
                    column: SealedRunIdentityColumn::StableEdgeIds,
                    source,
                }
            })?;
        let offsets_ef = EliasFano::try_new(&offsets, EliasFanoEntryLimit::new(offset_count))
            .map_err(|source| SealedRunError::OffsetIndex { source })?;
        let offset_storage_words = offsets_ef.logical_storage_words();
        if offset_storage_words > limits.max_offset_storage_words {
            return Err(SealedRunError::OffsetStorageLimitExceeded {
                words: offset_storage_words,
                limit: limits.max_offset_storage_words,
            });
        }

        let digest_rows = borrowed_digest_rows(rows, &neighbor_lists)?;
        let logical_digest =
            digest_encoded_neighbor_run(&digest_rows, resolver, limits.logical_digest)
                .map_err(|source| SealedRunError::LogicalDigest { source })?;
        let evidence = SealedRunEvidence {
            row_count: rows.len(),
            edge_count: plan.total_entries,
            offset_storage_words,
            codec_counts,
            source_identity: source_ids.as_column().descriptor(),
            stable_eids: stable_eids.descriptor(),
            logical_digest,
        };

        Ok(Self {
            source_ids,
            offsets: offsets_ef,
            neighbor_lists,
            stable_eids,
            origin_birth_orders,
            logical_digest,
            evidence,
        })
    }

    /// Number of source rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.neighbor_lists.len()
    }

    /// Number of edge incidences.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.stable_eids.len()
    }

    /// Returns whether the run has no edge incidences.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stable_eids.is_empty()
    }

    /// Canonical stable-tuple validation digest fixed at seal time.
    #[must_use]
    pub const fn logical_digest(&self) -> LogicalNeighborRunDigest {
        self.logical_digest
    }

    /// Deterministic structural evidence fixed at seal time.
    #[must_use]
    pub const fn evidence(&self) -> SealedRunEvidence {
        self.evidence
    }

    /// Sorted source identity column used for point location.
    #[must_use]
    pub const fn source_ids(&self) -> &SortedIdentityColumn<VId> {
        &self.source_ids
    }

    /// Position-aligned stable EID identity column.
    #[must_use]
    pub const fn stable_eids(&self) -> &IdentityColumn<EId> {
        &self.stable_eids
    }

    /// Position-aligned immutable origin-order tuples.
    #[must_use]
    pub fn origin_birth_orders(&self) -> &[OriginBirthOrder<EId>] {
        &self.origin_birth_orders
    }

    /// Returns one canonical row offset.
    #[must_use]
    pub fn offset_at(&self, index: usize) -> Option<usize> {
        self.offsets
            .select(index)
            .and_then(|offset| usize::try_from(offset).ok())
    }

    /// Locates a source row with identity-column lower-bound.
    ///
    /// `Ok(None)` means the source is absent. A structural inconsistency is
    /// returned rather than silently interpreting a wrong row.
    pub fn locate(&self, source: VId) -> Result<Option<SealedNeighborList<'_>>, SealedRunError> {
        let row_index = self.source_ids.lower_bound(source);
        if self.source_ids.get(row_index) != Some(source) {
            return Ok(None);
        }
        self.list_at(row_index).map(Some)
    }

    /// Returns the list at a canonical source-row position.
    pub fn list_at(&self, row_index: usize) -> Result<SealedNeighborList<'_>, SealedRunError> {
        let source = self
            .source_ids
            .get(row_index)
            .ok_or(SealedRunError::RowOutOfBounds {
                row_index,
                rows: self.row_count(),
            })?;
        let (start, end) = self.offset_range(row_index)?;
        let neighbors = self
            .neighbor_lists
            .get(row_index)
            .ok_or(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::NeighborListCount,
                row_index: Some(row_index),
            })?;
        if end.checked_sub(start) != Some(neighbors.len()) {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::RowLength,
                row_index: Some(row_index),
            });
        }
        Ok(SealedNeighborList {
            run: self,
            row_index,
            source,
            start,
            end,
        })
    }

    /// Materializes existing structured evidence for the two identity payloads.
    ///
    /// Both exact payloads and their scalar dispatch path are provenance-bound
    /// by [`ScalarKernels`]. `corpus_id` remains a bounded symbolic diagnostic
    /// label, not a durable identity.
    pub fn try_identity_evidence(
        &self,
        corpus_id: &str,
        max_output_bytes: usize,
    ) -> Result<RunIdentityEvidence, SealedRunError> {
        let kernels = ScalarKernels;
        let source_output = kernels
            .encode_identity_payload(self.source_ids.as_column(), max_output_bytes)
            .map_err(|source| SealedRunError::IdentityColumn {
                column: SealedRunIdentityColumn::SourceIds,
                source,
            })?;
        let edge_output = kernels
            .encode_identity_payload(&self.stable_eids, max_output_bytes)
            .map_err(|source| SealedRunError::IdentityColumn {
                column: SealedRunIdentityColumn::StableEdgeIds,
                source,
            })?;
        let source_ids = CodecRunRow::try_from_kernel_output(
            "sealed-run-source-identity-scalar",
            corpus_id,
            self.row_count(),
            &source_output,
        )
        .map_err(|source| SealedRunError::Evidence { source })?;
        let stable_eids = CodecRunRow::try_from_kernel_output(
            "sealed-run-stable-eid-scalar",
            corpus_id,
            self.edge_count(),
            &edge_output,
        )
        .map_err(|source| SealedRunError::Evidence { source })?;
        Ok(RunIdentityEvidence {
            source_ids,
            stable_eids,
        })
    }

    /// Rechecks every structural column and recomputes the logical digest.
    ///
    /// This detects private-representation corruption before a run is
    /// substituted or handed to an enclosing durable owner.
    pub fn validate_integrity<R>(
        &self,
        resolver: &R,
        logical_limits: LogicalDigestLimits,
    ) -> Result<(), SealedRunError>
    where
        R: DestinationIdResolver + ?Sized,
    {
        self.validate_structure()?;
        let edge_values = self.collect_edge_ids()?;
        validate_unique_edges(&edge_values)?;
        let digest_rows = self.borrowed_digest_rows(&edge_values)?;
        let actual = digest_encoded_neighbor_run(&digest_rows, resolver, logical_limits)
            .map_err(|source| SealedRunError::LogicalDigest { source })?;
        if actual != self.logical_digest {
            return Err(SealedRunError::LogicalDigestMismatch {
                sealed: self.logical_digest,
                actual,
            });
        }
        self.validate_evidence()?;
        Ok(())
    }

    /// Validates a complete physical replacement against stable logical rows.
    ///
    /// The two runs may use different neighbor-codec arms and different
    /// resolver implementations. Equality covers stable source/destination/EID
    /// tuples, order, multiplicity, and row boundaries only; it does not claim
    /// visibility, security projection, properties, or `AnswerContract`.
    pub fn validate_logical_substitution<ExistingResolver, ReplacementResolver>(
        &self,
        existing_resolver: &ExistingResolver,
        replacement: &Self,
        replacement_resolver: &ReplacementResolver,
        logical_limits: LogicalDigestLimits,
    ) -> Result<LogicalNeighborRunDigest, SealedRunError>
    where
        ExistingResolver: DestinationIdResolver + ?Sized,
        ReplacementResolver: DestinationIdResolver + ?Sized,
    {
        self.validate_integrity(existing_resolver, logical_limits)?;
        replacement.validate_integrity(replacement_resolver, logical_limits)?;

        let existing_edges = self.collect_edge_ids()?;
        let replacement_edges = replacement.collect_edge_ids()?;
        let existing_rows = self.borrowed_digest_rows(&existing_edges)?;
        let replacement_rows = replacement.borrowed_digest_rows(&replacement_edges)?;
        validate_codec_substitution(
            &existing_rows,
            existing_resolver,
            &replacement_rows,
            replacement_resolver,
            logical_limits,
        )
        .map_err(|source| SealedRunError::CodecSubstitution { source })
    }

    fn offset_range(&self, row_index: usize) -> Result<(usize, usize), SealedRunError> {
        let next = row_index
            .checked_add(1)
            .ok_or(SealedRunError::SizeOverflow {
                calculation: SealedRunSizeCalculation::OffsetIndex,
            })?;
        let start = self
            .offset_at(row_index)
            .ok_or(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OffsetCount,
                row_index: Some(row_index),
            })?;
        let end = self.offset_at(next).ok_or(SealedRunError::CorruptRun {
            invariant: SealedRunInvariant::OffsetCount,
            row_index: Some(row_index),
        })?;
        if start > end || end > self.edge_count() {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OffsetRange,
                row_index: Some(row_index),
            });
        }
        Ok((start, end))
    }

    fn collect_edge_ids(&self) -> Result<Vec<EId>, SealedRunError> {
        let mut edges = allocate_vec(self.edge_count(), SealedRunAllocation::StableEdgeIds)?;
        for row in 0..self.edge_count() {
            edges.push(
                self.stable_eids
                    .get(row)
                    .ok_or(SealedRunError::CorruptRun {
                        invariant: SealedRunInvariant::StableEdgeColumnLength,
                        row_index: None,
                    })?,
            );
        }
        Ok(edges)
    }

    fn borrowed_digest_rows<'a>(
        &'a self,
        edge_values: &'a [EId],
    ) -> Result<Vec<EncodedNeighborRow<'a>>, SealedRunError> {
        let mut rows = allocate_vec(self.row_count(), SealedRunAllocation::LogicalRows)?;
        for row_index in 0..self.row_count() {
            let source = self
                .source_ids
                .get(row_index)
                .ok_or(SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::SourceColumnLength,
                    row_index: Some(row_index),
                })?;
            let (start, end) = self.offset_range(row_index)?;
            let edge_ids = edge_values
                .get(start..end)
                .ok_or(SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::OffsetRange,
                    row_index: Some(row_index),
                })?;
            let neighbors =
                self.neighbor_lists
                    .get(row_index)
                    .ok_or(SealedRunError::CorruptRun {
                        invariant: SealedRunInvariant::NeighborListCount,
                        row_index: Some(row_index),
                    })?;
            rows.push(EncodedNeighborRow::new(source, neighbors, edge_ids));
        }
        Ok(rows)
    }

    fn validate_structure(&self) -> Result<(), SealedRunError> {
        if self.source_ids.len() != self.neighbor_lists.len() {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::SourceColumnLength,
                row_index: None,
            });
        }
        let expected_offsets =
            self.row_count()
                .checked_add(1)
                .ok_or(SealedRunError::SizeOverflow {
                    calculation: SealedRunSizeCalculation::OffsetCount,
                })?;
        if self.offsets.len() != expected_offsets {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OffsetCount,
                row_index: None,
            });
        }
        if self.offset_at(0) != Some(0) {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OffsetOrigin,
                row_index: None,
            });
        }
        if self.offset_at(self.row_count()) != Some(self.edge_count()) {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OffsetTerminal,
                row_index: None,
            });
        }
        if self.origin_birth_orders.len() != self.edge_count() {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OriginColumnLength,
                row_index: None,
            });
        }
        let mut previous_source = None;
        for row_index in 0..self.row_count() {
            let list = self.list_at(row_index)?;
            if previous_source.is_some_and(|previous| previous >= list.source) {
                return Err(SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::SourceOrder,
                    row_index: Some(row_index),
                });
            }
            previous_source = Some(list.source);
            let mut cursor = list.cursor();
            let mut decoded = 0_usize;
            while cursor.try_next()?.is_some() {
                decoded = decoded.checked_add(1).ok_or(SealedRunError::SizeOverflow {
                    calculation: SealedRunSizeCalculation::DecodedEntries,
                })?;
            }
            if decoded != list.len() {
                return Err(SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::RowLength,
                    row_index: Some(row_index),
                });
            }
        }
        Ok(())
    }

    fn validate_evidence(&self) -> Result<(), SealedRunError> {
        let mut codec_counts = NeighborCodecCounts::default();
        for list in &self.neighbor_lists {
            codec_counts.increment(list.codec())?;
        }
        let expected = SealedRunEvidence {
            row_count: self.row_count(),
            edge_count: self.edge_count(),
            offset_storage_words: self.offsets.logical_storage_words(),
            codec_counts,
            source_identity: self.source_ids.as_column().descriptor(),
            stable_eids: self.stable_eids.descriptor(),
            logical_digest: self.logical_digest,
        };
        if expected != self.evidence {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::Evidence,
                row_index: None,
            });
        }
        Ok(())
    }
}

/// Borrowed located source list within a sealed run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedNeighborList<'a> {
    run: &'a SealedNeighborRun,
    row_index: usize,
    source: VId,
    start: usize,
    end: usize,
}

impl<'a> SealedNeighborList<'a> {
    /// Zero-based source-row position.
    #[must_use]
    pub const fn row_index(self) -> usize {
        self.row_index
    }

    /// Stable source identity.
    #[must_use]
    pub const fn source(self) -> VId {
        self.source
    }

    /// Number of edge incidences in this row.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    /// Returns whether this source has no incidences.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Explicit scalar neighbor representation.
    #[must_use]
    pub fn codec(self) -> NeighborCodec {
        self.neighbors().codec()
    }

    /// Position of the first incidence in the flattened columns.
    #[must_use]
    pub const fn global_start(self) -> usize {
        self.start
    }

    /// Exclusive flattened-column end.
    #[must_use]
    pub const fn global_end(self) -> usize {
        self.end
    }

    /// Underlying immutable neighbor representation.
    #[must_use]
    pub fn neighbors(self) -> &'a EncodedNeighbors {
        &self.run.neighbor_lists[self.row_index]
    }

    /// Creates a fused aligned scan cursor.
    #[must_use = "a sealed-list cursor does no work until try_next is called"]
    pub fn cursor(self) -> SealedNeighborCursor<'a> {
        SealedNeighborCursor {
            row_index: self.row_index,
            inner: self.neighbors().cursor(),
            stable_eids: &self.run.stable_eids,
            origin_birth_orders: &self.run.origin_birth_orders,
            global_index: self.start,
            end: self.end,
            fused: false,
        }
    }

    /// Intersects physical destination scalars under an explicit output bound.
    ///
    /// Both lists must use the same caller-defined physical scalar universe.
    /// This method does not infer stable-ID map authority or claim a logical
    /// graph intersection.
    pub fn intersect_physical(
        self,
        other: Self,
        output_limit: NeighborEntryLimit,
    ) -> Result<Vec<u64>, SealedRunError> {
        self.neighbors()
            .intersection(other.neighbors(), output_limit)
            .map_err(|source| SealedRunError::Intersection { source })
    }

    /// Returns whether a physical destination scalar is present.
    #[must_use]
    pub fn contains_physical(self, encoded_destination: u64) -> bool {
        self.neighbors().contains(encoded_destination)
    }

    /// Returns one physical destination scalar by logical position.
    #[must_use]
    pub fn select_physical(self, index: usize) -> Option<u64> {
        self.neighbors().select(index)
    }
}

/// One aligned incidence produced by [`SealedNeighborCursor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedNeighborEntry {
    encoded_destination: u64,
    stable_edge_id: EId,
    origin_birth_order: OriginBirthOrder<EId>,
}

impl SealedNeighborEntry {
    /// Caller-defined physical destination scalar.
    #[must_use]
    pub const fn encoded_destination(self) -> u64 {
        self.encoded_destination
    }

    /// Stable edge identity aligned with the destination position.
    #[must_use]
    pub const fn stable_edge_id(self) -> EId {
        self.stable_edge_id
    }

    /// Immutable origin order aligned with the same edge identity.
    #[must_use]
    pub const fn origin_birth_order(self) -> OriginBirthOrder<EId> {
        self.origin_birth_order
    }
}

/// Allocation-bounded, fused scan over aligned run columns.
#[must_use = "a sealed-list cursor does no work until try_next is called"]
pub struct SealedNeighborCursor<'a> {
    row_index: usize,
    inner: NeighborCursor<'a>,
    stable_eids: &'a IdentityColumn<EId>,
    origin_birth_orders: &'a [OriginBirthOrder<EId>],
    global_index: usize,
    end: usize,
    fused: bool,
}

impl SealedNeighborCursor<'_> {
    /// Returns the next aligned incidence.
    ///
    /// Normal exhaustion or any error fuses the cursor.
    pub fn try_next(&mut self) -> Result<Option<SealedNeighborEntry>, SealedRunError> {
        if self.fused {
            return Ok(None);
        }
        if self.global_index >= self.end {
            self.fused = true;
            return Ok(None);
        }
        let entry_index = self.global_index;
        let encoded_destination = match self.inner.try_next() {
            Ok(Some(value)) => value,
            Ok(None) => {
                self.fused = true;
                return Err(SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::RowLength,
                    row_index: Some(self.row_index),
                });
            }
            Err(source) => {
                self.fused = true;
                return Err(SealedRunError::NeighborScan {
                    row_index: self.row_index,
                    entry_index,
                    source,
                });
            }
        };
        let stable_edge_id = self.stable_eids.get(entry_index).ok_or_else(|| {
            self.fused = true;
            SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::StableEdgeColumnLength,
                row_index: Some(self.row_index),
            }
        })?;
        let origin_birth_order = self
            .origin_birth_orders
            .get(entry_index)
            .copied()
            .ok_or_else(|| {
                self.fused = true;
                SealedRunError::CorruptRun {
                    invariant: SealedRunInvariant::OriginColumnLength,
                    row_index: Some(self.row_index),
                }
            })?;
        if origin_birth_order.element_id() != stable_edge_id {
            self.fused = true;
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::OriginEdgeAlignment,
                row_index: Some(self.row_index),
            });
        }
        self.global_index =
            self.global_index
                .checked_add(1)
                .ok_or(SealedRunError::SizeOverflow {
                    calculation: SealedRunSizeCalculation::DecodedEntries,
                })?;
        if self.global_index == self.end {
            self.fused = true;
        }
        Ok(Some(SealedNeighborEntry {
            encoded_destination,
            stable_edge_id,
            origin_birth_order,
        }))
    }

    /// Whether normal exhaustion or an error has fused the cursor.
    #[must_use]
    pub const fn is_fused(&self) -> bool {
        self.fused
    }
}

/// Vector whose allocation failed during run construction or validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealedRunAllocation {
    /// Stable source IDs before identity-column construction.
    SourceIds,
    /// Canonical cumulative row offsets.
    Offsets,
    /// Explicit encoded neighbor lists.
    NeighborLists,
    /// Stable EIDs before identity-column construction.
    StableEdgeIds,
    /// Immutable origin-order tuples.
    OriginBirthOrders,
    /// Temporary EID order used for uniqueness validation.
    EdgeUniqueness,
    /// Borrowed logical-digest row descriptors.
    LogicalRows,
}

/// Checked size calculation performed by the run layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealedRunSizeCalculation {
    /// Sum of incidence counts.
    TotalEntries,
    /// Number of cumulative offsets.
    OffsetCount,
    /// Conversion of a cumulative offset to canonical `u64`.
    OffsetValue,
    /// Addition of a row index and its successor.
    OffsetIndex,
    /// Per-codec row count.
    CodecCount,
    /// Number of decoded scan entries.
    DecodedEntries,
}

/// Identity column named by a typed failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealedRunIdentityColumn {
    /// Sorted stable source IDs.
    SourceIds,
    /// Position-aligned stable edge IDs.
    StableEdgeIds,
}

/// Private invariant checked before use or substitution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealedRunInvariant {
    /// Source identity column length equals neighbor-list count.
    SourceColumnLength,
    /// Source IDs are strictly increasing.
    SourceOrder,
    /// Offset count equals row count plus one.
    OffsetCount,
    /// First offset is zero.
    OffsetOrigin,
    /// Last offset equals the flattened edge count.
    OffsetTerminal,
    /// Every offset range is ordered and in bounds.
    OffsetRange,
    /// Neighbor-list count equals row count.
    NeighborListCount,
    /// Each offset range length equals its neighbor-list length.
    RowLength,
    /// Stable EID column has every referenced row.
    StableEdgeColumnLength,
    /// Origin-order column length equals edge count.
    OriginColumnLength,
    /// Each origin tuple names its aligned stable EID.
    OriginEdgeAlignment,
    /// Attached structural evidence matches private state.
    Evidence,
}

/// Failure while sealing, reading, or validating a neighbor run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SealedRunError {
    /// More source rows than the caller-authorized ceiling.
    RowLimitExceeded {
        /// Supplied rows.
        rows: usize,
        /// Caller ceiling.
        limit: usize,
    },
    /// One row exceeds the caller-authorized incidence ceiling.
    RowEntryLimitExceeded {
        /// Source-row position.
        row_index: usize,
        /// Supplied incidences.
        entries: usize,
        /// Caller ceiling.
        limit: usize,
    },
    /// Total incidences exceed the caller-authorized ceiling.
    TotalEntryLimitExceeded {
        /// Supplied total.
        entries: usize,
        /// Caller ceiling.
        limit: usize,
    },
    /// Position-aligned input columns have different lengths.
    InputColumnLengthMismatch {
        /// Source-row position.
        row_index: usize,
        /// Destination scalars.
        destinations: usize,
        /// Stable EIDs.
        edge_ids: usize,
        /// Origin-order tuples.
        origin_birth_orders: usize,
    },
    /// Source rows are not in strict stable-ID order.
    SourceOrder {
        /// First invalid source-row position.
        row_index: usize,
        /// Preceding stable source.
        previous: VId,
        /// Current stable source.
        current: VId,
    },
    /// An origin-order tuple names a different edge than its aligned EID.
    OriginEdgeMismatch {
        /// Source-row position.
        row_index: usize,
        /// Incidence position within the source row.
        entry_index: usize,
        /// Stable EID column value.
        edge_id: EId,
        /// EID carried by the origin tuple.
        origin_edge_id: EId,
    },
    /// One stable EID occurs more than once in the run.
    DuplicateEdgeId {
        /// Repeated stable identity.
        edge_id: EId,
    },
    /// Checked arithmetic or count conversion failed.
    SizeOverflow {
        /// Named calculation.
        calculation: SealedRunSizeCalculation,
    },
    /// Reserving a preflight-bounded vector failed.
    AllocationFailed {
        /// Vector being allocated.
        target: SealedRunAllocation,
        /// Exact element capacity requested.
        requested: usize,
    },
    /// One explicit neighbor arm rejected its row.
    NeighborConstruction {
        /// Source-row position.
        row_index: usize,
        /// Typed scalar failure.
        source: NeighborError,
    },
    /// A scan encountered private neighbor corruption.
    NeighborScan {
        /// Source-row position.
        row_index: usize,
        /// Flattened incidence position.
        entry_index: usize,
        /// Typed scalar failure.
        source: NeighborError,
    },
    /// Elias–Fano offset construction failed.
    OffsetIndex {
        /// Typed scalar failure.
        source: EliasFanoError,
    },
    /// Elias–Fano offset accounting exceeds the caller ceiling.
    OffsetStorageLimitExceeded {
        /// Required logical storage words.
        words: usize,
        /// Caller ceiling.
        limit: usize,
    },
    /// A typed identity column failed.
    IdentityColumn {
        /// Column being built or encoded.
        column: SealedRunIdentityColumn,
        /// Typed identity failure.
        source: IdentityColumnError,
    },
    /// Stable logical-digest derivation failed.
    LogicalDigest {
        /// Typed logical failure.
        source: LogicalDigestError,
    },
    /// Recomputed logical content differs from the sealed digest.
    LogicalDigestMismatch {
        /// Digest retained at seal time.
        sealed: LogicalNeighborRunDigest,
        /// Digest recomputed from current columns.
        actual: LogicalNeighborRunDigest,
    },
    /// Full codec substitution validation failed.
    CodecSubstitution {
        /// Typed validator failure.
        source: CodecSubstitutionError,
    },
    /// Existing structured evidence construction failed.
    Evidence {
        /// Typed evidence failure.
        source: EvidenceError,
    },
    /// A located row index is outside the run.
    RowOutOfBounds {
        /// Requested row.
        row_index: usize,
        /// Available rows.
        rows: usize,
    },
    /// Physical neighbor intersection failed.
    Intersection {
        /// Typed scalar failure.
        source: NeighborError,
    },
    /// An immutable private invariant did not hold.
    CorruptRun {
        /// Failed invariant.
        invariant: SealedRunInvariant,
        /// Affected row when one is identifiable.
        row_index: Option<usize>,
    },
}

impl fmt::Display for SealedRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowLimitExceeded { rows, limit } => {
                write!(formatter, "sealed run has {rows} rows, limit is {limit}")
            }
            Self::RowEntryLimitExceeded {
                row_index,
                entries,
                limit,
            } => write!(
                formatter,
                "sealed run row {row_index} has {entries} entries, limit is {limit}"
            ),
            Self::TotalEntryLimitExceeded { entries, limit } => write!(
                formatter,
                "sealed run has {entries} total entries, limit is {limit}"
            ),
            Self::InputColumnLengthMismatch {
                row_index,
                destinations,
                edge_ids,
                origin_birth_orders,
            } => write!(
                formatter,
                "sealed run row {row_index} has {destinations} destinations, \
                 {edge_ids} EIDs, and {origin_birth_orders} origin orders"
            ),
            Self::SourceOrder {
                row_index,
                previous,
                current,
            } => write!(
                formatter,
                "sealed run sources are not strictly increasing at row \
                 {row_index}: {previous:?} then {current:?}"
            ),
            Self::OriginEdgeMismatch {
                row_index,
                entry_index,
                edge_id,
                origin_edge_id,
            } => write!(
                formatter,
                "sealed run row {row_index} entry {entry_index} aligns EID \
                 {edge_id:?} with origin EID {origin_edge_id:?}"
            ),
            Self::DuplicateEdgeId { edge_id } => {
                write!(formatter, "sealed run repeats stable EID {edge_id:?}")
            }
            Self::SizeOverflow { calculation } => {
                write!(
                    formatter,
                    "sealed run {calculation:?} arithmetic overflowed"
                )
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} elements for sealed run {target:?}"
            ),
            Self::NeighborConstruction { row_index, source } => {
                write!(
                    formatter,
                    "sealed run row {row_index} neighbor build: {source}"
                )
            }
            Self::NeighborScan {
                row_index,
                entry_index,
                source,
            } => write!(
                formatter,
                "sealed run row {row_index} scan failed at flattened entry \
                 {entry_index}: {source}"
            ),
            Self::OffsetIndex { source } => {
                write!(formatter, "sealed run offset index: {source}")
            }
            Self::OffsetStorageLimitExceeded { words, limit } => write!(
                formatter,
                "sealed run offset index needs {words} logical words, limit is {limit}"
            ),
            Self::IdentityColumn { column, source } => {
                write!(formatter, "sealed run {column:?} identity column: {source}")
            }
            Self::LogicalDigest { source } => {
                write!(formatter, "sealed run logical digest: {source}")
            }
            Self::LogicalDigestMismatch { sealed, actual } => write!(
                formatter,
                "sealed run logical digest changed: sealed={sealed:?}, actual={actual:?}"
            ),
            Self::CodecSubstitution { source } => {
                write!(formatter, "sealed run codec substitution: {source}")
            }
            Self::Evidence { source } => write!(formatter, "sealed run evidence: {source}"),
            Self::RowOutOfBounds { row_index, rows } => {
                write!(
                    formatter,
                    "sealed run row {row_index} is outside {rows} rows"
                )
            }
            Self::Intersection { source } => {
                write!(formatter, "sealed run physical intersection: {source}")
            }
            Self::CorruptRun {
                invariant,
                row_index,
            } => write!(
                formatter,
                "sealed run private invariant {invariant:?} failed at {row_index:?}"
            ),
        }
    }
}

impl std::error::Error for SealedRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NeighborConstruction { source, .. }
            | Self::NeighborScan { source, .. }
            | Self::Intersection { source } => Some(source),
            Self::OffsetIndex { source } => Some(source),
            Self::IdentityColumn { source, .. } => Some(source),
            Self::LogicalDigest { source } => Some(source),
            Self::CodecSubstitution { source } => Some(source),
            Self::Evidence { source } => Some(source),
            Self::RowLimitExceeded { .. }
            | Self::RowEntryLimitExceeded { .. }
            | Self::TotalEntryLimitExceeded { .. }
            | Self::InputColumnLengthMismatch { .. }
            | Self::SourceOrder { .. }
            | Self::OriginEdgeMismatch { .. }
            | Self::DuplicateEdgeId { .. }
            | Self::SizeOverflow { .. }
            | Self::AllocationFailed { .. }
            | Self::OffsetStorageLimitExceeded { .. }
            | Self::LogicalDigestMismatch { .. }
            | Self::RowOutOfBounds { .. }
            | Self::CorruptRun { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SealPlan {
    total_entries: usize,
}

fn preflight_rows(
    rows: &[SealedNeighborRowInput<'_>],
    limits: SealedRunLimits,
) -> Result<SealPlan, SealedRunError> {
    if rows.len() > limits.max_rows {
        return Err(SealedRunError::RowLimitExceeded {
            rows: rows.len(),
            limit: limits.max_rows,
        });
    }
    let mut total_entries = 0_usize;
    let mut previous_source = None;
    for (row_index, row) in rows.iter().copied().enumerate() {
        if previous_source.is_some_and(|previous| previous >= row.source) {
            return Err(SealedRunError::SourceOrder {
                row_index,
                previous: previous_source.unwrap_or(row.source),
                current: row.source,
            });
        }
        previous_source = Some(row.source);
        let entries = row.encoded_destinations.len();
        if entries > limits.max_entries_per_row {
            return Err(SealedRunError::RowEntryLimitExceeded {
                row_index,
                entries,
                limit: limits.max_entries_per_row,
            });
        }
        if entries != row.stable_edge_ids.len() || entries != row.origin_birth_orders.len() {
            return Err(SealedRunError::InputColumnLengthMismatch {
                row_index,
                destinations: entries,
                edge_ids: row.stable_edge_ids.len(),
                origin_birth_orders: row.origin_birth_orders.len(),
            });
        }
        for (entry_index, (&edge_id, origin)) in row
            .stable_edge_ids
            .iter()
            .zip(row.origin_birth_orders)
            .enumerate()
        {
            let origin_edge_id = origin.element_id();
            if origin_edge_id != edge_id {
                return Err(SealedRunError::OriginEdgeMismatch {
                    row_index,
                    entry_index,
                    edge_id,
                    origin_edge_id,
                });
            }
        }
        total_entries = total_entries
            .checked_add(entries)
            .ok_or(SealedRunError::SizeOverflow {
                calculation: SealedRunSizeCalculation::TotalEntries,
            })?;
        if total_entries > limits.max_total_entries {
            return Err(SealedRunError::TotalEntryLimitExceeded {
                entries: total_entries,
                limit: limits.max_total_entries,
            });
        }
    }
    Ok(SealPlan { total_entries })
}

fn borrowed_digest_rows<'a, 'data>(
    inputs: &'a [SealedNeighborRowInput<'data>],
    neighbor_lists: &'a [EncodedNeighbors],
) -> Result<Vec<EncodedNeighborRow<'a>>, SealedRunError>
where
    'data: 'a,
{
    let mut rows = allocate_vec(inputs.len(), SealedRunAllocation::LogicalRows)?;
    for (row_index, (input, neighbors)) in inputs.iter().copied().zip(neighbor_lists).enumerate() {
        if neighbors.len() != input.stable_edge_ids.len() {
            return Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::RowLength,
                row_index: Some(row_index),
            });
        }
        rows.push(EncodedNeighborRow::new(
            input.source,
            neighbors,
            input.stable_edge_ids,
        ));
    }
    Ok(rows)
}

fn validate_unique_edges(edge_values: &[EId]) -> Result<(), SealedRunError> {
    let mut sorted = allocate_vec(edge_values.len(), SealedRunAllocation::EdgeUniqueness)?;
    sorted.extend_from_slice(edge_values);
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0] == pair[1] {
            return Err(SealedRunError::DuplicateEdgeId { edge_id: pair[0] });
        }
    }
    Ok(())
}

fn allocate_vec<T>(capacity: usize, target: SealedRunAllocation) -> Result<Vec<T>, SealedRunError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| SealedRunError::AllocationFailed {
            target,
            requested: capacity,
        })?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use fgdb_types::CommitSeq;

    use super::*;

    fn limits() -> SealedRunLimits {
        SealedRunLimits::new(
            8,
            16,
            64,
            256,
            IdentityColumnLimits::new(8, 8, 256),
            IdentityColumnLimits::new(64, 64, 1_024),
            LogicalDigestLimits::new(8, 16, 64, 8_192),
        )
    }

    fn birth(edge: EId, ordinal: u64) -> OriginBirthOrder<EId> {
        OriginBirthOrder::new(CommitSeq(7), ordinal, 0, edge)
    }

    fn resolver(_row: usize, encoded: u64) -> Option<VId> {
        Some(VId(u128::from(encoded) + 10_000))
    }

    fn fixture(codec_a: NeighborCodec, codec_b: NeighborCodec) -> SealedNeighborRun {
        let first_destinations = [2, 4, 8];
        let first_edges = [EId(101), EId(102), EId(103)];
        let first_births = [
            birth(first_edges[0], 0),
            birth(first_edges[1], 1),
            birth(first_edges[2], 2),
        ];
        let empty_destinations = [];
        let empty_edges = [];
        let empty_births = [];
        let last_destinations = [4, 8, 16];
        let last_edges = [EId(201), EId(202), EId(203)];
        let last_births = [
            birth(last_edges[0], 3),
            birth(last_edges[1], 4),
            birth(last_edges[2], 5),
        ];
        let rows = [
            SealedNeighborRowInput::new(
                VId(10),
                &first_destinations,
                &first_edges,
                &first_births,
                codec_a,
            ),
            SealedNeighborRowInput::new(
                VId(20),
                &empty_destinations,
                &empty_edges,
                &empty_births,
                NeighborCodec::StreamVByte,
            ),
            SealedNeighborRowInput::new(
                VId(30),
                &last_destinations,
                &last_edges,
                &last_births,
                codec_b,
            ),
        ];
        SealedNeighborRun::try_seal(&rows, &resolver, limits()).expect("fixture seals")
    }

    #[test]
    fn seal_locate_and_scan_preserve_aligned_columns() {
        let run = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        assert_eq!(run.row_count(), 3);
        assert_eq!(run.edge_count(), 6);
        assert_eq!(
            (0..=run.row_count())
                .map(|index| run.offset_at(index))
                .collect::<Vec<_>>(),
            [Some(0), Some(3), Some(3), Some(6)]
        );

        let list = run.locate(VId(10)).expect("locate validates").expect("row");
        assert_eq!(list.codec(), NeighborCodec::EliasFano);
        assert_eq!(list.len(), 3);
        let mut cursor = list.cursor();
        let mut scanned = Vec::new();
        while let Some(entry) = cursor.try_next().expect("scan") {
            scanned.push((
                entry.encoded_destination(),
                entry.stable_edge_id(),
                entry.origin_birth_order().element_id(),
            ));
        }
        assert_eq!(
            scanned,
            [
                (2, EId(101), EId(101)),
                (4, EId(102), EId(102)),
                (8, EId(103), EId(103)),
            ]
        );
        assert!(cursor.is_fused());
        assert_eq!(cursor.try_next().expect("fused"), None);
    }

    #[test]
    fn empty_and_missing_lists_are_distinct() {
        let run = fixture(NeighborCodec::StreamVByte, NeighborCodec::DenseIntervals);
        let empty = run.locate(VId(20)).expect("locate").expect("empty row");
        assert!(empty.is_empty());
        assert_eq!(empty.global_start(), 3);
        assert_eq!(empty.global_end(), 3);
        assert_eq!(empty.cursor().try_next().expect("empty scan"), None);
        assert!(run.locate(VId(21)).expect("locate").is_none());
        assert!(run.locate(VId(1)).expect("locate").is_none());
        assert!(run.locate(VId(99)).expect("locate").is_none());
    }

    #[test]
    fn physical_intersection_uses_each_honest_codec_path() {
        let run = fixture(NeighborCodec::StreamVByte, NeighborCodec::DenseIntervals);
        let left = run.locate(VId(10)).expect("locate").expect("left");
        let right = run.locate(VId(30)).expect("locate").expect("right");
        assert_eq!(
            left.intersect_physical(right, NeighborEntryLimit::new(2))
                .expect("intersection"),
            [4, 8]
        );
        assert_eq!(
            left.intersect_physical(right, NeighborEntryLimit::new(1)),
            Err(SealedRunError::Intersection {
                source: NeighborError::IntersectionLimitExceeded { limit: 1 }
            })
        );
    }

    #[test]
    fn all_codec_substitutions_preserve_stable_digest() {
        let codecs = [
            NeighborCodec::EliasFano,
            NeighborCodec::StreamVByte,
            NeighborCodec::DenseIntervals,
        ];
        for existing_codec in codecs {
            for replacement_codec in codecs {
                let existing = fixture(existing_codec, existing_codec);
                let replacement = fixture(replacement_codec, replacement_codec);
                assert_eq!(
                    existing
                        .validate_logical_substitution(
                            &resolver,
                            &replacement,
                            &resolver,
                            limits().logical_digest(),
                        )
                        .expect("equivalent"),
                    existing.logical_digest()
                );
            }
        }
    }

    #[test]
    fn substitution_rejects_resolver_drift() {
        let existing = fixture(NeighborCodec::EliasFano, NeighborCodec::StreamVByte);
        let replacement = fixture(NeighborCodec::DenseIntervals, NeighborCodec::DenseIntervals);
        let shifted = |_row: usize, encoded: u64| Some(VId(u128::from(encoded) + 20_000));
        assert!(matches!(
            existing.validate_logical_substitution(
                &resolver,
                &replacement,
                &shifted,
                limits().logical_digest(),
            ),
            Err(SealedRunError::LogicalDigestMismatch { .. })
        ));
    }

    #[test]
    fn structural_evidence_and_identity_rows_are_deterministic() {
        let left = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        let right = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        assert_eq!(left.evidence(), right.evidence());
        assert_eq!(left.evidence().row_count(), 3);
        assert_eq!(left.evidence().edge_count(), 6);
        assert_eq!(
            left.evidence()
                .codec_counts()
                .count(NeighborCodec::EliasFano),
            1
        );
        assert_eq!(
            left.evidence()
                .codec_counts()
                .count(NeighborCodec::StreamVByte),
            1
        );
        assert_eq!(
            left.evidence()
                .codec_counts()
                .count(NeighborCodec::DenseIntervals),
            1
        );

        let left_rows = left
            .try_identity_evidence("sealed-run-fixture-v1", 1_024)
            .expect("evidence");
        let right_rows = right
            .try_identity_evidence("sealed-run-fixture-v1", 1_024)
            .expect("evidence");
        assert_eq!(left_rows, right_rows);
        assert_eq!(left_rows.source_ids().entry_count(), 3);
        assert_eq!(left_rows.stable_eids().entry_count(), 6);
        assert!(
            left_rows
                .source_ids()
                .to_ndjson()
                .expect("NDJSON")
                .contains("\"dispatch_path\":\"scalar\"")
        );
    }

    #[test]
    fn input_alignment_and_origin_identity_are_rejected_before_encoding() {
        let destinations = [1, 2];
        let edges = [EId(1)];
        let births = [birth(EId(1), 0)];
        let row = SealedNeighborRowInput::new(
            VId(1),
            &destinations,
            &edges,
            &births,
            NeighborCodec::EliasFano,
        );
        assert!(matches!(
            SealedNeighborRun::try_seal(&[row], &resolver, limits()),
            Err(SealedRunError::InputColumnLengthMismatch { .. })
        ));

        let edges = [EId(1), EId(2)];
        let births = [birth(EId(1), 0), birth(EId(3), 1)];
        let row = SealedNeighborRowInput::new(
            VId(1),
            &destinations,
            &edges,
            &births,
            NeighborCodec::EliasFano,
        );
        assert_eq!(
            SealedNeighborRun::try_seal(&[row], &resolver, limits()),
            Err(SealedRunError::OriginEdgeMismatch {
                row_index: 0,
                entry_index: 1,
                edge_id: EId(2),
                origin_edge_id: EId(3),
            })
        );
    }

    #[test]
    fn noncanonical_source_destination_and_duplicate_edge_orders_fail_closed() {
        let destinations = [1, 1];
        let edges = [EId(1), EId(2)];
        let births = [birth(EId(1), 0), birth(EId(2), 1)];
        let row = SealedNeighborRowInput::new(
            VId(1),
            &destinations,
            &edges,
            &births,
            NeighborCodec::StreamVByte,
        );
        assert!(matches!(
            SealedNeighborRun::try_seal(&[row], &resolver, limits()),
            Err(SealedRunError::NeighborConstruction {
                source: NeighborError::NotStrictlyIncreasing { .. },
                ..
            })
        ));

        let destinations_a = [1];
        let destinations_b = [2];
        let duplicate_edge = [EId(7)];
        let births_a = [birth(EId(7), 0)];
        let births_b = [birth(EId(7), 1)];
        let rows = [
            SealedNeighborRowInput::new(
                VId(1),
                &destinations_a,
                &duplicate_edge,
                &births_a,
                NeighborCodec::EliasFano,
            ),
            SealedNeighborRowInput::new(
                VId(2),
                &destinations_b,
                &duplicate_edge,
                &births_b,
                NeighborCodec::DenseIntervals,
            ),
        ];
        assert_eq!(
            SealedNeighborRun::try_seal(&rows, &resolver, limits()),
            Err(SealedRunError::DuplicateEdgeId { edge_id: EId(7) })
        );

        let edges_c = [EId(8)];
        let births_c = [birth(EId(8), 1)];
        let rows = [
            SealedNeighborRowInput::new(
                VId(2),
                &destinations_a,
                &duplicate_edge,
                &births_a,
                NeighborCodec::EliasFano,
            ),
            SealedNeighborRowInput::new(
                VId(1),
                &destinations_b,
                &edges_c,
                &births_c,
                NeighborCodec::DenseIntervals,
            ),
        ];
        assert!(matches!(
            SealedNeighborRun::try_seal(&rows, &resolver, limits()),
            Err(SealedRunError::SourceOrder { row_index: 1, .. })
        ));
    }

    #[test]
    fn every_structural_limit_is_enforced() {
        let destinations = [1, 2, 3];
        let edges = [EId(1), EId(2), EId(3)];
        let births = [birth(EId(1), 0), birth(EId(2), 1), birth(EId(3), 2)];
        let row = SealedNeighborRowInput::new(
            VId(1),
            &destinations,
            &edges,
            &births,
            NeighborCodec::EliasFano,
        );

        let mut bounded = limits();
        bounded.max_rows = 0;
        assert_eq!(
            SealedNeighborRun::try_seal(&[row], &resolver, bounded),
            Err(SealedRunError::RowLimitExceeded { rows: 1, limit: 0 })
        );
        bounded = limits();
        bounded.max_entries_per_row = 2;
        assert_eq!(
            SealedNeighborRun::try_seal(&[row], &resolver, bounded),
            Err(SealedRunError::RowEntryLimitExceeded {
                row_index: 0,
                entries: 3,
                limit: 2,
            })
        );
        bounded = limits();
        bounded.max_total_entries = 2;
        assert_eq!(
            SealedNeighborRun::try_seal(&[row], &resolver, bounded),
            Err(SealedRunError::TotalEntryLimitExceeded {
                entries: 3,
                limit: 2,
            })
        );
        bounded = limits();
        bounded.max_offset_storage_words = 0;
        assert!(matches!(
            SealedNeighborRun::try_seal(&[row], &resolver, bounded),
            Err(SealedRunError::OffsetStorageLimitExceeded { limit: 0, .. })
        ));
    }

    #[test]
    fn integrity_validation_detects_offset_and_logical_corruption() {
        let mut bad_offsets = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        bad_offsets.offsets =
            EliasFano::try_new(&[0, 2, 3, 6], EliasFanoEntryLimit::new(4)).expect("test offsets");
        assert!(matches!(
            bad_offsets.validate_integrity(&resolver, limits().logical_digest()),
            Err(SealedRunError::CorruptRun {
                invariant: SealedRunInvariant::RowLength,
                row_index: Some(0),
            })
        ));

        let mut bad_logical = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        bad_logical.neighbor_lists[0] =
            EncodedNeighbors::try_dense_intervals(&[2, 5, 8], NeighborEntryLimit::new(3))
                .expect("replacement");
        assert!(matches!(
            bad_logical.validate_integrity(&resolver, limits().logical_digest()),
            Err(SealedRunError::LogicalDigestMismatch { .. })
        ));
    }

    #[test]
    fn row_index_and_evidence_output_bounds_fail_closed() {
        let run = fixture(NeighborCodec::EliasFano, NeighborCodec::DenseIntervals);
        assert_eq!(
            run.list_at(3),
            Err(SealedRunError::RowOutOfBounds {
                row_index: 3,
                rows: 3,
            })
        );
        assert!(matches!(
            run.try_identity_evidence("sealed-run-fixture-v1", 0),
            Err(SealedRunError::IdentityColumn {
                source: IdentityColumnError::PayloadLimitExceeded { limit: 0, .. },
                ..
            })
        ));
    }
}
