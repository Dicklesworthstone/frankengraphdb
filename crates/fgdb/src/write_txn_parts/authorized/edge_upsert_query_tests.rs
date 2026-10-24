//! The real masked upsert, native projection and Chronicle-backed memory VFS.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);
const NOW: u64 = 100;
const HEAD: &str = "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b)";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x92; 32], NS, [0x94; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9993), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant(rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: rows,
        },
        expires_at_ms: 10_000,
    }
}
fn policy(rows: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000)),
        100,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn prepared(text: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(text, S, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn lab<F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, secret: Option<CanonicalScalar>) {
    let mut batch = WriteBatch::new(S);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(20))]);
    batch.create_vertex(VId(3), vec![H], vec![(P, CanonicalScalar::Int(10))]);
    // Duplicate visible occurrences collapse under the explicitly singular API.
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    // A hidden endpoint must not introduce a second pair or an ambiguity.
    batch.add_edge(EId(90), VId(1), VId(3), vec![]);
    db.write(cx, batch).await.unwrap();
    if let Some(secret) = secret {
        let mut batch = WriteBatch::new(R);
        batch.add_edge(
            EId(13),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(7)), (SECRET, secret)],
        );
        db.write(cx, batch).await.unwrap();
    }
}
fn authorization(error: QueryFault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
            GraphEdgeUpsertError::Staging(WriteTxnError::Authorization(error)),
        ))
        | GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(GraphEdgeUpsertError::Merge(
            fgdb_gql::GraphEdgeMergeError::Source(WriteTxnError::Authorization(error)),
        ))) => error,
        other => panic!("expected authorization refusal: {other:?}"),
    }
}

#[test]
fn projected_branches_read_final_state_and_hidden_payloads_never_enter_return() {
    lab(0xa9c1, |contexts| async move {
        let authority = authority();
        let token = authority.issue_at(&grant(1), NOW).unwrap();
        let query = prepared(&format!(
            "{HEAD} ON CREATE SET r.p=4 ON MATCH SET r.p=r.p+3 \
            SET r.p=r.p*2 RETURN r.p AS value,r.secret AS hidden"
        ));
        let mut matched_cost = None;
        // The large hidden payload is half the 16 KiB stored-object ceiling
        // (strata's MAX_STORED_OBJECT_BYTES): a row past it is refused at
        // preparation (eb4bf6ec), so the 32,000-byte text this law used no
        // longer reaches the upsert it is about.
        let large = 8_192;
        for secret in [
            None,
            Some(CanonicalScalar::Int(99)),
            Some(CanonicalScalar::ucs_basic_text(&"x".repeat(large)).unwrap()),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            seed(&mut db, &contexts.commit(), secret.clone()).await;
            let vertices = db.vertices().unwrap();
            let before = db.frontier().unwrap();
            let expected = if secret.is_some() { 20 } else { 8 };
            let (stats, outcome, result, completion) = db
                .execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(1),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(outcome.created(), secret.is_none());
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].values(),
                &[
                    GraphValue::Scalar(CanonicalScalar::Int(expected)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
            );
            assert!(result.evaluator.work_units > stats.evaluator.work_units);
            if secret.is_some() {
                let costs = (result.rows, result.evaluator);
                if let Some(prior) = matched_cost {
                    assert_eq!(costs, prior);
                }
                matched_cost = Some(costs);
            }
            let mut expected_props = vec![(P, CanonicalScalar::Int(expected).into())];
            if let Some(secret) = secret {
                expected_props.push((SECRET, secret.into()));
            }
            assert_eq!(
                db.edge(outcome.edge().unwrap()).unwrap().unwrap().props,
                expected_props
            );
            assert_eq!(db.vertices().unwrap(), vertices);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: db.frontier().unwrap()
                }
            );
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
}

#[test]
fn final_row_budgets_do_not_charge_private_matches_and_late_projection_is_atomic() {
    lab(0xa9c2, |contexts| async move {
        let authority = authority();
        let token = authority.issue_at(&grant(0), NOW).unwrap();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        seed(&mut db, &contexts.commit(), None).await;
        // Zero native and signed output rows do not suppress the actual write.
        let query = prepared(&format!("{HEAD} ON CREATE SET r.p=5 RETURN r.p LIMIT 0"));
        let (_, _, rows, _) = db
            .execute_graph_edge_upsert_query_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &query,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for (signed, native) in [(0, 1), (1, 0)] {
            let token = authority.issue_at(&grant(signed), NOW).unwrap();
            let query = prepared(&format!("{HEAD} ON MATCH SET r.p=6 RETURN r.p"));
            let error = db
                .execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(native),
                    || NOW,
                )
                .await
                .unwrap_err();
            if signed == 0 {
                assert!(matches!(authorization(error), Error::LimitExceeded(_)));
            } else {
                assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}");
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        let query = prepared(&format!("{HEAD} ON MATCH SET r.p=0 RETURN 10/r.p LIMIT 0"));
        let error = db
            .execute_graph_edge_upsert_query_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &query,
                policy(0),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(_))
            ),
            "{error:?}"
        );
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
    });
}

#[test]
fn preflight_and_attempted_fields_stay_authorized_even_with_empty_output() {
    lab(0xa9c3, |contexts| async move {
        let authority = authority();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        seed(&mut db, &contexts.commit(), None).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let empty = prepared(
            "MATCH (a)-[:S]->(b) WHERE a.p=999 MERGE (a)-[r:R]->(b) \
            ON CREATE SET r.p=1 RETURN r.p LIMIT 0",
        );
        for rights in [Rights::Read, Rights::Write] {
            let token = authority
                .issue_at(&Grant { rights, ..grant(0) }, NOW)
                .unwrap();
            let error = db
                .execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &empty,
                    policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::PermissionDenied);
        }
        let denied = Grant {
            relations: Scope::only([S]),
            ..grant(0)
        };
        let token = authority.issue_at(&denied, NOW).unwrap();
        let error = db
            .execute_graph_edge_upsert_query_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &empty,
                policy(0),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(error), Error::ScopeDenied);
        let token = authority.issue_at(&grant(0), NOW).unwrap();
        for text in [
            format!("{HEAD} ON CREATE SET r.p=1,r.secret=99 RETURN r.p LIMIT 0"),
            "MATCH (a)-[:S]->(b) MERGE (a)-[r:R {secret:99}]->(b) ON CREATE SET r.p=1 RETURN r.p LIMIT 0".into(),
        ] {
            let error = db.execute_graph_edge_upsert_query_authorized(
                &contexts.txn(), &contexts.query(), &contexts.commit(), &authority, &token,
                "main", &prepared(&text), policy(0), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
        }
        let (_, outcome, result, _) = db
            .execute_graph_edge_upsert_query_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &empty,
                policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
        assert!(result.value.is_empty());
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
    });
}

#[test]
fn ambiguous_pairs_and_parallel_matches_are_not_arbitrarily_selected() {
    lab(0xa9c4, |contexts| async move {
        let authority = authority();
        let token = authority.issue_at(&grant(10), NOW).unwrap();
        let query = prepared(&format!(
            "{HEAD} ON CREATE SET r.p=1 ON MATCH SET r.p=2 RETURN r.p"
        ));
        for parallel in [false, true] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            seed(&mut db, &contexts.commit(), Some(CanonicalScalar::Int(99))).await;
            let mut batch = WriteBatch::new(if parallel { R } else { S });
            batch.add_edge(
                EId(14),
                VId(1),
                if parallel { VId(2) } else { VId(1) },
                vec![],
            );
            db.write(&contexts.commit(), batch).await.unwrap();
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let error = db
                .execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(10),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                        GraphEdgeUpsertError::Merge(_)
                    ))
                ),
                "{error:?}"
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
    });
}

#[test]
fn late_expiry_and_each_native_allowance_refuse_before_publication() {
    lab(0xa9c5, |contexts| async move {
        let authority = authority();
        let token = authority.issue_at(&grant(1), NOW).unwrap();
        let query = prepared(&format!("{HEAD} ON CREATE SET r.p=9 RETURN r.p"));
        let calls = AtomicU64::new(0);
        let mut control = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        seed(&mut control, &contexts.commit(), None).await;
        control
            .execute_graph_edge_upsert_query_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &query,
                policy(1),
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    NOW
                },
            )
            .await
            .unwrap();
        let total = calls.load(Ordering::Relaxed);
        assert!(total > 2);
        for stop in [0, total / 2, total - 1] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            seed(&mut db, &contexts.commit(), None).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let calls = AtomicU64::new(0);
            let error = db
                .execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(1),
                    || {
                        if calls.fetch_add(1, Ordering::Relaxed) >= stop {
                            10_001
                        } else {
                            NOW
                        }
                    },
                )
                .await
                .unwrap_err();
            let _ = authorization(error);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
        }
        for dimension in 0..5 {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            seed(&mut db, &contexts.commit(), None).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut bounded = policy(1);
            match dimension {
                0 => bounded.merge.query = GqlQueryPolicy::new(0, 1, 1_000_000, 100_000),
                1 => bounded.merge.query.evaluator.max_work_units = 0,
                2 => bounded.merge.query.evaluator.max_scratch_entries = 0,
                3 => bounded.merge.max_created_edges = 0,
                _ => bounded.max_actions = 0,
            }
            assert!(
                db.execute_graph_edge_upsert_query_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &authority,
                    &token,
                    "main",
                    &query,
                    bounded,
                    || NOW,
                )
                .await
                .is_err(),
                "dimension {dimension}"
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
    });
}

#[test]
fn checked_output_refusal_and_zero_row_admission_stay_inside_rollback_boundary() {
    lab(0xa9c6, |contexts| async move {
        let authority = authority();
        let token = authority.issue_at(&grant(1), NOW).unwrap();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        seed(&mut db, &contexts.commit(), None).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let query = prepared(&format!("{HEAD} ON CREATE SET r.p=9 RETURN r.p"));
        let mut calls = 0;
        let error = db
            .execute_graph_edge_upsert_query_authorized_checked(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &token,
                "main",
                &query,
                policy(1),
                || NOW,
                |rows, work| {
                    calls += 1;
                    assert_eq!(rows.len(), 1);
                    work(1)?;
                    Err(WriteTxnError::AuthorizedMutationRefused)
                },
            )
            .await
            .unwrap_err();
        assert_eq!(calls, 1);
        assert!(
            matches!(
                error,
                GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(
                    GraphEdgeUpsertError::Staging(WriteTxnError::AuthorizedMutationRefused)
                ))
            ),
            "{error:?}"
        );
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );

        let zero = prepared(&format!("{HEAD} ON CREATE SET r.p=9 RETURN r.p LIMIT 0"));
        let mut admitted = 0;
        let (_, outcome, rows, completion) = db
            .execute_graph_edge_upsert_query_authorized_checked(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &authority,
                &authority.issue_at(&grant(0), NOW).unwrap(),
                "main",
                &zero,
                policy(0),
                || NOW,
                |rows, _| {
                    admitted += 1;
                    assert!(rows.is_empty());
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(admitted, 1);
        assert!(rows.value.is_empty());
        assert!(outcome.created());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
}
