//! Compound inputs are complete relations, never independently summarized arms.
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow,
    IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateOrder, GraphAggregateRow, GraphExactAverage, GraphHavingExpression,
    GraphHavingOp, GraphHavingOperand, GraphSetExecutionError, GraphSetOperation,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, PreparedGraphAggregate,
    PreparedGraphSet, PreparedGraphSetAggregate, MAX_GRAPH_SET_DEPTH,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

const P: PropertyKeyId = PropertyKeyId(1);
const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
type Datum = (LabelId, CanonicalScalar);
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn leaf(label: LabelId) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::HasLabel(label)).unwrap();
    builder.prepare_values(&[GraphColumn::property("x", "n", P)], 0, None)
        .unwrap().with_duplicates()
}
fn compound(operation: GraphSetOperation, quantifier: GraphSetQuantifier) -> PreparedGraphSet {
    PreparedGraphSet::from(leaf(A)).combine(operation, quantifier, leaf(B).into()).unwrap()
}
fn summary(input: PreparedGraphSet) -> PreparedGraphSetAggregate {
    PreparedGraphSetAggregate::prepare(input, &[], &[
        GraphAggregate::count_rows("rows"), GraphAggregate::count("nonnull", 0),
        GraphAggregate::count_distinct("unique", 0), GraphAggregate::sum_int("sum", 0),
        GraphAggregate::min("min", 0), GraphAggregate::max("max", 0),
        GraphAggregate::sum_int_distinct("unique_sum", 0), GraphAggregate::average_int("avg", 0),
        GraphAggregate::average_int_distinct("unique_avg", 0),
    ], 0, None).unwrap()
}
fn source<C>(pattern: &PreparedGraphPattern<GraphValueRow>, data: &[Datum],
    policy: GqlQueryPolicy, checkpoint: impl FnMut() -> Result<(), C>)
    -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, C>> {
    pattern.plan().execute_governed_with_properties(
        data.len() as u64, (0..data.len()).map(|i| VId(i as u128)), [],
        |vid, tests| {
            let (label, value) = &data[vid.0 as usize];
            Ok(tests.iter().all(|test| test.matches(&[*label], &[(P, value.clone())])))
        },
        |vid, _| Ok(Some(&data[vid.0 as usize].1)), policy, checkpoint,
    )
}
fn run<C>(query: &PreparedGraphSetAggregate, data: &[Datum], policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>)
    -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<&'static str>, C>> {
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(policy,
        |pattern, remaining| source(pattern, data, remaining, || (*checkpoint.borrow_mut())()),
        || (*checkpoint.borrow_mut())())
}
fn assert_summary(row: &GraphAggregateRow, expected: &[CanonicalScalar]) {
    let integers = expected.iter().filter_map(|value| match value {
        CanonicalScalar::Int(n) => Some(*n), _ => None,
    }).collect::<Vec<_>>();
    let unique = integers.iter().copied().collect::<BTreeSet<_>>();
    let cells = row.values();
    assert_eq!(cells[0].as_count(), Some(expected.len() as u64));
    assert_eq!(cells[1].as_count(), Some(integers.len() as u64));
    assert_eq!(cells[2].as_count(), Some(unique.len() as u64));
    if integers.is_empty() {
        assert!(cells[3..].iter().all(|cell| cell.is_null()));
    } else {
        let sum = integers.iter().copied().map(i128::from).sum::<i128>();
        let unique_sum = unique.iter().copied().map(i128::from).sum::<i128>();
        assert_eq!(cells[3].as_integer(), Some(sum));
        assert_eq!(cells[4].as_value().unwrap().as_scalar(), Some(&CanonicalScalar::Int(*integers.iter().min().unwrap())));
        assert_eq!(cells[5].as_value().unwrap().as_scalar(), Some(&CanonicalScalar::Int(*integers.iter().max().unwrap())));
        assert_eq!(cells[6].as_integer(), Some(unique_sum));
        assert_eq!(cells[7].as_average(), GraphExactAverage::new(sum, integers.len() as u64));
        assert_eq!(cells[8].as_average(), GraphExactAverage::new(unique_sum, unique.len() as u64));
    }
}

#[test]
fn every_set_quantifier_and_page_matches_independent_bag_arithmetic() {
    // Exhaust all four-row assignments of NULL and two signed integers. The
    // oracle counts raw occurrences; it calls no set, graph or aggregate engine.
    for mut encoded in 0..81 {
        let mut data = Vec::new();
        for index in 0..4 {
            let value = match encoded % 3 {
                0 => CanonicalScalar::Null, 1 => CanonicalScalar::Int(-2), _ => CanonicalScalar::Int(5),
            };
            encoded /= 3;
            data.push((if index < 2 { A } else { B }, value));
        }
        let mut counts: BTreeMap<CanonicalScalar, [usize; 2]> = BTreeMap::new();
        for (label, value) in &data {
            counts.entry(value.clone()).or_default()[usize::from(*label == B)] += 1;
        }
        for operation in [GraphSetOperation::Union, GraphSetOperation::Intersect, GraphSetOperation::Except] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let mut expected = Vec::new();
                for (value, &[left, right]) in &counts {
                    let multiplicity = match (operation, quantifier) {
                        (GraphSetOperation::Union, GraphSetQuantifier::All) => left + right,
                        (GraphSetOperation::Intersect, GraphSetQuantifier::All) => left.min(right),
                        (GraphSetOperation::Except, GraphSetQuantifier::All) => left.saturating_sub(right),
                        (GraphSetOperation::Union, _) => usize::from(left + right != 0),
                        (GraphSetOperation::Intersect, _) => usize::from(left != 0 && right != 0),
                        (GraphSetOperation::Except, _) => usize::from(left != 0 && right == 0),
                    };
                    expected.extend(std::iter::repeat_n(value.clone(), multiplicity));
                }
                for (offset, count) in [(0, None), (1, Some(2)), (0, Some(0))] {
                    let query = summary(compound(operation, quantifier).with_page(offset, count));
                    let result = run(&query, &data, wide(), || Ok::<_, ()>(())).unwrap();
                    let page = expected.iter().skip(offset as usize)
                        .take(count.unwrap_or(u64::MAX) as usize).cloned().collect::<Vec<_>>();
                    assert_eq!(result.rows.snapshot_records, 8);
                    assert_eq!(result.rows.result_rows, 1);
                    assert_summary(&result.value[0], &page);
                }
            }
        }
    }
}

#[test]
fn final_relation_schema_and_exact_numeric_domains_do_not_come_from_the_first_leaf() {
    let value = GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap();
    let input = compound(GraphSetOperation::Union, GraphSetQuantifier::All)
        .project(vec![GraphSetProjection::new("x", GraphSetValue::Column(0)),
            GraphSetProjection::new("bucket", GraphSetValue::Literal(value))], GraphSetQuantifier::All).unwrap();
    let query = PreparedGraphSetAggregate::prepare(input, &[1], &[
        GraphAggregate::sum_int("sum", 0), GraphAggregate::average_int("avg", 0),
    ], 0, None).unwrap();
    let data = [(A, CanonicalScalar::Int(i64::MAX)), (B, CanonicalScalar::Int(i64::MAX)),
        (B, CanonicalScalar::Int(i64::MIN))];
    let result = run(&query, &data, wide(), || Ok::<_, ()>(())).unwrap();
    let sum = i128::from(i64::MAX) * 2 + i128::from(i64::MIN);
    assert_eq!(query.key_columns(), &["bucket"]);
    assert_eq!(result.value[0].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(7)));
    assert_eq!(result.value[0].values()[0].as_integer(), Some(sum));
    assert_eq!(result.value[0].values()[1].as_average(), GraphExactAverage::new(sum, 3));
}

#[test]
fn grouped_results_reuse_having_hidden_keys_order_distinct_and_output_prefix() {
    let input = compound(GraphSetOperation::Union, GraphSetQuantifier::All);
    let query = PreparedGraphSetAggregate::prepare(input, &[0], &[GraphAggregate::count_rows("n")], 0, None).unwrap();
    let having = GraphHavingExpression::prepare(&[GraphHavingOp::Compare {
        left: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(0)),
        comparison: IntegerComparison::GreaterOrEqual, right: GraphHavingOperand::Integer(2),
    }]).unwrap();
    let hidden = query.clone().with_key_output_columns(&[]).unwrap()
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::GroupKey(0))]).unwrap()
        .with_having_expression(&having).unwrap().with_distinct_output(true);
    let data = [(A, CanonicalScalar::Int(1)), (A, CanonicalScalar::Int(2)),
        (B, CanonicalScalar::Int(1)), (B, CanonicalScalar::Int(2))];
    let result = run(&hidden, &data, wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 1);
    assert!(result.value[0].keys().is_empty());
    assert_eq!(result.value[0].values()[0].as_count(), Some(2));
    assert_eq!(hidden.evaluation_key_columns(), query.key_columns());
    assert_eq!(hidden.evaluation_aggregate_columns(), query.aggregate_columns());
    let empty = hidden.with_aggregate_output_prefix(0).unwrap();
    let result = run(&empty, &data, wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 1);
    assert!(result.value[0].values().is_empty());
}

#[test]
fn all_sources_and_grouping_share_exact_caps_and_every_cancellation_checkpoint() {
    let query = summary(compound(GraphSetOperation::Union, GraphSetQuantifier::All));
    let data = [(A, CanonicalScalar::Int(2)), (B, CanonicalScalar::Int(5))];
    let checkpoints = Cell::new(0);
    let measured = run(&query, &data, wide(), || {
        checkpoints.set(checkpoints.get() + 1); Ok::<_, usize>(())
    }).unwrap();
    let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
        measured.evaluator.work_units, measured.evaluator.scratch_entries];
    assert_eq!(caps[0], 4);
    assert_eq!(caps[1], 1, "intermediate rows are not public aggregate rows");
    assert_eq!(run(&query, &data, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), || Ok::<_, ()>(())).unwrap(), measured);
    for dimension in 0..4 {
        let mut cap = caps; cap[dimension] -= 1;
        let error = run(&query, &data, GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]), || Ok::<_, ()>(())).unwrap_err();
        match error {
            GqlQueryError::Rows(error) if dimension < 2 => assert_eq!(error.limit, cap[dimension]),
            GqlQueryError::Evaluator(error) if dimension >= 2 => assert_eq!(error.limit, cap[dimension]),
            other => panic!("unexpected budget refusal: {other:?}"),
        }
    }
    for stop in 1..=checkpoints.get() {
        let mut calls = 0;
        let result = run(&query, &data, wide(), || {
            calls += 1; if calls == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
}

#[test]
fn empty_left_and_zero_group_limit_do_not_mask_a_later_source_failure() {
    for operation in [GraphSetOperation::Union, GraphSetOperation::Intersect, GraphSetOperation::Except] {
        let query = PreparedGraphSetAggregate::prepare(compound(operation, GraphSetQuantifier::All),
            &[], &[GraphAggregate::count_rows("n")], 0, Some(0)).unwrap();
        let mut calls = 0;
        let result = query.execute_governed(wide(), |pattern, allowance| {
            calls += 1;
            if calls == 2 { return Err(GqlQueryError::Source("second graph failed")); }
            assert_eq!(pattern, &leaf(A));
            source(pattern, &[], allowance, || Ok::<_, ()>(()))
        }, || Ok::<_, ()>(()));
        assert_eq!(calls, 2);
        let error = match result { Err(GqlQueryError::Source(error)) => error, other => panic!("{other:?}") };
        assert!(matches!(error.map_source(|message| message.len()),
            GraphAggregateError::InputRelation(GraphSetExecutionError::Source(19))));
    }
}

#[test]
fn late_source_statistics_and_schema_refusals_keep_the_operand_index() {
    let query = summary(compound(GraphSetOperation::Union, GraphSetQuantifier::All));
    let data = [(A, CanonicalScalar::Int(2)), (B, CanonicalScalar::Int(5))];
    for schema_error in [false, true] {
        let mut calls = 0;
        let result = query.execute_governed(wide(), |pattern, remaining| {
            calls += 1;
            if calls == 2 && schema_error {
                let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
                let wrong = builder.prepare_values(&[GraphColumn::vertex("x", "n")], 0, None).unwrap();
                return source(&wrong, &data, remaining, || Ok::<_, ()>(()));
            }
            let mut result = source(pattern, &data, remaining, || Ok::<_, ()>(()))?;
            if calls == 2 { result.rows.result_rows += 1; }
            Ok(result)
        }, || Ok::<_, ()>(()));
        assert_eq!(calls, 2);
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::InputRelation(error)))
            if if schema_error { matches!(error, GraphSetExecutionError::InputSchema { operand: 1 }) }
                else { matches!(error, GraphSetExecutionError::InvalidSourceStatistics { operand: 1 }) }));
    }
}

#[test]
fn single_source_contract_transcripts_and_definition_bounds_remain_intact() {
    let input: PreparedGraphSet = leaf(A).into();
    let aggregates = [GraphAggregate::count_rows("count")];
    let old = PreparedGraphAggregate::prepare_relation(input.clone(), &[], &aggregates, 0, None).unwrap();
    let new = PreparedGraphSetAggregate::prepare(input.clone(), &[], &aggregates, 0, None).unwrap();
    assert_eq!(new.canonical_bytes(), old.canonical_bytes());
    let data = [(A, CanonicalScalar::Int(4))];
    let ordinary = old.execute_governed(1, [VId(0)], [], |_, _| Ok::<_, &str>(true),
        |_, _| Ok(Some(&data[0].1)), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(run(&new, &data, wide(), || Ok::<_, ()>(())).unwrap(), ordinary);
    let binary = compound(GraphSetOperation::Union, GraphSetQuantifier::All);
    assert!(matches!(PreparedGraphAggregate::prepare_relation(binary.clone(), &[], &aggregates, 0, None),
        Err(GraphAggregateBuildError::RequiresSingleGraphSource)));
    let new = PreparedGraphSetAggregate::prepare(binary, &[], &aggregates, 0, None).unwrap();
    assert_eq!(new.input().operand_count(), 2);
    assert_ne!(new.canonical_bytes(), old.canonical_bytes());
    assert!(!format!("{new:?}").contains("count"));
    assert!(matches!(PreparedGraphSetAggregate::prepare(input.clone(), &[1], &aggregates, 0, None),
        Err(GraphAggregateBuildError::UnknownColumn { column: 1 })));
    let mut deepest = input;
    for _ in 1..MAX_GRAPH_SET_DEPTH { deepest = deepest.nested().unwrap(); }
    assert!(matches!(PreparedGraphSetAggregate::prepare(deepest, &[], &aggregates, 0, None),
        Err(GraphAggregateBuildError::RelationalInput(_))));
}

#[test]
fn input_ranking_and_group_ranking_are_separate_and_keyed_empty_sets_stay_empty() {
    let input = compound(GraphSetOperation::Union, GraphSetQuantifier::All)
        .with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(1, Some(2));
    let query = PreparedGraphSetAggregate::prepare(input, &[0], &[GraphAggregate::count_rows("n")], 0, None).unwrap();
    let data = [(A, CanonicalScalar::Int(1)), (A, CanonicalScalar::Int(5)), (B, CanonicalScalar::Int(9))];
    let rows = run(&query, &data, wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(1)));
    assert_eq!(rows[1].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(5)));
    let empty = PreparedGraphSetAggregate::prepare(compound(GraphSetOperation::Except, GraphSetQuantifier::Distinct),
        &[0], &[GraphAggregate::count_rows("n")], 0, None).unwrap();
    let data = [(A, CanonicalScalar::Int(1)), (B, CanonicalScalar::Int(1))];
    assert!(run(&empty, &data, wide(), || Ok::<_, ()>(())).unwrap().value.is_empty());
}
