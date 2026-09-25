use super::*;
use crate::{GraphAggregate, GraphSetValue, PreparedGraphSetAggregate};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}

fn relation(values: &[Option<i64>]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                values
                    .iter()
                    .map(|value| GraphSetValue::Value(scalar(*value)))
                    .collect(),
            ),
        )
        .unwrap()
}

fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    Err(GqlQueryError::Source("unexpected graph source"))
}

fn execute(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
) -> SetResult<GqlQueryExecution<GraphValueRow>, &'static str, usize> {
    query.execute_governed(policy, no_source, || Ok(()))
}

fn folded(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    let mut output = Vec::new();
    let (rows, _) = query
        .fold_governed(
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
            no_source,
            || Ok(()),
            |row, _| {
                output.push(row);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(rows.snapshot_records, 0);
    assert_eq!(rows.result_rows, 0);
    output
}

/// Independent count arithmetic, including set equality of nulls. It does not
/// call the production merge, sorting, filtering or aggregate implementations.
fn expected(
    left: &[Option<i64>],
    right: &[Option<i64>],
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
) -> Vec<GraphValueRow> {
    let mut counts = BTreeMap::<GraphValue, (usize, usize)>::new();
    for value in left {
        counts.entry(scalar(*value)).or_default().0 += 1;
    }
    for value in right {
        counts.entry(scalar(*value)).or_default().1 += 1;
    }
    let mut rows = Vec::new();
    for (value, (mut left, mut right)) in counts {
        if quantifier == GraphSetQuantifier::Distinct {
            left = usize::from(left != 0);
            right = usize::from(right != 0);
        }
        let count = match operation {
            GraphSetOperation::Union if quantifier == GraphSetQuantifier::Distinct => {
                usize::from(left + right != 0)
            }
            GraphSetOperation::Union => left + right,
            GraphSetOperation::Intersect => left.min(right),
            GraphSetOperation::Except => left.saturating_sub(right),
        };
        for _ in 0..count {
            rows.push(GraphValueRow::from_owned_values(vec![value.clone()]));
        }
    }
    rows
}

const OPERATIONS: [GraphSetOperation; 3] = [
    GraphSetOperation::Union,
    GraphSetOperation::Intersect,
    GraphSetOperation::Except,
];
const QUANTIFIERS: [GraphSetQuantifier; 2] =
    [GraphSetQuantifier::All, GraphSetQuantifier::Distinct];

#[test]
fn set_folds_keep_null_multiplicity_child_pages_and_nested_windows() {
    let bags = [
        vec![],
        vec![None],
        vec![Some(-3), Some(2), None, Some(2)],
        vec![Some(2), Some(2), Some(2)],
        vec![None, None, Some(-3)],
    ];
    for left in &bags {
        for right in &bags {
            for operation in OPERATIONS {
                for quantifier in QUANTIFIERS {
                    for inner_skip in [0, 1, u64::MAX] {
                        let selected_left: Vec<_> = left
                            .iter()
                            .copied()
                            .skip(usize::try_from(inner_skip).unwrap_or(usize::MAX))
                            .take(3)
                            .collect();
                        let selected_right: Vec<_> = right.iter().copied().take(4).collect();
                        let values =
                            expected(&selected_left, &selected_right, operation, quantifier);
                        let input = relation(left)
                            .with_page(inner_skip, Some(3))
                            .combine(operation, quantifier, relation(right).with_page(0, Some(4)))
                            .unwrap();
                        assert!(input.has_foldable_expansion());
                        for (skip, count) in [(0, 0), (1, 2), (u64::MAX, 1)] {
                            let query =
                                input.clone().nested().unwrap().with_page(skip, Some(count));
                            let before = query.canonical_bytes();
                            let page: Vec<_> = values
                                .iter()
                                .skip(usize::try_from(skip).unwrap_or(usize::MAX))
                                .take(count as usize)
                                .cloned()
                                .collect();
                            assert_eq!(folded(&query), page);
                            assert_eq!(execute(&query, wide()).unwrap().value, page);
                            assert_eq!(query.canonical_bytes(), before);
                        }
                        assert_eq!(folded(&input), values);
                    }
                }
            }
        }
    }
}

#[test]
fn ranked_set_pages_reuse_topk_with_exact_direction_and_null_placement() {
    let left = [Some(4), None, Some(-2), Some(4), Some(1)];
    let right = [None, Some(-2), Some(-2), Some(3)];
    for operation in OPERATIONS {
        for quantifier in QUANTIFIERS {
            for descending in [false, true] {
                for nulls_first in [false, true] {
                    let mut values = expected(&left, &right, operation, quantifier);
                    values.sort_by(|a, b| {
                        let (a, b) = (&a.values()[0], &b.values()[0]);
                        match (a.is_null(), b.is_null()) {
                            (true, true) => Ordering::Equal,
                            (true, false) => {
                                if nulls_first {
                                    Ordering::Less
                                } else {
                                    Ordering::Greater
                                }
                            }
                            (false, true) => {
                                if nulls_first {
                                    Ordering::Greater
                                } else {
                                    Ordering::Less
                                }
                            }
                            (false, false) => {
                                if descending {
                                    a.cmp(b).reverse()
                                } else {
                                    a.cmp(b)
                                }
                            }
                        }
                    });
                    let query = relation(&left)
                        .combine(operation, quantifier, relation(&right))
                        .unwrap()
                        .with_order_by(&[GraphValueOrder {
                            column: 0,
                            descending,
                            nulls_first,
                        }])
                        .unwrap()
                        .with_page(1, Some(3));
                    assert!(page::supports(&query));
                    let expected: Vec<_> = values.into_iter().skip(1).take(3).collect();
                    assert_eq!(execute(&query, wide()).unwrap().value, expected);
                    assert_eq!(folded(&query), expected);
                }
            }
        }
    }
}

#[test]
fn a_finite_set_page_retains_only_selected_merge_output_and_spends_only_public_rows() {
    let values: Vec<_> = (0..32).map(Some).collect();
    let query = relation(&values)
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            relation(&values),
        )
        .unwrap();
    let materialized = execute(&query, wide()).unwrap();
    assert_eq!(materialized.value.len(), 64);
    let mut visited = 0_u64;
    let (rows, evaluator) = query
        .fold_governed(
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
            no_source,
            || Ok(()),
            |_, _| {
                visited += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(visited, 64);
    assert_eq!(rows.result_rows, 0);
    // The child bags and sort/dedup work are identical. Only the complete
    // merged output bag's 64 retention events disappear from the fold.
    assert_eq!(
        materialized.evaluator.scratch_entries,
        evaluator.scratch_entries + 64
    );
    let page = query.clone().with_page(0, Some(1));
    let policy = GqlQueryPolicy::new(0, 1, u64::MAX, evaluator.scratch_entries + 1);
    let result = execute(&page, policy).unwrap();
    assert_eq!(result.value, materialized.value[..1]);
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(
        result.evaluator.scratch_entries,
        evaluator.scratch_entries + 1
    );
    let empty = execute(
        &query.with_page(0, Some(0)),
        GqlQueryPolicy::new(0, 0, u64::MAX, evaluator.scratch_entries),
    )
    .unwrap();
    assert!(empty.value.is_empty());
}

fn graph_leaf() -> PreparedGraphSet {
    let mut builder = crate::algebra::GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
        .unwrap()
        .into()
}

#[test]
fn empty_set_pages_and_empty_left_inputs_do_not_hide_the_right_source_or_reset_admission() {
    for operation in OPERATIONS {
        for quantifier in QUANTIFIERS {
            for count in [0, 1] {
                let query = graph_leaf()
                    .combine(operation, quantifier, graph_leaf())
                    .unwrap()
                    .with_page(0, Some(count));
                let mut calls = 0;
                let failure = query
                    .execute_governed(
                        wide(),
                        |_, _| {
                            calls += 1;
                            if calls == 2 {
                                return Err(GqlQueryError::Source("late right source"));
                            }
                            Ok(GqlQueryExecution {
                                value: Vec::new(),
                                rows: GqlExecutionStats {
                                    snapshot_records: 2,
                                    result_rows: 0,
                                },
                                evaluator: GlaExecutionStats::default(),
                            })
                        },
                        || Ok::<_, usize>(()),
                    )
                    .unwrap_err();
                assert_eq!(calls, 2);
                assert_eq!(
                    failure,
                    GqlQueryError::Source(GraphSetExecutionError::Source("late right source"))
                );
                let mut calls = 0;
                let failure = query
                    .execute_governed(
                        GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
                        |_, _| {
                            calls += 1;
                            Ok::<_, GqlQueryError<&'static str, usize>>(GqlQueryExecution {
                                value: vec![GraphValueRow::from_owned_values(vec![
                                    GraphValue::Vertex(VId(1)),
                                ])],
                                rows: GqlExecutionStats {
                                    snapshot_records: 2,
                                    result_rows: 1,
                                },
                                evaluator: GlaExecutionStats::default(),
                            })
                        },
                        || Ok(()),
                    )
                    .unwrap_err();
                assert_eq!(calls, 2);
                assert!(matches!(
                    failure,
                    GqlQueryError::Rows(GqlBudgetExceeded {
                        dimension: GqlBudgetDimension::SnapshotRecords,
                        limit: 3,
                        observed: 4,
                    })
                ));
            }
        }
    }
}

#[test]
fn a_late_child_expression_error_wins_before_any_set_row_reaches_the_sink() {
    let invalid = PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(vec![
                GraphSetValue::List(vec![GraphSetValue::Value(scalar(Some(1)))]),
                GraphSetValue::Value(GraphValue::Scalar(
                    CanonicalScalar::ucs_basic_text("private invalid list").unwrap(),
                )),
            ]),
        )
        .unwrap()
        .unwind("y".into(), GraphSetValue::Column(0))
        .unwrap();
    let empty = relation(&[])
        .unwind("y".into(), GraphSetValue::List(Vec::new()))
        .unwrap();
    for operation in OPERATIONS {
        for quantifier in QUANTIFIERS {
            let query = empty
                .clone()
                .combine(operation, quantifier, invalid.clone())
                .unwrap()
                .with_page(0, Some(0));
            let mut delivered = 0;
            let error = query
                .fold_governed(
                    wide(),
                    no_source,
                    || Ok(()),
                    |_, _| {
                        delivered += 1;
                        Ok(())
                    },
                )
                .unwrap_err();
            assert_eq!(delivered, 0);
            assert!(matches!(
                &error,
                GqlQueryError::Source(GraphSetExecutionError::Projection {
                    row: 1,
                    column: 1,
                    ..
                })
            ));
            assert_eq!(execute(&query, wide()).unwrap_err(), error);
            assert!(!format!("{error}").contains("private invalid list"));
        }
    }
}

#[test]
fn every_set_page_checkpoint_and_resource_dimension_can_refuse_without_partial_success() {
    for operation in OPERATIONS {
        for quantifier in QUANTIFIERS {
            let query = relation(&[Some(0), Some(1), Some(1), Some(2)])
                .combine(operation, quantifier, relation(&[Some(1), Some(3)]))
                .unwrap()
                .with_page(0, Some(1));
            let mut total = 0;
            let normal = query
                .execute_governed(wide(), no_source, || {
                    total += 1;
                    Ok(())
                })
                .unwrap();
            for stop in 1..=total {
                let mut seen = 0;
                let failure = query
                    .execute_governed(wide(), no_source, || {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err();
                assert_eq!(failure, GqlQueryError::Interrupted(stop));
                assert_eq!(seen, stop);
            }
            for (dimension, work, scratch) in [
                (
                    GlaLimitDimension::WorkUnits,
                    normal.evaluator.work_units - 1,
                    u64::MAX,
                ),
                (
                    GlaLimitDimension::ScratchEntries,
                    u64::MAX,
                    normal.evaluator.scratch_entries - 1,
                ),
            ] {
                let failure =
                    execute(&query, GqlQueryPolicy::new(0, 1, work, scratch)).unwrap_err();
                assert!(matches!(failure,
                    GqlQueryError::Evaluator(GlaLimitExceeded { dimension: actual, .. })
                        if actual == dimension
                ));
            }
            assert!(matches!(
                execute(&query, GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX)),
                Err(GqlQueryError::Rows(GqlBudgetExceeded {
                    dimension: GqlBudgetDimension::ResultRows,
                    limit: 0,
                    observed: 1,
                }))
            ));
        }
    }
}

#[test]
fn numeric_aggregates_over_sets_match_the_explicit_materialized_order_barrier() {
    for operation in OPERATIONS {
        for quantifier in QUANTIFIERS {
            let input = relation(&[None, Some(-2), Some(3), Some(3)])
                .combine(operation, quantifier, relation(&[Some(3), None, Some(4)]))
                .unwrap()
                .with_page(1, Some(5));
            for keys in [Vec::new(), vec![0]] {
                let aggregates = [
                    GraphAggregate::count_rows("rows"),
                    GraphAggregate::count("nonnull", 0),
                    GraphAggregate::sum_int("sum", 0),
                    GraphAggregate::average_int("mean", 0),
                ];
                let folded =
                    PreparedGraphSetAggregate::prepare(input.clone(), &keys, &aggregates, 0, None)
                        .unwrap();
                // Keep the selected input page inside its own scope. An outer
                // explicit order blocks aggregate folding, without changing
                // this numeric summary or pushing the input page past the set.
                let barrier = input
                    .clone()
                    .nested()
                    .unwrap()
                    .with_order_by(&[GraphValueOrder {
                        column: 0,
                        descending: false,
                        nulls_first: true,
                    }])
                    .unwrap();
                assert!(folded.input().has_foldable_expansion());
                assert!(!barrier.has_foldable_expansion());
                let materialized =
                    PreparedGraphSetAggregate::prepare(barrier, &keys, &aggregates, 0, None)
                        .unwrap();
                let a = folded
                    .execute_governed(wide(), no_source, || Ok(()))
                    .unwrap();
                let b = materialized
                    .execute_governed(wide(), no_source, || Ok(()))
                    .unwrap();
                assert_eq!(a.value, b.value);
                assert_eq!(a.rows, b.rows);
            }
        }
    }
}
