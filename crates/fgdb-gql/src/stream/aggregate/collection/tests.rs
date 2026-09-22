use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, PreparedGraphPattern};
use crate::{GraphAggregate, GraphAggregateColumn, GraphAggregateOrder, GraphSetProjection, GraphSetValue};
use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

const P: PropertyKeyId = PropertyKeyId(1);
const B: PropertyKeyId = PropertyKeyId(2);
type Rows = BTreeMap<VId, Vec<(PropertyKeyId, CanonicalScalar)>>;
struct Source {
    rows: Rows,
    after: Option<VId>,
    fail: Option<VId>,
    reads: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Source {
    fn drop(&mut self) { self.drops.fetch_add(1, Ordering::SeqCst); }
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(19) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = self.rows.range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next().map(|(&vid, _)| vid);
        if next.is_some() && next == self.fail { return Err(VertexScanSourceError::Source("late source failure")); }
        if let Some(vid) = next { self.after = Some(vid); }
        Ok(next)
    }
    fn vertex<C>(&self, vid: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'_>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.rows.get(&vid).map(|properties| VertexScanRow { labels: &[], properties }))
    }
}
fn source(rows: Rows) -> Source {
    Source { rows, after: None, fail: None, reads: Arc::new(AtomicUsize::new(0)), drops: Arc::new(AtomicUsize::new(0)) }
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn input(identity_first: bool) -> PreparedGraphPattern<GraphValueRow> {
    let mut b = GraphPatternBuilder::new(); b.vertex("n").unwrap();
    let columns = if identity_first {
        vec![GraphColumn::vertex("id", "n"), GraphColumn::property("bucket", "n", B), GraphColumn::property("amount", "n", P)]
    } else { vec![GraphColumn::property("amount", "n", P)] };
    b.prepare_values(&columns, 0, None).unwrap().with_duplicates()
}
fn definition(grouped: bool, count: Option<u64>) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(input(true), if grouped { &[1] } else { &[] }, &[
        GraphAggregate::collect("values", 2), GraphAggregate::collect_distinct("support", 2),
        GraphAggregate::collect("vertices", 0), GraphAggregate::count_rows("n"),
    ], 0, count).unwrap()
}
fn batch(q: &PreparedGraphAggregate, rows: &Rows) -> Vec<GraphAggregateRow> {
    q.execute_governed(rows.len() as u64, rows.keys().copied(), [],
        |_, _| Ok::<_, &'static str>(true),
        |vid, key| Ok(rows[&vid].iter().find(|(k, _)| *k == key).map(|(_, value)| value)),
        wide(), || Ok::<_, ()>(())).unwrap().value
}
fn list(values: Vec<GraphValue>) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(values.into_boxed_slice()))
}
fn values(value: &GraphAggregateValue) -> &[GraphValue] {
    let GraphAggregateValue::Value(GraphValue::List(values)) = value else { panic!("collection result"); };
    values
}
fn state(function: GraphAggregateFunction) -> NumericState {
    NumericState::new_governed(function, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn snapshot(state: &NumericState) -> (Vec<GraphValue>, usize, usize, usize, usize) {
    match state {
        NumericState::Collect(values) => (values.clone(), 0, 0, 0, 0),
        NumericState::Distinct(s) => {
            let mut result = snapshot(&s.accumulator);
            result.1 = s.scalars.len(); result.2 = s.vertices.len(); result.3 = s.values.len(); result.4 = s.max_payload;
            result
        }
        _ => panic!("collection state"),
    }
}

#[test]
fn collection_preserves_first_occurrence_and_every_native_value_domain() {
    let raw = vec![
        GraphValue::Scalar(CanonicalScalar::Int(7)), GraphValue::Vertex(VId(7)),
        GraphValue::Edge(EId(7)), GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Bool(true)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("private collection payload").unwrap()),
        GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Null), GraphValue::Vertex(VId(u128::MAX))].into_boxed_slice()),
        GraphValue::Scalar(CanonicalScalar::Int(7)), GraphValue::Vertex(VId(7)),
        GraphValue::Scalar(CanonicalScalar::bytes(vec![3; 1025]).unwrap()),
    ];
    for function in [GraphAggregateFunction::Collect, GraphAggregateFunction::CollectDistinct] {
        let mut s = state(function); let mut expected = Vec::new(); let mut seen = BTreeSet::new();
        for value in &raw {
            s.update_governed::<(), ()>(Input::Value(value), 0, &mut |_| Ok(())).unwrap();
            if !value.is_null() && (function == GraphAggregateFunction::Collect || seen.insert(value.clone())) {
                expected.push(value.clone());
            }
        }
        s.update_governed::<(), ()>(Input::Scalar(None), 0, &mut |_| Ok(())).unwrap();
        assert_eq!(s.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(), list(expected));
        assert_eq!(state(function).finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(), list(vec![]));
    }
}

#[test]
fn every_append_refusal_or_unwind_leaves_membership_and_list_unchanged() {
    let first = GraphValue::Vertex(VId(u128::MAX));
    let next = GraphValue::Scalar(CanonicalScalar::bytes(vec![9; 513]).unwrap());
    for function in [GraphAggregateFunction::Collect, GraphAggregateFunction::CollectDistinct] {
        let make = || { let mut s = state(function);
            s.update_governed::<(), usize>(Input::Value(&first), 0, &mut |_| Ok(())).unwrap(); s };
        let mut full = make(); let mut calls = 0;
        full.update_governed::<(), usize>(Input::Value(&next), 0, &mut |_| { calls += 1; Ok(()) }).unwrap();
        for stop in 1..=calls {
            for unwind in [false, true] {
                let mut s = make(); let before = snapshot(&s); let mut at = 0;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    s.update_governed::<(), usize>(Input::Value(&next), 0, &mut |_| {
                        at += 1;
                        if at == stop { if unwind { panic!("checkpoint"); }
                            return Err(GqlQueryError::Interrupted(stop)); }
                        Ok(())
                    })
                }));
                if unwind { assert!(result.is_err()); }
                else { assert!(matches!(result, Ok(Err(GqlQueryError::Interrupted(n))) if n == stop)); }
                assert_eq!(snapshot(&s), before); assert_eq!(at, stop);
                s.update_governed::<(), usize>(Input::Value(&next), 0, &mut |_| Ok(())).unwrap();
                assert_eq!(snapshot(&s), snapshot(&full));
            }
        }
    }
}

#[test]
fn vertex_collections_match_independent_ordered_groups_and_batch_without_input_bag() {
    let choices = [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(9)), Some(CanonicalScalar::Int(-4))];
    for code in 0..256_usize {
        let mut rows = Rows::new();
        for (at, vid) in [0, 1, 1_u128 << 100, u128::MAX].into_iter().enumerate() {
            let mut properties: Vec<_> = choices[(code >> (2 * at)) & 3].clone().into_iter().map(|v| (P, v)).collect();
            properties.push((B, CanonicalScalar::Int((at % 2) as i64)));
            rows.insert(VId(vid), properties);
        }
        for grouped in [false, true] {
            let q = definition(grouped, None); let before = q.canonical_bytes();
            let expected = batch(&q, &rows);
            let mut groups = BTreeMap::<Option<i64>, (Vec<GraphValue>, Vec<GraphValue>)>::new();
            for (&vid, props) in &rows {
                let bucket = match props.last().unwrap().1 { CanonicalScalar::Int(n) => n, _ => panic!() };
                let (all, ids) = groups.entry(grouped.then_some(bucket)).or_default();
                ids.push(GraphValue::Vertex(vid));
                if let Some((_, value)) = props.iter().find(|(k, v)| *k == P && !v.is_null()) {
                    all.push(GraphValue::Scalar(value.clone()));
                }
            }
            let s = source(rows.clone()); let drops = s.drops.clone();
            let mut cursor = VertexAggregateCursor::new(s, VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()));
            let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(actual, expected); assert_eq!(actual.len(), groups.len());
            for (row, (_, (all, ids))) in actual.iter().zip(groups) {
                let mut support = BTreeSet::new();
                let unique: Vec<_> = all.iter().filter(|v| support.insert((*v).clone())).cloned().collect();
                assert_eq!(row.values(), &[list(all), list(unique), list(ids.clone()), GraphAggregateValue::Count(ids.len() as u64)]);
            }
            assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted); assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(q.canonical_bytes(), before);
        }
    }
}

#[test]
fn computed_collection_requires_the_childs_existing_canonical_order_proof() {
    for identity_first in [false, true] {
        let column = if identity_first { 2 } else { 0 };
        let q = PreparedGraphAggregate::prepare_projected(input(identity_first),
            vec![GraphSetProjection::new("v", GraphSetValue::Column(column))], &[],
            &[GraphAggregate::collect("values", 0)], 0, None).unwrap();
        let rows = BTreeMap::from([
            (VId(0), vec![(P, CanonicalScalar::Int(9)), (B, CanonicalScalar::Int(0))]),
            (VId(1), vec![(P, CanonicalScalar::Int(-4)), (B, CanonicalScalar::Int(0))]),
        ]);
        let expected = batch(&q, &rows);
        if identity_first {
            let result = VertexAggregateCursor::new(source(rows), VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(result, expected);
            assert_eq!(values(&result[0].values()[0]), &[GraphValue::Scalar(CanonicalScalar::Int(9)), GraphValue::Scalar(CanonicalScalar::Int(-4))]);
        } else {
            assert!(matches!(VertexAggregatePlan::compile(&q), Err(VertexAggregateBuildError::Scan(_))));
            assert_eq!(values(&expected[0].values()[0]), &[GraphValue::Scalar(CanonicalScalar::Int(-4)), GraphValue::Scalar(CanonicalScalar::Int(9))]);
        }
    }
}

#[test]
fn completed_collection_outputs_use_shared_ranking_projection_and_distinct() {
    let rows: Rows = (0..8_u128).map(|id| (VId(id), vec![(P, CanonicalScalar::Int((id % 2) as i64)), (B, CanonicalScalar::Int((id % 3) as i64))])).collect();
    for count in [None, Some(0), Some(1), Some(3)] {
        for distinct in [false, true] {
            let q = definition(true, count)
                .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap()
                .with_key_output_columns(&[]).unwrap()
                .with_output_projection(vec![
                    GraphSetProjection::new("list", GraphSetValue::Column(1)),
                    GraphSetProjection::new("size", GraphSetValue::Size(Box::new(GraphSetValue::Column(1)))),
                ]).unwrap().with_distinct_output(distinct);
            let expected = batch(&q, &rows);
            let actual = VertexAggregateCursor::new(source(rows.clone()), VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn collection_cursor_uses_cumulative_limits_fuses_on_refusal_and_never_drains_on_close() {
    let rows = BTreeMap::from([
        (VId(0), vec![(P, CanonicalScalar::Int(9)), (B, CanonicalScalar::Int(0))]),
        (VId(1), vec![(P, CanonicalScalar::Int(9)), (B, CanonicalScalar::Int(0))]),
        (VId(u128::MAX), vec![(P, CanonicalScalar::Int(-4)), (B, CanonicalScalar::Int(0))]),
    ]);
    let q = definition(false, None); let plan = VertexAggregatePlan::compile(&q).unwrap(); let mut calls = 0;
    let mut cursor = VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || { calls += 1; Ok::<_, usize>(()) });
    let expected = cursor.next().unwrap().unwrap(); let r = cursor.row_stats(); let e = cursor.evaluator_stats(); drop(cursor);
    let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
    assert_eq!(VertexAggregateCursor::new(source(rows.clone()), plan.clone(), exact, || Ok::<_, ()>(())).next().unwrap().unwrap(), expected);
    for stop in 1..=calls {
        let s = source(rows.clone()); let drops = s.drops.clone(); let mut at = 0;
        let mut c = VertexAggregateCursor::new(s, plan.clone(), exact, || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(c.next(), Some(Err(GqlQueryError::Interrupted(n))) if n == stop));
        assert_eq!(c.row_stats().result_rows, 0); assert_eq!(c.state(), VertexScanState::Failed);
        assert!(c.next().is_none()); assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    for p in [GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1)] {
        let mut c = VertexAggregateCursor::new(source(rows.clone()), plan.clone(), p, || Ok::<_, ()>(()));
        assert!(c.next().unwrap().is_err()); assert_eq!(c.row_stats().result_rows, 0); assert!(c.next().is_none());
    }
    let s = source(rows.clone()); let reads = s.reads.clone(); let drops = s.drops.clone();
    let mut c = VertexAggregateCursor::new(s, plan.clone(), wide(), || -> Result<(), ()> { panic!("close drove source"); });
    c.close(); c.close(); assert!(c.next().is_none()); assert_eq!(reads.load(Ordering::SeqCst), 0); assert_eq!(drops.load(Ordering::SeqCst), 1);
    for limit in [0, 1] {
        let q = definition(false, Some(limit)); let mut s = source(rows.clone()); s.fail = Some(VId(u128::MAX));
        let mut c = VertexAggregateCursor::new(s, VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()));
        assert!(matches!(c.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::Source(VertexScanError::Source("late source failure")))))));
        assert_eq!(c.row_stats().result_rows, 0); assert!(c.next().is_none());
    }
}

#[test]
fn large_collections_are_one_result_row_and_empty_global_is_a_real_list() {
    let rows: Rows = (0..4097_u128).map(|id| (VId(id), vec![(P, CanonicalScalar::Int((id % 2) as i64)), (B, CanonicalScalar::Int(0))])).collect();
    let q = definition(false, None);
    let mut c = VertexAggregateCursor::new(source(rows), VertexAggregatePlan::compile(&q).unwrap(), GqlQueryPolicy::new(4097, 1, u64::MAX, u64::MAX), || Ok::<_, ()>(()));
    let row = c.next().unwrap().unwrap(); assert_eq!(values(&row.values()[0]).len(), 4097); assert_eq!(values(&row.values()[1]).len(), 2);
    assert_eq!(c.row_stats().result_rows, 1); assert!(c.next().is_none());
    for grouped in [false, true] {
        let q = definition(grouped, None);
        let actual = VertexAggregateCursor::new(source(Rows::new()), VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, batch(&q, &Rows::new()));
        if !grouped { assert_eq!(actual[0].values(), &[list(vec![]), list(vec![]), list(vec![]), GraphAggregateValue::Count(0)]); }
    }
}
