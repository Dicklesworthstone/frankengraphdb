//! Cooperative row intake through the existing scoped source and GLA cursor.
//! Pending retains one source position, signed permit and cumulative meter.

use super::*;
use core::task::Poll;
use fgdb_gql::edge_stream::{EdgeScanCursor, EdgeScanState};

/// A capability-checked row stream that yields after at most 64 root/incidence
/// examinations or fixed-join transitions. Hidden and rejected candidates,
/// projected SKIP rows and the required post-LIMIT suffix also yield. Native
/// predicates, projections, ordering, errors and logical charges are identical
/// to stream_send(); one source callback or probe remains synchronous.
///
/// Keep one next_row() future while servicing host controls. Dropping a polled
/// pending future fuses the cursor, releases its source and closes the session.
/// Closing between pulls preserves a valid reusable session and scans nothing.
/// No row queue, background task, restart token or physical byte bound is added.
pub struct AuthorizedAsyncRowCursor<'q> {
    inner: AuthorizedRowCursor<'q, GraphValueRow, AsyncPull<'q, GraphValueRow>>,
}

impl AuthorizedAsyncRowCursor<'_> {
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
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
    pub async fn next_row(&mut self) -> Result<Option<GraphValueRow>, QueryError> {
        self.inner.next_async().await
    }
}

impl core::fmt::Debug for AuthorizedAsyncRowCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedAsyncRowCursor")
            .field("state", &self.state())
            .field("authority_source_and_position", &"[REDACTED]")
            .finish()
    }
}

fn projection_error(ordinal: u64, column: usize, error: fgdb_gql::GraphIntegerError) -> QueryError {
    QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection {
        row: usize::try_from(ordinal).unwrap_or(usize::MAX),
        column,
        error,
    }))
}

fn build<'q, C: FnMut() -> u64 + Send>(
    selected: BoundRows,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphValueRow, (), AsyncPull<'q, GraphValueRow>> {
    let (plan, at, columns, pull) = selected;
    let driver: AsyncPull<'q, GraphValueRow> = match plan {
        RowPlan::Vertex(plan) => {
            let source = ScopedSource {
                inner: view
                    .vertex_scan_source(cx, at)
                    .map_err(QueryError::Read)?
                    .with_root_equality(&plan, |key| {
                        execution.borrow().permit.predicates().allows_property(key)
                    }),
                execution: execution.clone(),
            };
            let control = execution.clone();
            let mut cursor = VertexScanCursor::new(source, plan, policy, move || {
                control.borrow_mut().checkpoint()
            });
            execution.borrow_mut().checkpoint()?;
            Box::new(move |context| {
                // Poll-only authority checks bound control latency without
                // charging hidden source quanta as logical work.
                execution.borrow_mut().reserve_work(0)?;
                let result = if let Some(pull) = &pull {
                    cursor.poll_next_projected(context, pull, scan_error, projection_error)
                } else {
                    cursor
                        .poll_next(context)
                        .map(|row| row.map(|row| row.map_err(scan_error)))
                };
                execution.borrow_mut().reserve_work(0)?;
                let Poll::Ready(row) = result else {
                    return Poll::Pending;
                };
                let row = row.transpose()?;
                execution.borrow_mut().deliver(usize::from(row.is_some()))?;
                Poll::Ready(Ok((row, cursor.state() != VertexScanState::Open)))
            })
        }
        RowPlan::Edge(plan) => {
            let source = aggregate::edge::ordered_source(at, view, cx, execution.clone())?;
            let control = execution.clone();
            let mut cursor = EdgeScanCursor::new(source, plan, policy, move || {
                control.borrow_mut().checkpoint()
            });
            execution.borrow_mut().checkpoint()?;
            Box::new(move |context| {
                execution.borrow_mut().reserve_work(0)?;
                let result = if let Some(pull) = &pull {
                    cursor.poll_next_projected(
                        context,
                        pull,
                        aggregate::edge::row_error,
                        projection_error,
                    )
                } else {
                    cursor
                        .poll_next(context)
                        .map(|row| row.map(|row| row.map_err(aggregate::edge::row_error)))
                };
                execution.borrow_mut().reserve_work(0)?;
                let Poll::Ready(row) = result else {
                    return Poll::Pending;
                };
                let row = row.transpose()?;
                execution.borrow_mut().deliver(usize::from(row.is_some()))?;
                Poll::Ready(Ok((row, cursor.state() != EdgeScanState::Open)))
            })
        }
    };
    Ok((AuthorizedRowCursor::new(driver, columns, at), ()))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send> AuthorizedReadSession<'_, R, C> {
    /// Open the same native row profile as stream_send(), with cooperative
    /// source intake. Opening reads no candidates; every pending quantum
    /// revalidates authority and cancellation under the original allowances.
    pub fn stream_async<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedAsyncRowCursor<'q>, QueryError> {
        let (inner, ()) = self.open_cursor(cx, prepared, params, bind, build)?;
        Ok(AuthorizedAsyncRowCursor { inner })
    }
}
