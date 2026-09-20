use super::*;
use crate::GraphAggregate;
use crate::algebra::{GraphColumn, GraphPatternBuilder};
use std::cell::Cell;
use std::rc::Rc;

mod distinct_tests;

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
    builder
        .filter("n", VertexPredicate::HasLabel(LABEL))
        .unwrap();
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
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
            Ok::<_, ()>(())
        });
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
        run(GqlQueryPolicy::new(
            3,
            1,
            stats.work_units,
            stats.scratch_entries
        ))
        .unwrap(),
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
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { aggregate: 2 }
        )))
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
    let mut baseline =
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), move || {
            count.set(count.get() + 1);
            Ok::<_, ()>(())
        });
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
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Interrupted(())))
        ));
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
        GraphAggregate::collect("a", 0),
        GraphAggregate::collect_distinct("a", 0),
    ] {
        let definition =
            PreparedGraphAggregate::prepare(input(), &[], &[function], 0, None).unwrap();
        assert_eq!(
            VertexAggregatePlan::compile(&definition).unwrap_err(),
            VertexAggregateBuildError::RequiresPlainGlobalAggregate
        );
    }
    for (keys, offset, count) in [
        (vec![0], 0, Some(1)),
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
        Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate: 4 }
        ))
    ));
    let mut sum = NumericState::Sum(Some(i128::MAX));
    assert!(matches!(
        sum.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(1))), 5),
        Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate: 5 }
        ))
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

const BUCKET: PropertyKeyId = PropertyKeyId(8);
fn grouped_definition(keys: &[usize]) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .filter("n", VertexPredicate::HasLabel(LABEL))
        .unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("bucket", "n", BUCKET),
                GraphColumn::property("value", "n", KEY),
                GraphColumn::vertex("id", "n"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        keys,
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::count("identities", 2),
            GraphAggregate::min("minimum", 1),
            GraphAggregate::max("maximum", 1),
            GraphAggregate::min("first_id", 2),
            GraphAggregate::max("last_id", 2),
            GraphAggregate::min("first_bucket", 0),
            GraphAggregate::max("last_bucket", 0),
        ],
        0,
        None,
    )
    .unwrap()
}
fn grouped_row(vid: u128, bucket: Option<CanonicalScalar>, value: Option<i64>) -> Row {
    let mut result = row(vid, value.map(CanonicalScalar::Int));
    result
        .properties
        .extend(bucket.map(|value| (BUCKET, value)));
    result
}
fn expected_groups(definition: &PreparedGraphAggregate, rows: &[Row]) -> Vec<GraphAggregateRow> {
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
                            .all(|p| p.matches(&row.labels, &row.properties))
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
}

#[test]
fn grouped_count_sum_and_exact_average_match_batch_for_4096_unsorted_inputs() {
    let definition = grouped_definition(&[0]);
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::ucs_basic_text("same").unwrap()),
    ];
    for mut code in 0..4096_usize {
        let mut rows = Vec::new();
        for (at, vid) in [0, 1, 1_u128 << 100, u128::MAX].into_iter().enumerate() {
            let bucket = choices[code % 4].clone();
            code /= 4;
            let value = [None, Some(i64::MIN), Some(i64::MAX), Some(3)][(code + at) % 4];
            let mut row = grouped_row(vid, bucket, value);
            if code % 13 == 0 {
                row.visible = false;
            }
            if code % 17 == 0 {
                row.labels.clear();
            }
            rows.push(row);
        }
        let expected = expected_groups(&definition, &rows);
        let input = source(rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor =
            VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        assert_eq!(cursor.key_columns(), &["bucket"]);
        assert_eq!(
            cursor.columns(),
            &[
                "rows",
                "nonnull",
                "sum",
                "average",
                "identities",
                "minimum",
                "maximum",
                "first_id",
                "last_id",
                "first_bucket",
                "last_bucket"
            ]
        );
        assert_eq!(cursor.size_hint(), (0, None));
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected);
        assert!(dropped.get());
        assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert_eq!(cursor.size_hint(), (0, Some(0)));
    }
}

#[test]
fn owned_composite_keys_preserve_null_domains_wide_identities_and_release_source_before_delivery() {
    let buckets = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(7)),
        Some(CanonicalScalar::ucs_basic_text("7").unwrap()),
        Some(CanonicalScalar::bytes(vec![7; 8192]).unwrap()),
    ];
    let rows: Vec<_> = buckets
        .into_iter()
        .enumerate()
        .map(|(at, bucket)| {
            grouped_row(
                if at == 4 { u128::MAX } else { at as u128 },
                bucket,
                Some(at as i64),
            )
        })
        .collect();
    for keys in [&[0][..], &[0, 2][..], &[2, 0][..]] {
        let definition = grouped_definition(keys);
        let expected = expected_groups(&definition, &rows);
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexAggregateCursor::new(
            input,
            VertexAggregatePlan::compile(&definition).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        let first = cursor.next().unwrap().unwrap();
        assert!(dropped.get());
        assert_eq!(first, expected[0]);
        assert_eq!(cursor.size_hint(), (0, Some(expected.len() - 1)));
        assert_eq!(cursor.pending.as_ref().unwrap().len(), expected.len() - 1);
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected[1..]
        );
        assert!(cursor.pending.is_none());
    }
    let definition = grouped_definition(&[0]);
    let mut cursor = VertexAggregateCursor::new(
        source(vec![]),
        VertexAggregatePlan::compile(&definition).unwrap(),
        GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().is_none());
    assert_eq!(cursor.row_stats().result_rows, 0);
    let definition = grouped_definition(&[]);
    let mut cursor = VertexAggregateCursor::new(
        source(vec![]),
        VertexAggregatePlan::compile(&definition).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    let row = cursor.next().unwrap().unwrap();
    assert_eq!(row.values()[0].as_count(), Some(0));
    assert!(row.values()[2].is_null() && row.values()[3].is_null());
}

#[test]
fn grouped_source_data_and_final_group_budget_refusals_emit_no_partial_groups() {
    let definition = grouped_definition(&[0]);
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let rows = vec![
        grouped_row(1, Some(CanonicalScalar::Int(2)), Some(5)),
        grouped_row(2, Some(CanonicalScalar::Int(1)), Some(9)),
    ];
    let mut input = source(rows.clone());
    input.fail_at = Some(2);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
            VertexScanError::Source("source failed")
        ))))
    ));
    assert!(dropped.get());
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.pending.is_none());
    let mut bad = rows.clone();
    bad[1].properties[0].1 = CanonicalScalar::ucs_basic_text("bad").unwrap();
    let mut cursor =
        VertexAggregateCursor::new(source(bad), plan.clone(), wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { aggregate: 2 }
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());
    for bound in [0, 1] {
        let mut cursor = VertexAggregateCursor::new(
            source(rows.clone()),
            plan.clone(),
            GqlQueryPolicy::new(2, bound, u64::MAX, u64::MAX),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    }
    let mut cursor = VertexAggregateCursor::new(
        source(rows.clone()),
        plan,
        GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    assert_eq!(
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected_groups(&definition, &rows)
    );
}

#[test]
fn grouped_cancellation_at_every_scan_and_delivery_checkpoint_fuses_and_drops_all_retained_state() {
    let definition = grouped_definition(&[0]);
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let rows = vec![
        grouped_row(1, Some(CanonicalScalar::Int(2)), Some(3)),
        grouped_row(2, None, Some(-2)),
        grouped_row(3, Some(CanonicalScalar::Int(1)), None),
    ];
    let expected = expected_groups(&definition, &rows);
    let calls = Rc::new(Cell::new(0));
    let counting = Rc::clone(&calls);
    let mut baseline =
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), move || {
            counting.set(counting.get() + 1);
            Ok::<_, usize>(())
        });
    assert_eq!(
        baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        expected
    );
    let stats = baseline.evaluator_stats();
    for stop in 1..=calls.get() {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut at = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), move || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(actual))) => {
                    assert_eq!(actual, stop);
                    break;
                }
                other => panic!("expected interruption, got {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.pending.is_none());
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    for (work, scratch, passes) in [
        (stats.work_units, stats.scratch_entries, true),
        (stats.work_units - 1, stats.scratch_entries, false),
        (stats.work_units, stats.scratch_entries - 1, false),
    ] {
        let mut cursor = VertexAggregateCursor::new(
            source(rows.clone()),
            plan.clone(),
            GqlQueryPolicy::new(3, 3, work, scratch),
            || Ok::<_, ()>(()),
        );
        let result = cursor.by_ref().collect::<Result<Vec<_>, _>>();
        assert_eq!(result.is_ok(), passes);
        assert!(cursor.pending.is_none());
    }
    let mut cursor = VertexAggregateCursor::new(source(rows), plan, wide(), || Ok::<_, ()>(()));
    cursor.next().unwrap().unwrap();
    assert!(cursor.pending.is_some());
    cursor.close();
    cursor.close();
    assert!(cursor.pending.is_none());
    assert!(cursor.next().is_none());
    assert_eq!(cursor.state(), VertexScanState::Closed);
}

#[test]
fn averages_keep_exact_ratios_and_check_both_overflow_domains_before_mutation() {
    for (sum, count) in [(i128::MAX, 1), (0, u64::MAX)] {
        let mut state = NumericState::Average { sum, count };
        assert!(matches!(
            state.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(1))), 9),
            Err(GqlQueryError::Source(
                GraphAggregateError::ArithmeticOverflow { aggregate: 9 }
            ))
        ));
        assert!(
            matches!(state, NumericState::Average { sum: a, count: b } if a == sum && b == count)
        );
        assert!(matches!(
            state.update::<(), ()>(Input::Identity, 9),
            Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerAverage { aggregate: 9 }
            ))
        ));
    }
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(&[GraphColumn::property("value", "n", KEY)], 0, None)
        .unwrap()
        .with_duplicates();
    let definition = PreparedGraphAggregate::prepare(
        input,
        &[],
        &[GraphAggregate::average_int("a", 0)],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    for values in [
        vec![],
        vec![None],
        vec![Some(-2), Some(1)],
        vec![Some(i64::MAX), Some(i64::MIN)],
    ] {
        let rows: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(at, v)| row(at as u128, v.map(CanonicalScalar::Int)))
            .collect();
        let mut cursor =
            VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
                Ok::<_, ()>(())
            });
        assert_eq!(
            cursor.next().unwrap().unwrap(),
            expected(&definition, &rows)
        );
    }
    let mut cursor = VertexAggregateCursor::new(
        source(vec![row(
            0,
            Some(CanonicalScalar::ucs_basic_text("bad").unwrap()),
        )]),
        plan,
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerAverage { aggregate: 0 }
        )))
    ));
}

fn statistics() -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::average_int("avg", 0),
            GraphAggregate::min("min", 0),
            GraphAggregate::max("max", 0),
        ],
        0,
        None,
    )
    .unwrap()
}

#[test]
fn statistics_match_the_ordinary_engine_across_nulls_visibility_and_integer_extremes() {
    let definition = statistics();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(i64::MIN)),
        Some(CanonicalScalar::Int(i64::MAX)),
        Some(CanonicalScalar::Int(0)),
    ];
    for code in 0..625_usize {
        let mut digits = code;
        let rows: Vec<_> = [0, 1, 1_u128 << 100, u128::MAX]
            .into_iter()
            .enumerate()
            .map(|(at, vid)| {
                let mut row = row(vid, choices[digits % choices.len()].clone());
                digits /= choices.len();
                row.visible = (code + at) % 7 != 0;
                if (code + at) % 5 == 0 {
                    row.labels.clear();
                }
                row
            })
            .collect();
        let oracle = expected(&definition, &rows);
        let input = source(rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor =
            VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        assert_eq!(cursor.next().unwrap().unwrap(), oracle);
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    let mut empty = VertexAggregateCursor::new(source(vec![]), plan, wide(), || Ok::<_, ()>(()));
    let result = empty.next().unwrap().unwrap();
    assert_eq!(result, expected(&definition, &[]));
    assert_eq!(result.values()[0].as_count(), Some(0));
    assert!(
        result.values()[1..]
            .iter()
            .all(GraphAggregateValue::is_null)
    );
}

#[test]
fn averages_retain_exact_fractions_and_identity_extrema_retain_all_128_bits() {
    let definition = statistics();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    for (a, b, numerator, denominator) in [
        (i64::MAX, i64::MAX - 1, 2 * i128::from(i64::MAX) - 1, 2),
        (i64::MIN, i64::MIN + 1, 2 * i128::from(i64::MIN) + 1, 2),
        (-7, 3, -2, 1),
        (0, 0, 0, 1),
    ] {
        let mut cursor = VertexAggregateCursor::new(
            source(vec![
                row(0, Some(CanonicalScalar::Int(a))),
                row(u128::MAX, Some(CanonicalScalar::Int(b))),
            ]),
            plan.clone(),
            wide(),
            || Ok::<_, ()>(()),
        );
        let result = cursor.next().unwrap().unwrap();
        let average = result.values()[2].as_average().unwrap();
        assert_eq!(average.numerator(), numerator);
        assert_eq!(average.denominator(), denominator);
        assert!(result.values()[2].as_integer().is_none());
    }
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let vertices = builder
        .prepare_values(&[GraphColumn::vertex("id", "n")], 0, None)
        .unwrap()
        .with_duplicates();
    let definition = PreparedGraphAggregate::prepare(
        vertices.clone(),
        &[],
        &[GraphAggregate::min("min", 0), GraphAggregate::max("max", 0)],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let rows = vec![row(0, None), row(1_u128 << 100, None), row(u128::MAX, None)];
    let mut cursor =
        VertexAggregateCursor::new(source(rows.clone()), plan, wide(), || Ok::<_, ()>(()));
    let result = cursor.next().unwrap().unwrap();
    assert_eq!(result, expected(&definition, &rows));
    assert_eq!(
        result.values()[0].as_value(),
        Some(&GraphValue::Vertex(VId(0)))
    );
    assert_eq!(
        result.values()[1].as_value(),
        Some(&GraphValue::Vertex(VId(u128::MAX)))
    );

    // Numeric domain errors are evaluated, not guessed from static column
    // types. Empty input is NULL; the first nonnull vertex must fail typed.
    for average in [false, true] {
        let spec = if average {
            GraphAggregate::average_int("n", 0)
        } else {
            GraphAggregate::sum_int("n", 0)
        };
        let definition =
            PreparedGraphAggregate::prepare(vertices.clone(), &[], &[spec], 0, None).unwrap();
        let plan = VertexAggregatePlan::compile(&definition).unwrap();
        let mut empty =
            VertexAggregateCursor::new(source(vec![]), plan.clone(), wide(), || Ok::<_, ()>(()));
        let result = empty.next().unwrap().unwrap();
        assert!(result.values()[0].is_null());
        assert_eq!(result, expected(&definition, &[]));
        let mut nonempty = VertexAggregateCursor::new(
            source(vec![row(1, None)]),
            plan,
            wide(),
            || Ok::<_, ()>(()),
        );
        let error = nonempty.next().unwrap().unwrap_err();
        assert!(match error {
            GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate: 0 }) =>
                average,
            GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 0 }) => !average,
            _ => false,
        });
        assert_eq!(nonempty.row_stats().result_rows, 0);
        assert!(nonempty.next().is_none());
    }
}

#[test]
fn extrema_preserve_canonical_scalar_order_without_retaining_an_input_bag() {
    let definition = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::min("min", 0), GraphAggregate::max("max", 0)],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Bool(false)),
        Some(CanonicalScalar::Int(-17)),
        Some(CanonicalScalar::ucs_basic_text(&"é\0".repeat(1024)).unwrap()),
        Some(CanonicalScalar::bytes(vec![0xff; 4096]).unwrap()),
    ];
    for a in &choices {
        for b in &choices {
            for c in &choices {
                let rows = vec![row(0, a.clone()), row(1, b.clone()), row(2, c.clone())];
                let oracle = expected(&definition, &rows);
                let mut cursor =
                    VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || {
                        Ok::<_, ()>(())
                    });
                assert_eq!(cursor.next().unwrap().unwrap(), oracle);
                assert!(cursor.next().is_none());
            }
        }
    }
    // Equal operands pay comparison work but cause no replacement ownership.
    // This assertion is about operator scratch, not the fixture/source's RAM.
    let value = CanonicalScalar::ucs_basic_text(&"private".repeat(512)).unwrap();
    let mut scratch = None;
    let mut work = 0;
    for count in [1, 2, 100] {
        let rows = (0..count).map(|id| row(id, Some(value.clone()))).collect();
        let mut cursor =
            VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || Ok::<_, ()>(()));
        cursor.next().unwrap().unwrap();
        let stats = cursor.evaluator_stats();
        assert_eq!(
            *scratch.get_or_insert(stats.scratch_entries),
            stats.scratch_entries
        );
        assert!(stats.work_units > work);
        work = stats.work_units;
        assert!(!format!("{cursor:?} {plan:?}").contains("private"));
    }
}

#[test]
fn extrema_payload_admission_and_every_checkpoint_refusal_are_atomic() {
    let definition = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::min("min", 0), GraphAggregate::max("max", 0)],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let rows = ["mmmm", "aaaa", "zzzz"]
        .into_iter()
        .enumerate()
        .map(|(id, text)| {
            row(
                id as u128,
                Some(CanonicalScalar::ucs_basic_text(&text.repeat(512)).unwrap()),
            )
        })
        .collect::<Vec<_>>();
    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);
    let mut baseline =
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), move || {
            observed.set(observed.get() + 1);
            Ok::<_, ()>(())
        });
    let result = baseline.next().unwrap().unwrap();
    assert_eq!(result, expected(&definition, &rows));
    let stats = baseline.evaluator_stats();
    let mut exact = VertexAggregateCursor::new(
        source(rows.clone()),
        plan.clone(),
        GqlQueryPolicy::new(3, 1, stats.work_units, stats.scratch_entries),
        || Ok::<_, ()>(()),
    );
    assert_eq!(exact.next().unwrap().unwrap(), result);
    for policy in [
        GqlQueryPolicy::new(3, 1, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(3, 1, u64::MAX, stats.scratch_entries - 1),
    ] {
        let mut cursor =
            VertexAggregateCursor::new(source(rows.clone()), plan.clone(), policy, || {
                Ok::<_, ()>(())
            });
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Evaluator(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
    }
    for stop in 0..calls.get() {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut at = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), move || {
            let here = at;
            at += 1;
            if here == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(at))) if at == stop));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    // Refusing ANY charge for a winning replacement leaves the prior value
    // and its size untouched; copying takes place only after the last charge.
    let old = CanonicalScalar::ucs_basic_text("a").unwrap();
    let new = CanonicalScalar::ucs_basic_text(&"z".repeat(8192)).unwrap();
    let old_units = Input::Scalar(Some(&old)).payload_units();
    let new_units = Input::Scalar(Some(&new)).payload_units();
    let events = old_units + new_units + 1 + new_units;
    for stop in 0..events {
        let mut state = NumericState::Extreme {
            value: Some(GraphValue::Scalar(old.clone())),
            maximum: true,
            payload_units: old_units,
        };
        let mut at = 0;
        let error = state.update_governed::<(), usize>(Input::Scalar(Some(&new)), 0, &mut |_| {
            let here = at;
            at += 1;
            if here == stop {
                Err(GqlQueryError::Interrupted(stop))
            } else {
                Ok(())
            }
        });
        assert!(matches!(error, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert!(matches!(state, NumericState::Extreme {
            value: Some(GraphValue::Scalar(ref retained)), payload_units, ..
        } if *retained == old && payload_units == old_units));
    }
}

#[test]
fn average_overflow_is_atomic_and_scratch_is_independent_of_cardinality() {
    for (sum, count) in [(0, u64::MAX), (i128::MAX, 1), (i128::MIN, 1)] {
        let mut state = NumericState::Average { sum, count };
        let value = CanonicalScalar::Int(if sum == i128::MIN { -1 } else { 1 });
        assert!(matches!(
            state.update::<(), ()>(Input::Scalar(Some(&value)), 7),
            Err(GqlQueryError::Source(
                GraphAggregateError::ArithmeticOverflow { aggregate: 7 }
            ))
        ));
        assert!(
            matches!(state, NumericState::Average { sum: s, count: c } if s == sum && c == count)
        );
        state.update::<(), ()>(Input::Scalar(None), 7).unwrap();
        state
            .update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Null)), 7)
            .unwrap();
        assert!(
            matches!(state, NumericState::Average { sum: s, count: c } if s == sum && c == count)
        );
    }
    let definition = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::average_int("avg", 0)],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let mut scratch = None;
    for count in [0, 1, 100, 10_000] {
        let rows = (0..count)
            .map(|vid| row(vid, Some(CanonicalScalar::Int(-7))))
            .collect();
        let mut cursor =
            VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || Ok::<_, ()>(()));
        let result = cursor.next().unwrap().unwrap();
        if count == 0 {
            assert!(result.values()[0].is_null());
        } else {
            let avg = result.values()[0].as_average().unwrap();
            assert_eq!((avg.numerator(), avg.denominator()), (-7, 1));
        }
        let stats = cursor.evaluator_stats();
        assert_eq!(
            *scratch.get_or_insert(stats.scratch_entries),
            stats.scratch_entries
        );
    }
    let bad = source(vec![
        row(0, Some(CanonicalScalar::Int(5))),
        row(1, Some(CanonicalScalar::Bool(true))),
    ]);
    let dropped = Rc::clone(&bad.dropped);
    let mut cursor = VertexAggregateCursor::new(bad, plan, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerAverage { aggregate: 0 }
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(dropped.get());
    assert!(cursor.next().is_none());
}
