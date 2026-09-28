//! Captured relationships use the chosen edge, not an endpoint alias or an
//! outer capture. The complete-product oracle is independent of probe DFS.
use super::super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::stream::{
    VertexScanCursor, VertexScanEvent, VertexScanPlan, VertexScanSource,
    VertexScanSourceError, VertexScanState,
};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Props = Vec<(PropertyKeyId, CanonicalScalar)>;

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
            vertices: (0..=4).map(|id| (VId(id), vec![(P, CanonicalScalar::Int(5))])).collect(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            edge_after: None,
            vertex_after: None,
            fail_property: None,
            roots: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        };
        for ((eid, source, target), value) in [
            (0, 0, 1), (1, 0, 1), (2, 1, 2), (u128::MAX, 2, 2),
        ].into_iter().zip(values) {
            f.insert(EId(eid), VId(source), VId(target), R, value);
        }
        f.insert(EId(9), VId(1), VId(4), S, Some(CanonicalScalar::Int(20)));
        f
    }
    fn regular() -> Self {
        Self::new([
            Some(CanonicalScalar::Int(0)), Some(CanonicalScalar::Int(10)),
            Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(9)),
        ])
    }
    fn insert(&mut self, eid: EId, source: VId, target: VId, relation: RelationId,
              value: Option<CanonicalScalar>) {
        self.outgoing.entry(source).or_default().insert(eid);
        self.incoming.entry(target).or_default().insert(eid);
        self.edges.insert(eid, Edge {
            source, target, relation, props: value.into_iter().map(|value| (P, value)).collect(),
        });
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
}
impl EdgeScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
    fn next_edge<C>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self.edges.range((self.edge_after.map_or(Unbounded, Excluded), Unbounded))
            .next().map(|(&id, _)| id);
        if next.is_some() { self.edge_after = next; }
        Ok(next)
    }
    fn edge<'a, C>(&'a self, eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.edges.get(&eid).map(|edge| EdgeScanRow {
            source: edge.source, target: edge.target, relation: edge.relation, properties: &edge.props,
        }))
    }
    fn edge_property<'a, C>(&'a self, eid: EId, key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fail_property == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge property denied"));
        }
        Ok(self.edges.get(&eid).map(|edge| edge.props.iter()
            .find(|(found, _)| *found == key).map(|(_, value)| value)))
    }
    fn vertex<'a, C>(&'a self, vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow { labels: &[], properties }))
    }
    fn next_probe_vertex<C>(&self, after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        Ok(self.vertices.range((after.map_or(Unbounded, Excluded), Unbounded))
            .next().map(|(&id, _)| id))
    }
    fn next_incident_edge<C>(&self, endpoint: VId, direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        let seek = |map: &BTreeMap<VId, BTreeSet<EId>>| {
            map.get(&endpoint).and_then(|set| set.range((after.map_or(Unbounded, Excluded), Unbounded))
                .next().copied())
        };
        Ok(match direction {
            GlaDirection::Forward => seek(&self.outgoing),
            GlaDirection::Reverse => seek(&self.incoming),
            GlaDirection::Undirected => match (seek(&self.outgoing), seek(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b),
            },
        })
    }
}
impl VertexScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        self.roots.fetch_add(1, Ordering::SeqCst);
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = self.vertices.range((self.vertex_after.map_or(Unbounded, Excluded), Unbounded))
            .next().map(|(&id, _)| id);
        if next.is_some() { self.vertex_after = next; }
        Ok(next)
    }
    fn vertex<'a, C>(&'a self, vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(self.vertices.get(&vid).map(|properties| VertexScanRow { labels: &[], properties }))
    }
    fn next_probe_vertex<C>(&self, after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::next_probe_vertex(self, after, control)
    }
    fn next_probe_edge<C>(&self, endpoint: VId, direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::next_incident_edge(self, endpoint, direction, after, control)
    }
    fn probe_edge<'a, C>(&'a self, eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        EdgeScanSource::edge(self, eid, control).map_err(EdgeExpansionSourceError::Read)
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 10_000, 1_000_000, 1_000_000) }
fn eager(f: &Fixture, plan: &PreparedGraphPattern<GraphValueRow>) -> Vec<GraphValueRow> {
    plan.plan().execute_governed_with_element_properties(
        (f.edges.len() + f.vertices.len()) as u64,
        f.vertices.keys().copied(),
        f.edges.iter().map(|(&id, edge)| (id, edge.source, edge.relation, edge.target)),
        |_, _| Ok::<_, ()>(true),
        |vid, key| Ok(f.vertices.get(&vid).and_then(|p| p.iter().find(|(k, _)| *k == key)).map(|(_, v)| v)),
        |eid, key| Ok(f.edges.get(&eid).and_then(|e| e.props.iter().find(|(k, _)| *k == key)).map(|(_, v)| v)),
        policy(), || Ok::<_, ()>(()),
    ).unwrap().value
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn eager_and_vertex_pull_match_an_independent_parallel_edge_property_oracle() {
    for encoded in 0..256_u32 {
        let values: [Option<CanonicalScalar>; 4] = std::array::from_fn(|at| match (encoded >> (2 * at)) & 3 {
            0 => None, 1 => Some(CanonicalScalar::Null),
            2 => Some(CanonicalScalar::Int(0)), _ => Some(CanonicalScalar::Int(10)),
        });
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            let atom = match direction {
                GlaDirection::Forward => "(a)-[e:R]->(b)",
                GlaDirection::Reverse => "(a)<-[e:R]-(b)",
                GlaDirection::Undirected => "(a)-[e:R]-(b)",
            };
            for anti in [false, true] {
                let plan = prepare(&format!("MATCH (a) WHERE {}EXISTS {{ MATCH {atom} WHERE e.p > a.p }} RETURN a",
                    if anti { "NOT " } else { "" }));
                let f = Fixture::new(values.clone());
                let expected: Vec<_> = f.vertices.keys().filter(|&&vid| {
                    let found = f.edges.values().any(|edge| {
                        edge.relation == R && match direction {
                            GlaDirection::Forward => edge.source == vid,
                            GlaDirection::Reverse => edge.target == vid,
                            GlaDirection::Undirected => edge.source == vid || edge.target == vid,
                        } && edge.props.iter().any(|(key, value)| *key == P && matches!(value, CanonicalScalar::Int(n) if *n > 5))
                    });
                    found != anti
                }).map(|&vid| vec![GraphValue::Vertex(vid)]).collect();
                assert_eq!(plain(&eager(&f, &plan)), expected);
                let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(plain(&rows), expected, "values={encoded} direction={direction:?} anti={anti}");
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
            VertexScanCursor::new(f, VertexScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(())).collect::<Result<Vec<_>, _>>().unwrap()
        } else {
            EdgeScanCursor::new(f, EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(())).collect::<Result<Vec<_>, _>>().unwrap()
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
    let mut cursor = EdgeScanCursor::new(f, EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(cursor.next().unwrap().is_ok());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    for anti in [false, true] {
        let plan = prepare(&text.replace("WHERE EXISTS", if anti { "WHERE NOT EXISTS" } else { "WHERE EXISTS" }).replace("e.p >= 0", "e.p > 5"));
        let mut f = Fixture::regular();
        f.fail_property = Some(EId(1)); // Failed zero-valued parallel edge cannot end this probe.
        let drops = f.drops.clone();
        let mut cursor = EdgeScanCursor::new(f, EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(EdgeScanError::Source("edge property denied"))))));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn every_capture_checkpoint_fuses_the_cursor_without_losing_delivered_prefixes() {
    let plan = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a, r.p");
    let calls = Cell::new(0);
    let mut good = EdgeScanCursor::new(Fixture::regular(), EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || {
        calls.set(calls.get() + 1); Ok::<_, usize>(())
    });
    let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let total = calls.get();
    assert!(total > 20 && expected.len() > 1);
    for stop in 1..=total {
        let at = Cell::new(0);
        let f = Fixture::regular();
        let drops = f.drops.clone();
        let mut cursor = EdgeScanCursor::new(f, EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || {
            at.set(at.get() + 1);
            if at.get() == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(actual))) => { assert_eq!(actual, stop); break; }
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
    let plan = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a, r.p");
    let mut cursor = EdgeScanCursor::new(Fixture::regular(), EdgeScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    let exact = [rows.snapshot_records, rows.result_rows, evaluator.work_units, evaluator.scratch_entries];
    assert!(exact.iter().all(|value| *value > 0));
    for reduced in 0..=4 {
        let mut limits = exact;
        if reduced < 4 { limits[reduced] -= 1; }
        let mut candidate = EdgeScanCursor::new(Fixture::regular(), EdgeScanPlan::compile(plan.plan()).unwrap(),
            GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]), || Ok::<_, ()>(()));
        let result = candidate.by_ref().collect::<Result<Vec<_>, _>>();
        if reduced == 4 { assert_eq!(result.unwrap(), expected); }
        else {
            assert!(matches!(result, Err(GqlQueryError::Rows(_)) | Err(GqlQueryError::Evaluator(_))));
            assert_eq!(candidate.state(), EdgeScanState::Failed);
        }
    }
    let plan = prepare("MATCH (a) WHERE EXISTS { MATCH (a)-[e:R]->(b) WHERE e.p > 5 } RETURN a LIMIT 0");
    let f = Fixture::regular();
    let roots = f.roots.clone();
    let seeks = f.seeks.clone();
    let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(plan.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(cursor.next().is_none());
    assert_eq!(roots.load(Ordering::SeqCst), 0);
    assert_eq!(seeks.load(Ordering::SeqCst), 0);
}

#[test]
fn physical_preparation_rejects_foreign_or_nonedge_captures_even_at_limit_zero() {
    use crate::algebra::BindingSlot;
    let plan = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[e:R]->(x) WHERE e.p > 5 } RETURN r, a LIMIT 0");
    let original = plan.plan().operators();
    let start = original.iter().position(|op| matches!(op, GlaOperator::Probe { .. })).unwrap();
    let capture_at = (start + 1..original.len()).find(|&at| {
        matches!(original[at], GlaOperator::CapturePath { .. })
    }).unwrap();
    assert!(super::Probe::compile(original, start, 2).is_ok());
    for corruption in 0..5 {
        let mut ops = original.to_vec();
        if corruption == 4 {
            // Keep the typed edge-property reference but remove its declaration.
            // It must not bind the outer r or turn into a missing scalar value.
            ops[capture_at] = GlaOperator::Select { slot: BindingSlot(0), predicates: vec![] };
        } else {
            let GlaOperator::CapturePath { capture, start, segments } = &mut ops[capture_at] else {
                unreachable!("the fixture includes a local relationship capture")
            };
            match corruption {
                0 => segments[0] = BindingSlot(2), // copied b, not an edge expansion
                1 => *start = BindingSlot(0),      // not that expansion's actual source
                2 => *capture = crate::algebra::MAX_PATTERN_IDENTITIES as u32,
                3 => segments.push(segments[0]),  // a compound path is not a relationship
                _ => unreachable!(),
            }
        }
        assert!(super::Probe::compile(&ops, start, 2).is_err());
    }
}
