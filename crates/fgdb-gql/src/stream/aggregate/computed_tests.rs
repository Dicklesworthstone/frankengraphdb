use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder};
use crate::{
    GraphAggregate, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphIntegerUnary,
    GraphSetProjection, GraphSetValue,
};
use std::cell::Cell;
use std::rc::Rc;

const P: PropertyKeyId = PropertyKeyId(1);
#[derive(Clone)]
struct Record {
    id: VId,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    visible: bool,
}
struct Source {
    records: Vec<Record>,
    at: usize,
    dropped: Rc<Cell<bool>>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
impl VertexScanSource for Source {
    type Error = ();
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<(), C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let next = self.records.get(self.at).map(|r| r.id);
        if next.is_some() {
            self.at += 1;
        }
        Ok(next)
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<(), C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let row = &self.records[self.at - 1];
        assert_eq!(row.id, id);
        Ok(row.visible.then_some(VertexScanRow {
            labels: &[],
            properties: &row.properties,
        }))
    }
}
fn records(values: &[Option<i64>]) -> Vec<Record> {
    values
        .iter()
        .enumerate()
        .map(|(at, value)| Record {
            id: VId(if at == 3 { u128::MAX } else { at as u128 }),
            properties: value
                .map(|value| vec![(P, CanonicalScalar::Int(value))])
                .unwrap_or_default(),
            visible: true,
        })
        .collect()
}
fn source(records: Vec<Record>) -> Source {
    Source {
        records,
        at: 0,
        dropped: Rc::new(Cell::new(false)),
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn input() -> crate::algebra::PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[
                GraphColumn::property("value", "n", P),
                GraphColumn::vertex("id", "n"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}
fn definition(keys: &[usize]) -> PreparedGraphAggregate {
    let absolute = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Unary(GraphIntegerUnary::Abs),
    ])
    .unwrap();
    let scaled = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Unary(GraphIntegerUnary::Abs),
        GraphIntegerOp::Literal(Some(3)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    PreparedGraphAggregate::prepare_projected(
        input(),
        vec![
            GraphSetProjection::new("key", GraphSetValue::Integer(absolute)),
            GraphSetProjection::new("scaled", GraphSetValue::Integer(scaled)),
            GraphSetProjection::new("id", GraphSetValue::Column(1)),
        ],
        keys,
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::count("present", 1),
            GraphAggregate::count_distinct("support", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("unique_sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::average_int_distinct("unique_average", 1),
            GraphAggregate::min("minimum", 1),
            GraphAggregate::max("last_id", 2),
        ],
        0,
        None,
    )
    .unwrap()
}
fn expected(query: &PreparedGraphAggregate, rows: &[Record]) -> Vec<GraphAggregateRow> {
    query
        .execute_governed(
            rows.len() as u64,
            rows.iter().filter(|r| r.visible).map(|r| r.id),
            [],
            |_, _| Ok::<_, ()>(true),
            |id, key| {
                Ok(rows
                    .iter()
                    .find(|r| r.id == id)
                    .and_then(|r| r.properties.iter().find(|(k, _)| *k == key).map(|(_, v)| v)))
            },
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn computed_vertex_groups_match_batch_across_nullable_visible_and_duplicate_values() {
    let query = definition(&[0]);
    let plan = VertexAggregatePlan::compile(&query).unwrap();
    let choices = [None, Some(-2), Some(-1), Some(0), Some(2)];
    for code in 0..625 {
        let mut digits = code;
        let values: Vec<_> = (0..4)
            .map(|_| {
                let v = choices[digits % 5];
                digits /= 5;
                v
            })
            .collect();
        let mut rows = records(&values);
        for (at, row) in rows.iter_mut().enumerate() {
            row.visible = (code + at) % 7 != 0;
        }
        let want = expected(&query, &rows);
        let mut cursor =
            VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || Ok::<_, ()>(()));
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            want
        );
        assert_eq!(cursor.row_stats().result_rows, want.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    }
    let rows = records(&[Some(-2), Some(2), None, Some(1)]);
    for keys in [&[0, 2][..], &[2, 0][..], &[][..]] {
        let query = definition(keys);
        let want = expected(&query, &rows);
        let input = source(rows.clone());
        let dropped = input.dropped.clone();
        let mut cursor = VertexAggregateCursor::new(
            input,
            VertexAggregatePlan::compile(&query).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(cursor.next().unwrap().unwrap(), want[0]);
        assert!(
            dropped.get(),
            "completed input must release its source before delivery"
        );
        assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), want[1..]);
    }
}

#[test]
fn every_computed_vertex_checkpoint_and_one_less_quota_refuses_and_fuses() {
    let query = definition(&[0]);
    let plan = VertexAggregatePlan::compile(&query).unwrap();
    let rows = records(&[Some(-2), Some(2), None]);
    let want = expected(&query, &rows);
    let mut total = 0;
    let mut baseline =
        VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
            total += 1;
            Ok::<_, usize>(())
        });
    assert_eq!(
        baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        want
    );
    let r = baseline.row_stats();
    let e = baseline.evaluator_stats();
    drop(baseline);
    let exact = GqlQueryPolicy::new(
        r.snapshot_records,
        r.result_rows,
        e.work_units,
        e.scratch_entries,
    );
    assert_eq!(
        VertexAggregateCursor::new(
            source(rows.clone()),
            plan.clone(),
            exact,
            || Ok::<_, ()>(())
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        want
    );
    for stop in 1..=total {
        let input = source(rows.clone());
        let dropped = input.dropped.clone();
        let mut calls = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), exact, || {
            calls += 1;
            if calls == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next().expect("selected checkpoint must be reached") {
                Ok(row) => prefix.push(row),
                Err(GqlQueryError::Interrupted(at)) => {
                    assert_eq!(at, stop);
                    break;
                }
                Err(other) => panic!("unexpected refusal {other:?}"),
            }
        }
        assert!(want.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        cursor.close();
        drop(cursor);
        assert_eq!(calls, stop);
    }
    for quota in [
        GqlQueryPolicy::new(r.snapshot_records - 1, r.result_rows, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut cursor =
            VertexAggregateCursor::new(source(rows.clone()), plan.clone(), quota, || {
                Ok::<_, ()>(())
            });
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn plain_values_do_not_gain_projection_copies_and_computed_failures_keep_local_context() {
    let query = PreparedGraphAggregate::prepare(
        input(),
        &[],
        &[GraphAggregate::sum_int("sum", 0)],
        0,
        None,
    )
    .unwrap();
    let mut scratch = None;
    for count in [0, 1, 100] {
        let rows = (0..count)
            .map(|id| Record {
                id: VId(id),
                properties: vec![(P, CanonicalScalar::Int(7))],
                visible: true,
            })
            .collect();
        let mut cursor = VertexAggregateCursor::new(
            source(rows),
            VertexAggregatePlan::compile(&query).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        cursor.next().unwrap().unwrap();
        assert_eq!(
            *scratch.get_or_insert(cursor.evaluator_stats().scratch_entries),
            cursor.evaluator_stats().scratch_entries
        );
    }
    let query = definition(&[0]);
    let mut rows = records(&[Some(-1), Some(i64::MIN), Some(i64::MAX)]);
    rows[1].visible = false; // Invisible input must not execute ABS(MIN).
    let input = source(rows);
    let dropped = input.dropped.clone();
    let mut cursor = VertexAggregateCursor::new(
        input,
        VertexAggregatePlan::compile(&query).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(
            GraphAggregateError::InputExpression {
                row: 0,
                column: 1,
                ..
            }
        )))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    let input = source(records(&[Some(1)]));
    let dropped = input.dropped.clone();
    let mut cursor = VertexAggregateCursor::new(
        input,
        VertexAggregatePlan::compile(&query).unwrap(),
        wide(),
        || -> Result<(), ()> { panic!("close must not drive expressions") },
    );
    cursor.close();
    assert!(dropped.get());
    assert!(cursor.next().is_none());
}
