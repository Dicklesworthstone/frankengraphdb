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
    state_bytes: AtomicUsize,
    state_peak: AtomicUsize,
    state_charges: std::sync::Mutex<BTreeMap<usize, usize>>,
    traversal_reservations: AtomicUsize,
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
struct Guard(Arc<Counts>, usize);
impl Drop for Guard {
    fn drop(&mut self) {
        if self.1 != 0 {
            self.0.state_bytes.fetch_sub(self.1, Ordering::SeqCst);
            let mut charges = self.0.state_charges.lock().unwrap();
            let count = charges.get_mut(&self.1).expect("admitted byte guard");
            *count -= 1;
            if *count == 0 {
                charges.remove(&self.1);
            }
        }
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
    state_limit: usize,
    pending_after: usize,
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
            state_limit: usize::MAX,
            pending_after: usize::MAX,
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
        Ok(Guard(self.counts.clone(), 0))
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
        let seek = self.counts.seeks.fetch_add(1, Ordering::SeqCst);
        if self.edge_fault == Fault::Pending || seek == self.pending_after {
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
        self.counts
            .traversal_reservations
            .fetch_add(1, Ordering::SeqCst);
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

    fn reserve_shortest_state<C>(
        &self,
        bytes: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        if self.refuse_backing {
            return Err(EdgeScanSourceError::Source("shortest backing refused"));
        }
        let next = self
            .counts
            .state_bytes
            .load(Ordering::SeqCst)
            .checked_add(bytes)
            .filter(|next| *next <= self.state_limit)
            .ok_or(EdgeScanSourceError::Source("shortest state bytes refused"))?;
        let mut guard = self.guard(control)?;
        guard.1 = bytes;
        self.counts.state_bytes.fetch_add(bytes, Ordering::SeqCst);
        self.counts.state_peak.fetch_max(next, Ordering::SeqCst);
        if bytes != 0 {
            *self
                .counts
                .state_charges
                .lock()
                .unwrap()
                .entry(bytes)
                .or_default() += 1;
        }
        Ok(guard)
    }

    fn reserve_shortest_output<C>(
        &self,
        root: &Root,
        records: &[Record],
        _: usize,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        self.reserve_walk_output(root, records, columns, control)
    }

    fn release_shortest_temporaries(&self, root: &Root, records: &[Record]) {
        root.scratch.set(0);
        for record in records {
            record.scratch.set(0);
        }
    }
}

fn drain(plan: GlaPlan<GraphValueRow>, source: Source) -> Vec<Vec<GraphValue>> {
    let (physical, _) = AsyncGraphWalkPlan::compile_sort_input(&plan).unwrap();
    let counts = source.counts.clone();
    let record_bound = if physical.search == GraphWalkSearch::AnyShortest {
        // Two breadth layers, one root, and one rejected next candidate.
        2 * source.data.vertices.len() + 2
    } else if physical.search == GraphWalkSearch::AllShortest {
        // Shared shortest layers retain each incident orientation at most
        // once per depth, plus one root and one candidate being admitted.
        // After the minimum, every unbounded nonempty layer settles a
        // previously unsettled endpoint from this finite fixture.
        let layers = if physical.bounds.is_bounded() {
            physical.maximum_hops()
        } else {
            physical.bounds.minimum() as usize + source.data.vertices.len()
        };
        2 * source.data.edges.len() * layers + 2
    } else {
        physical.maximum_hops() + 2
    };
    let mut cursor = AsyncGraphWalkCursor::new(source, physical, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert_eq!(cursor.snapshot_seq(), CommitSeq(41));
    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
    assert!(counts.peak.load(Ordering::SeqCst) <= record_bound);
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
                        GraphWalkSearch::All
                        | GraphWalkSearch::AnyShortest
                        | GraphWalkSearch::AllShortest => true,
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
    if search == GraphWalkSearch::AnyShortest {
        let mut selected = BTreeMap::<(VId, VId), Vec<GraphValue>>::new();
        for row in rows {
            let (GraphValue::Vertex(start), GraphValue::Vertex(end), GraphValue::Path(path)) =
                (&row[0], &row[1], &row[2])
            else {
                unreachable!();
            };
            let key = (*start, *end);
            let replace = selected.get(&key).is_none_or(|previous| {
                let GraphValue::Path(previous) = &previous[2] else {
                    unreachable!();
                };
                (path.len(), path) < (previous.len(), previous)
            });
            if replace {
                selected.insert(key, row);
            }
        }
        rows = selected.into_values().collect();
    } else if search == GraphWalkSearch::AllShortest {
        // Select only after independently enumerating every finite route.
        // The fourth column is the literal hop count, not cursor state.
        let mut minimum = BTreeMap::new();
        for row in &rows {
            let key = (row[0].clone(), row[1].clone());
            let depth = minimum.entry(key).or_insert_with(|| row[3].clone());
            if row[3] < *depth {
                *depth = row[3].clone();
            }
        }
        rows.retain(|row| {
            row[3] == minimum[&(row[0].clone(), row[1].clone())]
        });
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
    // shortestPath now has a real AnyShortest owner. Keep its original
    // LIMIT 0 input, and execute the complete input before that outer page.
    let logical = prepared("MATCH p = shortestPath((a)-[:R*1..3]->(b)) RETURN p LIMIT 0");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert_eq!(plan.search, GraphWalkSearch::AnyShortest);
    assert_eq!(plan.maximum_hops(), 3);
    assert_eq!(tail.count(), Some(0));
    let source = Source::new(1);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(
        rows,
        vec![vec![GraphValue::Path(GraphPath::new(
            IDS[0],
            vec![(EId(0), IDS[1])].into(),
        ))]],
    );
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert_eq!(counts.roots.load(Ordering::SeqCst), IDS.len() + 1);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    for text in [
        "MATCH p = (a)-[:R*]->(b) RETURN p LIMIT 0",
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

#[test]
fn shortest_captures_match_independent_complete_routes_and_native_selection() {
    let mut cases = 0;
    for mask in 0..32 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (minimum, maximum) in [(0, 0), (0, 3), (1, 2), (2, 3), (3, 3)] {
                for different in [false, true] {
                    if different && minimum != 0 {
                        continue;
                    }
                    let source = Source::new(mask);
                    let text = statement(direction, minimum, maximum);
                    let logical = definition(&text, GraphWalkSearch::AnyShortest, different);
                    let expected = oracle(
                        &source.data,
                        direction,
                        minimum,
                        maximum,
                        GraphWalkSearch::AnyShortest,
                        different,
                    );
                    assert_eq!(
                        native_with_edge_properties(&logical, &source.data),
                        expected,
                        "native shortest {text}, mask={mask}, different={different}"
                    );
                    assert_eq!(
                        drain(logical, source),
                        expected,
                        "cold shortest {text}, mask={mask}, different={different}"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 672);
}

#[test]
fn unbounded_shortest_matches_finite_route_oracle_on_every_three_vertex_topology() {
    let mut cases = 0;
    for mask in 0..512 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for minimum in 0..=2 {
                for different in [false, true] {
                    if different && minimum != 0 {
                        continue;
                    }
                    let mut source = Source::new(0);
                    let data = Arc::get_mut(&mut source.data).unwrap();
                    for from in 0..3 {
                        for to in 0..3 {
                            let at = 3 * from + to;
                            if mask & (1 << at) != 0 {
                                let eid = if at == 8 {
                                    u128::MAX
                                } else {
                                    (at as u128) << 100
                                };
                                data.edges.push((EId(eid), IDS[from], IDS[to]));
                            }
                        }
                    }
                    // All edges are among three vertices; IDS[3] is isolated.
                    // An optimal WALK cannot repeat a vertex after its first
                    // minimum hops: removing that cycle leaves a shorter legal
                    // walk. Complete enumeration through minimum+2 therefore
                    // contains every optimum, independent of BFS settlement.
                    let maximum = minimum + 2;
                    let text = statement(direction, minimum, maximum).replacen(
                        &format!("..{maximum}]"),
                        "..]",
                        1,
                    );
                    let logical = definition(&text, GraphWalkSearch::AnyShortest, different);
                    let (physical, _) = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap();
                    assert!(!physical.bounds.is_bounded());
                    let expected = oracle(
                        &source.data,
                        direction,
                        minimum,
                        maximum,
                        GraphWalkSearch::AnyShortest,
                        different,
                    );
                    assert_eq!(
                        native_with_edge_properties(&logical, &source.data),
                        expected,
                        "native {text}, mask={mask}, different={different}"
                    );
                    let counts = source.counts.clone();
                    assert_eq!(
                        drain(logical, source),
                        expected,
                        "cold {text}, mask={mask}, different={different}"
                    );
                    assert_eq!(counts.traversal_reservations.load(Ordering::SeqCst), 0);
                    assert!(counts.state_peak.load(Ordering::SeqCst) < 1_000_000);
                    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 6144);
}

#[test]
fn unbounded_shortest_keeps_positive_lower_bounds_and_repetition_refusals() {
    // A three-hop lower bound is not a filter over the unrestricted optimum:
    // the shortest admitted root-to-root walk traverses the self-loop three
    // times. It is longer than this graph's simple-path diameter (zero).
    let text = "MATCH REPEATABLE ELEMENTS p = ANY SHORTEST (a {p:1})-[:R*3..]->(b) RETURN p";
    let logical = prepared(text);
    let source = Source::new(16);
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        vec![(EId(u128::MAX), IDS[0]); 3].into(),
    ))]];
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    assert_eq!(drain(logical.plan().clone(), source), expected);

    // An undirected shortest WALK can return on the same edge. A TRAIL cannot;
    // its valid root cycle may instead require a third vertex or a parallel
    // edge. Endpoint-only prefix dominance must not erase that distinction.
    for mask in [1, 3, 13] {
        let text =
            "MATCH REPEATABLE ELEMENTS p = ANY SHORTEST (a {p:1})-[:R*1..]-(b {p:1}) RETURN p";
        let logical = prepared(text);
        let source = Source::new(mask);
        let expected = vec![vec![GraphValue::Path(GraphPath::new(
            IDS[0],
            vec![(EId(0), IDS[1]), (EId(0), IDS[0])].into(),
        ))]];
        assert_eq!(
            native_with_edge_properties(logical.plan(), &source.data),
            expected
        );
        assert_eq!(drain(logical.plan().clone(), source), expected);
        let restricted = prepared(&text.replacen("REPEATABLE ELEMENTS ", "", 1));
        assert!(AsyncGraphWalkPlan::compile_sort_input(restricted.plan()).is_err());
    }
    for text in [
        "MATCH p = ANY SHORTEST (a)-[:R*2..]->(b) RETURN p LIMIT 0",
        "MATCH p = (a)-[:R*]->(b) RETURN p LIMIT 0",
    ] {
        let logical = prepared(text);
        assert!(
            AsyncGraphWalkPlan::compile_sort_input(logical.plan()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn unbounded_shortest_keeps_exact_limits_and_late_cancellation() {
    for minimum in [0, 2] {
        let text = format!(
            "MATCH REPEATABLE ELEMENTS p = ANY SHORTEST (a {{p:1}})-[:R*{minimum}..]->(b) RETURN p LIMIT 0"
        );
        let logical = prepared(&text);
        let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
        assert_eq!(tail.count(), Some(0));
        check_all_live_limits(plan, || Source::new(13));
    }
}

#[test]
fn unbounded_shortest_does_not_invent_the_finite_definition_hop_limit() {
    let hops = crate::MAX_GRAPH_WALK_HOPS as usize + 1;
    let mut source = Source::new(0);
    let data = Arc::get_mut(&mut source.data).unwrap();
    for at in 1..=hops {
        let vid = VId(at as u128);
        data.vertices.insert(
            vid,
            vec![(
                P,
                CanonicalScalar::Int(if at == hops { 99 } else { 10 }).into(),
            )],
        );
        data.edges.push((EId(at as u128), VId(at as u128 - 1), vid));
    }
    let logical = prepared("MATCH p = shortestPath((a {p:1})-[:R*]->(b {p:99})) RETURN p");
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        (1..=hops)
            .map(|at| (EId(at as u128), VId(at as u128)))
            .collect(),
    ))]];
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    assert_eq!(drain(logical.plan().clone(), source), expected);
    assert_eq!(counts.traversal_reservations.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
}

#[test]
fn shortest_canonical_ties_compare_the_complete_real_edge_prefix() {
    let mut source = Source::new(0);
    Arc::get_mut(&mut source.data).unwrap().edges = vec![
        (EId(0), IDS[2], IDS[3]),
        (EId(1), IDS[0], IDS[1]),
        (EId(2), IDS[0], IDS[2]),
        (EId(u128::MAX), IDS[1], IDS[3]),
    ];
    let logical = definition(
        "MATCH p = (a {p:1})-[:R*0..4]->(b {p:4}) RETURN p",
        GraphWalkSearch::AnyShortest,
        true,
    );
    // The route ending in the greatest EId wins because its FIRST EId is
    // smaller. Endpoint predicates cannot prune the intermediate vertices.
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        vec![(EId(1), IDS[1]), (EId(u128::MAX), IDS[3])].into(),
    ))]];
    assert_eq!(
        native_with_edge_properties(&logical, &source.data),
        expected
    );
    assert_eq!(drain(logical, source), expected);
}

#[test]
fn shortest_positive_lower_bound_dominates_parallel_walks_before_copying() {
    const HOPS: u32 = 64;
    const PARALLEL: usize = 8;
    let mut source = Source::new(0);
    Arc::get_mut(&mut source.data).unwrap().edges = (0..PARALLEL)
        .map(|at| (EId(at as u128), IDS[0], IDS[0]))
        .collect();
    let counts = source.counts.clone();
    let logical = definition(
        &format!("MATCH p = (a {{p:1}})-[:R*{HOPS}..{HOPS}]->(b) RETURN p"),
        GraphWalkSearch::AnyShortest,
        false,
    );
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        vec![(EId(0), IDS[0]); HOPS as usize].into(),
    ))]];
    assert_eq!(
        native_with_edge_properties(&logical, &source.data),
        expected
    );
    assert_eq!(drain(logical, source), expected);
    // There are 8^64 legal walks. The cold cursor makes exactly one indexed
    // candidate pass per depth, never materializing the exponentially tied bag.
    assert_eq!(
        counts.seeks.load(Ordering::SeqCst),
        (PARALLEL + 1) * HOPS as usize
    );
    assert!(counts.peak.load(Ordering::SeqCst) <= 4);
}

#[test]
fn shortest_scalar_edge_filters_precede_canonical_selection() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for (minimum, maximum) in [(0, 3), (2, 3)] {
            let source = scalar_edges(31);
            let text = statement(direction, minimum, maximum).replacen(']', " {p:7,q:false}]", 1);
            let logical = definition(&text, GraphWalkSearch::AnyShortest, false);
            let selected = Data {
                edges: vec![(EId(5), IDS[0], IDS[1])],
                edge_properties: BTreeMap::new(),
                vertices: source.data.vertices.clone(),
            };
            let expected = oracle(
                &selected,
                direction,
                minimum,
                maximum,
                GraphWalkSearch::AnyShortest,
                false,
            );
            assert_eq!(
                native_with_edge_properties(&logical, &source.data),
                expected
            );
            assert_eq!(drain(logical, source), expected);
        }
    }
}

#[test]
fn shortest_live_checkpoints_and_exact_cumulative_limits_are_terminal() {
    for (minimum, maximum) in [(0, 3), (2, 3)] {
        let logical = definition(
            &statement(GlaDirection::Undirected, minimum, maximum),
            GraphWalkSearch::AnyShortest,
            false,
        );
        let plan = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap().0;
        check_all_live_limits(plan, || Source::new(3));
    }
}

#[test]
fn shortest_errors_and_pending_drop_release_all_breadth_and_output_guards() {
    let logical = definition(
        &statement(GlaDirection::Forward, 0, 3),
        GraphWalkSearch::AnyShortest,
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
    for root_fault in [false, true] {
        for fault in [
            Fault::Io,
            Fault::Missing,
            Fault::Twice,
            Fault::Mismatch,
            Fault::FalseEof,
            Fault::Repeat,
            Fault::Pending,
            Fault::Dangling,
            Fault::Unavailable,
        ] {
            if root_fault && matches!(fault, Fault::Dangling | Fault::Unavailable) {
                continue;
            }
            let mut source = Source::new(31);
            if root_fault {
                source.root_fault = fault;
            } else {
                source.edge_fault = fault;
            }
            let counts = source.counts.clone();
            let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
            if fault == Fault::Pending {
                drop(cursor.next::<usize>());
                assert_eq!(cursor.state(), EdgeScanState::Open);
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
            } else {
                let mut failed = false;
                while let Some(row) = run(cursor.next()) {
                    if row.is_err() {
                        failed = true;
                        break;
                    }
                }
                assert!(failed);
            }
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(run(cursor.next()).is_none());
            assert_eq!(counts.records.load(Ordering::SeqCst), 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        }
    }
    for kind in 0..4 {
        let mut source = Source::new(31);
        match kind {
            0 => source.refuse_backing = true,
            1 => source.refuse_output = true,
            2 => source.refuse_evaluation = true,
            _ => source.scratch_limit = 0,
        }
        let counts = source.counts.clone();
        let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            if row.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn shortest_vector_capacity_stays_admitted_after_entries_leave_and_across_awaits() {
    const WIDTH: usize = 257;
    let source = || {
        let mut source = Source::new(0);
        let data = Arc::get_mut(&mut source.data).unwrap();
        for at in 0..WIDTH {
            let vid = VId(at as u128 + 1);
            data.vertices
                .insert(vid, vec![(P, CanonicalScalar::Int(10).into())]);
            data.edges.push((EId(at as u128), IDS[0], vid));
        }
        source
    };
    let plan = AsyncGraphWalkPlan::compile_sort_input(&definition(
        "MATCH (a {p:1})-[:R*0..1]->(b) RETURN ALL b",
        GraphWalkSearch::AnyShortest,
        false,
    ))
    .unwrap()
    .0;
    let vector_bytes = WIDTH * core::mem::size_of::<shortest::Prefix<Record, Guard>>();
    let input = source();
    let counts = input.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(input, plan.clone(), wide(), ok);
    drop(run(cursor.next()).unwrap().unwrap()); // zero-hop root
    for _ in 0..WIDTH - 1 {
        drop(run(cursor.next()).unwrap().unwrap());
    }
    assert_eq!(counts.records.load(Ordering::SeqCst), 2); // root + final pending prefix
    assert_eq!(
        counts.state_charges.lock().unwrap().get(&vector_bytes),
        Some(&1),
        "one remaining pending entry still owns the full original Vec allocation"
    );
    cursor.close();
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);

    let finish = |input: Source| {
        let counts = input.counts.clone();
        let mut cursor = AsyncGraphWalkCursor::new(input, plan.clone(), wide(), ok);
        let mut rows = 0;
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            match row {
                Ok(row) => {
                    drop(row);
                    rows += 1;
                }
                Err(error) => {
                    assert!(matches!(
                        error,
                        GqlQueryError::Source(EdgeScanError::Source(
                            "shortest state bytes refused"
                        ))
                    ));
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            assert_eq!(rows, WIDTH + 1);
        }
        assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
        assert!(counts.state_charges.lock().unwrap().is_empty());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        (counts.state_peak.load(Ordering::SeqCst), failed)
    };
    let (peak, failed) = finish(source());
    assert!(!failed);
    assert!(peak > vector_bytes);
    for (limit, expected_failure) in [(peak, false), (peak - 1, true)] {
        let mut input = source();
        input.state_limit = limit;
        assert_eq!(finish(input).1, expected_failure);
    }

    // Below the lower bound, consumed frontier entries are dropped rather
    // than becoming pending output. Pause on the last leaf's incidence: the
    // empty old IntoIter still has WIDTH slots and must retain its guard.
    let plan = AsyncGraphWalkPlan::compile_sort_input(&definition(
        "MATCH (a {p:1})-[:R*2..2]->(b) RETURN ALL b",
        GraphWalkSearch::AnyShortest,
        false,
    ))
    .unwrap()
    .0;
    let mut input = source();
    input.pending_after = 2 * WIDTH;
    let counts = input.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(input, plan, wide(), ok);
    let mut future = Box::pin(cursor.next::<usize>());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(counts.records.load(Ordering::SeqCst), 2);
    assert_eq!(
        counts.state_charges.lock().unwrap().get(&vector_bytes),
        Some(&1)
    );
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn all_shortest_captures_match_independent_complete_routes_and_native_selection() {
    let mut cases = 0;
    for mask in 0..32 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (minimum, maximum) in [(0, 0), (0, 3), (1, 2), (2, 3), (3, 3)] {
                for different in [false, true] {
                    if different && minimum != 0 {
                        continue;
                    }
                    let source = Source::new(mask);
                    let text = statement(direction, minimum, maximum);
                    let logical = definition(&text, GraphWalkSearch::AllShortest, different);
                    let expected = oracle(
                        &source.data,
                        direction,
                        minimum,
                        maximum,
                        GraphWalkSearch::AllShortest,
                        different,
                    );
                    assert_eq!(
                        native_with_edge_properties(&logical, &source.data),
                        expected,
                        "native all shortest {text}, mask={mask}, different={different}"
                    );
                    assert_eq!(
                        drain(logical, source),
                        expected,
                        "cold all shortest {text}, mask={mask}, different={different}"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 672);
}

fn shortest_diamond() -> Source {
    let mut source = Source::new(0);
    Arc::get_mut(&mut source.data).unwrap().edges = vec![
        (EId(0), IDS[2], IDS[3]),
        (EId(1), IDS[0], IDS[1]),
        (EId(2), IDS[0], IDS[2]),
        (EId(3), IDS[0], IDS[1]),
        (EId(4), IDS[1], IDS[2]),
        (EId(5), IDS[0], IDS[0]),
        (EId(u128::MAX), IDS[1], IDS[3]),
    ];
    source
}

#[test]
fn all_shortest_keeps_parallel_routes_in_complete_edge_prefix_order() {
    let logical =
        prepared("MATCH p = ALL SHORTEST (a {p:1})-[:R*0..4]->(b {p:4}) RETURN p");
    let expected: Vec<Vec<GraphValue>> = [
        vec![(EId(1), IDS[1]), (EId(u128::MAX), IDS[3])],
        vec![(EId(2), IDS[2]), (EId(0), IDS[3])],
        vec![(EId(3), IDS[1]), (EId(u128::MAX), IDS[3])],
    ]
    .into_iter()
    .map(|steps| vec![GraphValue::Path(GraphPath::new(IDS[0], steps.into()))])
    .collect();
    let source = shortest_diamond();
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    // This compares actual delivery order without sorting the cold results.
    // The greatest final EId belongs to both the first and last route.
    assert_eq!(rows, expected);
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
    assert!(run(cursor.next()).is_none());
}

#[test]
fn all_shortest_lower_bounds_retain_repeated_edges_and_zero_hop_isolates() {
    let logical = prepared(
        "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a {p:1})-[:R*3..3]->(b {p:1}) RETURN p",
    );
    let source = Source::new(16);
    let expected = vec![vec![GraphValue::Path(GraphPath::new(
        IDS[0],
        vec![(EId(u128::MAX), IDS[0]); 3].into(),
    ))]];
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    assert_eq!(drain(logical.plan().clone(), source), expected);

    let logical = prepared(
        "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a {p:1})-[:R*1..3]-(b {p:1}) RETURN p",
    );
    let expected: Vec<Vec<GraphValue>> = [
        [(EId(0), IDS[1]), (EId(0), IDS[0])],
        [(EId(0), IDS[1]), (EId(5), IDS[0])],
        [(EId(5), IDS[1]), (EId(0), IDS[0])],
        [(EId(5), IDS[1]), (EId(5), IDS[0])],
    ]
    .into_iter()
    .map(|steps| {
        vec![GraphValue::Path(GraphPath::new(
            IDS[0],
            steps.to_vec().into(),
        ))]
    })
    .collect();
    let source = Source::new(3);
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    assert_eq!(drain(logical.plan().clone(), source), expected);

    let restricted = definition(
        &statement(GlaDirection::Undirected, 1, 3),
        GraphWalkSearch::AllShortest,
        true,
    );
    assert!(
        restricted
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::DifferentEdges { .. }))
    );
    assert!(AsyncGraphWalkPlan::compile_sort_input(&restricted).is_err());

    let logical =
        prepared("MATCH p = ALL SHORTEST (a)-[:R*0..3]->(b) RETURN a, b, p");
    let rows = drain(logical.plan().clone(), Source::new(0));
    let expected: Vec<Vec<GraphValue>> = IDS
        .iter()
        .map(|&vid| {
            vec![
                GraphValue::Vertex(vid),
                GraphValue::Vertex(vid),
                GraphValue::Path(GraphPath::new(vid, Box::new([]))),
            ]
        })
        .collect();
    assert_eq!(rows, expected);
}

#[test]
fn all_shortest_input_completes_before_limit_distinct_and_hidden_ordering() {
    // This is the original refused bounded ALL SHORTEST definition. Its
    // LIMIT 0 remains a final window, not permission to skip source input.
    let logical =
        prepared("MATCH p = ALL SHORTEST (a)-[:R*0..3]->(b) RETURN p LIMIT 0");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert_eq!(plan.search, GraphWalkSearch::AllShortest);
    assert_eq!(tail.count(), Some(0));
    let source = Source::new(3);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let mut rows = 0;
    while let Some(row) = run(cursor.next()) {
        row.unwrap();
        rows += 1;
    }
    assert_eq!(rows, 6); // four zero-hop routes and two parallel one-hop routes
    assert_eq!(counts.roots.load(Ordering::SeqCst), IDS.len() + 1);

    let logical =
        prepared("MATCH ALL SHORTEST (a)-[:R*0..2]->(b) RETURN DISTINCT b LIMIT 1");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert!(tail.distinct());
    assert_eq!(tail.count(), Some(1));
    let mut cursor = AsyncGraphWalkCursor::new(Source::new(3), plan, wide(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(rows.len(), 6);
    assert_eq!(
        rows.iter()
            .filter(|row| row[0] == GraphValue::Vertex(IDS[1]))
            .count(),
        3
    );

    let logical = prepared(
        "MATCH ALL SHORTEST (a)-[:R*0..2]->(b) RETURN a.p AS value ORDER BY b.p DESC LIMIT 0",
    );
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert_eq!(tail.count(), Some(0));
    assert_eq!(tail.visible_width(), 1);
    assert_eq!(tail.evaluation_width(), 2);
    let mut cursor = AsyncGraphWalkCursor::new(Source::new(3), plan, wide(), ok);
    let mut rows = 0;
    while let Some(row) = run(cursor.next()) {
        assert_eq!(row.unwrap().values().len(), 2);
        rows += 1;
    }
    assert_eq!(rows, 6);
}

#[test]
fn all_shortest_scalar_edge_filters_precede_path_selection() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for (minimum, maximum) in [(0, 3), (2, 3)] {
            let source = scalar_edges(31);
            let text = statement(direction, minimum, maximum).replacen(']', " {p:7}]", 1);
            let logical = definition(&text, GraphWalkSearch::AllShortest, false);
            let selected = Data {
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
            let expected = oracle(
                &selected,
                direction,
                minimum,
                maximum,
                GraphWalkSearch::AllShortest,
                false,
            );
            assert_eq!(
                native_with_edge_properties(&logical, &source.data),
                expected
            );
            assert_eq!(drain(logical, source), expected);
        }
    }
    // A masked first candidate cannot erase the visible parallel route.
    let logical =
        prepared("MATCH p = ALL SHORTEST (a {p:1})-[:R*0..2]->(b {p:2}) RETURN p");
    let mut source = Source::new(3);
    source.hidden = Some(EId(0));
    assert_eq!(
        drain(logical.plan().clone(), source),
        vec![vec![GraphValue::Path(GraphPath::new(
            IDS[0],
            vec![(EId(5), IDS[1])].into(),
        ))]]
    );
}

#[test]
fn all_shortest_live_checkpoints_and_exact_cumulative_limits_are_terminal() {
    for (minimum, maximum) in [(0, 3), (2, 3)] {
        let logical = definition(
            &statement(GlaDirection::Undirected, minimum, maximum),
            GraphWalkSearch::AllShortest,
            false,
        );
        let plan = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap().0;
        check_all_live_limits(plan, || Source::new(3));
    }
}

#[test]
fn all_shortest_failures_release_shared_layers_and_independent_outputs() {
    let logical = definition(
        &statement(GlaDirection::Forward, 0, 3),
        GraphWalkSearch::AllShortest,
        false,
    );
    let plan = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap().0;
    let source = Source::new(31);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    cursor.close();
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    drop(row);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert!(run(cursor.next()).is_none());

    for root_fault in [false, true] {
        for fault in [
            Fault::Io,
            Fault::Missing,
            Fault::Twice,
            Fault::Mismatch,
            Fault::FalseEof,
            Fault::Repeat,
            Fault::Pending,
            Fault::Dangling,
            Fault::Unavailable,
        ] {
            if root_fault && matches!(fault, Fault::Dangling | Fault::Unavailable) {
                continue;
            }
            let mut source = Source::new(31);
            if root_fault {
                source.root_fault = fault;
            } else {
                source.edge_fault = fault;
            }
            let counts = source.counts.clone();
            let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
            if fault == Fault::Pending {
                drop(cursor.next::<usize>());
                assert_eq!(cursor.state(), EdgeScanState::Open);
                loop {
                    let mut future = Box::pin(cursor.next::<usize>());
                    fn assert_send<T: Send>(_: &T) {}
                    assert_send(&future);
                    match future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                    {
                        Poll::Pending => {
                            drop(future);
                            break;
                        }
                        Poll::Ready(output) => {
                            // A zero-hop result may precede the first edge
                            // read. Consume it before suspending on that read.
                            drop(future);
                            assert!(output.as_ref().is_some_and(|row| row.is_ok()));
                            drop(output);
                        }
                    }
                }
            } else {
                let mut failed = false;
                while let Some(row) = run(cursor.next()) {
                    if row.is_err() {
                        failed = true;
                        break;
                    }
                }
                assert!(failed);
            }
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(run(cursor.next()).is_none());
            assert_eq!(counts.records.load(Ordering::SeqCst), 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
            assert!(counts.state_charges.lock().unwrap().is_empty());
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        }
    }
    for kind in 0..5 {
        let mut source = Source::new(31);
        match kind {
            0 => source.refuse_backing = true,
            1 => source.refuse_output = true,
            2 => source.refuse_evaluation = true,
            3 => source.scratch_limit = 0,
            _ => source.state_limit = 0,
        }
        let counts = source.counts.clone();
        let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            if row.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
        assert!(counts.state_charges.lock().unwrap().is_empty());
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn all_shortest_shared_state_admits_the_exact_peak_and_refuses_one_byte_less() {
    let logical =
        prepared("MATCH p = ALL SHORTEST (a {p:1})-[:R*0..4]->(b {p:4}) RETURN p");
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let expected = native_with_edge_properties(logical.plan(), &shortest_diamond().data);
    assert_eq!(expected.len(), 3);
    let finish = |limit| {
        let mut source = shortest_diamond();
        source.state_limit = limit;
        let counts = source.counts.clone();
        let mut cursor = AsyncGraphWalkCursor::new(source, plan.clone(), wide(), ok);
        let mut rows = Vec::new();
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            match row {
                Ok(row) => rows.push(row.values().to_vec()),
                Err(error) => {
                    assert!(matches!(
                        error,
                        GqlQueryError::Source(EdgeScanError::Source(
                            "shortest state bytes refused"
                        ))
                    ));
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            assert_eq!(rows, expected);
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        } else {
            assert_eq!(cursor.state(), EdgeScanState::Failed);
        }
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
        assert!(counts.state_charges.lock().unwrap().is_empty());
        (counts.state_peak.load(Ordering::SeqCst), failed)
    };
    let (peak, failed) = finish(usize::MAX);
    assert!(!failed);
    assert!(peak > 0);
    assert_eq!(finish(peak), (peak, false));
    assert!(finish(peak - 1).1);
}

#[test]
fn all_shortest_exponential_ties_are_pulled_lazily_from_bounded_shared_layers() {
    const HOPS: usize = 64;
    const PARALLEL: usize = 8;
    const STATE_LIMIT: usize = 4 * 1024 * 1024;
    let source = || {
        let mut source = Source::new(0);
        Arc::get_mut(&mut source.data).unwrap().edges = (0..PARALLEL)
            .map(|at| (EId(at as u128), IDS[0], IDS[0]))
            .collect();
        source.state_limit = STATE_LIMIT;
        source
    };
    let logical = prepared(&format!(
        "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a {{p:1}})-[:R*{HOPS}..{HOPS}]->(b) RETURN p"
    ));
    let plan = AsyncGraphWalkPlan::compile_sort_input(logical.plan())
        .unwrap()
        .0;
    let input = source();
    let counts = input.counts.clone();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel = cancelled.clone();
    let mut cursor = AsyncGraphWalkCursor::new(input, plan.clone(), wide(), move || {
        if cancel.load(Ordering::SeqCst) {
            Err(7_usize)
        } else {
            Ok(())
        }
    });
    // There are 8^64 equal shortest routes. Only requested output paths may
    // be expanded from the shared graph; a complete prefix bag cannot fit.
    for last in 0..4 {
        let mut steps = vec![(EId(0), IDS[0]); HOPS];
        steps[HOPS - 1].0 = EId(last);
        let row = run(cursor.next()).unwrap().unwrap();
        assert_eq!(
            row.values(),
            &[GraphValue::Path(GraphPath::new(IDS[0], steps.into()))]
        );
    }
    assert_eq!(cursor.row_stats().result_rows, 4);
    assert_eq!(
        counts.seeks.load(Ordering::SeqCst),
        (PARALLEL + 1) * HOPS
    );
    assert_eq!(counts.traversal_reservations.load(Ordering::SeqCst), 0);
    assert!(counts.state_bytes.load(Ordering::SeqCst) > 0);
    assert!(counts.state_peak.load(Ordering::SeqCst) <= STATE_LIMIT);
    assert!(counts.peak.load(Ordering::SeqCst) <= PARALLEL * HOPS + 2);

    cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(
        run(cursor.next()).unwrap(),
        Err(GqlQueryError::Interrupted(7))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);

    // Dropping a suspended pull after real layer admission must also release
    // the full retained DAG, not only its currently visited incidence.
    let mut input = source();
    input.pending_after = (PARALLEL + 1) * (HOPS / 2);
    let counts = input.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(input, plan, wide(), ok);
    drop(cursor.next::<usize>());
    assert_eq!(cursor.state(), EdgeScanState::Open);
    let mut future = Box::pin(cursor.next::<usize>());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(counts.state_bytes.load(Ordering::SeqCst) > 0);
    assert!(counts.records.load(Ordering::SeqCst) > 0);
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn all_shortest_releases_record_temporaries_between_tied_output_paths() {
    const TIES: usize = 32;
    let mut source = Source::new(0);
    let data = Arc::get_mut(&mut source.data).unwrap();
    data.edges = (0..TIES)
        .map(|at| (EId(at as u128), IDS[0], IDS[1]))
        .chain(std::iter::once((EId(u128::MAX), IDS[1], IDS[2])))
        .collect();
    // Every two-hop route shares the same final record. This allowance fits
    // one complete capture and projection but not all 32 accumulated copies.
    source.scratch_limit = 64;
    let logical = prepared(
        "MATCH p = ALL SHORTEST (a {p:1})-[:R*0..2]->(b {p:3}) RETURN p",
    );
    let expected: Vec<Vec<GraphValue>> = (0..TIES)
        .map(|at| {
            vec![GraphValue::Path(GraphPath::new(
                IDS[0],
                vec![(EId(at as u128), IDS[1]), (EId(u128::MAX), IDS[2])].into(),
            ))]
        })
        .collect();
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    assert_eq!(drain(logical.plan().clone(), source), expected);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
}

#[test]
fn unbounded_all_shortest_matches_finite_routes_on_every_three_vertex_topology() {
    let mut cases = 0;
    for mask in 0..512 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for minimum in 0..=2 {
                for different in [false, true] {
                    if different && minimum != 0 {
                        continue;
                    }
                    let mut source = Source::new(0);
                    let data = Arc::get_mut(&mut source.data).unwrap();
                    for from in 0..3 {
                        for to in 0..3 {
                            let at = 3 * from + to;
                            if mask & (1 << at) != 0 {
                                let eid = if at == 8 {
                                    u128::MAX
                                } else {
                                    (at as u128) << 100
                                };
                                data.edges.push((EId(eid), IDS[from], IDS[to]));
                            }
                        }
                    }
                    // Any repeated vertex after the first minimum hops would
                    // give a removable cycle and a shorter admissible walk.
                    // On these three incident vertices, minimum+2 therefore
                    // contains every optimum, including every tied route.
                    // IDS[3] is isolated and remains a zero-hop answer.
                    let maximum = minimum + 2;
                    let text = statement(direction, minimum, maximum).replacen(
                        &format!("..{maximum}]"),
                        "..]",
                        1,
                    );
                    let logical = definition(&text, GraphWalkSearch::AllShortest, different);
                    let (physical, _) = AsyncGraphWalkPlan::compile_sort_input(&logical).unwrap();
                    assert!(!physical.bounds.is_bounded());
                    let expected = oracle(
                        &source.data,
                        direction,
                        minimum,
                        maximum,
                        GraphWalkSearch::AllShortest,
                        different,
                    );
                    assert_eq!(
                        native_with_edge_properties(&logical, &source.data),
                        expected,
                        "native all shortest {text}, mask={mask}, different={different}"
                    );
                    let counts = source.counts.clone();
                    assert_eq!(
                        drain(logical, source),
                        expected,
                        "cold all shortest {text}, mask={mask}, different={different}"
                    );
                    assert_eq!(counts.traversal_reservations.load(Ordering::SeqCst), 0);
                    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
                    assert!(counts.state_charges.lock().unwrap().is_empty());
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 6144);
}

#[test]
fn unbounded_all_shortest_keeps_zero_pages_parallel_ties_and_lower_bound_cycles() {
    // Preserve the exact former unsupported definition as a complete-input
    // positive. Neither its omitted upper bound nor LIMIT 0 skips intake.
    let logical =
        prepared("MATCH p = ALL SHORTEST (a)-[:R*0..]->(b) RETURN p LIMIT 0");
    let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
    assert!(!plan.bounds.is_bounded());
    assert_eq!(tail.count(), Some(0));
    let source = Source::new(3);
    let counts = source.counts.clone();
    let mut cursor = AsyncGraphWalkCursor::new(source, plan, wide(), ok);
    let mut rows = 0;
    while let Some(row) = run(cursor.next()) {
        row.unwrap();
        rows += 1;
    }
    assert_eq!(rows, 6);
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert_eq!(counts.roots.load(Ordering::SeqCst), IDS.len() + 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);

    let source = shortest_diamond();
    let bounded =
        prepared("MATCH p = ALL SHORTEST (a {p:1})-[:R*0..4]->(b {p:4}) RETURN p");
    let expected = native_with_edge_properties(bounded.plan(), &source.data);
    assert_eq!(expected.len(), 3);
    let logical =
        prepared("MATCH p = allShortestPaths((a {p:1})-[:R*0..]->(b {p:4})) RETURN p");
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    assert_eq!(drain(logical.plan().clone(), source), expected);

    let text =
        "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a {p:1})-[:R*3..]->(b) RETURN p";
    let logical = prepared(text);
    let mut source = Source::new(0);
    Arc::get_mut(&mut source.data).unwrap().edges = vec![
        (EId(0), IDS[0], IDS[0]),
        (EId(u128::MAX), IDS[0], IDS[0]),
    ];
    let mut expected = Vec::new();
    for first in [EId(0), EId(u128::MAX)] {
        for second in [EId(0), EId(u128::MAX)] {
            for third in [EId(0), EId(u128::MAX)] {
                expected.push(vec![GraphValue::Path(GraphPath::new(
                    IDS[0],
                    vec![(first, IDS[0]), (second, IDS[0]), (third, IDS[0])].into(),
                ))]);
            }
        }
    }
    assert_eq!(expected.len(), 8);
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    assert_eq!(drain(logical.plan().clone(), source), expected);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);

    let restricted = prepared(&text.replacen("REPEATABLE ELEMENTS ", "", 1));
    assert!(AsyncGraphWalkPlan::compile_sort_input(restricted.plan()).is_err());
    let restricted = definition(
        "MATCH p = (a)-[:R*1..]-(b) RETURN p LIMIT 0",
        GraphWalkSearch::AllShortest,
        true,
    );
    assert!(AsyncGraphWalkPlan::compile_sort_input(&restricted).is_err());
}

#[test]
fn unbounded_all_shortest_keeps_exact_limits_and_late_cancellation() {
    for minimum in [0, 2] {
        let text = format!(
            "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a {{p:1}})-[:R*{minimum}..]-(b) RETURN p LIMIT 0"
        );
        let logical = prepared(&text);
        let (plan, tail) = AsyncGraphWalkPlan::compile_sort_input(logical.plan()).unwrap();
        assert_eq!(tail.count(), Some(0));
        assert!(!plan.bounds.is_bounded());
        check_all_live_limits(plan, || Source::new(3));
    }
}

#[test]
fn unbounded_all_shortest_keeps_tied_routes_beyond_the_finite_definition_hop_limit() {
    let hops = crate::MAX_GRAPH_WALK_HOPS as usize + 1;
    let mut source = Source::new(0);
    let data = Arc::get_mut(&mut source.data).unwrap();
    // Two physical first edges lead into one long shared suffix.
    data.edges.push((EId(0), IDS[0], VId(1)));
    for at in 1..=hops {
        let vid = VId(at as u128);
        data.vertices.insert(
            vid,
            vec![(
                P,
                CanonicalScalar::Int(if at == hops { 99 } else { 10 }).into(),
            )],
        );
        data.edges.push((EId(at as u128), VId(at as u128 - 1), vid));
    }
    let logical =
        prepared("MATCH p = allShortestPaths((a {p:1})-[:R*]->(b {p:99})) RETURN p");
    let expected: Vec<Vec<GraphValue>> = [EId(0), EId(1)]
        .into_iter()
        .map(|first| {
            let steps = std::iter::once((first, VId(1)))
                .chain((2..=hops).map(|at| (EId(at as u128), VId(at as u128))))
                .collect();
            vec![GraphValue::Path(GraphPath::new(IDS[0], steps))]
        })
        .collect();
    assert_eq!(
        native_with_edge_properties(logical.plan(), &source.data),
        expected
    );
    let counts = source.counts.clone();
    assert_eq!(drain(logical.plan().clone(), source), expected);
    assert_eq!(counts.traversal_reservations.load(Ordering::SeqCst), 0);
    assert!(counts.peak.load(Ordering::SeqCst) <= hops + 3);
    assert_eq!(counts.state_bytes.load(Ordering::SeqCst), 0);
    assert!(counts.state_charges.lock().unwrap().is_empty());
}
