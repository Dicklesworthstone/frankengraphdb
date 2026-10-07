//! Projected mutation results retain masked inputs and native atomic completion.
use super::*;
use fgdb_gql::PreparedGraphMutationQueryText;
use fgdb_gql::algebra::GraphValue;

fn prepared(text: &str) -> PreparedGraphMutationQuery {
    PreparedGraphMutationQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn auth(error: QueryFault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphMutationQueryError::Mutation(GraphMutationError::Source(
            WriteTxnError::Authorization(error),
        ))) => error,
        other => panic!("expected authorization failure, got {other:?}"),
    }
}

#[test]
fn projected_rows_see_simultaneous_assignments_and_only_masked_inputs() {
    lab(0xb951, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = prepared(
            "MATCH (a:Visible)-[e:R]->(b:Visible) WHERE a.p = 10 \
             SET a.p = a.p + 1, a.q = a.p, e.p = e.p + 5 \
             RETURN a.p AS p, a.q AS q, e.p AS edge, a.secret AS secret ORDER BY edge",
        );
        let (stats, result, completion) = db
            .execute_graph_mutation_query_authorized(
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
        assert_eq!(stats.effects, 4);
        let expected = vec![
            int(11),
            int(10),
            int(7),
            GraphValue::Scalar(CanonicalScalar::Null),
        ];
        assert_eq!(result.value.len(), 2);
        for row in result.value {
            assert_eq!(row.values(), &expected);
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: db.frontier().unwrap()
            }
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(11)),
                (SECRET, CanonicalScalar::Int(71)),
                (Q, CanonicalScalar::Int(10))
            ]
        );
        assert_eq!(
            db.vertex(VId(3)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(30))]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn projected_row_limit_and_scope_refusals_never_publish_effects() {
    lab(0xb952, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        for (text, zero_rows) in [
            (
                "MATCH (a:Visible) WHERE a.p = 10 SET a.p = 99 RETURN a.p",
                true,
            ),
            (
                "MATCH (a:Visible) WHERE a.p = 10 SET a.p = 99, a.secret = 71 RETURN a.p",
                false,
            ),
            (
                "MATCH (a:Visible) WHERE a.p = 10 DETACH DELETE a RETURN a",
                false,
            ),
            (
                "MATCH (a:Visible) WHERE a.p = 10 SET a.p = 99 RETURN 1 / 0",
                false,
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut scope = grant();
            if zero_rows {
                scope.limits.max_rows = 0;
            }
            let token = authority.issue_at(&scope, NOW).unwrap();
            let error = db
                .execute_graph_mutation_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &prepared(text),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            if zero_rows {
                assert_eq!(auth(error), Error::LimitExceeded(LimitDimension::Rows));
            } else if text.contains("DETACH DELETE") {
                assert_eq!(auth(error), Error::ScopeDenied);
            }
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
}

#[test]
fn limit_zero_still_updates_and_remove_returns_null_without_empty_commit() {
    lab(0xb953, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let authority = authority();
        let mut scope = grant();
        scope.limits.max_rows = 0;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let before = db.frontier().unwrap();
        let (_, rows, completion) = db
            .execute_graph_mutation_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MATCH (a:Visible) SET a.q = a.p RETURN a.q LIMIT 0"),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        for (id, value) in [(1, 10), (2, 20)] {
            assert!(
                db.vertex(VId(id))
                    .unwrap()
                    .unwrap()
                    .props
                    .contains(&(Q, CanonicalScalar::Int(value)))
            );
        }
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (_, rows, _) = db
            .execute_graph_mutation_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MATCH (a:Visible) WHERE a.p = 10 REMOVE a.q RETURN a.q"),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Null)]
        );
        let before = db.frontier().unwrap();
        let (_, rows, completion) = db
            .execute_graph_mutation_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MATCH (a:Visible) WHERE a.p = 404 SET a.q = 1 RETURN a.q"),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
    });
}

#[test]
fn projected_mutation_rechecks_expiry_through_final_commit_admission() {
    lab(0xb954, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = prepared("MATCH (a:Visible) SET a.q = a.p + 1 RETURN a.q");
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_mutation_query_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &statement,
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
                .execute_graph_mutation_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &statement,
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
            assert_eq!(auth(error), Error::Expired);
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
fn unrestricted_detach_returns_deleted_identity_after_atomic_cascade() {
    lab(0xb955, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let authority = authority();
        let token = authority
            .issue_at(
                &Grant {
                    labels: Scope::All,
                    relations: Scope::All,
                    properties: Scope::All,
                    ..grant()
                },
                NOW,
            )
            .unwrap();
        let before = db.frontier().unwrap();
        let (_, rows, completion) = db
            .execute_graph_mutation_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MATCH (a:Visible) WHERE a.p = 10 DETACH DELETE a RETURN a"),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(rows.value[0].values(), &[GraphValue::Vertex(VId(1))]);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edges().unwrap().is_empty());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert!(db.vertex(VId(3)).unwrap().is_some());
    });
}

#[test]
fn cancelled_mutation_returning_discards_workspace_and_rows() {
    let ((), report) = run_async_under_lab(0xb956, |root| async move {
        for cutoff in [1, 20] {
            let mut task = root
                .spawn(move |child| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&child);
                    let (commit, query, txn) =
                        (contexts.commit(), contexts.query(), contexts.txn());
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit, true).await;
                    let before = (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap(),
                    );
                    let authority = authority();
                    let token = authority.issue_at(&grant(), NOW).unwrap();
                    let mut calls = 0;
                    let error = db
                        .execute_graph_mutation_query_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &token,
                            "main",
                            &prepared(
                                "MATCH (a:Visible)-[e:R]->(b:Visible) SET a.q = a.p + 1 RETURN a.q",
                            ),
                            policy(),
                            || {
                                calls += 1;
                                if calls == cutoff {
                                    child.cancel_with(
                                        asupersync::CancelKind::User,
                                        Some("mutation RETURN cancellation"),
                                    );
                                }
                                NOW
                            },
                        )
                        .await
                        .unwrap_err();
                    assert!(matches!(
                        error,
                        GqlQueryError::Interrupted(WriteTxnError::Interrupted(_))
                            | GqlQueryError::Source(GraphMutationQueryError::Mutation(
                                GraphMutationError::Source(WriteTxnError::Interrupted(_))
                            ))
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
                })
                .unwrap();
            assert_eq!(task.join(&root).await, Ok(()));
        }
        assert!(root.checkpoint().is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
