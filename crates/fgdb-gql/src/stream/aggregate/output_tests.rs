//! Completed-result clauses use the shared semantics, not source-row filters.
use super::*;
use crate::{GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder,
    GraphAggregateTest, GraphNullPlacement, GqlParameters, PreparedGraphAggregateText,
    RelationBind, GraphSymbolResolver};
use crate::algebra::IntegerComparison;
use std::cell::Cell;
use std::rc::Rc;

const K: PropertyKeyId = PropertyKeyId(1);
const V: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
type Record = (VId, Vec<(PropertyKeyId, CanonicalScalar)>);
struct Source {
    rows: Vec<Record>,
    at: usize,
    fail_at: Option<usize>,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq { CommitSeq(7) }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.at) { return Err(VertexScanSourceError::Source("late source")); }
        let next = self.rows.get(self.at).map(|r| r.0);
        if next.is_some() { self.at += 1; }
        Ok(next)
    }
    fn vertex<'a, C>(&'a self, id: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.reads.set(self.reads.get() + 1);
        let record = &self.rows[self.at - 1];
        assert_eq!(record.0, id);
        Ok(Some(VertexScanRow { labels: &[L], properties: &record.1 }))
    }
}
impl Drop for Source { fn drop(&mut self) { self.dropped.set(true); } }
fn source(rows: Vec<Record>) -> Source {
    Source { rows, at: 0, fail_at: None, reads: Rc::new(Cell::new(0)), dropped: Rc::new(Cell::new(false)) }
}
fn record(vid: u128, k: Option<i64>, v: Option<i64>) -> Record {
    let mut props = Vec::new();
    if let Some(k) = k { props.push((K, CanonicalScalar::Int(k))); }
    if let Some(v) = v { props.push((V, CanonicalScalar::Int(v))); }
    (VId(vid), props)
}
fn fixture(mask: u32) -> Vec<Record> {
    [(Some(0), Some(-2)), (Some(0), Some(4)), (Some(1), Some(4)),
     (Some(1), None), (Some(2), Some(-1)), (None, None)]
        .into_iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
        .map(|(at, (k, v))| record(if at == 5 { u128::MAX } else { at as u128 }, k, v)).collect()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn prepare(text: &str) -> PreparedGraphAggregate {
    let mut bind = RelationBind::new().with_label("L", L).with_property("k", K).with_property("v", V);
    PreparedGraphAggregateText::prepare(text, move |kind, name| bind.resolve_symbol(kind, name))
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn run(q: &PreparedGraphAggregate, rows: Vec<Record>, policy: GqlQueryPolicy)
    -> VertexAggregateCursor<Source, impl FnMut() -> Result<(), usize>> {
    VertexAggregateCursor::new(source(rows), VertexAggregatePlan::compile(q).unwrap(), policy, || Ok(()))
}
fn eager(q: &PreparedGraphAggregate, rows: &[Record]) -> Vec<GraphAggregateRow> {
    q.execute_governed(rows.len() as u64, rows.iter().map(|r| r.0), [],
        |id, tests| Ok::<_, ()>(rows.iter().find(|r| r.0 == id).is_some_and(|r|
            tests.iter().all(|p| p.matches(&[L], &r.1)))),
        |id, key| Ok(rows.iter().find(|r| r.0 == id).and_then(|r|
            r.1.iter().find(|(k, _)| *k == key).map(|(_, v)| v))),
        wide(), || Ok::<_, ()>(())).unwrap().value
}
fn scalar(props: &[(PropertyKeyId, CanonicalScalar)], key: PropertyKeyId) -> Option<i128> {
    props.iter().find(|(k, _)| *k == key).and_then(|(_, v)| match v {
        CanonicalScalar::Int(v) => Some(i128::from(*v)), _ => None,
    })
}

#[test]
fn all_6144_having_distinct_order_and_window_cases_match_independent_groups() {
    let base = prepare("MATCH (n:L) RETURN n.k AS key,COUNT(*) AS n,AVG(n.v) AS avg GROUP BY n.k");
    for mask in 0..64 {
        let rows = fixture(mask);
        let mut groups = BTreeMap::<Option<i128>, (u64, i128, u64)>::new();
        for (_, props) in &rows {
            let g = groups.entry(scalar(props, K)).or_default(); g.0 += 1;
            if let Some(v) = scalar(props, V) { g.1 += v; g.2 += 1; }
        }
        for descending in [false, true] { for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
            for distinct in [false, true] { for hidden in [false, true] { for minimum in [1, 2] {
                for (offset, count) in [(0, 0), (0, 2), (1, 2)] {
                    let q = PreparedGraphAggregate::prepare(base.input_pattern().clone(), base.group_key_columns(),
                        &[GraphAggregate::count_rows("n"), GraphAggregate::average_int("avg", base.aggregates()[1].argument_column().unwrap())],
                        offset, Some(count)).unwrap().with_result_clauses(&[GraphAggregateFilter {
                            column: GraphAggregateColumn::Aggregate(0), test: GraphAggregateTest::Integer {
                                comparison: IntegerComparison::GreaterOrEqual, value: minimum,
                            },
                        }], &[GraphAggregateOrder { column: GraphAggregateColumn::Aggregate(1), descending, nulls }])
                        .unwrap().with_key_output_columns(if hidden { &[] } else { &[0] }).unwrap()
                        .with_distinct_output(distinct);
                    // Exact small-domain rational comparator independent of the shared rank kernel.
                    let mut expected: Vec<_> = groups.iter().filter(|(_, (n, _, _))| i128::from(*n) >= minimum).collect();
                    expected.sort_by(|(ka, (_, sa, ca)), (kb, (_, sb, cb))| {
                        use core::cmp::Ordering;
                        let order = match (*ca == 0, *cb == 0) {
                            (true, true) => Ordering::Equal,
                            (true, false) => if nulls == GraphNullPlacement::First { Ordering::Less } else { Ordering::Greater },
                            (false, true) => if nulls == GraphNullPlacement::First { Ordering::Greater } else { Ordering::Less },
                            _ => { let order = (*sa * i128::from(*cb)).cmp(&(*sb * i128::from(*ca)));
                                if descending { order.reverse() } else { order } },
                        };
                        order.then_with(|| ka.cmp(kb))
                    });
                    let mut projected = Vec::new();
                    for (key, (n, sum, count)) in expected {
                        let key = if hidden { vec![] } else { vec![GraphValue::Scalar(key.map_or(CanonicalScalar::Null,
                            |k| CanonicalScalar::Int(i64::try_from(k).unwrap())))] };
                        let average = if *count == 0 { GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)) }
                            else { GraphAggregateValue::Average(GraphExactAverage::new(*sum, *count).unwrap()) };
                        let row = GraphAggregateRow::from_group_values(key, vec![GraphAggregateValue::Count(*n), average]);
                        if !distinct || !projected.contains(&row) { projected.push(row); }
                    }
                    let expected: Vec<_> = projected.into_iter().skip(offset as usize).take(count as usize).collect();
                    let canonical = q.canonical_bytes();
                    let batch = eager(&q, &rows);
                    let mut cursor = run(&q, rows.clone(), wide());
                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                    let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(actual, expected); assert_eq!(actual, batch);
                    assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                    assert_eq!(q.canonical_bytes(), canonical);
                    assert!(cursor.next().is_none());
                }
            }}}
        }}
    }
}

#[test]
fn computed_inputs_hidden_sort_cells_and_repeated_outputs_keep_the_native_domains() {
    for text in [
        "MATCH (n:L) RETURN SUM(n.v)+COUNT(*) AS s,n.k AS k,AVG(n.v) AS avg,n.k AS again GROUP BY n.k HAVING COUNT(*)>0 ORDER BY avg DESC NULLS FIRST LIMIT 3",
        "MATCH (n:L) RETURN DISTINCT COUNT(*) AS n GROUP BY n.k ORDER BY AVG(n.v) DESC NULLS LAST",
        "MATCH (n:L) RETURN DISTINCT COUNT(*)+0 AS n GROUP BY n.k HAVING COUNT(*)>0 ORDER BY AVG(n.v) DESC LIMIT 2",
        "MATCH (n:L) RETURN n AS id,SUM(n.v*2) AS v GROUP BY n HAVING COUNT(*)>0 ORDER BY id DESC LIMIT 2",
        "MATCH (n:L) RETURN [COUNT(*),SUM(n.v)] AS cells GROUP BY n.k ORDER BY AVG(n.v) DESC",
        "MATCH (n:L) RETURN COUNT(*) AS n HAVING n>0 ORDER BY n LIMIT 1",
    ] {
        let q = prepare(text); let rows = fixture(63);
        assert_eq!(run(&q, rows.clone(), wide()).collect::<Result<Vec<_>,_>>().unwrap(), eager(&q, &rows), "{text}");
    }
}

#[test]
fn zero_full_and_impossible_pages_do_not_hide_source_having_or_expression_errors() {
    for window in [" LIMIT 0", " LIMIT 1", " SKIP 18446744073709551615 LIMIT 1"] {
        for order in ["", " ORDER BY COUNT(*) DESC"] {
            let q = prepare(&format!("MATCH (n:L) RETURN 1/(COUNT(*)-1) AS bad GROUP BY n.k{order}{window}"));
            let mut cursor = run(&q, fixture(63), wide());
            assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::OutputExpression { .. })))));
            assert_eq!(cursor.row_stats().result_rows, 0); assert!(cursor.next().is_none());
            assert!(cursor.pending.is_none() && cursor.completed.is_none());
        }
        let q = prepare(&format!("MATCH (n:L) RETURN DISTINCT COUNT(*) AS n GROUP BY n.k ORDER BY n{window}"));
        let mut s = source(fixture(63)); s.fail_at = Some(s.rows.len()); let dropped = s.dropped.clone();
        let mut cursor = VertexAggregateCursor::new(s, VertexAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<_, ()>(()));
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::Source(VertexScanError::Source("late source")))))));
        assert!(dropped.get()); assert_eq!(cursor.row_stats().result_rows, 0); assert!(cursor.next().is_none());
        let q = prepare(&format!("MATCH (n:L) RETURN MIN(n.v) AS m GROUP BY n.k HAVING m>0 OR TRUE ORDER BY m{window}"));
        let mut rows = fixture(63); rows.last_mut().unwrap().1.push((V, CanonicalScalar::ucs_basic_text("private").unwrap()));
        let mut cursor = run(&q, rows, wide());
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { .. })))));
        assert_eq!(cursor.row_stats().result_rows, 0); assert!(cursor.next().is_none());
    }
    // Rejected groups do not evaluate output expressions.
    let q = prepare("MATCH (n:L) RETURN 1/(COUNT(*)-1) AS bad GROUP BY n.k HAVING COUNT(*)<0 LIMIT 1");
    assert!(run(&q, fixture(63), wide()).next().is_none());
}

#[test]
fn every_checkpoint_and_exact_boundary_preserve_a_complete_prefix_and_release_the_source() {
    let q = prepare("MATCH (n:L) RETURN DISTINCT AVG(n.v) AS avg GROUP BY n.k HAVING COUNT(*)>0 ORDER BY avg DESC NULLS LAST LIMIT 3");
    let mut calls = 0;
    let mut full = VertexAggregateCursor::new(source(fixture(63)), VertexAggregatePlan::compile(&q).unwrap(), wide(),
        || { calls += 1; Ok::<_, usize>(()) });
    let expected = full.by_ref().collect::<Result<Vec<_>,_>>().unwrap();
    let r = full.row_stats(); let e = full.evaluator_stats(); drop(full);
    assert_eq!(expected.len(), 3);
    let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
    assert_eq!(run(&q, fixture(63), exact).collect::<Result<Vec<_>,_>>().unwrap(), expected);
    for stop in 1..=calls {
        let mut seen = 0; let s = source(fixture(63)); let dropped = s.dropped.clone();
        let mut cursor = VertexAggregateCursor::new(s, VertexAggregatePlan::compile(&q).unwrap(), exact,
            || { seen += 1; if seen == stop { Err(stop) } else { Ok(()) } });
        let mut prefix = Vec::new();
        loop { match cursor.next().expect("checkpoint must be visited") {
            Ok(row) => prefix.push(row),
            Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); break; }
            Err(e) => panic!("unexpected refusal {e:?}"),
        }}
        assert!(expected.starts_with(&prefix)); assert!(dropped.get());
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.pending.is_none() && cursor.completed.is_none());
        assert!(cursor.next().is_none()); drop(cursor); assert_eq!(seen, stop);
    }
    for p in [GqlQueryPolicy::new(r.snapshot_records-1, r.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows-1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units-1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries-1)] {
        let mut cursor = run(&q, fixture(63), p);
        assert!(cursor.by_ref().collect::<Result<Vec<_>,_>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed); assert!(cursor.next().is_none());
    }
    let s = source(fixture(63)); let reads = s.reads.clone(); let dropped = s.dropped.clone();
    let mut cursor = VertexAggregateCursor::new(s, VertexAggregatePlan::compile(&q).unwrap(), wide(), || -> Result<(), ()> { panic!("unpolled close") });
    cursor.close(); assert!(dropped.get()); assert_eq!(reads.get(), 0); assert!(cursor.next().is_none());
    let mut cursor = run(&q, fixture(63), exact); cursor.next().unwrap().unwrap();
    let before = (cursor.row_stats(), cursor.evaluator_stats());
    cursor.close(); cursor.close(); assert!(cursor.completed.is_none()); assert!(cursor.next().is_none());
    assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
}

#[test]
fn private_groups_and_duplicate_classes_do_not_spend_the_selected_row_allowance() {
    let mut rows: Vec<_> = (0..256).map(|id| record(id, Some(id as i64), Some(7))).collect();
    rows.push(record(u128::MAX, Some(256), Some(-1)));
    for text in [
        "MATCH (n:L) RETURN DISTINCT AVG(n.v) AS avg GROUP BY n.k ORDER BY avg DESC SKIP 1 LIMIT 1",
        "MATCH (n:L) RETURN AVG(n.v) AS avg GROUP BY n.k ORDER BY avg ASC LIMIT 1",
        "MATCH (n:L) RETURN AVG(n.v) AS avg GROUP BY n.k SKIP 256 LIMIT 1",
    ] {
        let q = prepare(text); let mut cursor = run(&q, rows.clone(), GqlQueryPolicy::new(257,1,u64::MAX,u64::MAX));
        let result = cursor.next().unwrap().unwrap();
        assert_eq!(result.values(), &[GraphAggregateValue::Average(GraphExactAverage::new(-1,1).unwrap())]);
        assert_eq!(cursor.row_stats().snapshot_records,257); assert!(cursor.next().is_none());
    }
    for text in [
        "MATCH (n:L) RETURN COUNT(*) AS n HAVING n>0",
        "MATCH (n:L) RETURN COUNT(*) AS n LIMIT 0",
        "MATCH (n:L) RETURN DISTINCT COUNT(*) AS n GROUP BY n.k ORDER BY n",
    ] {
        assert!(run(&prepare(text), vec![], GqlQueryPolicy::new(0,0,u64::MAX,u64::MAX)).next().is_none());
    }
    let q = prepare("MATCH (n:L) RETURN COUNT(*) AS n ORDER BY n LIMIT 1");
    let mut global = run(&q, vec![], wide());
    assert_eq!(global.next().unwrap().unwrap().values(), &[GraphAggregateValue::Count(0)]);
    assert!(global.next().is_none());
}
