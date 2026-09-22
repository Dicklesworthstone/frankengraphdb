use super::*;
use crate::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphSetProjection, GraphSetValue,
};
use crate::algebra::IntegerComparison;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free aggregate opened a graph")
}
fn factor(n: usize) -> PreparedGraphSet {
    PreparedGraphSet::singleton().unwind("x".into(), GraphSetValue::List(
        (0..n).map(|i| GraphSetValue::Value(GraphValue::Scalar(
            CanonicalScalar::Int(i as i64),
        ))).collect(),
    )).unwrap()
}
fn query(input: PreparedGraphSet, count: Option<u64>) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_set_relation(input, &[], &[
        GraphAggregate::count_rows("n"), GraphAggregate::count_rows("same"),
    ], 0, count).unwrap()
}

#[test]
fn repeated_count_slots_reuse_having_projection_distinct_and_final_paging() {
    for a in 0..5 {
        for b in 0..5 {
            for count in [None, Some(0), Some(1)] {
                let q = query(factor(a).cross_join(factor(b)).unwrap(), count)
                    .with_result_clauses(&[GraphAggregateFilter {
                        column: GraphAggregateColumn::Aggregate(0),
                        test: GraphAggregateTest::Integer {
                            comparison: IntegerComparison::Greater, value: 2,
                        },
                    }], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1))]).unwrap()
                    .with_output_projection(vec![GraphSetProjection::new("n",
                        GraphSetValue::Column(1)),
                    ]).unwrap().with_distinct_output(true);
                assert!(q.uses_factorized_cardinality());
                let transcript = q.canonical_bytes();
                let actual = q.execute_relational_with_source(wide(), no_source, || Ok(())).unwrap();
                let expected = q.execute_relational_materialized(wide(), no_source, || Ok(())).unwrap();
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.rows.result_rows, actual.value.len() as u64);
                assert_eq!(q.canonical_bytes(), transcript);
            }
        }
    }
}

#[test]
fn trillion_pair_count_finishes_under_factor_sized_allowances() {
    let mut relation = factor(32);
    for _ in 0..3 {
        relation = relation.clone().cross_join(relation).unwrap();
    }
    let q = query(relation, None); // 32^8 = 2^40
    let result = q.execute_relational_with_source(
        GqlQueryPolicy::new(0, 1, 100_000, 20_000), no_source, || Ok(()),
    ).unwrap();
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(result.value[0].values()[0].as_count(), Some(1_u64 << 40));
    assert_eq!(result.value[0].values()[1].as_count(), Some(1_u64 << 40));
}

#[test]
fn input_page_can_recover_from_overflow_but_output_limit_cannot_hide_it() {
    let mut relation = factor(16);
    for _ in 0..4 {
        relation = relation.clone().cross_join(relation).unwrap();
    }
    for count in [None, Some(0), Some(1)] {
        let q = query(relation.clone(), count);
        assert!(matches!(q.execute_relational_with_source(wide(), no_source, || Ok(())),
            Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
    }
    let q = query(relation.clone().with_page(u64::MAX, None), None);
    let result = q.execute_relational_with_source(wide(), no_source, || Ok(())).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(1));
    let q = query(relation.cross_join(factor(0)).unwrap(), None);
    let result = q.execute_relational_with_source(wide(), no_source, || Ok(())).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(0));
}

#[test]
fn final_row_allowance_and_every_result_checkpoint_share_the_original_meter() {
    let q = query(factor(2).cross_join(factor(3)).unwrap(), None);
    let mut calls = 0;
    let baseline = q.execute_relational_with_source(wide(), no_source, || {
        calls += 1;
        Ok(())
    }).unwrap();
    let exact = GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units,
        baseline.evaluator.scratch_entries);
    assert_eq!(q.execute_relational_with_source(exact, no_source, || Ok(())).unwrap().value,
        baseline.value);
    for stop in 1..=calls {
        let mut seen = 0;
        let result = q.execute_relational_with_source(exact, no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    assert!(matches!(q.execute_relational_with_source(
        GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX), no_source, || Ok(())),
        Err(GqlQueryError::Rows(_))));
}

#[test]
fn grouped_or_value_dependent_aggregates_keep_row_execution() {
    let relation = factor(2).cross_join(factor(3)).unwrap();
    let grouped = PreparedGraphAggregate::prepare_set_relation(relation.clone(), &[0],
        &[GraphAggregate::count_rows("n")], 0, None).unwrap();
    let value_dependent = PreparedGraphAggregate::prepare_set_relation(relation, &[],
        &[GraphAggregate::count("n", 1)], 0, None).unwrap();
    for q in [grouped, value_dependent] {
        assert!(!q.uses_factorized_cardinality());
        assert_eq!(q.execute_relational_with_source(wide(), no_source, || Ok(())).unwrap().value,
            q.execute_relational_materialized(wide(), no_source, || Ok(())).unwrap().value);
    }
}
