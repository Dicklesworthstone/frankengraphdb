use super::*;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText};

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(match name {
            "R" => 1, "S" => 2, "T" => 3, "U" => 4, _ => 5,
        }))),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
        GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1))),
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, resolve).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, resolve).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }

fn projected(columns: &[ValueProjection], bindings: &[Option<VId>]) -> Vec<VId> {
    columns.iter().map(|column| {
        let ValueProjection::Vertex { slot } = column else { panic!("vertex fixture"); };
        bindings[slot.ordinal() as usize].unwrap()
    }).collect()
}
fn collect(plan: &GlaPlan<GraphValueRow>, topology: &BTreeMap<TopologyKey, Multiplicity>, optimized: bool)
    -> Vec<Vec<VId>> {
    let mut result = Vec::new();
    if optimized {
        visit_bindings(plan, [], topology,
            |_, _| Ok::<_, VisitError<(), ()>>(true),
            |_, _| Ok(None), |_| Ok(()),
            |columns, bindings, _, _| { result.push(projected(columns, bindings)); Ok(()) }).unwrap();
    } else {
        plan.visit_value_bindings([], topology.keys().copied(),
            |_, _| Ok::<_, VisitError<(), ()>>(true),
            |_, _| Ok(None), |_| Ok(()),
            |columns, bindings, _, _| { result.push(projected(columns, bindings)); Ok(()) }).unwrap();
    }
    result.sort();
    result
}

#[test]
fn triangle_intersections_match_independent_assignments_and_the_original_visitor() {
    for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        let arrow = match direction { GlaDirection::Forward => "-[:R]->", GlaDirection::Reverse => "<-[:R]-", _ => "-[:R]-" };
        for unequal in [false, true] {
            let q = pattern(&format!("MATCH (a){arrow}(b){arrow}(c){arrow}(a) {} RETURN a,b,c",
                if unequal { "WHERE a<>b AND b<>c AND a<>c" } else { "" }));
            assert!(Shape::compile(q.plan()).is_some());
            for mask in 0..512 {
                let mut topology = BTreeMap::new();
                for left in 0..3 {
                    for right in 0..3 {
                        if mask & (1 << (3 * left + right)) != 0 {
                            topology.insert(normalized(VId(left), RelationId(1), VId(right),
                                direction == GlaDirection::Undirected), Multiplicity::ONE);
                        }
                    }
                }
                let present = |from, to| topology.contains_key(&match direction {
                    GlaDirection::Forward => (VId(from), RelationId(1), VId(to)),
                    GlaDirection::Reverse => (VId(to), RelationId(1), VId(from)),
                    _ => normalized(VId(from), RelationId(1), VId(to), true),
                });
                // Enumerate vertex assignments, not trie candidates or GLA operators.
                let mut expected = Vec::new();
                for a in 0..3 { for b in 0..3 { for c in 0..3 {
                    if (!unequal || (a != b && b != c && a != c))
                        && present(a,b) && present(b,c) && present(c,a) {
                        expected.push(vec![VId(a), VId(b), VId(c)]);
                    }
                }}}
                assert_eq!(collect(q.plan(), &topology, true), expected);
                assert_eq!(collect(q.plan(), &topology, false), expected);
            }
        }
    }
}

#[test]
fn cliques_parallel_constraints_loops_aliases_and_reversed_atoms_share_one_binding_assignment() {
    let topology: BTreeMap<_, _> = [0, 1, 1_u128 << 100, u128::MAX].into_iter().flat_map(|a|
        [0, 1, 1_u128 << 100, u128::MAX].into_iter().flat_map(move |b|
            (1..=5).filter(move |relation| (a ^ b ^ u128::from(*relation)) & 3 != 0)
                .map(move |relation| ((VId(a), RelationId(relation), VId(b)), Multiplicity::ONE))))
        .collect();
    for input in [
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(a)-[:U]->(d)-[:S]->(c),(b)-[:T]->(d) RETURN a,b,c,d",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(a)-[:R]->(b),(c)-[:U]->(c) RETURN c,a,b",
        "MATCH (a)<-[:R]-(b)-[:S]->(c)<-[:T]-(a) WHERE a<>c RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a=a AND a<>a RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(b)-[:U]->(d) RETURN a,b,c,d",
    ] {
        let q = pattern(input);
        assert!(Shape::compile(q.plan()).is_some(), "{input}");
        assert_eq!(collect(q.plan(), &topology, true), collect(q.plan(), &topology, false), "{input}");
    }
}

#[test]
fn only_pure_cyclic_support_uses_intersection_and_fallback_keeps_its_event_trace() {
    for input in [
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(a) RETURN a,b",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN DISTINCT a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.p=1 RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a.p,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a,b,c LIMIT 0",
    ] { assert!(Shape::compile(pattern(input).plan()).is_none(), "{input}"); }
    let q = pattern("MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a,b,c");
    let topology = BTreeMap::from([((VId(0),RelationId(1),VId(1)),Multiplicity::ONE)]);
    let mut direct_events = Vec::new();
    q.plan().visit_value_bindings([], topology.keys().copied(),
        |_,_| Ok::<_,VisitError<(),()>>(true), |_,_| Ok(None),
        |event| { direct_events.push(event); Ok(()) }, |_,_,_,_| Ok(())).unwrap();
    let mut events = Vec::new();
    visit_bindings(q.plan(), [], &topology, |_,_| Ok::<_,VisitError<(),()>>(true), |_,_| Ok(None),
        |event| { events.push(event); Ok(()) }, |_,_,_,_| Ok(())).unwrap();
    assert_eq!(events, direct_events);
}

#[test]
fn weighted_counts_groups_and_support_summaries_equal_unfactored_bags() {
    let mut edges = Vec::new();
    for a in 0..3 { for b in 0..3 {
        for relation in 1..=3 {
            for _ in 0..((a + 2*b + u128::from(relation)) % 3) {
                edges.push((VId(a),RelationId(relation),VId(b)));
            }
        }
    }}
    for body in [
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a)",
        "(a)<-[:R]-(b)-[:S]-(c)-[:T]->(a)",
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a<>c",
    ] {
        let prefix = format!("MATCH {body} RETURN a,b,c,COUNT(*) AS n,COUNT(c) AS m,COUNT(DISTINCT c) AS d,MIN(c) AS lo,MAX(c) AS hi");
        let q = aggregate(&format!("{prefix} GROUP BY a,b,c"));
        let ordinary = aggregate(&format!("{prefix},COLLECT(c) AS ignored GROUP BY a,b,c"))
            .with_aggregate_output_prefix(5).unwrap();
        let run = |q: &PreparedGraphAggregate| q.execute_governed(edges.len() as u64, (0..3).map(VId), edges.iter().copied(),
            |_,_| Ok::<_,()>(true), |_,_| Ok(None), wide(), || Ok::<_,()>(())).unwrap();
        assert_eq!(run(&q).value, run(&ordinary).value);
    }
}

#[test]
fn every_trie_build_seek_and_complete_binding_refusal_retries_without_partial_aggregate() {
    let q = aggregate("MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c");
    let edges: Vec<_> = (0..3).flat_map(|a| (0..3).flat_map(move |b|
        (1..=3).map(move |r| (VId(a),RelationId(r),VId(b))))).collect();
    let mut total = 0;
    let baseline = q.execute_governed(edges.len() as u64, (0..3).map(VId), edges.iter().copied(),
        |_,_| Ok::<_,()>(true), |_,_| Ok(None), wide(), || { total += 1; Ok::<_,usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = q.execute_governed(edges.len() as u64, (0..3).map(VId), edges.iter().copied(),
            |_,_| Ok::<_,()>(true), |_,_| Ok(None), wide(), || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(at, stop);
    }
    let exact = GqlQueryPolicy::new(baseline.rows.snapshot_records, baseline.rows.result_rows,
        baseline.evaluator.work_units, baseline.evaluator.scratch_entries);
    let run = |policy| q.execute_governed(edges.len() as u64, (0..3).map(VId), edges.iter().copied(),
        |_,_| Ok::<_,()>(true), |_,_| Ok(None), policy, || Ok::<_,usize>(()));
    assert_eq!(run(exact).unwrap(), baseline);
    assert!(matches!(run(GqlQueryPolicy::new(27,27,exact.evaluator.max_work_units-1,u64::MAX)), Err(GqlQueryError::Evaluator(_))));
    assert!(matches!(run(GqlQueryPolicy::new(27,27,u64::MAX,exact.evaluator.max_scratch_entries-1)), Err(GqlQueryError::Evaluator(_))));
}

#[test]
fn sparse_cycle_closure_intersects_before_a_quadratic_wedge_is_generated() {
    let n = 1024_u128;
    let mut edges = Vec::new();
    for i in 0..n {
        edges.push((VId(i), RelationId(1), VId(n)));
        edges.push((VId(n), RelationId(2), VId(n + 1 + i)));
        edges.push((VId(n + 1 + i), RelationId(3), VId(i)));
    }
    let q = aggregate("MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c");
    assert!(Shape::compile(q.input_pattern().plan()).is_some());
    let result = q.execute_governed(edges.len() as u64, (0..=2*n).map(VId), edges,
        |_,_| Ok::<_,()>(true), |_,_| Ok(None), GqlQueryPolicy::new(3*n as u64,n as u64,1_000_000,300_000),
        || Ok::<_,()>(())).unwrap();
    assert_eq!(result.value.len(), n as usize);
    assert!(result.evaluator.work_units < n as u64 * n as u64);
}

#[test]
fn support_summaries_do_not_expand_or_reject_an_above_count_parallel_product() {
    let body = "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a)".to_owned()
        + &",(a)-[:R]->(b)".repeat(5);
    // Eight independent factors of 256 give 2^64 edge-identified occurrences,
    // but only one complete vertex assignment. Its support remains meaningful.
    let edges: Vec<_> = [(VId(0),RelationId(1),VId(1)),
        (VId(1),RelationId(2),VId(2)), (VId(2),RelationId(3),VId(0))]
        .into_iter().flat_map(|edge| std::iter::repeat_n(edge,256)).collect();
    let support = aggregate(&format!("MATCH {body} RETURN a,b,c,COUNT(DISTINCT c) AS n GROUP BY a,b,c"));
    let count = aggregate(&format!("MATCH {body} RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c"));
    let run = |q: &PreparedGraphAggregate| q.execute_governed(768,[VId(0),VId(1),VId(2)],edges.iter().copied(),
        |_,_| Ok::<_,()>(true), |_,_| Ok(None),GqlQueryPolicy::new(768,1,100_000,100_000),|| Ok::<_,()>(()));
    let rows = run(&support).unwrap().value;
    assert_eq!(rows.len(),1);
    assert_eq!(rows[0].get(0).unwrap().as_count(),Some(1));
    assert!(matches!(run(&count),Err(GqlQueryError::Source(_))));
}
