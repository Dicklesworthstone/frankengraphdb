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

struct PropertyMapCatalog;
impl fgdb_gql::GraphSymbolResolver for PropertyMapCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        let mut catalog = fgdb_gql::ReverseSymbolCatalog::new();
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        // SECRET deliberately has no name. A capability-hidden field must
        // not become a naming refusal or add payload charges.
        Some(catalog)
    }
}

fn prepared_map(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, PropertyMapCatalog)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn merge_returning_whole_maps_mask_before_naming_and_preserve_atomic_authority() {
    lab(0xb966, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let statement = prepared_map(
            "MERGE (n:Visible {p:7}) ON MATCH SET n.q=12 \
             RETURN properties(n) AS props, keys(n) AS names, n{.*,q:13} AS projected",
        );
        let named = |q| GraphValue::map(vec![("p".into(), int(7)), ("q".into(), int(q))]).unwrap();
        let names = GraphValue::List(
            ["p", "q"]
                .into_iter()
                .map(|name| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap()))
                .collect(),
        );
        let mut observed = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut fields = vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(2))];
            if hidden {
                fields.push((SECRET, CanonicalScalar::bytes(vec![0x75; 2_048]).unwrap()));
            }
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![L], fields);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let (_, outcome, rows, completion) = db
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
            assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
            assert_eq!(
                rows.value[0].values(),
                &[named(12), names.clone(), named(13)]
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            observed.push((rows.value, rows.evaluator));
            let fields = db.vertex(VId(1)).unwrap().unwrap().props;
            assert!(fields.contains(&(Q, CanonicalScalar::Int(12).into())));
            assert_eq!(fields.iter().any(|(key, _)| *key == SECRET), hidden);
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            for text in [
                "MERGE (n:Visible {p:7}) SET n.q=90 RETURN n{.*,failed:1/0} AS props LIMIT 0",
                "MERGE (n:Visible {p:7}) SET n.secret=1 RETURN properties(n) AS props",
            ] {
                assert!(
                    db.execute_graph_vertex_upsert_query_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &prepared_map(text),
                        upsert_policy(),
                        || NOW,
                    )
                    .await
                    .is_err()
                );
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            }
            if hidden {
                let mut full = grant();
                full.properties = Scope::All;
                let full = authority.issue_at(&full, NOW).unwrap();
                let failed = db
                    .execute_graph_vertex_upsert_query_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &full,
                        "main",
                        &statement,
                        upsert_policy(),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(
                    failed,
                    GqlQueryError::Source(fgdb_gql::GraphVertexUpsertError::Staging(
                        WriteTxnError::Read(crate::ReadError::UnmappedProperty(SECRET))
                    ))
                ));
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(observed[0], observed[1]);
    });
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
                .contains(&(SECRET, CanonicalScalar::Int(71).into()))
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
        let statement = prepared(
            "MERGE (n:Visible {p:99}) ON CREATE SET n.q=5 ON MATCH SET n.q=n.q+1 \
             RETURN n.q LIMIT 0",
        );
        // The signed output limit AND the caller's local output allowance are
        // zero. Neither limits the private MATCH or creation unit input.
        let mut zero_rows = upsert_policy();
        zero_rows.merge.query.rows = fgdb_gql::GqlExecutionBudget::new(
            zero_rows.merge.query.rows.max_snapshot_records().unwrap(),
            0,
        );
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut count = 0;
        let (_, created, rows, completion) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                zero_rows,
                || {
                    count += 1;
                    NOW
                },
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        assert!(created.created());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        let (_, matched, rows, completion) = db
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &statement,
                zero_rows,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(matched, GraphVertexMergeOutcome::Matched(created.vertex()));
        assert!(rows.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert!(
            db.vertex(created.vertex())
                .unwrap()
                .unwrap()
                .props
                .contains(&(Q, CanonicalScalar::Int(6).into()))
        );
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
                    zero_rows,
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

#[test]
fn aggregate_write_returns_mask_before_grouping_and_charge_only_final_rows() {
    lab(0xb965, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let mut scope = grant();
        scope.limits.max_rows = 1;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let mutation = fgdb_gql::PreparedGraphMutationQueryText::prepare(
            "MATCH (n:Visible) SET n.q=n.p+1 \
             RETURN count(*) AS c,sum(n.q) AS total,count(n.secret) AS secrets",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut mutation_policy = policy();
        mutation_policy.query.rows = fgdb_gql::GqlExecutionBudget::new(10_000, 1);
        let mut merge_policy = upsert_policy();
        merge_policy.merge.query.rows = fgdb_gql::GqlExecutionBudget::new(10_000, 1);
        let mut observed = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, hidden).await;
            let before = db.frontier().unwrap();
            let (_, result, completion) = db
                .execute_graph_mutation_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &mutation,
                    mutation_policy,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.value[0].values(), &[int(2), int(32), int(0)]);
            assert_eq!(result.rows.result_rows, 1);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let (_, outcome, merged, _) = db
                .execute_graph_vertex_upsert_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &prepared(
                        "MERGE (n:Visible {p:10}) ON MATCH SET n.q=n.q+100 \
                         SET n.q=n.q+1 \
                         RETURN count(*) AS c,sum(n.q) AS total,count(n.secret) AS secrets",
                    ),
                    merge_policy,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
            assert_eq!(merged.value.len(), 1);
            assert_eq!(merged.value[0].values(), &[int(1), int(112), int(0)]);
            assert_eq!(merged.rows.result_rows, 1);
            if hidden {
                assert_eq!(
                    db.vertex(VId(3)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(30).into())]
                );
                assert!(
                    db.vertex(VId(1))
                        .unwrap()
                        .unwrap()
                        .props
                        .contains(&(SECRET, CanonicalScalar::Int(71).into(),))
                );
                let second = db.vertex(VId(2)).unwrap().unwrap();
                assert!(second.labels.contains(&HIDDEN));
                assert!(
                    second
                        .props
                        .contains(&(SECRET, CanonicalScalar::Int(72).into()))
                );
            }
            observed.push((result.value, merged.value));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(observed[0], observed[1]);
    });
}

#[test]
fn aggregate_merge_failure_with_zero_output_allowance_preserves_committed_state() {
    lab(0xb966, |contexts| async move {
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let mut scope = grant();
        scope.limits.max_rows = 0;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let mut zero_rows = upsert_policy();
        zero_rows.merge.query.rows = fgdb_gql::GqlExecutionBudget::new(10_000, 0);
        for text in [
            "MERGE (n:Visible {p:10}) ON MATCH SET n.q='bad' \
             RETURN sum(n.q) AS total LIMIT 0",
            "MERGE (n:Visible {p:99}) ON CREATE SET n.q='bad' \
             RETURN sum(n.q) AS total LIMIT 0",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let error = db
                .execute_graph_vertex_upsert_query_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &prepared(text),
                    zero_rows,
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("SUM"), "{error}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before,
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
