//! Independent probes run through the same pinned source and native dispatcher.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(7);
const Q: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const L: LabelId = LabelId(10);
const IDS: [VId; 6] = [VId(0), VId(1), VId(17), VId(1_u128 << 100), VId(u128::MAX), VId(101)];
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x51;32], DatabaseSecurityNamespaceId([0x62;32]), [0x73;32]) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,10_000,10_000_000,1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind,name) {
        (GraphSymbolKind::Relation,"R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation,"Q") => Some(GraphSymbol::Relation(Q)),
        (GraphSymbolKind::Property,"p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label,"L") => Some(GraphSymbol::Label(L)), _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text,symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> { rows.iter().map(|r|r.values().to_vec()).collect() }
fn statement(edge: bool, direction: GlaDirection, anti: bool, shape: usize, cut: Option<CommitSeq>) -> String {
    let pattern = match direction { GlaDirection::Forward => "(x)-[:Q]->(y)",
        GlaDirection::Reverse => "(x)<-[:Q]-(y)", GlaDirection::Undirected => "(x)-[:Q]-(y)" };
    let body = match shape {
        0 => format!("MATCH {pattern} WHERE y.p > 5"),
        1 => format!("MATCH {pattern} WHERE x.p = a.p"),
        _ => "MATCH (x:L)".to_owned(),
    };
    let time = cut.map(|s|format!(" FOR SYSTEM_TIME AS OF SEQ {}",s.0)).unwrap_or_default();
    format!("MATCH {}{time} WHERE {}EXISTS {{ {body} }} RETURN {}",
        if edge { "(a)-[r:R]->(b)" } else { "(a)" }, if anti { "NOT " } else { "" },
        if edge { "r, a, b" } else { "a" })
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut roots=WriteBatch::new(R);
    for (i,&id) in IDS[..5].iter().enumerate() {
        let value=match i { 0|1 => CanonicalScalar::Int(1), 2 => CanonicalScalar::Null,
            3 => CanonicalScalar::Int(7), _ => CanonicalScalar::Int(-2) };
        roots.create_vertex(id,if i==4 {vec![L]} else {vec![]},vec![(P,value)]);
    }
    roots.add_edge(EId(0),IDS[0],IDS[1],vec![]);
    roots.add_edge(EId(1),IDS[0],IDS[1],vec![]);
    roots.add_edge(EId(2),IDS[1],IDS[2],vec![]);
    let mut probes=WriteBatch::new(Q);
    probes.add_edge(EId(10),IDS[0],IDS[3],vec![]);
    probes.add_edge(EId(11),IDS[0],IDS[3],vec![]);
    probes.add_edge(EId(12),IDS[1],IDS[2],vec![]);
    db.write_atomic(cx,vec![roots,probes]).await.unwrap()
}
fn property(db: &Database<MemVfs>, cut: CommitSeq, id: VId) -> CanonicalScalar {
    db.vertex_at(id,cut).unwrap().unwrap().props.iter().find(|(k,_)|*k==P)
        .map(|(_,v)|v.clone()).unwrap_or(CanonicalScalar::Null)
}
// Enumerate complete inner relations before reducing to existence. Local
// labels, NULL comparisons and stored orientations are independent of Probe.
fn oracle(db: &Database<MemVfs>, cut: CommitSeq, edge: bool, d: GlaDirection, anti: bool, shape: usize) -> Vec<Vec<GraphValue>> {
    let edges=db.edges_at(cut).unwrap();
    let relation:Vec<_>=edges.iter().filter(|e|e.entry.relation==Q).flat_map(|e| {
        let (a,b)=(e.entry.src,e.entry.dst);
        match d { GlaDirection::Forward=>vec![(a,b)],GlaDirection::Reverse=>vec![(b,a)],
            GlaDirection::Undirected if a==b=>vec![(a,b)],_=>vec![(a,b),(b,a)] }
    }).collect();
    let witnesses=|a| {
        if shape==2 {
            return IDS.iter().filter(|&&id|db.vertex_at(id,cut).unwrap().is_some_and(|v|v.labels.contains(&L))).count();
        }
        relation.iter().filter(|&&(x,y)| if shape==0 { matches!(property(db,cut,y),CanonicalScalar::Int(n) if n>5) }
            else { matches!((property(db,cut,x),property(db,cut,a)),(CanonicalScalar::Int(x),CanonicalScalar::Int(a)) if x==a) }).count()
    };
    let mut rows:Vec<_>=if edge {
        edges.iter().filter(|e|e.entry.relation==R && (witnesses(e.entry.src)>0)!=anti)
            .map(|e|vec![GraphValue::Edge(e.entry.eid),GraphValue::Vertex(e.entry.src),GraphValue::Vertex(e.entry.dst)]).collect()
    } else {
        IDS.iter().filter(|&&a|db.vertex_at(a,cut).unwrap().is_some() && (witnesses(a)>0)!=anti)
            .map(|&a|vec![GraphValue::Vertex(a)]).collect()
    };
    rows.sort(); rows
}

#[test]
fn independent_native_queries_preserve_history_captures_isolates_and_paused_pins() {
    let ((),report)=run_async_under_lab(0x696e_6401,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let vfs=MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await; let view=db.read_session().unwrap(); let args=GqlParameters::new();
        let mut paused=Vec::new();
        for edge in [false,true] {
            let text=statement(edge,GlaDirection::Forward,false,0,None);
            let native=PreparedNativeRead::prepare(&text,&args,symbols).unwrap();
            let (_,mut cursor)=native.stream(&db,&cx,&args,wide()).unwrap();
            let expected=oracle(&db,basis,edge,GlaDirection::Forward,false,0);
            let first=cursor.next().unwrap().unwrap(); paused.push((text,expected,first,cursor));
        }
        let mut edit=WriteBatch::new(Q); edit.delete_edge(EId(10));
        let one=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(Q); edit.delete_edge(EId(11));
        let none=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.set_vertex_property(IDS[2],P,Some(CanonicalScalar::Int(8)));
        let changed=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.delete_vertex(IDS[2]);
        let cascaded=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(Q); edit.delete_vertex(IDS[4]);
        let no_isolate=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.create_vertex(IDS[5],vec![L],vec![]);
        let new_isolate=db.write(&commit,edit).await.unwrap();
        let mut cases=Vec::new();
        for cut in [CommitSeq(0),basis,one,none,changed,cascaded,no_isolate,new_isolate] {
            for edge in [false,true] { for d in [GlaDirection::Forward,GlaDirection::Reverse,GlaDirection::Undirected] {
                for anti in [false,true] { for shape in 0..3 {
                    let expected=oracle(&db,cut,edge,d,anti,shape); let q=prepare(&statement(edge,d,anti,shape,None));
                    let eager=db.execute_graph_pattern_governed_at(&cx,&q,cut,wide()).unwrap().value;
                    assert_eq!(plain(&eager),expected);
                    let (actual,r,e)=if edge {
                        let mut c=db.stream_graph_edges_governed_at(&cx,&q,cut,wide()).unwrap();
                        (c.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),c.row_stats(),c.evaluator_stats())
                    } else {
                        let mut c=db.stream_graph_values_governed_at(&cx,&q,cut,wide()).unwrap();
                        (c.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),c.row_stats(),c.evaluator_stats())
                    };
                    assert_eq!(plain(&actual),expected);
                    let input=statement(edge,d,anti,shape,Some(cut));
                    let native=PreparedNativeRead::prepare(&input,&args,symbols).unwrap();
                    let (_,mut c)=native.stream(&db,&cx,&args,wide()).unwrap();
                    assert_eq!(c.snapshot_seq(),cut); assert_eq!(plain(&c.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),expected);
                    assert_eq!((c.row_stats(),c.evaluator_stats()),(r,e)); cases.push((input,expected));
                } }
            } }
        }
        db.compact(&commit).await.unwrap(); drop(db);
        let db=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        for (input,expected,first,mut c) in paused {
            let mut rows=vec![first]; rows.extend(c.by_ref().map(Result::unwrap)); assert_eq!(plain(&rows),expected);
            let native=PreparedNativeRead::prepare(&input,&args,symbols).unwrap();
            let (_,mut c)=native.stream_in_view(&view,&cx,&args,wide()).unwrap();
            assert_eq!(plain(&c.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),expected);
        }
        for (input,expected) in cases {
            let native=PreparedNativeRead::prepare(&input,&args,symbols).unwrap();
            let (_,mut c)=native.stream(&db,&cx,&args,wide()).unwrap();
            assert_eq!(plain(&c.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),expected);
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn first_independent_witness_uses_three_candidates_despite_unrelated_topology() {
    let ((),report)=run_async_under_lab(0x696e_6402,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();let mut roots=WriteBatch::new(R);
        for id in [0,1,2,3,100,101] { roots.create_vertex(VId(id),vec![],vec![(P,CanonicalScalar::Int(7))]); }
        roots.add_edge(EId(0),VId(1),VId(2),vec![]);
        let mut probes=WriteBatch::new(Q);probes.add_edge(EId(1),VId(0),VId(3),vec![]);
        for id in 2..2050 { probes.add_edge(EId(id),VId(100),VId(101),vec![]); }
        db.write_atomic(&commit,vec![roots,probes]).await.unwrap();let args=GqlParameters::new();
        for edge in [false,true] {
            let input=statement(edge,GlaDirection::Forward,false,0,None)+" LIMIT 1";
            let native=PreparedNativeRead::prepare(&input,&args,symbols).unwrap();
            let (_,mut c)=native.stream(&db,&cx,&args,GqlQueryPolicy::new(3,1,10_000,10_000)).unwrap();
            assert!(c.next().unwrap().is_ok());assert_eq!(c.row_stats().snapshot_records,3);assert!(c.next().is_none());
            let negative=statement(edge,GlaDirection::Forward,true,0,None).replace("> 5","> 99")+" LIMIT 1";
            let native=PreparedNativeRead::prepare(&negative,&args,symbols).unwrap();
            let (_,mut c)=native.stream(&db,&cx,&args,GqlQueryPolicy::new(2,1,10_000,10_000)).unwrap();
            assert!(c.next().unwrap().is_err());assert_eq!(c.row_stats().result_rows,0);assert!(c.next().is_none());
            let (_,mut c)=native.stream(&db,&cx,&args,wide()).unwrap();assert!(c.next().unwrap().is_ok());
            let zero=statement(edge,GlaDirection::Forward,false,0,None)+" LIMIT 0";
            let native=PreparedNativeRead::prepare(&zero,&args,symbols).unwrap();
            let (_,mut c)=native.stream(&db,&cx,&args,GqlQueryPolicy::new(0,0,1,0)).unwrap();
            assert!(c.next().is_none());assert_eq!(c.row_stats().snapshot_records,0);
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn independent_scope_parameters_and_budgets_do_not_reset_or_leak_local_bindings() {
    let ((),report)=run_async_under_lab(0x696e_6403,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();let basis=seed(&mut db,&commit).await;
        let args=GqlParameters::new();
        for input in [
            "MATCH (a) WHERE EXISTS { MATCH (x:L) } AND NOT EXISTS { MATCH (a)-[:Q]->(x) WHERE x.p = 99 } RETURN a",
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (x:L) } AND NOT EXISTS { MATCH (x) WHERE x.p = 99 } RETURN r, a, b",
        ] {
            let q=prepare(input);let expected=db.execute_graph_pattern_governed(&cx,&q,wide()).unwrap().value;
            let native=PreparedNativeRead::prepare(input,&args,symbols).unwrap();let (_,mut c)=native.stream(&db,&cx,&args,wide()).unwrap();
            assert_eq!(c.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),expected);assert!(!expected.is_empty());
            let r=c.row_stats();let e=c.evaluator_stats();let exact=GqlQueryPolicy::new(r.snapshot_records,r.result_rows,e.work_units,e.scratch_entries);
            let (_,mut retry)=native.stream(&db,&cx,&args,exact).unwrap();
            assert_eq!(retry.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),expected);assert_eq!((retry.row_stats(),retry.evaluator_stats()),(r,e));
            for p in [GqlQueryPolicy::new(r.snapshot_records-1,10_000,u64::MAX,u64::MAX),
                GqlQueryPolicy::new(100_000,r.result_rows-1,u64::MAX,u64::MAX),
                GqlQueryPolicy::new(100_000,10_000,e.work_units-1,u64::MAX),GqlQueryPolicy::new(100_000,10_000,u64::MAX,e.scratch_entries-1)] {
                let (_,mut failed)=native.stream(&db,&cx,&args,p).unwrap();let mut prefix=Vec::new();
                loop { match failed.next() { Some(Ok(row))=>prefix.push(row),Some(Err(_))=>break,None=>panic!("quota became EOF") } }
                assert_eq!(prefix,expected[..prefix.len()]);assert_eq!(failed.row_stats().result_rows,prefix.len() as u64);assert!(failed.next().is_none());
            }
        }
        let input=statement(false,GlaDirection::Forward,false,0,None).replace("> 5","> $floor");
        let low=GqlParameters::new().with_int64("floor",5).unwrap();let high=GqlParameters::new().with_int64("floor",99).unwrap();
        let native=PreparedNativeRead::prepare(&input,&low,symbols).unwrap();
        assert!(matches!(native.stream(&db,&cx,&args,wide()),Err(QueryError::PatternText(_))));
        let (_,mut low)=native.stream(&db,&cx,&low,wide()).unwrap();let (_,mut high)=native.stream(&db,&cx,&high,wide()).unwrap();
        assert!(low.next().unwrap().is_ok());assert!(high.next().is_none());
        let future=statement(false,GlaDirection::Forward,false,0,Some(CommitSeq(basis.0+1)))+" LIMIT 0";
        let native=PreparedNativeRead::prepare(&future,&args,symbols).unwrap();assert!(native.stream(&db,&cx,&args,GqlQueryPolicy::new(0,0,0,0)).is_err());
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
