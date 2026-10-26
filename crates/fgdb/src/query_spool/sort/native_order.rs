//! Compile the native terminal order/window into the existing spill pipeline.
//! No text rewriting, identity-prefix injection, eager fallback or new evaluator.

use super::*;
use crate::gql_exec::source::{SourceEvent, find_vertex};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::GlaOperator;
use fgdb_gql::edge_stream::{EdgeScanCursor, EdgeScanError, EdgeScanPlan};
use fgdb_gql::scan_stream::ScanSortTail;
use fgdb_gql::spill_set::{AsyncSpillElementSource, AsyncSpillSetPlan};
use fgdb_gql::stream::{VertexScanCursor, VertexScanPlan};
use fgdb_gql::{GlaExecutionEvent, GqlBudgetDimension, GqlExecutionBudget};
use fgdb_types::CanonicalPropertyValue;

#[path = "buffered_order.rs"]
mod buffered;
pub use buffered::PreparedBufferedOrder;
#[path = "binary.rs"]
mod binary;
#[path = "cross.rs"]
mod cross;
#[path = "relational_join.rs"]
mod relational_join;
#[path = "staged.rs"]
mod staged;
pub(crate) use staged::execute_with_controls as ordered_stages;

fn input_policy(policy: GqlQueryPolicy, max_input_rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy {
        // These are PRIVATE input occurrences. The original output allowance
        // is enforced only after ordering and pagination, never spent twice.
        rows: match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, max_input_rows),
            None => GqlExecutionBudget::result_rows(max_input_rows),
        },
        evaluator: policy.evaluator,
    }
}

// All sources, including the native singleton, enter the same physical
// execution contract before reserving a scratch writer or evaluating a row.
fn validate_spill_limits(
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_sort_work: u64,
) -> Result<()> {
    if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
        return Err(SpillError::InvalidLimits.into());
    }
    if max_runs == 0 {
        return Err(NativeSpoolError::SortRunLimit {
            required: 1,
            limit: 0,
        });
    }
    if max_sort_work == 0 {
        return Err(NativeSpoolError::SortWorkLimit {
            attempted: 1,
            limit: 0,
        });
    }
    Ok(())
}

pub(crate) struct OpenedOrder<I> {
    pub(crate) columns: Vec<String>,
    pub(crate) input: I,
    pub(crate) tail: ScanSortTail,
    // A unary relation first finishes its native graph leaf, including that
    // leaf's own order and page. The shared spill stages then consume those
    // exact rows; projecting in identity order would change failure ordinals.
    pub(crate) stages: Option<(AsyncSpillSetPlan, EmbeddedReadView)>,
}

fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
    max_input_rows: u64,
) -> core::result::Result<OpenedOrder<impl SpoolInput + 'q + use<'q>>, QueryError> {
    let bound_set = match prepared {
        PreparedNativeRead::Set(template) => Some((
            template
                .bind_parameters(params)
                .map_err(QueryError::SetText)?,
            view.frontier(),
        )),
        PreparedNativeRead::TemporalSet(template) => {
            let bound = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            Some((bound.query().clone(), bound.as_of()))
        }
        _ => None,
    };
    let mut projection = None;
    let mut projected_tail = None;
    let mut projected_columns = None;
    let mut stages = None;
    let unsupported = || QueryError::StreamingUnsupported {
        facade: prepared.facade_class(),
    };
    let (query, as_of) = if let Some((set, as_of)) = bound_set {
        // Admit a bound historical cut before choosing its physical profile,
        // including when that profile will be refused without opening a scan.
        view.snapshot.check_frontier(as_of).map_err(|error| {
            QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(error)))
        })?;
        if let Some((pull, tail)) = set.ordered_row_pull() {
            // This compiler proof permits projection directly in source order.
            // In particular, it also covers a constant over a zero-column
            // pattern without manufacturing a public identity column.
            let pattern = pull.pattern().clone();
            projection = Some(pull);
            projected_tail = Some(tail);
            projected_columns = Some(set.columns().to_vec());
            (pattern, as_of)
        } else {
            let plan = AsyncSpillSetPlan::compile(&set).map_err(|_| unsupported())?;
            // Binary products, groups and procedure sources require their own
            // physical owners. A unary chain retains exactly one graph leaf.
            let pattern = plan.source_pattern().ok_or_else(unsupported)?.clone();
            stages = Some((plan, view.clone()));
            (pattern, as_of)
        }
    } else {
        match prepared {
            PreparedNativeRead::Pattern(template) => (
                template
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?,
                view.frontier(),
            ),
            PreparedNativeRead::TemporalPattern(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                (bound.pattern().clone(), bound.as_of())
            }
            _ => return Err(unsupported()),
        }
    };
    // Exact-cut admission precedes physical compilation, as for ordinary scans.
    // This clones only the existing immutable source pin, never candidate rows.
    // Select once from the bound root, never by retrying a failed source or
    // compiler. Both variants retain their own errors, counters and one pin.
    let (cursor, tail) = if matches!(
        query.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        let source = view
            .edge_scan_source(cx, as_of)
            .map_err(|e| QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Source(e))))?;
        let (plan, tail) = EdgeScanPlan::compile_sort_input(query.plan())
            .map_err(|e| QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(e))))?;
        cx.with_restriction(|| cx.checkpoint())
            .map_err(|e| QueryError::EdgeStream(GqlQueryError::Interrupted(e)))?;
        (
            ScanCursor::Edge(Box::new(EdgeScanCursor::new(
                source,
                plan,
                input_policy(policy, max_input_rows),
                move || cx.with_restriction(|| cx.checkpoint()),
            ))),
            tail,
        )
    } else {
        let source = view
            .vertex_scan_source(cx, as_of)
            .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(e))))?;
        let (plan, tail) = match projected_tail {
            Some(tail) => VertexScanPlan::compile(query.plan()).map(|plan| (plan, tail)),
            None => VertexScanPlan::compile_sort_input(query.plan()),
        }
        .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(e))))?;
        cx.with_restriction(|| cx.checkpoint())
            .map_err(|e| QueryError::Stream(GqlQueryError::Interrupted(e)))?;
        let cursor = VertexScanCursor::new(
            source,
            plan,
            input_policy(policy, max_input_rows),
            move || cx.with_restriction(|| cx.checkpoint()),
        );
        let cursor = match projection {
            Some(pull) => ScanCursor::ProjectedVertex(Box::new((cursor, pull))),
            None => ScanCursor::Vertex(cursor),
        };
        (cursor, tail)
    };
    Ok(OpenedOrder {
        columns: projected_columns.unwrap_or_else(|| query.columns().to_vec()),
        input: cursor,
        tail,
        stages,
    })
}

// The source and all later element reads share one immutable generation/cut.
// Borrow stored values until the shared evaluator admits their destination
// copies; neither a row clone nor a second query source crosses this boundary.
struct ResidentElements<'s> {
    view: &'s EmbeddedReadView,
    as_of: CommitSeq,
    budget: GqlExecutionBudget,
    records: u64,
}

impl<'s> ResidentElements<'s> {
    fn read(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<()>,
    ) -> Result<Option<&'s CanonicalPropertyValue>> {
        let snapshot: &'s crate::Snapshot = &self.view.snapshot;
        snapshot.check_frontier(self.as_of).map_err(|error| {
            NativeSpoolError::Execute(Box::new(GqlQueryError::Source(match element {
                ElementId::Vertex(_) => ScanError::Vertex(VertexScanError::Source(error)),
                ElementId::Edge(_) => ScanError::Edge(EdgeScanError::Source(error)),
            })))
        })?;
        // A reached identity is one logical record even if it is absent,
        // retired, or repeated. Continue the source spool's original budget.
        let records = self
            .records
            .checked_add(1)
            .ok_or(SpillError::SizeOverflow)?;
        self.budget
            .check(GqlBudgetDimension::SnapshotRecords, records)
            .map_err(|error| NativeSpoolError::Execute(Box::new(GqlQueryError::Rows(error))))?;
        self.records = records;
        let mut physical = |event| {
            control(match event {
                SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            })
        };
        let props = match element {
            ElementId::Vertex(vid) => find_vertex(snapshot, vid, self.as_of, &mut physical)?
                .map(|row| row.props.as_slice()),
            ElementId::Edge(eid) => snapshot
                .adjacency_index()
                .statement_at_controlled(&snapshot.blocks, eid, self.as_of, &mut physical)?
                .and_then(|(block, row)| {
                    snapshot
                        .block_props
                        .get(block)
                        .and_then(Option::as_ref)
                        .map(|props| props.props_ref(row))
                }),
        }
        .unwrap_or(&[]);
        let mut low = 0;
        let mut high = props.len();
        while low < high {
            let middle = low + (high - low) / 2;
            control(GlaExecutionEvent::Work)?;
            match props[middle].0.cmp(&key) {
                core::cmp::Ordering::Less => low = middle + 1,
                core::cmp::Ordering::Greater => high = middle,
                core::cmp::Ordering::Equal => return Ok(Some(&props[middle].1)),
            }
        }
        Ok(None)
    }
}

impl<'s> AsyncSpillElementSource for ResidentElements<'s> {
    type Error = NativeSpoolError;
    type Property = &'s CanonicalPropertyValue;

    async fn element_property(
        &mut self,
        _: ElementId,
        _: PropertyKeyId,
    ) -> Option<Result<Option<Self::Property>>> {
        // This resident source requires the native evaluator's control. An
        // ungoverned caller cannot silently perform an unmetered index lookup.
        None
    }

    async fn element_property_with_control(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Option<Result<Option<Self::Property>>> {
        Some(self.read(element, key, control))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_opened<I, A, B>(
    opened: OpenedOrder<I>,
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
) -> Result<(NativeResultSpool, u64)>
where
    I: SpoolInput,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let source_policy = if opened.stages.is_some() {
        input_policy(policy, max_input_rows)
    } else {
        policy
    };
    let (input, work) = ordered_input(
        cx,
        opened.columns,
        opened.input,
        opened.tail,
        source_policy,
        scratch,
        destination,
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_sort_work,
    )
    .await?;
    let Some((plan, view)) = opened.stages else {
        return Ok((input, work));
    };
    let mut elements = ResidentElements {
        view: &view,
        as_of: input.snapshot_seq(),
        budget: policy.rows,
        records: input.row_stats().snapshot_records,
    };
    let (mut output, work) = staged::execute_with_elements(
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
        work,
        None,
    )
    .await
    .map_err(resident_stage_error)?;
    output.rows.snapshot_records = elements.records;
    Ok((output, work))
}

// Shared stages have no buffered source on this path. Preserve the owner's
// public execution-error accessor for controls; never disguise a source error.
fn resident_stage_error(error: NativeSpoolError) -> NativeSpoolError {
    let NativeSpoolError::BufferedExecute(error) = error else {
        return error;
    };
    let error = match *error {
        GqlQueryError::Source(error) => {
            return NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Source(error)));
        }
        GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
        GqlQueryError::Interrupted(error) => GqlQueryError::Interrupted(error),
        GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
        GqlQueryError::Data(error) => GqlQueryError::Data(error),
    };
    NativeSpoolError::Execute(Box::new(error))
}

impl PreparedNativeRead {
    /// Execute a native scan or fixed-edge join with external ORDER BY, THEN SKIP/LIMIT.
    ///
    /// Unlike spool_sorted's caller-supplied post-selection order, this compiles
    /// the query's OWN terminal clauses from its bound GLA. Property-only and
    /// property-first projections, explicit direction/null placement, implicit
    /// canonical whole-row ordering, predicates and supported EXISTS probes use
    /// the ordinary collectors and pinned sources. No synthetic identity column
    /// changes DISTINCT, ties or the public schema. Compiler-owned hidden
    /// ORDER BY cells remain in the evaluation row through sorting and paging,
    /// then the final writer retains the visible prefix without re-encoding its
    /// values. Hidden cells are never admitted under DISTINCT.
    /// DISTINCT compares complete canonical rows after sorting and BEFORE
    /// pagination; it retains only the previous unique frame, not an input-sized
    /// seen-set. Equal ORDER BY keys
    /// alone never collapse rows. Edge-rooted fixed-hop chains, branches and
    /// identity closures retain the native indexed join traversal, edge/path
    /// values and property predicates. Parallel edges and both undirected
    /// orientations remain distinct input occurrences; only explicit DISTINCT
    /// can collapse equal complete output rows. Traversal state is proportional
    /// to the admitted fixed hop count, not the number of matches.
    /// Unary Set and TemporalSet relations over one admitted pattern use the
    /// same external Project/Filter/Scope/UNWIND stages as buffered reads.
    /// Every source and intermediate order, DISTINCT and window finishes
    /// before the next expression runs. Computed property-only output therefore
    /// keeps native ordering and source-row error coordinates. Properties are
    /// captured by that governed pattern; properties reached through carried
    /// elements borrow the same pinned generation at its exact cut. Each
    /// reached lookup spends the original snapshot-record allowance, while
    /// history/key searches share the cumulative native evaluator controls.
    /// Binary/group/procedure sources and
    /// unsupported physical instructions refuse before execution.
    ///
    /// Opening binds parameters and pins the exact source synchronously. The
    /// future borrows ONLY cx and the two scratch files; writer/template/params
    /// may change or drop. Unpolled futures reserve no scratch. Unsupported
    /// shapes refuse even under LIMIT 0. Complete source evaluation and sorting
    /// precede pagination, so a small page cannot hide a later data exception.
    ///
    /// policy's source-record/work/scratch limits govern one source traversal.
    /// max_input_rows is a separate intermediate-occurrence ceiling (reported
    /// by the input cursor's ResultRows error). policy.rows.max_result_rows is
    /// enforced on the FINAL selected rows, including zero, before acceptance.
    /// The returned spool reports source snapshot records, final result count
    /// and cumulative source/stage evaluator statistics. The second returned value
    /// is ADDITIONAL sort/window work under max_sort_work: neither the sort nor
    /// the final window resets that allowance. Spill buffers/catalogs are byte-
    /// charged to their existing pools; source logical scratch remains separate.
    ///
    /// Both files must be distinct query-private files; use children of a common
    /// pool for a combined resident bound. Runs append without quota refunds or
    /// recycling. Intermediates initially use destination, sort into scratch,
    /// then the final page is appended to destination. Only its complete result
    /// handle escapes. Failures/drop keep the existing poisoning/cleanup laws.
    /// sort_into documents run_rows/max_runs/page_bytes and minimum headroom.
    /// DISTINCT additionally retains one pool-charged maximum-row frame; its
    /// byte comparisons checkpoint in at most 4 KiB chunks under max_sort_work.
    /// Hidden-prefix framing and writes spend that same allowance, borrowing the
    /// already charged input frame and existing output page instead of copying
    /// another maximum-size row. The complete hidden tail is validated first.
    /// One native row plus its canonical encoding and the decoded source remain
    /// outside the spill pool. This is not a Warden grant, durable result, or a
    /// claim that every GQL operator or the underlying database is out-of-core.
    /// The returned future proves Send here, so callers do not recursively
    /// re-prove every source, transformation and spill-write future.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_ordered<'q, V, A, B>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let opened = database
            .read_session()
            .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(e))))
            .and_then(|view| open(self, &view, cx, params, policy, max_input_rows));
        Box::pin(async move {
            let opened = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            execute_opened(
                opened,
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
            )
            .await
        })
    }

    /// The same query-owned external order/window at this already-pinned view.
    /// A temporal selector uses its bound historical cut, not a newer live head.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_ordered_in_view<'q, A, B>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let opened = open(self, view, cx, params, policy, max_input_rows);
        Box::pin(async move {
            let opened = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            execute_opened(
                opened,
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
            )
            .await
        })
    }
}

#[allow(clippy::too_many_arguments)]
// Scoped sources enter this same pipeline after the host has reserved their
// signed physical allowance. They retain their native statistics and errors.
pub(crate) async fn ordered_input<I, A, B>(
    cx: &QueryCx,
    columns: Vec<String>,
    cursor: I,
    tail: ScanSortTail,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_sort_work: u64,
) -> Result<(NativeResultSpool, u64)>
where
    I: SpoolInput,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    if columns.len() != tail.visible_width() {
        return Err(SpillError::InvalidRun.into());
    }
    validate_order(tail.order(), tail.evaluation_width())?;
    validate_spill_limits(run_rows, max_runs, page_bytes, max_sort_work)?;
    let mut work = Work {
        cx,
        used: 0,
        limit: max_sort_work,
    };
    let input = drain(
        cx,
        columns,
        tail.evaluation_width(),
        cursor,
        destination,
        page_bytes,
        max_row_bytes,
        Some(&mut work),
    )
    .await?;
    // The bound terminal operator owns its comparison law. A synthesized
    // canonical key vector can equal an explicit ASC NULLS FIRST vector;
    // never infer language ordering from that vector or from observed rows.
    let (sorted, used) = input
        .sort_query_continuing(
            cx,
            destination,
            scratch,
            tail.order(),
            tail.has_explicit_order(),
            run_rows,
            max_runs,
            page_bytes,
            max_sort_work,
            work.used,
        )
        .await?;
    work.used = used;
    let result = window(
        &sorted,
        scratch,
        destination,
        &tail,
        policy.rows,
        page_bytes,
        &mut work,
    )
    .await?;
    Ok((result, work.used))
}

trait Window: Sync {
    fn evaluation_width(&self) -> usize;
    fn visible_width(&self) -> usize;
    fn distinct(&self) -> bool;
    fn offset(&self) -> u64;
    fn count(&self) -> Option<u64>;
}

impl Window for ScanSortTail {
    fn evaluation_width(&self) -> usize {
        ScanSortTail::evaluation_width(self)
    }
    fn visible_width(&self) -> usize {
        ScanSortTail::visible_width(self)
    }
    fn distinct(&self) -> bool {
        ScanSortTail::distinct(self)
    }
    fn offset(&self) -> u64 {
        ScanSortTail::offset(self)
    }
    fn count(&self) -> Option<u64> {
        ScanSortTail::count(self)
    }
}

impl Window for fgdb_gql::spill_set::SpillSetStage {
    fn evaluation_width(&self) -> usize {
        self.columns().len()
    }
    fn visible_width(&self) -> usize {
        self.columns().len()
    }
    fn distinct(&self) -> bool {
        fgdb_gql::spill_set::SpillSetStage::distinct(self)
    }
    fn offset(&self) -> u64 {
        fgdb_gql::spill_set::SpillSetStage::offset(self)
    }
    fn count(&self) -> Option<u64> {
        fgdb_gql::spill_set::SpillSetStage::count(self)
    }
}

async fn window<A, B>(
    sorted: &NativeResultSpool,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    tail: &impl Window,
    budget: GqlExecutionBudget,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    if sorted.encoded_columns != tail.evaluation_width()
        || sorted.columns.len() != tail.visible_width()
        || (tail.distinct() && tail.visible_width() != tail.evaluation_width())
    {
        return Err(SpillError::InvalidRun.into());
    }
    let mut reader = sorted.reader(scratch);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    let mut skip = tail.offset();
    let mut selected = 0_u64;
    let mut largest = 0;
    let mut previous: Option<TrackedBytes> = None;
    while let Some(row) = reader.next_row(work.cx).await? {
        work.charge(1)?;
        // The sort's complete-row tie-break makes all equal frames adjacent,
        // even when ORDER BY names only some columns and merges many runs.
        // Compare actual canonical bytes, never an order key or a hash.
        if let Some(last) = &previous
            && equal_frame(last.as_ref(), row.as_ref(), &mut |units| work.charge(units))?
        {
            continue;
        }
        if skip != 0 {
            skip -= 1;
        } else if tail.count().is_none_or(|count| selected < count) {
            let next = selected.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            budget
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|e| NativeSpoolError::Execute(Box::new(GqlQueryError::Rows(e))))?;
            let bytes = if tail.visible_width() == tail.evaluation_width() {
                write_row(&mut writer, row.as_ref(), work).await?;
                row.len()
            } else {
                write_visible_row(&mut writer, row.as_ref(), tail, work).await?
            };
            largest = largest.max(bytes);
            selected = next;
        }
        if tail.distinct() {
            // Move the admitted frame, including skipped classes, rather than
            // cloning payloads. ALL retains no predecessor and keeps every row.
            previous = Some(row);
        }
    }
    // Read and authenticate the complete sorted population even after LIMIT.
    // No source/transfer failure is converted into a successful page prefix.
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(reader);
    drop(previous);
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        columns: Arc::clone(&sorted.columns),
        encoded_columns: tail.visible_width(),
        snapshot: sorted.snapshot,
        kind: sorted.kind,
        rows: GqlExecutionStats {
            snapshot_records: sorted.rows.snapshot_records,
            result_rows: selected,
        },
        evaluator: sorted.evaluator,
        max_row_bytes: largest,
        run,
    })
}

// The complete row has already participated in order and window selection.
// Reframe the original visible cells directly into the charged page writer;
// no typed decoding, scalar conversion or second row allocation is involved.
async fn write_visible_row<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    writer: &mut PagedSpillWriter<'_, F>,
    row: &[u8],
    tail: &impl Window,
    work: &mut Work<'_>,
) -> Result<usize> {
    let frames =
        canonical::visible_prefix(row, tail.evaluation_width(), tail.visible_width(), work)?;
    let header = canonical::ROW.len() + 8;
    let bytes = header
        .checked_add(frames.len())
        .ok_or(SpillError::SizeOverflow)?;
    let len = u64::try_from(bytes).map_err(|_| SpillError::SizeOverflow)?;
    let width = u64::try_from(tail.visible_width()).map_err(|_| SpillError::SizeOverflow)?;
    work.charge(header)?;
    writer.write(work.cx, &len.to_be_bytes()).await?;
    writer.write(work.cx, canonical::ROW).await?;
    writer.write(work.cx, &width.to_be_bytes()).await?;
    for chunk in frames.chunks(4096) {
        work.charge(chunk.len())?;
        writer.write(work.cx, chunk).await?;
    }
    Ok(bytes)
}

// Inputs have authenticated and passed the native structural frame validator.
// Length inequality is decisive; equal lengths still require exact bytes.
// A bounded chunk is the largest indivisible payload comparison. Controls
// precede work even for empty frames or a mismatch at the very first byte.
fn equal_frame(
    left: &[u8],
    right: &[u8],
    charge: &mut impl FnMut(usize) -> Result<()>,
) -> Result<bool> {
    charge(1)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (a, b) in left.chunks(4096).zip(right.chunks(4096)) {
        charge(a.len())?;
        if a != b {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
#[path = "native_order_tests.rs"]
mod tests;
