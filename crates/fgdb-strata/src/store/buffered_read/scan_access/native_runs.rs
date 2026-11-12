//! Admission for the opt-in native durable-run source. Metadata selects one
//! implementation before payload I/O; a selected implementation never falls
//! back after a read, allocation, cancellation or control refusal.

use super::*;
use crate::tiered::sealed::{AdjRunSegment, MAX_ADJ_RUN_BYTES, SealedLimits};

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
    /// Only complete history can replace the native historical candidate
    /// domain. A query below the frontier still admits identities born after
    /// that query's cut, exactly as the ordinary Tier-D source does.
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
                return Ok((reference.floor == CommitSeq(0)
                    && reference.resolved_verified_through(self.root.published_at)
                        == self.root.published_at)
                    .then_some(normalized(limits)));
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
    ) -> Result<(AdjRunSegment, MemoryCharge), BufferedScanError<C>> {
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
        let run = self
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
            .map_err(BufferedReadError::from)?
            .ok_or(BufferedReadError::Store(Box::new(
                StoreError::MalformedAdjRun(Box::new(
                    crate::tiered::sealed::AdjRunError::ScopeMismatch,
                )),
            )))?;
        let anchor = run.partition().anchor();
        if anchor.scope().floor != CommitSeq(0)
            || anchor.admitted_envelope_root() != Some(self.root_id)
            || anchor.admitted_publication() != self.root.published_at
            || anchor.verified_through() != self.root.published_at
        {
            return Err(
                BufferedReadError::Store(Box::new(StoreError::MalformedAdjRun(Box::new(
                    crate::tiered::sealed::AdjRunError::ScopeMismatch,
                ))))
                .into(),
            );
        }
        work.step(cx, observe)?;
        Ok((run, charge))
    }
}
