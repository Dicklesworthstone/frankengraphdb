//! Explicit owner-level composition of one compressed analytics source with
//! the shared external relational executor. The kernel remains resident and
//! bounded; ordinary graph reads and intermediate relations remain buffered.

use super::*;
use crate::GqlError;
use crate::query::ProcedureRows;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::SpillSetRowError;
use fgdb_gql::{GlaExecutionLimits, GlaLimitDimension, GlaLimitExceeded, GraphProcedureRowCursor};
use fgdb_prism::{FnxMemoryLimits, FnxValue};
use fgdb_strata::tiered::memory::MemoryPool;
use fgdb_strata::tiered::sealed::SealedLimits;

#[derive(Clone, Copy)]
struct SealedOptions {
    relation: RelationId,
    memory: FnxMemoryLimits,
    sealing: SealedLimits,
    max_source_incidences: usize,
}

/// One non-correlated FnX CALL followed by supported external query stages.
/// Preparation binds the complete physical shape without graph or scratch IO.
/// Procedure arguments execute only when their source is reached, preserving
/// earlier child failures. Repeated or correlated procedure calls refuse.
///
/// The compressed image, projection and completed kernel vectors retain their
/// independent resident limits. This is not an external-memory graph kernel.
/// Input rows use the private input cap; only the final selected relation uses
/// the query's result cap. Cache reuse never refunds native source/evaluator
/// work, and each invocation executes its kernel and result processing afresh.
#[derive(Clone)]
pub struct PreparedBufferedSealedOrder {
    plan: AsyncSpillSetPlan,
    as_of: Option<CommitSeq>,
    options: SealedOptions,
}

impl core::fmt::Debug for PreparedBufferedSealedOrder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PreparedBufferedSealedOrder([REDACTED])")
    }
}

impl PreparedNativeRead {
    /// Explicit privileged admission for a single sealed procedure source.
    /// Ordinary buffered and authorized compilers do not acquire this ability.
    /// The selected relation must also be present in the CALL's own projection;
    /// it is an admission boundary, never an implicit projection override.
    pub fn prepare_buffered_sealed_order(
        &self,
        params: &GqlParameters,
        relation: RelationId,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
    ) -> Result<PreparedBufferedSealedOrder> {
        let prepare = |error| NativeSpoolError::Prepare(Box::new(error));
        let unsupported = || {
            prepare(QueryError::StreamingUnsupported {
                facade: self.facade_class(),
            })
        };
        let (query, as_of) = match self {
            Self::Set(template) => (
                template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::SetText(error)))?,
                None,
            ),
            Self::TemporalSet(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::TemporalSetText(error)))?;
                (bound.query().clone(), Some(bound.as_of()))
            }
            _ => return Err(unsupported()),
        };
        let plan =
            AsyncSpillSetPlan::compile_with_single_procedure(&query).map_err(|_| unsupported())?;
        PreparedBufferedSealedOrder::from_relation(
            plan,
            as_of,
            relation,
            memory,
            sealing,
            max_source_incidences,
        )
        .ok_or_else(unsupported)
    }
}

// Inspect only the registered shape. Argument expressions, named projection
// values and parameter-combination laws retain their reached-source binding.
// No placeholder arguments or private copy of the procedure registry is used.
fn admit_procedure_shape(
    source: &fgdb_gql::spill_set::SpillSetProcedurePlan,
    relation: RelationId,
) -> bool {
    use fgdb_gql::ProcedureGraphDirection;
    use fgdb_prism::{FnxGraphKind, FnxOutput, FnxSignatureRegistry};

    let call = source.call();
    if call.namespace() != "fnx" {
        return false;
    }
    let Some(signature) = FnxSignatureRegistry::lookup(&format!("fnx.{}", call.name())) else {
        return false;
    };
    if call
        .outputs()
        .first()
        .is_none_or(|name| !FnxOutput::Vertex.is_named(name))
    {
        return false;
    }
    for (index, name) in call.outputs().iter().enumerate() {
        let Some(field) = signature.outputs.iter().find(|field| field.is_named(name)) else {
            return false;
        };
        if call.outputs()[..index]
            .iter()
            .any(|earlier| field.is_named(earlier))
            || (call.vertex_outputs().contains(&index) && !field.is_vertex_reference())
        {
            return false;
        }
    }
    let names = call.argument_names();
    if let Some(projection) = call.graph_projection() {
        if !names.is_empty() || projection.relation != Some(relation) {
            return false;
        }
        if let Some(direction) = projection.direction {
            let compatible = match signature.graph_kind {
                FnxGraphKind::Any => true,
                FnxGraphKind::Directed => direction != ProcedureGraphDirection::Undirected,
                FnxGraphKind::Undirected => direction == ProcedureGraphDirection::Undirected,
            };
            if !compatible {
                return false;
            }
        }
    } else if !names.iter().any(|name| name == "relation") {
        return false;
    }
    if names.is_empty() {
        let positional = signature
            .parameters
            .iter()
            .filter(|parameter| parameter.positional);
        return source.argument_count() <= positional.clone().count()
            && positional
                .skip(source.argument_count())
                .all(|parameter| parameter.default.is_some());
    }
    const PROJECTION_FIELDS: [&str; 8] = [
        "args",
        "label",
        "relation",
        "weight",
        "direction",
        "parallel",
        "self_loops",
        "missing",
    ];
    let mut kernel = false;
    for name in names {
        if PROJECTION_FIELDS.contains(&name.as_str()) {
            continue;
        }
        if !signature
            .parameters
            .iter()
            .any(|parameter| parameter.name == name)
        {
            return false;
        }
        kernel = true;
    }
    if names.iter().any(|name| name == "missing") && !names.iter().any(|name| name == "weight") {
        return false;
    }
    if names.iter().any(|name| name == "args") {
        // The list's actual arity/domain stays lazy, including expression errors.
        return !kernel;
    }
    signature.parameters.iter().all(|parameter| {
        !parameter.positional
            || parameter.default.is_some()
            || names.iter().any(|name| name == parameter.name)
    })
}

impl PreparedBufferedSealedOrder {
    /// The terminal aggregate owner uses this same registry admission and host
    /// for its complete input relation. No second procedure executor is needed.
    pub(crate) fn from_relation(
        plan: AsyncSpillSetPlan,
        as_of: Option<CommitSeq>,
        relation: RelationId,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
    ) -> Option<Self> {
        if !admit_procedure_shape(plan.procedure_source()?, relation) {
            return None;
        }
        Some(Self {
            plan,
            as_of,
            options: SealedOptions {
                relation,
                memory,
                sealing,
                max_source_incidences,
            },
        })
    }

    pub fn stage_count(&self) -> usize {
        self.plan.stage_count()
    }

    pub fn aggregate_count(&self) -> usize {
        self.plan.aggregate_count()
    }

    /// Select an owner-supplied historical cut without overriding a selector
    /// already bound by the statement. Publication admission still occurs on
    /// the pinned buffered view when the execution future is polled.
    pub fn with_as_of(
        mut self,
        requested: CommitSeq,
    ) -> core::result::Result<Self, crate::SealedQueryError> {
        if let Some(retained) = self.as_of
            && retained != requested
        {
            return Err(crate::SealedQueryError::SnapshotMismatch {
                requested,
                retained,
            });
        }
        self.as_of = Some(requested);
        Ok(self)
    }

    /// Complete all sources and stages before publishing an authenticated
    /// result handle. Scratch ownership, cumulative quotas, exact cuts and
    /// row/encoding reservations follow [`PreparedBufferedOrder::spool_in_view`].
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        auxiliary: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        self.spool_in_view_with_resolver(
            view,
            cx,
            policy,
            scratch,
            destination,
            auxiliary,
            group_capacity,
            max_partitions,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            max_sort_work,
            None,
        )
    }

    /// Carry the native scalar resolver through every external barrier.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view_with_resolver<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        auxiliary: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let plan = self.plan.clone();
        let snapshot = self.as_of.unwrap_or_else(|| view.frontier());
        let options = self.options;
        Box::pin(async move {
            let mut host = SealedHost {
                ordinary: BufferedHost { view },
                options,
            };
            execute_relation(
                &plan,
                None,
                snapshot,
                &mut host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                ExternalOrderLimits {
                    group_capacity,
                    max_partitions,
                    run_rows,
                    max_runs,
                    page_bytes,
                    max_row_bytes,
                    max_input_rows,
                    max_sort_work,
                },
                resolver,
            )
            .await
        })
    }
}

struct SealedHost<'v, V: Vfs> {
    ordinary: BufferedHost<'v, V>,
    options: SealedOptions,
}

fn procedure_error(error: GqlQueryError<GqlError, Cancel>) -> NativeSpoolError {
    NativeSpoolError::Procedure(Box::new(error))
}

fn argument_error(error: SpillSetRowError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        SpillSetRowError::Control(error) => error,
        SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
    }
}

// Arguments spend evaluator allowance before the physical source begins.
// A source refusal must name the complete invocation's allowance/counter.
fn rebase_arguments(
    error: GqlQueryError<GqlError, Cancel>,
    policy: GqlQueryPolicy,
    arguments: GlaExecutionStats,
) -> NativeSpoolError {
    let error = match error {
        GqlQueryError::Evaluator(error) => {
            let (used, limit) = match error.dimension {
                GlaLimitDimension::WorkUnits => {
                    (arguments.work_units, policy.evaluator.max_work_units)
                }
                GlaLimitDimension::ScratchEntries => (
                    arguments.scratch_entries,
                    policy.evaluator.max_scratch_entries,
                ),
            };
            GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: error.dimension,
                limit,
                observed: error.observed.saturating_add(u128::from(used)),
            })
        }
        other => other,
    };
    procedure_error(error)
}

fn reserve_values<T>(
    pool: &MemoryPool,
    cx: &QueryCx,
    width: usize,
    header: usize,
) -> Result<MemoryCharge> {
    let bytes = width
        .checked_mul(size_of::<T>())
        .and_then(|bytes| bytes.checked_mul(8))
        .and_then(|bytes| bytes.checked_add(header))
        .ok_or(SpillError::SizeOverflow)?;
    pool.reserve(cx, bytes)
        .map_err(SpillError::Memory)
        .map_err(Into::into)
}

impl<V: Vfs + Clone> ExternalOrderHost for SealedHost<'_, V> {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()> {
        self.ordinary.check_snapshot(snapshot)
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.ordinary.checkpoint()
    }
    fn native_event(&mut self, event: GlaExecutionEvent) -> Result<()> {
        self.ordinary.native_event(event)
    }

    fn leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let AsyncSpillSetSourcePlan::Procedure(source) = plan.source() else {
            return self.ordinary.leaf(
                plan,
                snapshot,
                cx,
                policy,
                scratch,
                destination,
                limits,
                resolver,
            );
        };
        Box::pin(async move {
            self.check_snapshot(snapshot)?;
            let pool = self.ordinary.view.memory_pool().clone();
            let mut argument_charge = reserve_values::<GraphValue>(
                &pool,
                cx,
                source.argument_count(),
                size_of::<Vec<GraphValue>>(),
            )?;
            let mut arguments_used = GlaExecutionStats::default();
            let arguments = source
                .evaluate_arguments(&mut |event| {
                    staged::native_event(cx, policy, &mut arguments_used, event)?;
                    if event == GlaExecutionEvent::ScratchEntry {
                        argument_charge
                            .grow(
                                cx,
                                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                            )
                            .map_err(SpillError::Memory)?;
                    }
                    Ok(())
                })
                .map_err(argument_error)?;
            let remaining = GqlQueryPolicy {
                rows: policy.rows,
                evaluator: GlaExecutionLimits::new(
                    policy
                        .evaluator
                        .max_work_units
                        .checked_sub(arguments_used.work_units)
                        .ok_or(NativeSpoolError::IncompleteCursor)?,
                    policy
                        .evaluator
                        .max_scratch_entries
                        .checked_sub(arguments_used.scratch_entries)
                        .ok_or(NativeSpoolError::IncompleteCursor)?,
                ),
            };
            let (cursor, rows, mut evaluator) = self
                .ordinary
                .view
                .execute_fnx_sealed_query_rows(
                    cx,
                    source.call(),
                    &arguments,
                    snapshot,
                    self.options.relation,
                    remaining,
                    self.options.memory,
                    self.options.sealing,
                    self.options.max_source_incidences,
                )
                .await
                .map_err(|error| rebase_arguments(error, policy, arguments_used))?;
            drop(arguments);
            drop(argument_charge);
            evaluator.work_units = evaluator
                .work_units
                .checked_add(arguments_used.work_units)
                .ok_or(SpillError::SizeOverflow)?;
            evaluator.scratch_entries = evaluator
                .scratch_entries
                .checked_add(arguments_used.scratch_entries)
                .ok_or(SpillError::SizeOverflow)?;
            let width = source.columns().len();
            let charge = reserve_values::<FnxValue>(&pool, cx, width, size_of::<ProcedureRows>())?;
            let cursor = ProcedureInput {
                cursor: ProcedureRows::new(cursor).map_err(procedure_error)?,
                _charge: charge,
                pool,
                cx,
                width,
                snapshot,
                rows,
                evaluator,
                delivered: 0,
                state: ScanState::Open,
            };
            let mut work = Work {
                cx,
                used: 0,
                limit: limits.max_sort_work,
            };
            let spool = drain(
                cx,
                source.columns().to_vec(),
                width,
                cursor,
                destination,
                limits.page_bytes,
                limits.max_row_bytes,
                Some(&mut work),
            )
            .await?;
            Ok((spool, work.used))
        })
    }

    fn stages<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        input: NativeResultSpool,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        prior_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        self.ordinary.stages(
            plan,
            input,
            cx,
            policy,
            scratch,
            destination,
            limits,
            prior_work,
            resolver,
        )
    }
}

// The native cursor retains only completed kernel state and one reusable FnX
// row. Reserve conversion backing before every pull; drain keeps the returned
// row's affine charge alive through canonical encoding and awaited appends.
struct ProcedureInput<'q> {
    cursor: ProcedureRows,
    _charge: MemoryCharge,
    pool: MemoryPool,
    cx: &'q QueryCx,
    width: usize,
    snapshot: CommitSeq,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    delivered: u64,
    state: ScanState,
}

impl SpoolInput for ProcedureInput<'_> {
    fn pull(&mut self) -> impl Future<Output = Option<Result<SpoolRow>>> + Send {
        async move {
            if self.state != ScanState::Open {
                return None;
            }
            self.state = ScanState::Failed;
            let result = (|| {
                let charge = reserve_values::<GraphValue>(
                    &self.pool,
                    self.cx,
                    self.width,
                    size_of::<GraphValueRow>(),
                )?;
                let row = self
                    .cursor
                    .next_row(&mut || self.cx.with_restriction(|| self.cx.checkpoint()))
                    .map_err(procedure_error)?;
                if row.is_none() {
                    self.cx
                        .with_restriction(|| self.cx.checkpoint())
                        .map_err(|error| procedure_error(GqlQueryError::Interrupted(error)))?;
                }
                if let Some(row) = row {
                    if row.len() != self.width {
                        return Err(NativeSpoolError::IncompleteCursor);
                    }
                    self.delivered = self
                        .delivered
                        .checked_add(1)
                        .ok_or(SpillError::SizeOverflow)?;
                    if self.delivered > self.rows.result_rows {
                        return Err(NativeSpoolError::IncompleteCursor);
                    }
                    self.state = ScanState::Open;
                    Ok(Some(SpoolRow::buffered((row, charge))))
                } else if self.delivered == self.rows.result_rows {
                    self.state = ScanState::Exhausted;
                    Ok(None)
                } else {
                    Err(NativeSpoolError::IncompleteCursor)
                }
            })();
            result.transpose()
        }
    }
    fn spool_state(&self) -> ScanState {
        self.state
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (self.snapshot, ScanKind::Set, self.rows, self.evaluator)
    }
}
