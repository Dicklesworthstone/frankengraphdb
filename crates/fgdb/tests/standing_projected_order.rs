//! Real committed deltas must maintain computed aggregate rank and result pages.
use asupersync::lab::run_async_under_lab;
use fgdb::WriteBatch;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 100_000_000, 10_000_000)
}
fn delivery() -> GqlQueryPolicy {
    // Reading an accepted ranking must not scan any graph records.
    GqlQueryPolicy::new(0, 10_000, 100_000_000, 10_000_000)
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
async fn seed(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, key, value) in [
        (1, 1, Some(10)),
        (11, 1, Some(10)),
        (2, 2, Some(5)),
        (3, 3, None),
        (4, 4, Some(5)),
        (5, 5, Some(i64::MAX)),
        (6, 5, Some(i64::MAX - 1)),
    ] {
        let mut properties = vec![(PropertyKeyId(1), CanonicalScalar::Int(key))];
        if let Some(value) = value {
            properties.push((PropertyKeyId(2), CanonicalScalar::Int(value)));
        }
        batch.create_vertex(VId(id), vec![LabelId(1)], properties);
    }
    for (edge, owner) in [(1, 1), (2, 1), (3, 2), (4, 5)] {
        batch.add_edge(EId(edge), VId(owner), VId(4), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn native_ranked_aggregates_follow_updates_deletes_and_empty_ticks_without_rebinding() {
    let ((), report) = run_async_under_lab(0x6a98_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = seed(&commit).await;
        let params = GqlParameters::new();
        let statements = [
            "MATCH (n:L) RETURN n.p AS bucket,-COUNT(*) AS score ORDER BY score DESC,bucket ASC SKIP 1 LIMIT 2",
            "MATCH (n:L) RETURN DISTINCT -COUNT(*) AS score GROUP BY n.p ORDER BY score DESC LIMIT 2",
            "MATCH (n:L) RETURN n.p AS bucket,-SUM(n.q) AS score,AVG(n.q) AS mean ORDER BY score DESC NULLS LAST,bucket ASC LIMIT 3",
            "MATCH (n:L) RETURN n.p AS bucket,-SUM(n.q) AS score ORDER BY score ASC NULLS FIRST,bucket DESC LIMIT 3",
            "MATCH (n:L) WITH n.p AS bucket,n.q AS amount RETURN bucket,-SUM(amount) AS score ORDER BY score DESC,bucket ASC LIMIT 3",
            "MATCH (a:L)-[:R]->(b) RETURN a.p AS bucket,-COUNT(*) AS score GROUP BY a.p ORDER BY score DESC,bucket ASC LIMIT 2",
            "MATCH (n:L) RETURN -SUM(n.q) AS score GROUP BY n.p HAVING COUNT(*)>1 ORDER BY score DESC LIMIT 2",
            "MATCH (n:L) RETURN -COUNT(*) AS score GROUP BY n.p ORDER BY score LIMIT 0",
        ];
        let mut registered = Vec::new();
        for text in statements {
            let prepared = PreparedNativeRead::prepare(text, &params, resolve).unwrap();
            let handle = prepared
                .register_standing(&mut db, &cx, &params, policy())
                .unwrap_or_else(|error| panic!("register {text}: {error}"));
            registered.push((text, handle));
        }
        for stage in 0..4 {
            if stage != 0 {
                let mut batch = WriteBatch::new(RelationId(1));
                match stage {
                    1 => {
                        batch.delete_vertex(VId(11));
                        batch.set_vertex_property(
                            VId(1),
                            PropertyKeyId(1),
                            Some(CanonicalScalar::Int(2)),
                        );
                        batch.set_vertex_property(
                            VId(1),
                            PropertyKeyId(2),
                            Some(CanonicalScalar::Int(7)),
                        );
                        batch.set_vertex_property(
                            VId(2),
                            PropertyKeyId(2),
                            Some(CanonicalScalar::Null),
                        );
                        batch.create_vertex(
                            VId(7),
                            vec![LabelId(1)],
                            vec![
                                (PropertyKeyId(1), CanonicalScalar::Int(6)),
                                (PropertyKeyId(2), CanonicalScalar::Int(2)),
                            ],
                        );
                        batch.add_edge(EId(5), VId(7), VId(4), vec![]);
                    }
                    2 => {
                        batch.delete_edge(EId(1));
                        batch.delete_edge(EId(2));
                        batch.delete_vertex(VId(1));
                        batch.delete_vertex(VId(3));
                        batch.set_vertex_property(
                            VId(7),
                            PropertyKeyId(2),
                            Some(CanonicalScalar::Int(i64::MAX)),
                        );
                    }
                    _ => {
                        // A real commit irrelevant to every registered input.
                        batch.create_vertex(VId(100), vec![], vec![]);
                    }
                }
                db.write(&commit, batch).await.unwrap();
            }
            for (text, handle) in &registered {
                let expected = db.query(&cx, text, &params, resolve, policy()).unwrap();
                let (at, actual) = db
                    .standing_native_query(&cx, handle, delivery())
                    .unwrap_or_else(|error| panic!("stage {stage}, {text}: {error}"));
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(actual, expected, "stage {stage}, {text}");
                let QueryResult::Rows { columns, rows } = &actual else {
                    panic!("read result");
                };
                assert_eq!(db.standing_native_columns(&cx, handle).unwrap(), columns);
                assert!(rows.len() <= 3);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameterized_computed_rank_registrations_keep_independent_bound_sort_cells() {
    let ((), report) = run_async_under_lab(0x6a98_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = seed(&commit).await;
        let text = "MATCH (n:L) RETURN n.p AS bucket,SUM(n.q)*$scale AS score ORDER BY score DESC,bucket ASC LIMIT 3";
        let positive = GqlParameters::new().with_int64("scale", 1).unwrap();
        let negative = GqlParameters::new().with_int64("scale", -1).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &positive, resolve).unwrap();
        let forward = prepared
            .register_standing(&mut db, &cx, &positive, policy())
            .unwrap();
        let backward = prepared
            .register_standing(&mut db, &cx, &negative, policy())
            .unwrap();
        drop(prepared);
        for changed in [false, true] {
            if changed {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(4),
                    PropertyKeyId(2),
                    Some(CanonicalScalar::Int(-30)),
                );
                db.write(&commit, batch).await.unwrap();
            }
            let (_, a) = db.standing_native_query(&cx, &forward, delivery()).unwrap();
            let (_, b) = db
                .standing_native_query(&cx, &backward, delivery())
                .unwrap();
            assert_eq!(
                a,
                db.query(&cx, text, &positive, resolve, policy()).unwrap()
            );
            assert_eq!(
                b,
                db.query(&cx, text, &negative, resolve, policy()).unwrap()
            );
            assert_ne!(
                a, b,
                "bound ranks must not borrow another registration's scale"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
