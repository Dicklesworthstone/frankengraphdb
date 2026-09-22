//! Explicit in-core analytics over the same admitted generation and borrowed
//! historical visitors as native GLA reads. No cloned property rows or copied
//! algorithms, new authorization boundary, hidden reduction, or streaming claim.

use super::Cancel;
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_prism::{
    FnxCallSpec, FnxParameters, FnxReadError, FnxReadOptions, FnxReadResult,
    FnxSelection, FnxSourceLimits, ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge,
    ProjectionError, ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
    SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{CommitSeq, QueryCx};
use std::mem::size_of;

type Error = FnxReadError<ReadError, Cancel>;

impl<V: Vfs + Clone> Database<V> {
    /// Bind a registered CALL before acquiring a generation; then pin exactly
    /// once. This explicit in-core entrypoint does not extend the general GQL
    /// operator/external-memory catalog. Callers must choose the graph laws
    /// and budgets rather than inheriting an implicit multigraph collapse.
    pub fn call_fnx(
        &self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(FnxReadError::Bind)?;
        self.execute_fnx(cx, &call, options)
    }

    /// Execute an already bound CALL against a fresh authenticated read view.
    /// A fenced writer cannot mint a view, even for an empty projection.
    pub fn execute_fnx(
        &self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            self.read_session().map_err(FnxReadError::Read)?.execute_fnx(cx, call, options)
        })
    }
}

impl EmbeddedReadView {
    pub fn call_fnx(
        &self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(FnxReadError::Bind)?;
        self.execute_fnx(cx, &call, options)
    }

    /// The retained view remains the sole data source across writer updates,
    /// compaction, fencing or drop. Future cuts refuse rather than clamp.
    pub fn execute_fnx(
        &self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            let graph = self.prism_projection_at(
                cx, options.as_of.unwrap_or(self.frontier()), options.selection,
                options.projection, options.projection_limits, options.source_limits,
            )?;
            let result = call.execute(&graph, options.execution_limits, || cx.checkpoint())
                .map_err(FnxReadError::Execution)?;
            Ok(FnxReadResult::bind_selection(result, options.selection))
        })
    }

    /// Prepare a clone-shared graph once and reuse it for several bound calls.
    /// Only selected VIds and scalar edge weights are staged. Properties stay
    /// borrowed in the admitted snapshot; no vertex or property row is cloned.
    /// Staged buffers transfer into the builder instead of being copied again;
    /// their full capacities enter projection admission. Projection sorting and
    /// assembly checkpoint under this same query context and pinned read view.
    /// The cache owns its decoded data and can outlive this read-view handle.
    /// The source's work/scratch counters are separate from the cache budget;
    /// neither the source nor fnx is presented as a spill-capable operator.
    pub fn prism_projection_at(
        &self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
    ) -> Result<SnapshotGraphView, FnxReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            self.snapshot.check_frontier(as_of).map_err(FnxReadError::Read)?;
            let mut work = 0u64;
            let mut scratch = 0u64;
            let mut control = |event| -> Result<(), Error> {
                cx.checkpoint().map_err(FnxReadError::Cancelled)?;
                let (counter, limit, resource) = match event {
                    SourceEvent::Work | SourceEvent::SnapshotRecord => (&mut work, source_limits.max_work_units, "work units"),
                    SourceEvent::ScratchEntry => (&mut scratch, source_limits.max_scratch_entries, "scratch entries"),
                };
                *counter = counter.checked_add(1).ok_or(FnxReadError::SizeOverflow)?;
                source_admit(resource, u128::from(*counter), u128::from(limit))
            };
            let mut vertices = Vec::new();
            let mut staging_bytes = 0usize;
            source::visit_vertices(&self.snapshot.patches, as_of, &mut control, |row, control| {
                control(SourceEvent::Work)?;
                if selection.vertex_label.is_some_and(|label| row.labels.binary_search(&label).is_err()) {
                    return Ok(());
                }
                // Check the visitor's ordered-emission invariant incrementally,
                // not in an uninterruptible post-scan debug assertion.
                debug_assert!(vertices.last().is_none_or(|last| *last < row.vid));
                push_staged(&mut vertices, row.vid, "vertices", limits.max_vertices,
                    &mut staging_bytes, source_limits.max_staging_bytes)
            })?;
            let mut edges = Vec::new();
            source::visit_edges_with_properties(&self.snapshot, as_of, &mut control, |entry, props, control| {
                control(SourceEvent::Work)?;
                if selection.relation.is_some_and(|relation| relation != entry.relation)
                    || vertices.binary_search(&entry.src).is_err()
                    || vertices.binary_search(&entry.dst).is_err() {
                    return Ok(());
                }
                if entry.src == entry.dst {
                    match spec.self_loops {
                        SelfLoopPolicy::Drop => return Ok(()),
                        SelfLoopPolicy::Reject => return Err(FnxReadError::Projection(ProjectionError::SelfLoop(entry.eid))),
                        SelfLoopPolicy::Keep => {}
                    }
                }
                let weight = if spec.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                    1.0 // explicitly discard weights BEFORE property observation
                } else {
                    let value = selection.weight.property_key().and_then(|key| {
                        props.binary_search_by_key(&key, |(key, _)| *key).ok().map(|index| &props[index].1)
                    });
                    selection.weight.resolve(value).map_err(|reason| FnxReadError::Weight { edge: entry.eid, reason })?
                };
                push_staged(&mut edges, ProjectionEdge {
                    eid: entry.eid, source: entry.src, target: entry.dst, weight,
                }, "input edges", limits.max_input_edges, &mut staging_bytes, source_limits.max_staging_bytes)
            })?;
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            let graph = SnapshotGraphView::build_owned_with_checkpoint(
                SnapshotBinding { root: self.partition_root().0, as_of },
                vertices, edges, spec, limits, || cx.checkpoint(),
            ).map_err(|error| match error {
                ProjectionBuildError::Cancelled(error) => FnxReadError::Cancelled(error),
                ProjectionBuildError::Projection(error) => FnxReadError::Projection(error),
            })?;
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            Ok(graph)
        })
    }
}

fn source_admit(resource: &'static str, requested: u128, limit: u128) -> Result<(), Error> {
    if requested > limit {
        Err(FnxReadError::SourceLimit { resource, limit, requested })
    } else {
        Ok(())
    }
}

/// Geometric growth with count admission BEFORE allocation; never reserve the
/// caller's entire maximum for a tiny projection. Charge the conservative
/// old+new backing-store peak while a growth might relocate the allocation.
fn push_staged<T>(
    output: &mut Vec<T>,
    value: T,
    resource: &'static str,
    maximum: usize,
    staged_bytes: &mut usize,
    byte_limit: usize,
) -> Result<(), Error> {
    let next = output.len().checked_add(1).ok_or(FnxReadError::SizeOverflow)?;
    if next > maximum {
        return Err(FnxReadError::Projection(ProjectionError::LimitExceeded { resource, limit: maximum, observed: next }));
    }
    if output.len() == output.capacity() {
        let old_capacity = output.capacity();
        let target = old_capacity.saturating_mul(2).max(4).min(maximum);
        let new_bytes = target.checked_mul(size_of::<T>()).ok_or(FnxReadError::SizeOverflow)?;
        let peak = staged_bytes.checked_add(new_bytes).ok_or(FnxReadError::SizeOverflow)?;
        source_admit("staging bytes", peak as u128, byte_limit as u128)?;
        output.try_reserve_exact(target - output.len()).map_err(|_| FnxReadError::AllocationFailed)?;
        let added = (output.capacity() - old_capacity).checked_mul(size_of::<T>()).ok_or(FnxReadError::SizeOverflow)?;
        *staged_bytes = staged_bytes.checked_add(added).ok_or(FnxReadError::SizeOverflow)?;
        source_admit("staging bytes", *staged_bytes as u128, byte_limit as u128)?;
    }
    output.push(value);
    Ok(())
}
