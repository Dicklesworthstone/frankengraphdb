use super::*;
use crate::algebra::{GlaPlan, GraphValue, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

mod vertex_probe;

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(3);
const Q: PropertyKeyId = PropertyKeyId(4);
const IDS: [VId; 4] = [VId(0), VId(1 << 100), VId(u128::MAX - 1), VId(u128::MAX)];
const EDGES: [(u128, usize, usize); 5] = [
    (0, 0, 1),
    (5, 0, 1),
    (9, 1, 2),
    (12, 2, 0),
    (u128::MAX, 0, 0),
];

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..1024 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("ready fixture did not complete");
}
fn prepared(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "missing") => Some(GraphSymbol::Property(PropertyKeyId(5))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn definition(
    text: &str,
    search: GraphWalkSearch,
    different_edges: bool,
) -> GlaPlan<GraphValueRow> {
    let prepared = prepared(text);
    let mut operators = prepared.plan().operators().to_vec();
    for op in &mut operators {
        if let GlaOperator::VarLengthExpand { search: mode, .. } = op {
            *mode = search;
        }
    }
    if !different_edges {
        operators.retain(|op| !matches!(op, GlaOperator::DifferentEdges { .. }));
    }
    GlaPlan::from_operators(operators)
}
fn statement(direction: GlaDirection, minimum: u32, maximum: u32) -> String {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    format!(
        "MATCH p = (a){left}[:R*{minimum}..{maximum}]{right}(b) RETURN ALL a, b, p, length(p) AS n, nodes(p) AS vs, relationships(p) AS es, a.p AS ap, b.p AS bp"
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 100_000_000, 100_000_000)
}
fn ok() -> Result<(), usize> {
    Ok(())
}

#[derive(Default)]
struct Counts {
    records: AtomicUsize,
    peak: AtomicUsize,
    guards: AtomicUsize,
    roots: AtomicUsize,
    seeks: AtomicUsize,
    drops: AtomicUsize,
}
struct Data {
    edges: Vec<(EId, VId, VId)>,
    edge_properties: BTreeMap<EId, Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
    vertices: BTreeMap<VId, Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
}
struct Root {
    data: Arc<Data>,
    vid: VId,
    counts: Arc<Counts>,
    scratch: Cell<usize>,
}
impl AsyncVertexScanRecord for Root {
    fn as_row(&self) -> VertexScanRow<'_> {
        VertexScanRow {
            labels: &[],
            properties: &self.data.vertices[&self.vid],
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        self.counts.records.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Record {
    data: Arc<Data>,
    index: usize,
    counts: Arc<Counts>,
    scratch: Cell<usize>,
    dangling: bool,
}
impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        let (eid, source, target) = self.data.edges[self.index];
        EdgeScanRow {
            source,
            target,
            relation: R,
            properties: self
                .data
                .edge_properties
                .get(&eid)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        }
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        if self.dangling {
            return None;
        }
        let (_, source, target) = self.data.edges[self.index];
        if vid != source && vid != target {
            return None;
        }
        self.data
            .vertices
            .get(&vid)
            .map(|properties| VertexScanRow {
                labels: &[],
                properties,
            })
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        self.counts.records.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Guard(Arc<Counts>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.guards.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Io,
    Unavailable,
    Missing,
    Twice,
    Mismatch,
    FalseEof,
    Repeat,
    Pending,
    Dangling,
}
struct Source {
    data: Arc<Data>,
    counts: Arc<Counts>,
    root: usize,
    root_fault: Fault,
    edge_fault: Fault,
    hidden: Option<EId>,
    refuse_backing: bool,
    refuse_output: bool,
    refuse_evaluation: bool,
    scratch_limit: usize,
}
impl Source {
    fn new(mask: usize) -> Self {
        Self {
            data: Arc::new(Data {
                edges: EDGES
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| mask & (1 << at) != 0)
                    .map(|(_, &(id, from, to))| (EId(id), IDS[from], IDS[to]))
                    .collect(),
                edge_properties: BTreeMap::new(),
                vertices: IDS
                    .iter()
                    .enumerate()
                    .map(|(at, &vid)| (vid, vec![(P, CanonicalScalar::Int(at as i64 + 1).into())]))
                    .collect(),
            }),
            counts: Arc::default(),
            root: 0,
            root_fault: Fault::None,
            edge_fault: Fault::None,
            hidden: None,
            refuse_backing: false,
            refuse_output: false,
            refuse_evaluation: false,
            scratch_limit: usize::MAX,
        }
    }
    fn with_edge_properties(
        mut self,
        properties: Vec<(EId, Vec<(PropertyKeyId, CanonicalPropertyValue)>)>,
    ) -> Self {
        Arc::get_mut(&mut self.data).unwrap().edge_properties = properties.into_iter().collect();
        self
    }
    fn live(&self) {
        let live = self.counts.records.fetch_add(1, Ordering::SeqCst) + 1;
        self.counts.peak.fetch_max(live, Ordering::SeqCst);
    }
    fn guard<C>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.counts.guards.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(self.counts.clone()))
    }
    fn scratch(&self, scratch: &Cell<usize>, event: GlaExecutionEvent) -> Result<(), &'static str> {
        if event == GlaExecutionEvent::ScratchEntry {
            if scratch.get() >= self.scratch_limit {
                return Err("temporary reservation refused");
            }
            scratch.set(scratch.get() + 1);
        }
        Ok(())
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl AsyncEdgeScanSource for Source {
    type Error = &'static str;
    type Record = Record;
    type OutputGuard = Guard;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(41)
    }
    async fn next_candidate<C: Send>(
        &mut self,
        _: EdgeRelation,
        _: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<Self::Error, C>> {
        panic!("vertex-root walk must not open a whole-edge scan")
    }
    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        panic!("path output must include its root and complete route")
    }
    fn evaluation_event(
        &self,
        record: &Record,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if self.refuse_evaluation {
            return Err("candidate evaluation refused");
        }
        self.scratch(&record.scratch, event)
    }
}
impl AsyncEdgeJoinSource for Source {
    type TraversalGuard = Guard;
    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        _: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> AsyncIncidentCandidateResult<Record, Self::Error, C> {
        self.counts.seeks.fetch_add(1, Ordering::SeqCst);
        if self.edge_fault == Fault::Pending {
            std::future::pending::<()>().await;
        }
        if self.edge_fault == Fault::Unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        if self.edge_fault == Fault::Io {
            return Err(EdgeExpansionSourceError::Read(EdgeScanSourceError::Source(
                "edge read",
            )));
        }
        let emit =
            |this: &Self, control: &mut (dyn FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send)| {
                control(AsyncEdgeScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
                let index = this.data.edges.iter().position(|&(eid, from, to)| {
                    (this.edge_fault == Fault::Repeat || after.is_none_or(|after| eid > after))
                        && match direction {
                            GlaDirection::Forward => from == endpoint,
                            GlaDirection::Reverse => to == endpoint,
                            GlaDirection::Undirected => from == endpoint || to == endpoint,
                        }
                });
                let Some(index) = index else { return Ok(None) };
                let (eid, _, _) = this.data.edges[index];
                if this.edge_fault != Fault::Missing {
                    control(AsyncEdgeScanEvent::Candidate(eid))
                        .map_err(EdgeScanSourceError::Control)?;
                }
                if this.edge_fault == Fault::Twice {
                    control(AsyncEdgeScanEvent::Candidate(eid))
                        .map_err(EdgeScanSourceError::Control)?;
                }
                if this.edge_fault == Fault::FalseEof {
                    return Ok(None);
                }
                control(AsyncEdgeScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
                let record = if this.hidden == Some(eid) {
                    None
                } else {
                    this.live();
                    Some(Record {
                        data: this.data.clone(),
                        index,
                        counts: this.counts.clone(),
                        scratch: Cell::new(0),
                        dangling: this.edge_fault == Fault::Dangling,
                    })
                };
                Ok(Some(AsyncEdgeCandidate {
                    eid: if this.edge_fault == Fault::Mismatch {
                        EId(eid.0 ^ 1)
                    } else {
                        eid
                    },
                    record,
                }))
            };
        emit(self, control).map_err(EdgeExpansionSourceError::Read)
    }
    fn reserve_traversal<C>(
        &self,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        if self.refuse_backing {
            return Err(EdgeScanSourceError::Source("route backing refused"));
        }
        self.guard(control)
    }
    fn reserve_join_output<C>(
        &self,
        _: &[Record],
        _: usize,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        panic!("walk output must include zero-hop/root values")
    }
}
impl AsyncGraphWalkSource for Source {
    type RootRecord = Root;
    async fn next_root_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Root>>, EdgeScanSourceError<Self::Error, C>> {
        self.counts.roots.fetch_add(1, Ordering::SeqCst);
        if self.root_fault == Fault::Pending {
            std::future::pending::<()>().await;
        }
        if self.root_fault == Fault::Io {
            return Err(EdgeScanSourceError::Source("root read"));
        }
        control(AsyncVertexScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let Some(&vid) = IDS.get(if self.root_fault == Fault::Repeat {
            0
        } else {
            self.root
        }) else {
            return Ok(None);
        };
        if self.root_fault != Fault::Missing {
            control(AsyncVertexScanEvent::Candidate(vid)).map_err(EdgeScanSourceError::Control)?;
        }
        if self.root_fault == Fault::Twice {
            control(AsyncVertexScanEvent::Candidate(vid)).map_err(EdgeScanSourceError::Control)?;
        }
        if self.root_fault == Fault::FalseEof {
            return Ok(None);
        }
        control(AsyncVertexScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.root += 1;
        self.live();
        let root = Root {
            data: self.data.clone(),
            vid,
            counts: self.counts.clone(),
            scratch: Cell::new(0),
        };
        Ok(Some(AsyncVertexCandidate {
            vid: if self.root_fault == Fault::Mismatch {
                VId(vid.0 ^ 1)
            } else {
                vid
            },
            record: Some(root),
        }))
    }
    fn root_evaluation_event(
        &self,
        root: &Root,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        self.scratch(&root.scratch, event)
    }
    fn reserve_walk_output<C>(
        &self,
        _: &Root,
        _: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        if self.refuse_output {
            return Err(EdgeScanSourceError::Source("output reservation refused"));
        }
        self.guard(control)
    }
}

fn drain(plan: GlaPlan<GraphValueRow>, source: Source) -> Vec<Vec<GraphValue>> {
    let (physical, _) = AsyncGraphWalkPlan::compile_sort_input(&plan).unwrap();
    let counts = source.counts.clone();
    let maximum = physical.maximum_hops();
    let mut cursor = AsyncGraphWalkCursor::new(source, physical, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert_eq!(cursor.snapshot_seq(), CommitSeq(41));
    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
    assert!(counts.peak.load(Ordering::SeqCst) <= maximum + 2); // rejected next candidate can coexist
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert!(run(cursor.next()).is_none());
    rows.sort();
    rows
}

// Independent finite route generator: enumerate edge sequences first, then
// test the complete sequence. It shares no cursor state, pruning or admission.
fn oracle(
    data: &Data,
    direction: GlaDirection,
    minimum: u32,
    maximum: u32,
    search: GraphWalkSearch,
    different: bool,
) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    for &start in data.vertices.keys() {
        let mut frontier = vec![Vec::<(EId, VId)>::new()];
        for depth in 0..=maximum {
            for steps in &frontier {
                let vertices = std::iter::once(start)
                    .chain(steps.iter().map(|s| s.1))
                    .collect::<Vec<_>>();
                let edge_unique = steps
                    .iter()
                    .enumerate()
                    .all(|(at, step)| !steps[..at].iter().any(|p| p.0 == step.0));
                let vertex_unique = vertices
                    .iter()
                    .enumerate()
                    .all(|(at, v)| !vertices[..at].contains(v));
                let simple = vertices.iter().enumerate().all(|(at, v)| {
                    !vertices[..at].contains(v) || (at + 1 == vertices.len() && *v == start)
                });
                let valid = (!different || edge_unique)
                    && match search {
                        GraphWalkSearch::All => true,
                        GraphWalkSearch::Trail => edge_unique,
                        GraphWalkSearch::Acyclic => vertex_unique,
                        GraphWalkSearch::Simple => simple,
                        _ => unreachable!(),
                    };
                if depth >= minimum && valid {
                    let endpoint = *vertices.last().unwrap();
                    rows.push(vec![
                        GraphValue::Vertex(start),
                        GraphValue::Vertex(endpoint),
                        GraphValue::Path(GraphPath::new(start, steps.clone().into_boxed_slice())),
                        GraphValue::Scalar(CanonicalScalar::Int(depth as i64)),
                        GraphValue::Vertices(vertices.into_boxed_slice()),
                        GraphValue::Edges(
                            steps
                                .iter()
                                .map(|s| s.0)
                                .collect::<Vec<_>>()
                                .into_boxed_slice(),
                        ),
                        GraphValue::from_property_value(&data.vertices[&start][0].1).unwrap(),
                        GraphValue::from_property_value(&data.vertices[&endpoint][0].1).unwrap(),
                    ]);
                }
            }
            if depth == maximum {
                break;
            }
            let mut next = Vec::new();
            for steps in frontier {
                let endpoint = steps.last().map_or(start, |s| s.1);
                for &(eid, from, to) in &data.edges {
                    let target = match direction {
                        GlaDirection::Forward if from == endpoint => to,
                        GlaDirection::Reverse if to == endpoint => from,
                        GlaDirection::Undirected if from == endpoint => to,
                        GlaDirection::Undirected if to == endpoint => from,
                        _ => continue,
                    };
                    let mut route = steps.clone();
                    route.push((eid, target));
                    next.push(route);
                }
            }
            frontier = next;
        }
    }
    rows.sort();
    rows
}

#[test]
fn every_bounded_mode_matches_independent_routes_and_the_native_executor() {
    let mut cases = 0;
    for mask in 0..32 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for search in [
                GraphWalkSearch::All,
                GraphWalkSearch::Trail,
                GraphWalkSearch::Acyclic,
                GraphWalkSearch::Simple,
            ] {
                for different in [false, true] {
                    for (minimum, maximum) in [(0, 0), (0, 3), (1, 2), (2, 3), (3, 3)] {
                        let source = Source::new(mask);
                        let text = statement(direction, minimum, maximum);
                        let logical = definition(&text, search, different);
                        let expected =
                            oracle(&source.data, direction, minimum, maximum, search, different);
                        let native = logical
                            .execute_with_identified_properties_control(
                                source.data.vertices.keys().copied(),
                                source.data.edges.iter().map(|&(eid, a, b)| (eid, a, R, b)),
                                |vid, predicates| {
                                    Ok::<_, crate::GraphIntegerError>(
                                        predicates
                                            .iter()
                                            .all(|p| p.matches(&[], &source.data.vertices[&vid])),
                                    )
                                },
                                |vid, key| {
                                    Ok(source.data.vertices[&vid]
                                        .iter()
                                        .find(|p| p.0 == key)
                                        .map(|p| &p.1))
                                },
                                |_| Ok(()),
                            )
                            .unwrap();
                        assert_eq!(
                            native
                                .iter()
                                .map(|r| r.values().to_vec())
                                .collect::<Vec<_>>(),
                            expected,
                            "native {text} mask {mask} {search:?} different {different}"
                        );
                        assert_eq!(
                            drain(logical, source),
                            expected,
                            "stream {text} mask {mask} {search:?} different {different}"
                        );
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 3840);
}

#[test]
fn endpoint_and_path_filters_do_not_prune_transit_and_zero_hops_keep_isolates() {
    let query = "MATCH p = (a {p:1})-[:R*0..3]->(b {p:3}) WHERE length(p) >= 2 RETURN ALL a, b, p";
    let rows = drain(
        definition(query, GraphWalkSearch::All, true),
        Source::new(31),
    );
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row|row[0]==GraphValue::Vertex(IDS[0]) && row[1]==GraphValue::Vertex(IDS[2])));
    let rows = drain(
        definition(
            "MATCH p = (a)-[:R*0..0]->(b) WHERE p IS NOT NULL RETURN ALL a, b, p",
            GraphWalkSearch::All,
            true,
        ),
        Source::new(0),
    );
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().any(|row| row[0] == GraphValue::Vertex(IDS[3])));
    assert!(
        rows.iter()
            .all(|row| matches!(&row[2],GraphValue::Path(path) if path.is_empty()))
    );
}

#[test]
fn complete_input_precedes_limit_distinct_and_hidden_sort_keys() {
    let logical =
        prepared("MATCH p = (a)-[:R*0..2]->(b) RETURN a.p AS value ORDER BY b.p DESC LIMIT 0");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert_eq!(tail.count(), Some(0));
    assert_eq!(tail.visible_width(), 1);
    assert_eq!(tail.evaluation_width(), 2);
    let source = Source::new(31);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let mut count = 0;
    while let Some(row) = run(cursor.next()) {
        assert_eq!(row.unwrap().values().len(), 2);
        count += 1;
    }
    assert!(count > 4);
    assert_eq!(counts.roots.load(Ordering::SeqCst), 5);
    let logical = prepared("MATCH (a)-[:R*1..2]->(b) RETURN DISTINCT b LIMIT 1");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert!(tail.distinct());
    assert_eq!(tail.count(), Some(1));
    let mut cursor = AsyncGraphWalkCursor::new(Source::new(3), plan, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec())
    }
    assert!(rows.len() > 1);
    assert!(rows.windows(2).any(|pair| pair[0] == pair[1]));
}

#[test]
fn unsupported_definitions_refuse_before_a_zero_limit_or_source() {
    for text in [
        "MATCH p = (a)-[:R*]->(b) RETURN p LIMIT 0",
        "MATCH p = shortestPath((a)-[:R*0..3]->(b)) RETURN p LIMIT 0",
        "MATCH p = (a)-[:R*1..2]->(b)-[:R]->(c) RETURN p LIMIT 0",
        "MATCH (a)-[:R*0..2 {p:[]}]->(b) RETURN b LIMIT 0",
        "MATCH (a)-[:R*0..2 {p:{nested:1}}]->(b) RETURN b LIMIT 0",
        "MATCH (a)-[:R*0..2]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN b LIMIT 0",
        "MATCH (a)-[:R*0..2]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN b LIMIT 0",
    ] {
        let logical = prepared(text);
        assert!(
            AsyncGraphWalkPlan::compile_sort_input(logical.plan()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn source_failures_never_become_absence_and_every_refusal_releases_ownership() {
    let query = statement(GlaDirection::Forward, 0, 2);
    for root_fault in [false, true] {
        for fault in [
            Fault::Io,
            Fault::Missing,
            Fault::Twice,
            Fault::Mismatch,
            Fault::FalseEof,
            Fault::Repeat,
        ] {
            let mut source = Source::new(31);
            if root_fault {
                source.root_fault = fault
            } else {
                source.edge_fault = fault
            }
            let counts = source.counts.clone();
            let plan = AsyncGraphWalkPlan::compile_sort_input(&definition(
                &query,
                GraphWalkSearch::All,
                false,
            ))
            .unwrap()
            .0;
            let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
            let mut error = false;
            while let Some(row) = run(cursor.next()) {
                if row.is_err() {
                    error = true;
                    break;
                }
            }
            assert!(error);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(run(cursor.next()).is_none());
            assert_eq!(counts.records.load(Ordering::SeqCst), 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        }
    }
    for fault in [Fault::Unavailable, Fault::Dangling] {
        let mut source = Source::new(31);
        source.edge_fault = fault;
        let plan = AsyncGraphWalkPlan::compile_sort_input(&definition(
            &query,
            GraphWalkSearch::All,
            false,
        ))
        .unwrap()
        .0;
        let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
        assert!(run(cursor.next()).unwrap().is_ok());
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
    }
    for kind in 0..3 {
        let mut source = Source::new(31);
        match kind {
            0 => source.refuse_backing = true,
            1 => source.refuse_output = true,
            _ => source.scratch_limit = 0,
        }
        let counts = source.counts.clone();
        let plan = AsyncGraphWalkPlan::compile_sort_input(&definition(
            &query,
            GraphWalkSearch::All,
            false,
        ))
        .unwrap()
        .0;
        let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn all_live_checkpoints_and_exact_cumulative_limits_are_terminal() {
    let logical = definition(
        &statement(GlaDirection::Forward, 0, 2),
        GraphWalkSearch::All,
        true,
    );
    let plan = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap().0;
    check_all_live_limits(plan, || Source::new(1));
}

#[test]
fn scalar_edge_filter_payload_checkpoints_and_limits_are_terminal() {
    let payload = "x".repeat(4096);
    let logical = prepared(&format!(
        "MATCH (a)-[:R*1..2 {{p:'{payload}'}}]->(b) RETURN b LIMIT 0"
    ));
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    check_all_live_limits(plan, || {
        Source::new(1).with_edge_properties(vec![(
            EId(0),
            vec![(P, CanonicalScalar::ucs_basic_text(&payload).unwrap().into())],
        )])
    });
}

fn check_all_live_limits(plan: AsyncGraphWalkPlan, source: impl Fn() -> Source) {
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source(), plan.clone(), wide(), move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, usize>(())
    });
    while let Some(row) = run(cursor.next()) {
        row.unwrap();
    }
    let checkpoints = seen.load(Ordering::SeqCst);
    let stats = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    for fail_at in 0..checkpoints {
        let input = source();
        let counts = input.counts.clone();
        let mut called = 0;
        let mut cursor = AsyncGraphWalkCursor::new(input, plan.clone(), wide(), move || {
            let n = called;
            called += 1;
            if n == fail_at { Err(n) } else { Ok(()) }
        });
        let mut error = None;
        while let Some(row) = run(cursor.next()) {
            if let Err(e) = row {
                error = Some(e);
                break;
            }
        }
        assert!(matches!(error,Some(GqlQueryError::Interrupted(at)) if at==fail_at));
        assert!(run(cursor.next()).is_none());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let exact = [
        stats.snapshot_records,
        stats.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    for dimension in 0..5 {
        let mut limits = exact;
        if dimension < 4 {
            assert!(limits[dimension] > 0);
            limits[dimension] -= 1
        }
        let policy = GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]);
        let mut cursor = AsyncGraphWalkCursor::new(source(), plan.clone(), policy, ok);
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            if row.is_err() {
                failed = true;
                break;
            }
        }
        assert_eq!(failed, dimension < 4);
    }
}

#[test]
fn pending_drop_is_terminal_and_outputs_keep_their_independent_guards() {
    let logical = definition(
        &statement(GlaDirection::Forward, 0, 2),
        GraphWalkSearch::All,
        false,
    );
    let plan = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap().0;
    let source = Source::new(31);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    cursor.close();
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    drop(row);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    for root_pending in [false, true] {
        let mut source = Source::new(31);
        if root_pending {
            source.root_fault = Fault::Pending
        } else {
            source.edge_fault = Fault::Pending
        }
        let counts = source.counts.clone();
        let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
        // Merely creating and dropping an UNPOLLED next does not change state.
        drop(cursor.next::<usize>());
        assert_eq!(cursor.state(), EdgeScanState::Open);
        if !root_pending {
            drop(run(cursor.next()).unwrap().unwrap())
        }
        let mut future = Box::pin(cursor.next::<usize>());
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&future);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(future);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    }
}

fn scalar_edges(mask: usize) -> Source {
    use fgdb_types::CanonicalF64;

    Source::new(mask).with_edge_properties(vec![
        (
            EId(0),
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Bool(true).into()),
            ],
        ),
        (
            EId(5),
            vec![
                (P, CanonicalScalar::Float(CanonicalF64::new(7.0)).into()),
                (Q, CanonicalScalar::Bool(false).into()),
            ],
        ),
        (EId(9), vec![(P, CanonicalScalar::Int(7).into())]),
        (
            EId(12),
            vec![(P, CanonicalScalar::ucs_basic_text("7").unwrap().into())],
        ),
        (
            EId(u128::MAX),
            vec![
                (P, CanonicalScalar::Bool(true).into()),
                (Q, CanonicalScalar::Bool(true).into()),
            ],
        ),
    ])
}

fn native_with_edge_properties(plan: &GlaPlan<GraphValueRow>, data: &Data) -> Vec<Vec<GraphValue>> {
    plan.execute_with_element_properties_control(
        data.vertices.keys().copied(),
        data.edges.iter().map(|&(eid, a, b)| (eid, a, R, b)),
        |vid, predicates| {
            Ok::<_, crate::GraphIntegerError>(
                predicates
                    .iter()
                    .all(|p| p.matches(&[], &data.vertices[&vid])),
            )
        },
        |vid, key| {
            Ok(data.vertices[&vid]
                .iter()
                .find(|p| p.0 == key)
                .map(|p| &p.1))
        },
        |eid, key| {
            Ok(data
                .edge_properties
                .get(&eid)
                .and_then(|properties| properties.iter().find(|p| p.0 == key))
                .map(|p| &p.1))
        },
        |_| Ok(None),
        |_| Ok(None),
        |_| Ok(()),
    )
    .unwrap()
    .into_iter()
    .map(|row| row.values().to_vec())
    .collect()
}

#[test]
fn scalar_edge_maps_match_literal_edge_selection_and_native_complete_routes() {
    // Accepted identities are literal facts about this fixture, independent
    // of the native comparator and the cursor's per-candidate pruning.
    let mut cases = 0;
    for mask in [0, 1, 3, 7, 15, 31] {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for search in [
                GraphWalkSearch::All,
                GraphWalkSearch::Trail,
                GraphWalkSearch::Acyclic,
                GraphWalkSearch::Simple,
            ] {
                for different in [false, true] {
                    for (minimum, maximum) in [(0, 0), (0, 3), (1, 2)] {
                        let source = scalar_edges(mask);
                        let text =
                            statement(direction, minimum, maximum).replacen(']', " {p:7}]", 1);
                        let logical = definition(&text, search, different);
                        let accepted = Data {
                            edges: source
                                .data
                                .edges
                                .iter()
                                .copied()
                                .filter(|(eid, _, _)| [EId(0), EId(5), EId(9)].contains(eid))
                                .collect(),
                            edge_properties: BTreeMap::new(),
                            vertices: source.data.vertices.clone(),
                        };
                        let expected =
                            oracle(&accepted, direction, minimum, maximum, search, different);
                        assert_eq!(
                            native_with_edge_properties(&logical, &source.data),
                            expected
                        );
                        assert_eq!(drain(logical, source), expected, "{text} {search:?}");
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 432);
}

#[test]
fn scalar_edge_maps_keep_numeric_domains_nulls_and_conjunctions_native() {
    for (map, accepted) in [
        ("p:7", vec![EId(0), EId(5), EId(9)]),
        ("p:7.0", vec![EId(0), EId(5), EId(9)]),
        ("p:'7'", vec![EId(12)]),
        ("p:true", vec![EId(u128::MAX)]),
        ("p:false", vec![]),
        ("p:null", vec![]),
        ("missing:7", vec![]),
        ("p:7,q:true", vec![EId(0)]),
        ("p:7,q:false", vec![EId(5)]),
    ] {
        for search in [
            GraphWalkSearch::All,
            GraphWalkSearch::Trail,
            GraphWalkSearch::Acyclic,
            GraphWalkSearch::Simple,
        ] {
            let source = scalar_edges(31);
            let text =
                statement(GlaDirection::Undirected, 0, 3).replacen(']', &format!(" {{{map}}}]"), 1);
            let logical = definition(&text, search, false);
            let selected = Data {
                edges: source
                    .data
                    .edges
                    .iter()
                    .copied()
                    .filter(|(eid, _, _)| accepted.contains(eid))
                    .collect(),
                edge_properties: BTreeMap::new(),
                vertices: source.data.vertices.clone(),
            };
            let expected = oracle(&selected, GlaDirection::Undirected, 0, 3, search, false);
            assert_eq!(
                native_with_edge_properties(&logical, &source.data),
                expected
            );
            assert_eq!(drain(logical, source), expected, "{map} {search:?}");
        }
    }
    // Vertex 0 has p=1, but its outgoing edge has explicit NULL. The lookup
    // must stay in the relationship property domain and NULL must not match.
    let source = Source::new(1)
        .with_edge_properties(vec![(EId(0), vec![(P, CanonicalScalar::Null.into())])]);
    let logical = definition(
        "MATCH p = (a)-[:R*1..2 {p:1}]->(b) RETURN p",
        GraphWalkSearch::All,
        false,
    );
    assert!(native_with_edge_properties(&logical, &source.data).is_empty());
    assert!(drain(logical, source).is_empty());
}

#[test]
fn scalar_edge_filter_charges_payloads_and_propagates_candidate_control() {
    fn measured(payload: &str) -> GlaExecutionStats {
        let logical = prepared(&format!(
            "MATCH (a)-[:R*1..1 {{p:'{payload}'}}]->(b) RETURN b"
        ));
        let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
            .unwrap()
            .0;
        let source = Source::new(1).with_edge_properties(vec![(
            EId(0),
            vec![(P, CanonicalScalar::ucs_basic_text(payload).unwrap().into())],
        )]);
        let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
        let future = cursor.next::<usize>();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&future);
        run(future).unwrap().unwrap();
        assert!(run(cursor.next()).is_none());
        cursor.evaluator_stats()
    }
    let short = measured("x");
    let long = measured(&"x".repeat(4096));
    let unit = crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
    assert_eq!(
        long.work_units - short.work_units,
        (2 * (4096usize.div_ceil(unit) - 1usize.div_ceil(unit))) as u64
    );
    assert_eq!(long.scratch_entries, short.scratch_entries);

    let logical = prepared("MATCH (a)-[:R*1..2 {p:7}]->(b) RETURN b LIMIT 0");
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let mut source = scalar_edges(1);
    source.refuse_evaluation = true;
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    assert!(matches!(
        run(cursor.next()).unwrap(),
        Err(GqlQueryError::Source(EdgeScanError::Source(
            "candidate evaluation refused"
        )))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}
