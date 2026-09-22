use super::*;
use crate::algebra::IntegerComparison;
use crate::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphSetProjection, GraphSetValue,
};

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
fn query(input: PreparedGraphSet, count: Option<u64>) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_set_relation(
        input,
        &[],
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::count_rows("same"),
        ],
        0,
        count,
    )
    .unwrap()
}

#[test]
fn repeated_count_slots_reuse_having_projection_distinct_and_final_paging() {
    for a in 0..5 {
        for b in 0..5 {
            for count in [None, Some(0), Some(1)] {
                let q = query(factor(a).cross_join(factor(b)).unwrap(), count)
                    .with_result_clauses(
                        &[GraphAggregateFilter {
                            column: GraphAggregateColumn::Aggregate(0),
                            test: GraphAggregateTest::Integer {
                                comparison: IntegerComparison::Greater,
                                value: 2,
                            },
                        }],
                        &[GraphAggregateOrder::descending(
                            GraphAggregateColumn::Aggregate(1),
                        )],
                    )
                    .unwrap()
                    .with_output_projection(vec![GraphSetProjection::new(
                        "n",
                        GraphSetValue::Column(1),
                    )])
                    .unwrap()
                    .with_distinct_output(true);
                assert!(q.uses_factorized_cardinality());
                let transcript = q.canonical_bytes();
                let actual = q
                    .execute_relational_with_source(wide(), no_source, || Ok(()))
                    .unwrap();
                let expected = q
                    .execute_relational_materialized(wide(), no_source, || Ok(()))
                    .unwrap();
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
    let result = q
        .execute_relational_with_source(
            GqlQueryPolicy::new(0, 1, 100_000, 20_000),
            no_source,
            || Ok(()),
        )
        .unwrap();
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
        assert!(matches!(
            q.execute_relational_with_source(wide(), no_source, || Ok(())),
            Err(GqlQueryError::Source(
                GraphAggregateError::ArithmeticOverflow { aggregate: 0 }
            ))
        ));
    }
    let q = query(relation.clone().with_page(u64::MAX, None), None);
    let result = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(1));
    let q = query(relation.cross_join(factor(0)).unwrap(), None);
    let result = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(0));
}

#[test]
fn final_row_allowance_and_every_result_checkpoint_share_the_original_meter() {
    let q = query(factor(2).cross_join(factor(3)).unwrap(), None);
    let mut calls = 0;
    let baseline = q
        .execute_relational_with_source(wide(), no_source, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let exact = GqlQueryPolicy::new(
        0,
        1,
        baseline.evaluator.work_units,
        baseline.evaluator.scratch_entries,
    );
    assert_eq!(
        q.execute_relational_with_source(exact, no_source, || Ok(()))
            .unwrap()
            .value,
        baseline.value
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = q.execute_relational_with_source(exact, no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    assert!(matches!(
        q.execute_relational_with_source(
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
            no_source,
            || Ok(())
        ),
        Err(GqlQueryError::Rows(_))
    ));
}

#[test]
fn grouped_or_value_dependent_aggregates_keep_row_execution() {
    let relation = factor(2).cross_join(factor(3)).unwrap();
    let grouped = PreparedGraphAggregate::prepare_set_relation(
        relation.clone(),
        &[0],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    let value_dependent = PreparedGraphAggregate::prepare_set_relation(
        relation,
        &[],
        &[GraphAggregate::count("n", 1)],
        0,
        None,
    )
    .unwrap();
    for q in [grouped, value_dependent] {
        assert!(!q.uses_factorized_cardinality());
        assert_eq!(
            q.execute_relational_with_source(wide(), no_source, || Ok(()))
                .unwrap()
                .value,
            q.execute_relational_materialized(wide(), no_source, || Ok(()))
                .unwrap()
                .value
        );
    }
}

#[test]
fn constant_unwind_pipeline_with_with_aliases_uses_the_integrated_count_path() {
    let mut input = PreparedGraphSet::singleton();
    for at in 0..8 {
        input = input
            .unwind(
                format!("x{at}"),
                GraphSetValue::List(
                    (0..32)
                        .map(|i| GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(i))))
                        .collect(),
                ),
            )
            .unwrap();
    }
    let input = input
        .project(
            vec![GraphSetProjection::new("last", GraphSetValue::Column(7))],
            crate::GraphSetQuantifier::All,
        )
        .unwrap();
    let q = query(input, None);
    assert!(q.uses_factorized_cardinality());
    let result = q
        .execute_relational_with_source(GqlQueryPolicy::new(0, 1, 20_000, 5_000), no_source, || {
            Ok(())
        })
        .unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(1_u64 << 40));
    assert_eq!(result.rows.result_rows, 1);
}

#[test]
fn graph_backed_constant_expansions_admit_the_source_once_and_keep_snapshot_limits() {
    let mut builder = crate::algebra::GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let leaf = PreparedGraphSet::from(
        builder
            .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
            .unwrap(),
    );
    let input = leaf
        .unwind(
            "a".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
            ]),
        )
        .unwrap();
    let q = PreparedGraphAggregate::prepare_relation(
        input,
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    let calls = std::cell::Cell::new(0);
    let source = |_: &PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy| {
        calls.set(calls.get() + 1);
        Ok::<_, GqlQueryError<&'static str, usize>>(GqlQueryExecution {
            value: vec![
                GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]),
                GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(2))]),
            ],
            rows: GqlExecutionStats {
                snapshot_records: 2,
                result_rows: 2,
            },
            evaluator: GlaExecutionStats::default(),
        })
    };
    let result = q
        .execute_relational_with_source(GqlQueryPolicy::new(2, 1, 100_000, 10_000), source, || {
            Ok(())
        })
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(result.rows.snapshot_records, 2);
    assert_eq!(result.value[0].values()[0].as_count(), Some(4));
    let result = q.execute_relational_with_source(
        GqlQueryPolicy::new(1, 1, 100_000, 10_000),
        source,
        || Ok(()),
    );
    assert_eq!(calls.get(), 2);
    assert!(matches!(result, Err(GqlQueryError::Rows(error))
        if error.dimension == GqlBudgetDimension::SnapshotRecords));
}
