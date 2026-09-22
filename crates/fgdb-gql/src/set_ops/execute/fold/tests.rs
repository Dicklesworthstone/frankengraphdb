use super::*;
use crate::{
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetOperand,
    GraphSetPredicateOp, GraphSetProjection, GraphSetValue,
};
use fgdb_types::CanonicalScalar;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn list(values: &[Option<i64>]) -> GraphSetValue {
    GraphSetValue::List(values.iter().map(|value| GraphSetValue::Value(scalar(*value))).collect())
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("constant relations do not observe a graph")
}
fn eager(query: &PreparedGraphSet) -> SetResult<GqlQueryExecution<GraphValueRow>, &'static str, usize> {
    query.execute_governed(wide(), no_source, || Ok(()))
}
fn folded(query: &PreparedGraphSet) -> SetResult<Vec<GraphValueRow>, &'static str, usize> {
    let mut rows = Vec::new();
    let (stats, _) = query.fold_governed(wide(), no_source, || Ok(()), |row, _| {
        rows.push(row);
        Ok(())
    })?;
    assert_eq!(stats.snapshot_records, 0);
    assert_eq!(stats.result_rows, 0); // These are private intermediate rows.
    Ok(rows)
}

#[test]
fn folded_scopes_filters_projections_and_pages_preserve_occurrence_order() {
    for values in [vec![], vec![None], vec![Some(1), Some(1), Some(-2)], vec![Some(3), None, Some(-1)]] {
        for inner_skip in [0, 1, u64::MAX] {
            for inner_count in [0, 1, 4] {
                for skip in [0, 1, 5] {
                    for count in [0, 1, 8] {
                        let query = PreparedGraphSet::singleton()
                            .unwind("x".into(), list(&values)).unwrap()
                            .with_page(inner_skip, Some(inner_count))
                            .unwind("y".into(), list(&[Some(2), Some(-1), Some(2)])).unwrap()
                            .project(vec![
                                GraphSetProjection::new("y", GraphSetValue::Column(1)),
                                GraphSetProjection::new("x", GraphSetValue::Column(0)),
                            ], GraphSetQuantifier::All).unwrap()
                            .filter(&[GraphSetPredicateOp::IsNull {
                                operand: GraphSetOperand::Column(1), is_null: false,
                            }]).unwrap()
                            .nested().unwrap().with_page(skip, Some(count));
                        let before = query.canonical_bytes();
                        let mut expected = Vec::new();
                        for &x in values.iter().skip(usize::try_from(inner_skip).unwrap_or(usize::MAX))
                            .take(inner_count as usize)
                        {
                            for y in [2, -1, 2] {
                                if x.is_some() {
                                    expected.push(GraphValueRow::from_owned_values(vec![scalar(Some(y)), scalar(x)]));
                                }
                            }
                        }
                        let expected: Vec<_> = expected.into_iter().skip(skip as usize).take(count as usize).collect();
                        assert!(query.has_foldable_expansion());
                        assert_eq!(folded(&query).unwrap(), expected);
                        assert_eq!(eager(&query).unwrap().value, expected);
                        assert_eq!(query.canonical_bytes(), before);
                    }
                }
            }
        }
    }
}

#[test]
fn cross_join_keeps_left_major_order_and_children_keep_their_own_windows() {
    let left = PreparedGraphSet::singleton().unwind("a".into(), list(&[Some(3), Some(1), Some(3)])).unwrap();
    let right = PreparedGraphSet::singleton().unwind("b".into(), list(&[None, Some(4), Some(4)])).unwrap();
    for left_count in [0, 1, 3] {
        for skip in 0..5 {
            let query = left.clone().with_page(0, Some(left_count))
                .cross_join(right.clone().with_page(1, Some(2))).unwrap().with_page(skip, Some(3));
            let expected: Vec<_> = [3, 1, 3].into_iter().take(left_count as usize)
                .flat_map(|a| [4, 4].into_iter().map(move |b| {
                    GraphValueRow::from_owned_values(vec![scalar(Some(a)), scalar(Some(b))])
                })).skip(skip as usize).take(3).collect();
            assert_eq!(folded(&query).unwrap(), expected);
            assert_eq!(eager(&query).unwrap().value, expected);
        }
    }
}

fn invalid_child() -> PreparedGraphSet {
    let values = GraphSetValue::List(vec![
        GraphSetValue::List(vec![GraphSetValue::Value(scalar(Some(1)))]),
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::ucs_basic_text("private invalid list").unwrap())),
    ]);
    PreparedGraphSet::singleton().unwind("x".into(), values).unwrap()
        .unwind("y".into(), GraphSetValue::Column(0)).unwrap()
}
fn divide_zero() -> GraphSetValue {
    GraphSetValue::Integer(GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap())
}

#[test]
fn upstream_data_errors_win_over_earlier_downstream_errors_and_empty_pages() {
    for count in [0, 1, 8] {
        let query = invalid_child().project(
            vec![GraphSetProjection::new("bad", divide_zero())], GraphSetQuantifier::All,
        ).unwrap().with_page(0, Some(count));
        let expected = eager(&query).unwrap_err();
        assert!(matches!(&expected, GqlQueryError::Source(GraphSetExecutionError::Projection {
            row: 1, column: 1, ..
        })));
        assert_eq!(folded(&query).unwrap_err(), expected);
        assert!(!format!("{expected}").contains("private invalid list"));
    }
    let mut calls = 0;
    let error = invalid_child().fold_governed(wide(), no_source, || Ok(()), |_, _| {
        calls += 1;
        Err(GqlQueryError::Source(GraphSetExecutionError::Source("sink failed")))
    }).unwrap_err();
    assert_eq!(calls, 1);
    assert!(matches!(error, GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, .. })));
}

#[test]
fn folding_yields_before_expansion_finishes_and_never_charges_private_result_rows() {
    let values: Vec<_> = (0..32).map(Some).collect();
    let query = PreparedGraphSet::singleton()
        .unwind("a".into(), list(&values)).unwrap()
        .unwind("b".into(), list(&values)).unwrap()
        .unwind("c".into(), list(&values)).unwrap();
    let mut checkpoints = 0;
    let mut visited = 0;
    let result = query.fold_governed(wide(), no_source, || { checkpoints += 1; Ok(()) }, |_, _| {
        visited += 1;
        Err(GqlQueryError::Interrupted(999))
    });
    assert!(matches!(result, Err(GqlQueryError::Interrupted(999))));
    assert_eq!(visited, 1);
    assert!(checkpoints < 1024, "the first row must not wait for the 32768-row expansion");
    let mut count = 0;
    let (rows, _) = query.fold_governed(GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
        no_source, || Ok(()), |_, _| { count += 1; Ok(()) }).unwrap();
    assert_eq!(count, 32 * 32 * 32);
    assert_eq!(rows.result_rows, 0);
}

#[test]
fn every_checkpoint_and_exact_work_scratch_limits_are_fail_stop() {
    let query = PreparedGraphSet::singleton().unwind("x".into(), list(&[Some(2), None, Some(2)])).unwrap()
        .unwind("y".into(), list(&[Some(-1), Some(3)])).unwrap().with_page(1, Some(3));
    let expected = folded(&query).unwrap();
    let mut calls = 0;
    let (_, used) = query.fold_governed(wide(), no_source, || { calls += 1; Ok(()) }, |_, _| Ok(())).unwrap();
    let exact = GqlQueryPolicy::new(0, 0, used.work_units, used.scratch_entries);
    assert!(query.fold_governed(exact, no_source, || Ok(()), |_, _| Ok(())).is_ok());
    for stop in 1..=calls {
        let mut seen = 0;
        let mut prefix = Vec::new();
        let result = query.fold_governed(exact, no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }, |row, _| { prefix.push(row); Ok(()) });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
        assert!(expected.starts_with(&prefix));
    }
    for policy in [GqlQueryPolicy::new(0, 0, used.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 0, u64::MAX, used.scratch_entries - 1)]
    {
        assert!(matches!(query.fold_governed(policy, no_source, || Ok(()), |_, _| Ok(())),
            Err(GqlQueryError::Evaluator(_))));
    }
}

#[test]
fn sorting_and_distinct_barriers_remain_in_the_original_executor() {
    let input = PreparedGraphSet::singleton().unwind("x".into(), list(&[Some(3), Some(1), Some(3)])).unwrap();
    let distinct = input.clone().project(vec![GraphSetProjection::new("x", GraphSetValue::Column(0))],
        GraphSetQuantifier::Distinct).unwrap();
    assert!(!distinct.has_foldable_expansion());
    let query = distinct.unwind("y".into(), list(&[Some(4), Some(-1)])).unwrap();
    assert_eq!(folded(&query).unwrap(), eager(&query).unwrap().value);
    let ranked = query.with_order_by(&[GraphValueOrder { column: 1, descending: true, nulls_first: false }]).unwrap();
    assert!(!ranked.has_foldable_expansion());
    assert_eq!(folded(&ranked).unwrap(), eager(&ranked).unwrap().value);
}

#[test]
fn all_graph_children_are_observed_once_even_with_an_empty_left_relation() {
    let leaf = |name: &str| {
        let mut builder = crate::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(builder.prepare_values(&[crate::algebra::GraphColumn::vertex(name, "n")], 0, None).unwrap())
    };
    let query = leaf("left").cross_join(leaf("right")).unwrap().with_page(0, Some(0));
    let mut calls = 0;
    let result = query.fold_governed(wide(), |_, _| {
        calls += 1;
        if calls == 2 { return Err(GqlQueryError::Source("right failed")); }
        Ok(GqlQueryExecution {
            value: vec![], rows: GqlExecutionStats { snapshot_records: 2, result_rows: 0 },
            evaluator: GlaExecutionStats::default(),
        })
    }, || Ok::<_, usize>(()), |_, _| panic!("empty product"));
    assert_eq!(calls, 2);
    assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Source("right failed")))));
}
