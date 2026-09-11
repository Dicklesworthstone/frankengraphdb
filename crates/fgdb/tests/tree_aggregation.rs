//! Folded branches must retain exact bag counts and their read dependencies.
//! The oracle enumerates owned edge occurrences, not another compiled query.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const STATEMENT: &str = "MATCH (a)-[:R]->(b), (b)-[:S]->(c)-[:T]->(d), (b)-[:S]->(e) \
    RETURN a,COUNT(*) AS n,COUNT(b) AS m,COUNT(DISTINCT b) AS distinct_b,MIN(b) AS lo,MAX(b) AS hi \
    GROUP BY a ORDER BY n DESC,a ASC";
type Summary = (VId, u64, u64, u64, Option<VId>, Option<VId>);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(T)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0, 1, 2, 10, 11, 12, 20, 21, 22, 30, 31] {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    let mut first = WriteBatch::new(R);
    for (id, a, b) in [(10, 0, 10), (11, 0, 10), (12, 1, 11), (13, 2, 12)] {
        first.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (id, a, b) in [(20, 10, 20), (21, 10, 20), (22, 10, 21), (23, 11, 21), (24, 12, 22)] {
        second.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    let mut third = WriteBatch::new(T);
    for (id, a, b) in [(30, 20, 30), (31, 21, 31), (32, 21, 31)] {
        third.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second, third]).await.unwrap()
}
fn oracle(edges: &[EdgeRecord]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for ab in edges.iter().filter(|e| e.entry.relation == R) {
        for bc in edges.iter().filter(|e| e.entry.relation == S && e.entry.src == ab.entry.dst) {
            for _cd in edges.iter().filter(|e| e.entry.relation == T && e.entry.src == bc.entry.dst) {
                for _be in edges.iter().filter(|e| e.entry.relation == S && e.entry.src == ab.entry.dst) {
                    groups.entry(ab.entry.src).or_default().push(ab.entry.dst);
                }
            }
        }
    }
    let mut result: Vec<_> = groups.into_iter().map(|(a, values)| {
        let support: BTreeSet<_> = values.iter().copied().collect();
        (a, values.len() as u64, values.len() as u64, support.len() as u64,
            support.first().copied(), support.last().copied())
    }).collect();
    result.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0));
    result
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_count().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex(),
        row.get(4).unwrap().as_value().unwrap().as_vertex())).collect()
}

#[test]
fn forest_results_follow_canonical_staging_and_all_retained_read_surfaces() {
    let ((), report) = run_async_under_lab(0xf07e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let query = prepare(STATEMENT); let frozen = query.canonical_bytes();
        let expected = oracle(&db.edges().unwrap());
        assert_eq!(expected, vec![(VId(0), 24, 24, 1, Some(VId(10)), Some(VId(10))),
            (VId(1), 2, 2, 1, Some(VId(11)), Some(VId(11)))]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            view.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap()] {
            assert_eq!(plain(&result.value), expected);
        }
        let measured = db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx, &query, exact).unwrap(), measured);
        for cap in [GqlQueryPolicy::new(1000, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_aggregate_governed(&cx, &query, cap).is_err());
        }
        let mut changes = WriteBatch::new(S);
        changes.delete_edge(EId(20));
        changes.ensure_edge_by_triple(EId(999), VId(10), VId(20), vec![]);
        changes.delete_vertex(VId(21));
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let staged = oracle(&txn.edges(&db).unwrap());
        assert_eq!(staged, vec![(VId(0), 2, 2, 1, Some(VId(10)), Some(VId(10)))]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap().value), staged);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), expected);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), staged);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), expected);
        assert_eq!(plain(&view.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), expected);
        assert_eq!(query.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_and_eliminated_branches_keep_dependencies_after_refused_output() {
    let ((), report) = run_async_under_lab(0xf07e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let query = prepare(&format!("{STATEMENT}{}", if mode == 2 { " LIMIT 0" } else { "" }));
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &query,
                    GqlQueryPolicy::new(1000, if mode == 1 { 0 } else { 100 }, 1_000_000, 100_000));
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 2),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                let mut winner = WriteBatch::new(if change == 2 { S } else { T });
                match change {
                    0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                    1 => { winner.add_edge(EId(90), VId(22), VId(30), vec![]); }
                    _ => { winner.delete_edge(EId(20)); }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // Commit immediately: no follow-up read may repair a lost
                // dependency for the zero-completion or compressed branches.
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

#[test]
fn distinct_vertex_assignments_are_factored_in_the_actual_database_entrypoint() {
    let ((), report) = run_async_under_lab(0xf07e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut vertices = WriteBatch::new(RelationId(9));
        for id in 0..10 { vertices.create_vertex(VId(id), vec![], vec![]); }
        let mut first = WriteBatch::new(R); first.add_edge(EId(10), VId(0), VId(1), vec![]);
        let mut choices = WriteBatch::new(S);
        for id in 2..10 { choices.add_edge(EId(100 + id), VId(1), VId(id), vec![]); }
        db.write_atomic(&commit, vec![vertices, first, choices]).await.unwrap();
        let mut statement = "MATCH (a)-[:R]->(b)".to_owned();
        for at in 0..10 { statement.push_str(&format!(",(b)-[:S]->(x{at})")); }
        statement.push_str(" RETURN COUNT(*) AS n,COUNT(DISTINCT b) AS d");
        let query = prepare(&statement);
        let result = db.execute_graph_aggregate_governed(&cx, &query,
            GqlQueryPolicy::new(9, 1, 65_536, 2048)).unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(8_u64.pow(10)));
        assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(1));
        assert_eq!(result.rows.snapshot_records, 9);
        assert!(matches!(db.execute_graph_aggregate_governed(&cx, &query,
            GqlQueryPolicy::new(8, 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
