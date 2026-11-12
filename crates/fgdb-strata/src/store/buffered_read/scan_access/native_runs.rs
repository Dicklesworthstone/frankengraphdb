//! Admission for the opt-in native durable-run source. Metadata selects one
//! implementation before payload I/O; a selected implementation never falls
//! back after a read, allocation, cancellation or control refusal.

use super::*;
use crate::tiered::sealed::{
    AdjRunSegment, MAX_ADJ_RUN_BYTES, SealedAnchor, SealedError, SealedLimits,
};

/// Complete post-horizon statements from one admitted pinned root. Only this
/// loader can mint the receipt; no caller supplies a horizon or block list.
/// Original root ordinals and every raw history row remain in order, including
/// versions invisible at the query cut and old births restated by retirement.
pub(crate) struct NativeRunSuffix {
    blocks: Vec<Vec<AdjacencyEntry>>,
    properties: Vec<Option<BlockProps>>,
    anchor: SealedAnchor,
    root: PartitionRootVersion,
    publication: CommitSeq,
    relation: RelationId,
    verified_through: CommitSeq,
    incidences: usize,
    // Every retained allocation is destroyed before its affine reservation.
    _charges: Vec<MemoryCharge>,
    _owner_charge: MemoryCharge,
}

impl NativeRunSuffix {
    pub(crate) fn blocks(&self) -> &[Vec<AdjacencyEntry>] {
        &self.blocks
    }

    pub(crate) fn properties(&self) -> &[Option<BlockProps>] {
        &self.properties
    }

    pub(crate) const fn incidence_count(&self) -> usize {
        self.incidences
    }

    /// The base keeps its original proof horizon. Only this complete suffix
    /// receipt authorizes a combined reader through the pinned publication.
    pub(crate) fn authorize(
        &self,
        anchor: SealedAnchor,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<(), SealedError> {
        if anchor != self.anchor
            || relation != self.relation
            || anchor.relation() != Some(relation)
            || anchor.scope().floor != CommitSeq(0)
            || anchor.admitted_envelope_root() != Some(self.root)
            || anchor.admitted_publication() != self.publication
            || anchor.verified_through() != self.verified_through
            || anchor.scope().publication > self.verified_through
            || self.verified_through >= self.publication
        {
            return Err(SealedError::ImageMismatch);
        }
        if as_of > self.publication {
            return Err(SealedError::SnapshotOutsideAnchor {
                requested: as_of,
                floor: CommitSeq(0),
                publication: self.publication,
            });
        }
        Ok(())
    }
}

fn invalid_run_source() -> BufferedReadError {
    BufferedReadError::Store(Box::new(StoreError::MalformedAdjRun(Box::new(
        crate::tiered::sealed::AdjRunError::ScopeMismatch,
    ))))
}

fn normalized(mut limits: SealedLimits) -> SealedLimits {
    limits.max_rows = limits.max_rows.min(limits.max_incidences);
    limits.max_image_bytes = limits.max_image_bytes.min(MAX_ADJ_RUN_BYTES);
    limits.max_property_bytes = limits.max_property_bytes.min(limits.max_image_bytes);
    limits
}

/// A capacity reservation, not an allocator/RSS measurement. The encoded
/// frame, codec columns/temporary integer vectors, origins, validation sets,
/// incoming index, and nested property decoding coexist during admission.
/// Keep this conservative allowance until the image is dropped, including
/// its incoming index. Encoded extents are deliberately ineligible: their
/// erasure decoder has an independent workspace that this bound does not own.
pub(crate) fn decode_capacity(limits: SealedLimits) -> Result<usize, BufferedReadError> {
    let limits = normalized(limits);
    // Each encoded property byte can conservatively account for a value,
    // map entry and property slot, with spare vector capacity. A recursive
    // decoder holds at most three encoded fields at each admitted level;
    // scalar copies and stable-digest re-encoding have separate slack.
    let property_unit = 4
        * (size_of::<CanonicalPropertyValue>()
            + size_of::<fgdb_types::CanonicalMapEntry>()
            + size_of::<(PropertyKeyId, CanonicalPropertyValue)>())
        + 3 * (fgdb_types::MAX_PROPERTY_NESTING_DEPTH + 1)
        + 32;
    limits
        .max_image_bytes
        .checked_mul(4)
        .and_then(|bytes| limits.max_incidences.checked_mul(4096)?.checked_add(bytes))
        .and_then(|bytes| {
            limits
                .max_property_bytes
                .checked_mul(property_unit)?
                .checked_add(bytes)
        })
        .and_then(|bytes| bytes.checked_add(65_536))
        .ok_or(BufferedReadError::SizeOverflow)
}

impl<V: Vfs> BufferedPartition<V> {
    /// Only a floor-zero base plus its complete admitted suffix can replace
    /// the native historical candidate domain. A query below the frontier
    /// still admits identities born after its cut, as ordinary Tier D does.
    pub(crate) async fn native_run_limits_for<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<SealedLimits>, BufferedScanError<C>> {
        let Some(limits) = self.native_run_limits else {
            return Ok(None);
        };
        if self.store.ecs_coding.is_some() {
            return Ok(None);
        }
        work.step(cx, observe)?;
        let known_legacy = self
            .store
            .root_run_presence
            .lock()
            .ok()
            .is_some_and(|hint| *hint == Some((self.root_id, false)));
        if known_legacy {
            return Ok(None);
        }
        let bytes = self
            .limits
            .max_root_bytes
            .min(crate::root::MAX_ENCODED_ROOT_BYTES);
        let _metadata = self.reserve_scan_bytes(
            cx,
            bytes
                .checked_mul(16)
                .ok_or(BufferedReadError::SizeOverflow)?,
        )?;
        let refs = self
            .store
            .published_adj_run_refs(cx, self.root_id, bytes)
            .await
            .map_err(BufferedReadError::from)?;
        for reference in refs {
            work.step(cx, observe)?;
            if reference.relation == relation {
                return Ok((reference.floor == CommitSeq(0)).then_some(normalized(limits)));
            }
        }
        Ok(None)
    }

    /// Pre-admit the entire decoder profile before reading one FGRN byte.
    /// Work is a conservative capacity debit; actual locator comparisons and
    /// endpoint reads still spend the same cumulative source allowance.
    pub(crate) async fn native_run<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        limits: SealedLimits,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(AdjRunSegment, MemoryCharge, Option<NativeRunSuffix>), BufferedScanError<C>> {
        let limits = normalized(limits);
        let units = limits
            .max_image_bytes
            .checked_add(limits.max_property_bytes)
            .and_then(|units| limits.max_incidences.checked_mul(32)?.checked_add(units))
            .ok_or(BufferedReadError::SizeOverflow)?;
        for _ in 0..units {
            work.step(cx, observe)?;
        }
        let charge = self.reserve_scan_bytes(cx, decode_capacity(limits)?)?;
        let root_bytes = self
            .limits
            .max_root_bytes
            .min(crate::root::MAX_ENCODED_ROOT_BYTES);
        let _metadata = self.reserve_scan_bytes(
            cx,
            root_bytes
                .checked_mul(16)
                .ok_or(BufferedReadError::SizeOverflow)?,
        )?;
        work.step(cx, observe)?;
        let complete = self
            .store
            .get_published_adj_run(
                cx,
                self.root_id,
                relation,
                self.root.published_at,
                limits,
                root_bytes,
            )
            .await
            .map_err(BufferedReadError::from)?;
        let run = match complete {
            Some(run) => run,
            None => {
                work.step(cx, observe)?;
                self.store
                    .get_published_adj_run_post_seal(
                        cx,
                        self.root_id,
                        relation,
                        self.root.published_at,
                        limits,
                        root_bytes,
                    )
                    .await
                    .map_err(BufferedReadError::from)?
                    .ok_or_else(invalid_run_source)?
                    .0
            }
        };
        let anchor = run.partition().anchor();
        if anchor.scope().floor != CommitSeq(0)
            || anchor.relation() != Some(relation)
            || anchor.admitted_envelope_root() != Some(self.root_id)
            || anchor.admitted_publication() != self.root.published_at
            || anchor.scope().publication > anchor.verified_through()
            || anchor.verified_through() > self.root.published_at
        {
            return Err(invalid_run_source().into());
        }
        drop(_metadata);
        let suffix = if anchor.verified_through() < self.root.published_at {
            Some(
                self.native_run_suffix(
                    cx,
                    relation,
                    anchor,
                    run.partition().incidence_count(),
                    limits.max_incidences,
                    work,
                    observe,
                )
                .await?,
            )
        } else {
            None
        };
        work.step(cx, observe)?;
        Ok((run, charge, suffix))
    }

    /// Admit the combined base and raw suffix population before faulting any
    /// suffix payload. RelationBlocks visits ascending root ordinals, which
    /// preserve Strata's last-publication precedence for equal content births.
    #[allow(clippy::too_many_arguments)]
    async fn native_run_suffix<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        anchor: SealedAnchor,
        base_incidences: usize,
        max_incidences: usize,
        work: &mut EdgeScanWork,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<NativeRunSuffix, BufferedScanError<C>> {
        let verified_through = anchor.verified_through();
        let selected = self.edge_scan_relation_blocks(relation, cx, work, observe)?;
        let mut counting = selected;
        let mut count = 0usize;
        let mut incidences = 0usize;
        while let Some(at) = self.edge_scan_next_relation_block(&mut counting, cx, work, observe)? {
            if self.root.blocks[at].last_seq <= verified_through {
                continue;
            }
            count = count
                .checked_add(1)
                .ok_or(BufferedReadError::SizeOverflow)?;
            incidences = incidences
                .checked_add(self.blocks[at].rows)
                .ok_or(BufferedReadError::SizeOverflow)?;
            let combined = base_incidences
                .checked_add(incidences)
                .ok_or(BufferedReadError::SizeOverflow)?;
            limit("native run source incidences", combined, max_incidences)?;
        }

        let bytes = count
            .checked_mul(size_of::<Vec<AdjacencyEntry>>() + size_of::<Option<BlockProps>>())
            .and_then(|bytes| {
                count
                    .checked_mul(size_of::<MemoryCharge>())
                    .and_then(|charges| bytes.checked_add(charges))
            })
            .and_then(|bytes| bytes.checked_add(size_of::<NativeRunSuffix>()))
            .ok_or(BufferedReadError::SizeOverflow)?;
        work.step(cx, observe)?;
        let charge = self.reserve_scan_bytes(cx, bytes)?;
        let mut suffix = NativeRunSuffix {
            blocks: Vec::new(),
            properties: Vec::new(),
            anchor,
            root: self.root_id,
            publication: self.root.published_at,
            relation,
            verified_through,
            incidences,
            _charges: Vec::new(),
            _owner_charge: charge,
        };
        let allocation_error =
            |_| BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes });
        work.step(cx, observe)?;
        suffix
            .blocks
            .try_reserve_exact(count)
            .map_err(allocation_error)?;
        work.step(cx, observe)?;
        suffix
            .properties
            .try_reserve_exact(count)
            .map_err(allocation_error)?;
        work.step(cx, observe)?;
        suffix
            ._charges
            .try_reserve_exact(count)
            .map_err(allocation_error)?;
        let allocated = suffix
            .blocks
            .capacity()
            .checked_mul(size_of::<Vec<AdjacencyEntry>>())
            .and_then(|bytes| {
                suffix
                    .properties
                    .capacity()
                    .checked_mul(size_of::<Option<BlockProps>>())
                    .and_then(|properties| bytes.checked_add(properties))
            })
            .and_then(|bytes| {
                suffix
                    ._charges
                    .capacity()
                    .checked_mul(size_of::<MemoryCharge>())
                    .and_then(|charges| bytes.checked_add(charges))
            })
            .and_then(|bytes| bytes.checked_add(size_of::<NativeRunSuffix>()))
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("native run suffix owner bytes", allocated, bytes)?;

        let mut selected = selected;
        while let Some(at) = self.edge_scan_next_relation_block(&mut selected, cx, work, observe)? {
            if self.root.blocks[at].last_seq <= verified_through {
                continue;
            }
            let BufferedValue {
                value: (entries, properties),
                _charge,
            } = self
                .edge_scan_block(cx, at, EdgePropertyAccess::Read, work, observe)
                .await?;
            suffix.blocks.push(entries);
            suffix.properties.push(properties);
            suffix._charges.push(_charge);
        }
        work.step(cx, observe)?;
        Ok(suffix)
    }
}
