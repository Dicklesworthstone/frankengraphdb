//! Projected aggregate ordering must reach both public pull-cursor families.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue};
use fgdb_gql::edge_stream::aggregate::{EdgeAggregateCursor, EdgeAggregatePlan};
use fgdb_gql::edge_stream::{
    EdgeExpansionSourceError, EdgeScanRow, EdgeScanSource, EdgeScanSourceError, EdgeScanState,
};
use fgdb_gql::stream::aggregate::{VertexAggregateCursor, VertexAggregatePlan};
use fgdb_gql::stream::{
    VertexScanEvent, VertexScanRow, VertexScanSource, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateRow, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq, EId, VId};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

const G: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
type Record = (VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>);

fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, results, 10_000_000, 10_000_000)
}

fn query(statement: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(statement, |kind, name| match (kind, name) {
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(G)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn records(values: &[(i64, Option<i64>)]) -> Vec<Record> {
    values
        .iter()
        .enumerate()
        .map(|(at, (group, value))| {
            let mut properties = vec![(G, CanonicalScalar::Int(*group).into())];
            if let Some(value) = value {
                properties.push((P, CanonicalScalar::Int(*value).into()));
            }
            (VId(at as u128 + 1), properties)
        })
        .collect()
}

struct Vertices {
    rows: Vec<Record>,
    at: usize,
    fail_at: Option<usize>,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
}
impl Vertices {
    fn new(rows: Vec<Record>) -> Self {
        Self {
            rows,
            at: 0,
            fail_at: None,
            reads: Rc::new(Cell::new(0)),
            dropped: Rc::new(Cell::new(false)),
        }
    }
}
impl Drop for Vertices {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
impl VertexScanSource for Vertices {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.at) {
            return Err(VertexScanSourceError::Source("late vertex source"));
        }
        let next = self.rows.get(self.at).map(|row| row.0);
        if next.is_some() {
            self.at += 1;
        }
        Ok(next)
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.reads.set(self.reads.get() + 1);
        let row = &self.rows[self.at - 1];
        assert_eq!(row.0, id);
        Ok(Some(VertexScanRow {
            labels: &[],
            properties: &row.1,
        }))
    }
}

fn batch_vertices(q: &PreparedGraphAggregate, rows: &[Record]) -> Vec<GraphAggregateRow> {
    q.execute_governed(
        rows.len() as u64,
        rows.iter().map(|row| row.0),
        [],
        |_, _| Ok::<_, ()>(true),
        |id, key| {
            Ok(rows.iter().find(|row| row.0 == id).and_then(|row| {
                row.1
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, value)| value)
            }))
        },
        policy(10_000),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}

fn bucket(row: &GraphAggregateRow) -> i64 {
    match row.values()[0].as_value().and_then(GraphValue::as_scalar) {
        Some(CanonicalScalar::Int(value)) => *value,
        value => panic!("expected bucket, got {value:?}"),
    }
}

#[test]
fn vertex_stream_orders_computed_aliases_and_preserves_exact_aggregate_outputs() {
    let rows = records(&[
        (1, Some(10)),
        (1, Some(10)),
        (1, Some(10)),
        (2, Some(20)),
        (3, None),
    ]);
    for (direction, nulls, expected) in [
        ("DESC", "LAST", [2, 1, 3]),
        ("DESC", "FIRST", [3, 2, 1]),
        ("ASC", "LAST", [1, 2, 3]),
        ("ASC", "FIRST", [3, 1, 2]),
    ] {
        let q = query(&format!(
            "MATCH (n) RETURN n.g AS bucket,-SUM(n.p) AS score,AVG(n.p) AS average \
             ORDER BY score {direction} NULLS {nulls},bucket ASC"
        ));
        assert!(q.ordering().is_empty() && !q.projected_ordering().is_empty());
        let before = q.canonical_bytes();
        let source = Vertices::new(rows.clone());
        let reads = source.reads.clone();
        let dropped = source.dropped.clone();
        let mut cursor = VertexAggregateCursor::new(
            source,
            VertexAggregatePlan::compile(&q).unwrap(),
            policy(3),
            || Ok::<_, ()>(()),
        );
        assert_eq!(cursor.snapshot_seq(), CommitSeq(17));
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual.iter().map(bucket).collect::<Vec<_>>(), expected);
        assert_eq!(actual, batch_vertices(&q, &rows));
        assert!(
            actual
                .iter()
                .all(|row| row.keys().is_empty() && row.values().len() == 3)
        );
        assert_eq!(reads.get(), rows.len());
        assert!(dropped.get());
        assert_eq!(cursor.row_stats().result_rows, 3);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
        assert_eq!(q.canonical_bytes(), before);
    }
}

#[test]
fn vertex_stream_evaluates_private_rank_before_distinct_window_and_limit_zero() {
    let rows = records(&[
        (1, Some(1)),
        (2, Some(1)),
        (2, Some(1)),
        (3, Some(1)),
        (4, Some(1)),
        (4, Some(1)),
        (4, Some(1)),
    ]);
    let q = query(
        "MATCH (n) RETURN DISTINCT count(*) AS c GROUP BY n.g ORDER BY n.g + count(*) DESC SKIP 1 LIMIT 1",
    );
    let mut cursor = VertexAggregateCursor::new(
        Vertices::new(rows.clone()),
        VertexAggregatePlan::compile(&q).unwrap(),
        policy(1),
        || Ok::<_, ()>(()),
    );
    let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(actual, batch_vertices(&q, &rows));
    assert_eq!(actual.len(), 1);
    assert_eq!(actual[0].values()[0].as_count(), Some(2));
    assert!(actual[0].keys().is_empty() && actual[0].values().len() == 1);
    for page in ["LIMIT 0", "LIMIT 1"] {
        let q = query(&format!(
            "MATCH (n) RETURN count(*) AS c GROUP BY n.g ORDER BY 1/(n.g-4) {page}"
        ));
        let mut cursor = VertexAggregateCursor::new(
            Vertices::new(rows.clone()),
            VertexAggregatePlan::compile(&q).unwrap(),
            policy(1),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn cooperative_vertex_polling_keeps_projected_distinct_window_and_quotas() {
    let values: Vec<_> = (0..130).map(|at| (at % 5, Some(at % 5 + 1))).collect();
    let rows = records(&values);
    let q = query(
        "MATCH (n) RETURN DISTINCT -SUM(n.p) AS score GROUP BY n.g \
        ORDER BY score DESC SKIP 1 LIMIT 2",
    );
    let source = Vertices::new(rows.clone());
    let dropped = source.dropped.clone();
    let mut cursor = VertexAggregateCursor::new(
        source,
        VertexAggregatePlan::compile(&q).unwrap(),
        policy(2),
        || Ok::<_, ()>(()),
    );
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(cursor.poll_next(&mut context), Poll::Pending));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(!dropped.get());
    let mut actual = Vec::new();
    let mut complete = false;
    for _ in 0..20 {
        match cursor.poll_next(&mut context) {
            Poll::Pending => assert_eq!(cursor.row_stats().result_rows, 0),
            Poll::Ready(Some(row)) => actual.push(row.unwrap()),
            Poll::Ready(None) => {
                complete = true;
                break;
            }
        }
    }
    assert!(complete);
    assert_eq!(actual, batch_vertices(&q, &rows));
    assert_eq!(
        actual
            .iter()
            .map(|row| row.values()[0].as_integer().unwrap())
            .collect::<Vec<_>>(),
        [-52, -78]
    );
    assert_eq!(cursor.row_stats().result_rows, 2);
    assert!(dropped.get());
    assert!(matches!(cursor.poll_next(&mut context), Poll::Ready(None)));
}

#[test]
fn vertex_projected_order_keeps_late_errors_cancellation_and_budget_failures_terminal() {
    let rows = records(&[(1, Some(5)), (1, Some(5)), (2, Some(7))]);
    for count in [0, 1] {
        let q = query(&format!(
            "MATCH (n) RETURN n.g AS bucket,1/(COUNT(*)-1) AS score \
            ORDER BY score LIMIT {count}"
        ));
        let source = Vertices::new(rows.clone());
        let dropped = source.dropped.clone();
        let mut cursor = VertexAggregateCursor::new(
            source,
            VertexAggregatePlan::compile(&q).unwrap(),
            policy(10),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    let q = query("MATCH (n) RETURN n.g AS bucket,-SUM(n.p) AS score ORDER BY score LIMIT 1");
    let mut source = Vertices::new(rows.clone());
    source.fail_at = Some(2);
    let dropped = source.dropped.clone();
    let mut cursor = VertexAggregateCursor::new(
        source,
        VertexAggregatePlan::compile(&q).unwrap(),
        policy(10),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(dropped.get() && cursor.next().is_none());
    let source = Vertices::new(rows.clone());
    let dropped = source.dropped.clone();
    let mut cancelled = VertexAggregateCursor::new(
        source,
        VertexAggregatePlan::compile(&q).unwrap(),
        policy(10),
        || Err::<(), _>("cancel"),
    );
    assert!(matches!(
        cancelled.next(),
        Some(Err(GqlQueryError::Interrupted("cancel")))
    ));
    assert!(dropped.get() && cancelled.next().is_none());
    for p in [policy(0), GqlQueryPolicy::new(100, 10, 0, 1_000_000)] {
        let source = Vertices::new(rows.clone());
        let dropped = source.dropped.clone();
        let mut limited = VertexAggregateCursor::new(
            source,
            VertexAggregatePlan::compile(&q).unwrap(),
            p,
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            limited.next(),
            Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))
        ));
        assert_eq!(limited.row_stats().result_rows, 0);
        assert_eq!(limited.state(), VertexScanState::Failed);
        assert!(dropped.get() && limited.next().is_none());
    }
}

struct Edges {
    vertices: BTreeSet<VId>,
    edges: BTreeMap<EId, (VId, RelationId, VId)>,
    after: Option<EId>,
    fail: Option<EId>,
    dropped: Rc<Cell<bool>>,
}
impl Edges {
    fn new() -> Self {
        let owners = [VId(1), VId(2), VId(u128::MAX)];
        let mut edges = BTreeMap::new();
        for (at, owner) in [owners[0], owners[0], owners[0], owners[1], owners[2]]
            .into_iter()
            .enumerate()
        {
            edges.insert(EId(at as u128 + 1), (owner, R, VId(0)));
        }
        edges.insert(EId(6), (VId(0), S, VId(3)));
        edges.insert(EId(7), (VId(0), S, VId(4)));
        Self {
            vertices: [VId(0), owners[0], owners[1], owners[2], VId(3), VId(4)].into(),
            edges,
            after: None,
            fail: None,
            dropped: Rc::new(Cell::new(false)),
        }
    }
}
impl Drop for Edges {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
impl EdgeScanSource for Edges {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if let Some(id) = next {
            self.after = Some(id);
        }
        Ok(next)
    }
    fn edge<C>(
        &self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail == Some(id) {
            return Err(EdgeScanSourceError::Source("late edge source"));
        }
        Ok(self
            .edges
            .get(&id)
            .map(|&(source, relation, target)| EdgeScanRow {
                source,
                relation,
                target,
                properties: &[],
            }))
    }
    fn vertex<C>(
        &self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.contains(&id).then_some(VertexScanRow {
            labels: &[],
            properties: &[],
        }))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        for (&id, &(source, _, target)) in self
            .edges
            .range((after.map_or(Unbounded, Excluded), Unbounded))
        {
            control(GlaExecutionEvent::Work).map_err(|error| {
                EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error))
            })?;
            let matches = match direction {
                GlaDirection::Forward => source == endpoint,
                GlaDirection::Reverse => target == endpoint,
                GlaDirection::Undirected => source == endpoint || target == endpoint,
            };
            if matches {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

fn batch_edges(q: &PreparedGraphAggregate, source: &Edges) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        (source.vertices.len() + source.edges.len()) as u64,
        source.vertices.iter().copied(),
        source.edges.iter().map(|(&id, &(a, r, b))| (id, a, r, b)),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        |_, _| Ok(None),
        policy(10_000),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}

#[test]
fn fixed_edge_and_join_streams_rank_computed_counts_and_full_width_identities() {
    for (pattern, score) in [("(a)-[e:R]->(b)", -1), ("(a)-[e:R]->(b)-[f:S]->(c)", -2)] {
        let q = query(&format!(
            "MATCH {pattern} RETURN a AS owner,-COUNT(*) AS score \
            GROUP BY a ORDER BY score DESC,owner DESC LIMIT 2"
        ));
        let source = Edges::new();
        let expected = batch_edges(&q, &source);
        let dropped = source.dropped.clone();
        let mut cursor = EdgeAggregateCursor::new(
            source,
            EdgeAggregatePlan::compile(&q).unwrap(),
            policy(2),
            || Ok::<_, ()>(()),
        );
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows, expected);
        assert_eq!(
            rows.iter()
                .map(|row| {
                    row.values()[0]
                        .as_value()
                        .and_then(GraphValue::as_vertex)
                        .unwrap()
                })
                .collect::<Vec<_>>(),
            [VId(u128::MAX), VId(2)]
        );
        assert!(
            rows.iter()
                .all(|row| row.keys().is_empty() && row.values()[1].as_integer() == Some(score))
        );
        assert_eq!(cursor.row_stats().result_rows, 2);
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        assert!(dropped.get() && cursor.next().is_none());
    }
}

#[test]
fn edge_and_join_streams_keep_private_count_ranks_out_of_visible_rows() {
    for pattern in ["(a)-[e:R]->(b)", "(a)-[e:R]->(b)-[f:S]->(c)"] {
        let q = query(&format!(
            "MATCH {pattern} RETURN a AS owner,COUNT(*) AS count ORDER BY count + 1 DESC,owner DESC LIMIT 2"
        ));
        let expected = batch_edges(&q, &Edges::new());
        let mut cursor = EdgeAggregateCursor::new(
            Edges::new(),
            EdgeAggregatePlan::compile(&q).unwrap(),
            policy(2),
            || Ok::<_, ()>(()),
        );
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected);
        assert!(actual.iter().all(|row| row.keys().is_empty()
            && row.values().len() == 2
            && row.values()[1].as_count().is_some()));
        assert_eq!(cursor.row_stats().result_rows, 2);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn edge_projected_order_preserves_late_source_expression_and_result_quota_failures() {
    let q = query(
        "MATCH (a)-[e:R]->(b) RETURN a AS owner,-COUNT(*) AS score \
        GROUP BY a ORDER BY score DESC LIMIT 1",
    );
    let mut source = Edges::new();
    source.fail = Some(EId(5));
    let dropped = source.dropped.clone();
    let mut cursor = EdgeAggregateCursor::new(
        source,
        EdgeAggregatePlan::compile(&q).unwrap(),
        policy(10),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(_))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(dropped.get() && cursor.next().is_none());
    for count in [0, 1] {
        let q = query(&format!(
            "MATCH (a)-[e:R]->(b) RETURN a AS owner,1/(COUNT(*)-1) AS score \
            GROUP BY a ORDER BY score LIMIT {count}"
        ));
        let source = Edges::new();
        let dropped = source.dropped.clone();
        let mut cursor = EdgeAggregateCursor::new(
            source,
            EdgeAggregatePlan::compile(&q).unwrap(),
            policy(10),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(dropped.get() && cursor.next().is_none());
    }
    let source = Edges::new();
    let dropped = source.dropped.clone();
    let mut limited = EdgeAggregateCursor::new(
        source,
        EdgeAggregatePlan::compile(&q).unwrap(),
        policy(0),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(limited.next(), Some(Err(GqlQueryError::Rows(_)))));
    assert_eq!(limited.state(), EdgeScanState::Failed);
    assert_eq!(limited.row_stats().result_rows, 0);
    assert!(dropped.get() && limited.next().is_none());
}
