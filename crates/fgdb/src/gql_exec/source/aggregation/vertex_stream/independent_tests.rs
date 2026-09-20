//! Candidate positions and every actual root/probe/delivery refusal boundary.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_gql::stream::VertexScanState;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::sync::Arc;

#[test]
fn local_successors_preserve_root_position_and_every_lookup_refusal_retries() {
    let ((),report)=run_async_under_lab(0x696e_6404,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let keys=crate::DatabaseKeys::new([0x81;32],DatabaseSecurityNamespaceId([0x82;32]),[0x83;32]);
        let mut db=Database::open_memory(&commit,keys).await.unwrap();let mut batch=crate::WriteBatch::new(RelationId(1));
        for id in [0,17,1_u128<<100,u128::MAX] { batch.create_vertex(VId(id),vec![],vec![]); }
        db.write(&commit,batch).await.unwrap();
        let mut source=SnapshotVertexSource {view:db.read_session().unwrap(),cx:&cx,as_of:db.frontier().unwrap(),after:None};
        assert_eq!(source.next_vertex(&mut |_|Ok::<_,usize>(())).unwrap(),Some(VId(0)));
        for after in [None,Some(VId(0)),Some(VId(17)),Some(VId(1_u128<<100)),Some(VId(u128::MAX))] {
            let mut total=0;
            let expected=source.next_probe_vertex(after,&mut |_|{total+=1;Ok::<_,usize>(())}).unwrap();
            assert!(total>0);assert_eq!(source.after,Some(VId(0)));
            for stop in 1..=total {
                let mut at=0;
                let refused=source.next_probe_vertex(after,&mut |_|{at+=1;if at==stop {Err(stop)} else {Ok(())}});
                assert!(matches!(refused,Err(EdgeExpansionSourceError::Read(VertexScanSourceError::Control(n))) if n==stop));
                assert_eq!(at,stop);assert_eq!(source.after,Some(VId(0)));
                assert_eq!(source.next_probe_vertex(after,&mut |_|Ok::<_,usize>(())).unwrap(),expected);
            }
        }
        assert_eq!(source.next_vertex(&mut |_|Ok::<_,usize>(())).unwrap(),Some(VId(17)));
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn independent_history_and_capture_checkpoints_release_the_one_pin_on_refusal() {
    let ((),report)=run_async_under_lab(0x696e_6405,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);let cx=contexts.query();let commit=contexts.commit();
        let keys=crate::DatabaseKeys::new([0x81;32],DatabaseSecurityNamespaceId([0x82;32]),[0x83;32]);
        let mut db=Database::open_memory(&commit,keys).await.unwrap();let mut batch=crate::WriteBatch::new(RelationId(1));
        for (value,id) in [0,17,1_u128<<100,u128::MAX].into_iter().enumerate() {
            batch.create_vertex(VId(id),vec![],vec![(PropertyKeyId(1),CanonicalScalar::Int(value as i64))]);
        }
        let basis=db.write(&commit,batch).await.unwrap();
        let mut edit=crate::WriteBatch::new(RelationId(1));edit.delete_vertex(VId(17));
        let latest=db.write(&commit,edit).await.unwrap();let view=db.read_session().unwrap();
        let wide=GqlQueryPolicy::new(1000,1000,100_000,100_000);
        for cut in [CommitSeq(0),basis,latest] { for anti in [false,true] {
            let text=format!("MATCH (a) WHERE {}EXISTS {{ MATCH (x) WHERE x.p > a.p }} RETURN a",if anti {"NOT "} else {""});
            let q=PreparedGraphText::prepare(&text,|kind,_:&str|match kind {
                GraphSymbolKind::Property=>Some(GraphSymbol::Property(PropertyKeyId(1))),_=>None,
            }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let plan=VertexScanPlan::compile(q.plan()).unwrap();
            let build=||SnapshotVertexSource {view:view.clone(),cx:&cx,as_of:cut,after:None};
            let mut total=0;let mut good=VertexScanCursor::new(build(),plan.clone(),wide,||{total+=1;Ok::<_,usize>(())});
            let expected=good.by_ref().collect::<Result<Vec<_>,_>>().unwrap();drop(good);
            for stop in 1..=total {
                let pins=Arc::strong_count(&view.snapshot);let mut calls=0;
                let mut c=VertexScanCursor::new(build(),plan.clone(),wide,||{calls+=1;if calls==stop {Err(stop)} else {Ok(())}});
                assert_eq!(Arc::strong_count(&view.snapshot),pins+1);
                let mut prefix=Vec::new();loop {match c.next() {
                    Some(Ok(row))=>prefix.push(row),Some(Err(GqlQueryError::Interrupted(at)))=>{assert_eq!(at,stop);break;},
                    other=>panic!("interruption became {other:?}"),
                }}
                assert_eq!(prefix,expected[..prefix.len()]);assert_eq!(c.row_stats().result_rows,prefix.len() as u64);
                assert_eq!(c.state(),VertexScanState::Failed);assert!(c.next().is_none());
                assert_eq!(Arc::strong_count(&view.snapshot),pins);drop(c);assert_eq!(calls,stop);
                let retry=VertexScanCursor::new(build(),plan.clone(),wide,||Ok::<_,usize>(()));
                assert_eq!(retry.collect::<Result<Vec<_>,_>>().unwrap(),expected);
                assert_eq!(db.frontier().unwrap(),latest);
            }
        } }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
