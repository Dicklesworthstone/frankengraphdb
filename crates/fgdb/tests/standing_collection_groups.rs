//! Graph-backed list/Any pipelines use the ordinary maintained group circuit.
//! The snapshot engine and explicit hand controls are independent oracles.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const B: PropertyKeyId = PropertyKeyId(1);
const N: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb4; 32],
        DatabaseSecurityNamespaceId([0xb5; 32]),
        [0xb6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "bucket") => Some(GraphSymbol::Property(B)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(N)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, bucket, amount) in [
        (1, 1, Some(7)),
        (2, 1, Some(7)),
        (3, 2, None),
        (u128::MAX, 2, Some(2)),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (B, CanonicalScalar::Int(bucket)),
                (
                    N,
                    amount.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ),
            ],
        );
    }
    batch
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap()
}
fn matches_snapshot(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    args: &GqlParameters,
    handle: &StandingQueryHandle,
) {
    let (at, actual) = db.standing_native_query(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(
        actual,
        db.query(cx, text, args, symbols, policy()).unwrap(),
        "{text}"
    );
}
fn result_rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read returned a write")
    };
    rows
}

fn list(values: &[i64]) -> GraphValue {
    GraphValue::List(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
            .collect(),
    )
}

const NATIVE_COLLECT: &str = "MATCH (n) RETURN collect(n.amount) AS values,collect(DISTINCT n.amount) AS unique,collect(n) AS nodes,COUNT(*) AS count,SUM(n.amount) AS total,AVG(n.amount) AS average";

const EDGE_COLLECT: &str = "MATCH (a)-[:R]->(b) RETURN collect(b.amount) AS values,collect(DISTINCT b.amount) AS unique,COUNT(*) AS count,SUM(b.amount) AS total,AVG(b.amount) AS average";

fn edge_seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, bucket, amount) in [(1, 1, 7), (2, 1, -4), (3, 2, 2), (4, 2, 11)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (B, CanonicalScalar::Int(bucket)),
                (N, CanonicalScalar::Int(amount)),
            ],
        );
    }
    // Physical identities and write arrival deliberately disagree with endpoint
    // order. The two parallel edges remain two occurrences; a self-loop is one.
    for (id, source, destination) in [
        (31, 3, 3),
        (14, 1, 4),
        (12, 1, 2),
        (30, 3, 1),
        (23, 2, 3),
        (11, 1, 2),
    ] {
        batch.add_edge(EId(id), VId(source), VId(destination), vec![]);
    }
    batch
}

#[test]
fn fixed_edge_collections_follow_incident_changes_parallel_edges_and_cascades() {
    let ((), report) = run_async_under_lab(0x4c47_0109, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, edge_seed()).await.unwrap();
        let args = GqlParameters::new();
        let texts = [
            EDGE_COLLECT,
            "MATCH (a)-[:R]->(b) RETURN a.bucket AS bucket,collect(b.amount) AS values,collect(DISTINCT b.amount) AS unique GROUP BY a.bucket ORDER BY a.bucket",
            "MATCH (a)<-[:R]-(b) RETURN collect(b.amount) AS values",
            "MATCH (a)-[:R]-(b) RETURN collect(b.amount) AS values",
            "MATCH (a)-[:R]->(b) RETURN collect(-b.amount) AS values,collect(DISTINCT -b.amount) AS unique",
            "MATCH (a)-[:R]->(b) RETURN collect(a) AS sources,collect(b) AS destinations,collect([a.amount,b.amount]) AS pairs",
            "MATCH (a)-[:R]->(b) WHERE a<>b AND (a.bucket=b.bucket OR b.amount>0) RETURN [x IN collect(b.amount) | x*2] AS values",
            "MATCH (a)-[:R]->(b) RETURN a.bucket AS bucket,[x IN collect(b.amount) | x+a.bucket] AS values GROUP BY a.bucket HAVING COUNT(b.amount)>0 ORDER BY SUM(b.amount) DESC LIMIT 2",
            "MATCH (a)-[:R]->(b) WITH a.bucket AS bucket,b.amount AS amount RETURN bucket,collect(amount) AS values GROUP BY bucket ORDER BY bucket",
            "MATCH (a)-->(b) RETURN collect(b.amount) AS values,collect(DISTINCT b.amount) AS unique",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
                assert_eq!(
                    prepared.subscription_footprint(&cx, &args).unwrap(),
                    (3, 2),
                    "{text}"
                );
                prepared
                    .register_standing(&mut db, &cx, &args, policy())
                    .unwrap()
            })
            .collect();
        let rows = result_rows(
            db.standing_native_query(&cx, &handles[0], policy())
                .unwrap()
                .1,
        );
        assert_eq!(rows[0][0].as_value(), Some(&list(&[-4, -4, 11, 2, 7, 2])));
        assert_eq!(rows[0][1].as_value(), Some(&list(&[-4, 11, 2, 7])));
        assert_eq!(rows[0][2].as_count(), Some(6));
        assert_eq!(rows[0][3].as_integer(), Some(14));
        let average = rows[0][4].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (7, 3));
        for (index, expected) in [
            (2, &[2, 7, 7, -4, 2, 7][..]),
            (3, &[-4, -4, 2, 11, 7, 7, 2, 7, -4, 2, 7][..]),
            (4, &[4, 4, -2, -2, -7, -11][..]),
        ] {
            let rows = result_rows(
                db.standing_native_query(&cx, &handles[index], policy())
                    .unwrap()
                    .1,
            );
            assert_eq!(rows[0][0].as_value(), Some(&list(expected)));
        }
        for (text, handle) in texts.iter().zip(&handles) {
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        let mut integrated: Vec<_> = handles
            .iter()
            .map(|handle| copy(db.standing_query(&cx, handle).unwrap().rows()))
            .collect();
        let expected: [&[i64]; 8] = [
            &[-4, -4, -9, 2, 7, 2],
            &[-4, -9, 2, 7, 2],
            &[-4, 2, -9, 2, -9, 7, 2],
            &[2, -9, 2, -9, 7, 2],
            &[-9, -9],
            &[-9, -9, 13, -9],
            &[-9, -9, 13, -9],
            &[],
        ];
        for (step, expected) in expected.into_iter().enumerate() {
            let mut edit = WriteBatch::new(RelationId(if step == 6 { 2 } else { 1 }));
            match step {
                0 => {
                    edit.set_vertex_property(VId(4), N, Some(CanonicalScalar::Int(-9)));
                }
                1 => {
                    edit.delete_edge(EId(11));
                }
                2 => {
                    edit.add_edge(EId(24), VId(2), VId(4), vec![]);
                    edit.add_edge(EId(13), VId(1), VId(3), vec![]);
                }
                3 => {
                    edit.set_vertex_property(VId(2), N, None);
                }
                4 => {
                    edit.delete_vertex(VId(3));
                }
                5 => {
                    edit.create_vertex(
                        VId(5),
                        vec![],
                        vec![(B, CanonicalScalar::Int(1)), (N, CanonicalScalar::Int(13))],
                    );
                    edit.add_edge(EId(54), VId(5), VId(4), vec![]);
                    edit.add_edge(EId(45), VId(4), VId(5), vec![]);
                }
                6 => {
                    edit.set_vertex_property(
                        VId(1),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(8)),
                    );
                    edit.add_edge(EId(51), VId(5), VId(1), vec![]);
                }
                _ => {
                    for id in [12, 14, 24, 54, 45] {
                        edit.delete_edge(EId(id));
                    }
                }
            }
            db.write(&commit, edit).await.unwrap();
            let rows = result_rows(
                db.standing_native_query(&cx, &handles[0], policy())
                    .unwrap()
                    .1,
            );
            assert_eq!(rows[0][0].as_value(), Some(&list(expected)), "step {step}");
            if step >= 6 {
                let any = result_rows(
                    db.standing_native_query(&cx, &handles[9], policy())
                        .unwrap()
                        .1,
                );
                let expected: &[i64] = if step == 6 {
                    &[-9, -9, 13, 7, -9]
                } else {
                    &[7]
                };
                assert_eq!(any[0][0].as_value(), Some(&list(expected)));
            }
            for ((text, handle), rows) in texts.iter().zip(&handles).zip(&mut integrated) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                rows.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*rows, db.standing_query(&cx, handle).unwrap().rows());
                matches_snapshot(&db, &cx, text, &args, handle);
            }
            if step == 5 {
                db.compact(&commit).await.unwrap();
                for (text, handle) in texts.iter().zip(&handles) {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
                    matches_snapshot(&db, &cx, text, &args, handle);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fixed_edge_collection_errors_under_zero_pages_fence_and_rebuild_atomically() {
    let ((), report) = run_async_under_lab(0x4c47_010a, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, edge_seed()).await.unwrap();
        let args = GqlParameters::new();
        let input_error = "MATCH (a)-[:R]->(b) RETURN collect(14/b.amount) AS values LIMIT 0";
        let output_error =
            "MATCH (a)-[:R]->(b) RETURN [x IN collect(b.amount) | 14/x] AS values LIMIT 0";
        let input = db
            .register_standing_native(&cx, input_error, &args, symbols, policy())
            .unwrap();
        let output = db
            .register_standing_native(&cx, output_error, &args, symbols, policy())
            .unwrap();
        let healthy = db
            .register_standing_native(&cx, EDGE_COLLECT, &args, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(0)));
        db.write(&commit, edit).await.unwrap();
        assert!(matches!(
            db.standing_native_query(&cx, &input, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert!(matches!(
            db.standing_native_query(&cx, &output, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::OutputExpression { .. },
                ..
            })
        ));
        for (text, handle) in [(input_error, &input), (output_error, &output)] {
            assert!(db.query(&cx, text, &args, symbols, policy()).is_err());
            assert!(
                db.register_standing_native(&cx, text, &args, symbols, policy())
                    .is_err()
            );
            assert!(db.rebuild_standing_query(&cx, handle, policy()).is_err());
        }
        matches_snapshot(&db, &cx, EDGE_COLLECT, &args, &healthy);
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(-4)));
        repair.delete_edge(EId(12));
        db.write(&commit, repair).await.unwrap();
        for (text, handle) in [(input_error, &input), (output_error, &output)] {
            assert!(db.standing_native_query(&cx, handle, policy()).is_err());
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        matches_snapshot(&db, &cx, EDGE_COLLECT, &args, &healthy);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fixed_edge_collection_replay_retains_parameters_and_reopens_after_compaction() {
    let ((), report) = run_async_under_lab(0x4c47_010b, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let directory = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &directory, keys())
            .await
            .unwrap();
        db.write(&commit, edge_seed()).await.unwrap();
        let text = "MATCH (a)-[:R]->(b) RETURN collect(b.amount+$extra) AS values,collect(DISTINCT b.amount+$extra) AS unique";
        let old = GqlParameters::new().with_int64("extra", 3).unwrap();
        let current = GqlParameters::new().with_int64("extra", 17).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &old, symbols).unwrap();
        let first = prepared
            .register_standing(&mut db, &cx, &old, policy())
            .unwrap();
        let second = prepared
            .register_standing(&mut db, &cx, &current, policy())
            .unwrap();
        drop(prepared);
        let mut subscription = db.open_standing_subscription(&cx, &first).unwrap();
        subscription
            .enable_replay(&mut db, &cx, 8, 1000, 1_000_000, policy())
            .unwrap();
        let baseline = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut accepted = copy(baseline.rows());
        subscription.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for step in 0..3 {
            let mut edit = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(13)));
                }
                1 => {
                    edit.delete_edge(EId(12));
                }
                _ => {
                    edit.add_edge(EId(41), VId(4), VId(1), vec![]);
                }
            }
            let at = db.write(&commit, edit).await.unwrap();
            matches_snapshot(&db, &cx, text, &old, &first);
            matches_snapshot(&db, &cx, text, &current, &second);
            expected.push((at, db.standing_native_bag(&cx, &first, policy()).unwrap().1));
        }
        // Delivery intentionally trails several commits; each retained signed
        // replacement reconstructs the corresponding earlier collection.
        for (at, expected) in expected {
            let frame = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
            assert!(!frame.is_snapshot());
            assert_eq!(frame.frontier(), at);
            accepted
                .integrate(frame.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(accepted, expected);
            subscription.acknowledge(frame.receipt()).unwrap();
        }
        assert!(subscription.poll(&db, &cx, policy()).unwrap().is_none());
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &directory, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &first),
            Err(StandingQueryError::ForeignHandle)
        ));
        let rebuilt = db
            .register_standing_native(&cx, text, &current, symbols, policy())
            .unwrap();
        matches_snapshot(&db, &cx, text, &current, &rebuilt);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_collections_keep_vertex_order_retractions_and_exact_composed_results() {
    let ((), report) = run_async_under_lab(0x4c47_0105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let texts = [
            NATIVE_COLLECT,
            "MATCH (n) RETURN n.bucket AS bucket,collect(n.amount) AS values,collect(DISTINCT n.amount) AS unique GROUP BY n.bucket ORDER BY n.bucket",
            "MATCH (n) RETURN [x IN collect(n.amount) | x*2] AS values,reduce(total=0,x IN collect(n.amount) | total+x) AS total,any(x IN collect(n.amount) WHERE x<0) AS negative",
            "MATCH (n) RETURN collect(-n.amount) AS values,collect(DISTINCT -n.amount) AS unique",
            "MATCH (n) RETURN n.bucket AS bucket,[x IN collect(n.amount) | x+n.bucket] AS values GROUP BY n.bucket HAVING COUNT(n.amount)>0 ORDER BY SUM(n.amount) DESC LIMIT 2",
            "MATCH (n) RETURN collect([n.bucket,n.amount]) AS pairs",
            "MATCH (n) WHERE n.amount IS NOT NULL AND n.bucket>1 RETURN collect(n.amount) AS values",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
                // The private ordered carrier uses the real source, one projection
                // and the existing group node. Reserve the same complete footprint.
                assert_eq!(prepared.subscription_footprint(&cx, &args).unwrap(), (3, 1));
                prepared
                    .register_standing(&mut db, &cx, &args, policy())
                    .unwrap()
            })
            .collect();
        let rows = result_rows(
            db.standing_native_query(&cx, &handles[0], policy())
                .unwrap()
                .1,
        );
        assert_eq!(rows[0][0].as_value(), Some(&list(&[7, 7, 2])));
        assert_eq!(rows[0][1].as_value(), Some(&list(&[7, 2])));
        assert_eq!(
            rows[0][2].as_value(),
            Some(&GraphValue::List(
                [1, 2, 3, u128::MAX]
                    .into_iter()
                    .map(|id| GraphValue::Vertex(VId(id)))
                    .collect()
            ))
        );
        assert_eq!(rows[0][3].as_count(), Some(4));
        assert_eq!(rows[0][4].as_integer(), Some(16));
        let average = rows[0][5].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (16, 3));
        let computed = result_rows(
            db.standing_native_query(&cx, &handles[3], policy())
                .unwrap()
                .1,
        );
        assert_eq!(computed[0][0].as_value(), Some(&list(&[-2, -7, -7])));
        for (text, handle) in texts.iter().zip(&handles) {
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        let mut integrated: Vec<_> = handles
            .iter()
            .map(|handle| copy(db.standing_query(&cx, handle).unwrap().rows()))
            .collect();
        let expected: [&[i64]; 7] = [
            &[7, 7, 5, 2],
            &[7, -4, 5, 2],
            &[7, -4, 11, 5, 2],
            &[-4, 11, 5, 2],
            &[-4, 11, 5, 2],
            &[-4, 11, 2],
            &[-4, 11, 7, 2],
        ];
        for (step, expected) in expected.into_iter().enumerate() {
            let mut edit = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    edit.create_vertex(
                        VId(99),
                        vec![],
                        vec![(B, CanonicalScalar::Int(2)), (N, CanonicalScalar::Int(5))],
                    );
                }
                1 => {
                    edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(-4)));
                }
                2 => {
                    edit.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(11)));
                }
                3 => {
                    edit.delete_vertex(VId(1));
                }
                4 => {
                    edit.set_vertex_property(VId(u128::MAX), B, Some(CanonicalScalar::Int(9)));
                }
                5 => {
                    edit.delete_vertex(VId(99));
                }
                _ => {
                    edit.create_vertex(
                        VId(4),
                        vec![],
                        vec![(B, CanonicalScalar::Int(1)), (N, CanonicalScalar::Int(7))],
                    );
                }
            }
            let at = db.write(&commit, edit).await.unwrap();
            for ((text, handle), rows) in texts.iter().zip(&handles).zip(&mut integrated) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                rows.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*rows, db.standing_query(&cx, handle).unwrap().rows());
                matches_snapshot(&db, &cx, text, &args, handle);
            }
            let rows = result_rows(
                db.standing_native_query(&cx, &handles[0], policy())
                    .unwrap()
                    .1,
            );
            assert_eq!(rows[0][0].as_value(), Some(&list(expected)), "step {step}");
        }
        db.compact(&commit).await.unwrap();
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            matches_snapshot(&db, &cx, text, &args, handle);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_collections_keep_global_empty_lists_and_all_null_groups() {
    let ((), report) = run_async_under_lab(0x4c47_0106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        let grouped = "MATCH (n) RETURN n.bucket AS bucket,collect(n.amount) AS values,COUNT(*) AS count GROUP BY n.bucket ORDER BY n.bucket";
        let global = db
            .register_standing_native(&cx, NATIVE_COLLECT, &args, symbols, policy())
            .unwrap();
        let groups = db
            .register_standing_native(&cx, grouped, &args, symbols, policy())
            .unwrap();
        let rows = result_rows(db.standing_native_query(&cx, &global, policy()).unwrap().1);
        assert_eq!(rows.len(), 1);
        for column in &rows[0][..3] {
            assert_eq!(column.as_value(), Some(&list(&[])));
        }
        assert_eq!(rows[0][3].as_count(), Some(0));
        assert!(rows[0][4].is_null() && rows[0][5].is_null());
        assert!(
            result_rows(db.standing_native_query(&cx, &groups, policy()).unwrap().1).is_empty()
        );
        let mut edit = WriteBatch::new(RelationId(1));
        edit.create_vertex(
            VId(3),
            vec![],
            vec![(B, CanonicalScalar::Int(2)), (N, CanonicalScalar::Null)],
        );
        edit.create_vertex(VId(5), vec![], vec![(B, CanonicalScalar::Int(1))]);
        db.write(&commit, edit).await.unwrap();
        let rows = result_rows(db.standing_native_query(&cx, &groups, policy()).unwrap().1);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row[1].as_value(), Some(&list(&[])));
            assert_eq!(row[2].as_count(), Some(1));
        }
        matches_snapshot(&db, &cx, grouped, &args, &groups);
        matches_snapshot(&db, &cx, NATIVE_COLLECT, &args, &global);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(3));
        edit.delete_vertex(VId(5));
        db.write(&commit, edit).await.unwrap();
        for (text, handle) in [(grouped, &groups), (NATIVE_COLLECT, &global)] {
            matches_snapshot(&db, &cx, text, &args, handle);
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            matches_snapshot(&db, &cx, text, &args, handle);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_collection_failures_under_zero_pages_fence_atomically_and_rebuild() {
    let ((), report) = run_async_under_lab(0x4c47_0107, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let input_error = "MATCH (n) RETURN collect(14/n.amount) AS values LIMIT 0";
        let output_error = "MATCH (n) RETURN [x IN collect(n.amount) | 14/x] AS values LIMIT 0";
        let inputs = db
            .register_standing_native(&cx, input_error, &args, symbols, policy())
            .unwrap();
        let outputs = db
            .register_standing_native(&cx, output_error, &args, symbols, policy())
            .unwrap();
        let healthy = db
            .register_standing_native(&cx, NATIVE_COLLECT, &args, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(0)));
        let durable = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(
            db.standing_native_query(&cx, &inputs, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert!(matches!(
            db.standing_native_query(&cx, &outputs, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::OutputExpression { .. },
                ..
            })
        ));
        for (text, handle) in [(input_error, &inputs), (output_error, &outputs)] {
            assert!(db.query(&cx, text, &args, symbols, policy()).is_err());
            assert!(
                db.register_standing_native(&cx, text, &args, symbols, policy())
                    .is_err()
            );
            assert!(db.rebuild_standing_query(&cx, handle, policy()).is_err());
        }
        matches_snapshot(&db, &cx, NATIVE_COLLECT, &args, &healthy);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(4)));
        db.write(&commit, edit).await.unwrap();
        for (text, handle) in [(input_error, &inputs), (output_error, &outputs)] {
            assert!(db.standing_native_query(&cx, handle, policy()).is_err());
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        // A delivery allowance cannot corrupt or fence the maintained result.
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &healthy,
                GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        matches_snapshot(&db, &cx, NATIVE_COLLECT, &args, &healthy);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_collection_parameters_and_replay_preserve_delayed_ordered_replacements() {
    let ((), report) = run_async_under_lab(0x4c47_0108, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let directory = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &directory, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n) RETURN collect(n.amount+$extra) AS values,collect(DISTINCT n.amount+$extra) AS unique";
        let old = GqlParameters::new().with_int64("extra", 3).unwrap();
        let current = GqlParameters::new().with_int64("extra", 17).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &old, symbols).unwrap();
        let first = prepared
            .register_standing(&mut db, &cx, &old, policy())
            .unwrap();
        let second = prepared
            .register_standing(&mut db, &cx, &current, policy())
            .unwrap();
        drop(prepared);
        let mut subscription = db.open_standing_subscription(&cx, &first).unwrap();
        subscription
            .enable_replay(&mut db, &cx, 8, 1000, 1_000_000, policy())
            .unwrap();
        let baseline = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut accepted = copy(baseline.rows());
        subscription.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for (id, value) in [(2, -4), (3, 11), (1, 2)] {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(id), N, Some(CanonicalScalar::Int(value)));
            let at = db.write(&commit, edit).await.unwrap();
            matches_snapshot(&db, &cx, text, &old, &first);
            matches_snapshot(&db, &cx, text, &current, &second);
            expected.push((at, db.standing_native_bag(&cx, &first, policy()).unwrap().1));
        }
        for (at, expected) in expected {
            let frame = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
            assert!(!frame.is_snapshot());
            assert_eq!(frame.frontier(), at);
            accepted
                .integrate(frame.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(accepted, expected);
            subscription.acknowledge(frame.receipt()).unwrap();
        }
        assert!(subscription.poll(&db, &cx, policy()).unwrap().is_none());
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &directory, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &first),
            Err(StandingQueryError::ForeignHandle)
        ));
        let reloaded = db
            .register_standing_native(&cx, text, &current, symbols, policy())
            .unwrap();
        matches_snapshot(&db, &cx, text, &current, &reloaded);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const NINE: &str = "MATCH (n) WITH [n.amount,n.amount] AS xs UNWIND xs AS x \
    RETURN COUNT(*) AS rows,COUNT(x) AS nonnull,COUNT(DISTINCT x) AS unique,\
    SUM(x) AS total,SUM(DISTINCT x) AS unique_sum,MIN(x) AS low,MAX(x) AS high,\
    AVG(x) AS average,AVG(DISTINCT x) AS unique_average";
fn queries() -> [&'static str; 5] {
    [
        NINE,
        "MATCH (n) WITH [n.amount,NULL] AS xs UNWIND xs AS x RETURN x AS k,COUNT(*) AS copies GROUP BY x HAVING copies > 0 ORDER BY copies DESC LIMIT 3",
        "MATCH (n) WITH [n.bucket,[n.amount]] AS k,n.amount AS x RETURN k,COUNT(*) AS copies,MAX(x) AS high GROUP BY k HAVING k IS NOT NULL ORDER BY copies DESC",
        "MATCH (n) WITH [n,n.amount,[n.amount]] AS xs UNWIND xs AS x RETURN COUNT(x) AS present,COUNT(DISTINCT x) AS kinds,MIN(x) AS low,MAX(x) AS high",
        "MATCH (n) WITH [n.amount,n.bucket] AS xs WITH xs[0] AS x RETURN SUM(x) AS total,AVG(x) AS average",
    ]
}

#[test]
fn list_keys_any_extrema_and_nine_numeric_aggregates_follow_commits_and_exact_deltas() {
    let ((), report) = run_async_under_lab(0x4c47_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let texts = queries();
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                db.register_standing_native(&cx, text, &args, symbols, policy())
                    .unwrap()
            })
            .collect();
        let QueryResult::Rows { rows, .. } = db
            .standing_native_query(&cx, &handles[0], policy())
            .unwrap()
            .1
        else {
            panic!("read returned a write")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_count(), Some(8));
        assert_eq!(rows[0][1].as_count(), Some(6));
        assert_eq!(rows[0][2].as_count(), Some(2));
        assert_eq!(rows[0][3].as_integer(), Some(32));
        assert_eq!(rows[0][4].as_integer(), Some(9));
        let average = rows[0][7].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (16, 3));
        let average = rows[0][8].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (9, 2));
        for handle in &handles {
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
        }
        let mut integrated: Vec<_> = handles
            .iter()
            .map(|h| copy(db.standing_query(&cx, h).unwrap().rows()))
            .collect();
        for step in 0..5 {
            for (text, handle) in texts.iter().zip(&handles) {
                matches_snapshot(&db, &cx, text, &args, handle);
            }
            let mut edit = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(-4)));
                }
                1 => {
                    edit.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(11)));
                }
                2 => {
                    edit.delete_vertex(VId(1));
                }
                3 => {
                    edit.set_vertex_property(VId(u128::MAX), B, Some(CanonicalScalar::Int(9)));
                }
                _ => {
                    edit.create_vertex(
                        VId(99),
                        vec![],
                        vec![(B, CanonicalScalar::Int(9)), (N, CanonicalScalar::Int(11))],
                    );
                }
            }
            db.write(&commit, edit).await.unwrap();
            for ((text, handle), rows) in texts.iter().zip(&handles).zip(&mut integrated) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                rows.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*rows, db.standing_query(&cx, handle).unwrap().rows());
                matches_snapshot(&db, &cx, text, &args, handle);
            }
        }
        db.compact(&commit).await.unwrap();
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        // Delivery refusal is not a maintenance failure.
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &handles[0],
                GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        matches_snapshot(&db, &cx, texts[0], &args, &handles[0]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_dynamic_numeric_failure_fences_only_its_view_and_rebuild_repairs_the_circuit() {
    let ((), report) = run_async_under_lab(0x4c47_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let healthy = "MATCH (n) WITH [n.amount] AS xs UNWIND xs AS x RETURN COUNT(DISTINCT x) AS kinds,MIN(x) AS low,MAX(x) AS high";
        let numeric = db
            .register_standing_native(&cx, NINE, &args, symbols, policy())
            .unwrap();
        let other = db
            .register_standing_native(&cx, healthy, &args, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(u128::MAX), N, Some(CanonicalScalar::Bool(true)));
        let durable = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(
            db.standing_native_query(&cx, &numeric, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::NonIntegerAggregate { .. },
                ..
            })
        ));
        assert!(db.query(&cx, NINE, &args, symbols, policy()).is_err());
        matches_snapshot(&db, &cx, healthy, &args, &other);
        assert!(db.rebuild_standing_query(&cx, &numeric, policy()).is_err());
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(u128::MAX), N, Some(CanonicalScalar::Int(13)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &numeric, policy()).is_err());
        db.rebuild_standing_query(&cx, &numeric, policy()).unwrap();
        assert!(db.standing_group_delta(&cx, &numeric).unwrap().is_none());
        matches_snapshot(&db, &cx, NINE, &args, &numeric);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        db.write(&commit, edit).await.unwrap();
        matches_snapshot(&db, &cx, NINE, &args, &numeric);
        matches_snapshot(&db, &cx, healthy, &args, &other);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_expansions_keep_global_nulls_and_zero_pages_never_hide_operand_errors() {
    let ((), report) = run_async_under_lab(0x4c47_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n) WITH [] AS xs UNWIND xs AS x RETURN SUM(x) AS total,AVG(x) AS average,COUNT(*) AS count",
            "MATCH (n) WITH [] AS xs UNWIND xs AS x RETURN x,COUNT(*) AS count GROUP BY x",
            "MATCH (n) WITH [NULL,NULL] AS xs UNWIND xs AS x RETURN SUM(DISTINCT x) AS total,AVG(DISTINCT x) AS average,COUNT(DISTINCT x) AS count",
        ] {
            let handle = db
                .register_standing_native(&cx, text, &args, symbols, policy())
                .unwrap();
            matches_snapshot(&db, &cx, text, &args, &handle);
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            matches_snapshot(&db, &cx, text, &args, &handle);
        }
        for tail in ["", " HAVING count < 0", " LIMIT 0"] {
            let text = format!(
                "MATCH (n) WITH [[n.amount]] AS xs UNWIND xs AS x RETURN COUNT(*) AS count,SUM(x) AS total{tail}"
            );
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            assert!(matches!(
                prepared.register_standing(&mut db, &cx, &args, policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::NonIntegerAggregate { .. }
                ))
            ));
            assert!(prepared.execute(&db, &cx, &args, policy()).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_pipeline_parameters_are_frozen_and_reopening_rebuilds_from_storage() {
    let ((), report) = run_async_under_lab(0x4c47_0104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let directory = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &directory, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n) WITH [n.amount,$extra] AS xs UNWIND xs AS x RETURN SUM(x) AS total,COUNT(DISTINCT x) AS kinds";
        let mut args = GqlParameters::new().with_int64("extra", 3).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let first = prepared
            .register_standing(&mut db, &cx, &args, policy())
            .unwrap();
        args = GqlParameters::new().with_int64("extra", 17).unwrap();
        let second = prepared
            .register_standing(&mut db, &cx, &args, policy())
            .unwrap();
        drop(prepared);
        let old = GqlParameters::new().with_int64("extra", 3).unwrap();
        assert_ne!(
            db.standing_native_query(&cx, &first, policy()).unwrap(),
            db.standing_native_query(&cx, &second, policy()).unwrap()
        );
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        db.write(&commit, edit).await.unwrap();
        matches_snapshot(&db, &cx, text, &old, &first);
        matches_snapshot(&db, &cx, text, &args, &second);
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &directory, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &first),
            Err(StandingQueryError::ForeignHandle)
        ));
        let current = db
            .register_standing_native(&cx, text, &args, symbols, policy())
            .unwrap();
        matches_snapshot(&db, &cx, text, &args, &current);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
