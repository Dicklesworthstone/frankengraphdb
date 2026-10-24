use super::*;
use crate::algebra::{GlaDirection, GraphValue};
use crate::edge_stream::aggregate::{EdgeAggregateCursor, EdgeAggregatePlan};
use crate::edge_stream::{
    EdgeExpansionSourceError, EdgeScanRow, EdgeScanSource, EdgeScanSourceError, EdgeScanState,
    VertexScanRow,
};
use crate::stream::aggregate::{VertexAggregateCursor, VertexAggregatePlan};
use crate::stream::{
    VertexScanEvent, VertexScanRecord, VertexScanSource, VertexScanSourceError, VertexScanState,
};
use crate::{
    GqlParameters, GqlQueryPolicy, GraphAggregateRow, GraphAggregateValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphPipelineAggregateText,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq, EId, VId};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const TEXT: &str = "MATCH (a)-[r:R]->(b) WITH b AS owner,r.p AS p WHERE p>=0 \
    WITH p*2 AS value,owner AS owner RETURN owner,COUNT(*) AS n,SUM(value) AS total,\
    COUNT(DISTINCT value) AS support GROUP BY owner";

struct Source {
    props: Vec<Vec<(PropertyKeyId, CanonicalPropertyValue)>>,
    next: usize,
    fault: Option<EId>,
    drops: Arc<AtomicUsize>,
}
impl Source {
    fn new(values: &[Option<i64>]) -> Self {
        Self {
            props: values
                .iter()
                .map(|v| {
                    v.map(|v| vec![(P, CanonicalScalar::Int(v).into())])
                        .unwrap_or_default()
                })
                .collect(),
            next: 0,
            fault: None,
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn owner(at: usize) -> VId {
        VId((at % 3 + 1) as u128)
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(9)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.next == self.props.len() {
            return Ok(None);
        }
        let id = EId(self.next as u128);
        self.next += 1;
        Ok(Some(id))
    }
    fn edge<C>(
        &self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        if self.fault == Some(eid) {
            return Err(EdgeScanSourceError::Source("edge unavailable"));
        }
        Ok(usize::try_from(eid.0).ok().and_then(|at| {
            self.props.get(at).map(|properties| EdgeScanRow {
                source: VId(0),
                target: Self::owner(at),
                relation: R,
                properties,
            })
        }))
    }
    fn vertex<C>(
        &self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok((vid.0 <= 3).then_some(VertexScanRow {
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
        for at in 0..self.props.len() {
            control(GlaExecutionEvent::Work)
                .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
            let id = EId(at as u128);
            if after.is_some_and(|previous| id <= previous) {
                continue;
            }
            if (direction != GlaDirection::Reverse && endpoint == VId(0))
                || (direction != GlaDirection::Forward && endpoint == Self::owner(at))
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
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn cursor<C>(
    q: &PreparedGraphAggregate,
    source: Source,
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> EdgeAggregateCursor<Source, impl FnMut() -> Result<(), C>> {
    EdgeAggregateCursor::new(
        source,
        EdgeAggregatePlan::compile(q).unwrap(),
        policy,
        checkpoint,
    )
}
fn eager(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        s.props.len() as u64 + 4,
        (0..4).map(VId),
        s.props
            .iter()
            .enumerate()
            .map(|(at, _)| (EId(at as u128), VId(0), R, Source::owner(at))),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        |eid, key| {
            Ok(s.props[eid.0 as usize]
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v))
        },
        wide(),
        || Ok::<_, ()>(()),
    )
    .unwrap()
    .value
}

#[test]
fn composed_aliases_filters_and_argument_distinct_match_independent_occurrences_and_batch() {
    let q = prepare(TEXT);
    for mut code in 0..256 {
        let values: Vec<_> = (0..4)
            .map(|_| {
                let at = code % 4;
                code /= 4;
                [None, Some(-2), Some(0), Some(3)][at]
            })
            .collect();
        let source = Source::new(&values);
        let expected = eager(&q, &source);
        let actual = cursor(&q, source, wide(), || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, expected);
        // Independent direct occurrence oracle; neither the GLA matcher nor
        // the row predicate/projection implementation computes these totals.
        let mut groups = BTreeMap::<VId, Vec<i64>>::new();
        for (at, value) in values.iter().enumerate() {
            if let Some(value) = value.filter(|value| *value >= 0) {
                groups.entry(Source::owner(at)).or_default().push(value * 2);
            }
        }
        assert_eq!(actual.len(), groups.len());
        for (row, (owner, values)) in actual.iter().zip(groups) {
            assert_eq!(row.keys(), &[GraphValue::Vertex(owner)]);
            assert_eq!(row.values()[0].as_count(), Some(values.len() as u64));
            assert_eq!(
                row.values()[1].as_integer(),
                Some(values.iter().map(|v| i128::from(*v)).sum())
            );
            assert_eq!(
                row.values()[2].as_count(),
                Some(
                    values
                        .into_iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len() as u64
                )
            );
        }
    }
}

#[test]
fn filter_is_not_eof_and_does_not_execute_downstream_projection_on_rejected_rows() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE p>0 WITH 12/p AS v RETURN SUM(v) AS total,COUNT(*) AS n",
    );
    let source = Source::new(&[Some(0), None, Some(-2), Some(3), Some(4)]);
    let expected = eager(&q, &source);
    let mut stream = cursor(&q, source, wide(), || Ok::<_, ()>(()));
    let row = stream.next().unwrap().unwrap();
    assert_eq!(
        row.values(),
        &[
            GraphAggregateValue::Integer(7),
            GraphAggregateValue::Count(2)
        ]
    );
    assert_eq!(vec![row], expected);
    let mut none = cursor(
        &q,
        Source::new(&[Some(0), None]),
        wide(),
        || Ok::<_, ()>(()),
    );
    let row = none.next().unwrap().unwrap();
    assert!(row.values()[0].is_null());
    assert_eq!(row.values()[1].as_count(), Some(0));
}

#[test]
fn hidden_expression_and_late_source_errors_cannot_be_erased_by_filters_or_limit_zero() {
    for text in [
        "MATCH (a)-[r:R]->(b) WITH 1/r.p AS unused WHERE FALSE RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE p>=0 WITH 1/p AS unused RETURN COUNT(*) AS n LIMIT 0",
    ] {
        let q = prepare(text);
        let mut stream = cursor(&q, Source::new(&[Some(1), Some(0)]), wide(), || {
            Ok::<_, ()>(())
        });
        assert!(matches!(
            stream.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::InputRelation(GraphSetExecutionError::Projection {
                    row: 0,
                    ..
                })
            )))
        ));
        assert_eq!(stream.row_stats().result_rows, 0);
        assert_eq!(stream.state(), EdgeScanState::Failed);
        assert!(stream.next().is_none());
    }
    let q = prepare("MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE FALSE RETURN COUNT(*) AS n LIMIT 0");
    let mut source = Source::new(&[Some(1), Some(2)]);
    source.fault = Some(EId(1));
    let mut stream = cursor(&q, source, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        stream.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(_))))
    ));
    assert_eq!(stream.row_stats().result_rows, 0);
}

#[test]
fn admission_keeps_every_intermediate_barrier_and_collection_order_contract() {
    for text in [
        "MATCH (a)-[r:R]->(b) WITH DISTINCT r.p AS p RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p LIMIT 0 RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p SKIP 1 RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p ORDER BY p RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p RETURN COLLECT(p) AS xs",
    ] {
        assert!(
            EdgeAggregatePlan::compile(&prepare(text)).is_err(),
            "barrier lost: {text}"
        );
    }
    let q = prepare(
        "MATCH (a)-[r:R]->(b) WITH r.p AS p RETURN SUM(p) AS total HAVING total>0 ORDER BY total LIMIT 1",
    );
    assert!(
        EdgeAggregatePlan::compile(&q).is_ok(),
        "result clauses are not input barriers"
    );
    let other = prepare("MATCH (a)-[r:R]->(b) WITH b AS owner RETURN COUNT(*) AS n");
    assert!(InputPipeline::compile(q.input_relation().unwrap(), other.input_pattern()).is_none());
    let random = PreparedGraphSet::from(q.input_pattern().clone())
        .project(
            vec![GraphSetProjection::new(
                "draw",
                GraphSetValue::Random { site: 0, scope: 0 },
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert!(InputPipeline::compile(&random, q.input_pattern()).is_none());
}

#[test]
fn every_pipeline_query_checkpoint_is_terminal_and_exact_limits_do_not_reset() {
    let q = prepare(TEXT);
    let values = [None, Some(-1), Some(3), Some(2), Some(3)];
    let mut calls = 0;
    let mut full = cursor(&q, Source::new(&values), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = full.row_stats();
    let stats = full.evaluator_stats();
    drop(full);
    for stop in 1..=calls {
        let mut at = 0;
        let source = Source::new(&values);
        let drops = source.drops.clone();
        let mut stream = cursor(&q, source, wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match stream.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(value))) => {
                    assert_eq!(value, stop);
                    break;
                }
                other => panic!("missed cancellation {stop}: {other:?}"),
            }
        }
        assert_eq!(delivered, expected[..delivered.len()]);
        assert_eq!(stream.state(), EdgeScanState::Failed);
        assert!(stream.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(stream);
        assert_eq!(at, stop);
    }
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        rows.result_rows,
        stats.work_units,
        stats.scratch_entries,
    );
    assert_eq!(
        cursor(&q, Source::new(&values), exact, || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    for policy in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
    ] {
        assert!(
            cursor(&q, Source::new(&values), policy, || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>()
                .is_err()
        );
    }
}

#[test]
fn filtered_matches_still_yield_cooperatively_and_close_never_drains() {
    struct Wake(AtomicUsize);
    impl std::task::Wake for Wake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let q = prepare("MATCH (a)-[r:R]->(b) WITH r.p AS p WHERE p<0 RETURN COUNT(*) AS n");
    let source = Source::new(&vec![Some(1); 512]);
    let drops = source.drops.clone();
    let mut stream = cursor(
        &q,
        source,
        GqlQueryPolicy::new(512, 1, 10_000_000, 10_000_000),
        || Ok::<_, ()>(()),
    );
    let wake = Arc::new(Wake(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wake.clone());
    let mut context = std::task::Context::from_waker(&waker);
    assert!(stream.poll_next(&mut context).is_pending());
    assert_eq!(stream.row_stats().result_rows, 0);
    assert!(stream.row_stats().snapshot_records < 512);
    let row = loop {
        if let std::task::Poll::Ready(row) = stream.poll_next(&mut context) {
            break row.unwrap().unwrap();
        }
    };
    assert_eq!(row.values()[0].as_count(), Some(0));
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let mut closed = cursor(&q, Source::new(&[Some(1)]), wide(), || -> Result<(), ()> {
        panic!("closed source driven");
    });
    closed.close();
    assert!(closed.next().is_none());
    assert_eq!(closed.row_stats().snapshot_records, 0);
}

// Use the same occurrence fixture with the vertex access contract. The raw
// route deliberately refuses; every projection/filter must see the admitted
// record, including its redacted fields, not call vertex() behind its back.
impl VertexScanSource for Source {
    type Error = &'static str;

    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(9)
    }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.next == self.props.len() {
            return Ok(None);
        }
        let id = VId(self.next as u128);
        self.next += 1;
        Ok(Some(id))
    }

    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        panic!("WITH bypassed the admitted vertex record");
    }

    fn vertex_record<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fault == Some(EId(id.0)) {
            return Err(VertexScanSourceError::Source("vertex unavailable"));
        }
        let Some(properties) = usize::try_from(id.0).ok().and_then(|at| self.props.get(at)) else {
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

fn vertex_cursor<C>(
    query: &PreparedGraphAggregate,
    source: Source,
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> VertexAggregateCursor<Source, impl FnMut() -> Result<(), C>> {
    VertexAggregateCursor::new(
        source,
        VertexAggregatePlan::compile(query).unwrap(),
        policy,
        checkpoint,
    )
}

fn eager_vertex(query: &PreparedGraphAggregate, source: &Source) -> Vec<GraphAggregateRow> {
    query
        .execute_governed_with_element_properties(
            source.props.len() as u64,
            (0..source.props.len()).map(|at| VId(at as u128)),
            std::iter::empty::<(EId, VId, RelationId, VId)>(),
            |_, _| Ok::<_, ()>(true),
            |id, key| {
                Ok(source.props[id.0 as usize]
                    .iter()
                    .find(|(candidate, _)| key == P && *candidate == key)
                    .map(|(_, value)| value))
            },
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

const VERTEX_TEXT: &str = "MATCH (n) WITH n.p AS p,n.hidden AS secret \
    WHERE secret IS NULL AND p>=0 WITH p/2 AS bucket,p*2 AS value \
    RETURN bucket,COUNT(*) AS n,SUM(value) AS total,COUNT(DISTINCT value) AS support GROUP BY bucket";

#[test]
fn vertex_with_masked_records_match_batch_and_independent_grouped_occurrences() {
    let query = prepare(VERTEX_TEXT);
    for encoded in 0..625 {
        let mut code = encoded;
        let values: [Option<i64>; 4] = std::array::from_fn(|_| {
            let value = [None, Some(-2), Some(0), Some(2), Some(3)][code % 5];
            code /= 5;
            value
        });
        let mut source = Source::new(&values);
        for properties in &mut source.props {
            properties.push((PropertyKeyId(2), CanonicalScalar::Bool(true).into()));
        }
        let expected = eager_vertex(&query, &source);
        let drops = source.drops.clone();
        let mut stream = vertex_cursor(&query, source, wide(), || Ok::<_, ()>(()));
        let actual = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected, "fixture {encoded}");
        assert_eq!(stream.row_stats().snapshot_records, 4);
        assert_eq!(stream.row_stats().result_rows, actual.len() as u64);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut groups = BTreeMap::<i64, Vec<i64>>::new();
        for value in values.into_iter().flatten().filter(|value| *value >= 0) {
            groups.entry(value / 2).or_default().push(value * 2);
        }
        assert_eq!(actual.len(), groups.len());
        for (row, (key, values)) in actual.iter().zip(groups) {
            assert_eq!(row.keys(), &[GraphValue::Scalar(CanonicalScalar::Int(key))]);
            assert_eq!(row.values()[0].as_count(), Some(values.len() as u64));
            assert_eq!(
                row.values()[1].as_integer(),
                Some(values.iter().map(|v| i128::from(*v)).sum())
            );
            let support = values
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(row.values()[2].as_count(), Some(support.len() as u64));
        }
    }
}

#[test]
fn vertex_relation_widening_and_plain_group_finalization_use_the_final_schema() {
    use crate::GraphAggregate;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(&[GraphColumn::property("p", "n", P)], 0, None)
        .unwrap()
        .with_duplicates();
    let relation = PreparedGraphSet::from(input)
        .project(
            vec![
                GraphSetProjection::new(
                    "extra",
                    GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(77))),
                ),
                GraphSetProjection::new("key", GraphSetValue::Column(0)),
                GraphSetProjection::new("value", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = PreparedGraphAggregate::prepare_relation(
        relation,
        &[1],
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", 2),
        ],
        0,
        None,
    )
    .unwrap();
    // No result clauses hide the plain finalizer, whose maintained-row
    // constructor deliberately does not accept relational definitions.
    assert!(!query.has_streamed_output_stage());
    assert!(query.input_projection().is_none());
    let source = Source::new(&[Some(2), Some(-1), Some(2), None]);
    let expected = eager_vertex(&query, &source);
    let actual = vertex_cursor(&query, source, wide(), || Ok::<_, ()>(()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 3);
    for row in actual {
        match row.keys()[0] {
            GraphValue::Scalar(CanonicalScalar::Null) => {
                assert_eq!(row.values()[0].as_count(), Some(1));
                assert!(row.values()[1].is_null());
            }
            GraphValue::Scalar(CanonicalScalar::Int(-1)) => {
                assert_eq!(row.values()[0].as_count(), Some(1));
                assert_eq!(row.values()[1].as_integer(), Some(-1));
            }
            GraphValue::Scalar(CanonicalScalar::Int(2)) => {
                assert_eq!(row.values()[0].as_count(), Some(2));
                assert_eq!(row.values()[1].as_integer(), Some(4));
            }
            _ => panic!("group used a raw source column instead of the WITH schema"),
        }
    }
}

#[test]
fn vertex_with_filters_errors_empty_groups_and_input_barriers_keep_their_contracts() {
    let query = prepare(
        "MATCH (n) WITH n.p AS p WHERE p>0 WITH 12/p AS v RETURN SUM(v) AS total,COUNT(*) AS n",
    );
    let mut stream = vertex_cursor(
        &query,
        Source::new(&[Some(0), None, Some(-2), Some(3), Some(4)]),
        wide(),
        || Ok::<_, ()>(()),
    );
    let row = stream.next().unwrap().unwrap();
    assert_eq!(
        row.values(),
        &[
            GraphAggregateValue::Integer(7),
            GraphAggregateValue::Count(2)
        ]
    );
    assert!(stream.next().is_none());
    let mut empty = vertex_cursor(&query, Source::new(&[Some(0), None]), wide(), || {
        Ok::<_, ()>(())
    });
    let row = empty.next().unwrap().unwrap();
    assert!(row.values()[0].is_null());
    assert_eq!(row.values()[1].as_count(), Some(0));
    assert!(
        vertex_cursor(&prepare(VERTEX_TEXT), Source::new(&[]), wide(), || Ok::<
            _,
            (),
        >(
            ()
        ))
        .next()
        .is_none()
    );
    for text in [
        "MATCH (n) WITH 1/n.p AS unused WHERE FALSE RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS p WHERE p>=0 WITH 1/p AS unused RETURN COUNT(*) AS n LIMIT 0",
    ] {
        let query = prepare(text);
        let mut stream = vertex_cursor(&query, Source::new(&[Some(1), Some(0)]), wide(), || {
            Ok::<_, ()>(())
        });
        assert!(matches!(
            stream.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::InputRelation(GraphSetExecutionError::Projection {
                    row: 0,
                    ..
                })
            )))
        ));
        assert_eq!(stream.row_stats().result_rows, 0);
        assert_eq!(stream.state(), VertexScanState::Failed);
        assert!(stream.next().is_none());
    }
    let query = prepare("MATCH (n) WITH n.p AS p WHERE FALSE RETURN COUNT(*) AS n LIMIT 0");
    let mut source = Source::new(&[Some(1), Some(2)]);
    source.fault = Some(EId(1));
    let drops = source.drops.clone();
    let mut stream = vertex_cursor(&query, source, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        stream.next(),
        Some(Err(GqlQueryError::Source(GraphAggregateError::Source(_))))
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(stream.row_stats().result_rows, 0);
    assert!(stream.next().is_none());
    for text in [
        "MATCH (n) WITH DISTINCT n.p AS p RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS p LIMIT 0 RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS p SKIP 1 RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS p ORDER BY p RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS p RETURN COLLECT(p) AS xs",
    ] {
        assert!(
            VertexAggregatePlan::compile(&prepare(text)).is_err(),
            "barrier lost: {text}"
        );
    }
}

#[test]
fn vertex_with_checkpoint_cuts_and_exact_budgets_remain_cumulative() {
    let query = prepare(VERTEX_TEXT);
    let values = [None, Some(-1), Some(0), Some(2), Some(3)];
    let mut calls = 0;
    let mut full = vertex_cursor(&query, Source::new(&values), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let rows = full.row_stats();
    let stats = full.evaluator_stats();
    drop(full);
    for stop in 1..=calls {
        let source = Source::new(&values);
        let drops = source.drops.clone();
        let mut at = 0;
        let mut stream = vertex_cursor(&query, source, wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        let mut delivered = Vec::new();
        loop {
            match stream.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Interrupted(value))) => {
                    assert_eq!(value, stop);
                    break;
                }
                other => panic!("missed vertex cancellation {stop}: {other:?}"),
            }
        }
        assert!(expected.starts_with(&delivered));
        assert_eq!(stream.state(), VertexScanState::Failed);
        assert!(stream.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(stream);
        assert_eq!(at, stop);
    }
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        rows.result_rows,
        stats.work_units,
        stats.scratch_entries,
    );
    assert_eq!(
        vertex_cursor(&query, Source::new(&values), exact, || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    for policy in [
        GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
    ] {
        let mut stream = vertex_cursor(&query, Source::new(&values), policy, || Ok::<_, ()>(()));
        assert!(stream.by_ref().collect::<Result<Vec<_>, _>>().is_err());
        assert_eq!(stream.state(), VertexScanState::Failed);
        assert!(stream.next().is_none());
    }
}

#[test]
fn vertex_with_all_filtered_inputs_yield_and_resume_without_a_new_budget() {
    struct Wake(AtomicUsize);
    impl std::task::Wake for Wake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let query = prepare("MATCH (n) WITH n.p AS p WHERE p<0 RETURN COUNT(*) AS n");
    let values = vec![Some(1); 512];
    let mut sync = vertex_cursor(&query, Source::new(&values), wide(), || Ok::<_, ()>(()));
    let expected = sync.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
    let source = Source::new(&values);
    let drops = source.drops.clone();
    let mut stream = vertex_cursor(&query, source, wide(), || Ok::<_, ()>(()));
    let wake = Arc::new(Wake(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wake.clone());
    let mut context = std::task::Context::from_waker(&waker);
    assert!(stream.poll_next(&mut context).is_pending());
    assert_eq!(stream.row_stats().result_rows, 0);
    assert!(stream.row_stats().snapshot_records < 512);
    let row = loop {
        if let std::task::Poll::Ready(row) = stream.poll_next(&mut context) {
            break row.unwrap().unwrap();
        }
    };
    assert_eq!(row.values()[0].as_count(), Some(0));
    assert_eq!(vec![row], expected);
    assert_eq!(stream.row_stats(), sync.row_stats());
    assert_eq!(stream.evaluator_stats(), sync.evaluator_stats());
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(stream.next().is_none());
    let source = Source::new(&[Some(1)]);
    let drops = source.drops.clone();
    let mut closed = vertex_cursor(&query, source, wide(), || -> Result<(), ()> {
        panic!("closed vertex pipeline was driven");
    });
    closed.close();
    assert!(closed.next().is_none());
    assert_eq!(closed.row_stats().snapshot_records, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
