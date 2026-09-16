//! Branch-specific directed relationship MERGE property actions.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphEdgeMergeOutcome,
    GraphEdgeMergePolicy, GraphEdgeMergeRequest, GraphEdgeUpsertAction, GraphEdgeUpsertBranch,
    GraphEdgeUpsertError, GraphEdgeUpsertPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMerge, PreparedGraphEdgeUpsert, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x11; 32], DatabaseSecurityNamespaceId([0x12; 32]), [0x13; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn scalar(value: i64) -> GqlScalarParameter {
    GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap()
}
fn upsert(source: i64, destination: i64) -> PreparedGraphEdgeUpsert {
    let text = format!("MATCH (a),(b) WHERE a.p={source} AND b.p={destination} RETURN ALL a,b");
    let selection = PreparedGraphText::prepare(&text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let merge = PreparedGraphEdgeMerge::prepare(selection, R, 0, 1, vec![]).unwrap();
    PreparedGraphEdgeUpsert::prepare(
        merge,
        vec![GraphEdgeUpsertAction { key: W, value: scalar(100) }],
        vec![GraphEdgeUpsertAction { key: W, value: scalar(200) }],
    ).unwrap()
}
fn policy(max_actions: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(20_000, 20_000, 3_000_000, 3_000_000)),
        max_actions,
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3_u128 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![(W, CanonicalScalar::Int(1))]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn on_match_updates_existing_edge_without_allocating() {
    let ((), report) = run_async_under_lab(0xed9e_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn.execute_graph_edge_upsert_governed(
            &mut db, &query, &upsert(1, 2), policy(1),
            |_| -> Result<ElementId, ()> { panic!("ON MATCH must not allocate") },
        ).unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::Match);
        assert_eq!(stats.action_effects, 1);
        assert_eq!(txn.edge(&db, EId(10)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(100))]);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.edge(EId(10)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(100))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn on_create_decorates_new_edge_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0xed9e_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn.execute_graph_edge_upsert_governed(
            &mut db, &query, &upsert(2, 3), policy(1),
            |request| { assert_eq!(request, GraphEdgeMergeRequest); Ok::<_, ()>(ElementId::Edge(EId(20))) },
        ).unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(20)));
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::Create);
        assert_eq!(txn.edge(&db, EId(20)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(200))]);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.edge(EId(20)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(200))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_input_runs_no_branch_and_finishes_read_only() {
    let ((), report) = run_async_under_lab(0xed9e_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let (stats, outcome, completion) = db.execute_graph_edge_upsert_autocommit_governed(
            &txcx, &query, &commit, &upsert(99, 3), policy(1),
            |_| -> Result<ElementId, ()> { panic!("NoInput must not allocate") },
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::NoInput);
        assert_eq!(stats.action_effects, 0);
        assert!(matches!(completion, fgdb_types::EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_branch_action_limit_rolls_back_created_edge() {
    let ((), report) = run_async_under_lab(0xed9e_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result = txn.execute_graph_edge_upsert_governed(
            &mut db, &query, &upsert(2, 3), policy(0),
            |_| Ok::<_, ()>(ElementId::Edge(EId(20))),
        );
        assert!(matches!(result,
            Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit { limit: 0, observed: 1 }))));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.edge(&db, EId(20)).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_autocommit_creates_then_updates_with_no_second_identity() {
    let ((), report) = run_async_under_lab(0xed9e_3101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let template = fgdb_gql::PreparedGraphEdgeUpsertText::prepare(
            "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[e:R]->(b) ON CREATE SET e.w=$fresh ON MATCH SET e.w=$seen",
            R, |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
                (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
                _ => symbols(kind, name),
            },
        ).unwrap();
        let args = GqlParameters::new().with_int64("left", 2).unwrap().with_int64("right", 3).unwrap()
            .with_int64("fresh", 200).unwrap().with_int64("seen", 100).unwrap();
        let prepared = template.bind_parameters(&args).unwrap();
        let (created, outcome, completion) = db.execute_graph_edge_upsert_autocommit_governed(
            &txcx, &query, &commit, &prepared, policy(1),
            |_| Ok::<_, ()>(ElementId::Edge(EId(20))),
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(20)));
        assert!(matches!(completion, fgdb_types::EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(created.evaluator.work_units, created.merge.evaluator.work_units + 2);
        assert_eq!(created.evaluator.scratch_entries, created.merge.evaluator.scratch_entries + 1);
        assert_eq!(db.edge(EId(20)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(200))]);

        let mut no_creations = policy(1);
        no_creations.merge.max_created_edges = 0;
        let (_, outcome, completion) = db.execute_graph_edge_upsert_autocommit_governed(
            &txcx, &query, &commit, &prepared, no_creations,
            |_| -> Result<ElementId, ()> { panic!("existing relationship must not allocate") },
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(20)));
        assert!(matches!(completion, fgdb_types::EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.edge(EId(20)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(100))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_branch_work_and_scratch_limits_accept_and_one_below_rolls_back() {
    let ((), report) = run_async_under_lab(0xed9e_3102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let prepared = upsert(2, 3);
        let mut reference = db.begin(&txcx).unwrap();
        let (expected, _) = reference.execute_graph_edge_upsert_governed(
            &mut db, &query, &prepared, policy(1),
            |_| Ok::<_, ()>(ElementId::Edge(EId(30))),
        ).unwrap();
        reference.abort();
        for mode in 0..3 {
            let mut limits = policy(1);
            limits.merge.query = GqlQueryPolicy::new(
                expected.merge.match_selection.snapshot_records + expected.merge.overlay_edges,
                expected.merge.match_selection.result_rows,
                expected.evaluator.work_units - u64::from(mode == 0),
                expected.evaluator.scratch_entries - u64::from(mode == 1),
            );
            let mut txn = db.begin(&txcx).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let eid = EId(40 + mode as u128);
            let result = txn.execute_graph_edge_upsert_governed(
                &mut db, &query, &prepared, limits,
                |_| Ok::<_, ()>(ElementId::Edge(eid)),
            );
            if mode < 2 {
                let Err(GqlQueryError::Evaluator(error)) = result else {
                    panic!("expected branch resource refusal for mode {mode}");
                };
                assert_eq!(error.dimension, if mode == 0 {
                    fgdb_gql::GlaLimitDimension::WorkUnits
                } else { fgdb_gql::GlaLimitDimension::ScratchEntries });
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                assert!(txn.edge(&db, eid).unwrap().is_none());
            } else {
                assert_eq!(result.unwrap().0, expected);
                assert_eq!(txn.edge(&db, eid).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(200))]);
            }
            txn.abort();
        }
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_action_keeps_outer_prefix_and_negative_relationship_witness() {
    let ((), report) = run_async_under_lab(0xed9e_3103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(9), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let allocations = std::cell::Cell::new(0);
        assert!(txn.execute_graph_edge_upsert_governed(
            &mut db, &query, &upsert(2, 3), policy(0), |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Edge(EId(20)))
            },
        ).is_err());
        assert_eq!(allocations.get(), 1, "rollback does not rewind the allocator");
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(9)).unwrap().is_some());
        assert!(txn.edge(&db, EId(20)).unwrap().is_none());
        let mut concurrent = WriteBatch::new(R);
        concurrent.add_edge(EId(21), VId(2), VId(3), vec![]);
        db.write(&commit, concurrent).await.unwrap();
        assert!(txn.finish(&mut db, &commit).await.is_err(), "failed MERGE still observed absence");
        assert!(db.vertex(VId(9)).unwrap().is_none());
        assert!(db.edge(EId(21)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn inactive_create_actions_need_no_quota_and_matching_can_remain_read_only() {
    let ((), report) = run_async_under_lab(0xed9e_3104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let base = upsert(1, 2);
        let prepared = PreparedGraphEdgeUpsert::prepare(base.merge().clone(), vec![], base.on_create().to_vec()).unwrap();
        let mut limits = policy(0);
        limits.merge.max_created_edges = 0;
        let before = db.frontier().unwrap();
        let (stats, outcome, completion) = db.execute_graph_edge_upsert_autocommit_governed(
            &txcx, &query, &commit, &prepared, limits,
            |_| -> Result<ElementId, ()> { panic!("matched branch cannot allocate") },
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert_eq!(stats.action_effects, 0);
        assert!(matches!(completion, fgdb_types::EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.edge(EId(10)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(1))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
