use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use crate::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateTest,
    PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
type Operator = IncrementalGroupAggregate<PreparedGraphSetAggregate>;
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(group: i64, value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![scalar(Some(group)), scalar(value)])
}
fn input() -> PreparedGraphSet {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    source
        .prepare_values(
            &[
                GraphColumn::property("g", "n", PropertyKeyId(1)),
                GraphColumn::property("v", "n", PropertyKeyId(2)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}
fn definition(global: bool, having: bool) -> PreparedGraphSetAggregate {
    let query = PreparedGraphSetAggregate::prepare(
        input(),
        if global { &[] } else { &[0] },
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("different", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("distinct_sum", 1),
            GraphAggregate::average_int("mean", 1),
            GraphAggregate::average_int_distinct("distinct_mean", 1),
            GraphAggregate::min("low", 1),
            GraphAggregate::max("high", 1),
        ],
        0,
        None,
    )
    .unwrap();
    if having {
        query
            .with_result_clauses(
                &[GraphAggregateFilter {
                    column: GraphAggregateColumn::Aggregate(0),
                    test: GraphAggregateTest::Integer {
                        comparison: IntegerComparison::GreaterOrEqual,
                        value: 2,
                    },
                }],
                &[],
            )
            .unwrap()
    } else {
        query
    }
}
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter().map(|(r, n)| (r, ZWeight::from_i128(n))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn bag(mut n: usize) -> ZSet<GraphValueRow> {
    z([(0, None), (0, Some(2)), (0, Some(5)), (1, Some(2))]
        .into_iter()
        .map(|(g, v)| {
            let count = n % 3;
            n /= 3;
            (row(g, v), count as i128)
        }))
}
fn seed(global: bool, having: bool, input: &ZSet<GraphValueRow>) -> Operator {
    let mut state =
        Operator::new(definition(global, having), &[GraphSetColumnType::Scalar; 2]).unwrap();
    state
        .prepare(input, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    state
}
// Independent full-bag arithmetic, not derivatives or the shared aggregate kernel.
fn oracle(global: bool, having: bool, rows: &ZSet<GraphValueRow>) -> ZSet<GraphAggregateRow> {
    let mut groups: BTreeMap<i64, (u64, Vec<i64>)> = BTreeMap::new();
    if global {
        groups.insert(0, (0, Vec::new()));
    }
    for (row, n) in rows.iter() {
        let GraphValue::Scalar(CanonicalScalar::Int(g)) = row.values()[0] else {
            unreachable!()
        };
        let group = groups.entry(if global { 0 } else { g }).or_default();
        let n = u64::try_from(n.to_i128().unwrap()).unwrap();
        group.0 += n;
        if let GraphValue::Scalar(CanonicalScalar::Int(v)) = row.values()[1] {
            for _ in 0..n {
                group.1.push(v);
            }
        }
    }
    let mut output = Vec::new();
    let definition = definition(global, having);
    for (g, (count, mut values)) in groups {
        if having && count < 2 {
            continue;
        }
        values.sort();
        let mut unique = values.clone();
        unique.dedup();
        let null = Value::Value(scalar(None));
        let numeric = |values: &[i64], average: bool| {
            if values.is_empty() {
                null.clone()
            } else {
                let sum = values.iter().map(|n| i128::from(*n)).sum();
                if average {
                    Value::Average(GraphExactAverage::new(sum, values.len() as u64).unwrap())
                } else {
                    Value::Integer(sum)
                }
            }
        };
        let cells = vec![
            Value::Count(count),
            Value::Count(values.len() as u64),
            Value::Count(unique.len() as u64),
            numeric(&values, false),
            numeric(&unique, false),
            numeric(&values, true),
            numeric(&unique, true),
            Value::Value(scalar(values.first().copied())),
            Value::Value(scalar(values.last().copied())),
        ];
        let result = definition
            .materialize_incremental_row(
                if global {
                    vec![]
                } else {
                    vec![scalar(Some(g))]
                },
                cells,
            )
            .unwrap();
        output.push((result, ZWeight::ONE));
    }
    ZSet::from_updates(output, LIMBS, &mut allow).unwrap()
}

#[test]
fn all_small_bag_transitions_match_nine_function_recomputation_and_inverse_deltas() {
    for global in [false, true] {
        for having in [false, true] {
            for before in 0..81 {
                for after in 0..81 {
                    let old = bag(before);
                    let new = bag(after);
                    let mut state = seed(global, having, &old);
                    assert_eq!(state.rows(), &oracle(global, having, &old));
                    let delta = new.minus(&old, LIMBS, &mut allow).unwrap();
                    let output = state
                        .prepare(&delta, LIMBS, None, &mut allow)
                        .unwrap()
                        .commit();
                    let mut integrated = oracle(global, having, &old);
                    integrated.integrate(&output, LIMBS, &mut allow).unwrap();
                    assert_eq!(
                        integrated,
                        oracle(global, having, &new),
                        "global={global},having={having},{before}->{after}"
                    );
                    assert_eq!(state.rows(), &integrated);
                    assert_eq!(state.input, new);
                    let reverse = state
                        .prepare(
                            &delta.negated(LIMBS, &mut allow).unwrap(),
                            LIMBS,
                            None,
                            &mut allow,
                        )
                        .unwrap()
                        .commit();
                    assert_eq!(reverse, output.negated(LIMBS, &mut allow).unwrap());
                    assert_eq!(state.rows(), &oracle(global, having, &old));
                }
            }
        }
    }
}

#[test]
fn every_control_refusal_drop_unwind_and_final_quota_preserve_all_state() {
    let initial = bag(17);
    let next = bag(76);
    let change = next.minus(&initial, LIMBS, &mut allow).unwrap();
    let make = || seed(false, true, &initial);
    let mut accepted = make();
    let mut calls = 0;
    let expected = accepted
        .prepare(&change, LIMBS, None, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .commit();
    for stop in 1..=calls {
        let mut state = make();
        let mut seen = 0;
        assert_eq!(
            state
                .prepare(&change, LIMBS, None, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                })
                .unwrap_err(),
            GroupError::Delta(ZSetError::Control(stop))
        );
        assert_eq!(seen, stop);
        assert_eq!(state, make());
        assert_eq!(
            state
                .prepare(&change, LIMBS, None, &mut allow)
                .unwrap()
                .commit(),
            expected
        );
        assert_eq!(state, accepted);
    }
    let mut state = make();
    {
        let pending = state.prepare(&change, LIMBS, None, &mut allow).unwrap();
        assert_eq!(pending.delta(), &expected);
    }
    assert_eq!(state, make());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _pending = state.prepare(&change, LIMBS, None, &mut allow).unwrap();
        panic!("downstream abort");
    }));
    assert_eq!(state, make());
    let mut state = seed(false, false, &z([(row(9, Some(3)), 1)]));
    let swap = z([(row(9, Some(3)), -1), (row(1, Some(5)), 1)]);
    state
        .prepare(&swap, LIMBS, Some(1), &mut allow)
        .unwrap()
        .commit();
    let snapshot = seed(false, false, &z([(row(1, Some(5)), 1)]));
    assert_eq!(state, snapshot);
    assert!(matches!(
        state.prepare(&z([(row(2, None), 1)]), LIMBS, Some(1), &mut allow),
        Err(GroupError::ResultBudget { limit: 1 })
    ));
    assert_eq!(state, snapshot);
}

#[test]
fn hidden_retractions_numeric_failures_and_native_overflow_never_publish() {
    let initial = z([(row(0, Some(2)), 1)]);
    let mut state = seed(false, true, &initial);
    let invalid = z([(row(0, Some(5)), -1), (row(0, Some(2)), 1)]);
    assert_eq!(
        state
            .prepare(&invalid, LIMBS, Some(0), &mut allow)
            .unwrap_err(),
        GroupError::NegativeMultiplicity
    );
    assert_eq!(state, seed(false, true, &initial));
    let bad = GraphValueRow::from_owned_values(vec![
        scalar(Some(0)),
        GraphValue::Scalar(CanonicalScalar::Bool(true)),
    ]);
    assert_eq!(
        state
            .prepare(&z([(bad, 1)]), LIMBS, None, &mut allow)
            .unwrap_err(),
        GroupError::NonInteger { column: 1 }
    );
    assert_eq!(state, seed(false, true, &initial));
    assert_eq!(
        state
            .prepare(
                &z([(row(1, Some(2)), i128::from(u64::MAX) + 1)]),
                LIMBS,
                None,
                &mut allow
            )
            .unwrap_err(),
        GroupError::Arithmetic
    );
    assert_eq!(state, seed(false, true, &initial));
    let bad = GraphValueRow::from_owned_values(vec![scalar(Some(0))]);
    assert_eq!(
        state
            .prepare(&z([(bad, 1)]), LIMBS, None, &mut allow)
            .unwrap_err(),
        GroupError::InputSchema
    );
}

#[test]
fn scalar_and_vertex_support_preserve_last_witness_and_refill_extrema() {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input: PreparedGraphSet = source
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("v", "n", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let definition = PreparedGraphSetAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_distinct("ids", 0),
            GraphAggregate::min("first", 0),
            GraphAggregate::max("last", 0),
            GraphAggregate::count_distinct("values", 1),
            GraphAggregate::min("low", 1),
            GraphAggregate::max("high", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let make = || {
        Operator::new(
            definition.clone(),
            &[GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
        )
        .unwrap()
    };
    let a = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(fgdb_types::VId(1)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap()),
    ]);
    let b = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(fgdb_types::VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("z").unwrap()),
    ]);
    let mut state = make();
    state
        .prepare(
            &z([(a.clone(), 2), (b.clone(), 3)]),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap()
        .commit();
    let before = state.rows().checked_clone(LIMBS, &mut allow).unwrap();
    assert!(
        state
            .prepare(&z([(a.clone(), -1)]), LIMBS, None, &mut allow)
            .unwrap()
            .commit()
            .is_empty()
    );
    assert_eq!(state.rows(), &before);
    state
        .prepare(&z([(a, -1)]), LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    let result = state.rows().iter().next().unwrap().0;
    assert_eq!(result.values()[0], Value::Count(1));
    assert_eq!(result.values()[1], Value::Value(b.values()[0].clone()));
    assert_eq!(result.values()[4], Value::Value(b.values()[1].clone()));
    // Extreme-only summaries may retain promoted raw counts without narrowing.
    state
        .prepare(&z([(b.clone(), i128::MAX - 3)]), LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    state
        .prepare(&z([(b.clone(), 1)]), LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    assert!(state.input.weight(&b).unwrap().is_promoted());
    assert_eq!(
        state.rows().iter().next().unwrap().0.values()[0],
        Value::Count(1)
    );
}

#[test]
fn changed_tuple_work_is_independent_of_unrelated_groups_and_occurrence_count() {
    let mut measured = Vec::new();
    for size in [8, 1024] {
        for n in [1, 1_000_000] {
            let mut initial = vec![(row(0, Some(2)), n)];
            initial.extend((1..size).map(|g| (row(g, Some(g)), 1)));
            let mut state = seed(false, false, &z(initial));
            let mut events = [0; 2];
            state
                .prepare(
                    &z([(row(0, Some(2)), -n), (row(0, Some(5)), n)]),
                    LIMBS,
                    None,
                    &mut |event| {
                        events[match event {
                            ZSetEvent::Work => 0,
                            ZSetEvent::ScratchEntry => 1,
                        }] += 1;
                        Ok::<_, usize>(())
                    },
                )
                .unwrap()
                .commit();
            measured.push(events);
        }
    }
    assert_eq!(measured[0], measured[1]);
    assert_eq!(measured[0], measured[2]);
    assert_eq!(measured[2], measured[3]);
}

#[test]
fn complete_definition_admission_refuses_transforms_and_checks_empty_input_schema() {
    assert!(matches!(
        Operator::new(definition(false, false), &[GraphSetColumnType::Scalar]),
        Err(GroupBuildError::InputSchema { .. })
    ));
    let ranked = definition(false, false).with_distinct_output(true);
    assert!(matches!(
        Operator::new(ranked, &[GraphSetColumnType::Scalar; 2]),
        Err(GroupBuildError::UnsupportedDefinition)
    ));
    let query = PreparedGraphSetAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::collect("xs", 1)],
        0,
        None,
    )
    .unwrap();
    // b0c3bd5f made COLLECT over an input with a proved occurrence order an
    // incrementally maintained aggregate. The UnsupportedAggregate refusal
    // branch is witnessed by value_tests.rs and collection/tests.rs.
    assert!(Operator::new(query, &[GraphSetColumnType::Scalar; 2]).is_ok());
    let state = seed(true, false, &ZSet::new());
    assert_eq!(state.rows().len(), 1);
    assert_eq!(
        state.rows().iter().next().unwrap().0.values()[0],
        Value::Count(0)
    );
    assert!(!format!("{state:?}").contains("CanonicalScalar"));
}
