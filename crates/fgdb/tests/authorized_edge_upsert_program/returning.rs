use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GraphEdgeMergePolicy, GraphEdgeUpsertPolicy, PreparedGraphEdgeUpsertQuery,
    PreparedGraphEdgeUpsertQueryText,
};

fn query(suffix: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(&format!("{UPSERT} {suffix}"), R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn query_policy() -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(policy().mutations.query).with_creation_limit(100),
        100,
    )
}

#[test]
fn aggregate_and_distinct_apply_after_every_occurrence_not_before_actions() {
    lab(0x6510, |contexts| async move {
        let (cx, qcx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (suffix, expected, row_limit) in [
            ("RETURN max(r.value) AS value", vec![31], 1),
            (
                "RETURN r.value AS value ORDER BY value",
                vec![12, 12, 22, 22, 31],
                5,
            ),
            (
                "RETURN DISTINCT r.value AS value ORDER BY value",
                vec![12, 22, 31],
                3,
            ),
        ] {
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let token = token.attenuate(Restriction::MaxRows(row_limit)).unwrap();
            let (stats, result, _) = db
                .execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &qcx,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &query(suffix),
                    query_policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(
                (
                    stats.match_selection.result_rows,
                    stats.matched_occurrences,
                    stats.action_effects
                ),
                (3, 5, 5)
            );
            let actual: Vec<_> = result
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect();
            let expected: Vec<_> = expected
                .into_iter()
                .map(|n| vec![GraphValue::Scalar(CanonicalScalar::Int(n))])
                .collect();
            assert_eq!(actual, expected);
            assert_eq!(value(&db, E1), CanonicalScalar::Int(12));
            assert_eq!(value(&db, EId(902)), CanonicalScalar::Int(777));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn final_projection_and_signed_row_failures_roll_back_even_with_limit_zero() {
    lab(0x6511, |contexts| async move {
        let (cx, qcx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (suffix, rows) in [
            ("RETURN 1/(r.value-31) AS value", 10),
            ("RETURN 1/(r.value-31) AS value LIMIT 0", 0),
            ("RETURN r.value AS value", 4),
        ] {
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let token = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            assert!(
                db.execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &qcx,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &query(suffix),
                    query_policy(),
                    || NOW,
                )
                .await
                .is_err()
            );
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
        let (mut db, _, _) = fixture(&cx, true, true).await;
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (_, result, completion) = db
            .execute_graph_edge_upserts_query_authorized(
                &txn,
                &qcx,
                &cx,
                &issuer,
                &zero,
                "main",
                &query("RETURN r.value AS value LIMIT 0"),
                query_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert!(result.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(value(&db, E1), CanonicalScalar::Int(12));
    });
}

#[test]
fn hidden_return_fields_are_null_and_host_admission_cannot_publish_a_refused_write() {
    lab(0x6512, |contexts| async move {
        let (cx, qcx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        for failure in 0..3 {
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut called = 0;
            let result = db
                .execute_graph_edge_upserts_query_authorized_checked(
                    &txn,
                    &qcx,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &query("RETURN r.secret AS hidden"),
                    query_policy(),
                    || NOW,
                    |rows, work| {
                        called += 1;
                        assert_eq!(rows.len(), 5);
                        assert!(
                            rows.iter()
                                .all(|row| row.values()
                                    == [GraphValue::Scalar(CanonicalScalar::Null)])
                        );
                        match failure {
                            0 => Err(WriteTxnError::AuthorizedMutationRefused),
                            1 => work(u64::MAX),
                            _ => {
                                issuer.retire();
                                work(0)
                            }
                        }
                    },
                )
                .await;
            assert!(result.is_err());
            assert_eq!(called, 1);
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
fn empty_input_uses_native_aggregate_semantics_without_a_write_marker() {
    lab(0x6513, |contexts| async move {
        let (cx, qcx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let (stats, rows, completion) = db
            .execute_graph_edge_upserts_query_authorized(
                &txn,
                &qcx,
                &cx,
                &issuer,
                &token,
                "main",
                &query("RETURN max(r.value) AS value"),
                query_policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            (
                stats.created_edges,
                stats.matched_occurrences,
                stats.action_effects
            ),
            (0, 0, 0)
        );
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Null)]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn explicit_unique_query_keeps_its_cardinality_refusal() {
    lab(0x6514, |contexts| async move {
        let (cx, qcx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let (mut db, _, _) = fixture(&cx, true, true).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        let query = query("RETURN r.value AS value");
        let error = db
            .execute_graph_edge_upsert_query_authorized(
                &txn,
                &qcx,
                &cx,
                &issuer,
                &token,
                "main",
                &query,
                query_policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphEdgeUpsertQueryError::Upsert(
                fgdb_gql::GraphEdgeUpsertError::Merge(
                    fgdb_gql::GraphEdgeMergeError::AmbiguousEndpointPairs { .. }
                )
            ))
        ));
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
