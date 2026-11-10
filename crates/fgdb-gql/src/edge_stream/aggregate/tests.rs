use super::*;
mod computed;
mod cooperative;
mod grouped;
mod groups;
mod output;
mod spill;
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
type Edge = (
    VId,
    RelationId,
    VId,
    Vec<(PropertyKeyId, CanonicalPropertyValue)>,
);
#[derive(Clone)]
struct Source {
    vertices: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
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
        (ids[0], vec![(P, CanonicalScalar::Int(-3).into())]),
        (ids[1], vec![(P, CanonicalScalar::Null.into())]),
        (ids[2], vec![(P, CanonicalScalar::Int(7).into())]),
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
            .map(|w| vec![(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(w)))])
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
        GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_types::LabelId(1))),
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
fn integer(props: &[(PropertyKeyId, CanonicalPropertyValue)]) -> Option<i128> {
    props.iter().find_map(|(key, value)| match value {
        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)) if *key == P => {
            Some(i128::from(*value))
        }
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
    let atoms = [(0, R, 1, dir), (1, S, 2, dir), (2, R, 0, dir)];
    let width = if shape == 0 { 2 } else { 3 };
    assignment_oracle(s, &atoms[..=shape], width, false)
}

fn assignment_oracle(
    s: &Source,
    atoms: &[(usize, RelationId, usize, GlaDirection)],
    width: usize,
    repeatable: bool,
) -> Vec<GraphAggregateValue> {
    let domain: Vec<_> = s.vertices.keys().copied().collect();
    let mut n = 0;
    let mut present = 0;
    let mut edge_sum = None;
    let mut vertex_sum = None;
    for mut code in 0..domain.len().pow(width as u32) {
        let mut values = Vec::new();
        for _ in 0..width {
            values.push(domain[code % domain.len()]);
            code /= domain.len();
        }
        let mut choices = Vec::new();
        for &(from, rel, to, dir) in atoms {
            choices.push(
                s.edges
                    .iter()
                    .filter(|(_, (a, r, b, _))| {
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
                    .map(|(&eid, edge)| (eid, edge))
                    .collect::<Vec<_>>(),
            );
        }
        let combinations = choices.iter().map(Vec::len).product::<usize>();
        for mut code in 0..combinations {
            let first = choices[0][code % choices[0].len()].1;
            let mut selected = Vec::new();
            let mut distinct = true;
            for atom in &choices {
                let (eid, _) = atom[code % atom.len()];
                code /= atom.len();
                distinct &= !selected.contains(&eid);
                selected.push(eid);
            }
            if !repeatable && !distinct {
                continue;
            }
            n += 1;
            if let Some(value) = integer(&first.3) {
                present += 1;
                edge_sum = Some(edge_sum.unwrap_or(0) + value);
            }
            if let Some(value) = integer(&s.vertices[&values[width - 1]]) {
                vertex_sum = Some(vertex_sum.unwrap_or(0) + value);
            }
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
    use crate::scan_stream::AggregateScanPlan;

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
                // Exercise ordinary dispatch against the independent oracle,
                // including all 192 two-hop graph/direction combinations.
                // The triangle keeps the explicit cursor's broader contract.
                let plan = if shape < 2 {
                    let Some(AggregateScanPlan::Edge(plan)) =
                        AggregateScanPlan::compile_numeric(&q)
                    else {
                        panic!("ordinary aggregate refused shape {shape}: {pattern}");
                    };
                    plan
                } else {
                    assert!(AggregateScanPlan::compile_numeric(&q).is_none());
                    EdgeAggregatePlan::compile(&q).unwrap()
                };
                let mut stream = EdgeAggregateCursor::new(s, plan, policy(), || Ok::<_, ()>(()));
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
fn ordinary_tree_numeric_dispatch_matches_full_assignment_bags_in_both_match_modes() {
    use crate::scan_stream::AggregateScanPlan;

    // Three through five-edge chains and branches rooted at different earlier
    // vertices. The oracle enumerates complete assignments and edge products,
    // so it does not share the cursor's incremental expansion strategy.
    let shapes: [&[(usize, RelationId, usize)]; 6] = [
        &[(0, R, 1), (1, S, 2), (2, R, 3)],
        &[(0, R, 1), (1, R, 2), (2, S, 3), (3, R, 4)],
        &[(0, R, 1), (1, S, 2), (2, R, 3), (3, R, 4), (4, S, 5)],
        &[(0, R, 1), (0, R, 2)],
        &[(0, R, 1), (0, R, 2), (1, S, 3)],
        &[(0, R, 1), (0, R, 2), (1, S, 3), (2, R, 4)],
    ];
    let directions = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    for shape in shapes {
        let width = shape
            .iter()
            .flat_map(|&(from, _, to)| [from, to])
            .max()
            .unwrap()
            + 1;
        for profile in 0..4 {
            let atoms: Vec<_> = shape
                .iter()
                .enumerate()
                .map(|(at, &(from, relation, to))| {
                    let direction = if profile < 3 {
                        directions[profile]
                    } else {
                        directions[at % 3]
                    };
                    (from, relation, to, direction)
                })
                .collect();
            for repeatable in [false, true] {
                for captured in [false, true] {
                    let pattern = atoms
                        .iter()
                        .enumerate()
                        .map(|(at, &(from, relation, to, direction))| {
                            let from = char::from(b'a' + from as u8);
                            let to = char::from(b'a' + to as u8);
                            let name = if captured {
                                format!("r{at}")
                            } else {
                                String::new()
                            };
                            let relation = if relation == R { "R" } else { "S" };
                            match direction {
                                GlaDirection::Forward => {
                                    format!("({from})-[{name}:{relation}]->({to})")
                                }
                                GlaDirection::Reverse => {
                                    format!("({from})<-[{name}:{relation}]-({to})")
                                }
                                GlaDirection::Undirected => {
                                    format!("({from})-[{name}:{relation}]-({to})")
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let mode = if repeatable { "REPEATABLE ELEMENTS " } else { "" };
                    let end = char::from(b'a' + (width - 1) as u8);
                    let columns = if captured {
                        format!(
                            "COUNT(*) AS n,COUNT(r0.p) AS present,SUM(r0.p) AS edge_sum,SUM({end}.p) AS vertex_sum"
                        )
                    } else {
                        format!("COUNT(*) AS n,SUM({end}.p) AS vertex_sum")
                    };
                    let text = format!("MATCH {mode}{pattern} RETURN {columns}");
                    let q = prepare(&text);
                    assert!(!q.uses_root_group_retirement(), "{text}");
                    let Some(AggregateScanPlan::Edge(plan)) =
                        AggregateScanPlan::compile_numeric(&q)
                    else {
                        panic!("ordinary tree aggregate refused: {text}");
                    };
                    for mask in 0..64 {
                        let s = source(mask);
                        let mut want = assignment_oracle(&s, &atoms, width, repeatable);
                        if !captured {
                            want = vec![want[0].clone(), want[3].clone()];
                        }
                        let mut stream =
                            EdgeAggregateCursor::new(s, plan.clone(), policy(), || Ok::<_, ()>(()));
                        assert_eq!(
                            stream.next().unwrap().unwrap().values(),
                            want,
                            "mask={mask}: {text}"
                        );
                        assert_eq!(stream.row_stats().result_rows, 1, "{text}");
                        assert_eq!(stream.state(), EdgeScanState::Exhausted, "{text}");
                        assert!(stream.next().is_none(), "{text}");
                    }
                }
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
fn ordinary_filtered_edge_dispatch_matches_eager_and_literal_answers() {
    use crate::scan_stream::AggregateScanPlan;

    // Parallel edges, a missing edge property, a stored NULL vertex property
    // and a self-loop distinguish bag multiplicity from endpoint identity.
    // Each pair is (COUNT(*), SUM(r.p)), forward/reverse/undirected.
    let cases = [
        ("b.p IS NULL", [(2, Some(5)), (0, None), (2, Some(5))]),
        ("a.p IS NULL", [(0, None), (2, Some(5)), (2, Some(5))]),
        (
            "a.p IS NOT NULL AND b.p IS NOT NULL",
            [(2, Some(-3)), (2, Some(-3)), (3, Some(-2))],
        ),
        ("a <> b", [(3, Some(6)), (3, Some(6)), (6, Some(12))]),
        ("a = b", [(1, Some(-4)), (1, Some(-4)), (1, Some(-4))]),
        ("a.p > b.p", [(1, Some(1)), (0, None), (1, Some(1))]),
        ("a.p < b.p", [(0, None), (1, Some(1)), (1, Some(1))]),
        (
            "r.p > 0 OR b.p IS NULL",
            [(3, Some(6)), (2, Some(6)), (5, Some(12))],
        ),
        (
            "NOT (r.p > 0) OR a.p IS NULL",
            [(1, Some(-4)), (3, Some(1)), (3, Some(1))],
        ),
        ("r.p IS NULL", [(1, None), (1, None), (2, None)]),
        (
            "r.p IS NOT NULL AND a <> b",
            [(2, Some(6)), (2, Some(6)), (4, Some(12))],
        ),
    ];
    for (filter, expected) in cases {
        for (pattern, (count, total)) in ["(a)-[r:R]->(b)", "(a)<-[r:R]-(b)", "(a)-[r:R]-(b)"]
            .into_iter()
            .zip(expected)
        {
            let text =
                format!("MATCH {pattern} WHERE {filter} RETURN COUNT(*) AS n,SUM(r.p) AS total");
            let q = prepare(&text);
            let Some(AggregateScanPlan::Edge(plan)) = AggregateScanPlan::compile_numeric(&q) else {
                panic!("filtered single-edge aggregate was not admitted: {text}");
            };
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
            let mut stream = EdgeAggregateCursor::new(s, plan, policy(), || Ok::<_, ()>(()));
            let row = stream.next().unwrap().unwrap();
            assert_eq!(
                row.values(),
                &[GraphAggregateValue::Count(count), sum(total)],
                "{text}"
            );
            assert_eq!(vec![row], eager.value, "{text}");
            assert_eq!(stream.row_stats().result_rows, 1);
            assert!(stream.next().is_none());
        }
    }
}

#[test]
fn ordinary_filtered_edge_dispatch_preserves_exact_count_candidates() {
    use crate::scan_stream::AggregateScanPlan;

    // Endpoint-label universality still requires the host's snapshot proof.
    for text in [
        "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS n",
        "MATCH (a:L)<-[:R]-(b:L) RETURN COUNT(a) AS n",
        "MATCH (a:L)-[]->(b:L) RETURN COUNT(b) AS n",
    ] {
        let q = prepare(text);
        assert!(q.count_metadata_source().is_none(), "{text}");
        assert!(q.count_metadata_candidate_source().is_some(), "{text}");
        assert!(AggregateScanPlan::compile_numeric(&q).is_none(), "{text}");
    }
    for text in [
        "MATCH (a:L)-[r:R]->(b:L) RETURN SUM(r.p) AS total",
        "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(DISTINCT a) AS n",
        "MATCH (a:L)-[r:R]->(b:L) RETURN COUNT(DISTINCT r) AS n",
        "MATCH (a:L)-[:R]-(b:L) RETURN COUNT(*) AS n",
    ] {
        let q = prepare(text);
        assert!(q.count_metadata_candidate_source().is_none(), "{text}");
        assert!(
            matches!(
                AggregateScanPlan::compile_numeric(&q),
                Some(AggregateScanPlan::Edge(_))
            ),
            "{text}"
        );
    }
}

#[test]
fn ordinary_filtered_edge_dispatch_keeps_keyed_and_multihop_host_plans() {
    use crate::scan_stream::AggregateScanPlan;

    for text in [
        "MATCH (a)-[r:R]->(b) WHERE a.p=7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE b.p=7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE a.p>=7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE b.p<7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE a.p<>7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S]->(c) } RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE b.p IS NULL RETURN COLLECT(r.p) AS values",
    ] {
        assert!(
            AggregateScanPlan::compile_numeric(&prepare(text)).is_none(),
            "{text}"
        );
    }
}

#[test]
fn ordinary_two_hop_numeric_dispatch_admits_plain_directional_predicates() {
    use crate::scan_stream::AggregateScanPlan;

    for pattern in [
        "(a)-[r:R]->(b)-[s:S]->(c)",
        "(a)<-[r:R]-(b)<-[s:S]-(c)",
        "(a)-[r:R]-(b)-[s:S]-(c)",
        "(a)-[r:R]->(b)<-[s:S]-(c)",
        "(a)<-[r:R]-(b)-[s:S]->(c)",
        "(a)-[r:R]->(b)-[s:S]->(c)-[t:R]->(d)",
        "(a)-[r:R]->(b), (a)-[s:S]->(c)",
        "(a)-[r:R]->(b)-[s:S]->(c), (b)-[t:R]->(d)",
        "(a)<-[r:R]-(b)-[s:S]-(c)-[t:R]->(d)-[u:S]->(e)-[v:R]->(f)",
    ] {
        for filter in [
            "",
            "WHERE b.p IS NULL",
            "WHERE c.p < 7",
            "WHERE a <> c",
            "WHERE a = b",
            "WHERE a.p < c.p",
            "WHERE r.p > 0 OR b.p IS NULL",
        ] {
            for numeric in [
                "SUM(r.p)",
                "SUM(DISTINCT r.p)",
                "AVG(s.p)",
                "AVG(DISTINCT s.p)",
            ] {
                let text =
                    format!("MATCH {pattern} {filter} RETURN COUNT(*) AS n,{numeric} AS numeric");
                assert!(
                    matches!(
                        AggregateScanPlan::compile_numeric(&prepare(&text)),
                        Some(AggregateScanPlan::Edge(_))
                    ),
                    "{text}"
                );
            }
        }
    }
    assert!(matches!(
        AggregateScanPlan::compile_numeric(&prepare(
            "MATCH (a:L)-[r:R]->(b:L)-[s:S]->(c:L) RETURN SUM(s.p) AS total"
        )),
        Some(AggregateScanPlan::Edge(_))
    ));
    assert!(matches!(
        AggregateScanPlan::compile_numeric(&prepare(
            "MATCH (a)-[r:R]->(b)-[:S]->(c)-[:R]->(d) WHERE b.p IS NULL RETURN SUM(r.p) AS total"
        )),
        Some(AggregateScanPlan::Edge(_))
    ));
}

#[test]
fn ordinary_two_hop_numeric_dispatch_preserves_host_join_strategies() {
    use crate::scan_stream::AggregateScanPlan;

    for text in [
        // Counts and support summaries keep their factorized evaluator.
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(r.p) AS n",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(DISTINCT c) AS n",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN MIN(c.p) AS lo,MAX(c.p) AS hi",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:R]->(d) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b), (a)-[:S]->(c) RETURN MIN(c.p) AS lo,MAX(c.p) AS hi",
        // Root literal predicates can select an indexed endpoint closure.
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE a.p=7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE b.p>=7 RETURN AVG(s.p) AS mean",
        "MATCH (a)-[r:R]->(b)-[:S]->(c)-[:R]->(d) WHERE a.p=7 RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b), (a)-[s:S]->(c) WHERE b.p>=7 RETURN AVG(s.p) AS mean",
        // Closing identities retain indexed seeks and intersection access.
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE a=c RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE b=c RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b)-[s:S]->(a) RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b)-[:S]->(c)-[:R]->(d) WHERE a=d RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b)-[:S]->(c)-[:R]->(a) RETURN SUM(r.p) AS total",
        "MATCH (a)-[r:R]->(b), (a)-[:S]->(c), (b)-[:R]->(d) WHERE c=d RETURN SUM(r.p) AS total",
        // Collection order and computed input remain outside this extension.
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN SUM(r.p) AS total,COLLECT(c.p) AS values",
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN SUM(r.p * c.p + 2) AS total",
        "MATCH (a)-[r:R]->(b)-[:S]->(c)-[:R]->(d) RETURN SUM(r.p) AS total,COLLECT(d.p) AS values",
        "MATCH (a)-[r:R]->(b), (a)-[:S]->(c), (b)-[:R]->(d) RETURN SUM(r.p * d.p + 2) AS total",
    ] {
        assert!(
            AggregateScanPlan::compile_numeric(&prepare(text)).is_none(),
            "{text}"
        );
    }
}

#[test]
fn ordinary_numeric_dispatch_preserves_reachable_finite_root_group_retirement() {
    use crate::scan_stream::AggregateScanPlan;

    for text in [
        "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS mean GROUP BY a LIMIT 2",
        "MATCH (a)<-[:R]-(b) RETURN a,SUM(b.p) AS total GROUP BY a SKIP 1 LIMIT 2",
        "MATCH (a)-[:R]-(b) RETURN AVG(DISTINCT b.p) AS mean GROUP BY a LIMIT 2",
        "MATCH (a)-[:R]->(b) RETURN a,SUM(b.p) AS total GROUP BY a SKIP 18446744073709551615 LIMIT 0",
        // Distinct relationship types prove DIFFERENT EDGES without EIds.
        "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a,SUM(c.p) AS total GROUP BY a LIMIT 2",
        "MATCH (a)-[:R]->(b), (a)-[:S]->(c) RETURN a,AVG(c.p) AS mean GROUP BY a LIMIT 2",
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c) RETURN a,SUM(c.p) AS total GROUP BY a SKIP 1 LIMIT 2",
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c)-[:R]->(d) RETURN a,SUM(d.p) AS total GROUP BY a LIMIT 2",
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b), (a)-[:R]->(c), (b)-[:R]->(d) RETURN a,SUM(d.p) AS total GROUP BY a LIMIT 2",
    ] {
        let q = prepare(text);
        let definition = q.canonical_bytes();
        assert!(!q.input_pattern().plan().requires_identified_edges(), "{text}");
        assert!(q.uses_root_group_retirement(), "{text}");
        assert!(AggregateScanPlan::compile_numeric(&q).is_none(), "{text}");
        assert_eq!(q.canonical_bytes(), definition, "{text}");
    }
}

#[test]
fn ordinary_numeric_dispatch_does_not_infer_root_retirement_from_group_key_alone() {
    use crate::scan_stream::AggregateScanPlan;

    for text in [
        "MATCH (a)-[:R]->(b) RETURN SUM(b.p) AS total",
        "MATCH (a)-[:R]->(b) RETURN b,SUM(b.p) AS total GROUP BY b LIMIT 2",
        "MATCH (a)-[:R]->(b) RETURN a,b,SUM(b.p) AS total GROUP BY a,b LIMIT 2",
        "MATCH (a)-[:R]->(b) RETURN a.p,SUM(b.p) AS total GROUP BY a.p LIMIT 2",
        "MATCH (a)-[:R]->(b) RETURN a,SUM(b.p) AS total GROUP BY a",
        "MATCH (a)-[:R]->(b) RETURN a,SUM(b.p) AS total GROUP BY a SKIP 1 LIMIT 18446744073709551615",
        "MATCH (a)-[:R]->(b) RETURN DISTINCT AVG(b.p) AS mean GROUP BY a LIMIT 2",
        "MATCH (a)-[:R]->(b) RETURN a,SUM(b.p + 1) AS total GROUP BY a LIMIT 2",
        // Both edge captures and real DIFFERENT EDGES checks route through
        // the element executor, which cannot reach the root-group owner.
        "MATCH (a)-[r:R]->(b) RETURN a,SUM(r.p) AS total GROUP BY a LIMIT 2",
        "MATCH REPEATABLE ELEMENTS (a)-[r:R]->(b) RETURN a,SUM(b.p) AS total GROUP BY a LIMIT 2",
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a,SUM(c.p) AS total GROUP BY a LIMIT 2",
        "MATCH (a)-[:R]->(b), (a)-[:R]->(c), (b)-[:R]->(d) RETURN a,SUM(d.p) AS total GROUP BY a LIMIT 2",
    ] {
        let q = prepare(text);
        let definition = q.canonical_bytes();
        assert!(!q.uses_root_group_retirement(), "{text}");
        assert!(
            matches!(
                AggregateScanPlan::compile_numeric(&q),
                Some(AggregateScanPlan::Edge(_))
            ),
            "{text}"
        );
        assert_eq!(q.canonical_bytes(), definition, "{text}");
    }
}

#[test]
fn root_group_capability_keeps_computed_and_relational_input_owners_separate() {
    use crate::scan_stream::AggregateScanPlan;
    use crate::{GraphAggregate, PreparedGraphSet};

    let plain = prepare(
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c) RETURN a,SUM(c.p) AS total GROUP BY a LIMIT 2",
    );
    assert!(plain.uses_root_group_retirement());
    let relation = PreparedGraphAggregate::prepare_relation(
        PreparedGraphSet::from(plain.input_pattern().clone()),
        plain.group_key_columns(),
        &[GraphAggregate::sum_int(
            "total",
            plain.aggregates()[0].argument_column().unwrap(),
        )],
        0,
        Some(2),
    )
    .unwrap();
    assert!(relation.input_relation().is_some());
    assert!(!relation.uses_root_group_retirement());
    assert!(AggregateScanPlan::compile_numeric(&relation).is_none());

    let computed = prepare(
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:R]->(c) RETURN a,SUM(c.p + 1) AS total GROUP BY a LIMIT 2",
    );
    assert!(computed.input_projection().is_some());
    assert!(!computed.uses_root_group_retirement());
    assert!(AggregateScanPlan::compile_numeric(&computed).is_none());
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
                s.edges.get_mut(&EId(6)).unwrap().3 = vec![(P, CanonicalScalar::Bool(true).into())];
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
    // Plain edge collections now stream (output.rs pins them to the eager
    // answer); a computed collection and an optional join still refuse.
    for text in [
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
            (
                VId(0),
                R,
                VId(1),
                vec![(P, CanonicalScalar::Int(i64::MAX).into())],
            ),
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
