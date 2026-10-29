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
fn collection_and_overwide_apply_refuse_before_empty_output_pages() {
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
    assert!(matches!(
        AsyncSpillSetPlan::compile(&collected.clone().with_page(0, Some(0))),
        Err(SpillSetBuildError::Unsupported { depth: 0 })
    ));
    let projected = collected
        .project(
            vec![GraphSetProjection::new("result", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&projected),
        Err(SpillSetBuildError::Unsupported { depth: 1 })
    ));
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
