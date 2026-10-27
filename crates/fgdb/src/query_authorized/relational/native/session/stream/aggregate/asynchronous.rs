//! Cooperative source intake with the ordinary native aggregate states.
//! One pending pull owns its reducer, snapshot pin and live signed permit.

use super::*;
use core::future::poll_fn;
use core::task::{Context, Poll};
use fgdb_gql::edge_stream::EdgeScanState;
use fgdb_gql::edge_stream::aggregate::EdgeAggregateCursor;

type AsyncPull<'q> = Box<
    dyn FnMut(&mut Context<'_>) -> Poll<Result<(Option<GraphAggregateRow>, bool), QueryError>>
        + Send
        + 'q,
>;

/// Completed native aggregates with cooperative source intake. Each poll
/// examines at most 64 root/incidence candidates or join transitions before
/// yielding, including hidden and predicate-rejected candidates. One current
/// row and the existing governed group/DISTINCT/COLLECT state are retained;
/// scratch files and an eager source bag are unnecessary.
///
/// The physical aggregate plan, exact values, output layout and selection are
/// the same as stream_aggregate(). No partial group escapes. Source/probe,
/// accumulation and delivery share one cumulative native meter and permit.
/// Individual source callbacks, probes and completed-group selection remain
/// synchronous governed sections; this is not a wall-clock or byte bound.
///
/// Keep one next_row() future while processing unrelated host controls.
/// Dropping a polled pending future releases the source and all group state,
/// fuses the cursor and closes its session, rather than resuming a lost prefix.
/// A normal close before/after a pull preserves a valid reusable session.
pub struct AuthorizedAsyncAggregateCursor<'q> {
    inner: AuthorizedRowCursor<'q, GraphAggregateRow, AsyncPull<'q>>,
    layout: Layout,
}

impl AuthorizedAsyncAggregateCursor<'_> {
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
    }
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.layout.slots
    }
    pub fn key_columns(&self) -> &[String] {
        &self.layout.keys
    }
    pub fn aggregate_columns(&self) -> &[String] {
        &self.layout.values
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }
    pub fn state(&self) -> VertexScanState {
        self.inner.state()
    }
    pub fn close(&mut self) {
        self.inner.close();
    }

    pub async fn next_row(&mut self) -> Result<Option<GraphAggregateRow>, QueryError> {
        if self.inner.state != VertexScanState::Open {
            return Ok(None);
        }
        self.inner.state = VertexScanState::Failed;
        let mut guard = self.inner.guard.take();
        if let Some(guard) = &mut guard {
            guard.armed = true;
        }
        let mut driver = self
            .inner
            .driver
            .take()
            .expect("open aggregate owns driver");
        let result = poll_fn(|context| driver(context)).await;
        match result {
            Ok((value, finished)) => {
                if let Some(guard) = &mut guard {
                    guard.armed = false;
                }
                if finished || value.is_none() {
                    self.inner.state = VertexScanState::Exhausted;
                } else {
                    self.inner.state = VertexScanState::Open;
                    self.inner.driver = Some(driver);
                    self.inner.guard = guard;
                }
                Ok(value)
            }
            Err(error) => {
                let result = Err(error);
                if let Some(guard) = &mut guard {
                    guard.armed = terminal(&result);
                }
                result
            }
        }
    }
}

impl core::fmt::Debug for AuthorizedAsyncAggregateCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedAsyncAggregateCursor")
            .field("state", &self.state())
            .field("authority_source_and_position", &"[REDACTED]")
            .finish()
    }
}

fn build<'q, C: FnMut() -> u64 + Send>(
    bound: Bound,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphAggregateRow, Layout, AsyncPull<'q>> {
    let Bound {
        plan,
        at,
        columns,
        layout,
    } = bound;
    let driver: AsyncPull<'q> = match plan {
        Plan::Vertex(plan) => {
            let source = ScopedSource {
                inner: view
                    .vertex_scan_source(cx, at)
                    .map_err(QueryError::Read)?
                    .with_root_equality_predicates(plan.leading_root_predicates(), |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    })
                    .with_root_prefix(plan.leading_root_prefix(), |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    })
                    .with_root_membership(plan.leading_root_membership(), |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    }),
                execution: execution.clone(),
            };
            let control = execution.clone();
            let mut cursor = VertexAggregateCursor::new(source, plan, policy, move || {
                control.borrow_mut().checkpoint()
            });
            execution.borrow_mut().checkpoint()?;
            Box::new(move |context| {
                // Fresh authority around every bounded poll; zero extra work
                // units keeps hidden skip quanta out of resource accounting.
                execution.borrow_mut().reserve_work(0)?;
                let result = cursor.poll_next(context);
                execution.borrow_mut().reserve_work(0)?;
                let Poll::Ready(row) = result else {
                    return Poll::Pending;
                };
                let row = row.transpose().map_err(error)?;
                execution.borrow_mut().deliver(usize::from(row.is_some()))?;
                Poll::Ready(Ok((row, cursor.state() != VertexScanState::Open)))
            })
        }
        Plan::Edge(plan) => {
            let source = edge::ordered_source(at, view, cx, execution.clone())?;
            let control = execution.clone();
            let mut cursor = EdgeAggregateCursor::new(source, plan, policy, move || {
                control.borrow_mut().checkpoint()
            });
            execution.borrow_mut().checkpoint()?;
            Box::new(move |context| {
                execution.borrow_mut().reserve_work(0)?;
                let result = cursor.poll_next(context);
                execution.borrow_mut().reserve_work(0)?;
                let Poll::Ready(row) = result else {
                    return Poll::Pending;
                };
                let row = row.transpose().map_err(edge::error)?;
                execution.borrow_mut().deliver(usize::from(row.is_some()))?;
                Poll::Ready(Ok((row, cursor.state() != EdgeScanState::Open)))
            })
        }
    };
    Ok((AuthorizedRowCursor::new(driver, columns, at), layout))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send> AuthorizedReadSession<'_, R, C> {
    /// Open the same scoped aggregate physical plan with a Send cooperative
    /// driver. Admission reads no candidates and requires no scratch policy.
    /// Hold one next_row() future across host credit/Ping waits; each source
    /// quantum rechecks live authorization and cancellation without resetting
    /// either allowance. Only completed delivered groups spend signed rows.
    pub fn stream_aggregate_async<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedAsyncAggregateCursor<'q>, QueryError> {
        let (inner, layout) = self.open_cursor(cx, prepared, params, bind_aggregate, build)?;
        Ok(AuthorizedAsyncAggregateCursor { inner, layout })
    }
}
