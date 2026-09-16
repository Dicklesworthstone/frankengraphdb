//! Ordered MATCH chains use retained storage, canonical overlays and real commits.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy, GraphWriteScriptExecutionError,
    PreparedGraphText, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn joined() -> PreparedGraphPattern<GraphValueRow> {
    prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) RETURN ALL c")
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut left = WriteBatch::new(R);
    for vertex in 1..=5_u128 {
        left.create_vertex(VId(vertex), vec![], vec![(P, CanonicalScalar::Int(vertex as i64))]);
    }
    left.add_edge(EId(11), VId(1), VId(2), vec![]);
    left.add_edge(EId(12), VId(1), VId(2), vec![]);
    left.add_edge(EId(13), VId(4), VId(5), vec![]);
    db.write(cx, left).await.unwrap();
    let mut right = WriteBatch::new(S);
    right.add_edge(EId(21), VId(2), VId(3), vec![]);
    right.add_edge(EId(22), VId(2), VId(3), vec![]);
    db.write(cx, right).await.unwrap();
}
fn complete_second_chain() -> WriteBatch {
    let mut batch = WriteBatch::new(S);
    batch.add_edge(EId(23), VId(5), VId(3), vec![]);
    batch
}

#[test]
fn ordered_join_bags_follow_pinned_history_through_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x4d41_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let first = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let query = joined();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(3); 4]);
        let second = db.write(&commit, complete_second_chain()).await.unwrap();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(3); 5]);
        assert_eq!(ids(&pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(3); 4]);
        assert!(pinned.execute_graph_pattern_governed_at(&cx, &query, second, policy()).is_err());
        let mut remove = WriteBatch::new(S);
        remove.delete_edge(EId(21));
        let third = db.write(&commit, remove).await.unwrap();
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(3); 3]);
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for (seq, count) in [(first, 4), (second, 5), (third, 3)] {
            assert_eq!(ids(&db.execute_graph_pattern_governed_at(&cx, &query, seq, policy()).unwrap().value),
                vec![VId(3); count], "historical join changed at {seq:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn required_match_observes_staged_relationship_creation_and_deletion_without_publication() {
    let ((), report) = run_async_under_lab(0x4d41_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = joined();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, complete_second_chain()).unwrap();
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap().value), vec![VId(3); 5]);
        assert_eq!(ids(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), vec![VId(3); 4]);
        let mut remove = WriteBatch::new(S);
        remove.delete_edge(EId(23));
        txn.write(&mut db, remove).unwrap();
        assert_eq!(ids(&txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap().value), vec![VId(3); 4]);
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(23)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_new_required_witness_conflicts_even_when_the_reader_returned_no_rows() {
    let ((), report) = run_async_under_lab(0x4d41_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for empty in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let query = if empty {
                prepare("MATCH (a) WHERE a.p=4 OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) RETURN c")
            } else { joined() };
            let mut txn = db.begin(&txcx).unwrap();
            let rows = txn.execute_graph_pattern_governed(&mut db, &cx, &query, policy()).unwrap();
            assert_eq!(rows.value.len(), if empty { 0 } else { 4 });
            db.write(&commit, complete_second_chain()).await.unwrap();
            assert!(matches!(txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn joined_creation_and_later_updates_share_one_rollback_guard_and_one_commit() {
    let ((), report) = run_async_under_lab(0x4d41_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let script = PreparedGraphWriteScript::prepare(
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) CREATE (n {q:$tag}); MATCH (n) WHERE n.q=$tag SET n.p=$tag",
            R, symbols,
        ).unwrap();
        let args = GqlParameters::new().with_int64("tag", 77).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocated = 0;
        let failed = txn.execute_graph_write_script_governed(&mut db, &cx, &script, &args,
            GraphWriteProgramPolicy::new(policy(), 3, 4, 0), |_| {
                let vid = VId(100 + allocated);
                allocated += 1;
                Ok::<_, ()>(ElementId::Vertex(vid))
            });
        assert!(failed.is_err(), "four update targets exceed the shared effect limit");
        assert_eq!(allocated, 4, "the first joined creation step must actually have run");
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        for vid in 100..104 { assert!(txn.vertex(&db, VId(vid)).unwrap().is_none()); }
        let mut created = 0;
        let receipt = txn.execute_graph_write_script_governed(&mut db, &cx, &script, &args,
            GraphWriteProgramPolicy::new(policy(), 4, 4, 0), |request| {
                assert_eq!(request.statement, 0);
                let vid = VId(200 + created);
                created += 1;
                Ok::<_, ()>(ElementId::Vertex(vid))
            }).unwrap();
        assert_eq!(created, 4);
        assert_eq!(receipt.stats().created_vertices, 4);
        assert_eq!(receipt.stats().mutation_effects, 4);
        assert_eq!(receipt.steps()[0].created_vertices(), Some(&[VId(200), VId(201), VId(202), VId(203)][..]));
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        for vid in 200..204 {
            let row = db.vertex(VId(vid)).unwrap().unwrap();
            assert_eq!(row.props, vec![(P, CanonicalScalar::Int(77)), (Q, CanonicalScalar::Int(77))]);
        }
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameter_batches_continue_after_an_empty_join_and_keep_global_creation_limits() {
    let ((), report) = run_async_under_lab(0x4d41_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let script = PreparedGraphWriteScript::prepare(
            "MATCH (a) WHERE a.p=$source OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) CREATE (n {q:$tag})",
            R, symbols,
        ).unwrap();
        let args = [(1, 7), (4, 8), (1, 9)].into_iter().map(|(source, tag)|
            GqlParameters::new().with_int64("source", source).unwrap().with_int64("tag", tag).unwrap()
        ).collect::<Vec<_>>();
        let batch = script.bind_parameter_sets(&args).unwrap();
        let mut issued = 0;
        let error = db.execute_bound_graph_write_script_batch_autocommit_governed(
            &txcx, &cx, &commit, &batch, GraphWriteProgramPolicy::new(policy(), 0, 7, 0), |_| {
                let vid = VId(100 + issued);
                issued += 1;
                Ok::<_, ()>(ElementId::Vertex(vid))
            },
        ).await.unwrap_err();
        match error {
            GraphWriteScriptExecutionError::BatchProgram { location: Some(location), .. } => {
                assert_eq!(location.argument_set, 2);
                assert_eq!(location.statement, 0);
            }
            other => panic!("wrong joined batch error: {other:?}"),
        }
        assert_eq!(issued, 4);
        assert_eq!(db.frontier().unwrap(), frontier);
        for vid in 100..104 { assert!(db.vertex(VId(vid)).unwrap().is_none()); }
        assert_eq!(txcx.outstanding_obligations(), 0);
        let mut issued = 0;
        let (receipt, completion) = db.execute_bound_graph_write_script_batch_autocommit_governed(
            &txcx, &cx, &commit, &batch, GraphWriteProgramPolicy::new(policy(), 0, 8, 0), |request| {
                assert_eq!(request.statement, if issued < 4 { 0 } else { 2 });
                let vid = VId(200 + issued);
                issued += 1;
                Ok::<_, ()>(ElementId::Vertex(vid))
            },
        ).await.unwrap();
        assert_eq!(issued, 8);
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_vertices, 8);
        assert!(batch.record_receipts(&receipt, 1).unwrap()[0].created_vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        for vid in 200..208 {
            let tag = if vid < 204 { 7 } else { 9 };
            assert_eq!(db.vertex(VId(vid)).unwrap().unwrap().props, vec![(Q, CanonicalScalar::Int(tag))]);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn the_entire_join_uses_one_source_result_work_and_scratch_allowance() {
    let ((), report) = run_async_under_lab(0x4d41_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let frontier = db.frontier().unwrap();
        let query = joined();
        let measured = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries];
        assert_eq!(measured.value.len(), 4);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])).unwrap(), measured);
        for dimension in 0..4 {
            let mut limited = caps;
            limited[dimension] -= 1;
            assert!(db.execute_graph_pattern_governed(&cx, &query,
                GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3])).is_err());
            assert_eq!(db.frontier().unwrap(), frontier);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
