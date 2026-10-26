//! Historical result differences stay inside the ordered session's authority.
//! The existing masked sources and external sorter retain all query semantics.

use super::*;
use crate::NativeResultSpool;

// Delay cursor construction, not source admission. Both sources own the same
// pin before returning a future; the second cursor receives the first's exact
// remaining native allowance. Neither source can mint another signed permit.
type PendingInput<'q> = Box<dyn FnOnce(GqlQueryPolicy) -> OrderedInput<'q> + Send + 'q>;
struct Endpoint<'q> {
    input: PendingInput<'q>,
    tail: ScanSortTail,
    columns: Vec<String>,
    stages: Option<(AsyncSpillSetPlan, EmbeddedReadView)>,
    at: CommitSeq,
}

struct CompletedEndpoint {
    spool: NativeResultSpool,
    work: u64,
    // Stage spools retain their source's row statistics. Carried element
    // lookups have their own governed counter, which must also reduce the
    // second endpoint's allowance rather than disappear at a spill barrier.
    snapshot_records: u64,
}

// Complete the same source order/page and unary stage program as an ordinary
// ordered read. Both endpoints append to the same two private files; finishing
// the second endpoint never replaces the first endpoint's completed run.
#[allow(clippy::too_many_arguments)]
async fn complete_endpoint<'q, C, A, B>(
    endpoint: Endpoint<'q>,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    limits: AuthorizedSortLimits,
    max_sort_work: u64,
) -> Result<CompletedEndpoint>
where
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let Endpoint {
        input,
        tail,
        columns,
        stages,
        at,
    } = endpoint;
    let mut source_policy = policy;
    if stages.is_some() {
        source_policy.rows = match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, limits.max_input_rows),
            None => GqlExecutionBudget::result_rows(limits.max_input_rows),
        };
    }
    let (spool, work) = ordered_input(
        cx,
        columns,
        input(source_policy),
        tail,
        source_policy,
        scratch,
        destination,
        limits.run_rows,
        limits.max_runs,
        limits.page_bytes,
        limits.max_row_bytes,
        max_sort_work,
    )
    .await?;
    let snapshot_records = spool.row_stats().snapshot_records;
    let Some((stages, view)) = stages else {
        return Ok(CompletedEndpoint {
            spool,
            work,
            snapshot_records,
        });
    };
    let mut elements = ScopedElements {
        view: &view,
        cx,
        at,
        execution: execution.clone(),
        budget: policy.rows,
        records: snapshot_records,
    };
    let (spool, work) = ordered_stages(
        stages,
        spool,
        &mut elements,
        &mut || execution.borrow_mut().checkpoint().map_err(authorized),
        cx,
        policy,
        scratch,
        destination,
        limits.run_rows,
        limits.max_runs,
        limits.page_bytes,
        limits.max_row_bytes,
        limits.max_input_rows,
        max_sort_work,
        work,
        None,
    )
    .await
    .map_err(stage_error)?;
    Ok(CompletedEndpoint {
        spool,
        work,
        snapshot_records: elements.records,
    })
}

fn pending<'q, C: FnMut() -> u64 + Send>(
    plan: RowPlan,
    pull: Option<GraphSetRowPull>,
    at: CommitSeq,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    max_input_rows: u64,
) -> core::result::Result<PendingInput<'q>, QueryError> {
    let input_policy = move |mut policy: GqlQueryPolicy| {
        policy.rows = match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, max_input_rows),
            None => GqlExecutionBudget::result_rows(max_input_rows),
        };
        policy
    };
    let control = execution.clone();
    match plan {
        RowPlan::Vertex(plan) => {
            let source = ScopedSource {
                inner: view
                    .vertex_scan_source(cx, at)
                    .map_err(QueryError::Read)?
                    .with_root_equality(&plan, |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    }),
                execution,
            };
            Ok(Box::new(move |policy| {
                let rows = VertexInput(VertexScanCursor::new(
                    source,
                    plan,
                    input_policy(policy),
                    move || control.borrow_mut().checkpoint(),
                ));
                let input: Box<dyn InputCursor + 'q> = match pull {
                    Some(pull) => Box::new(SetInput { rows, pull }),
                    None => Box::new(rows),
                };
                OrderedInput(input)
            }))
        }
        RowPlan::Edge(plan) => {
            let source = aggregate::edge::ordered_source(at, view, cx, execution)?;
            Ok(Box::new(move |policy| {
                OrderedInput(Box::new(EdgeInput(EdgeScanCursor::new(
                    source,
                    plan,
                    input_policy(policy),
                    move || control.borrow_mut().checkpoint(),
                ))))
            }))
        }
    }
}

fn private_policy(mut policy: GqlQueryPolicy) -> GqlQueryPolicy {
    policy.rows = match policy.rows.max_snapshot_records() {
        Some(records) => GqlExecutionBudget::snapshot_records(records),
        None => GqlExecutionBudget::UNLIMITED,
    };
    policy
}
fn remainder(limit: u64, used: u64) -> Result<u64> {
    limit
        .checked_sub(used)
        .ok_or(NativeSpoolError::IncompleteCursor)
}
fn after_policy(mut policy: GqlQueryPolicy, first: &CompletedEndpoint) -> Result<GqlQueryPolicy> {
    if let Some(limit) = policy.rows.max_snapshot_records() {
        policy.rows =
            GqlExecutionBudget::snapshot_records(remainder(limit, first.snapshot_records)?);
    }
    policy.evaluator.max_work_units = remainder(
        policy.evaluator.max_work_units,
        first.spool.evaluator_stats().work_units,
    )?;
    policy.evaluator.max_scratch_entries = remainder(
        policy.evaluator.max_scratch_entries,
        first.spool.evaluator_stats().scratch_entries,
    )?;
    Ok(policy)
}
fn difference_error(error: GqlQueryError<NativeSpoolError, Cancel>) -> NativeSpoolError {
    match error {
        GqlQueryError::Source(error) => error,
        GqlQueryError::Interrupted(error) => SpillError::Interrupted(error).into(),
        GqlQueryError::Rows(error) => authorized(QueryError::Stream(GqlQueryError::Rows(error))),
        GqlQueryError::Evaluator(error) => {
            authorized(QueryError::Stream(GqlQueryError::Evaluator(error)))
        }
        GqlQueryError::Data(error) => authorized(QueryError::Stream(GqlQueryError::Data(error))),
        GqlQueryError::IdentifiedEdgesRequired => {
            authorized(QueryError::Stream(GqlQueryError::IdentifiedEdgesRequired))
        }
    }
}

impl<R, C, A, B> AuthorizedOrderedReadSession<'_, R, C, A, B>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    /// Visit every changed tuple under the same pull state machine exposed by
    /// [`Self::difference_stream`]. The callback is awaited before another pull,
    /// and live authority is checked around every callback poll. A callback
    /// cannot hide expiry or retirement behind its own error.
    ///
    /// The visitor may receive a prefix before a later comparison, callback, or
    /// credential failure. Stage that prefix until this future returns success.
    /// Retrying may repeat callbacks. Callback unwind closes the session; an
    /// ordinary callback error preserves it subject to scratch poisoning rules.
    #[allow(clippy::too_many_arguments)]
    pub fn visit_difference<'q, F, Fut>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        mut visitor: F,
    ) -> Result<crate::SendFuture<'q, Result<Vec<String>>>>
    where
        F: FnMut(i128, TrackedBytes) -> Fut + Send + 'q,
        Fut: Future<Output = Result<()>> + Send + 'q,
    {
        let execution = self.difference_stream(cx, prepared, params, before, after)?;
        Ok(Box::pin(async move {
            let mut cursor = execution.await?;
            while let Some((weight, row)) = cursor.next_change().await? {
                // Invoke the visitor inside the guarded poll, not before it: a
                // visitor that unwinds while building its future must leave the
                // pin guard armed, so dropping the cursor closes the session.
                cursor
                    .checked_callback(async { visitor(weight, row).await })
                    .await?;
            }
            Ok(cursor.columns().to_vec())
        }))
    }
}

#[cfg(test)]
#[path = "difference/tests.rs"]
mod tests;

#[path = "difference/pull.rs"]
mod pull;
pub use pull::{AuthorizedDifferenceCursor, AuthorizedDifferenceExecution};
