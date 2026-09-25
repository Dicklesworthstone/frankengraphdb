//! Scoped fixed-edge/join inputs shared by native row and aggregate cursors.
//! Both readers own the same already-admitted generation and selected cut.
//! Root enumeration owns one EId position; probes keep their own positions.

use super::*;
use fgdb_gql::edge_stream::aggregate::{EdgeAggregateCursor, EdgeAggregateError, EdgeAggregatePlan};
use fgdb_gql::edge_stream::{
    EdgeScanCursor, EdgeScanPlan, EdgeScanRecord, EdgeScanSource, EdgeScanSourceError, EdgeScanState,
};

struct Source<'q, Edges, Vertices> {
    edges: Edges,
    vertices: ScopedSource<'q, Vertices>,
    after: Option<EId>,
}

// Raw index/history work polls cancellation and retained failures, not charged by
// hidden population or history size. A refused poll is a source
// failure here, not the generic caller control. Never turn it into absence.
fn private_error<C>(
    error: EdgeScanSourceError<ReadError, QueryError>,
) -> EdgeScanSourceError<QueryError, C> {
    EdgeScanSourceError::Source(match error {
        EdgeScanSourceError::Source(error) => QueryError::Read(error),
        EdgeScanSourceError::Control(error) => error,
    })
}

impl<E, V> Source<'_, E, V>
where
    E: EdgeScanSource<Error = ReadError>,
    V: VertexScanSource<Error = ReadError>,
{
    fn poll<C>(&self) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        self.vertices.execution.borrow_mut().poll().map_err(EdgeScanSourceError::Source)
    }

    /// Resolve the complete winning topology BEFORE scoping it. This private
    /// preflight lends no payload to a predicate and charges no hidden records.
    /// The public source routes charge only once the edge and both endpoints
    /// are admitted. Vertex records/fields then use the existing scoped route.
    fn visible<'a, C>(
        &'a self,
        eid: EId,
        requested: Option<RelationId>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<QueryError, C>> {
        self.poll()?;
        let execution = &self.vertices.execution;
        let mut poll = |_: GlaExecutionEvent| execution.borrow_mut().poll();
        let Some(edge) = self.edges.edge(eid, &mut poll).map_err(private_error)? else {
            return Ok(None);
        };
        if requested.is_some_and(|relation| edge.relation != relation)
            || !execution.borrow().permit.predicates().allows_relation(edge.relation)
        {
            return Ok(None);
        }
        for endpoint in [Some(edge.source), (edge.target != edge.source).then_some(edge.target)]
            .into_iter().flatten()
        {
            let Some(vertex) = self.edges.vertex(endpoint, &mut poll).map_err(private_error)? else {
                return Ok(None);
            };
            if !execution.borrow().permit.predicates().allows_vertex(vertex.labels) {
                return Ok(None);
            }
        }
        self.poll()?;
        Ok(Some(edge))
    }
}

impl<E, V> EdgeScanSource for Source<'_, E, V>
where
    E: EdgeScanSource<Error = ReadError>,
    V: VertexScanSource<Error = ReadError>,
{
    type Error = QueryError;
    fn snapshot_seq(&self) -> CommitSeq { self.edges.snapshot_seq() }

    fn next_edge<C>(
        &mut self,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<QueryError, C>> {
        Err(EdgeScanSourceError::Source(QueryError::Unsupported {
            diagnostics: vec!["scoped edge enumeration requires its bound relation".to_owned()],
        }))
    }

    fn next_edge_for_relation<C>(
        &mut self,
        relation: RelationId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<QueryError, C>> {
        self.poll()?;
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if !self.vertices.execution.borrow().permit.predicates().allows_relation(relation) {
            return Ok(None); // No candidate directory, even for a nonempty raw graph.
        }
        loop {
            let execution = &self.vertices.execution;
            let mut poll = |_: GlaExecutionEvent| execution.borrow_mut().poll();
            let next = self.edges.next_edge_for_relation(relation, &mut poll).map_err(private_error)?;
            let Some(eid) = next else { return Ok(None) };
            // Validate raw positions too: a repeated hidden identity must not
            // become an endless uncharged scan or be hidden from the caller.
            if self.after.is_some_and(|after| eid <= after) {
                return Err(EdgeScanSourceError::Source(source_error(EdgeScanError::NonIncreasingIdentity)));
            }
            self.after = Some(eid);
            if self.visible::<C>(eid, Some(relation))?.is_some() {
                control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
                return Ok(Some(eid));
            }
        }
    }

    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<QueryError, C>> {
        let Some(edge) = self.visible(eid, None)? else { return Ok(None) };
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        // The matching kernel needs topology; it obtains fields only through
        // the separately scoped record/property routes below.
        Ok(Some(EdgeScanRow { properties: &[], ..edge }))
    }

    fn edge_record<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRecord<'a>>, EdgeScanSourceError<QueryError, C>> {
        let Some(edge) = self.visible(eid, None)? else { return Ok(None) };
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        EdgeScanRecord::copy_masked(edge, |key| {
            self.vertices.execution.borrow().permit.predicates().allows_property(key)
        }, control).map(Some).map_err(EdgeScanSourceError::Control)
    }

    fn edge_property<'a, C>(
        &'a self,
        eid: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<QueryError, C>> {
        let Some(edge) = self.visible(eid, None)? else { return Ok(None) };
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if !self.vertices.execution.borrow().permit.predicates().allows_property(key) {
            return Ok(Some(None)); // Do not resolve or copy a masked field.
        }
        // Search the admitted field sequence. Hidden key count must not alter
        // the logical work transcript; this is not a CPU/timing isolation claim.
        for (candidate, value) in edge.properties {
            if !self.vertices.execution.borrow().permit.predicates().allows_property(*candidate) {
                continue;
            }
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            match candidate.cmp(&key) {
                core::cmp::Ordering::Equal => return Ok(Some(Some(value))),
                core::cmp::Ordering::Greater => break,
                core::cmp::Ordering::Less => {}
            }
        }
        Ok(Some(None))
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<QueryError, C>> {
        // The existing source refuses to lend raw metadata. Preserve that
        // refusal even if a future kernel accidentally requests this route.
        self.vertices.vertex(vid, &mut |event| control(edge_event(event)))
    }
    fn vertex_record<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, EdgeScanSourceError<QueryError, C>> {
        self.vertices.vertex_record(vid, &mut |event| control(edge_event(event)))
    }
    fn vertex_property<'a, C>(
        &'a self,
        vid: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<QueryError, C>> {
        self.vertices.vertex_property(vid, key, &mut |event| control(edge_event(event)))
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<QueryError, C>> {
        self.vertices.next_probe_vertex(after, control)
    }
    fn next_incident_edge_for_relation<C>(
        &self,
        endpoint: VId,
        relation: RelationId,
        direction: fgdb_gql::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<QueryError, C>> {
        self.vertices.next_probe_edge_for_relation(endpoint, relation, direction, after, control)
    }
}

fn source_error(error: EdgeScanError<QueryError>) -> QueryError {
    let error = match error {
        EdgeScanError::Source(error) => return error,
        EdgeScanError::Plan(error) => EdgeScanError::Plan(error),
        EdgeScanError::NonIncreasingIdentity => EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::DanglingEndpoint => EdgeScanError::DanglingEndpoint,
        EdgeScanError::CounterExhausted => EdgeScanError::CounterExhausted,
        EdgeScanError::ExpansionUnavailable => EdgeScanError::ExpansionUnavailable,
        EdgeScanError::BoundEdgeUnavailable => EdgeScanError::BoundEdgeUnavailable,
    };
    QueryError::EdgeStream(GqlQueryError::Source(error))
}
fn error(error: EdgeAggregateError<QueryError, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(GraphAggregateError::Source(error)) => source_error(error),
        GqlQueryError::Source(GraphAggregateError::InputRelation(GraphSetExecutionError::Source(error))) => {
            source_error(error)
        }
        GqlQueryError::Source(GraphAggregateError::InputRelation(error)) => {
            QueryError::EdgeAggregateStream(GqlQueryError::Source(GraphAggregateError::InputRelation(
                error.map_source(|_| unreachable!("source arm handled above")),
            )))
        }
        GqlQueryError::Source(error) => QueryError::EdgeAggregateStream(GqlQueryError::Source(
            error.map_source(|_| unreachable!("source arms handled above")),
        )),
        GqlQueryError::Rows(error) => QueryError::EdgeAggregateStream(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::EdgeAggregateStream(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => QueryError::EdgeAggregateStream(GqlQueryError::IdentifiedEdgesRequired),
    }
}

fn row_error(error: GqlQueryError<EdgeScanError<QueryError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => source_error(error),
        GqlQueryError::Rows(error) => QueryError::EdgeStream(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::EdgeStream(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::EdgeStream(GqlQueryError::IdentifiedEdgesRequired)
        }
    }
}

impl<'q> AuthorizedRowCursor<'q> {
    // An inherent factory lets the enclosing stream dispatcher reuse this
    // private source without exporting it or changing the aggregate interface.
    // Source and permit ownership never escape to the calling application.
    pub(in super::super) fn from_edge_plan(
        selected: (EdgeScanPlan, CommitSeq, Vec<String>),
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        execution: Shared<'q>,
        policy: GqlQueryPolicy,
    ) -> Opened<'q, GraphValueRow, ()> {
        let (plan, at, columns) = selected;
        let edges = view.edge_scan_source(cx, at).map_err(QueryError::Read)?;
        let inner = view.vertex_scan_source(cx, at).map_err(QueryError::Read)?;
        let source = Source {
            edges,
            vertices: ScopedSource {
                inner,
                execution: Rc::clone(&execution),
            },
            after: None,
        };
        let control = Rc::clone(&execution);
        let mut cursor = EdgeScanCursor::new(source, plan, policy, move || {
            control.borrow_mut().checkpoint()
        });
        execution.borrow_mut().checkpoint()?;
        let driver = Box::new(move || {
            let row = cursor.next().transpose().map_err(row_error)?;
            // Delivery is charged only after matching, projection and native
            // admission. Natural EOF and LIMIT 0 still recheck live authority.
            execution.borrow_mut().deliver(usize::from(row.is_some()))?;
            Ok((row, cursor.state() != EdgeScanState::Open))
        });
        Ok((
            AuthorizedRowCursor {
                driver: Some(driver),
                guard: None,
                columns,
                snapshot_seq: at,
                state: VertexScanState::Open,
            },
            (),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build<'q>(
    plan: EdgeAggregatePlan, at: CommitSeq, columns: Vec<String>, layout: Layout,
    view: &EmbeddedReadView, cx: &'q QueryCx, execution: Shared<'q>, policy: GqlQueryPolicy,
) -> Opened<'q, GraphAggregateRow, Layout> {
    // Both factories borrow the SAME view and cut; neither can refresh from a
    // writer. Constructing these cheap pin owners scans no graph candidates.
    let edges = view.edge_scan_source(cx, at).map_err(QueryError::Read)?;
    let inner = view.vertex_scan_source(cx, at).map_err(QueryError::Read)?;
    let source = Source {
        edges, vertices: ScopedSource { inner, execution: Rc::clone(&execution) }, after: None,
    };
    let control = Rc::clone(&execution);
    let mut cursor = EdgeAggregateCursor::new(source, plan, policy, move || {
        control.borrow_mut().checkpoint()
    });
    execution.borrow_mut().checkpoint()?;
    let driver = Box::new(move || {
        let row = cursor.next().transpose().map_err(error)?;
        execution.borrow_mut().deliver(usize::from(row.is_some()))?;
        Ok((row, cursor.state() != EdgeScanState::Open))
    });
    Ok((AuthorizedRowCursor {
        driver: Some(driver), guard: None, columns, snapshot_seq: at, state: VertexScanState::Open,
    }, layout))
}

#[cfg(test)]
#[path = "edge/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "edge/row_tests.rs"]
mod row_tests;
