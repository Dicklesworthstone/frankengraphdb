//! Host result admission shares the native transaction's rollback and permit.
//! The callback sees final projected rows, not a rematch or a committed read.

#![recursion_limit = "256"]

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{AuthorizedWriteOutput, Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
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
        &mut dyn AuthorizedWriteOutput,
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
                |rows, work, _| {
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
                    |rows, work, _| {
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
                    |rows, work, _| {
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
                    |_, work, _| {
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
                    |rows, work, _| {
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

mod graph_output_metadata {
    use super::*;
    use fgdb_delta_types::ElementId;
    use fgdb_gql::{
        GlaExecutionStats, GlaLimitDimension, GraphElementMetadata, PreparedGraphWriteChainText,
        ReverseSymbolCatalog,
    };
    use fgdb_types::EId;

    fn catalog() -> ReverseSymbolCatalog {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(L, "Item");
        catalog.insert_relation(R, "LINK");
        catalog.insert_property(K, "key");
        catalog.insert_property(P, "value");
        catalog
    }

    fn text(value: &str) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
    }

    fn properties(key: i64, value: i64) -> GraphValue {
        GraphValue::map(vec![
            ("key".into(), GraphValue::Scalar(CanonicalScalar::Int(key))),
            (
                "value".into(),
                GraphValue::Scalar(CanonicalScalar::Int(value)),
            ),
        ])
        .unwrap()
    }

    fn vertex(rows: &[GraphValueRow]) -> ElementId {
        let [GraphValue::Vertex(vertex)] = rows[0].values() else {
            panic!("the native CREATE result must retain its vertex identity")
        };
        ElementId::Vertex(*vertex)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create(
        db: &mut Database<MemVfs>,
        commit: &CommitCx,
        query: &QueryCx,
        txn: &TxnCx,
        authority: &Authority,
        token: &CapabilityToken,
        query_policy: GqlQueryPolicy,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<fgdb_gql::GqlQueryExecution<GraphValueRow>, String> {
        let prepared = PreparedGraphInsertQueryText::prepare(
            "CREATE (n:Item {key:2,value:20}) RETURN n",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        db.execute_graph_insert_query_authorized_checked(
            txn,
            query,
            commit,
            authority,
            token,
            "main",
            &prepared,
            GraphInsertPolicy::new(query_policy, 100, 100),
            || NOW,
            admit,
        )
        .await
        .map(|(_, result, _)| result)
        .map_err(|error| error.to_string())
    }

    #[test]
    fn created_nodes_relationship_and_endpoints_hydrate_before_publication() {
        run(async |commit, query, txn| {
            let (mut db, authority, token) = fixture(commit).await;
            let before = db.frontier().unwrap();
            let prepared = PreparedGraphInsertQueryText::prepare(
                "CREATE (a:Item {key:2,value:20})-[r:LINK {value:30}]->\
                 (b:Item {key:3,value:40}) RETURN a,r,b",
                R,
                |kind, name: &str| match (kind, name) {
                    (GraphSymbolKind::Relation, "LINK") => Some(GraphSymbol::Relation(R)),
                    _ => symbols(kind, name),
                },
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let names = catalog();
            let mut admitted = false;
            let (_, result, completion) = db
                .execute_graph_insert_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    GraphInsertPolicy::new(policy(), 100, 100),
                    || NOW,
                    |rows, work, output| {
                        work(1)?;
                        let [
                            GraphValue::Vertex(a),
                            GraphValue::Edge(edge),
                            GraphValue::Vertex(b),
                        ] = rows[0].values()
                        else {
                            panic!("CREATE must preserve all three identity domains")
                        };
                        for (vertex, key, value) in [(*a, 2, 20), (*b, 3, 40)] {
                            assert_eq!(
                                output.metadata(
                                    ElementId::Vertex(vertex),
                                    GraphElementMetadata::Labels,
                                    &names,
                                )?,
                                Some(GraphValue::List(vec![text("Item")].into_boxed_slice()))
                            );
                            assert_eq!(
                                output.metadata(
                                    ElementId::Vertex(vertex),
                                    GraphElementMetadata::Properties,
                                    &names,
                                )?,
                                Some(properties(key, value))
                            );
                        }
                        for (kind, expected) in [
                            (GraphElementMetadata::Type, text("LINK")),
                            (GraphElementMetadata::StartNode, GraphValue::Vertex(*a)),
                            (GraphElementMetadata::EndNode, GraphValue::Vertex(*b)),
                            (
                                GraphElementMetadata::Properties,
                                GraphValue::map(vec![(
                                    "value".into(),
                                    GraphValue::Scalar(CanonicalScalar::Int(30)),
                                )])
                                .unwrap(),
                            ),
                        ] {
                            assert_eq!(
                                output.metadata(ElementId::Edge(*edge), kind, &names)?,
                                Some(expected)
                            );
                        }
                        admitted = true;
                        Ok(())
                    },
                )
                .await
                .unwrap();
            assert!(admitted);
            assert_eq!(result.value.len(), 1);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 3);
            assert_eq!(db.edges().unwrap().len(), 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }

    #[test]
    fn mutation_and_merge_metadata_read_the_final_staged_property_value() {
        run(async |commit, query, txn| {
            for kind in [Kind::Mutation, Kind::Merge] {
                let (mut db, authority, token) = fixture(commit).await;
                let names = catalog();
                checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    false,
                    |_, _, output| {
                        assert_eq!(
                            output.metadata(
                                ElementId::Vertex(VId(1)),
                                GraphElementMetadata::Properties,
                                &names,
                            )?,
                            Some(properties(1, 20))
                        );
                        Ok(())
                    },
                )
                .await
                .unwrap();
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn hidden_fields_and_relationship_endpoints_are_masked_before_catalog_access() {
        run(async |commit, query, txn| {
            let mut observations = Vec::new();
            for hidden in [false, true] {
                let (mut db, authority, _) = fixture(commit).await;
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(3), vec![L], vec![]);
                if hidden {
                    seed.set_vertex_label(VId(1), LabelId(2), true);
                    seed.set_vertex_property(
                        VId(1),
                        PropertyKeyId(3),
                        Some(CanonicalScalar::Int(999)),
                    );
                    seed.create_vertex(VId(2), vec![LabelId(2)], vec![]);
                    seed.add_edge(EId(1), VId(1), VId(2), vec![]);
                }
                db.write(commit, seed).await.unwrap();
                let token = authority
                    .issue_at(
                        &Grant {
                            branch: "main".into(),
                            labels: Scope::only([L]),
                            relations: Scope::only([R]),
                            properties: Scope::only([K, P]),
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
                // Deliberately omit the protected label and property names.
                let names = catalog();
                let result = create(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    policy(),
                    |_, _, output| {
                        assert_eq!(
                            output.metadata(
                                ElementId::Vertex(VId(1)),
                                GraphElementMetadata::Properties,
                                &names,
                            )?,
                            Some(properties(1, 10))
                        );
                        assert_eq!(
                            output.metadata(
                                ElementId::Vertex(VId(1)),
                                GraphElementMetadata::Labels,
                                &names,
                            )?,
                            Some(GraphValue::List(vec![text("Item")].into_boxed_slice()))
                        );
                        for kind in [
                            GraphElementMetadata::Type,
                            GraphElementMetadata::Properties,
                            GraphElementMetadata::StartNode,
                            GraphElementMetadata::EndNode,
                        ] {
                            assert_eq!(
                                output.metadata(ElementId::Edge(EId(1)), kind, &names)?,
                                None,
                                "an edge with a hidden endpoint must observe like an absent edge"
                            );
                        }
                        Ok(())
                    },
                )
                .await
                .unwrap();
                observations.push(result.evaluator);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert_eq!(observations[0], observations[1]);
        });
    }

    #[test]
    fn metadata_spends_the_cumulative_native_work_and_scratch_allowance() {
        run(async |commit, query, txn| {
            let mut baseline = None;
            let mut measured: Option<GlaExecutionStats> = None;
            for mode in 0..5 {
                let (mut db, authority, token) = fixture(commit).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                let mut limits = policy();
                if mode >= 2 {
                    let used = measured.unwrap();
                    limits.evaluator.max_work_units = used.work_units - u64::from(mode == 3);
                    limits.evaluator.max_scratch_entries =
                        used.scratch_entries - u64::from(mode == 4);
                }
                let names = catalog();
                let mut refusal = None;
                let result = create(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    limits,
                    |rows, _, output| {
                        if mode == 0 {
                            return Ok(());
                        }
                        let value = match output.metadata(
                            vertex(rows),
                            GraphElementMetadata::Properties,
                            &names,
                        ) {
                            Ok(value) => value,
                            Err(error) => {
                                if let WriteTxnError::OutputLimit(limit) = &error {
                                    refusal = Some(limit.dimension);
                                }
                                return Err(error);
                            }
                        };
                        assert_eq!(value, Some(properties(2, 20)));
                        Ok(())
                    },
                )
                .await;
                if mode >= 3 {
                    assert!(result.is_err());
                    assert_eq!(
                        refusal,
                        Some(if mode == 3 {
                            GlaLimitDimension::WorkUnits
                        } else {
                            GlaLimitDimension::ScratchEntries
                        })
                    );
                    assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                } else {
                    let used = result.unwrap().evaluator;
                    match mode {
                        0 => baseline = Some(used),
                        1 => {
                            let base = baseline.unwrap();
                            assert!(used.work_units > base.work_units);
                            assert!(used.scratch_entries > base.scratch_entries);
                            measured = Some(used);
                        }
                        _ => assert_eq!(Some(used), measured),
                    }
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        });
    }

    #[test]
    fn swallowed_work_metadata_and_live_authority_refusals_cannot_publish() {
        run(async |commit, query, txn| {
            for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
                for mode in 0..3 {
                    let (mut db, authority, token) = fixture(commit).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap());
                    let mut refused = false;
                    let result = checked(
                        &mut db,
                        commit,
                        query,
                        txn,
                        &authority,
                        &token,
                        kind,
                        false,
                        |_, work, output| {
                            if mode == 0 {
                                assert!(work(u64::MAX).is_err());
                            } else {
                                let names = if mode == 1 {
                                    ReverseSymbolCatalog::new()
                                } else {
                                    authority.retire();
                                    catalog()
                                };
                                let error = output
                                    .metadata(
                                        ElementId::Vertex(VId(1)),
                                        GraphElementMetadata::Properties,
                                        &names,
                                    )
                                    .unwrap_err();
                                if mode == 2 {
                                    assert!(matches!(error, WriteTxnError::Authorization(_)));
                                }
                            }
                            refused = true;
                            Ok(())
                        },
                    )
                    .await;
                    assert!(refused);
                    assert!(result.is_err(), "{kind:?}, refusal mode {mode}");
                    assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
            }
        });
    }

    #[test]
    fn hydrating_a_retired_chain_identity_refuses_and_rolls_back_the_delete() {
        run(async |commit, query, txn| {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let chain = PreparedGraphWriteChainText::prepare(
                "MATCH (n:Item {key:1}) FOREACH (i IN [1] | DELETE n) RETURN n",
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let names = catalog();
            let mut refused = false;
            let result = db
                .execute_graph_write_chain_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &chain,
                    GraphInsertPolicy::new(policy(), 100, 100).with_mutation_limit(100),
                    || NOW,
                    |rows, _, output| {
                        let error = output
                            .metadata(vertex(rows), GraphElementMetadata::Properties, &names)
                            .unwrap_err();
                        assert!(matches!(error, WriteTxnError::DeletedElementRead));
                        refused = true;
                        Err(error)
                    },
                )
                .await;
            assert!(refused);
            assert!(result.is_err());
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        });
    }
}
