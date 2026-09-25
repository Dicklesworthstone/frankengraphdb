//! Plain DELETE through actual capabilities, native incidence and Chronicle.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphDeleteText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x61; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x60; 32], NS, [0x62; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9961), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::All,
        relations: Scope::All,
        properties: Scope::All,
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Other") => Some(GraphSymbol::Relation(OTHER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn deletion(text: &str) -> PreparedGraphDelete {
    PreparedGraphDeleteText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn mixed() -> PreparedGraphDelete {
    // A vertex is deliberately declared BEFORE its edges, and both endpoints
    // recur in the parallel-edge MATCH bag. Physical deletion must reorder.
    deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE a, e, b")
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphDeleteError::Source(WriteTxnError::Authorization(error))) => {
            error
        }
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
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, parallel: bool) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(20))]);
    batch.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
    if parallel {
        batch.add_edge(EId(12), VId(1), VId(2), vec![(P, CanonicalScalar::Int(2))]);
    }
    db.write(cx, batch).await.unwrap();
}

#[test]
fn mixed_delete_reorders_edges_deduplicates_receipts_and_reopens() {
    lab(0xa961, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit, true).await;
        let frontier = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (stats, vertices, edges, completion) = db
            .execute_graph_delete_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &mixed(),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.selection.result_rows, 2);
        assert_eq!((stats.target_vertices, stats.target_edges), (2, 2));
        assert_eq!(vertices, vec![VId(1), VId(2)]);
        assert_eq!(edges, vec![EId(11), EId(12)]);
        // Four MATCH source records plus the two native incidence witnesses.
        assert_eq!(stats.selection.snapshot_records, 6);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn scoped_edge_deletion_checks_actual_relation_and_preserves_endpoints() {
    lab(0xa962, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let mut other = WriteBatch::new(OTHER);
        other.add_edge(
            EId(21),
            VId(1),
            VId(2),
            vec![(SECRET, CanonicalScalar::Int(99))],
        );
        db.write(&commit, other).await.unwrap();
        let before = db.vertices().unwrap();
        let frontier = db.frontier().unwrap();
        let authority = authority();
        let mut grant = grant();
        grant.labels = Scope::only([L]);
        grant.relations = Scope::only([OTHER]);
        let token = authority.issue_at(&grant, NOW).unwrap();
        // The default coordinate R is not authorized; the actual EId belongs
        // to Other, which is authorized. Deletion must use the original edge.
        let statement = deletion("MATCH (a:Visible)-[e:Other]->(b:Visible) DELETE e");
        let (stats, vertices, edges, _) = db
            .execute_graph_delete_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.target_vertices, stats.target_edges), (0, 1));
        assert!(vertices.is_empty());
        assert_eq!(edges, vec![EId(21)]);
        assert_eq!(db.vertices().unwrap(), before);
        assert!(db.edge(EId(21)).unwrap().is_none());
        assert!(db.edge(EId(11)).unwrap().is_some() && db.edge(EId(12)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn every_unselected_incidence_kind_refuses_without_implicit_cascade() {
    lab(0xa963, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = deletion("MATCH (a:Visible)-[e:R]->(b:Visible) WHERE e.p = 1 DELETE a, e");
        for mode in 0..6 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            if mode != 0 {
                let mut extra = WriteBatch::new(if mode == 4 { OTHER } else { R });
                if mode == 5 {
                    extra.create_vertex(VId(3), vec![HIDDEN], vec![]);
                }
                let (source, destination) = match mode {
                    2 => (VId(2), VId(1)), // incoming
                    3 => (VId(1), VId(1)), // self-loop
                    5 => (VId(1), VId(3)), // endpoint excluded by user MATCH
                    _ => (VId(1), VId(2)), // parallel or cross-relation
                };
                extra.add_edge(
                    EId(30),
                    source,
                    destination,
                    vec![(P, CanonicalScalar::Int(2))],
                );
                db.write(&commit, extra).await.unwrap();
            }
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let result = db
                .execute_graph_delete_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &statement,
                    policy(),
                    || NOW,
                )
                .await;
            if mode == 0 {
                let (_, vertices, edges, _) = result.unwrap();
                assert_eq!(vertices, vec![VId(1)]);
                assert_eq!(edges, vec![EId(11)]);
                assert!(db.vertex(VId(1)).unwrap().is_none());
                assert!(db.vertex(VId(2)).unwrap().is_some());
                assert!(db.edges().unwrap().is_empty());
            } else {
                assert!(
                    matches!(
                        result,
                        Err(GqlQueryError::Source(
                            GraphDeleteError::IncidentRelationships
                        ))
                    ),
                    "mode={mode}"
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn deletion_scope_refusal_does_not_disclose_hidden_fields_or_incidence() {
    lab(0xa964, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            if hidden {
                let mut extra = WriteBatch::new(OTHER);
                extra.create_vertex(VId(3), vec![HIDDEN], vec![]);
                extra.add_edge(EId(30), VId(1), VId(3), vec![]);
                extra.set_edge_property(EId(11), SECRET, Some(CanonicalScalar::Int(99)));
                db.write(&commit, extra).await.unwrap();
            }
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut vertex_grant = grant();
            vertex_grant.labels = Scope::only([L]);
            vertex_grant.relations = Scope::only([R]);
            let token = authority.issue_at(&vertex_grant, NOW).unwrap();
            // The visible selected edge would leave the source isolated only
            // in one fixture. Both must refuse from capability scope instead.
            let error = db
                .execute_graph_delete_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE a, e"),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            let mut edge_grant = grant();
            edge_grant.properties = Scope::only([P]);
            let token = authority.issue_at(&edge_grant, NOW).unwrap();
            let error = db
                .execute_graph_delete_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE e"),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn result_target_and_incidence_record_limits_all_precede_publication() {
    lab(0xa965, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for rows in 0..=4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let result = db
                .execute_graph_delete_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &limited,
                    "main",
                    &mixed(),
                    policy(),
                    || NOW,
                )
                .await;
            if rows < 4 {
                assert_eq!(
                    authorization(result.unwrap_err()),
                    Error::LimitExceeded(LimitDimension::Rows)
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            } else {
                let (_, vertices, edges, _) = result.unwrap();
                assert_eq!(vertices.len() + edges.len(), 4);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let limited = GraphDeletePolicy {
            max_targets: 3,
            ..policy()
        };
        let error = db
            .execute_graph_delete_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &mixed(),
                limited,
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphDeleteError::TargetLimit { limit: 3, .. })
        ));
        let limited = GraphDeletePolicy::new(GqlQueryPolicy::new(5, 100, 1_000_000, 100_000), 100);
        let error = db
            .execute_graph_delete_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &mixed(),
                limited,
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, GqlQueryError::Rows(_)));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, _) = db
            .execute_graph_delete_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &zero,
                "main",
                &mixed(),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.target_vertices, stats.target_edges), (2, 2));
        assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn readwrite_rights_are_required_even_for_empty_delete_and_empty_results_read_close() {
    lab(0xa966, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let statement = deletion("MATCH (a:Visible) WHERE a.p = 999 DELETE a");
        for rights in [Rights::Read, Rights::Write] {
            let mut grant = grant();
            grant.rights = rights;
            grant.limits.max_work = 0;
            let token = authority.issue_at(&grant, NOW).unwrap();
            let error = db
                .execute_graph_delete_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &statement,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::PermissionDenied);
        }
        let token = authority
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(0))
            .unwrap();
        let (stats, vertices, edges, completion) = db
            .execute_graph_delete_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.target_vertices, stats.target_edges), (0, 0));
        assert!(vertices.is_empty() && edges.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn plain_delete_uses_one_node_allowance_and_hidden_topology_does_not_change_it() {
    lab(0xa967, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut grant = grant();
        grant.labels = Scope::only([L]);
        grant.relations = Scope::only([R]);
        let token = authority.issue_at(&grant, NOW).unwrap();
        let statement = deletion("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE e");
        let mut floors = Vec::new();
        for mode in 0..3 {
            let (mut low, mut high) = (0_u64, 128_u64);
            while low < high {
                let middle = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxNodes(middle)).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, true).await;
                if mode == 2 {
                    let mut extra = WriteBatch::new(OTHER);
                    extra.create_vertex(VId(3), vec![HIDDEN], vec![]);
                    extra.add_edge(EId(30), VId(1), VId(3), vec![]);
                    db.write(&commit, extra).await.unwrap();
                }
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let success = if mode == 0 {
                    let mut batch = WriteBatch::new(R);
                    batch.delete_edge(EId(11));
                    batch.delete_edge(EId(12));
                    match db
                        .write_authorized(
                            &txn,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            batch,
                            || NOW,
                        )
                        .await
                    {
                        Ok(_) => true,
                        Err(WriteTxnError::Authorization(error)) => {
                            assert_eq!(error, Error::LimitExceeded(LimitDimension::Nodes));
                            false
                        }
                        other => panic!("native witness: {other:?}"),
                    }
                } else {
                    match db
                        .execute_graph_delete_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &statement,
                            policy(),
                            || NOW,
                        )
                        .await
                    {
                        Ok(_) => true,
                        Err(error) => {
                            assert_eq!(
                                authorization(error),
                                Error::LimitExceeded(LimitDimension::Nodes)
                            );
                            false
                        }
                    }
                };
                if success {
                    high = middle;
                } else {
                    low = middle + 1;
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 0 && low < 128);
            floors.push(low);
        }
        assert_eq!(
            floors[1],
            floors[0] + 2,
            "MATCH source admissions were reset"
        );
        assert_eq!(
            floors[2], floors[1],
            "hidden graph changed the node refusal threshold"
        );
    });
}

#[test]
fn expiry_during_collection_proof_or_native_completion_discards_every_delete() {
    lab(0xa968, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_delete_returning_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &mixed(),
            policy(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        )
        .await
        .unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        for cutoff in [1, count / 2, count - 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let calls = AtomicU64::new(0);
            let error = db
                .execute_graph_delete_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &mixed(),
                    policy(),
                    || {
                        if calls.fetch_add(1, Ordering::Relaxed) < cutoff {
                            NOW
                        } else {
                            10_000
                        }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::Expired);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn isolated_vertex_delete_preserves_unrelated_topology() {
    lab(0xa969, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, value) in [(1, 10), (2, 20), (3, 30)] {
            batch.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(value))]);
        }
        batch.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write(&commit, batch).await.unwrap();
        let before = (
            db.vertex(VId(2)).unwrap(),
            db.vertex(VId(3)).unwrap(),
            db.edges().unwrap(),
        );
        let frontier = db.frontier().unwrap();
        let (stats, vertices, edges, _) = db
            .execute_graph_delete_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &deletion("MATCH (a:Visible) WHERE a.p = 10 DELETE a"),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.target_vertices, stats.target_edges), (1, 0));
        assert_eq!(vertices, vec![VId(1)]);
        assert!(edges.is_empty());
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert_eq!(
            (
                db.vertex(VId(2)).unwrap(),
                db.vertex(VId(3)).unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(db.frontier().unwrap().0, frontier.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
