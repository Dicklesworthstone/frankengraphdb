//! Completed aggregates from the existing masked source and native reducer.
//! No per-row query, input bag, alternate accumulator, or privileged fallback.
use super::*;
use fgdb_gql::edge_stream::aggregate::EdgeAggregatePlan;
use fgdb_gql::stream::aggregate::{
    VertexAggregateCursor, VertexAggregateError, VertexAggregatePlan,
};
use fgdb_gql::{
    GraphAggregateError, GraphAggregateRow, GraphAggregateTextSlot, GraphSetExecutionError,
    PreparedGraphAggregate,
};

#[path = "aggregate/edge.rs"]
mod edge;

struct Layout {
    slots: Vec<GraphAggregateTextSlot>,
    keys: Vec<String>,
    values: Vec<String>,
}
enum Plan {
    Vertex(VertexAggregatePlan),
    Edge(EdgeAggregatePlan),
}
impl Plan {
    fn columns(&self) -> &[String] {
        match self {
            Self::Vertex(plan) => plan.columns(),
            Self::Edge(plan) => plan.columns(),
        }
    }
    fn key_columns(&self) -> &[String] {
        match self {
            Self::Vertex(plan) => plan.key_columns(),
            Self::Edge(plan) => plan.key_columns(),
        }
    }
}
struct Bound {
    plan: Plan,
    at: CommitSeq,
    columns: Vec<String>,
    layout: Layout,
}

/// A capability-checked aggregate cursor over the session's fixed generation.
/// Opening consumes no candidates. First demand consumes the aggregate input;
/// later demands move complete groups out, not recompute or rerun the query.
/// Only delivered groups spend signed rows. Source/accumulation errors precede
/// any output; a later delivery refusal preserves only already-delivered groups.
///
/// RETURN layout is metadata over separate keys()/values(): use output_slots()
/// with columns(). Repeated aliases require no duplicated list/scalar payload.
/// Source nodes/work, masking, expiry, owner checks and pin lifetime share the
/// ordinary scoped cursor. Close/drop never drains undelivered input or groups.
/// Group state, argument DISTINCT and COLLECT remain governed in-memory state,
/// not bounded by result rows; a global numeric summary has fixed cell count.
/// No raw source counters, reusable permit, spill or durable lease is exported.
pub struct AuthorizedAggregateCursor<'q> {
    inner: AuthorizedRowCursor<'q, GraphAggregateRow>,
    layout: Layout,
}
impl AuthorizedAggregateCursor<'_> {
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
}
impl Iterator for AuthorizedAggregateCursor<'_> {
    type Item = Result<GraphAggregateRow, QueryError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}
impl FusedIterator for AuthorizedAggregateCursor<'_> {}
impl core::fmt::Debug for AuthorizedAggregateCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedAggregateCursor")
            .field("state", &self.state())
            .field("authority_source_and_position", &"[REDACTED]")
            .finish()
    }
}

fn compiled(
    query: &PreparedGraphAggregate,
    at: CommitSeq,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    facade: crate::NativeReadClass,
) -> Result<Bound, QueryError> {
    // Choose the source from the compiler-owned root, never by trial execution
    // or a failed compilation. Each physical compiler retains every clause and
    // rejects unsupported relational inputs rather than extracting a first leaf.
    let plan = if matches!(
        query.input_pattern().plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        Plan::Edge(
            EdgeAggregatePlan::compile(query).map_err(QueryError::EdgeAggregateStreamPlan)?,
        )
    } else {
        let plan = VertexAggregatePlan::compile(query).map_err(QueryError::AggregateStreamPlan)?;
        admit_source_profile(query.input_pattern())?;
        Plan::Vertex(plan)
    };
    // Preserve compiler-owned RETURN ordinals without an extra value encoder.
    // Check the complete layout before any source can be opened.
    if columns.len() != slots.len()
        || columns.len() > fgdb_gql::algebra::MAX_PATTERN_VERTICES
        || slots.iter().any(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => at >= plan.key_columns().len(),
            GraphAggregateTextSlot::Aggregate(at) => at >= plan.columns().len(),
        })
    {
        return Err(QueryError::StreamingUnsupported { facade });
    }
    let layout = Layout {
        slots: slots.to_vec(),
        keys: plan.key_columns().to_vec(),
        values: plan.columns().to_vec(),
    };
    Ok(Bound {
        plan,
        at,
        columns: columns.to_vec(),
        layout,
    })
}
fn bind_aggregate(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    at: CommitSeq,
) -> Result<Bound, QueryError> {
    let facade = prepared.facade_class();
    match prepared {
        PreparedNativeRead::Aggregate(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            compiled(
                &query,
                at,
                prepared.columns(),
                prepared.output_slots(),
                facade,
            )
        }
        PreparedNativeRead::TemporalAggregate(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            compiled(
                query.aggregate(),
                query.as_of(),
                prepared.columns(),
                prepared.output_slots(),
                facade,
            )
        }
        PreparedNativeRead::PipelineAggregate(prepared) => {
            if prepared.is_source_free() {
                return Err(QueryError::StreamingUnsupported { facade });
            }
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PipelineText)?;
            compiled(
                &query,
                at,
                prepared.columns(),
                prepared.output_slots(),
                facade,
            )
        }
        _ => Err(QueryError::StreamingUnsupported { facade }),
    }
}

// Authorization and context errors must stay at the outer QueryError so the
// common pin guard sees terminal credentials. Other aggregate faults retain
// their exact native variants; a source failure is never an empty group.
fn error(error: VertexAggregateError<QueryError, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(GraphAggregateError::Source(error)) => {
            scan_error(GqlQueryError::Source(error))
        }
        GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Source(error),
        )) => scan_error(GqlQueryError::Source(error)),
        GqlQueryError::Source(GraphAggregateError::InputRelation(error)) => {
            QueryError::AggregateStream(GqlQueryError::Source(GraphAggregateError::InputRelation(
                error.map_source(|_| unreachable!("source arm handled above")),
            )))
        }
        GqlQueryError::Source(error) => QueryError::AggregateStream(GqlQueryError::Source(
            error.map_source(|_| unreachable!("both source-bearing arms handled above")),
        )),
        GqlQueryError::Rows(error) => QueryError::AggregateStream(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => {
            QueryError::AggregateStream(GqlQueryError::Evaluator(error))
        }
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::AggregateStream(GqlQueryError::IdentifiedEdgesRequired)
        }
    }
}

fn build<'q>(
    bound: Bound,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphAggregateRow, Layout> {
    let Bound {
        plan,
        at,
        columns,
        layout,
    } = bound;
    let plan = match plan {
        Plan::Edge(plan) => {
            return edge::build(plan, at, columns, layout, view, cx, execution, policy);
        }
        Plan::Vertex(plan) => plan,
    };
    // Includes exact historical-cut admission even when LIMIT/HAVING returns
    // no rows. The source owns the existing immutable generation, not a copy.
    let inner = view.vertex_scan_source(cx, at).map_err(QueryError::Read)?;
    let source = ScopedSource {
        inner,
        execution: Rc::clone(&execution),
    };
    let control = Rc::clone(&execution);
    let mut cursor = VertexAggregateCursor::new(source, plan, policy, move || {
        control.borrow_mut().checkpoint()
    });
    execution.borrow_mut().checkpoint()?;
    let driver = Box::new(move || {
        let row = cursor.next().transpose().map_err(error)?;
        // This also guards group delivery after the reducer releases its source,
        // plus natural EOF and empty selected pages. Input rows are not delivery.
        execution.borrow_mut().deliver(usize::from(row.is_some()))?;
        let finished = cursor.state() != VertexScanState::Open;
        Ok((row, finished))
    });
    Ok((
        AuthorizedRowCursor {
            driver: Some(driver),
            guard: None,
            columns,
            snapshot_seq: at,
            state: VertexScanState::Open,
        },
        layout,
    ))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Open a vertex- or fixed-edge-rooted aggregate under this session's capability.
    /// Plain, temporal and supported computed-input native definitions retain
    /// grouping, argument DISTINCT, collections, HAVING, output expressions,
    /// DISTINCT, order and pages through the existing physical aggregate plan.
    /// Predicates can use scoped anonymous existence probes. Edge roots and
    /// connected fixed-edge joins additionally admit captured path/edge values
    /// and masked edge properties through the native physical plan. Every
    /// transit endpoint is scoped before traversal. The kernel's canonical
    /// child-order proof is still required for COLLECT. Masking precedes
    /// expressions and grouping, not final delivery.
    ///
    /// Authentication/owner/branch/profile admission happens before source reads.
    /// No candidates are read until next(). The first pull completes the source
    /// and accumulation; it is synchronous, not bounded-duration or cooperative.
    /// Signed node/work use spans opening, all input/probe work and all pulls.
    /// Signed rows count only final groups, never input occurrences/list items.
    /// Native candidate-history/work/scratch/result limits also stay cumulative.
    ///
    /// Each emitted GraphAggregateRow preserves native values and is addressed
    /// by the cursor's RETURN layout. Input failures release no partial groups;
    /// delivery/credential failure can follow earlier complete rows. Expiry,
    /// retirement or host-clock unwind closes the same session as stream().
    ///
    /// Unsupported relational/source-free roots, variable-length outer joins,
    /// optional/nested scopes and unsafe probe payload shapes refuse with no
    /// eager fallback, including LIMIT 0. This borrows the
    /// session and QueryCx, not the writer or prepared template. It remains
    /// thread-local; no Send, external-memory or physical noninterference claim.
    pub fn stream_aggregate<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedAggregateCursor<'q>, QueryError> {
        let (inner, layout) = self.open_cursor(cx, prepared, params, bind_aggregate, build)?;
        Ok(AuthorizedAggregateCursor { inner, layout })
    }
}

#[cfg(test)]
#[path = "aggregate/tests.rs"]
mod tests;
