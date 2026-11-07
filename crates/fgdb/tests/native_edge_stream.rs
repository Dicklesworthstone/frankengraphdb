//! Native text must reach the existing edge operator, not a materialized bag.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, ReadError,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::edge_stream::EdgeScanError;
use fgdb_gql::scan_stream::{ScanKind, ScanState};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const WIDE: VId = VId(1_u128 << 100);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(VId(0), 3), (WIDE, 7), (VId(u128::MAX), 11)] {
        batch.create_vertex(id, vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    for (id, from, to, value) in [
        (0, WIDE, VId(0), 4),
        (2, WIDE, VId(0), 5),
        (3, WIDE, WIDE, 8),
        (u128::MAX, VId(0), VId(u128::MAX), -1),
    ] {
        batch.add_edge(EId(id), from, to, vec![(P, CanonicalScalar::Int(value))]);
    }
    batch
}
fn result(columns: Vec<String>, rows: Vec<GraphValueRow>) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(GraphAggregateValue::Value)
                    .collect()
            })
            .collect(),
    }
}
fn statement(left: &str, right: &str, filter: &str, distinct: &str, page: &str) -> String {
    format!("MATCH (a){left}[r:R]{right}(b) {filter} RETURN {distinct} r, a, b, r.p AS cost {page}")
}

#[test]
fn independent_collections_keep_scan_order_outer_values_and_the_pinned_graph() {
    fn expected(edge_root: bool, direction: usize) -> Vec<GraphValueRow> {
        let edges = [
            (0, WIDE, VId(0), 4),
            (2, WIDE, VId(0), 5),
            (3, WIDE, WIDE, 8),
            (u128::MAX, VId(0), VId(u128::MAX), -1),
        ];
        let property = |id| {
            if id == VId(0) {
                3
            } else if id == WIDE {
                7
            } else {
                11
            }
        };
        let mut witnesses = Vec::new();
        for (eid, from, to, weight) in edges {
            match direction {
                0 => witnesses.push((from, eid, to, weight)),
                1 => witnesses.push((to, eid, from, weight)),
                _ => {
                    witnesses.push((from, eid, to, weight));
                    if from != to {
                        witnesses.push((to, eid, from, weight));
                    }
                }
            }
        }
        // Independent roots are visited by VId, then their relationships by
        // EId. In this fixture that differs from a global relationship scan.
        witnesses.sort_by_key(|&(from, eid, _, _)| (from, eid));
        let collection = |outer| {
            GraphValue::List(
                witnesses
                    .iter()
                    .map(|&(from, eid, to, weight)| {
                        GraphValue::List(
                            vec![
                                GraphValue::Vertex(from),
                                GraphValue::Vertex(to),
                                GraphValue::Edge(EId(eid)),
                                GraphValue::Scalar(CanonicalScalar::Int(property(outer))),
                                GraphValue::Scalar(CanonicalScalar::Int(property(from))),
                                GraphValue::Scalar(CanonicalScalar::Int(property(to))),
                                GraphValue::Scalar(CanonicalScalar::Int(weight)),
                            ]
                            .into_boxed_slice(),
                        )
                    })
                    .collect(),
            )
        };
        if edge_root {
            edges
                .into_iter()
                .map(|(eid, from, to, _)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(eid)),
                        GraphValue::Vertex(from),
                        GraphValue::Vertex(to),
                        collection(from),
                    ])
                })
                .collect()
        } else {
            [VId(0), WIDE, VId(u128::MAX)]
                .into_iter()
                .map(|id| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id), collection(id)])
                })
                .collect()
        }
    }

    let ((), report) = run_async_under_lab(0x6e65_0022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let args = GqlParameters::new();
        for edge_root in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let (head, prefix, kind) = if edge_root {
                ("MATCH (a)-[r:R]->(b)", "r,a,b", ScanKind::Edge)
            } else {
                ("MATCH (a)", "a", ScanKind::Vertex)
            };
            for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
                .into_iter()
                .enumerate()
            {
                // The pattern is independent of a; only its value expression
                // captures a property from the outer row.
                let text = format!(
                    "{head} RETURN {prefix}, [(x){left}[s:R]{right}(y) | \
                     [x,y,s,a.p,x.p,y.p,s.p]] AS graph"
                );
                let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                let (columns, mut cursor) = query.stream(&db, &cx, &args, policy()).unwrap();
                assert_eq!(cursor.kind(), kind);
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows, expected(edge_root, direction), "{text}");
                assert_eq!(
                    result(columns, rows),
                    query.execute(&db, &cx, &args, policy()).unwrap()
                );
                assert_eq!(cursor.state(), ScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
            let projection =
                format!("RETURN {prefix}, [(x)-[s:R]->(y) | [x,y,s,a.p,x.p,y.p,s.p]] AS graph");
            let text = format!("{head} {projection}");
            let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (_, mut paused) = query.stream(&db, &cx, &args, policy()).unwrap();
            let mut rows = vec![paused.next().unwrap().unwrap()];
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(WIDE, P, Some(CanonicalScalar::Int(70)));
            edit.delete_edge(EId(0));
            edit.add_edge(
                EId(99),
                VId(u128::MAX),
                WIDE,
                vec![(P, CanonicalScalar::Int(21))],
            );
            db.write(&commit, edit).await.unwrap();
            db.compact(&commit).await.unwrap();
            rows.extend(paused.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
            assert_eq!(rows, expected(edge_root, 0));
            assert_eq!(paused.snapshot_seq(), basis);
            let (_, old) = query.stream_in_view(&view, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), rows);
            let historical = PreparedNativeRead::prepare(
                &format!("{head} FOR SYSTEM_TIME AS OF SEQ {} {projection}", basis.0),
                &args,
                symbols,
            )
            .unwrap();
            let (_, old) = historical.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), rows);
            let (columns, fresh) = query.stream(&db, &cx, &args, policy()).unwrap();
            let fresh = fresh.collect::<Result<Vec<_>, _>>().unwrap();
            assert_ne!(fresh, rows);
            assert_eq!(
                result(columns, fresh),
                query.execute(&db, &cx, &args, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multihop_collections_keep_local_witnesses_properties_and_historical_cuts() {
    let ((), report) = run_async_under_lab(0x6e65_0021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let args = GqlParameters::new();
        // An independent finite witness list for seed(): two parallel first
        // edges reach the last edge, and the self-loop can precede either
        // parallel edge. Reusing the self-loop twice is forbidden locally.
        let witnesses: Vec<_> = [
            (0, u128::MAX, VId(0), VId(u128::MAX), 3, 11, 4, -1),
            (2, u128::MAX, VId(0), VId(u128::MAX), 3, 11, 5, -1),
            (3, 0, WIDE, VId(0), 7, 3, 8, 4),
            (3, 2, WIDE, VId(0), 7, 3, 8, 5),
        ]
        .into_iter()
        .map(|(s, t, c, d, cp, dp, sp, tp)| {
            GraphValue::List(
                vec![
                    GraphValue::Edge(EId(s)),
                    GraphValue::Edge(EId(t)),
                    GraphValue::Vertex(c),
                    GraphValue::Vertex(d),
                    GraphValue::Scalar(CanonicalScalar::Int(cp)),
                    GraphValue::Scalar(CanonicalScalar::Int(dp)),
                    GraphValue::Scalar(CanonicalScalar::Int(sp)),
                    GraphValue::Scalar(CanonicalScalar::Int(tp)),
                ]
                .into_boxed_slice(),
            )
        })
        .collect();
        for edge_root in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let (head, prefix, anchor, kind) = if edge_root {
                ("MATCH (a)-[r:R]->(b)", "r,a,b", "b", ScanKind::Edge)
            } else {
                ("MATCH (a)", "a", "a", ScanKind::Vertex)
            };
            let projection = format!(
                "RETURN {prefix}, [({anchor})-[s:R]->(c)-[t:R]->(d) | \
                 [s,t,c,d,c.p,d.p,s.p,t.p]] AS routes"
            );
            let text = format!("{head} {projection}");
            let list = |at| {
                GraphValue::List(
                    if at == WIDE {
                        witnesses.clone()
                    } else {
                        Vec::new()
                    }
                    .into_boxed_slice(),
                )
            };
            let expected: Vec<_> = if edge_root {
                [
                    (0, WIDE, VId(0)),
                    (2, WIDE, VId(0)),
                    (3, WIDE, WIDE),
                    (u128::MAX, VId(0), VId(u128::MAX)),
                ]
                .into_iter()
                .map(|(r, a, b)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(r)),
                        GraphValue::Vertex(a),
                        GraphValue::Vertex(b),
                        list(b),
                    ])
                })
                .collect()
            } else {
                [VId(0), WIDE, VId(u128::MAX)]
                    .into_iter()
                    .map(|a| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(a), list(a)]))
                    .collect()
            };
            let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let eager = query.execute(&db, &cx, &args, policy()).unwrap();
            let (columns, mut paused) = query.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(paused.kind(), kind);
            assert_eq!(paused.snapshot_seq(), basis);
            assert_eq!(result(columns, expected.clone()), eager);
            let mut actual = vec![paused.next().unwrap().unwrap()];

            // Change values on both hops and the witness set while the first
            // generation is still held by the cursor and the separate view.
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(31)));
            edit.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(41)));
            edit.delete_edge(EId(0));
            edit.add_edge(
                EId(99),
                VId(u128::MAX),
                WIDE,
                vec![(P, CanonicalScalar::Int(21))],
            );
            db.write(&commit, edit).await.unwrap();
            db.compact(&commit).await.unwrap();
            actual.extend(paused.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
            assert_eq!(actual, expected, "{text}");
            assert_eq!(paused.state(), ScanState::Exhausted);
            assert!(paused.next().is_none());
            let (_, old) = query.stream_in_view(&view, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), expected);

            let temporal = format!("{head} FOR SYSTEM_TIME AS OF SEQ {} {projection}", basis.0);
            let historical = PreparedNativeRead::prepare(&temporal, &args, symbols).unwrap();
            let (_, old) = historical.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(old.snapshot_seq(), basis);
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), expected);
            let (columns, fresh) = query.stream(&db, &cx, &args, policy()).unwrap();
            let fresh = fresh.collect::<Result<Vec<_>, _>>().unwrap();
            assert_ne!(fresh, expected);
            assert_eq!(
                result(columns, fresh),
                query.execute(&db, &cx, &args, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_trail_collections_preserve_route_occurrences_and_pinned_generations() {
    let ((), report) = run_async_under_lab(0x6e65_0024, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let args = GqlParameters::new();
        let last = VId(u128::MAX);
        // Literal trail endpoint order for seed(), including the zero-hop
        // prefix. Parallel edges are distinct witnesses. The self-loop can
        // occur once in a route, then either parallel edge and the last edge.
        let endpoints = |anchor| {
            if anchor == WIDE {
                vec![
                    WIDE,
                    VId(0),
                    last,
                    VId(0),
                    last,
                    WIDE,
                    VId(0),
                    last,
                    VId(0),
                    last,
                ]
            } else if anchor == VId(0) {
                vec![VId(0), last]
            } else {
                vec![last]
            }
        };
        let values = |anchor| {
            GraphValue::List(
                endpoints(anchor)
                    .into_iter()
                    .map(|id| {
                        let property = if id == WIDE {
                            7
                        } else if id == VId(0) {
                            3
                        } else {
                            11
                        };
                        GraphValue::List(
                            vec![
                                GraphValue::Vertex(id),
                                GraphValue::Scalar(CanonicalScalar::Int(property)),
                                GraphValue::Scalar(CanonicalScalar::Null),
                            ]
                            .into_boxed_slice(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        };
        let exactly_two = |anchor| {
            GraphValue::List(
                if anchor == WIDE {
                    vec![last, last, VId(0), VId(0)]
                } else {
                    Vec::new()
                }
                .into_iter()
                .map(GraphValue::Vertex)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            )
        };
        for edge_root in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let (head, prefix, anchor, kind) = if edge_root {
                ("MATCH (a)-[r:R]->(b)", "r,a,b", "b", ScanKind::Edge)
            } else {
                ("MATCH (a)", "a", "a", ScanKind::Vertex)
            };
            let projection = format!(
                "RETURN {prefix}, \
                 [({anchor})-[:R*0..3]->(c) | [c,c.p,NULL]] AS routes, \
                 [({anchor})-[:R*2..2]->(d) | d] AS two_hops"
            );
            let text = format!("{head} {projection}");
            let expected: Vec<_> = if edge_root {
                // The outer edge does not forbid that identity inside the
                // collection's own trail: the self-loop root may use it again.
                [
                    (0, WIDE, VId(0)),
                    (2, WIDE, VId(0)),
                    (3, WIDE, WIDE),
                    (u128::MAX, VId(0), last),
                ]
                .into_iter()
                .map(|(r, a, b)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(r)),
                        GraphValue::Vertex(a),
                        GraphValue::Vertex(b),
                        values(b),
                        exactly_two(b),
                    ])
                })
                .collect()
            } else {
                [VId(0), WIDE, last]
                    .into_iter()
                    .map(|a| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(a),
                            values(a),
                            exactly_two(a),
                        ])
                    })
                    .collect()
            };
            let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (columns, mut cursor) = query.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(cursor.kind(), kind);
            assert_eq!(cursor.snapshot_seq(), basis);
            assert_eq!(
                query.execute(&db, &cx, &args, policy()).unwrap(),
                result(columns, expected.clone()),
                "{text}"
            );
            let mut actual = vec![cursor.next().unwrap().unwrap()];
            let mut edit = WriteBatch::new(R);
            edit.delete_edge(EId(0));
            edit.set_vertex_property(last, P, Some(CanonicalScalar::Int(31)));
            edit.add_edge(EId(99), last, WIDE, vec![]);
            db.write(&commit, edit).await.unwrap();
            db.compact(&commit).await.unwrap();
            actual.extend(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
            assert_eq!(actual, expected, "{text}");
            assert_eq!(cursor.state(), ScanState::Exhausted);
            assert!(cursor.next().is_none());
            let (_, old) = query.stream_in_view(&view, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), expected);
            let temporal = format!("{head} FOR SYSTEM_TIME AS OF SEQ {} {projection}", basis.0);
            let historical = PreparedNativeRead::prepare(&temporal, &args, symbols).unwrap();
            let (_, old) = historical.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(old.snapshot_seq(), basis);
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), expected);
            let (columns, fresh) = query.stream(&db, &cx, &args, policy()).unwrap();
            let fresh = fresh.collect::<Result<Vec<_>, _>>().unwrap();
            assert_ne!(fresh, expected);
            assert_eq!(
                result(columns, fresh),
                query.execute(&db, &cx, &args, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_dispatch_matches_existing_edge_cursors_and_eager_rows_in_every_direction() {
    let ((), report) = run_async_under_lab(0x6e65_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let seq = db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for (left, right) in [("-", "->"), ("<-", "-"), ("-", "-")] {
            for filter in ["", "WHERE r.p > 0 AND a.p IS NOT NULL", "WHERE a = b"] {
                for distinct in ["ALL", "DISTINCT"] {
                    for page in ["", "SKIP 1 LIMIT 2", "LIMIT 0"] {
                        let text = statement(left, right, filter, distinct, page);
                        let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                        let pattern = PreparedGraphText::prepare(&text, symbols)
                            .unwrap()
                            .bind_parameters(&args)
                            .unwrap();
                        let eager = prepared.execute(&db, &cx, &args, policy()).unwrap();
                        let mut direct = db
                            .stream_graph_edges_governed(&cx, &pattern, policy())
                            .unwrap();
                        let (columns, mut cursor) =
                            prepared.stream(&db, &cx, &args, policy()).unwrap();
                        fn fused(_: &impl std::iter::FusedIterator) {}
                        fused(&cursor);
                        assert_eq!(cursor.kind(), ScanKind::Edge);
                        assert_eq!(cursor.snapshot_seq(), seq);
                        assert_eq!(cursor.row_stats().snapshot_records, 0);
                        assert_eq!(cursor.evaluator_stats().work_units, 0);
                        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(
                            actual,
                            direct.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                            "{text}"
                        );
                        assert_eq!(cursor.row_stats(), direct.row_stats());
                        assert_eq!(cursor.evaluator_stats(), direct.evaluator_stats());
                        assert_eq!(result(columns, actual), eager, "{text}");
                        assert_eq!(cursor.state(), ScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }
        // The same native API still chooses the original vertex specialization.
        let prepared =
            PreparedNativeRead::prepare("MATCH (n) RETURN n, n.p AS p", &args, symbols).unwrap();
        let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.kind(), ScanKind::Vertex);
        assert_eq!(
            cursor
                .by_ref()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            3
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn streamed_pattern_collections_preserve_parallel_witnesses_and_the_pinned_generation() {
    let ((), report) = run_async_under_lab(0x6e65_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (a)-[r:R]->(b) RETURN r, a, b, \
                    [(b)-[s:R]->(c) | c.p] AS neighbors, \
                    [(b)-[s:R]->(c) | s.p] AS weights";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let expected = prepared.execute(&db, &cx, &args, policy()).unwrap();
        let (columns, mut stream) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(stream.kind(), ScanKind::Edge);
        assert_eq!(stream.snapshot_seq(), basis);
        assert_eq!(stream.row_stats().snapshot_records, 0);
        let first = stream.next().unwrap().unwrap();

        // Change both a collected value and the witness set after the first
        // outer occurrence. Neither later pulls nor a separately opened cursor
        // on the retained view may consult this successor generation.
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(31)));
        update.delete_edge(EId(0));
        update.add_edge(
            EId(99),
            VId(u128::MAX),
            WIDE,
            vec![(P, CanonicalScalar::Int(21))],
        );
        let current = db.write(&commit, update).await.unwrap();
        let mut old_rows = vec![first];
        old_rows.extend(stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(stream.state(), ScanState::Exhausted);
        assert!(stream.next().is_none());
        assert_eq!(result(columns, old_rows.clone()), expected);

        // Literal witnesses pin duplicates, empty lists, self-loops and EId
        // order independently of the eager evaluator. Full-width identities
        // remain the outer keys; collection order is not destination sorting.
        for (row, (id, neighbors, weights)) in old_rows.iter().zip([
            (EId(0), vec![11], vec![-1]),
            (EId(2), vec![11], vec![-1]),
            (EId(3), vec![3, 3, 7], vec![4, 5, 8]),
            (EId(u128::MAX), vec![], vec![]),
        ]) {
            assert_eq!(row.values()[0], GraphValue::Edge(id));
            for (column, values) in [(3, neighbors), (4, weights)] {
                assert_eq!(
                    row.values()[column],
                    GraphValue::List(
                        values
                            .into_iter()
                            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                            .collect(),
                    ),
                );
            }
        }
        assert_eq!(old_rows.len(), 4);
        let (columns, cursor) = prepared
            .stream_in_view(&view, &cx, &args, policy())
            .unwrap();
        assert_eq!(
            result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap()),
            expected,
        );

        let (columns, cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), current);
        let fresh = result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(fresh, prepared.execute(&db, &cx, &args, policy()).unwrap());
        assert_ne!(fresh, expected);
        let temporal = format!(
            "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ {} \
             RETURN r, a, b, [(b)-[s:R]->(c) | c.p] AS neighbors, \
             [(b)-[s:R]->(c) | s.p] AS weights",
            basis.0,
        );
        let historical = PreparedNativeRead::prepare(&temporal, &args, symbols).unwrap();
        let (columns, cursor) = historical.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(
            result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap()),
            expected,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_rooted_collections_keep_incidence_order_tuples_and_null_placeholders() {
    let ((), report) = run_async_under_lab(0x6e65_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let integer = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let property = |id| match id {
            VId(0) => 3,
            WIDE => 7,
            VId(u128::MAX) => 11,
            _ => panic!("the independent fixture names exactly three vertices"),
        };
        // Enumerate the written incidences independently of every GLA or
        // database traversal. Edge identity, not neighbor identity, orders the
        // witnesses. An undirected self-loop contributes one occurrence.
        let edges = [
            (EId(0), WIDE, VId(0), 4),
            (EId(2), WIDE, VId(0), 5),
            (EId(3), WIDE, WIDE, 8),
            (EId(u128::MAX), VId(0), VId(u128::MAX), -1),
        ];
        for (left, right) in [("-", "->"), ("<-", "-"), ("-", "-")] {
            let atom = format!("(a){left}[r:R]{right}(b)");
            let text = format!(
                "MATCH (a) RETURN a, [{atom} | b] AS neighbors, \
                 [{atom} | [r.p,b.p]] AS details, \
                 [{atom} | CASE WHEN r.p=5 THEN null ELSE b.p END] AS holes"
            );
            let expected: Vec<_> = [VId(0), WIDE, VId(u128::MAX)]
                .into_iter()
                .map(|anchor| {
                    let witnesses: Vec<_> = edges
                        .iter()
                        .filter_map(|&(_, from, to, weight)| {
                            if left == "-" && from == anchor {
                                Some((to, weight))
                            } else if right == "-" && to == anchor {
                                Some((from, weight))
                            } else {
                                None
                            }
                        })
                        .collect();
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(anchor),
                        GraphValue::List(
                            witnesses
                                .iter()
                                .map(|&(neighbor, _)| GraphValue::Vertex(neighbor))
                                .collect(),
                        ),
                        GraphValue::List(
                            witnesses
                                .iter()
                                .map(|&(neighbor, weight)| {
                                    GraphValue::List(
                                        vec![integer(weight), integer(property(neighbor))].into(),
                                    )
                                })
                                .collect(),
                        ),
                        GraphValue::List(
                            witnesses
                                .iter()
                                .map(|&(neighbor, weight)| {
                                    if weight == 5 {
                                        GraphValue::Scalar(CanonicalScalar::Null)
                                    } else {
                                        integer(property(neighbor))
                                    }
                                })
                                .collect(),
                        ),
                    ])
                })
                .collect();
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (columns, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Vertex);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(
                result(columns, actual),
                prepared.execute(&db, &cx, &args, policy()).unwrap(),
                "{text}",
            );
            assert_eq!(cursor.state(), ScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_rooted_collections_keep_their_snapshot_across_paused_pulls() {
    let ((), report) = run_async_under_lab(0x6e65_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (a) RETURN a, [(a)-[r:R]->(b) | b.p] AS neighbors, \
                    [(a)-[r:R]->(b) | r.p] AS weights";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let expected = prepared.execute(&db, &cx, &args, policy()).unwrap();
        let (columns, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.kind(), ScanKind::Vertex);
        assert_eq!(cursor.snapshot_seq(), basis);
        let first = cursor.next().unwrap().unwrap();
        assert_eq!(first.values()[0], GraphValue::Vertex(VId(0)));
        assert_eq!(
            first.values()[1],
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(11))].into()),
        );

        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(31)));
        update.delete_edge(EId(0));
        update.add_edge(
            EId(99),
            VId(u128::MAX),
            WIDE,
            vec![(P, CanonicalScalar::Int(21))],
        );
        let current = db.write(&commit, update).await.unwrap();
        let mut actual = vec![first];
        actual.extend(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(result(columns, actual), expected);
        assert_eq!(cursor.state(), ScanState::Exhausted);
        assert!(cursor.next().is_none());

        let (columns, cursor) = prepared
            .stream_in_view(&view, &cx, &args, policy())
            .unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(
            result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap()),
            expected,
        );
        let (columns, cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), current);
        let fresh = result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap());
        assert_ne!(fresh, expected);
        assert_eq!(fresh, prepared.execute(&db, &cx, &args, policy()).unwrap());

        let historical = PreparedNativeRead::prepare(
            &format!(
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ {} \
                 RETURN a, [(a)-[r:R]->(b) | b.p] AS neighbors, \
                 [(a)-[r:R]->(b) | r.p] AS weights",
                basis.0,
            ),
            &args,
            symbols,
        )
        .unwrap();
        let (columns, cursor) = historical.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(
            result(columns, cursor.collect::<Result<Vec<_>, _>>().unwrap()),
            expected,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_rooted_collection_budgets_and_late_errors_preserve_complete_prefixes() {
    let ((), report) = run_async_under_lab(0x6e65_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (a) RETURN a, [(a)-[r:R]->(b) | [r.p,b.p]] AS details";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = cursor.row_stats();
        let events = cursor.evaluator_stats();
        assert_eq!(rows.result_rows, 3);
        assert!(rows.snapshot_records > 3);
        assert!(events.work_units > 0);
        assert!(events.scratch_entries > 0);
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            events.work_units,
            events.scratch_entries,
        );
        let (_, mut retry) = prepared.stream(&db, &cx, &args, exact).unwrap();
        assert_eq!(
            retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected,
        );
        assert_eq!((retry.row_stats(), retry.evaluator_stats()), (rows, events));
        for allowance in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, events.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, events.scratch_entries - 1),
        ] {
            let (_, mut refused) = prepared.stream(&db, &cx, &args, allowance).unwrap();
            let mut prefix = Vec::new();
            loop {
                match refused.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("collection budget lost by vertex dispatch: {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(refused.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(refused.state(), ScanState::Failed);
            let stopped = (refused.row_stats(), refused.evaluator_stats());
            assert!(refused.next().is_none());
            assert!(refused.next().is_none());
            refused.close();
            assert!(refused.next().is_none());
            assert_eq!((refused.row_stats(), refused.evaluator_stats()), stopped);
        }

        // Anonymous hops retain eager endpoint order even when EId order
        // disagrees and parallel edges contribute duplicate endpoints.
        let mut opposed = WriteBatch::new(R);
        opposed.add_edge(
            EId(1),
            WIDE,
            VId(u128::MAX),
            vec![(P, CanonicalScalar::Int(13))],
        );
        db.write(&commit, opposed).await.unwrap();
        for value in ["b", "b.p"] {
            for page in ["", " LIMIT 0"] {
                let text =
                    format!("MATCH (a) RETURN a, [(a)-[:R]->(b) | {value}] AS neighbors{page}");
                let anonymous = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                let (columns, mut cursor) = anonymous.stream(&db, &cx, &args, policy()).unwrap();
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                if page.is_empty() {
                    let ordered = if value == "b" {
                        [VId(0), VId(0), WIDE, VId(u128::MAX)].map(GraphValue::Vertex)
                    } else {
                        [3, 3, 7, 11].map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    };
                    assert_eq!(rows[1].values()[1], GraphValue::List(ordered.into()));
                } else {
                    assert!(rows.is_empty());
                    if value == "b" {
                        assert_eq!(cursor.row_stats().snapshot_records, 0);
                        assert_eq!(cursor.evaluator_stats().work_units, 0);
                    } else {
                        // A mapped Set page evaluates before selection.
                        assert!(cursor.row_stats().snapshot_records > 3);
                    }
                }
                assert_eq!(
                    result(columns, rows),
                    anonymous.execute(&db, &cx, &args, policy()).unwrap(),
                );
                assert_eq!(cursor.state(), ScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
        // Value mapping must begin only after endpoint ordering. VId(0) is
        // safe, WIDE overflows, and VMAX divides by zero; EId order would
        // observe VMAX's different exception before WIDE's exception.
        let error_order = PreparedNativeRead::prepare(
            "MATCH (a) WHERE a.p = 7 RETURN a, [(a)-[:R]->(b) | \
             1/(11-b.p) + (9223372036854775807 + (b.p-3))] AS values",
            &args,
            symbols,
        )
        .unwrap();
        assert!(matches!(
            error_order.execute(&db, &cx, &args, policy()),
            Err(QueryError::Set(GqlQueryError::Source(
                fgdb_gql::GraphSetExecutionError::Projection { row: 0, column: 1, error }
            ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::Overflow
        ));
        let (_, mut ordered) = error_order.stream(&db, &cx, &args, policy()).unwrap();
        assert!(matches!(
            ordered.next(),
            Some(Err(GqlQueryError::Source(
                fgdb_gql::scan_stream::ScanError::Projection { row: 0, column: 1, error }
            ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::Overflow
        ));
        assert_eq!(ordered.row_stats().result_rows, 0);
        assert_eq!(ordered.state(), ScanState::Failed);
        assert!(ordered.next().is_none());

        // Sorting completed endpoints cannot preserve the first failure of a
        // fallible witness predicate evaluated in a different order.
        for page in ["", " LIMIT 0"] {
            let text = format!(
                "MATCH (a) RETURN a, [(a)-[:R]->(b) WHERE b.p / 0 = 1 | b] AS neighbors{page}"
            );
            let fallible = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            assert!(matches!(
                fallible.stream(&db, &cx, &args, GqlQueryPolicy::new(0, 0, 0, 0)),
                Err(QueryError::Stream(GqlQueryError::Source(
                    fgdb_gql::stream::VertexScanError::Plan(_)
                )))
            ));
        }

        // LIMIT and close preserve backpressure even though each selected
        // root needs its own complete collection before it can be delivered.
        let zero = PreparedNativeRead::prepare(
            "MATCH (a) RETURN a, [(a)-[r:R]->(b) | b] AS neighbors LIMIT 0",
            &args,
            symbols,
        )
        .unwrap();
        let (_, mut untouched) = zero
            .stream(&db, &cx, &args, GqlQueryPolicy::new(0, 0, 0, 0))
            .unwrap();
        assert!(untouched.next().is_none());
        assert_eq!(untouched.row_stats().snapshot_records, 0);
        assert_eq!(untouched.evaluator_stats().work_units, 0);
        assert_eq!(untouched.evaluator_stats().scratch_entries, 0);
        let limited = PreparedNativeRead::prepare(
            "MATCH (a) RETURN a, [(a)-[r:R]->(b) | b] AS neighbors LIMIT 1",
            &args,
            symbols,
        )
        .unwrap();
        let (_, mut limited) = limited
            .stream(&db, &cx, &args, GqlQueryPolicy::new(2, 1, 10_000, 10_000))
            .unwrap();
        let first = limited.next().unwrap().unwrap();
        assert_eq!(first.values()[0], GraphValue::Vertex(VId(0)));
        assert_eq!(
            first.values()[1],
            GraphValue::List(vec![GraphValue::Vertex(VId(u128::MAX))].into()),
        );
        assert!(limited.next().is_none());
        assert_eq!(limited.row_stats().snapshot_records, 2);
        let (_, mut closed) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        closed.next().unwrap().unwrap();
        let stopped = (closed.row_stats(), closed.evaluator_stats());
        closed.close();
        closed.close();
        assert_eq!(closed.state(), ScanState::Closed);
        assert!(closed.next().is_none());
        assert_eq!((closed.row_stats(), closed.evaluator_stats()), stopped);

        // VId(0)'s only witness is safe. WIDE's second parallel edge divides
        // by zero: the first mapped element must not escape as a partial row.
        let broken = PreparedNativeRead::prepare(
            "MATCH (a) RETURN a, [(a)-[r:R]->(b) | 1/(r.p-5)] AS values",
            &args,
            symbols,
        )
        .unwrap();
        let (_, mut broken) = broken.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(
            broken.next().unwrap().unwrap().values()[0],
            GraphValue::Vertex(VId(0)),
        );
        assert!(matches!(
            broken.next(),
            Some(Err(GqlQueryError::Source(
                fgdb_gql::scan_stream::ScanError::Projection { row: 1, column: 1, error }
            ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(broken.row_stats().result_rows, 1);
        assert_eq!(broken.state(), ScanState::Failed);
        let stopped = (broken.row_stats(), broken.evaluator_stats());
        assert!(broken.next().is_none());
        assert_eq!((broken.row_stats(), broken.evaluator_stats()), stopped);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collected_fixed_paths_keep_oriented_segments_locals_and_closed_endpoints() {
    let ((), report) = run_async_under_lab(0x6e65_0017, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        batch.add_edge(EId(1), VId(0), WIDE, vec![(P, CanonicalScalar::Int(13))]);
        db.write(&commit, batch).await.unwrap();
        let args = GqlParameters::new();
        let edges = [
            (0, WIDE, VId(0), 4),
            (1, VId(0), WIDE, 13),
            (2, WIDE, VId(0), 5),
            (3, WIDE, WIDE, 8),
            (u128::MAX, VId(0), VId(u128::MAX), -1),
        ];
        let vertex_property = |id| {
            if id == VId(0) {
                3
            } else if id == WIDE {
                7
            } else {
                11
            }
        };
        for (edge_root, direction, closed) in [
            (false, 0, false),
            (false, 1, false),
            (false, 2, true),
            (true, 0, false),
        ] {
            let (left, right) = match direction {
                0 => ("-", "->"),
                1 => ("<-", "-"),
                _ => ("-", "-"),
            };
            let end_name = if closed { "a" } else { "c" };
            let body = format!(
                "[p = (a){left}[s:R]{right}(b){left}[t:R]{right}({end_name}) | \
                 [p,b,{end_name},s,t,nodes(p),relationships(p),b.p,{end_name}.p,s.p,t.p]] AS routes"
            );
            let text = if edge_root {
                format!("MATCH (x)-[r:R]->(a) RETURN r,x,a,{body}")
            } else {
                format!("MATCH (a) RETURN a,{body}")
            };
            // A literal pair enumeration is independent of the compiled
            // traversal/capture maps. A self-loop occurs once and DIFFERENT
            // EDGES rejects only reuse of the same relationship identity.
            let routes = |start| {
                let target = |at, from, to| match direction {
                    0 if from == at => Some(to),
                    1 if to == at => Some(from),
                    2 if from == at => Some(to),
                    2 if to == at => Some(from),
                    _ => None,
                };
                let mut values = Vec::new();
                for &(s, from, to, s_property) in &edges {
                    let Some(middle) = target(start, from, to) else {
                        continue;
                    };
                    for &(t, from, to, t_property) in &edges {
                        if s == t {
                            continue;
                        }
                        let Some(end) = target(middle, from, to) else {
                            continue;
                        };
                        if closed && end != start {
                            continue;
                        }
                        values.push(GraphValue::List(
                            vec![
                                GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                    start,
                                    vec![(EId(s), middle), (EId(t), end)].into_boxed_slice(),
                                )),
                                GraphValue::Vertex(middle),
                                GraphValue::Vertex(end),
                                GraphValue::Edge(EId(s)),
                                GraphValue::Edge(EId(t)),
                                GraphValue::Vertices([start, middle, end].into()),
                                GraphValue::Edges([EId(s), EId(t)].into()),
                                GraphValue::Scalar(CanonicalScalar::Int(vertex_property(middle))),
                                GraphValue::Scalar(CanonicalScalar::Int(vertex_property(end))),
                                GraphValue::Scalar(CanonicalScalar::Int(s_property)),
                                GraphValue::Scalar(CanonicalScalar::Int(t_property)),
                            ]
                            .into_boxed_slice(),
                        ));
                    }
                }
                GraphValue::List(values.into_boxed_slice())
            };
            let expected: Vec<_> = if edge_root {
                edges
                    .iter()
                    .map(|&(id, from, to, _)| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Edge(EId(id)),
                            GraphValue::Vertex(from),
                            GraphValue::Vertex(to),
                            routes(to),
                        ])
                    })
                    .collect()
            } else {
                [VId(0), WIDE, VId(u128::MAX)]
                    .into_iter()
                    .map(|id| {
                        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id), routes(id)])
                    })
                    .collect()
            };
            assert!(
                expected.iter().any(|row| match row.values().last() {
                    Some(GraphValue::List(routes)) => !routes.is_empty(),
                    _ => false,
                }),
                "{text}"
            );
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (columns, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
            let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(
                result(columns, actual),
                prepared.execute(&db, &cx, &args, policy()).unwrap(),
                "{text}",
            );
            assert_eq!(cursor.state(), ScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_trail_collections_keep_exact_routes_orientation_and_pinned_generations() {
    let ((), report) = run_async_under_lab(0x6e65_0028, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = seed();
        batch.add_edge(EId(1), VId(0), WIDE, vec![(P, CanonicalScalar::Int(13))]);
        let basis = db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let edges = [
            (EId(0), WIDE, VId(0)),
            (EId(1), VId(0), WIDE),
            (EId(2), WIDE, VId(0)),
            (EId(3), WIDE, WIDE),
            (EId(u128::MAX), VId(0), VId(u128::MAX)),
        ];
        let routes = |anchor, direction, reverse, closed, minimum, maximum| {
            // Enumerate complete products by length, independently of the
            // production DFS, then sort identity sequences with prefixes first.
            let mut frontier = vec![(Vec::<(EId, VId)>::new(), anchor)];
            let mut accepted = if minimum == 0 {
                frontier.clone()
            } else {
                Vec::new()
            };
            for length in 1..=maximum {
                let mut next = Vec::new();
                for (path, at) in frontier {
                    for &(eid, source, target) in &edges {
                        if path.iter().any(|&(used, _)| used == eid) {
                            continue;
                        }
                        let to = match direction {
                            0 if source == at => target,
                            1 if target == at => source,
                            2 if source == at => target,
                            2 if target == at => source,
                            _ => continue,
                        };
                        let mut extended = path.clone();
                        extended.push((eid, to));
                        next.push((extended, to));
                    }
                }
                if length >= minimum {
                    accepted.extend(next.iter().cloned());
                }
                frontier = next;
            }
            accepted.sort_unstable_by(|left, right| {
                left.0
                    .iter()
                    .map(|step| step.0)
                    .cmp(right.0.iter().map(|step| step.0))
            });
            GraphValue::List(
                accepted
                    .into_iter()
                    .filter_map(|(steps, endpoint)| {
                        if closed && endpoint != anchor {
                            return None;
                        }
                        let vertices: Vec<_> = std::iter::once(anchor)
                            .chain(steps.iter().map(|step| step.1))
                            .collect();
                        // The eager capture contract prefers forward orientation when
                        // both ends equal the declared start, including closed routes.
                        let (start, steps) = if reverse && endpoint != anchor {
                            (
                                endpoint,
                                steps
                                    .iter()
                                    .enumerate()
                                    .rev()
                                    .map(|(at, &(eid, _))| (eid, vertices[at]))
                                    .collect::<Vec<_>>(),
                            )
                        } else {
                            (anchor, steps)
                        };
                        let nodes = std::iter::once(start)
                            .chain(steps.iter().map(|step| step.1))
                            .collect::<Vec<_>>()
                            .into_boxed_slice();
                        let relationships = steps
                            .iter()
                            .map(|step| step.0)
                            .collect::<Vec<_>>()
                            .into_boxed_slice();
                        let length = steps.len() as i64;
                        let property = if endpoint == WIDE {
                            7
                        } else if endpoint == VId(0) {
                            3
                        } else {
                            11
                        };
                        Some(GraphValue::List(
                            vec![
                                GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                    start,
                                    steps.into_boxed_slice(),
                                )),
                                GraphValue::Vertices(nodes),
                                GraphValue::Edges(relationships),
                                GraphValue::Scalar(CanonicalScalar::Int(length)),
                                GraphValue::Vertex(endpoint),
                                GraphValue::Scalar(CanonicalScalar::Int(property)),
                            ]
                            .into_boxed_slice(),
                        ))
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            )
        };
        for edge_root in [false, true] {
            let (head, prefix, anchor) = if edge_root {
                ("MATCH (x)-[r:R]->(a)", "r,x,a", "a")
            } else {
                ("MATCH (a)", "a", "a")
            };
            for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
                .into_iter()
                .enumerate()
            {
                for (reverse, closed) in [(false, false), (true, false), (false, true)] {
                    for (minimum, maximum) in [(0, 0), (0, 3), (2, 2)] {
                        let endpoint = if closed { anchor } else { "c" };
                        let (start, end) = if reverse {
                            (endpoint, anchor)
                        } else {
                            (anchor, endpoint)
                        };
                        let text = format!(
                            "{head} RETURN {prefix}, [p = ({start}){left}[:R*{minimum}..{maximum}]{right}({end}) | \
                             [p,nodes(p),relationships(p),length(p),{endpoint},{endpoint}.p]] AS routes"
                        );
                        let walk_direction = if reverse && direction < 2 {
                            1 - direction
                        } else {
                            direction
                        };
                        let values =
                            |at| routes(at, walk_direction, reverse, closed, minimum, maximum);
                        let expected: Vec<_> = if edge_root {
                            edges
                                .iter()
                                .map(|&(r, x, a)| {
                                    GraphValueRow::from_owned_values(vec![
                                        GraphValue::Edge(r),
                                        GraphValue::Vertex(x),
                                        GraphValue::Vertex(a),
                                        values(a),
                                    ])
                                })
                                .collect()
                        } else {
                            [VId(0), WIDE, VId(u128::MAX)]
                                .into_iter()
                                .map(|a| {
                                    GraphValueRow::from_owned_values(vec![
                                        GraphValue::Vertex(a),
                                        values(a),
                                    ])
                                })
                                .collect()
                        };
                        let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                        let (columns, mut cursor) =
                            prepared.stream(&db, &cx, &args, policy()).unwrap();
                        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(actual, expected, "{text}");
                        assert_eq!(
                            result(columns, actual),
                            prepared.execute(&db, &cx, &args, policy()).unwrap(),
                            "{text}"
                        );
                        assert_eq!(cursor.state(), ScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }

        // Retained capture values, later roots, and a separately opened old-view
        // cursor all keep the original identities after writes and compaction.
        let text = "MATCH (a) RETURN a, [p = (a)-[:R*0..3]->(c) | \
                    [p,nodes(p),relationships(p),length(p),c,c.p]] AS routes";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let expected: Vec<_> = [VId(0), WIDE, VId(u128::MAX)]
            .into_iter()
            .map(|a| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(a),
                    routes(a, 0, false, false, 0, 3),
                ])
            })
            .collect();
        let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        let mut actual = vec![cursor.next().unwrap().unwrap()];
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(0));
        edit.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(31)));
        edit.add_edge(EId(99), VId(u128::MAX), WIDE, vec![]);
        db.write(&commit, edit).await.unwrap();
        db.compact(&commit).await.unwrap();
        actual.extend(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(actual, expected);
        let (_, old) = prepared
            .stream_in_view(&view, &cx, &args, policy())
            .unwrap();
        assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        let (columns, fresh) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        let fresh = fresh.collect::<Result<Vec<_>, _>>().unwrap();
        assert_ne!(fresh, expected);
        assert_eq!(
            result(columns, fresh),
            prepared.execute(&db, &cx, &args, policy()).unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_graph_pages_rebind_windows_and_charge_only_selected_results() {
    let ((), report) = run_async_under_lab(0x6e65_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let mut queries = vec![
            "MATCH (a) RETURN a, [(a)-[r:R]->(b) | [r.p,b.p]] AS details".to_owned(),
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,r.p + 1 AS cost".to_owned(),
        ];
        for (left, right) in [("-", "->"), ("<-", "-"), ("-", "-")] {
            queries.push(format!(
                "MATCH (a){left}[r:R]{right}(b) RETURN r,a,b,r.p + 1 AS cost,\
                 [(b)-[s:R]->(c) | c.p] AS values"
            ));
        }
        for base in queries {
            let complete = PreparedNativeRead::prepare(&base, &args, symbols).unwrap();
            let (columns, mut cursor) = complete.stream(&db, &cx, &args, policy()).unwrap();
            let full = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert!(!full.is_empty(), "{base}");
            assert_eq!(
                result(columns.clone(), full.clone()),
                complete.execute(&db, &cx, &args, policy()).unwrap(),
            );
            let source_rows = cursor.row_stats();
            let source_events = cursor.evaluator_stats();
            let text = format!("{base} SKIP $skip LIMIT $take");
            let initial = GqlParameters::new()
                .with_uint64("skip", 0)
                .unwrap()
                .with_uint64("take", 1)
                .unwrap();
            let prepared = PreparedNativeRead::prepare(&text, &initial, symbols).unwrap();
            for (skip, take) in [
                (0, 0),
                (0, 1),
                (1, 1),
                (2, 2),
                (99, 2),
                (u64::MAX, u64::MAX),
            ] {
                let args = GqlParameters::new()
                    .with_uint64("skip", skip)
                    .unwrap()
                    .with_uint64("take", take)
                    .unwrap();
                let expected: Vec<_> = full
                    .iter()
                    .skip(usize::try_from(skip).unwrap_or(usize::MAX))
                    .take(usize::try_from(take).unwrap_or(usize::MAX))
                    .cloned()
                    .collect();
                // The input population exceeds the final row allowance. A
                // LIMIT 0 page has no delivery allowance at all.
                let allowance = GqlQueryPolicy::new(
                    source_rows.snapshot_records,
                    expected.len() as u64,
                    source_events.work_units,
                    source_events.scratch_entries,
                );
                let (paged_columns, mut paged) =
                    prepared.stream(&db, &cx, &args, allowance).unwrap();
                let actual = paged.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(actual, expected, "{text}: {skip}/{take}");
                assert_eq!(paged_columns, columns);
                assert_eq!(
                    result(paged_columns, actual),
                    prepared.execute(&db, &cx, &args, policy()).unwrap(),
                );
                assert_eq!(
                    paged.row_stats().snapshot_records,
                    source_rows.snapshot_records
                );
                assert_eq!(paged.row_stats().result_rows, expected.len() as u64);
                assert_eq!(
                    paged.evaluator_stats().scratch_entries,
                    source_events.scratch_entries,
                );
                assert_eq!(paged.state(), ScanState::Exhausted);
                let stopped = (paged.row_stats(), paged.evaluator_stats());
                assert!(paged.next().is_none());
                assert_eq!((paged.row_stats(), paged.evaluator_stats()), stopped);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_graph_pages_preserve_skipped_and_suffix_errors_until_explicit_close() {
    let ((), report) = run_async_under_lab(0x6e65_0016, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for (base, column) in [
            ("MATCH (a) RETURN a, 1/(a.p-11) AS value", 1),
            ("MATCH (a)-[r:R]->(b) RETURN r,a,b,1/(r.p-8) AS value", 3),
        ] {
            for (page, delivered) in [
                (" SKIP 1 LIMIT 1", 1),
                (" LIMIT 0", 0),
                (" SKIP 3 LIMIT 1", 0),
            ] {
                let text = format!("{base}{page}");
                let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                assert!(prepared.execute(&db, &cx, &args, policy()).is_err());
                let allowance = GqlQueryPolicy::new(u64::MAX, delivered, u64::MAX, u64::MAX);
                let (_, mut cursor) = prepared.stream(&db, &cx, &args, allowance).unwrap();
                for _ in 0..delivered {
                    assert!(cursor.next().unwrap().is_ok());
                    assert_eq!(cursor.state(), ScanState::Open);
                }
                assert!(
                    matches!(
                        cursor.next(),
                        Some(Err(GqlQueryError::Source(
                            fgdb_gql::scan_stream::ScanError::Projection { row: 2, column: actual, error }
                        ))) if actual == column && error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                    ),
                    "{text}"
                );
                assert_eq!(cursor.row_stats().result_rows, delivered);
                assert_eq!(cursor.state(), ScanState::Failed);
                let stopped = (cursor.row_stats(), cursor.evaluator_stats());
                assert!(cursor.next().is_none());
                assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stopped);
            }

            // Closing after the selected row releases the source without
            // evaluating its unread, failing suffix.
            let text = format!("{base} SKIP 1 LIMIT 1");
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
            cursor.next().unwrap().unwrap();
            let stopped = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            assert_eq!(cursor.state(), ScanState::Closed);
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stopped);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_edge_streams_share_budgets_and_publish_only_complete_rows() {
    let ((), report) = run_async_under_lab(0x6e65_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for (left, right) in [("-", "->"), ("<-", "-"), ("-", "-")] {
            let text = format!("MATCH (a){left}[r:R]{right}(b) RETURN r, a, b, r.p + 1 AS cost");
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (columns, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let expected = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(
                result(columns, expected.clone()),
                prepared.execute(&db, &cx, &args, policy()).unwrap(),
                "{text}",
            );
            let rows = cursor.row_stats();
            let events = cursor.evaluator_stats();
            assert_eq!(rows.result_rows, expected.len() as u64);
            let exact = GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                events.work_units,
                events.scratch_entries,
            );
            let (_, mut retry) = prepared.stream(&db, &cx, &args, exact).unwrap();
            assert_eq!(
                retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
            );
            assert_eq!((retry.row_stats(), retry.evaluator_stats()), (rows, events));
            for dimension in [
                fgdb_gql::GlaLimitDimension::WorkUnits,
                fgdb_gql::GlaLimitDimension::ScratchEntries,
            ] {
                let mut allowance = exact;
                match dimension {
                    fgdb_gql::GlaLimitDimension::WorkUnits => {
                        allowance.evaluator.max_work_units -= 1;
                    }
                    fgdb_gql::GlaLimitDimension::ScratchEntries => {
                        allowance.evaluator.max_scratch_entries -= 1;
                    }
                }
                let (_, mut refused) = prepared.stream(&db, &cx, &args, allowance).unwrap();
                let mut prefix = Vec::new();
                loop {
                    match refused.next() {
                        Some(Ok(row)) => prefix.push(row),
                        Some(Err(GqlQueryError::Evaluator(error))) => {
                            assert_eq!(error.dimension, dimension);
                            break;
                        }
                        other => panic!("{text}, {dimension:?}: {other:?}"),
                    }
                }
                assert_eq!(prefix, expected[..prefix.len()]);
                assert_eq!(refused.row_stats().result_rows, prefix.len() as u64);
                assert_eq!(refused.state(), ScanState::Failed);
                let stopped = (refused.row_stats(), refused.evaluator_stats());
                assert!(refused.next().is_none());
                refused.close();
                assert!(refused.next().is_none());
                assert_eq!((refused.row_stats(), refused.evaluator_stats()), stopped);
            }
        }

        // The first relationship has p=4, the next has p=5. The later data
        // exception keeps its source-row ordinal and must not count or expose
        // the failed projection as a delivered row.
        let prepared = PreparedNativeRead::prepare(
            "MATCH (a)-[r:R]->(b) RETURN r, a, b, 1 / (r.p - 5) AS inverse",
            &args,
            symbols,
        )
        .unwrap();
        let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        let first = cursor.next().unwrap().unwrap();
        assert_eq!(first.values()[0], GraphValue::Edge(EId(0)));
        assert_eq!(
            first.values()[3],
            GraphValue::Scalar(CanonicalScalar::Int(-1))
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                fgdb_gql::scan_stream::ScanError::Projection { row: 1, column: 3, error }
            ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn paused_native_edge_streams_and_temporal_reads_do_not_switch_generations() {
    let ((), report) = run_async_under_lab(0x6e65_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let text = statement("-", "-", "", "ALL", "");
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
        let expected = prepared.execute(&db, &cx, &args, policy()).unwrap();
        let (columns, mut stream) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        let first = stream.next().unwrap().unwrap();
        let (_, mut pinned) = prepared
            .stream_in_view(&view, &cx, &args, policy())
            .unwrap();
        drop(prepared);
        drop(text);
        drop(args);
        let mut changed = WriteBatch::new(R);
        changed.set_edge_property(EId(2), P, Some(CanonicalScalar::Int(99)));
        changed.delete_edge(EId(0));
        db.write(&commit, changed).await.unwrap();
        {
            // Read overlapping base/overlay identities before compaction can
            // flatten them; neither paused old cursor advances here.
            let args = GqlParameters::new();
            let fresh = PreparedNativeRead::prepare(
                "MATCH (a)-[r:R]->(b) RETURN r, a, b, r.p AS cost",
                &args,
                symbols,
            )
            .unwrap();
            let (_, cursor) = fresh.stream(&db, &cx, &args, policy()).unwrap();
            let expected: Vec<_> = [
                (EId(2), WIDE, VId(0), 99),
                (EId(3), WIDE, WIDE, 8),
                (EId(u128::MAX), VId(0), VId(u128::MAX), -1),
            ]
            .into_iter()
            .map(|(eid, source, target, cost)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Edge(eid),
                    GraphValue::Vertex(source),
                    GraphValue::Vertex(target),
                    GraphValue::Scalar(CanonicalScalar::Int(cost)),
                ])
            })
            .collect();
            assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        }
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(WIDE);
        db.write(&commit, cascade).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let mut actual = vec![first];
        actual.extend(stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(result(columns.clone(), actual), expected);
        assert_eq!(
            result(
                columns,
                pinned.by_ref().collect::<Result<Vec<_>, _>>().unwrap()
            ),
            expected
        );
        assert_eq!(stream.snapshot_seq(), basis);
        let args = GqlParameters::new();
        let temporal = format!(
            "MATCH (a)-[r:R]-(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN r, a, b, r.p AS cost",
            basis.0
        );
        let prepared = PreparedNativeRead::prepare(&temporal, &args, symbols).unwrap();
        let (columns, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(
            result(
                columns,
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap()
            ),
            expected
        );
        let (columns, mut cursor) = prepared
            .stream_in_view(&view, &cx, &args, policy())
            .unwrap();
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(
            result(
                columns,
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap()
            ),
            expected
        );
        let fresh =
            PreparedNativeRead::prepare("MATCH (a)-[r:R]->(b) RETURN r, a, b", &args, symbols)
                .unwrap();
        let (_, cursor) = fresh.stream(&db, &cx, &args, policy()).unwrap();
        let rows = cursor.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values()[0], GraphValue::Edge(EId(u128::MAX)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_edge_rebinding_refusals_and_future_fences_precede_source_work() {
    let ((), report) = run_async_under_lab(0x6e65_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let low = GqlParameters::new().with_int64("floor", 0).unwrap();
        let high = GqlParameters::new().with_int64("floor", 5).unwrap();
        let calls = Cell::new(0);
        let prepared = PreparedNativeRead::prepare(
            "MATCH (a)-[r:R]->(b) WHERE r.p > $floor RETURN r, a, b",
            &low,
            |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        let resolved = calls.get();
        for (args, count) in [(&low, 3), (&high, 1)] {
            let (_, cursor) = prepared.stream_in_view(&view, &cx, args, policy()).unwrap();
            assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap().len(), count);
        }
        assert_eq!(calls.get(), resolved);
        assert!(matches!(
            prepared.stream_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(QueryError::PatternText(_))
        ));
        let empty = GqlParameters::new();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN a, r LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN r, a, r.p AS p ORDER BY p LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, c LIMIT 0",
        ] {
            let plan = PreparedNativeRead::prepare(text, &empty, symbols).unwrap();
            let error = plan
                .stream_in_view(&view, &cx, &empty, GqlQueryPolicy::new(0, 0, 0, 0))
                .unwrap_err();
            assert!(
                matches!(
                    &error,
                    QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(_)))
                ),
                "{error:?}"
            );
            assert!(std::error::Error::source(&error).is_some());
        }
        let future = format!(
            "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN r, a, b LIMIT 0",
            basis.0 + 1
        );
        let plan = PreparedNativeRead::prepare(&future, &empty, symbols).unwrap();
        assert!(matches!(
            plan.stream_in_view(&view, &cx, &empty, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::EdgeStream(GqlQueryError::Source(
                EdgeScanError::Source(ReadError::BeyondFrontier { .. })
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_edge_quotas_keep_prefixes_and_exact_counters_across_pulls() {
    let ((), report) = run_async_under_lab(0x6e65_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let plan = PreparedNativeRead::prepare(
            "MATCH (a)-[r:R]->(b) RETURN r, a, b, r.p AS cost",
            &args,
            symbols,
        )
        .unwrap();
        let (_, mut full) = plan.stream(&db, &cx, &args, policy()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = full.row_stats();
        let work = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            work.work_units,
            work.scratch_entries,
        );
        let (_, mut retry) = plan.stream(&db, &cx, &args, exact).unwrap();
        assert_eq!(
            retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert_eq!(retry.evaluator_stats(), work);
        for limited in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 100, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100, rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100, 100, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(100, 100, u64::MAX, work.scratch_entries - 1),
        ] {
            let (_, mut cursor) = plan.stream(&db, &cx, &args, limited).unwrap();
            let mut delivered = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => delivered.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota lost by native dispatch: {other:?}"),
                }
            }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(cursor.state(), ScanState::Failed);
            let before = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_edge_limit_and_close_never_drive_an_unrequested_candidate_suffix() {
    let ((), report) = run_async_under_lab(0x6e65_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![]);
        batch.create_vertex(WIDE, vec![], vec![]);
        for id in 0..512 {
            batch.add_edge(EId(id), VId(0), WIDE, vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let args = GqlParameters::new();
        for limit in [0, 1] {
            let text = format!("MATCH (a)-[r:R]->(b) RETURN r, a, b LIMIT {limit}");
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            let (_, mut cursor) = prepared
                .stream(
                    &db,
                    &cx,
                    &args,
                    GqlQueryPolicy::new(limit, limit, 10_000, 1000),
                )
                .unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(actual.len() as u64, limit);
            assert_eq!(cursor.row_stats().snapshot_records, limit);
        }
        let prepared =
            PreparedNativeRead::prepare("MATCH (a)-[r:R]->(b) RETURN r, a, b", &args, symbols)
                .unwrap();
        let (_, mut cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        cursor.next().unwrap().unwrap();
        let before = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        assert!(cursor.next().is_none());
        assert_eq!(cursor.state(), ScanState::Closed);
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
        assert_eq!(before.0.snapshot_records, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn root_edge_scan_work_is_linear_and_every_public_allowance_preserves_prefixes() {
    use fgdb_gql::GlaLimitDimension;
    use fgdb_gql::edge_stream::EdgeScanState;

    let ((), report) = run_async_under_lab(0x6e65_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let args = GqlParameters::new();
        let matching =
            PreparedGraphText::prepare("MATCH (a)-[r:R]->(b) RETURN r, a, b, r.p AS cost", symbols)
                .unwrap()
                .bind_parameters(&args)
                .unwrap();
        let mismatched = PreparedGraphText::prepare(
            "MATCH (a)-[r:Absent]->(b) RETURN r, a, b, r.p AS cost",
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "Absent") => Some(GraphSymbol::Relation(RelationId(2))),
                _ => symbols(kind, name),
            },
        )
        .unwrap()
        .bind_parameters(&args)
        .unwrap();

        for initial_count in [24_usize, 192] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let initial_ids: Vec<_> = (0..initial_count - 1)
                .map(|id| EId(2 * id as u128))
                .chain([EId(u128::MAX)])
                .collect();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(0), vec![], vec![]);
            batch.create_vertex(WIDE, vec![], vec![]);
            for &eid in &initial_ids {
                batch.add_edge(eid, VId(0), WIDE, vec![(P, CanonicalScalar::Int(1))]);
            }
            db.write(&commit, batch).await.unwrap();
            drop(db);
            let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            let before = db.read_session().unwrap();
            // Recovery discards the writer's initial overlay. A normal public
            // read now builds the flat base before the next write extends it.
            let mut warm = before
                .stream_graph_edges_governed(&cx, &mismatched, policy())
                .unwrap();
            assert!(warm.next().is_none());
            assert_eq!(warm.row_stats().snapshot_records, initial_count as u64);
            assert_eq!(warm.evaluator_stats().scratch_entries, 0);

            let added_count = initial_count / 3;
            let mut extended_ids = initial_ids.clone();
            let mut added = WriteBatch::new(R);
            for id in 0..added_count {
                let eid = EId(2 * id as u128 + 1);
                extended_ids.push(eid);
                added.add_edge(eid, VId(0), WIDE, vec![(P, CanonicalScalar::Int(2))]);
            }
            db.write(&commit, added).await.unwrap();
            extended_ids.sort_unstable();
            let after = db.read_session().unwrap();

            for (view, identities, overlay_count) in [
                (before, initial_ids, 0_usize),
                (after, extended_ids, added_count),
            ] {
                let n = identities.len() as u64;
                // An AVL tree with k nodes has height <= 2*floor(log2(k))+2.
                // The source reserves one spine entry per level, once.
                let spine_bound = if overlay_count == 0 {
                    0
                } else {
                    2 * u64::from(usize::BITS - overlay_count.leading_zeros())
                };
                // A mismatched relation reads no endpoints or output fields.
                // Each single-version identity spends four Work events:
                // candidate admission, history winner, visible row, relation.
                // Enumeration spends n+1 plus at most k overlay-node visits.
                // The operator's first advance adds one. ScratchEntry also
                // spends Work, so the complete public bound is 5n+k+h+2.
                let scan_work_bound = 5 * n + overlay_count as u64 + spine_bound + 2;
                for (emits_rows, pattern) in [(false, &mismatched), (true, &matching)] {
                    let expected: Vec<_> = if emits_rows {
                        identities
                            .iter()
                            .map(|&eid| {
                                let value = if eid != EId(u128::MAX) && eid.0 % 2 == 1 {
                                    2
                                } else {
                                    1
                                };
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Edge(eid),
                                    GraphValue::Vertex(VId(0)),
                                    GraphValue::Vertex(WIDE),
                                    GraphValue::Scalar(CanonicalScalar::Int(value)),
                                ])
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let allowance = if emits_rows {
                        policy()
                    } else {
                        GqlQueryPolicy::new(n, 0, scan_work_bound, spine_bound)
                    };
                    let mut full = view
                        .stream_graph_edges_governed(&cx, pattern, allowance)
                        .unwrap();
                    assert_eq!(full.snapshot_seq(), view.frontier());
                    assert_eq!(full.row_stats().snapshot_records, 0);
                    assert_eq!(full.evaluator_stats().work_units, 0);
                    assert_eq!(full.evaluator_stats().scratch_entries, 0);
                    assert_eq!(
                        full.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                        expected
                    );
                    let rows = full.row_stats();
                    let events = full.evaluator_stats();
                    assert_eq!(rows.snapshot_records, n);
                    assert_eq!(rows.result_rows, expected.len() as u64);
                    if !emits_rows {
                        assert!(events.work_units <= scan_work_bound);
                        assert!(events.scratch_entries <= spine_bound);
                        assert_eq!(events.scratch_entries == 0, overlay_count == 0);
                    }
                    let exact = GqlQueryPolicy::new(
                        rows.snapshot_records,
                        rows.result_rows,
                        events.work_units,
                        events.scratch_entries,
                    );
                    for dimension in [
                        GlaLimitDimension::WorkUnits,
                        GlaLimitDimension::ScratchEntries,
                    ] {
                        let total = match dimension {
                            GlaLimitDimension::WorkUnits => events.work_units,
                            GlaLimitDimension::ScratchEntries => events.scratch_entries,
                        };
                        for limit in 0..total {
                            let mut allowance = exact;
                            match dimension {
                                GlaLimitDimension::WorkUnits => {
                                    allowance.evaluator.max_work_units = limit;
                                }
                                GlaLimitDimension::ScratchEntries => {
                                    allowance.evaluator.max_scratch_entries = limit;
                                }
                            }
                            let mut refused = view
                                .stream_graph_edges_governed(&cx, pattern, allowance)
                                .unwrap();
                            let mut prefix = Vec::new();
                            loop {
                                match refused.next() {
                                    Some(Ok(row)) => prefix.push(row),
                                    Some(Err(GqlQueryError::Evaluator(error))) => {
                                        assert_eq!(error.dimension, dimension);
                                        assert_eq!(error.limit, limit);
                                        assert_eq!(error.observed, u128::from(limit) + 1);
                                        break;
                                    }
                                    other => {
                                        panic!("{n} identities, {dimension:?}={limit}: {other:?}")
                                    }
                                }
                            }
                            assert_eq!(prefix, expected[..prefix.len()]);
                            assert_eq!(refused.row_stats().result_rows, prefix.len() as u64);
                            assert_eq!(refused.state(), EdgeScanState::Failed);
                            let stopped = (refused.row_stats(), refused.evaluator_stats());
                            let admitted = match dimension {
                                GlaLimitDimension::WorkUnits => stopped.1.work_units,
                                GlaLimitDimension::ScratchEntries => stopped.1.scratch_entries,
                            };
                            assert_eq!(admitted, limit);
                            assert!(refused.next().is_none());
                            assert!(refused.next().is_none());
                            assert_eq!((refused.row_stats(), refused.evaluator_stats()), stopped);
                            refused.close();
                            assert!(refused.next().is_none());
                            assert_eq!((refused.row_stats(), refused.evaluator_stats()), stopped);
                        }
                    }
                    // Public cursors fuse on failure. A fresh cursor on the
                    // same pinned view must accept the exact observed budget.
                    let mut retry = view
                        .stream_graph_edges_governed(&cx, pattern, exact)
                        .unwrap();
                    assert_eq!(
                        retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                        expected
                    );
                    assert_eq!(retry.row_stats(), rows);
                    assert_eq!(retry.evaluator_stats(), events);
                    assert_eq!(retry.state(), EdgeScanState::Exhausted);
                    assert!(retry.next().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_trail_filters_keep_occurrences_and_their_original_graph() {
    fn values(anchor: VId, minimum: usize, maximum: usize, predicate: usize) -> GraphValue {
        let last = VId(u128::MAX);
        let endpoints = match (minimum, maximum) {
            (0, 0) => vec![anchor],
            (2, 2) if anchor == WIDE => vec![last, last, VId(0), VId(0)],
            (2, 2) => Vec::new(),
            _ if anchor == WIDE => vec![
                WIDE,
                VId(0),
                last,
                VId(0),
                last,
                WIDE,
                VId(0),
                last,
                VId(0),
                last,
            ],
            _ if anchor == VId(0) => vec![VId(0), last],
            _ => vec![last],
        };
        // In seed(), only WIDE has an outgoing edge with p > 3. Only VId(0)
        // has an outgoing edge but none to the vertex with p = 7. Filtering
        // these literal route occurrences must never coalesce equal endpoints.
        GraphValue::List(
            endpoints
                .into_iter()
                .filter(|&endpoint| match predicate {
                    0 => endpoint == WIDE,
                    1 => endpoint != WIDE,
                    _ => endpoint == VId(0),
                })
                .map(|endpoint| {
                    let property = if endpoint == WIDE {
                        7
                    } else if endpoint == VId(0) {
                        3
                    } else {
                        11
                    };
                    GraphValue::List(
                        vec![
                            GraphValue::Vertex(endpoint),
                            GraphValue::Scalar(CanonicalScalar::Int(property)),
                        ]
                        .into_boxed_slice(),
                    )
                })
                .collect(),
        )
    }
    fn expected(
        edge_root: bool,
        minimum: usize,
        maximum: usize,
        predicate: usize,
    ) -> Vec<GraphValueRow> {
        if edge_root {
            [
                (0, WIDE, VId(0)),
                (2, WIDE, VId(0)),
                (3, WIDE, WIDE),
                (u128::MAX, VId(0), VId(u128::MAX)),
            ]
            .into_iter()
            .map(|(eid, from, to)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Edge(EId(eid)),
                    GraphValue::Vertex(from),
                    GraphValue::Vertex(to),
                    values(to, minimum, maximum, predicate),
                ])
            })
            .collect()
        } else {
            [VId(0), WIDE, VId(u128::MAX)]
                .into_iter()
                .map(|id| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(id),
                        values(id, minimum, maximum, predicate),
                    ])
                })
                .collect()
        }
    }

    let ((), report) = run_async_under_lab(0x6e65_0027, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let args = GqlParameters::new();
        for edge_root in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let (head, prefix, kind) = if edge_root {
                ("MATCH (x)-[r:R]->(a)", "r,x,a", ScanKind::Edge)
            } else {
                ("MATCH (a)", "a", ScanKind::Vertex)
            };
            for (minimum, maximum) in [(0, 0), (0, 3), (2, 2)] {
                for (predicate, filter) in [
                    "EXISTS { MATCH (b)-[t:R]->(c) WHERE t.p > 3 }",
                    "NOT EXISTS { MATCH (b)-[t:R]->(c) WHERE t.p > 3 }",
                    "EXISTS { MATCH (b)-[t:R]->(c) } \
                     AND NOT EXISTS { MATCH (b)-[u:R]->(d) WHERE d.p = 7 }",
                ]
                .into_iter()
                .enumerate()
                {
                    let text = format!(
                        "{head} RETURN {prefix}, [(a)-[:R*{minimum}..{maximum}]->(b) \
                         WHERE {filter} | [b,b.p]] AS routes"
                    );
                    let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                    let (columns, mut cursor) = query.stream(&db, &cx, &args, policy()).unwrap();
                    assert_eq!(cursor.kind(), kind);
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(
                        rows,
                        expected(edge_root, minimum, maximum, predicate),
                        "{text}"
                    );
                    assert_eq!(
                        result(columns, rows),
                        query.execute(&db, &cx, &args, policy()).unwrap(),
                        "{text}",
                    );
                    assert_eq!(cursor.state(), ScanState::Exhausted);
                }
            }
            let projection = format!(
                "RETURN {prefix}, [(a)-[:R*0..3]->(b) WHERE EXISTS {{ \
                 MATCH (b)-[t:R]->(c) WHERE t.p > 3 }} | [b,b.p]] AS routes"
            );
            let query =
                PreparedNativeRead::prepare(&format!("{head} {projection}"), &args, symbols)
                    .unwrap();
            let (_, mut paused) = query.stream(&db, &cx, &args, policy()).unwrap();
            let mut rows = vec![paused.next().unwrap().unwrap()];
            let mut change = WriteBatch::new(R);
            for eid in [0, 2, 3] {
                change.set_edge_property(EId(eid), P, Some(CanonicalScalar::Int(0)));
            }
            change.set_vertex_property(WIDE, P, Some(CanonicalScalar::Int(70)));
            db.write(&commit, change).await.unwrap();
            db.compact(&commit).await.unwrap();
            rows.extend(paused.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
            assert_eq!(rows, expected(edge_root, 0, 3, 0));
            assert_eq!(paused.snapshot_seq(), basis);
            let (_, old) = query.stream_in_view(&view, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), rows);
            let historical = PreparedNativeRead::prepare(
                &format!("{head} FOR SYSTEM_TIME AS OF SEQ {} {projection}", basis.0),
                &args,
                symbols,
            )
            .unwrap();
            let (_, old) = historical.stream(&db, &cx, &args, policy()).unwrap();
            assert_eq!(old.collect::<Result<Vec<_>, _>>().unwrap(), rows);
            let (columns, fresh) = query.stream(&db, &cx, &args, policy()).unwrap();
            let fresh = fresh.collect::<Result<Vec<_>, _>>().unwrap();
            assert_ne!(fresh, rows);
            assert_eq!(
                result(columns, fresh),
                query.execute(&db, &cx, &args, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_trail_filters_preserve_live_path_captures() {
    let ((), report) = run_async_under_lab(0x6e65_0029, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (a) RETURN a, [p = (a)-[:R*0..3]->(b) WHERE EXISTS { \
             MATCH (b)-[t:R]->(c) WHERE t.p > 3 } | p] AS paths",
            &args,
            symbols,
        )
        .unwrap();
        let expected: Vec<_> = [VId(0), WIDE, VId(u128::MAX)]
            .into_iter()
            .map(|at| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(at),
                    GraphValue::List(
                        if at == WIDE {
                            vec![
                                GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                    WIDE, Vec::new().into_boxed_slice(),
                                )),
                                GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                    WIDE, vec![(EId(3), WIDE)].into_boxed_slice(),
                                )),
                            ]
                        } else {
                            Vec::new()
                        }
                        .into_boxed_slice(),
                    ),
                ])
            })
            .collect();
        let (columns, cursor) = prepared.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(
            prepared.execute(&db, &cx, &args, policy()).unwrap(),
            result(columns, expected),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_nested_collection_filters_keep_parent_witnesses_and_capture_frames() {
    fn expected(edge_root: bool, direction: usize, predicate: usize) -> Vec<GraphValueRow> {
        let edges = [
            (0, WIDE, VId(0), 4),
            (2, WIDE, VId(0), 5),
            (3, WIDE, WIDE, 8),
            (u128::MAX, VId(0), VId(u128::MAX), -1),
        ];
        let property = |id| {
            if id == VId(0) {
                3
            } else if id == WIDE {
                7
            } else {
                11
            }
        };
        let values = |anchor| {
            let mut values = Vec::new();
            for (eid, from, to, weight) in edges {
                let target = match direction {
                    0 if from == anchor => to,
                    1 if to == anchor => from,
                    2 if from == anchor => to,
                    2 if to == anchor => from,
                    _ => continue,
                };
                // Each accepted parent contributes once, even when several
                // child relationships satisfy EXISTS. Child and parent edge
                // reuse domains are separate, including the self-loop.
                let qualifying_child = edges
                    .iter()
                    .any(|&(_, start, _, weight)| start == target && weight > 3);
                let accepted = match predicate {
                    0 => qualifying_child,
                    1 => !qualifying_child,
                    _ => {
                        edges.iter().any(|&(_, start, _, _)| start == target)
                            && !edges
                                .iter()
                                .any(|&(_, start, end, _)| start == target && property(end) == 7)
                    }
                };
                if accepted {
                    values.push(GraphValue::List(
                        vec![
                            GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                anchor,
                                vec![(EId(eid), target)].into_boxed_slice(),
                            )),
                            GraphValue::Edge(EId(eid)),
                            GraphValue::Vertex(target),
                            GraphValue::Scalar(CanonicalScalar::Int(weight)),
                            GraphValue::Scalar(CanonicalScalar::Int(property(target))),
                        ]
                        .into_boxed_slice(),
                    ));
                }
            }
            GraphValue::List(values.into_boxed_slice())
        };
        if edge_root {
            edges
                .into_iter()
                .map(|(eid, from, to, _)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(eid)),
                        GraphValue::Vertex(from),
                        GraphValue::Vertex(to),
                        values(to),
                    ])
                })
                .collect()
        } else {
            [VId(0), WIDE, VId(u128::MAX)]
                .into_iter()
                .map(|id| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id), values(id)])
                })
                .collect()
        }
    }

    let ((), report) = run_async_under_lab(0x6e65_0025, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for edge_root in [false, true] {
            let (head, prefix, kind) = if edge_root {
                ("MATCH (x)-[r:R]->(a)", "r,x,a", ScanKind::Edge)
            } else {
                ("MATCH (a)", "a", ScanKind::Vertex)
            };
            for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
                .into_iter()
                .enumerate()
            {
                for (predicate, filter) in [
                    "EXISTS { MATCH (b)-[t:R]->(c) WHERE t.p > 3 }",
                    "NOT EXISTS { MATCH (b)-[t:R]->(c) WHERE t.p > 3 }",
                    "EXISTS { MATCH (b)-[t:R]->(c) } \
                     AND NOT EXISTS { MATCH (b)-[u:R]->(d) WHERE d.p = 7 }",
                ]
                .into_iter()
                .enumerate()
                {
                    let text = format!(
                        "{head} RETURN {prefix}, \
                         [p = (a){left}[s:R]{right}(b) WHERE {filter} | \
                         [p,s,b,s.p,b.p]] AS values"
                    );
                    let query = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
                    let (columns, mut cursor) = query.stream(&db, &cx, &args, policy()).unwrap();
                    assert_eq!(cursor.kind(), kind);
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(rows, expected(edge_root, direction, predicate), "{text}");
                    assert_eq!(
                        result(columns, rows.clone()),
                        query.execute(&db, &cx, &args, policy()).unwrap(),
                        "{text}",
                    );
                    assert_eq!(cursor.state(), ScanState::Exhausted);
                    assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                    assert!(cursor.next().is_none());
                    if !edge_root && direction == 0 && predicate == 0 {
                        let stats = cursor.row_stats();
                        let evaluator = cursor.evaluator_stats();
                        let exact = GqlQueryPolicy::new(
                            stats.snapshot_records,
                            stats.result_rows,
                            evaluator.work_units,
                            evaluator.scratch_entries,
                        );
                        let (_, mut retry) = query.stream(&db, &cx, &args, exact).unwrap();
                        assert_eq!(retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), rows,);
                        assert_eq!(retry.row_stats(), stats);
                        assert_eq!(retry.evaluator_stats(), evaluator);
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reached_child_errors_never_publish_a_partial_collection() {
    let ((), report) = run_async_under_lab(0x6e65_0026, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let query = PreparedNativeRead::prepare(
            "MATCH (a) RETURN a, \
             [(a)-[s:R]->(b) WHERE EXISTS { \
             MATCH (b)-[t:R]->(c) WHERE 1/(t.p-5) > 0 } | s] AS values",
            &args,
            symbols,
        )
        .unwrap();
        assert!(query.execute(&db, &cx, &args, policy()).is_err());
        let (_, mut cursor) = query.stream(&db, &cx, &args, policy()).unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap().values(),
            &[
                GraphValue::Vertex(VId(0)),
                GraphValue::List(Vec::new().into_boxed_slice()),
            ],
        );
        // At WIDE the loop reaches a child whose first candidate is false
        // and whose second divides by zero. No partial WIDE list escapes.
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Data(error)))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), ScanState::Failed);
        let stopped = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stopped);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
