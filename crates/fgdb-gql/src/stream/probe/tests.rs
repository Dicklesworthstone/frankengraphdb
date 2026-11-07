//! Vertex-rooted probe laws against an independent complete-relation oracle.
use super::*;
use crate::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::RelationId;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
struct Fixture {
    vertices: BTreeSet<VId>,
    props: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
    edges: BTreeMap<EId, (VId, VId)>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<VId>,
    roots: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    repeat: bool,
    fail_edge: Option<EId>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Fixture {
    fn from_edges(edges: impl IntoIterator<Item = (u128, u128, u128)>) -> Self {
        let mut f = Self {
            vertices: BTreeSet::new(),
            props: BTreeMap::new(),
            edges: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            roots: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            repeat: false,
            fail_edge: None,
        };
        for (id, a, b) in edges {
            f.vertices.extend([VId(a), VId(b)]);
            f.edges.insert(EId(id), (VId(a), VId(b)));
            f.outgoing.entry(VId(a)).or_default().insert(EId(id));
            f.incoming.entry(VId(b)).or_default().insert(EId(id));
        }
        f
    }
    fn small(mask: usize) -> Self {
        let mut f = Self::from_edges(
            [
                (0, 0, 0),
                (1, 0, 1),
                (2, 0, 1),
                (3, 1, 2),
                (4, 2, 0),
                (u128::MAX, 2, 2),
            ]
            .into_iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, e)| e),
        );
        // Isolates, even on an empty graph, must be candidates for NOT EXISTS.
        f.vertices
            .extend([VId(0), VId(1), VId(2), VId(3), VId(u128::MAX)]);
        for (id, value) in [
            (0, CanonicalScalar::Int(3)),
            (1, CanonicalScalar::Null),
            (2, CanonicalScalar::Int(7)),
            (u128::MAX, CanonicalScalar::Int(i64::MIN)),
        ] {
            f.props.insert(VId(id), vec![(P, value.into())]);
        }
        f
    }
    fn value(&self, vid: VId) -> CanonicalScalar {
        self.props
            .get(&vid)
            .and_then(|p| p.first())
            .map(|p| p.1.as_scalar().unwrap().clone())
            .unwrap_or(CanonicalScalar::Null)
    }
}
impl VertexScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(13)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.roots.fetch_add(1, Ordering::SeqCst);
        let next = self
            .vertices
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .copied();
        if next.is_some() {
            self.after = next;
        }
        Ok(next)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(self.vertices.contains(&vid).then_some(VertexScanRow {
            labels: &[],
            properties: self.props.get(&vid).map_or(&[], Vec::as_slice),
        }))
    }
    fn next_probe_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        self.seeks.fetch_add(1, Ordering::SeqCst);
        if self.repeat && after.is_some() {
            return Ok(after);
        }
        let next = |index: &BTreeMap<VId, BTreeSet<EId>>| {
            index.get(&endpoint).and_then(|set| {
                set.range((after.map_or(Unbounded, Excluded), Unbounded))
                    .next()
                    .copied()
            })
        };
        Ok(match direction {
            GlaDirection::Forward => next(&self.outgoing),
            GlaDirection::Reverse => next(&self.incoming),
            GlaDirection::Undirected => match (next(&self.outgoing), next(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        })
    }
    fn probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        if self.fail_edge == Some(eid) {
            return Err(EdgeExpansionSourceError::Read(
                VertexScanSourceError::Source("edge read denied"),
            ));
        }
        Ok(self.edges.get(&eid).map(|&(source, target)| EdgeScanRow {
            source,
            target,
            relation: R,
            properties: &[],
        }))
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 10_000_000)
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}
fn atom(end: &str, d: GlaDirection) -> String {
    match d {
        GlaDirection::Forward => format!("-[:R]->({end})"),
        GlaDirection::Reverse => format!("<-[:R]-({end})"),
        GlaDirection::Undirected => format!("-[:R]-({end})"),
    }
}
fn text(
    shape: usize,
    d: GlaDirection,
    anti: bool,
    nullable: bool,
    distinct: bool,
    skip: usize,
    limit: usize,
) -> String {
    let tail = match shape {
        1 => atom("a", d),
        2 => format!(", (a){}", atom("y", d)),
        _ => String::new(),
    };
    let condition = if nullable {
        "NOT (x.p = 7)"
    } else if shape == 2 {
        "x <> y"
    } else {
        "x <> a"
    };
    format!(
        "MATCH (a) WHERE {}EXISTS {{ MATCH (a){}{tail} WHERE {condition} }} RETURN {}a, a.p AS p SKIP {skip} LIMIT {limit}",
        if anti { "NOT " } else { "" },
        atom("x", d),
        if distinct { "DISTINCT " } else { "" }
    )
}
// Enumerate every complete inner product. Reduce to a bool only afterward;
// no incidence index, prefix pruning or production Probe is used here.
fn expected(
    f: &Fixture,
    shape: usize,
    d: GlaDirection,
    anti: bool,
    nullable: bool,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphValue>> {
    let relation: Vec<_> = f
        .edges
        .iter()
        .flat_map(|(&eid, &(a, b))| match d {
            GlaDirection::Forward => vec![(eid, a, b)],
            GlaDirection::Reverse => vec![(eid, b, a)],
            GlaDirection::Undirected if a == b => vec![(eid, a, b)],
            _ => vec![(eid, a, b), (eid, b, a)],
        })
        .collect();
    let mut rows = Vec::new();
    for &a in &f.vertices {
        let mut witnesses = 0;
        for &(first, start, x) in &relation {
            if start != a {
                continue;
            }
            let allowed = |y| {
                if nullable {
                    matches!(f.value(x), CanonicalScalar::Int(v) if v != 7)
                } else if shape == 2 {
                    x != y
                } else {
                    x != a
                }
            };
            if shape == 0 {
                if allowed(a) {
                    witnesses += 1;
                }
                continue;
            }
            for &(second_eid, second, y) in &relation {
                if first == second_eid || second != (if shape == 1 { x } else { a }) {
                    continue;
                }
                if (shape != 1 || y == a) && allowed(y) {
                    witnesses += 1;
                }
            }
        }
        if (witnesses > 0) != anti {
            rows.push(vec![GraphValue::Vertex(a), GraphValue::Scalar(f.value(a))]);
        }
    }
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn vertex_semijoins_include_isolates_and_equal_complete_products_and_eager_gla() {
    for mask in [0, 1, 10, 15, 31, 63] {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for shape in 0..3 {
                for anti in [false, true] {
                    for nullable in [false, true] {
                        for distinct in [false, true] {
                            for (skip, limit) in [(0, 20), (1, 2), (0, 0)] {
                                let input =
                                    text(shape, direction, anti, nullable, distinct, skip, limit);
                                let q = prepare(&input);
                                let f = Fixture::small(mask);
                                let want =
                                    expected(&f, shape, direction, anti, nullable, skip, limit);
                                let eager = q
                                    .plan()
                                    .execute_governed_with_element_properties(
                                        f.vertices.len() as u64 + f.edges.len() as u64,
                                        f.vertices.iter().copied(),
                                        f.edges.iter().map(|(&id, &(a, b))| (id, a, R, b)),
                                        |vid, predicates| {
                                            Ok::<_, ()>(predicates.iter().all(|p| {
                                                p.matches_borrowed(
                                                    [],
                                                    f.props
                                                        .get(&vid)
                                                        .into_iter()
                                                        .flatten()
                                                        .map(|(k, v)| (*k, v)),
                                                )
                                            }))
                                        },
                                        |vid, _| {
                                            Ok(f.props
                                                .get(&vid)
                                                .and_then(|p| p.first())
                                                .map(|p| &p.1))
                                        },
                                        |_, _| Ok(None),
                                        policy(),
                                        || Ok::<_, ()>(()),
                                    )
                                    .unwrap();
                                assert_eq!(plain(&eager.value), want, "eager {input}");
                                let mut stream = VertexScanCursor::new(
                                    f,
                                    VertexScanPlan::compile(q.plan()).unwrap(),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                );
                                let mut actual = Vec::new();
                                for page in [1, 2, 16] {
                                    for _ in 0..page {
                                        let Some(row) = stream.next() else {
                                            break;
                                        };
                                        actual.push(row.unwrap());
                                    }
                                }
                                assert_eq!(plain(&actual), want, "stream {input}");
                                assert_eq!(stream.row_stats().result_rows, want.len() as u64);
                                assert_eq!(stream.state(), VertexScanState::Exhausted);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn first_witness_never_reads_a_bad_suffix_and_probe_failures_are_not_absence() {
    let input = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a LIMIT 1";
    let q = prepare(input);
    let mut f = Fixture::small(63);
    f.repeat = true;
    f.fail_edge = Some(EId(1));
    let seeks = f.seeks.clone();
    let roots = f.roots.clone();
    let mut cursor = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        GqlQueryPolicy::new(2, 1, 100_000, 10_000),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        cursor.next().unwrap().unwrap().values(),
        &[GraphValue::Vertex(VId(0))]
    );
    assert_eq!(cursor.row_stats().snapshot_records, 2);
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    assert_eq!(roots.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    let q = prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) WHERE x.p > 100 } RETURN a");
    let mut f = Fixture::small(63);
    f.fail_edge = Some(EId(1));
    let mut cursor = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Source(
            "edge read denied"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());
    let mut f = Fixture::small(63);
    f.repeat = true;
    let mut cursor = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::NonIncreasingIdentity
        ))))
    ));
}

struct NoLookup(Fixture);
impl VertexScanSource for NoLookup {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        self.0.snapshot_seq()
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        self.0.next_vertex(control)
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        self.0.vertex(id, control)
    }
}
struct OnlyIndex(Fixture);
impl VertexScanSource for OnlyIndex {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        self.0.snapshot_seq()
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        self.0.next_vertex(control)
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        self.0.vertex(id, control)
    }
    fn next_probe_edge<C>(
        &self,
        at: VId,
        d: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.0.next_probe_edge(at, d, after, control)
    }
}

#[test]
fn both_optional_source_seams_refuse_but_vertex_only_probes_need_neither() {
    let q = prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) } RETURN a");
    let mut absent = VertexScanCursor::new(
        NoLookup(Fixture::small(63)),
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        absent.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::ExpansionUnavailable
        ))))
    ));
    let mut partial = VertexScanCursor::new(
        OnlyIndex(Fixture::small(63)),
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        partial.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::ExpansionUnavailable
        ))))
    ));
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (a) WHERE a.p = 7 } RETURN a");
    let cursor = VertexScanCursor::new(
        NoLookup(Fixture::small(0)),
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        plain(&cursor.collect::<Result<Vec<_>, _>>().unwrap()),
        vec![vec![GraphValue::Vertex(VId(2))]]
    );
    let q = prepare(
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } AND NOT EXISTS { MATCH (a)-[:R]->(x) WHERE x.p IS NULL } RETURN a",
    );
    let cursor = VertexScanCursor::new(
        Fixture::small(63),
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        plain(&cursor.collect::<Result<Vec<_>, _>>().unwrap()),
        vec![
            vec![GraphValue::Vertex(VId(1))],
            vec![GraphValue::Vertex(VId(2))]
        ]
    );
}

#[test]
fn every_interruption_is_terminal_retains_only_delivered_rows_and_retries_exactly() {
    for anti in [false, true] {
        let q = prepare(&text(1, GlaDirection::Undirected, anti, true, false, 0, 20));
        let mut total = 0;
        let mut good = VertexScanCursor::new(
            Fixture::small(63),
            VertexScanPlan::compile(q.plan()).unwrap(),
            policy(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        );
        let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        drop(good);
        for stop in 1..=total {
            let mut calls = 0;
            let f = Fixture::small(63);
            let drops = f.drops.clone();
            let mut cursor = VertexScanCursor::new(
                f,
                VertexScanPlan::compile(q.plan()).unwrap(),
                policy(),
                || {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            let mut delivered = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => delivered.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => {
                        assert_eq!(at, stop);
                        break;
                    }
                    other => panic!("interruption became {other:?}"),
                }
            }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(cursor);
            assert_eq!(calls, stop);
            let retry = VertexScanCursor::new(
                Fixture::small(63),
                VertexScanPlan::compile(q.plan()).unwrap(),
                policy(),
                || Ok::<_, usize>(()),
            );
            assert_eq!(retry.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        }
    }
}

#[test]
fn exact_limits_cover_outer_candidates_and_all_inner_work_without_resetting() {
    let q = prepare(&text(2, GlaDirection::Undirected, true, true, false, 0, 20));
    let run = |p| {
        VertexScanCursor::new(
            Fixture::small(63),
            VertexScanPlan::compile(q.plan()).unwrap(),
            p,
            || Ok::<_, ()>(()),
        )
    };
    let mut good = run(policy());
    let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = good.row_stats();
    let e = good.evaluator_stats();
    assert!(!expected.is_empty());
    assert!(r.snapshot_records > 5);
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    let mut retry = run(exact);
    assert_eq!(
        retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    assert_eq!((retry.row_stats(), retry.evaluator_stats()), (r, e));
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor = run(p);
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn full_depth_probes_find_first_witness_without_an_outer_edge_or_result_bag() {
    for hops in [40, crate::algebra::MAX_PATTERN_EDGES] {
        let mut input = "MATCH (a) WHERE EXISTS { MATCH (a)".to_owned();
        for i in 0..hops {
            input.push_str(&format!("-[:R]->(v{})", i + 1));
        }
        input.push_str(" } RETURN a LIMIT 1");
        let q = prepare(&input);
        let f = Fixture::from_edges(
            (0..hops as u128).flat_map(|i| [(2 * i, i, i + 1), (2 * i + 1, i, i + 1)]),
        );
        let seeks = f.seeks.clone();
        let drops = f.drops.clone();
        let mut cursor = VertexScanCursor::new(
            f,
            VertexScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(hops as u64 + 1, 1, 100_000, 10_000),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            cursor.next().unwrap().unwrap().values(),
            &[GraphValue::Vertex(VId(0))]
        );
        assert_eq!(cursor.row_stats().snapshot_records, hops as u64 + 1);
        assert_eq!(seeks.load(Ordering::SeqCst), hops);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unsupported_scopes_and_outputs_refuse_even_at_limit_zero_without_driving_a_source() {
    for input in [
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(x) RETURN a, x LIMIT 0",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a.p LIMIT 0",
    ] {
        let q = prepare(input);
        assert!(VertexScanPlan::compile(q.plan()).is_err(), "{input}");
    }
    for body in ["MATCH (a)-[:R*1..3]->(x)", "MATCH (x)-[:R*1..3]->(y)"] {
        let q = prepare(&format!(
            "MATCH (a) WHERE NOT EXISTS {{ {body} }} RETURN a LIMIT 0"
        ));
        let f = Fixture::small(63);
        let roots = f.roots.clone();
        let seeks = f.seeks.clone();
        let mut cursor = VertexScanCursor::new(
            NoLookup(f),
            VertexScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(0, 0, 10, 0),
            || Ok::<_, ()>(()),
        );
        assert!(cursor.next().is_none());
        assert_eq!(roots.load(Ordering::SeqCst), 0);
        assert_eq!(seeks.load(Ordering::SeqCst), 0);
    }
}

fn collection_pattern(
    direction: GlaDirection,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    use crate::algebra::{GraphColumn, GraphMatchClause, GraphPatternBuilder};

    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("a").unwrap().vertex("b").unwrap();
    inner.edge("a", R, direction, "b").unwrap();
    inner.capture_edge("r", 0).unwrap();
    inner.capture_path("walk").unwrap();
    outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&inner, Some("b")),
                GraphMatchClause::collect(&inner, Some("r")),
                GraphMatchClause::collect(&inner, Some("walk")),
                GraphMatchClause::collect_tuple(&inner, vec!["a", "r", "b", "walk"]),
                GraphMatchClause::collect(&inner, None),
            ],
            &[
                GraphColumn::vertex("id", "a"),
                GraphColumn::collection("vertices", 0),
                GraphColumn::collection("edges", 1),
                GraphColumn::collection("paths", 2),
                GraphColumn::collection("tuples", 3),
                GraphColumn::collection("nulls", 4),
            ],
            0,
            count,
        )
        .unwrap()
}

fn collected_list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
    GraphValue::List(values.into_iter().collect::<Vec<_>>().into_boxed_slice())
}

// Enumerate the complete physical relation independently. Every EId is a
// witness; a self-loop contributes once in the undirected case. No production
// incidence iterator, Collection evaluator or path capture builds this oracle.
fn expected_collections(fixture: &Fixture, direction: GlaDirection) -> Vec<Vec<GraphValue>> {
    fixture
        .vertices
        .iter()
        .map(|&root| {
            let witnesses: Vec<_> = fixture
                .edges
                .iter()
                .filter_map(|(&eid, &(from, to))| {
                    let target = match direction {
                        GlaDirection::Forward if from == root => to,
                        GlaDirection::Reverse if to == root => from,
                        GlaDirection::Undirected if from == root => to,
                        GlaDirection::Undirected if to == root => from,
                        _ => return None,
                    };
                    Some((eid, target))
                })
                .collect();
            let path = |eid, target| {
                GraphValue::Path(crate::algebra::GraphPath::new(
                    root,
                    vec![(eid, target)].into_boxed_slice(),
                ))
            };
            vec![
                GraphValue::Vertex(root),
                collected_list(witnesses.iter().map(|&(_, to)| GraphValue::Vertex(to))),
                collected_list(witnesses.iter().map(|&(eid, _)| GraphValue::Edge(eid))),
                collected_list(witnesses.iter().map(|&(eid, to)| path(eid, to))),
                collected_list(witnesses.iter().map(|&(eid, to)| {
                    collected_list([
                        GraphValue::Vertex(root),
                        GraphValue::Edge(eid),
                        GraphValue::Vertex(to),
                        path(eid, to),
                    ])
                })),
                collected_list(
                    witnesses
                        .iter()
                        .map(|_| GraphValue::Scalar(CanonicalScalar::Null)),
                ),
            ]
        })
        .collect()
}

#[test]
fn vertex_collections_preserve_every_witness_capture_and_empty_root() {
    for mask in 0..64 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let pattern = collection_pattern(direction, None);
            let identity = pattern.plan().canonical_bytes();
            let fixture = Fixture::small(mask);
            let expected = expected_collections(&fixture, direction);
            let plan = VertexScanPlan::compile(pattern.plan()).unwrap();
            assert!(plan.has_graph_probes());
            let mut cursor =
                VertexScanCursor::new(fixture, plan, policy(), || Ok::<_, ()>(()));
            let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(plain(&actual), expected, "mask={mask}, {direction:?}");
            assert_eq!(cursor.row_stats().result_rows, 5);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
            assert_eq!(pattern.plan().canonical_bytes(), identity);
        }
    }
}

#[test]
fn every_collection_checkpoint_keeps_complete_prefixes_and_releases_the_source() {
    // Root zero has an empty list; root one has a witness. Interruption must
    // preserve that first complete root, never a partial second list.
    let pattern = collection_pattern(GlaDirection::Forward, Some(2));
    let plan = VertexScanPlan::compile(pattern.plan()).unwrap();
    let mut total = 0;
    let mut baseline = VertexScanCursor::new(Fixture::small(8), plan.clone(), policy(), || {
        total += 1;
        Ok::<_, usize>(())
    });
    let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(expected.len(), 2);
    drop(baseline);
    assert!(total > 0);
    for stop in 1..=total {
        let fixture = Fixture::small(8);
        let drops = fixture.drops.clone();
        let mut calls = 0;
        let mut cursor = VertexScanCursor::new(fixture, plan.clone(), policy(), || {
            calls += 1;
            if calls == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => {
                    assert_eq!(at, stop);
                    break;
                }
                other => panic!("collection interruption became {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        let frozen = (cursor.row_stats(), cursor.evaluator_stats());
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), frozen);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(cursor);
        assert_eq!(calls, stop);
    }
    let retry = VertexScanCursor::new(Fixture::small(8), plan, policy(), || Ok::<_, usize>(()));
    assert_eq!(retry.collect::<Result<Vec<_>, _>>().unwrap(), expected);
}

#[test]
fn collection_source_failures_never_become_absence_or_publish_partial_rows() {
    let pattern = collection_pattern(GlaDirection::Forward, None);
    let plan = VertexScanPlan::compile(pattern.plan()).unwrap();
    let mut fixture = Fixture::small(63);
    let expected = expected_collections(&fixture, GlaDirection::Forward);
    fixture.fail_edge = Some(EId(3));
    let drops = fixture.drops.clone();
    let mut cursor =
        VertexScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
    assert_eq!(cursor.next().unwrap().unwrap().values(), expected[0]);
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Source(
            "edge read denied"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let mut fixture = Fixture::small(63);
    fixture.repeat = true;
    let mut cursor =
        VertexScanCursor::new(fixture, plan.clone(), policy(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::NonIncreasingIdentity
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());

    let mut cursor = VertexScanCursor::new(
        NoLookup(Fixture::small(63)),
        plan,
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::ExpansionUnavailable
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());
}

fn anonymous_collection_pattern(
    direction: GlaDirection,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    use crate::algebra::{GraphColumn, GraphMatchClause, GraphPatternBuilder};

    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("a").unwrap().vertex("b").unwrap();
    inner.edge("a", R, direction, "b").unwrap();
    outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&inner, Some("b")),
                GraphMatchClause::collect_tuple(&inner, vec!["b", "a", "b"]),
                GraphMatchClause::collect(&inner, Some("a")),
                GraphMatchClause::collect(&inner, None),
            ],
            &[
                GraphColumn::vertex("id", "a"),
                GraphColumn::collection("neighbors", 0),
                GraphColumn::collection("tuples", 1),
                GraphColumn::collection("roots", 2),
                GraphColumn::collection("nulls", 3),
            ],
            0,
            count,
        )
        .unwrap()
}

fn anonymous_collection_fixture(mask: usize) -> Fixture {
    let mut fixture = Fixture::from_edges(
        [
            (0, 0, 2),
            (1, 0, 1),
            (2, 0, 1),
            (3, 2, 0),
            (4, 0, 0),
            (u128::MAX, 2, 2),
        ]
        .into_iter()
        .enumerate()
        .filter(|(at, _)| mask & (1 << at) != 0)
        .map(|(_, edge)| edge),
    );
    fixture
        .vertices
        .extend([VId(0), VId(1), VId(2), VId(3), VId(u128::MAX)]);
    fixture
}

#[test]
fn anonymous_collections_keep_endpoint_order_tuples_and_parallel_multiplicity() {
    for mask in 0..64 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let pattern = anonymous_collection_pattern(direction, None);
            assert!(!pattern.plan().requires_identified_edges());
            let fixture = anonymous_collection_fixture(mask);
            // Complete physical relation, independent of the indexed walker
            // and its governed sorter. EId order deliberately disagrees with
            // endpoint order; parallel edges and self-loops remain witnesses.
            let expected: Vec<_> = fixture
                .vertices
                .iter()
                .map(|&root| {
                    let mut endpoints: Vec<_> = fixture
                        .edges
                        .values()
                        .filter_map(|&(from, to)| match direction {
                            GlaDirection::Forward if from == root => Some(to),
                            GlaDirection::Reverse if to == root => Some(from),
                            GlaDirection::Undirected if from == root => Some(to),
                            GlaDirection::Undirected if to == root => Some(from),
                            _ => None,
                        })
                        .collect();
                    endpoints.sort_unstable();
                    vec![
                        GraphValue::Vertex(root),
                        collected_list(endpoints.iter().copied().map(GraphValue::Vertex)),
                        collected_list(endpoints.iter().map(|&to| {
                            collected_list([
                                GraphValue::Vertex(to),
                                GraphValue::Vertex(root),
                                GraphValue::Vertex(to),
                            ])
                        })),
                        collected_list(endpoints.iter().map(|_| GraphValue::Vertex(root))),
                        collected_list(
                            endpoints
                                .iter()
                                .map(|_| GraphValue::Scalar(CanonicalScalar::Null)),
                        ),
                    ]
                })
                .collect();
            let eager = pattern
                .plan()
                .execute_governed_with_element_properties(
                    (fixture.vertices.len() + fixture.edges.len()) as u64,
                    fixture.vertices.iter().copied(),
                    fixture
                        .edges
                        .iter()
                        .map(|(&eid, &(from, to))| (eid, from, R, to)),
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(plain(&eager.value), expected, "eager mask={mask}, {direction:?}");
            let mut cursor = VertexScanCursor::new(
                fixture,
                VertexScanPlan::compile(pattern.plan()).unwrap(),
                policy(),
                || Ok::<_, ()>(()),
            );
            assert_eq!(
                plain(&cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                expected,
                "stream mask={mask}, {direction:?}"
            );
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert_eq!(cursor.row_stats().result_rows, 5);
        }
    }
}

#[test]
fn anonymous_collection_normalization_keeps_one_meter_and_terminal_prefixes() {
    let pattern = anonymous_collection_pattern(GlaDirection::Forward, Some(2));
    let plan = VertexScanPlan::compile(pattern.plan()).unwrap();
    let mut total = 0;
    let mut baseline =
        VertexScanCursor::new(anonymous_collection_fixture(63), plan.clone(), policy(), || {
            total += 1;
            Ok::<_, usize>(())
        });
    let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = baseline.row_stats();
    let evaluator = baseline.evaluator_stats();
    drop(baseline);
    assert_eq!(expected.len(), 2);
    for stop in 1..=total {
        let fixture = anonymous_collection_fixture(63);
        let drops = fixture.drops.clone();
        let mut calls = 0;
        let mut cursor = VertexScanCursor::new(fixture, plan.clone(), policy(), || {
            calls += 1;
            if calls == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => {
                    assert_eq!(at, stop);
                    break;
                }
                other => panic!("anonymous normalization interruption became {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        let frozen = (cursor.row_stats(), cursor.evaluator_stats());
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), frozen);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(cursor);
        assert_eq!(calls, stop);
    }
    let run = |budget| {
        VertexScanCursor::new(
            anonymous_collection_fixture(63),
            plan.clone(),
            budget,
            || Ok::<_, usize>(()),
        )
    };
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    );
    let mut retry = run(exact);
    assert_eq!(retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    assert_eq!((retry.row_stats(), retry.evaluator_stats()), (rows, evaluator));
    for budget in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, evaluator.scratch_entries - 1),
    ] {
        let mut cursor = run(budget);
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(_)) => break,
                None => panic!("one-short anonymous collection budget succeeded"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let zero = anonymous_collection_pattern(GlaDirection::Forward, Some(0));
    let fixture = anonymous_collection_fixture(63);
    let roots = fixture.roots.clone();
    let seeks = fixture.seeks.clone();
    let mut cursor = VertexScanCursor::new(
        fixture,
        VertexScanPlan::compile(zero.plan()).unwrap(),
        GqlQueryPolicy::new(0, 0, 0, 0),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().is_none());
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(seeks.load(Ordering::SeqCst), 0);
}

#[test]
fn collection_order_uses_the_whole_plan_identity_profile() {
    use crate::algebra::{GraphColumn, GraphMatchClause, GraphPatternBuilder};

    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap();
    let mut anonymous = GraphPatternBuilder::new();
    anonymous.vertex("a").unwrap().vertex("b").unwrap();
    anonymous.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut identified = anonymous.clone();
    identified.capture_edge("r", 0).unwrap();
    let pattern = outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&anonymous, Some("b")),
                GraphMatchClause::collect(&identified, Some("r")),
            ],
            &[
                GraphColumn::vertex("id", "a"),
                GraphColumn::collection("neighbors", 0),
                GraphColumn::collection("edges", 1),
            ],
            0,
            Some(1),
        )
        .unwrap();
    assert!(pattern.plan().requires_identified_edges());
    let mut cursor = VertexScanCursor::new(
        anonymous_collection_fixture(63),
        VertexScanPlan::compile(pattern.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        cursor.next().unwrap().unwrap().values(),
        &[
            GraphValue::Vertex(VId(0)),
            collected_list([2, 1, 1, 0].map(|id| GraphValue::Vertex(VId(id)))),
            collected_list([0, 1, 2, 4].map(|id| GraphValue::Edge(EId(id)))),
        ],
    );
    assert!(cursor.next().is_none());
}

#[test]
fn anonymous_fallible_predicates_keep_their_typed_pre_source_refusal() {
    for page in ["", " LIMIT 0"] {
        let pattern = prepare(&format!(
            "MATCH (a) RETURN a, \
             [(a)-[:R]->(b) WHERE b.p / 0 = 1 | b] AS neighbors{page}"
        ));
        assert!(!pattern.plan().requires_identified_edges());
        let operator = pattern
            .plan()
            .operators()
            .iter()
            .position(|operator| matches!(operator, GlaOperator::Collect { .. }))
            .unwrap();
        assert_eq!(
            VertexScanPlan::compile(pattern.plan()).unwrap_err(),
            VertexScanBuildError { operator },
        );
    }
}

#[test]
fn collection_async_profiles_still_refuse_before_source_construction() {
    use crate::algebra::{GraphColumn, GraphMatchClause, GraphPatternBuilder};
    use crate::stream::{AsyncVertexProbePlan, AsyncVertexScanPlan};

    let pattern = collection_pattern(GlaDirection::Forward, Some(0));
    assert!(AsyncVertexScanPlan::compile(pattern.plan()).is_err());
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("a").unwrap().vertex("b").unwrap();
    inner.edge("a", R, GlaDirection::Forward, "b").unwrap();
    inner.capture_edge("r", 0).unwrap();
    let pattern = outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::exists(&inner),
                GraphMatchClause::collect(&inner, Some("b")),
            ],
            &[
                GraphColumn::vertex("id", "a"),
                GraphColumn::collection("neighbors", 1),
            ],
            0,
            Some(0),
        )
        .unwrap();
    assert!(VertexScanPlan::compile(pattern.plan()).is_ok());
    let operator = pattern
        .plan()
        .operators()
        .iter()
        .position(|operator| matches!(operator, GlaOperator::Collect { .. }))
        .unwrap();
    assert_eq!(
        AsyncVertexProbePlan::compile(pattern.plan()).unwrap_err(),
        VertexScanBuildError { operator },
    );
}

mod independent;
