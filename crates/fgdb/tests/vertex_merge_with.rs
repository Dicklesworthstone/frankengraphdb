//! fgdb-cpny9: vertex MERGE WITH tails use the real staged vertex, one
//! publication, and the same rollback boundary as ON clauses and RETURN.

#![recursion_limit = "256"]

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertBranch, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
    PreparedGraphWriteChainText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn policy(rows: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)),
        100,
    )
}

fn prepare(text: &str) -> PreparedGraphVertexUpsertQuery {
    assert!(PreparedGraphVertexUpsertQueryText::has_return_clause(text).unwrap());
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn computed_vertex_outputs_read_created_and_matched_final_values_in_heads_and_tails() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        for (tail, list) in [
            ("RETURN [v IN [n] | v.p] AS values", true),
            ("WITH collect(n) AS ns RETURN [v IN ns | v.p] AS values", true),
            ("WITH [n][0] AS chosen RETURN chosen.p AS value", false),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let query = prepare(&format!(
                "MERGE (n:Item {{id:1}}) ON CREATE SET n.p=2 ON MATCH SET n.p=n.p+1 SET n.p=n.p*10 {tail}"
            ));
            let mut allocated = 0;
            for expected in [20, 210] {
                let (_, _, result, _) = db.execute_graph_vertex_upsert_query_autocommit_governed(
                    &txcx, &cx, &commit, &query, policy(1), |_| {
                        allocated += 1;
                        Ok::<_, ()>(ElementId::Vertex(VId((1_u128 << 100) + 7)))
                    },
                ).await.unwrap();
                let expected = if list { GraphValue::List(vec![int(expected)].into()) } else { int(expected) };
                assert_eq!(result.value.len(), 1, "{tail}");
                assert_eq!(result.value[0].values(), &[expected], "{tail}");
            }
            assert_eq!(allocated, 1);
            assert_eq!(read(&db, &cx), vec![vec![QueryValue::Value(int(1)), QueryValue::Value(int(210))]]);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_vertex_merge_chain_heads_observe_all_occurrences_before_the_next_write() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let query = PreparedGraphWriteChainText::prepare(
            "UNWIND [1,2,3] AS i MERGE (a:Item {id:1}) \
             ON CREATE SET a.p=0 ON MATCH SET a.p=a.p+1 \
             WITH a,[v IN [a] | v.p] AS final \
             MERGE (b:Item {id:2}) ON CREATE SET b.p=7 \
             RETURN final,[v IN [b] | v.p] AS next",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut next = 100;
        let (stats, output, _) = transaction
            .execute_graph_write_chain_governed(
                &mut db,
                &cx,
                &query,
                fgdb_gql::insertion::GraphInsertPolicy::new(policy(3).merge.query, 3, 0),
                |_| {
                    next += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(next)))
                },
            )
            .unwrap();
        let expected = [
            GraphValue::List(vec![int(2)].into()),
            GraphValue::List(vec![int(7)].into()),
        ];
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(output.value.len(), 3);
        for row in output.value {
            assert_eq!(row.values(), &expected);
        }
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            read(&db, &cx),
            vec![
                vec![QueryValue::Value(int(1)), QueryValue::Value(int(2))],
                vec![QueryValue::Value(int(2)), QueryValue::Value(int(7))],
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_vertex_bag_grouping_reads_every_final_target_once_per_occurrence() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (vertex, value) in [(VId(1), 2), (VId((1_u128 << 100) + 1), 3)] {
            seed.create_vertex(
                vertex,
                vec![ITEM],
                vec![
                    (ID, CanonicalScalar::Int(1)),
                    (P, CanonicalScalar::Int(value)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let query = prepare(
            "MERGE (n:Item {id:1}) ON MATCH SET n.p=n.p+1 RETURN [v IN collect(n) | v.p] AS values",
        );
        let mut transaction = db.begin(&txcx).unwrap();
        let (_, result) = transaction
            .execute_graph_vertex_upserts_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("existing targets must not allocate") },
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].values(),
            &[GraphValue::List(vec![int(3), int(4)].into())]
        );
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_merge_output_keeps_off_root_field_witnesses_after_success_or_rollback() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        for fail_output in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(12))]);
            db.write(&commit, seed).await.unwrap();
            let text = if fail_output {
                "MERGE (n:Item {id:1}) RETURN [v IN $refs | v.p][0] AS p,1/0 AS failure"
            } else {
                "MERGE (n:Item {id:1}) RETURN [v IN $refs | v.p][0] AS p"
            };
            let query = PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
                text,
                R,
                &[("refs", GqlParameterType::List)],
                symbols,
            )
            .unwrap()
            .bind_parameters(
                &GqlParameters::new()
                    .with_list("refs", vec![GraphValue::Vertex(VId(99))])
                    .unwrap(),
            )
            .unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(98), vec![], vec![]);
            transaction.write(&mut db, prefix).unwrap();
            let result = transaction.execute_graph_vertex_upsert_query_governed(
                &mut db,
                &cx,
                &query,
                policy(1),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(100))),
            );
            assert_eq!(result.is_err(), fail_output, "{result:?}");
            if let Ok((_, _, output)) = result {
                assert_eq!(output.value[0].values(), &[int(12)]);
            }
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(99), P, Some(CanonicalScalar::Int(13)));
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(
                transaction.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(fgdb::WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.vertices().unwrap().len(), 1);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn computed_merge_output_refusals_preserve_prefix_and_every_checkpoint_is_terminal() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let query = prepare("MERGE (n:Item {id:1}) ON CREATE SET n.p=7 WITH collect(n) AS ns RETURN [v IN ns | v.p] AS values");
        let prefix = || {
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(40), vec![], vec![(P, CanonicalScalar::Int(40))]);
            batch
        };
        let allocate = |_| Ok::<_, ()>(ElementId::Vertex(VId(99)));
        let mut baseline = db.begin(&txcx).unwrap();
        baseline.write(&mut db, prefix()).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let (_, _, expected) = baseline.execute_graph_vertex_upsert_query_governed(
            &mut db, &cx.with_checkpoint_probe(probe.clone()), &query, policy(1), allocate,
        ).unwrap();
        let calls = probe.calls();
        assert!(calls > 10);
        baseline.abort();
        for cut in 1..=calls {
            let mut transaction = db.begin(&txcx).unwrap();
            transaction.write(&mut db, prefix()).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
            let result = transaction.execute_graph_vertex_upsert_query_governed(
                &mut db, &cx.with_checkpoint_probe(probe.clone()), &query, policy(1), allocate,
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(_))), "cut {cut}: {result:?}");
            assert_eq!(probe.calls(), cut);
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
            transaction.abort();
        }
        for limits in [
            GqlQueryPolicy::new(1000, 1, expected.evaluator.work_units - 1, 100_000),
            GqlQueryPolicy::new(1000, 1, 1_000_000, expected.evaluator.scratch_entries - 1),
        ] {
            let mut transaction = db.begin(&txcx).unwrap();
            transaction.write(&mut db, prefix()).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let limited = GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(limits), 100);
            let result = transaction.execute_graph_vertex_upsert_query_governed(
                &mut db, &cx, &query, limited, allocate,
            );
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))), "{result:?}");
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            transaction.abort();
        }
        let failed = prepare("MERGE (n:Item {id:1}) ON CREATE SET n.p=7 RETURN [v IN [n] | 10/(v.p-7)] AS failure LIMIT 0");
        let mut transaction = db.begin(&txcx).unwrap();
        transaction.write(&mut db, prefix()).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        assert!(transaction.execute_graph_vertex_upsert_query_governed(
            &mut db, &cx, &failed, policy(0), allocate,
        ).is_err());
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.vertex(VId(40)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

fn read(database: &Database<fgdb::MemVfs>, cx: &QueryCx) -> Vec<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = database
        .query(
            cx,
            "MATCH (n:Item) RETURN n.id AS id, n.p AS p ORDER BY id",
            &GqlParameters::new(),
            symbols,
            policy(100).merge.query,
        )
        .unwrap()
    else {
        panic!("row query");
    };
    rows
}

#[test]
fn with_reads_selected_on_actions_and_trailing_set_and_never_suppresses_creation() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let query = prepare(
            "MERGE (n:Item {id:1}) ON CREATE SET n.p = 2 \
             ON MATCH SET n.p = n.p + 1 SET n.p = n.p * 10 \
             WITH n AS item WHERE item.p > 0 RETURN item.p AS value",
        );
        let vertex = VId((1_u128 << 100) + 1);
        let mut allocations = 0;
        for (branch, expected) in [
            (GraphVertexUpsertBranch::Create, 20),
            (GraphVertexUpsertBranch::Match, 210),
        ] {
            let (stats, outcome, result, _) = db
                .execute_graph_vertex_upsert_query_autocommit_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &query,
                    policy(1),
                    |_| {
                        allocations += 1;
                        Ok::<_, ()>(ElementId::Vertex(vertex))
                    },
                )
                .await
                .unwrap();
            assert_eq!(stats.branch, branch);
            assert_eq!(outcome.vertex(), vertex);
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.value[0].values(), &[int(expected)]);
            assert_eq!(
                read(&db, &cx),
                vec![vec![
                    QueryValue::Value(int(1)),
                    QueryValue::Value(int(expected))
                ]]
            );
        }
        assert_eq!(allocations, 1, "ON MATCH must not create another vertex");

        let filtered = prepare(
            "MERGE (n:Item {id:2}) ON CREATE SET n.p = 7 \
             WITH n AS item WHERE item.p = 0 RETURN item",
        );
        let before = db.frontier().unwrap();
        let (_, outcome, result, _) = db
            .execute_graph_vertex_upsert_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &filtered,
                policy(0),
                |_| Ok::<_, ()>(ElementId::Vertex(VId((1_u128 << 100) + 2))),
            )
            .await
            .unwrap();
        assert!(outcome.created());
        assert!(result.value.is_empty());
        assert_ne!(db.frontier().unwrap(), before);
        assert_eq!(
            read(&db, &cx),
            vec![
                vec![QueryValue::Value(int(1)), QueryValue::Value(int(210))],
                vec![QueryValue::Value(int(2)), QueryValue::Value(int(7))],
            ]
        );
    });
}

#[test]
fn late_tail_failure_restores_the_statement_but_preserves_an_earlier_transaction_prefix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![ITEM],
            vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))],
        );
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(
            VId(40),
            vec![ITEM],
            vec![
                (ID, CanonicalScalar::Int(40)),
                (P, CanonicalScalar::Int(40)),
            ],
        );
        txn.write(&mut db, prefix).unwrap();
        let failed = prepare(
            "MERGE (n:Item {id:2}) ON CREATE SET n.p = 2 \
             WITH n AS item RETURN 10 / (item.p - 2) AS value LIMIT 0",
        );
        let mut allocated = 0;
        let result = txn.execute_graph_vertex_upsert_query_governed(
            &mut db,
            &cx,
            &failed,
            policy(0),
            |_| {
                allocated += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(99)))
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexUpsertError::Returning(_)))
        ));
        assert_eq!(allocated, 1, "failure must occur after the create arm");
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            read(&db, &cx),
            vec![
                vec![QueryValue::Value(int(1)), QueryValue::Value(int(10))],
                vec![QueryValue::Value(int(40)), QueryValue::Value(int(40))],
            ],
            "only the failed statement is rolled back"
        );
    });
}
