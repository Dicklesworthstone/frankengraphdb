use super::*;
use fgdb_gql::algebra::{
    GraphColumn, GraphMatchClause, GraphMatchMode, GraphPath, GraphPathFunction,
    GraphPatternBuilder, IntegerComparison, VertexPredicate,
};

fn collection(
    direction: GlaDirection,
    two: bool,
    filtered: bool,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("r", 0).unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.match_mode(GraphMatchMode::DifferentEdges);
    inner.vertex("b").unwrap().vertex("c").unwrap();
    inner.edge("b", R, direction, "c").unwrap();
    inner.capture_edge("s", 0).unwrap();
    let tuple = if two {
        inner.vertex("d").unwrap();
        inner.edge("c", R, direction, "d").unwrap();
        inner.capture_edge("t", 1).unwrap();
        vec!["b", "s", "c", "t", "d", "p"]
    } else {
        vec!["b", "s", "c", "p"]
    };
    inner.capture_path("p").unwrap();
    if filtered {
        inner
            .filter(
                if two { "d" } else { "c" },
                VertexPredicate::IntegerProperty {
                    key: P,
                    comparison: IntegerComparison::Greater,
                    value: 2,
                },
            )
            .unwrap();
    }
    outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&inner, Some("p")),
                GraphMatchClause::collect_tuple(&inner, tuple),
            ],
            &[
                GraphColumn::path("r", "r", GraphPathFunction::Edge),
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::collection("paths", 0),
                GraphColumn::collection("tuples", 1),
            ],
            0,
            count,
        )
        .unwrap()
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

// Compute history and complete edge products from fixture literals. This is
// independent of the storage readers, collection matcher and native projector.
fn oracle(cut: u64, direction: GlaDirection, two: bool, filtered: bool) -> Vec<Vec<GraphValue>> {
    if cut == 0 {
        return Vec::new();
    }
    let edges: Vec<_> = EDGES
        .into_iter()
        .filter(|(eid, _, _, _)| cut == 1 || *eid != 2)
        .collect();
    let orient = |from, to| match direction {
        GlaDirection::Forward => vec![(from, to)],
        GlaDirection::Reverse => vec![(to, from)],
        GlaDirection::Undirected if from == to => vec![(from, to)],
        GlaDirection::Undirected => vec![(from, to), (to, from)],
    };
    let property = |vertex| {
        if cut >= 2 && vertex == 0 {
            21
        } else {
            vertex as i64 + 1
        }
    };
    let mut rows = Vec::new();
    for &(r, a, b, _) in &edges {
        let mut routes = Vec::new();
        for &(s, x, y, _) in &edges {
            for (from, c) in orient(x, y) {
                if from != b {
                    continue;
                }
                if two {
                    for &(t, x, y, _) in &edges {
                        if s == t {
                            continue;
                        }
                        for (from, d) in orient(x, y) {
                            if from == c && (!filtered || property(d) > 2) {
                                routes.push(vec![(EId(s), IDS[c]), (EId(t), IDS[d])]);
                            }
                        }
                    }
                } else if !filtered || property(c) > 2 {
                    routes.push(vec![(EId(s), IDS[c])]);
                }
            }
        }
        let mut paths = Vec::new();
        let mut tuples = Vec::new();
        for route in routes {
            let path = GraphValue::Path(GraphPath::new(IDS[b], route.clone().into_boxed_slice()));
            paths.push(path.clone());
            let mut fields = vec![GraphValue::Vertex(IDS[b])];
            for (eid, endpoint) in route {
                fields.push(GraphValue::Edge(eid));
                fields.push(GraphValue::Vertex(endpoint));
            }
            fields.push(path);
            tuples.push(list(fields));
        }
        rows.push(vec![
            GraphValue::Edge(EId(r)),
            GraphValue::Vertex(IDS[a]),
            GraphValue::Vertex(IDS[b]),
            list(paths),
            list(tuples),
        ]);
    }
    rows
}

#[test]
fn cold_and_resident_async_collections_match_48_exact_history_route_and_predicate_cases() {
    let ((), report) = run_async_under_lab(0xc01d_20c1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, resident) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut cold = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let mut cases = 0;
        for cut in 0..=3 {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for two in [false, true] {
                    for filtered in [false, true] {
                        let definition = collection(direction, two, filtered, None);
                        let identity = definition.canonical_bytes();
                        let want = oracle(cut, direction, two, filtered);
                        let mut cursor = cold
                            .stream_graph_edge_joins_governed_at(
                                &cx,
                                &definition,
                                CommitSeq(cut),
                                policy(),
                            )
                            .unwrap();
                        let mut actual = Vec::new();
                        while let Some(row) = cursor.next().await {
                            actual.push(row.unwrap().values().to_vec());
                            assert!(pool.used() <= pool.limit());
                        }
                        assert_eq!(actual, want);
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
                        drop(cursor);
                        assert_eq!(pool.used(), baseline);

                        let mut cursor = resident
                            .stream_graph_edge_joins_governed_at(
                                &cx,
                                &definition,
                                CommitSeq(cut),
                                policy(),
                            )
                            .unwrap();
                        let mut actual = Vec::new();
                        while let Some(row) = cursor.next().await {
                            actual.push(row.unwrap().values().to_vec());
                        }
                        assert_eq!(actual, want);
                        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                        let sync = resident
                            .stream_graph_edges_governed_at(
                                &cx,
                                &definition,
                                CommitSeq(cut),
                                policy(),
                            )
                            .unwrap()
                            .map(|row| row.unwrap().values().to_vec())
                            .collect::<Vec<_>>();
                        assert_eq!(sync, want);
                        assert_eq!(definition.canonical_bytes(), identity);
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 48);
        drop(cold);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_collection_results_retain_an_independent_byte_owner_and_zero_limit_reads_nothing() {
    let ((), report) = run_async_under_lab(0xc01d_20c2, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut cold = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let zero = collection(GlaDirection::Forward, true, false, Some(0));
        let mut cursor = cold
            .stream_graph_edge_joins_governed_at(&cx, &zero, CommitSeq(1), policy())
            .unwrap();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        drop(cursor);
        assert_eq!(cold.buffer_stats().misses, 0);
        assert_eq!(pool.used(), baseline);

        let definition = collection(GlaDirection::Forward, true, false, Some(1));
        let mut cursor = cold
            .stream_graph_edge_joins_governed_at(&cx, &definition, CommitSeq(1), policy())
            .unwrap();
        drop(definition);
        let next = cursor.next();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&next);
        let row = next.await.unwrap().unwrap();
        assert_eq!(
            row.values(),
            oracle(1, GlaDirection::Forward, true, false)[0].as_slice()
        );
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        drop(cursor);
        drop(cold);
        assert!(
            pool.used() > 0,
            "only the delivered collection retains bytes"
        );
        let (row, guard) = row.into_parts();
        assert!(!row.values()[3].is_null());
        drop(row);
        assert!(pool.used() > 0);
        drop(guard);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
