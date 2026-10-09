//! The native async join supplies private aggregate occurrences; this tiny
//! host exercises the existing exact cells, not an alternate graph evaluator.

use core::convert::Infallible;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{EdgeRelation, GlaDirection};
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeJoinSource, AsyncEdgeScanEvent, AsyncEdgeScanRecord,
    AsyncEdgeScanSource, EdgeExpansionSourceError, EdgeScanError, EdgeScanRow, EdgeScanSourceError,
    EdgeScanState, VertexScanRow,
};
use fgdb_gql::spill_aggregate::{
    AsyncEdgeJoinSpillAggregateCursor, AsyncEdgeJoinSpillAggregatePlan,
};
use fgdb_gql::stream::VertexScanEvent;
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
const QUERY: &str =
    "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS rows, SUM(r.p+s.p) AS total";
type Candidate<C> =
    Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<&'static str, C>>;

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let mut task = Context::from_waker(Waker::noop());
    for _ in 0..1024 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut task) {
            return value;
        }
    }
    panic!("finite fixture did not complete");
}
fn plan(text: &str) -> AsyncEdgeJoinSpillAggregatePlan {
    let prepared =
        PreparedGraphAggregateText::prepare(text, |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    AsyncEdgeJoinSpillAggregatePlan::compile(&prepared).unwrap()
}
fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, results, 10_000_000, 10_000_000)
}
fn ok() -> Result<(), ()> {
    Ok(())
}

#[derive(Default)]
struct Counts {
    sources: AtomicUsize,
    reads: AtomicUsize,
    records: AtomicUsize,
    peak: AtomicUsize,
    guards: AtomicUsize,
    traversals: AtomicUsize,
}
struct Image {
    from: VId,
    to: VId,
    relation: RelationId,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
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
        (vid == self.image.from || vid == self.image.to).then_some(VertexScanRow {
            labels: &[],
            properties: &[],
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
struct TraversalGuard(Arc<Counts>);
impl Drop for TraversalGuard {
    fn drop(&mut self) {
        self.0.traversals.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Source {
    edges: BTreeMap<EId, Arc<Image>>,
    outgoing: BTreeMap<VId, Vec<EId>>,
    incoming: BTreeMap<VId, Vec<EId>>,
    after: Option<EId>,
    counts: Arc<Counts>,
    suspend_nested: bool,
    unavailable: bool,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.counts.sources.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Source {
    fn new() -> Self {
        let mut value = Self {
            edges: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            after: None,
            counts: Arc::new(Counts::default()),
            suspend_nested: false,
            unavailable: false,
        };
        value.counts.sources.store(1, Ordering::SeqCst);
        for (eid, from, to, relation, p) in [
            (0, 0, 1, R, 2),
            (1, 0, 1, R, 3),
            (2, 1, 1, R, 5),
            (3, 1, 2, R, 7),
            (4, 2, 0, R, 11),
            (u128::MAX, 2, 2, RelationId(8), 13),
        ] {
            let (eid, from, to) = (EId(eid), IDS[from], IDS[to]);
            value.outgoing.entry(from).or_default().push(eid);
            value.incoming.entry(to).or_default().push(eid);
            value.edges.insert(
                eid,
                Arc::new(Image {
                    from,
                    to,
                    relation,
                    props: vec![(P, CanonicalScalar::Int(p))],
                }),
            );
        }
        value
    }
    fn candidate<C>(
        &self,
        id: Option<EId>,
        relation: EdgeRelation,
        control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
    ) -> Candidate<C> {
        control(AsyncEdgeScanEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        let Some(eid) = id else {
            return Ok(None);
        };
        control(AsyncEdgeScanEvent::Candidate(eid)).map_err(EdgeScanSourceError::Control)?;
        let image = &self.edges[&eid];
        let record = if relation.matches(image.relation) {
            control(AsyncEdgeScanEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
            let live = self.counts.records.fetch_add(1, Ordering::SeqCst) + 1;
            self.counts.peak.fetch_max(live, Ordering::SeqCst);
            Some(Record {
                image: image.clone(),
                counts: self.counts.clone(),
            })
        } else {
            None
        };
        Ok(Some(AsyncEdgeCandidate { eid, record }))
    }
    fn guard<C>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.counts.guards.fetch_add(1, Ordering::SeqCst);
        Ok(Guard(self.counts.clone()))
    }
}
impl AsyncEdgeScanSource for Source {
    type Error = &'static str;
    type Record = Record;
    type OutputGuard = Guard;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(17)
    }
    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Candidate<C> {
        let id = self
            .edges
            .keys()
            .find(|&&eid| self.after.is_none_or(|after| eid > after))
            .copied();
        let candidate = self.candidate(id, relation, control)?;
        if id.is_some() {
            self.after = id;
        }
        Ok(candidate)
    }
    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        self.guard(control)
    }
    fn evaluation_event(&self, _: &Record, _: GlaExecutionEvent) -> Result<(), &'static str> {
        Ok(())
    }
}
impl AsyncEdgeJoinSource for Source {
    type TraversalGuard = TraversalGuard;
    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeExpansionSourceError<&'static str, C>> {
        if self.unavailable {
            return Err(EdgeExpansionSourceError::Unavailable);
        }
        let seek = |index: &BTreeMap<VId, Vec<EId>>| {
            index
                .get(&endpoint)
                .and_then(|ids| ids.iter().find(|&&id| after.is_none_or(|a| id > a)))
                .copied()
        };
        let id = match direction {
            GlaDirection::Forward => seek(&self.outgoing),
            GlaDirection::Reverse => seek(&self.incoming),
            GlaDirection::Undirected => match (seek(&self.outgoing), seek(&self.incoming)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        };
        let candidate = self
            .candidate(id, relation, control)
            .map_err(EdgeExpansionSourceError::Read)?;
        if self.suspend_nested {
            std::future::pending::<()>().await;
        }
        Ok(candidate)
    }
    fn reserve_traversal<C>(
        &self,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<TraversalGuard, EdgeScanSourceError<&'static str, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.counts.traversals.fetch_add(1, Ordering::SeqCst);
        Ok(TraversalGuard(self.counts.clone()))
    }
    fn reserve_join_output<C>(
        &self,
        _: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Guard, EdgeScanSourceError<&'static str, C>> {
        self.guard(control)
    }
}
fn released(counts: &Counts) {
    assert_eq!(counts.sources.load(Ordering::SeqCst), 0);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.traversals.load(Ordering::SeqCst), 0);
}
fn atom(edge: &str, target: &str, direction: usize, any: bool) -> String {
    let relation = if any { "" } else { ":R" };
    match direction {
        0 => format!("-[{edge}{relation}]->({target})"),
        1 => format!("<-[{edge}{relation}]-({target})"),
        _ => format!("-[{edge}{relation}]-({target})"),
    }
}
fn orientations(edge: &Image, direction: usize) -> Vec<(VId, VId)> {
    match direction {
        0 => vec![(edge.from, edge.to)],
        1 => vec![(edge.to, edge.from)],
        _ if edge.from == edge.to => vec![(edge.from, edge.to)],
        _ => vec![(edge.from, edge.to), (edge.to, edge.from)],
    }
}

#[test]
fn joined_aggregate_occurrences_match_108_independent_relation_summaries() {
    let mut cases = 0;
    for shape in 0..3 {
        for left in 0..3 {
            for right in 0..3 {
                for any in [false, true] {
                    for filter in [false, true] {
                        let first = atom("r", "b", left, any);
                        let next = atom("s", if shape == 2 { "a" } else { "c" }, right, any);
                        let text = format!(
                            "MATCH (a){first}{}{next} {} RETURN COUNT(*) AS rows, SUM(r.p+s.p) AS total",
                            if shape == 1 { ", (a)" } else { "" },
                            if filter { "WHERE r.p < s.p" } else { "" }
                        );
                        let source = Source::new();
                        let counts = source.counts.clone();
                        // Independent cartesian relation join, not the cursor's DFS or GLA.
                        let mut expected_count = 0_u64;
                        let mut expected_sum = 0_i128;
                        for r in source.edges.values() {
                            for s in source.edges.values() {
                                if !any && (r.relation != R || s.relation != R) {
                                    continue;
                                }
                                let (CanonicalScalar::Int(rp), CanonicalScalar::Int(sp)) =
                                    (&r.props[0].1, &s.props[0].1)
                                else {
                                    unreachable!()
                                };
                                if filter && rp >= sp {
                                    continue;
                                }
                                for (a, b) in orientations(r, left) {
                                    for (from, c) in orientations(s, right) {
                                        if from != (if shape == 1 { a } else { b })
                                            || shape == 2 && c != a
                                        {
                                            continue;
                                        }
                                        expected_count += 1;
                                        expected_sum += i128::from(*rp) + i128::from(*sp);
                                    }
                                }
                            }
                        }
                        let plan = plan(&text);
                        let definition = plan.definition().clone();
                        let mut cursor =
                            AsyncEdgeJoinSpillAggregateCursor::new(source, plan, policy(1), ok);
                        let mut state = definition
                            .new_state(&mut |event| cursor.charge::<Infallible, ()>(event))
                            .unwrap();
                        let mut observed = 0;
                        while let Some(row) = run(cursor.next_input(&mut |guard, _| {
                            assert_eq!(guard.0.guards.load(Ordering::SeqCst), 1);
                            Ok(())
                        }))
                        .unwrap()
                        {
                            definition
                                .update(&mut state, &row, &mut |event| {
                                    cursor.charge::<Infallible, ()>(event)
                                })
                                .unwrap();
                            observed += 1;
                            assert_eq!(
                                cursor.row_stats().result_rows,
                                0,
                                "private input is not a final row"
                            );
                        }
                        assert_eq!(observed, expected_count, "{text}");
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        assert!(cursor.row_stats().snapshot_records > 6);
                        assert!(counts.peak.load(Ordering::SeqCst) <= 2);
                        released(&counts);
                        let row = definition
                            .finish(vec![], state, &mut |event| {
                                cursor.charge::<Infallible, ()>(event)
                            })
                            .unwrap();
                        assert_eq!(
                            row.values()[0],
                            GraphAggregateValue::Count(expected_count),
                            "{text}"
                        );
                        if expected_count != 0 {
                            assert_eq!(
                                row.values()[1],
                                GraphAggregateValue::Integer(expected_sum),
                                "{text}"
                            );
                        }
                        cursor.finish_result::<Infallible, ()>().unwrap();
                        assert_eq!(cursor.row_stats().result_rows, 1);
                        assert!(matches!(
                            cursor.finish_result::<Infallible, ()>(),
                            Err(GqlQueryError::Rows(_))
                        ));
                        assert_eq!(cursor.state(), EdgeScanState::Failed);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 108);
}

#[test]
fn dropped_nested_io_and_refused_computed_memory_release_the_entire_join() {
    for suspended in [false, true] {
        let mut source = Source::new();
        source.suspend_nested = suspended;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinSpillAggregateCursor::new(source, plan(QUERY), policy(0), ok);
        let mut control = |_: &mut Guard, event| {
            if event == VertexScanEvent::ScratchEntry {
                Err("computed memory")
            } else {
                Ok(())
            }
        };
        if suspended {
            let mut future = Box::pin(cursor.next_input(&mut control));
            fn require_send(_: &impl Send) {}
            require_send(&future);
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(counts.records.load(Ordering::SeqCst), 2);
            drop(future);
        } else {
            assert!(matches!(
                run(cursor.next_input(&mut control)),
                Err(GqlQueryError::Source(GraphAggregateError::Source(
                    EdgeScanError::Source("computed memory")
                )))
            ));
        }
        released(&counts);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(
            run(cursor.next_input(&mut |_, _| Ok(())))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn final_zero_row_budget_does_not_mask_bad_domains_or_missing_indexes() {
    for missing_index in [false, true] {
        let mut source = Source::new();
        source.unavailable = missing_index;
        Arc::get_mut(source.edges.get_mut(&EId(2)).unwrap())
            .unwrap()
            .props[0]
            .1 = CanonicalScalar::ucs_basic_text("not numeric").unwrap();
        let counts = source.counts.clone();
        let text = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN SUM(s.p) AS total LIMIT 0";
        let mut cursor = AsyncEdgeJoinSpillAggregateCursor::new(source, plan(text), policy(0), ok);
        let result = run(cursor.next_input(&mut |_, _| Ok(())));
        if missing_index {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphAggregateError::Source(
                    EdgeScanError::ExpansionUnavailable
                )))
            ));
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                    aggregate: 0
                }))
            ));
        }
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        released(&counts);
    }
}

#[test]
fn reduction_cannot_finish_open_or_closed_input_and_empty_input_can_finish_once() {
    for close in [false, true] {
        let source = Source::new();
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinSpillAggregateCursor::new(source, plan(QUERY), policy(1), ok);
        if close {
            cursor.close();
        }
        assert!(matches!(
            cursor.finish_result::<Infallible, ()>(),
            Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput
            ))
        ));
        assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
        released(&counts);
    }
    let mut source = Source::new();
    source.edges.clear();
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinSpillAggregateCursor::new(source, plan(QUERY), policy(1), ok);
    assert!(
        run(cursor.next_input(&mut |_, _| Ok(())))
            .unwrap()
            .is_none()
    );
    assert_eq!(cursor.row_stats().snapshot_records, 0);
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    cursor.finish_result::<Infallible, ()>().unwrap();
    assert_eq!(cursor.row_stats().result_rows, 1);
    released(&counts);
}

#[test]
fn every_first_input_checkpoint_can_refuse_without_a_delivered_summary() {
    let mut total = 0;
    {
        let mut cursor =
            AsyncEdgeJoinSpillAggregateCursor::new(Source::new(), plan(QUERY), policy(1), || {
                total += 1;
                Ok::<_, usize>(())
            });
        assert!(
            run(cursor.next_input(&mut |_, _| Ok(())))
                .unwrap()
                .is_some()
        );
    }
    assert!(total > 10);
    for stop in 1..=total {
        let source = Source::new();
        let counts = source.counts.clone();
        let mut observed = 0;
        let mut cursor =
            AsyncEdgeJoinSpillAggregateCursor::new(source, plan(QUERY), policy(1), || {
                observed += 1;
                if observed == stop { Err(stop) } else { Ok(()) }
            });
        assert!(
            matches!(run(cursor.next_input(&mut |_, _| Ok(()))), Err(GqlQueryError::Interrupted(at)) if at == stop)
        );
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        released(&counts);
        drop(cursor);
        assert_eq!(observed, stop);
    }
}

#[test]
fn distinct_cells_receive_external_support_without_erasing_ordinary_multiplicity() {
    let definition = plan(
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS rows, COUNT(DISTINCT s.p) AS support, SUM(DISTINCT s.p) AS total",
    );
    let mut cursor =
        AsyncEdgeJoinSpillAggregateCursor::new(Source::new(), definition, policy(1), ok);
    let definition = cursor.definition().clone();
    let columns: Vec<_> = definition.distinct_argument_columns().collect();
    assert_eq!(
        columns.len(),
        1,
        "COUNT and SUM share their argument support"
    );
    let mut state = definition
        .new_state(&mut |event| cursor.charge::<Infallible, ()>(event))
        .unwrap();
    // A deliberately small materialized test host supplies the external
    // uniqueness contract. The production adapter retains no support set.
    let mut support = BTreeMap::new();
    while let Some(row) = run(cursor.next_input(&mut |_, _| Ok(()))).unwrap() {
        definition
            .update(&mut state, &row, &mut |event| {
                cursor.charge::<Infallible, ()>(event)
            })
            .unwrap();
        support
            .entry(row.values()[columns[0]].clone())
            .or_insert_with(|| row.row().clone());
        assert_eq!(cursor.row_stats().result_rows, 0);
    }
    for row in support.values() {
        definition
            .update_distinct_argument(&mut state, row, columns[0], &mut |event| {
                cursor.charge::<Infallible, ()>(event)
            })
            .unwrap();
    }
    let result = definition
        .finish(vec![], state, &mut |event| {
            cursor.charge::<Infallible, ()>(event)
        })
        .unwrap();
    assert_eq!(
        result.values(),
        &[
            GraphAggregateValue::Count(9),
            GraphAggregateValue::Count(5),
            GraphAggregateValue::Integer(28)
        ]
    );
    cursor.finish_result::<Infallible, ()>().unwrap();
}

#[test]
fn exact_source_work_and_scratch_limits_apply_across_all_private_occurrences() {
    let mut cursor =
        AsyncEdgeJoinSpillAggregateCursor::new(Source::new(), plan(QUERY), policy(0), ok);
    let mut count = 0;
    while run(cursor.next_input(&mut |_, _| Ok(())))
        .unwrap()
        .is_some()
    {
        count += 1;
    }
    assert_eq!(count, 9);
    let rows = cursor.row_stats();
    let stats = cursor.evaluator_stats();
    assert_eq!(rows.result_rows, 0);
    for case in 0..4 {
        let source = Source::new();
        let counts = source.counts.clone();
        let policy = GqlQueryPolicy::new(
            rows.snapshot_records - u64::from(case == 1),
            0,
            stats.work_units - u64::from(case == 2),
            stats.scratch_entries - u64::from(case == 3),
        );
        let mut cursor = AsyncEdgeJoinSpillAggregateCursor::new(source, plan(QUERY), policy, ok);
        let result = loop {
            match run(cursor.next_input(&mut |_, _| Ok(()))) {
                Ok(Some(_)) => {}
                outcome => break outcome,
            }
        };
        if case == 0 {
            assert!(result.unwrap().is_none());
            assert_eq!(cursor.evaluator_stats(), stats);
            assert_eq!(cursor.row_stats(), rows);
        } else {
            assert!(result.is_err());
            assert_eq!(cursor.state(), EdgeScanState::Failed);
        }
        released(&counts);
    }
}
