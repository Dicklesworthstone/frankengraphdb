use super::*;
use fgdb_delta_types::LimbLimit;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
use fgdb_gql::{GraphAggregate, GraphExactAverage};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::rc::Rc;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000)
}
fn row(value: GraphValue) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![value])
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, n)| (row, ZWeight::from_i128(n))),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn layout(width: usize) -> Arc<Layout> {
    Arc::new(Layout::Rows {
        columns: (0..width).map(|i| format!("c{i}")).collect(),
    })
}
fn make<'a>(
    rows: &'a ZSet<GraphValueRow>,
    order: Option<&'a [Arc<GraphValueRow>]>,
    width: usize,
    policy: GqlQueryPolicy,
) -> Pull<'a> {
    Pull::new(
        view_runs(rows, order, NativeRow::Values),
        layout(width),
        CommitSeq(7),
        policy,
        StandingQueryStats {
            work_units: 1,
            scratch_entries: 1,
            ..StandingQueryStats::default()
        },
    )
}
fn drain(pull: &mut Pull<'_>) -> Result<Vec<Vec<QueryValue>>, StandingQueryFailure> {
    let mut rows = Vec::new();
    while let Some(row) = pull.next_checked(&mut || Ok(())) {
        rows.push(row?);
    }
    Ok(rows)
}
fn cells(row: &GraphValueRow) -> Vec<QueryValue> {
    row.values()
        .iter()
        .cloned()
        .map(QueryValue::Value)
        .collect()
}

#[test]
fn all_small_native_bags_and_explicit_sequences_match_eager_delivery() {
    let values = [
        GraphValue::Scalar(CanonicalScalar::Null),
        int(-3),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::List(vec![int(4), int(1)].into_boxed_slice()),
    ];
    for code in 0..81_usize {
        let rows = bag(values.iter().enumerate().map(|(i, value)| {
            (
                row(value.clone()),
                ((code / 3usize.pow(i as u32)) % 3) as i128,
            )
        }));
        let before = rows
            .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let expected: Vec<_> = rows
            .iter()
            .flat_map(|(row, weight)| {
                std::iter::repeat_n(cells(row), weight.to_i128().unwrap() as usize)
            })
            .collect();
        let mut pull = make(&rows, None, 1, policy());
        assert_eq!(drain(&mut pull).unwrap(), expected);
        assert_eq!(pull.delivered, expected.len() as u64);
        assert_eq!(pull.state, VertexScanState::Exhausted);
        let end_stats = pull.stats;
        assert!(
            pull.next_checked(&mut || panic!("fused cursor invoked control"))
                .is_none()
        );
        pull.close();
        assert_eq!(pull.state, VertexScanState::Exhausted);
        assert_eq!(pull.stats, end_stats);
        let sequence: Vec<_> = rows
            .iter()
            .rev()
            .flat_map(|(row, weight)| {
                std::iter::repeat_n(Arc::new(row.clone()), weight.to_i128().unwrap() as usize)
            })
            .collect();
        let expected: Vec<_> = sequence.iter().map(|row| cells(row)).collect();
        let mut ordered = make(&rows, Some(&sequence), 1, policy());
        assert_eq!(drain(&mut ordered).unwrap(), expected);
        assert_eq!(rows, before);
    }
    let zero = bag([(GraphValueRow::from_owned_values(vec![]), 2)]);
    assert_eq!(
        drain(&mut make(&zero, None, 0, policy())).unwrap(),
        vec![vec![], vec![]]
    );
}

struct Tracked<'a> {
    runs: std::vec::IntoIter<Run<'a>>,
    visits: Rc<Cell<usize>>,
    drops: Rc<Cell<usize>>,
}
impl<'a> Iterator for Tracked<'a> {
    type Item = Run<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.visits.set(self.visits.get() + 1);
        self.runs.next()
    }
}
impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn promoted_run_support_is_not_narrowed_and_close_never_drains() {
    let r = row(GraphValue::Vertex(VId(u128::MAX)));
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::from_i128(i128::MAX), LimbLimit::new(4))
        .unwrap();
    assert_eq!(huge.to_i128(), None);
    let visits = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let iterator = Tracked {
        runs: vec![Run {
            row: NativeRow::Values(&r),
            weight: Some(&huge),
        }]
        .into_iter(),
        visits: Rc::clone(&visits),
        drops: Rc::clone(&drops),
    };
    let mut pull = Pull::new(
        Box::new(iterator),
        layout(1),
        CommitSeq(9),
        GqlQueryPolicy::new(0, 3, 1000, 1000),
        StandingQueryStats::default(),
    );
    assert_eq!(visits.get(), 0);
    for _ in 0..3 {
        assert_eq!(
            pull.next_checked(&mut || Ok(())).unwrap().unwrap(),
            cells(&r)
        );
    }
    assert_eq!(visits.get(), 1);
    assert_eq!(pull.delivered, 3);
    assert_eq!(
        pull.next_checked(&mut || Ok(())),
        Some(Err(StandingQueryFailure::ResultBudget))
    );
    assert_eq!(pull.state, VertexScanState::Failed);
    assert_eq!(drops.get(), 1);
    assert!(pull.runs.is_none() && pull.pending.is_none());
    let stats = pull.stats;
    pull.close();
    pull.close();
    assert_eq!(pull.state, VertexScanState::Failed);
    assert_eq!(pull.stats, stats);
    assert_eq!(visits.get(), 1);
    let iterator = Tracked {
        runs: vec![Run {
            row: NativeRow::Values(&r),
            weight: None,
        }]
        .into_iter(),
        visits: Rc::clone(&visits),
        drops: Rc::clone(&drops),
    };
    let mut unopened = Pull::new(
        Box::new(iterator),
        layout(1),
        CommitSeq(9),
        policy(),
        StandingQueryStats::default(),
    );
    unopened.close();
    assert_eq!(unopened.state, VertexScanState::Closed);
    assert!(
        unopened
            .next_checked(&mut || panic!("closed cursor invoked control"))
            .is_none()
    );
    assert_eq!(visits.get(), 1);
    assert_eq!(drops.get(), 2);
}

#[test]
fn every_delivery_checkpoint_keeps_only_complete_rows_and_fuses_on_refusal_or_unwind() {
    let rows = bag([
        (row(int(2)), 2),
        (
            row(GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::bytes(vec![7; 512]).unwrap()),
                    int(1),
                ]
                .into_boxed_slice(),
            )),
            1,
        ),
    ]);
    let before = rows
        .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap();
    let mut successful = make(&rows, None, 1, policy());
    let mut calls = 0;
    let mut expected = Vec::new();
    while let Some(result) = successful.next_checked(&mut || {
        calls += 1;
        Ok(())
    }) {
        expected.push(result.unwrap());
    }
    for stop in 1..=calls {
        let mut candidate = make(&rows, None, 1, policy());
        let mut seen = 0;
        let mut prefix = Vec::new();
        loop {
            match candidate.next_checked(&mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            }) {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(error)) => {
                    assert_eq!(error, StandingQueryFailure::Interrupted);
                    break;
                }
                None => panic!("failure point was not exercised"),
            }
        }
        assert_eq!(seen, stop);
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(candidate.delivered, prefix.len() as u64);
        assert_eq!(candidate.state, VertexScanState::Failed);
        assert!(candidate.runs.is_none() && candidate.pending.is_none());
        assert!(
            candidate
                .next_checked(&mut || panic!("failure was not fused"))
                .is_none()
        );
        assert_eq!(rows, before);
        assert_eq!(
            drain(&mut make(&rows, None, 1, policy())).unwrap(),
            expected
        );
    }
    for stop in [1, calls / 2, calls] {
        let mut candidate = make(&rows, None, 1, policy());
        let mut seen = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while candidate
                    .next_checked(&mut || {
                        seen += 1;
                        assert_ne!(seen, stop, "injected downstream unwind");
                        Ok(())
                    })
                    .is_some()
                {}
            }))
            .is_err()
        );
        assert_eq!(candidate.state, VertexScanState::Failed);
        assert!(candidate.runs.is_none() && candidate.pending.is_none());
        assert_eq!(rows, before);
    }
}

#[test]
fn cumulative_exact_and_one_less_allowances_include_eof_without_partial_cells() {
    let rows = bag([(row(int(1)), 2), (row(int(4)), 1)]);
    let mut reference = make(&rows, None, 1, policy());
    let expected = drain(&mut reference).unwrap();
    let stats = reference.stats;
    let exact = GqlQueryPolicy::new(0, 3, stats.work_units, stats.scratch_entries);
    assert_eq!(drain(&mut make(&rows, None, 1, exact)).unwrap(), expected);
    for (policy, error) in [
        (
            GqlQueryPolicy::new(0, 3, stats.work_units - 1, stats.scratch_entries),
            StandingQueryFailure::WorkBudget,
        ),
        (
            GqlQueryPolicy::new(0, 3, stats.work_units, stats.scratch_entries - 1),
            StandingQueryFailure::ScratchBudget,
        ),
        (
            GqlQueryPolicy::new(0, 2, stats.work_units, stats.scratch_entries),
            StandingQueryFailure::ResultBudget,
        ),
    ] {
        let mut pull = make(&rows, None, 1, policy);
        let mut prefix = Vec::new();
        loop {
            match pull.next_checked(&mut || Ok(())) {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(actual)) => {
                    assert_eq!(actual, error);
                    break;
                }
                None => panic!("one-less allowance succeeded"),
            }
        }
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(pull.delivered, prefix.len() as u64);
        assert_eq!(pull.state, VertexScanState::Failed);
    }
    let empty = ZSet::new();
    assert!(
        drain(&mut make(
            &empty,
            None,
            1,
            GqlQueryPolicy::new(0, 0, 100, 100)
        ))
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        make(&rows, None, 1, GqlQueryPolicy::new(0, 0, 100, 100)).next_checked(&mut || Ok(())),
        Some(Err(StandingQueryFailure::ResultBudget))
    );
}

#[test]
fn exact_aggregate_cells_keep_textual_slot_order_and_repetitions() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let pattern = builder
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("p", "n", fgdb_delta_types::PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let definition = PreparedGraphAggregate::prepare(
        pattern,
        &[0],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("avg", 1),
            GraphAggregate::min("id_min", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let average = QueryValue::Average(GraphExactAverage::new(-3, 2).unwrap());
    let id = GraphValue::Vertex(VId(u128::MAX));
    let row = definition
        .materialize_incremental_row(
            vec![id.clone()],
            vec![
                QueryValue::Count(u64::MAX),
                QueryValue::Integer(i128::MIN),
                average.clone(),
                QueryValue::Value(id.clone()),
            ],
        )
        .unwrap();
    use GraphAggregateTextSlot::{Aggregate as A, GroupKey as K};
    let metadata = Arc::new(Layout::Aggregate {
        columns: (0..6).map(|i| format!("c{i}")).collect(),
        slots: vec![A(2), K(0), A(1), A(0), A(2), A(3)],
    });
    let mut pull = Pull::new(
        Box::new(std::iter::once(Run {
            row: NativeRow::Group(&row),
            weight: None,
        })),
        metadata,
        CommitSeq(5),
        policy(),
        StandingQueryStats::default(),
    );
    assert_eq!(
        drain(&mut pull).unwrap(),
        vec![vec![
            average.clone(),
            QueryValue::Value(id.clone()),
            QueryValue::Integer(i128::MIN),
            QueryValue::Count(u64::MAX),
            average,
            QueryValue::Value(id)
        ]]
    );
    let metadata = Arc::new(Layout::Aggregate {
        columns: vec!["first".into(), "missing".into()],
        slots: vec![A(0), A(9)],
    });
    let mut invalid = Pull::new(
        Box::new(std::iter::once(Run {
            row: NativeRow::Group(&row),
            weight: None,
        })),
        metadata,
        CommitSeq(5),
        policy(),
        StandingQueryStats::default(),
    );
    assert_eq!(
        invalid.next_checked(&mut || Ok(())),
        Some(Err(StandingQueryFailure::InvalidDelta))
    );
    assert_eq!(invalid.delivered, 0);
    assert_eq!(invalid.state, VertexScanState::Failed);
}

#[test]
fn invalid_run_weights_and_widths_are_not_silently_treated_as_empty() {
    let row = row(int(1));
    for weight in [ZWeight::ZERO, ZWeight::from_i128(-1)] {
        let mut pull = Pull::new(
            Box::new(std::iter::once(Run {
                row: NativeRow::Values(&row),
                weight: Some(&weight),
            })),
            layout(1),
            CommitSeq(1),
            policy(),
            StandingQueryStats::default(),
        );
        assert_eq!(
            pull.next_checked(&mut || Ok(())),
            Some(Err(StandingQueryFailure::InvalidDelta))
        );
        assert_eq!(pull.delivered, 0);
    }
    let rows = bag([(row, 1)]);
    let mut wrong = make(&rows, None, 2, policy());
    assert_eq!(
        wrong.next_checked(&mut || Ok(())),
        Some(Err(StandingQueryFailure::InvalidDelta))
    );
    assert_eq!(wrong.delivered, 0);
}

#[test]
fn prefix_work_is_independent_of_unread_support() {
    let mut usages = Vec::new();
    for size in [2, 4096] {
        let rows = bag((0..size).map(|n| (row(int(n)), 7)));
        let mut pull = make(&rows, None, 1, GqlQueryPolicy::new(0, 1, 100, 100));
        assert_eq!(
            pull.next_checked(&mut || Ok(())).unwrap().unwrap(),
            vec![QueryValue::Value(int(0))]
        );
        pull.close();
        usages.push(pull.stats);
        assert_eq!(pull.delivered, 1);
    }
    assert_eq!(usages[0], usages[1]);
}

#[path = "database_tests.rs"]
mod database;
