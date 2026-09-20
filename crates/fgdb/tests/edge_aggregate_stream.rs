//! Database integration for complete numeric reductions over indexed joins.
//! Expected rows come from the ordinary engine; no streaming helper computes
//! that control result, and small hand counts guard the shared-source fixture.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, ReadError, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};
use fgdb_gql::edge_stream::aggregate::EdgeAggregatePlan;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const START: VId = VId(1_u128 << 100);
const END: VId = VId(u128::MAX);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0xa1;32], DatabaseSecurityNamespaceId([0xa2;32]), [0xa3;32]) }
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,1,10_000_000,10_000_000) }
fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name: &str| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(if name == "S" { S } else { R })),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(P)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut r = WriteBatch::new(R);
    for (id,n) in [(START,Some(3)),(VId(1),None),(END,Some(-2))] {
        r.create_vertex(id,vec![],n.map(|n|vec![(P,CanonicalScalar::Int(n))]).unwrap_or_default());
    }
    for (id,from,to,n) in [(1,START,VId(1),Some(7)),(2,START,VId(1),None),
        (3,END,START,Some(-4)),(4,START,START,Some(2))] {
        r.add_edge(EId(id),from,to,n.map(|n|vec![(P,CanonicalScalar::Int(n))]).unwrap_or_default());
    }
    let mut s = WriteBatch::new(S);
    s.add_edge(EId(5),VId(1),END,vec![(P,CanonicalScalar::Int(-3))]);
    s.add_edge(EId(6),VId(1),VId(1),vec![]);
    db.write_atomic(cx,vec![r,s]).await.unwrap()
}
fn shapes() -> Vec<String> {
    let mut result = Vec::new();
    for direction in 0..3 {
        let edge = |name: &str, rel: &str, end: &str| match direction {
            0 => format!("-[{name}:{rel}]->({end})"),
            1 => format!("<-[{name}:{rel}]-({end})"),
            _ => format!("-[{name}:{rel}]-({end})"),
        };
        let mut body = format!("(a){}",edge("r","R","b"));
        for shape in 0..3 {
            if shape == 1 { body.push_str(&edge("s","S","c")); }
            if shape == 2 { body.push_str(&edge("t","R","a")); }
            let end = if shape == 0 { "b" } else { "c" };
            result.push(format!("MATCH {body} RETURN COUNT(*) AS n,COUNT(r.p) AS present,SUM(r.p) AS edge_sum,SUM({end}.p) AS vertex_sum"));
        }
    }
    result.extend([
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) WHERE r.p>0 OR b.p IS NULL RETURN COUNT(*) AS n,SUM(s.p) AS total",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(c) } RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) WHERE c.p>0 } RETURN COUNT(*) AS n",
    ].map(str::to_owned));
    result
}

#[test]
fn edge_aggregate_reads_and_paused_cursors_keep_one_cut_through_recovery() {
    let ((),report) = run_async_under_lab(0xc0a5_5011,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit,vfs.clone(),&path,keys()).await.unwrap();
        let basis = seed(&mut db,&commit).await; let pinned = db.read_session().unwrap();
        let mut paused = Vec::new();
        for text in shapes() {
            let q = query(&text); let plan = EdgeAggregatePlan::compile(&q).unwrap();
            let expected = db.execute_graph_aggregate_governed(&cx,&q,policy()).unwrap().value;
            let cursor = db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
            assert_eq!(cursor.row_stats().snapshot_records,0);
            assert_eq!(cursor.row_stats().result_rows,0);
            assert_eq!(cursor.snapshot_seq(),basis);
            paused.push((q,plan,expected,cursor));
        }
        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(1),P,Some(CanonicalScalar::Int(-20)));
        change.set_vertex_property(END,P,Some(CanonicalScalar::Int(9)));
        change.delete_edge(EId(2));
        let edited = db.write(&commit,change).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let mut db = Database::open_with_vfs(&commit,vfs,&path,keys()).await.unwrap();
        for (q,plan,expected,mut cursor) in paused {
            assert_eq!(vec![cursor.next().unwrap().unwrap()],expected);
            assert_eq!(cursor.state(),EdgeScanState::Exhausted);
            assert_eq!(cursor.row_stats().result_rows,1); assert!(cursor.next().is_none());
            let mut old = db.stream_global_edge_aggregate_governed_at(&cx,&plan,basis,policy()).unwrap();
            assert_eq!(vec![old.next().unwrap().unwrap()],expected);
            let mut old = pinned.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
            assert_eq!(vec![old.next().unwrap().unwrap()],expected);
            let mut old = pinned.stream_global_edge_aggregate_governed_at(&cx,&plan,basis,policy()).unwrap();
            assert_eq!(vec![old.next().unwrap().unwrap()],expected);
            let want = db.execute_graph_aggregate_governed_at(&cx,&q,edited,policy()).unwrap().value;
            let mut live = db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
            assert_eq!(vec![live.next().unwrap().unwrap()],want);
        }
        let mut cascade = WriteBatch::new(R); cascade.delete_vertex(VId(1));
        db.write(&commit,cascade).await.unwrap();
        for text in shapes() {
            let q=query(&text); let plan=EdgeAggregatePlan::compile(&q).unwrap();
            let expected=db.execute_graph_aggregate_governed(&cx,&q,policy()).unwrap().value;
            let mut stream=db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
            assert_eq!(vec![stream.next().unwrap().unwrap()],expected);
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn cumulative_candidate_work_scratch_and_output_limits_refuse_atomically() {
    let ((),report)=run_async_under_lab(0xc0a5_5012,|root| async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap(); let basis=seed(&mut db,&commit).await;
        let q=query("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(*) AS n,SUM(r.p) AS total");
        let plan=EdgeAggregatePlan::compile(&q).unwrap();
        let mut full=db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
        let expected=full.next().unwrap().unwrap(); let rows=full.row_stats(); let eval=full.evaluator_stats();
        assert_eq!(expected.values()[0].as_count(),Some(4));
        assert_eq!(expected.values()[1].as_integer(),Some(14));
        let exact=GqlQueryPolicy::new(rows.snapshot_records,1,eval.work_units,eval.scratch_entries);
        assert_eq!(db.stream_global_edge_aggregate_governed(&cx,&plan,exact).unwrap().next().unwrap().unwrap(),expected);
        for p in [GqlQueryPolicy::new(rows.snapshot_records-1,1,u64::MAX,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,0,u64::MAX,u64::MAX),GqlQueryPolicy::new(u64::MAX,1,eval.work_units-1,u64::MAX),
            GqlQueryPolicy::new(u64::MAX,1,u64::MAX,eval.scratch_entries-1)] {
            let mut stream=db.stream_global_edge_aggregate_governed(&cx,&plan,p).unwrap();
            assert!(stream.next().unwrap().is_err()); assert_eq!(stream.row_stats().result_rows,0);
            assert_eq!(stream.state(),EdgeScanState::Failed); assert!(stream.next().is_none());
        }
        let mut stopped=db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
        stopped.close(); assert!(stopped.next().is_none()); assert_eq!(stopped.row_stats().snapshot_records,0);
        assert!(matches!(db.stream_global_edge_aggregate_governed_at(&cx,&plan,CommitSeq(basis.0+1),GqlQueryPolicy::new(0,0,0,0)),
            Err(GqlQueryError::Source(GraphAggregateError::Source(EdgeScanError::Source(ReadError::BeyondFrontier{..}))))));
        assert_eq!(db.stream_global_edge_aggregate_governed(&cx,&plan,exact).unwrap().next().unwrap().unwrap(),expected);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn late_invalid_sums_do_not_publish_the_accumulated_count_and_old_views_stay_valid() {
    let ((),report)=run_async_under_lab(0xc0a5_5013,|root| async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap(); seed(&mut db,&commit).await;
        let view=db.read_session().unwrap();
        let q=query("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n,SUM(r.p) AS total");
        let plan=EdgeAggregatePlan::compile(&q).unwrap();
        let mut bad=WriteBatch::new(R); bad.set_edge_property(EId(4),P,Some(CanonicalScalar::Bool(true)));
        db.write(&commit,bad).await.unwrap();
        let mut stream=db.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
        assert!(matches!(stream.next(),Some(Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum{aggregate:1})))));
        assert_eq!(stream.row_stats().result_rows,0); assert!(stream.next().is_none());
        let mut old=view.stream_global_edge_aggregate_governed(&cx,&plan,policy()).unwrap();
        let row=old.next().unwrap().unwrap(); assert_eq!(row.values()[0].as_count(),Some(4)); assert_eq!(row.values()[1].as_integer(),Some(5));
        let empty=EdgeAggregatePlan::compile(&query("MATCH (a)-[r:R]->(b) WHERE a.p=999 RETURN COUNT(*) AS n,SUM(r.p) AS total")).unwrap();
        let row=view.stream_global_edge_aggregate_governed(&cx,&empty,policy()).unwrap().next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_count(),Some(0)); assert!(row.values()[1].is_null());
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn thousands_of_parallel_inputs_need_one_output_row_allowance() {
    let ((),report)=run_async_under_lab(0xc0a5_5014,|root| async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query(); let commit=contexts.commit();
        let mut db=Database::open_memory(&commit,keys()).await.unwrap();
        let mut batch=WriteBatch::new(R); batch.create_vertex(START,vec![],vec![]); batch.create_vertex(END,vec![],vec![]);
        for id in 0..4096 { batch.add_edge(EId(id),START,END,vec![(P,CanonicalScalar::Int(i64::MAX))]); }
        db.write(&commit,batch).await.unwrap();
        let q=query("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n,SUM(r.p) AS total");
        let plan=EdgeAggregatePlan::compile(&q).unwrap();
        let mut stream=db.stream_global_edge_aggregate_governed(&cx,&plan,GqlQueryPolicy::new(4096,1,10_000_000,1_000_000)).unwrap();
        let result=stream.next().unwrap().unwrap();
        assert_eq!(result.values()[0].as_count(),Some(4096));
        assert_eq!(result.values()[1].as_integer(),Some(4096*i128::from(i64::MAX)));
        assert_eq!(stream.row_stats().snapshot_records,4096); assert_eq!(stream.row_stats().result_rows,1);
        assert!(stream.next().is_none());
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
