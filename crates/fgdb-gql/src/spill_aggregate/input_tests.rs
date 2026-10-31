//! Row-local input -> external-reducer boundary, independently of scratch I/O.
//! The native integration tests separately force authenticated partition replay.
use super::*;
use crate::algebra::GlaDirection;
use crate::edge_stream::{
    EdgeExpansionSourceError, EdgeScanRow, EdgeScanSource, EdgeScanSourceError,
};
use crate::stream::{VertexScanRecord, VertexScanRow, VertexScanSource, VertexScanSourceError};
use crate::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphPipelineAggregateText,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CommitSeq, EId, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

const P: PropertyKeyId = PropertyKeyId(1);
const HIDDEN: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);

struct Source {
    rows: Vec<Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)>>,
    position: usize,
    fault: Option<usize>,
    drops: Arc<AtomicUsize>,
}
impl Source {
    fn new(values: &[Option<i64>]) -> Self {
        Self {
            rows: values
                .iter()
                .map(|value| {
                    let mut row = Vec::new();
                    if let Some(value) = value {
                        row.push((P, CanonicalScalar::Int(*value).into()));
                    }
                    row.push((HIDDEN, CanonicalScalar::Bool(true).into()));
                    row
                })
                .collect(),
            position: 0,
            fault: None,
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl VertexScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.position == self.rows.len() {
            return Ok(None);
        }
        let id = VId(self.position as u128);
        self.position += 1;
        Ok(Some(id))
    }
    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        panic!("spill WITH bypassed the source-admitted masked record");
    }
    fn vertex_record<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fault.is_some_and(|at| id.0 == at as u128) {
            return Err(VertexScanSourceError::Source("record unavailable"));
        }
        let Some(properties) = usize::try_from(id.0).ok().and_then(|at| self.rows.get(at)) else {
            return Ok(None);
        };
        VertexScanRecord::copy_masked(
            VertexScanRow {
                labels: &[],
                properties,
            },
            |_| true,
            |key| key == P,
            control,
        )
        .map(Some)
        .map_err(VertexScanSourceError::Control)
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.position == self.rows.len() {
            return Ok(None);
        }
        let id = EId(self.position as u128);
        self.position += 1;
        Ok(Some(id))
    }
    fn edge<C>(
        &self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fault.is_some_and(|at| id.0 == at as u128) {
            return Err(EdgeScanSourceError::Source("edge unavailable"));
        }
        Ok(usize::try_from(id.0)
            .ok()
            .and_then(|at| self.rows.get(at))
            .map(|properties| EdgeScanRow {
                source: VId(id.0),
                target: VId(id.0 + 1),
                relation: R,
                properties,
            }))
    }
    fn vertex<C>(
        &self,
        id: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok((id.0 <= self.rows.len() as u128).then_some(VertexScanRow {
            labels: &[],
            properties: &[],
        }))
    }
    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        for at in 0..self.rows.len() {
            control(GlaExecutionEvent::Work).map_err(|error| {
                EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error))
            })?;
            let id = EId(at as u128);
            if after.is_some_and(|previous| id <= previous) {
                continue;
            }
            if (direction != GlaDirection::Reverse && endpoint == VId(id.0))
                || (direction != GlaDirection::Forward && endpoint == VId(id.0 + 1))
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphPipelineAggregateText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(HIDDEN)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn prepare_query(edge: bool, tail: &str) -> PreparedGraphAggregate {
    let head = if edge {
        "MATCH (a)-[r:R]->(b) WITH r.p AS p"
    } else {
        "MATCH (n) WITH n.p AS p"
    };
    prepare(&format!("{head} {tail}"))
}
const GROUPS: &str = "WHERE p>=0 WITH 77 AS extra,p/2 AS bucket,p*2 AS value \
    RETURN bucket,COUNT(*) AS n,SUM(value) AS total,COUNT(DISTINCT value) AS support GROUP BY bucket";

// Instantiate exactly the same assertions against both typed source adapters.
// No trait erasure, replacement evaluator or mapped/flattened error type.
macro_rules! with_input {
    ($query:expr, $source:expr, $policy:expr, $checkpoint:expr, |$cursor:ident| $body:block) => {{
        match SpillAggregatePlan::compile($query).unwrap() {
            SpillAggregatePlan::Vertex(plan) => {
                let mut $cursor =
                    VertexSpillAggregateCursor::new($source, plan, $policy, $checkpoint);
                $body
            }
            SpillAggregatePlan::Edge(plan) => {
                let mut $cursor =
                    EdgeSpillAggregateCursor::new($source, plan, $policy, $checkpoint);
                $body
            }
        }
    }};
}

#[test]
fn spill_inputs_preserve_every_occurrence_and_use_the_final_widened_schema() {
    for edge in [false, true] {
        let query = prepare_query(edge, GROUPS);
        for encoded in 0..256 {
            let mut code = encoded;
            let values: [Option<i64>; 4] = std::array::from_fn(|_| {
                let value = [None, Some(-2), Some(0), Some(3)][code % 4];
                code /= 4;
                value
            });
            let expected: Vec<_> = values
                .iter()
                .flatten()
                .filter(|value| **value >= 0)
                .map(|value| vec![int(77), int(value / 2), int(value * 2)])
                .collect();
            let source = Source::new(&values);
            let drops = source.drops.clone();
            with_input!(&query, source, wide(), || Ok::<_, usize>(()), |cursor| {
                assert_eq!(cursor.definition().input_width(), 3);
                assert_eq!(cursor.definition().group_key_columns(), &[1]);
                assert_eq!(
                    cursor
                        .definition()
                        .distinct_argument_columns()
                        .collect::<Vec<_>>(),
                    vec![2]
                );
                let actual: Vec<_> = cursor
                    .by_ref()
                    .map(|row| row.unwrap().values().to_vec())
                    .collect();
                assert_eq!(actual, expected, "source={edge}, fixture={encoded}");
                assert_eq!(cursor.row_stats().snapshot_records, 4);
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert_eq!(cursor.snapshot_seq(), CommitSeq(7));
                assert!(cursor.next_input().unwrap().is_none());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            });
        }
    }
}

#[test]
fn partition_reduction_and_distinct_replay_do_not_execute_with_expressions_twice() {
    for edge in [false, true] {
        let query = prepare_query(edge, GROUPS);
        with_input!(
            &query,
            Source::new(&[Some(-2), Some(0), Some(3), Some(3), None, Some(2)]),
            wide(),
            || Ok::<_, usize>(()),
            |cursor| {
                let definition = cursor.definition().clone();
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                // Model only the host's ownership/order contract, not its I/O.
                // Reverse the partition replay order; the source VM is NOT called.
                let mut groups = BTreeMap::new();
                let mut support = BTreeSet::new();
                for row in rows.iter().rev() {
                    let key = vec![row.values()[1].clone()];
                    if !groups.contains_key(&key) {
                        let state = definition
                            .new_state(&mut |event| cursor.charge(event))
                            .unwrap();
                        groups.insert(key.clone(), state);
                    }
                    let state = groups.get_mut(&key).unwrap();
                    definition
                        .update(state, row, &mut |event| cursor.charge(event))
                        .unwrap();
                    if support.insert((key, row.values()[2].clone())) {
                        definition
                            .update_distinct_argument(state, row, 2, &mut |event| {
                                cursor.charge(event)
                            })
                            .unwrap();
                    }
                }
                let actual: Vec<_> = groups
                    .into_iter()
                    .map(|(key, state)| {
                        definition
                            .finish(key, state, &mut |event| cursor.charge(event))
                            .unwrap()
                    })
                    .collect();
                use GraphAggregateValue::{Count, Integer};
                assert_eq!(
                    actual,
                    vec![
                        GraphAggregateRow::from_group_values(
                            vec![int(0)],
                            vec![Count(1), Integer(0), Count(1)]
                        ),
                        GraphAggregateRow::from_group_values(
                            vec![int(1)],
                            vec![Count(3), Integer(16), Count(2)]
                        ),
                    ]
                );
                assert_eq!(cursor.row_stats().result_rows, 0);
                for _ in &actual {
                    cursor.finish_result().unwrap();
                }
                assert_eq!(cursor.row_stats().result_rows, 2);
            }
        );
    }
}

#[test]
fn filters_are_not_eof_and_masked_values_precede_the_first_with_stage() {
    for edge in [false, true] {
        let query = prepare_query(
            edge,
            "WHERE p>0 WITH 12/p AS value RETURN SUM(value) AS total",
        );
        with_input!(
            &query,
            Source::new(&[Some(0), None, Some(-1), Some(3), Some(4)]),
            wide(),
            || Ok::<_, usize>(()),
            |cursor| {
                let actual: Vec<_> = cursor
                    .by_ref()
                    .map(|row| row.unwrap().values().to_vec())
                    .collect();
                assert_eq!(actual, vec![vec![int(4)], vec![int(3)]]);
                assert_eq!(cursor.row_stats().snapshot_records, 5);
                assert_eq!(cursor.row_stats().result_rows, 0);
            }
        );
    }
    let masked = prepare(
        "MATCH (n) WITH n.hidden AS secret,n.p AS p WHERE secret IS NULL \
        WITH p*2 AS value RETURN SUM(value) AS total",
    );
    with_input!(
        &masked,
        Source::new(&[Some(3), Some(4)]),
        wide(),
        || Ok::<_, usize>(()),
        |cursor| {
            let actual: Vec<_> = cursor
                .by_ref()
                .map(|row| row.unwrap().values().to_vec())
                .collect();
            assert_eq!(actual, vec![vec![int(6)], vec![int(8)]]);
        }
    );
}

#[test]
fn late_projection_source_and_domain_errors_fuse_before_any_final_result() {
    for edge in [false, true] {
        for tail in [
            "WITH 12/p AS unused WHERE FALSE RETURN COUNT(*) AS n",
            "WHERE p>=0 WITH 12/p AS unused RETURN COUNT(*) AS n LIMIT 0",
        ] {
            let query = prepare_query(edge, tail);
            let source = Source::new(&[Some(3), Some(0)]);
            let drops = source.drops.clone();
            with_input!(&query, source, wide(), || Ok::<_, usize>(()), |cursor| {
                let result = cursor.by_ref().collect::<Result<Vec<_>, _>>();
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphAggregateError::InputRelation(_)))
                ));
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert!(cursor.next_input().unwrap().is_none());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert!(cursor.finish_result().is_err());
            });
        }
        let query = prepare_query(edge, "WHERE FALSE RETURN COUNT(*) AS n LIMIT 0");
        let mut source = Source::new(&[Some(1), Some(2)]);
        source.fault = Some(1);
        with_input!(&query, source, wide(), || Ok::<_, usize>(()), |cursor| {
            assert!(matches!(
                cursor.next_input(),
                Err(GqlQueryError::Source(GraphAggregateError::Source(_)))
            ));
            assert!(cursor.next_input().unwrap().is_none());
            assert_eq!(cursor.row_stats().result_rows, 0);
        });
        let query = prepare_query(edge, "RETURN SUM(p) AS total LIMIT 0");
        let mut source = Source::new(&[Some(1), Some(2)]);
        source.rows[1][0].1 = CanonicalScalar::Bool(true).into();
        with_input!(&query, source, wide(), || Ok::<_, usize>(()), |cursor| {
            assert!(cursor.next_input().unwrap().is_some());
            assert!(matches!(
                cursor.next_input(),
                Err(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { .. }
                ))
            ));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next_input().unwrap().is_none());
        });
    }
}

#[test]
fn input_reduction_and_delivery_share_exact_limits_and_every_interruption_cut() {
    for edge in [false, true] {
        let query = prepare_query(edge, "WHERE p>=0 WITH p*2 AS value RETURN COUNT(*) AS n");
        let values = [None, Some(-1), Some(0), Some(2), Some(3)];
        let mut calls = 0;
        let (rows, stats) = with_input!(
            &query,
            Source::new(&values),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
            |cursor| {
                while cursor.next_input().unwrap().is_some() {}
                cursor.charge(VertexScanEvent::Work).unwrap();
                cursor.finish_result().unwrap();
                (cursor.row_stats(), cursor.evaluator_stats())
            }
        );
        for stop in 1..=calls {
            let mut seen = 0;
            let source = Source::new(&values);
            let drops = source.drops.clone();
            with_input!(
                &query,
                source,
                wide(),
                || {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
                |cursor| {
                    let result = (|| {
                        while cursor.next_input()?.is_some() {}
                        cursor.charge(VertexScanEvent::Work)?;
                        cursor.finish_result()
                    })();
                    assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
                    assert!(cursor.next_input().unwrap().is_none());
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                }
            );
            assert_eq!(seen, stop);
        }
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            stats.work_units,
            stats.scratch_entries,
        );
        for (policy, succeeds) in [
            (exact, true),
            (
                GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
                false,
            ),
            (
                GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
                false,
            ),
            (
                GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
                false,
            ),
            (
                GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
                false,
            ),
        ] {
            with_input!(
                &query,
                Source::new(&values),
                policy,
                || Ok::<_, usize>(()),
                |cursor| {
                    let result = (|| {
                        while cursor.next_input()?.is_some() {}
                        cursor.charge(VertexScanEvent::Work)?;
                        cursor.finish_result()
                    })();
                    assert_eq!(result.is_ok(), succeeds);
                    assert!(cursor.next_input().unwrap().is_none());
                }
            );
        }
    }
}

#[test]
fn close_never_drains_and_empty_or_fully_filtered_input_reaches_real_eof() {
    for edge in [false, true] {
        let query = prepare_query(edge, "WHERE p<0 RETURN COUNT(*) AS n");
        for values in [vec![], vec![Some(1); 512]] {
            let source = Source::new(&values);
            let drops = source.drops.clone();
            with_input!(
                &query,
                source,
                GqlQueryPolicy::new(values.len() as u64, 1, 1_000_000, 1_000_000),
                || Ok::<_, usize>(()),
                |cursor| {
                    assert!(cursor.next_input().unwrap().is_none());
                    assert_eq!(cursor.row_stats().snapshot_records, values.len() as u64);
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                    cursor.finish_result().unwrap();
                    assert_eq!(cursor.row_stats().result_rows, 1);
                }
            );
        }
        let source = Source::new(&[Some(1)]);
        let drops = source.drops.clone();
        with_input!(
            &query,
            source,
            wide(),
            || -> Result<(), usize> {
                panic!("closed input driven");
            },
            |cursor| {
                cursor.close();
                assert!(cursor.next_input().unwrap().is_none());
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        );
        with_input!(
            &query,
            Source::new(&[Some(1)]),
            wide(),
            || Ok::<_, usize>(()),
            |cursor| {
                assert!(matches!(
                    cursor.finish_result(),
                    Err(GqlQueryError::Source(
                        GraphAggregateError::InvalidReductionInput
                    ))
                ));
                assert!(cursor.next_input().unwrap().is_none());
                assert_eq!(cursor.row_stats().snapshot_records, 0);
            }
        );
    }
}

#[test]
fn synchronous_fusion_never_erases_intermediate_barriers_or_changes_async_ownership() {
    for edge in [false, true] {
        let query = prepare_query(edge, GROUPS);
        assert!(SpillAggregatePlan::compile(&query).is_ok());
        // The async relation driver owns its complete WITH program. Its
        // definition must not apply the source-only program a second time.
        let physical = query.prepare_complete_group_output().unwrap();
        let externally_evaluated = SpillAggregateDefinition::from_physical(physical).unwrap();
        assert!(externally_evaluated.input_pipeline.is_none());
        for tail in [
            "WITH DISTINCT p AS p RETURN COUNT(*) AS n",
            "WITH p AS p ORDER BY p RETURN COUNT(*) AS n",
            "WITH p AS p LIMIT 0 RETURN COUNT(*) AS n",
            "WITH p AS p SKIP 1 RETURN COUNT(*) AS n",
            "RETURN COLLECT(p) AS xs",
        ] {
            assert!(
                SpillAggregatePlan::compile(&prepare_query(edge, tail)).is_err(),
                "{tail}"
            );
        }
    }
}


type OutputError = GqlQueryError<GraphAggregateError<&'static str>, usize>;

#[derive(Default)]
struct OutputElements {
    reads: Vec<(fgdb_delta_types::ElementId, PropertyKeyId)>,
    failure: Option<OutputError>,
}
impl crate::GraphElementReader<OutputError> for OutputElements {
    fn element_property(
        &mut self,
        element: fgdb_delta_types::ElementId,
        key: PropertyKeyId,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, OutputError> {
        self.reads.push((element, key));
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        Ok(Some(CanonicalScalar::Int(7).into()))
    }
}

fn output_property() -> crate::GraphSetValue {
    crate::GraphSetValue::MapGet {
        map: Box::new(crate::GraphSetValue::Column(0)),
        key: "p".into(),
        property: crate::GraphElementKey::Property(P),
    }
}

fn output_query(value: crate::GraphSetValue, ranked: bool) -> PreparedGraphAggregate {
    use crate::{GraphSetProjection as Projection, GraphSetValue as Value};
    let query = prepare(
        "MATCH (n) RETURN n AS v,COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a GROUP BY n",
    )
    .with_output_projection(vec![
        Projection::new("p", value),
        Projection::new("c", Value::Column(1)),
        Projection::new("s", Value::Column(2)),
        Projection::new("a", Value::Column(3)),
    ])
    .unwrap();
    if !ranked {
        return query;
    }
    query
        .with_computed_order_by(
            // Visible aliases follow the four complete evaluation cells.
            vec![
                output_property(),
                Value::Column(5),
                Value::Column(6),
                Value::Column(7),
            ],
            &[crate::algebra::GraphValueOrder {
                column: 0,
                descending: true,
                nulls_first: false,
            }],
        )
        .unwrap()
}

fn complete_output_group() -> GraphAggregateRow {
    GraphAggregateRow::from_group_values(
        vec![GraphValue::Vertex(VId(u128::MAX))],
        vec![
            GraphAggregateValue::Count(u64::MAX),
            GraphAggregateValue::Integer(i128::MAX - 7),
            GraphAggregateValue::Average(
                crate::GraphExactAverage::new(i128::MAX - 12, 3).unwrap(),
            ),
        ],
    )
}

#[test]
fn explicit_final_reader_keeps_exact_cells_and_reader_free_rank_admission() {
    let query = output_query(output_property(), true);
    assert!(matches!(
        SpillAggregatePlan::compile(&query),
        Err(SpillAggregateBuildError::Unsupported)
    ));
    assert!(matches!(
        AsyncSpillAggregatePlan::compile(&query),
        Err(SpillAggregateBuildError::Unsupported)
    ));
    let plan = SpillAggregatePlan::compile_with_element_reads(&query).unwrap();
    let definition = plan.definition();
    assert!(definition.output_may_read_elements());
    assert_eq!(definition.evaluation_key_columns().len(), 1);
    assert_eq!(definition.evaluation_aggregate_columns().len(), 3);
    let row = complete_output_group();
    let mut expected = vec![GraphAggregateValue::Value(int(7))];
    expected.extend_from_slice(row.values());
    let mut reader = OutputElements::default();
    let (projected, rank) = definition
        .project_output_with_rank_with_elements(row, &mut reader, &mut |_| {
            Ok::<_, OutputError>(())
        })
        .unwrap();
    assert_eq!(projected, GraphAggregateRow::from_group_values(vec![], expected.clone()));
    assert_eq!(rank.unwrap(), GraphAggregateRow::from_group_values(vec![], expected));
    assert_eq!(
        reader.reads,
        vec![(fgdb_delta_types::ElementId::Vertex(VId(u128::MAX)), P); 2]
    );
}

#[test]
fn final_reader_relay_restores_source_and_control_errors_and_stops_at_every_event() {
    let plan =
        SpillAggregatePlan::compile_with_element_reads(&output_query(output_property(), true))
            .unwrap();
    let definition = plan.definition();
    for source_failure in [false, true] {
        let mut reader = OutputElements {
            failure: Some(if source_failure {
                GqlQueryError::Source(GraphAggregateError::Source("retained source unavailable"))
            } else {
                GqlQueryError::Interrupted(91)
            }),
            ..OutputElements::default()
        };
        let result = definition.project_output_with_rank_with_elements(
            complete_output_group(),
            &mut reader,
            &mut |_| Ok::<_, OutputError>(()),
        );
        if source_failure {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphAggregateError::Source(
                    "retained source unavailable"
                )))
            ));
        } else {
            assert!(matches!(result, Err(GqlQueryError::Interrupted(91))));
        }
        assert_eq!(reader.reads.len(), 1);
    }

    let mut total = 0;
    let mut reader = OutputElements::default();
    definition
        .project_output_with_rank_with_elements(
            complete_output_group(),
            &mut reader,
            &mut |_| {
                total += 1;
                Ok::<_, OutputError>(())
            },
        )
        .unwrap();
    assert!(total > 0);
    assert_eq!(reader.reads.len(), 2);
    for stop in 1..=total {
        let mut seen = 0;
        let mut reader = OutputElements::default();
        let result = definition.project_output_with_rank_with_elements(
            complete_output_group(),
            &mut reader,
            &mut |_| {
                seen += 1;
                if seen == stop {
                    Err(GqlQueryError::Interrupted(stop))
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
        assert!(reader.reads.len() <= 2);
    }
}

#[test]
fn final_reader_preserves_lazy_branches_and_does_not_tighten_default_admission() {
    use crate::GraphSetValue as Value;
    let lazy = Value::Case {
        arms: vec![(Value::Value(GraphValue::Scalar(CanonicalScalar::Bool(false))), output_property())],
        otherwise: Some(Box::new(Value::Column(1))),
    };
    let query = output_query(lazy, false);
    let reader_free = SpillAggregatePlan::compile(&query).unwrap();
    let expected = reader_free
        .definition()
        .project_output_with_rank(complete_output_group(), &mut |_| {
            Ok::<_, OutputError>(())
        })
        .unwrap();
    let plan = SpillAggregatePlan::compile_with_element_reads(&query).unwrap();
    assert!(plan.definition().output_may_read_elements());
    let mut reader = OutputElements {
        failure: Some(GqlQueryError::Interrupted(91)),
        ..OutputElements::default()
    };
    let actual = plan
        .definition()
        .project_output_with_rank_with_elements(
            complete_output_group(),
            &mut reader,
            &mut |_| Ok::<_, OutputError>(()),
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.0.values()[0], GraphAggregateValue::Count(u64::MAX));
    assert!(reader.reads.is_empty());
    assert!(reader.failure.is_some());

    let plan = SpillAggregatePlan::compile_with_element_reads(
        &output_query(output_property(), false),
    )
    .unwrap();
    let result = plan.definition().project_output_with_rank_with_elements(
        complete_output_group(),
        &mut reader,
        &mut |_| Ok::<_, OutputError>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Interrupted(91))));
    assert_eq!(reader.reads.len(), 1);
}

#[test]
fn final_record_reads_consume_the_original_cursor_allowance_and_fuse_on_refusal() {
    for edge in [false, true] {
        let query = prepare_query(edge, "RETURN COUNT(*) AS n");
        let source = Source::new(&[Some(1), Some(2), Some(3)]);
        let drops = source.drops.clone();
        with_input!(
            &query,
            source,
            GqlQueryPolicy::new(4, u64::MAX, u64::MAX, u64::MAX),
            || Ok::<_, usize>(()),
            |cursor| {
                while cursor.next_input().unwrap().is_some() {}
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(cursor.row_stats().snapshot_records, 3);
                cursor.charge(VertexScanEvent::Work).unwrap();
                cursor.admit_output_record().unwrap();
                assert_eq!(cursor.row_stats().snapshot_records, 4);
                cursor.charge(VertexScanEvent::Work).unwrap();
                assert!(matches!(
                    cursor.admit_output_record(),
                    Err(GqlQueryError::Rows(_))
                ));
                assert_eq!(cursor.row_stats().snapshot_records, 4);
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert!(cursor.next_input().unwrap().is_none());
                assert!(cursor.finish_result().is_err());
            }
        );
        let source = Source::new(&[Some(1)]);
        let drops = source.drops.clone();
        with_input!(&query, source, wide(), || Ok::<_, usize>(()), |cursor| {
            assert!(matches!(
                cursor.admit_output_record(),
                Err(GqlQueryError::Source(
                    GraphAggregateError::InvalidReductionInput
                ))
            ));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert!(cursor.next_input().unwrap().is_none());
        });
    }
}
