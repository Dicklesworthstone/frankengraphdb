use super::*;
use crate::algebra::GraphValue;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::LabelId;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1024 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
    panic!("fixture future did not finish");
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn plan(text: &str) -> AsyncEdgeScanPlan {
    let prepared = PreparedGraphText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let bytes = prepared.plan().canonical_bytes();
    let plan = AsyncEdgeScanPlan::compile(prepared.plan()).unwrap();
    assert_eq!(prepared.plan().canonical_bytes(), bytes);
    plan
}
fn query(direction: GlaDirection, any: bool, filter: usize, distinct: bool, skip: u64, limit: u64) -> String {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    let relation = if any { "" } else { ":R" };
    let filter = match filter {
        0 => "",
        1 => "WHERE r.p > 0",
        2 => "WHERE a.p < b.p",
        _ => "WHERE r.p IS NULL",
    };
    let quantifier = if distinct { "DISTINCT" } else { "ALL" };
    format!("MATCH (a){left}[r{relation}]{right}(b) {filter} RETURN {quantifier} \
        r, a, b, r.p AS ep, a.p AS ap, b.p AS bp SKIP {skip} LIMIT {limit}")
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100, 100, 100_000, 100_000) }
fn ok() -> Result<(), ()> { Ok(()) }

#[derive(Default)]
struct Counts {
    reads: AtomicUsize,
    drops: AtomicUsize,
    built: AtomicUsize,
    records: AtomicUsize,
    maximum_records: AtomicUsize,
    guards: AtomicUsize,
    reservations: AtomicUsize,
    events: AtomicUsize,
}
#[derive(Clone)]
struct Vertex {
    id: VId,
    labels: Vec<LabelId>,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}
#[derive(Clone)]
struct Image {
    source: VId,
    target: VId,
    relation: RelationId,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    vertices: Vec<Vertex>,
}
struct Record { image: Arc<Image>, counts: Arc<Counts> }
impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        EdgeScanRow { source: self.image.source, target: self.image.target,
            relation: self.image.relation, properties: &self.image.properties }
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        self.image.vertices.iter().find(|vertex| vertex.id == vid).map(|vertex|
            VertexScanRow { labels: &vertex.labels, properties: &vertex.properties })
    }
}
impl Drop for Record {
    fn drop(&mut self) { self.counts.records.fetch_sub(1, Ordering::SeqCst); }
}
struct Guard(Arc<Counts>);
impl Drop for Guard {
    fn drop(&mut self) { self.0.guards.fetch_sub(1, Ordering::SeqCst); }
}
#[derive(Clone, Copy)]
enum Admission { Exact, Missing, Twice, Different, Eof }
type Inputs = Vec<(EId, Option<Arc<Image>>)>;
struct Source {
    input: VecDeque<(EId, Option<Arc<Image>>)>,
    counts: Arc<Counts>,
    suspend: bool,
    admission: Admission,
    fail_at: Option<usize>,
    fail_reserve: bool,
    fail_evaluation: bool,
    fail_result: bool,
}
impl Source {
    fn new(input: Inputs) -> Self {
        Self { input: input.into(), counts: Arc::new(Counts::default()), suspend: true,
            admission: Admission::Exact, fail_at: None, fail_reserve: false,
            fail_evaluation: false, fail_result: false }
    }
}
impl Drop for Source {
    fn drop(&mut self) { self.counts.drops.fetch_add(1, Ordering::SeqCst); }
}
impl AsyncEdgeScanSource for Source {
    type Error = &'static str;
    type Record = Record;
    type OutputGuard = Guard;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(11) }
    fn next_candidate<C: Send>(
        &mut self,
        _relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> impl Future<Output = Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<Self::Error, C>>> + Send {
        async move {
            control(AsyncEdgeScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
            let at = self.counts.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(at) { return Err(EdgeScanSourceError::Source("read")); }
            let Some((eid, image)) = self.input.pop_front() else { return Ok(None) };
            if !matches!(self.admission, Admission::Missing) {
                control(AsyncEdgeScanEvent::Candidate(eid)).map_err(EdgeScanSourceError::Control)?;
            }
            if matches!(self.admission, Admission::Twice) {
                control(AsyncEdgeScanEvent::Candidate(eid)).map_err(EdgeScanSourceError::Control)?;
            }
            if self.suspend {
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if yielded { Poll::Ready(()) } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }).await;
            }
            if matches!(self.admission, Admission::Eof) { return Ok(None) }
            let record = if let Some(image) = image {
                control(AsyncEdgeScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
                self.counts.built.fetch_add(1, Ordering::SeqCst);
                let live = self.counts.records.fetch_add(1, Ordering::SeqCst) + 1;
                self.counts.maximum_records.fetch_max(live, Ordering::SeqCst);
                Some(Record { image, counts: self.counts.clone() })
            } else { None };
            let eid = if matches!(self.admission, Admission::Different) { EId(19) } else { eid };
            Ok(Some(AsyncEdgeCandidate { eid, record }))
        }
    }
    fn reserve_output<C>(
        &self, _record: &Record, columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        assert_eq!(columns, 6);
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.counts.reservations.fetch_add(1, Ordering::SeqCst);
        if self.fail_reserve { return Err(EdgeScanSourceError::Source("reserve")) }
        self.counts.guards.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(self.counts.clone()))
    }
    fn evaluation_event(&self, _record: &Record, event: GlaExecutionEvent) -> Result<(), Self::Error> {
        self.counts.events.fetch_add(1, Ordering::SeqCst);
        if self.fail_evaluation || (self.fail_result && matches!(event, GlaExecutionEvent::ResultRow)) {
            Err("evaluation")
        } else { Ok(()) }
    }
}
fn inputs() -> Inputs {
    let vertices: Vec<_> = IDS.into_iter().zip([1, 3, 9]).map(|(id, value)| Vertex {
        id, labels: vec![], properties: vec![(P, CanonicalScalar::Int(value))],
    }).collect();
    let mut input = vec![(EId(0), None)];
    for (id, from, to, relation, value) in [
        (1, 2, 0, R, Some(5)), (2, 2, 0, R, Some(-1)),
        (3, 0, 0, R, None), (4, 0, 2, S, Some(7)), (u128::MAX, 1, 0, R, None),
    ] {
        input.push((EId(id), Some(Arc::new(Image {
            source: IDS[from], target: IDS[to], relation,
            properties: value.map(|value| vec![(P, CanonicalScalar::Int(value))]).unwrap_or_default(),
            vertices: vertices.clone(),
        }))));
    }
    input
}
fn scalar(value: Option<&CanonicalScalar>) -> GraphValue {
    GraphValue::Scalar(value.cloned().unwrap_or(CanonicalScalar::Null))
}
// Independent finite-relation oracle: orient stored tuples, compare known
// integer payloads, sort full rows and window. No GLA or cursor code is called.
fn oracle(input: &Inputs, direction: GlaDirection, any: bool, filter: usize, skip: usize, limit: usize) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    for (eid, image) in input {
        let Some(image) = image else { continue };
        if !any && image.relation != R { continue }
        let orientations = match direction {
            GlaDirection::Forward => vec![(image.source, image.target)],
            GlaDirection::Reverse => vec![(image.target, image.source)],
            GlaDirection::Undirected if image.source == image.target => vec![(image.source, image.target)],
            _ => vec![(image.source, image.target), (image.target, image.source)],
        };
        let ep = image.properties.first().map(|(_, value)| value);
        for (a, b) in orientations {
            let ap = &image.vertices.iter().find(|vertex| vertex.id == a).unwrap().properties[0].1;
            let bp = &image.vertices.iter().find(|vertex| vertex.id == b).unwrap().properties[0].1;
            let accept = match filter {
                0 => true,
                1 => matches!(ep, Some(CanonicalScalar::Int(n)) if *n > 0),
                2 => matches!((ap, bp), (CanonicalScalar::Int(a), CanonicalScalar::Int(b)) if a < b),
                _ => ep.is_none_or(|value| matches!(value, CanonicalScalar::Null)),
            };
            if accept {
                rows.push(vec![GraphValue::Edge(*eid), GraphValue::Vertex(a), GraphValue::Vertex(b),
                    scalar(ep), scalar(Some(ap)), scalar(Some(bp))]);
            }
        }
    }
    rows.sort();
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn awaited_orientations_filters_parallel_edges_and_windows_match_independent_rows() {
    for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        for any in [false, true] {
            for filter in 0..4 {
                for distinct in [false, true] {
                    for (skip, limit) in [(0, 100), (1, 2), (2, 0), (99, 1)] {
                        let text = query(direction, any, filter, distinct, skip, limit);
                        let source = Source::new(inputs());
                        let counts = source.counts.clone();
                        let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
                        assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
                        assert_eq!(cursor.snapshot_seq(), CommitSeq(11));
                        let mut rows = Vec::new();
                        while let Some(row) = run(cursor.next()) {
                            rows.push(row.unwrap().values().to_vec());
                        }
                        assert_eq!(rows, oracle(&inputs(), direction, any, filter, skip as usize, limit as usize), "{text}");
                        assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        assert_eq!(counts.maximum_records.load(Ordering::SeqCst), usize::from(limit != 0));
                        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
                        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
                        assert!(run(cursor.next()).is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn reverse_orientation_reuses_record_and_does_not_prefetch_or_lose_its_output_guard() {
    let source = Source::new(vec![inputs()[1].clone(), inputs()[2].clone()]);
    let counts = source.counts.clone();
    let text = query(GlaDirection::Undirected, false, 0, false, 0, 2);
    let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
    let first = run(cursor.next()).unwrap().unwrap();
    assert_eq!(counts.reads.load(Ordering::SeqCst), 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    let second = run(cursor.next()).unwrap().unwrap();
    assert_eq!(counts.reads.load(Ordering::SeqCst), 1);
    assert_eq!(counts.built.load(Ordering::SeqCst), 1);
    assert_eq!(cursor.row_stats().snapshot_records, 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 2);
    assert_ne!(first.values()[1], second.values()[1]);
    drop(first);
    let (row, guard) = second.into_parts();
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    drop(row);
    drop(guard);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn source_and_output_admission_precede_record_and_result_allocation() {
    for (snapshot, result, work, scratch) in [(0, 10, 10000, 10000), (10, 0, 10000, 10000),
        (10, 10, 0, 10000), (10, 10, 10000, 0)] {
        let source = Source::new(vec![inputs()[1].clone()]);
        let counts = source.counts.clone();
        let text = query(GlaDirection::Forward, false, 0, false, 0, 100);
        let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text),
            GqlQueryPolicy::new(snapshot, result, work, scratch), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.reservations.load(Ordering::SeqCst), 0);
        assert_eq!(cursor.row_stats().result_rows, 0);
        if snapshot == 0 || scratch == 0 || work == 0 { assert_eq!(counts.built.load(Ordering::SeqCst), 0); }
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
    }
    let source = Source::new(vec![inputs()[1].clone()]);
    let counts = source.counts.clone();
    let text = query(GlaDirection::Undirected, false, 0, false, 0, 100);
    let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), GqlQueryPolicy::new(1, 1, 10000, 10000), ok);
    assert!(run(cursor.next()).unwrap().is_ok());
    assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Rows(_))));
    assert_eq!(counts.reservations.load(Ordering::SeqCst), 1);
    assert_eq!(cursor.row_stats().snapshot_records, 1);
    assert_eq!(cursor.row_stats().result_rows, 1);
}

#[test]
fn malformed_source_admissions_dangling_endpoints_and_late_read_errors_are_terminal() {
    let text = query(GlaDirection::Forward, false, 0, false, 0, 100);
    for admission in [Admission::Missing, Admission::Twice, Admission::Different, Admission::Eof] {
        let mut source = Source::new(vec![inputs()[1].clone()]);
        source.admission = admission;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
        assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Source(EdgeScanError::InvalidCandidateAdmission))));
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
    }
    for reverse in [false, true] {
        let mut input = vec![inputs()[1].clone(), inputs()[2].clone()];
        if reverse { input.reverse(); } else { input[1].0 = input[0].0; }
        let mut cursor = AsyncEdgeScanCursor::new(Source::new(input), plan(&text), policy(), ok);
        assert!(run(cursor.next()).unwrap().is_ok());
        assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity))));
    }
    let mut input = inputs()[1].clone();
    Arc::make_mut(input.1.as_mut().unwrap()).vertices.clear();
    let mut cursor = AsyncEdgeScanCursor::new(Source::new(vec![input]), plan(&text), policy(), ok);
    assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))));
    let mut source = Source::new(inputs()[1..].to_vec());
    source.fail_at = Some(1);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
    assert!(run(cursor.next()).unwrap().is_ok());
    assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Source(EdgeScanError::Source("read")))));
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert!(run(cursor.next()).is_none());
}

#[test]
fn dropping_unpolled_and_pending_pulls_has_distinct_lifecycles_and_no_hidden_drain() {
    let text = query(GlaDirection::Undirected, false, 0, false, 0, 100);
    let source = Source::new(inputs());
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
    drop(cursor.next());
    assert_eq!(cursor.state(), EdgeScanState::Open);
    assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
    let mut future = Box::pin(cursor.next());
    fn send<T: Send>(_: &T) {}
    send(&future);
    assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert!(run(cursor.next()).is_none());
    let source = Source::new(vec![inputs()[1].clone()]);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
    let output = run(cursor.next()).unwrap().unwrap();
    cursor.close();
    cursor.close();
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.reads.load(Ordering::SeqCst), 1);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    assert_eq!(cursor.state(), EdgeScanState::Closed);
    drop(output);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_after_await_and_every_checkpoint_preserves_complete_prefix_only() {
    let text = query(GlaDirection::Undirected, false, 0, false, 0, 100);
    let cancelled = Arc::new(AtomicBool::new(false));
    let check = cancelled.clone();
    let mut cursor = AsyncEdgeScanCursor::new(Source::new(vec![inputs()[1].clone()]), plan(&text), policy(), move || {
        if check.load(Ordering::SeqCst) { Err(()) } else { Ok(()) }
    });
    let mut future = Box::pin(cursor.next());
    assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(run(future), Some(Err(GqlQueryError::Interrupted(())))));
    assert_eq!(cursor.row_stats().result_rows, 0);
    let events = Arc::new(AtomicUsize::new(0));
    let counter = events.clone();
    let mut cursor = AsyncEdgeScanCursor::new(Source::new(inputs()), plan(&text), policy(), move || {
        counter.fetch_add(1, Ordering::SeqCst); Ok::<_, usize>(())
    });
    while let Some(row) = run(cursor.next()) { row.unwrap(); }
    for stop in 0..events.load(Ordering::SeqCst) {
        let source = Source::new(inputs());
        let counts = source.counts.clone();
        let mut at = 0;
        let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), move || {
            let current = at; at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        loop {
            match run(cursor.next()).expect("a checkpoint refusal must surface") {
                Ok(_) => {}
                Err(GqlQueryError::Interrupted(found)) => { assert_eq!(found, stop); break; }
                Err(error) => panic!("unexpected refusal: {error:?}"),
            }
        }
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert!(run(cursor.next()).is_none());
    }
}

#[test]
fn host_memory_and_final_delivery_refusals_drop_guards_and_do_not_increment_results() {
    let text = query(GlaDirection::Forward, false, 0, false, 0, 100);
    for mode in 0..3 {
        let mut source = Source::new(vec![inputs()[1].clone()]);
        source.fail_reserve = mode == 0;
        source.fail_evaluation = mode == 1;
        source.fail_result = mode == 2;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeScanCursor::new(source, plan(&text), policy(), ok);
        assert!(matches!(run(cursor.next()).unwrap(), Err(GqlQueryError::Source(EdgeScanError::Source(_)))));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn asynchronous_compilation_refuses_nested_reads_and_non_streamable_output() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, s, c",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN r, a",
        "MATCH (a)-[r:R]->(b) RETURN r, a ORDER BY a",
        "MATCH (a)-[r:R]->(b) RETURN a, r",
    ] {
        let prepared = PreparedGraphText::prepare(text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        assert!(AsyncEdgeScanPlan::compile(prepared.plan()).is_err(), "{text}");
    }
}

// A synchronous reader of the same immutable fixtures. It is an independent
// source implementation, not an async cursor polled inside synchronous code.
struct SyncSource { input: Inputs, at: usize }
impl EdgeScanSource for SyncSource {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(11) }
    fn next_edge<C>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let eid = self.input.get(self.at).map(|(eid, _)| *eid);
        self.at += usize::from(eid.is_some());
        Ok(eid)
    }
    fn edge<'a, C>(&'a self, eid: EId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.input.iter().find(|(id, _)| *id == eid).and_then(|(_, image)| image.as_ref())
            .map(|image| EdgeScanRow { source: image.source, target: image.target,
                relation: image.relation, properties: &image.properties }))
    }
    fn vertex<'a, C>(&'a self, vid: VId, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(self.input.iter().filter_map(|(_, image)| image.as_ref())
            .flat_map(|image| image.vertices.iter()).find(|vertex| vertex.id == vid)
            .map(|vertex| VertexScanRow { labels: &vertex.labels, properties: &vertex.properties }))
    }
}

#[test]
fn positional_endpoint_comparisons_execute_in_both_drivers_and_reject_foreign_slots() {
    use crate::algebra::{BindingSlot, IntegerComparison};
    for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        let text = query(direction, false, 0, false, 0, 100);
        let prepared = PreparedGraphText::prepare(&text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        let mut ops = prepared.plan().operators().to_vec();
        let at = ops.iter().position(|op| matches!(op, GlaOperator::ProjectValues { .. })).unwrap();
        // Exercise this IR directly, regardless of whether a text compiler
        // elects to represent the same predicate as SelectBoolean today.
        ops.insert(at, GlaOperator::CompareProperties {
            left: BindingSlot(0), left_key: P, right: BindingSlot(1), right_key: P,
            comparison: IntegerComparison::Less,
        });
        let logical = GlaPlan::<GraphValueRow>::from_operators(ops.clone());
        let mut asynchronous = AsyncEdgeScanCursor::new(Source::new(inputs()),
            AsyncEdgeScanPlan::compile(&logical).unwrap(), policy(), ok);
        let mut rows = Vec::new();
        while let Some(row) = run(asynchronous.next()) { rows.push(row.unwrap().values().to_vec()); }
        let expected = oracle(&inputs(), direction, false, 2, 0, 100);
        assert_eq!(rows, expected);
        let synchronous = EdgeScanCursor::new(SyncSource { input: inputs(), at: 0 },
            EdgeScanPlan::compile(&logical).unwrap(), policy(), ok);
        let rows: Vec<_> = synchronous.map(|row| row.unwrap().values().to_vec()).collect();
        assert_eq!(rows, expected);
        for side in 0..2 {
            let mut invalid = ops.clone();
            let GlaOperator::CompareProperties { left, right, .. } = &mut invalid[at] else { unreachable!() };
            if side == 0 { *left = BindingSlot(2); } else { *right = BindingSlot(2); }
            let logical = GlaPlan::<GraphValueRow>::from_operators(invalid);
            assert_eq!(EdgeScanPlan::compile(&logical).unwrap_err().operator, at);
            assert_eq!(AsyncEdgeScanPlan::compile(&logical).unwrap_err().operator, at);
        }
    }
}

#[test]
fn endpoint_nulls_and_property_domains_do_not_resolve_through_captured_edge_slots() {
    use crate::algebra::{BindingSlot, IntegerComparison};
    let text = query(GlaDirection::Forward, false, 0, false, 0, 100);
    let prepared = PreparedGraphText::prepare(&text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let mut ops = prepared.plan().operators().to_vec();
    let at = ops.iter().position(|op| matches!(op, GlaOperator::ProjectValues { .. })).unwrap();
    ops.insert(at, GlaOperator::CompareProperties {
        left: BindingSlot(0), left_key: P, right: BindingSlot(1), right_key: P,
        comparison: IntegerComparison::Less,
    });
    let logical = GlaPlan::<GraphValueRow>::from_operators(ops);
    for missing in [true, false] {
        let mut input = vec![inputs()[1].clone()];
        let image = Arc::make_mut(input[0].1.as_mut().unwrap());
        let source_id = image.source;
        let vertex = image.vertices.iter_mut().find(|vertex| vertex.id == source_id).unwrap();
        if missing { vertex.properties.clear(); }
        else { vertex.properties[0].1 = CanonicalScalar::Null; }
        // The captured edge value would PASS (< target.p == 1). A vertex
        // NULL/missing property must never be read from capture ordinal zero.
        image.properties[0].1 = CanonicalScalar::Int(-10);
        let counts;
        {
            let source = Source::new(input.clone());
            counts = source.counts.clone();
            let mut cursor = AsyncEdgeScanCursor::new(source,
                AsyncEdgeScanPlan::compile(&logical).unwrap(), policy(), ok);
            assert!(run(cursor.next()).is_none());
            assert_eq!(cursor.row_stats().snapshot_records, 1);
            assert_eq!(cursor.row_stats().result_rows, 0);
        }
        assert_eq!(counts.reservations.load(Ordering::SeqCst), 0);
        let mut cursor = EdgeScanCursor::new(SyncSource { input, at: 0 },
            EdgeScanPlan::compile(&logical).unwrap(), policy(), ok);
        assert!(cursor.next().is_none());
    }
}
