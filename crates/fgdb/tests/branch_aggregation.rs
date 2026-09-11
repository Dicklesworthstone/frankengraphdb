//! Interleaved hidden branches retain weights, snapshots and observations.
//! The oracle walks actual owned edge records and has no slot-remapping logic.

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
const U: RelationId = RelationId(4);
const V: RelationId = RelationId(5);
const STATEMENT: &str = "MATCH (a)-[:R]->(b), (b)-[:S]->(h)-[:T]->(leaf), \
    (b)-[:U]->(c), (c)-[:V]->(a), (c)-[:S]->(j), (a)-[:U]->(d) \
    RETURN c,d,COUNT(*) AS paths,COUNT(DISTINCT a) AS roots,MIN(a) AS lo,MAX(a) AS hi \
    GROUP BY c,d ORDER BY paths DESC,c,d";
type Summary = (VId, VId, u64, u64, VId, VId);
type EdgeSpec = (u128, u128, u128);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 100_000) }
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
        GraphSymbolKind::Relation => match name {
            "R" => Some(GraphSymbol::Relation(R)), "S" => Some(GraphSymbol::Relation(S)),
            "T" => Some(GraphSymbol::Relation(T)), "U" => Some(GraphSymbol::Relation(U)),
            "V" => Some(GraphSymbol::Relation(V)), _ => None,
        },
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(99));
    for id in [0,1,10,11,20,21,22,30,31,40,41,50,51,60,61] {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    let groups: [(RelationId, &[EdgeSpec]); 5] = [
        (R, &[(1,0,10), (2,0,10), (3,1,11)]),
        (S, &[(10,10,20), (11,10,20), (12,10,21), (13,10,22), (14,11,21),
            (15,40,50), (16,40,51), (17,41,51)]),
        (T, &[(20,20,30), (21,20,31), (22,21,30)]),
        (U, &[(30,10,40), (31,11,41), (32,0,60), (33,0,61), (34,1,61)]),
        (V, &[(40,40,0), (41,40,0), (42,41,1)]),
    ];
    let mut batches = vec![vertices];
    for (relation, entries) in groups {
        let mut batch = WriteBatch::new(relation);
        for &(eid, source, destination) in entries {
            batch.add_edge(EId(eid), VId(source), VId(destination), vec![]);
        }
        batches.push(batch);
    }
    db.write_atomic(cx, batches).await.unwrap()
}
fn oracle(edges: &[EdgeRecord]) -> Vec<Summary> {
    let mut groups: BTreeMap<(VId,VId), (u64,BTreeSet<VId>)> = BTreeMap::new();
    for root in edges.iter().filter(|e| e.entry.relation == R) {
        for hidden in edges.iter().filter(|e| e.entry.relation == S && e.entry.src == root.entry.dst) {
            for _leaf in edges.iter().filter(|e| e.entry.relation == T && e.entry.src == hidden.entry.dst) {
                for middle in edges.iter().filter(|e| e.entry.relation == U && e.entry.src == root.entry.dst) {
                    for _close in edges.iter().filter(|e| e.entry.relation == V
                        && e.entry.src == middle.entry.dst && e.entry.dst == root.entry.src) {
                        for _sibling in edges.iter().filter(|e| e.entry.relation == S && e.entry.src == middle.entry.dst) {
                            for last in edges.iter().filter(|e| e.entry.relation == U && e.entry.src == root.entry.src) {
                                let group = groups.entry((middle.entry.dst, last.entry.dst)).or_default();
                                group.0 += 1;
                                group.1.insert(root.entry.src);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut result: Vec<_> = groups.into_iter().map(|((c,d),(count,roots))|
        (c,d,count,roots.len() as u64,*roots.first().unwrap(),*roots.last().unwrap())).collect();
    result.sort_by_key(|row| (std::cmp::Reverse(row.2),row.0,row.1));
    result
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.keys()[1].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_value().unwrap().as_vertex().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex().unwrap())).collect()
}

#[test]
fn interleaved_forests_and_cycles_cover_all_reads_canonical_staging_and_reopening() {
    let ((), report) = run_async_under_lab(0xf0ce_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let query = prepare(STATEMENT); let original = query.canonical_bytes();
        let old = oracle(&db.edges().unwrap());
        assert_eq!(old, vec![(VId(40),VId(60),40,1,VId(0),VId(0)),
            (VId(40),VId(61),40,1,VId(0),VId(0)), (VId(41),VId(61),1,1,VId(1),VId(1))]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            view.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap()] {
            assert_eq!(plain(&result.value), old);
            assert_eq!(result.rows.snapshot_records, 22);
        }
        let measured = db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap();
        let exact = GqlQueryPolicy::new(22,3,measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx,&query,exact).unwrap(), measured);
        for policy in [GqlQueryPolicy::new(22,2,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(22,3,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(22,3,u64::MAX,measured.evaluator.scratch_entries-1)] {
            assert!(db.execute_graph_aggregate_governed(&cx,&query,policy).is_err());
        }
        let mut changes = WriteBatch::new(S);
        changes.delete_edge(EId(10));
        changes.ensure_edge_by_triple(EId(999),VId(10),VId(20),vec![]);
        changes.delete_vertex(VId(51));
        changes.add_edge(EId(90),VId(41),VId(50),vec![]);
        txn.write(&mut db,changes).unwrap();
        assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected = oracle(&txn.edges(&db).unwrap());
        assert_eq!(expected, vec![(VId(40),VId(60),12,1,VId(0),VId(0)),
            (VId(40),VId(61),12,1,VId(0),VId(0)), (VId(41),VId(61),1,1,VId(1),VId(1))]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),old);
        txn.commit(&mut db,&commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap().value),old);
        assert_eq!(plain(&view.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),old);
        assert_eq!(query.canonical_bytes(),original);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn eliminated_early_branches_keep_zero_completion_and_existing_edge_dependencies() {
    let ((), report) = run_async_under_lab(0xf0ce_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit,keys()).await.unwrap(); seed(&mut db,&commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777),vec![],vec![]);
                txn.write(&mut db,stage).unwrap();
                let query = prepare(&format!("{STATEMENT}{}",if mode==2 { " LIMIT 0" } else { "" }));
                let result = txn.execute_graph_aggregate_governed(&db,&cx,&query,
                    GqlQueryPolicy::new(1000,if mode==1 { 0 } else { 100 },1_000_000,100_000));
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(),3),
                    1 => assert!(matches!(result,Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                let mut winner = WriteBatch::new(T);
                match change {
                    0 => { winner.create_vertex(VId(888),vec![],vec![]); }
                    1 => { winner.add_edge(EId(91),VId(22),VId(30),vec![]); }
                    _ => { winner.delete_edge(EId(20)); }
                }
                db.write(&commit,winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // Commit immediately. Another query could restore a lost witness
                // and conceal a defect in the earlier refused or empty output.
                let result = txn.commit(&mut db,&commit).await;
                if change==0 { result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some()); }
                else {
                    assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(),frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn early_billion_assignment_branches_are_folded_before_the_returned_cycle() {
    let ((), report) = run_async_under_lab(0xf0ce_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit,keys()).await.unwrap();
        let mut vertices = WriteBatch::new(RelationId(99));
        for id in (0..10).chain(std::iter::once(20)) { vertices.create_vertex(VId(id),vec![],vec![]); }
        let mut root_edge = WriteBatch::new(R); root_edge.add_edge(EId(1),VId(0),VId(1),vec![]);
        let mut choices = WriteBatch::new(S);
        for id in 2..10 { choices.add_edge(EId(100+id),VId(1),VId(id),vec![]); }
        let mut target = WriteBatch::new(U); target.add_edge(EId(2),VId(1),VId(20),vec![]);
        let mut closing = WriteBatch::new(V); closing.add_edge(EId(3),VId(20),VId(0),vec![]);
        db.write_atomic(&commit,vec![vertices,root_edge,choices,target,closing]).await.unwrap();
        let mut statement = "MATCH (a)-[:R]->(b)".to_owned();
        for at in 0..10 { statement.push_str(&format!(",(b)-[:S]->(h{at})")); }
        statement.push_str(",(b)-[:U]->(c)-[:V]->(a) RETURN c,COUNT(*) AS n,COUNT(DISTINCT a) AS d GROUP BY c");
        let query = prepare(&statement);
        let result = db.execute_graph_aggregate_governed(&cx,&query,
            GqlQueryPolicy::new(11,1,65_536,4096)).unwrap();
        assert_eq!(result.value.len(),1);
        assert_eq!(result.value[0].keys()[0].as_vertex(),Some(VId(20)));
        assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(8_u64.pow(10)));
        assert_eq!(result.value[0].get(1).unwrap().as_count(),Some(1));
        assert_eq!(result.rows.snapshot_records,11);
        assert!(matches!(db.execute_graph_aggregate_governed(&cx,&query,
            GqlQueryPolicy::new(10,1,u64::MAX,u64::MAX)),Err(GqlQueryError::Rows(_))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
