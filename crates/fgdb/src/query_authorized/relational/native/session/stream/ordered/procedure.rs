//! Registered FnX sources under the ordered session's original authority.
//! Graph admission and kernels remain bounded resident operations. Completed
//! native rows feed the shared authenticated external relation one at a time.

use super::*;
use crate::GqlError;
use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
use crate::query::ProcedureRows;
use crate::query::authorized::analytics;
use crate::query::view::spool_input;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::{SpillSetProcedurePlan, SpillSetRowError};
use fgdb_gql::{GqlQueryExecution, GraphProcedureRowCursor};
use fgdb_prism::{FnxGraphKind, FnxSignatureRegistry, FnxValue, SnapshotBinding};
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryPool};

// Inspect every registered source before any graph or scratch demand. Value,
// domain and parameter-combination errors remain lazy at the reached call;
// in particular, an inactive correlated body never evaluates its arguments.
pub(super) fn admit(source: &SpillSetProcedurePlan) -> bool {
    use fgdb_gql::ProcedureGraphDirection;

    let call = source.call();
    if call.namespace() != "fnx" {
        return false;
    }
    let Some(signature) = FnxSignatureRegistry::lookup(&format!("fnx.{}", call.name())) else {
        return false;
    };
    // The external bag owner preserves every native row and selected column
    // order, including edge pairs and scalar results with no leading vertex.
    if call.outputs().is_empty() {
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
        if !names.is_empty() {
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
        return !kernel;
    }
    signature.parameters.iter().all(|parameter| {
        !parameter.positional
            || parameter.default.is_some()
            || names.iter().any(|name| name == parameter.name)
    })
}

fn procedure_error(error: GqlQueryError<GqlError, QueryError>) -> NativeSpoolError {
    authorized(crate::query::authorized::relational::set_error(
        error.map_source(fgdb_gql::GraphSetExecutionError::Source),
    ))
}

fn argument_error(error: SpillSetRowError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        SpillSetRowError::Control(error) => error,
        SpillSetRowError::Native(error) => stage_error(NativeSpoolError::SetExecution(error)),
    }
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<'q, C, F>(
    host: &mut ScopedOrderHost<'q, C>,
    source: &SpillSetProcedurePlan,
    frame: Option<&GraphValueRow>,
    snapshot: CommitSeq,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    destination: &mut SpillFile<F>,
    limits: ExternalOrderLimits,
) -> Result<(NativeResultSpool, u64)>
where
    C: FnMut() -> u64 + Send,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let pool = destination.memory_pool().clone();
    let execution = host.execution.clone();
    let mut usage = AdmissionUsage::default();
    let mut argument_charge = reserve_values::<GraphValue>(
        &pool,
        cx,
        source.argument_count(),
        size_of::<Vec<GraphValue>>(),
    )?;
    let (arguments, argument_records) = {
        let mut elements = ScopedElements {
            view: &host.view,
            cx: host.cx,
            at: snapshot,
            execution: execution.clone(),
            budget: policy.rows,
            records: 0,
        };
        let arguments = source
            .evaluate_arguments_with_async_elements(
                frame,
                &mut |event| {
                    let event = match event {
                        GlaExecutionEvent::Work => SourceEvent::Work,
                        GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                        GlaExecutionEvent::ResultRow => {
                            return Err(NativeSpoolError::IncompleteCursor);
                        }
                    };
                    execution.borrow_mut().checkpoint().map_err(authorized)?;
                    usage
                        .observe::<GqlError, QueryError>(policy, event)
                        .map_err(procedure_error)?;
                    if event == SourceEvent::ScratchEntry {
                        argument_charge
                            .grow(
                                cx,
                                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                            )
                            .map_err(SpillError::Memory)?;
                    }
                    Ok(())
                },
                &mut elements,
                &|bytes| {
                    pool.reserve(cx, bytes)
                        .map_err(|error| NativeSpoolError::Spill(SpillError::Memory(error)))
                },
            )
            .await
            .map_err(argument_error)?;
        (arguments, elements.records)
    };
    // Property/metadata argument reads already spent their evaluator events.
    // Register their admitted records on the SAME usage accumulator without
    // charging those events again, then continue it through source and kernel.
    if !usage.absorbs(policy, argument_records, 0, 0) {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    usage.absorb(argument_records, 0, 0);
    let mut remaining = usage.remaining(policy);
    if let Some(records) = policy.rows.max_snapshot_records() {
        remaining.rows = GqlExecutionBudget::new(
            records
                .checked_sub(argument_records)
                .ok_or(NativeSpoolError::IncompleteCursor)?,
            policy.rows.max_result_rows().unwrap_or(u64::MAX),
        );
    }
    let spec = crate::query::bind_procedure(source.call(), &arguments).map_err(procedure_error)?;
    let options = crate::query::procedure_options(&spec, snapshot, remaining);
    analytics::preflight(&spec, options.projection.directedness)
        .map_err(crate::query::procedure_failure)
        .map_err(procedure_error)?;
    let scope = execution.borrow().permit.predicates().clone();
    let width = source.columns().len();
    let charge = reserve_values::<FnxValue>(&pool, cx, width, size_of::<ProcedureRows>())?;
    // All synchronous analytics callbacks use ONLY this local execution cell.
    // The shared lock and cell are gone before any scratch future is polled.
    let (cursor, rows, evaluator) = cx.with_restriction(|| {
        execution.with_local(|local| {
            let cursor = analytics::execute_rows(
                &host.view.snapshot,
                SnapshotBinding {
                    root: host.view.snapshot.root.0,
                    as_of: snapshot,
                },
                &spec,
                options,
                &scope,
                local,
                analytics::UsageObserver {
                    usage: &mut usage,
                    policy,
                },
            )
            .map_err(crate::query::procedure_failure)
            .map_err(procedure_error)?;
            let count = u64::try_from(cursor.remaining()).map_err(|_| SpillError::SizeOverflow)?;
            policy
                .rows
                .check(GqlBudgetDimension::ResultRows, count)
                .map_err(|error| procedure_error(GqlQueryError::Rows(error)))?;
            // Dense sparse-result scans visit the whole visible population;
            // scalar acyclicity still visits one result on an empty graph.
            let pulls = cursor.certificate().vertices.max(cursor.remaining());
            let cursor = ProcedureRows::new(cursor).map_err(procedure_error)?;
            analytics::reserve_cursor_pull(
                pulls,
                local,
                analytics::UsageObserver {
                    usage: &mut usage,
                    policy,
                },
            )
            .map_err(crate::query::procedure_failure)
            .map_err(procedure_error)?;
            let stats = usage
                .finish::<GqlError, QueryError, ()>(
                    policy,
                    Ok(GqlQueryExecution::<()> {
                        rows: GqlExecutionStats {
                            snapshot_records: usage.snapshot_records(),
                            result_rows: count,
                        },
                        evaluator: GlaExecutionStats::default(),
                        value: Vec::new(),
                    }),
                )
                .map_err(procedure_error)?;
            Ok::<_, NativeSpoolError>((cursor, stats.rows, stats.evaluator))
        })
    })?;
    drop(arguments);
    drop(argument_charge);
    let cursor = ProcedureInput {
        cursor,
        _charge: charge,
        pool,
        cx,
        execution,
        width,
        snapshot,
        rows,
        evaluator,
        delivered: 0,
        state: ScanState::Open,
    };
    spool_input(
        cx,
        source.columns().to_vec(),
        cursor,
        destination,
        limits.page_bytes,
        limits.max_row_bytes,
        limits.max_sort_work,
    )
    .await
}

// The native result keeps its bounded kernel owner and one reusable FnX row.
// Each converted row retains its reservation through encoding and awaited IO.
struct ProcedureInput<'s, 'q, C> {
    cursor: ProcedureRows,
    _charge: MemoryCharge,
    pool: MemoryPool,
    cx: &'s QueryCx,
    execution: Shared<'q, C>,
    width: usize,
    snapshot: CommitSeq,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    delivered: u64,
    state: ScanState,
}

fn poll_live<C: FnMut() -> u64>(execution: &Shared<'_, C>) -> core::result::Result<(), QueryError> {
    let mut live = execution.borrow_mut();
    live.poll()?;
    // Pull work was reserved before spooling. Fresh checks spend no second
    // unit and cannot reuse an old clock sample throughout result traversal.
    let now = live.sample();
    let checked = live.permit.checkpoint_at(now);
    checked.map_err(|error| live.refusal(error))
}

impl<C: FnMut() -> u64 + Send> SpoolInput for ProcedureInput<'_, '_, C> {
    async fn pull(&mut self) -> Option<Result<SpoolRow>> {
        if self.state != ScanState::Open {
            return None;
        }
        self.state = ScanState::Failed;
        let result = self.cx.with_restriction(|| {
            poll_live(&self.execution).map_err(authorized)?;
            let charge = reserve_values::<GraphValue>(
                &self.pool,
                self.cx,
                self.width,
                size_of::<GraphValueRow>(),
            )?;
            let row = self
                .cursor
                .next_row(&mut || poll_live(&self.execution))
                .map_err(procedure_error)?;
            if row.is_none() {
                poll_live(&self.execution).map_err(authorized)?;
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
        });
        result.transpose()
    }

    fn spool_state(&self) -> ScanState {
        self.state
    }

    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (self.snapshot, ScanKind::Set, self.rows, self.evaluator)
    }
}
