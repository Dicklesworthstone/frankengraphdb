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
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xe1; 32]);
const R: RelationId = RelationId(1);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
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

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
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
