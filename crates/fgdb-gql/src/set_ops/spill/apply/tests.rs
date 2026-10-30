use super::*;
use fgdb_types::{CanonicalScalar, VId};

fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn arguments(names: &[&str], types: &[GraphSetColumnType]) -> PreparedGraphSet {
    PreparedGraphSet::arguments(
        names.iter().map(|name| (*name).into()).collect(),
        types.to_vec(),
    )
    .unwrap()
}

fn apply(kind: SpillSetApplyKind) -> SpillSetApplyPlan {
    let left = arguments(
        &["payload", "owner", "absent"],
        &[
            GraphSetColumnType::Any,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Scalar,
        ],
    );
    let right = arguments(
        &["who", "value", "again", "missing"],
        &[
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Any,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Scalar,
        ],
    );
    let query = match kind {
        SpillSetApplyKind::Inner => left.apply(right, vec![1, 0, 1, 2]),
        SpillSetApplyKind::Optional => left.optional_apply(right, vec![1, 0, 1, 2]),
        SpillSetApplyKind::Collect { .. } => panic!("the pair fixture has four body columns"),
    }
    .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    assert!(plan.source_pattern().is_none());
    assert!(plan.source_tail().is_none());
    assert_eq!(plan.source_count(), 0);
    assert_eq!(plan.source().kind(), ScanKind::Set);
    let AsyncSpillSetSourcePlan::Apply(apply) = plan.source() else {
        panic!("correlation must remain a native Apply source");
    };
    assert_eq!(plan.source_columns(), apply.columns());
    assert_eq!(apply.imports(), &[1, 0, 1, 2]);
    assert_eq!(apply.kind(), kind);
    assert_eq!(plan.stages().len(), 1);
    assert_eq!(plan.stages()[0].order(), None);
    apply.clone()
}

fn left_row() -> GraphValueRow {
    row(vec![
        GraphValue::List(
            vec![
                scalar(3),
                GraphValue::Scalar(CanonicalScalar::Null),
                scalar(3),
            ]
            .into_boxed_slice(),
        ),
        GraphValue::Vertex(VId(19)),
        GraphValue::Scalar(CanonicalScalar::Null),
    ])
}

#[test]
fn imports_preserve_position_repetition_nulls_and_owned_payloads() {
    let apply = apply(SpillSetApplyKind::Inner);
    let left = left_row();
    let frame = apply
        .prepare_arguments(&left, 7, &mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        frame.values(),
        &[
            GraphValue::Vertex(VId(19)),
            GraphValue::List(
                vec![
                    scalar(3),
                    GraphValue::Scalar(CanonicalScalar::Null),
                    scalar(3)
                ]
                .into_boxed_slice()
            ),
            GraphValue::Vertex(VId(19)),
            GraphValue::Scalar(CanonicalScalar::Null),
        ]
    );
    let AsyncSpillSetSourcePlan::Arguments(arguments) = apply.right().source() else {
        panic!("body keeps its typed argument leaf");
    };
    for _ in 0..3 {
        let cursor = arguments
            .start(Some(&frame), &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let (copied, cursor) = cursor
            .next_with_control(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(copied, frame);
        assert!(
            cursor
                .next_with_control(&mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
    }
    let output = apply
        .copy_pair(&left, &frame, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    let mut expected = left.values().to_vec();
    expected.extend_from_slice(frame.values());
    assert_eq!(output.values(), expected);
    // Copying a leaf repeatedly neither consumes nor mutates its input frame.
    assert_eq!(left, left_row());
}

#[test]
fn optional_absence_is_one_null_cell_per_body_column_and_keeps_left_values() {
    let apply = apply(SpillSetApplyKind::Optional);
    let left = left_row();
    let output = apply
        .null_extend(&left, &mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(output.len(), 7);
    assert_eq!(&output.values()[..3], left.values());
    assert!(output.values()[3..].iter().all(GraphValue::is_null));
    assert_eq!(apply.column_types()[3], GraphSetColumnType::Vertex);
    assert_eq!(apply.column_types()[4], GraphSetColumnType::Any);
}

#[test]
fn arguments_require_present_frame_but_zero_columns_and_nulls_are_occurrences() {
    let query = arguments(&[], &[]).with_page(0, Some(0));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Arguments(arguments) = plan.source() else {
        panic!("zero-column arguments are not a Values replacement");
    };
    assert_eq!(plan.stages()[0].count(), Some(0));
    assert_eq!(plan.source_count(), 0);
    assert_eq!(plan.source_columns(), &[] as &[String]);
    assert!(matches!(
        arguments.start(None, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::ArgumentsUnavailable
        ))
    ));
    let empty = GraphValueRow::unit();
    let cursor = arguments
        .start(Some(&empty), &mut |_| Ok::<_, ()>(()))
        .unwrap();
    let (copied, cursor) = cursor
        .next_with_control(&mut |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    assert!(copied.values().is_empty());
    assert!(
        cursor
            .next_with_control(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn malformed_frames_and_pair_schemas_refuse_before_any_payload_copy() {
    let apply = apply(SpillSetApplyKind::Inner);
    let left = left_row();
    let frame = apply
        .prepare_arguments(&left, 0, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    let AsyncSpillSetSourcePlan::Arguments(arguments) = apply.right().source() else {
        unreachable!();
    };
    for wrong in [
        GraphValueRow::unit(),
        row(vec![
            scalar(19),
            scalar(3),
            GraphValue::Vertex(VId(19)),
            scalar(0),
        ]),
    ] {
        let mut copies = 0;
        assert!(matches!(
            arguments.start(Some(&wrong), &mut |event| {
                copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            }),
            Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 }
            ))
        ));
        assert_eq!(copies, 0);
        assert!(matches!(
            apply.copy_pair(&left, &wrong, &mut |_| Ok::<_, ()>(())),
            Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 1 }
            ))
        ));
    }
    let wrong_left = row(vec![scalar(3), scalar(19), scalar(0)]);
    assert!(matches!(
        apply.prepare_arguments(&wrong_left, 11, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 0 }
        ))
    ));
    assert!(matches!(
        apply.copy_pair(&wrong_left, &frame, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 0 }
        ))
    ));
}

#[test]
fn every_copy_control_refusal_is_terminal_without_consuming_its_inputs() {
    let apply = apply(SpillSetApplyKind::Optional);
    let left = left_row();
    let frame = apply
        .prepare_arguments(&left, 0, &mut |_| Ok::<_, usize>(()))
        .unwrap();
    let AsyncSpillSetSourcePlan::Arguments(arguments) = apply.right().source() else {
        unreachable!();
    };
    for operation in 0..4 {
        let run = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| match operation
        {
            0 => apply.prepare_arguments(&left, 0, &mut |event| control(event)),
            1 => apply.copy_pair(&left, &frame, &mut |event| control(event)),
            2 => apply.null_extend(&left, &mut |event| control(event)),
            _ => {
                let cursor = arguments.start(Some(&frame), &mut |event| control(event))?;
                let (row, _) = cursor
                    .next_with_control(&mut |event| control(event))?
                    .expect("a copied frame owns exactly one occurrence");
                Ok(row)
            }
        };
        let mut events = 0;
        run(&mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            events += 1;
            Ok(())
        })
        .unwrap();
        for stop in 0..events {
            let mut calls = 0;
            assert!(matches!(
                run(&mut |_| {
                    let at = calls;
                    calls += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                }),
                Err(SpillSetRowError::Control(at)) if at == stop
            ));
            assert_eq!(calls, stop + 1);
            assert_eq!(left, left_row());
        }
    }
}

#[test]
fn collection_admits_while_overwide_apply_refuses_before_empty_output_pages() {
    let input = PreparedGraphSet::singleton()
        .project(
            vec![GraphSetProjection::new(
                "items",
                GraphSetValue::Value(GraphValue::List(vec![scalar(1)].into_boxed_slice())),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(&["list"], &[GraphSetColumnType::List])
        .project(
            vec![GraphSetProjection::new(
                "member",
                GraphSetValue::Value(scalar(1)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let collected = input.collect_apply(body, vec![0], 0).unwrap();
    let admitted = AsyncSpillSetPlan::compile(&collected.clone().with_page(0, Some(0))).unwrap();
    let AsyncSpillSetSourcePlan::Apply(apply) = admitted.source() else {
        panic!("collection keeps its complete correlated source");
    };
    assert_eq!(apply.kind(), SpillSetApplyKind::Collect { null_input: 0 });
    assert_eq!(admitted.stages().last().unwrap().count(), Some(0));
    let projected = collected
        .project(
            vec![GraphSetProjection::new("result", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(AsyncSpillSetPlan::compile(&projected).is_ok());
    let wide = PreparedGraphSet::arguments(
        (0..MAX_PATTERN_VERTICES)
            .map(|at| format!("c{at}"))
            .collect(),
        vec![GraphSetColumnType::Scalar; MAX_PATTERN_VERTICES],
    )
    .unwrap();
    let extra = arguments(&["extra"], &[GraphSetColumnType::Scalar]);
    let query = wide.apply(extra, vec![0]).unwrap().with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 0 })
    ));
}

fn collected_query(input: PreparedGraphSet) -> PreparedGraphSet {
    let body = arguments(&["items"], &[GraphSetColumnType::Any])
        .unwind("member".into(), GraphSetValue::Column(0))
        .unwrap()
        .project_ordered_output(vec![GraphSetProjection::new(
            "mapped",
            GraphSetValue::Column(1),
        )])
        .unwrap();
    input.collect_apply(body, vec![0], 0).unwrap()
}

fn collection() -> SpillSetApplyPlan {
    let query = collected_query(arguments(&["items"], &[GraphSetColumnType::Any]));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Apply(apply) = plan.source() else {
        panic!("collection keeps an Apply source");
    };
    apply.clone()
}

#[test]
fn collected_outputs_match_resident_order_duplicates_nulls_and_compact_members() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let members = vec![
        scalar(3),
        null.clone(),
        scalar(1),
        scalar(3),
        GraphValue::Scalar(CanonicalScalar::bytes(vec![0xa5; 257]).unwrap()),
        GraphValue::map(vec![(
            "nested".into(),
            GraphValue::List(vec![null.clone()].into()),
        )])
        .unwrap(),
    ];
    let inputs = vec![
        GraphValue::List(members.clone().into()),
        GraphValue::List(members.clone().into()),
        GraphValue::List(Box::new([])),
        null.clone(),
        GraphValue::Vertices(vec![VId(u128::MAX), VId(2), VId(u128::MAX)].into()),
        GraphValue::Edges(vec![fgdb_types::EId(u128::MAX), fgdb_types::EId(1)].into()),
    ];
    let input = PreparedGraphSet::singleton()
        .unwind(
            "items".into(),
            GraphSetValue::Value(GraphValue::List(inputs.clone().into())),
        )
        .unwrap();
    let query = collected_query(input);
    let resident =
        query
            .execute_governed(
                crate::GqlQueryPolicy::new(0, inputs.len() as u64, 1_000_000, 1_000_000),
                |_: &PreparedGraphPattern<GraphValueRow>,
                 _: crate::GqlQueryPolicy|
                 -> Result<
                    crate::GqlQueryExecution<GraphValueRow>,
                    crate::GqlQueryError<(), ()>,
                > { panic!("a value-only collection has no graph source") },
                || Ok::<_, ()>(()),
            )
            .unwrap();
    let plan = collection();
    let mut actual = Vec::new();
    for (row_index, input) in inputs.iter().enumerate() {
        let left = row(vec![input.clone()]);
        let mut control = |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            Ok::<_, ()>(())
        };
        if plan.collect_input_is_null(&left, &mut control).unwrap() {
            actual.push(plan.null_collect(&left, &mut control).unwrap());
            continue;
        }
        let mapped: Vec<_> = match input {
            GraphValue::List(values) => values.to_vec(),
            GraphValue::Vertices(values) => {
                values.iter().copied().map(GraphValue::Vertex).collect()
            }
            GraphValue::Edges(values) => values.iter().copied().map(GraphValue::Edge).collect(),
            _ => unreachable!(),
        };
        let mut collector = plan.start_collection(row_index, &mut control).unwrap();
        for member in mapped {
            collector = collector.push(&row(vec![member]), &mut control).unwrap();
        }
        actual.push(collector.finish(&left, &mut control).unwrap());
    }
    assert_eq!(actual, resident.value);
    assert_eq!(
        actual[0].values()[1],
        GraphValue::List(members.clone().into())
    );
    assert_eq!(actual[0], actual[1]);
    assert_eq!(actual[2].values()[1], GraphValue::List(Box::new([])));
    assert!(actual[3].values()[1].is_null());
    assert_eq!(
        actual[4].values()[1],
        GraphValue::List(
            vec![
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(u128::MAX)),
            ]
            .into()
        )
    );
    for output in actual {
        assert!(output.values().iter().all(GraphValue::validate_bounds));
        let encoded = output.canonical_bytes().unwrap();
        assert_eq!(GraphValueRow::decode_canonical(&encoded).unwrap(), output);
    }
}

#[test]
fn collected_null_short_circuit_and_schemas_refuse_before_payload_admission() {
    let plan = collection();
    let null = row(vec![GraphValue::Scalar(CanonicalScalar::Null)]);
    let empty = row(vec![GraphValue::List(Box::new([]))]);
    for (left, expected) in [(&null, true), (&empty, false)] {
        let mut copies = 0;
        assert_eq!(
            plan.collect_input_is_null(left, &mut |event| {
                copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            })
            .unwrap(),
            expected
        );
        assert_eq!(copies, 0);
    }
    assert!(matches!(
        plan.null_collect(&empty, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 0 }
        ))
    ));
    let output = plan.null_collect(&null, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(
        output.values(),
        &[
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Null)
        ]
    );
    for invalid in [GraphValueRow::unit(), row(vec![scalar(1), scalar(2)])] {
        let collector = plan.start_collection(7, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut copies = 0;
        assert!(matches!(
            collector.push(&invalid, &mut |event| {
                copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            }),
            Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 1 }
            ))
        ));
        assert_eq!(copies, 0);
    }
    let collector = plan.start_collection(7, &mut |_| Ok::<_, ()>(())).unwrap();
    assert!(matches!(
        collector.finish(&null, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 0 }
        ))
    ));
}

#[test]
fn collected_bounds_include_the_outer_list_before_member_copy() {
    let plan = collection();
    let left = row(vec![GraphValue::List(Box::new([]))]);
    for depth in [GraphValue::MAX_LIST_DEPTH - 1, GraphValue::MAX_LIST_DEPTH] {
        let mut member = scalar(1);
        for at in 0..depth {
            member = if at % 2 == 0 {
                GraphValue::List(vec![member].into())
            } else {
                GraphValue::map(vec![("v".into(), member)]).unwrap()
            };
        }
        assert!(member.validate_bounds());
        let expected = GraphValue::List(vec![member.clone()].into()).validate_bounds();
        let collector = plan.start_collection(17, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut copies = 0;
        let result = collector.push(&row(vec![member]), &mut |event| {
            copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, ()>(())
        });
        if expected {
            let output = result
                .unwrap()
                .finish(&left, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert!(output.values()[1].validate_bounds());
        } else {
            assert!(matches!(
                result,
                Err(SpillSetRowError::Native(
                    GraphSetExecutionError::Projection {
                        row: 17,
                        column: 1,
                        error: crate::GraphIntegerError {
                            kind: crate::GraphIntegerErrorKind::Overflow,
                            ..
                        }
                    }
                ))
            ));
            assert_eq!(copies, 0);
        }
    }
    // Count nested nodes, not just the number of mapped body rows.
    let wide = row(vec![GraphValue::List(
        vec![scalar(1); GraphValue::MAX_LIST_NODES - 3].into(),
    )]);
    for excess in [false, true] {
        let collector = plan
            .start_collection(23, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .push(&wide, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .push(&row(vec![scalar(2)]), &mut |_| Ok::<_, ()>(()))
            .unwrap();
        if excess {
            let mut copies = 0;
            assert!(matches!(
                collector.push(&row(vec![scalar(3)]), &mut |event| {
                    copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
                    Ok::<_, ()>(())
                }),
                Err(SpillSetRowError::Native(
                    GraphSetExecutionError::Projection {
                        row: 23,
                        column: 1,
                        error: crate::GraphIntegerError {
                            kind: crate::GraphIntegerErrorKind::Overflow,
                            ..
                        }
                    }
                ))
            ));
            assert_eq!(copies, 0);
        } else {
            let output = collector.finish(&left, &mut |_| Ok::<_, ()>(())).unwrap();
            assert!(output.values()[1].validate_bounds());
        }
    }
    for invalid in [
        GraphValue::Map {
            keys: vec!["a".into(), "a".into()].into(),
            values: vec![scalar(1), scalar(2)].into(),
        },
        GraphValue::Map {
            keys: vec!["a".into()].into(),
            values: Box::new([]),
        },
    ] {
        let collector = plan.start_collection(31, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut copies = 0;
        assert!(matches!(
            collector.push(&row(vec![invalid]), &mut |event| {
                copies += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            }),
            Err(SpillSetRowError::Native(
                GraphSetExecutionError::Projection {
                    row: 31,
                    column: 1,
                    error: crate::GraphIntegerError {
                        kind: crate::GraphIntegerErrorKind::Overflow,
                        ..
                    }
                }
            ))
        ));
        assert_eq!(copies, 0);
    }
}

#[test]
fn every_collected_control_refusal_is_terminal_without_mutating_inputs() {
    let plan = collection();
    let left = row(vec![GraphValue::List(vec![scalar(1)].into())]);
    let null = row(vec![GraphValue::Scalar(CanonicalScalar::Null)]);
    let member = row(vec![
        GraphValue::map(vec![(
            "payload".into(),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![0x5a; 257]).unwrap()),
        )])
        .unwrap(),
    ]);
    let saved = (left.clone(), null.clone(), member.clone());
    for operation in 0..3 {
        let run = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| -> Result<(), SpillSetRowError<usize>> {
            match operation {
                0 => plan.collect_input_is_null(&null, &mut |event| control(event)).map(|_| ()),
                1 => plan.null_collect(&null, &mut |event| control(event)).map(|_| ()),
                _ => plan.start_collection(5, &mut |event| control(event))?
                    .push(&member, &mut |event| control(event))?
                    .push(&row(vec![GraphValue::Scalar(CanonicalScalar::Null)]), &mut |event| control(event))?
                    .finish(&left, &mut |event| control(event)).map(|_| ()),
            }
        };
        let mut events = 0;
        run(&mut |event| {
            assert_ne!(event, GlaExecutionEvent::ResultRow);
            events += 1;
            Ok(())
        })
        .unwrap();
        for stop in 0..events {
            let mut calls = 0;
            assert!(matches!(run(&mut |_| {
                let at = calls;
                calls += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            }), Err(SpillSetRowError::Control(at)) if at == stop));
            assert_eq!(calls, stop + 1);
            assert_eq!((&left, &null, &member), (&saved.0, &saved.1, &saved.2));
        }
    }
}

#[test]
fn collected_null_inputs_do_not_bypass_static_child_admission() {
    let input = PreparedGraphSet::singleton()
        .project_ordered_output(vec![GraphSetProjection::new(
            "items",
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        )])
        .unwrap();
    let body = PreparedGraphSet::owned(vec!["mapped".into()], vec![GraphSetColumnType::Any]);
    let query = input
        .collect_apply(body, vec![], 0)
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 1 })
    ));
}
