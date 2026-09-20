//! Complete-product and interruption oracles for the indexed probe kernel.
use super::super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
struct Fixture {
    edges: BTreeMap<EId, (VId, VId, Vec<(PropertyKeyId, CanonicalScalar)>)>,
    vertices: BTreeSet<VId>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<EId>,
    roots: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    unavailable: bool,
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
            edges: BTreeMap::new(),
            vertices: BTreeSet::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            roots: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            unavailable: false,
            repeat: false,
            fail_edge: None,
        };
        for (eid, a, b) in edges {
            f.vertices.extend([VId(a), VId(b)]);
            f.outgoing.entry(VId(a)).or_default().insert(EId(eid));
            f.incoming.entry(VId(b)).or_default().insert(EId(eid));
            f.edges.insert(EId(eid), (VId(a), VId(b), Vec::new()));
        }
        f
    }
    fn small(mask: usize) -> Self {
        Self::from_edges(
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
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, e)| e),
        )
    }
}
impl EdgeScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(13)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if next.is_some() {
            self.after = next;
        }
        Ok(next)
    }
    fn edge<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail_edge == Some(id) {
            return Err(EdgeScanSourceError::Source("read denied"));
        }
        Ok(self.edges.get(&id).map(|(a, b, p)| EdgeScanRow {
            source: *a,
            target: *b,
            relation: R,
            properties: p,
        }))
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.contains(&id).then_some(VertexScanRow {
            labels: &[],
            properties: &[],
        }))
    }
    fn next_incident_edge<C>(
        &self,
        at: VId,
        d: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        if self.repeat && after.is_some() {
            return Ok(after);
        }
        let seek = |face: &BTreeMap<VId, BTreeSet<EId>>| {
            face.get(&at).and_then(|set| {
                set.range((after.map_or(Unbounded, Excluded), Unbounded))
                    .next()
                    .copied()
            })
        };
        Ok(match d {
            GlaDirection::Forward => seek(&self.outgoing),
            GlaDirection::Reverse => seek(&self.incoming),
            GlaDirection::Undirected => match (seek(&self.outgoing), seek(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        })
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
    GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000)
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}
fn atom(edge: &str, end: &str, d: GlaDirection) -> String {
    match d {
        GlaDirection::Forward => format!("-[{edge}:R]->({end})"),
        GlaDirection::Reverse => format!("<-[{edge}:R]-({end})"),
        GlaDirection::Undirected => format!("-[{edge}:R]-({end})"),
    }
}

fn body(shape: usize, direction: GlaDirection, nullable: bool) -> String {
    let first = format!("(b){}", atom("", "x", direction));
    let tail = match shape {
        1 => atom("", "a", direction),
        2 => format!(", (b){}", atom("", "a", direction)),
        _ => String::new(),
    };
    let condition = if nullable { "NOT (x.p = 7)" } else { "x <> a" };
    format!("MATCH {first}{tail} WHERE {condition}")
}

fn statement(
    shape: usize,
    dirs: [GlaDirection; 2],
    anti: bool,
    nullable: bool,
    distinct: bool,
    skip: usize,
    limit: usize,
) -> String {
    format!(
        "MATCH (a){} WHERE {}EXISTS {{ {} }} RETURN {}r, a, b SKIP {skip} LIMIT {limit}",
        atom("r", "b", dirs[0]),
        if anti { "NOT " } else { "" },
        body(shape, dirs[1], nullable),
        if distinct { "DISTINCT " } else { "" }
    )
}

fn orientations(a: VId, b: VId, d: GlaDirection) -> Vec<(VId, VId)> {
    match d {
        GlaDirection::Forward => vec![(a, b)],
        GlaDirection::Reverse => vec![(b, a)],
        GlaDirection::Undirected if a == b => vec![(a, b)],
        _ => vec![(a, b), (b, a)],
    }
}

// Enumerate complete inner relation products independently, then collapse to
// existence for each outer occurrence. Do not reuse the index or DFS cursor.
fn expected(
    f: &Fixture,
    shape: usize,
    dirs: [GlaDirection; 2],
    anti: bool,
    nullable: bool,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphValue>> {
    let inner: Vec<_> = f
        .edges
        .values()
        .flat_map(|(a, b, _)| orientations(*a, *b, dirs[1]))
        .collect();
    let mut rows = Vec::new();
    for (&eid, (from, to, _)) in &f.edges {
        for (a, b) in orientations(*from, *to, dirs[0]) {
            let mut witnesses = 0;
            for &(start, x) in &inner {
                // All fixture vertex properties are absent. NOT(NULL = 7)
                // is UNKNOWN, not TRUE and not a witness.
                if start != b || nullable || x == a {
                    continue;
                }
                if shape == 0 {
                    witnesses += 1;
                    continue;
                }
                for &(second, last) in &inner {
                    if second == (if shape == 1 { x } else { b }) && last == a {
                        witnesses += 1;
                    }
                }
            }
            if (witnesses != 0) != anti {
                rows.push(vec![
                    GraphValue::Edge(eid),
                    GraphValue::Vertex(a),
                    GraphValue::Vertex(b),
                ]);
            }
        }
    }
    rows.sort();
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn correlated_semi_and_anti_joins_match_complete_products_and_eager_gla() {
    let directions = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    for mask in [0, 10, 31, 63] {
        for outer in directions {
            for inner in directions {
                for shape in 0..3 {
                    for anti in [false, true] {
                        for nullable in [false, true] {
                            for distinct in [false, true] {
                                for (skip, limit) in [(0, 100), (1, 3), (0, 0)] {
                                    let text = statement(
                                        shape,
                                        [outer, inner],
                                        anti,
                                        nullable,
                                        distinct,
                                        skip,
                                        limit,
                                    );
                                    let q = prepare(&text);
                                    let f = Fixture::small(mask);
                                    let want = expected(
                                        &f,
                                        shape,
                                        [outer, inner],
                                        anti,
                                        nullable,
                                        skip,
                                        limit,
                                    );
                                    let eager = q
                                        .plan()
                                        .execute_governed_with_element_properties(
                                            f.edges.len() as u64,
                                            f.vertices.iter().copied(),
                                            f.edges
                                                .iter()
                                                .map(|(&eid, (a, b, _))| (eid, *a, R, *b)),
                                            |_, _| Ok::<_, ()>(true),
                                            |_, _| Ok(None),
                                            |_, _| Ok(None),
                                            policy(),
                                            || Ok::<_, ()>(()),
                                        )
                                        .unwrap();
                                    assert_eq!(plain(&eager.value), want, "eager {text}");
                                    let roots = f.roots.clone();
                                    let mut cursor = EdgeScanCursor::new(
                                        f,
                                        EdgeScanPlan::compile(q.plan()).unwrap(),
                                        policy(),
                                        || Ok::<_, ()>(()),
                                    );
                                    assert_eq!(roots.load(Ordering::SeqCst), 0);
                                    let mut rows = Vec::new();
                                    for page in [1, 2, 128] {
                                        for _ in 0..page {
                                            let Some(row) = cursor.next() else {
                                                break;
                                            };
                                            rows.push(row.unwrap());
                                        }
                                    }
                                    assert_eq!(plain(&rows), want, "stream {text}");
                                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                                    assert!(cursor.next().is_none());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn first_complete_witness_stops_before_a_bad_suffix_but_failed_probes_are_not_absence() {
    let input = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(x) } RETURN r, a, b LIMIT 1";
    let q = prepare(input);
    let mut f = Fixture::from_edges([(0, 0, 1), (1, 1, 2), (2, 1, 3)]);
    f.repeat = true;
    let seeks = f.seeks.clone();
    let mut cursor = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        GqlQueryPolicy::new(2, 1, 100_000, 10_000),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        cursor.next().unwrap().unwrap().values()[0],
        GraphValue::Edge(EId(0))
    );
    assert_eq!(cursor.row_stats().snapshot_records, 2);
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);

    let q = prepare(
        &input
            .replace("WHERE EXISTS", "WHERE NOT EXISTS")
            .replace("MATCH (b)-[:R]->(x)", "MATCH (b)-[:R]->(x) WHERE x = a"),
    );
    let mut f = Fixture::from_edges([(0, 0, 1), (1, 1, 2), (2, 1, 3)]);
    f.repeat = true;
    let mut cursor = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            EdgeScanError::NonIncreasingIdentity
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.next().is_none());
    let mut f = Fixture::small(63);
    f.unavailable = true;
    let mut cursor = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            EdgeScanError::ExpansionUnavailable
        )))
    ));
}

#[test]
fn every_probe_checkpoint_fuses_once_preserves_delivered_prefix_and_retries() {
    for anti in [false, true] {
        let q = prepare(&statement(
            1,
            [GlaDirection::Undirected; 2],
            anti,
            false,
            false,
            0,
            100,
        ));
        let mut calls = 0;
        let mut good = EdgeScanCursor::new(
            Fixture::small(63),
            EdgeScanPlan::compile(q.plan()).unwrap(),
            policy(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        );
        let want = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        drop(good);
        for stop in 1..=calls {
            let mut at = 0;
            let f = Fixture::small(63);
            let drops = f.drops.clone();
            let mut cursor = EdgeScanCursor::new(
                f,
                EdgeScanPlan::compile(q.plan()).unwrap(),
                policy(),
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(n))) => {
                        assert_eq!(n, stop);
                        break;
                    }
                    other => panic!("interrupted probe became {other:?}"),
                }
            }
            assert_eq!(prefix, want[..prefix.len()]);
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.traversal.is_none());
            assert!(cursor.next().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(cursor);
            assert_eq!(at, stop);
            let retry = EdgeScanCursor::new(
                Fixture::small(63),
                EdgeScanPlan::compile(q.plan()).unwrap(),
                policy(),
                || Ok::<_, usize>(()),
            )
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
            assert_eq!(retry, want);
        }
    }
}

#[test]
fn probe_work_and_candidate_records_share_the_outer_query_limits() {
    let q = prepare(&statement(
        2,
        [GlaDirection::Undirected; 2],
        false,
        false,
        false,
        0,
        3,
    ));
    let run = |p| {
        EdgeScanCursor::new(
            Fixture::small(63),
            EdgeScanPlan::compile(q.plan()).unwrap(),
            p,
            || Ok::<_, ()>(()),
        )
    };
    let mut good = run(policy());
    let want = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = good.row_stats();
    let e = good.evaluator_stats();
    assert!(!want.is_empty());
    assert!(r.snapshot_records > r.result_rows);
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    let mut again = run(exact);
    assert_eq!(again.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), want);
    assert_eq!((again.row_stats(), again.evaluator_stats()), (r, e));
    for p in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 100, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, 100, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(1000, 100, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut c = run(p);
        assert!(c.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(c.state(), EdgeScanState::Failed);
        assert!(c.next().is_none());
    }
}

#[test]
fn a_deep_probe_finds_one_of_exponentially_many_witnesses_with_linear_demand() {
    for hops in [40, MAX_PATTERN_EDGES - 1] {
        let mut text = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)".to_owned();
        for i in 0..hops {
            text.push_str(&format!("-[:R]->(v{})", i + 1));
        }
        text.push_str(" } RETURN r, a, b LIMIT 1");
        let q = prepare(&text);
        let edges = std::iter::once((0, 1000, 0))
            .chain((0..hops as u128).flat_map(|i| [(i * 2 + 1, i, i + 1), (i * 2 + 2, i, i + 1)]));
        let f = Fixture::from_edges(edges);
        let roots = f.roots.clone();
        let seeks = f.seeks.clone();
        let mut cursor = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(hops as u64 + 1, 1, 100_000, 10_000),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            cursor.next().unwrap().unwrap().values()[0],
            GraphValue::Edge(EId(0))
        );
        assert_eq!(cursor.row_stats().snapshot_records, hops as u64 + 1);
        assert_eq!(roots.load(Ordering::SeqCst), 1);
        assert_eq!(seeks.load(Ordering::SeqCst), hops);
        assert!(cursor.next().is_none());
        cursor.close();
        assert_eq!(seeks.load(Ordering::SeqCst), hops);
    }
}

#[test]
fn finite_atoms_do_not_read_a_limit_zero_source() {
    for body in ["MATCH (x)-[:R*1..2]->(y)", "MATCH (b)-[:R*1..2]->(x)"] {
        let text = format!("MATCH (a)-[r:R]->(b) WHERE EXISTS {{ {body} }} RETURN r, a, b LIMIT 0");
        let q = prepare(&text);
        let f = Fixture::small(63);
        let seeks = f.seeks.clone();
        let roots = f.roots.clone();
        let mut c = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(q.plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(c.next().is_none());
        assert_eq!(seeks.load(Ordering::SeqCst), 0);
        assert_eq!(roots.load(Ordering::SeqCst), 0);
    }
    // A correlated zero-edge probe copies a live vertex; no incidence scan.
    let q = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b) } RETURN r, a, b LIMIT 1");
    let f = Fixture::small(63);
    let seeks = f.seeks.clone();
    let mut c = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(c.next().unwrap().is_ok());
    assert_eq!(seeks.load(Ordering::SeqCst), 0);
}

#[test]
fn a_source_error_in_a_required_probe_is_not_an_anti_join_witness() {
    let text = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R]->(x) WHERE x = a } RETURN r, a, b LIMIT 1";
    let q = prepare(text);
    let mut f = Fixture::from_edges([(0, 0, 1), (1, 1, 2), (2, 1, 3)]);
    f.fail_edge = Some(EId(2));
    let mut cursor = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "read denied"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.next().is_none());
}
