//! Actual native dispatch, snapshots and layouts for computed edge aggregates.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeAggregateCursor, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const Q: PropertyKeyId = PropertyKeyId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const HIGH: VId = VId(u128::MAX);
type Edge = (u128, VId, VId, Option<i64>, i64);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x31;32], DatabaseSecurityNamespaceId([0x32;32]), [0x33;32]) }
fn symbols() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_property("quantity", Q).with_property("price", P)
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX,u64::MAX,u64::MAX,u64::MAX) }
fn arguments(cut: u64, scale: i64) -> GqlParameters {
    GqlParameters::new().with_uint64("cut",cut).unwrap().with_int64("scale",scale).unwrap()
}
fn edges(cut: u64) -> Vec<Edge> {
    if cut==0 { return vec![]; }
    let mut rows=vec![(10,VId(0),VId(1),Some(2),5),(11,VId(0),VId(1),Some(5),2),
        (12,VId(1),HIGH,Some(3),7),(13,VId(0),HIGH,None,7),
        (14,HIGH,VId(0),Some(-2),4),(15,HIGH,HIGH,Some(0),9)];
    if cut>=2 { rows[0].3=Some(4); rows.retain(|e|e.0!=11); }
    if cut>=3 { rows.retain(|e|e.1!=VId(1) && e.2!=VId(1)); }
    rows
}
fn seed() -> WriteBatch {
    let mut batch=WriteBatch::new(R);
    for vid in [VId(0),VId(1),HIGH,VId(99)] { batch.create_vertex(vid,vec![],vec![]); }
    for (id,from,to,q,p) in edges(1) {
        let mut props=vec![(P,CanonicalScalar::Int(p))];
        if let Some(q)=q { props.push((Q,CanonicalScalar::Int(q))); }
        batch.add_edge(EId(id),from,to,props);
    }
    batch
}
fn text(direction:usize,hops:usize,grouped:bool)->String {
    let atom=|edge,target|match direction {
        0=>format!("-[{edge}:R]->({target})"),
        1=>format!("<-[{edge}:R]-({target})"),
        _=>format!("-[{edge}:R]-({target})"),
    };
    let mut pattern=format!("(a){}",atom("r","b"));
    if hops==2 { pattern+=&atom("s","c"); }
    let end=if hops==1 {"b"} else {"c"};
    let value="r.quantity*r.price*$scale";
    let key=if grouped {format!(", {end} AS destination")} else {String::new()};
    let repeated=if grouped {format!(", {end} AS repeated")} else {String::new()};
    let clause=if grouped {format!(" GROUP BY {end}")} else {String::new()};
    format!("MATCH {pattern} FOR SYSTEM_TIME AS OF SEQ $cut RETURN SUM({value}) AS total{key}, COUNT(*) AS occurrences, AVG({value}) AS average{repeated}, COUNT(DISTINCT {value}) AS support{clause}")
}
fn drain(cursor:&mut NativeAggregateCursor<'_>)->Vec<Vec<GraphAggregateValue>> {
    let slots=cursor.output_slots().to_vec();
    cursor.by_ref().map(|row| {
        let row=row.unwrap();
        slots.iter().map(|slot|match *slot {
            GraphAggregateTextSlot::Aggregate(at)=>row.values()[at].clone(),
            GraphAggregateTextSlot::GroupKey(at)=>GraphAggregateValue::Value(row.keys()[at].clone()),
        }).collect()
    }).collect()
}
// Enumerate real oriented edge occurrences independently of GLA/parser/storage.
// Project only completed bindings, group afterward, and encode RETURN layout.
fn oracle(cut:u64,scale:i64,direction:usize,hops:usize,grouped:bool)->Vec<Vec<GraphAggregateValue>> {
    let mut oriented=Vec::new();
    for (_,a,b,q,p) in edges(cut) {
        let value=q.map(|q|i128::from(q)*i128::from(p)*i128::from(scale));
        if direction==1 { oriented.push((b,a,value)); } else {
            oriented.push((a,b,value));
            if direction==2 && a!=b { oriented.push((b,a,value)); }
        }
    }
    let mut complete=Vec::new();
    for &(_,b,value) in &oriented {
        if hops==1 { complete.push((b,value)); } else {
            for &(from,to,_) in &oriented { if from==b { complete.push((to,value)); } }
        }
    }
    let mut groups=BTreeMap::<Option<VId>,Vec<Option<i128>>>::new();
    if !grouped { groups.insert(None,vec![]); }
    for (end,value) in complete { groups.entry(grouped.then_some(end)).or_default().push(value); }
    groups.into_iter().map(|(key,all)| {
        let values:Vec<_>=all.iter().flatten().copied().collect();
        let null=||GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let mut row=vec![if values.is_empty() {null()} else {GraphAggregateValue::Integer(values.iter().sum())}];
        if let Some(key)=key { row.push(GraphAggregateValue::Value(GraphValue::Vertex(key))); }
        row.push(GraphAggregateValue::Count(all.len() as u64));
        row.push(if values.is_empty() {null()} else {
            GraphAggregateValue::Average(GraphExactAverage::new(values.iter().sum(),values.len() as u64).unwrap())
        });
        if let Some(key)=key { row.push(GraphAggregateValue::Value(GraphValue::Vertex(key))); }
        row.push(GraphAggregateValue::Count(values.iter().collect::<BTreeSet<_>>().len() as u64));
        row
    }).collect()
}

#[test]
fn native_computed_inputs_keep_bound_parameters_layouts_and_pinned_history_through_reopen() {
    let ((),report)=run_async_under_lab(0xc0a5_5001,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let vfs=MemVfs::new().unwrap(); let path=vfs.database_dir();
        let mut db=Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis=db.write(&commit,seed()).await.unwrap(); assert_eq!(basis,CommitSeq(1));
        let pin=db.read_session().unwrap();
        let mut paused=Vec::new();
        for direction in 0..3 { for hops in 1..=2 { for grouped in [false,true] {
            let statement=text(direction,hops,grouped); let params=arguments(1,2);
            let prepared=PreparedNativeRead::prepare(&statement,&params,symbols()).unwrap();
            let cursor=prepared.stream_aggregate(&db,&cx,&params,wide()).unwrap();
            fn send(_: &impl Send) {} send(&cursor);
            assert_eq!(cursor.kind(),ScanKind::Edge);
            assert_eq!(cursor.row_stats().snapshot_records,0);
            assert_eq!(cursor.evaluator_stats().work_units,0);
            paused.push((direction,hops,grouped,cursor));
            // These owners can drop: the opened cursor borrows only QueryCx.
            drop(prepared); drop(params);
        }}}
        let mut edit=WriteBatch::new(R);
        edit.set_edge_property(EId(10),Q,Some(CanonicalScalar::Int(4)));
        edit.delete_edge(EId(11)); db.write(&commit,edit).await.unwrap();
        let mut cascade=WriteBatch::new(R); cascade.delete_vertex(VId(1));
        db.write(&commit,cascade).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let db=Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        for (direction,hops,grouped,mut cursor) in paused {
            assert_eq!(drain(&mut cursor),oracle(1,2,direction,hops,grouped));
            assert_eq!(cursor.snapshot_seq(),basis); assert!(cursor.next().is_none());
            let statement=text(direction,hops,grouped);
            let prepared=PreparedNativeRead::prepare(&statement,&arguments(1,1),symbols()).unwrap();
            for cut in 0..=3 { for scale in [1,2] {
                let params=arguments(cut,scale);
                let QueryResult::Rows{columns,rows}=prepared.execute(&db,&cx,&params,wide()).unwrap() else {panic!("read");};
                let mut cursor=prepared.stream_aggregate(&db,&cx,&params,wide()).unwrap();
                assert_eq!(cursor.columns(),columns);
                assert_eq!(drain(&mut cursor),rows);
                assert_eq!(rows,oracle(cut,scale,direction,hops,grouped));
                assert_eq!(cursor.snapshot_seq(),CommitSeq(cut));
            }}
            let mut pinned=prepared.stream_aggregate_in_view(&pin,&cx,&arguments(1,1),wide()).unwrap();
            assert_eq!(drain(&mut pinned),oracle(1,1,direction,hops,grouped));
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn native_expression_failures_and_exact_quotas_never_release_partial_summaries() {
    let ((),report)=run_async_under_lab(0xc0a5_5002,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap(); db.write(&commit,seed()).await.unwrap();
        let statement=text(0,2,true); let params=arguments(1,2);
        let prepared=PreparedNativeRead::prepare(&statement,&params,symbols()).unwrap();
        let mut baseline=prepared.stream_aggregate(&db,&cx,&params,wide()).unwrap();
        let expected=drain(&mut baseline); let r=baseline.row_stats(); let e=baseline.evaluator_stats();
        let exact=GqlQueryPolicy::new(r.snapshot_records,r.result_rows,e.work_units,e.scratch_entries);
        assert_eq!(drain(&mut prepared.stream_aggregate(&db,&cx,&params,exact).unwrap()),expected);
        for allowance in [GqlQueryPolicy::new(r.snapshot_records-1,r.result_rows,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,r.result_rows-1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,r.result_rows,e.work_units-1,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,r.result_rows,u64::MAX,e.scratch_entries-1)] {
            let mut cursor=prepared.stream_aggregate(&db,&cx,&params,allowance).unwrap();
            assert!(cursor.by_ref().collect::<Result<Vec<_>,_>>().is_err());
            assert_eq!(cursor.state(),VertexScanState::Failed); assert!(cursor.next().is_none());
        }
        let invalid="MATCH (a)-[r:R]->(b) RETURN b, SUM(r.quantity*r.price) AS total GROUP BY b";
        let prepared=PreparedNativeRead::prepare(invalid,&GqlParameters::new(),symbols()).unwrap();
        for value in [CanonicalScalar::Int(i64::MAX),CanonicalScalar::ucs_basic_text("private arithmetic operand").unwrap()] {
            let mut edit=WriteBatch::new(R); edit.set_edge_property(EId(15),Q,Some(value));
            db.write(&commit,edit).await.unwrap();
            let mut cursor=prepared.stream_aggregate(&db,&cx,&GqlParameters::new(),wide()).unwrap();
            let error=cursor.next().unwrap().unwrap_err();
            assert!(matches!(&error,GqlQueryError::Source(GraphAggregateError::InputExpression{row:0,..})));
            assert!(!format!("{error}").contains("private arithmetic operand"));
            assert_eq!(cursor.row_stats().result_rows,0); assert!(cursor.next().is_none());
        }
        let mut closed=prepared.stream_aggregate(&db,&cx,&GqlParameters::new(),wide()).unwrap();
        closed.close(); closed.close(); assert!(closed.next().is_none());
        assert_eq!(closed.row_stats().snapshot_records,0); assert_eq!(closed.evaluator_stats().work_units,0);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn computed_source_admission_and_unsupported_relational_children_never_fall_back() {
    let ((),report)=run_async_under_lab(0xc0a5_5003,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap(); db.write(&commit,seed()).await.unwrap();
        for statement in [
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.quantity+1) AS total LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.quantity+1) AS total HAVING total>0",
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.quantity+1) AS values",
            "MATCH (a)-[r:R]->(b) WITH DISTINCT r.quantity AS q RETURN SUM(q+1) AS total",
            "MATCH (a)-[r:R]->(b) WITH r.quantity AS q LIMIT 1 RETURN SUM(q+1) AS total",
        ] {
            let prepared=PreparedNativeRead::prepare(statement,&GqlParameters::new(),symbols()).unwrap();
            assert!(prepared.stream_aggregate(&db,&cx,&GqlParameters::new(),GqlQueryPolicy::new(0,0,0,0)).is_err(),"{statement}");
        }
        let statement=text(0,1,false); let params=arguments(1,2);
        let prepared=PreparedNativeRead::prepare(&statement,&params,symbols()).unwrap();
        assert!(prepared.stream_aggregate(&db,&cx,&GqlParameters::new(),wide()).is_err());
        assert!(matches!(prepared.stream_aggregate(&db,&cx,&arguments(2,2),GqlQueryPolicy::new(0,0,0,0)),Err(QueryError::EdgeAggregateStream(_))));
        // Empty inputs produce native empty aggregates rather than evaluating
        // a scalar expression with no binding; source history is still checked.
        let empty="MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 0 RETURN SUM(1/0) AS total, COUNT(*) AS rows";
        let mut cursor=db.query_aggregate_stream(&cx,empty,&GqlParameters::new(),symbols(),wide()).unwrap();
        let row=cursor.next().unwrap().unwrap();
        assert!(row.values()[0].is_null()); assert_eq!(row.values()[1].as_count(),Some(0));
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
