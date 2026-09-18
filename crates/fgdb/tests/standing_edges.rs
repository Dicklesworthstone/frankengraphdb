//! One-hop standing results compared to independent public-storage enumeration.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValue, IntegerComparison, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphExactAverage, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const SOURCE: LabelId = LabelId(1);
const BUCKET: PropertyKeyId = PropertyKeyId(1);
const AMOUNT: PropertyKeyId = PropertyKeyId(2);
const SELECTED: PropertyKeyId = PropertyKeyId(3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000)
}
fn definition(direction: GlaDirection, identity: Option<bool>) -> PreparedGraphAggregate {
    let mut pattern = GraphPatternBuilder::new();
    pattern.vertex("a").unwrap().vertex("b").unwrap();
    pattern.edge("a", R, direction, "b").unwrap();
    pattern.filter("a", VertexPredicate::HasLabel(SOURCE)).unwrap();
    pattern.filter("b", VertexPredicate::IntegerProperty {
        key: SELECTED, comparison: IntegerComparison::Equal, value: 1,
    }).unwrap();
    if let Some(equal) = identity { pattern.identity("a", "b", equal).unwrap(); }
    let input = pattern.prepare_values(&[
        GraphColumn::property("bucket", "a", BUCKET),
        GraphColumn::property("amount", "b", AMOUNT),
    ], 0, None).unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, &[0], &[
        GraphAggregate::count_rows("rows"), GraphAggregate::count("nonnull", 1),
        GraphAggregate::sum_int("sum", 1), GraphAggregate::average_int("average", 1),
    ], 0, None).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (id, labelled, selected, bucket, amount) in [
        (1, true, 1, 10, Some(5)), (2, true, 1, 20, Some(7)),
        (3, false, 1, 30, None), (4, true, 0, 20, Some(11)),
        (5, true, 1, 40, Some(13)),
    ] {
        let mut props = vec![(BUCKET, CanonicalScalar::Int(bucket)), (SELECTED, CanonicalScalar::Int(selected))];
        if let Some(value) = amount { props.push((AMOUNT, CanonicalScalar::Int(value))); }
        batch.create_vertex(VId(id), if labelled { vec![SOURCE] } else { vec![] }, props);
    }
    for (eid, src, dst) in [(101, 1, 2), (102, 1, 2), (103, 2, 1), (104, 2, 2), (105, 2, 3), (106, 4, 2)] {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(OTHER);
    other.add_edge(EId(201), VId(1), VId(5), vec![]);
    db.write(cx, other).await.unwrap();
}

fn assert_state(
    db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle,
    query: &PreparedGraphAggregate, direction: GlaDirection, identity: Option<bool>,
) {
    let vertices = db.vertices().unwrap();
    let by_id: BTreeMap<_, _> = vertices.iter().map(|v| (v.vid, v)).collect();
    let mut expected: BTreeMap<Vec<GraphValue>, Vec<Option<i64>>> = BTreeMap::new();
    for edge in db.edges().unwrap() {
        if edge.entry.relation != R { continue; }
        let (src, dst) = (edge.entry.src, edge.entry.dst);
        let orientations = match direction {
            GlaDirection::Forward => vec![(src, dst)],
            GlaDirection::Reverse => vec![(dst, src)],
            GlaDirection::Undirected if src == dst => vec![(src, dst)],
            GlaDirection::Undirected => vec![(src, dst), (dst, src)],
        };
        for (a, b) in orientations {
            if identity.is_some_and(|equal| (a == b) != equal) { continue; }
            let (a, b) = (by_id[&a], by_id[&b]);
            if !a.labels.contains(&SOURCE) || !b.props.iter().any(|(key, value)|
                *key == SELECTED && matches!(value, CanonicalScalar::Int(1))) { continue; }
            let bucket = a.props.iter().find(|(key, _)| *key == BUCKET)
                .map(|(_, value)| value.clone()).unwrap_or(CanonicalScalar::Null);
            let amount = b.props.iter().find(|(key, _)| *key == AMOUNT)
                .and_then(|(_, value)| match value {
                    CanonicalScalar::Int(value) => Some(*value),
                    CanonicalScalar::Null => None,
                    _ => panic!("integer-only fixture"),
                });
            expected.entry(vec![GraphValue::Scalar(bucket)]).or_default().push(amount);
        }
    }
    let maintained = db.standing_query(cx, handle).unwrap();
    assert_eq!(maintained.frontier(), db.frontier().unwrap());
    assert_eq!(maintained.rows().len(), expected.len());
    for (row, weight) in maintained.rows().iter() {
        assert_eq!(weight, &ZWeight::ONE);
        let rows = &expected[row.keys()];
        let numbers: Vec<_> = rows.iter().flatten().copied().collect();
        assert_eq!(row.get(0).unwrap().as_count(), Some(rows.len() as u64));
        assert_eq!(row.get(1).unwrap().as_count(), Some(numbers.len() as u64));
        if numbers.is_empty() {
            assert!(row.get(2).unwrap().is_null() && row.get(3).unwrap().is_null());
        } else {
            let sum = numbers.iter().map(|v| i128::from(*v)).sum();
            assert_eq!(row.get(2).unwrap().as_integer(), Some(sum));
            assert_eq!(row.get(3).unwrap().as_average(), GraphExactAverage::new(sum, numbers.len() as u64));
        }
    }
    // Also exercise the canonical full GLA executor, independently of the
    // storage-only oracle above and of standing input arrangements.
    let full = db.execute_graph_aggregate_governed(cx, query, policy()).unwrap();
    assert_eq!(full.value.len(), expected.len());
    for row in &full.value { assert_eq!(maintained.rows().weight(row), Some(&ZWeight::ONE)); }
}

#[test]
fn directions_parallel_edges_endpoint_changes_and_cascades_match_storage() {
    let ((), report) = run_async_under_lab(0x7e01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut queries = Vec::new();
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for identity in [None, Some(true), Some(false)] {
                let query = definition(direction, identity);
                let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
                assert_state(&db, &cx, &handle, &query, direction, identity);
                queries.push((query, handle, direction, identity));
            }
        }
        for step in 0..7 {
            let mut batch = WriteBatch::new(R);
            match step {
                0 => { // Both endpoints change in the same tick; shared edges retract once.
                    batch.set_vertex_property(VId(1), BUCKET, Some(CanonicalScalar::Int(25)));
                    batch.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(-3)));
                    batch.set_vertex_label(VId(3), SOURCE, true);
                    batch.add_edge(EId(107), VId(3), VId(1), vec![]);
                }
                1 => { batch.delete_edge(EId(101)); }
                2 => {
                    batch.set_vertex_property(VId(2), SELECTED, Some(CanonicalScalar::Int(0)));
                    batch.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Null));
                }
                3 => {
                    batch.set_vertex_property(VId(2), SELECTED, Some(CanonicalScalar::Int(1)));
                    batch.set_vertex_property(VId(2), AMOUNT, None);
                    batch.set_vertex_property(VId(4), BUCKET, None);
                }
                4 => { // Deletes parallel, incoming, outgoing and self-loop occurrences.
                    batch.delete_vertex(VId(2));
                    batch.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Int(9)));
                }
                5 => { // An explicit delete and two endpoint cascades overlap.
                    batch.delete_edge(EId(107));
                    batch.delete_vertex(VId(1));
                    batch.delete_vertex(VId(3));
                }
                6 => {
                    batch.create_vertex(VId(6), vec![SOURCE], vec![
                        (SELECTED, CanonicalScalar::Int(1)), (AMOUNT, CanonicalScalar::Int(8)),
                    ]);
                    batch.add_edge(EId(108), VId(6), VId(6), vec![]);
                }
                _ => unreachable!(),
            }
            db.write(&commit, batch).await.unwrap();
            for (query, handle, direction, identity) in &queries {
                assert_state(&db, &cx, handle, query, *direction, *identity);
            }
        }
        db.compact(&commit).await.unwrap();
        for (query, handle, direction, identity) in &queries {
            assert_state(&db, &cx, handle, query, *direction, *identity);
        }
        drop(db);
        let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for (query, old, direction, identity) in queries {
            assert!(matches!(reopened.standing_query(&cx, &old), Err(fgdb::StandingQueryError::ForeignHandle)));
            let handle = reopened.register_standing_query(&cx, query.clone(), policy()).unwrap();
            assert_state(&reopened, &cx, &handle, &query, direction, identity);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parser_and_transactions_feed_the_same_committed_one_hop_maintainer() {
    let ((), report) = run_async_under_lab(0x7e02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let text = "MATCH (a:Source)-[:R]->(b) WHERE b.selected=1 \
            RETURN a.bucket,COUNT(*) AS rows,COUNT(b.amount) AS nonnull,\
            SUM(b.amount) AS total,AVG(b.amount) AS average GROUP BY a.bucket";
        let query = PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
            (GraphSymbolKind::Property, "bucket") => Some(GraphSymbol::Property(BUCKET)),
            (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(AMOUNT)),
            (GraphSymbolKind::Property, "selected") => Some(GraphSymbol::Property(SELECTED)),
            _ => None,
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.delete_vertex(VId(2));
        batch.set_vertex_property(VId(3), AMOUNT, Some(CanonicalScalar::Int(17)));
        batch.add_edge(EId(150), VId(1), VId(3), vec![]);
        txn.write(&mut db, batch).unwrap();
        assert_state(&db, &cx, &handle, &query, GlaDirection::Forward, None);
        assert_eq!(db.standing_query(&cx, &handle).unwrap().frontier(), before);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_state(&db, &cx, &handle, &query, GlaDirection::Forward, None);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unchanged_regions_do_not_increase_endpoint_maintenance_work() {
    let ((), report) = run_async_under_lab(0x7e03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut measured = Vec::new();
        for large in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            if large {
                let mut batch = WriteBatch::new(R);
                for id in 1000..1400 {
                    batch.create_vertex(VId(id), vec![SOURCE], vec![
                        (BUCKET, CanonicalScalar::Int(id as i64)),
                        (SELECTED, CanonicalScalar::Int(1)), (AMOUNT, CanonicalScalar::Int(1)),
                    ]);
                    batch.add_edge(EId(id), VId(id), VId(id), vec![]);
                }
                db.write(&commit, batch).await.unwrap();
            }
            let query = definition(GlaDirection::Forward, None);
            let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
            let mut batch = WriteBatch::new(R);
            batch.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(19)));
            db.write(&commit, batch).await.unwrap();
            assert_state(&db, &cx, &handle, &query, GlaDirection::Forward, None);
            let stats = *db.standing_query(&cx, &handle).unwrap().last_maintenance();
            assert_eq!(stats.affected_vertices, 1);
            assert_eq!(stats.affected_edges, 6);
            measured.push(stats);
            let mut irrelevant = WriteBatch::new(OTHER);
            irrelevant.set_vertex_property(VId(2), PropertyKeyId(99), Some(CanonicalScalar::Int(4)));
            irrelevant.set_edge_property(EId(201), PropertyKeyId(88), Some(CanonicalScalar::Int(8)));
            db.write(&commit, irrelevant).await.unwrap();
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.last_maintenance().affected_vertices, 0);
            assert_eq!(view.last_maintenance().affected_edges, 0);
        }
        assert_eq!(measured[0], measured[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn initialization_admits_edges_and_failed_maintenance_can_rebuild() {
    let ((), report) = run_async_under_lab(0x7e04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let query = definition(GlaDirection::Forward, None);
        // Five vertex rows fit, but the physical edge rows must also be admitted.
        assert!(matches!(db.register_standing_query(&cx, query.clone(),
            GqlQueryPolicy::new(5, 10_000, 10_000_000, 10_000_000)),
            Err(fgdb::StandingQueryError::Maintenance(fgdb::StandingQueryFailure::SnapshotBudget))));
        let probe = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
        let stats = *db.standing_query(&cx, &probe).unwrap().last_maintenance();
        let exact = GqlQueryPolicy::new(100_000, 10_000, stats.work_units, stats.scratch_entries);
        let handle = db.register_standing_query(&cx, query.clone(), exact).unwrap();
        for cap in [
            GqlQueryPolicy::new(100_000, 10_000, stats.work_units - 1, stats.scratch_entries),
            GqlQueryPolicy::new(100_000, 10_000, stats.work_units, stats.scratch_entries - 1),
        ] {
            assert!(db.register_standing_query(&cx, query.clone(), cap).is_err());
        }
        // Invalid numeric data is a view failure, not a failed durable write.
        let mut bad = WriteBatch::new(R);
        bad.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Bool(true)));
        let at = db.write(&commit, bad).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(db.standing_query(&cx, &handle), Err(fgdb::StandingQueryError::Unavailable { .. })));
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(3)));
        repair.delete_edge(EId(102));
        db.write(&commit, repair).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_state(&db, &cx, &handle, &query, GlaDirection::Forward, None);
        let mut next = WriteBatch::new(R);
        next.delete_vertex(VId(2));
        db.write(&commit, next).await.unwrap();
        assert_state(&db, &cx, &handle, &query, GlaDirection::Forward, None);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_global_edge_counts_and_repeated_endpoint_identity_are_preserved() {
    let ((), report) = run_async_under_lab(0x7e06, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut queries = Vec::new();
        for same in [false, true] {
            let mut pattern = GraphPatternBuilder::new();
            pattern.vertex("a").unwrap();
            if !same { pattern.vertex("b").unwrap(); }
            pattern.edge("a", R, GlaDirection::Undirected, if same { "a" } else { "b" }).unwrap();
            let input = pattern.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None)
                .unwrap().with_duplicates();
            let query = PreparedGraphAggregate::prepare(input, &[], &[
                GraphAggregate::count_rows("count"), GraphAggregate::count("vertices", 0),
            ], 0, None).unwrap();
            let handle = db.register_standing_query(&cx, query.clone(), policy()).unwrap();
            let view = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(view.rows().len(), 1);
            assert_eq!(view.rows().iter().next().unwrap().0.get(0).unwrap().as_count(), Some(0));
            queries.push((query, handle, same));
        }
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(1), VId(2), vec![]);
        batch.add_edge(EId(3), VId(1), VId(1), vec![]);
        db.write(&commit, batch).await.unwrap();
        for (query, handle, same) in &queries {
            let full = db.execute_graph_aggregate_governed(&cx, query, policy()).unwrap();
            let expected = if *same { 1 } else { 5 };
            assert_eq!(full.value[0].get(0).unwrap().as_count(), Some(expected));
            assert_eq!(full.value[0].get(1).unwrap().as_count(), Some(expected));
            assert_eq!(db.standing_query(&cx, handle).unwrap().rows().weight(&full.value[0]), Some(&ZWeight::ONE));
        }
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(1));
        deletion.delete_vertex(VId(2));
        db.write(&commit, deletion).await.unwrap();
        for (query, handle, _) in &queries {
            let full = db.execute_graph_aggregate_governed(&cx, query, policy()).unwrap();
            let view = db.standing_query(&cx, handle).unwrap();
            assert_eq!(view.rows().len(), 1);
            assert_eq!(full.value[0].get(0).unwrap().as_count(), Some(0));
            assert_eq!(view.rows().weight(&full.value[0]), Some(&ZWeight::ONE));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
