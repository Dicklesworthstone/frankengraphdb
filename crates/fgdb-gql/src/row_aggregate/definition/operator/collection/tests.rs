use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryExecution, GqlQueryPolicy, GraphAggregate,
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateTest, GraphSetValue,
    PreparedGraphAggregate, PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::VId;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Operator = IncrementalGroupAggregate<PreparedGraphSetAggregate>;
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(g: i64, rank: Option<i64>, value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![scalar(Some(g)), scalar(rank), scalar(value)])
}
fn source() -> PreparedGraphSet {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.prepare_values(&[
        GraphColumn::property("g", "n", PropertyKeyId(1)),
        GraphColumn::property("rank", "n", PropertyKeyId(2)),
        GraphColumn::property("value", "n", PropertyKeyId(3)),
    ], 0, None).unwrap().with_duplicates().into()
}
fn definition(global: bool, ordered: bool, having: bool) -> PreparedGraphSetAggregate {
    let input = if ordered {
        source().with_order_by(&[GraphValueOrder::descending(1).with_nulls_first(true)]).unwrap()
    } else { source() };
    let query = PreparedGraphSetAggregate::prepare(input, if global { &[] } else { &[0] }, &[
        GraphAggregate::collect("values", 2),
        GraphAggregate::collect_distinct("distinct_values", 2),
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("nonnull", 2),
        GraphAggregate::sum_int("sum", 2),
    ], 0, None).unwrap();
    if having {
        query.with_result_clauses(&[GraphAggregateFilter {
            column: GraphAggregateColumn::Aggregate(2),
            test: GraphAggregateTest::Integer {
                comparison: crate::algebra::IntegerComparison::GreaterOrEqual, value: 2,
            },
        }], &[]).unwrap()
    } else { query }
}
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(row, weight)| (row, ZWeight::from_i128(weight))),
        LIMBS, &mut allow).unwrap()
}
fn seed(definition: PreparedGraphSetAggregate, rows: &ZSet<GraphValueRow>) -> Operator {
    let schema = definition.input().column_types().to_vec();
    let mut operator = Operator::new(definition, &schema).unwrap();
    operator.prepare(rows, LIMBS, None, &mut allow).unwrap().commit();
    operator
}
fn bag(mut code: usize) -> ZSet<GraphValueRow> {
    z([(0, None, Some(7)), (0, Some(1), Some(3)), (0, Some(2), Some(7)), (1, None, None)]
        .into_iter().map(|(g, rank, value)| {
            let weight = code % 3;
            code /= 3;
            (row(g, rank, value), weight as i128)
        }))
}
// Primitive occurrence oracle: no production collection index, row comparator,
// aggregate accumulator or materialize_incremental_row is used here.
fn oracle(rows: &ZSet<GraphValueRow>, global: bool, ordered: bool, having: bool) -> ZSet<GraphAggregateRow> {
    let mut input = Vec::new();
    for (row, weight) in rows.iter() {
        let value = |at| match row.values()[at] {
            GraphValue::Scalar(CanonicalScalar::Null) => None,
            GraphValue::Scalar(CanonicalScalar::Int(n)) => Some(n),
            _ => panic!("primitive fixture"),
        };
        let tuple = (value(0).unwrap(), value(1), value(2));
        input.extend(std::iter::repeat_n(tuple, weight.to_i128().unwrap() as usize));
    }
    input.sort_by(|a, b| {
        let order = if ordered {
            match (a.1, b.1) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(a), Some(b)) => b.cmp(&a),
            }
        } else { Ordering::Equal };
        order.then_with(|| a.cmp(b))
    });
    let mut groups = BTreeMap::<i64, (u64, Vec<i64>)>::new();
    if global { groups.insert(0, (0, Vec::new())); }
    for (g, _, value) in input {
        let group = groups.entry(if global { 0 } else { g }).or_default();
        group.0 += 1;
        if let Some(value) = value { group.1.push(value); }
    }
    let mut output = Vec::new();
    for (group, (n, values)) in groups {
        if having && n < 2 { continue; }
        let mut distinct = Vec::new();
        for &value in &values {
            if !distinct.contains(&value) { distinct.push(value); }
        }
        let list = |items: &[i64]| Value::Value(GraphValue::List(
            items.iter().map(|&v| scalar(Some(v))).collect::<Vec<_>>().into_boxed_slice(),
        ));
        let cells = vec![list(&values), list(&distinct), Value::Count(n), Value::Count(values.len() as u64),
            if values.is_empty() { Value::Value(scalar(None)) } else {
                Value::Integer(values.iter().map(|&n| i128::from(n)).sum())
            }];
        output.push((GraphAggregateRow::from_group_values(
            if global { vec![] } else { vec![scalar(Some(group))] }, cells,
        ), ZWeight::ONE));
    }
    ZSet::from_updates(output, LIMBS, &mut allow).unwrap()
}

#[test]
fn collection_transitions_match_independent_ordered_occurrences_and_existing_batch_groups() {
    for ordered in [false, true] {
        for global in [false, true] {
            for having in [false, true] {
                let definition = definition(global, ordered, having);
                for before in [0, 1, 4, 10, 26, 50, 80] {
                    let old = bag(before);
                    for after in 0..81 {
                        let new = bag(after);
                        let mut state = seed(definition.clone(), &old);
                        assert_eq!(state.rows(), &oracle(&old, global, ordered, having));
                        let changes = new.minus(&old, LIMBS, &mut allow).unwrap();
                        let delta = state.prepare(&changes, LIMBS, None, &mut allow).unwrap().commit();
                        let expected = oracle(&new, global, ordered, having);
                        assert_eq!(state.rows(), &expected);
                        assert_eq!(delta, expected.minus(&oracle(&old, global, ordered, having), LIMBS, &mut allow).unwrap());
                        let restored = old.minus(&new, LIMBS, &mut allow).unwrap();
                        state.prepare(&restored, LIMBS, None, &mut allow).unwrap().commit();
                        assert_eq!(state, seed(definition.clone(), &old));
                    }
                }
                for code in 0..81 {
                    let rows = bag(code);
                    let mut expanded = Vec::new();
                    for (row, n) in rows.iter() {
                        expanded.extend(std::iter::repeat_n(row.clone(), n.to_i128().unwrap() as usize));
                    }
                    let len = expanded.len() as u64;
                    let mut once = Some(expanded);
                    let batch = definition.execute_governed(
                        GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000),
                        |_, _| Ok::<_, GqlQueryError<usize, usize>>(GqlQueryExecution {
                            value: once.take().expect("one source"),
                            rows: GqlExecutionStats { snapshot_records: len, result_rows: len },
                            evaluator: GlaExecutionStats::default(),
                        }), || Ok::<_, usize>(()),
                    ).unwrap();
                    let expected = oracle(&rows, global, ordered, having);
                    assert_eq!(batch.value, expected.iter().map(|(row, _)| row.clone()).collect::<Vec<_>>());
                    assert!(once.is_none());
                }
            }
        }
    }
}

#[test]
fn count_neutral_replacement_changes_distinct_first_occurrence_and_dropped_guards_preserve_it() {
    let query = definition(false, false, false);
    let old = z([(row(0, Some(1), Some(10)), 1), (row(0, Some(2), Some(20)), 1), (row(0, Some(3), Some(10)), 1)]);
    let change = z([(row(0, Some(1), Some(10)), -1), (row(0, Some(4), Some(10)), 1)]);
    let mut state = seed(query.clone(), &old);
    assert_eq!(state.rows().iter().next().unwrap().0.values()[1],
        Value::Value(GraphValue::List(vec![scalar(Some(10)), scalar(Some(20))].into_boxed_slice())));
    {
        let pending = state.prepare(&change, LIMBS, None, &mut allow).unwrap();
        assert_eq!(pending.delta().len(), 2);
    }
    assert_eq!(state, seed(query.clone(), &old));
    state.prepare(&change, LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(state.rows().iter().next().unwrap().0.values()[1],
        Value::Value(GraphValue::List(vec![scalar(Some(20)), scalar(Some(10))].into_boxed_slice())));
    let new = old.plus(&change, LIMBS, &mut allow).unwrap();
    assert_eq!(state, seed(query, &new));
}

fn any_definition(distinct: bool) -> PreparedGraphSetAggregate {
    let input = PreparedGraphSet::singleton().unwind("item".into(), GraphSetValue::List(vec![])).unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)]).unwrap();
    PreparedGraphSetAggregate::prepare(input, &[], &[
        if distinct { GraphAggregate::collect_distinct("items", 0) } else { GraphAggregate::collect("items", 0) },
    ], 0, None).unwrap()
}
fn one(value: GraphValue) -> GraphValueRow { GraphValueRow::from_owned_values(vec![value]) }
fn only_list(state: &Operator) -> &[GraphValue] {
    let Value::Value(GraphValue::List(values)) = &state.rows().iter().next().unwrap().0.values()[0] else { panic!() };
    values
}

#[test]
fn canonical_nested_values_full_width_ids_and_large_distinct_support_remain_exact() {
    let values = [
        scalar(None),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::List(vec![scalar(None), scalar(Some(7))].into_boxed_slice()),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("text").unwrap()),
    ];
    for distinct in [false, true] {
        let query = any_definition(distinct);
        let rows = z(values.iter().cloned().map(|value| (one(value), 2)));
        let state = seed(query, &rows);
        let mut expected = Vec::new();
        for value in [&values[3], &values[1], &values[2]] {
            expected.extend(std::iter::repeat_n(value.clone(), if distinct { 1 } else { 2 }));
        }
        assert_eq!(only_list(&state), expected);
    }
    let huge = z([(one(scalar(Some(5))), i128::MAX)]);
    let state = seed(any_definition(true), &huge);
    assert_eq!(only_list(&state), &[scalar(Some(5))]);
    let state = seed(any_definition(false), &z([(one(scalar(None)), i128::MAX)]));
    assert!(only_list(&state).is_empty());
}

#[test]
fn collection_depth_and_node_bounds_refuse_before_expansion_and_retry_from_accepted_state() {
    let query = any_definition(false);
    let empty = ZSet::new();
    let mut state = seed(query.clone(), &empty);
    let mut deep = scalar(Some(1));
    for _ in 0..GraphValue::MAX_LIST_DEPTH { deep = GraphValue::List(vec![deep].into_boxed_slice()); }
    assert!(deep.validate_bounds());
    for rows in [
        z([(one(deep), 1)]),
        z([(one(scalar(Some(1))), GraphValue::MAX_LIST_NODES as i128)]),
        z([(one(scalar(Some(1))), i128::MAX)]),
    ] {
        assert!(matches!(state.prepare(&rows, LIMBS, None, &mut allow), Err(GroupError::Arithmetic)));
        assert_eq!(state, seed(query.clone(), &empty));
    }
    let valid = z([(one(scalar(Some(1))), (GraphValue::MAX_LIST_NODES - 1) as i128)]);
    state.prepare(&valid, LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(only_list(&state).len(), GraphValue::MAX_LIST_NODES - 1);
    let Value::Value(value) = &state.rows().iter().next().unwrap().0.values()[0] else { panic!() };
    assert!(value.validate_bounds());
    let inverse = empty.minus(&valid, LIMBS, &mut allow).unwrap();
    state.prepare(&inverse, LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(state, seed(query, &empty));
}

#[test]
fn every_update_checkpoint_and_exact_quota_preserves_raw_counts_index_summaries_and_output() {
    let query = definition(false, true, false);
    let old = bag(10);
    let new = bag(50);
    let change = new.minus(&old, LIMBS, &mut allow).unwrap();
    let mut state = seed(query.clone(), &old);
    let mut calls = 0;
    let mut work = 0;
    let mut scratch = 0;
    state.prepare(&change, LIMBS, None, &mut |event| {
        calls += 1;
        match event { ZSetEvent::Work => work += 1, ZSetEvent::ScratchEntry => scratch += 1 }
        Ok::<_, usize>(())
    }).unwrap().commit();
    for stop in 1..=calls {
        let mut state = seed(query.clone(), &old);
        let mut seen = 0;
        assert!(matches!(state.prepare(&change, LIMBS, None, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(GroupError::Delta(ZSetError::Control(n))) if n == stop));
        assert_eq!(state, seed(query.clone(), &old));
    }
    for (work_limit, scratch_limit, succeeds) in [(work, scratch, true), (work - 1, scratch, false), (work, scratch - 1, false)] {
        let mut state = seed(query.clone(), &old);
        let (mut w, mut s) = (0, 0);
        {
            let result = state.prepare(&change, LIMBS, None, &mut |event| {
                match event { ZSetEvent::Work => w += 1, ZSetEvent::ScratchEntry => s += 1 }
                if w > work_limit || s > scratch_limit { Err(0) } else { Ok(()) }
            });
            match result {
                Ok(pending) => { assert!(succeeds); pending.commit(); }
                Err(_) => assert!(!succeeds),
            }
        }
        assert_eq!(state, seed(query.clone(), if succeeds { &new } else { &old }));
    }
    let mut state = seed(query.clone(), &old);
    let invalid = z([(row(9, None, Some(4)), -1)]);
    assert!(matches!(state.prepare(&invalid, LIMBS, None, &mut allow), Err(GroupError::NegativeMultiplicity)));
    assert_eq!(state, seed(query.clone(), &old));
    assert!(matches!(state.prepare(&change, LIMBS, Some(0), &mut allow), Err(GroupError::ResultBudget { limit: 0 })));
    assert_eq!(state, seed(query, &old));
}

#[test]
fn unproved_sequences_refuse_but_complete_definition_and_other_aggregate_profiles_stay_intact() {
    let input = source();
    let pattern = input.incremental_pattern().unwrap().clone();
    let graph = PreparedGraphAggregate::prepare(pattern, &[], &[GraphAggregate::collect("items", 2)], 0, None).unwrap();
    assert!(matches!(IncrementalGroupAggregate::new(graph, &[GraphSetColumnType::Scalar; 3]),
        Err(GroupBuildError::UnsupportedAggregate { aggregate: 0 })));
    let positional = input.clone().cross_join(input).unwrap();
    let query = PreparedGraphSetAggregate::prepare(positional, &[], &[GraphAggregate::collect("items", 2)], 0, None).unwrap();
    assert!(matches!(Operator::new(query, &[GraphSetColumnType::Scalar; 6]),
        Err(GroupBuildError::UnsupportedAggregate { aggregate: 0 })));
    let definition = definition(false, true, false);
    let bytes = definition.canonical_bytes();
    let state = seed(definition.clone(), &bag(10));
    assert_eq!(state.definition().canonical_bytes(), bytes);
    let valid = GraphAggregateRow::from_group_values(vec![scalar(Some(0))], vec![
        Value::Value(GraphValue::List(vec![scalar(Some(7))].into_boxed_slice())),
        Value::Value(GraphValue::List(vec![scalar(Some(7))].into_boxed_slice())),
        Value::Count(1), Value::Count(1), Value::Integer(7),
    ]);
    assert!(definition.materialize_incremental_row(valid.keys().to_vec(), valid.values().to_vec()).is_some());
    for bad in [scalar(None), GraphValue::List(vec![scalar(None)].into_boxed_slice()),
        GraphValue::List(vec![GraphValue::Vertex(VId(1))].into_boxed_slice())] {
        let mut values = valid.values().to_vec();
        values[0] = Value::Value(bad);
        assert!(definition.materialize_incremental_row(valid.keys().to_vec(), values).is_none());
    }
}

#[test]
fn unrelated_groups_do_not_expand_the_changed_collection_work_domain() {
    let query = definition(false, false, false);
    let focused = z([(row(0, Some(1), Some(10)), 2), (row(0, Some(2), Some(20)), 1)]);
    let background = z((1..=1000).map(|g| (row(g, Some(1), Some(g)), 1)));
    let large = focused.plus(&background, LIMBS, &mut allow).unwrap();
    let change = z([(row(0, Some(1), Some(10)), -1), (row(0, Some(3), Some(30)), 1)]);
    let measure = |rows: &ZSet<GraphValueRow>| {
        let mut operator = seed(query.clone(), rows);
        let mut work = 0;
        operator.prepare(&change, LIMBS, None, &mut |event| {
            work += usize::from(event == ZSetEvent::Work);
            Ok::<_, usize>(())
        }).unwrap().commit();
        work
    };
    assert_eq!(measure(&focused), measure(&large));
}
