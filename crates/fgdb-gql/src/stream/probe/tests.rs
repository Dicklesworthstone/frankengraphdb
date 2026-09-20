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
    props: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>,
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
    fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
}
impl Fixture {
    fn from_edges(edges: impl IntoIterator<Item = (u128, u128, u128)>) -> Self {
        let mut f = Self {
            vertices: BTreeSet::new(), props: BTreeMap::new(), edges: BTreeMap::new(),
            outgoing: BTreeMap::new(), incoming: BTreeMap::new(), after: None,
            roots: Arc::new(AtomicUsize::new(0)), seeks: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)), repeat: false, fail_edge: None,
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
        let mut f = Self::from_edges([(0, 0, 0), (1, 0, 1), (2, 0, 1), (3, 1, 2),
            (4, 2, 0), (u128::MAX, 2, 2)].into_iter().enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0).map(|(_, e)| e));
        // Isolates, even on an empty graph, must be candidates for NOT EXISTS.
        f.vertices.extend([VId(0), VId(1), VId(2), VId(3), VId(u128::MAX)]);
        for (id, value) in [(0, CanonicalScalar::Int(3)), (1, CanonicalScalar::Null),
            (2, CanonicalScalar::Int(7)), (u128::MAX, CanonicalScalar::Int(i64::MIN))] {
            f.props.insert(VId(id), vec![(P, value)]);
        }
        f
    }
    fn value(&self, vid: VId) -> CanonicalScalar {
        self.props.get(&vid).and_then(|p| p.first()).map(|p| p.1.clone()).unwrap_or(CanonicalScalar::Null)
    }
}
impl VertexScanSource for Fixture {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(13) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.roots.fetch_add(1, Ordering::SeqCst);
        let next = self.vertices.range((self.after.map_or(Unbounded, Excluded), Unbounded)).next().copied();
        if next.is_some() { self.after = next; }
        Ok(next)
    }
    fn vertex<'a, C>(&'a self, vid: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok(self.vertices.contains(&vid).then_some(VertexScanRow {
            labels: &[], properties: self.props.get(&vid).map_or(&[], Vec::as_slice),
        }))
    }
    fn next_probe_edge<C>(&self, endpoint: VId, direction: GlaDirection, after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        self.seeks.fetch_add(1, Ordering::SeqCst);
        if self.repeat && after.is_some() { return Ok(after); }
        let next = |index: &BTreeMap<VId, BTreeSet<EId>>| index.get(&endpoint)
            .and_then(|set| set.range((after.map_or(Unbounded, Excluded), Unbounded)).next().copied());
        Ok(match direction {
            GlaDirection::Forward => next(&self.outgoing),
            GlaDirection::Reverse => next(&self.incoming),
            GlaDirection::Undirected => match (next(&self.outgoing), next(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b),
            },
        })
    }
    fn probe_edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        if self.fail_edge == Some(eid) {
            return Err(EdgeExpansionSourceError::Read(VertexScanSourceError::Source("edge read denied")));
        }
        Ok(self.edges.get(&eid).map(|&(source, target)| EdgeScanRow { source, target, relation: R, properties: &[] }))
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)), _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 10_000_000) }
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> { rows.iter().map(|r| r.values().to_vec()).collect() }
fn atom(end: &str, d: GlaDirection) -> String {
    match d {
        GlaDirection::Forward => format!("-[:R]->({end})"),
        GlaDirection::Reverse => format!("<-[:R]-({end})"),
        GlaDirection::Undirected => format!("-[:R]-({end})"),
    }
}
fn text(shape: usize, d: GlaDirection, anti: bool, nullable: bool, distinct: bool, skip: usize, limit: usize) -> String {
    let tail = match shape { 1 => atom("a", d), 2 => format!(", (a){}", atom("y", d)), _ => String::new() };
    let condition = if nullable { "NOT (x.p = 7)" } else if shape == 2 { "x <> y" } else { "x <> a" };
    format!("MATCH (a) WHERE {}EXISTS {{ MATCH (a){}{tail} WHERE {condition} }} RETURN {}a, a.p AS p SKIP {skip} LIMIT {limit}",
        if anti { "NOT " } else { "" }, atom("x", d), if distinct { "DISTINCT " } else { "" })
}
// Enumerate every complete inner product. Reduce to a bool only afterward;
// no incidence index, prefix pruning or production Probe is used here.
fn expected(f: &Fixture, shape: usize, d: GlaDirection, anti: bool, nullable: bool, skip: usize, limit: usize) -> Vec<Vec<GraphValue>> {
    let relation: Vec<_> = f.edges.values().flat_map(|&(a, b)| match d {
        GlaDirection::Forward => vec![(a, b)], GlaDirection::Reverse => vec![(b, a)],
        GlaDirection::Undirected if a == b => vec![(a, b)], _ => vec![(a, b), (b, a)],
    }).collect();
    let mut rows = Vec::new();
    for &a in &f.vertices {
        let mut witnesses = 0;
        for &(start, x) in &relation {
            if start != a { continue; }
            let allowed = |y| if nullable { matches!(f.value(x), CanonicalScalar::Int(v) if v != 7) }
                else if shape == 2 { x != y } else { x != a };
            if shape == 0 { if allowed(a) { witnesses += 1; } continue; }
            for &(second, y) in &relation {
                if second != (if shape == 1 { x } else { a }) { continue; }
                if (shape != 1 || y == a) && allowed(y) { witnesses += 1; }
            }
        }
        if (witnesses > 0) != anti { rows.push(vec![GraphValue::Vertex(a), GraphValue::Scalar(f.value(a))]); }
    }
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn vertex_semijoins_include_isolates_and_equal_complete_products_and_eager_gla() {
    for mask in [0, 1, 10, 15, 31, 63] { for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        for shape in 0..3 { for anti in [false, true] { for nullable in [false, true] { for distinct in [false, true] {
            for (skip, limit) in [(0, 20), (1, 2), (0, 0)] {
                let input = text(shape, direction, anti, nullable, distinct, skip, limit);
                let q = prepare(&input); let f = Fixture::small(mask);
                let want = expected(&f, shape, direction, anti, nullable, skip, limit);
                let eager = q.plan().execute_governed_with_element_properties(f.vertices.len() as u64 + f.edges.len() as u64,
                    f.vertices.iter().copied(), f.edges.iter().map(|(&id, &(a, b))| (id, a, R, b)),
                    |vid, predicates| Ok::<_, ()>(predicates.iter().all(|p| p.matches_borrowed([], f.props.get(&vid).into_iter().flatten().map(|(k,v)| (*k,v))))),
                    |vid, _| Ok(f.props.get(&vid).and_then(|p| p.first()).map(|p| &p.1)), |_, _| Ok(None), policy(), || Ok::<_, ()>(())).unwrap();
                assert_eq!(plain(&eager.value), want, "eager {input}");
                let mut stream = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
                let mut actual = Vec::new();
                for page in [1, 2, 16] { for _ in 0..page {
                    let Some(row) = stream.next() else { break; }; actual.push(row.unwrap());
                } }
                assert_eq!(plain(&actual), want, "stream {input}");
                assert_eq!(stream.row_stats().result_rows, want.len() as u64);
                assert_eq!(stream.state(), VertexScanState::Exhausted);
            }
        } } } }
    } }
}

#[test]
fn first_witness_never_reads_a_bad_suffix_and_probe_failures_are_not_absence() {
    let input = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a LIMIT 1";
    let q = prepare(input); let mut f = Fixture::small(63); f.repeat = true; f.fail_edge = Some(EId(1));
    let seeks = f.seeks.clone(); let roots = f.roots.clone();
    let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(),
        GqlQueryPolicy::new(2, 1, 100_000, 10_000), || Ok::<_, ()>(()));
    assert_eq!(cursor.next().unwrap().unwrap().values(), &[GraphValue::Vertex(VId(0))]);
    assert_eq!(cursor.row_stats().snapshot_records, 2);
    assert_eq!(seeks.load(Ordering::SeqCst), 1); assert_eq!(roots.load(Ordering::SeqCst), 1);
    assert!(cursor.next().is_none());
    assert_eq!(seeks.load(Ordering::SeqCst), 1);
    let q = prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) WHERE x.p > 100 } RETURN a");
    let mut f = Fixture::small(63); f.fail_edge = Some(EId(1));
    let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(VertexScanError::Source("edge read denied"))))));
    assert_eq!(cursor.row_stats().result_rows, 0); assert!(cursor.next().is_none());
    let mut f = Fixture::small(63); f.repeat = true;
    let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::NonIncreasingIdentity))))));
}

struct NoLookup(Fixture);
impl VertexScanSource for NoLookup {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { self.0.snapshot_seq() }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> { self.0.next_vertex(control) }
    fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> { self.0.vertex(id, control) }
}
struct OnlyIndex(Fixture);
impl VertexScanSource for OnlyIndex {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { self.0.snapshot_seq() }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> { self.0.next_vertex(control) }
    fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> { self.0.vertex(id, control) }
    fn next_probe_edge<C>(&self, at: VId, d: GlaDirection, after: Option<EId>, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> { self.0.next_probe_edge(at, d, after, control) }
}

#[test]
fn both_optional_source_seams_refuse_but_vertex_only_probes_need_neither() {
    let q = prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) } RETURN a");
    let mut absent = VertexScanCursor::new(NoLookup(Fixture::small(63)), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(matches!(absent.next(), Some(Err(GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::ExpansionUnavailable))))));
    let mut partial = VertexScanCursor::new(OnlyIndex(Fixture::small(63)), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert!(matches!(partial.next(), Some(Err(GqlQueryError::Source(VertexScanError::Probe(EdgeScanError::ExpansionUnavailable))))));
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (a) WHERE a.p = 7 } RETURN a");
    let cursor = VertexScanCursor::new(NoLookup(Fixture::small(0)), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert_eq!(plain(&cursor.collect::<Result<Vec<_>, _>>().unwrap()), vec![vec![GraphValue::Vertex(VId(2))]]);
    let q = prepare("MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } AND NOT EXISTS { MATCH (a)-[:R]->(x) WHERE x.p IS NULL } RETURN a");
    let cursor = VertexScanCursor::new(Fixture::small(63), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, ()>(()));
    assert_eq!(plain(&cursor.collect::<Result<Vec<_>, _>>().unwrap()), vec![vec![GraphValue::Vertex(VId(1))], vec![GraphValue::Vertex(VId(2))]]);
}

#[test]
fn every_interruption_is_terminal_retains_only_delivered_rows_and_retries_exactly() {
    for anti in [false, true] {
        let q = prepare(&text(1, GlaDirection::Undirected, anti, true, false, 0, 20));
        let mut total = 0;
        let mut good = VertexScanCursor::new(Fixture::small(63), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || { total += 1; Ok::<_, usize>(()) });
        let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap(); drop(good);
        for stop in 1..=total {
            let mut calls = 0; let f = Fixture::small(63); let drops = f.drops.clone();
            let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(), policy(), || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
            let mut delivered = Vec::new();
            loop { match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => { assert_eq!(at, stop); break; }
                other => panic!("interruption became {other:?}"),
            } }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Failed); assert!(cursor.next().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1); drop(cursor); assert_eq!(calls, stop);
            let retry = VertexScanCursor::new(Fixture::small(63), VertexScanPlan::compile(q.plan()).unwrap(), policy(), || Ok::<_, usize>(()));
            assert_eq!(retry.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        }
    }
}

#[test]
fn exact_limits_cover_outer_candidates_and_all_inner_work_without_resetting() {
    let q = prepare(&text(2, GlaDirection::Undirected, true, true, false, 0, 20));
    let run = |p| VertexScanCursor::new(Fixture::small(63), VertexScanPlan::compile(q.plan()).unwrap(), p, || Ok::<_, ()>(()));
    let mut good = run(policy()); let expected = good.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let r = good.row_stats(); let e = good.evaluator_stats();
    assert!(!expected.is_empty()); assert!(r.snapshot_records > 5);
    let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
    let mut retry = run(exact); assert_eq!(retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    assert_eq!((retry.row_stats(), retry.evaluator_stats()), (r, e));
    for p in [GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1)] {
        let mut cursor = run(p); assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed); assert!(cursor.next().is_none());
    }
}

#[test]
fn full_depth_probes_find_first_witness_without_an_outer_edge_or_result_bag() {
    for hops in [40, crate::algebra::MAX_PATTERN_EDGES] {
        let mut input = "MATCH (a) WHERE EXISTS { MATCH (a)".to_owned();
        for i in 0..hops { input.push_str(&format!("-[:R]->(v{})", i + 1)); }
        input.push_str(" } RETURN a LIMIT 1");
        let q = prepare(&input);
        let f = Fixture::from_edges((0..hops as u128).flat_map(|i| [(2*i, i, i+1), (2*i+1, i, i+1)]));
        let seeks = f.seeks.clone(); let drops = f.drops.clone();
        let mut cursor = VertexScanCursor::new(f, VertexScanPlan::compile(q.plan()).unwrap(),
            GqlQueryPolicy::new(hops as u64 + 1, 1, 100_000, 10_000), || Ok::<_, ()>(()));
        assert_eq!(cursor.next().unwrap().unwrap().values(), &[GraphValue::Vertex(VId(0))]);
        assert_eq!(cursor.row_stats().snapshot_records, hops as u64 + 1);
        assert_eq!(seeks.load(Ordering::SeqCst), hops); assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unsupported_scopes_and_outputs_refuse_even_at_limit_zero_without_driving_a_source() {
    for input in [
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(x) RETURN a, x LIMIT 0",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a.p LIMIT 0",
    ] { let q = prepare(input); assert!(VertexScanPlan::compile(q.plan()).is_err(), "{input}"); }
    for body in ["MATCH (a)-[:R*1..3]->(x)", "MATCH (x)-[:R*1..3]->(y)"] {
        let q = prepare(&format!("MATCH (a) WHERE NOT EXISTS {{ {body} }} RETURN a LIMIT 0"));
        let f = Fixture::small(63); let roots = f.roots.clone(); let seeks = f.seeks.clone();
        let mut cursor = VertexScanCursor::new(NoLookup(f), VertexScanPlan::compile(q.plan()).unwrap(), GqlQueryPolicy::new(0,0,10,0), || Ok::<_, ()>(()));
        assert!(cursor.next().is_none());
        assert_eq!(roots.load(Ordering::SeqCst), 0); assert_eq!(seeks.load(Ordering::SeqCst), 0);
    }
}

mod independent;
