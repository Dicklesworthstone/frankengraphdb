//! Product checks for indexed closing constraints, with independent row oracles.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GraphAggregateRow, GraphSymbol, GraphSymbolKind, GqlParameters, GqlQueryError,
    GqlQueryPolicy, PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a)";
type Plain = (VId, VId, Option<i64>);
type Summary = (VId, u64, Option<i128>);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
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
    PreparedGraphText::prepare(&format!("{HEAD} RETURN a,c,c.score AS score"), symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn aggregate() -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(&format!("{HEAD} RETURN a,COUNT(*) AS n,SUM(c.score) AS total GROUP BY a ORDER BY n DESC,a ASC"), symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0, 1, 10, 11, 99] { vertices.create_vertex(VId(id), vec![], vec![]); }
    for (id, score) in [(20, 5), (21, 7), (30, 9)] {
        vertices.create_vertex(VId(id), vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    let mut first = WriteBatch::new(R);
    for (eid, source, destination) in [(10, 0, 10), (11, 0, 10), (12, 1, 11)] {
        first.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, source, destination) in [(20, 10, 20), (21, 10, 20), (22, 10, 21), (23, 11, 21), (24, 10, 30)] {
        second.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    let mut third = WriteBatch::new(T);
    for (eid, source, destination) in [(30, 20, 0), (31, 20, 0), (32, 20, 0), (33, 21, 1), (34, 30, 99)] {
        third.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second, third]).await.unwrap()
}

fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut result = Vec::new();
    for first in edges.iter().filter(|row| row.entry.relation == R) {
        for second in edges.iter().filter(|row| row.entry.relation == S && row.entry.src == first.entry.dst) {
            for _ in edges.iter().filter(|row| row.entry.relation == T && row.entry.src == second.entry.dst && row.entry.dst == first.entry.src) {
                let vertex = vertices.iter().find(|row| row.vid == second.entry.dst).unwrap();
                let score = vertex.props.iter().find_map(|(key, value)| match value {
                    CanonicalScalar::Int(value) if *key == SCORE => Some(*value),
                    _ => None,
                });
                result.push((first.entry.src, second.entry.dst, score));
            }
        }
    }
    result.sort_unstable(); result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let score = match row.get(2).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value), CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture value"),
        };
        (row.get(0).unwrap().as_vertex().unwrap(), row.get(1).unwrap().as_vertex().unwrap(), score)
    }).collect()
}
fn expected_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, (u64, Option<i128>)> = BTreeMap::new();
    for &(key, _, value) in rows {
        let group = groups.entry(key).or_insert((0, None));
        group.0 += 1;
        if let Some(value) = value { group.1 = Some(group.1.unwrap_or(0) + i128::from(value)); }
    }
    let mut result: Vec<_> = groups.into_iter().map(|(key, (count, sum))| (key, count, sum)).collect();
    result.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0)); result
}
fn summary(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.get(0).unwrap().as_count().unwrap(),
        row.get(1).unwrap().as_integer())).collect()
}

#[test]
fn indexed_pattern_and_aggregation_agree_across_all_five_read_entrypoints() {
    let ((), report) = run_async_under_lab(0x10a1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap(); let txn = db.begin(&txn_cx).unwrap();
        let pattern = pattern(); let aggregate = aggregate();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(expected.len(), 13);
        assert_eq!(expected_summary(&expected), vec![(VId(0), 12, Some(60)), (VId(1), 1, Some(7))]);
        for result in [db.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy()).unwrap(),
            view.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap(),
            view.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy()).unwrap()] {
            assert_eq!(plain(&result.value), expected);
        }
        for result in [db.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy()).unwrap(),
            view.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy()).unwrap()] {
            assert_eq!(summary(&result.value), expected_summary(&expected));
        }
        let measured = db.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &pattern, exact).unwrap(), measured);
        for cap in [GqlQueryPolicy::new(measured.rows.snapshot_records, 12, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(measured.rows.snapshot_records, 13, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(measured.rows.snapshot_records, 13, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_pattern_governed(&cx, &pattern, cap).is_err());
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_results_follow_canonical_staging_and_survive_compaction_and_reopening() {
    let ((), report) = run_async_under_lab(0x10a1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let pattern = pattern(); let aggregate = aggregate();
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
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy()).unwrap().value), expected);
        assert_eq!(summary(&txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap().value), expected);
        assert_eq!(summary(&reopened.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap().value), expected_summary(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy()).unwrap().value), old);
        assert_eq!(plain(&view.execute_graph_pattern_governed(&cx, &pattern, policy()).unwrap().value), old);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_closing_constraints_keep_phantom_and_rejected_vertex_dependencies() {
    let ((), report) = run_async_under_lab(0x10a1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let pattern = pattern();
        for refused in [false, true] {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R); staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let cap = GqlQueryPolicy::new(1000, if refused { 0 } else { 1000 }, 1_000_000, 1_000_000);
                let result = txn.execute_graph_pattern_governed(&db, &cx, &pattern, cap);
                if refused { assert!(matches!(result, Err(GqlQueryError::Rows(_)))); }
                else { assert_eq!(result.unwrap().value.len(), 13); }
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
                    result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
