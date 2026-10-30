//! Product checks for indexed closing constraints, with independent row oracles.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a)";
type Plain = (VId, VId, Option<i64>);
type Summary = (VId, u64, Option<i128>);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(T)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}
fn pattern() -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!("{HEAD} RETURN a,c,c.score AS score"), symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn aggregate() -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(
        &format!(
            "{HEAD} RETURN a,COUNT(*) AS n,SUM(c.score) AS total GROUP BY a ORDER BY n DESC,a ASC"
        ),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0, 1, 10, 11, 99] {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    for (id, score) in [(20, 5), (21, 7), (30, 9)] {
        vertices.create_vertex(VId(id), vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    let mut first = WriteBatch::new(R);
    for (eid, source, destination) in [(10, 0, 10), (11, 0, 10), (12, 1, 11)] {
        first.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, source, destination) in [
        (20, 10, 20),
        (21, 10, 20),
        (22, 10, 21),
        (23, 11, 21),
        (24, 10, 30),
    ] {
        second.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    let mut third = WriteBatch::new(T);
    for (eid, source, destination) in [
        (30, 20, 0),
        (31, 20, 0),
        (32, 20, 0),
        (33, 21, 1),
        (34, 30, 99),
    ] {
        third.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second, third])
        .await
        .unwrap()
}

fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut result = Vec::new();
    for first in edges.iter().filter(|row| row.entry.relation == R) {
        for second in edges
            .iter()
            .filter(|row| row.entry.relation == S && row.entry.src == first.entry.dst)
        {
            for _ in edges.iter().filter(|row| {
                row.entry.relation == T
                    && row.entry.src == second.entry.dst
                    && row.entry.dst == first.entry.src
            }) {
                let vertex = vertices
                    .iter()
                    .find(|row| row.vid == second.entry.dst)
                    .unwrap();
                let score = vertex.props.iter().find_map(|(key, value)| match value {
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value))
                        if *key == SCORE =>
                    {
                        Some(*value)
                    }
                    _ => None,
                });
                result.push((first.entry.src, second.entry.dst, score));
            }
        }
    }
    result.sort_unstable();
    result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter()
        .map(|row| {
            let score = match row.get(2).unwrap().as_scalar().unwrap() {
                CanonicalScalar::Int(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => panic!("unexpected fixture value"),
            };
            (
                row.get(0).unwrap().as_vertex().unwrap(),
                row.get(1).unwrap().as_vertex().unwrap(),
                score,
            )
        })
        .collect()
}
fn expected_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, (u64, Option<i128>)> = BTreeMap::new();
    for &(key, _, value) in rows {
        let group = groups.entry(key).or_insert((0, None));
        group.0 += 1;
        if let Some(value) = value {
            group.1 = Some(group.1.unwrap_or(0) + i128::from(value));
        }
    }
    let mut result: Vec<_> = groups
        .into_iter()
        .map(|(key, (count, sum))| (key, count, sum))
        .collect();
    result.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0));
    result
}
fn summary(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_integer(),
            )
        })
        .collect()
}

#[test]
fn indexed_pattern_and_aggregation_agree_across_all_five_read_entrypoints() {
    let ((), report) = run_async_under_lab(0x10a1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let pattern = pattern();
        let aggregate = aggregate();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(expected.len(), 13);
        assert_eq!(
            expected_summary(&expected),
            vec![(VId(0), 12, Some(60)), (VId(1), 1, Some(7))]
        );
        for result in [
            db.execute_graph_pattern_governed(&cx, &pattern, policy())
                .unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                .unwrap(),
            view.execute_graph_pattern_governed(&cx, &pattern, policy())
                .unwrap(),
            view.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                .unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                .unwrap(),
        ] {
            assert_eq!(plain(&result.value), expected);
        }
        for result in [
            db.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                .unwrap(),
        ] {
            assert_eq!(summary(&result.value), expected_summary(&expected));
        }
        let measured = db
            .execute_graph_pattern_governed(&cx, &pattern, policy())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &pattern, exact)
                .unwrap(),
            measured
        );
        for cap in [
            GqlQueryPolicy::new(measured.rows.snapshot_records, 12, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                13,
                measured.evaluator.work_units - 1,
                u64::MAX,
            ),
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                13,
                u64::MAX,
                measured.evaluator.scratch_entries - 1,
            ),
        ] {
            assert!(
                db.execute_graph_pattern_governed(&cx, &pattern, cap)
                    .is_err()
            );
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_results_follow_canonical_staging_and_survive_compaction_and_reopening() {
    let ((), report) = run_async_under_lab(0x10a1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let pattern = pattern();
        let aggregate = aggregate();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut changes = WriteBatch::new(T);
        changes.delete_edge(EId(30));
        changes.ensure_edge_by_triple(EId(999), VId(20), VId(0), vec![]);
        changes.add_edge(EId(90), VId(21), VId(0), vec![]);
        changes.set_vertex_property(VId(20), SCORE, None);
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected.len(), 11);
        assert_eq!(
            plain(
                &txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summary(
                &txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summary(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            plain(
                &view
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            old
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_closing_constraints_keep_phantom_and_rejected_vertex_dependencies() {
    let ((), report) = run_async_under_lab(0x10a1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let pattern = pattern();
        for refused in [false, true] {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let cap =
                    GqlQueryPolicy::new(1000, if refused { 0 } else { 1000 }, 1_000_000, 1_000_000);
                let result = txn.execute_graph_pattern_governed(&db, &cx, &pattern, cap);
                if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                } else {
                    assert_eq!(result.unwrap().value.len(), 13);
                }
                let mut winner = WriteBatch::new(T);
                match change {
                    0 => winner.create_vertex(VId(888), vec![], vec![]),
                    1 => winner.add_edge(EId(91), VId(30), VId(0), vec![]),
                    _ => winner.set_vertex_property(VId(30), SCORE, Some(CanonicalScalar::Int(20))),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No extra transaction read is allowed to repair a lost witness.
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-2npty: `UNWIND $rows AS row MATCH (a {id: row.id})` admits only the
/// roots whose `id` posting a row value names, not every vertex. 401
/// vertices sit behind an allowance of 20 records, so a scan refuses; the
/// generous answer is the semantic oracle at every step.
#[test]
fn row_keyed_property_matches_admit_only_the_posted_roots() {
    use fgdb::{QueryResult, QueryValue};
    use fgdb_gql::algebra::GraphValue;
    const NODE: fgdb_delta_types::LabelId = fgdb_delta_types::LabelId(1);
    const ID: PropertyKeyId = PropertyKeyId(2);
    const X: PropertyKeyId = PropertyKeyId(3);
    fn names(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(NODE)),
            (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
            (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
            _ => None,
        }
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn rows(ids: Vec<Option<GraphValue>>) -> GqlParameters {
        let rows = ids
            .into_iter()
            .map(|id| GraphValue::map(id.map(|id| ("id".into(), id)).into_iter().collect()))
            .collect::<Option<Vec<_>>>()
            .unwrap();
        GqlParameters::new().with_list("rows", rows).unwrap()
    }
    fn xs(result: QueryResult) -> Vec<i64> {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("a read returns rows")
        };
        rows.into_iter()
            .map(|row| match row.as_slice() {
                [QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(x)))] => *x,
                other => panic!("one integer column, got {other:?}"),
            })
            .collect()
    }
    let ((), report) = run_async_under_lab(0x10a1_2a7f, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=400_i64 {
            batch.create_vertex(
                VId(id as u128),
                vec![NODE],
                vec![
                    (ID, CanonicalScalar::Int(id)),
                    (X, CanonicalScalar::Int(id * 10)),
                ],
            );
        }
        // A list-valued key no scalar posting names.
        let list = fgdb_types::CanonicalList::try_new(vec![
            CanonicalScalar::Int(1).into(),
            CanonicalScalar::Int(2).into(),
        ])
        .unwrap();
        batch.create_vertex_values(
            VId(401),
            vec![NODE],
            vec![
                (ID, CanonicalPropertyValue::List(list)),
                (
                    X,
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(4010)),
                ),
            ],
        );
        for id in 1..400_u128 {
            batch.add_edge(EId(id), VId(id), VId(id + 1), vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        // Vertex 7's id moves on: its id = 7 posting stays, as history.
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(7), ID, Some(CanonicalScalar::Int(700)));
        db.write(&commit, change).await.unwrap();

        let tight = GqlQueryPolicy::new(20, 1000, 1_000_000, 1_000_000);
        let float = |value: f64| {
            GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value)))
        };
        let text =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let vertex = "UNWIND $rows AS row MATCH (a:Node {id: row.id}) RETURN a.x AS x ORDER BY x";
        let edge = "UNWIND $rows AS row MATCH (a:Node {id: row.id})-[:R]->(b) \
            RETURN b.x AS x ORDER BY x";
        for (statement, ids, expected) in [
            // A duplicate row joins twice.
            (
                vertex,
                vec![Some(int(3)), Some(int(5)), Some(int(5)), Some(int(400))],
                vec![30, 50, 50, 4000],
            ),
            // Equality spans the numeric domains; a fraction names no id.
            (vertex, vec![Some(float(3.0)), Some(float(2.5))], vec![30]),
            // NULL and a missing field match nothing; text never equals Int.
            (
                vertex,
                vec![
                    Some(GraphValue::Scalar(CanonicalScalar::Null)),
                    None,
                    Some(text("9")),
                    Some(int(9)),
                ],
                vec![90],
            ),
            // The id = 7 posting is history: vertex 7 now answers to 700.
            (vertex, vec![Some(int(7)), Some(int(700))], vec![70]),
            // No row names a root: nothing is admitted, nothing returned.
            (vertex, vec![Some(int(1_000))], vec![]),
            // An edge-rooted pattern walks from the posted roots only.
            (edge, vec![Some(int(3)), Some(int(5))], vec![40, 60]),
        ] {
            let params = rows(ids);
            let reference = xs(db.query(&cx, statement, &params, names, policy()).unwrap());
            assert_eq!(reference, expected, "{statement}");
            assert_eq!(
                xs(db.query(&cx, statement, &params, names, tight).unwrap()),
                expected,
                "{statement}: the seeded read fits a 20-record allowance"
            );
        }
        // A key computed from the row is a private join column seeded the
        // same way, in the map or as a flat WHERE equality.
        let field =
            |name: &str, value: GraphValue| GraphValue::map(vec![(name.into(), value)]).unwrap();
        let texts = GqlParameters::new()
            .with_list("rows", vec![field("s", text("3")), field("s", text("5"))])
            .unwrap();
        let numbers = GqlParameters::new()
            .with_list("rows", vec![field("id", int(2)), field("id", int(4))])
            .unwrap();
        for (statement, params) in [
            (
                "UNWIND $rows AS row MATCH (a:Node {id: toInteger(row.s)}) RETURN a.x AS x \
                 ORDER BY x",
                &texts,
            ),
            (
                "UNWIND $rows AS row MATCH (a:Node {id: row.id + 1}) RETURN a.x AS x ORDER BY x",
                &numbers,
            ),
            (
                "UNWIND $rows AS row MATCH (a:Node) WHERE a.id = toInteger(row.s) \
                 RETURN a.x AS x ORDER BY x",
                &texts,
            ),
            (
                "UNWIND $rows AS row MATCH (a:Node) WHERE a.id = row.id + 1 \
                 RETURN a.x AS x ORDER BY x",
                &numbers,
            ),
            // A WITH stage joins the MATCH as a row join, seeded the same way.
            (
                "UNWIND $rows AS row WITH toInteger(row.s) AS k MATCH (a:Node {id: k}) \
                 RETURN a.x AS x ORDER BY x",
                &texts,
            ),
            (
                "UNWIND $rows AS row WITH row MATCH (a:Node {id: row.id + 1}) \
                 RETURN a.x AS x ORDER BY x",
                &numbers,
            ),
        ] {
            let reference = xs(db
                .query(&cx, statement, params, names, policy())
                .unwrap_or_else(|error| panic!("{statement}: {error:?}")));
            assert_eq!(reference, [30, 50], "{statement}");
            assert_eq!(
                xs(db
                    .query(&cx, statement, params, names, tight)
                    .unwrap_or_else(|error| panic!("{statement}: {error:?}"))),
                [30, 50],
                "{statement}: the seeded read fits a 20-record allowance"
            );
        }
        // The allowance is below one scan: an unkeyed correlation refuses.
        let unkeyed = "UNWIND $rows AS row MATCH (a:Node) WHERE a.x > row.id RETURN a.x AS x";
        let params = rows(vec![Some(int(4_005))]);
        assert_eq!(
            xs(db.query(&cx, unkeyed, &params, names, policy()).unwrap()),
            [4010]
        );
        assert!(db.query(&cx, unkeyed, &params, names, tight).is_err());
        // A list row may equal a stored list, which no scalar posting names,
        // so seeding declines and the read keeps the scan. The scan joins
        // the list element by element (fgdb-2npty): [1, 2] and [1.0, 2] find
        // vertex 401, while [1, null] is never TRUE-equal and finds nothing.
        let list = |items: Vec<GraphValue>| GraphValue::List(items.into_boxed_slice());
        let params = rows(vec![Some(list(vec![int(1), int(2)]))]);
        assert_eq!(
            xs(db.query(&cx, vertex, &params, names, policy()).unwrap()),
            [4010]
        );
        assert!(db.query(&cx, vertex, &params, names, tight).is_err());
        let filtered =
            "UNWIND $rows AS row MATCH (a:Node) WHERE a.id = row.id RETURN a.x AS x ORDER BY x";
        for (key, expected) in [
            (list(vec![float(1.0), int(2)]), vec![4010]),
            (
                list(vec![int(1), GraphValue::Scalar(CanonicalScalar::Null)]),
                vec![],
            ),
            (list(vec![int(2), int(1)]), vec![]),
        ] {
            let params = rows(vec![Some(key)]);
            for statement in [vertex, filtered] {
                assert_eq!(
                    xs(db.query(&cx, statement, &params, names, policy()).unwrap()),
                    expected,
                    "{statement}"
                );
            }
        }
        // A bare key list mixes a list key with a scalar one.
        let keyed = GqlParameters::new()
            .with_list("keys", vec![list(vec![int(1), int(2)]), int(3)])
            .unwrap();
        assert_eq!(
            xs(db
                .query(
                    &cx,
                    "UNWIND $keys AS key MATCH (a:Node {id: key}) RETURN a.x AS x ORDER BY x",
                    &keyed,
                    names,
                    policy(),
                )
                .unwrap()),
            [30, 4010]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_property_seeds_follow_staged_rekeys_creations_and_numeric_equality() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GraphAggregate, PreparedGraphSetAggregate, PreparedGraphSetText};

    let ((), report) = run_async_under_lab(0x10a1_2a80, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txn_cx) = (contexts.query(), contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=400_i64 {
            batch.create_vertex(
                VId(id as u128),
                vec![],
                vec![(SCORE, CanonicalScalar::Int(id))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(5), SCORE, Some(CanonicalScalar::Int(600)));
        staged.set_vertex_property(VId(11), SCORE, Some(CanonicalScalar::Int(5)));
        staged.delete_vertex(VId(300));
        staged.create_vertex(VId(777), vec![], vec![(SCORE, CanonicalScalar::Int(900))]);
        txn.write(&mut db, staged).unwrap();
        let params = GqlParameters::new()
            .with_list(
                "keys",
                [
                    CanonicalScalar::Int(5),
                    CanonicalScalar::Float(fgdb_types::CanonicalF64::new(7.0)),
                    CanonicalScalar::Int(5),
                    CanonicalScalar::Null,
                    CanonicalScalar::Int(900),
                    CanonicalScalar::Int(300),
                ]
                .into_iter()
                .map(GraphValue::Scalar)
                .collect(),
            )
            .unwrap();
        let query = PreparedGraphSetText::prepare(
            "UNWIND $keys AS key WITH key AS k MATCH (n {score: k}) \
             RETURN n.score AS score ORDER BY score",
            symbols,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let tight = GqlQueryPolicy::new(20, 1000, 1_000_000, 1_000_000);
        let result = txn
            .execute_graph_set_governed(&db, &cx, &query, tight)
            .unwrap();
        let actual: Vec<_> = result
            .value
            .iter()
            .map(|row| row.get(0).unwrap().as_scalar().unwrap().clone())
            .collect();
        assert_eq!(
            actual,
            [5, 5, 7, 900].map(CanonicalScalar::Int),
            "the stale key, deleted vertex and NULL must not join"
        );
        assert!(result.rows.snapshot_records < 20);
        let aggregate = PreparedGraphSetAggregate::prepare(
            query,
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        let result = txn
            .execute_graph_set_aggregate_governed(&db, &cx, &aggregate, tight)
            .unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(4));
        assert!(result.rows.snapshot_records < 20);
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_property_seed_witnesses_catch_empty_phantoms_and_refused_reads() {
    use fgdb_gql::PreparedGraphSetText;
    use fgdb_gql::algebra::GraphValue;

    let ((), report) = run_async_under_lab(0x10a1_2a81, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txn_cx) = (contexts.query(), contexts.commit(), contexts.txn());
        for case in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=40_i64 {
                batch.create_vertex(
                    VId(id as u128),
                    vec![],
                    vec![(SCORE, CanonicalScalar::Int(id))],
                );
            }
            db.write(&commit, batch).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, staged).unwrap();
            let refused = case == 4;
            let query = PreparedGraphSetText::prepare(
                "UNWIND $keys AS key WITH key AS k MATCH (n {score: k}) RETURN n.score AS score",
                symbols,
            )
            .unwrap()
            .bind_parameters(
                &GqlParameters::new()
                    .with_list(
                        "keys",
                        vec![GraphValue::Scalar(CanonicalScalar::Int(if refused {
                            5
                        } else {
                            500
                        }))],
                    )
                    .unwrap(),
            )
            .unwrap();
            let cap = GqlQueryPolicy::new(if refused { 0 } else { 10 }, 100, 1_000_000, 1_000_000);
            let result = txn.execute_graph_set_governed(&db, &cx, &query, cap);
            if refused {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().value.is_empty());
            }
            let mut winner = WriteBatch::new(R);
            match case {
                0 => winner.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(500))),
                1 => winner.set_vertex_property(
                    VId(3),
                    SCORE,
                    Some(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(500.0))),
                ),
                2 => {
                    winner.create_vertex(VId(901), vec![], vec![(SCORE, CanonicalScalar::Int(500))])
                }
                _ => winner.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(499))),
            };
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            // No getter or full scan may repair a missing membership witness.
            let result = txn.commit(&mut db, &commit).await;
            if case == 3 {
                result.unwrap();
                assert!(db.vertex(VId(777)).unwrap().is_some());
            } else {
                assert!(
                    matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ),
                    "case {case}: {result:?}"
                );
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_computed_roots_admit_connected_staged_edge_and_walk_closures() {
    use fgdb_gql::PreparedGraphSetText;
    use fgdb_gql::algebra::GraphValue;

    let ((), report) = run_async_under_lab(0x10a1_2a82, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txn_cx) = (contexts.query(), contexts.commit(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=400_i64 {
            batch.create_vertex(
                VId(id as u128),
                vec![],
                vec![(SCORE, CanonicalScalar::Int(id))],
            );
        }
        for (eid, src, dst) in [(1, 5, 6), (2, 6, 7), (3, 7, 8), (4, 11, 12)] {
            batch.add_edge(
                EId(eid),
                VId(src),
                VId(dst),
                vec![(SCORE, CanonicalScalar::Int(1))],
            );
        }
        for id in 100..300 {
            batch.add_edge(EId(id), VId(id), VId(id + 1), vec![]);
        }
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(9), VId(11), VId(50), vec![]);
        db.write_atomic(&commit, vec![batch, other]).await.unwrap();

        let mut txn = db.begin(&txn_cx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(5), SCORE, Some(CanonicalScalar::Int(600)));
        staged.set_vertex_property(VId(11), SCORE, Some(CanonicalScalar::Int(5)));
        staged.create_vertex(VId(777), vec![], vec![(SCORE, CanonicalScalar::Int(900))]);
        staged.delete_edge(EId(4));
        staged.add_edge(
            EId(5),
            VId(11),
            VId(777),
            vec![(SCORE, CanonicalScalar::Int(1))],
        );
        staged.add_edge(
            EId(6),
            VId(777),
            VId(7),
            vec![(SCORE, CanonicalScalar::Int(1))],
        );
        txn.write(&mut db, staged).unwrap();
        let parameters = GqlParameters::new()
            .with_list(
                "keys",
                [5, 5, 900, 400]
                    .into_iter()
                    .map(|key| GraphValue::Scalar(CanonicalScalar::Int(key)))
                    .chain([GraphValue::Scalar(CanonicalScalar::Null)])
                    .collect(),
            )
            .unwrap();
        let tight = GqlQueryPolicy::new(50, 1000, 1_000_000, 1_000_000);
        for (matching, expected) in [
            (
                "MATCH (a {score:k})-[r:R]->(b)",
                vec![(5, 900), (5, 900), (900, 7)],
            ),
            ("MATCH (a {score:k})<-[r:R]-(b)", vec![(900, 5)]),
            (
                "MATCH (a {score:k})-[r:R]-(b)",
                vec![(5, 900), (5, 900), (900, 5), (900, 7)],
            ),
            (
                "MATCH (a {score:k})-[r]->(b)",
                vec![(5, 50), (5, 50), (5, 900), (5, 900), (900, 7)],
            ),
            (
                "MATCH WALK (a {score:k})-[:R*0..2 {score:1}]->(b)",
                vec![
                    (5, 5),
                    (5, 5),
                    (5, 7),
                    (5, 7),
                    (5, 900),
                    (5, 900),
                    (400, 400),
                    (900, 7),
                    (900, 8),
                    (900, 900),
                ],
            ),
        ] {
            let text = format!(
                "UNWIND $keys AS key WITH key AS k {matching} \
                 RETURN a.score AS source,b.score AS target ORDER BY source,target"
            );
            let prepared = PreparedGraphSetText::prepare(&text, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            let result = txn
                .execute_graph_set_governed(&db, &cx, &prepared, tight)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let actual: Vec<_> = result
                .value
                .iter()
                .map(|row| {
                    let value = |column| match row.get(column).unwrap().as_scalar().unwrap() {
                        CanonicalScalar::Int(value) => *value,
                        value => panic!("expected an integer, got {value:?}"),
                    };
                    (value(0), value(1))
                })
                .collect();
            assert_eq!(actual, expected, "{text}");
            assert!(result.rows.snapshot_records < 50, "{text}");
        }

        // A disconnected component cannot be admitted from slot zero's
        // closure: its isolated winner lies outside every traversed edge.
        let disconnected = PreparedGraphSetText::prepare(
            "UNWIND [5] AS k WITH k MATCH (a {score:k})-[:R]->(b),(z {score:400}) \
             RETURN b.score AS target,z.score AS isolated",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(
            txn.execute_graph_set_governed(&db, &cx, &disconnected, tight)
                .is_err()
        );
        let result = txn
            .execute_graph_set_governed(&db, &cx, &disconnected, policy())
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].get(0).unwrap().as_scalar(),
            Some(&CanonicalScalar::Int(900))
        );
        assert_eq!(
            result.value[0].get(1).unwrap().as_scalar(),
            Some(&CanonicalScalar::Int(400))
        );
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_seeded_traversals_keep_empty_root_and_relation_phantoms() {
    use fgdb_gql::PreparedGraphSetText;

    let ((), report) = run_async_under_lab(0x10a1_2a83, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit, txn_cx) = (contexts.query(), contexts.commit(), contexts.txn());
        for case in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=40_i64 {
                batch.create_vertex(
                    VId(id as u128),
                    vec![],
                    vec![(SCORE, CanonicalScalar::Int(id))],
                );
            }
            batch.add_edge(EId(1), VId(3), VId(4), vec![]);
            db.write(&commit, batch).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, staged).unwrap();
            let key = match case {
                1 | 4 => 5,
                3 => 3,
                _ => 500,
            };
            let relation = if case == 4 { "" } else { ":R" };
            let text = format!(
                "UNWIND [{key}] AS k WITH k MATCH (a {{score:k}})-[r{relation}]->(b) \
                 RETURN b.score AS score"
            );
            let prepared = PreparedGraphSetText::prepare(&text, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let refused = case == 3;
            let cap = GqlQueryPolicy::new(if refused { 0 } else { 10 }, 100, 1_000_000, 1_000_000);
            let result = txn.execute_graph_set_governed(&db, &cx, &prepared, cap);
            if refused {
                assert!(result.is_err(), "{text}");
            } else {
                assert!(result.unwrap().value.is_empty(), "{text}");
            }
            let mut winner = WriteBatch::new(if case == 4 { S } else { R });
            match case {
                0 => winner.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(500))),
                1 | 4 => winner.add_edge(EId(2), VId(5), VId(6), vec![]),
                3 => winner.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(499))),
                _ => winner.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(499))),
            };
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit(&mut db, &commit).await;
            if case == 2 {
                result.unwrap();
                assert!(db.vertex(VId(777)).unwrap().is_some());
            } else {
                assert!(
                    matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ),
                    "case {case}: {result:?}"
                );
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
