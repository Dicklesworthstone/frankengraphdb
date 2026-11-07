use super::*;
use crate::algebra::{GlaPlan, GraphValue, GraphValueRow, VertexPredicate};
use crate::stream::VertexScanPlan;
use crate::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};
use std::rc::Rc;

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const MODES: [GraphWalkSearch; 6] = [
    GraphWalkSearch::All,
    GraphWalkSearch::AllShortest,
    GraphWalkSearch::AnyShortest,
    GraphWalkSearch::Trail,
    GraphWalkSearch::Acyclic,
    GraphWalkSearch::Simple,
];
struct Source {
    edges: BTreeMap<EId, (VId, VId)>,
    ids: BTreeSet<VId>,
    properties: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<EId>,
    bad: Option<EId>,
    repeat: bool,
    unavailable: bool,
    records: Rc<Cell<u64>>,
}
impl Source {
    fn new(edges: impl IntoIterator<Item = (u128, u128, u128)>) -> Self {
        let mut s = Self {
            edges: BTreeMap::new(),
            ids: (0..4).map(VId).collect(),
            properties: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            bad: None,
            repeat: false,
            unavailable: false,
            records: Rc::new(Cell::new(0)),
        };
        for (eid, a, b) in edges {
            let (eid, a, b) = (EId(eid), VId(a), VId(b));
            s.edges.insert(eid, (a, b));
            s.ids.extend([a, b]);
            s.outgoing.entry(a).or_default().insert(eid);
            s.incoming.entry(b).or_default().insert(eid);
        }
        s.properties
            .insert(VId(0), vec![(P, CanonicalScalar::Int(1).into())]);
        s.properties
            .insert(VId(1), vec![(P, CanonicalScalar::Null.into())]);
        s.properties
            .insert(VId(2), vec![(P, CanonicalScalar::Int(7).into())]);
        s
    }
    fn small(mask: usize) -> Self {
        Self::new(
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
        )
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(1)
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
            .map(|(id, _)| *id);
        if next.is_some() {
            self.after = next;
        }
        Ok(next)
    }
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.bad == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge refused"));
        }
        Ok(self.edges.get(&eid).map(|&(source, target)| EdgeScanRow {
            source,
            target,
            relation: R,
            properties: &[],
        }))
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.ids.contains(&vid).then_some(VertexScanRow {
            labels: &[],
            properties: self.properties.get(&vid).map_or(&[], Vec::as_slice),
        }))
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|error| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error)))?;
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        Ok(self
            .ids
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .copied())
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        if self.repeat && after.is_some() {
            return Ok(after);
        }
        let next = |map: &BTreeMap<VId, BTreeSet<EId>>| {
            map.get(&endpoint).and_then(|s| {
                s.range((after.map_or(Unbounded, Excluded), Unbounded))
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
}
fn expansion(direction: GlaDirection) -> Expansion {
    Expansion {
        source: 0,
        relation: R.into(),
        direction,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

// Enumerate all bounded walks, then apply whole-route laws. This oracle has no
// depth-state pruning, active membership sets, endpoint caching or index seeks.
fn oracle(
    s: &Source,
    start: VId,
    d: GlaDirection,
    lo: u32,
    hi: u32,
    mode: GraphWalkSearch,
) -> BTreeSet<VId> {
    let mut layer = vec![(vec![start], Vec::<EId>::new())];
    let mut endpoints = BTreeSet::new();
    for depth in 0..=hi {
        if depth >= lo {
            for (vs, es) in &layer {
                let valid = match mode {
                    GraphWalkSearch::Trail => es.iter().collect::<BTreeSet<_>>().len() == es.len(),
                    GraphWalkSearch::Acyclic => {
                        vs.iter().collect::<BTreeSet<_>>().len() == vs.len()
                    }
                    GraphWalkSearch::Simple => {
                        let n = vs.len() - usize::from(vs.len() > 1 && vs.last() == Some(&start));
                        vs[..n].iter().collect::<BTreeSet<_>>().len() == n
                    }
                    _ => true,
                };
                if valid {
                    endpoints.insert(*vs.last().unwrap());
                }
            }
        }
        if depth == hi {
            break;
        }
        let mut next = Vec::new();
        for (vs, es) in layer {
            let from = *vs.last().unwrap();
            for (&eid, &(a, b)) in &s.edges {
                let mut to = Vec::new();
                if d != GlaDirection::Reverse && a == from {
                    to.push(b);
                }
                if d != GlaDirection::Forward
                    && b == from
                    && (d != GlaDirection::Undirected || a != b)
                {
                    to.push(a);
                }
                for to in to {
                    let mut vs = vs.clone();
                    vs.push(to);
                    let mut es = es.clone();
                    es.push(eid);
                    next.push((vs, es));
                }
            }
        }
        layer = next;
    }
    endpoints
}
fn collect(
    s: &Source,
    start: VId,
    d: GlaDirection,
    lo: u32,
    hi: u32,
    mode: GraphWalkSearch,
) -> BTreeSet<VId> {
    let mut control = |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(());
    let mut q = Endpoints::new(
        start,
        GraphWalkBounds::new(lo, hi).unwrap(),
        mode,
        s,
        &mut control,
    )
    .unwrap();
    let mut answers = BTreeSet::new();
    while let Some(v) = q
        .next(expansion(d), s, &mut control, &mut || {
            s.records.set(s.records.get() + 1);
            Ok(())
        })
        .unwrap()
    {
        assert!(answers.insert(v), "duplicate endpoint support");
    }
    answers
}
#[test]
fn all_six_modes_match_complete_walk_support_for_every_small_multigraph_interval_and_direction() {
    for mask in 0..64 {
        let s = Source::small(mask);
        for mode in MODES {
            for d in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for start in (0..4).map(VId) {
                    for hi in 0..=3 {
                        for lo in 0..=hi {
                            assert_eq!(
                                collect(&s, start, d, lo, hi, mode),
                                oracle(&s, start, d, lo, hi, mode),
                                "mask={mask}, mode={mode:?}, lo={lo}, hi={hi}"
                            );
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn cycles_lower_bounds_closure_and_atom_local_history_do_not_become_vertex_settlement() {
    let s = Source::new([(1, 0, 1), (2, 1, 0), (3, 1, 2)]);
    for mode in [
        GraphWalkSearch::All,
        GraphWalkSearch::AllShortest,
        GraphWalkSearch::AnyShortest,
    ] {
        assert!(collect(&s, VId(0), GlaDirection::Forward, 4, 4, mode).contains(&VId(2)));
    }
    let s = Source::new([(u128::MAX, 0, 1)]);
    assert!(
        collect(
            &s,
            VId(0),
            GlaDirection::Undirected,
            2,
            3,
            GraphWalkSearch::Trail
        )
        .is_empty()
    );
    assert!(
        collect(
            &s,
            VId(0),
            GlaDirection::Undirected,
            2,
            3,
            GraphWalkSearch::Acyclic
        )
        .is_empty()
    );
    assert_eq!(
        collect(
            &s,
            VId(0),
            GlaDirection::Undirected,
            2,
            3,
            GraphWalkSearch::Simple
        ),
        BTreeSet::from([VId(0)])
    );
    assert!(
        collect(
            &s,
            VId(0),
            GlaDirection::Undirected,
            3,
            4,
            GraphWalkSearch::Simple
        )
        .is_empty()
    );
    let s = Source::new([
        (1, 0, 1),
        (2, 1, 3),
        (3, 0, 2),
        (4, 2, 3),
        (5, 3, 1),
        (6, 1, 4),
    ]);
    // The first prefix to 3 has already used 1; the other prefix has not.
    assert!(
        collect(
            &s,
            VId(0),
            GlaDirection::Forward,
            4,
            4,
            GraphWalkSearch::Acyclic
        )
        .contains(&VId(4))
    );
}
fn plan(input: &str, mode: GraphWalkSearch) -> GlaPlan<GraphValueRow> {
    // This suite varies each atom's search mode independently and verifies
    // endpoint-support composition. Declare clause repeatability explicitly;
    // endpoint-only cursors cannot enforce cross-atom DifferentEdges domains.
    let input = input.replace("MATCH ", "MATCH REPEATABLE ELEMENTS ");
    let q = PreparedGraphText::prepare(&input, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    // Exercise all public search semantics on the compiler's unchanged scope,
    // binding and projection layout. No AST execution is used by either path.
    let mut ops = q.plan().operators().to_vec();
    for op in &mut ops {
        if let GlaOperator::VarLengthExpand { search, .. } = op {
            *search = mode;
        }
    }
    GlaPlan::from_operators(ops)
}
fn eager(plan: &GlaPlan<GraphValueRow>, s: &Source) -> Vec<GraphValueRow> {
    plan.execute_governed_with_element_properties(
        (s.ids.len() + s.edges.len()) as u64,
        s.ids.iter().copied(),
        s.edges.iter().map(|(&id, &(a, b))| (id, a, R, b)),
        |v, predicates: &[VertexPredicate]| {
            Ok::<_, ()>(predicates.iter().all(|p| {
                p.matches_borrowed(
                    [],
                    s.properties
                        .get(&v)
                        .into_iter()
                        .flatten()
                        .map(|(k, v)| (*k, v)),
                )
            }))
        },
        |v, k| {
            Ok(s.properties
                .get(&v)
                .and_then(|p| p.iter().find(|(key, _)| *key == k))
                .map(|(_, v)| v))
        },
        |_, _| Ok(None),
        wide(),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}

#[test]
fn different_edges_variable_probes_use_native_trails_and_refuse_cross_atom_intake() {
    let input = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R*2..2]-(x) } RETURN r,a,b";
    let prepared = PreparedGraphText::prepare(input, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    // The genuine identified executor still owns this logical query. A lone
    // edge cannot form a two-hop trail, whereas two parallel identities can.
    let single = Source::new([(1, 1, 2)]);
    assert!(eager(prepared.plan(), &single).is_empty());
    let parallel = Source::new([(1, 1, 2), (2, 1, 2)]);
    let expected = eager(prepared.plan(), &parallel);
    assert_eq!(expected.len(), 2);
    let streamed = EdgeScanCursor::new(
        parallel,
        EdgeScanPlan::compile(prepared.plan()).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(streamed, expected);
    let cross_atom = input.replace("[:R*2..2]-(x)", "[:R*1..1]-(x)-[:R*1..1]-(b)");
    let prepared = PreparedGraphText::prepare(&cross_atom, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(EdgeScanPlan::compile(prepared.plan()).is_err());
    let parallel = Source::new([(1, 1, 2), (2, 1, 2)]);
    assert_eq!(eager(prepared.plan(), &parallel).len(), 2);
}

#[test]
fn variable_atoms_compose_with_fixed_edges_branches_predicates_and_backtracking_in_both_stream_lanes()
 {
    let bodies = [
        "MATCH (b)-[:R*0..3]->(x) WHERE x.p > 5",
        "MATCH (b)-[:R*1..2]->(x)-[:R*0..2]->(y) WHERE y = a",
        "MATCH (b)-[:R*0..2]->(x), (b)-[:R*1..2]->(y) WHERE x <> y",
        "MATCH (b)-[:R*0..2]->(x)-[:R]->(a) WHERE x.p IS NULL",
    ];
    for mode in MODES {
        for mask in [0, 15, 31, 63] {
            for body in bodies {
                for anti in [false, true] {
                    let input = format!(
                        "MATCH (a)-[r:R]->(b) WHERE {}EXISTS {{ {body} }} RETURN r,a,b",
                        if anti { "NOT " } else { "" }
                    );
                    let p = plan(&input, mode);
                    let s = Source::small(mask);
                    let want = eager(&p, &s);
                    let got =
                        EdgeScanCursor::new(s, EdgeScanPlan::compile(&p).unwrap(), wide(), || {
                            Ok::<_, ()>(())
                        });
                    assert_eq!(
                        got.collect::<Result<Vec<_>, _>>().unwrap(),
                        want,
                        "{input} {mode:?}"
                    );
                    // Vertex compilation must admit the same scope, retaining isolates.
                    let p = plan(
                        &format!(
                            "MATCH (b) WHERE {}EXISTS {{ MATCH (b)-[:R*0..3]->(x) WHERE x.p > 5 }} RETURN b",
                            if anti { "NOT " } else { "" }
                        ),
                        mode,
                    );
                    assert!(VertexScanPlan::compile(&p).is_ok());
                }
            }
        }
    }
    // Reusing the same real edge in different atoms is legal even in TRAIL.
    let p = plan(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R*1..1]-(x)-[:R*1..1]-(b) } RETURN r,a,b",
        GraphWalkSearch::Trail,
    );
    let want = eager(&p, &Source::new([(0, 0, 1)]));
    assert!(!want.is_empty());
    assert_eq!(
        EdgeScanCursor::new(
            Source::new([(0, 0, 1)]),
            EdgeScanPlan::compile(&p).unwrap(),
            wide(),
            || Ok::<_, ()>(())
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        want
    );
}
#[test]
fn unrestricted_absence_coalesces_exponential_parallel_walks_and_positive_existence_stops_early() {
    for mode in [
        GraphWalkSearch::All,
        GraphWalkSearch::AllShortest,
        GraphWalkSearch::AnyShortest,
    ] {
        let p = plan(
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*40..40]->(x) WHERE x = a } RETURN r,a,b LIMIT 1",
            mode,
        );
        let s = Source::new(
            std::iter::once((0, 9999, 0))
                .chain((0..40).flat_map(|i| [(2 * i + 1, i, i + 1), (2 * i + 2, i, i + 1)])),
        );
        let mut c = EdgeScanCursor::new(
            s,
            EdgeScanPlan::compile(&p).unwrap(),
            GqlQueryPolicy::new(81, 1, 100_000, 10_000),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            c.next().unwrap().unwrap().values()[0],
            GraphValue::Edge(EId(0))
        );
        assert_eq!(c.row_stats().snapshot_records, 81);
        assert!(c.next().is_none());
    }
    let p = plan(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R*40..40]->(x) } RETURN r,a,b LIMIT 1",
        GraphWalkSearch::All,
    );
    let mut s = Source::new(
        std::iter::once((0, 9999, 0))
            .chain((0..40).flat_map(|i| [(2 * i + 1, i, i + 1), (2 * i + 2, i, i + 1)])),
    );
    s.bad = Some(EId(2));
    let mut c = EdgeScanCursor::new(
        s,
        EdgeScanPlan::compile(&p).unwrap(),
        GqlQueryPolicy::new(41, 1, 100_000, 10_000),
        || Ok::<_, ()>(()),
    );
    assert!(c.next().unwrap().is_ok());
    assert_eq!(c.row_stats().snapshot_records, 41);
    assert!(c.next().is_none());
}
#[test]
fn all_cancellation_sites_and_exact_limits_are_terminal_and_retryable() {
    for mode in MODES {
        let p = plan(
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*2..3]->(x) WHERE x.p > 50 } RETURN r,a,b LIMIT 2",
            mode,
        );
        let scan = EdgeScanPlan::compile(&p).unwrap();
        let calls = Cell::new(0);
        let mut c = EdgeScanCursor::new(Source::small(63), scan.clone(), wide(), || {
            calls.set(calls.get() + 1);
            Ok::<_, usize>(())
        });
        let want = c.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = c.row_stats();
        let stats = c.evaluator_stats();
        drop(c);
        for stop in 1..=calls.get() {
            let at = Cell::new(0);
            let mut c = EdgeScanCursor::new(Source::small(63), scan.clone(), wide(), || {
                at.set(at.get() + 1);
                if at.get() == stop { Err(stop) } else { Ok(()) }
            });
            let mut prefix = Vec::new();
            loop {
                match c.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Interrupted(n))) => {
                        assert_eq!(n, stop);
                        break;
                    }
                    other => panic!("interruption became {other:?}"),
                }
            }
            assert_eq!(prefix, want[..prefix.len()]);
            assert_eq!(c.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(c.state(), EdgeScanState::Failed);
            assert!(c.next().is_none());
            assert_eq!(at.get(), stop);
        }
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            stats.work_units,
            stats.scratch_entries,
        );
        let run = |policy| {
            EdgeScanCursor::new(Source::small(63), scan.clone(), policy, || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>()
        };
        assert_eq!(run(exact).unwrap(), want);
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
        ] {
            assert!(run(policy).is_err());
        }
        assert_eq!(run(exact).unwrap(), want);
    }
}
#[test]
fn zero_hops_null_anchors_and_broken_sources_cannot_fabricate_anti_join_success() {
    let p = plan(
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R*1..3]->(x) WHERE x.p > 50 } RETURN r,a,b",
        GraphWalkSearch::All,
    );
    for fault in 0..3 {
        let mut s = Source::small(63);
        s.bad = (fault == 0).then_some(EId(1));
        s.repeat = fault == 1;
        s.unavailable = fault == 2;
        let mut c = EdgeScanCursor::new(s, EdgeScanPlan::compile(&p).unwrap(), wide(), || {
            Ok::<_, ()>(())
        });
        assert!(c.next().unwrap().is_err());
        assert_eq!(c.row_stats().result_rows, 0);
        assert!(c.next().is_none());
    }
    let s = Source::new([]);
    for mode in MODES {
        assert_eq!(
            collect(&s, VId(3), GlaDirection::Forward, 0, 0, mode),
            BTreeSet::from([VId(3)])
        );
    }
    let p = plan(
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*0..0]->(x) } RETURN a",
        GraphWalkSearch::All,
    );
    let at = p
        .operators()
        .iter()
        .position(|op| matches!(op, GlaOperator::Probe { .. }))
        .unwrap();
    let (probe, _) = Probe::compile(p.operators(), at, 1).unwrap();
    assert!(
        !probe
            .accepts(
                &[None],
                &s,
                &mut |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(()),
                &mut || Ok(())
            )
            .unwrap()
    );
}
#[test]
fn maximum_depth_and_full_width_edge_ids_need_no_recursion_or_identity_arithmetic() {
    let hi = crate::MAX_GRAPH_WALK_HOPS;
    let s = Source::new((0..u128::from(hi)).map(|i| (u128::MAX - i, i, i + 1)));
    for mode in MODES {
        let ends = collect(&s, VId(0), GlaDirection::Forward, hi, hi, mode);
        assert_eq!(ends, BTreeSet::from([VId(u128::from(hi))]));
    }
}

fn trail_collection_plan(
    direction: GlaDirection,
    bounds: GraphWalkBounds,
    explicit_trail: bool,
    count: Option<u64>,
) -> crate::algebra::PreparedGraphPattern<GraphValueRow> {
    use crate::algebra::{
        GraphColumn, GraphMatchClause, GraphMatchMode, GraphPathFunction, GraphPatternBuilder,
    };

    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("r", 0).unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("b").unwrap().vertex("x").unwrap();
    if explicit_trail {
        inner.trail_walk("b", R, direction, "x", bounds).unwrap();
    } else {
        inner.walk("b", R, direction, "x", bounds).unwrap();
        inner.match_mode(GraphMatchMode::DifferentEdges);
    }
    outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&inner, Some("x")),
                GraphMatchClause::collect_tuple(&inner, vec!["x", "b", "x"]),
                GraphMatchClause::collect(&inner, None),
            ],
            &[
                GraphColumn::path("r", "r", GraphPathFunction::Edge),
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::collection("endpoints", 0),
                GraphColumn::collection("tuples", 1),
                GraphColumn::collection("nulls", 2),
            ],
            0,
            count,
        )
        .unwrap()
}

fn list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
    GraphValue::List(values.into_iter().collect())
}

// Generate every bounded walk by length, then filter complete routes for
// unique EIds. Sorting identity sequences puts a prefix before its descendants
// and gives an independent oracle for the eager TRAIL's DFS witness order.
fn trail_occurrence_oracle(
    source: &Source,
    start: VId,
    direction: GlaDirection,
    bounds: GraphWalkBounds,
) -> Vec<VId> {
    let mut layer = vec![(Vec::<EId>::new(), start)];
    let mut routes = Vec::new();
    for depth in 0..=bounds.maximum() {
        if depth >= bounds.minimum() {
            routes.extend(
                layer
                    .iter()
                    .filter(|(edges, _)| edges.iter().collect::<BTreeSet<_>>().len() == edges.len())
                    .cloned(),
            );
        }
        if depth == bounds.maximum() {
            break;
        }
        let mut next = Vec::new();
        for (edges, from) in layer {
            for (&eid, &(a, b)) in &source.edges {
                let to = match direction {
                    GlaDirection::Forward if a == from => Some(b),
                    GlaDirection::Reverse if b == from => Some(a),
                    GlaDirection::Undirected if a == from => Some(b),
                    GlaDirection::Undirected if b == from => Some(a),
                    _ => None,
                };
                if let Some(to) = to {
                    let mut edges = edges.clone();
                    edges.push(eid);
                    next.push((edges, to));
                }
            }
        }
        layer = next;
    }
    routes.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    routes.into_iter().map(|(_, endpoint)| endpoint).collect()
}

#[derive(Clone, Copy, Debug)]
enum TrailChild {
    Correlated,
    Independent,
    Walk,
}

fn filtered_trail_plan(
    direction: GlaDirection,
    bounds: GraphWalkBounds,
    child_direction: GlaDirection,
    child_kind: TrailChild,
    anti: bool,
    sibling: bool,
    count: Option<u64>,
) -> crate::algebra::PreparedGraphPattern<GraphValueRow> {
    use crate::algebra::{
        GraphColumn, GraphMatchClause, GraphMatchMode, GraphPathFunction, GraphPatternBuilder,
    };

    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("r", 0).unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("b").unwrap().vertex("x").unwrap();
    inner.trail_walk("b", R, direction, "x", bounds).unwrap();
    let mut child = GraphPatternBuilder::new();
    child.match_mode(GraphMatchMode::RepeatableElements);
    let (from, target) = match child_kind {
        TrailChild::Independent => ("y", "x"),
        _ => ("x", "b"),
    };
    child.vertex(from).unwrap().vertex("z").unwrap();
    child.outer_vertex(target).unwrap();
    if matches!(child_kind, TrailChild::Walk) {
        child
            .walk(
                from,
                R,
                child_direction,
                "z",
                GraphWalkBounds::new(2, 2).unwrap(),
            )
            .unwrap();
    } else {
        child.edge(from, R, child_direction, "z").unwrap();
        child.capture_edge("s", 0).unwrap();
    }
    child.identity("z", target, true).unwrap();
    let mut guard = GraphPatternBuilder::new();
    guard.vertex("x").unwrap();
    guard.edge("x", R, GlaDirection::Forward, "x").unwrap();
    guard.capture_edge("t", 0).unwrap();
    let mut clauses = [
        GraphMatchClause::collect(&inner, Some("x")),
        GraphMatchClause::collect_tuple(&inner, vec!["x", "b", "x"]),
        GraphMatchClause::collect(&inner, None),
    ];
    for clause in &mut clauses {
        clause.nested.push(if anti {
            GraphMatchClause::not_exists(&child)
        } else {
            GraphMatchClause::exists(&child)
        });
        if sibling {
            clause.nested.push(GraphMatchClause::not_exists(&guard));
        }
    }
    outer
        .prepare_values_with_clauses(
            &clauses,
            &[
                GraphColumn::path("r", "r", GraphPathFunction::Edge),
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::collection("endpoints", 0),
                GraphColumn::collection("tuples", 1),
                GraphColumn::collection("nulls", 2),
            ],
            0,
            count,
        )
        .unwrap()
}

// Filter complete parent routes by independently enumerated child relations.
// Child identities are deliberately not excluded by the parent's active TRAIL:
// match modes belong to their own clauses, and EXISTS observes only truth.
fn filtered_trail_oracle(
    source: &Source,
    start: VId,
    directions: [GlaDirection; 2],
    bounds: GraphWalkBounds,
    child_kind: TrailChild,
    anti: bool,
    sibling: bool,
) -> Vec<VId> {
    let [direction, child_direction] = directions;
    let mut children = Vec::new();
    for &(a, b) in source.edges.values() {
        match child_direction {
            GlaDirection::Forward => children.push((a, b)),
            GlaDirection::Reverse => children.push((b, a)),
            GlaDirection::Undirected => {
                children.push((a, b));
                if a != b {
                    children.push((b, a));
                }
            }
        }
    }
    trail_occurrence_oracle(source, start, direction, bounds)
        .into_iter()
        .filter(|&endpoint| {
            let found = match child_kind {
                TrailChild::Correlated => children
                    .iter()
                    .any(|&(from, to)| from == endpoint && to == start),
                TrailChild::Independent => children.iter().any(|&(_, to)| to == endpoint),
                TrailChild::Walk => children.iter().any(|&(from, mid)| {
                    from == endpoint
                        && children.iter().any(|&(from, to)| from == mid && to == start)
                }),
            };
            found != anti
                && (!sibling
                    || !source
                        .edges
                        .values()
                        .any(|&(from, to)| from == endpoint && to == endpoint))
        })
        .collect()
}

#[test]
fn collected_trails_apply_child_truth_to_every_complete_route_occurrence() {
    use GlaDirection::{Forward as F, Reverse as R, Undirected as U};

    for (direction, child_direction) in [(F, F), (F, R), (R, U), (U, F)] {
        for (low, high) in [(0, 0), (0, 2), (1, 3), (2, 2)] {
            let bounds = GraphWalkBounds::new(low, high).unwrap();
            for child_kind in [
                TrailChild::Correlated,
                TrailChild::Independent,
                TrailChild::Walk,
            ] {
                for anti in [false, true] {
                    for sibling in [false, true] {
                        let query = filtered_trail_plan(
                            direction,
                            bounds,
                            child_direction,
                            child_kind,
                            anti,
                            sibling,
                            None,
                        );
                        let plan = EdgeScanPlan::compile(query.plan()).unwrap();
                        for mask in [0, 3, 11, 31, 63] {
                            let source = Source::small(mask);
                            let expected: Vec<_> = source
                                .edges
                                .iter()
                                .map(|(&eid, &(a, b))| {
                                    let ends = filtered_trail_oracle(
                                        &source,
                                        b,
                                        [direction, child_direction],
                                        bounds,
                                        child_kind,
                                        anti,
                                        sibling,
                                    );
                                    GraphValueRow::from_owned_values(vec![
                                        GraphValue::Edge(eid),
                                        GraphValue::Vertex(a),
                                        GraphValue::Vertex(b),
                                        list(ends.iter().copied().map(GraphValue::Vertex)),
                                        list(ends.iter().map(|&to| {
                                            list([
                                                GraphValue::Vertex(to),
                                                GraphValue::Vertex(b),
                                                GraphValue::Vertex(to),
                                            ])
                                        })),
                                        list(ends.iter().map(|_| {
                                            GraphValue::Scalar(CanonicalScalar::Null)
                                        })),
                                    ])
                                })
                                .collect();
                            assert_eq!(eager(query.plan(), &source), expected, "eager");
                            let actual = EdgeScanCursor::new(
                                source,
                                plan.clone(),
                                wide(),
                                || Ok::<_, ()>(()),
                            )
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                            assert_eq!(
                                actual, expected,
                                "{direction:?}/{child_direction:?}, {low}..{high}, \
                                 {child_kind:?}, anti={anti}, sibling={sibling}, mask={mask}",
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn rejected_trail_endpoints_still_expand_and_child_edges_have_their_own_domain() {
    let bounds = GraphWalkBounds::new(0, 2).unwrap();
    let source = Source::new([(0, 9, 0), (1, 0, 1), (2, 0, 1), (3, 1, 2), (4, 2, 0)]);
    let query = filtered_trail_plan(
        GlaDirection::Forward,
        bounds,
        GlaDirection::Forward,
        TrailChild::Correlated,
        false,
        false,
        Some(1),
    );
    let mut cursor = EdgeScanCursor::new(
        source,
        EdgeScanPlan::compile(query.plan()).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    // Zero-hop 0 and transit endpoint 1 fail the child. Both parallel
    // routes continue through 1 to endpoint 2, whose child returns to 0.
    assert_eq!(
        cursor.next().unwrap().unwrap().values()[3],
        list([GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(2))]),
    );
    assert!(cursor.next().is_none());

    let query = filtered_trail_plan(
        GlaDirection::Forward,
        GraphWalkBounds::new(1, 1).unwrap(),
        GlaDirection::Reverse,
        TrailChild::Correlated,
        false,
        false,
        Some(1),
    );
    let mut cursor = EdgeScanCursor::new(
        Source::new([(0, 9, 0), (1, 0, 1), (u128::MAX, 0, 1)]),
        EdgeScanPlan::compile(query.plan()).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    // The first child's reverse witness is the same EId as its parent hop.
    // Neither that reuse nor multiple child witnesses can erase or multiply
    // either of the two parent occurrences.
    assert_eq!(
        cursor.next().unwrap().unwrap().values()[3],
        list([GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(1))]),
    );
    assert!(cursor.next().is_none());
}

#[test]
fn zero_hop_collection_children_distinguish_absence_null_and_missing_sources() {
    for anti in [false, true] {
        let negate = if anti { "NOT " } else { "" };
        let plan = collection_text(&format!(
            "MATCH (b) RETURN b,[(b)-[:R*0..0]->(x) \
             WHERE {negate}EXISTS {{ MATCH (x)-[s:R]->(z) WHERE z = b }} | x] AS endpoints"
        ));
        assert!(VertexScanPlan::compile(&plan).is_ok());
        let start = plan
            .operators()
            .iter()
            .position(|op| matches!(op, GlaOperator::Collect { .. }))
            .unwrap();
        for nullable in [false, true] {
            let mut ops = plan.operators().to_vec();
            if nullable {
                let GlaOperator::BindVertex { source } = ops[start + 1] else {
                    panic!("correlated collection root")
                };
                ops[start + 1] = GlaOperator::BindOuterVertex { source };
            }
            let (collection, _) = Collection::compile(&ops, start, 1).unwrap();
            let mut source = Source::new([]);
            let mut control = |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(());
            let mut record = || Ok(());
            let expected = if anti {
                list([GraphValue::Vertex(VId(3))])
            } else {
                list([])
            };
            assert_eq!(
                collection
                    .evaluate(&[Some(VId(3))], &source, &mut control, &mut record)
                    .unwrap(),
                expected,
            );
            source.unavailable = true;
            // A real zero-hop parent must still execute its child constraint.
            assert!(matches!(
                collection.evaluate(&[Some(VId(3))], &source, &mut control, &mut record),
                Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable)),
            ));
            // NULL never manufactures even an identity route or reaches a child.
            assert_eq!(
                collection
                    .evaluate(&[None], &source, &mut control, &mut record)
                    .unwrap(),
                list([]),
            );
            for op in &mut ops {
                if let GlaOperator::VarLengthExpand { bounds, .. } = op {
                    *bounds = GraphWalkBounds::empty();
                }
            }
            let (empty, _) = Collection::compile(&ops, start, 1).unwrap();
            assert_eq!(
                empty
                    .evaluate(&[Some(VId(3))], &source, &mut control, &mut record)
                    .unwrap(),
                list([]),
            );
        }
    }
}

#[test]
fn trail_child_failures_preserve_prior_rows_but_never_partial_occurrence_lists() {
    for anti in [false, true] {
        let query = filtered_trail_plan(
            GlaDirection::Forward,
            GraphWalkBounds::new(1, 1).unwrap(),
            GlaDirection::Forward,
            TrailChild::Correlated,
            anti,
            false,
            None,
        );
        let mut source = Source::new([
            (0, 8, 8),
            (1, 9, 0),
            (2, 0, 1),
            (3, 0, 3),
            (4, 1, if anti { 2 } else { 0 }),
            (5, 3, 0),
        ]);
        let expected = eager(query.plan(), &source);
        assert!(!expected[1].values()[3].as_list().unwrap().is_empty());
        // The second root has accepted the route to 1 before its route to 3
        // demands this child edge. Its own bounded parent walk never reads 5.
        source.bad = Some(EId(5));
        let mut cursor = EdgeScanCursor::new(
            source,
            EdgeScanPlan::compile(query.plan()).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(cursor.next().unwrap().unwrap(), expected[0]);
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(EdgeScanError::Source("edge refused")))),
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.source.is_none() && cursor.traversal.is_none());
        let frozen = (cursor.row_stats(), cursor.evaluator_stats());
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), frozen);

        let query = filtered_trail_plan(
            GlaDirection::Forward,
            GraphWalkBounds::new(1, 1).unwrap(),
            GlaDirection::Forward,
            TrailChild::Correlated,
            anti,
            false,
            Some(1),
        );
        let mut source = Source::new([(0, 9, 0), (1, 0, 1), (2, 1, 0), (3, 1, 2)]);
        let expected = eager(query.plan(), &source);
        source.bad = Some(EId(3));
        let mut cursor = EdgeScanCursor::new(
            source,
            EdgeScanPlan::compile(query.plan()).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        // The complete TRUE child stops at edge 2 for both semi and anti
        // joins. Later poison is not read, and the parent still exhausts.
        assert_eq!(cursor.next().unwrap().unwrap(), expected[0]);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn trail_child_null_predicates_and_arithmetic_errors_keep_their_three_valued_meaning() {
    for anti in [false, true] {
        let negate = if anti { "NOT " } else { "" };
        let comparison = if anti { "<" } else { ">=" };
        let plan = collection_text(&format!(
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,[(b)-[:R*1..1]->(x) \
             WHERE {negate}EXISTS {{ MATCH (x)-[s:R]->(y) \
             WHERE 1/(y.p-1) {comparison} 0 }} | x] AS endpoints"
        ));
        let source = Source::new([
            (0, 8, 8),
            (1, 9, 0),
            (2, 0, 1),
            (3, 0, 3),
            (4, 1, 2),
            (5, 3, 0),
        ]);
        let mut cursor = EdgeScanCursor::new(
            source,
            EdgeScanPlan::compile(&plan).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        // Missing y.p at vertex 8 yields UNKNOWN, so only NOT EXISTS keeps
        // that parent occurrence. Both predicates accept the later route to 1.
        let first = if anti {
            list([GraphValue::Vertex(VId(8))])
        } else {
            list([])
        };
        assert_eq!(
            cursor.next().unwrap().unwrap().values(),
            &[
                GraphValue::Edge(EId(0)),
                GraphValue::Vertex(VId(8)),
                GraphValue::Vertex(VId(8)),
                first,
            ],
        );
        // y.p=7 succeeds on the first private route; y.p=1 then divides by
        // zero in the child of the second. Negation cannot turn it into absence.
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Data(crate::GraphIntegerError {
                kind: crate::GraphIntegerErrorKind::DivisionByZero,
                ..
            }))),
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.source.is_none() && cursor.traversal.is_none());
        assert!(cursor.next().is_none());
    }
}

#[test]
fn finite_trail_collections_preserve_every_ordered_route_occurrence() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for (low, high) in [(0, 0), (0, 1), (1, 1), (0, 3), (1, 3), (2, 2)] {
            let bounds = GraphWalkBounds::new(low, high).unwrap();
            for explicit_trail in [false, true] {
                let query = trail_collection_plan(direction, bounds, explicit_trail, None);
                let plan = EdgeScanPlan::compile(query.plan()).unwrap();
                for mask in 0..64 {
                    let source = Source::small(mask);
                    let expected: Vec<_> = source
                        .edges
                        .iter()
                        .map(|(&eid, &(a, b))| {
                            let ends = trail_occurrence_oracle(&source, b, direction, bounds);
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Edge(eid),
                                GraphValue::Vertex(a),
                                GraphValue::Vertex(b),
                                list(ends.iter().copied().map(GraphValue::Vertex)),
                                list(ends.iter().map(|&to| {
                                    list([
                                        GraphValue::Vertex(to),
                                        GraphValue::Vertex(b),
                                        GraphValue::Vertex(to),
                                    ])
                                })),
                                list(
                                    ends.iter()
                                        .map(|_| GraphValue::Scalar(CanonicalScalar::Null)),
                                ),
                            ])
                        })
                        .collect();
                    assert_eq!(
                        eager(query.plan(), &source),
                        expected,
                        "eager mask={mask}, {direction:?}, {low}..{high}, explicit={explicit_trail}",
                    );
                    let actual =
                        EdgeScanCursor::new(source, plan.clone(), wide(), || Ok::<_, ()>(()))
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                    assert_eq!(
                        actual, expected,
                        "native mask={mask}, {direction:?}, {low}..{high}, explicit={explicit_trail}",
                    );
                }
            }
        }
    }
}

#[test]
fn trail_occurrences_do_not_settle_or_allocate_endpoint_support() {
    let source = Source::small(63);
    let bounds = GraphWalkBounds::new(0, 3).unwrap();
    let mut control = |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(());
    let mut cursor = Endpoints::trail_occurrences(VId(0), bounds, &source, &mut control).unwrap();
    let mut actual = Vec::new();
    while let Some(endpoint) = cursor
        .next(
            expansion(GlaDirection::Undirected),
            &source,
            &mut control,
            &mut || Ok(()),
        )
        .unwrap()
    {
        actual.push(endpoint);
        assert!(cursor.emitted.is_empty());
        assert!(cursor.visited.is_empty());
        assert!(cursor.stack.len() <= bounds.maximum() as usize + 1);
    }
    assert_eq!(
        actual,
        trail_occurrence_oracle(&source, VId(0), GlaDirection::Undirected, bounds),
    );
    let support = collect(
        &source,
        VId(0),
        GlaDirection::Undirected,
        0,
        3,
        GraphWalkSearch::Trail,
    );
    assert!(actual.len() > support.len());
    assert_eq!(actual.into_iter().collect::<BTreeSet<_>>(), support);
    assert!(cursor.stack.is_empty() && cursor.edges.is_empty());
}

fn collection_text(input: &str) -> GlaPlan<GraphValueRow> {
    crate::PreparedGraphSetText::prepare(input, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .single_pattern_input()
    .unwrap()
    .plan()
    .clone()
}

#[test]
fn collected_trails_filter_complete_endpoints_without_pruning_transit_vertices() {
    let plan = collection_text(
        "MATCH (a)-[r:R]->(b) RETURN r,a,b,\
         [(b)-[:R*0..3]->(x) WHERE x.p > 5 | x] AS endpoints",
    );
    let source = Source::small(63);
    let expected = eager(&plan, &source);
    for row in &expected {
        let GraphValue::Vertex(start) = row.values()[2] else {
            panic!("outer endpoint")
        };
        assert_eq!(
            row.values()[3],
            list(
                trail_occurrence_oracle(
                    &source,
                    start,
                    GlaDirection::Forward,
                    GraphWalkBounds::new(0, 3).unwrap(),
                )
                .into_iter()
                .filter(|endpoint| *endpoint == VId(2))
                .map(GraphValue::Vertex),
            ),
        );
    }
    // Reaching vertex 2 from the first outer root needs transit vertex 1,
    // whose NULL property fails the final predicate. Multiple trails survive.
    assert!(expected[0].values()[3].as_list().unwrap().len() > 1);
    assert_eq!(
        EdgeScanCursor::new(
            source,
            EdgeScanPlan::compile(&plan).unwrap(),
            wide(),
            || { Ok::<_, ()>(()) }
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        expected,
    );
}

#[test]
fn collected_zero_hops_keep_isolates_empty_intervals_and_null_correlations_distinct() {
    let plan = collection_text("MATCH (a) RETURN a,[(a)-[:R*0..0]->(b) | b] AS endpoints");
    assert!(VertexScanPlan::compile(&plan).is_ok());
    let start = plan
        .operators()
        .iter()
        .position(|op| matches!(op, GlaOperator::Collect { .. }))
        .unwrap();
    let mut source = Source::new([]);
    source.unavailable = true;
    for nullable in [false, true] {
        let mut ops = plan.operators().to_vec();
        if nullable {
            let GlaOperator::BindVertex { source } = ops[start + 1] else {
                panic!("correlated collection root")
            };
            ops[start + 1] = GlaOperator::BindOuterVertex { source };
        }
        let (collection, _) = Collection::compile(&ops, start, 1).unwrap();
        let records = Cell::new(0);
        let mut control = |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(());
        let mut record = || {
            records.set(records.get() + 1);
            Ok(())
        };
        assert_eq!(
            collection
                .evaluate(&[Some(VId(3))], &source, &mut control, &mut record)
                .unwrap(),
            list([GraphValue::Vertex(VId(3))]),
        );
        assert_eq!(
            collection
                .evaluate(&[None], &source, &mut control, &mut record)
                .unwrap(),
            list([]),
        );
        assert_eq!(records.get(), 0, "zero hops need no incidence index");
        for op in &mut ops {
            if let GlaOperator::VarLengthExpand { bounds, .. } = op {
                *bounds = GraphWalkBounds::empty();
            }
        }
        let (empty, _) = Collection::compile(&ops, start, 1).unwrap();
        assert_eq!(
            empty
                .evaluate(&[Some(VId(3))], &source, &mut control, &mut record)
                .unwrap(),
            list([]),
        );
        assert_eq!(records.get(), 0);
    }
}

#[test]
fn collected_trails_share_every_checkpoint_and_exact_cumulative_allowance() {
    let bounds = GraphWalkBounds::new(0, 2).unwrap();
    let bare = trail_collection_plan(GlaDirection::Undirected, bounds, false, Some(1));
    let independent = filtered_trail_plan(
        GlaDirection::Undirected,
        bounds,
        GlaDirection::Forward,
        TrailChild::Independent,
        false,
        true,
        Some(1),
    );
    let child_walk = filtered_trail_plan(
        GlaDirection::Undirected,
        bounds,
        GlaDirection::Forward,
        TrailChild::Walk,
        true,
        false,
        Some(1),
    );
    for query in [bare, independent, child_walk] {
        trail_checkpoint_law(&query);
    }
}

fn trail_checkpoint_law(query: &crate::algebra::PreparedGraphPattern<GraphValueRow>) {
    let plan = EdgeScanPlan::compile(query.plan()).unwrap();
    let source = || Source::new([(0, 0, 1), (1, 1, 0), (2, 1, 1)]);
    let mut total = 0;
    let mut cursor = EdgeScanCursor::new(source(), plan.clone(), wide(), || {
        total += 1;
        Ok::<_, usize>(())
    });
    let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = cursor.row_stats();
    let stats = cursor.evaluator_stats();
    drop(cursor);
    assert_eq!(expected.len(), 1);
    for stop in 1..=total {
        let mut at = 0;
        let mut cursor = EdgeScanCursor::new(source(), plan.clone(), wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(reported))) => {
                    assert_eq!(reported, stop);
                    break;
                }
                other => panic!("checkpoint {stop} became {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.source.is_none() && cursor.traversal.is_none());
        let frozen = (cursor.row_stats(), cursor.evaluator_stats());
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), frozen);
        drop(cursor);
        assert_eq!(at, stop);
    }
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        rows.result_rows,
        stats.work_units,
        stats.scratch_entries,
    );
    let mut cursor = EdgeScanCursor::new(source(), plan.clone(), exact, || Ok::<_, ()>(()));
    assert_eq!(
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    assert_eq!(
        (cursor.row_stats(), cursor.evaluator_stats()),
        (rows, stats)
    );
    for allowance in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
    ] {
        let mut cursor = EdgeScanCursor::new(source(), plan.clone(), allowance, || Ok::<_, ()>(()));
        assert!(matches!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>(),
            Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)),
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.source.is_none() && cursor.traversal.is_none());
        assert!(cursor.next().is_none());
    }
}

#[test]
fn trail_collection_admission_keeps_unproved_profiles_refused_before_reads() {
    use crate::algebra::{BindingSlot, GraphPathFunction, ValueProjection, WalkEdgePredicate};

    let query = trail_collection_plan(
        GlaDirection::Forward,
        GraphWalkBounds::new(0, 3).unwrap(),
        true,
        Some(0),
    );
    let original = query.plan().operators();
    let start = original
        .iter()
        .position(|op| matches!(op, GlaOperator::Collect { .. }))
        .unwrap();
    let (_, end) = Collection::compile(original, start, 2).unwrap();
    let walk = (start + 1..end)
        .find(|&at| matches!(original[at], GlaOperator::VarLengthExpand { .. }))
        .unwrap();
    for mode in MODES {
        let mut ops = original.to_vec();
        let GlaOperator::VarLengthExpand { search, .. } = &mut ops[walk] else {
            unreachable!("the fixture has a walk")
        };
        *search = mode;
        assert_eq!(
            EdgeScanPlan::compile(&GlaPlan::from_operators(ops)).is_ok(),
            mode == GraphWalkSearch::Trail,
            "{mode:?}",
        );
    }
    for profile in 0..6 {
        let mut ops = original[..=end].to_vec();
        match profile {
            0 => {
                let GlaOperator::VarLengthExpand { bounds, .. } = &mut ops[walk] else {
                    unreachable!("the fixture has a walk")
                };
                *bounds = GraphWalkBounds::unbounded(0).unwrap();
            }
            // Independent fixed scopes are admitted separately; a TRAIL
            // collection still requires a correlated root.
            1 => ops[start + 1] = GlaOperator::ScanVertices,
            2 => {
                let GlaOperator::VarLengthExpand {
                    edge_predicates, ..
                } = &mut ops[walk]
                else {
                    unreachable!("the fixture has a walk")
                };
                edge_predicates.push(WalkEdgePredicate {
                    key: P,
                    value: CanonicalScalar::Int(1).into(),
                });
            }
            3..=5 => {
                let addition = match profile {
                    3 => GlaOperator::Expand {
                        source: BindingSlot(3),
                        relation: R.into(),
                        direction: GlaDirection::Forward,
                    },
                    4 => GlaOperator::VarLengthExpand {
                        source: BindingSlot(3),
                        relation: R.into(),
                        direction: GlaDirection::Forward,
                        bounds: GraphWalkBounds::new(0, 1).unwrap(),
                        search: GraphWalkSearch::Trail,
                        edge_predicates: Vec::new(),
                    },
                    _ => GlaOperator::CapturePath {
                        capture: 1,
                        start: BindingSlot(2),
                        segments: vec![BindingSlot(3)],
                    },
                };
                ops.insert(end, addition);
                let close = {
                    let GlaOperator::Collect { end, slots, .. } = &mut ops[start] else {
                        unreachable!("the fixture starts a collection")
                    };
                    *end += 1;
                    if profile < 5 {
                        *slots += 1;
                    }
                    *end as usize
                };
                if profile == 5 {
                    let GlaOperator::CollectEnd { value, .. } = &mut ops[close] else {
                        unreachable!("the fixture closes its collection")
                    };
                    // A finite TRAIL capture is now a supported Path value,
                    // but it must never masquerade as a fixed relationship.
                    *value = Some(ValueProjection::Path {
                        capture: 1,
                        function: GraphPathFunction::Edge,
                    });
                }
            }
            _ => unreachable!(),
        }
        assert!(
            Collection::compile(&ops, start, 2).is_err(),
            "profile {profile}"
        );
    }
    assert!(AsyncEdgeJoinPlan::compile(query.plan()).is_err());
    assert!(AsyncEdgeJoinPlan::compile_sort_input(query.plan()).is_err());
    let mut source = Source::small(63);
    source.bad = Some(EId(0));
    source.unavailable = true;
    let mut cursor = EdgeScanCursor::new(
        source,
        EdgeScanPlan::compile(query.plan()).unwrap(),
        GqlQueryPolicy::new(0, 0, 0, 0),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().is_none());
    assert_eq!(cursor.row_stats().snapshot_records, 0);
}

#[test]
fn trail_collection_failures_discard_private_witnesses_and_never_truncate_a_list() {
    let query = trail_collection_plan(
        GlaDirection::Forward,
        GraphWalkBounds::new(0, 2).unwrap(),
        false,
        None,
    );
    let plan = EdgeScanPlan::compile(query.plan()).unwrap();
    let mut source = Source::new([(0, 0, 1), (1, 1, 2), (2, 2, 3), (3, 2, 4)]);
    assert_eq!(
        eager(query.plan(), &source)[0].values()[3]
            .as_list()
            .unwrap()
            .len(),
        4,
    );
    source.bad = Some(EId(3)); // Zero hops and two later witnesses already completed privately.
    let mut cursor = EdgeScanCursor::new(source, plan.clone(), wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "edge refused"
        )))),
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.source.is_none() && cursor.traversal.is_none());
    assert!(cursor.next().is_none());

    // 1 + 128 + 128*127 distinct trail occurrences fit the endpoint list,
    // but exceed the value-node bound of the repeated three-field tuples.
    // The already completed first list must not escape with a truncated second.
    let source = Source::new((0..128).map(|id| (id, 0, 0)));
    let mut cursor = EdgeScanCursor::new(source, plan, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Data(crate::GraphIntegerError {
            kind: crate::GraphIntegerErrorKind::CollectionBounds,
            ..
        }))),
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(cursor.source.is_none() && cursor.traversal.is_none());
    assert!(cursor.next().is_none());
}
