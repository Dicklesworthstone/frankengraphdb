//! ANY shortest queries use the real snapshot, overlay and commit mechanisms.

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
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
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
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn query(selector: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(
        &format!(
            "MATCH {selector} SHORTEST WALK (a)-[:R*1..2]->(b) WHERE a.p=$source RETURN ALL b"
        ),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new().with_int64("source", 1).unwrap())
    .unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.values()[0].as_vertex().unwrap())
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for vertex in [1_u128, 2, 3, 4, 9] {
        batch.create_vertex(
            VId(vertex),
            vec![],
            vec![(P, CanonicalScalar::Int(vertex as i64))],
        );
    }
    for (index, (source, destination)) in [(1, 2), (1, 2), (2, 3), (2, 3), (3, 4)]
        .into_iter()
        .enumerate()
    {
        batch.add_edge(
            EId(10 + index as u128),
            VId(source),
            VId(destination),
            vec![],
        );
    }
    db.write(cx, batch).await.unwrap();
}
fn shortcut() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(EId(50), VId(1), VId(3), vec![]);
    batch
}

#[test]
fn any_pairs_obey_pinned_history_and_exact_caps_through_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xa117_0001, |root| async move {
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
        let query = query("ANY");
        let measured = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(ids(&measured.value), vec![VId(2), VId(3)]);
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
            assert_eq!(db.frontier().unwrap(), old);
        }
        db.write(&commit, shortcut()).await.unwrap();
        let new = db.frontier().unwrap();
        // The shortcut admits vertex 4 within the two-hop upper bound. Comparing
        // only broad reachability would not detect a historical-topology leak.
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3), VId(4)]
        );
        assert_eq!(
            ids(&pinned
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3)]
        );
        assert!(
            pinned
                .execute_graph_pattern_governed_at(&cx, &query, new, policy())
                .is_err()
        );
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed_at(&cx, &query, old, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3)]
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed_at(&cx, &query, new, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3), VId(4)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_topology_changes_affect_any_search_without_publishing() {
    let ((), report) = run_async_under_lab(0xa117_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = query("ANY");
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3)]
        );
        txn.write(&mut db, shortcut()).unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3), VId(4)]
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3)]
        );
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(50));
        txn.write(&mut db, remove).unwrap();
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &query, policy())
                .unwrap()
                .value),
            vec![VId(2), VId(3)]
        );
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(50)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn any_selected_ingestion_creates_per_pair_and_late_refusal_restores_outer_work() {
    let ((), report) = run_async_under_lab(0xa117_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &query("ALL"), policy())
                .unwrap()
                .value),
            vec![VId(2), VId(2), VId(3), VId(3), VId(3), VId(3)]
        );
        let script = PreparedGraphWriteScript::prepare(
            "CREATE (seed {q:0}); MATCH ANY SHORTEST WALK (a)-[:R*1..2]->(b) WHERE a.p=$source CREATE (n {q:$tag})",
            R, symbols,
        ).unwrap();
        let args = GqlParameters::new()
            .with_int64("source", 1)
            .unwrap()
            .with_int64("tag", 7)
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let failed = txn.execute_graph_write_script_governed(
            &mut db,
            &cx,
            &script,
            &args,
            GraphWriteProgramPolicy::new(policy(), 0, 2, 0),
            |request| {
                allocations += 1;
                assert_eq!(
                    request.statement, 0,
                    "the second step must refuse before allocation"
                );
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            },
        );
        assert!(failed.is_err());
        assert_eq!(allocations, 1);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.vertex(&db, VId(100)).unwrap().is_none());
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        let receipt = txn
            .execute_graph_write_script_governed(
                &mut db,
                &cx,
                &script,
                &args,
                GraphWriteProgramPolicy::new(policy(), 0, 3, 0),
                |request| {
                    let fgdb_gql::insertion::GraphInsertRequest::Vertex { row, vertex: 0 } =
                        request.request
                    else {
                        panic!("one vertex per selected endpoint pair")
                    };
                    let id = if request.statement == 0 {
                        200
                    } else {
                        201 + row as u128
                    };
                    Ok::<_, ()>(ElementId::Vertex(VId(id)))
                },
            )
            .unwrap();
        assert_eq!(receipt.stats().created_vertices, 3);
        assert_eq!(
            receipt.steps()[1].created_vertices(),
            Some(&[VId(201), VId(202)][..])
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        for vertex in [VId(201), VId(202)] {
            assert_eq!(
                db.vertex(vertex).unwrap().unwrap().props,
                vec![(Q, CanonicalScalar::Int(7))]
            );
        }
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn concurrent_shortcut_invalidates_any_search_read_close() {
    let ((), report) = run_async_under_lab(0xa117_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut reader = db.begin(&txcx).unwrap();
        let rows = reader
            .execute_graph_pattern_governed(&db, &cx, &query("ANY"), policy())
            .unwrap();
        assert_eq!(ids(&rows.value), vec![VId(2), VId(3)]);
        db.write(&commit, shortcut()).await.unwrap();
        assert!(matches!(
            reader.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_any_search_coalesces_parallel_cycles_at_the_maximum_hop_bound() {
    let ((), report) = run_async_under_lab(0xa117_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![]);
        for id in 10..18 {
            batch.add_edge(EId(id), VId(1), VId(1), vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let frontier = db.frontier().unwrap();
        let prepare = |selector| {
            PreparedGraphText::prepare(
                &format!("MATCH {selector} SHORTEST WALK (a)-[:R*1024]->(b) RETURN ALL b"),
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
        };
        let cap = GqlQueryPolicy::new(100, 10, 50_000, 10_000);
        let rows = db
            .execute_graph_pattern_governed(&cx, &prepare("ANY"), cap)
            .unwrap();
        assert_eq!(ids(&rows.value), vec![VId(1)]);
        assert!(
            db.execute_graph_pattern_governed(&cx, &prepare("ALL"), cap)
                .is_err(),
            "ALL cannot silently inherit ANY's occurrence coalescing"
        );
        assert_eq!(db.frontier().unwrap(), frontier);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
