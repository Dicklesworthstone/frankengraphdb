//! Explicit in-core analytics over the same admitted generation and borrowed
//! historical visitors as native GLA reads. No cloned property rows or copied
//! algorithms, new authorization boundary, hidden reduction, or streaming claim.

use super::Cancel;
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError, FnxReadOptions,
    FnxReadResult, FnxSealedExecutionError, FnxSealedReadError, FnxSelection, FnxSourceLimits,
    ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge, ProjectionError, ProjectionLimits,
    ProjectionSpec, SealedGraphView, SealedProjectionError, SealedProjectionSpec, SelfLoopPolicy,
    SnapshotBinding, SnapshotGraphView,
};
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits, SealedPartition};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::mem::size_of;

type Error = FnxReadError<ReadError, Cancel>;
type SealedReadError = FnxSealedReadError<ReadError, Cancel>;

#[cfg(test)]
#[path = "query_prism_sealed_tests.rs"]
mod sealed_tests;

impl<V: Vfs + Clone> Database<V> {
    /// Bind before acquiring a generation, then seal that exact admitted root
    /// and execute its native compressed kernel. Source vertex selection uses
    /// the ordinary historical visitor, not a caller-provided vertex list.
    /// No decoded analytics adjacency or weighted-edge staging is constructed.
    #[allow(clippy::too_many_arguments)]
    pub async fn call_fnx_sealed(
        &self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        self.execute_fnx_sealed(cx, &call, options, memory, sealing).await
    }

    /// Pin once before storage I/O. The temporary image retains history from
    /// the requested cut through this view's publication; it is not installed
    /// as new durable graph authority. Sealing uses Strata's existing verified
    /// reopen/compaction path, which itself materializes its admitted source.
    /// Sealing, vertex staging, projection and kernel have separate budgets.
    /// This async I/O composition does not make the CPU kernel asynchronous or
    /// spill-capable, nor does it replace secure-view admission.
    pub async fn execute_fnx_sealed(
        &self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(Error::Cancelled)?;
            supported_sealed_call(call)?;
            let graph = self.prism_sealed_projection_at(cx, options.as_of, options.selection,
                options.projection, options.projection_limits, options.source_limits, sealing).await?;
            finish_sealed_read(cx, call, &graph, options, memory)
        }).await
    }

    /// Prepare once and reuse the returned image/directory for multiple native
    /// calls. None selects this newly pinned view's frontier. The owned result
    /// does not borrow the writer, so later writes and writer drop cannot
    /// change its vertices, edge weights or source identity. Preparing itself
    /// does not grant a durable lease or make the source larger-than-memory.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_at(
        &self, cx: &QueryCx, as_of: Option<CommitSeq>, selection: FnxSelection,
        spec: ProjectionSpec, limits: ProjectionLimits, source_limits: FnxSourceLimits,
        sealing: SealedLimits,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(Error::Cancelled)?;
            let view = self.read_session().map_err(Error::Read)?;
            let as_of = as_of.unwrap_or(view.frontier());
            view.check_sealed_request(cx, as_of, selection, spec)?;
            // Refuse vertex admission before the potentially expensive seal.
            let mut control = source_control(cx, source_limits);
            let mut staging_bytes = 0usize;
            let vertices = view.select_prism_vertices(as_of, selection, limits, source_limits,
                &mut control, &mut staging_bytes)?;
            let partition = self.store.seal_partition(cx, view.partition_root(), as_of, sealing)
                .await.map_err(SealedReadError::Seal)?;
            view.check_sealed_source(&partition, as_of)?;
            SealedGraphView::build(cx, &partition, &vertices,
                SealedProjectionSpec { as_of, selection, projection: spec }, limits)
                .map_err(SealedReadError::Projection)
        }).await
    }

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
            self.read_session()
                .map_err(FnxReadError::Read)?
                .execute_fnx(cx, call, options)
        })
    }
}

impl EmbeddedReadView {
    /// Execute a supplied, authenticated image only under the exact read view
    /// that admits its source root. A newer image is not a substitute for an
    /// older pinned view, even when a historical cut or empty selection agrees.
    #[allow(clippy::too_many_arguments)]
    pub fn call_fnx_sealed(
        &self, cx: &QueryCx, partition: &SealedPartition, text: &str,
        parameters: &FnxParameters, options: FnxReadOptions, memory: FnxMemoryLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        self.execute_fnx_sealed(cx, partition, &call, options, memory)
    }

    pub fn execute_fnx_sealed(
        &self, cx: &QueryCx, partition: &SealedPartition, call: &FnxCallSpec,
        options: FnxReadOptions, memory: FnxMemoryLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Cancelled)?;
            supported_sealed_call(call)?;
            let graph = self.prism_sealed_projection_at(cx, partition,
                options.as_of.unwrap_or(self.frontier()), options.selection, options.projection,
                options.projection_limits, options.source_limits)?;
            finish_sealed_read(cx, call, &graph, options, memory)
        })
    }

    /// Build a reusable compressed projection from this view's visible,
    /// label-selected vertex directory. The retained image supplies edges;
    /// both endpoints must belong to that directory before weight observation.
    /// Cloning the result retains its image independently of this view/writer.
    #[allow(clippy::too_many_arguments)]
    pub fn prism_sealed_projection_at(
        &self, cx: &QueryCx, partition: &SealedPartition, as_of: CommitSeq,
        selection: FnxSelection, spec: ProjectionSpec, limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Cancelled)?;
            self.snapshot.check_frontier(as_of).map_err(Error::Read)?;
            self.check_sealed_source(partition, as_of)?;
            self.check_sealed_request(cx, as_of, selection, spec)?;
            let mut control = source_control(cx, source_limits);
            let mut staging_bytes = 0usize;
            let vertices = self.select_prism_vertices(as_of, selection, limits, source_limits,
                &mut control, &mut staging_bytes)?;
            SealedGraphView::build(cx, partition, &vertices,
                SealedProjectionSpec { as_of, selection, projection: spec }, limits)
                .map_err(SealedReadError::Projection)
        })
    }

    fn check_sealed_request(
        &self, cx: &QueryCx, as_of: CommitSeq, selection: FnxSelection, spec: ProjectionSpec,
    ) -> Result<(), SealedReadError> {
        cx.checkpoint().map_err(Error::Cancelled)?;
        self.snapshot.check_frontier(as_of).map_err(Error::Read)?;
        if spec.directedness != Directedness::Directed {
            return Err(SealedReadError::Projection(SealedProjectionError::UnsupportedDirectedness(spec.directedness)));
        }
        if selection.relation.is_none() {
            return Err(SealedReadError::Projection(SealedProjectionError::RelationRequired));
        }
        Ok(())
    }

    fn check_sealed_source(&self, partition: &SealedPartition, as_of: CommitSeq) -> Result<(), SealedReadError> {
        let scope = partition.anchor().scope();
        if scope.source_root != self.partition_root() || scope.publication != self.frontier()
            || scope.graph != crate::GRAPH || scope.branch != crate::BRANCH || scope.partition != crate::PARTITION {
            return Err(SealedReadError::SourceMismatch);
        }
        if as_of < scope.floor || as_of > scope.publication {
            return Err(SealedReadError::Projection(SealedProjectionError::Read(SealedError::SnapshotOutsideAnchor {
                requested: as_of, floor: scope.floor, publication: scope.publication,
            })));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn select_prism_vertices(
        &self, as_of: CommitSeq, selection: FnxSelection, limits: ProjectionLimits,
        source_limits: FnxSourceLimits, control: &mut impl FnMut(SourceEvent) -> Result<(), Error>,
        staging_bytes: &mut usize,
    ) -> Result<Vec<VId>, Error> {
        let mut vertices = Vec::new();
        source::visit_vertices(&self.snapshot.patches, as_of, control, |row, control| {
            control(SourceEvent::Work)?;
            if selection.vertex_label.is_some_and(|label| row.labels.binary_search(&label).is_err()) {
                return Ok(());
            }
            // The same visitor/order/admission law serves BOTH adapters.
            debug_assert!(vertices.last().is_none_or(|last| *last < row.vid));
            push_staged(&mut vertices, row.vid, "vertices", limits.max_vertices,
                staging_bytes, source_limits.max_staging_bytes)
        })?;
        Ok(vertices)
    }

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
                cx,
                options.as_of.unwrap_or(self.frontier()),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )?;
            let result = call
                .execute(&graph, options.execution_limits, || cx.checkpoint())
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
            self.snapshot
                .check_frontier(as_of)
                .map_err(FnxReadError::Read)?;
            let mut control = source_control(cx, source_limits);
            let mut staging_bytes = 0usize;
            let vertices = self.select_prism_vertices(
                as_of,
                selection,
                limits,
                source_limits,
                &mut control,
                &mut staging_bytes,
            )?;
            let mut edges = Vec::new();
            source::visit_edges_with_properties(
                &self.snapshot,
                as_of,
                &mut control,
                |entry, props, control| {
                    control(SourceEvent::Work)?;
                    if selection
                        .relation
                        .is_some_and(|relation| relation != entry.relation)
                        || vertices.binary_search(&entry.src).is_err()
                        || vertices.binary_search(&entry.dst).is_err()
                    {
                        return Ok(());
                    }
                    if entry.src == entry.dst {
                        match spec.self_loops {
                            SelfLoopPolicy::Drop => return Ok(()),
                            SelfLoopPolicy::Reject => {
                                return Err(FnxReadError::Projection(ProjectionError::SelfLoop(
                                    entry.eid,
                                )));
                            }
                            SelfLoopPolicy::Keep => {}
                        }
                    }
                    let weight = if spec.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                        1.0 // explicitly discard weights BEFORE property observation
                    } else {
                        let value = selection.weight.property_key().and_then(|key| {
                            props
                                .binary_search_by_key(&key, |(key, _)| *key)
                                .ok()
                                .map(|index| &props[index].1)
                        });
                        selection
                            .weight
                            .resolve(value)
                            .map_err(|reason| FnxReadError::Weight {
                                edge: entry.eid,
                                reason,
                            })?
                    };
                    push_staged(
                        &mut edges,
                        ProjectionEdge {
                            eid: entry.eid,
                            source: entry.src,
                            target: entry.dst,
                            weight,
                        },
                        "input edges",
                        limits.max_input_edges,
                        &mut staging_bytes,
                        source_limits.max_staging_bytes,
                    )
                },
            )?;
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            let graph = SnapshotGraphView::build_owned_with_checkpoint(
                SnapshotBinding {
                    root: self.partition_root().0,
                    as_of,
                },
                vertices,
                edges,
                spec,
                limits,
                || cx.checkpoint(),
            )
            .map_err(|error| match error {
                ProjectionBuildError::Cancelled(error) => FnxReadError::Cancelled(error),
                ProjectionBuildError::Projection(error) => FnxReadError::Projection(error),
            })?;
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            Ok(graph)
        })
    }
}

fn supported_sealed_call(call: &FnxCallSpec) -> Result<(), SealedReadError> {
    if !call.supports_sealed_execution() {
        return Err(SealedReadError::Execution(FnxSealedExecutionError::UnsupportedAlgorithm(call.algorithm())));
    }
    Ok(())
}

fn finish_sealed_read(
    cx: &QueryCx, call: &FnxCallSpec, graph: &SealedGraphView, options: FnxReadOptions, memory: FnxMemoryLimits,
) -> Result<FnxReadResult, SealedReadError> {
    let result = call.execute_sealed(cx, graph, options.execution_limits, memory)
        .map_err(SealedReadError::Execution)?;
    Ok(FnxReadResult::bind_selection(result, options.selection))
}

fn source_control(cx: &QueryCx, limits: FnxSourceLimits) -> impl FnMut(SourceEvent) -> Result<(), Error> + '_ {
    let mut work = 0u64;
    let mut scratch = 0u64;
    move |event| {
        cx.checkpoint().map_err(Error::Cancelled)?;
        let (counter, limit, resource) = match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => (&mut work, limits.max_work_units, "work units"),
            SourceEvent::ScratchEntry => (&mut scratch, limits.max_scratch_entries, "scratch entries"),
        };
        *counter = counter.checked_add(1).ok_or(Error::SizeOverflow)?;
        source_admit(resource, u128::from(*counter), u128::from(limit))
    }
}

fn source_admit(resource: &'static str, requested: u128, limit: u128) -> Result<(), Error> {
    if requested > limit {
        Err(FnxReadError::SourceLimit {
            resource,
            limit,
            requested,
        })
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
    let next = output
        .len()
        .checked_add(1)
        .ok_or(FnxReadError::SizeOverflow)?;
    if next > maximum {
        return Err(FnxReadError::Projection(ProjectionError::LimitExceeded {
            resource,
            limit: maximum,
            observed: next,
        }));
    }
    if output.len() == output.capacity() {
        let old_capacity = output.capacity();
        let target = old_capacity.saturating_mul(2).max(4).min(maximum);
        let new_bytes = target
            .checked_mul(size_of::<T>())
            .ok_or(FnxReadError::SizeOverflow)?;
        let peak = staged_bytes
            .checked_add(new_bytes)
            .ok_or(FnxReadError::SizeOverflow)?;
        source_admit("staging bytes", peak as u128, byte_limit as u128)?;
        output
            .try_reserve_exact(target - output.len())
            .map_err(|_| FnxReadError::AllocationFailed)?;
        let added = (output.capacity() - old_capacity)
            .checked_mul(size_of::<T>())
            .ok_or(FnxReadError::SizeOverflow)?;
        *staged_bytes = staged_bytes
            .checked_add(added)
            .ok_or(FnxReadError::SizeOverflow)?;
        source_admit("staging bytes", *staged_bytes as u128, byte_limit as u128)?;
    }
    output.push(value);
    Ok(())
}
