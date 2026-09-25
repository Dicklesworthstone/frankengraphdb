//! Completed relations keep native domains; no graph-binding carrier is used.
use super::*;
use crate::algebra::IntegerComparison;
use crate::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn, GraphHavingExpression,
    GraphHavingOp, GraphHavingOperand, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetAggregate, PreparedGraphText,
};
use fgdb_delta_types::RelationId;
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
fn ok(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn row(key: GraphValue, value: GraphValue) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![key, value])
}
fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, n)| (row, ZWeight::from_i128(n))),
        LIMBS,
        &mut ok,
    )
    .unwrap()
}
fn path(eid: u128) -> GraphValue {
    let query = PreparedGraphText::prepare(
        "MATCH p = (a)-[:R]->(b) RETURN p",
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = query
        .plan()
        .execute_governed_with_identified_properties(
            3,
            [VId(1), VId(u128::MAX)],
            [(EId(eid), VId(1), RelationId(1), VId(u128::MAX))],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    result.value[0].values()[0].clone()
}
fn domains() -> Vec<GraphValue> {
    vec![
        null(),
        int(7),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Edge(EId(u128::MAX)),
        path(u128::MAX),
        GraphValue::Vertices(vec![VId(1), VId(u128::MAX)].into_boxed_slice()),
        GraphValue::Edges(vec![EId(1), EId(u128::MAX)].into_boxed_slice()),
        list(vec![
            int(1),
            list(vec![null(), GraphValue::Edge(EId(u128::MAX))]),
        ]),
    ]
}
fn dynamic() -> PreparedGraphSet {
    // Only the immutable schema is used by this operator test. Source execution
    // remains the responsibility of the owner, not of the grouping kernel.
    PreparedGraphSet::singleton()
        .unwind("key".into(), GraphSetValue::List(vec![]))
        .unwrap()
        .unwind("value".into(), GraphSetValue::List(vec![]))
        .unwrap()
}
fn definition(numeric: bool) -> PreparedGraphSetAggregate {
    let mut aggregates = vec![
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("nonnull", 1),
        GraphAggregate::count_distinct("distinct", 1),
        GraphAggregate::min("low", 1),
        GraphAggregate::max("high", 1),
    ];
    if numeric {
        aggregates.extend([
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("dsum", 1),
            GraphAggregate::average_int("avg", 1),
            GraphAggregate::average_int_distinct("davg", 1),
        ]);
    }
    PreparedGraphSetAggregate::prepare(dynamic(), &[0], &aggregates, 0, None).unwrap()
}
fn state(numeric: bool) -> IncrementalGroupAggregate<PreparedGraphSetAggregate> {
    IncrementalGroupAggregate::new(definition(numeric), &[GraphSetColumnType::Any; 2]).unwrap()
}
type Observed = BTreeMap<Vec<GraphValue>, Vec<Value>>;
fn observed(state: &IncrementalGroupAggregate<PreparedGraphSetAggregate>) -> Observed {
    state
        .rows()
        .iter()
        .map(|(row, n)| {
            assert_eq!(n, &ZWeight::ONE);
            (row.keys().to_vec(), row.values().to_vec())
        })
        .collect()
}
fn oracle(input: &ZSet<GraphValueRow>) -> Observed {
    let mut groups: BTreeMap<GraphValue, (u64, u64, BTreeSet<GraphValue>)> = BTreeMap::new();
    for (row, n) in input.iter() {
        let n = u64::try_from(n.to_i128().unwrap()).unwrap();
        let (rows, nonnull, support) = groups.entry(row.values()[0].clone()).or_default();
        *rows += n;
        if !row.values()[1].is_null() {
            *nonnull += n;
            support.insert(row.values()[1].clone());
        }
    }
    groups
        .into_iter()
        .map(|(key, (rows, nonnull, support))| {
            (
                vec![key],
                vec![
                    Value::Count(rows),
                    Value::Count(nonnull),
                    Value::Count(support.len() as u64),
                    Value::Value(support.first().cloned().unwrap_or_else(null)),
                    Value::Value(support.last().cloned().unwrap_or_else(null)),
                ],
            )
        })
        .collect()
}

#[test]
fn all_native_domains_keep_group_keys_distinct_support_and_extrema_on_retraction() {
    let values = domains();
    let source = bag(values.iter().flat_map(|key| {
        values
            .iter()
            .map(move |value| (row(key.clone(), value.clone()), 2))
    }));
    let mut state = state(false);
    state
        .prepare(&source, LIMBS, Some(8), &mut ok)
        .unwrap()
        .commit();
    assert_eq!(observed(&state), oracle(&source));
    assert_eq!(state.rows().len(), values.len());
    let mut remaining = source.checked_clone(LIMBS, &mut ok).unwrap();
    // Remove the last witness of the current maximum, then one of two copies
    // of the minimum. Typed support must be replaced only at zero crossings.
    for value in [
        values.iter().filter(|v| !v.is_null()).max().unwrap(),
        values.iter().filter(|v| !v.is_null()).min().unwrap(),
    ] {
        let changes = bag(values
            .iter()
            .map(|key| (row(key.clone(), value.clone()), -1)));
        for _ in 0..2 {
            remaining.integrate(&changes, LIMBS, &mut ok).unwrap();
            state
                .prepare(&changes, LIMBS, Some(8), &mut ok)
                .unwrap()
                .commit();
            assert_eq!(observed(&state), oracle(&remaining));
        }
    }
}

#[test]
fn exhaustive_mixed_domain_bags_integrate_and_invert_without_expanding_counts() {
    let rows = [
        row(list(vec![int(1)]), GraphValue::Edge(EId(u128::MAX))),
        row(list(vec![int(1)]), list(vec![null(), int(2)])),
        row(GraphValue::Vertex(VId(u128::MAX)), null()),
        row(GraphValue::Vertex(VId(u128::MAX)), path(1_u128 << 100)),
    ];
    let weights = |mut n: usize| -> [i128; 4] {
        std::array::from_fn(|_| {
            let v = (n % 3) as i128;
            n /= 3;
            v
        })
    };
    for before in 0..81 {
        for after in 0..81 {
            let a = weights(before);
            let b = weights(after);
            let input = bag(rows.iter().cloned().zip(a));
            let target = bag(rows.iter().cloned().zip(b));
            let changes = bag(rows
                .iter()
                .cloned()
                .zip(a.iter().zip(b).map(|(a, b)| b - *a)));
            let inverse = bag(rows
                .iter()
                .cloned()
                .zip(a.iter().zip(b).map(|(a, b)| *a - b)));
            let mut candidate = state(false);
            candidate
                .prepare(&input, LIMBS, Some(2), &mut ok)
                .unwrap()
                .commit();
            let mut delivered = candidate.rows().checked_clone(LIMBS, &mut ok).unwrap();
            let delta = candidate
                .prepare(&changes, LIMBS, Some(2), &mut ok)
                .unwrap()
                .commit();
            delivered.integrate(&delta, LIMBS, &mut ok).unwrap();
            assert_eq!(&delivered, candidate.rows());
            assert_eq!(observed(&candidate), oracle(&target));
            candidate
                .prepare(&inverse, LIMBS, Some(2), &mut ok)
                .unwrap()
                .commit();
            assert_eq!(observed(&candidate), oracle(&input));
        }
    }
}

#[test]
fn dynamic_numeric_operands_keep_nine_exact_results_and_do_not_hide_invalid_inputs() {
    let key = list(vec![GraphValue::Vertex(VId(u128::MAX))]);
    let source = bag([
        (row(key.clone(), int(i64::MAX)), 2),
        (row(key.clone(), int(i64::MAX - 1)), 1),
        (row(key.clone(), null()), 5),
    ]);
    let mut candidate = state(true);
    candidate
        .prepare(&source, LIMBS, Some(1), &mut ok)
        .unwrap()
        .commit();
    let output = candidate.rows().iter().next().unwrap().0;
    assert_eq!(output.keys(), std::slice::from_ref(&key));
    assert_eq!(
        &output.values()[..3],
        &[Value::Count(8), Value::Count(3), Value::Count(2)]
    );
    assert_eq!(
        output.values()[5].as_integer(),
        Some(3 * i128::from(i64::MAX) - 1)
    );
    assert_eq!(
        output.values()[6].as_integer(),
        Some(2 * i128::from(i64::MAX) - 1)
    );
    assert_eq!(
        output.values()[7].as_average(),
        GraphExactAverage::new(3 * i128::from(i64::MAX) - 1, 3)
    );
    assert_eq!(
        output.values()[8].as_average(),
        GraphExactAverage::new(2 * i128::from(i64::MAX) - 1, 2)
    );
    let before = observed(&candidate);
    for value in [
        GraphValue::Vertex(VId(1)),
        GraphValue::Edge(EId(1)),
        list(vec![int(1)]),
        GraphValue::Scalar(CanonicalScalar::Bool(true)),
    ] {
        assert!(matches!(
            candidate.prepare(&bag([(row(key.clone(), value), 1)]), LIMBS, None, &mut ok),
            Err(GroupError::NonInteger { column: 1 })
        ));
        assert_eq!(observed(&candidate), before);
    }
    // Same group and aggregate images as valid inputs cannot hide an invalid
    // raw tuple count, even when its numeric or support changes would cancel.
    let invalid = bag([(row(key.clone(), int(12)), -1), (row(key, int(13)), 1)]);
    assert!(matches!(
        candidate.prepare(&invalid, LIMBS, None, &mut ok),
        Err(GroupError::NegativeMultiplicity)
    ));
    assert_eq!(observed(&candidate), before);
}

#[test]
fn typed_schema_and_bounded_payloads_refuse_without_relaxing_graph_source_admission() {
    for value in domains().into_iter().filter(|value| !value.is_null()) {
        let input = PreparedGraphSet::singleton()
            .project(
                vec![
                    GraphSetProjection::new("key", GraphSetValue::Value(value.clone())),
                    GraphSetProjection::new("value", GraphSetValue::Value(value.clone())),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let schema = input.column_types().to_vec();
        let definition = PreparedGraphSetAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count("count", 1),
                GraphAggregate::min("min", 1),
                GraphAggregate::max("max", 1),
            ],
            0,
            None,
        )
        .unwrap();
        let mut candidate = IncrementalGroupAggregate::new(definition.clone(), &schema).unwrap();
        candidate
            .prepare(
                &bag([(row(value.clone(), value.clone()), 1)]),
                LIMBS,
                Some(1),
                &mut ok,
            )
            .unwrap()
            .commit();
        let result = candidate.rows().iter().next().unwrap().0;
        assert_eq!(result.keys(), std::slice::from_ref(&value));
        assert_eq!(
            result.values(),
            &[
                Value::Count(1),
                Value::Value(value.clone()),
                Value::Value(value)
            ]
        );
        assert!(IncrementalGroupAggregate::new(definition, &[GraphSetColumnType::Any; 2]).is_err());
        let bad = if schema[0] == GraphSetColumnType::Scalar {
            GraphValue::Edge(EId(1))
        } else {
            int(1)
        };
        assert!(matches!(
            candidate.prepare(&bag([(row(bad.clone(), bad), 1)]), LIMBS, None, &mut ok),
            Err(GroupError::InputSchema)
        ));
    }
    let definition = definition(false);
    let mut too_deep = int(0);
    for _ in 0..256 {
        too_deep = list(vec![too_deep]);
    }
    assert!(!too_deep.validate_bounds());
    assert!(
        definition
            .materialize_incremental_row(
                vec![too_deep],
                vec![
                    Value::Count(1),
                    Value::Count(1),
                    Value::Count(1),
                    Value::Value(int(0)),
                    Value::Value(int(0))
                ]
            )
            .is_none()
    );
    let graph = PreparedGraphText::prepare("MATCH (n) RETURN n", |_, _: &str| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let relational = crate::PreparedGraphAggregate::prepare_relation(
        graph.into(),
        &[],
        &[GraphAggregate::count_rows("count")],
        0,
        None,
    )
    .unwrap();
    assert!(
        relational
            .evaluate_incremental_input(vec![GraphValue::Vertex(VId(1))], &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn native_value_having_uses_the_existing_null_unknown_and_numeric_error_rules() {
    for value in domains() {
        let def = definition(false)
            .with_having_expression(
                &GraphHavingExpression::prepare(&[
                    GraphHavingOp::IsNull {
                        operand: GraphHavingOperand::Column(GraphAggregateColumn::GroupKey(0)),
                        is_null: false,
                    },
                    GraphHavingOp::IsNull {
                        operand: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(3)),
                        is_null: false,
                    },
                    GraphHavingOp::And,
                ])
                .unwrap(),
            )
            .unwrap();
        let mut candidate =
            IncrementalGroupAggregate::new(def, &[GraphSetColumnType::Any; 2]).unwrap();
        candidate
            .prepare(
                &bag([(row(value.clone(), value.clone()), 1)]),
                LIMBS,
                None,
                &mut ok,
            )
            .unwrap()
            .commit();
        assert_eq!(candidate.rows().len(), usize::from(!value.is_null()));
    }
    let expression = GraphHavingExpression::prepare(&[
        GraphHavingOp::Truth(Some(true)),
        GraphHavingOp::Compare {
            left: GraphHavingOperand::Column(GraphAggregateColumn::GroupKey(0)),
            comparison: IntegerComparison::Equal,
            right: GraphHavingOperand::Integer(1),
        },
        GraphHavingOp::Or,
    ])
    .unwrap();
    let def = definition(false)
        .with_having_expression(&expression)
        .unwrap();
    let mut candidate = IncrementalGroupAggregate::new(def, &[GraphSetColumnType::Any; 2]).unwrap();
    assert!(matches!(
        candidate.prepare(
            &bag([(row(list(vec![int(1)]), int(1)), 1)]),
            LIMBS,
            None,
            &mut ok
        ),
        Err(GroupError::NonIntegerHaving)
    ));
    assert!(candidate.rows().is_empty());
    // COLLECT's visitation order is not supplied by a bag. It remains refused.
    let collect = PreparedGraphSetAggregate::prepare(
        dynamic(),
        &[0],
        &[GraphAggregate::collect("items", 1)],
        0,
        None,
    )
    .unwrap();
    assert!(matches!(
        IncrementalGroupAggregate::new(collect, &[GraphSetColumnType::Any; 2]),
        Err(GroupBuildError::UnsupportedAggregate { aggregate: 0 })
    ));
}

#[test]
fn every_checkpoint_drop_and_unwind_preserves_full_state_then_retries() {
    let key = list(vec![int(3)]);
    let source = bag([
        (row(key.clone(), list(vec![int(1)])), 2),
        (row(key.clone(), GraphValue::Edge(EId(9))), 1),
    ]);
    let change = bag([
        (row(key.clone(), list(vec![int(1)])), -2),
        (row(key.clone(), GraphValue::Edge(EId(9))), -1),
        (
            row(GraphValue::Vertex(VId(u128::MAX)), list(vec![int(2)])),
            3,
        ),
    ]);
    let seeded = || {
        let mut candidate = state(false);
        candidate
            .prepare(&source, LIMBS, Some(1), &mut ok)
            .unwrap()
            .commit();
        candidate
    };
    let before = seeded();
    let mut expected = seeded();
    let mut calls = 0;
    expected
        .prepare(&change, LIMBS, Some(1), &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .commit();
    for stop in 1..=calls {
        let mut candidate = seeded();
        let mut seen = 0;
        assert!(
            matches!(candidate.prepare(&change, LIMBS, Some(1), &mut |_| {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(GroupError::Delta(ZSetError::Control(at))) if at == stop)
        );
        assert_eq!(seen, stop);
        assert_eq!(candidate, before);
        drop(candidate.prepare(&change, LIMBS, Some(1), &mut ok).unwrap());
        assert_eq!(candidate, before);
        candidate
            .prepare(&change, LIMBS, Some(1), &mut ok)
            .unwrap()
            .commit();
        assert_eq!(candidate, expected);
    }
    let mut candidate = seeded();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _update = candidate.prepare(&change, LIMBS, Some(1), &mut ok).unwrap();
        panic!("caller unwinds before publication");
    }));
    assert!(panic.is_err());
    assert_eq!(candidate, before);
    assert!(matches!(
        candidate.prepare(&change, LIMBS, Some(0), &mut ok),
        Err(GroupError::ResultBudget { limit: 0 })
    ));
    assert_eq!(candidate, before);
    candidate
        .prepare(&change, LIMBS, Some(1), &mut ok)
        .unwrap()
        .commit();
    assert_eq!(candidate, expected);
}
