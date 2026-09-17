//! List semantics through the public native read entrypoint under the lab runtime.
//! Indexes are zero-based; negative indexes count from the end; missing indexes
//! yield NULL. UNWIND follows openCypher: NULL/empty lists produce zero rows.
//! COLLECT retains deterministic input order, skipping NULL; DISTINCT keeps the
//! first occurrence. These rules are asserted independently of the evaluator.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p" | "id") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, int(3)), (2, null()), (3, int(1)), (4, int(3))] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(P, value.as_scalar().unwrap().clone())],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn rows(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
) -> Vec<Vec<GraphValue>> {
    match db.query(cx, text, params, symbols, policy()).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| match cell {
                        GraphAggregateValue::Value(value) => value,
                        other => panic!("expected typed value, got {other:?}"),
                    })
                    .collect()
            })
            .collect(),
        QueryResult::Write { .. } => panic!("read unexpectedly classified as write"),
    }
}

#[test]
fn literals_nested_null_index_size_and_with_are_lossless() {
    let ((), report) = run_async_under_lab(0x1157_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = seeded(&contexts.commit()).await;
        let cx = contexts.query();
        let params = GqlParameters::new();
        assert_eq!(
            rows(
                &db,
                &cx,
                "MATCH (n) WHERE n.p=1 RETURN [7,TRUE,'hello',NULL,[2,3]] AS xs",
                &params
            ),
            vec![vec![list(vec![
                int(7),
                GraphValue::Scalar(CanonicalScalar::Bool(true)),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("hello").unwrap()),
                null(),
                list(vec![int(2), int(3)])
            ])]]
        );
        assert_eq!(
            rows(
                &db,
                &cx,
                "MATCH (n) WHERE n.p=1 WITH [7,NULL,[2,3]] AS xs RETURN size(xs) AS length,xs[0] AS first,xs[-1] AS last,xs[3] AS past,xs[-4] AS before,xs[1] AS nil",
                &params
            ),
            vec![vec![
                int(3),
                int(7),
                list(vec![int(2), int(3)]),
                null(),
                null(),
                null()
            ]]
        );
        assert_eq!(
            rows(
                &db,
                &cx,
                "MATCH (n) WHERE n.p=1 RETURN size(NULL) AS length,NULL[0] AS item,size([]) AS empty",
                &params
            ),
            vec![vec![null(), null(), int(0)]]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
#[test]
fn unwind_parameters_match_and_empty_inputs_preserve_multiplicity() {
    let ((), report) = run_async_under_lab(0x1157_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = seeded(&contexts.commit()).await;
        let cx = contexts.query();
        let params = GqlParameters::new()
            .with_list("ids", vec![int(3), int(1), int(3)])
            .unwrap();
        assert_eq!(
            rows(
                &db,
                &cx,
                "UNWIND $ids AS id MATCH (n {id:id}) RETURN n",
                &params
            ),
            vec![
                vec![GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(4))],
                vec![GraphValue::Vertex(VId(3))],
                vec![GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(4))]
            ]
        );
        for expression in ["[]", "NULL"] {
            assert_eq!(
                rows(
                    &db,
                    &cx,
                    &format!("UNWIND {expression} AS x RETURN x"),
                    &GqlParameters::new()
                ),
                Vec::<Vec<GraphValue>>::new()
            );
        }
        assert_eq!(
            rows(
                &db,
                &cx,
                "MATCH (n) WHERE n.p=1 WITH [2,NULL,2] AS xs UNWIND xs AS x RETURN x",
                &GqlParameters::new()
            ),
            vec![vec![int(2)], vec![null()], vec![int(2)]]
        );
        assert_eq!(
            rows(&db, &cx, "MATCH (n {p:1}) RETURN n", &GqlParameters::new()),
            vec![vec![GraphValue::Vertex(VId(3))]]
        );
        assert!(
            db.query(
                &cx,
                "UNWIND $ids AS id MATCH (n) OPTIONAL MATCH (m {p:id}) RETURN n",
                &params,
                symbols,
                policy()
            )
            .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collect_order_distinct_nulls_temporal_and_identical_databases() {
    let ((), report) = run_async_under_lab(0x1157_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = seeded(&commit).await;
        let twin = seeded(&commit).await;
        let params = GqlParameters::new();
        let query =
            "MATCH (n) RETURN collect(n.p) AS all_values,collect(DISTINCT n.p) AS distinct_values";
        let expected = vec![vec![
            list(vec![int(3), int(1), int(3)]),
            list(vec![int(3), int(1)]),
        ]];
        let first = rows(&db, &cx, query, &params);
        assert_eq!(first, expected);
        let other = rows(&twin, &cx, query, &params);
        assert_eq!(first, other);
        let bytes = |rows: &[Vec<GraphValue>]| {
            rows.iter()
                .map(|row| {
                    row.iter()
                        .map(GraphValue::canonical_bytes)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(bytes(&first), bytes(&other));
        let before = db.frontier().unwrap();
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(9)));
        db.write(&commit, batch).await.unwrap();
        assert_eq!(
            rows(
                &db,
                &cx,
                &format!(
                    "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN collect(n.p) AS xs",
                    before.0
                ),
                &params
            ),
            vec![vec![list(vec![int(3), int(1), int(3)])]]
        );
        assert_eq!(
            rows(&db, &cx, "MATCH (n) RETURN collect(n.p) AS xs", &params),
            vec![vec![list(vec![int(3), int(9), int(3)])]]
        );
        assert_eq!(
            rows(
                &db,
                &cx,
                "MATCH (n) WHERE n.p=999 RETURN collect(n.p) AS xs",
                &params
            ),
            vec![vec![list(vec![])]]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
