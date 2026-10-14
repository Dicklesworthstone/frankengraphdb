//! Order-sensitive lists must agree with the canonical identified child, not
//! merely with a multiset or a commutative numeric summary.
use super::*;
use crate::{
    GqlParameters, GraphAggregate, GraphAggregateValue, GraphSetProjection, GraphSetValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use std::ops::Bound::{Excluded, Unbounded};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, VId, Vec<(PropertyKeyId, CanonicalScalar)>);
#[derive(Clone)]
struct Source {
    edges: BTreeMap<EId, Edge>,
    after: Option<EId>,
    fail: Option<EId>,
    drops: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl EdgeScanSource for Source {
    type Error = &'static str;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(23)
    }
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let next = self
            .edges
            .range((self.after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id);
        if let Some(id) = next {
            self.after = Some(id);
        }
        Ok(next)
    }
    fn edge<C>(
        &self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail == Some(id) {
            return Err(EdgeScanSourceError::Source("late edge failure"));
        }
        Ok(self
            .edges
            .get(&id)
            .map(|(source, target, properties)| EdgeScanRow {
                source: *source,
                target: *target,
                relation: R,
                properties,
            }))
    }
    fn vertex<C>(
        &self,
        _: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'_>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(Some(VertexScanRow {
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
        for (&id, (a, b, _)) in self
            .edges
            .range((after.map_or(Unbounded, Excluded), Unbounded))
        {
            control(GlaExecutionEvent::Work)
                .map_err(|e| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(e)))?;
            if match direction {
                GlaDirection::Forward => *a == endpoint,
                GlaDirection::Reverse => *b == endpoint,
                GlaDirection::Undirected => *a == endpoint || *b == endpoint,
            } {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}
fn source(mask: usize) -> Source {
    let ids = [VId(0), VId(1), VId(u128::MAX)];
    let raw = [
        (2, 0, Some(9)),
        (0, 1, Some(-4)),
        (2, 0, Some(9)),
        (1, 1, None),
        (1, 2, Some(7)),
    ];
    Source {
        edges: raw
            .into_iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(at, (a, b, p))| {
                (
                    EId(at as u128 + 1),
                    (
                        ids[a],
                        ids[b],
                        p.into_iter()
                            .map(|p| (P, CanonicalScalar::Int(p)))
                            .collect(),
                    ),
                )
            })
            .collect(),
        after: None,
        fail: None,
        drops: Arc::new(AtomicUsize::new(0)),
        reads: Arc::new(AtomicUsize::new(0)),
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, _: &str| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(R)),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn definition(
    dir: usize,
    hops: usize,
    grouped: bool,
    count: Option<u64>,
) -> PreparedGraphAggregate {
    let atom = |name, end| match dir {
        0 => format!("-[{name}:R]->({end})"),
        1 => format!("<-[{name}:R]-({end})"),
        _ => format!("-[{name}:R]-({end})"),
    };
    let mut text = format!("MATCH (a){}", atom("r", "b"));
    if hops == 2 {
        text += &atom("s", "c");
    }
    text += " RETURN COLLECT(r) AS roots,COLLECT(a) AS starts,";
    text += if hops == 2 {
        "COLLECT(s) AS suffixes,COLLECT(c) AS ends,"
    } else {
        "COLLECT(b) AS ends,"
    };
    text += "COLLECT(r.p) AS amounts,COLLECT(DISTINCT r.p) AS support";
    let q = prepare(&text);
    let cols: Vec<_> = q
        .aggregates()
        .iter()
        .map(|s| s.argument_column().unwrap())
        .collect();
    let specs: Vec<_> = q
        .aggregate_columns()
        .iter()
        .enumerate()
        .map(|(at, name)| {
            let name = if grouped && name == "ends" {
                "collected_ends"
            } else {
                name.as_str()
            };
            if at + 1 == cols.len() {
                GraphAggregate::collect_distinct(name, cols[at])
            } else {
                GraphAggregate::collect(name, cols[at])
            }
        })
        .collect();
    PreparedGraphAggregate::prepare(
        q.input_pattern().clone(),
        if grouped {
            &cols[hops + 1..hops + 2]
        } else {
            &[]
        },
        &specs,
        0,
        count,
    )
    .unwrap()
}
fn batch(q: &PreparedGraphAggregate, s: &Source) -> Vec<GraphAggregateRow> {
    q.execute_governed_with_element_properties(
        s.edges.len() as u64,
        [VId(0), VId(1), VId(u128::MAX)],
        s.edges.iter().map(|(&id, (a, b, _))| (id, *a, R, *b)),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        |id, key| {
            Ok(s.edges[&id]
                .2
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
fn list(values: Vec<GraphValue>) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(values.into_boxed_slice()))
}
// Independent edge-choice enumeration, sorted by root EId/orientation and each
// suffix EId. Keys only partition complete occurrences; no GLA helper is used.
fn oracle(
    s: &Source,
    dir: usize,
    hops: usize,
    grouped: bool,
    count: Option<u64>,
) -> Vec<GraphAggregateRow> {
    let mut oriented = Vec::new();
    for (&id, (a, b, p)) in &s.edges {
        let value = p.first().map(|(_, p)| GraphValue::Scalar(p.clone()));
        let (a, b) = if dir == 1 { (*b, *a) } else { (*a, *b) };
        oriented.push((id, a, b, value.clone()));
        if dir == 2 && a != b {
            oriented.push((id, b, a, value));
        }
    }
    oriented.sort_by_key(|(id, a, _, _)| (*id, *a));
    let mut groups: BTreeMap<Vec<GraphValue>, Vec<Vec<GraphValue>>> = BTreeMap::new();
    if !grouped {
        groups.insert(vec![], vec![vec![]; hops + 4]);
    }
    for (root, start, end, value) in &oriented {
        let suffixes: Vec<_> = if hops == 1 {
            vec![(None, *end)]
        } else {
            // DIFFERENT EDGES, the textual MATCH default: the two R hops
            // never bind one edge (fgdb-vexfr).
            oriented
                .iter()
                .filter(|(id, a, _, _)| a == end && id != root)
                .map(|(id, _, b, _)| (Some(*id), *b))
                .collect()
        };
        for (suffix, end) in suffixes {
            let key = if grouped {
                vec![GraphValue::Vertex(end)]
            } else {
                vec![]
            };
            let lists = groups.entry(key).or_insert_with(|| vec![vec![]; hops + 4]);
            lists[0].push(GraphValue::Edge(*root));
            lists[1].push(GraphValue::Vertex(*start));
            if let Some(id) = suffix {
                lists[2].push(GraphValue::Edge(id));
            }
            lists[hops + 1].push(GraphValue::Vertex(end));
            if let Some(value) = value {
                lists[hops + 2].push(value.clone());
                if !lists[hops + 3].contains(value) {
                    lists[hops + 3].push(value.clone());
                }
            }
        }
    }
    groups
        .into_iter()
        .take(count.map_or(usize::MAX, |n| n as usize))
        .map(|(keys, values)| {
            GraphAggregateRow::from_group_values(keys, values.into_iter().map(list).collect())
        })
        .collect()
}

#[test]
fn collected_edge_choices_and_orientations_match_independent_lists_and_batch_order() {
    for mask in 0..32 {
        for dir in 0..3 {
            for hops in 1..=2 {
                for grouped in [false, true] {
                    for count in [None, Some(0), Some(1)] {
                        let q = definition(dir, hops, grouped, count);
                        let s = source(mask);
                        let before = q.canonical_bytes();
                        let expected = oracle(&s, dir, hops, grouped, count);
                        assert_eq!(batch(&q, &s), expected);
                        let drops = s.drops.clone();
                        let mut c = EdgeAggregateCursor::new(
                            s,
                            EdgeAggregatePlan::compile(&q).unwrap(),
                            wide(),
                            || Ok::<_, ()>(()),
                        );
                        let actual = c.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(actual, expected);
                        assert_eq!(c.row_stats().result_rows, actual.len() as u64);
                        assert_eq!(drops.load(Ordering::SeqCst), 1);
                        assert!(c.next().is_none());
                        assert_eq!(q.canonical_bytes(), before);
                    }
                }
            }
        }
    }
}

#[test]
fn computed_collections_keep_original_child_order_and_unproved_shapes_refuse() {
    let raw = definition(2, 2, false, None);
    let at = raw.aggregates()[4].argument_column().unwrap();
    let q = PreparedGraphAggregate::prepare_projected(
        raw.input_pattern().clone(),
        vec![GraphSetProjection::new("amount", GraphSetValue::Column(at))],
        &[],
        &[
            GraphAggregate::collect("all", 0),
            GraphAggregate::collect_distinct("unique", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let s = source(31);
    let expected = batch(&q, &s);
    assert_eq!(
        EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || Ok::<
            _,
            (),
        >(
            ()
        ))
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        expected
    );
    // A directed root's edge identity alone proves its order and uniqueness
    // (67506bf7), so a collection of the edge itself streams and equals the
    // batch answer.
    let edges = prepare("MATCH (a)-[r:R]->(b) RETURN COLLECT(r) AS values");
    let s = source(31);
    let expected = batch(&edges, &s);
    assert_eq!(
        EdgeAggregateCursor::new(
            s,
            EdgeAggregatePlan::compile(&edges).unwrap(),
            wide(),
            || Ok::<_, ()>(())
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        expected
    );
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(a) AS starts,COLLECT(r) AS edges",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(b) AS ends,COLLECT(r.p) AS values",
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS values,COLLECT(r) AS edges",
    ] {
        let q = prepare(text);
        let s = source(31);
        let expected = batch(&q, &s);
        let plan = EdgeAggregatePlan::compile(&q).unwrap();
        assert!(plan.ordered_collections.is_some(), "{text}");
        assert_eq!(
            EdgeAggregateCursor::new(s, plan, wide(), || Ok::<_, ()>(()))
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected,
            "{text}"
        );
    }
    // A plain endpoint collection is proved now (fgdb-dvtfk) and streams the
    // batch answer; a computed collection is still refused by the scan.
    let ends = prepare("MATCH (a)-[r:R]->(b) RETURN COLLECT(b) AS values");
    let s = source(31);
    let expected = batch(&ends, &s);
    assert_eq!(
        EdgeAggregateCursor::new(
            s,
            EdgeAggregatePlan::compile(&ends).unwrap(),
            wide(),
            || Ok::<_, ()>(())
        )
        .collect::<Result<Vec<_>, _>>()
        .unwrap(),
        expected
    );
    assert!(matches!(
        EdgeAggregatePlan::compile(&prepare(
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p + 1) AS values"
        )),
        Err(EdgeAggregateBuildError::Scan(_))
    ));
}

// Independent property-order oracle. The child orders its projected columns;
// within a group, the first varying column is the collected integer property.
// Edge identities intentionally visit [9, -4, 9, NULL, 7], a different order.
fn property_collections(s: &Source, direction: usize, grouped: bool) -> Vec<GraphAggregateRow> {
    let mut groups: BTreeMap<Vec<GraphValue>, Vec<i64>> = BTreeMap::new();
    if !grouped {
        groups.insert(Vec::new(), Vec::new());
    }
    for (a, b, properties) in s.edges.values() {
        let (a, b) = if direction == 1 { (*b, *a) } else { (*a, *b) };
        let ends = if direction == 2 && a != b {
            vec![b, a]
        } else {
            vec![b]
        };
        for end in ends {
            let key = if grouped {
                vec![GraphValue::Vertex(end)]
            } else {
                Vec::new()
            };
            let values = groups.entry(key).or_default();
            if let Some((_, CanonicalScalar::Int(value))) = properties.first() {
                values.push(*value);
            }
        }
    }
    groups
        .into_iter()
        .map(|(keys, mut values)| {
            values.sort();
            let all = integer_list(&values);
            values.dedup();
            GraphAggregateRow::from_group_values(keys, vec![all, integer_list(&values)])
        })
        .collect()
}

fn integer_list(values: &[i64]) -> GraphAggregateValue {
    list(
        values
            .iter()
            .map(|&value| GraphValue::Scalar(CanonicalScalar::Int(value)))
            .collect(),
    )
}

#[test]
fn canonical_property_collections_match_identified_batch_order_and_independent_lists() {
    for mask in 0..32 {
        for (direction, pattern) in [
            (0, "(a)-[r:R]->(b)"),
            (1, "(a)<-[r:R]-(b)"),
            (2, "(a)-[r:R]-(b)"),
            (0, "(a)-[r]->(b)"),
        ] {
            for grouped in [false, true] {
                let key = if grouped { "b AS endpoint," } else { "" };
                let text = format!(
                    "MATCH {pattern} RETURN {key}COLLECT(r.p) AS values, \
                     COLLECT(DISTINCT r.p) AS support"
                );
                let q = prepare(&text);
                let before = q.canonical_bytes();
                let s = source(mask);
                let expected = property_collections(&s, direction, grouped);
                assert_eq!(batch(&q, &s), expected, "eager: {mask}: {text}");
                let drops = s.drops.clone();
                let mut cursor = EdgeAggregateCursor::new(
                    s,
                    EdgeAggregatePlan::compile(&q).unwrap(),
                    wide(),
                    || Ok::<_, ()>(()),
                );
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected,
                    "stream: {mask}: {text}"
                );
                assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert!(cursor.next().is_none());
                assert_eq!(q.canonical_bytes(), before);
            }
        }
    }
    assert_eq!(
        property_collections(&source(31), 0, false)[0].values(),
        &[integer_list(&[-4, 7, 9, 9]), integer_list(&[-4, 7, 9])]
    );
}

#[test]
fn canonical_collection_order_precedes_output_expressions_and_group_windows() {
    for (text, expected) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN \
             [x IN COLLECT(r.p) WHERE x > 0 | x + 1] AS values",
            vec![GraphAggregateRow::from_group_values(
                vec![],
                vec![integer_list(&[8, 10, 10])],
            )],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN b AS endpoint, COLLECT(r.p) AS values \
             ORDER BY endpoint DESC SKIP 1 LIMIT 1",
            vec![GraphAggregateRow::from_group_values(
                vec![GraphValue::Vertex(VId(1))],
                vec![integer_list(&[-4])],
            )],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT COLLECT(r.p) AS values",
            vec![GraphAggregateRow::from_group_values(
                vec![],
                vec![integer_list(&[-4, 7, 9, 9])],
            )],
        ),
    ] {
        let q = prepare(text);
        let s = source(31);
        assert_eq!(batch(&q, &s), expected, "eager: {text}");
        let actual =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
                Ok::<_, ()>(())
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, expected, "stream: {text}");
    }
}

#[test]
fn canonical_collection_sort_shares_limits_and_every_cancellation_is_terminal() {
    let q = prepare(
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS values, \
         COLLECT(DISTINCT r.p) AS support",
    );
    let plan = EdgeAggregatePlan::compile(&q).unwrap();
    let mut calls = 0;
    let mut complete = EdgeAggregateCursor::new(source(31), plan.clone(), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = complete.next().unwrap().unwrap();
    assert_eq!(
        expected.values(),
        &[integer_list(&[-4, 7, 9, 9]), integer_list(&[-4, 7, 9])]
    );
    let rows = complete.row_stats();
    let evaluator = complete.evaluator_stats();
    drop(complete);
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        1,
        evaluator.work_units,
        evaluator.scratch_entries,
    );
    assert_eq!(
        EdgeAggregateCursor::new(source(31), plan.clone(), exact, || Ok::<_, ()>(()))
            .next()
            .unwrap()
            .unwrap(),
        expected
    );
    for policy in [
        GqlQueryPolicy::new(u64::MAX, 1, evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, evaluator.scratch_entries - 1),
    ] {
        let s = source(31);
        let drops = s.drops.clone();
        let mut cursor = EdgeAggregateCursor::new(s, plan.clone(), policy, || Ok::<_, ()>(()));
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Evaluator(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    for stop in 1..=calls {
        let s = source(31);
        let drops = s.drops.clone();
        let mut at = 0;
        let mut cursor = EdgeAggregateCursor::new(s, plan.clone(), exact, || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Interrupted(at))) if at == stop
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn canonical_collection_pages_do_not_hide_a_late_source_failure() {
    for count in [0, 1] {
        let q = prepare(&format!(
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS values LIMIT {count}"
        ));
        let mut s = source(31);
        s.fail = Some(EId(5));
        let drops = s.drops.clone();
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
                Ok::<_, ()>(())
            });
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
                EdgeScanError::Source("late edge failure")
            ))))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn edge_collection_limits_and_every_interruption_release_no_partial_list() {
    let q = definition(2, 2, false, None);
    let p = EdgeAggregatePlan::compile(&q).unwrap();
    let mut calls = 0;
    let mut full = EdgeAggregateCursor::new(source(31), p.clone(), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = full.next().unwrap().unwrap();
    let r = full.row_stats();
    let e = full.evaluator_stats();
    drop(full);
    let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
    assert_eq!(
        EdgeAggregateCursor::new(source(31), p.clone(), exact, || Ok::<_, ()>(()))
            .next()
            .unwrap()
            .unwrap(),
        expected
    );
    for stop in 1..=calls {
        let s = source(31);
        let drops = s.drops.clone();
        let mut at = 0;
        let mut c = EdgeAggregateCursor::new(s, p.clone(), exact, || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(c.next(), Some(Err(GqlQueryError::Interrupted(n))) if n == stop));
        assert_eq!(c.row_stats().result_rows, 0);
        assert_eq!(c.state(), EdgeScanState::Failed);
        assert!(c.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    for policy in [
        GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1),
    ] {
        let mut c = EdgeAggregateCursor::new(source(31), p.clone(), policy, || Ok::<_, ()>(()));
        assert!(c.next().unwrap().is_err());
        assert_eq!(c.row_stats().result_rows, 0);
        assert!(c.next().is_none());
    }
    for count in [0, 1] {
        let q = definition(0, 1, false, Some(count));
        let mut s = source(31);
        s.fail = Some(EId(5));
        let mut c =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
                Ok::<_, ()>(())
            });
        assert!(c.next().unwrap().is_err());
        assert_eq!(c.row_stats().result_rows, 0);
        assert!(c.next().is_none());
    }
    let s = source(31);
    let reads = s.reads.clone();
    let drops = s.drops.clone();
    let mut c = EdgeAggregateCursor::new(s, p, wide(), || -> Result<(), ()> {
        panic!("close drove source");
    });
    c.close();
    c.close();
    assert!(c.next().is_none());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_collection_order_preserves_correlated_arrays_and_unreturned_prefixes() {
    let edges = |ids: &[u128]| list(ids.iter().map(|&id| GraphValue::Edge(EId(id))).collect());
    for (text, expected) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS amounts,COLLECT(r) AS edges,COUNT(*) AS rows",
            vec![
                integer_list(&[-4, 7, 9, 9]),
                edges(&[4, 2, 5, 1, 3]),
                GraphAggregateValue::Count(5),
            ],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(b) AS count,COLLECT(r.p) AS amounts,COLLECT(r) AS edges",
            vec![
                GraphAggregateValue::Count(5),
                integer_list(&[9, 9, -4, 7]),
                edges(&[1, 3, 4, 2, 5]),
            ],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(b) AS count,COLLECT(DISTINCT r.p) AS amounts,COLLECT(r) AS edges",
            vec![
                GraphAggregateValue::Count(5),
                integer_list(&[9, -4, 7]),
                edges(&[1, 3, 4, 2, 5]),
            ],
        ),
    ] {
        let q = prepare(text);
        let before = q.canonical_bytes();
        let s = source(31);
        let expected = vec![GraphAggregateRow::from_group_values(Vec::new(), expected)];
        assert_eq!(batch(&q, &s), expected, "batch: {text}");
        let plan = EdgeAggregatePlan::compile(&q).unwrap();
        assert!(plan.ordered_collections.is_some());
        assert!(!plan.canonical_collections);
        let drops = s.drops.clone();
        let actual = EdgeAggregateCursor::new(s, plan, wide(), || Ok::<_, ()>(()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(actual, expected, "stream: {text}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(q.canonical_bytes(), before);
    }
}

#[test]
fn shared_collection_order_covers_joins_orientations_groups_and_result_windows() {
    for mask in 0..32 {
        for direction in 0..3 {
            let atom = |edge, end| match direction {
                0 => format!("-[{edge}:R]->({end})"),
                1 => format!("<-[{edge}:R]-({end})"),
                _ => format!("-[{edge}:R]-({end})"),
            };
            for hops in 1..=2 {
                let mut pattern = format!("(a){}", atom("r", "b"));
                if hops == 2 {
                    pattern += &atom("s", "c");
                }
                for group in ["", "b AS destination,"] {
                    for page in ["", " LIMIT 0", " SKIP 1 LIMIT 1"] {
                        let tail = if hops == 2 {
                            ",COLLECT(s) AS suffixes"
                        } else {
                            ""
                        };
                        let text = format!(
                            "MATCH {pattern} RETURN {group}COLLECT(r.p) AS amounts,\
                            COLLECT(r) AS roots,COLLECT(a) AS starts,COLLECT(DISTINCT r.p) AS support{tail}{page}"
                        );
                        let q = prepare(&text);
                        let s = source(mask);
                        let expected = batch(&q, &s);
                        let plan = EdgeAggregatePlan::compile(&q).unwrap();
                        assert!(plan.ordered_collections.is_some(), "{text}");
                        let mut cursor =
                            EdgeAggregateCursor::new(s, plan, wide(), || Ok::<_, ()>(()));
                        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(actual, expected, "mask {mask}: {text}");
                        assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn shared_collection_order_limits_and_all_checkpoints_refuse_before_a_partial_list() {
    let q = prepare("MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS amounts,COLLECT(r) AS edges");
    let plan = EdgeAggregatePlan::compile(&q).unwrap();
    let mut calls = 0;
    let mut full = EdgeAggregateCursor::new(source(31), plan.clone(), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let expected = full.next().unwrap().unwrap();
    let rows = full.row_stats();
    let stats = full.evaluator_stats();
    drop(full);
    let exact = GqlQueryPolicy::new(
        rows.snapshot_records,
        1,
        stats.work_units,
        stats.scratch_entries,
    );
    assert_eq!(
        EdgeAggregateCursor::new(source(31), plan.clone(), exact, || Ok::<_, ()>(()))
            .next()
            .unwrap()
            .unwrap(),
        expected
    );
    for stop in 1..=calls {
        let s = source(31);
        let drops = s.drops.clone();
        let mut seen = 0;
        let mut cursor = EdgeAggregateCursor::new(s, plan.clone(), exact, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(at))) if at == stop));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(cursor);
        assert_eq!(seen, stop);
    }
    for policy in [
        GqlQueryPolicy::new(u64::MAX, 1, stats.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, stats.scratch_entries - 1),
        GqlQueryPolicy::new(rows.snapshot_records - 1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
    ] {
        let mut cursor =
            EdgeAggregateCursor::new(source(31), plan.clone(), policy, || Ok::<_, ()>(()));
        assert!(cursor.next().unwrap().is_err());
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().is_none());
    }
    for page in [" LIMIT 0", " LIMIT 1", " SKIP 100 LIMIT 1"] {
        let q = prepare(&format!(
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS amounts,COLLECT(r) AS edges{page}"
        ));
        let mut s = source(31);
        s.fail = Some(EId(5));
        let drops = s.drops.clone();
        let mut cursor =
            EdgeAggregateCursor::new(s, EdgeAggregatePlan::compile(&q).unwrap(), wide(), || {
                Ok::<_, ()>(())
            });
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
                EdgeScanError::Source("late edge failure")
            ))))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
