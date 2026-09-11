//! Cyclic factorization preserves canonical snapshots and transaction witnesses.
//! The oracle joins owned edge occurrences without factor tables or slot aliases.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const R:RelationId=RelationId(1);
const S:RelationId=RelationId(2);
const T:RelationId=RelationId(3);
const U:RelationId=RelationId(4);
const V:RelationId=RelationId(5);
const STATEMENT:&str="MATCH (a)-[:R]->(b)-[:S]->(x)-[:T]->(y)-[:U]->(b), (b)-[:V]->(leaf) RETURN a,b,COUNT(*) AS n GROUP BY a,b ORDER BY n DESC,a,b";
type Summary=(VId,VId,u64);
fn keys()->DatabaseKeys {DatabaseKeys::new([0xc1;32],DatabaseSecurityNamespaceId([0xc2;32]),[0xc3;32])}
fn wide()->GqlQueryPolicy {GqlQueryPolicy::new(1000,1000,4_000_000,1_000_000)}
fn prepare(text:&str)->PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text,|kind,name|match kind {
        GraphSymbolKind::Relation=>Some(GraphSymbol::Relation(match name {
            "R"=>R,"S"=>S,"T"=>T,"U"=>U,_=>V,
        })),_=>None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db:&mut Database<MemVfs>,cx:&CommitCx)->CommitSeq {
    let mut vertices=WriteBatch::new(RelationId(99));
    for id in [0,1,10,11,20,21,22,30,31,32,50,51] {vertices.create_vertex(VId(id),vec![],vec![]);}
    type Entry=(u128,u128,u128);
    let groups:[(RelationId,&[Entry]);5]=[
        (R,&[(1,0,10),(2,0,10),(3,1,11)]),
        (S,&[(10,10,20),(11,10,20),(12,10,21),(13,10,22),(14,11,21)]),
        (T,&[(20,20,30),(21,20,31),(22,21,30),(23,22,32)]),
        (U,&[(30,30,10),(31,30,10),(32,31,10),(33,30,11)]),
        (V,&[(40,10,50),(41,10,51),(42,11,51)]),
    ];
    let mut batches=vec![vertices];
    for (relation,entries) in groups {
        let mut batch=WriteBatch::new(relation);
        for &(eid,a,b) in entries {batch.add_edge(EId(eid),VId(a),VId(b),vec![]);}
        batches.push(batch);
    }
    db.write_atomic(cx,batches).await.unwrap()
}
fn oracle(edges:&[EdgeRecord])->Vec<Summary> {
    let mut counts=BTreeMap::new();
    for root in edges.iter().filter(|e|e.entry.relation==R) {
        for first in edges.iter().filter(|e|e.entry.relation==S && e.entry.src==root.entry.dst) {
            for second in edges.iter().filter(|e|e.entry.relation==T && e.entry.src==first.entry.dst) {
                for _closing in edges.iter().filter(|e|e.entry.relation==U && e.entry.src==second.entry.dst && e.entry.dst==root.entry.dst) {
                    for _leaf in edges.iter().filter(|e|e.entry.relation==V && e.entry.src==root.entry.dst) {
                        *counts.entry((root.entry.src,root.entry.dst)).or_insert(0_u64)+=1;
                    }
                }
            }
        }
    }
    let mut rows:Vec<_>=counts.into_iter().map(|((a,b),n)|(a,b,n)).collect();
    rows.sort_by_key(|row|(std::cmp::Reverse(row.2),row.0,row.1));rows
}
fn plain(rows:&[GraphAggregateRow])->Vec<Summary> {
    rows.iter().map(|row|(row.keys()[0].as_vertex().unwrap(),row.keys()[1].as_vertex().unwrap(),row.get(0).unwrap().as_count().unwrap())).collect()
}

#[test]
fn cycles_and_attached_forests_cover_all_read_surfaces_and_reopened_history() {
    let ((),report)=run_async_under_lab(0xc1c1_0001,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let vfs=MemVfs::new().unwrap();let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await;let pinned=db.read_session().unwrap();
        let query=prepare(STATEMENT);let frozen=query.canonical_bytes();
        let old=oracle(&db.edges().unwrap());
        assert_eq!(old,vec![(VId(0),VId(10),32),(VId(1),VId(11),1)]);
        let mut txn=db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap()] {
            assert_eq!(plain(&result.value),old);assert_eq!(result.rows.snapshot_records,19);
        }
        let measured=db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap();
        let exact=GqlQueryPolicy::new(19,2,measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx,&query,exact).unwrap(),measured);
        for cap in [GqlQueryPolicy::new(19,1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(19,2,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(19,2,u64::MAX,measured.evaluator.scratch_entries-1)] {
            assert!(db.execute_graph_aggregate_governed(&cx,&query,cap).is_err());
        }
        let mut changes=WriteBatch::new(S);
        changes.delete_edge(EId(10));changes.ensure_edge_by_triple(EId(999),VId(10),VId(20),vec![]);
        changes.delete_vertex(VId(51));txn.write(&mut db,changes).unwrap();
        assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected=oracle(&txn.edges(&db).unwrap());assert_eq!(expected,vec![(VId(0),VId(10),10)]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap().value),expected);
        txn.commit(&mut db,&commit).await.unwrap();db.compact(&commit).await.unwrap();drop(db);
        let reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap().value),old);
        assert_eq!(plain(&pinned.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value),old);
        assert_eq!(query.canonical_bytes(),frozen);
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn eliminated_cycles_keep_missing_closing_edges_and_existing_parallel_dependencies() {
    let ((),report)=run_async_under_lab(0xc1c1_0002,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        for mode in 0..3 {for change in 0..4 {
            let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
            let mut txn=db.begin(&txn_cx).unwrap();
            let mut staged=WriteBatch::new(R);staged.create_vertex(VId(777),vec![],vec![]);txn.write(&mut db,staged).unwrap();
            let query=prepare(&format!("{STATEMENT}{}",if mode==2 {" LIMIT 0"} else {""}));
            let result=txn.execute_graph_aggregate_governed(&db,&cx,&query,
                GqlQueryPolicy::new(1000,if mode==1 {0} else {100},4_000_000,1_000_000));
            match mode {0=>assert_eq!(result.unwrap().value.len(),2),
                1=>assert!(matches!(result,Err(GqlQueryError::Rows(_)))),_=>assert!(result.unwrap().value.is_empty()),}
            let mut winner=WriteBatch::new(match change {2=>T,3=>V,_=>U});
            match change {
                0=>{winner.create_vertex(VId(888),vec![],vec![]);}
                1=>{winner.add_edge(EId(90),VId(32),VId(10),vec![]);}
                2=>{winner.delete_edge(EId(20));}
                _=>{winner.delete_edge(EId(42));}
            }
            db.write(&commit,winner).await.unwrap();let frontier=db.frontier().unwrap();
            // No subsequent transaction read may repair an earlier lost witness.
            let result=txn.commit(&mut db,&commit).await;
            if change==0 {result.unwrap();assert!(db.vertex(VId(777)).unwrap().is_some());}
            else {assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins {law:"FG-LAW-FCW-READ-01",..}))));
                assert_eq!(db.frontier().unwrap(),frontier);assert!(db.vertex(VId(777)).unwrap().is_none());}
        }}
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn database_counts_eight_hidden_cycles_with_original_source_admission() {
    let ((),report)=run_async_under_lab(0xc1c1_0003,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut vertices=WriteBatch::new(RelationId(99));for id in 0..18 {vertices.create_vertex(VId(id),vec![],vec![]);}
        let mut first=WriteBatch::new(R);first.add_edge(EId(1),VId(0),VId(1),vec![]);
        let mut second=WriteBatch::new(S);let mut middle=WriteBatch::new(T);let mut last=WriteBatch::new(U);let mut eid=10;
        for a in 2..10 {second.add_edge(EId(eid),VId(1),VId(a),vec![]);eid+=1;
            for b in 10..18 {middle.add_edge(EId(eid),VId(a),VId(b),vec![]);eid+=1;}}
        for b in 10..18 {last.add_edge(EId(eid),VId(b),VId(1),vec![]);eid+=1;}
        db.write_atomic(&commit,vec![vertices,first,second,middle,last]).await.unwrap();
        let mut statement="MATCH (a)-[:R]->(b)".to_owned();
        for at in 0..8 {statement.push_str(&format!(",(b)-[:S]->(x{at})-[:T]->(y{at})-[:U]->(b)"));}
        statement.push_str(" RETURN COUNT(*) AS n,COUNT(DISTINCT b) AS d");let query=prepare(&statement);
        let result=db.execute_graph_aggregate_governed(&cx,&query,GqlQueryPolicy::new(81,1,1_000_000,100_000)).unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(64_u64.pow(8)));
        assert_eq!(result.value[0].get(1).unwrap().as_count(),Some(1));assert_eq!(result.rows.snapshot_records,81);
        assert!(matches!(db.execute_graph_aggregate_governed(&cx,&query,GqlQueryPolicy::new(80,1,u64::MAX,u64::MAX)),Err(GqlQueryError::Rows(_))));
    });assert!(report.lab_test_passed(),"{report:?}");
}
