use super::*;
use crate::scan_stream::ScanError;
use crate::{
    GraphElementKey, GraphSetProjection, GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
};

fn root(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}

fn property(column: usize, key: PropertyKeyId) -> GraphSetValue {
    GraphSetValue::MapGet {
        map: Box::new(GraphSetValue::Column(column)),
        key: format!("p{}", key.0).into(),
        property: GraphElementKey::Property(key),
    }
}

fn projected_root(direction: GlaDirection, any: bool) -> PreparedGraphSet {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    let relation = if any { "" } else { ":R" };
    root(&format!(
        "MATCH (a){left}[r{relation}]{right}(b) RETURN r,a,b"
    ))
    .project(
        vec![
            GraphSetProjection::new("r", GraphSetValue::Column(0)),
            GraphSetProjection::new("a", GraphSetValue::Column(1)),
            GraphSetProjection::new("b", GraphSetValue::Column(2)),
            GraphSetProjection::new("ep", property(0, P)),
            GraphSetProjection::new("ap", property(1, P)),
            GraphSetProjection::new("bp", property(2, P)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap()
}

#[test]
fn computed_edge_reads_match_independent_orientation_oracle_without_another_history() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for any in [false, true] {
            for page in [None, Some((1, 2))] {
                let mut query = projected_root(direction, any);
                if let Some((offset, count)) = page {
                    query = query.with_page(offset, Some(count));
                }
                let compiled = AsyncEdgeProjectionPlan::compile(&query).unwrap();
                assert_eq!(compiled.root_direction(), direction);
                assert_eq!(
                    compiled.selected_property_demand(),
                    EdgeScanPropertySelection {
                        edge: PropertySelection::one(P),
                        endpoints: PropertySelection::one(P),
                    }
                );
                let input = inputs();
                let expected = oracle(
                    &input,
                    direction,
                    any,
                    0,
                    page.map_or(0, |(offset, _)| offset as usize),
                    page.map_or(usize::MAX, |(_, count)| count as usize),
                );
                let input_len = input.len();
                let mut source = Source::new(input);
                source.expected_columns = 3;
                let counts = source.counts.clone();
                let mut cursor = AsyncEdgeProjectionCursor::new(source, compiled, policy(), ok);
                let mut rows = Vec::new();
                while let Some(row) = run(cursor.next()) {
                    rows.push(row.unwrap());
                }
                assert_eq!(
                    rows.iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(cursor.snapshot_seq(), CommitSeq(11));
                assert_eq!(cursor.row_stats().snapshot_records, input_len as u64);
                assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                assert_eq!(counts.reads.load(Ordering::SeqCst), input_len + 1);
                assert_eq!(counts.built.load(Ordering::SeqCst), input_len - 1);
                assert_eq!(counts.maximum_records.load(Ordering::SeqCst), 1);
                assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
                drop(rows);
                assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            }
        }
    }
}

#[test]
fn computed_edge_demand_is_complete_bounded_and_rejects_indirect_or_joined_reads() {
    let make = |overflow: Option<usize>| {
        let mut cells = vec![
            GraphSetProjection::new("r", GraphSetValue::Column(0)),
            GraphSetProjection::new("a", GraphSetValue::Column(1)),
        ];
        for (name, column) in [("edge", 0), ("endpoint", 1)] {
            let fields = GraphSetValue::List(
                (0..16)
                    .map(|key| property(column, PropertyKeyId(key)))
                    .collect(),
            );
            let value = if overflow == Some(column) {
                GraphSetValue::Case {
                    arms: vec![(
                        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        fields,
                    )],
                    otherwise: Some(Box::new(GraphSetValue::List(vec![property(
                        column,
                        PropertyKeyId(16),
                    )]))),
                }
            } else {
                fields
            };
            cells.push(GraphSetProjection::new(name, value));
        }
        root("MATCH (a)-[r:R]->(b) WHERE r.p > 0 AND a.p > 0 RETURN r,a,b")
            .project(cells, GraphSetQuantifier::All)
            .unwrap()
    };
    let selected = AsyncEdgeProjectionPlan::compile(&make(None))
        .unwrap()
        .selected_property_demand();
    assert_eq!(selected.edge.len(), 16);
    assert_eq!(selected.endpoints.len(), 16);
    assert!(AsyncEdgeProjectionPlan::compile(&make(Some(0))).is_err());
    assert!(AsyncEdgeProjectionPlan::compile(&make(Some(1))).is_err());

    let indirect = root("MATCH (a)-[r:R]->(b) RETURN r,a,b")
        .project(
            vec![
                GraphSetProjection::new("r", GraphSetValue::Column(0)),
                GraphSetProjection::new(
                    "value",
                    GraphSetValue::MapGet {
                        map: Box::new(GraphSetValue::Index {
                            list: Box::new(GraphSetValue::List(vec![GraphSetValue::Column(0)])),
                            index: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                                CanonicalScalar::Int(0),
                            ))),
                        }),
                        key: "p".into(),
                        property: GraphElementKey::Property(P),
                    },
                ),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert!(AsyncEdgeProjectionPlan::compile(&indirect).is_err());
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN r.p + 1 AS cost ORDER BY r",
        "MATCH (a)-[r:R]->(b) RETURN r, r.p + 1 AS cost ORDER BY r DESC",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p AS cost",
    ] {
        let query = crate::PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(AsyncEdgeProjectionPlan::compile(&query).is_err(), "{text}");
    }
}

#[test]
fn computed_edge_suffix_errors_and_pending_drop_preserve_the_orientation_owner() {
    for count in [0, 1] {
        let query = crate::PreparedGraphSetText::prepare(
            &format!("MATCH (a)-[r:R]->(b) RETURN r,10/r.p AS quotient LIMIT {count}"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let compiled = AsyncEdgeProjectionPlan::compile(&query).unwrap();
        let mut input = inputs();
        input.truncate(3);
        let mut zero = (**input[2].1.as_ref().unwrap()).clone();
        zero.properties = vec![(P, CanonicalScalar::Int(0).into())];
        input[2].1 = Some(Arc::new(zero));
        let mut source = Source::new(input);
        source.expected_columns = 2;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeProjectionCursor::new(source, compiled, policy(), ok);
        if count == 1 {
            drop(run(cursor.next()).unwrap().unwrap());
        }
        assert!(matches!(
            run(cursor.next()),
            Some(Err(GqlQueryError::Source(ScanError::Projection {
                row: 1,
                column: 1,
                ..
            })))
        ));
        assert_eq!(cursor.row_stats().result_rows, count);
        assert_eq!(cursor.row_stats().snapshot_records, 3);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert!(run(cursor.next()).is_none());
    }

    let query = projected_root(GlaDirection::Undirected, false).with_page(0, Some(1));
    let compiled = AsyncEdgeProjectionPlan::compile(&query).unwrap();
    let mut source = Source::new(inputs());
    source.expected_columns = 3;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeProjectionCursor::new(source, compiled, policy(), ok);
    let held = run(cursor.next()).unwrap().unwrap();
    assert_eq!(
        counts.records.load(Ordering::SeqCst),
        1,
        "second orientation retains the same record"
    );
    assert_eq!(counts.built.load(Ordering::SeqCst), 1);
    let mut pending = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&pending);
    assert!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(pending);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.guards.load(Ordering::SeqCst),
        1,
        "earlier delivery keeps its guard"
    );
    assert!(run(cursor.next()).is_none());
    drop(held);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}
