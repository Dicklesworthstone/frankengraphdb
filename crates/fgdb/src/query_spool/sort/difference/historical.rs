//! Bind once, pin both cuts, and feed complete ordered endpoint results into
//! the existing external difference cursor. No endpoint occurrence table.

use super::*;
use crate::{SnapshotEdgeSource, SnapshotVertexSource};
use fgdb_gql::algebra::{GlaOperator, PreparedGraphPattern};
use fgdb_gql::edge_stream::{EdgeScanCursor, EdgeScanError, EdgeScanPlan};
use fgdb_gql::scan_stream::ScanSortTail;
use fgdb_gql::stream::{VertexScanCursor, VertexScanPlan};

// Both sources own the SAME admitted generation before the future is returned.
// Defer cursor construction so the second receives only the first's remainder.
// Boxing bounds the sum's size without retaining any candidate or result rows.
enum PendingInput<'q> {
    Vertex(Box<(SnapshotVertexSource<'q>, VertexScanPlan<GraphValueRow>)>),
    Edge(Box<(SnapshotEdgeSource<'q>, EdgeScanPlan)>),
}
impl<'q> PendingInput<'q> {
    fn cursor(
        self,
        cx: &'q QueryCx,
        mut policy: GqlQueryPolicy,
        max_input_rows: u64,
    ) -> impl SpoolInput + 'q + use<'q> {
        policy.rows = match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, max_input_rows),
            None => GqlExecutionBudget::result_rows(max_input_rows),
        };
        let checkpoint = move || cx.with_restriction(|| cx.checkpoint());
        match self {
            Self::Vertex(pending) => {
                let (source, plan) = *pending;
                ScanCursor::Vertex(VertexScanCursor::new(source, plan, policy, checkpoint))
            }
            Self::Edge(pending) => {
                let (source, plan) = *pending;
                ScanCursor::Edge(Box::new(EdgeScanCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
        }
    }
}

struct Opened<'q> {
    columns: Vec<String>,
    before: (PendingInput<'q>, ScanSortTail),
    after: (PendingInput<'q>, ScanSortTail),
}

fn open_at<'q>(
    query: &PreparedGraphPattern<GraphValueRow>,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    at: CommitSeq,
) -> core::result::Result<(PendingInput<'q>, ScanSortTail), QueryError> {
    // These are the same physical compilers and sources as spool_ordered.
    // Compilation removes only the checked terminal sort/window. No primary
    // leaf, synthetic identity prefix or error-triggered fallback is substituted.
    if matches!(
        query.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        let source = view
            .edge_scan_source(cx, at)
            .map_err(|e| QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Source(e))))?;
        let (plan, tail) = EdgeScanPlan::compile_sort_input(query.plan())
            .map_err(|e| QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(e))))?;
        Ok((PendingInput::Edge(Box::new((source, plan))), tail))
    } else {
        let source = view
            .vertex_scan_source(cx, at)
            .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(e))))?;
        let (plan, tail) = VertexScanPlan::compile_sort_input(query.plan())
            .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(e))))?;
        Ok((PendingInput::Vertex(Box::new((source, plan))), tail))
    }
}

fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    before: CommitSeq,
    after: CommitSeq,
) -> core::result::Result<Opened<'q>, QueryError> {
    // Both endpoint fences precede binding and physical compilation, including
    // equal endpoints and LIMIT 0. A future cut is not clamped to this pin.
    view.snapshot
        .check_frontier(before)
        .map_err(QueryError::Read)?;
    view.snapshot
        .check_frontier(after)
        .map_err(QueryError::Read)?;
    cx.with_restriction(|| {
        cx.checkpoint()
            .map_err(|e| QueryError::Stream(GqlQueryError::Interrupted(e)))?;
        let PreparedNativeRead::Pattern(template) = prepared else {
            // In particular, a temporal statement cannot override one or both
            // explicit endpoint arguments. Aggregate/set classes are not scans.
            return Err(QueryError::StreamingUnsupported {
                facade: prepared.facade_class(),
            });
        };
        let query = template
            .bind_parameters(params)
            .map_err(QueryError::PatternText)?;
        let before = open_at(&query, view, cx, before)?;
        let after = open_at(&query, view, cx, after)?;
        cx.checkpoint()
            .map_err(|e| QueryError::Stream(GqlQueryError::Interrupted(e)))?;
        Ok(Opened {
            columns: query.columns().to_vec(),
            before,
            after,
        })
    })
}

impl PreparedNativeRead {
    /// Evaluate this ONE prepared query at two exact historical cuts and pull
    /// the complete result BAG difference, after minus before, through scratch.
    /// Unlike query_diff, neither endpoint nor its difference is collected in
    /// a resident result table. The decoded graph generation remains resident.
    ///
    /// Pin once and bind parameters once synchronously. Both endpoint bounds and
    /// physical plans are admitted before any scratch writer or source pull.
    /// The future borrows only cx and the two files: database, template and
    /// parameters may change/drop after this call. An unpolled future owns only
    /// its pins/plans and reserves no scratch. No resolver runs during execution.
    ///
    /// Admits the vertex/fixed-edge scan profiles of spool_ordered, including
    /// property-only projections, supported predicates, DISTINCT, hidden sort
    /// keys, direction/NULL placement and SKIP/LIMIT. Each endpoint executes its
    /// COMPLETE terminal order/window before canonical bag subtraction. Equal
    /// cuts still execute both inputs; LIMIT 0 never hides a late source error.
    /// Unsupported physical shapes, aggregate/set classes and explicit temporal
    /// selectors refuse. No eager fallback or temporal-text rewriting occurs.
    ///
    /// policy's source-record/work/scratch bounds cover BOTH source traversals.
    /// The second source receives the remainder, not a fresh allowance; its
    /// ordinary typed budget error reports that residual allowance. ResultRows
    /// counts only final nonzero changed tuples, not private endpoint results.
    /// max_input_rows bounds private occurrences PER ENDPOINT. max_runs bounds
    /// each individual external sort, with both files' append quotas also live.
    /// max_work_units is ONE additional physical allowance for both endpoint
    /// order/windows AND both difference sorts and all later pulls. The cursor's
    /// work_units includes that entire additional work, not source evaluation.
    ///
    /// Both files must be distinct query-private owners; a shared parent pool
    /// bounds their combined tracked buffers. Existing native ordered intake's
    /// transient native row/encoding and decoded snapshot remain outside it.
    /// Run sizes, row bytes, cleanup and poisoned-file rules are spool_ordered's.
    /// Completed intermediates remain append-only and consume file quotas.
    ///
    /// The returned cursor borrows only scratch and cx. Stage delivered changes
    /// until successful exhaustion; failure/drop is not a complete transition.
    /// This is privileged same-history result comparison, not authorized DIFF,
    /// cross-branch comparison, durable resume, or the wide-aggregate carrier.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_difference<'q, 's, V, A, B>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
        source: &'s mut SpillFile<A>,
        scratch: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
    ) -> impl Future<Output = VisitResult<NativeSpoolDifference<'q, B>>> + 's + use<'q, 's, V, A, B>
    where
        'q: 's,
        V: Vfs + Clone,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 's,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let opened = database
            .read_session()
            .map_err(QueryError::Read)
            .and_then(|view| open(self, &view, cx, params, before, after));
        execute(
            opened,
            cx,
            policy,
            source,
            scratch,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            max_work_units,
        )
    }

    /// Compare cuts retained by this immutable view, never a newer writer pin.
    /// A cut newer than view.frontier refuses even after the writer advances.
    /// All semantics, cumulative allowances and failure rules are those of
    /// spool_difference. Neither the returned future nor cursor borrows view.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_difference_in_view<'q, 's, A, B>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
        source: &'s mut SpillFile<A>,
        scratch: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
    ) -> impl Future<Output = VisitResult<NativeSpoolDifference<'q, B>>> + 's + use<'q, 's, A, B>
    where
        'q: 's,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 's,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        execute(
            open(self, view, cx, params, before, after),
            cx,
            policy,
            source,
            scratch,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            max_work_units,
        )
    }
}

fn subtract(limit: u64, spent: u64) -> Result<u64> {
    limit
        .checked_sub(spent)
        .ok_or(NativeSpoolError::IncompleteCursor)
}
fn private_policy(mut policy: GqlQueryPolicy) -> GqlQueryPolicy {
    policy.rows = match policy.rows.max_snapshot_records() {
        Some(records) => GqlExecutionBudget::snapshot_records(records),
        None => GqlExecutionBudget::UNLIMITED,
    };
    policy
}
fn remaining(mut policy: GqlQueryPolicy, first: &NativeResultSpool) -> Result<GqlQueryPolicy> {
    if let Some(limit) = policy.rows.max_snapshot_records() {
        policy.rows =
            GqlExecutionBudget::snapshot_records(subtract(limit, first.rows.snapshot_records)?);
    }
    policy.evaluator.max_work_units =
        subtract(policy.evaluator.max_work_units, first.evaluator.work_units)?;
    policy.evaluator.max_scratch_entries = subtract(
        policy.evaluator.max_scratch_entries,
        first.evaluator.scratch_entries,
    )?;
    Ok(policy)
}
fn physical_error(error: NativeSpoolError, prior: u64, limit: u64) -> NativeSpoolError {
    match error {
        NativeSpoolError::SortWorkLimit { attempted, .. } => match prior.checked_add(attempted) {
            Some(attempted) => NativeSpoolError::SortWorkLimit { attempted, limit },
            None => SpillError::SizeOverflow.into(),
        },
        error => error,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute<'q, 's, A, B>(
    opened: core::result::Result<Opened<'q>, QueryError>,
    cx: &'q QueryCx,
    policy: GqlQueryPolicy,
    source: &'s mut SpillFile<A>,
    scratch: &'q mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_work_units: u64,
) -> VisitResult<NativeSpoolDifference<'q, B>>
where
    'q: 's,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 's,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
{
    let Opened {
        columns,
        before: (before, before_tail),
        after: (after, after_tail),
    } = opened.map_err(|e| GqlQueryError::Source(NativeSpoolError::Prepare(Box::new(e))))?;
    let input_policy = private_policy(policy);
    let (before, used) = native(
        super::super::ordered_input(
            cx,
            columns.clone(),
            before.cursor(cx, input_policy, max_input_rows),
            before_tail,
            input_policy,
            scratch,
            source,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_work_units,
        )
        .await,
    )?;
    let input_policy = native(remaining(input_policy, &before))?;
    let allowance = native(subtract(max_work_units, used))?;
    let (after, spent) = native(
        super::super::ordered_input(
            cx,
            columns,
            after.cursor(cx, input_policy, max_input_rows),
            after_tail,
            input_policy,
            scratch,
            source,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            allowance,
        )
        .await
        .map_err(|e| physical_error(e, used, max_work_units)),
    )?;
    let used = used
        .checked_add(spent)
        .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
    // Original endpoint handles stay private. The same accepted scratch owner
    // supplies the cursor, and its first charge includes ALL preceding work.
    before
        .difference_continuing(
            &after,
            cx,
            source,
            scratch,
            run_rows,
            max_runs,
            page_bytes,
            policy.rows.max_result_rows().unwrap_or(u64::MAX),
            max_work_units,
            used,
        )
        .await
}

#[cfg(test)]
#[path = "historical/tests.rs"]
mod tests;
