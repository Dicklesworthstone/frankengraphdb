//! Source-neutral cells must preserve numeric laws and every native value bit.
use super::*;
use fgdb_types::EId;
use std::collections::BTreeSet;

fn make(function: GraphAggregateFunction) -> NumericState {
    NumericState::new_governed(function, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn result(function: GraphAggregateFunction, values: &[GraphValue]) -> GraphAggregateValue {
    let mut state = make(function);
    for value in values {
        let input = if function == GraphAggregateFunction::CountRows {
            Input::Identity
        } else {
            Input::from_value(value)
        };
        state
            .update_governed::<(), ()>(input, 0, &mut |_| Ok(()))
            .unwrap();
    }
    state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap()
}
fn oracle(function: GraphAggregateFunction, values: &[Option<i64>]) -> GraphAggregateValue {
    use GraphAggregateFunction::*;
    let plain: Vec<i64> = values.iter().flatten().copied().collect();
    let unique: Vec<i64> = plain
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let selected = if matches!(
        function,
        CountDistinct | SumIntDistinct | AverageIntDistinct
    ) {
        &unique
    } else {
        &plain
    };
    let null = GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
    match function {
        CountRows => GraphAggregateValue::Count(values.len() as u64),
        Count | CountDistinct => GraphAggregateValue::Count(selected.len() as u64),
        SumInt | SumIntDistinct if !selected.is_empty() => {
            GraphAggregateValue::Integer(selected.iter().map(|n| i128::from(*n)).sum())
        }
        AverageInt | AverageIntDistinct if !selected.is_empty() => GraphAggregateValue::Average(
            GraphExactAverage::new(
                selected.iter().map(|n| i128::from(*n)).sum(),
                selected.len() as u64,
            )
            .unwrap(),
        ),
        Min | Max => {
            let extreme = if function == Min {
                selected.iter().min()
            } else {
                selected.iter().max()
            };
            extreme.map_or(null, |n| {
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(*n)))
            })
        }
        _ => null,
    }
}

#[test]
fn shared_constructor_updates_and_finalizer_match_all_nine_exact_functions() {
    use GraphAggregateFunction::*;
    let functions = [
        CountRows,
        Count,
        CountDistinct,
        SumInt,
        SumIntDistinct,
        AverageInt,
        AverageIntDistinct,
        Min,
        Max,
    ];
    let choices = [None, Some(i64::MIN), Some(i64::MAX), Some(7)];
    for mut code in 0..256_usize {
        let mut inputs = Vec::new();
        for _ in 0..4 {
            inputs.push(choices[code % 4]);
            code /= 4;
        }
        let values: Vec<_> = inputs
            .iter()
            .map(|n| GraphValue::Scalar(n.map_or(CanonicalScalar::Null, CanonicalScalar::Int)))
            .collect();
        for function in functions {
            assert!(NumericState::supports(function));
            assert_eq!(result(function, &values), oracle(function, &inputs));
            assert_eq!(result(function, &[]), oracle(function, &[]));
        }
    }
    assert!(!NumericState::supports(Collect));
    assert!(!NumericState::supports(CollectDistinct));
}

#[test]
fn distinct_uses_canonical_full_width_edges_lists_and_normalized_scalar_vertex_domains() {
    let list = GraphValue::List(
        vec![
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("é\0same").unwrap()),
        ]
        .into_boxed_slice(),
    );
    let values = vec![
        GraphValue::Edge(EId(0)),
        GraphValue::Edge(EId(u128::MAX)),
        GraphValue::Edge(EId(u128::MAX)),
        GraphValue::Vertex(VId(0)),
        GraphValue::Scalar(CanonicalScalar::Int(0)),
        list.clone(),
        list,
        GraphValue::Edges(vec![EId(0), EId(u128::MAX)].into_boxed_slice()),
        GraphValue::Scalar(CanonicalScalar::Null),
    ];
    let expected = values
        .iter()
        .filter(|v| !v.is_null())
        .cloned()
        .collect::<BTreeSet<_>>()
        .len();
    assert_eq!(
        result(GraphAggregateFunction::CountDistinct, &values),
        GraphAggregateValue::Count(expected as u64)
    );
    let mut state = make(GraphAggregateFunction::CountDistinct);
    let scalar = GraphValue::Scalar(CanonicalScalar::Int(9));
    let vertex = GraphValue::Vertex(VId(u128::MAX));
    for input in [
        Input::Value(&scalar),
        Input::from_value(&scalar),
        Input::Value(&vertex),
        Input::Vertex(VId(u128::MAX)),
    ] {
        state
            .update_governed::<(), ()>(input, 0, &mut |_| Ok(()))
            .unwrap();
    }
    assert_eq!(
        state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
        GraphAggregateValue::Count(2)
    );
}

#[test]
fn shared_extrema_move_selected_native_values_without_narrowing() {
    for values in [
        vec![
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::Edge(EId(0)),
            GraphValue::Edge(EId(1)),
        ],
        vec![
            GraphValue::Vertices(vec![VId(u128::MAX)].into_boxed_slice()),
            GraphValue::Vertices(vec![VId(0), VId(u128::MAX)].into_boxed_slice()),
        ],
        vec![
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(7))].into_boxed_slice()),
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(-3))].into_boxed_slice()),
        ],
    ] {
        assert_eq!(
            result(GraphAggregateFunction::Min, &values),
            GraphAggregateValue::Value(values.iter().min().unwrap().clone())
        );
        assert_eq!(
            result(GraphAggregateFunction::Max, &values),
            GraphAggregateValue::Value(values.iter().max().unwrap().clone())
        );
    }
}

#[test]
fn complex_distinct_witnesses_are_not_published_on_any_refusal_or_numeric_error() {
    let old = GraphValue::List(vec![GraphValue::Edge(EId(1))].into_boxed_slice());
    let new = GraphValue::List(
        vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"z".repeat(64)).unwrap(),
        )]
        .into_boxed_slice(),
    );
    let seed = || {
        let mut state = make(GraphAggregateFunction::CountDistinct);
        state
            .update_governed::<(), usize>(Input::from_value(&old), 0, &mut |_| Ok(()))
            .unwrap();
        state
    };
    let mut calls = 0;
    seed()
        .update_governed::<(), usize>(Input::from_value(&new), 0, &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert!(calls > 0);
    for stop in 1..=calls {
        let mut state = seed();
        let mut at = 0;
        let error = state.update_governed::<(), usize>(Input::from_value(&new), 0, &mut |_| {
            at += 1;
            if at == stop {
                Err(GqlQueryError::Interrupted(stop))
            } else {
                Ok(())
            }
        });
        assert!(matches!(error, Err(GqlQueryError::Interrupted(n)) if n == stop));
        let NumericState::Distinct(state) = state else {
            panic!("wrong state");
        };
        assert_eq!(state.values, BTreeSet::from([old.clone()]));
        assert!(matches!(state.accumulator, NumericState::Count(1)));
    }
    for function in [
        GraphAggregateFunction::SumIntDistinct,
        GraphAggregateFunction::AverageIntDistinct,
    ] {
        let mut state = make(function);
        let error = state.update_governed::<(), ()>(Input::from_value(&old), 7, &mut |_| Ok(()));
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { aggregate: 7 }
                    | GraphAggregateError::NonIntegerAverage { aggregate: 7 }
            ))
        ));
        let NumericState::Distinct(state) = state else {
            panic!("wrong state");
        };
        assert!(state.values.is_empty() && state.scalars.is_empty() && state.vertices.is_empty());
    }
}

#[test]
fn average_finalization_and_complex_extremum_replacement_are_governed() {
    for stop in 1..=128 {
        let mut at = 0;
        let result = NumericState::Average { sum: -1, count: 2 }.finish_governed(&mut |_| {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(result.unwrap_err(), stop);
    }
    let old = GraphValue::Edges(vec![EId(1)].into_boxed_slice());
    let new = GraphValue::Edges(vec![EId(u128::MAX), EId(3)].into_boxed_slice());
    let mut seed = make(GraphAggregateFunction::Max);
    seed.update_governed::<(), ()>(Input::from_value(&old), 0, &mut |_| Ok(()))
        .unwrap();
    assert!(matches!(
        seed.update_governed::<(), ()>(Input::from_value(&new), 0, &mut |_| Err(
            GqlQueryError::Interrupted(())
        )),
        Err(GqlQueryError::Interrupted(()))
    ));
    assert_eq!(
        seed.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
        GraphAggregateValue::Value(old)
    );
}
