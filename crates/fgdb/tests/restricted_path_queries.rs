//! Vertex-restricted paths through real Chronicle/Strata reads and write programs.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphText, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 5_000_000, 5_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn query(mode: &str, bounds: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(
        &format!("MATCH {mode} (a)-[:R*{bounds}]->(b) WHERE a.p=1 RETURN ALL b"),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.values()[0].as_vertex().unwrap())
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for (id, a, b) in [(10, 1, 2), (11, 1, 2), (12, 2, 1), (13, 2, 3)] {
        batch.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn restricted_paths_read_staged_topology_and_pinned_history_through_reopen() {
    let ((), report) = run_async_under_lab(0xac1c_0001_u64, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let acyclic = query("ACYCLIC", "1..3");
        let simple = query("SIMPLE", "1..3");
        let old_acyclic = vec![VId(2), VId(2), VId(3), VId(3)];
        let old_simple = vec![VId(1), VId(1), VId(2), VId(2), VId(3), VId(3)];
        for (plan, expected) in [(&acyclic, &old_acyclic), (&simple, &old_simple)] {
            assert_eq!(
                &ids(&db
                    .execute_graph_pattern_governed(&cx, plan, policy())
                    .unwrap()
                    .value),
                expected
            );
            assert_eq!(
                &ids(&pinned
                    .execute_graph_pattern_governed(&cx, plan, policy())
                    .unwrap()
                    .value),
                expected
            );
        }
        let mut txn = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(11));
        changes.add_edge(EId(14), VId(3), VId(1), vec![]);
        txn.write(&mut db, changes).unwrap();
        let new_acyclic = vec![VId(2), VId(3)];
        let new_simple = vec![VId(1), VId(1), VId(2), VId(3)];
        for (plan, expected) in [(&acyclic, &new_acyclic), (&simple, &new_simple)] {
            assert_eq!(
                &ids(&txn
                    .execute_graph_pattern_governed(&db, &cx, plan, policy())
                    .unwrap()
                    .value),
                expected
            );
        }
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &simple, policy())
                .unwrap()
                .value),
            old_simple
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (plan, current, historic) in [
            (&acyclic, &new_acyclic, &old_acyclic),
            (&simple, &new_simple, &old_simple),
        ] {
            assert_eq!(
                &ids(&db
                    .execute_graph_pattern_governed(&cx, plan, policy())
                    .unwrap()
                    .value),
                current
            );
            assert_eq!(
                &ids(&db
                    .execute_graph_pattern_governed_at(&cx, plan, basis, policy())
                    .unwrap()
                    .value),
                historic
            );
            assert_eq!(
                &ids(&pinned
                    .execute_graph_pattern_governed(&cx, plan, policy())
                    .unwrap()
                    .value),
                historic
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_restricted_paths_keep_phantom_witnesses_without_extra_transaction_reads() {
    let ((), report) = run_async_under_lab(0xac1c_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (mode, bounds) in [("ACYCLIC", "1..8"), ("SIMPLE", "2..8")] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut initial = WriteBatch::new(R);
            initial.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
            initial.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
            initial.add_edge(EId(10), VId(1), VId(1), vec![]);
            db.write(&commit, initial).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(999), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            assert!(
                txn.execute_graph_pattern_governed(&db, &cx, &query(mode, bounds), policy())
                    .unwrap()
                    .value
                    .is_empty()
            );
            let mut concurrent = WriteBatch::new(R);
            concurrent.add_edge(EId(20), VId(1), VId(2), vec![]);
            concurrent.add_edge(EId(21), VId(2), VId(1), vec![]);
            db.write(&commit, concurrent).await.unwrap();
            let frontier = db.frontier().unwrap();
            // No additional read can repair a missing negative observation.
            assert!(matches!(
                txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(999)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn restricted_path_ingestion_rolls_back_a_late_action_failure_and_can_be_retried_explicitly() {
    let ((), report) = run_async_under_lab(0xac1c_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let script = PreparedGraphWriteScript::prepare(
            "MATCH ACYCLIC (a)-[:R*1..2]->(b) WHERE a.p=1 CREATE (n {p:$tag}); MATCH (n) WHERE n.p=$tag SET n.p=0",
            R, symbols).unwrap();
        let args = GqlParameters::new().with_int64("tag", 42).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(999), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let failed = txn.execute_graph_write_script_governed(
            &mut db,
            &cx,
            &script,
            &args,
            GraphWriteProgramPolicy::new(policy(), 3, 4, 0),
            |request| {
                assert_eq!(request.statement, 0);
                let fgdb_gql::insertion::GraphInsertRequest::Vertex { row, vertex: 0 } =
                    request.request
                else {
                    panic!("one vertex per selected path occurrence")
                };
                allocations += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(100 + row as u128)))
            },
        );
        assert!(failed.is_err());
        assert_eq!(allocations, 4);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        for id in 100..104 {
            assert!(txn.vertex(&db, VId(id)).unwrap().is_none());
        }
        assert!(txn.vertex(&db, VId(999)).unwrap().is_some());
        // External IDs issued by the failed call are not reclaimed or reused.
        let receipt = txn
            .execute_graph_write_script_governed(
                &mut db,
                &cx,
                &script,
                &args,
                GraphWriteProgramPolicy::new(policy(), 4, 4, 0),
                |request| {
                    let fgdb_gql::insertion::GraphInsertRequest::Vertex { row, vertex: 0 } =
                        request.request
                    else {
                        panic!("vertex allocation")
                    };
                    Ok::<_, ()>(ElementId::Vertex(VId(200 + row as u128)))
                },
            )
            .unwrap();
        assert_eq!(receipt.stats().created_vertices, 4);
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        for id in 200..204 {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(0))]
            );
        }
        assert!(db.vertex(VId(999)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_membership_and_result_phases_share_the_exact_database_allowance() {
    let ((), report) = run_async_under_lab(0xac1c_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for mode in ["ACYCLIC", "SIMPLE"] {
            let plan = query(mode, "1..3");
            let measured = db
                .execute_graph_pattern_governed(&cx, &plan, policy())
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
                    &plan,
                    GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
                )
                .unwrap(),
                measured
            );
            for dimension in 0..4 {
                let mut cap = caps;
                cap[dimension] -= 1;
                assert!(
                    db.execute_graph_pattern_governed(
                        &cx,
                        &plan,
                        GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3])
                    )
                    .is_err()
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn captured_query(mode: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(
        &format!("MATCH route = {mode} (a)-[:R*1..3]->(b) WHERE a.p=1 RETURN ALL route"),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn edge_routes(rows: &[GraphValueRow]) -> Vec<Vec<EId>> {
    rows.iter()
        .map(|row| {
            let path = row.get(0).unwrap().as_path().unwrap();
            assert_eq!(path.start(), VId(1));
            path.edges().collect()
        })
        .collect()
}

#[test]
fn captured_restricted_routes_keep_parallel_identities_and_pinned_generations() {
    let ((), report) = run_async_under_lab(0xac1c_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let acyclic = captured_query("ACYCLIC");
        let simple = captured_query("SIMPLE");
        let old_acyclic = vec![
            vec![EId(10)],
            vec![EId(10), EId(13)],
            vec![EId(11)],
            vec![EId(11), EId(13)],
        ];
        let old_simple = vec![
            vec![EId(10)],
            vec![EId(10), EId(12)],
            vec![EId(10), EId(13)],
            vec![EId(11)],
            vec![EId(11), EId(12)],
            vec![EId(11), EId(13)],
        ];
        let old_rows = db
            .execute_graph_pattern_governed(&cx, &simple, policy())
            .unwrap()
            .value;
        assert_eq!(edge_routes(&old_rows), old_simple);
        assert_eq!(
            edge_routes(
                &db.execute_graph_pattern_governed(&cx, &acyclic, policy())
                    .unwrap()
                    .value
            ),
            old_acyclic
        );
        let mut txn = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(11));
        changes.add_edge(EId(14), VId(3), VId(1), vec![]);
        txn.write(&mut db, changes).unwrap();
        let new_acyclic = vec![vec![EId(10)], vec![EId(10), EId(13)]];
        let new_simple = vec![
            vec![EId(10)],
            vec![EId(10), EId(12)],
            vec![EId(10), EId(13)],
            vec![EId(10), EId(13), EId(14)],
        ];
        for (plan, expected) in [(&acyclic, &new_acyclic), (&simple, &new_simple)] {
            assert_eq!(
                &edge_routes(
                    &txn.execute_graph_pattern_governed(&db, &cx, plan, policy())
                        .unwrap()
                        .value
                ),
                expected
            );
        }
        assert_eq!(
            pinned
                .execute_graph_pattern_governed(&cx, &simple, policy())
                .unwrap()
                .value,
            old_rows
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (plan, expected) in [(&acyclic, &new_acyclic), (&simple, &new_simple)] {
            assert_eq!(
                &edge_routes(
                    &db.execute_graph_pattern_governed(&cx, plan, policy())
                        .unwrap()
                        .value
                ),
                expected
            );
        }
        assert_eq!(
            db.execute_graph_pattern_governed_at(&cx, &simple, basis, policy())
                .unwrap()
                .value,
            old_rows
        );
        assert_eq!(
            pinned
                .execute_graph_pattern_governed(&cx, &simple, policy())
                .unwrap()
                .value,
            old_rows
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
