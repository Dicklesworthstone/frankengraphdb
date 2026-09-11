//! Tree marginalization compared with independent complete assignments.
//! Distinct hidden VIds, not only parallel edge IDs, may contribute multiplicity.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

type Edge = (VId, RelationId, VId);
type Atom = (usize, u64, u8, usize);
type Summary = (VId, u64, u64, u64, Option<VId>, Option<VId>);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }
fn run(query: &PreparedGraphAggregate, edges: &[Edge])
    -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<()>, usize>> {
    query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, usize>(()))
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_count().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex(),
        row.get(4).unwrap().as_value().unwrap().as_vertex())).collect()
}
fn oracle(edges: &[Edge], atoms: &[Atom], variables: usize, selected: usize) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, (u64, BTreeSet<VId>)> = BTreeMap::new();
    for bits in 0..(1_usize << variables) {
        let assignment: Vec<_> = (0..variables).map(|at|
            if bits & (1 << at) == 0 { VId(0) } else { VId(u128::MAX) }).collect();
        // Exhaust every assignment and count each actual edge atom independently.
        // No message maps, eliminated variables, or compiled operators here.
        let count = atoms.iter().map(|&(left, relation, direction, right)| {
            let (a, b) = (assignment[left], assignment[right]);
            edges.iter().filter(|&&(source, r, destination)| r == RelationId(relation)
                && match direction {
                    0 => source == a && destination == b,
                    1 => destination == a && source == b,
                    _ => (source == a && destination == b) || (source == b && destination == a),
                }).count() as u64
        }).product::<u64>();
        if count != 0 {
            let (total, support) = groups.entry(assignment[0]).or_default();
            *total += count;
            support.insert(assignment[selected]);
        }
    }
    groups.into_iter().map(|(a, (count, support))|
        (a, count, count, support.len() as u64, support.first().copied(), support.last().copied())).collect()
}

#[test]
fn forest_counts_and_projected_support_match_complete_assignment_enumeration() {
    type Case<'a> = (&'a str, &'a [Atom], usize, &'a str, usize);
    let cases: [Case<'_>; 6] = [
        ("MATCH (a)-[:R]->(b)-[:S]->(c)-[:R]->(d)",
            &[(0, 1, 0, 1), (1, 2, 0, 2), (2, 1, 0, 3)], 4, "b", 1),
        ("MATCH (a)-[:R]->(b), (b)-[:S]->(c), (b)-[:S]->(d), (a)-[:R]->(e)",
            &[(0, 1, 0, 1), (1, 2, 0, 2), (1, 2, 0, 3), (0, 1, 0, 4)], 5, "b", 1),
        ("MATCH (a)<-[:R]-(b)<-[:S]-(c), (b)-[:R]->(d)",
            &[(0, 1, 1, 1), (1, 2, 1, 2), (1, 1, 0, 3)], 4, "b", 1),
        ("MATCH (a)-[:R]-(b)-[:S]-(c)-[:R]-(d)",
            &[(0, 1, 2, 1), (1, 2, 2, 2), (2, 1, 2, 3)], 4, "b", 1),
        ("MATCH (a)-[:R]->(b)-[:S]->(a), (b)-[:R]->(c)-[:S]->(d)",
            &[(0, 1, 0, 1), (1, 2, 0, 0), (1, 1, 0, 2), (2, 2, 0, 3)], 4, "b", 1),
        // A projected intermediate must survive; only its following tail folds.
        ("MATCH (a)-[:R]->(b)-[:S]->(c)-[:R]->(d)",
            &[(0, 1, 0, 1), (1, 2, 0, 2), (2, 1, 0, 3)], 4, "c", 2),
    ];
    let prepared: Vec<_> = cases.iter().map(|(head, _, _, selected, _)| prepare(&format!(
        "{head} RETURN a,COUNT(*) AS n,COUNT({selected}) AS m,COUNT(DISTINCT {selected}) AS d,MIN({selected}) AS lo,MAX({selected}) AS hi GROUP BY a"))).collect();
    let universe: Vec<_> = [RelationId(1), RelationId(2)].into_iter().flat_map(|r|
        [VId(0), VId(u128::MAX)].into_iter().flat_map(move |a|
            [VId(0), VId(u128::MAX)].into_iter().map(move |b| (a, r, b)))).collect();
    for mask in 0..256_usize {
        let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        if let Some(first) = edges.first().copied() { edges.extend([first, first]); }
        for ((_, atoms, variables, _, selected), query) in cases.iter().zip(&prepared) {
            assert_eq!(plain(&run(query, &edges).unwrap().value), oracle(&edges, atoms, *variables, *selected),
                "mask={mask}, atoms={atoms:?}, selected={selected}");
        }
    }
}

fn star(leaves: usize, extra: &str) -> String {
    let mut head = format!("MATCH (a)-[:R]->(b){extra}");
    for at in 0..leaves { head.push_str(&format!(",(b)-[:S]->(x{at})")); }
    head
}
fn star_edges(choices: usize) -> Vec<Edge> {
    let mut edges = vec![(VId(0), RelationId(1), VId(1))];
    edges.extend((0..choices).map(|at| (VId(1), RelationId(2), VId(2 + at as u128))));
    edges
}

#[test]
fn a_billion_distinct_hidden_assignments_do_not_require_a_billion_visits() {
    let edges = star_edges(8);
    let query = prepare(&format!("{} RETURN COUNT(*) AS n,COUNT(b) AS m,COUNT(DISTINCT b) AS d", star(10, "")));
    let frozen = query.canonical_bytes();
    let result = query.execute_governed(9, [], edges.iter().copied(),
        |_, _| Err::<bool, _>("no predicates in this profile"),
        |_, _| Err::<Option<&CanonicalScalar>, _>("no property reads in this profile"),
        GqlQueryPolicy::new(9, 1, 4096, 256), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(8_u64.pow(10)));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(8_u64.pow(10)));
    assert_eq!(result.value[0].get(2).unwrap().as_count(), Some(1));
    assert_eq!(result.rows.snapshot_records, 9);
    assert_eq!(query.canonical_bytes(), frozen);
}

#[test]
fn overflow_support_and_impossible_branches_obey_the_positive_weight_law() {
    let edges = star_edges(3);
    let query = prepare(&format!("{} RETURN COUNT(*) AS n", star(63, "")));
    assert!(matches!(run(&query, &edges), Err(GqlQueryError::Source(
        GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
    let support = prepare(&format!("{} RETURN COUNT(DISTINCT b) AS d,MIN(b) AS lo,MAX(b) AS hi", star(63, "")));
    let result = run(&support, &edges).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(result.value[0].get(1).unwrap().as_value().unwrap().as_vertex(), Some(VId(1)));
    // The impossible branch is eliminated LAST, after overflowing siblings.
    let missing = prepare(&format!("{} RETURN COUNT(*) AS n", star(62, ",(b)-[:T]->(absent)")));
    assert_eq!(run(&missing, &edges).unwrap().value[0].get(0).unwrap().as_count(), Some(0));
}

#[test]
fn forest_preprocessing_and_prefix_execution_share_every_limit_and_checkpoint() {
    let query = prepare(&format!("{} RETURN a,COUNT(*) AS n GROUP BY a HAVING n>=1 ORDER BY n DESC,a LIMIT 1", star(3, "")));
    let edges = star_edges(3);
    let mut calls = 0;
    let mut consumed = 0;
    let measured = query.execute_governed(edges.len() as u64, [], edges.iter().copied().inspect(|_| consumed += 1),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || { calls += 1; Ok::<_, usize>(()) }).unwrap();
    assert_eq!(consumed, edges.len());
    assert_eq!(measured.value[0].get(0).unwrap().as_count(), Some(27));
    let rerun = |policy| query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy, || Ok::<_, usize>(()));
    let exact = GqlQueryPolicy::new(edges.len() as u64, 1,
        measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(rerun(exact).unwrap(), measured);
    for cap in [GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(4, 1, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(rerun(cap).is_err());
    }
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
            |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn predicates_and_property_arguments_never_disappear_into_completion_weights() {
    let query = prepare("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN COUNT(c.n) AS n");
    let edges = [(VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(2), VId(2)), (VId(1), RelationId(2), VId(2))];
    let scalar = CanonicalScalar::Int(1);
    let mut calls = 0;
    let result = query.execute_governed(3, [], edges, |_, _| Ok::<_, &str>(true), |_, _| {
        calls += 1;
        if calls == 2 { Err("second property read") } else { Ok(Some(&scalar)) }
    }, wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("second property read")))));
    assert_eq!(calls, 2);
    let query = prepare("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.n=1 RETURN COUNT(*) AS n");
    let result = query.execute_governed(3, [], edges,
        |_, _| Err::<bool, _>("hidden vertex predicate"), |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("hidden vertex predicate")))));
}
