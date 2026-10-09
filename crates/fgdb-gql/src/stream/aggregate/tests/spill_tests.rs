use super::*;
use crate::spill_aggregate::{
    SpillAggregateBuildError, SpillAggregatePlan, VertexSpillAggregateCursor,
};
use crate::{
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetProjection, GraphSetValue,
};

fn computed_definition() -> PreparedGraphAggregate {
    let scaled = GraphIntegerExpression::prepare_scalar(&[
        GraphIntegerOp::ScalarColumn(0),
        GraphIntegerOp::Literal(Some(3)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    PreparedGraphAggregate::prepare_projected(
        input(),
        vec![
            GraphSetProjection::new("original", GraphSetValue::Column(0)),
            GraphSetProjection::new("scaled", GraphSetValue::Integer(scaled)),
            GraphSetProjection::new("again", GraphSetValue::Column(0)),
        ],
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::min("minimum", 2),
        ],
        0,
        None,
    )
    .unwrap()
}

#[test]
fn spill_computed_columns_use_projected_positions_and_the_existing_numeric_domains() {
    for value in [
        CanonicalScalar::Int(2),
        CanonicalScalar::Float(fgdb_types::CanonicalF64::new(0.5)),
    ] {
        let rows = vec![row(1, Some(value)), row(2, None)];
        let query = computed_definition();
        let expected = expected(&query, &rows);
        let SpillAggregatePlan::Vertex(plan) = SpillAggregatePlan::compile(&query).unwrap() else {
            panic!("vertex input");
        };
        let definition = plan.definition().clone();
        assert_eq!(definition.input_width(), 3);
        let source = source(rows);
        let dropped = source.dropped.clone();
        let mut cursor = VertexSpillAggregateCursor::new(source, plan, wide(), || Ok::<_, ()>(()));
        let mut state = definition
            .new_state(&mut |event| cursor.charge(event))
            .unwrap();
        while let Some(row) = cursor.next_input().unwrap() {
            assert_eq!(row.len(), 3, "source had only one column");
            assert_eq!(cursor.row_stats().result_rows, 0);
            definition
                .update(&mut state, &row, &mut |event| cursor.charge(event))
                .unwrap();
        }
        assert!(dropped.get());
        let actual = definition
            .finish(vec![], state, &mut |event| cursor.charge(event))
            .unwrap();
        assert_eq!(actual, expected);
        cursor.finish_result().unwrap();
        assert_eq!(cursor.row_stats().result_rows, 1);
    }
}

#[test]
fn spill_executes_unused_computed_inputs_before_limit_zero_or_numeric_validation() {
    let divide = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(10)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    let query = PreparedGraphAggregate::prepare_projected(
        input(),
        vec![
            GraphSetProjection::new("used", GraphSetValue::Column(0)),
            GraphSetProjection::new("unused", GraphSetValue::Integer(divide)),
        ],
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    let SpillAggregatePlan::Vertex(plan) = SpillAggregatePlan::compile(&query).unwrap() else {
        panic!("vertex input");
    };
    let source = source(vec![
        row(1, Some(CanonicalScalar::Int(2))),
        row(2, Some(CanonicalScalar::Int(0))),
        row(3, Some(CanonicalScalar::Int(1))),
    ]);
    let dropped = source.dropped.clone();
    let reads = source.reads.clone();
    let mut cursor = VertexSpillAggregateCursor::new(source, plan, wide(), || Ok::<_, ()>(()));
    assert!(cursor.next_input().unwrap().is_some());
    assert!(
        matches!(cursor.next_input(), Err(GqlQueryError::Source(GraphAggregateError::InputExpression { row: 0, column: 1, error })) if error.kind == crate::GraphIntegerErrorKind::DivisionByZero)
    );
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(dropped.get());
    assert_eq!(reads.get(), 2);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next_input().unwrap().is_none());
}

#[test]
fn spill_computed_input_checkpoints_share_the_source_meter_and_release_failed_sources() {
    let SpillAggregatePlan::Vertex(plan) =
        SpillAggregatePlan::compile(&computed_definition()).unwrap()
    else {
        panic!("vertex input");
    };
    let calls = Rc::new(Cell::new(0));
    let seen = calls.clone();
    let mut cursor = VertexSpillAggregateCursor::new(
        source(vec![row(1, Some(CanonicalScalar::Int(7)))]),
        plan.clone(),
        wide(),
        move || {
            seen.set(seen.get() + 1);
            Ok::<_, usize>(())
        },
    );
    let expected = cursor.next_input().unwrap().unwrap();
    let stats = cursor.evaluator_stats();
    for stop in 1..=calls.get() {
        let source = source(vec![row(1, Some(CanonicalScalar::Int(7)))]);
        let dropped = source.dropped.clone();
        let mut at = 0;
        let mut cursor = VertexSpillAggregateCursor::new(source, plan.clone(), wide(), move || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(cursor.next_input(), Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next_input().unwrap().is_none());
    }
    for (work, scratch, succeeds) in [
        (stats.work_units, stats.scratch_entries, true),
        (stats.work_units - 1, stats.scratch_entries, false),
        (stats.work_units, stats.scratch_entries - 1, false),
    ] {
        let mut cursor = VertexSpillAggregateCursor::new(
            source(vec![row(1, Some(CanonicalScalar::Int(7)))]),
            plan.clone(),
            GqlQueryPolicy::new(1, 0, work, scratch),
            || Ok::<_, ()>(()),
        );
        let result = cursor.next_input();
        if succeeds {
            assert_eq!(result.unwrap(), Some(expected.clone()));
        } else {
            assert!(result.is_err());
        }
        assert_eq!(cursor.row_stats().result_rows, 0);
    }
}

fn numeric_definition() -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 0),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::average_int("average", 0),
            GraphAggregate::min("min", 0),
            GraphAggregate::max("max", 0),
        ],
        0,
        None,
    )
    .unwrap()
}

fn reduce(
    rows: Vec<Row>,
    policy: GqlQueryPolicy,
) -> Result<
    (GraphAggregateRow, GqlExecutionStats, GlaExecutionStats),
    VertexAggregateError<&'static str, ()>,
> {
    let SpillAggregatePlan::Vertex(plan) =
        SpillAggregatePlan::compile(&numeric_definition()).unwrap()
    else {
        panic!("vertex input");
    };
    let definition = plan.definition().clone();
    let source = source(rows);
    let dropped = source.dropped.clone();
    let mut cursor = VertexSpillAggregateCursor::new(source, plan, policy, || Ok::<_, ()>(()));
    let mut state = definition.new_state(&mut |event| cursor.charge(event))?;
    while let Some(row) = cursor.next_input()? {
        assert_eq!(cursor.row_stats().result_rows, 0);
        definition.update(&mut state, &row, &mut |event| cursor.charge(event))?;
    }
    assert!(
        dropped.get(),
        "source is released before reduction finalization"
    );
    let output = definition.finish(Vec::new(), state, &mut |event| cursor.charge(event))?;
    cursor.finish_result()?;
    Ok((output, cursor.row_stats(), cursor.evaluator_stats()))
}

#[test]
fn spill_cells_match_existing_numeric_reducer_and_only_charge_final_groups() {
    let float = |value| CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value));
    for values in [
        vec![],
        vec![None, Some(CanonicalScalar::Null)],
        vec![
            Some(CanonicalScalar::Int(i64::MAX)),
            Some(CanonicalScalar::Int(i64::MAX)),
            Some(CanonicalScalar::Int(-3)),
            None,
        ],
        vec![
            Some(CanonicalScalar::Int(3)),
            Some(float(1e30)),
            Some(float(-1e30)),
            Some(float(0.25)),
            None,
        ],
    ] {
        let rows: Vec<_> = values
            .into_iter()
            .enumerate()
            .map(|(at, value)| row(at as u128 + 1, value))
            .collect();
        let expected = VertexAggregateCursor::new(
            source(rows.clone()),
            VertexAggregatePlan::compile(&numeric_definition()).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let (actual, stats, _) =
            reduce(rows, GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(expected, vec![actual]);
        assert_eq!(stats.result_rows, 1);
    }
}

#[test]
fn spill_source_reduction_and_delivery_use_one_cumulative_meter() {
    let rows = vec![
        row(1, Some(CanonicalScalar::Int(9))),
        row(2, None),
        row(3, Some(CanonicalScalar::Int(-4))),
    ];
    let (_, r, e) = reduce(rows.clone(), wide()).unwrap();
    assert!(
        reduce(
            rows.clone(),
            GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries)
        )
        .is_ok()
    );
    for policy in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1),
    ] {
        assert!(reduce(rows.clone(), policy).is_err());
    }
}

#[test]
fn spill_initial_drain_preserves_numeric_domain_error_and_fuses_source() {
    let SpillAggregatePlan::Vertex(plan) =
        SpillAggregatePlan::compile(&numeric_definition()).unwrap()
    else {
        panic!("vertex input");
    };
    let source = source(vec![
        row(1, Some(CanonicalScalar::Int(3))),
        row(2, Some(CanonicalScalar::Bool(true))),
        row(3, Some(CanonicalScalar::Int(5))),
    ]);
    let reads = source.reads.clone();
    let dropped = source.dropped.clone();
    let mut cursor = VertexSpillAggregateCursor::new(source, plan, wide(), || Ok::<_, ()>(()));
    assert!(cursor.next_input().unwrap().is_some());
    assert!(matches!(
        cursor.next_input(),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 2
        }))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(dropped.get());
    assert_eq!(reads.get(), 2);
    assert!(cursor.next_input().unwrap().is_none());
    assert!(cursor.next_input().unwrap().is_none());
    assert_eq!(reads.get(), 2);
}

#[test]
fn spill_reduction_remains_cancellable_after_source_release() {
    let SpillAggregatePlan::Vertex(plan) =
        SpillAggregatePlan::compile(&numeric_definition()).unwrap()
    else {
        panic!("vertex input");
    };
    let definition = plan.definition().clone();
    let interrupted = Rc::new(Cell::new(false));
    let flag = interrupted.clone();
    let mut cursor = VertexSpillAggregateCursor::new(source(vec![]), plan, wide(), move || {
        if flag.get() { Err("cancelled") } else { Ok(()) }
    });
    assert!(cursor.next_input().unwrap().is_none());
    interrupted.set(true);
    assert!(matches!(
        definition.new_state(&mut |event| cursor.charge(event)),
        Err(GqlQueryError::Interrupted("cancelled"))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
}

#[test]
fn spill_rejects_unimplemented_semantics_and_cross_definition_states() {
    let relation = PreparedGraphAggregate::prepare_relation(
        crate::PreparedGraphSet::from(input()),
        &[],
        &[GraphAggregate::count_rows("rows")],
        0,
        None,
    )
    .unwrap();
    assert!(matches!(
        SpillAggregatePlan::compile(&relation),
        Err(SpillAggregateBuildError::Unsupported)
    ));
    let definition = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::collect("value", 0)],
        0,
        None,
    )
    .unwrap();
    assert!(matches!(
        SpillAggregatePlan::compile(&definition),
        Err(SpillAggregateBuildError::Unsupported)
    ));
    // DISTINCT arguments spill through the host's canonical dedup since
    // 4a84095f; collection is still refused above.
    for function in [
        GraphAggregate::count_distinct("value", 0),
        GraphAggregate::sum_int_distinct("value", 0),
    ] {
        let definition =
            PreparedGraphAggregate::prepare(input(), &[], &[function], 0, None).unwrap();
        assert!(SpillAggregatePlan::compile(&definition).is_ok());
    }
    let paged = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    let paged = SpillAggregatePlan::compile(&paged).unwrap();
    assert_eq!(paged.definition().result_window(), (0, Some(0)));
    assert!(paged.definition().has_output_stage());
    let left = SpillAggregatePlan::compile(&numeric_definition()).unwrap();
    let right = SpillAggregatePlan::compile(&numeric_definition()).unwrap();
    let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(());
    let state = left.definition().new_state(&mut control).unwrap();
    assert!(matches!(
        right.definition().finish(vec![], state, &mut control),
        Err(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput
        ))
    ));
    let mut state = left.definition().new_state(&mut control).unwrap();
    let short = GraphValueRow::from_owned_values(vec![]);
    assert!(matches!(
        left.definition().update(&mut state, &short, &mut control),
        Err(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput
        ))
    ));
    assert!(matches!(
        left.definition()
            .finish(vec![GraphValue::Vertex(VId(1))], state, &mut control),
        Err(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput
        ))
    ));
}
