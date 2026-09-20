use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GqlParameters, GraphSetColumnType, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId};

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000,100_000,1_000_000,1_000_000) }
fn same(a: &State, b: &State) {
    assert_eq!(a.operator,b.operator);assert_eq!(a.last_delta,b.last_delta);
    assert_eq!(a.frontier,b.frontier);assert_eq!(a.failure,b.failure);assert_eq!(a.stats,b.stats);
}
#[test]
fn every_composed_checkpoint_and_exact_quota_boundary_preserves_the_accepted_generation() {
    let ((),report)=run_async_under_lab(0x7072_6410,|root| async move {
        let c=fgdb_types::PurposeContexts::narrow_runtime_root(&root);let cx=c.query();let commit=c.commit();
        let keys=DatabaseKeys::new([0x61;32],DatabaseSecurityNamespaceId([0x62;32]),[0x63;32]);
        let mut db=Database::open_memory(&commit,keys).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1),vec![],vec![(PropertyKeyId(1),CanonicalScalar::Int(4))]);
        db.write(&commit,seed).await.unwrap();
        let definition=PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p",|kind,name:&str| match (kind,name) {
            (GraphSymbolKind::Property,"p")=>Some(GraphSymbol::Property(PropertyKeyId(1))), _=>None
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let parent=db.register_standing_rows(&cx,definition,policy()).unwrap();
        let baseline=db.standing_rows(&cx,&parent).unwrap().rows().checked_clone(LIMBS,&mut |_|Ok::<_,()>(())).unwrap();
        let basis=db.frontier().unwrap();
        let build=|| {
            let spec=RowProjectionSpec::new(vec![GraphSetColumnType::Scalar],
                vec![GraphSetProjection::new("p",GraphSetValue::Column(0))],GraphSetQuantifier::Distinct).unwrap();
            let mut state=State {input:parent.index,columns:vec!["p".into()],operator:IncrementalRowProjection::new(spec),
                last_delta:None,policy:policy(),frontier:basis,stats:StandingQueryStats::default(),failure:None};
            let mut check=||Ok(());let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut check};
            state.prepare_and_publish(&baseline,&mut meter).unwrap();state.last_delta=None;state
        };
        let before=build();let mut change=WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1),PropertyKeyId(1),Some(CanonicalScalar::Int(9)));
        db.write(&commit,change).await.unwrap();let batch=db.delta_since(basis).unwrap().next().unwrap();
        let mut success=build();let mut calls=0;
        let stats={let mut check=||{calls+=1;Ok(())};let mut meter=Meter {
            policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut check};
            success.maintain(batch,&db.standing_queries,&mut meter).unwrap();meter.stats};
        for stop in 1..=calls {
            let mut state=build();let mut seen=0;let mut check=||{seen+=1;if seen==stop {Err(StandingQueryFailure::Interrupted)}else{Ok(())}};
            let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut check};
            assert_eq!(state.maintain(batch,&db.standing_queries,&mut meter),Err(StandingQueryFailure::Interrupted));
            same(&state,&before);
        }
        for (work,scratch,expected) in [(stats.work_units,stats.scratch_entries,None),
            (stats.work_units-1,stats.scratch_entries,Some(StandingQueryFailure::WorkBudget)),
            (stats.work_units,stats.scratch_entries-1,Some(StandingQueryFailure::ScratchBudget))] {
            let mut state=build();let mut check=||Ok(());let mut meter=Meter {
                policy:GqlQueryPolicy::new(100,1,work,scratch),stats:StandingQueryStats::default(),checkpoint:&mut check};
            let result=state.maintain(batch,&db.standing_queries,&mut meter);
            if let Some(error)=expected {assert_eq!(result,Err(error));same(&state,&before);}
            else {result.unwrap();assert_eq!(state.operator,success.operator);assert_eq!(state.last_delta,success.last_delta);}
        }
    });
    assert!(report.lab_test_passed(),"{report:?}");
}
