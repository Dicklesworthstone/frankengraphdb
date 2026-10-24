//! Public typed registration must not have weaker numeric/collection semantics
//! than snapshot execution. Private access below checks atomic circuit ownership,
//! not an alternate registration, accumulator, or publication implementation.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
use fgdb_gql::{
    GqlParameters, GraphAggregate, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts,
};

const P: PropertyKeyId = PropertyKeyId(1);
const G: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}
fn float_result(value: f64) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::Scalar(float(value)))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(G)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn numeric() -> PreparedGraphAggregate {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input = source
        .prepare_values(&[GraphColumn::property("value", "n", P)], 0, None)
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count_distinct("types", 0),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::sum_int_distinct("distinct_sum", 0),
            GraphAggregate::average_int("avg", 0),
            GraphAggregate::average_int_distinct("distinct_avg", 0),
        ],
        0,
        None,
    )
    .unwrap()
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    Database::open_memory(
        cx,
        DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        ),
    )
    .await
    .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, values: &[CanonicalScalar]) {
    let mut batch = WriteBatch::new(R);
    for (index, value) in values.iter().enumerate() {
        batch.create_vertex(
            VId(index as u128 + 1),
            vec![L],
            vec![
                (P, value.clone()),
                (G, CanonicalScalar::Int((index % 2) as i64)),
            ],
        );
    }
    db.write(cx, batch).await.unwrap();
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    definition: &PreparedGraphAggregate,
    handle: &StandingQueryHandle,
) -> Vec<GraphAggregateRow> {
    let expected = db
        .execute_graph_aggregate_governed(cx, definition, policy())
        .unwrap()
        .value;
    let expected_bag = ZSet::from_updates(
        expected.iter().cloned().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let current = db.standing_query(cx, handle).unwrap();
    assert_eq!(current.frontier(), db.frontier().unwrap());
    assert_eq!(current.rows(), &expected_bag);
    if let Some(ordered) = current.ordered_rows() {
        assert_eq!(ordered.cloned().collect::<Vec<_>>(), expected);
    }
    expected
}
fn integrate(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    previous: &mut ZSet<GraphAggregateRow>,
) {
    let delta = db.standing_query_delta(cx, handle).unwrap().unwrap();
    assert_eq!(delta.frontier(), db.frontier().unwrap());
    previous
        .integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(&*previous, db.standing_query(cx, handle).unwrap().rows());
}

#[test]
fn typed_numeric_registration_retracts_mixed_support_back_to_exact_integer_results() {
    let ((), report) = run_async_under_lab(0x7479_6701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        seed(
            &mut db,
            &commit,
            &[
                CanonicalScalar::Int(1),
                float(1.0),
                float(2.5),
                float(1.0),
                CanonicalScalar::Null,
            ],
        )
        .await;
        let definition = numeric();
        let original = definition.canonical_bytes();
        let handle = db
            .register_standing_query(&cx, definition.clone(), policy())
            .unwrap();
        assert!(matches!(
            &db.standing_queries[handle.index],
            StandingQuery::Group(_)
        ));
        assert!(matches!(
            handle.native.as_deref(),
            Some(native::Layout::GroupCircuit { .. })
        ));
        assert!(db.standing_query_delta(&cx, &handle).unwrap().is_none());
        let rows = check(&db, &cx, &definition, &handle);
        assert_eq!(rows[0].get(0).unwrap().as_count(), Some(5));
        // Int(1) and Float(1) are distinct support, not numerically deduplicated.
        assert_eq!(rows[0].get(1).unwrap().as_count(), Some(3));
        assert_eq!(rows[0].get(2), Some(&float_result(5.5)));
        assert_eq!(rows[0].get(3), Some(&float_result(4.5)));
        assert_eq!(rows[0].get(4), Some(&float_result(1.375)));
        assert_eq!(rows[0].get(5), Some(&float_result(1.5)));
        let mut previous = copy(db.standing_query(&cx, &handle).unwrap().rows());
        for (index, id) in [4, 3, 2, 1, 5].into_iter().enumerate() {
            let mut batch = WriteBatch::new(R);
            batch.delete_vertex(VId(id));
            db.write(&commit, batch).await.unwrap();
            integrate(&db, &cx, &handle, &mut previous);
            let rows = check(&db, &cx, &definition, &handle);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get(0).unwrap().as_count(), Some(4 - index as u64));
            if index == 2 {
                assert_eq!(rows[0].get(2).unwrap().as_integer(), Some(1));
                let average = rows[0].get(4).unwrap().as_average().unwrap();
                assert_eq!((average.numerator(), average.denominator()), (1, 1));
            } else if index >= 3 {
                for column in 2..6 {
                    assert!(rows[0].get(column).unwrap().is_null());
                }
            }
        }
        assert_eq!(definition.canonical_bytes(), original);
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(db.standing_query_delta(&cx, &handle).unwrap().is_none());
        check(&db, &cx, &definition, &handle);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_numeric_registration_keeps_exact_cancellation_and_retractable_nonfinite_values() {
    let ((), report) = run_async_under_lab(0x7479_6702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        seed(
            &mut db,
            &commit,
            &[float(1e300), CanonicalScalar::Int(7), float(-1e300)],
        )
        .await;
        let definition = numeric();
        let handle = db
            .register_standing_query(&cx, definition.clone(), policy())
            .unwrap();
        assert_eq!(
            check(&db, &cx, &definition, &handle)[0].get(2),
            Some(&float_result(7.0))
        );
        let mut previous = copy(db.standing_query(&cx, &handle).unwrap().rows());
        for (index, value) in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN]
            .into_iter()
            .enumerate()
        {
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(index as u128 + 10), vec![], vec![(P, float(value))]);
            db.write(&commit, batch).await.unwrap();
            integrate(&db, &cx, &handle, &mut previous);
            let expected = if index == 0 { f64::INFINITY } else { f64::NAN };
            assert_eq!(
                check(&db, &cx, &definition, &handle)[0].get(2),
                Some(&float_result(expected))
            );
        }
        for (id, expected) in [(12, f64::NAN), (10, f64::NEG_INFINITY), (11, 7.0)] {
            let mut batch = WriteBatch::new(R);
            batch.delete_vertex(VId(id));
            db.write(&commit, batch).await.unwrap();
            integrate(&db, &cx, &handle, &mut previous);
            assert_eq!(
                check(&db, &cx, &definition, &handle)[0].get(2),
                Some(&float_result(expected))
            );
        }
        let mut batch = WriteBatch::new(R);
        batch.delete_vertex(VId(1));
        batch.delete_vertex(VId(3));
        db.write(&commit, batch).await.unwrap();
        integrate(&db, &cx, &handle, &mut previous);
        assert_eq!(
            check(&db, &cx, &definition, &handle)[0]
                .get(2)
                .unwrap()
                .as_integer(),
            Some(7)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_scoped_and_computed_aggregates_keep_complete_graph_sources_and_output_pages() {
    let ((), report) = run_async_under_lab(0x7479_6703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        seed(
            &mut db,
            &commit,
            &[
                CanonicalScalar::Int(1),
                float(2.5),
                CanonicalScalar::Int(3),
                float(4.5),
                CanonicalScalar::Null,
            ],
        )
        .await;
        let mut edges = WriteBatch::new(R);
        for (id, src, dst) in [(10, 1, 2), (11, 1, 2), (20, 2, 3), (30, 4, 3)] {
            edges.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, edges).await.unwrap();
        let definitions: Vec<_> = [
            "MATCH (a)-[:R]->(b) RETURN SUM(b.p) AS total,AVG(b.p) AS mean,COUNT(*) AS copies",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN SUM(DISTINCT c.p) AS total,AVG(c.p) AS mean",
            "MATCH (a:L) OPTIONAL MATCH (a)-[:R]->(b) RETURN a.g AS bucket,SUM(b.p) AS total,COUNT(*) AS copies GROUP BY a.g ORDER BY a.g",
            "MATCH (a:L) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN SUM(a.p) AS total,AVG(a.p) AS mean",
            "MATCH (n:L) RETURN n.g AS bucket,SUM(n.p*2.0) AS total,AVG(n.p) AS mean GROUP BY n.g HAVING COUNT(*)>0 ORDER BY total DESC LIMIT 1",
        ].into_iter().map(prepare).collect();
        let handles: Vec<_> = definitions
            .iter()
            .map(|definition| {
                assert!(definition.input_relation().is_none());
                db.register_standing_query(&cx, definition.clone(), policy())
                    .unwrap()
            })
            .collect();
        let mut previous: Vec<_> = handles
            .iter()
            .map(|handle| copy(db.standing_query(&cx, handle).unwrap().rows()))
            .collect();
        for tick in 0..4 {
            for (definition, handle) in definitions.iter().zip(&handles) {
                check(&db, &cx, definition, handle);
            }
            let mut batch = WriteBatch::new(R);
            match tick {
                0 => {
                    batch.set_vertex_property(VId(3), P, Some(float(0.5)));
                    batch.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(9)));
                }
                1 => {
                    batch.delete_edge(EId(10));
                    batch.add_edge(EId(50), VId(5), VId(3), vec![]);
                }
                2 => {
                    batch.delete_vertex(VId(2));
                }
                _ => {
                    batch.set_vertex_label(VId(4), L, false);
                    batch.set_vertex_property(VId(1), G, Some(CanonicalScalar::Int(5)));
                }
            }
            db.write(&commit, batch).await.unwrap();
            for ((definition, handle), previous) in
                definitions.iter().zip(&handles).zip(&mut previous)
            {
                integrate(&db, &cx, handle, previous);
                check(&db, &cx, definition, handle);
            }
        }
        for (definition, handle) in definitions.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_query_delta(&cx, handle).unwrap().is_none());
            check(&db, &cx, definition, handle);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_collections_keep_occurrence_order_and_null_duplicate_retractions() {
    let ((), report) = run_async_under_lab(0x7479_6704, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        seed(
            &mut db,
            &commit,
            &[
                CanonicalScalar::Int(30),
                CanonicalScalar::Int(10),
                CanonicalScalar::Int(20),
                CanonicalScalar::Int(10),
                CanonicalScalar::Null,
            ],
        )
        .await;
        let definitions = [
            prepare(
                "MATCH (n) RETURN collect(n.p) AS bag,collect(DISTINCT n.p) AS distinct_values",
            ),
            prepare(
                "MATCH (n) RETURN collect(-n.p) AS bag,collect(DISTINCT -n.p) AS distinct_values",
            ),
        ];
        let handles: Vec<_> = definitions
            .iter()
            .map(|definition| {
                db.register_standing_query(&cx, definition.clone(), policy())
                    .unwrap()
            })
            .collect();
        let list = |values: &[i64]| {
            GraphAggregateValue::Value(GraphValue::List(
                values
                    .iter()
                    .map(|v| GraphValue::Scalar(CanonicalScalar::Int(*v)))
                    .collect(),
            ))
        };
        let raw = check(&db, &cx, &definitions[0], &handles[0]);
        assert_eq!(raw[0].get(0), Some(&list(&[30, 10, 20, 10])));
        assert_eq!(raw[0].get(1), Some(&list(&[30, 10, 20])));
        let computed = check(&db, &cx, &definitions[1], &handles[1]);
        assert_eq!(computed[0].get(0), Some(&list(&[-10, -10, -20, -30])));
        let mut previous: Vec<_> = handles
            .iter()
            .map(|handle| copy(db.standing_query(&cx, handle).unwrap().rows()))
            .collect();
        for id in [2, 4, 1, 3, 5] {
            let mut batch = WriteBatch::new(R);
            batch.delete_vertex(VId(id));
            db.write(&commit, batch).await.unwrap();
            for ((definition, handle), bag) in definitions.iter().zip(&handles).zip(&mut previous) {
                integrate(&db, &cx, handle, bag);
                check(&db, &cx, definition, handle);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_group_setup_and_failed_private_source_rebuild_are_atomic() {
    let ((), report) = run_async_under_lab(0x7479_6705, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        seed(
            &mut db,
            &commit,
            &[CanonicalScalar::Int(2), CanonicalScalar::Int(5)],
        )
        .await;
        let count = prepare("MATCH (n) RETURN COUNT(*) AS copies");
        let healthy = db
            .register_standing_query(&cx, count.clone(), policy())
            .unwrap();
        assert!(matches!(
            &db.standing_queries[healthy.index],
            StandingQuery::Aggregate(_)
        ));
        assert!(healthy.native.is_none());
        let definition = prepare("MATCH (n) RETURN SUM(10/n.p) AS total LIMIT 0");
        let before = db.standing_queries.len();
        for allowance in [
            GqlQueryPolicy::new(100_000, 100_000, 0, 20_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 0),
            GqlQueryPolicy::new(0, 100_000, 20_000_000, 20_000_000),
        ] {
            assert!(
                db.register_standing_query(&cx, definition.clone(), allowance)
                    .is_err()
            );
            assert_eq!(db.standing_queries.len(), before);
            check(&db, &cx, &count, &healthy);
        }
        let handle = db
            .register_standing_query(&cx, definition.clone(), policy())
            .unwrap();
        let size = db.standing_queries.len();
        assert!(
            size > before + 1,
            "source and projection must belong to the returned circuit"
        );
        assert!(check(&db, &cx, &definition, &handle).is_empty());
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(0)));
        db.write(&commit, batch).await.unwrap();
        assert!(
            db.execute_graph_aggregate_governed(&cx, &definition, policy())
                .is_err()
        );
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unavailable { .. })
        ));
        assert!(db.standing_query_delta(&cx, &handle).is_err());
        check(&db, &cx, &count, &healthy);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(db.standing_queries.len(), size);
        assert!(
            db.register_standing_query(&cx, definition.clone(), policy())
                .is_err()
        );
        assert_eq!(db.standing_queries.len(), size);
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_query(&cx, &handle).is_err());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), size);
        assert!(db.standing_query_delta(&cx, &handle).unwrap().is_none());
        check(&db, &cx, &definition, &handle);
        let other = database(&commit).await;
        assert!(matches!(
            other.standing_query(&cx, &handle),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_numeric_selection_precedes_data_and_recovers_only_through_explicit_rebuild() {
    let ((), report) = run_async_under_lab(0x7479_6706, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let mut db = database(&commit).await;
        let definition = numeric();
        let handle = db
            .register_standing_query(&cx, definition.clone(), policy())
            .unwrap();
        check(&db, &cx, &definition, &handle);
        seed(
            &mut db,
            &commit,
            &[CanonicalScalar::Int(3), CanonicalScalar::Int(1)],
        )
        .await;
        assert_eq!(
            check(&db, &cx, &definition, &handle)[0]
                .get(2)
                .unwrap()
                .as_integer(),
            Some(4)
        );
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(1), P, Some(float(0.5)));
        db.write(&commit, batch).await.unwrap();
        assert_eq!(
            check(&db, &cx, &definition, &handle)[0].get(2),
            Some(&float_result(1.5))
        );
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(VId(1), P, Some(CanonicalScalar::Bool(true)));
        let at = db.write(&commit, invalid).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(
            db.execute_graph_aggregate_governed(&cx, &definition, policy())
                .is_err()
        );
        assert!(db.standing_query(&cx, &handle).is_err());
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(3)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_query(&cx, &handle).is_err());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(
            check(&db, &cx, &definition, &handle)[0]
                .get(2)
                .unwrap()
                .as_integer(),
            Some(4)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
