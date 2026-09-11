//! Multiway access keeps canonical results and transaction observations intact.
//! The oracle enumerates actual owned edge records, not another compiled query.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const SCORE: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH (a)-[:R]->(b)-[:S]->(c), (a)-[:T]->(c), (c)-[:U]->(a), (b)-[:V]->(c)";
type Plain = (Option<i64>, VId, VId);
type Summary = (VId, u64, Option<i128>);
type EdgeSpec = (u128, u128, u128);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Relation, "U") => Some(GraphSymbol::Relation(RelationId(4))),
        (GraphSymbolKind::Relation, "V") => Some(GraphSymbol::Relation(RelationId(5))),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}
fn pattern() -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!("{HEAD} RETURN c.score AS score,a,c"), symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn aggregate() -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(&format!(
        "{HEAD} RETURN a,COUNT(*) AS paths,SUM(c.score) AS total GROUP BY a ORDER BY paths DESC,a ASC"), symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0, 1, 10, 11] { vertices.create_vertex(VId(id), vec![], vec![]); }
    for (id, score) in [(20, 5), (21, 7), (30, 9)] {
        vertices.create_vertex(VId(id), vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    let groups: [(u64, &[EdgeSpec]); 5] = [
        (1, &[(10, 0, 10), (11, 0, 10), (12, 1, 11)]),
        (2, &[(20, 10, 20), (21, 10, 20), (22, 10, 21), (23, 11, 21), (24, 10, 30)]),
        (3, &[(30, 0, 20), (31, 0, 20), (32, 0, 20), (33, 0, 21), (34, 1, 21), (35, 0, 30)]),
        (4, &[(40, 20, 0), (41, 20, 0), (42, 21, 1), (43, 21, 0)]),
        (5, &[(50, 10, 20), (51, 11, 21), (52, 10, 30)]),
    ];
    let mut batches = vec![vertices];
    for (relation, entries) in groups {
        let mut batch = WriteBatch::new(RelationId(relation));
        for &(eid, source, destination) in entries {
            batch.add_edge(EId(eid), VId(source), VId(destination), vec![]);
        }
        batches.push(batch);
    }
    db.write_atomic(cx, batches).await.unwrap()
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut result = Vec::new();
    for first in edges.iter().filter(|e| e.entry.relation == RelationId(1)) {
        for second in edges.iter().filter(|e| e.entry.relation == RelationId(2) && e.entry.src == first.entry.dst) {
            for _ in edges.iter().filter(|e| e.entry.relation == RelationId(3)
                && e.entry.src == first.entry.src && e.entry.dst == second.entry.dst) {
                for _ in edges.iter().filter(|e| e.entry.relation == RelationId(4)
                    && e.entry.src == second.entry.dst && e.entry.dst == first.entry.src) {
                    for _ in edges.iter().filter(|e| e.entry.relation == RelationId(5)
                        && e.entry.src == first.entry.dst && e.entry.dst == second.entry.dst) {
                        let vertex = vertices.iter().find(|row| row.vid == second.entry.dst).unwrap();
                        let score = vertex.props.iter().find_map(|(key, value)| match value {
                            CanonicalScalar::Int(value) if *key == SCORE => Some(*value),
                            _ => None,
                        });
                        result.push((score, first.entry.src, second.entry.dst));
                    }
                }
            }
        }
    }
    result.sort(); result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let score = match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value), CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture scalar"),
        };
        (score, row.get(1).unwrap().as_vertex().unwrap(), row.get(2).unwrap().as_vertex().unwrap())
    }).collect()
}
fn expected_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups = BTreeMap::new();
    for &(score, key, _) in rows {
        let entry = groups.entry(key).or_insert((0_u64, None::<i128>));
        entry.0 += 1;
        if let Some(score) = score { entry.1 = Some(entry.1.unwrap_or(0) + i128::from(score)); }
    }
    let mut rows: Vec<_> = groups.into_iter().map(|(key, (count, sum))| (key, count, sum)).collect();
    rows.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0)); rows
}
fn summary(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.get(0).unwrap().as_count().unwrap(),
        row.get(1).unwrap().as_integer())).collect()
}

#[test]
fn multiconstraint_bags_and_aggregates_agree_across_all_five_read_entrypoints() {
    let ((), report) = run_async_under_lab(0x4d57_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap(); let txn = db.begin(&txn_cx).unwrap();
        let pattern = pattern(); let aggregate = aggregate();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(expected.len(), 25);
        assert_eq!(expected_summary(&expected), vec![(VId(0), 24, Some(120)), (VId(1), 1, Some(7))]);
        for result in [db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap(),
            view.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap(),
            view.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, wide()).unwrap()] {
            assert_eq!(plain(&result.value), expected);
        }
        for result in [db.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, wide()).unwrap(),
            view.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, wide()).unwrap()] {
            assert_eq!(summary(&result.value), expected_summary(&expected));
        }
        let measured = db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &pattern, exact).unwrap(), measured);
        for cap in [GqlQueryPolicy::new(1000, 24, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 25, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 25, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_pattern_governed(&cx, &pattern, cap).is_err());
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multiconstraint_results_track_canonical_staging_and_reopened_history() {
    let ((), report) = run_async_under_lab(0x4d57_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let pattern = pattern(); let aggregate = aggregate(); let frozen = pattern.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut changes = WriteBatch::new(RelationId(4));
        changes.delete_edge(EId(40)); changes.ensure_edge_by_triple(EId(999), VId(20), VId(0), vec![]);
        changes.add_edge(EId(90), VId(30), VId(0), vec![]);
        changes.set_vertex_property(VId(20), SCORE, None);
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected.len(), 15);
        assert_eq!(expected_summary(&expected), vec![(VId(0), 14, Some(18)), (VId(1), 1, Some(7))]);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &pattern, wide()).unwrap().value), expected);
        assert_eq!(summary(&txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, wide()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), expected);
        assert_eq!(summary(&reopened.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap().value), old);
        assert_eq!(plain(&view.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), old);
        assert_eq!(pattern.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_rejected_candidates_keep_dependencies_even_after_output_refusal() {
    let ((), report) = run_async_under_lab(0x4d57_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let pattern = pattern();
        for refused in [false, true] {
            for change in 0..4 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(RelationId(1)); stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let result = txn.execute_graph_pattern_governed(&db, &cx, &pattern,
                    GqlQueryPolicy::new(1000, if refused { 0 } else { 1000 }, 1_000_000, 1_000_000));
                if refused { assert!(matches!(result, Err(GqlQueryError::Rows(_)))); }
                else { assert_eq!(result.unwrap().value.len(), 25); }
                let mut winner = WriteBatch::new(RelationId(if change == 1 { 5 } else { 4 }));
                match change {
                    0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                    1 => { winner.add_edge(EId(91), VId(10), VId(21), vec![]); }
                    2 => { winner.set_vertex_property(VId(30), SCORE, Some(CanonicalScalar::Int(20))); }
                    _ => { winner.add_edge(EId(92), VId(30), VId(0), vec![]); }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // Commit immediately. No extra read can repair an observation
                // lost during pruning or the earlier result-budget refusal.
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
