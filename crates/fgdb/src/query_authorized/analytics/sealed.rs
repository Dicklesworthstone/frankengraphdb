//! Compressed analytics for a retained owner with complete visible incidence.
//!
//! Raw image admission counts retained history and incoming-index workspace.
//! Therefore it is usable here only after the current predicates prove that
//! no incidence or field is hidden. Every other scope stays on the ordinary
//! pre-masked source; filtering raw image endpoints would not prove that law.

use super::*;
use crate::EmbeddedReadView;
use fgdb_delta_types::RelationId;
use fgdb_prism::{
    FnxMemoryLimits, FnxSealedExecutionError, SealedGraphView, SealedProjectionError,
    SealedProjectionMask, SealedProjectionSpec,
};
use fgdb_strata::tiered::sealed::{SealedError, SealedPartition};
use fgdb_types::CommitSeq;
use std::sync::Weak;

/// Private host provenance, never a capability response or authority token.
pub(crate) struct Source {
    owner: Weak<Snapshot>,
    binding: SnapshotBinding,
    partition: SealedPartition,
    relation: RelationId,
    memory: FnxMemoryLimits,
}

impl Source {
    pub(crate) fn new(
        view: &EmbeddedReadView,
        partition: &SealedPartition,
        as_of: CommitSeq,
        relation: RelationId,
        memory: FnxMemoryLimits,
    ) -> Self {
        Self {
            owner: Arc::downgrade(&view.snapshot),
            binding: SnapshotBinding {
                root: view.partition_root().0,
                as_of,
            },
            partition: partition.clone(),
            relation,
            memory,
        }
    }

    pub(crate) fn as_of(&self) -> CommitSeq {
        self.binding.as_of
    }

    pub(super) fn eligible(
        &self,
        snapshot: &Arc<Snapshot>,
        binding: SnapshotBinding,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        scope: &PlannerPredicates,
    ) -> bool {
        // Capability-only predicates come first: restricted calls do not
        // inspect image descriptors, counts, weights or source fingerprints.
        scope.sees_all_incidence()
            && scope.sees_all_fields()
            && self.owner.ptr_eq(&Arc::downgrade(snapshot))
            && self.binding == binding
            && options.as_of.is_none_or(|at| at == binding.as_of)
            && options.selection.relation == Some(self.relation)
            && call
                .validate_sealed_projection(options.projection.directedness)
                .is_ok()
    }
}

#[cfg(test)]
thread_local! {
    /// Physical compressed selections, never returned as authorized metadata.
    pub(crate) static EXECUTIONS: Cell<usize> = const { Cell::new(0) };
}

fn invalid_source() -> Error {
    // An earned resident image should not fail structural validation. Its raw
    // anchor, history bounds and internal ordinal must never become a public
    // capability error, even on that defensive failure path.
    Error::Execution(FnxExecutionError::InvalidUpstreamResult)
}

fn projection_error(error: SealedProjectionError) -> Error {
    match error {
        SealedProjectionError::Projection(error) => Error::Projection(error),
        SealedProjectionError::Weight { edge, reason } => Error::Weight { edge, reason },
        SealedProjectionError::Read(SealedError::Interrupted(error)) => {
            control_error(super::super::interrupted(error))
        }
        SealedProjectionError::Read(SealedError::AllocationFailed) => Error::AllocationFailed,
        SealedProjectionError::Read(SealedError::SizeOverflow) => Error::SizeOverflow,
        SealedProjectionError::Read(SealedError::Limit {
            resource,
            requested,
            limit,
        }) => Error::SourceLimit {
            resource,
            requested: requested as u128,
            limit: limit as u128,
        },
        SealedProjectionError::Guard(_)
        | SealedProjectionError::Read(_)
        | SealedProjectionError::RelationRequired
        | SealedProjectionError::UnsupportedDirectedness(_)
        | SealedProjectionError::NonCanonicalVertices
        | SealedProjectionError::UnknownOrdinal(_) => invalid_source(),
    }
}

fn kernel_error(error: FnxSealedExecutionError) -> Error {
    match error {
        FnxSealedExecutionError::Bind(error) => Error::Bind(error),
        FnxSealedExecutionError::Source(error) => projection_error(error),
        FnxSealedExecutionError::Execution(error) => {
            Error::Execution(error.split_cancel().unwrap_or_else(|never| match never {}))
        }
        FnxSealedExecutionError::Cancelled(error) => {
            match error.downcast::<asupersync::error::Error>() {
                Ok(error) => control_error(super::super::interrupted(error)),
                Err(_) => invalid_source(),
            }
        }
        FnxSealedExecutionError::UnsupportedAlgorithm(_)
        | FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(_) => invalid_source(),
    }
}

fn guard(
    failure: &RefCell<Option<Error>>,
    result: Result<(), Error>,
) -> Result<(), SealedProjectionError> {
    result.map_err(|error| {
        if failure.borrow().is_none() {
            *failure.borrow_mut() = Some(error);
        }
        SealedProjectionError::Guard(Box::new(std::io::Error::other(
            "authorized compressed execution refused",
        )))
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute<Clock: FnMut() -> u64>(
    retained: &Source,
    snapshot: &Arc<Snapshot>,
    binding: SnapshotBinding,
    call: &FnxCallSpec,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: impl Observe,
    sink: ResultSink<'_>,
) -> Result<Vec<Vec<FnxValue>>, Error> {
    // This fresh guard precedes the first compressed metadata access. The
    // dispatcher has proved eligibility; the source remains private throughout.
    poll_live(&mut execution.borrow_mut()).map_err(control_error)?;
    if !retained.eligible(snapshot, binding, call, options, scope) {
        return Err(invalid_source());
    }
    #[cfg(test)]
    EXECUTIONS.with(|count| count.set(count.get() + 1));
    let observe = RefCell::new(observe);
    let work = Cell::new(0u64);
    let control = |event| -> Result<(), Error> {
        poll_live(&mut execution.borrow_mut()).map_err(control_error)?;
        execution.borrow_mut().checkpoint().map_err(control_error)?;
        observe.borrow_mut().event(event).map_err(control_error)?;
        let count = work.get().checked_add(1).ok_or(Error::SizeOverflow)?;
        work.set(count);
        source_admit(
            "work units",
            u128::from(count),
            u128::from(options.source_limits.max_work_units),
        )
    };
    let mut vertices = Vec::new();
    let mut staged_bytes = 0;
    let scratch = Cell::new(0u64);
    let record = || -> Result<(), Error> {
        observe
            .borrow_mut()
            .event(SourceEvent::SnapshotRecord)
            .map_err(control_error)?;
        observe
            .borrow_mut()
            .event(SourceEvent::ScratchEntry)
            .map_err(control_error)?;
        let count = scratch.get().checked_add(1).ok_or(Error::SizeOverflow)?;
        scratch.set(count);
        source_admit(
            "scratch entries",
            u128::from(count),
            u128::from(options.source_limits.max_scratch_entries),
        )
    };
    // Even an unselected vertex spends the signed node allowance, matching
    // ordinary authorized source admission before the user's label selector.
    source::visit_vertices(
        &snapshot.patches,
        binding.as_of,
        &mut |_| poll_live(&mut execution.borrow_mut()).map_err(control_error),
        |row, _| {
            control(SourceEvent::Work)?;
            for _ in &row.labels {
                control(SourceEvent::Work)?;
            }
            execution.borrow_mut().node().map_err(control_error)?;
            record()?;
            if options
                .selection
                .vertex_label
                .is_some_and(|label| row.labels.binary_search(&label).is_err())
            {
                return Ok(());
            }
            push_staged(
                &mut vertices,
                row.vid,
                "vertices",
                options.projection_limits.max_vertices,
                &mut staged_bytes,
                options.source_limits.max_staging_bytes,
            )
        },
    )?;
    // Full visibility was proved before reading this count. Old versions and
    // query-excluded endpoints still consume the compressed source's admission;
    // masked scopes cannot enter this path or observe that population.
    poll_live(&mut execution.borrow_mut()).map_err(control_error)?;
    for _ in 0..retained.partition.incidence_count() {
        control(SourceEvent::Work)?;
        record()?;
    }
    let failure = RefCell::new(None);
    let cx = execution.borrow().cx;
    let built = SealedGraphView::build_with_checkpoint(
        cx,
        &retained.partition,
        &vertices,
        SealedProjectionSpec {
            as_of: binding.as_of,
            selection: options.selection,
            projection: options.projection,
        },
        SealedProjectionMask::UNMASKED,
        options.projection_limits,
        || guard(&failure, control(SourceEvent::Work)),
    );
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    let graph = built.map_err(projection_error)?;
    let signed_rows = scope.limits().max_rows;
    let mut limits = options.execution_limits;
    limits.max_result_rows = limits
        .max_result_rows
        .min(usize::try_from(signed_rows).unwrap_or(usize::MAX));
    // Raw cursor opens/pulls, reverse projections, kernel passes and result
    // hashing all retain this same fresh-time guard and cumulative host meter.
    let result =
        call.execute_sealed_rows_with_checkpoint(cx, &graph, limits, retained.memory, || {
            guard(
                &failure,
                (|| {
                    poll_live(&mut execution.borrow_mut()).map_err(control_error)?;
                    execution.borrow_mut().checkpoint().map_err(control_error)?;
                    observe
                        .borrow_mut()
                        .event(SourceEvent::Work)
                        .map_err(control_error)
                })(),
            )
        });
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error);
    }
    let result = result.map_err(kernel_error);
    if let Err(Error::Execution(FnxExecutionError::LimitExceeded {
        resource: "result rows",
        requested,
        ..
    })) = &result
        && *requested as u128 > u128::from(signed_rows)
    {
        execution
            .borrow_mut()
            .deliver(*requested)
            .map_err(control_error)?;
    }
    let mut cursor = result?;
    // Checkpoints are control work, not an instruction counter. Reserve the
    // registered conservative kernel estimate as well before any consumer;
    // neither a result nor its private certificate can escape a failed charge.
    super::super::work_runs(execution, control_error, || {
        let mut live = execution.borrow_mut();
        let mut meter = KernelMeter {
            execution: &mut live,
            observe: &mut *observe.borrow_mut(),
        };
        meter
            .units(cursor.certificate().estimated_work)
            .map_err(control_error)?;
        meter.poll().map_err(control_error)
    })?;
    if let ResultSink::Retained(retained) = sink {
        *retained = Some(cursor);
        return Ok(Vec::new());
    }
    // A retained relational cursor is prepaid by its existing caller. These
    // collected/mapped consumers own that additional pass here instead.
    reserve_cursor_pull(graph.node_count(), execution, |event| {
        observe.borrow_mut().event(event)
    })?;
    let mut rows = Vec::new();
    let mut sink = match sink {
        ResultSink::Mapped(sink) => {
            sink.begin(cursor.remaining(), call.outputs().len())
                .map_err(Error::Execution)?;
            Some(sink)
        }
        ResultSink::Collected => {
            rows.try_reserve_exact(cursor.remaining())
                .map_err(|_| Error::AllocationFailed)?;
            None
        }
        ResultSink::Retained(_) => unreachable!("retained cursor returned above"),
    };
    let mut row = Vec::new();
    while cursor
        .next_row(&mut row, || poll_live(&mut execution.borrow_mut()))
        .map_err(|error| match error {
            FnxExecutionError::Cancelled(error) => control_error(error),
            other => Error::Execution(other),
        })?
    {
        match &mut sink {
            Some(sink) => sink.row(&row).map_err(Error::Execution)?,
            None => rows.push(std::mem::take(&mut row)),
        }
    }
    poll_live(&mut execution.borrow_mut()).map_err(control_error)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_resource_refusals_remain_typed_while_anchor_failures_are_redacted() {
        assert!(matches!(
            projection_error(SealedProjectionError::Read(SealedError::AllocationFailed)),
            Error::AllocationFailed,
        ));
        assert!(matches!(
            projection_error(SealedProjectionError::Read(SealedError::SizeOverflow)),
            Error::SizeOverflow,
        ));
        assert!(matches!(
            kernel_error(FnxSealedExecutionError::Source(
                SealedProjectionError::Read(SealedError::Limit {
                    resource: "incoming workspace bytes",
                    requested: 1024,
                    limit: 1023,
                },)
            )),
            Error::SourceLimit {
                resource: "incoming workspace bytes",
                requested: 1024,
                limit: 1023,
            },
        ));
        for error in [
            SealedError::SnapshotOutsideAnchor {
                requested: CommitSeq(1),
                floor: CommitSeq(2),
                publication: CommitSeq(9),
            },
            SealedError::RelationOutsideAnchor {
                requested: RelationId(1),
                retained: RelationId(2),
            },
            SealedError::ImageMismatch,
        ] {
            assert!(matches!(
                projection_error(SealedProjectionError::Read(error)),
                Error::Execution(FnxExecutionError::InvalidUpstreamResult),
            ));
        }
    }
}
