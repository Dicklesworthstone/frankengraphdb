//! Composite MATCH keys share the native three-valued equality law across
//! direct UNWIND, WITH joins, reads and atomic graph writes (fgdb-2npty).

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphSetText, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId,
};

const NODE: LabelId = LabelId(1);
const PROBE: LabelId = LabelId(2);
const COPY: LabelId = LabelId(3);
const ID: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn names(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(NODE)),
        (GraphSymbolKind::Label, "Probe") => Some(GraphSymbol::Label(PROBE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000)
}
fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn float(n: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(n)))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn map(value: GraphValue) -> GraphValue {
    GraphValue::map(vec![("key".into(), list(vec![value]))]).unwrap()
}
fn vertex(n: u128) -> VId {
    VId((1_u128 << 100) + n)
}
fn parameters(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            values
                .into_iter()
                .map(|value| GraphValue::map(vec![("id".into(), value)]).unwrap())
                .collect(),
        )
        .unwrap()
}
fn xs(result: QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read rows")
    };
    rows.into_iter()
        .map(|row| match row.as_slice() {
            [QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))] => *n,
            other => panic!("one integer column: {other:?}"),
        })
        .collect()
}
fn row_xs(rows: &[GraphValueRow]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(n) => *n,
            _ => panic!("integer marker"),
        })
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (n, label, key) in [
        (1, NODE, list(vec![int(1), int(2)])),
        (2, NODE, list(vec![float(1.0), float(2.0)])),
        (3, NODE, map(int(3))),
        (4, NODE, map(float(3.0))),
        (5, NODE, list(vec![null(), int(2)])),
        (6, NODE, map(null())),
        (7, NODE, list(vec![int(9_007_199_254_740_993)])),
        (8, NODE, list(vec![float(9_007_199_254_740_992.0)])),
        (11, NODE, int(7)),
        (12, NODE, float(7.0)),
        (21, PROBE, list(vec![float(1.0), int(2)])),
        (22, PROBE, map(float(3.0))),
        (23, PROBE, float(7.0)),
    ] {
        batch.create_vertex_values(
            vertex(n),
            vec![label],
            vec![
                (ID, key.to_property_value().unwrap()),
                (X, CanonicalScalar::Int(n as i64 * 10).into()),
            ],
        );
    }
    db.write(cx, batch).await.unwrap();
}

#[test]
fn composite_keys_match_across_direct_with_and_stored_property_inputs() {
    let ((), report) = run_async_under_lab(0xc011_2a01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (contexts.query(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let params = parameters(vec![
            list(vec![int(1), float(2.0)]),
            list(vec![float(1.0), int(2)]),
            map(float(3.0)),
            list(vec![null(), int(2)]),
            map(null()),
            list(vec![int(9_007_199_254_740_993)]),
        ]);
        for text in [
            "UNWIND $rows AS row MATCH (a:Node {id:row.id}) RETURN a.x AS x ORDER BY x",
            "UNWIND $rows AS row WITH row MATCH (a:Node {id:row.id}) RETURN a.x AS x ORDER BY x",
            "UNWIND $rows AS row WITH row.id AS key MATCH (a:Node {id:key}) RETURN a.x AS x ORDER BY x",
            "UNWIND $rows AS row MATCH (a:Node) WHERE a.id = row.id RETURN a.x AS x ORDER BY x",
            "UNWIND $rows AS row MATCH (a:Node) WHERE row.id = a.id RETURN a.x AS x ORDER BY x",
        ] {
            let result = db
                .query(&cx, text, &params, names, policy())
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(xs(result), [10, 10, 20, 20, 30, 40, 70], "{text}");
        }
        // This left column is Stored, not UNWIND's Any. Textual equality
        // must never be strengthened to canonical typed identity at WITH.
        let text = "MATCH (source:Probe) WITH source.id AS key MATCH (a:Node {id:key}) RETURN a.x AS x ORDER BY x";
        assert_eq!(
            xs(db
                .query(&cx, text, &GqlParameters::new(), names, policy())
                .unwrap()),
            [10, 20, 30, 40, 110, 120]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn composite_write_matches_preserve_overlay_bags_and_rollback_on_late_failure() {
    let ((), report) = run_async_under_lab(0xc011_2a02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txcx) = (contexts.query(), contexts.commit(), contexts.txn());
        for head in [
            "UNWIND $rows AS row MATCH (a:Node {id:row.id})",
            "UNWIND $rows AS row WITH row.id AS key MATCH (a:Node {id:key})",
        ] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.set_vertex_property_value(
                vertex(1),
                ID,
                Some(list(vec![int(99), int(99)]).to_property_value().unwrap()),
            );
            staged.create_vertex_values(
                vertex(9),
                vec![NODE],
                vec![
                    (ID, list(vec![int(1), int(2)]).to_property_value().unwrap()),
                    (X, CanonicalScalar::Int(90).into()),
                ],
            );
            txn.write(&mut db, staged).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let frontier = db.frontier().unwrap();
            let params = parameters(vec![
                list(vec![float(1.0), int(2)]),
                list(vec![int(1), float(2.0)]),
                list(vec![null(), int(2)]),
            ]);
            let text = format!("{head} CREATE (a)-[:R]->(:Copy {{x:a.x}})");
            let failed = PreparedGraphWriteScript::prepare(
                &format!("{text}; MATCH (n:Copy) SET n.x=1/0"),
                R,
                names,
            )
            .unwrap()
            .bind_parameters(&params)
            .unwrap();
            let allowance = GraphWriteProgramPolicy::new(policy(), 1_000, 1_000, 1_000);
            assert!(
                txn.execute_graph_write_program_engine_governed(&mut db, &cx, &failed, allowance)
                    .is_err(),
                "{head}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert!(txn.edges(&db).unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), frontier);
            let program = PreparedGraphWriteScript::prepare(&text, R, names)
                .unwrap()
                .bind_parameters(&params)
                .unwrap();
            txn.execute_graph_write_program_engine_governed(&mut db, &cx, &program, allowance)
                .unwrap();
            assert_eq!(
                txn.edges(&db).unwrap().len(),
                4,
                "duplicate input rows retain their matches"
            );
            let read =
                PreparedGraphSetText::prepare("MATCH (n:Copy) RETURN n.x AS x ORDER BY x", names)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap();
            assert_eq!(
                row_xs(
                    &txn.execute_graph_set_governed(&db, &cx, &read, policy())
                        .unwrap()
                        .value
                ),
                [20, 20, 90, 90]
            );
            txn.finish(&mut db, &commit).await.unwrap();
            // The writer handle must close before the database reopens.
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                row_xs(
                    &reopened
                        .execute_graph_set_governed(&cx, &read, policy())
                        .unwrap()
                        .value
                ),
                [20, 20, 90, 90]
            );
            assert_eq!(reopened.edges().unwrap().len(), 4);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
