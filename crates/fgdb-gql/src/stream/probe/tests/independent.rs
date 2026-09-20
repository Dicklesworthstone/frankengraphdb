//! Independent and capture-only scopes use the same compiled probe engine.
use super::*;
use crate::edge_stream::{EdgeScanCursor, EdgeScanPlan};

struct Independent {
    source: Fixture,
    candidates: BTreeSet<VId>,
    scans: Arc<AtomicUsize>,
    repeat: bool,
    fail: Option<VId>,
}
impl Independent {
    fn new(mask: usize) -> Self {
        let source = Fixture::small(mask);
        let mut candidates = source.vertices.clone();
        candidates.insert(VId(4)); // A historical candidate with no visible row.
        Self {
            source,
            candidates,
            scans: Arc::new(AtomicUsize::new(0)),
            repeat: false,
            fail: None,
        }
    }
}
impl VertexScanSource for Independent {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        self.source.snapshot_seq()
    }
    fn next_vertex<C>(
        &mut self,
        c: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        self.source.next_vertex(c)
    }
    fn vertex<'a, C>(
        &'a self,
        v: VId,
        c: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        if self.fail == Some(v) {
            return Err(VertexScanSourceError::Source("vertex refused"));
        }
        self.source.vertex(v, c)
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        c(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        self.scans.fetch_add(1, Ordering::SeqCst);
        if self.repeat && after.is_some() {
            return Ok(after);
        }
        Ok(self
            .candidates
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .copied())
    }
    fn next_probe_edge<C>(
        &self,
        v: VId,
        d: GlaDirection,
        after: Option<EId>,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.source.next_probe_edge(v, d, after, c)
    }
    fn probe_edge<'a, C>(
        &'a self,
        e: EId,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        self.source.probe_edge(e, c)
    }
}

// Exactly the same admitted fixture can serve an edge-rooted outer query.
struct Edges {
    source: Independent,
    after: Option<EId>,
}
impl EdgeScanSource for Edges {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        self.source.snapshot_seq()
    }
    fn next_edge<C>(
        &mut self,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        c(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self
            .source
            .source
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if next.is_some() {
            self.after = next;
        }
        Ok(next)
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.source.next_probe_vertex(after, c)
    }
    fn next_incident_edge<C>(
        &self,
        v: VId,
        d: GlaDirection,
        after: Option<EId>,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.source.next_probe_edge(v, d, after, c)
    }
    fn vertex<'a, C>(
        &'a self,
        v: VId,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.source.vertex(v, &mut |event| c(edge_event(event)))
    }
    fn edge<'a, C>(
        &'a self,
        e: EId,
        c: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.source.probe_edge(e, c).map_err(|error| match error {
            EdgeExpansionSourceError::Read(error) => error,
            EdgeExpansionSourceError::Unavailable => {
                EdgeScanSourceError::Source("fixture lacks edge reader")
            }
        })
    }
}
fn body(shape: usize, d: GlaDirection) -> String {
    let edge = |name: &str| match d {
        GlaDirection::Forward => format!("-[:R]->({name})"),
        GlaDirection::Reverse => format!("<-[:R]-({name})"),
        GlaDirection::Undirected => format!("-[:R]-({name})"),
    };
    match shape {
        0 => "MATCH (x) WHERE x.p > 5".into(),
        1 => format!("MATCH (x){} WHERE y.p > 5", edge("y")),
        2 => format!("MATCH (x){}{}", edge("y"), edge("x")),
        _ => format!("MATCH (x){} WHERE x.p = a.p", edge("y")),
    }
}
fn input(shape: usize, d: GlaDirection, anti: bool, edge: bool, skip: u64, count: u64) -> String {
    format!(
        "MATCH {} WHERE {}EXISTS {{ {} }} RETURN {} SKIP {skip} LIMIT {count}",
        if edge { "(a)-[r:R]->(b)" } else { "(a)" },
        if anti { "NOT " } else { "" },
        body(shape, d),
        if edge { "r, a, b" } else { "a" }
    )
}
// Deliberately form the whole inner relation independently of the production
// cursor and only then count qualifying witnesses. No first-witness shortcut.
fn witnesses(f: &Fixture, shape: usize, d: GlaDirection, root: VId) -> usize {
    let pairs: Vec<_> = f
        .edges
        .values()
        .flat_map(|&(a, b)| match d {
            GlaDirection::Forward => vec![(a, b)],
            GlaDirection::Reverse => vec![(b, a)],
            GlaDirection::Undirected if a != b => vec![(a, b), (b, a)],
            _ => vec![(a, b)],
        })
        .collect();
    match shape {
        0 => f
            .vertices
            .iter()
            .filter(|v| matches!(f.value(**v), CanonicalScalar::Int(n) if n > 5))
            .count(),
        1 => pairs
            .iter()
            .filter(|(_, b)| matches!(f.value(*b), CanonicalScalar::Int(n) if n > 5))
            .count(),
        2 => pairs
            .iter()
            .flat_map(|&(a, b)| pairs.iter().filter(move |&&(x, y)| x == b && y == a))
            .count(),
        _ => pairs
            .iter()
            .filter(|(a, _)| match (f.value(*a), f.value(root)) {
                (CanonicalScalar::Int(a), CanonicalScalar::Int(b)) => a == b,
                _ => false,
            })
            .count(),
    }
}

#[test]
fn independent_scopes_match_complete_relations_and_eager_gla_on_both_outer_roots() {
    for mask in 0..64 {
        for shape in 0..4 {
            for d in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for anti in [false, true] {
                    for edge in [false, true] {
                        for (skip, count) in [(0, 20), (1, 2), (0, 0)] {
                            let q = prepare(&input(shape, d, anti, edge, skip, count));
                            let source = Independent::new(mask);
                            let f = &source.source;
                            let mut expected: Vec<Vec<GraphValue>> = if edge {
                                f.edges
                                    .iter()
                                    .filter(|(_, (a, _))| (witnesses(f, shape, d, *a) > 0) != anti)
                                    .map(|(&e, &(a, b))| {
                                        vec![
                                            GraphValue::Edge(e),
                                            GraphValue::Vertex(a),
                                            GraphValue::Vertex(b),
                                        ]
                                    })
                                    .collect()
                            } else {
                                f.vertices
                                    .iter()
                                    .filter(|&&a| (witnesses(f, shape, d, a) > 0) != anti)
                                    .map(|&a| vec![GraphValue::Vertex(a)])
                                    .collect()
                            };
                            expected.sort();
                            let expected: Vec<_> = expected
                                .into_iter()
                                .skip(skip as usize)
                                .take(count as usize)
                                .collect();
                            let eager = q
                                .plan()
                                .execute_governed_with_element_properties(
                                    3 + f.edges.len() as u64,
                                    f.vertices.iter().copied(),
                                    f.edges.iter().map(|(&eid, &(a, b))| (eid, a, R, b)),
                                    |vid, tests| {
                                        Ok::<_, ()>(tests.iter().all(|t| {
                                            t.matches(
                                                &[],
                                                f.props.get(&vid).map_or(&[], Vec::as_slice),
                                            )
                                        }))
                                    },
                                    |vid, _| {
                                        Ok(f.props
                                            .get(&vid)
                                            .and_then(|p| p.first())
                                            .map(|(_, v)| v))
                                    },
                                    |_, _| Ok(None),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                )
                                .unwrap();
                            assert_eq!(plain(&eager.value), expected);
                            let actual = if edge {
                                EdgeScanCursor::new(
                                    Edges {
                                        source,
                                        after: None,
                                    },
                                    EdgeScanPlan::compile(q.plan()).unwrap(),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                )
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap()
                            } else {
                                VertexScanCursor::new(
                                    source,
                                    VertexScanPlan::compile(q.plan()).unwrap(),
                                    policy(),
                                    || Ok::<_, ()>(()),
                                )
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap()
                            };
                            assert_eq!(
                                plain(&actual),
                                expected,
                                "shape={shape} direction={d:?} anti={anti} edge={edge}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn unavailable_bad_order_and_failed_reads_are_not_successful_absence() {
    let q = prepare("MATCH (a) WHERE NOT EXISTS { MATCH (x) WHERE x.p > 100 } RETURN a");
    let plan = VertexScanPlan::compile(q.plan()).unwrap();
    let mut missing =
        VertexScanCursor::new(NoLookup(Fixture::small(0)), plan.clone(), policy(), || {
            Ok::<_, ()>(())
        });
    assert!(matches!(
        missing.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Probe(
            EdgeScanError::ExpansionUnavailable
        ))))
    ));
    for repeat in [true, false] {
        let mut f = Independent::new(0);
        f.repeat = repeat;
        if !repeat {
            f.fail = Some(VId(1));
        }
        let drops = f.source.drops.clone();
        let mut c = VertexScanCursor::new(f, plan.clone(), policy(), || Ok::<_, ()>(()));
        let err = c.next().unwrap().unwrap_err();
        if repeat {
            assert!(matches!(
                err,
                GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::NonIncreasingIdentity))
            ));
        } else {
            assert!(matches!(
                err,
                GqlQueryError::Source(VertexScanError::Source("vertex refused"))
            ));
        }
        assert_eq!(c.row_stats().result_rows, 0);
        assert_eq!(c.state(), VertexScanState::Failed);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(c.next().is_none());
    }
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (x) WHERE x.p IS NULL } RETURN a");
    let mut f = Independent::new(0);
    f.source.props.values_mut().for_each(|p| p.clear());
    // Leave only a visible non-null outer row and invisible scan candidates.
    f.source.vertices = BTreeSet::from([VId(0)]);
    f.source
        .props
        .insert(VId(0), vec![(P, CanonicalScalar::Int(1))]);
    let c = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(c.collect::<Result<Vec<_>, _>>().unwrap().is_empty());
}

#[test]
fn first_witness_limit_zero_and_sequential_scopes_do_not_move_outer_positions() {
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (x) } RETURN a LIMIT 1");
    let mut f = Independent::new(0);
    f.repeat = true;
    f.fail = Some(VId(1));
    let roots = f.source.roots.clone();
    let scans = f.scans.clone();
    let mut c = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        GqlQueryPolicy::new(2, 1, 1000, 1000),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        c.next().unwrap().unwrap().values(),
        &[GraphValue::Vertex(VId(0))]
    );
    assert_eq!(c.row_stats().snapshot_records, 2);
    assert!(c.next().is_none());
    assert_eq!(roots.load(Ordering::SeqCst), 1);
    assert_eq!(scans.load(Ordering::SeqCst), 1);
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (x) } RETURN a LIMIT 0");
    let mut c = VertexScanCursor::new(
        NoLookup(Fixture::small(0)),
        VertexScanPlan::compile(q.plan()).unwrap(),
        GqlQueryPolicy::new(0, 0, 10, 0),
        || Ok::<_, ()>(()),
    );
    assert!(c.next().is_none());
    assert_eq!(c.row_stats().snapshot_records, 0);
    let q = prepare(
        "MATCH (a) WHERE EXISTS { MATCH (x) WHERE x.p > 5 } AND NOT EXISTS { MATCH (x) WHERE x.p = 99 } RETURN a",
    );
    let f = Independent::new(0);
    let expected = f
        .source
        .vertices
        .iter()
        .map(|&a| vec![GraphValue::Vertex(a)])
        .collect::<Vec<_>>();
    let c = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(q.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert_eq!(plain(&c.collect::<Result<Vec<_>, _>>().unwrap()), expected);
}

#[test]
fn every_independent_scan_checkpoint_refuses_once_and_preserves_the_delivered_prefix() {
    for anti in [false, true] {
        let q = prepare(&input(3, GlaDirection::Forward, anti, false, 0, 20));
        let plan = VertexScanPlan::compile(q.plan()).unwrap();
        let mut total = 0;
        let mut good = VertexScanCursor::new(Independent::new(63), plan.clone(), policy(), || {
            total += 1;
            Ok::<_, usize>(())
        });
        let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        drop(good);
        for stop in 1..=total {
            let mut calls = 0;
            let f = Independent::new(63);
            let drops = f.source.drops.clone();
            let mut c = VertexScanCursor::new(f, plan.clone(), policy(), || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            let mut rows = Vec::new();
            loop {
                match c.next() {
                    Some(Ok(row)) => rows.push(row),
                    Some(Err(GqlQueryError::Interrupted(at))) => {
                        assert_eq!(at, stop);
                        break;
                    }
                    other => panic!("refusal became {other:?}"),
                }
            }
            assert_eq!(rows, expected[..rows.len()]);
            assert_eq!(c.row_stats().result_rows, rows.len() as u64);
            assert_eq!(c.state(), VertexScanState::Failed);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(c.next().is_none());
            drop(c);
            assert_eq!(calls, stop);
            let retry = VertexScanCursor::new(Independent::new(63), plan.clone(), policy(), || {
                Ok::<_, usize>(())
            });
            assert_eq!(retry.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        }
    }
}

#[test]
fn root_vertex_edge_and_repeat_probe_costs_share_exact_limits() {
    let q = prepare(&input(3, GlaDirection::Forward, false, false, 0, 20));
    let plan = VertexScanPlan::compile(q.plan()).unwrap();
    let run = |p| VertexScanCursor::new(Independent::new(63), plan.clone(), p, || Ok::<_, ()>(()));
    let mut c = run(policy());
    let expected = c.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    assert!(!expected.is_empty());
    let r = c.row_stats();
    let e = c.evaluator_stats();
    assert!(r.snapshot_records > 10);
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
        let mut failed = run(p);
        assert!(failed.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(failed.state(), VertexScanState::Failed);
        assert!(failed.next().is_none());
    }
}

#[test]
fn independent_deep_first_witness_does_not_enumerate_the_complete_inner_relation() {
    for hops in [40, crate::algebra::MAX_PATTERN_EDGES] {
        let mut input = "MATCH (a) WHERE EXISTS { MATCH (x0)".to_owned();
        for i in 1..=hops {
            input.push_str(&format!("-[:R]->(x{i})"));
        }
        input.push_str(" } RETURN a LIMIT 1");
        let q = prepare(&input);
        let source = Fixture::from_edges(
            (0..hops as u128).flat_map(|i| [(2 * i, i, i + 1), (2 * i + 1, i, i + 1)]),
        );
        let f = Independent {
            candidates: source.vertices.clone(),
            source,
            scans: Arc::new(AtomicUsize::new(0)),
            repeat: false,
            fail: None,
        };
        let scans = f.scans.clone();
        let seeks = f.source.seeks.clone();
        let mut c = VertexScanCursor::new(
            f,
            VertexScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(hops as u64 + 2, 1, 100_000, 10_000),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            c.next().unwrap().unwrap().values(),
            &[GraphValue::Vertex(VId(0))]
        );
        assert_eq!(c.row_stats().snapshot_records, hops as u64 + 2);
        assert_eq!(scans.load(Ordering::SeqCst), 1);
        assert_eq!(seeks.load(Ordering::SeqCst), hops);
        assert!(c.next().is_none());
    }
}
