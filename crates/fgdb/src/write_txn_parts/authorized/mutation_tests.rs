//! Real token -> masked MATCH -> native mutation -> Chronicle completion tests.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const H: RelationId = RelationId(9);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x51; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x50; 32], NS, [0x52; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9951), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Hidden") => Some(GraphSymbol::Relation(H)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn mutation(text: &str) -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn update() -> PreparedGraphMutation {
    mutation("MATCH (a:Visible)-[e:R]->(b:Visible) WHERE a.p = 10 \
        SET a.p = CASE WHEN a.secret IS NULL THEN a.p + 1 ELSE 0 END, \
        a.q = b.p, e.p = e.p + 5")
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::Authorization(error))) => error,
        other => panic!("expected authorization failure, got {other:?}"),
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
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for (id, value, secret) in [(1, 10, 71), (2, 20, 72)] {
        let mut props = vec![(P, CanonicalScalar::Int(value))];
        let mut labels = vec![L];
        if hidden {
            props.push((SECRET, CanonicalScalar::Int(secret)));
            if id == 2 { labels.push(HIDDEN); }
        }
        batch.create_vertex(VId(id), labels, props);
    }
    for (id, secret) in [(11, 81), (12, 82)] {
        let mut props = vec![(P, CanonicalScalar::Int(2))];
        if hidden { props.push((SECRET, CanonicalScalar::Int(secret))); }
        batch.add_edge(EId(id), VId(1), VId(2), props);
    }
    if hidden {
        batch.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(30))]);
        batch.add_edge(EId(13), VId(1), VId(3), vec![(P, CanonicalScalar::Int(9))]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut batch = WriteBatch::new(H);
        batch.add_edge(EId(21), VId(1), VId(2), vec![(P, CanonicalScalar::Int(8))]);
        db.write(cx, batch).await.unwrap();
    }
}

#[test]
fn masked_simultaneous_vertex_and_edge_updates_deduplicate_and_reopen() {
    lab(0xa951, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let frontier = db.frontier().unwrap();
        let preserved = (db.vertex(VId(2)).unwrap(), db.vertex(VId(3)).unwrap(),
            db.edge(EId(13)).unwrap(), db.edge(EId(21)).unwrap());
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (stats, vertices, edges, completion) = db.execute_graph_mutation_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &update(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.selection.result_rows, 2);
        assert_eq!((stats.target_vertices, stats.target_edges, stats.effects), (1, 2, 4));
        assert_eq!(vertices, vec![VId(1)]);
        assert_eq!(edges, vec![EId(11), EId(12)]);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(11)), (SECRET, CanonicalScalar::Int(71)), (Q, CanonicalScalar::Int(20))]);
        for (id, secret) in [(11, 81), (12, 82)] {
            assert_eq!(db.edge(EId(id)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(7)), (SECRET, CanonicalScalar::Int(secret))]);
        }
        assert_eq!((db.vertex(VId(2)).unwrap(), db.vertex(VId(3)).unwrap(),
            db.edge(EId(13)).unwrap(), db.edge(EId(21)).unwrap()), preserved);
        let vertices = db.vertices().unwrap();
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
fn forbidden_field_noops_and_scope_escape_leave_no_published_prefix() {
    lab(0xa952, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for hidden in [false, true] {
            for tail in ["SET a.secret = a.secret", "REMOVE a.secret", "REMOVE a:Visible"] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, hidden).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                let statement = mutation(&format!("MATCH (a:Visible) WHERE a.p = 10 SET a.p = 11 {tail}"));
                let error = db.execute_graph_mutation_authorized(
                    &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
                ).await.unwrap_err();
                assert_eq!(authorization(error), Error::ScopeDenied);
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn receipt_quota_counts_distinct_targets_and_refuses_before_publication() {
    lab(0xa953, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for rows in [0, 1, 2, 3] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let result = db.execute_graph_mutation_returning_authorized(
                &txn, &query, &commit, &authority, &limited, "main", &update(), policy(), || NOW,
            ).await;
            if rows < 3 {
                assert_eq!(authorization(result.unwrap_err()), Error::LimitExceeded(LimitDimension::Rows));
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            } else {
                let (stats, vertices, edges, _) = result.unwrap();
                assert_eq!(stats.effects, 4);
                assert_eq!(vertices.len() + edges.len(), 3);
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, _) = db.execute_graph_mutation_authorized(
            &txn, &query, &commit, &authority, &zero, "main", &update(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.effects, 4);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn rights_precede_source_access_and_empty_matches_close_without_a_commit() {
    lab(0xa954, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
        let statement = mutation("MATCH (a:Visible) WHERE a.p = 999 SET a.p = 1");
        for rights in [Rights::Read, Rights::Write] {
            let mut grant = grant();
            grant.rights = rights;
            grant.limits.max_work = 0;
            let token = authority.issue_at(&grant, NOW).unwrap();
            let error = db.execute_graph_mutation_authorized(
                &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::PermissionDenied);
        }
        let token = authority.issue_at(&grant(), NOW).unwrap()
            .attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, vertices, edges, completion) = db.execute_graph_mutation_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.effects, 0);
        assert!(vertices.is_empty() && edges.is_empty());
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn conflicting_assignments_and_effect_limits_refuse_the_complete_statement() {
    lab(0xa955, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
        let statement = mutation("MATCH (a:Visible) WHERE a.p = 10 SET a.p = 1, a.p = 2");
        let error = db.execute_graph_mutation_authorized(
            &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. })));
        let limited = GraphMutationPolicy { max_effects: 3, ..policy() };
        let error = db.execute_graph_mutation_authorized(
            &txn, &query, &commit, &authority, &token, "main", &update(), limited, || NOW,
        ).await.unwrap_err();
        assert!(matches!(error, GqlQueryError::Source(GraphMutationError::EffectLimit { limit: 3, .. })));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn detach_delete_cannot_probe_hidden_incidence_but_unrestricted_cascades_are_atomic() {
    lab(0xa956, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = mutation("MATCH (a:Visible) WHERE a.p = 10 DETACH DELETE a");
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, hidden).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let error = db.execute_graph_mutation_authorized(
                &txn, &query, &commit, &authority, &token, "main", &statement, policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            let mut grant = grant();
            grant.labels = Scope::All;
            grant.relations = Scope::All;
            grant.properties = Scope::All;
            let full = authority.issue_at(&grant, NOW).unwrap();
            let (stats, vertices, edges, _) = db.execute_graph_mutation_returning_authorized(
                &txn, &query, &commit, &authority, &full, "main", &statement, policy(), || NOW,
            ).await.unwrap();
            assert_eq!(stats.effects, 1);
            assert_eq!(vertices, vec![VId(1)]);
            assert!(edges.is_empty(), "cascade edges are not explicit proposal targets");
            assert!(db.vertex(VId(1)).unwrap().is_none());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn expiry_in_selection_staging_or_final_admission_never_publishes_a_prefix() {
    lab(0xa957, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_mutation_returning_authorized(
            &txn, &query, &commit, &authority, &token, "main", &update(), policy(), || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        ).await.unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        for cutoff in [1, count / 2, count - 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let calls = AtomicU64::new(0);
            let error = db.execute_graph_mutation_returning_authorized(
                &txn, &query, &commit, &authority, &token, "main", &update(), policy(), || {
                    if calls.fetch_add(1, Ordering::Relaxed) < cutoff { NOW } else { 10_000 }
                },
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::Expired);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn selection_and_mutation_share_the_native_node_allowance() {
    lab(0xa958, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut floors = Vec::new();
        for matched in [false, true] {
            let (mut low, mut high) = (0_u64, 128_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxNodes(middle)).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, false).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                let success = if matched {
                    match db.execute_graph_mutation_authorized(
                        &txn, &query, &commit, &authority, &limited, "main", &update(), policy(), || NOW,
                    ).await {
                        Ok(_) => true,
                        Err(error) => {
                            assert_eq!(authorization(error), Error::LimitExceeded(LimitDimension::Nodes));
                            false
                        }
                    }
                } else {
                    // Independent native witness for the four canonical effects,
                    // not a second call through the MATCH mutation adapter.
                    let mut batch = WriteBatch::new(R);
                    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
                    batch.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(20)));
                    batch.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(7)));
                    batch.set_edge_property(EId(12), P, Some(CanonicalScalar::Int(7)));
                    match db.write_authorized(&txn, &commit, &authority, &limited, "main", batch, || NOW).await {
                        Ok(_) => true,
                        Err(WriteTxnError::Authorization(error)) => {
                            assert_eq!(error, Error::LimitExceeded(LimitDimension::Nodes));
                            false
                        }
                        other => panic!("native witness failed: {other:?}"),
                    }
                };
                if success {
                    high = middle;
                } else {
                    low = middle + 1;
                    assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 0 && low < 128);
            floors.push(low);
        }
        assert_eq!(floors[1], floors[0] + 2, "MATCH must add the two visible source admissions");
    });
}

#[test]
fn hidden_fields_topology_and_relations_do_not_change_signed_refusal_thresholds() {
    lab(0xa959, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for dimension in [LimitDimension::Work, LimitDimension::Nodes] {
            let mut floors = Vec::new();
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let restriction = match dimension {
                        LimitDimension::Work => Restriction::MaxWork(middle),
                        LimitDimension::Nodes => Restriction::MaxNodes(middle),
                        _ => unreachable!(),
                    };
                    let limited = token.attenuate(restriction).unwrap();
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit, hidden).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                    match db.execute_graph_mutation_authorized(
                        &txn, &query, &commit, &authority, &limited, "main", &update(), policy(), || NOW,
                    ).await {
                        Ok((stats, _)) => {
                            assert_eq!((stats.target_vertices, stats.target_edges, stats.effects), (1, 2, 4));
                            high = middle;
                        }
                        Err(error) => {
                            assert_eq!(authorization(error), Error::LimitExceeded(dimension));
                            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                            low = middle + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(low > 0 && low < 8192);
                floors.push(low);
            }
            assert_eq!(floors[0], floors[1], "hidden data changed {dimension:?} refusal threshold");
        }
    });
}
