use super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::LabelId;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn prepared(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn statement(direction: GlaDirection, predicate: &str, all: bool, skip: u64, limit: u64) -> String {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    format!(
        "MATCH (a){left}[r:R]{right}(b) {predicate} RETURN {}r, a, b, r.p AS ep, a.p AS ap, b.p AS bp SKIP {skip} LIMIT {limit}",
        if all { "ALL " } else { "DISTINCT " }
    )
}
type EdgeFixtures = BTreeMap<EId, (VId, RelationId, VId, Vec<(PropertyKeyId, CanonicalScalar)>)>;
type VertexFixtures = BTreeMap<VId, (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;
#[derive(Clone)]
struct Fixture {
    ids: Vec<EId>,
    at: usize,
    edges: EdgeFixtures,
    vertices: VertexFixtures,
    fail: Option<EId>,
    drops: Arc<AtomicUsize>,
    visits: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Fixture {
    fn new() -> Self {
        let vertices = BTreeMap::from([
            (
                IDS[0],
                (vec![LabelId(1)], vec![(P, CanonicalScalar::Int(4))]),
            ),
            (IDS[1], (vec![], vec![(P, CanonicalScalar::Null)])),
            (
                IDS[2],
                (vec![LabelId(1)], vec![(P, CanonicalScalar::Int(1))]),
            ),
        ]);
        let mut edges = BTreeMap::new();
        for (id, src, rel, dst, value) in [
            (0, 0, R, 1, Some(CanonicalScalar::Int(2))),
            (1, 2, R, 0, Some(CanonicalScalar::Int(-1))),
            (2, 2, R, 0, Some(CanonicalScalar::Int(5))), // parallel, not a duplicate
            (3, 1, R, 1, None),                          // self loop, only one orientation
            (4, 0, RelationId(8), 2, Some(CanonicalScalar::Int(99))),
            (u128::MAX, 1, R, 2, Some(CanonicalScalar::Null)),
        ] {
            edges.insert(
                EId(id),
                (
                    IDS[src],
                    rel,
                    IDS[dst],
                    value.map(|v| vec![(P, v)]).unwrap_or_default(),
                ),
            );
        }
        Self {
            ids: edges.keys().copied().collect(),
            at: 0,
            edges,
            vertices,
            fail: None,
            drops: Arc::new(AtomicUsize::new(0)),
            visits: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn value(&self, vertex: VId) -> Option<&CanonicalScalar> {
        self.vertices[&vertex].1.first().map(|(_, value)| value)
    }
}
impl EdgeScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(11)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let id = self.ids.get(self.at).copied();
        self.at += usize::from(id.is_some());
        Ok(id)
    }
    fn edge<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.visits.fetch_add(1, Ordering::SeqCst);
        if self.fail == Some(id) {
            return Err(EdgeScanSourceError::Source("unreadable edge"));
        }
        Ok(self
            .edges
            .get(&id)
            .map(|(src, rel, dst, props)| EdgeScanRow {
                source: *src,
                relation: *rel,
                target: *dst,
                properties: props,
            }))
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.get(&id).map(|(labels, props)| VertexScanRow {
            labels,
            properties: props,
        }))
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 100_000, 100_000)
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}
// Independent row oracle: enumerate the finite edge relation, orient, filter
// complete rows, sort and paginate. It does not compile/interpret any GLA.
fn oracle(
    f: &Fixture,
    direction: GlaDirection,
    filter: usize,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphValue>> {
    let scalar = |value: Option<&CanonicalScalar>| {
        GraphValue::Scalar(value.cloned().unwrap_or(CanonicalScalar::Null))
    };
    let mut rows = Vec::new();
    for (&id, (src, rel, dst, props)) in &f.edges {
        if *rel != R {
            continue;
        }
        let endpoints = match direction {
            GlaDirection::Forward => vec![(*src, *dst)],
            GlaDirection::Reverse => vec![(*dst, *src)],
            GlaDirection::Undirected if src == dst => vec![(*src, *dst)],
            _ => vec![(*src, *dst), (*dst, *src)],
        };
        let edge_value = props.first().map(|(_, v)| v);
        for (a, b) in endpoints {
            let accept = match filter {
                0 => true,
                1 => matches!(edge_value, Some(CanonicalScalar::Int(n)) if *n > 0),
                2 => edge_value.is_none_or(|v| matches!(v, CanonicalScalar::Null)),
                3 => a != b,
                _ => {
                    matches!((edge_value, f.value(a)), (Some(CanonicalScalar::Int(x)), Some(CanonicalScalar::Int(y))) if x > y)
                }
            };
            if accept {
                rows.push(vec![
                    GraphValue::Edge(id),
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                    scalar(edge_value),
                    scalar(f.value(a)),
                    scalar(f.value(b)),
                ]);
            }
        }
    }
    rows.sort();
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn directed_reverse_undirected_parallel_null_and_predicate_results_match_independent_and_eager_oracles()
 {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for (filter, predicate) in [
            "",
            "WHERE r.p > 0",
            "WHERE r.p IS NULL",
            "WHERE a <> b",
            "WHERE r.p > a.p",
        ]
        .iter()
        .enumerate()
        {
            for all in [false, true] {
                for (skip, limit) in [(0, 100), (1, 2), (2, 0), (99, 4)] {
                    let pattern = prepared(&statement(direction, predicate, all, skip, limit));
                    let identity = pattern.plan().canonical_bytes();
                    let plan = EdgeScanPlan::compile(pattern.plan()).unwrap();
                    let fixture = Fixture::new();
                    let expected =
                        oracle(&fixture, direction, filter, skip as usize, limit as usize);
                    let eager = pattern
                        .plan()
                        .execute_governed_with_element_properties(
                            6,
                            fixture.vertices.keys().copied(),
                            fixture
                                .edges
                                .iter()
                                .map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
                            |id, predicates| {
                                Ok::<_, ()>(predicates.iter().all(|p| {
                                    p.matches(&fixture.vertices[&id].0, &fixture.vertices[&id].1)
                                }))
                            },
                            |id, _| Ok(fixture.value(id)),
                            |id, _| Ok(fixture.edges[&id].3.first().map(|(_, v)| v)),
                            policy(),
                            || Ok::<_, ()>(()),
                        )
                        .unwrap();
                    assert_eq!(plain(&eager.value), expected);
                    let mut cursor =
                        EdgeScanCursor::new(fixture, plan, policy(), || Ok::<_, ()>(()));
                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(plain(&rows), expected);
                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                    assert_eq!(pattern.plan().canonical_bytes(), identity);
                    assert!(cursor.next().is_none());
                }
            }
        }
    }
}

#[test]
fn unread_failing_suffix_zero_limit_and_early_close_do_not_drive_the_source() {
    for limit in [0, 1, 100] {
        let pattern = prepared(&statement(GlaDirection::Forward, "", true, 0, limit));
        let mut source = Fixture::new();
        source.fail = Some(EId(1));
        let visits = source.visits.clone();
        let drops = source.drops.clone();
        let mut cursor = EdgeScanCursor::new(
            source,
            EdgeScanPlan::compile(pattern.plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(visits.load(Ordering::SeqCst), 0);
        if limit != 0 {
            cursor.next().unwrap().unwrap();
        }
        if limit == 100 {
            cursor.close();
        }
        assert!(cursor.next().is_none());
        assert_eq!(visits.load(Ordering::SeqCst), usize::from(limit != 0));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        cursor.close();
    }
}

#[test]
fn exact_cumulative_limits_and_every_checkpoint_preserve_delivered_prefix_and_fuse() {
    let pattern = prepared(&statement(
        GlaDirection::Undirected,
        "WHERE r.p IS NULL OR r.p > 0",
        true,
        0,
        100,
    ));
    let plan = EdgeScanPlan::compile(pattern.plan()).unwrap();
    let checkpoints = std::cell::Cell::new(0);
    let mut baseline = EdgeScanCursor::new(Fixture::new(), plan.clone(), policy(), || {
        checkpoints.set(checkpoints.get() + 1);
        Ok::<_, usize>(())
    });
    let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = baseline.row_stats();
    let e = baseline.evaluator_stats();
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    let mut retry = EdgeScanCursor::new(Fixture::new(), plan.clone(), exact, || Ok::<_, usize>(()));
    assert_eq!(
        retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    assert_eq!(retry.evaluator_stats(), e);
    for stop in 1..=checkpoints.get() {
        let calls = std::cell::Cell::new(0);
        let fixture = Fixture::new();
        let drops = fixture.drops.clone();
        let mut cursor = EdgeScanCursor::new(fixture, plan.clone(), exact, || {
            calls.set(calls.get() + 1);
            if calls.get() == stop {
                Err(stop)
            } else {
                Ok(())
            }
        });
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => {
                    assert_eq!(at, stop);
                    break;
                }
                other => panic!("missed interruption {stop}: {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(calls.get(), stop);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = EdgeScanCursor::new(Fixture::new(), plan.clone(), p, || Ok::<_, ()>(()));
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn invalid_sources_are_errors_not_missing_rows_and_debug_is_redacted() {
    let pattern = prepared(&statement(GlaDirection::Forward, "", true, 0, 100));
    let plan = EdgeScanPlan::compile(pattern.plan()).unwrap();
    for kind in 0..3 {
        let mut fixture = Fixture::new();
        match kind {
            0 => fixture.ids.insert(1, EId(0)),
            1 => {
                fixture.vertices.remove(&IDS[1]);
            }
            _ => fixture.fail = Some(EId(1)),
        }
        let mut cursor = EdgeScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
        let error = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap_err();
        match (kind, error) {
            (0, GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity))
            | (1, GqlQueryError::Source(EdgeScanError::DanglingEndpoint))
            | (2, GqlQueryError::Source(EdgeScanError::Source("unreadable edge"))) => {}
            other => panic!("source failure changed category: {other:?}"),
        }
        assert!(cursor.next().is_none());
        assert!(!format!("{cursor:?}").contains(&u128::MAX.to_string()));
    }
}

#[test]
fn unsupported_shape_or_non_streamable_order_is_refused_instead_of_reordered() {
    for statement in [
        "MATCH (a)-[r:R]->(b) RETURN a, r, b",
        "MATCH (a)-[r:R]->(b) RETURN r.p",
        "MATCH (a)-[r:R]->(b) RETURN r, a, b ORDER BY b",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, c",
        "MATCH (a) RETURN a",
    ] {
        assert!(
            EdgeScanPlan::compile(prepared(statement).plan()).is_err(),
            "accepted {statement}"
        );
    }
}
