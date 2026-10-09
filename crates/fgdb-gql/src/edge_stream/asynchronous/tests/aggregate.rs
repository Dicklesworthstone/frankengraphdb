use super::*;
use crate::spill_aggregate::{AsyncEdgeSpillAggregateCursor, AsyncSpillAggregatePlan};
use crate::stream::VertexScanEvent;
use crate::{
    GraphAggregateError, GraphAggregateValue, PreparedGraphAggregate, PreparedGraphAggregateText,
};
use core::convert::Infallible;

fn definition(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn async_edge_aggregate_inputs_keep_native_orientation_filters_and_computed_values() {
    for (pattern, direction) in [
        ("(a)-[r:R]->(b)", GlaDirection::Forward),
        ("(a)<-[r:R]-(b)", GlaDirection::Reverse),
        ("(a)-[r:R]-(b)", GlaDirection::Undirected),
    ] {
        let query = definition(&format!(
            "MATCH {pattern} WHERE a <> b RETURN COUNT(*) AS rows, SUM(r.p * 2) AS total"
        ));
        let AsyncSpillAggregatePlan::Edge(plan) = AsyncSpillAggregatePlan::compile(&query).unwrap()
        else {
            panic!("edge aggregate");
        };
        let definition = plan.definition().clone();
        let mut source = Source::new(inputs());
        source.expected_columns = query.input_pattern().columns().len();
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeSpillAggregateCursor::new(
            source,
            plan,
            GqlQueryPolicy::new(100, 1, 100_000, 100_000),
            ok,
        );
        let mut state = definition
            .new_state(&mut |event| cursor.charge::<Infallible, ()>(event))
            .unwrap();
        let mut observed_scratch = 0;
        let mut count = 0_u64;
        while let Some(row) = run(cursor.next_input(&mut |guard, event| {
            assert_eq!(guard.0.guards.load(Ordering::SeqCst), 1);
            if event == VertexScanEvent::ScratchEntry {
                observed_scratch += 1;
            }
            Ok(())
        }))
        .unwrap()
        {
            definition
                .update(&mut state, &row, &mut |event| {
                    cursor.charge::<Infallible, ()>(event)
                })
                .unwrap();
            count += 1;
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
        }
        let rows: Vec<_> = oracle(&inputs(), direction, false, 0, 0, usize::MAX)
            .into_iter()
            .filter(|row| row[1] != row[2])
            .collect();
        let expected_sum = rows
            .iter()
            .filter_map(|row| match row[3] {
                GraphValue::Scalar(CanonicalScalar::Int(value)) => Some(i128::from(value) * 2),
                _ => None,
            })
            .sum::<i128>();
        assert_eq!(count, rows.len() as u64);
        assert!(observed_scratch > 0);
        assert_eq!(cursor.row_stats().snapshot_records, inputs().len() as u64);
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.maximum_records.load(Ordering::SeqCst), 1);
        let result = definition
            .finish(vec![], state, &mut |event| {
                cursor.charge::<Infallible, ()>(event)
            })
            .unwrap();
        assert_eq!(
            result.values(),
            &[
                GraphAggregateValue::Count(count),
                GraphAggregateValue::Integer(expected_sum)
            ]
        );
        cursor.finish_result::<Infallible, ()>().unwrap();
        assert_eq!(cursor.row_stats().result_rows, 1);
    }
}

#[test]
fn async_edge_aggregate_refusals_keep_typed_failures_and_release_pending_orientation() {
    let query =
        definition("MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS rows, SUM(r.p * 2) AS total LIMIT 0");
    let AsyncSpillAggregatePlan::Edge(plan) = AsyncSpillAggregatePlan::compile(&query).unwrap()
    else {
        panic!("edge aggregate");
    };
    let mut source = Source::new(inputs());
    source.expected_columns = query.input_pattern().columns().len();
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeSpillAggregateCursor::new(
        source,
        plan.clone(),
        GqlQueryPolicy::new(100, 0, 100_000, 100_000),
        ok,
    );
    assert!(matches!(
        run(cursor.next_input(&mut |guard, event| {
            assert_eq!(guard.0.guards.load(Ordering::SeqCst), 1);
            if event == VertexScanEvent::ScratchEntry {
                Err("computed memory")
            } else {
                Ok(())
            }
        })),
        Err(GqlQueryError::Source(GraphAggregateError::Source(
            EdgeScanError::Source("computed memory")
        )))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert!(
        run(cursor.next_input(&mut |_, _| Ok(())))
            .unwrap()
            .is_none()
    );

    let mut source = Source::new(inputs());
    source.expected_columns = query.input_pattern().columns().len();
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeSpillAggregateCursor::new(source, plan, policy(), ok);
    let mut control = |_: &mut Guard, _: VertexScanEvent| Ok(());
    let mut pull = Box::pin(cursor.next_input(&mut control));
    assert!(
        pull.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(pull);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn async_edge_aggregate_selects_complete_inputs_and_reduction_requires_exhaustion() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[:R*1..2]->(c) RETURN COUNT(*) AS rows LIMIT 0",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN COUNT(*) AS rows LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(DISTINCT r.p) AS rows LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS rows LIMIT 0",
    ] {
        assert!(
            AsyncSpillAggregatePlan::compile(&definition(text)).is_err(),
            "{text}"
        );
    }
    assert!(matches!(
        AsyncSpillAggregatePlan::compile(&definition(
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN COUNT(*) AS rows LIMIT 0"
        )),
        Ok(AsyncSpillAggregatePlan::Join(_))
    ));
    assert!(matches!(
        AsyncSpillAggregatePlan::compile(&definition(
            "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r.p) AS rows LIMIT 0"
        )),
        Ok(AsyncSpillAggregatePlan::Edge(_))
    ));
    let query = definition("MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS rows");
    let AsyncSpillAggregatePlan::Edge(plan) = AsyncSpillAggregatePlan::compile(&query).unwrap()
    else {
        panic!("edge aggregate");
    };
    let source = Source::new(inputs());
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeSpillAggregateCursor::new(source, plan, policy(), ok);
    assert!(matches!(
        cursor.finish_result::<Infallible, ()>(),
        Err(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput
        ))
    ));
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn unwinding_computed_admission_releases_pending_undirected_orientation() {
    let query = definition("MATCH (a)-[r:R]-(b) RETURN SUM(r.p * 2) AS total");
    let AsyncSpillAggregatePlan::Edge(plan) = AsyncSpillAggregatePlan::compile(&query).unwrap()
    else {
        panic!("edge aggregate");
    };
    let mut source = Source::new(inputs());
    source.expected_columns = query.input_pattern().columns().len();
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeSpillAggregateCursor::new(source, plan, policy(), ok);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = run(cursor.next_input(&mut |_, event| {
            assert_ne!(
                event,
                VertexScanEvent::ScratchEntry,
                "injected host admission unwind"
            );
            Ok(())
        }));
    }));
    assert!(result.is_err());
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}
