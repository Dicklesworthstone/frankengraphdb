use super::*;
use core::task::{Context, Poll, Waker};

#[test]
fn cooperative_intake_preserves_groups_and_the_exact_cumulative_meter() {
    let rows: Vec<_> = (0..257)
        .map(|id| row(id, Some(CanonicalScalar::Int((id % 7) as i64))))
        .collect();
    let definition = PreparedGraphAggregate::prepare(
        input(),
        &[0],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::average_int("avg", 0),
            GraphAggregate::count_distinct("support", 0),
            GraphAggregate::collect("items", 0),
            GraphAggregate::collect_distinct("unique", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let mut direct = VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
        Ok::<_, ()>(())
    });
    let expected: Vec<_> = direct.by_ref().map(Result::unwrap).collect();
    let input = source(rows);
    let reads = Rc::clone(&input.reads);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, plan, wide(), || Ok::<_, ()>(()));
    let mut context = Context::from_waker(Waker::noop());
    assert!(cursor.poll_next(&mut context).is_pending());
    assert_eq!(reads.get(), 64);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(!dropped.get());
    let mut got = Vec::new();
    loop {
        match cursor.poll_next(&mut context) {
            Poll::Pending => assert_eq!(cursor.row_stats().result_rows, 0),
            Poll::Ready(Some(Ok(row))) => got.push(row),
            Poll::Ready(Some(Err(error))) => panic!("{error:?}"),
            Poll::Ready(None) => break,
        }
    }
    assert_eq!(got, expected);
    assert_eq!(cursor.row_stats(), direct.row_stats());
    assert_eq!(cursor.evaluator_stats(), direct.evaluator_stats());
    assert!(dropped.get());
}

#[test]
fn rejected_candidates_yield_and_cancellation_discards_the_unfinished_summary() {
    let mut rows: Vec<_> = (0..257).map(|id| row(id, None)).collect();
    for row in &mut rows {
        row.labels.clear();
    }
    let input = source(rows);
    let reads = Rc::clone(&input.reads);
    let dropped = Rc::clone(&input.dropped);
    let cancel = Rc::new(Cell::new(false));
    let control = Rc::clone(&cancel);
    let mut cursor = VertexAggregateCursor::new(
        input,
        VertexAggregatePlan::compile(&definition()).unwrap(),
        wide(),
        move || if control.get() { Err(()) } else { Ok(()) },
    );
    let mut context = Context::from_waker(Waker::noop());
    assert!(cursor.poll_next(&mut context).is_pending());
    assert_eq!(reads.get(), 64);
    cancel.set(true);
    assert!(matches!(
        cursor.poll_next(&mut context),
        Poll::Ready(Some(Err(GqlQueryError::Interrupted(()))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(dropped.get());
    assert!(matches!(cursor.poll_next(&mut context), Poll::Ready(None)));
    assert!(cursor.next().is_none());
}
