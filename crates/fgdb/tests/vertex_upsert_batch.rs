//! All matching vertices use one frozen MERGE selection, one cumulative budget
//! and one rollback workspace. Expected effects and results are enumerated from
//! the small seed graph, not inferred from another invocation of the upsert.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphVertexMergeError, GraphVertexMergePolicy,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}
fn symbols(kind: fgdb_gql::GraphSymbolKind, name: &str) -> Option<fgdb_gql::GraphSymbol> {
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn policy(rows: u64, actions: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(100_000, rows, 10_000_000, 1_000_000)),
        actions,
    )
}
fn prepare(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
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
async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            vec![ITEM],
            vec![
                (P, CanonicalScalar::Int(id as i64)),
                (Q, CanonicalScalar::Int(7)),
            ],
        );
    }
    db.write(commit, batch).await.unwrap();
}
fn values(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected native row result")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected plain value"),
                })
                .collect()
        })
        .collect()
}
fn read(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    values(
        db.query(
            cx,
            text,
            &GqlParameters::new(),
            symbols,
            policy(100, 100).merge.query,
        )
        .unwrap(),
    )
}

#[test]
fn all_matches_receive_actions_and_one_aggregate_uses_the_final_bag() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit).await;
        let mut txn = db.begin(txcx).unwrap();
        let query = prepare(
            "MERGE (n:Item {q: 7}) ON MATCH SET n.p = n.p + 10, n.q = 8 \
             SET n.p = n.p * 2 WITH n AS node RETURN count(*) AS n, sum(node.p) AS total",
        );
        let (stats, rows) = txn
            .execute_graph_vertex_upserts_query_governed(
                &mut db,
                cx,
                &query,
                policy(1, 9),
                |_| -> Result<ElementId, ()> { panic!("a complete match does not allocate") },
            )
            .unwrap();
        assert_eq!(stats.merge.created_vertices, 0);
        assert_eq!(stats.merge.match_selection.result_rows, 3);
        assert_eq!(stats.action_effects, 9);
        assert_eq!(rows.rows.result_rows, 1);
        assert_eq!(
            rows.rows.snapshot_records,
            stats.merge.match_selection.snapshot_records
        );
        assert_eq!(rows.value[0].values(), &[int(3), int(72)]);
        assert!(matches!(
            txn.finish(&mut db, commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            read(
                &db,
                cx,
                "MATCH (n:Item) RETURN n.p AS p, n.q AS q ORDER BY p"
            ),
            vec![
                vec![int(22), int(8)],
                vec![int(24), int(8)],
                vec![int(26), int(8)]
            ]
        );
    });
}

#[test]
fn batch_autocommit_creates_once_then_matches_without_allocating_again() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let text = "MERGE (n:Item {q: 7}) ON CREATE SET n.p = 4 \
                    ON MATCH SET n.p = n.p + 1 SET n.p = n.p * 2 RETURN n.p AS p";
        let query = prepare(text);
        let mut allocated = 0;
        let (created, first, _) = db
            .execute_graph_vertex_upserts_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                policy(1, 100),
                |request| {
                    assert!(matches!(
                        request,
                        fgdb_gql::insertion::GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    allocated += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(100)))
                },
            )
            .await
            .unwrap();
        assert_eq!(allocated, 1);
        assert_eq!(created.merge.created_vertices, 1);
        assert_eq!(first.value[0].values(), &[int(8)]);
        let (matched, second, _) = db
            .execute_graph_vertex_upserts_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                policy(1, 100),
                |_| -> Result<ElementId, ()> { panic!("existing target must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(matched.merge.created_vertices, 0);
        assert_eq!(second.value[0].values(), &[int(18)]);
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n AS n, n.p AS p"),
            vec![vec![GraphValue::Vertex(VId(100)), int(18)]]
        );
    });
}

#[test]
fn a_late_action_and_a_late_tail_failure_restore_every_target_and_the_prior_prefix() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit).await;
        for (at, text) in [
            "MERGE (n:Item) ON MATCH SET n.p = 12 / (n.p - 2) RETURN n.p AS p",
            "MERGE (n:Item) ON MATCH SET n.p = n.p + 10 \
             WITH n AS node RETURN 12 / (node.p - 12) AS p LIMIT 0",
        ]
        .into_iter()
        .enumerate()
        {
            let mut txn = db.begin(txcx).unwrap();
            let marker = VId(900 + at as u128);
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(marker, vec![], vec![(P, CanonicalScalar::Int(99))]);
            txn.write(&mut db, prefix).unwrap();
            let error = txn
                .execute_graph_vertex_upserts_query_engine_governed(
                    &mut db,
                    cx,
                    &prepare(text),
                    policy(100, 100),
                )
                .unwrap_err();
            assert!(matches!(
                error,
                GqlQueryError::Source(
                    GraphVertexUpsertError::Expression { .. }
                        | GraphVertexUpsertError::Returning(_)
                )
            ));
            txn.finish(&mut db, commit).await.unwrap();
            assert_eq!(
                read(&db, cx, "MATCH (n:Item) RETURN n.p AS p ORDER BY p"),
                vec![vec![int(1)], vec![int(2)], vec![int(3)]]
            );
            assert!(
                read(&db, cx, "MATCH (n) RETURN n AS n")
                    .contains(&vec![GraphValue::Vertex(marker)])
            );
        }
    });
}

#[test]
fn action_cap_is_statement_wide_but_a_filtered_output_keeps_all_effects() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit).await;
        let query = prepare(
            "MERGE (n:Item) ON MATCH SET n.p = n.p + 10 SET n.q = 8 \
             WITH n.p AS p WHERE p < 0 RETURN p",
        );
        let mut txn = db.begin(txcx).unwrap();
        let error = txn
            .execute_graph_vertex_upserts_query_engine_governed(&mut db, cx, &query, policy(0, 5))
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: 5,
                observed: 6
            })
        ));
        // Retry in the same transaction after the refused statement. There
        // must be no partial +10 from the earlier attempt.
        let (_, result) = txn
            .execute_graph_vertex_upserts_query_engine_governed(&mut db, cx, &query, policy(0, 6))
            .unwrap();
        assert!(result.value.is_empty());
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n:Item) RETURN n.p AS p ORDER BY p"),
            vec![vec![int(11)], vec![int(12)], vec![int(13)]]
        );
    });
}

#[test]
fn explicit_unique_api_stays_unique_while_batch_accepts_all_matches() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit).await;
        let query = prepare("MERGE (n:Item) RETURN count(*) AS n");
        let mut txn = db.begin(txcx).unwrap();
        let error = txn
            .execute_graph_vertex_upsert_query_engine_governed(
                &mut db,
                cx,
                &query,
                policy(100, 100),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexUpsertError::Merge(
                GraphVertexMergeError::AmbiguousMatches { observed: 3 }
            ))
        ));
        let (_, result) = txn
            .execute_graph_vertex_upserts_query_engine_governed(&mut db, cx, &query, policy(1, 100))
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(3)]);
        txn.finish(&mut db, commit).await.unwrap();
    });
}
