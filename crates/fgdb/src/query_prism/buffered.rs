//! Bounded Prism projections over authenticated buffered source cursors.
//! Source payloads stay in the extent pool; only selected IDs and scalar weights
//! transfer to the existing builders. Sealed projections retain compressed
//! adjacency rather than decoded CSR. No resident Snapshot is opened.

use super::{BufferedReadError, BufferedReadView};
use crate::gql_exec::source::SourceEvent;
use crate::query::{
    CooperativeSealedBuild, ImmediateSealedBuild, ProjectionRequest, SealedBuild,
    SealedProjectionRequest, SealedProjectionSource, prism_source_control, push_prism_staged,
};
use asupersync::fs::Vfs;
use fgdb_prism::{
    FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError, FnxReadOptions, FnxReadResult,
    FnxSealedReadError, FnxSelection, FnxSourceLimits, ParallelEdgePolicy, ProjectionBuildError,
    ProjectionEdge, ProjectionError, ProjectionLimits, ProjectionSpec, SealedGraphView,
    SealedProjectionError, SealedProjectionSpec, SelfLoopPolicy, SnapshotBinding,
    SnapshotGraphView, SortedVertexLookup,
};
use fgdb_strata::store::{BufferedEdge, BufferedScanError, BufferedScanEvent, BufferedValue};
use fgdb_strata::tiered::buffer::BufferError;
use fgdb_strata::tiered::edge_scan::{
    BufferedEdgeCandidate, BufferedEdgeScanEvent, BufferedEdgeTopology,
};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::num::NonZeroUsize;

type Cancel = Box<asupersync::error::Error>;
type Error = FnxReadError<BufferedReadError, Cancel>;

enum ProjectionSourceEdge {
    Complete(BufferedValue<BufferedEdge>),
    Topology(BufferedEdgeTopology),
}

impl ProjectionSourceEdge {
    fn entry(&self) -> &fgdb_strata::AdjacencyEntry {
        match self {
            Self::Complete(row) => &row.entry,
            Self::Topology(row) => row.entry(),
        }
    }
}

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

    /// Execute the registered compressed kernel over this already-admitted
    /// buffered generation. No writer, resident Snapshot, root reopen or
    /// decoded-adjacency fallback is constructed. Unsupported algorithms and
    /// directions refuse before vertex or edge payloads are faulted.
    ///
    /// Vertex selection uses options.source_limits; Strata's source walk uses
    /// this view's pool/work limits and max_source_incidences for selected RAW
    /// histories. Image, projection, kernel and result limits remain separate.
    /// These bounds do not claim a spillable compactor or whole-operation RSS
    /// ceiling. The result certificate names this view's root and requested cut.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_fnx_sealed(
        &mut self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
    ) -> Result<FnxReadResult, FnxSealedReadError<BufferedReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            call.validate_sealed_projection(options.projection.directedness)
                .map_err(FnxSealedReadError::Execution)?;
            let graph = self
                .prism_sealed_projection_at(
                    cx,
                    options.as_of.unwrap_or(self.frontier()),
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                    sealing,
                    max_source_incidences,
                )
                .await?;
            let result = call
                .execute_sealed(cx, &graph, options.execution_limits, memory)
                .map_err(FnxSealedReadError::Execution)?;
            Ok(FnxReadResult::bind_selection(result, options.selection))
        })
        .await
    }

    /// Build a reusable sealed projection from this view's opaque Strata
    /// admission. The shared historical vertex cursor includes isolates and
    /// applies the label selection at exactly as_of. Strata then faults only
    /// the selected relation and runs its existing compactor/image encoder;
    /// initial buffered open already proved the complete source root.
    ///
    /// The caller cannot substitute a root claim or decoded source rows. Both
    /// vertex selection and the sealed anchor belong to this pinned generation,
    /// even if another handle advances or compacts the database meanwhile.
    /// Refusal returns no partial projection, leaves this reader usable, and
    /// never heals a missing/lagging checkpoint through a writable open.
    /// The owned image may outlive the view. Its storage has the existing
    /// sealing/projection allowances, not the source pool's lifetime charge.
    /// Completed exact requests may share this view's separately charged
    /// compressed cache. All construction limits are part of its key; a hit
    /// skips physical build checkpoints/I/O but always checks live cancellation.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_at(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
    ) -> Result<SealedGraphView, FnxSealedReadError<BufferedReadError, Cancel>> {
        self.prism_sealed_projection_build(
            cx,
            as_of,
            selection,
            spec,
            limits,
            source_limits,
            sealing,
            max_source_incidences,
            ImmediateSealedBuild,
        )
        .await
    }

    /// Prepare the same reusable compressed projection with bounded yields in
    /// directory copying, incoming-index construction and raw projection scans.
    /// Source admission, compaction and image encoding retain synchronous
    /// stages. Only a completed graph enters this owner's cache; cancellation
    /// or future drop leaves the admitted reader and its limits intact.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_cooperative_at(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
        quantum: NonZeroUsize,
    ) -> Result<SealedGraphView, FnxSealedReadError<BufferedReadError, Cancel>> {
        self.prism_sealed_projection_build(
            cx,
            as_of,
            selection,
            spec,
            limits,
            source_limits,
            sealing,
            max_source_incidences,
            CooperativeSealedBuild(quantum),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prism_sealed_projection_build<B: SealedBuild>(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
        mut build: B,
    ) -> Result<SealedGraphView, FnxSealedReadError<BufferedReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            let relation = selection.relation.ok_or(FnxSealedReadError::Projection(
                SealedProjectionError::RelationRequired,
            ))?;
            if as_of > self.frontier() {
                return Err(read_failure(BufferedReadError::BeyondPublication {
                    requested: as_of,
                    publication: self.frontier(),
                })
                .into());
            }
            let request = SealedProjectionRequest {
                projection: ProjectionRequest {
                    binding: SnapshotBinding {
                        root: self.partition_root().0,
                        as_of,
                    },
                    selection,
                    projection: spec,
                    projection_limits: limits,
                    source_limits,
                },
                source: SealedProjectionSource::Buffered {
                    sealing,
                    max_source_incidences,
                },
                query_policy: None,
            };
            if let Some(graph) = self
                .sealed_cache
                .lookup(cx, request)
                .map_err(FnxReadError::Cancelled)?
            {
                return Ok(graph);
            }
            let mut control = prism_source_control(cx, source_limits);
            let mut staged_bytes = 0;
            let vertices = self
                .select_prism_vertices(
                    cx,
                    as_of,
                    selection,
                    limits,
                    source_limits,
                    &mut control,
                    &mut staged_bytes,
                )
                .await?;
            let partition = self
                .partition
                .seal_relation(cx, as_of, relation, sealing, max_source_incidences)
                .await
                .map_err(FnxSealedReadError::Seal)?;
            let scope = partition.anchor().scope();
            let root = self.partition.root();
            if scope.source_root != self.partition_root()
                || scope.publication != self.frontier()
                || scope.graph != root.graph
                || scope.branch != root.branch
                || scope.partition != root.partition
            {
                return Err(FnxSealedReadError::SourceMismatch);
            }
            let graph = build
                .build(
                    cx,
                    &partition,
                    &vertices,
                    SealedProjectionSpec {
                        as_of,
                        selection,
                        projection: spec,
                    },
                    limits,
                )
                .await
                .map_err(FnxSealedReadError::Projection)?;
            self.sealed_cache
                .remember(cx, request, &graph)
                .map_err(FnxReadError::Cancelled)?;
            Ok(graph)
        })
        .await
    }

    /// One vertex-selection implementation for decoded and sealed adapters.
    /// Keep the decoded path's event order and cumulative two-scan allowance.
    #[allow(clippy::too_many_arguments)]
    async fn select_prism_vertices(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        // The buffered scan's observer crosses its awaits, so it must be Send.
        control: &mut (impl FnMut(SourceEvent) -> Result<(), Error> + Send),
        staged_bytes: &mut usize,
    ) -> Result<Vec<VId>, Error> {
        let mut vertices = Vec::new();
        let mut scan = self
            .partition
            .vertex_scan(cx, as_of)
            .map_err(read_failure)?;
        while let Some(candidate) = scan
            .next_topology_candidate_with(cx, &mut |event| {
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
                .is_some_and(|label| row.labels().binary_search(&label).is_err())
            {
                continue;
            }
            debug_assert!(vertices.last().is_none_or(|last| *last < row.vid()));
            push_prism_staged(
                &mut vertices,
                row.vid(),
                "vertices",
                limits.max_vertices,
                staged_bytes,
                source_limits.max_staging_bytes,
            )?;
        }
        Ok(vertices)
    }

    /// Prepare a reusable decoded projection from buffered historical cursors.
    /// Visible selected vertices, including isolates, precede the canonical EId
    /// scan. Relation and both endpoint selections precede weight observation;
    /// self-loop and parallel-edge policies are the ordinary Prism laws.
    /// An explicit relation also prunes authenticated block metadata before
    /// edge payload faults and head allocation. Initial admission still checks
    /// the complete root; all histories of the selected relation remain visible
    /// to the same-cut merge, including later retirement restatements.
    /// Unit weights and CollapseUnit do not fault edge-property sidecars. A
    /// property-weighted projection retains the complete-record source path.
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
    /// Completed exact requests may share this view's separately bounded decoded
    /// cache. Root/cut, selection, graph laws and every construction allowance
    /// remain key dimensions. A hit skips physical source I/O, staging and build
    /// checkpoints, but checks live cancellation before returning. Kernel and
    /// result admissions still apply to every CALL; this is not an authorized
    /// host or a replay of capability work charges.
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
            if as_of > self.frontier() {
                return Err(read_failure(BufferedReadError::BeyondPublication {
                    requested: as_of,
                    publication: self.frontier(),
                }));
            }
            let binding = SnapshotBinding {
                root: self.partition_root().0,
                as_of,
            };
            let request = ProjectionRequest {
                binding,
                selection,
                projection: spec,
                projection_limits: limits,
                source_limits,
            };
            if let Some(graph) = self
                .projection_cache
                .lookup(cx, request)
                .map_err(FnxReadError::Cancelled)?
            {
                return Ok(graph);
            }
            let mut control = prism_source_control(cx, source_limits);
            let mut staged_bytes = 0;
            let vertices = self
                .select_prism_vertices(
                    cx,
                    as_of,
                    selection,
                    limits,
                    source_limits,
                    &mut control,
                    &mut staged_bytes,
                )
                .await?;
            let selected_vertices = SortedVertexLookup::from_sorted(&vertices);
            let mut edges = Vec::new();
            let needs_properties = selection.weight.property_key().is_some()
                && spec.parallel_edges != ParallelEdgePolicy::CollapseUnit;
            {
                let mut scan = match selection.relation {
                    Some(relation) => self.partition.edge_scan_relation(cx, as_of, relation),
                    None => self.partition.edge_scan(cx, as_of),
                }
                .map_err(read_failure)?;
                while let Some(candidate) = {
                    let mut observe = |event| {
                        control(match event {
                            BufferedEdgeScanEvent::Work => SourceEvent::Work,
                            BufferedEdgeScanEvent::Identity(_) => SourceEvent::ScratchEntry,
                        })
                    };
                    if needs_properties {
                        scan.next_candidate_with(cx, &mut observe)
                            .await
                            .map(|candidate| {
                                candidate.map(|candidate| BufferedEdgeCandidate {
                                    eid: candidate.eid,
                                    row: candidate.row.map(ProjectionSourceEdge::Complete),
                                })
                            })
                    } else {
                        scan.next_topology_candidate_with(cx, &mut observe)
                            .await
                            .map(|candidate| {
                                candidate.map(|candidate| BufferedEdgeCandidate {
                                    eid: candidate.eid,
                                    row: candidate.row.map(ProjectionSourceEdge::Topology),
                                })
                            })
                    }
                }
                .map_err(scan_failure)?
                {
                    let Some(row) = candidate.row else {
                        continue;
                    };
                    control(SourceEvent::Work)?;
                    let entry = row.entry();
                    if selection
                        .relation
                        .is_some_and(|relation| relation != entry.relation)
                        || !selected_vertices.contains(entry.src)
                        || !selected_vertices.contains(entry.dst)
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
                        let value = match &row {
                            ProjectionSourceEdge::Complete(row) => {
                                selection.weight.property_key().and_then(|key| {
                                    row.props
                                        .binary_search_by_key(&key, |(key, _)| *key)
                                        .ok()
                                        .map(|index| &row.props[index].1)
                                })
                            }
                            ProjectionSourceEdge::Topology(_) => None,
                        };
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
            self.projection_cache
                .remember(cx, request, &graph)
                .map_err(FnxReadError::Cancelled)?;
            Ok(graph)
        })
        .await
    }
}
