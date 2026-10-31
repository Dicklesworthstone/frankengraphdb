use super::*;
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
    GraphSetOperand, GraphSetPredicateOp, ProcedureGraphDirection, ProcedureGraphProjection,
    ProcedureGraphWeight, ProcedureMissingWeight, ProcedureParallelEdges, ProcedureSelfLoops,
};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::CanonicalScalar;

fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn call(namespace: &str, alias: &str, arguments: Vec<GraphSetValue>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .procedure_call(
            namespace.into(),
            "rows".into(),
            arguments,
            Vec::new(),
            vec![("value".into(), alias.into())],
            Vec::new(),
        )
        .unwrap()
}

fn expression(left: i64, operation: GraphIntegerBinary, right: i64) -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(left)),
            GraphIntegerOp::Literal(Some(right)),
            GraphIntegerOp::Binary(operation),
        ])
        .unwrap(),
    )
}

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[test]
fn explicit_capability_preserves_original_call_metadata_and_source_clauses() {
    let projection = ProcedureGraphProjection {
        vertex_label: Some(LabelId(11)),
        relation: Some(RelationId(17)),
        weight: ProcedureGraphWeight::Property {
            key: PropertyKeyId(23),
            missing: ProcedureMissingWeight::Unit,
        },
        direction: Some(ProcedureGraphDirection::Reversed),
        parallel_edges: ProcedureParallelEdges::Maximum,
        self_loops: ProcedureSelfLoops::Drop,
    };
    let query = PreparedGraphSet::singleton()
        .procedure_call_with_projection(
            "fnx".into(),
            "named".into(),
            vec![
                GraphSetValue::Value(scalar(7)),
                GraphSetValue::Value(scalar(19)),
            ],
            vec!["first".into(), "second".into()],
            vec![
                ("node".into(), "person".into()),
                ("score".into(), "rank".into()),
            ],
            vec![0],
            Some(projection),
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap()
        .with_page(2, Some(0));
    let transcript = query.canonical_bytes();
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 0 })
    ));
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
    let source = plan.procedure_source().unwrap();
    let SetNode::ProcedureCall(original) = &query.node else {
        unreachable!();
    };
    assert!(source.call() == original.as_ref());
    assert_eq!(source.call().namespace(), "fnx");
    assert_eq!(source.call().name(), "named");
    assert_eq!(source.call().argument_names(), ["first", "second"]);
    assert_eq!(source.call().outputs(), ["node", "score"]);
    assert_eq!(source.call().vertex_outputs(), [0]);
    assert_eq!(source.call().graph_projection(), Some(&projection));
    assert_eq!(source.columns(), ["person", "rank"]);
    assert_eq!(source.column_types(), [GraphSetColumnType::Any; 2]);
    assert_eq!(source.argument_count(), 2);
    assert_eq!(plan.source_columns(), source.columns());
    assert!(plan.source_pattern().is_none() && plan.source_tail().is_none());
    assert_eq!(plan.source_count(), 1);
    assert_eq!(plan.source().kind(), ScanKind::Set);
    assert_eq!(plan.stage_count(), 1);
    assert_eq!(plan.aggregate_count(), 0);
    assert_eq!(
        plan.stages()[0].order(),
        Some([GraphValueOrder::descending(1)].as_slice())
    );
    assert_eq!(plan.stages()[0].offset(), 2);
    assert_eq!(plan.stages()[0].count(), Some(0));
    assert_eq!(query.canonical_bytes(), transcript);

    for (namespace, ranked) in [("fnx", false), ("hybrid", true)] {
        let query = call(namespace, "value", Vec::new());
        let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
        assert_eq!(plan.stages()[0].order().is_none(), ranked);
        if !ranked {
            assert_eq!(plan.stages()[0].order(), Some([].as_slice()));
        }
        assert!(AsyncSpillSetPlan::compile(&query).is_err());
    }
}

#[test]
fn single_procedure_admission_crosses_every_child_but_never_an_apply_body() {
    let single = || call("fnx", "value", Vec::new());
    assert!(AsyncSpillSetPlan::compile(&PreparedGraphSet::singleton()).is_ok());
    assert!(
        AsyncSpillSetPlan::compile_with_single_procedure(&PreparedGraphSet::singleton()).is_err()
    );

    let typed = PreparedGraphSet::arguments(vec!["imported".into()], vec![GraphSetColumnType::Any])
        .unwrap();
    let once = single().apply(typed, vec![0]).unwrap().nested().unwrap();
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&once).unwrap();
    assert_eq!(plan.source_count(), 1);
    assert_eq!(plan.procedure_source().unwrap().call().namespace(), "fnx");
    assert!(AsyncSpillSetPlan::compile(&once).is_err());

    let list_input = PreparedGraphSet::singleton()
        .unwind(
            "xs".into(),
            GraphSetValue::List(vec![GraphSetValue::List(Vec::new())]),
        )
        .unwrap();
    let repeated = [
        PreparedGraphSet::singleton()
            .apply(single(), Vec::new())
            .unwrap(),
        PreparedGraphSet::singleton()
            .optional_apply(single(), Vec::new())
            .unwrap(),
        list_input.collect_apply(single(), Vec::new(), 0).unwrap(),
        PreparedGraphSet::singleton()
            .apply(
                single()
                    .apply(PreparedGraphSet::singleton(), Vec::new())
                    .unwrap(),
                Vec::new(),
            )
            .unwrap(),
    ];
    for query in repeated {
        for query in [query.clone(), query.with_page(0, Some(0))] {
            assert!(matches!(
                AsyncSpillSetPlan::compile_with_single_procedure(&query),
                Err(SpillSetBuildError::Unsupported { .. })
            ));
        }
    }

    let two = [
        single()
            .cross_join(call("fnx", "other", Vec::new()))
            .unwrap(),
        single()
            .combine(GraphSetOperation::Union, GraphSetQuantifier::All, single())
            .unwrap(),
    ];
    for query in two {
        assert!(
            AsyncSpillSetPlan::compile_with_single_procedure(&query.with_page(0, Some(0))).is_err()
        );
    }
    let unsupported = PreparedGraphSet::owned(Vec::new(), Vec::new())
        .cross_join(single())
        .unwrap()
        .with_page(0, Some(0));
    assert!(AsyncSpillSetPlan::compile_with_single_procedure(&unsupported).is_err());
}

#[test]
fn selected_products_keep_the_single_capability_and_original_source_order() {
    let left = PreparedGraphSet::singleton()
        .unwind(
            "left".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(scalar(7))]),
        )
        .unwrap();
    let query = left
        .cross_join(call("fnx", "right", Vec::new()).with_page(1, Some(3)))
        .unwrap()
        .nested()
        .unwrap()
        .filter(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(0),
            comparison: crate::algebra::IntegerComparison::Equal,
            right: GraphSetOperand::Column(1),
        }])
        .unwrap()
        .with_page(2, Some(0));
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("the native selected-product seam must remain selected");
    };
    assert!(cross.has_selection());
    assert_eq!(cross.left().source_count(), 0);
    assert_eq!(cross.right().source_count(), 1);
    assert_eq!(cross.right().stages()[0].offset(), 1);
    assert_eq!(cross.right().stages()[0].count(), Some(3));
    assert_eq!(plan.source_count(), 1);
    assert_eq!(plan.procedure_source().unwrap().columns(), ["right"]);
    assert_eq!(plan.stages()[0].offset(), 2);
    assert_eq!(plan.stages()[0].count(), Some(0));
    assert!(AsyncSpillSetPlan::compile(&query).is_err());
    let repeated = PreparedGraphSet::singleton()
        .apply(query, Vec::new())
        .unwrap();
    assert!(AsyncSpillSetPlan::compile_with_single_procedure(&repeated).is_err());
}

#[test]
fn lazy_arguments_equal_the_native_host_values_and_every_control_can_refuse() {
    let payload = GraphValue::Map {
        keys: vec!["nested".into(), "payload".into()].into(),
        values: vec![
            GraphValue::List(
                vec![
                    scalar(3),
                    GraphValue::Scalar(CanonicalScalar::Null),
                    scalar(3),
                ]
                .into(),
            ),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![0x71; 257]).unwrap()),
        ]
        .into(),
    };
    let query = call(
        "fnx",
        "value",
        vec![
            GraphSetValue::Value(payload.clone()),
            expression(7, GraphIntegerBinary::Add, 5),
        ],
    );
    let transcript = query.canonical_bytes();
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
    let source = plan.procedure_source().unwrap();
    let mut native_arguments = None;
    query
        .execute_governed_with_procedures(
            wide(),
            |_, _| -> Result<_, GqlQueryError<Infallible, usize>> {
                panic!("this argument law has no graph source")
            },
            |_, arguments, _| {
                native_arguments = Some(arguments.to_vec());
                Ok(GqlQueryExecution {
                    value: vec![GraphValueRow::from_owned_values(vec![scalar(1)])],
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 1,
                    },
                    evaluator: GlaExecutionStats::default(),
                })
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    let mut events = Vec::new();
    let actual = source
        .evaluate_arguments(&mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(actual, [payload, scalar(12)]);
    assert_eq!(Some(actual.clone()), native_arguments);
    assert_eq!(events.first(), Some(&GlaExecutionEvent::ScratchEntry));
    for stop in 0..events.len() {
        let mut seen = 0;
        assert!(matches!(source.evaluate_arguments(&mut |event| {
            assert_eq!(event, events[seen]);
            let current = seen;
            seen += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        }), Err(SpillSetRowError::Control(at)) if at == stop));
        assert_eq!(seen, stop + 1);
        assert_eq!(query.canonical_bytes(), transcript);
    }
    assert_eq!(
        source
            .evaluate_arguments(&mut |_| Ok::<_, usize>(()))
            .unwrap(),
        actual
    );
}

#[test]
fn argument_failures_stay_lazy_typed_and_visible_beneath_empty_pages() {
    let query = call(
        "fnx",
        "value",
        vec![
            GraphSetValue::Value(scalar(7)),
            expression(1, GraphIntegerBinary::Divide, 0),
        ],
    )
    .with_page(0, Some(0));
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
    let error = plan
        .procedure_source()
        .unwrap()
        .evaluate_arguments(&mut |_| Ok::<_, ()>(()))
        .unwrap_err();
    let SpillSetRowError::Native(expected) = error else {
        panic!("native argument arithmetic must remain typed");
    };
    assert!(
        matches!(&expected, GraphSetExecutionError::Projection { row: 0, column: 1, error }
        if error.kind == GraphIntegerErrorKind::DivisionByZero)
    );
    let mut called = false;
    let native = query.execute_governed_with_procedures(
        wide(),
        |_, _| -> Result<_, GqlQueryError<Infallible, ()>> {
            panic!("this argument law has no graph source")
        },
        |_, _, _| {
            called = true;
            Ok(GqlQueryExecution {
                value: Vec::new(),
                rows: GqlExecutionStats::default(),
                evaluator: GlaExecutionStats::default(),
            })
        },
        || Ok::<_, ()>(()),
    );
    let Err(GqlQueryError::Source(actual)) = native else {
        panic!("the argument fails before the procedure host is reached");
    };
    assert_eq!(actual, expected);
    assert!(!called);
}

#[test]
fn repeated_capability_admits_all_apply_kinds_without_widening_existing_profiles() {
    let single = || call("fnx", "value", Vec::new());
    let lists = PreparedGraphSet::singleton()
        .unwind(
            "items".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Null,
            ))]),
        )
        .unwrap();
    let queries = [
        PreparedGraphSet::singleton().apply(single(), Vec::new()).unwrap(),
        PreparedGraphSet::singleton().optional_apply(single(), Vec::new()).unwrap(),
        lists.collect_apply(single(), Vec::new(), 0).unwrap(),
        PreparedGraphSet::singleton()
            .apply(
                PreparedGraphSet::singleton().apply(single(), Vec::new()).unwrap(),
                Vec::new(),
            )
            .unwrap(),
    ];
    for query in queries {
        for query in [query.clone(), query.with_page(0, Some(0))] {
            assert!(AsyncSpillSetPlan::compile(&query).is_err());
            assert!(AsyncSpillSetPlan::compile_with_single_procedure(&query).is_err());
            assert!(AsyncSpillSetPlan::compile_with_independent_procedures(&query).is_err());
            assert!(
                AsyncSpillSetPlan::compile_with_independent_procedures_and_bounded_walks(&query)
                    .is_err()
            );
            for plan in [
                AsyncSpillSetPlan::compile_with_repeated_procedures(&query).unwrap(),
                AsyncSpillSetPlan::compile_with_repeated_procedures_and_bounded_walks(&query)
                    .unwrap(),
            ] {
                assert_eq!(plan.source_count(), 1);
                assert_eq!(plan.procedure_source().unwrap().columns(), ["value"]);
                let mut visited = 0;
                assert!(plan.all_procedure_sources(&mut |source| {
                    assert_eq!(source.call().namespace(), "fnx");
                    visited += 1;
                    true
                }));
                assert_eq!(visited, 1);
                assert!(!plan.all_procedure_sources(&mut |_| false));
            }
        }
    }
    assert!(
        AsyncSpillSetPlan::compile_with_repeated_procedures(&PreparedGraphSet::singleton())
            .is_err()
    );
    let unsupported = PreparedGraphSet::singleton()
        .apply(
            PreparedGraphSet::owned(Vec::new(), Vec::new())
                .cross_join(single())
                .unwrap(),
            Vec::new(),
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(AsyncSpillSetPlan::compile_with_repeated_procedures(&unsupported).is_err());
}

#[test]
fn repeated_procedure_arguments_remain_constant_and_lazy() {
    assert!(
        PreparedGraphSet::arguments(vec!["outer".into()], vec![GraphSetColumnType::Any])
            .unwrap()
            .procedure_call(
                "fnx".into(),
                "rows".into(),
                vec![GraphSetValue::Column(0)],
                Vec::new(),
                vec![("value".into(), "result".into())],
                Vec::new(),
            )
            .is_err()
    );
    let query = PreparedGraphSet::singleton()
        .apply(
            call("fnx", "value", vec![expression(1, GraphIntegerBinary::Divide, 0)]),
            Vec::new(),
        )
        .unwrap()
        .with_page(0, Some(0));
    let plan = AsyncSpillSetPlan::compile_with_repeated_procedures(&query).unwrap();
    assert!(matches!(
        plan.procedure_source().unwrap().evaluate_arguments(&mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(GraphSetExecutionError::Projection { error, .. }))
            if error.kind == GraphIntegerErrorKind::DivisionByZero
    ));
}

#[test]
fn repeated_optional_and_null_collection_keep_native_body_invocation_laws() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let optional = PreparedGraphSet::singleton()
        .optional_apply(call("fnx", "value", Vec::new()).with_page(0, Some(0)), Vec::new())
        .unwrap();
    let mut calls = 0;
    let answer = optional
        .execute_governed_with_procedures(
            wide(),
            |_, _| -> Result<_, GqlQueryError<Infallible, ()>> {
                panic!("no graph source")
            },
            |_, _, _| {
                calls += 1;
                Ok(GqlQueryExecution {
                    value: vec![GraphValueRow::from_owned_values(vec![scalar(7)])],
                    rows: GqlExecutionStats { snapshot_records: 1, result_rows: 1 },
                    evaluator: GlaExecutionStats::default(),
                })
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(answer.value[0].values(), [null.clone()]);
    let plan = AsyncSpillSetPlan::compile_with_repeated_procedures(&optional).unwrap();
    let AsyncSpillSetSourcePlan::Apply(apply) = plan.source() else { unreachable!() };
    assert_eq!(apply.kind(), SpillSetApplyKind::Optional);
    assert_eq!(
        apply.null_extend(&GraphValueRow::unit(), &mut |_| Ok::<_, ()>(())).unwrap().values(),
        answer.value[0].values(),
    );

    let collected = PreparedGraphSet::singleton()
        .unwind("items".into(), GraphSetValue::List(vec![GraphSetValue::Value(null.clone())]))
        .unwrap()
        .collect_apply(
            call("fnx", "value", vec![expression(1, GraphIntegerBinary::Divide, 0)]),
            Vec::new(),
            0,
        )
        .unwrap();
    let plan = AsyncSpillSetPlan::compile_with_repeated_procedures(&collected).unwrap();
    let mut definitions = 0;
    assert!(plan.all_procedure_sources(&mut |_| { definitions += 1; true }));
    assert_eq!(definitions, 1);
    let answer = collected
        .execute_governed_with_procedures(
            wide(),
            |_, _| -> Result<_, GqlQueryError<Infallible, ()>> {
                panic!("no graph source")
            },
            |_, _, _| {
                panic!("a saved NULL list cannot evaluate its failing procedure body");
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(answer.value[0].values(), [null.clone(), null.clone()]);
    let AsyncSpillSetSourcePlan::Apply(apply) = plan.source() else { unreachable!() };
    let left = GraphValueRow::from_owned_values(vec![null]);
    assert!(apply.collect_input_is_null(&left, &mut |_| Ok::<_, ()>(())).unwrap());
    assert_eq!(
        apply.null_collect(&left, &mut |_| Ok::<_, ()>(())).unwrap().values(),
        answer.value[0].values(),
    );
}
