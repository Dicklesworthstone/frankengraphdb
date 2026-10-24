//! Grouped vertex-MERGE WITH reads final branch/SET values under the existing
//! transaction acceptance boundary. Result filtering never suppresses effects.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphVertexMergePolicy,
    GraphVertexUpsertPolicy, PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, TxnCx, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const TARGET: VId = VId((1_u128 << 100) + 7);
const PREFIX: &str = "MERGE (n {q:9}) ON CREATE SET n.p=3 ON MATCH SET n.p=n.p+1 SET n.p=n.p*2";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x74; 32],
        DatabaseSecurityNamespaceId([0x75; 32]),
        [0x76; 32],
    )
}

fn policy(rows: u64) -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, rows, 1_000_000, 100_000)),
        100,
    )
}

fn prepare(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
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

fn read(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(
            cx,
            text,
            &GqlParameters::new(),
            symbols,
            policy(100).merge.query,
        )
        .expect(text)
    else {
        panic!("expected rows")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected ordinary native value"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn grouped_return_reports_final_create_match_and_trailing_set_values() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let query = prepare(&format!(
            "{PREFIX} WITH n AS item, count(*) AS c \
            WHERE item.p > 0 RETURN item AS identity, item.p AS p, c"
        ));
        let mut txn = db.begin(txcx).unwrap();
        let (_, first, created) = txn
            .execute_graph_vertex_upsert_query_governed(&mut db, cx, &query, policy(1), |_| {
                Ok::<_, ()>(ElementId::Vertex(TARGET))
            })
            .unwrap();
        assert!(first.created());
        assert_eq!(created.value.len(), 1);
        assert_eq!(
            created.value[0].values(),
            &[GraphValue::Vertex(TARGET), int(6), int(1)]
        );
        let (_, second, matched) = txn
            .execute_graph_vertex_upsert_query_governed(
                &mut db,
                cx,
                &query,
                policy(1),
                |_| -> Result<ElementId, ()> { panic!("the chosen vertex already exists") },
            )
            .unwrap();
        assert!(!second.created());
        assert_eq!(second.vertex(), TARGET);
        assert_eq!(
            matched.value[0].values(),
            &[GraphValue::Vertex(TARGET), int(14), int(1)]
        );
        assert_eq!(
            created.value[0].values(),
            &[GraphValue::Vertex(TARGET), int(6), int(1)]
        );
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n {q:9}) RETURN n.p"),
            vec![vec![int(14)]]
        );

        let filtered = prepare(&format!(
            "{PREFIX} WITH n AS item, count(*) AS c \
            WHERE item.p < 0 RETURN c"
        ));
        let mut txn = db.begin(txcx).unwrap();
        let (_, _, output) = txn
            .execute_graph_vertex_upsert_query_governed(
                &mut db,
                cx,
                &filtered,
                policy(0),
                |_| -> Result<ElementId, ()> { panic!("the chosen vertex already exists") },
            )
            .unwrap();
        assert!(output.value.is_empty());
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n {q:9}) RETURN n.p"),
            vec![vec![int(30)]]
        );
    });
}

#[test]
fn a_failed_grouped_tail_restores_branch_updates_and_preserves_prior_writes() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            TARGET,
            vec![],
            vec![(P, CanonicalScalar::Int(6)), (Q, CanonicalScalar::Int(9))],
        );
        db.write(commit, seed).await.unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(80), vec![], vec![(P, CanonicalScalar::Int(80))]);
        txn.write(&mut db, prefix).unwrap();
        let query = prepare(
            "MERGE (n {q:9}) ON MATCH SET n.p=100 SET n.p=0 \
             WITH n AS item, count(*) AS c RETURN 12/item.p AS value LIMIT 0",
        );
        let result = txn.execute_graph_vertex_upsert_query_governed(
            &mut db,
            cx,
            &query,
            policy(0),
            |_| -> Result<ElementId, ()> { panic!("the existing match must not allocate") },
        );
        assert!(result.is_err());
        txn.finish(&mut db, commit).await.unwrap();
        assert_eq!(
            read(&db, cx, "MATCH (n {q:9}) RETURN n.p"),
            vec![vec![int(6)]]
        );
        assert_eq!(
            read(&db, cx, "MATCH (n {p:80}) RETURN n.p"),
            vec![vec![int(80)]]
        );
    });
}
