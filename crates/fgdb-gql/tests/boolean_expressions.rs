//! Boolean expressions share GLA traversal, nullable scopes and aggregation.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphBooleanExpression, GraphBooleanOp as Op,
    GraphBooleanOperand as Arg, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison as Cmp, PatternBuildError, PreparedGraphPattern};
use fgdb_gql::{GraphAggregate, GqlQueryError, GqlQueryPolicy, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000) }
fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new(); for name in names { b.vertex(name).unwrap(); } b
}
fn condition(left: &str, right: &str) -> GraphBooleanExpression {
    GraphBooleanExpression::prepare(&[Op::Compare {
        left: Arg::Property { variable: left, key: P }, comparison: Cmp::Less,
        right: Arg::Property { variable: right, key: P },
    }, Op::IsNull { operand: Arg::Property { variable: right, key: P }, is_null: true }, Op::Or]).unwrap()
}
fn columns() -> [GraphColumn<'static>; 2] { [GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")] }
fn run(pattern: &PreparedGraphPattern<GraphValueRow>, values: &[Option<CanonicalScalar>], edges: &[(VId, RelationId, VId)]) -> Vec<(VId,VId)> {
    pattern.plan().execute_governed_with_properties(edges.len() as u64, [], edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(values[vid.0 as usize].as_ref()), wide(), || Ok::<_, ()>(())).unwrap()
        .value.iter().map(|row| (row.get(0).unwrap().as_vertex().unwrap(), row.get(1).unwrap().as_vertex().unwrap())).collect()
}

#[test]
fn cross_vertex_disjunction_matches_independent_multigraph_and_scalar_oracle() {
    let choices = [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::Int(2)), Some(CanonicalScalar::Bool(true))];
    let mut b = builder(&["a", "b"]); b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    b.filter_boolean(&condition("a", "b")).unwrap();
    let query = b.prepare_values(&columns(), 0, None).unwrap().with_duplicates();
    let universe = [(VId(0),R,VId(0)),(VId(0),R,VId(1)),(VId(1),R,VId(0)),(VId(1),R,VId(1))];
    for left in &choices { for right in &choices { for mask in 0..16 {
        let values = [left.clone(), right.clone()];
        let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at,_)| mask & (1 << at) != 0).map(|(_,e)| *e).collect();
        if let Some(first) = edges.first().copied() { edges.push(first); }
        let mut expected = Vec::new();
        for &(a,_,b) in &edges {
            let null = values[b.0 as usize].as_ref().is_none_or(|v| matches!(v,CanonicalScalar::Null));
            let less = match (&values[a.0 as usize],&values[b.0 as usize]) {
                (Some(CanonicalScalar::Int(a)),Some(CanonicalScalar::Int(b))) => a < b, _ => false,
            };
            if null || less { expected.push((a,b)); }
        }
        expected.sort(); assert_eq!(run(&query,&values,&edges),expected);
    }}}
}

#[test]
fn scope_remapping_preserves_optional_and_existence_witnesses_and_aggregation() {
    let outer = builder(&["a"]); let mut child = builder(&["b","a"]);
    child.edge("b", R, GlaDirection::Reverse, "a").unwrap();
    child.filter_boolean(&condition("a","b")).unwrap();
    let values = [CanonicalScalar::Int(5),CanonicalScalar::Int(8),CanonicalScalar::Int(1),CanonicalScalar::Int(10)];
    let edges = [(VId(0),R,VId(1)),(VId(0),R,VId(1)),(VId(0),R,VId(2)),(VId(3),R,VId(2))];
    let query = outer.prepare_values_with_clauses(&[GraphMatchClause::optional(&child)],&columns(),0,None).unwrap().with_duplicates();
    let rows = query.plan().execute_governed_with_properties(6,[VId(0),VId(3)],edges,
        |_,_|Ok::<_,()>(true),|vid,_|Ok(Some(&values[vid.0 as usize])),wide(),||Ok::<_,()>(())).unwrap().value;
    assert_eq!(rows.len(),3); assert_eq!(rows[0],rows[1]);
    assert_eq!(rows[0].get(1).unwrap().as_vertex(),Some(VId(1))); assert!(rows[2].get(1).unwrap().is_null());
    let aggregate = PreparedGraphAggregate::prepare(query,&[0],&[GraphAggregate::count_rows("n"),GraphAggregate::count("present",1)],0,None).unwrap();
    let result = aggregate.execute_governed(6,[VId(0),VId(3)],edges,|_,_|Ok::<_,()>(true),
        |vid,_|Ok(Some(&values[vid.0 as usize])),wide(),||Ok::<_,()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(),Some(2));
    assert_eq!(result.value[1].get(1).unwrap().as_count(),Some(0));
    for anti in [false,true] {
        let clause = if anti { GraphMatchClause::not_exists(&child) } else { GraphMatchClause::exists(&child) };
        let query = outer.prepare_values_with_clauses(&[clause],&columns()[..1],0,None).unwrap();
        let rows = query.plan().execute_governed_with_properties(6,[VId(0),VId(3)],edges,
            |_,_|Ok::<_,()>(true),|vid,_|Ok(Some(&values[vid.0 as usize])),wide(),||Ok::<_,()>(())).unwrap().value;
        assert_eq!(rows.len(),1); assert_eq!(rows[0].get(0).unwrap().as_vertex(),Some(VId(if anti {3}else{0})));
    }
}

#[test]
fn invalid_attachment_leaves_builder_unchanged_and_requires_a_value_source() {
    let mut b = builder(&["a"]); let original = b.prepare_values(&columns()[..1],0,None).unwrap();
    assert_eq!(b.filter_boolean(&condition("a","missing")).unwrap_err(),PatternBuildError::UnknownVariable);
    assert_eq!(b.prepare_values(&columns()[..1],0,None).unwrap(),original);
    b.filter_boolean(&GraphBooleanExpression::prepare(&[Op::Truth(Some(true))]).unwrap()).unwrap();
    assert_eq!(b.prepare("a",0,None).unwrap_err(),PatternBuildError::RequiresValueProjection);
    for _ in 1..256 { b.filter_boolean(&GraphBooleanExpression::prepare(&[Op::Truth(Some(true))]).unwrap()).unwrap(); }
    let before = b.prepare_values(&columns()[..1],0,None).unwrap();
    assert!(matches!(b.filter_boolean(&GraphBooleanExpression::prepare(&[Op::Truth(None)]).unwrap()),Err(PatternBuildError::LimitExceeded { .. })));
    assert_eq!(b.prepare_values(&columns()[..1],0,None).unwrap(),before);
}

#[test]
fn expression_execution_shares_limits_and_never_erases_source_or_interrupt_errors() {
    let mut b = builder(&["a","b"]); b.edge("a",R,GlaDirection::Forward,"b").unwrap();
    b.filter_boolean(&condition("a","b")).unwrap();
    let query = b.prepare_values(&columns(),0,Some(1)).unwrap().with_duplicates();
    let values=[CanonicalScalar::Int(0),CanonicalScalar::Int(2)];
    let edges=[(VId(0),R,VId(1));2];
    let mut total=0;
    let measured=query.plan().execute_governed_with_properties(2,[],edges,|_,_|Ok::<_,()>(true),
        |vid,_|Ok(Some(&values[vid.0 as usize])),wide(),||{total+=1;Ok::<_,usize>(())}).unwrap();
    let exact=GqlQueryPolicy::new(2,1,measured.evaluator.work_units,measured.evaluator.scratch_entries);
    let run=|cap|query.plan().execute_governed_with_properties(2,[],edges,|_,_|Ok::<_,()>(true),
        |vid,_|Ok(Some(&values[vid.0 as usize])),cap,||Ok::<_,usize>(()));
    assert_eq!(run(exact).unwrap(),measured);
    for cap in [GqlQueryPolicy::new(2,0,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(2,1,measured.evaluator.work_units-1,u64::MAX),
        GqlQueryPolicy::new(2,1,u64::MAX,measured.evaluator.scratch_entries-1)] { assert!(run(cap).is_err()); }
    for stop in 1..=total {
        let mut at=0;
        let result=query.plan().execute_governed_with_properties(2,[],edges,|_,_|Ok::<_,()>(true),
            |vid,_|Ok(Some(&values[vid.0 as usize])),wide(),||{at+=1;if at==stop{Err(stop)}else{Ok(())}});
        assert!(matches!(result,Err(GqlQueryError::Interrupted(value)) if value==stop)); assert_eq!(at,stop);
    }
    let result=query.plan().execute_governed_with_properties(2,[],edges,|_,_|Ok::<_,&str>(true),
        |_,_|Err("source"),wide(),||Ok::<_,()>(()));
    assert!(matches!(result,Err(GqlQueryError::Source("source"))));
}
