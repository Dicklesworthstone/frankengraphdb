use super::*;
use crate::algebra::IntegerComparison;
use crate::{GqlParameters, GraphSetOperand, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::CanonicalScalar;

fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS k,n.q AS v", |kind, name: &str| {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    })
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
            if current == fail {
                Err(fail)
            } else {
                Ok(())
            }
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
        let output = plan
            .select_pair_with_control(
                &GraphValueRow::unit(),
                &GraphValueRow::unit(),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(output, (truth == Some(true)).then(GraphValueRow::unit));
    }
}
