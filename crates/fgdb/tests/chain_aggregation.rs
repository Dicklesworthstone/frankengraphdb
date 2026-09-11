//! Joint endpoint factors must preserve source snapshots and original observations.
//! The oracle enumerates owned edge occurrences, not another lowered query.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const U: RelationId = RelationId(3);
const T: RelationId = RelationId(4);
const V: RelationId = RelationId(5);
const W: RelationId = RelationId(6);
const STATEMENT: &str = "MATCH (a)-[:R]->(b)-[:S]->(h)-[:U]->(k)-[:S]->(c), \
    (h)-[:T]->(leaf), (c)-[:V]->(a), (c)-[:W]->(tail) \
    RETURN a,c,COUNT(*) AS n,COUNT(DISTINCT b) AS d,MIN(b) AS lo,MAX(b) AS hi \
    GROUP BY a,c ORDER BY n DESC,a,c";
type Summary = (VId, VId, u64, u64, VId, VId);
type EdgeSpec = (u128, u128, u128);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1;32],DatabaseSecurityNamespaceId([0xc2;32]),[0xc3;32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000,1000,2_000_000,100_000) }
fn prepare(statement: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(statement, |kind,name| match kind {
        GraphSymbolKind::Relation => match name {
            "R"=>Some(GraphSymbol::Relation(R)), "S"=>Some(GraphSymbol::Relation(S)),
            "U"=>Some(GraphSymbol::Relation(U)), "T"=>Some(GraphSymbol::Relation(T)),
            "V"=>Some(GraphSymbol::Relation(V)), "W"=>Some(GraphSymbol::Relation(W)), _=>None,
        },
        _=>None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for id in [0,1,10,11,20,21,22,23,30,31,32,40,41,50,51,60,61] {
        vertices.create_vertex(VId(id),vec![],vec![]);
    }
    let groups: [(RelationId,&[EdgeSpec]);6] = [
        (R,&[(1,0,10),(2,0,10),(3,1,11)]),
        (S,&[(10,10,20),(11,10,21),(12,11,22),(13,10,23),
            (14,30,40),(15,30,40),(16,31,40),(17,32,41)]),
        (U,&[(20,20,30),(21,20,30),(22,20,31),(23,21,31),(24,22,32)]),
        (T,&[(30,20,50),(31,20,51),(32,21,51),(33,22,51),(34,23,51)]),
        (V,&[(40,40,0),(41,41,1)]),
        (W,&[(60,40,60),(61,40,61),(62,41,61)]),
    ];
    let mut batches = vec![vertices];
    for (relation,edges) in groups {
        let mut batch = WriteBatch::new(relation);
        for &(eid,source,destination) in edges {
            batch.add_edge(EId(eid),VId(source),VId(destination),vec![]);
        }
        batches.push(batch);
    }
    db.write_atomic(cx,batches).await.unwrap()
}
fn oracle(edges: &[EdgeRecord]) -> Vec<Summary> {
    let mut groups: BTreeMap<(VId,VId),(u64,BTreeSet<VId>)> = BTreeMap::new();
    for root in edges.iter().filter(|e| e.entry.relation==R) {
        for first in edges.iter().filter(|e| e.entry.relation==S && e.entry.src==root.entry.dst) {
            for middle in edges.iter().filter(|e| e.entry.relation==U && e.entry.src==first.entry.dst) {
                for last in edges.iter().filter(|e| e.entry.relation==S && e.entry.src==middle.entry.dst) {
                    for _side in edges.iter().filter(|e| e.entry.relation==T && e.entry.src==first.entry.dst) {
                        for _close in edges.iter().filter(|e| e.entry.relation==V
                            && e.entry.src==last.entry.dst && e.entry.dst==root.entry.src) {
                            for _tail in edges.iter().filter(|e| e.entry.relation==W && e.entry.src==last.entry.dst) {
                                let group = groups.entry((root.entry.src,last.entry.dst)).or_default();
                                group.0+=1; group.1.insert(root.entry.dst);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut result: Vec<_> = groups.into_iter().map(|((a,c),(n,support))|
        (a,c,n,support.len() as u64,*support.first().unwrap(),*support.last().unwrap())).collect();
    result.sort_by_key(|row| (std::cmp::Reverse(row.2),row.0,row.1)); result
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),row.keys()[1].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(),row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_value().unwrap().as_vertex().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex().unwrap())).collect()
}

#[test]
fn joint_endpoints_composed_with_forests_cover_all_reads_staging_and_reopened_history() {
    let ((),report) = run_async_under_lab(0xc0a1_0001,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit(); let txn_cx=contexts.txn();
        let vfs=MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await; let view=db.read_session().unwrap();
        let query=prepare(STATEMENT); let original=query.canonical_bytes();
        let old=oracle(&db.edges().unwrap());
        assert_eq!(old,vec![(VId(0),VId(40),44,1,VId(10),VId(10)),(VId(1),VId(41),1,1,VId(11),VId(11))]);
        let mut txn=db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            view.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            view.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap()] {
            assert_eq!(plain(&result.value),old); assert_eq!(result.rows.snapshot_records,26);
        }
        let measured=db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap();
        let exact=GqlQueryPolicy::new(26,2,measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx,&query,exact).unwrap(),measured);
        for cap in [GqlQueryPolicy::new(25,2,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(26,1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(26,2,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(26,2,u64::MAX,measured.evaluator.scratch_entries-1)] {
            assert!(db.execute_graph_aggregate_governed(&cx,&query,cap).is_err());
        }
        let mut changes=WriteBatch::new(U);
        changes.delete_edge(EId(20));
        changes.ensure_edge_by_triple(EId(999),VId(20),VId(30),vec![]);
        changes.add_edge(EId(90),VId(23),VId(30),vec![]);
        changes.delete_vertex(VId(31));
        txn.write(&mut db,changes).unwrap();
        assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected=oracle(&txn.edges(&db).unwrap());
        assert_eq!(expected,vec![(VId(0),VId(40),24,1,VId(10),VId(10)),(VId(1),VId(41),1,1,VId(11),VId(11))]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),old);
        txn.commit(&mut db,&commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap().value),old);
        assert_eq!(plain(&view.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),old);
        assert_eq!(query.canonical_bytes(),original);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn contracted_hidden_paths_keep_missing_existing_and_attachment_edge_dependencies() {
    let ((),report) = run_async_under_lab(0xc0a1_0002,|root| async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit(); let txn_cx=contexts.txn();
        for mode in 0..3 { for change in 0..4 {
            let mut db=Database::open_memory(&commit,keys()).await.unwrap(); seed(&mut db,&commit).await;
            let mut txn=db.begin(&txn_cx).unwrap();
            let mut stage=WriteBatch::new(R); stage.create_vertex(VId(777),vec![],vec![]);
            txn.write(&mut db,stage).unwrap();
            let query=prepare(&format!("{STATEMENT}{}",if mode==2 {" LIMIT 0"} else {""}));
            let result=txn.execute_graph_aggregate_governed(&db,&cx,&query,
                GqlQueryPolicy::new(1000,if mode==1 {0} else {100},2_000_000,100_000));
            match mode {
                0=>assert_eq!(result.unwrap().value.len(),2),
                1=>assert!(matches!(result,Err(GqlQueryError::Rows(_)))),
                _=>assert!(result.unwrap().value.is_empty()),
            }
            let mut winner=WriteBatch::new(if change==3 {W} else {U});
            match change {
                0=>{winner.create_vertex(VId(888),vec![],vec![]);}
                1=>{winner.add_edge(EId(90),VId(23),VId(30),vec![]);}
                2=>{winner.delete_edge(EId(21));}
                _=>{winner.delete_edge(EId(61));}
            }
            db.write(&commit,winner).await.unwrap(); let frontier=db.frontier().unwrap();
            // Commit immediately. No read may restore a dependency lost by
            // contraction, zero-completion pruning or an output refusal.
            let result=txn.commit(&mut db,&commit).await;
            if change==0 {result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some());}
            else {
                assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law:"FG-LAW-FCW-READ-01", ..
                }))));
                assert_eq!(db.frontier().unwrap(),frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn database_returns_joint_endpoints_of_a_billion_walks_with_original_source_limits() {
    let ((),report) = run_async_under_lab(0xc0a1_0003,|root| async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut vertices=WriteBatch::new(RelationId(9));
        for id in 0..=8 {vertices.create_vertex(VId(id),vec![],vec![]);}
        let mut first=WriteBatch::new(R); first.add_edge(EId(1),VId(0),VId(1),vec![]);
        let mut transitions=WriteBatch::new(S);
        let mut eid=10;
        for a in 1..=8 { for b in 1..=8 {
            transitions.add_edge(EId(eid),VId(a),VId(b),vec![]); eid+=1;
        }}
        db.write_atomic(&commit,vec![vertices,first,transitions]).await.unwrap();
        let mut statement="MATCH (a)-[:R]->(b)".to_owned();
        for at in 1..=10 {statement.push_str(&format!("-[:S]->(x{at})"));}
        statement.push_str(" RETURN a,x10,COUNT(*) AS n GROUP BY a,x10");
        let query=prepare(&statement);
        let result=db.execute_graph_aggregate_governed(&cx,&query,
            GqlQueryPolicy::new(65,8,131_072,16_384)).unwrap();
        assert_eq!(result.value.len(),8); assert_eq!(result.rows.snapshot_records,65);
        for (at,row) in result.value.iter().enumerate() {
            assert_eq!(row.keys()[0].as_vertex(),Some(VId(0)));
            assert_eq!(row.keys()[1].as_vertex(),Some(VId(at as u128+1)));
            assert_eq!(row.get(0).unwrap().as_count(),Some(8_u64.pow(9)));
        }
        assert!(matches!(db.execute_graph_aggregate_governed(&cx,&query,
            GqlQueryPolicy::new(64,8,u64::MAX,u64::MAX)),Err(GqlQueryError::Rows(_))));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
