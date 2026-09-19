use super::*;
use crate::algebra::{GlaDirection, GraphPatternBuilder, IntegerComparison, PreparedGraphPattern};
use std::cell::Cell;
use std::rc::Rc;

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);

#[derive(Clone)]
struct Sample {
    vid: VId,
    visible: bool,
    labels: Vec<LabelId>,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}
struct Source {
    rows: Vec<Sample>,
    next: usize,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    fail_at: Option<usize>,
    scratch: bool,
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.next) {
            return Err(VertexScanSourceError::Source("source failed"));
        }
        let result = self.rows.get(self.next).map(|row| row.vid);
        if result.is_some() {
            self.next += 1;
        }
        Ok(result)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.scratch {
            control(VertexScanEvent::ScratchEntry).map_err(VertexScanSourceError::Control)?;
        }
        self.reads.set(self.reads.get() + 1);
        let row = &self.rows[self.next - 1];
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
fn source(rows: Vec<Sample>) -> Source {
    Source {
        rows,
        next: 0,
        reads: Rc::new(Cell::new(0)),
        dropped: Rc::new(Cell::new(false)),
        fail_at: None,
        scratch: false,
    }
}
fn sample(vid: u128, value: i64, visible: bool) -> Sample {
    Sample {
        vid: VId(vid),
        visible,
        labels: vec![LABEL],
        properties: vec![(KEY, CanonicalScalar::Int(value))],
    }
}
fn pattern(
    offset: u64,
    count: Option<u64>,
    predicates: &[VertexPredicate],
) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("node").unwrap();
    for predicate in predicates {
        builder.filter("node", predicate.clone()).unwrap();
    }
    builder.prepare("node", offset, count).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[test]
fn pull_results_equal_ordinary_gla_across_visibility_predicates_offsets_limits_and_quantifiers() {
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::Int(3)),
    ];
    let predicates = [
        VertexPredicate::HasLabel(LABEL),
        VertexPredicate::IntegerProperty {
            key: KEY,
            comparison: IntegerComparison::GreaterOrEqual,
            value: 0,
        },
        VertexPredicate::PropertyNull {
            key: KEY,
            is_null: true,
        },
        VertexPredicate::PropertyNull {
            key: KEY,
            is_null: false,
        },
    ];
    for code in 0..256_usize {
        let rows: Vec<_> = [0, 1, 1_u128 << 100, u128::MAX]
            .into_iter()
            .enumerate()
            .map(|(at, vid)| {
                let value = choices[(code >> (at * 2)) & 3].clone();
                Sample {
                    vid: VId(vid),
                    visible: (code + at) % 5 != 0,
                    labels: if (code + at) % 3 == 0 {
                        vec![]
                    } else {
                        vec![LABEL]
                    },
                    properties: value.into_iter().map(|value| (KEY, value)).collect(),
                }
            })
            .collect();
        for predicate in &predicates {
            for offset in [0, 1, 4, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(4)] {
                    for all in [false, true] {
                        let prepared = pattern(offset, count, std::slice::from_ref(predicate));
                        let prepared = if all {
                            prepared.with_duplicates()
                        } else {
                            prepared
                        };
                        let expected = prepared
                            .plan()
                            .execute(
                                rows.iter().filter(|row| row.visible).map(|row| row.vid),
                                [],
                                |vid, tests| {
                                    Ok::<_, ()>(rows.iter().find(|row| row.vid == vid).is_some_and(
                                        |row| {
                                            tests.iter().all(|test| {
                                                test.matches(&row.labels, &row.properties)
                                            })
                                        },
                                    ))
                                },
                            )
                            .unwrap();
                        let mut cursor = VertexScanCursor::new(
                            source(rows.clone()),
                            VertexScanPlan::compile(prepared.plan()).unwrap(),
                            wide(),
                            || Ok::<_, ()>(()),
                        );
                        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(actual, expected);
                        assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                        assert_eq!(cursor.state(), VertexScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn first_pull_close_limit_and_drop_never_visit_the_unconsumed_suffix() {
    let prepared = pattern(0, None, &[]);
    let mut input = source((0..10_000).map(|id| sample(id, 1, true)).collect());
    input.fail_at = Some(1); // A hidden eager read would fail the first pull.
    let reads = Rc::clone(&input.reads);
    let dropped = Rc::clone(&input.dropped);
    let calls = Cell::new(0);
    let mut cursor = VertexScanCursor::new(
        input,
        VertexScanPlan::compile(prepared.plan()).unwrap(),
        wide(),
        || {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(())
        },
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(cursor.snapshot_seq(), CommitSeq(7));
    assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
    assert_eq!(reads.get(), 1);
    assert_eq!(cursor.row_stats().snapshot_records, 1);
    let before = (calls.get(), cursor.row_stats(), cursor.evaluator_stats());
    cursor.close();
    cursor.close();
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    assert_eq!(cursor.state(), VertexScanState::Closed);
    assert_eq!(
        (calls.get(), cursor.row_stats(), cursor.evaluator_stats()),
        before
    );

    for count in [0, 1] {
        let prepared = pattern(0, Some(count), &[]);
        let mut input = source(vec![sample(1, 1, true), sample(2, 1, true)]);
        input.fail_at = Some(count as usize);
        let reads = Rc::clone(&input.reads);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexScanCursor::new(
            input,
            VertexScanPlan::compile(prepared.plan()).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(
            cursor
                .by_ref()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            count as usize
        );
        assert_eq!(reads.get(), count as usize);
        assert!(dropped.get());
    }
    let input = source(vec![sample(1, 1, true)]);
    let dropped = Rc::clone(&input.dropped);
    let cursor = VertexScanCursor::new(
        input,
        VertexScanPlan::compile(prepared.plan()).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    drop(cursor);
    assert!(dropped.get());
}

#[test]
fn every_interruption_boundary_is_terminal_releases_the_source_and_retries_from_a_fresh_cursor() {
    let rows = vec![
        sample(0, 3, false),
        sample(1, -1, true),
        sample(2, 4, true),
        sample(3, 8, true),
    ];
    let prepared = pattern(
        0,
        None,
        &[VertexPredicate::IntegerProperty {
            key: KEY,
            comparison: IntegerComparison::Greater,
            value: 0,
        }],
    );
    let scan = VertexScanPlan::compile(prepared.plan()).unwrap();
    let mut total = 0;
    let expected = VertexScanCursor::new(source(rows.clone()), scan.clone(), wide(), || {
        total += 1;
        Ok::<_, usize>(())
    })
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(expected, vec![VId(2), VId(3)]);
    for stop in 1..=total {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let calls = Cell::new(0);
        let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || {
            let at = calls.get() + 1;
            calls.set(at);
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(vid) => prefix.push(vid),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(other) => panic!("wrong refusal: {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(calls.get(), stop);
        assert_eq!(
            VertexScanCursor::new(source(rows.clone()), scan.clone(), wide(), || Ok::<_, ()>(
                ()
            ))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
            expected
        );
    }
}

#[test]
fn budgets_are_cumulative_and_exact_over_skipped_invisible_and_returned_rows() {
    let rows = vec![
        sample(0, 0, false),
        sample(1, 1, true),
        sample(2, 2, true),
        sample(3, 3, true),
    ];
    let scan = VertexScanPlan::compile(pattern(1, None, &[]).plan()).unwrap();
    let mut input = source(rows.clone());
    input.scratch = true;
    let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || Ok::<_, ()>(()));
    assert_eq!(cursor.next().unwrap().unwrap(), VId(2));
    assert_eq!(
        cursor.row_stats(),
        GqlExecutionStats {
            snapshot_records: 3,
            result_rows: 1
        }
    );
    assert_eq!(cursor.next().unwrap().unwrap(), VId(3));
    assert!(cursor.next().is_none());
    let stats = cursor.evaluator_stats();
    let exact = GqlQueryPolicy::new(4, 2, stats.work_units, stats.scratch_entries);
    let run = |policy| {
        let mut input = source(rows.clone());
        input.scratch = true;
        VertexScanCursor::new(input, scan.clone(), policy, || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
    };
    assert_eq!(run(exact).unwrap(), vec![VId(2), VId(3)]);
    assert!(matches!(
        run(GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX)),
        Err(GqlQueryError::Rows(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX)),
        Err(GqlQueryError::Rows(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(4, 2, stats.work_units - 1, u64::MAX)),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(
            4,
            2,
            u64::MAX,
            stats.scratch_entries - 1
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    // Candidate admission happens before resolving even an invisible row.
    let input = source(rows);
    let reads = Rc::clone(&input.reads);
    let mut cursor =
        VertexScanCursor::new(input, scan, GqlQueryPolicy::new(0, 2, 100, 100), || {
            Ok::<_, ()>(())
        });
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
    assert_eq!(reads.get(), 0);
    assert!(cursor.next().is_none());
}

#[test]
fn source_errors_and_nonincreasing_identities_are_not_null_rows_or_silent_eof() {
    let scan = VertexScanPlan::compile(pattern(0, None, &[]).plan()).unwrap();
    let mut input = source(vec![sample(1, 1, true), sample(2, 1, true)]);
    input.fail_at = Some(1);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || Ok::<_, ()>(()));
    assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Source(
            "source failed"
        ))))
    ));
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    for second in [1, 0] {
        let mut cursor = VertexScanCursor::new(
            source(vec![sample(1, 1, true), sample(second, 1, false)]),
            scan.clone(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                VertexScanError::NonIncreasingIdentity
            )))
        ));
        assert!(cursor.next().is_none());
    }
}

#[test]
fn compiler_rejects_expansion_instead_of_eager_fallback_and_preserves_identity_tests() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap();
    builder.vertex("b").unwrap();
    builder
        .edge(
            "a",
            fgdb_delta_types::RelationId(1),
            GlaDirection::Forward,
            "b",
        )
        .unwrap();
    assert!(VertexScanPlan::compile(builder.prepare("a", 0, None).unwrap().plan()).is_err());
    let mut node = GraphPatternBuilder::new();
    node.vertex("n").unwrap();
    node.identity("n", "n", false).unwrap();
    let scan = VertexScanPlan::compile(node.prepare("n", 0, None).unwrap().plan()).unwrap();
    assert!(
        VertexScanCursor::new(
            source(vec![sample(1, 1, true)]),
            scan,
            wide(),
            || Ok::<_, ()>(())
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .is_empty()
    );
}

#[test]
fn counters_refuse_before_wrap_and_diagnostics_redact_fields() {
    let scan = VertexScanPlan::compile(pattern(0, None, &[]).plan()).unwrap();
    for records in [false, true] {
        let mut policy = wide();
        policy.rows = crate::GqlExecutionBudget::UNLIMITED;
        let mut cursor = VertexScanCursor::new(
            source(vec![sample(12345, 98765, true)]),
            scan.clone(),
            policy,
            || Ok::<_, ()>(()),
        );
        if records {
            cursor.meter.rows.snapshot_records = u64::MAX;
        } else {
            cursor.meter.rows.result_rows = u64::MAX;
        }
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                VertexScanError::CounterExhausted
            )))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
        let diagnostic = format!("{cursor:?} {scan:?}");
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(!diagnostic.contains("12345"));
        assert!(!diagnostic.contains("98765"));
    }
}

fn value_pattern(
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern<crate::algebra::GraphValueRow> {
    use crate::algebra::GraphColumn;
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("value", "n", KEY),
                GraphColumn::vertex("same_id", "n"),
                GraphColumn::property("missing", "n", PropertyKeyId(999)),
                GraphColumn::property("same_value", "n", KEY),
            ],
            offset,
            count,
        )
        .unwrap()
}

#[test]
fn property_rows_use_ordinary_projection_with_exact_nulls_types_order_and_duplicates() {
    let text = CanonicalScalar::ucs_basic_text(&"a\0é".repeat(100)).unwrap();
    let mut rows = vec![
        sample(0, 9, true),
        sample(1, -7, false),
        sample(2, -100, true),
        sample(1_u128 << 100, 4, true),
        sample(u128::MAX, 0, true),
    ];
    rows[0].properties = vec![(KEY, text)];
    rows[2].properties.clear();
    rows[3].properties = vec![(KEY, CanonicalScalar::Null)];
    for offset in [0, 1, 4, u64::MAX] {
        for count in [None, Some(0), Some(1), Some(4)] {
            for all in [false, true] {
                let prepared = value_pattern(offset, count);
                let prepared = if all {
                    prepared.with_duplicates()
                } else {
                    prepared
                };
                let expected = prepared
                    .plan()
                    .execute_governed_with_properties(
                        rows.len() as u64,
                        rows.iter().filter(|row| row.visible).map(|row| row.vid),
                        [],
                        |vid, tests| {
                            Ok::<_, ()>(rows.iter().find(|row| row.vid == vid).is_some_and(|row| {
                                tests
                                    .iter()
                                    .all(|test| test.matches(&row.labels, &row.properties))
                            }))
                        },
                        |vid, key| {
                            Ok(rows
                                .iter()
                                .find(|row| row.vid == vid)
                                .and_then(|row| {
                                    row.properties.iter().find(|(actual, _)| *actual == key)
                                })
                                .map(|(_, value)| value))
                        },
                        wide(),
                        || Ok::<_, ()>(()),
                    )
                    .unwrap()
                    .value;
                let mut cursor = VertexScanCursor::new(
                    source(rows.clone()),
                    VertexScanPlan::compile(prepared.plan()).unwrap(),
                    wide(),
                    || Ok::<_, ()>(()),
                );
                let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(actual, expected);
                for row in actual {
                    assert_eq!(row.get(0), row.get(2));
                    assert_eq!(row.get(1), row.get(4));
                    assert_eq!(
                        row.get(3).and_then(|value| value.as_scalar()),
                        Some(&CanonicalScalar::Null)
                    );
                }
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
            }
        }
    }
}

#[test]
fn every_property_lookup_and_payload_copy_refusal_keeps_only_complete_delivered_rows() {
    let mut rows = vec![sample(1, 8, true), sample(2, 9, true)];
    rows[1].properties = vec![(
        KEY,
        CanonicalScalar::ucs_basic_text(&"payload".repeat(512)).unwrap(),
    )];
    let scan = VertexScanPlan::compile(value_pattern(0, None).plan()).unwrap();
    let mut total = 0;
    let mut baseline = VertexScanCursor::new(source(rows.clone()), scan.clone(), wide(), || {
        total += 1;
        Ok::<_, usize>(())
    });
    let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let stats = baseline.evaluator_stats();
    drop(baseline);
    assert!(
        stats.scratch_entries > 100,
        "large canonical payloads must be charged"
    );
    for stop in 1..=total {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let calls = Cell::new(0);
        let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || {
            let at = calls.get() + 1;
            calls.set(at);
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next().expect("chosen boundary must be reached") {
                Ok(row) => {
                    assert_eq!(row.len(), 5);
                    prefix.push(row);
                }
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(error) => panic!("unexpected refusal: {error:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        assert_eq!(calls.get(), stop);
    }
    let exact = GqlQueryPolicy::new(2, 2, stats.work_units, stats.scratch_entries);
    let run = |policy| {
        VertexScanCursor::new(source(rows.clone()), scan.clone(), policy, || {
            Ok::<_, ()>(())
        })
        .collect::<Result<Vec<_>, _>>()
    };
    assert_eq!(run(exact).unwrap(), expected);
    assert!(matches!(
        run(GqlQueryPolicy::new(
            2,
            2,
            u64::MAX,
            stats.scratch_entries - 1
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(2, 2, stats.work_units - 1, u64::MAX)),
        Err(GqlQueryError::Evaluator(_))
    ));
}

#[test]
fn property_only_or_property_leading_output_refuses_instead_of_reordering_or_losing_distinctness() {
    use crate::algebra::GraphColumn;
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    for columns in [
        vec![GraphColumn::property("value", "n", KEY)],
        vec![
            GraphColumn::property("value", "n", KEY),
            GraphColumn::vertex("id", "n"),
        ],
    ] {
        let prepared = builder.prepare_values(&columns, 0, None).unwrap();
        assert!(VertexScanPlan::compile(prepared.plan()).is_err());
        assert!(VertexScanPlan::compile(prepared.with_duplicates().plan()).is_err());
    }
}

#[test]
fn skipped_and_output_refused_property_rows_do_not_copy_payloads_or_reset_the_budget() {
    let mut rows = vec![sample(1, 1, true), sample(2, 2, true), sample(3, 3, true)];
    for row in &mut rows {
        row.properties = vec![(
            KEY,
            CanonicalScalar::ucs_basic_text(&"large".repeat(1024)).unwrap(),
        )];
    }
    // Skipping all matching rows needs neither property projection nor scratch.
    let skip = VertexScanPlan::compile(value_pattern(3, None).plan()).unwrap();
    let mut cursor = VertexScanCursor::new(
        source(rows.clone()),
        skip,
        GqlQueryPolicy::new(3, 0, 1000, 0),
        || Ok::<_, ()>(()),
    );
    assert!(cursor.next().is_none());
    assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
    // A result-row refusal is checked before any scalar payload allocation.
    let scan = VertexScanPlan::compile(value_pattern(0, None).plan()).unwrap();
    let mut cursor = VertexScanCursor::new(
        source(rows.clone()),
        scan.clone(),
        GqlQueryPolicy::new(3, 0, 1000, 0),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
    assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
    let mut cursor = VertexScanCursor::new(
        source(rows),
        scan,
        GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    let first = cursor.next().unwrap().unwrap();
    let scratch = cursor.evaluator_stats().scratch_entries;
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
    assert_eq!(cursor.evaluator_stats().scratch_entries, scratch);
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert_eq!(first.len(), 5);
    assert!(cursor.next().is_none());
}
