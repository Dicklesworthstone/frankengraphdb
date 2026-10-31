//! Native GQL composition over one pinned, authenticated compressed source.
//! Preparation owns storage I/O; the ordinary relational executor still owns
//! filters, joins, ordering, pages and exact aggregation. No decoded analytics
//! projection, replacement evaluator or capability boundary is introduced.

use super::*;
use crate::gql_exec::AdmissionUsage;
use crate::{PreparedNativeRead, QueryError, QueryResult};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlParameters, GraphAggregateError,
    GraphAggregateRow, GraphSetExecutionError, GraphSetSource, PreparedGraphSet,
    PreparedGraphSetAggregate,
};
use fgdb_prism::SealedProjectionMask;
use std::cell::RefCell;

type Failure = GqlQueryError<GqlError, Cancel>;
type SetResult = Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<GqlError>, Cancel>,
>;
type AggregateResult = Result<
    GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<GqlError>, Cancel>,
>;

/// Every native class retains its original binding, budget and source errors.
/// A temporal template cannot silently select another source cut, and CALLs
/// always retain the compressed analytics source.
#[derive(Debug)]
pub enum SealedQueryError {
    SnapshotMismatch {
        requested: CommitSeq,
        retained: CommitSeq,
    },
    Query(QueryError),
}

impl core::fmt::Display for SealedQueryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SnapshotMismatch {
                requested,
                retained,
            } => write!(
                f,
                "native read selects {requested:?}, but the sealed owner retains {retained:?}",
            ),
            Self::Query(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for SealedQueryError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::SnapshotMismatch { .. } => None,
            Self::Query(error) => Some(error),
        }
    }
}

impl From<QueryError> for SealedQueryError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

/// A native read view paired with the compressed image earned from that same
/// authenticated generation. Each FnX CALL must explicitly name `relation()`;
/// its label, weights, direction and reduction laws remain query-bound.
/// Pattern and property reads use the same pinned historical cut.
///
/// This is an owner-level API, like `EmbeddedReadView`, not a Warden permission
/// or a server session. It never accepts a caller-supplied graph or vertex list.
/// Preparing the image has separate root/sealing limits. Executing a query
/// charges source records and observed projection work, the kernel's admitted
/// work estimate, and its complete cursor traversal to one GQL policy. Kernel
/// and output backing stores retain the explicit `FnxMemoryLimits` admission.
/// These are finite in-memory bounds, not an external-memory kernel or RSS cap.
#[must_use = "a sealed read view has no effect unless its pinned source is read"]
pub struct SealedReadView {
    view: EmbeddedReadView,
    partition: SealedPartition,
    as_of: CommitSeq,
    relation: RelationId,
}

impl SealedReadView {
    // Only the trusted authorized-session constructor receives these parts.
    // No capability response can acquire a raw read view or image anchor.
    pub(crate) fn authorized_parts(
        &self,
    ) -> (&EmbeddedReadView, &SealedPartition, CommitSeq, RelationId) {
        (&self.view, &self.partition, self.as_of, self.relation)
    }
}

impl core::fmt::Debug for SealedReadView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SealedReadView([REDACTED])")
    }
}

impl Database<UnixVfs> {
    /// Open a compressed native read owner beside a live writer. Selection
    /// requires a current authenticated checkpoint and never repairs storage
    /// or obtains a writer lease. The compressed relation and ordinary GQL
    /// property/pattern source come from that same selected immutable root.
    ///
    /// `None` selects its publication; an explicit cut is exact. Source and
    /// image construction have separate admission limits. One authenticated
    /// source traversal supplies both the compressed relation and decoded GQL
    /// pattern/property source. Both representations remain resident; this is
    /// not an external-memory read owner or an RSS bound.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_sealed_read(
        cx: &CommitCx,
        query_cx: &QueryCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        as_of: Option<CommitSeq>,
        relation: RelationId,
        sealing: SealedLimits,
        source_limits: RootReadLimits,
    ) -> Result<SealedReadView, SealedProjectionOpenError> {
        Self::open_sealed_read_with_vfs(
            cx,
            query_cx,
            UnixVfs::new(),
            path,
            keys,
            as_of,
            relation,
            sealing,
            source_limits,
        )
        .await
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Injected-filesystem equivalent of `open_sealed_read`. A writer may
    /// publish during the source read: it still addresses the one pinned root,
    /// and the finished owner survives writer advancement and drop.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_sealed_read_with_vfs(
        cx: &CommitCx,
        query_cx: &QueryCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        as_of: Option<CommitSeq>,
        relation: RelationId,
        sealing: SealedLimits,
        source_limits: RootReadLimits,
    ) -> Result<SealedReadView, SealedProjectionOpenError> {
        query_cx
            .with_restriction_async(async {
                query_cx
                    .checkpoint()
                    .map_err(Error::Cancelled)
                    .map_err(SealedReadError::from)?;
                let path = path.as_ref();
                crate::require_database_dir(cx, &vfs, path).await?;
                let published = match Self::read_current_generation(
                    cx,
                    &vfs,
                    path,
                    &keys,
                    Some(&source_limits),
                )
                .await?
                {
                    crate::ReadGeneration::Current(published) => published,
                    crate::ReadGeneration::Lagging(_) | crate::ReadGeneration::Unselected => {
                        return Err(SealedProjectionOpenError::RecoveryRequired);
                    }
                };
                let as_of = as_of.unwrap_or(published.published_at);
                if as_of > published.published_at {
                    return Err(
                        SealedReadError::from(Error::Read(ReadError::BeyondFrontier {
                            asked: as_of,
                            frontier: published.published_at,
                        }))
                        .into(),
                    );
                }
                // Keep the sealer's deep Send proof behind the same explicit
                // future boundary as the existing read-only Prism opener.
                let sealing: crate::SendFuture<'_, _> =
                    Box::pin(published.store.seal_relation_with_read_source(
                        query_cx,
                        published.root_id,
                        as_of,
                        relation,
                        sealing,
                        source_limits,
                    ));
                let (partition, (root, blocks, block_props), patches) =
                    sealing.await.map_err(SealedReadError::Seal)?;
                query_cx
                    .checkpoint()
                    .map_err(Error::Cancelled)
                    .map_err(SealedReadError::from)?;
                // Reuse the complete admitted source, including unselected
                // relations needed by ordinary GQL patterns and properties.
                // This is the same snapshot construction as read_published,
                // without rereading or decoding any immutable payload.
                let mut snapshot = crate::current_generation(
                    published.manifest,
                    published.root_id,
                    &root,
                    blocks,
                    block_props,
                    patches,
                );
                snapshot.delta_index = crate::LocalDeltaBatchIndex::empty_at(snapshot.frontier);
                let view = EmbeddedReadView {
                    snapshot: std::sync::Arc::new(snapshot),
                    namespace: keys.namespace,
                    beacon_indexes: crate::query::BeaconQueryIndexes::default(),
                };
                view.check_sealed_source(&partition, as_of)?;
                partition
                    .anchor()
                    .check_relation(relation)
                    .map_err(SealedReadError::Seal)?;
                query_cx
                    .checkpoint()
                    .map_err(Error::Cancelled)
                    .map_err(SealedReadError::from)?;
                Ok(SealedReadView {
                    view,
                    partition,
                    as_of,
                    relation,
                })
            })
            .await
    }

    /// Pin one generation before I/O, authenticate the complete root, and seal
    /// only the requested relation through Strata's existing compaction path.
    /// `None` chooses the pinned publication. A future or unretained cut fails;
    /// no requested sequence is clamped and no image becomes durable authority.
    ///
    /// Root reading and image construction spend the supplied independent
    /// limits. Later queries retain their own cumulative GQL admission. A
    /// refused or dropped preparation publishes no partial view. The completed
    /// owner remains usable after writer advancement, compaction or drop.
    pub async fn prepare_sealed_read(
        &self,
        cx: &QueryCx,
        as_of: Option<CommitSeq>,
        relation: RelationId,
        sealing: SealedLimits,
        source_limits: RootReadLimits,
    ) -> Result<SealedReadView, SealedReadError> {
        cx.with_restriction_async(async {
            let view = self.read_session().map_err(Error::Read)?;
            let as_of = as_of.unwrap_or(view.frontier());
            view.snapshot.check_frontier(as_of).map_err(Error::Read)?;
            cx.checkpoint().map_err(Error::Cancelled)?;
            let partition = self
                .store
                .seal_relation_with_source_limits(
                    cx,
                    view.partition_root(),
                    as_of,
                    relation,
                    sealing,
                    source_limits,
                )
                .await
                .map_err(SealedReadError::Seal)?;
            view.check_sealed_source(&partition, as_of)?;
            partition
                .anchor()
                .check_relation(relation)
                .map_err(SealedReadError::Seal)?;
            cx.checkpoint().map_err(Error::Cancelled)?;
            Ok(SealedReadView {
                view,
                partition,
                as_of,
                relation,
            })
        })
        .await
    }
}

impl SealedReadView {
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }

    #[must_use]
    pub fn frontier(&self) -> CommitSeq {
        self.view.frontier()
    }

    #[must_use]
    pub fn partition_root(&self) -> fgdb_strata::PartitionRootVersion {
        self.view.partition_root()
    }

    #[must_use]
    pub fn relation(&self) -> RelationId {
        self.relation
    }

    /// Execute every prepared native read class using the existing engines,
    /// result schema and lossless aggregate slot mapping. Ordinary pattern and
    /// property reads use the resident view; CALLs use the compressed source.
    /// Every operand uses this owner's exact `snapshot_seq()`. Temporal reads
    /// must bind that same cut, even for empty results; they cannot switch to
    /// another cut or a decoded analytics projection.
    pub fn execute_prepared(
        &self,
        cx: &QueryCx,
        prepared: &PreparedNativeRead,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
    ) -> Result<QueryResult, SealedQueryError> {
        let requested = prepared.bound_snapshot_seq(params, self.as_of)?;
        if requested != self.as_of {
            return Err(SealedQueryError::SnapshotMismatch {
                requested,
                retained: self.as_of,
            });
        }
        match prepared {
            PreparedNativeRead::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = self
                    .execute_graph_set_governed(cx, &query, policy, memory)
                    .map_err(QueryError::Set)?;
                Ok(crate::query::values(
                    prepared.columns().to_vec(),
                    result.value,
                ))
            }
            PreparedNativeRead::TemporalSet(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalSetText)?;
                let result = self
                    .execute_graph_set_governed(cx, query.query(), policy, memory)
                    .map_err(QueryError::Set)?;
                Ok(crate::query::values(
                    prepared.columns().to_vec(),
                    result.value,
                ))
            }
            PreparedNativeRead::PipelineAggregate(prepared)
                if prepared.requires_relational_input() =>
            {
                let query = prepared
                    .bind_relation_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                let result = self
                    .execute_graph_set_aggregate_governed(cx, &query, policy, memory)
                    .map_err(QueryError::Aggregate)?;
                Ok(crate::query::aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            PreparedNativeRead::Pattern(_)
            | PreparedNativeRead::Aggregate(_)
            | PreparedNativeRead::PipelineAggregate(_)
            | PreparedNativeRead::TemporalPattern(_)
            | PreparedNativeRead::TemporalAggregate(_) => prepared
                .execute_in_view_at_seq(&self.view, cx, params, policy, self.as_of)
                .map_err(SealedQueryError::Query),
        }
    }

    /// Execute the existing set/page operators with compressed FnX sources.
    /// Canonical vertex-first calls feed completed cursors to bounded ranking
    /// and pagination. Other row shapes retain the ordinary GQL materialization
    /// barrier, consuming the same compressed result rather than decoding a graph.
    /// Every source is validated even when the final page is empty.
    pub fn execute_graph_set_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
    ) -> SetResult {
        self.execute_graph_set_evidenced(cx, query, policy, memory, &mut Vec::new())
    }

    pub(crate) fn execute_graph_set_evidenced(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
        evidence: &mut Vec<FnxCertificate>,
    ) -> SetResult {
        cx.with_restriction(|| {
            self.preflight(cx)
                .map_err(|error| error.map_source(GraphSetExecutionError::Source))?;
            let evidence = RefCell::new(evidence);
            query.execute_governed_from_source(policy, self.source(cx, memory, &evidence), || {
                cx.checkpoint()
            })
        })
    }

    /// Exact grouping over the complete relation at this same cut. Counts,
    /// wide integer sums, rational averages, HAVING and output pages retain
    /// the existing aggregate semantics and cumulative query policy.
    pub fn execute_graph_set_aggregate_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphSetAggregate,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
    ) -> AggregateResult {
        self.execute_graph_set_aggregate_evidenced(cx, query, policy, memory, &mut Vec::new())
    }

    pub(crate) fn execute_graph_set_aggregate_evidenced(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphSetAggregate,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
        evidence: &mut Vec<FnxCertificate>,
    ) -> AggregateResult {
        cx.with_restriction(|| {
            self.preflight(cx)
                .map_err(|error| error.map_source(GraphAggregateError::Source))?;
            let evidence = RefCell::new(evidence);
            query.execute_governed_from_source(policy, self.source(cx, memory, &evidence), || {
                cx.checkpoint()
            })
        })
    }

    fn preflight(&self, cx: &QueryCx) -> Result<(), Failure> {
        self.view
            .check_sealed_source(&self.partition, self.as_of)
            .map_err(sealed_failure)?;
        cx.checkpoint().map_err(GqlQueryError::Interrupted)
    }

    fn source<'a, 'e: 'a>(
        &'a self,
        cx: &'a QueryCx,
        memory: FnxMemoryLimits,
        evidence: &'a RefCell<&'e mut Vec<FnxCertificate>>,
    ) -> impl GraphSetSource<GqlError, Cancel> + 'a {
        fgdb_gql::with_element_metadata(
            fgdb_gql::procedure_row_source(
                move |pattern, remaining| {
                    crate::gql_exec::execute_seeded_pattern_at(
                        &self.view,
                        pattern,
                        None,
                        self.as_of,
                        remaining,
                        || cx.checkpoint(),
                    )
                },
                move |pattern, seeds, remaining| {
                    Some(crate::gql_exec::execute_seeded_pattern_at(
                        &self.view,
                        pattern,
                        Some(seeds),
                        self.as_of,
                        remaining,
                        || cx.checkpoint(),
                    ))
                },
                move |call, arguments, remaining| {
                    if crate::query::is_hybrid(call) {
                        return crate::query::hybrid_procedure(
                            &self.view.snapshot,
                            Some(&self.view.beacon_indexes),
                            self.as_of,
                            call,
                            arguments,
                            remaining,
                            cx,
                        );
                    }
                    let mut completed =
                        self.procedure(cx, call, arguments, remaining, memory, false, evidence)?;
                    let mut value = Vec::new();
                    value
                        .try_reserve_exact(completed.count)
                        .map_err(|_| procedure_failure(Error::AllocationFailed))?;
                    while let Some(row) = completed.cursor.next_row(&mut || cx.checkpoint())? {
                        value.push(row);
                    }
                    Ok(GqlQueryExecution {
                        value,
                        rows: completed.rows,
                        evaluator: completed.evaluator,
                    })
                },
                move |call, arguments, remaining| {
                    Some(
                        self.procedure(cx, call, arguments, remaining, memory, true, evidence)
                            .map(|completed| GraphProcedureExecution {
                                cursor: Box::new(completed.cursor),
                                rows: completed.rows,
                                evaluator: completed.evaluator,
                            }),
                    )
                },
                move |element, key| {
                    crate::gql_exec::element_property_at(&self.view, element, key, self.as_of)
                },
            ),
            move |element, kind, catalog, control| {
                crate::gql_exec::element_metadata_at(
                    &self.view, element, kind, catalog, self.as_of, control,
                )
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn procedure(
        &self,
        cx: &QueryCx,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
        canonical: bool,
        evidence: &RefCell<&mut Vec<FnxCertificate>>,
    ) -> Result<Completed, Failure> {
        let bound = bind_procedure(call, arguments)?;
        if canonical
            && (matches!(bound.algorithm(), fgdb_prism::FnxAlgorithm::Bridges)
                || bound
                    .outputs()
                    .first()
                    .is_none_or(|column| column.field != FnxOutput::Vertex))
        {
            return Err(refuse(ProcedureError::ResultDomain));
        }
        let mut options = procedure_options(&bound, self.as_of, policy);
        if options.selection.relation != Some(self.relation) {
            return Err(refuse(ProcedureError::SealedRelationMismatch {
                requested: options.selection.relation,
                retained: self.relation,
            }));
        }
        supported_sealed_call(&bound, options.projection.directedness).map_err(sealed_failure)?;
        let (graph, mut usage) = self.projection(cx, options, policy)?;
        // Sparse cursors examine unreachable ordinals too. Reserve that entire
        // future pass before kernel execution or any downstream callback.
        usage.charge_work(policy, count(graph.node_count())?)?;
        options.execution_limits.max_estimated_work =
            options.execution_limits.max_estimated_work.min(
                usize::try_from(usage.remaining(policy).evaluator.max_work_units)
                    .unwrap_or(usize::MAX),
            );
        let cursor = bound
            .execute_sealed_rows(cx, &graph, options.execution_limits, memory)
            .map_err(|error| sealed_failure(SealedReadError::Execution(error)))?;
        let count = cursor.remaining();
        let certificate = cursor.certificate().clone();
        // This is the registered conservative kernel estimate, not a claim
        // that its live checkpoint count measures all arithmetic instructions.
        usage.charge_work(policy, self::count(certificate.estimated_work)?)?;
        charge_usage(
            &mut usage,
            policy,
            0,
            self::count(count)?,
            self::count(count)?,
        )?;
        let cursor = CompletedRows(ProcedureRows::new(cursor)?);
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let measured = usage.finish::<GqlError, Cancel, ()>(
            policy,
            Ok(GqlQueryExecution {
                value: Vec::new(),
                rows: GqlExecutionStats {
                    snapshot_records: usage.snapshot_records(),
                    result_rows: self::count(count)?,
                },
                evaluator: GlaExecutionStats::default(),
            }),
        )?;
        evidence.borrow_mut().push(certificate);
        Ok(Completed {
            cursor,
            count,
            rows: measured.rows,
            evaluator: measured.evaluator,
        })
    }

    fn projection(
        &self,
        cx: &QueryCx,
        options: FnxReadOptions,
        policy: GqlQueryPolicy,
    ) -> Result<(SealedGraphView, AdmissionUsage), Failure> {
        self.preflight(cx)?;
        let request = SealedProjectionRequest {
            projection: ProjectionRequest {
                binding: SnapshotBinding {
                    root: self.view.partition_root().0,
                    as_of: self.as_of,
                },
                selection: options.selection,
                projection: options.projection,
                projection_limits: options.projection_limits,
                source_limits: options.source_limits,
            },
            source: SealedProjectionSource::Admitted(self.partition.anchor()),
            query_policy: Some(policy),
        };
        let cache = &self.view.snapshot.read_caches.sealed;
        if let Some(retained) =
            cache.lookup_query_controlled(&self.view.snapshot, request, &mut || {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)
            })?
        {
            let Some(usage) = retained.query_usage else {
                return Err(procedure_failure(Error::Execution(
                    FnxExecutionError::InvalidUpstreamResult,
                )));
            };
            // An exact request or a complete cost that fits the fresh bounds
            // proves construction cannot refuse. Prepay its exact counters
            // again; no authority, kernel or result is reused by this sidecar.
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            return Ok((retained.graph, usage.admission));
        }
        let mut usage = SourceUsage::new(policy, options.source_limits);
        let mut vertices = Vec::new();
        let mut staged_bytes = 0;
        source::visit_vertices(
            &self.view.snapshot.patches,
            self.as_of,
            &mut |event| {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.event(event)
            },
            |row, control| {
                control(SourceEvent::SnapshotRecord)?;
                if options
                    .selection
                    .vertex_label
                    .is_some_and(|label| row.labels.binary_search(&label).is_err())
                {
                    return Ok(());
                }
                control(SourceEvent::ScratchEntry)?;
                push_staged(
                    &mut vertices,
                    row.vid,
                    "vertices",
                    options.projection_limits.max_vertices,
                    &mut staged_bytes,
                    options.source_limits.max_staging_bytes,
                )
                .map_err(procedure_failure)
            },
        )?;
        // The compressed image can contain old versions and excluded endpoints.
        // Its authenticated raw population is source admission, even if no
        // selected vertex or final result row survives the query.
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.raw_records(count(self.partition.incidence_count())?)?;
        let graph = SealedGraphView::build_with_checkpoint(
            cx,
            &self.partition,
            &vertices,
            SealedProjectionSpec {
                as_of: self.as_of,
                selection: options.selection,
                projection: options.projection,
            },
            SealedProjectionMask::UNMASKED,
            options.projection_limits,
            || {
                usage
                    .event(SourceEvent::Work)
                    .map_err(|error| SealedProjectionError::Guard(Box::new(error)))
            },
        )
        .map_err(|error| sealed_failure(SealedReadError::Projection(error)))?;
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let usage = sealed_cache::QueryUsage {
            admission: usage.admission,
            source_work: usage.work,
            source_scratch: usage.scratch,
        };
        let retained = sealed_cache::Retained {
            graph: graph.clone(),
            query_usage: Some(usage),
        };
        if let Some(bytes) = retained.charged_bytes() {
            cache.remember(&self.view.snapshot, request, retained, bytes, &mut || {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)
            })?;
        }
        Ok((graph, usage.admission))
    }
}

struct Completed {
    cursor: CompletedRows,
    count: usize,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}

struct CompletedRows(ProcedureRows);

impl GraphProcedureRowCursor<GqlError, Cancel> for CompletedRows {
    fn next_row(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), Cancel>,
    ) -> Result<Option<GraphValueRow>, Failure> {
        let row = self.0.next_row(checkpoint)?;
        if row.is_none() {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
        }
        Ok(row)
    }
}

pub(crate) struct SourceUsage {
    admission: AdmissionUsage,
    policy: GqlQueryPolicy,
    limits: FnxSourceLimits,
    work: u64,
    scratch: u64,
}

impl SourceUsage {
    pub(crate) fn new(policy: GqlQueryPolicy, limits: FnxSourceLimits) -> Self {
        Self {
            admission: AdmissionUsage::default(),
            policy,
            limits,
            work: 0,
            scratch: 0,
        }
    }

    pub(crate) fn event(&mut self, event: SourceEvent) -> Result<(), Failure> {
        let (value, limit, resource) = match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => {
                (&mut self.work, self.limits.max_work_units, "work units")
            }
            SourceEvent::ScratchEntry => (
                &mut self.scratch,
                self.limits.max_scratch_entries,
                "scratch entries",
            ),
        };
        *value = value
            .checked_add(1)
            .ok_or_else(|| procedure_failure(Error::SizeOverflow))?;
        source_admit(resource, u128::from(*value), u128::from(limit)).map_err(procedure_failure)?;
        self.admission.observe(self.policy, event)
    }

    pub(crate) fn raw_records(&mut self, records: u64) -> Result<(), Failure> {
        self.work = self
            .work
            .checked_add(records)
            .ok_or_else(|| procedure_failure(Error::SizeOverflow))?;
        source_admit(
            "work units",
            u128::from(self.work),
            u128::from(self.limits.max_work_units),
        )
        .map_err(procedure_failure)?;
        charge_usage(&mut self.admission, self.policy, records, records, 0)
    }

    /// A completed construction receipt, before kernel or output work. An
    /// exact buffered query request recharges this admission on every reuse.
    pub(crate) fn completed(self) -> sealed_cache::QueryUsage {
        sealed_cache::QueryUsage {
            admission: self.admission,
            source_work: self.work,
            source_scratch: self.scratch,
        }
    }
}

pub(crate) fn count(value: usize) -> Result<u64, Failure> {
    u64::try_from(value).map_err(|_| procedure_failure(Error::SizeOverflow))
}

/// Admit a known complete population without iterating solely to increment a
/// counter. Check each dimension before mutating the successful usage record.
pub(crate) fn charge_usage(
    usage: &mut AdmissionUsage,
    policy: GqlQueryPolicy,
    records: u64,
    work: u64,
    scratch: u64,
) -> Result<(), Failure> {
    let observed = usage
        .snapshot_records()
        .checked_add(records)
        .ok_or_else(|| procedure_failure(Error::SizeOverflow))?;
    policy
        .rows
        .check(GqlBudgetDimension::SnapshotRecords, observed)
        .map_err(GqlQueryError::Rows)?;
    let remaining = usage.remaining(policy).evaluator;
    for (used, amount, limit, dimension) in [
        (
            policy.evaluator.max_work_units - remaining.max_work_units,
            work,
            policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        ),
        (
            policy.evaluator.max_scratch_entries - remaining.max_scratch_entries,
            scratch,
            policy.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        ),
    ] {
        let observed = u128::from(used) + u128::from(amount);
        if observed > u128::from(limit) {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed,
            }));
        }
    }
    usage.absorb(records, work, scratch);
    Ok(())
}

fn sealed_failure(error: SealedReadError) -> Failure {
    let refusal = match error {
        SealedReadError::Input(error) => return procedure_failure(error),
        SealedReadError::Seal(SealedError::Interrupted(error))
        | SealedReadError::Projection(SealedProjectionError::Read(SealedError::Interrupted(
            error,
        ))) => return GqlQueryError::Interrupted(error),
        SealedReadError::Projection(SealedProjectionError::Guard(error)) => {
            match error.downcast::<Failure>() {
                Ok(error) => return *error,
                Err(error) => FnxSealedReadError::Projection(SealedProjectionError::Guard(error)),
            }
        }
        SealedReadError::Execution(FnxSealedExecutionError::Cancelled(error)) => {
            match error.downcast::<asupersync::error::Error>() {
                Ok(error) => return GqlQueryError::Interrupted(error),
                Err(error) => {
                    FnxSealedReadError::Execution(FnxSealedExecutionError::Cancelled(error))
                }
            }
        }
        SealedReadError::Seal(error) => FnxSealedReadError::Seal(error),
        SealedReadError::Projection(error) => FnxSealedReadError::Projection(error),
        SealedReadError::Execution(error) => FnxSealedReadError::Execution(error),
        SealedReadError::SourceMismatch => FnxSealedReadError::SourceMismatch,
    };
    refuse(ProcedureError::SealedRead(Box::new(refusal)))
}
