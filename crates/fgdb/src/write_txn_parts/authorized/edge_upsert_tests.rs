//! Conditional authorized relationship writes over the native MERGE and branch lowerer.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GqlScalarParameter, GraphEdgeMergePolicy,
    GraphEdgeMergeError, GraphEdgeUpsertAction, GraphEdgeUpsertBranch, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(9);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x92; 32], NS, [0x94; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9993), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn prepared(text: &str) -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(text, S, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn duplicate_input() -> PreparedGraphEdgeMerge {
    prepared("MATCH (a:Visible)-[:S]->(b:Visible) MERGE (a)-[:R]->(b)")
}
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool, matched: bool) {
    let mut batch = WriteBatch::new(S);
    for (id, value) in [(1, 10), (2, 20)] {
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        let mut labels = vec![L];
        if hidden {
            properties.push((SECRET, CanonicalScalar::Int(70 + value)));
            labels.push(HIDDEN);
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    for id in [11, 12] {
        batch.add_edge(EId(id), VId(1), VId(2), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if matched {
        let mut batch = WriteBatch::new(R);
        let mut properties = vec![(P, CanonicalScalar::Int(7))];
        if hidden { properties.push((SECRET, CanonicalScalar::Int(99))); }
        batch.add_edge(EId(13), VId(1), VId(2), properties);
        db.write(cx, batch).await.unwrap();
    }
    if hidden {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(10))]);
        batch.add_edge(EId(90), VId(1), VId(3), vec![]);
        batch.add_edge(EId(91), VId(3), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
        let mut batch = WriteBatch::new(H);
        batch.add_edge(EId(92), VId(1), VId(2), vec![]);
        batch.add_edge(EId(93), VId(2), VId(1), vec![]);
        db.write(cx, batch).await.unwrap();
    }
}


fn action(key: PropertyKeyId, value: i64) -> GraphEdgeUpsertAction {
    GraphEdgeUpsertAction {
        key,
        value: GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap(),
    }
}
fn conditional() -> PreparedGraphEdgeUpsert {
    PreparedGraphEdgeUpsert::prepare(
        duplicate_input(), vec![action(P, 11)], vec![action(P, 7)],
    ).unwrap()
}
fn upsert_policy() -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(policy(), 100)
}
fn upsert_authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphEdgeUpsertError::Staging(WriteTxnError::Authorization(error)))
        | GqlQueryError::Source(GraphEdgeUpsertError::Merge(GraphEdgeMergeError::Source(
            WriteTxnError::Authorization(error),
        ))) => error,
        other => panic!("expected upsert authorization error, got {other:?}"),
    }
}

#[test]
fn selected_upsert_branch_alone_executes_and_preserves_hidden_properties() {
    lab(0xa9b1, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap()
            .attenuate(Restriction::MaxRows(1)).unwrap();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, matched).await;
            let vertices = db.vertices().unwrap();
            let frontier = db.frontier().unwrap();
            // The unselected branch is deliberately forbidden. It is not an
            // attempted write, and must not be executed or charged as one.
            let (on_match, on_create) = if matched {
                (vec![action(P, 11)], vec![action(SECRET, 999)])
            } else {
                (vec![action(SECRET, 999)], vec![action(P, 7)])
            };
            let input = PreparedGraphEdgeUpsert::prepare(duplicate_input(), on_match, on_create).unwrap();
            let (stats, outcome, completion) = db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &input, upsert_policy(), || NOW,
            ).await.unwrap();
            assert_eq!(stats.branch, if matched { GraphEdgeUpsertBranch::Match } else { GraphEdgeUpsertBranch::Create });
            assert_eq!(stats.action_effects, 1);
            assert_eq!(stats.evaluator.work_units, stats.merge.evaluator.work_units + 2);
            assert_eq!(stats.evaluator.scratch_entries, stats.merge.evaluator.scratch_entries + 1);
            assert_eq!(outcome.created(), !matched);
            let props = db.edge(outcome.edge().unwrap()).unwrap().unwrap().props;
            if matched {
                assert_eq!(props, vec![(P, CanonicalScalar::Int(11)), (SECRET, CanonicalScalar::Int(99))]);
            } else {
                assert_eq!(props, vec![(P, CanonicalScalar::Int(7))]);
            }
            assert_eq!(db.vertices().unwrap(), vertices);
            let seq = db.frontier().unwrap();
            assert_eq!(seq.0, frontier.0 + 1);
            assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn refused_upsert_tail_rolls_back_creation_and_forbidden_equal_value_actions() {
    lab(0xa9b2, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, matched).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let forbidden = vec![action(P, 11), action(SECRET, 99)];
            let input = PreparedGraphEdgeUpsert::prepare(
                duplicate_input(), forbidden.clone(), forbidden,
            ).unwrap();
            let error = db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &input, upsert_policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(upsert_authorization(error), Error::ScopeDenied);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            let limited = GraphEdgeUpsertPolicy::new(policy(), 0);
            let error = db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &conditional(), limited, || NOW,
            ).await.unwrap_err();
            assert!(matches!(error, GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit { limit: 0, observed: 1 })));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn upsert_no_input_runs_neither_branch_but_still_requires_readwrite() {
    lab(0xa9b6, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, true).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
        let input = PreparedGraphEdgeUpsert::prepare(
            prepared("MATCH (a:Visible) WHERE a.p = 999 MERGE (a)-[:R]->(a)"),
            vec![action(SECRET, 9)], vec![action(SECRET, 10)],
        ).unwrap();
        for rights in [Rights::Read, Rights::Write, Rights::ReadWrite] {
            let mut scope = grant();
            scope.rights = rights;
            scope.limits.max_rows = 0;
            if rights != Rights::ReadWrite { scope.limits.max_work = 0; }
            let token = authority.issue_at(&scope, NOW).unwrap();
            let result = db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &input,
                GraphEdgeUpsertPolicy::new(policy().with_creation_limit(0), 0), || NOW,
            ).await;
            if rights == Rights::ReadWrite {
                let (stats, outcome, completion) = result.unwrap();
                assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
                assert_eq!(stats.branch, GraphEdgeUpsertBranch::NoInput);
                assert_eq!(stats.action_effects, 0);
                assert_eq!(stats.evaluator.work_units, stats.merge.evaluator.work_units + 1);
                assert_eq!(stats.evaluator.scratch_entries, stats.merge.evaluator.scratch_entries);
                assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
            } else {
                assert_eq!(upsert_authorization(result.unwrap_err()), Error::PermissionDenied);
            }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn upsert_native_resource_caps_cover_selection_merge_and_actions_cumulatively() {
    lab(0xa9b7, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false, matched).await;
            let (stats, _, _) = db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &conditional(), upsert_policy(), || NOW,
            ).await.unwrap();
            let records = stats.merge.match_selection.snapshot_records + stats.merge.overlay_edges;
            for dimension in 0..3 {
                for below in [false, true] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit, false, matched).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                    let reduce = u64::from(below);
                    let policy = GraphEdgeUpsertPolicy::new(GraphEdgeMergePolicy::new(
                        GqlQueryPolicy::new(
                            records - if dimension == 0 { reduce } else { 0 },
                            stats.merge.match_selection.result_rows,
                            stats.evaluator.work_units - if dimension == 1 { reduce } else { 0 },
                            stats.evaluator.scratch_entries - if dimension == 2 { reduce } else { 0 },
                        ),
                    ), 1);
                    let result = db.execute_graph_edge_upsert_authorized(
                        &txn, &query, &commit, &authority, &token, "main", &conditional(), policy, || NOW,
                    ).await;
                    if below {
                        assert!(matches!(result, Err(GqlQueryError::Rows(_)) | Err(GqlQueryError::Evaluator(_))));
                        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                    } else {
                        assert_eq!(result.unwrap().0, stats);
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        }
    });
}

#[test]
fn every_upsert_expiry_boundary_discards_created_and_matched_action_prefixes() {
    lab(0xa9b8, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, matched).await;
            let calls = AtomicU64::new(0);
            db.execute_graph_edge_upsert_authorized(
                &txn, &query, &commit, &authority, &token, "main", &conditional(), upsert_policy(), || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    NOW
                },
            ).await.unwrap();
            let total = calls.load(Ordering::Relaxed);
            for cutoff in 0..total {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, true, matched).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                let calls = AtomicU64::new(0);
                let error = db.execute_graph_edge_upsert_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &conditional(), upsert_policy(), || {
                        if calls.fetch_add(1, Ordering::Relaxed) < cutoff { NOW } else { 10_000 }
                    },
                ).await.unwrap_err();
                assert_eq!(upsert_authorization(error), Error::Expired);
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}
