//! Weighted summaries preserve the durable source and transaction observations.
//! Expected counts enumerate actual owned edge occurrences, not another query.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const STATEMENT: &str = "MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a,COUNT(*) AS paths,\
    COUNT(DISTINCT b) AS targets,MIN(b) AS least,MAX(b) AS greatest GROUP BY a ORDER BY paths DESC,a ASC";
type Plain = (VId, u64, u64, VId, VId);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0, 1, 10, 11, 99] { vertices.create_vertex(VId(id), vec![], vec![]); }
    let mut first = WriteBatch::new(R);
    for (eid, src, dst) in [(10, 0, 10), (11, 0, 10), (12, 0, 11), (13, 1, 11), (14, 1, 10)] {
        first.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, src, dst) in [(20, 10, 0), (21, 10, 0), (22, 10, 0), (23, 11, 0), (24, 11, 0), (25, 11, 1)] {
        second.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second]).await.unwrap()
}
fn oracle(edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut groups: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for first in edges.iter().filter(|row| row.entry.relation == R) {
        for _ in edges.iter().filter(|row| row.entry.relation == S
            && row.entry.src == first.entry.dst && row.entry.dst == first.entry.src) {
            groups.entry(first.entry.src).or_default().push(first.entry.dst);
        }
    }
    let mut rows: Vec<_> = groups.into_iter().map(|(owner, values)| {
        let support: BTreeSet<_> = values.iter().copied().collect();
        (owner, values.len() as u64, support.len() as u64,
            *support.first().unwrap(), *support.last().unwrap())
    }).collect();
    rows.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0)); rows
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Plain> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_value().unwrap().as_vertex().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex().unwrap())).collect()
}

#[test]
fn weighted_counts_cover_all_read_surfaces_and_canonical_staging_and_history() {
    let ((), report) = run_async_under_lab(0xfa67_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let aggregate = prepare(STATEMENT); let frozen = aggregate.canonical_bytes();
        let old = oracle(&db.edges().unwrap());
        assert_eq!(old, vec![(VId(0), 8, 2, VId(10), VId(11)), (VId(1), 1, 1, VId(11), VId(11))]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, wide()).unwrap(),
            view.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, wide()).unwrap()] {
            assert_eq!(plain(&result.value), old);
            assert_eq!(result.rows.snapshot_records, 11, "do not substitute compressed topology cardinality");
        }
        let measured = db.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap();
        let exact = GqlQueryPolicy::new(11, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx, &aggregate, exact).unwrap(), measured);
        for policy in [GqlQueryPolicy::new(10, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(11, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(11, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(11, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_aggregate_governed(&cx, &aggregate, policy).is_err());
        }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(10)); changes.delete_edge(EId(12));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.add_edge(EId(15), VId(1), VId(11), vec![]);
        changes.set_vertex_property(VId(10), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.edges(&db).unwrap());
        assert_eq!(expected, vec![(VId(0), 3, 1, VId(10), VId(10)), (VId(1), 2, 1, VId(11), VId(11))]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, wide()).unwrap().value), expected);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap().value), expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, wide()).unwrap().value), old);
        assert_eq!(plain(&view.execute_graph_aggregate_governed(&cx, &aggregate, wide()).unwrap().value), old);
        assert_eq!(aggregate.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compressed_and_rejected_counts_retain_insertion_and_existing_edge_dependencies() {
    let ((), report) = run_async_under_lab(0xfa67_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let aggregate = prepare(&format!("{STATEMENT}{}", if mode == 2 { " LIMIT 0" } else { "" }));
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &aggregate,
                    GqlQueryPolicy::new(1000, if mode == 1 { 0 } else { 100 }, 1_000_000, 100_000));
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 2),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                let mut winner = WriteBatch::new(S);
                match change {
                    0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                    1 => { winner.add_edge(EId(90), VId(10), VId(1), vec![]); }
                    _ => { winner.delete_edge(EId(20)); }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // Do not repair a lost observation with a second query/read.
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
fn actual_database_counts_a_billion_parallel_walks_under_a_small_work_allowance() {
    let ((), report) = run_async_under_lab(0xfa67_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R); seed.create_vertex(VId(5), vec![], vec![]);
        for id in 100..108 { seed.add_edge(EId(id), VId(5), VId(5), vec![]); }
        db.write(&commit, seed).await.unwrap();
        let aggregate = prepare(&format!("MATCH (a){} RETURN COUNT(*) AS paths,COUNT(DISTINCT a) AS owners",
            "-[:R]->(a)".repeat(10)));
        let result = db.execute_graph_aggregate_governed(&cx, &aggregate,
            GqlQueryPolicy::new(8, 1, 65_536, 2048)).unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(8_u64.pow(10)));
        assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(1));
        assert_eq!(result.rows.snapshot_records, 8);
        assert!(matches!(db.execute_graph_aggregate_governed(&cx, &aggregate,
            GqlQueryPolicy::new(7, 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
