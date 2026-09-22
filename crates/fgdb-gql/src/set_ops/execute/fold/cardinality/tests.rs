use super::*;
use crate::{GraphSetOperand, GraphSetPredicateOp, GraphSetProjection};
use fgdb_types::CanonicalScalar;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free relation opened a graph")
}
fn factor(n: usize) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                (0..n)
                    .map(|i| {
                        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(i as i64)))
                    })
                    .collect(),
            ),
        )
        .unwrap()
}
fn size(query: &PreparedGraphSet) -> Option<u64> {
    let (count, rows, _) = query.count_governed(wide(), no_source, || Ok(())).unwrap();
    assert_eq!(rows.result_rows, 0);
    assert_eq!(rows.snapshot_records, 0);
    count
}
fn power(mut input: PreparedGraphSet, squarings: usize) -> PreparedGraphSet {
    for _ in 0..squarings {
        input = input.clone().cross_join(input).unwrap();
    }
    input
}

#[test]
fn product_union_all_and_nested_pages_match_the_materialized_executor() {
    for a in 0..5 {
        for b in 0..5 {
            let product = factor(a).cross_join(factor(b)).unwrap();
            let union = product
                .clone()
                .combine(
                    GraphSetOperation::Union,
                    GraphSetQuantifier::All,
                    product.clone(),
                )
                .unwrap();
            for input in [product, union] {
                for skip in [0, 1, 6, u64::MAX] {
                    for limit in [None, Some(0), Some(1), Some(7)] {
                        let query = input
                            .clone()
                            .with_page(1, Some(11))
                            .nested()
                            .unwrap()
                            .with_order_by(&[GraphValueOrder::descending(0)])
                            .unwrap()
                            .with_page(skip, limit);
                        let transcript = query.canonical_bytes();
                        let expected = query
                            .execute_governed(wide(), no_source, || Ok(()))
                            .unwrap();
                        assert_eq!(size(&query), Some(expected.value.len() as u64));
                        assert_eq!(query.canonical_bytes(), transcript);
                    }
                }
            }
        }
    }
}

#[test]
fn oversized_intermediate_counts_can_page_back_into_range_or_be_annihilated() {
    let exactly_two_to_64 = power(factor(16), 4);
    assert_eq!(size(&exactly_two_to_64), None);
    assert_eq!(
        size(&exactly_two_to_64.clone().with_page(u64::MAX, None)),
        Some(1)
    );
    assert_eq!(
        size(&exactly_two_to_64.clone().with_page(u64::MAX, Some(7))),
        Some(1)
    );
    let enormous = power(factor(256), 5); // 2^256, well beyond u128.
    assert_eq!(size(&enormous), None);
    assert_eq!(
        size(&enormous.clone().with_page(u64::MAX, Some(7))),
        Some(7)
    );
    assert_eq!(
        size(&enormous.clone().cross_join(factor(0)).unwrap()),
        Some(0)
    );
    assert_eq!(size(&factor(0).cross_join(enormous).unwrap()), Some(0));
}

#[test]
fn work_tracks_factors_not_cartesian_pairs() {
    // Eight 16-row factors represent 2^32 pairs using only 128 leaf rows.
    let query = power(factor(16), 3);
    let policy = GqlQueryPolicy::new(0, 0, 100_000, 20_000);
    let (count, rows, used) = query.count_governed(policy, no_source, || Ok(())).unwrap();
    assert_eq!(count, Some(1_u64 << 32));
    assert_eq!(rows.result_rows, 0);
    assert!(used.work_units < 100_000);
    assert!(used.scratch_entries < 20_000);
}

#[test]
fn sorting_before_value_sensitive_filters_and_distinct_is_not_elided() {
    let input = PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(3))),
            ]),
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(0, Some(1));
    let filtered = input
        .filter(&[GraphSetPredicateOp::IsNull {
            operand: GraphSetOperand::Column(0),
            is_null: true,
        }])
        .unwrap();
    assert_eq!(size(&filtered.cross_join(factor(9)).unwrap()), Some(0));
    let distinct = factor(4)
        .project(
            vec![GraphSetProjection::new(
                "same",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
            )],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    assert_eq!(size(&distinct.cross_join(factor(9)).unwrap()), Some(9));
}

#[test]
fn late_source_errors_survive_empty_and_oversized_left_factors_and_limit_zero() {
    let mut builder = crate::algebra::GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let leaf = PreparedGraphSet::from(
        builder
            .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
            .unwrap(),
    );
    for left in [factor(0), power(factor(256), 5)] {
        let query = left.cross_join(leaf.clone()).unwrap().with_page(0, Some(0));
        let mut calls = 0;
        let result = query.count_governed(
            wide(),
            |_, _| {
                calls += 1;
                Err(GqlQueryError::Source("right failed"))
            },
            || Ok::<_, usize>(()),
        );
        assert_eq!(calls, 1);
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "right failed"
            )))
        ));
    }
}

#[test]
fn exact_resource_limits_and_every_cancellation_checkpoint_are_fail_stop() {
    let query = factor(2).cross_join(factor(3)).unwrap();
    let mut calls = 0;
    let (expected, _, used) = query
        .count_governed(wide(), no_source, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let exact = GqlQueryPolicy::new(0, 0, used.work_units, used.scratch_entries);
    assert_eq!(
        query.count_governed(exact, no_source, || Ok(())).unwrap().0,
        expected
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.count_governed(exact, no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    for policy in [
        GqlQueryPolicy::new(0, 0, used.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 0, u64::MAX, used.scratch_entries - 1),
    ] {
        assert!(matches!(
            query.count_governed(policy, no_source, || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
}
