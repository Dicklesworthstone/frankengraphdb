//! MERGE RETURN uses the same selected vertex, masked fields and one completion.
use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GraphVertexMergeOutcome, GraphVertexMergePolicy, GraphVertexUpsertPolicy,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
};

fn prepared(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn upsert_policy() -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy().query), 100)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn merge_returning_projects_one_chosen_vertex_after_all_action_clauses() {
    lab(0xb961, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = prepared(
            "MERGE (n:Visible {p:7}) ON CREATE SET n.q = 1 ON MATCH SET n.q = n.q + 1 SET n.q = n.q + 10 RETURN n, n.q AS q",
        );
        let (_, first, rows, completion) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(matches!(first, GraphVertexMergeOutcome::Created(_)));
        let id = first.vertex();
        assert_eq!(rows.value[0].values(), &[GraphValue::Vertex(id), int(11)]);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let (_, second, rows, _) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(second, GraphVertexMergeOutcome::Matched(id));
        assert_eq!(rows.value[0].values(), &[GraphValue::Vertex(id), int(22)]);
        assert_eq!(db.vertices().unwrap().len(), 1);
        let before = db.frontier().unwrap();
        let (_, outcome, rows, completion) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MERGE (n:Visible {p:7}) RETURN n, n.q AS q"),
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(id));
        assert_eq!(rows.value[0].values(), &[GraphValue::Vertex(id), int(22)]);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn merge_returning_masks_hidden_fields_and_never_matches_hidden_vertices() {
    lab(0xb962, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (_, outcome, rows, _) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared(
                    "MERGE (n:Visible {p:10}) SET n.q = 5 RETURN n.q AS q, n.secret AS secret",
                ),
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(
            rows.value[0].values(),
            &[int(5), GraphValue::Scalar(CanonicalScalar::Null)]
        );
        assert!(
            db.vertex(VId(1))
                .unwrap()
                .unwrap()
                .props
                .contains(&(SECRET, CanonicalScalar::Int(71)))
        );
        // Without the Visible label in MATCH, the hidden vertex would match
        // p=30 in the privileged graph. Masking must remove it before MERGE:
        // the resulting unlabeled creation is then outside this capability.
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        db.execute_graph_vertex_upsert_query_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &prepared("MERGE (n {p:30}) RETURN n"),
            upsert_policy(),
            || NOW,
        )
        .await
        .unwrap_err();
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        let (_, outcome, rows, _) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared("MERGE (n:Visible {p:30}) RETURN n"),
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, GraphVertexMergeOutcome::Created(_)));
        assert_ne!(outcome.vertex(), VId(3));
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Vertex(outcome.vertex())]
        );
        assert_eq!(db.vertex(VId(3)).unwrap().unwrap().labels, vec![HIDDEN]);
    });
}

#[test]
fn merge_return_failure_or_denied_action_rolls_back_all_private_clauses() {
    lab(0xb963, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        for (statement, zero_rows) in [
            ("MERGE (n:Visible {p:10}) SET n.q = 5 RETURN n.q", true),
            (
                "MERGE (n:Visible {p:99}) ON CREATE SET n.q = 5 RETURN n.q",
                true,
            ),
            (
                "MERGE (n:Visible {p:10}) ON MATCH SET n.q = 5 SET n.secret = 71 RETURN n.q",
                false,
            ),
            (
                "MERGE (n:Visible {p:99}) ON CREATE SET n.q = 5 RETURN 1 / 0",
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
            db.execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &prepared(statement),
                upsert_policy(),
                || NOW,
            )
            .await
            .unwrap_err();
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
                "{statement}"
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn merge_limit_zero_commits_but_expiry_before_completion_discards_all_effects() {
    lab(0xb964, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let mut scope = grant();
        scope.limits.max_rows = 0;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let statement =
            prepared("MERGE (n:Visible {p:99}) ON CREATE SET n.q = 5 RETURN n.q LIMIT 0");
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut count = 0;
        let (_, _, rows, completion) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                upsert_policy(),
                || {
                    count += 1;
                    NOW
                },
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(count > 10);
        for cutoff in [1, count / 2, count - 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut calls = 0;
            let error = db
                .execute_graph_vertex_upsert_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &statement,
                    upsert_policy(),
                    || {
                        calls += 1;
                        if calls <= cutoff { NOW } else { 10_000 }
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("expired"), "{error}");
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
