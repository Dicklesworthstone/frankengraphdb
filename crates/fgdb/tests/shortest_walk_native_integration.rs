//! Shortest queries traverse real durable snapshots and canonical transaction overlays.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy, PreparedGraphText, PreparedGraphWriteScript};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId,
    EmbeddedTxnCompletion, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000) }
fn query() -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(
        "MATCH ALL SHORTEST WALK (a)-[:R*1..4]->(b) WHERE a.p=1 AND b.p=4 RETURN ALL b",
        symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for vertex in 1..=5_u128 {
        batch.create_vertex(VId(vertex), vec![], vec![(P, CanonicalScalar::Int(vertex as i64))]);
    }
    // Three tied shortest routes to 4: two through 2, one through 3. The
    // self-loop contributes longer walks but no additional shortest route.
    for (index, (source, destination)) in [(1, 2), (1, 2), (2, 4), (1, 3), (3, 4), (4, 4)]
        .into_iter().enumerate() {
        batch.add_edge(EId(10 + index as u128), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap();
}
fn shortcut() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(EId(50), VId(1), VId(4), vec![]);
    batch
}

#[test]
fn shortest_ties_survive_pinned_history_compaction_and_authoritative_reopen() {
    let ((), report) = run_async_under_lab(0x5a07_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let old = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let query = query();
        let before = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        assert_eq!(ids(&before.value), vec![VId(4); 3]);
        db.write(&commit, shortcut()).await.unwrap();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(4)]);
        assert_eq!(ids(&pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(4); 3]);
        assert_eq!(ids(&db.execute_graph_pattern_governed_at(&cx, &query, old, policy()).unwrap().value), vec![VId(4); 3]);
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(4)]);
        assert_eq!(ids(&db.execute_graph_pattern_governed_at(&cx, &query, old, policy()).unwrap().value), vec![VId(4); 3]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_shortcuts_and_deletions_change_search_without_publishing() {
    let ((), report) = run_async_under_lab(0x5a07_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = query();
        let mut txn = db.begin(&txcx).unwrap();
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap().value), vec![VId(4); 3]);
        txn.write(&mut db, shortcut()).unwrap();
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap().value), vec![VId(4)]);
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(4); 3]);
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(50));
        txn.write(&mut db, remove).unwrap();
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap().value), vec![VId(4); 3]);
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(50)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn concurrent_shorter_route_invalidates_the_existing_transaction_read_witness() {
    let ((), report) = run_async_under_lab(0x5a07_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut reader = db.begin(&txcx).unwrap();
        let rows = reader.execute_graph_pattern_governed(&mut db, &cx, &query(), policy()).unwrap();
        assert_eq!(ids(&rows.value), vec![VId(4); 3]);
        db.write(&commit, shortcut()).await.unwrap();
        assert!(matches!(reader.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_shortest_match_feeds_atomic_creation_with_tie_multiplicity_and_rollback() {
    let ((), report) = run_async_under_lab(0x5a07_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let script = PreparedGraphWriteScript::prepare(
            "MATCH ALL SHORTEST WALK (a)-[:R*1..4]->(b) WHERE a.p=1 AND b.p=4 CREATE (n {q:$tag})",
            R, symbols,
        ).unwrap();
        let args = GqlParameters::new().with_int64("tag", 7).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let result = txn.execute_graph_write_script_governed(
            &mut db, &cx, &script, &args, GraphWriteProgramPolicy::new(policy(), 0, 2, 0), |_| {
                allocations += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            },
        );
        assert!(result.is_err());
        assert_eq!(allocations, 0, "three shortest occurrences exceed the cap before any allocation");
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        let receipt = txn.execute_graph_write_script_governed(
            &mut db, &cx, &script, &args, GraphWriteProgramPolicy::new(policy(), 0, 3, 0), |request| {
                let fgdb_gql::insertion::GraphInsertRequest::Vertex { row, vertex: 0 } = request.request
                    else { panic!("one vertex per selected shortest occurrence") };
                assert_eq!(request.statement, 0);
                allocations += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(100 + row as u128)))
            },
        ).unwrap();
        assert_eq!(allocations, 3);
        assert_eq!(receipt.stats().created_vertices, 3);
        assert_eq!(receipt.steps()[0].created_vertices(), Some(&[VId(100), VId(101), VId(102)][..]));
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        for vertex in [VId(100), VId(101), VId(102)] {
            assert_eq!(db.vertex(vertex).unwrap().unwrap().props, vec![(Q, CanonicalScalar::Int(7))]);
        }
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_shortest_queries_respect_source_result_work_and_scratch_caps() {
    let ((), report) = run_async_under_lab(0x5a07_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = query();
        let measured = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries];
        assert_eq!(ids(&measured.value), vec![VId(4); 3]);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])).unwrap(), measured);
        for dimension in 0..4 {
            let mut limited = caps; limited[dimension] -= 1;
            assert!(db.execute_graph_pattern_governed(&cx, &query,
                GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3])).is_err(),
                "dimension {dimension} cannot release a partial result");
            assert_eq!(db.frontier().unwrap(), frontier);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
