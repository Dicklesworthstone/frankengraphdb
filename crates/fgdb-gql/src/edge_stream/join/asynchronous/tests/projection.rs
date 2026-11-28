use super::*;
use crate::scan_stream::ScanError;
use crate::{
    GraphElementKey, GraphSetProjection, GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
};

fn root(text: &str) -> PreparedGraphSet {
    prepared(text).into()
}

fn property(column: usize, key: PropertyKeyId) -> GraphSetValue {
    GraphSetValue::MapGet {
        map: Box::new(GraphSetValue::Column(column)),
        key: format!("p{}", key.0).into(),
        property: GraphElementKey::Property(key),
    }
}

fn projected(text: &str) -> PreparedGraphSet {
    root(text)
        .project(
            vec![
                GraphSetProjection::new("r", GraphSetValue::Column(0)),
                GraphSetProjection::new("a", GraphSetValue::Column(1)),
                GraphSetProjection::new("s", GraphSetValue::Column(2)),
                GraphSetProjection::new("b", GraphSetValue::Column(3)),
                GraphSetProjection::new("c", GraphSetValue::Column(4)),
                GraphSetProjection::new("rp", property(0, P)),
                GraphSetProjection::new("sp", property(2, P)),
                GraphSetProjection::new("ap", property(1, P)),
                GraphSetProjection::new("bp", property(3, P)),
                GraphSetProjection::new("cp", property(4, P)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

#[test]
fn computed_join_reads_all_current_bindings_without_new_history_admission() {
    for (shape, directions, any) in [
        (0, [GlaDirection::Forward; 2], false),
        (0, [GlaDirection::Reverse; 2], false),
        (0, [GlaDirection::Undirected; 2], false),
        (1, [GlaDirection::Forward; 2], false),
        (2, [GlaDirection::Undirected; 2], false),
        (0, [GlaDirection::Forward, GlaDirection::Reverse], true),
    ] {
        let first = format!("MATCH (a){}", atom("r", "b", directions[0], any));
        let next = match shape {
            1 => format!(", (a){}", atom("s", "c", directions[1], any)),
            2 => atom("s", "a", directions[1], any),
            _ => atom("s", "c", directions[1], any),
        };
        let end = if shape == 2 { "a AS c" } else { "c" };
        let text = format!("{first}{next} RETURN r,a,s,b,{end}");
        let baseline = Source::new(63);
        let baseline_counts = baseline.counts.clone();
        let mut baseline = AsyncEdgeJoinCursor::new(baseline, plan(&text), policy(), ok);
        while let Some(row) = run(baseline.next()) {
            row.unwrap();
        }
        for page in [None, Some((1, 2))] {
            let mut query = projected(&text);
            if let Some((offset, count)) = page {
                query = query.with_page(offset, Some(count));
            }
            let compiled = AsyncEdgeJoinProjectionPlan::compile(&query).unwrap();
            assert_eq!(compiled.root_direction(), directions[0]);
            assert_eq!(
                compiled.selected_property_demand(),
                EdgeScanPropertySelection {
                    edge: PropertySelection::one(P),
                    endpoints: PropertySelection::one(P),
                }
            );
            let source = Source::new(63);
            let mut expected = oracle(
                &source,
                shape,
                directions,
                any,
                false,
                page.map_or(0, |(offset, _)| offset as usize),
                page.map_or(usize::MAX, |(_, count)| count as usize),
            );
            for row in &mut expected {
                // Independent endpoint oracle: the fixture assigns 4 and 7
                // to the first two identities; the last has no property.
                for column in [1, 3, 4] {
                    let GraphValue::Vertex(vid) = &row[column] else {
                        panic!("oracle endpoint identity");
                    };
                    let vid = *vid;
                    row.push(GraphValue::Scalar(if vid == IDS[0] {
                        CanonicalScalar::Int(4)
                    } else if vid == IDS[1] {
                        CanonicalScalar::Int(7)
                    } else {
                        CanonicalScalar::Null
                    }));
                }
            }
            let counts = source.counts.clone();
            let limits = if page.is_some() {
                GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000)
            } else {
                policy()
            };
            let mut cursor = AsyncEdgeJoinProjectionCursor::new(source, compiled, limits, ok);
            let mut actual = Vec::new();
            while let Some(row) = run(cursor.next()) {
                actual.push(row.unwrap().values().to_vec());
            }
            assert_eq!(actual, expected, "{text}, page {page:?}");
            assert_eq!(cursor.snapshot_seq(), CommitSeq(13));
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
            assert_eq!(
                cursor.row_stats().snapshot_records,
                baseline.row_stats().snapshot_records
            );
            assert_eq!(
                counts.roots.load(Ordering::SeqCst),
                baseline_counts.roots.load(Ordering::SeqCst)
            );
            assert_eq!(
                counts.seeks.load(Ordering::SeqCst),
                baseline_counts.seeks.load(Ordering::SeqCst)
            );
            assert_eq!(counts.output_columns.load(Ordering::SeqCst), 5);
            assert!(counts.peak.load(Ordering::SeqCst) <= 2);
            assert_eq!(counts.records.load(Ordering::SeqCst), 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn computed_join_demands_cover_every_binding_and_lazy_branch_before_masking() {
    let make = |overflow: Option<usize>| {
        let mut cells = vec![
            GraphSetProjection::new("r", GraphSetValue::Column(0)),
            GraphSetProjection::new("a", GraphSetValue::Column(1)),
            GraphSetProjection::new("s", GraphSetValue::Column(2)),
        ];
        for (name, column) in [("child", 2), ("last", 4)] {
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
        root("MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE r.p > 0 AND a.p > 0 RETURN r,a,s,b,c")
            .project(cells, GraphSetQuantifier::All)
            .unwrap()
    };
    let keys = AsyncEdgeJoinProjectionPlan::compile(&make(None))
        .unwrap()
        .selected_property_demand();
    assert_eq!(keys.edge.len(), 16);
    assert_eq!(keys.endpoints.len(), 16);
    assert!(AsyncEdgeJoinProjectionPlan::compile(&make(Some(2))).is_err());
    assert!(AsyncEdgeJoinProjectionPlan::compile(&make(Some(4))).is_err());

    for text in [
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r.p+s.p AS cost ORDER BY r,s",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p AS cost ORDER BY r DESC,s",
    ] {
        let query = crate::PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(
            AsyncEdgeJoinProjectionPlan::compile(&query).is_err(),
            "{text}"
        );
    }
}

#[test]
fn computed_join_suffix_errors_keep_full_occurrence_ordinals_and_fuse_the_frame() {
    for count in [0, 1] {
        let query = crate::PreparedGraphSetText::prepare(
            &format!("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,10/s.p AS quotient LIMIT {count}"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let compiled = AsyncEdgeJoinProjectionPlan::compile(&query).unwrap();
        let source = Source::new(15);
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinProjectionCursor::new(source, compiled, policy(), ok);
        if count == 1 {
            let first = run(cursor.next()).unwrap().unwrap();
            assert_eq!(first.values()[0], GraphValue::Edge(EId(0)));
            assert_eq!(first.values()[1], GraphValue::Edge(EId(1)));
            drop(first);
        }
        assert!(matches!(
            run(cursor.next()),
            Some(Err(GqlQueryError::Source(ScanError::Projection {
                row: 1,
                column: 2,
                ..
            })))
        ));
        assert_eq!(cursor.row_stats().result_rows, count);
        assert_eq!(cursor.row_stats().snapshot_records, 4);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert!(run(cursor.next()).is_none());
    }
}

#[test]
fn dropping_a_pending_computed_join_releases_its_ancestor_and_source() {
    let query = projected("MATCH (a)-[r:R]-(b)-[s:R]-(c) RETURN r,a,s,b,c");
    let compiled = AsyncEdgeJoinProjectionPlan::compile(&query).unwrap();
    let mut source = Source::new(63);
    source.suspend = true;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinProjectionCursor::new(source, compiled, policy(), ok);
    drop(cursor.next());
    assert_eq!(cursor.state(), EdgeScanState::Open);
    assert_eq!(counts.roots.load(Ordering::SeqCst), 0);
    let mut pending = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&pending);
    assert!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    drop(pending);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert!(run(cursor.next()).is_none());
}
