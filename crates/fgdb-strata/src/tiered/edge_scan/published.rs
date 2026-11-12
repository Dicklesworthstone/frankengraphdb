//! A native source over complete, admitted durable relation history.
//! The query operator still owns identity admission, predicates and ordering;
//! this module supplies only historical candidates and same-cut stored rows.

use super::*;
use crate::tiered::sealed::{AdjRunError, SealedEdgeLocator, SealedError};

pub(super) struct RunCursor {
    locator: SealedEdgeLocator,
    relation: RelationId,
    after: Option<EId>,
    admitted_first: Option<EId>,
    pub(super) finished: bool,
    // The entire decoder/image allowance outlives the source and its index.
    _image_charge: MemoryCharge,
}

fn sealed_error<C>(error: SealedError) -> BufferedScanError<C> {
    match error {
        SealedError::Interrupted(error) => BufferedReadError::Interrupted(error),
        SealedError::Limit {
            resource,
            requested,
            limit,
        } => BufferedReadError::Limit {
            resource,
            requested,
            limit,
        },
        SealedError::SizeOverflow => BufferedReadError::SizeOverflow,
        SealedError::Store(error) => BufferedReadError::Store(error),
        error => BufferedReadError::Store(Box::new(crate::store::StoreError::MalformedAdjRun(
            Box::new(AdjRunError::Sealed(error)),
        ))),
    }
    .into()
}

impl<V: Vfs> BufferedEdgeScan<'_, V> {
    pub(super) fn has_published_relation(&self, relation: Option<RelationId>) -> bool {
        self.state
            .as_ref()
            .and_then(|state| state.run.as_ref())
            .is_some_and(|run| relation == Some(run.relation))
    }

    pub(super) async fn prepare_published_run<C: Send>(
        &mut self,
        state: &mut State,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        let Some(relation) = self.relation else {
            return Ok(());
        };
        // Routed/selected-root states are already initialized before advance.
        // Their candidate domain must never be widened to a whole relation.
        if state.initialized || state.incidence.is_some() {
            return Ok(());
        }
        let Some(limits) = self
            .partition
            .native_run_limits_for(cx, relation, &mut self.work, observe)
            .await?
        else {
            return Ok(());
        };
        let mut blocks =
            self.partition
                .edge_scan_relation_blocks(relation, cx, &mut self.work, observe)?;
        let mut first = None;
        while let Some(block) = self.partition.edge_scan_next_relation_block(
            &mut blocks,
            cx,
            &mut self.work,
            observe,
        )? {
            if let Some((eid, _)) = self.partition.edge_scan_first(block, Some(relation)) {
                first = Some(first.map_or(eid, |old: EId| old.min(eid)));
            }
        }
        // Admit the first historical candidate from authenticated descriptors,
        // before loading or decoding FGRN. A zero record allowance therefore
        // refuses before the run can allocate or expose a payload failure.
        if let Some(eid) = first {
            self.work.step(cx, observe)?;
            observe(BufferedEdgeScanEvent::Identity(eid)).map_err(BufferedScanError::Control)?;
        }
        let loaded = self
            .partition
            .native_run(cx, relation, limits, &mut self.work, observe)
            .await?;
        // Declare the reservation first: on every later early return, the
        // decoded run (and then the locator) drops before its allowance.
        let image_charge = loaded.1;
        let run = loaded.0;
        let bytes = SealedEdgeLocator::required_bytes(run.partition().incidence_count())
            .map_err(sealed_error)?;
        self.work.step(cx, observe)?;
        let directory_charge = self.partition.reserve_scan_bytes(cx, bytes)?;
        let locator = SealedEdgeLocator::build_with(
            run.into_partition(),
            relation,
            directory_charge,
            cx,
            &mut || self.work.step(cx, observe),
            sealed_error,
        )?;
        let observed = locator.next_root_with(
            cx,
            self.as_of,
            None,
            &mut || self.work.step(cx, observe),
            sealed_error,
        )?;
        if observed != first {
            return Err(sealed_error(SealedError::ImageMismatch));
        }
        state.run = Some(RunCursor {
            locator,
            relation,
            after: None,
            admitted_first: first,
            finished: false,
            _image_charge: image_charge,
        });
        state.initialized = true;
        Ok(())
    }

    /// Reuse the same admitted locator for nested reads in its relation. A
    /// different relation or an untyped request keeps the independent routed
    /// Tier-D source; it cannot inherit the root's narrower relation scope.
    pub(super) fn published_incident<C>(
        &mut self,
        cx: &QueryCx,
        request: incidence::Incidence,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<Option<BufferedEdgeCandidate>>, BufferedScanError<C>> {
        let Some(run) = self.state.as_ref().and_then(|state| state.run.as_ref()) else {
            return Ok(None);
        };
        if request.relation != Some(run.relation) {
            return Ok(None);
        }
        let eid = run.locator.next_incident_with(
            cx,
            self.as_of,
            request.endpoint,
            request.direction,
            request.after,
            &mut || self.work.step(cx, observe),
            sealed_error,
        )?;
        let Some(eid) = eid else {
            return Ok(Some(None));
        };
        observe(BufferedEdgeScanEvent::Identity(eid)).map_err(BufferedScanError::Control)?;
        row(
            self.partition,
            &mut self.work,
            &run.locator,
            cx,
            self.as_of,
            eid,
            EdgePropertyAccess::Read,
            observe,
        )
        .map(|candidate| Some(Some(candidate)))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn next<V: Vfs, C>(
    partition: &mut BufferedPartition<V>,
    work: &mut EdgeScanWork,
    run: &mut RunCursor,
    cx: &QueryCx,
    as_of: CommitSeq,
    properties: EdgePropertyAccess,
    observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
) -> Result<Option<BufferedEdgeCandidate>, BufferedScanError<C>> {
    let eid = run.locator.next_root_with(
        cx,
        as_of,
        run.after,
        &mut || work.step(cx, observe),
        sealed_error,
    )?;
    let Some(eid) = eid else {
        run.finished = true;
        return Ok(None);
    };
    if let Some(first) = run.admitted_first.take() {
        if first != eid {
            return Err(sealed_error(SealedError::ImageMismatch));
        }
    } else {
        observe(BufferedEdgeScanEvent::Identity(eid)).map_err(BufferedScanError::Control)?;
    }
    let candidate = row(
        partition,
        work,
        &run.locator,
        cx,
        as_of,
        eid,
        properties,
        observe,
    )?;
    run.after = Some(eid);
    Ok(Some(candidate))
}

#[allow(clippy::too_many_arguments)]
fn row<V: Vfs, C>(
    partition: &mut BufferedPartition<V>,
    work: &mut EdgeScanWork,
    locator: &SealedEdgeLocator,
    cx: &QueryCx,
    as_of: CommitSeq,
    eid: EId,
    access: EdgePropertyAccess,
    observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
) -> Result<BufferedEdgeCandidate, BufferedScanError<C>> {
    let visible =
        locator.edge_with(cx, as_of, eid, &mut || work.step(cx, observe), sealed_error)?;
    let row = if let Some(visible) = visible {
        let selected = match access {
            EdgePropertyAccess::Read => visible.properties,
            EdgePropertyAccess::Property(key) => {
                let mut low = 0;
                let mut high = visible.properties.len();
                while low < high {
                    work.step(cx, observe)?;
                    let middle = low + (high - low) / 2;
                    if visible.properties[middle].0 < key {
                        low = middle + 1;
                    } else {
                        high = middle;
                    }
                }
                match visible
                    .properties
                    .get(low)
                    .filter(|property| property.0 == key)
                {
                    Some(property) => std::slice::from_ref(property),
                    None => &[],
                }
            }
            EdgePropertyAccess::Skip => &[],
        };
        // A sealed property row is not constrained to one Tier-D object.
        // Measure the borrowed immutable owner before cloning any selected
        // payload; keep explicit vector/copy slack and check the completed
        // record through the ordinary retained-ownership path below.
        let mut bytes = selected
            .len()
            .checked_mul(size_of::<(PropertyKeyId, CanonicalPropertyValue)>())
            .and_then(|bytes| bytes.checked_add(size_of::<BufferedEdge>()))
            .ok_or(BufferedReadError::SizeOverflow)?;
        for (_, value) in selected {
            let owned = value
                .owned_heap_bytes_controlled(&mut || work.step(cx, observe))?
                .ok_or(BufferedReadError::SizeOverflow)?;
            bytes = bytes
                .checked_add(owned)
                .ok_or(BufferedReadError::SizeOverflow)?;
        }
        let bytes = bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        work.step(cx, observe)?;
        let charge = partition.reserve_scan_bytes(cx, bytes)?;
        let mut props = Vec::new();
        props.try_reserve_exact(selected.len()).map_err(|_| {
            BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
        })?;
        for property in selected {
            work.step(cx, observe)?;
            props.push(property.clone());
        }
        Some(
            BufferedValue::from_reserved(
                BufferedEdge {
                    entry: visible.entry,
                    props,
                },
                charge,
            )
            .retain_scan_edge(&mut || work.step(cx, observe))?,
        )
    } else {
        None
    };
    Ok(BufferedEdgeCandidate { eid, row })
}
