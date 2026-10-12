//! Native COLLECT over computed graph inputs retains the original complete-row
//! occurrence order while the existing circuit maintains insertions/retractions.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphSetProjection, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const R: RelationId = RelationId(1);
const ROOT: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x63; 32],
        DatabaseSecurityNamespaceId([0x64; 32]),
        [0x65; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value, root) in [
        (1, Some(30), true),
        (2, Some(10), true),
        (3, Some(20), false),
        (4, Some(10), true),
        (5, None, false),
        (6, Some(40), true),
    ] {
        batch.create_vertex(
            VId(id),
            if root { vec![ROOT] } else { vec![] },
            vec![(P, value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))],
        );
    }
    // Identity, endpoint and value orders intentionally differ. Equal projected
    // rows still represent separate real relationships.
    for (eid, src, dst) in [(40, 1, 2), (10, 1, 3), (20, 1, 2), (30, 4, 5), (50, 2, 3)] {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn register(db: &mut Database<MemVfs>, cx: &QueryCx, text: &str) -> StandingQueryHandle {
    db.register_standing_native(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap_or_else(|error| panic!("registration failed for {text}: {error:?}"))
}
fn differential(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    handle: &StandingQueryHandle,
) -> QueryResult {
    let expected = db
        .query(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap();
    let (at, actual) = db.standing_native_query(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(actual, expected, "{text}");
    actual
}
fn list(values: &[i64]) -> QueryValue {
    QueryValue::Value(GraphValue::List(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
            .collect(),
    ))
}

fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap()
}

#[test]
fn edge_collections_preserve_occurrence_order_parallel_multiplicity_and_distinct() {
    let ((), report) = run_async_under_lab(0x636f_6c31, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = fixture(&commit).await;
        let text = "MATCH (a)-[:R]->(b) RETURN collect(-b.p) AS bag, \
                    collect(DISTINCT -b.p) AS unique, count(*) AS copies";
        let handle = register(&mut db, &cx, text);
        assert_eq!(
            differential(&db, &cx, text, &handle),
            QueryResult::Rows {
                columns: vec!["bag".into(), "unique".into(), "copies".into()],
                // The computed argument reverses value order. Sorting arguments
                // instead of the original input would put -20 before -10.
                rows: vec![vec![
                    list(&[-10, -10, -20, -20]),
                    list(&[-10, -20]),
                    QueryValue::Count(5),
                ]],
            }
        );
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(50)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            differential(&db, &cx, text, &handle),
            QueryResult::Rows {
                columns: vec!["bag".into(), "unique".into(), "copies".into()],
                rows: vec![vec![
                    list(&[-20, -20, -50, -50]),
                    list(&[-20, -50]),
                    QueryValue::Count(5),
                ]],
            }
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn joined_optional_and_existence_collections_track_native_results_across_commits() {
    let ((), report) = run_async_under_lab(0x636f_6c32, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = fixture(&commit).await;
        let texts = [
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN collect(-c.p) AS bag, count(*) AS copies",
            "MATCH (a:Root) OPTIONAL MATCH (a)-[:R]->(b) \
             RETURN collect(-b.p) AS bag, collect(DISTINCT -a.p) AS roots",
            "MATCH (a:Root) WHERE EXISTS { MATCH (a)-[:R]->(b) } RETURN collect(-a.p) AS bag",
            "MATCH (a:Root) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN collect(-a.p) AS bag",
            "MATCH (a)-[:R]->(b) RETURN a.p AS key, collect(-b.p) AS bag, count(*) AS copies \
             GROUP BY a.p ORDER BY a.p DESC SKIP 1 LIMIT 2",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| register(&mut db, &cx, text))
            .collect();
        let mut integrated: Vec<_> = handles
            .iter()
            .map(|handle| copy(db.standing_query(&cx, handle).unwrap().rows()))
            .collect();
        for tick in 0..6 {
            for (text, handle) in texts.iter().zip(&handles) {
                differential(&db, &cx, text, handle);
            }
            let mut change = WriteBatch::new(R);
            match tick {
                0 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(50)));
                    change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(10)));
                    change.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(0)));
                }
                1 => {
                    change.delete_edge(EId(20));
                    change.add_edge(EId(90), VId(6), VId(5), vec![]);
                    change.set_vertex_property(VId(1), P, None);
                }
                2 => {
                    change.delete_vertex(VId(3));
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Null));
                }
                3 => {
                    change.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(-7))]);
                    change.add_edge(EId(77), VId(4), VId(7), vec![]);
                }
                4 => {
                    for id in [1, 2, 4, 5, 6, 7] {
                        change.delete_vertex(VId(id));
                    }
                }
                _ => break,
            }
            let at = db.write(&commit, change).await.unwrap();
            for (handle, rows) in handles.iter().zip(&mut integrated) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                rows.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*rows, db.standing_query(&cx, handle).unwrap().rows());
            }
        }
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            differential(&db, &cx, text, handle);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_zero_does_not_hide_a_later_collection_input_exception() {
    let ((), report) = run_async_under_lab(0x636f_6c33, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = fixture(&commit).await;
        let text = "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN collect(10 / c.p) AS bag LIMIT 0";
        let handle = register(&mut db, &cx, text);
        assert!(matches!(
            differential(&db, &cx, text, &handle),
            QueryResult::Rows { rows, .. } if rows.is_empty()
        ));
        let healthy_text = "MATCH (a)-[:R]->(b) RETURN collect(-b.p) AS bag";
        let healthy = register(&mut db, &cx, healthy_text);
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(0)));
        // A maintained-query failure fences that view, not a durable write or
        // another independently registered query.
        db.write(&commit, change).await.unwrap();
        assert!(
            db.query(&cx, text, &GqlParameters::new(), symbols, policy())
                .is_err()
        );
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        differential(&db, &cx, healthy_text, &healthy);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(5)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(db.standing_group_delta(&cx, &handle).unwrap().is_none());
        differential(&db, &cx, text, &handle);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_setup_does_not_damage_an_existing_collection_and_can_retry() {
    let ((), report) = run_async_under_lab(0x636f_6c34, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = fixture(&commit).await;
        let text = "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN collect(-b.p) AS bag";
        let original = register(&mut db, &cx, text);
        let expected = differential(&db, &cx, text, &original);
        for denied in [
            GqlQueryPolicy::new(100_000, 100_000, 0, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 0),
            GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000),
        ] {
            assert!(
                db.register_standing_native(&cx, text, &GqlParameters::new(), symbols, denied)
                    .is_err()
            );
            assert_eq!(differential(&db, &cx, text, &original), expected);
        }
        let retried = register(&mut db, &cx, text);
        assert_eq!(differential(&db, &cx, text, &retried), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn normalization_does_not_invent_an_unprojected_join_occurrence_order() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap();
    builder.vertex("b").unwrap();
    builder.vertex("c").unwrap();
    builder
        .edge("a", R, fgdb_gql::algebra::GlaDirection::Forward, "b")
        .unwrap();
    builder
        .edge("b", R, fgdb_gql::algebra::GlaDirection::Forward, "c")
        .unwrap();
    let input = builder
        .prepare_values(
            &[GraphColumn::property("_fgdb_collect_order_0", "c", P)],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let aggregates = [GraphAggregate::collect("bag", 0)];
    let raw = PreparedGraphAggregate::prepare(input.clone(), &[], &aggregates, 0, None).unwrap();
    assert!(raw.incremental_collection_relation().is_none());
    let projected = PreparedGraphAggregate::prepare_projected(
        input,
        vec![GraphSetProjection::new(
            "_fgdb_collect_order_1",
            GraphSetValue::Column(0),
        )],
        &[],
        &aggregates,
        0,
        None,
    )
    .unwrap();
    assert!(projected.incremental_collection_relation().is_some());
}
