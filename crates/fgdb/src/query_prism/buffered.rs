//! Bounded decoded Prism projections over authenticated buffered source cursors.
//! Source payloads stay in the extent pool; only selected IDs and scalar weights
//! transfer to the existing in-core builder. No resident Snapshot is opened.

use super::{BufferedReadError, BufferedReadView};
use crate::gql_exec::source::SourceEvent;
use crate::query::{prism_source_control, push_prism_staged};
use asupersync::fs::Vfs;
use fgdb_prism::{
    FnxCallSpec, FnxParameters, FnxReadError, FnxReadOptions, FnxReadResult, FnxSelection,
    FnxSourceLimits, ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge, ProjectionError,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_strata::store::{BufferedScanError, BufferedScanEvent};
use fgdb_strata::tiered::buffer::BufferError;
use fgdb_strata::tiered::edge_scan::BufferedEdgeScanEvent;
use fgdb_types::{CommitSeq, QueryCx};

type Cancel = Box<asupersync::error::Error>;
type Error = FnxReadError<BufferedReadError, Cancel>;

fn read_failure(error: BufferedReadError) -> Error {
    match error {
        BufferedReadError::Interrupted(cancel)
        | BufferedReadError::Buffer(BufferError::Cancelled(cancel)) => {
            FnxReadError::Cancelled(cancel)
        }
        other => FnxReadError::Read(other),
    }
}

fn scan_failure(error: BufferedScanError<Error>) -> Error {
    match error {
        BufferedScanError::Read(error) => read_failure(error),
        BufferedScanError::Control(error) => error,
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Bind a standalone registered CALL, then execute on this authenticated
    /// buffered generation. Binding errors precede all source reads. This is
    /// an owner-level API, not a capability-authorized GQL procedure host.
    pub async fn call_fnx(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<BufferedReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(FnxReadError::Bind)?;
        self.execute_fnx(cx, &call, options).await
    }

    /// Execute an already bound CALL without loading a resident database.
    /// Both scans and the result certificate use this view's immutable root
    /// and exact requested cut. None selects its frontier; future cuts refuse.
    ///
    /// The source cache, history merge and decoded source rows use this view's
    /// MemoryPool. Selected ID/weighted-edge staging and the decoded CSR use
    /// the separate source/projection limits in options. Kernel work/results
    /// retain the ordinary execution limits. These are finite admission bounds,
    /// not an external-memory kernel, spill path or new global projection cache.
    pub async fn execute_fnx(
        &mut self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
    ) -> Result<FnxReadResult, FnxReadError<BufferedReadError, Cancel>> {
        cx.with_restriction_async(async {
            let graph = self
                .prism_projection_at(
                    cx,
                    options.as_of.unwrap_or(self.frontier()),
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                )
                .await?;
            let result = call
                .execute(&graph, options.execution_limits, || cx.checkpoint())
                .map_err(FnxReadError::Execution)?;
            Ok(FnxReadResult::bind_selection(result, options.selection))
        })
        .await
    }

    /// Prepare a reusable decoded projection from buffered historical cursors.
    /// Visible selected vertices, including isolates, precede the canonical EId
    /// scan. Relation and both endpoint selections precede weight observation;
    /// self-loop and parallel-edge policies are the ordinary Prism laws.
    /// An explicit relation also prunes authenticated block metadata before
    /// edge payload faults and head allocation. Initial admission still checks
    /// the complete root; all histories of the selected relation remain visible
    /// to the same-cut merge, including later retirement restatements.
    ///
    /// Every buffered Work event and selection visit charges source work. Each
    /// merged VId/EId candidate charges one source scratch entry, including
    /// invisible candidates; this logical allowance is cumulative across both
    /// scans. Physical history/head/payload storage is separately charged to
    /// the view's MemoryPool, and each cursor retains its BufferedReadLimits.
    /// This is not the resident visitor's physical work transcript.
    ///
    /// The builder takes ownership of the bounded selected buffers. Its returned
    /// clone-shared CSR may outlive the buffered view and its source pool; its
    /// decoded allocations belong to the explicit projection workspace budget.
    /// No complete property table, edge-winner map, resident Snapshot, silent
    /// fallback or cross-process retention lease is constructed here.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_projection_at(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
    ) -> Result<SnapshotGraphView, FnxReadError<BufferedReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            let binding = SnapshotBinding {
                root: self.partition_root().0,
                as_of,
            };
            let mut control = prism_source_control(cx, source_limits);
            let mut staged_bytes = 0;
            let mut vertices = Vec::new();
            {
                let mut scan = self
                    .partition
                    .vertex_scan(cx, as_of)
                    .map_err(read_failure)?;
                while let Some(candidate) = scan
                    .next_candidate_with(cx, &mut |event| {
                        control(match event {
                            BufferedScanEvent::Work => SourceEvent::Work,
                            BufferedScanEvent::Identity(_) => SourceEvent::ScratchEntry,
                        })
                    })
                    .await
                    .map_err(scan_failure)?
                {
                    let Some(row) = candidate.row else {
                        continue;
                    };
                    control(SourceEvent::Work)?;
                    if selection
                        .vertex_label
                        .is_some_and(|label| row.labels.binary_search(&label).is_err())
                    {
                        continue;
                    }
                    debug_assert!(vertices.last().is_none_or(|last| *last < row.vid));
                    push_prism_staged(
                        &mut vertices,
                        row.vid,
                        "vertices",
                        limits.max_vertices,
                        &mut staged_bytes,
                        source_limits.max_staging_bytes,
                    )?;
                }
            }
            let mut edges = Vec::new();
            {
                let mut scan = match selection.relation {
                    Some(relation) => self.partition.edge_scan_relation(cx, as_of, relation),
                    None => self.partition.edge_scan(cx, as_of),
                }
                .map_err(read_failure)?;
                while let Some(candidate) = scan
                    .next_candidate_with(cx, &mut |event| {
                        control(match event {
                            BufferedEdgeScanEvent::Work => SourceEvent::Work,
                            BufferedEdgeScanEvent::Identity(_) => SourceEvent::ScratchEntry,
                        })
                    })
                    .await
                    .map_err(scan_failure)?
                {
                    let Some(row) = candidate.row else {
                        continue;
                    };
                    control(SourceEvent::Work)?;
                    let entry = &row.entry;
                    if selection
                        .relation
                        .is_some_and(|relation| relation != entry.relation)
                        || vertices.binary_search(&entry.src).is_err()
                        || vertices.binary_search(&entry.dst).is_err()
                    {
                        continue;
                    }
                    if entry.src == entry.dst {
                        match spec.self_loops {
                            SelfLoopPolicy::Drop => continue,
                            SelfLoopPolicy::Reject => {
                                return Err(FnxReadError::Projection(ProjectionError::SelfLoop(
                                    entry.eid,
                                )));
                            }
                            SelfLoopPolicy::Keep => {}
                        }
                    }
                    let weight = if spec.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                        1.0
                    } else {
                        let value = selection.weight.property_key().and_then(|key| {
                            row.props
                                .binary_search_by_key(&key, |(key, _)| *key)
                                .ok()
                                .map(|index| &row.props[index].1)
                        });
                        selection
                            .weight
                            .resolve(value)
                            .map_err(|reason| FnxReadError::Weight {
                                edge: entry.eid,
                                reason,
                            })?
                    };
                    push_prism_staged(
                        &mut edges,
                        ProjectionEdge {
                            eid: entry.eid,
                            source: entry.src,
                            target: entry.dst,
                            weight,
                        },
                        "input edges",
                        limits.max_input_edges,
                        &mut staged_bytes,
                        source_limits.max_staging_bytes,
                    )?;
                }
            }
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            let graph = SnapshotGraphView::build_owned_with_checkpoint(
                binding,
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
        .await
    }
}
