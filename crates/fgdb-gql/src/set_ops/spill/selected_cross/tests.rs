use super::*;
use crate::algebra::IntegerComparison;
use crate::{GqlParameters, GraphSetOperand, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::CanonicalScalar;

fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare(
        "MATCH (n) RETURN n.p AS k,n.q AS v",
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}
fn product() -> PreparedGraphSet {
    leaf().cross_join(leaf()).unwrap()
}
fn predicate(a: usize, b: usize) -> [GraphSetPredicateOp; 1] {
    [GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(b),
    }]
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .collect(),
    )
}
fn cross(query: &PreparedGraphSet) -> SpillSetCrossPlan {
    let plan = AsyncSpillSetPlan::compile(query).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("expected an external product");
    };
    cross.clone()
}
fn copies() -> Vec<GraphSetProjection> {
    [(3, "rv"), (0, "lk"), (3, "again")]
        .into_iter()
        .map(|(at, name)| GraphSetProjection::new(name, GraphSetValue::Column(at)))
        .collect()
}

#[test]
fn selected_cross_keeps_original_child_windows_and_only_finishes_the_filter() {
    let left = leaf().with_page(1, Some(2));
    let right = leaf().with_page(2, Some(3));
    let query = left
        .cross_join(right)
        .unwrap()
        .nested()
        .unwrap()
        .project(copies(), GraphSetQuantifier::All)
        .unwrap()
        .nested()
        .unwrap()
        .filter(&predicate(1, 0))
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(0));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("selected product");
    };
    assert!(cross.has_selection());
    assert_eq!(cross.left().stages().last().unwrap().offset(), 1);
    assert_eq!(cross.left().stages().last().unwrap().count(), Some(2));
    assert_eq!(cross.right().stages().last().unwrap().offset(), 2);
    assert_eq!(cross.right().stages().last().unwrap().count(), Some(3));
    assert_eq!(cross.columns(), query.columns());
    assert_eq!(plan.stages().len(), 1);
    let finish = &plan.stages()[0];
    assert!(matches!(finish.operation, Operation::Identity));
    assert_eq!(
        finish.order(),
        Some([GraphValueOrder::descending(0)].as_slice())
    );
    assert_eq!(finish.offset(), 1);
    assert_eq!(finish.count(), Some(0));
    assert_eq!(
        plan.stage_count(),
        cross.left().stage_count() + cross.right().stage_count() + 3
    );
}

#[test]
fn selected_pair_maps_predicate_and_repeated_output_columns_without_rejected_copies() {
    let query = product()
        .project(copies(), GraphSetQuantifier::All)
        .unwrap()
        .filter(&predicate(1, 0))
        .unwrap();
    let plan = cross(&query);
    let left = row(&[Some(5), Some(99)]);
    for (value, accepted) in [
        (Some(8), true),
        (Some(5), false),
        (Some(4), false),
        (None, false),
    ] {
        let right = row(&[Some(100), value]);
        let mut scratch = 0;
        let output = plan
            .select_pair_with_control(&left, &right, &mut |event| {
                assert_ne!(event, GlaExecutionEvent::ResultRow);
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, usize>(())
            })
            .unwrap();
        if accepted {
            assert_eq!(output, Some(row(&[value, Some(5), value])));
            assert!(scratch > 0);
        } else {
            assert_eq!(output, None);
            assert_eq!(
                scratch, 0,
                "FALSE and UNKNOWN must not clone any output payload"
            );
        }
    }
    assert_eq!(left, row(&[Some(5), Some(99)]));
}

#[test]
fn false_selection_still_validates_both_complete_pair_schemas() {
    let plan = cross(
        &product()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))])
            .unwrap(),
    );
    let valid = row(&[Some(1), Some(2)]);
    let bad = GraphValueRow::unit();
    for (side, left, right) in [(0, &bad, &valid), (1, &valid, &bad)] {
        let result = plan.select_pair_with_control(left, right, &mut |_| Ok::<_, ()>(()));
        assert!(matches!(
            result,
            Err(SpillSetRowError::Native(GraphSetExecutionError::InputSchema { operand }))
                if operand == side
        ));
    }
}

#[test]
fn selected_pair_propagates_every_control_refusal_without_mutating_inputs() {
    let query = product()
        .project(copies(), GraphSetQuantifier::All)
        .unwrap()
        .filter(&predicate(1, 0))
        .unwrap();
    let plan = cross(&query);
    let left = row(&[Some(1), Some(2)]);
    let right = row(&[Some(3), Some(4)]);
    let mut events = Vec::new();
    let expected = plan
        .select_pair_with_control(&left, &right, &mut |event| {
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(expected, Some(row(&[Some(4), Some(1), Some(4)])));
    for fail in 0..events.len() {
        let mut at = 0;
        let actual = plan.select_pair_with_control(&left, &right, &mut |event| {
            assert_eq!(event, events[at]);
            let current = at;
            at += 1;
            if current == fail { Err(fail) } else { Ok(()) }
        });
        assert!(matches!(actual, Err(SpillSetRowError::Control(at)) if at == fail));
        assert_eq!(at, fail + 1);
    }
    assert_eq!(left, row(&[Some(1), Some(2)]));
    assert_eq!(right, row(&[Some(3), Some(4)]));
}

#[test]
fn native_fusion_boundaries_keep_pages_order_distinct_and_expression_stages() {
    let plain = product();
    let projected = plain
        .clone()
        .project(copies(), GraphSetQuantifier::All)
        .unwrap();
    let barriers = [
        plain.clone().with_page(0, Some(0)),
        plain.clone().with_page(1, None),
        plain
            .clone()
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap(),
        plain
            .clone()
            .project(copies(), GraphSetQuantifier::Distinct)
            .unwrap(),
        plain
            .project(
                vec![GraphSetProjection::new(
                    "literal",
                    GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
                )],
                GraphSetQuantifier::All,
            )
            .unwrap(),
        projected.clone().with_page(0, Some(2)),
        projected
            .project(
                vec![GraphSetProjection::new("second", GraphSetValue::Column(0))],
                GraphSetQuantifier::All,
            )
            .unwrap(),
    ];
    for input in barriers {
        let query = input
            .filter(&[GraphSetPredicateOp::Truth(Some(true))])
            .unwrap();
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
            panic!("product");
        };
        assert!(!cross.has_selection());
        assert!(
            plan.stages()
                .iter()
                .any(|stage| matches!(stage.operation, Operation::Filter(_)))
        );
    }
}

#[test]
fn unsupported_descendants_keep_their_original_depth_through_transparent_wrappers() {
    let owned = PreparedGraphSet::owned(vec!["x".into()], vec![GraphSetColumnType::Scalar]);
    let query = owned
        .cross_join(PreparedGraphSet::singleton())
        .unwrap()
        .nested()
        .unwrap()
        .project(
            vec![GraphSetProjection::new("copy", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .nested()
        .unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap();
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 5 })
    ));
}

#[test]
fn selected_zero_column_identity_and_unknown_have_native_truth_semantics() {
    for truth in [Some(true), Some(false), None] {
        let query = PreparedGraphSet::singleton()
            .cross_join(PreparedGraphSet::singleton())
            .unwrap()
            .filter(&[GraphSetPredicateOp::Truth(truth)])
            .unwrap();
        let plan = cross(&query);
        assert!(plan.has_selection());
        assert!(!plan.supports_keyed_selection());
        let output = plan
            .select_pair_with_control(&GraphValueRow::unit(), &GraphValueRow::unit(), &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(output, (truth == Some(true)).then(GraphValueRow::unit));
    }
}

fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}

#[test]
fn keyed_selection_uses_native_boolean_proof_and_counts_every_physical_barrier() {
    use GraphSetPredicateOp::{And, Not, Or, Truth};
    assert!(!cross(&product()).supports_keyed_selection());
    for (code, expected) in [
        (vec![equal(0, 2)], vec![(0, 0)]),
        (vec![equal(3, 1)], vec![(1, 1)]),
        (vec![equal(0, 1)], vec![]),
        (vec![equal(0, 2), equal(1, 3), And], vec![(0, 0), (1, 1)]),
        (vec![equal(0, 2), equal(0, 3), And], vec![(0, 0), (0, 1)]),
        (vec![equal(0, 2), Truth(None), And], vec![(0, 0)]),
        (
            vec![equal(0, 2), Truth(Some(false)), And, equal(2, 0), Or],
            vec![(0, 0)],
        ),
        (
            vec![
                equal(0, 2),
                equal(1, 3),
                And,
                equal(0, 2),
                equal(1, 2),
                And,
                Or,
            ],
            vec![(0, 0)],
        ),
        (vec![equal(0, 2), equal(1, 3), Or], vec![]),
        (vec![equal(0, 2), Truth(Some(true)), Or], vec![]),
        (vec![equal(0, 2), Not], vec![]),
        (vec![equal(0, 2), Not, Not], vec![]),
        (vec![Truth(Some(true))], vec![]),
    ] {
        let query = product().filter(&code).unwrap();
        let compiled = AsyncSpillSetPlan::compile(&query).unwrap();
        let AsyncSpillSetSourcePlan::Cross(plan) = compiled.source() else {
            panic!("selected product");
        };
        assert_eq!(plan.supports_keyed_selection(), !expected.is_empty());
        assert_eq!(plan.probe_keys_with_control(&mut allow).unwrap(), expected);
        assert_eq!(
            compiled.stage_count(),
            compiled.stages().len()
                + plan.left().stage_count()
                + plan.right().stage_count()
                + if expected.is_empty() { 2 } else { 8 }
        );
    }
}

#[test]
fn keyed_selection_maps_projected_columns_and_compares_the_complete_tuple_only() {
    use core::cmp::Ordering;
    let query = product()
        .project(copies(), GraphSetQuantifier::All)
        .unwrap()
        .filter(&[equal(0, 1), equal(2, 1), GraphSetPredicateOp::And])
        .unwrap();
    let plan = cross(&query);
    assert!(plan.supports_keyed_selection());
    let keys = plan.probe_keys_with_control(&mut allow).unwrap();
    assert_eq!(keys, [(0, 1)]);
    let left = row(&[Some(7), Some(999)]);
    let right = row(&[Some(-999), Some(7)]);
    assert_eq!(
        plan.compare_probe_keys_with_control(&left, &right, &keys, &mut allow)
            .unwrap(),
        Ordering::Equal,
        "unselected payload columns must not break a candidate tie"
    );
    assert_eq!(
        plan.select_pair_with_control(&left, &right, &mut allow)
            .unwrap(),
        Some(row(&[Some(7), Some(7), Some(7)]))
    );

    let plan = cross(
        &product()
            .filter(&[equal(0, 2), equal(0, 3), GraphSetPredicateOp::And])
            .unwrap(),
    );
    let keys = plan.probe_keys_with_control(&mut allow).unwrap();
    assert_eq!(keys, [(0, 0), (0, 1)]);
    for (right, expected) in [
        (row(&[Some(7), Some(7)]), Ordering::Equal),
        (row(&[Some(7), Some(8)]), Ordering::Less),
        (row(&[Some(6), Some(100)]), Ordering::Greater),
    ] {
        assert_eq!(
            plan.compare_probe_keys_with_control(&left, &right, &keys, &mut allow)
                .unwrap(),
            expected
        );
    }
}

#[test]
fn keyed_selection_reserves_private_width_without_rejecting_the_original_plan() {
    for width in [
        MAX_PATTERN_VERTICES - 2,
        MAX_PATTERN_VERTICES - 1,
        MAX_PATTERN_VERTICES,
    ] {
        let query = product()
            .project(
                (0..width)
                    .map(|column| {
                        GraphSetProjection::new(
                            format!("c{column}"),
                            GraphSetValue::Column(if column == 1 { 2 } else { 0 }),
                        )
                    })
                    .collect(),
                GraphSetQuantifier::All,
            )
            .unwrap()
            .filter(&[equal(0, 1)])
            .unwrap();
        let plan = cross(&query);
        assert!(plan.has_selection());
        assert_eq!(
            plan.supports_keyed_selection(),
            width <= MAX_PATTERN_VERTICES - 2
        );
        assert_eq!(plan.probe_keys_with_control(&mut allow).unwrap(), [(0, 0)]);
        assert_eq!(
            plan.select_pair_with_control(
                &row(&[Some(7), Some(1)]),
                &row(&[Some(7), Some(2)]),
                &mut allow,
            )
            .unwrap()
            .unwrap(),
            row(&vec![Some(7); width]),
            "wide selections must retain their original nested execution"
        );
    }
}

fn any_leaf() -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "key".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(0),
            ))]),
        )
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("key", GraphSetValue::Column(0)),
                GraphSetProjection::new("payload", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

fn any_keyed_plan() -> SpillSetCrossPlan {
    cross(
        &any_leaf()
            .cross_join(any_leaf())
            .unwrap()
            .filter(&[equal(0, 2)])
            .unwrap(),
    )
}

fn key_row(value: GraphValue) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![value, GraphValue::Scalar(CanonicalScalar::Int(91))])
}

fn nested(value: CanonicalScalar) -> GraphValue {
    GraphValue::map(vec![(
        "key".into(),
        GraphValue::List(Box::new([GraphValue::Scalar(value)])),
    )])
    .unwrap()
}

#[test]
fn keyed_composite_groups_preserve_numeric_peers_and_exclude_nested_unknown() {
    use core::cmp::Ordering;
    use fgdb_types::{CanonicalDecimal, CanonicalF64, EId, VId};
    let plan = any_keyed_plan();
    let keys = plan.probe_keys_with_control(&mut allow).unwrap();
    let int = nested(CanonicalScalar::Int(7));
    let float = nested(CanonicalScalar::Float(CanonicalF64::new(7.0)));
    let decimal = nested(CanonicalScalar::Decimal(
        CanonicalDecimal::from_integer(7).unwrap(),
    ));
    let null = nested(CanonicalScalar::Null);
    let nan = nested(CanonicalScalar::Float(CanonicalF64::new(f64::NAN)));
    for (left, right, eligible, matches) in [
        (int.clone(), float.clone(), true, true),
        (float, int.clone(), true, true),
        (int, decimal, true, false),
        (null.clone(), null, false, false),
        (nan.clone(), nan, true, true),
        (
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Null),
            false,
            false,
        ),
        (
            GraphValue::Vertices(Box::new([VId(3)])),
            GraphValue::List(Box::new([GraphValue::Vertex(VId(3))])),
            true,
            true,
        ),
        (
            GraphValue::Edges(Box::new([EId(4)])),
            GraphValue::List(Box::new([GraphValue::Edge(EId(4))])),
            true,
            true,
        ),
        (
            GraphValue::Vertices(Box::new([])),
            GraphValue::Edges(Box::new([])),
            true,
            true,
        ),
    ] {
        let left = key_row(left);
        let right = key_row(right);
        assert_eq!(
            plan.probe_eligible_with_control(&left, 0, &keys, &mut allow)
                .unwrap(),
            eligible
        );
        assert_eq!(
            plan.probe_eligible_with_control(&right, 1, &keys, &mut allow)
                .unwrap(),
            eligible
        );
        assert_eq!(
            plan.compare_probe_keys_with_control(&left, &right, &keys, &mut allow)
                .unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            plan.select_pair_with_control(&left, &right, &mut allow)
                .unwrap()
                .is_some(),
            matches
        );
    }
}

#[test]
fn keyed_probe_validation_precedes_null_exclusion_and_unequal_short_circuiting() {
    let plan = cross(&product().filter(&[equal(0, 2)]).unwrap());
    let null = row(&[None, Some(1)]);
    let left = row(&[Some(1), Some(2)]);
    let right = row(&[Some(9), Some(8)]);
    let invalid_payload = GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Vertex(fgdb_types::VId(1)),
    ]);
    for (row, side, keys, operand) in [
        (&null, 0, vec![(0, 0), (2, 0)], 0),
        (&null, 1, vec![(0, 0), (0, 2)], 1),
        (&left, 2, vec![(0, 0)], 2),
        (&invalid_payload, 0, vec![(0, 0)], 0),
        (&invalid_payload, 1, vec![(0, 0)], 1),
    ] {
        assert!(matches!(
            plan.probe_eligible_with_control(row, side, &keys, &mut allow),
            Err(SpillSetRowError::Native(GraphSetExecutionError::InputSchema {
                operand: actual
            })) if actual == operand
        ));
    }
    for (keys, operand) in [(vec![(0, 0), (2, 0)], 0), (vec![(0, 0), (0, 2)], 1)] {
        assert!(matches!(
            plan.compare_probe_keys_with_control(&left, &right, &keys, &mut allow),
            Err(SpillSetRowError::Native(GraphSetExecutionError::InputSchema {
                operand: actual
            })) if actual == operand
        ));
    }
    assert!(matches!(
        plan.probe_eligible_with_control(&GraphValueRow::unit(), 1, &[], &mut allow),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 1 }
        ))
    ));
}

#[test]
fn every_keyed_probe_control_refusal_is_terminal_and_keeps_original_inputs() {
    use fgdb_types::CanonicalF64;
    let plan = any_keyed_plan();
    let left = key_row(nested(CanonicalScalar::Int(7)));
    let right = key_row(nested(CanonicalScalar::Float(CanonicalF64::new(7.0))));
    let frozen = (left.clone(), right.clone());
    let exercise = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| {
        let mut controlled = |event| control(event);
        let keys = plan.probe_keys_with_control(&mut controlled)?;
        assert!(plan.probe_eligible_with_control(&left, 0, &keys, &mut controlled)?);
        assert!(plan.probe_eligible_with_control(&right, 1, &keys, &mut controlled)?);
        assert_eq!(
            plan.compare_probe_keys_with_control(&left, &right, &keys, &mut controlled)?,
            core::cmp::Ordering::Equal
        );
        plan.select_pair_with_control(&left, &right, &mut controlled)
    };
    let mut events = Vec::new();
    assert!(
        exercise(&mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            events.push(event);
            Ok(())
        })
        .unwrap()
        .is_some()
    );
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    for stop in 0..events.len() {
        let mut seen = 0;
        let actual = exercise(&mut |event| {
            assert_eq!(event, events[seen]);
            let current = seen;
            seen += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(actual, Err(SpillSetRowError::Control(stop)));
        assert_eq!(seen, stop + 1);
        assert_eq!((&left, &right), (&frozen.0, &frozen.1));
    }
}
