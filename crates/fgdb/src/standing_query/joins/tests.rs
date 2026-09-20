use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::{GqlParameters, GraphSetColumnType, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000,1000,1_000_000,1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind,name) {
        (GraphSymbolKind::Label,"L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label,"R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property,"k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property,"p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn build(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, at: CommitSeq) -> State {
    let spec=RowJoinSpec::new(&[GraphSetColumnType::Scalar;2],&[GraphSetColumnType::Scalar;2],&[(0,0)]).unwrap();
    let mut state=State { inputs:[0,1],columns:vec!["left.k".into(),"left.p".into(),"right.k".into(),"right.p".into()],
        operator:IncrementalRowJoin::new(spec),last_delta:None,policy:policy(),frontier:at,
        stats:StandingQueryStats::default(),failure:None };
    let mut checkpoint=||Ok(());
    let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut checkpoint};
    state.apply(left,right,&mut meter).unwrap(); state.last_delta=None; state.stats=meter.stats; state
}
fn unchanged(a: &State, b: &State) {
    assert_eq!(a.operator,b.operator); assert_eq!(a.last_delta,b.last_delta);
    assert_eq!(a.frontier,b.frontier); assert_eq!(a.policy,b.policy);
    assert_eq!(a.stats,b.stats); assert_eq!(a.failure,b.failure);
    assert_eq!(a.inputs,b.inputs); assert_eq!(a.columns,b.columns);
}

#[test]
fn every_composed_refusal_preserves_accepted_state_and_baselines_never_become_empty_ticks() {
    let ((),report)=run_async_under_lab(0x726a_0201,|root|async move {
        let contexts=PurposeContexts::narrow_runtime_root(&root);
        let cx=contexts.query();let commit=contexts.commit();
        let keys=DatabaseKeys::new([0x71;32],DatabaseSecurityNamespaceId([0x72;32]),[0x73;32]);
        let mut db=Database::open_memory(&commit,keys).await.unwrap();
        let mut seed=WriteBatch::new(RelationId(1));
        for (id,label,value) in [(1,1,10),(2,2,20)] {
            seed.create_vertex(VId(id),vec![LabelId(label)],vec![(PropertyKeyId(1),CanonicalScalar::Int(1)),
                (PropertyKeyId(2),CanonicalScalar::Int(value))]);
        }
        let basis=db.write(&commit,seed).await.unwrap();
        let definition=|label: &str|PreparedGraphText::prepare(&format!("MATCH (n:{label}) RETURN n.k AS k, n.p AS p"),symbols)
            .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let left=db.register_standing_rows(&cx,definition("L"),policy()).unwrap();
        let right=db.register_standing_rows(&cx,definition("R"),policy()).unwrap();
        assert_eq!([left.index,right.index],[0,1]);
        let copy=|rows:&ZSet<GraphValueRow>|rows.checked_clone(LIMBS,&mut |_|Ok::<_,StandingQueryFailure>(())).unwrap();
        let old_left=copy(db.standing_rows(&cx,&left).unwrap().rows());
        let old_right=copy(db.standing_rows(&cx,&right).unwrap().rows());
        let before=build(&old_left,&old_right,basis);
        let mut next=WriteBatch::new(RelationId(1));
        next.create_vertex(VId(3),vec![LabelId(1)],vec![(PropertyKeyId(1),CanonicalScalar::Int(1)),
            (PropertyKeyId(2),CanonicalScalar::Int(11))]);
        next.set_vertex_property(VId(2),PropertyKeyId(2),Some(CanonicalScalar::Int(21)));
        let at=db.write(&commit,next).await.unwrap();
        let expected=build(&copy(db.standing_rows(&cx,&left).unwrap().rows()),
            &copy(db.standing_rows(&cx,&right).unwrap().rows()),at);
        let batch=db.delta_since(basis).unwrap().next().unwrap();
        let mut success=build(&old_left,&old_right,basis);
        let mut calls=0;
        let stats={
            let mut checkpoint=||{calls+=1;Ok(())};
            let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut checkpoint};
            success.maintain(batch,&db.standing_queries,&mut meter).unwrap();meter.stats
        };
        assert_eq!(success.operator,expected.operator);
        assert!(calls>0&&stats.work_units>0&&stats.scratch_entries>0);
        for stop in 1..=calls {
            let mut state=build(&old_left,&old_right,basis);let mut visited=0;
            {
                let mut checkpoint=||{visited+=1;if visited==stop {Err(StandingQueryFailure::Interrupted)}else{Ok(())}};
                let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut checkpoint};
                assert_eq!(state.maintain(batch,&db.standing_queries,&mut meter),Err(StandingQueryFailure::Interrupted));
            }
            assert_eq!(visited,stop);unchanged(&state,&before);
        }
        for (work,scratch,rows,error) in [
            (stats.work_units,stats.scratch_entries,2,None),
            (stats.work_units-1,stats.scratch_entries,2,Some(StandingQueryFailure::WorkBudget)),
            (stats.work_units,stats.scratch_entries-1,2,Some(StandingQueryFailure::ScratchBudget)),
            (stats.work_units,stats.scratch_entries,1,Some(StandingQueryFailure::ResultBudget)),
        ] {
            let mut state=build(&old_left,&old_right,basis);let mut checkpoint=||Ok(());
            let mut meter=Meter {policy:GqlQueryPolicy::new(1000,rows,work,scratch),
                stats:StandingQueryStats::default(),checkpoint:&mut checkpoint};
            let result=state.maintain(batch,&db.standing_queries,&mut meter);
            if let Some(error)=error {assert_eq!(result,Err(error));unchanged(&state,&before);}
            else {result.unwrap();assert_eq!(state.operator,expected.operator);}
        }
        assert!(matches!(db.prepare_standing_join(&cx,[0,1],&[(0,0)],RowJoinKind::Inner,GqlQueryPolicy::new(2,1000,1_000_000,1_000_000),2),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        assert!(db.prepare_standing_join(&cx,[0,1],&[(0,0)],RowJoinKind::Inner,GqlQueryPolicy::new(3,1000,1_000_000,1_000_000),2).is_ok());
        // Real rebuild creates a new baseline, not a fabricated zero delta.
        db.rebuild_standing_query(&cx,&left,policy()).unwrap();
        let batch=db.delta_since(basis).unwrap().next().unwrap();
        let mut state=build(&old_left,&old_right,basis);let mut checkpoint=||Ok(());
        let mut meter=Meter {policy:policy(),stats:StandingQueryStats::default(),checkpoint:&mut checkpoint};
        assert_eq!(state.maintain(batch,&db.standing_queries,&mut meter),Err(StandingQueryFailure::DependencyUnavailable));
        unchanged(&state,&before);
        assert_eq!(db.frontier().unwrap(),at);
    });
    assert!(report.lab_test_passed(),"{report:?}");
}

#[test]
fn outer_and_presence_registry_publication_is_atomic_at_every_checkpoint() {
    use fgdb_gql::algebra::GraphValue;
    let bag = |key: i64, payload: i64, count: i128| ZSet::from_updates([
        (GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(key)),
            GraphValue::Scalar(CanonicalScalar::Int(payload))]), ZWeight::from_i128(count))],
        LIMBS, &mut |_| Ok::<_, StandingQueryFailure>(())).unwrap();
    let left = bag(1, 10, 2); let right = bag(1, 20, 1);
    let dl = bag(1, 11, 3); let dr = bag(1, 20, -1);
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let seed = || {
            let spec = RowJoinSpec::new(&[GraphSetColumnType::Scalar; 2],
                &[GraphSetColumnType::Scalar; 2], &[(0, 0)]).unwrap().with_kind(kind);
            let mut state = State { inputs: [0, 1], columns: vec![],
                operator: IncrementalRowJoin::new(spec), last_delta: None, policy: policy(),
                frontier: CommitSeq(1), stats: StandingQueryStats::default(), failure: None };
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            state.apply(&left, &right, &mut meter).unwrap();
            state.last_delta = None;
            state
        };
        let before = seed(); let mut complete = seed(); let mut calls = 0;
        {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            complete.apply(&dl, &dr, &mut meter).unwrap();
        }
        assert!(calls > 1 && complete.last_delta.is_some());
        for stop in 1..=calls {
            let mut state = seed(); let mut at = 0;
            {
                let mut checkpoint = || {
                    at += 1;
                    if at == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                };
                let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                assert_eq!(state.apply(&dl, &dr, &mut meter), Err(StandingQueryFailure::Interrupted));
            }
            assert_eq!(at, stop);
            unchanged(&state, &before);
        }
    }
}
