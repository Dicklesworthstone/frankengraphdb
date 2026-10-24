//! Explicit in-core analytics over the same admitted generation and borrowed
//! historical visitors as native GLA reads. No cloned property rows or copied
//! algorithms, new authorization boundary, hidden reduction, or streaming claim.

use super::Cancel;
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, DatabaseKeys, EmbeddedReadView, GqlError, OpenError, ReadError, UnixVfs};
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    PreparedProcedureCall,
};
use fgdb_prism::{
    Directedness, FnxArgument, FnxBindErrorKind, FnxCallError, FnxCallSite, FnxCallSpec,
    FnxCertificate, FnxMemoryLimits, FnxOutput, FnxParameters, FnxReadError, FnxReadOptions,
    FnxReadResult, FnxSealedReadError, FnxSelection, FnxSourceLimits, FnxValue, ParallelEdgePolicy,
    ProjectionBuildError, ProjectionEdge, ProjectionError, ProjectionLimits, ProjectionSpec,
    SealedGraphView, SealedProjectionError, SealedProjectionSpec, SelfLoopPolicy, SnapshotBinding,
    SnapshotGraphView, SortedVertexLookup,
};
use fgdb_strata::store::RootReadLimits;
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits, SealedPartition};
use fgdb_types::{CanonicalF64, CanonicalScalar, CommitCx, CommitSeq, QueryCx, VId};
use std::mem::size_of;
use std::num::NonZeroUsize;
use std::path::Path;

use fgdb_prism::FnxSealedExecutionError;

#[path = "query_prism/cache.rs"]
mod cache;
#[path = "query_prism/projection.rs"]
mod projection;
#[path = "query_prism/sealed_cache.rs"]
mod sealed_cache;
#[path = "query_prism/sealed_prepare.rs"]
mod sealed_prepare;
pub(crate) use cache::{
    CACHE_BYTES as PROJECTION_CACHE_BYTES, LiveCache, ProjectionCache, Request as ProjectionRequest,
};
pub(crate) use projection::{bind_procedure, procedure_options};
pub(crate) use sealed_cache::{
    BufferedCache as BufferedSealedCache, CACHE_BYTES as SEALED_CACHE_BYTES,
    Request as SealedProjectionRequest, SealedProjectionCache, Source as SealedProjectionSource,
};
pub(crate) use sealed_prepare::{CooperativeSealedBuild, ImmediateSealedBuild, SealedBuild};

type Error = FnxReadError<ReadError, Cancel>;
type SealedReadError = FnxSealedReadError<ReadError, Cancel>;

/// Failure to prepare compressed analytics directly from a read-only
/// checkpoint. Source authentication, projection admission and opening keep
/// their original causes; no error falls back to a writable resident open.
#[derive(Debug)]
pub enum SealedProjectionOpenError {
    Open(OpenError),
    Read(SealedReadError),
    /// No checkpoint is current with the observed committed chain. Recovery
    /// or a writer's pending publication must complete before retrying.
    RecoveryRequired,
}

impl core::fmt::Display for SealedProjectionOpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::RecoveryRequired => f.write_str(
                "sealed open needs a current authenticated checkpoint; the published root is missing or behind the commit log (wait for the writer or recover through Database::open first)",
            ),
        }
    }
}

impl core::error::Error for SealedProjectionOpenError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::RecoveryRequired => None,
        }
    }
}

impl From<OpenError> for SealedProjectionOpenError {
    fn from(error: OpenError) -> Self {
        Self::Open(error)
    }
}

impl From<SealedReadError> for SealedProjectionOpenError {
    fn from(error: SealedReadError) -> Self {
        Self::Read(error)
    }
}

impl Database<UnixVfs> {
    /// Prepare a reusable compressed projection with one authenticated source
    /// traversal and no resident database or writer lease. The current root
    /// must already be published; this path never repairs or writes storage.
    /// Only the selected relation is retained for sealing, while every source
    /// object and its complete history are authenticated under `root_limits`.
    /// The source, vertex staging, sealed image and projection retain their
    /// separate limits. This is not a persistent Tier R or an RSS bound.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_prism_sealed_projection(
        cx: &CommitCx,
        query_cx: &QueryCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        options: FnxReadOptions,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
    ) -> Result<SealedGraphView, SealedProjectionOpenError> {
        Self::open_prism_sealed_projection_with_vfs(
            cx,
            query_cx,
            UnixVfs::new(),
            path,
            keys,
            options,
            sealing,
            root_limits,
        )
        .await
    }

    /// Read-only sealed preparation with cooperative projection construction.
    /// Vertex-directory copying, incoming-index building and raw projection
    /// scans yield under `quantum`; source admission, compaction and image
    /// encoding retain synchronous stages and their existing separate limits.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_prism_sealed_projection_cooperative(
        cx: &CommitCx,
        query_cx: &QueryCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        options: FnxReadOptions,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
        quantum: NonZeroUsize,
    ) -> Result<SealedGraphView, SealedProjectionOpenError> {
        Self::open_prism_sealed_projection_cooperative_with_vfs(
            cx,
            query_cx,
            UnixVfs::new(),
            path,
            keys,
            options,
            sealing,
            root_limits,
            quantum,
        )
        .await
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// The same read-only preparation through the injected production/lab
    /// filesystem. A concurrent writer can advance without changing the
    /// already selected immutable root or the returned projection.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_prism_sealed_projection_with_vfs(
        cx: &CommitCx,
        query_cx: &QueryCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        options: FnxReadOptions,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
    ) -> Result<SealedGraphView, SealedProjectionOpenError> {
        Self::open_prism_sealed_projection_build(
            cx,
            query_cx,
            vfs,
            path,
            keys,
            options,
            sealing,
            root_limits,
            ImmediateSealedBuild,
        )
        .await
    }

    /// Injected-filesystem cooperative equivalent of
    /// `open_prism_sealed_projection_cooperative`, with identical source
    /// authentication, projection bytes, limits and recovery refusals.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_prism_sealed_projection_cooperative_with_vfs(
        cx: &CommitCx,
        query_cx: &QueryCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        options: FnxReadOptions,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
        quantum: NonZeroUsize,
    ) -> Result<SealedGraphView, SealedProjectionOpenError> {
        Self::open_prism_sealed_projection_build(
            cx,
            query_cx,
            vfs,
            path,
            keys,
            options,
            sealing,
            root_limits,
            CooperativeSealedBuild(quantum),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_prism_sealed_projection_build<B: SealedBuild>(
        cx: &CommitCx,
        query_cx: &QueryCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
        options: FnxReadOptions,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
        mut build: B,
    ) -> Result<SealedGraphView, SealedProjectionOpenError> {
        query_cx
            .with_restriction_async(async {
                query_cx
                    .checkpoint()
                    .map_err(Error::Cancelled)
                    .map_err(SealedReadError::from)?;
                let relation = options
                    .selection
                    .relation
                    .ok_or(SealedReadError::Projection(
                        SealedProjectionError::RelationRequired,
                    ))?;
                let path = path.as_ref();
                crate::require_database_dir(cx, &vfs, path).await?;
                let published =
                    match Self::read_current_generation(cx, &vfs, path, &keys, Some(&root_limits))
                        .await?
                    {
                        crate::ReadGeneration::Current(published) => published,
                        crate::ReadGeneration::Lagging(_) | crate::ReadGeneration::Unselected => {
                            return Err(SealedProjectionOpenError::RecoveryRequired);
                        }
                    };
                let as_of = options.as_of.unwrap_or(published.published_at);
                if as_of > published.published_at {
                    return Err(
                        SealedReadError::from(Error::Read(ReadError::BeyondFrontier {
                            asked: as_of,
                            frontier: published.published_at,
                        }))
                        .into(),
                    );
                }
                // Type-erased like the commit chokepoints (fgdb-a5y6m): the
                // sealer's future nests deep enough that a concrete caller's
                // Send proof overflows unless the chain is split here.
                let sealing: crate::SendFuture<'_, _> =
                    Box::pin(published.store.seal_relation_with_vertex_history(
                        query_cx,
                        published.root_id,
                        as_of,
                        relation,
                        sealing,
                        root_limits,
                    ));
                let (partition, patches) = sealing.await.map_err(SealedReadError::Seal)?;
                let scope = partition.anchor().scope();
                if scope.source_root != published.root_id
                    || scope.publication != published.published_at
                    || scope.graph != crate::GRAPH
                    || scope.branch != crate::BRANCH
                    || scope.partition != crate::PARTITION
                {
                    return Err(SealedReadError::SourceMismatch.into());
                }
                let vertices = select_prism_vertices(
                    &patches,
                    as_of,
                    options.selection,
                    options.projection_limits,
                    options.source_limits,
                    &mut source_control(query_cx, options.source_limits),
                    &mut 0usize,
                )
                .map_err(SealedReadError::from)?;
                // Properties and history used only to select the directory are
                // no longer retained while incoming/projection indexes are built.
                drop(patches);
                build
                    .build(
                        query_cx,
                        &partition,
                        &vertices,
                        SealedProjectionSpec {
                            as_of,
                            selection: options.selection,
                            projection: options.projection,
                        },
                        options.projection_limits,
                    )
                    .await
                    .map_err(SealedReadError::Projection)
                    .map_err(SealedProjectionOpenError::Read)
            })
            .await
    }
}

/// Why a native read's `CALL fnx.*` stage was refused.
#[derive(Debug)]
pub enum ProcedureError {
    /// No registered signature accepts this name, argument or YIELD item.
    /// Arguments keep their exact domain: text, decimal, list, path and edge
    /// values have no Prism argument type and are never coerced.
    Bind(FnxCallError),
    /// Projection, source admission or kernel execution refused. A
    /// cancellation is never a refusal: it surfaces as the read's own
    /// interruption, so this error cannot carry one.
    Read(Box<FnxReadError<ReadError, core::convert::Infallible>>),
    /// A count beyond the GQL Int64 domain.
    ResultDomain,
    /// A `CALL hybrid.search` refused its arguments or its retrieval.
    Search(crate::query::HybridCallError),
}
impl core::fmt::Display for ProcedureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::ResultDomain => f.write_str("Prism count exceeds the Int64 domain"),
            Self::Search(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for ProcedureError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Search(error) => Some(error),
            Self::Read(_) | Self::ResultDomain => None,
        }
    }
}
impl ProcedureError {
    /// Whether the statement alone fixes this refusal: no registered
    /// signature binds it, or its projection has a graph kind the procedure
    /// refuses, which the binder checks before any source is scanned.
    #[must_use]
    pub fn is_statement_refusal(&self) -> bool {
        match self {
            Self::Bind(_) => true,
            Self::Read(error) => matches!(
                **error,
                FnxReadError::Execution(fgdb_prism::FnxExecutionError::GraphKind { .. })
            ),
            Self::ResultDomain | Self::Search(_) => false,
        }
    }
}

/// Prism as the procedure host of a native set read: bind the call through
/// the same typed resolution as `call_fnx`, then run its explicit projection
/// or canonical whole-graph default at the read's own snapshot, so a later
/// MATCH and the analytics see one generation. Admission is capped by what
/// the read has left; the rows' cost is charged to that same policy. A completed
/// call's Prism certificate is appended to `evidence`, in execution order,
/// for a certified read to fold into its own certificate (fgdb-3b1v7).
pub(crate) fn fnx_procedure(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    as_of: CommitSeq,
    remaining: GqlQueryPolicy,
    evidence: &mut Vec<FnxCertificate>,
    execute: impl FnOnce(&FnxCallSpec, FnxReadOptions) -> Result<FnxReadResult, Error>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, Cancel>> {
    let spec = bind_procedure(call, arguments)?;
    let analytics = execute(&spec, procedure_options(&spec, as_of, remaining))
        .map_err(procedure_failure)?
        .analytics;
    let rows = procedure_rows(analytics.rows)?;
    let certificate = analytics.certificate;
    let records = certificate.vertices.saturating_add(certificate.input_edges);
    let execution = GqlQueryExecution {
        rows: GqlExecutionStats {
            snapshot_records: u64::try_from(records).unwrap_or(u64::MAX),
            result_rows: rows.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: u64::try_from(certificate.estimated_work).unwrap_or(u64::MAX),
            scratch_entries: rows.len() as u64,
        },
        value: rows,
    };
    evidence.push(certificate);
    Ok(execution)
}

fn refuse<C>(error: ProcedureError) -> GqlQueryError<GqlError, C> {
    GqlQueryError::Source(GqlError::Procedure(error))
}

/// Bind the kernel portion after projection arguments have been separated.
/// Every host retains the same registered argument and output-domain checks.
fn bind_kernel<C>(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
) -> Result<FnxCallSpec, GqlQueryError<GqlError, C>> {
    let mut supplied = Vec::with_capacity(arguments.len());
    for (index, value) in arguments.iter().enumerate() {
        supplied.push(kernel_argument(value, index)?);
    }
    let yields: Vec<(&str, &str)> = call
        .outputs()
        .iter()
        .map(|name| (name.as_str(), name.as_str()))
        .collect();
    let spec = FnxCallSpec::from_arguments(call.namespace(), call.name(), &supplied, &yields)
        .map_err(|error| refuse(ProcedureError::Bind(error)))?;
    check_kernel_outputs(call, spec)
}

/// Named kernel arguments use the same typed registry as standalone calls.
/// The caller maps their local argument ordinals back around projection fields.
fn bind_named_kernel<C>(
    call: &PreparedProcedureCall,
    arguments: &[(&str, &GraphValue)],
) -> Result<FnxCallSpec, GqlQueryError<GqlError, C>> {
    let mut supplied = Vec::with_capacity(arguments.len());
    for (index, &(name, value)) in arguments.iter().enumerate() {
        supplied.push((name, kernel_argument(value, index)?));
    }
    let yields: Vec<(&str, &str)> = call
        .outputs()
        .iter()
        .map(|name| (name.as_str(), name.as_str()))
        .collect();
    let spec = FnxCallSpec::from_named_arguments(call.namespace(), call.name(), &supplied, &yields)
        .map_err(|error| refuse(ProcedureError::Bind(error)))?;
    check_kernel_outputs(call, spec)
}

fn kernel_argument<C>(
    value: &GraphValue,
    index: usize,
) -> Result<FnxArgument, GqlQueryError<GqlError, C>> {
    Ok(match value {
        GraphValue::Scalar(CanonicalScalar::Null) => FnxArgument::Null,
        GraphValue::Scalar(CanonicalScalar::Bool(value)) => FnxArgument::Boolean(*value),
        GraphValue::Scalar(CanonicalScalar::Int(value)) => FnxArgument::Integer(*value),
        GraphValue::Scalar(CanonicalScalar::Float(value)) => FnxArgument::Float(value.get()),
        GraphValue::Vertex(vertex) => FnxArgument::Vertex(*vertex),
        _ => {
            return Err(refuse(ProcedureError::Bind(FnxCallError {
                site: FnxCallSite::Argument(index),
                kind: FnxBindErrorKind::InvalidArgument("no Prism argument domain"),
            })));
        }
    })
}

fn check_kernel_outputs<C>(
    call: &PreparedProcedureCall,
    spec: FnxCallSpec,
) -> Result<FnxCallSpec, GqlQueryError<GqlError, C>> {
    // The statement matches these outputs as vertices (fgdb-luq0b). Vertex
    // and component cells are vertex identities; anything else refuses here
    // rather than matching nothing and dropping every row.
    for &index in call.vertex_outputs() {
        let vertex = spec
            .outputs()
            .get(index)
            .is_some_and(|column| matches!(column.field, FnxOutput::Vertex | FnxOutput::Component));
        if !vertex {
            return Err(refuse(ProcedureError::Bind(FnxCallError {
                site: FnxCallSite::Yield(index),
                kind: FnxBindErrorKind::Expected(
                    "a vertex output where the statement reads a vertex",
                ),
            })));
        }
    }
    Ok(spec)
}

/// The canonical whole-graph projection at the read's sequence, admitted
/// within what the read has left: the statement's snapshot-record budget
/// bounds the source's work units and its scratch budget the source's scratch
/// entries, in place of a standalone call's fixed defaults. Under those
/// defaults a pokec PageRank refused at 10M scratch entries whatever budget
/// its statement carried (fgdb-avyqt). The staged vertices and weighted edges
/// become the projection builder's input, whose capacities it charges again
/// against its workspace, so they are staged up to that workspace: the 1 GiB
/// staging default refused pokec's 1.6 GB under a 4 GiB workspace. Explicit
/// projections retain these ceilings.
fn kernel_options(
    spec: &FnxCallSpec,
    as_of: CommitSeq,
    remaining: GqlQueryPolicy,
) -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph_for(spec, Some(as_of));
    if let Some(records) = remaining.rows.max_snapshot_records() {
        options.source_limits.max_work_units = records;
    }
    options.source_limits.max_scratch_entries = remaining.evaluator.max_scratch_entries;
    options.source_limits.max_staging_bytes = options.projection_limits.max_workspace_bytes;
    options.execution_limits.max_estimated_work = options
        .execution_limits
        .max_estimated_work
        .min(usize::try_from(remaining.evaluator.max_work_units).unwrap_or(usize::MAX));
    options
}

/// A failed Prism read: the caller's own cancellation, or a typed refusal.
pub(crate) fn procedure_failure<C>(
    error: FnxReadError<ReadError, C>,
) -> GqlQueryError<GqlError, C> {
    match error.split_cancel::<core::convert::Infallible>() {
        Err(cancel) => GqlQueryError::Interrupted(cancel),
        Ok(refusal) => refuse(ProcedureError::Read(Box::new(refusal))),
    }
}

/// Prism rows as GQL rows: vertex identities, exact counts within the Int64
/// domain, and floats.
pub(crate) fn procedure_rows<C>(
    rows: Vec<Vec<FnxValue>>,
) -> Result<Vec<GraphValueRow>, GqlQueryError<GqlError, C>> {
    let mut converted = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(row.len());
        for value in row {
            values.push(match value {
                FnxValue::Vertex(vertex) => GraphValue::Vertex(vertex),
                FnxValue::Integer(count) => GraphValue::Scalar(CanonicalScalar::Int(
                    i64::try_from(count).map_err(|_| refuse(ProcedureError::ResultDomain))?,
                )),
                FnxValue::Score(value) | FnxValue::Float(value) => {
                    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
                }
            });
        }
        converted.push(GraphValueRow::from_owned_values(values));
    }
    Ok(converted)
}

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
        self.execute_fnx_sealed(cx, &call, options, memory, sealing)
            .await
    }

    /// Pin once before storage I/O. The temporary image retains history from
    /// the requested cut through this view's publication; it is not installed
    /// as new durable graph authority. Sealing uses Strata's existing verified
    /// reopen/compaction path, which itself materializes its admitted source.
    /// Sealing, vertex staging, projection and kernel have separate budgets.
    /// Completed exact preparations may reuse this database owner's compressed
    /// cache. Every construction limit is part of the key; current cancellation
    /// and kernel/result admission still run on every call.
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
        // Kept apart from the cooperative path: this future must not contain
        // the cooperative kernel's (non-Send) future, so a Send-requiring
        // host such as a lab task can await it.
        cx.with_restriction_async(async {
            let graph = self
                .prepare_fnx_sealed(cx, call, &options, sealing, false, ImmediateSealedBuild)
                .await?;
            finish_sealed_read(cx, call, &graph, options, memory)
        })
        .await
    }

    /// Opt into cooperative compressed BFS/PageRank execution. Binding still
    /// precedes source access; all source, graph-law and memory limits remain
    /// explicit. The quantum is a nonzero count of raw/scalar steps, not time.
    /// No synchronous fallback exists for an unsupported cooperative procedure.
    #[allow(clippy::too_many_arguments)]
    pub async fn call_fnx_sealed_cooperative(
        &self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        quantum: NonZeroUsize,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        self.execute_fnx_sealed_cooperative(cx, &call, options, memory, sealing, quantum)
            .await
    }

    /// Use the pinned runtime's yield primitive between bounded kernel/scalar
    /// steps, including invisible-incidence scans and result-row conversion.
    /// This reuses the SAME pinned-source preparation as execute_fnx_sealed.
    /// Projection and incoming-index construction also yield. Source admission,
    /// sealing and compaction still have synchronous CPU stages; this is not an end-to-end poll-time
    /// bound, external-memory operator, or capability-authorization entrypoint.
    /// Existing entrypoints keep their synchronous mode and kernel identifiers.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_fnx_sealed_cooperative(
        &self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        quantum: NonZeroUsize,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction_async(async {
            let graph = self
                .prepare_fnx_sealed(
                    cx,
                    call,
                    &options,
                    sealing,
                    true,
                    CooperativeSealedBuild(quantum),
                )
                .await?;
            let result = call
                .execute_sealed_cooperative(
                    cx,
                    &graph,
                    options.execution_limits,
                    memory,
                    quantum,
                    asupersync::runtime::yield_now::yield_now,
                )
                .await
                .map_err(SealedReadError::Execution)?;
            Ok(FnxReadResult::bind_selection(result, options.selection))
        })
        .await
    }

    /// The pinned-source preparation both sealed paths share: binding checks,
    /// then the sealed projection. A cooperative call also refuses an
    /// algorithm without a cooperative kernel before any source access.
    async fn prepare_fnx_sealed<B: SealedBuild>(
        &self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: &FnxReadOptions,
        sealing: SealedLimits,
        cooperative: bool,
        build: B,
    ) -> Result<fgdb_prism::SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        cx.checkpoint().map_err(Error::Cancelled)?;
        if cooperative && !call.supports_cooperative_sealed_execution() {
            return Err(SealedReadError::Execution(
                FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(call.algorithm()),
            ));
        }
        supported_sealed_call(call, options.projection.directedness)?;
        let mut root_limits = RootReadLimits::default();
        root_limits.max_incidences = root_limits.max_incidences.max(sealing.max_incidences);
        self.prism_sealed_projection_build(
            cx,
            options.as_of,
            options.selection,
            options.projection,
            options.projection_limits,
            options.source_limits,
            sealing,
            root_limits,
            build,
        )
        .await
    }

    /// Prepare once and reuse the returned image/directory for multiple native
    /// calls. None selects this newly pinned view's frontier. The owned result
    /// does not borrow the writer, so later writes and writer drop cannot
    /// change its vertices, edge weights or source identity. Preparing itself
    /// does not grant a durable lease or make the source larger-than-memory.
    /// Only the selected relation enters compaction, the sealed image and its
    /// incoming index. On a miss every source object is read and authenticated;
    /// a completed exact request reuses that admission and immutable projection
    /// under this owner's separately configured sealed-projection allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_at(
        &self,
        cx: &QueryCx,
        as_of: Option<CommitSeq>,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        let mut root_limits = RootReadLimits::default();
        root_limits.max_incidences = root_limits.max_incidences.max(sealing.max_incidences);
        self.prism_sealed_projection_with_source_limits(
            cx,
            as_of,
            selection,
            spec,
            limits,
            source_limits,
            sealing,
            root_limits,
        )
        .await
    }

    /// Prepare a selected-relation projection with independent ceilings for
    /// the complete raw root and its retained image. Unrelated relations and
    /// invisible source history consume `root_limits`; only the selected
    /// relation's compacted output consumes `sealing`. Full-root validation
    /// remains mandatory before a scoped image can be returned.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_with_source_limits(
        &self,
        cx: &QueryCx,
        as_of: Option<CommitSeq>,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        self.prism_sealed_projection_build(
            cx,
            as_of,
            selection,
            spec,
            limits,
            source_limits,
            sealing,
            root_limits,
            ImmediateSealedBuild,
        )
        .await
    }

    /// Prepare a reusable compressed graph with cooperative directory,
    /// incoming-index and raw-incidence construction. The source image still
    /// uses the ordinary synchronous compactor/encoder. Completed graphs share
    /// the exact same cache key as immediate preparation; scheduling grants no
    /// extra source, projection, memory or publication allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn prism_sealed_projection_cooperative_at(
        &self,
        cx: &QueryCx,
        as_of: Option<CommitSeq>,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
        quantum: NonZeroUsize,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        self.prism_sealed_projection_build(
            cx,
            as_of,
            selection,
            spec,
            limits,
            source_limits,
            sealing,
            root_limits,
            CooperativeSealedBuild(quantum),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prism_sealed_projection_build<B: SealedBuild>(
        &self,
        cx: &QueryCx,
        as_of: Option<CommitSeq>,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        sealing: SealedLimits,
        root_limits: RootReadLimits,
        mut build: B,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(Error::Cancelled)?;
            let view = self.read_session().map_err(Error::Read)?;
            let as_of = as_of.unwrap_or(view.frontier());
            view.check_sealed_request(cx, as_of, selection)?;
            let relation = selection.relation.ok_or(SealedReadError::Projection(
                SealedProjectionError::RelationRequired,
            ))?;
            let request = SealedProjectionRequest {
                projection: ProjectionRequest {
                    binding: SnapshotBinding {
                        root: view.partition_root().0,
                        as_of,
                    },
                    selection,
                    projection: spec,
                    projection_limits: limits,
                    source_limits,
                },
                source: SealedProjectionSource::Root {
                    sealing,
                    limits: root_limits,
                },
            };
            let cache = &view.snapshot.read_caches.sealed;
            if let Some(graph) = cache.lookup_controlled(&view.snapshot, request, &mut || {
                cx.checkpoint().map_err(Error::Cancelled)
            })? {
                cx.checkpoint().map_err(Error::Cancelled)?;
                return Ok(graph);
            }
            // Refuse vertex admission before the potentially expensive seal.
            let mut control = source_control(cx, source_limits);
            let mut staging_bytes = 0usize;
            let vertices = view.select_prism_vertices(
                as_of,
                selection,
                limits,
                source_limits,
                &mut control,
                &mut staging_bytes,
            )?;
            let partition = self
                .store
                .seal_relation_with_source_limits(
                    cx,
                    view.partition_root(),
                    as_of,
                    relation,
                    sealing,
                    root_limits,
                )
                .await
                .map_err(SealedReadError::Seal)?;
            view.check_sealed_source(&partition, as_of)?;
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
                .map_err(SealedReadError::Projection)?;
            cx.checkpoint().map_err(Error::Cancelled)?;
            cache.remember(
                &view.snapshot,
                request,
                graph.clone(),
                graph.charged_resident_bytes(),
                &mut || cx.checkpoint().map_err(Error::Cancelled),
            )?;
            Ok(graph)
        })
        .await
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
        &self,
        cx: &QueryCx,
        partition: &SealedPartition,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        self.execute_fnx_sealed(cx, partition, &call, options, memory)
    }

    pub fn execute_fnx_sealed(
        &self,
        cx: &QueryCx,
        partition: &SealedPartition,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
    ) -> Result<FnxReadResult, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Cancelled)?;
            supported_sealed_call(call, options.projection.directedness)?;
            let graph = self.prism_sealed_projection_at(
                cx,
                partition,
                options.as_of.unwrap_or(self.frontier()),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )?;
            finish_sealed_read(cx, call, &graph, options, memory)
        })
    }

    /// Build a reusable compressed projection from this view's visible,
    /// label-selected vertex directory. The retained image supplies edges;
    /// both endpoints must belong to that directory before weight observation.
    /// Cloning the result retains its image independently of this view/writer.
    #[allow(clippy::too_many_arguments)]
    pub fn prism_sealed_projection_at(
        &self,
        cx: &QueryCx,
        partition: &SealedPartition,
        as_of: CommitSeq,
        selection: FnxSelection,
        spec: ProjectionSpec,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
    ) -> Result<SealedGraphView, FnxSealedReadError<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Cancelled)?;
            self.snapshot.check_frontier(as_of).map_err(Error::Read)?;
            self.check_sealed_source(partition, as_of)?;
            self.check_sealed_request(cx, as_of, selection)?;
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
                source: SealedProjectionSource::Admitted(partition.anchor()),
            };
            let cache = &self.snapshot.read_caches.sealed;
            if let Some(graph) = cache.lookup_controlled(&self.snapshot, request, &mut || {
                cx.checkpoint().map_err(Error::Cancelled)
            })? {
                cx.checkpoint().map_err(Error::Cancelled)?;
                return Ok(graph);
            }
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
            let graph = SealedGraphView::build(
                cx,
                partition,
                &vertices,
                SealedProjectionSpec {
                    as_of,
                    selection,
                    projection: spec,
                },
                limits,
            )
            .map_err(SealedReadError::Projection)?;
            cx.checkpoint().map_err(Error::Cancelled)?;
            cache.remember(
                &self.snapshot,
                request,
                graph.clone(),
                graph.charged_resident_bytes(),
                &mut || cx.checkpoint().map_err(Error::Cancelled),
            )?;
            Ok(graph)
        })
    }

    fn check_sealed_request(
        &self,
        cx: &QueryCx,
        as_of: CommitSeq,
        selection: FnxSelection,
    ) -> Result<(), SealedReadError> {
        cx.checkpoint().map_err(Error::Cancelled)?;
        self.snapshot.check_frontier(as_of).map_err(Error::Read)?;
        if selection.relation.is_none() {
            return Err(SealedReadError::Projection(
                SealedProjectionError::RelationRequired,
            ));
        }
        Ok(())
    }

    fn check_sealed_source(
        &self,
        partition: &SealedPartition,
        as_of: CommitSeq,
    ) -> Result<(), SealedReadError> {
        let scope = partition.anchor().scope();
        if scope.source_root != self.partition_root()
            || scope.publication != self.frontier()
            || scope.graph != crate::GRAPH
            || scope.branch != crate::BRANCH
            || scope.partition != crate::PARTITION
        {
            return Err(SealedReadError::SourceMismatch);
        }
        if as_of < scope.floor || as_of > scope.publication {
            return Err(SealedReadError::Projection(SealedProjectionError::Read(
                SealedError::SnapshotOutsideAnchor {
                    requested: as_of,
                    floor: scope.floor,
                    publication: scope.publication,
                },
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn select_prism_vertices(
        &self,
        as_of: CommitSeq,
        selection: FnxSelection,
        limits: ProjectionLimits,
        source_limits: FnxSourceLimits,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Error>,
        staging_bytes: &mut usize,
    ) -> Result<Vec<VId>, Error> {
        select_prism_vertices(
            &self.snapshot.patches,
            as_of,
            selection,
            limits,
            source_limits,
            control,
            staging_bytes,
        )
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
    /// An explicit relation prunes edge-history winner scratch before merging;
    /// every raw block and history row still consumes source work. The complete
    /// snapshot was authenticated before any projection selection is applied.
    /// The source's work/scratch counters are separate from the cache budget;
    /// neither the source nor fnx is presented as a spill-capable operator.
    ///
    /// Identical requests on the same live snapshot allocation may reuse one
    /// complete projection, including its original admission limits. A bounded
    /// per-database sidecar retains at most eight projections within the host's
    /// [`crate::ReadCacheLimits`] resident-byte allowance. Full caches rebuild
    /// normally; contention checks live cancellation until access is available.
    /// Admission does not evict live entries, and weak owners do not pin a
    /// database generation.
    /// This does not provide cross-process CLI reuse or authorize a secure view.
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
            cache::projection(self, cx, as_of, selection, spec, limits, source_limits)
        })
    }

    // The incumbent source admission and builder remain the miss path and the
    // native benchmark control. Never construct a second projection algorithm.
    fn build_prism_projection_at(
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
            let selected_vertices = SortedVertexLookup::from_sorted(&vertices);
            let mut edges = Vec::new();
            source::visit_edges_in_relation_with_properties(
                &self.snapshot,
                as_of,
                selection.relation,
                &mut control,
                |entry, props, control| {
                    control(SourceEvent::Work)?;
                    if !selected_vertices.contains(entry.src)
                        || !selected_vertices.contains(entry.dst)
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

/// One visibility, label selection, ordering and admission law for resident
/// views and the directly opened compressed projection.
fn select_prism_vertices(
    patches: &[fgdb_strata::vertex::VertexPatchRows],
    as_of: CommitSeq,
    selection: FnxSelection,
    limits: ProjectionLimits,
    source_limits: FnxSourceLimits,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Error>,
    staging_bytes: &mut usize,
) -> Result<Vec<VId>, Error> {
    let mut vertices = Vec::new();
    source::visit_vertices(patches, as_of, control, |row, control| {
        control(SourceEvent::Work)?;
        if selection
            .vertex_label
            .is_some_and(|label| row.labels.binary_search(&label).is_err())
        {
            return Ok(());
        }
        debug_assert!(vertices.last().is_none_or(|last| *last < row.vid));
        push_staged(
            &mut vertices,
            row.vid,
            "vertices",
            limits.max_vertices,
            staging_bytes,
            source_limits.max_staging_bytes,
        )
    })?;
    Ok(vertices)
}

fn supported_sealed_call(
    call: &FnxCallSpec,
    direction: Directedness,
) -> Result<(), SealedReadError> {
    call.validate_sealed_projection(direction)
        .map_err(SealedReadError::Execution)
}

fn finish_sealed_read(
    cx: &QueryCx,
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    options: FnxReadOptions,
    memory: FnxMemoryLimits,
) -> Result<FnxReadResult, SealedReadError> {
    let result = call
        .execute_sealed(cx, graph, options.execution_limits, memory)
        .map_err(SealedReadError::Execution)?;
    Ok(FnxReadResult::bind_selection(result, options.selection))
}

pub(crate) fn source_control<S>(
    cx: &QueryCx,
    limits: FnxSourceLimits,
) -> impl FnMut(SourceEvent) -> Result<(), FnxReadError<S, Cancel>> + '_ {
    let mut work = 0u64;
    let mut scratch = 0u64;
    move |event| {
        cx.checkpoint().map_err(FnxReadError::Cancelled)?;
        let (counter, limit, resource) = match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => {
                (&mut work, limits.max_work_units, "work units")
            }
            SourceEvent::ScratchEntry => {
                (&mut scratch, limits.max_scratch_entries, "scratch entries")
            }
        };
        *counter = counter.checked_add(1).ok_or(FnxReadError::SizeOverflow)?;
        source_admit(resource, u128::from(*counter), u128::from(limit))
    }
}

pub(super) fn source_admit<S, C>(
    resource: &'static str,
    requested: u128,
    limit: u128,
) -> Result<(), FnxReadError<S, C>> {
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
pub(crate) fn push_staged<T, S, C>(
    output: &mut Vec<T>,
    value: T,
    resource: &'static str,
    maximum: usize,
    staged_bytes: &mut usize,
    byte_limit: usize,
) -> Result<(), FnxReadError<S, C>> {
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
