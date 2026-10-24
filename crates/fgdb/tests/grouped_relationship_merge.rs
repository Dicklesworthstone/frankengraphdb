//! fgdb-cpny9: grouped relationship-MERGE continuations use real transaction
//! state, final-field readers and the existing statement rollback workspace.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, TxnCx,
    VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const EDGE: EId = EId((1_u128 << 100) + 7);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn policy(rows: u64) -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000))
}

fn prepare(text: &str) -> PreparedGraphEdgeMergeQuery {
    PreparedGraphEdgeMergeQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
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

async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx, parallel: bool) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id as u128),
            vec![],
            vec![(P, CanonicalScalar::Int(id * 10))],
        );
    }
    if parallel {
        batch.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
        batch.add_edge(EId(12), VId(1), VId(2), vec![(P, CanonicalScalar::Int(9))]);
    }
    db.write(commit, batch).await.unwrap();
}

fn read(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, &GqlParameters::new(), symbols, policy(100).query)
        .expect(text)
    else {
        panic!("expected native row result: {text}");
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected ordinary row-domain values"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn grouped_creation_keeps_occurrences_and_final_edge_endpoint_and_match_fields() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let query = prepare(
            "MATCH (a {p:10}),(b {p:20}),(duplicate) MERGE (a)-[r:R {p:7}]->(b) \
             WITH r AS edge, count(*) AS n, a AS matched, startNode(r) AS source \
             WHERE edge.p > 0 RETURN n, edge.p AS p, matched.p AS mp, source.p AS sp",
        );
        let before = db.frontier().unwrap();
        let mut allocated = 0;
        let (_, output, _) = db
            .execute_graph_edge_merge_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                policy(1),
                |_| {
                    allocated += 1;
                    Ok::<_, ()>(ElementId::Edge(EDGE))
                },
            )
            .await
            .unwrap();
        assert_eq!(allocated, 1);
        assert_eq!(output.value.len(), 1);
        assert_eq!(
            output.value[0].values(),
            &[int(3), int(7), int(10), int(10)]
        );
        assert_ne!(db.frontier().unwrap(), before);
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r, r.p"),
            vec![vec![GraphValue::Edge(EDGE), int(7)]]
        );
    });
}

#[test]
fn parallel_groups_are_filtered_before_the_final_one_row_grant() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, true).await;
        let query = prepare(
            "MATCH (a {p:10}),(b {p:20}),(duplicate) MERGE (a)-[r:R]->(b) \
             WITH r AS edge, count(*) AS n WHERE edge.p > 7 RETURN n, edge.p AS p",
        );
        let (_, output, _) = db
            .execute_graph_edge_merge_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("both relationships already exist") },
            )
            .await
            .unwrap();
        assert_eq!(output.value.len(), 1);
        assert_eq!(output.value[0].values(), &[int(3), int(9)]);
        assert_eq!(output.rows.result_rows, 1);
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r.p AS p ORDER BY p"),
            vec![vec![int(7)], vec![int(9)]]
        );
    });
}

#[test]
fn filtered_grouped_output_still_commits_the_created_relationship() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let query = prepare(
            "MATCH (a {p:10}),(b {p:20}) MERGE (a)-[r:R {p:7}]->(b) \
             WITH r AS edge, count(*) AS n WHERE edge.p < 0 RETURN n",
        );
        let before = db.frontier().unwrap();
        let (_, output, _) = db
            .execute_graph_edge_merge_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                policy(0),
                |_| Ok::<_, ()>(ElementId::Edge(EDGE)),
            )
            .await
            .unwrap();
        assert!(output.value.is_empty());
        assert_eq!(output.rows.result_rows, 0);
        assert_ne!(db.frontier().unwrap(), before);
        assert_eq!(
            read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r"),
            vec![vec![GraphValue::Edge(EDGE)]]
        );
    });
}

#[test]
fn failing_grouped_tail_rolls_back_creation_but_keeps_the_transaction_prefix() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        seed(&mut db, commit, false).await;
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(80), vec![], vec![(P, CanonicalScalar::Int(80))]);
        txn.write(&mut db, prefix).unwrap();
        let query = prepare(
            "MATCH (a {p:10}),(b {p:20}) MERGE (a)-[r:R {p:0}]->(b) \
             WITH r AS edge, count(*) AS n RETURN 12 / edge.p AS value LIMIT 0",
        );
        let before = db.frontier().unwrap();
        let mut allocated = 0;
        let result =
            txn.execute_graph_edge_merge_query_governed(&mut db, cx, &query, policy(0), |_| {
                allocated += 1;
                Ok::<_, ()>(ElementId::Edge(EDGE))
            });
        assert!(result.is_err());
        assert_eq!(
            allocated, 1,
            "the error follows actual creation, not preparation"
        );
        assert_eq!(
            db.frontier().unwrap(),
            before,
            "no failed statement publishes"
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert!(read(&db, cx, "MATCH (a)-[r:R]->(b) RETURN r").is_empty());
        assert_eq!(
            read(&db, cx, "MATCH (n {p:80}) RETURN n.p"),
            vec![vec![int(80)]]
        );
    });
}
