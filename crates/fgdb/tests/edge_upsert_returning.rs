//! RETURN observes the exact relationship selected by MERGE after every clause.
//! The real Chronicle/Strata transaction owns effects, witnesses and rollback.

#![recursion_limit = "256"]

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeReturnBinding, GraphEdgeUpsertBranch,
    GraphEdgeUpsertPolicy, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const UPSERT: &str = "MATCH (a:Item)-[:S]->(b:Item) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p = 7 ON MATCH SET r.p = r.p + 1 SET r.p = r.p * 2";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn policy(rows: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        fgdb_gql::GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000)),
        100,
    )
}
fn prepared() -> PreparedGraphEdgeUpsertQuery {
    let upsert = PreparedGraphEdgeUpsertText::prepare(UPSERT, S, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphEdgeUpsertQuery::prepare(
        upsert,
        vec![
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(P),
            GraphEdgeReturnBinding::Property(Q),
        ],
        ["edge", "p", "missing"]
            .into_iter()
            .enumerate()
            .map(|(column, name)| GraphSetProjection::new(name, GraphSetValue::Column(column)))
            .collect(),
        GraphSetQuantifier::All,
    )
    .unwrap()
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn row(edge: EId, value: i64) -> Vec<GraphValueRow> {
    vec![GraphValueRow::from_owned_values(vec![
        GraphValue::Edge(edge),
        int(value),
        GraphValue::Scalar(CanonicalScalar::Null),
    ])]
}
fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &query, &txn))
}
async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx, input: bool) {
    let mut batch = WriteBatch::new(S);
    for id in [1, 2] {
        batch.create_vertex(
            VId(id),
            vec![ITEM],
            vec![(P, CanonicalScalar::Int(id as i64))],
        );
    }
    if input {
        // Two occurrences, but one unique endpoint pair and one chosen edge.
        for id in [11, 12] {
            batch.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
    }
    db.write(commit, batch).await.unwrap();
}
fn read(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(
            cx,
            text,
            &GqlParameters::new(),
            symbols,
            policy(100).merge.query,
        )
        .unwrap()
    else {
        panic!("expected native row result")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected plain value"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn created_then_matched_return_reads_final_staged_properties_of_the_same_edge() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let query = prepared();
        let (first_stats, first, first_rows) = txn
            .execute_graph_edge_upsert_query_engine_governed(&mut db, cx, &query, policy(1))
            .unwrap();
        let edge = first.edge().unwrap();
        assert!(first.created());
        assert_eq!(first_stats.branch, GraphEdgeUpsertBranch::Create);
        assert_eq!(first_rows.value, row(edge, 14));
        assert_eq!(first_rows.rows.result_rows, 1);
        assert!(first_rows.evaluator.work_units > first_stats.evaluator.work_units);
        let (second_stats, second, second_rows) = txn
            .execute_graph_edge_upsert_query_engine_governed(&mut db, cx, &query, policy(1))
            .unwrap();
        assert!(!second.created());
        assert_eq!(second.edge(), Some(edge));
        assert_eq!(second_stats.branch, GraphEdgeUpsertBranch::Match);
        assert_eq!(second_rows.value, row(edge, 30));
        assert_eq!(
            first_rows.value,
            row(edge, 14),
            "later clauses cannot rewrite returned values"
        );
        assert_eq!(db.frontier().unwrap(), frontier, "staging does not publish");
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(30)]]
        );
    });
}

#[test]
fn zero_page_keeps_effects_but_a_zero_result_allowance_rolls_them_back() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(80), vec![ITEM], vec![(P, CanonicalScalar::Int(80))]);
        txn.write(&mut db, prefix).unwrap();
        let error = txn
            .execute_graph_edge_upsert_query_governed(&mut db, cx, &prepared(), policy(0), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(90)))
            })
            .unwrap_err();
        assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}");
        txn.finish(&mut db, commit).await.unwrap();
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
        assert_eq!(
            read(&db, cx, "MATCH (a:Item {p:80}) RETURN a.p AS p"),
            vec![vec![int(80)]],
            "prior workspace survives"
        );

        let mut txn = db.begin(txcx).unwrap();
        let (_, outcome, output) = txn
            .execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &prepared().with_page(0, Some(0)),
                policy(0),
                |_| Ok::<_, ()>(ElementId::Edge(EId(91))),
            )
            .unwrap();
        assert!(outcome.created());
        assert!(output.value.is_empty());
        assert_eq!(output.rows.result_rows, 0);
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(14)]]
        );
    });
}

#[test]
fn no_input_is_empty_and_never_allocates_or_evaluates_branch_properties() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (stats, outcome, output) = txn
            .execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &prepared(),
                policy(0),
                |_| -> Result<ElementId, ()> { panic!("no input allocates nothing") },
            )
            .unwrap();
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::NoInput);
        assert!(outcome.edge().is_none());
        assert!(output.value.is_empty());
        assert_eq!(stats.action_effects, 0);
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
    });
}

#[test]
fn return_continues_the_on_and_set_work_budget_even_when_limit_is_zero() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let query = prepared();
        let mut measured = db.begin(txcx).unwrap();
        let (stats, _) = measured
            .execute_graph_edge_upsert_governed(&mut db, cx, query.upsert(), policy(100), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(90)))
            })
            .unwrap();
        measured.abort();
        let mut limited = policy(0);
        limited.merge.query.evaluator.max_work_units = stats.evaluator.work_units;
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let error = txn
            .execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &query.with_page(0, Some(0)),
                limited,
                |_| Ok::<_, ()>(ElementId::Edge(EId(91))),
            )
            .unwrap_err();
        assert!(matches!(error, GqlQueryError::Evaluator(_)), "{error:?}");
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
    });
}

#[test]
fn return_property_witnesses_still_conflict_with_a_concurrent_edge_change() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let (_, _, _, completion) = db
            .execute_graph_edge_upsert_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &prepared(),
                policy(1),
                |_| Ok::<_, ()>(ElementId::Edge(EId(90))),
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let mut txn = db.begin(txcx).unwrap();
        let (_, _, output) = txn
            .execute_graph_edge_upsert_query_engine_governed(&mut db, cx, &prepared(), policy(1))
            .unwrap();
        assert_eq!(output.value, row(EId(90), 30));
        let mut concurrent = WriteBatch::new(R);
        concurrent.set_edge_property(EId(90), Q, Some(CanonicalScalar::Int(99)));
        db.write(commit, concurrent).await.unwrap();
        assert!(
            txn.finish(&mut db, commit).await.is_err(),
            "RETURN observed the formerly absent property"
        );
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p,r.q AS q"),
            vec![vec![int(14), int(99)]]
        );
    });
}

#[test]
fn autocommit_refusal_never_publishes_the_private_result_or_creation() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let frontier = db.frontier().unwrap();
        let error = db
            .execute_graph_edge_upsert_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &prepared(),
                policy(0),
                |_| Ok::<_, ()>(ElementId::Edge(EId(90))),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}");
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
    });
}

#[test]
fn parsed_return_expression_failure_restores_the_statement_workspace() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let text = format!("{UPSERT} RETURN r.p / 0 AS invalid LIMIT 0");
        let query = fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(&text, S, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(80), vec![ITEM], vec![(P, CanonicalScalar::Int(80))]);
        txn.write(&mut db, prefix).unwrap();
        assert!(
            txn.execute_graph_edge_upsert_query_governed(&mut db, cx, &query, policy(0), |_| Ok::<
                _,
                (),
            >(
                ElementId::Edge(EId(90))
            ),)
                .is_err()
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
        assert_eq!(
            read(&db, cx, "MATCH (a:Item {p:80}) RETURN a.p AS p"),
            vec![vec![int(80)]]
        );
    });
}

#[test]
fn native_text_return_composes_parameters_branch_effects_and_aggregate_results() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let text = "MATCH (a:Item)-[:S]->(b:Item) MERGE (a)-[r:R]->(b) \
            ON CREATE SET r.p = $initial SET r.p = r.p * 2 \
            RETURN count(*) AS rows,sum(r.p) AS total,collect(r) AS edges";
        let template =
            fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(text, S, symbols).unwrap();
        let query = template
            .bind_parameters(&GqlParameters::new().with_int64("initial", 7).unwrap())
            .unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (_, outcome, output) = txn
            .execute_graph_edge_upsert_query_engine_governed(&mut db, cx, &query, policy(1))
            .unwrap();
        let edge = outcome.edge().unwrap();
        assert_eq!(
            output.value,
            vec![GraphValueRow::from_owned_values(vec![
                int(1),
                int(14),
                GraphValue::List(vec![GraphValue::Edge(edge)].into_boxed_slice()),
            ])]
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(14)]]
        );
    });
}

#[test]
fn native_no_action_merge_return_counts_no_input_without_creating_an_edge() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let query = fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(
            "MATCH (a:Item)-[:S]->(b:Item) MERGE (a)-[r:R]->(b) RETURN count(*) AS rows",
            S,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let (stats, outcome, output) = txn
            .execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &query,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("no endpoint input") },
            )
            .unwrap();
        assert_eq!(stats.branch, GraphEdgeUpsertBranch::NoInput);
        assert!(outcome.edge().is_none());
        assert_eq!(
            output.value,
            vec![GraphValueRow::from_owned_values(vec![int(0)])]
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
    });
}

#[test]
fn computed_edge_outputs_read_the_complete_upsert_bag_in_every_output_phase() {
    run(async |commit, cx, txcx| {
        for plural in [false, true] {
            for (tail, aggregate, collected) in [
                ("RETURN [e IN [r] | e.p] AS values", false, false),
                ("RETURN sum([e IN [r] | e.p][0]) AS total", true, false),
                (
                    "WITH collect(r) AS edges RETURN [e IN edges | e.p] AS values",
                    false,
                    true,
                ),
            ] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                seed(&mut db, commit, true).await;
                let query = fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(
                    &format!("{UPSERT} {tail}"),
                    S,
                    symbols,
                )
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
                let mut transaction = db.begin(txcx).unwrap();
                let occurrences = if plural { 2 } else { 1 };
                for expected in if plural { [30, 126] } else { [14, 30] } {
                    let output = if plural {
                        transaction
                            .execute_graph_edge_upserts_query_engine_governed(
                                &mut db,
                                cx,
                                &query,
                                policy(if aggregate || collected { 1 } else { 2 }),
                            )
                            .unwrap()
                            .1
                    } else {
                        transaction
                            .execute_graph_edge_upsert_query_engine_governed(
                                &mut db,
                                cx,
                                &query,
                                policy(1),
                            )
                            .unwrap()
                            .2
                    };
                    let expected_rows = if aggregate {
                        vec![GraphValueRow::from_owned_values(vec![int(
                            expected * occurrences
                        )])]
                    } else if collected {
                        vec![GraphValueRow::from_owned_values(vec![GraphValue::List(
                            vec![int(expected); occurrences as usize].into_boxed_slice(),
                        )])]
                    } else {
                        vec![
                            GraphValueRow::from_owned_values(vec![GraphValue::List(
                                vec![int(expected)].into_boxed_slice(),
                            )]);
                            occurrences as usize
                        ]
                    };
                    assert_eq!(output.value, expected_rows, "plural={plural}: {tail}");
                }
                transaction.finish(&mut db, commit).await.unwrap();
                assert_eq!(
                    read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
                    vec![vec![int(if plural { 126 } else { 30 })]]
                );
            }
        }
    });
}

#[test]
fn computed_edge_output_errors_beyond_limit_zero_restore_the_outer_prefix() {
    run(async |commit, cx, txcx| {
        for plural in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            seed(&mut db, commit, true).await;
            let final_value = if plural { 30 } else { 14 };
            let query = fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(
                &format!(
                    "{UPSERT} WITH collect(r) AS edges \
                     RETURN [e IN edges | 1 / (e.p - {final_value})] AS bad LIMIT 0"
                ),
                S,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let mut transaction = db.begin(txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(80), vec![ITEM], vec![(P, CanonicalScalar::Int(80))]);
            transaction.write(&mut db, prefix).unwrap();
            let before = (
                db.frontier().unwrap(),
                transaction.staged_effect_digest().unwrap(),
            );
            let refused = if plural {
                transaction
                    .execute_graph_edge_upserts_query_engine_governed(
                        &mut db,
                        cx,
                        &query,
                        policy(0),
                    )
                    .is_err()
            } else {
                transaction
                    .execute_graph_edge_upsert_query_engine_governed(&mut db, cx, &query, policy(0))
                    .is_err()
            };
            assert!(refused, "plural={plural}");
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    transaction.staged_effect_digest().unwrap(),
                ),
                before
            );
            transaction.finish(&mut db, commit).await.unwrap();
            assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
            assert_eq!(
                read(&db, cx, "MATCH (a:Item {p:80}) RETURN a.p AS p"),
                vec![vec![int(80)]]
            );
        }
    });
}
