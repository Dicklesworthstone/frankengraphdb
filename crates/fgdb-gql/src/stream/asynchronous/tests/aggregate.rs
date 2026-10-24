use super::*;
use crate::spill_aggregate::{AsyncSpillAggregatePlan, AsyncVertexSpillAggregateCursor};
use crate::{
    GraphAggregate, GraphAggregateError, GraphAggregateValue, GraphIntegerBinary,
    GraphIntegerExpression, GraphIntegerOp, GraphSetProjection, GraphSetValue,
    PreparedGraphAggregate,
};
use core::convert::Infallible;

fn definition(divide: bool, limit: Option<u64>) -> PreparedGraphAggregate {
    let input = PreparedGraphText::prepare("MATCH (n) RETURN ALL n.p AS p", |kind, name: &str| {
        (kind == GraphSymbolKind::Property && name == "p")
            .then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let expression = GraphIntegerExpression::prepare_scalar(if divide {
        &[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::ScalarColumn(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ]
    } else {
        &[
            GraphIntegerOp::ScalarColumn(0),
            GraphIntegerOp::Literal(Some(3)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
        ]
    })
    .unwrap();
    PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            GraphSetProjection::new("original", GraphSetValue::Column(0)),
            GraphSetProjection::new("computed", GraphSetValue::Integer(expression)),
            GraphSetProjection::new("again", GraphSetValue::Column(0)),
        ],
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::sum_int("total", 1),
        ],
        0,
        limit,
    )
    .unwrap()
}

#[test]
fn async_computed_occurrences_keep_guards_and_one_meter_until_external_reduction_finishes() {
    let AsyncSpillAggregatePlan::Vertex(plan) =
        AsyncSpillAggregatePlan::compile(&definition(false, None)).unwrap()
    else {
        panic!("vertex aggregate");
    };
    let definition = plan.definition().clone();
    assert_eq!(definition.input_width(), 3);
    let input = source(vec![
        sample(1, Some(CanonicalScalar::Int(2)), true),
        sample(2, Some(CanonicalScalar::Int(4)), true),
        sample(3, None, true),
        sample(4, Some(CanonicalScalar::Int(99)), false),
    ]);
    let signals = input.signals.clone();
    let mut cursor = AsyncVertexSpillAggregateCursor::new(
        input,
        plan,
        GqlQueryPolicy::new(4, 1, 100_000, 100_000),
        || Ok::<_, ()>(()),
    );
    let mut state = definition
        .new_state(&mut |event| cursor.charge::<Infallible, ()>(event))
        .unwrap();
    let mut computed_scratch = 0;
    let mut retained = Vec::new();
    while let Some(row) = ready(cursor.next_input(&mut |guard, event| {
        assert!(
            guard.0.live_outputs.get() > 0,
            "raw guard precedes every computed allocation"
        );
        if event == VertexScanEvent::ScratchEntry {
            computed_scratch += 1;
        }
        Ok(())
    }))
    .unwrap()
    {
        assert_eq!(row.len(), 3);
        assert_eq!(row.values()[0], row.values()[2]);
        assert_eq!(cursor.row_stats().result_rows, 0);
        definition
            .update(&mut state, &row, &mut |event| {
                cursor.charge::<Infallible, ()>(event)
            })
            .unwrap();
        retained.push(row);
    }
    assert!(computed_scratch > 0);
    assert!(
        signals.dropped.get(),
        "EOF releases source before final groups"
    );
    assert_eq!(signals.live_outputs.get(), 3);
    assert_eq!(cursor.row_stats().snapshot_records, 4);
    assert_eq!(cursor.row_stats().result_rows, 0);
    let actual = definition
        .finish(vec![], state, &mut |event| {
            cursor.charge::<Infallible, ()>(event)
        })
        .unwrap();
    assert_eq!(
        actual.values(),
        &[
            GraphAggregateValue::Count(3),
            GraphAggregateValue::Integer(18)
        ]
    );
    cursor.finish_result::<Infallible, ()>().unwrap();
    assert_eq!(cursor.row_stats().result_rows, 1);
    assert!(matches!(
        cursor.finish_result::<Infallible, ()>(),
        Err(GqlQueryError::Rows(_))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    drop(retained);
    assert_eq!(signals.live_outputs.get(), 0);
}

#[test]
fn async_aggregate_late_expression_failure_is_visible_before_limit_zero_and_releases_source() {
    let AsyncSpillAggregatePlan::Vertex(plan) =
        AsyncSpillAggregatePlan::compile(&definition(true, Some(0))).unwrap()
    else {
        panic!("vertex aggregate");
    };
    let input = source(vec![
        sample(1, Some(CanonicalScalar::Int(2)), true),
        sample(2, Some(CanonicalScalar::Int(0)), true),
        sample(3, Some(CanonicalScalar::Int(1)), true),
    ]);
    let signals = input.signals.clone();
    let mut cursor = AsyncVertexSpillAggregateCursor::new(
        input,
        plan,
        GqlQueryPolicy::new(100, 0, 100_000, 100_000),
        || Ok::<_, ()>(()),
    );
    drop(
        ready(cursor.next_input(&mut |_, _| Ok(())))
            .unwrap()
            .unwrap(),
    );
    assert!(matches!(ready(cursor.next_input(&mut |_, _| Ok(()))),
        Err(GqlQueryError::Source(GraphAggregateError::InputExpression { column: 1, error, .. }))
        if error.kind == crate::GraphIntegerErrorKind::DivisionByZero));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert_eq!(signals.reads.get(), 2);
    assert!(signals.dropped.get());
    assert_eq!(signals.live_outputs.get(), 0);
    assert!(
        ready(cursor.next_input(&mut |_, _| Ok(())))
            .unwrap()
            .is_none()
    );
}

#[test]
fn every_computed_guard_refusal_is_terminal_and_allocation_cannot_outrun_logical_admission() {
    let AsyncSpillAggregatePlan::Vertex(plan) =
        AsyncSpillAggregatePlan::compile(&definition(false, None)).unwrap()
    else {
        panic!("vertex aggregate");
    };
    let rows = || vec![sample(1, Some(CanonicalScalar::Int(2)), true)];
    let mut baseline =
        AsyncVertexSpillAggregateCursor::new(source(rows()), plan.clone(), wide(), || {
            Ok::<_, ()>(())
        });
    let mut events = 0;
    drop(
        ready(baseline.next_input(&mut |_, event| {
            if event == VertexScanEvent::ScratchEntry {
                events += 1;
            }
            Ok(())
        }))
        .unwrap()
        .unwrap(),
    );
    assert!(events > 0);
    for fail_at in 1..=events {
        let input = source(rows());
        let signals = input.signals.clone();
        let mut cursor =
            AsyncVertexSpillAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        let mut observed = 0;
        assert!(matches!(
            ready(cursor.next_input(&mut |guard, event| {
                assert_eq!(guard.0.live_outputs.get(), 1);
                if event == VertexScanEvent::ScratchEntry {
                    observed += 1;
                    if observed == fail_at {
                        return Err("computed memory refused");
                    }
                }
                Ok(())
            })),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                VertexScanError::Source("computed memory refused")
            )))
        ));
        assert_eq!(observed, fail_at);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(signals.dropped.get());
        assert_eq!(signals.live_outputs.get(), 0);
    }
    let input = source(rows());
    let mut cursor = AsyncVertexSpillAggregateCursor::new(
        input,
        plan,
        GqlQueryPolicy::new(100, 0, 0, 100_000),
        || Ok::<_, ()>(()),
    );
    let mut called = false;
    assert!(
        ready(cursor.next_input(&mut |_, _| {
            called = true;
            Ok(())
        }))
        .is_err()
    );
    assert!(
        !called,
        "source logical work refuses before any computed allocation callback"
    );
}

#[test]
fn unwinding_computed_admission_releases_the_vertex_source_and_its_output_guard() {
    let AsyncSpillAggregatePlan::Vertex(plan) =
        AsyncSpillAggregatePlan::compile(&definition(false, None)).unwrap()
    else {
        panic!("vertex aggregate");
    };
    let input = source(vec![sample(1, Some(CanonicalScalar::Int(2)), true)]);
    let signals = input.signals.clone();
    let mut cursor = AsyncVertexSpillAggregateCursor::new(input, plan, wide(), || Ok::<_, ()>(()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ready(cursor.next_input(&mut |_, event| {
            assert_ne!(
                event,
                VertexScanEvent::ScratchEntry,
                "injected host admission unwind"
            );
            Ok(())
        }));
    }));
    assert!(result.is_err());
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(signals.dropped.get());
    assert_eq!(signals.live_outputs.get(), 0);
    assert!(
        ready(cursor.next_input(&mut |_, _| Ok(())))
            .unwrap()
            .is_none()
    );
}
