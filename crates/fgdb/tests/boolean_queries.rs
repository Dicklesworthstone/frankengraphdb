//! Compound predicates use canonical sources and retain rejected observations.
//! The oracle reads owned records and implements its own three-valued condition.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database,DatabaseKeys,EdgeRecord,GqlError,MemVfs,ReadError,VertexRow,WriteBatch,WriteError,WriteTxnError};
use fgdb_delta_types::{LabelId,PropertyKeyId,RelationId};
use fgdb_gql::algebra::{GraphValueRow,PreparedGraphPattern};
use fgdb_gql::{GqlParameters,GqlQueryError,GqlQueryPolicy,GraphAggregateRow,GraphSymbol,GraphSymbolKind,
    PreparedGraphAggregate,PreparedGraphAggregateText,PreparedGraphText};
use fgdb_types::{CanonicalScalar,CommitCx,CommitSeq,DatabaseSecurityNamespaceId,EId,PurposeContexts,VId};
use std::cmp::Ordering;
use std::collections::BTreeMap;

const OWNER:LabelId=LabelId(1);
const R:RelationId=RelationId(1);
const N:PropertyKeyId=PropertyKeyId(1);
const FLAG:PropertyKeyId=PropertyKeyId(2);
const HEAD:&str="MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) \
    WHERE (b.n>a.n OR b.n IS NULL) AND NOT (a.flag=FALSE)";
type Plain=(VId,Option<VId>,Option<i64>);
type Summary=(VId,u64,u64);
fn keys()->DatabaseKeys{DatabaseKeys::new([0xd1;32],DatabaseSecurityNamespaceId([0xd2;32]),[0xd3;32])}
fn wide()->GqlQueryPolicy{GqlQueryPolicy::new(1000,1000,2_000_000,1_000_000)}
fn symbols(kind:GraphSymbolKind,name:&str)->Option<GraphSymbol>{match (kind,name){
    (GraphSymbolKind::Label,"Owner")=>Some(GraphSymbol::Label(OWNER)),
    (GraphSymbolKind::Relation,"R")=>Some(GraphSymbol::Relation(R)),
    (GraphSymbolKind::Property,"n")=>Some(GraphSymbol::Property(N)),
    (GraphSymbolKind::Property,"flag")=>Some(GraphSymbol::Property(FLAG)),_=>None,
}}
fn pattern(count:Option<u64>)->PreparedGraphPattern<GraphValueRow>{
    PreparedGraphText::prepare(&format!("{HEAD} RETURN a,b,b.n AS n ORDER BY n DESC NULLS LAST,a,b{}",
        count.map_or(String::new(),|count|format!(" LIMIT {count}"))),symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn aggregate()->PreparedGraphAggregate{
    PreparedGraphAggregateText::prepare(&format!("{HEAD} RETURN a,COUNT(*) AS n,COUNT(b) AS present GROUP BY a ORDER BY a"),symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db:&mut Database<MemVfs>,cx:&CommitCx)->CommitSeq{
    let mut batch=WriteBatch::new(R);
    for (id,value,flag) in [(0,5,true),(1,10,true),(2,2,true),(3,5,false)]{
        batch.create_vertex(VId(id),vec![OWNER],vec![(N,CanonicalScalar::Int(value)),(FLAG,CanonicalScalar::Bool(flag))]);
    }
    batch.create_vertex(VId(40),vec![OWNER],vec![(FLAG,CanonicalScalar::Bool(true))]);
    for (id,value) in [(10,8),(11,1),(13,20)]{batch.create_vertex(VId(id),vec![],vec![(N,CanonicalScalar::Int(value))]);}
    batch.create_vertex(VId(12),vec![],vec![(N,CanonicalScalar::Null)]);
    for (id,source,destination) in [(1,0,10),(2,0,10),(3,0,11),(4,1,11),(5,2,12),(6,3,13)]{
        batch.add_edge(EId(id),VId(source),VId(destination),vec![]);
    }
    db.write(cx,batch).await.unwrap()
}
fn property(row:&VertexRow,key:PropertyKeyId)->Option<&CanonicalScalar>{row.props.iter().find(|(k,_)|*k==key).map(|(_,v)|v)}
fn and(left:Option<bool>,right:Option<bool>)->Option<bool>{match (left,right){
    (Some(false),_)|(_,Some(false))=>Some(false),(Some(true),Some(true))=>Some(true),_=>None,
}}
fn or(left:Option<bool>,right:Option<bool>)->Option<bool>{match (left,right){
    (Some(true),_)|(_,Some(true))=>Some(true),(Some(false),Some(false))=>Some(false),_=>None,
}}
fn compare(left:&Plain,right:&Plain)->Ordering{
    match (left.2,right.2){
        (Some(a),Some(b))=>b.cmp(&a),(None,Some(_))=>Ordering::Greater,(Some(_),None)=>Ordering::Less,
        _=>Ordering::Equal,
    }.then_with(||left.0.cmp(&right.0)).then_with(||left.1.cmp(&right.1))
}
fn oracle(vertices:&[VertexRow],edges:&[EdgeRecord])->Vec<Plain>{
    let mut rows=Vec::new();
    for owner in vertices.iter().filter(|row|row.labels.contains(&OWNER)){
        let before=rows.len();
        for edge in edges.iter().filter(|edge|edge.entry.relation==R && edge.entry.src==owner.vid){
            let other=vertices.iter().find(|row|row.vid==edge.entry.dst).unwrap();
            let value=property(other,N);
            let greater=match (value,property(owner,N)){
                (Some(CanonicalScalar::Int(a)),Some(CanonicalScalar::Int(b)))=>Some(a>b),_=>None,
            };
            let null=Some(value.is_none_or(|v|matches!(v,CanonicalScalar::Null)));
            let not_false=match property(owner,FLAG){Some(CanonicalScalar::Bool(value))=>Some(*value),_=>None};
            if and(or(greater,null),not_false)==Some(true){
                let value=match value{Some(CanonicalScalar::Int(value))=>Some(*value),_=>None};
                rows.push((owner.vid,Some(other.vid),value));
            }
        }
        if rows.len()==before{rows.push((owner.vid,None,None));}
    }
    rows.sort_by(compare);rows
}
fn plain(rows:&[GraphValueRow])->Vec<Plain>{rows.iter().map(|row|{
    let value=match row.get(2).unwrap().as_scalar().unwrap(){CanonicalScalar::Int(value)=>Some(*value),CanonicalScalar::Null=>None,_=>panic!("fixture scalar")};
    (row.get(0).unwrap().as_vertex().unwrap(),row.get(1).unwrap().as_vertex(),value)
}).collect()}
fn summaries(rows:&[Plain])->Vec<Summary>{
    let mut groups:BTreeMap<VId,(u64,u64)>=BTreeMap::new();
    for (owner,destination,_) in rows{let group=groups.entry(*owner).or_default();group.0+=1;group.1+=u64::from(destination.is_some());}
    groups.into_iter().map(|(owner,(rows,present))|(owner,rows,present)).collect()
}
fn summary(rows:&[GraphAggregateRow])->Vec<Summary>{rows.iter().map(|row|(row.keys()[0].as_vertex().unwrap(),
    row.get(0).unwrap().as_count().unwrap(),row.get(1).unwrap().as_count().unwrap())).collect()}

#[test]
fn boolean_rows_and_groups_cover_all_reads_staging_and_reopened_history(){
    let ((),report)=run_async_under_lab(0xb001_0001,|root|async move{
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let vfs=MemVfs::new().unwrap();let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await;let pinned=db.read_session().unwrap();let mut txn=db.begin(&txn_cx).unwrap();
        let query=pattern(None);let aggregate=aggregate();let frozen=query.canonical_bytes();
        let old=oracle(&db.vertices().unwrap(),&db.edges().unwrap());
        assert_eq!(old,vec![(VId(0),Some(VId(10)),Some(8)),(VId(0),Some(VId(10)),Some(8)),
            (VId(1),None,None),(VId(2),Some(VId(12)),None),(VId(3),None,None),(VId(40),None,None)]);
        for result in [db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap(),
            pinned.execute_graph_pattern_governed(&cx,&query,wide()).unwrap(),
            pinned.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap(),
            txn.execute_graph_pattern_governed(&db,&cx,&query,wide()).unwrap()]{assert_eq!(plain(&result.value),old);}
        for result in [db.execute_graph_aggregate_governed(&cx,&aggregate,wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx,&aggregate,basis,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx,&aggregate,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx,&aggregate,basis,wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db,&cx,&aggregate,wide()).unwrap()]{assert_eq!(summary(&result.value),summaries(&old));}
        let mut changes=WriteBatch::new(R);
        changes.delete_edge(EId(1));changes.ensure_edge_by_triple(EId(999),VId(0),VId(10),vec![]);
        changes.set_vertex_property(VId(10),N,Some(CanonicalScalar::Int(3)));
        changes.set_vertex_property(VId(11),N,Some(CanonicalScalar::Int(30)));
        changes.set_vertex_property(VId(12),N,None);
        changes.set_vertex_property(VId(3),FLAG,Some(CanonicalScalar::Bool(true)));
        changes.delete_vertex(VId(13));txn.write(&mut db,changes).unwrap();
        assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected=oracle(&txn.vertices(&db).unwrap(),&txn.edges(&db).unwrap());assert_eq!(expected.len(),5);assert_ne!(expected,old);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db,&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(summary(&txn.execute_graph_aggregate_governed(&db,&cx,&aggregate,wide()).unwrap().value),summaries(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),old);
        txn.commit(&mut db,&commit).await.unwrap();db.compact(&commit).await.unwrap();drop(db);
        let reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),expected);
        assert_eq!(summary(&reopened.execute_graph_aggregate_governed(&cx,&aggregate,wide()).unwrap().value),summaries(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx,&query,basis,wide()).unwrap().value),old);
        assert_eq!(plain(&pinned.execute_graph_pattern_governed(&cx,&query,wide()).unwrap().value),old);
        assert_eq!(query.canonical_bytes(),frozen);
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn rejected_boolean_operands_and_new_matches_remain_conflicts_after_refusal(){
    let ((),report)=run_async_under_lab(0xb001_0002,|root|async move{
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        for mode in 0..3{for change in 0..4{
            let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
            let mut txn=db.begin(&txn_cx).unwrap();let mut stage=WriteBatch::new(R);stage.create_vertex(VId(777),vec![],vec![]);txn.write(&mut db,stage).unwrap();
            let query=pattern(Some(u64::from(mode!=2)));
            let result=txn.execute_graph_pattern_governed(&db,&cx,&query,GqlQueryPolicy::new(1000,u64::from(mode==0),2_000_000,1_000_000));
            match mode{0=>assert_eq!(plain(&result.unwrap().value),vec![(VId(0),Some(VId(10)),Some(8))]),
                1=>assert!(matches!(result,Err(GqlQueryError::Rows(_)))),_=>assert!(result.unwrap().value.is_empty()),}
            let mut winner=WriteBatch::new(R);match change{
                0=>{winner.create_vertex(VId(888),vec![],vec![]);}
                1=>{winner.set_vertex_property(VId(11),N,Some(CanonicalScalar::Int(100)));}
                2=>{winner.set_vertex_property(VId(3),FLAG,Some(CanonicalScalar::Bool(true)));}
                _=>{winner.add_edge(EId(90),VId(0),VId(13),vec![]);}
            }
            db.write(&commit,winner).await.unwrap();let frontier=db.frontier().unwrap();
            // No subsequent transaction read can restore a lost operand or
            // absent-relationship witness before this immediate commit.
            let result=txn.commit(&mut db,&commit).await;
            if change==0{result.unwrap();assert!(db.vertex(VId(777)).unwrap().is_some());}
            else{assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins{law:"FG-LAW-FCW-READ-01",..}))));
                assert_eq!(db.frontier().unwrap(),frontier);assert!(db.vertex(VId(777)).unwrap().is_none());}
        }}
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn boolean_sources_share_exact_limits_and_preserve_owner_and_snapshot_errors(){
    let ((),report)=run_async_under_lab(0xb001_0003,|root|async move{
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();let basis=seed(&mut db,&commit).await;
        let query=pattern(Some(2));let measured=db.execute_graph_pattern_governed(&cx,&query,wide()).unwrap();
        let exact=GqlQueryPolicy::new(measured.rows.snapshot_records,measured.rows.result_rows,
            measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx,&query,exact).unwrap(),measured);
        for policy in [GqlQueryPolicy::new(measured.rows.snapshot_records-1,2,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(1000,1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(1000,2,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(1000,2,u64::MAX,measured.evaluator.scratch_entries-1)]{
            assert!(db.execute_graph_pattern_governed(&cx,&query,policy).is_err());
        }
        let foreign=Database::open_memory(&commit,keys()).await.unwrap();let txn=db.begin(&txn_cx).unwrap();let zero=GqlQueryPolicy::new(0,0,0,0);
        assert!(matches!(txn.execute_graph_pattern_governed(&foreign,&cx,&query,zero),Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        assert!(matches!(db.execute_graph_pattern_governed_at(&cx,&query,CommitSeq(basis.0+1),zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier{..})))));
        txn.abort();
    });assert!(report.lab_test_passed(),"{report:?}");
}
