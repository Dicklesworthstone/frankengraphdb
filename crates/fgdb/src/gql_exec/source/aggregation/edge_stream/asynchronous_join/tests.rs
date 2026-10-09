use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::edge_stream::EdgeScanState;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
const QUERY: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c) \
    RETURN r,a,s,b,c,r.p AS rp,s.p AS sp,a.p AS ap,c.p AS cp";
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind,name) {
        (GraphSymbolKind::Relation,"R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property,"p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,100_000,10_000_000,10_000_000) }
fn keys() -> crate::DatabaseKeys {
    crate::DatabaseKeys::new([0xb1;32],DatabaseSecurityNamespaceId([0xb2;32]),[0xb3;32])
}
fn initial() -> crate::WriteBatch {
    let mut batch = crate::WriteBatch::new(R);
    for (at,id) in IDS.into_iter().enumerate() {
        batch.create_vertex(id,vec![],vec![(P,CanonicalScalar::Int(at as i64+1))]);
    }
    for (id,from,to,value) in [(0,0,0,5),(1,0,1,7),(2,0,1,11),(u128::MAX,1,2,13)] {
        batch.add_edge(EId(id),IDS[from],IDS[to],vec![(P,CanonicalScalar::Int(value))]);
    }
    batch
}
// Independent finite effect oracle, not either query executor or source index.
fn expected(cut: u64) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    if cut == 0 { return rows; }
    let edges = [(0,0,0,5),(1,0,1,7),(2,0,1,11),
        (u128::MAX,1,2,if cut==1 { 13 } else { 17 })];
    let vertex = |at: usize| if cut>1 && at==2 { 23 } else { at as i64+1 };
    for &(r,a,b,rp) in &edges {
        if cut>=3 && r==2 { continue; }
        for &(s,from,c,sp) in &edges {
            if b!=from || cut>=3 && s==2 { continue; }
            rows.push(vec![GraphValue::Edge(EId(r)),GraphValue::Vertex(IDS[a]),
                GraphValue::Edge(EId(s)),GraphValue::Vertex(IDS[b]),GraphValue::Vertex(IDS[c]),
                GraphValue::Scalar(CanonicalScalar::Int(rp)),GraphValue::Scalar(CanonicalScalar::Int(sp)),
                GraphValue::Scalar(CanonicalScalar::Int(vertex(a))),GraphValue::Scalar(CanonicalScalar::Int(vertex(c)))]);
        }
    }
    rows.sort(); rows
}

#[test]
fn async_native_joins_keep_exact_mvcc_cuts_through_property_updates_and_retirement() {
    let ((),report) = run_async_under_lab(0xa51c_1001,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit,cx) = (contexts.commit(),contexts.query());
        let mut db = Database::open_memory(&commit,keys()).await.unwrap();
        assert_eq!(db.write(&commit,initial()).await.unwrap(),CommitSeq(1));
        let mut changed = crate::WriteBatch::new(R);
        changed.set_edge_property(EId(u128::MAX),P,Some(CanonicalScalar::Int(17)));
        changed.set_vertex_property(IDS[2],P,Some(CanonicalScalar::Int(23)));
        assert_eq!(db.write(&commit,changed).await.unwrap(),CommitSeq(2));
        let mut retired = crate::WriteBatch::new(R); retired.delete_edge(EId(2));
        assert_eq!(db.write(&commit,retired).await.unwrap(),CommitSeq(3));
        let view = db.read_session().unwrap();
        for cut in 0..=3 { for (skip,count) in [(0,100),(1,2),(0,0)] {
            let definition = pattern(&format!("{QUERY} SKIP {skip} LIMIT {count}"));
            let mut cursor = db.stream_graph_edge_joins_governed_at(&cx,&definition,CommitSeq(cut),policy()).unwrap();
            assert_eq!(cursor.row_stats().snapshot_records,0);
            let mut actual = Vec::new();
            while let Some(row) = cursor.next().await { actual.push(row.unwrap().values().to_vec()); }
            let oracle: Vec<_> = expected(cut).into_iter().skip(skip).take(count).collect();
            assert_eq!(actual,oracle);
            assert_eq!(cursor.state(),EdgeScanState::Exhausted);
            assert_eq!(cursor.snapshot_seq(),CommitSeq(cut));
            assert!(cursor.next().await.is_none());
            let sync: Vec<_> = view.stream_graph_edges_governed_at(&cx,&definition,CommitSeq(cut),policy())
                .unwrap().map(|row| row.unwrap().values().to_vec()).collect();
            assert_eq!(sync,oracle,"existing synchronous driver is also checked against the oracle");
            let mut pinned = view.stream_graph_edge_joins_governed_at(&cx,&definition,CommitSeq(cut),policy()).unwrap();
            let mut actual = Vec::new();
            while let Some(row) = pinned.next().await { actual.push(row.unwrap().values().to_vec()); }
            assert_eq!(actual,oracle);
        }}
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn the_cursor_owns_its_generation_not_the_database_view_or_prepared_definition() {
    let ((),report) = run_async_under_lab(0xa51c_1002,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit,cx) = (contexts.commit(),contexts.query());
        let mut db = Database::open_memory(&commit,keys()).await.unwrap();
        db.write(&commit,initial()).await.unwrap();
        let mut cursor = {
            let view = db.read_session().unwrap();
            let definition = pattern(QUERY);
            view.stream_graph_edge_joins_governed(&cx,&definition,policy()).unwrap()
        };
        let first = cursor.next().await.unwrap().unwrap();
        let mut changed = crate::WriteBatch::new(R);
        changed.set_edge_property(EId(u128::MAX),P,Some(CanonicalScalar::Int(17)));
        changed.set_vertex_property(IDS[2],P,Some(CanonicalScalar::Int(23)));
        db.write(&commit,changed).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut actual = vec![first.values().to_vec()];
        while let Some(row) = cursor.next().await { actual.push(row.unwrap().values().to_vec()); }
        assert_eq!(actual,expected(1));
        assert_eq!(cursor.snapshot_seq(),CommitSeq(1));
        assert_eq!(cursor.state(),EdgeScanState::Exhausted);
        drop(cursor);
        assert_eq!(first.values().to_vec(),expected(1)[0]);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn admission_fences_and_cumulative_quotas_survive_the_real_source_adapter() {
    let ((),report) = run_async_under_lab(0xa51c_1003,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit,cx) = (contexts.commit(),contexts.query());
        let mut db = Database::open_memory(&commit,keys()).await.unwrap();
        db.write(&commit,initial()).await.unwrap();
        let definition = pattern(QUERY);
        let zero = pattern(&format!("{QUERY} LIMIT 0"));
        assert!(db.stream_graph_edge_joins_governed_at(&cx,&zero,CommitSeq(2),policy()).is_err());
        let mut cursor = db.stream_graph_edge_joins_governed(&cx,&zero,GqlQueryPolicy::new(0,0,100,100)).unwrap();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records,0);
        let mut cursor = db.stream_graph_edge_joins_governed(&cx,&definition,
            GqlQueryPolicy::new(1,100,1_000_000,1_000_000)).unwrap();
        assert!(matches!(cursor.next().await,Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().snapshot_records,1,"nested candidate shares root budget");
        assert_eq!(cursor.row_stats().result_rows,0);
        assert!(cursor.next().await.is_none());
        let mut cursor = db.stream_graph_edge_joins_governed(&cx,&definition,
            GqlQueryPolicy::new(100,1,1_000_000,1_000_000)).unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        assert!(matches!(cursor.next().await,Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().result_rows,1);
        assert_eq!(cursor.state(),EdgeScanState::Failed);
        assert_eq!(first.values().to_vec(),expected(1)[0]);
        let unsupported = pattern("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN c");
        assert!(matches!(db.stream_graph_edge_joins_governed(&cx,&unsupported,policy()),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn every_real_history_lookup_and_payload_copy_control_can_refuse_before_delivery() {
    let ((),report) = run_async_under_lab(0xa51c_1004,|root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit,cx) = (contexts.commit(),contexts.query());
        let mut db = Database::open_memory(&commit,keys()).await.unwrap();
        db.write(&commit,initial()).await.unwrap();
        let view = db.read_session().unwrap();
        let definition = pattern(&format!("{QUERY} LIMIT 1"));
        let mut events = 0;
        {
            let source = Source(view.edge_scan_source(&cx,CommitSeq(1)).unwrap());
            let mut cursor = AsyncEdgeJoinCursor::new(source,AsyncEdgeJoinPlan::compile(definition.plan()).unwrap(),policy(),|| {
                events += 1; Ok::<_,usize>(())
            });
            assert_eq!(cursor.next().await.unwrap().unwrap().values().to_vec(),expected(1)[0]);
            assert_eq!(cursor.state(),EdgeScanState::Exhausted);
        }
        for stop in 0..events {
            let source = Source(view.edge_scan_source(&cx,CommitSeq(1)).unwrap());
            let mut at = 0;
            let mut cursor = AsyncEdgeJoinCursor::new(source,AsyncEdgeJoinPlan::compile(definition.plan()).unwrap(),policy(),|| {
                let here = at; at += 1;
                if here==stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(cursor.next().await,Some(Err(GqlQueryError::Interrupted(found))) if found==stop));
            assert_eq!(cursor.row_stats().result_rows,0);
            assert!(cursor.next().await.is_none());
        }
        let mut cursor = db.stream_graph_edge_joins_governed(&cx,&definition,policy()).unwrap();
        assert_eq!(cursor.next().await.unwrap().unwrap().values().to_vec(),expected(1)[0]);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
