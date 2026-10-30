//! Terminal aggregate admission retains the complete procedure relation.
use super::*;
use crate::algebra::GraphValueOrder;
use crate::spill_set::SpillSetRowError;
use crate::{
    GraphAggregate, GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression,
    GraphIntegerOp, GraphSetExecutionError, GraphSetValue, PreparedGraphSet,
};

fn call(alias: &str, arguments: Vec<GraphSetValue>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .procedure_call(
            "fnx".into(),
            "rows".into(),
            arguments,
            Vec::new(),
            vec![("value".into(), alias.into())],
            Vec::new(),
        )
        .unwrap()
}

#[test]
fn explicit_terminal_owners_keep_the_complete_input_and_native_definition() {
    let relation = call("score", Vec::new())
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(3))
        .nested()
        .unwrap();
    let aggregates = [
        GraphAggregate::sum_int("sum", 0),
        GraphAggregate::average_int("mean", 0),
        GraphAggregate::collect("values", 0),
    ];
    let direct = PreparedGraphAggregate::prepare_set_relation(
        relation.clone(),
        &[],
        &aggregates,
        2,
        Some(1),
    )
    .unwrap();
    let compound =
        PreparedGraphSetAggregate::prepare(relation.clone(), &[], &aggregates, 2, Some(1)).unwrap();
    let transcript = relation.canonical_bytes();
    assert!(AsyncSpillAggregatePlan::compile(&direct).is_err());
    assert!(AsyncSpillAggregatePlan::compile_relation(&compound).is_err());
    for plan in [
        AsyncSpillAggregatePlan::compile_with_single_procedure(&direct).unwrap(),
        AsyncSpillAggregatePlan::compile_relation_with_single_procedure(&compound).unwrap(),
    ] {
        let AsyncSpillAggregatePlan::Relation { input, definition } = plan else {
            panic!("a terminal procedure aggregate must keep its complete relation");
        };
        assert_eq!(input.source_count(), 1);
        assert_eq!(input.procedure_source().unwrap().columns(), ["score"]);
        assert_eq!(
            input.procedure_source().unwrap().call().outputs(),
            ["value"]
        );
        assert!(input.source_pattern().is_none() && input.source_tail().is_none());
        assert_eq!(definition.input_width(), 1);
        assert_eq!(definition.result_window(), (2, Some(1)));
        assert!(definition.has_collections());
        assert!(definition.input_pipeline.is_none());
    }
    assert_eq!(relation.canonical_bytes(), transcript);
}

#[test]
fn terminal_capability_requires_one_call_outside_every_apply_body() {
    let one = || call("value", Vec::new());
    let inputs = [
        PreparedGraphSet::singleton(),
        one().cross_join(call("other", Vec::new())).unwrap(),
        PreparedGraphSet::singleton()
            .apply(one(), Vec::new())
            .unwrap(),
        PreparedGraphSet::singleton()
            .optional_apply(one(), Vec::new())
            .unwrap(),
    ];
    for input in inputs {
        // Empty input and final pages cannot hide unsupported source shapes.
        for input in [input.clone(), input.with_page(0, Some(0))] {
            let direct = PreparedGraphAggregate::prepare_set_relation(
                input.clone(),
                &[],
                &[GraphAggregate::count_rows("n")],
                0,
                Some(0),
            )
            .unwrap();
            let compound = PreparedGraphSetAggregate::prepare(
                input,
                &[],
                &[GraphAggregate::count_rows("n")],
                0,
                Some(0),
            )
            .unwrap();
            assert!(AsyncSpillAggregatePlan::compile_with_single_procedure(&direct).is_err());
            assert!(
                AsyncSpillAggregatePlan::compile_relation_with_single_procedure(&compound).is_err()
            );
        }
    }
}

#[test]
fn empty_terminal_pages_never_evaluate_or_hide_call_argument_failures() {
    let argument = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let query = PreparedGraphSetAggregate::prepare(
        call("value", vec![argument]),
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    let AsyncSpillAggregatePlan::Relation { input, definition } =
        AsyncSpillAggregatePlan::compile_relation_with_single_procedure(&query).unwrap()
    else {
        unreachable!();
    };
    assert_eq!(definition.result_window(), (0, Some(0)));
    assert!(matches!(
        input
            .procedure_source()
            .unwrap()
            .evaluate_arguments(&mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(GraphSetExecutionError::Projection {
            row: 0,
            column: 0,
            error,
        })) if error.kind == GraphIntegerErrorKind::DivisionByZero
    ));
}
