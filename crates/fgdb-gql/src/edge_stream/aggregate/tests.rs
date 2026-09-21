use super::*;
mod groups;
mod grouped;
mod output;
mod computed;
use crate::algebra::GraphValue;
use crate::{
    GqlParameters, GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId, Vec<(PropertyKeyId, CanonicalScalar)>);
#[derive(Clone)]
struct Source {
    vertices: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>,
    edges: BTreeMap<EId, Edge>,
    incident: BTreeMap<(VId, u8), BTreeSet<EId>>,
    after: Option<EId>,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    fail: Option<EId>,
    unavailable: bool,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let next = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(id, _)| *id);
        if let Some(id) = next {
            self.after = Some(id);
        }
        Ok(next)
    }
    fn edge<C>(
        &self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge unavailable"));
        }
        Ok(self
            .edges
            .get(&eid)
            .map(|(source, relation, target, properties)| EdgeScanRow {
                source: *source,
                target: *target,
                relation: *relation,
                properties,
            }))
    }
    fn vertex<C>(
        &self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow {
            labels: &[],
            properties,
        }))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        let dir = match direction {
            GlaDirection::Forward => 0,
            GlaDirection::Reverse => 1,
            GlaDirection::Undirected => 2,
        };
        Ok(self.incident.get(&(endpoint, dir)).and_then(|ids| {
            ids.range((after.map_or(Unbounded, Excluded), Unbounded))
                .next()
                .copied()
        }))
    }
}
fn source(mask: u32) -> Source {
    let ids = [VId(0), VId(1), VId(u128::MAX)];
    let vertices = BTreeMap::from([
        (ids[0], vec![(P, CanonicalScalar::Int(-3))]),
        (ids[1], vec![(P, CanonicalScalar::Null)]),
        (ids[2], vec![(P, CanonicalScalar::Int(7))]),
    ]);
    let mut edges = BTreeMap::new();
    let mut incident: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    let raw = [
        (0, R, 1, Some(5)),
        (0, R, 1, None),
        (1, S, 2, Some(-2)),
        (2, R, 0, Some(1)),
        (1, S, 1, None),
        (0, R, 0, Some(-4)),
    ];
    for (at, (a, rel, b, weight)) in raw.into_iter().enumerate() {
        if mask & (1 << at) == 0 {
            continue;
        }
        let eid = EId(at as u128 + 1);
        let from = ids[a];
        let to = ids[b];
        let properties = weight
            .map(|w| vec![(P, CanonicalScalar::Int(w))])
            .unwrap_or_default();
        edges.insert(eid, (from, rel, to, properties));
        for key in [(from, 0), (to, 1), (from, 2), (to, 2)] {
            incident.entry(key).or_default().insert(eid);
        }
    }
    Source {
        vertices,
        edges,
        incident,
        after: None,
        reads: Arc::new(AtomicUsize::new(0)),
        drops: Arc::new(AtomicUsize::new(0)),
        fail: None,
        unavailable: false,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name: &str| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(if name == "S" { S } else { R })),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1, 10_000_000, 10_000_000)
}
fn run(
    q: &PreparedGraphAggregate,
    s: Source,
    p: GqlQueryPolicy,
) -> EdgeAggregateCursor<Source, impl FnMut() -> Result<(), ()>> {
    EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(q).unwrap(), p, || Ok(()))
}
fn integer(props: &[(PropertyKeyId, CanonicalScalar)]) -> Option<i128> {
    props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == P => Some(i128::from(*value)),
        _ => None,
    })
}
fn sum(value: Option<i128>) -> GraphAggregateValue {
    value.map_or_else(
        || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        GraphAggregateValue::Integer,
    )
}
// Enumerate complete VId assignments independently, then count every edge
// choice for each atom. No production cursor, GLA slot or prefix pruning.
fn oracle(s: &Source, shape: usize, dir: GlaDirection) -> Vec<GraphAggregateValue> {
    let atoms = [(0, R, 1), (1, S, 2), (2, R, 0)];
    let width = if shape == 0 { 2 } else { 3 };
    let domain: Vec<_> = s.vertices.keys().copied().collect();
    let mut n = 0;
    let mut present = 0;
    let mut edge_sum = None;
    let mut vertex_sum = None;
    for mut code in 0..domain.len().pow(width) {
        let mut values = Vec::new();
        for _ in 0..width {
            values.push(domain[code % domain.len()]);
            code /= domain.len();
        }
        let mut choices = Vec::new();
        for &(from, rel, to) in &atoms[..=shape] {
            choices.push(
                s.edges
                    .values()
                    .filter(|(a, r, b, _)| {
                        *r == rel
                            && match dir {
                                GlaDirection::Forward => *a == values[from] && *b == values[to],
                                GlaDirection::Reverse => *b == values[from] && *a == values[to],
                                GlaDirection::Undirected => {
                                    (*a == values[from] && *b == values[to])
                                        || (*b == values[from] && *a == values[to])
                                }
                            }
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let count = choices.iter().map(|c| c.len() as u64).product::<u64>();
        if count == 0 {
            continue;
        }
        n += count;
        let suffix = choices
            .iter()
            .skip(1)
            .map(|c| c.len() as u64)
            .product::<u64>();
        for edge in &choices[0] {
            if let Some(value) = integer(&edge.3) {
                present += suffix;
                edge_sum = Some(edge_sum.unwrap_or(0) + value * i128::from(suffix));
            }
        }
        if let Some(value) = integer(&s.vertices[&values[width as usize - 1]]) {
            vertex_sum = Some(vertex_sum.unwrap_or(0) + value * i128::from(count));
        }
    }
    vec![
        GraphAggregateValue::Count(n),
        GraphAggregateValue::Count(present),
        sum(edge_sum),
        sum(vertex_sum),
    ]
}

#[test]
fn global_edge_aggregates_match_full_assignment_oracle_and_eager_results() {
    for mask in 0..64 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let edge = |name: &str, rel: &str, end: &str| match direction {
                GlaDirection::Forward => format!("-[{name}:{rel}]->({end})"),
                GlaDirection::Reverse => format!("<-[{name}:{rel}]-({end})"),
                GlaDirection::Undirected => format!("-[{name}:{rel}]-({end})"),
            };
            for shape in 0..3 {
                let mut pattern = format!("(a){}", edge("r", "R", "b"));
                if shape > 0 {
                    pattern.push_str(&edge("s", "S", "c"));
                }
                if shape > 1 {
                    pattern.push_str(&edge("t", "R", "a"));
                }
                let end = if shape == 0 { "b" } else { "c" };
                let q = prepare(&format!(
                    "MATCH {pattern} RETURN COUNT(*) AS n,COUNT(r.p) AS present,SUM(r.p) AS edge_sum,SUM({end}.p) AS vertex_sum"
                ));
                let s = source(mask);
                let want = oracle(&s, shape, direction);
                let eager = q
                    .execute_governed_with_element_properties(
                        s.edges.len() as u64,
                        s.vertices.keys().copied(),
                        s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
                        |_, _| Ok::<_, ()>(true),
                        |id, _| {
                            Ok(s.vertices[&id]
                                .iter()
                                .find(|(k, _)| *k == P)
                                .map(|(_, v)| v))
                        },
                        |id, _| Ok(s.edges[&id].3.iter().find(|(k, _)| *k == P).map(|(_, v)| v)),
                        policy(),
                        || Ok::<_, ()>(()),
                    )
                    .unwrap();
                let mut stream = run(&q, s, policy());
                assert_eq!(stream.row_stats().result_rows, 0);
                let row = stream.next().unwrap().unwrap();
                assert_eq!(row.values(), want);
                assert_eq!(vec![row], eager.value);
                assert_eq!(stream.row_stats().result_rows, 1);
                assert_eq!(stream.state(), EdgeScanState::Exhausted);
                assert!(stream.next().is_none());
            }
        }
    }
}

#[test]
fn predicates_probes_and_captured_paths_still_use_the_original_binding_executor() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE r.p>0 OR b.p IS NULL RETURN COUNT(*) AS n,SUM(s.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(c) } RETURN COUNT(*) AS n,SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) WHERE c.p>0 } RETURN COUNT(*) AS n",
        "MATCH p=(a)-[:R]->(b)-[:S]->(c) RETURN COUNT(p) AS n",
    ] {
        let q = prepare(text);
        let s = source(63);
        let eager = q
            .execute_governed_with_element_properties(
                s.edges.len() as u64,
                s.vertices.keys().copied(),
                s.edges.iter().map(|(&id, (a, r, b, _))| (id, *a, *r, *b)),
                |vid, tests| {
                    Ok::<_, ()>(tests.iter().all(|test| {
                        test.matches_borrowed([], s.vertices[&vid].iter().map(|(k, v)| (*k, v)))
                    }))
                },
                |id, _| {
                    Ok(s.vertices[&id]
                        .iter()
                        .find(|(k, _)| *k == P)
                        .map(|(_, v)| v))
                },
                |id, _| Ok(s.edges[&id].3.iter().find(|(k, _)| *k == P).map(|(_, v)| v)),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(
            vec![run(&q, s, policy()).next().unwrap().unwrap()],
            eager.value,
            "{text}"
        );
    }
}

#[test]
fn every_cancellation_and_one_less_budget_is_atomic_and_releases_the_source() {
    let q = prepare("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(*) AS n,SUM(r.p) AS total");
    let mut full = run(&q, source(63), policy());
    let expected = full.next().unwrap().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
    assert_eq!(
        run(&q, source(63), exact).next().unwrap().unwrap(),
        expected
    );
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1),
    ] {
        let s = source(63);
        let dropped = s.drops.clone();
        let mut stream = run(&q, s, p);
        assert!(stream.next().unwrap().is_err());
        assert_eq!(stream.row_stats().result_rows, 0);
        assert_eq!(stream.state(), EdgeScanState::Failed);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(stream.next().is_none());
    }
    let mut total = 0;
    EdgeAggregateCursor::new(
        source(63),
        EdgeAggregatePlan::compile(&q).unwrap(),
        exact,
        || {
            total += 1;
            Ok::<_, usize>(())
        },
    )
    .next()
    .unwrap()
    .unwrap();
    for stop in 1..=total {
        let s = source(63);
        let dropped = s.drops.clone();
        let mut calls = 0;
        let mut stream =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), exact, || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(stream.next(), Some(Err(GqlQueryError::Interrupted(at))) if at == stop));
        assert_eq!(stream.row_stats().result_rows, 0);
        assert!(stream.next().is_none());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        drop(stream);
        assert_eq!(calls, stop);
        assert_eq!(
            run(&q, source(63), exact).next().unwrap().unwrap(),
            expected
        );
    }
}

#[test]
fn invalid_data_and_late_source_failures_never_become_partial_counts() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n,SUM(r.p) AS total");
    for kind in 0..3 {
        let mut s = source(63);
        match kind {
            0 => s.fail = Some(EId(6)),
            1 => {
                s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true))];
            }
            _ => {
                s.vertices.remove(&VId(1));
            }
        }
        let mut stream = run(&q, s, policy());
        assert!(stream.next().unwrap().is_err());
        assert_eq!(stream.row_stats().result_rows, 0);
        assert!(stream.next().is_none());
    }
    let q = prepare(
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) } RETURN COUNT(*) AS n",
    );
    let mut s = source(63);
    s.unavailable = true;
    assert!(matches!(
        run(&q, s, policy()).next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
            EdgeScanError::ExpansionUnavailable
        ))))
    ));
}

#[test]
fn ordinary_row_profile_is_not_relaxed_and_unsupported_aggregate_shapes_refuse() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN SUM(r.p) AS total");
    assert!(EdgeScanPlan::compile(q.input_pattern().plan()).is_err());
    assert!(EdgeAggregatePlan::compile(&q).is_ok());
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(DISTINCT b) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p+1) AS n",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) RETURN COUNT(*) AS n",
    ] {
        assert!(
            EdgeAggregatePlan::compile(&prepare(text)).is_err(),
            "{text}"
        );
    }
    let s = source(63);
    let reads = s.reads.clone();
    let dropped = s.drops.clone();
    let mut stream = run(&q, s, policy());
    stream.close();
    stream.close();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(stream.next().is_none());
}

#[test]
fn many_input_rows_need_only_one_result_allowance_and_wide_sums_are_exact() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n,SUM(r.p) AS total");
    let mut s = source(0);
    for id in 0..4096 {
        s.edges.insert(
            EId(id),
            (VId(0), R, VId(1), vec![(P, CanonicalScalar::Int(i64::MAX))]),
        );
    }
    let mut stream = run(&q, s, policy());
    let row = stream.next().unwrap().unwrap();
    assert_eq!(
        row.values(),
        &[
            GraphAggregateValue::Count(4096),
            GraphAggregateValue::Integer(4096 * i128::from(i64::MAX))
        ]
    );
    assert_eq!(stream.row_stats().snapshot_records, 4096);
    assert_eq!(stream.row_stats().result_rows, 1);
    // Boundary arithmetic is the same checked accumulator used by vertices.
    let mut count = NumericState::Count(u64::MAX);
    assert!(matches!(
        count.update::<(), ()>(Input::Identity, 0),
        Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate: 0 }
        ))
    ));
    let mut total = NumericState::Sum(Some(i128::MAX));
    assert!(matches!(
        total.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(1))), 1),
        Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate: 1 }
        ))
    ));
}

mod statistics;