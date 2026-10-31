use super::*;
use crate::algebra::{GlaPlan, GraphValue, GraphWalkSearch, PreparedGraphPattern};
use crate::edge_stream::{AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, EdgeScanState};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(3);
const IDS: [VId; 3] = [VId(0), VId(1 << 100), VId(u128::MAX)];
const EDGES: [(u128, usize, usize, RelationId, i64); 6] = [
    (0, 0, 1, R, 5),
    (1, 0, 1, R, 6),
    (2, 1, 2, R, 7),
    (3, 1, 0, S, -1),
    (4, 1, 0, S, 9),
    (u128::MAX, 0, 0, S, 10),
];
fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..1024 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("fixture did not complete");
}
fn definition(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn plan(text: &str) -> AsyncEdgeJoinPlan {
    let definition = definition(text);
    let frozen = definition.canonical_bytes();
    let compiled = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    assert_eq!(frozen, definition.canonical_bytes());
    compiled
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn ok() -> Result<(), usize> {
    Ok(())
}

#[derive(Default)]
struct Counts {
    records: AtomicUsize,
    peak: AtomicUsize,
    guards: AtomicUsize,
    seeks: AtomicUsize,
    roots: AtomicUsize,
    drops: AtomicUsize,
}
struct Image {
    id: EId,
    from: VId,
    to: VId,
    relation: RelationId,
    props: Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)>,
}
struct Data {
    edges: Vec<Image>,
    vertices: BTreeMap<VId, Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)>>,
}
struct Record {
    data: Arc<Data>,
    index: usize,
    counts: Arc<Counts>,
    // Real cold records likewise are Send but not Sync. A Send future must
    // not retain a shared loan of such a record across a nested await.
    scratch: Cell<usize>,
}
impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        let image = &self.data.edges[self.index];
        EdgeScanRow {
            source: image.from,
            target: image.to,
            relation: image.relation,
            properties: &image.props,
        }
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        let image = &self.data.edges[self.index];
        if vid != image.from && vid != image.to {
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
    Unavailable,
    Io,
    Missing,
    Twice,
    Mismatch,
    FalseEof,
    Repeat,
    Pending,
}
struct Source {
    data: Arc<Data>,
    next_root: usize,
    counts: Arc<Counts>,
    hidden: Option<EId>,
    fault: Fault,
}
impl Source {
    fn new(mask: usize) -> Self {
        let vertices = BTreeMap::from([
            (IDS[0], vec![(P, CanonicalScalar::Int(1).into())]),
            (IDS[1], vec![(P, CanonicalScalar::Int(2).into())]),
            (IDS[2], Vec::new()),
        ]);
        let edges = EDGES
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, &(id, from, to, relation, p))| Image {
                id: EId(id),
                from: IDS[from],
                to: IDS[to],
                relation,
                props: vec![(P, CanonicalScalar::Int(p).into())],
            })
            .collect();
        Self {
            data: Arc::new(Data { edges, vertices }),
            next_root: 0,
            counts: Arc::default(),
            hidden: None,
            fault: Fault::None,
        }
    }
    fn guard<C>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.counts.guards.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(self.counts.clone()))
    }
    fn emit<C>(
        &self,
        index: Option<usize>,
        fault: Fault,
        control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<&'static str, C>> {
        let Some(index) = index else {
            return Ok(None);
        };
        let id = self.data.edges[index].id;
        if fault != Fault::Missing {
            control(AsyncEdgeScanEvent::Candidate(id)).map_err(EdgeScanSourceError::Control)?;
        }
        if fault == Fault::Twice {
            control(AsyncEdgeScanEvent::Candidate(id)).map_err(EdgeScanSourceError::Control)?;
        }
        if fault == Fault::FalseEof {
            return Ok(None);
        }
        control(AsyncEdgeScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        let record = if self.hidden == Some(id) {
            None
        } else {
            let live = self.counts.records.fetch_add(1, Ordering::SeqCst) + 1;
            self.counts.peak.fetch_max(live, Ordering::SeqCst);
            Some(Record {
                data: self.data.clone(),
                index,
                counts: self.counts.clone(),
                scratch: Cell::new(0),
            })
        };
        let eid = if fault == Fault::Mismatch {
            EId(id.0 ^ 1)
        } else {
            id
        };
        Ok(Some(AsyncEdgeCandidate { eid, record }))
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
        CommitSeq(19)
    }
    async fn next_candidate<C: Send>(
        &mut self,
        _: crate::algebra::EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<Self::Error, C>> {
        self.counts.roots.fetch_add(1, Ordering::SeqCst);
        control(AsyncEdgeScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let index = (self.next_root < self.data.edges.len()).then_some(self.next_root);
        self.next_root += usize::from(index.is_some());
        self.emit(index, Fault::None, control)
    }
    fn evaluation_event(
        &self,
        record: &Record,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            record.scratch.set(record.scratch.get() + 1);
        }
        Ok(())
    }
    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        self.guard(control)
    }
}
impl AsyncEdgeJoinSource for Source {
    type TraversalGuard = Guard;
    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        _: crate::algebra::EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> AsyncIncidentCandidateResult<Record, Self::Error, C> {
        self.counts.seeks.fetch_add(1, Ordering::SeqCst);
        if self.fault == Fault::Unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(AsyncEdgeScanEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
        if self.fault == Fault::Pending {
            std::future::pending::<()>().await;
        }
        if self.fault == Fault::Io {
            return Err(EdgeExpansionSourceError::Read(EdgeScanSourceError::Source(
                "probe I/O",
            )));
        }
        let mut index = None;
        for (at, edge) in self.data.edges.iter().enumerate() {
            control(AsyncEdgeScanEvent::Work)
                .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
            let ordered = if self.fault == Fault::Repeat && after.is_some() {
                Some(edge.id) == after
            } else {
                after.is_none_or(|id| edge.id > id)
            };
            let incident = match direction {
                GlaDirection::Forward => edge.from == endpoint,
                GlaDirection::Reverse => edge.to == endpoint,
                GlaDirection::Undirected => edge.from == endpoint || edge.to == endpoint,
            };
            if ordered && incident {
                index = Some(at);
                break;
            }
        }
        self.emit(index, self.fault, control)
            .map_err(EdgeExpansionSourceError::Read)
    }
    fn reserve_traversal<C>(
        &self,
        slots: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        assert!(slots > 0 && slots <= MAX_PATTERN_BINDINGS);
        self.guard(control)
    }
    fn reserve_join_output<C>(
        &self,
        _: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        self.guard(control)
    }
}
fn text(direction: usize, two: bool, anti: bool, filtered: bool) -> String {
    let atom = |name: &str, end: &str| match direction {
        0 => format!("-[{name}:S]->({end})"),
        1 => format!("<-[{name}:S]-({end})"),
        _ => format!("-[{name}:S]-({end})"),
    };
    format!(
        "MATCH (a)-[r:R]->(b) WHERE {}EXISTS {{ MATCH (b){}{} {} }} RETURN r,a,b",
        if anti { "NOT " } else { "" },
        atom("s", "c"),
        if two { atom("t", "d") } else { String::new() },
        if filtered {
            "WHERE s.p >= 0 AND c.p = a.p"
        } else {
            ""
        }
    )
}
// Complete finite relation products, with no cursor, source positions or GLA.
fn oracle(
    mask: usize,
    direction: usize,
    two: bool,
    anti: bool,
    filtered: bool,
    hidden: Option<EId>,
) -> Vec<EId> {
    let orient = |from, to| match direction {
        0 => vec![(from, to)],
        1 => vec![(to, from)],
        _ if from == to => vec![(from, to)],
        _ => vec![(from, to), (to, from)],
    };
    let edges: Vec<_> = EDGES
        .iter()
        .enumerate()
        .filter(|(at, edge)| mask & (1 << at) != 0 && hidden != Some(EId(edge.0)))
        .map(|(_, edge)| *edge)
        .collect();
    edges
        .iter()
        .filter(|edge| edge.3 == R)
        .filter_map(|&(id, a, b, _, _)| {
            let witness = edges
                .iter()
                .filter(|edge| edge.3 == S)
                .any(|&(s, from, to, _, p)| {
                    orient(from, to).into_iter().any(|(start, c)| {
                        start == b
                            && (!filtered || (p >= 0 && c != 2 && a != 2 && c == a))
                            && (!two
                                || edges.iter().filter(|edge| edge.3 == S && edge.0 != s).any(
                                    |&(_, from, to, _, _)| {
                                        orient(from, to).iter().any(|&(from, _)| from == c)
                                    },
                                ))
                    })
                });
            (witness != anti).then_some(EId(id))
        })
        .collect()
}
fn drain<F: FnMut() -> Result<(), usize> + Send>(
    cursor: &mut AsyncEdgeJoinCursor<Source, F>,
) -> ScanResult<Vec<EId>, &'static str, usize> {
    run(async {
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().await {
            let row = row?;
            let GraphValue::Edge(id) = &row.values()[0] else {
                panic!("edge column");
            };
            rows.push(*id);
        }
        Ok(rows)
    })
}
#[test]
fn correlated_probes_match_complete_products_without_exporting_or_multiplying_witnesses() {
    let mut cases = 0;
    for mask in 0..64 {
        for direction in 0..3 {
            for two in [false, true] {
                for anti in [false, true] {
                    for filtered in [false, true] {
                        let source = Source::new(mask);
                        let counts = source.counts.clone();
                        let query = text(direction, two, anti, filtered);
                        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(&query), wide(), ok);
                        assert_eq!(
                            drain(&mut cursor).unwrap(),
                            oracle(mask, direction, two, anti, filtered, None),
                            "{query}: {mask}"
                        );
                        assert!(counts.peak.load(Ordering::SeqCst) <= if two { 3 } else { 2 });
                        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1536);
}
#[test]
fn invisible_candidates_copy_only_probes_and_parallel_witnesses_preserve_existence() {
    for hidden in [EId(3), EId(4), EId(u128::MAX)] {
        for anti in [false, true] {
            let mut source = Source::new(63);
            source.hidden = Some(hidden);
            let mut cursor =
                AsyncEdgeJoinCursor::new(source, plan(&text(0, true, anti, false)), wide(), ok);
            assert_eq!(
                drain(&mut cursor).unwrap(),
                oracle(63, 0, true, anti, false, Some(hidden))
            );
        }
    }
    for (body, expected) in [
        ("EXISTS { MATCH (b) WHERE b.p IS NULL }", vec![EId(2)]),
        (
            "NOT EXISTS { MATCH (b) WHERE b.p IS NULL }",
            vec![EId(0), EId(1)],
        ),
        (
            "EXISTS { MATCH (b)-[s:S]->(c) WHERE s.p >= 0 } AND NOT EXISTS { MATCH (b)-[:S]->(d) WHERE d.p IS NULL }",
            vec![EId(0), EId(1)],
        ),
    ] {
        let query = format!("MATCH (a)-[r:R]->(b) WHERE {body} RETURN r,a,b");
        let mut cursor = AsyncEdgeJoinCursor::new(Source::new(63), plan(&query), wide(), ok);
        assert_eq!(drain(&mut cursor).unwrap(), expected, "{query}");
    }
}
#[test]
fn async_probe_errors_and_bad_candidate_admission_never_prove_absence() {
    let query =
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[s:S]->(c) WHERE s.p=999 } RETURN r,a,b";
    for fault in [
        Fault::Unavailable,
        Fault::Io,
        Fault::Missing,
        Fault::Twice,
        Fault::Mismatch,
        Fault::FalseEof,
        Fault::Repeat,
    ] {
        let mut source = Source::new(63);
        source.fault = fault;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(query), wide(), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn first_witness_short_circuits_and_pending_probe_drop_releases_every_guard() {
    let query = format!("{} LIMIT 1", text(0, false, false, false));
    let source = Source::new(63);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(&query), wide(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    // Index admits one nonmatching R edge, then the first S witness. The other
    // parallel S edge and all later root bindings are not requested.
    assert_eq!(counts.roots.load(Ordering::SeqCst), 1);
    assert_eq!(counts.seeks.load(Ordering::SeqCst), 2);
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    drop(cursor);
    assert_eq!(row.values()[0], GraphValue::Edge(EId(0)));
    drop(row);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);

    let mut source = Source::new(63);
    source.fault = Fault::Pending;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(&query), wide(), ok);
    drop(cursor.next());
    assert_eq!(counts.roots.load(Ordering::SeqCst), 0);
    let mut future = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}
#[test]
fn probes_share_all_four_limits_and_every_checkpoint_with_the_outer_query() {
    let query = text(0, true, true, true);
    let mut calls = 0;
    let mut cursor = AsyncEdgeJoinCursor::new(Source::new(63), plan(&query), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = drain(&mut cursor).unwrap();
    let row_stats = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    drop(cursor);
    assert!(!expected.is_empty());
    let limits = [
        row_stats.snapshot_records,
        row_stats.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    for boundary in 0..calls {
        let source = Source::new(63);
        let counts = source.counts.clone();
        let mut seen = 0;
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(&query), wide(), || {
            let at = seen;
            seen += 1;
            if at == boundary {
                Err(boundary)
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(drain(&mut cursor), Err(GqlQueryError::Interrupted(at)) if at == boundary)
        );
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    for dimension in 0..4 {
        for exact in [true, false] {
            let mut cap = limits;
            if !exact {
                cap[dimension] -= 1;
            }
            let mut cursor = AsyncEdgeJoinCursor::new(
                Source::new(63),
                plan(&query),
                GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]),
                ok,
            );
            let result = drain(&mut cursor);
            if exact {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert!(result.is_err());
            }
        }
    }
}
#[test]
fn all_async_consumers_admit_correlated_path_support_but_keep_other_source_refusals() {
    let accepted = definition(&text(0, true, false, true));
    assert!(AsyncEdgeJoinPlan::compile(accepted.plan()).is_ok());
    assert!(AsyncEdgeJoinPlan::compile_aggregate(accepted.plan()).is_ok());
    assert!(AsyncEdgeJoinPlan::compile_sort_input(accepted.plan()).is_ok());
    let accepted = definition(
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH WALK (b)-[:S*1..2]->(c) } RETURN r,a,b",
    );
    assert!(AsyncEdgeJoinPlan::compile(accepted.plan()).is_ok());
    assert!(AsyncEdgeJoinPlan::compile_aggregate(accepted.plan()).is_ok());
    assert!(AsyncEdgeJoinPlan::compile_sort_input(accepted.plan()).is_ok());
    for body in [
        "MATCH (c)",
        "MATCH (c)-[:S*1..2]->(d)",
        "MATCH p = (b)-[:S*1..2]->(c) WHERE length(p)=2",
    ] {
        let query = definition(&format!(
            "MATCH (a)-[r:R]->(b) WHERE EXISTS {{ {body} }} RETURN r,a,b"
        ));
        assert!(AsyncEdgeJoinPlan::compile(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_aggregate(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_sort_input(query.plan()).is_err());
    }
    for text in [
        "MATCH (a)-[r:R]->(b)-[:S*1..2]->(c) RETURN r,a,b,c",
        "MATCH p = (a)-[:R*1..2]->(b) RETURN p",
    ] {
        let query = definition(text);
        assert!(AsyncEdgeJoinPlan::compile(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_aggregate(query.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_sort_input(query.plan()).is_err());
    }
}

fn walk_text(direction: usize, lo: u32, hi: u32, anti: bool) -> String {
    let atom = match direction {
        0 => format!("-[:S*{lo}..{hi}]->"),
        1 => format!("<-[:S*{lo}..{hi}]-"),
        _ => format!("-[:S*{lo}..{hi}]-"),
    };
    format!(
        "MATCH (a)-[r:R]->(b) WHERE {}EXISTS {{ MATCH REPEATABLE ELEMENTS (b){atom}(c) WHERE c.p = a.p }} RETURN r,a,b",
        if anti { "NOT " } else { "" },
    )
}

fn walk_plan(query: &str, mode: GraphWalkSearch) -> AsyncEdgeJoinPlan {
    let definition = definition(query);
    let mut operators = definition.plan().operators().to_vec();
    for operator in &mut operators {
        if let GlaOperator::VarLengthExpand { search, .. } = operator {
            *search = mode;
        }
    }
    AsyncEdgeJoinPlan::compile(&GlaPlan::from_operators(operators)).unwrap()
}

// Complete bounded routes, followed by whole-route legality checks. This does
// not use endpoint support, depth dominance, cursor positions or the evaluator.
fn walk_oracle(
    mask: usize,
    direction: usize,
    lo: u32,
    hi: u32,
    mode: GraphWalkSearch,
    anti: bool,
    hidden: Option<EId>,
) -> Vec<EId> {
    let edges: Vec<_> = EDGES
        .iter()
        .enumerate()
        .filter(|(at, edge)| mask & (1 << at) != 0 && hidden != Some(EId(edge.0)))
        .map(|(_, edge)| *edge)
        .collect();
    edges
        .iter()
        .filter(|edge| edge.3 == R)
        .filter_map(|&(id, a, b, _, _)| {
            let mut layer = vec![(vec![b], Vec::<u128>::new())];
            let mut witness = false;
            for depth in 0..=hi {
                if depth >= lo {
                    for (vertices, identities) in &layer {
                        let valid = match mode {
                            GraphWalkSearch::Trail => {
                                identities
                                    .iter()
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .len()
                                    == identities.len()
                            }
                            GraphWalkSearch::Acyclic => {
                                vertices
                                    .iter()
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .len()
                                    == vertices.len()
                            }
                            GraphWalkSearch::Simple => {
                                let count = vertices.len()
                                    - usize::from(
                                        vertices.len() > 1 && vertices.last() == Some(&b),
                                    );
                                vertices[..count]
                                    .iter()
                                    .collect::<std::collections::BTreeSet<_>>()
                                    .len()
                                    == count
                            }
                            _ => true,
                        };
                        witness |= valid && a != 2 && vertices.last() == Some(&a);
                    }
                }
                if depth == hi {
                    break;
                }
                let mut next = Vec::new();
                for (vertices, identities) in layer {
                    let from = *vertices.last().unwrap();
                    for &(eid, start, end, relation, _) in &edges {
                        if relation != S {
                            continue;
                        }
                        let target = match direction {
                            0 if start == from => Some(end),
                            1 if end == from => Some(start),
                            2 if start == from => Some(end),
                            2 if end == from => Some(start),
                            _ => None,
                        };
                        if let Some(target) = target {
                            let mut vertices = vertices.clone();
                            vertices.push(target);
                            let mut identities = identities.clone();
                            identities.push(eid);
                            next.push((vertices, identities));
                        }
                    }
                }
                layer = next;
            }
            (witness != anti).then_some(EId(id))
        })
        .collect()
}

#[test]
fn async_path_support_matches_complete_routes_for_modes_bounds_directions_and_multigraphs() {
    let mut cases = 0;
    for mask in 0..64 {
        for direction in 0..3 {
            for (lo, hi) in [(0, 0), (0, 3), (1, 3), (3, 3)] {
                for mode in [
                    GraphWalkSearch::All,
                    GraphWalkSearch::AllShortest,
                    GraphWalkSearch::AnyShortest,
                    GraphWalkSearch::Trail,
                    GraphWalkSearch::Acyclic,
                    GraphWalkSearch::Simple,
                ] {
                    for anti in [false, true] {
                        let query = walk_text(direction, lo, hi, anti);
                        let source = Source::new(mask);
                        let counts = source.counts.clone();
                        let mut cursor =
                            AsyncEdgeJoinCursor::new(source, walk_plan(&query, mode), wide(), ok);
                        assert_eq!(
                            drain(&mut cursor).unwrap(),
                            walk_oracle(mask, direction, lo, hi, mode, anti, None),
                            "{query}, mask={mask}, mode={mode:?}"
                        );
                        assert!(counts.peak.load(Ordering::SeqCst) <= 2);
                        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 9216);
}

#[test]
fn async_paths_preserve_masking_zero_hops_chained_atoms_and_local_endpoint_properties() {
    for hidden in [EId(3), EId(4), EId(u128::MAX)] {
        for anti in [false, true] {
            let query = walk_text(2, 2, 3, anti);
            let mut source = Source::new(63);
            source.hidden = Some(hidden);
            let mut cursor = AsyncEdgeJoinCursor::new(
                source,
                walk_plan(&query, GraphWalkSearch::Trail),
                wide(),
                ok,
            );
            assert_eq!(
                drain(&mut cursor).unwrap(),
                walk_oracle(63, 2, 2, 3, GraphWalkSearch::Trail, anti, Some(hidden))
            );
        }
    }
    for (body, expected) in [
        (
            "MATCH WALK (b)-[:S*0..0]->(c) WHERE c.p IS NULL",
            vec![EId(2)],
        ),
        (
            "MATCH REPEATABLE ELEMENTS (b)-[:S*1..3]->(c)-[:R]->(d) WHERE c.p=a.p AND d.p=2",
            vec![EId(0), EId(1)],
        ),
        (
            "MATCH REPEATABLE ELEMENTS (b)-[:S*1..3]->(c)-[:S*0..0]->(d) WHERE d.p=a.p",
            vec![EId(0), EId(1)],
        ),
        (
            "MATCH REPEATABLE ELEMENTS (b)-[:S]->(c)-[:S*0..2]->(d) WHERE d.p=a.p",
            vec![EId(0), EId(1)],
        ),
    ] {
        let query = format!("MATCH (a)-[r:R]->(b) WHERE EXISTS {{ {body} }} RETURN r,a,b");
        let mut cursor = AsyncEdgeJoinCursor::new(Source::new(63), plan(&query), wide(), ok);
        assert_eq!(drain(&mut cursor).unwrap(), expected, "{query}");
    }
    let query = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH WALK (b)-[:S*0..0]->(c) } RETURN r,a,b";
    let mut source = Source::new(63);
    source.fault = Fault::Unavailable;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(query), wide(), ok);
    assert_eq!(drain(&mut cursor).unwrap(), vec![EId(0), EId(1), EId(2)]);
    assert_eq!(counts.seeks.load(Ordering::SeqCst), 0);
}

#[test]
fn async_path_source_errors_and_pending_cancellation_never_prove_absence() {
    let query = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH WALK (b)-[:S*1..3]->(c) WHERE c.p=999 } RETURN r,a,b";
    for fault in [
        Fault::Unavailable,
        Fault::Io,
        Fault::Missing,
        Fault::Twice,
        Fault::Mismatch,
        Fault::FalseEof,
        Fault::Repeat,
    ] {
        let mut source = Source::new(63);
        source.fault = fault;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(query), wide(), ok);
        assert!(run(cursor.next()).unwrap().is_err(), "fault failed to stop");
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let mut source = Source::new(63);
    source.fault = Fault::Pending;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(query), wide(), ok);
    let mut future = Box::pin(cursor.next());
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
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn async_path_support_shares_exact_limits_and_every_checkpoint_with_its_outer_binding() {
    let query = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH WALK (b)-[:S*1..3]->(c) WHERE c.p=999 } RETURN r,a,b";
    let mut calls = 0;
    let mut cursor = AsyncEdgeJoinCursor::new(Source::new(63), plan(query), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = drain(&mut cursor).unwrap();
    let rows = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    drop(cursor);
    assert_eq!(expected, vec![EId(0), EId(1), EId(2)]);
    for stop in 0..calls {
        let source = Source::new(63);
        let counts = source.counts.clone();
        let mut calls = 0;
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(query), wide(), || {
            let at = calls;
            calls += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(drain(&mut cursor), Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let exact = [
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    for dimension in 0..4 {
        for below in [false, true] {
            let mut limits = exact;
            limits[dimension] -= u64::from(below);
            let mut cursor = AsyncEdgeJoinCursor::new(
                Source::new(63),
                plan(query),
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                ok,
            );
            let result = drain(&mut cursor);
            if below {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), expected);
            }
        }
    }
}
