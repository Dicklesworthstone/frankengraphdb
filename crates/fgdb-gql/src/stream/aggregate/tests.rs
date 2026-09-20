use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder};
use crate::GraphAggregate;
use std::cell::Cell;
use std::rc::Rc;

const KEY: PropertyKeyId = PropertyKeyId(7);
const LABEL: LabelId = LabelId(3);

#[derive(Clone)]
struct Row {
    vid: VId,
    visible: bool,
    labels: Vec<LabelId>,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}
struct Source {
    rows: Vec<Row>,
    at: usize,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    fail_at: Option<usize>,
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(9)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.at) {
            return Err(VertexScanSourceError::Source("source failed"));
        }
        let vid = self.rows.get(self.at).map(|row| row.vid);
        if vid.is_some() {
            self.at += 1;
        }
        Ok(vid)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.reads.set(self.reads.get() + 1);
        let row = &self.rows[self.at - 1];
        assert_eq!(row.vid, vid);
        Ok(row.visible.then_some(VertexScanRow {
            labels: &row.labels,
            properties: &row.properties,
        }))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
fn source(rows: Vec<Row>) -> Source {
    Source {
        rows,
        at: 0,
        reads: Rc::new(Cell::new(0)),
        dropped: Rc::new(Cell::new(false)),
        fail_at: None,
    }
}
fn row(vid: u128, value: Option<CanonicalScalar>) -> Row {
    Row {
        vid: VId(vid),
        visible: true,
        labels: vec![LABEL],
        properties: value.into_iter().map(|v| (KEY, v)).collect(),
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn input() -> crate::algebra::PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
    builder
        .prepare_values(&[GraphColumn::property("value", "n", KEY)], 0, None)
        .unwrap()
        .with_duplicates()
}
fn definition() -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 0),
            GraphAggregate::sum_int("sum", 0),
        ],
        0,
        None,
    )
    .unwrap()
}
fn expected(definition: &PreparedGraphAggregate, rows: &[Row]) -> GraphAggregateRow {
    definition
        .execute_governed(
            rows.len() as u64,
            rows.iter().filter(|row| row.visible).map(|row| row.vid),
            [],
            |vid, predicates| {
                Ok::<_, &'static str>(rows.iter().find(|row| row.vid == vid).is_some_and(|row| {
                    row.visible
                        && predicates
                            .iter()
                            .all(|predicate| predicate.matches(&row.labels, &row.properties))
                }))
            },
            |vid, key| {
                Ok::<_, &'static str>(rows.iter().find(|row| row.vid == vid).and_then(|row| {
                    row.properties
                        .iter()
                        .find(|(actual, _)| *actual == key)
                        .map(|(_, value)| value)
                }))
            },
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn exact_rows_match_existing_aggregate_engine_without_identity_led_projection() {
    let definition = definition();
    assert!(VertexScanPlan::compile(definition.input_pattern().plan()).is_err());
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(i64::MIN)),
        Some(CanonicalScalar::Int(i64::MAX)),
    ];
    for code in 0..256_usize {
        let rows: Vec<_> = [0, 1, 1_u128 << 100, u128::MAX]
            .into_iter()
            .enumerate()
            .map(|(at, vid)| {
                let mut row = row(vid, choices[(code >> (at * 2)) & 3].clone());
                row.visible = (code + at) % 7 != 0;
                if (code + at) % 5 == 0 {
                    row.labels.clear();
                }
                row
            })
            .collect();
        let expected = expected(&definition, &rows);
        let input = source(rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor =
            VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        assert!(!dropped.get());
        assert_eq!(cursor.snapshot_seq(), CommitSeq(9));
        assert_eq!(cursor.columns(), definition.aggregate_columns());
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        assert_eq!(
            cursor.row_stats(),
            GqlExecutionStats {
                snapshot_records: 4,
                result_rows: 1,
            }
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
}

#[test]
fn empty_input_releases_one_zero_count_and_null_sum_row() {
    let definition = definition();
    let mut cursor = VertexAggregateCursor::new(
        source(Vec::new()),
        VertexAggregatePlan::compile(&definition).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    let result = cursor.next().unwrap().unwrap();
    assert_eq!(result, expected(&definition, &[]));
    assert_eq!(result.values()[0].as_count(), Some(0));
    assert_eq!(result.values()[1].as_count(), Some(0));
    assert!(result.values()[2].is_null());
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert!(cursor.next().is_none());
}

#[test]
fn aggregate_scratch_is_independent_of_input_cardinality() {
    let plan = VertexAggregatePlan::compile(&definition()).unwrap();
    let mut expected_scratch = None;
    for count in [0_u64, 1, 100, 10_000] {
        let rows = (0..count)
            .map(|vid| row(u128::from(vid), Some(CanonicalScalar::Int(7))))
            .collect();
        let mut cursor =
            VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || Ok::<_, ()>(()));
        let result = cursor.next().unwrap().unwrap();
        assert_eq!(result.values()[0].as_count(), Some(count));
        assert_eq!(result.values()[1].as_count(), Some(count));
        if count > 0 {
            assert_eq!(result.values()[2].as_integer(), Some(i128::from(count) * 7));
        }
        let scratch = cursor.evaluator_stats().scratch_entries;
        assert_eq!(*expected_scratch.get_or_insert(scratch), scratch);
    }
}

#[test]
fn one_meter_enforces_exact_boundaries_and_never_counts_input_as_output() {
    let plan = VertexAggregatePlan::compile(&definition()).unwrap();
    let rows = vec![
        row(0, Some(CanonicalScalar::Int(5))),
        row(1, None),
        row(2, Some(CanonicalScalar::Int(-3))),
    ];
    let mut baseline =
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || Ok::<_, ()>(()));
    let expected = baseline.next().unwrap().unwrap();
    let stats = baseline.evaluator_stats();
    let run = |policy| {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut cursor =
            VertexAggregateCursor::new(input, plan.clone(), policy, || Ok::<_, ()>(()));
        let result = cursor.next().unwrap();
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        result
    };
    assert_eq!(
        run(GqlQueryPolicy::new(3, 1, stats.work_units, stats.scratch_entries)).unwrap(),
        expected
    );
    for policy in [
        GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(3, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(3, 1, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(3, 1, u64::MAX, stats.scratch_entries - 1),
    ] {
        assert!(run(policy).is_err());
    }
    for policy in [
        GqlQueryPolicy::new(0, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(3, 0, u64::MAX, u64::MAX),
    ] {
        let input = source(rows.clone());
        let reads = Rc::clone(&input.reads);
        let mut cursor =
            VertexAggregateCursor::new(input, plan.clone(), policy, || Ok::<_, ()>(()));
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(reads.get(), 0);
        assert_eq!(cursor.row_stats().result_rows, 0);
    }
}

#[test]
fn late_data_and_source_errors_are_terminal_and_release_no_partial_row() {
    let plan = VertexAggregatePlan::compile(&definition()).unwrap();
    let mut input = source(vec![
        row(1, Some(CanonicalScalar::Int(7))),
        row(2, Some(CanonicalScalar::ucs_basic_text("secret").unwrap())),
    ]);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 2
        })))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(dropped.get());
    assert!(!format!("{cursor:?}").contains("secret"));
    assert!(cursor.next().is_none());
    input = source(vec![row(1, Some(CanonicalScalar::Int(7)))]);
    input.fail_at = Some(1);
    let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
            VertexScanError::Source("source failed")
        ))))
    ));
    assert!(cursor.next().is_none());
    for vid in [0, 1] {
        let mut cursor = VertexAggregateCursor::new(
            source(vec![row(1, None), row(vid, None)]),
            plan.clone(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
                VertexScanError::NonIncreasingIdentity
            ))))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn cancellation_at_every_checkpoint_is_terminal_and_close_never_scans() {
    let plan = VertexAggregatePlan::compile(&definition()).unwrap();
    let rows = vec![row(1, Some(CanonicalScalar::Int(7))), row(2, None)];
    let calls = Rc::new(Cell::new(0));
    let count = Rc::clone(&calls);
    let mut baseline = VertexAggregateCursor::new(
        source(rows.clone()),
        plan.clone(),
        wide(),
        move || {
            count.set(count.get() + 1);
            Ok::<_, ()>(())
        },
    );
    baseline.next().unwrap().unwrap();
    for boundary in 0..calls.get() {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut calls = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), move || {
            let at = calls;
            calls += 1;
            if at == boundary { Err(()) } else { Ok(()) }
        });
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(())))));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    let input = source(rows);
    let reads = Rc::clone(&input.reads);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, plan, wide(), || -> Result<(), ()> {
        panic!("close must not poll")
    });
    cursor.close();
    cursor.close();
    assert!(dropped.get());
    assert_eq!(reads.get(), 0);
    assert_eq!(cursor.state(), VertexScanState::Closed);
    assert!(cursor.next().is_none());
}

#[test]
fn unsupported_definitions_refuse_before_source_construction() {
    for function in [
        GraphAggregate::count_distinct("a", 0),
        GraphAggregate::sum_int_distinct("a", 0),
        GraphAggregate::average_int("a", 0),
        GraphAggregate::min("a", 0),
        GraphAggregate::collect("a", 0),
    ] {
        let definition =
            PreparedGraphAggregate::prepare(input(), &[], &[function], 0, None).unwrap();
        assert_eq!(
            VertexAggregatePlan::compile(&definition).unwrap_err(),
            VertexAggregateBuildError::RequiresPlainGlobalCountOrSum
        );
    }
    for (keys, offset, count) in [
        (vec![0], 0, None),
        (vec![], 1, None),
        (vec![], 0, Some(1)),
    ] {
        let definition = PreparedGraphAggregate::prepare(
            input(),
            &keys,
            &[GraphAggregate::count_rows("a")],
            offset,
            count,
        )
        .unwrap();
        assert!(VertexAggregatePlan::compile(&definition).is_err());
    }
}

#[test]
fn numeric_accumulators_check_overflow_and_reject_noninteger_identities() {
    let mut count = NumericState::Count(u64::MAX);
    assert!(matches!(
        count.update::<(), ()>(Input::Identity, 4),
        Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow {
            aggregate: 4
        }))
    ));
    let mut sum = NumericState::Sum(Some(i128::MAX));
    assert!(matches!(
        sum.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(1))), 5),
        Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow {
            aggregate: 5
        }))
    ));
    assert!(matches!(
        sum.update::<(), ()>(Input::Identity, 5),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 5
        }))
    ));
    sum.update::<(), ()>(Input::Scalar(None), 5).unwrap();
    assert!(matches!(sum, NumericState::Sum(Some(i128::MAX))));
}
