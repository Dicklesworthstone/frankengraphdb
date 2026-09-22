use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use crate::{GqlParameters, PreparedGraphSetText};
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
fn equal() -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(0), comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(1),
    }
}
// Primitive independent ordering: explicit NULL placement is NOT reversed by
// DESC, and a tied explicit key is followed by canonical complete-row order.
fn compare(a: &[Option<i64>; 2], b: &[Option<i64>; 2], order: &[GraphValueOrder]) -> Ordering {
    for key in order {
        let result = match (a[key.column], b[key.column]) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => if key.nulls_first { Ordering::Less } else { Ordering::Greater },
            (Some(_), None) => if key.nulls_first { Ordering::Greater } else { Ordering::Less },
            (Some(a), Some(b)) => if key.descending { b.cmp(&a) } else { a.cmp(&b) },
        };
        if result != Ordering::Equal { return result; }
    }
    a.cmp(b)
}

#[test]
fn ranked_product_and_predicate_pages_match_independent_null_and_occurrence_oracles() {
    let left = [Some(3), None, Some(1), Some(3)];
    let right = [Some(1), Some(3), None, Some(3)];
    for mask in 0..256 {
        let a: Vec<_> = left.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, value)| *value).collect();
        let b: Vec<_> = right.iter().enumerate().filter(|(i, _)| mask & (1 << (i + 4)) != 0)
            .map(|(_, value)| *value).collect();
        for filtered in [false, true] {
            let product = sequence(&a, "a").cross_join(sequence(&b, "b")).unwrap();
            let input = if filtered { product.filter(&[equal()]).unwrap() } else { product };
            for descending in [false, true] {
                for nulls_first in [false, true] {
                    let order = [GraphValueOrder { column: 1, descending, nulls_first }];
                    let mut all = Vec::new();
                    for &a in &a {
                        for &b in &b {
                            if !filtered || a.zip(b).is_some_and(|(a, b)| a == b) { all.push([a, b]); }
                        }
                    }
                    all.sort_by(|a, b| compare(a, b, &order));
                    for (skip, count) in [(0, 0), (0, 1), (1, 3), (4, 20)] {
                        let query = input.clone().with_order_by(&order).unwrap().with_page(skip, Some(count));
                        assert!(supports(&query));
                        let observed = execute(&query, policy(), || Ok(())).unwrap();
                        let expected: Vec<_> = all.iter().skip(skip as usize).take(count as usize)
                            .map(|value| row(value)).collect();
                        assert_eq!(observed.value, expected);
                        assert_eq!(observed.rows.result_rows, expected.len() as u64);
                    }
                }
            }
        }
    }
}

#[test]
fn heap_retains_only_offset_plus_limit_and_reuses_slots_across_twenty_thousand_offers() {
    let order = [GraphValueOrder::ascending(0)];
    let mut ranked = Ranked::new(&order, 2, 5);
    let mut scratch = 0;
    let mut control = |event| {
        scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
        Ok::<_, usize>(())
    };
    for i in 0..20_000 {
        let value = (i * 37) % 20_000;
        ranked.offer(row(&[Some(value)]), &mut control).unwrap();
        assert!(ranked.rows.len() <= 7);
        for child in 1..ranked.rows.len() {
            assert!(ranked.rows[(child - 1) / 2] >= ranked.rows[child]);
        }
    }
    assert_eq!(scratch, 7); // replacement moves into an existing retained slot
    let output = ranked.finish(2, 5, &mut |_| Ok::<_, usize>(())).unwrap();
    let expected: Vec<_> = (2..7).map(|value| row(&[Some(value)])).collect();
    assert_eq!(output, expected);
    for (offset, count) in [(u64::MAX, u64::MAX), (u64::MAX, 0)] {
        let mut ranked = Ranked::new(&order, offset, count);
        for value in [3, 1, 2] {
            ranked.offer(row(&[Some(value)]), &mut |_| Ok::<_, usize>(())).unwrap();
        }
        assert_eq!(ranked.rows.len(), if count == 0 { 0 } else { 3 });
        assert!(ranked.finish(offset, count, &mut |_| Ok::<_, usize>(())).unwrap().is_empty());
    }
}

#[test]
fn full_row_ties_multikey_order_and_nested_pages_survive_topk_selection() {
    let product = sequence(&[Some(3), Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(0), Some(1)], "b")).unwrap();
    let query = product.clone().with_page(1, Some(2)).nested().unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(1));
    assert_eq!(execute(&query, policy(), || Ok(())).unwrap().value, vec![row(&[Some(3), Some(1)])]);
    // Dropping the child page would choose (3, 0), not (3, 1).
    let direct = product.clone().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(1));
    assert_eq!(execute(&direct, policy(), || Ok(())).unwrap().value, vec![row(&[Some(3), Some(0)])]);
    let order = [GraphValueOrder::ascending(1), GraphValueOrder::descending(0)];
    let query = product.clone().with_order_by(&order).unwrap().with_page(1, Some(3));
    assert_eq!(execute(&query, policy(), || Ok(())).unwrap().value, vec![
        row(&[Some(2), Some(0)]), row(&[Some(1), Some(0)]), row(&[Some(3), Some(1)]),
    ]);
    // The existing full materialization/sorter remains a differential baseline.
    let all = execute(&product.with_order_by(&order).unwrap(), policy(), || Ok(())).unwrap().value;
    assert_eq!(execute(&query, policy(), || Ok(())).unwrap().value, all[1..4]);
}

#[test]
fn ranker_preserves_dynamic_value_domains_and_full_width_identities() {
    let order = [GraphValueOrder { column: 0, descending: false, nulls_first: true }];
    let values = vec![
        GraphValue::Vertex(VId(u128::MAX)), GraphValue::Vertex(VId(1)),
        GraphValue::Scalar(CanonicalScalar::Null), GraphValue::Scalar(CanonicalScalar::Int(2)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("two").unwrap()),
        GraphValue::Scalar(CanonicalScalar::bytes(vec![2]).unwrap()),
        GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice()),
        GraphValue::Scalar(CanonicalScalar::Int(2)),
    ];
    let mut expected: Vec<_> = values.iter().cloned().map(|value| {
        GraphValueRow::from_owned_values(vec![value])
    }).collect();
    expected.sort();
    let mut ranked = Ranked::new(&order, 1, 6);
    for value in values {
        ranked.offer(GraphValueRow::from_owned_values(vec![value]), &mut |_| Ok::<_, usize>(())).unwrap();
    }
    assert_eq!(ranked.finish(1, 6, &mut |_| Ok::<_, usize>(())).unwrap(), expected[1..7]);
}

#[test]
fn rank_selection_never_hides_a_late_expression_error_even_for_limit_zero() {
    let input = PreparedGraphSet::singleton().unwind("value".into(), GraphSetValue::List(vec![
        GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1)))]),
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
    ])).unwrap();
    let input = input.project(vec![GraphSetProjection::new(
        "size", GraphSetValue::Size(Box::new(GraphSetValue::Column(0))),
    )], GraphSetQuantifier::All).unwrap();
    for count in [0, 1] {
        let query = input.clone().with_order_by(&[GraphValueOrder::ascending(0)]).unwrap().with_page(0, Some(count));
        assert!(supports(&query));
        assert!(matches!(execute(&query, policy(), || Ok(())),
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, .. }))));
    }
    let input = sequence(&[Some(2), Some(1), Some(2)], "value").project(vec![
        GraphSetProjection::new("value", GraphSetValue::Column(0)),
    ], GraphSetQuantifier::Distinct).unwrap();
    let query = input.with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(2));
    assert!(!supports(&query)); // DISTINCT still owns its complete duplicate classes
    assert_eq!(execute(&query, policy(), || Ok(())).unwrap().value, vec![row(&[Some(2)]), row(&[Some(1)])]);
}

#[test]
fn native_ordered_unwind_match_with_queries_use_the_same_topk_path() {
    let query = PreparedGraphSetText::prepare(
        "UNWIND [5, 1, 5, 2] AS n RETURN n AS value ORDER BY value DESC SKIP 1 LIMIT 2",
        |_, _| None,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    assert!(supports(&query));
    assert_eq!(execute(&query, policy(), || Ok(())).unwrap().value, vec![row(&[Some(5)]), row(&[Some(2)])]);
    let query = PreparedGraphSetText::prepare(
        "UNWIND [2, 1, 2] AS wanted MATCH (n) WITH wanted, n.p AS value WHERE wanted = value RETURN value, wanted ORDER BY value DESC LIMIT 2",
        |kind, name| {
            if kind == crate::GraphSymbolKind::Property && name == "p" {
                Some(crate::GraphSymbol::Property(fgdb_delta_types::PropertyKeyId(1)))
            } else { None }
        },
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    assert!(supports(&query));
    let frozen = query.canonical_bytes();
    let mut calls = 0;
    let observed = query.execute_governed(policy(), |pattern, remaining| {
        calls += 1;
        pattern.plan().execute_governed_with_properties(
            2, [VId(1), VId(2)], [], |_, _| Ok::<_, usize>(true),
            |id, _| Ok(Some(CanonicalScalar::Int(id.0 as i64))), remaining, || Ok::<_, usize>(()),
        )
    }, || Ok::<_, usize>(())).unwrap();
    assert_eq!(calls, 1);
    assert_eq!(observed.rows.snapshot_records, 2);
    assert_eq!(observed.value, vec![row(&[Some(2), Some(2)]), row(&[Some(2), Some(2)])]);
    assert_eq!(query.canonical_bytes(), frozen);
}

#[test]
fn late_source_failure_and_snapshot_budget_are_not_converted_into_empty_ranked_results() {
    let leaf = || {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(builder.prepare_values(&[GraphColumn::vertex("n", "n")], 0, None).unwrap())
    };
    for count in [0, 1] {
        let query = leaf().cross_join(leaf()).unwrap().with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap().with_page(0, Some(count));
        let mut calls = 0;
        let result = query.execute_governed(policy(), |_, _| {
            calls += 1;
            if calls == 1 {
                Ok(GqlQueryExecution {
                    value: vec![], rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
                    evaluator: GlaExecutionStats::default(),
                })
            } else { Err(GqlQueryError::Source(71_usize)) }
        }, || Ok::<_, usize>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Source(71)))));
        assert_eq!(calls, 2);
        let result = query.execute_governed(GqlQueryPolicy::new(1, 10, 100_000, 100_000), |_, _| {
            Ok::<_, GqlQueryError<usize, usize>>(GqlQueryExecution {
                value: vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))])],
                rows: GqlExecutionStats { snapshot_records: 1, result_rows: 1 },
                evaluator: GlaExecutionStats::default(),
            })
        }, || Ok::<_, usize>(()));
        assert!(matches!(result, Err(GqlQueryError::Rows(GqlBudgetExceeded {
            dimension: GqlBudgetDimension::SnapshotRecords, observed: 2, ..
        }))));
    }
}

#[test]
fn every_heap_and_final_sort_failure_drops_the_tentative_selection() {
    let order = [GraphValueOrder::descending(0)];
    let run = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| {
        let mut ranked = Ranked::new(&order, 1, 3);
        let mut event = |value| control(value);
        for value in [1, 8, 3, 7, 2, 9, 5, 9, 0] {
            ranked.offer(row(&[Some(value)]), &mut event)?;
        }
        ranked.finish(1, 3, &mut event)
    };
    let mut count = 0;
    let expected = run(&mut |_| { count += 1; Ok(()) }).unwrap();
    assert_eq!(expected, vec![row(&[Some(9)]), row(&[Some(8)]), row(&[Some(7)])]);
    for stop in 1..=count {
        let mut seen = 0;
        assert_eq!(run(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn ordered_pipeline_cancellation_and_exact_quotas_cover_input_heap_sort_and_delivery() {
    let query = sequence(&[Some(3), None, Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(2), Some(3)], "b")).unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(2, Some(3));
    let frozen = query.canonical_bytes();
    let mut calls = 0;
    let expected = execute(&query, policy(), || { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(execute(&query, policy(), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    let work = expected.evaluator.work_units;
    let scratch = expected.evaluator.scratch_entries;
    let exact = GqlQueryPolicy::new(0, 3, work, scratch);
    assert_eq!(execute(&query, exact, || Ok(())).unwrap().value, expected.value);
    for denied in [
        GqlQueryPolicy::new(0, 2, work, scratch),
        GqlQueryPolicy::new(0, 3, work - 1, scratch),
        GqlQueryPolicy::new(0, 3, work, scratch - 1),
    ] { assert!(execute(&query, denied, || Ok(())).is_err()); }
    assert_eq!(query.canonical_bytes(), frozen);
}
