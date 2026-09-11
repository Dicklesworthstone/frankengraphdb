//! Independent occurrence enumeration for hidden cyclic factor elimination.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

type Edge = (VId, RelationId, VId);
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(match name {
            "R" => 1, "S" => 2, "T" => 3, "U" => 4, _ => 5,
        }))),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 4_000_000, 1_000_000) }

#[test]
fn cycles_parallel_constraints_self_loops_and_output_aliases_match_assignments() {
    let domain = [VId(0), VId(u128::MAX)];
    let universe: Vec<_> = [RelationId(2), RelationId(3)].into_iter().flat_map(|r|
        domain.into_iter().flat_map(move |a| domain.into_iter().map(move |b| (a,r,b)))).collect();
    for direction in 0..3 {
        let edge = match direction { 0=>"-[:S]->", 1=>"<-[:S]-", _=>"-[:S]-" };
        for repeats in [false,true] {
            let head = format!("MATCH (a)-[:R]->(b){edge}(x)-[:T]->(y){edge}(b){}",
                if repeats { ",(b)-[:S]->(x)" } else { "" });
            // Avoid mixed undirected/directed relation mode in the repeated case.
            if repeats && direction == 2 { continue; }
            for project_x in [false,true] {
                let keys = if project_x { "a,x" } else { "a,b" };
                let query=prepare(&format!("{head} RETURN {keys},COUNT(*) AS n GROUP BY {keys}"));
                let frozen=query.canonical_bytes();
                for mask in 0..256 {
                    let mut edges=vec![(VId(0),RelationId(1),VId(0)),(VId(u128::MAX),RelationId(1),VId(u128::MAX))];
                    edges.extend(universe.iter().enumerate().filter(|(at,_)|mask & (1<<at)!=0).map(|(_,edge)|*edge));
                    if edges.len()>2 { edges.push(edges[2]); }
                    let count=|a,r,b,d| edges.iter().filter(|&&(s,t,e)|t==RelationId(r) && match d {
                        0=>s==a && e==b, 1=>e==a && s==b,
                        _=>(s==a && e==b)||(e==a && s==b),
                    }).count() as u64;
                    let mut expected=BTreeMap::new();
                    for a in domain { for b in domain { for x in domain { for y in domain {
                        let mut n=count(a,1,b,0)*count(b,2,x,direction)*count(x,3,y,0)*count(y,2,b,direction);
                        if repeats { n*=count(b,2,x,0); }
                        if n>0 { *expected.entry((a,if project_x{x}else{b})).or_insert(0)+=n; }
                    }}}}
                    let result=query.execute_governed(edges.len() as u64,[],edges,
                        |_,_|Ok::<_,()>(true),|_,_|Ok(None),wide(),||Ok::<_,()>(())).unwrap();
                    let actual:BTreeMap<_,_>=result.value.iter().map(|row|
                        ((row.keys()[0].as_vertex().unwrap(),row.keys()[1].as_vertex().unwrap()),row.get(0).unwrap().as_count().unwrap())).collect();
                    assert_eq!(actual,expected,"mask={mask} direction={direction} repeats={repeats} projected={project_x}");
                    assert_eq!(query.canonical_bytes(),frozen);
                }
            }
        }
    }
}

fn lobes(n:usize,output:&str)->String {
    let mut text="MATCH (a)-[:R]->(b)".to_owned();
    for at in 0..n { text.push_str(&format!(",(b)-[:S]->(x{at})-[:T]->(y{at})-[:U]->(b)")); }
    text.push_str(output);text
}
fn complete_lobe()->Vec<Edge> {
    let mut edges=vec![(VId(0),RelationId(1),VId(1))];
    for a in 2..10 { edges.push((VId(1),RelationId(2),VId(a)));
        for b in 10..18 { edges.push((VId(a),RelationId(3),VId(b))); }
    }
    for b in 10..18 { edges.push((VId(b),RelationId(4),VId(1))); }
    edges
}

#[test]
fn eight_hidden_cycles_count_hundreds_of_trillions_without_enumerating_them() {
    let query=prepare(&lobes(8," RETURN a,COUNT(*) AS n,COUNT(DISTINCT b) AS d GROUP BY a"));
    let edges=complete_lobe();
    let result=query.execute_governed(81,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),
        GqlQueryPolicy::new(81,1,1_000_000,100_000),||Ok::<_,()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(64_u64.pow(8)));
    assert_eq!(result.value[0].get(1).unwrap().as_count(),Some(1));
    assert_eq!(result.rows.snapshot_records,81);
}

#[test]
fn cyclic_overflow_is_not_zero_and_support_does_not_require_representable_counts() {
    let edges=complete_lobe();
    for support in [false,true] {
        let query=prepare(&lobes(11,if support {" RETURN COUNT(DISTINCT a) AS n"} else {" RETURN COUNT(*) AS n"}));
        let result=query.execute_governed(81,[],edges.iter().copied(),|_,_|Ok::<_,()>(true),|_,_|Ok(None),wide(),||Ok::<_,()>(()));
        if support {assert_eq!(result.unwrap().value[0].get(0).unwrap().as_count(),Some(1));}
        else {assert!(matches!(result,Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow{aggregate:0}))));}
    }
    let query=prepare(&lobes(11," WHERE a=b RETURN COUNT(*) AS n"));
    let result=query.execute_governed(81,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),wide(),||Ok::<_,()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(0));
}

#[test]
fn cyclic_compilation_tables_and_output_share_exact_limits_and_interruption() {
    let query=prepare("MATCH (a)-[:R]->(b)-[:S]->(x)-[:T]->(y)-[:U]->(b) RETURN a,COUNT(*) AS n GROUP BY a");
    let edges=[(VId(0),RelationId(1),VId(1)),(VId(1),RelationId(2),VId(2)),
        (VId(2),RelationId(3),VId(3)),(VId(3),RelationId(4),VId(1))];
    let mut total=0;
    let measured=query.execute_governed(4,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),wide(),||{
        total+=1;Ok::<_,usize>(())
    }).unwrap();
    let exact=GqlQueryPolicy::new(4,1,measured.evaluator.work_units,measured.evaluator.scratch_entries);
    assert_eq!(query.execute_governed(4,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),exact,||Ok::<_,usize>(())).unwrap(),measured);
    for cap in [GqlQueryPolicy::new(4,0,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(4,1,measured.evaluator.work_units-1,u64::MAX),
        GqlQueryPolicy::new(4,1,u64::MAX,measured.evaluator.scratch_entries-1)] {
        assert!(query.execute_governed(4,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),cap,||Ok::<_,usize>(())).is_err());
    }
    for stop in 1..=total {
        let mut at=0;
        let result=query.execute_governed(4,[],edges,|_,_|Ok::<_,()>(true),|_,_|Ok(None),wide(),||{
            at+=1;if at==stop {Err(stop)} else {Ok(())}
        });
        assert!(matches!(result,Err(GqlQueryError::Interrupted(n)) if n==stop));assert_eq!(at,stop);
    }
}

#[test]
fn property_projection_keeps_each_fallible_occurrence_on_the_original_path() {
    let query=prepare("MATCH (a)-[:R]->(b)-[:S]->(x)-[:T]->(b) RETURN COUNT(b.p) AS n");
    let edges=[(VId(0),RelationId(1),VId(1)),(VId(0),RelationId(1),VId(1)),
        (VId(1),RelationId(2),VId(2)),(VId(2),RelationId(3),VId(1))];
    let scalar=CanonicalScalar::Int(7);let mut calls=0;
    let result=query.execute_governed(4,[],edges,|_,_|Ok::<_,&str>(true),|_,_|{
        calls+=1;if calls==2 {Err("second occurrence")} else {Ok(Some(&scalar))}
    },wide(),||Ok::<_,()>(()));
    assert!(matches!(result,Err(GqlQueryError::Source(GraphAggregateError::Source("second occurrence")))));
    assert_eq!(calls,2);
}
