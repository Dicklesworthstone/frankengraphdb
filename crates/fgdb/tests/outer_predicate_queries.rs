//! Predicate-only outer correlations execute against real snapshots and transactions.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphText, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn joined() -> PreparedGraphPattern<GraphValueRow> {
    pattern("MATCH (a) WHERE a.q=1 OPTIONAL MATCH (b) WHERE b.p=a.p RETURN b")
}
fn ids(rows: &[GraphValueRow]) -> Vec<Option<VId>> {
    rows.iter().map(|row| row.values()[0].as_vertex()).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [
        (1, Some(CanonicalScalar::Int(7))),
        (2, Some(CanonicalScalar::Int(7))),
        (3, Some(CanonicalScalar::Int(9))),
        (4, Some(CanonicalScalar::Null)),
        (5, None),
    ] {
        let mut properties = Vec::new();
        if let Some(value) = value {
            properties.push((P, value));
        }
        properties.push((Q, CanonicalScalar::Int(id as i64)));
        batch.create_vertex(VId(id), vec![], properties);
    }
    db.write(cx, batch).await.unwrap();
}
fn set_p(id: VId, value: CanonicalScalar) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(id, P, Some(value));
    batch
}

#[test]
fn captured_property_joins_pin_history_through_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x0ca9_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let old = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let query = joined();
        let before = vec![Some(VId(1)), Some(VId(2))];
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value),
            before
        );
        db.write(&commit, set_p(VId(1), CanonicalScalar::Int(9)))
            .await
            .unwrap();
        let after = vec![Some(VId(1)), Some(VId(3))];
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value),
            after
        );
        assert_eq!(
            ids(&pinned
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value),
            before
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed_at(&cx, &query, old, wide())
                .unwrap()
                .value),
            before
        );
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value),
            after
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed_at(&cx, &query, old, wide())
                .unwrap()
                .value),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nullable_captures_and_staged_outer_values_read_the_canonical_overlay() {
    let ((), report) = run_async_under_lab(0x0ca9_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = pattern(
            "MATCH (a) WHERE a.q=1 OPTIONAL MATCH (a)-[:R]->(b) MATCH (c) WHERE b IS NULL OR c.p=b.p RETURN c",
        );
        let all = (1..=5).map(|id| Some(VId(id))).collect::<Vec<_>>();
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, wide())
                .unwrap()
                .value),
            all
        );
        let mut edge = WriteBatch::new(R);
        edge.add_edge(EId(10), VId(1), VId(2), vec![]);
        txn.write(&mut db, edge).unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, wide())
                .unwrap()
                .value),
            vec![Some(VId(1)), Some(VId(2))]
        );
        txn.write(&mut db, set_p(VId(2), CanonicalScalar::Int(9)))
            .unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, wide())
                .unwrap()
                .value),
            vec![Some(VId(2)), Some(VId(3))]
        );
        // A real vertex with a stored-null property is NOT a null vertex value.
        txn.write(&mut db, set_p(VId(2), CanonicalScalar::Null))
            .unwrap();
        assert!(
            txn.execute_graph_pattern_governed(&db, &cx, &query, wide())
                .unwrap()
                .value
                .is_empty()
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value),
            all
        );
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_successful_and_empty_outer_property_joins_retain_conflict_observations() {
    let ((), report) = run_async_under_lab(0x0ca9_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for initially_empty in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            if initially_empty {
                db.write(&commit, set_p(VId(1), CanonicalScalar::Int(9)))
                    .await
                    .unwrap();
            }
            let query = pattern("MATCH (a) WHERE a.q=1 MATCH (b) WHERE b.q=2 AND b.p=a.p RETURN b");
            let mut reader = db.begin(&txcx).unwrap();
            let rows = reader
                .execute_graph_pattern_governed(&db, &cx, &query, wide())
                .unwrap();
            assert_eq!(rows.value.len(), usize::from(!initially_empty));
            db.write(
                &commit,
                set_p(
                    VId(1),
                    CanonicalScalar::Int(if initially_empty { 7 } else { 9 }),
                ),
            )
            .await
            .unwrap();
            assert!(matches!(
                reader.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_join_creation_and_late_update_failure_restore_outer_work_without_reusing_ids() {
    let ((), report) = run_async_under_lab(0x0ca9_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let script = PreparedGraphWriteScript::prepare(
            "MATCH (a) WHERE a.q=1 MATCH (b) WHERE b.p=a.p CREATE (n {q:99,p:b.p}); MATCH (n) WHERE n.q=99 SET n.p=$new",
            R, symbols,
        ).unwrap();
        let args = GqlParameters::new().with_int64("new", 42).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(90), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let original = txn.staged_effect_digest().unwrap();
        let mut allocated = 0;
        let result = txn.execute_graph_write_script_governed(
            &mut db,
            &cx,
            &script,
            &args,
            GraphWriteProgramPolicy::new(wide(), 1, 2, 0),
            |_| {
                let id = VId(100 + allocated);
                allocated += 1;
                Ok::<_, ()>(ElementId::Vertex(id))
            },
        );
        assert!(result.is_err());
        assert_eq!(
            allocated, 2,
            "the late failure must happen after joined creation"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), original);
        assert!(txn.vertex(&db, VId(90)).unwrap().is_some());
        for id in [VId(100), VId(101)] {
            assert!(txn.vertex(&db, id).unwrap().is_none());
        }
        let mut next = 200;
        let receipt = txn
            .execute_graph_write_script_governed(
                &mut db,
                &cx,
                &script,
                &args,
                GraphWriteProgramPolicy::new(wide(), 2, 2, 0),
                |_| {
                    let id = VId(next);
                    next += 1;
                    Ok::<_, ()>(ElementId::Vertex(id))
                },
            )
            .unwrap();
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(
            receipt.steps()[0].created_vertices(),
            Some(&[VId(200), VId(201)][..])
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        for id in [VId(200), VId(201)] {
            let row = db.vertex(id).unwrap().unwrap();
            assert!(row.props.contains(&(P, CanonicalScalar::Int(42))));
            assert!(row.props.contains(&(Q, CanonicalScalar::Int(99))));
        }
        assert!(db.vertex(VId(90)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_joins_in_parameter_batches_skip_empty_records_and_share_creation_quota() {
    let ((), report) =
        run_async_under_lab(0x0ca9_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let frontier = db.frontier().unwrap();
            let script = PreparedGraphWriteScript::prepare(
            "MATCH (a) WHERE a.q=$owner MATCH (b) WHERE b.q=$peer AND b.p=a.p CREATE (n {q:900})",
            R, symbols,
        ).unwrap();
            let arguments = [2, 3, 2]
                .into_iter()
                .map(|peer| {
                    GqlParameters::new()
                        .with_int64("owner", 1)
                        .unwrap()
                        .with_int64("peer", peer)
                        .unwrap()
                })
                .collect::<Vec<_>>();
            let batch = script.bind_parameter_sets(&arguments).unwrap();
            let mut allocations = 0;
            let failed = db
                .execute_bound_graph_write_script_batch_autocommit_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &batch,
                    GraphWriteProgramPolicy::new(wide(), 0, 1, 0),
                    |_| {
                        allocations += 1;
                        Ok::<_, ()>(ElementId::Vertex(VId(300)))
                    },
                )
                .await;
            assert!(failed.is_err());
            assert_eq!(allocations, 1);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(300)).unwrap().is_none());
            let mut next = 400;
            let (receipt, completion) = db
                .execute_bound_graph_write_script_batch_autocommit_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &batch,
                    GraphWriteProgramPolicy::new(wide(), 0, 2, 0),
                    |_| {
                        let id = VId(next);
                        next += 1;
                        Ok::<_, ()>(ElementId::Vertex(id))
                    },
                )
                .await
                .unwrap();
            assert_eq!(receipt.stats().created_vertices, 2);
            assert_eq!(
                batch.record_receipts(&receipt, 0).unwrap()[0].created_vertices(),
                Some(&[VId(400)][..])
            );
            assert_eq!(
                batch.record_receipts(&receipt, 1).unwrap()[0].created_vertices(),
                Some(&[][..])
            );
            assert_eq!(
                batch.record_receipts(&receipt, 2).unwrap()[0].created_vertices(),
                Some(&[VId(401)][..])
            );
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_captures_share_exact_query_caps_without_publishing_partial_results() {
    let ((), report) = run_async_under_lab(0x0ca9_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = joined();
        let measured = db
            .execute_graph_pattern_governed(&cx, &query, wide())
            .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            db.execute_graph_pattern_governed(
                &cx,
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut short = caps;
            short[dimension] -= 1;
            assert!(
                db.execute_graph_pattern_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(short[0], short[1], short[2], short[3])
                )
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), frontier);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
