//! Public write dispatch keeps FOREACH binding, scope and atomicity intact.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "F") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000),
        100,
        100,
        100,
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn rows(result: QueryResult, expected_columns: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("explicit FOREACH RETURN must use the public row result");
    };
    assert_eq!(columns, expected_columns);
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    other => panic!("expected a native value, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn parameterized_loop_restores_shadowed_outer_name_and_publishes_once() {
    let ((), report) = run_async_under_lab(0xf0ec_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut next = 100_u128;
        let statement = "WITH 99 AS x FOREACH (x IN $xs | CREATE (:F {p:x})) \
                         RETURN x AS outer_value";
        for members in [vec![1, 2, 3], vec![], vec![4]] {
            let basis = db.frontier().unwrap();
            let before = db.vertices().unwrap().len();
            let arguments = GqlParameters::new()
                .with_list("xs", members.iter().copied().map(int).collect())
                .unwrap();
            let original = arguments.canonical_bytes();
            let result = db
                .query_write(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    statement,
                    &arguments,
                    symbols,
                    R,
                    policy(1),
                    |_| {
                        next += 1;
                        Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                    },
                )
                .await
                .unwrap();
            assert_eq!(rows(result, &["outer_value"]), vec![vec![int(99)]]);
            assert_eq!(arguments.canonical_bytes(), original);
            assert_eq!(db.vertices().unwrap().len(), before + members.len());
            let commits = u64::from(!members.is_empty());
            assert_eq!(db.frontier().unwrap().0, basis.0 + commits);
            assert_eq!(db.delta_since(basis).unwrap().count(), commits as usize);
        }
        let values: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect();
        assert_eq!(
            values,
            (1..=4)
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_transaction_refreshes_elements_but_preserves_evaluated_scalar_aliases() {
    let ((), report) = run_async_under_lab(0xf0ec_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let result = transaction
            .query_write(
                &mut db,
                &contexts.query(),
                "CREATE (n:F {p:0}) WITH n, n.p AS frozen \
             FOREACH (x IN [1,2,3] | SET n.p=n.p+x) \
             RETURN n.p AS current, frozen",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                |_| Ok::<_, Infallible>(ElementId::Vertex(VId(100))),
            )
            .unwrap();
        assert_eq!(
            rows(result, &["current", "frozen"]),
            vec![vec![int(6), int(0)]]
        );
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(
            transaction.vertex(&db, VId(100)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(6).into())]
        );
        transaction
            .finish(&mut db, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(6).into())]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_is_evaluated_against_the_overlay_when_each_outer_row_is_reached() {
    let ((), report) = run_async_under_lab(0xf0ec_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "CREATE (n:F {p:0}) WITH n UNWIND [1,2] AS repeat \
             FOREACH (x IN range(1,n.p+1) | SET n.p=n.p+1) RETURN n.p AS value",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2),
                |_| Ok::<_, Infallible>(ElementId::Vertex(VId(100))),
            )
            .await
            .unwrap();
        // The first outer row executes once, taking p from 0 to 1. Only then
        // does the next row evaluate range(1, p+1), and execute twice.
        assert_eq!(rows(result, &["value"]), vec![vec![int(3)], vec![int(3)]]);
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(3).into())]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_match_reads_all_loop_effects_and_a_late_read_error_rolls_them_back() {
    let ((), report) = run_async_under_lab(0xf0ec_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let parameters = GqlParameters::new()
            .with_list("xs", vec![int(3), int(1), int(2)])
            .unwrap();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "FOREACH (x IN $xs | CREATE (:F {p:x})) MATCH (n:F) RETURN n.p AS p ORDER BY p",
                &parameters,
                symbols,
                R,
                policy(3),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["p"]),
            vec![vec![int(1)], vec![int(2)], vec![int(3)]]
        );
        let before = db.frontier().unwrap();
        let vertices = db.vertices().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        transaction
            .query_write(
                &mut db,
                &contexts.query(),
                "FOREACH (x IN [4,5] | CREATE (:F {p:x})) \
             MATCH (n:F) RETURN 1/(n.p-5) AS failing LIMIT 0",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .expect_err("a terminal read must not hide expression errors or accept loop writes");
        assert_eq!(transaction.vertices(&db).unwrap(), vertices);
        transaction
            .finish(&mut db, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_loop_local_map_shadows_an_outer_element_without_hoisting_its_fields() {
    let ((), report) = run_async_under_lab(0xf0ec_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "CREATE (n:F {p:100}) FOREACH (n IN [{local:7}] | CREATE (:F {p:n.local})) \
             RETURN n.p AS outer_value",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        // `local` is deliberately absent from the graph symbol resolver. It
        // names a map key in the loop's private scope, never a field of the
        // outer element nor a hidden property column of its CREATE.
        assert_eq!(rows(result, &["outer_value"]), vec![vec![int(100)]]);
        assert_eq!(
            db.vertices()
                .unwrap()
                .into_iter()
                .map(|row| row.props)
                .collect::<Vec<_>>(),
            vec![
                vec![(P, CanonicalScalar::Int(100).into())],
                vec![(P, CanonicalScalar::Int(7).into())]
            ]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn body_create_fields_read_the_current_staged_element_before_each_allocation() {
    let ((), report) = run_async_under_lab(0xf0ec_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "CREATE (n:F {p:0}) FOREACH (x IN [1,2,3] | \
             CREATE (:F {p:n.p}) SET n.p=n.p+x) RETURN n.p AS value",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(rows(result, &["value"]), vec![vec![int(6)]]);
        assert_eq!(
            db.vertices()
                .unwrap()
                .into_iter()
                .map(|row| row.props)
                .collect::<Vec<_>>(),
            [6, 0, 1, 3]
                .into_iter()
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameterized_merge_upserts_publish_once_and_survive_reopen() {
    let ((), report) = run_async_under_lab(0xf0ec_1010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let pinned = db.read_session().unwrap();
        let basis = db.frontier().unwrap();
        let parameters = GqlParameters::new()
            .with_list(
                "rows",
                [(1, 2), (2, 4), (1, 5)]
                    .into_iter()
                    .map(|(key, delta)| {
                        GraphValue::map(vec![
                            ("key".into(), int(key)),
                            ("delta".into(), int(delta)),
                        ])
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        let original = parameters.canonical_bytes();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "FOREACH (row IN $rows | MERGE (n:F {p:row.key}) \
                 ON CREATE SET n.q=0 SET n.q=n.q+row.delta) \
                 MATCH (n:F) RETURN n.p AS key, n.q AS total ORDER BY key",
                &parameters,
                symbols,
                R,
                policy(2),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["key", "total"]),
            vec![vec![int(1), int(7)], vec![int(2), int(4)]]
        );
        assert_eq!(next, 102, "a repeated map key must reuse the staged vertex");
        assert_eq!(parameters.canonical_bytes(), original);
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        let expected = db.vertices().unwrap();
        assert_eq!(expected.len(), 2);
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_parameterized_graph_import_publishes_once_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0xf0ec_1020, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let parameters = GqlParameters::new()
            .with_list(
                "groups",
                [
                    (1, vec![vec![10, 20], vec![]]),
                    (2, vec![vec![]]),
                    (1, vec![vec![20], vec![30]]),
                ]
                .into_iter()
                .map(|(key, batches)| {
                    GraphValue::map(vec![
                        ("key".into(), int(key)),
                        (
                            "batches".into(),
                            GraphValue::List(
                                batches
                                    .into_iter()
                                    .map(|children| {
                                        GraphValue::List(children.into_iter().map(int).collect())
                                    })
                                    .collect(),
                            ),
                        ),
                    ])
                    .unwrap()
                })
                .collect(),
            )
            .unwrap();
        let original = parameters.canonical_bytes();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "FOREACH (group IN $groups | MERGE (parent:F {p:group.key}) \
             ON CREATE SET parent.q=0 \
             FOREACH (batch IN group.batches | FOREACH (child_key IN batch | \
             MERGE (child:F {p:child_key}) MERGE (parent)-[edge:R]->(child) \
             ON CREATE SET edge.p=1 ON MATCH SET edge.p=edge.p+1 \
             SET parent.q=parent.q+1))) \
             MATCH (n:F) WHERE n.p<10 RETURN n.p AS key, n.q AS total ORDER BY key",
                &parameters,
                symbols,
                R,
                policy(2),
                |request| {
                    next += 1;
                    Ok::<_, Infallible>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["key", "total"]),
            vec![vec![int(1), int(4)], vec![int(2), int(0)]]
        );
        assert_eq!(
            next, 108,
            "repeated child keys reuse both vertex and relationship identities"
        );
        assert_eq!(parameters.canonical_bytes(), original);
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        assert!(pinned.edges().unwrap().is_empty());
        let expected_vertices = db.vertices().unwrap();
        let expected_edges = db.edges().unwrap();
        assert_eq!(expected_vertices.len(), 5);
        assert_eq!(expected_edges.len(), 3);
        let mut children = Vec::new();
        for edge in &expected_edges {
            assert_eq!(edge.entry.relation, R);
            assert_eq!(
                db.vertex(edge.entry.src).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, CanonicalScalar::Int(4).into()),
                ]
            );
            children.push((
                db.vertex(edge.entry.dst).unwrap().unwrap().props,
                edge.props.clone(),
            ));
        }
        children.sort();
        assert_eq!(
            children,
            [(10, 1), (20, 2), (30, 1)]
                .into_iter()
                .map(|(key, count)| (
                    vec![(P, CanonicalScalar::Int(key).into())],
                    vec![(P, CanonicalScalar::Int(count).into())],
                ))
                .collect::<Vec<_>>()
        );
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected_vertices);
        assert_eq!(reopened.edges().unwrap(), expected_edges);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
