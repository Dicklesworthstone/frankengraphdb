//! Ordered pages retain exact snapshot results and unreturned observations.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database,DatabaseKeys,EdgeRecord,MemVfs,VertexRow,WriteBatch,WriteError,WriteTxnError};
use fgdb_delta_types::{LabelId,PropertyKeyId,RelationId};
use fgdb_gql::algebra::{GraphValueRow,PreparedGraphPattern};
use fgdb_gql::{GqlParameters,GqlQueryError,GqlQueryPolicy,GraphSymbol,GraphSymbolKind,PreparedGraphText};
use fgdb_types::{CanonicalScalar,CommitCx,CommitSeq,DatabaseSecurityNamespaceId,EId,PurposeContexts,VId};
use std::cmp::Ordering;

const R:RelationId=RelationId(1);
const OWNER:LabelId=LabelId(1);
const RANK:PropertyKeyId=PropertyKeyId(1);
type Plain=(VId,Option<i64>,Option<VId>);
fn keys()->DatabaseKeys {
    DatabaseKeys::new([0xd1;32],DatabaseSecurityNamespaceId([0xd2;32]),[0xd3;32])
}
fn wide()->GqlQueryPolicy {GqlQueryPolicy::new(1000,1000,2_000_000,100_000)}
fn symbols(kind:GraphSymbolKind,name:&str)->Option<GraphSymbol> {
    match (kind,name) {
        (GraphSymbolKind::Relation,"R")=>Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label,"Owner")=>Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property,"rank")=>Some(GraphSymbol::Property(RANK)),
        _=>None,
    }
}
fn query(distinct:bool,offset:u64,count:u64)->PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!(
        "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) RETURN {} a,b.rank AS rank,b \
        ORDER BY rank DESC NULLS LAST,a ASC SKIP $off LIMIT $take",
        if distinct {"DISTINCT"} else {"ALL"}),symbols).unwrap()
        .bind_parameters(&GqlParameters::new().with_uint64("off",offset).unwrap().with_uint64("take",count).unwrap()).unwrap()
}
async fn seed(db:&mut Database<MemVfs>,cx:&CommitCx)->CommitSeq {
    let mut batch=WriteBatch::new(R);
    for id in [0,1] {batch.create_vertex(VId(id),vec![OWNER],vec![]);}
    for (id,value) in [(10,Some(30)),(11,Some(20)),(12,None)] {
        batch.create_vertex(VId(id),vec![],value.map(|n|(RANK,CanonicalScalar::Int(n))).into_iter().collect());
    }
    for (eid,destination) in [(10,10),(11,10),(12,11),(13,12)] {
        batch.add_edge(EId(eid),VId(0),VId(destination),vec![]);
    }
    db.write(cx,batch).await.unwrap()
}
fn plain(rows:&[GraphValueRow])->Vec<Plain> {
    rows.iter().map(|row| (
        row.get(0).unwrap().as_vertex().unwrap(),
        match row.get(1).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value)=>Some(*value),CanonicalScalar::Null=>None,
            _=>panic!("unexpected fixture scalar"),
        },row.get(2).unwrap().as_vertex(),
    )).collect()
}
fn oracle(vertices:&[VertexRow],edges:&[EdgeRecord],distinct:bool,offset:u64,count:u64)->Vec<Plain> {
    let mut rows=Vec::new();
    for owner in vertices.iter().filter(|v|v.labels.contains(&OWNER)) {
        let before=rows.len();
        for edge in edges.iter().filter(|e|e.entry.relation==R && e.entry.src==owner.vid) {
            let destination=vertices.iter().find(|v|v.vid==edge.entry.dst).unwrap();
            let rank=destination.props.iter().find_map(|(key,value)|match value {
                CanonicalScalar::Int(value) if *key==RANK=>Some(*value),_=>None,
            });
            rows.push((owner.vid,rank,Some(destination.vid)));
        }
        if rows.len()==before {rows.push((owner.vid,None,None));}
    }
    rows.sort_by(|a,b| {
        let rank=match (a.1,b.1) {
            (None,None)=>Ordering::Equal,(None,Some(_))=>Ordering::Greater,
            (Some(_),None)=>Ordering::Less,(Some(a),Some(b))=>b.cmp(&a),
        };
        rank.then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2))
    });
    if distinct {rows.dedup();}
    rows.into_iter().skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(count).unwrap_or(usize::MAX)).collect()
}

#[test]
fn ordered_optional_pages_cover_all_five_read_entrypoints_and_canonical_staging() {
    let ((),report)=run_async_under_lab(0x0d3e_0001,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let vfs=MemVfs::new().unwrap();let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await;let pinned=db.read_session().unwrap();
        let vertices=db.vertices().unwrap();let edges=db.edges().unwrap();
        let mut txn=db.begin(&txn_cx).unwrap();
        for distinct in [false,true] {for (offset,count) in [(0,0),(0,1),(1,3),(3,2),(u64::MAX,1),(1,u64::MAX)] {
            let query=query(distinct,offset,count);let expected=oracle(&vertices,&edges,distinct,offset,count);
            for result in [db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap(),
                db.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap(),
                pinned.execute_graph_pattern_governed(&cx,&query,wide()).unwrap(),
                pinned.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap(),
                txn.execute_graph_pattern_governed(&db,&cx,&query,wide()).unwrap()] {
                assert_eq!(plain(&result.value),expected);
            }
        }}
        let query=query(false,1,3);let frozen=query.canonical_bytes();
        let old=oracle(&vertices,&edges,false,1,3);
        let mut changes=WriteBatch::new(R);changes.delete_edge(EId(10));
        changes.ensure_edge_by_triple(EId(999),VId(0),VId(10),vec![]);
        changes.set_vertex_property(VId(10),RANK,Some(CanonicalScalar::Int(-1)));
        changes.set_vertex_property(VId(11),RANK,None);changes.delete_vertex(VId(12));
        changes.create_vertex(VId(13),vec![],vec![(RANK,CanonicalScalar::Int(90))]);
        changes.add_edge(EId(14),VId(0),VId(13),vec![]);txn.write(&mut db,changes).unwrap();
        assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected=oracle(&txn.vertices(&db).unwrap(),&txn.edges(&db).unwrap(),false,1,3);
        assert_eq!(expected,vec![(VId(0),Some(-1),Some(VId(10))),(VId(0),None,Some(VId(11))),(VId(1),None,None)]);
        assert_ne!(expected,old);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db,&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),old);
        txn.commit(&mut db,&commit).await.unwrap();db.compact(&commit).await.unwrap();drop(db);
        let reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap().value),old);
        assert_eq!(plain(&pinned.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),old);
        assert_eq!(query.canonical_bytes(),frozen);
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn unreturned_ordering_values_and_absent_relationships_remain_conflict_dependencies() {
    let ((),report)=run_async_under_lab(0x0d3e_0002,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        for mode in 0..3 {for change in 0..3 {
            let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
            let mut txn=db.begin(&txn_cx).unwrap();
            let mut stage=WriteBatch::new(R);stage.create_vertex(VId(777),vec![],vec![]);txn.write(&mut db,stage).unwrap();
            let query=query(false,0,u64::from(mode!=2));
            let result=txn.execute_graph_pattern_governed(&db,&cx,&query,
                GqlQueryPolicy::new(100,if mode==1 {0} else {1},1_000_000,100_000));
            match mode {
                0=>assert_eq!(plain(&result.unwrap().value),vec![(VId(0),Some(30),Some(VId(10)))]),
                1=>assert!(matches!(result,Err(GqlQueryError::Rows(_)))),
                _=>assert!(result.unwrap().value.is_empty()),
            }
            let mut winner=WriteBatch::new(R);
            match change {
                0=>{winner.create_vertex(VId(888),vec![],vec![]);}
                1=>{winner.set_vertex_property(VId(11),RANK,Some(CanonicalScalar::Int(40)));}
                _=>{
                    winner.create_vertex(VId(13),vec![],vec![(RANK,CanonicalScalar::Int(100))]);
                    winner.add_edge(EId(14),VId(1),VId(13),vec![]);
                }
            }
            db.write(&commit,winner).await.unwrap();let frontier=db.frontier().unwrap();
            // Commit immediately: a second read could repair a dependency lost
            // while retaining just the first result, refusing it, or LIMIT 0.
            let result=txn.commit(&mut db,&commit).await;
            if change==0 {result.unwrap();assert!(db.vertex(VId(777)).unwrap().is_some());}
            else {
                assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law:"FG-LAW-FCW-READ-01",..
                }))));
                assert_eq!(db.frontier().unwrap(),frontier);assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn ordered_database_pages_share_the_existing_exact_resource_policy() {
    let ((),report)=run_async_under_lab(0x0d3e_0003,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
        let query=query(false,1,3);
        let measured=db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap();
        let exact=GqlQueryPolicy::new(measured.rows.snapshot_records,measured.rows.result_rows,
            measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx,&query,exact).unwrap(),measured);
        for cap in [GqlQueryPolicy::new(100,2,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(100,3,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(100,3,u64::MAX,measured.evaluator.scratch_entries-1)] {
            assert!(db.execute_graph_pattern_governed(&cx,&query,cap).is_err());
        }
        let txn=db.begin(&txn_cx).unwrap();
        let own=txn.execute_graph_pattern_governed(&db,&cx,&query,wide()).unwrap();
        let exact=GqlQueryPolicy::new(own.rows.snapshot_records,own.rows.result_rows,
            own.evaluator.work_units,own.evaluator.scratch_entries);
        assert_eq!(txn.execute_graph_pattern_governed(&db,&cx,&query,exact).unwrap(),own);
        txn.abort();
    });assert!(report.lab_test_passed(),"{report:?}");
}
