//! Aggregate inputs must use the same admitted records as ordinary row scans.
use super::*;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
use std::cell::Cell;
use std::rc::Rc;

const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 5] = [VId(0), VId(1), VId(2), VId(3), VId(u128::MAX)];
struct Masked {
    position: usize,
    props: Vec<Vec<(PropertyKeyId, CanonicalScalar)>>,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    fail_at: Option<VId>,
}
impl Masked {
    fn new() -> Self {
        Self {
            position: 0,
            props: [Some(7), Some(3), Some(999), Some(7), None]
                .into_iter()
                .map(|value| {
                    let mut row = Vec::new();
                    if let Some(value) = value {
                        row.push((P, CanonicalScalar::Int(value)));
                    }
                    row.push((
                        H,
                        CanonicalScalar::ucs_basic_text("hidden-nonnumeric").unwrap(),
                    ));
                    row
                })
                .collect(),
            reads: Rc::new(Cell::new(0)),
            dropped: Rc::new(Cell::new(false)),
            fail_at: None,
        }
    }
}
impl Drop for Masked {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}
impl VertexScanSource for Masked {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let id = IDS.get(self.position).copied();
        self.position += usize::from(id.is_some());
        Ok(id)
    }
    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        panic!("aggregate bypassed owned masked records");
    }
    fn vertex_record<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        self.reads.set(self.reads.get() + 1);
        if self.fail_at == Some(id) {
            return Err(VertexScanSourceError::Source("record refused"));
        }
        if id == VId(2) {
            return Ok(None);
        }
        let at = IDS.iter().position(|candidate| *candidate == id).unwrap();
        VertexScanRecord::copy_masked(
            VertexScanRow {
                labels: &[LabelId(1), LabelId(99)],
                properties: &self.props[at],
            },
            |id| id == LabelId(1),
            |key| key == P,
            control,
        )
        .map(Some)
        .map_err(VertexScanSourceError::Control)
    }
}
fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => {
            Some(GraphSymbol::Relation(fgdb_delta_types::RelationId(1)))
        }
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn run(text: &str) -> Vec<GraphAggregateRow> {
    VertexAggregateCursor::new(
        Masked::new(),
        VertexAggregatePlan::compile(&query(text)).unwrap(),
        policy(),
        || Ok::<_, usize>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

#[test]
fn all_eleven_cells_group_only_visible_masked_occurrences() {
    let text = "MATCH (n) WHERE n.hidden IS NULL RETURN n.hidden AS k,COUNT(*) AS rows,COUNT(n.p) AS present,SUM(n.p) AS total,AVG(n.p) AS average,MIN(n.p) AS lo,MAX(n.p) AS hi,COUNT(DISTINCT n.p) AS support,SUM(DISTINCT n.p) AS unique_sum,AVG(DISTINCT n.p) AS unique_avg,COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS unique_items GROUP BY n.hidden";
    use GraphAggregateValue::{Average, Count, Integer, Value};
    assert_eq!(
        run(text),
        vec![GraphAggregateRow::from_group_values(
            vec![null()],
            vec![
                Count(4),
                Count(3),
                Integer(17),
                Average(GraphExactAverage::new(17, 3).unwrap()),
                Value(int(3)),
                Value(int(7)),
                Count(2),
                Integer(10),
                Average(GraphExactAverage::new(10, 2).unwrap()),
                Value(GraphValue::List(
                    vec![int(7), int(3), int(7)].into_boxed_slice()
                )),
                Value(GraphValue::List(vec![int(7), int(3)].into_boxed_slice())),
            ]
        )]
    );
    assert_eq!(
        run("MATCH (n) RETURN COUNT(n.hidden) AS present,SUM(n.hidden) AS total"),
        vec![GraphAggregateRow::from_global_values(vec![
            Count(0),
            Value(null())
        ])]
    );
    assert_eq!(
        run("MATCH (n:H) RETURN COUNT(*) AS n"),
        vec![GraphAggregateRow::from_global_values(vec![Count(0)])]
    );
    assert!(
        run("MATCH (n) WHERE NOT (n.hidden = 7) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p")
            .is_empty()
    );
}

#[test]
fn masked_values_precede_computed_input_having_and_ranked_delivery() {
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use crate::{
        GraphAggregate, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
        GraphSetProjection, GraphSetValue,
    };
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("hidden", "n", H),
                GraphColumn::property("p", "n", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let expression = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Add),
    ])
    .unwrap();
    let definition = PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            GraphSetProjection::new("key", GraphSetValue::Integer(expression)),
            GraphSetProjection::new("value", GraphSetValue::Column(1)),
        ],
        &[0],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::sum_int("sum", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let rows = VertexAggregateCursor::new(
        Masked::new(),
        VertexAggregatePlan::compile(&definition).unwrap(),
        policy(),
        || Ok::<_, usize>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(
        rows,
        vec![GraphAggregateRow::from_group_values(
            vec![null()],
            vec![
                GraphAggregateValue::Count(4),
                GraphAggregateValue::Integer(17),
            ]
        )]
    );
    let rows = run(
        "MATCH (n) RETURN n.p AS key,COUNT(*) AS n GROUP BY n.p HAVING COUNT(*) > 1 ORDER BY key DESC LIMIT 1",
    );
    assert_eq!(
        rows,
        vec![GraphAggregateRow::from_group_values(
            vec![int(7)],
            vec![GraphAggregateValue::Count(2)]
        )]
    );
}

#[test]
fn one_record_at_a_time_and_late_failures_release_no_partial_group_even_under_limit_zero() {
    for suffix in ["", " LIMIT 0"] {
        let definition = query(&format!("MATCH (n) RETURN COUNT(*) AS n{suffix}"));
        let mut source = Masked::new();
        source.fail_at = Some(VId(u128::MAX));
        let reads = Rc::clone(&source.reads);
        let dropped = Rc::clone(&source.dropped);
        let mut cursor = VertexAggregateCursor::new(
            source,
            VertexAggregatePlan::compile(&definition).unwrap(),
            policy(),
            || Ok::<_, usize>(()),
        );
        assert_eq!(reads.get(), 0);
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
                VertexScanError::Source("record refused")
            ))))
        ));
        assert!(dropped.get());
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let source = Masked::new();
    let reads = Rc::clone(&source.reads);
    let dropped = Rc::clone(&source.dropped);
    let mut cursor = VertexAggregateCursor::new(
        source,
        VertexAggregatePlan::compile(&query("MATCH (n) RETURN COUNT(*) AS n")).unwrap(),
        policy(),
        || Ok::<_, usize>(()),
    );
    cursor.close();
    assert_eq!(reads.get(), 0);
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    let definition =
        query("MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN COUNT(*) AS n");
    let mut cursor = VertexAggregateCursor::new(
        Masked::new(),
        VertexAggregatePlan::compile(&definition).unwrap(),
        policy(),
        || Ok::<_, usize>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
            VertexScanError::Probe(crate::edge_stream::EdgeScanError::ExpansionUnavailable)
        ))))
    ));
}

#[test]
fn all_cancellation_cuts_and_inclusive_native_budgets_use_the_masked_record_path() {
    let definition =
        query("MATCH (n) RETURN n.p AS key,SUM(n.p) AS total GROUP BY n.p ORDER BY key DESC");
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let mut calls = 0;
    let (expected, rows, evaluator) = {
        let mut cursor = VertexAggregateCursor::new(Masked::new(), plan.clone(), policy(), || {
            calls += 1;
            Ok::<_, usize>(())
        });
        let result = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        (result, cursor.row_stats(), cursor.evaluator_stats())
    };
    assert!(calls > 0);
    for cut in 1..=calls {
        let mut seen = 0;
        let mut cursor = VertexAggregateCursor::new(Masked::new(), plan.clone(), policy(), || {
            seen += 1;
            if seen == cut { Err(cut) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => {
                    assert_eq!(at, cut);
                    break;
                }
                other => panic!("expected interruption at {cut}, got {other:?}"),
            }
        }
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    );
    assert_eq!(
        VertexAggregateCursor::new(Masked::new(), plan.clone(), exact, || Ok::<_, usize>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    for limits in [
        GqlQueryPolicy::new(
            rows.snapshot_records - 1,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries,
        ),
        GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows - 1,
            evaluator.work_units,
            evaluator.scratch_entries,
        ),
        GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units - 1,
            evaluator.scratch_entries,
        ),
        GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries - 1,
        ),
    ] {
        assert!(
            VertexAggregateCursor::new(Masked::new(), plan.clone(), limits, || Ok::<_, usize>(()))
                .collect::<Result<Vec<_>, _>>()
                .is_err()
        );
    }
}
