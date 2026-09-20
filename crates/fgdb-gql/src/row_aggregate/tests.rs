use super::*;
use fgdb_types::VId;
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn row(key: Option<i64>, value: Option<i64>, payload: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values([key, value, Some(payload)].into_iter()
        .map(|v| GraphValue::Scalar(v.map_or(CanonicalScalar::Null, CanonicalScalar::Int))).collect())
}
fn bag(rows: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.iter().map(|(row, w)| (row.clone(), ZWeight::from_i128(*w))), LIMBS, &mut allow).unwrap()
}
fn seed(global: bool, input: &ZSet<GraphValueRow>) -> IncrementalRowAggregate {
    let spec = RowAggregateSpec::new(&[GraphSetColumnType::Scalar; 3], if global { &[] } else { &[0] }, 1).unwrap();
    let mut state = IncrementalRowAggregate::new(spec);
    state.prepare(input, LIMBS, None, &mut allow).unwrap().commit();
    state
}
// Independent exact small-integer result signature; includes each summary
// statistic and nullable average numerator/denominator separately.
type Signature = (Vec<GraphValue>, i128, i128, i128, Option<i128>, Option<i128>,
    Option<i128>, Option<i128>, Option<(i128, i128)>, Option<(i128, i128)>);
fn signature(row: &RowAggregateRow) -> Signature {
    let n = |w: &ZWeight| w.to_i128().unwrap();
    (row.keys().to_vec(), n(row.count_rows()), n(row.count_values()), n(row.count_distinct()),
        row.sum().map(n), row.sum_distinct().map(n), row.minimum(), row.maximum(),
        row.average_parts().map(|(a,b)| (n(a), n(b))), row.average_distinct_parts().map(|(a,b)| (n(a),n(b))))
}
fn observed(rows: &ZSet<RowAggregateRow>) -> BTreeMap<Signature, i128> {
    rows.iter().map(|(row,w)| (signature(row), w.to_i128().unwrap())).collect()
}
fn oracle(global: bool, input: &ZSet<GraphValueRow>) -> BTreeMap<Signature, i128> {
    let mut groups: BTreeMap<Vec<GraphValue>, (i128, Vec<(i128,i128)>)> = BTreeMap::new();
    if global { groups.insert(vec![], (0,vec![])); }
    for (row, weight) in input.iter() {
        let weight = weight.to_i128().unwrap();
        let group = groups.entry(if global { vec![] } else { vec![row.values()[0].clone()] }).or_default();
        group.0 += weight;
        if let GraphValue::Scalar(CanonicalScalar::Int(value)) = &row.values()[1] {
            group.1.push((i128::from(*value),weight));
        }
    }
    groups.into_iter().map(|(key,(rows,values))| {
        let count: i128 = values.iter().map(|(_,w)| w).sum();
        let sum: i128 = values.iter().map(|(v,w)| v*w).sum();
        let distinct: std::collections::BTreeSet<_> = values.iter().map(|(v,_)| *v).collect();
        let ds: i128 = distinct.iter().sum(); let dc = distinct.len() as i128;
        ((key, rows, count, dc, (count>0).then_some(sum), (dc>0).then_some(ds),
            distinct.first().copied(), distinct.last().copied(),
            (count>0).then_some((sum,count)), (dc>0).then_some((ds,dc))),1)
    }).collect()
}
fn difference(new: &BTreeMap<Signature,i128>, old: &BTreeMap<Signature,i128>) -> BTreeMap<Signature,i128> {
    let mut result = new.clone();
    for (row,w) in old { *result.entry(row.clone()).or_default() -= w; }
    result.retain(|_,w| *w!=0); result
}

#[test]
fn all_13122_grouped_and_global_transitions_match_independent_full_summaries() {
    let choices = [row(Some(0),None,1), row(Some(0),Some(-3),2),
        row(None,Some(7),3), row(None,Some(7),4)];
    for global in [false,true] {
        for mut code in 0..6561_u32 {
            let mut weights = [0_i128;8];
            for w in &mut weights { *w=i128::from(code%3); code/=3; }
            let make = |at| bag(&(0..4).map(|i| (choices[i].clone(),weights[at+i])).collect::<Vec<_>>());
            let before=make(0); let after=make(4);
            let delta=after.minus(&before,LIMBS,&mut allow).unwrap();
            let mut state=seed(global,&before);
            let expected_before=oracle(global,&before); let expected_after=oracle(global,&after);
            assert_eq!(observed(state.rows()),expected_before);
            {
                let pending=state.prepare(&delta,LIMBS,None,&mut allow).unwrap();
                assert_eq!(observed(pending.delta()),difference(&expected_after,&expected_before));
            }
            assert_eq!(state,seed(global,&before));
            state.prepare(&delta,LIMBS,None,&mut allow).unwrap().commit();
            assert_eq!(observed(state.rows()),expected_after);
            state.prepare(&delta.negated(LIMBS,&mut allow).unwrap(),LIMBS,None,&mut allow).unwrap().commit();
            assert_eq!(state,seed(global,&before));
        }
    }
}

#[test]
fn invalid_raw_retractions_and_nonnumeric_values_cannot_hide_in_projection() {
    let before=bag(&[(row(Some(0),Some(7),1),2)]);
    let mut state=seed(false,&before);
    // Same projected group/value, zero projected delta, invalid raw row.
    let invalid=bag(&[(row(Some(0),Some(7),99),-1),(row(Some(0),Some(7),1),1)]);
    assert_eq!(state.prepare(&invalid,LIMBS,None,&mut allow).unwrap_err(),RowAggregateError::NegativeMultiplicity);
    assert_eq!(state,seed(false,&before));
    let bad=GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(0)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("7").unwrap()),GraphValue::Scalar(CanonicalScalar::Int(1))]);
    assert_eq!(state.prepare(&bag(&[(bad,1)]),LIMBS,None,&mut allow).unwrap_err(),
        RowAggregateError::NonIntegerValue {column:1});
    assert_eq!(state,seed(false,&before));
    let wrong=GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]);
    assert_eq!(state.prepare(&bag(&[(wrong,1)]),LIMBS,None,&mut allow).unwrap_err(),RowAggregateError::InputSchema);
}

#[test]
fn every_control_boundary_and_downstream_unwind_leave_input_support_and_output_unchanged() {
    let before=bag(&[(row(Some(0),Some(9),1),2),(row(Some(0),Some(-3),2),1)]);
    let change=bag(&[(row(Some(0),Some(9),1),-2),(row(Some(1),None,3),4)]);
    for global in [false,true] {
        let expected=seed(global,&before); let mut complete=seed(global,&before);
        let mut counts=[0_usize;2]; let mut calls=0;
        complete.prepare(&change,LIMBS,None,&mut |event| {
            counts[usize::from(event==ZSetEvent::ScratchEntry)]+=1;calls+=1;Ok::<_,usize>(())
        }).unwrap().commit();
        assert!(counts.iter().all(|n| *n>0));
        for stop in 1..=calls {
            let mut state=seed(global,&before);let mut at=0;
            assert_eq!(state.prepare(&change,LIMBS,None,&mut |_| {
                at+=1;if at==stop {Err(stop)}else{Ok(())}
            }).unwrap_err(),RowAggregateError::Delta(ZSetError::Control(stop)));
            assert_eq!(at,stop);assert_eq!(state,expected);
        }
        for dimension in 0..2 {
            for below in [false,true] {
                let mut state=seed(global,&before);let mut used=[0;2];
                let limit=counts[dimension]-usize::from(below);
                let result=state.prepare(&change,LIMBS,None,&mut |event| {
                    let at=usize::from(event==ZSetEvent::ScratchEntry);used[at]+=1;
                    if at==dimension&&used[at]>limit {Err(at)}else{Ok(())}
                }).map(RowAggregateUpdate::commit);
                if below {assert!(result.is_err());assert_eq!(state,expected);}
                else {result.unwrap();assert_eq!(state,complete);}
            }
        }
        let mut state=seed(global,&before);
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending=state.prepare(&change,LIMBS,None,&mut allow).unwrap();
            panic!("downstream refusal");
        }));
        assert!(result.is_err());assert_eq!(state,expected);
    }
}

#[test]
fn global_empty_null_only_and_final_group_limits_are_distinct() {
    let mut state=seed(true,&ZSet::new());
    let empty=state.rows().iter().next().unwrap().0;
    assert_eq!(empty.count_rows(),&ZWeight::ZERO);assert!(empty.sum().is_none());
    assert!(state.prepare(&ZSet::new(),LIMBS,Some(0),&mut allow).is_err());
    let values=bag(&[(row(Some(1),None,1),3)]);
    state.prepare(&values,LIMBS,Some(1),&mut allow).unwrap().commit();
    let nulls=state.rows().iter().next().unwrap().0;
    assert_eq!(nulls.count_rows(),&ZWeight::from_i128(3));assert_eq!(nulls.count_values(),&ZWeight::ZERO);
    assert!(nulls.sum().is_none());assert!(nulls.minimum().is_none());
    state.prepare(&values.negated(LIMBS,&mut allow).unwrap(),LIMBS,Some(1),&mut allow).unwrap().commit();
    assert_eq!(state,seed(true,&ZSet::new()));
    let old=bag(&[(row(Some(9),Some(1),1),1)]);let new=bag(&[(row(Some(0),Some(2),2),1)]);
    let mut state=seed(false,&old);let delta=new.minus(&old,LIMBS,&mut allow).unwrap();
    state.prepare(&delta,LIMBS,Some(1),&mut allow).unwrap().commit();
    assert_eq!(observed(state.rows()),oracle(false,&new));
}

#[test]
fn native_group_domains_and_wide_integer_statistics_never_narrow() {
    let types=[GraphSetColumnType::Vertex,GraphSetColumnType::Scalar];
    let spec=RowAggregateSpec::new(&types,&[0],1).unwrap();
    let input=GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Int(i64::MAX))]);
    let delta=bag(&[(input,i128::MAX)]);
    let mut state=IncrementalRowAggregate::new(spec.clone());
    assert!(matches!(state.prepare(&delta,LimbLimit::new(0),None,&mut allow),
        Err(RowAggregateError::Delta(ZSetError::Arithmetic(_)))));
    assert_eq!(state,IncrementalRowAggregate::new(spec));
    state.prepare(&delta,LIMBS,Some(1),&mut allow).unwrap().commit();
    state.prepare(&delta,LIMBS,Some(1),&mut allow).unwrap().commit();
    let result=state.rows().iter().next().unwrap().0;
    assert_eq!(result.keys(),&[GraphValue::Vertex(VId(u128::MAX))]);
    assert!(result.count_rows().is_promoted());assert!(result.sum().unwrap().is_promoted());
    assert_eq!(result.count_distinct(),&ZWeight::ONE);
    assert_eq!(result.minimum(),Some(i128::from(i64::MAX)));
    assert_eq!(result.sum_distinct().unwrap(),&ZWeight::from_i128(i128::from(i64::MAX)));
    assert_eq!(result.average_parts().unwrap().1,result.count_values());
}

#[test]
fn admission_and_changed_group_locality_are_explicit() {
    use GraphSetColumnType::{Scalar,Vertex,List};
    assert_eq!(RowAggregateSpec::new(&[],&[],0),Err(RowAggregateBuildError::EmptyInput));
    assert_eq!(RowAggregateSpec::new(&[Vertex],&[],0),Err(RowAggregateBuildError::RequiresScalarArgument));
    assert_eq!(RowAggregateSpec::new(&[Scalar,List],&[],0),Err(RowAggregateBuildError::UnsupportedColumn {column:1}));
    assert_eq!(RowAggregateSpec::new(&[Scalar],&[0,0],0),Err(RowAggregateBuildError::DuplicateKey {column:0}));
    assert_eq!(RowAggregateSpec::new(&[Scalar],&[1],0),Err(RowAggregateBuildError::UnknownColumn {column:1}));
    assert_eq!(RowAggregateSpec::new(&[Scalar],&[],1),Err(RowAggregateBuildError::UnknownColumn {column:1}));
    let run=|n:i64| {
        let all=bag(&(0..n).map(|i|(row(Some(i),Some(i),i),2)).collect::<Vec<_>>());
        let mut state=seed(false,&all);let mut counts=[0;2];
        let delta=state.prepare(&bag(&[(row(Some(0),Some(0),0),-1)]),LIMBS,None,&mut |event| {
            counts[usize::from(event==ZSetEvent::ScratchEntry)]+=1;Ok::<_,usize>(())
        }).unwrap().commit();
        (counts,observed(&delta))
    };
    assert_eq!(run(2),run(2048));
}
