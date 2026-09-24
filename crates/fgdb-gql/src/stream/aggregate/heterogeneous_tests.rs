//! Computed Any values retain the ordinary canonical MIN/MAX laws.
use super::*;
use crate::algebra::{GraphColumn, GraphPath, GraphPatternBuilder};
use crate::{GraphAggregate, GraphSetProjection, GraphSetValue};

const INDEX: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn values() -> Vec<GraphValue> {
    use fgdb_types::EId;
    vec![
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(-9)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("text").unwrap()),
        GraphValue::Vertex(VId(0)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Path(GraphPath::new(
            VId(0),
            vec![(EId(u128::MAX), VId(1))].into_boxed_slice(),
        )),
        GraphValue::Vertices(vec![VId(0), VId(u128::MAX)].into_boxed_slice()),
        GraphValue::Edges(vec![EId(u128::MAX)].into_boxed_slice()),
        GraphValue::Edge(EId(0)),
        GraphValue::List(Box::new([])),
        GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Null), GraphValue::Vertex(VId(7))]
                .into_boxed_slice(),
        ),
    ]
}
fn reduce(values: &[&GraphValue], maximum: bool) -> GraphAggregateValue {
    let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(());
    let kind = if maximum {
        GraphAggregateFunction::Max
    } else {
        GraphAggregateFunction::Min
    };
    let mut state = NumericState::new_governed(kind, &mut control).unwrap();
    for value in values {
        state
            .update_governed(Input::from_value(value), 0, &mut control)
            .unwrap();
    }
    state.finish_governed(&mut control).unwrap()
}

#[test]
fn mixed_native_domain_triples_agree_with_graph_value_order_in_both_directions() {
    let domain = values();
    for first in &domain {
        for second in &domain {
            for third in &domain {
                let inputs = [first, second, third];
                for maximum in [false, true] {
                    let ordered = inputs.into_iter().filter(|value| !value.is_null());
                    let expected = if maximum { ordered.max() } else { ordered.min() };
                    let expected = expected
                        .cloned()
                        .unwrap_or(GraphValue::Scalar(CanonicalScalar::Null));
                    assert_eq!(reduce(&inputs, maximum), GraphAggregateValue::Value(expected));
                }
            }
        }
    }
}

struct Source {
    rows: Vec<(VId, Vec<(PropertyKeyId, CanonicalScalar)>)>,
    at: usize,
}
impl Source {
    fn new() -> Self {
        Self {
            rows: [Some(1), Some(0), Some(2), Some(1), None]
                .into_iter()
                .enumerate()
                .map(|(at, index)| {
                    let id = if at == 4 { u128::MAX } else { u128::try_from(at).unwrap() };
                    let properties = index
                        .map(|index| vec![(INDEX, CanonicalScalar::Int(index))])
                        .unwrap_or_default();
                    (VId(id), properties)
                })
                .collect(),
            at: 0,
        }
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
        let next = self.rows.get(self.at).map(|row| row.0);
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
        Ok(self.rows.iter().find(|row| row.0 == id).map(|row| VertexScanRow {
            labels: &[],
            properties: &row.1,
        }))
    }
}
fn definition(grouped: bool) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("index", "n", INDEX),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            GraphSetProjection::new(
                "value",
                GraphSetValue::Index {
                    list: Box::new(GraphSetValue::List(vec![
                        GraphSetValue::Column(0),
                        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
                        GraphSetValue::Value(GraphValue::List(Box::new([]))),
                    ])),
                    index: Box::new(GraphSetValue::Column(1)),
                },
            ),
            GraphSetProjection::new(
                "key",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(0))),
            ),
        ],
        if grouped { &[1] } else { &[] },
        &[GraphAggregate::min("low", 0), GraphAggregate::max("high", 0)],
        0,
        None,
    )
    .unwrap()
}
fn expected(query: &PreparedGraphAggregate) -> Vec<GraphAggregateRow> {
    let source = Source::new();
    query
        .execute_governed(
            5,
            source.rows.iter().map(|row| row.0),
            [],
            |_, _| Ok::<_, ()>(true),
            |id, key| {
                Ok(source.rows.iter().find(|row| row.0 == id).and_then(|row| {
                    row.1.iter().find(|(at, _)| *at == key).map(|(_, value)| value)
                }))
            },
            policy(),
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value
}

#[test]
fn checked_any_list_index_inputs_work_in_global_and_grouped_physical_aggregates() {
    for grouped in [false, true] {
        let query = definition(grouped);
        let mut cursor = VertexAggregateCursor::new(
            Source::new(),
            VertexAggregatePlan::compile(&query).unwrap(),
            policy(),
            || Ok::<_, usize>(()),
        );
        let got = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(got, expected(&query));
        assert_eq!(
            got[0].values(),
            &[
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
                GraphAggregateValue::Value(GraphValue::List(Box::new([]))),
            ]
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn every_mixed_input_cursor_cut_and_inclusive_native_limit_remains_fail_closed() {
    let query = definition(false);
    let plan = VertexAggregatePlan::compile(&query).unwrap();
    let mut total = 0;
    let (rows, stats) = {
        let mut cursor = VertexAggregateCursor::new(Source::new(), plan.clone(), policy(), || {
            total += 1;
            Ok::<_, usize>(())
        });
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        (rows, cursor.evaluator_stats())
    };
    for cut in 1..=total {
        let mut seen = 0;
        let mut cursor = VertexAggregateCursor::new(Source::new(), plan.clone(), policy(), || {
            seen += 1;
            if seen == cut { Err(cut) } else { Ok(()) }
        });
        assert!(matches!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>(),
            Err(GqlQueryError::Interrupted(at)) if at == cut
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let exact = GqlQueryPolicy::new(5, 1, stats.work_units, stats.scratch_entries);
    let mut cursor = VertexAggregateCursor::new(
        Source::new(), plan.clone(), exact, || Ok::<_, usize>(()),
    );
    assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), rows);
    for denied in [
        GqlQueryPolicy::new(5, 1, stats.work_units - 1, stats.scratch_entries),
        GqlQueryPolicy::new(5, 1, stats.work_units, stats.scratch_entries - 1),
    ] {
        let mut cursor = VertexAggregateCursor::new(
            Source::new(), plan.clone(), denied, || Ok::<_, usize>(()),
        );
        assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(cursor.state(), VertexScanState::Failed);
    }
}

#[test]
fn mixed_domain_replacements_admit_payload_before_mutation_and_never_copy_losers() {
    let prior = GraphValue::List(Box::new([]));
    let candidate = GraphValue::Scalar(
        CanonicalScalar::ucs_basic_text(&"visible".repeat(100)).unwrap(),
    );
    let initial = || {
        let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(());
        let mut state = NumericState::new_governed(GraphAggregateFunction::Min, &mut control)
            .unwrap();
        state.update_governed(Input::from_value(&prior), 0, &mut control).unwrap();
        state
    };
    let mut total = 0;
    initial().update_governed(Input::from_value(&candidate), 0, &mut |_| {
        total += 1;
        Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(())
    }).unwrap();
    for cut in 1..=total {
        let mut state = initial();
        let mut seen = 0;
        let result = state.update_governed(Input::from_value(&candidate), 0, &mut |_| {
            seen += 1;
            if seen == cut {
                Err(GqlQueryError::<GraphAggregateError<()>, _>::Interrupted(cut))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == cut));
        assert_eq!(
            state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
            GraphAggregateValue::Value(prior.clone())
        );
    }
    let mut state = NumericState::new_governed(
        GraphAggregateFunction::Max, &mut |_| Ok::<_, ()>(()),
    ).unwrap();
    state.update_governed(Input::from_value(&prior), 0, &mut |_| {
        Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(())
    }).unwrap();
    let mut scratch = 0;
    state.update_governed(Input::from_value(&candidate), 0, &mut |event| {
        scratch += usize::from(event == VertexScanEvent::ScratchEntry);
        Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(())
    }).unwrap();
    assert_eq!(scratch, 0, "a losing candidate must not be cloned for its domain comparison");
    assert_eq!(
        state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap(),
        GraphAggregateValue::Value(prior)
    );
}
