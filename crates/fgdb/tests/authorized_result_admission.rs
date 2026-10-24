//! Host result admission shares the native transaction's rollback and permit.
//! The callback sees final projected rows, not a rematch or a committed read.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertPolicy, PreparedGraphInsertQueryText,
    PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};

const NOW: u64 = 100;
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

async fn fixture(commit: &CommitCx) -> (Database<MemVfs>, Authority, CapabilityToken) {
    let namespace = DatabaseSecurityNamespaceId([0x39; 32]);
    let keys = DatabaseKeys::new([0x38; 32], namespace, [0x40; 32]);
    let mut db = Database::open_memory(commit, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(1),
        vec![L],
        vec![(K, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))],
    );
    db.write(commit, batch).await.unwrap();
    let authority = Authority::new(
        AuthKey::from_seed(0x391),
        namespace,
        "test",
        SchemaEpoch(0),
        1,
    )
    .unwrap();
    let token = authority
        .issue_at(
            &Grant {
                branch: "main".into(),
                labels: Scope::All,
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: 100_000,
                },
                expires_at_ms: 1000,
            },
            NOW,
        )
        .unwrap();
    (db, authority, token)
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Create,
    Mutation,
    Merge,
}

impl Kind {
    fn statement(self) -> &'static str {
        match self {
            Self::Create => "CREATE (n:Item {key:2,value:20}) RETURN n.value AS value",
            Self::Mutation => "MATCH (n:Item {key:1}) SET n.value=20 RETURN n.value AS value",
            Self::Merge => "MERGE (n:Item {key:1}) SET n.value=20 RETURN n.value AS value",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn checked(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    txn: &TxnCx,
    authority: &Authority,
    token: &CapabilityToken,
    kind: Kind,
    limit_zero: bool,
    admit: impl FnOnce(
        &[GraphValueRow],
        &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError>,
) -> Result<(Vec<GraphValueRow>, EmbeddedTxnCompletion), String> {
    let text = format!(
        "{}{}",
        kind.statement(),
        if limit_zero { " LIMIT 0" } else { "" }
    );
    let parameters = GqlParameters::new();
    match kind {
        Kind::Create => {
            let prepared = PreparedGraphInsertQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_insert_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphInsertPolicy::new(policy(), 100, 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
        Kind::Mutation => {
            let prepared = PreparedGraphMutationQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_mutation_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphMutationPolicy::new(policy(), 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
        Kind::Merge => {
            let prepared = PreparedGraphVertexUpsertQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_vertex_upsert_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy()), 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, _, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
    }
}

#[test]
fn refusing_any_native_result_discards_all_private_effects_and_pins() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut calls = 0;
            let result = checked(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                kind,
                false,
                |rows, work| {
                    calls += 1;
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        rows[0].values(),
                        &[GraphValue::Scalar(CanonicalScalar::Int(20))]
                    );
                    work(7)?;
                    Err(WriteTxnError::AuthorizedMutationRefused)
                },
            )
            .await;
            assert!(result.is_err(), "{kind:?}");
            assert_eq!(calls, 1);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn successful_admission_preserves_exact_rows_and_limit_zero_still_checks() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            for empty in [false, true] {
                let (mut db, authority, token) = fixture(commit).await;
                let before = db.frontier().unwrap();
                let mut admitted = None;
                let (rows, completion) = checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    empty,
                    |rows, work| {
                        assert!(admitted.is_none());
                        work(1)?;
                        admitted = Some(rows.to_vec());
                        Ok(())
                    },
                )
                .await
                .unwrap();
                assert_eq!(Some(rows.clone()), admitted);
                assert_eq!(rows.is_empty(), empty);
                assert!(matches!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { .. }
                ));
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn admission_uses_the_same_live_work_budget_even_for_an_empty_result() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut entered = false;
            assert!(
                checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    true,
                    |rows, work| {
                        entered = true;
                        assert!(rows.is_empty());
                        work(u64::MAX)
                    },
                )
                .await
                .is_err()
            );
            assert!(entered);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn issuer_retirement_inside_result_admission_is_not_followed_by_commit() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            assert!(
                checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    false,
                    |_, work| {
                        authority.retire();
                        work(0)
                    },
                )
                .await
                .is_err()
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

mod all_target_merge {
    use super::*;

    const SECOND: VId = VId((1_u128 << 96) | 1);
    const HIDDEN: LabelId = LabelId(2);

    async fn seed_more(db: &mut Database<MemVfs>, commit: &CommitCx) {
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(
            SECOND,
            vec![L],
            vec![(K, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(20))],
        );
        batch.create_vertex(
            VId(3),
            vec![HIDDEN],
            vec![(K, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(90))],
        );
        db.write(commit, batch).await.unwrap();
    }

    fn token(authority: &Authority, rows: u64, hide_value: bool) -> CapabilityToken {
        authority
            .issue_at(
                &Grant {
                    branch: "main".into(),
                    labels: Scope::only([L]),
                    relations: Scope::All,
                    properties: if hide_value {
                        Scope::only([K])
                    } else {
                        Scope::All
                    },
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 100_000_000,
                        max_rows: rows,
                    },
                    expires_at_ms: 1000,
                },
                NOW,
            )
            .unwrap()
    }

    fn prepare(text: &str) -> fgdb_gql::PreparedGraphVertexUpsertQuery {
        PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }

    fn upsert_policy(actions: u64) -> GraphVertexUpsertPolicy {
        GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy()), actions)
    }

    fn value(db: &Database<MemVfs>, id: VId) -> CanonicalScalar {
        db.vertex(id)
            .unwrap()
            .unwrap()
            .props
            .into_iter()
            .find(|(key, _)| *key == P)
            .unwrap()
            .1
            .as_scalar()
            .expect("scalar property")
            .clone()
    }

    #[test]
    fn all_visible_targets_update_before_one_final_aggregate_and_unique_api_stays_unique() {
        run(async |commit, query, txn| {
            let (mut db, authority, _) = fixture(commit).await;
            seed_more(&mut db, commit).await;
            let token = token(&authority, 1, false);
            let prepared = prepare(
                "MERGE (n {key:1}) ON MATCH SET n.value=n.value+1 RETURN max(n.value) AS value",
            );
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            assert!(
                db.execute_graph_vertex_upsert_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(2),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            let (stats, result, completion) = db
                .execute_graph_vertex_upserts_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(2),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.action_effects, 2);
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(21))]
            );
            assert_eq!(value(&db, VId(1)), CanonicalScalar::Int(11));
            assert_eq!(value(&db, SECOND), CanonicalScalar::Int(21));
            assert_eq!(value(&db, VId(3)), CanonicalScalar::Int(90));
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, (before.0).0 + 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn limit_zero_and_with_filters_do_not_suppress_actions_or_rematch_changed_keys() {
        run(async |commit, query, txn| {
            for tail in [
                "RETURN n.value AS value LIMIT 0",
                "WITH n.value AS v WHERE v>11 RETURN v",
            ] {
                let (mut db, authority, _) = fixture(commit).await;
                seed_more(&mut db, commit).await;
                let zero = tail.ends_with("LIMIT 0");
                let token = token(&authority, u64::from(!zero), false);
                let prepared = prepare(&format!(
                    "MERGE (n:Item {{key:1}}) ON MATCH SET n.value=n.value+1 SET n.key=2 {tail}",
                ));
                let (_, result, _) = db
                    .execute_graph_vertex_upserts_query_authorized(
                        txn,
                        query,
                        commit,
                        &authority,
                        &token,
                        "main",
                        &prepared,
                        upsert_policy(4),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.value.len(), usize::from(!zero));
                if !zero {
                    assert_eq!(
                        result.value[0].values(),
                        &[GraphValue::Scalar(CanonicalScalar::Int(21))]
                    );
                }
                for (id, expected) in [(VId(1), 11), (SECOND, 21)] {
                    let row = db.vertex(id).unwrap().unwrap();
                    assert!(row.props.contains(&(K, CanonicalScalar::Int(2).into())));
                    assert!(
                        row.props
                            .contains(&(P, CanonicalScalar::Int(expected).into()))
                    );
                }
                assert_eq!(value(&db, VId(3)), CanonicalScalar::Int(90));
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn whole_statement_action_cap_refuses_without_partial_effects_and_can_be_retried() {
        run(async |commit, query, txn| {
            let (mut db, authority, _) = fixture(commit).await;
            seed_more(&mut db, commit).await;
            let token = token(&authority, 2, false);
            let prepared =
                prepare("MERGE (n:Item {key:1}) SET n.value=n.value+1 RETURN n.value AS value");
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            assert!(
                db.execute_graph_vertex_upserts_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(1),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            let (_, result, _) = db
                .execute_graph_vertex_upserts_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(2),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(result.value.len(), 2);
            assert_eq!(value(&db, VId(1)), CanonicalScalar::Int(11));
            assert_eq!(value(&db, SECOND), CanonicalScalar::Int(21));
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn later_projection_and_signed_row_refusals_rollback_every_target() {
        run(async |commit, query, txn| {
            for (tail, rows) in [
                ("RETURN 1/(n.value-21) AS value", 2),
                ("RETURN n.value AS value", 1),
            ] {
                let (mut db, authority, _) = fixture(commit).await;
                seed_more(&mut db, commit).await;
                let token = token(&authority, rows, false);
                let prepared = prepare(&format!(
                    "MERGE (n:Item {{key:1}}) SET n.value=n.value+1 {tail}"
                ));
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                assert!(
                    db.execute_graph_vertex_upserts_query_authorized(
                        txn,
                        query,
                        commit,
                        &authority,
                        &token,
                        "main",
                        &prepared,
                        upsert_policy(2),
                        || NOW,
                    )
                    .await
                    .is_err()
                );
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn hidden_fields_are_null_and_attempted_hidden_noop_writes_refuse() {
        run(async |commit, query, txn| {
            let (mut db, authority, _) = fixture(commit).await;
            seed_more(&mut db, commit).await;
            let token = token(&authority, 2, true);
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let prepared = prepare("MERGE (n {key:1}) RETURN n.value AS value");
            let (_, result, _) = db
                .execute_graph_vertex_upserts_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(0),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(result.value.len(), 2);
            for row in &result.value {
                assert_eq!(row.values(), &[GraphValue::Scalar(CanonicalScalar::Null)]);
            }
            for assignment in ["n.value=n.value", "n.value=10"] {
                // The literal assignment is already the first target's value.
                // Original-intent authorization must still reject that no-op.
                let prepared = prepare(&format!(
                    "MERGE (n {{key:1}}) SET {assignment} RETURN n.value AS value",
                ));
                assert!(
                    db.execute_graph_vertex_upserts_query_authorized(
                        txn,
                        query,
                        commit,
                        &authority,
                        &token,
                        "main",
                        &prepared,
                        upsert_policy(2),
                        || NOW,
                    )
                    .await
                    .is_err()
                );
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn final_admission_owns_the_whole_result_and_retirement_prevents_publication() {
        run(async |commit, query, txn| {
            let (mut db, authority, _) = fixture(commit).await;
            seed_more(&mut db, commit).await;
            let token = token(&authority, 2, false);
            let prepared = prepare(
                "MERGE (n:Item {key:1}) SET n.value=n.value+1 RETURN n.value AS value ORDER BY value",
            );
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut calls = 0;
            assert!(
                db.execute_graph_vertex_upserts_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    upsert_policy(2),
                    || NOW,
                    |rows, work| {
                        calls += 1;
                        assert_eq!(rows.len(), 2);
                        assert_eq!(
                            rows[0].values(),
                            &[GraphValue::Scalar(CanonicalScalar::Int(11))]
                        );
                        assert_eq!(
                            rows[1].values(),
                            &[GraphValue::Scalar(CanonicalScalar::Int(21))]
                        );
                        authority.retire();
                        work(0)
                    },
                )
                .await
                .is_err()
            );
            assert_eq!(calls, 1);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn an_empty_visible_match_creates_once_and_the_next_call_matches_it() {
        run(async |commit, query, txn| {
            let (mut db, authority, _) = fixture(commit).await;
            let mut hidden = WriteBatch::new(R);
            hidden.create_vertex(VId(3), vec![HIDDEN], vec![(K, CanonicalScalar::Int(2))]);
            db.write(commit, hidden).await.unwrap();
            let token = token(&authority, 1, false);
            let prepared =
                prepare("MERGE (n:Item {key:2}) ON CREATE SET n.value=30 RETURN n.value AS value");
            let before = db.frontier().unwrap();
            for iteration in 0..2 {
                let (_, result, _) = db
                    .execute_graph_vertex_upserts_query_authorized(
                        txn,
                        query,
                        commit,
                        &authority,
                        &token,
                        "main",
                        &prepared,
                        upsert_policy(1),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.value.len(), 1);
                assert_eq!(
                    result.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(30))]
                );
                assert_eq!(db.vertices().unwrap().len(), 3, "iteration {iteration}");
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }
}
