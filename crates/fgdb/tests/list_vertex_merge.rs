//! Stored-list vertex MERGE through native, authorized and durable owners.
//! Expected graph values and row multiplicities are stated independently.

#![recursion_limit = "256"]

use asupersync::Budget;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertStats};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryExecution, GqlQueryPolicy, GraphSymbol,
    GraphSymbolKind, GraphVertexMergeOutcome, GraphVertexMergePolicy, PreparedGraphVertexMergeText,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId,
    PurposeContexts, QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x83; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x82; 32], NS, [0x84; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
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
fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(100_000, rows, 10_000_000, 2_000_000),
        100,
        0,
    )
    .with_mutation_limit(100)
}
fn chain(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}
fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(test(&commit, &query, &txn));
}

async fn execute(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    txn: &TxnCx,
    program: &PreparedGraphWriteChain,
    authorized: bool,
    allowance: GraphInsertPolicy,
) -> Result<(GraphInsertStats, GqlQueryExecution<GraphValueRow>), String> {
    if authorized {
        let issuer =
            Authority::new(AuthKey::from_seed(0x8301), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let grant = Grant {
            branch: "main".into(),
            labels: Scope::only([ITEM]),
            relations: Scope::only([R]),
            properties: Scope::only([P, Q]),
            rights: Rights::ReadWrite,
            limits: QueryLimits {
                max_nodes: 100_000,
                max_work: 20_000_000,
                max_rows: allowance.query.rows.max_result_rows().unwrap_or(100_000),
            },
            expires_at_ms: 10_000,
        };
        let token = issuer.issue_at(&grant, 100).unwrap();
        db.execute_graph_write_chain_authorized(
            txn,
            query,
            commit,
            &issuer,
            &token,
            "main",
            program,
            allowance,
            || 100,
        )
        .await
        .map(|(stats, rows, _)| (stats, rows))
        .map_err(|error| format!("{error:?}"))
    } else {
        let mut transaction = db.begin(txn).unwrap();
        let before = transaction.staged_effect_digest().ok();
        match transaction.execute_graph_write_chain_engine_governed(db, query, program, allowance) {
            Ok((stats, rows, _)) => {
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

fn values(db: &Database<MemVfs>) -> Vec<(CanonicalPropertyValue, Option<CanonicalPropertyValue>)> {
    let mut values = db
        .vertices()
        .unwrap()
        .into_iter()
        .filter(|vertex| vertex.labels.contains(&ITEM))
        .map(|vertex| {
            let p = vertex
                .props
                .iter()
                .find(|(key, _)| *key == P)
                .unwrap()
                .1
                .clone();
            let q = vertex
                .props
                .iter()
                .find(|(key, _)| *key == Q)
                .map(|(_, value)| value.clone());
            (p, q)
        })
        .collect::<Vec<_>>();
    values.sort();
    values
}

#[test]
fn literal_and_rebound_parameter_keys_create_and_match_the_same_stored_list() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
            "MERGE (n:Item {p:$key})",
            R,
            &[("key", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        let expected = list(vec![int(1), list(vec![int(2)]), list(vec![])]);
        let arguments = GqlParameters::new()
            .with_list("key", vec![int(1), list(vec![int(2)]), list(vec![])])
            .unwrap();
        let first = template.bind_parameters(&arguments).unwrap();
        let id = VId((1_u128 << 100) + 1);
        let (stats, outcome, _) = db
            .execute_graph_vertex_merge_autocommit_governed(
                txn,
                query,
                commit,
                &first,
                GraphVertexMergePolicy::new(policy(10).query),
                |_| Ok::<_, ()>(ElementId::Vertex(id)),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(id));
        assert_eq!(
            db.vertex(id).unwrap().unwrap().props,
            vec![(P, stored(expected))]
        );

        let numeric_peer =
            PreparedGraphVertexMergeText::prepare("MERGE (n:Item {p:[1.0,[2.0],[]]})", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let before = db.frontier().unwrap();
        let (stats, outcome, _) = db
            .execute_graph_vertex_merge_autocommit_governed(
                txn,
                query,
                commit,
                &numeric_peer,
                GraphVertexMergePolicy::new(policy(10).query),
                |_| -> Result<ElementId, ()> { panic!("numeric-equivalent list must match") },
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(id));
        assert_eq!(db.frontier().unwrap(), before);

        let empty = template
            .bind_parameters(&GqlParameters::new().with_list("key", vec![]).unwrap())
            .unwrap();
        let second_id = VId((1_u128 << 100) + 2);
        let (stats, _, _) = db
            .execute_graph_vertex_merge_autocommit_governed(
                txn,
                query,
                commit,
                &empty,
                GraphVertexMergePolicy::new(policy(10).query),
                |_| Ok::<_, ()>(ElementId::Vertex(second_id)),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(
            db.vertex(second_id).unwrap().unwrap().props,
            vec![(P, stored(list(vec![])))]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn scalar_list_scalar_chain_flushes_held_creations_once_and_reopens() {
    run(async |commit, query, txn| {
        let program = chain(
            "UNWIND [{key:[1,2]},{key:[1.0,2.0]},{key:[]}] AS row \
             MERGE (a:Item {p:7}) ON CREATE SET a.q=8 \
             MERGE (b:Item {p:row.key}) ON CREATE SET b.q=1 \
             MERGE (c:Item {p:7}) ON CREATE SET c.q=9",
        );
        for authorized in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let (stats, output) =
                execute(&mut db, commit, query, txn, &program, authorized, policy(0))
                    .await
                    .unwrap();
            assert_eq!(stats.created_vertices, 3);
            assert!(output.value.is_empty());
            assert_eq!(output.rows.result_rows, 0);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let mut expected = vec![
                (stored(int(7)), Some(stored(int(8)))),
                (stored(list(vec![int(1), int(2)])), Some(stored(int(1)))),
                (stored(list(vec![])), Some(stored(int(1)))),
            ];
            expected.sort();
            assert_eq!(values(&db), expected);
            let frontier = db.frontier().unwrap();
            drop(db);
            let mut reopened = Database::open_with_vfs(commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(values(&reopened), expected);
            let (stats, _) = execute(
                &mut reopened,
                commit,
                query,
                txn,
                &program,
                authorized,
                policy(0),
            )
            .await
            .unwrap();
            assert_eq!(stats.created_vertices, 0);
            assert_eq!(reopened.frontier().unwrap(), frontier);
            assert_eq!(values(&reopened), expected);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn null_children_never_become_memoized_matches_and_empty_lists_do_match() {
    run(async |commit, query, txn| {
        for (keys_text, expected_created, expected_list_count) in
            [("[[1,NULL],[1,NULL]]", 3, 2), ("[[],[]]", 2, 1)]
        {
            let program = chain(&format!(
                "UNWIND {keys_text} AS key MERGE (a:Item {{p:key}}) MERGE (b:Item {{p:0}})"
            ));
            for authorized in [false, true] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                let (stats, _) =
                    execute(&mut db, commit, query, txn, &program, authorized, policy(0))
                        .await
                        .unwrap();
                assert_eq!(stats.created_vertices, expected_created, "{keys_text}");
                assert_eq!(
                    values(&db)
                        .iter()
                        .filter(|(value, _)| value.is_collection())
                        .count(),
                    expected_list_count,
                    "{keys_text}",
                );
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn all_matching_vertices_and_duplicate_inputs_preserve_their_bag() {
    run(async |commit, query, txn| {
        let program = chain(
            "UNWIND [[1,2],[1.0,2.0]] AS key MERGE (a:Item {p:key}) \
             MERGE (b:Item {p:0}) RETURN count(*) AS occurrences",
        );
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in [VId(1), VId((1_u128 << 96) + 1)] {
                seed.create_vertex_values(
                    id,
                    vec![ITEM],
                    vec![(P, stored(list(vec![int(1), int(2)])))],
                );
            }
            db.write(commit, seed).await.unwrap();
            let (stats, result) =
                execute(&mut db, commit, query, txn, &program, authorized, policy(1))
                    .await
                    .unwrap();
            assert_eq!(stats.created_vertices, 1);
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.value[0].values(), &[int(4)]);
            assert_eq!(values(&db).len(), 3);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn branch_key_mutation_invalidates_later_scalar_and_list_matches() {
    run(async |commit, query, txn| {
        let program = chain(
            "UNWIND [7,[1,2],7,[1.0,2.0]] AS key \
             MERGE (a:Item {p:key}) ON CREATE SET a.p=9 \
             MERGE (b:Item {p:0})",
        );
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let (stats, _) = execute(&mut db, commit, query, txn, &program, authorized, policy(0))
                .await
                .unwrap();
            assert_eq!(stats.created_vertices, 5);
            assert_eq!(
                values(&db)
                    .iter()
                    .filter(|(value, _)| *value == stored(int(9)))
                    .count(),
                4,
            );
            assert_eq!(values(&db).len(), 5);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn late_invalid_keys_and_creation_or_effect_limits_roll_back_the_flushed_prefix() {
    run(async |commit, query, txn| {
        for (text, allowance) in [
            (
                "UNWIND [[1,2],NULL] AS key MERGE (a:Item {p:7}) \
                 MERGE (b:Item {p:key}) MERGE (c:Item {p:9})",
                policy(0),
            ),
            (
                "UNWIND [[1,2],[{x:1}]] AS key MERGE (a:Item {p:7}) \
                 MERGE (b:Item {p:key}) MERGE (c:Item {p:9})",
                policy(0),
            ),
            (
                "MERGE (a:Item {p:7}) MERGE (b:Item {p:[1,2]})",
                GraphInsertPolicy::new(policy(0).query, 1, 0),
            ),
            (
                "MERGE (a:Item {p:7}) MERGE (b:Item {p:[1,2]}) ON CREATE SET b.q=1",
                policy(0).with_mutation_limit(0),
            ),
        ] {
            let program = chain(text);
            for authorized in [false, true] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                assert!(
                    execute(&mut db, commit, query, txn, &program, authorized, allowance,)
                        .await
                        .is_err(),
                    "{text}"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn hidden_list_candidates_do_not_change_authorized_results_or_native_statistics() {
    run(async |commit, query, txn| {
        let program = chain("MERGE (a:Item {p:[1,2]}) MERGE (b:Item {p:7})");
        let mut expected = None;
        for hidden in [0_u128, 40] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            if hidden != 0 {
                let mut seed = WriteBatch::new(R);
                for id in 1..=hidden {
                    seed.create_vertex_values(
                        VId((1_u128 << 110) + id),
                        vec![HIDDEN],
                        vec![(P, stored(list(vec![int(1), int(2)])))],
                    );
                }
                db.write(commit, seed).await.unwrap();
            }
            let (stats, output) = execute(&mut db, commit, query, txn, &program, true, policy(0))
                .await
                .unwrap();
            assert_eq!(stats.created_vertices, 2);
            assert_eq!(values(&db).len(), 2);
            let actual = (stats, output.rows, output.evaluator);
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected);
            } else {
                expected = Some(actual);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
