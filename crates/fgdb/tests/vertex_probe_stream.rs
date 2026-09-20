//! Real vertex-rooted native streams, including isolates and historical absence.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::stream::{VertexScanError, VertexScanState};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(7);
const Q: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const L: LabelId = LabelId(10);
const IDS: [VId; 6] = [VId(0), VId(17), VId(1_u128 << 100), VId(u128::MAX), VId(99), VId(101)];
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x41;32], DatabaseSecurityNamespaceId([0x52;32]), [0x63;32]) }
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 10_000, 5_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Q") => Some(GraphSymbol::Relation(Q)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)), _ => None,
    }
}
fn prepare(input: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(input, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn atom(target: &str, d: GlaDirection) -> String {
    match d { GlaDirection::Forward => format!("-[:R]->({target})"),
        GlaDirection::Reverse => format!("<-[:R]-({target})"),
        GlaDirection::Undirected => format!("-[:R]-({target})") }
}
fn statement(d: GlaDirection, anti: bool, cycle: bool, cut: Option<CommitSeq>) -> String {
    let temporal = cut.map(|s| format!(" FOR SYSTEM_TIME AS OF SEQ {}", s.0)).unwrap_or_default();
    format!("MATCH (a){temporal} WHERE {}EXISTS {{ MATCH (a){}{} WHERE x.p > 5 }} RETURN a, a.p AS p",
        if anti { "NOT " } else { "" }, atom("x",d), if cycle { atom("a",d) } else { String::new() })
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut roots = WriteBatch::new(R);
    for (at, id) in IDS[..5].iter().copied().enumerate() {
        let props = match at { 0 => vec![(P,CanonicalScalar::Int(1))], 1 => vec![(P,CanonicalScalar::Null)],
            2 => vec![(P,CanonicalScalar::Int(7))], 3 => vec![(P,CanonicalScalar::Int(-2))], _ => vec![] };
        roots.create_vertex(id, if at == 0 || at == 3 { vec![L] } else { vec![] }, props);
    }
    for (eid,a,b) in [(0,0,2),(1,0,2),(2,1,3),(3,2,0),(4,3,3)] {
        roots.add_edge(EId(eid),IDS[a],IDS[b],vec![]);
    }
    let mut other = WriteBatch::new(Q);
    other.add_edge(EId(5),IDS[4],IDS[0],vec![]);
    db.write_atomic(cx, vec![roots,other]).await.unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> { rows.iter().map(|r|r.values().to_vec()).collect() }
fn field(db: &Database<MemVfs>, cut: CommitSeq, id: VId) -> CanonicalScalar {
    db.vertex_at(id,cut).unwrap().unwrap().props.iter().find(|(p,_)| *p == P)
        .map(|(_,v)|v.clone()).unwrap_or(CanonicalScalar::Null)
}
fn orient(a: VId,b: VId,d: GlaDirection) -> Vec<(VId,VId)> {
    match d { GlaDirection::Forward => vec![(a,b)], GlaDirection::Reverse => vec![(b,a)],
        GlaDirection::Undirected if a == b => vec![(a,b)], _ => vec![(a,b),(b,a)] }
}
// Full owned Cartesian oracle; no production probe, index or early-exit path.
fn oracle(db: &Database<MemVfs>, cut: CommitSeq, d: GlaDirection, anti: bool, cycle: bool) -> Vec<Vec<GraphValue>> {
    let edges = db.edges_at(cut).unwrap();
    let relation: Vec<_> = edges.iter().filter(|e|e.entry.relation==R)
        .flat_map(|e|orient(e.entry.src,e.entry.dst,d)).collect();
    let mut result = Vec::new();
    for a in IDS {
        if db.vertex_at(a,cut).unwrap().is_none() { continue; }
        let mut witnesses = 0;
        for &(from,x) in &relation {
            if from != a || !matches!(field(db,cut,x),CanonicalScalar::Int(v) if v > 5) { continue; }
            if cycle { for &(via,end) in &relation { if via == x && end == a { witnesses += 1; } } }
            else { witnesses += 1; }
        }
        if (witnesses>0) != anti { result.push(vec![GraphValue::Vertex(a),GraphValue::Scalar(field(db,cut,a))]); }
    }
    result.sort(); result
}

#[test]
fn vertex_probe_truth_is_snapshot_exact_and_preserves_isolates_across_compaction_and_reopen() {
    let ((),report) = run_async_under_lab(0x7670_0001,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit(); let params=GqlParameters::new();
        let vfs=MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=seed(&mut db,&commit).await; let view=db.read_session().unwrap();
        let input=statement(GlaDirection::Forward,true,false,None);
        let q=prepare(&input); let old=oracle(&db,basis,GlaDirection::Forward,true,false);
        assert!(old.iter().any(|r|r[0]==GraphValue::Vertex(IDS[4])));
        let mut paused=db.stream_graph_values_governed(&cx,&q,policy()).unwrap();
        let first=paused.next().unwrap().unwrap(); drop(q);
        let mut edit=WriteBatch::new(R); edit.set_vertex_property(IDS[3],P,Some(CanonicalScalar::Int(8)));
        let changed=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.delete_edge(EId(0));
        let one_left=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.delete_edge(EId(1));
        let gone=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(Q); edit.delete_vertex(IDS[3]);
        let cascaded=db.write(&commit,edit).await.unwrap();
        let mut edit=WriteBatch::new(R); edit.create_vertex(IDS[5],vec![],vec![]);
        let new_isolate=db.write(&commit,edit).await.unwrap();
        let mut cases=Vec::new();
        for cut in [CommitSeq(0),basis,changed,one_left,gone,cascaded,new_isolate] {
            for d in [GlaDirection::Forward,GlaDirection::Reverse,GlaDirection::Undirected] {
                for anti in [false,true] { for cycle in [false,true] {
                    let want=oracle(&db,cut,d,anti,cycle); let q=prepare(&statement(d,anti,cycle,None));
                    let eager=db.execute_graph_pattern_governed_at(&cx,&q,cut,policy()).unwrap();
                    assert_eq!(plain(&eager.value),want);
                    let mut direct=db.stream_graph_values_governed_at(&cx,&q,cut,policy()).unwrap();
                    assert_eq!(plain(&direct.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),want);
                    let native=PreparedNativeRead::prepare(&statement(d,anti,cycle,Some(cut)),&params,symbols).unwrap();
                    let (columns,mut stream)=native.stream(&db,&cx,&params,policy()).unwrap();
                    assert_eq!(columns,vec!["a".to_owned(),"p".to_owned()]);
                    assert_eq!(stream.snapshot_seq(),cut);
                    assert_eq!(plain(&stream.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),want);
                    assert_eq!(stream.row_stats(),direct.row_stats());
                    assert_eq!(stream.evaluator_stats(),direct.evaluator_stats());
                    cases.push((cut,d,anti,cycle,want));
                } }
            }
        }
        db.compact(&commit).await.unwrap(); drop(db);
        let db=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        let mut actual=vec![first]; actual.extend(paused.by_ref().map(Result::unwrap));
        assert_eq!(plain(&actual),old); assert_eq!(paused.snapshot_seq(),basis);
        let native=PreparedNativeRead::prepare(&input,&params,symbols).unwrap();
        let (_,mut pinned)=native.stream_in_view(&view,&cx,&params,policy()).unwrap();
        assert_eq!(plain(&pinned.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),old);
        for (cut,d,anti,cycle,want) in cases {
            let q=prepare(&statement(d,anti,cycle,None));
            let mut stream=db.stream_graph_values_governed_at(&cx,&q,cut,policy()).unwrap();
            assert_eq!(plain(&stream.by_ref().collect::<Result<Vec<_>,_>>().unwrap()),want);
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn vertex_only_sequential_and_filtered_probes_keep_the_native_binding_and_null_semantics() {
    let ((),report)=run_async_under_lab(0x7670_0002,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query();
        let mut db=Database::open_memory(&contexts.commit(),keys()).await.unwrap();
        seed(&mut db,&contexts.commit()).await;
        for input in [
            "MATCH (a) WHERE EXISTS { MATCH (a) WHERE a.p IS NULL } RETURN a, a.p AS p",
            "MATCH (a) WHERE NOT EXISTS { MATCH (a) } RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } AND NOT EXISTS { MATCH (a)-[:R]->(x) WHERE x.p IS NULL } RETURN a",
            "MATCH (a:L) WHERE a.p < 0 AND EXISTS { MATCH (a)-[:R]->(x) WHERE x = a } RETURN a, a.p AS p",
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) WHERE NOT (x.p = 7) } RETURN DISTINCT a SKIP 1 LIMIT 2",
        ] {
            let q=prepare(input); let expected=db.execute_graph_pattern_governed(&cx,&q,policy()).unwrap().value;
            let native=PreparedNativeRead::prepare(input,&GqlParameters::new(),symbols).unwrap();
            let (_,mut stream)=native.stream(&db,&cx,&GqlParameters::new(),policy()).unwrap();
            assert_eq!(stream.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),expected,"{input}");
        }
        let input=statement(GlaDirection::Forward,false,false,None).replace("> 5","> $floor");
        let args=GqlParameters::new().with_int64("floor",5).unwrap();
        let native=PreparedNativeRead::prepare(&input,&args,symbols).unwrap();
        assert!(matches!(native.stream(&db,&cx,&GqlParameters::new(),policy()),Err(QueryError::PatternText(_))));
        let (_,mut low)=native.stream(&db,&cx,&args,policy()).unwrap();
        let high=GqlParameters::new().with_int64("floor",100).unwrap();
        let (_,mut high)=native.stream(&db,&cx,&high,policy()).unwrap();
        assert!(high.next().is_none());
        assert_eq!(low.by_ref().collect::<Result<Vec<_>,_>>().unwrap().len(),1);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn one_root_and_one_witness_ignore_unrelated_edges_and_limit_zero_never_reads_the_source() {
    let ((),report)=run_async_under_lab(0x7670_0003,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap(); let mut seed=WriteBatch::new(R);
        for i in 0..5 { seed.create_vertex(VId(i),vec![],vec![]); }
        seed.add_edge(EId(0),VId(0),VId(1),vec![]);
        for i in 1..=2048 { seed.add_edge(EId(i),VId(2),VId(3),vec![]); }
        db.write(&commit,seed).await.unwrap();
        let input="MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a LIMIT 1";
        let native=PreparedNativeRead::prepare(input,&GqlParameters::new(),symbols).unwrap();
        let small=GqlQueryPolicy::new(2,1,10_000,10_000);
        let (_,mut stream)=native.stream(&db,&cx,&GqlParameters::new(),small).unwrap();
        assert_eq!(stream.next().unwrap().unwrap().values(),&[GraphValue::Vertex(VId(0))]);
        assert_eq!(stream.row_stats().snapshot_records,2); assert!(stream.next().is_none());
        let negative=prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(a) } RETURN a LIMIT 1");
        let mut bad=db.stream_graph_values_governed(&cx,&negative,GqlQueryPolicy::new(1,1,10_000,10_000)).unwrap();
        assert!(matches!(bad.next(),Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(bad.state(),VertexScanState::Failed); assert_eq!(bad.row_stats().result_rows,0);
        assert!(bad.next().is_none());
        let mut retry=db.stream_graph_values_governed(&cx,&negative,small).unwrap();
        assert_eq!(retry.next().unwrap().unwrap().values(),&[GraphValue::Vertex(VId(0))]);
        assert_eq!(retry.row_stats().snapshot_records,2);
        let zero=prepare("MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(x) } RETURN a LIMIT 0");
        let mut cursor=db.stream_graph_values_governed(&cx,&zero,GqlQueryPolicy::new(0,0,1,0)).unwrap();
        assert!(cursor.next().is_none()); assert_eq!(cursor.row_stats().snapshot_records,0);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn exact_cumulative_limits_and_admission_precedence_survive_vertex_probe_dispatch() {
    let ((),report)=run_async_under_lab(0x7670_0004,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root); let cx=contexts.query();
        let mut db=Database::open_memory(&contexts.commit(),keys()).await.unwrap(); let basis=seed(&mut db,&contexts.commit()).await;
        let input=statement(GlaDirection::Undirected,true,false,None); let q=prepare(&input);
        let mut good=db.stream_graph_values_governed(&cx,&q,policy()).unwrap();
        let expected=good.by_ref().collect::<Result<Vec<_>,_>>().unwrap(); let r=good.row_stats(); let e=good.evaluator_stats();
        assert!(!expected.is_empty());
        let exact=GqlQueryPolicy::new(r.snapshot_records,r.result_rows,e.work_units,e.scratch_entries);
        let mut retry=db.stream_graph_values_governed(&cx,&q,exact).unwrap();
        assert_eq!(retry.by_ref().collect::<Result<Vec<_>,_>>().unwrap(),expected);
        assert_eq!(retry.evaluator_stats(),e);
        for bad in [GqlQueryPolicy::new(r.snapshot_records-1,10000,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(100000,r.result_rows-1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(100000,10000,e.work_units-1,u64::MAX),
            GqlQueryPolicy::new(100000,10000,u64::MAX,e.scratch_entries-1)] {
            let mut stream=db.stream_graph_values_governed(&cx,&q,bad).unwrap(); let mut delivered=Vec::new();
            loop { match stream.next() {
                Some(Ok(row))=>delivered.push(row),
                Some(Err(GqlQueryError::Rows(_)|GqlQueryError::Evaluator(_)))=>break,
                other=>panic!("refusal became {other:?}"),
            } }
            assert_eq!(delivered,expected[..delivered.len()]); assert_eq!(stream.row_stats().result_rows,delivered.len() as u64);
            assert_eq!(stream.state(),VertexScanState::Failed); stream.close(); assert!(stream.next().is_none());
        }
        let unavailable=prepare("MATCH (a) WHERE EXISTS { MATCH (x)-[:R*1..2]->(y) } RETURN a LIMIT 0");
        let none=GqlQueryPolicy::new(0,0,0,0);
        assert!(matches!(db.stream_graph_values_governed(&cx,&unavailable,none),Err(GqlQueryError::Source(VertexScanError::Plan(_)))));
        assert!(matches!(db.stream_graph_values_governed_at(&cx,&unavailable,CommitSeq(basis.0+1),none),
            Err(GqlQueryError::Source(VertexScanError::Source(ReadError::BeyondFrontier{..})))));
        let view=db.read_session().unwrap();
        assert!(matches!(view.stream_graph_values_governed_at(&cx,&q,CommitSeq(basis.0+1),none),
            Err(GqlQueryError::Source(VertexScanError::Source(ReadError::BeyondFrontier{..})))));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
