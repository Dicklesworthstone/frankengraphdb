//! Captured relationships use the chosen edge, not an endpoint alias or an
//! outer capture. The complete-product oracle is independent of probe DFS.
use super::super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::stream::{
    VertexScanCursor, VertexScanEvent, VertexScanPlan, VertexScanSource, VertexScanSourceError,
    VertexScanState,
};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Props = Vec<(PropertyKeyId, CanonicalPropertyValue)>;

struct Edge {
    source: VId,
    target: VId,
    relation: RelationId,
    props: Props,
}
struct Fixture {
    edges: BTreeMap<EId, Edge>,
    vertices: BTreeMap<VId, Props>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    edge_after: Option<EId>,
    vertex_after: Option<VId>,
    fail_property: Option<EId>,
    roots: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(values: [Option<CanonicalScalar>; 4]) -> Self {
        let mut f = Self {
            edges: BTreeMap::new(),
            vertices: (0..=4)
                .map(|id| {
                    (
                        VId(id),
                        vec![(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(5)))],
                    )
                })
                .collect(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            edge_after: None,
            vertex_after: None,
            fail_property: None,
            roots: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        };
        for ((eid, source, target), value) in [(0, 0, 1), (1, 0, 1), (2, 1, 2), (u128::MAX, 2, 2)]
            .into_iter()
            .zip(values)
        {
            f.insert(EId(eid), VId(source), VId(target), R, value);
        }
        f.insert(EId(9), VId(1), VId(4), S, Some(CanonicalScalar::Int(20)));
        f
    }
    fn regular() -> Self {
        Self::new([
            Some(CanonicalScalar::Int(0)),
            Some(CanonicalScalar::Int(10)),
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Int(9)),
        ])
    }
    fn insert(
        &mut self,
        eid: EId,
        source: VId,
        target: VId,
        relation: RelationId,
        value: Option<CanonicalScalar>,
    ) {
        self.outgoing.entry(source).or_default().insert(eid);
        self.incoming.entry(target).or_default().insert(eid);
        self.edges.insert(
            eid,
            Edge {
                source,
                target,
                relation,
                props: value
                    .into_iter()
                    .map(|value| (P, CanonicalPropertyValue::Scalar(value)))
                    .collect(),
            },
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl EdgeScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self
            .edges
            .range((self.edge_after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if next.is_some() {
            self.edge_after = next;
        }
        Ok(next)
    }
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.edges.get(&eid).map(|edge| EdgeScanRow {
            source: edge.source,
            target: edge.target,
            relation: edge.relation,
            properties: &edge.props,
        }))
    }
    fn edge_property<'a, C>(
        &'a self,
        eid: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalPropertyValue>>, EdgeScanSourceError<Self::Error, C>>
    {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail_property == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge property denied"));
        }
        Ok(self.edges.get(&eid).map(|edge| {
            edge.props
                .iter()
                .find(|(found, _)| *found == key)
                .map(|(_, value)| value)
        }))
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow {
            labels: &[],
            properties,
        }))
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        Ok(self
            .vertices
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        let seek = |map: &BTreeMap<VId, BTreeSet<EId>>| {
            map.get(&endpoint).and_then(|set| {
                set.range((after.map_or(Unbounded, Excluded), Unbounded))
                    .next()
                    .copied()
            })
        };
        Ok(match direction {
            GlaDirection::Forward => seek(&self.outgoing),
            GlaDirection::Reverse => seek(&self.incoming),
            GlaDirection::Undirected => match (seek(&self.outgoing), seek(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        })
    }
}
impl VertexScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = self
            .vertices
            .range((self.vertex_after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if next.is_some() {
            self.vertex_after = next;
        }
        Ok(next)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow {
            labels: &[],
            properties,
        }))
    }
    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::next_probe_vertex(self, after, control)
    }
    fn next_probe_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::next_incident_edge(self, endpoint, direction, after, control)
    }
    fn probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::edge(self, eid, control).map_err(EdgeExpansionSourceError::Read)
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 1_000_000, 1_000_000)
}
fn eager(f: &Fixture, plan: &PreparedGraphPattern<GraphValueRow>) -> Vec<GraphValueRow> {
    plan.plan()
        .execute_governed_with_element_properties(
            (f.edges.len() + f.vertices.len()) as u64,
            f.vertices.keys().copied(),
            f.edges
                .iter()
                .map(|(&id, edge)| (id, edge.source, edge.relation, edge.target)),
            |_, _| Ok::<_, ()>(true),
            |vid, key| {
                Ok(f.vertices
                    .get(&vid)
                    .and_then(|p| p.iter().find(|(k, _)| *k == key))
                    .map(|(_, v)| v))
            },
            |eid, key| {
                Ok(f.edges
                    .get(&eid)
                    .and_then(|e| e.props.iter().find(|(k, _)| *k == key))
                    .map(|(_, v)| v))
            },
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn eager_and_vertex_pull_match_an_independent_parallel_edge_property_oracle() {
    for encoded in 0..256_u32 {
        let values: [Option<CanonicalScalar>; 4] =
            std::array::from_fn(|at| match (encoded >> (2 * at)) & 3 {
                0 => None,
                1 => Some(CanonicalScalar::Null),
                2 => Some(CanonicalScalar::Int(0)),
                _ => Some(CanonicalScalar::Int(10)),
            });
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let atom = match direction {
                GlaDirection::Forward => "(a)-[e:R]->(b)",
                GlaDirection::Reverse => "(a)<-[e:R]-(b)",
                GlaDirection::Undirected => "(a)-[e:R]-(b)",
            };
            for anti in [false, true] {
                let plan = prepare(&format!(
                    "MATCH (a) WHERE {}EXISTS {{ MATCH {atom} WHERE e.p > a.p }} RETURN a",
                    if anti { "NOT " } else { "" }
                ));
                let f = Fixture::new(values.clone());
                let expected: Vec<_> = f
                    .vertices
                    .keys()
                    .filter(|&&vid| {
                        let found = f.edges.values().any(|edge| {
                            edge.relation == R
                                && match direction {
                                    GlaDirection::Forward => edge.source == vid,
                                    GlaDirection::Reverse => edge.target == vid,
                                    GlaDirection::Undirected => {
                                        edge.source == vid || edge.target == vid
                                    }
                                }
                                && edge.props.iter().any(|(key, value)| {
                                    *key == P
                                        && matches!(value, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)) if *n > 5)
                                })
                        });
                        found != anti
                    })
                    .map(|&vid| vec![GraphValue::Vertex(vid)])
                    .collect();
                assert_eq!(plain(&eager(&f, &plan)), expected);
                let mut cursor = VertexScanCursor::new(
                    f,
                    VertexScanPlan::compile(plan.plan()).unwrap(),
                    policy(),
                    || Ok::<_, ()>(()),
                );
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(
                    plain(&rows),
                    expected,
                    "values={encoded} direction={direction:?} anti={anti}"
                );
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
            }
        }
    }
}

#[test]
fn edge_pull_restores_outer_capture_and_sibling_probe_names() {
    for text in [
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[s:R]->(x) WHERE s.p = 10 } AND NOT EXISTS { MATCH (b)-[s:R]->(y) WHERE s.p = 10 } RETURN r, a, r.p",
        "MATCH (anchor) WHERE EXISTS { MATCH (x)-[left:R]->(y), (z)-[right:S]->(anchor) WHERE left.p = 10 AND right.p = 20 } RETURN anchor",
    ] {
        let plan = prepare(text);
        let f = Fixture::regular();
        let expected = eager(&f, &plan);
        assert!(!expected.is_empty());
        let rows = if text.starts_with("MATCH (anchor)") {
            VertexScanCursor::new(
                f,
                VertexScanPlan::compile(plan.plan()).unwrap(),
                policy(),
                || Ok::<_, ()>(()),
            )
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        } else {
            EdgeScanCursor::new(
                f,
                EdgeScanPlan::compile(plan.plan()).unwrap(),
                policy(),
                || Ok::<_, ()>(()),
            )
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(rows, expected);
    }
}

#[test]
fn only_a_complete_true_witness_short_circuits_and_property_failures_are_terminal() {
    let text = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[e:R]->(x) WHERE e.p >= 0 } RETURN r, a LIMIT 1";
    let plan = prepare(text);
    let mut f = Fixture::regular();
    f.fail_property = Some(EId(1)); // The first candidate is already a complete TRUE witness.
    let seeks = f.seeks.clone();
    let mut cursor = EdgeScanCursor::new(
        f,
        EdgeScanPlan::compile(plan.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().unwrap().is_ok());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    for anti in [false, true] {
        let plan = prepare(
            &text
                .replace(
                    "WHERE EXISTS",
                    if anti {
                        "WHERE NOT EXISTS"
                    } else {
                        "WHERE EXISTS"
                    },
                )
                .replace("e.p >= 0", "e.p > 5"),
        );
        let mut f = Fixture::regular();
        f.fail_property = Some(EId(1)); // Failed zero-valued parallel edge cannot end this probe.
        let drops = f.drops.clone();
        let mut cursor = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(plan.plan()).unwrap(),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(EdgeScanError::Source(
                "edge property denied"
            ))))
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn every_capture_checkpoint_fuses_the_cursor_without_losing_delivered_prefixes() {
    let plan = prepare(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a, r.p",
    );
    let calls = Cell::new(0);
    let mut good = EdgeScanCursor::new(
        Fixture::regular(),
        EdgeScanPlan::compile(plan.plan()).unwrap(),
        policy(),
        || {
            calls.set(calls.get() + 1);
            Ok::<_, usize>(())
        },
    );
    let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let total = calls.get();
    assert!(total > 20 && expected.len() > 1);
    for stop in 1..=total {
        let at = Cell::new(0);
        let f = Fixture::regular();
        let drops = f.drops.clone();
        let mut cursor = EdgeScanCursor::new(
            f,
            EdgeScanPlan::compile(plan.plan()).unwrap(),
            policy(),
            || {
                at.set(at.get() + 1);
                if at.get() == stop { Err(stop) } else { Ok(()) }
            },
        );
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(actual))) => {
                    assert_eq!(actual, stop);
                    break;
                }
                other => panic!("expected injected failure at {stop}, got {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(at.get(), stop);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        assert_eq!(at.get(), stop);
    }
}

#[test]
fn captures_share_existing_exact_quotas_and_zero_limit_reads_nothing() {
    let plan = prepare(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a, r.p",
    );
    let mut cursor = EdgeScanCursor::new(
        Fixture::regular(),
        EdgeScanPlan::compile(plan.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    let exact = [
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    assert!(exact.iter().all(|value| *value > 0));
    for reduced in 0..=4 {
        let mut limits = exact;
        if reduced < 4 {
            limits[reduced] -= 1;
        }
        let mut candidate = EdgeScanCursor::new(
            Fixture::regular(),
            EdgeScanPlan::compile(plan.plan()).unwrap(),
            GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
            || Ok::<_, ()>(()),
        );
        let result = candidate.by_ref().collect::<Result<Vec<_>, _>>();
        if reduced == 4 {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Rows(_)) | Err(GqlQueryError::Evaluator(_))
            ));
            assert_eq!(candidate.state(), EdgeScanState::Failed);
        }
    }
    let plan =
        prepare("MATCH (a) WHERE EXISTS { MATCH (a)-[e:R]->(b) WHERE e.p > 5 } RETURN a LIMIT 0");
    let f = Fixture::regular();
    let roots = f.roots.clone();
    let seeks = f.seeks.clone();
    let mut cursor = VertexScanCursor::new(
        f,
        VertexScanPlan::compile(plan.plan()).unwrap(),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().is_none());
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(seeks.load(Ordering::SeqCst), 0);
}

#[test]
fn physical_preparation_rejects_foreign_or_nonedge_captures_even_at_limit_zero() {
    use crate::algebra::BindingSlot;
    let plan = prepare(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a LIMIT 0",
    );
    let original = plan.plan().operators();
    let start = original
        .iter()
        .position(|op| matches!(op, GlaOperator::Probe { .. }))
        .unwrap();
    let capture_at = (start + 1..original.len())
        .find(|&at| matches!(original[at], GlaOperator::CapturePath { .. }))
        .unwrap();
    assert!(super::Probe::compile(original, start, 2).is_ok());
    for corruption in 0..5 {
        let mut ops = original.to_vec();
        if corruption == 4 {
            // Keep the typed edge-property reference but remove its declaration.
            // It must not bind the outer r or turn into a missing scalar value.
            ops[capture_at] = GlaOperator::Select {
                slot: BindingSlot(0),
                predicates: vec![],
            };
        } else {
            let GlaOperator::CapturePath {
                capture,
                start,
                segments,
            } = &mut ops[capture_at]
            else {
                unreachable!("the fixture includes a local relationship capture")
            };
            match corruption {
                0 => segments[0] = BindingSlot(2), // copied b, not an edge expansion
                1 => *start = BindingSlot(0),      // not that expansion's actual source
                2 => *capture = crate::algebra::MAX_PATTERN_IDENTITIES as u32,
                3 => segments.push(segments[0]), // a compound path is not a relationship
                _ => unreachable!(),
            }
        }
        assert!(super::Probe::compile(&ops, start, 2).is_err());
    }
}

// Enumerate the tiny fixture by a complete edge scan at every prefix. This
// oracle has no indexed seek state, endpoint cache, or production walk frames.
fn expected_trails(
    fixture: &Fixture,
    start: VId,
    direction: GlaDirection,
    bounds: (usize, usize),
) -> Vec<GraphPath> {
    fn visit(
        fixture: &Fixture,
        start: VId,
        endpoint: VId,
        direction: GlaDirection,
        bounds: (usize, usize),
        steps: &mut Vec<(EId, VId)>,
        paths: &mut Vec<GraphPath>,
    ) {
        if steps.len() >= bounds.0 {
            paths.push(GraphPath::new(start, steps.clone().into_boxed_slice()));
        }
        if steps.len() == bounds.1 {
            return;
        }
        for (&eid, edge) in &fixture.edges {
            if edge.relation != R || steps.iter().any(|(used, _)| *used == eid) {
                continue;
            }
            let next = match direction {
                GlaDirection::Forward if edge.source == endpoint => edge.target,
                GlaDirection::Reverse if edge.target == endpoint => edge.source,
                GlaDirection::Undirected if edge.source == endpoint => edge.target,
                GlaDirection::Undirected if edge.target == endpoint => edge.source,
                _ => continue,
            };
            steps.push((eid, next));
            visit(fixture, start, next, direction, bounds, steps, paths);
            steps.pop();
        }
    }
    let mut paths = Vec::new();
    visit(
        fixture,
        start,
        start,
        direction,
        bounds,
        &mut Vec::new(),
        &mut paths,
    );
    paths
}

#[test]
fn captured_trails_keep_zero_hops_parallel_routes_cycles_and_declared_orientation() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        // Seed the declared first endpoint, seed its far endpoint (normalizing
        // the expansion backwards), and constrain the route to close on a.
        for shape in 0..3 {
            let tail = if shape == 2 { "a" } else { "b" };
            let anchor = if shape == 1 { "b" } else { "a" };
            let atom = match direction {
                GlaDirection::Forward => format!("(a)-[:R*0..3]->({tail})"),
                GlaDirection::Reverse => format!("(a)<-[:R*0..3]-({tail})"),
                GlaDirection::Undirected => format!("(a)-[:R*0..3]-({tail})"),
            };
            let plan = prepare(&format!(
                "MATCH ({anchor}) RETURN {anchor}, [p = {atom} | p] AS paths"
            ));
            let mut fixture = Fixture::regular();
            fixture.insert(EId(3), VId(2), VId(0), R, None);
            let scan_direction = if shape == 1 {
                match direction {
                    GlaDirection::Forward => GlaDirection::Reverse,
                    GlaDirection::Reverse => GlaDirection::Forward,
                    GlaDirection::Undirected => GlaDirection::Undirected,
                }
            } else {
                direction
            };
            let expected: Vec<_> = fixture
                .vertices
                .keys()
                .map(|&root| {
                    let paths = expected_trails(&fixture, root, scan_direction, (0, 3))
                        .into_iter()
                        .filter_map(|path| {
                            let end = path.nodes().last().unwrap();
                            if shape == 2 && end != root {
                                return None;
                            }
                            // Eager prefers forward when both ends coincide.
                            let path = if shape == 1 && end != root {
                                let nodes: Vec<_> = path.nodes().collect();
                                let reversed = path
                                    .steps()
                                    .iter()
                                    .map(|(eid, _)| *eid)
                                    .zip(nodes)
                                    .rev()
                                    .collect::<Vec<_>>()
                                    .into_boxed_slice();
                                GraphPath::new(end, reversed)
                            } else {
                                path
                            };
                            Some(GraphValue::Path(path))
                        })
                        .collect::<Vec<_>>();
                    vec![
                        GraphValue::Vertex(root),
                        GraphValue::List(paths.into_boxed_slice()),
                    ]
                })
                .collect();
            assert_eq!(plain(&eager(&fixture, &plan)), expected);
            let mut cursor = VertexScanCursor::new(
                fixture,
                VertexScanPlan::compile(plan.plan()).unwrap(),
                policy(),
                || Ok::<_, ()>(()),
            );
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(plain(&rows), expected, "{direction:?}, shape={shape}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    }
}

#[test]
fn captured_variable_segments_never_become_fixed_edge_operands() {
    let plan = prepare("MATCH (a) RETURN [p = (a)-[:R*0..3]->(b) | p] AS paths LIMIT 0");
    let original = plan.plan().operators();
    let start = original
        .iter()
        .position(|op| matches!(op, GlaOperator::Collect { .. }))
        .unwrap();
    let (collection, end) = super::Collection::compile(original, start, 1).unwrap();
    // A NULL correlation has no zero-hop identity and performs no adjacency
    // read. An isolated visible vertex has exactly its zero-hop route instead.
    let fixture = Fixture::regular();
    let value = collection
        .evaluate(
            &[None],
            &fixture,
            &mut |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(()),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(value, GraphValue::List(Box::new([])));
    assert_eq!(fixture.seeks.load(Ordering::SeqCst), 0);
    let value = collection
        .evaluate(
            &[Some(VId(3))],
            &fixture,
            &mut |_| Ok::<_, GqlQueryError<EdgeScanError<&'static str>, ()>>(()),
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        value,
        GraphValue::List(Box::new([GraphValue::Path(GraphPath::new(
            VId(3),
            Box::new([]),
        ))])),
    );

    let mut edge_output = original.to_vec();
    let GlaOperator::CollectEnd {
        value: Some(ValueProjection::Path { function, .. }),
        ..
    } = &mut edge_output[end]
    else {
        panic!("the fixture projects its local capture")
    };
    *function = GraphPathFunction::Edge;
    assert!(super::Collection::compile(&edge_output, start, 1).is_err());

    // A variable segment with exactly zero or one hop still carries its Path
    // domain. Its syntactic segment count must not make it a fixed Edge.
    for hops in [0, 1] {
        let mut singleton = original.to_vec();
        for op in &mut singleton {
            if let GlaOperator::VarLengthExpand { bounds, .. } = op {
                *bounds = crate::GraphWalkBounds::new(hops, hops).unwrap();
            }
        }
        assert!(super::Collection::compile(&singleton, start, 1).is_ok());
        let GlaOperator::CollectEnd {
            value: Some(ValueProjection::Path { function, .. }),
            ..
        } = &mut singleton[end]
        else {
            unreachable!()
        };
        *function = GraphPathFunction::Edge;
        assert!(super::Collection::compile(&singleton, start, 1).is_err());
    }

    let property =
        prepare("MATCH (a) RETURN [(a)-[r:R]->(b) WHERE r.p > 5 | b] AS vertices LIMIT 0");
    let predicate = property
        .plan()
        .operators()
        .iter()
        .find(|op| matches!(op, GlaOperator::SelectBoolean { .. }))
        .unwrap()
        .clone();
    let mut edge_property = original.to_vec();
    edge_property.insert(end, predicate);
    if let GlaOperator::Collect { end, .. } = &mut edge_property[start] {
        *end += 1;
    }
    assert!(super::Collection::compile(&edge_property, start, 1).is_err());

    let mut probe = original.to_vec();
    let GlaOperator::Collect { group, .. } = probe[start] else {
        unreachable!()
    };
    probe[start] = GlaOperator::Probe {
        group,
        anti: false,
        end: end as u32,
    };
    probe[end] = GlaOperator::ProbeEnd { group };
    // Ordinary and async EXISTS share this compiler and remain support-only.
    assert!(super::Probe::compile(&probe, start, 1).is_err());
}

#[test]
fn every_captured_trail_checkpoint_and_quota_is_terminal_without_a_partial_list() {
    let plan = prepare("MATCH (a) RETURN a, [p = (a)-[:R*0..3]->(b) | [b, p, length(p)]] AS paths");
    let calls = Cell::new(0);
    let mut good = VertexScanCursor::new(
        Fixture::regular(),
        VertexScanPlan::compile(plan.plan()).unwrap(),
        policy(),
        || {
            calls.set(calls.get() + 1);
            Ok::<_, usize>(())
        },
    );
    let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = good.row_stats();
    let evaluator = good.evaluator_stats();
    let total = calls.get();
    assert!(total > 20 && expected.len() > 1);
    for stop in 1..=total {
        let at = Cell::new(0);
        let fixture = Fixture::regular();
        let drops = fixture.drops.clone();
        let mut cursor = VertexScanCursor::new(
            fixture,
            VertexScanPlan::compile(plan.plan()).unwrap(),
            policy(),
            || {
                at.set(at.get() + 1);
                if at.get() == stop { Err(stop) } else { Ok(()) }
            },
        );
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(actual))) => {
                    assert_eq!(actual, stop);
                    break;
                }
                other => panic!("expected captured-trail failure at {stop}, got {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        assert_eq!(at.get(), stop);
    }
    let exact = [
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    assert!(exact.iter().all(|limit| *limit > 0));
    for reduced in 0..=4 {
        let mut limits = exact;
        if reduced < 4 {
            limits[reduced] -= 1;
        }
        let mut cursor = VertexScanCursor::new(
            Fixture::regular(),
            VertexScanPlan::compile(plan.plan()).unwrap(),
            GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
            || Ok::<_, ()>(()),
        );
        let result = cursor.by_ref().collect::<Result<Vec<_>, _>>();
        if reduced == 4 {
            assert_eq!(result.unwrap(), expected);
            assert_eq!(cursor.evaluator_stats(), evaluator);
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
    }
}
