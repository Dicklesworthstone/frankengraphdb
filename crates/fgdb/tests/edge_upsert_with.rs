//! Relationship ON/SET -> WITH stays within the native statement's rollback
//! boundary. These tests exercise the real transaction and commit adapters.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertPolicy,
    GraphEdgeUpsertQueryError, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const PREFIX: &str = "MATCH (a:Item)-[:S]->(b:Item) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1 SET r.p=r.p*2";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn policy(rows: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000)),
        100,
    )
}
fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(&format!("{PREFIX} {tail}"), S, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
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
        batch.create_vertex(VId(id), vec![ITEM], vec![]);
    }
    if input {
        // Multiple occurrences retain the existing unique-pair upsert law.
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
        panic!("native rows expected")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("plain value expected"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn filtered_create_commits_then_with_reads_the_matched_branch_after_trailing_set() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let edge = EId((1_u128 << 100) + 90);
        let filtered = prepare("WITH r AS edge WHERE edge.p < 0 RETURN edge.p AS p");
        let (stats, outcome, output, completion) = db
            .execute_graph_edge_upsert_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &filtered,
                policy(0),
                |_| Ok::<_, ()>(ElementId::Edge(edge)),
            )
            .await
            .unwrap();
        assert!(outcome.created());
        assert_eq!(outcome.edge(), Some(edge));
        assert!(output.value.is_empty());
        assert_eq!(output.rows.result_rows, 0);
        assert!(output.evaluator.work_units > stats.evaluator.work_units);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(14)]]
        );

        let mut txn = db.begin(txcx).unwrap();
        let query = prepare("WITH r AS edge WHERE edge.p > 0 RETURN edge.p AS p, edge AS identity");
        let (_, outcome, output) = txn
            .execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &query,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("a matched edge must not allocate") },
            )
            .unwrap();
        assert!(!outcome.created());
        assert_eq!(outcome.edge(), Some(edge));
        assert_eq!(output.value.len(), 1);
        assert_eq!(output.value[0].values(), &[int(30), GraphValue::Edge(edge)]);
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(30)]]
        );
    });
}

#[test]
fn late_tail_failure_rolls_back_creation_and_updates_but_preserves_prior_statements() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(80), vec![ITEM], vec![(P, CanonicalScalar::Int(80))]);
        txn.write(&mut db, prefix).unwrap();
        let query = prepare("WITH r.p AS p RETURN 10/(p-14) AS n LIMIT 0");
        let failure = txn
            .execute_graph_edge_upsert_query_governed(&mut db, cx, &query, policy(0), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(90)))
            })
            .unwrap_err();
        assert!(matches!(
            failure,
            GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
        txn.finish(&mut db, commit).await.unwrap();
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
        assert_eq!(
            read(&db, cx, "MATCH (n:Item {p:80}) RETURN n.p AS p"),
            vec![vec![int(80)]]
        );

        let mut txn = db.begin(txcx).unwrap();
        txn.execute_graph_edge_upsert_query_governed(
            &mut db,
            cx,
            &prepare("WITH r RETURN r.p AS p"),
            policy(1),
            |_| Ok::<_, ()>(ElementId::Edge(EId(91))),
        )
        .unwrap();
        // The first accepted statement created p=14. The refused second
        // would write p=30; its API-level LIMIT 0 cannot conceal division by 0.
        let failing = prepare("WITH r.p AS p RETURN 10/(p-30) AS n").with_page(0, Some(0));
        assert!(matches!(
            txn.execute_graph_edge_upsert_query_governed(
                &mut db,
                cx,
                &failing,
                policy(0),
                |_| -> Result<ElementId, ()> { panic!("matched path") },
            ),
            Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(
                GraphSetExecutionError::Projection { .. }
            )))
        ));
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p"),
            vec![vec![int(14)]]
        );
    });
}

#[test]
fn absent_match_produces_no_effects_and_native_tail_aggregation_reports_zero() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        for tail in ["WITH r RETURN count(*) AS n", "WITH count(*) AS n RETURN n"] {
            let (_, outcome, output) = txn
                .execute_graph_edge_upsert_query_governed(
                    &mut db,
                    cx,
                    &prepare(tail),
                    policy(1),
                    |_| -> Result<ElementId, ()> { panic!("NoInput must not allocate") },
                )
                .unwrap();
            assert_eq!(outcome.edge(), None);
            assert_eq!(output.value.len(), 1);
            assert_eq!(output.value[0].values(), &[int(0)]);
        }
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
    });
}
