//! Completed-group predicates preserve canonical sources and observations.
//! The oracle enumerates owned rows/edge occurrences independently of GLA.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateRow, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cmp::Ordering;

const OWNER: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const CATEGORY: PropertyKeyId = PropertyKeyId(2);
const HEAD: &str = "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) \
    RETURN a,a.category AS category,COUNT(*) AS n,COUNT(b.p) AS present,SUM(b.p) AS total \
    GROUP BY a,a.category HAVING category=$wanted AND (total>present OR total IS NULL) AND NOT (n=0) \
    ORDER BY total DESC NULLS LAST,a";
type Summary = (VId,u64,u64,Option<i128>);
fn text(value:&str)->CanonicalScalar {CanonicalScalar::ucs_basic_text(value).unwrap()}
fn keys()->DatabaseKeys {DatabaseKeys::new([0xa1;32],DatabaseSecurityNamespaceId([0xa2;32]),[0xa3;32])}
fn wide()->GqlQueryPolicy {GqlQueryPolicy::new(1000,1000,2_000_000,1_000_000)}
fn symbols(kind:GraphSymbolKind,name:&str)->Option<GraphSymbol> {match (kind,name) {
    (GraphSymbolKind::Label,"Owner")=>Some(GraphSymbol::Label(OWNER)),
    (GraphSymbolKind::Relation,"R")=>Some(GraphSymbol::Relation(R)),
    (GraphSymbolKind::Property,"p")=>Some(GraphSymbol::Property(P)),
    (GraphSymbolKind::Property,"category")=>Some(GraphSymbol::Property(CATEGORY)),_=>None,
}}
fn template(count:Option<u64>)->PreparedGraphAggregateText {
    PreparedGraphAggregateText::prepare_with_parameter_types(&format!("{HEAD}{}",
        count.map_or(String::new(),|count|format!(" LIMIT {count}"))),
        &[("wanted",GqlParameterType::Scalar(CanonicalScalarKind::Text))],symbols).unwrap()
}
fn bind(template:&PreparedGraphAggregateText,category:&str)->PreparedGraphAggregate {
    template.bind_parameters(&GqlParameters::new().with_text("wanted",category).unwrap()).unwrap()
}
async fn seed(db:&mut Database<MemVfs>,cx:&CommitCx)->CommitSeq {
    let mut batch=WriteBatch::new(R);
    for id in 0..5 {batch.create_vertex(VId(id),vec![OWNER],vec![(CATEGORY,text(if id==2 {"hold"}else{"active"}))]);}
    for (id,p) in [(10,8),(11,-4),(13,20),(14,5)] {batch.create_vertex(VId(id),vec![],vec![(P,CanonicalScalar::Int(p))]);}
    batch.create_vertex(VId(12),vec![],vec![(P,CanonicalScalar::Null)]);
    for (id,source,destination) in [(1,0,10),(2,0,10),(3,0,12),(4,1,11),(5,2,13),(6,4,14)] {
        batch.add_edge(EId(id),VId(source),VId(destination),vec![]);
    }
    db.write(cx,batch).await.unwrap()
}
fn property(row:&VertexRow,key:PropertyKeyId)->Option<&CanonicalScalar> {
    row.props.iter().find(|(k,_)|*k==key).map(|(_,value)|value)
}
fn order(left:&Summary,right:&Summary)->Ordering {
    match (left.3,right.3) {
        (Some(a),Some(b))=>b.cmp(&a),(None,Some(_))=>Ordering::Greater,
        (Some(_),None)=>Ordering::Less,(None,None)=>Ordering::Equal,
    }.then_with(||left.0.cmp(&right.0))
}
fn oracle(vertices:&[VertexRow],edges:&[EdgeRecord],wanted:&str)->Vec<Summary> {
    let wanted=text(wanted);let mut result=Vec::new();
    for owner in vertices.iter().filter(|v|v.labels.contains(&OWNER)) {
        let mut n=0;let mut present=0;let mut sum=0_i128;
        for edge in edges.iter().filter(|e|e.entry.relation==R && e.entry.src==owner.vid) {
            n+=1;let target=vertices.iter().find(|v|v.vid==edge.entry.dst).unwrap();
            match property(target,P) {
                Some(CanonicalScalar::Int(value))=>{present+=1;sum+=i128::from(*value);}
                Some(CanonicalScalar::Null)|None=>{}
                _=>panic!("fixture requires integer or null aggregate arguments"),
            }
        }
        if n==0 {n=1;}
        let total=(present!=0).then_some(sum);
        // Independent null-aware selection of already complete aggregates.
        if property(owner,CATEGORY)==Some(&wanted) && (total.is_none() || sum>i128::from(present)) && n!=0 {
            result.push((owner.vid,n,present,total));
        }
    }
    result.sort_by(order);result
}
fn plain(rows:&[GraphAggregateRow],wanted:&str)->Vec<Summary> {
    rows.iter().map(|row| {
        assert_eq!(row.keys()[1].as_scalar(),Some(&text(wanted)));
        (row.keys()[0].as_vertex().unwrap(),row.get(0).unwrap().as_count().unwrap(),
            row.get(1).unwrap().as_count().unwrap(),row.get(2).unwrap().as_integer())
    }).collect()
}

#[test]
fn having_covers_all_five_reads_canonical_staging_and_reopened_history() {
    let ((),report)=run_async_under_lab(0x4841_0001,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        let vfs=MemVfs::new().unwrap();let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await;let pinned=db.read_session().unwrap();
        let prepared=template(None);let query=bind(&prepared,"active");let frozen=query.canonical_bytes();
        let old=oracle(&db.vertices().unwrap(),&db.edges().unwrap(),"active");
        assert_eq!(old,vec![(VId(0),3,2,Some(16)),(VId(4),1,1,Some(5)),(VId(3),1,0,None)]);
        let mut txn=db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap()] {
            assert_eq!(plain(&result.value,"active"),old);
        }
        let hold=bind(&prepared,"hold");
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx,&hold,wide()).unwrap().value,"hold"),vec![(VId(2),1,1,Some(20))]);
        let mut changes=WriteBatch::new(R);
        changes.delete_edge(EId(1));changes.ensure_edge_by_triple(EId(999),VId(0),VId(10),vec![]);
        changes.set_vertex_property(VId(10),P,Some(CanonicalScalar::Int(1)));
        changes.set_vertex_property(VId(11),P,Some(CanonicalScalar::Int(30)));
        changes.set_vertex_property(VId(14),P,None);changes.delete_vertex(VId(12));
        txn.write(&mut db,changes).unwrap();assert!(txn.edge(&db,EId(999)).unwrap().is_none());
        let expected=oracle(&txn.vertices(&db).unwrap(),&txn.edges(&db).unwrap(),"active");
        assert_eq!(expected,vec![(VId(1),1,1,Some(30)),(VId(3),1,0,None),(VId(4),1,0,None)]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db,&cx,&query,wide()).unwrap().value,"active"),expected);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value,"active"),old);
        txn.commit(&mut db,&commit).await.unwrap();db.compact(&commit).await.unwrap();drop(db);
        let reopened=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value,"active"),expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx,&query,basis,wide()).unwrap().value,"active"),old);
        assert_eq!(plain(&pinned.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap().value,"active"),old);
        assert_eq!(query.canonical_bytes(),frozen);
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn rejected_having_groups_keep_property_category_and_phantom_dependencies_after_refusal() {
    let ((),report)=run_async_under_lab(0x4841_0002,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();let txn_cx=contexts.txn();
        for mode in 0..3 {for change in 0..5 {
            let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
            let mut txn=db.begin(&txn_cx).unwrap();let mut stage=WriteBatch::new(R);
            stage.create_vertex(VId(777),vec![],vec![]);txn.write(&mut db,stage).unwrap();
            let query=bind(&template(Some(u64::from(mode!=2))),"active");
            let result=txn.execute_graph_aggregate_governed(&db,&cx,&query,
                GqlQueryPolicy::new(1000,u64::from(mode==0),2_000_000,1_000_000));
            match mode {
                0=>assert_eq!(plain(&result.unwrap().value,"active"),vec![(VId(0),3,2,Some(16))]),
                1=>assert!(matches!(result,Err(GqlQueryError::Rows(_)))),_=>assert!(result.unwrap().value.is_empty()),
            }
            let mut winner=WriteBatch::new(R);
            match change {
                0=>{winner.create_vertex(VId(888),vec![],vec![]);}
                1=>{winner.set_vertex_property(VId(11),P,Some(CanonicalScalar::Int(100)));}
                2=>{winner.set_vertex_property(VId(2),CATEGORY,Some(text("active")));}
                3=>{winner.add_edge(EId(90),VId(3),VId(13),vec![]);}
                _=>{winner.delete_edge(EId(2));}
            }
            db.write(&commit,winner).await.unwrap();let frontier=db.frontier().unwrap();
            // Immediate commit: no subsequent transaction read may restore a
            // group/property/absence witness lost by HAVING or output refusal.
            let result=txn.commit(&mut db,&commit).await;
            if change==0 {result.unwrap();assert!(db.vertex(VId(777)).unwrap().is_some());}
            else {
                assert!(matches!(result,Err(WriteTxnError::Write(WriteError::FirstCommitterWins {law:"FG-LAW-FCW-READ-01",..}))));
                assert_eq!(db.frontier().unwrap(),frontier);assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}
    });assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn having_shares_source_and_result_allowances_and_keeps_empty_global_semantics() {
    let ((),report)=run_async_under_lab(0x4841_0003,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();seed(&mut db,&commit).await;
        let query=bind(&template(Some(2)),"active");let measured=db.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap();
        assert_eq!(plain(&measured.value,"active"),vec![(VId(0),3,2,Some(16)),(VId(4),1,1,Some(5))]);
        let exact=GqlQueryPolicy::new(measured.rows.snapshot_records,2,measured.evaluator.work_units,measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx,&query,exact).unwrap(),measured);
        for policy in [GqlQueryPolicy::new(measured.rows.snapshot_records-1,2,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(1000,1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(1000,2,measured.evaluator.work_units-1,u64::MAX),
            GqlQueryPolicy::new(1000,2,u64::MAX,measured.evaluator.scratch_entries-1)] {
            assert!(db.execute_graph_aggregate_governed(&cx,&query,policy).is_err());
        }
        let zero=bind(&template(Some(0)),"active");
        let empty=db.execute_graph_aggregate_governed(&cx,&zero,wide()).unwrap();
        assert!(empty.value.is_empty());assert_eq!(empty.rows.snapshot_records,measured.rows.snapshot_records);
        let blank=Database::open_memory(&commit,keys()).await.unwrap();
        for (condition,count) in [("n=0 AND (total IS NULL OR total>n)",1),("NOT (total=0)",0)] {
            let query=PreparedGraphAggregateText::prepare(&format!("MATCH (a:Owner) RETURN COUNT(*) AS n,SUM(a.p) AS total HAVING {condition}"),symbols)
                .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let result=blank.execute_graph_aggregate_governed(&cx,&query,wide()).unwrap();assert_eq!(result.value.len(),count);
            if count==1 {assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(0));assert!(result.value[0].get(1).unwrap().is_null());}
        }
    });assert!(report.lab_test_passed(),"{report:?}");
}
