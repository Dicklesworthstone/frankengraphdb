//! FOREACH shares the native authorized chain's scope, meters and one commit.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xe1; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xe0; 32], NS, [0xe2; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xf0eac), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights, rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 2_000_000,
            max_rows: rows,
        },
        expires_at_ms: 10_000,
    }
}

fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 2_000_000, 1_000_000),
        100,
        100,
    )
    .with_mutation_limit(100)
}

fn merge_grant(rows: u64) -> Grant {
    let mut scoped = grant(Rights::ReadWrite, rows);
    scoped.properties = Scope::only([P, Q]);
    scoped
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

#[test]
fn authorized_foreach_preserves_outer_rows_and_reads_each_staged_iteration() {
    let ((), report) = run_async_under_lab(0xf0ea_c001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 5), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, label, value) in [(1, VISIBLE, 0), (2, VISIBLE, 10), (3, HIDDEN, 100)] {
            seed.create_vertex(VId(id), vec![label], vec![(P, CanonicalScalar::Int(value))]);
        }
        db.write(&commit, seed).await.unwrap();
        let hidden = db.vertex(VId(3)).unwrap();
        for (text, expected) in [
            (
                "MATCH (n:Visible) FOREACH (x IN [1,2,3] | SET n.p=n.p+x) \
                 RETURN n.p AS p ORDER BY p",
                vec![CanonicalScalar::Int(6), CanonicalScalar::Int(16)],
            ),
            (
                "UNWIND [1,1] AS repeat MATCH (n:Visible {p:6}) \
                 FOREACH (x IN [1,2] | SET n.p=n.p+x) RETURN n.p AS p",
                vec![CanonicalScalar::Int(12), CanonicalScalar::Int(12)],
            ),
            (
                "UNWIND [1,1] AS repeat MATCH (n:Visible {p:12}) \
                 FOREACH (x IN range(1,n.p-11) | SET n.p=n.p+1) RETURN n.p AS p",
                // The second occurrence reads p=13 for its collection and
                // iterates twice; it must not reuse the first list of length 1.
                vec![CanonicalScalar::Int(15), CanonicalScalar::Int(15)],
            ),
            (
                "MATCH (n:Visible {p:15}) FOREACH (x IN [1] | REMOVE n.p) RETURN n.p AS p",
                vec![CanonicalScalar::Null],
            ),
        ] {
            let before = db.frontier().unwrap();
            let (_, rows, completion) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(5),
                    || NOW,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let actual: Vec<_> = rows
                .value
                .iter()
                .map(|row| row.get(0).unwrap().as_scalar().unwrap().clone())
                .collect();
            assert_eq!(actual, expected, "{text}");
            assert!(
                matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                if commit_seq.0 == before.0 + 1)
            );
            assert_eq!(db.vertex(VId(3)).unwrap(), hidden);
        }

        // NULL and [] perform no writes. [NULL], a scalar, and a map each
        // perform one iteration. All five original rows survive unchanged;
        // private body occurrences do not spend the five signed result rows.
        let before = db.frontier().unwrap();
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "UNWIND [NULL,[],[NULL],7,{p:9}] AS xs \
                        FOREACH (x IN xs | CREATE (:Visible)) RETURN 42 AS answer",
                ),
                policy(5),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 3);
        assert_eq!(rows.value.len(), 5);
        for row in rows.value {
            assert_eq!(
                row.values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(42))]
            );
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_preflights_empty_bodies_before_allocating_any_identity() {
    let ((), report) = run_async_under_lab(0xf0ea_c002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "CREATE (n:Visible {p:0}) \
                        FOREACH (x IN [] | SET n.p=1 SET n.secret=x) RETURN n.p",
                ),
                policy(1),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        let mut pristine = Database::open_memory(&commit, keys()).await.unwrap();
        let request = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
        assert_eq!(
            db.allocate_identity(&query, request).unwrap(),
            pristine.allocate_identity(&query, request).unwrap(),
            "a denied field in an empty later body must precede the first CREATE allocation"
        );

        // Source-free, output-free CREATE keeps the standalone Write right.
        // Iteration itself does not manufacture a read or public result row.
        let write_only = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &write_only,
                "main",
                &chain("FOREACH (x IN [1,2,3] | CREATE (:Visible {p:x}))"),
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 3);
        assert!(rows.value.is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_keeps_one_creation_effect_and_evaluator_allowance() {
    let ((), report) = run_async_under_lab(0xf0ea_c003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        for (text, bounded) in [
            (
                "FOREACH (x IN [1,2,3] | CREATE (:Visible {p:x})) RETURN 1 AS done",
                GraphInsertPolicy::new(policy(1).query, 2, 100).with_mutation_limit(100),
            ),
            (
                "MATCH (n:Visible) FOREACH (x IN [1,2,3] | SET n.p=n.p+x) RETURN n.p",
                policy(1).with_mutation_limit(2),
            ),
            (
                "FOREACH (x IN [1,0] | CREATE (:Visible {p:10/x})) RETURN 1 AS done",
                policy(1),
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![VISIBLE], vec![(P, CanonicalScalar::Int(0))]);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let vertices = db.vertices().unwrap();
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "{text}"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.vertices().unwrap(), vertices);
        }
        let program = chain(
            "FOREACH (x IN [1,2,3] | CREATE (n:Visible {p:x}) SET n.p=n.p+1) \
                             RETURN 1 AS done",
        );
        let mut reference = Database::open_memory(&commit, keys()).await.unwrap();
        let (complete, _, _) = reference
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        for scratch in [false, true] {
            let mut bounded = policy(1);
            if scratch {
                bounded.query.evaluator.max_scratch_entries =
                    complete.evaluator.scratch_entries - 1;
            } else {
                bounded.query.evaluator.max_work_units = complete.evaluator.work_units - 1;
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_cancellation_and_late_expiration_discard_all_iterations() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ((), report) = run_async_under_lab(0xf0ea_c004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "FOREACH (x IN [1,2,3,4,5] | CREATE (n:Visible {p:x}) SET n.p=n.p+1) \
                             RETURN 1 AS done",
        );
        let observed = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&observed));
        let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
        let (stats, _, _) = baseline
            .execute_graph_write_chain_authorized(
                &txn,
                &controlled,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 5);
        let calls = observed.calls();
        assert!(calls > 3);
        for stop in [calls / 2, calls - 1] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &controlled,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(probe.calls(), stop);
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }

        let expired = Arc::new(AtomicBool::new(false));
        let clock = Arc::clone(&expired);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                move || {
                    if clock.load(Ordering::Relaxed) {
                        10_000
                    } else {
                        NOW
                    }
                },
                |rows, _| {
                    assert_eq!(rows.len(), 1);
                    // Output admission is reached only after all five CREATE
                    // and SET iterations. Expiry still precedes publication.
                    expired.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(expired.load(Ordering::Relaxed));
        assert_eq!(authorization(&error), Some(Error::Expired));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_merge_reuses_local_keys_and_reopens_one_import() {
    let ((), report) = run_async_under_lab(0xf0ea_c005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&merge_grant(0), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![HIDDEN],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(100))],
        );
        db.write(&commit, seed).await.unwrap();
        let hidden = db.vertex(VId(1)).unwrap();
        let pinned = db.read_session().unwrap();
        let before = db.frontier().unwrap();
        let program = chain(
            "FOREACH (row IN [{s:1,d:2},{s:1,d:2},{s:2,d:3}] | \
             MERGE (a:Visible {p:row.s}) ON CREATE SET a.q=0 ON MATCH SET a.q=a.q+1 \
             MERGE (b:Visible {p:row.d}) ON CREATE SET b.q=0 ON MATCH SET b.q=b.q+1 \
             MERGE (a)-[e:R]->(b) ON CREATE SET e.q=0 ON MATCH SET e.q=e.q+1)",
        );
        let bounded = GraphInsertPolicy::new(policy(0).query, 3, 2).with_mutation_limit(100);
        let (stats, rows, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                bounded,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (3, 2));
        assert_eq!(rows.rows.result_rows, 0);
        assert!(rows.value.is_empty(), "body MERGE rows stay private");
        let frontier = db.frontier().unwrap();
        assert_eq!(frontier.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: frontier
            }
        );
        assert_eq!(db.delta_since(before).unwrap().count(), 1);
        assert_eq!(db.vertex(VId(1)).unwrap(), hidden);
        assert_eq!(pinned.vertices().unwrap(), vec![hidden.clone().unwrap()]);
        assert!(pinned.edges().unwrap().is_empty());

        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        assert_eq!(vertices.len(), 4);
        assert_eq!(edges.len(), 2);
        let visible = |key: i64| {
            vertices
                .iter()
                .find(|vertex| {
                    vertex.labels == vec![VISIBLE]
                        && vertex
                            .props
                            .contains(&(P, CanonicalScalar::Int(key).into()))
                })
                .unwrap()
        };
        for (key, visits) in [(1, 1), (2, 2), (3, 0)] {
            assert_eq!(
                visible(key).props,
                vec![
                    (P, CanonicalScalar::Int(key).into()),
                    (Q, CanonicalScalar::Int(visits).into()),
                ]
            );
        }
        for (source, destination, visits) in [(1, 2, 1), (2, 3, 0)] {
            let edge = edges
                .iter()
                .find(|edge| {
                    edge.entry.src == visible(source).vid
                        && edge.entry.dst == visible(destination).vid
                })
                .unwrap();
            assert_eq!(edge.props, vec![(Q, CanonicalScalar::Int(visits).into())]);
        }
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), frontier);
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(reopened.edges().unwrap(), edges);

        let mut denied = Database::open_memory(&commit, keys()).await.unwrap();
        let before = denied.frontier().unwrap();
        assert!(
            denied
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    GraphInsertPolicy::new(policy(0).query, 3, 1).with_mutation_limit(100),
                    || NOW,
                )
                .await
                .is_err(),
            "the second distinct relationship must exhaust one shared edge allowance"
        );
        assert_eq!(denied.frontier().unwrap(), before);
        assert!(denied.vertices().unwrap().is_empty());
        assert!(denied.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_merge_visits_all_targets_under_one_effect_allowance() {
    let ((), report) = run_async_under_lab(0xf0ea_c006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&merge_grant(1), NOW).unwrap();
        let program = chain(
            "FOREACH (delta IN [1,2] | \
             MERGE (a:Visible {p:7}) ON MATCH SET a.q=a.q+delta \
             MERGE (b:Visible {p:8}) \
             MERGE (a)-[e:R]->(b) ON CREATE SET e.secret=99 \
             ON MATCH SET e.q=e.q+delta) RETURN 42 AS answer",
        );
        for limit in [10, 9] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for (id, label, key, value) in [
                (1, VISIBLE, 7, 10),
                (2, VISIBLE, 7, 20),
                (3, HIDDEN, 7, 100),
                (4, VISIBLE, 8, 0),
            ] {
                seed.create_vertex(
                    VId(id),
                    vec![label],
                    vec![
                        (P, CanonicalScalar::Int(key)),
                        (Q, CanonicalScalar::Int(value)),
                    ],
                );
            }
            for (id, source, value) in [(21, 1, 10), (22, 1, 20), (23, 2, 30)] {
                seed.add_edge(
                    EId(id),
                    VId(source),
                    VId(4),
                    vec![(Q, CanonicalScalar::Int(value))],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let bounded = GraphInsertPolicy::new(policy(1).query, 0, 0).with_mutation_limit(limit);
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if limit == 10 {
                let (stats, rows, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
                assert_eq!(rows.rows.result_rows, 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(42))]
                );
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                for (id, value) in [(1, 13), (2, 23)] {
                    assert_eq!(
                        db.vertex(VId(id)).unwrap().unwrap().props,
                        vec![
                            (P, CanonicalScalar::Int(7).into()),
                            (Q, CanonicalScalar::Int(value).into()),
                        ]
                    );
                }
                assert_eq!(db.vertex(VId(3)).unwrap().as_ref(), Some(&before.1[2]));
                let edges = db.edges().unwrap();
                assert_eq!(edges.len(), 3);
                for (id, value) in [(21, 13), (22, 23), (23, 33)] {
                    let edge = edges.iter().find(|edge| edge.entry.eid == EId(id)).unwrap();
                    assert_eq!(edge.props, vec![(Q, CanonicalScalar::Int(value).into())]);
                }
            } else {
                let error = result.unwrap_err();
                assert!(format!("{error:?}").contains("EffectLimit"), "{error:?}");
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
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_merge_preflights_empty_lookup_scope_and_read_rights() {
    let ((), report) = run_async_under_lab(0xf0ea_c007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (text, rights, expected) in [
            (
                "CREATE (marker:Visible) FOREACH (x IN [] | MERGE (n:Hidden {p:x}))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (marker:Visible) FOREACH (x IN [] | MERGE (n:Visible {secret:x}))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (a:Visible),(b:Visible) FOREACH (x IN [] | MERGE (a)-[:S]->(b))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (marker:Visible) FOREACH (x IN [] | MERGE (n:Visible {p:x}))",
                Rights::Write,
                Error::PermissionDenied,
            ),
            (
                "CREATE (a:Visible),(b:Visible) FOREACH (x IN [] | MERGE (a)-[:R]->(b))",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let token = issuer.issue_at(&grant(rights, 0), NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(expected), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            let mut pristine = Database::open_memory(&commit, keys()).await.unwrap();
            let request = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
            assert_eq!(
                db.allocate_identity(&query, request).unwrap(),
                pristine.allocate_identity(&query, request).unwrap(),
                "lookup scope and Read right precede any prefix CREATE: {text}"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_merge_admits_only_selected_on_branches() {
    let ((), report) = run_async_under_lab(0xf0ea_c008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 0), NOW).unwrap();
        for (text, created) in [
            (
                "FOREACH (x IN [1,1] | MERGE (n:Visible {p:x}) ON CREATE SET n.secret=0)",
                0,
            ),
            (
                "FOREACH (x IN [2,3] | MERGE (n:Visible {p:x}) ON MATCH SET n.secret=0)",
                2,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![VISIBLE],
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (SECRET, CanonicalScalar::Int(99)),
                ],
            );
            db.write(&commit, seed).await.unwrap();
            let existing = db.vertex(VId(1)).unwrap();
            let (stats, rows, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0).with_mutation_limit(0),
                    || NOW,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(stats.created_vertices, created);
            assert!(rows.value.is_empty());
            assert_eq!(db.vertex(VId(1)).unwrap(), existing);
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        // The first three select a denied branch after an earlier body effect
        // or creation. The last also checks the imported-element property
        // reader. Neither route may publish any prefix of the statement.
        for text in [
            "FOREACH (x IN [1,2] | MERGE (n:Visible {p:x}) \
             ON CREATE SET n.secret=0 ON MATCH SET n.p=n.p+10)",
            "FOREACH (x IN [2,2] | MERGE (n:Visible {p:x}) \
             ON CREATE SET n.p=x ON MATCH SET n.secret=0)",
            "CREATE (a:Visible),(b:Visible) \
             FOREACH (x IN [1,1] | MERGE (a)-[e:R {p:x}]->(b) ON MATCH SET e.secret=0)",
            "MATCH (n:Visible {p:1}) FOREACH (x IN [1] | SET n.p=2 \
             MERGE (m:Visible {p:n.secret}))",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![VISIBLE],
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (SECRET, CanonicalScalar::Int(99)),
                ],
            );
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{text}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "{text}"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_foreach_merge_refreshes_pattern_values_with_cumulative_limits() {
    let ((), report) = run_async_under_lab(0xf0ea_c009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "MATCH (n:Visible {p:0}) FOREACH (x IN [1,2,3] | SET n.p=n.p+1 \
             MERGE (m:Visible {p:10+n.p})) RETURN n.p AS value",
        );
        let mut measured = None;
        for attempt in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![VISIBLE], vec![(P, CanonicalScalar::Int(0))]);
            db.write(&commit, seed).await.unwrap();
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut bounded = GraphInsertPolicy::new(policy(1).query, 3, 0).with_mutation_limit(3);
            if attempt == 1 {
                bounded.max_vertices = 2;
            } else if attempt == 2 {
                bounded.max_mutation_effects = 2;
            } else if let Some((work, scratch)) = measured {
                bounded.query.evaluator.max_work_units = work - u64::from(attempt == 3);
                bounded.query.evaluator.max_scratch_entries = scratch - u64::from(attempt == 4);
            }
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if attempt == 0 {
                let (stats, rows, _) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (3, 0));
                assert_eq!(rows.rows.result_rows, 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(3))]
                );
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                let vertices = db.vertices().unwrap();
                assert_eq!(vertices.len(), 4);
                for key in [3, 11, 12, 13] {
                    assert_eq!(
                        vertices
                            .iter()
                            .filter(|vertex| {
                                vertex.props == vec![(P, CanonicalScalar::Int(key).into())]
                            })
                            .count(),
                        1,
                        "each MERGE key reads the current staged n.p"
                    );
                }
                assert!(stats.evaluator.work_units > 0);
                assert!(stats.evaluator.scratch_entries > 0);
                measured = Some((stats.evaluator.work_units, stats.evaluator.scratch_entries));
            } else {
                assert!(result.is_err(), "limit attempt {attempt} must refuse");
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert!(db.edges().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        assert!(
            db.execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain("FOREACH (x IN [1,0] | MERGE (n:Visible {p:10/x})) RETURN 1 AS done"),
                policy(1),
                || NOW,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_nested_foreach_restores_each_scope_and_outer_occurrence() {
    let ((), report) = run_async_under_lab(0xf0ea_c00a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 2), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let (stats, rows, completion) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "UNWIND [99,99] AS x FOREACH (x IN [1,2] | \
                     FOREACH (x IN [x,x+10] | CREATE (:Visible {p:x})) \
                     CREATE (:Visible {p:x+100})) RETURN x",
                ),
                GraphInsertPolicy::new(policy(2).query, 12, 0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 12);
        assert_eq!(rows.rows.result_rows, 2);
        assert_eq!(rows.value.len(), 2);
        for row in rows.value {
            assert_eq!(
                row.values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(99))]
            );
        }
        let vertices = db.vertices().unwrap();
        assert_eq!(vertices.len(), 12);
        for key in [1, 2, 11, 12, 101, 102] {
            assert_eq!(
                vertices
                    .iter()
                    .filter(|vertex| {
                        vertex.props == vec![(P, CanonicalScalar::Int(key).into())]
                    })
                    .count(),
                2,
                "inner x, parent x and the original x have separate scopes"
            );
        }
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!(db.delta_since(before).unwrap().count(), 1);

        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "CREATE (n:Visible {p:0}) FOREACH (x IN [1,2] | \
                     FOREACH (y IN range(1,n.p+1) | SET n.p=n.p+1)) RETURN n.p",
                ),
                policy(2).with_mutation_limit(3),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(3))]
        );
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(3).into())],
            "each child collection is fresh at entry and pinned until that child finishes"
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_nested_foreach_preflights_descendants_beneath_empty_parents() {
    let ((), report) = run_async_under_lab(0xf0ea_c00b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        for (text, rights, expected) in [
            (
                "CREATE (n:Visible {p:0}) FOREACH (x IN [] | \
                 FOREACH (y IN [] | SET n.secret=1))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (n:Visible {p:0}) FOREACH (x IN [] | \
                 FOREACH (y IN n.secret | CREATE (:Visible {p:1})))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (n:Visible) FOREACH (x IN [] | \
                 FOREACH (y IN [] | MERGE (:Hidden {p:y})))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (a:Visible),(b:Visible) FOREACH (x IN [] | \
                 FOREACH (y IN [] | MERGE (a)-[:S]->(b)))",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "CREATE (n:Visible) FOREACH (x IN [] | \
                 FOREACH (y IN [] | MERGE (:Visible {p:y})))",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let token = issuer.issue_at(&grant(rights, 0), NOW).unwrap();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(expected), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            let mut pristine = Database::open_memory(&commit, keys()).await.unwrap();
            let request = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
            assert_eq!(
                db.allocate_identity(&query, request).unwrap(),
                pristine.allocate_identity(&query, request).unwrap(),
                "a denied descendant must precede every prefix identity: {text}"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (stats, rows, _) = db
            .execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "FOREACH (x IN [1,2] | FOREACH (y IN [3,4] | \
                     CREATE (:Visible {p:x+y})))",
                ),
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 4);
        assert!(rows.value.is_empty());
        assert_eq!(rows.rows.result_rows, 0);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_nested_foreach_keeps_on_admission_branch_selected() {
    let ((), report) = run_async_under_lab(0xf0ea_c00c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&merge_grant(0), NOW).unwrap();
        for (text, created, visits) in [
            (
                "FOREACH (x IN [1,1] | FOREACH (y IN [x,x] | \
                 MERGE (n:Visible {p:y}) ON CREATE SET n.secret=0 \
                 ON MATCH SET n.q=n.q+1))",
                0,
                4,
            ),
            (
                "FOREACH (x IN [2,3] | FOREACH (y IN [x] | \
                 MERGE (n:Visible {p:y}) ON CREATE SET n.q=7 ON MATCH SET n.secret=0))",
                2,
                0,
            ),
            (
                "FOREACH (x IN [] | FOREACH (y IN [2] | \
                 MERGE (n:Visible {p:y}) ON CREATE SET n.secret=0))",
                0,
                0,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![VISIBLE],
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (SECRET, CanonicalScalar::Int(99)),
                    (Q, CanonicalScalar::Int(0)),
                ],
            );
            db.write(&commit, seed).await.unwrap();
            let (stats, rows, _) = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0).with_mutation_limit(4),
                    || NOW,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(stats.created_vertices, created);
            assert!(rows.value.is_empty());
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (SECRET, CanonicalScalar::Int(99).into()),
                    (Q, CanonicalScalar::Int(visits).into()),
                ]
            );
            if created == 2 {
                let vertices = db.vertices().unwrap();
                assert_eq!(vertices.len(), 3);
                for key in [2, 3] {
                    assert_eq!(
                        vertices
                            .iter()
                            .filter(|vertex| {
                                vertex.props
                                    == vec![
                                        (P, CanonicalScalar::Int(key).into()),
                                        (Q, CanonicalScalar::Int(7).into()),
                                    ]
                            })
                            .count(),
                        1
                    );
                }
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        for text in [
            "FOREACH (x IN [1,2] | FOREACH (y IN [x] | \
             MERGE (n:Visible {p:y}) ON MATCH SET n.q=n.q+1 ON CREATE SET n.secret=0))",
            "FOREACH (x IN [2] | FOREACH (y IN [x,x] | \
             MERGE (n:Visible {p:y}) ON CREATE SET n.q=7 ON MATCH SET n.secret=0))",
            "CREATE (a:Visible {p:10}),(b:Visible {p:20}) \
             FOREACH (x IN [1] | FOREACH (y IN [1,1] | \
             MERGE (a)-[e:R {p:y}]->(b) ON MATCH SET e.secret=0))",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![VISIBLE],
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(0))],
            );
            db.write(&commit, seed).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let error = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &chain(text),
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::ScopeDenied),
                "{text}: {error:?}"
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "a denied branch in a later parent or child iteration aborts all prior work"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_nested_foreach_spends_one_statement_allowance() {
    let ((), report) = run_async_under_lab(0xf0ea_c00d, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "FOREACH (x IN [1,2] | FOREACH (y IN [1,2] | \
             CREATE (n:Visible {p:10*x+y}) SET n.p=n.p+1)) RETURN 1 AS done",
        );
        let mut measured = None;
        for attempt in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut bounded = GraphInsertPolicy::new(policy(1).query, 4, 0).with_mutation_limit(4);
            if attempt == 1 {
                bounded.max_vertices = 3;
            } else if attempt == 2 {
                bounded.max_mutation_effects = 3;
            } else if let Some((work, scratch)) = measured {
                bounded.query.evaluator.max_work_units = work - u64::from(attempt == 3);
                bounded.query.evaluator.max_scratch_entries = scratch - u64::from(attempt == 4);
            }
            let result = db
                .execute_graph_write_chain_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    bounded,
                    || NOW,
                )
                .await;
            if attempt == 0 {
                let (stats, rows, completion) = result.unwrap();
                assert_eq!((stats.created_vertices, stats.created_edges), (4, 0));
                assert_eq!(rows.rows.result_rows, 1);
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(1))]
                );
                let vertices = db.vertices().unwrap();
                assert_eq!(vertices.len(), 4);
                for key in [12, 13, 22, 23] {
                    assert_eq!(
                        vertices
                            .iter()
                            .filter(|vertex| {
                                vertex.props == vec![(P, CanonicalScalar::Int(key).into())]
                            })
                            .count(),
                        1
                    );
                }
                assert!(
                    matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                    if commit_seq.0 == before.0 + 1)
                );
                assert!(stats.evaluator.work_units > 0);
                assert!(stats.evaluator.scratch_entries > 0);
                measured = Some((stats.evaluator.work_units, stats.evaluator.scratch_entries));
            } else {
                assert!(
                    result.is_err(),
                    "nested limit attempt {attempt} must refuse"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert!(db.edges().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        assert!(
            db.execute_graph_write_chain_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &chain(
                    "FOREACH (x IN [1,0] | FOREACH (y IN [1,2] | \
                 CREATE (:Visible {p:y/x}))) RETURN 1 AS done",
                ),
                policy(1),
                || NOW,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_nested_foreach_cancellation_and_expiry_abort_every_frame() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ((), report) = run_async_under_lab(0xf0ea_c00e, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let program = chain(
            "FOREACH (x IN [1,2] | FOREACH (y IN [1,2] | \
             CREATE (n:Visible {p:x+y}) SET n.p=n.p+1)) RETURN 1 AS done",
        );
        let observed = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&observed));
        let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
        let (stats, _, _) = baseline
            .execute_graph_write_chain_authorized(
                &txn,
                &controlled,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 4);
        let calls = observed.calls();
        assert!(calls > 3);
        for stop in [calls / 2, calls - 1] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            assert!(
                db.execute_graph_write_chain_authorized(
                    &txn,
                    &controlled,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &program,
                    policy(1),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(probe.calls(), stop);
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }

        let expired = Arc::new(AtomicBool::new(false));
        let clock = Arc::clone(&expired);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &program,
                policy(1),
                move || {
                    if clock.load(Ordering::Relaxed) {
                        10_000
                    } else {
                        NOW
                    }
                },
                |rows, _| {
                    assert_eq!(rows.len(), 1);
                    // Every parent and child has finished. Reauthorization
                    // must still precede the one publication boundary.
                    expired.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(expired.load(Ordering::Relaxed));
        assert_eq!(authorization(&error), Some(Error::Expired));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
