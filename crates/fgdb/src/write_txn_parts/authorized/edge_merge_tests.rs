//! Real directed MERGE through capabilities, the canonical overlay and Chronicle.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GqlScalarParameter, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(9);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x90; 32], NS, [0x92; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9991), NS, "graph", SchemaEpoch(1), 1).unwrap()
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
        (GraphSymbolKind::Relation, "Hidden") => Some(GraphSymbol::Relation(H)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(text: &str) -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn merge() -> PreparedGraphEdgeMerge {
    definition("MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[:S]->(b)")
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphEdgeMergeError::Source(WriteTxnError::Authorization(error)))
        | GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(WriteTxnError::Authorization(error))) => error,
        other => panic!("expected authorization refusal, got {other:?}"),
    }
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
    let mut batch = WriteBatch::new(R);
    for (id, p) in [(1, 10), (2, 20)] {
        let mut props = vec![(P, CanonicalScalar::Int(p))];
        if hidden { props.push((SECRET, CanonicalScalar::Int(99))); }
        batch.create_vertex(VId(id), vec![L], props);
    }
    for id in [11, 12] {
        batch.add_edge(EId(id), VId(1), VId(2), vec![]);
    }
    if hidden {
        for id in 3..8 {
            batch.create_vertex(VId(id), vec![HIDDEN], vec![]);
            batch.add_edge(EId(30 + id), VId(1), VId(id), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if matched {
        let mut batch = WriteBatch::new(S);
        batch.add_edge(EId(21), VId(1), VId(2),
            if hidden { vec![(SECRET, CanonicalScalar::Int(101))] } else { vec![] });
        db.write(cx, batch).await.unwrap();
    }
    if hidden {
        let mut batch = WriteBatch::new(H);
        for id in 50..55 {
            batch.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        db.write(cx, batch).await.unwrap();
    }
}

#[test]
fn creates_once_reduces_duplicate_pairs_and_reopens() {
    lab(0xab01, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let vertices = db.vertices().unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (stats, outcome, completion) = db.execute_graph_edge_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &merge(), policy(), || NOW,
        ).await.unwrap();
        let GraphEdgeMergeOutcome::Created(edge) = outcome else { panic!("missing creation") };
        assert_eq!((stats.match_selection.result_rows, stats.overlay_edges, stats.created_edges), (2, 0, 1));
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        let record = db.edge(edge).unwrap().unwrap();
        assert_eq!((record.entry.src, record.entry.relation, record.entry.dst), (VId(1), S, VId(2)));
        let (stats, outcome, completion) = db.execute_graph_edge_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &merge(),
            policy().with_creation_limit(0), || NOW,
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(edge));
        assert_eq!((stats.created_edges, stats.overlay_edges), (0, 1));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), vertices);
        let edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn ambiguous_pairs_parallel_eids_and_null_endpoints_never_allocate_or_commit() {
    lab(0xab02, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false, mode == 1).await;
            if mode == 1 {
                let mut extra = WriteBatch::new(S);
                extra.add_edge(EId(22), VId(1), VId(2), vec![]);
                db.write(&commit, extra).await.unwrap();
            }
            let statement = match mode {
                0 => definition("MATCH (a:Visible) MERGE (a)-[:S]->(a)"),
                1 => merge(),
                _ => definition("MATCH (a:Visible) WHERE a.p = 10 OPTIONAL MATCH (a)-[:S]->(b) MERGE (a)-[:R]->(b)"),
            };
            let request = GraphInsertRequest::Edge { row: 0, edge: 0 };
            let ElementId::Edge(reserved) = db.allocate_identity(&query, request).unwrap() else { panic!("edge id") };
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let error = db.execute_graph_edge_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
            ).await.unwrap_err();
            match (mode, error) {
                (0, GqlQueryError::Source(GraphEdgeMergeError::AmbiguousEndpointPairs { observed: 2 }))
                | (1, GqlQueryError::Source(GraphEdgeMergeError::AmbiguousRelationships { observed: 2 }))
                | (2, GqlQueryError::Source(GraphEdgeMergeError::NullEndpoint { .. })) => {}
                other => panic!("unexpected ambiguity result: {other:?}"),
            }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(db.allocate_identity(&query, request).unwrap(), ElementId::Edge(EId(reserved.0 + 1)));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn empty_input_has_no_receipt_or_creation_and_self_loop_is_one_edge() {
    lab(0xab03, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let empty = definition("MATCH (a:Hidden) MERGE (a)-[:S]->(a)");
        let (stats, outcome, completion) = db.execute_graph_edge_merge_authorized(
            &txn, &query, &commit, &authority, &zero, "main", &empty,
            policy().with_creation_limit(0), || NOW,
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
        assert_eq!((stats.match_selection.result_rows, stats.overlay_edges, stats.created_edges), (0, 0, 0));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
        let looped = definition("MATCH (a:Visible) WHERE a.p = 10 MERGE (a)-[:S]->(a)");
        let (_, outcome, _) = db.execute_graph_edge_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &looped, policy(), || NOW,
        ).await.unwrap();
        let edge = db.edge(outcome.edge().unwrap()).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(1)));
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn rights_relation_creation_fields_and_receipt_limits_refuse_before_publication() {
    lab(0xab04, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, matched).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            for rights in [Rights::Read, Rights::Write] {
                let mut grant = grant();
                grant.rights = rights;
                grant.limits.max_work = 0;
                let token = authority.issue_at(&grant, NOW).unwrap();
                let error = db.execute_graph_edge_merge_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &merge(), policy(), || NOW,
                ).await.unwrap_err();
                assert_eq!(authorization(error), Error::PermissionDenied);
            }
            let token = authority.issue_at(&grant(), NOW).unwrap();
            let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
            let error = db.execute_graph_edge_merge_authorized(
                &txn, &query, &commit, &authority, &zero, "main", &merge(), policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::LimitExceeded(LimitDimension::Rows));
            let forbidden = definition("MATCH (a:Hidden) MERGE (a)-[:Hidden]->(a)");
            let error = db.execute_graph_edge_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &forbidden, policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            let basic = merge();
            let fields = PreparedGraphEdgeMerge::prepare(
                basic.selection().clone(), S, basic.source_column(), basic.destination_column(),
                vec![(SECRET, GqlScalarParameter::new(CanonicalScalar::Int(999)).unwrap())],
            ).unwrap();
            let result = db.execute_graph_edge_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &fields, policy(), || NOW,
            ).await;
            if matched {
                assert_eq!(result.unwrap().1, GraphEdgeMergeOutcome::Matched(EId(21)));
            } else {
                assert_eq!(authorization(result.unwrap_err()), Error::ScopeDenied);
                let error = db.execute_graph_edge_merge_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &merge(),
                    policy().with_creation_limit(0), || NOW,
                ).await.unwrap_err();
                assert!(matches!(error, GqlQueryError::Source(GraphEdgeMergeError::CreationLimit { limit: 0, observed: 1 })));
            }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_graph_changes_no_signed_or_native_refusal_threshold() {
    lab(0xab05, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for matched in [false, true] {
            // Signed work/nodes and native records/work/scratch share the same
            // logical visible domain. This catches charge-before-mask mutants.
            for dimension in 0..5 {
                let mut floors = Vec::new();
                for hidden in [false, true] {
                    let (mut low, mut high) = (0_u64, 4096_u64);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        let limited = match dimension {
                            0 => token.attenuate(Restriction::MaxWork(middle)).unwrap(),
                            1 => token.attenuate(Restriction::MaxNodes(middle)).unwrap(),
                            _ => token.clone(),
                        };
                        let mut policy = policy();
                        match dimension {
                            2 => policy.query.rows = fgdb_gql::GqlExecutionBudget::snapshot_records(middle),
                            3 => policy.query.evaluator.max_work_units = middle,
                            4 => policy.query.evaluator.max_scratch_entries = middle,
                            _ => {}
                        }
                        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                        seed(&mut db, &commit, hidden, matched).await;
                        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                        match db.execute_graph_edge_merge_authorized(
                            &txn, &query, &commit, &authority, &limited, "main", &merge(), policy, || NOW,
                        ).await {
                            Ok((stats, outcome, _)) => {
                                assert_eq!(stats.created_edges, u64::from(!matched));
                                assert_eq!(outcome.created(), !matched);
                                high = middle;
                            }
                            Err(error) => {
                                match dimension {
                                    0 => assert_eq!(authorization(error), Error::LimitExceeded(LimitDimension::Work)),
                                    1 => assert_eq!(authorization(error), Error::LimitExceeded(LimitDimension::Nodes)),
                                    2 => assert!(matches!(error, GqlQueryError::Rows(_))),
                                    _ => assert!(matches!(error, GqlQueryError::Evaluator(_))),
                                }
                                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                                low = middle + 1;
                            }
                        }
                        assert_eq!(txn.outstanding_obligations(), 0);
                    }
                    assert!(low > 0 && low < 4096);
                    floors.push(low);
                }
                assert_eq!(floors[0], floors[1], "matched={matched}, dimension={dimension}");
            }
        }
    });
}

#[test]
fn every_live_expiry_boundary_discards_creation_and_releases_the_pin() {
    lab(0xab06, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_edge_merge_authorized(
            &txn, &query, &commit, &authority, &token, "main", &merge(), policy(), || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        ).await.unwrap();
        let total = calls.load(Ordering::Relaxed);
        assert!(total > 10);
        for cutoff in 0..total {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, false).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let calls = AtomicU64::new(0);
            let error = db.execute_graph_edge_merge_authorized(
                &txn, &query, &commit, &authority, &token, "main", &merge(), policy(), || {
                    if calls.fetch_add(1, Ordering::Relaxed) < cutoff { NOW } else { 10_000 }
                },
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::Expired, "cutoff={cutoff}");
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn shared_native_collector_retains_negative_edge_conflict_witnesses() {
    lab(0xab07, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false, false).await;
        let mut transaction = db.begin(&txn_cx).unwrap();
        let (_, outcome) = transaction.execute_graph_edge_merge_governed(
            &mut db, &query, &merge(), policy(), |_| Ok::<_, ()>(ElementId::Edge(EId(80))),
        ).unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(80)));
        let mut competing = WriteBatch::new(S);
        competing.add_edge(EId(81), VId(1), VId(2), vec![]);
        db.write(&commit, competing).await.unwrap();
        let seq = db.frontier().unwrap();
        assert!(transaction.finish(&mut db, &commit).await.is_err());
        assert_eq!(db.frontier().unwrap(), seq);
        assert!(db.edge(EId(80)).unwrap().is_none());
        assert!(db.edge(EId(81)).unwrap().is_some());
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
}
