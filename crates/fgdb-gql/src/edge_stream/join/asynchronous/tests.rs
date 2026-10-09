use super::*;
use crate::algebra::{GraphValue, PreparedGraphPattern};
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};
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
fn prepared(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn plan(text: &str) -> AsyncEdgeJoinPlan {
    let definition = prepared(text);
    let bytes = definition.plan().canonical_bytes();
    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    assert_eq!(definition.plan().canonical_bytes(), bytes);
    plan
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn ok() -> Result<(), ()> {
    Ok(())
}

#[derive(Default)]
struct Counts {
    roots: AtomicUsize,
    seeks: AtomicUsize,
    drops: AtomicUsize,
    records: AtomicUsize,
    peak: AtomicUsize,
    guards: AtomicUsize,
}
#[derive(Clone)]
struct Image {
    from: VId,
    to: VId,
    relation: RelationId,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
    left: Vec<(PropertyKeyId, CanonicalScalar)>,
    right: Vec<(PropertyKeyId, CanonicalScalar)>,
}
struct Record {
    image: Arc<Image>,
    counts: Arc<Counts>,
}
impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        EdgeScanRow {
            source: self.image.from,
            target: self.image.to,
            relation: self.image.relation,
            properties: &self.image.props,
        }
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        let properties = if vid == self.image.from {
            &self.image.left
        } else if vid == self.image.to {
            &self.image.right
        } else {
            return None;
        };
        Some(VertexScanRow {
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
#[derive(Clone, Copy)]
enum Admission {
    Exact,
    Missing,
    Twice,
    Different,
    Eof,
    Repeat,
}
struct Source {
    images: BTreeMap<EId, Arc<Image>>,
    outgoing: BTreeMap<VId, BTreeSet<EId>>,
    incoming: BTreeMap<VId, BTreeSet<EId>>,
    after: Option<EId>,
    counts: Arc<Counts>,
    admission: Admission,
    unavailable: bool,
    suspend: bool,
    fail_seek: Option<usize>,
    fail_reserve: bool,
    hidden: Option<EId>,
}
impl Source {
    fn new(mask: usize) -> Self {
        let mut source = Self {
            images: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            counts: Arc::default(),
            admission: Admission::Exact,
            unavailable: false,
            suspend: false,
            fail_seek: None,
            fail_reserve: false,
            hidden: None,
        };
        for (at, (id, from, to, relation, prop)) in [
            (0, 0, 0, R, -2),
            (1, 0, 1, R, -1),
            (2, 0, 1, R, 0),
            (3, 1, 2, R, 1),
            (4, 2, 0, S, 2),
            (u128::MAX, 2, 2, R, 3),
        ]
        .into_iter()
        .enumerate()
        {
            if mask & (1 << at) == 0 {
                continue;
            }
            let (from, to) = (IDS[from], IDS[to]);
            let properties = |id| {
                if id == IDS[2] {
                    Vec::new()
                } else {
                    vec![(P, CanonicalScalar::Int(if id == IDS[0] { 4 } else { 7 }))]
                }
            };
            source.outgoing.entry(from).or_default().insert(EId(id));
            source.incoming.entry(to).or_default().insert(EId(id));
            source.images.insert(
                EId(id),
                Arc::new(Image {
                    from,
                    to,
                    relation,
                    props: vec![(P, CanonicalScalar::Int(prop))],
                    left: properties(from),
                    right: properties(to),
                }),
            );
        }
        source
    }
    fn guard<C>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        if self.fail_reserve {
            return Err(EdgeScanSourceError::Source("reservation"));
        }
        self.counts.guards.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(self.counts.clone()))
    }
    fn candidate<C>(
        &self,
        id: Option<EId>,
        admission: Admission,
        control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<&'static str, C>> {
        let Some(id) = id else {
            return Ok(None);
        };
        if !matches!(admission, Admission::Missing) {
            control(AsyncEdgeScanEvent::Candidate(id)).map_err(EdgeScanSourceError::Control)?;
        }
        if matches!(admission, Admission::Twice) {
            control(AsyncEdgeScanEvent::Candidate(id)).map_err(EdgeScanSourceError::Control)?;
        }
        if matches!(admission, Admission::Eof) {
            return Ok(None);
        }
        control(AsyncEdgeScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        let record = if self.hidden == Some(id) {
            None
        } else {
            let live = self.counts.records.fetch_add(1, Ordering::SeqCst) + 1;
            self.counts.peak.fetch_max(live, Ordering::SeqCst);
            Some(Record {
                image: self.images[&id].clone(),
                counts: self.counts.clone(),
            })
        };
        let eid = if matches!(admission, Admission::Different) {
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
        CommitSeq(13)
    }
    async fn next_candidate<C: Send>(
        &mut self,
        _relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<Self::Error, C>> {
        self.counts.roots.fetch_add(1, Ordering::SeqCst);
        control(AsyncEdgeScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let id = self
            .images
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if id.is_some() {
            self.after = id;
        }
        self.candidate(id, Admission::Exact, control)
    }
    fn evaluation_event(&self, _: &Record, _: GlaExecutionEvent) -> Result<(), Self::Error> {
        Ok(())
    }
    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        panic!("a multi-record projection must use join admission");
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
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        control(AsyncEdgeScanEvent::Work)
            .map_err(|error| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error)))?;
        if self.suspend {
            std::future::pending::<()>().await;
        }
        if self.fail_seek == Some(seek) {
            return Err(EdgeExpansionSourceError::Read(EdgeScanSourceError::Source(
                "incident I/O",
            )));
        }
        let next = |index: &BTreeMap<VId, BTreeSet<EId>>| {
            index.get(&endpoint).and_then(|set| {
                set.range((after.map_or(Unbounded, Excluded), Unbounded))
                    .next()
                    .copied()
            })
        };
        let id = if matches!(self.admission, Admission::Repeat) && after.is_some() {
            after
        } else {
            match direction {
                GlaDirection::Forward => next(&self.outgoing),
                GlaDirection::Reverse => next(&self.incoming),
                GlaDirection::Undirected => match (next(&self.outgoing), next(&self.incoming)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                },
            }
        };
        self.candidate(id, self.admission, control)
            .map_err(EdgeExpansionSourceError::Read)
    }
    fn reserve_traversal<C>(
        &self,
        hops: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        assert!(hops > 0 && hops <= MAX_PATTERN_EDGES);
        self.guard(control)
    }
    fn reserve_join_output<C>(
        &self,
        records: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<Self::Error, C>> {
        assert_eq!(records.len(), 2);
        self.guard(control)
    }
}
fn atom(name: &str, end: &str, direction: GlaDirection, any: bool) -> String {
    let relation = if any { "" } else { ":R" };
    match direction {
        GlaDirection::Forward => format!("-[{name}{relation}]->({end})"),
        GlaDirection::Reverse => format!("<-[{name}{relation}]-({end})"),
        GlaDirection::Undirected => format!("-[{name}{relation}]-({end})"),
    }
}
fn statement(
    shape: usize,
    directions: [GlaDirection; 2],
    any: bool,
    filter: bool,
    skip: usize,
    count: usize,
    distinct: bool,
) -> String {
    let first = format!("MATCH (a){}", atom("r", "b", directions[0], any));
    let next = match shape {
        1 => format!(", (a){}", atom("s", "c", directions[1], any)),
        2 => atom("s", "a", directions[1], any),
        _ => atom("s", "c", directions[1], any),
    };
    let end = if shape == 2 { "a AS c" } else { "c" };
    let filter = if filter {
        "WHERE r.p >= 0 AND s.p < 2"
    } else {
        ""
    };
    format!(
        "{first}{next} {filter} RETURN {} r, a, s, b, {end}, r.p AS rp, s.p AS sp SKIP {skip} LIMIT {count}",
        if distinct { "DISTINCT" } else { "ALL" }
    )
}
// Independent Cartesian relation oracle; it knows no cursor, state machine or
// compiler. Evaluate every orientation, join endpoint identities, sort/page.
fn oracle(
    source: &Source,
    shape: usize,
    directions: [GlaDirection; 2],
    any: bool,
    filter: bool,
    skip: usize,
    count: usize,
) -> Vec<Vec<GraphValue>> {
    let orientations = |image: &Image, direction| match direction {
        GlaDirection::Forward => vec![(image.from, image.to)],
        GlaDirection::Reverse => vec![(image.to, image.from)],
        GlaDirection::Undirected if image.from == image.to => vec![(image.from, image.to)],
        _ => vec![(image.from, image.to), (image.to, image.from)],
    };
    let mut rows = Vec::new();
    for (&r, left) in &source.images {
        if source.hidden == Some(r) || (!any && left.relation != R) {
            continue;
        }
        for (a, b) in orientations(left, directions[0]) {
            for (&s, right) in &source.images {
                if source.hidden == Some(s) || (!any && right.relation != R) {
                    continue;
                }
                for (from, c) in orientations(right, directions[1]) {
                    if from != (if shape == 1 { a } else { b }) || (shape == 2 && c != a) {
                        continue;
                    }
                    if filter
                        && !matches!((&left.props[0].1, &right.props[0].1),
                        (CanonicalScalar::Int(x), CanonicalScalar::Int(y)) if *x >= 0 && *y < 2)
                    {
                        continue;
                    }
                    rows.push(vec![
                        GraphValue::Edge(r),
                        GraphValue::Vertex(a),
                        GraphValue::Edge(s),
                        GraphValue::Vertex(b),
                        GraphValue::Vertex(c),
                        GraphValue::Scalar(left.props[0].1.clone()),
                        GraphValue::Scalar(right.props[0].1.clone()),
                    ]);
                }
            }
        }
    }
    rows.sort();
    rows.into_iter().skip(skip).take(count).collect()
}
const CHAIN: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,a,s,b,c,r.p AS rp,s.p AS sp";

#[test]
fn indexed_async_chains_branches_and_cycles_match_independent_occurrence_oracles() {
    let directions = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    let mut cases = 0;
    for mask in [0, 10, 63] {
        for shape in 0..3 {
            for first in directions {
                for second in directions {
                    for any in [false, true] {
                        for filter in [false, true] {
                            for (skip, count) in [(0, 1000), (1, 3), (0, 0)] {
                                for distinct in [false, true] {
                                    let text = statement(
                                        shape,
                                        [first, second],
                                        any,
                                        filter,
                                        skip,
                                        count,
                                        distinct,
                                    );
                                    let source = Source::new(mask);
                                    let expected = oracle(
                                        &source,
                                        shape,
                                        [first, second],
                                        any,
                                        filter,
                                        skip,
                                        count,
                                    );
                                    let counts = source.counts.clone();
                                    let plan = plan(&text);
                                    assert_eq!(plan.hop_count(), 2);
                                    let mut cursor =
                                        AsyncEdgeJoinCursor::new(source, plan, policy(), ok);
                                    let rows = run(async {
                                        let mut rows = Vec::new();
                                        while let Some(row) = cursor.next().await {
                                            rows.push(row.unwrap().values().to_vec());
                                        }
                                        rows
                                    });
                                    assert_eq!(rows, expected, "{text} mask {mask}");
                                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                                    assert_eq!(cursor.snapshot_seq(), CommitSeq(13));
                                    assert!(run(cursor.next()).is_none());
                                    assert!(counts.peak.load(Ordering::SeqCst) <= 2);
                                    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                                    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
                                    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1944);
}

#[test]
fn zero_limit_unpolled_and_cancelled_nested_pulls_release_without_draining() {
    let source = Source::new(63);
    let counts = source.counts.clone();
    let mut cursor =
        AsyncEdgeJoinCursor::new(source, plan(&format!("{CHAIN} LIMIT 0")), policy(), ok);
    let future = cursor.next();
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Open);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.roots.load(Ordering::SeqCst), 0);
    assert_eq!(counts.seeks.load(Ordering::SeqCst), 0);
    assert_eq!(counts.peak.load(Ordering::SeqCst), 0);

    let mut source = Source::new(63);
    source.suspend = true;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
    let mut next = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&next);
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    drop(next);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn early_close_keeps_only_the_delivered_output_reservation() {
    let source = Source::new(63);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    assert_eq!(counts.records.load(Ordering::SeqCst), 2);
    let reads = counts.roots.load(Ordering::SeqCst) + counts.seeks.load(Ordering::SeqCst);
    cursor.close();
    cursor.close();
    assert_eq!(cursor.state(), EdgeScanState::Closed);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    assert_eq!(
        reads,
        counts.roots.load(Ordering::SeqCst) + counts.seeks.load(Ordering::SeqCst)
    );
    drop(cursor);
    assert_eq!(row.values()[0], GraphValue::Edge(EId(0)));
    drop(row);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn candidate_contract_and_missing_indexes_never_become_successful_eof() {
    for admission in [
        Admission::Missing,
        Admission::Twice,
        Admission::Different,
        Admission::Eof,
        Admission::Repeat,
    ] {
        let mut source = Source::new(63);
        source.admission = admission;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
        let error = run(async {
            loop {
                match cursor.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error,
                    None => panic!("false EOF"),
                }
            }
        });
        assert!(matches!(
            error,
            GqlQueryError::Source(
                EdgeScanError::InvalidCandidateAdmission | EdgeScanError::NonIncreasingIdentity
            )
        ));
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let mut source = Source::new(63);
    source.unavailable = true;
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
    assert!(matches!(
        run(cursor.next()),
        Some(Err(GqlQueryError::Source(
            EdgeScanError::ExpansionUnavailable
        )))
    ));
    assert!(run(cursor.next()).is_none());
    let mut source = Source::new(63);
    source.fail_seek = Some(1);
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
    assert!(run(cursor.next()).unwrap().is_ok());
    assert!(matches!(
        run(cursor.next()),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "incident I/O"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert!(run(cursor.next()).is_none());
}

#[test]
fn all_checkpoint_boundaries_and_exact_cumulative_allowances_are_enforced() {
    let mut calls = 0;
    let mut cursor = AsyncEdgeJoinCursor::new(Source::new(10), plan(CHAIN), policy(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    run(async {
        while let Some(row) = cursor.next().await {
            row.unwrap();
        }
    });
    let stats = cursor.evaluator_stats();
    let rows = cursor.row_stats();
    drop(cursor);
    assert!(calls > 0 && rows.result_rows > 0);
    for boundary in 0..calls {
        let source = Source::new(10);
        let counts = source.counts.clone();
        let mut at = 0;
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), || {
            let current = at;
            at += 1;
            if current == boundary {
                Err(boundary)
            } else {
                Ok(())
            }
        });
        let error = run(async {
            loop {
                match cursor.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error,
                    None => panic!("missed boundary"),
                }
            }
        });
        assert!(matches!(error, GqlQueryError::Interrupted(found) if found == boundary));
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    for dimension in 0..4 {
        for exact in [true, false] {
            let mut limits = [
                rows.snapshot_records,
                rows.result_rows,
                stats.work_units,
                stats.scratch_entries,
            ];
            if !exact {
                limits[dimension] -= 1;
            }
            let mut cursor = AsyncEdgeJoinCursor::new(
                Source::new(10),
                plan(CHAIN),
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                ok,
            );
            let result = run(async {
                while let Some(row) = cursor.next().await {
                    row?;
                }
                Ok::<_, GqlQueryError<EdgeScanError<&str>, ()>>(())
            });
            assert_eq!(result.is_ok(), exact, "dimension {dimension}");
        }
    }
    let mut source = Source::new(10);
    source.fail_reserve = true;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(CHAIN), policy(), ok);
    assert!(run(cursor.next()).unwrap().is_err());
    assert_eq!(counts.roots.load(Ordering::SeqCst), 0);
}

#[test]
fn sort_input_preserves_occurrences_and_defers_the_complete_tail() {
    let text = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN DISTINCT b ORDER BY b DESC SKIP 1 LIMIT 0";
    let definition = prepared(text);
    assert!(AsyncEdgeJoinPlan::compile(definition.plan()).is_err());
    let (plan, tail) = AsyncEdgeJoinPlan::compile_sort_input(definition.plan()).unwrap();
    assert!(tail.distinct());
    assert_eq!(tail.count(), Some(0));
    assert_eq!(tail.offset(), 1);
    let source = Source::new(63);
    let expected = oracle(
        &source,
        0,
        [GlaDirection::Forward; 2],
        false,
        false,
        0,
        1000,
    );
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan, policy(), ok);
    let actual = run(async {
        let mut actual = Vec::new();
        while let Some(row) = cursor.next().await {
            actual.push(row.unwrap().values()[0].clone());
        }
        actual
    });
    let mut actual = actual;
    actual.sort();
    let mut values: Vec<_> = expected.into_iter().map(|row| row[3].clone()).collect();
    values.sort();
    assert_eq!(actual, values);
    assert!(
        actual.len() > actual.iter().collect::<BTreeSet<_>>().len(),
        "input duplicates survive"
    );
    let source = Source::new(63);
    let mut cursor = AsyncEdgeJoinCursor::new(
        source,
        AsyncEdgeJoinPlan::compile_sort_input(definition.plan())
            .unwrap()
            .0,
        GqlQueryPolicy::new(0, 100, 100_000, 100_000),
        ok,
    );
    assert!(matches!(
        run(cursor.next()),
        Some(Err(GqlQueryError::Rows(_)))
    ));
}

#[test]
fn invisible_candidates_are_charged_but_do_not_resurrect_as_join_bindings() {
    let mut source = Source::new(63);
    source.hidden = Some(EId(3));
    let expected = oracle(&source, 0, [GlaDirection::Forward; 2], true, false, 0, 1000);
    let text = statement(0, [GlaDirection::Forward; 2], true, false, 0, 1000, false);
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan(&text), policy(), ok);
    let actual = run(async {
        let mut actual = Vec::new();
        while let Some(row) = cursor.next().await {
            actual.push(row.unwrap().values().to_vec());
        }
        actual
    });
    assert_eq!(actual, expected);
    assert!(
        cursor.row_stats().snapshot_records > 6,
        "nested candidates share root admission"
    );
}
