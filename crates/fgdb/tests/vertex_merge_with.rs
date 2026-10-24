//! fgdb-cpny9: vertex MERGE WITH tails use the real staged vertex, one
//! publication, and the same rollback boundary as ON clauses and RETURN.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertBranch, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};

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
