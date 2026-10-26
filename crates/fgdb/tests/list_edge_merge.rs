//! Stored-list relationship MERGE keys through native and authorized owners.
//! These laws cover real storage, bag occurrences, rollback and recovery.

#![recursion_limit = "256"]

use asupersync::Budget;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeMergeStats,
    GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText,
    PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const KEY: PropertyKeyId = PropertyKeyId(2);
const STATE: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x92; 32], NS, [0x94; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        (GraphSymbolKind::Property, "state") => Some(GraphSymbol::Property(STATE)),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn stored(value: GraphValue) -> CanonicalPropertyValue {
    value.to_written_property().unwrap()
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 2_000_000))
        .with_creation_limit(100)
}
fn prepared(text: &str) -> PreparedGraphEdgeMergeQuery {
    PreparedGraphEdgeMergeQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}
fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}
fn credentials(properties: Scope<PropertyKeyId>) -> (Authority, CapabilityToken) {
    let issuer =
        Authority::new(AuthKey::from_seed(0x9301), NS, "graph", SchemaEpoch(1), 1).unwrap();
    let grant = Grant {
        branch: "main".into(),
        labels: Scope::only([ITEM]),
        relations: Scope::only([R]),
        properties,
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 20_000_000,
            max_rows: 100_000,
        },
        expires_at_ms: 10_000,
    };
    let token = issuer.issue_at(&grant, 100).unwrap();
    (issuer, token)
}
async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for n in [1_u128, 2] {
        batch.create_vertex(
            VId(n),
            vec![ITEM],
            vec![(P, CanonicalScalar::Int(n as i64))],
        );
    }
    db.write(commit, batch).await.unwrap();
}
#[allow(clippy::too_many_arguments)]
async fn execute(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    txn: &TxnCx,
    definition: &PreparedGraphEdgeMergeQuery,
    authorized: bool,
    allowance: GraphEdgeMergePolicy,
) -> Result<(GraphEdgeMergeStats, GqlQueryExecution<GraphValueRow>), String> {
    if authorized {
        let (issuer, token) = credentials(Scope::only([P, KEY, STATE]));
        db.execute_graph_edge_merge_query_authorized(
            txn,
            query,
            commit,
            &issuer,
            &token,
            "main",
            definition,
            allowance,
            || 100,
        )
        .await
        .map(|(stats, rows, _)| (stats, rows))
        .map_err(|error| format!("{error:?}"))
    } else {
        let mut transaction = db.begin(txn).unwrap();
        let before = transaction.staged_effect_digest().ok();
        match transaction
            .execute_graph_edge_merge_query_engine_governed(db, query, definition, allowance)
        {
            Ok((stats, rows)) => {
                transaction
                    .finish(db, commit)
                    .await
                    .map_err(|error| format!("{error:?}"))?;
                Ok((stats, rows))
            }
            Err(error) => {
                assert_eq!(transaction.staged_effect_digest().ok(), before);
                transaction.abort();
                Err(format!("{error:?}"))
            }
        }
    }
}

#[test]
fn nested_list_keys_create_match_numeric_peers_and_survive_reopen() {
    run(async |commit, query, txn| {
        for authorized in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, commit).await;
            let create = prepared(
                "MATCH (a:Item {p:1}),(b:Item {p:2}) \
                 MERGE (a)-[r:R {key:[1,[2],[]]}]->(b) RETURN r.key AS value",
            );
            let (stats, rows) = execute(&mut db, commit, query, txn, &create, authorized, policy())
                .await
                .unwrap();
            let expected = list(vec![int(1), list(vec![int(2)]), list(vec![])]);
            assert_eq!(stats.created_edges, 1);
            assert_eq!(rows.value[0].values(), &[expected.clone()]);
            assert_eq!(
                db.edges().unwrap()[0].props,
                vec![(KEY, stored(expected.clone()))]
            );
            let before = db.frontier().unwrap();
            drop(db);
            let mut db = Database::open_with_vfs(commit, vfs, &path, keys())
                .await
                .unwrap();
            let matched = prepared(
                "MATCH (a:Item {p:1}),(b:Item {p:2}) \
                 MERGE (a)-[r:R {key:[1.0,[2.0],[]]}]->(b) RETURN r.key AS value",
            );
            let (stats, rows) = execute(
                &mut db,
                commit,
                query,
                txn,
                &matched,
                authorized,
                policy().with_creation_limit(0),
            )
            .await
            .unwrap();
            assert_eq!(stats.created_edges, 0);
            assert_eq!(rows.value[0].values(), &[expected]);
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.edges().unwrap().len(), 1);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn null_list_keys_preserve_each_input_occurrence_while_empty_lists_share_matches() {
    run(async |commit, query, txn| {
        for (key, created, distinct) in [("[1,NULL]", 2, 2), ("[]", 1, 1)] {
            let definition = prepared(&format!(
                "MATCH (a:Item {{p:1}}),(b:Item {{p:2}}),(copy:Item) \
                 MERGE (a)-[r:R {{key:{key}}}]->(b) RETURN r AS edge,r.key AS value"
            ));
            for authorized in [false, true] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                seed(&mut db, commit).await;
                let (stats, rows) = execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &definition,
                    authorized,
                    policy(),
                )
                .await
                .unwrap();
                assert_eq!(stats.created_edges, created, "{key}");
                assert_eq!(rows.value.len(), 2);
                let ids = rows
                    .value
                    .iter()
                    .map(|row| row.values()[0].clone())
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(ids.len(), distinct, "{key}");
                assert_eq!(db.edges().unwrap().len(), distinct);
                let (again, repeated) = execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &definition,
                    authorized,
                    policy(),
                )
                .await
                .unwrap();
                assert_eq!(again.created_edges, if key == "[]" { 0 } else { 2 });
                assert_eq!(
                    repeated.value.len(),
                    2,
                    "no cross-product of occurrence results"
                );
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn undirected_parallel_list_matches_and_duplicate_inputs_keep_their_bag() {
    run(async |commit, query, txn| {
        let definition = prepared(
            "MATCH (a:Item {p:1}),(b:Item {p:2}),(copy:Item) \
             MERGE (a)-[r:R {key:[1.0,2.0]}]-(b) RETURN count(*) AS matches",
        );
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit).await;
            let mut batch = WriteBatch::new(R);
            for (id, src, dst) in [(EId(10), VId(1), VId(2)), (EId(11), VId(2), VId(1))] {
                batch.add_edge_values(
                    id,
                    src,
                    dst,
                    vec![(KEY, stored(list(vec![int(1), int(2)])))],
                );
            }
            db.write(commit, batch).await.unwrap();
            let before = db.frontier().unwrap();
            let (stats, rows) = execute(
                &mut db,
                commit,
                query,
                txn,
                &definition,
                authorized,
                policy().with_creation_limit(0),
            )
            .await
            .unwrap();
            assert_eq!(stats.created_edges, 0);
            assert_eq!(rows.value[0].values(), &[int(4)]);
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn list_creation_limits_and_late_return_errors_roll_back_every_proposal() {
    run(async |commit, query, txn| {
        for (text, allowance) in [
            (
                "MATCH (a:Item {p:1}),(b:Item {p:2}),(copy:Item) \
              MERGE (a)-[r:R {key:[NULL]}]->(b) RETURN r",
                policy().with_creation_limit(1),
            ),
            (
                "MATCH (a:Item {p:1}),(b:Item {p:2}) \
              MERGE (a)-[r:R {key:[1]}]->(b) RETURN 1/0 AS bad LIMIT 0",
                policy(),
            ),
        ] {
            let definition = prepared(text);
            for authorized in [false, true] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                seed(&mut db, commit).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                assert!(
                    execute(
                        &mut db,
                        commit,
                        query,
                        txn,
                        &definition,
                        authorized,
                        allowance,
                    )
                    .await
                    .is_err()
                );
                assert!(db.edges().unwrap().is_empty());
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn list_key_upsert_actions_and_later_merges_see_the_current_overlay() {
    run(async |commit, query, txn| {
        let program = PreparedGraphWriteChainText::prepare(
            "MATCH (a:Item {p:1}),(b:Item {p:2}) \
             MERGE (a)-[r:R {key:[1]}]->(b) ON CREATE SET r.key=[2] \
             MERGE (a)-[s:R {key:[1]}]->(b) RETURN r.key AS first,s.key AS second",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let allowance = GraphInsertPolicy::new(policy().query, 0, 10).with_mutation_limit(10);
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit).await;
            let (stats, rows) = if authorized {
                let (issuer, token) = credentials(Scope::only([P, KEY, STATE]));
                let (stats, rows, _) = db
                    .execute_graph_write_chain_authorized(
                        txn,
                        query,
                        commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        allowance,
                        || 100,
                    )
                    .await
                    .unwrap();
                (stats, rows)
            } else {
                let mut transaction = db.begin(txn).unwrap();
                let (stats, rows, _) = transaction
                    .execute_graph_write_chain_engine_governed(&mut db, query, &program, allowance)
                    .unwrap();
                transaction.finish(&mut db, commit).await.unwrap();
                (stats, rows)
            };
            assert_eq!(stats.created_edges, 2);
            assert_eq!(
                rows.value[0].values(),
                &[list(vec![int(2)]), list(vec![int(1)])]
            );
            let mut keys = db
                .edges()
                .unwrap()
                .into_iter()
                .map(|edge| edge.props[0].1.clone())
                .collect::<Vec<_>>();
            keys.sort();
            assert_eq!(
                keys,
                vec![stored(list(vec![int(1)])), stored(list(vec![int(2)]))]
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_list_keys_refuse_before_empty_selection_or_writes() {
    run(async |commit, query, txn| {
        for key in [1, 999] {
            let definition = prepared(&format!(
                "MATCH (a:Item {{p:{key}}}),(b:Item {{p:2}}) \
                 MERGE (a)-[r:R {{key:[1]}}]->(b) RETURN r"
            ));
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit).await;
            let before = db.frontier().unwrap();
            let (issuer, token) = credentials(Scope::only([P]));
            assert!(
                db.execute_graph_edge_merge_query_authorized(
                    txn,
                    query,
                    commit,
                    &issuer,
                    &token,
                    "main",
                    &definition,
                    policy(),
                    || 100,
                )
                .await
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn hidden_list_candidates_do_not_change_authorized_rows_or_statistics() {
    run(async |commit, query, txn| {
        let definition = prepared(
            "MATCH (a:Item {p:1}),(b:Item {p:2}) \
             MERGE (a)-[r:R {key:[1,2]}]->(b) RETURN r.key AS value",
        );
        let mut expected = None;
        for hidden in [0_u128, 30] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit).await;
            if hidden != 0 {
                let mut batch = WriteBatch::new(R);
                let src = VId(1_u128 << 110);
                let dst = VId((1_u128 << 110) + 1);
                batch.create_vertex(src, vec![HIDDEN], vec![(P, CanonicalScalar::Int(1))]);
                batch.create_vertex(dst, vec![HIDDEN], vec![(P, CanonicalScalar::Int(2))]);
                for n in 0..hidden {
                    batch.add_edge_values(
                        EId((1_u128 << 100) + n),
                        src,
                        dst,
                        vec![(KEY, stored(list(vec![int(1), int(2)])))],
                    );
                }
                db.write(commit, batch).await.unwrap();
            }
            let (stats, rows) = execute(&mut db, commit, query, txn, &definition, true, policy())
                .await
                .unwrap();
            assert_eq!(stats.created_edges, 1);
            let actual = (
                stats,
                rows.rows,
                rows.evaluator,
                rows.value[0].values().to_vec(),
            );
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected);
            } else {
                expected = Some(actual);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn chained_null_list_merges_keep_occurrences_through_the_next_write() {
    run(async |commit, query, txn| {
        let program = PreparedGraphWriteChainText::prepare(
            "MATCH (a:Item {p:1}),(b:Item {p:2}),(copy:Item) \
             MERGE (a)-[r:R {key:[NULL]}]->(b) \
             MERGE (a)-[s:R {key:[]}]->(b) \
             RETURN count(DISTINCT r) AS first,count(DISTINCT s) AS second",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let allowance = GraphInsertPolicy::new(policy().query, 0, 10);
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit).await;
            let (stats, rows) = if authorized {
                let (issuer, token) = credentials(Scope::only([P, KEY, STATE]));
                let (stats, rows, _) = db
                    .execute_graph_write_chain_authorized(
                        txn,
                        query,
                        commit,
                        &issuer,
                        &token,
                        "main",
                        &program,
                        allowance,
                        || 100,
                    )
                    .await
                    .unwrap();
                (stats, rows)
            } else {
                let mut transaction = db.begin(txn).unwrap();
                let (stats, rows, _) = transaction
                    .execute_graph_write_chain_engine_governed(&mut db, query, &program, allowance)
                    .unwrap();
                transaction.finish(&mut db, commit).await.unwrap();
                (stats, rows)
            };
            assert_eq!(stats.created_edges, 3);
            assert_eq!(rows.value[0].values(), &[int(2), int(1)]);
            assert_eq!(db.edges().unwrap().len(), 3);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
