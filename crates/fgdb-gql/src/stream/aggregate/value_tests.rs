//! Source-neutral cells must preserve numeric laws and every native value bit.
use super::*;
use fgdb_types::{CanonicalF64, EId};
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

#[test]
fn binary64_inputs_move_sum_and_average_cells_to_one_exact_rounding() {
    use GraphAggregateFunction::*;
    let float = |value: f64| GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)));
    let int = |value: i64| GraphValue::Scalar(CanonicalScalar::Int(value));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    // An integer prefix, then floats; naive addition would lose the 3.
    let values = [
        int(3),
        null.clone(),
        float(1e100),
        float(0.1),
        float(-1e100),
        float(0.2),
        float(0.3),
        float(0.3),
    ];
    for (function, expected) in [
        (SumInt, float(3.9)),
        (AverageInt, float(0.5571428571428572)),
        (SumIntDistinct, float(3.6)),
        (AverageIntDistinct, float(0.6)),
    ] {
        let mut reversed = values.to_vec();
        reversed.reverse();
        for order in [&values[..], &reversed[..]] {
            assert!(
                result(function, order) == GraphAggregateValue::Value(expected.clone()),
                "{function:?}"
            );
        }
    }
    let mut state = make(SumInt);
    state
        .update_governed::<(), ()>(Input::from_value(&float(1.0)), 2, &mut |_| Ok(()))
        .unwrap();
    assert!(matches!(
        state.update_governed::<(), ()>(
            Input::from_value(&GraphValue::Scalar(CanonicalScalar::Bool(true))),
            2,
            &mut |_| Ok(())
        ),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 2
        }))
    ));
}

mod float_admission {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use crate::{GlaLimitDimension, GraphAggregate};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn float(value: f64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
    }

    fn seeded(function: GraphAggregateFunction) -> NumericState {
        let mut state = make(function);
        state
            .update_governed::<(), usize>(
                Input::Scalar(Some(&CanonicalScalar::Int(4))),
                7,
                &mut |_| Ok(()),
            )
            .unwrap();
        state
    }

    #[test]
    fn plain_promotion_admits_one_box_before_replacing_the_integer_prefix() {
        for (function, expected) in [
            (GraphAggregateFunction::SumInt, 10.0),
            (GraphAggregateFunction::AverageInt, 5.0),
        ] {
            let value = float(6.0);
            // The wrapped form must normalize before deciding whether the
            // numeric transition needs a separately owned accumulator.
            for input in [Input::from_value(&value), Input::Value(&value)] {
                let mut state = seeded(function);
                let mut calls = 0;
                let denied = state.update_governed::<(), usize>(input, 7, &mut |event| {
                    calls += 1;
                    assert_eq!(event, VertexScanEvent::ScratchEntry);
                    Err(GqlQueryError::Interrupted(17))
                });
                assert!(matches!(denied, Err(GqlQueryError::Interrupted(17))));
                assert_eq!(calls, 1);
                assert!(matches!(
                    state,
                    NumericState::Sum(Some(4)) | NumericState::Average { sum: 4, count: 1 }
                ));

                let mut scratch = 0;
                state
                    .update_governed::<(), usize>(input, 7, &mut |event| {
                        scratch += usize::from(event == VertexScanEvent::ScratchEntry);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(scratch, 1, "{function:?}");
                assert_eq!(
                    state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
                    GraphAggregateValue::Value(float(expected))
                );
            }
        }
    }

    #[test]
    fn zero_and_nonfinite_first_inputs_pay_once_and_later_inputs_reuse_the_box() {
        for function in [
            GraphAggregateFunction::SumInt,
            GraphAggregateFunction::AverageInt,
        ] {
            for first in [0.0, -0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut state = make(function);
                let mut scratch = 0;
                state
                    .update_governed::<(), ()>(Input::from_value(&float(first)), 0, &mut |event| {
                        scratch += usize::from(event == VertexScanEvent::ScratchEntry);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(scratch, 1);
                assert!(matches!(state, NumericState::Float { count: 1, .. }));
                for value in [
                    float(2.0),
                    GraphValue::Scalar(CanonicalScalar::Int(3)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ] {
                    state
                        .update_governed::<(), ()>(Input::from_value(&value), 0, &mut |_| {
                            panic!("an existing exact accumulator needs no new box");
                        })
                        .unwrap();
                }
                assert!(matches!(state, NumericState::Float { count: 3, .. }));
            }
        }
    }

    #[test]
    fn distinct_promotion_reserves_both_owners_without_partial_contribution() {
        for (function, expected) in [
            (GraphAggregateFunction::SumIntDistinct, 10.0),
            (GraphAggregateFunction::AverageIntDistinct, 5.0),
        ] {
            let value = float(6.0);
            let mut events = Vec::new();
            seeded(function)
                .update_governed::<(), usize>(Input::from_value(&value), 7, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .unwrap();
            // One canonical membership entry and one exact-accumulator box.
            // This is an independent allocation contract, not a budget learned
            // from the implementation and then accepted as its own oracle.
            assert_eq!(
                events
                    .iter()
                    .filter(|&&event| event == VertexScanEvent::ScratchEntry)
                    .count(),
                2
            );
            for cut in 1..=events.len() {
                let mut state = seeded(function);
                let mut calls = 0;
                let denied =
                    state.update_governed::<(), usize>(Input::from_value(&value), 7, &mut |_| {
                        calls += 1;
                        if calls == cut {
                            Err(GqlQueryError::Interrupted(cut))
                        } else {
                            Ok(())
                        }
                    });
                assert!(matches!(denied, Err(GqlQueryError::Interrupted(at)) if at == cut));
                let NumericState::Distinct(ref distinct) = state else {
                    panic!("lost DISTINCT ownership");
                };
                assert_eq!(distinct.scalars, BTreeSet::from([CanonicalScalar::Int(4)]));
                assert!(distinct.vertices.is_empty() && distinct.values.is_empty());
                assert_eq!(distinct.max_payload, 0);
                assert!(matches!(
                    distinct.accumulator,
                    NumericState::Sum(Some(4)) | NumericState::Average { sum: 4, count: 1 }
                ));
                state
                    .update_governed::<(), usize>(Input::from_value(&value), 7, &mut |_| Ok(()))
                    .unwrap();
                let mut duplicate_scratch = 0;
                state
                    .update_governed::<(), usize>(Input::from_value(&value), 7, &mut |event| {
                        duplicate_scratch += usize::from(event == VertexScanEvent::ScratchEntry);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(duplicate_scratch, 0);
                assert_eq!(
                    state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
                    GraphAggregateValue::Value(float(expected))
                );
            }
        }
    }

    #[test]
    fn overflowing_average_refuses_before_allocation_and_keeps_its_integer_state() {
        let value = float(1.0);
        for governed in [false, true] {
            let mut state = NumericState::Average {
                sum: i128::MAX,
                count: u64::MAX,
            };
            let denied = if governed {
                state.update_governed::<(), ()>(Input::from_value(&value), 7, &mut |_| {
                    panic!("count overflow must precede allocation admission");
                })
            } else {
                state.update::<(), ()>(Input::from_value(&value), 7)
            };
            assert!(matches!(
                denied,
                Err(GqlQueryError::Source(
                    GraphAggregateError::ArithmeticOverflow { aggregate: 7 }
                ))
            ));
            assert!(matches!(
                state,
                NumericState::Average {
                    sum: i128::MAX,
                    count: u64::MAX
                }
            ));
        }
    }

    const PROPERTY: PropertyKeyId = PropertyKeyId(1);
    const RECORDS: u128 = 3;

    struct Source {
        next: u128,
        properties: [(PropertyKeyId, CanonicalScalar); 1],
        drops: Arc<AtomicUsize>,
    }
    impl Source {
        fn new(floating: bool) -> Self {
            Self {
                next: 1,
                properties: [(
                    PROPERTY,
                    if floating {
                        CanonicalScalar::Float(CanonicalF64::new(4.0))
                    } else {
                        CanonicalScalar::Int(4)
                    },
                )],
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl VertexScanSource for Source {
        type Error = ();
        fn snapshot_seq(&self) -> CommitSeq {
            CommitSeq(7)
        }
        fn next_vertex<C>(
            &mut self,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VId>, VertexScanSourceError<(), C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            if self.next > RECORDS {
                return Ok(None);
            }
            let id = VId(self.next);
            self.next += 1;
            Ok(Some(id))
        }
        fn vertex<C>(
            &self,
            id: VId,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRow<'_>>, VertexScanSourceError<(), C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            Ok((1..=RECORDS).contains(&id.0).then_some(VertexScanRow {
                labels: &[],
                properties: &self.properties,
            }))
        }
    }

    fn definition(grouped: bool) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("value", "n", PROPERTY),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            if grouped { &[0] } else { &[] },
            &[
                GraphAggregate::sum_int("total", 1),
                GraphAggregate::average_int("mean", 1),
                GraphAggregate::sum_int_distinct("unique_total", 1),
                GraphAggregate::average_int_distinct("unique_mean", 1),
            ],
            0,
            None,
        )
        .unwrap()
    }

    #[test]
    fn native_cursors_charge_each_promotion_and_enforce_inclusive_budgets() {
        let wide = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        for grouped in [false, true] {
            let plan = VertexAggregatePlan::compile(&definition(grouped)).unwrap();
            let mut integer =
                VertexAggregateCursor::new(Source::new(false), plan.clone(), wide, || {
                    Ok::<_, usize>(())
                });
            let integer_rows = integer.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let integer_stats = integer.evaluator_stats();
            let source = Source::new(true);
            let drops = source.drops.clone();
            let mut calls = 0;
            let mut floating = VertexAggregateCursor::new(source, plan.clone(), wide, || {
                calls += 1;
                Ok::<_, usize>(())
            });
            let expected = floating.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let stats = floating.evaluator_stats();
            let row_stats = floating.row_stats();
            drop(floating);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(expected.len(), integer_rows.len());
            assert_eq!(
                stats.scratch_entries - integer_stats.scratch_entries,
                4 * expected.len() as u64,
                "one exact box per SUM/AVG cell, per group, including DISTINCT"
            );
            for row in &expected {
                let total = if grouped { 4.0 } else { 12.0 };
                assert_eq!(
                    row.values(),
                    &[total, 4.0, 4.0, 4.0].map(|value| GraphAggregateValue::Value(float(value)))
                );
            }
            let exact = GqlQueryPolicy::new(
                row_stats.snapshot_records,
                row_stats.result_rows,
                stats.work_units,
                stats.scratch_entries,
            );
            let mut admitted =
                VertexAggregateCursor::new(Source::new(true), plan.clone(), exact, || {
                    Ok::<_, usize>(())
                });
            assert_eq!(
                admitted.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            for (policy, dimension) in [
                (
                    GqlQueryPolicy::new(
                        row_stats.snapshot_records,
                        row_stats.result_rows,
                        stats.work_units,
                        stats.scratch_entries - 1,
                    ),
                    GlaLimitDimension::ScratchEntries,
                ),
                (
                    GqlQueryPolicy::new(
                        row_stats.snapshot_records,
                        row_stats.result_rows,
                        stats.work_units - 1,
                        stats.scratch_entries,
                    ),
                    GlaLimitDimension::WorkUnits,
                ),
            ] {
                let source = Source::new(true);
                let drops = source.drops.clone();
                let mut cursor =
                    VertexAggregateCursor::new(source, plan.clone(), policy, || Ok::<_, usize>(()));
                assert!(matches!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>(),
                    Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension
                ));
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
            for cut in 1..=calls {
                let source = Source::new(true);
                let drops = source.drops.clone();
                let mut seen = 0;
                let mut cursor = VertexAggregateCursor::new(source, plan.clone(), exact, || {
                    seen += 1;
                    if seen == cut { Err(cut) } else { Ok(()) }
                });
                assert!(matches!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>(),
                    Err(GqlQueryError::Interrupted(at)) if at == cut
                ));
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        }
    }
}
