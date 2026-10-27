//! External execution of the checked native unary relation program. Every
//! completed child is a real scratch barrier: source order, intermediate
//! DISTINCT/window and expression-failure precedence survive composition.

use super::*;
use crate::{BufferedEdge, BufferedReadView, BufferedValue, VertexRow};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::{
    AsyncSpillElementSource, AsyncSpillSetPlan, SpillSetRowError, SpillSetStage, SpillSetValuesPlan,
};
use fgdb_gql::{GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded};
use fgdb_types::CanonicalScalarResolver;

// A property lends its scalar while owning the authenticated cold record and
// that record's byte grant. The evaluator reserves its destination copy before
// cloning; neither a property table nor an uncharged scalar crosses the seam.
enum PinnedProperty {
    Vertex(BufferedValue<VertexRow>, usize),
    Edge(BufferedValue<BufferedEdge>, usize),
}

impl std::borrow::Borrow<fgdb_types::CanonicalPropertyValue> for PinnedProperty {
    fn borrow(&self) -> &fgdb_types::CanonicalPropertyValue {
        match self {
            Self::Vertex(record, at) => &record.props[*at].1,
            Self::Edge(record, at) => &record.props[*at].1,
        }
    }
}

struct ElementSource<'v, 'q, V: Vfs> {
    view: &'v mut BufferedReadView<V>,
    cx: &'q QueryCx,
    as_of: CommitSeq,
    budget: GqlExecutionBudget,
    records: u64,
}

impl<V: Vfs> ElementSource<'_, '_, V> {
    fn admit_record(&mut self) -> Result<()> {
        // A reached identity is one logical record even when absent, retired,
        // or repeated. Keep every stage on the completed source's allowance,
        // and refuse before admitting a cold record or its I/O.
        let records = self
            .records
            .checked_add(1)
            .ok_or(SpillError::SizeOverflow)?;
        self.budget
            .check(GqlBudgetDimension::SnapshotRecords, records)
            .map_err(|error| {
                NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
            })?;
        self.records = records;
        Ok(())
    }
}

impl<V: Vfs + Clone> AsyncSpillElementSource for ElementSource<'_, '_, V> {
    type Error = NativeSpoolError;
    type Property = PinnedProperty;

    async fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<PinnedProperty>>> {
        if let Err(error) = self.admit_record() {
            return Some(Err(error));
        }
        let cx = self.cx;
        let as_of = self.as_of;
        let view = &mut *self.view;
        let read = async move {
            match element {
                ElementId::Vertex(vid) => {
                    let record = view
                        .vertex_at(cx, vid, as_of)
                        .await
                        .map_err(|error| NativeSpoolError::BufferedRead(Box::new(error)))?;
                    Ok(record.and_then(|record| {
                        record
                            .props
                            .binary_search_by_key(&key, |(key, _)| *key)
                            .ok()
                            .map(|at| PinnedProperty::Vertex(record, at))
                    }))
                }
                ElementId::Edge(eid) => {
                    let record = view
                        .edge_at(cx, eid, as_of)
                        .await
                        .map_err(|error| NativeSpoolError::BufferedRead(Box::new(error)))?;
                    Ok(record.and_then(|record| {
                        record
                            .props
                            .binary_search_by_key(&key, |(key, _)| *key)
                            .ok()
                            .map(|at| PinnedProperty::Edge(record, at))
                    }))
                }
            }
        };
        Some(cx.with_restriction_async(read).await)
    }

    async fn element_metadata_with_control(
        &mut self,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Option<Result<Option<GraphValue>>> {
        use crate::gql_exec::property_maps::{self, MetadataRecord};
        if let Err(error) = self.admit_record() {
            return Some(Err(error));
        }
        let cx = self.cx;
        let as_of = self.as_of;
        let view = &mut *self.view;
        let read = async move {
            let failure = |error| {
                NativeSpoolError::Execute(Box::new(GqlQueryError::Source(match element {
                    ElementId::Vertex(_) => ScanError::Vertex(VertexScanError::Source(error)),
                    ElementId::Edge(_) => ScanError::Edge(EdgeScanError::Source(error)),
                })))
            };
            let mut physical = |event| {
                control(match event {
                    SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                    SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                })
            };
            physical(SourceEvent::Work)?;
            match element {
                ElementId::Vertex(id) => {
                    let Some(record) = view
                        .vertex_at(cx, id, as_of)
                        .await
                        .map_err(|error| NativeSpoolError::BufferedRead(Box::new(error)))?
                    else {
                        return Ok(None);
                    };
                    property_maps::metadata(
                        MetadataRecord::Vertex(&record),
                        kind,
                        catalog,
                        |_| true,
                        |_| true,
                        &mut physical,
                        &failure,
                    )
                    .map(Some)
                }
                ElementId::Edge(id) => {
                    let Some(record) = view
                        .edge_at(cx, id, as_of)
                        .await
                        .map_err(|error| NativeSpoolError::BufferedRead(Box::new(error)))?
                    else {
                        return Ok(None);
                    };
                    property_maps::metadata(
                        MetadataRecord::Edge(record.entry.relation, &record.props),
                        kind,
                        catalog,
                        |_| true,
                        |_| true,
                        &mut physical,
                        &failure,
                    )
                    .map(Some)
                }
            }
        };
        Some(cx.with_restriction_async(read).await)
    }
}

#[derive(Clone, Copy)]
struct Limits {
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
}

// Continue the original source's native allowance. Physical decode/encoding,
// copying and sorting have their separate cumulative Work counter below.
pub(super) fn native_event(
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    event: GlaExecutionEvent,
) -> Result<()> {
    cx.with_restriction(|| cx.checkpoint()).map_err(|error| {
        NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Interrupted(error)))
    })?;
    if event == GlaExecutionEvent::ResultRow {
        // The checked unary evaluator never admits final rows. A stage must
        // finish in full before the following stage or final quota can run.
        return Err(NativeSpoolError::IncompleteCursor);
    }
    let next = |used: u64, added: u64, limit, dimension| {
        let observed = u128::from(used) + u128::from(added);
        if observed > u128::from(limit) {
            Err(NativeSpoolError::BufferedExecute(Box::new(
                GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }),
            )))
        } else {
            Ok(observed as u64)
        }
    };
    let work_units = next(
        stats.work_units,
        1,
        policy.evaluator.max_work_units,
        GlaLimitDimension::WorkUnits,
    )?;
    let scratch_entries = next(
        stats.scratch_entries,
        u64::from(event == GlaExecutionEvent::ScratchEntry),
        policy.evaluator.max_scratch_entries,
        GlaLimitDimension::ScratchEntries,
    )?;
    *stats = GlaExecutionStats {
        work_units,
        scratch_entries,
    };
    Ok(())
}

/// Materialize the native Values source using ordinary canonical row frames.
/// No graph cursor, schema-carrier pattern or graph record is manufactured.
/// Its one private occurrence precedes every native stage and output window.
#[allow(clippy::too_many_arguments)]
pub(super) async fn seed_values<F>(
    plan: &SpillSetValuesPlan,
    snapshot: CommitSeq,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    destination: &mut SpillFile<F>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
) -> Result<(NativeResultSpool, u64)>
where
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    super::validate_spill_limits(run_rows, max_runs, page_bytes, max_sort_work)?;
    let pool = destination.memory_pool().clone();
    let mut evaluator = GlaExecutionStats::default();
    let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
    let cursor = plan
        .start(&mut |event| {
            native_event(cx, policy, &mut evaluator, event)?;
            if event == GlaExecutionEvent::ScratchEntry {
                charge
                    .grow(
                        cx,
                        8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                    )
                    .map_err(SpillError::Memory)?;
            }
            Ok(())
        })
        .map_err(|error| match error {
            SpillSetRowError::Control(error) => error,
            SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
        })?;
    let mut state = (cursor, charge);
    let mut work = Work {
        cx,
        used: 0,
        limit: max_sort_work,
    };
    let mut writer = destination.paged_writer(cx, page_bytes)?;
    let mut rows = 0;
    let mut largest = 0;
    loop {
        let next = state
            .0
            .next_with_control(&mut |event| {
                native_event(cx, policy, &mut evaluator, event)?;
                if event == GlaExecutionEvent::ScratchEntry {
                    state
                        .1
                        .grow(
                            cx,
                            8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                        )
                        .map_err(SpillError::Memory)?;
                }
                Ok(())
            })
            .map_err(|error| match error {
                SpillSetRowError::Control(error) => error,
                SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
            })?;
        let Some((row, remaining)) = next else {
            break;
        };
        state.0 = remaining;
        if !row.is_empty() || rows != 0 {
            return Err(SpillError::InvalidRun.into());
        }
        GqlExecutionBudget::result_rows(max_input_rows)
            .check(GqlBudgetDimension::ResultRows, 1)
            .map_err(|error| {
                NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
            })?;
        // The source's state reservation stays live through this row's entire
        // encoded write, even though its remaining cursor is already empty.
        let encoded = codec::encode(&pool, &mut work, &row, max_row_bytes)?;
        write_row(&mut writer, &encoded.0, &mut work).await?;
        largest = encoded.0.len();
        rows = 1;
        drop(encoded);
        drop(row);
    }
    if rows != 1 {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok((
        NativeResultSpool {
            columns: Vec::new().into(),
            encoded_columns: 0,
            snapshot,
            kind: ScanKind::Values,
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: rows,
            },
            evaluator,
            max_row_bytes: largest,
            run,
        },
        work.used,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<V, A, B>(
    plan: AsyncSpillSetPlan,
    input: NativeResultSpool,
    view: &mut BufferedReadView<V>,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    prior_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    V: Vfs + Clone,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let mut elements = ElementSource {
        view,
        cx,
        as_of: input.snapshot_seq(),
        budget: policy.rows,
        records: input.row_stats().snapshot_records,
    };
    let (mut output, work) = execute_with_elements(
        plan,
        input,
        &mut elements,
        cx,
        policy,
        scratch,
        destination,
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_input_rows,
        max_sort_work,
        prior_work,
        resolver,
    )
    .await?;
    output.rows.snapshot_records = elements.records;
    Ok((output, work))
}

/// Continue the same native stage program over an already completed source.
/// The host supplies only exact-cut element reads; evaluation, every semantic
/// barrier, spill ownership and cumulative allowances remain shared with the
/// buffered executor. A resident source never needs a synthetic buffered view.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_with_elements<A, B, S>(
    plan: AsyncSpillSetPlan,
    input: NativeResultSpool,
    elements: &mut S,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    prior_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    S: AsyncSpillElementSource<Error = NativeSpoolError>,
{
    execute_with_controls(
        plan,
        input,
        elements,
        &mut || Ok(()),
        cx,
        policy,
        scratch,
        destination,
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_input_rows,
        max_sort_work,
        prior_work,
        resolver,
    )
    .await
}

/// Continue native stages under a host's live work admission. The callback
/// precedes each native work/scratch event, including element reads, and may
/// retain an authority refusal. It never admits result rows: final delivery
/// remains the caller's separate owner. Source and stage native statistics
/// and physical work remain cumulative across every materialization barrier.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_with_controls<A, B, S>(
    plan: AsyncSpillSetPlan,
    mut input: NativeResultSpool,
    elements: &mut S,
    checkpoint: &mut (dyn FnMut() -> Result<()> + Send),
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    prior_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    S: AsyncSpillElementSource<Error = NativeSpoolError>,
{
    if plan.stages().is_empty() || input.columns() != plan.source_columns() {
        return Err(SpillError::InvalidRun.into());
    }
    let limits = Limits {
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_input_rows,
    };
    let mut work = Work {
        cx,
        used: prior_work,
        limit: max_sort_work,
    };
    let mut evaluator = input.evaluator_stats();
    let mut in_destination = true;
    for (ordinal, stage) in plan.stages().iter().enumerate() {
        let budget = if ordinal + 1 == plan.stages().len() {
            policy.rows
        } else {
            GqlExecutionBudget::result_rows(max_input_rows)
        };
        input = if in_destination {
            run_stage(
                input,
                stage,
                destination,
                scratch,
                limits,
                budget,
                policy,
                &mut evaluator,
                &mut work,
                resolver,
                elements,
                checkpoint,
            )
            .await?
        } else {
            run_stage(
                input,
                stage,
                scratch,
                destination,
                limits,
                budget,
                policy,
                &mut evaluator,
                &mut work,
                resolver,
                elements,
                checkpoint,
            )
            .await?
        };
        in_destination = !in_destination;
    }
    if !in_destination {
        input = copy(&input, scratch, destination, page_bytes, &mut work).await?;
    }
    if input.columns() != plan.columns() {
        return Err(SpillError::InvalidRun.into());
    }
    input.evaluator = evaluator;
    Ok((input, work.used))
}

// This function always leaves its result in destination. Both files append;
// no handle is overwritten, recycled or selected by retrying a failed phase.
#[allow(clippy::too_many_arguments)]
async fn run_stage<A, B, S>(
    input: NativeResultSpool,
    stage: &SpillSetStage,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    limits: Limits,
    budget: GqlExecutionBudget,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    elements: &mut S,
    checkpoint: &mut (dyn FnMut() -> Result<()> + Send),
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    S: AsyncSpillElementSource<Error = NativeSpoolError>,
{
    let transformed = transform(
        &input,
        stage,
        source,
        destination,
        limits,
        policy,
        evaluator,
        work,
        resolver,
        elements,
        checkpoint,
    )
    .await?;
    if let Some(requested) = stage.order()
        && !stage.columns().is_empty()
    {
        // Empty requested order is native canonical tuple order. Explicit
        // keys use language orderability, including numeric representation
        // ties. Preserve that distinction before expanding an empty key list.
        let count = if requested.is_empty() {
            stage.columns().len()
        } else {
            requested.len()
        };
        work.charge(count)?;
        let charge = destination
            .memory_pool()
            .reserve(
                work.cx,
                count
                    .checked_mul(size_of::<GraphValueOrder>())
                    .ok_or(SpillError::SizeOverflow)?,
            )
            .map_err(SpillError::Memory)?;
        let order = (
            if requested.is_empty() {
                (0..count)
                    .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
                    .collect::<Vec<_>>()
            } else {
                requested.to_vec()
            },
            charge,
        );
        let (sorted, used) = transformed
            .sort_query_continuing(
                work.cx,
                destination,
                source,
                &order.0,
                !requested.is_empty(),
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                work.limit,
                work.used,
            )
            .await?;
        work.used = used;
        drop(order);
        window(
            &sorted,
            source,
            destination,
            stage,
            budget,
            limits.page_bytes,
            work,
        )
        .await
    } else {
        // Filter/Scope keep the child's sequence unless the native plan orders
        // them explicitly. Materialize first even for a zero output count, so
        // a later predicate failure cannot disappear behind pagination.
        if stage.distinct() && !stage.columns().is_empty() {
            return Err(SpillError::InvalidRun.into());
        }
        let selected = window(
            &transformed,
            destination,
            source,
            stage,
            budget,
            limits.page_bytes,
            work,
        )
        .await?;
        copy(&selected, source, destination, limits.page_bytes, work).await
    }
}

// Keep the evaluator's future separate from orchestration and sorting Send
// proofs. This is one allocation per stage, before the input-row loop; the
// complete transform remains lazy and retains the same affine byte charges.
#[allow(clippy::too_many_arguments)]
fn transform<'q, A, B, S>(
    input: &'q NativeResultSpool,
    stage: &'q SpillSetStage,
    source: &'q mut SpillFile<A>,
    destination: &'q mut SpillFile<B>,
    limits: Limits,
    policy: GqlQueryPolicy,
    evaluator: &'q mut GlaExecutionStats,
    work: &'q mut Work<'_>,
    resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    elements: &'q mut S,
    checkpoint: &'q mut (dyn FnMut() -> Result<()> + Send),
) -> crate::SendFuture<'q, Result<NativeResultSpool>>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    S: AsyncSpillElementSource<Error = NativeSpoolError>,
{
    Box::pin(async move {
        if input.encoded_columns != stage.input_types().len() {
            return Err(SpillError::InvalidRun.into());
        }
        let pool = destination.memory_pool().clone();
        let mut reader = input.reader(source);
        let mut writer = destination.paged_writer(work.cx, limits.page_bytes)?;
        let mut rows = 0_u64;
        let mut row_index = 0_usize;
        let mut largest = 0_usize;
        while let Some(bytes) = reader.next_row(work.cx).await? {
            work.charge(bytes.len())?;
            let reserved = codec::decoded(bytes.as_ref(), work.cx)?;
            let mut charge = pool
                .reserve(work.cx, reserved)
                .map_err(SpillError::Memory)?;
            let row = match resolver {
                Some(resolver) => {
                    GraphValueRow::decode_canonical_with_resolver(bytes.as_ref(), resolver)
                }
                None => GraphValueRow::decode_stored(bytes.as_ref()),
            }
            .map_err(NativeSpoolError::Decode)?;
            drop(bytes);
            let retained = codec::row_memory(&row, work)?;
            codec::retain_reservation(&mut charge, retained, work.cx)?;
            let cursor = stage
                .start_with_async_elements(
                    row,
                    row_index,
                    &mut |event| {
                        if event != GlaExecutionEvent::ResultRow {
                            checkpoint()?;
                        }
                        native_event(work.cx, policy, evaluator, event)?;
                        if event == GlaExecutionEvent::ScratchEntry {
                            let bytes =
                                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
                            charge.grow(work.cx, bytes).map_err(SpillError::Memory)?;
                        }
                        Ok(())
                    },
                    elements,
                    &|bytes| {
                        pool.reserve(work.cx, bytes)
                            .map_err(|error| NativeSpoolError::Spill(SpillError::Memory(error)))
                    },
                )
                .await
                .map_err(|error| match error {
                    SpillSetRowError::Control(error) => error,
                    SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
                })?;
            // Evaluation has finished, so its temporary workspace can be released.
            // Preserve the initial cursor footprint through the entire expansion:
            // it covers the original iterator allocation AND payloads moved into
            // outputs, including asynchronous appends. Native work/scratch counts
            // stay cumulative; only no-longer-resident byte admission is refunded.
            let retained = codec::cursor_memory(&cursor, work)?;
            codec::retain_reservation(&mut charge, retained, work.cx)?;
            // The cursor's payloads are dropped before its affine charge.
            let mut state = (cursor, charge);
            row_index = row_index.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            loop {
                // Prefix copies belong to one output occurrence, not to the full
                // expansion. Refund them before pulling the next row while the
                // original input/list reservation remains live. Native work and
                // scratch-entry accounting nevertheless continue cumulatively.
                let mut charge = pool.reserve(work.cx, 0).map_err(SpillError::Memory)?;
                let next = state
                    .0
                    .next_with_control(&mut |event| {
                        if event != GlaExecutionEvent::ResultRow {
                            checkpoint()?;
                        }
                        native_event(work.cx, policy, evaluator, event)?;
                        if event == GlaExecutionEvent::ScratchEntry {
                            let bytes =
                                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
                            charge.grow(work.cx, bytes).map_err(SpillError::Memory)?;
                        }
                        Ok(())
                    })
                    .map_err(|error| match error {
                        SpillSetRowError::Control(error) => error,
                        SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
                    })?;
                let Some((row, remaining)) = next else {
                    break;
                };
                state.0 = remaining;
                if row.len() != stage.columns().len() {
                    return Err(SpillError::InvalidRun.into());
                }
                let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
                GqlExecutionBudget::result_rows(limits.max_input_rows)
                    .check(GqlBudgetDimension::ResultRows, next)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                    })?;
                let row = SpoolRow::buffered((row, charge));
                // Data precedes each affine charge in both aggregates. They stay
                // intact across every await, including cancellation and unwinding.
                let encoded = codec::encode(&pool, work, &row.row, limits.max_row_bytes)?;
                write_row(&mut writer, &encoded.0, work).await?;
                largest = largest.max(encoded.0.len());
                rows = next;
                drop(encoded);
                drop(row);
            }
        }
        if reader.state() != ScanState::Exhausted
            || u64::try_from(row_index).ok() != Some(input.row_count())
        {
            return Err(NativeSpoolError::IncompleteCursor);
        }
        drop(reader);
        work.charge(1)?;
        let run = writer.finish(work.cx).await?;
        Ok(NativeResultSpool {
            columns: stage.columns().to_vec().into(),
            encoded_columns: stage.columns().len(),
            snapshot: input.snapshot,
            kind: input.kind,
            rows: GqlExecutionStats {
                snapshot_records: input.rows.snapshot_records,
                result_rows: rows,
            },
            evaluator: *evaluator,
            max_row_bytes: largest,
            run,
        })
    })
}

pub(super) async fn copy<A, B>(
    input: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let mut reader = input.reader(source);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    while let Some(row) = reader.next_row(work.cx).await? {
        write_row(&mut writer, row.as_ref(), work).await?;
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(reader);
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        run,
        ..input.clone()
    })
}
