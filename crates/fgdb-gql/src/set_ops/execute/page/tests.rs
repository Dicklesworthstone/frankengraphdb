use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_types::{CanonicalScalar, VId};

fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter().map(|value| {
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
    }).collect())
}
fn sequence(values: &[Option<i64>], name: &str) -> PreparedGraphSet {
    PreparedGraphSet::singleton().unwind(name.into(), GraphSetValue::List(
        values.iter().map(|value| GraphSetValue::Value(
            GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
        )).collect(),
    )).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn execute(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> SetResult<GqlQueryExecution<GraphValueRow>, usize, usize> {
    query.execute_governed(policy, |_, _| {
        panic!("source-free execution must not open a graph")
    }, checkpoint)
}
fn equality() -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(0),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(1),
    }
}
fn leaf() -> PreparedGraphSet {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
        .unwrap().with_duplicates().into()
}

#[test]
fn product_and_selected_pages_match_primitive_occurrence_order_not_key_order() {
    let a = [Some(2), None, Some(1), Some(2)];
    let b = [Some(1), Some(2), Some(2), None];
    for mask in 0..256 {
        let left: Vec<_> = a.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, value)| *value).collect();
        let right: Vec<_> = b.iter().enumerate().filter(|(i, _)| mask & (1 << (i + 4)) != 0)
            .map(|(_, value)| *value).collect();
        for filtered in [false, true] {
            let mut query = sequence(&left, "a").cross_join(sequence(&right, "b")).unwrap();
            if filtered { query = query.filter(&[equality()]).unwrap(); }
            let mut all = Vec::new();
            for &a in &left {
                for &b in &right {
                    if !filtered || a.zip(b).is_some_and(|(a, b)| a == b) {
                        all.push(row(&[a, b]));
                    }
                }
            }
            for (skip, count) in [(0, 0), (0, 1), (1, 3), (3, 20), (20, 1)] {
                let selected = query.clone().with_page(skip, Some(count));
                assert!(supports(&selected));
                let expected: Vec<_> = all.iter().skip(skip as usize)
                    .take(count as usize).cloned().collect();
                let observed = execute(&selected, policy(), || Ok(())).unwrap();
                assert_eq!(observed.value, expected);
                assert_eq!(observed.rows.result_rows, expected.len() as u64);
                assert_eq!(observed.rows.snapshot_records, 0);
            }
        }
    }
}

#[test]
fn hundred_million_pair_page_copies_only_the_selected_occurrences() {
    let n = 10_000_u64;
    let values: Vec<_> = (0..n).map(|value| Some(value as i64)).collect();
    let product = sequence(&values, "a").cross_join(sequence(&values, "b")).unwrap();
    let limited = GqlQueryPolicy::new(0, 3, 2_000_000, 1_000_000);
    let selected = product.clone().with_page(n * n - 5, Some(3));
    let observed = execute(&selected, limited, || Ok(())).unwrap();
    assert_eq!(observed.value, vec![
        row(&[Some(9999), Some(9995)]),
        row(&[Some(9999), Some(9996)]),
        row(&[Some(9999), Some(9997)]),
    ]);
    assert!(observed.evaluator.scratch_entries < 64 * n);
    assert!(observed.evaluator.work_units < 128 * n);
    // Huge cuts do not allocate a skip buffer or overflow offset + count.
    for (skip, count) in [(u64::MAX, u64::MAX), (0, 0)] {
        let result = execute(&product.clone().with_page(skip, Some(count)), limited, || Ok(())).unwrap();
        assert!(result.value.is_empty());
    }
    // The incumbent materialized path is exercised in the same invocation;
    // its complete result cannot fit the SAME scratch/work/result allowance.
    assert!(execute(&product, limited, || Ok(())).is_err());
}

#[test]
fn selected_page_rejects_no_payload_after_the_output_page_is_full() {
    let values: Vec<_> = (0..256).map(Some).collect();
    let query = sequence(&values, "a").cross_join(sequence(&values, "b")).unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(true))]).unwrap().with_page(20_000, Some(2));
    let observed = execute(&query, GqlQueryPolicy::new(0, 2, 2_000_000, 20_000), || Ok(())).unwrap();
    assert_eq!(observed.value, vec![row(&[Some(78), Some(32)]), row(&[Some(78), Some(33)])]);
    // Predicate visits remain governed, but unselected joined cells are not copied.
    assert!(observed.evaluator.scratch_entries < 64 * 256);
}

#[test]
fn nested_pages_distinct_and_computed_stages_keep_their_original_boundaries() {
    let product = sequence(&[Some(2), Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(1), Some(2)], "b")).unwrap();
    // WHERE after the first product occurrence returns empty. Pushing it below
    // that page would incorrectly return a matching pair.
    let barrier = product.clone().with_page(0, Some(1)).filter(&[equality()]).unwrap()
        .with_page(0, Some(1));
    assert!(execute(&barrier, policy(), || Ok(())).unwrap().value.is_empty());
    let selected = product.clone().filter(&[equality()]).unwrap().with_page(1, Some(2))
        .nested().unwrap().with_page(1, Some(1));
    assert_eq!(execute(&selected, policy(), || Ok(())).unwrap().value,
        vec![row(&[Some(2), Some(2)])]);
    let distinct = product.clone().project(vec![
        GraphSetProjection::new("a", GraphSetValue::Column(0)),
    ], GraphSetQuantifier::Distinct).unwrap().with_page(0, Some(1));
    assert!(!supports(&distinct));
    assert_eq!(execute(&distinct, policy(), || Ok(())).unwrap().value, vec![row(&[Some(1)])]);
    // Row-order-preserving ALL projection remains upstream of selection.
    let projected = product.project(vec![
        GraphSetProjection::new("b", GraphSetValue::Column(1)),
        GraphSetProjection::new("a", GraphSetValue::Column(0)),
    ], GraphSetQuantifier::All).unwrap().with_page(1, Some(3));
    assert_eq!(execute(&projected, policy(), || Ok(())).unwrap().value, vec![
        row(&[Some(2), Some(2)]), row(&[Some(1), Some(1)]), row(&[Some(2), Some(1)]),
    ]);
}

#[test]
fn exhausted_or_empty_page_does_not_hide_a_later_unwind_type_failure() {
    let input = PreparedGraphSet::singleton().unwind("value".into(), GraphSetValue::List(vec![
        GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1)))]),
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
    ])).unwrap();
    let input = input.unwind("element".into(), GraphSetValue::Column(0)).unwrap();
    for (skip, count) in [(0, 0), (0, 1), (u64::MAX, 2)] {
        let query = input.clone().with_page(skip, Some(count));
        assert!(matches!(execute(&query, policy(), || Ok(())),
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, .. }))));
    }
}

#[test]
fn both_graph_sources_and_right_schema_are_admitted_before_empty_output_succeeds() {
    for filtered in [false, true] {
        for limit in [0, 1] {
            let query = leaf().cross_join(leaf()).unwrap();
            let query = if filtered { query.filter(&[equality()]).unwrap() } else { query };
            let query = query.with_page(0, Some(limit));
            for invalid_schema in [false, true] {
                let mut calls = 0;
                let result = query.execute_governed(policy(), |_, _| {
                    calls += 1;
                    if calls == 1 {
                        Ok(GqlQueryExecution {
                            value: vec![], rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
                            evaluator: GlaExecutionStats::default(),
                        })
                    } else if invalid_schema {
                        Ok(GqlQueryExecution {
                            value: vec![GraphValueRow::unit()],
                            rows: GqlExecutionStats { snapshot_records: 1, result_rows: 1 },
                            evaluator: GlaExecutionStats::default(),
                        })
                    } else { Err(GqlQueryError::Source(71_usize)) }
                }, || Ok::<_, usize>(()));
                if invalid_schema {
                    assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 1 }))));
                } else {
                    assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Source(71)))));
                }
                assert_eq!(calls, 2);
            }
        }
    }
    // Retained source visits sum across operands even when only one pair leaves.
    let query = leaf().cross_join(leaf()).unwrap().with_page(0, Some(1));
    let mut calls = 0;
    let result = query.execute_governed(policy(), |_, _| {
        calls += 1;
        Ok::<_, GqlQueryError<usize, usize>>(GqlQueryExecution {
            value: vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(calls))])],
            rows: GqlExecutionStats { snapshot_records: 1, result_rows: 1 },
            evaluator: GlaExecutionStats::default(),
        })
    }, || Ok::<_, usize>(())).unwrap();
    assert_eq!(result.rows.snapshot_records, 2);
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(calls, 2);
}

#[test]
fn every_checkpoint_and_inclusive_result_work_scratch_limit_is_fail_closed() {
    let query = sequence(&[Some(3), Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(2), Some(3), Some(1)], "b")).unwrap()
        .filter(&[equality()]).unwrap().with_page(1, Some(2));
    let frozen = query.canonical_bytes();
    let mut calls = 0;
    let expected = execute(&query, policy(), || { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let result = execute(&query, policy(), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    let work = expected.evaluator.work_units;
    let scratch = expected.evaluator.scratch_entries;
    let rows = expected.rows.result_rows;
    let exact = GqlQueryPolicy::new(0, rows, work, scratch);
    assert_eq!(execute(&query, exact, || Ok(())).unwrap().value, expected.value);
    for denied in [
        GqlQueryPolicy::new(0, rows - 1, work, scratch),
        GqlQueryPolicy::new(0, rows, work - 1, scratch),
        GqlQueryPolicy::new(0, rows, work, scratch - 1),
    ] { assert!(execute(&query, denied, || Ok(())).is_err()); }
    assert_eq!(query.canonical_bytes(), frozen);
}

#[test]
fn chunked_selection_is_exact_even_when_the_virtual_end_exceeds_u64() {
    for skip in [0, 1, 2, 3, 7, 8, 9, u64::MAX] {
        for count in [0, 1, 2, 8, u64::MAX] {
            let mut selected = Selection { skip, remaining: count };
            let mut actual = Vec::new();
            let mut base = 0;
            for len in [0, 3, 1, 0, 4] {
                actual.extend(selected.range(len).map(|at| base + at));
                base += len;
            }
            let expected: Vec<_> = (0..8).filter(|at| {
                (*at as u128) >= u128::from(skip)
                    && (*at as u128) - u128::from(skip) < u128::from(count)
            }).collect();
            assert_eq!(actual, expected);
        }
    }
}
